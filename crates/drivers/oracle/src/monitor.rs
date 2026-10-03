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
pub(crate) fn rows(c: &Connection, sql: &str) -> std::result::Result<Vec<Vec<Value>>, oracledb::Error> {
    rows_max(c, sql, 200)
}

/// [`rows`], keeping at most `max`.
pub(crate) fn rows_max(c: &Connection, sql: &str, max: usize) -> std::result::Result<Vec<Vec<Value>>, oracledb::Error> {
    // Out of the statement cache: the thin client caches a statement whose
    // parse failed (ORA-00942 without the grant) and the next snapshot
    // would get ORA-01003 instead of the real error.
    let stmt = c.statement(sql).map(|b| b.exclude_from_cache()).and_then(|b| b.build())?;
    let cursor = stmt.query(&[])?;
    let types: Vec<&'static oracledb::DbType> = cursor.columns().iter().map(|m| m.db_type()).collect();
    let mut out = Vec::new();
    for row in cursor {
        let row = row?;
        out.push(types.iter().enumerate().map(|(i, t)| cell(&row, i, t)).collect());
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
                    Some(942 | 1031 | 1039) if !self.denied.contains(&what) => self.denied.push(what),
                    Some(942 | 1031 | 1039) => {}
                    _ => self.notes.push(format!("No se pudo leer {what}: {}", e.to_string().lines().next().unwrap_or_default())),
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

fn table(key: &str, title: &str, cols: &[&str], rows: Option<Vec<Vec<Value>>>) -> Option<MonitorTable> {
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

const LOCK_COUNTS: &str = "SELECT COUNT(*), SUM(CASE WHEN request > 0 THEN 1 ELSE 0 END) FROM v$lock";

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

const WAITS: &str = "SELECT * FROM (SELECT event, wait_class, total_waits, ROUND(time_waited_micro / 1000),
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

const TABLESPACE_COLS: &[&str] = &["Tablespace", "Tamaño (MB)", "Usado (MB)", "Libre (MB)", "Máximo (MB)", "% del máximo"];

const PDBS: &str = "SELECT name, open_mode, ROUND(total_size / 1048576, 1), restricted FROM v$pdbs ORDER BY con_id";

const TOP_SEGMENTS: &str = "SELECT * FROM (SELECT owner || '.' || segment_name, segment_type, ROUND(bytes / 1048576, 2), tablespace_name
  FROM dba_segments
 ORDER BY bytes DESC) WHERE ROWNUM <= 20";

const DATAGUARD: &str = "SELECT name, value, unit, time_computed FROM v$dataguard_stats";

const ARCHIVE_DESTS: &str = "SELECT dest_name, status, type, database_mode, recovery_mode, gap_status
  FROM v$archive_dest_status WHERE status <> 'INACTIVE' AND type <> 'LOCAL'";

/// `+00 00:00:05` (a Data Guard lag) in seconds.
fn dg_lag(s: &str) -> Option<f64> {
    let s = s.trim().trim_start_matches('+');
    let (days, time) = s.split_once(' ')?;
    let t: Vec<f64> = time.split(':').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    (t.len() == 3).then(|| days.parse::<f64>().unwrap_or(0.0) * 86_400.0 + t[0] * 3600.0 + t[1] * 60.0 + t[2])
}

/// The monitor snapshot. `last_os` keeps the previous `(BUSY_TIME,
/// IDLE_TIME)` of V$OSSTAT: when V$SYSMETRIC has no host CPU figure (a
/// PDB), it's the difference between two snapshots.
pub fn snapshot(c: &Connection, last_os: &mut Option<(f64, f64)>) -> MonitorSnapshot {
    let mut snap = MonitorSnapshot::default();
    let mut g = Gaps::default();

    let inst = g.probe(c, "la instancia (V$INSTANCE)", INSTANCE).and_then(|r| r.into_iter().next());
    if let Some(r) = &inst {
        let s = |i: usize| r.get(i).and_then(txt);
        for (label, i) in [("Instancia", 0), ("Equipo", 1), ("Versión", 2), ("Estado", 3), ("Base", 5), ("Rol", 6), ("Modo", 7), ("Archivado", 8), ("Contenedor", 9), ("Zona horaria de la base", 11)] {
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
    let params = pairs(&g.probe(c, "los parámetros (V$PARAMETER)", PARAMETERS).map(|rows| {
        rows.into_iter().map(|r| r.into_iter().map(|v| if let Value::String(s) = &v { dbine_driver::monitor::num(s).map_or(v, Value::from) } else { v }).collect()).collect()
    }));
    for (label, name) in [("Sesiones máximas", "sessions"), ("Procesos máximos", "processes"), ("CPU (cpu_count)", "cpu_count")] {
        if let Some(v) = get(&params, name) {
            snap.info.push((label.into(), format!("{v}")));
        }
    }
    for (label, name) in [("SGA objetivo", "sga_target"), ("PGA objetivo", "pga_aggregate_target"), ("Memoria objetivo", "memory_target")] {
        if let Some(v) = get(&params, name).filter(|v| *v > 0.0) {
            snap.info.push((label.into(), format!("{:.0} MB", v / 1_048_576.0)));
        }
    }

    let metric = pairs(&g.probe(c, "las métricas (V$SYSMETRIC)", SYSMETRIC));
    let stat = pairs(&g.probe(c, "las estadísticas (V$SYSSTAT)", SYSSTAT));
    let os = pairs(&g.probe(c, "el sistema operativo (V$OSSTAT)", OSSTAT));
    let sga = pairs(&g.probe(c, "la SGA (V$SGAINFO)", SGA));
    let pga = pairs(&g.probe(c, "la PGA (V$PGASTAT)", PGA));
    let counts = g.probe(c, "las sesiones (V$SESSION)", SESSION_COUNTS).and_then(|r| r.into_iter().next());
    let locks = g.probe(c, "los bloqueos (V$LOCK)", LOCK_COUNTS).and_then(|r| r.into_iter().next());
    let storage = g.probe(c, "los archivos de datos (DBA_DATA_FILES)", STORAGE).and_then(|r| r.into_iter().next());
    let at = |r: &Option<Vec<Value>>, i: usize| r.as_ref().and_then(|r| r.get(i)).and_then(num);

    // CPU: V$SYSMETRIC's host figure, or the V$OSSTAT difference.
    let os_now = get(&os, "BUSY_TIME").zip(get(&os, "IDLE_TIME"));
    let host_cpu = get(&metric, "Host CPU Utilization (%)").or_else(|| match (*last_os, os_now) {
        (Some((b0, i0)), Some((b1, i1))) if (b1 - b0) + (i1 - i0) > 0.0 => Some((b1 - b0) / ((b1 - b0) + (i1 - i0)) * 100.0),
        _ => None,
    });
    *last_os = os_now;
    let m = &mut snap.metrics;
    m.push(Metric::new("cpu", "CPU del servidor", "CPU", U::Percent, host_cpu).max(Some(100.0)));
    // Centiseconds of CPU: × 100 / 100.
    m.push(Metric::new("cpu_time", "CPU de la base", "CPU", U::Percent, get(&stat, "CPU used by this session")).counter());
    m.push(Metric::new("load", "Carga del equipo", "CPU", U::Count, get(&os, "LOAD")));
    m.push(Metric::new("avg_active_sessions", "Sesiones activas promedio", "CPU", U::Count, get(&metric, "Average Active Sessions")));

    let sga_total = get(&sga, "SGA total");
    let pga_alloc = get(&pga, "total PGA allocated");
    let used = match (sga_total, pga_alloc) {
        (Some(s), Some(p)) => Some(s + p),
        (s, p) => s.or(p),
    };
    let target = get(&params, "memory_target").filter(|v| *v > 0.0).or_else(|| {
        let s = get(&params, "sga_target").filter(|v| *v > 0.0).or(get(&sga, "Maximum SGA Size"))?;
        Some(s + get(&params, "pga_aggregate_target").unwrap_or(0.0))
    });
    m.push(Metric::new("mem_used", "Memoria (SGA + PGA)", "Memoria", U::Bytes, used).max(target));
    m.push(Metric::new("mem_cache", "Caché de datos (buffer cache)", "Memoria", U::Bytes, get(&sga, "Buffer Cache Size")));
    m.push(Metric::new("pga", "PGA asignada", "Memoria", U::Bytes, pga_alloc).max(get(&params, "pga_aggregate_limit").filter(|v| *v > 0.0)));
    let phys = get(&os, "PHYSICAL_MEMORY_BYTES");
    m.push(Metric::new("mem_host", "Memoria del equipo en uso", "Memoria", U::Bytes, phys.zip(get(&os, "FREE_MEMORY_BYTES")).map(|(t, f)| t - f)).max(phys));

    m.push(Metric::new("connections", "Sesiones", "Conexiones", U::Count, at(&counts, 0)).max(get(&params, "sessions")));
    m.push(Metric::new("active_sessions", "Sesiones activas", "Conexiones", U::Count, at(&counts, 1)));
    m.push(Metric::new("logons", "Inicios de sesión", "Conexiones", U::Count, get(&stat, "logons cumulative")).counter());

    m.push(Metric::new("queries", "Ejecuciones", "Actividad", U::Count, get(&stat, "execute count")).counter());
    let tx = get(&stat, "user commits").map(|c| c + get(&stat, "user rollbacks").unwrap_or(0.0));
    m.push(Metric::new("transactions", "Transacciones", "Actividad", U::Count, tx).counter());
    m.push(Metric::new("logical_reads", "Lecturas lógicas", "Actividad", U::Count, get(&stat, "session logical reads")).counter());
    m.push(Metric::new("hard_parses", "Análisis completos (hard parse)", "Actividad", U::Count, get(&stat, "parse count (hard)")).counter());
    m.push(Metric::new("rows_read", "Filas leídas por recorridos", "Actividad", U::Count, get(&stat, "table scan rows gotten")).counter());
    m.push(Metric::new("redo", "Redo generado", "Actividad", U::Bytes, get(&stat, "redo size")).counter());

    m.push(Metric::new("net_in", "Red entrante", "Red", U::Bytes, get(&stat, "bytes received via SQL*Net from client")).counter());
    m.push(Metric::new("net_out", "Red saliente", "Red", U::Bytes, get(&stat, "bytes sent via SQL*Net to client")).counter());
    m.push(Metric::new("disk_read", "Lectura en disco", "Disco", U::Bytes, get(&stat, "physical read total bytes")).counter());
    m.push(Metric::new("disk_write", "Escritura en disco", "Disco", U::Bytes, get(&stat, "physical write total bytes")).counter());

    // V$SYSMETRIC is empty in a PDB: the ratio since startup instead.
    let since_start = match (get(&stat, "physical reads cache"), get(&stat, "consistent gets from cache"), get(&stat, "db block gets from cache")) {
        (Some(p), Some(c), Some(d)) if c + d > 0.0 => Some(((1.0 - p / (c + d)) * 100.0).clamp(0.0, 100.0)),
        _ => None,
    };
    let hit = get(&metric, "Buffer Cache Hit Ratio").or(since_start);
    m.push(Metric::new("cache_hit", "Aciertos de caché", "Caché", U::Percent, hit).max(Some(100.0)));
    m.push(Metric::new("library_hit", "Aciertos de library cache", "Caché", U::Percent, get(&metric, "Library Cache Hit Ratio")).max(Some(100.0)));

    let (total, free, max) = (at(&storage, 0), at(&storage, 1), at(&storage, 2));
    m.push(Metric::new("storage_used", "Espacio usado", "Almacenamiento", U::Bytes, total.map(|t| t - free.unwrap_or(0.0))).max(max));
    m.push(Metric::new("storage_allocated", "Espacio asignado", "Almacenamiento", U::Bytes, total).max(max));

    m.push(Metric::new("locks_waiting", "Sesiones bloqueadas", "Bloqueos", U::Count, at(&counts, 2).or(at(&locks, 1))));
    m.push(Metric::new("locks_held", "Bloqueos (V$LOCK)", "Bloqueos", U::Count, at(&locks, 0)));
    m.push(Metric::new("deadlocks", "Deadlocks", "Bloqueos", U::Count, get(&stat, "enqueue deadlocks")).counter());

    // Data Guard.
    let dg = g.probe(c, "Data Guard (V$DATAGUARD_STATS)", DATAGUARD);
    let lag = dg.iter().flatten().filter(|r| r.first().and_then(txt).is_some_and(|n| n.ends_with("lag"))).filter_map(|r| r.get(1).and_then(txt).and_then(|v| dg_lag(&v))).reduce(f64::max);
    snap.metrics.push(Metric::new("replication_lag", "Retraso de réplica (Data Guard)", "Replicación", U::Seconds, lag));
    snap.metrics.push(Metric::new("uptime", "Tiempo activo", "Servidor", U::Seconds, at(&inst, 4)));

    let tables = [
        table("sessions", "Sesiones", SESSION_COLS, g.probe(c, "las sesiones (V$SESSION)", SESSIONS)),
        table("queries", "Consultas en curso", QUERY_COLS, g.probe(c, "las consultas en curso", QUERIES)),
        table("locks", "Bloqueos / esperas", BLOCKED_COLS, g.probe(c, "las sesiones bloqueadas", BLOCKED)),
        table("waits", "Esperas principales (desde el inicio)", WAIT_COLS, g.probe(c, "las esperas (V$SYSTEM_EVENT)", WAITS)),
        table("tablespaces", "Tablespaces", TABLESPACE_COLS, g.probe(c, "los tablespaces", TABLESPACES)),
        table("databases", "Bases conectables (PDB)", &["PDB", "Modo", "Tamaño (MB)", "Restringida"], g.probe(c, "las PDB (V$PDBS)", PDBS)).filter(|t| !t.rows.is_empty()),
        table("top_objects", "Segmentos más grandes", &["Segmento", "Tipo", "Tamaño (MB)", "Tablespace"], g.probe(c, "los segmentos (DBA_SEGMENTS)", TOP_SEGMENTS)),
        table("replication", "Data Guard", &["Métrica", "Valor", "Unidad", "Calculado"], dg).filter(|t| !t.rows.is_empty()),
        table("archive_dests", "Destinos de archivado remotos", &["Destino", "Estado", "Tipo", "Modo de la base", "Recuperación", "Brecha"], g.probe(c, "los destinos de archivado", ARCHIVE_DESTS)).filter(|t| !t.rows.is_empty()),
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
        let rows = Some(vec![vec![Value::from("a"), Value::from(1)], vec![Value::from("b"), Value::Null], vec![Value::from("c"), Value::from("2.5")]]);
        assert_eq!(pairs(&rows), vec![("a".to_string(), 1.0), ("c".to_string(), 2.5)]);
    }
}
