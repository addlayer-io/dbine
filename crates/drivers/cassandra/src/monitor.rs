//! `Session::monitor` over CQL.
//!
//! - Cassandra 4.0+: the `system_views` virtual tables (clients, running
//!   queries, thread pools, caches, CQL metrics, local and coordinator
//!   latencies, disk usage, SSTable tasks, pending hints, gossip, settings).
//! - ScyllaDB: its own virtual tables in `system` (runtime_info,
//!   cluster_status, clients, load_per_node, compactions_in_progress,
//!   large_partitions, config, versions).
//! - Amazon Keyspaces: only `system.local` / `system.peers` and
//!   `system_schema_mcs`; the service's metrics are in CloudWatch.
//!
//! Every query is optional: a table the server doesn't have leaves a note.

use crate::value;
use crate::Flavor;
use dbine_driver::monitor::{Metric, MetricUnit, MonitorSnapshot, MonitorTable};
use scylla::client::session::Session;
use scylla::value::{CqlValue, Row};
use serde_json::{json, Value as J};
use std::collections::{BTreeMap, HashMap};

const MIB: f64 = 1024.0 * 1024.0;
const MAX_ROWS: usize = 200;

type Rec = HashMap<String, Option<CqlValue>>;

async fn query(s: &Session, cql: &str) -> std::result::Result<Vec<Rec>, String> {
    let res = s.query_unpaged(cql, ()).await.map_err(|e| e.to_string())?;
    let rows = res.into_rows_result().map_err(|e| e.to_string())?;
    let names: Vec<String> = rows.column_specs().iter().map(|c| c.name().to_string()).collect();
    let mut out = Vec::new();
    for r in rows.rows::<Row>().map_err(|e| e.to_string())? {
        let r = r.map_err(|e| e.to_string())?;
        out.push(names.iter().cloned().zip(r.columns).collect());
    }
    Ok(out)
}

fn cql_f64(v: &CqlValue) -> Option<f64> {
    match v {
        CqlValue::Int(n) => Some(*n as f64),
        CqlValue::BigInt(n) => Some(*n as f64),
        CqlValue::SmallInt(n) => Some(*n as f64),
        CqlValue::TinyInt(n) => Some(*n as f64),
        CqlValue::Counter(c) => Some(c.0 as f64),
        CqlValue::Float(f) => Some(*f as f64),
        CqlValue::Double(f) => Some(*f),
        CqlValue::Text(s) | CqlValue::Ascii(s) => dbine_driver::monitor::num(s),
        _ => None,
    }
    .filter(|f| f.is_finite())
}

fn f(r: &Rec, k: &str) -> Option<f64> {
    r.get(k)?.as_ref().and_then(cql_f64)
}

fn s(r: &Rec, k: &str) -> String {
    match r.get(k) {
        Some(Some(CqlValue::Text(s) | CqlValue::Ascii(s))) => s.clone(),
        Some(Some(v)) => match value::to_json(v) {
            J::String(s) => s,
            other => other.to_string(),
        },
        _ => String::new(),
    }
}

fn c(r: &Rec, k: &str) -> J {
    value::cell(r.get(k).and_then(Option::as_ref))
}

fn total(rows: &[Rec], k: &str) -> Option<f64> {
    let v: Vec<f64> = rows.iter().filter_map(|r| f(r, k)).collect();
    (!v.is_empty()).then(|| v.iter().sum())
}

fn clip(t: &str, max: usize) -> String {
    if t.chars().count() <= max {
        t.to_string()
    } else {
        format!("{}…", t.chars().take(max).collect::<String>())
    }
}

/// `"64 seconds"`, `"1 hour 2 minutes"`… (Scylla's runtime_info uptime).
fn parse_uptime(t: &str) -> Option<f64> {
    let words: Vec<&str> = t.split_whitespace().collect();
    let mut secs = 0.0;
    let mut any = false;
    for pair in words.chunks(2) {
        let n: f64 = pair.first()?.parse().ok()?;
        let unit = pair.get(1).copied().unwrap_or("seconds");
        secs += n * match unit.trim_end_matches('s') {
            "second" => 1.0,
            "minute" => 60.0,
            "hour" => 3600.0,
            "day" => 86_400.0,
            _ => return None,
        };
        any = true;
    }
    any.then_some(secs)
}

