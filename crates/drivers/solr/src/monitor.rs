//! `Session::monitor` for Solr: `/admin/info/system` (CPU, memory, JVM
//! heap, uptime, versions), `/admin/metrics` (GC, threads, Jetty requests,
//! per-core requests, caches, index sizes, disk), `/admin/cores?action=STATUS`
//! (cores and sizes) and, on SolrCloud, `/admin/collections?action=CLUSTERSTATUS`
//! (collections, live nodes, replicas). A refused endpoint leaves a note.

use dbine_driver::monitor::{Metric, MetricUnit, MonitorSnapshot, MonitorTable};
use dbine_driver_elasticsearch::json::J;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const MAX_ROWS: usize = 200;

pub(crate) const NODE_METRICS: &str = "/solr/admin/metrics?wt=json&group=jvm,node,jetty\
&prefix=gc.,threads.count,CONTAINER.fs.,CONTAINER.cores.,org.eclipse.jetty.server.handler.DefaultHandler.";
pub(crate) const CORE_METRICS: &str = "/solr/admin/metrics?wt=json&group=core\
&prefix=QUERY./select.requests,QUERY./select.errors,UPDATE./update.requests,INDEX.sizeInBytes,CACHE.searcher.";

fn f(j: &J, path: &[&str]) -> Option<f64> {
    match j.at(path)? {
        J::Num(n) => n.as_f64(),
        // Meters and timers: their running count.
        J::Obj(_) => f(j, &[path, &["count"]].concat()),
        J::Str(s) => dbine_driver::monitor::num(s),
        _ => None,
    }
}

fn opt(v: Option<f64>) -> Value {
    v.map(|x| json!(x)).unwrap_or(Value::Null)
}

fn cell(j: Option<&J>) -> Value {
    j.map(J::cell).unwrap_or(Value::Null)
}

pub(crate) fn from_system(s: &mut MonitorSnapshot, sys: &J) {
    let os = |k: &str| f(sys, &["system", k]);
    let cpu = os("cpuLoad").or_else(|| os("systemCpuLoad")).filter(|v| *v >= 0.0);
    s.metrics.push(Metric::new("cpu", "CPU del servidor", "CPU", MetricUnit::Percent, cpu.map(|c| c * 100.0)).max(Some(100.0)));
    s.metrics.push(
        Metric::new("cpu_time", "CPU del proceso", "CPU", MetricUnit::Percent, os("processCpuTime").map(|ns| ns / 1e9 * 100.0)).counter(),
    );
    if let Some(l) = os("systemLoadAverage").filter(|l| *l >= 0.0) {
        s.metrics.push(Metric::new("load_1m", "Carga (1 min)", "CPU", MetricUnit::Count, Some(l)));
    }
    let total = os("totalPhysicalMemorySize").or_else(|| os("totalMemorySize"));
    let free = os("freePhysicalMemorySize").or_else(|| os("freeMemorySize"));
    let used = total.zip(free).map(|(t, f)| t - f);
    s.metrics.push(Metric::new("mem_used", "Memoria usada", "Memoria", MetricUnit::Bytes, used).max(total));
    s.metrics.push(
        Metric::new("heap_used", "Heap del JVM", "Memoria", MetricUnit::Bytes, f(sys, &["jvm", "memory", "raw", "used"]))
            .max(f(sys, &["jvm", "memory", "raw", "max"])),
    );
    if let Some(n) = os("openFileDescriptorCount") {
        s.metrics.push(Metric::new("open_files", "Descriptores abiertos", "Servidor", MetricUnit::Count, Some(n)).max(os("maxFileDescriptorCount")));
    }
    s.metrics.push(Metric::new("uptime", "Tiempo activo", "Servidor", MetricUnit::Seconds, f(sys, &["jvm", "jmx", "upTimeMS"]).map(|ms| ms / 1000.0)));

    let mode = sys.get("mode").map(J::text).unwrap_or_default();
    s.info.push(("Modo".into(), if mode == "solrcloud" { "SolrCloud".into() } else { "independiente".into() }));
    for (path, label) in [
        (&["lucene", "solr-spec-version"][..], "Versión de Solr"),
        (&["lucene", "lucene-spec-version"][..], "Versión de Lucene"),
        (&["jvm", "name"][..], "JVM"),
        (&["jvm", "version"][..], "Versión de Java"),
        (&["jvm", "processors"][..], "Procesadores"),
        (&["jvm", "memory", "max"][..], "Heap máximo"),
        (&["system", "name"][..], "Sistema operativo"),
        (&["zkHost"][..], "ZooKeeper"),
        (&["node"][..], "Nodo"),
        (&["solr_home"][..], "Directorio de Solr"),
    ] {
        if let Some(v) = sys.at(path).map(J::text).filter(|v| !v.is_empty()) {
            s.info.push((label.into(), v));
        }
    }
}

