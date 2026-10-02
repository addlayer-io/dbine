//! A table's indexes and how they're used (`Session::index_usage`).
//!
//! - The indexes and their columns come from the catalog (`sys.indexes`,
//!   `sys.index_columns`), which every login that sees the table can read.
//! - The counters: `sys.indexes` LEFT JOIN `sys.dm_db_index_usage_stats`
//!   (this database, `DB_ID()`), so an index never used since the counters
//!   started shows zeros instead of disappearing. They need VIEW SERVER
//!   STATE (SQL Server; VIEW SERVER PERFORMANCE STATE from 2022) or VIEW
//!   DATABASE STATE (Azure SQL Database). Refused: the indexes are still
//!   listed, `stats_available` is false and the note says why.
//! - The size: `sys.dm_db_partition_stats` (used pages × 8 KB); refused,
//!   the size stays empty.
//! - Since when: `sqlserver_start_time` of `sys.dm_os_sys_info`; where the
//!   login (or Azure SQL) can't read it, `None`.
//!
//! Heaps (`index_id` 0) are skipped: a heap is the table itself, not an
//! index, and its scans aren't an index's reads. The read share is over the
//! table's indexes only.
//!
//! Fabric warehouses have no indexes (`supports_index_usage` is false).
//! Babelfish has the catalog views; what its DMVs don't answer stays empty.

use crate::variant::Variant;
use crate::{err, is_desync, text, SqlServerSession};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{ForeignKeyDef, IndexUsage, IndexUsageReport, ObjectRef, Result};
use std::collections::HashMap;
use tiberius::Row;

/// The table's indexes (no heap, no hypothetical ones). `@P1`: the table.
pub(crate) const INDEXES_SQL: &str = "SELECT CAST(i.index_id AS int), i.name, i.type_desc, i.is_unique, i.is_primary_key, i.filter_definition
  FROM sys.indexes i
 WHERE i.object_id = OBJECT_ID(@P1) AND i.index_id > 0 AND i.is_hypothetical = 0
 ORDER BY i.index_id";

/// Their columns: keys in key order, then the INCLUDE ones.
pub(crate) const COLUMNS_SQL: &str = "SELECT CAST(ic.index_id AS int), c.name, ic.is_included_column, ic.is_descending_key, CAST(ic.key_ordinal AS int)
  FROM sys.index_columns ic
  JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
 WHERE ic.object_id = OBJECT_ID(@P1)
 ORDER BY ic.index_id, ic.is_included_column, ic.key_ordinal, ic.index_column_id";

/// The counters, zeros for the indexes the DMV has no row for.
pub(crate) const USAGE_SQL: &str = "SELECT CAST(i.index_id AS int),
       CAST(ISNULL(u.user_seeks, 0) AS bigint), CAST(ISNULL(u.user_scans, 0) AS bigint),
       CAST(ISNULL(u.user_lookups, 0) AS bigint), CAST(ISNULL(u.user_updates, 0) AS bigint),
       CONVERT(varchar(19), (SELECT MAX(v) FROM (VALUES (u.last_user_seek), (u.last_user_scan), (u.last_user_lookup)) AS r(v)), 120),
       CONVERT(varchar(19), u.last_user_update, 120)
  FROM sys.indexes i
  LEFT JOIN sys.dm_db_index_usage_stats u
    ON u.database_id = DB_ID() AND u.object_id = i.object_id AND u.index_id = i.index_id
 WHERE i.object_id = OBJECT_ID(@P1) AND i.index_id > 0";

/// Used pages × 8 KB per index (all partitions, LOB and row overflow included).
pub(crate) const SIZE_SQL: &str = "SELECT CAST(ps.index_id AS int), CAST(SUM(ps.used_page_count) * 8 AS bigint)
  FROM sys.dm_db_partition_stats ps
 WHERE ps.object_id = OBJECT_ID(@P1) AND ps.index_id > 0
 GROUP BY ps.index_id";

