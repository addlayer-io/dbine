//! The tools MCP clients call. Structure tools need a connection at level
//! `schema`; data tools (`sample_rows`, `run_query`, `explain`) need `read`
//! and always run on a read-only session of their own; `run_query` and
//! `explain` run without asking only where the server enforces the read
//! and the query calls only side-effect-free built-ins, and otherwise after
//! the user approves the query (`reads.rs`). `execute`
//! (writes) needs `write` and the user's approval of each call (`write.rs`).
//! Answers are compact text; no tool ever shows hosts, users or secrets.

use super::activity::ActivityEntry;
use super::approvals::{ApprovalKind, Outcome, APPROVAL_TIMEOUT};
use super::reads::{self, Attempt, How, Refused};
use super::{effective_level, load_config, Inner, McpClient, McpLevel};
use crate::commands::explorer::{META_LIMIT, SCHEMA_LIMIT};
use crate::error::CommandError;
use crate::state::SessionEntry;
use dbine_core::SavedConnection;
use dbine_driver::{kinds, Error, Language, ObjectRef, PlanNode, QueryOutcome, StatementResult};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub const INSTRUCTIONS: &str = "DBine exposes the user's saved database connections. Start with list_connections, \
then list_databases, list_objects and describe_object. Data tools (sample_rows, run_query, explain) only work on \
connections the user set to the 'read' level, and every query they run is read-only: statements that change data or \
structure are refused. run_query and explain run at once where the database server itself enforces the read as \
read-only and the query calls only built-in, side-effect-free functions; otherwise (other functions, or engines that \
can't enforce reads) the user approves each query in DBine first (it waits up to 2 minutes for the answer). \
To change data or structure use execute, on connections at the 'write' level: the user must \
approve each call in DBine (it waits up to 2 minutes for the answer). Pass database \"\" for engines without databases.";

const SAMPLE_MAX: u64 = 100;
const QUERY_MAX: u64 = 500;
const QUERY_DEFAULT: u64 = 100;
pub(super) const TIMEOUT_DEFAULT: u64 = 30;
pub(super) const TIMEOUT_MAX: u64 = 300;
/// A cell longer than this is cut.
const CELL_MAX: usize = 300;
/// An answer longer than this is cut.
const TEXT_MAX: usize = 200_000;
const LIST_MAX: usize = 5_000;

const NAMES: &[&str] = &["list_connections", "list_databases", "list_objects", "describe_object", "index_usage", "sample_rows", "run_query", "explain", super::write::TOOL];

/// What DBine's own assistant may call (a local model, on the tab's
/// connection): no list_connections (it stays on its connection) and no
/// execute (it never writes).
pub const ASSISTANT_TOOLS: &[&str] = &["list_databases", "list_objects", "describe_object", "index_usage", "sample_rows", "run_query", "explain"];

pub fn exists(name: &str) -> bool {
    NAMES.contains(&name)
}

pub fn definitions() -> Value {
    let conn = json!({ "type": "string", "description": "Connection name (or id) from list_connections." });
    let db = json!({ "type": "string", "description": "Database from list_databases (\"\" for engines without databases)." });
    let obj = json!({ "type": "string", "description": "Object name as list_objects shows it: schema.name or name." });
    let ro = json!({ "readOnlyHint": true, "openWorldHint": false });
    json!([
        {
            "name": "list_connections",
            "description": "The user's database connections available to MCP: name, engine and access level (schema = structure only; read = also read-only queries; write = also execute, with the user's approval of each call).",
            "inputSchema": { "type": "object", "properties": {} },
            "annotations": ro,
        },
        {
            "name": "list_databases",
            "description": "Databases (or keyspaces, catalogs…) of a connection.",
            "inputSchema": { "type": "object", "properties": { "connection": conn }, "required": ["connection"] },
            "annotations": ro,
        },
        {
            "name": "list_objects",
            "description": "Tables, views, collections and other objects of a database, one per line: kind and name.",
            "inputSchema": { "type": "object", "properties": { "connection": conn, "database": db }, "required": ["connection", "database"] },
            "annotations": ro,
        },
        {
            "name": "describe_object",
            "description": "Columns (type, nullability, default), primary key, foreign keys and indexes of a table or other object.",
            "inputSchema": { "type": "object", "properties": { "connection": conn, "database": db, "object": obj }, "required": ["connection", "database", "object"] },
            "annotations": ro,
        },
        {
            "name": "index_usage",
            "description": "A table's indexes and how they're used since the server's counters started: kind, key columns, size, reads (seeks, scans, lookups), writes, share of the table's reads, unused (written, never read) and disabled.",
            "inputSchema": { "type": "object", "properties": { "connection": conn, "database": db, "object": obj }, "required": ["connection", "database", "object"] },
            "annotations": ro,
        },
        {
            "name": "sample_rows",
            "description": "The first rows of a table or collection (needs the 'read' level).",
            "inputSchema": { "type": "object", "properties": {
                "connection": conn, "database": db, "object": obj,
                "limit": { "type": "integer", "minimum": 1, "maximum": SAMPLE_MAX, "default": 20 }
            }, "required": ["connection", "database", "object"] },
            "annotations": ro,
        },
        {
            "name": "run_query",
            "description": "Run a read-only query in the engine's own language (SQL, a Mongo command, Cypher…) and get the rows as text. Needs the 'read' level; writes are refused. Where the server can enforce a read-only transaction it runs at once (one statement per call); on other engines the user approves this exact query in DBine first (the call waits up to 2 minutes and is rejected without an answer).",
            "inputSchema": { "type": "object", "properties": {
                "connection": conn, "database": db,
                "query": { "type": "string" },
                "max_rows": { "type": "integer", "minimum": 1, "maximum": QUERY_MAX, "default": QUERY_DEFAULT },
                "timeout_seconds": { "type": "integer", "minimum": 1, "maximum": TIMEOUT_MAX, "default": TIMEOUT_DEFAULT }
            }, "required": ["connection", "database", "query"] },
            "annotations": ro,
        },
        {
            "name": "explain",
            "description": "The estimated execution plan of a query (nothing runs). Needs the 'read' level and an engine with plans. On engines where the server can't enforce reads as read-only, the user approves the query in DBine first (the call waits up to 2 minutes).",
            "inputSchema": { "type": "object", "properties": { "connection": conn, "database": db, "query": { "type": "string" } }, "required": ["connection", "database", "query"] },
            "annotations": ro,
        },
        super::write::definition(conn, db),
    ])
}

