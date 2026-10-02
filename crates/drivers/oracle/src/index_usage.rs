//! A table's indexes and how they're used (`Session::index_usage`).
//!
//! - The indexes and their columns come from `ALL_INDEXES`,
//!   `ALL_IND_COLUMNS` and `ALL_IND_EXPRESSIONS` (function-based and DESC
//!   columns), the primary key from `ALL_CONSTRAINTS`; every user that sees
//!   the table can read them. LOB indexes are skipped (they're the LOB's,
//!   not a query's). Oracle has no INCLUDE columns nor filtered indexes.
//!   The kind is `INDEX_TYPE` (`NORMAL`, `BITMAP`, `FUNCTION-BASED NORMAL`,
//!   `IOT - TOP`…), with ` INVISIBLE` appended when the optimizer ignores it.
//! - The counters: `DBA_INDEX_USAGE` (12.2+), which Oracle keeps across
//!   restarts. It has a single "accessed N times" counter
//!   (`TOTAL_ACCESS_COUNT`), not seeks apart from scans: it goes to `seeks`,
//!   `scans` and `lookups` stay 0 and `seek_scan_split` is false (no seek
//!   ratio). `LAST_USED` is the last read at the flush's granularity: it is
//!   the time of the flush that recorded the access, not of the access
//!   itself (up to 15 minutes later). The index's access buckets (by
//!   rows returned) aren't a seek / scan split, so they aren't used. Oracle
//!   tracks by sampling by default and flushes to that view every 15 minutes
//!   (`V$INDEX_USAGE_INFO.LAST_FLUSH_TIME`, shown in the note). An index with
//!   no row was never used since tracking started: zeros.
//! - The writes (`updates`): "db block changes" of the index's segments in
//!   `V$SEGSTAT` (since the instance started): blocks changed maintaining
//!   the index, not rows. Refused, they're unknown (`writes_counted`
//!   false) and nothing is marked unused.
//!   The two windows differ: reads persist across restarts, writes restart
//!   with the instance; and until the first flush a freshly created or used
//!   index has writes and 0 reads, so it shows as unused (the note says so).
//! - Both views need SELECT on them (SELECT_CATALOG_ROLE, SELECT ANY
//!   DICTIONARY). Refused, or before 12.2: the table owner's indexes under
//!   `ALTER INDEX … MONITORING USAGE` (`USER_OBJECT_USAGE`) still say whether
//!   each one was used (1 read or 0) since `START_MONITORING`, when every
//!   index of the table is monitored; otherwise the indexes are listed
//!   without counters and the note names the privilege.
//! - The size: `DBA_SEGMENTS`, or `USER_SEGMENTS` when every index belongs
//!   to the session user; an index without a segment yet (deferred segment
//!   creation) is then 0 KB. Without DBA_SEGMENTS, another schema's indexes
//!   have no size (`None`, unknown), never 0 KB.
//! - Since when: `DBA_INDEX_USAGE` doesn't say (`None`); under MONITORING
//!   USAGE, the earliest `START_MONITORING`.

use crate::{db_code, err};
use dbine_driver::{ForeignKeyDef, IndexUsage, IndexUsageReport, Result};
use oracledb::{Connection, Row};
use std::collections::HashMap;

/// The table's indexes (`:1` owner, `:2` table), the primary key's first.
pub(crate) const INDEXES_SQL: &str = "SELECT i.owner, i.index_name, i.index_type, i.uniqueness, i.visibility,
        CASE WHEN EXISTS (SELECT 1 FROM all_constraints k
                           WHERE k.owner = i.table_owner AND k.table_name = i.table_name AND k.constraint_type = 'P'
                             AND k.index_owner = i.owner AND k.index_name = i.index_name) THEN 1 ELSE 0 END
   FROM all_indexes i
  WHERE i.table_owner = :1 AND i.table_name = :2 AND i.index_type <> 'LOB'
  ORDER BY 6 DESC, i.index_name";

/// Their columns in key order; the expression (a LONG, so last) of a
/// function-based or DESC one.
pub(crate) const COLUMNS_SQL: &str = "SELECT ic.index_owner, ic.index_name, ic.column_name, ic.descend, e.column_expression
   FROM all_ind_columns ic
   LEFT JOIN all_ind_expressions e
     ON e.index_owner = ic.index_owner AND e.index_name = ic.index_name AND e.column_position = ic.column_position
  WHERE ic.table_owner = :1 AND ic.table_name = :2
  ORDER BY ic.index_owner, ic.index_name, ic.column_position";

