//! `Session::monitor` for the MySQL family. Every engine gets what its
//! catalog offers:
//!
//! - MySQL, MariaDB, Aurora, Cloud SQL: `SHOW GLOBAL STATUS` / `VARIABLES`,
//!   InnoDB, `information_schema.PROCESSLIST`, lock waits (`data_lock_waits`
//!   on MySQL 8, `INNODB_LOCK_WAITS` on MariaDB), Performance Schema waits
//!   and statement digests, replica status; Aurora adds
//!   `REPLICA_HOST_STATUS` (CPU and lag per instance).
//! - TiDB: `CLUSTER_INFO`, `CLUSTER_LOAD`, `MEMORY_USAGE`,
//!   `CLUSTER_PROCESSLIST`, `DATA_LOCK_WAITS`, `STATEMENTS_SUMMARY`.
//! - OceanBase: `GV$SYSSTAT`, `GV$OB_SERVERS`; SingleStore: `MV_NODES`.
//! - StarRocks, Doris, VeloDB: `SHOW FRONTENDS` / `BACKENDS`, `SHOW PROC`.
//! - Databend: `system.processes` / `clusters` / `tables`.
//! - Manticore: `SHOW STATUS`, `SHOW THREADS`, `SHOW TABLE … STATUS`.
//! - GreptimeDB: `cluster_info`, `process_list`, `region_statistics`.
//!
//! A part that fails (missing privilege, older version) is left out with a
//! note; only a dead connection fails the whole snapshot. Sizes come from
//! the catalog and are refreshed once a minute.

use crate::cells::cell;
use crate::session::{at, lit, named, MySqlSession};
use crate::{err, Variant};
use dbine_driver::monitor::num;
use dbine_driver::{Metric, MetricUnit, MonitorSnapshot, MonitorTable, Result};
use mysql_async::prelude::Queryable;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const MAX_ROWS: usize = 200;
const MAX_TEXT: usize = 2000;
const SIZES_TTL: Duration = Duration::from_secs(60);

/// Database and table sizes, cached between snapshots.
pub(crate) struct Sizes {
    at: Instant,
    total: Option<f64>,
    tables: Vec<MonitorTable>,
    notes: Vec<String>,
}

/// `name → value` with lower-case names (SHOW STATUS, SHOW VARIABLES…).
type Kv = HashMap<String, String>;

fn get(kv: &Kv, k: &str) -> Option<f64> {
    kv.get(&k.to_ascii_lowercase()).and_then(|v| num(v))
}

/// The sum of the keys present; `None` if none is.
fn sum(kv: &Kv, keys: &[&str]) -> Option<f64> {
    keys.iter().filter_map(|k| get(kv, k)).fold(None, |acc, v| Some(acc.unwrap_or(0.0) + v))
}

fn on(kv: &Kv, k: &str) -> bool {
    kv.get(k).is_some_and(|v| v.eq_ignore_ascii_case("ON") || v == "1")
}

