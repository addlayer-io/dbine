//! A table's indexes (`Session::index_usage`).
//!
//! - The indexes and their columns come from `INFORMATION_SCHEMA.INDEXES` /
//!   `INDEX_COLUMNS`: the primary key (`PRIMARY_KEY`, the table itself is
//!   stored in its order), secondary indexes (`UNIQUE`, `NULL_FILTERED`,
//!   `STORING` columns as the included ones, `WHERE` filter) and search and
//!   vector indexes. The ones Spanner manages for foreign keys
//!   (`SPANNER_IS_MANAGED`) are left out, as in `database_schema`, so every
//!   listed index can be dropped through the schema sync. A vector index
//!   carries its distance and tree sizes ([`IndexUsage::vector`]) from the
//!   `OPTIONS` of its DDL, as `database_schema` reads them.
//! - Counters: `SPANNER_SYS.TABLE_OPERATIONS_STATS_HOUR` has a row per
//!   table and per index (`TABLE_NAME` is either) and hour, kept for 30
//!   days. Reads (`seeks`) = the sum of `READ_QUERY_COUNT`, the queries
//!   that read the index; Spanner doesn't tell targeted reads from scans,
//!   so the report says `seek_scan_split: false` (neutral color). Writes
//!   (`updates`) = `WRITE_COUNT + DELETE_COUNT`. The primary key's are the
//!   table's own row. `since` = the oldest `INTERVAL_END` minus the hour it
//!   closes (UTC). An index without a row had no operations: zeros.
//!   The emulator has no `SPANNER_SYS`, and logins without
//!   `spanner.databases.select` (or, under fine-grained access control,
//!   the `spanner_sys_reader` role) can't read it: then the indexes are listed
//!   without counters (`stats_available` false) and the note names the
//!   permission. With no row at all yet (a new database), the same.
//! - The size: `USED_BYTES` of the last hour in
//!   `SPANNER_SYS.TABLE_SIZES_STATS_1HOUR`, which has a row per table and per
//!   index (the primary key's is the table's). The emulator and logins
//!   without `spanner.databases.select` on `SPANNER_SYS` leave it empty.
//! - The foreign keys from `REFERENTIAL_CONSTRAINTS` / `KEY_COLUMN_USAGE`.

use crate::{schema_of, structure, SpannerSession};
use dbine_driver::{ForeignKeyDef, IndexUsage, IndexUsageReport, ObjectRef, Result};
use std::collections::{BTreeMap, HashMap};

/// The counters were read.
pub(crate) const NOTE: &str = "Lecturas = consultas que leyeron cada índice (READ_QUERY_COUNT); escrituras = WRITE_COUNT + DELETE_COUNT; de SPANNER_SYS.TABLE_OPERATIONS_STATS_HOUR, que guarda los últimos 30 días. Spanner no separa búsquedas de recorridos. Hora en UTC.";

/// `SPANNER_SYS` couldn't be read.
pub(crate) const NOTE_DENIED: &str = "No se pudieron leer las estadísticas de SPANNER_SYS.TABLE_OPERATIONS_STATS_HOUR (hace falta el permiso spanner.databases.select y, con control de acceso detallado, el rol spanner_sys_reader; el emulador no las tiene): se listan los índices sin contadores.";

/// `SPANNER_SYS` was read but has no hour collected yet.
pub(crate) const NOTE_EMPTY: &str = "SPANNER_SYS.TABLE_OPERATIONS_STATS_HOUR todavía no tiene datos (se llena al cerrar cada hora): se listan los índices sin contadores.";