pub(crate) fn from_node_metrics(s: &mut MonitorSnapshot, m: &J) {
    let jvm = m.at(&["metrics", "solr.jvm"]);
    let node = m.at(&["metrics", "solr.node"]);
    let jetty = m.at(&["metrics", "solr.jetty"]);
    if let Some(jvm) = jvm.and_then(J::as_obj) {
        let gc: Vec<f64> = jvm.iter().filter(|(k, _)| k.starts_with("gc.") && k.ends_with(".time")).filter_map(|(_, v)| f(v, &[])).collect();
        if !gc.is_empty() {
            s.metrics.push(Metric::new("gc_time", "Tiempo de GC", "Memoria", MetricUnit::Millis, Some(gc.iter().sum())).counter());
        }
    }
    if let Some(t) = jvm.and_then(|j| f(j, &["threads.count"])) {
        s.metrics.push(Metric::new("threads", "Hilos del JVM", "Servidor", MetricUnit::Count, Some(t)));
    }
    if let Some(jetty) = jetty {
        let h = |k: &str| f(jetty, &[&format!("org.eclipse.jetty.server.handler.DefaultHandler.{k}")]);
        s.metrics.push(Metric::new("active_sessions", "Pedidos en curso", "Conexiones", MetricUnit::Count, h("active-requests")));
        s.metrics.push(Metric::new("http_requests", "Pedidos HTTP", "Actividad", MetricUnit::Count, h("requests")).counter());
        if let Some(e) = h("5xx-responses") {
            s.metrics.push(Metric::new("http_5xx", "Respuestas 5xx", "Actividad", MetricUnit::Count, Some(e)).counter());
        }
        if let Some(e) = h("4xx-responses") {
            s.metrics.push(Metric::new("http_4xx", "Respuestas 4xx", "Actividad", MetricUnit::Count, Some(e)).counter());
        }
    }
    if let Some(node) = node {
        let total = f(node, &["CONTAINER.fs.totalSpace"]);
        let usable = f(node, &["CONTAINER.fs.usableSpace"]);
        s.metrics.push(Metric::new("disk_used", "Disco usado", "Almacenamiento", MetricUnit::Bytes, total.zip(usable).map(|(t, u)| t - u)).max(total));
        if let Some(c) = f(node, &["CONTAINER.cores.loaded"]) {
            s.metrics.push(Metric::new("cores", "Cores cargados", "Servidor", MetricUnit::Count, Some(c)));
        }
    }
}

pub(crate) fn from_core_metrics(s: &mut MonitorSnapshot, m: &J) {
    let cores: Vec<&J> = m.get("metrics").and_then(J::as_obj).map(|o| o.iter().map(|(_, v)| v).collect()).unwrap_or_default();
    let sum = |k: &str| -> Option<f64> {
        let v: Vec<f64> = cores.iter().filter_map(|c| f(c, &[k])).collect();
        (!v.is_empty()).then(|| v.iter().sum())
    };
    s.metrics.push(Metric::new("queries", "Consultas (/select)", "Actividad", MetricUnit::Count, sum("QUERY./select.requests")).counter());
    s.metrics.push(Metric::new("rows_written", "Actualizaciones (/update)", "Actividad", MetricUnit::Count, sum("UPDATE./update.requests")).counter());
    s.metrics.push(Metric::new("query_errors", "Consultas con error", "Actividad", MetricUnit::Count, sum("QUERY./select.errors")).counter());
    s.metrics.push(Metric::new("storage_used", "Tamaño de los índices", "Almacenamiento", MetricUnit::Bytes, sum("INDEX.sizeInBytes")));

    let cache = |name: &str, field: &str| -> Option<f64> {
        let v: Vec<f64> = cores.iter().filter_map(|c| f(c, &[&format!("CACHE.searcher.{name}"), field])).collect();
        (!v.is_empty()).then(|| v.iter().sum())
    };
    let hit = |name: &str| match (cache(name, "cumulative_hits"), cache(name, "cumulative_lookups")) {
        (Some(h), Some(l)) if l > 0.0 => Some(h / l * 100.0),
        _ => None,
    };
    s.metrics.push(Metric::new("cache_hit", "Aciertos de queryResultCache", "Caché", MetricUnit::Percent, hit("queryResultCache")).max(Some(100.0)));
    s.metrics.push(Metric::new("filter_cache_hit", "Aciertos de filterCache", "Caché", MetricUnit::Percent, hit("filterCache")).max(Some(100.0)));
    s.metrics.push(Metric::new("document_cache_hit", "Aciertos de documentCache", "Caché", MetricUnit::Percent, hit("documentCache")).max(Some(100.0)));
    let ram: Vec<f64> = ["queryResultCache", "filterCache", "documentCache", "fieldValueCache", "perSegFilter"]
        .iter()
        .filter_map(|c| cache(c, "ramBytesUsed"))
        .collect();
    if !ram.is_empty() {
        s.metrics.push(Metric::new("mem_cache", "Cachés del searcher", "Memoria", MetricUnit::Bytes, Some(ram.iter().sum())));
    }
    let ev: Vec<f64> = ["queryResultCache", "filterCache", "documentCache"].iter().filter_map(|c| cache(c, "cumulative_evictions")).collect();
    if !ev.is_empty() {
        s.metrics.push(Metric::new("evictions", "Desalojos de caché", "Caché", MetricUnit::Count, Some(ev.iter().sum())).counter());
    }
}

