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
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(rest.len());
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

/// Where the query memory figures came from.
#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub(crate) enum QueryMem {
    #[default]
    None,
    /// JMX `ClusterMemoryManager` (Trino).
    Cluster,
    /// JMX `general` cluster memory pool (Presto).
    GeneralPool,
    /// The coordinator's own pool, from `/v1/status` (no `jmx` catalog).
    Coordinator,
}

/// Where the cluster's core count came from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Cores {
    /// `availableprocessors` of every node's operating system MBean.
    Os,
    /// The cluster memory manager's `totalavailableprocessors`.
    ClusterMemory,
}

/// Turns the running CPU totals into counters whose rate is a share of the
/// cluster's cores. Each read adds only the CPU spent since the previous
/// one, over the cores of that interval, so a node that joins or leaves
/// doesn't make the rate jump (dividing the whole total by today's cores
/// would). A read that fails gives no value: the UI keeps the last one and
/// the next read covers both intervals.
#[derive(Default, Debug)]
pub(crate) struct CpuTotals {
    /// Each node's process CPU (ns) at the last read.
    nodes: HashMap<String, f64>,
    /// The engine's CPU so far, in % of the cores × seconds.
    process: f64,
    /// The cluster's query CPU seconds at the last read, and whether they
    /// came from `/v1/cluster` (its total and the JMX one differ).
    query_secs: Option<(f64, bool)>,
    /// Query CPU so far: % of the cores × seconds once the cores are known,
    /// CPU seconds before that.
    query: f64,
    /// `query` counts in % of the cores.
    query_pct: bool,
    /// The cluster's cores, as last read.
    cores: Option<(f64, Cores)>,
}

impl CpuTotals {
    /// `now`: each node's process CPU (ns) and cores, as read now; empty when
    /// the read failed.
    pub(crate) fn process(&mut self, now: &[(String, f64, f64)]) -> Option<f64> {
        if now.is_empty() {
            return None;
        }
        let (mut cpu, mut cores) = (0.0, 0.0);
        for (node, ns, c) in now {
            // Only the nodes read both times, over their own cores.
            if let Some(prev) = self.nodes.get(node) {
                // Lower than before: the node restarted, all of it is new.
                cpu += if ns >= prev { ns - prev } else { *ns };
                cores += c;
            }
        }
        if cores > 0.0 {
            self.process += cpu / 1e9 * 100.0 / cores;
        }
        self.nodes = now.iter().map(|(n, ns, _)| (n.clone(), *ns)).collect();
        Some(self.process)
    }

    /// `secs`: the cluster's query CPU seconds, if read now (`from_cluster`:
    /// out of `/v1/cluster`); `cores`: its cores, if read now. The value,
    /// and where its cores came from (`None`: it counts CPU seconds); the
    /// last known cores count even on a read without them.
    pub(crate) fn query(
        &mut self,
        secs: Option<f64>,
        from_cluster: bool,
        cores: Option<(f64, Cores)>,
    ) -> (Option<f64>, Option<Cores>) {
        if cores.is_some() {
            self.cores = cores;
        }
        let from = self.cores.map(|c| c.1);
        let Some(secs) = secs else {
            return (None, from);
        };
        // Presto's /v1/cluster adds the running queries, so it can dip as
        // they end; a change of source starts from zero.
        let delta = match self.query_secs {
            Some((prev, src)) if src == from_cluster => (secs - prev).max(0.0),
            _ => 0.0,
        };
        self.query_secs = Some((secs, from_cluster));
        match self.cores {
            Some((c, _)) => {
                if !self.query_pct {
                    // Seconds until now: starting again reads as a
                    // restart in the UI, not as a jump.
                    self.query_pct = true;
                    self.query = 0.0;
                }
                self.query += delta * 100.0 / c;
            }
            None => self.query += delta,
        }
        (Some(self.query), from)
    }
}

/// Where the figures that have more than one source came from, so each
/// metric says what it really is.
#[derive(Default, Debug)]
pub(crate) struct Origin {
    /// Keys read from Presto's `/v1/cluster`.
    cluster: HashSet<&'static str>,
    /// CPU and heap of the coordinator only (`/v1/status`), without JMX.
    cpu_coordinator: bool,
    heap_coordinator: bool,
    query_mem: QueryMem,
    /// Running and queued counted from `system.runtime.queries`.
    counted: bool,
    /// The JMX tables read, named as this product names them.
    jmx: JmxTables,
    /// Where the cores query CPU is a share of came from; `None`: it
    /// counts CPU seconds.
    query_cores: Option<Cores>,
}

