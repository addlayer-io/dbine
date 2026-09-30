//! `Session::monitor` for CouchDB: `/_node/_local/_stats` (requests,
//! reads, writes, request time, HTTP errors), `/_node/_local/_system`
//! (Erlang VM memory, run queue, processes, I/O, B-tree cache),
//! `/_active_tasks`, `/_scheduler/jobs`, `/_up`, `/_membership`,
//! `/_dbs_info` (sizes) and the node's configuration. An endpoint the
//! user can't read (most need the server admin role) leaves a note.

use crate::CouchSession;
use dbine_driver::monitor::{Metric, MetricUnit, MonitorSnapshot, MonitorTable};
use reqwest::Method;
use serde_json::{json, Value};

const MAX_ROWS: usize = 200;
const MAX_DBS: usize = 100;

/// A number at `path` (`value` objects of `_stats` are unwrapped).
fn at(v: &Value, path: &[&str]) -> Option<f64> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    match cur {
        Value::Number(n) => n.as_f64(),
        Value::Object(o) => o.get("value").and_then(Value::as_f64),
        _ => None,
    }
}

fn sum(values: &[Option<f64>]) -> Option<f64> {
    let v: Vec<f64> = values.iter().flatten().copied().collect();
    (!v.is_empty()).then(|| v.iter().sum())
}

fn cell(v: Option<&Value>) -> Value {
    match v {
        None | Some(Value::Null) => Value::Null,
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(Value::Number(n)) => Value::Number(n.clone()),
        Some(Value::Bool(b)) => Value::Bool(*b),
        Some(other) => Value::String(other.to_string()),
    }
}

/// Unix seconds → `YYYY-MM-DD HH:MM:SS` (UTC).
fn when(v: Option<&Value>) -> Value {
    v.and_then(Value::as_i64)
        .and_then(|t| chrono::DateTime::from_timestamp(t, 0))
        .map(|d| Value::String(d.format("%Y-%m-%d %H:%M:%S").to_string()))
        .unwrap_or_else(|| cell(v))
}

pub(crate) fn from_stats(s: &mut MonitorSnapshot, st: &Value) {
    let c = |p: &[&str]| at(st, &[&["couchdb"], p].concat());
    s.metrics.push(Metric::new("queries", "Pedidos HTTP", "Actividad", MetricUnit::Count, c(&["httpd", "requests"])).counter());
    s.metrics.push(Metric::new("rows_read", "Lecturas de documentos", "Actividad", MetricUnit::Count, c(&["database_reads"])).counter());
    s.metrics.push(Metric::new("rows_written", "Escrituras de documentos", "Actividad", MetricUnit::Count, c(&["database_writes"])).counter());
    if let Some(v) = c(&["document_inserts"]) {
        s.metrics.push(Metric::new("document_inserts", "Documentos nuevos", "Actividad", MetricUnit::Count, Some(v)).counter());
    }
    if let Some(v) = c(&["httpd", "view_reads"]) {
        s.metrics.push(Metric::new("view_reads", "Lecturas de vistas", "Actividad", MetricUnit::Count, Some(v)).counter());
    }
    let rt = st.get("couchdb").and_then(|x| x.get("request_time")).and_then(|x| x.get("value"));
    if let Some(rt) = rt {
        s.metrics.push(Metric::new(
            "request_time",
            "Tiempo medio por pedido",
            "Actividad",
            MetricUnit::Millis,
            rt.get("arithmetic_mean").and_then(Value::as_f64),
        ));
        let p99 = rt
            .get("percentile")
            .and_then(Value::as_array)
            .and_then(|a| a.iter().find(|p| p.get(0).and_then(Value::as_f64) == Some(99.0)))
            .and_then(|p| p.get(1))
            .and_then(Value::as_f64);
        if p99.is_some() {
            s.metrics.push(Metric::new("request_time_p99", "Tiempo por pedido (p99)", "Actividad", MetricUnit::Millis, p99));
        }
    }
    let codes = st.get("couchdb").and_then(|x| x.get("httpd_status_codes")).and_then(Value::as_object);
    if let Some(codes) = codes {
        let errors = |first: char| {
            codes.iter().filter(|(k, _)| k.starts_with(first)).filter_map(|(_, v)| v.get("value").and_then(Value::as_f64)).sum::<f64>()
        };
        s.metrics.push(Metric::new("http_4xx", "Respuestas 4xx", "Actividad", MetricUnit::Count, Some(errors('4'))).counter());
        s.metrics.push(Metric::new("http_5xx", "Respuestas 5xx", "Actividad", MetricUnit::Count, Some(errors('5'))).counter());
    }
    if let Some(v) = c(&["open_databases"]) {
        s.metrics.push(Metric::new("open_databases", "Bases abiertas", "Conexiones", MetricUnit::Count, Some(v)));
    }
    if let Some(v) = c(&["httpd", "clients_requesting_changes"]) {
        s.metrics.push(Metric::new("changes_clients", "Clientes escuchando _changes", "Conexiones", MetricUnit::Count, Some(v)));
    }
    if let Some(v) = c(&["open_os_files"]) {
        s.metrics.push(Metric::new("open_files", "Archivos abiertos", "Disco", MetricUnit::Count, Some(v)));
    }
    if let Some(v) = at(st, &["fsync", "count"]) {
        s.metrics.push(Metric::new("fsyncs", "fsync", "Disco", MetricUnit::Count, Some(v)).counter());
    }
    let (hits, misses) = (c(&["auth_cache_hits"]), c(&["auth_cache_misses"]));
    if let (Some(h), Some(m)) = (hits, misses) {
        if h + m > 0.0 {
            s.metrics.push(Metric::new("auth_cache_hit", "Aciertos de la caché de autenticación", "Caché", MetricUnit::Percent, Some(h / (h + m) * 100.0)).max(Some(100.0)));
        }
    }
}

