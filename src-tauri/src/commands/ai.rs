//! AI assistant commands (docs/ai-assistant.md): detect the providers,
//! chat with streamed answers, and the built-in model's downloads. The
//! context sent with each question is built here: engine, database, its
//! structure (compact, trimmed to what fits), the editor's text and the last
//! error. Rows of data are never sent.

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dashmap::DashMap;
use dbine_ai::embedded::{self, CatalogModel};
use dbine_ai::prompts::{self, Prompts};
use dbine_ai::{
    AiError, Cancel, ChatMessage, ChatRequest, Delta, Endpoints, ProviderInfo, ProviderKind,
};
use dbine_driver::{Language, TableSchema};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};

/// The assistant's runtime state (managed by Tauri).
pub struct AiRuntime {
    pub endpoints: Endpoints,
    chats: DashMap<String, Cancel>,
    downloads: DashMap<String, Cancel>,
    /// Structure read for context, per connection + database (10 minutes).
    schemas: DashMap<String, (Instant, Arc<Vec<TableSchema>>)>,
    /// Reads waiting for the user's Aprobar / Rechazar in the chat.
    approvals: DashMap<String, tokio::sync::oneshot::Sender<String>>,
}

impl AiRuntime {
    pub fn new(models_dir: std::path::PathBuf) -> Self {
        Self {
            endpoints: Endpoints {
                models_dir,
                ..Endpoints::default()
            },
            chats: DashMap::new(),
            downloads: DashMap::new(),
            schemas: DashMap::new(),
            approvals: DashMap::new(),
        }
    }
}

impl From<AiError> for CommandError {
    fn from(e: AiError) -> Self {
        match e {
            AiError::Cancelled => CommandError::Cancelled,
            AiError::NotAvailable(m) => CommandError::BadRequest(m),
            AiError::Provider(m) => CommandError::Connect(m),
        }
    }
}

#[derive(Serialize)]
pub struct CatalogEntry {
    #[serde(flatten)]
    pub model: CatalogModel,
    pub installed: bool,
    pub downloading: bool,
    /// Suggested for this machine (RAM).
    pub recommended: bool,
}

#[derive(Serialize)]
pub struct DetectOut {
    pub providers: Vec<ProviderInfo>,
    /// Models the built-in provider can download.
    pub catalog: Vec<CatalogEntry>,
    pub embedded_enabled: bool,
    pub ollama_recommended_model: &'static str,
}

fn ram_gb() -> u64 {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok();
        if let Some(n) = out.and_then(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .parse::<u64>()
                .ok()
        }) {
            return n / (1 << 30);
        }
    }
    #[cfg(target_os = "linux")]
    if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
        if let Some(kb) = s
            .lines()
            .find(|l| l.starts_with("MemTotal"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
        {
            return kb / (1 << 20);
        }
    }
    8
}

#[tauri::command]
pub async fn ai_detect(ai: State<'_, AiRuntime>) -> CommandResult<DetectOut> {
    let providers = dbine_ai::detect(&ai.endpoints).await;
    let installed = embedded::installed(&ai.endpoints.models_dir);
    let ram = ram_gb();
    // The biggest model this machine is comfortable with.
    let best = embedded::CATALOG
        .iter()
        .filter(|m| u64::from(m.min_ram_gb) <= ram)
        .map(|m| m.id)
        .next_back();
    Ok(DetectOut {
        providers,
        catalog: embedded::CATALOG
            .iter()
            .map(|m| CatalogEntry {
                model: m.clone(),
                installed: installed.iter().any(|x| x.id == m.id),
                downloading: ai.downloads.contains_key(m.id),
                recommended: Some(m.id) == best,
            })
            .collect(),
        embedded_enabled: embedded::ENABLED,
        // Same rule as the built-in catalog: the 32B from 48 GB.
        ollama_recommended_model: if ram >= 48 {
            "qwen2.5-coder:32b"
        } else {
            dbine_ai::ollama::RECOMMENDED_MODEL
        },
    })
}

#[derive(Deserialize, Default)]
pub struct ChatContext {
    #[serde(default)]
    pub connection_id: Option<String>,
    #[serde(default)]
    pub database: Option<String>,
    /// The object the tab shows (a table's data), `schema.name`.
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default)]
    pub editor_sql: Option<String>,
    #[serde(default)]
    pub selection: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    /// "Aprobar lecturas en esta conversación": a local model's reads of rows
    /// (sample_rows, run_query, explain) run without asking each time. The
    /// UI resets it on a new conversation or another connection / database.
    #[serde(default)]
    pub approve_reads: bool,
}

#[derive(Deserialize)]
pub struct ChatArgs {
    pub chat_id: String,
    pub provider: ProviderKind,
    #[serde(default)]
    pub model: Option<String>,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub context: ChatContext,
    /// The UI language ("en", "pt"…): the instructions to the model go in
    /// it (English when unknown).
    #[serde(default)]
    pub ui_language: Option<String>,
}

#[derive(Clone, Serialize)]
struct DeltaEvent<'a> {
    chat_id: &'a str,
    delta: Delta,
}

/// A read waiting for the user: where and the exact query.
#[derive(Clone, Serialize)]
struct ApprovalEvent<'a> {
    chat_id: &'a str,
    request_id: &'a str,
    label: &'a str,
    connection: &'a str,
    database: &'a str,
    sql: &'a str,
}

#[derive(Deserialize)]
pub struct ApproveArgs {
    pub request_id: String,
    /// `approve`, `approve_all` (the rest of the conversation) or `reject`.
    pub decision: String,
}

/// The user's answer to a read the assistant asked for.
#[tauri::command(rename_all = "camelCase")]
pub async fn ai_approve(ai: State<'_, AiRuntime>, args: ApproveArgs) -> CommandResult<()> {
    if let Some((_, tx)) = ai.approvals.remove(&args.request_id) {
        let _ = tx.send(args.decision);
    }
    Ok(())
}

#[derive(Clone, Serialize)]
struct ToolEvent<'a> {
    chat_id: &'a str,
    /// What it read, for the chat ("estructura de ventas.clientes en tenant-compras").
    label: &'a str,
    ok: bool,
}

#[derive(Clone, Serialize)]
struct StatusEvent<'a> {
    chat_id: &'a str,
    /// `schema` (reading the structure), `engine` (downloading the built-in
    /// model's engine; `note` has the percentage), `thinking` (waiting for
    /// the model).
    phase: &'a str,
    note: Option<String>,
}

#[derive(Serialize)]
pub struct ChatOut {
    pub text: String,
    /// What went as context, for the UI's "what was sent" line (the UI
    /// words it in its language).
    pub context: Vec<ContextItem>,
}

