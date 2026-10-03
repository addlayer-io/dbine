//! Server monitor for Snowflake. There is no CPU or memory to report (the
//! service doesn't expose them); what the SQL API gives is warehouses
//! (state, running and queued queries), running queries and recent
//! history, warehouse load and credits, locks and storage.
//!
//! `SHOW` commands run in the cloud services layer and are asked every
//! time. Everything else needs a running warehouse: those parts are only
//! refreshed when the session's warehouse is already started (the monitor
//! never resumes one) and are cached for a while, so the dashboard doesn't
//! keep a warehouse awake or spend credits every few seconds.

use dbine_driver::monitor::{num, Metric, MetricUnit as U, MonitorSnapshot, MonitorTable};
use serde_json::Value;
use std::time::{Duration, Instant};

/// QUERY_TAG of the monitor's own statements, to leave them out of the history.
pub const TAG: &str = "dbine-monitor";
const MAX_ROWS: usize = 200;
const MAX_TEXT: usize = 2000;
const HISTORY_TTL: Duration = Duration::from_secs(20);
const LOAD_TTL: Duration = Duration::from_secs(5 * 60);
const STORAGE_TTL: Duration = Duration::from_secs(15 * 60);

/// A result set by column name (lowercase), every value as text.
#[derive(Debug, Clone, Default)]
pub struct Set {
    pub cols: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

impl Set {
    fn idx(&self, name: &str) -> Option<usize> {
        self.cols.iter().position(|c| c.eq_ignore_ascii_case(name))
    }
    fn get(&self, row: usize, name: &str) -> Option<&str> {
        let i = self.idx(name)?;
        self.rows.get(row)?.get(i)?.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }
    fn num(&self, row: usize, name: &str) -> Option<f64> {
        self.get(row, name).and_then(num)
    }
    fn text(&self, name: &str) -> Option<String> {
        self.get(0, name).map(str::to_string)
    }
    fn sum(&self, name: &str) -> Option<f64> {
        let v: Vec<f64> = (0..self.rows.len()).filter_map(|r| self.num(r, name)).collect();
        (!v.is_empty()).then(|| v.iter().sum())
    }
    fn count_where(&self, name: &str, pred: impl Fn(&str) -> bool) -> f64 {
        (0..self.rows.len()).filter(|&r| self.get(r, name).is_some_and(&pred)).count() as f64
    }
    fn select(&self, rows: &[usize]) -> Set {
        Set { cols: self.cols.clone(), rows: rows.iter().filter_map(|&r| self.rows.get(r).cloned()).collect() }
    }
}

fn cell(v: Option<&str>) -> Value {
    let Some(s) = v.map(str::trim).filter(|s| !s.is_empty()) else { return Value::Null };
    if let Ok(i) = s.parse::<i64>() {
        return i.into();
    }
    if s.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-') {
        if let Some(n) = s.parse::<f64>().ok().and_then(serde_json::Number::from_f64) {
            return Value::Number(n);
        }
    }
    if s.chars().count() > MAX_TEXT {
        format!("{}…", s.chars().take(MAX_TEXT).collect::<String>()).into()
    } else {
        s.to_string().into()
    }
}

fn table(key: &str, title: &str, cols: &[(&str, &str)], set: &Set) -> Option<MonitorTable> {
    let present: Vec<&(&str, &str)> = cols.iter().filter(|(c, _)| set.idx(c).is_some()).collect();
    if present.is_empty() {
        return None;
    }
    let labels: Vec<&str> = present.iter().map(|(_, l)| *l).collect();
    let mut t = MonitorTable::new(key, title, &labels);
    for r in 0..set.rows.len().min(MAX_ROWS) {
        t.rows.push(present.iter().map(|(c, _)| cell(set.get(r, c))).collect());
    }
    Some(t)
}

/// Parts that need a warehouse, with when they were read.
#[derive(Default)]
pub struct Cache {
    history: Option<(Instant, Result<Set, String>)>,
    load: Option<(Instant, Result<Set, String>)>,
    metering: Option<(Instant, Result<Set, String>)>,
    storage: Option<(Instant, Result<Set, String>)>,
    tables: Option<(Instant, Result<Set, String>)>,
}

fn fresh(slot: &Option<(Instant, Result<Set, String>)>, ttl: Duration) -> bool {
    slot.as_ref().is_some_and(|(t, _)| t.elapsed() < ttl)
}