/// The table's indexes, as a subquery of `(owner, index_name)`.
const TABLE_INDEXES: &str = "SELECT owner, index_name FROM all_indexes WHERE table_owner = :1 AND table_name = :2";

/// The counters (12.2+); indexes never used have no row.
pub(crate) fn usage_sql() -> String {
    format!(
        "SELECT u.owner, u.name, u.total_access_count, TO_CHAR(u.last_used, 'YYYY-MM-DD HH24:MI:SS')
   FROM dba_index_usage u
  WHERE (u.owner, u.name) IN ({TABLE_INDEXES})"
    )
}

/// When the counters were last flushed from memory.
pub(crate) const FLUSH_SQL: &str = "SELECT TO_CHAR(MAX(last_flush_time), 'YYYY-MM-DD HH24:MI:SS') FROM v$index_usage_info";

/// MONITORING USAGE of the user's own indexes (`:1` must be the user).
/// Binds are positional, in order of appearance: `:1` before `:2`.
pub(crate) const MONITORING_SQL: &str = "SELECT index_name, monitoring, used, start_monitoring
   FROM user_object_usage
  WHERE :1 = USER AND table_name = :2";

/// Blocks changed in each index's segments since the instance started.
pub(crate) fn writes_sql() -> String {
    format!(
        "SELECT o.owner, o.object_name, SUM(s.value)
   FROM v$segstat s
   JOIN all_objects o ON o.object_id = s.obj#
  WHERE s.statistic_name = 'db block changes' AND o.object_type LIKE 'INDEX%'
    AND (o.owner, o.object_name) IN ({TABLE_INDEXES})
  GROUP BY o.owner, o.object_name"
    )
}

/// Bytes of each index's segments (all partitions).
pub(crate) fn size_sql() -> String {
    format!(
        "SELECT owner, segment_name, SUM(bytes)
   FROM dba_segments
  WHERE segment_type LIKE 'INDEX%' AND (owner, segment_name) IN ({TABLE_INDEXES})
  GROUP BY owner, segment_name"
    )
}

/// The same from USER_SEGMENTS, for one's own indexes.
pub(crate) const USER_SIZE_SQL: &str = "SELECT USER, segment_name, SUM(bytes)
   FROM user_segments
  WHERE segment_type LIKE 'INDEX%'
    AND segment_name IN (SELECT index_name FROM all_indexes WHERE table_owner = :1 AND table_name = :2 AND owner = USER)
  GROUP BY segment_name";

/// The session user, to know whether `USER_SIZE_SQL` covers the indexes.
pub(crate) const SESSION_USER_SQL: &str = "SELECT USER FROM dual";

/// Whether every index belongs to the session user (so USER_SEGMENTS has
/// their segments, and a missing one really means no segment yet).
pub(crate) fn all_owned_by(indexes: &[IndexRow], user: Option<&str>) -> bool {
    user.is_some_and(|u| indexes.iter().all(|i| i.owner == u))
}

/// The table's foreign keys, one row per column pair.
pub(crate) const FOREIGN_KEYS_SQL: &str = "SELECT c.constraint_name, cc.column_name, r.owner, r.table_name, rc.column_name, c.delete_rule
   FROM all_constraints c
   JOIN all_cons_columns cc ON cc.owner = c.owner AND cc.constraint_name = c.constraint_name AND cc.table_name = c.table_name
   LEFT JOIN all_constraints r ON r.owner = c.r_owner AND r.constraint_name = c.r_constraint_name
   LEFT JOIN all_cons_columns rc ON rc.owner = r.owner AND rc.constraint_name = r.constraint_name AND rc.position = cc.position
  WHERE c.owner = :1 AND c.table_name = :2 AND c.constraint_type = 'R'
  ORDER BY c.constraint_name, cc.position";

