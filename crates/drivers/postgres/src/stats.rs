//! What the catalog already knows about the session database's objects
//! ([`dbine_driver::Session::row_estimates`],
//! [`dbine_driver::Session::object_comments`]).
//!
//! Row counts only come from statistics the engine keeps, never from
//! counting:
//!
//! - PostgreSQL and most of the family: `pg_class.reltuples` of tables,
//!   partitioned tables and materialized views (-1, or 0 with no pages
//!   before PostgreSQL 14: never analyzed, left out).
//! - CockroachDB: `SHOW TABLES`' `estimated_row_count`, from its table
//!   statistics (`pg_class.reltuples` is always null there); a 0 only
//!   when `SHOW STATISTICS` has the table (0 is also "no statistics").
//! - Redshift: `svv_table_info.tbl_rows`, then `pg_class`.
//! - Yellowbrick: `sys.table` row counts, then `pg_class`.
//! - CrateDB: documents of the primary shards (`sys.shards`).
//! - RisingWave: keys of each table and materialized view's state
//!   (`rw_catalog.rw_table_stats`).
//! - H2: `information_schema.tables.row_count_estimate`.
//! - Materialize and Denodo: none (Materialize keeps sizes, not rows;
//!   Denodo's views read their sources live).
//!
//! Comments: `obj_description` of views, materialized views, sequences,
//! functions, procedures, triggers and types; `mz_internal.mz_comments` on
//! Materialize; `REMARKS` on H2; the views' description on Denodo. CrateDB
//! has no comments.
//!
//! Each query is its own: one that fails (an older version, no
//! permission, a catalog the variant emulates) is skipped.

use crate::catalog::{cell, lit};
use crate::compare::{has_types, DOMAIN};
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{kinds, ObjectRef, Result};
use std::collections::HashSet;
use std::time::Duration;
use tokio_postgres::SimpleQueryRow;

/// A catalog query is cancelled after this long.
const QUERY_LIMIT: Duration = Duration::from_secs(30);
/// CockroachDB tables at 0 rows whose statistics are looked up at most.
const MAX_ZERO_CHECKS: usize = 200;

/// An estimate as the catalog writes it (`1234`, `1.5e+06`, `-1`);
/// `None` when unknown or negative (never analyzed).
pub(crate) fn parse_rows(s: &str) -> Option<u64> {
    let s = s.trim();
    if let Ok(n) = s.parse::<i64>() {
        return u64::try_from(n).ok();
    }
    let f = s.parse::<f64>().ok()?;
    (f.is_finite() && f >= 0.0).then(|| f.round() as u64)
}

/// The explorer's kind of what a query returns: the kinds themselves, a
/// Materialize object type (`mz_objects.type`) or a RisingWave relation
/// type (`rw_relations.relation_type`).
pub(crate) fn kind_of(t: &str) -> Option<&'static str> {
    Some(match t {
        "table" => kinds::TABLE,
        "view" => kinds::VIEW,
        "materialized-view" | "materialized view" => kinds::MATERIALIZED_VIEW,
        "source" => crate::SOURCE,
        "sink" => crate::SINK,
        "type" => kinds::TYPE,
        "function" => kinds::FUNCTION,
        "procedure" => kinds::PROCEDURE,
        "trigger" => kinds::TRIGGER,
        "sequence" => kinds::SEQUENCE,
        _ => return None,
    })
}

fn object(kind: &str, schema: Option<String>, name: String) -> ObjectRef {
    ObjectRef { kind: kind.to_string(), schema: schema.filter(|s| !s.is_empty()), name }
}

/// `(kind, sch, name, rows)` rows as estimates; a kind the explorer
/// doesn't list and unknown counts are left out.
fn estimates(rows: &[SimpleQueryRow]) -> Vec<RowEstimate> {
    rows.iter()
        .filter_map(|r| {
            let kind = kind_of(&cell(r, "kind")?)?;
            let rows = parse_rows(&cell(r, "rows")?)?;
            Some(RowEstimate { object: object(kind, cell(r, "sch"), cell(r, "name")?), rows })
        })
        .collect()
}