/// What a successful call returns.
pub(super) struct Done {
    pub text: String,
    pub rows: Option<u64>,
}

/// Run a tool and record it in the activity log. `(text, is_error)`.
pub async fn call(inner: &Inner, client: &McpClient, tool: &str, args: &Value) -> (String, bool) {
    crate::commands::telemetry::track_backend_feature(&inner.state, "mcp_tool");
    if tool == super::write::TOOL {
        return super::write::call(inner, client, args).await;
    }
    let mut connection = String::new();
    let mut phase = None;
    let result = run(inner, tool, args, &mut connection, &mut phase, None, Who::Mcp(client)).await;
    logged(inner, client.name.as_str(), tool, phase, args, connection, result)
}

/// DBine's own assistant with a local model: the same tools, on the tab's
/// connection only, at the level the user gave it in the chat (structure, or
/// also data with "datos"), whatever MCP's settings say. Never writes.
pub async fn assistant_call(inner: &Inner, conn: &SavedConnection, tool: &str, args: &Value, allow_data: bool) -> (String, bool) {
    if !ASSISTANT_TOOLS.contains(&tool) {
        return (format!("herramienta desconocida: {tool}"), true);
    }
    let level = if allow_data { McpLevel::Read } else { McpLevel::Schema };
    let mut connection = String::new();
    let mut phase = None;
    let result = run(inner, tool, args, &mut connection, &mut phase, Some((conn.clone(), level)), Who::Chat).await;
    logged(inner, "Asistente de DBine", tool, phase, args, connection, result)
}

/// Who approves a read the engine can't enforce on the server.
#[derive(Clone, Copy)]
pub(super) enum Who<'a> {
    /// An MCP client: DBine's approval dialog, as for `execute`.
    Mcp(&'a McpClient),
    /// DBine's assistant: the chat already showed the exact query and the
    /// user approved it (or approved the conversation's reads), before
    /// `assistant_call`.
    Chat,
}

/// The exact query a read of rows will run, for the user to approve first:
/// the engine's browse query for sample_rows, the model's own for
/// run_query and explain. An error (no such object) goes to the model.
pub async fn assistant_preview(inner: &Inner, conn: &SavedConnection, tool: &str, args: &Value) -> Result<String, String> {
    let db = args.get("database").and_then(Value::as_str).unwrap_or("");
    match tool {
        "sample_rows" => {
            let obj = find_object(inner, conn, db, arg(args, "object")?).await?;
            let limit = arg_num(args, "limit", 20, SAMPLE_MAX);
            let entry = inner.state.session(&crate::state::meta_key(&conn.id, db), &conn.id, db).await.map_err(|e| failure(conn, e))?;
            let query = entry.session.lock().await.browse_query(&obj, limit as u32);
            Ok(query)
        }
        "run_query" | "explain" => Ok(arg(args, "query")?.to_string()),
        other => Err(format!("herramienta desconocida: {other}")),
    }
}

/// The log entry of a call; `phase` names how a read ran (`run_query:enforced`,
/// `run_query:approved`…) or why it didn't (`run_query:rejected`).
fn logged(inner: &Inner, client: &str, tool: &str, phase: Option<&str>, args: &Value, connection: String, result: Result<Done, String>) -> (String, bool) {
    let (ok, rows, error) = match &result {
        Ok(d) => (true, d.rows, None),
        Err(e) => (false, None, Some(e.clone())),
    };
    inner.activity.record(&ActivityEntry {
        id: 0,
        at: chrono::Utc::now().to_rfc3339(),
        client: client.to_string(),
        connection,
        tool: match phase {
            Some(p) => format!("{tool}:{p}"),
            None => tool.to_string(),
        },
        summary: summary(tool, args),
        ok,
        rows,
        error,
    });
    match result {
        Ok(d) => (d.text, false),
        Err(e) => (e, true),
    }
}

