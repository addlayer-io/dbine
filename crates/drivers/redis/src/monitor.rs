//! `Session::monitor` for Redis, Valkey and Dragonfly: `INFO all` (CPU
//! time, memory, clients, commands, keyspace hits and misses, evictions,
//! network, replication, command stats, keyspace), `CLIENT LIST`,
//! `SLOWLOG GET` and, in cluster mode, `CLUSTER NODES`. A command a
//! managed service disables (they often rename `CONFIG` or `CLIENT`) is
//! skipped with a note.

use crate::shape;
use crate::RedisSession;
use dbine_driver::monitor::{num, Metric, MetricUnit, MonitorSnapshot, MonitorTable};
use redis::Value;
use serde_json::{json, Value as J};
use std::collections::HashMap;

const MAX_ROWS: usize = 200;

/// `INFO` text → `field → value` (section headers and blanks skipped).
pub(crate) fn parse_info(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.trim().split_once(':'))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// `k1=v1,k2=v2` (keyspace lines, replica lines, commandstats).
fn pairs(s: &str) -> HashMap<&str, &str> {
    s.split(',').filter_map(|p| p.split_once('=')).collect()
}

fn n(info: &HashMap<String, String>, k: &str) -> Option<f64> {
    info.get(k).and_then(|v| num(v))
}

fn cell(v: Option<&str>) -> J {
    match v {
        None => J::Null,
        Some(s) => num(s).map(|n| json!(n)).unwrap_or_else(|| J::String(s.to_string())),
    }
}

