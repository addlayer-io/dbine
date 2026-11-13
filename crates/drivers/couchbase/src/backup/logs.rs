//! The Backup Service's log as the console's event log shows it: the
//! cluster's system events (`GET /events`, Couchbase Server 7.1 on) of the
//! `backup` component (plans and repositories created, changed, archived,
//! deleted; tasks started, done, failed or skipped), newest first.

use super::{ready, Ctx};
use crate::CbSession;
use dbine_driver::{Error, MonitorTable, Result};
use serde_json::{json, Value};

/// Events read from the cluster (it keeps 10,000 by default).
const READ: usize = 10_000;
/// Rows shown at most.
const MAX_ROWS: usize = 2_000;

const NO_EVENTS: &str = "Leer los eventos del servicio de backup requiere Couchbase Server 7.1 o posterior y el permiso de ver los eventos del cluster (Read-Only Admin, Backup Full Admin o Full Admin).";

fn level(sev: &str) -> &'static str {
    match sev {
        "error" => "Error",
        "fatal" => "Grave",
        "warn" | "warning" => "Advertencia",
        _ => "Información",
    }
}

/// The attributes of an event, as `key=value` pairs.
fn detail(e: &Value) -> String {
    e.get("extra_attributes")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, v)| match v {
                    Value::String(s) => format!("{k}={s}"),
                    v => format!("{k}={v}"),
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// The backup events of an `/events` answer, newest first.
pub(crate) fn table(events: &Value) -> MonitorTable {
    let mut t = MonitorTable::new(
        "backup_events",
        "Eventos del servicio de backup",
        &["Fecha", "Nivel", "Evento", "Detalle", "Nodo"],
    );
    let list = events
        .get("events")
        .and_then(Value::as_array)
        .or_else(|| events.as_array());
    let mut rows: Vec<(String, Vec<Value>)> = list
        .into_iter()
        .flatten()
        .filter(|e| e.get("component").and_then(Value::as_str) == Some("backup"))
        .map(|e| {
            let at = e
                .get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let s = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or_default();
            (
                at.clone(),
                vec![
                    json!(at),
                    json!(level(s("severity"))),
                    json!(s("description")),
                    json!(detail(e)),
                    json!(s("node")),
                ],
            )
        })
        .collect();
    // ISO times sort as text.
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    rows.truncate(MAX_ROWS);
    t.rows = rows.into_iter().map(|(_, r)| r).collect();
    t
}

fn allowed(c: &Ctx) -> bool {
    c.events && c.version >= (7, 1)
}

pub(crate) async fn logs(s: &CbSession) -> Result<Vec<MonitorTable>> {
    let c = ready(s).await?;
    if !allowed(&c) {
        return Err(Error::Unsupported(NO_EVENTS.into()));
    }
    let v = s.conn.mgmt_get(&format!("/events?limit={READ}")).await?;
    Ok(vec![table(&v)])
}
