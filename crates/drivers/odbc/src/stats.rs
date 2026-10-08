//! What each preset's catalog already knows about the database's objects
//! ([`dbine_driver::Session::row_estimates`],
//! [`dbine_driver::Session::object_comments`]).
//!
//! Rows come only from statistics the engine keeps, never a count:
//!
//! - Db2 (LUW): `SYSCAT.TABLES.CARD` (RUNSTATS; -1 = never analyzed).
//! - Db2 for z/OS: `SYSIBM.SYSTABLES.CARDF` (RUNSTATS; -1 = never).
//! - Db2 for i: `QSYS2.SYSTABLESTAT.NUMBER_ROWS` (the object description).
//! - Sybase ASE: `row_count()` (systabstats, kept by the server).
//! - SQL Anywhere: `SYS.SYSTAB.count` (kept at each checkpoint).
//! - Informix and GBase 8s: `systables.nrows` of the tables UPDATE
//!   STATISTICS has seen (`ustlowts` set).
//! - Teradata: `DBC.StatsV.RowCount` (COLLECT STATISTICS).
//! - Vertica: `v_monitor.projection_storage.row_count`, per projection
//!   (summed over nodes when segmented), the largest projection.
//! - Exasol: `EXA_ALL_TABLES.TABLE_ROW_COUNT`.
//! - Netezza: `_V_TABLE.RELTUPLES`.
//! - Dameng: `ALL_TABLES.NUM_ROWS` (DBMS_STATS; NULL = never).
//! - MonetDB: `sys.tablestorage.rowcount` (the column heaps' counts).
//! - Ingres: `iitables.num_rows`.
//! - SQream: `sqream_catalog.tables.row_count`.
//! - SQL Server behind the generic preset: `sys.partitions.rows`.
//!
//! Comments (views, routines, sequences, aliases, types) where the
//! engine keeps them: Db2 (`REMARKS`), Db2 for i (`LONG_COMMENT`), SQL
//! Anywhere (`remarks`), Teradata (`CommentString`), Vertica
//! (`v_catalog.comments`), Exasol (`*_COMMENT`), Netezza (`DESCRIPTION`),
//! Dameng (`ALL_TAB_COMMENTS`), MonetDB (`sys.comments`) and SQL Server
//! (`MS_Description`). ASE, Informix and Ingres comment only tables and
//! columns (or nothing).
//!
//! Every other preset: none. Hive, Impala and Spark keep their stats per
//! table in the metastore (`DESCRIBE FORMATTED`, `SHOW TABLE STATS`, one
//! round trip per table); the rest keep no row statistics a catalog query
//! reaches, or DBINE doesn't know one that is safe to read. Each query is
//! its own: one that fails (no access, an older release) is skipped.

use crate::design::{self, Eng};
use crate::presets::Preset;
use crate::{col, monitor, OdbcSession, Rows};
use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::{kinds, ObjectRef, Result};