/// Every table's and index's operations of the last 30 days, and when the
/// oldest hour started (UTC).
pub(crate) const OPERATIONS_SQL: &str = "SELECT TABLE_NAME, CAST(SUM(READ_QUERY_COUNT) AS STRING), CAST(SUM(WRITE_COUNT) + SUM(DELETE_COUNT) AS STRING),
       (SELECT FORMAT_TIMESTAMP('%F %T', TIMESTAMP_SUB(MIN(INTERVAL_END), INTERVAL 1 HOUR), 'UTC') FROM SPANNER_SYS.TABLE_OPERATIONS_STATS_HOUR)
  FROM SPANNER_SYS.TABLE_OPERATIONS_STATS_HOUR
 GROUP BY TABLE_NAME";

/// `@p0` schema (`""` the default), `@p1` table.
pub(crate) const INDEXES_SQL: &str =
    "SELECT INDEX_NAME, INDEX_TYPE, IS_UNIQUE, IS_NULL_FILTERED, FILTER
  FROM INFORMATION_SCHEMA.INDEXES
 WHERE TABLE_SCHEMA = @p0 AND TABLE_NAME = @p1
   AND INDEX_TYPE IN ('PRIMARY_KEY', 'INDEX', 'SEARCH', 'VECTOR') AND NOT SPANNER_IS_MANAGED
 ORDER BY INDEX_TYPE <> 'PRIMARY_KEY', INDEX_NAME";

/// Key columns in key order (with their ordering), then the stored ones
/// (no ordinal position).
pub(crate) const COLUMNS_SQL: &str =
    "SELECT INDEX_NAME, INDEX_TYPE, COLUMN_NAME, CAST(ORDINAL_POSITION AS STRING), COLUMN_ORDERING
  FROM INFORMATION_SCHEMA.INDEX_COLUMNS
 WHERE TABLE_SCHEMA = @p0 AND TABLE_NAME = @p1
 ORDER BY INDEX_NAME, ORDINAL_POSITION IS NULL, ORDINAL_POSITION, COLUMN_NAME";

pub(crate) const FOREIGN_KEYS_SQL: &str = "SELECT rc.CONSTRAINT_NAME, k.COLUMN_NAME, u.TABLE_SCHEMA, u.TABLE_NAME, u.COLUMN_NAME, rc.DELETE_RULE
  FROM INFORMATION_SCHEMA.REFERENTIAL_CONSTRAINTS rc
  JOIN INFORMATION_SCHEMA.KEY_COLUMN_USAGE k ON k.CONSTRAINT_CATALOG = rc.CONSTRAINT_CATALOG
   AND k.CONSTRAINT_SCHEMA = rc.CONSTRAINT_SCHEMA AND k.CONSTRAINT_NAME = rc.CONSTRAINT_NAME
  JOIN INFORMATION_SCHEMA.KEY_COLUMN_USAGE u ON u.CONSTRAINT_CATALOG = rc.UNIQUE_CONSTRAINT_CATALOG
   AND u.CONSTRAINT_SCHEMA = rc.UNIQUE_CONSTRAINT_SCHEMA AND u.CONSTRAINT_NAME = rc.UNIQUE_CONSTRAINT_NAME
   AND u.ORDINAL_POSITION = k.POSITION_IN_UNIQUE_CONSTRAINT
 WHERE k.TABLE_SCHEMA = @p0 AND k.TABLE_NAME = @p1
 ORDER BY rc.CONSTRAINT_NAME, k.ORDINAL_POSITION";

/// The last hour's size of every table and index.
pub(crate) const SIZES_SQL: &str = "SELECT TABLE_NAME, CAST(USED_BYTES AS STRING)
  FROM SPANNER_SYS.TABLE_SIZES_STATS_1HOUR
 WHERE INTERVAL_END = (SELECT MAX(INTERVAL_END) FROM SPANNER_SYS.TABLE_SIZES_STATS_1HOUR)";

type Rows = Vec<Vec<Option<String>>>;

fn cell(r: &[Option<String>], i: usize) -> Option<String> {
    r.get(i).cloned().flatten().filter(|v| !v.is_empty())
}