pub(crate) async fn snapshot(session: &Session, flavor: Flavor) -> dbine_driver::Result<MonitorSnapshot> {
    let mut snap = MonitorSnapshot::default();
    let local = query(session, "SELECT * FROM system.local").await.map_err(dbine_driver::Error::Query)?;
    let local = local.into_iter().next().unwrap_or_default();
    let product = match flavor {
        Flavor::Cassandra => "Apache Cassandra",
        Flavor::Scylla => "ScyllaDB",
        Flavor::Keyspaces => "Amazon Keyspaces",
    };
    snap.info.push(("Producto".into(), product.into()));
    for (k, label) in [
        ("release_version", "Versión (compatible con Cassandra)"),
        ("cluster_name", "Cluster"),
        ("data_center", "Datacenter"),
        ("rack", "Rack"),
        ("cql_version", "Versión de CQL"),
        ("native_protocol_version", "Protocolo nativo"),
        ("partitioner", "Particionador"),
        ("broadcast_address", "Dirección"),
    ] {
        let v = s(&local, k);
        if !v.is_empty() {
            snap.info.push((label.into(), v));
        }
    }
    match flavor {
        Flavor::Cassandra => cassandra(session, &local, &mut snap).await,
        Flavor::Scylla => scylla(session, &mut snap).await,
        Flavor::Keyspaces => keyspaces(session, &mut snap).await,
    }
    Ok(snap)
}

fn note_missing(snap: &mut MonitorSnapshot, what: &str, e: &str) {
    snap.notes.push(format!("No se pudo leer {what}: {e}"));
}

// ------------------------------------------------------------ Cassandra