/// The call's arguments for the log: names, and the query cut short.
pub(super) fn summary(tool: &str, args: &Value) -> String {
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    let where_ = [s("database"), s("object")].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(".");
    match tool {
        "run_query" | "explain" | super::write::TOOL => {
            let q = s(if tool == super::write::TOOL { "code" } else { "query" }).split_whitespace().collect::<Vec<_>>().join(" ");
            let q = cut(&q, 200);
            if where_.is_empty() { q } else { format!("[{where_}] {q}") }
        }
        _ => where_,
    }
}

pub(super) fn cut(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

pub(super) fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key).and_then(Value::as_str).ok_or_else(|| format!("falta el argumento «{key}»"))
}

pub(super) fn arg_num(args: &Value, key: &str, default: u64, max: u64) -> u64 {
    args.get(key).and_then(Value::as_u64).unwrap_or(default).clamp(1, max)
}

async fn run(
    inner: &Inner,
    tool: &str,
    args: &Value,
    connection: &mut String,
    phase: &mut Option<&'static str>,
    on: Option<(SavedConnection, McpLevel)>,
    who: Who<'_>,
) -> Result<Done, String> {
    let default = load_config(&inner.state).default_level;
    if tool == "list_connections" {
        return list_connections(inner, default);
    }
    let (conn, level) = match on {
        Some(c) => c,
        None => find_connection(inner, arg(args, "connection")?, default)?,
    };
    *connection = conn.name.clone();
    let needs = match tool {
        "sample_rows" | "run_query" | "explain" => McpLevel::Read,
        _ => McpLevel::Schema,
    };
    if level < needs {
        return Err(format!(
            "la conexión «{}» tiene el nivel «{}» por MCP y {tool} necesita «{}». El usuario lo puede cambiar en DBine, en la conexión o en Configuración › MCP.",
            conn.name,
            level.label(),
            needs.label()
        ));
    }
    let db = if tool == "list_databases" { "" } else { arg(args, "database")? };
    match tool {
        "list_databases" => {
            let dbs = inner.state.meta_read(&conn.id, "", META_LIMIT, |s| Box::pin(s.list_databases())).await.map_err(|e| failure(&conn, e))?;
            let n = dbs.len() as u64;
            let text = if dbs.is_empty() { "(this engine has no databases: use database \"\")".to_string() } else { dbs.join("\n") };
            Ok(Done { text, rows: Some(n) })
        }
        "list_objects" => {
            let objects = inner.state.meta_read(&conn.id, db, META_LIMIT, |s| Box::pin(s.list_objects())).await.map_err(|e| failure(&conn, e))?;
            let mut lines: Vec<String> = objects
                .iter()
                .take(LIST_MAX)
                .map(|o| {
                    let name = qualified(o.schema.as_deref(), &o.name);
                    match &o.parent {
                        Some(p) => format!("{} {name} (on {p})", o.kind),
                        None => format!("{} {name}", o.kind),
                    }
                })
                .collect();
            if objects.len() > LIST_MAX {
                lines.push(format!("… {} more not shown", objects.len() - LIST_MAX));
            }
            if lines.is_empty() {
                lines.push("(no objects)".into());
            }
            Ok(Done { text: lines.join("\n"), rows: Some(objects.len() as u64) })
        }
        "describe_object" => describe(inner, &conn, db, arg(args, "object")?).await,
        "index_usage" => match args.get("object").and_then(Value::as_str).filter(|o| !o.trim().is_empty()) {
            Some(o) => index_usage(inner, &conn, db, o).await,
            None => index_overview(inner, &conn, db).await,
        },
        "sample_rows" => {
            let obj = find_object(inner, &conn, db, arg(args, "object")?).await?;
            let limit = arg_num(args, "limit", 20, SAMPLE_MAX);
            let entry = inner.state.session(&crate::state::meta_key(&conn.id, db), &conn.id, db).await.map_err(|e| failure(&conn, e))?;
            let query = entry.session.lock().await.browse_query(&obj, limit as u32);
            let out = run_read_only(inner, &conn, db, Op::Execute(query, limit as usize), Duration::from_secs(TIMEOUT_DEFAULT)).await?;
            Ok(results_text(&out, limit))
        }
        "run_query" => {
            let query = arg(args, "query")?;
            refuse_writes(&conn, query)?;
            let max = arg_num(args, "max_rows", QUERY_DEFAULT, QUERY_MAX);
            let limit = Duration::from_secs(arg_num(args, "timeout_seconds", TIMEOUT_DEFAULT, TIMEOUT_MAX));
            let (key, entry) = read_only_session(inner, &conn, db).await?;
            let routed = reads::route(
                approval_free(&conn, query),
                async {
                    match run_op(inner, &key, entry.clone(), Op::ReadOnly(query.to_string(), max as usize), limit).await {
                        Ok(out) => Ok(Attempt::Done(out)),
                        Err(RunFail::Unsupported) => Ok(Attempt::NotEnforced),
                        Err(RunFail::Failed(e)) => Err(e),
                    }
                },
                approve_read(inner, who, tool, args, &conn, db, query),
                run_read_only(inner, &conn, db, Op::Execute(query.to_string(), max as usize), limit),
            )
            .await;
            let (out, how) = routed_phase(routed, phase)?;
            Ok(approved_head(how, results_text(&out, max)))
        }
        "explain" => {
            let query = arg(args, "query")?;
            let driver = dbine_drivers::find(&conn.config.driver).ok_or_else(|| format!("esta versión no incluye el driver '{}'", conn.config.driver))?;
            if !driver.supports_explain() {
                return Err(format!("{} no ofrece planes de ejecución", driver.info().name));
            }
            refuse_writes(&conn, query)?;
            let limit = Duration::from_secs(TIMEOUT_DEFAULT);
            let (key, entry) = read_only_session(inner, &conn, db).await?;
            // An estimated plan runs nothing where the engine enforces reads
            // (asked with a trivial read, `Op::Probe`: only `Unsupported`
            // says it can't). Elsewhere (SQL Server runs the batch under SHOWPLAN)
            // the guard alone isn't a boundary: the user approves it first.
            // A function outside the allowlist asks too: the planner may
            // run one (a user's function declared IMMUTABLE).
            let routed = reads::route(
                approval_free(&conn, query),
                async {
                    match run_op(inner, &key, entry.clone(), Op::Probe, limit).await {
                        Err(RunFail::Unsupported) => Ok(Attempt::NotEnforced),
                        _ => run_on(inner, &key, entry.clone(), Op::Explain(query.to_string()), limit).await.map(Attempt::Done),
                    }
                },
                approve_read(inner, who, tool, args, &conn, db, query),
                run_read_only(inner, &conn, db, Op::Explain(query.to_string()), limit),
            )
            .await;
            let (out, how) = routed_phase(routed, phase)?;
            Ok(approved_head(how, plans_text(&out)))
        }
        _ => Err(format!("herramienta desconocida: {tool}")),
    }
}

