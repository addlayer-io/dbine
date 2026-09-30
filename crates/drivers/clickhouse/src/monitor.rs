//! `Session::monitor`: ClickHouse (and Timeplus Proton, a fork) describe
//! themselves in `system.*`: `asynchronous_metrics` (OS CPU and memory,
//! caches), `metrics` (gauges: connections, running queries), `events`
//! (counters since start), `processes`, `tables`, `merges`, `mutations`,
//! `replicas`, `disks` and `clusters`. Each part that fails (an older
//! version, a missing grant) is left out with a note.

use crate::{text, Body, ClickHouseSession, Flavor};
use dbine_driver::monitor::num;
use dbine_driver::{Metric, MetricUnit, MonitorSnapshot, MonitorTable, QueryOutcome, Result};
use serde_json::Value;
use std::collections::HashMap;

const MAX_ROWS: usize = 200;

/// A number from a JSON cell (64-bit integers come quoted).
fn fnum(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => num(s),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

type Kv = HashMap<String, f64>;

fn get(kv: &Kv, k: &str) -> Option<f64> {
    kv.get(k).copied()
}

fn sum(kv: &Kv, keys: &[&str]) -> Option<f64> {
    keys.iter().filter_map(|k| get(kv, k)).fold(None, |a, v| Some(a.unwrap_or(0.0) + v))
}

fn metric(key: &str, label: &str, group: &str, unit: MetricUnit, value: Option<f64>) -> Metric {
    Metric::new(key, label, group, unit, value)
}

fn add(s: &mut MonitorSnapshot, mut m: Metric) {
    if m.value.is_some() {
        // An empty f64 sum is -0.0.
        m.value = m.value.map(|v| v + 0.0);
        s.metrics.push(m);
    }
}

fn why(e: &dbine_driver::Error) -> String {
    let s = e.to_string();
    let first = s.lines().next().unwrap_or_default().trim();
    // "Code: 497. DB::Exception: user: Not enough privileges…" → the message.
    let msg = first.split("DB::Exception: ").nth(1).unwrap_or(first);
    let msg = msg.split(" (version").next().unwrap_or(msg);
    msg.chars().take(200).collect()
}

fn bytes_text(b: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let (mut v, mut i) = (b, 0);
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}

struct Grid {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
}

impl Grid {
    fn table(self, key: &str, title: &str) -> MonitorTable {
        let cols: Vec<&str> = self.columns.iter().map(String::as_str).collect();
        let mut t = MonitorTable::new(key, title, &cols);
        t.rows = self.rows;
        t
    }

    /// Only these columns `(source, label)`, those the server has.
    fn project(&self, key: &str, title: &str, pick: &[(&str, &str)]) -> MonitorTable {
        let cols: Vec<(usize, &str)> = pick.iter().filter_map(|(n, l)| Some((self.col(n)?, *l))).collect();
        let labels: Vec<&str> = cols.iter().map(|(_, l)| *l).collect();
        let mut t = MonitorTable::new(key, title, &labels);
        t.rows = self.rows.iter().map(|r| cols.iter().map(|(i, _)| r.get(*i).cloned().unwrap_or(Value::Null)).collect()).collect();
        t
    }

    fn col(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c == name)
    }
}

