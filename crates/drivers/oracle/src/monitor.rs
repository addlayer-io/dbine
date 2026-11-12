//! Server monitor from Oracle's dynamic performance views: `V$SYSMETRIC`
//! (host CPU, hit ratios), `V$SYSSTAT` (running totals), `V$OSSTAT`,
//! `V$SGAINFO` / `V$PGASTAT`, `V$SESSION`, `V$LOCK`, `V$SYSTEM_EVENT`,
//! tablespaces from `DBA_DATA_FILES` / `DBA_FREE_SPACE`, and Data Guard.
//!
//! The V$ views need SELECT_CATALOG_ROLE (or SELECT ANY DICTIONARY); each
//! section is its own query, so what the user can't read is left out
//! with a note and the rest still shows.

use crate::cell;
use dbine_driver::monitor::{Metric, MetricUnit as U, MonitorSnapshot, MonitorTable};
use oracledb::Connection;
use serde_json::Value;

/// Rows of a query as JSON cells (numbers as numbers or exact strings).
pub(crate) fn rows(
    c: &Connection,
    sql: &str,
) -> std::result::Result<Vec<Vec<Value>>, oracledb::Error> {
    rows_max(c, sql, 200)
}

/// [`rows`], keeping at most `max`.
pub(crate) fn rows_max(
    c: &Connection,
    sql: &str,
    max: usize,
) -> std::result::Result<Vec<Vec<Value>>, oracledb::Error> {
    // Out of the statement cache: the thin client caches a statement whose
    // parse failed (ORA-00942 without the grant) and the next snapshot
    // would get ORA-01003 instead of the real error.
    let stmt = c
        .statement(sql)
        .map(|b| b.exclude_from_cache())
        .and_then(|b| b.build())?;
    let cursor = stmt.query(&[])?;
    let types: Vec<&'static oracledb::DbType> =
        cursor.columns().iter().map(|m| m.db_type()).collect();
    let mut out = Vec::new();
    for row in cursor {
        let row = row?;
        out.push(
            types
                .iter()
                .enumerate()
                .map(|(i, t)| cell(&row, i, t))
                .collect(),
        );
        if out.len() >= max {
            break;
        }
    }
    Ok(out)
}

pub(crate) fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => dbine_driver::monitor::num(s),
        _ => None,
    }
}

pub(crate) fn txt(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// What couldn't be read, folded into notes at the end.
#[derive(Default)]
struct Gaps {
    denied: Vec<&'static str>,
    notes: Vec<String>,
}

impl Gaps {
    fn probe(&mut self, c: &Connection, what: &'static str, sql: &str) -> Option<Vec<Vec<Value>>> {
        match rows(c, sql) {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::debug!("oracle monitor: {what}: {e}");
                match crate::db_code(&e) {
                    // Table or view doesn't exist (no grant) / insufficient privileges.
                    Some(942 | 1031 | 1039) if !self.denied.contains(&what) => {
                        self.denied.push(what)
                    }
                    Some(942 | 1031 | 1039) => {}
                    _ => self.notes.push(format!(
                        "No se pudo leer {what}: {}",
                        e.to_string().lines().next().unwrap_or_default()
                    )),
                }
                None
            }
        }
    }
}

/// Name → value pairs of a two-column result.
fn pairs(rows: &Option<Vec<Vec<Value>>>) -> Vec<(String, f64)> {
    rows.iter()
        .flatten()
        .filter_map(|r| Some((txt(r.first()?)?, num(r.get(1)?)?)))
        .collect()
}

fn get(p: &[(String, f64)], name: &str) -> Option<f64> {
    p.iter().find(|(n, _)| n == name).map(|(_, v)| *v)
}

fn table(
    key: &str,
    title: &str,
    cols: &[&str],
    rows: Option<Vec<Vec<Value>>>,
) -> Option<MonitorTable> {
    let mut t = MonitorTable::new(key, title, cols);
    t.rows = rows?;
    Some(t)
}

const INSTANCE: &str = "SELECT i.instance_name, i.host_name, i.version, i.status,
       ROUND((SYSDATE - i.startup_time) * 86400), d.name, d.database_role, d.open_mode, d.log_mode,
       SYS_CONTEXT('USERENV', 'CON_NAME'), SESSIONTIMEZONE, DBTIMEZONE
  FROM v$instance i CROSS JOIN v$database d";

const PARAMETERS: &str = "SELECT name, value FROM v$parameter
 WHERE name IN ('sessions', 'processes', 'sga_target', 'sga_max_size', 'pga_aggregate_target',
                'pga_aggregate_limit', 'memory_target', 'cpu_count', 'db_block_size')";

/// Newest value of each metric, from the longest interval (60 s).
const SYSMETRIC: &str = "SELECT metric_name, value FROM (
  SELECT metric_name, value, ROW_NUMBER() OVER (PARTITION BY metric_name ORDER BY intsize_csec DESC, end_time DESC) rn
    FROM v$sysmetric
   WHERE metric_name IN ('Host CPU Utilization (%)', 'Buffer Cache Hit Ratio', 'Average Active Sessions',
                         'Database CPU Time Ratio', 'Library Cache Hit Ratio', 'Memory Sorts Ratio')
) WHERE rn = 1";