/// The rows put together. `sizes` by table or index name (qualified with
/// the schema outside the default one), `table` the table's own name there.
pub(crate) fn assemble(
    indexes: &Rows,
    columns: &Rows,
    sizes: &HashMap<String, u64>,
    schema: Option<&str>,
    table: &str,
) -> Vec<IndexUsage> {
    let full = |n: &str| match schema {
        Some(s) => format!("{s}.{n}"),
        None => n.to_string(),
    };
    indexes
        .iter()
        .map(|r| {
            let name = cell(r, 0).unwrap_or_default();
            let ty = cell(r, 1).unwrap_or_default();
            let pk = ty == "PRIMARY_KEY";
            let null_filtered = cell(r, 3).as_deref() == Some("true");
            let mut keys = Vec::new();
            let mut stored = Vec::new();
            for c in columns.iter().filter(|c| {
                cell(c, 0).as_deref() == Some(name.as_str())
                    && cell(c, 1).as_deref() == Some(ty.as_str())
            }) {
                let col = cell(c, 2).unwrap_or_default();
                match cell(c, 3) {
                    Some(_) => keys.push(if cell(c, 4).as_deref() == Some("DESC") {
                        format!("{col} DESC")
                    } else {
                        col
                    }),
                    // The primary key lists every column, the non-key ones without a position.
                    None if !pk => stored.push(col),
                    None => {}
                }
            }
            let size = if pk {
                sizes.get(&full(table))
            } else {
                sizes.get(&full(&name))
            };
            IndexUsage {
                kind: match ty.as_str() {
                    "INDEX" if null_filtered => "NULL_FILTERED INDEX".into(),
                    _ => ty.clone(),
                },
                unique: pk || (ty == "INDEX" && cell(r, 2).as_deref() == Some("true")),
                primary_key: pk,
                key_columns: keys,
                included_columns: stored,
                filter: cell(r, 4),
                size_kb: size.map(|b| b.div_ceil(1024)),
                name,
                ..Default::default()
            }
        })
        .collect()
}

/// The vector indexes' settings ([`structure::vector_of`]) from the
/// `OPTIONS` of the DDL, by schema-qualified index name. A vector index
/// whose DDL wasn't read still says it's one, with no settings.
pub(crate) fn vectors(
    list: &mut [IndexUsage],
    options: &BTreeMap<String, BTreeMap<String, String>>,
    schema: Option<&str>,
) {
    for i in list.iter_mut().filter(|i| i.kind == structure::VECTOR) {
        let full = match schema {
            Some(s) => format!("{s}.{}", i.name),
            None => i.name.clone(),
        };
        i.vector = Some(structure::vector_of(
            options.get(&full).unwrap_or(&BTreeMap::new()),
        ));
    }
}

/// The search and vector indexes' `OPTIONS` from the database's DDL (the
/// `GetDatabaseDdl` response), none when it couldn't be read.
pub(crate) fn ddl_index_options(
    ddl: Option<&serde_json::Value>,
) -> BTreeMap<String, BTreeMap<String, String>> {
    structure::index_options_from_ddl(
        ddl.and_then(|d| d.get("statements"))
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str),
    )
}

/// `FOREIGN_KEYS_SQL` rows grouped by constraint.
pub(crate) fn foreign_keys(rows: &Rows) -> Vec<ForeignKeyDef> {
    let mut out: Vec<ForeignKeyDef> = Vec::new();
    for r in rows {
        let name = cell(r, 0);
        let (col, ref_col) = (
            cell(r, 1).unwrap_or_default(),
            cell(r, 4).unwrap_or_default(),
        );
        match out.last_mut().filter(|f| f.name == name) {
            Some(fk) => {
                fk.columns.push(col);
                fk.ref_columns.push(ref_col);
            }
            None => out.push(ForeignKeyDef {
                name,
                columns: vec![col],
                ref_schema: schema_of(cell(r, 2)),
                ref_table: cell(r, 3).unwrap_or_default(),
                ref_columns: vec![ref_col],
                on_delete: (cell(r, 5).as_deref() == Some("CASCADE")).then(|| "CASCADE".into()),
                on_update: None,
            }),
        }
    }
    out
}

