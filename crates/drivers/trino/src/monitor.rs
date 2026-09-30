//! `Session::monitor` for Trino, Presto and Starburst. The coordinator's
//! REST endpoints give its own health (`/v1/info`, `/v1/status`; Presto also
//! has the cluster totals of `/v1/cluster`), the `jmx` catalog gives every
//! node's CPU and heap plus the query manager's running totals, and
//! `system.runtime.*` lists nodes, queries and tasks. Each part is optional:
//! what fails (no `jmx` catalog, no access to `system`) becomes a note.

use crate::{http_error, text, Flavor, TrinoSession};
use dbine_driver::monitor::num;
use dbine_driver::{Error, Metric, MetricUnit, MonitorSnapshot, MonitorTable, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// Marks the monitor's own statements, so they don't list themselves as
/// running queries.
const TAG: &str = "/* dbine-monitor */";
const MAX_SQL: usize = 2000;

/// The names of the JMX tables the monitor reads; they depend on the
/// product (`trino.execution…`, `com.facebook.presto.execution…`).
#[derive(Default, Clone, Debug)]
pub(crate) struct JmxTables {
    query_manager: Option<String>,
    cluster_memory: Option<String>,
    general_pool: Option<String>,
    /// Each table's columns, which vary between products and versions.
    columns: HashMap<String, HashSet<String>>,
}

impl JmxTables {
    fn from_names(names: &[String]) -> Self {
        let pick = |suffix: &str| names.iter().find(|n| n.ends_with(suffix)).cloned();
        Self {
            query_manager: pick(".execution:name=querymanager"),
            cluster_memory: pick(".memory:name=clustermemorymanager"),
            general_pool: pick(".memory:name=general,type=clustermemorypool"),
            columns: HashMap::new(),
        }
    }
}

fn q(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Airlift's duration text ("34.73s", "5.23m", "1.20h", "2.00d", "12ms").
pub(crate) fn duration_secs(s: &str) -> Option<f64> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic())?;
    let (n, unit) = s.split_at(split);
    let n: f64 = n.trim().parse().ok()?;
    let mult = match unit {
        "ns" => 1e-9,
        "us" => 1e-6,
        "ms" => 1e-3,
        "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        "d" => 86400.0,
        _ => return None,
    };
    Some(n * mult)
}

