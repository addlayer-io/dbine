//! `Session::monitor` for Elasticsearch, OpenSearch and Open Distro:
//! `_cluster/health`, `_nodes/stats` (CPU, memory, JVM heap, GC, HTTP
//! connections, search and indexing counters, caches, transport, disk),
//! `_cat/indices` (sizes), `_tasks` (running requests) and
//! `_cluster/pending_tasks`. Every figure is summed over the nodes (CPU
//! and load are averaged). A refused endpoint (a user without the
//! `monitor` cluster privilege, serverless projects) leaves a note.

use crate::json::J;
use dbine_driver::monitor::{Metric, MetricUnit, MonitorSnapshot, MonitorTable};
use serde_json::{json, Value};

const MAX_ROWS: usize = 200;

pub(crate) fn f(j: &J, path: &[&str]) -> Option<f64> {
    match j.at(path)? {
        J::Num(n) => n.as_f64(),
        J::Str(s) => dbine_driver::monitor::num(s),
        _ => None,
    }
}

fn nodes(stats: &J) -> Vec<&J> {
    stats.get("nodes").and_then(J::as_obj).map(|o| o.iter().map(|(_, n)| n).collect()).unwrap_or_default()
}

/// Sum of a figure over the nodes that report it.
fn total(nodes: &[&J], path: &[&str]) -> Option<f64> {
    let v: Vec<f64> = nodes.iter().filter_map(|n| f(n, path)).collect();
    (!v.is_empty()).then(|| v.iter().sum())
}

fn avg(nodes: &[&J], path: &[&str]) -> Option<f64> {
    let v: Vec<f64> = nodes.iter().filter_map(|n| f(n, path)).collect();
    (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64)
}

fn ratio(hits: Option<f64>, misses: Option<f64>) -> Option<f64> {
    match (hits, misses) {
        (Some(h), Some(m)) if h + m > 0.0 => Some(h / (h + m) * 100.0),
        _ => None,
    }
}

fn cell(j: Option<&J>) -> Value {
    j.map(J::cell).unwrap_or(Value::Null)
}

fn opt(v: Option<f64>) -> Value {
    v.map(|x| json!(x)).unwrap_or(Value::Null)
}

pub(crate) fn from_health(s: &mut MonitorSnapshot, h: &J) {
    for (k, label) in [("cluster_name", "Cluster"), ("status", "Estado del cluster")] {
        if let Some(v) = h.get(k).map(J::text) {
            let v = match v.as_str() {
                "green" => "verde".to_string(),
                "yellow" => "amarillo (hay réplicas sin asignar)".to_string(),
                "red" => "rojo (hay shards primarios sin asignar)".to_string(),
                _ => v,
            };
            s.info.push((label.into(), v));
        }
    }
    let nodes = f(h, &["number_of_nodes"]);
    s.metrics.push(Metric::new("nodes", "Nodos", "Cluster", MetricUnit::Count, nodes));
    s.metrics.push(Metric::new("data_nodes", "Nodos de datos", "Cluster", MetricUnit::Count, f(h, &["number_of_data_nodes"])));
    s.metrics.push(Metric::new("active_shards", "Shards activos", "Cluster", MetricUnit::Count, f(h, &["active_shards"])));
    s.metrics.push(Metric::new("unassigned_shards", "Shards sin asignar", "Cluster", MetricUnit::Count, f(h, &["unassigned_shards"])));
    s.metrics.push(Metric::new("relocating_shards", "Shards en movimiento", "Cluster", MetricUnit::Count, f(h, &["relocating_shards"])));
    s.metrics.push(Metric::new("initializing_shards", "Shards inicializando", "Cluster", MetricUnit::Count, f(h, &["initializing_shards"])));
    s.metrics.push(
        Metric::new("active_shards_percent", "Shards activos (%)", "Cluster", MetricUnit::Percent, f(h, &["active_shards_percent_as_number"]))
            .max(Some(100.0)),
    );
    s.metrics.push(Metric::new("pending_tasks", "Tareas pendientes del cluster", "Cluster", MetricUnit::Count, f(h, &["number_of_pending_tasks"])));
}