pub(crate) fn from_system(s: &mut MonitorSnapshot, sys: &Value) {
    let mem = sys.get("memory").and_then(Value::as_object);
    let total = mem.map(|m| {
        ["processes", "binary", "code", "ets", "atom", "other"].iter().filter_map(|k| m.get(*k).and_then(Value::as_f64)).sum::<f64>()
    });
    s.metrics.push(Metric::new("mem_used", "Memoria de la VM Erlang", "Memoria", MetricUnit::Bytes, total));
    for (k, label) in [("processes", "Memoria de procesos"), ("binary", "Memoria binaria"), ("ets", "Tablas ETS")] {
        if let Some(v) = at(sys, &["memory", k]) {
            s.metrics.push(Metric::new(&format!("mem_{k}"), label, "Memoria", MetricUnit::Bytes, Some(v)));
        }
    }
    if let Some(cache) = sys.get("bt_engine_cache") {
        s.metrics.push(
            Metric::new("mem_cache", "Caché de B-trees", "Memoria", MetricUnit::Bytes, at(cache, &["memory"]))
                .max(at(cache, &["max_memory"])),
        );
        let hit = match (at(cache, &["hits"]), at(cache, &["misses"])) {
            (Some(h), Some(m)) if h + m > 0.0 => Some(h / (h + m) * 100.0),
            _ => None,
        };
        s.metrics.push(Metric::new("cache_hit", "Aciertos de caché", "Caché", MetricUnit::Percent, hit).max(Some(100.0)));
    }
    let rq = sum(&[at(sys, &["run_queue"]), at(sys, &["run_queue_dirty_cpu"])]);
    s.metrics.push(Metric::new("run_queue", "Cola de ejecución", "CPU", MetricUnit::Count, rq));
    if let Some(r) = at(sys, &["reductions"]) {
        s.metrics.push(Metric::new("reductions", "Reducciones (trabajo de la VM)", "CPU", MetricUnit::Count, Some(r)).counter());
    }
    s.metrics.push(
        Metric::new("processes", "Procesos Erlang", "Servidor", MetricUnit::Count, at(sys, &["process_count"]))
            .max(at(sys, &["process_limit"])),
    );
    if let Some(v) = at(sys, &["io_input"]) {
        s.metrics.push(Metric::new("net_in", "E/S entrante", "Red", MetricUnit::Bytes, Some(v)).counter());
    }
    if let Some(v) = at(sys, &["io_output"]) {
        s.metrics.push(Metric::new("net_out", "E/S saliente", "Red", MetricUnit::Bytes, Some(v)).counter());
    }
    s.metrics.push(Metric::new("uptime", "Tiempo activo", "Servidor", MetricUnit::Seconds, at(sys, &["uptime"])));
}

pub(crate) fn active_tasks(tasks: &Value) -> MonitorTable {
    let mut t = MonitorTable::new(
        "queries",
        "Tareas activas",
        &["tipo", "base", "documento de diseño", "progreso (%)", "cambios", "total", "inicio", "actualizada", "nodo", "pid"],
    );
    for task in tasks.as_array().map(Vec::as_slice).unwrap_or(&[]).iter().take(MAX_ROWS) {
        t.rows.push(vec![
            cell(task.get("type")),
            cell(task.get("database").or_else(|| task.get("source"))),
            cell(task.get("design_document")),
            cell(task.get("progress")),
            cell(task.get("changes_done").or_else(|| task.get("docs_written"))),
            cell(task.get("total_changes")),
            when(task.get("started_on")),
            when(task.get("updated_on")),
            cell(task.get("node")),
            cell(task.get("pid")),
        ]);
    }
    t
}