/// What the monitor asks of the session.
#[dbine_driver::async_trait]
pub trait Runner {
    async fn rows(&self, sql: &str) -> dbine_driver::Result<Set>;
    fn warehouse(&self) -> Option<String>;
    fn database(&self) -> Option<String>;
}

pub(crate) fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub async fn snapshot(r: &(dyn Runner + Sync), cache: &mut Cache) -> MonitorSnapshot {
    let mut s = MonitorSnapshot::default();
    let is = match r.database() {
        Some(db) => format!("{}.INFORMATION_SCHEMA", quote(&db)),
        None => "SNOWFLAKE.INFORMATION_SCHEMA".to_string(),
    };

    if let Ok(i) = r
        .rows("SELECT CURRENT_VERSION() AS version, CURRENT_ACCOUNT() AS account, CURRENT_REGION() AS region, CURRENT_ROLE() AS role")
        .await
    {
        for (label, col) in [("Versión", "version"), ("Cuenta", "account"), ("Región", "region"), ("Rol", "role")] {
            if let Some(v) = i.text(col) {
                s.info.push((label.into(), v));
            }
        }
    }

    // Warehouses: SHOW needs no warehouse.
    let mut current: Option<(String, bool)> = None;
    match r.rows("SHOW WAREHOUSES").await {
        Ok(w) => {
            let started = w.count_where("state", |v| v.eq_ignore_ascii_case("STARTED"));
            s.metrics.push(Metric::new("warehouses_started", "Warehouses encendidos", "Warehouses", U::Count, Some(started)).max(Some(w.rows.len() as f64)));
            s.metrics.push(Metric::new("active_sessions", "Consultas en ejecución", "Conexiones", U::Count, w.sum("running")));
            s.metrics.push(Metric::new("queued", "Consultas en cola", "Conexiones", U::Count, w.sum("queued")));
            let name = r.warehouse().or_else(|| {
                (0..w.rows.len()).find(|&i| w.get(i, "is_current") == Some("Y")).and_then(|i| w.get(i, "name").map(str::to_string))
            });
            if let Some(n) = name {
                let row = (0..w.rows.len()).find(|&i| w.get(i, "name").is_some_and(|x| x.eq_ignore_ascii_case(&n)));
                let on = row.and_then(|i| w.get(i, "state")).is_some_and(|v| v.eq_ignore_ascii_case("STARTED"));
                current = Some((n, on));
            }
            s.tables.extend(table(
                "warehouses",
                "Warehouses",
                &[
                    ("name", "Warehouse"),
                    ("state", "Estado"),
                    ("type", "Tipo"),
                    ("size", "Tamaño"),
                    ("running", "En ejecución"),
                    ("queued", "En cola"),
                    ("started_clusters", "Clusters encendidos"),
                    ("max_cluster_count", "Máx. clusters"),
                    ("auto_suspend", "Autosuspensión (s)"),
                ],
                &w,
            ));
        }
        Err(e) => s.notes.push(format!("Warehouses: no disponible ({e}).")),
    }
    match r.rows("SHOW LOCKS").await {
        Ok(l) => {
            s.metrics.push(Metric::new(
                "locks_waiting",
                "Bloqueos en espera",
                "Bloqueos",
                U::Count,
                Some(l.count_where("status", |v| v.eq_ignore_ascii_case("WAITING"))),
            ));
            s.tables.extend(table(
                "locks",
                "Bloqueos",
                &[
                    ("resource", "Recurso"),
                    ("type", "Tipo"),
                    ("transaction", "Transacción"),
                    ("status", "Estado"),
                    ("acquired_on", "Desde"),
                    ("query_id", "Consulta"),
                ],
                &l,
            ));
        }
        Err(e) => s.notes.push(format!("Bloqueos: no disponible ({e}).")),
    }

    // The rest needs a running warehouse.
    let can_run = match &current {
        Some((_, true)) => true,
        Some((n, false)) => {
            s.notes.push(format!(
                "El warehouse {n} está suspendido: el monitor no lo reanuda, así que el historial de consultas, la carga y el almacenamiento muestran la última lectura."
            ));
            false
        }
        None => {
            s.notes.push("Sin un warehouse en la conexión, solo se muestran los warehouses y los bloqueos.".into());
            false
        }
    };
    if can_run {
        if !fresh(&cache.history, HISTORY_TTL) {
            let sql = format!(
                "SELECT query_id, user_name, warehouse_name, execution_status, start_time, total_elapsed_time,
                        DATEDIFF('second', start_time, CURRENT_TIMESTAMP()) AS secs, bytes_scanned, rows_produced,
                        LEFT(query_text, 2000) AS query_text
                   FROM TABLE({is}.QUERY_HISTORY(RESULT_LIMIT => 10000))
                  WHERE COALESCE(query_tag, '') <> '{TAG}' AND start_time >= DATEADD('hour', -1, CURRENT_TIMESTAMP())
                  ORDER BY start_time DESC"
            );
            cache.history = Some((Instant::now(), r.rows(&sql).await.map_err(|e| e.to_string())));
        }
        if !fresh(&cache.load, LOAD_TTL) {
            let sql = format!(
                "SELECT warehouse_name, AVG(avg_running) AS running, AVG(avg_queued_load) AS queued,
                        AVG(avg_queued_provisioning) AS provisioning, AVG(avg_blocked) AS blocked
                   FROM TABLE({is}.WAREHOUSE_LOAD_HISTORY(DATE_RANGE_START => DATEADD('hour', -1, CURRENT_TIMESTAMP())))
                  GROUP BY 1 ORDER BY 1"
            );
            cache.load = Some((Instant::now(), r.rows(&sql).await.map_err(|e| e.to_string())));
            let sql = format!(
                "SELECT warehouse_name, SUM(credits_used) AS credits, SUM(credits_used_compute) AS compute,
                        SUM(credits_used_cloud_services) AS cloud
                   FROM TABLE({is}.WAREHOUSE_METERING_HISTORY(DATE_RANGE_START => DATEADD('day', -1, CURRENT_TIMESTAMP())))
                  GROUP BY 1 ORDER BY 2 DESC"
            );
            cache.metering = Some((Instant::now(), r.rows(&sql).await.map_err(|e| e.to_string())));
        }
        if !fresh(&cache.storage, STORAGE_TTL) {
            let sql = "SELECT * FROM SNOWFLAKE.ACCOUNT_USAGE.STORAGE_USAGE ORDER BY usage_date DESC LIMIT 1";
            cache.storage = Some((Instant::now(), r.rows(sql).await.map_err(|e| e.to_string())));
            if r.database().is_some() {
                let sql = format!(
                    "SELECT table_schema, table_name, row_count, bytes FROM {is}.TABLES
                      WHERE table_type = 'BASE TABLE' ORDER BY bytes DESC NULLS LAST LIMIT 20"
                );
                cache.tables = Some((Instant::now(), r.rows(&sql).await.map_err(|e| e.to_string())));
            }
        }
    }

    match cache.history.as_ref().map(|(_, r)| r) {
        Some(Ok(h)) => {
            let running: Vec<usize> = (0..h.rows.len())
                .filter(|&i| {
                    h.get(i, "execution_status")
                        .is_some_and(|v| matches!(v.to_ascii_uppercase().as_str(), "RUNNING" | "QUEUED" | "BLOCKED" | "RESUMING_WAREHOUSE"))
                })
                .collect();
            let done: Vec<usize> = (0..h.rows.len()).filter(|i| !running.contains(i)).collect();
            let finished = h.select(&done);
            s.metrics.push(Metric::new("queries_last_hour", "Consultas (última hora)", "Actividad", U::Count, Some(done.len() as f64)));
            s.metrics.push(Metric::new(
                "failed_last_hour",
                "Consultas fallidas (última hora)",
                "Actividad",
                U::Count,
                Some(finished.count_where("execution_status", |v| v.to_ascii_uppercase().starts_with("FAIL"))),
            ));
            s.metrics.push(Metric::new("bytes_scanned", "Bytes escaneados (última hora)", "Actividad", U::Bytes, finished.sum("bytes_scanned")));
            s.tables.extend(table(
                "queries",
                "Consultas en curso",
                &[
                    ("query_id", "ID"),
                    ("user_name", "Usuario"),
                    ("warehouse_name", "Warehouse"),
                    ("execution_status", "Estado"),
                    ("secs", "Duración (s)"),
                    ("query_text", "Consulta"),
                ],
                &h.select(&running),
            ));
            let recent: Vec<usize> = done.iter().copied().take(50).collect();
            s.tables.extend(table(
                "history",
                "Historial reciente",
                &[
                    ("query_id", "ID"),
                    ("user_name", "Usuario"),
                    ("warehouse_name", "Warehouse"),
                    ("execution_status", "Estado"),
                    ("start_time", "Inicio"),
                    ("total_elapsed_time", "Duración (ms)"),
                    ("bytes_scanned", "Bytes escaneados"),
                    ("rows_produced", "Filas"),
                    ("query_text", "Consulta"),
                ],
                &h.select(&recent),
            ));
            s.notes.push(
                "El historial muestra las consultas propias; las de otros usuarios necesitan el privilegio MONITOR sobre sus warehouses."
                    .into(),
            );
        }
        Some(Err(e)) => s.notes.push(format!("Historial de consultas: no disponible ({e}).")),
        None => {}
    }
    match cache.load.as_ref().map(|(_, r)| r) {
        Some(Ok(l)) => s.tables.extend(table(
            "warehouse_load",
            "Carga de los warehouses (promedio de la última hora)",
            &[
                ("warehouse_name", "Warehouse"),
                ("running", "En ejecución"),
                ("queued", "En cola por carga"),
                ("provisioning", "En cola por aprovisionamiento"),
                ("blocked", "Bloqueadas"),
            ],
            l,
        )),
        Some(Err(e)) => s.notes.push(format!("Carga de warehouses: no disponible ({e}).")),
        None => {}
    }
    match cache.metering.as_ref().map(|(_, r)| r) {
        Some(Ok(m)) => {
            s.metrics.push(Metric::new("credits_24h", "Créditos (últimas 24 h)", "Créditos", U::Count, m.sum("credits")));
            s.tables.extend(table(
                "credits",
                "Créditos por warehouse (últimas 24 h)",
                &[("warehouse_name", "Warehouse"), ("credits", "Créditos"), ("compute", "Cómputo"), ("cloud", "Servicios en la nube")],
                m,
            ));
        }
        Some(Err(e)) => s.notes.push(format!("Créditos: no disponible ({e}).")),
        None => {}
    }
    match cache.storage.as_ref().map(|(_, r)| r) {
        Some(Ok(st)) => {
            let total = ["storage_bytes", "stage_bytes", "failsafe_bytes", "hybrid_table_storage_bytes"]
                .iter()
                .filter_map(|c| st.num(0, c))
                .fold(None, |a: Option<f64>, v| Some(a.unwrap_or(0.0) + v));
            s.metrics.push(Metric::new("storage_used", "Espacio usado (cuenta)", "Almacenamiento", U::Bytes, total));
            if let Some(d) = st.text("usage_date") {
                s.info.push(("Almacenamiento al".into(), d));
            }
            s.notes.push(
                "El almacenamiento sale de SNOWFLAKE.ACCOUNT_USAGE, que se actualiza con hasta dos horas de demora (un valor por día).".into(),
            );
        }
        Some(Err(_)) => s.notes.push(
            "El almacenamiento de la cuenta necesita acceso a SNOWFLAKE.ACCOUNT_USAGE (IMPORTED PRIVILEGES sobre la base SNOWFLAKE).".into(),
        ),
        None => {}
    }
    match cache.tables.as_ref().map(|(_, r)| r) {
        Some(Ok(t)) => s.tables.extend(table(
            "top_objects",
            "Objetos más grandes (base actual)",
            &[("table_schema", "Esquema"), ("table_name", "Tabla"), ("row_count", "Filas"), ("bytes", "Tamaño (bytes)")],
            t,
        )),
        Some(Err(e)) => s.notes.push(format!("Tamaños de tablas: no disponible ({e}).")),
        None => {}
    }
    s.notes.push("Snowflake no expone CPU ni memoria de los warehouses: se informan consultas, carga, créditos y almacenamiento.".into());
    s.notes.push(
        "Para no consumir créditos, el historial se refresca cada 20 s, la carga y los créditos cada 5 min y el almacenamiento cada 15 min, y solo con el warehouse ya encendido."
            .into(),
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct Fake {
        wh: Option<String>,
        answers: Vec<(&'static str, Set)>,
        seen: Mutex<Vec<String>>,
    }

    #[dbine_driver::async_trait]
    impl Runner for Fake {
        async fn rows(&self, sql: &str) -> dbine_driver::Result<Set> {
            self.seen.lock().unwrap().push(sql.to_string());
            self.answers
                .iter()
                .find(|(n, _)| sql.contains(n))
                .map(|(_, s)| s.clone())
                .ok_or_else(|| dbine_driver::Error::Query("Insufficient privileges".into()))
        }
        fn warehouse(&self) -> Option<String> {
            self.wh.clone()
        }
        fn database(&self) -> Option<String> {
            Some("DB".into())
        }
    }

    fn set(cols: &[&str], rows: &[&[&str]]) -> Set {
        Set {
            cols: cols.iter().map(|c| c.to_string()).collect(),
            rows: rows.iter().map(|r| r.iter().map(|c| (!c.is_empty()).then(|| c.to_string())).collect()).collect(),
        }
    }

    fn warehouses(state: &str) -> Set {
        set(&["name", "state", "running", "queued", "is_current"], &[&["WH", state, "2", "1", "Y"], &["OTHER", "SUSPENDED", "0", "0", "N"]])
    }

    fn metric(s: &MonitorSnapshot, k: &str) -> Option<f64> {
        s.metrics.iter().find(|m| m.key == k).and_then(|m| m.value)
    }

    #[tokio::test]
    async fn a_suspended_warehouse_is_not_resumed() {
        let f = Fake { wh: None, answers: vec![("SHOW WAREHOUSES", warehouses("SUSPENDED"))], seen: Mutex::new(Vec::new()) };
        let mut cache = Cache::default();
        let s = snapshot(&f, &mut cache).await;
        assert_eq!(metric(&s, "active_sessions"), Some(2.0));
        assert_eq!(metric(&s, "warehouses_started"), Some(0.0));
        assert!(s.notes.iter().any(|n| n.contains("WH está suspendido")));
        assert!(!f.seen.lock().unwrap().iter().any(|q| q.contains("QUERY_HISTORY") || q.contains("ACCOUNT_USAGE")));
    }

    #[tokio::test]
    async fn a_running_warehouse_reads_history_once_per_ttl() {
        let history = set(
            &["query_id", "execution_status", "bytes_scanned", "query_text"],
            &[&["a", "RUNNING", "", "select 1"], &["b", "SUCCESS", "100", "select 2"], &["c", "FAILED_WITH_ERROR", "5", "x"]],
        );
        let f = Fake {
            wh: Some("wh".into()),
            answers: vec![
                ("SHOW WAREHOUSES", warehouses("STARTED")),
                ("QUERY_HISTORY", history),
                ("WAREHOUSE_METERING_HISTORY", set(&["warehouse_name", "credits"], &[&["WH", "1.5"], &["OTHER", "0.5"]])),
                ("STORAGE_USAGE", set(&["usage_date", "storage_bytes", "stage_bytes", "failsafe_bytes"], &[&["2026-09-26", "100", "10", "1"]])),
            ],
            seen: Mutex::new(Vec::new()),
        };
        let mut cache = Cache::default();
        let s = snapshot(&f, &mut cache).await;
        assert_eq!(metric(&s, "queries_last_hour"), Some(2.0));
        assert_eq!(metric(&s, "failed_last_hour"), Some(1.0));
        assert_eq!(metric(&s, "bytes_scanned"), Some(105.0));
        assert_eq!(metric(&s, "credits_24h"), Some(2.0));
        assert_eq!(metric(&s, "storage_used"), Some(111.0));
        let q = s.tables.iter().find(|t| t.key == "queries").unwrap();
        assert_eq!(q.rows.len(), 1);
        assert!(s.notes.iter().any(|n| n.starts_with("Carga de warehouses: no disponible")));
        assert!(f.seen.lock().unwrap().iter().any(|q| q.contains("'dbine-monitor'")));
        let before = f.seen.lock().unwrap().len();
        let _ = snapshot(&f, &mut cache).await;
        let again: Vec<String> = f.seen.lock().unwrap()[before..].to_vec();
        assert!(!again.iter().any(|q| q.contains("QUERY_HISTORY") || q.contains("STORAGE_USAGE")), "{again:?}");
    }
}