async fn cassandra(session: &Session, local: &Rec, snap: &mut MonitorSnapshot) {
    // Cassandra 3.x has no virtual tables: nodes only.
    let settings = match query(session, "SELECT name, value FROM system_views.settings").await {
        Ok(r) => r,
        Err(_) => {
            snap.notes.push(
                "Este Cassandra no tiene las tablas virtuales system_views (llegaron en la 4.0): solo se muestran los nodos."
                    .into(),
            );
            peers_nodes(session, local, snap).await;
            return;
        }
    };
    let setting: HashMap<String, String> = settings.iter().map(|r| (s(r, "name"), s(r, "value"))).collect();
    snap.notes.push(
        "Cassandra no expone el uso de CPU ni la memoria del JVM por CQL; para eso hace falta JMX (nodetool info / tpstats)."
            .into(),
    );

    // Connections.
    let clients = query(session, "SELECT * FROM system_views.clients").await;
    let max_conn = setting.get("native_transport_max_concurrent_connections").and_then(|v| v.parse::<f64>().ok()).filter(|v| *v > 0.0);
    let running = query(session, "SELECT * FROM system_views.queries").await;
    match &clients {
        Ok(rows) => {
            snap.metrics.push(Metric::new("connections", "Conexiones", "Conexiones", MetricUnit::Count, Some(rows.len() as f64)).max(max_conn));
        }
        Err(e) => note_missing(snap, "system_views.clients", e),
    }
    if let Ok(q) = &running {
        snap.metrics.push(Metric::new("active_sessions", "Consultas en curso", "Conexiones", MetricUnit::Count, Some(q.len() as f64)));
    }

    // Activity.
    let cql: HashMap<String, f64> = query(session, "SELECT name, value FROM system_views.cql_metrics")
        .await
        .map(|r| r.iter().filter_map(|x| Some((s(x, "name"), f(x, "value")?))).collect())
        .unwrap_or_default();
    let statements = match (cql.get("regular_statements_executed"), cql.get("prepared_statements_executed")) {
        (None, None) => clients.as_ref().ok().and_then(|r| total(r, "request_count")),
        (a, b) => Some(a.unwrap_or(&0.0) + b.unwrap_or(&0.0)),
    };
    snap.metrics.push(Metric::new("queries", "Sentencias CQL", "Actividad", MetricUnit::Count, statements).counter());
    let lat = |t: &'static str| async move { query(session, &format!("SELECT * FROM system_views.{t}")).await.unwrap_or_default() };
    let local_reads = lat("local_read_latency").await;
    let local_scans = lat("local_scan_latency").await;
    let local_writes = lat("local_write_latency").await;
    let coord_reads = lat("coordinator_read_latency").await;
    let coord_writes = lat("coordinator_write_latency").await;
    let reads = match (total(&local_reads, "count"), total(&local_scans, "count")) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0.0) + b.unwrap_or(0.0)),
    };
    snap.metrics.push(Metric::new("rows_read", "Lecturas locales", "Actividad", MetricUnit::Count, reads).counter());
    snap.metrics.push(Metric::new("rows_written", "Escrituras locales", "Actividad", MetricUnit::Count, total(&local_writes, "count")).counter());

    // Thread pools.
    match query(session, "SELECT * FROM system_views.thread_pools").await {
        Ok(pools) => {
            snap.metrics.push(Metric::new("pending_tasks", "Tareas pendientes", "Actividad", MetricUnit::Count, total(&pools, "pending_tasks")));
            snap.metrics.push(Metric::new("locks_waiting", "Tareas bloqueadas", "Bloqueos", MetricUnit::Count, total(&pools, "blocked_tasks")));
            if let Some(c) = pools.iter().find(|p| s(p, "name") == "CompactionExecutor") {
                snap.metrics.push(Metric::new("compactions", "Compactaciones", "Actividad", MetricUnit::Count, f(c, "completed_tasks")).counter());
            }
            let mut t = MonitorTable::new(
                "waits",
                "Pools de hilos",
                &["pool", "activas", "límite", "pendientes", "bloqueadas", "bloqueadas (total)", "completadas"],
            );
            let mut pools = pools;
            pools.sort_by(|a, b| {
                let k = |r: &Rec| f(r, "pending_tasks").unwrap_or(0.0) + f(r, "active_tasks").unwrap_or(0.0);
                k(b).total_cmp(&k(a)).then_with(|| s(a, "name").cmp(&s(b, "name")))
            });
            for p in pools.iter().take(MAX_ROWS) {
                t.rows.push(vec![
                    c(p, "name"),
                    c(p, "active_tasks"),
                    c(p, "active_tasks_limit"),
                    c(p, "pending_tasks"),
                    c(p, "blocked_tasks"),
                    c(p, "blocked_tasks_all_time"),
                    c(p, "completed_tasks"),
                ]);
            }
            snap.tables.push(t);
        }
        Err(e) => note_missing(snap, "system_views.thread_pools", &e),
    }

    // Caches.
    if let Ok(caches) = query(session, "SELECT * FROM system_views.caches").await {
        let used = total(&caches, "size_bytes");
        let cap = total(&caches, "capacity_bytes");
        snap.metrics.push(Metric::new("mem_cache", "Cachés (claves, filas, contadores)", "Memoria", MetricUnit::Bytes, used).max(cap));
        let keys = caches.iter().find(|r| s(r, "name") == "keys");
        let hit = keys.and_then(|k| {
            let req = f(k, "request_count")?;
            (req > 0.0).then(|| f(k, "hit_count").unwrap_or(0.0) / req * 100.0)
        });
        snap.metrics.push(Metric::new("cache_hit", "Aciertos de la caché de claves", "Caché", MetricUnit::Percent, hit).max(Some(100.0)));
        if let Some(rows) = caches.iter().find(|r| s(r, "name") == "rows").filter(|r| f(r, "capacity_bytes").unwrap_or(0.0) > 0.0) {
            let req = f(rows, "request_count").unwrap_or(0.0);
            let h = (req > 0.0).then(|| f(rows, "hit_count").unwrap_or(0.0) / req * 100.0);
            snap.metrics.push(Metric::new("row_cache_hit", "Aciertos de la caché de filas", "Caché", MetricUnit::Percent, h).max(Some(100.0)));
        }
    }

    // Storage.
    match query(session, "SELECT * FROM system_views.disk_usage").await {
        Ok(du) => {
            let bytes = total(&du, "mebibytes").map(|m| m * MIB);
            snap.metrics.push(Metric::new("storage_used", "Espacio usado (este nodo)", "Almacenamiento", MetricUnit::Bytes, bytes));
            sizes(snap, du.iter().map(|r| (s(r, "keyspace_name"), s(r, "table_name"), f(r, "mebibytes").unwrap_or(0.0) * MIB)));
        }
        Err(e) => note_missing(snap, "system_views.disk_usage", &e),
    }
    if let Ok(h) = query(session, "SELECT * FROM system_views.pending_hints").await {
        snap.metrics.push(Metric::new("pending_hints", "Archivos de hints pendientes", "Replicación", MetricUnit::Count, Some(total(&h, "files").unwrap_or(0.0))));
    }

    // Sessions and running queries.
    if let Ok(rows) = &clients {
        let mut t = MonitorTable::new(
            "sessions",
            "Sesiones",
            &["cliente", "usuario", "keyspace", "driver", "estado", "pedidos", "protocolo", "TLS"],
        );
        for r in rows.iter().take(MAX_ROWS) {
            t.rows.push(vec![
                J::String(format!("{}:{}", s(r, "address"), s(r, "port"))),
                c(r, "username"),
                c(r, "keyspace_name"),
                J::String(format!("{} {}", s(r, "driver_name"), s(r, "driver_version")).trim().to_string()),
                c(r, "connection_stage"),
                c(r, "request_count"),
                c(r, "protocol_version"),
                c(r, "ssl_enabled"),
            ]);
        }
        snap.tables.insert(0, t);
    }
    if let Ok(q) = running {
        let mut t = MonitorTable::new("queries", "Consultas en curso", &["hilo", "en cola (ms)", "ejecutando (ms)", "consulta"]);
        for r in q.iter().take(MAX_ROWS) {
            t.rows.push(vec![
                c(r, "thread_id"),
                f(r, "queued_micros").map(|v| json!(v / 1000.0)).unwrap_or(J::Null),
                f(r, "running_micros").map(|v| json!(v / 1000.0)).unwrap_or(J::Null),
                J::String(clip(&s(r, "task"), 2000)),
            ]);
        }
        snap.tables.insert(1.min(snap.tables.len()), t);
    }

    // Busiest tables (coordinator requests).
    let mut busy: BTreeMap<(String, String), [Option<f64>; 4]> = BTreeMap::new();
    for (rows, i) in [(&coord_reads, 0), (&coord_writes, 1)] {
        for r in rows {
            let e = busy.entry((s(r, "keyspace_name"), s(r, "table_name"))).or_default();
            e[i] = f(r, "count");
            e[i + 2] = f(r, "p99th_ms");
        }
    }
    let mut busy: Vec<_> = busy.into_iter().filter(|(_, v)| v[0].unwrap_or(0.0) + v[1].unwrap_or(0.0) > 0.0).collect();
    busy.sort_by(|a, b| {
        let k = |v: &[Option<f64>; 4]| v[0].unwrap_or(0.0) + v[1].unwrap_or(0.0);
        k(&b.1).total_cmp(&k(&a.1))
    });
    if !busy.is_empty() {
        let mut t = MonitorTable::new(
            "top_activity",
            "Tablas con más pedidos",
            &["tabla", "lecturas", "escrituras", "p99 lectura (ms)", "p99 escritura (ms)"],
        );
        for ((ks, tb), v) in busy.into_iter().take(20) {
            t.rows.push(vec![
                J::String(format!("{ks}.{tb}")),
                v[0].map(|x| json!(x)).unwrap_or(J::Null),
                v[1].map(|x| json!(x)).unwrap_or(J::Null),
                v[2].map(|x| json!(x)).unwrap_or(J::Null),
                v[3].map(|x| json!(x)).unwrap_or(J::Null),
            ]);
        }
        snap.tables.push(t);
    }

    // Compactions and other SSTable tasks.
    if let Ok(tasks) = query(session, "SELECT * FROM system_views.sstable_tasks").await {
        snap.metrics.push(Metric::new("compactions_running", "Compactaciones en curso", "Actividad", MetricUnit::Count, Some(tasks.len() as f64)));
        let mut t = MonitorTable::new("compactions", "Compactaciones en curso", &["tabla", "tipo", "progreso", "total", "unidad", "avance (%)"]);
        for r in tasks.iter().take(MAX_ROWS) {
            t.rows.push(vec![
                J::String(format!("{}.{}", s(r, "keyspace_name"), s(r, "table_name"))),
                c(r, "kind"),
                c(r, "progress"),
                c(r, "total"),
                c(r, "unit"),
                f(r, "completion_ratio").map(|v| json!((v * 1000.0).round() / 10.0)).unwrap_or(J::Null),
            ]);
        }
        snap.tables.push(t);
    }

    // Nodes and uptime (gossip's generation is the node's start time).
    match query(session, "SELECT * FROM system_views.gossip_info").await {
        Ok(gossip) => {
            let me = s(local, "host_id");
            let started = gossip.iter().find(|g| s(g, "host_id") == me).and_then(|g| f(g, "generation"));
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
            snap.metrics.push(Metric::new("uptime", "Tiempo activo", "Servidor", MetricUnit::Seconds, started.map(|g| (now - g).max(0.0))));
            let mut t = MonitorTable::new("nodes", "Nodos del cluster", &["dirección", "datacenter", "rack", "estado", "carga", "versión", "host_id"]);
            for g in gossip.iter().take(MAX_ROWS) {
                t.rows.push(vec![
                    c(g, "address"),
                    c(g, "dc"),
                    c(g, "rack"),
                    J::String(s(g, "status").split(',').next().unwrap_or_default().to_string()),
                    f(g, "load").map(|v| json!(v)).unwrap_or(J::Null),
                    c(g, "release_version"),
                    c(g, "host_id"),
                ]);
            }
            snap.tables.push(t);
        }
        Err(_) => peers_nodes(session, local, snap).await,
    }

    if let Ok(props) = query(session, "SELECT name, value FROM system_views.system_properties").await {
        let p: HashMap<String, String> = props.iter().map(|r| (s(r, "name"), s(r, "value"))).collect();
        for (k, label) in [("java.version", "Java"), ("os.name", "Sistema operativo"), ("user.timezone", "Zona horaria")] {
            if let Some(v) = p.get(k).filter(|v| !v.is_empty()) {
                snap.info.push((label.into(), v.clone()));
            }
        }
    }
    for (k, label) in [
        ("num_tokens", "Tokens por nodo"),
        ("endpoint_snitch", "Snitch"),
        ("concurrent_reads", "Lecturas concurrentes"),
        ("concurrent_writes", "Escrituras concurrentes"),
        ("memtable_heap_space", "Memtables (heap)"),
        ("key_cache_size", "Caché de claves"),
        ("row_cache_size", "Caché de filas"),
        ("native_transport_max_concurrent_connections", "Conexiones máximas"),
    ] {
        if let Some(v) = setting.get(k).filter(|v| !v.is_empty()) {
            let v = if v == "-1" { "sin límite".to_string() } else { v.clone() };
            snap.info.push((label.into(), v));
        }
    }
}