const SYSSTAT: &str = "SELECT name, value FROM v$sysstat
 WHERE name IN ('CPU used by this session', 'execute count', 'user commits', 'user rollbacks',
                'physical read total bytes', 'physical write total bytes',
                'bytes received via SQL*Net from client', 'bytes sent via SQL*Net to client',
                'enqueue deadlocks', 'redo size', 'session logical reads', 'parse count (hard)',
                'logons cumulative', 'table scan rows gotten', 'physical reads cache',
                'consistent gets from cache', 'db block gets from cache')";

const OSSTAT: &str = "SELECT stat_name, value FROM v$osstat
 WHERE stat_name IN ('BUSY_TIME', 'IDLE_TIME', 'NUM_CPUS', 'PHYSICAL_MEMORY_BYTES', 'FREE_MEMORY_BYTES', 'LOAD')";

const SGA: &str = "SELECT name, bytes FROM v$sgainfo
 WHERE name IN ('Buffer Cache Size', 'Shared Pool Size', 'Maximum SGA Size', 'Free SGA Memory Available')
 UNION ALL SELECT 'SGA total', SUM(bytes) FROM v$sgastat";

const PGA: &str = "SELECT name, value FROM v$pgastat
 WHERE name IN ('total PGA allocated', 'total PGA inuse', 'aggregate PGA target parameter', 'maximum PGA allocated')";

const SESSION_COUNTS: &str = "SELECT COUNT(*),
       SUM(CASE WHEN status = 'ACTIVE' AND sid <> SYS_CONTEXT('USERENV', 'SID') THEN 1 ELSE 0 END),
       SUM(CASE WHEN blocking_session IS NOT NULL THEN 1 ELSE 0 END)
  FROM v$session WHERE type = 'USER'";

const LOCK_COUNTS: &str =
    "SELECT COUNT(*), SUM(CASE WHEN request > 0 THEN 1 ELSE 0 END) FROM v$lock";

const STORAGE: &str = "SELECT (SELECT SUM(bytes) FROM dba_data_files),
       (SELECT SUM(bytes) FROM dba_free_space),
       (SELECT SUM(GREATEST(bytes, CASE WHEN autoextensible = 'YES' THEN maxbytes ELSE bytes END)) FROM dba_data_files)
  FROM dual";

const SESSIONS: &str = "SELECT * FROM (SELECT s.sid, s.serial#, s.username, s.schemaname, s.machine, s.program, s.status,
       s.last_call_et, s.event, s.sql_id, SUBSTR(q.sql_text, 1, 2000)
  FROM v$session s
  LEFT JOIN v$sql q ON q.sql_id = s.sql_id AND q.child_number = s.sql_child_number
 WHERE s.type = 'USER' AND s.sid <> SYS_CONTEXT('USERENV', 'SID')
 ORDER BY CASE WHEN s.status = 'ACTIVE' THEN 0 ELSE 1 END, s.sid) WHERE ROWNUM <= 200";

const SESSION_COLS: &[&str] = &[
    "SID",
    "Serial",
    "Usuario",
    "Esquema",
    "Equipo",
    "Programa",
    "Estado",
    "Duración (s)",
    "Evento",
    "SQL ID",
    "Consulta",
];

const QUERIES: &str = "SELECT * FROM (SELECT s.sid, s.username, s.sql_id, s.last_call_et, s.event, s.wait_class, s.blocking_session,
       ROUND(q.elapsed_time / GREATEST(q.executions, 1) / 1000), q.buffer_gets, SUBSTR(q.sql_text, 1, 2000)
  FROM v$session s
  LEFT JOIN v$sql q ON q.sql_id = s.sql_id AND q.child_number = s.sql_child_number
 WHERE s.type = 'USER' AND s.status = 'ACTIVE' AND s.sid <> SYS_CONTEXT('USERENV', 'SID')
 ORDER BY s.last_call_et DESC) WHERE ROWNUM <= 200";

const QUERY_COLS: &[&str] = &[
    "SID",
    "Usuario",
    "SQL ID",
    "Duración (s)",
    "Evento",
    "Clase de espera",
    "Bloqueada por",
    "Promedio por ejecución (ms)",
    "Lecturas lógicas",
    "Consulta",
];

const BLOCKED: &str = "SELECT * FROM (SELECT s.sid, s.blocking_session, s.username, s.event, ROUND(s.wait_time_micro / 1000),
       l.type, l.lmode, l.request, o.owner || '.' || o.object_name, SUBSTR(q.sql_text, 1, 2000)
  FROM v$session s
  LEFT JOIN v$lock l ON l.sid = s.sid AND l.request > 0
  LEFT JOIN all_objects o ON o.object_id = s.row_wait_obj#
  LEFT JOIN v$sql q ON q.sql_id = s.sql_id AND q.child_number = s.sql_child_number
 WHERE s.blocking_session IS NOT NULL
 ORDER BY s.wait_time_micro DESC) WHERE ROWNUM <= 200";