/// One part of what went as context.
#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextItem {
    Engine {
        name: String,
    },
    Database {
        name: String,
    },
    /// The object names a local model got.
    Objects {
        count: usize,
    },
    /// The structure: tables with columns, of all of them.
    Tables {
        shown: usize,
        total: usize,
    },
    Editor,
    Selection,
    LastError,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn ai_chat(
    app: AppHandle,
    state: State<'_, AppState>,
    ai: State<'_, AiRuntime>,
    mcp: State<'_, crate::mcp::McpRuntime>,
    args: ChatArgs,
) -> CommandResult<ChatOut> {
    let cancel = Cancel::new();
    ai.chats.insert(args.chat_id.clone(), cancel.clone());
    let id = args.chat_id.clone();
    let result = async {
        let p = prompts::get(args.ui_language.as_deref().unwrap_or(""));
        let (system, summary) = build_system(&app, &state, &ai, &args, p).await;
        // The built-in model's engine, the first time (models downloaded
        // before it came separately).
        if args.provider == ProviderKind::Embedded {
            let progress = |done: u64, total: u64| {
                let pct = (done * 100).checked_div(total).unwrap_or(0);
                let note = format!("{pct} % de {} MB", total.div_ceil(1_000_000));
                let _ = app.emit("ai-status", StatusEvent { chat_id: &id, phase: "engine", note: Some(note) });
            };
            embedded::ensure_engine(&ai.endpoints.models_dir, &progress, &cancel).await?;
        }
        let _ = app.emit("ai-status", StatusEvent { chat_id: &id, phase: "thinking", note: None });
        let mut req = ChatRequest { kind: args.provider, model: args.model.clone(), system, messages: args.messages.clone(), lang: p.lang() };
        let emit = |d: Delta| {
            let _ = app.emit("ai-delta", DeltaEvent { chat_id: &id, delta: d });
        };
        // The model may read the tab's whole connection through DBine's
        // read-only tools (the MCP server's code): it asks with a <tool>
        // line, DBine answers, and it goes on. Each call to Claude Code or
        // Codex is a new process: the exchange so far goes along in
        // `req.messages`. Remote models only read the catalog
        // (REMOTE_TOOLS): rows never leave the computer for them.
        let conn = args.context.connection_id.as_deref().and_then(|c| state.store.get_connection(c).ok().flatten());
        let tools_on = conn.is_some();
        let tab_db = args.context.database.clone().unwrap_or_default();
        if tools_on {
            let name = conn.as_ref().map(|c| c.name.as_str()).unwrap_or("");
            req.system.push_str(&tools_prompt(p, name, &tab_db, args.provider.local()));
        }
        let mut steps = 0;
        let mut approve_all = args.context.approve_reads;
        // Small models loop asking the same thing: a repeat isn't run again.
        let mut asked: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut text = loop {
            if cancel.is_cancelled() {
                return Err(AiError::Cancelled.into());
            }
            // Stream the answer, but not a tool request: hold the start
            // until it's clear which one it is.
            let held = std::sync::Mutex::new((String::new(), !tools_on));
            let on = |d: Delta| match d {
                Delta::Text(t) => {
                    let mut h = held.lock().unwrap();
                    if h.1 {
                        emit(Delta::Text(t));
                        return;
                    }
                    h.0.push_str(&t);
                    let start = h.0.trim_start();
                    if (start.len() >= TOOL_OPEN.len() || !TOOL_OPEN.starts_with(start))
                        && !start.starts_with(TOOL_OPEN)
                        && !start.starts_with('{')
                    {
                        h.1 = true;
                        emit(Delta::Text(std::mem::take(&mut h.0)));
                    }
                }
                other => emit(other),
            };
            let out = dbine_ai::chat(&req, &ai.endpoints, &on, &cancel).await?;
            let call = if tools_on && steps < MAX_TOOL_STEPS { tool_call(&out) } else { None };
            let Some((name, mut targs)) = call else {
                let (rest, passed) = {
                    let h = held.lock().unwrap();
                    (h.0.clone(), h.1)
                };
                if !passed && !rest.is_empty() {
                    emit(Delta::Text(rest));
                }
                break out;
            };
            steps += 1;
            let conn = conn.as_ref().expect("tools_on");
            // A data tool from a remote model, or a database the connection
            // doesn't have: the model gets why, nothing runs.
            let mut blocked = prepare_call(args.provider, &name, &mut targs, &tab_db).err();
            if blocked.is_none() {
                blocked = check_database(&state, conn, &name, &mut targs, &tab_db).await.err();
            }
            if let Some(why) = blocked {
                let label = format!("{} (no se leyó)", tool_label(&name, &targs));
                let _ = app.emit("ai-tool", ToolEvent { chat_id: &id, label: &label, ok: false });
                req.messages.push(ChatMessage { role: "assistant".into(), content: format!("{TOOL_OPEN}{}{TOOL_CLOSE}", serde_json::json!({ "name": name, "arguments": targs })) });
                req.messages.push(ChatMessage { role: "user".into(), content: format!("<error>\n{why}\n</error>") });
                if steps == MAX_TOOL_STEPS {
                    req.system.push_str(&format!("\n{}\n", p.text("assistant.max_steps")));
                }
                continue;
            }
            if !asked.insert(format!("{name}{targs}")) {
                req.messages.push(ChatMessage { role: "assistant".into(), content: format!("{TOOL_OPEN}{}{TOOL_CLOSE}", serde_json::json!({ "name": name, "arguments": targs })) });
                req.messages.push(ChatMessage { role: "user".into(), content: format!("<error>\n{}\n</error>", p.text("assistant.repeated")) });
                continue;
            }
            let mut label = tool_label(&name, &targs);
            let _ = app.emit("ai-status", StatusEvent { chat_id: &id, phase: "tool", note: Some(label.clone()) });
            // A read of rows runs only after the user approves its exact
            // query in the chat (or chose "Aprobar lecturas en esta
            // conversación": `approve_reads` starts off and only that button
            // turns it on); catalog reads (structure, index usage) don't ask.
            // That approval is what lets a read run on an engine that can't
            // enforce it on the server (mcp/reads.rs); where it can, it runs
            // through `run_read_only` anyway.
            let refused = if READS_ROWS.contains(&name.as_str()) && !approve_all {
                let preview = tokio::select! {
                    p = crate::mcp::tools::assistant_preview(&mcp.inner, conn, &name, &targs) => p,
                    _ = cancel.cancelled() => return Err(AiError::Cancelled.into()),
                };
                match preview {
                    Err(e) => Some(e),
                    Ok(sql) => {
                        let request_id = format!("{id}:{steps}");
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        ai.approvals.insert(request_id.clone(), tx);
                        let database = targs["database"].as_str().unwrap_or("").to_string();
                        let _ = app.emit("ai-approval", ApprovalEvent { chat_id: &id, request_id: &request_id, label: &label, connection: &conn.name, database: &database, sql: &sql });
                        let decision = tokio::select! {
                            d = rx => d.unwrap_or_default(),
                            _ = cancel.cancelled() => {
                                ai.approvals.remove(&request_id);
                                return Err(AiError::Cancelled.into());
                            }
                        };
                        match decision.as_str() {
                            "approve" => None,
                            "approve_all" => {
                                approve_all = true;
                                None
                            }
                            _ => Some(p.text("assistant.refused_read").to_string()),
                        }
                    }
                }
            } else {
                None
            };
            let (result, failed) = match refused {
                Some(why) => {
                    label = format!("{label} (no se leyó)");
                    (why, true)
                }
                // Stop ends the turn mid-read too (a whole database's index
                // overview can take a couple of minutes).
                None => {
                    // Rows only for a local model (prepare_call already
                    // refused them to a remote one).
                    let (text, failed) = tokio::select! {
                        r = crate::mcp::tools::assistant_call(&mcp.inner, conn, &name, &targs, args.provider.local()) => r,
                        _ = cancel.cancelled() => return Err(AiError::Cancelled.into()),
                    };
                    let db = targs["database"].as_str().unwrap_or("");
                    if failed && name != "list_databases" && !db.is_empty() && db != tab_db {
                        (other_database_failed(db, &text), true)
                    } else {
                        (text, failed)
                    }
                }
            };
            let _ = app.emit("ai-tool", ToolEvent { chat_id: &id, label: &label, ok: !failed });
            req.messages.push(ChatMessage { role: "assistant".into(), content: format!("{TOOL_OPEN}{}{TOOL_CLOSE}", serde_json::json!({ "name": name, "arguments": targs })) });
            let tag = if failed { "error" } else { "result" };
            req.messages.push(ChatMessage { role: "user".into(), content: format!("<{tag}>\n{}\n</{tag}>", truncate(&result, TOOL_RESULT_MAX, p)) });
            if steps == MAX_TOOL_STEPS {
                req.system.push_str(&format!("\n{}\n", p.text("assistant.max_steps")));
            }
        };
        // Small local models sometimes refuse a legitimate ask: once, the
        // same question again with a line saying it's fine. The UI drops
        // the refusal it showed on "retry".
        if refused(&text) {
            let _ = app.emit("ai-status", StatusEvent { chat_id: &id, phase: "retry", note: None });
            req.system.push_str(&format!("\n{}\n", p.text("assistant.after_refusal")));
            text = dbine_ai::chat(&req, &ai.endpoints, &emit, &cancel).await?;
        }
        Ok(ChatOut { text, context: summary })
    }
    .await;
    ai.chats.remove(&args.chat_id);
    result
}