/// Size per keyspace (`databases`) and the 20 largest tables (`top_objects`).
fn sizes(snap: &mut MonitorSnapshot, items: impl Iterator<Item = (String, String, f64)>) {
    let items: Vec<(String, String, f64)> = items.collect();
    let mut per_ks: BTreeMap<String, (f64, usize)> = BTreeMap::new();
    for (ks, _, b) in &items {
        let e = per_ks.entry(ks.clone()).or_default();
        e.0 += b;
        e.1 += 1;
    }
    let mut t = MonitorTable::new("databases", "Keyspaces y tamaños", &["keyspace", "tablas", "tamaño"]);
    for (ks, (b, n)) in per_ks.into_iter().take(MAX_ROWS) {
        t.rows.push(vec![J::String(ks), json!(n), json!(b)]);
    }
    snap.tables.push(t);
    let mut items = items;
    items.sort_by(|a, b| b.2.total_cmp(&a.2));
    let mut t = MonitorTable::new("top_objects", "Tablas más grandes", &["tabla", "tamaño"]);
    for (ks, tb, b) in items.into_iter().filter(|x| x.2 > 0.0).take(20) {
        t.rows.push(vec![J::String(format!("{ks}.{tb}")), json!(b)]);
    }
    snap.tables.push(t);
}