/// When the instance started, which is when the counters did.
pub(crate) const SINCE_SQL: &str = "SELECT CONVERT(varchar(19), sqlserver_start_time, 120) FROM sys.dm_os_sys_info";

/// The table's foreign keys, one row per column pair.
pub(crate) const FOREIGN_KEYS_SQL: &str = "SELECT fk.name, pc.name, rs.name, rt.name, rc.name
  FROM sys.foreign_keys fk
  JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id
  JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id
  JOIN sys.objects rt ON rt.object_id = fkc.referenced_object_id
  JOIN sys.schemas rs ON rs.schema_id = rt.schema_id
  JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id
 WHERE fk.parent_object_id = OBJECT_ID(@P1)
 ORDER BY fk.name, fkc.constraint_column_id";

/// One `sys.indexes` row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct IndexRow {
    pub id: i32,
    pub name: String,
    pub kind: String,
    pub unique: bool,
    pub primary_key: bool,
    pub filter: Option<String>,
}

/// One `sys.index_columns` row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ColumnRow {
    pub index: i32,
    pub name: String,
    pub included: bool,
    pub descending: bool,
    pub key_ordinal: i32,
}

/// One index's counters.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct UsageRow {
    pub seeks: u64,
    pub scans: u64,
    pub lookups: u64,
    pub updates: u64,
    pub last_read: Option<String>,
    pub last_write: Option<String>,
}

/// The rows put together, in `indexes`' order. `usage` `None`: no counters
/// (all zeros); an index missing from it is one never used.
pub(crate) fn assemble(indexes: &[IndexRow], columns: &[ColumnRow], usage: Option<&HashMap<i32, UsageRow>>, sizes: Option<&HashMap<i32, u64>>) -> Vec<IndexUsage> {
    indexes
        .iter()
        .map(|ix| {
            let columnstore = ix.kind.contains("COLUMNSTORE");
            let mut keys = Vec::new();
            let mut include = Vec::new();
            for c in columns.iter().filter(|c| c.index == ix.id) {
                if c.included {
                    include.push(c.name.clone());
                } else if c.key_ordinal > 0 || columnstore {
                    // A columnstore's columns have no key order; any other
                    // column without one is a partitioning column.
                    keys.push(if c.descending { format!("{} DESC", c.name) } else { c.name.clone() });
                }
            }
            let u = usage.and_then(|m| m.get(&ix.id)).cloned().unwrap_or_default();
            IndexUsage {
                name: ix.name.clone(),
                kind: ix.kind.clone(),
                unique: ix.unique,
                primary_key: ix.primary_key,
                key_columns: keys,
                included_columns: include,
                filter: ix.filter.clone(),
                size_kb: sizes.and_then(|m| m.get(&ix.id).copied()),
                seeks: u.seeks,
                scans: u.scans,
                lookups: u.lookups,
                updates: u.updates,
                last_read: u.last_read,
                last_write: u.last_write,
                ..Default::default()
            }
        })
        .collect()
}

/// Foreign key rows (`FOREIGN_KEYS_SQL`'s columns) grouped by constraint.
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

/// What the note says when the counters were refused (the server's own
/// message goes to the log).
pub(crate) fn usage_note(variant: Variant) -> &'static str {
    match variant {
        Variant::AzureSql => "Para ver cuánto se usa cada índice el usuario necesita el permiso VIEW DATABASE STATE. Se listan los índices sin estadísticas de uso.",
        Variant::Babelfish => "Babelfish no informa cuánto se usa cada índice. Se listan los índices sin estadísticas de uso.",
        _ => "Para ver cuánto se usa cada índice el usuario necesita el permiso VIEW SERVER STATE (VIEW SERVER PERFORMANCE STATE desde SQL Server 2022). Se listan los índices sin estadísticas de uso.",
    }
}