fn list_connections(inner: &Inner, default: McpLevel) -> Result<Done, String> {
    let conns = inner.state.store.list_connections().map_err(|e| e.to_string())?;
    let mut lines = Vec::new();
    for c in &conns {
        let level = effective_level(c, default).level;
        if level < McpLevel::Schema {
            continue;
        }
        let engine = dbine_drivers::find(&c.config.driver).map(|d| d.info().name).unwrap_or(c.config.driver.as_str());
        lines.push(format!("{} | {} | {} | id {}", c.name, engine, level.as_str(), c.id));
    }
    let n = lines.len() as u64;
    let text = if lines.is_empty() {
        "(no connections are available to MCP: the user can enable them in DBine, Settings › MCP)".to_string()
    } else {
        format!("name | engine | level | id\n{}", lines.join("\n"))
    };
    Ok(Done { text, rows: Some(n) })
}

/// The connection a client named, with its effective level. A connection
/// that doesn't exist and one MCP can't see answer the same.
pub(super) fn find_connection(inner: &Inner, wanted: &str, default: McpLevel) -> Result<(SavedConnection, McpLevel), String> {
    let conns = inner.state.store.list_connections().map_err(|e| e.to_string())?;
    let visible: Vec<(SavedConnection, McpLevel)> = conns
        .into_iter()
        .map(|c| {
            let l = effective_level(&c, default).level;
            (c, l)
        })
        .filter(|(_, l)| *l >= McpLevel::Schema)
        .collect();
    let wanted = wanted.trim();
    if let Some(hit) = visible.iter().find(|(c, _)| c.id == wanted) {
        return Ok(hit.clone());
    }
    let named: Vec<_> = visible.into_iter().filter(|(c, _)| c.name.trim().eq_ignore_ascii_case(wanted)).collect();
    match named.len() {
        1 => Ok(named.into_iter().next().unwrap()),
        0 => Err(format!("no hay ninguna conexión «{wanted}» disponible por MCP (list_connections muestra las que hay)")),
        _ => Err(format!("hay varias conexiones llamadas «{wanted}»: indicá su id (list_connections)")),
    }
}

/// A command error as the client sees it, without asking for things MCP
/// can't give (a password prompt, trusting an SSH host).
pub(super) fn failure(conn: &SavedConnection, e: CommandError) -> String {
    match e {
        CommandError::PasswordRequired(_) => format!(
            "la conexión «{}» necesita la contraseña: el usuario tiene que conectarse una vez desde DBine en esta sesión",
            conn.name
        ),
        CommandError::SshUnknownHost { .. } => format!(
            "el servidor SSH del túnel de «{}» no está verificado: el usuario tiene que conectarse una vez desde DBine",
            conn.name
        ),
        e => e.to_string(),
    }
}

fn qualified(schema: Option<&str>, name: &str) -> String {
    match schema.filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{name}"),
        None => name.to_string(),
    }
}