/// Nodes from `system.local` + `system.peers` (every CQL server has them).
async fn peers_nodes(session: &Session, local: &Rec, snap: &mut MonitorSnapshot) {
    let mut t = MonitorTable::new("nodes", "Nodos del cluster", &["dirección", "datacenter", "rack", "versión", "host_id"]);
    let addr = |r: &Rec| {
        ["broadcast_address", "rpc_address", "peer", "listen_address"]
            .iter()
            .map(|k| s(r, k))
            .find(|v| !v.is_empty())
            .unwrap_or_default()
    };
    t.rows.push(vec![J::String(format!("{} (este)", addr(local))), c(local, "data_center"), c(local, "rack"), c(local, "release_version"), c(local, "host_id")]);
    match query(session, "SELECT * FROM system.peers").await {
        Ok(peers) => {
            for p in peers.iter().take(MAX_ROWS) {
                t.rows.push(vec![J::String(addr(p)), c(p, "data_center"), c(p, "rack"), c(p, "release_version"), c(p, "host_id")]);
            }
        }
        Err(e) => note_missing(snap, "system.peers", &e),
    }
    snap.metrics.push(Metric::new("nodes", "Nodos", "Servidor", MetricUnit::Count, Some(t.rows.len() as f64)));
    snap.tables.push(t);
}