/// `used=123` out of a JMX composite rendered as text.
fn composite(s: &str, field: &str) -> Option<f64> {
    let at = s.find(&format!("{field}="))? + field.len() + 1;
    let rest = &s[at..];
    let end = rest.find(|c: char| !c.is_ascii_digit() && c != '-').unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn f(v: &Value, ptr: &str) -> Option<f64> {
    v.pointer(ptr).and_then(Value::as_f64)
}

fn cell(s: &str) -> Value {
    if s.is_empty() {
        Value::Null
    } else {
        Value::String(s.to_string())
    }
}

fn cell_num(s: &str) -> Value {
    num(s).map_or_else(|| cell(s), |n| json!(n))
}

fn truncate(s: &str) -> String {
    if s.chars().count() > MAX_SQL {
        s.chars().take(MAX_SQL).collect::<String>() + "…"
    } else {
        s.to_string()
    }
}

impl TrinoSession {
    pub(crate) async fn get_json(&self, path: &str) -> Result<Value> {
        let rb = self.conn.http.get(format!("{}{path}", self.conn.base)).header(self.flavor.header("user"), &self.conn.user);
        let resp = self.conn.auth(rb).send().await.map_err(http_error)?;
        if !resp.status().is_success() {
            return Err(Error::Query(format!("HTTP {}", resp.status())));
        }
        resp.json().await.map_err(Error::query)
    }

    async fn jmx_tables(&mut self) -> Result<JmxTables> {
        if let Some(t) = &self.jmx {
            return Ok(t.clone());
        }
        let rows = self
            .strings(&format!(
                "{TAG} SELECT table_name, column_name FROM jmx.information_schema.columns WHERE table_schema = 'current'
                 AND (table_name LIKE '%:name=querymanager' OR table_name LIKE '%:name=clustermemorymanager'
                      OR table_name LIKE '%:name=general,type=clustermemorypool')"
            ))
            .await?;
        let mut t = JmxTables::default();
        for r in rows.into_iter().filter(|r| r.len() == 2) {
            t.columns.entry(r[0].clone()).or_default().insert(r[1].clone());
        }
        let names: Vec<String> = t.columns.keys().cloned().collect();
        let found = JmxTables::from_names(&names);
        t.query_manager = found.query_manager;
        t.cluster_memory = found.cluster_memory;
        t.general_pool = found.general_pool;
        self.jmx = Some(t.clone());
        Ok(t)
    }

    /// `agg(column)` of each column over the JMX table's rows (one per
    /// node); `None` where the table or the column doesn't exist in this
    /// product or version.
    async fn jmx_values(&mut self, t: &JmxTables, table: Option<&str>, cols: &[(&str, &str)]) -> Vec<Option<f64>> {
        let none = vec![None; cols.len()];
        let Some(table) = table else { return none };
        let have = t.columns.get(table);
        let sel = cols
            .iter()
            .map(|(agg, c)| if have.is_some_and(|h| h.contains(*c)) { format!("{agg}({})", q(c)) } else { "NULL".into() })
            .collect::<Vec<_>>()
            .join(", ");
        match self.strings(&format!("{TAG} SELECT {sel} FROM jmx.current.{}", q(table))).await {
            Ok(rows) => rows.first().map_or(none, |r| r.iter().map(|v| num(v)).collect()),
            Err(e) => {
                tracing::debug!("jmx {table}: {e}");
                none
            }
        }
    }

    pub(crate) async fn snapshot(&mut self) -> Result<MonitorSnapshot> {
        let mut snap = MonitorSnapshot::default();
        let mut m: HashMap<&'static str, f64> = HashMap::new();

        // The coordinator itself.
        let info = self.get_json("/v1/info").await?;
        let version = info.pointer("/nodeVersion/version").map(text).unwrap_or_default();
        snap.info.push(("Versión".into(), format!("{} {version}", crate::info_name(self.flavor))));
        if let Some(e) = info.get("environment").map(text) {
            snap.info.push(("Entorno".into(), e));
        }
        if let Some(st) = info.get("state").map(text).filter(|s| !s.is_empty()) {
            snap.info.push(("Estado del coordinador".into(), st));
        }
        if let Some(up) = info.get("uptime").and_then(Value::as_str).and_then(duration_secs) {
            m.insert("uptime", up);
        }
        let status = self.get_json("/v1/status").await.ok();
        if let Some(st) = &status {
            if let Some(n) = st.get("nodeId").map(text) {
                snap.info.push(("Nodo coordinador".into(), n));
            }
            if let Some(p) = f(st, "/processors") {
                snap.info.push(("Procesadores del coordinador".into(), p.to_string()));
            }
        }

        // Presto's cluster totals (Trino moved them to the web UI's API,
        // behind the UI login).
        let cluster = if self.flavor == Flavor::Presto { self.get_json("/v1/cluster").await.ok() } else { None };
        if let Some(c) = &cluster {
            for (k, p) in [
                ("running", "/runningQueries"),
                ("queued", "/queuedQueries"),
                ("blocked", "/blockedQueries"),
                ("drivers", "/runningDrivers"),
                ("workers", "/activeWorkers"),
                ("query_mem", "/reservedMemory"),
                ("rows_read", "/totalInputRows"),
                ("bytes_read", "/totalInputBytes"),
                ("query_cpu", "/totalCpuTimeSecs"),
            ] {
                if let Some(v) = f(c, p) {
                    m.insert(k, v);
                }
            }
        }

        // JMX: every node's OS and heap, the query manager's totals.
        let mut node_os: HashMap<String, (Option<f64>, Option<f64>, Option<f64>)> = HashMap::new();
        match self.jmx_tables().await {
            Ok(t) => {
                let os = self
                    .strings(&format!(
                        "{TAG} SELECT o.node, o.systemcpuload, o.processcputime, m.heapmemoryusage
                         FROM jmx.current.\"java.lang:type=operatingsystem\" o
                         LEFT JOIN jmx.current.\"java.lang:type=memory\" m ON m.node = o.node"
                    ))
                    .await
                    .unwrap_or_default();
                let (mut cpu_sum, mut cpu_n, mut cpu_time, mut heap, mut heap_max) = (0.0, 0, None::<f64>, None::<f64>, None::<f64>);
                for r in os.iter().filter(|r| r.len() == 4) {
                    let load = num(&r[1]).filter(|l| *l >= 0.0);
                    if let Some(l) = load {
                        cpu_sum += l;
                        cpu_n += 1;
                    }
                    if let Some(ns) = num(&r[2]) {
                        *cpu_time.get_or_insert(0.0) += ns / 1e9 * 100.0;
                    }
                    let used = composite(&r[3], "used");
                    if let Some(u) = used {
                        *heap.get_or_insert(0.0) += u;
                    }
                    if let Some(x) = composite(&r[3], "max").filter(|x| *x > 0.0) {
                        *heap_max.get_or_insert(0.0) += x;
                    }
                    node_os.insert(r[0].clone(), (load.map(|l| l * 100.0), used, composite(&r[3], "max")));
                }
                if cpu_n > 0 {
                    m.insert("cpu", cpu_sum / cpu_n as f64 * 100.0);
                }
                if let Some(v) = cpu_time {
                    m.insert("cpu_time", v);
                }
                if let Some(v) = heap {
                    m.insert("heap", v);
                }
                if let Some(v) = heap_max {
                    m.insert("heap_max", v);
                }
                let qm = self
                    .jmx_values(
                        &t,
                        t.query_manager.as_deref(),
                        &[
                            ("sum", "runningqueries"),
                            ("sum", "queuedqueries"),
                            ("sum", "fullyblockedqueries"),
                            ("sum", "runningdrivers"),
                            ("sum", "startedqueries.totalcount"),
                            ("sum", "failedqueries.totalcount"),
                            ("sum", "consumedinputrows.totalcount"),
                            ("sum", "consumedinputbytes.totalcount"),
                            ("sum", "consumedcputimesecs.totalcount"),
                        ],
                    )
                    .await;
                let keys = ["running", "queued", "blocked", "drivers", "started", "failed", "rows_read", "bytes_read", "query_cpu"];
                for (k, v) in keys.iter().zip(qm) {
                    if let Some(v) = v {
                        // This very query is one of the running ones.
                        let v = if *k == "running" { (v - 1.0).max(0.0) } else { v };
                        m.entry(k).or_insert(v);
                    }
                }
                let cm = self
                    .jmx_values(
                        &t,
                        t.cluster_memory.as_deref(),
                        &[
                            ("max", "clustertotalmemoryreservation"),
                            ("max", "clustermemorybytes"),
                            ("max", "totalavailableprocessors"),
                            ("max", "querieskilledduetooutofmemory"),
                        ],
                    )
                    .await;
                let gp = self
                    .jmx_values(&t, t.general_pool.as_deref(), &[("max", "reserveddistributedbytes"), ("max", "totaldistributedbytes")])
                    .await;
                for (k, v) in ["query_mem", "query_mem_max", "processors", "oom_kills", "query_mem", "query_mem_max"].iter().zip(cm.into_iter().chain(gp)) {
                    if let Some(v) = v {
                        m.entry(k).or_insert(v);
                    }
                }
            }
            Err(_) => snap.notes.push(
                "Sin el catálogo jmx solo se ven el CPU y la memoria del coordinador; configurá el conector JMX (catalog/jmx.properties) para ver los de cada nodo y los totales de consultas."
                    .into(),
            ),
        }

        // Without JMX, the coordinator's own figures.
        if let Some(st) = &status {
            if !m.contains_key("cpu") {
                if let Some(l) = f(st, "/systemCpuLoad").filter(|l| *l >= 0.0) {
                    m.insert("cpu", l * 100.0);
                }
            }
            if !m.contains_key("heap") {
                if let Some(h) = f(st, "/heapUsed") {
                    m.insert("heap", h);
                }
                if let Some(h) = f(st, "/heapAvailable") {
                    m.insert("heap_max", h);
                }
            }
            if !m.contains_key("query_mem") {
                // Trino has one pool; Presto, "general" and "reserved".
                let pool = |k: &str| f(st, &format!("/memoryInfo/pool/{k}")).or_else(|| f(st, &format!("/memoryInfo/pools/general/{k}")));
                if let (Some(r), Some(x)) = (pool("reservedBytes"), pool("maxBytes")) {
                    m.insert("query_mem", r);
                    m.insert("query_mem_max", x);
                }
            }
        }

        // Nodes, with their running tasks.
        let nodes = self
            .strings(&format!("{TAG} SELECT node_id, http_uri, node_version, coordinator, state FROM system.runtime.nodes ORDER BY coordinator DESC, node_id"))
            .await;
        match nodes {
            Ok(nodes) => {
                let tasks: HashMap<String, Vec<String>> = self
                    .strings(&format!(
                        "{TAG} SELECT node_id, count(*), sum(running_splits), sum(queued_splits) FROM system.runtime.tasks
                         WHERE state = 'RUNNING' GROUP BY node_id"
                    ))
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|r| r.len() == 4)
                    .map(|r| (r[0].clone(), r[1..].to_vec()))
                    .collect();
                let active = nodes.iter().filter(|r| r.get(4).is_some_and(|s| s.eq_ignore_ascii_case("active"))).count();
                m.insert("nodes", active as f64);
                let mut t = MonitorTable::new(
                    "nodes",
                    "Nodos del cluster",
                    &["Nodo", "URI", "Versión", "Rol", "Estado", "CPU (%)", "Heap usado", "Heap máximo", "Tareas", "Splits en curso", "Splits en cola"],
                );
                for r in nodes.iter().filter(|r| r.len() == 5).take(200) {
                    let (cpu, used, max) = node_os.get(&r[0]).copied().unwrap_or_default();
                    let tk = tasks.get(&r[0]);
                    let tk = |i: usize| tk.and_then(|v| v.get(i)).map_or(json!(0), |s| cell_num(s));
                    t.rows.push(vec![
                        cell(&r[0]),
                        cell(&r[1]),
                        cell(&r[2]),
                        json!(if r[3] == "true" { "coordinador" } else { "worker" }),
                        cell(&r[4]),
                        cpu.map_or(Value::Null, |c| json!((c * 10.0).round() / 10.0)),
                        used.map_or(Value::Null, |v| json!(v)),
                        max.filter(|x| *x > 0.0).map_or(Value::Null, |v| json!(v)),
                        tk(0),
                        tk(1),
                        tk(2),
                    ]);
                }
                snap.tables.push(t);
            }
            Err(e) => snap.notes.push(format!("No se pudo leer system.runtime.nodes: {e}")),
        }

        // Queries: state counts, the ones in flight and the latest finished.
        match self
            .strings(&format!(
                "{TAG} SELECT query_id, state, \"user\", source, query, queued_time_ms, created, started,
                        date_diff('millisecond', coalesce(started, created), coalesce(\"end\", current_timestamp)), \"end\", {error}
                 FROM system.runtime.queries WHERE query NOT LIKE '/* dbine-monitor */%' ORDER BY created DESC LIMIT 1000",
                // Presto's table has no error columns.
                error = if self.flavor == Flavor::Presto { "NULL" } else { "error_code" }
            ))
            .await
        {
            Ok(rows) => {
                let rows: Vec<Vec<String>> = rows.into_iter().filter(|r| r.len() == 11).collect();
                let finished = |s: &str| matches!(s, "FINISHED" | "FAILED");
                if !m.contains_key("running") {
                    m.insert("running", rows.iter().filter(|r| r[1] == "RUNNING" || r[1] == "FINISHING").count() as f64);
                    m.insert("queued", rows.iter().filter(|r| matches!(r[1].as_str(), "QUEUED" | "WAITING_FOR_RESOURCES" | "DISPATCHING")).count() as f64);
                }
                let mut running = MonitorTable::new(
                    "queries",
                    "Consultas en curso",
                    &["Id", "Estado", "Usuario", "Origen", "Creada", "En cola (ms)", "Duración (ms)", "Consulta"],
                );
                let mut recent = MonitorTable::new(
                    "recent_queries",
                    "Consultas recientes",
                    &["Id", "Estado", "Usuario", "Origen", "Fin", "Duración (ms)", "Error", "Consulta"],
                );
                for r in &rows {
                    if !finished(&r[1]) {
                        if running.rows.len() < 200 {
                            running.rows.push(vec![
                                cell(&r[0]),
                                cell(&r[1]),
                                cell(&r[2]),
                                cell(&r[3]),
                                cell(&r[6]),
                                cell_num(&r[5]),
                                cell_num(&r[8]),
                                json!(truncate(&r[4])),
                            ]);
                        }
                    } else if recent.rows.len() < 50 {
                        recent.rows.push(vec![
                            cell(&r[0]),
                            cell(&r[1]),
                            cell(&r[2]),
                            cell(&r[3]),
                            cell(&r[9]),
                            cell_num(&r[8]),
                            cell(&r[10]),
                            json!(truncate(&r[4])),
                        ]);
                    }
                }
                snap.tables.insert(0, running);
                snap.tables.push(recent);
            }
            Err(e) => snap.notes.push(format!("No se pudo leer system.runtime.queries: {e}")),
        }

        let g = |k: &str| m.get(k).copied();
        let pct = |k: &str| g(k).map(|v| (v * 10.0).round() / 10.0);
        snap.metrics = vec![
            Metric::new("cpu", "CPU del cluster (promedio de nodos)", "CPU", MetricUnit::Percent, pct("cpu")).max(Some(100.0)),
            Metric::new("cpu_time", "CPU de los procesos", "CPU", MetricUnit::Percent, g("cpu_time")).counter(),
            Metric::new("query_cpu", "CPU de las consultas", "CPU", MetricUnit::Percent, g("query_cpu").map(|s| s * 100.0)).counter(),
            Metric::new("mem_used", "Heap usado (JVM)", "Memoria", MetricUnit::Bytes, g("heap")).max(g("heap_max")),
            Metric::new("query_memory", "Memoria reservada por consultas", "Memoria", MetricUnit::Bytes, g("query_mem"))
                .max(g("query_mem_max")),
            Metric::new("active_sessions", "Consultas en ejecución", "Conexiones", MetricUnit::Count, g("running")),
            Metric::new("queued_queries", "Consultas en cola", "Conexiones", MetricUnit::Count, g("queued")),
            Metric::new("blocked_queries", "Consultas bloqueadas", "Conexiones", MetricUnit::Count, g("blocked")),
            Metric::new("queries", "Consultas iniciadas", "Actividad", MetricUnit::Count, g("started")).counter(),
            Metric::new("failed_queries", "Consultas fallidas", "Actividad", MetricUnit::Count, g("failed")).counter(),
            Metric::new("rows_read", "Filas leídas", "Actividad", MetricUnit::Count, g("rows_read")).counter(),
            Metric::new("bytes_read", "Datos leídos", "Actividad", MetricUnit::Bytes, g("bytes_read")).counter(),
            Metric::new("running_drivers", "Drivers en ejecución", "Actividad", MetricUnit::Count, g("drivers")),
            Metric::new("nodes", "Nodos activos", "Cluster", MetricUnit::Count, g("nodes").or(g("workers"))),
            Metric::new("oom_kills", "Consultas terminadas por falta de memoria", "Memoria", MetricUnit::Count, g("oom_kills")),
            Metric::new("uptime", "Tiempo activo del coordinador", "Servidor", MetricUnit::Seconds, g("uptime")),
        ];
        if let Some(p) = g("processors") {
            snap.info.push(("Procesadores del cluster".into(), p.to_string()));
        }
        snap.notes.push(format!(
            "{} no informa el uso de disco ni de red por la API; el almacenamiento depende de cada conector (Hive, Iceberg…).",
            crate::info_name(self.flavor)
        ));
        Ok(snap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration_secs("34.73s"), Some(34.73));
        assert_eq!(duration_secs("2.00m"), Some(120.0));
        assert_eq!(duration_secs("1.50h"), Some(5400.0));
        assert_eq!(duration_secs("1.00d"), Some(86400.0));
        assert_eq!(duration_secs("250.00ms"), Some(0.25));
        assert_eq!(duration_secs("x"), None);
    }

    #[test]
    fn jmx_composites_and_names() {
        let heap = "javax.management.openmbean.CompositeDataSupport(compositeType=...,contents={committed=268435456, init=1, max=13455327232, used=184863336})";
        assert_eq!(composite(heap, "used"), Some(184863336.0));
        assert_eq!(composite(heap, "max"), Some(13455327232.0));
        assert_eq!(composite(heap, "nope"), None);
        let t = JmxTables::from_names(&[
            "com.facebook.presto.execution:name=querymanager".into(),
            "trino.memory:name=clustermemorymanager".into(),
        ]);
        assert_eq!(t.query_manager.as_deref(), Some("com.facebook.presto.execution:name=querymanager"));
        assert!(t.cluster_memory.is_some() && t.general_pool.is_none());
        assert_eq!(q("a\"b"), "\"a\"\"b\"");
    }
}