/// `jmx.current."<table>"`, with a generic name when it wasn't found.
fn jmx_table(name: &Option<String>, generic: &str) -> String {
    format!("jmx.current.\"{}\"", name.as_deref().unwrap_or(generic))
}

/// The headline figures, from what the snapshot read (keyed as in
/// [`TrinoSession::snapshot`]).
pub(crate) fn metrics(m: &HashMap<&str, f64>, o: &Origin) -> Vec<Metric> {
    let g = |k: &str| m.get(k).copied();
    let pct = |k: &str| g(k).map(|v| (v * 10.0).round() / 10.0);
    let from_cluster = |k: &str| o.cluster.contains(k);
    let qm = jmx_table(
        &o.jmx.query_manager,
        "<product>.execution:name=querymanager",
    );
    let cm = jmx_table(
        &o.jmx.cluster_memory,
        "<product>.memory:name=clustermemorymanager",
    );
    let gp = jmx_table(
        &o.jmx.general_pool,
        "<product>.memory:name=general,type=clustermemorypool",
    );
    // Query CPU as a share of every node's cores when they are known;
    // otherwise CPU seconds (whose rate is cores busy), never a share of
    // one core. The value is already built up read by read (`CpuTotals`).
    let query_cpu_src = if from_cluster("query_cpu") {
        "GET /v1/cluster: totalCpuTimeSecs (finished queries plus the running ones)".to_string()
    } else {
        format!("{qm}: consumedcputimesecs.totalcount (each query's whole CPU seconds, added as it ends)")
    };
    let query_cpu = match o.query_cores {
        Some(from) => Metric::new(
            "query_cpu",
            "CPU de las consultas",
            "CPU",
            MetricUnit::Percent,
            g("query_cpu"),
        )
        .max(Some(100.0))
        .help(if from_cluster("query_cpu") {
            "Tiempo de CPU que gastaron las consultas, como parte de todos los núcleos del cluster: 100 % es todos ocupados. Comparado con el CPU de los procesos, muestra cuánto se va en consultas y cuánto en el resto del motor."
        } else {
            "Tiempo de CPU que gastaron las consultas, como parte de todos los núcleos del cluster: 100 % es todos ocupados. Cada consulta suma sus segundos enteros de CPU cuando termina: las de menos de un segundo no cuentan, y una larga aparece de golpe al final y puede pasar del 100 % por un momento."
        })
        .source(&format!(
            "{query_cpu_src}: its increase since the previous read × 100 ÷ the cluster's cores ({})",
            match from {
                Cores::Os => "jmx.current.\"java.lang:type=operatingsystem\" availableprocessors, summed over the nodes".to_string(),
                Cores::ClusterMemory => format!("{cm} totalavailableprocessors"),
            }
        )),
        None => Metric::new(
            "query_cpu",
            "CPU de las consultas",
            "CPU",
            MetricUnit::Seconds,
            g("query_cpu"),
        )
        .help(if from_cluster("query_cpu") {
            "Tiempo de CPU que gastaron las consultas, en segundos de CPU: un segundo por segundo es un núcleo ocupado. Se muestra así porque, sin el catálogo jmx, no se sabe cuántos núcleos tiene el cluster."
        } else {
            "Tiempo de CPU que gastaron las consultas, en segundos de CPU: un segundo por segundo es un núcleo ocupado, porque no se pudo leer cuántos núcleos tiene el cluster. Cada consulta suma sus segundos enteros de CPU cuando termina: las de menos de un segundo no cuentan, y una larga aparece de golpe al final."
        })
        .source(&query_cpu_src),
    }
    .counter();
    let (cpu_help, cpu_src) = if o.cpu_coordinator {
        (
            "Uso de CPU de la máquina del coordinador, con otros procesos incluidos: sin el catálogo jmx no se ven los workers. 100 % es todos sus núcleos ocupados.",
            "GET /v1/status: systemCpuLoad × 100 (coordinator only)",
        )
    } else {
        (
            "Uso de CPU de las máquinas del cluster, promedio entre nodos, con otros procesos incluidos. 100 % es todos los núcleos ocupados; el de cada nodo está en la tabla de nodos.",
            "jmx.current.\"java.lang:type=operatingsystem\": systemcpuload × 100, averaged over the nodes",
        )
    };
    let (heap_help, heap_src) = if o.heap_coordinator {
        (
            "Heap de Java en uso en el coordinador, sobre su máximo: sin el catálogo jmx no se ven los workers. Sube y baja con cada recolección.",
            "GET /v1/status: heapUsed; max: heapAvailable (coordinator only)",
        )
    } else {
        (
            "Heap de Java en uso en todos los nodos, sumado, sobre la suma de sus máximos (-Xmx). Sube y baja con cada recolección; si queda cerca del máximo aun después, faltan memoria o nodos.",
            "jmx.current.\"java.lang:type=memory\": heapmemoryusage used, summed over the nodes; max: heapmemoryusage max (sum)",
        )
    };
    let (mem_help, mem_src) = match (from_cluster("query_mem"), o.query_mem) {
        (true, _) => (
            "Memoria de usuario que tienen reservada las consultas en curso, en todo el cluster, sobre la memoria para consultas de todos los nodos. Cerca del máximo, las consultas nuevas esperan o se cancelan por falta de memoria.",
            format!("GET /v1/cluster: reservedMemory; max: {cm} clustermemorybytes (or {gp} totaldistributedbytes)"),
        ),
        (false, QueryMem::Coordinator) => (
            "Memoria reservada por las consultas en el pool del coordinador, sobre su tamaño: sin el catálogo jmx no se ven los workers.",
            "GET /v1/status: memoryInfo pool (or pools.general) reservedBytes; max: maxBytes (coordinator only)".to_string(),
        ),
        (false, QueryMem::GeneralPool) => (
            "Memoria reservada por las consultas en el pool general de todo el cluster, sobre su tamaño en todos los nodos. Cerca del máximo, las consultas nuevas esperan o se cancelan por falta de memoria.",
            format!("{gp}: reserveddistributedbytes; max: totaldistributedbytes"),
        ),
        (false, _) => (
            "Memoria que tienen reservada las consultas en todo el cluster, sobre la memoria para consultas de todos los nodos. Cerca del máximo, las consultas nuevas esperan o se cancelan por falta de memoria.",
            format!("{cm}: clustertotalmemoryreservation; max: clustermemorybytes"),
        ),
    };
    let running_src = if from_cluster("running") {
        "GET /v1/cluster: runningQueries (fully blocked ones not included)".to_string()
    } else if o.counted {
        "system.runtime.queries (latest 1000, without the monitor's): state RUNNING or FINISHING"
            .to_string()
    } else {
        format!("{qm}: runningqueries, minus the monitor's own query")
    };
    let queued_src = if from_cluster("queued") {
        "GET /v1/cluster: queuedQueries".to_string()
    } else if o.counted {
        "system.runtime.queries (latest 1000): state QUEUED, WAITING_FOR_RESOURCES or DISPATCHING"
            .to_string()
    } else {
        format!("{qm}: queuedqueries")
    };
    let pick = |k: &str, cluster: &str, jmx: &str| {
        if from_cluster(k) {
            format!("GET /v1/cluster: {cluster}")
        } else {
            format!("{qm}: {jmx}")
        }
    };
    // Presto's /v1/cluster counts as queries go; the JMX totals, when each ends.
    let (rows_help, bytes_help) = if from_cluster("rows_read") {
        (
            "Filas que las consultas leyeron de los conectores (Hive, Iceberg, otras bases…). Se cuentan a medida que avanzan las consultas.",
            "Datos que las consultas leyeron de los conectores. Se cuentan a medida que avanzan las consultas.",
        )
    } else {
        (
            "Filas que las consultas leyeron de los conectores (Hive, Iceberg, otras bases…). Se cuentan cuando termina cada consulta, así que una consulta larga aparece de golpe al final.",
            "Datos que las consultas leyeron de los conectores. Se cuentan cuando termina cada consulta, así que una consulta larga aparece de golpe al final.",
        )
    };
    let (nodes_help, nodes_src) = (
        if g("nodes").is_some() {
            "Nodos activos del cluster, el coordinador incluido. Si baja, un worker se cayó o se está apagando; el estado de cada uno está en la tabla de nodos."
        } else {
            "Workers activos del cluster según el coordinador. Si baja, un worker se cayó o se está apagando."
        },
        if g("nodes").is_some() {
            "system.runtime.nodes: rows with state = 'active'"
        } else {
            "GET /v1/cluster: activeWorkers"
        },
    );
    vec![
        Metric::new(
            "cpu",
            "CPU del cluster (promedio de nodos)",
            "CPU",
            MetricUnit::Percent,
            pct("cpu"),
        )
        .max(Some(100.0))
        .help(cpu_help)
        .source(cpu_src),
        Metric::new(
            "cpu_time",
            "CPU de los procesos",
            "CPU",
            MetricUnit::Percent,
            g("cpu_time"),
        )
        .max(Some(100.0))
        .counter()
        .help("Tiempo de CPU que consumen los procesos del motor en todos los nodos, como parte de todos sus núcleos: 100 % es todos ocupados. Comparado con el CPU del cluster, muestra cuánto de esa carga es del motor.")
        .source("jmx.current.\"java.lang:type=operatingsystem\": processcputime (ns), each node's increase since the previous read, summed ÷ 1e9 × 100 ÷ those nodes' availableprocessors (summed)"),
        query_cpu,
        Metric::new(
            "mem_used",
            "Heap usado (JVM)",
            "Memoria",
            MetricUnit::Bytes,
            g("heap"),
        )
        .max(g("heap_max"))
        .help(heap_help)
        .source(heap_src),
        Metric::new(
            "query_memory",
            "Memoria reservada por consultas",
            "Memoria",
            MetricUnit::Bytes,
            g("query_mem"),
        )
        .max(g("query_mem_max"))
        .help(mem_help)
        .source(&mem_src),
        Metric::new(
            "active_sessions",
            "Consultas en ejecución",
            "Conexiones",
            MetricUnit::Count,
            g("running"),
        )
        .help("Consultas que se están ejecutando ahora en el cluster, sin la del monitor.")
        .source(&running_src),
        Metric::new(
            "queued_queries",
            "Consultas en cola",
            "Conexiones",
            MetricUnit::Count,
            g("queued"),
        )
        .help("Consultas que esperan para empezar, por los límites de los grupos de recursos o por falta de memoria. Si no vuelve a cero, el cluster está saturado o un grupo de recursos queda chico.")
        .source(&queued_src),
        Metric::new(
            "blocked_queries",
            "Consultas bloqueadas",
            "Conexiones",
            MetricUnit::Count,
            g("blocked"),
        )
        .help("Consultas en ejecución que están del todo bloqueadas, esperando datos de un conector, memoria o el resultado de otra etapa. Muchas a la vez suelen indicar una fuente lenta o falta de memoria.")
        .source(&pick("blocked", "blockedQueries", "fullyblockedqueries")),
        Metric::new(
            "queries",
            "Consultas iniciadas",
            "Actividad",
            MetricUnit::Count,
            g("started"),
        )
        .counter()
        .help("Consultas que empezaron en el cluster, del total que lleva el coordinador desde que arrancó. Incluye las del propio monitor, que hace varias en cada lectura.")
        .source(&format!("{qm}: startedqueries.totalcount")),
        Metric::new(
            "failed_queries",
            "Consultas fallidas",
            "Actividad",
            MetricUnit::Count,
            g("failed"),
        )
        .counter()
        .help("Consultas que fallaron, por un error en la consulta, en un conector o por falta de recursos. El error de cada una está en la tabla de consultas recientes.")
        .source(&format!("{qm}: failedqueries.totalcount")),
        Metric::new(
            "rows_read",
            "Filas leídas",
            "Actividad",
            MetricUnit::Count,
            g("rows_read"),
        )
        .counter()
        .help(rows_help)
        .source(&pick("rows_read", "totalInputRows (finished queries plus the running ones)", "consumedinputrows.totalcount")),
        Metric::new(
            "bytes_read",
            "Datos leídos",
            "Actividad",
            MetricUnit::Bytes,
            g("bytes_read"),
        )
        .counter()
        .help(bytes_help)
        .source(&pick("bytes_read", "totalInputBytes (finished queries plus the running ones)", "consumedinputbytes.totalcount")),
        Metric::new(
            "running_drivers",
            "Drivers en ejecución",
            "Actividad",
            MetricUnit::Count,
            g("drivers"),
        )
        .help("Drivers (las unidades que procesan cada split de una tarea) corriendo ahora en el cluster: es el paralelismo en uso y crece con la carga.")
        .source(&pick("drivers", "runningDrivers", "runningdrivers")),
        Metric::new(
            "nodes",
            "Nodos activos",
            "Cluster",
            MetricUnit::Count,
            g("nodes").or(g("workers")),
        )
        .help(nodes_help)
        .source(nodes_src),
        Metric::new(
            "oom_kills",
            "Consultas terminadas por falta de memoria",
            "Memoria",
            MetricUnit::Count,
            g("oom_kills"),
        )
        .help("Consultas que el coordinador canceló para liberar memoria del cluster, en total desde que arrancó. Si sube, faltan memoria o nodos, o hay consultas que reservan demasiado.")
        .source(&format!("{cm}: querieskilledduetooutofmemory")),
        Metric::new(
            "uptime",
            "Tiempo activo del coordinador",
            "Servidor",
            MetricUnit::Seconds,
            g("uptime"),
        )
        .help("Tiempo desde que arrancó el coordinador. Si vuelve a cero, se reinició.")
        .source("GET /v1/info: uptime"),
    ]
}