pub(crate) fn replication_jobs(jobs: &Value) -> MonitorTable {
    let mut t = MonitorTable::new("replication", "Replicaciones", &["id", "origen", "destino", "estado", "inicio", "nodo", "documento"]);
    for j in jobs.get("jobs").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]).iter().take(MAX_ROWS) {
        let state = j.get("history").and_then(Value::as_array).and_then(|h| h.first()).and_then(|h| h.get("type"));
        t.rows.push(vec![
            cell(j.get("id")),
            cell(j.get("source")),
            cell(j.get("target")),
            cell(state),
            cell(j.get("start_time")),
            cell(j.get("node")),
            cell(j.get("doc_id")),
        ]);
    }
    t
}

impl CouchSession {
    pub(crate) async fn snapshot(&self) -> dbine_driver::Result<MonitorSnapshot> {
        let mut s = MonitorSnapshot::default();
        let get = |p: &'static str| self.call(Method::GET, p, None);
        let mut refused = Vec::new();
        let welcome = get("/").await?;
        s.info.push(("Versión".into(), welcome.get("version").and_then(Value::as_str).unwrap_or("?").into()));
        if let Some(v) = welcome.get("vendor").and_then(|v| v.get("name")).and_then(Value::as_str) {
            s.info.push(("Distribuidor".into(), v.into()));
        }
        if let Some(f) = welcome.get("features").and_then(Value::as_array) {
            let f: Vec<&str> = f.iter().filter_map(Value::as_str).collect();
            s.info.push(("Funciones".into(), f.join(", ")));
        }
        s.notes.push(
            "CouchDB no informa el uso de CPU ni las conexiones HTTP abiertas; la cola de ejecución y las reducciones de la VM Erlang muestran la carga."
                .into(),
        );

        match get("/_node/_local/_system").await {
            Ok(sys) => from_system(&mut s, &sys),
            Err(_) => refused.push("/_node/_local/_system"),
        }
        match get("/_node/_local/_stats").await {
            Ok(st) => from_stats(&mut s, &st),
            Err(_) => refused.push("/_node/_local/_stats"),
        }
        match get("/_active_tasks").await {
            Ok(t) => {
                let t = active_tasks(&t);
                s.metrics.push(Metric::new("active_sessions", "Tareas activas", "Actividad", MetricUnit::Count, Some(t.rows.len() as f64)));
                s.tables.push(t);
            }
            Err(_) => refused.push("/_active_tasks"),
        }
        match get("/_scheduler/jobs").await {
            Ok(j) => {
                let t = replication_jobs(&j);
                s.metrics.push(Metric::new("replications", "Replicaciones activas", "Replicación", MetricUnit::Count, Some(t.rows.len() as f64)));
                s.tables.push(t);
            }
            Err(_) => refused.push("/_scheduler/jobs"),
        }

        // Databases and sizes.
        match get("/_all_dbs").await {
            Ok(all) => {
                let names: Vec<Value> = all.as_array().cloned().unwrap_or_default();
                if names.len() > MAX_DBS {
                    s.notes.push(format!("Se muestran los tamaños de las primeras {MAX_DBS} de {} bases.", names.len()));
                }
                let keys: Vec<Value> = names.into_iter().take(MAX_DBS).collect();
                let infos = if keys.is_empty() {
                    Ok(Value::Array(Vec::new()))
                } else {
                    self.call(Method::POST, "/_dbs_info", Some(&json!({ "keys": keys }))).await
                };
                match infos {
                    Ok(infos) => databases(&mut s, &infos),
                    Err(_) => refused.push("/_dbs_info"),
                }
            }
            Err(_) => refused.push("/_all_dbs"),
        }

        // Cluster.
        let up = get("/_up").await.ok();
        let status = up.as_ref().and_then(|u| u.get("status")).and_then(Value::as_str).unwrap_or("?").to_string();
        s.info.push(("Estado (_up)".into(), status.clone()));
        match get("/_membership").await {
            Ok(m) => {
                let all: Vec<&str> = m.get("all_nodes").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                let cluster: Vec<&str> = m.get("cluster_nodes").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                let mut t = MonitorTable::new("nodes", "Nodos del cluster", &["nodo", "estado"]);
                for n in cluster.iter().take(MAX_ROWS) {
                    let state = if all.contains(n) { "conectado" } else { "sin conexión" };
                    t.rows.push(vec![Value::String((*n).into()), Value::String(state.into())]);
                }
                s.metrics.push(Metric::new("nodes_up", "Nodos conectados", "Servidor", MetricUnit::Count, Some(all.len() as f64)).max(Some(cluster.len() as f64)));
                s.tables.push(t);
            }
            Err(_) => refused.push("/_membership"),
        }
        if let Ok(cfg) = get("/_node/_local/_config").await {
            for (section, key, label) in [
                ("cluster", "q", "Shards por base (q)"),
                ("cluster", "n", "Réplicas por shard (n)"),
                ("couchdb", "max_dbs_open", "Bases abiertas máximas"),
                ("couchdb", "single_node", "Nodo único"),
                ("chttpd", "port", "Puerto"),
                ("chttpd", "max_http_request_size", "Pedido HTTP máximo"),
                ("couchdb", "database_dir", "Directorio de datos"),
            ] {
                if let Some(v) = cfg.get(section).and_then(|s| s.get(key)).and_then(Value::as_str) {
                    s.info.push((label.into(), v.into()));
                }
            }
        }
        if !refused.is_empty() {
            s.notes.push(format!(
                "Sin permiso de administrador del servidor no se pueden leer: {}.",
                refused.join(", ")
            ));
        }
        Ok(s)
    }
}

