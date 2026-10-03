//! The process list ([`dbine_driver::Session::processes`]), stopping a
//! query ([`dbine_driver::Session::cancel_query`]) and ending the session
//! that runs it ([`dbine_driver::Session::kill_session`] with a query id).
//!
//! Snowflake lists queries, not connections: the list is the queries not yet
//! finished in `INFORMATION_SCHEMA.QUERY_HISTORY` (what the monitor reads),
//! each with its session. The id is the query id; `SYSTEM$CANCEL_QUERY`
//! cancels it and `SYSTEM$ABORT_SESSION` ends its session. The table
//! function needs a running warehouse: as in the monitor, a suspended one
//! is never resumed.

use crate::monitor::{quote, Runner, Set, TAG};
use dbine_driver::{Error, Result, ServerProcess};

/// Characters kept of a statement's text.
const MAX_TEXT: usize = 20000;
/// Rows at most.
const MAX_ROWS: usize = 2000;

/// A query id as Snowflake prints it (`01b2c3d4-0000-1234-0000-…`).
pub(crate) fn query_id(id: &str) -> Option<&str> {
    let id = id.trim();
    (!id.is_empty() && id.len() <= 64 && id.contains('-') && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')).then_some(id)
}

fn get<'a>(s: &'a Set, row: usize, name: &str) -> Option<&'a str> {
    let i = s.cols.iter().position(|c| c.eq_ignore_ascii_case(name))?;
    s.rows.get(row)?.get(i)?.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

fn num(s: &Set, row: usize, name: &str) -> Option<u64> {
    get(s, row, name).and_then(|v| v.parse::<f64>().ok()).map(|v| v.max(0.0) as u64)
}

/// The rows of the listing query.
fn rows(s: &Set) -> Vec<ServerProcess> {
    (0..s.rows.len())
        .map(|i| {
            let status = get(s, i, "execution_status").map(str::to_string);
            let wait = match status.as_deref() {
                Some("QUEUED") => Some("en cola del warehouse"),
                Some("RESUMING_WAREHOUSE") => Some("reanudando el warehouse"),
                Some("BLOCKED") => Some("bloqueada por otra transacción"),
                _ => None,
            };
            let db = get(s, i, "database_name").map(|d| match get(s, i, "schema_name") {
                Some(sc) => format!("{d}.{sc}"),
                None => d.to_string(),
            });
            ServerProcess {
                id: get(s, i, "query_id").unwrap_or_default().to_string(),
                // Running, queued or blocked: every listed query is in flight.
                active: true,
                own: matches!(get(s, i, "own"), Some("true" | "TRUE" | "1")),
                user: get(s, i, "user_name").map(str::to_string),
                // The warehouse that runs it, where a server would go.
                host: get(s, i, "warehouse_name").map(str::to_string),
                program: get(s, i, "session_id").map(|n| format!("sesión {n}")),
                database: db,
                command: get(s, i, "query_type").map(str::to_string),
                elapsed_ms: num(s, i, "elapsed"),
                reads: num(s, i, "bytes_scanned"),
                wait: wait.map(str::to_string),
                sql: get(s, i, "query_text").map(|t| t.chars().take(MAX_TEXT).collect()),
                status,
                ..Default::default()
            }
        })
        .filter(|p| !p.id.is_empty())
        .collect()
}

/// `<db>.INFORMATION_SCHEMA` of the session's database, as the monitor.
fn info_schema(r: &(dyn Runner + Sync)) -> String {
    match r.database() {
        Some(db) => format!("{}.INFORMATION_SCHEMA", quote(&db)),
        None => "SNOWFLAKE.INFORMATION_SCHEMA".to_string(),
    }
}

/// Refuses when the session's warehouse isn't running (it isn't resumed).
async fn warehouse_running(r: &(dyn Runner + Sync)) -> Result<()> {
    let Some(wh) = r.warehouse() else {
        return Err(Error::Query("Snowflake necesita un warehouse para listar las consultas en curso: la conexión no tiene uno".into()));
    };
    let w = r.rows(&format!("SHOW WAREHOUSES LIKE '{}'", wh.replace('\'', "''").replace('\\', "\\\\"))).await?;
    let on = (0..w.rows.len())
        .any(|i| get(&w, i, "name").is_some_and(|n| n.eq_ignore_ascii_case(&wh)) && get(&w, i, "state").is_some_and(|s| s.eq_ignore_ascii_case("STARTED")));
    if on {
        Ok(())
    } else {
        Err(Error::Query(format!(
            "el warehouse {wh} está suspendido: la lista de procesos no lo reanuda para no consumir créditos"
        )))
    }
}

