//! The lineage of a whole database (`Session::lineage_graph`).
//!
//! The nodes, the jobs (MySQL events, through `jobs` / `job_references`) and
//! the code read from procedures, functions and triggers come from the
//! contract's scan ([`graph_scan`]); this adds what the catalog records, for
//! the whole database in one query each:
//!
//! - views: `VIEW_TABLE_USAGE` and `VIEW_ROUTINE_USAGE` (MySQL 8.0.13+),
//!   confirmed; where they don't exist (MariaDB, older MySQL, TiDB…) the
//!   views' code is read instead (probable);
//! - foreign keys: `KEY_COLUMN_USAGE` (confirmed), a key to another database
//!   is an external source;
//! - triggers: the table they are on (`TRIGGERS`, confirmed).

use crate::references::{catalog_rows, note_catalog_denied, used_kind};
use crate::session::{at, lit, MySqlSession};
use dbine_driver::dependencies::{
    graph_scan, Confidence, DependencyScan, DependencyTarget, ExternalRef, LineageEdge,
    LineageExternal, LineageGraph, LineageScope, Relation, GRAPH_DEFINITIONS,
};
use dbine_driver::{kinds, ObjectRef, Result};

/// The views' uses, the foreign keys and the triggers' tables of `db`, as
/// `(user, used database, used name, type)`.
pub(crate) fn view_uses_all_sql(db: &str) -> String {
    let db = lit(db);
    format!(
        "SELECT u.VIEW_NAME, u.TABLE_SCHEMA, u.TABLE_NAME, t.TABLE_TYPE
  FROM information_schema.VIEW_TABLE_USAGE u
  LEFT JOIN information_schema.TABLES t
    ON t.TABLE_SCHEMA = u.TABLE_SCHEMA AND t.TABLE_NAME = u.TABLE_NAME
 WHERE u.VIEW_SCHEMA = {db}
UNION ALL
SELECT r.TABLE_NAME, r.SPECIFIC_SCHEMA, r.SPECIFIC_NAME, 'FUNCTION'
  FROM information_schema.VIEW_ROUTINE_USAGE r
 WHERE r.TABLE_SCHEMA = {db}"
    )
}

/// `(table, constraint, referenced database, referenced table)`.
pub(crate) fn foreign_keys_all_sql(db: &str) -> String {
    format!(
        "SELECT DISTINCT TABLE_NAME, CONSTRAINT_NAME, REFERENCED_TABLE_SCHEMA, REFERENCED_TABLE_NAME
  FROM information_schema.KEY_COLUMN_USAGE
 WHERE TABLE_SCHEMA = {} AND REFERENCED_TABLE_NAME IS NOT NULL",
        lit(db)
    )
}

/// `(trigger, table's database, table, timing, event)`.
pub(crate) fn triggers_all_sql(db: &str) -> String {
    format!(
        "SELECT TRIGGER_NAME, EVENT_OBJECT_SCHEMA, EVENT_OBJECT_TABLE, ACTION_TIMING, EVENT_MANIPULATION
  FROM information_schema.TRIGGERS
 WHERE TRIGGER_SCHEMA = {}",
        lit(db)
    )
}

pub(crate) trait Row {
    fn cell(&self, i: usize) -> Option<String>;
}

impl Row for mysql_async::Row {
    fn cell(&self, i: usize) -> Option<String> {
        at(self, i)
    }
}