const BLOCKED_COLS: &[&str] = &[
    "SID",
    "Bloqueada por",
    "Usuario",
    "Evento",
    "Esperando (ms)",
    "Tipo de bloqueo",
    "Modo tenido",
    "Modo pedido",
    "Objeto",
    "Consulta",
];

const WAITS: &str =
    "SELECT * FROM (SELECT event, wait_class, total_waits, ROUND(time_waited_micro / 1000),
       ROUND(time_waited_micro / GREATEST(total_waits, 1) / 1000, 2)
  FROM v$system_event
 WHERE wait_class <> 'Idle'
 ORDER BY time_waited_micro DESC) WHERE ROWNUM <= 20";

const WAIT_COLS: &[&str] = &["Evento", "Clase", "Esperas", "Tiempo (ms)", "Promedio (ms)"];

const TABLESPACES: &str = "SELECT d.tablespace_name, ROUND(d.bytes / 1048576, 1), ROUND((d.bytes - NVL(f.bytes, 0)) / 1048576, 1),
       ROUND(NVL(f.bytes, 0) / 1048576, 1), ROUND(d.maxbytes / 1048576, 1),
       ROUND((d.bytes - NVL(f.bytes, 0)) * 100 / NULLIF(d.maxbytes, 0), 1)
  FROM (SELECT tablespace_name, SUM(bytes) bytes,
               SUM(GREATEST(bytes, CASE WHEN autoextensible = 'YES' THEN maxbytes ELSE bytes END)) maxbytes
          FROM dba_data_files GROUP BY tablespace_name) d
  LEFT JOIN (SELECT tablespace_name, SUM(bytes) bytes FROM dba_free_space GROUP BY tablespace_name) f
    ON f.tablespace_name = d.tablespace_name
 ORDER BY d.tablespace_name";

const TABLESPACE_COLS: &[&str] = &[
    "Tablespace",
    "Tamaño (MB)",
    "Usado (MB)",
    "Libre (MB)",
    "Máximo (MB)",
    "% del máximo",
];

const PDBS: &str = "SELECT name, open_mode, ROUND(total_size / 1048576, 1), restricted FROM v$pdbs ORDER BY con_id";

const TOP_SEGMENTS: &str = "SELECT * FROM (SELECT owner || '.' || segment_name, segment_type, ROUND(bytes / 1048576, 2), tablespace_name
  FROM dba_segments
 ORDER BY bytes DESC) WHERE ROWNUM <= 20";

const DATAGUARD: &str = "SELECT name, value, unit, time_computed FROM v$dataguard_stats";

const ARCHIVE_DESTS: &str =
    "SELECT dest_name, status, type, database_mode, recovery_mode, gap_status
  FROM v$archive_dest_status WHERE status <> 'INACTIVE' AND type <> 'LOCAL'";

/// `+00 00:00:05` (a Data Guard lag) in seconds.
fn dg_lag(s: &str) -> Option<f64> {
    let s = s.trim().trim_start_matches('+');
    let (days, time) = s.split_once(' ')?;
    let t: Vec<f64> = time
        .split(':')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    (t.len() == 3)
        .then(|| days.parse::<f64>().unwrap_or(0.0) * 86_400.0 + t[0] * 3600.0 + t[1] * 60.0 + t[2])
}

