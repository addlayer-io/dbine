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
const SIZES_HELP: &str = "Datos e índices de todas las tablas según information_schema, sin las bases de sistema; se actualiza cada minuto. Son las cifras que estima el motor (en InnoDB, aproximadas).";
const SIZES_SOURCE: &str =
    "information_schema.TABLES: SUM(DATA_LENGTH + INDEX_LENGTH), refreshed every minute";

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
    keys.iter()
        .filter_map(|k| get(kv, k))
        .fold(None, |acc, v| Some(acc.unwrap_or(0.0) + v))
}

fn on(kv: &Kv, k: &str) -> bool {
    kv.get(k)
        .is_some_and(|v| v.eq_ignore_ascii_case("ON") || v == "1")
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
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .unwrap_or(s.len());
    let n: f64 = s[..split].parse().ok()?;
    let unit = s[split..].trim().to_ascii_uppercase();
    let unit = unit
        .trim_end_matches("BYTES")
        .trim_end_matches("IB")
        .trim_end_matches('B');
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

/// Performance Schema's statement CPU time (picoseconds) as a share of the
/// CPUs InnoDB sees (`cpu_n`): its rate is then the % of all of them (100 =
/// every CPU busy), never "500 %" for five busy cores. Unknown CPU count:
/// no value.
fn statement_cpu(picos: f64, cpus: Option<f64>) -> Metric {
    metric(
        "cpu_time",
        "CPU de las sentencias",
        "CPU",
        MetricUnit::Percent,
        cpus.map(|n| picos / 1e10 / n),
    )
    .max(Some(100.0))
    .counter()
    .help("Tiempo de CPU que usaron las sentencias, como parte de todos los núcleos del servidor: 100 % equivale a todos ocupados con sentencias. Alto y sostenido suele venir de consultas sin un índice adecuado o de ordenamientos grandes.")
    .source("performance_schema.events_statements_summary_global_by_event_name: SUM(SUM_CPU_TIME) (picoseconds), ÷ information_schema.INNODB_METRICS cpu_n")
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
    /// Rows the server sent; `rows` keeps the first `MAX_ROWS`.
    total: usize,
}

impl Grid {
    fn col(&self, names: &[&str]) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| names.iter().any(|n| c.eq_ignore_ascii_case(n)))
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
        let cols: Vec<(usize, &str)> = pick
            .iter()
            .filter_map(|(names, label)| Some((self.col(names)?, *label)))
            .collect();
        let labels: Vec<&str> = cols.iter().map(|(_, l)| *l).collect();
        let mut t = MonitorTable::new(key, title, &labels);
        t.rows = self
            .rows
            .iter()
            .map(|r| {
                cols.iter()
                    .map(|(i, _)| r.get(*i).cloned().unwrap_or(Value::Null))
                    .collect()
            })
            .collect();
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
        format!(
            "{}…",
            &s[..s.char_indices().nth(200).map_or(s.len(), |(i, _)| i)]
        )
    } else {
        s.to_string()
    }
}

/// Manticore's figures from `SHOW STATUS`, each with what it measures and
/// where it comes from (no value: not reported).
fn manticore_metrics(st: &Kv) -> Vec<Metric> {
    // "load" is "1m 5m 15m" averages of the worker pool.
    let load = st
        .get("load")
        .and_then(|l| l.split_whitespace().next())
        .and_then(num);
    let workers = get(st, "workers_total").filter(|w| *w > 0.0);
    vec![
        metric("load", "Carga de los workers (1 min)", "CPU", MetricUnit::Count, load)
            .help("Carga del pool de workers de searchd, promedio del último minuto: crece con los trabajos en curso y en cola.")
            .source("SHOW STATUS: load (the 1-minute value)"),
        metric(
            "connections",
            "Clientes conectados",
            "Conexiones",
            MetricUnit::Count,
            get(st, "workers_clients"),
        )
        .help("Conexiones de clientes abiertas ahora, por el protocolo MySQL o HTTP, incluida la del monitor.")
        .source("SHOW STATUS: workers_clients"),
        metric(
            "active_sessions",
            "Workers activos",
            "Conexiones",
            MetricUnit::Count,
            get(st, "workers_active"),
        )
        .max(get(st, "workers_total"))
        .help("Workers (hilos del pool de searchd) ocupados ahora; el máximo es workers_total, por omisión uno por núcleo. Todos ocupados por mucho tiempo significa que los pedidos hacen cola.")
        .source("SHOW STATUS: workers_active (max: workers_total)"),
        metric(
            "work_queue",
            "Cola de trabajos",
            "Conexiones",
            MetricUnit::Count,
            get(st, "work_queue_length"),
        )
        .help("Trabajos esperando un worker libre. Si no vuelve a cero, searchd no da abasto.")
        .source("SHOW STATUS: work_queue_length"),
        metric(
            "maxed_out",
            "Conexiones rechazadas",
            "Conexiones",
            MetricUnit::Count,
            get(st, "maxed_out"),
        )
        .counter()
        .help("Conexiones que searchd rechazó porque ya tenía abiertas las que permite max_connections.")
        .source("SHOW STATUS: maxed_out"),
        metric(
            "connections_total",
            "Conexiones aceptadas",
            "Conexiones",
            MetricUnit::Count,
            get(st, "connections"),
        )
        .counter()
        .help("Conexiones que aceptó searchd, por cualquier protocolo.")
        .source("SHOW STATUS: connections"),
        metric("queries", "Búsquedas", "Actividad", MetricUnit::Count, get(st, "queries"))
            .counter()
            .help("Búsquedas que atendió searchd, por cualquier protocolo.")
            .source("SHOW STATUS: queries"),
        metric(
            "writes",
            "Escrituras",
            "Actividad",
            MetricUnit::Count,
            sum(
                st,
                &[
                    "command_insert",
                    "command_replace",
                    "command_update",
                    "command_delete",
                ],
            ),
        )
        .counter()
        .help("Pedidos INSERT, REPLACE, UPDATE y DELETE que recibió searchd.")
        .source("SHOW STATUS: command_insert + command_replace + command_update + command_delete"),
        metric(
            "avg_query_wall",
            "Tiempo medio de búsqueda",
            "Actividad",
            MetricUnit::Seconds,
            get(st, "avg_query_wall"),
        )
        .help("Tiempo de reloj medio por búsqueda, promedio desde el arranque, por eso cambia despacio.")
        .source("SHOW STATUS: avg_query_wall"),
        // Seconds of CPU over the workers (by default one per core): the
        // rate is the share of all of them, never "500 %" for five.
        metric(
            "cpu_time",
            "CPU de las búsquedas",
            "CPU",
            MetricUnit::Percent,
            get(st, "query_cpu")
                .zip(workers)
                .map(|(c, w)| c * 100.0 / w),
        )
        .max(Some(100.0))
        .counter()
        .help("Tiempo de CPU de las búsquedas como parte de los workers de searchd (por omisión, uno por núcleo): 100 % equivale a todos ocupados con búsquedas. Solo se mide con searchd arrancado con --cpustats.")
        .source("SHOW STATUS: query_cpu (seconds), ÷ workers_total"),
        metric(
            "mem_cache",
            "Caché de consultas",
            "Caché",
            MetricUnit::Bytes,
            get(st, "qcache_used_bytes"),
        )
        .max(get(st, "qcache_max_bytes"))
        .help("Memoria que ocupa la caché de resultados de búsquedas; el máximo es qcache_max_bytes.")
        .source("SHOW STATUS: qcache_used_bytes (max: qcache_max_bytes)"),
        metric(
            "qcache_hits",
            "Aciertos de la caché de consultas",
            "Caché",
            MetricUnit::Count,
            get(st, "qcache_hits"),
        )
        .counter()
        .help("Búsquedas que se respondieron desde la caché de resultados, sin volver a ejecutarse.")
        .source("SHOW STATUS: qcache_hits"),
        metric("uptime", "Tiempo activo", "Servidor", MetricUnit::Seconds, get(st, "uptime"))
            .help("Tiempo desde que arrancó searchd.")
            .source("SHOW STATUS: uptime"),
    ]
}

impl MySqlSession {
    async fn grid(&mut self, sql: &str) -> Result<Grid> {
        let mut res = self.conn.query_iter(sql).await.map_err(err)?;
        let cols = res.columns().map(|c| c.to_vec()).unwrap_or_default();
        let mut rows = Vec::new();
        let mut total = 0;
        while let Some(row) = res.next().await.map_err(err)? {
            total += 1;
            if rows.len() < MAX_ROWS {
                rows.push(
                    row.unwrap()
                        .into_iter()
                        .zip(&cols)
                        .map(|(v, c)| clip_json(cell(c, v)))
                        .collect(),
                );
            }
        }
        res.drop_result().await.map_err(err)?;
        Ok(Grid {
            columns: cols.iter().map(|c| c.name_str().into_owned()).collect(),
            rows,
            total,
        })
    }

    /// Two-column `name, value` rows as a map; `None` if the query failed.
    async fn kv(&mut self, sql: &str) -> Option<Kv> {
        let rows = self.rows(sql).await.ok()?;
        Some(
            rows.iter()
                .filter_map(|r| {
                    Some((at(r, 0)?.to_ascii_lowercase(), at(r, 1).unwrap_or_default()))
                })
                .collect(),
        )
    }

    async fn scalar(&mut self, sql: &str) -> Option<f64> {
        let rows = self.rows(sql).await.ok()?;
        rows.first().and_then(|r| at(r, 0)).and_then(|v| num(&v))
    }