/// The object `wanted` names (`schema.name` or `name`, any case).
async fn find_object(inner: &Inner, conn: &SavedConnection, db: &str, wanted: &str) -> Result<ObjectRef, String> {
    let objects = inner.state.meta_read(&conn.id, db, META_LIMIT, |s| Box::pin(s.list_objects())).await.map_err(|e| failure(conn, e))?;
    let wanted = wanted.trim();
    let matches = |o: &&dbine_driver::DbObject| {
        qualified(o.schema.as_deref(), &o.name).eq_ignore_ascii_case(wanted) || o.name.eq_ignore_ascii_case(wanted)
    };
    // An exact qualified name beats a bare name found in several schemas.
    let exact: Vec<_> = objects.iter().filter(|o| qualified(o.schema.as_deref(), &o.name) == wanted).collect();
    let hits: Vec<_> = if exact.len() == 1 { exact } else { objects.iter().filter(matches).collect() };
    let hits: Vec<_> = if hits.len() > 1 && hits.iter().any(|o| o.parent.is_none()) { hits.into_iter().filter(|o| o.parent.is_none()).collect() } else { hits };
    match hits.as_slice() {
        [o] => Ok(ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() }),
        [] => Err(format!("no existe el objeto «{wanted}» en «{db}» (list_objects muestra los que hay)")),
        many => Err(format!(
            "«{wanted}» coincide con varios objetos: {}. Indicá el esquema.",
            many.iter().take(10).map(|o| qualified(o.schema.as_deref(), &o.name)).collect::<Vec<_>>().join(", ")
        )),
    }
}

/// Tables an overview reads at most, and for how long.
const OVERVIEW_TABLES: usize = 400;
const OVERVIEW_TIME: Duration = Duration::from_secs(120);

/// index_usage without an object: every table of the database, summed up in
/// what an analysis needs (unused, disabled, never read), not every index.
async fn index_overview(inner: &Inner, conn: &SavedConnection, db: &str) -> Result<Done, String> {
    let objects = inner.state.meta_read(&conn.id, db, META_LIMIT, |s| Box::pin(s.list_objects())).await.map_err(|e| failure(conn, e))?;
    let tables: Vec<ObjectRef> = objects
        .iter()
        .filter(|o| o.kind == kinds::TABLE || o.kind == kinds::COLLECTION)
        .map(|o| ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() })
        .collect();
    let start = std::time::Instant::now();
    let (mut read, mut total, mut since, mut no_stats) = (0usize, 0usize, None, false);
    let (mut unused, mut disabled, mut idle) = (Vec::new(), Vec::new(), Vec::new());
    for t in tables.iter().take(OVERVIEW_TABLES) {
        if start.elapsed() > OVERVIEW_TIME {
            break;
        }
        let o = t.clone();
        let Ok(Some(r)) = inner.state.meta_read(&conn.id, db, META_LIMIT, move |s| Box::pin(async move { s.index_usage(&o).await })).await else {
            continue;
        };
        let r = r.derived();
        read += 1;
        since = since.or(r.since.clone());
        no_stats |= !r.stats_available;
        let table = qualified(t.schema.as_deref(), &t.name);
        for i in &r.indexes {
            total += 1;
            let line = format!("  {table}.{} ({}; {}){}", i.name, i.kind, i.key_columns.join(", "), i.size_kb.map(|k| format!(" {k} KB")).unwrap_or_default());
            if i.disabled {
                disabled.push(line);
            } else if i.unused {
                unused.push(format!("{line} writes {}", i.updates));
            } else if r.stats_available && i.reads == 0 && !i.primary_key {
                idle.push(line);
            }
        }
    }
    let mut out = vec![format!(
        "{read} of {} tables read in {db}{}, {total} indexes{}",
        tables.len(),
        if read < tables.len() { " (stopped early: too many tables or too slow; ask index_usage per table for the rest)" } else { "" },
        since.map(|s| format!("; counters since {s}")).unwrap_or_default()
    )];
    if no_stats {
        out.push("usage counters not available for some tables (permissions or engine): judge only by structure there".into());
    }
    let mut section = |title: &str, lines: Vec<String>| {
        out.push(format!("{title}: {}", lines.len()));
        out.extend(lines.into_iter().take(200));
    };
    section("UNUSED (written, never read: they cost on every write)", unused);
    section("DISABLED", disabled);
    section("never read nor written (no evidence either way)", idle);
    Ok(Done { rows: Some(total as u64), text: out.join("\n") })
}

async fn index_usage(inner: &Inner, conn: &SavedConnection, db: &str, wanted: &str) -> Result<Done, String> {
    let obj = find_object(inner, conn, db, wanted).await?;
    let o = obj.clone();
    let report = inner.state.meta_read(&conn.id, db, META_LIMIT, move |s| Box::pin(async move { s.index_usage(&o).await })).await.map_err(|e| failure(conn, e))?;
    let Some(report) = report.map(dbine_driver::IndexUsageReport::derived) else {
        return Err(format!("{} no informa el uso de índices", conn.config.driver));
    };
    let name = qualified(obj.schema.as_deref(), &obj.name);
    let mut out = vec![format!("indexes of {name}{}", report.since.as_deref().map(|s| format!(" (counters since {s})")).unwrap_or_default())];
    if !report.stats_available {
        out.push(format!("usage counters not available{}", report.note.as_deref().map(|n| format!(": {n}")).unwrap_or_default()));
    }
    for i in &report.indexes {
        let mut flags = Vec::new();
        if i.primary_key { flags.push("PK"); }
        if i.unique { flags.push("UNIQUE"); }
        if i.disabled { flags.push("DISABLED"); }
        if i.unused { flags.push("UNUSED (written, never read)"); }
        let share = i.read_share.map(|r| format!(" share {:.0}%", r * 100.0)).unwrap_or_default();
        let writes = if report.writes_counted { format!(" writes {}", i.updates) } else { String::new() };
        let size = i.size_kb.map(|k| format!(" {k} KB")).unwrap_or_default();
        out.push(format!(
            "  {} {} ({}){}{} reads {} (seeks {}, scans {}, lookups {}){writes}{share}{}",
            i.name,
            i.kind,
            i.key_columns.join(", "),
            if i.included_columns.is_empty() { String::new() } else { format!(" include ({})", i.included_columns.join(", ")) },
            size,
            i.reads,
            i.seeks,
            i.scans,
            i.lookups,
            if flags.is_empty() { String::new() } else { format!(" [{}]", flags.join(", ")) },
        ));
    }
    if report.indexes.is_empty() {
        out.push("  (no indexes)".into());
    }
    Ok(Done { rows: Some(report.indexes.len() as u64), text: out.join("\n") })
}