/// Rows of `sql`, or the server's refusal (`Err(message)`); a broken
/// connection is replaced and the read retried, like any catalog read.
async fn try_query(s: &mut SqlServerSession, sql: &str, params: &[&str]) -> Result<std::result::Result<Vec<Row>, String>> {
    let mut r = s.try_rows(sql, params).await;
    if matches!(&r, Err(e) if is_desync(e)) {
        s.reconnect().await?;
        r = s.try_rows(sql, params).await;
    }
    match r {
        Ok(rows) => Ok(Ok(rows)),
        Err(tiberius::error::Error::Server(e)) => {
            tracing::debug!("sqlserver: index usage read refused: {}", e.message());
            Ok(Err(e.message().to_string()))
        }
        Err(e) => Err(err(e)),
    }
}

fn int(r: &Row, i: usize) -> i32 {
    r.try_get::<i32, _>(i).ok().flatten().unwrap_or(0)
}

fn big(r: &Row, i: usize) -> u64 {
    r.try_get::<i64, _>(i).ok().flatten().unwrap_or(0).max(0) as u64
}

fn flag(r: &Row, i: usize) -> bool {
    r.try_get::<bool, _>(i).ok().flatten().unwrap_or(false)
}

pub(crate) async fn report(s: &mut SqlServerSession, table: &ObjectRef) -> Result<IndexUsageReport> {
    let object = qualified_name(Quote::Bracket, table.schema(), &table.name);
    let p: [&str; 1] = [&object];
    // The catalog: what every login that sees the table can read.
    let indexes: Vec<IndexRow> = s
        .rows(INDEXES_SQL, &p)
        .await?
        .iter()
        .map(|r| IndexRow {
            id: int(r, 0),
            name: text(r, 1).unwrap_or_default(),
            kind: text(r, 2).unwrap_or_default(),
            unique: flag(r, 3),
            primary_key: flag(r, 4),
            filter: text(r, 5),
        })
        .collect();
    let columns: Vec<ColumnRow> = s
        .rows(COLUMNS_SQL, &p)
        .await?
        .iter()
        .map(|r| ColumnRow { index: int(r, 0), name: text(r, 1).unwrap_or_default(), included: flag(r, 2), descending: flag(r, 3), key_ordinal: int(r, 4) })
        .collect();
    let fk_rows: Vec<_> = match try_query(s, FOREIGN_KEYS_SQL, &p).await? {
        Ok(rows) => rows.iter().map(|r| (text(r, 0).unwrap_or_default(), text(r, 1).unwrap_or_default(), text(r, 2), text(r, 3).unwrap_or_default(), text(r, 4).unwrap_or_default())).collect(),
        Err(_) => Vec::new(),
    };

    // The DMVs: each one may be refused on its own.
    let mut notes: Vec<&str> = Vec::new();
    let usage = match try_query(s, USAGE_SQL, &p).await? {
        Ok(rows) => Some(
            rows.iter()
                .map(|r| {
                    let u = UsageRow { seeks: big(r, 1), scans: big(r, 2), lookups: big(r, 3), updates: big(r, 4), last_read: text(r, 5), last_write: text(r, 6) };
                    (int(r, 0), u)
                })
                .collect::<HashMap<_, _>>(),
        ),
        Err(_) => {
            notes.push(usage_note(s.variant));
            None
        }
    };
    let sizes = match try_query(s, SIZE_SQL, &p).await? {
        Ok(rows) => Some(rows.iter().map(|r| (int(r, 0), big(r, 1))).collect::<HashMap<_, _>>()),
        Err(_) => None,
    };
    let since = match usage {
        Some(_) => match try_query(s, SINCE_SQL, &[]).await? {
            Ok(rows) => rows.first().and_then(|r| text(r, 0)),
            Err(_) => None,
        },
        None => None,
    };
    Ok(IndexUsageReport {
        since,
        stats_available: usage.is_some(),
        note: (!notes.is_empty()).then(|| notes.join(" ")),
        indexes: assemble(&indexes, &columns, usage.as_ref(), sizes.as_ref()),
        foreign_keys: foreign_keys(&fk_rows),
        seek_scan_split: true,
        writes_counted: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ix(id: i32, name: &str, kind: &str) -> IndexRow {
        IndexRow { id, name: name.into(), kind: kind.into(), unique: false, primary_key: false, filter: None }
    }

    fn col(index: i32, name: &str, included: bool, descending: bool, key_ordinal: i32) -> ColumnRow {
        ColumnRow { index, name: name.into(), included, descending, key_ordinal }
    }

    #[test]
    fn usage_is_a_left_join_on_this_database() {
        assert!(USAGE_SQL.contains("FROM sys.indexes i\n  LEFT JOIN sys.dm_db_index_usage_stats u"), "{USAGE_SQL}");
        assert!(USAGE_SQL.contains("u.database_id = DB_ID()"));
        assert!(USAGE_SQL.contains("ISNULL(u.user_seeks, 0)"), "never-used indexes read as zeros");
        // Heaps are skipped everywhere.
        for sql in [INDEXES_SQL, USAGE_SQL, SIZE_SQL] {
            assert!(sql.contains("index_id > 0"), "{sql}");
        }
        assert!(INDEXES_SQL.contains("is_hypothetical = 0"));
        assert!(SINCE_SQL.contains("sqlserver_start_time"));
    }

    #[test]
    fn indexes_get_their_columns_counters_and_size() {
        let mut pk = ix(1, "PK_t", "CLUSTERED");
        pk.primary_key = true;
        pk.unique = true;
        let mut nc = ix(2, "ix_a", "NONCLUSTERED");
        nc.filter = Some("([a]>(0))".into());
        let indexes = [pk, nc, ix(3, "ix_never", "NONCLUSTERED"), ix(4, "ccs", "NONCLUSTERED COLUMNSTORE")];
        let columns = [
            col(1, "id", false, false, 1),
            col(2, "a", false, true, 1),
            col(2, "b", false, false, 2),
            col(2, "c", true, false, 0),
            // A partitioning column of a non-aligned index: not a key.
            col(3, "p", false, false, 0),
            col(3, "b", false, false, 1),
            col(4, "a", false, false, 0),
            col(4, "b", false, false, 0),
        ];
        let usage = HashMap::from([
            (1, UsageRow { seeks: 9, scans: 1, updates: 3, last_read: Some("2026-10-01 10:00:00".into()), ..Default::default() }),
            (2, UsageRow { seeks: 4, lookups: 2, updates: 3, ..Default::default() }),
        ]);
        let sizes = HashMap::from([(1, 72u64), (2, 16)]);
        let out = assemble(&indexes, &columns, Some(&usage), Some(&sizes));
        assert_eq!(out.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["PK_t", "ix_a", "ix_never", "ccs"]);
        assert!(out[0].primary_key && out[0].unique);
        assert_eq!(out[0].key_columns, ["id"]);
        assert_eq!((out[0].seeks, out[0].scans, out[0].size_kb), (9, 1, Some(72)));
        assert_eq!(out[0].last_read.as_deref(), Some("2026-10-01 10:00:00"));
        assert_eq!(out[1].key_columns, ["a DESC", "b"]);
        assert_eq!(out[1].included_columns, ["c"]);
        assert_eq!(out[1].filter.as_deref(), Some("([a]>(0))"));
        assert_eq!((out[1].lookups, out[1].updates), (2, 3));
        // Not in the DMV: never used since the counters started.
        assert_eq!((out[2].seeks, out[2].scans, out[2].lookups, out[2].updates, out[2].size_kb), (0, 0, 0, 0, None));
        assert_eq!(out[2].key_columns, ["b"]);
        assert_eq!(out[3].key_columns, ["a", "b"]);

        // Counters refused: every index still listed, all zeros.
        let bare = assemble(&indexes, &columns, None, None);
        assert_eq!(bare.len(), 4);
        assert!(bare.iter().all(|i| i.seeks == 0 && i.updates == 0 && i.size_kb.is_none()));
        let r = IndexUsageReport { stats_available: false, indexes: bare, ..Default::default() }.derived();
        assert!(r.indexes.iter().all(|i| i.read_share.is_none() && !i.unused));
    }

    #[test]
    fn foreign_key_columns_group_by_constraint() {
        let row = |n: &str, c: &str, t: &str, r: &str| (n.to_string(), c.to_string(), Some("dbo".to_string()), t.to_string(), r.to_string());
        let fks = foreign_keys(&[row("fk_a", "x", "p", "id"), row("fk_a", "y", "p", "id2"), row("fk_b", "z", "q", "id")]);
        assert_eq!(fks.len(), 2);
        assert_eq!((fks[0].columns.clone(), fks[0].ref_columns.clone()), (vec!["x".to_string(), "y".into()], vec!["id".to_string(), "id2".into()]));
        assert_eq!((fks[1].ref_table.as_str(), fks[1].ref_schema.as_deref()), ("q", Some("dbo")));
    }

    /// Against a real server (`DBINE_TEST_SQLSERVER_URL`, by default the
    /// `dbine-test-sqlserver` container):
    ///
    /// ```sh
    /// cargo test -p dbine-driver-sqlserver --lib index_usage_live -- --ignored
    /// ```
    #[tokio::test]
    #[ignore]
    async fn index_usage_live() {
        use crate::{variant, SqlServerDriver};
        use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
        let url = std::env::var("DBINE_TEST_SQLSERVER_URL").unwrap_or_else(|_| "mssql://sa:Pw_12345!@localhost:25013".into());
        let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
        let (auth, hostport) = rest.rsplit_once('@').unwrap();
        let (user, pass) = auth.split_once(':').unwrap();
        let (host, port) = hostport.rsplit_once(':').unwrap();
        let cfg = ConnectionConfig {
            driver: "sqlserver".into(),
            host: host.into(),
            port: port.trim_end_matches('/').parse().unwrap(),
            username: Some(user.into()),
            password: Some(pass.into()),
            ..Default::default()
        };
        let d = SqlServerDriver { info: variant::info(Variant::SqlServer), variant: Variant::SqlServer };
        async fn run(s: &mut SqlServerSession, sql: &str) {
            let mut out = QueryOutcome::default();
            s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        }
        let db = "dbine_index_usage_live";
        let reset = format!("IF DB_ID('{db}') IS NOT NULL BEGIN ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]; END");
        let mut admin = d.open(&cfg, None).await.expect("connect");
        run(&mut admin, &format!("{reset}; CREATE DATABASE [{db}]")).await;
        let mut s = d.open(&cfg, Some(db)).await.expect("connect to the test database");
        run(&mut s, "CREATE TABLE dbo.parent (id int PRIMARY KEY)").await;
        run(
            &mut s,
            "CREATE TABLE dbo.t (id int CONSTRAINT pk_t PRIMARY KEY, a int, b int, c int, p int CONSTRAINT fk_t_parent REFERENCES dbo.parent(id))",
        )
        .await;
        run(&mut s, "CREATE INDEX ix_seeked ON dbo.t (a) INCLUDE (c)").await;
        run(&mut s, "CREATE INDEX ix_untouched ON dbo.t (b DESC) WHERE b > 0").await;
        run(&mut s, "INSERT INTO dbo.t (id, a, b, c) SELECT TOP 500 ROW_NUMBER() OVER (ORDER BY (SELECT 1)), 1, 2, 3 FROM sys.all_objects").await;
        for i in 0..5 {
            run(&mut s, &format!("SELECT a, c FROM dbo.t WITH (INDEX(ix_seeked), FORCESEEK) WHERE a = {i}")).await;
        }
        let r = s.index_usage(&ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "t".into() }).await.unwrap().unwrap().derived();
        let get = |n: &str| r.indexes.iter().find(|i| i.name == n).unwrap_or_else(|| panic!("{n} in {r:?}"));
        assert!(r.stats_available, "{r:?}");
        assert!(r.since.as_deref().is_some_and(|s| s.len() == 19), "{:?}", r.since);
        assert_eq!(r.indexes.len(), 3, "no heap, the PK and two indexes");
        let pk = get("pk_t");
        assert!(pk.primary_key && pk.unique && pk.kind == "CLUSTERED", "{pk:?}");
        let seeked = get("ix_seeked");
        assert_eq!(seeked.seeks, 5, "{seeked:?}");
        assert_eq!((seeked.key_columns.clone(), seeked.included_columns.clone()), (vec!["a".to_string()], vec!["c".to_string()]));
        assert!(seeked.last_read.is_some() && seeked.size_kb.is_some_and(|k| k > 0), "{seeked:?}");
        let untouched = get("ix_untouched");
        assert_eq!((untouched.seeks, untouched.scans, untouched.lookups), (0, 0, 0), "{untouched:?}");
        assert!(untouched.updates > 0 && untouched.unused, "written by the insert, never read: {untouched:?}");
        assert_eq!(untouched.key_columns, ["b DESC"]);
        assert!(untouched.filter.as_deref().is_some_and(|f| f.contains("[b]>(0)")), "{untouched:?}");
        assert_eq!(untouched.read_share, Some(0.0));
        assert!(seeked.read_share.is_some_and(|v| v > 0.0));
        assert_eq!(r.foreign_keys.len(), 1);
        assert_eq!((r.foreign_keys[0].columns.clone(), r.foreign_keys[0].ref_table.as_str()), (vec!["p".to_string()], "parent"));
        drop(s);
        run(&mut admin, &reset).await;
    }

    /// Babelfish (`DBINE_TEST_BABELFISH_URL`; skipped without it): the
    /// catalog answers, and whatever its DMVs don't is left empty, not an error.
    #[tokio::test]
    #[ignore]
    async fn index_usage_babelfish_live() {
        use crate::{variant, SqlServerDriver};
        use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
        let Ok(url) = std::env::var("DBINE_TEST_BABELFISH_URL") else {
            eprintln!("DBINE_TEST_BABELFISH_URL not set; skipping");
            return;
        };
        let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
        let (auth, hostport) = rest.rsplit_once('@').unwrap();
        let (user, pass) = auth.split_once(':').unwrap();
        let (host, port) = hostport.rsplit_once(':').unwrap();
        let cfg = ConnectionConfig {
            driver: "babelfish".into(),
            host: host.into(),
            port: port.trim_end_matches('/').parse().unwrap(),
            username: Some(user.into()),
            password: Some(pass.into()),
            ..Default::default()
        };
        let d = SqlServerDriver { info: variant::info(Variant::Babelfish), variant: Variant::Babelfish };
        let mut s = d.open(&cfg, None).await.expect("connect");
        async fn run(s: &mut SqlServerSession, sql: &str) {
            let mut out = QueryOutcome::default();
            s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        }
        run(&mut s, "IF OBJECT_ID('dbo.dbine_ix_live') IS NOT NULL DROP TABLE dbo.dbine_ix_live").await;
        run(&mut s, "CREATE TABLE dbo.dbine_ix_live (id int PRIMARY KEY, a int, b int)").await;
        run(&mut s, "CREATE INDEX ix_live_a ON dbo.dbine_ix_live (a)").await;
        let r = s.index_usage(&ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "dbine_ix_live".into() }).await;
        run(&mut s, "DROP TABLE dbo.dbine_ix_live").await;
        let r = r.expect("no error").expect("reported").derived();
        eprintln!("{r:#?}");
        assert_eq!(r.indexes.len(), 2, "{r:?}");
        assert!(r.stats_available || r.note.is_some(), "{r:?}");
    }

    #[test]
    fn the_note_names_the_permission() {
        assert!(usage_note(Variant::SqlServer).contains("VIEW SERVER STATE"));
        assert!(usage_note(Variant::AzureSql).contains("VIEW DATABASE STATE"));
        assert!(usage_note(Variant::Babelfish).contains("Babelfish"));
    }
}
