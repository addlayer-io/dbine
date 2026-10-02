//! A table's indexes and how they're used (`Session::index_usage`).
//!
//! The indexes come from `SHOW INDEX` (every variant but Databend, which
//! lists its own in `system.indexes`, and Manticore, which has no indexes
//! to list: `supports_index_usage` is false). Key parts in key order, with
//! prefixes (`col(10)`), expressions (`(expr)`) and ` DESC`; no INCLUDE
//! columns or filters (these engines don't have them). The foreign keys
//! come from `information_schema.KEY_COLUMN_USAGE`.
//!
//! The counters, per engine, and how they map:
//!
//! - **MySQL** (and Aurora, Cloud SQL, MariaDB with performance_schema on):
//!   `performance_schema.table_io_waits_summary_by_index_usage`.
//!   `COUNT_FETCH` (rows read through the index) goes in `seeks`: the
//!   engine counts rows, not searches, and doesn't tell a lookup from a
//!   full index scan, so `seek_scan_split` is false. Inserts are counted
//!   against no index (`INDEX_NAME` NULL) and updates / deletes against the
//!   index that found the rows, so `updates` is the table's writes
//!   (`COUNT_INSERT + COUNT_UPDATE + COUNT_DELETE` over all its rows): the
//!   writes every index of the table has to follow. An index with no reads
//!   in a written table is then "unused", what `sys.schema_unused_indexes`
//!   reports. Rows read with no index (`INDEX_NAME` NULL: table scans) go
//!   in the primary key's `scans` on InnoDB, whose table is its clustered
//!   primary key. Off (`performance_schema = OFF`, the table I/O instrument
//!   disabled) or refused (no SELECT on performance_schema): the indexes
//!   without counters and a note that says why.
//! - **MariaDB**: `information_schema.INDEX_STATISTICS` (`ROWS_READ` in
//!   `seeks`; the table's `ROWS_READ` less those, the table scans, in the
//!   InnoDB primary key's `scans`) and `TABLE_STATISTICS` (`ROWS_CHANGED`
//!   in `updates`) when
//!   `userstat` is on; else performance_schema when it's on (off by default
//!   in MariaDB); else no counters and a note on how to turn either on.
//! - **TiDB** 8.0+: `information_schema.CLUSTER_TIDB_INDEX_USAGE` (summed
//!   over the TiDB instances; `TIDB_INDEX_USAGE`, this instance only, when
//!   the cluster view fails). It counts queries by the share of the table's
//!   rows they read: under 10% go in `seeks`, 10% or more in `scans`, so
//!   the seek ratio is real. `LAST_ACCESS_TIME` is `last_read`. Writes per
//!   index aren't counted: `updates` is `mysql.stats_meta.modify_count`
//!   (rows changed since the table's last ANALYZE); without SELECT on it
//!   the writes are unknown (`writes_counted` false). A clustered primary key
//!   is the row handle, not an index, and TiDB counts no reads on it: it
//!   keeps zeros (never "unused"). TiDB counts only on tables with
//!   statistics. Before 8.0 there's no such view: no counters, a note.
//! - **OceanBase** 4.x: `oceanbase.DBA_INDEX_USAGE` (its `NAME` is the
//!   index's internal table, `__idx_<table id>_<index>`, matched through
//!   `DBA_OBJECTS`). `TOTAL_ACCESS_COUNT` goes in `seeks` (no split: its
//!   buckets are rows returned, not a share of the table), `LAST_USED` in
//!   `last_read`, `DBA_TAB_MODIFICATIONS` (rows written since the last
//!   statistics gathering) in `updates`. The table is organized by its
//!   primary key, which isn't tracked: zeros, never "unused". Counted by
//!   sampling unless `_iut_stat_collection_type = ALL`, and flushed to the
//!   view in the background. The counts persist across restarts: no `since`.
//!   Off (`_iut_enable`) or refused (no SELECT on `oceanbase`): a note.
//! - **SingleStore, StarRocks, Doris / VeloDB, Databend, GreptimeDB**: no
//!   per-index usage counters; the indexes are listed with a note.
//!
//! Since when: the server's start (`Uptime`); the counters can also have
//! been reset since (TRUNCATE of the performance_schema table, FLUSH
//! INDEX_STATISTICS). Size: InnoDB's `mysql.innodb_index_stats` (`size`
//! pages × `innodb_page_size`, partitions summed; persistent statistics,
//! as of the last ANALYZE), where the login can read it.

use crate::session::{lit, named, MySqlSession};
use crate::{err, Variant};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{ForeignKeyDef, IndexUsage, IndexUsageReport, ObjectRef, Result, Session};
use mysql_async::prelude::Queryable;
use mysql_async::Row;
use std::collections::HashMap;

/// Engines with indexes to list: all but Manticore (its tables are the index).
pub(crate) fn supported(v: Variant) -> bool {
    v != Variant::Manticore
}

/// The table's database: the ref's schema, else the session's.
fn db_expr(table: &ObjectRef) -> String {
    table.schema().map_or_else(|| "DATABASE()".to_string(), lit)
}

pub(crate) fn show_index_sql(table: &ObjectRef) -> String {
    let t = quote_ident(Quote::Backtick, &table.name);
    match table.schema() {
        Some(s) => format!("SHOW INDEX FROM {}.{t}", quote_ident(Quote::Backtick, s)),
        None => format!("SHOW INDEX FROM {t}"),
    }
}