async fn describe(inner: &Inner, conn: &SavedConnection, db: &str, wanted: &str) -> Result<Done, String> {
    let obj = find_object(inner, conn, db, wanted).await?;
    let o = obj.clone();
    let columns = inner.state.meta_read(&conn.id, db, META_LIMIT, move |s| Box::pin(async move { s.columns(&o).await })).await.map_err(|e| failure(conn, e))?;
    let name = qualified(obj.schema.as_deref(), &obj.name);
    let mut out = vec![format!("{} {name}", obj.kind), "columns:".to_string()];
    for c in &columns {
        let mut line = format!("  {} {}", c.name, if c.data_type.is_empty() { "?" } else { &c.data_type });
        line.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
        if c.primary_key {
            line.push_str(" PK");
        }
        if c.auto_increment {
            line.push_str(" auto-increment");
        }
        if let Some(d) = &c.default_value {
            line.push_str(&format!(" default {}", cut(d, 80)));
        }
        out.push(line);
    }
    if columns.is_empty() {
        out.push("  (no columns reported)".into());
    }
    // Keys and indexes come with the whole database's structure.
    if obj.kind == kinds::TABLE || obj.kind == kinds::COLLECTION {
        match inner.state.meta_read(&conn.id, db, SCHEMA_LIMIT, |s| Box::pin(s.database_schema())).await {
            Ok(tables) => {
                if let Some(t) = tables.iter().find(|t| t.name == obj.name && t.schema.as_deref().unwrap_or("") == obj.schema.as_deref().unwrap_or("")) {
                    if let Some(pk) = &t.primary_key {
                        out.push(format!("primary key: ({})", pk.columns.join(", ")));
                    }
                    if !t.foreign_keys.is_empty() {
                        out.push("foreign keys:".into());
                        for fk in &t.foreign_keys {
                            let mut line = format!(
                                "  {}({}) -> {}({})",
                                fk.name.as_deref().map(|n| format!("{n} ")).unwrap_or_default(),
                                fk.columns.join(", "),
                                qualified(fk.ref_schema.as_deref(), &fk.ref_table),
                                fk.ref_columns.join(", ")
                            );
                            if let Some(d) = &fk.on_delete {
                                line.push_str(&format!(" ON DELETE {d}"));
                            }
                            if let Some(u) = &fk.on_update {
                                line.push_str(&format!(" ON UPDATE {u}"));
                            }
                            out.push(line);
                        }
                    }
                    if !t.indexes.is_empty() {
                        out.push("indexes:".into());
                        for i in &t.indexes {
                            out.push(format!(
                                "  {}{} ({}){}",
                                i.name,
                                if i.unique { " UNIQUE" } else { "" },
                                i.columns.join(", "),
                                i.kind.as_deref().map(|k| format!(" {k}")).unwrap_or_default()
                            ));
                        }
                    }
                    if let Some(c) = t.comment.as_deref().filter(|c| !c.is_empty()) {
                        out.push(format!("comment: {}", cut(c, 500)));
                    }
                }
            }
            Err(e) => out.push(format!("(keys and indexes unavailable: {})", failure(conn, e))),
        }
    }
    Ok(Done { text: out.join("\n"), rows: Some(columns.len() as u64) })
}

/// For SQL engines, refuse a write before it reaches the server (the
/// read-only session would refuse it too; this says why up front). Other
/// engines refuse writes in their read-only mode.
pub(super) fn refuse_writes_in(language: Language, query: &str) -> Result<(), String> {
    if language == Language::Sql {
        if let Some(kw) = dbine_driver::read_only::first_write(query) {
            return Err(format!("DBine solo permite lecturas por MCP: la consulta tiene una sentencia {kw}"));
        }
    }
    Ok(())
}

/// Whether `query` may skip the approval where the engine enforces reads:
/// it calls only side-effect-free built-ins (`reads::approval_free`).
fn approval_free(conn: &SavedConnection, query: &str) -> bool {
    dbine_drivers::find(&conn.config.driver).is_some_and(|d| {
        let info = d.info();
        reads::approval_free(info.language, info.dialect, &d.script_dialect(), query).is_ok()
    })
}