pub(crate) const REFUSED_NOTE: &str = "Para ver cuánto se usa cada índice el usuario necesita SELECT sobre DBA_INDEX_USAGE y V$SEGSTAT (los da el rol SELECT_CATALOG_ROLE).";
pub(crate) const OLD_NOTE: &str = "Oracle anterior a 12.2 no cuenta cuántas veces se usa cada índice.";
pub(crate) const NO_STATS: &str = "Se listan los índices sin estadísticas de uso.";
pub(crate) const MONITORING_HINT: &str = "Con ALTER INDEX … MONITORING USAGE en todos los índices de la tabla, Oracle registra al menos si cada uno se usó.";
pub(crate) const MONITORING_NOTE: &str =
    "Oracle solo registra si cada índice se usó desde que se activó MONITORING USAGE, no cuántas veces: cada índice usado cuenta como una lectura.";
pub(crate) const WRITES_NOTE: &str =
    "Sin SELECT sobre V$SEGSTAT no se ven las escrituras de cada índice, así que ninguno se marca como sin uso.";
pub(crate) const COUNTER_NOTE: &str = "Oracle cuenta los accesos a cada índice sin distinguir búsquedas de recorridos completos";

pub(crate) const FLUSH_LAG: &str = "el uso más reciente puede no figurar todavía, así que un índice recién creado o recién usado puede verse sin uso hasta el próximo volcado, y la última lectura es la hora del volcado que la registró, no la del acceso.";
pub(crate) const SAMPLED_NOTE: &str =
    "Además, DBA_INDEX_USAGE se llena por muestreo (salvo con _iut_stat_collection_type = ALL): un índice que se usa poco puede verse sin uso.";
pub(crate) const WINDOWS_NOTE: &str =
    "Las lecturas se conservan entre reinicios; las escrituras (V$SEGSTAT) se cuentan desde el arranque de la instancia.";

/// What the note says about the flush (the counters lag up to 15 minutes)
/// and the sampling (a rarely used index may never be sampled).
pub(crate) fn flush_note(last_flush: Option<&str>) -> String {
    match last_flush {
        Some(t) => format!("{COUNTER_NOTE} y los vuelca cada 15 minutos (último volcado: {t}): {FLUSH_LAG} {SAMPLED_NOTE}"),
        None => format!("{COUNTER_NOTE} y los vuelca cada 15 minutos: {FLUSH_LAG} {SAMPLED_NOTE}"),
    }
}

/// One ALL_INDEXES row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IndexRow {
    pub owner: String,
    pub name: String,
    pub kind: String,
    pub unique: bool,
    pub invisible: bool,
    pub primary_key: bool,
}

/// One ALL_IND_COLUMNS row (with its expression, if any).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ColumnRow {
    pub owner: String,
    pub index: String,
    pub name: String,
    pub descending: bool,
    pub expression: Option<String>,
}

/// One index's counters.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct UsageRow {
    pub reads: u64,
    pub last_read: Option<String>,
}

type Key = (String, String);

/// A `FOREIGN_KEYS_SQL` row: constraint, column, referenced owner, table and
/// column, delete rule.
pub(crate) type FkRow = (String, String, Option<String>, String, String, Option<String>);

/// A key column as shown: a name, an expression as the dictionary writes
/// it, `"B"` (a DESC column's stored expression) unquoted, ` DESC` appended.
fn column(c: &ColumnRow) -> String {
    let base = match c.expression.as_deref().map(str::trim) {
        Some(e) => match e.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
            Some(name) if !name.contains('"') => name.to_string(),
            _ => e.to_string(),
        },
        None => c.name.clone(),
    };
    if c.descending {
        format!("{base} DESC")
    } else {
        base
    }
}

/// The rows put together, in `indexes`' order. `usage` `None`: no counters
/// (all zeros); an index missing from it is one never used. `sizes` in
/// bytes; missing from a read map, the index has no segment yet (0).
pub(crate) fn assemble(
    indexes: &[IndexRow],
    columns: &[ColumnRow],
    usage: Option<&HashMap<Key, UsageRow>>,
    writes: Option<&HashMap<Key, u64>>,
    sizes: Option<&HashMap<Key, u64>>,
) -> Vec<IndexUsage> {
    indexes
        .iter()
        .map(|ix| {
            let key = (ix.owner.clone(), ix.name.clone());
            let u = usage.and_then(|m| m.get(&key)).cloned().unwrap_or_default();
            IndexUsage {
                name: ix.name.clone(),
                kind: if ix.invisible { format!("{} INVISIBLE", ix.kind) } else { ix.kind.clone() },
                unique: ix.unique || ix.primary_key,
                primary_key: ix.primary_key,
                key_columns: columns.iter().filter(|c| c.owner == ix.owner && c.index == ix.name).map(column).collect(),
                size_kb: sizes.map(|m| m.get(&key).copied().unwrap_or(0).div_ceil(1024)),
                seeks: u.reads,
                updates: writes.and_then(|m| m.get(&key).copied()).unwrap_or(0),
                last_read: u.last_read,
                ..Default::default()
            }
        })
        .collect()
}