pub(crate) fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// The metrics and tables `INFO all` gives.
pub(crate) fn from_info(s: &mut MonitorSnapshot, info: &HashMap<String, String>, maxclients: Option<f64>) {
    let product = if info.contains_key("dragonfly_version") {
        "Dragonfly"
    } else if info.contains_key("valkey_version") {
        "Valkey"
    } else {
        "Redis"
    };

    // CPU.
    let cpu = match (n(info, "used_cpu_sys"), n(info, "used_cpu_user")) {
        (Some(a), Some(b)) => Some((a + b) * 100.0),
        (a, b) => a.or(b).map(|x| x * 100.0),
    };
    s.metrics.push(Metric::new("cpu_time", "CPU del proceso", "CPU", MetricUnit::Percent, cpu).counter());
    s.notes.push(format!("{product} no informa el uso de CPU del host; se muestra el tiempo de CPU del proceso (la tasa es el % de un núcleo)."));

    // Memory.
    let maxmemory = n(info, "maxmemory").filter(|m| *m > 0.0);
    let ceiling = maxmemory.or_else(|| n(info, "total_system_memory").filter(|m| *m > 0.0));
    s.metrics.push(Metric::new("mem_used", "Memoria usada", "Memoria", MetricUnit::Bytes, n(info, "used_memory")).max(ceiling));
    if let Some(v) = n(info, "used_memory_rss") {
        s.metrics.push(Metric::new("mem_rss", "Memoria residente (RSS)", "Memoria", MetricUnit::Bytes, Some(v)).max(ceiling));
    }
    if let Some(v) = n(info, "used_memory_peak") {
        s.metrics.push(Metric::new("mem_peak", "Pico de memoria", "Memoria", MetricUnit::Bytes, Some(v)));
    }
    if let Some(v) = n(info, "mem_fragmentation_ratio") {
        s.metrics.push(Metric::new("mem_fragmentation", "Fragmentación (×100)", "Memoria", MetricUnit::Percent, Some(v * 100.0)));
    }

    // Clients.
    let maxclients = n(info, "maxclients").or(maxclients);
    s.metrics.push(
        Metric::new("connections", "Conexiones", "Conexiones", MetricUnit::Count, n(info, "connected_clients")).max(maxclients),
    );
    if let Some(v) = n(info, "blocked_clients") {
        s.metrics.push(Metric::new("blocked_clients", "Clientes bloqueados", "Conexiones", MetricUnit::Count, Some(v)));
    }
    if let Some(v) = n(info, "total_connections_received") {
        s.metrics.push(Metric::new("connections_received", "Conexiones recibidas", "Conexiones", MetricUnit::Count, Some(v)).counter());
    }
    if let Some(v) = n(info, "rejected_connections") {
        s.metrics.push(Metric::new("rejected_connections", "Conexiones rechazadas", "Conexiones", MetricUnit::Count, Some(v)).counter());
    }

    // Activity.
    s.metrics.push(
        Metric::new("queries", "Comandos", "Actividad", MetricUnit::Count, n(info, "total_commands_processed")).counter(),
    );
    if let Some(v) = n(info, "instantaneous_ops_per_sec") {
        s.metrics.push(Metric::new("ops_per_sec", "Operaciones por segundo", "Actividad", MetricUnit::Count, Some(v)));
    }
    if let Some(v) = n(info, "expired_keys") {
        s.metrics.push(Metric::new("expired_keys", "Claves vencidas", "Actividad", MetricUnit::Count, Some(v)).counter());
    }

    // Network.
    s.metrics.push(Metric::new("net_in", "Red entrante", "Red", MetricUnit::Bytes, n(info, "total_net_input_bytes")).counter());
    s.metrics.push(Metric::new("net_out", "Red saliente", "Red", MetricUnit::Bytes, n(info, "total_net_output_bytes")).counter());

    // Cache.
    let hits = n(info, "keyspace_hits");
    let misses = n(info, "keyspace_misses");
    let ratio = match (hits, misses) {
        (Some(h), Some(m)) if h + m > 0.0 => Some(h / (h + m) * 100.0),
        _ => None,
    };
    s.metrics.push(Metric::new("cache_hit", "Aciertos de caché", "Caché", MetricUnit::Percent, ratio).max(Some(100.0)));
    if hits.is_some() {
        s.metrics.push(Metric::new("keyspace_hits", "Aciertos", "Caché", MetricUnit::Count, hits).counter());
        s.metrics.push(Metric::new("keyspace_misses", "Fallos", "Caché", MetricUnit::Count, misses).counter());
    }
    s.metrics.push(Metric::new("evictions", "Claves desalojadas", "Caché", MetricUnit::Count, n(info, "evicted_keys")).counter());

    // Keyspace.
    let mut dbs: Vec<(i64, HashMap<&str, &str>)> = info
        .iter()
        .filter_map(|(k, v)| Some((k.strip_prefix("db")?.parse().ok()?, pairs(v))))
        .collect();
    dbs.sort_by_key(|(i, _)| *i);
    let keys: f64 = dbs.iter().filter_map(|(_, p)| p.get("keys").and_then(|v| num(v))).sum();
    s.metrics.push(Metric::new("keys", "Claves", "Almacenamiento", MetricUnit::Count, Some(keys)));
    if let Some(v) = n(info, "rdb_changes_since_last_save") {
        s.metrics.push(Metric::new("unsaved_changes", "Cambios sin persistir", "Almacenamiento", MetricUnit::Count, Some(v)));
    }
    let mut table = MonitorTable::new("databases", "Bases (keyspace)", &["base", "claves", "con vencimiento", "TTL promedio (ms)"]);
    for (i, p) in &dbs {
        table.rows.push(vec![
            J::String(format!("db{i}")),
            cell(p.get("keys").copied()),
            cell(p.get("expires").copied()),
            cell(p.get("avg_ttl").copied()),
        ]);
    }
    s.tables.push(table);

    // Replication.
    let role = info.get("role").map(String::as_str).unwrap_or("");
    let mut repl = MonitorTable::new(
        "replication",
        "Réplicas",
        &["réplica", "estado", "offset", "retraso (s)"],
    );
    let lag = if role == "slave" || role == "replica" {
        let link = info.get("master_link_status").map(String::as_str);
        repl.rows.push(vec![
            J::String(format!(
                "primario {}:{}",
                info.get("master_host").map(String::as_str).unwrap_or("?"),
                info.get("master_port").map(String::as_str).unwrap_or("?")
            )),
            cell(link),
            cell(info.get("slave_repl_offset").or_else(|| info.get("master_repl_offset")).map(String::as_str)),
            cell(info.get("master_last_io_seconds_ago").map(String::as_str)),
        ]);
        n(info, "master_last_io_seconds_ago").filter(|v| *v >= 0.0)
    } else {
        let mut worst: Option<f64> = None;
        let mut replicas: Vec<(&String, &String)> =
            info.iter().filter(|(k, _)| k.strip_prefix("slave").is_some_and(|r| r.parse::<u32>().is_ok())).collect();
        replicas.sort();
        for (_, v) in replicas {
            let p = pairs(v);
            let l = p.get("lag").and_then(|x| num(x));
            if let Some(l) = l {
                worst = Some(worst.map_or(l, |w| w.max(l)));
            }
            repl.rows.push(vec![
                J::String(format!("{}:{}", p.get("ip").unwrap_or(&"?"), p.get("port").unwrap_or(&"?"))),
                cell(p.get("state").copied()),
                cell(p.get("offset").copied()),
                cell(p.get("lag").copied()),
            ]);
        }
        worst
    };
    s.metrics.push(Metric::new("replication_lag", "Retraso de réplica", "Replicación", MetricUnit::Seconds, lag));
    if let Some(v) = n(info, "connected_slaves") {
        s.metrics.push(Metric::new("replicas", "Réplicas conectadas", "Replicación", MetricUnit::Count, Some(v)));
    }
    if !repl.rows.is_empty() {
        s.tables.push(repl);
    }

    // Server.
    s.metrics.push(Metric::new("uptime", "Tiempo activo", "Servidor", MetricUnit::Seconds, n(info, "uptime_in_seconds")));

    // Command statistics, busiest first.
    let mut stats: Vec<(f64, Vec<J>)> = info
        .iter()
        .filter_map(|(k, v)| {
            let name = k.strip_prefix("cmdstat_")?;
            let p = pairs(v);
            let calls = p.get("calls").and_then(|x| num(x))?;
            Some((
                calls,
                vec![
                    J::String(name.to_string()),
                    json!(calls),
                    p.get("usec").and_then(|x| num(x)).map(|u| json!(u / 1000.0)).unwrap_or(J::Null),
                    cell(p.get("usec_per_call").copied()),
                    cell(p.get("failed_calls").copied()),
                    cell(p.get("rejected_calls").copied()),
                ],
            ))
        })
        .collect();
    if !stats.is_empty() {
        stats.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut t = MonitorTable::new(
            "top_commands",
            "Comandos más usados",
            &["comando", "llamadas", "tiempo total (ms)", "µs por llamada", "con error", "rechazadas"],
        );
        t.rows = stats.into_iter().take(20).map(|(_, r)| r).collect();
        s.tables.push(t);
    }

    // Info.
    let version = info
        .get("dragonfly_version")
        .or_else(|| info.get("valkey_version"))
        .or_else(|| info.get("redis_version"))
        .map(|v| v.trim_start_matches("df-v").to_string());
    s.info.push(("Producto".into(), product.into()));
    if let Some(v) = version {
        s.info.push(("Versión".into(), v));
    }
    for (k, label) in [
        ("redis_mode", "Modo"),
        ("role", "Rol"),
        ("os", "Sistema operativo"),
        ("arch_bits", "Arquitectura (bits)"),
        ("tcp_port", "Puerto"),
        ("maxmemory_human", "Memoria máxima"),
        ("maxmemory_policy", "Política de desalojo"),
        ("total_system_memory_human", "Memoria del host"),
        ("mem_allocator", "Asignador de memoria"),
        ("aof_enabled", "AOF activado"),
        ("cluster_enabled", "Cluster"),
        ("io_threads_active", "Hilos de E/S activos"),
        ("thread_count", "Hilos"),
    ] {
        if k == "maxmemory_human" && maxmemory.is_none() {
            continue;
        }
        if let Some(v) = info.get(k).filter(|v| !v.is_empty()) {
            let v = match (k, v.as_str()) {
                ("aof_enabled" | "cluster_enabled", "0") => "no".to_string(),
                ("aof_enabled" | "cluster_enabled", "1") => "sí".to_string(),
                _ => v.clone(),
            };
            s.info.push((label.into(), v));
        }
    }
    if let Some(m) = maxclients {
        s.info.push(("Clientes máximos".into(), format!("{m}")));
    }
    s.notes.push(format!("{product} guarda los datos en memoria: el espacio usado es la memoria; no hay tamaños en disco por base."));
    if maxmemory.is_none() && info.contains_key("maxmemory") {
        s.notes.push("maxmemory no está configurado: el tope de memoria que se muestra es la memoria del host.".into());
    }
}

