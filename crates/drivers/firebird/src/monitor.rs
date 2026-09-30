//! Server monitor from Firebird's monitoring tables (MON$DATABASE,
//! MON$ATTACHMENTS, MON$STATEMENTS, MON$TRANSACTIONS, MON$IO_STATS,
//! MON$RECORD_STATS, MON$MEMORY_USAGE, MON$TABLE_STATS).
//!
//! The MON$ snapshot is taken once per transaction, so every query runs in
//! a short read-only transaction of its own: fresh figures each time, and
//! the session's own transaction (autocommit off) is left alone. Users
//! without SYSDBA / RDB$ADMIN / MONITOR_ANY_ATTACHMENT only see their own
//! attachments.

use crate::{cell, message, Conn};
use dbine_driver::monitor::{Metric, MetricUnit as U, MonitorSnapshot, MonitorTable};
use rsfbclient_core::{
    Column, Dialect, FbError, FirebirdClientSqlOps, FreeStmtOp, SqlType, TrDataAccessMode,
    TrIsolationLevel, TrLockResolution, TrOp, TransactionConfiguration,
};

fn f(c: &Column) -> Option<f64> {
    match &c.value {
        SqlType::Integer(i) => Some(*i as f64),
        SqlType::Floating(x) => Some(*x),
        SqlType::Boolean(b) => Some(if *b { 1.0 } else { 0.0 }),
        SqlType::Text(s) => dbine_driver::monitor::num(s),
        _ => None,
    }
}

fn s(c: &Column) -> Option<String> {
    match &c.value {
        SqlType::Text(t) => Some(t.trim().to_string()).filter(|t| !t.is_empty()),
        SqlType::Integer(i) => Some(i.to_string()),
        SqlType::Null => None,
        other => Some(format!("{other:?}")),
    }
}

const DATABASE: &str = "
SELECT d.MON$PAGE_SIZE, d.MON$PAGES, d.MON$PAGE_BUFFERS, d.MON$NEXT_TRANSACTION, d.MON$OLDEST_ACTIVE,
       d.MON$OLDEST_TRANSACTION, d.MON$SWEEP_INTERVAL, d.MON$FORCED_WRITES, d.MON$READ_ONLY,
       d.MON$ODS_MAJOR || '.' || d.MON$ODS_MINOR, d.MON$SQL_DIALECT, TRIM(d.MON$DATABASE_NAME),
       io.MON$PAGE_READS, io.MON$PAGE_WRITES, io.MON$PAGE_FETCHES, io.MON$PAGE_MARKS,
       r.MON$RECORD_SEQ_READS + r.MON$RECORD_IDX_READS,
       r.MON$RECORD_INSERTS + r.MON$RECORD_UPDATES + r.MON$RECORD_DELETES,
       r.MON$RECORD_WAITS, r.MON$RECORD_CONFLICTS,
       r.MON$RECORD_BACKOUTS + r.MON$RECORD_PURGES + r.MON$RECORD_EXPUNGES,
       m.MON$MEMORY_USED, m.MON$MEMORY_ALLOCATED, d.MON$SHUTDOWN_MODE, d.MON$BACKUP_STATE
  FROM MON$DATABASE d
  LEFT JOIN MON$IO_STATS io ON io.MON$STAT_ID = d.MON$STAT_ID
  LEFT JOIN MON$RECORD_STATS r ON r.MON$STAT_ID = d.MON$STAT_ID
  LEFT JOIN MON$MEMORY_USAGE m ON m.MON$STAT_ID = d.MON$STAT_ID";

const ATTACHMENT_COUNTS: &str = "
SELECT COUNT(*),
       SUM(CASE WHEN MON$STATE = 1 AND MON$ATTACHMENT_ID <> CURRENT_CONNECTION THEN 1 ELSE 0 END),
       (SELECT DATEDIFF(SECOND FROM MIN(x.MON$TIMESTAMP) TO CURRENT_TIMESTAMP) FROM MON$ATTACHMENTS x
         WHERE x.MON$SYSTEM_FLAG = 1)
  FROM MON$ATTACHMENTS WHERE MON$SYSTEM_FLAG = 0";

