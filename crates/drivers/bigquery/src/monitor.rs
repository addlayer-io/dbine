//! `Session::monitor` for BigQuery. The service exposes no CPU or memory;
//! what it does report: jobs (running, pending and recent, with bytes
//! processed and billed and slot time) through the free `jobs.list` call,
//! storage per dataset and table through `INFORMATION_SCHEMA.TABLE_STORAGE`
//! and slot reservations through `INFORMATION_SCHEMA.RESERVATIONS`. Those
//! two are billed queries, so they run once every [`STORAGE_TTL`].

use crate::{BigQuerySession, Json};
use dbine_driver::monitor::num;
use dbine_driver::{Metric, MetricUnit, MonitorSnapshot, MonitorTable, Result};
use serde_json::json;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Marks the monitor's own jobs, left out of the job lists.
const TAG: &str = "/* dbine-monitor */";
/// How far back "recent jobs" go.
const RECENT: Duration = Duration::from_secs(15 * 60);
/// How often the (billed) storage and reservation queries run again.
pub(crate) const STORAGE_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_SQL: usize = 2000;

/// Storage and reservations, refreshed every [`STORAGE_TTL`].
#[derive(Default)]
pub(crate) struct StorageCache {
    at: Option<Instant>,
    tables: Vec<TableSize>,
    /// (name, slot capacity, edition, autoscale max)
    reservations: Vec<(String, Option<f64>, String, Option<f64>)>,
    notes: Vec<String>,
    source: &'static str,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct TableSize {
    dataset: String,
    table: String,
    rows: Option<f64>,
    logical: Option<f64>,
    physical: Option<f64>,
}

/// `region-us`, `region-eu`, `region-southamerica-east1`.
pub(crate) fn region_qualifier(location: Option<&str>) -> String {
    format!("`region-{}`", location.unwrap_or("US").trim().to_ascii_lowercase())
}

fn n(v: Option<&Json>) -> Option<f64> {
    match v? {
        Json::String(s) => num(s),
        v => v.as_f64(),
    }
}

fn s<'a>(v: &'a Json, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(Json::as_str).unwrap_or("")
}

fn truncate(s: &str) -> String {
    if s.chars().count() > MAX_SQL {
        s.chars().take(MAX_SQL).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

/// jobs.list times are epoch milliseconds as strings.
fn ms_text(ms: Option<f64>) -> Json {
    let Some(ms) = ms else { return Json::Null };
    chrono::DateTime::from_timestamp_millis(ms as i64).map_or(Json::Null, |t| json!(t.format("%Y-%m-%d %H:%M:%S").to_string()))
}

/// What the dashboard needs from one job of `jobs.list` (projection=full).
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Job {
    id: String,
    user: String,
    kind: String,
    state: String,
    created: Option<f64>,
    started: Option<f64>,
    ended: Option<f64>,
    bytes_processed: Option<f64>,
    bytes_billed: Option<f64>,
    slot_ms: Option<f64>,
    cache_hit: bool,
    error: String,
    query: String,
}

/// Adds up, from +0.0 (an empty `f64` sum is -0.0).
fn total(it: impl Iterator<Item = f64>) -> f64 {
    it.fold(0.0, |a, b| a + b)
}

/// Epoch milliseconds; the emulator reports seconds.
fn epoch_ms(v: Option<f64>) -> Option<f64> {
    v.map(|t| if t < 1e11 { t * 1000.0 } else { t })
}

pub(crate) fn job(v: &Json) -> Job {
    let st = |p: &str| n(v.pointer(&format!("/statistics/{p}")));
    Job {
        id: s(v, "/jobReference/jobId").to_string(),
        user: v.get("user_email").and_then(Json::as_str).unwrap_or("").to_string(),
        kind: v
            .pointer("/configuration/jobType")
            .and_then(Json::as_str)
            .or_else(|| v.pointer("/statistics/query/statementType").and_then(Json::as_str))
            .unwrap_or("")
            .to_string(),
        state: s(v, "/status/state").to_string(),
        created: epoch_ms(st("creationTime")),
        started: epoch_ms(st("startTime")),
        ended: epoch_ms(st("endTime")),
        bytes_processed: st("query/totalBytesProcessed").or(st("totalBytesProcessed")),
        bytes_billed: st("query/totalBytesBilled"),
        slot_ms: st("query/totalSlotMs").or(st("totalSlotMs")),
        cache_hit: v.pointer("/statistics/query/cacheHit").and_then(Json::as_bool).unwrap_or(false),
        error: s(v, "/status/errorResult/message").to_string(),
        query: s(v, "/configuration/query/query").to_string(),
    }
}

fn now_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0)
}

