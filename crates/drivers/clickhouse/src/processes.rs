//! The process list ([`dbine_driver::Session::processes`]) and stopping a
//! running query ([`dbine_driver::Session::cancel_query`]).
//!
//! ClickHouse (and Timeplus Proton) speak HTTP: there are no sessions to
//! list or end, only the queries running now in `system.processes`. Each row
//! is one query, its id is the `query_id`, and cancelling it is
//! `KILL QUERY WHERE query_id = …`.

use crate::{text, ClickHouseSession};
use dbine_driver::{Error, Result, ServerProcess};
use serde_json::Value;
use std::time::Duration;

/// Longest the list may take: it's polled every few seconds.
const QUERY_LIMIT: Duration = Duration::from_secs(5);
/// Characters kept of a statement's text.
const MAX_TEXT: usize = 20000;
/// Rows at most.
const MAX_ROWS: usize = 2000;
/// Tags the listing query so it leaves itself out.
const MARK: &str = "dbine_processes_list";

/// A `query_id` the list showed: the server's UUIDs, or a client's own id
/// (letters, digits and `-_.:`). Anything else is refused before SQL.
pub(crate) fn valid_id(id: &str) -> Option<&str> {
    let id = id.trim();
    (!id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c))).then_some(id)
}

/// The statement's first word, upper-cased ("SELECT", "INSERT"…).
fn command(sql: &str) -> Option<String> {
    let word: String = sql.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    (!word.is_empty()).then(|| word.to_ascii_uppercase())
}

fn opt(v: Option<&Value>) -> Option<String> {
    v.map(text).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

/// One `system.processes` row (columns as in `processes`).
fn row(r: &[Value]) -> ServerProcess {
    let sql = opt(r.get(8)).map(|q| q.chars().take(MAX_TEXT).collect::<String>());
    let cancelled = num(r.get(9)).is_some_and(|v| v != 0.0);
    ServerProcess {
        id: opt(r.first()).unwrap_or_default(),
        status: Some(if cancelled { "cancelando" } else { "en ejecución" }.into()),
        active: true,
        // A distributed query's parts on other shards.
        system: num(r.get(10)).is_some_and(|v| v == 0.0),
        user: opt(r.get(1)),
        host: opt(r.get(2)).map(|a| a.trim_start_matches("::ffff:").to_string()),
        program: opt(r.get(3)),
        database: opt(r.get(4)),
        command: sql.as_deref().and_then(command),
        elapsed_ms: num(r.get(5)).map(|s| (s.max(0.0) * 1000.0) as u64),
        reads: num(r.get(6)).map(|v| v.max(0.0) as u64),
        writes: num(r.get(7)).map(|v| v.max(0.0) as u64),
        sql,
        ..Default::default()
    }
}

impl ClickHouseSession {
    pub(crate) async fn processes_list(&self) -> Result<Vec<ServerProcess>> {
        let sql = format!(
            "SELECT /* {MARK} */ query_id, user, address,
                    if(http_user_agent != '', http_user_agent, client_name), current_database,
                    elapsed, read_rows, written_rows, substring(query, 1, {MAX_TEXT}), is_cancelled, is_initial_query
             FROM system.processes
             WHERE query NOT LIKE '%{MARK}%'
             ORDER BY elapsed DESC LIMIT {MAX_ROWS}
             SETTINGS max_execution_time = 5"
        );
        let rows = tokio::time::timeout(QUERY_LIMIT, self.rows(&sql, &[]))
            .await
            .map_err(|_| Error::Query("la lista de consultas tardó demasiado".into()))??;
        Ok(rows.iter().map(|r| row(r)).filter(|p| !p.id.is_empty()).collect())
    }

    pub(crate) async fn cancel_running(&self, id: &str) -> Result<()> {
        let id = valid_id(id).ok_or_else(|| Error::Query(format!("«{}» no es un id de consulta de ClickHouse", id.trim())))?;
        // Checked above: no quote or backslash can be in it.
        let rows = self.rows(&format!("KILL QUERY WHERE query_id = '{id}' ASYNC"), &[]).await?;
        if rows.is_empty() {
            return Err(Error::Query(format!(
                "no se pudo cancelar la consulta {id}: ya terminó o tu usuario no tiene permiso para verla"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ids_are_checked() {
        assert_eq!(valid_id(" 4f8e2a1c-1b2c-4d5e-8f90-a1b2c3d4e5f6 "), Some("4f8e2a1c-1b2c-4d5e-8f90-a1b2c3d4e5f6"));
        assert_eq!(valid_id("my_job.1:a"), Some("my_job.1:a"));
        assert_eq!(valid_id("x' OR 1=1 --"), None);
        assert_eq!(valid_id(""), None);
    }

    #[test]
    fn rows_become_processes() {
        let r = [
            json!("abc"),
            json!("dbine"),
            json!("::ffff:172.17.0.1"),
            json!("curl/8"),
            json!("default"),
            json!(1.5),
            json!("1000"),
            json!("0"),
            json!(" select sleep(3)"),
            json!(0),
            json!(1),
        ];
        let p = row(&r);
        assert_eq!(p.id, "abc");
        assert!(p.active && !p.system && !p.own);
        assert_eq!(p.host.as_deref(), Some("172.17.0.1"));
        assert_eq!(p.command.as_deref(), Some("SELECT"));
        assert_eq!(p.elapsed_ms, Some(1500));
        assert_eq!(p.reads, Some(1000));
        assert_eq!(p.status.as_deref(), Some("en ejecución"));
    }
}