const STATEMENT_COUNTS: &str = "
SELECT COUNT(*), (SELECT COUNT(*) FROM MON$TRANSACTIONS t
                   JOIN MON$ATTACHMENTS a ON a.MON$ATTACHMENT_ID = t.MON$ATTACHMENT_ID AND a.MON$SYSTEM_FLAG = 0),
       (SELECT DATEDIFF(SECOND FROM MIN(t.MON$TIMESTAMP) TO CURRENT_TIMESTAMP) FROM MON$TRANSACTIONS t
         WHERE t.MON$ATTACHMENT_ID <> CURRENT_CONNECTION)
  FROM MON$STATEMENTS WHERE MON$STATE = 1 AND MON$ATTACHMENT_ID <> CURRENT_CONNECTION";

/// Firebird 4+: replication role and the monitoring privilege.
const FB4: &str = "
SELECT d.MON$REPLICA_MODE, RDB$SYSTEM_PRIVILEGE(MONITOR_ANY_ATTACHMENT) FROM MON$DATABASE d";

/// Firebird 4+, SYSDBA / RDB$ADMIN only.
const CONFIG: &str = "
SELECT TRIM(RDB$CONFIG_NAME), TRIM(RDB$CONFIG_VALUE) FROM RDB$CONFIG
 WHERE RDB$CONFIG_NAME IN ('ServerMode', 'DefaultDbCachePages', 'TempCacheLimit', 'LockMemSize', 'WireCrypt')";

const SESSIONS: &str = "
SELECT FIRST 200 a.MON$ATTACHMENT_ID, TRIM(a.MON$USER), TRIM(a.MON$ROLE), TRIM(a.MON$REMOTE_ADDRESS),
       TRIM(a.MON$REMOTE_PROCESS), CASE a.MON$STATE WHEN 1 THEN 'activa' ELSE 'inactiva' END,
       DATEDIFF(SECOND FROM a.MON$TIMESTAMP TO CURRENT_TIMESTAMP), m.MON$MEMORY_USED,
       io.MON$PAGE_READS, io.MON$PAGE_FETCHES,
       (SELECT FIRST 1 CAST(SUBSTRING(st.MON$SQL_TEXT FROM 1 FOR 2000) AS VARCHAR(2000))
          FROM MON$STATEMENTS st WHERE st.MON$ATTACHMENT_ID = a.MON$ATTACHMENT_ID AND st.MON$STATE = 1)
  FROM MON$ATTACHMENTS a
  LEFT JOIN MON$MEMORY_USAGE m ON m.MON$STAT_ID = a.MON$STAT_ID
  LEFT JOIN MON$IO_STATS io ON io.MON$STAT_ID = a.MON$STAT_ID
 WHERE a.MON$SYSTEM_FLAG = 0 AND a.MON$ATTACHMENT_ID <> CURRENT_CONNECTION
 ORDER BY a.MON$STATE DESC, a.MON$ATTACHMENT_ID";

const SESSION_COLS: &[&str] = &[
    "ID",
    "Usuario",
    "Rol",
    "Dirección",
    "Programa",
    "Estado",
    "Conectada hace (s)",
    "Memoria (bytes)",
    "Páginas leídas",
    "Páginas consultadas",
    "Consulta en curso",
];

const QUERIES: &str = "
SELECT FIRST 200 s.MON$STATEMENT_ID, s.MON$ATTACHMENT_ID, TRIM(a.MON$USER), s.MON$TRANSACTION_ID,
       DATEDIFF(SECOND FROM s.MON$TIMESTAMP TO CURRENT_TIMESTAMP), io.MON$PAGE_READS, io.MON$PAGE_FETCHES,
       r.MON$RECORD_SEQ_READS + r.MON$RECORD_IDX_READS,
       CAST(SUBSTRING(s.MON$SQL_TEXT FROM 1 FOR 2000) AS VARCHAR(2000))
  FROM MON$STATEMENTS s
  JOIN MON$ATTACHMENTS a ON a.MON$ATTACHMENT_ID = s.MON$ATTACHMENT_ID
  LEFT JOIN MON$IO_STATS io ON io.MON$STAT_ID = s.MON$STAT_ID
  LEFT JOIN MON$RECORD_STATS r ON r.MON$STAT_ID = s.MON$STAT_ID
 WHERE s.MON$STATE = 1 AND s.MON$ATTACHMENT_ID <> CURRENT_CONNECTION
 ORDER BY s.MON$TIMESTAMP";