pub(crate) fn foreign_keys_sql(table: &ObjectRef) -> String {
    format!(
        "SELECT CONSTRAINT_NAME, COLUMN_NAME, NULLIF(REFERENCED_TABLE_SCHEMA, TABLE_SCHEMA), REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME
  FROM information_schema.KEY_COLUMN_USAGE
 WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {} AND REFERENCED_TABLE_NAME IS NOT NULL
 ORDER BY CONSTRAINT_NAME, ORDINAL_POSITION",
        db_expr(table),
        lit(&table.name)
    )
}

/// performance_schema's rows for the table: index (NULL: no index), rows
/// fetched, rows written.
pub(crate) fn perf_schema_sql(table: &ObjectRef) -> String {
    format!(
        "SELECT INDEX_NAME, COUNT_FETCH, COUNT_INSERT + COUNT_UPDATE + COUNT_DELETE
  FROM performance_schema.table_io_waits_summary_by_index_usage
 WHERE OBJECT_TYPE = 'TABLE' AND OBJECT_SCHEMA = {} AND OBJECT_NAME = {}",
        db_expr(table),
        lit(&table.name)
    )
}

/// The table's storage engine.
pub(crate) fn engine_sql(table: &ObjectRef) -> String {
    format!("SELECT ENGINE FROM information_schema.TABLES WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {}", db_expr(table), lit(&table.name))
}

/// The instrument the table I/O counters come from.
pub(crate) const PERF_INSTRUMENT_SQL: &str =
    "SELECT ENABLED FROM performance_schema.setup_instruments WHERE NAME = 'wait/io/table/sql/handler'";

pub(crate) fn userstat_sql(table: &ObjectRef) -> (String, String) {
    let filter = format!("TABLE_SCHEMA = {} AND TABLE_NAME = {}", db_expr(table), lit(&table.name));
    (
        format!("SELECT INDEX_NAME, ROWS_READ FROM information_schema.INDEX_STATISTICS WHERE {filter}"),
        format!("SELECT ROWS_READ, ROWS_CHANGED FROM information_schema.TABLE_STATISTICS WHERE {filter}"),
    )
}

/// TiDB's index usage, summed over instances (`view`: the cluster view or
/// this instance's): index, queries under 10% of the rows, the rest, last access.
pub(crate) fn tidb_usage_sql(view: &str, table: &ObjectRef) -> String {
    format!(
        "SELECT INDEX_NAME,
       SUM(IFNULL(PERCENTAGE_ACCESS_0, 0) + IFNULL(PERCENTAGE_ACCESS_0_1, 0) + IFNULL(PERCENTAGE_ACCESS_1_10, 0)),
       SUM(IFNULL(PERCENTAGE_ACCESS_10_20, 0) + IFNULL(PERCENTAGE_ACCESS_20_50, 0) + IFNULL(PERCENTAGE_ACCESS_50_100, 0) + IFNULL(PERCENTAGE_ACCESS_100, 0)),
       DATE_FORMAT(MAX(LAST_ACCESS_TIME), '%Y-%m-%d %H:%i:%s')
  FROM information_schema.{view}
 WHERE TABLE_SCHEMA = {} AND TABLE_NAME = {}
 GROUP BY INDEX_NAME",
        db_expr(table),
        lit(&table.name)
    )
}

/// OceanBase's `DBA_INDEX_USAGE` for the table's indexes: index, accesses,
/// last use. Its `NAME` is the index's internal table, `__idx_<table id>_<index>`.
pub(crate) fn oceanbase_usage_sql(table: &ObjectRef) -> String {
    format!(
        "SELECT o.OBJECT_NAME, u.TOTAL_ACCESS_COUNT, LEFT(u.LAST_USED, 19)
  FROM oceanbase.DBA_INDEX_USAGE u
  JOIN oceanbase.DBA_OBJECTS o ON o.OBJECT_ID = u.OBJECT_ID AND o.OBJECT_TYPE = 'INDEX'
  JOIN oceanbase.DBA_OBJECTS t ON t.OWNER = o.OWNER AND t.OBJECT_TYPE = 'TABLE' AND t.OBJECT_NAME = {} AND t.SUBOBJECT_NAME IS NULL
 WHERE o.OWNER = {} AND u.NAME = CONCAT('__idx_', t.OBJECT_ID, '_', o.OBJECT_NAME)",
        lit(&table.name),
        db_expr(table)
    )
}

/// Whether OceanBase collects index usage (`_iut_enable`).
pub(crate) const OCEANBASE_ENABLED_SQL: &str = "SELECT VALUE FROM oceanbase.GV$OB_PARAMETERS WHERE NAME = '_iut_enable'";

/// Rows written since the table's statistics were last gathered (OceanBase):
/// the table's own row, or its partitions'.
pub(crate) fn oceanbase_writes_sql(table: &ObjectRef) -> String {
    format!(
        "SELECT GREATEST(SUM(CASE WHEN PARTITION_NAME IS NULL THEN INSERTS + UPDATES + DELETES ELSE 0 END),
                SUM(CASE WHEN PARTITION_NAME IS NOT NULL THEN INSERTS + UPDATES + DELETES ELSE 0 END))
  FROM oceanbase.DBA_TAB_MODIFICATIONS
 WHERE TABLE_OWNER = {} AND TABLE_NAME = {}",
        db_expr(table),
        lit(&table.name)
    )
}