    pub(crate) async fn snapshot(&mut self) -> Result<MonitorSnapshot> {
        let mut s = MonitorSnapshot::default();
        // The one query that must work: it proves the connection is alive.
        let version = self
            .rows("SELECT VERSION()")
            .await?
            .first()
            .and_then(|r| at(r, 0))
            .unwrap_or_default();
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
            s.notes.push(
                "El servidor no respondió SHOW GLOBAL STATUS: no hay contadores de actividad."
                    .into(),
            );
        }
        let wanted = [
            "max_connections",
            "innodb_buffer_pool_size",
            "version_comment",
            "hostname",
            "time_zone",
            "system_time_zone",
            "read_only",
            "super_read_only",
            "server_id",
            "performance_schema",
            "default_storage_engine",
            "character_set_server",
            "aurora_version",
            "aurora_server_id",
            "log_bin",
            "innodb_read_only",
        ];
        let list = wanted.iter().map(|w| lit(w)).collect::<Vec<_>>().join(", ");
        let vars = match self
            .kv(&format!(
                "SHOW GLOBAL VARIABLES WHERE Variable_name IN ({list})"
            ))
            .await
        {
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
        let tz = vars
            .get("time_zone")
            .filter(|t| !t.eq_ignore_ascii_case("SYSTEM"))
            .or(vars.get("system_time_zone"));
        info(s, "Zona horaria", tz.cloned());
        info(s, "Máx. conexiones", vars.get("max_connections").cloned());
        info(
            s,
            "Buffer pool InnoDB",
            get(&vars, "innodb_buffer_pool_size").map(human),
        );
        info(
            s,
            "Motor predeterminado",
            vars.get("default_storage_engine").cloned(),
        );
        info(
            s,
            "Juego de caracteres",
            vars.get("character_set_server").cloned(),
        );
        info(s, "ID de servidor", vars.get("server_id").cloned());
        if full {
            info(s, "Log binario", vars.get("log_bin").cloned());
            info(
                s,
                "Performance Schema",
                vars.get("performance_schema").cloned(),
            );
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
        add(
            s,
            metric("cpu", "CPU del servidor", "CPU", MetricUnit::Percent, cpu)
                .max(Some(100.0))
                .help("Uso de CPU de esta instancia de Aurora como lo informa Aurora: 100 % equivale a todos sus vCPU ocupados. Alto y sostenido suele venir de consultas costosas o de muchas conexiones a la vez.")
                .source("information_schema.REPLICA_HOST_STATUS: CPU (the row of this instance, aurora_server_id)"),
        );
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
                Some(ps) => {
                    // How many CPUs InnoDB sees: its cpu_n counter, off by default.
                    let cpus = self
                        .scalar("SELECT `COUNT` FROM information_schema.INNODB_METRICS WHERE NAME = 'cpu_n'")
                        .await
                        .filter(|n| *n > 0.0);
                    add(s, statement_cpu(ps, cpus));
                    if cpus.is_none() {
                        s.notes.push(match self.product {
                            // Managed: no SET GLOBAL, the setting goes in
                            // the parameter group or the instance's flags.
                            Variant::AuroraMySql | Variant::CloudSqlMySql => {
                                "El tiempo de CPU de las sentencias se muestra como parte de los núcleos del servidor, y MySQL solo informa cuántos son con el contador cpu_n de InnoDB: en Aurora o Cloud SQL, agregá cpu_n a innodb_monitor_enable en el grupo de parámetros (o en las marcas de la instancia)."
                            }
                            _ => "El tiempo de CPU de las sentencias se muestra como parte de los núcleos del servidor, y MySQL solo informa cuántos son con el contador cpu_n de InnoDB: activalo con SET GLOBAL innodb_monitor_enable = 'cpu_n'.",
                        }.into());
                    }
                }
                None => s.notes.push(
                    "El tiempo de CPU de las sentencias requiere activar el consumidor events_statements_cpu de Performance Schema (MySQL 8.0.28+).".into(),
                ),
            }
        }

        // Memory.
        let (mem, mem_help, mem_src) = if maria {
            (
                get(&status, "Memory_used"),
                "Memoria que MariaDB tiene reservada según su propio contador: el buffer pool de InnoDB, el key buffer, las cachés y la memoria de cada conexión. Antes de MariaDB 10.6 no incluye el buffer pool ni el key buffer.",
                "SHOW GLOBAL STATUS: Memory_used",
            )
        } else if ps {
            (
                self.scalar("SELECT SUM(CURRENT_NUMBER_OF_BYTES_USED) FROM performance_schema.memory_summary_global_by_event_name")
                    .await,
                "Memoria que registra Performance Schema: buffer pool, cachés y la memoria de cada conexión. No cuenta la que no tiene un instrumento activado.",
                "performance_schema.memory_summary_global_by_event_name: SUM(CURRENT_NUMBER_OF_BYTES_USED)",
            )
        } else {
            (None, "", "")
        };
        add(
            s,
            metric(
                "mem_used",
                "Memoria usada",
                "Memoria",
                MetricUnit::Bytes,
                mem,
            )
            .help(mem_help)
            .source(mem_src),
        );
        if mem.is_none() && full {
            s.notes.push("La memoria usada solo se ve con Performance Schema activado (performance_schema = ON).".into());
        }
        let (pool, pool_src) = match get(&status, "Innodb_buffer_pool_bytes_data") {
            Some(b) => (
                Some(b),
                "SHOW GLOBAL STATUS: Innodb_buffer_pool_bytes_data (max: innodb_buffer_pool_size)",
            ),
            None => (
                get(&status, "Innodb_buffer_pool_pages_data")
                    .map(|p| p * get(&status, "Innodb_page_size").unwrap_or(16384.0)),
                "SHOW GLOBAL STATUS: Innodb_buffer_pool_pages_data × Innodb_page_size (max: innodb_buffer_pool_size)",
            ),
        };
        add(
            s,
            metric(
                "mem_cache",
                "Buffer pool (datos)",
                "Memoria",
                MetricUnit::Bytes,
                pool,
            )
            .max(get(&vars, "innodb_buffer_pool_size"))
            .help("Datos que ocupa ahora el buffer pool de InnoDB, en páginas limpias o modificadas; el máximo es innodb_buffer_pool_size. Lleno es lo normal cuando hay más datos que buffer pool.")
            .source(pool_src),
        );

        // Connections and activity.
        let max_conn = get(&vars, "max_connections").filter(|m| *m > 0.0);
        add(
            s,
            metric(
                "connections",
                "Conexiones",
                "Conexiones",
                MetricUnit::Count,
                get(&status, "Threads_connected"),
            )
            .max(max_conn)
            .help("Conexiones de clientes abiertas ahora en este servidor, incluida la del monitor. El máximo es max_connections: al llegar, las nuevas se rechazan con «Too many connections».")
            .source("SHOW GLOBAL STATUS: Threads_connected (max: max_connections)"),
        );
        add(
            s,
            metric(
                "active_sessions",
                "Sesiones activas",
                "Conexiones",
                MetricUnit::Count,
                get(&status, "Threads_running"),
            )
            .help("Hilos ejecutando una sentencia ahora, incluido el del monitor. Si se mantiene por encima de la cantidad de núcleos, las sentencias compiten por CPU o esperan bloqueos.")
            .source("SHOW GLOBAL STATUS: Threads_running"),
        );
        add(
            s,
            metric(
                "aborted_connects",
                "Conexiones rechazadas",
                "Conexiones",
                MetricUnit::Count,
                get(&status, "Aborted_connects"),
            )
            .counter()
            .help("Intentos de conexión que fallaron: contraseña incorrecta, sin permiso sobre la base, paquetes inválidos o tiempo agotado. Si sube de forma sostenida, puede ser un cliente mal configurado o alguien probando contraseñas.")
            .source("SHOW GLOBAL STATUS: Aborted_connects"),
        );
        let (queries, queries_help, queries_src) = match get(&status, "Questions") {
            Some(q) => (
                Some(q),
                "Sentencias que enviaron los clientes, sin contar las que se ejecutan dentro de procedimientos almacenados.",
                "SHOW GLOBAL STATUS: Questions",
            ),
            None => (
                get(&status, "Queries"),
                "Sentencias que ejecutó el servidor, incluidas las de procedimientos almacenados.",
                "SHOW GLOBAL STATUS: Queries",
            ),
        };
        add(
            s,
            metric(
                "queries",
                "Consultas",
                "Actividad",
                MetricUnit::Count,
                queries,
            )
            .counter()
            .help(queries_help)
            .source(queries_src),
        );
        let (commits, commits_help, commits_src) = match get(&status, "Handler_commit") {
            Some(c) => (
                Some(c),
                "Commits internos de los motores de almacenamiento. En autocommit, cada sentencia sobre una tabla transaccional cuenta uno, también los SELECT.",
                "SHOW GLOBAL STATUS: Handler_commit",
            ),
            None => (
                sum(&status, &["Com_commit", "Com_rollback"]),
                "COMMIT y ROLLBACK explícitos que ejecutaron los clientes; las sentencias en autocommit no cuentan.",
                "SHOW GLOBAL STATUS: Com_commit + Com_rollback",
            ),
        };
        add(
            s,
            metric(
                "transactions",
                "Transacciones (commits)",
                "Actividad",
                MetricUnit::Count,
                commits,
            )
            .counter()
            .help(commits_help)
            .source(commits_src),
        );
        let (read, read_help, read_src) = match get(&status, "Innodb_rows_read") {
            Some(r) => (
                Some(r),
                "Filas que leyó InnoDB para responder sentencias. Si crece mucho más que las consultas, hay sentencias que recorren tablas o índices enteros.",
                "SHOW GLOBAL STATUS: Innodb_rows_read",
            ),
            None => (
                sum(
                    &status,
                    &[
                        "Handler_read_first",
                        "Handler_read_key",
                        "Handler_read_next",
                        "Handler_read_rnd",
                        "Handler_read_rnd_next",
                    ],
                ),
                "Pedidos de lectura de filas que el servidor hizo a los motores de almacenamiento, por índice o recorriendo la tabla. Si crece mucho más que las consultas, hay sentencias que recorren tablas enteras.",
                "SHOW GLOBAL STATUS: Handler_read_first + Handler_read_key + Handler_read_next + Handler_read_rnd + Handler_read_rnd_next",
            ),
        };
        let (written, written_help, written_src) = match sum(
            &status,
            &[
                "Innodb_rows_inserted",
                "Innodb_rows_updated",
                "Innodb_rows_deleted",
            ],
        ) {
            Some(w) => (
                Some(w),
                "Filas insertadas, actualizadas y borradas en tablas InnoDB.",
                "SHOW GLOBAL STATUS: Innodb_rows_inserted + Innodb_rows_updated + Innodb_rows_deleted",
            ),
            None => (
                sum(
                    &status,
                    &["Handler_write", "Handler_update", "Handler_delete"],
                ),
                "Filas que los motores de almacenamiento insertaron, actualizaron o borraron; incluye las escrituras en tablas temporales internas.",
                "SHOW GLOBAL STATUS: Handler_write + Handler_update + Handler_delete",
            ),
        };
        add(
            s,
            metric(
                "rows_read",
                "Filas leídas",
                "Actividad",
                MetricUnit::Count,
                read,
            )
            .counter()
            .help(read_help)
            .source(read_src),
        );
        add(
            s,
            metric(
                "rows_written",
                "Filas escritas",
                "Actividad",
                MetricUnit::Count,
                written,
            )
            .counter()
            .help(written_help)
            .source(written_src),
        );
        add(
            s,
            metric(
                "slow_queries",
                "Consultas lentas",
                "Actividad",
                MetricUnit::Count,
                get(&status, "Slow_queries"),
            )
            .counter()
            .help("Sentencias que tardaron más que long_query_time (10 segundos por omisión). Se cuentan aunque el log de consultas lentas esté apagado.")
            .source("SHOW GLOBAL STATUS: Slow_queries"),
        );
        add(
            s,
            metric(
                "tmp_disk_tables",
                "Tablas temporales en disco",
                "Actividad",
                MetricUnit::Count,
                get(&status, "Created_tmp_disk_tables"),
            )
            .counter()
            .help("Tablas temporales internas que no entraron en memoria y se crearon en disco, por GROUP BY, ORDER BY, UNION o subconsultas grandes. Si sube, revisá esas consultas o tmp_table_size.")
            .source("SHOW GLOBAL STATUS: Created_tmp_disk_tables"),
        );
        add(
            s,
            metric(
                "net_in",
                "Red entrante",
                "Red",
                MetricUnit::Bytes,
                get(&status, "Bytes_received"),
            )
            .counter()
            .help("Bytes que el servidor recibió de todos los clientes.")
            .source("SHOW GLOBAL STATUS: Bytes_received"),
        );
        add(
            s,
            metric(
                "net_out",
                "Red saliente",
                "Red",
                MetricUnit::Bytes,
                get(&status, "Bytes_sent"),
            )
            .counter()
            .help("Bytes que el servidor envió a todos los clientes. Un salto suele venir de consultas que devuelven muchas filas.")
            .source("SHOW GLOBAL STATUS: Bytes_sent"),
        );
        add(
            s,
            metric(
                "disk_read",
                "Lectura en disco",
                "Disco",
                MetricUnit::Bytes,
                get(&status, "Innodb_data_read"),
            )
            .counter()
            .help("Bytes que InnoDB leyó de sus archivos; incluye lo que resuelve la caché del sistema operativo. Si sube junto con la caída de aciertos del buffer pool, los datos no entran en memoria.")
            .source("SHOW GLOBAL STATUS: Innodb_data_read"),
        );
        add(
            s,
            metric(
                "disk_write",
                "Escritura en disco",
                "Disco",
                MetricUnit::Bytes,
                get(&status, "Innodb_data_written"),
            )
            .counter()
            .help("Bytes que InnoDB escribió en sus archivos.")
            .source("SHOW GLOBAL STATUS: Innodb_data_written"),
        );
        let hit = match (
            get(&status, "Innodb_buffer_pool_read_requests"),
            get(&status, "Innodb_buffer_pool_reads"),
        ) {
            (Some(req), Some(miss)) if req > 0.0 => {
                Some((100.0 * (1.0 - miss / req)).clamp(0.0, 100.0))
            }
            _ => None,
        };
        add(
            s,
            metric(
                "cache_hit",
                "Aciertos del buffer pool",
                "Caché",
                MetricUnit::Percent,
                hit,
            )
            .max(Some(100.0))
            .help("Porcentaje de lecturas de páginas de InnoDB que se resolvieron en el buffer pool sin ir al disco, acumulado desde el arranque, por eso cambia despacio. Si baja, el buffer pool quedó chico para los datos que se consultan.")
            .source("SHOW GLOBAL STATUS: 1 − Innodb_buffer_pool_reads / Innodb_buffer_pool_read_requests"),
        );