impl PgSession {
    async fn stats_rows(&self, what: &str, sql: &str) -> Option<Vec<SimpleQueryRow>> {
        match self.text_within(sql, QUERY_LIMIT).await {
            Ok(rows) => Some(rows),
            Err(e) => {
                tracing::debug!("{:?}: {what} unavailable: {e}", self.variant);
                None
            }
        }
    }

    /// Row estimates from the planner's `pg_class.reltuples`.
    fn reltuples_sql(&self) -> String {
        // Before 14 a table that was never analyzed has 0 tuples and 0 pages.
        let analyzed = if self.version > 0 && self.version < 140000 { "c.reltuples > 0 OR c.relpages > 0" } else { "c.reltuples >= 0" };
        let kinds = if self.variant == Variant::Redshift { "'r'" } else { "'r', 'p', 'm'" };
        format!(
            "SELECT CASE WHEN c.relkind = 'm' THEN 'materialized view' ELSE 'table' END AS kind,
                    n.nspname AS sch, c.relname AS name, c.reltuples::bigint AS rows
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relkind IN ({kinds}) AND ({analyzed}) AND {}",
            self.filter("n.nspname")
        )
    }

    /// The queries to try, in order, until one works.
    fn row_queries(&self) -> Vec<String> {
        let v = self.variant;
        match v {
            Variant::Materialize | Variant::Denodo => Vec::new(),
            Variant::Cockroach => vec![format!(
                "SELECT CASE WHEN type = 'materialized view' THEN type ELSE 'table' END AS kind,
                        schema_name AS sch, table_name AS name, estimated_row_count AS rows
                 FROM [SHOW TABLES] WHERE type IN ('table', 'materialized view') AND estimated_row_count IS NOT NULL AND {}",
                self.filter("schema_name")
            )],
            Variant::Redshift => vec![
                format!(
                    "SELECT 'table' AS kind, \"schema\" AS sch, \"table\" AS name, tbl_rows AS rows
                     FROM svv_table_info WHERE \"database\" = current_database() AND {}",
                    self.filter("\"schema\"")
                ),
                self.reltuples_sql(),
            ],
            Variant::Yellowbrick => vec![
                format!(
                    "SELECT 'table' AS kind, s.name AS sch, t.name AS name,
                            coalesce(t.rows_columnstore, 0) + coalesce(t.rows_rowstore, 0) AS rows
                     FROM sys.table t JOIN sys.schema s ON s.schema_id = t.schema_id AND s.database_id = t.database_id
                     JOIN sys.database d ON d.database_id = t.database_id
                     WHERE d.name = current_database() AND {}",
                    self.filter("s.name")
                ),
                self.reltuples_sql(),
            ],
            Variant::CrateDb => vec![format!(
                "SELECT 'table' AS kind, schema_name AS sch, table_name AS name, sum(num_docs) AS rows
                 FROM sys.shards WHERE \"primary\" AND {} GROUP BY schema_name, table_name",
                self.filter("schema_name")
            )],
            Variant::RisingWave => vec![format!(
                "SELECT r.relation_type AS kind, sc.name AS sch, r.name AS name, s.total_key_count AS rows
                 FROM rw_catalog.rw_table_stats s
                 JOIN rw_catalog.rw_relations r ON r.id = s.id
                 JOIN rw_catalog.rw_schemas sc ON sc.id = r.schema_id
                 WHERE r.relation_type IN ('table', 'materialized view') AND {}",
                self.filter("sc.name")
            )],
            Variant::H2 => vec![format!(
                "SELECT 'table' AS kind, table_schema AS sch, table_name AS name, row_count_estimate AS rows
                 FROM information_schema.tables WHERE table_type = 'BASE TABLE' AND {}",
                self.filter("table_schema")
            )],
            _ => vec![self.reltuples_sql()],
        }
    }