/// `/admin/cores?action=STATUS` → cores and sizes, largest first.
pub(crate) fn cores(s: &mut MonitorSnapshot, st: &J) {
    let mut rows: Vec<(f64, Vec<Value>)> = st
        .get("status")
        .and_then(J::as_obj)
        .into_iter()
        .flatten()
        .map(|(name, c)| {
            let size = f(c, &["index", "sizeInBytes"]).unwrap_or(0.0);
            (
                size,
                vec![
                    Value::String(name.clone()),
                    cell(c.at(&["cloud", "collection"])),
                    opt(f(c, &["index", "numDocs"])),
                    opt(f(c, &["index", "deletedDocs"])),
                    opt(f(c, &["index", "segmentCount"])),
                    json!(size),
                    opt(f(c, &["uptime"]).map(|ms| ms / 1000.0)),
                    cell(c.get("startTime")),
                ],
            )
        })
        .collect();
    let docs: f64 = rows.iter().filter_map(|(_, r)| r[2].as_f64()).sum();
    s.metrics.push(Metric::new("docs", "Documentos", "Almacenamiento", MetricUnit::Count, Some(docs)));
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut t = MonitorTable::new(
        "databases",
        "Cores y tamaños",
        &["core", "colección", "documentos", "borrados", "segmentos", "tamaño", "activo (s)", "inicio"],
    );
    t.rows = rows.into_iter().take(MAX_ROWS).map(|(_, r)| r).collect();
    s.tables.push(t);
    if let Some(fails) = st.get("initFailures").and_then(J::as_obj).filter(|f| !f.is_empty()) {
        for (core, e) in fails {
            s.notes.push(format!("El core {core} no pudo iniciarse: {}", dbine_driver_elasticsearch::http::clip(&e.text(), 300)));
        }
    }
}