/// `CLIENT LIST` text → the sessions table (newest activity first).
pub(crate) fn clients(text: &str) -> MonitorTable {
    let mut rows: Vec<(f64, Vec<J>)> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let p: HashMap<&str, &str> = l.split_whitespace().filter_map(|f| f.split_once('=')).collect();
            let idle = p.get("idle").and_then(|v| num(v)).unwrap_or(0.0);
            let cmd = p.get("cmd").copied().filter(|c| *c != "NULL");
            let state = if cmd.is_some() && idle == 0.0 { "activa" } else { "inactiva" };
            (
                idle,
                vec![
                    cell(p.get("id").copied()),
                    cell(p.get("user").copied()),
                    p.get("db").map(|d| J::String(format!("db{d}"))).unwrap_or(J::Null),
                    cell(p.get("addr").copied()),
                    cell(p.get("name").copied().filter(|n| !n.is_empty())),
                    J::String(state.into()),
                    cell(p.get("age").copied()),
                    json!(idle),
                    cell(p.get("flags").copied()),
                    cmd.map(|c| J::String(clip(c, 2000))).unwrap_or(J::Null),
                ],
            )
        })
        .collect();
    rows.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut t = MonitorTable::new(
        "sessions",
        "Sesiones",
        &["id", "usuario", "base", "cliente", "nombre", "estado", "duración (s)", "inactiva (s)", "flags", "último comando"],
    );
    t.rows = rows.into_iter().take(MAX_ROWS).map(|(_, r)| r).collect();
    t
}

