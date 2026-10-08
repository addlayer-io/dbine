//! What Snowflake's metadata already knows about the session database's
//! objects ([`dbine_driver::Session::row_estimates`],
//! [`dbine_driver::Session::object_comments`]).
//!
//! Everything comes from `SHOW … IN DATABASE`, which the cloud services
//! layer answers from metadata: it needs no warehouse, so it neither
//! resumes one nor bills compute (an `INFORMATION_SCHEMA` query would).
//!
//! - Rows: the `rows` column of `SHOW TABLES` (tables, dynamic and Iceberg
//!   tables) and `SHOW MATERIALIZED VIEWS`, kept by the micro-partition
//!   metadata. External tables have no count.
//! - Comments: views, materialized views, functions, procedures, sequences,
//!   streams and tasks. `SHOW USER FUNCTIONS`/`PROCEDURES` put a fixed text
//!   in `description` when there's no comment: that text is left out.
//!
//! A `SHOW` the role can't run is skipped.

use crate::{ddl, SnowflakeSession};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::{kinds, ObjectRef, Result};

/// What `SHOW USER FUNCTIONS`/`PROCEDURES` say when there's no comment.
const NO_COMMENT: &[&str] = &["user-defined function", "user-defined procedure"];

fn own(r: &ddl::Row) -> bool {
    r.get("schema_name").is_some_and(|s| !s.eq_ignore_ascii_case("INFORMATION_SCHEMA"))
}

fn object(kind: &str, r: &ddl::Row) -> Option<ObjectRef> {
    Some(ObjectRef { kind: kind.into(), schema: r.get("schema_name").cloned(), name: r.get("name")?.clone() })
}

/// `SHOW TABLES` / `SHOW MATERIALIZED VIEWS` rows as estimates.
pub(crate) fn estimates(kind: &str, rows: &[ddl::Row]) -> Vec<RowEstimate> {
    rows.iter()
        .filter(|r| own(r))
        .filter_map(|r| Some(RowEstimate { object: object(kind, r)?, rows: r.get("rows")?.trim().parse().ok()? }))
        .collect()
}

/// `SHOW …` rows as comments, from the column `col`. A view that is
/// materialized (`SHOW VIEWS` lists both) takes that kind. Overloaded
/// functions and procedures give one comment per name: the first one.
pub(crate) fn comments(kind: &str, col: &str, rows: &[ddl::Row]) -> Vec<ObjectComment> {
    let mut out: Vec<ObjectComment> = Vec::new();
    for r in rows.iter().filter(|r| own(r)) {
        let Some(comment) = r.get(col).map(|c| c.trim()).filter(|c| !c.is_empty() && !NO_COMMENT.contains(c)) else { continue };
        let kind = if kind == kinds::VIEW && r.get("is_materialized").is_some_and(|m| m.eq_ignore_ascii_case("true")) {
            kinds::MATERIALIZED_VIEW
        } else {
            kind
        };
        let Some(object) = object(kind, r) else { continue };
        if !out.iter().any(|c| c.object.kind == object.kind && c.object.schema == object.schema && c.object.name == object.name) {
            out.push(ObjectComment { object, comment: comment.to_string() });
        }
    }
    out
}

pub(crate) async fn row_estimates(s: &SnowflakeSession) -> Result<Vec<RowEstimate>> {
    let Ok(db) = s.database() else { return Ok(Vec::new()) };
    let db = qualified_name(Quote::Double, None, &db);
    let mut out = Vec::new();
    for (what, kind) in [("TABLES", kinds::TABLE), ("MATERIALIZED VIEWS", kinds::MATERIALIZED_VIEW)] {
        if let Ok(rows) = s.named_rows(&format!("SHOW {what} IN DATABASE {db}")).await {
            out.extend(estimates(kind, &rows));
        }
    }
    Ok(out)
}

pub(crate) async fn object_comments(s: &SnowflakeSession) -> Result<Vec<ObjectComment>> {
    let Ok(db) = s.database() else { return Ok(Vec::new()) };
    let db = qualified_name(Quote::Double, None, &db);
    let mut out = Vec::new();
    for (what, kind, col) in [
        ("VIEWS", kinds::VIEW, "comment"),
        ("USER FUNCTIONS", kinds::FUNCTION, "description"),
        ("USER PROCEDURES", kinds::PROCEDURE, "description"),
        ("SEQUENCES", kinds::SEQUENCE, "comment"),
        ("STREAMS", kinds::STREAM, "comment"),
        ("TASKS", ddl::TASK, "comment"),
    ] {
        if let Ok(rows) = s.named_rows(&format!("SHOW {what} IN DATABASE {db}")).await {
            out.extend(comments(kind, col, &rows));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pairs: &[(&str, &str)]) -> ddl::Row {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn table_rows() {
        let rows = vec![
            row(&[("schema_name", "PUBLIC"), ("name", "VENTAS"), ("rows", "1200")]),
            row(&[("schema_name", "PUBLIC"), ("name", "EXTERNA")]),
            row(&[("schema_name", "INFORMATION_SCHEMA"), ("name", "TABLES"), ("rows", "5")]),
        ];
        let e = estimates(kinds::TABLE, &rows);
        assert_eq!(e.len(), 1);
        assert_eq!((e[0].object.schema.as_deref(), e[0].object.name.as_str(), e[0].rows), (Some("PUBLIC"), "VENTAS", 1200));
    }

    #[test]
    fn view_and_function_comments() {
        let views = vec![
            row(&[("schema_name", "PUBLIC"), ("name", "V"), ("comment", "ventas por día"), ("is_materialized", "false")]),
            row(&[("schema_name", "PUBLIC"), ("name", "MV"), ("comment", "resumen"), ("is_materialized", "true")]),
            row(&[("schema_name", "PUBLIC"), ("name", "SIN"), ("comment", "")]),
            row(&[("schema_name", "INFORMATION_SCHEMA"), ("name", "TABLES"), ("comment", "The tables")]),
        ];
        let c = comments(kinds::VIEW, "comment", &views);
        let got: Vec<_> = c.iter().map(|c| (c.object.kind.as_str(), c.object.name.as_str(), c.comment.as_str())).collect();
        assert_eq!(got, [("view", "V", "ventas por día"), ("materialized_view", "MV", "resumen")]);

        let funcs = vec![
            row(&[("schema_name", "PUBLIC"), ("name", "F"), ("description", "user-defined function")]),
            row(&[("schema_name", "PUBLIC"), ("name", "G"), ("description", "suma")]),
            row(&[("schema_name", "PUBLIC"), ("name", "G"), ("description", "suma (otra firma)")]),
        ];
        let c = comments(kinds::FUNCTION, "description", &funcs);
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].object.name.as_str(), c[0].comment.as_str()), ("G", "suma"));
    }
}