const QUERY_COLS: &[&str] = &[
    "Sentencia",
    "Conexión",
    "Usuario",
    "Transacción",
    "Duración (s)",
    "Páginas leídas",
    "Páginas consultadas",
    "Filas leídas",
    "Consulta",
];

const TRANSACTIONS: &str = "
SELECT FIRST 200 t.MON$TRANSACTION_ID, t.MON$ATTACHMENT_ID, TRIM(a.MON$USER),
       CASE t.MON$ISOLATION_MODE WHEN 0 THEN 'consistency' WHEN 1 THEN 'snapshot'
            WHEN 2 THEN 'read committed (record version)' WHEN 3 THEN 'read committed (no record version)'
            WHEN 4 THEN 'read committed (read consistency)' ELSE '' END,
       CASE WHEN t.MON$LOCK_TIMEOUT = -1 THEN 'espera' WHEN t.MON$LOCK_TIMEOUT = 0 THEN 'no espera'
            ELSE t.MON$LOCK_TIMEOUT || ' s' END,
       CASE WHEN t.MON$READ_ONLY = 1 THEN 'sí' ELSE 'no' END,
       DATEDIFF(SECOND FROM t.MON$TIMESTAMP TO CURRENT_TIMESTAMP),
       r.MON$RECORD_INSERTS + r.MON$RECORD_UPDATES + r.MON$RECORD_DELETES
  FROM MON$TRANSACTIONS t
  JOIN MON$ATTACHMENTS a ON a.MON$ATTACHMENT_ID = t.MON$ATTACHMENT_ID
  LEFT JOIN MON$RECORD_STATS r ON r.MON$STAT_ID = t.MON$STAT_ID
 WHERE a.MON$SYSTEM_FLAG = 0 AND t.MON$ATTACHMENT_ID <> CURRENT_CONNECTION
 ORDER BY t.MON$TIMESTAMP";

const TRANSACTION_COLS: &[&str] =
    &["Transacción", "Conexión", "Usuario", "Aislamiento", "Bloqueos", "Solo lectura", "Abierta hace (s)", "Filas modificadas"];

const TOP_TABLES: &str = "
SELECT FIRST 20 TRIM(ts.MON$TABLE_NAME), r.MON$RECORD_SEQ_READS, r.MON$RECORD_IDX_READS, r.MON$RECORD_INSERTS,
       r.MON$RECORD_UPDATES, r.MON$RECORD_DELETES,
       r.MON$RECORD_BACKOUTS + r.MON$RECORD_PURGES + r.MON$RECORD_EXPUNGES
  FROM MON$TABLE_STATS ts
  JOIN MON$RECORD_STATS r ON r.MON$STAT_ID = ts.MON$RECORD_STAT_ID
  JOIN MON$DATABASE d ON d.MON$STAT_ID = ts.MON$STAT_ID
 ORDER BY r.MON$RECORD_SEQ_READS + r.MON$RECORD_IDX_READS + r.MON$RECORD_INSERTS
          + r.MON$RECORD_UPDATES + r.MON$RECORD_DELETES DESC";

const TOP_TABLE_COLS: &[&str] = &[
    "Tabla",
    "Lecturas secuenciales",
    "Lecturas por índice",
    "Inserciones",
    "Actualizaciones",
    "Borrados",
    "Limpieza de versiones",
];

/// Rows of each query, all from one monitoring snapshot (one read-only
/// transaction).
pub(crate) fn in_snapshot(c: &mut Conn, queries: &[&str]) -> Result<Vec<Result<Vec<Vec<Column>>, String>>, FbError> {
    let conf = TransactionConfiguration {
        data_access: TrDataAccessMode::ReadOnly,
        isolation: TrIsolationLevel::Concurrency,
        lock_resolution: TrLockResolution::NoWait,
    };
    let mut tr = c.client.begin_transaction(&mut c.db, conf)?;
    let mut out = Vec::new();
    for sql in queries {
        let res = (|| {
            let (_, mut stmt) = c.client.prepare_statement(&mut c.db, &mut tr, Dialect::D3, sql)?;
            let rows = (|| {
                c.client.execute(&mut c.db, &mut tr, &mut stmt, vec![])?;
                let mut rows = Vec::new();
                while let Some(row) = c.client.fetch(&mut c.db, &mut tr, &mut stmt)? {
                    rows.push(row);
                }
                Ok(rows)
            })();
            let _ = c.client.free_statement(&mut stmt, FreeStmtOp::Drop);
            rows
        })();
        out.push(res.map_err(|e: FbError| message(&e)));
    }
    let _ = c.client.transaction_operation(&mut tr, TrOp::Commit);
    Ok(out)
}