    pub(crate) async fn row_estimates_impl(&self) -> Result<Vec<RowEstimate>> {
        for sql in self.row_queries() {
            if let Some(rows) = self.stats_rows("row estimates", &sql).await {
                let mut out = estimates(&rows);
                if self.variant == Variant::Cockroach {
                    self.drop_unanalyzed(&mut out).await;
                }
                return Ok(out);
            }
        }
        Ok(Vec::new())
    }

    /// CockroachDB reports 0 rows for a table without statistics too: a 0
    /// stays only when the table has statistics (`SHOW STATISTICS` reads
    /// `system.table_statistics`, not the table), for the first
    /// [`MAX_ZERO_CHECKS`] of them.
    async fn drop_unanalyzed(&self, out: &mut Vec<RowEstimate>) {
        let mut checked = 0;
        let mut keep = Vec::with_capacity(out.len());
        for e in out.drain(..) {
            if e.rows > 0 {
                keep.push(e);
                continue;
            }
            if checked >= MAX_ZERO_CHECKS {
                continue;
            }
            checked += 1;
            let q = qualified_name(Quote::Double, e.object.schema(), &e.object.name);
            let sql = format!("SELECT row_count AS rows FROM [SHOW STATISTICS FOR TABLE {q}] ORDER BY created DESC LIMIT 1");
            let rows = self.stats_rows("table statistics", &sql).await.unwrap_or_default();
            if let Some(n) = rows.first().and_then(|r| cell(r, "rows")).and_then(|v| parse_rows(&v)) {
                keep.push(RowEstimate { rows: n, ..e });
            }
        }
        *out = keep;
    }