/// Rows changed since the table's last ANALYZE (TiDB).
pub(crate) fn tidb_writes_sql(table: &ObjectRef) -> String {
    format!(
        "SELECT m.modify_count FROM mysql.stats_meta m
  JOIN information_schema.TABLES t ON t.TIDB_TABLE_ID = m.table_id
 WHERE t.TABLE_SCHEMA = {} AND t.TABLE_NAME = {}",
        db_expr(table),
        lit(&table.name)
    )
}

/// InnoDB's persistent size statistic, in KB, partitions summed.
pub(crate) fn innodb_size_sql(table: &ObjectRef) -> String {
    format!(
        "SELECT index_name, CAST(SUM(stat_value) * @@innodb_page_size DIV 1024 AS UNSIGNED)
  FROM mysql.innodb_index_stats
 WHERE database_name = {} AND SUBSTRING_INDEX(table_name, '#', 1) = {} AND stat_name = 'size'
 GROUP BY index_name",
        db_expr(table),
        lit(&table.name)
    )
}

/// One `SHOW INDEX` row.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct KeyPart {
    pub index: String,
    pub seq: u64,
    /// `col`, `col(10)` (prefix) or `(expr)` (functional key part).
    pub part: Option<String>,
    pub descending: bool,
    pub unique: bool,
    pub kind: Option<String>,
    /// TiDB: the clustered primary key (the row handle).
    pub clustered: bool,
}

/// A `SHOW INDEX` row through `get` (a cell by any of its column names).
pub(crate) fn key_part(get: &dyn Fn(&[&str]) -> Option<String>) -> Option<KeyPart> {
    let index = get(&["Key_name", "INDEX_NAME"])?;
    let kind = get(&["Index_type", "INDEX_TYPE"]).filter(|k| !k.is_empty());
    let spatial = kind.as_deref().is_some_and(|k| k.eq_ignore_ascii_case("SPATIAL"));
    // TiDB writes the text NULL for an empty expression.
    let expression = get(&["Expression"]).filter(|e| !e.is_empty() && e != "NULL");
    let part = match (get(&["Column_name", "COLUMN_NAME"]).filter(|c| !c.is_empty() && c != "NULL"), expression) {
        (_, Some(e)) => Some(format!("({})", e.replace("\\'", "'"))),
        (Some(c), None) => Some(match get(&["Sub_part", "SUB_PART"]).filter(|n| !n.is_empty() && n != "NULL" && !spatial) {
            Some(n) => format!("{c}({n})"),
            None => c,
        }),
        _ => None,
    };
    Some(KeyPart {
        index,
        seq: get(&["Seq_in_index", "SEQ_IN_INDEX"]).and_then(|s| s.parse().ok()).unwrap_or(0),
        part,
        descending: get(&["Collation", "COLLATION"]).is_some_and(|c| c.eq_ignore_ascii_case("D")),
        unique: get(&["Non_unique", "NON_UNIQUE"]).is_some_and(|n| n == "0"),
        kind,
        clustered: get(&["Clustered"]).is_some_and(|c| c.eq_ignore_ascii_case("YES")),
    })
}

/// One index's counters.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Counters {
    pub seeks: u64,
    pub scans: u64,
    pub last_read: Option<String>,
}

/// What the engine said about the table's use.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Usage {
    /// By index name; an index missing here wasn't read.
    pub reads: HashMap<String, Counters>,
    /// The table's writes, which every index follows.
    pub writes: u64,
    /// Rows read scanning the table, without an index: in InnoDB that's
    /// a scan of the clustered primary key, which gets them as `scans`
    /// (0 for other storage engines).
    pub table_scans: u64,
    pub seek_scan_split: bool,
    /// The engine counts no reads on the primary key (OceanBase: the table
    /// is organized by it): it keeps zeros, never "unused".
    pub pk_untracked: bool,
    /// The table's writes couldn't be read (refused): `writes` is unknown,
    /// not 0, and the report says `writes_counted: false`.
    pub writes_unknown: bool,
}

/// The key parts grouped by index, in the order `SHOW INDEX` lists them
/// (the primary key first), with their counters and sizes.
pub(crate) fn assemble(parts: &[KeyPart], usage: Option<&Usage>, sizes: Option<&HashMap<String, u64>>) -> Vec<IndexUsage> {
    let mut order: Vec<&str> = Vec::new();
    for p in parts {
        if !order.contains(&p.index.as_str()) {
            order.push(&p.index);
        }
    }
    order
        .into_iter()
        .map(|name| {
            let mut own: Vec<&KeyPart> = parts.iter().filter(|p| p.index == name).collect();
            own.sort_by_key(|p| p.seq);
            let first = own[0];
            let primary_key = name == "PRIMARY";
            let c = usage.and_then(|u| u.reads.get(name)).cloned().unwrap_or_default();
            // TiDB's clustered key is the row handle: no reads are counted on it.
            let handle = primary_key && (first.clustered || usage.is_some_and(|u| u.pk_untracked));
            IndexUsage {
                name: name.to_string(),
                kind: first.kind.clone().unwrap_or_default(),
                unique: primary_key || first.unique,
                primary_key,
                key_columns: own.iter().filter_map(|p| p.part.as_ref().map(|c| if p.descending { format!("{c} DESC") } else { c.clone() })).collect(),
                size_kb: sizes.and_then(|m| m.get(name).copied()),
                seeks: c.seeks,
                scans: c.scans + if primary_key { usage.map_or(0, |u| u.table_scans) } else { 0 },
                updates: if handle { 0 } else { usage.map_or(0, |u| u.writes) },
                last_read: c.last_read,
                ..Default::default()
            }
        })
        .collect()
}