fn table(key: &str, title: &str, cols: &[&str], rows: Option<Vec<Vec<Column>>>) -> Option<MonitorTable> {
    let mut t = MonitorTable::new(key, title, cols);
    t.rows = rows?.into_iter().map(|r| r.into_iter().map(|c| cell(c.value)).collect()).collect();
    Some(t)
}

pub fn snapshot(c: &mut Conn) -> dbine_driver::Result<MonitorSnapshot> {
    let what = [
        "la base (MON$DATABASE)",
        "las conexiones (MON$ATTACHMENTS)",
        "las sentencias (MON$STATEMENTS)",
        "",
        "",
        "las conexiones (MON$ATTACHMENTS)",
        "las sentencias en curso (MON$STATEMENTS)",
        "las transacciones (MON$TRANSACTIONS)",
        "las tablas (MON$TABLE_STATS)",
    ];
    let mut res = in_snapshot(c, &[DATABASE, ATTACHMENT_COUNTS, STATEMENT_COUNTS, FB4, CONFIG, SESSIONS, QUERIES, TRANSACTIONS, TOP_TABLES])
        .map_err(|e| dbine_driver::Error::Query(message(&e)))?
        .into_iter();
    let mut snap = MonitorSnapshot::default();
    let mut notes = Vec::new();
    let mut take = |i: usize, r: Option<Result<Vec<Vec<Column>>, String>>| match r {
        Some(Ok(rows)) => Some(rows),
        Some(Err(e)) => {
            if !what[i].is_empty() {
                notes.push(format!("No se pudo leer {}: {e}", what[i]));
            }
            None
        }
        None => None,
    };
    let db = take(0, res.next()).and_then(|r| r.into_iter().next());
    let counts = take(1, res.next()).and_then(|r| r.into_iter().next());
    let stmts = take(2, res.next()).and_then(|r| r.into_iter().next());
    let fb4 = take(3, res.next()).and_then(|r| r.into_iter().next());
    let config = take(4, res.next());
    let at = |r: &Option<Vec<Column>>, i: usize| r.as_ref().and_then(|r| r.get(i)).and_then(f);
    let page = at(&db, 0).unwrap_or(0.0);
    let pages = |i: usize| at(&db, i).map(|p| p * page);

    if let Some(r) = &db {
        let sv = |i: usize| r.get(i).and_then(s);
        if let Some(n) = sv(11) {
            snap.info.push(("Archivo".into(), n));
        }
        snap.info.push(("Tamaño de página".into(), format!("{page} bytes")));
        if let Some(v) = sv(9) {
            snap.info.push(("ODS".into(), v));
        }
        if let Some(v) = sv(10) {
            snap.info.push(("Dialecto".into(), v));
        }
        let yes = |i: usize| if at(&db, i) == Some(1.0) { "sí" } else { "no" };
        snap.info.push(("Escrituras forzadas".into(), yes(7).into()));
        snap.info.push(("Solo lectura".into(), yes(8).into()));
        if let Some(v) = at(&db, 6) {
            snap.info.push(("Intervalo de sweep".into(), format!("{v}")));
        }
        if let Some(v) = at(&db, 23).filter(|v| *v > 0.0) {
            snap.info.push(("Apagado".into(), match v as i64 { 1 => "multi", 2 => "single", _ => "full" }.into()));
        }
        if let Some(v) = at(&db, 24).filter(|v| *v > 0.0) {
            snap.info.push(("Copia (nbackup)".into(), if v == 1.0 { "bloqueada" } else { "fusionando" }.into()));
        }
    }
    if let Some(mode) = at(&fb4, 0) {
        let role = match mode as i64 {
            1 => "Réplica de solo lectura",
            2 => "Réplica de lectura y escritura",
            _ => "Primaria (sin replicación)",
        };
        snap.info.push(("Rol".into(), role.into()));
    }
    for r in config.iter().flatten() {
        if let (Some(k), Some(v)) = (r.first().and_then(s), r.get(1).and_then(s)) {
            snap.info.push((k, v));
        }
    }

    let m = &mut snap.metrics;
    m.push(Metric::new("mem_used", "Memoria de la base", "Memoria", U::Bytes, at(&db, 21)).max(at(&db, 22)));
    m.push(Metric::new("mem_cache", "Caché de páginas", "Memoria", U::Bytes, pages(2)));
    m.push(Metric::new("connections", "Conexiones", "Conexiones", U::Count, at(&counts, 0)));
    m.push(Metric::new("active_sessions", "Conexiones activas", "Conexiones", U::Count, at(&counts, 1)));
    m.push(Metric::new("running_statements", "Sentencias en curso", "Actividad", U::Count, at(&stmts, 0)));
    m.push(Metric::new("transactions", "Transacciones", "Actividad", U::Count, at(&db, 3)).counter());
    m.push(Metric::new("open_transactions", "Transacciones abiertas", "Actividad", U::Count, at(&stmts, 1)));
    m.push(Metric::new("rows_read", "Filas leídas", "Actividad", U::Count, at(&db, 16)).counter());
    m.push(Metric::new("rows_written", "Filas escritas", "Actividad", U::Count, at(&db, 17)).counter());
    m.push(Metric::new("garbage", "Versiones limpiadas", "Actividad", U::Count, at(&db, 20)).counter());
    m.push(Metric::new("disk_read", "Lectura en disco", "Disco", U::Bytes, pages(12)).counter());
    m.push(Metric::new("disk_write", "Escritura en disco", "Disco", U::Bytes, pages(13)).counter());
    let hit = match (at(&db, 12), at(&db, 14)) {
        (Some(r), Some(f)) if f > 0.0 => Some(((1.0 - r / f) * 100.0).clamp(0.0, 100.0)),
        _ => None,
    };
    m.push(Metric::new("cache_hit", "Aciertos de caché", "Caché", U::Percent, hit).max(Some(100.0)));
    m.push(Metric::new("page_fetches", "Páginas consultadas", "Caché", U::Count, at(&db, 14)).counter());
    m.push(Metric::new("storage_used", "Tamaño de la base", "Almacenamiento", U::Bytes, pages(1)));
    m.push(Metric::new("lock_waits", "Esperas por registros bloqueados", "Bloqueos", U::Count, at(&db, 18)).counter());
    m.push(Metric::new("conflicts", "Conflictos de actualización", "Bloqueos", U::Count, at(&db, 19)).counter());
    // Transactions a sweep can't clean up yet: the classic Firebird health figure.
    let gap = at(&db, 3).zip(at(&db, 4)).map(|(next, oat)| next - oat);
    m.push(Metric::new("tx_gap", "Distancia a la transacción activa más vieja", "Transacciones", U::Count, gap));
    m.push(Metric::new("oldest_tx_age", "Transacción abierta más vieja", "Transacciones", U::Seconds, at(&stmts, 2)));
    m.push(Metric::new("uptime", "Base abierta hace", "Servidor", U::Seconds, at(&counts, 2)));

    let tables = [
        table("sessions", "Sesiones", SESSION_COLS, take(5, res.next())),
        table("queries", "Consultas en curso", QUERY_COLS, take(6, res.next())),
        table("transactions", "Transacciones abiertas", TRANSACTION_COLS, take(7, res.next())),
        table("top_objects", "Tablas más usadas (desde que se abrió la base)", TOP_TABLE_COLS, take(8, res.next())),
    ];
    snap.tables.extend(tables.into_iter().flatten());

    snap.notes.push("Firebird no expone el uso de CPU del servidor por SQL.".into());
    if fb4.as_ref().and_then(|r| r.get(1)).and_then(f) == Some(0.0) {
        snap.notes.push(
            "Sin SYSDBA, RDB$ADMIN o el privilegio MONITOR_ANY_ATTACHMENT solo se ven las conexiones de este usuario.".into(),
        );
    }
    if config.is_none() && fb4.is_some() {
        snap.notes.push("La configuración del servidor (RDB$CONFIG) solo la ven SYSDBA y RDB$ADMIN.".into());
    }
    if at(&counts, 2).is_none() {
        snap.notes.push("En el modo Classic no hay conexiones del sistema: no se sabe desde cuándo está abierta la base.".into());
    }
    snap.notes.push("Los contadores de filas y páginas son de esta base desde que se abrió, no de todo el servidor.".into());
    snap.notes.extend(notes);
    Ok(snap)
}