    /// `(kind, sch, name, comment)` queries, each for its own kinds.
    fn comment_queries(&self) -> Vec<String> {
        let v = self.variant;
        let filter = self.filter("n.nspname");
        let nonempty = "WHERE x.comment IS NOT NULL AND x.comment <> ''";
        match v {
            Variant::CrateDb => Vec::new(),
            Variant::Denodo => vec![format!(
                "SELECT 'view' AS kind, name, description AS comment
                 FROM GET_VIEWS() WHERE input_database_name = {} AND view_type <> 0 AND description <> ''",
                lit(v, &self.database)
            )],
            Variant::H2 => vec![
                format!(
                    "SELECT 'view' AS kind, table_schema AS sch, table_name AS name, remarks AS comment
                     FROM information_schema.tables WHERE table_type = 'VIEW' AND remarks <> '' AND {}",
                    self.filter("table_schema")
                ),
                format!(
                    "SELECT 'sequence' AS kind, sequence_schema AS sch, sequence_name AS name, remarks AS comment
                     FROM information_schema.sequences WHERE remarks <> '' AND {}",
                    self.filter("sequence_schema")
                ),
                format!(
                    "SELECT '{DOMAIN}' AS kind, domain_schema AS sch, domain_name AS name, remarks AS comment
                     FROM information_schema.domains WHERE remarks <> '' AND {}",
                    self.filter("domain_schema")
                ),
            ],
            Variant::Materialize => vec![format!(
                "SELECT o.type AS kind, s.name AS sch, o.name AS name, c.comment AS comment
                 FROM mz_internal.mz_comments c
                 JOIN mz_catalog.mz_objects o ON o.id = c.id
                 JOIN mz_catalog.mz_schemas s ON s.id = o.schema_id
                 JOIN mz_catalog.mz_databases d ON d.id = s.database_id
                 WHERE c.object_sub_id IS NULL AND o.type <> 'table' AND c.comment <> '' AND d.name = {}",
                lit(v, &self.database)
            )],
            // COMMENT ON only takes tables, views and columns there.
            Variant::Redshift => vec![format!(
                "SELECT * FROM (SELECT 'view' AS kind, n.nspname AS sch, c.relname AS name,
                                       obj_description(c.oid, 'pg_class') AS comment
                                FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                                WHERE c.relkind = 'v' AND {filter}) x {nonempty}"
            )],
            _ => {
                let mut out = vec![
                    format!(
                        "SELECT * FROM (SELECT CASE c.relkind WHEN 'v' THEN 'view' WHEN 'm' THEN 'materialized view' ELSE 'sequence' END AS kind,
                                               n.nspname AS sch, c.relname AS name, obj_description(c.oid, 'pg_class') AS comment
                                        FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
                                        WHERE c.relkind IN ('v', 'm', 'S') AND {filter}) x {nonempty}"
                    ),
                    format!(
                        "SELECT * FROM (SELECT {kind} AS kind, n.nspname AS sch, p.proname AS name,
                                               obj_description(p.oid, 'pg_proc') AS comment
                                        FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
                                        WHERE {routines} AND {filter}) x {nonempty}",
                        kind = self.routine_kind(),
                        routines = self.routine_filter(),
                    ),
                ];
                if !v.streaming() {
                    out.push(format!(
                        "SELECT * FROM (SELECT 'trigger' AS kind, n.nspname AS sch, t.tgname AS name,
                                               obj_description(t.oid, 'pg_trigger') AS comment
                                        FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid
                                        JOIN pg_namespace n ON n.oid = c.relnamespace
                                        WHERE NOT t.tgisinternal AND {filter}) x {nonempty}"
                    ));
                }
                if has_types(v) {
                    out.push(format!(
                        "SELECT * FROM (SELECT 'type' AS kind, n.nspname AS sch, t.typname AS name,
                                               obj_description(t.oid, 'pg_type') AS comment
                                        FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace
                                        WHERE t.typtype IN ('e', 'd', 'c', 'r')
                                          AND (t.typtype <> 'c' OR NOT EXISTS (SELECT 1 FROM pg_class rc
                                                                               WHERE rc.oid = t.typrelid AND rc.relkind <> 'c'))
                                          AND {filter}) x {nonempty}"
                    ));
                }
                out
            }
        }
    }

    pub(crate) async fn object_comments_impl(&self) -> Result<Vec<ObjectComment>> {
        let with_schema = self.variant != Variant::Denodo;
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for sql in self.comment_queries() {
            let Some(rows) = self.stats_rows("object comments", &sql).await else { continue };
            for r in &rows {
                let (Some(kind), Some(name), Some(comment)) = (cell(r, "kind"), cell(r, "name"), cell(r, "comment")) else {
                    continue;
                };
                let kind = if kind == DOMAIN { DOMAIN } else if let Some(k) = kind_of(&kind) { k } else { continue };
                if comment.trim().is_empty() {
                    continue;
                }
                let o = object(kind, if with_schema { cell(r, "sch") } else { None }, name);
                // Overloaded routines: the first comment stands for the name.
                if seen.insert((o.kind.clone(), o.schema.clone(), o.name.clone())) {
                    out.push(ObjectComment { object: o, comment });
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_parse_as_the_catalogs_write_them() {
        assert_eq!(parse_rows("1234"), Some(1234));
        assert_eq!(parse_rows(" 0 "), Some(0));
        assert_eq!(parse_rows("1.5e+06"), Some(1_500_000));
        assert_eq!(parse_rows("42.4"), Some(42));
        assert_eq!(parse_rows("-1"), None);
        assert_eq!(parse_rows("-1.0"), None);
        assert_eq!(parse_rows("NaN"), None);
        assert_eq!(parse_rows(""), None);
    }

    #[test]
    fn engine_types_map_to_the_explorer_kinds() {
        assert_eq!(kind_of("materialized-view"), Some(kinds::MATERIALIZED_VIEW));
        assert_eq!(kind_of("materialized view"), Some(kinds::MATERIALIZED_VIEW));
        assert_eq!(kind_of("table"), Some(kinds::TABLE));
        assert_eq!(kind_of("source"), Some(crate::SOURCE));
        assert_eq!(kind_of("sink"), Some(crate::SINK));
        assert_eq!(kind_of("index"), None);
        assert_eq!(kind_of("secret"), None);
    }
}