pub(crate) async fn processes(r: &(dyn Runner + Sync)) -> Result<Vec<ServerProcess>> {
    warehouse_running(r).await?;
    let sql = format!(
        "SELECT query_id, execution_status, user_name, warehouse_name, session_id, database_name, schema_name, query_type,
                DATEDIFF('millisecond', start_time, CURRENT_TIMESTAMP()) AS elapsed, bytes_scanned,
                LEFT(query_text, {MAX_TEXT}) AS query_text, (session_id::varchar = CURRENT_SESSION()) AS own
           FROM TABLE({is}.QUERY_HISTORY(RESULT_LIMIT => 10000))
          WHERE execution_status IN ('RUNNING', 'QUEUED', 'RESUMING_WAREHOUSE', 'BLOCKED')
            AND COALESCE(query_tag, '') <> '{TAG}'
          ORDER BY start_time LIMIT {MAX_ROWS}",
        is = info_schema(r)
    );
    Ok(rows(&r.rows(&sql).await?))
}

fn first(s: &Set) -> Option<&str> {
    s.rows.first()?.first()?.as_deref()
}

pub(crate) async fn cancel(r: &(dyn Runner + Sync), id: &str) -> Result<()> {
    let id = query_id(id).ok_or_else(|| Error::Query(format!("«{}» no es un id de consulta de Snowflake", id.trim())))?;
    // "query [01b…] terminated." or "Identified SQL statement is not
    // currently executing."
    let answer = r.rows(&format!("SELECT SYSTEM$CANCEL_QUERY('{id}')")).await?;
    match first(&answer) {
        Some(t) if t.contains("terminated") => Ok(()),
        _ => Err(Error::Query(format!("no se pudo cancelar la consulta {id}: ya terminó o no existe"))),
    }
}

/// Ends the session that runs query `id` (the process list's ids).
pub(crate) async fn abort_session_of(r: &(dyn Runner + Sync), id: &str) -> Result<()> {
    let id = query_id(id).ok_or_else(|| Error::Query(format!("«{}» no es un id de consulta de Snowflake", id.trim())))?;
    let found = r
        .rows(&format!(
            "SELECT session_id, (session_id::varchar = CURRENT_SESSION()) AS own
               FROM TABLE({is}.QUERY_HISTORY(RESULT_LIMIT => 10000)) WHERE query_id = '{id}'",
            is = info_schema(r)
        ))
        .await?;
    let session: i64 = get(&found, 0, "session_id")
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| Error::Query(format!("no se encontró la sesión de la consulta {id}")))?;
    if matches!(get(&found, 0, "own"), Some("true" | "TRUE" | "1")) {
        return Err(Error::Query("esa es la sesión con la que DBine está consultando: no se puede terminar desde acá".into()));
    }
    r.rows(&format!("SELECT SYSTEM$ABORT_SESSION({session})")).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_ids_are_checked() {
        assert_eq!(query_id(" 01b2c3d4-0000-1234-0000-00a1b2c3d4e5 "), Some("01b2c3d4-0000-1234-0000-00a1b2c3d4e5"));
        assert_eq!(query_id("1721330303831000000"), None, "a transaction id");
        assert_eq!(query_id("x'); DROP"), None);
    }

    #[test]
    fn history_rows_become_processes() {
        let cols = [
            "QUERY_ID", "EXECUTION_STATUS", "USER_NAME", "WAREHOUSE_NAME", "SESSION_ID", "DATABASE_NAME", "SCHEMA_NAME",
            "QUERY_TYPE", "ELAPSED", "BYTES_SCANNED", "QUERY_TEXT", "OWN",
        ];
        let row = ["01b2-c3", "QUEUED", "ANA", "WH", "123", "DB", "PUBLIC", "SELECT", "1500", "", "select 1", "false"];
        let s = Set {
            cols: cols.iter().map(|c| c.to_string()).collect(),
            rows: vec![row.iter().map(|v| (!v.is_empty()).then(|| v.to_string())).collect()],
        };
        let p = &rows(&s)[0];
        assert_eq!(p.id, "01b2-c3");
        assert!(p.active && !p.own);
        assert_eq!(p.wait.as_deref(), Some("en cola del warehouse"));
        assert_eq!(p.database.as_deref(), Some("DB.PUBLIC"));
        assert_eq!(p.program.as_deref(), Some("sesión 123"));
        assert_eq!(p.elapsed_ms, Some(1500));
        assert_eq!(p.reads, None);
    }
}