/// The monitor snapshot. `last_os` keeps the previous `(BUSY_TIME,
/// IDLE_TIME)` of V$OSSTAT: when V$SYSMETRIC has no host CPU figure (a
/// PDB), it's the difference between two snapshots.
pub fn snapshot(c: &Connection, last_os: &mut Option<(f64, f64)>) -> MonitorSnapshot {
    let mut snap = MonitorSnapshot::default();
    let mut g = Gaps::default();

    let inst = g
        .probe(c, "la instancia (V$INSTANCE)", INSTANCE)
        .and_then(|r| r.into_iter().next());
    if let Some(r) = &inst {
        let s = |i: usize| r.get(i).and_then(txt);
        for (label, i) in [
            ("Instancia", 0),
            ("Equipo", 1),
            ("Versión", 2),
            ("Estado", 3),
            ("Base", 5),
            ("Rol", 6),
            ("Modo", 7),
            ("Archivado", 8),
            ("Contenedor", 9),
            ("Zona horaria de la base", 11),
        ] {
            if let Some(v) = s(i) {
                snap.info.push((label.into(), v));
            }
        }
    } else if let Ok(Some(con)) = c
        .query_row("SELECT SYS_CONTEXT('USERENV', 'CON_NAME') FROM dual", &[])
        .and_then(|r| r.get::<Option<String>>(0))
    {
        snap.info.push(("Contenedor".into(), con));
    }
    let params = pairs(
        &g.probe(c, "los parámetros (V$PARAMETER)", PARAMETERS)
            .map(|rows| {
                rows.into_iter()
                    .map(|r| {
                        r.into_iter()
                            .map(|v| {
                                if let Value::String(s) = &v {
                                    dbine_driver::monitor::num(s).map_or(v, Value::from)
                                } else {
                                    v
                                }
                            })
                            .collect()
                    })
                    .collect()
            }),
    );
    for (label, name) in [
        ("Sesiones máximas", "sessions"),
        ("Procesos máximos", "processes"),
        ("CPU (cpu_count)", "cpu_count"),
    ] {
        if let Some(v) = get(&params, name) {
            snap.info.push((label.into(), format!("{v}")));
        }
    }
    for (label, name) in [
        ("SGA objetivo", "sga_target"),
        ("PGA objetivo", "pga_aggregate_target"),
        ("Memoria objetivo", "memory_target"),
    ] {
        if let Some(v) = get(&params, name).filter(|v| *v > 0.0) {
            snap.info
                .push((label.into(), format!("{:.0} MB", v / 1_048_576.0)));
        }
    }

    let metric = pairs(&g.probe(c, "las métricas (V$SYSMETRIC)", SYSMETRIC));
    let stat = pairs(&g.probe(c, "las estadísticas (V$SYSSTAT)", SYSSTAT));
    let os = pairs(&g.probe(c, "el sistema operativo (V$OSSTAT)", OSSTAT));
    let sga = pairs(&g.probe(c, "la SGA (V$SGAINFO)", SGA));
    let pga = pairs(&g.probe(c, "la PGA (V$PGASTAT)", PGA));
    let counts = g
        .probe(c, "las sesiones (V$SESSION)", SESSION_COUNTS)
        .and_then(|r| r.into_iter().next());
    let locks = g
        .probe(c, "los bloqueos (V$LOCK)", LOCK_COUNTS)
        .and_then(|r| r.into_iter().next());
    let storage = g
        .probe(c, "los archivos de datos (DBA_DATA_FILES)", STORAGE)
        .and_then(|r| r.into_iter().next());
    // CPU: V$SYSMETRIC's host figure, or the V$OSSTAT difference.
    let os_now = get(&os, "BUSY_TIME").zip(get(&os, "IDLE_TIME"));
    let host_cpu = get(&metric, "Host CPU Utilization (%)").or_else(|| match (*last_os, os_now) {
        (Some((b0, i0)), Some((b1, i1))) if (b1 - b0) + (i1 - i0) > 0.0 => {
            Some((b1 - b0) / ((b1 - b0) + (i1 - i0)) * 100.0)
        }
        _ => None,
    });
    *last_os = os_now;

    // Data Guard.
    let dg = g.probe(c, "Data Guard (V$DATAGUARD_STATS)", DATAGUARD);
    let lag = dg
        .iter()
        .flatten()
        .filter(|r| r.first().and_then(txt).is_some_and(|n| n.ends_with("lag")))
        .filter_map(|r| r.get(1).and_then(txt).and_then(|v| dg_lag(&v)))
        .reduce(f64::max);
    snap.metrics = metrics(&Readings {
        params,
        metric,
        stat,
        os,
        sga,
        pga,
        counts,
        locks,
        storage,
        inst,
        host_cpu,
        lag,
    });

    let tables = [
        table(
            "sessions",
            "Sesiones",
            SESSION_COLS,
            g.probe(c, "las sesiones (V$SESSION)", SESSIONS),
        ),
        table(
            "queries",
            "Consultas en curso",
            QUERY_COLS,
            g.probe(c, "las consultas en curso", QUERIES),
        ),
        table(
            "locks",
            "Bloqueos / esperas",
            BLOCKED_COLS,
            g.probe(c, "las sesiones bloqueadas", BLOCKED),
        ),
        table(
            "waits",
            "Esperas principales (desde el inicio)",
            WAIT_COLS,
            g.probe(c, "las esperas (V$SYSTEM_EVENT)", WAITS),
        ),
        table(
            "tablespaces",
            "Tablespaces",
            TABLESPACE_COLS,
            g.probe(c, "los tablespaces", TABLESPACES),
        ),
        table(
            "databases",
            "Bases conectables (PDB)",
            &["PDB", "Modo", "Tamaño (MB)", "Restringida"],
            g.probe(c, "las PDB (V$PDBS)", PDBS),
        )
        .filter(|t| !t.rows.is_empty()),
        table(
            "top_objects",
            "Segmentos más grandes",
            &["Segmento", "Tipo", "Tamaño (MB)", "Tablespace"],
            g.probe(c, "los segmentos (DBA_SEGMENTS)", TOP_SEGMENTS),
        ),
        table(
            "replication",
            "Data Guard",
            &["Métrica", "Valor", "Unidad", "Calculado"],
            dg,
        )
        .filter(|t| !t.rows.is_empty()),
        table(
            "archive_dests",
            "Destinos de archivado remotos",
            &[
                "Destino",
                "Estado",
                "Tipo",
                "Modo de la base",
                "Recuperación",
                "Brecha",
            ],
            g.probe(c, "los destinos de archivado", ARCHIVE_DESTS),
        )
        .filter(|t| !t.rows.is_empty()),
    ];
    snap.tables.extend(tables.into_iter().flatten());
    if host_cpu.is_none() && os_now.is_some() {
        snap.notes.push("El uso de CPU del equipo aparece desde la segunda lectura (se calcula entre dos muestras de V$OSSTAT).".into());
    }
    if !g.denied.is_empty() {
        snap.notes.push(format!(
            "Sin SELECT_CATALOG_ROLE (o SELECT ANY DICTIONARY) no se ven: {}.",
            g.denied.join(", ")
        ));
    }
    snap.notes.extend(g.notes);
    snap
}

