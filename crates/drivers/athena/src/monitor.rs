//! `Session::monitor` for Athena. Serverless: no CPU, memory or
//! connections. What the API reports, without running (and paying for)
//! any query: the workgroup's latest query executions (`ListQueryExecutions`
//! + `BatchGetQueryExecution`: state, data scanned, queue and engine time),
//! the workgroups, and provisioned capacity reservations (DPUs).

use crate::{err, AthenaSession};
use aws_sdk_athena::primitives::{DateTime, DateTimeFormat};
use aws_sdk_athena::types::{QueryExecution, QueryExecutionState};
use dbine_driver::{Metric, MetricUnit, MonitorSnapshot, MonitorTable, Result};
use serde_json::{json, Value as Json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How far back "recent queries" go.
const RECENT: Duration = Duration::from_secs(15 * 60);
/// Executions looked at per snapshot (the API's batch limit).
const BATCH: i32 = 50;
const MAX_SQL: usize = 2000;

fn truncate(s: &str) -> String {
    if s.chars().count() > MAX_SQL {
        s.chars().take(MAX_SQL).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

fn when(t: Option<&DateTime>) -> Json {
    t.and_then(|t| t.fmt(DateTimeFormat::DateTime).ok()).map_or(Json::Null, |s| json!(s.replace('T', " ").trim_end_matches('Z').to_string()))
}

fn opt(v: Option<i64>) -> Json {
    v.map_or(Json::Null, |v| json!(v))
}

fn state(q: &QueryExecution) -> Option<&QueryExecutionState> {
    q.status().and_then(|s| s.state())
}

fn active(q: &QueryExecution) -> bool {
    matches!(state(q), Some(QueryExecutionState::Running | QueryExecutionState::Queued))
}

/// Metrics and tables from a batch of executions (newest first). `now` in
/// seconds since the epoch.
pub(crate) fn executions(list: &[QueryExecution], now: f64, snap: &mut MonitorSnapshot) {
    let since = now - RECENT.as_secs_f64();
    let is = |q: &QueryExecution, s: QueryExecutionState| state(q) == Some(&s);
    let running = list.iter().filter(|q| is(q, QueryExecutionState::Running)).count();
    let queued = list.iter().filter(|q| is(q, QueryExecutionState::Queued)).count();
    let recent: Vec<&QueryExecution> = list
        .iter()
        .filter(|q| !active(q))
        .filter(|q| q.status().and_then(|s| s.completion_date_time().or(s.submission_date_time())).is_some_and(|t| t.as_secs_f64() >= since))
        .collect();
    let stat = |f: fn(&aws_sdk_athena::types::QueryExecutionStatistics) -> Option<i64>| {
        recent.iter().filter_map(|q| q.statistics().and_then(f)).map(|v| v as f64).fold(0.0, |a, b| a + b)
    };
    let failed = recent.iter().filter(|q| is(q, QueryExecutionState::Failed)).count();
    let reused = recent
        .iter()
        .filter(|q| q.statistics().and_then(|s| s.result_reuse_information()).is_some_and(|r| r.reused_previous_result()))
        .count();
    let n = recent.len() as f64;
    snap.metrics.extend([
        Metric::new("active_sessions", "Consultas en ejecución", "Actividad", MetricUnit::Count, Some(running as f64)),
        Metric::new("queued_queries", "Consultas en cola", "Actividad", MetricUnit::Count, Some(queued as f64)),
        Metric::new("recent_queries", "Consultas terminadas (últimos 15 min)", "Últimos 15 minutos", MetricUnit::Count, Some(n)),
        Metric::new("recent_failed", "Consultas fallidas (últimos 15 min)", "Últimos 15 minutos", MetricUnit::Count, Some(failed as f64)),
        Metric::new(
            "bytes_scanned",
            "Datos escaneados (últimos 15 min)",
            "Últimos 15 minutos",
            MetricUnit::Bytes,
            Some(stat(|s| s.data_scanned_in_bytes())),
        ),
        Metric::new(
            "engine_time",
            "Tiempo de motor (últimos 15 min)",
            "Últimos 15 minutos",
            MetricUnit::Millis,
            Some(stat(|s| s.engine_execution_time_in_millis())),
        ),
        Metric::new(
            "queue_time",
            "Espera media en cola (últimos 15 min)",
            "Últimos 15 minutos",
            MetricUnit::Millis,
            (n > 0.0).then(|| (stat(|s| s.query_queue_time_in_millis()) / n).round()),
        ),
        Metric::new(
            "cache_hit",
            "Resultados reutilizados (últimos 15 min)",
            "Caché",
            MetricUnit::Percent,
            (n > 0.0).then(|| (reused as f64 * 1000.0 / n).round() / 10.0),
        )
        .max(Some(100.0)),
    ]);

    let mut t = MonitorTable::new(
        "queries",
        "Consultas en curso",
        &["Id", "Estado", "Base", "Enviada", "Cola (ms)", "Motor (ms)", "Datos escaneados", "Consulta"],
    );
    for q in list.iter().filter(|q| active(q)).take(200) {
        let st = q.statistics();
        t.rows.push(vec![
            json!(q.query_execution_id().unwrap_or("")),
            json!(state(q).map(|s| s.as_str()).unwrap_or("")),
            json!(q.query_execution_context().and_then(|c| c.database()).unwrap_or("")),
            when(q.status().and_then(|s| s.submission_date_time())),
            opt(st.and_then(|s| s.query_queue_time_in_millis())),
            opt(st.and_then(|s| s.engine_execution_time_in_millis())),
            opt(st.and_then(|s| s.data_scanned_in_bytes())),
            json!(truncate(q.query().unwrap_or(""))),
        ]);
    }
    snap.tables.push(t);

    let mut t = MonitorTable::new(
        "recent_queries",
        "Consultas recientes (últimos 15 min)",
        &["Id", "Estado", "Tipo", "Fin", "Total (ms)", "Cola (ms)", "Datos escaneados", "DPU", "Error", "Consulta"],
    );
    for q in recent.iter().take(100) {
        let st = q.statistics();
        let error = q.status().and_then(|s| s.state_change_reason()).filter(|_| is(q, QueryExecutionState::Failed));
        t.rows.push(vec![
            json!(q.query_execution_id().unwrap_or("")),
            json!(state(q).map(|s| s.as_str()).unwrap_or("")),
            json!(q.statement_type().map(|s| s.as_str()).unwrap_or("")),
            when(q.status().and_then(|s| s.completion_date_time())),
            opt(st.and_then(|s| s.total_execution_time_in_millis())),
            opt(st.and_then(|s| s.query_queue_time_in_millis())),
            opt(st.and_then(|s| s.data_scanned_in_bytes())),
            st.and_then(|s| s.dpu_count()).map_or(Json::Null, |d| json!(d)),
            error.map_or(Json::Null, |e| json!(truncate(e))),
            json!(truncate(q.query().unwrap_or(""))),
        ]);
    }
    snap.tables.push(t);
}

impl AthenaSession {
    pub(crate) async fn snapshot(&mut self) -> Result<MonitorSnapshot> {
        let mut snap = MonitorSnapshot::default();
        snap.info.push(("Región".into(), self.region.clone()));
        snap.info.push(("Workgroup".into(), self.workgroup.clone()));
        snap.info.push(("Catálogo".into(), self.catalog.clone()));

        // The workgroup's settings.
        match self.client.get_work_group().work_group(&self.workgroup).send().await {
            Ok(wg) => {
                if let Some(w) = wg.work_group() {
                    if let Some(s) = w.state() {
                        snap.info.push(("Estado del workgroup".into(), s.as_str().into()));
                    }
                    if let Some(c) = w.configuration() {
                        if let Some(v) = c.engine_version().and_then(|e| e.effective_engine_version().or(e.selected_engine_version())) {
                            snap.info.push(("Versión del motor".into(), v.into()));
                        }
                        if let Some(b) = c.bytes_scanned_cutoff_per_query() {
                            snap.info.push(("Límite de datos por consulta".into(), format!("{b} bytes")));
                        }
                        if let Some(o) = c.result_configuration().and_then(|r| r.output_location()) {
                            snap.info.push(("Resultados en".into(), o.into()));
                        }
                        if let Some(p) = c.publish_cloud_watch_metrics_enabled() {
                            snap.info.push(("Métricas en CloudWatch".into(), if p { "sí" } else { "no" }.into()));
                        }
                    }
                }
            }
            Err(e) => snap.notes.push(format!("No se pudo leer la configuración del workgroup: {}", err(e))),
        }

        // The latest executions of the workgroup.
        let ids = self.client.list_query_executions().work_group(&self.workgroup).max_results(BATCH).send().await.map_err(err)?;
        let ids = ids.query_execution_ids().to_vec();
        let list = if ids.is_empty() {
            Vec::new()
        } else {
            self.client.batch_get_query_execution().set_query_execution_ids(Some(ids)).send().await.map_err(err)?.query_executions().to_vec()
        };
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        executions(&list, now, &mut snap);

        // Every workgroup, and provisioned capacity.
        match self.client.list_work_groups().max_results(50).send().await {
            Ok(r) => {
                let mut t = MonitorTable::new("workgroups", "Workgroups", &["Nombre", "Estado", "Versión del motor", "Descripción"]);
                for w in r.work_groups() {
                    t.rows.push(vec![
                        json!(w.name().unwrap_or("")),
                        json!(w.state().map(|s| s.as_str()).unwrap_or("")),
                        json!(w.engine_version().and_then(|e| e.effective_engine_version()).unwrap_or("")),
                        json!(w.description().unwrap_or("")),
                    ]);
                }
                snap.tables.push(t);
            }
            Err(e) => snap.notes.push(format!("No se pudieron listar los workgroups: {}", err(e))),
        }
        match self.client.list_capacity_reservations().send().await {
            Ok(r) if !r.capacity_reservations().is_empty() => {
                let (mut alloc, mut target) = (0.0, 0.0);
                let mut t = MonitorTable::new("capacity", "Reservas de capacidad", &["Nombre", "Estado", "DPU objetivo", "DPU asignadas"]);
                for c in r.capacity_reservations() {
                    alloc += f64::from(c.allocated_dpus());
                    target += f64::from(c.target_dpus());
                    t.rows.push(vec![json!(c.name()), json!(c.status().as_str()), json!(c.target_dpus()), json!(c.allocated_dpus())]);
                }
                snap.metrics.push(Metric::new("dpus", "DPU asignadas", "Capacidad", MetricUnit::Count, Some(alloc)).max(Some(target)));
                snap.tables.push(t);
            }
            Ok(_) => {}
            Err(e) => tracing::debug!("athena capacity reservations: {}", err(e)),
        }

        snap.notes.push(
            "Athena es un servicio sin servidor: no expone CPU, memoria ni conexiones; el consumo se mide en datos escaneados (o DPU con capacidad reservada)."
                .into(),
        );
        snap.notes.push(format!(
            "Las consultas en curso y recientes salen de las últimas {BATCH} ejecuciones del workgroup «{}».",
            self.workgroup
        ));
        Ok(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_athena::types::{QueryExecutionStatistics, QueryExecutionStatus, ResultReuseInformation};

    fn exec(id: &str, st: QueryExecutionState, done_at: Option<i64>, scanned: i64, reused: bool) -> QueryExecution {
        let mut status = QueryExecutionStatus::builder().state(st.clone()).submission_date_time(DateTime::from_secs(1_700_000_000));
        if let Some(t) = done_at {
            status = status.completion_date_time(DateTime::from_secs(t));
        }
        if st == QueryExecutionState::Failed {
            status = status.state_change_reason("SYNTAX_ERROR");
        }
        QueryExecution::builder()
            .query_execution_id(id)
            .query("SELECT 1")
            .status(status.build())
            .statistics(
                QueryExecutionStatistics::builder()
                    .data_scanned_in_bytes(scanned)
                    .engine_execution_time_in_millis(100)
                    .query_queue_time_in_millis(20)
                    .result_reuse_information(ResultReuseInformation::builder().reused_previous_result(reused).build())
                    .build(),
            )
            .build()
    }

    #[test]
    fn executions_into_metrics() {
        let now = 1_700_000_600.0;
        let list = vec![
            exec("r", QueryExecutionState::Running, None, 0, false),
            exec("q", QueryExecutionState::Queued, None, 0, false),
            exec("a", QueryExecutionState::Succeeded, Some(1_700_000_500), 1000, true),
            exec("b", QueryExecutionState::Failed, Some(1_700_000_550), 24, false),
            // Older than 15 minutes: left out.
            exec("old", QueryExecutionState::Succeeded, Some(1_699_990_000), 99_999, false),
        ];
        let mut snap = MonitorSnapshot::default();
        executions(&list, now, &mut snap);
        let v = |k: &str| snap.metrics.iter().find(|m| m.key == k).and_then(|m| m.value);
        assert_eq!(v("active_sessions"), Some(1.0));
        assert_eq!(v("queued_queries"), Some(1.0));
        assert_eq!(v("recent_queries"), Some(2.0));
        assert_eq!(v("recent_failed"), Some(1.0));
        assert_eq!(v("bytes_scanned"), Some(1024.0));
        assert_eq!(v("engine_time"), Some(200.0));
        assert_eq!(v("queue_time"), Some(20.0));
        assert_eq!(v("cache_hit"), Some(50.0));
        let running = snap.tables.iter().find(|t| t.key == "queries").unwrap();
        assert_eq!(running.rows.len(), 2);
        let recent = snap.tables.iter().find(|t| t.key == "recent_queries").unwrap();
        assert_eq!(recent.rows.len(), 2);
        assert_eq!(recent.rows[1][8], json!("SYNTAX_ERROR"));
        assert_eq!(recent.rows[0][3], json!("2023-11-14 22:21:40"));
    }
}