/// Foreign key rows (`foreign_keys_sql`'s columns) grouped by constraint.
pub(crate) fn foreign_keys(rows: &[(String, String, Option<String>, String, String)]) -> Vec<ForeignKeyDef> {
    let mut out: Vec<ForeignKeyDef> = Vec::new();
    for (name, col, ref_schema, ref_table, ref_col) in rows {
        match out.last_mut() {
            Some(fk) if fk.name.as_deref() == Some(name.as_str()) => {
                fk.columns.push(col.clone());
                fk.ref_columns.push(ref_col.clone());
            }
            _ => out.push(ForeignKeyDef {
                name: Some(name.clone()),
                columns: vec![col.clone()],
                ref_schema: ref_schema.clone(),
                ref_table: ref_table.clone(),
                ref_columns: vec![ref_col.clone()],
                ..Default::default()
            }),
        }
    }
    out
}

/// `SHOW GLOBAL STATUS LIKE 'Uptime'`'s seconds into the start time.
pub(crate) fn since_sql(uptime: &str) -> Option<String> {
    let secs: u64 = uptime.trim().parse().ok()?;
    Some(format!("SELECT DATE_FORMAT(NOW() - INTERVAL {secs} SECOND, '%Y-%m-%d %H:%i:%s')"))
}

pub(crate) const NOTE_PERF_ROWS: &str = "performance_schema cuenta filas leídas por cada índice sin separar búsquedas de recorridos: «seeks» son esas filas, «scans» de la clave primaria las leídas recorriendo la tabla (InnoDB) y «updates», las filas escritas en la tabla.";
pub(crate) const NOTE_USERSTAT_ROWS: &str = "MariaDB (userstat) cuenta filas leídas por cada índice sin separar búsquedas de recorridos: «seeks» son esas filas, «scans» de la clave primaria las leídas recorriendo la tabla (InnoDB) y «updates», las filas escritas en la tabla.";
pub(crate) const NOTE_TIDB: &str = "TiDB cuenta consultas: «seeks» son las que leyeron menos del 10% de las filas de la tabla por el índice y «scans», el resto. «updates» son las filas modificadas desde el último ANALYZE. La clave primaria agrupada no lleva contadores.";
pub(crate) const NOTE_PERF_OFF: &str = "performance_schema está desactivado en el servidor (performance_schema = OFF; se activa en la configuración y requiere reiniciar). Se listan los índices sin estadísticas de uso.";
pub(crate) const NOTE_PERF_INSTRUMENT: &str = "El instrumento wait/io/table/sql/handler de performance_schema está desactivado: el servidor no cuenta el uso de índices. Se listan los índices sin estadísticas de uso.";
pub(crate) const NOTE_PERF_DENIED: &str = "Para ver cuánto se usa cada índice el usuario necesita el permiso SELECT sobre performance_schema. Se listan los índices sin estadísticas de uso.";
pub(crate) const NOTE_MARIADB_OFF: &str = "MariaDB no está contando el uso de índices: activá userstat (SET GLOBAL userstat = 1, sin reiniciar) o performance_schema (en la configuración, con reinicio). Se listan los índices sin estadísticas de uso.";
pub(crate) const NOTE_OCEANBASE: &str = "OceanBase cuenta accesos por índice sin separar búsquedas de recorridos (por muestreo salvo con _iut_stat_collection_type = ALL) y los vuelca a DBA_INDEX_USAGE cada tanto: «seeks» son esos accesos y «updates», las filas escritas en la tabla desde las últimas estadísticas. La clave primaria no lleva contadores.";
pub(crate) const NOTE_OCEANBASE_OFF: &str = "OceanBase no está contando el uso de índices (_iut_enable = False). Se listan los índices sin estadísticas de uso.";
pub(crate) const NOTE_OCEANBASE_DENIED: &str = "Para ver cuánto se usa cada índice el usuario necesita SELECT sobre la base oceanbase (DBA_INDEX_USAGE, DBA_OBJECTS). Se listan los índices sin estadísticas de uso.";
pub(crate) const NOTE_TIDB_OLD: &str = "TiDB informa el uso de índices desde la versión 8.0 (information_schema.TIDB_INDEX_USAGE). Se listan los índices sin estadísticas de uso.";

/// Engines with indexes but no usage counters.
pub(crate) fn no_counters_note(v: Variant) -> &'static str {
    match v {
        Variant::SingleStore => "SingleStore no informa cuánto se usa cada índice. Se listan los índices sin estadísticas de uso.",
        Variant::StarRocks => "StarRocks no informa cuánto se usa cada índice. Se listan los índices sin estadísticas de uso.",
        Variant::Doris | Variant::VeloDb => "Apache Doris no informa cuánto se usa cada índice. Se listan los índices sin estadísticas de uso.",
        Variant::Databend => "Databend no informa cuánto se usa cada índice. Se listan los índices sin estadísticas de uso.",
        Variant::GreptimeDb => "GreptimeDB no informa cuánto se usa cada índice. Se listan los índices sin estadísticas de uso.",
        _ => "Este motor no informa cuánto se usa cada índice. Se listan los índices sin estadísticas de uso.",
    }
}

