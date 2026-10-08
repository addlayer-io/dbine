//! What the catalog already knows about the open database's objects
//! ([`dbine_driver::Session::row_estimates`],
//! [`dbine_driver::Session::object_comments`]).
//!
//! - Rows: `duckdb_tables().estimated_size`, the row count DuckDB keeps in
//!   the table's storage metadata (no scan). Tables of attached catalogs
//!   from other engines report none.
//! - Comments (`COMMENT ON`): views, macros, sequences and types, with the
//!   same kind ids as `list_objects`. Tables and columns come with
//!   `database_schema`.

use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::{kinds, ObjectRef};
use std::collections::BTreeSet;

/// schema, table, estimated rows (`?1`: the catalog).
pub const ROWS: &str = "SELECT schema_name, table_name, estimated_size::VARCHAR FROM duckdb_tables()
 WHERE database_name = ?1 AND NOT internal";

/// kind, schema, name, comment (`?1`: the catalog).
pub const COMMENTS: &str = "SELECT 'view', schema_name, view_name, comment FROM duckdb_views()
   WHERE database_name = ?1 AND NOT internal AND comment <> ''
 UNION ALL
 SELECT DISTINCT 'function', schema_name, function_name, comment FROM duckdb_functions()
   WHERE database_name = ?1 AND NOT internal AND function_type IN ('macro', 'table_macro') AND comment <> ''
 UNION ALL
 SELECT 'sequence', schema_name, sequence_name, comment FROM duckdb_sequences()
   WHERE database_name = ?1 AND comment <> ''
 UNION ALL
 SELECT 'type', schema_name, type_name, comment FROM duckdb_types()
   WHERE database_name = ?1 AND NOT internal AND comment <> ''";

pub fn row_estimates(rows: Vec<Vec<Option<String>>>) -> Vec<RowEstimate> {
    rows.into_iter()
        .filter_map(|r| {
            let mut r = r.into_iter();
            let (schema, name, rows) = (r.next()?, r.next()??, r.next()??);
            Some(RowEstimate { object: ObjectRef { kind: kinds::TABLE.into(), schema, name }, rows: rows.trim().parse().ok()? })
        })
        .collect()
}

/// One comment per object: overloads of a macro share the name, and the
/// first comment wins.
pub fn object_comments(rows: Vec<Vec<Option<String>>>) -> Vec<ObjectComment> {
    let mut seen = BTreeSet::new();
    rows.into_iter()
        .filter_map(|r| {
            let mut r = r.into_iter();
            let (kind, schema, name, comment) = (r.next()??, r.next()?, r.next()??, r.next()??);
            if comment.trim().is_empty() || !seen.insert((kind.clone(), schema.clone(), name.clone())) {
                return None;
            }
            Some(ObjectComment { object: ObjectRef { kind, schema, name }, comment })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cells: &[Option<&str>]) -> Vec<Option<String>> {
        cells.iter().map(|c| c.map(str::to_string)).collect()
    }

    #[test]
    fn rows_skip_unknown_sizes() {
        let got = row_estimates(vec![row(&[Some("main"), Some("t"), Some("1000")]), row(&[Some("main"), Some("ext"), None])]);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].object.kind.as_str(), got[0].object.schema.as_deref(), got[0].object.name.as_str(), got[0].rows), ("table", Some("main"), "t", 1000));
    }

    #[test]
    fn comments_one_per_object() {
        let got = object_comments(vec![
            row(&[Some("view"), Some("main"), Some("v"), Some("Ventas")]),
            row(&[Some("function"), Some("main"), Some("m"), Some("uno")]),
            row(&[Some("function"), Some("main"), Some("m"), Some("dos")]),
            row(&[Some("type"), Some("main"), Some("mood"), Some(" ")]),
        ]);
        let got: Vec<_> = got.iter().map(|c| (c.object.kind.as_str(), c.object.name.as_str(), c.comment.as_str())).collect();
        assert_eq!(got, vec![("view", "v", "Ventas"), ("function", "m", "uno")]);
    }
}
