//! AI assistant commands (docs/asistente-ia.md): detect the providers,
//! chat with streamed answers, and the built-in model's downloads. The
//! context sent with each question is built here: engine, database, its
//! structure (compact, trimmed to what fits), the editor's text and the last
//! error. Rows of data are never sent.

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dashmap::DashMap;
use dbine_ai::embedded::{self, CatalogModel};
use dbine_ai::{AiError, Cancel, ChatMessage, ChatRequest, Delta, Endpoints, ProviderInfo, ProviderKind};
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
}

impl AiRuntime {
    pub fn new(models_dir: std::path::PathBuf) -> Self {
        Self {
            endpoints: Endpoints { models_dir, ..Endpoints::default() },
            chats: DashMap::new(),
            downloads: DashMap::new(),
            schemas: DashMap::new(),
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
        let out = std::process::Command::new("sysctl").args(["-n", "hw.memsize"]).output().ok();
        if let Some(n) = out.and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok()) {
            return n / (1 << 30);
        }
    }
    #[cfg(target_os = "linux")]
    if let Ok(s) = std::fs::read_to_string("/proc/meminfo") {
        if let Some(kb) = s.lines().find(|l| l.starts_with("MemTotal")).and_then(|l| l.split_whitespace().nth(1)).and_then(|v| v.parse::<u64>().ok()) {
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
    let best = embedded::CATALOG.iter().filter(|m| u64::from(m.min_ram_gb) <= ram).map(|m| m.id).last();
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
        ollama_recommended_model: if ram >= 48 { "qwen2.5-coder:32b" } else { dbine_ai::ollama::RECOMMENDED_MODEL },
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
    #[serde(default = "yes")]
    pub include_schema: bool,
}

fn yes() -> bool {
    true
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
}

#[derive(Clone, Serialize)]
struct DeltaEvent<'a> {
    chat_id: &'a str,
    delta: Delta,
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
    /// What went as context (for the UI's "what was sent" line).
    pub context_summary: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn ai_chat(app: AppHandle, state: State<'_, AppState>, ai: State<'_, AiRuntime>, args: ChatArgs) -> CommandResult<ChatOut> {
    let cancel = Cancel::new();
    ai.chats.insert(args.chat_id.clone(), cancel.clone());
    let id = args.chat_id.clone();
    let result = async {
        let (system, summary) = build_system(&app, &state, &ai, &args).await;
        // The built-in model's engine, the first time (models downloaded
        // before it came separately).
        if args.provider == ProviderKind::Embedded {
            let progress = |done: u64, total: u64| {
                let pct = if total > 0 { done * 100 / total } else { 0 };
                let note = format!("{pct} % de {} MB", total.div_ceil(1_000_000));
                let _ = app.emit("ai-status", StatusEvent { chat_id: &id, phase: "engine", note: Some(note) });
            };
            embedded::ensure_engine(&ai.endpoints.models_dir, &progress, &cancel).await?;
        }
        let _ = app.emit("ai-status", StatusEvent { chat_id: &id, phase: "thinking", note: None });
        let mut req = ChatRequest { kind: args.provider, model: args.model.clone(), system, messages: args.messages.clone() };
        let emit = |d: Delta| {
            let _ = app.emit("ai-delta", DeltaEvent { chat_id: &id, delta: d });
        };
        let mut text = dbine_ai::chat(&req, &ai.endpoints, &emit, &cancel).await?;
        // Small local models sometimes refuse a legitimate ask: once, the
        // same question again with a line saying it's fine. The UI drops
        // the refusal it showed on "retry".
        if refused(&text) {
            let _ = app.emit("ai-status", StatusEvent { chat_id: &id, phase: "retry", note: None });
            req.system.push_str(AFTER_REFUSAL);
            text = dbine_ai::chat(&req, &ai.endpoints, &emit, &cancel).await?;
        }
        Ok(ChatOut { text, context_summary: summary })
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
pub async fn ai_download_model(app: AppHandle, ai: State<'_, AiRuntime>, args: IdArgs) -> CommandResult<()> {
    let m = embedded::catalog_model(&args.id).ok_or_else(|| CommandError::NotFound("modelo desconocido".into()))?;
    if ai.downloads.contains_key(m.id) {
        return Err(CommandError::BadRequest("ya se está descargando".into()));
    }
    let cancel = Cancel::new();
    ai.downloads.insert(m.id.to_string(), cancel.clone());
    let dir = &ai.endpoints.models_dir;
    let engine = embedded::engine_pending(dir);
    let total = engine + m.size;
    let progress = |done: u64| {
        let _ = app.emit("ai-download", DownloadEvent { id: m.id, status: "descargando", done, total });
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
    let m = embedded::catalog_model(&args.id).ok_or_else(|| CommandError::NotFound("modelo desconocido".into()))?;
    Ok(embedded::delete(&ai.endpoints.models_dir, m)?)
}

#[tauri::command]
pub async fn ai_start_ollama(ai: State<'_, AiRuntime>) -> CommandResult<()> {
    Ok(dbine_ai::ollama::start(&ai.endpoints.ollama).await?)
}

/// Download a model into Ollama (`ollama pull`), with progress.
#[tauri::command(rename_all = "camelCase")]
pub async fn ai_pull_ollama(app: AppHandle, ai: State<'_, AiRuntime>, args: IdArgs) -> CommandResult<()> {
    let key = format!("ollama:{}", args.id);
    let progress = |status: &str, done: u64, total: u64| {
        let _ = app.emit("ai-download", DownloadEvent { id: &key, status, done, total });
    };
    Ok(dbine_ai::ollama::pull(&ai.endpoints.ollama, &args.id, &progress).await?)
}

// -- context ----------------------------------------------------------------------

/// How much structure fits: small local models get less.
fn schema_budget(kind: ProviderKind) -> usize {
    match kind {
        ProviderKind::Embedded | ProviderKind::Ollama | ProviderKind::LmStudio => 24_000,
        ProviderKind::ClaudeCode | ProviderKind::Codex => 150_000,
    }
}

async fn build_system(app: &AppHandle, state: &AppState, ai: &AiRuntime, args: &ChatArgs) -> (String, String) {
    let ctx = &args.context;
    let mut s = String::from(
        "Sos el asistente de DBine, un gestor de bases de datos de escritorio. Ayudás a escribir, explicar, optimizar y corregir consultas y a entender la estructura de la base.\n\n\
         Reglas:\n\
         - Usá solo tablas, colecciones y columnas que existan en la estructura de abajo. Si falta algo o no hay estructura, decilo y pedí el dato en vez de inventarlo.\n\
         - Poné el código en bloques de código con la sintaxis exacta del motor.\n\
         - Sé breve: primero el código, después una explicación corta si hace falta.\n\
         - Si algo modifica o borra datos o estructura (UPDATE, DELETE, DROP, TRUNCATE, ALTER…), avisalo claramente y sugerí probarlo antes (un SELECT equivalente o una transacción).\n\
         - No podés ejecutar nada ni conectarte a la base: solo escribís código. La ejecución es siempre del usuario. Si te pide ejecutar, borrar, crear o cambiar algo («borrá la tabla X»), escribí la consulta que lo hace; DBine la agrega al final de la query que tiene abierta y él decide si la ejecuta. Nunca digas que ejecutaste algo ni inventes resultados.\n\
         - Si te pide revisar o corregir lo que hay en el editor, devolvé el contenido completo corregido en un solo bloque de código (DBine le ofrece reemplazar el editor con él) y explicá brevemente qué cambiaste.\n\
         - Respondé en el idioma en que te escriben.\n",
    );
    let mut summary: Vec<String> = Vec::new();
    let conn = ctx.connection_id.as_deref().and_then(|id| state.store.get_connection(id).ok().flatten());
    if let Some(conn) = &conn {
        let info = dbine_drivers::find(&conn.config.driver).map(|d| d.info());
        let db = ctx.database.clone().unwrap_or_default();
        if let Some(info) = info {
            let lang = match info.language {
                Language::Sql => format!("SQL, dialecto {}", info.dialect),
                Language::Cql => "CQL (Cassandra)".into(),
                Language::Json => "comandos / documentos JSON".into(),
                Language::Redis => "comandos de Redis, uno por línea".into(),
                Language::Flux => "Flux (InfluxDB)".into(),
                Language::Cypher => "Cypher".into(),
            };
            s.push_str(&format!("\nMotor: {} · lenguaje de consultas: {lang}.\n", info.name));
            if let Some(hint) = dialect_hint(info.dialect) {
                s.push_str(hint);
            }
            summary.push(info.name.to_string());
        }
        if !db.is_empty() {
            s.push_str(&format!("Base de datos actual: {db}.\n"));
            summary.push(db.clone());
        }
        if conn.config.read_only {
            s.push_str("La conexión es de solo lectura: DBine bloquea todo lo que no sea lectura.\n");
        }
        if let Some(o) = &ctx.object {
            s.push_str(&format!("El usuario está mirando el objeto {o}.\n"));
        }
        if ctx.include_schema {
            let _ = app.emit("ai-status", StatusEvent { chat_id: &args.chat_id, phase: "schema", note: None });
            match schema_for(state, ai, &conn.id, &db).await {
                Ok(tables) => {
                    let question = args.messages.iter().map(|m| m.content.as_str()).collect::<Vec<_>>().join("\n");
                    let hint = format!("{question}\n{}\n{}", ctx.editor_sql.as_deref().unwrap_or(""), ctx.object.as_deref().unwrap_or(""));
                    let (text, shown, total) = compact_schema(&tables, &hint, schema_budget(args.provider));
                    s.push_str(&format!("\n<estructura>\n{text}</estructura>\n"));
                    summary.push(if shown == total { format!("{total} tablas") } else { format!("{shown} de {total} tablas") });
                }
                Err(e) => {
                    s.push_str(&format!("\n(No se pudo leer la estructura de la base: {e}.)\n"));
                    let _ = app.emit("ai-status", StatusEvent { chat_id: &args.chat_id, phase: "schema", note: Some(format!("sin estructura: {e}")) });
                }
            }
        }
    } else {
        s.push_str("\nNo hay ninguna base seleccionada: respondé en general o pedí que abra una.\n");
    }
    if let Some(sql) = ctx.editor_sql.as_deref().filter(|t| !t.trim().is_empty()) {
        s.push_str(&format!("\n<editor>\n{}\n</editor>\n", truncate(sql, 20_000)));
        summary.push("editor".into());
    }
    if let Some(sel) = ctx.selection.as_deref().filter(|t| !t.trim().is_empty()) {
        s.push_str(&format!("\n<seleccion>\n{}\n</seleccion>\n(Si el usuario dice «esto», se refiere a la selección.)\n", truncate(sel, 10_000)));
        summary.push("selección".into());
    }
    if let Some(err) = ctx.last_error.as_deref().filter(|t| !t.trim().is_empty()) {
        s.push_str(&format!("\n<ultimo_error>\n{}\n</ultimo_error>\n", truncate(err, 4_000)));
        summary.push("último error".into());
    }
    // Small models follow what comes last, and an example, best.
    s.push_str(
        "\nRecordá: vos no ejecutás nada, solo escribís la consulta. Nunca digas que hiciste algo («borré», «borró», «se borró», «ejecuté», «eliminé», «creé»): \
         decí qué hace la consulta y que el usuario la ejecuta cuando quiera.\n\n\
         Ejemplo. Usuario: «borrá la tabla ventas». Respuesta:\n\
         ```sql\nDROP TABLE ventas;\n```\n\
         Esta consulta borra la tabla `ventas` con todos sus datos y no se puede deshacer. No se ejecutó: revisala y ejecutala vos cuando estés seguro.\n",
    );
    (s, summary.join(" · "))
}

/// What small models get wrong most in each dialect: quoting, naming another
/// database, limiting rows.
fn dialect_hint(dialect: &str) -> Option<&'static str> {
    Some(match dialect {
        "mssql" | "sybase" => {
            "Sintaxis de SQL Server: los nombres con guiones, espacios o palabras reservadas van entre corchetes ([mi-base], [Order]). \
             Una tabla de otra base del mismo servidor se nombra con tres partes: [base].esquema.tabla. \
             Para limitar filas, TOP n después de SELECT (no existe LIMIT). En un UNION, una parte con TOP y su propio ORDER BY va dentro de una subconsulta: \
             SELECT * FROM (SELECT TOP 10 … FROM [b1].dbo.t ORDER BY Fecha DESC) AS t1 UNION ALL SELECT * FROM (…) AS t2.\n"
        }
        "postgres" => {
            "Sintaxis de PostgreSQL: los nombres con mayúsculas, guiones o palabras reservadas van entre comillas dobles (\"MiTabla\"). \
             Para limitar filas, LIMIT n. Una consulta no puede leer tablas de otra base (cada base es aparte, salvo dblink o postgres_fdw).\n"
        }
        "mysql" => {
            "Sintaxis de MySQL: los nombres con guiones o palabras reservadas van entre backticks (`mi-base`). \
             Una tabla de otra base del mismo servidor se nombra base.tabla. Para limitar filas, LIMIT n.\n"
        }
        "oracle" => {
            "Sintaxis de Oracle: los nombres con minúsculas, guiones o palabras reservadas van entre comillas dobles. \
             Una tabla de otro esquema se nombra ESQUEMA.TABLA. Para limitar filas, FETCH FIRST n ROWS ONLY (12c en adelante); no existe LIMIT ni TOP.\n"
        }
        "sqlite" => "Sintaxis de SQLite: para limitar filas, LIMIT n. Cada base es un archivo; otra base se usa con ATTACH.\n",
        _ => return None,
    })
}

/// The model refused a legitimate request ("Lo siento, no puedo ayudarte con
/// eso"): a short answer without code that apologizes or says it can't.
fn refused(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    if t.is_empty() || t.contains("```") || t.chars().count() > 600 {
        return false;
    }
    const SIGNS: &[&str] = &[
        "no puedo ayudar", "no puedo asistir", "no puedo hacer eso", "no puedo proporcionar", "lo siento, no puedo", "lo siento, pero no puedo",
        "can't help with", "cannot help with", "can't assist", "cannot assist", "i'm sorry, but i can", "i am sorry, but i can",
        "não posso ajudar", "desculpe, mas não posso", "je ne peux pas vous aider", "je ne peux pas t'aider", "non posso aiutar",
    ];
    SIGNS.iter().any(|s| t.contains(s))
}

/// Added to the prompt on the second try after a refusal.
const AFTER_REFUSAL: &str = "\nEl pedido anterior es legítimo: es la base del propio usuario y él decide qué ejecutar. \
     No te niegues ni pidas disculpas: escribí la consulta que pide (DBine no la ejecuta) y, si es riesgosa o toca datos sensibles, avisalo en una línea.\n";

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (recortado)", &s[..end])
}

async fn schema_for(state: &AppState, ai: &AiRuntime, connection_id: &str, database: &str) -> CommandResult<Arc<Vec<TableSchema>>> {
    let key = format!("{connection_id}\u{0}{database}");
    if let Some(e) = ai.schemas.get(&key) {
        if e.0.elapsed() < Duration::from_secs(600) {
            return Ok(e.1.clone());
        }
    }
    let tables = state.meta_read(connection_id, database, Duration::from_secs(60), |s| Box::pin(s.database_schema())).await?;
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

fn table_line(t: &TableSchema) -> String {
    let pk: Vec<&str> = t.primary_key.as_ref().map(|k| k.columns.iter().map(String::as_str).collect()).unwrap_or_default();
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
    let kind = if t.kind.is_empty() || t.kind == "table" { String::new() } else { format!("[{}] ", t.kind) };
    let mut line = format!("{kind}{}({})", qualified(t), cols.join(", "));
    for fk in &t.foreign_keys {
        let target = match &fk.ref_schema {
            Some(s) if !s.is_empty() => format!("{s}.{}", fk.ref_table),
            _ => fk.ref_table.clone(),
        };
        line.push_str(&format!("\n  FK ({}) -> {target}({})", fk.columns.join(", "), fk.ref_columns.join(", ")));
    }
    if let Some(c) = t.comment.as_deref().filter(|c| !c.is_empty()) {
        line.push_str(&format!("\n  -- {}", truncate(c, 200)));
    }
    line.push('\n');
    line
}

/// The structure in a compact form within `budget` characters. Tables named
/// in `hint` (the question, the editor) go first and whole; the rest whole
/// while they fit, then by name only. Returns (text, tables with columns,
/// total).
fn compact_schema(tables: &[TableSchema], hint: &str, budget: usize) -> (String, usize, usize) {
    let hint = hint.to_lowercase();
    let mentioned = |t: &TableSchema| {
        let n = t.name.to_lowercase();
        // Whole-word-ish match, so "id" or "a" don't match everything.
        n.len() > 2 && hint.match_indices(&n).any(|(i, _)| {
            let before = hint[..i].chars().last().is_none_or(|c| !c.is_alphanumeric() && c != '_');
            let after = hint[i + n.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric() && c != '_');
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
        let line = table_line(t);
        if out.len() + line.len() <= budget {
            out.push_str(&line);
            shown += 1;
        } else {
            names_only.push(qualified(t));
        }
    }
    if !names_only.is_empty() {
        let mut rest = format!("Otras tablas (sin detalle, pedí las que necesites): {}\n", names_only.join(", "));
        if out.len() + rest.len() > budget + 20_000 {
            rest = truncate(&rest, 20_000);
        }
        out.push_str(&rest);
    }
    (out, shown, tables.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals() {
        assert!(refused("Lo siento, no puedo ayudarte con eso."));
        assert!(refused("I'm sorry, but I can't assist with that request."));
        assert!(!refused("```sql\nSELECT 1;\n```\nLo siento, no puedo garantizar el orden."), "with code it answered");
        assert!(!refused("Esta consulta borra la tabla y no se puede deshacer."));
        assert!(!refused(&format!("{} lo siento, no puedo", "x".repeat(700))), "a long answer isn't a refusal");
    }

    #[test]
    fn dialect_hints() {
        assert!(dialect_hint("mssql").unwrap().contains("[base].esquema.tabla"));
        assert!(dialect_hint("postgres").unwrap().contains("LIMIT"));
        assert!(dialect_hint("standard").is_none());
    }
    use dbine_driver::{ColumnDef, ForeignKeyDef, KeyDef};

    fn table(name: &str, cols: &[&str]) -> TableSchema {
        TableSchema {
            kind: "table".into(),
            schema: Some("dbo".into()),
            name: name.into(),
            columns: cols
                .iter()
                .map(|c| ColumnDef { name: (*c).into(), data_type: "int".into(), nullable: *c != "id", ..Default::default() })
                .collect(),
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }),
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
        let (text, shown, total) = compact_schema(&[p], "", 10_000);
        assert_eq!((shown, total), (1, 1));
        assert_eq!(text, "dbo.pedidos(id int PK, cliente_id int)\n  FK (cliente_id) -> dbo.clientes(id)\n");
    }

    #[test]
    fn mentioned_tables_come_first_when_it_doesnt_fit() {
        let mut tables: Vec<TableSchema> = (0..200).map(|i| table(&format!("tabla_{i:03}"), &["id", "a", "b", "c"])).collect();
        tables.push(table("facturas", &["id", "total"]));
        let (text, shown, total) = compact_schema(&tables, "sumá el total de FACTURAS por mes", 2_000);
        assert_eq!(total, 201);
        assert!(shown < total);
        assert!(text.starts_with("dbo.facturas("), "{}", &text[..60]);
        assert!(text.contains("Otras tablas (sin detalle"));
        // "id" in the question doesn't pull every table.
        let (t2, _, _) = compact_schema(&tables, "where id = 1", 2_000);
        assert!(t2.starts_with("dbo.tabla_000("));
    }
}