        // Locks.
        add(
            s,
            metric(
                "locks_waiting",
                "Bloqueos en espera",
                "Bloqueos",
                MetricUnit::Count,
                get(&status, "Innodb_row_lock_current_waits"),
            )
            .help("Sentencias esperando ahora un bloqueo de fila de InnoDB. Si no baja, mirá en la tabla de bloqueos qué sesión los retiene.")
            .source("SHOW GLOBAL STATUS: Innodb_row_lock_current_waits"),
        );
        let (deadlocks, deadlocks_src) = match get(&status, "Innodb_deadlocks") {
            Some(d) => (Some(d), "SHOW GLOBAL STATUS: Innodb_deadlocks"),
            None if full => (
                self.scalar("SELECT `COUNT` FROM information_schema.INNODB_METRICS WHERE NAME = 'lock_deadlocks'").await,
                "information_schema.INNODB_METRICS: COUNT WHERE NAME = 'lock_deadlocks'",
            ),
            None => (None, ""),
        };
        add(
            s,
            metric(
                "deadlocks",
                "Deadlocks",
                "Bloqueos",
                MetricUnit::Count,
                deadlocks,
            )
            .counter()
            .help("Deadlocks que InnoDB detectó y resolvió revirtiendo una de las transacciones (error 1213). Si aparecen seguido, hay transacciones que toman las mismas filas en distinto orden.")
            .source(deadlocks_src),
        );
        add(
            s,
            metric(
                "row_lock_waits",
                "Esperas de bloqueo de fila",
                "Bloqueos",
                MetricUnit::Count,
                get(&status, "Innodb_row_lock_waits"),
            )
            .counter()
            .help("Veces que una sentencia tuvo que esperar un bloqueo de fila de InnoDB.")
            .source("SHOW GLOBAL STATUS: Innodb_row_lock_waits"),
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
                add(
                    s,
                    metric(
                        "replication_lag",
                        "Retraso de réplica",
                        "Replicación",
                        MetricUnit::Seconds,
                        lag.0,
                    )
                    .help("Cuánto atrasa esta réplica respecto de su origen, en el canal más atrasado. Si crece, la réplica no da abasto con los cambios del origen; no se informa con el hilo SQL detenido.")
                    .source("SHOW REPLICA STATUS: MAX(Seconds_Behind_Source) (older servers: SHOW SLAVE STATUS, Seconds_Behind_Master)"),
                );
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
        add(
            s,
            metric(
                "uptime",
                "Tiempo activo",
                "Servidor",
                MetricUnit::Seconds,
                get(&status, "Uptime"),
            )
            .help("Tiempo desde que arrancó el servidor.")
            .source("SHOW GLOBAL STATUS: Uptime"),
        );

        // Engine extras.
        match v {
            Variant::SingleStore => self.singlestore(s).await,
            Variant::OceanBase => self.oceanbase(s).await,
            _ => {}
        }

        self.sessions(s, Some("information_schema.PROCESSLIST"))
            .await;
        if full {
            self.innodb_locks(s).await;
        } else {
            s.notes.push(format!(
                "{} no expone las esperas de bloqueo por SQL.",
                self.engine_name()
            ));
        }
        if ps {
            self.performance_schema(s).await;
        } else if full {
            s.notes.push(
                "Performance Schema está desactivado: no hay esperas ni estadísticas por sentencia (activalo con performance_schema = ON).".into(),
            );
        }
        self.sizes_from_information_schema(s, None, SIZES_HELP, SIZES_SOURCE)
            .await;
        if full && !self.has_process_privilege().await {
            s.notes
                .push("Sin el privilegio PROCESS solo se ven tus propias sesiones.".into());
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
            &[
                "rol",
                "servidor",
                "canal",
                "E/S",
                "SQL",
                "retraso (s)",
                "último error",
            ],
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
                    let l = named(r, &["Seconds_Behind_Source", "Seconds_Behind_Master"])
                        .and_then(|v| num(&v));
                    if let Some(l) = l {
                        lag = Some(lag.map_or(l, |w| w.max(l)));
                    }
                    let host = named(r, &["Source_Host", "Master_Host"]).unwrap_or_default();
                    let port = named(r, &["Source_Port", "Master_Port"]).unwrap_or_default();
                    let error = [
                        &["Last_IO_Error", "Last_Error"][..],
                        &["Last_SQL_Error"][..],
                    ]
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
                if maria {
                    "REPLICA MONITOR o REPLICATION CLIENT"
                } else {
                    "REPLICATION CLIENT"
                },
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
    /// else (or with `None`) `SHOW FULL PROCESSLIST`. True when they came
    /// from the view.
    async fn sessions(&mut self, s: &mut MonitorSnapshot, view: Option<&str>) -> bool {
        let me = self.conn.id();
        let sql = match view {
            Some(view) => format!("SELECT * FROM {view} WHERE ID <> {me} ORDER BY (COMMAND = 'Sleep'), TIME DESC LIMIT {MAX_ROWS}"),
            None => "SHOW FULL PROCESSLIST".into(),
        };
        let (g, view) = match self.grid(&sql).await {
            Ok(g) => (g, view),
            Err(first) => match self.grid("SHOW FULL PROCESSLIST").await {
                Ok(g) => (g, None),
                Err(_) => {
                    s.notes.push(format!(
                        "No se pudo leer la lista de sesiones ({}).",
                        why(&first)
                    ));
                    return false;
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
            .map(|r| {
                id_col
                    .and_then(|i| r.get(i))
                    .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(num)))
                    != Some(me as f64)
            })
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
                let busy = cmd.and_then(|c| r[c].as_str()).is_none_or(|c| {
                    !matches!(c, "Sleep" | "Daemon" | "Binlog Dump" | "Binlog Dump GTID")
                });
                busy && query.is_some_and(|q| !r[q].is_null())
            })
            .cloned()
            .collect();
        let scope = match self.variant {
            Variant::TiDb if view.is_some() => {
                "Sesiones abiertas en todas las instancias de TiDB del cluster, incluida la del monitor."
            }
            // SHOW FULL PROCESSLIST lists only this instance's.
            Variant::TiDb => "Sesiones abiertas en la instancia de TiDB a la que está conectado DBine, incluida la del monitor.",
            Variant::StarRocks | Variant::Doris => {
                "Conexiones abiertas en el frontend (FE) al que está conectado DBine, incluida la del monitor."
            }
            _ => "Sesiones abiertas en el servidor, incluida la del monitor. Sin el privilegio PROCESS solo cuenta las tuyas.",
        };
        if !s.metrics.iter().any(|m| m.key == "connections") {
            // The list stops at MAX_ROWS; the count doesn't.
            let (count, src) = match view {
                Some(view) => match self.scalar(&format!("SELECT COUNT(*) FROM {view}")).await {
                    Some(n) => (n, format!("{view}: COUNT(*)")),
                    // Plus this one, left out of the table.
                    None => (
                        sessions.rows.len() as f64 + 1.0,
                        format!("{view}: number of rows (up to {MAX_ROWS})"),
                    ),
                },
                None => (
                    g.total as f64,
                    "SHOW FULL PROCESSLIST: number of rows".into(),
                ),
            };
            add(
                s,
                metric(
                    "connections",
                    "Conexiones",
                    "Conexiones",
                    MetricUnit::Count,
                    Some(count),
                )
                .help(scope)
                .source(&src),
            );
        }
        if !s.metrics.iter().any(|m| m.key == "active_sessions") {
            const IDLE: &str = "'Sleep', 'Daemon', 'Binlog Dump', 'Binlog Dump GTID'";
            let counted = match view {
                Some(view) => self
                    .scalar(&format!(
                        "SELECT COUNT(*) FROM {view} WHERE ID <> {me} AND COMMAND NOT IN ({IDLE}) AND INFO IS NOT NULL"
                    ))
                    .await
                    .map(|n| (n, view)),
                None => None,
            };
            let (n, src) = match counted {
                Some((n, view)) => (
                    n,
                    format!("{view}: COUNT(*) WHERE INFO IS NOT NULL AND COMMAND isn't Sleep, Daemon or Binlog Dump, excluding this session"),
                ),
                None => (
                    queries.rows.len() as f64,
                    format!(
                        "{}: rows with a statement whose COMMAND isn't Sleep, Daemon or Binlog Dump, excluding this session (up to {MAX_ROWS})",
                        view.unwrap_or("SHOW FULL PROCESSLIST")
                    ),
                ),
            };
            add(
                s,
                metric(
                    "active_sessions",
                    "Sesiones activas",
                    "Conexiones",
                    MetricUnit::Count,
                    Some(n),
                )
                .help("Sesiones ejecutando una sentencia ahora, sin contar la del monitor.")
                .source(&src),
            );
        }
        s.tables.push(sessions);
        s.tables.push(queries);
        view.is_some()
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
            Ok(g) => s
                .tables
                .push(g.table("waits", "Esperas principales (desde el arranque)")),
            Err(e) => s.notes.push(format!(
                "No se pudieron leer las esperas de Performance Schema ({}).",
                why(&e)
            )),
        }
        let digests = "SELECT SCHEMA_NAME AS base, LEFT(DIGEST_TEXT, 2000) AS consulta, COUNT_STAR AS ejecuciones,
                   ROUND(SUM_TIMER_WAIT / 1e12, 3) AS `tiempo total (s)`, ROUND(AVG_TIMER_WAIT / 1e9, 3) AS `promedio (ms)`,
                   SUM_ROWS_EXAMINED AS `filas examinadas`, SUM_ROWS_SENT AS `filas enviadas`, SUM_ERRORS AS errores
            FROM performance_schema.events_statements_summary_by_digest
            ORDER BY SUM_TIMER_WAIT DESC LIMIT 20";
        match self.grid(digests).await {
            Ok(g) => s
                .tables
                .push(g.table("top_queries", "Consultas más costosas (acumulado)")),
            Err(e) => s.notes.push(format!(
                "No se pudo leer el resumen de sentencias ({}).",
                why(&e)
            )),
        }
    }

