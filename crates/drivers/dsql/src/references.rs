//! What an object uses (`Session::references`): PostgreSQL's `pg_depend`
//! through the query the PostgreSQL driver uses (`pg_references`), with
//! what DSQL has: views from their rewrite rule, functions from their own
//! dependencies (SQL-standard bodies), tables from their defaults, checks
//! and indexes (DSQL has no foreign keys, triggers, policies, inheritance
//! or foreign tables). Confirmed.
//!
//! DSQL's catalog is a subset of PostgreSQL's, and nothing says it records
//! every dependency: the generic scan of the definition's text is always
//! added (probable where only the text finds a name). If the catalog can't
//! be read, the generic scan alone, with a note.

use crate::pg_references::{self as pr, Catalog};
use crate::transfer::literal;
use crate::{DsqlSession, DSQL_PG_VERSION, SYSTEM_SCHEMAS};
use dbine_driver::dependencies::{
    references_scan, DependencyReport, DependencyScan, DependencyTarget,
};
use dbine_driver::{Error, Result, Session};
use tokio_postgres::SimpleQueryMessage;

/// DSQL's catalog: PostgreSQL 16's core, nothing optional.
const CATALOG: Catalog = Catalog {
    version: DSQL_PG_VERSION,
    full: false,
};

/// `col` is a user schema.
fn user_schema(col: &str) -> String {
    format!("{col} NOT IN {SYSTEM_SCHEMAS} AND {col} NOT LIKE 'pg\\_%'")
}

/// The catalog query for `target`, `None` for a kind it doesn't read.
fn sql(target: &DependencyTarget) -> Option<String> {
    let t = &target.object;
    pr::uses_sql(
        CATALOG,
        &t.kind,
        &literal(t.schema().unwrap_or("")),
        &literal(&t.name),
        user_schema,
    )
}

pub(crate) async fn references(
    s: &mut DsqlSession,
    target: &DependencyTarget,
    scan: &DependencyScan,
) -> Result<DependencyReport> {
    let Some(sql) = sql(target) else {
        return references_scan(s, target, scan).await;
    };
    let catalog = match s.client.simple_query(&sql).await.map_err(crate::err) {
        Ok(msgs) => DependencyReport {
            items: msgs
                .iter()
                .filter_map(|m| match m {
                    SimpleQueryMessage::Row(r) => pr::used(
                        ["kind", "sch", "name", "rel", "detail"]
                            .map(|c| r.try_get(c).ok().flatten().map(str::to_string)),
                    ),
                    _ => None,
                })
                .collect(),
            ..Default::default()
        },
        Err(e @ (Error::Query(_) | Error::Statement(_) | Error::Unsupported(_))) => {
            tracing::debug!("dsql: dependency catalog unavailable: {e}");
            let mut r = references_scan(s, target, scan).await?;
            r.note = Some(pr::unread_note(&e));
            return Ok(r);
        }
        Err(e) => return Err(e),
    };
    let mut report = pr::merge(catalog, references_scan(s, target, scan).await?);
    let t = &target.object;
    if report.items.iter().any(|d| d.mentions.is_empty()) && scan.written_kinds.contains(&t.kind) {
        if let Ok(Some(body)) = Session::definition(s, t).await {
            report.scanned = report.scanned.max(1);
            pr::add_mentions(&mut report.items, &body, &scan.dialect);
        }
    }
    pr::sort(&mut report.items);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ObjectRef;

    fn target(kind: &str, name: &str) -> DependencyTarget {
        DependencyTarget {
            object: ObjectRef {
                kind: kind.into(),
                schema: Some("ventas".into()),
                name: name.into(),
            },
            column: None,
        }
    }

    #[test]
    fn the_query_keeps_to_dsqls_catalog() {
        let v = sql(&target("view", "v")).unwrap();
        assert!(v.contains("c.relname = E'v'") && v.contains("n.nspname = E'ventas'"));
        assert!(v.contains("FROM pg_rewrite r"));
        assert!(
            v.contains("n.nspname NOT IN ('pg_catalog', 'information_schema', 'sys', 'pg_toast')")
        );
        let t = sql(&target("table", "pedidos")).unwrap();
        for absent in ["pg_policy", "pg_inherits", "pg_foreign", "attgenerated"] {
            assert!(!t.contains(absent), "{absent}");
        }
        assert!(t.contains("FROM pg_attrdef d JOIN tgt"));
        let f = sql(&target("function", "f")).unwrap();
        assert!(f.contains("p.prokind <> 'p'"));
        assert!(sql(&target("sequence", "s")).is_none());
        assert!(sql(&target("type", "t")).is_none());
    }
}