/// `SLOWLOG GET` reply → the slow-queries table.
pub(crate) fn slowlog(v: &Value) -> MonitorTable {
    let mut t = MonitorTable::new(
        "queries",
        "Consultas lentas (SLOWLOG)",
        &["id", "fecha", "duración (ms)", "comando", "cliente", "nombre"],
    );
    let Value::Array(entries) = v else { return t };
    for e in entries.iter().take(MAX_ROWS) {
        let Value::Array(f) = e else { continue };
        let int = |i: usize| match f.get(i) {
            Some(Value::Int(n)) => Some(*n),
            _ => None,
        };
        let when = int(1)
            .and_then(|ts| chrono_like(ts))
            .map(J::String)
            .unwrap_or(J::Null);
        let cmd = match f.get(3) {
            Some(Value::Array(args)) => args.iter().map(shape::text_of).collect::<Vec<_>>().join(" "),
            _ => String::new(),
        };
        t.rows.push(vec![
            int(0).map(|n| json!(n)).unwrap_or(J::Null),
            when,
            int(2).map(|us| json!(us as f64 / 1000.0)).unwrap_or(J::Null),
            J::String(clip(&cmd, 2000)),
            f.get(4).map(|v| J::String(shape::text_of(v))).unwrap_or(J::Null),
            f.get(5).map(|v| J::String(shape::text_of(v))).filter(|v| v != "").unwrap_or(J::Null),
        ]);
    }
    t
}

