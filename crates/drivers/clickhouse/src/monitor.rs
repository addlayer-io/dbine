//! `Session::monitor`: ClickHouse (and Timeplus Proton, a fork) describe
//! themselves in `system.*`: `asynchronous_metrics` (OS CPU and memory),
//! `metrics` (gauges: connections, running queries, caches), `events`
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
    keys.iter()
        .filter_map(|k| get(kv, k))
        .fold(None, |a, v| Some(a.unwrap_or(0.0) + v))
}

fn metric(key: &str, label: &str, group: &str, unit: MetricUnit, value: Option<f64>) -> Metric {
    Metric::new(key, label, group, unit, value)
}

/// What the server reported for the metrics.
#[derive(Default)]
struct Readings {
    /// `system.asynchronous_metrics`.
    asy: Kv,
    /// `system.metrics` (gauges).
    gauges: Kv,
    /// `system.events` (counters since start).
    events: Kv,
    /// `system.server_settings`.
    settings: Kv,
    uptime: Option<f64>,
    /// `sum(total_bytes)` of `system.tables`.
    tables_bytes: Option<f64>,
    /// `sum(total_space)` of `system.disks`.
    disk_total: Option<f64>,
    /// With replicated tables, the worst `absolute_delay`.
    replica_lag: Option<Option<f64>>,
}

/// Host CPU busy (%), without I/O wait.
fn host_cpu(asy: &Kv) -> Option<f64> {
    get(asy, "OSIdleTimeNormalized").map(|idle| {
        (100.0 * (1.0 - idle - get(asy, "OSIOWaitTimeNormalized").unwrap_or(0.0))).clamp(0.0, 100.0)
    })
}

/// The OS CPU times the server reports, summed over every CPU and
/// normalized (divided by the CPUs it counts).
const OS_TIMES: [(&str, &str); 4] = [
    ("OSUserTime", "OSUserTimeNormalized"),
    ("OSSystemTime", "OSSystemTimeNormalized"),
    ("OSIdleTime", "OSIdleTimeNormalized"),
    ("OSIOWaitTime", "OSIOWaitTimeNormalized"),
];

/// The CPUs the server may use, and where that comes from: its cgroup's
/// quota, else the cores it reports, else the ones it divides its OS CPU
/// times by (recent versions report no core count).
fn cores(asy: &Kv) -> Option<(f64, &'static str)> {
    let positive = |k: &str| get(asy, k).filter(|c| *c > 0.0);
    if let Some(c) = positive("CGroupMaxCPU") {
        return Some((c, "asynchronous_metrics CGroupMaxCPU"));
    }
    if let Some(c) = positive("NumberOfLogicalCPUCores") {
        return Some((c, "asynchronous_metrics NumberOfLogicalCPUCores"));
    }
    let raw = sum(asy, &OS_TIMES.map(|(t, _)| t));
    let normalized = sum(asy, &OS_TIMES.map(|(_, n)| n)).filter(|n| *n > 0.0);
    raw.zip(normalized)
        .map(|(r, n)| r / n)
        .filter(|c| *c >= 0.5)
        .map(|c| {
            (
                c,
                "asynchronous_metrics (OSUserTime + OSSystemTime + OSIdleTime + OSIOWaitTime) / the same *Normalized",
            )
        })
}

/// The memory the server sees: its cgroup's, else the host's.
fn mem_total(asy: &Kv) -> Option<f64> {
    get(asy, "CGroupMemoryTotal")
        .filter(|t| *t > 0.0)
        .or(get(asy, "OSMemoryTotal"))
}