#[derive(Deserialize)]
pub struct IdArgs {
    pub id: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn ai_cancel(ai: State<'_, AiRuntime>, args: IdArgs) -> CommandResult<()> {
    if let Some((_, c)) = ai.chats.remove(&args.id) {
        c.cancel();
    }
    if let Some((_, c)) = ai.downloads.remove(&args.id) {
        c.cancel();
    }
    Ok(())
}

#[derive(Clone, Serialize)]
struct DownloadEvent<'a> {
    id: &'a str,
    status: &'a str,
    done: u64,
    total: u64,
}

/// Download a built-in model (resumes a partial one; checks SHA-256), with
/// its engine the first time: one progress bar for both.
#[tauri::command(rename_all = "camelCase")]
pub async fn ai_download_model(
    app: AppHandle,
    ai: State<'_, AiRuntime>,
    args: IdArgs,
) -> CommandResult<()> {
    let m = embedded::catalog_model(&args.id)
        .ok_or_else(|| CommandError::NotFound("modelo desconocido".into()))?;
    if ai.downloads.contains_key(m.id) {
        return Err(CommandError::BadRequest("ya se está descargando".into()));
    }
    let cancel = Cancel::new();
    ai.downloads.insert(m.id.to_string(), cancel.clone());
    let dir = &ai.endpoints.models_dir;
    let engine = embedded::engine_pending(dir);
    let total = engine + m.size;
    let progress = |done: u64| {
        let _ = app.emit(
            "ai-download",
            DownloadEvent {
                id: m.id,
                status: "descargando",
                done,
                total,
            },
        );
    };
    let r = match embedded::ensure_engine(dir, &|done, _| progress(done), &cancel).await {
        Ok(()) => embedded::download(dir, m, &|done, _| progress(engine + done), &cancel).await,
        Err(e) => Err(e),
    };
    ai.downloads.remove(m.id);
    r?;
    Ok(())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn ai_delete_model(ai: State<'_, AiRuntime>, args: IdArgs) -> CommandResult<()> {
    let m = embedded::catalog_model(&args.id)
        .ok_or_else(|| CommandError::NotFound("modelo desconocido".into()))?;
    Ok(embedded::delete(&ai.endpoints.models_dir, m)?)
}

#[tauri::command]
pub async fn ai_start_ollama(ai: State<'_, AiRuntime>) -> CommandResult<()> {
    Ok(dbine_ai::ollama::start(&ai.endpoints.ollama).await?)
}

/// Download a model into Ollama (`ollama pull`), with progress.
#[tauri::command(rename_all = "camelCase")]
pub async fn ai_pull_ollama(
    app: AppHandle,
    ai: State<'_, AiRuntime>,
    args: IdArgs,
) -> CommandResult<()> {
    let key = format!("ollama:{}", args.id);
    let progress = |status: &str, done: u64, total: u64| {
        let _ = app.emit(
            "ai-download",
            DownloadEvent {
                id: &key,
                status,
                done,
                total,
            },
        );
    };
    Ok(dbine_ai::ollama::pull(&ai.endpoints.ollama, &args.id, &progress).await?)
}

// -- context ----------------------------------------------------------------------

/// Object names a local model gets up front; it asks for the rest.
const SUMMARY_NAMES: usize = 400;

/// How much structure fits: small local models get less.
fn schema_budget(kind: ProviderKind) -> usize {
    match kind {
        ProviderKind::Embedded | ProviderKind::Ollama | ProviderKind::LmStudio => 24_000,
        ProviderKind::ClaudeCode | ProviderKind::Codex => 150_000,
    }
}

async fn build_system(
    app: &AppHandle,
    state: &AppState,
    ai: &AiRuntime,
    args: &ChatArgs,
    p: &Prompts,
) -> (String, Vec<ContextItem>) {
    let ctx = &args.context;
    let mut s = format!("{}\n", p.text("assistant.system"));
    let mut summary: Vec<ContextItem> = Vec::new();
    let conn = ctx
        .connection_id
        .as_deref()
        .and_then(|id| state.store.get_connection(id).ok().flatten());
    if let Some(conn) = &conn {
        let info = dbine_drivers::find(&conn.config.driver).map(|d| d.info());
        let db = ctx.database.clone().unwrap_or_default();
        if let Some(info) = info {
            let lang = match info.language {
                Language::Sql => p.fill("assistant.language_sql", &[("dialect", info.dialect)]),
                Language::Cql => "CQL (Cassandra)".into(),
                Language::Json => p.text("assistant.language_json").into(),
                Language::Redis => p.text("assistant.language_redis").into(),
                Language::Flux => "Flux (InfluxDB)".into(),
                Language::Cypher => "Cypher".into(),
            };
            s.push_str(&format!(
                "\n{}\n",
                p.fill(
                    "assistant.engine",
                    &[("engine", info.name), ("language", &lang)]
                )
            ));
            if let Some(hint) = dialect_hint(info.dialect) {
                s.push_str(&format!("{}\n", p.text(hint)));
            }
            summary.push(ContextItem::Engine {
                name: info.name.to_string(),
            });
        }
        if !db.is_empty() {
            s.push_str(&format!(
                "{}\n",
                p.fill("assistant.database", &[("database", &db)])
            ));
            summary.push(ContextItem::Database { name: db.clone() });
        }
        if conn.config.read_only {
            s.push_str(&format!("{}\n", p.text("assistant.read_only")));
        }
        if let Some(o) = &ctx.object {
            s.push_str(&format!(
                "{}\n",
                p.fill("assistant.object", &[("object", o)])
            ));
        }
        if args.provider.local() {
            // A local model reads details through the catalog tools: it gets
            // only the names here (from the explorer's cached list).
            match state
                .meta_read(&conn.id, &db, Duration::from_secs(60), |s| {
                    Box::pin(s.list_objects())
                })
                .await
            {
                Ok(objects) => {
                    let names: Vec<String> = objects
                        .iter()
                        .filter(|o| o.parent.is_none())
                        .map(|o| match o.schema.as_deref().filter(|x| !x.is_empty()) {
                            Some(sc) => format!("{} {sc}.{}", o.kind, o.name),
                            None => format!("{} {}", o.kind, o.name),
                        })
                        .collect();
                    let shown = names.len().min(SUMMARY_NAMES);
                    let more = if names.len() > shown {
                        format!(
                            "\n{}",
                            p.fill(
                                "assistant.objects_more",
                                &[("count", &(names.len() - shown).to_string())]
                            )
                        )
                    } else {
                        String::new()
                    };
                    s.push_str(&format!(
                        "\n<objects database=\"{db}\">\n{}{more}\n</objects>\n{}\n",
                        names[..shown].join("\n"),
                        p.text("assistant.objects_hint")
                    ));
                    summary.push(ContextItem::Objects { count: names.len() });
                }
                Err(e) => s.push_str(&format!(
                    "\n{}\n",
                    p.fill("assistant.objects_failed", &[("error", &e.to_string())])
                )),
            }
        } else {
            // Claude Code and Codex get the tab's database structure as
            // context; other databases only through the catalog tools,
            // when the model asks for them.
            let _ = app.emit(
                "ai-status",
                StatusEvent {
                    chat_id: &args.chat_id,
                    phase: "schema",
                    note: None,
                },
            );
            match schema_for(state, ai, &conn.id, &db).await {
                Ok(tables) => {
                    let question = args
                        .messages
                        .iter()
                        .map(|m| m.content.as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    let hint = format!(
                        "{question}\n{}\n{}",
                        ctx.editor_sql.as_deref().unwrap_or(""),
                        ctx.object.as_deref().unwrap_or("")
                    );
                    let (text, shown, total) =
                        compact_schema(&tables, &hint, schema_budget(args.provider), p);
                    s.push_str(&format!("\n<structure>\n{text}</structure>\n"));
                    summary.push(ContextItem::Tables { shown, total });
                }
                Err(e) => {
                    s.push_str(&format!(
                        "\n{}\n",
                        p.fill("assistant.structure_failed", &[("error", &e.to_string())])
                    ));
                    let _ = app.emit(
                        "ai-status",
                        StatusEvent {
                            chat_id: &args.chat_id,
                            phase: "schema",
                            note: Some(format!("sin estructura: {e}")),
                        },
                    );
                }
            }
        }
    } else {
        s.push_str(&format!("\n{}\n", p.text("assistant.no_database")));
    }
    if let Some(sql) = ctx.editor_sql.as_deref().filter(|t| !t.trim().is_empty()) {
        s.push_str(&format!(
            "\n<editor>\n{}\n</editor>\n",
            truncate(sql, 20_000, p)
        ));
        summary.push(ContextItem::Editor);
    }
    if let Some(sel) = ctx.selection.as_deref().filter(|t| !t.trim().is_empty()) {
        s.push_str(&format!(
            "\n<selection>\n{}\n</selection>\n{}\n",
            truncate(sel, 10_000, p),
            p.text("assistant.selection_hint")
        ));
        summary.push(ContextItem::Selection);
    }
    if let Some(err) = ctx.last_error.as_deref().filter(|t| !t.trim().is_empty()) {
        s.push_str(&format!(
            "\n<last_error>\n{}\n</last_error>\n",
            truncate(err, 4_000, p)
        ));
        summary.push(ContextItem::LastError);
    }
    // Small models follow what comes last, and an example, best.
    s.push_str(&format!("\n{}\n", p.text("assistant.reminder")));
    (s, summary)
}

/// What small models get wrong most in each dialect: quoting, naming another
/// database, limiting rows. The prompt key of its hint.
fn dialect_hint(dialect: &str) -> Option<&'static str> {
    Some(match dialect {
        "mssql" | "sybase" => "assistant.dialect_mssql",
        "postgres" => "assistant.dialect_postgres",
        "mysql" => "assistant.dialect_mysql",
        "oracle" => "assistant.dialect_oracle",
        "sqlite" => "assistant.dialect_sqlite",
        _ => return None,
    })
}

/// The model refused a legitimate request ("Lo siento, no puedo ayudarte con
/// eso"): a short answer without code that apologizes or says it can't, in
/// any of the UI languages (scripts/i18n/languages.json).
fn refused(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    if t.is_empty() || t.contains("```") || t.chars().count() > 600 {
        return false;
    }
    const SIGNS: &[&str] = &[
        "no puedo ayudar",
        "no puedo asistir",
        "no puedo hacer eso",
        "no puedo proporcionar",
        "lo siento, no puedo",
        "lo siento, pero no puedo",
        "can't help with",
        "cannot help with",
        "can't assist",
        "cannot assist",
        "i'm sorry, but i can",
        "i am sorry, but i can",
        "não posso ajudar",
        "desculpe, mas não posso",
        "je ne peux pas vous aider",
        "je ne peux pas t'aider",
        "non posso aiutar",
        "kann ich nicht helfen",
        "kann ich ihnen nicht helfen",
        "kann ich dir nicht helfen",
        "ich kann ihnen dabei nicht helfen",
        "ich kann dir dabei nicht helfen",
        "не могу помочь",
        "не могу с этим помочь",
        "не могу выполнить этот запрос",
        "无法帮助",
        "无法协助",
        "不能帮助",
        "无法提供帮助",
        "抱歉，我无法",
        "抱歉，我不能",
        "お手伝いできません",
        "お手伝いすることはできません",
        "お応えできません",
        "対応できません",
        "도와드릴 수 없습니다",
        "도와드릴 수 없어요",
        "도움을 드릴 수 없습니다",
        "지원할 수 없습니다",
    ];
    SIGNS.iter().any(|s| t.contains(s))
}

/// The line a local model asks for a tool with. The same in every language:
/// DBine reads the answer by it.
const TOOL_OPEN: &str = "<tool>";
const TOOL_CLOSE: &str = "</tool>";
/// Reads per answer: enough for a few databases' structure, bounded so a
/// confused model can't loop.
const MAX_TOOL_STEPS: usize = 12;
/// Tools that read rows: each one waits for the user's approval.
const READS_ROWS: &[&str] = &["sample_rows", "run_query", "explain"];
/// A tool's answer, cut for a small model's context.
const TOOL_RESULT_MAX: usize = 12_000;

/// What a remote model (Claude Code, Codex) may read: the catalog only
/// (databases, objects, structure, index statistics), of any database of
/// the tab's connection. sample_rows and run_query return rows, and
/// explain runs the query on some engines: those stay with local models,
/// so no table data reaches a remote provider.
const REMOTE_TOOLS: &[&str] = &[
    "list_databases",
    "list_objects",
    "describe_object",
    "index_usage",
];

/// How the model reads the connection: one tool per answer, in a line
/// DBine recognizes (models' own tool formats vary and small ones misuse
/// them; this works the same on every provider). A remote model's list has
/// only REMOTE_TOOLS: the lines of the data tools, and the one after them
/// about approving their queries, are left out (every language has the same
/// lines, a test checks it).
fn tools_prompt(p: &Prompts, connection: &str, database: &str, local: bool) -> String {
    let text = p.fill(
        "assistant.tools",
        &[("connection", connection), ("database", database)],
    );
    if local {
        return format!("\n{text}\n");
    }
    let lines: Vec<&str> = text.lines().collect();
    let data = |l: &str| READS_ROWS.iter().any(|t| l.starts_with(&format!("- {t} ")));
    let after_data = lines.iter().rposition(|l| data(l)).map(|i| i + 1);
    let kept: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(i, l)| !data(l) && Some(*i) != after_data)
        .map(|(_, l)| *l)
        .collect();
    format!("\n{}\n", kept.join("\n"))
}