/// Rows of `sql`, or the server's refusal (`Err((code, message))`); a
/// broken connection is an error.
async fn probe(s: &mut MySqlSession, sql: &str) -> Result<std::result::Result<Vec<Row>, (u16, String)>> {
    match s.conn.query::<Row, _>(sql).await {
        Ok(rows) => Ok(Ok(rows)),
        Err(mysql_async::Error::Server(e)) => {
            tracing::debug!("{:?}: index usage: {sql}: {}", s.variant, e.message);
            Ok(Err((e.code, e.message)))
        }
        Err(e) => Err(err(e)),
    }
}

/// The first cell of the first row, if any.
async fn scalar(s: &mut MySqlSession, sql: &str) -> Result<Option<String>> {
    Ok(probe(s, sql).await?.ok().and_then(|rows| rows.first().and_then(|r| crate::session::at(r, 0))))
}

/// ER_TABLEACCESS_DENIED_ERROR, ER_SPECIFIC_ACCESS_DENIED_ERROR, ER_DBACCESS_DENIED_ERROR, ER_ACCESS_DENIED_ERROR.
fn denied(code: u16) -> bool {
    matches!(code, 1142 | 1227 | 1044 | 1045)
}

fn num(s: Option<String>) -> u64 {
    s.and_then(|v| v.trim().parse::<f64>().ok()).map_or(0, |n| n.max(0.0) as u64)
}

/// performance_schema's counters: `Ok(None)` + a note when off or refused.
async fn perf_schema(s: &mut MySqlSession, table: &ObjectRef) -> Result<std::result::Result<Usage, &'static str>> {
    if scalar(s, "SELECT @@performance_schema").await?.is_none_or(|v| v == "0" || v.eq_ignore_ascii_case("OFF")) {
        return Ok(Err(NOTE_PERF_OFF));
    }
    if scalar(s, PERF_INSTRUMENT_SQL).await?.is_some_and(|v| v.eq_ignore_ascii_case("NO")) {
        return Ok(Err(NOTE_PERF_INSTRUMENT));
    }
    match probe(s, &perf_schema_sql(table)).await? {
        Ok(rows) => {
            let mut u = Usage::default();
            for r in &rows {
                u.writes += num(crate::session::at(r, 2));
                match crate::session::at(r, 0) {
                    Some(ix) => {
                        u.reads.insert(ix, Counters { seeks: num(crate::session::at(r, 1)), ..Default::default() });
                    }
                    None => u.table_scans += num(crate::session::at(r, 1)),
                }
            }
            Ok(Ok(u))
        }
        Err((code, _)) if denied(code) => Ok(Err(NOTE_PERF_DENIED)),
        Err(_) => Ok(Err(NOTE_PERF_OFF)),
    }
}

/// MariaDB's userstat counters; `None` when userstat is off (or its views
/// fail: performance_schema is tried next).
async fn userstat(s: &mut MySqlSession, table: &ObjectRef) -> Result<Option<Usage>> {
    if !scalar(s, "SELECT @@userstat").await?.is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("ON")) {
        return Ok(None);
    }
    let (index_sql, table_sql) = userstat_sql(table);
    let Ok(rows) = probe(s, &index_sql).await? else { return Ok(None) };
    let mut u = Usage::default();
    for r in &rows {
        if let Some(ix) = crate::session::at(r, 0) {
            u.reads.insert(ix, Counters { seeks: num(crate::session::at(r, 1)), ..Default::default() });
        }
    }
    match probe(s, &table_sql).await? {
        Ok(rows) => {
            if let Some(r) = rows.first() {
                // The table's rows read, less those read through an index.
                let by_index: u64 = u.reads.values().map(|c| c.seeks).sum();
                u.table_scans = num(crate::session::at(r, 0)).saturating_sub(by_index);
                u.writes = num(crate::session::at(r, 1));
            }
        }
        Err(_) => u.writes_unknown = true,
    }
    Ok(Some(u))
}

async fn tidb(s: &mut MySqlSession, table: &ObjectRef) -> Result<std::result::Result<Usage, &'static str>> {
    let mut rows = None;
    for view in ["CLUSTER_TIDB_INDEX_USAGE", "TIDB_INDEX_USAGE"] {
        if let Ok(r) = probe(s, &tidb_usage_sql(view, table)).await? {
            rows = Some(r);
            break;
        }
    }
    let Some(rows) = rows else { return Ok(Err(NOTE_TIDB_OLD)) };
    let mut u = Usage { seek_scan_split: true, ..Default::default() };
    for r in &rows {
        if let Some(ix) = crate::session::at(r, 0) {
            let c = Counters { seeks: num(crate::session::at(r, 1)), scans: num(crate::session::at(r, 2)), last_read: crate::session::at(r, 3) };
            u.reads.insert(ix, c);
        }
    }
    // mysql.stats_meta needs SELECT on the mysql schema.
    match probe(s, &tidb_writes_sql(table)).await? {
        Ok(rows) => u.writes = num(rows.first().and_then(|r| crate::session::at(r, 0))),
        Err(_) => u.writes_unknown = true,
    }
    Ok(Ok(u))
}