pub(crate) fn sizes(rows: &Rows) -> HashMap<String, u64> {
    rows.iter()
        .filter_map(|r| Some((cell(r, 0)?, cell(r, 1)?.parse().ok()?)))
        .collect()
}

/// `OPERATIONS_SQL` rows: (reads, writes) by table or index name, and since when.
pub(crate) fn operations(rows: &Rows) -> (HashMap<String, (u64, u64)>, Option<String>) {
    let n = |r: &[Option<String>], i| cell(r, i).and_then(|v| v.parse().ok()).unwrap_or(0);
    let ops = rows
        .iter()
        .filter_map(|r| Some((cell(r, 0)?, (n(r, 1), n(r, 2)))))
        .collect();
    (ops, rows.iter().find_map(|r| cell(r, 3)))
}

/// The counters into the indexes (the primary key's are the table's).
pub(crate) fn apply(
    indexes: &mut [IndexUsage],
    ops: &HashMap<String, (u64, u64)>,
    schema: Option<&str>,
    table: &str,
) {
    let full = |n: &str| match schema {
        Some(s) => format!("{s}.{n}"),
        None => n.to_string(),
    };
    for i in indexes {
        let key = if i.primary_key {
            full(table)
        } else {
            full(&i.name)
        };
        if let Some(&(reads, writes)) = ops.get(&key) {
            i.seeks = reads;
            i.updates = writes;
        }
    }
}