/// Whether the model may call `name`, and with which database: one without
/// a database reads the tab's; one naming another database of the
/// connection reads that one (only when the model asks: nothing is read
/// ahead). A data tool from a remote model is refused with why.
fn prepare_call(
    provider: ProviderKind,
    name: &str,
    args: &mut serde_json::Value,
    tab_db: &str,
) -> Result<(), String> {
    if !provider.local() && !REMOTE_TOOLS.contains(&name) {
        return Err(format!(
            "{name} reads table data, and with this model DBine only reads metadata: {}. \
             Continue without the rows: answer with what you have, or write the query for the user to run.",
            REMOTE_TOOLS.join(", ")
        ));
    }
    if args.get("database").and_then(|v| v.as_str()).is_none() {
        args["database"] = serde_json::Value::String(tab_db.to_string());
    }
    Ok(())
}

/// A database the model named that the connection doesn't list: whether
/// it's there (`Ok(None)`), there with other casing (`Ok(Some(name))`), or
/// not there or not visible to the connection's user (`Err`). An empty list
/// (engines without databases) can't tell, so it passes.
fn find_database(listed: &[String], wanted: &str) -> Result<Option<String>, ()> {
    if listed.is_empty() || listed.iter().any(|d| d == wanted) {
        return Ok(None);
    }
    let same: Vec<&String> = listed
        .iter()
        .filter(|d| d.eq_ignore_ascii_case(wanted))
        .collect();
    match same.as_slice() {
        [one] => Ok(Some((*one).clone())),
        _ => Err(()),
    }
}