impl TrinoSession {
    pub(crate) async fn get_json(&self, path: &str) -> Result<Value> {
        let rb = self
            .conn
            .http
            .get(format!("{}{path}", self.conn.base))
            .header(self.flavor.header("user"), &self.conn.user);
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
            t.columns
                .entry(r[0].clone())
                .or_default()
                .insert(r[1].clone());
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
    async fn jmx_values(
        &mut self,
        t: &JmxTables,
        table: Option<&str>,
        cols: &[(&str, &str)],
    ) -> Vec<Option<f64>> {
        let none = vec![None; cols.len()];
        let Some(table) = table else { return none };
        let have = t.columns.get(table);
        let sel = cols
            .iter()
            .map(|(agg, c)| {
                if have.is_some_and(|h| h.contains(*c)) {
                    format!("{agg}({})", q(c))
                } else {
                    "NULL".into()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        match self
            .strings(&format!("{TAG} SELECT {sel} FROM jmx.current.{}", q(table)))
            .await
        {
            Ok(rows) => rows
                .first()
                .map_or(none, |r| r.iter().map(|v| num(v)).collect()),
            Err(e) => {
                tracing::debug!("jmx {table}: {e}");
                none
            }
        }
    }

    pub(crate) async fn snapshot(&mut self) -> Result<MonitorSnapshot> {
        let mut snap = MonitorSnapshot::default();
        let mut m: HashMap<&'static str, f64> = HashMap::new();
        let mut origin = Origin::default();

        // The coordinator itself.
        let info = self.get_json("/v1/info").await?;
        let version = info
            .pointer("/nodeVersion/version")
            .map(text)
            .unwrap_or_default();
        snap.info.push((
            "Versión".into(),
            format!("{} {version}", crate::info_name(self.flavor)),
        ));
        if let Some(e) = info.get("environment").map(text) {
            snap.info.push(("Entorno".into(), e));
        }
        if let Some(st) = info.get("state").map(text).filter(|s| !s.is_empty()) {
            snap.info.push(("Estado del coordinador".into(), st));
        }
        if let Some(up) = info
            .get("uptime")
            .and_then(Value::as_str)
            .and_then(duration_secs)
        {
            m.insert("uptime", up);
        }
        let status = self.get_json("/v1/status").await.ok();
        if let Some(st) = &status {
            if let Some(n) = st.get("nodeId").map(text) {
                snap.info.push(("Nodo coordinador".into(), n));
            }
            if let Some(p) = f(st, "/processors") {
                snap.info
                    .push(("Procesadores del coordinador".into(), p.to_string()));
            }
        }

        // Presto's cluster totals (Trino moved them to the web UI's API,
        // behind the UI login).
        let cluster = if self.flavor == Flavor::Presto {
            self.get_json("/v1/cluster").await.ok()
        } else {
            None
        };
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
                    origin.cluster.insert(k);
                }
            }
        }

        // JMX: every node's OS and heap, the query manager's totals.
        type Triple = (Option<f64>, Option<f64>, Option<f64>);
        let mut node_os: HashMap<String, Triple> = HashMap::new();
        match self.jmx_tables().await {
            Ok(t) => {
                origin.jmx = JmxTables {
                    columns: HashMap::new(),
                    ..t.clone()
                };
                let os = self
                    .strings(&format!(
                        "{TAG} SELECT o.node, o.systemcpuload, o.processcputime, m.heapmemoryusage, o.availableprocessors
                         FROM jmx.current.\"java.lang:type=operatingsystem\" o
                         LEFT JOIN jmx.current.\"java.lang:type=memory\" m ON m.node = o.node"
                    ))
                    .await
                    .unwrap_or_default();
                let (mut cpu_sum, mut cpu_n, mut heap, mut heap_max) = (0.0, 0, None::<f64>, None::<f64>);
                // Each node's process CPU (ns) and the cores it is a share of.
                let mut cpu_time: Vec<(String, f64, f64)> = Vec::new();
                for r in os.iter().filter(|r| r.len() == 5) {
                    let load = num(&r[1]).filter(|l| *l >= 0.0);
                    if let Some(l) = load {
                        cpu_sum += l;
                        cpu_n += 1;
                    }
                    if let (Some(ns), Some(p)) = (num(&r[2]), num(&r[4]).filter(|p| *p > 0.0)) {
                        cpu_time.push((r[0].clone(), ns, p));
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
                // A share of every node's cores: 100 = all of them busy.
                if let Some(v) = self.cpu.process(&cpu_time) {
                    m.insert("cpu_time", v);
                    m.insert("cpus", cpu_time.iter().map(|n| n.2).sum());
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
                if !m.contains_key("query_mem") {
                    if cm[0].is_some() {
                        origin.query_mem = QueryMem::Cluster;
                    } else if gp[0].is_some() {
                        origin.query_mem = QueryMem::GeneralPool;
                    }
                }
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
                    origin.cpu_coordinator = true;
                }
            }
            if !m.contains_key("heap") {
                if let Some(h) = f(st, "/heapUsed") {
                    m.insert("heap", h);
                    origin.heap_coordinator = true;
                }
                if let Some(h) = f(st, "/heapAvailable") {
                    m.insert("heap_max", h);
                }
            }
            if !m.contains_key("query_mem") {
                // Trino has one pool; Presto, "general" and "reserved".
                let pool = |k: &str| {
                    f(st, &format!("/memoryInfo/pool/{k}"))
                        .or_else(|| f(st, &format!("/memoryInfo/pools/general/{k}")))
                };
                if let (Some(r), Some(x)) = (pool("reservedBytes"), pool("maxBytes")) {
                    m.insert("query_mem", r);
                    m.insert("query_mem_max", x);
                    origin.query_mem = QueryMem::Coordinator;
                }
            }
        }

        // Query CPU over the cluster's cores, interval by interval.
        let cores = m.get("cpus").map(|c| (*c, Cores::Os)).or_else(|| {
            m.get("processors")
                .filter(|c| **c > 0.0)
                .map(|c| (*c, Cores::ClusterMemory))
        });
        let (query_cpu, from) = self.cpu.query(
            m.remove("query_cpu"),
            origin.cluster.contains("query_cpu"),
            cores,
        );
        if let Some(v) = query_cpu {
            m.insert("query_cpu", v);
        }
        origin.query_cores = from;

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
                let active = nodes
                    .iter()
                    .filter(|r| r.get(4).is_some_and(|s| s.eq_ignore_ascii_case("active")))
                    .count();
                m.insert("nodes", active as f64);
                let mut t = MonitorTable::new(
                    "nodes",
                    "Nodos del cluster",
                    &[
                        "Nodo",
                        "URI",
                        "Versión",
                        "Rol",
                        "Estado",
                        "CPU (%)",
                        "Heap usado",
                        "Heap máximo",
                        "Tareas",
                        "Splits en curso",
                        "Splits en cola",
                    ],
                );
                for r in nodes.iter().filter(|r| r.len() == 5).take(200) {
                    let (cpu, used, max) = node_os.get(&r[0]).copied().unwrap_or_default();
                    let tk = tasks.get(&r[0]);
                    let tk = |i: usize| tk.and_then(|v| v.get(i)).map_or(json!(0), |s| cell_num(s));
                    t.rows.push(vec![
                        cell(&r[0]),
                        cell(&r[1]),
                        cell(&r[2]),
                        json!(if r[3] == "true" {
                            "coordinador"
                        } else {
                            "worker"
                        }),
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
            Err(e) => snap
                .notes
                .push(format!("No se pudo leer system.runtime.nodes: {e}")),
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
                    origin.counted = true;
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

        snap.metrics = metrics(&m, &origin);
        let g = |k: &str| m.get(k).copied();
        if let Some(p) = g("processors") {
            snap.info
                .push(("Procesadores del cluster".into(), p.to_string()));
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
        assert_eq!(
            t.query_manager.as_deref(),
            Some("com.facebook.presto.execution:name=querymanager")
        );
        assert!(t.cluster_memory.is_some() && t.general_pool.is_none());
        assert_eq!(q("a\"b"), "\"a\"\"b\"");
    }

    fn described(list: &[Metric]) {
        assert_eq!(list.len(), 16);
        for m in list {
            assert!(
                m.help.as_deref().is_some_and(|h| !h.is_empty())
                    && m.source.as_deref().is_some_and(|s| !s.is_empty()),
                "{} has no help or source",
                m.key
            );
        }
    }

    fn get<'a>(list: &'a [Metric], k: &str) -> &'a Metric {
        list.iter().find(|m| m.key == k).unwrap()
    }

    fn node(n: &str, secs: f64, cores: f64) -> (String, f64, f64) {
        (n.to_string(), secs * 1e9, cores)
    }

    #[test]
    fn process_cpu_adds_each_interval_over_its_cores() {
        let mut t = CpuTotals::default();
        // 3 nodes of 8 cores; c is nearly idle.
        let first = t.process(&[
            node("a", 100_000.0, 8.0),
            node("b", 100_000.0, 8.0),
            node("c", 10.0, 8.0),
        ]);
        assert_eq!(first, Some(0.0));
        // One second later, a and b used 4 cores each: 8 of 24 busy.
        let v = t.process(&[
            node("a", 100_004.0, 8.0),
            node("b", 100_004.0, 8.0),
            node("c", 10.0, 8.0),
        ]);
        assert!((v.unwrap() - 800.0 / 24.0).abs() < 1e-9);
        // c leaves: the next second is 8 of the 16 cores left, not the
        // jump the whole total over 16 cores would make.
        let before = v.unwrap();
        let v = t
            .process(&[node("a", 100_008.0, 8.0), node("b", 100_008.0, 8.0)])
            .unwrap();
        assert!((v - before - 50.0).abs() < 1e-9);
        // A failed read gives nothing; the next one covers both intervals.
        assert_eq!(t.process(&[]), None);
        let w = t
            .process(&[node("a", 100_016.0, 8.0), node("b", 100_016.0, 8.0)])
            .unwrap();
        assert!((w - v - 100.0).abs() < 1e-9);
        // A node that joins counts from its second read; one that
        // restarted counts all of its CPU since then.
        let x = t
            .process(&[
                node("a", 2.0, 8.0),
                node("b", 100_020.0, 8.0),
                node("d", 5_000.0, 8.0),
            ])
            .unwrap();
        assert!((x - w - 600.0 / 16.0).abs() < 1e-9);
    }

    #[test]
    fn query_cpu_adds_each_interval_over_the_last_known_cores() {
        let mut t = CpuTotals::default();
        let os = Some((24.0, Cores::Os));
        assert_eq!(
            t.query(Some(1000.0), false, os),
            (Some(0.0), Some(Cores::Os))
        );
        assert_eq!(
            t.query(Some(1012.0), false, os),
            (Some(50.0), Some(Cores::Os))
        );
        // The OS read failed: same cores, same unit.
        assert_eq!(
            t.query(Some(1024.0), false, None),
            (Some(100.0), Some(Cores::Os))
        );
        // A node left: fewer cores for the next interval, no jump.
        let fewer = Some((16.0, Cores::Os));
        assert_eq!(
            t.query(Some(1032.0), false, fewer),
            (Some(150.0), Some(Cores::Os))
        );
        // A dip or a change of source adds nothing.
        assert_eq!(
            t.query(Some(1030.0), false, fewer),
            (Some(150.0), Some(Cores::Os))
        );
        assert_eq!(
            t.query(Some(9000.0), true, fewer),
            (Some(150.0), Some(Cores::Os))
        );
        assert_eq!(t.query(None, true, fewer), (None, Some(Cores::Os)));

        // No cores yet: CPU seconds; once they are known, % from zero.
        let mut t = CpuTotals::default();
        assert_eq!(t.query(Some(500.0), true, None), (Some(0.0), None));
        assert_eq!(t.query(Some(530.0), true, None), (Some(30.0), None));
        assert_eq!(
            t.query(Some(536.0), true, os),
            (Some(25.0), Some(Cores::Os))
        );
    }

    #[test]
    fn query_cpu_is_a_share_of_every_core() {
        // Trino with the jmx catalog: 3 nodes of 8 cores.
        let m: HashMap<&str, f64> = [
            ("cpu", 40.0),
            ("cpu_time", 37.5),
            ("cpus", 24.0),
            ("query_cpu", 25.0),
            ("running", 2.0),
            ("nodes", 3.0),
        ]
        .into_iter()
        .collect();
        let o = Origin {
            jmx: JmxTables::from_names(&["trino.execution:name=querymanager".into()]),
            query_cores: Some(Cores::Os),
            ..Default::default()
        };
        let list = metrics(&m, &o);
        described(&list);
        let q = get(&list, "query_cpu");
        assert_eq!(
            (q.unit, q.value, q.max, q.counter),
            (MetricUnit::Percent, Some(25.0), Some(100.0), true)
        );
        // Out of JMX it only grows as each query ends: the help says so.
        assert!(q
            .help
            .as_deref()
            .unwrap()
            .contains("segundos enteros de CPU cuando termina"));
        assert!(q.source.as_deref().unwrap().contains("availableprocessors"));
        let c = get(&list, "cpu_time");
        assert_eq!(
            (c.unit, c.max, c.counter),
            (MetricUnit::Percent, Some(100.0), true)
        );
        assert!(get(&list, "active_sessions")
            .source
            .as_deref()
            .unwrap()
            .starts_with("jmx.current.\"trino.execution:name=querymanager\""));
    }

    #[test]
    fn without_jmx_the_texts_say_coordinator_only() {
        let m: HashMap<&str, f64> = [
            ("cpu", 10.0),
            ("heap", 1.0),
            ("query_cpu", 5.0),
            ("running", 1.0),
        ]
        .into_iter()
        .collect();
        let o = Origin {
            cpu_coordinator: true,
            heap_coordinator: true,
            query_mem: QueryMem::Coordinator,
            counted: true,
            ..Default::default()
        };
        let list = metrics(&m, &o);
        described(&list);
        assert!(get(&list, "cpu")
            .source
            .as_deref()
            .unwrap()
            .contains("/v1/status"));
        assert!(get(&list, "query_memory")
            .source
            .as_deref()
            .unwrap()
            .contains("/v1/status"));
        assert!(get(&list, "active_sessions")
            .source
            .as_deref()
            .unwrap()
            .starts_with("system.runtime.queries"));
        // No core count: CPU seconds, never a share of one core.
        let q = get(&list, "query_cpu");
        assert_eq!(
            (q.unit, q.max, q.counter),
            (MetricUnit::Seconds, None, true)
        );
    }

    #[test]
    fn presto_query_cpu_from_the_cluster_endpoint_grows_as_queries_go() {
        let m: HashMap<&str, f64> = [("query_cpu", 12.0)].into_iter().collect();
        let mut o = Origin {
            cluster: ["query_cpu"].into_iter().collect(),
            query_cores: Some(Cores::ClusterMemory),
            ..Default::default()
        };
        let list = metrics(&m, &o);
        described(&list);
        let q = get(&list, "query_cpu");
        assert_eq!((q.unit, q.max), (MetricUnit::Percent, Some(100.0)));
        assert!(!q.help.as_deref().unwrap().contains("cuando termina"));
        assert!(q
            .source
            .as_deref()
            .unwrap()
            .contains("totalavailableprocessors"));
        o.query_cores = None;
        let list = metrics(&m, &o);
        let q = get(&list, "query_cpu");
        assert_eq!(q.unit, MetricUnit::Seconds);
        assert!(q.help.as_deref().unwrap().contains("sin el catálogo jmx"));
    }

    #[test]
    fn presto_cluster_figures_say_so() {
        let m: HashMap<&str, f64> = [
            ("running", 1.0),
            ("query_mem", 9.0),
            ("rows_read", 5.0),
            ("workers", 2.0),
        ]
        .into_iter()
        .collect();
        let o = Origin {
            cluster: ["running", "query_mem", "rows_read", "workers"]
                .into_iter()
                .collect(),
            ..Default::default()
        };
        let list = metrics(&m, &o);
        described(&list);
        for k in ["active_sessions", "query_memory", "rows_read", "nodes"] {
            assert!(
                get(&list, k)
                    .source
                    .as_deref()
                    .unwrap()
                    .starts_with("GET /v1/cluster"),
                "{k}"
            );
        }
        assert_eq!(get(&list, "nodes").value, Some(2.0));
    }
}