pub(crate) fn from_node_stats(s: &mut MonitorSnapshot, stats: &J) {
    let ns = nodes(stats);
    let n = ns.as_slice();

    // CPU.
    s.metrics.push(Metric::new("cpu", "CPU del servidor", "CPU", MetricUnit::Percent, avg(n, &["os", "cpu", "percent"])).max(Some(100.0)));
    let cpu_ms = total(n, &["process", "cpu", "total_in_millis"]);
    s.metrics.push(Metric::new("cpu_time", "CPU del proceso", "CPU", MetricUnit::Percent, cpu_ms.map(|ms| ms / 1000.0 * 100.0)).counter());
    if let Some(l) = avg(n, &["os", "cpu", "load_average", "1m"]) {
        s.metrics.push(Metric::new("load_1m", "Carga (1 min)", "CPU", MetricUnit::Count, Some(l)));
    }

    // Memory.
    s.metrics.push(
        Metric::new("mem_used", "Memoria usada", "Memoria", MetricUnit::Bytes, total(n, &["os", "mem", "used_in_bytes"]))
            .max(total(n, &["os", "mem", "total_in_bytes"])),
    );
    s.metrics.push(
        Metric::new("heap_used", "Heap del JVM", "Memoria", MetricUnit::Bytes, total(n, &["jvm", "mem", "heap_used_in_bytes"]))
            .max(total(n, &["jvm", "mem", "heap_max_in_bytes"])),
    );
    let caches = [
        total(n, &["indices", "query_cache", "memory_size_in_bytes"]),
        total(n, &["indices", "request_cache", "memory_size_in_bytes"]),
        total(n, &["indices", "fielddata", "memory_size_in_bytes"]),
    ];
    let cache_mem = (caches.iter().any(Option::is_some)).then(|| caches.iter().flatten().sum());
    s.metrics.push(Metric::new("mem_cache", "Cachés (consultas, pedidos, fielddata)", "Memoria", MetricUnit::Bytes, cache_mem));
    let gc = n
        .iter()
        .filter_map(|node| node.at(&["jvm", "gc", "collectors"]).and_then(J::as_obj))
        .flat_map(|c| c.iter().filter_map(|(_, v)| f(v, &["collection_time_in_millis"])))
        .reduce(|a, b| a + b);
    if gc.is_some() {
        s.metrics.push(Metric::new("gc_time", "Tiempo de GC", "Memoria", MetricUnit::Millis, gc).counter());
    }

    // Connections.
    s.metrics.push(Metric::new("connections", "Conexiones HTTP", "Conexiones", MetricUnit::Count, total(n, &["http", "current_open"])));
    s.metrics.push(Metric::new("active_sessions", "Búsquedas en curso", "Conexiones", MetricUnit::Count, total(n, &["indices", "search", "query_current"])));
    if let Some(v) = total(n, &["http", "total_opened"]) {
        s.metrics.push(Metric::new("connections_opened", "Conexiones HTTP abiertas", "Conexiones", MetricUnit::Count, Some(v)).counter());
    }

    // Activity.
    s.metrics.push(Metric::new("queries", "Búsquedas", "Actividad", MetricUnit::Count, total(n, &["indices", "search", "query_total"])).counter());
    s.metrics.push(Metric::new("rows_read", "Fetch de documentos", "Actividad", MetricUnit::Count, total(n, &["indices", "search", "fetch_total"])).counter());
    s.metrics.push(Metric::new("rows_written", "Documentos indexados", "Actividad", MetricUnit::Count, total(n, &["indices", "indexing", "index_total"])).counter());
    s.metrics.push(Metric::new("gets", "GET de documentos", "Actividad", MetricUnit::Count, total(n, &["indices", "get", "total"])).counter());
    s.metrics.push(Metric::new("deletes", "Documentos borrados", "Actividad", MetricUnit::Count, total(n, &["indices", "indexing", "delete_total"])).counter());
    s.metrics.push(Metric::new("search_time", "Tiempo de búsqueda", "Actividad", MetricUnit::Millis, total(n, &["indices", "search", "query_time_in_millis"])).counter());
    s.metrics.push(Metric::new("indexing_time", "Tiempo de indexación", "Actividad", MetricUnit::Millis, total(n, &["indices", "indexing", "index_time_in_millis"])).counter());
    s.metrics.push(Metric::new("merges", "Merges", "Actividad", MetricUnit::Count, total(n, &["indices", "merges", "total"])).counter());
    s.metrics.push(Metric::new("refreshes", "Refresh", "Actividad", MetricUnit::Count, total(n, &["indices", "refresh", "total"])).counter());

    // Thread pools: queued and rejected.
    let pools = |key: &str| -> Option<f64> {
        let v: Vec<f64> = n
            .iter()
            .filter_map(|node| node.get("thread_pool").and_then(J::as_obj))
            .flat_map(|p| p.iter().filter_map(|(_, v)| f(v, &[key])))
            .collect();
        (!v.is_empty()).then(|| v.iter().sum())
    };
    s.metrics.push(Metric::new("locks_waiting", "Tareas en cola (thread pools)", "Bloqueos", MetricUnit::Count, pools("queue")));
    s.metrics.push(Metric::new("rejected", "Tareas rechazadas", "Bloqueos", MetricUnit::Count, pools("rejected")).counter());

    // Cache.
    let qc = ratio(total(n, &["indices", "query_cache", "hit_count"]), total(n, &["indices", "query_cache", "miss_count"]));
    s.metrics.push(Metric::new("cache_hit", "Aciertos de la caché de consultas", "Caché", MetricUnit::Percent, qc).max(Some(100.0)));
    let rc = ratio(total(n, &["indices", "request_cache", "hit_count"]), total(n, &["indices", "request_cache", "miss_count"]));
    s.metrics.push(Metric::new("request_cache_hit", "Aciertos de la caché de pedidos", "Caché", MetricUnit::Percent, rc).max(Some(100.0)));
    let ev = [
        total(n, &["indices", "query_cache", "evictions"]),
        total(n, &["indices", "request_cache", "evictions"]),
        total(n, &["indices", "fielddata", "evictions"]),
    ];
    if ev.iter().any(Option::is_some) {
        s.metrics.push(Metric::new("evictions", "Desalojos de caché", "Caché", MetricUnit::Count, Some(ev.iter().flatten().sum())).counter());
    }

    // Network (between nodes) and disk.
    s.metrics.push(Metric::new("net_in", "Red entre nodos (entrante)", "Red", MetricUnit::Bytes, total(n, &["transport", "rx_size_in_bytes"])).counter());
    s.metrics.push(Metric::new("net_out", "Red entre nodos (saliente)", "Red", MetricUnit::Bytes, total(n, &["transport", "tx_size_in_bytes"])).counter());
    if let Some(r) = total(n, &["fs", "io_stats", "total", "read_kilobytes"]) {
        s.metrics.push(Metric::new("disk_read", "Lectura en disco", "Disco", MetricUnit::Bytes, Some(r * 1024.0)).counter());
    }
    if let Some(w) = total(n, &["fs", "io_stats", "total", "write_kilobytes"]) {
        s.metrics.push(Metric::new("disk_write", "Escritura en disco", "Disco", MetricUnit::Bytes, Some(w * 1024.0)).counter());
    }

    // Storage.
    s.metrics.push(Metric::new("storage_used", "Tamaño de los índices", "Almacenamiento", MetricUnit::Bytes, total(n, &["indices", "store", "size_in_bytes"])));
    let disk_total = total(n, &["fs", "total", "total_in_bytes"]);
    let disk_used = match (disk_total, total(n, &["fs", "total", "available_in_bytes"])) {
        (Some(t), Some(a)) => Some(t - a),
        _ => None,
    };
    s.metrics.push(Metric::new("disk_used", "Disco usado", "Almacenamiento", MetricUnit::Bytes, disk_used).max(disk_total));
    s.metrics.push(Metric::new("docs", "Documentos", "Almacenamiento", MetricUnit::Count, total(n, &["indices", "docs", "count"])));
    s.metrics.push(Metric::new("segments", "Segmentos", "Almacenamiento", MetricUnit::Count, total(n, &["indices", "segments", "count"])));

    // Server.
    let up = n.iter().filter_map(|node| f(node, &["jvm", "uptime_in_millis"])).reduce(f64::max);
    s.metrics.push(Metric::new("uptime", "Tiempo activo", "Servidor", MetricUnit::Seconds, up.map(|ms| ms / 1000.0)));
    if let Some(fd) = total(n, &["process", "open_file_descriptors"]) {
        s.metrics.push(Metric::new("open_files", "Descriptores abiertos", "Servidor", MetricUnit::Count, Some(fd)).max(total(n, &["process", "max_file_descriptors"])));
    }

    // Nodes table.
    let mut t = MonitorTable::new(
        "nodes",
        "Nodos del cluster",
        &["nodo", "dirección", "roles", "CPU (%)", "carga 1m", "memoria (%)", "heap (%)", "disco usado", "disco total", "documentos", "búsquedas en curso"],
    );
    for node in n.iter().take(MAX_ROWS) {
        let roles = node.get("roles").and_then(J::as_arr).map(|r| r.iter().map(J::text).collect::<Vec<_>>().join(", "));
        let total_disk = f(node, &["fs", "total", "total_in_bytes"]);
        let used_disk = total_disk.zip(f(node, &["fs", "total", "available_in_bytes"])).map(|(t, a)| t - a);
        t.rows.push(vec![
            cell(node.get("name")),
            cell(node.get("ip").or_else(|| node.get("host"))),
            roles.map(Value::String).unwrap_or(Value::Null),
            opt(f(node, &["os", "cpu", "percent"])),
            opt(f(node, &["os", "cpu", "load_average", "1m"])),
            opt(f(node, &["os", "mem", "used_percent"])),
            opt(f(node, &["jvm", "mem", "heap_used_percent"])),
            opt(used_disk),
            opt(total_disk),
            opt(f(node, &["indices", "docs", "count"])),
            opt(f(node, &["indices", "search", "query_current"])),
        ]);
    }
    s.tables.push(t);

    // Busiest thread pools.
    let mut agg: Vec<(String, [f64; 4])> = Vec::new();
    for p in n.iter().filter_map(|node| node.get("thread_pool").and_then(J::as_obj)) {
        for (name, v) in p {
            let vals = [
                f(v, &["active"]).unwrap_or(0.0),
                f(v, &["queue"]).unwrap_or(0.0),
                f(v, &["rejected"]).unwrap_or(0.0),
                f(v, &["completed"]).unwrap_or(0.0),
            ];
            match agg.iter_mut().find(|(k, _)| k == name) {
                Some((_, a)) => (0..4).for_each(|i| a[i] += vals[i]),
                None => agg.push((name.clone(), vals)),
            }
        }
    }
    agg.retain(|(_, v)| v.iter().any(|x| *x > 0.0));
    agg.sort_by(|a, b| (b.1[0] + b.1[1]).total_cmp(&(a.1[0] + a.1[1])).then(b.1[3].total_cmp(&a.1[3])));
    let mut t = MonitorTable::new("waits", "Thread pools", &["pool", "activos", "en cola", "rechazados", "completados"]);
    for (name, v) in agg.into_iter().take(MAX_ROWS) {
        t.rows.push(vec![Value::String(name), json!(v[0]), json!(v[1]), json!(v[2]), json!(v[3])]);
    }
    s.tables.push(t);
}

