//! `execute`: code that changes data or structure, on connections at the
//! `write` level. Nothing runs before the user approves it in DBine
//! (`approvals`); unanswered, it's rejected. The log gets the request and
//! then its outcome (approved and run, rejected, or no answer).

use super::activity::ActivityEntry;
use super::approvals::{ApprovalRequest, Outcome, APPROVAL_TIMEOUT};
use super::tools::{arg, arg_num, failure, find_connection, results_text, run_on, summary, Op, TIMEOUT_DEFAULT, TIMEOUT_MAX};
use super::{effective_level, load_config, Cap, Inner, McpClient, McpLevel};
use dbine_core::SavedConnection;
use serde_json::{json, Value};
use std::time::Duration;

pub const TOOL: &str = "execute";
/// How many rows a statement that returns rows shows.
const SHOW_MAX: u64 = 100;

pub fn definition(conn: Value, db: Value) -> Value {
    json!({
        "name": TOOL,
        "description": format!(
            "Run code that changes data or structure (INSERT, UPDATE, DELETE, DDL, a Mongo write command, Redis SET…) in the \
engine's own language. Needs the 'write' level. Nothing runs until the user approves this exact code in DBine; the call \
waits up to {} seconds for the answer and is rejected without one. Returns what was run (rows affected, or the rows it \
returned), or says it was rejected. For reads use run_query.",
            APPROVAL_TIMEOUT.as_secs()
        ),
        "inputSchema": { "type": "object", "properties": {
            "connection": conn, "database": db,
            "code": { "type": "string", "description": "The exact code to run, as the user will see it." },
            "timeout_seconds": { "type": "integer", "minimum": 1, "maximum": TIMEOUT_MAX, "default": TIMEOUT_DEFAULT,
                "description": "Limit for running it once approved (the wait for the approval doesn't count)." }
        }, "required": ["connection", "database", "code"] },
        "annotations": { "readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false },
    })
}

/// What the log calls each step (the UI names them).
const REQUEST: &str = "execute:request";
const APPROVED: &str = "execute:approved";
const AUTO_APPROVED: &str = "execute:auto_approved";
const REJECTED: &str = "execute:rejected";
const TIMEOUT: &str = "execute:timeout";

pub async fn call(inner: &Inner, client: &McpClient, args: &Value) -> (String, bool) {
    let mut connection = String::new();
    let log = |tool: &str, connection: &str, ok: bool, rows: Option<u64>, error: Option<String>| {
        inner.activity.record(&ActivityEntry {
            id: 0,
            at: chrono::Utc::now().to_rfc3339(),
            client: client.name.clone(),
            connection: connection.to_string(),
            tool: tool.to_string(),
            summary: summary(TOOL, args),
            ok,
            rows,
            error,
        })
    };
    let prepared = match prepare(inner, client, args, &mut connection).await {
        Ok(p) => p,
        Err(e) => {
            log(TOOL, &connection, false, None, Some(e.clone()));
            return (e, true);
        }
    };
    log(REQUEST, &connection, true, None, None);
    let outcome = inner.approvals.ask(prepared.request, APPROVAL_TIMEOUT).await;
    let step = match outcome {
        Outcome::Rejected => {
            let e = "rechazado por el usuario: no se ejecutó nada".to_string();
            log(REJECTED, &connection, false, None, Some(e.clone()));
            return ("Rechazado por el usuario en DBine: no se ejecutó nada.".to_string(), true);
        }
        Outcome::TimedOut => {
            let e = format!("sin respuesta en {} s: rechazado, no se ejecutó nada", APPROVAL_TIMEOUT.as_secs());
            log(TIMEOUT, &connection, false, None, Some(e));
            return (
                format!("Sin respuesta del usuario en {} segundos: rechazado, no se ejecutó nada.", APPROVAL_TIMEOUT.as_secs()),
                true,
            );
        }
        Outcome::Approved => APPROVED,
        Outcome::AutoApproved => AUTO_APPROVED,
    };
    let (key, entry) = prepared.session;
    match run_on(inner, &key, entry, Op::Execute(prepared.code, SHOW_MAX as usize), prepared.limit).await {
        Ok(out) => {
            let affected: u64 = out.results.iter().filter_map(|r| r.rows_affected).sum();
            let returned: u64 = out.results.iter().map(|r| r.rows.len() as u64).sum();
            log(step, &connection, true, Some(affected + returned), None);
            let head = if step == AUTO_APPROVED {
                "Aprobado (el usuario aprobó todo lo de este cliente) y ejecutado."
            } else {
                "Aprobado por el usuario y ejecutado."
            };
            (format!("{head}\n\n{}", results_text(&out, SHOW_MAX).text), false)
        }
        Err(e) => {
            log(step, &connection, false, None, Some(e.clone()));
            (format!("Aprobado por el usuario, pero falló al ejecutarse: {e}"), true)
        }
    }
}

struct Prepared {
    request: ApprovalRequest,
    code: String,
    limit: Duration,
    session: (String, std::sync::Arc<crate::state::SessionEntry>),
}

/// Everything short of running: the level, the arguments and a session of
/// its own (so a missing password fails before the user is asked).
async fn prepare(inner: &Inner, client: &McpClient, args: &Value, connection: &mut String) -> Result<Prepared, String> {
    let default = load_config(&inner.state).default_level;
    let (conn, level) = find_connection(inner, arg(args, "connection")?, default)?;
    *connection = conn.name.clone();
    check_write(&conn, level, default)?;
    let db = arg(args, "database")?;
    let code = arg(args, "code")?.trim();
    if code.is_empty() {
        return Err("falta el código a ejecutar («code» está vacío)".into());
    }
    let driver = dbine_drivers::find(&conn.config.driver).ok_or_else(|| format!("esta versión no incluye el driver '{}'", conn.config.driver))?;
    let info = driver.info();
    let session = write_session(inner, &conn, db).await?;
    let now = chrono::Utc::now();
    let request = ApprovalRequest {
        id: uuid::Uuid::new_v4().to_string(),
        client_id: client.id.clone(),
        client: client.name.clone(),
        connection: conn.name.clone(),
        database: db.to_string(),
        engine: info.name.to_string(),
        language: serde_json::to_value(info.language).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "sql".into()),
        dialect: info.dialect.to_string(),
        code: code.to_string(),
        expires_at: (now + chrono::Duration::from_std(APPROVAL_TIMEOUT).unwrap_or_default()).to_rfc3339(),
        timeout_secs: APPROVAL_TIMEOUT.as_secs(),
    };
    Ok(Prepared {
        request,
        code: code.to_string(),
        limit: Duration::from_secs(arg_num(args, "timeout_seconds", TIMEOUT_DEFAULT, TIMEOUT_MAX)),
        session,
    })
}