/// Unix seconds → `YYYY-MM-DD HH:MM:SS` (UTC), without a date crate.
fn chrono_like(ts: i64) -> Option<String> {
    if ts < 0 {
        return None;
    }
    let days = ts / 86_400;
    let rem = ts % 86_400;
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    Some(format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60))
}

/// `CLUSTER NODES` text → the nodes table.
pub(crate) fn cluster_nodes(text: &str) -> MonitorTable {
    let mut t = MonitorTable::new("nodes", "Nodos del cluster", &["id", "dirección", "rol", "primario", "estado", "slots"]);
    for l in text.lines().filter(|l| !l.trim().is_empty()).take(MAX_ROWS) {
        let f: Vec<&str> = l.split_whitespace().collect();
        if f.len() < 8 {
            continue;
        }
        let role = f[2].split(',').filter(|x| *x != "myself").collect::<Vec<_>>().join(",");
        t.rows.push(vec![
            J::String(f[0].chars().take(12).collect()),
            J::String(f[1].split('@').next().unwrap_or(f[1]).into()),
            J::String(role),
            J::String(if f[3] == "-" { String::new() } else { f[3].chars().take(12).collect() }),
            J::String(f[7].into()),
            J::String(f[8..].join(" ")),
        ]);
    }
    t
}

