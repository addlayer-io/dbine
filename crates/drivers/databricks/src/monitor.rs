//! `Session::monitor` for Databricks SQL, from the REST API only: the
//! warehouse (`/api/2.0/sql/warehouses/{id}`: state, size, clusters,
//! sessions) and its query history (`/api/2.0/sql/history/queries`: what's
//! running or queued, and the last minutes with their metrics). Running SQL
//! here would keep the warehouse from auto-stopping and cost DBUs every few
//! seconds, so system tables are left out.

use crate::DatabricksSession;
use dbine_driver::{Metric, MetricUnit, MonitorSnapshot, MonitorTable, Result};
use serde_json::{json, Value as Json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How far back "recent queries" go.
const RECENT: Duration = Duration::from_secs(15 * 60);
const MAX_SQL: usize = 2000;

fn truncate(s: &str) -> String {
    if s.chars().count() > MAX_SQL {
        s.chars().take(MAX_SQL).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

fn n(v: &Json, ptr: &str) -> Option<f64> {
    match v.pointer(ptr)? {
        Json::String(s) => s.parse().ok(),
        x => x.as_f64(),
    }
}

fn s<'a>(v: &'a Json, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(Json::as_str).unwrap_or("")
}

fn opt(v: Option<f64>) -> Json {
    v.map_or(Json::Null, |v| json!(v))
}

fn ms_text(ms: Option<f64>) -> Json {
    let Some(ms) = ms else { return Json::Null };
    json!(datetime((ms / 1000.0) as u64))
}

/// `YYYY-MM-DD HH:MM:SS` (UTC) of a Unix time (Hinnant's civil_from_days).
fn datetime(secs: u64) -> String {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
}

/// The warehouse's facts and figures.
pub(crate) fn warehouse(w: &Json, snap: &mut MonitorSnapshot) {
    for (label, ptr) in [
        ("Warehouse", "/name"),
        ("Estado", "/state"),
        ("Tamaño", "/cluster_size"),
        ("Tipo", "/warehouse_type"),
        ("Canal", "/channel/name"),
        ("Creador", "/creator_name"),
    ] {
        if !s(w, ptr).is_empty() {
            snap.info.push((label.into(), s(w, ptr).to_string()));
        }
    }
    if let Some(b) = w.get("enable_serverless_compute").and_then(Json::as_bool) {
        snap.info.push(("Serverless".into(), if b { "sí" } else { "no" }.into()));
    }
    if let Some(m) = n(w, "/auto_stop_mins") {
        snap.info.push(("Detención automática".into(), if m == 0.0 { "nunca".into() } else { format!("{m} min") }));
    }
    if let (Some(a), Some(b)) = (n(w, "/min_num_clusters"), n(w, "/max_num_clusters")) {
        snap.info.push(("Clusters (mín.–máx.)".into(), format!("{a}–{b}")));
    }
    if !s(w, "/health/status").is_empty() {
        let summary = s(w, "/health/summary");
        snap.info.push((
            "Salud".into(),
            if summary.is_empty() { s(w, "/health/status").to_string() } else { format!("{} ({summary})", s(w, "/health/status")) },
        ));
    }
    let running = s(w, "/state") == "RUNNING";
    snap.metrics.push(
        Metric::new(
            "clusters",
            "Clusters en ejecución",
            "Capacidad",
            MetricUnit::Count,
            n(w, "/num_clusters").or(Some(if running { 1.0 } else { 0.0 })),
        )
        .max(n(w, "/max_num_clusters")),
    );
    snap.metrics.push(Metric::new("connections", "Sesiones abiertas", "Conexiones", MetricUnit::Count, n(w, "/num_active_sessions")));
}

/// Running / queued queries and the recent ones, from the query history.
pub(crate) fn history(active: &[Json], recent: &[Json], now: f64, snap: &mut MonitorSnapshot) {
    let status = |q: &Json| s(q, "/status").to_string();
    let running = active.iter().filter(|q| status(q) == "RUNNING").count();
    let queued = active.iter().filter(|q| status(q) == "QUEUED").count();
    let done: Vec<&Json> = recent.iter().filter(|q| matches!(status(q).as_str(), "FINISHED" | "FAILED" | "CANCELED")).collect();
    let failed = done.iter().filter(|q| status(q) == "FAILED").count();
    let sum = |ptr: &str| done.iter().filter_map(|q| n(q, ptr)).fold(0.0, |a, b| a + b);
    let cached = done.iter().filter(|q| q.pointer("/metrics/result_from_cache").and_then(Json::as_bool) == Some(true)).count();
    let avg_ms = (!done.is_empty()).then(|| sum("/duration") / done.len() as f64);

    snap.metrics.extend([
        Metric::new("active_sessions", "Consultas en ejecución", "Actividad", MetricUnit::Count, Some(running as f64)),
        Metric::new("queued_queries", "Consultas en cola", "Actividad", MetricUnit::Count, Some(queued as f64)),
        Metric::new("recent_queries", "Consultas terminadas (últimos 15 min)", "Últimos 15 minutos", MetricUnit::Count, Some(done.len() as f64)),
        Metric::new("recent_failed", "Consultas fallidas (últimos 15 min)", "Últimos 15 minutos", MetricUnit::Count, Some(failed as f64)),
        Metric::new("avg_duration", "Duración media (últimos 15 min)", "Últimos 15 minutos", MetricUnit::Millis, avg_ms),
        Metric::new("bytes_read", "Datos leídos (últimos 15 min)", "Últimos 15 minutos", MetricUnit::Bytes, Some(sum("/metrics/read_bytes"))),
        Metric::new("rows_read", "Filas leídas (últimos 15 min)", "Últimos 15 minutos", MetricUnit::Count, Some(sum("/metrics/rows_read_count"))),
        Metric::new("task_time", "Tiempo de cómputo (últimos 15 min)", "Últimos 15 minutos", MetricUnit::Millis, Some(sum("/metrics/task_total_time_ms"))),
        Metric::new("spill", "Derrame a disco (últimos 15 min)", "Últimos 15 minutos", MetricUnit::Bytes, Some(sum("/metrics/spill_to_disk_bytes"))),
        Metric::new(
            "cache_hit",
            "Resultados desde caché (últimos 15 min)",
            "Caché",
            MetricUnit::Percent,
            (!done.is_empty()).then(|| (cached as f64 * 1000.0 / done.len() as f64).round() / 10.0),
        )
        .max(Some(100.0)),
    ]);

    let mut t = MonitorTable::new("queries", "Consultas en curso", &["Id", "Usuario", "Estado", "Inicio", "Duración (s)", "Tipo", "Consulta"]);
    for q in active.iter().take(200) {
        let start = n(q, "/query_start_time_ms");
        t.rows.push(vec![
            json!(s(q, "/query_id")),
            json!(s(q, "/user_name")),
            json!(s(q, "/status")),
            ms_text(start),
            opt(start.map(|st| ((now - st) / 1000.0).round())),
            json!(s(q, "/statement_type")),
            json!(truncate(s(q, "/query_text"))),
        ]);
    }
    snap.tables.push(t);

    let mut t = MonitorTable::new(
        "recent_queries",
        "Consultas recientes (últimos 15 min)",
        &["Id", "Usuario", "Estado", "Fin", "Duración (ms)", "Bytes leídos", "Filas producidas", "Caché", "Error", "Consulta"],
    );
    for q in done.iter().take(100) {
        let err = s(q, "/error_message");
        t.rows.push(vec![
            json!(s(q, "/query_id")),
            json!(s(q, "/user_name")),
            json!(s(q, "/status")),
            ms_text(n(q, "/query_end_time_ms")),
            opt(n(q, "/duration")),
            opt(n(q, "/metrics/read_bytes")),
            opt(n(q, "/rows_produced").or(n(q, "/metrics/rows_produced_count"))),
            json!(if q.pointer("/metrics/result_from_cache").and_then(Json::as_bool) == Some(true) { "sí" } else { "no" }),
            if err.is_empty() { Json::Null } else { json!(truncate(err)) },
            json!(truncate(s(q, "/query_text"))),
        ]);
    }
    snap.tables.push(t);
}

impl DatabricksSession {
    async fn history_page(&self, query: &str) -> Result<Vec<Json>> {
        let r = self
            .api
            .get(&format!(
                "/api/2.0/sql/history/queries?filter_by.warehouse_ids={}&include_metrics=true&max_results=100{query}",
                self.warehouse
            ))
            .await?;
        Ok(r.get("res").and_then(Json::as_array).cloned().unwrap_or_default())
    }

    pub(crate) async fn snapshot(&mut self) -> Result<MonitorSnapshot> {
        let mut snap = MonitorSnapshot::default();
        match self.api.get(&format!("/api/2.0/sql/warehouses/{}", self.warehouse)).await {
            Ok(w) => warehouse(&w, &mut snap),
            Err(e) => snap.notes.push(format!(
                "No se pudo leer el estado del warehouse (hace falta el permiso CAN_MONITOR o CAN_MANAGE sobre él): {e}"
            )),
        }
        let now = now_ms();
        let active = self.history_page("&filter_by.statuses=RUNNING&filter_by.statuses=QUEUED").await;
        let since = (now - RECENT.as_millis() as f64) as u64;
        let recent = self.history_page(&format!("&filter_by.query_start_time_range.start_time_ms={since}")).await;
        match (active, recent) {
            (Ok(a), Ok(r)) => history(&a, &r, now, &mut snap),
            (a, r) => {
                let e = a.err().or(r.err()).map(|e| e.to_string()).unwrap_or_default();
                snap.notes.push(format!("No se pudo leer el historial de consultas: {e}"));
            }
        }
        if snap.metrics.is_empty() && snap.tables.is_empty() {
            return Err(dbine_driver::Error::Query(snap.notes.join(" ")));
        }
        snap.notes.push(
            "Databricks no expone el CPU ni la memoria del warehouse por la API; el consumo se ve en el tiempo de cómputo de las consultas y en las tablas de sistema (system.billing, system.query)."
                .into(),
        );
        snap.notes.push(
            "El monitor no ejecuta SQL en el warehouse (lo mantendría encendido y consumiría DBU); los tamaños de tablas no se informan."
                .into(),
        );
        snap.notes.push("Sin permiso de administrador del workspace, el historial muestra solo tus consultas.".into());
        Ok(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warehouse_and_history() {
        let mut snap = MonitorSnapshot::default();
        warehouse(
            &json!({"name": "Starter", "state": "RUNNING", "cluster_size": "2X-Small", "num_clusters": 1, "max_num_clusters": 3,
                    "min_num_clusters": 1, "num_active_sessions": 4, "auto_stop_mins": 10, "enable_serverless_compute": true,
                    "health": {"status": "HEALTHY"}}),
            &mut snap,
        );
        let m = |snap: &MonitorSnapshot, k: &str| snap.metrics.iter().find(|m| m.key == k).map(|m| (m.value, m.max));
        assert_eq!(m(&snap, "clusters"), Some((Some(1.0), Some(3.0))));
        assert_eq!(m(&snap, "connections"), Some((Some(4.0), None)));
        assert!(snap.info.iter().any(|(k, v)| k == "Detención automática" && v == "10 min"));
        let active = vec![
            json!({"query_id": "a", "status": "RUNNING", "query_start_time_ms": 1_000_000.0, "query_text": "SELECT 1", "user_name": "u"}),
            json!({"query_id": "b", "status": "QUEUED", "query_start_time_ms": 1_005_000.0}),
        ];
        let recent = vec![
            json!({"query_id": "c", "status": "FINISHED", "duration": 100, "metrics": {"read_bytes": 1024, "rows_read_count": 10,
                   "task_total_time_ms": 50, "result_from_cache": true}}),
            json!({"query_id": "d", "status": "FAILED", "duration": 300, "error_message": "boom", "metrics": {"read_bytes": 0}}),
            json!({"query_id": "a", "status": "RUNNING"}),
        ];
        history(&active, &recent, 1_010_000.0, &mut snap);
        assert_eq!(m(&snap, "active_sessions"), Some((Some(1.0), None)));
        assert_eq!(m(&snap, "queued_queries"), Some((Some(1.0), None)));
        assert_eq!(m(&snap, "recent_queries"), Some((Some(2.0), None)));
        assert_eq!(m(&snap, "recent_failed"), Some((Some(1.0), None)));
        assert_eq!(m(&snap, "avg_duration"), Some((Some(200.0), None)));
        assert_eq!(m(&snap, "bytes_read"), Some((Some(1024.0), None)));
        assert_eq!(m(&snap, "cache_hit").unwrap().0, Some(50.0));
        let q = snap.tables.iter().find(|t| t.key == "queries").unwrap();
        assert_eq!(q.rows[0][4], json!(10.0));
        assert_eq!(datetime(1_700_000_000), "2023-11-14 22:13:20");
    }
}
