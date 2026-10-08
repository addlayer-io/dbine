//! What the catalog already knows about the database's objects
//! ([`dbine_driver::Session::row_estimates`],
//! [`dbine_driver::Session::object_comments`]), as in PostgreSQL.
//!
//! - Rows: `pg_class.reltuples`, the planner's estimate that `ANALYZE`
//!   (DSQL runs it on its own) keeps. `-1` means never analyzed and is left
//!   out. No table is read.
//! - Comments: `obj_description` of views, sequences and functions. A
//!   catalog read DSQL refuses is skipped.

use crate::{DsqlSession, SYSTEM_SCHEMAS};
use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::{kinds, ObjectRef, Result};

/// `reltuples` as a count: negative (never analyzed) is none.
pub(crate) fn tuples(v: f64) -> Option<u64> {
    (v.is_finite() && v >= 0.0).then(|| v.round() as u64)
}

/// A `relkind` (or `f` for a function) as the kind `list_objects` gives it.
pub(crate) fn kind_of(relkind: &str) -> &'static str {
    match relkind {
        "v" | "m" => kinds::VIEW,
        "S" => kinds::SEQUENCE,
        "f" => kinds::FUNCTION,
        _ => kinds::TABLE,
    }
}

pub(crate) async fn row_estimates(s: &DsqlSession) -> Result<Vec<RowEstimate>> {
    let sql = format!(
        "SELECT n.nspname::text, c.relname::text, c.reltuples::float8
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         WHERE c.relkind IN ('r', 'p') AND n.nspname NOT IN {SYSTEM_SCHEMAS} AND n.nspname NOT LIKE 'pg\\_%'
         ORDER BY 1, 2"
    );
    let rows = s.client.query(sql.as_str(), &[]).await.unwrap_or_default();
    Ok(rows
        .iter()
        .filter_map(|r| {
            Some(RowEstimate {
                object: ObjectRef { kind: kinds::TABLE.into(), schema: Some(r.get(0)), name: r.get(1) },
                rows: tuples(r.get(2))?,
            })
        })
        .collect())
}

pub(crate) async fn object_comments(s: &DsqlSession) -> Result<Vec<ObjectComment>> {
    let sql = format!(
        "SELECT n.nspname::text, c.relname::text, c.relkind::text, pg_catalog.obj_description(c.oid, 'pg_class')
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
         WHERE c.relkind IN ('v', 'm', 'S') AND n.nspname NOT IN {SYSTEM_SCHEMAS} AND n.nspname NOT LIKE 'pg\\_%'
         UNION ALL
         SELECT n.nspname::text, p.proname::text, 'f', pg_catalog.obj_description(p.oid, 'pg_proc')
         FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
         WHERE n.nspname NOT IN {SYSTEM_SCHEMAS} AND n.nspname NOT LIKE 'pg\\_%'
         ORDER BY 1, 2"
    );
    let rows = s.client.query(sql.as_str(), &[]).await.unwrap_or_default();
    let mut out: Vec<ObjectComment> = Vec::new();
    for r in &rows {
        let Some(comment) = r.get::<_, Option<String>>(3).map(|c| c.trim().to_string()).filter(|c| !c.is_empty()) else { continue };
        let object = ObjectRef { kind: kind_of(&r.get::<_, String>(2)).into(), schema: Some(r.get(0)), name: r.get(1) };
        // Overloaded functions: one comment per name, the first.
        if !out.iter().any(|c| c.object.kind == object.kind && c.object.schema == object.schema && c.object.name == object.name) {
            out.push(ObjectComment { object, comment });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reltuples() {
        assert_eq!(tuples(-1.0), None);
        assert_eq!(tuples(0.0), Some(0));
        assert_eq!(tuples(1199.6), Some(1200));
        assert_eq!(tuples(f64::NAN), None);
    }

    #[test]
    fn kinds_match_list_objects() {
        assert_eq!(kind_of("v"), kinds::VIEW);
        assert_eq!(kind_of("S"), kinds::SEQUENCE);
        assert_eq!(kind_of("f"), kinds::FUNCTION);
    }
}
