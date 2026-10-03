//! The process list ([`dbine_driver::Session::processes`]) and stopping a
//! query ([`dbine_driver::Session::cancel_query`]).
//!
//! Spanner's sessions are a client-side pool, not connections an operator
//! ends: the list is the queries running now
//! (`SPANNER_SYS.OLDEST_ACTIVE_QUERIES`, as the monitor reads it), the id
//! is `QUERY_ID`, and cancelling it is `CALL cancel_query('…')`. The
//! emulator implements neither.

use crate::{SpannerSession, API};
use dbine_driver::{Error, Result, ServerProcess};
use std::time::Duration;

/// Longest the list may take: it's polled every few seconds.
const QUERY_LIMIT: Duration = Duration::from_secs(5);
/// Characters kept of a statement's text.
const MAX_TEXT: usize = 20000;
/// Rows at most.
const MAX_ROWS: usize = 2000;
/// Marks the list's own query, so it leaves itself out.
const MARK: &str = "dbine-processes-list";

/// A `QUERY_ID` as the table shows it.
pub(crate) fn valid_id(id: &str) -> Option<&str> {
    let id = id.trim();
    (!id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')).then_some(id)
}

/// The last segment of a session name (`projects/…/sessions/<id>`).
fn short(session: &str) -> &str {
    session.rsplit('/').next().unwrap_or(session)
}

/// The statement's first word, upper-cased ("SELECT", "UPDATE"…).
fn command(sql: &str) -> Option<String> {
    let word: String = sql.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    (!word.is_empty()).then(|| word.to_ascii_uppercase())
}

/// Rows of `QUERY_ID, SESSION_ID, elapsed ms, CLIENT_IP_ADDRESS,
/// USER_AGENT_HEADER, PRIORITY (unused), TRANSACTION_TYPE, TEXT`; `own` is this
/// session's name.
fn rows(rs: &[Vec<Option<String>>], own: &str) -> Vec<ServerProcess> {
    let get = |r: &Vec<Option<String>>, i: usize| r.get(i).cloned().flatten().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    rs.iter()
        .filter(|r| !get(r, 7).is_some_and(|t| t.contains(MARK)))
        .map(|r| {
            let sql = get(r, 7).map(|t| t.chars().take(MAX_TEXT).collect::<String>());
            let session = get(r, 1);
            ServerProcess {
                id: get(r, 0).unwrap_or_default(),
                status: get(r, 6).map(|t| format!("en ejecución ({})", t.to_lowercase().replace('_', " "))).or(Some("en ejecución".into())),
                active: true,
                own: session.as_deref().is_some_and(|s| short(s) == short(own)),
                host: get(r, 3),
                program: get(r, 4),
                command: sql.as_deref().and_then(command),
                elapsed_ms: get(r, 2).and_then(|v| v.parse::<f64>().ok()).map(|v| v.max(0.0) as u64),
                sql,
                ..Default::default()
            }
        })
        .filter(|p| !p.id.is_empty())
        .collect()
}

impl SpannerSession {
    /// The emulator answers SPANNER_SYS and `cancel_query` with an opaque
    /// error.
    fn refuse_emulator(&self) -> Result<()> {
        if self.api.base.starts_with(API) {
            Ok(())
        } else {
            Err(Error::Unsupported("el emulador de Spanner no implementa SPANNER_SYS ni cancel_query: no hay consultas en curso que listar".into()))
        }
    }

    pub(crate) async fn processes_list(&mut self) -> Result<Vec<ServerProcess>> {
        self.refuse_emulator()?;
        let sql = format!(
            "SELECT /* {MARK} */ QUERY_ID, SESSION_ID, TIMESTAMP_DIFF(CURRENT_TIMESTAMP(), START_TIME, MILLISECOND),
                    CLIENT_IP_ADDRESS, USER_AGENT_HEADER, PRIORITY, TRANSACTION_TYPE, SUBSTR(TEXT, 1, {MAX_TEXT})
             FROM SPANNER_SYS.OLDEST_ACTIVE_QUERIES ORDER BY START_TIME LIMIT {MAX_ROWS}"
        );
        let rs = tokio::time::timeout(QUERY_LIMIT, self.text_rows(&sql, &[]))
            .await
            .map_err(|_| Error::Query("la lista de consultas tardó demasiado".into()))??;
        Ok(rows(&rs, &self.session))
    }

    pub(crate) async fn cancel_running(&mut self, id: &str) -> Result<()> {
        let id = valid_id(id).ok_or_else(|| Error::Query(format!("«{}» no es un id de consulta de Spanner", id.trim())))?;
        self.refuse_emulator()?;
        // Checked above: no quote can be in it.
        self.text_rows(&format!("CALL cancel_query('{id}')"), &[]).await.map(|_| ()).map_err(|e| match e {
            Error::Query(m) => Error::Query(format!("no se pudo cancelar la consulta {id}: {m}")),
            e => e,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_checked() {
        assert_eq!(valid_id(" 1234567890 "), Some("1234567890"));
        assert_eq!(valid_id("1') --"), None);
        assert_eq!(valid_id(""), None);
    }

    #[test]
    fn active_queries_become_processes() {
        let s = |v: &str| (!v.is_empty()).then(|| v.to_string());
        let rs = vec![
            ["42", "projects/p/instances/i/databases/d/sessions/abc", "1500", "10.0.0.1", "go-client", "HIGH", "READ_ONLY", "select 1"]
                .map(s)
                .to_vec(),
            ["43", "xyz", "10", "", "", "", "", "SELECT /* dbine-processes-list */ 1"].map(s).to_vec(),
        ];
        let ps = rows(&rs, "projects/p/instances/i/databases/d/sessions/xyz");
        assert_eq!(ps.len(), 1, "the list leaves itself out");
        let p = &ps[0];
        assert_eq!(p.id, "42");
        assert!(p.active && !p.own);
        assert_eq!(p.elapsed_ms, Some(1500));
        assert_eq!(p.command.as_deref(), Some("SELECT"));
        assert_eq!(p.status.as_deref(), Some("en ejecución (read only)"));
    }
}
