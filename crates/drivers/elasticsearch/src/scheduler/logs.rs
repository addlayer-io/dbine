//! The scheduler's logs, newest first: on Elasticsearch the transforms'
//! audit messages (`.transform-notifications-read`, Kibana's "Messages"
//! tab), Watcher's history (`.watcher-history-*`) and SLM's
//! (`.slm-history-*`); on OpenSearch and Open Distro the Alerting plugin's
//! alerts (active and past) and each snapshot policy's last run. A log the
//! user may not read is left out.

use super::{ctx, Type};
use crate::jobs::{iso, millis, num, text};
use crate::json::J;
use crate::EsSession;
use dbine_driver::{MonitorTable, Result};
use serde_json::Value;

/// Rows per log.
const ROWS: usize = 200;

fn cell(v: Option<String>) -> Value {
    v.map_or(Value::Null, Value::String)
}

/// The hits of a search on `index`, newest first by `sort`; `None` when the
/// search is refused.
async fn hits(s: &EsSession, index: &str, sort: &str) -> Option<Vec<J>> {
    let body = J::parse(&format!(
        r#"{{"size": {ROWS}, "sort": [{{"{sort}": {{"order": "desc", "unmapped_type": "date"}}}}]}}"#
    ))
    .ok()?;
    let path = format!("/{index}/_search?ignore_unavailable=true&allow_no_indices=true");
    match s.job_probe("POST", &path, Some(&body)).await {
        Ok((200, v)) => Some(
            v.at(&["hits", "hits"])
                .and_then(J::as_arr)
                .map(|a| a.iter().filter_map(|h| h.get("_source").cloned()).collect())
                .unwrap_or_default(),
        ),
        _ => None,
    }
}

fn transform_messages(rows: &[J]) -> MonitorTable {
    let mut t = MonitorTable::new(
        "transform_messages",
        "Mensajes de los transforms",
        &["Fecha", "Transform", "Nivel", "Mensaje", "Nodo"],
    );
    for r in rows {
        t.rows.push(vec![
            cell(millis(r.get("timestamp")).or_else(|| iso(r.get("timestamp")))),
            cell(text(r.get("transform_id"))),
            cell(text(r.get("level"))),
            cell(text(r.get("message"))),
            cell(text(r.get("node_name"))),
        ]);
    }
    t
}

fn watcher_history(rows: &[J]) -> MonitorTable {
    let mut t = MonitorTable::new(
        "watcher_history",
        "Historial de Watcher",
        &[
            "Fecha",
            "Watch",
            "Estado",
            "Disparo",
            "Duración (ms)",
            "Acciones",
            "Mensaje",
        ],
    );
    for r in rows {
        let actions: Vec<String> = r
            .at(&["result", "actions"])
            .and_then(J::as_arr)
            .into_iter()
            .flatten()
            .map(|a| {
                format!(
                    "{}: {}",
                    text(a.get("id")).unwrap_or_default(),
                    text(a.get("status")).unwrap_or_default()
                )
            })
            .collect();
        let mut msg: Vec<String> = r
            .get("messages")
            .and_then(J::as_arr)
            .into_iter()
            .flatten()
            .map(J::text)
            .filter(|m| !m.is_empty())
            .collect();
        if let Some(e) = text(r.at(&["exception", "reason"])) {
            msg.push(e);
        }
        t.rows.push(vec![
            cell(iso(r.at(&["trigger_event", "triggered_time"]))),
            cell(text(r.get("watch_id"))),
            cell(text(r.get("state"))),
            cell(text(r.at(&["trigger_event", "type"]))),
            num(r.at(&["result", "execution_duration"])).map_or(Value::Null, |d| d.into()),
            cell((!actions.is_empty()).then(|| actions.join(", "))),
            cell((!msg.is_empty()).then(|| jobs_cut(&msg.join("\n")))),
        ]);
    }
    t
}

fn jobs_cut(s: &str) -> String {
    crate::jobs::cut(s)
}

