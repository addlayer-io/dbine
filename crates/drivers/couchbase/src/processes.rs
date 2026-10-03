//! The process list ([`dbine_driver::Session::processes`]) and cancelling
//! another request ([`dbine_driver::Session::cancel_query`]).
//!
//! The Query service is stateless HTTP: it has no sessions, only running
//! requests. The list is the monitor's `system:active_requests` (one row
//! per request, on every query node); the id is the `requestId`, and
//! cancelling deletes the request from `system:active_requests` (what the
//! console's "cancel" does). The listing's own request carries its
//! `client_context_id`, so it shows up as DBine's own.

use crate::{context_id, plan, CbSession};
use dbine_driver::{Error, Result, ServerProcess};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;

/// Characters kept of a statement.
const MAX_TEXT: usize = 20000;
const MAX_ROWS: usize = 2000;

/// A `requestId`: a UUID.
pub(crate) fn valid_request_id(id: &str) -> bool {
    id.len() == 36 && id.char_indices().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() })
}

fn s<'a>(r: &'a Value, k: &str) -> Option<&'a str> {
    r.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty())
}

pub(crate) fn rows(results: &[Value], own_ctx: &str) -> Vec<ServerProcess> {
    let ms = |r: &Value, k: &str| s(r, k).and_then(plan::duration_ms).map(|v| v.max(0.0) as u64);
    let mut out: Vec<ServerProcess> = results
        .iter()
        .filter_map(|r| {
            let id = s(r, "requestId")?.to_string();
            Some(ServerProcess {
                id,
                status: s(r, "state").map(str::to_string),
                active: true,
                own: s(r, "clientContextID") == Some(own_ctx),
                user: s(r, "users").map(str::to_string),
                host: s(r, "remoteAddr").map(str::to_string),
                program: s(r, "userAgent").map(str::to_string),
                database: s(r, "queryContext").map(|c| c.trim_start_matches("default:").to_string()),
                command: s(r, "statementType").map(str::to_string),
                elapsed_ms: ms(r, "elapsedTime"),
                cpu_ms: ms(r, "cpuTime"),
                reads: r.pointer("/phaseCounts/fetch").and_then(Value::as_u64),
                writes: r.get("mutations").and_then(Value::as_u64),
                sql: s(r, "statement").map(|t| t.chars().take(MAX_TEXT).collect()),
                ..Default::default()
            })
        })
        .collect();
    out.sort_by_key(|p| std::cmp::Reverse(p.elapsed_ms));
    out.truncate(MAX_ROWS);
    out
}

pub(crate) async fn processes(s: &CbSession) -> Result<Vec<ServerProcess>> {
    s.cancel.flag.store(false, Ordering::SeqCst);
    let own = context_id();
    let body = json!({
        "statement": "SELECT r.* FROM system:active_requests AS r",
        "client_context_id": own,
        "timeout": "5s",
    });
    let v = s.cancel.run(s.conn.post_query(&body)).await?;
    Ok(rows(v.get("results").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]), &own))
}

pub(crate) async fn cancel(s: &CbSession, id: &str) -> Result<()> {
    if s.conn.read_only {
        return Err(Error::Query("Conexión de solo lectura: no se pueden cancelar consultas.".into()));
    }
    let id = id.trim();
    if !valid_request_id(id) {
        return Err(Error::Query(format!("«{id}» no es un id de consulta (requestId)")));
    }
    s.cancel.flag.store(false, Ordering::SeqCst);
    let quoted = serde_json::to_string(id).unwrap_or_default();
    let found = s
        .cancel
        .run(s.conn.post_query(&json!({
            "statement": format!("SELECT RAW r.requestId FROM system:active_requests AS r WHERE r.requestId = {quoted}"),
        })))
        .await?;
    // DBine's own requests (the listing) have ended by the time a row can
    // be picked: there's no session of its own to protect.
    if found.get("results").and_then(Value::as_array).is_none_or(|a| a.is_empty()) {
        return Err(Error::Query(format!("la consulta {id} ya terminó o no existe")));
    }
    let stmt = format!("DELETE FROM system:active_requests WHERE requestId = {quoted}");
    s.cancel.run(s.conn.post_query(&json!({ "statement": stmt }))).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_from_active_requests() {
        let r = vec![
            json!({"requestId": "3a41b771-da0e-480d-a3a2-43505058e00f", "clientContextID": "me", "state": "running",
                   "elapsedTime": "5.1628ms", "cpuTime": "54.791µs", "statement": "SELECT 1", "statementType": "SELECT",
                   "users": "builtin:Administrator", "remoteAddr": "10.0.0.1:3", "userAgent": "curl"}),
            json!({"requestId": "244df474-bb21-4a60-b54a-06207385b8ea", "state": "running", "elapsedTime": "2.5s",
                   "queryContext": "default:`b`.`_default`", "phaseCounts": {"fetch": 12}, "statement": "SELECT * FROM b"}),
            json!({"state": "running"}),
        ];
        let p = rows(&r, "me");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].elapsed_ms, Some(2500));
        assert_eq!((p[0].database.as_deref(), p[0].reads), (Some("`b`.`_default`"), Some(12)));
        assert!(p[1].own && p[1].active);
        assert_eq!((p[1].elapsed_ms, p[1].cpu_ms, p[1].command.as_deref()), (Some(5), Some(0), Some("SELECT")));
        assert!(valid_request_id("3a41b771-da0e-480d-a3a2-43505058e00f"));
        assert!(!valid_request_id("1\" OR 1=1") && !valid_request_id(""));
    }
}
