//! What depends on an object (`Session::dependents`).
//!
//! - Foreign keys, indexes and checks: the catalog, through
//!   `database_schema` and the contract's `schema_dependents`.
//! - Views, routines and triggers: one query over `sys.sql_modules` that
//!   brings only the bodies naming the target (`LIKE`) or recorded as using
//!   it in `sys.sql_expression_dependencies`, so a database with thousands
//!   of procedures isn't read one by one.
//!
//! The catalog records which objects use a table, but a column only for
//! schema-bound ones (`referenced_minor_id` is 0 otherwise). So: a table
//! the catalog records is confirmed; a column is confirmed when the catalog
//! names it, probable when the catalog records the table and the body
//! names the column; anything only the text finds keeps the text's
//! confidence (dynamic SQL and deferred names aren't in the catalog).
//! Encrypted modules have no body: listed as unreadable when the catalog
//! says they use the target.

use crate::{text, SqlServerSession};
use dbine_driver::dependencies::{
    find_mentions, schema_dependents, Confidence, DependencyReport, DependencyScan,
    DependencyTarget, Dependent, Relation,
};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{kinds, Result, Session, TableSchema};
use tiberius::Row;

/// `@P1` the target (`[schema].[name]`), `@P2` its column or '', `@P3` the
/// LIKE pattern for the name searched.
const MODULES_SQL: &str = "WITH dep AS (
  SELECT d.referencing_id,
         MAX(CASE WHEN @P2 <> '' AND d.referenced_minor_id = COLUMNPROPERTY(OBJECT_ID(@P1), @P2, 'ColumnId') THEN 1 ELSE 0 END) AS col_hit
    FROM sys.sql_expression_dependencies d
   WHERE d.referenced_id = OBJECT_ID(@P1)
   GROUP BY d.referencing_id)
SELECT RTRIM(o.type), s.name, o.name, OBJECT_NAME(o.parent_object_id), m.definition,
       CAST(CASE WHEN dep.referencing_id IS NULL THEN 0 ELSE 1 END AS int), CAST(ISNULL(dep.col_hit, 0) AS int)
  FROM sys.sql_modules m
  JOIN sys.objects o ON o.object_id = m.object_id
  JOIN sys.schemas s ON s.schema_id = o.schema_id
  LEFT JOIN dep ON dep.referencing_id = o.object_id
 WHERE o.is_ms_shipped = 0
   AND o.object_id <> ISNULL(OBJECT_ID(@P1), 0)
   AND (dep.referencing_id IS NOT NULL OR m.definition LIKE @P3 ESCAPE '\\')
 ORDER BY s.name, o.name";

pub(crate) async fn dependents(
    s: &mut SqlServerSession,
    target: &DependencyTarget,
    scan: &DependencyScan,
) -> Result<DependencyReport> {
    let mut report = DependencyReport::default();
    let tables = s.database_schema().await?;
    report.items.extend(fulltext_index_users(&tables, target));
    report.items.extend(schema_dependents(&tables, target));
    let object = qualified_name(Quote::Bracket, target.object.schema(), &target.object.name);
    let column = target.column.clone().unwrap_or_default();
    let pattern = format!(
        "%{}%",
        like_escape(target.column.as_deref().unwrap_or(&target.object.name))
    );
    for r in s.rows(MODULES_SQL, &[&object, &column, &pattern]).await? {
        if let Some(d) = module(&r, target, scan, &mut report) {
            report.items.push(d);
        }
    }
    report.items.sort_by_key(|d| {
        (
            d.confidence,
            d.relation,
            d.schema.clone(),
            d.name.to_lowercase(),
        )
    });
    Ok(report)
}

