//! The process list ([`dbine_driver::Session::processes`]).
//!
//! - Cassandra 4.0+: the monitor's `system_views.clients` (one row per
//!   connection, id "address:port") plus `system_views.queries` (4.1+, the
//!   requests running now, id their thread). The two aren't linked: a
//!   query doesn't say which connection sent it.
//! - ScyllaDB: `system.clients` (connections only).
//!
//! Virtual tables are node-local: the list is the coordinator node's.
//! Connections opened by this session carry its `CLIENT_ID` (sent in
//! STARTUP), so they show up as DBine's own. CQL can't stop another
//! client's request nor close its connection: no `cancel_query`.
//! Amazon Keyspaces exposes neither table.

use crate::monitor::{f, query, s, Rec};
use crate::Flavor;
use dbine_driver::{Error, Result, ServerProcess};
use scylla::client::session::Session;
use scylla::value::CqlValue;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Longest the list may take: it's polled every few seconds.
const QUERY_LIMIT: Duration = Duration::from_secs(5);
/// Characters kept of a request's text.
const MAX_TEXT: usize = 20000;
const MAX_ROWS: usize = 2000;
/// The listing itself, as `system_views.queries` shows it.
const QUERIES: &str = "SELECT * FROM system_views.queries LIMIT 2000";

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A per-session `CLIENT_ID`.
pub(crate) fn new_client_id() -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("dbine-{}-{nanos:x}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed))
}

/// Why a flavor has no process list, in Spanish.
pub(crate) fn unsupported_reason(flavor: Flavor) -> Option<&'static str> {
    match flavor {
        Flavor::Keyspaces => Some("Amazon Keyspaces no expone sus conexiones ni las consultas en curso"),
        _ => None,
    }
}

async fn within(session: &Session, cql: &str) -> Result<std::result::Result<Vec<Rec>, String>> {
    tokio::time::timeout(QUERY_LIMIT, query(session, cql))
        .await
        .map_err(|_| Error::Query("el servidor tardó demasiado en listar sus conexiones".into()))
}

fn opt(r: &Rec, k: &str) -> Option<String> {
    Some(s(r, k).trim().to_string()).filter(|v| !v.is_empty())
}

fn option(r: &Rec, key: &str) -> Option<String> {
    match r.get("client_options") {
        Some(Some(CqlValue::Map(m))) => m.iter().find_map(|(k, v)| match (k, v) {
            (CqlValue::Text(k) | CqlValue::Ascii(k), CqlValue::Text(v) | CqlValue::Ascii(v)) if k == key => Some(v.clone()),
            _ => None,
        }),
        _ => None,
    }
}

pub(crate) fn client_rows(rows: &[Rec], client_id: &str) -> Vec<ServerProcess> {
    rows.iter()
        .filter_map(|r| {
            let addr = opt(r, "address")?;
            let port = opt(r, "port")?;
            let id = if addr.contains(':') { format!("[{addr}]:{port}") } else { format!("{addr}:{port}") };
            let driver = format!("{} {}", s(r, "driver_name"), s(r, "driver_version")).trim().to_string();
            Some(ServerProcess {
                id,
                status: opt(r, "connection_stage"),
                active: false,
                own: option(r, "CLIENT_ID").as_deref() == Some(client_id),
                user: opt(r, "username"),
                host: opt(r, "hostname").or(Some(addr)),
                program: option(r, "APPLICATION_NAME").or(Some(driver).filter(|d| !d.is_empty())),
                database: opt(r, "keyspace_name"),
                ..Default::default()
            })
        })
        .collect()
}

pub(crate) fn query_rows(rows: &[Rec]) -> Vec<ServerProcess> {
    rows.iter()
        .filter_map(|r| {
            let task = s(r, "task");
            let own = task.contains(QUERIES);
            Some(ServerProcess {
                id: opt(r, "thread_id")?,
                status: Some("ejecutando".into()),
                active: true,
                own,
                command: task.split_whitespace().next().map(str::to_string),
                elapsed_ms: f(r, "running_micros").map(|us| (us / 1000.0) as u64),
                wait: f(r, "queued_micros").filter(|us| *us >= 1000.0).map(|us| format!("en cola {:.0} ms", us / 1000.0)),
                sql: Some(task.chars().take(MAX_TEXT).collect()).filter(|t: &String| !t.is_empty()),
                ..Default::default()
            })
        })
        .collect()
}

pub(crate) async fn processes(session: &Session, flavor: Flavor, client_id: &str) -> Result<Vec<ServerProcess>> {
    if let Some(why) = unsupported_reason(flavor) {
        return Err(Error::Unsupported(why.into()));
    }
    let mut out = Vec::new();
    if flavor == Flavor::Scylla {
        let clients = within(session, "SELECT * FROM system.clients LIMIT 2000").await?.map_err(Error::Query)?;
        out.extend(client_rows(&clients, client_id));
    } else {
        let clients = within(session, "SELECT * FROM system_views.clients LIMIT 2000").await?.map_err(|e| {
            Error::Unsupported(format!("este Cassandra no lista sus conexiones (system_views llegó en la 4.0): {e}"))
        })?;
        // `queries` arrived in 4.1: without it, connections only.
        if let Ok(q) = within(session, QUERIES).await? {
            out.extend(query_rows(&q));
        }
        out.extend(client_rows(&clients, client_id));
    }
    out.truncate(MAX_ROWS);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn text(v: &str) -> Option<CqlValue> {
        Some(CqlValue::Text(v.into()))
    }

    #[test]
    fn rows_from_virtual_tables() {
        let client: Rec = [
            ("address".to_string(), Some(CqlValue::Inet(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))))),
            ("port".to_string(), Some(CqlValue::Int(48306))),
            ("connection_stage".to_string(), text("ready")),
            ("driver_name".to_string(), text("DataStax Python Driver")),
            ("driver_version".to_string(), text("3.29.0")),
            ("username".to_string(), text("anonymous")),
            ("keyspace_name".to_string(), None),
            (
                "client_options".to_string(),
                Some(CqlValue::Map(vec![(CqlValue::Text("CLIENT_ID".into()), CqlValue::Text("me".into()))])),
            ),
        ]
        .into_iter()
        .collect();
        let c = client_rows(&[client], "me");
        assert_eq!(c[0].id, "10.0.0.5:48306");
        assert!(c[0].own && !c[0].active);
        assert_eq!(c[0].program.as_deref(), Some("DataStax Python Driver 3.29.0"));
        assert_eq!(c[0].database, None);

        let q: Rec = [
            ("thread_id".to_string(), text("Native-Transport-Requests-8")),
            ("queued_micros".to_string(), Some(CqlValue::BigInt(133))),
            ("running_micros".to_string(), Some(CqlValue::BigInt(2_500_000))),
            ("task".to_string(), text("QUERY select * from t; [pageSize = 100] at consistency ONE")),
        ]
        .into_iter()
        .collect();
        let r = query_rows(&[q]);
        assert_eq!((r[0].id.as_str(), r[0].active, r[0].own), ("Native-Transport-Requests-8", true, false));
        assert_eq!((r[0].command.as_deref(), r[0].elapsed_ms, r[0].wait.as_deref()), (Some("QUERY"), Some(2500), None));
        assert!(unsupported_reason(Flavor::Keyspaces).is_some() && unsupported_reason(Flavor::Scylla).is_none());
    }
}