/// `execute` needs the `write` level; say why a connection doesn't have it.
fn check_write(conn: &SavedConnection, level: McpLevel, default: McpLevel) -> Result<(), String> {
    if level >= McpLevel::Write {
        return Ok(());
    }
    let why = match effective_level(conn, default).cap {
        Some(Cap::Prod) => " Tiene la etiqueta «prod»: por MCP nunca pasa de lectura.",
        Some(Cap::ReadOnly) => " Es de solo lectura: por MCP nunca pasa de lectura.",
        None => " El usuario lo puede cambiar en DBine, en la conexión o en Configuración › MCP.",
    };
    Err(format!(
        "la conexión «{}» tiene el nivel «{}» por MCP y {TOOL} necesita «{}».{why}",
        conn.name,
        level.label(),
        McpLevel::Write.label()
    ))
}

/// A session of MCP's own for writes (not read-only), kept between calls
/// like the read one; editing the connection drops it.
async fn write_session(inner: &Inner, conn: &SavedConnection, db: &str) -> Result<(String, std::sync::Arc<crate::state::SessionEntry>), String> {
    let key = format!("mcp-write:{}:{db}", conn.id);
    if let Some(e) = inner.state.sessions.get(&key) {
        if e.connection_id == conn.id && e.database == db {
            return Ok((key, e.clone()));
        }
    }
    let entry = inner.state.dedicated_session(&key, &conn.id, db, false).await.map_err(|e| failure(conn, e))?;
    Ok((key, entry))
}