impl BigQuerySession {
    /// One page of `jobs.list`; `all` = every user's (needs
    /// bigquery.jobs.listAll).
    async fn jobs(&self, all: bool, states: &[&str], min_created: Option<f64>, max: usize) -> Result<Vec<Job>> {
        let mut q = vec![("projection", "full".to_string()), ("maxResults", max.to_string())];
        if all {
            q.push(("allUsers", "true".into()));
        }
        for st in states {
            q.push(("stateFilter", st.to_string()));
        }
        if let Some(m) = min_created {
            q.push(("minCreationTime", format!("{}", m as u64)));
        }
        let resp = self.api.get(&["jobs"], &q).await?;
        Ok(resp
            .get("jobs")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .map(job)
            // Also filtered here: the emulator ignores the filters.
            .filter(|j| !j.query.starts_with(TAG) && states.iter().any(|st| j.state.eq_ignore_ascii_case(st)))
            .filter(|j| min_created.is_none_or(|m| j.created.is_none_or(|c| c >= m)))
            .collect())
    }

    async fn refresh_storage(&mut self) {
        if self.storage.at.is_some_and(|t| t.elapsed() < STORAGE_TTL) {
            return;
        }
        let mut c = StorageCache { at: Some(Instant::now()), ..Default::default() };
        let region = region_qualifier(self.api.location.as_deref());
        let sql = format!(
            "{TAG} SELECT table_schema, table_name, total_rows, total_logical_bytes, total_physical_bytes
             FROM {region}.INFORMATION_SCHEMA.TABLE_STORAGE WHERE deleted = FALSE"
        );
        match self.named_rows(&sql).await {
            Ok(rows) => {
                c.tables = rows
                    .iter()
                    .map(|r| TableSize {
                        dataset: r.get("table_schema").cloned().unwrap_or_default(),
                        table: r.get("table_name").cloned().unwrap_or_default(),
                        rows: r.get("total_rows").and_then(|v| num(v)),
                        logical: r.get("total_logical_bytes").and_then(|v| num(v)),
                        physical: r.get("total_physical_bytes").and_then(|v| num(v)),
                    })
                    .collect();
                c.source = "TABLE_STORAGE";
            }
            Err(e) => {
                // The emulator and logins without bigquery.tables.list on the
                // project: the session dataset's tables, one by one.
                tracing::debug!("TABLE_STORAGE: {e}");
                if let Some(ds) = self.dataset.clone() {
                    let tables = self.api.list_all(&["datasets", &ds, "tables"], "tables").await.unwrap_or_default();
                    for t in tables.iter().take(50) {
                        let id = s(t, "/tableReference/tableId").to_string();
                        if let Ok(meta) = self.api.get(&["datasets", &ds, "tables", &id], &[]).await {
                            c.tables.push(TableSize {
                                dataset: ds.clone(),
                                table: id,
                                rows: n(meta.get("numRows")),
                                logical: n(meta.get("numBytes")),
                                physical: n(meta.get("numTotalPhysicalBytes")).or(n(meta.get("numPhysicalBytes"))),
                            });
                        }
                    }
                    c.source = "tables.get";
                }
                c.notes.push(format!(
                    "No se pudo leer INFORMATION_SCHEMA.TABLE_STORAGE ({e}); los tamaños son solo los del dataset de la sesión (hasta 50 tablas)."
                ));
            }
        }
        let sql = format!(
            "{TAG} SELECT reservation_name, slot_capacity, edition, autoscale.max_slots AS autoscale_max
             FROM {region}.INFORMATION_SCHEMA.RESERVATIONS"
        );
        if let Ok(rows) = self.named_rows(&sql).await {
            c.reservations = rows
                .iter()
                .map(|r| {
                    (
                        r.get("reservation_name").cloned().unwrap_or_default(),
                        r.get("slot_capacity").and_then(|v| num(v)),
                        r.get("edition").cloned().unwrap_or_default(),
                        r.get("autoscale_max").and_then(|v| num(v)),
                    )
                })
                .collect();
        }
        self.storage = c;
    }

