//! The process list ([`dbine_driver::Session::processes`]) and cancelling a
//! connection's query ([`dbine_driver::Session::cancel_query`]).
//!
//! One row per `performance_schema.perf_connections` row, with the query
//! it runs from `perf_queries` (both views the monitor reads), matched by
//! `conn_id`. Clients report their running queries with each heartbeat
//! (about every second), so a query shows up once it outlives one. The id
//! is the `conn_id`: cancelling runs `KILL QUERY '<kill_id>'` for that
//! connection's queries. There's no closing a connection
//! (`KILL CONNECTION`): see below.
//!
//! DBine reaches the server through taosAdapter's REST API, whose pooled
//! native connections serve every REST client: none of them is DBine's own,
//! so no row is flagged `own`, and closing one would cut a connection other
//! clients share.

use crate::ddl::lit;
use crate::{text, TdSession};
use dbine_driver::{Error, Result, ServerProcess};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::time::Duration;

type Row = Map<String, Value>;

/// Longest the list may take: it's polled every few seconds.
const QUERY_LIMIT: Duration = Duration::from_secs(5);
/// Characters kept of a statement's text (the view keeps 2048).
const MAX_TEXT: usize = 20000;
/// Rows at most.
const MAX_ROWS: usize = 2000;

const CONNECTIONS: &str = "SELECT conn_id, `user`, app, end_point, user_app, user_ip, \
                                  CAST(NOW() AS BIGINT) - CAST(last_access AS BIGINT) AS idle_ms \
                           FROM performance_schema.perf_connections";
const QUERIES: &str = "SELECT kill_id, conn_id, `user`, app, end_point, exec_usec, sub_num, `sql`, user_app, user_ip \
                       FROM performance_schema.perf_queries";