/// Queries of `(schema, table, rows)`, tried in order until one works.
pub(crate) fn rows_sql(e: Eng) -> &'static [&'static str] {
    match e {
        Eng::Db2 => &["SELECT TABSCHEMA, TABNAME, CARD FROM SYSCAT.TABLES WHERE TYPE = 'T' AND CARD >= 0"],
        Eng::Db2zos => &["SELECT CREATOR, NAME, CARDF FROM SYSIBM.SYSTABLES WHERE TYPE = 'T' AND CARDF >= 0"],
        Eng::Db2i => &["SELECT TABLE_SCHEMA, TABLE_NAME, NUMBER_ROWS FROM QSYS2.SYSTABLESTAT WHERE TABLE_SCHEMA NOT LIKE 'Q%'"],
        Eng::Ase => &["SELECT user_name(uid), name, row_count(db_id(), id) FROM sysobjects WHERE type = 'U'"],
        Eng::Sqla => &["SELECT u.user_name, t.table_name, t.count FROM SYS.SYSTAB t JOIN SYS.SYSUSER u ON u.user_id = t.creator
                        WHERE t.table_type = 1"],
        Eng::Informix => &["SELECT TRIM(owner), tabname, nrows FROM systables
                            WHERE tabid >= 100 AND tabtype = 'T' AND ustlowts IS NOT NULL"],
        Eng::Teradata => &["SELECT TRIM(DatabaseName), TRIM(TableName), MAX(RowCount) FROM DBC.StatsV
                            WHERE RowCount IS NOT NULL GROUP BY 1, 2"],
        Eng::Vertica => &["SELECT s.anchor_table_schema, s.anchor_table_name,
                                  MAX(CASE WHEN p.is_segmented THEN s.total ELSE s.most END)
                             FROM (SELECT anchor_table_schema, anchor_table_name, projection_schema, projection_name,
                                          SUM(row_count) AS total, MAX(row_count) AS most
                                     FROM v_monitor.projection_storage GROUP BY 1, 2, 3, 4) s
                             JOIN v_catalog.projections p
                               ON p.projection_schema = s.projection_schema AND p.projection_name = s.projection_name
                            GROUP BY 1, 2"],
        Eng::Exasol => &["SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_ROW_COUNT FROM EXA_ALL_TABLES"],
        Eng::Netezza => &["SELECT SCHEMA, TABLENAME, RELTUPLES FROM _V_TABLE WHERE OBJTYPE = 'TABLE'"],
        Eng::Dameng => &["SELECT OWNER, TABLE_NAME, NUM_ROWS FROM ALL_TABLES WHERE NUM_ROWS IS NOT NULL"],
        Eng::MonetDb => &["SELECT \"schema\", \"table\", MAX(rowcount) FROM sys.tablestorage GROUP BY \"schema\", \"table\""],
        Eng::Ingres => &["SELECT TRIM(table_owner), TRIM(table_name), num_rows FROM iitables
                          WHERE table_type = 'T' AND system_use <> 'S'"],
        Eng::Sqream => &["SELECT schema_name, table_name, row_count FROM sqream_catalog.tables"],
        Eng::SqlServer => &["SELECT s.name, t.name, SUM(p.rows) FROM sys.tables t
                               JOIN sys.schemas s ON s.schema_id = t.schema_id
                               JOIN sys.partitions p ON p.object_id = t.object_id AND p.index_id IN (0, 1)
                              GROUP BY s.name, t.name"],
        _ => &[],
    }
}

/// Queries of `(kind, schema, name, comment)`, each run on its own.
pub(crate) fn comments_sql(e: Eng) -> &'static [&'static str] {
    match e {
        Eng::Db2 => &[
            "SELECT 'view', TABSCHEMA, TABNAME, REMARKS FROM SYSCAT.TABLES WHERE TYPE = 'V' AND REMARKS IS NOT NULL",
            "SELECT 'synonym', TABSCHEMA, TABNAME, REMARKS FROM SYSCAT.TABLES WHERE TYPE = 'A' AND REMARKS IS NOT NULL",
            "SELECT CASE ROUTINETYPE WHEN 'P' THEN 'procedure' ELSE 'function' END, ROUTINESCHEMA, ROUTINENAME, REMARKS
               FROM SYSCAT.ROUTINES WHERE ROUTINETYPE IN ('P', 'F') AND REMARKS IS NOT NULL",
            "SELECT 'sequence', SEQSCHEMA, SEQNAME, REMARKS FROM SYSCAT.SEQUENCES WHERE SEQTYPE = 'S' AND REMARKS IS NOT NULL",
            "SELECT 'type', TYPESCHEMA, TYPENAME, REMARKS FROM SYSCAT.DATATYPES WHERE METATYPE IN ('T', 'R') AND REMARKS IS NOT NULL",
        ],
        Eng::Db2zos => &[
            "SELECT 'view', CREATOR, NAME, REMARKS FROM SYSIBM.SYSTABLES WHERE TYPE = 'V' AND REMARKS <> ''",
            "SELECT CASE ROUTINETYPE WHEN 'P' THEN 'procedure' ELSE 'function' END, SCHEMA, NAME, REMARKS
               FROM SYSIBM.SYSROUTINES WHERE ROUTINETYPE IN ('P', 'F') AND REMARKS <> ''",
            "SELECT 'sequence', SCHEMA, NAME, REMARKS FROM SYSIBM.SYSSEQUENCES WHERE SEQTYPE = 'S' AND REMARKS <> ''",
        ],
        Eng::Db2i => &[
            "SELECT 'view', TABLE_SCHEMA, TABLE_NAME, LONG_COMMENT FROM QSYS2.SYSTABLES
              WHERE TABLE_TYPE = 'V' AND LONG_COMMENT IS NOT NULL AND TABLE_SCHEMA NOT LIKE 'Q%'",
            "SELECT CASE ROUTINE_TYPE WHEN 'PROCEDURE' THEN 'procedure' ELSE 'function' END, ROUTINE_SCHEMA, ROUTINE_NAME, LONG_COMMENT
               FROM QSYS2.SYSROUTINES WHERE LONG_COMMENT IS NOT NULL AND ROUTINE_SCHEMA NOT LIKE 'Q%'",
            "SELECT 'sequence', SEQUENCE_SCHEMA, SEQUENCE_NAME, LONG_COMMENT FROM QSYS2.SYSSEQUENCES
              WHERE LONG_COMMENT IS NOT NULL AND SEQUENCE_SCHEMA NOT LIKE 'Q%'",
        ],
        Eng::Sqla => &[
            "SELECT 'view', USER_NAME(creator), table_name, remarks FROM SYS.SYSTABLE WHERE table_type = 'VIEW' AND remarks IS NOT NULL",
            "SELECT 'procedure', USER_NAME(creator), proc_name, remarks FROM SYS.SYSPROCEDURE WHERE remarks IS NOT NULL",
        ],
        Eng::Teradata => &["SELECT CASE TableKind WHEN 'V' THEN 'view' WHEN 'F' THEN 'function' ELSE 'procedure' END,
                                   TRIM(DatabaseName), TRIM(TableName), CommentString
                              FROM DBC.TablesV WHERE TableKind IN ('V', 'P', 'E', 'F') AND CommentString IS NOT NULL"],
        Eng::Vertica => &["SELECT CASE object_type WHEN 'VIEW' THEN 'view' WHEN 'FUNCTION' THEN 'function'
                                   WHEN 'SEQUENCE' THEN 'sequence' ELSE 'procedure' END,
                                  object_schema, object_name, comment
                             FROM v_catalog.comments WHERE object_type IN ('VIEW', 'FUNCTION', 'SEQUENCE', 'PROCEDURE')"],
        Eng::Exasol => &[
            "SELECT 'view', VIEW_SCHEMA, VIEW_NAME, VIEW_COMMENT FROM EXA_ALL_VIEWS WHERE VIEW_COMMENT IS NOT NULL",
            "SELECT 'function', FUNCTION_SCHEMA, FUNCTION_NAME, FUNCTION_COMMENT FROM EXA_ALL_FUNCTIONS WHERE FUNCTION_COMMENT IS NOT NULL",
            "SELECT 'procedure', SCRIPT_SCHEMA, SCRIPT_NAME, SCRIPT_COMMENT FROM EXA_ALL_SCRIPTS WHERE SCRIPT_COMMENT IS NOT NULL",
        ],
        Eng::Netezza => &[
            "SELECT 'view', SCHEMA, VIEWNAME, DESCRIPTION FROM _V_VIEW WHERE DESCRIPTION IS NOT NULL",
            "SELECT 'procedure', SCHEMA, PROCEDURE, DESCRIPTION FROM _V_PROCEDURE WHERE DESCRIPTION IS NOT NULL",
        ],
        Eng::Dameng => &["SELECT 'view', OWNER, TABLE_NAME, COMMENTS FROM ALL_TAB_COMMENTS WHERE TABLE_TYPE = 'VIEW' AND COMMENTS IS NOT NULL"],
        Eng::MonetDb => &[
            "SELECT 'view', s.name, t.name, c.remark FROM sys.comments c JOIN sys.tables t ON t.id = c.id
               JOIN sys.schemas s ON s.id = t.schema_id WHERE t.type = 1 AND NOT t.system",
            "SELECT CASE f.type WHEN 2 THEN 'procedure' ELSE 'function' END, s.name, f.name, c.remark
               FROM sys.comments c JOIN sys.functions f ON f.id = c.id JOIN sys.schemas s ON s.id = f.schema_id WHERE NOT f.system",
            "SELECT 'sequence', s.name, q.name, c.remark FROM sys.comments c JOIN sys.sequences q ON q.id = c.id
               JOIN sys.schemas s ON s.id = q.schema_id",
        ],
        // The generic preset lists every SQL Server routine as a procedure
        // (SQLProcedures says "returns a value" for all of them).
        Eng::SqlServer => &["SELECT CASE o.type WHEN 'V' THEN 'view' ELSE 'procedure' END, s.name, o.name, CAST(ep.value AS NVARCHAR(4000))
                               FROM sys.extended_properties ep
                               JOIN sys.objects o ON o.object_id = ep.major_id
                               JOIN sys.schemas s ON s.schema_id = o.schema_id
                              WHERE ep.class = 1 AND ep.minor_id = 0 AND ep.name = 'MS_Description'
                                AND o.type IN ('V', 'P', 'PC', 'FN', 'IF', 'TF', 'FS', 'FT')"],
        _ => &[],
    }
}

/// The engine behind a preset; the generic one goes by the DBMS name.
fn engine(preset: &Preset, dbms: &str) -> Eng {
    match design::eng(preset) {
        Eng::Generic => monitor::eng_from_dbms(dbms).unwrap_or(Eng::Generic),
        e => e,
    }
}

/// A catalog schema as `list_objects` gives it: `None` for presets
/// without schemas, and for system schemas (left out).
fn schema_of(preset: &Preset, s: Option<String>) -> Option<Option<String>> {
    let s = s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if s.as_deref().is_some_and(|s| preset.is_system_schema(s)) {
        return None;
    }
    Some(if preset.has_schemas { s } else { None })
}

/// A row count as text (`250`, `250.0`, `2.5E2`); negative (never
/// analyzed) or unreadable: none.
fn count(v: Option<String>) -> Option<u64> {
    let n: f64 = v?.trim().parse().ok()?;
    (n.is_finite() && n >= 0.0).then(|| n.round() as u64)
}

fn estimates(preset: &Preset, rows: &Rows) -> Vec<RowEstimate> {
    rows.iter()
        .filter_map(|r| {
            let schema = schema_of(preset, col(r, 0))?;
            let name = col(r, 1)?.trim_end().to_string();
            Some(RowEstimate { object: ObjectRef { kind: kinds::TABLE.into(), schema, name }, rows: count(col(r, 2))? })
        })
        .collect()
}

fn comments(preset: &Preset, rows: &Rows) -> Vec<ObjectComment> {
    rows.iter()
        .filter_map(|r| {
            let kind = col(r, 0)?.trim().to_string();
            let schema = schema_of(preset, col(r, 1))?;
            let name = col(r, 2)?.trim_end().to_string();
            let comment = col(r, 3)?.trim().to_string();
            (!comment.is_empty()).then(|| ObjectComment { object: ObjectRef { kind, schema, name }, comment })
        })
        .collect()
}

impl OdbcSession {
    pub(crate) async fn row_estimates_impl(&mut self) -> Result<Vec<RowEstimate>> {
        for sql in rows_sql(engine(self.preset, &self.version)) {
            match self.query(sql.to_string(), Vec::new()).await {
                Ok(rows) => return Ok(estimates(self.preset, &rows)),
                Err(e) => tracing::debug!("odbc: row estimates not read: {e}"),
            }
        }
        Ok(Vec::new())
    }

    pub(crate) async fn object_comments_impl(&mut self) -> Result<Vec<ObjectComment>> {
        let mut out = Vec::new();
        for sql in comments_sql(engine(self.preset, &self.version)) {
            match self.query(sql.to_string(), Vec::new()).await {
                Ok(rows) => out.extend(comments(self.preset, &rows)),
                Err(e) => tracing::debug!("odbc: object comments not read: {e}"),
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::PRESETS;

    fn preset(id: &str) -> &'static Preset {
        PRESETS.iter().find(|p| p.id == id).unwrap()
    }

    fn row(v: &[Option<&str>]) -> Vec<Option<String>> {
        v.iter().map(|c| c.map(str::to_string)).collect()
    }

    #[test]
    fn counts_in_any_spelling() {
        assert_eq!(count(Some("250".into())), Some(250));
        assert_eq!(count(Some(" 2.5E2 ".into())), Some(250));
        assert_eq!(count(Some("249.6".into())), Some(250));
        assert_eq!(count(Some("-1".into())), None);
        assert_eq!(count(Some("x".into())), None);
        assert_eq!(count(None), None);
    }

    #[test]
    fn estimates_skip_system_schemas_and_unanalyzed_tables() {
        let rows = vec![
            row(&[Some("APP   "), Some("CLIENTES"), Some("250")]),
            row(&[Some("SYSIBM"), Some("SYSTABLES"), Some("900")]),
            row(&[Some("APP"), Some("NUEVA"), Some("-1")]),
        ];
        let e = estimates(preset("db2"), &rows);
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].object.kind.as_str(), e[0].object.schema.as_deref(), e[0].object.name.as_str(), e[0].rows), ("table", Some("APP"), "CLIENTES", 250));
        // CUBRID has no schemas: none on the reference either.
        let e = estimates(preset("cubrid"), &vec![row(&[Some("dba"), Some("t"), Some("3")])]);
        assert_eq!(e[0].object.schema, None);
    }

    #[test]
    fn comments_keep_their_kind() {
        let rows = vec![
            row(&[Some("view"), Some("APP"), Some("V_CLIENTES"), Some("Clientes visibles ")]),
            row(&[Some("procedure"), Some("APP"), Some("P_ALTA"), Some("  ")]),
            row(&[Some("sequence"), Some("SYSIBM"), Some("S"), Some("sistema")]),
        ];
        let c = comments(preset("db2"), &rows);
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].object.kind.as_str(), c[0].object.name.as_str(), c[0].comment.as_str()), ("view", "V_CLIENTES", "Clientes visibles"));
    }

    #[test]
    fn generic_preset_goes_by_the_dbms() {
        assert!(matches!(engine(preset("odbc"), "Microsoft SQL Server 16.00.4135"), Eng::SqlServer));
        assert!(!rows_sql(engine(preset("odbc"), "Microsoft SQL Server 16.00.4135")).is_empty());
        assert!(rows_sql(engine(preset("odbc"), "SomethingElse 1.0")).is_empty());
        assert!(matches!(engine(preset("gbase8s"), ""), Eng::Informix));
    }

    #[test]
    fn comment_kinds_are_listed_kinds() {
        let listed = [kinds::VIEW, kinds::PROCEDURE, kinds::FUNCTION, kinds::SEQUENCE, kinds::SYNONYM, kinds::TYPE];
        for p in PRESETS.iter() {
            for sql in comments_sql(design::eng(p)) {
                // Every literal kind a query yields is one list_objects uses.
                for lit in sql.split('\'').skip(1).step_by(2) {
                    if lit.chars().all(|c| c.is_ascii_lowercase()) && !lit.is_empty() {
                        assert!(listed.contains(&lit), "{}: '{lit}'", p.id);
                    }
                }
            }
        }
    }
}