/// A statement's text cut to what the dashboard shows.
fn clip(s: &str) -> String {
    match s.char_indices().nth(MAX_TEXT) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn clip_json(v: Value) -> Value {
    match v {
        Value::String(s) if s.chars().count() > MAX_TEXT => Value::String(clip(&s)),
        v => v,
    }
}

fn opt(v: Option<f64>) -> Value {
    v.map_or(Value::Null, |v| json!(v))
}

fn opt_text(v: Option<String>) -> Value {
    v.map_or(Value::Null, |s| Value::String(clip(&s)))
}

/// `1.234 GB`, `12 KB`, `0.000 `, `3.5GiB` (StarRocks, Doris, TiKV) in bytes.
pub(crate) fn parse_size(s: &str) -> Option<f64> {
    let s = s.trim();
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-')).unwrap_or(s.len());
    let n: f64 = s[..split].parse().ok()?;
    let unit = s[split..].trim().to_ascii_uppercase();
    let unit = unit.trim_end_matches("BYTES").trim_end_matches("IB").trim_end_matches('B');
    let pow = match unit {
        "" => 0,
        "K" => 1,
        "M" => 2,
        "G" => 3,
        "T" => 4,
        "P" => 5,
        _ => return None,
    };
    Some(n * 1024f64.powi(pow))
}

/// Bytes as a short text for `info`.
fn human(b: f64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = b;
    let mut i = 0;
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

fn metric(key: &str, label: &str, group: &str, unit: MetricUnit, value: Option<f64>) -> Metric {
    Metric::new(key, label, group, unit, value)
}

/// Adds a metric only when the server reported it.
fn add(s: &mut MonitorSnapshot, mut m: Metric) {
    if m.value.is_some() {
        // An empty f64 sum is -0.0.
        m.value = m.value.map(|v| v + 0.0);
        s.metrics.push(m);
    }
}

fn info(s: &mut MonitorSnapshot, label: &str, value: Option<String>) {
    if let Some(v) = value.filter(|v| !v.trim().is_empty()) {
        s.info.push((label.into(), v));
    }
}

/// A result set: column names and cells, at most `MAX_ROWS` rows.
struct Grid {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
}

impl Grid {
    fn col(&self, names: &[&str]) -> Option<usize> {
        self.columns.iter().position(|c| names.iter().any(|n| c.eq_ignore_ascii_case(n)))
    }

    fn text(&self, row: &[Value], names: &[&str]) -> Option<String> {
        let v = row.get(self.col(names)?)?;
        match v {
            Value::Null => None,
            Value::String(s) => Some(s.clone()),
            v => Some(v.to_string()),
        }
    }

    fn num(&self, row: &[Value], names: &[&str]) -> Option<f64> {
        let v = row.get(self.col(names)?)?;
        v.as_f64().or_else(|| v.as_str().and_then(num))
    }

    /// Only these columns, `(source names, label)`, in this order; the
    /// ones the engine doesn't have are dropped.
    fn project(&self, key: &str, title: &str, pick: &[(&[&str], &str)]) -> MonitorTable {
        let cols: Vec<(usize, &str)> = pick.iter().filter_map(|(names, label)| Some((self.col(names)?, *label))).collect();
        let labels: Vec<&str> = cols.iter().map(|(_, l)| *l).collect();
        let mut t = MonitorTable::new(key, title, &labels);
        t.rows = self.rows.iter().map(|r| cols.iter().map(|(i, _)| r.get(*i).cloned().unwrap_or(Value::Null)).collect()).collect();
        t
    }

    fn table(self, key: &str, title: &str) -> MonitorTable {
        let cols: Vec<&str> = self.columns.iter().map(String::as_str).collect();
        let mut t = MonitorTable::new(key, title, &cols);
        t.rows = self.rows;
        t
    }
}

/// The permission message for a note, or a generic one.
fn why(e: &dbine_driver::Error) -> String {
    let s = e.to_string();
    let s = s.lines().next().unwrap_or_default().trim();
    if s.len() > 200 {
        format!("{}…", &s[..s.char_indices().nth(200).map_or(s.len(), |(i, _)| i)])
    } else {
        s.to_string()
    }
}

impl MySqlSession {
    async fn grid(&mut self, sql: &str) -> Result<Grid> {
        let mut res = self.conn.query_iter(sql).await.map_err(err)?;
        let cols = res.columns().map(|c| c.to_vec()).unwrap_or_default();
        let mut rows = Vec::new();
        while let Some(row) = res.next().await.map_err(err)? {
            if rows.len() < MAX_ROWS {
                rows.push(row.unwrap().into_iter().zip(&cols).map(|(v, c)| clip_json(cell(c, v))).collect());
            }
        }
        res.drop_result().await.map_err(err)?;
        Ok(Grid { columns: cols.iter().map(|c| c.name_str().into_owned()).collect(), rows })
    }

    /// Two-column `name, value` rows as a map; `None` if the query failed.
    async fn kv(&mut self, sql: &str) -> Option<Kv> {
        let rows = self.rows(sql).await.ok()?;
        Some(rows.iter().filter_map(|r| Some((at(r, 0)?.to_ascii_lowercase(), at(r, 1).unwrap_or_default()))).collect())
    }

    async fn scalar(&mut self, sql: &str) -> Option<f64> {
        let rows = self.rows(sql).await.ok()?;
        rows.first().and_then(|r| at(r, 0)).and_then(|v| num(&v))
    }

    pub(crate) async fn snapshot(&mut self) -> Result<MonitorSnapshot> {
        let mut s = MonitorSnapshot::default();
        // The one query that must work: it proves the connection is alive.
        let version = self.rows("SELECT VERSION()").await?.first().and_then(|r| at(r, 0)).unwrap_or_default();
        s.info.push(("Versión".into(), version.clone()));
        match self.variant {
            Variant::MySql | Variant::MariaDb | Variant::SingleStore | Variant::OceanBase => {
                self.mysql_like(&mut s, &version).await
            }
            Variant::TiDb => self.tidb(&mut s).await,
            Variant::StarRocks | Variant::Doris => self.olap(&mut s).await,
            Variant::Databend => self.databend(&mut s).await,
            Variant::Manticore => self.manticore(&mut s).await,
            Variant::GreptimeDb => self.greptime(&mut s).await,
            _ => {}
        }
        Ok(s)
    }

    // ---------------------------------------------------------------- MySQL

    async fn mysql_like(&mut self, s: &mut MonitorSnapshot, version: &str) {
        let v = self.variant;
        let maria = v == Variant::MariaDb || version.contains("MariaDB");
        let full = matches!(v, Variant::MySql | Variant::MariaDb);
        let status = self.kv("SHOW GLOBAL STATUS").await.unwrap_or_default();
        if status.is_empty() {
            s.notes.push("El servidor no respondió SHOW GLOBAL STATUS: no hay contadores de actividad.".into());
        }
        let wanted = [
            "max_connections", "innodb_buffer_pool_size", "version_comment", "hostname", "time_zone",
            "system_time_zone", "read_only", "super_read_only", "server_id", "performance_schema",
            "default_storage_engine", "character_set_server", "aurora_version", "aurora_server_id", "log_bin",
            "innodb_read_only",
        ];
        let list = wanted.iter().map(|w| lit(w)).collect::<Vec<_>>().join(", ");
        let vars = match self.kv(&format!("SHOW GLOBAL VARIABLES WHERE Variable_name IN ({list})")).await {
            Some(k) if !k.is_empty() => k,
            _ => self.kv("SHOW GLOBAL VARIABLES").await.unwrap_or_default(),
        };
        let ps = full && on(&vars, "performance_schema");

        // Info.
        info(s, "Edición", vars.get("version_comment").cloned());
        if self.product == Variant::AuroraMySql {
            info(s, "Versión de Aurora", vars.get("aurora_version").cloned());
        }
        info(s, "Servidor", vars.get("hostname").cloned());
        let tz = vars.get("time_zone").filter(|t| !t.eq_ignore_ascii_case("SYSTEM")).or(vars.get("system_time_zone"));
        info(s, "Zona horaria", tz.cloned());
        info(s, "Máx. conexiones", vars.get("max_connections").cloned());
        info(s, "Buffer pool InnoDB", get(&vars, "innodb_buffer_pool_size").map(human));
        info(s, "Motor predeterminado", vars.get("default_storage_engine").cloned());
        info(s, "Juego de caracteres", vars.get("character_set_server").cloned());
        info(s, "ID de servidor", vars.get("server_id").cloned());
        if full {
            info(s, "Log binario", vars.get("log_bin").cloned());
            info(s, "Performance Schema", vars.get("performance_schema").cloned());
        }

        // CPU.
        let mut cpu = None;
        let mut aurora_role = None;
        if self.product == Variant::AuroraMySql {
            match self
                .grid(
                    "SELECT SERVER_ID, SESSION_ID, CPU, REPLICA_LAG_IN_MILLISECONDS, LAST_UPDATE_TIMESTAMP
                     FROM information_schema.REPLICA_HOST_STATUS",
                )
                .await
            {
                Ok(g) => {
                    let me = vars.get("aurora_server_id").cloned();
                    let mut t = MonitorTable::new(
                        "nodes",
                        "Instancias del cluster Aurora",
                        &["instancia", "rol", "CPU (%)", "retraso (ms)", "actualizado"],
                    );
                    for r in &g.rows {
                        let id = g.text(r, &["SERVER_ID"]);
                        let writer = g.text(r, &["SESSION_ID"]).as_deref() == Some("MASTER_SESSION_ID");
                        let role = if writer { "Escritor" } else { "Lector" };
                        if id.is_some() && id == me {
                            cpu = g.num(r, &["CPU"]);
                            aurora_role = Some(role);
                        }
                        t.rows.push(vec![
                            opt_text(id),
                            json!(role),
                            opt(g.num(r, &["CPU"])),
                            opt(g.num(r, &["REPLICA_LAG_IN_MILLISECONDS"])),
                            opt_text(g.text(r, &["LAST_UPDATE_TIMESTAMP"])),
                        ]);
                    }
                    s.tables.push(t);
                }
                Err(e) => s.notes.push(format!("No se pudo leer REPLICA_HOST_STATUS de Aurora ({}).", why(&e))),
            }
        }
        add(s, metric("cpu", "CPU del servidor", "CPU", MetricUnit::Percent, cpu).max(Some(100.0)));
        if cpu.is_none() {
            s.notes.push(match self.product {
                Variant::CloudSqlMySql => {
                    "Cloud SQL no expone el uso de CPU por SQL: está en Cloud Monitoring (métrica cloudsql.googleapis.com/database/cpu/utilization).".into()
                }
                Variant::AuroraMySql => "Aurora no informó el uso de CPU de esta instancia.".into(),
                _ if maria => "MariaDB no expone el uso de CPU del servidor por SQL.".into(),
                _ => "MySQL no expone el uso de CPU del servidor por SQL; miralo en el sistema operativo o en la consola del proveedor.".into(),
            });
        }
        if ps && !maria {
            let cpu_ps = self
                .scalar("SELECT SUM(SUM_CPU_TIME) FROM performance_schema.events_statements_summary_global_by_event_name")
                .await
                .filter(|v| *v > 0.0);
            match cpu_ps {
                // Picoseconds → seconds × 100.
                Some(ps) => add(
                    s,
                    metric("cpu_time", "CPU de las sentencias", "CPU", MetricUnit::Percent, Some(ps / 1e10)).counter(),
                ),
                None => s.notes.push(
                    "El tiempo de CPU de las sentencias requiere activar el consumidor events_statements_cpu de Performance Schema (MySQL 8.0.28+).".into(),
                ),
            }
        }

        // Memory.
        let mem = if maria {
            get(&status, "Memory_used")
        } else if ps {
            self.scalar("SELECT SUM(CURRENT_NUMBER_OF_BYTES_USED) FROM performance_schema.memory_summary_global_by_event_name")
                .await
        } else {
            None
        };
        add(s, metric("mem_used", "Memoria usada", "Memoria", MetricUnit::Bytes, mem));
        if mem.is_none() && full {
            s.notes.push("La memoria usada solo se ve con Performance Schema activado (performance_schema = ON).".into());
        }
        let pool = get(&status, "Innodb_buffer_pool_bytes_data").or_else(|| {
            Some(get(&status, "Innodb_buffer_pool_pages_data")? * get(&status, "Innodb_page_size").unwrap_or(16384.0))
        });
        add(
            s,
            metric("mem_cache", "Buffer pool (datos)", "Memoria", MetricUnit::Bytes, pool)
                .max(get(&vars, "innodb_buffer_pool_size")),
        );

        // Connections and activity.
        let max_conn = get(&vars, "max_connections").filter(|m| *m > 0.0);
        add(s, metric("connections", "Conexiones", "Conexiones", MetricUnit::Count, get(&status, "Threads_connected")).max(max_conn));
        add(s, metric("active_sessions", "Sesiones activas", "Conexiones", MetricUnit::Count, get(&status, "Threads_running")));
        add(
            s,
            metric("aborted_connects", "Conexiones rechazadas", "Conexiones", MetricUnit::Count, get(&status, "Aborted_connects"))
                .counter(),
        );
        let queries = get(&status, "Questions").or_else(|| get(&status, "Queries"));
        add(s, metric("queries", "Consultas", "Actividad", MetricUnit::Count, queries).counter());
        add(
            s,
            metric("transactions", "Transacciones (commits)", "Actividad", MetricUnit::Count, get(&status, "Handler_commit").or_else(|| sum(&status, &["Com_commit", "Com_rollback"])))
                .counter(),
        );
        let read = get(&status, "Innodb_rows_read").or_else(|| {
            sum(&status, &["Handler_read_first", "Handler_read_key", "Handler_read_next", "Handler_read_rnd", "Handler_read_rnd_next"])
        });
        let written = sum(&status, &["Innodb_rows_inserted", "Innodb_rows_updated", "Innodb_rows_deleted"])
            .or_else(|| sum(&status, &["Handler_write", "Handler_update", "Handler_delete"]));
        add(s, metric("rows_read", "Filas leídas", "Actividad", MetricUnit::Count, read).counter());
        add(s, metric("rows_written", "Filas escritas", "Actividad", MetricUnit::Count, written).counter());
        add(s, metric("slow_queries", "Consultas lentas", "Actividad", MetricUnit::Count, get(&status, "Slow_queries")).counter());
        add(
            s,
            metric("tmp_disk_tables", "Tablas temporales en disco", "Actividad", MetricUnit::Count, get(&status, "Created_tmp_disk_tables"))
                .counter(),
        );
        add(s, metric("net_in", "Red entrante", "Red", MetricUnit::Bytes, get(&status, "Bytes_received")).counter());
        add(s, metric("net_out", "Red saliente", "Red", MetricUnit::Bytes, get(&status, "Bytes_sent")).counter());
        add(s, metric("disk_read", "Lectura en disco", "Disco", MetricUnit::Bytes, get(&status, "Innodb_data_read")).counter());
        add(s, metric("disk_write", "Escritura en disco", "Disco", MetricUnit::Bytes, get(&status, "Innodb_data_written")).counter());
        let hit = match (get(&status, "Innodb_buffer_pool_read_requests"), get(&status, "Innodb_buffer_pool_reads")) {
            (Some(req), Some(miss)) if req > 0.0 => Some((100.0 * (1.0 - miss / req)).clamp(0.0, 100.0)),
            _ => None,
        };
        add(s, metric("cache_hit", "Aciertos del buffer pool", "Caché", MetricUnit::Percent, hit).max(Some(100.0)));

        // Locks.
        add(s, metric("locks_waiting", "Bloqueos en espera", "Bloqueos", MetricUnit::Count, get(&status, "Innodb_row_lock_current_waits")));
        let deadlocks = match get(&status, "Innodb_deadlocks") {
            Some(d) => Some(d),
            None if full => self.scalar("SELECT `COUNT` FROM information_schema.INNODB_METRICS WHERE NAME = 'lock_deadlocks'").await,
            None => None,
        };
        add(s, metric("deadlocks", "Deadlocks", "Bloqueos", MetricUnit::Count, deadlocks).counter());
        add(
            s,
            metric("row_lock_waits", "Esperas de bloqueo de fila", "Bloqueos", MetricUnit::Count, get(&status, "Innodb_row_lock_waits"))
                .counter(),
        );

        // Replication.
        let mut role = aurora_role.map(str::to_string);
        if full {
            let lag = self.replication(s, maria).await;
            if self.product == Variant::AuroraMySql {
                // Aurora replicas share storage; their lag is in the nodes table.
                if role.is_none() {
                    let reader = on(&vars, "innodb_read_only") || on(&vars, "read_only");
                    role = Some(if reader { "Lector" } else { "Escritor" }.into());
                }
            } else {
                add(s, metric("replication_lag", "Retraso de réplica", "Replicación", MetricUnit::Seconds, lag.0));
                if role.is_none() {
                    role = Some(if lag.1 {
                        "Réplica".into()
                    } else if on(&vars, "read_only") || on(&vars, "super_read_only") {
                        "Solo lectura".into()
                    } else {
                        "Primario".into()
                    });
                }
            }
        }
        info(s, "Rol", role);
        add(s, metric("uptime", "Tiempo activo", "Servidor", MetricUnit::Seconds, get(&status, "Uptime")));

        // Engine extras.
        match v {
            Variant::SingleStore => self.singlestore(s).await,
            Variant::OceanBase => self.oceanbase(s).await,
            _ => {}
        }

        self.sessions(s, Some("information_schema.PROCESSLIST")).await;
        if full {
            self.innodb_locks(s).await;
        } else {
            s.notes.push(format!("{} no expone las esperas de bloqueo por SQL.", self.engine_name()));
        }
        if ps {
            self.performance_schema(s).await;
        } else if full {
            s.notes.push(
                "Performance Schema está desactivado: no hay esperas ni estadísticas por sentencia (activalo con performance_schema = ON).".into(),
            );
        }
        self.sizes_from_information_schema(s).await;
        if full && !self.has_process_privilege().await {
            s.notes.push("Sin el privilegio PROCESS solo se ven tus propias sesiones.".into());
        }
    }

    fn engine_name(&self) -> &'static str {
        match self.variant {
            Variant::SingleStore => "SingleStore",
            Variant::OceanBase => "OceanBase",
            Variant::MariaDb => "MariaDB",
            _ => "Este motor",
        }
    }

    async fn has_process_privilege(&mut self) -> bool {
        let sql = "SELECT 1 FROM information_schema.USER_PRIVILEGES
                   WHERE PRIVILEGE_TYPE IN ('PROCESS', 'SUPER')
                   AND GRANTEE = CONCAT('''', SUBSTRING_INDEX(CURRENT_USER(), '@', 1), '''@''', SUBSTRING_INDEX(CURRENT_USER(), '@', -1), '''')";
        // Unknown counts as granted: no note rather than a wrong one.
        self.rows(sql).await.map_or(true, |r| !r.is_empty())
    }

    /// Replica status of this server plus the replicas attached to it.
    /// Returns (worst lag, is a replica).
    async fn replication(&mut self, s: &mut MonitorSnapshot, maria: bool) -> (Option<f64>, bool) {
        let mut t = MonitorTable::new(
            "replication",
            "Replicación",
            &["rol", "servidor", "canal", "E/S", "SQL", "retraso (s)", "último error"],
        );
        let status = match self.rows("SHOW REPLICA STATUS").await {
            Ok(r) => Ok(r),
            Err(_) => self.rows("SHOW SLAVE STATUS").await,
        };
        let mut lag: Option<f64> = None;
        let mut replica = false;
        match status {
            Ok(rows) => {
                for r in &rows {
                    replica = true;
                    let l = named(r, &["Seconds_Behind_Source", "Seconds_Behind_Master"]).and_then(|v| num(&v));
                    if let Some(l) = l {
                        lag = Some(lag.map_or(l, |w| w.max(l)));
                    }
                    let host = named(r, &["Source_Host", "Master_Host"]).unwrap_or_default();
                    let port = named(r, &["Source_Port", "Master_Port"]).unwrap_or_default();
                    let error = [&["Last_IO_Error", "Last_Error"][..], &["Last_SQL_Error"][..]]
                        .iter()
                        .filter_map(|n| named(r, n))
                        .find(|e| !e.is_empty());
                    t.rows.push(vec![
                        json!("Origen de esta réplica"),
                        json!(format!("{host}:{port}")),
                        opt_text(named(r, &["Channel_Name", "Connection_name"])),
                        opt_text(named(r, &["Replica_IO_Running", "Slave_IO_Running"])),
                        opt_text(named(r, &["Replica_SQL_Running", "Slave_SQL_Running"])),
                        opt(l),
                        opt_text(error),
                    ]);
                }
            }
            Err(e) => s.notes.push(format!(
                "No se pudo leer el estado de réplica: hace falta el privilegio {} ({}).",
                if maria { "REPLICA MONITOR o REPLICATION CLIENT" } else { "REPLICATION CLIENT" },
                why(&e)
            )),
        }
        let hosts = match self.rows("SHOW REPLICAS").await {
            Ok(r) => r,
            Err(_) => self.optional_rows("SHOW SLAVE HOSTS").await,
        };
        for r in &hosts {
            let host = named(r, &["Host"]).unwrap_or_default();
            let port = named(r, &["Port"]).unwrap_or_default();
            t.rows.push(vec![
                json!("Réplica conectada"),
                json!(format!("{host}:{port}")),
                opt_text(named(r, &["Server_Id", "Server_id"])),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]);
        }
        if !t.rows.is_empty() {
            s.tables.push(t);
        }
        (lag, replica)
    }

    /// `sessions` and `queries` (the non-idle ones) from a processlist view,
    /// else (or with `None`) `SHOW FULL PROCESSLIST`.
    async fn sessions(&mut self, s: &mut MonitorSnapshot, view: Option<&str>) {
        let me = self.conn.id();
        let sql = match view {
            Some(view) => format!("SELECT * FROM {view} WHERE ID <> {me} ORDER BY (COMMAND = 'Sleep'), TIME DESC LIMIT {MAX_ROWS}"),
            None => "SHOW FULL PROCESSLIST".into(),
        };
        let g = match self.grid(&sql).await {
            Ok(g) => g,
            Err(first) => match self.grid("SHOW FULL PROCESSLIST").await {
                Ok(g) => g,
                Err(_) => {
                    s.notes.push(format!("No se pudo leer la lista de sesiones ({}).", why(&first)));
                    return;
                }
            },
        };
        let pick: &[(&[&str], &str)] = &[
            (&["Id", "ID", "ConnectionId"], "id"),
            (&["INSTANCE", "FE", "ServerName"], "nodo"),
            (&["User", "USER"], "usuario"),
            (&["db", "DB"], "base"),
            (&["Host", "HOST"], "cliente"),
            (&["Command", "COMMAND"], "comando"),
            (&["State", "STATE"], "estado"),
            (&["Time", "TIME"], "duración (s)"),
            (&["MEM"], "memoria"),
            (&["Info", "INFO"], "consulta actual"),
        ];
        let mut sessions = g.project("sessions", "Sesiones", pick);
        // Our own monitoring query isn't interesting.
        let id_col = g.col(&["Id", "ID", "ConnectionId"]);
        let keep: Vec<bool> = g
            .rows
            .iter()
            .map(|r| id_col.and_then(|i| r.get(i)).and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(num))) != Some(me as f64))
            .collect();
        let mut i = 0;
        sessions.rows.retain(|_| {
            i += 1;
            keep[i - 1]
        });
        let cmd = sessions.columns.iter().position(|c| c == "comando");
        let query = sessions.columns.iter().position(|c| c == "consulta actual");
        let mut queries = MonitorTable::new("queries", "Consultas en curso", &[]);
        queries.columns = sessions.columns.clone();
        queries.rows = sessions
            .rows
            .iter()
            .filter(|r| {
                let busy = cmd.and_then(|c| r[c].as_str()).is_none_or(|c| !matches!(c, "Sleep" | "Daemon" | "Binlog Dump" | "Binlog Dump GTID"));
                busy && query.is_some_and(|q| !r[q].is_null())
            })
            .cloned()
            .collect();
        if !s.metrics.iter().any(|m| m.key == "connections") {
            // Plus this one, left out of the table.
            add(s, metric("connections", "Conexiones", "Conexiones", MetricUnit::Count, Some(sessions.rows.len() as f64 + 1.0)));
        }
        if !s.metrics.iter().any(|m| m.key == "active_sessions") {
            add(s, metric("active_sessions", "Sesiones activas", "Conexiones", MetricUnit::Count, Some(queries.rows.len() as f64)));
        }
        s.tables.push(sessions);
        s.tables.push(queries);
    }

    /// Row-lock waits: MySQL 8 `data_lock_waits`, else InnoDB's
    /// `INNODB_LOCK_WAITS` (MariaDB, MySQL 5.7).
    async fn innodb_locks(&mut self, s: &mut MonitorSnapshot) {
        let mysql8 = "SELECT r.trx_mysql_thread_id AS `sesión en espera`, b.trx_mysql_thread_id AS `bloqueada por`,
                   CONCAT(l.OBJECT_SCHEMA, '.', l.OBJECT_NAME) AS objeto, l.LOCK_MODE AS modo,
                   TIMESTAMPDIFF(SECOND, r.trx_wait_started, NOW()) AS `espera (s)`,
                   LEFT(r.trx_query, 2000) AS `consulta en espera`, LEFT(b.trx_query, 2000) AS `consulta que bloquea`
            FROM performance_schema.data_lock_waits w
            JOIN performance_schema.data_locks l ON l.ENGINE_LOCK_ID = w.REQUESTING_ENGINE_LOCK_ID
            JOIN information_schema.INNODB_TRX r ON r.trx_id = w.REQUESTING_ENGINE_TRANSACTION_ID
            JOIN information_schema.INNODB_TRX b ON b.trx_id = w.BLOCKING_ENGINE_TRANSACTION_ID
            LIMIT 200";
        let legacy = "SELECT r.trx_mysql_thread_id AS `sesión en espera`, b.trx_mysql_thread_id AS `bloqueada por`,
                   l.lock_table AS objeto, l.lock_mode AS modo,
                   TIMESTAMPDIFF(SECOND, r.trx_wait_started, NOW()) AS `espera (s)`,
                   LEFT(r.trx_query, 2000) AS `consulta en espera`, LEFT(b.trx_query, 2000) AS `consulta que bloquea`
            FROM information_schema.INNODB_LOCK_WAITS w
            JOIN information_schema.INNODB_LOCKS l ON l.lock_id = w.requested_lock_id
            JOIN information_schema.INNODB_TRX r ON r.trx_id = w.requesting_trx_id
            JOIN information_schema.INNODB_TRX b ON b.trx_id = w.blocking_trx_id
            LIMIT 200";
        let g = match self.grid(mysql8).await {
            Ok(g) => Ok(g),
            Err(_) => self.grid(legacy).await,
        };
        match g {
            Ok(g) => s.tables.push(g.table("locks", "Bloqueos en espera")),
            Err(e) => s.notes.push(format!("No se pudieron leer las esperas de bloqueo ({}); hace falta el privilegio PROCESS.", why(&e))),
        }
    }

    /// Top waits and statement digests since start.
    async fn performance_schema(&mut self, s: &mut MonitorSnapshot) {
        let waits = "SELECT EVENT_NAME AS evento, COUNT_STAR AS esperas,
                   ROUND(SUM_TIMER_WAIT / 1e12, 3) AS `tiempo total (s)`, ROUND(AVG_TIMER_WAIT / 1e9, 3) AS `promedio (ms)`
            FROM performance_schema.events_waits_summary_global_by_event_name
            WHERE EVENT_NAME <> 'idle' AND COUNT_STAR > 0
            ORDER BY SUM_TIMER_WAIT DESC LIMIT 20";
        match self.grid(waits).await {
            Ok(g) => s.tables.push(g.table("waits", "Esperas principales (desde el arranque)")),
            Err(e) => s.notes.push(format!("No se pudieron leer las esperas de Performance Schema ({}).", why(&e))),
        }
        let digests = "SELECT SCHEMA_NAME AS base, LEFT(DIGEST_TEXT, 2000) AS consulta, COUNT_STAR AS ejecuciones,
                   ROUND(SUM_TIMER_WAIT / 1e12, 3) AS `tiempo total (s)`, ROUND(AVG_TIMER_WAIT / 1e9, 3) AS `promedio (ms)`,
                   SUM_ROWS_EXAMINED AS `filas examinadas`, SUM_ROWS_SENT AS `filas enviadas`, SUM_ERRORS AS errores
            FROM performance_schema.events_statements_summary_by_digest
            ORDER BY SUM_TIMER_WAIT DESC LIMIT 20";
        match self.grid(digests).await {
            Ok(g) => s.tables.push(g.table("top_queries", "Consultas más costosas (acumulado)")),
            Err(e) => s.notes.push(format!("No se pudo leer el resumen de sentencias ({}).", why(&e))),
        }
    }

    /// `databases` and `top_objects` from information_schema.TABLES, plus
    /// `storage_used`; cached for a minute.
    async fn sizes_from_information_schema(&mut self, s: &mut MonitorSnapshot) {
        let fresh = self.sizes.as_ref().is_some_and(|z| z.at.elapsed() < SIZES_TTL);
        if !fresh {
            let skip = "UPPER(TABLE_SCHEMA) NOT IN ('INFORMATION_SCHEMA', 'PERFORMANCE_SCHEMA', 'METRICS_SCHEMA', '__INTERNAL_SCHEMA')";
            let dbs = format!(
                "SELECT TABLE_SCHEMA AS base, COUNT(*) AS tablas, SUM(TABLE_ROWS) AS filas,
                        SUM(DATA_LENGTH) AS datos, SUM(INDEX_LENGTH) AS `índices`,
                        SUM(COALESCE(DATA_LENGTH, 0) + COALESCE(INDEX_LENGTH, 0)) AS total
                 FROM information_schema.TABLES WHERE {skip}
                 GROUP BY TABLE_SCHEMA ORDER BY total DESC LIMIT {MAX_ROWS}"
            );
            let top = format!(
                "SELECT CONCAT(TABLE_SCHEMA, '.', TABLE_NAME) AS objeto, ENGINE AS motor, TABLE_ROWS AS filas,
                        DATA_LENGTH AS datos, INDEX_LENGTH AS `índices`,
                        COALESCE(DATA_LENGTH, 0) + COALESCE(INDEX_LENGTH, 0) AS total
                 FROM information_schema.TABLES WHERE {skip} AND DATA_LENGTH IS NOT NULL
                 ORDER BY total DESC LIMIT 20"
            );
            let mut z = Sizes { at: Instant::now(), total: None, tables: Vec::new(), notes: Vec::new() };
            match self.grid(&dbs).await {
                Ok(g) => {
                    let total: f64 = g.rows.iter().filter_map(|r| g.num(r, &["total"])).sum();
                    z.total = Some(total);
                    z.tables.push(g.table("databases", "Bases y tamaños (bytes)"));
                }
                Err(e) => z.notes.push(format!("No se pudieron leer los tamaños de las bases ({}).", why(&e))),
            }
            if let Ok(g) = self.grid(&top).await {
                z.tables.push(g.table("top_objects", "Tablas más grandes (bytes)"));
            }
            self.sizes = Some(z);
        }
        self.push_sizes(s, None);
    }

    fn push_sizes(&self, s: &mut MonitorSnapshot, max: Option<f64>) {
        if let Some(z) = &self.sizes {
            add(s, metric("storage_used", "Espacio usado", "Almacenamiento", MetricUnit::Bytes, z.total).max(max));
            s.tables.extend(z.tables.iter().cloned());
            s.notes.extend(z.notes.iter().cloned());
        }
    }

    // --------------------------------------------------- SingleStore, OceanBase

    async fn singlestore(&mut self, s: &mut MonitorSnapshot) {
        match self.grid("SELECT * FROM information_schema.MV_NODES").await {
            Ok(g) => {
                let mb = 1024.0 * 1024.0;
                let total = |names: &[&str]| -> Option<f64> {
                    let v: Vec<f64> = g.rows.iter().filter_map(|r| g.num(r, names)).collect();
                    (!v.is_empty()).then(|| v.iter().sum::<f64>() * mb)
                };
                if !s.metrics.iter().any(|m| m.key == "mem_used") {
                    add(s, metric("mem_used", "Memoria usada", "Memoria", MetricUnit::Bytes, total(&["MEMORY_USED_MB"])).max(total(&["MAX_MEMORY_MB"])));
                }
                add(
                    s,
                    metric("table_memory", "Memoria de tablas", "Memoria", MetricUnit::Bytes, total(&["TABLE_MEMORY_USED_MB"]))
                        .max(total(&["MAX_TABLE_MEMORY_MB"])),
                );
                if let (Some(disk), Some(free)) = (total(&["TOTAL_DATA_DISK_MB"]), total(&["AVAILABLE_DATA_DISK_MB"])) {
                    add(s, metric("disk_used", "Disco de datos usado", "Almacenamiento", MetricUnit::Bytes, Some(disk - free)).max(Some(disk)));
                }
                info(s, "Nodos", Some(g.rows.len().to_string()));
                s.tables.push(g.project(
                    "nodes",
                    "Nodos del cluster",
                    &[
                        (&["ID"], "id"),
                        (&["IP_ADDR"], "servidor"),
                        (&["PORT"], "puerto"),
                        (&["TYPE"], "tipo"),
                        (&["STATE"], "estado"),
                        (&["AVAILABILITY_GROUP"], "grupo"),
                        (&["NUM_CPUS"], "CPUs"),
                        (&["MEMORY_USED_MB"], "memoria (MB)"),
                        (&["MAX_MEMORY_MB"], "memoria máx. (MB)"),
                        (&["TOTAL_DATA_DISK_MB"], "disco (MB)"),
                        (&["AVAILABLE_DATA_DISK_MB"], "disco libre (MB)"),
                        (&["UPTIME"], "tiempo activo (s)"),
                    ],
                ));
            }
            Err(e) => s.notes.push(format!("No se pudo leer MV_NODES ({}).", why(&e))),
        }
        s.notes.push("SingleStore no expone el uso de CPU por SQL; miralo en SingleStore Studio o en el monitoreo del cluster.".into());
    }

    async fn oceanbase(&mut self, s: &mut MonitorSnapshot) {
        let names = [
            "active sessions", "sql select count", "sql insert count", "sql update count", "sql delete count",
            "sql replace count", "trans commit count", "trans rollback count", "io read bytes", "io write bytes",
            "memory usage", "rpc packet in bytes", "rpc packet out bytes",
        ];
        let list = names.iter().map(|n| lit(n)).collect::<Vec<_>>().join(", ");
        let sql = format!("SELECT NAME, SUM(VALUE) FROM oceanbase.GV$SYSSTAT WHERE NAME IN ({list}) GROUP BY NAME");
        match self.kv(&sql).await {
            Some(st) if !st.is_empty() => {
                let replace = |s: &mut MonitorSnapshot, m: Metric| {
                    s.metrics.retain(|x| x.key != m.key);
                    add(s, m);
                };
                replace(s, metric("active_sessions", "Sesiones activas", "Conexiones", MetricUnit::Count, get(&st, "active sessions")));
                replace(
                    s,
                    metric(
                        "queries",
                        "Consultas",
                        "Actividad",
                        MetricUnit::Count,
                        sum(&st, &["sql select count", "sql insert count", "sql update count", "sql delete count", "sql replace count"]),
                    )
                    .counter(),
                );
                replace(
                    s,
                    metric("transactions", "Transacciones", "Actividad", MetricUnit::Count, sum(&st, &["trans commit count", "trans rollback count"]))
                        .counter(),
                );
                replace(s, metric("mem_used", "Memoria usada", "Memoria", MetricUnit::Bytes, get(&st, "memory usage")));
                replace(s, metric("disk_read", "Lectura en disco", "Disco", MetricUnit::Bytes, get(&st, "io read bytes")).counter());
                replace(s, metric("disk_write", "Escritura en disco", "Disco", MetricUnit::Bytes, get(&st, "io write bytes")).counter());
                replace(s, metric("net_in", "Red entrante (RPC)", "Red", MetricUnit::Bytes, get(&st, "rpc packet in bytes")).counter());
                replace(s, metric("net_out", "Red saliente (RPC)", "Red", MetricUnit::Bytes, get(&st, "rpc packet out bytes")).counter());
            }
            _ => s.notes.push("No se pudo leer oceanbase.GV$SYSSTAT: los contadores de actividad requieren OceanBase 4.x.".into()),
        }
        match self.grid("SELECT * FROM oceanbase.GV$OB_SERVERS").await {
            Ok(g) => {
                let total = |names: &[&str]| -> Option<f64> {
                    let v: Vec<f64> = g.rows.iter().filter_map(|r| g.num(r, names)).collect();
                    (!v.is_empty()).then(|| v.iter().sum())
                };
                if let (Some(used), cap) = (total(&["DATA_DISK_IN_USE"]), total(&["DATA_DISK_CAPACITY"])) {
                    add(s, metric("disk_used", "Disco de datos usado", "Almacenamiento", MetricUnit::Bytes, Some(used)).max(cap));
                }
                info(s, "Servidores OBServer", Some(g.rows.len().to_string()));
                s.tables.push(g.project(
                    "nodes",
                    "Servidores (OBServer)",
                    &[
                        (&["SVR_IP"], "servidor"),
                        (&["SVR_PORT"], "puerto"),
                        (&["ZONE"], "zona"),
                        (&["CPU_CAPACITY"], "CPUs"),
                        (&["CPU_ASSIGNED"], "CPUs asignadas"),
                        (&["MEM_CAPACITY"], "memoria"),
                        (&["MEM_ASSIGNED"], "memoria asignada"),
                        (&["DATA_DISK_CAPACITY"], "disco de datos"),
                        (&["DATA_DISK_IN_USE"], "disco en uso"),
                        (&["LOG_DISK_CAPACITY"], "disco de log"),
                        (&["LOG_DISK_IN_USE"], "log en uso"),
                    ],
                ));
            }
            Err(e) => s.notes.push(format!("No se pudo leer GV$OB_SERVERS ({}).", why(&e))),
        }
        s.notes.push("OceanBase no expone el porcentaje de CPU por SQL; miralo en OCP (OceanBase Cloud Platform).".into());
    }

    // ----------------------------------------------------------------- TiDB

    async fn tidb(&mut self, s: &mut MonitorSnapshot) {
        let status = self.kv("SHOW GLOBAL STATUS").await.unwrap_or_default();
        let vars = self
            .kv("SHOW GLOBAL VARIABLES WHERE Variable_name IN ('max_connections', 'tidb_server_memory_limit', 'time_zone', 'system_time_zone', 'version_comment')")
            .await
            .unwrap_or_default();
        info(s, "Edición", vars.get("version_comment").cloned());
        info(s, "Zona horaria", vars.get("time_zone").filter(|t| !t.eq_ignore_ascii_case("SYSTEM")).or(vars.get("system_time_zone")).cloned());
        info(s, "Límite de memoria del servidor", vars.get("tidb_server_memory_limit").cloned());

        // Host load per instance: CPU idle and memory.
        let load = self
            .rows(
                "SELECT TYPE, INSTANCE, DEVICE_TYPE, NAME, VALUE FROM information_schema.CLUSTER_LOAD
                 WHERE (DEVICE_TYPE = 'cpu' AND DEVICE_NAME = 'usage' AND NAME = 'idle')
                    OR (DEVICE_TYPE = 'memory' AND DEVICE_NAME = 'virtual' AND NAME IN ('total', 'used'))",
            )
            .await;
        // instance → (cpu %, mem used, mem total)
        let mut per: HashMap<String, (Option<f64>, Option<f64>, Option<f64>)> = HashMap::new();
        match load {
            Ok(rows) => {
                for r in &rows {
                    let (Some(inst), Some(dev), Some(name), Some(val)) = (at(r, 1), at(r, 2), at(r, 3), at(r, 4).and_then(|v| num(&v)))
                    else {
                        continue;
                    };
                    let e = per.entry(inst).or_default();
                    match (dev.as_str(), name.as_str()) {
                        ("cpu", _) => e.0 = Some(((1.0 - val) * 100.0).clamp(0.0, 100.0)),
                        ("memory", "used") => e.1 = Some(val),
                        ("memory", "total") => e.2 = Some(val),
                        _ => {}
                    }
                }
            }
            Err(e) => s.notes.push(format!("No se pudo leer CLUSTER_LOAD ({}).", why(&e))),
        }
        // Hosts may run several instances: memory counts once per host.
        let mut hosts: HashMap<String, (f64, f64)> = HashMap::new();
        for (inst, (_, used, total)) in &per {
            if let (Some(u), Some(t)) = (used, total) {
                let host = inst.rsplit_once(':').map_or(inst.as_str(), |(h, _)| h).to_string();
                hosts.insert(host, (*u, *t));
            }
        }
        let cpus: Vec<f64> = per.values().filter_map(|p| p.0).collect();
        let cpu = (!cpus.is_empty()).then(|| cpus.iter().sum::<f64>() / cpus.len() as f64);
        add(s, metric("cpu", "CPU del cluster (promedio)", "CPU", MetricUnit::Percent, cpu).max(Some(100.0)));
        if !hosts.is_empty() {
            let used = hosts.values().map(|h| h.0).sum::<f64>();
            let total = hosts.values().map(|h| h.1).sum::<f64>();
            add(s, metric("host_mem", "Memoria de los hosts", "Memoria", MetricUnit::Bytes, Some(used)).max(Some(total)));
        }
        if let Ok(g) = self.grid("SELECT * FROM information_schema.MEMORY_USAGE").await {
            if let Some(r) = g.rows.first() {
                let limit = g.num(r, &["MEMORY_LIMIT"]).filter(|l| *l > 0.0).or_else(|| g.num(r, &["MEMORY_TOTAL"]));
                add(s, metric("mem_used", "Memoria de TiDB", "Memoria", MetricUnit::Bytes, g.num(r, &["MEMORY_CURRENT"])).max(limit));
            }
        }

        // Nodes.
        match self.grid("SELECT * FROM information_schema.CLUSTER_INFO").await {
            Ok(g) => {
                let mut t = MonitorTable::new(
                    "nodes",
                    "Nodos del cluster",
                    &["tipo", "instancia", "versión", "inicio", "tiempo activo", "CPU (%)", "memoria usada", "memoria total"],
                );
                for r in &g.rows {
                    let inst = g.text(r, &["INSTANCE"]).unwrap_or_default();
                    let p = per.get(&inst).copied().unwrap_or_default();
                    t.rows.push(vec![
                        opt_text(g.text(r, &["TYPE"])),
                        json!(inst),
                        opt_text(g.text(r, &["VERSION"])),
                        opt_text(g.text(r, &["START_TIME"])),
                        opt_text(g.text(r, &["UPTIME"])),
                        opt(p.0),
                        opt(p.1),
                        opt(p.2),
                    ]);
                }
                info(s, "Nodos", Some(t.rows.len().to_string()));
                s.tables.push(t);
            }
            Err(e) => s.notes.push(format!("No se pudo leer CLUSTER_INFO ({}).", why(&e))),
        }

        let max_conn = get(&vars, "max_connections").filter(|m| *m > 0.0);
        self.sessions(s, Some("information_schema.CLUSTER_PROCESSLIST")).await;
        if let Some(c) = s.metrics.iter_mut().find(|m| m.key == "connections") {
            c.max = max_conn;
        }

        // Locks: pessimistic lock waits and the recent deadlock history.
        match self
            .grid(
                "SELECT w.TRX_ID AS `transacción en espera`, w.CURRENT_HOLDING_TRX_ID AS `bloqueada por`,
                        w.KEY AS clave, LEFT(w.SQL_DIGEST_TEXT, 2000) AS `consulta en espera`
                 FROM information_schema.DATA_LOCK_WAITS w LIMIT 200",
            )
            .await
        {
            Ok(g) => {
                add(s, metric("locks_waiting", "Bloqueos en espera", "Bloqueos", MetricUnit::Count, Some(g.rows.len() as f64)));
                s.tables.push(g.table("locks", "Bloqueos en espera"));
            }
            Err(e) => s.notes.push(format!("No se pudieron leer las esperas de bloqueo ({}).", why(&e))),
        }
        let deadlocks = self.scalar("SELECT COUNT(DISTINCT DEADLOCK_ID) FROM information_schema.CLUSTER_DEADLOCKS").await;
        add(s, metric("recent_deadlocks", "Deadlocks recientes", "Bloqueos", MetricUnit::Count, deadlocks));

        // Statement summary of the current window.
        match self
            .grid(
                "SELECT SCHEMA_NAME AS base, LEFT(DIGEST_TEXT, 2000) AS consulta, EXEC_COUNT AS ejecuciones,
                        ROUND(SUM_LATENCY / 1e9, 3) AS `tiempo total (s)`, ROUND(AVG_LATENCY / 1e6, 3) AS `promedio (ms)`,
                        AVG_PROCESSED_KEYS AS `claves procesadas (prom.)`, AVG_MEM AS `memoria (prom.)`, SUM_ERRORS AS errores
                 FROM information_schema.CLUSTER_STATEMENTS_SUMMARY
                 ORDER BY SUM_LATENCY DESC LIMIT 20",
            )
            .await
        {
            Ok(g) => s.tables.push(g.table("top_queries", "Consultas más costosas (ventana actual)")),
            Err(e) => s.notes.push(format!("No se pudo leer STATEMENTS_SUMMARY ({}).", why(&e))),
        }

        // Storage: TiKV stores' capacity as the ceiling.
        let stores = self.grid("SELECT CAPACITY, AVAILABLE FROM information_schema.TIKV_STORE_STATUS").await.ok();
        let capacity = stores.and_then(|g| {
            let v: Vec<f64> = g.rows.iter().filter_map(|r| g.text(r, &["CAPACITY"]).and_then(|c| parse_size(&c))).collect();
            (!v.is_empty() && v.iter().sum::<f64>() > 0.0).then(|| v.iter().sum())
        });
        self.sizes_from_information_schema(s).await;
        if let Some(m) = s.metrics.iter_mut().find(|m| m.key == "storage_used") {
            m.max = capacity;
        }
        add(s, metric("uptime", "Tiempo activo", "Servidor", MetricUnit::Seconds, get(&status, "Uptime")));
        s.notes.push(
            "TiDB publica QPS, latencias y E/S en Prometheus/Grafana (puerto de estado /metrics), no por SQL; acá se ven la carga de los hosts, las sesiones y el resumen de sentencias.".into(),
        );
    }

    // --------------------------------------------------- StarRocks, Doris, VeloDB

    async fn olap(&mut self, s: &mut MonitorSnapshot) {
        let name = match self.product {
            Variant::StarRocks => "StarRocks",
            Variant::VeloDb => "VeloDB",
            _ => "Doris",
        };
        match self.grid("SHOW FRONTENDS").await {
            Ok(g) => {
                info(s, "Frontends", Some(g.rows.len().to_string()));
                // VERSION() answers the MySQL version it emulates.
                info(s, &format!("Versión de {name}"), g.rows.first().and_then(|r| g.text(r, &["Version"])));
                if let Some(r) = g.rows.iter().find(|r| g.text(r, &["IsMaster"]).as_deref() == Some("true")) {
                    info(s, "Frontend líder", g.text(r, &["Host", "IP"]));
                }
                s.tables.push(g.project(
                    "frontends",
                    "Frontends (FE)",
                    &[
                        (&["Host", "IP"], "servidor"),
                        (&["Role"], "rol"),
                        (&["IsMaster"], "líder"),
                        (&["Alive"], "vivo"),
                        (&["Join"], "unido"),
                        (&["ReplayedJournalId"], "journal"),
                        (&["StartTime"], "inicio"),
                        (&["LastHeartbeat"], "último latido"),
                        (&["Version"], "versión"),
                        (&["ErrMsg"], "error"),
                    ],
                ));
            }
            Err(e) => s.notes.push(format!("SHOW FRONTENDS requiere privilegios de administrador (OPERATE o NODE) ({}).", why(&e))),
        }
        let backends = match self.grid("SHOW BACKENDS").await {
            Ok(g) => Some(g),
            // Shared-data StarRocks runs compute nodes instead of backends.
            Err(e) => match self.grid("SHOW COMPUTE NODES").await {
                Ok(g) if !g.rows.is_empty() => Some(g),
                _ => {
                    s.notes.push(format!("SHOW BACKENDS requiere privilegios de administrador (OPERATE o NODE) ({}).", why(&e)));
                    None
                }
            },
        };
        if let Some(g) = backends {
            let col_sum = |names: &[&str], f: &dyn Fn(&str) -> Option<f64>| -> Option<f64> {
                let v: Vec<f64> = g.rows.iter().filter_map(|r| g.text(r, names).and_then(|t| f(&t))).collect();
                (!v.is_empty()).then(|| v.iter().sum())
            };
            let cpus: Vec<f64> = g.rows.iter().filter_map(|r| g.text(r, &["CpuUsedPct"]).and_then(|t| num(&t))).collect();
            let cpu = (!cpus.is_empty()).then(|| cpus.iter().sum::<f64>() / cpus.len() as f64);
            add(s, metric("cpu", "CPU de los backends (promedio)", "CPU", MetricUnit::Percent, cpu).max(Some(100.0)));
            // Memory used = MemUsedPct × MemLimit per backend.
            let mem: Vec<(f64, f64)> = g
                .rows
                .iter()
                .filter_map(|r| {
                    let pct = g.text(r, &["MemUsedPct"]).and_then(|t| num(&t))?;
                    let limit = g.text(r, &["MemLimit"]).and_then(|t| parse_size(&t))?;
                    Some((pct / 100.0 * limit, limit))
                })
                .collect();
            if !mem.is_empty() {
                add(
                    s,
                    metric("mem_used", "Memoria de los backends", "Memoria", MetricUnit::Bytes, Some(mem.iter().map(|m| m.0).sum()))
                        .max(Some(mem.iter().map(|m| m.1).sum())),
                );
            }
            let running = col_sum(&["NumRunningQueries"], &|t| num(t));
            add(s, metric("running_queries", "Consultas en ejecución", "Actividad", MetricUnit::Count, running));
            let used = col_sum(&["DataUsedCapacity"], &parse_size);
            let total = col_sum(&["TotalCapacity"], &parse_size);
            add(s, metric("storage_used", "Espacio usado", "Almacenamiento", MetricUnit::Bytes, used).max(total));
            add(s, metric("tablets", "Tablets", "Almacenamiento", MetricUnit::Count, col_sum(&["TabletNum"], &|t| num(t))));
            info(s, "Backends", Some(g.rows.len().to_string()));
            if cpu.is_none() {
                s.notes.push(format!("{name} no informa el uso de CPU por SQL; está en /metrics de cada backend (Prometheus)."));
            }
            s.tables.push(g.project(
                "nodes",
                "Backends (BE)",
                &[
                    (&["Host", "IP"], "servidor"),
                    (&["Alive"], "vivo"),
                    (&["TabletNum"], "tablets"),
                    (&["DataUsedCapacity"], "datos"),
                    (&["AvailCapacity"], "libre"),
                    (&["TotalCapacity"], "total"),
                    (&["UsedPct"], "uso de disco"),
                    (&["CpuCores"], "núcleos"),
                    (&["CpuUsedPct"], "CPU"),
                    (&["MemUsedPct"], "memoria"),
                    (&["MemLimit", "Memory"], "límite de memoria"),
                    (&["NumRunningQueries"], "consultas"),
                    (&["LastStartTime"], "inicio"),
                    (&["Version"], "versión"),
                    (&["ErrMsg"], "error"),
                ],
            ));
        }
        // Their information_schema.PROCESSLIST only lists this session.
        self.sessions(s, None).await;
        if let Ok(g) = self.grid("SHOW PROC '/current_queries'").await {
            s.tables.push(g.project(
                "running",
                "Consultas en ejecución (detalle)",
                &[
                    (&["QueryId"], "id"),
                    (&["ConnectionId"], "conexión"),
                    (&["feIp", "FE"], "frontend"),
                    (&["User"], "usuario"),
                    (&["Database"], "base"),
                    (&["ExecTime"], "duración"),
                    (&["ExecState"], "estado"),
                    (&["ScanRows"], "filas leídas"),
                    (&["ScanBytes"], "bytes leídos"),
                    (&["MemoryUsage"], "memoria"),
                    (&["CPUTime"], "CPU"),
                    (&["ResourceGroup", "Warehouse"], "grupo"),
                    (&["Statement", "Sql"], "consulta"),
                ],
            ));
        }
        match self.grid("SHOW PROC '/dbs'").await {
            Ok(g) => s.tables.push(g.project(
                "databases",
                "Bases de datos",
                &[
                    (&["DbName"], "base"),
                    (&["TableNum"], "tablas"),
                    (&["Size", "DataSize"], "tamaño"),
                    (&["ReplicaCount"], "réplicas"),
                    (&["Quota", "DataQuota"], "cuota"),
                    (&["LastConsistencyCheckTime"], "último control"),
                ],
            )),
            Err(_) => self.sizes_from_information_schema(s).await,
        }
        s.notes.push(format!(
            "{name} publica QPS, latencias y E/S solo por HTTP (/metrics en cada FE y BE), no por SQL."
        ));
        if self.product == Variant::VeloDb {
            s.notes.push("En VeloDB Cloud, el uso de CPU y memoria de cada cluster de cómputo está en la consola de VeloDB.".into());
        }
    }

    // ------------------------------------------------------------- Databend

    async fn databend(&mut self, s: &mut MonitorSnapshot) {
        match self.grid(&format!("SELECT * FROM system.processes LIMIT {MAX_ROWS}")).await {
            Ok(g) => {
                let mem: f64 = g.rows.iter().filter_map(|r| g.num(r, &["memory_usage"])).sum();
                let active = g
                    .rows
                    .iter()
                    .filter(|r| g.text(r, &["command"]).is_some_and(|c| !c.eq_ignore_ascii_case("sleep")) && g.text(r, &["extra_info"]).is_some_and(|q| !q.is_empty()))
                    .count();
                add(s, metric("connections", "Sesiones", "Conexiones", MetricUnit::Count, Some(g.rows.len() as f64)));
                add(s, metric("active_sessions", "Sesiones activas", "Conexiones", MetricUnit::Count, Some(active as f64)));
                add(s, metric("mem_used", "Memoria de las consultas", "Memoria", MetricUnit::Bytes, Some(mem)));
                s.tables.push(g.project(
                    "sessions",
                    "Sesiones",
                    &[
                        (&["id"], "id"),
                        (&["user"], "usuario"),
                        (&["database"], "base"),
                        (&["host"], "cliente"),
                        (&["command"], "comando"),
                        (&["status", "state"], "estado"),
                        (&["time"], "duración (s)"),
                        (&["memory_usage"], "memoria"),
                        (&["data_read_bytes"], "bytes leídos"),
                        (&["data_write_bytes"], "bytes escritos"),
                        (&["scan_progress_read_rows"], "filas leídas"),
                        (&["extra_info"], "consulta actual"),
                    ],
                ));
            }
            Err(e) => s.notes.push(format!("No se pudo leer system.processes ({}).", why(&e))),
        }
        match self.grid("SELECT * FROM system.clusters").await {
            Ok(g) => {
                info(s, "Nodos", Some(g.rows.len().to_string()));
                s.tables.push(g.table("nodes", "Nodos del cluster"));
            }
            Err(e) => s.notes.push(format!("No se pudo leer system.clusters ({}).", why(&e))),
        }
        let fresh = self.sizes.as_ref().is_some_and(|z| z.at.elapsed() < SIZES_TTL);
        if !fresh {
            let mut z = Sizes { at: Instant::now(), total: None, tables: Vec::new(), notes: Vec::new() };
            let dbs = "SELECT database AS base, COUNT(*) AS tablas, SUM(num_rows) AS filas, SUM(data_size) AS datos,
                              SUM(data_compressed_size) AS comprimido, SUM(index_size) AS `índices`
                       FROM system.tables WHERE database NOT IN ('system', 'information_schema')
                       GROUP BY database ORDER BY comprimido DESC LIMIT 200";
            match self.grid(dbs).await {
                Ok(g) => {
                    z.total = Some(
                        g.rows.iter().map(|r| g.num(r, &["comprimido"]).unwrap_or(0.0) + g.num(r, &["índices"]).unwrap_or(0.0)).sum(),
                    );
                    z.tables.push(g.table("databases", "Bases y tamaños (bytes)"));
                }
                Err(e) => z.notes.push(format!("No se pudieron leer los tamaños ({}).", why(&e))),
            }
            let top = "SELECT CONCAT(database, '.', name) AS objeto, engine AS motor, num_rows AS filas, data_size AS datos,
                              data_compressed_size AS comprimido, index_size AS `índices`
                       FROM system.tables WHERE database NOT IN ('system', 'information_schema')
                       ORDER BY data_compressed_size DESC LIMIT 20";
            if let Ok(g) = self.grid(top).await {
                z.tables.push(g.table("top_objects", "Tablas más grandes (bytes)"));
            }
            self.sizes = Some(z);
        }
        self.push_sizes(s, None);
        s.notes.push("Databend no expone el uso de CPU ni los contadores de consultas por SQL; están en /metrics de cada nodo (Prometheus).".into());
    }

    // ------------------------------------------------------------ Manticore

    async fn manticore(&mut self, s: &mut MonitorSnapshot) {
        let st = self.kv("SHOW STATUS").await.unwrap_or_default();
        // "load" is "1m 5m 15m" averages of busy workers.
        let load = st.get("load").and_then(|l| l.split_whitespace().next()).and_then(num);
        add(s, metric("load", "Carga de los workers (1 min)", "CPU", MetricUnit::Count, load));
        add(s, metric("connections", "Clientes conectados", "Conexiones", MetricUnit::Count, get(&st, "workers_clients")));
        add(
            s,
            metric("active_sessions", "Workers activos", "Conexiones", MetricUnit::Count, get(&st, "workers_active"))
                .max(get(&st, "workers_total")),
        );
        add(s, metric("work_queue", "Cola de trabajos", "Conexiones", MetricUnit::Count, get(&st, "work_queue_length")));
        add(s, metric("maxed_out", "Conexiones rechazadas", "Conexiones", MetricUnit::Count, get(&st, "maxed_out")).counter());
        add(s, metric("connections_total", "Conexiones aceptadas", "Conexiones", MetricUnit::Count, get(&st, "connections")).counter());
        add(s, metric("queries", "Búsquedas", "Actividad", MetricUnit::Count, get(&st, "queries")).counter());
        add(
            s,
            metric(
                "writes",
                "Escrituras",
                "Actividad",
                MetricUnit::Count,
                sum(&st, &["command_insert", "command_replace", "command_update", "command_delete"]),
            )
            .counter(),
        );
        add(s, metric("avg_query_wall", "Tiempo medio de búsqueda", "Actividad", MetricUnit::Seconds, get(&st, "avg_query_wall")));
        // query_cpu is "OFF" unless searchd runs with --cpustats.
        match get(&st, "query_cpu") {
            Some(c) => add(s, metric("cpu_time", "CPU de las búsquedas", "CPU", MetricUnit::Percent, Some(c * 100.0)).counter()),
            None => s.notes.push("El tiempo de CPU de las búsquedas requiere arrancar searchd con --cpustats.".into()),
        }
        add(
            s,
            metric("mem_cache", "Caché de consultas", "Caché", MetricUnit::Bytes, get(&st, "qcache_used_bytes")).max(get(&st, "qcache_max_bytes")),
        );
        add(s, metric("qcache_hits", "Aciertos de la caché de consultas", "Caché", MetricUnit::Count, get(&st, "qcache_hits")).counter());
        add(s, metric("uptime", "Tiempo activo", "Servidor", MetricUnit::Seconds, get(&st, "uptime")));
        info(s, "Workers", st.get("workers_total").cloned());

        match self.grid("SHOW THREADS").await {
            Ok(g) => {
                let pick: &[(&[&str], &str)] = &[
                    (&["TID", "Tid"], "id"),
                    (&["Name"], "hilo"),
                    (&["Proto"], "protocolo"),
                    (&["Connection from", "Host"], "cliente"),
                    (&["State"], "estado"),
                    (&["This/prev job time", "Time"], "duración"),
                    (&["Jobs done"], "trabajos"),
                    (&["Info"], "consulta actual"),
                ];
                let mut t = g.project("sessions", "Hilos", pick);
                let info_col = t.columns.iter().position(|c| c == "consulta actual");
                // Hide this monitor's own SHOW THREADS.
                t.rows.retain(|r| info_col.is_none_or(|i| r[i].as_str() != Some("SHOW THREADS")));
                s.tables.push(t);
            }
            Err(e) => s.notes.push(format!("No se pudo leer SHOW THREADS ({}).", why(&e))),
        }

        let fresh = self.sizes.as_ref().is_some_and(|z| z.at.elapsed() < SIZES_TTL);
        if !fresh {
            let mut z = Sizes { at: Instant::now(), total: None, tables: Vec::new(), notes: Vec::new() };
            let names: Vec<String> = self.optional_rows("SHOW TABLES").await.iter().filter_map(|r| at(r, 0)).take(MAX_ROWS).collect();
            let mut t = MonitorTable::new("databases", "Tablas y tamaños", &["tabla", "documentos", "RAM", "disco", "bytes indexados"]);
            let mut total = 0.0;
            for n in names {
                let q = dbine_driver::sql::quote_ident(dbine_driver::sql::Quote::Backtick, &n);
                let Some(kv) = self.kv(&format!("SHOW TABLE {q} STATUS")).await else { continue };
                let disk = get(&kv, "disk_bytes");
                total += disk.unwrap_or(0.0);
                t.rows.push(vec![
                    json!(n),
                    opt(get(&kv, "indexed_documents")),
                    opt(get(&kv, "ram_bytes")),
                    opt(disk),
                    opt(get(&kv, "indexed_bytes")),
                ]);
            }
            t.rows.sort_by(|a, b| b[3].as_f64().unwrap_or(0.0).total_cmp(&a[3].as_f64().unwrap_or(0.0)));
            z.total = Some(total);
            z.tables.push(t);
            self.sizes = Some(z);
        }
        self.push_sizes(s, None);
        s.notes.push("Manticore no expone el uso de CPU ni de memoria del proceso por SQL.".into());
    }

    // ----------------------------------------------------------- GreptimeDB

    async fn greptime(&mut self, s: &mut MonitorSnapshot) {
        match self.grid("SELECT * FROM information_schema.cluster_info").await {
            Ok(g) => {
                let total = |names: &[&str]| -> Option<f64> {
                    let v: Vec<f64> = g.rows.iter().filter_map(|r| g.num(r, names)).collect();
                    (!v.is_empty()).then(|| v.iter().sum())
                };
                let cpu = match (total(&["cpu_usage_millicores"]), total(&["total_cpu_millicores"])) {
                    (Some(u), Some(t)) if t > 0.0 => Some((u / t * 100.0).clamp(0.0, 100.0)),
                    _ => None,
                };
                add(s, metric("cpu", "CPU del servidor", "CPU", MetricUnit::Percent, cpu).max(Some(100.0)));
                add(s, metric("mem_used", "Memoria usada", "Memoria", MetricUnit::Bytes, total(&["memory_usage_bytes"])).max(total(&["total_memory_bytes"])));
                if let Some(r) = g.rows.first() {
                    info(s, "Tiempo activo", g.text(r, &["uptime"]));
                    info(s, "Inicio", g.text(r, &["start_time"]));
                }
                info(s, "Nodos", Some(g.rows.len().to_string()));
                s.tables.push(g.project(
                    "nodes",
                    "Nodos",
                    &[
                        (&["peer_id"], "id"),
                        (&["peer_type"], "tipo"),
                        (&["peer_addr"], "dirección"),
                        (&["peer_hostname"], "host"),
                        (&["cpu_usage_millicores"], "CPU (milinúcleos)"),
                        (&["total_cpu_millicores"], "CPU total (milinúcleos)"),
                        (&["memory_usage_bytes"], "memoria usada"),
                        (&["total_memory_bytes"], "memoria total"),
                        (&["version"], "versión"),
                        (&["uptime"], "tiempo activo"),
                        (&["node_status"], "estado"),
                    ],
                ));
            }
            Err(e) => s.notes.push(format!("No se pudo leer information_schema.cluster_info ({}).", why(&e))),
        }
        match self.grid("SELECT * FROM information_schema.process_list").await {
            Ok(g) => {
                let t = g.project(
                    "queries",
                    "Consultas en curso",
                    &[
                        (&["id"], "id"),
                        (&["catalog"], "catálogo"),
                        (&["schemas"], "base"),
                        (&["client"], "cliente"),
                        (&["frontend"], "frontend"),
                        (&["start_timestamp"], "inicio"),
                        (&["elapsed_time"], "duración"),
                        (&["query"], "consulta"),
                    ],
                );
                let q = t.columns.iter().position(|c| c == "consulta");
                let mut t = t;
                t.rows.retain(|r| q.is_none_or(|i| !r[i].as_str().is_some_and(|s| s.contains("information_schema.process_list"))));
                add(s, metric("active_sessions", "Consultas en curso", "Conexiones", MetricUnit::Count, Some(t.rows.len() as f64)));
                s.tables.push(t);
            }
            Err(e) => s.notes.push(format!("No se pudo leer process_list ({}).", why(&e))),
        }
        // Regions: live totals, and the sizes per table.
        let regions = "SELECT SUM(disk_size), SUM(memtable_size), SUM(written_bytes_since_open),
                              SUM(query_cpu_time_millis), SUM(query_scanned_bytes), SUM(region_rows), COUNT(*)
                       FROM information_schema.region_statistics";
        match self.rows(regions).await {
            Ok(rows) => {
                let r = rows.first();
                let n = |i: usize| r.and_then(|r| at(r, i)).and_then(|v| num(&v));
                add(s, metric("storage_used", "Espacio usado", "Almacenamiento", MetricUnit::Bytes, n(0)));
                add(s, metric("mem_cache", "Memtables", "Memoria", MetricUnit::Bytes, n(1)));
                add(s, metric("bytes_written", "Bytes escritos", "Actividad", MetricUnit::Bytes, n(2)).counter());
                // Milliseconds of CPU → seconds × 100.
                add(s, metric("cpu_time", "CPU de las consultas", "CPU", MetricUnit::Percent, n(3).map(|ms| ms / 10.0)).counter());
                add(s, metric("disk_read", "Bytes escaneados", "Disco", MetricUnit::Bytes, n(4)).counter());
                add(s, metric("rows", "Filas almacenadas", "Almacenamiento", MetricUnit::Count, n(5)));
                add(s, metric("regions", "Regiones", "Almacenamiento", MetricUnit::Count, n(6)));
            }
            Err(e) => s.notes.push(format!("No se pudo leer region_statistics ({}).", why(&e))),
        }
        let fresh = self.sizes.as_ref().is_some_and(|z| z.at.elapsed() < SIZES_TTL);
        if !fresh {
            let mut z = Sizes { at: Instant::now(), total: None, tables: Vec::new(), notes: Vec::new() };
            let dbs = "SELECT t.table_schema AS base, COUNT(DISTINCT t.table_id) AS tablas, SUM(r.region_rows) AS filas,
                              SUM(r.disk_size) AS disco, SUM(r.memtable_size) AS memtables, SUM(r.sst_size) AS sst,
                              SUM(r.index_size) AS `índices`
                       FROM information_schema.region_statistics r
                       JOIN information_schema.tables t ON t.table_id = r.table_id
                       GROUP BY t.table_schema ORDER BY disco DESC LIMIT 200";
            match self.grid(dbs).await {
                Ok(g) => z.tables.push(g.table("databases", "Bases y tamaños (bytes)")),
                Err(e) => z.notes.push(format!("No se pudieron leer los tamaños por base ({}).", why(&e))),
            }
            let top = "SELECT CONCAT(t.table_schema, '.', t.table_name) AS objeto, COUNT(*) AS regiones,
                              SUM(r.region_rows) AS filas, SUM(r.disk_size) AS disco, SUM(r.memtable_size) AS memtables,
                              SUM(r.sst_num) AS `archivos SST`
                       FROM information_schema.region_statistics r
                       JOIN information_schema.tables t ON t.table_id = r.table_id
                       GROUP BY t.table_schema, t.table_name ORDER BY disco DESC LIMIT 20";
            if let Ok(g) = self.grid(top).await {
                z.tables.push(g.table("top_objects", "Tablas más grandes (bytes)"));
            }
            self.sizes = Some(z);
        }
        if let Some(z) = &self.sizes {
            s.tables.extend(z.tables.iter().cloned());
            s.notes.extend(z.notes.iter().cloned());
        }
        s.notes.push("GreptimeDB no expone contadores de consultas ni de conexiones por SQL; están en /metrics (Prometheus).".into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_parse() {
        assert_eq!(parse_size("1.000 KB"), Some(1024.0));
        assert_eq!(parse_size("2 GB"), Some(2.0 * 1024f64.powi(3)));
        assert_eq!(parse_size("3.5GiB"), Some(3.5 * 1024f64.powi(3)));
        assert_eq!(parse_size("0.000 "), Some(0.0));
        assert_eq!(parse_size("12 B"), Some(12.0));
        assert_eq!(parse_size("7 Bytes"), Some(7.0));
        assert_eq!(parse_size("1 TB"), Some(1024f64.powi(4)));
        assert_eq!(parse_size("n/a"), None);
    }

    #[test]
    fn helpers() {
        let kv: Kv = [("a".to_string(), "2".to_string()), ("b".into(), "3.5".into()), ("c".into(), "ON".into())].into();
        assert_eq!(sum(&kv, &["A", "b", "missing"]), Some(5.5));
        assert_eq!(sum(&kv, &["missing"]), None);
        assert!(on(&kv, "c") && !on(&kv, "a"));
        assert_eq!(human(1536.0), "1.5 KB");
        let long = "x".repeat(MAX_TEXT + 10);
        assert_eq!(clip(&long).chars().count(), MAX_TEXT + 1);
    }

    #[test]
    fn grids_project_by_name() {
        let g = Grid {
            columns: vec!["Id".into(), "User".into(), "Info".into()],
            rows: vec![vec![json!(1), json!("root"), Value::Null]],
        };
        let t = g.project("sessions", "Sesiones", &[(&["ID"], "id"), (&["db"], "base"), (&["INFO"], "consulta")]);
        assert_eq!(t.columns, ["id", "consulta"]);
        assert_eq!(t.rows, vec![vec![json!(1), Value::Null]]);
        assert_eq!(g.num(&g.rows[0], &["id"]), Some(1.0));
        assert_eq!(g.text(&g.rows[0], &["user"]).as_deref(), Some("root"));
    }
}