/// `_cat/indices?format=json&bytes=b` → sizes per index.
pub(crate) fn indices(s: &mut MonitorSnapshot, cat: &J, show_system: bool) {
    let mut rows: Vec<(f64, Vec<Value>)> = cat
        .as_arr()
        .unwrap_or(&[])
        .iter()
        .filter(|i| show_system || !i.get("index").and_then(J::as_str).unwrap_or("").starts_with('.'))
        .map(|i| {
            let size = f(i, &["store.size"]).unwrap_or(0.0);
            (
                size,
                vec![
                    cell(i.get("index")),
                    cell(i.get("health")),
                    cell(i.get("status")),
                    cell(i.get("pri")),
                    cell(i.get("rep")),
                    opt(f(i, &["docs.count"])),
                    opt(f(i, &["docs.deleted"])),
                    json!(size),
                    opt(f(i, &["pri.store.size"])),
                ],
            )
        })
        .collect();
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut t = MonitorTable::new(
        "databases",
        "Índices y tamaños",
        &["índice", "salud", "estado", "primarios", "réplicas", "documentos", "borrados", "tamaño", "tamaño primario"],
    );
    t.rows = rows.into_iter().take(MAX_ROWS).map(|(_, r)| r).collect();
    s.tables.push(t);
}

/// `_tasks?detailed=true` → running requests (without the listing itself).
pub(crate) fn tasks(tasks: &J) -> MonitorTable {
    let mut rows: Vec<(f64, Vec<Value>)> = Vec::new();
    for (_, node) in tasks.get("nodes").and_then(J::as_obj).into_iter().flatten() {
        let node_name = node.get("name").map(J::text).unwrap_or_default();
        for (id, t) in node.get("tasks").and_then(J::as_obj).into_iter().flatten() {
            let action = t.get("action").map(J::text).unwrap_or_default();
            if action.starts_with("cluster:monitor/tasks/lists") {
                continue;
            }
            let ms = f(t, &["running_time_in_nanos"]).map(|ns| ns / 1e6).unwrap_or(0.0);
            let desc = t.get("description").map(J::text).unwrap_or_default();
            rows.push((
                ms,
                vec![
                    Value::String(id.clone()),
                    Value::String(node_name.clone()),
                    Value::String(action),
                    json!((ms * 100.0).round() / 100.0),
                    cell(t.get("cancellable")),
                    cell(t.at(&["headers", "X-Opaque-Id"])),
                    Value::String(crate::http::clip(&desc, 2000)),
                ],
            ));
        }
    }
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut t = MonitorTable::new(
        "queries",
        "Tareas en curso",
        &["tarea", "nodo", "acción", "duración (ms)", "cancelable", "X-Opaque-Id", "descripción"],
    );
    t.rows = rows.into_iter().take(MAX_ROWS).map(|(_, r)| r).collect();
    t
}