/// What the snapshot read, as plain values: [`metrics`] builds the figures
/// from it, so they can be checked without a server.
#[derive(Default)]
struct Readings {
    /// V$PARAMETER (numbers only).
    params: Vec<(String, f64)>,
    /// V$SYSMETRIC, the 60 s interval.
    metric: Vec<(String, f64)>,
    /// V$SYSSTAT running totals.
    stat: Vec<(String, f64)>,
    /// V$OSSTAT.
    os: Vec<(String, f64)>,
    /// V$SGAINFO, plus "SGA total" from V$SGASTAT.
    sga: Vec<(String, f64)>,
    /// V$PGASTAT.
    pga: Vec<(String, f64)>,
    /// V$SESSION: user sessions, active ones, blocked ones.
    counts: Option<Vec<Value>>,
    /// V$LOCK: locks, requests waiting.
    locks: Option<Vec<Value>>,
    /// Data files: allocated, free, maximum (bytes).
    storage: Option<Vec<Value>>,
    /// V$INSTANCE / V$DATABASE (uptime at 4).
    inst: Option<Vec<Value>>,
    /// Host CPU %: V$SYSMETRIC, or two V$OSSTAT readings.
    host_cpu: Option<f64>,
    /// Data Guard's largest lag, seconds.
    lag: Option<f64>,
}