fn same(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// The node `kind`/`name` of the graph (a name can be a table or a view).
fn node(g: &LineageGraph, kinds_: &[&str], name: &str) -> Option<ObjectRef> {
    g.objects
        .iter()
        .find(|o| kinds_.contains(&o.kind.as_str()) && same(&o.name, name))
        .cloned()
}

/// An edge the catalog confirms; one the code scan already found is
/// upgraded instead of repeated.
fn confirm(g: &mut LineageGraph, from: ObjectRef, to: ObjectRef, rel: Relation, d: Option<String>) {
    if let Some(e) = g.edges.iter_mut().find(|e| {
        e.relation == rel
            && e.from.name == from.name
            && e.from.kind == from.kind
            && e.to.name == to.name
            && e.to.kind == to.kind
    }) {
        e.confidence = Confidence::Confirmed;
        if d.is_some() {
            e.detail = d;
        }
        return;
    }
    g.edges.push(LineageEdge {
        from,
        to,
        relation: rel,
        confidence: Confidence::Confirmed,
        detail: d,
        mentions: Vec::new(),
    });
}

fn external(g: &mut LineageGraph, source: ExternalRef, to: &ObjectRef) {
    if !g.external.iter().any(|x| {
        x.to.name == to.name
            && x.to.kind == to.kind
            && x.source.database == source.database
            && x.source.name == source.name
    }) {
        g.external.push(LineageExternal {
            source,
            to: to.clone(),
        });
    }
}

fn other_db(db: Option<String>, name: String, detail: Option<String>) -> ExternalRef {
    ExternalRef {
        database: db,
        name: Some(name),
        detail,
        ..Default::default()
    }
}

const USABLE: &[&str] = &[kinds::TABLE, kinds::VIEW];

/// `view_uses_all_sql`'s rows.
pub(crate) fn add_views<R: Row>(g: &mut LineageGraph, rows: &[R], db: &str) {
    for r in rows {
        let (Some(view), Some(schema), Some(name)) = (r.cell(0), r.cell(1), r.cell(2)) else {
            continue;
        };
        let Some(to) = node(g, &[kinds::VIEW, kinds::MATERIALIZED_VIEW], &view) else {
            continue;
        };
        if !same(&schema, db) {
            external(g, other_db(Some(schema), name, None), &to);
            continue;
        }
        let kind = used_kind(r.cell(3).as_deref());
        let from = if kind == kinds::FUNCTION || kind == kinds::PROCEDURE {
            node(g, &[kind], &name)
        } else {
            node(g, USABLE, &name)
        };
        if let Some(from) = from {
            confirm(g, from, to, Relation::Code, None);
        }
    }
}

/// `foreign_keys_all_sql`'s rows.
pub(crate) fn add_foreign_keys<R: Row>(g: &mut LineageGraph, rows: &[R], db: &str) {
    for r in rows {
        let (Some(table), Some(constraint), Some(name)) = (r.cell(0), r.cell(1), r.cell(3)) else {
            continue;
        };
        let Some(to) = node(g, &[kinds::TABLE], &table) else {
            continue;
        };
        let schema = r.cell(2).unwrap_or_else(|| db.to_string());
        if !same(&schema, db) {
            external(g, other_db(Some(schema), name, Some(constraint)), &to);
        } else if let Some(from) = node(g, &[kinds::TABLE], &name) {
            if !same(&from.name, &to.name) {
                confirm(g, from, to, Relation::ForeignKey, Some(constraint));
            }
        }
    }
}

/// `triggers_all_sql`'s rows.
pub(crate) fn add_triggers<R: Row>(g: &mut LineageGraph, rows: &[R], db: &str) {
    for r in rows {
        let (Some(trigger), Some(schema), Some(table)) = (r.cell(0), r.cell(1), r.cell(2)) else {
            continue;
        };
        let Some(to) = node(g, &[kinds::TRIGGER], &trigger) else {
            continue;
        };
        let when = [r.cell(3), r.cell(4)]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        let detail = (!when.is_empty()).then_some(when);
        if !same(&schema, db) {
            external(g, other_db(Some(schema), table, detail), &to);
        } else if let Some(from) = node(g, &[kinds::TABLE, kinds::VIEW], &table) {
            confirm(g, from, to, Relation::Code, detail);
        }
    }
}

/// The views' uses read from their code, one `references` per view (the
/// engine has no `VIEW_TABLE_USAGE`), up to the contract's limit.
async fn views_from_code(
    s: &mut MySqlSession,
    db: &str,
    scan: &DependencyScan,
    g: &mut LineageGraph,
) {
    let views: Vec<ObjectRef> = g
        .objects
        .iter()
        .filter(|o| o.kind == kinds::VIEW || o.kind == kinds::MATERIALIZED_VIEW)
        .take(GRAPH_DEFINITIONS)
        .cloned()
        .collect();
    for v in views {
        let target = DependencyTarget {
            object: ObjectRef {
                schema: Some(db.to_string()),
                ..v.clone()
            },
            column: None,
        };
        match crate::references::references(s, &target, scan).await {
            Ok(r) => {
                g.scanned += 1;
                for d in r.items {
                    let Some(from) = node(g, &[d.kind.as_str()], &d.name) else {
                        continue;
                    };
                    if g.edges.iter().any(|e| {
                        e.from.name == from.name
                            && e.from.kind == from.kind
                            && e.to.name == v.name
                            && e.to.kind == v.kind
                    }) {
                        continue;
                    }
                    g.edges.push(LineageEdge {
                        from,
                        to: v.clone(),
                        relation: Relation::Code,
                        confidence: d.confidence,
                        detail: d.detail,
                        mentions: d.mentions,
                    });
                }
                for x in r.external {
                    external(g, x, &v);
                }
            }
            Err(_) => g.unreadable.push(v.name.clone()),
        }
    }
}

pub(crate) async fn lineage_graph(
    s: &mut MySqlSession,
    database: &str,
    scope: &LineageScope,
    scan: &DependencyScan,
) -> Result<LineageGraph> {
    let db = if database.is_empty() {
        s.current_database().await
    } else {
        Some(database.to_string())
    };
    let Some(db) = db else {
        return graph_scan(s, database, scope, scan).await;
    };
    // Views come from the catalog where it records them; ask first so the
    // contract's scan reads their code only when it doesn't.
    let views = catalog_rows(s, &view_uses_all_sql(&db)).await;
    let mut ctx = scan.clone();
    // Views are never read by the contract's scan: from the catalog, or one
    // by one through `references`, which knows how this engine stores them.
    ctx.written_kinds
        .retain(|k| k != kinds::VIEW && k != kinds::MATERIALIZED_VIEW);
    if s.variant.has_foreign_keys() {
        ctx.foreign_keys = false;
    }
    let mut g = graph_scan(s, &db, scope, &ctx).await?;
    match &views {
        Ok(rows) => add_views(&mut g, rows, &db),
        Err(denied) => {
            if *denied {
                g.notes.push(note_catalog_denied("VIEW_TABLE_USAGE"));
            }
            views_from_code(s, &db, scan, &mut g).await;
        }
    }
    if s.variant.has_foreign_keys() {
        match catalog_rows(s, &foreign_keys_all_sql(&db)).await {
            Ok(rows) => add_foreign_keys(&mut g, &rows, &db),
            Err(true) => g.notes.push(note_catalog_denied("KEY_COLUMN_USAGE")),
            Err(false) => {}
        }
    }
    if s.variant.has_triggers() {
        match catalog_rows(s, &triggers_all_sql(&db)).await {
            Ok(rows) => add_triggers(&mut g, &rows, &db),
            Err(true) => g.notes.push(note_catalog_denied("TRIGGERS")),
            Err(false) => {}
        }
    }
    Ok(g)
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Row for Vec<Option<&str>> {
        fn cell(&self, i: usize) -> Option<String> {
            self.get(i).copied().flatten().map(str::to_string)
        }
    }

    fn o(kind: &str, name: &str) -> ObjectRef {
        ObjectRef {
            kind: kind.into(),
            schema: None,
            name: name.into(),
        }
    }

    fn graph() -> LineageGraph {
        LineageGraph {
            objects: vec![
                o("table", "clientes"),
                o("table", "pedidos"),
                o("view", "v_base"),
                o("view", "v_ventas"),
                o("function", "f_total"),
                o("trigger", "tg_pedidos"),
            ],
            ..Default::default()
        }
    }

    fn pairs(g: &LineageGraph) -> Vec<String> {
        let mut v: Vec<String> = g
            .edges
            .iter()
            .map(|e| format!("{}>{}:{:?}", e.from.name, e.to.name, e.confidence))
            .collect();
        v.sort();
        v
    }

    #[test]
    fn views_foreign_keys_and_triggers_become_confirmed_edges() {
        let mut g = graph();
        // The code scan had found the trigger's table, as probable.
        g.edges.push(LineageEdge {
            from: o("table", "pedidos"),
            to: o("trigger", "tg_pedidos"),
            relation: Relation::Code,
            confidence: Confidence::Probable,
            detail: None,
            mentions: Vec::new(),
        });
        add_views(
            &mut g,
            &[
                vec![
                    Some("v_ventas"),
                    Some("ventas"),
                    Some("v_base"),
                    Some("VIEW"),
                ],
                vec![
                    Some("v_ventas"),
                    Some("ventas"),
                    Some("f_total"),
                    Some("FUNCTION"),
                ],
                vec![
                    Some("v_base"),
                    Some("Ventas"),
                    Some("clientes"),
                    Some("BASE TABLE"),
                ],
                vec![
                    Some("v_base"),
                    Some("stock"),
                    Some("items"),
                    Some("BASE TABLE"),
                ],
                vec![
                    Some("gone"),
                    Some("ventas"),
                    Some("clientes"),
                    Some("BASE TABLE"),
                ],
            ],
            "ventas",
        );
        add_foreign_keys(
            &mut g,
            &[
                vec![
                    Some("pedidos"),
                    Some("fk_cli"),
                    Some("ventas"),
                    Some("clientes"),
                ],
                vec![Some("pedidos"), Some("fk_x"), Some("otra"), Some("t")],
                vec![
                    Some("clientes"),
                    Some("fk_self"),
                    Some("ventas"),
                    Some("clientes"),
                ],
            ],
            "ventas",
        );
        add_triggers(
            &mut g,
            &[vec![
                Some("tg_pedidos"),
                Some("ventas"),
                Some("pedidos"),
                Some("BEFORE"),
                Some("INSERT"),
            ]],
            "ventas",
        );
        assert_eq!(
            pairs(&g),
            [
                "clientes>pedidos:Confirmed",
                "clientes>v_base:Confirmed",
                "f_total>v_ventas:Confirmed",
                "pedidos>tg_pedidos:Confirmed",
                "v_base>v_ventas:Confirmed",
            ]
        );
        // The scan's edge was upgraded, not repeated, and keeps the timing.
        let t = g.edges.iter().filter(|e| e.to.name == "tg_pedidos").count();
        assert_eq!(t, 1);
        assert_eq!(
            g.edges
                .iter()
                .find(|e| e.to.name == "tg_pedidos")
                .unwrap()
                .detail
                .as_deref(),
            Some("BEFORE INSERT")
        );
        let ext: Vec<_> = g
            .external
            .iter()
            .map(|x| {
                format!(
                    "{}.{}>{}",
                    x.source.database.as_deref().unwrap(),
                    x.source.name.as_deref().unwrap(),
                    x.to.name
                )
            })
            .collect();
        assert_eq!(ext, ["stock.items>v_base", "otra.t>pedidos"]);
    }

    #[test]
    fn the_catalog_queries_are_scoped_to_the_database() {
        for sql in [
            view_uses_all_sql("ve'ntas"),
            foreign_keys_all_sql("ve'ntas"),
            triggers_all_sql("ve'ntas"),
        ] {
            assert!(
                sql.contains("'ve''ntas'") || sql.contains("'ve\\'ntas'"),
                "{sql}"
            );
        }
    }
}