// ------------------------------------------------------------ ScyllaDB

async fn scylla(session: &Session, snap: &mut MonitorSnapshot) {
    snap.notes.push(
        "ScyllaDB publica el uso de CPU, el throughput y las latencias en su endpoint de Prometheus (puerto 9180); por CQL solo se leen memoria, caché, clientes, espacio y nodos."
            .into(),
    );
    if let Ok(v) = query(session, "SELECT version, build_mode FROM system.versions").await {
        if let Some(r) = v.first() {
            snap.info.insert(1, ("Versión de ScyllaDB".into(), s(r, "version")));
            let mode = s(r, "build_mode");
            if !mode.is_empty() {
                snap.info.push(("Compilación".into(), mode));
            }
        }
    }
    let rt: HashMap<(String, String), String> = match query(session, "SELECT group, item, value FROM system.runtime_info").await {
        Ok(rows) => rows.iter().map(|r| ((s(r, "group"), s(r, "item")), s(r, "value"))).collect(),
        Err(e) => {
            note_missing(snap, "system.runtime_info", &e);
            HashMap::new()
        }
    };
    let g = |group: &str, item: &str| rt.get(&(group.to_string(), item.to_string())).and_then(|v| dbine_driver::monitor::num(v));

    snap.metrics.push(Metric::new("mem_used", "Memoria usada", "Memoria", MetricUnit::Bytes, g("memory", "used")).max(g("memory", "total")));
    snap.metrics.push(Metric::new("mem_cache", "Caché de filas", "Memoria", MetricUnit::Bytes, g("cache", "memory_used")).max(g("cache", "memory_total")));
    if let Some(m) = g("memtable", "memory_used") {
        snap.metrics.push(Metric::new("mem_memtable", "Memtables", "Memoria", MetricUnit::Bytes, Some(m)).max(g("memtable", "memory_total").filter(|t| *t > 0.0)));
    }
    let hit = match (g("cache", "hits"), g("cache", "misses")) {
        (Some(h), Some(m)) if h + m > 0.0 => Some(h / (h + m) * 100.0),
        _ => None,
    };
    snap.metrics.push(Metric::new("cache_hit", "Aciertos de caché", "Caché", MetricUnit::Percent, hit).max(Some(100.0)));
    if let Some(r) = g("cache", "requests_total") {
        snap.metrics.push(Metric::new("cache_requests", "Pedidos a la caché", "Caché", MetricUnit::Count, Some(r)).counter());
    }
    if let Some(n) = g("cache", "entries") {
        snap.metrics.push(Metric::new("cache_entries", "Entradas en caché", "Caché", MetricUnit::Count, Some(n)));
    }

    // Clients.
    match query(session, "SELECT * FROM system.clients").await {
        Ok(rows) => {
            snap.metrics.push(Metric::new("connections", "Conexiones", "Conexiones", MetricUnit::Count, Some(rows.len() as f64)));
            let mut t = MonitorTable::new(
                "sessions",
                "Sesiones",
                &["cliente", "usuario", "tipo", "driver", "estado", "shard", "grupo de servicio", "protocolo", "TLS"],
            );
            for r in rows.iter().take(MAX_ROWS) {
                t.rows.push(vec![
                    J::String(format!("{}:{}", s(r, "address"), s(r, "port"))),
                    c(r, "username"),
                    c(r, "client_type"),
                    J::String(format!("{} {}", s(r, "driver_name"), s(r, "driver_version")).trim().to_string()),
                    c(r, "connection_stage"),
                    c(r, "shard_id"),
                    c(r, "scheduling_group"),
                    c(r, "protocol_version"),
                    c(r, "ssl_enabled"),
                ]);
            }
            snap.tables.push(t);
        }
        Err(e) => note_missing(snap, "system.clients", &e),
    }

    // Nodes and space.
    match query(session, "SELECT * FROM system.cluster_status").await {
        Ok(rows) => {
            let load = total(&rows, "load");
            let cap = query(session, "SELECT storage_capacity FROM system.load_per_node")
                .await
                .ok()
                .and_then(|r| total(&r, "storage_capacity"))
                .filter(|c| *c > 0.0);
            snap.metrics.push(Metric::new("storage_used", "Espacio usado (cluster)", "Almacenamiento", MetricUnit::Bytes, load).max(cap));
            let up = rows.iter().filter(|r| matches!(r.get("up"), Some(Some(CqlValue::Boolean(true))))).count();
            snap.metrics.push(Metric::new("nodes_up", "Nodos activos", "Servidor", MetricUnit::Count, Some(up as f64)).max(Some(rows.len() as f64)));
            let mut t = MonitorTable::new("nodes", "Nodos del cluster", &["dirección", "datacenter", "rack", "estado", "activo", "carga", "tokens", "posesión", "host_id"]);
            for r in rows.iter().take(MAX_ROWS) {
                t.rows.push(vec![c(r, "peer"), c(r, "dc"), c(r, "rack"), c(r, "status"), c(r, "up"), c(r, "load"), c(r, "tokens"), c(r, "owns"), c(r, "host_id")]);
            }
            snap.tables.push(t);
        }
        Err(_) => {
            let local = query(session, "SELECT * FROM system.local").await.ok().and_then(|v| v.into_iter().next()).unwrap_or_default();
            peers_nodes(session, &local, snap).await;
        }
    }
    snap.metrics.push(Metric::new("uptime", "Tiempo activo", "Servidor", MetricUnit::Seconds, rt.get(&("generic".into(), "uptime".into())).and_then(|u| parse_uptime(u))));

    // Compactions in progress and large partitions.
    if let Ok(rows) = query(session, "SELECT * FROM system.compactions_in_progress").await {
        snap.metrics.push(Metric::new("compactions_running", "Compactaciones en curso", "Actividad", MetricUnit::Count, Some(rows.len() as f64)));
        let mut t = MonitorTable::new("compactions", "Compactaciones en curso", &["tabla", "id"]);
        for r in rows.iter().take(MAX_ROWS) {
            t.rows.push(vec![J::String(format!("{}.{}", s(r, "keyspace_name"), s(r, "columnfamily_name"))), c(r, "id")]);
        }
        snap.tables.push(t);
    }
    if let Ok(rows) = query(session, "SELECT keyspace_name, table_name, partition_key, partition_size, rows FROM system.large_partitions LIMIT 20").await {
        let mut t = MonitorTable::new("top_objects", "Particiones grandes", &["tabla", "clave de partición", "tamaño", "filas"]);
        for r in &rows {
            t.rows.push(vec![J::String(format!("{}.{}", s(r, "keyspace_name"), s(r, "table_name"))), J::String(clip(&s(r, "partition_key"), 200)), c(r, "partition_size"), c(r, "rows")]);
        }
        snap.tables.push(t);
    }
    if let Ok(rows) = query(
        session,
        "SELECT name, value FROM system.config WHERE name IN ('num_tokens', 'endpoint_snitch', 'authenticator', 'enable_tablets', 'developer_mode')",
    )
    .await
    {
        for r in rows {
            let v = s(&r, "value");
            let label = match s(&r, "name").as_str() {
                "num_tokens" => "Tokens por nodo",
                "endpoint_snitch" => "Snitch",
                "authenticator" => "Autenticación",
                "enable_tablets" => "Tablets",
                "developer_mode" => "Modo desarrollador",
                _ => continue,
            };
            if !v.is_empty() {
                snap.info.push((label.into(), v.trim_matches('"').to_string()));
            }
        }
    }
}