async fn oceanbase(s: &mut MySqlSession, table: &ObjectRef) -> Result<std::result::Result<Usage, &'static str>> {
    if scalar(s, OCEANBASE_ENABLED_SQL).await?.is_some_and(|v| v.eq_ignore_ascii_case("False")) {
        return Ok(Err(NOTE_OCEANBASE_OFF));
    }
    let rows = match probe(s, &oceanbase_usage_sql(table)).await? {
        Ok(rows) => rows,
        Err(_) => return Ok(Err(NOTE_OCEANBASE_DENIED)),
    };
    let mut u = Usage { pk_untracked: true, ..Default::default() };
    for r in &rows {
        if let Some(ix) = crate::session::at(r, 0) {
            u.reads.insert(ix, Counters { seeks: num(crate::session::at(r, 1)), last_read: crate::session::at(r, 2), ..Default::default() });
        }
    }
    match probe(s, &oceanbase_writes_sql(table)).await? {
        Ok(rows) => u.writes = num(rows.first().and_then(|r| crate::session::at(r, 0))),
        Err(_) => u.writes_unknown = true,
    }
    Ok(Ok(u))
}

pub(crate) async fn report(s: &mut MySqlSession, table: &ObjectRef) -> Result<IndexUsageReport> {
    let v = s.variant;
    // The catalog.
    let parts: Vec<KeyPart> = if v == Variant::Databend {
        databend_parts(s, table).await?
    } else {
        s.rows(&show_index_sql(table)).await?.iter().filter_map(|r| key_part(&|n: &[&str]| named(r, n))).collect()
    };
    let fk_rows: Vec<_> = if v.has_foreign_keys() {
        match probe(s, &foreign_keys_sql(table)).await? {
            Ok(rows) => rows
                .iter()
                .map(|r| {
                    let at = |i| crate::session::at(r, i);
                    (at(0).unwrap_or_default(), at(1).unwrap_or_default(), at(2), at(3).unwrap_or_default(), at(4).unwrap_or_default())
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    } else {
        Vec::new()
    };

    // The counters.
    let maria = v == Variant::MariaDb || (v == Variant::MySql && s.server_version().await.unwrap_or_default().contains("MariaDB"));
    let (mut usage, note): (Option<Usage>, Option<&str>) = match v {
        Variant::MariaDb | Variant::MySql if maria => match userstat(s, table).await? {
            Some(u) => (Some(u), Some(NOTE_USERSTAT_ROWS)),
            None => match perf_schema(s, table).await? {
                Ok(u) => (Some(u), Some(NOTE_PERF_ROWS)),
                Err(NOTE_PERF_OFF) => (None, Some(NOTE_MARIADB_OFF)),
                Err(note) => (None, Some(note)),
            },
        },
        Variant::MySql => match perf_schema(s, table).await? {
            Ok(u) => (Some(u), Some(NOTE_PERF_ROWS)),
            Err(note) => (None, Some(note)),
        },
        Variant::TiDb => match tidb(s, table).await? {
            Ok(u) => (Some(u), Some(NOTE_TIDB)),
            Err(note) => (None, Some(note)),
        },
        Variant::OceanBase => match oceanbase(s, table).await? {
            Ok(u) => (Some(u), Some(NOTE_OCEANBASE)),
            Err(note) => (None, Some(note)),
        },
        _ => (None, Some(no_counters_note(v))),
    };
    // Table scans read InnoDB's clustered key; other engines' tables are heaps.
    if let Some(u) = usage.as_mut().filter(|u| u.table_scans > 0) {
        let engine = scalar(s, &engine_sql(table)).await?.unwrap_or_default();
        if !engine.eq_ignore_ascii_case("InnoDB") {
            u.table_scans = 0;
        }
    }
    let sizes = if v.is_mysql_server() {
        probe(s, &innodb_size_sql(table)).await?.ok().map(|rows| {
            rows.iter().filter_map(|r| Some((crate::session::at(r, 0)?, num(crate::session::at(r, 1))))).collect::<HashMap<_, _>>()
        })
    } else {
        None
    };
    // OceanBase keeps its counts in a table, across restarts: no start.
    let since = match usage.as_ref().filter(|_| v != Variant::OceanBase) {
        Some(_) => match probe(s, "SHOW GLOBAL STATUS LIKE 'Uptime'").await? {
            Ok(rows) => match rows.first().and_then(|r| crate::session::at(r, 1)).as_deref().and_then(since_sql) {
                Some(sql) => scalar(s, &sql).await?,
                None => None,
            },
            Err(_) => None,
        },
        None => None,
    };
    Ok(IndexUsageReport {
        since,
        stats_available: usage.is_some(),
        note: note.map(str::to_string),
        indexes: assemble(&parts, usage.as_ref(), sizes.as_ref()),
        foreign_keys: foreign_keys(&fk_rows),
        seek_scan_split: usage.as_ref().is_some_and(|u| u.seek_scan_split),
        writes_counted: usage.as_ref().is_some_and(|u| !u.writes_unknown),
    })
}

/// Databend's inverted / ngram indexes on the table (`system.indexes`).
async fn databend_parts(s: &mut MySqlSession, table: &ObjectRef) -> Result<Vec<KeyPart>> {
    let db = match table.schema() {
        Some(d) => d.to_string(),
        None => scalar(s, "SELECT DATABASE()").await?.unwrap_or_default(),
    };
    let mut parts = Vec::new();
    for r in s.optional_rows("SELECT * FROM system.indexes").await {
        let get = |n: &str| named(&r, &[n]).unwrap_or_default();
        let Some((t, ix)) = crate::structure::databend_index(&db, &get("name"), &get("type"), &get("original"), &get("definition")) else { continue };
        if t != table.name {
            continue;
        }
        for (n, c) in ix.columns.iter().enumerate() {
            parts.push(KeyPart { index: ix.name.clone(), seq: n as u64 + 1, part: Some(c.clone()), kind: ix.kind.clone(), ..Default::default() });
        }
        if ix.columns.is_empty() {
            parts.push(KeyPart { index: ix.name.clone(), seq: 1, kind: ix.kind.clone(), ..Default::default() });
        }
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cells: &[(&str, &str)]) -> KeyPart {
        let m: HashMap<String, String> = cells.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.to_string())).collect();
        key_part(&|names: &[&str]| names.iter().find_map(|n| m.get(&n.to_ascii_lowercase()).cloned())).unwrap()
    }

    fn t(schema: Option<&str>, name: &str) -> ObjectRef {
        ObjectRef { kind: "table".into(), schema: schema.map(Into::into), name: name.into() }
    }

    #[test]
    fn queries_name_the_table_and_its_database() {
        let x = t(None, "it's");
        assert_eq!(show_index_sql(&x), "SHOW INDEX FROM `it's`");
        assert_eq!(show_index_sql(&t(Some("d`b"), "t")), "SHOW INDEX FROM `d``b`.`t`");
        let ps = perf_schema_sql(&x);
        assert!(ps.contains("OBJECT_SCHEMA = DATABASE() AND OBJECT_NAME = 'it''s'"), "{ps}");
        assert!(ps.contains("COUNT_FETCH, COUNT_INSERT + COUNT_UPDATE + COUNT_DELETE"));
        assert!(perf_schema_sql(&t(Some("shop"), "t")).contains("OBJECT_SCHEMA = 'shop'"));
        let (ix, tb) = userstat_sql(&x);
        assert!(ix.contains("INDEX_STATISTICS") && ix.contains("ROWS_READ") && tb.contains("ROWS_CHANGED"));
        let tidb = tidb_usage_sql("CLUSTER_TIDB_INDEX_USAGE", &x);
        assert!(tidb.contains("FROM information_schema.CLUSTER_TIDB_INDEX_USAGE") && tidb.contains("GROUP BY INDEX_NAME"), "{tidb}");
        assert!(tidb.contains("PERCENTAGE_ACCESS_1_10, 0)),\n"), "under 10% are seeks: {tidb}");
        assert!(tidb_writes_sql(&x).contains("modify_count"));
        let ob = oceanbase_usage_sql(&t(Some("shop"), "orders"));
        assert!(ob.contains("t.OBJECT_NAME = 'orders'") && ob.contains("o.OWNER = 'shop'"), "{ob}");
        assert!(ob.contains("u.NAME = CONCAT('__idx_', t.OBJECT_ID, '_', o.OBJECT_NAME)"), "{ob}");
        assert!(oceanbase_writes_sql(&x).contains("DBA_TAB_MODIFICATIONS"));
        assert!(innodb_size_sql(&x).contains("SUBSTRING_INDEX(table_name, '#', 1) = 'it''s'"), "partitions summed");
        assert!(foreign_keys_sql(&x).contains("REFERENCED_TABLE_NAME IS NOT NULL"));
        assert_eq!(since_sql("3600").as_deref(), Some("SELECT DATE_FORMAT(NOW() - INTERVAL 3600 SECOND, '%Y-%m-%d %H:%i:%s')"));
        assert_eq!(since_sql("x; DROP"), None);
    }

    #[test]
    fn show_index_rows_become_indexes() {
        let parts = vec![
            row(&[("Key_name", "PRIMARY"), ("Seq_in_index", "1"), ("Column_name", "id"), ("Non_unique", "0"), ("Index_type", "BTREE"), ("Collation", "A")]),
            row(&[("Key_name", "ix_ab"), ("Seq_in_index", "2"), ("Column_name", "b"), ("Non_unique", "1"), ("Index_type", "BTREE"), ("Sub_part", "10")]),
            row(&[("Key_name", "ix_ab"), ("Seq_in_index", "1"), ("Column_name", "a"), ("Non_unique", "1"), ("Index_type", "BTREE"), ("Collation", "D")]),
            row(&[("Key_name", "ix_expr"), ("Seq_in_index", "1"), ("Column_name", "NULL"), ("Expression", "lower(`c`)"), ("Non_unique", "0")]),
            row(&[("Key_name", "sp"), ("Seq_in_index", "1"), ("Column_name", "g"), ("Sub_part", "32"), ("Index_type", "SPATIAL"), ("Non_unique", "1")]),
            row(&[("Key_name", "ix_never"), ("Seq_in_index", "1"), ("Column_name", "d"), ("Non_unique", "1"), ("Index_type", "BTREE")]),
        ];
        let usage = Usage {
            reads: HashMap::from([("PRIMARY".to_string(), Counters { seeks: 4, ..Default::default() }), ("ix_ab".to_string(), Counters { seeks: 6, ..Default::default() })]),
            writes: 9,
            table_scans: 3,
            ..Default::default()
        };
        let sizes = HashMap::from([("PRIMARY".to_string(), 16u64)]);
        let out = assemble(&parts, Some(&usage), Some(&sizes));
        assert_eq!(out.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["PRIMARY", "ix_ab", "ix_expr", "sp", "ix_never"]);
        assert!(out[0].primary_key && out[0].unique && out[0].kind == "BTREE");
        assert_eq!((out[0].seeks, out[0].scans, out[0].updates, out[0].size_kb), (4, 3, 9, Some(16)), "table scans read the clustered key");
        assert_eq!(out[1].scans, 0);
        assert_eq!(out[1].key_columns, ["a DESC", "b(10)"]);
        assert!(!out[1].unique);
        assert_eq!(out[2].key_columns, ["(lower(`c`))"]);
        assert!(out[2].unique);
        assert_eq!(out[3].key_columns, ["g"], "a spatial index's SUB_PART isn't a prefix");
        // Not read: zeros, but the table's writes.
        assert_eq!((out[4].seeks, out[4].scans, out[4].updates, out[4].size_kb), (0, 0, 9, None));
        let r = IndexUsageReport { stats_available: true, indexes: out, seek_scan_split: false, ..Default::default() }.derived();
        assert!(r.indexes[4].unused && !r.indexes[1].unused);
        assert!(r.indexes.iter().all(|i| i.seek_health.is_none()), "one counter: no seek health");
        assert_eq!(r.indexes[1].read_share, Some(6.0 / 13.0));

        // No counters: listed, all zeros.
        let bare = assemble(&parts, None, None);
        assert_eq!(bare.len(), 5);
        assert!(bare.iter().all(|i| i.seeks == 0 && i.updates == 0));
    }

    #[test]
    fn tidb_clustered_key_keeps_zeros() {
        let parts = vec![
            row(&[("Key_name", "PRIMARY"), ("Seq_in_index", "1"), ("Column_name", "id"), ("Non_unique", "0"), ("Clustered", "YES"), ("Expression", "NULL")]),
            row(&[("Key_name", "ix_a"), ("Seq_in_index", "1"), ("Column_name", "a"), ("Non_unique", "1"), ("Clustered", "NO"), ("Sub_part", "NULL")]),
        ];
        let usage = Usage {
            reads: HashMap::from([("ix_a".to_string(), Counters { seeks: 2, scans: 1, last_read: Some("2026-10-01 23:06:29".into()) })]),
            writes: 50,
            seek_scan_split: true,
            ..Default::default()
        };
        let out = assemble(&parts, Some(&usage), None);
        assert_eq!((out[0].updates, out[1].updates), (0, 50));
        assert_eq!(out[1].key_columns, ["a"]);
        assert_eq!(out[1].last_read.as_deref(), Some("2026-10-01 23:06:29"));
        let r = IndexUsageReport { stats_available: true, indexes: out, seek_scan_split: true, ..Default::default() }.derived();
        assert!(!r.indexes[0].unused, "the row handle is never unused");
        assert_eq!(r.indexes[1].seek_ratio.map(|x| (x * 100.0).round()), Some(67.0));
    }

    #[test]
    fn oceanbase_primary_key_keeps_zeros() {
        let parts = vec![
            row(&[("Key_name", "PRIMARY"), ("Seq_in_index", "1"), ("Column_name", "id"), ("Non_unique", "0"), ("Expression", "NULL")]),
            row(&[("Key_name", "ix_a"), ("Seq_in_index", "1"), ("Column_name", "a"), ("Non_unique", "1"), ("Expression", "NULL")]),
        ];
        let usage = Usage { writes: 7, pk_untracked: true, ..Default::default() };
        let r = IndexUsageReport { stats_available: true, indexes: assemble(&parts, Some(&usage), None), seek_scan_split: false, ..Default::default() }.derived();
        assert_eq!((r.indexes[0].updates, r.indexes[0].unused), (0, false));
        assert_eq!((r.indexes[1].updates, r.indexes[1].unused), (7, true));
    }

    #[test]
    fn olap_show_index_rows() {
        // StarRocks / Doris: no Non_unique, a bitmap index.
        let p = row(&[("Key_name", "ix_bm"), ("Seq_in_index", "1"), ("Column_name", "k"), ("Index_type", "BITMAP")]);
        let out = assemble(&[p], None, None);
        assert_eq!((out[0].kind.as_str(), out[0].unique, out[0].key_columns.clone()), ("BITMAP", false, vec!["k".to_string()]));
    }

    #[test]
    fn foreign_key_columns_group_by_constraint() {
        let row = |n: &str, c: &str, t: &str, r: &str| (n.to_string(), c.to_string(), None, t.to_string(), r.to_string());
        let fks = foreign_keys(&[row("fk_a", "x", "p", "id"), row("fk_a", "y", "p", "id2"), row("fk_b", "z", "q", "id")]);
        assert_eq!(fks.len(), 2);
        assert_eq!((fks[0].columns.clone(), fks[0].ref_columns.clone()), (vec!["x".to_string(), "y".into()], vec!["id".to_string(), "id2".into()]));
        assert_eq!((fks[1].ref_table.as_str(), fks[1].ref_schema.as_deref()), ("q", None));
    }

    #[test]
    fn notes_name_what_to_turn_on() {
        assert!(NOTE_PERF_DENIED.contains("SELECT sobre performance_schema"));
        assert!(NOTE_MARIADB_OFF.contains("userstat") && NOTE_MARIADB_OFF.contains("performance_schema"));
        assert!(no_counters_note(Variant::SingleStore).starts_with("SingleStore"));
        assert!(!supported(Variant::Manticore) && supported(Variant::GreptimeDb));
    }
}