/// `CLUSTERSTATUS` → collections, live nodes and replicas.
pub(crate) fn cluster(s: &mut MonitorSnapshot, cs: &J) {
    let live: Vec<String> = cs.at(&["cluster", "live_nodes"]).and_then(J::as_arr).unwrap_or(&[]).iter().map(J::text).collect();
    let mut per_node: BTreeMap<String, (usize, usize)> = live.iter().map(|n| (n.clone(), (0, 0))).collect();
    let mut cols = MonitorTable::new("collections", "Colecciones", &["colección", "salud", "shards", "réplicas", "configset", "router"]);
    let mut reps = MonitorTable::new("replication", "Réplicas", &["colección", "shard", "réplica", "core", "nodo", "tipo", "estado", "líder"]);
    let mut down = 0usize;
    for (name, c) in cs.at(&["cluster", "collections"]).and_then(J::as_obj).into_iter().flatten() {
        let shards = c.get("shards").and_then(J::as_obj).map(Vec::as_slice).unwrap_or(&[]);
        let mut n_reps = 0;
        for (shard, sh) in shards {
            for (rid, r) in sh.get("replicas").and_then(J::as_obj).into_iter().flatten() {
                n_reps += 1;
                let node = r.get("node_name").map(J::text).unwrap_or_default();
                let active = r.get("state").and_then(J::as_str) == Some("active");
                if !active {
                    down += 1;
                }
                let e = per_node.entry(node.clone()).or_default();
                e.0 += 1;
                if r.get("leader").map(J::text).as_deref() == Some("true") {
                    e.1 += 1;
                }
                if reps.rows.len() < MAX_ROWS {
                    reps.rows.push(vec![
                        Value::String(name.clone()),
                        Value::String(shard.clone()),
                        Value::String(rid.clone()),
                        cell(r.get("core")),
                        Value::String(node),
                        cell(r.get("type")),
                        cell(r.get("state")),
                        Value::Bool(r.get("leader").map(J::text).as_deref() == Some("true")),
                    ]);
                }
            }
        }
        if cols.rows.len() < MAX_ROWS {
            cols.rows.push(vec![
                Value::String(name.clone()),
                cell(c.get("health")),
                json!(shards.len()),
                json!(n_reps),
                cell(c.get("configName")),
                cell(c.at(&["router", "name"])),
            ]);
        }
    }
    let mut nodes = MonitorTable::new("nodes", "Nodos del cluster", &["nodo", "estado", "réplicas", "líderes"]);
    for (n, (r, l)) in per_node.iter().take(MAX_ROWS) {
        let state = if live.contains(n) { "activo" } else { "caído" };
        nodes.rows.push(vec![Value::String(n.clone()), Value::String(state.into()), json!(r), json!(l)]);
    }
    s.metrics.push(Metric::new("live_nodes", "Nodos activos", "Cluster", MetricUnit::Count, Some(live.len() as f64)).max(Some(per_node.len() as f64)));
    s.metrics.push(Metric::new("collections", "Colecciones", "Cluster", MetricUnit::Count, Some(cols.rows.len() as f64)));
    s.metrics.push(Metric::new("replicas_down", "Réplicas no activas", "Replicación", MetricUnit::Count, Some(down as f64)));
    s.tables.push(nodes);
    s.tables.push(cols);
    s.tables.push(reps);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(s: &str) -> J {
        J::parse(s).unwrap()
    }

    #[test]
    fn system_and_metrics() {
        let mut s = MonitorSnapshot::default();
        from_system(&mut s, &j(r#"{"mode": "std", "lucene": {"solr-spec-version": "9.10.1"},
            "jvm": {"memory": {"raw": {"used": 10, "max": 100}}, "jmx": {"upTimeMS": 5000}},
            "system": {"cpuLoad": 0.25, "processCpuTime": 2000000000, "totalPhysicalMemorySize": 1000, "freePhysicalMemorySize": 400}}"#));
        from_core_metrics(&mut s, &j(r#"{"metrics": {
            "solr.core.a": {"QUERY./select.requests": 5, "INDEX.sizeInBytes": 100,
                            "CACHE.searcher.queryResultCache": {"cumulative_hits": 3, "cumulative_lookups": 4, "ramBytesUsed": 10}},
            "solr.core.b": {"QUERY./select.requests": {"count": 2}, "INDEX.sizeInBytes": 50}}}"#));
        let m = |k: &str| s.metrics.iter().find(|m| m.key == k).unwrap();
        assert_eq!(m("cpu").value, Some(25.0));
        assert_eq!(m("cpu_time").value, Some(200.0));
        assert_eq!(m("mem_used").value, Some(600.0));
        assert_eq!(m("uptime").value, Some(5.0));
        assert_eq!(m("queries").value, Some(7.0));
        assert_eq!(m("storage_used").value, Some(150.0));
        assert_eq!(m("cache_hit").value, Some(75.0));
    }

    #[test]
    fn cluster_status() {
        let mut s = MonitorSnapshot::default();
        cluster(&mut s, &j(r#"{"cluster": {"live_nodes": ["n1"], "collections": {"c": {"health": "GREEN", "configName": "x",
            "shards": {"shard1": {"replicas": {"r1": {"core": "c_s1_r1", "node_name": "n1", "state": "active", "leader": "true"},
                                                "r2": {"core": "c_s1_r2", "node_name": "n2", "state": "down"}}}}}}}}"#));
        let t = |k: &str| s.tables.iter().find(|t| t.key == k).unwrap();
        assert_eq!(t("nodes").rows.len(), 2);
        assert_eq!(t("nodes").rows[1][1], json!("caído"));
        assert_eq!(t("replication").rows.len(), 2);
        assert_eq!(s.metrics.iter().find(|m| m.key == "replicas_down").unwrap().value, Some(1.0));
    }
}