/// Foreign key rows (`FOREIGN_KEYS_SQL`'s columns) grouped by constraint;
/// the referenced schema only when it isn't the table's.
pub(crate) fn foreign_keys(owner: &str, rows: &[FkRow]) -> Vec<ForeignKeyDef> {
    let mut out: Vec<ForeignKeyDef> = Vec::new();
    for (name, col, ref_owner, ref_table, ref_col, rule) in rows {
        match out.last_mut() {
            Some(fk) if fk.name.as_deref() == Some(name.as_str()) => {
                fk.columns.push(col.clone());
                fk.ref_columns.push(ref_col.clone());
            }
            _ => out.push(ForeignKeyDef {
                name: Some(name.clone()),
                columns: vec![col.clone()],
                ref_schema: ref_owner.clone().filter(|o| o != owner),
                ref_table: ref_table.clone(),
                ref_columns: vec![ref_col.clone()],
                on_delete: rule.clone().filter(|r| r != "NO ACTION"),
                on_update: None,
            }),
        }
    }
    out
}

/// `MM/DD/YYYY HH24:MI:SS` (START_MONITORING) as `YYYY-MM-DD HH24:MI:SS`.
pub(crate) fn monitoring_time(s: &str) -> Option<String> {
    let (date, time) = s.trim().split_once(' ')?;
    let mut p = date.split('/');
    let (m, d, y) = (p.next()?, p.next()?, p.next()?);
    (m.len() == 2 && d.len() == 2 && y.len() == 4 && time.len() == 8).then(|| format!("{y}-{m}-{d} {time}"))
}

/// MONITORING USAGE rows (`MONITORING_SQL`) as counters, when every index
/// of the table is monitored: (used → 1 read, the earliest start).
pub(crate) fn monitoring(owner: &str, indexes: &[IndexRow], rows: &[(String, String, String, Option<String>)]) -> Option<(HashMap<Key, UsageRow>, Option<String>)> {
    let on: HashMap<&str, &(String, String, String, Option<String>)> =
        rows.iter().filter(|r| r.1 == "YES").map(|r| (r.0.as_str(), r)).collect();
    if indexes.is_empty() || !indexes.iter().all(|ix| ix.owner == owner && on.contains_key(ix.name.as_str())) {
        return None;
    }
    let usage = indexes
        .iter()
        .map(|ix| ((ix.owner.clone(), ix.name.clone()), UsageRow { reads: u64::from(on[ix.name.as_str()].2 == "YES"), last_read: None }))
        .collect();
    let since = on.values().filter_map(|r| r.3.as_deref().and_then(monitoring_time)).min();
    Some((usage, since))
}

/// Rows of `sql`, or `None` when the server refuses it (no privilege, no
/// such view); a broken connection is an error.
fn try_rows(c: &Connection, sql: &str, owner: &str, table: &str) -> Result<Option<Vec<Row>>> {
    let params: [&dyn oracledb::ToDbValue; 2] = [&owner, &table];
    // Positional binds, in order of appearance: every query with them has `:1` then `:2`.
    let params: &[&dyn oracledb::ToDbValue] = if sql.contains(":1") { &params } else { &[] };
    let rows = match c.query(sql, params) {
        Ok(cur) => cur.collect::<std::result::Result<Vec<_>, _>>(),
        Err(e) => Err(e),
    };
    match rows {
        Ok(r) => Ok(Some(r)),
        Err(e) if db_code(&e).is_some() => {
            tracing::debug!("oracle: index usage read refused: {e}");
            Ok(None)
        }
        Err(e) => Err(err(e)),
    }
}

fn text(r: &Row, i: usize) -> Option<String> {
    r.get::<Option<String>>(i).ok().flatten()
}