pub(crate) fn pending(p: &J) -> MonitorTable {
    let mut t = MonitorTable::new("pending_tasks", "Tareas pendientes del cluster", &["orden", "prioridad", "origen", "en cola (ms)", "ejecutando"]);
    for task in p.get("tasks").and_then(J::as_arr).unwrap_or(&[]).iter().take(MAX_ROWS) {
        t.rows.push(vec![
            cell(task.get("insert_order")),
            cell(task.get("priority")),
            cell(task.get("source")),
            cell(task.get("time_in_queue_millis")),
            cell(task.get("executing")),
        ]);
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(s: &str) -> J {
        J::parse(s).unwrap()
    }

    #[test]
    fn node_stats_are_summed() {
        let stats = j(r#"{"nodes": {
            "a": {"name": "n1", "os": {"cpu": {"percent": 40}, "mem": {"used_in_bytes": 10, "total_in_bytes": 100}},
                  "process": {"cpu": {"total_in_millis": 2000}},
                  "jvm": {"mem": {"heap_used_in_bytes": 5, "heap_max_in_bytes": 50}, "uptime_in_millis": 9000},
                  "indices": {"search": {"query_total": 7}, "query_cache": {"hit_count": 3, "miss_count": 1}},
                  "thread_pool": {"search": {"active": 1, "queue": 2, "rejected": 0, "completed": 5}}},
            "b": {"name": "n2", "os": {"cpu": {"percent": 20}, "mem": {"used_in_bytes": 30, "total_in_bytes": 100}},
                  "jvm": {"mem": {"heap_used_in_bytes": 5, "heap_max_in_bytes": 50}, "uptime_in_millis": 4000},
                  "indices": {"search": {"query_total": 3}}}
        }}"#);
        let mut s = MonitorSnapshot::default();
        from_node_stats(&mut s, &stats);
        let m = |k: &str| s.metrics.iter().find(|m| m.key == k).unwrap();
        assert_eq!(m("cpu").value, Some(30.0));
        assert_eq!(m("cpu_time").value, Some(200.0));
        assert_eq!(m("mem_used").value, Some(40.0));
        assert_eq!(m("mem_used").max, Some(200.0));
        assert_eq!(m("heap_used").max, Some(100.0));
        assert_eq!(m("queries").value, Some(10.0));
        assert_eq!(m("cache_hit").value, Some(75.0));
        assert_eq!(m("locks_waiting").value, Some(2.0));
        assert_eq!(m("uptime").value, Some(9.0));
        assert_eq!(s.tables[0].rows.len(), 2);
        assert_eq!(s.tables[1].rows[0][0], json!("search"));
    }

    #[test]
    fn indices_and_tasks() {
        let mut s = MonitorSnapshot::default();
        indices(&mut s, &j(r#"[{"index": "a", "store.size": "100"}, {"index": ".sys", "store.size": "900"}, {"index": "b", "store.size": "300"}]"#), false);
        assert_eq!(s.tables[0].rows.len(), 2);
        assert_eq!(s.tables[0].rows[0][0], json!("b"));
        let t = tasks(&j(r#"{"nodes": {"x": {"name": "n1", "tasks": {
            "x:1": {"action": "indices:data/read/search", "running_time_in_nanos": 5000000, "description": "q"},
            "x:2": {"action": "cluster:monitor/tasks/lists", "running_time_in_nanos": 1}
        }}}}"#));
        assert_eq!(t.rows.len(), 1);
        assert_eq!(t.rows[0][3], json!(5.0));
    }
}