impl ClickHouseSession {
    /// Column names and rows of a query (at most `MAX_ROWS`).
    async fn grid(&self, sql: &str) -> Result<Grid> {
        let mut out = QueryOutcome::default();
        let res = async {
            if let Body::Rows(resp) = self.send(sql, &[], false).await? {
                crate::read_rows(resp, &mut out, MAX_ROWS, false).await?;
            }
            Ok::<(), dbine_driver::Error>(())
        }
        .await;
        self.done();
        res?;
        let r = out.results.pop().unwrap_or_default();
        let rows = r
            .rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|v| match v {
                        Value::String(s) if s.chars().count() > 2000 => Value::String(s.chars().take(2000).collect::<String>() + "…"),
                        v => v,
                    })
                    .collect()
            })
            .collect();
        Ok(Grid { columns: r.columns.into_iter().map(|c| c.name).collect(), rows })
    }

    /// `name, value` pairs as numbers.
    async fn kv(&self, sql: &str) -> Result<Kv> {
        let rows = self.rows(sql, &[]).await?;
        Ok(rows.iter().filter_map(|r| Some((text(r.first()?), fnum(r.get(1)?)?))).collect())
    }

    pub(crate) async fn snapshot(&mut self) -> Result<MonitorSnapshot> {
        let mut s = MonitorSnapshot::default();
        // The one query that must work.
        let head = self.rows("SELECT version(), uptime(), timezone(), hostName()", &[]).await?;
        let head = head.first().cloned().unwrap_or_default();
        s.info.push(("Versión".into(), head.first().map(text).unwrap_or_default()));
        let uptime = head.get(1).and_then(fnum);
        if let Some(tz) = head.get(2).map(text).filter(|t| !t.is_empty()) {
            s.info.push(("Zona horaria".into(), tz));
        }
        if let Some(h) = head.get(3).map(text).filter(|t| !t.is_empty()) {
            s.info.push(("Servidor".into(), h));
        }
        let product = match self.flavor {
            Flavor::ClickHouse => "ClickHouse",
            Flavor::Timeplus => "Timeplus Proton",
        };

        let asy = self
            .kv(
                "SELECT metric, value FROM system.asynchronous_metrics
                 WHERE metric IN ('OSIdleTimeNormalized', 'OSIOWaitTimeNormalized', 'OSMemoryTotal', 'OSMemoryAvailable',
                                  'MemoryResident', 'NumberOfPhysicalCPUCores', 'NumberOfLogicalCPUCores', 'CGroupMemoryTotal',
                                  'CGroupMaxCPU', 'MaxPartCountForPartition', 'TotalPartsOfMergeTreeTables',
                                  'TotalBytesOfMergeTreeTables', 'TotalRowsOfMergeTreeTables', 'NumberOfTables',
                                  'ReplicasMaxAbsoluteDelay', 'LoadAverage1')
                    OR metric LIKE '%CacheBytes'",
            )
            .await
            .unwrap_or_else(|e| {
                s.notes.push(format!("No se pudo leer system.asynchronous_metrics ({}).", why(&e)));
                Kv::new()
            });
        let gauges = self
            .kv(
                "SELECT metric, value FROM system.metrics
                 WHERE metric IN ('TCPConnection', 'HTTPConnection', 'MySQLConnection', 'PostgreSQLConnection',
                                  'InterserverConnection', 'Query', 'Merge', 'PartMutation', 'MemoryTracking',
                                  'RWLockWaitingReaders', 'RWLockWaitingWriters', 'DelayedInserts', 'BackgroundMergesAndMutationsPoolTask')",
            )
            .await
            .unwrap_or_else(|e| {
                s.notes.push(format!("No se pudo leer system.metrics ({}).", why(&e)));
                Kv::new()
            });
        let events = self
            .kv(
                "SELECT event, value FROM system.events
                 WHERE event IN ('Query', 'SelectQuery', 'InsertQuery', 'FailedQuery', 'SelectedRows', 'SelectedBytes',
                                 'InsertedRows', 'InsertedBytes', 'NetworkReceiveBytes', 'NetworkSendBytes',
                                 'ReadBufferFromFileDescriptorReadBytes', 'WriteBufferFromFileDescriptorWriteBytes',
                                 'OSReadBytes', 'OSWriteBytes', 'UserTimeMicroseconds', 'SystemTimeMicroseconds',
                                 'MarkCacheHits', 'MarkCacheMisses', 'MergedRows', 'Merge', 'RejectedInserts', 'DelayedInserts')",
            )
            .await
            .unwrap_or_else(|e| {
                s.notes.push(format!("No se pudo leer system.events ({}).", why(&e)));
                Kv::new()
            });
        let settings = self
            .kv(
                "SELECT name, toFloat64OrNull(value) FROM system.server_settings
                 WHERE name IN ('max_connections', 'max_concurrent_queries', 'max_server_memory_usage')",
            )
            .await
            .unwrap_or_default();

        // CPU.
        let cpu = get(&asy, "OSIdleTimeNormalized")
            .map(|idle| (100.0 * (1.0 - idle - get(&asy, "OSIOWaitTimeNormalized").unwrap_or(0.0))).clamp(0.0, 100.0));
        add(&mut s, metric("cpu", "CPU del host", "CPU", MetricUnit::Percent, cpu).max(Some(100.0)));
        let proc_cpu = sum(&events, &["UserTimeMicroseconds", "SystemTimeMicroseconds"]).map(|us| us / 1e4);
        add(&mut s, metric("cpu_time", "CPU de las consultas", "CPU", MetricUnit::Percent, proc_cpu).counter());
        add(&mut s, metric("load1", "Carga (1 min)", "CPU", MetricUnit::Count, get(&asy, "LoadAverage1")));
        if cpu.is_none() {
            s.notes.push(format!("{product} no informó el uso de CPU del host (system.asynchronous_metrics sin métricas OS*)."));
        }

        // Memory.
        let total = get(&asy, "CGroupMemoryTotal").filter(|t| *t > 0.0).or(get(&asy, "OSMemoryTotal"));
        let limit = get(&settings, "max_server_memory_usage").filter(|m| *m > 0.0).or(total);
        let mem = get(&gauges, "MemoryTracking").or(get(&asy, "MemoryResident"));
        add(&mut s, metric("mem_used", "Memoria del servidor", "Memoria", MetricUnit::Bytes, mem).max(limit));
        if let (Some(t), Some(a)) = (get(&asy, "OSMemoryTotal"), get(&asy, "OSMemoryAvailable")) {
            add(&mut s, metric("host_mem", "Memoria del host", "Memoria", MetricUnit::Bytes, Some(t - a)).max(Some(t)));
        }
        let caches: f64 = asy.iter().filter(|(k, _)| k.ends_with("CacheBytes")).map(|(_, v)| v).sum();
        let has_caches = asy.keys().any(|k| k.ends_with("CacheBytes"));
        add(&mut s, metric("mem_cache", "Cachés (marcas, sin comprimir, consultas…)", "Memoria", MetricUnit::Bytes, has_caches.then_some(caches)));

        // Connections and activity.
        let conns = sum(&gauges, &["TCPConnection", "HTTPConnection", "MySQLConnection", "PostgreSQLConnection", "InterserverConnection"]);
        add(
            &mut s,
            metric("connections", "Conexiones", "Conexiones", MetricUnit::Count, conns)
                .max(get(&settings, "max_connections").filter(|m| *m > 0.0)),
        );
        add(
            &mut s,
            metric("active_sessions", "Consultas en ejecución", "Conexiones", MetricUnit::Count, get(&gauges, "Query"))
                .max(get(&settings, "max_concurrent_queries").filter(|m| *m > 0.0)),
        );
        add(&mut s, metric("queries", "Consultas", "Actividad", MetricUnit::Count, get(&events, "Query")).counter());
        add(&mut s, metric("select_queries", "SELECT", "Actividad", MetricUnit::Count, get(&events, "SelectQuery")).counter());
        add(&mut s, metric("insert_queries", "INSERT", "Actividad", MetricUnit::Count, get(&events, "InsertQuery")).counter());
        add(&mut s, metric("failed_queries", "Consultas fallidas", "Actividad", MetricUnit::Count, get(&events, "FailedQuery")).counter());
        add(&mut s, metric("rows_read", "Filas leídas", "Actividad", MetricUnit::Count, get(&events, "SelectedRows")).counter());
        add(&mut s, metric("rows_written", "Filas escritas", "Actividad", MetricUnit::Count, get(&events, "InsertedRows")).counter());
        add(&mut s, metric("net_in", "Red entrante", "Red", MetricUnit::Bytes, get(&events, "NetworkReceiveBytes")).counter());
        add(&mut s, metric("net_out", "Red saliente", "Red", MetricUnit::Bytes, get(&events, "NetworkSendBytes")).counter());
        let disk_read = get(&events, "OSReadBytes").or(get(&events, "ReadBufferFromFileDescriptorReadBytes"));
        let disk_write = get(&events, "OSWriteBytes").or(get(&events, "WriteBufferFromFileDescriptorWriteBytes"));
        add(&mut s, metric("disk_read", "Lectura en disco", "Disco", MetricUnit::Bytes, disk_read).counter());
        add(&mut s, metric("disk_write", "Escritura en disco", "Disco", MetricUnit::Bytes, disk_write).counter());
        let hit = match (get(&events, "MarkCacheHits"), get(&events, "MarkCacheMisses")) {
            (Some(h), m) if h + m.unwrap_or(0.0) > 0.0 => Some(100.0 * h / (h + m.unwrap_or(0.0))),
            _ => None,
        };
        add(&mut s, metric("cache_hit", "Aciertos de la caché de marcas", "Caché", MetricUnit::Percent, hit).max(Some(100.0)));

        // MergeTree.
        add(&mut s, metric("merges", "Merges en curso", "MergeTree", MetricUnit::Count, get(&gauges, "Merge")));
        add(&mut s, metric("mutations", "Mutaciones en curso", "MergeTree", MetricUnit::Count, get(&gauges, "PartMutation")));
        add(&mut s, metric("parts", "Partes activas", "MergeTree", MetricUnit::Count, get(&asy, "TotalPartsOfMergeTreeTables")));
        add(
            &mut s,
            metric("max_parts_per_partition", "Máx. partes por partición", "MergeTree", MetricUnit::Count, get(&asy, "MaxPartCountForPartition")),
        );
        add(&mut s, metric("delayed_inserts", "Inserciones demoradas", "MergeTree", MetricUnit::Count, get(&gauges, "DelayedInserts")));
        add(&mut s, metric("merged_rows", "Filas fusionadas", "MergeTree", MetricUnit::Count, get(&events, "MergedRows")).counter());
        let rw = sum(&gauges, &["RWLockWaitingReaders", "RWLockWaitingWriters"]);
        add(&mut s, metric("locks_waiting", "Esperas de bloqueo de tablas", "Bloqueos", MetricUnit::Count, rw));

        // Storage: the disks as the ceiling.
        let disks = self.grid("SELECT * FROM system.disks").await;
        let disk_total = disks.as_ref().ok().and_then(|g| {
            let i = g.col("total_space")?;
            Some(g.rows.iter().filter_map(|r| r.get(i).and_then(fnum)).sum::<f64>())
        });
        let dbs = self
            .grid(
                "SELECT database AS base, count() AS tablas, sum(total_rows) AS filas, sum(total_bytes) AS bytes
                 FROM system.tables WHERE database NOT IN ('INFORMATION_SCHEMA', 'information_schema')
                 GROUP BY database ORDER BY bytes DESC LIMIT 200",
            )
            .await;
        let used = match &dbs {
            Ok(g) => g.col("bytes").map(|i| g.rows.iter().filter_map(|r| r.get(i).and_then(fnum)).sum::<f64>()),
            Err(_) => None,
        }
        .or(get(&asy, "TotalBytesOfMergeTreeTables"));
        add(&mut s, metric("storage_used", "Espacio usado", "Almacenamiento", MetricUnit::Bytes, used).max(disk_total.filter(|t| *t > 0.0)));
        add(&mut s, metric("tables", "Tablas", "Almacenamiento", MetricUnit::Count, get(&asy, "NumberOfTables")));

        // Replication.
        let replicas = self
            .grid(
                "SELECT database AS base, table AS tabla, is_leader AS `líder`, is_readonly AS `solo lectura`,
                        absolute_delay AS `retraso (s)`, queue_size AS cola, inserts_in_queue AS inserciones,
                        merges_in_queue AS merges, active_replicas AS `réplicas activas`, total_replicas AS `réplicas`
                 FROM system.replicas ORDER BY absolute_delay DESC LIMIT 200",
            )
            .await;
        if let Ok(g) = &replicas {
            if !g.rows.is_empty() {
                let lag = g.col("retraso (s)").and_then(|i| g.rows.iter().filter_map(|r| r.get(i).and_then(fnum)).reduce(f64::max));
                add(&mut s, metric("replication_lag", "Retraso de réplica", "Replicación", MetricUnit::Seconds, lag.or(get(&asy, "ReplicasMaxAbsoluteDelay"))));
            }
        }
        add(&mut s, metric("uptime", "Tiempo activo", "Servidor", MetricUnit::Seconds, uptime));

        // Info.
        if let Some(c) = get(&asy, "NumberOfLogicalCPUCores").or(get(&asy, "NumberOfPhysicalCPUCores")) {
            s.info.push(("Núcleos de CPU".into(), format!("{c:.0}")));
        }
        if let Some(t) = total {
            s.info.push(("Memoria total".into(), bytes_text(t)));
        }
        if let Some(m) = get(&settings, "max_server_memory_usage").filter(|m| *m > 0.0) {
            s.info.push(("Límite de memoria".into(), bytes_text(m)));
        }
        if let Some(m) = get(&settings, "max_connections") {
            s.info.push(("Máx. conexiones".into(), format!("{m:.0}")));
        }
        if let Some(m) = get(&settings, "max_concurrent_queries").filter(|m| *m > 0.0) {
            s.info.push(("Máx. consultas simultáneas".into(), format!("{m:.0}")));
        }

        // Tables.
        match self
            .grid(
                "SELECT query_id AS id, user AS usuario, address AS cliente, round(elapsed, 1) AS `duración (s)`,
                        read_rows AS `filas leídas`, read_bytes AS `bytes leídos`, written_rows AS `filas escritas`,
                        memory_usage AS memoria, query AS consulta
                 FROM system.processes
                 WHERE query NOT LIKE '%FROM system.processes%'
                 ORDER BY elapsed DESC LIMIT 200",
            )
            .await
        {
            Ok(g) => s.tables.push(g.table("queries", "Consultas en curso")),
            Err(e) => s.notes.push(format!("No se pudo leer system.processes ({}).", why(&e))),
        }
        match dbs {
            Ok(g) => s.tables.push(g.table("databases", "Bases y tamaños (bytes)")),
            Err(e) => s.notes.push(format!("No se pudieron leer los tamaños de system.tables ({}).", why(&e))),
        }
        if let Ok(g) = self
            .grid(
                "SELECT concat(database, '.', name) AS objeto, engine AS motor, total_rows AS filas, total_bytes AS bytes
                 FROM system.tables WHERE total_bytes > 0 ORDER BY total_bytes DESC LIMIT 20",
            )
            .await
        {
            s.tables.push(g.table("top_objects", "Tablas más grandes"));
        }
        match self
            .grid(
                "SELECT database AS base, table AS tabla, round(elapsed, 1) AS `duración (s)`, round(progress * 100, 1) AS `progreso (%)`,
                        num_parts AS partes, result_part_name AS `parte resultante`, memory_usage AS memoria,
                        is_mutation AS `mutación`
                 FROM system.merges LIMIT 200",
            )
            .await
        {
            Ok(g) => s.tables.push(g.table("merges", "Merges en curso")),
            Err(e) => s.notes.push(format!("No se pudo leer system.merges ({}).", why(&e))),
        }
        if let Ok(g) = self
            .grid(
                "SELECT database AS base, table AS tabla, mutation_id AS id, command AS comando, create_time AS creada,
                        parts_to_do AS `partes pendientes`, latest_fail_reason AS `último error`
                 FROM system.mutations WHERE NOT is_done LIMIT 200",
            )
            .await
        {
            s.tables.push(g.table("mutations", "Mutaciones pendientes"));
        }
        if let Ok(g) = replicas {
            if !g.rows.is_empty() {
                s.tables.push(g.table("replication", "Tablas replicadas"));
            }
        }
        match disks {
            Ok(g) => s.tables.push(g.project(
                "disks",
                "Discos",
                &[
                    ("name", "disco"),
                    ("path", "ruta"),
                    ("type", "tipo"),
                    ("free_space", "libre"),
                    ("total_space", "total"),
                    ("unreserved_space", "sin reservar"),
                    ("keep_free_space", "reserva"),
                    ("is_broken", "dañado"),
                ],
            )),
            Err(e) => s.notes.push(format!("No se pudo leer system.disks ({}).", why(&e))),
        }
        if let Ok(g) = self
            .grid(
                "SELECT cluster, shard_num AS shard, replica_num AS `réplica`, host_name AS servidor, port AS puerto,
                        is_local AS `local`, errors_count AS errores
                 FROM system.clusters LIMIT 200",
            )
            .await
        {
            if !g.rows.is_empty() {
                s.tables.push(g.table("nodes", "Nodos de los clusters"));
            }
        }
        s.notes.push(format!(
            "{product} no tiene sesiones persistentes (HTTP): la tabla de consultas muestra lo que corre ahora; no hay transacciones ni bloqueos de filas."
        ));
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_from_cells() {
        assert_eq!(fnum(&json!("18446744073709551615")), Some(18446744073709551615.0));
        assert_eq!(fnum(&json!(2.5)), Some(2.5));
        assert_eq!(fnum(&json!(true)), Some(1.0));
        assert_eq!(fnum(&Value::Null), None);
    }

    #[test]
    fn messages_are_short() {
        let e = dbine_driver::Error::Query(
            "Code: 497. DB::Exception: dbine: Not enough privileges. (ACCESS_DENIED) (version 24.1.1.1)".into(),
        );
        assert_eq!(why(&e), "dbine: Not enough privileges. (ACCESS_DENIED)");
        assert_eq!(bytes_text(1536.0), "1.5 KB");
        let kv: Kv = [("a".to_string(), 1.0), ("b".to_string(), 2.0)].into();
        assert_eq!(sum(&kv, &["a", "b", "c"]), Some(3.0));
        assert_eq!(sum(&kv, &["c"]), None);
    }
}