// ------------------------------------------------------------ Keyspaces

async fn keyspaces(session: &Session, snap: &mut MonitorSnapshot) {
    snap.notes.push(
        "Amazon Keyspaces es un servicio administrado: CPU, memoria, capacidad consumida y latencias se ven en Amazon CloudWatch, no por CQL."
            .into(),
    );
    let local = query(session, "SELECT * FROM system.local").await.ok().and_then(|v| v.into_iter().next()).unwrap_or_default();
    peers_nodes(session, &local, snap).await;
    match query(session, "SELECT keyspace_name, table_name, status, custom_properties FROM system_schema_mcs.tables").await {
        Ok(rows) => {
            let mut t = MonitorTable::new("tables", "Tablas y modo de capacidad", &["tabla", "estado", "propiedades"]);
            let mut rows: Vec<Rec> = rows.into_iter().filter(|r| !crate::is_system_keyspace(&s(r, "keyspace_name"))).collect();
            rows.sort_by_key(|r| (s(r, "keyspace_name"), s(r, "table_name")));
            snap.metrics.push(Metric::new("tables", "Tablas", "Almacenamiento", MetricUnit::Count, Some(rows.len() as f64)));
            for r in rows.iter().take(MAX_ROWS) {
                t.rows.push(vec![
                    J::String(format!("{}.{}", s(r, "keyspace_name"), s(r, "table_name"))),
                    c(r, "status"),
                    J::String(clip(&s(r, "custom_properties"), 2000)),
                ]);
            }
            snap.tables.push(t);
        }
        Err(e) => note_missing(snap, "system_schema_mcs.tables", &e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uptimes() {
        assert_eq!(parse_uptime("64 seconds"), Some(64.0));
        assert_eq!(parse_uptime("1 hour 2 minutes 3 seconds"), Some(3723.0));
        assert_eq!(parse_uptime("2 days"), Some(172_800.0));
        assert_eq!(parse_uptime("n/a"), None);
    }

    #[test]
    fn keyspace_sizes() {
        let mut snap = MonitorSnapshot::default();
        sizes(
            &mut snap,
            vec![("a".to_string(), "t1".to_string(), 10.0), ("a".into(), "t2".into(), 30.0), ("b".into(), "t".into(), 0.0)].into_iter(),
        );
        assert_eq!(snap.tables[0].rows[0], vec![json!("a"), json!(2), json!(40.0)]);
        assert_eq!(snap.tables[1].rows.len(), 2);
        assert_eq!(snap.tables[1].rows[0][0], json!("a.t2"));
    }

    #[test]
    fn numbers() {
        assert_eq!(cql_f64(&CqlValue::Double(f64::NAN)), None);
        assert_eq!(cql_f64(&CqlValue::Text("117453.0".into())), Some(117453.0));
        assert_eq!(cql_f64(&CqlValue::BigInt(5)), Some(5.0));
    }
}