fn refuse_writes(conn: &SavedConnection, query: &str) -> Result<(), String> {
    let driver = dbine_drivers::find(&conn.config.driver).ok_or_else(|| format!("esta versión no incluye el driver '{}'", conn.config.driver))?;
    refuse_writes_in(driver.info().language, query)
}

pub(super) enum Op {
    Execute(String, usize),
    Explain(String),
    /// `Session::run_read_only`: one statement as a read the server enforces.
    ReadOnly(String, usize),
    /// `run_read_only` of `SELECT 1`: whether the engine enforces reads at
    /// all. Only `Unsupported` says no (an engine whose language has no
    /// `SELECT 1` answers `Unsupported` before reading the statement, or an
    /// error that still means it enforces reads).
    Probe,
}

/// Why `run_op` failed.
pub(super) enum RunFail {
    /// `run_read_only` isn't there: the engine (or its driver host) can't
    /// enforce the read on the server.
    Unsupported,
    Failed(String),
}

/// The routed read's log step, and its error as the client sees it.
fn routed_phase(routed: Result<(QueryOutcome, How), Refused>, phase: &mut Option<&'static str>) -> Result<(QueryOutcome, How), String> {
    match routed {
        Ok((out, how)) => {
            *phase = Some(how.phase());
            Ok((out, how))
        }
        Err(r) => {
            *phase = Some(r.phase);
            Err(r.message)
        }
    }
}

/// An MCP client is told the user approved the read in DBine.
fn approved_head(how: How, mut done: Done) -> Done {
    if how == How::Approved {
        done.text = format!("{}\n\n{}", "Aprobado por el usuario y ejecutado.", done.text);
    }
    done
}

/// The user's approval of a read the engine can't enforce: DBine's dialog
/// for an MCP client (the log gets the request first, as for `execute`);
/// DBine's assistant was already approved in the chat.
pub(super) async fn approve_read(inner: &Inner, who: Who<'_>, tool: &str, args: &Value, conn: &SavedConnection, db: &str, code: &str) -> Result<How, Refused> {
    let client = match who {
        Who::Chat => return Ok(How::ApprovedInChat),
        Who::Mcp(client) => client,
    };
    let request = super::write::approval_request(ApprovalKind::Read, client, conn, db, code).map_err(|message| Refused { message, phase: "request" })?;
    inner.activity.record(&ActivityEntry {
        id: 0,
        at: chrono::Utc::now().to_rfc3339(),
        client: client.name.clone(),
        connection: conn.name.clone(),
        tool: format!("{tool}:request"),
        summary: summary(tool, args),
        ok: true,
        rows: None,
        error: None,
    });
    match inner.approvals.ask(request, APPROVAL_TIMEOUT).await {
        Outcome::Approved => Ok(How::Approved),
        Outcome::AutoApproved => Ok(How::AutoApproved),
        Outcome::Rejected => Err(Refused { message: "Rechazado por el usuario en DBine: no se ejecutó nada.".into(), phase: "rejected" }),
        Outcome::TimedOut => Err(Refused {
            message: format!("Sin respuesta del usuario en {} segundos: rechazado, no se ejecutó nada.", APPROVAL_TIMEOUT.as_secs()),
            phase: "timeout",
        }),
    }
}

/// A read-only session of MCP's own for the database: `cfg.read_only` is
/// forced, so SQL engines get the read-only decorator and the others their
/// own read-only mode. Kept between calls; editing the connection drops it.
async fn read_only_session(inner: &Inner, conn: &SavedConnection, db: &str) -> Result<(String, Arc<SessionEntry>), String> {
    let key = format!("mcp:{}:{db}", conn.id);
    if let Some(e) = inner.state.sessions.get(&key) {
        if e.connection_id == conn.id && e.database == db {
            return Ok((key, e.clone()));
        }
    }
    let entry = inner.state.dedicated_session(&key, &conn.id, db, true).await.map_err(|e| failure(conn, e))?;
    Ok((key, entry))
}

async fn run_read_only(inner: &Inner, conn: &SavedConnection, db: &str, op: Op, limit: Duration) -> Result<QueryOutcome, String> {
    let (key, entry) = read_only_session(inner, conn, db).await?;
    run_on(inner, &key, entry, op, limit).await
}

/// Run `op` on the session `key` holds, within `limit`; on timeout the
/// statement is interrupted and the session dropped.
pub(super) async fn run_on(inner: &Inner, key: &str, entry: Arc<SessionEntry>, op: Op, limit: Duration) -> Result<QueryOutcome, String> {
    run_op(inner, key, entry, op, limit).await.map_err(|e| match e {
        RunFail::Unsupported => Error::Unsupported("este motor no asegura lecturas de solo lectura en el servidor".into()).to_string(),
        RunFail::Failed(e) => e,
    })
}

