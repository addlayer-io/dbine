//! "Optimizar consulta" (docs/optimizar-consulta.md): analyze a query
//! (rule rewrites, notes, index suggestions from its estimated plan), ask
//! the configured AI for alternatives, and compare the versions on a
//! read-only session of their own (`cancel_query` / `optimizer_cancel` on
//! `optimize:<id>`). Nothing here writes: index scripts and rewrites only
//! travel to the UI, which opens them in a query.

use crate::commands::ai::AiRuntime;
use crate::commands::schema::driver_of;
use crate::error::{CommandError, CommandResult};
use crate::optimizer::compare::{self, Measure, Options, Version};
use crate::optimizer::hints::{self, IndexHint, PlanWarning};
use crate::optimizer::{self, ai as prompts, Candidate, Note};
use crate::state::AppState;
use dashmap::DashMap;
use dbine_ai::{embedded, Cancel, ChatMessage, ChatRequest, ProviderKind};
use dbine_driver::{DdlParts, Driver, Language, Plan, QueryOutcome, TableSchema};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};

/// Rows hashed per version unless the UI says otherwise.
const DEFAULT_MAX_ROWS: usize = 100_000;
/// Structure read for the rules and the AI, per connection + database.
const SCHEMA_TTL: Duration = Duration::from_secs(600);

fn schemas() -> &'static DashMap<String, (Instant, Arc<Vec<TableSchema>>)> {
    static S: OnceLock<DashMap<String, (Instant, Arc<Vec<TableSchema>>)>> = OnceLock::new();
    S.get_or_init(DashMap::new)
}

/// AI requests in flight, by the UI's id.
fn ai_runs() -> &'static DashMap<String, Cancel> {
    static R: OnceLock<DashMap<String, Cancel>> = OnceLock::new();
    R.get_or_init(DashMap::new)
}

async fn structure(state: &AppState, connection_id: &str, database: &str) -> CommandResult<Arc<Vec<TableSchema>>> {
    let key = format!("{connection_id}\u{0}{database}");
    if let Some(e) = schemas().get(&key) {
        if e.0.elapsed() < SCHEMA_TTL {
            return Ok(e.1.clone());
        }
    }
    let tables = Arc::new(state.meta_read(connection_id, database, Duration::from_secs(60), |s| Box::pin(s.database_schema())).await?);
    schemas().insert(key, (Instant::now(), tables.clone()));
    Ok(tables)
}

/// The statement writes (SQL only; other languages are refused by the
/// read-only session itself).
pub(crate) fn writes(driver: &dyn Driver, sql: &str) -> bool {
    driver.info().language == Language::Sql && dbine_driver::read_only::first_write_in(sql, &driver.script_dialect()).is_some()
}

/// CREATE INDEX in the driver's language; a plain one when the driver
/// doesn't generate DDL.
fn index_script(driver: &dyn Driver, t: &TableSchema) -> Option<String> {
    let parts = DdlParts { indexes: true, ..Default::default() };
    if let Ok(s) = driver.table_ddl(t, parts) {
        if !s.trim().is_empty() {
            return Some(s);
        }
    }
    let info = driver.info();
    if info.language != Language::Sql {
        return None;
    }
    let ix = t.indexes.first()?;
    let id = |n: &str| optimizer::rules::ident(info.dialect, n);
    let table = match t.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{}.{}", id(s), id(&t.name)),
        None => id(&t.name),
    };
    Some(format!("CREATE INDEX {} ON {table} ({});", id(&ix.name), ix.columns.iter().map(|c| id(c)).collect::<Vec<_>>().join(", ")))
}

#[derive(Deserialize)]
pub struct AnalyzeArgs {
    pub connection_id: String,
    pub database: String,
    pub sql: String,
    /// The UI's id (its `cancel_query` target is `optimize:<id>`).
    pub run_id: String,
}