fn slm_history(rows: &[J]) -> MonitorTable {
    let mut t = MonitorTable::new(
        "slm_history",
        "Historial de SLM",
        &[
            "Fecha",
            "Política",
            "Operación",
            "Snapshot",
            "Repositorio",
            "Correcto",
            "Error",
        ],
    );
    for r in rows {
        t.rows.push(vec![
            cell(millis(r.get("@timestamp")).or_else(|| iso(r.get("@timestamp")))),
            cell(text(r.get("policy"))),
            cell(text(r.get("operation"))),
            cell(text(r.get("snapshot_name"))),
            cell(text(r.get("repository"))),
            r.get("success")
                .and_then(J::as_bool)
                .map_or(Value::Null, Value::Bool),
            cell(text(r.get("error_details")).map(|e| jobs_cut(&e))),
        ]);
    }
    t
}

fn alerts(v: &J) -> MonitorTable {
    let mut t = MonitorTable::new(
        "alerts",
        "Alertas",
        &[
            "Inicio",
            "Monitor",
            "Disparador",
            "Estado",
            "Severidad",
            "Fin",
            "Error",
        ],
    );
    let mut rows: Vec<&J> = v
        .get("alerts")
        .and_then(J::as_arr)
        .into_iter()
        .flatten()
        .collect();
    rows.sort_by(|a, b| {
        num(b.get("start_time"))
            .unwrap_or(0.0)
            .total_cmp(&num(a.get("start_time")).unwrap_or(0.0))
    });
    for r in rows.into_iter().take(ROWS) {
        t.rows.push(vec![
            cell(millis(r.get("start_time"))),
            cell(text(r.get("monitor_name"))),
            cell(text(r.get("trigger_name"))),
            cell(text(r.get("state"))),
            cell(text(r.get("severity"))),
            cell(millis(r.get("end_time"))),
            cell(text(r.get("error_message")).map(|e| jobs_cut(&e))),
        ]);
    }
    t
}

fn sm_runs(policies: &[(J, Option<J>)]) -> MonitorTable {
    let mut t = MonitorTable::new(
        "sm_runs",
        "Snapshots de las políticas",
        &[
            "Política",
            "Estado",
            "Resultado",
            "Inicio",
            "Fin",
            "Mensaje",
            "Próximo",
        ],
    );
    for (p, e) in policies {
        let last = e
            .as_ref()
            .and_then(|e| e.at(&["creation", "latest_execution"]));
        let info = last.and_then(|l| l.get("info"));
        let msg = [
            info.and_then(|i| text(i.get("message"))),
            info.and_then(|i| text(i.get("cause"))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(": ");
        t.rows.push(vec![
            cell(text(p.get("name"))),
            cell(
                e.as_ref()
                    .and_then(|e| text(e.at(&["creation", "current_state"]))),
            ),
            cell(last.and_then(|l| text(l.get("status")))),
            cell(last.and_then(|l| millis(l.get("start_time")))),
            cell(last.and_then(|l| millis(l.get("end_time")))),
            cell((!msg.is_empty()).then(|| jobs_cut(&msg))),
            cell(
                e.as_ref()
                    .and_then(|e| millis(e.at(&["creation", "trigger", "time"]))),
            ),
        ]);
    }
    t
}

pub(crate) async fn logs(s: &EsSession) -> Result<Vec<MonitorTable>> {
    let c = ctx(s).await?;
    let mut out = Vec::new();
    if !c.os {
        if c.has(Type::Transform) {
            if let Some(rows) = hits(s, ".transform-notifications-read", "timestamp").await {
                out.push(transform_messages(&rows));
            }
        }
        if c.has(Type::Watch) {
            if let Some(rows) = hits(s, ".watcher-history-*", "trigger_event.triggered_time").await
            {
                out.push(watcher_history(&rows));
            }
        }
        if c.has(Type::Slm) {
            if let Some(rows) = hits(s, ".slm-history-*", "@timestamp").await {
                out.push(slm_history(&rows));
            }
        }
        return Ok(out);
    }
    if c.has(Type::Monitor) {
        let path = format!(
            "/{}/_alerting/monitors/alerts?size={ROWS}&sortString=start_time&sortOrder=desc",
            c.plugins
        );
        if let Ok((200, v)) = s.job_probe("GET", &path, None).await {
            out.push(alerts(&v));
        }
    }
    if c.has(Type::Sm) {
        if let Ok(Ok(ps)) = s.os_sm_policies().await {
            out.push(sm_runs(&ps));
        }
    }
    Ok(out)
}