    pub(crate) async fn snapshot(&mut self) -> Result<MonitorSnapshot> {
        let mut snap = MonitorSnapshot::default();
        snap.info.push(("Proyecto".into(), self.api.project.clone()));
        snap.info.push(("Ubicación".into(), self.api.location.clone().unwrap_or_else(|| "US (predeterminada)".into())));
        if self.api.emulator {
            snap.info.push(("Servidor".into(), "emulador".into()));
        }

        // Running and pending jobs; every user's if the login may.
        let (active, all_users) = match self.jobs(true, &["running", "pending"], None, 200).await {
            Ok(j) => (j, true),
            Err(e) => {
                snap.notes.push(format!(
                    "Solo se ven los jobs propios: listar los de todos los usuarios requiere el permiso bigquery.jobs.listAll ({e})."
                ));
                (self.jobs(false, &["running", "pending"], None, 200).await?, false)
            }
        };
        let now = now_ms();
        let since = now - RECENT.as_millis() as f64;
        let mut done = self.jobs(all_users, &["done"], Some(since), 500).await.unwrap_or_default();
        done.sort_by(|a, b| b.ended.unwrap_or(0.0).total_cmp(&a.ended.unwrap_or(0.0)));

        let running = active.iter().filter(|j| j.state == "RUNNING").count();
        let pending = active.iter().filter(|j| j.state == "PENDING").count();
        // Average slots of each running job so far: its slot-ms over its
        // elapsed time.
        let slots = total(active.iter().filter(|j| j.state == "RUNNING").filter_map(|j| Some(j.slot_ms? / (now - j.started?).max(1.0))));
        let recent_bytes = total(done.iter().filter_map(|j| j.bytes_processed));
        let recent_billed = total(done.iter().filter_map(|j| j.bytes_billed));
        let recent_slot_ms = total(done.iter().filter_map(|j| j.slot_ms));
        let failed = done.iter().filter(|j| !j.error.is_empty()).count();
        let cached = done.iter().filter(|j| j.cache_hit).count();

        self.refresh_storage().await;
        let st = &self.storage;
        let logical = (!st.tables.is_empty()).then(|| total(st.tables.iter().filter_map(|t| t.logical)));
        let physical = (!st.tables.is_empty()).then(|| total(st.tables.iter().filter_map(|t| t.physical)));
        let capacity: Option<f64> = (!st.reservations.is_empty())
            .then(|| total(st.reservations.iter().map(|r| r.1.unwrap_or(0.0) + r.3.unwrap_or(0.0))))
            .filter(|c: &f64| *c > 0.0);

        snap.metrics = vec![
            Metric::new("active_sessions", "Jobs en ejecución", "Actividad", MetricUnit::Count, Some(running as f64)),
            Metric::new("pending_jobs", "Jobs pendientes", "Actividad", MetricUnit::Count, Some(pending as f64)),
            Metric::new("slots", "Slots en uso (estimado)", "Capacidad", MetricUnit::Count, Some((slots * 10.0).round() / 10.0))
                .max(capacity),
            Metric::new("recent_jobs", "Jobs terminados (últimos 15 min)", "Últimos 15 minutos", MetricUnit::Count, Some(done.len() as f64)),
            Metric::new("recent_failed", "Jobs fallidos (últimos 15 min)", "Últimos 15 minutos", MetricUnit::Count, Some(failed as f64)),
            Metric::new("bytes_processed", "Bytes procesados (últimos 15 min)", "Últimos 15 minutos", MetricUnit::Bytes, Some(recent_bytes)),
            Metric::new("bytes_billed", "Bytes facturados (últimos 15 min)", "Últimos 15 minutos", MetricUnit::Bytes, Some(recent_billed)),
            Metric::new("slot_time", "Tiempo de slot (últimos 15 min)", "Últimos 15 minutos", MetricUnit::Millis, Some(recent_slot_ms)),
            Metric::new(
                "cache_hit",
                "Consultas servidas desde caché (últimos 15 min)",
                "Caché",
                MetricUnit::Percent,
                (!done.is_empty()).then(|| (cached as f64 * 1000.0 / done.len() as f64).round() / 10.0),
            )
            .max(Some(100.0)),
            Metric::new("storage_used", "Almacenamiento lógico", "Almacenamiento", MetricUnit::Bytes, logical),
            Metric::new("storage_physical", "Almacenamiento físico", "Almacenamiento", MetricUnit::Bytes, physical),
        ];

        let cols_active = ["Id", "Usuario", "Tipo", "Estado", "Creado", "Duración (s)", "Bytes procesados", "Slot (ms)", "Consulta"];
        let mut t = MonitorTable::new("queries", "Jobs en curso", &cols_active);
        for j in active.iter().take(200) {
            t.rows.push(vec![
                json!(j.id),
                json!(j.user),
                json!(j.kind),
                json!(j.state),
                ms_text(j.created),
                j.created.map_or(Json::Null, |c| json!(((now - j.started.unwrap_or(c)) / 1000.0).round())),
                j.bytes_processed.map_or(Json::Null, |v| json!(v)),
                j.slot_ms.map_or(Json::Null, |v| json!(v)),
                json!(truncate(&j.query)),
            ]);
        }
        snap.tables.push(t);

        let mut t = MonitorTable::new(
            "recent_queries",
            "Jobs recientes (últimos 15 min)",
            &["Id", "Usuario", "Tipo", "Fin", "Duración (s)", "Bytes procesados", "Bytes facturados", "Slot (ms)", "Caché", "Error", "Consulta"],
        );
        for j in done.iter().take(100) {
            t.rows.push(vec![
                json!(j.id),
                json!(j.user),
                json!(j.kind),
                ms_text(j.ended),
                match (j.started.or(j.created), j.ended) {
                    (Some(a), Some(b)) => json!(((b - a) / 100.0).round() / 10.0),
                    _ => Json::Null,
                },
                j.bytes_processed.map_or(Json::Null, |v| json!(v)),
                j.bytes_billed.map_or(Json::Null, |v| json!(v)),
                j.slot_ms.map_or(Json::Null, |v| json!(v)),
                json!(if j.cache_hit { "sí" } else { "no" }),
                if j.error.is_empty() { Json::Null } else { json!(j.error) },
                json!(truncate(&j.query)),
            ]);
        }
        snap.tables.push(t);

        if !st.tables.is_empty() {
            let mut by_ds: std::collections::BTreeMap<&str, (usize, f64, f64, f64)> = Default::default();
            for t in &st.tables {
                let e = by_ds.entry(t.dataset.as_str()).or_default();
                e.0 += 1;
                e.1 += t.rows.unwrap_or(0.0);
                e.2 += t.logical.unwrap_or(0.0);
                e.3 += t.physical.unwrap_or(0.0);
            }
            let mut dbs = MonitorTable::new("databases", "Datasets y tamaños", &["Dataset", "Tablas", "Filas", "Bytes lógicos", "Bytes físicos"]);
            let mut sorted: Vec<_> = by_ds.into_iter().collect();
            sorted.sort_by(|a, b| b.1 .2.total_cmp(&a.1 .2));
            for (ds, (tables, rows, l, p)) in sorted.into_iter().take(200) {
                dbs.rows.push(vec![json!(ds), json!(tables), json!(rows), json!(l), json!(p)]);
            }
            snap.tables.push(dbs);
            let mut top: Vec<&TableSize> = st.tables.iter().collect();
            top.sort_by(|a, b| b.logical.unwrap_or(0.0).total_cmp(&a.logical.unwrap_or(0.0)));
            let mut t = MonitorTable::new("top_objects", "Tablas más grandes", &["Dataset", "Tabla", "Filas", "Bytes lógicos", "Bytes físicos"]);
            for x in top.into_iter().take(20) {
                let o = |v: Option<f64>| v.map_or(Json::Null, |v| json!(v));
                t.rows.push(vec![json!(x.dataset), json!(x.table), o(x.rows), o(x.logical), o(x.physical)]);
            }
            snap.tables.push(t);
        }
        if !st.reservations.is_empty() {
            let mut t = MonitorTable::new("reservations", "Reservas de slots", &["Reserva", "Slots", "Edición", "Autoescalado máx."]);
            for (name, cap, ed, auto) in st.reservations.iter().take(200) {
                let o = |v: &Option<f64>| v.map_or(Json::Null, |v| json!(v));
                t.rows.push(vec![json!(name), o(cap), json!(ed), o(auto)]);
            }
            snap.tables.push(t);
            snap.info.push(("Reservas de slots".into(), st.reservations.len().to_string()));
        }
        snap.notes.extend(st.notes.iter().cloned());
        snap.notes.push("BigQuery es un servicio administrado: no expone CPU, memoria ni conexiones; el consumo se mide en slots y bytes procesados.".into());
        if !st.source.is_empty() {
            snap.notes.push(format!(
                "Los tamaños de almacenamiento se actualizan cada {} minutos{}.",
                STORAGE_TTL.as_secs() / 60,
                if st.source == "TABLE_STORAGE" { " (la consulta a INFORMATION_SCHEMA se factura como cualquier otra)" } else { "" }
            ));
        }
        if st.reservations.is_empty() {
            snap.notes.push("No hay reservas de slots visibles en esta región (proyecto con precios on-demand o sin permiso bigquery.reservations.list).".into());
        }
        Ok(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn region_names() {
        assert_eq!(region_qualifier(None), "`region-us`");
        assert_eq!(region_qualifier(Some("EU")), "`region-eu`");
        assert_eq!(region_qualifier(Some("southamerica-east1")), "`region-southamerica-east1`");
    }

    #[test]
    fn jobs_from_the_api() {
        let v = json!({
            "jobReference": {"jobId": "j1", "location": "US"},
            "user_email": "a@b.com",
            "configuration": {"jobType": "QUERY", "query": {"query": "SELECT 1"}},
            "status": {"state": "DONE", "errorResult": {"message": "boom"}},
            "statistics": {"creationTime": "1700000000000", "startTime": "1700000000100", "endTime": "1700000002100",
                "query": {"totalBytesProcessed": "1048576", "totalBytesBilled": "10485760", "totalSlotMs": "500", "cacheHit": true}}
        });
        let j = job(&v);
        assert_eq!(j.id, "j1");
        assert_eq!(j.kind, "QUERY");
        assert_eq!(j.bytes_processed, Some(1048576.0));
        assert_eq!(j.bytes_billed, Some(10485760.0));
        assert_eq!(j.slot_ms, Some(500.0));
        assert!(j.cache_hit);
        assert_eq!(j.error, "boom");
        assert_eq!(ms_text(j.created), json!("2023-11-14 22:13:20"));
        // The emulator's shape: no configuration, top-level bytes.
        let e = job(&json!({"jobReference": {"jobId": "x"}, "status": {"state": "DONE"},
            "statistics": {"creationTime": "1700000000", "totalBytesProcessed": "1", "query": {"statementType": "SELECT"}}}));
        assert_eq!((e.kind.as_str(), e.bytes_processed, e.created), ("SELECT", Some(1.0), Some(1.7e12)));
        assert_eq!(total(std::iter::empty()).to_string(), "0");
    }
}