fn get(r: &Row, k: &str) -> Option<String> {
    r.get(k).map(text).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn num(r: &Row, k: &str) -> Option<f64> {
    match r.get(k)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Connections and running queries → one row per connection (plus any
/// query whose connection isn't listed anymore), running first.
pub(crate) fn merge(conns: &[Row], queries: &[Row]) -> Vec<ServerProcess> {
    let mut by_conn: HashMap<String, Vec<&Row>> = HashMap::new();
    for q in queries {
        if let Some(c) = get(q, "conn_id") {
            by_conn.entry(c).or_default().push(q);
        }
    }
    let row = |id: String, c: Option<&Row>, qs: &[&Row]| {
        let any = |k: &str| c.and_then(|c| get(c, k)).or_else(|| qs.iter().find_map(|q| get(q, k)));
        // The longest-running one stands for the connection.
        let q = qs.iter().max_by(|a, b| num(a, "exec_usec").unwrap_or(0.0).total_cmp(&num(b, "exec_usec").unwrap_or(0.0)));
        let sql = q.and_then(|q| get(q, "sql"));
        ServerProcess {
            id,
            status: Some(if q.is_some() { "ejecutando" } else { "inactiva" }.into()),
            active: q.is_some(),
            user: any("user"),
            host: any("user_ip").or_else(|| any("end_point")),
            program: any("user_app").or_else(|| any("app")),
            command: sql.as_deref().and_then(|s| s.split_whitespace().next()).map(str::to_uppercase),
            elapsed_ms: match q {
                Some(q) => num(q, "exec_usec").map(|us| (us / 1000.0).max(0.0) as u64),
                None => c.and_then(|c| num(c, "idle_ms")).map(|ms| ms.max(0.0) as u64),
            },
            wait: q.and_then(|q| num(q, "sub_num")).filter(|n| *n > 1.0).map(|n| format!("{n} subconsultas")),
            sql: sql.map(|s| s.chars().take(MAX_TEXT).collect()),
            ..Default::default()
        }
    };
    let mut out: Vec<ServerProcess> = conns
        .iter()
        .filter_map(|c| {
            let id = get(c, "conn_id")?;
            let qs = by_conn.remove(&id).unwrap_or_default();
            Some(row(id, Some(c), &qs))
        })
        .collect();
    out.extend(by_conn.into_iter().map(|(id, qs)| row(id, None, &qs)));
    out.sort_by(|a, b| b.active.cmp(&a.active).then_with(|| (a.id.len(), &a.id).cmp(&(b.id.len(), &b.id))));
    out.truncate(MAX_ROWS);
    out
}

/// A connection id (an unsigned 32-bit number).
fn conn_id(id: &str) -> Result<u32> {
    let id = id.trim();
    id.parse().map_err(|_| Error::Query(format!("«{id}» no es un id de conexión de TDengine")))
}

impl TdSession {
    /// Rows by column name, outside the session's cancel (a stopped query
    /// in the editor mustn't fail the list).
    async fn rows(&self, stmt: &str) -> Result<Vec<Row>> {
        let a = self.conn.sql(None, stmt).await?;
        Ok(a.data.into_iter().map(|r| a.columns.iter().map(|(n, _)| n.clone()).zip(r).collect()).collect())
    }

    pub(crate) async fn processes(&self) -> Result<Vec<ServerProcess>> {
        let both = async { Ok::<_, Error>((self.rows(CONNECTIONS).await?, self.rows(QUERIES).await?)) };
        let (conns, queries) = tokio::time::timeout(QUERY_LIMIT, both)
            .await
            .map_err(|_| Error::Query("performance_schema no respondió a tiempo".into()))??;
        Ok(merge(&conns, &queries))
    }

    pub(crate) async fn cancel_connection_query(&self, id: &str) -> Result<()> {
        let n = conn_id(id)?;
        let kill_ids: Vec<String> =
            self.rows(&format!("SELECT kill_id FROM performance_schema.perf_queries WHERE conn_id = {n}")).await?.iter().filter_map(|r| get(r, "kill_id")).collect();
        if kill_ids.is_empty() {
            return Err(Error::Query(format!("la conexión {n} no está ejecutando ninguna consulta (o todavía no la informó)")));
        }
        for k in kill_ids {
            self.conn.sql(None, &format!("KILL QUERY {}", lit(&k))).await.map_err(|e| match e {
                Error::Query(m) => Error::Query(format!("no se pudo cancelar la consulta {k} de la conexión {n}: {m}")),
                e => e,
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(v: Value) -> Row {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn connections_with_their_queries() {
        let conns = vec![
            row(json!({"conn_id": 425933464u32, "user": "root", "app": "taosadapter", "end_point": "0.0.0.0:42214", "user_app": "", "user_ip": "", "idle_ms": 1276.0})),
            row(json!({"conn_id": 2794328456u32, "user": "root", "app": "taosadapter", "end_point": "0.0.0.0:42232", "user_app": "grafana", "user_ip": "10.0.0.7", "idle_ms": 5.0})),
        ];
        let queries = vec![
            row(json!({"kill_id": "a68e1188:8f", "conn_id": 2794328456u32, "user": "root", "exec_usec": 749237, "sub_num": 1, "sql": "select v from big"})),
            row(json!({"kill_id": "b1:1", "conn_id": 7, "user": "app", "exec_usec": 2000, "sub_num": 4, "sql": "select 1"})),
        ];
        let p = merge(&conns, &queries);
        assert_eq!(p.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["7", "2794328456", "425933464"]);
        let busy = &p[1];
        assert!(busy.active);
        assert_eq!((busy.elapsed_ms, busy.command.as_deref()), (Some(749), Some("SELECT")));
        assert_eq!((busy.host.as_deref(), busy.program.as_deref()), (Some("10.0.0.7"), Some("grafana")));
        assert_eq!(p[0].wait.as_deref(), Some("4 subconsultas"));
        let idle = &p[2];
        assert!(!idle.active && idle.sql.is_none());
        assert_eq!((idle.elapsed_ms, idle.host.as_deref(), idle.program.as_deref()), (Some(1276), Some("0.0.0.0:42214"), Some("taosadapter")));
        assert!(conn_id("12; DROP DATABASE x").is_err());
        assert_eq!(conn_id(" 2794328456 ").unwrap(), 2794328456);
    }
}