/// Every figure, with what it measures and where it comes from; the ones
/// the server didn't report go with no value (and `add` leaves them out).
fn metrics(r: &Readings, flavor: Flavor) -> Vec<Metric> {
    let (asy, gauges, events, settings) = (&r.asy, &r.gauges, &r.events, &r.settings);
    let mut out = Vec::new();

    // CPU.
    out.push(
        metric("cpu", "CPU del host", "CPU", MetricUnit::Percent, host_cpu(asy))
            .max(Some(100.0))
            .help("Uso de CPU de la máquina donde corre el servidor, sin contar la espera de E/S, en la última medición de las métricas asíncronas (cada segundo por omisión): 100 % equivale a todos los núcleos ocupados. Incluye todo lo que corre en esa máquina, no solo el servidor.")
            .source("system.asynchronous_metrics: 1 − OSIdleTimeNormalized − OSIOWaitTimeNormalized"),
    );
    // Microseconds of CPU spread over the cores the server may use: the
    // rate is the share of all of them (100 = every core busy), never
    // "500 %" for five busy cores. Unknown core count: not shown.
    let cpu_us = sum(events, &["UserTimeMicroseconds", "SystemTimeMicroseconds"]);
    let (n_cores, cores_src) = match cores(asy) {
        Some((n, src)) => (Some(n), src),
        None => (None, "the CPUs the server counts"),
    };
    out.push(
        metric(
            "cpu_time",
            "CPU de las consultas",
            "CPU",
            MetricUnit::Percent,
            cpu_us.zip(n_cores).map(|(us, n)| us / 1e4 / n),
        )
        .max(Some(100.0))
        .counter()
        .help("Tiempo de CPU (usuario y sistema) de los hilos que procesan consultas y otras tareas del servidor, como merges e inserciones, como parte de los núcleos que puede usar: 100 % equivale a todos ocupados.")
        .source(&format!("system.events: UserTimeMicroseconds + SystemTimeMicroseconds, ÷ {cores_src}")),
    );
    out.push(
        metric("load1", "Carga (1 min)", "CPU", MetricUnit::Count, get(asy, "LoadAverage1"))
            .help("Promedio de carga del sistema operativo en el último minuto: procesos ejecutando o esperando CPU (y, en Linux, disco). Si supera la cantidad de núcleos, hay trabajo esperando.")
            .source("system.asynchronous_metrics: LoadAverage1"),
    );

    // Memory.
    let limit = get(settings, "max_server_memory_usage")
        .filter(|m| *m > 0.0)
        .or(mem_total(asy));
    let (mem, mem_src) = match get(gauges, "MemoryTracking") {
        Some(v) => (Some(v), "system.metrics: MemoryTracking"),
        None => (
            get(asy, "MemoryResident"),
            "system.asynchronous_metrics: MemoryResident",
        ),
    };
    out.push(
        metric("mem_used", "Memoria del servidor", "Memoria", MetricUnit::Bytes, mem)
            .max(limit)
            .help("Memoria que el servidor tiene asignada según su propio contador (o la residente del proceso, si no lo informa). El máximo es max_server_memory_usage, o la memoria del contenedor o del host; al llegar, las consultas que piden más fallan con MEMORY_LIMIT_EXCEEDED.")
            .source(&format!("{mem_src} (max: server_settings max_server_memory_usage, else asynchronous_metrics CGroupMemoryTotal or OSMemoryTotal)")),
    );
    let host_mem = get(asy, "OSMemoryTotal").zip(get(asy, "OSMemoryAvailable"));
    out.push(
        metric(
            "host_mem",
            "Memoria del host",
            "Memoria",
            MetricUnit::Bytes,
            host_mem.map(|(t, a)| t - a),
        )
        .max(host_mem.map(|(t, _)| t))
        .help("Memoria en uso en la máquina (la total menos la disponible), de todos los procesos; el máximo es la memoria total.")
        .source("system.asynchronous_metrics: OSMemoryTotal − OSMemoryAvailable (max: OSMemoryTotal)"),
    );
    // The in-memory caches, as gauges in system.metrics (recent versions)
    // or in asynchronous_metrics (older ones), each counted once.
    // FilesystemCacheBytes is the cache of remote storage (S3 and the
    // like): on disk, and possibly far bigger.
    let in_memory = |k: &str| k.ends_with("CacheBytes") && !k.starts_with("Filesystem");
    let mut each: HashMap<&str, f64> = HashMap::new();
    for (k, v) in asy.iter().chain(gauges.iter()) {
        if in_memory(k) {
            each.insert(k, *v);
        }
    }
    let caches = (!each.is_empty()).then(|| each.values().sum::<f64>());
    out.push(
        metric(
            "mem_cache",
            "Cachés (marcas, sin comprimir, consultas…)",
            "Memoria",
            MetricUnit::Bytes,
            caches,
        )
        .help("Memoria que ocupan las cachés del servidor sumadas: marcas, bloques sin comprimir, resultados de consultas, índices y otras. Cada una tiene su propio tamaño máximo en la configuración.")
        .source("system.metrics (older versions: system.asynchronous_metrics): sum of the metrics named *CacheBytes, each once, except FilesystemCacheBytes (on disk)"),
    );

    // Connections and activity.
    let conns = sum(
        gauges,
        &[
            "TCPConnection",
            "HTTPConnection",
            "MySQLConnection",
            "PostgreSQLConnection",
            "InterserverConnection",
        ],
    );
    out.push(
        metric("connections", "Conexiones", "Conexiones", MetricUnit::Count, conns)
            .max(get(settings, "max_connections").filter(|m| *m > 0.0))
            .help("Conexiones abiertas por todos los protocolos: TCP nativo, HTTP, MySQL, PostgreSQL y entre réplicas. El máximo es max_connections. Una conexión HTTP dura lo que el pedido, o lo que la mantenga abierta el keep-alive.")
            .source("system.metrics: TCPConnection + HTTPConnection + MySQLConnection + PostgreSQLConnection + InterserverConnection (max: server_settings max_connections)"),
    );
    let running = match flavor {
        Flavor::ClickHouse => "Consultas ejecutándose ahora, incluida la que usa el monitor para leer este valor. El máximo es max_concurrent_queries: al llegar, las consultas nuevas se rechazan.",
        Flavor::Timeplus => "Consultas ejecutándose ahora, incluida la que usa el monitor para leer este valor. Cuenta también las consultas de streaming, que siguen corriendo hasta que se cancelan. El máximo es max_concurrent_queries: al llegar, las consultas nuevas se rechazan.",
    };
    out.push(
        metric(
            "active_sessions",
            "Consultas en ejecución",
            "Conexiones",
            MetricUnit::Count,
            get(gauges, "Query"),
        )
        .max(get(settings, "max_concurrent_queries").filter(|m| *m > 0.0))
        .help(running)
        .source("system.metrics: Query (max: server_settings max_concurrent_queries)"),
    );
    let counters = [
        ("queries", "Consultas", "Actividad", MetricUnit::Count, "Query", "Consultas que el servidor recibió para ejecutar, de cualquier tipo. No cuenta las subconsultas ni las que no se pudieron interpretar."),
        ("select_queries", "SELECT", "Actividad", MetricUnit::Count, "SelectQuery", "Consultas SELECT que recibió el servidor."),
        ("insert_queries", "INSERT", "Actividad", MetricUnit::Count, "InsertQuery", "Consultas INSERT que recibió el servidor. Muchas inserciones chicas generan muchas partes y más trabajo de merges: conviene agruparlas."),
        ("failed_queries", "Consultas fallidas", "Actividad", MetricUnit::Count, "FailedQuery", "Consultas que terminaron con error, de cualquier tipo."),
        ("rows_read", "Filas leídas", "Actividad", MetricUnit::Count, "SelectedRows", "Filas que leyeron los SELECT, de todas las tablas (también las de sistema)."),
        ("rows_written", "Filas escritas", "Actividad", MetricUnit::Count, "InsertedRows", "Filas insertadas en todas las tablas."),
        ("net_in", "Red entrante", "Red", MetricUnit::Bytes, "NetworkReceiveBytes", "Bytes que el servidor recibió por la red, de los clientes y de otras réplicas."),
        ("net_out", "Red saliente", "Red", MetricUnit::Bytes, "NetworkSendBytes", "Bytes que el servidor envió por la red, a los clientes y a otras réplicas."),
    ];
    for (key, label, group, unit, event, help) in counters {
        out.push(
            metric(key, label, group, unit, get(events, event))
                .counter()
                .help(help)
                .source(&format!("system.events: {event}")),
        );
    }
    // The OS figures are real disk I/O; the file ones include what the
    // page cache serves.
    let io = |os: &str, file: &str| match get(events, os) {
        Some(v) => (Some(v), format!("system.events: {os}")),
        None => (get(events, file), format!("system.events: {file}")),
    };
    let (read, read_src) = io("OSReadBytes", "ReadBufferFromFileDescriptorReadBytes");
    out.push(
        metric("disk_read", "Lectura en disco", "Disco", MetricUnit::Bytes, read)
            .counter()
            .help("Bytes que el servidor leyó del disco. Si el sistema no informa la E/S real (OSReadBytes), son las lecturas de archivos, que incluyen lo que resuelve la caché del sistema operativo.")
            .source(&read_src),
    );
    let (write, write_src) = io("OSWriteBytes", "WriteBufferFromFileDescriptorWriteBytes");
    out.push(
        metric("disk_write", "Escritura en disco", "Disco", MetricUnit::Bytes, write)
            .counter()
            .help("Bytes que el servidor escribió en disco: partes nuevas, merges y logs. Si el sistema no informa la E/S real (OSWriteBytes), son las escrituras en archivos.")
            .source(&write_src),
    );
    let hit = match (get(events, "MarkCacheHits"), get(events, "MarkCacheMisses")) {
        (Some(h), m) if h + m.unwrap_or(0.0) > 0.0 => Some(100.0 * h / (h + m.unwrap_or(0.0))),
        _ => None,
    };
    out.push(
        metric(
            "cache_hit",
            "Aciertos de la caché de marcas",
            "Caché",
            MetricUnit::Percent,
            hit,
        )
        .max(Some(100.0))
        .help("Porcentaje de búsquedas en la caché de marcas (el índice de cada parte MergeTree) que encontraron lo que buscaban, acumulado desde el arranque, por eso cambia despacio. Si baja, mark_cache_size quedó chica para las tablas que se consultan.")
        .source("system.events: MarkCacheHits / (MarkCacheHits + MarkCacheMisses)"),
    );

    // MergeTree.
    out.push(
        metric(
            "merges",
            "Merges en curso",
            "MergeTree",
            MetricUnit::Count,
            get(gauges, "Merge"),
        )
        .help("Merges de partes que corren ahora en segundo plano.")
        .source("system.metrics: Merge"),
    );
    out.push(
        metric(
            "mutations",
            "Mutaciones en curso",
            "MergeTree",
            MetricUnit::Count,
            get(gauges, "PartMutation"),
        )
        .help("Mutaciones (ALTER … UPDATE o DELETE) que están reescribiendo partes ahora.")
        .source("system.metrics: PartMutation"),
    );
    out.push(
        metric(
            "parts",
            "Partes activas",
            "MergeTree",
            MetricUnit::Count,
            get(asy, "TotalPartsOfMergeTreeTables"),
        )
        .help("Partes activas de todas las tablas MergeTree. Si crecen sin parar, los merges no dan abasto con las inserciones.")
        .source("system.asynchronous_metrics: TotalPartsOfMergeTreeTables"),
    );
    out.push(
        metric(
            "max_parts_per_partition",
            "Máx. partes por partición",
            "MergeTree",
            MetricUnit::Count,
            get(asy, "MaxPartCountForPartition"),
        )
        .help("La mayor cantidad de partes activas en una misma partición. Al llegar a parts_to_delay_insert, el servidor demora las inserciones en esa tabla, y en parts_to_throw_insert las rechaza (Too many parts).")
        .source("system.asynchronous_metrics: MaxPartCountForPartition"),
    );
    out.push(
        metric(
            "delayed_inserts",
            "Inserciones demoradas",
            "MergeTree",
            MetricUnit::Count,
            get(gauges, "DelayedInserts"),
        )
        .help("INSERT que el servidor está demorando ahora porque su partición tiene demasiadas partes activas.")
        .source("system.metrics: DelayedInserts"),
    );
    out.push(
        metric(
            "merged_rows",
            "Filas fusionadas",
            "MergeTree",
            MetricUnit::Count,
            get(events, "MergedRows"),
        )
        .counter()
        .help("Filas que leyeron los merges de fondo, contadas antes de fusionar.")
        .source("system.events: MergedRows"),
    );
    out.push(
        metric(
            "locks_waiting",
            "Esperas de bloqueo de tablas",
            "Bloqueos",
            MetricUnit::Count,
            sum(gauges, &["RWLockWaitingReaders", "RWLockWaitingWriters"]),
        )
        .help("Hilos esperando el bloqueo de lectura o escritura de una tabla, por ejemplo detrás de un ALTER, un RENAME o un DROP.")
        .source("system.metrics: RWLockWaitingReaders + RWLockWaitingWriters"),
    );

    // Storage.
    let (used, used_src) = match r.tables_bytes {
        Some(v) => (
            Some(v),
            "system.tables: sum(total_bytes), without information_schema",
        ),
        None => (
            get(asy, "TotalBytesOfMergeTreeTables"),
            "system.asynchronous_metrics: TotalBytesOfMergeTreeTables",
        ),
    };
    out.push(
        metric("storage_used", "Espacio usado", "Almacenamiento", MetricUnit::Bytes, used)
            .max(r.disk_total.filter(|t| *t > 0.0))
            .help("Bytes que ocupan las tablas de todas las bases (en MergeTree, comprimidos en disco). El máximo es el tamaño de los discos configurados, que pueden tener también otros archivos.")
            .source(&format!("{used_src} (max: system.disks sum(total_space))")),
    );
    out.push(
        metric(
            "tables",
            "Tablas",
            "Almacenamiento",
            MetricUnit::Count,
            get(asy, "NumberOfTables"),
        )
        .help("Tablas de todas las bases, sin contar las de motores de base que las arman al vuelo (MySQL, PostgreSQL, Lazy…).")
        .source("system.asynchronous_metrics: NumberOfTables"),
    );

    // Replication.
    if let Some(lag) = r.replica_lag {
        let (lag, src) = match lag {
            Some(v) => (Some(v), "system.replicas: max(absolute_delay)"),
            None => (
                get(asy, "ReplicasMaxAbsoluteDelay"),
                "system.asynchronous_metrics: ReplicasMaxAbsoluteDelay",
            ),
        };
        out.push(
            metric(
                "replication_lag",
                "Retraso de réplica",
                "Replicación",
                MetricUnit::Seconds,
                lag,
            )
            .help("Atraso de la tabla replicada más atrasada de este servidor: cuánto hace que llegó la entrada más vieja de su cola que todavía no aplicó. Si crece, esa réplica no está al día.")
            .source(src),
        );
    }
    out.push(
        metric(
            "uptime",
            "Tiempo activo",
            "Servidor",
            MetricUnit::Seconds,
            r.uptime,
        )
        .help("Tiempo desde que arrancó el servidor.")
        .source("uptime()"),
    );
    out
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
        let cols: Vec<(usize, &str)> = pick
            .iter()
            .filter_map(|(n, l)| Some((self.col(n)?, *l)))
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
                        Value::String(s) if s.chars().count() > 2000 => {
                            Value::String(s.chars().take(2000).collect::<String>() + "…")
                        }
                        v => v,
                    })
                    .collect()
            })
            .collect();
        Ok(Grid {
            columns: r.columns.into_iter().map(|c| c.name).collect(),
            rows,
        })
    }

    /// `name, value` pairs as numbers.
    async fn kv(&self, sql: &str) -> Result<Kv> {
        let rows = self.rows(sql, &[]).await?;
        Ok(rows
            .iter()
            .filter_map(|r| Some((text(r.first()?), fnum(r.get(1)?)?)))
            .collect())
    }

    pub(crate) async fn snapshot(&mut self) -> Result<MonitorSnapshot> {
        let mut s = MonitorSnapshot::default();
        // The one query that must work.
        let head = self
            .rows("SELECT version(), uptime(), timezone(), hostName()", &[])
            .await?;
        let head = head.first().cloned().unwrap_or_default();
        s.info
            .push(("Versión".into(), head.first().map(text).unwrap_or_default()));
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
                                  'OSUserTime', 'OSSystemTime', 'OSIdleTime', 'OSIOWaitTime', 'OSUserTimeNormalized',
                                  'OSSystemTimeNormalized',
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
                                  'RWLockWaitingReaders', 'RWLockWaitingWriters', 'DelayedInserts', 'BackgroundMergesAndMutationsPoolTask')
                    OR metric LIKE '%CacheBytes'",
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

        // Storage: the disks as the ceiling.
        let disks = self.grid("SELECT * FROM system.disks").await;
        let disk_total = disks.as_ref().ok().and_then(|g| {
            let i = g.col("total_space")?;
            Some(
                g.rows
                    .iter()
                    .filter_map(|r| r.get(i).and_then(fnum))
                    .sum::<f64>(),
            )
        });
        let dbs = self
            .grid(
                "SELECT database AS base, count() AS tablas, sum(total_rows) AS filas, sum(total_bytes) AS bytes
                 FROM system.tables WHERE database NOT IN ('INFORMATION_SCHEMA', 'information_schema')
                 GROUP BY database ORDER BY bytes DESC LIMIT 200",
            )
            .await;
        let tables_bytes = match &dbs {
            Ok(g) => g.col("bytes").map(|i| {
                g.rows
                    .iter()
                    .filter_map(|r| r.get(i).and_then(fnum))
                    .sum::<f64>()
            }),
            Err(_) => None,
        };

        // Replication.
        let replicas = self
            .grid(
                "SELECT database AS base, table AS tabla, is_leader AS `líder`, is_readonly AS `solo lectura`,
                        absolute_delay AS `retraso (s)`, queue_size AS cola, inserts_in_queue AS inserciones,
                        merges_in_queue AS merges, active_replicas AS `réplicas activas`, total_replicas AS `réplicas`
                 FROM system.replicas ORDER BY absolute_delay DESC LIMIT 200",
            )
            .await;
        let replica_lag = match &replicas {
            Ok(g) if !g.rows.is_empty() => Some(g.col("retraso (s)").and_then(|i| {
                g.rows
                    .iter()
                    .filter_map(|r| r.get(i).and_then(fnum))
                    .reduce(f64::max)
            })),
            _ => None,
        };

        let r = Readings {
            asy,
            gauges,
            events,
            settings,
            uptime,
            tables_bytes,
            disk_total,
            replica_lag,
        };
        for m in metrics(&r, self.flavor) {
            add(&mut s, m);
        }
        let Readings {
            asy,
            settings,
            events,
            ..
        } = r;
        if host_cpu(&asy).is_none() {
            s.notes.push(format!("{product} no informó el uso de CPU del host (system.asynchronous_metrics sin métricas OS*)."));
        }
        if events.contains_key("UserTimeMicroseconds") && cores(&asy).is_none() {
            s.notes.push(format!(
                "{product} no informó cuántos núcleos puede usar (system.asynchronous_metrics): sin ese dato no se muestra la CPU de las consultas."
            ));
        }
        let total = mem_total(&asy);

        // Info.
        if let Some(c) =
            get(&asy, "NumberOfLogicalCPUCores").or(get(&asy, "NumberOfPhysicalCPUCores"))
        {
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
            s.info
                .push(("Máx. consultas simultáneas".into(), format!("{m:.0}")));
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
            Ok(g) => s
                .tables
                .push(g.table("databases", "Bases y tamaños (bytes)")),
            Err(e) => s.notes.push(format!(
                "No se pudieron leer los tamaños de system.tables ({}).",
                why(&e)
            )),
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
            Err(e) => s
                .notes
                .push(format!("No se pudo leer system.disks ({}).", why(&e))),
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
        assert_eq!(
            fnum(&json!("18446744073709551615")),
            Some(18446744073709551615.0)
        );
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

    fn kv(keys: &[&str], v: f64) -> Kv {
        keys.iter().map(|k| (k.to_string(), v)).collect()
    }

    /// A server that reports everything the monitor reads.
    fn full() -> Readings {
        Readings {
            asy: kv(
                &[
                    "OSIdleTimeNormalized",
                    "OSIOWaitTimeNormalized",
                    "OSMemoryTotal",
                    "OSMemoryAvailable",
                    "MemoryResident",
                    "NumberOfLogicalCPUCores",
                    "CGroupMemoryTotal",
                    "MaxPartCountForPartition",
                    "TotalPartsOfMergeTreeTables",
                    "TotalBytesOfMergeTreeTables",
                    "NumberOfTables",
                    "ReplicasMaxAbsoluteDelay",
                    "LoadAverage1",
                    "MarkCacheBytes",
                ],
                0.25,
            ),
            gauges: kv(
                &[
                    "TCPConnection",
                    "HTTPConnection",
                    "Query",
                    "Merge",
                    "PartMutation",
                    "MemoryTracking",
                    "RWLockWaitingReaders",
                    "DelayedInserts",
                ],
                2.0,
            ),
            events: kv(
                &[
                    "Query",
                    "SelectQuery",
                    "InsertQuery",
                    "FailedQuery",
                    "SelectedRows",
                    "InsertedRows",
                    "NetworkReceiveBytes",
                    "NetworkSendBytes",
                    "OSReadBytes",
                    "OSWriteBytes",
                    "UserTimeMicroseconds",
                    "SystemTimeMicroseconds",
                    "MarkCacheHits",
                    "MarkCacheMisses",
                    "MergedRows",
                ],
                1e6,
            ),
            settings: kv(&["max_connections", "max_concurrent_queries"], 100.0),
            uptime: Some(60.0),
            tables_bytes: Some(1e9),
            disk_total: Some(1e12),
            replica_lag: Some(Some(3.0)),
        }
    }

    /// Every figure says what it measures and where it comes from, in both
    /// flavors, with and without readings.
    #[test]
    fn every_metric_has_help_and_source() {
        for flavor in [Flavor::ClickHouse, Flavor::Timeplus] {
            let all = metrics(&full(), flavor);
            assert!(
                all.iter().all(|m| m.value.is_some()),
                "a fixture is missing"
            );
            assert_eq!(all.len(), 30);
            let empty = metrics(&Readings::default(), flavor);
            for x in all.iter().chain(&empty) {
                assert!(
                    x.help.as_deref().is_some_and(|h| !h.trim().is_empty()),
                    "{} has no help",
                    x.key
                );
                assert!(
                    x.source.as_deref().is_some_and(|s| !s.trim().is_empty()),
                    "{} has no source",
                    x.key
                );
            }
            let mut keys: Vec<&str> = all.iter().map(|m| m.key.as_str()).collect();
            keys.sort_unstable();
            keys.dedup();
            assert_eq!(keys.len(), all.len(), "duplicate keys");
        }
        let help = |f| {
            metrics(&full(), f)
                .into_iter()
                .find(|m| m.key == "active_sessions")
                .and_then(|m| m.help)
                .unwrap()
        };
        assert!(help(Flavor::Timeplus).contains("streaming"));
        assert!(!help(Flavor::ClickHouse).contains("streaming"));
    }

    /// The queries' CPU is a share of every core the server may use, never
    /// a per-core figure; without the core count it isn't shown.
    #[test]
    fn query_cpu_is_a_share_of_every_core() {
        let cpu = |r: &Readings| {
            metrics(r, Flavor::ClickHouse)
                .into_iter()
                .find(|m| m.key == "cpu_time")
                .unwrap()
        };
        let mut r = Readings {
            events: kv(&["UserTimeMicroseconds", "SystemTimeMicroseconds"], 4e6),
            asy: kv(&["NumberOfLogicalCPUCores"], 8.0),
            ..Default::default()
        };
        // 8 s of CPU over 8 cores: a second of all of them is 100.
        let m = cpu(&r);
        assert_eq!(m.value, Some(100.0));
        assert_eq!(m.max, Some(100.0));
        assert!(m.counter);
        // A cgroup quota wins over the host's cores.
        r.asy.insert("CGroupMaxCPU".into(), 2.0);
        assert_eq!(cpu(&r).value, Some(400.0));
        r.asy.clear();
        assert_eq!(cpu(&r).value, None);
        // Recent versions report no core count: the one the OS CPU times
        // are normalized by (here 4).
        r.asy = [
            ("OSUserTime", 1.0),
            ("OSIdleTime", 3.0),
            ("OSUserTimeNormalized", 0.25),
            ("OSIdleTimeNormalized", 0.75),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), *v))
        .collect();
        let m = cpu(&r);
        assert_eq!(m.value, Some(200.0));
        assert!(m.source.as_deref().unwrap().contains("Normalized"));
        // Disk I/O falls back to the file counters, and says so.
        let r = Readings {
            events: kv(&["ReadBufferFromFileDescriptorReadBytes"], 5.0),
            ..Default::default()
        };
        let m = metrics(&r, Flavor::ClickHouse);
        let read = m.iter().find(|m| m.key == "disk_read").unwrap();
        assert_eq!(read.value, Some(5.0));
        assert!(read
            .source
            .as_deref()
            .unwrap()
            .contains("ReadBufferFromFileDescriptor"));
    }

    /// The memory caches leave out the filesystem cache, which is on disk.
    #[test]
    fn memory_caches_leave_the_disk_cache_out() {
        let r = Readings {
            asy: [
                ("MarkCacheBytes", 10.0),
                ("UncompressedCacheBytes", 5.0),
                ("FilesystemCacheBytes", 1e12),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), *v))
            .collect(),
            ..Default::default()
        };
        let cache = |r: &Readings| {
            metrics(r, Flavor::ClickHouse)
                .into_iter()
                .find(|m| m.key == "mem_cache")
                .unwrap()
                .value
        };
        assert_eq!(cache(&r), Some(15.0));
        // Recent versions keep them in system.metrics: each counts once.
        let gauges = Readings {
            asy: kv(&["MarkCacheBytes"], 10.0),
            gauges: kv(&["MarkCacheBytes", "QueryCacheBytes"], 10.0),
            ..Default::default()
        };
        assert_eq!(cache(&gauges), Some(20.0));
        let only_disk = Readings {
            asy: kv(&["FilesystemCacheBytes"], 1e12),
            ..Default::default()
        };
        assert_eq!(cache(&only_disk), None);
    }
}