/// What the model gets for a database the connection doesn't have.
fn missing_database(wanted: &str, connection: &str, listed: &[String]) -> String {
    const SHOWN: usize = 50;
    let mut names = listed
        .iter()
        .take(SHOWN)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if listed.len() > SHOWN {
        names.push_str(&format!(" … ({} more)", listed.len() - SHOWN));
    }
    format!(
        "The database “{wanted}” doesn't exist on the connection “{connection}”, or the connection's user can't access it. \
         Its databases: {names}. Tell the user; don't guess what that database has."
    )
}

/// A read of another database than the tab's that failed: said plainly
/// (it may not exist, or the user may lack permission on it).
fn other_database_failed(database: &str, error: &str) -> String {
    format!(
        "Could not read the database “{database}”: {error}\n\
         It may not exist, or the connection's user may not have permission on it. Tell the user; don't guess what it has."
    )
}

/// A tool naming another database than the tab's: checked against the
/// connection's list first (the explorer's cached one), so a database that
/// isn't there comes back as a clear error, not an empty list.
async fn check_database(
    state: &AppState,
    conn: &dbine_core::SavedConnection,
    name: &str,
    args: &mut serde_json::Value,
    tab_db: &str,
) -> Result<(), String> {
    let wanted = args["database"].as_str().unwrap_or("").to_string();
    if name == "list_databases" || wanted.is_empty() || wanted == tab_db {
        return Ok(());
    }
    // A list that can't be read doesn't stop the call: the tool's own
    // error says more.
    let Ok(listed) = state
        .meta_read(&conn.id, "", Duration::from_secs(60), |s| {
            Box::pin(s.list_databases())
        })
        .await
    else {
        return Ok(());
    };
    match find_database(&listed, &wanted) {
        Ok(None) => Ok(()),
        Ok(Some(fixed)) => {
            args["database"] = serde_json::Value::String(fixed);
            Ok(())
        }
        Err(()) => Err(missing_database(&wanted, &conn.name, &listed)),
    }
}

