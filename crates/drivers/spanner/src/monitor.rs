//! `Session::monitor` for Cloud Spanner. The database reports its own
//! activity in the `SPANNER_SYS` tables: queries running now
//! (`OLDEST_ACTIVE_QUERIES`), per-minute query, transaction and lock
//! statistics, and hourly table sizes. The instance's CPU and storage live
//! in Cloud Monitoring, read with the same credentials when the login may.
//! Unqualified upper-case names work in both dialects (PostgreSQL folds them
//! to `spanner_sys.…`).

use crate::{Json, SpannerSession, API};
use dbine_driver::monitor::num;
use dbine_driver::{Metric, MetricUnit, MonitorSnapshot, MonitorTable, Result};
use serde_json::json;
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const MONITORING: &str = "https://monitoring.googleapis.com";
const MAX_SQL: usize = 2000;

fn truncate(s: &str) -> String {
    if s.chars().count() > MAX_SQL {
        s.chars().take(MAX_SQL).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

fn cell(v: &Option<String>) -> Json {
    v.as_ref().map_or(Json::Null, |s| json!(s))
}

fn cell_num(v: &Option<String>) -> Json {
    match v.as_deref().and_then(num) {
        Some(n) => json!(n),
        None => cell(v),
    }
}

fn f(v: &Option<String>) -> Option<f64> {
    v.as_deref().and_then(num)
}

/// RFC 3339 (UTC) of a Unix time, for Cloud Monitoring's interval.
pub(crate) fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The latest point of each series of an aggregated `timeSeries.list`
/// answer; `None` when there are none.
fn latest_points(v: &Json) -> Option<Vec<f64>> {
    let series = v.get("timeSeries")?.as_array()?;
    let mut points = Vec::new();
    for s in series {
        let p = s.get("points")?.as_array()?.first()?;
        points.push(
            p.pointer("/value/doubleValue")
                .and_then(Json::as_f64)
                .or_else(|| {
                    p.pointer("/value/int64Value")
                        .and_then(|x| x.as_str().and_then(|s| s.parse().ok()).or(x.as_f64()))
                })?,
        );
    }
    (!points.is_empty()).then_some(points)
}

/// The latest point of an aggregated `timeSeries.list` answer, summed
/// over its series.
pub(crate) fn latest_point(v: &Json) -> Option<f64> {
    latest_points(v).map(|p| p.iter().sum())
}

/// The highest latest point among the series (one per region when
/// grouped by location).
pub(crate) fn latest_max(v: &Json) -> Option<f64> {
    latest_points(v)?.into_iter().reduce(f64::max)
}

/// The headline figures, from what the snapshot read (keyed as in
/// [`SpannerSession::snapshot`]).
pub(crate) fn metrics(m: &HashMap<&str, f64>) -> Vec<Metric> {
    let g = |k: &str| m.get(k).copied();
    vec![
        Metric::new(
            "cpu",
            "CPU de la instancia",
            "CPU",
            MetricUnit::Percent,
            g("cpu").map(|v| (v * 10.0).round() / 10.0),
        )
        .help("Uso del CPU aprovisionado de la instancia (sus nodos o unidades de procesamiento), promedio del último minuto, de todas sus bases y con las tareas de sistema; en una configuración multirregión, el de la región más cargada. 100 % es toda la capacidad; si se mantiene alto, la instancia necesita más nodos o unidades de procesamiento.")
        .source(if g("cpu_summed").is_some() {
            "Cloud Monitoring spanner.googleapis.com/instance/cpu/utilization (ALIGN_MEAN 60 s, REDUCE_SUM over series) × 100"
        } else {
            "Cloud Monitoring spanner.googleapis.com/instance/cpu/utilization (ALIGN_MEAN 60 s, REDUCE_SUM by resource.label.location, the highest region) × 100"
        })
        .max(Some(100.0)),
        Metric::new(
            "query_cpu",
            "Tiempo de CPU de consultas (último minuto)",
            "CPU",
            MetricUnit::Seconds,
            g("q_cpu"),
        )
        .help("Segundos de CPU que gastaron las consultas de esta base en el último minuto completo: 60 s equivalen a un núcleo ocupado todo el minuto. Compará con el CPU de la instancia para ver cuánto de la carga es de estas consultas.")
        .source("SPANNER_SYS.QUERY_STATS_TOTAL_MINUTE (latest interval): AVG_CPU_SECONDS × EXECUTION_COUNT"),
        Metric::new(
            "active_sessions",
            "Consultas activas",
            "Conexiones",
            MetricUnit::Count,
            g("active"),
        )
        .help("Consultas de esta base que se están ejecutando ahora, sin la del monitor.")
        .source("SPANNER_SYS.ACTIVE_QUERIES_SUMMARY: ACTIVE_COUNT, minus the monitor's own query"),
        Metric::new(
            "slow_queries",
            "Consultas activas hace más de 10 s",
            "Conexiones",
            MetricUnit::Count,
            g("older_10s"),
        )
        .help("Consultas que llevan más de 10 segundos ejecutándose. Si se acumulan, mirá las más viejas en la tabla de consultas en curso.")
        .source("SPANNER_SYS.ACTIVE_QUERIES_SUMMARY: COUNT_OLDER_THAN_10S"),
        Metric::new(
            "queries_minute",
            "Consultas (último minuto)",
            "Actividad",
            MetricUnit::Count,
            g("q_count"),
        )
        .help("Consultas que se ejecutaron en esta base en el último minuto completo.")
        .source("SPANNER_SYS.QUERY_STATS_TOTAL_MINUTE (latest interval): EXECUTION_COUNT"),
        Metric::new(
            "query_latency",
            "Latencia media de consultas",
            "Actividad",
            MetricUnit::Millis,
            g("q_latency"),
        )
        .help("Latencia media de las consultas del último minuto completo, desde que llegan hasta que devuelven el último resultado.")
        .source("SPANNER_SYS.QUERY_STATS_TOTAL_MINUTE (latest interval): AVG_LATENCY_SECONDS × 1000"),
        Metric::new(
            "rows_scanned_minute",
            "Filas escaneadas (último minuto)",
            "Actividad",
            MetricUnit::Count,
            g("q_scanned"),
        )
        .help("Filas que recorrieron las consultas del último minuto completo. Si es mucho más que las devueltas, faltan índices o los filtros no usan la clave primaria.")
        .source("SPANNER_SYS.QUERY_STATS_TOTAL_MINUTE (latest interval): AVG_ROWS_SCANNED × EXECUTION_COUNT"),
        Metric::new(
            "rows_returned_minute",
            "Filas devueltas (último minuto)",
            "Actividad",
            MetricUnit::Count,
            g("q_rows"),
        )
        .help("Filas que devolvieron las consultas del último minuto completo.")
        .source("SPANNER_SYS.QUERY_STATS_TOTAL_MINUTE (latest interval): AVG_ROWS × EXECUTION_COUNT"),
        Metric::new(
            "transactions_minute",
            "Commits (último minuto)",
            "Actividad",
            MetricUnit::Count,
            g("t_commits"),
        )
        .help("Intentos de commit de transacciones de lectura y escritura en el último minuto completo, incluidos los que se abortaron.")
        .source("SPANNER_SYS.TXN_STATS_TOTAL_MINUTE (latest interval): COMMIT_ATTEMPT_COUNT"),
        Metric::new(
            "aborts_minute",
            "Transacciones abortadas (último minuto)",
            "Actividad",
            MetricUnit::Count,
            g("t_aborts"),
        )
        .help("Commits abortados en el último minuto completo, casi siempre por conflictos de bloqueo con otra transacción; el cliente los reintenta. Si son una parte grande de los intentos, hay transacciones que compiten por las mismas filas.")
        .source("SPANNER_SYS.TXN_STATS_TOTAL_MINUTE (latest interval): COMMIT_ABORT_COUNT"),
        Metric::new(
            "commit_latency",
            "Latencia media de commit",
            "Actividad",
            MetricUnit::Millis,
            g("t_latency"),
        )
        .help("Latencia media de commit de las transacciones del último minuto completo. Sube con las esperas por bloqueos y, en configuraciones multirregión, con la distancia entre réplicas.")
        .source("SPANNER_SYS.TXN_STATS_TOTAL_MINUTE (latest interval): AVG_COMMIT_LATENCY_SECONDS × 1000"),
        Metric::new(
            "lock_wait",
            "Espera por bloqueos (último minuto)",
            "Bloqueos",
            MetricUnit::Seconds,
            g("lock_wait"),
        )
        .help("Tiempo que las transacciones pasaron esperando bloqueos en el último minuto completo, sumado entre todas. Las filas más disputadas están en la tabla de esperas por bloqueos.")
        .source("SPANNER_SYS.LOCK_STATS_TOTAL_MINUTE (latest interval): TOTAL_LOCK_WAIT_SECONDS"),
        Metric::new(
            "storage_used",
            "Espacio usado por la instancia",
            "Almacenamiento",
            MetricUnit::Bytes,
            g("inst_bytes"),
        )
        .help("Espacio que ocupan todas las bases de la instancia, sobre el límite que permite su capacidad (nodos o unidades de procesamiento).")
        .source("Cloud Monitoring spanner.googleapis.com/instance/storage/used_bytes (REDUCE_SUM); max: instance/storage/limit_bytes")
        .max(g("inst_limit")),
        Metric::new(
            "database_size",
            "Tamaño de la base (última hora)",
            "Almacenamiento",
            MetricUnit::Bytes,
            g("db_bytes"),
        )
        .help("Tamaño de las tablas de esta base, sumado, de la última medición horaria.")
        .source("SPANNER_SYS.TABLE_SIZES_STATS_1HOUR (latest interval): USED_BYTES (sum)"),
    ]
}

impl SpannerSession {
    /// The last minutes of a Cloud Monitoring metric of this instance, its
    /// series summed (within each `group_by` value, when given).
    async fn monitoring(
        &self,
        project: &str,
        instance_id: &str,
        metric: &str,
        group_by: Option<&str>,
    ) -> Result<Json> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let filter = format!("metric.type=\"spanner.googleapis.com/{metric}\" AND resource.labels.instance_id=\"{instance_id}\"");
        let (start, end) = (rfc3339(now.saturating_sub(300)), rfc3339(now));
        let mut params = vec![
            ("filter", filter.as_str()),
            ("interval.startTime", &start),
            ("interval.endTime", &end),
            ("aggregation.alignmentPeriod", "60s"),
            ("aggregation.perSeriesAligner", "ALIGN_MEAN"),
            ("aggregation.crossSeriesReducer", "REDUCE_SUM"),
        ];
        if let Some(g) = group_by {
            params.push(("aggregation.groupByFields", g));
        }
        let url = reqwest::Url::parse_with_params(
            &format!("{MONITORING}/v3/projects/{project}/timeSeries"),
            &params,
        )
        .map_err(|e| dbine_driver::Error::Query(e.to_string()))?;
        self.api.send(self.api.http.get(url)).await
    }

    /// A SPANNER_SYS query; on failure, a note and no rows.
    async fn sys(
        &mut self,
        what: &str,
        sql: &str,
        failures: &mut Vec<String>,
    ) -> Vec<Vec<Option<String>>> {
        match self.text_rows(sql, &[]).await {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("{what}: {e}"));
                Vec::new()
            }
        }
    }

    pub(crate) async fn snapshot(&mut self) -> Result<MonitorSnapshot> {
        let mut snap = MonitorSnapshot::default();
        let mut m: HashMap<&'static str, f64> = HashMap::new();
        let emulated = !self.api.base.starts_with(API);

        // Instance and database.
        let inst = self.api.get(&self.instance).await?;
        let s = |v: &Json, k: &str| v.get(k).and_then(Json::as_str).map(str::to_string);
        snap.info.push((
            "Instancia".into(),
            s(&inst, "displayName").unwrap_or_else(|| self.instance.clone()),
        ));
        if let Some(c) = s(&inst, "config") {
            snap.info.push((
                "Configuración".into(),
                c.rsplit('/').next().unwrap_or(&c).to_string(),
            ));
        }
        if let Some(e) = s(&inst, "edition") {
            snap.info.push(("Edición".into(), e));
        }
        let pu = inst.get("processingUnits").and_then(Json::as_f64);
        if let Some(n) = inst.get("nodeCount").and_then(Json::as_f64) {
            snap.info.push(("Nodos".into(), n.to_string()));
        }
        if let Some(p) = pu {
            snap.info
                .push(("Unidades de procesamiento".into(), p.to_string()));
        }
        if let Ok(db) = self.api.get(&self.database).await {
            if let Some(d) = s(&db, "databaseDialect") {
                snap.info.push((
                    "Dialecto".into(),
                    if d == "POSTGRESQL" {
                        "PostgreSQL".into()
                    } else {
                        "GoogleSQL".into()
                    },
                ));
            }
            if let Some(r) = s(&db, "versionRetentionPeriod") {
                snap.info.push(("Retención de versiones".into(), r));
            }
            if let Some(st) = s(&db, "state") {
                snap.info.push(("Estado de la base".into(), st));
            }
        }
        if emulated {
            snap.info.push(("Servidor".into(), "emulador".into()));
        }

        let mut failures = Vec::new();

        // Queries running now.
        let summary = self
            .sys(
                "ACTIVE_QUERIES_SUMMARY",
                "SELECT ACTIVE_COUNT, COUNT_OLDER_THAN_1S, COUNT_OLDER_THAN_10S, COUNT_OLDER_THAN_100S FROM SPANNER_SYS.ACTIVE_QUERIES_SUMMARY",
                &mut failures,
            )
            .await;
        if let Some(r) = summary.first() {
            for (k, v) in ["active", "older_1s", "older_10s", "older_100s"]
                .into_iter()
                .zip(r)
            {
                if let Some(v) = f(v) {
                    m.insert(k, v);
                }
            }
        }
        let active = self
            .sys(
                "OLDEST_ACTIVE_QUERIES",
                "SELECT QUERY_ID, SESSION_ID, START_TIME, CLIENT_IP_ADDRESS, USER_AGENT_HEADER, PRIORITY, TRANSACTION_TYPE, TEXT
                 FROM SPANNER_SYS.OLDEST_ACTIVE_QUERIES ORDER BY START_TIME LIMIT 200",
                &mut failures,
            )
            .await;
        let mut t = MonitorTable::new(
            "queries",
            "Consultas en curso",
            &[
                "Id",
                "Sesión",
                "Inicio",
                "Cliente",
                "Agente",
                "Prioridad",
                "Transacción",
                "Consulta",
            ],
        );
        for r in active.iter().filter(|r| r.len() == 8) {
            // Leave out this very query.
            if r[7]
                .as_deref()
                .is_some_and(|q| q.contains("SPANNER_SYS.OLDEST_ACTIVE_QUERIES"))
            {
                m.entry("active").and_modify(|v| *v = (*v - 1.0).max(0.0));
                continue;
            }
            let session = r[1]
                .as_deref()
                .map(|s| s.rsplit('/').next().unwrap_or(s).to_string());
            t.rows.push(vec![
                cell(&r[0]),
                cell(&session),
                cell(&r[2]),
                cell(&r[3]),
                cell(&r[4]),
                cell(&r[5]),
                cell(&r[6]),
                json!(truncate(r[7].as_deref().unwrap_or(""))),
            ]);
        }
        snap.tables.push(t);

        // The last complete minute.
        let total = self
            .sys(
                "QUERY_STATS_TOTAL_MINUTE",
                "SELECT EXECUTION_COUNT, AVG_LATENCY_SECONDS, AVG_CPU_SECONDS, AVG_ROWS_SCANNED, AVG_ROWS, AVG_BYTES
                 FROM SPANNER_SYS.QUERY_STATS_TOTAL_MINUTE ORDER BY INTERVAL_END DESC LIMIT 1",
                &mut failures,
            )
            .await;
        if let Some(r) = total.first().filter(|r| r.len() == 6) {
            let n = f(&r[0]).unwrap_or(0.0);
            m.insert("q_count", n);
            if let Some(l) = f(&r[1]) {
                m.insert("q_latency", l * 1000.0);
            }
            if let Some(c) = f(&r[2]) {
                // CPU seconds spent in the minute. Not a percentage: the
                // instance's cores aren't known, so it can't be a share of
                // them (the instance's own CPU share is `cpu`).
                m.insert("q_cpu", c * n);
            }
            if let Some(x) = f(&r[3]) {
                m.insert("q_scanned", x * n);
            }
            if let Some(x) = f(&r[4]) {
                m.insert("q_rows", x * n);
            }
            if let Some(x) = f(&r[5]) {
                m.insert("q_bytes", x * n);
            }
        }
        let txn = self
            .sys(
                "TXN_STATS_TOTAL_MINUTE",
                "SELECT COMMIT_ATTEMPT_COUNT, COMMIT_ABORT_COUNT, AVG_COMMIT_LATENCY_SECONDS
                 FROM SPANNER_SYS.TXN_STATS_TOTAL_MINUTE ORDER BY INTERVAL_END DESC LIMIT 1",
                &mut failures,
            )
            .await;
        if let Some(r) = txn.first().filter(|r| r.len() == 3) {
            for (k, v) in ["t_commits", "t_aborts", "t_latency"].into_iter().zip(r) {
                if let Some(v) = f(v) {
                    m.insert(k, if k == "t_latency" { v * 1000.0 } else { v });
                }
            }
        }
        let locks = self
            .sys(
                "LOCK_STATS_TOTAL_MINUTE",
                "SELECT TOTAL_LOCK_WAIT_SECONDS FROM SPANNER_SYS.LOCK_STATS_TOTAL_MINUTE ORDER BY INTERVAL_END DESC LIMIT 1",
                &mut failures,
            )
            .await;
        if let Some(v) = locks.first().and_then(|r| r.first()).and_then(f) {
            m.insert("lock_wait", v);
        }

        // The last minute's heaviest queries, transactions and lock waits.
        let top = self
            .sys(
                "QUERY_STATS_TOP_MINUTE",
                "SELECT TEXT, EXECUTION_COUNT, AVG_LATENCY_SECONDS, AVG_CPU_SECONDS, AVG_ROWS_SCANNED, AVG_BYTES
                 FROM SPANNER_SYS.QUERY_STATS_TOP_MINUTE
                 WHERE INTERVAL_END = (SELECT MAX(INTERVAL_END) FROM SPANNER_SYS.QUERY_STATS_TOP_MINUTE)
                 ORDER BY AVG_CPU_SECONDS * EXECUTION_COUNT DESC LIMIT 20",
                &mut failures,
            )
            .await;
        let mut t = MonitorTable::new(
            "top_queries",
            "Consultas más costosas (último minuto)",
            &[
                "Consulta",
                "Ejecuciones",
                "Latencia prom. (s)",
                "CPU prom. (s)",
                "Filas escaneadas prom.",
                "Bytes prom.",
            ],
        );
        for r in top.iter().filter(|r| r.len() == 6) {
            t.rows.push(vec![
                json!(truncate(r[0].as_deref().unwrap_or(""))),
                cell_num(&r[1]),
                cell_num(&r[2]),
                cell_num(&r[3]),
                cell_num(&r[4]),
                cell_num(&r[5]),
            ]);
        }
        snap.tables.push(t);
        let top_txn = self
            .sys(
                "TXN_STATS_TOP_MINUTE",
                "SELECT FPRINT, READ_COLUMNS, WRITE_CONSTRUCTIVE_COLUMNS, COMMIT_ATTEMPT_COUNT, COMMIT_ABORT_COUNT, AVG_COMMIT_LATENCY_SECONDS
                 FROM SPANNER_SYS.TXN_STATS_TOP_MINUTE
                 WHERE INTERVAL_END = (SELECT MAX(INTERVAL_END) FROM SPANNER_SYS.TXN_STATS_TOP_MINUTE)
                 ORDER BY COMMIT_ATTEMPT_COUNT DESC LIMIT 20",
                &mut failures,
            )
            .await;
        let mut t = MonitorTable::new(
            "transactions",
            "Transacciones principales (último minuto)",
            &[
                "Huella",
                "Columnas leídas",
                "Columnas escritas",
                "Intentos de commit",
                "Abortos",
                "Latencia prom. (s)",
            ],
        );
        for r in top_txn.iter().filter(|r| r.len() == 6) {
            t.rows.push(vec![
                cell(&r[0]),
                cell(&r[1]),
                cell(&r[2]),
                cell_num(&r[3]),
                cell_num(&r[4]),
                cell_num(&r[5]),
            ]);
        }
        snap.tables.push(t);
        let top_locks = self
            .sys(
                "LOCK_STATS_TOP_MINUTE",
                "SELECT ROW_RANGE_START_KEY, LOCK_WAIT_SECONDS, SAMPLE_LOCK_REQUESTS FROM SPANNER_SYS.LOCK_STATS_TOP_MINUTE
                 WHERE INTERVAL_END = (SELECT MAX(INTERVAL_END) FROM SPANNER_SYS.LOCK_STATS_TOP_MINUTE)
                 ORDER BY LOCK_WAIT_SECONDS DESC LIMIT 20",
                &mut failures,
            )
            .await;
        let mut t = MonitorTable::new(
            "locks",
            "Esperas por bloqueos (último minuto)",
            &["Clave inicial", "Espera (s)", "Solicitudes de muestra"],
        );
        for r in top_locks.iter().filter(|r| r.len() == 3) {
            t.rows.push(vec![
                cell(&r[0]),
                cell_num(&r[1]),
                json!(truncate(r[2].as_deref().unwrap_or(""))),
            ]);
        }
        snap.tables.push(t);

        // Table sizes (hourly).
        let sizes = self
            .sys(
                "TABLE_SIZES_STATS_1HOUR",
                "SELECT TABLE_NAME, USED_BYTES FROM SPANNER_SYS.TABLE_SIZES_STATS_1HOUR
                 WHERE INTERVAL_END = (SELECT MAX(INTERVAL_END) FROM SPANNER_SYS.TABLE_SIZES_STATS_1HOUR)
                 ORDER BY USED_BYTES DESC",
                &mut failures,
            )
            .await;
        if !sizes.is_empty() {
            m.insert(
                "db_bytes",
                sizes
                    .iter()
                    .filter_map(|r| r.get(1).and_then(f))
                    .fold(0.0, |a, b| a + b),
            );
            let mut t = MonitorTable::new(
                "top_objects",
                "Tablas más grandes",
                &["Tabla", "Bytes usados"],
            );
            for r in sizes.iter().filter(|r| r.len() == 2).take(20) {
                t.rows.push(vec![cell(&r[0]), cell_num(&r[1])]);
            }
            snap.tables.push(t);
        }

        // The instance's CPU and storage, from Cloud Monitoring.
        if !emulated {
            let mut parts = self.instance.split('/');
            let (project, instance_id) = (
                parts.nth(1).unwrap_or("").to_string(),
                parts.nth(1).unwrap_or("").to_string(),
            );
            // A multi-region instance reports each region's CPU: summing
            // them would pass 100 %, so the busiest region counts. If the
            // grouping is refused, the sum (the same with one region).
            const CPU: &str = "instance/cpu/utilization";
            let cpu = match self
                .monitoring(&project, &instance_id, CPU, Some("resource.label.location"))
                .await
            {
                Ok(v) => Ok(latest_max(&v)),
                Err(e) => {
                    tracing::debug!("spanner cpu by location: {e}");
                    m.insert("cpu_summed", 1.0);
                    self.monitoring(&project, &instance_id, CPU, None)
                        .await
                        .map(|v| latest_point(&v))
                }
            };
            match cpu {
                Ok(v) => {
                    if let Some(v) = v {
                        m.insert("cpu", v * 100.0);
                    }
                    if let Ok(Some(v)) = self.monitoring(&project, &instance_id, "instance/storage/used_bytes", None).await.map(|v| latest_point(&v)) {
                        m.insert("inst_bytes", v);
                    }
                    if let Ok(Some(v)) = self.monitoring(&project, &instance_id, "instance/storage/limit_bytes", None).await.map(|v| latest_point(&v)) {
                        m.insert("inst_limit", v);
                    }
                }
                Err(e) => snap.notes.push(format!(
                    "El CPU y el almacenamiento de la instancia están en Cloud Monitoring, y esta cuenta no puede leerlos (hace falta monitoring.timeSeries.list): {e}"
                )),
            }
        } else {
            snap.notes.push(
                "El emulador no tiene Cloud Monitoring: sin CPU ni almacenamiento de la instancia."
                    .into(),
            );
        }

        snap.metrics = metrics(&m);

        if !failures.is_empty() {
            snap.notes.push(if emulated {
                "El emulador no implementa las tablas SPANNER_SYS: sin consultas activas ni estadísticas.".into()
            } else {
                format!("No se pudieron leer algunas tablas SPANNER_SYS: {}", failures.join("; "))
            });
        }
        snap.notes.push(
            "Las estadísticas de SPANNER_SYS son por minuto (llegan con hasta un minuto de atraso) y los tamaños de tabla, por hora.".into(),
        );
        snap.notes.push("Spanner no expone memoria ni conexiones: la capacidad se mide en nodos o unidades de procesamiento.".into());
        Ok(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn monitoring_points() {
        let v = json!({"timeSeries": [
            {"points": [{"value": {"doubleValue": 0.25}}, {"value": {"doubleValue": 0.1}}]},
            {"points": [{"value": {"doubleValue": 0.05}}]}
        ]});
        assert!((latest_point(&v).unwrap() - 0.3).abs() < 1e-9);
        // Grouped by region: the busiest one, not the sum.
        assert_eq!(latest_max(&v), Some(0.25));
        assert_eq!(latest_max(&json!({"timeSeries": []})), None);
        assert_eq!(
            latest_point(&json!({"timeSeries": [{"points": [{"value": {"int64Value": "1024"}}]}]})),
            Some(1024.0)
        );
        assert_eq!(latest_point(&json!({})), None);
    }

    #[test]
    fn every_metric_says_what_it_is() {
        let m: HashMap<&str, f64> = [("cpu", 42.04), ("q_cpu", 90.0)].into_iter().collect();
        let list = metrics(&m);
        assert_eq!(list.len(), 14);
        let cpu = |k: &str| list.iter().find(|x| x.key == k).unwrap();
        assert_eq!(cpu("cpu").value, Some(42.0));
        // Query CPU is CPU seconds, not a share of an unknown core count.
        assert_eq!(cpu("query_cpu").unit, MetricUnit::Seconds);
        assert_eq!(cpu("query_cpu").value, Some(90.0));
        assert!(cpu("cpu")
            .source
            .as_deref()
            .unwrap()
            .contains("by resource.label.location"));
        let summed: HashMap<&str, f64> = [("cpu", 42.0), ("cpu_summed", 1.0)].into_iter().collect();
        let list2 = metrics(&summed);
        let c = list2.iter().find(|x| x.key == "cpu").unwrap();
        assert!(c.source.as_deref().unwrap().contains("over series"));
        for m in &list {
            assert!(
                m.help.as_deref().is_some_and(|h| !h.is_empty())
                    && m.source.as_deref().is_some_and(|s| !s.is_empty()),
                "{} has no help or source",
                m.key
            );
        }
    }
}