/// The headline figures, in display order, each with what it measures and
/// where it comes from.
fn metrics(r: &Readings) -> Vec<Metric> {
    let at = |v: &Option<Vec<Value>>, i: usize| v.as_ref().and_then(|v| v.get(i)).and_then(num);
    let mut m = Vec::new();
    m.push(
        Metric::new("cpu", "CPU del servidor", "CPU", U::Percent, r.host_cpu)
            .max(Some(100.0))
            .help("Uso de CPU de todo el equipo, de Oracle y de los demás procesos: el promedio del último minuto o, en una PDB (donde V$SYSMETRIC no lo informa), el uso entre dos lecturas de V$OSSTAT. Cerca de 100 % sostenido, los procesos hacen cola para usar CPU.")
            .source("V$SYSMETRIC: 'Host CPU Utilization (%)' (60 s interval); else V$OSSTAT: Δ BUSY_TIME / Δ (BUSY_TIME + IDLE_TIME) between two snapshots"),
    );
    // Centiseconds of CPU spread over the CPUs the instance may use
    // (cpu_count, else the host's): the rate is the % of all of them, so
    // 100 % is every CPU busy with this database, never "500 %" for five
    // busy cores. Unknown CPU count: not shown rather than per core.
    let cpus = get(&r.params, "cpu_count")
        .or_else(|| get(&r.os, "NUM_CPUS"))
        .filter(|n| *n > 0.0);
    m.push(
        Metric::new(
            "cpu_time",
            "CPU de la base",
            "CPU",
            U::Percent,
            get(&r.stat, "CPU used by this session")
                .zip(cpus)
                .map(|(cs, n)| cs / n),
        )
        .max(Some(100.0))
        .counter()
        .help("Tiempo de CPU que usaron las sesiones de la base, repartido entre las CPU que puede usar la instancia (cpu_count o, si no está, las del equipo). 100 % equivale a todas esas CPU ocupadas con esta base; cerca de ese valor, las consultas esperan CPU.")
        .source("V$SYSSTAT: 'CPU used by this session' (centiseconds) / V$PARAMETER cpu_count (else V$OSSTAT NUM_CPUS)"),
    );
    m.push(
        Metric::new("load", "Carga del equipo", "CPU", U::Count, get(&r.os, "LOAD"))
            .help("Procesos del sistema operativo ejecutando o esperando CPU: la carga promedio del equipo. Si se mantiene por encima de la cantidad de CPU, el equipo está saturado.")
            .source("V$OSSTAT: LOAD"),
    );
    m.push(
        Metric::new(
            "avg_active_sessions",
            "Sesiones activas promedio",
            "CPU",
            U::Count,
            get(&r.metric, "Average Active Sessions"),
        )
        .help("Cuántas sesiones estuvieron en promedio trabajando o esperando algo que no es el cliente durante el último minuto (DB time por segundo). Si supera la cantidad de CPU, hay sesiones esperando CPU, disco o bloqueos. En una PDB queda vacío: Oracle no informa V$SYSMETRIC ahí.")
        .source("V$SYSMETRIC: 'Average Active Sessions' (60 s interval)"),
    );

    let sga_total = get(&r.sga, "SGA total");
    let pga_alloc = get(&r.pga, "total PGA allocated");
    let used = match (sga_total, pga_alloc) {
        (Some(s), Some(p)) => Some(s + p),
        (s, p) => s.or(p),
    };
    let target = get(&r.params, "memory_target")
        .filter(|v| *v > 0.0)
        .or_else(|| {
            let s = get(&r.params, "sga_target")
                .filter(|v| *v > 0.0)
                .or(get(&r.sga, "Maximum SGA Size"))?;
            Some(s + get(&r.params, "pga_aggregate_target").unwrap_or(0.0))
        });
    m.push(
        Metric::new("mem_used", "Memoria (SGA + PGA)", "Memoria", U::Bytes, used)
            .max(target)
            .help("Memoria de la instancia: la SGA completa más la PGA asignada a los procesos. El máximo es memory_target o, si no está, sga_target (o el tamaño máximo de la SGA) más pga_aggregate_target, que la PGA puede superar.")
            .source("SUM(V$SGASTAT.bytes) + V$PGASTAT: 'total PGA allocated'"),
    );
    m.push(
        Metric::new(
            "mem_cache",
            "Caché de datos (buffer cache)",
            "Memoria",
            U::Bytes,
            get(&r.sga, "Buffer Cache Size"),
        )
        .help("Tamaño de la buffer cache, la parte de la SGA que guarda bloques de datos. Con la gestión automática de memoria, Oracle la agranda o la achica según la carga.")
        .source("V$SGAINFO: 'Buffer Cache Size'"),
    );
    m.push(
        Metric::new("pga", "PGA asignada", "Memoria", U::Bytes, pga_alloc)
            .max(get(&r.params, "pga_aggregate_limit").filter(|v| *v > 0.0))
            .help("Memoria privada de los procesos (ordenamientos, hash joins, cursores) asignada ahora. El máximo es pga_aggregate_limit: al llegar, Oracle corta las llamadas que más memoria usan (ORA-04036).")
            .source("V$PGASTAT: 'total PGA allocated'"),
    );
    let phys = get(&r.os, "PHYSICAL_MEMORY_BYTES");
    m.push(
        Metric::new(
            "mem_host",
            "Memoria del equipo en uso",
            "Memoria",
            U::Bytes,
            phys.zip(get(&r.os, "FREE_MEMORY_BYTES"))
                .map(|(t, f)| t - f),
        )
        .max(phys)
        .help("Memoria física del equipo menos la libre según el sistema operativo, para todos los procesos. Puede incluir la caché de archivos del sistema, así que verla casi llena no siempre es falta de memoria.")
        .source("V$OSSTAT: PHYSICAL_MEMORY_BYTES - FREE_MEMORY_BYTES"),
    );

    m.push(
        Metric::new(
            "connections",
            "Sesiones",
            "Conexiones",
            U::Count,
            at(&r.counts, 0),
        )
        .max(get(&r.params, "sessions"))
        .help("Sesiones de usuario abiertas, sin los procesos de fondo. El máximo es el parámetro sessions, que también cuenta los de fondo: al llegar, las conexiones nuevas fallan con ORA-00018.")
        .source("V$SESSION: COUNT(*) WHERE type = 'USER'"),
    );
    m.push(
        Metric::new(
            "active_sessions",
            "Sesiones activas",
            "Conexiones",
            U::Count,
            at(&r.counts, 1),
        )
        .help("Sesiones de usuario en medio de una llamada (estado ACTIVE), trabajando o esperando disco o bloqueos, sin contar la de DBine.")
        .source("V$SESSION: COUNT(*) WHERE type = 'USER' AND status = 'ACTIVE', without DBine's own"),
    );
    m.push(
        Metric::new(
            "logons",
            "Inicios de sesión",
            "Conexiones",
            U::Count,
            get(&r.stat, "logons cumulative"),
        )
        .counter()
        .help("Conexiones nuevas a la base. Muchas por segundo de forma sostenida suelen ser una aplicación que se conecta en cada operación en vez de usar un pool.")
        .source("V$SYSSTAT: 'logons cumulative'"),
    );

    m.push(
        Metric::new(
            "queries",
            "Ejecuciones",
            "Actividad",
            U::Count,
            get(&r.stat, "execute count"),
        )
        .counter()
        .help("Ejecuciones de sentencias SQL, incluidas las recursivas que Oracle lanza por su cuenta (diccionario, PL/SQL). Es la medida habitual de carga.")
        .source("V$SYSSTAT: 'execute count'"),
    );
    let tx =
        get(&r.stat, "user commits").map(|c| c + get(&r.stat, "user rollbacks").unwrap_or(0.0));
    m.push(
        Metric::new("transactions", "Transacciones", "Actividad", U::Count, tx)
            .counter()
            .help("Transacciones de los usuarios terminadas con commit o con rollback.")
            .source("V$SYSSTAT: 'user commits' + 'user rollbacks'"),
    );
    m.push(
        Metric::new(
            "logical_reads",
            "Lecturas lógicas",
            "Actividad",
            U::Count,
            get(&r.stat, "session logical reads"),
        )
        .counter()
        .help("Bloques leídos de la buffer cache (consistent gets y db block gets): el trabajo principal de las consultas. Un salto suele ser un plan que recorre mucho más de lo necesario.")
        .source("V$SYSSTAT: 'session logical reads'"),
    );
    m.push(
        Metric::new(
            "hard_parses",
            "Análisis completos (hard parse)",
            "Actividad",
            U::Count,
            get(&r.stat, "parse count (hard)"),
        )
        .counter()
        .help("Sentencias que Oracle tuvo que analizar y optimizar desde cero porque no estaban en la shared pool. Muchas por segundo suelen ser SQL con literales en vez de variables bind, y gastan CPU.")
        .source("V$SYSSTAT: 'parse count (hard)'"),
    );
    m.push(
        Metric::new(
            "rows_read",
            "Filas leídas por recorridos",
            "Actividad",
            U::Count,
            get(&r.stat, "table scan rows gotten"),
        )
        .counter()
        .help("Filas leídas recorriendo tablas completas (full table scan). Si crece, suele haber consultas sin un índice adecuado.")
        .source("V$SYSSTAT: 'table scan rows gotten'"),
    );
    m.push(
        Metric::new(
            "redo",
            "Redo generado",
            "Actividad",
            U::Bytes,
            get(&r.stat, "redo size"),
        )
        .counter()
        .help("Bytes de redo que generaron los cambios: el volumen de escritura de la base. De él dependen cuánto crecen los archive logs y cuánto envía Data Guard a la standby.")
        .source("V$SYSSTAT: 'redo size'"),
    );

    m.push(
        Metric::new(
            "net_in",
            "Red entrante",
            "Red",
            U::Bytes,
            get(&r.stat, "bytes received via SQL*Net from client"),
        )
        .counter()
        .help("Bytes que la base recibió de los clientes por SQL*Net: sentencias, parámetros y datos que se cargan.")
        .source("V$SYSSTAT: 'bytes received via SQL*Net from client'"),
    );
    m.push(
        Metric::new(
            "net_out",
            "Red saliente",
            "Red",
            U::Bytes,
            get(&r.stat, "bytes sent via SQL*Net to client"),
        )
        .counter()
        .help("Bytes que la base envió a los clientes por SQL*Net, sobre todo filas de resultados.")
        .source("V$SYSSTAT: 'bytes sent via SQL*Net to client'"),
    );
    m.push(
        Metric::new(
            "disk_read",
            "Lectura en disco",
            "Disco",
            U::Bytes,
            get(&r.stat, "physical read total bytes"),
        )
        .counter()
        .help("Bytes leídos de disco por toda la instancia: consultas, copias de seguridad, recuperación y demás utilidades.")
        .source("V$SYSSTAT: 'physical read total bytes'"),
    );
    m.push(
        Metric::new(
            "disk_write",
            "Escritura en disco",
            "Disco",
            U::Bytes,
            get(&r.stat, "physical write total bytes"),
        )
        .counter()
        .help("Bytes escritos a disco por toda la instancia, incluidas las copias de seguridad, la recuperación y demás utilidades.")
        .source("V$SYSSTAT: 'physical write total bytes'"),
    );

    // V$SYSMETRIC is empty in a PDB: the ratio since startup instead.
    let since_start = match (
        get(&r.stat, "physical reads cache"),
        get(&r.stat, "consistent gets from cache"),
        get(&r.stat, "db block gets from cache"),
    ) {
        (Some(p), Some(c), Some(d)) if c + d > 0.0 => {
            Some(((1.0 - p / (c + d)) * 100.0).clamp(0.0, 100.0))
        }
        _ => None,
    };
    let hit = get(&r.metric, "Buffer Cache Hit Ratio").or(since_start);
    m.push(
        Metric::new("cache_hit", "Aciertos de caché", "Caché", U::Percent, hit)
            .max(Some(100.0))
            .help("Porcentaje de lecturas de bloques resueltas en la buffer cache sin ir a disco, en el último minuto (en una PDB, desde el arranque). Un descenso sostenido indica una caché chica para la carga o consultas que recorren tablas enormes.")
            .source("V$SYSMETRIC: 'Buffer Cache Hit Ratio' (60 s interval); else V$SYSSTAT: 1 - 'physical reads cache' / ('consistent gets from cache' + 'db block gets from cache')"),
    );
    m.push(
        Metric::new(
            "library_hit",
            "Aciertos de library cache",
            "Caché",
            U::Percent,
            get(&r.metric, "Library Cache Hit Ratio"),
        )
        .max(Some(100.0))
        .help("Porcentaje de pedidos de SQL y PL/SQL que encontraron su versión ya analizada en la library cache, en el último minuto. Si baja, suele deberse a muchos hard parses o a una shared pool chica. En una PDB queda vacío: Oracle no informa V$SYSMETRIC ahí.")
        .source("V$SYSMETRIC: 'Library Cache Hit Ratio' (60 s interval)"),
    );

    let (total, free, max) = (at(&r.storage, 0), at(&r.storage, 1), at(&r.storage, 2));
    m.push(
        Metric::new(
            "storage_used",
            "Espacio usado",
            "Almacenamiento",
            U::Bytes,
            total.map(|t| t - free.unwrap_or(0.0)),
        )
        .max(max)
        .help("Espacio ocupado dentro de los archivos de datos, sin los temporales. El máximo es hasta dónde pueden crecer los archivos con autoextensión: al llegar, las inserciones fallan por falta de espacio.")
        .source("SUM(DBA_DATA_FILES.bytes) - SUM(DBA_FREE_SPACE.bytes); max SUM(maxbytes if autoextensible, else bytes)"),
    );
    m.push(
        Metric::new(
            "storage_allocated",
            "Espacio asignado",
            "Almacenamiento",
            U::Bytes,
            total,
        )
        .max(max)
        .help("Tamaño actual de los archivos de datos en disco, ocupados o no, sin los temporales. Crece cuando un archivo con autoextensión se agranda.")
        .source("SUM(DBA_DATA_FILES.bytes)"),
    );

    m.push(
        Metric::new(
            "locks_waiting",
            "Sesiones bloqueadas",
            "Bloqueos",
            U::Count,
            at(&r.counts, 2).or(at(&r.locks, 1)),
        )
        .help("Sesiones de usuario que esperan un recurso que tiene otra sesión. Si se mantienen, la tabla de bloqueos muestra quién encabeza la cadena.")
        .source("V$SESSION: COUNT(*) WHERE type = 'USER' AND blocking_session IS NOT NULL; else V$LOCK: COUNT(*) WHERE request > 0"),
    );
    m.push(
        Metric::new(
            "locks_held",
            "Bloqueos (V$LOCK)",
            "Bloqueos",
            U::Count,
            at(&r.locks, 0),
        )
        .help("Bloqueos y encolamientos tenidos o pedidos en este momento, incluidos los internos de Oracle. Sirve como tendencia: las esperas reales están en Sesiones bloqueadas.")
        .source("V$LOCK: COUNT(*)"),
    );
    m.push(
        Metric::new(
            "deadlocks",
            "Deadlocks",
            "Bloqueos",
            U::Count,
            get(&r.stat, "enqueue deadlocks"),
        )
        .counter()
        .help("Deadlocks entre sesiones que Oracle detectó y resolvió devolviendo ORA-00060 a una de ellas. Si aparecen seguido, hay transacciones que toman los mismos recursos en distinto orden.")
        .source("V$SYSSTAT: 'enqueue deadlocks'"),
    );

    m.push(
        Metric::new(
            "replication_lag",
            "Retraso de réplica (Data Guard)",
            "Replicación",
            U::Seconds,
            r.lag,
        )
        .help("En una standby de Data Guard, el mayor entre el retraso de transporte y el de aplicación: cuánto le falta para estar al día con la primaria. Vacío sin Data Guard.")
        .source("V$DATAGUARD_STATS: largest value WHERE name LIKE '%lag' (transport lag, apply lag)"),
    );
    m.push(
        Metric::new(
            "uptime",
            "Tiempo activo",
            "Servidor",
            U::Seconds,
            at(&r.inst, 4),
        )
        .help("Tiempo desde que arrancó la instancia.")
        .source("V$INSTANCE: (SYSDATE - startup_time) × 86400"),
    );
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_guard_lags() {
        assert_eq!(dg_lag("+00 00:00:05"), Some(5.0));
        assert_eq!(dg_lag("+01 02:00:00"), Some(93_600.0));
        assert_eq!(dg_lag(""), None);
    }

    #[test]
    fn pairs_skip_nulls() {
        let rows = Some(vec![
            vec![Value::from("a"), Value::from(1)],
            vec![Value::from("b"), Value::Null],
            vec![Value::from("c"), Value::from("2.5")],
        ]);
        assert_eq!(
            pairs(&rows),
            vec![("a".to_string(), 1.0), ("c".to_string(), 2.5)]
        );
    }

    /// Every figure says what it measures and where it comes from, even
    /// when the server reported nothing (all of them are always there).
    #[test]
    fn every_metric_has_help_and_source() {
        let m = metrics(&Readings::default());
        assert_eq!(m.len(), 30);
        let mut keys: Vec<&str> = m.iter().map(|m| m.key.as_str()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), m.len(), "duplicate keys");
        for x in &m {
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
        // A help is one literal: the UI translates it as one message, and a
        // format! would leave a fragment translated on its own.
        assert!(!include_str!("monitor.rs").contains(concat!(".help(&", "format!")));
    }

    /// The database CPU is centiseconds over the CPUs the instance may use:
    /// its rate is the share of all of them (100 = all busy).
    #[test]
    fn database_cpu_is_a_share_of_every_cpu() {
        let cpu = |r: Readings| {
            metrics(&r)
                .into_iter()
                .find(|m| m.key == "cpu_time")
                .unwrap()
        };
        let stat = vec![("CPU used by this session".to_string(), 1000.0)];
        let m = cpu(Readings {
            params: vec![("cpu_count".into(), 4.0)],
            os: vec![("NUM_CPUS".into(), 8.0)],
            stat: stat.clone(),
            ..Default::default()
        });
        assert_eq!(m.value, Some(250.0));
        assert!(m.counter && m.max == Some(100.0) && m.unit == U::Percent);
        // Without cpu_count, the host's CPUs; without either, nothing.
        let m = cpu(Readings {
            os: vec![("NUM_CPUS".into(), 8.0)],
            stat: stat.clone(),
            ..Default::default()
        });
        assert_eq!(m.value, Some(125.0));
        assert_eq!(
            cpu(Readings {
                stat,
                ..Default::default()
            })
            .value,
            None
        );
    }
}