/// The tool a model's answer asks for, wherever it wrote it: our
/// <tool> line (or <herramienta>, what the Spanish-only prompt asked
/// for), Qwen's own <tool_call> tags, a ```json block, or a bare
/// {"name": …, "arguments": …} object after some text (small models mix them).
fn tool_call(out: &str) -> Option<(String, serde_json::Value)> {
    let tagged = [
        (TOOL_OPEN, TOOL_CLOSE),
        ("<herramienta>", "</herramienta>"),
        ("<tool_call>", "</tool_call>"),
    ]
    .iter()
    .filter_map(|(open, close)| {
        let a = out.find(open)? + open.len();
        let b = out[a..].find(close).map_or(out.len(), |b| a + b);
        Some(&out[a..b])
    })
    .collect::<Vec<_>>();
    for body in tagged.iter().copied().chain(json_objects(out)) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(body.trim()) else {
            continue;
        };
        let Some(name) = v.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        if !crate::mcp::tools::ASSISTANT_TOOLS.contains(&name) {
            continue;
        }
        let args = v
            .get("arguments")
            .or_else(|| v.get("parameters"))
            .filter(|a| a.is_object())
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        return Some((name.to_string(), args));
    }
    None
}

/// Every balanced {…} in `text` that mentions "name", outermost first.
fn json_objects(text: &str) -> impl Iterator<Item = &str> {
    let b = text.as_bytes();
    let mut found = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'{' {
            let (mut depth, mut j, mut in_str, mut esc) = (0usize, i, false, false);
            while j < b.len() {
                let c = b[j];
                if in_str {
                    if esc {
                        esc = false
                    } else if c == b'\\' {
                        esc = true
                    } else if c == b'"' {
                        in_str = false
                    }
                } else if c == b'"' {
                    in_str = true;
                } else if c == b'{' {
                    depth += 1;
                } else if c == b'}' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                j += 1;
            }
            if j < b.len() {
                let obj = &text[i..=j];
                if obj.contains("\"name\"") {
                    found.push(obj);
                    i = j + 1;
                    continue;
                }
            }
        }
        i += 1;
    }
    found.into_iter()
}

/// What the chat shows for a read ("estructura de ventas.clientes en tenant-compras").
fn tool_label(name: &str, args: &serde_json::Value) -> String {
    let s = |k: &str| {
        args.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let (db, obj) = (s("database"), s("object"));
    // Whole sentences (with and without the database), so the backend
    // catalog can translate each one.
    let with_db = !db.is_empty();
    match name {
        "list_databases" => "bases de la conexión".into(),
        "list_objects" if with_db => format!("objetos de la base {db}"),
        "list_objects" => "objetos de la base".into(),
        "describe_object" if with_db => format!("estructura de {obj} en {db}"),
        "describe_object" => format!("estructura de {obj}"),
        "index_usage" if obj.is_empty() && with_db => {
            format!("uso de índices de todas las tablas en {db}")
        }
        "index_usage" if obj.is_empty() => "uso de índices de todas las tablas".into(),
        "index_usage" if with_db => format!("uso de índices de {obj} en {db}"),
        "index_usage" => format!("uso de índices de {obj}"),
        "sample_rows" if with_db => format!("filas de muestra de {obj} en {db}"),
        "sample_rows" => format!("filas de muestra de {obj}"),
        "run_query" => {
            let q: String = s("query")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(120)
                .collect();
            if with_db {
                format!("consulta en {db}: {q}")
            } else {
                format!("consulta de la conexión: {q}")
            }
        }
        "explain" if with_db => format!("plan de ejecución en {db}"),
        "explain" => "plan de ejecución".into(),
        other => other.to_string(),
    }
}

fn truncate(s: &str, max: usize, p: &Prompts) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n{}", &s[..end], p.text("truncated"))
}

async fn schema_for(
    state: &AppState,
    ai: &AiRuntime,
    connection_id: &str,
    database: &str,
) -> CommandResult<Arc<Vec<TableSchema>>> {
    let key = format!("{connection_id}\u{0}{database}");
    if let Some(e) = ai.schemas.get(&key) {
        if e.0.elapsed() < Duration::from_secs(600) {
            return Ok(e.1.clone());
        }
    }
    let tables = state
        .meta_read(connection_id, database, Duration::from_secs(60), |s| {
            Box::pin(s.database_schema())
        })
        .await?;
    let tables = Arc::new(tables);
    ai.schemas.insert(key, (Instant::now(), tables.clone()));
    Ok(tables)
}

fn qualified(t: &TableSchema) -> String {
    match &t.schema {
        Some(s) if !s.is_empty() => format!("{s}.{}", t.name),
        _ => t.name.clone(),
    }
}