impl RedisSession {
    pub(crate) async fn snapshot(&mut self) -> dbine_driver::Result<MonitorSnapshot> {
        let mut s = MonitorSnapshot::default();
        // INFO all is the one command the snapshot can't do without.
        let text = match self.run(&[b"INFO", b"all"]).await {
            Ok(v) => shape::text_of(&v),
            Err(_) => shape::text_of(&self.run(&[b"INFO"]).await?),
        };
        let info = parse_info(&text);
        let maxclients = if info.contains_key("maxclients") {
            None
        } else {
            match self.run(&[b"CONFIG", b"GET", b"maxclients"]).await {
                Ok(Value::Array(a)) => a.get(1).map(shape::text_of).and_then(|v| num(&v)),
                Ok(Value::Map(m)) => m.first().map(|(_, v)| shape::text_of(v)).and_then(|v| num(&v)),
                _ => None,
            }
        };
        from_info(&mut s, &info, maxclients);

        match self.run(&[b"CLIENT", b"LIST"]).await {
            Ok(v) => {
                let t = clients(&shape::text_of(&v));
                let active = t.rows.iter().filter(|r| r[5] == json!("activa")).count();
                s.metrics.insert(
                    s.metrics.iter().position(|m| m.key == "connections").map_or(0, |i| i + 1),
                    Metric::new("active_sessions", "Sesiones activas", "Conexiones", MetricUnit::Count, Some(active as f64)),
                );
                s.tables.insert(0, t);
            }
            Err(e) => s.notes.push(format!("No se pudo leer CLIENT LIST (en servicios administrados suele estar deshabilitado): {e}")),
        }
        match self.run(&[b"SLOWLOG", b"GET", b"50"]).await {
            Ok(v) => s.tables.insert(1.min(s.tables.len()), slowlog(&v)),
            Err(e) => s.notes.push(format!("No se pudo leer SLOWLOG: {e}")),
        }
        if info.get("cluster_enabled").map(String::as_str) == Some("1") {
            match self.run(&[b"CLUSTER", b"NODES"]).await {
                Ok(v) => s.tables.push(cluster_nodes(&shape::text_of(&v))),
                Err(e) => s.notes.push(format!("No se pudo leer CLUSTER NODES: {e}")),
            }
        }
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const INFO: &str = "# Server\r\nredis_version:7.2.4\r\nredis_mode:standalone\r\nuptime_in_seconds:120\r\n\
# Clients\r\nconnected_clients:3\r\nmaxclients:10000\r\nblocked_clients:0\r\n\
# Memory\r\nused_memory:1048576\r\nmaxmemory:0\r\ntotal_system_memory:8589934592\r\n\
# Stats\r\ntotal_commands_processed:500\r\nkeyspace_hits:30\r\nkeyspace_misses:10\r\nevicted_keys:0\r\n\
total_net_input_bytes:100\r\ntotal_net_output_bytes:200\r\n\
# Replication\r\nrole:master\r\nconnected_slaves:1\r\nslave0:ip=10.0.0.2,port=6380,state=online,offset=42,lag=2\r\n\
# CPU\r\nused_cpu_sys:1.5\r\nused_cpu_user:2.5\r\n\
# Commandstats\r\ncmdstat_get:calls=10,usec=100,usec_per_call=10.00,rejected_calls=0,failed_calls=0\r\n\
cmdstat_set:calls=20,usec=300,usec_per_call=15.00,rejected_calls=0,failed_calls=1\r\n\
# Keyspace\r\ndb0:keys=5,expires=1,avg_ttl=0\r\ndb2:keys=7,expires=0,avg_ttl=0\r\n";

    #[test]
    fn info_metrics() {
        let mut s = MonitorSnapshot::default();
        from_info(&mut s, &parse_info(INFO), None);
        let m = |k: &str| s.metrics.iter().find(|m| m.key == k).unwrap();
        assert_eq!(m("cpu_time").value, Some(400.0));
        assert!(m("cpu_time").counter);
        assert_eq!(m("mem_used").max, Some(8589934592.0));
        assert_eq!(m("connections").max, Some(10000.0));
        assert_eq!(m("cache_hit").value, Some(75.0));
        assert_eq!(m("keys").value, Some(12.0));
        assert_eq!(m("replication_lag").value, Some(2.0));
        let t = |k: &str| s.tables.iter().find(|t| t.key == k).unwrap();
        assert_eq!(t("databases").rows.len(), 2);
        assert_eq!(t("top_commands").rows[0][0], json!("set"));
        assert_eq!(t("replication").rows[0][0], json!("10.0.0.2:6380"));
    }

    #[test]
    fn client_list() {
        let t = clients(
            "id=3 addr=127.0.0.1:5000 name= age=10 idle=5 flags=N db=0 cmd=get user=default\n\
             id=4 addr=127.0.0.1:5001 name=app age=2 idle=0 flags=N db=1 cmd=client|list user=default\n",
        );
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[0][0], json!(4.0));
        assert_eq!(t.rows[0][5], json!("activa"));
        assert_eq!(t.rows[1][2], json!("db0"));
    }

    #[test]
    fn slowlog_entries() {
        let v = Value::Array(vec![Value::Array(vec![
            Value::Int(1),
            Value::Int(1_700_000_000),
            Value::Int(2500),
            Value::Array(vec![Value::BulkString(b"KEYS".to_vec()), Value::BulkString(b"*".to_vec())]),
            Value::BulkString(b"127.0.0.1:1".to_vec()),
            Value::BulkString(b"".to_vec()),
        ])]);
        let t = slowlog(&v);
        assert_eq!(t.rows[0][1], json!("2023-11-14 22:13:20"));
        assert_eq!(t.rows[0][2], json!(2.5));
        assert_eq!(t.rows[0][3], json!("KEYS *"));
    }

    #[test]
    fn nodes() {
        let t = cluster_nodes("07c3 127.0.0.1:30004@31004 slave e7d1 0 1 4 connected\n67ed 127.0.0.1:30002@31002 myself,master - 0 0 2 connected 5461-10922\n");
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[1][2], json!("master"));
        assert_eq!(t.rows[1][5], json!("5461-10922"));
    }
}