pub(crate) fn databases(s: &mut MonitorSnapshot, infos: &Value) {
    let mut t = MonitorTable::new(
        "databases",
        "Bases y tamaños",
        &["base", "documentos", "borrados", "archivo", "datos activos", "datos (sin comprimir)", "shards", "compactando"],
    );
    let mut total = 0.0;
    let mut rows: Vec<(f64, Vec<Value>)> = Vec::new();
    for item in infos.as_array().map(Vec::as_slice).unwrap_or(&[]) {
        let Some(info) = item.get("info") else { continue };
        let file = at(info, &["sizes", "file"]).unwrap_or(0.0);
        total += file;
        rows.push((
            file,
            vec![
                cell(item.get("key").or_else(|| info.get("db_name"))),
                cell(info.get("doc_count")),
                cell(info.get("doc_del_count")),
                json!(file),
                cell(info.get("sizes").and_then(|x| x.get("active"))),
                cell(info.get("sizes").and_then(|x| x.get("external"))),
                cell(info.get("cluster").and_then(|x| x.get("q"))),
                cell(info.get("compact_running")),
            ],
        ));
    }
    rows.sort_by(|a, b| b.0.total_cmp(&a.0));
    t.rows = rows.into_iter().map(|(_, r)| r).collect();
    s.metrics.push(Metric::new("storage_used", "Espacio usado", "Almacenamiento", MetricUnit::Bytes, Some(total)));
    s.tables.push(t);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_and_system() {
        let st = json!({
            "couchdb": {
                "httpd": { "requests": { "value": 8, "type": "counter" } },
                "database_reads": { "value": 3, "type": "counter" },
                "database_writes": { "value": 2, "type": "counter" },
                "request_time": { "value": { "arithmetic_mean": 1.5, "percentile": [[50, 1], [99, 9.5]] }, "type": "histogram" },
                "httpd_status_codes": { "200": { "value": 5 }, "404": { "value": 2 }, "401": { "value": 1 }, "500": { "value": 0 } }
            }
        });
        let sys = json!({ "uptime": 60, "memory": { "processes": 10, "binary": 5, "code": 1, "ets": 4, "atom": 0, "other": 0 },
                          "run_queue": 1, "run_queue_dirty_cpu": 0, "process_count": 400, "process_limit": 1000,
                          "bt_engine_cache": { "memory": 20, "max_memory": 100, "hits": 3, "misses": 1 } });
        let mut s = MonitorSnapshot::default();
        from_stats(&mut s, &st);
        from_system(&mut s, &sys);
        let m = |k: &str| s.metrics.iter().find(|m| m.key == k).unwrap();
        assert_eq!(m("queries").value, Some(8.0));
        assert!(m("queries").counter);
        assert_eq!(m("request_time_p99").value, Some(9.5));
        assert_eq!(m("http_4xx").value, Some(3.0));
        assert_eq!(m("mem_used").value, Some(20.0));
        assert_eq!(m("cache_hit").value, Some(75.0));
        assert_eq!(m("processes").max, Some(1000.0));
    }

    #[test]
    fn tables() {
        let t = active_tasks(&json!([{ "type": "indexer", "database": "db", "progress": 40, "started_on": 1700000000 }]));
        assert_eq!(t.rows[0][0], json!("indexer"));
        assert_eq!(t.rows[0][6], json!("2023-11-14 22:13:20"));
        let mut s = MonitorSnapshot::default();
        databases(&mut s, &json!([{ "key": "a", "info": { "doc_count": 2, "sizes": { "file": 100, "active": 50 } } }, { "key": "b", "error": "not_found" }]));
        assert_eq!(s.tables[0].rows.len(), 1);
        assert_eq!(s.metrics[0].value, Some(100.0));
    }
}