    /// `databases` and `top_objects` from information_schema.TABLES, plus
    /// `storage_used`; cached for a minute.
    async fn sizes_from_information_schema(
        &mut self,
        s: &mut MonitorSnapshot,
        max: Option<f64>,
        help: &str,
        source: &str,
    ) {
        let fresh = self
            .sizes
            .as_ref()
            .is_some_and(|z| z.at.elapsed() < SIZES_TTL);
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
            let mut z = Sizes {
                at: Instant::now(),
                total: None,
                tables: Vec::new(),
                notes: Vec::new(),
            };
            match self.grid(&dbs).await {
                Ok(g) => {
                    let total: f64 = g.rows.iter().filter_map(|r| g.num(r, &["total"])).sum();
                    z.total = Some(total);
                    z.tables
                        .push(g.table("databases", "Bases y tamaños (bytes)"));
                }
                Err(e) => z.notes.push(format!(
                    "No se pudieron leer los tamaños de las bases ({}).",
                    why(&e)
                )),
            }
            if let Ok(g) = self.grid(&top).await {
                z.tables
                    .push(g.table("top_objects", "Tablas más grandes (bytes)"));
            }
            self.sizes = Some(z);
        }
        self.push_sizes(s, max, help, source);
    }

    fn push_sizes(&self, s: &mut MonitorSnapshot, max: Option<f64>, help: &str, source: &str) {
        if let Some(z) = &self.sizes {
            // A figure from the nodes (StarRocks, Doris) wins.
            if !s.metrics.iter().any(|m| m.key == "storage_used") {
                add(
                    s,
                    metric(
                        "storage_used",
                        "Espacio usado",
                        "Almacenamiento",
                        MetricUnit::Bytes,
                        z.total,
                    )
                    .max(max)
                    .help(help)
                    .source(source),
                );
            }
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
                    add(
                        s,
                        metric(
                            "mem_used",
                            "Memoria usada",
                            "Memoria",
                            MetricUnit::Bytes,
                            total(&["MEMORY_USED_MB"]),
                        )
                        .max(total(&["MAX_MEMORY_MB"]))
                        .help("Memoria que usan los nodos del cluster, sumada; el máximo es la suma de sus maximum_memory. Cerca del máximo, las consultas que piden más memoria fallan.")
                        .source("information_schema.MV_NODES: SUM(MEMORY_USED_MB) (max: SUM(MAX_MEMORY_MB))"),
                    );
                }
                add(
                    s,
                    metric(
                        "table_memory",
                        "Memoria de tablas",
                        "Memoria",
                        MetricUnit::Bytes,
                        total(&["TABLE_MEMORY_USED_MB"]),
                    )
                    .max(total(&["MAX_TABLE_MEMORY_MB"]))
                    .help("Memoria que ocupan las tablas en memoria (rowstore) y sus índices en todos los nodos; el máximo es la suma de maximum_table_memory. Al llegar, las escrituras que necesitan más memoria fallan.")
                    .source("information_schema.MV_NODES: SUM(TABLE_MEMORY_USED_MB) (max: SUM(MAX_TABLE_MEMORY_MB))"),
                );
                if let (Some(disk), Some(free)) = (
                    total(&["TOTAL_DATA_DISK_MB"]),
                    total(&["AVAILABLE_DATA_DISK_MB"]),
                ) {
                    add(
                        s,
                        metric(
                            "disk_used",
                            "Disco de datos usado",
                            "Almacenamiento",
                            MetricUnit::Bytes,
                            Some(disk - free),
                        )
                        .max(Some(disk))
                        .help("Espacio ocupado en los discos de datos de todos los nodos (el total menos el libre, incluido lo que no es de SingleStore); el máximo es su tamaño.")
                        .source("information_schema.MV_NODES: SUM(TOTAL_DATA_DISK_MB − AVAILABLE_DATA_DISK_MB) (max: SUM(TOTAL_DATA_DISK_MB))"),
                    );
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
            Err(e) => s
                .notes
                .push(format!("No se pudo leer MV_NODES ({}).", why(&e))),
        }
        s.notes.push("SingleStore no expone el uso de CPU por SQL; miralo en SingleStore Studio o en el monitoreo del cluster.".into());
    }

    async fn oceanbase(&mut self, s: &mut MonitorSnapshot) {
        let names = [
            "active sessions",
            "sql select count",
            "sql insert count",
            "sql update count",
            "sql delete count",
            "sql replace count",
            "trans commit count",
            "trans rollback count",
            "io read bytes",
            "io write bytes",
            "memory usage",
            "rpc packet in bytes",
            "rpc packet out bytes",
        ];
        let list = names.iter().map(|n| lit(n)).collect::<Vec<_>>().join(", ");
        let sql = format!("SELECT NAME, SUM(VALUE) FROM oceanbase.GV$SYSSTAT WHERE NAME IN ({list}) GROUP BY NAME");
        match self.kv(&sql).await {
            Some(st) if !st.is_empty() => {
                let replace = |s: &mut MonitorSnapshot, m: Metric| {
                    s.metrics.retain(|x| x.key != m.key);
                    add(s, m);
                };
                const SYSSTAT: &str = "oceanbase.GV$SYSSTAT: SUM(VALUE)";
                replace(
                    s,
                    metric("active_sessions", "Sesiones activas", "Conexiones", MetricUnit::Count, get(&st, "active sessions"))
                        .help("Sesiones ejecutando algo ahora, en todos los OBServer y en los tenants que ve este usuario.")
                        .source(&format!("{SYSSTAT} WHERE NAME = 'active sessions'")),
                );
                replace(
                    s,
                    metric(
                        "queries",
                        "Consultas",
                        "Actividad",
                        MetricUnit::Count,
                        sum(&st, &["sql select count", "sql insert count", "sql update count", "sql delete count", "sql replace count"]),
                    )
                    .counter()
                    .help("Sentencias SELECT, INSERT, UPDATE, DELETE y REPLACE ejecutadas en todos los OBServer, en los tenants que ve este usuario.")
                    .source(&format!("{SYSSTAT} of 'sql select count', 'sql insert count', 'sql update count', 'sql delete count', 'sql replace count'")),
                );
                replace(
                    s,
                    metric("transactions", "Transacciones", "Actividad", MetricUnit::Count, sum(&st, &["trans commit count", "trans rollback count"]))
                        .counter()
                        .help("Transacciones confirmadas y revertidas en todos los OBServer, en los tenants que ve este usuario.")
                        .source(&format!("{SYSSTAT} of 'trans commit count', 'trans rollback count'")),
                );
                replace(
                    s,
                    metric("mem_used", "Memoria usada", "Memoria", MetricUnit::Bytes, get(&st, "memory usage"))
                        .help("Memoria en uso de los tenants que ve este usuario, sumada en todos los OBServer.")
                        .source(&format!("{SYSSTAT} WHERE NAME = 'memory usage'")),
                );
                replace(
                    s,
                    metric("disk_read", "Lectura en disco", "Disco", MetricUnit::Bytes, get(&st, "io read bytes"))
                        .counter()
                        .help("Bytes que los OBServer leyeron del disco para los tenants que ve este usuario.")
                        .source(&format!("{SYSSTAT} WHERE NAME = 'io read bytes'")),
                );
                replace(
                    s,
                    metric("disk_write", "Escritura en disco", "Disco", MetricUnit::Bytes, get(&st, "io write bytes"))
                        .counter()
                        .help("Bytes que los OBServer escribieron en disco para los tenants que ve este usuario.")
                        .source(&format!("{SYSSTAT} WHERE NAME = 'io write bytes'")),
                );
                replace(
                    s,
                    metric("net_in", "Red entrante (RPC)", "Red", MetricUnit::Bytes, get(&st, "rpc packet in bytes"))
                        .counter()
                        .help("Bytes que los OBServer recibieron por RPC, la red interna entre los nodos del cluster; no es el tráfico de los clientes.")
                        .source(&format!("{SYSSTAT} WHERE NAME = 'rpc packet in bytes'")),
                );
                replace(
                    s,
                    metric("net_out", "Red saliente (RPC)", "Red", MetricUnit::Bytes, get(&st, "rpc packet out bytes"))
                        .counter()
                        .help("Bytes que los OBServer enviaron por RPC, la red interna entre los nodos del cluster; no es el tráfico de los clientes.")
                        .source(&format!("{SYSSTAT} WHERE NAME = 'rpc packet out bytes'")),
                );
            }
            _ => s.notes.push("No se pudo leer oceanbase.GV$SYSSTAT: los contadores de actividad requieren OceanBase 4.x.".into()),
        }
        match self.grid("SELECT * FROM oceanbase.GV$OB_SERVERS").await {
            Ok(g) => {
                let total = |names: &[&str]| -> Option<f64> {
                    let v: Vec<f64> = g.rows.iter().filter_map(|r| g.num(r, names)).collect();
                    (!v.is_empty()).then(|| v.iter().sum())
                };
                if let (Some(used), cap) =
                    (total(&["DATA_DISK_IN_USE"]), total(&["DATA_DISK_CAPACITY"]))
                {
                    add(
                        s,
                        metric(
                            "disk_used",
                            "Disco de datos usado",
                            "Almacenamiento",
                            MetricUnit::Bytes,
                            Some(used),
                        )
                        .max(cap)
                        .help("Espacio de datos en uso en todos los OBServer; el máximo es la capacidad de disco de datos que tienen asignada.")
                        .source("oceanbase.GV$OB_SERVERS: SUM(DATA_DISK_IN_USE) (max: SUM(DATA_DISK_CAPACITY))"),
                    );
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
            Err(e) => s
                .notes
                .push(format!("No se pudo leer GV$OB_SERVERS ({}).", why(&e))),
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
        info(
            s,
            "Zona horaria",
            vars.get("time_zone")
                .filter(|t| !t.eq_ignore_ascii_case("SYSTEM"))
                .or(vars.get("system_time_zone"))
                .cloned(),
        );
        info(
            s,
            "Límite de memoria del servidor",
            vars.get("tidb_server_memory_limit").cloned(),
        );

        // Host load per instance: CPU idle and memory.
        let load = self
            .rows(
                "SELECT TYPE, INSTANCE, DEVICE_TYPE, NAME, VALUE FROM information_schema.CLUSTER_LOAD
                 WHERE (DEVICE_TYPE = 'cpu' AND DEVICE_NAME = 'usage' AND NAME = 'idle')
                    OR (DEVICE_TYPE = 'memory' AND DEVICE_NAME = 'virtual' AND NAME IN ('total', 'used'))",
            )
            .await;
        // instance → (cpu %, mem used, mem total)
        type Triple = (Option<f64>, Option<f64>, Option<f64>);
        let mut per: HashMap<String, Triple> = HashMap::new();
        match load {
            Ok(rows) => {
                for r in &rows {
                    let (Some(inst), Some(dev), Some(name), Some(val)) =
                        (at(r, 1), at(r, 2), at(r, 3), at(r, 4).and_then(|v| num(&v)))
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
            Err(e) => s
                .notes
                .push(format!("No se pudo leer CLUSTER_LOAD ({}).", why(&e))),
        }
        // Hosts may run several instances: memory counts once per host.
        let mut hosts: HashMap<String, (f64, f64)> = HashMap::new();
        for (inst, (_, used, total)) in &per {
            if let (Some(u), Some(t)) = (used, total) {
                let host = inst
                    .rsplit_once(':')
                    .map_or(inst.as_str(), |(h, _)| h)
                    .to_string();
                hosts.insert(host, (*u, *t));
            }
        }
        let cpus: Vec<f64> = per.values().filter_map(|p| p.0).collect();
        let cpu = (!cpus.is_empty()).then(|| cpus.iter().sum::<f64>() / cpus.len() as f64);
        add(
            s,
            metric(
                "cpu",
                "CPU del cluster (promedio)",
                "CPU",
                MetricUnit::Percent,
                cpu,
            )
            .max(Some(100.0))
            .help("Uso de CPU de los hosts del cluster (TiDB, TiKV, PD…), promedio entre las instancias: 100 % equivale a todos los núcleos ocupados. Lo informa el sistema operativo de cada host, así que incluye todo lo que corre en ellos.")
            .source("information_schema.CLUSTER_LOAD: 1 − VALUE WHERE DEVICE_TYPE = 'cpu' AND DEVICE_NAME = 'usage' AND NAME = 'idle', average of the instances"),
        );
        if !hosts.is_empty() {
            let used = hosts.values().map(|h| h.0).sum::<f64>();
            let total = hosts.values().map(|h| h.1).sum::<f64>();
            add(
                s,
                metric(
                    "host_mem",
                    "Memoria de los hosts",
                    "Memoria",
                    MetricUnit::Bytes,
                    Some(used),
                )
                .max(Some(total))
                .help("Memoria en uso en los hosts del cluster, de todos los procesos, contando cada host una vez; el máximo es su memoria total.")
                .source("information_schema.CLUSTER_LOAD: memory 'virtual' used (max: total), once per host"),
            );
        }
        if let Ok(g) = self
            .grid("SELECT * FROM information_schema.MEMORY_USAGE")
            .await
        {
            if let Some(r) = g.rows.first() {
                let limit = g
                    .num(r, &["MEMORY_LIMIT"])
                    .filter(|l| *l > 0.0)
                    .or_else(|| g.num(r, &["MEMORY_TOTAL"]));
                add(
                    s,
                    metric(
                        "mem_used",
                        "Memoria de TiDB",
                        "Memoria",
                        MetricUnit::Bytes,
                        g.num(r, &["MEMORY_CURRENT"]),
                    )
                    .max(limit)
                    .help("Memoria que usa la instancia de TiDB a la que está conectado DBine. El máximo es tidb_server_memory_limit (o la memoria total, sin límite); cerca de él, TiDB cancela la consulta que más memoria usa.")
                    .source("information_schema.MEMORY_USAGE: MEMORY_CURRENT (max: MEMORY_LIMIT, else MEMORY_TOTAL)"),
                );
            }
        }

        // Nodes.
        let mut tidb_nodes = None;
        match self
            .grid("SELECT * FROM information_schema.CLUSTER_INFO")
            .await
        {
            Ok(g) => {
                tidb_nodes = Some(
                    g.rows
                        .iter()
                        .filter(|r| {
                            g.text(r, &["TYPE"])
                                .is_some_and(|t| t.eq_ignore_ascii_case("tidb"))
                        })
                        .count()
                        .max(1) as f64,
                );
                let mut t = MonitorTable::new(
                    "nodes",
                    "Nodos del cluster",
                    &[
                        "tipo",
                        "instancia",
                        "versión",
                        "inicio",
                        "tiempo activo",
                        "CPU (%)",
                        "memoria usada",
                        "memoria total",
                    ],
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
            Err(e) => s
                .notes
                .push(format!("No se pudo leer CLUSTER_INFO ({}).", why(&e))),
        }

        let max_conn = get(&vars, "max_connections").filter(|m| *m > 0.0);
        let cluster = self
            .sessions(s, Some("information_schema.CLUSTER_PROCESSLIST"))
            .await;
        // max_connections is per TiDB instance: the cluster's count goes
        // against all of them, SHOW FULL PROCESSLIST's against this one.
        if let Some(c) = s.metrics.iter_mut().find(|m| m.key == "connections") {
            let (nodes, why) = if cluster {
                (
                    tidb_nodes.unwrap_or(1.0),
                    " × TiDB instances in CLUSTER_INFO",
                )
            } else {
                (1.0, ", this instance's")
            };
            c.max = max_conn.map(|m| m * nodes);
            if c.max.is_some() {
                c.source = c
                    .source
                    .take()
                    .map(|src| format!("{src} (max: max_connections{why})"));
            }
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
                // The list stops at 200; the count doesn't.
                let (n, src) = match self
                    .scalar("SELECT COUNT(*) FROM information_schema.DATA_LOCK_WAITS")
                    .await
                {
                    Some(n) => (n, "information_schema.DATA_LOCK_WAITS: COUNT(*)"),
                    None => (
                        g.rows.len() as f64,
                        "information_schema.DATA_LOCK_WAITS: number of rows (up to 200)",
                    ),
                };
                add(
                    s,
                    metric("locks_waiting", "Bloqueos en espera", "Bloqueos", MetricUnit::Count, Some(n))
                        .help("Transacciones pesimistas esperando ahora un bloqueo de fila que tiene otra transacción.")
                        .source(src),
                );
                s.tables.push(g.table("locks", "Bloqueos en espera"));
            }
            Err(e) => s.notes.push(format!("No se pudieron leer las esperas de bloqueo ({}).", why(&e))),
        }
        let deadlocks = self
            .scalar("SELECT COUNT(DISTINCT DEADLOCK_ID) FROM information_schema.CLUSTER_DEADLOCKS")
            .await;
        add(
            s,
            metric(
                "recent_deadlocks",
                "Deadlocks recientes",
                "Bloqueos",
                MetricUnit::Count,
                deadlocks,
            )
            .help("Deadlocks que guarda el historial de las instancias de TiDB (los últimos tidb_deadlock_history_capacity de cada una, 10 por omisión). No es un contador desde el arranque: muestra cuántos quedan en el historial.")
            .source("information_schema.CLUSTER_DEADLOCKS: COUNT(DISTINCT DEADLOCK_ID)"),
        );

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
        let stores = self
            .grid("SELECT CAPACITY, AVAILABLE FROM information_schema.TIKV_STORE_STATUS")
            .await
            .ok();
        let capacity = stores.and_then(|g| {
            let v: Vec<f64> = g
                .rows
                .iter()
                .filter_map(|r| g.text(r, &["CAPACITY"]).and_then(|c| parse_size(&c)))
                .collect();
            (!v.is_empty() && v.iter().sum::<f64>() > 0.0).then(|| v.iter().sum())
        });
        self.sizes_from_information_schema(
            s,
            capacity,
            "Datos e índices de todas las tablas según information_schema, sin las bases de sistema; se actualiza cada minuto. TiDB los estima con las estadísticas de las tablas. El máximo es la capacidad de los nodos TiKV.",
            "information_schema.TABLES: SUM(DATA_LENGTH + INDEX_LENGTH), refreshed every minute (max: TIKV_STORE_STATUS SUM(CAPACITY))",
        )
        .await;
        add(
            s,
            metric(
                "uptime",
                "Tiempo activo",
                "Servidor",
                MetricUnit::Seconds,
                get(&status, "Uptime"),
            )
            .help("Tiempo desde que arrancó la instancia de TiDB a la que está conectado DBine.")
            .source("SHOW GLOBAL STATUS: Uptime"),
        );
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
                info(
                    s,
                    &format!("Versión de {name}"),
                    g.rows.first().and_then(|r| g.text(r, &["Version"])),
                );
                if let Some(r) = g
                    .rows
                    .iter()
                    .find(|r| g.text(r, &["IsMaster"]).as_deref() == Some("true"))
                {
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
            Err(e) => s.notes.push(format!(
                "SHOW FRONTENDS requiere privilegios de administrador (OPERATE o NODE) ({}).",
                why(&e)
            )),
        }
        let backends = match self.grid("SHOW BACKENDS").await {
            Ok(g) => Some((g, "SHOW BACKENDS")),
            // Shared-data StarRocks runs compute nodes instead of backends.
            Err(e) => match self.grid("SHOW COMPUTE NODES").await {
                Ok(g) if !g.rows.is_empty() => Some((g, "SHOW COMPUTE NODES")),
                _ => {
                    s.notes.push(format!("SHOW BACKENDS requiere privilegios de administrador (OPERATE o NODE) ({}).", why(&e)));
                    None
                }
            },
        };
        if let Some((g, show)) = backends {
            let col_sum = |names: &[&str], f: &dyn Fn(&str) -> Option<f64>| -> Option<f64> {
                let v: Vec<f64> = g
                    .rows
                    .iter()
                    .filter_map(|r| g.text(r, names).and_then(|t| f(&t)))
                    .collect();
                (!v.is_empty()).then(|| v.iter().sum())
            };
            let cpus: Vec<f64> = g
                .rows
                .iter()
                .filter_map(|r| g.text(r, &["CpuUsedPct"]).and_then(|t| num(&t)))
                .collect();
            let cpu = (!cpus.is_empty()).then(|| cpus.iter().sum::<f64>() / cpus.len() as f64);
            add(
                s,
                metric(
                    "cpu",
                    "CPU de los backends (promedio)",
                    "CPU",
                    MetricUnit::Percent,
                    cpu,
                )
                .max(Some(100.0))
                .help("Uso de CPU que informa cada backend (o nodo de cómputo), promedio entre ellos: 100 % equivale a todos sus núcleos ocupados. Alto y sostenido suele venir de consultas que leen muchos datos.")
                .source(&format!("{show}: CpuUsedPct, average of the nodes")),
            );
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
                    metric(
                        "mem_used",
                        "Memoria de los backends",
                        "Memoria",
                        MetricUnit::Bytes,
                        Some(mem.iter().map(|m| m.0).sum()),
                    )
                    .max(Some(mem.iter().map(|m| m.1).sum()))
                    .help("Memoria que usan los procesos de los backends, sumada; el máximo es la suma de sus límites de memoria (mem_limit). Cerca del límite, las consultas que piden más memoria fallan.")
                    .source(&format!("{show}: SUM(MemUsedPct × MemLimit) (max: SUM(MemLimit))")),
                );
            }
            let running = col_sum(&["NumRunningQueries"], &|t| num(t));
            add(
                s,
                metric(
                    "running_queries",
                    "Consultas en ejecución",
                    "Actividad",
                    MetricUnit::Count,
                    running,
                )
                .help("Fragmentos de consultas ejecutándose ahora en los backends, sumados: una consulta distribuida cuenta en cada backend donde corre.")
                .source(&format!("{show}: SUM(NumRunningQueries)")),
            );
            let used = col_sum(&["DataUsedCapacity"], &parse_size);
            // A backend that hasn't reported its disks says 0.
            let total = col_sum(&["TotalCapacity"], &parse_size).filter(|t| *t > 0.0);
            add(
                s,
                metric(
                    "storage_used",
                    "Espacio usado",
                    "Almacenamiento",
                    MetricUnit::Bytes,
                    used,
                )
                .max(total)
                .help("Espacio que ocupan los datos en los discos de los backends, réplicas incluidas; el máximo es la capacidad total de esos discos.")
                .source(&format!("{show}: SUM(DataUsedCapacity) (max: SUM(TotalCapacity))")),
            );
            add(
                s,
                metric(
                    "tablets",
                    "Tablets",
                    "Almacenamiento",
                    MetricUnit::Count,
                    col_sum(&["TabletNum"], &|t| num(t)),
                )
                .help("Tablets (las porciones en que se reparten los datos) que guardan los backends, réplicas incluidas. Muchos tablets chicos cargan la memoria y los metadatos de los nodos.")
                .source(&format!("{show}: SUM(TabletNum)")),
            );
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
            Err(_) => {
                self.sizes_from_information_schema(s, None, SIZES_HELP, SIZES_SOURCE)
                    .await
            }
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
        match self
            .grid(&format!("SELECT * FROM system.processes LIMIT {MAX_ROWS}"))
            .await
        {
            Ok(g) => {
                let mem: f64 = g
                    .rows
                    .iter()
                    .filter_map(|r| g.num(r, &["memory_usage"]))
                    .sum();
                let active = g
                    .rows
                    .iter()
                    .filter(|r| {
                        g.text(r, &["command"])
                            .is_some_and(|c| !c.eq_ignore_ascii_case("sleep"))
                            && g.text(r, &["extra_info"]).is_some_and(|q| !q.is_empty())
                    })
                    .count();
                // The list stops at MAX_ROWS; the server's own figures don't.
                let counted = self
                    .grid(
                        "SELECT COUNT(*) AS n,
                                SUM(CASE WHEN LOWER(command) <> 'sleep' AND COALESCE(extra_info, '') <> '' THEN 1 ELSE 0 END) AS busy,
                                SUM(memory_usage) AS mem
                           FROM system.processes",
                    )
                    .await
                    .ok()
                    .and_then(|a| {
                        let r = a.rows.first()?;
                        Some((
                            a.num(r, &["n"])?,
                            a.num(r, &["busy"]).unwrap_or(0.0),
                            a.num(r, &["mem"]).unwrap_or(0.0),
                        ))
                    });
                let (n, active, mem, srcs) = match counted {
                    Some((n, busy, m)) => (
                        n,
                        busy,
                        m,
                        [
                            "system.processes: COUNT(*)".to_string(),
                            "system.processes: COUNT(*) WHERE command <> 'Sleep' AND extra_info <> ''".to_string(),
                            "system.processes: SUM(memory_usage)".to_string(),
                        ],
                    ),
                    None => (
                        g.rows.len() as f64,
                        active as f64,
                        mem,
                        [
                            format!("system.processes: number of rows (up to {MAX_ROWS})"),
                            format!("system.processes: rows with command <> 'Sleep' and extra_info (up to {MAX_ROWS})"),
                            format!("system.processes: SUM(memory_usage) (up to {MAX_ROWS} sessions)"),
                        ],
                    ),
                };
                add(
                    s,
                    metric(
                        "connections",
                        "Sesiones",
                        "Conexiones",
                        MetricUnit::Count,
                        Some(n),
                    )
                    .help("Sesiones abiertas en el nodo de consultas al que está conectado DBine, incluida la del monitor.")
                    .source(&srcs[0]),
                );
                add(
                    s,
                    metric(
                        "active_sessions",
                        "Sesiones activas",
                        "Conexiones",
                        MetricUnit::Count,
                        Some(active),
                    )
                    .help("Sesiones del nodo de consultas al que está conectado DBine ejecutando una consulta ahora, incluida la del monitor.")
                    .source(&srcs[1]),
                );
                add(
                    s,
                    metric(
                        "mem_used",
                        "Memoria de las consultas",
                        "Memoria",
                        MetricUnit::Bytes,
                        Some(mem),
                    )
                    .help("Memoria que usan ahora las sesiones del nodo de consultas al que está conectado DBine, sumada; no incluye las cachés ni el resto del proceso.")
                    .source(&srcs[2]),
                );
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
            Err(e) => s
                .notes
                .push(format!("No se pudo leer system.processes ({}).", why(&e))),
        }
        match self.grid("SELECT * FROM system.clusters").await {
            Ok(g) => {
                info(s, "Nodos", Some(g.rows.len().to_string()));
                s.tables.push(g.table("nodes", "Nodos del cluster"));
            }
            Err(e) => s
                .notes
                .push(format!("No se pudo leer system.clusters ({}).", why(&e))),
        }
        let fresh = self
            .sizes
            .as_ref()
            .is_some_and(|z| z.at.elapsed() < SIZES_TTL);
        if !fresh {
            let mut z = Sizes {
                at: Instant::now(),
                total: None,
                tables: Vec::new(),
                notes: Vec::new(),
            };
            let dbs = "SELECT database AS base, COUNT(*) AS tablas, SUM(num_rows) AS filas, SUM(data_size) AS datos,
                              SUM(data_compressed_size) AS comprimido, SUM(index_size) AS `índices`
                       FROM system.tables WHERE database NOT IN ('system', 'information_schema')
                       GROUP BY database ORDER BY comprimido DESC LIMIT 200";
            match self.grid(dbs).await {
                Ok(g) => {
                    z.total = Some(
                        g.rows
                            .iter()
                            .map(|r| {
                                g.num(r, &["comprimido"]).unwrap_or(0.0)
                                    + g.num(r, &["índices"]).unwrap_or(0.0)
                            })
                            .sum(),
                    );
                    z.tables
                        .push(g.table("databases", "Bases y tamaños (bytes)"));
                }
                Err(e) => z
                    .notes
                    .push(format!("No se pudieron leer los tamaños ({}).", why(&e))),
            }
            let top = "SELECT CONCAT(database, '.', name) AS objeto, engine AS motor, num_rows AS filas, data_size AS datos,
                              data_compressed_size AS comprimido, index_size AS `índices`
                       FROM system.tables WHERE database NOT IN ('system', 'information_schema')
                       ORDER BY data_compressed_size DESC LIMIT 20";
            if let Ok(g) = self.grid(top).await {
                z.tables
                    .push(g.table("top_objects", "Tablas más grandes (bytes)"));
            }
            self.sizes = Some(z);
        }
        self.push_sizes(
            s,
            None,
            "Tamaño comprimido de los datos y los índices de todas las tablas, sin system ni information_schema; se actualiza cada minuto.",
            "system.tables: SUM(data_compressed_size + index_size), refreshed every minute",
        );
        s.notes.push("Databend no expone el uso de CPU ni los contadores de consultas por SQL; están en /metrics de cada nodo (Prometheus).".into());
    }

    // ------------------------------------------------------------ Manticore

    async fn manticore(&mut self, s: &mut MonitorSnapshot) {
        let st = self.kv("SHOW STATUS").await.unwrap_or_default();
        for m in manticore_metrics(&st) {
            add(s, m);
        }
        // query_cpu is "OFF" unless searchd runs with --cpustats.
        if get(&st, "query_cpu").is_none() {
            s.notes.push(
                "El tiempo de CPU de las búsquedas requiere arrancar searchd con --cpustats."
                    .into(),
            );
        } else if get(&st, "workers_total").filter(|w| *w > 0.0).is_none() {
            s.notes.push(
                "Manticore no informó cuántos workers tiene (workers_total): sin ese dato no se muestra la CPU de las búsquedas."
                    .into(),
            );
        }
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
                t.rows
                    .retain(|r| info_col.is_none_or(|i| r[i].as_str() != Some("SHOW THREADS")));
                s.tables.push(t);
            }
            Err(e) => s
                .notes
                .push(format!("No se pudo leer SHOW THREADS ({}).", why(&e))),
        }

        let fresh = self
            .sizes
            .as_ref()
            .is_some_and(|z| z.at.elapsed() < SIZES_TTL);
        if !fresh {
            let mut z = Sizes {
                at: Instant::now(),
                total: None,
                tables: Vec::new(),
                notes: Vec::new(),
            };
            let names: Vec<String> = self
                .optional_rows("SHOW TABLES")
                .await
                .iter()
                .filter_map(|r| at(r, 0))
                .take(MAX_ROWS)
                .collect();
            let mut t = MonitorTable::new(
                "databases",
                "Tablas y tamaños",
                &["tabla", "documentos", "RAM", "disco", "bytes indexados"],
            );
            let mut total = 0.0;
            for n in names {
                let q = dbine_driver::sql::quote_ident(dbine_driver::sql::Quote::Backtick, &n);
                let Some(kv) = self.kv(&format!("SHOW TABLE {q} STATUS")).await else {
                    continue;
                };
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
            t.rows.sort_by(|a, b| {
                b[3].as_f64()
                    .unwrap_or(0.0)
                    .total_cmp(&a[3].as_f64().unwrap_or(0.0))
            });
            z.total = Some(total);
            z.tables.push(t);
            self.sizes = Some(z);
        }
        self.push_sizes(
            s,
            None,
            "Espacio en disco de todas las tablas, actualizado cada minuto.",
            "SHOW TABLE <table> STATUS: SUM(disk_bytes), refreshed every minute",
        );
        s.notes
            .push("Manticore no expone el uso de CPU ni de memoria del proceso por SQL.".into());
    }

    // ----------------------------------------------------------- GreptimeDB

    async fn greptime(&mut self, s: &mut MonitorSnapshot) {
        // The cores of the nodes that run the queries on the regions.
        let mut cores = None;
        match self
            .grid("SELECT * FROM information_schema.cluster_info")
            .await
        {
            Ok(g) => {
                let total = |names: &[&str]| -> Option<f64> {
                    let v: Vec<f64> = g.rows.iter().filter_map(|r| g.num(r, names)).collect();
                    (!v.is_empty()).then(|| v.iter().sum())
                };
                let data_nodes: f64 = g
                    .rows
                    .iter()
                    .filter(|r| {
                        g.text(r, &["peer_type"]).is_some_and(|t| {
                            t.eq_ignore_ascii_case("DATANODE")
                                || t.eq_ignore_ascii_case("STANDALONE")
                        })
                    })
                    .filter_map(|r| g.num(r, &["total_cpu_millicores"]))
                    .sum();
                cores = Some(data_nodes)
                    .filter(|m| *m > 0.0)
                    .or_else(|| total(&["total_cpu_millicores"]))
                    .map(|m| m / 1000.0)
                    .filter(|n| *n > 0.0);
                let cpu = match (
                    total(&["cpu_usage_millicores"]),
                    total(&["total_cpu_millicores"]),
                ) {
                    (Some(u), Some(t)) if t > 0.0 => Some((u / t * 100.0).clamp(0.0, 100.0)),
                    _ => None,
                };
                add(
                    s,
                    metric("cpu", "CPU del servidor", "CPU", MetricUnit::Percent, cpu)
                        .max(Some(100.0))
                        .help("Uso de CPU de los nodos de GreptimeDB (frontends, datanodes, metasrv), sumado: 100 % equivale a todos los núcleos que tienen ocupados.")
                        .source("information_schema.cluster_info: SUM(cpu_usage_millicores) / SUM(total_cpu_millicores)"),
                );
                add(
                    s,
                    metric(
                        "mem_used",
                        "Memoria usada",
                        "Memoria",
                        MetricUnit::Bytes,
                        total(&["memory_usage_bytes"]),
                    )
                    .max(total(&["total_memory_bytes"]))
                    .help("Memoria que usan los nodos de GreptimeDB, sumada; el máximo es la memoria total que tienen.")
                    .source("information_schema.cluster_info: SUM(memory_usage_bytes) (max: SUM(total_memory_bytes))"),
                );
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
            Err(e) => s.notes.push(format!(
                "No se pudo leer information_schema.cluster_info ({}).",
                why(&e)
            )),
        }
        match self
            .grid("SELECT * FROM information_schema.process_list")
            .await
        {
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
                t.rows.retain(|r| {
                    q.is_none_or(|i| {
                        !r[i]
                            .as_str()
                            .is_some_and(|s| s.contains("information_schema.process_list"))
                    })
                });
                // The table stops at MAX_ROWS; the count doesn't (and leaves
                // out the monitor's queries, this one included).
                let (n, src) = match self
                    .scalar(
                        "SELECT COUNT(*) FROM information_schema.process_list
                          WHERE query NOT LIKE '%information_schema.process_list%'",
                    )
                    .await
                {
                    Some(n) => (
                        n,
                        "information_schema.process_list: COUNT(*) of the queries that don't read process_list"
                            .to_string(),
                    ),
                    None => (
                        t.rows.len() as f64,
                        format!("information_schema.process_list: number of rows, excluding this query (up to {MAX_ROWS})"),
                    ),
                };
                add(
                    s,
                    metric(
                        "active_sessions",
                        "Consultas en curso",
                        "Conexiones",
                        MetricUnit::Count,
                        Some(n),
                    )
                    .help("Consultas ejecutándose ahora en el cluster, sin contar la del monitor.")
                    .source(&src),
                );
                s.tables.push(t);
            }
            Err(e) => s
                .notes
                .push(format!("No se pudo leer process_list ({}).", why(&e))),
        }
        // Regions: live totals, and the sizes per table.
        let regions = "SELECT SUM(disk_size), SUM(memtable_size), SUM(written_bytes_since_open),
                              SUM(query_cpu_time_millis), SUM(query_scanned_bytes), SUM(region_rows), COUNT(*)
                       FROM information_schema.region_statistics";
        match self.rows(regions).await {
            Ok(rows) => {
                let r = rows.first();
                let n = |i: usize| r.and_then(|r| at(r, i)).and_then(|v| num(&v));
                const REGIONS: &str = "information_schema.region_statistics";
                add(
                    s,
                    metric(
                        "storage_used",
                        "Espacio usado",
                        "Almacenamiento",
                        MetricUnit::Bytes,
                        n(0),
                    )
                    .help("Espacio que ocupan en disco las regiones de todas las tablas.")
                    .source(&format!("{REGIONS}: SUM(disk_size)")),
                );
                add(
                    s,
                    metric("mem_cache", "Memtables", "Memoria", MetricUnit::Bytes, n(1))
                        .help("Datos recientes que todavía están en memoria (memtables), antes de volcarse a archivos SST.")
                        .source(&format!("{REGIONS}: SUM(memtable_size)")),
                );
                add(
                    s,
                    metric(
                        "bytes_written",
                        "Bytes escritos",
                        "Actividad",
                        MetricUnit::Bytes,
                        n(2),
                    )
                    .counter()
                    .help("Bytes escritos en las regiones, contados desde que se abrió cada una; una región que se vuelve a abrir empieza de cero.")
                    .source(&format!("{REGIONS}: SUM(written_bytes_since_open)")),
                );
                // Milliseconds of CPU over the cores of the nodes that run
                // them: the rate is the share of all of them (100 = every
                // core busy), never "500 %" for five. Unknown cores: no value.
                add(
                    s,
                    metric(
                        "cpu_time",
                        "CPU de las consultas",
                        "CPU",
                        MetricUnit::Percent,
                        n(3).zip(cores).map(|(ms, c)| ms / 10.0 / c),
                    )
                    .max(Some(100.0))
                    .counter()
                    .help("Tiempo de CPU que usaron las consultas sobre las regiones, como parte de los núcleos de los nodos de datos (o del servidor standalone): 100 % equivale a todos ocupados con consultas.")
                    .source(&format!("{REGIONS}: SUM(query_cpu_time_millis), ÷ cluster_info SUM(total_cpu_millicores) of the DATANODE or STANDALONE nodes")),
                );
                add(
                    s,
                    metric(
                        "disk_read",
                        "Bytes escaneados",
                        "Disco",
                        MetricUnit::Bytes,
                        n(4),
                    )
                    .counter()
                    .help("Bytes que leyeron las consultas de las regiones, de memoria o de disco.")
                    .source(&format!("{REGIONS}: SUM(query_scanned_bytes)")),
                );
                add(
                    s,
                    metric(
                        "rows",
                        "Filas almacenadas",
                        "Almacenamiento",
                        MetricUnit::Count,
                        n(5),
                    )
                    .help("Filas guardadas en todas las regiones.")
                    .source(&format!("{REGIONS}: SUM(region_rows)")),
                );
                add(
                    s,
                    metric(
                        "regions",
                        "Regiones",
                        "Almacenamiento",
                        MetricUnit::Count,
                        n(6),
                    )
                    .help("Regiones (las particiones de las tablas) que hay en el cluster.")
                    .source(&format!("{REGIONS}: COUNT(*)")),
                );
                if n(3).is_some() && cores.is_none() {
                    s.notes.push("GreptimeDB no informó cuántos núcleos tienen sus nodos (cluster_info): sin ese dato no se muestra la CPU de las consultas.".into());
                }
            }
            Err(e) => s
                .notes
                .push(format!("No se pudo leer region_statistics ({}).", why(&e))),
        }
        let fresh = self
            .sizes
            .as_ref()
            .is_some_and(|z| z.at.elapsed() < SIZES_TTL);
        if !fresh {
            let mut z = Sizes {
                at: Instant::now(),
                total: None,
                tables: Vec::new(),
                notes: Vec::new(),
            };
            let dbs = "SELECT t.table_schema AS base, COUNT(DISTINCT t.table_id) AS tablas, SUM(r.region_rows) AS filas,
                              SUM(r.disk_size) AS disco, SUM(r.memtable_size) AS memtables, SUM(r.sst_size) AS sst,
                              SUM(r.index_size) AS `índices`
                       FROM information_schema.region_statistics r
                       JOIN information_schema.tables t ON t.table_id = r.table_id
                       GROUP BY t.table_schema ORDER BY disco DESC LIMIT 200";
            match self.grid(dbs).await {
                Ok(g) => z
                    .tables
                    .push(g.table("databases", "Bases y tamaños (bytes)")),
                Err(e) => z.notes.push(format!(
                    "No se pudieron leer los tamaños por base ({}).",
                    why(&e)
                )),
            }
            let top = "SELECT CONCAT(t.table_schema, '.', t.table_name) AS objeto, COUNT(*) AS regiones,
                              SUM(r.region_rows) AS filas, SUM(r.disk_size) AS disco, SUM(r.memtable_size) AS memtables,
                              SUM(r.sst_num) AS `archivos SST`
                       FROM information_schema.region_statistics r
                       JOIN information_schema.tables t ON t.table_id = r.table_id
                       GROUP BY t.table_schema, t.table_name ORDER BY disco DESC LIMIT 20";
            if let Ok(g) = self.grid(top).await {
                z.tables
                    .push(g.table("top_objects", "Tablas más grandes (bytes)"));
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
        let kv: Kv = [
            ("a".to_string(), "2".to_string()),
            ("b".into(), "3.5".into()),
            ("c".into(), "ON".into()),
        ]
        .into();
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
            total: 1,
        };
        let t = g.project(
            "sessions",
            "Sesiones",
            &[(&["ID"], "id"), (&["db"], "base"), (&["INFO"], "consulta")],
        );
        assert_eq!(t.columns, ["id", "consulta"]);
        assert_eq!(t.rows, vec![vec![json!(1), Value::Null]]);
        assert_eq!(g.num(&g.rows[0], &["id"]), Some(1.0));
        assert_eq!(g.text(&g.rows[0], &["user"]).as_deref(), Some("root"));
    }

    fn documented(m: &Metric) {
        assert!(
            m.help.as_deref().is_some_and(|h| !h.trim().is_empty()),
            "{} has no help",
            m.key
        );
        assert!(
            m.source.as_deref().is_some_and(|s| !s.trim().is_empty()),
            "{} has no source",
            m.key
        );
    }

    /// Every metric the monitor builds says what it measures and where it
    /// comes from: each `metric(…)` is followed by `.help(…)` and
    /// `.source(…)`. A snapshot needs a live server, so this reads the code.
    #[test]
    fn every_metric_call_has_help_and_source() {
        let src = include_str!("monitor.rs");
        let code = &src[..src.find("#[cfg(test)]").unwrap()];
        let b = code.as_bytes();
        // The index after the parenthesis that closes the one at `open`.
        let close = |open: usize| -> usize {
            let (mut depth, mut i, mut quoted) = (0, open, false);
            loop {
                match b[i] {
                    b'\\' if quoted => i += 1,
                    b'"' => quoted = !quoted,
                    b'(' if !quoted => depth += 1,
                    b')' if !quoted => {
                        depth -= 1;
                        if depth == 0 {
                            return i + 1;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
        };
        let mut calls = 0;
        for (at, _) in code.match_indices("metric(") {
            let prev = b[at - 1];
            if prev.is_ascii_alphanumeric()
                || prev == b'_'
                || prev == b'.'
                || code[..at].ends_with("fn ")
            {
                continue;
            }
            calls += 1;
            let mut end = close(at + "metric".len());
            let mut chain = Vec::new();
            // The next token after `end`, past whitespace and line comments.
            let next = |mut rest: &'static str| loop {
                rest = rest.trim_start();
                match rest.strip_prefix("//") {
                    Some(c) => rest = c.split_once('\n').map_or("", |(_, r)| r),
                    None => return rest,
                }
            };
            while let Some(r) = next(&code[end..]).strip_prefix('.') {
                let name: String = r
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                let open = code.len() - r.len() + name.len();
                if b[open] != b'(' {
                    break;
                }
                chain.push(name);
                end = close(open);
            }
            assert!(
                chain.iter().any(|n| n == "help") && chain.iter().any(|n| n == "source"),
                "line {}: metric(…) without .help(…) and .source(…)",
                code[..at].lines().count()
            );
        }
        assert!(calls > 60, "only {calls} metric(…) calls found");
    }

    #[test]
    fn manticore_figures() {
        let st: Kv = [
            ("load", "0.50 0.20 0.10"),
            ("workers_total", "8"),
            ("workers_active", "2"),
            ("workers_clients", "3"),
            ("work_queue_length", "0"),
            ("maxed_out", "0"),
            ("connections", "40"),
            ("queries", "100"),
            ("command_insert", "5"),
            ("command_delete", "1"),
            ("avg_query_wall", "0.002"),
            ("query_cpu", "4"),
            ("qcache_used_bytes", "10"),
            ("qcache_max_bytes", "100"),
            ("qcache_hits", "7"),
            ("uptime", "60"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let m = manticore_metrics(&st);
        assert_eq!(m.len(), 13);
        for x in &m {
            assert!(x.value.is_some(), "{}", x.key);
            documented(x);
        }
        let get = |k: &str| m.iter().find(|m| m.key == k).unwrap();
        assert_eq!(get("load").value, Some(0.5));
        assert_eq!(get("writes").value, Some(6.0));
        // 4 s of CPU over 8 workers: 50, so 100 is all of them busy.
        assert_eq!(get("cpu_time").value, Some(50.0));
        assert_eq!(get("cpu_time").max, Some(100.0));
        // Without --cpustats there is no CPU figure.
        let mut off = st.clone();
        off.insert("query_cpu".into(), "OFF".into());
        let m = manticore_metrics(&off);
        assert_eq!(m.iter().find(|m| m.key == "cpu_time").unwrap().value, None);
    }

    /// The statements' CPU is a share of every CPU (100 = all busy), never a
    /// per-core figure; without the CPU count there is no value.
    #[test]
    fn statement_cpu_is_a_share_of_every_cpu() {
        // 8 s of CPU (in picoseconds) over 4 CPUs: 200.
        let m = statement_cpu(8e12, Some(4.0));
        assert_eq!(m.value, Some(200.0));
        assert_eq!(m.max, Some(100.0));
        assert!(m.counter);
        documented(&m);
        assert_eq!(statement_cpu(8e12, None).value, None);
    }
}