#[derive(Serialize)]
pub struct Analysis {
    pub language: Language,
    pub dialect: String,
    pub engine: String,
    /// The query writes: it's never run, "Comparar" compares estimated plans.
    pub writes: bool,
    pub supports_explain: bool,
    pub candidates: Vec<Candidate>,
    pub notes: Vec<Note>,
    pub hints: Vec<IndexHint>,
    pub warnings: Vec<PlanWarning>,
    /// The original's estimated plans.
    pub plans: Vec<Plan>,
    pub cost: Option<f64>,
    /// What couldn't be read, with why.
    pub skipped: Vec<String>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn optimizer_analyze(state: State<'_, AppState>, args: AnalyzeArgs) -> CommandResult<Analysis> {
    let driver = driver_of(&state, &args.connection_id)?;
    let info = driver.info();
    let mut skipped = Vec::new();
    let tables = if info.language == Language::Sql {
        match structure(&state, &args.connection_id, &args.database).await {
            Ok(t) => Some(t),
            Err(e) => {
                skipped.push(format!("Estructura de las tablas: {e}"));
                None
            }
        }
    } else {
        None
    };
    let mut plans = Vec::new();
    if driver.supports_explain() {
        let key = format!("optimize:{}", args.run_id);
        let entry = state.dedicated_session(&key, &args.connection_id, &args.database, true).await?;
        let mut out = QueryOutcome::default();
        let r = {
            let mut s = entry.session.lock().await;
            tokio::select! {
                r = s.explain(&args.sql, false, 100, &mut out) => Some(r),
                _ = entry.cancel.notified() => None,
            }
        };
        state.sessions.remove(&key);
        match r {
            Some(Ok(())) => plans = out.plans,
            Some(Err(e)) => skipped.push(format!("Plan de ejecución: {e}")),
            None => return Err(CommandError::Cancelled),
        }
    }
    Ok(build(driver.as_ref(), &args.sql, tables.as_deref().map(Vec::as_slice), plans, skipped))
}

/// The analysis of `sql` once the structure and the plans are read.
pub(crate) fn build(driver: &dyn Driver, sql: &str, tables: Option<&[TableSchema]>, plans: Vec<Plan>, skipped: Vec<String>) -> Analysis {
    let info = driver.info();
    let (candidates, notes) = optimizer::rewrites(sql, info.language, info.dialect, tables);
    let ddl = |t: &TableSchema| index_script(driver, t);
    let found = hints::from_plans(&plans, sql, info.dialect, info.language != Language::Sql, tables, &ddl);
    let costs: Vec<f64> = plans.iter().filter_map(|p| p.root.total_cost).collect();
    Analysis {
        language: info.language,
        dialect: info.dialect.to_string(),
        engine: info.name.to_string(),
        writes: writes(driver, sql),
        supports_explain: driver.supports_explain(),
        candidates,
        notes,
        hints: found.hints,
        warnings: found.warnings,
        cost: (!costs.is_empty()).then(|| costs.iter().sum()),
        plans,
        skipped,
    }
}

#[derive(Deserialize)]
pub struct AiArgs {
    pub connection_id: String,
    pub database: String,
    pub sql: String,
    pub run_id: String,
    pub provider: ProviderKind,
    #[serde(default)]
    pub model: Option<String>,
    /// The original's plans, summarized for the prompt.
    #[serde(default)]
    pub plans: Vec<Plan>,
    /// The UI's language (`en`, `pt`…): the titles and explanations come in it.
    #[serde(default)]
    pub ui_language: Option<String>,
}

#[derive(Serialize)]
pub struct AiOut {
    pub candidates: Vec<Candidate>,
    /// The model answered that it sees no improvement.
    pub none: bool,
    /// What went with the query ("3 tablas · plan").
    pub sent: Vec<String>,
    /// Alternatives left out because they didn't compile, even after the
    /// AI was asked to fix them.
    pub discarded: usize,
}

/// The engine's error for each candidate that doesn't compile: its estimated
/// plan is asked for on a read-only session, so nothing runs. Engines without
/// plans can't tell: their candidates pass, and Compare shows any error.
async fn compile_errors(state: &AppState, driver: &dyn Driver, args: &AiArgs, cands: &[Candidate]) -> CommandResult<Vec<Option<String>>> {
    if !driver.supports_explain() || cands.is_empty() {
        return Ok(vec![None; cands.len()]);
    }
    let key = format!("optimize:{}", args.run_id);
    let entry = state.dedicated_session(&key, &args.connection_id, &args.database, true).await?;
    let mut errors = Vec::with_capacity(cands.len());
    let mut cancelled = false;
    for c in cands {
        let mut out = QueryOutcome::default();
        let mut s = entry.session.lock().await;
        let r = tokio::select! {
            r = s.explain(&c.sql, false, 1, &mut out) => r,
            _ = entry.cancel.notified() => { cancelled = true; break; }
        };
        errors.push(r.err().map(|e| e.to_string()));
    }
    state.sessions.remove(&key);
    if cancelled {
        return Err(CommandError::Cancelled);
    }
    Ok(errors)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn optimizer_ai(state: State<'_, AppState>, ai: State<'_, AiRuntime>, args: AiArgs) -> CommandResult<AiOut> {
    let driver = driver_of(&state, &args.connection_id)?;
    let info = driver.info();
    let cancel = Cancel::new();
    ai_runs().insert(args.run_id.clone(), cancel.clone());
    let result = async {
        let mut sent = Vec::new();
        let budget = if args.provider.local() { 12_000 } else { 60_000 };
        let structure_text = match structure(&state, &args.connection_id, &args.database).await {
            Ok(t) => {
                let s = prompts::structure(&t, &args.sql, budget);
                if !s.is_empty() {
                    sent.push(format!("{} tablas", s.lines().filter(|l| !l.starts_with("  ")).count()));
                }
                s
            }
            Err(e) => {
                tracing::debug!("optimizer: no structure for the AI: {e}");
                String::new()
            }
        };
        let plan = hints::summary(&args.plans, 80);
        if !plan.is_empty() {
            sent.push("plan".into());
        }
        let language = match info.language {
            Language::Sql => format!("SQL, dialecto {}", info.dialect),
            Language::Cql => "CQL".into(),
            Language::Json => "comandos JSON / mongosh".into(),
            Language::Redis => "comandos".into(),
            Language::Flux => "Flux".into(),
            Language::Cypher => "Cypher".into(),
        };
        if args.provider == ProviderKind::Embedded {
            embedded::ensure_engine(&ai.endpoints.models_dir, &|_, _| {}, &cancel).await?;
        }
        let req = ChatRequest {
            kind: args.provider,
            model: args.model.clone(),
            system: prompts::system_prompt(info.name, &language, prompts::answer_language(args.ui_language.as_deref().unwrap_or("es"))),
            messages: vec![ChatMessage { role: "user".into(), content: prompts::user_prompt(&args.sql, &structure_text, &plan) }],
        };
        let text = dbine_ai::chat(&req, &ai.endpoints, &|_| {}, &cancel).await?;
        let first = prompts::parse(&text, &args.sql);
        let none = first.is_empty() && text.to_uppercase().contains("NINGUNA");

        // Only alternatives that compile reach the user; the others go back
        // to the AI once, with the engine's error, to be fixed.
        let errors = compile_errors(&state, driver.as_ref(), &args, &first).await?;
        let (mut candidates, mut failed) = (Vec::new(), Vec::new());
        for (c, e) in first.into_iter().zip(errors) {
            match e {
                None => candidates.push(c),
                Some(e) => failed.push((c, e)),
            }
        }
        let mut discarded = failed.len();
        if !failed.is_empty() {
            let mut req = req;
            req.messages.push(ChatMessage { role: "assistant".into(), content: text });
            req.messages.push(ChatMessage { role: "user".into(), content: prompts::repair_prompt(&failed) });
            let fixed_text = dbine_ai::chat(&req, &ai.endpoints, &|_| {}, &cancel).await?;
            let fixed: Vec<Candidate> = prompts::parse(&fixed_text, &args.sql)
                .into_iter()
                .filter(|f| !candidates.iter().chain(failed.iter().map(|(c, _)| c)).any(|c: &Candidate| c.sql.trim() == f.sql.trim()))
                .take(failed.len())
                .collect();
            let errors = compile_errors(&state, driver.as_ref(), &args, &fixed).await?;
            for (c, e) in fixed.into_iter().zip(errors) {
                match e {
                    None => {
                        candidates.push(c);
                        discarded -= 1;
                    }
                    Some(e) => tracing::debug!("optimizer: an AI alternative still doesn't compile: {e}"),
                }
            }
        }
        for (i, c) in candidates.iter_mut().enumerate() {
            c.id = format!("ai-{}-{i}", args.run_id);
        }
        Ok(AiOut { candidates, none, sent, discarded })
    }
    .await;
    ai_runs().remove(&args.run_id);
    result
}

#[derive(Deserialize)]
pub struct CompareArgs {
    pub connection_id: String,
    pub database: String,
    pub run_id: String,
    /// The original first, then the candidates.
    pub versions: Vec<Version>,
    #[serde(default = "default_runs")]
    pub runs: u32,
    #[serde(default)]
    pub max_rows: Option<usize>,
}

fn default_runs() -> u32 {
    3
}

#[derive(Serialize, Clone)]
struct Progress<'a> {
    run_id: &'a str,
    measure: &'a Measure,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn optimizer_compare(app: AppHandle, state: State<'_, AppState>, args: CompareArgs) -> CommandResult<Vec<Measure>> {
    let driver = driver_of(&state, &args.connection_id)?;
    let info = driver.info();
    let Some(original) = args.versions.first() else { return Ok(Vec::new()) };
    let original_writes = writes(driver.as_ref(), &original.sql);
    let ordered = optimizer::ordered(&original.sql, info.language, info.dialect);
    let key = format!("optimize:{}", args.run_id);
    let entry = state.dedicated_session(&key, &args.connection_id, &args.database, true).await?;
    let mut measures = Vec::new();
    {
        let mut s = entry.session.lock().await;
        for v in &args.versions {
            if entry.cancelled.load(Ordering::SeqCst) {
                break;
            }
            let o = Options {
                runs: args.runs.clamp(1, 20),
                max_rows: args.max_rows.unwrap_or(DEFAULT_MAX_ROWS).max(1),
                ordered,
                // What writes only gets its estimated plan; a read the original isn't is refused the same way.
                execute: !original_writes && !writes(driver.as_ref(), &v.sql),
                explain: driver.supports_explain(),
            };
            let cancelled = || entry.cancelled.load(Ordering::SeqCst);
            let m = tokio::select! {
                m = compare::measure(&mut **s, v, &o, &cancelled) => m,
                _ = entry.cancel.notified() => break,
            };
            let _ = app.emit("optimizer-progress", Progress { run_id: &args.run_id, measure: &m });
            measures.push(m);
        }
    }
    let cancelled = entry.cancelled.load(Ordering::SeqCst);
    state.sessions.remove(&key);
    if cancelled && measures.len() < args.versions.len() {
        return Err(CommandError::Cancelled);
    }
    compare::mark_equivalence(&mut measures);
    Ok(measures)
}

#[derive(Deserialize)]
pub struct CancelArgs {
    pub run_id: String,
}

/// Stop an analysis, an AI request or a comparison.
#[tauri::command(rename_all = "camelCase")]
pub async fn optimizer_cancel(state: State<'_, AppState>, args: CancelArgs) -> CommandResult<()> {
    if let Some((_, c)) = ai_runs().remove(&args.run_id) {
        c.cancel();
    }
    if let Some(entry) = state.sessions.get(&format!("optimize:{}", args.run_id)).map(|e| e.clone()) {
        entry.cancelled.store(true, Ordering::SeqCst);
        if let Some(i) = &entry.interrupter {
            i();
        }
        entry.cancel.notify_waiters();
    }
    Ok(())
}