fn table_line(t: &TableSchema, p: &Prompts) -> String {
    let pk: Vec<&str> = t
        .primary_key
        .as_ref()
        .map(|k| k.columns.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let cols: Vec<String> = t
        .columns
        .iter()
        .map(|c| {
            let mut s = format!("{} {}", c.name, c.data_type);
            if pk.contains(&c.name.as_str()) {
                s.push_str(" PK");
            } else if !c.nullable {
                s.push_str(" NOT NULL");
            }
            s
        })
        .collect();
    let kind = if t.kind.is_empty() || t.kind == "table" {
        String::new()
    } else {
        format!("[{}] ", t.kind)
    };
    let mut line = format!("{kind}{}({})", qualified(t), cols.join(", "));
    for fk in &t.foreign_keys {
        let target = match &fk.ref_schema {
            Some(s) if !s.is_empty() => format!("{s}.{}", fk.ref_table),
            _ => fk.ref_table.clone(),
        };
        line.push_str(&format!(
            "\n  FK ({}) -> {target}({})",
            fk.columns.join(", "),
            fk.ref_columns.join(", ")
        ));
    }
    if let Some(c) = t.comment.as_deref().filter(|c| !c.is_empty()) {
        line.push_str(&format!("\n  -- {}", truncate(c, 200, p)));
    }
    line.push('\n');
    line
}

/// The structure in a compact form within `budget` characters. Tables named
/// in `hint` (the question, the editor) go first and whole; the rest whole
/// while they fit, then by name only. Returns (text, tables with columns,
/// total).
fn compact_schema(
    tables: &[TableSchema],
    hint: &str,
    budget: usize,
    p: &Prompts,
) -> (String, usize, usize) {
    let hint = hint.to_lowercase();
    let mentioned = |t: &TableSchema| {
        let n = t.name.to_lowercase();
        // Whole-word-ish match, so "id" or "a" don't match everything.
        n.len() > 2
            && hint.match_indices(&n).any(|(i, _)| {
                let before = hint[..i]
                    .chars()
                    .last()
                    .is_none_or(|c| !c.is_alphanumeric() && c != '_');
                let after = hint[i + n.len()..]
                    .chars()
                    .next()
                    .is_none_or(|c| !c.is_alphanumeric() && c != '_');
                before && after
            })
    };
    let mut order: Vec<&TableSchema> = tables.iter().collect();
    order.sort_by_key(|t| !mentioned(t));
    // Tables the mentioned ones point to come right after them.
    let mut out = String::new();
    let mut names_only: Vec<String> = Vec::new();
    let mut shown = 0;
    for t in order {
        let line = table_line(t, p);
        if out.len() + line.len() <= budget {
            out.push_str(&line);
            shown += 1;
        } else {
            names_only.push(qualified(t));
        }
    }
    if !names_only.is_empty() {
        let mut rest = format!(
            "{}\n",
            p.fill(
                "assistant.other_tables",
                &[("names", &names_only.join(", "))]
            )
        );
        if out.len() + rest.len() > budget + 20_000 {
            rest = truncate(&rest, 20_000, p);
        }
        out.push_str(&rest);
    }
    (out, shown, tables.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_calls() {
        let (n, a) = tool_call("<tool>{\"name\": \"describe_object\", \"arguments\": {\"database\": \"b\", \"object\": \"ventas.clientes\"}}</tool>").unwrap();
        assert_eq!(n, "describe_object");
        assert_eq!(a["object"], "ventas.clientes");
        assert_eq!(tool_call("Primero listo las bases.\n<herramienta>{\"name\": \"list_databases\", \"arguments\": {}}</herramienta>").unwrap().0, "list_databases", "the old tag");
        assert_eq!(tool_call("First the databases.\n<tool>{\"name\": \"list_databases\", \"arguments\": {}}</tool>").unwrap().0, "list_databases");
        assert_eq!(
            tool_call("{\"name\": \"list_databases\", \"arguments\": {}}")
                .unwrap()
                .0,
            "list_databases",
            "bare JSON"
        );
        assert!(
            tool_call("<tool>{\"name\": \"execute\", \"arguments\": {}}</tool>").is_none(),
            "never a write"
        );
        assert!(tool_call("```sql\nSELECT 1;\n```").is_none());
        // What the 3B wrote in the owner's test: prose, then the JSON.
        let owner = "Como no tengo acceso a la conexión, podés usar index_usage:\n{\"name\": \"index_usage\", \"arguments\": {\"database\": \"tenant-compras\"}}";
        let (n, a) = tool_call(owner).unwrap();
        assert_eq!(
            (n.as_str(), a["database"].as_str()),
            ("index_usage", Some("tenant-compras"))
        );
        assert_eq!(
            tool_call(
                "```json\n{\"name\": \"list_objects\", \"arguments\": {\"database\": \"b\"}}\n```"
            )
            .unwrap()
            .0,
            "list_objects"
        );
        assert_eq!(
            tool_call(
                "<tool_call>\n{\"name\": \"list_databases\", \"arguments\": {}}\n</tool_call>"
            )
            .unwrap()
            .0,
            "list_databases"
        );
        assert!(tool_call("el objeto {\"name\": \"Pepe\"} no es una herramienta").is_none());
        assert_eq!(
            tool_label(
                "describe_object",
                &serde_json::json!({"database": "t1", "object": "ventas.clientes"})
            ),
            "estructura de ventas.clientes en t1"
        );
    }

    /// Claude Code and Codex get the same protocol with the catalog tools
    /// only, in every language; local models keep the whole list.
    #[test]
    fn remote_models_get_only_the_metadata_tools() {
        for l in prompts::LANGS {
            let p = prompts::get(l);
            let local = tools_prompt(p, "c1", "ventas", true);
            let remote = tools_prompt(p, "c1", "ventas", false);
            for t in READS_ROWS {
                assert!(local.contains(&format!("- {t} ")), "{l}: {t}");
                assert!(!remote.contains(t), "{l}: {t} in\n{remote}");
            }
            for t in REMOTE_TOOLS {
                assert!(remote.contains(&format!("- {t} ")), "{l}: {t}");
            }
            // Only the data tools' lines and the one about approving them go.
            assert_eq!(
                remote.lines().count() + READS_ROWS.len() + 1,
                local.lines().count(),
                "{l}"
            );
            assert!(remote.contains(TOOL_OPEN) && remote.contains("c1"), "{l}");
            let example = remote
                .lines()
                .filter(|x| x.starts_with(TOOL_OPEN))
                .filter_map(|x| tool_call(x).map(|c| c.0))
                .collect::<Vec<_>>();
            assert!(
                !example.is_empty() && example.iter().all(|n| REMOTE_TOOLS.contains(&n.as_str())),
                "{l}: {example:?}"
            );
        }
    }

    #[test]
    fn remote_models_cant_read_rows() {
        for provider in [ProviderKind::ClaudeCode, ProviderKind::Codex] {
            for t in ["sample_rows", "run_query", "explain"] {
                let mut a =
                    serde_json::json!({"database": "ventas", "query": "select 1", "object": "t"});
                let why = prepare_call(provider, t, &mut a, "ventas").unwrap_err();
                assert!(why.contains(t) && why.contains("describe_object"), "{why}");
            }
            for t in REMOTE_TOOLS {
                assert!(prepare_call(provider, t, &mut serde_json::json!({}), "ventas").is_ok());
            }
        }
        // Local models keep every tool (rows still wait for the approval).
        for provider in [
            ProviderKind::Embedded,
            ProviderKind::Ollama,
            ProviderKind::LmStudio,
        ] {
            for t in crate::mcp::tools::ASSISTANT_TOOLS {
                assert!(prepare_call(provider, t, &mut serde_json::json!({}), "ventas").is_ok());
            }
        }
        // The answer the remote model gets is an <error> it can read, and
        // the call is parsed (so it's refused, not ignored).
        assert_eq!(
            tool_call(
                "<tool>{\"name\": \"sample_rows\", \"arguments\": {\"object\": \"t\"}}</tool>"
            )
            .unwrap()
            .0,
            "sample_rows"
        );
    }

    /// "the columns of inventario.productos" while the tab is on ventas: the
    /// call goes to inventario; one without a database reads the tab's.
    #[test]
    fn a_tool_reads_the_database_the_model_names() {
        for provider in [
            ProviderKind::ClaudeCode,
            ProviderKind::Codex,
            ProviderKind::Ollama,
        ] {
            let mut a = serde_json::json!({"database": "inventario", "object": "productos"});
            prepare_call(provider, "describe_object", &mut a, "ventas").unwrap();
            assert_eq!(a["database"], "inventario");
            let mut a = serde_json::json!({"object": "clientes"});
            prepare_call(provider, "describe_object", &mut a, "ventas").unwrap();
            assert_eq!(a["database"], "ventas");
            let mut a = serde_json::json!({"database": "inventario"});
            prepare_call(provider, "list_objects", &mut a, "ventas").unwrap();
            assert_eq!(a["database"], "inventario");
        }
    }

    #[test]
    fn missing_databases_are_reported() {
        let listed = vec!["ventas".to_string(), "Inventario".to_string()];
        assert_eq!(find_database(&listed, "ventas"), Ok(None));
        assert_eq!(
            find_database(&listed, "inventario"),
            Ok(Some("Inventario".into())),
            "other casing"
        );
        assert_eq!(find_database(&listed, "compras"), Err(()));
        assert_eq!(
            find_database(&[], "compras"),
            Ok(None),
            "no list: can't tell"
        );
        let two = vec!["A".to_string(), "a".to_string()];
        assert_eq!(find_database(&two, "a"), Ok(None));
        assert_eq!(find_database(&two, "b"), Err(()));
        let msg = missing_database("compras", "prod", &listed);
        assert!(
            msg.contains("“compras”")
                && msg.contains("“prod”")
                && msg.contains("ventas, Inventario"),
            "{msg}"
        );
        let many: Vec<String> = (0..60).map(|i| format!("b{i}")).collect();
        assert!(missing_database("x", "c", &many).contains("(10 more)"));
        let failed = other_database_failed("compras", "permission denied for database compras");
        assert!(failed.contains("“compras”") && failed.contains("permission denied"));
    }

    #[test]
    fn refusals() {
        assert!(refused("Lo siento, no puedo ayudarte con eso."));
        assert!(refused("I'm sorry, but I can't assist with that request."));
        assert!(refused("Entschuldigung, dabei kann ich nicht helfen."));
        assert!(refused("Извините, я не могу помочь с этим."));
        assert!(refused("抱歉，我无法帮助你完成这个请求。"));
        assert!(refused(
            "申し訳ありませんが、そのご依頼はお手伝いできません。"
        ));
        assert!(refused("죄송하지만 그 요청은 도와드릴 수 없습니다."));
        assert!(!refused("このクエリは表を削除し、元に戻せません。"));
        assert!(
            !refused("```sql\nSELECT 1;\n```\nLo siento, no puedo garantizar el orden."),
            "with code it answered"
        );
        assert!(!refused(
            "Esta consulta borra la tabla y no se puede deshacer."
        ));
        assert!(
            !refused(&format!("{} lo siento, no puedo", "x".repeat(700))),
            "a long answer isn't a refusal"
        );
    }

    #[test]
    fn dialect_hints() {
        let p = prompts::get("es");
        assert!(p
            .text(dialect_hint("mssql").unwrap())
            .contains("[base].esquema.tabla"));
        assert!(p.text(dialect_hint("sybase").unwrap()).contains("TOP n"));
        assert!(p.text(dialect_hint("postgres").unwrap()).contains("LIMIT"));
        assert!(prompts::get("en")
            .text(dialect_hint("oracle").unwrap())
            .contains("FETCH FIRST n ROWS ONLY"));
        assert!(dialect_hint("standard").is_none());
    }

    /// Every text that goes to the model comes from the UI language's file;
    /// the tags DBine reads the answer by are the same in all of them.
    #[test]
    fn instructions_follow_the_ui_language() {
        for l in prompts::LANGS {
            let p = prompts::get(l);
            let tools = tools_prompt(p, "c1", "b1", true);
            assert!(
                tools.contains(TOOL_OPEN) && tools.contains(TOOL_CLOSE),
                "{l}"
            );
            // The example in the prompt is a call DBine recognizes.
            let example = tools
                .lines()
                .find(|x| x.starts_with(TOOL_OPEN) && x.contains("list_databases"))
                .unwrap();
            assert_eq!(tool_call(example).unwrap().0, "list_databases", "{l}");
            for k in [
                "assistant.dialect_mssql",
                "assistant.dialect_postgres",
                "assistant.dialect_mysql",
                "assistant.dialect_oracle",
                "assistant.dialect_sqlite",
            ] {
                assert!(!p.text(k).is_empty(), "{l}: {k}");
            }
        }
        let en = prompts::get("en");
        assert!(en.text("assistant.system").contains("English"));
        assert!(!en.text("assistant.system").contains("Respondé"));
        assert!(prompts::get("es")
            .text("assistant.system")
            .contains("español"));
        let (_, shown, _) = compact_schema(&[table("t", &["id"])], "", 1, en);
        assert_eq!(shown, 0);
        let (text, _, _) = compact_schema(&[table("t", &["id"])], "", 1, en);
        assert!(!text.contains("Otras tablas"), "{text}");
    }

    #[test]
    fn context_items_serialize_for_the_ui() {
        let v = serde_json::to_value(vec![
            ContextItem::Engine {
                name: "PostgreSQL".into(),
            },
            ContextItem::Tables {
                shown: 3,
                total: 10,
            },
            ContextItem::LastError,
        ])
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!([{"kind": "engine", "name": "PostgreSQL"}, {"kind": "tables", "shown": 3, "total": 10}, {"kind": "last_error"}])
        );
    }
    use dbine_driver::{ColumnDef, ForeignKeyDef, KeyDef};

    fn table(name: &str, cols: &[&str]) -> TableSchema {
        TableSchema {
            kind: "table".into(),
            schema: Some("dbo".into()),
            name: name.into(),
            columns: cols
                .iter()
                .map(|c| ColumnDef {
                    name: (*c).into(),
                    data_type: "int".into(),
                    nullable: *c != "id",
                    ..Default::default()
                })
                .collect(),
            primary_key: Some(KeyDef {
                name: None,
                columns: vec!["id".into()],
            }),
            ..Default::default()
        }
    }

    #[test]
    fn compact_lines_with_keys() {
        let mut p = table("pedidos", &["id", "cliente_id"]);
        p.foreign_keys.push(ForeignKeyDef {
            columns: vec!["cliente_id".into()],
            ref_schema: Some("dbo".into()),
            ref_table: "clientes".into(),
            ref_columns: vec!["id".into()],
            ..Default::default()
        });
        let (text, shown, total) = compact_schema(&[p], "", 10_000, prompts::get("es"));
        assert_eq!((shown, total), (1, 1));
        assert_eq!(
            text,
            "dbo.pedidos(id int PK, cliente_id int)\n  FK (cliente_id) -> dbo.clientes(id)\n"
        );
    }

    #[test]
    fn mentioned_tables_come_first_when_it_doesnt_fit() {
        let mut tables: Vec<TableSchema> = (0..200)
            .map(|i| table(&format!("tabla_{i:03}"), &["id", "a", "b", "c"]))
            .collect();
        tables.push(table("facturas", &["id", "total"]));
        let (text, shown, total) = compact_schema(
            &tables,
            "sumá el total de FACTURAS por mes",
            2_000,
            prompts::get("es"),
        );
        assert_eq!(total, 201);
        assert!(shown < total);
        assert!(text.starts_with("dbo.facturas("), "{}", &text[..60]);
        assert!(text.contains("Otras tablas (sin detalle"));
        // "id" in the question doesn't pull every table.
        let (t2, _, _) = compact_schema(&tables, "where id = 1", 2_000, prompts::get("es"));
        assert!(t2.starts_with("dbo.tabla_000("));
    }
}