pub(crate) async fn report(s: &mut SpannerSession, table: &ObjectRef) -> Result<IndexUsageReport> {
    let schema = table.schema().unwrap_or("");
    let args = [schema, table.name.as_str()];
    let indexes = s.text_rows(INDEXES_SQL, &args).await?;
    let columns = s.text_rows(COLUMNS_SQL, &args).await?;
    let fks = s.text_rows(FOREIGN_KEYS_SQL, &args).await?;
    let sizes = match s.text_rows(SIZES_SQL, &[]).await {
        Ok(rows) => sizes(&rows),
        Err(e) => {
            tracing::debug!("spanner: index sizes not read: {e}");
            HashMap::new()
        }
    };
    let qualifier = schema_of(Some(schema.to_string()));
    let mut list = assemble(
        &indexes,
        &columns,
        &sizes,
        qualifier.as_deref(),
        &table.name,
    );
    if list.iter().any(|i| i.kind == structure::VECTOR) {
        // Vector indexes keep their OPTIONS only in the DDL.
        let ddl = s
            .api
            .get(&format!("{}/ddl", s.database))
            .await
            .map_err(|e| tracing::debug!("spanner: vector index options not read: {e}"))
            .ok();
        vectors(
            &mut list,
            &ddl_index_options(ddl.as_ref()),
            qualifier.as_deref(),
        );
    }
    let (stats_available, since, note) = match s.text_rows(OPERATIONS_SQL, &[]).await {
        Ok(rows) if rows.is_empty() => (false, None, NOTE_EMPTY),
        Ok(rows) => {
            let (ops, since) = operations(&rows);
            apply(&mut list, &ops, qualifier.as_deref(), &table.name);
            (true, since, NOTE)
        }
        Err(e) => {
            tracing::debug!("spanner: index operations not read: {e}");
            (false, None, NOTE_DENIED)
        }
    };
    Ok(IndexUsageReport {
        since,
        stats_available,
        note: Some(note.into()),
        indexes: list,
        foreign_keys: foreign_keys(&fks),
        seek_scan_split: false,
        // WRITE_COUNT + DELETE_COUNT, per index.
        writes_counted: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(v: &[Option<&str>]) -> Vec<Option<String>> {
        v.iter().map(|c| c.map(str::to_string)).collect()
    }

    #[test]
    fn indexes_columns_and_sizes() {
        let indexes = vec![
            row(&[
                Some("PRIMARY_KEY"),
                Some("PRIMARY_KEY"),
                Some("true"),
                Some("false"),
                None,
            ]),
            row(&[
                Some("ix_a"),
                Some("INDEX"),
                Some("false"),
                Some("true"),
                Some("a IS NOT NULL"),
            ]),
            row(&[
                Some("ux_b"),
                Some("INDEX"),
                Some("true"),
                Some("false"),
                None,
            ]),
            row(&[
                Some("sx"),
                Some("SEARCH"),
                Some("false"),
                Some("false"),
                None,
            ]),
        ];
        let columns = vec![
            row(&[
                Some("PRIMARY_KEY"),
                Some("PRIMARY_KEY"),
                Some("id"),
                Some("1"),
                Some("ASC"),
            ]),
            row(&[
                Some("PRIMARY_KEY"),
                Some("PRIMARY_KEY"),
                Some("a"),
                None,
                None,
            ]),
            row(&[
                Some("ix_a"),
                Some("INDEX"),
                Some("a"),
                Some("1"),
                Some("DESC"),
            ]),
            row(&[Some("ix_a"), Some("INDEX"), Some("c"), None, None]),
            row(&[
                Some("ux_b"),
                Some("INDEX"),
                Some("b"),
                Some("1"),
                Some("ASC"),
            ]),
            row(&[Some("sx"), Some("SEARCH"), Some("toks"), Some("1"), None]),
        ];
        let sizes = sizes(&vec![
            row(&[Some("s.t"), Some("4096")]),
            row(&[Some("s.ix_a"), Some("100")]),
            row(&[Some("t"), Some("9")]),
        ]);
        let r = assemble(&indexes, &columns, &sizes, Some("s"), "t");
        assert_eq!(r.len(), 4);
        assert!(r[0].primary_key && r[0].unique);
        assert_eq!(
            (
                r[0].key_columns.clone(),
                r[0].included_columns.len(),
                r[0].size_kb
            ),
            (vec!["id".to_string()], 0, Some(4))
        );
        assert_eq!(r[1].kind, "NULL_FILTERED INDEX");
        assert_eq!(r[1].key_columns, vec!["a DESC"]);
        assert_eq!(r[1].included_columns, vec!["c"]);
        assert_eq!(
            (r[1].filter.as_deref(), r[1].size_kb, r[1].unique),
            (Some("a IS NOT NULL"), Some(1), false)
        );
        assert!(r[2].unique && r[2].size_kb.is_none());
        assert_eq!(
            (r[3].kind.as_str(), r[3].unique, r[3].key_columns.clone()),
            ("SEARCH", false, vec!["toks".to_string()])
        );
        // The default schema's names aren't qualified.
        let r = assemble(&indexes, &columns, &sizes, None, "t");
        assert_eq!(r[0].size_kb, Some(1));
    }

    #[test]
    fn vector_indexes_carry_their_settings() {
        let mut list = vec![
            IndexUsage {
                name: "vx".into(),
                kind: "VECTOR".into(),
                ..Default::default()
            },
            IndexUsage {
                name: "ix".into(),
                kind: "INDEX".into(),
                ..Default::default()
            },
            IndexUsage {
                name: "lost".into(),
                kind: "VECTOR".into(),
                ..Default::default()
            },
        ];
        let ddl = serde_json::json!({"statements": [
            "CREATE TABLE s.v (k INT64, e ARRAY<FLOAT32>(vector_length=>3)) PRIMARY KEY (k)",
            "CREATE VECTOR INDEX s.vx ON s.v(e) WHERE e IS NOT NULL OPTIONS ( distance_type = 'COSINE', tree_depth = 3, num_leaves = 100 )",
        ]});
        let mut unread = list.clone();
        vectors(&mut list, &ddl_index_options(Some(&ddl)), Some("s"));
        let v = list[0].vector.as_ref().unwrap();
        assert_eq!(v.metric.as_deref(), Some("cosine"));
        assert_eq!(v.params.get("lists").map(String::as_str), Some("100"));
        assert_eq!(v.params.get("tree_depth").map(String::as_str), Some("3"));
        assert!(list[1].vector.is_none());
        // The same settings as the describe's IndexDef.vector.
        let mut o = BTreeMap::new();
        o.insert("distance_type".to_string(), "'COSINE'".to_string());
        o.insert("tree_depth".to_string(), "3".to_string());
        o.insert("num_leaves".to_string(), "100".to_string());
        assert_eq!(Some(structure::vector_of(&o)), list[0].vector);
        // Without its DDL it's still a vector index.
        assert!(list[2].vector.is_some());
        // So is every one when the DDL can't be read at all.
        vectors(&mut unread, &ddl_index_options(None), Some("s"));
        assert!(unread[0].vector.is_some() && unread[2].vector.is_some());
        assert!(unread[1].vector.is_none());
    }

    #[test]
    fn operations_per_index_and_table() {
        let (ops, since) = operations(&vec![
            row(&[
                Some("s.t"),
                Some("40"),
                Some("7"),
                Some("2026-09-02 10:00:00"),
            ]),
            row(&[
                Some("s.ix_a"),
                Some("0"),
                Some("7"),
                Some("2026-09-02 10:00:00"),
            ]),
            row(&[
                Some("s.ux_b"),
                Some("12"),
                Some("3"),
                Some("2026-09-02 10:00:00"),
            ]),
        ]);
        assert_eq!(since.as_deref(), Some("2026-09-02 10:00:00"));
        let mut ixs = vec![
            IndexUsage {
                name: "PRIMARY_KEY".into(),
                primary_key: true,
                ..Default::default()
            },
            IndexUsage {
                name: "ix_a".into(),
                ..Default::default()
            },
            IndexUsage {
                name: "ux_b".into(),
                ..Default::default()
            },
            IndexUsage {
                name: "sx".into(),
                ..Default::default()
            },
        ];
        apply(&mut ixs, &ops, Some("s"), "t");
        assert_eq!(
            ixs.iter().map(|i| (i.seeks, i.updates)).collect::<Vec<_>>(),
            vec![(40, 7), (0, 7), (12, 3), (0, 0)]
        );
        let r = IndexUsageReport {
            stats_available: true,
            seek_scan_split: false,
            indexes: ixs,
            ..Default::default()
        }
        .derived();
        // Written, never read: the case the hourly table is for.
        assert!(r.indexes[1].unused && !r.indexes[3].unused);
        assert!(r.indexes.iter().all(|i| i.seek_health.is_none()));
    }

    #[test]
    fn foreign_keys_group_by_constraint() {
        let fks = foreign_keys(&vec![
            row(&[
                Some("fk_p"),
                Some("a"),
                Some(""),
                Some("p"),
                Some("x"),
                Some("CASCADE"),
            ]),
            row(&[
                Some("fk_p"),
                Some("b"),
                Some(""),
                Some("p"),
                Some("y"),
                Some("CASCADE"),
            ]),
            row(&[
                Some("fk_q"),
                Some("c"),
                Some("s"),
                Some("q"),
                Some("id"),
                Some("NO ACTION"),
            ]),
        ]);
        assert_eq!(fks.len(), 2);
        assert_eq!(
            (
                fks[0].columns.clone(),
                fks[0].ref_columns.clone(),
                fks[0].ref_schema.clone()
            ),
            (
                vec!["a".into(), "b".into()],
                vec!["x".into(), "y".into()],
                None
            )
        );
        assert_eq!(fks[0].on_delete.as_deref(), Some("CASCADE"));
        assert_eq!(
            (
                fks[1].ref_schema.as_deref(),
                fks[1].ref_table.as_str(),
                fks[1].on_delete.clone()
            ),
            (Some("s"), "q", None)
        );
    }
}