/// `run_on`, telling `Unsupported` of `run_read_only` apart.
pub(super) async fn run_op(inner: &Inner, key: &str, entry: Arc<SessionEntry>, op: Op, limit: Duration) -> Result<QueryOutcome, RunFail> {
    let mut out = QueryOutcome::default();
    let work = async {
        let mut s = entry.session.lock().await;
        match &op {
            Op::Execute(q, max) => s.execute(q, *max, &mut out).await,
            Op::Explain(q) => s.explain(q, false, 100, &mut out).await,
            Op::ReadOnly(q, max) => s.run_read_only(q, *max, &mut out).await,
            Op::Probe => s.run_read_only("SELECT 1", 1, &mut out).await,
        }
    };
    let enforced_call = matches!(op, Op::ReadOnly(..) | Op::Probe);
    match tokio::time::timeout(limit, work).await {
        Ok(Ok(())) => {}
        Ok(Err(Error::Unsupported(_))) if enforced_call => return Err(RunFail::Unsupported),
        Ok(Err(e)) => return Err(RunFail::Failed(e.to_string())),
        Err(_) => {
            if let Some(i) = &entry.interrupter {
                i();
            }
            entry.cancel.notify_waiters();
            inner.state.sessions.remove_if(key, |_, e| Arc::ptr_eq(e, &entry));
            return Err(RunFail::Failed(format!("la consulta superó el límite de {} s y se canceló", limit.as_secs())));
        }
    }
    if let Some(e) = out.error.take() {
        return Err(RunFail::Failed(e));
    }
    Ok(out)
}

fn cell(v: &Value) -> String {
    let s = match v {
        Value::Null => "NULL".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    cut(&s.replace('\r', "").replace('\n', "\\n").replace('\t', " "), CELL_MAX)
}

fn table_text(r: &StatementResult, shown_max: u64) -> String {
    let mut lines = vec![r.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join(" | ")];
    for row in &r.rows {
        lines.push(row.iter().map(cell).collect::<Vec<_>>().join(" | "));
    }
    let total = r.total_rows.max(r.rows.len() as u64);
    if r.truncated || total > r.rows.len() as u64 {
        lines.push(format!("({} of {total} rows shown; limit {shown_max})", r.rows.len()));
    } else {
        lines.push(format!("({} rows)", r.rows.len()));
    }
    lines.join("\n")
}

pub(super) fn results_text(out: &QueryOutcome, max: u64) -> Done {
    let mut parts = Vec::new();
    let mut rows = 0u64;
    for r in &out.results {
        if !r.columns.is_empty() {
            rows += r.rows.len() as u64;
            parts.push(table_text(r, max));
        } else if let Some(n) = r.rows_affected {
            parts.push(format!("({n} rows affected)"));
        }
    }
    if !out.messages.is_empty() {
        parts.push(format!("messages:\n{}", out.messages.iter().map(|m| cut(m, 500)).collect::<Vec<_>>().join("\n")));
    }
    if parts.is_empty() {
        parts.push("(no results)".into());
    }
    Done { text: cap_text(parts.join("\n\n")), rows: Some(rows) }
}

fn cap_text(s: String) -> String {
    if s.len() <= TEXT_MAX {
        return s;
    }
    let mut end = TEXT_MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (answer cut at {TEXT_MAX} characters)", &s[..end])
}

fn plan_lines(n: &PlanNode, depth: usize, out: &mut Vec<String>) {
    if out.len() > 2_000 {
        return;
    }
    let mut line = format!("{}{}", "  ".repeat(depth), n.op);
    if !n.detail.is_empty() {
        line.push_str(&format!(" [{}]", n.detail));
    }
    if let Some(o) = &n.object {
        line.push_str(&format!(" on {o}"));
    }
    let mut figures = Vec::new();
    if let Some(c) = n.total_cost {
        figures.push(format!("cost={c}"));
    }
    if let Some(r) = n.est_rows {
        figures.push(format!("rows={r}"));
    }
    if !figures.is_empty() {
        line.push_str(&format!(" ({})", figures.join(", ")));
    }
    for w in &n.warnings {
        line.push_str(&format!(" !{w}"));
    }
    out.push(line);
    for c in &n.children {
        plan_lines(c, depth + 1, out);
    }
}

fn plans_text(out: &QueryOutcome) -> Done {
    let mut parts = Vec::new();
    for p in &out.plans {
        let mut lines = vec![format!("plan for: {}", cut(&p.statement, 500))];
        if p.root.op.is_empty() {
            lines.push(cut(&p.raw, 20_000));
        } else {
            plan_lines(&p.root, 1, &mut lines);
        }
        parts.push(lines.join("\n"));
    }
    if parts.is_empty() {
        // Engines that give the plan as a result set.
        return results_text(out, 100);
    }
    Done { text: cap_text(parts.join("\n\n")), rows: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_run_query_refuses_writes() {
        assert!(refuse_writes_in(Language::Sql, "select 1").is_ok());
        assert!(refuse_writes_in(Language::Sql, "-- note\nWITH x AS (SELECT 1) SELECT * FROM x").is_ok());
        let e = refuse_writes_in(Language::Sql, "select 1; delete from t").unwrap_err();
        assert!(e.contains("DELETE"), "{e}");
        assert!(refuse_writes_in(Language::Sql, "DROP TABLE t").is_err());
        assert!(refuse_writes_in(Language::Sql, "update t set a = 1").is_err());
    }

    #[test]
    fn mcp_summary_cuts_queries() {
        let long = "select ".to_string() + &"x, ".repeat(200);
        let s = summary("run_query", &json!({ "connection": "c", "database": "main", "query": long }));
        assert!(s.starts_with("[main] select"));
        assert!(s.chars().count() < 220);
    }
}