fn num(r: &Row, i: usize) -> u64 {
    r.get::<Option<i64>>(i).ok().flatten().unwrap_or(0).max(0) as u64
}

fn keyed(rows: &[Row]) -> HashMap<Key, u64> {
    rows.iter().map(|r| ((text(r, 0).unwrap_or_default(), text(r, 1).unwrap_or_default()), num(r, 2))).collect()
}

pub(crate) fn report(c: &Connection, owner: &str, table: &str) -> Result<IndexUsageReport> {
    let p: [&dyn oracledb::ToDbValue; 2] = [&owner, &table];
    // The dictionary: what every user that sees the table can read.
    let mut indexes = Vec::new();
    for r in c.query(INDEXES_SQL, &p).map_err(err)? {
        let r = r.map_err(err)?;
        indexes.push(IndexRow {
            owner: text(&r, 0).unwrap_or_default(),
            name: text(&r, 1).unwrap_or_default(),
            kind: text(&r, 2).unwrap_or_default(),
            unique: text(&r, 3).as_deref() == Some("UNIQUE"),
            invisible: text(&r, 4).as_deref() == Some("INVISIBLE"),
            primary_key: num(&r, 5) == 1,
        });
    }
    let mut columns = Vec::new();
    for r in c.query(COLUMNS_SQL, &p).map_err(err)? {
        let r = r.map_err(err)?;
        columns.push(ColumnRow {
            owner: text(&r, 0).unwrap_or_default(),
            index: text(&r, 1).unwrap_or_default(),
            name: text(&r, 2).unwrap_or_default(),
            descending: text(&r, 3).as_deref() == Some("DESC"),
            expression: text(&r, 4),
        });
    }
    let fk_rows: Vec<FkRow> = try_rows(c, FOREIGN_KEYS_SQL, owner, table)?
        .unwrap_or_default()
        .iter()
        .map(|r| (text(r, 0).unwrap_or_default(), text(r, 1).unwrap_or_default(), text(r, 2), text(r, 3).unwrap_or_default(), text(r, 4).unwrap_or_default(), text(r, 5)))
        .collect();

    // The usage views: each one may be refused on its own.
    let mut notes: Vec<String> = Vec::new();
    let mut since = None;
    let modern = c.version().map(|v| (v.0, v.1) >= (12, 2)).unwrap_or(true);
    let index_usage = if modern { try_rows(c, &usage_sql(), owner, table)? } else { None };
    let counted = index_usage.is_some();
    let usage: Option<HashMap<Key, UsageRow>> = match index_usage {
        Some(rows) => {
            let flushed = try_rows(c, FLUSH_SQL, owner, table)?.and_then(|r| r.first().and_then(|r| text(r, 0)));
            notes.push(flush_note(flushed.as_deref()));
            Some(
                rows.iter()
                    .map(|r| ((text(r, 0).unwrap_or_default(), text(r, 1).unwrap_or_default()), UsageRow { reads: num(r, 2), last_read: text(r, 3) }))
                    .collect(),
            )
        }
        None => {
            let rows: Vec<_> = try_rows(c, MONITORING_SQL, owner, table)?
                .unwrap_or_default()
                .iter()
                .map(|r| (text(r, 0).unwrap_or_default(), text(r, 1).unwrap_or_default(), text(r, 2).unwrap_or_default(), text(r, 3)))
                .collect();
            match monitoring(owner, &indexes, &rows) {
                Some((u, s)) => {
                    notes.push(MONITORING_NOTE.into());
                    since = s;
                    Some(u)
                }
                None => {
                    notes.push(format!("{} {NO_STATS} {MONITORING_HINT}", if modern { REFUSED_NOTE } else { OLD_NOTE }));
                    None
                }
            }
        }
    };
    let writes = match usage {
        Some(_) => match try_rows(c, &writes_sql(), owner, table)? {
            Some(rows) => {
                if counted {
                    // DBA_INDEX_USAGE reads vs. V$SEGSTAT writes: two windows.
                    notes.push(WINDOWS_NOTE.into());
                }
                Some(keyed(&rows))
            }
            None => {
                notes.push(WRITES_NOTE.into());
                None
            }
        },
        None => None,
    };
    let sizes = match try_rows(c, &size_sql(), owner, table)? {
        Some(rows) => Some(keyed(&rows)),
        None => {
            // USER_SEGMENTS only covers the session user's own segments: for
            // anyone else's index an empty answer means "unknown", not 0 KB.
            let user = try_rows(c, SESSION_USER_SQL, owner, table)?.and_then(|r| r.first().and_then(|r| text(r, 0)));
            if all_owned_by(&indexes, user.as_deref()) { try_rows(c, USER_SIZE_SQL, owner, table)?.map(|r| keyed(&r)) } else { None }
        }
    };
    Ok(IndexUsageReport {
        since,
        stats_available: usage.is_some(),
        note: (!notes.is_empty()).then(|| notes.join(" ")),
        indexes: assemble(&indexes, &columns, usage.as_ref(), writes.as_ref(), sizes.as_ref()),
        foreign_keys: foreign_keys(owner, &fk_rows),
        seek_scan_split: false,
        writes_counted: writes.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ix(name: &str, kind: &str) -> IndexRow {
        IndexRow { owner: "APP".into(), name: name.into(), kind: kind.into(), unique: false, invisible: false, primary_key: false }
    }

    fn col(index: &str, name: &str, descending: bool, expression: Option<&str>) -> ColumnRow {
        ColumnRow { owner: "APP".into(), index: index.into(), name: name.into(), descending, expression: expression.map(Into::into) }
    }

    fn key(n: &str) -> Key {
        ("APP".into(), n.into())
    }

    #[test]
    fn queries_read_the_table_and_skip_lob_indexes() {
        assert!(INDEXES_SQL.contains("i.index_type <> 'LOB'"));
        assert!(INDEXES_SQL.contains("k.constraint_type = 'P'"));
        // LONG last in the select list.
        assert!(COLUMNS_SQL.split("FROM").next().unwrap().trim_end().ends_with("e.column_expression"));
        assert!(usage_sql().contains("total_access_count") && usage_sql().contains("(u.owner, u.name) IN (SELECT owner, index_name FROM all_indexes"));
        assert!(writes_sql().contains("'db block changes'"));
        assert!(size_sql().contains("dba_segments"));
        assert!(MONITORING_SQL.contains(":1 = USER"));
        assert!(FOREIGN_KEYS_SQL.contains("c.constraint_type = 'R'"));
    }

    #[test]
    fn indexes_get_their_columns_counters_and_size() {
        let mut pk = ix("PK_T", "NORMAL");
        pk.primary_key = true;
        let mut fb = ix("IX_FB", "FUNCTION-BASED NORMAL");
        fb.invisible = true;
        let indexes = [pk, ix("IX_A", "NORMAL"), fb, ix("IX_NEVER", "BITMAP")];
        let columns = [
            col("PK_T", "ID", false, None),
            col("IX_A", "A", false, None),
            col("IX_A", "SYS_NC00005$", true, Some("\"B\"")),
            col("IX_FB", "SYS_NC00006$", false, Some("UPPER(\"C\")")),
            col("IX_NEVER", "D", false, None),
        ];
        let usage = HashMap::from([
            (key("PK_T"), UsageRow { reads: 9, last_read: Some("2026-10-01 10:00:00".into()) }),
            (key("IX_A"), UsageRow { reads: 4, last_read: None }),
        ]);
        let writes = HashMap::from([(key("PK_T"), 30u64), (key("IX_NEVER"), 12)]);
        let sizes = HashMap::from([(key("PK_T"), 65536u64), (key("IX_A"), 1000)]);
        let out = assemble(&indexes, &columns, Some(&usage), Some(&writes), Some(&sizes));
        assert_eq!(out.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["PK_T", "IX_A", "IX_FB", "IX_NEVER"]);
        assert!(out[0].primary_key && out[0].unique, "a PK is unique");
        assert_eq!((out[0].seeks, out[0].scans, out[0].updates, out[0].size_kb), (9, 0, 30, Some(64)));
        assert_eq!(out[0].last_read.as_deref(), Some("2026-10-01 10:00:00"));
        assert_eq!(out[1].key_columns, ["A", "B DESC"]);
        assert_eq!(out[1].size_kb, Some(1));
        assert_eq!(out[2].key_columns, ["UPPER(\"C\")"]);
        assert_eq!(out[2].kind, "FUNCTION-BASED NORMAL INVISIBLE");
        // No segment yet: 0 KB; no usage row: never used.
        assert_eq!(out[2].size_kb, Some(0));
        assert_eq!((out[3].seeks, out[3].updates), (0, 12));
        assert!(out.iter().all(|i| i.included_columns.is_empty() && i.filter.is_none()));

        let r = IndexUsageReport { stats_available: true, seek_scan_split: false, indexes: out, ..Default::default() }.derived();
        assert!(r.indexes[3].unused);
        assert!(r.indexes.iter().all(|i| i.seek_ratio.is_none() && i.seek_health.is_none()), "one counter: no seek health");

        // Counters refused: every index still listed, all zeros.
        let bare = assemble(&indexes, &columns, None, None, None);
        assert_eq!(bare.len(), 4);
        assert!(bare.iter().all(|i| i.seeks == 0 && i.updates == 0 && i.size_kb.is_none()));
    }

    #[test]
    fn foreign_keys_group_by_constraint() {
        let row = |n: &str, c: &str, o: &str, t: &str, r: &str| (n.to_string(), c.to_string(), Some(o.to_string()), t.to_string(), r.to_string(), Some("CASCADE".to_string()));
        let fks = foreign_keys("APP", &[row("FK_A", "X", "APP", "P", "ID"), row("FK_A", "Y", "APP", "P", "ID2"), row("FK_B", "Z", "OTHER", "Q", "ID")]);
        assert_eq!(fks.len(), 2);
        assert_eq!((fks[0].columns.clone(), fks[0].ref_columns.clone()), (vec!["X".to_string(), "Y".into()], vec!["ID".to_string(), "ID2".into()]));
        assert_eq!((fks[0].ref_schema.as_deref(), fks[0].on_delete.as_deref()), (None, Some("CASCADE")));
        assert_eq!((fks[1].ref_table.as_str(), fks[1].ref_schema.as_deref()), ("Q", Some("OTHER")));
    }

    #[test]
    fn monitoring_usage_only_when_every_index_is_monitored() {
        let indexes = [ix("PK_T", "NORMAL"), ix("IX_A", "NORMAL")];
        let row = |n: &str, m: &str, u: &str, s: &str| (n.to_string(), m.to_string(), u.to_string(), Some(s.to_string()));
        let rows = [row("PK_T", "YES", "YES", "10/02/2026 09:00:00"), row("IX_A", "YES", "NO", "10/01/2026 08:30:00")];
        let (usage, since) = monitoring("APP", &indexes, &rows).unwrap();
        assert_eq!((usage[&key("PK_T")].reads, usage[&key("IX_A")].reads), (1, 0));
        assert_eq!(since.as_deref(), Some("2026-10-01 08:30:00"));
        // One index not monitored (or turned off): no counters at all.
        assert!(monitoring("APP", &indexes, &rows[..1]).is_none());
        assert!(monitoring("APP", &indexes, &[rows[0].clone(), row("IX_A", "NO", "NO", "10/01/2026 08:30:00")]).is_none());
        // Someone else's table: USER_OBJECT_USAGE doesn't cover it.
        assert!(monitoring("OTHER", &indexes, &rows).is_none());
        assert_eq!(monitoring_time("bad"), None);
    }

    #[test]
    fn the_notes_name_the_privilege_and_the_flush() {
        assert!(REFUSED_NOTE.contains("DBA_INDEX_USAGE") && REFUSED_NOTE.contains("SELECT_CATALOG_ROLE"));
        assert!(flush_note(Some("2026-10-02 10:15:00")).contains("último volcado: 2026-10-02 10:15:00"));
        assert!(flush_note(None).contains("15 minutos"));
        assert!(flush_note(None).contains("hora del volcado"));
        assert!(flush_note(None).contains("muestreo") && flush_note(Some("x")).contains("se usa poco"));
    }

    #[test]
    fn user_segments_only_for_the_session_users_indexes() {
        let mine = vec![ix("A", "NORMAL"), ix("B", "NORMAL")];
        assert!(all_owned_by(&mine, Some("APP")));
        // A reader of APP's table: USER_SEGMENTS can't tell their size.
        assert!(!all_owned_by(&mine, Some("READER")));
        assert!(!all_owned_by(&mine, None));
    }
}