/// The tables whose full-text index uses this catalog or stoplist (the
/// server won't drop one while an index uses it).
fn fulltext_index_users(tables: &[TableSchema], target: &DependencyTarget) -> Vec<Dependent> {
    let option = match target.object.kind.as_str() {
        crate::structure::FULLTEXT_CATALOG => "CATALOG",
        crate::structure::FULLTEXT_STOPLIST => "STOPLIST",
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    for t in tables {
        for ix in t.indexes.iter().filter(|ix| {
            ix.options
                .get(option)
                .is_some_and(|v| v.eq_ignore_ascii_case(&target.object.name))
        }) {
            out.push(Dependent {
                kind: t.kind.clone(),
                schema: t.schema.clone(),
                name: t.name.clone(),
                parent: None,
                relation: Relation::Index,
                confidence: Confidence::Confirmed,
                detail: Some(format!("{} ({})", ix.name, ix.columns.join(", "))),
                mentions: Vec::new(),
            });
        }
    }
    out
}

fn module(
    r: &Row,
    target: &DependencyTarget,
    scan: &DependencyScan,
    report: &mut DependencyReport,
) -> Option<Dependent> {
    let kind = match text(r, 0)?.as_str() {
        "V" => kinds::VIEW,
        "P" | "PC" | "RF" => kinds::PROCEDURE,
        "TR" => kinds::TRIGGER,
        _ => kinds::FUNCTION,
    };
    let (schema, name) = (text(r, 1), text(r, 2)?);
    let recorded = r.get::<i32, _>(5).unwrap_or(0) == 1;
    let column_recorded = r.get::<i32, _>(6).unwrap_or(0) == 1;
    let Some(body) = text(r, 4) else {
        if recorded {
            report.unreadable.push(
                schema
                    .as_deref()
                    .map_or(name.clone(), |s| format!("{s}.{name}")),
            );
        }
        return None;
    };
    report.scanned += 1;
    let found = find_mentions(&body, &scan.dialect, target);
    let confidence = match (&found, target.column.is_some()) {
        _ if column_recorded => Confidence::Confirmed,
        (_, false) if recorded => Confidence::Confirmed,
        (Some((c, _)), _) => *c,
        (None, _) => return None,
    };
    Some(Dependent {
        kind: kind.into(),
        schema,
        name,
        parent: if kind == kinds::TRIGGER {
            text(r, 3)
        } else {
            None
        },
        relation: Relation::Code,
        confidence,
        detail: None,
        mentions: found.map(|(_, m)| m).unwrap_or_default(),
    })
}

/// `%`, `_`, `[` and the escape itself, taken literally.
fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '[' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod fulltext_tests {
    use super::*;
    use dbine_driver::{IndexDef, ObjectRef};

    fn target(kind: &str, name: &str) -> DependencyTarget {
        DependencyTarget {
            object: ObjectRef {
                kind: kind.into(),
                schema: None,
                name: name.into(),
            },
            column: None,
        }
    }

    #[test]
    fn a_catalog_and_a_stoplist_list_the_indexes_that_use_them() {
        let mut ix = IndexDef {
            name: "fulltext".into(),
            columns: vec!["body".into()],
            kind: Some("FULLTEXT".into()),
            ..Default::default()
        };
        ix.options.insert("CATALOG".into(), "cat_a".into());
        ix.options.insert("STOPLIST".into(), "sl_a".into());
        let tables = [TableSchema {
            schema: Some("dbo".into()),
            name: "docs".into(),
            kind: "table".into(),
            indexes: vec![ix],
            ..Default::default()
        }];
        let users = |kind, name| fulltext_index_users(&tables, &target(kind, name));
        let found = users(crate::structure::FULLTEXT_CATALOG, "CAT_A");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "docs");
        assert_eq!(found[0].detail.as_deref(), Some("fulltext (body)"));
        assert_eq!(users(crate::structure::FULLTEXT_STOPLIST, "sl_a").len(), 1);
        assert!(users(crate::structure::FULLTEXT_CATALOG, "otro").is_empty());
        // The option's value isn't looked at for other kinds.
        assert!(users("table", "cat_a").is_empty());
    }
}
