//! The lineage of a whole database (`Session::lineage_graph`), from one read
//! of `system.tables` for the database.
//!
//! Each row is read as `references` reads one object (the same functions:
//! the view's query, a materialized view's `TO` and `DEPENDS ON`, the engine
//! of Distributed, Buffer, Dictionary… tables, a dictionary's `SOURCE`,
//! `loading_dependencies_*`), but the names are resolved against the
//! objects already listed instead of one query per object, and the
//! `dependencies_*` columns (the views that read each table, Confirmed) give
//! a materialized view's sources. Sources outside the database are
//! `external`. Merge tables' regular expressions are not resolved here
//! (their tables only show in the object's own dependencies).

use crate::references::{
    code_refs, dictionary_source, engine_refs, mentions_at, view_clauses, Found, Wanted,
};
use crate::{text, ClickHouseSession, DICTIONARY};
use dbine_driver::dependencies::{
    graph_scan, Confidence, DependencyScan, ExternalRef, LineageEdge, LineageExternal,
    LineageGraph, LineageScope, Relation,
};
use dbine_driver::sql::ScriptDialect;
use dbine_driver::{kinds, ObjectRef, Result};
use serde_json::Value;

/// One row per object: name, engine, engine_full, create_table_query,
/// dependencies_database, dependencies_table and, on servers that have
/// them, `loading_dependencies_*`.
const ROWS_SQL: &str = "SELECT name, engine, engine_full, create_table_query,
       dependencies_database, dependencies_table,
       loading_dependencies_database, loading_dependencies_table
  FROM system.tables WHERE database = {db:String}";
const ROWS_SQL_OLD: &str = "SELECT name, engine, engine_full, create_table_query,
       dependencies_database, dependencies_table
  FROM system.tables WHERE database = {db:String}";
/// Clusters this server is part of.
const LOCAL_CLUSTER_SQL: &str =
    "SELECT count() FROM system.clusters WHERE cluster = {c:String} AND is_local";

fn strings(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().map(text).collect(),
        Some(Value::String(s)) => serde_json::from_str::<Vec<String>>(s).unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn pairs(dbs: Option<&Value>, names: Option<&Value>) -> Vec<(String, String)> {
    strings(dbs).into_iter().zip(strings(names)).collect()
}

fn node(g: &LineageGraph, name: &str) -> Option<ObjectRef> {
    g.objects.iter().find(|o| o.name == name).cloned()
}

/// An edge: a catalog one wins over what the code only suggests.
fn put(
    g: &mut LineageGraph,
    from: ObjectRef,
    to: ObjectRef,
    confidence: Confidence,
    detail: Option<String>,
    mentions: Vec<dbine_driver::dependencies::Mention>,
) {
    if from.name == to.name {
        return;
    }
    if let Some(e) = g
        .edges
        .iter_mut()
        .find(|e| e.from.name == from.name && e.to.name == to.name)
    {
        e.confidence = e.confidence.min(confidence);
        if e.detail.is_none() {
            e.detail = detail;
        }
        if e.mentions.is_empty() {
            e.mentions = mentions;
        }
        return;
    }
    g.edges.push(LineageEdge {
        from,
        to,
        relation: Relation::Code,
        confidence,
        detail,
        mentions,
    });
}

fn outside(g: &mut LineageGraph, source: ExternalRef, to: &ObjectRef) {
    let same = |x: &LineageExternal| {
        x.to.name == to.name
            && x.source.system == source.system
            && x.source.server == source.server
            && x.source.database == source.database
            && x.source.schema == source.schema
            && x.source.name == source.name
            && x.source.detail == source.detail
    };
    if !g.external.iter().any(same) {
        g.external.push(LineageExternal {
            source,
            to: to.clone(),
        });
    }
}

/// What `system.tables`' rows say, added to the graph. `local` tells whether
/// this server is part of a cluster.
pub(crate) fn add_rows(
    g: &mut LineageGraph,
    db: &str,
    rows: &[Vec<Value>],
    dialect: &ScriptDialect,
    written: &[String],
    local: &dyn Fn(&str) -> bool,
) {
    for r in rows {
        let name = r.first().map(text).unwrap_or_default();
        let Some(me) = node(g, &name) else { continue };
        let engine = r.get(1).map(text).unwrap_or_default();
        let engine_full = r.get(2).map(text).unwrap_or_default();
        let create = r.get(3).map(text).unwrap_or_default();

        // The views that read this table: confirmed.
        for (d, n) in pairs(r.get(4), r.get(5)) {
            if d == db {
                if let Some(user) = node(g, &n) {
                    put(g, me.clone(), user, Confidence::Confirmed, None, Vec::new());
                }
            }
        }

        let mut found = Found::default();
        let code = written.contains(&me.kind)
            && matches!(me.kind.as_str(), kinds::VIEW | kinds::MATERIALIZED_VIEW);
        let body = if code { create.as_str() } else { "" };
        if code && !create.is_empty() {
            g.scanned += 1;
            code_refs(&create, dialect, db, &mut found);
        }
        if engine == "MaterializedView" {
            for w in view_clauses(&create, dialect, db) {
                push_wanted(&mut found, w);
            }
        }
        engine_refs(&engine_full, db, &mut found);
        if me.kind == DICTIONARY {
            dictionary_source(&create, dialect, db, &mut found);
        }
        for (d, n) in pairs(r.get(6), r.get(7)) {
            push_wanted(
                &mut found,
                Wanted {
                    database: d,
                    name: n,
                    confidence: Confidence::Confirmed,
                    detail: None,
                    at: Vec::new(),
                    call: false,
                },
            );
        }
        // Distributed tables: the table on the cluster, here if the server is
        // part of it.
        for (cluster, w) in std::mem::take(&mut found.clusters) {
            if local(&cluster) {
                push_wanted(&mut found, w);
            } else {
                outside(
                    g,
                    ExternalRef {
                        system: Some("ClickHouse".into()),
                        server: Some(cluster),
                        database: Some(w.database),
                        name: Some(w.name),
                        detail: w.detail,
                        mentions: mentions_at(body, &w.at),
                        ..Default::default()
                    },
                    &me,
                );
            }
        }
        for w in &found.wanted {
            if w.database == db {
                // A name that is no listed object is a function or a
                // table outside the scope.
                if let Some(from) = node(g, &w.name) {
                    put(
                        g,
                        from,
                        me.clone(),
                        w.confidence,
                        w.detail.clone(),
                        mentions_at(body, &w.at),
                    );
                }
            } else if !w.call {
                outside(
                    g,
                    ExternalRef {
                        system: Some("ClickHouse".into()),
                        database: Some(w.database.clone()),
                        name: Some(w.name.clone()),
                        detail: w.detail.clone(),
                        mentions: mentions_at(body, &w.at),
                        ..Default::default()
                    },
                    &me,
                );
            }
        }
        for x in std::mem::take(&mut found.external) {
            outside(g, x, &me);
        }
    }
}

fn push_wanted(found: &mut Found, w: Wanted) {
    match found
        .wanted
        .iter_mut()
        .find(|x| x.database == w.database && x.name == w.name)
    {
        Some(x) => {
            x.confidence = x.confidence.min(w.confidence);
            x.detail = x.detail.take().or(w.detail);
            x.at.extend(w.at);
            x.call |= w.call;
        }
        None => found.wanted.push(w),
    }
}

impl ClickHouseSession {
    pub(crate) async fn lineage_of(
        &mut self,
        database: &str,
        scope: &LineageScope,
        scan: &DependencyScan,
    ) -> Result<LineageGraph> {
        let db = if database.is_empty() {
            self.database.clone()
        } else {
            database.to_string()
        };
        let params = [("db", db.as_str())];
        let rows = match self.rows(ROWS_SQL, &params).await {
            Ok(r) => r,
            Err(_) => match self.rows(ROWS_SQL_OLD, &params).await {
                Ok(r) => r,
                // Without system.tables the contract's scan reads the code.
                Err(_) => return graph_scan(self, &db, scope, scan).await,
            },
        };
        // The nodes, the jobs and the code of what has no catalog row come
        // from the contract's scan; views are read from their row here.
        let mut ctx = scan.clone();
        ctx.written_kinds.clear();
        // Dictionaries are nodes too: their source is a table.
        if !ctx.source_kinds.iter().any(|k| k == DICTIONARY) {
            ctx.source_kinds.push(DICTIONARY.to_string());
        }
        ctx.foreign_keys = false;
        let mut g = graph_scan(self, &db, scope, &ctx).await?;
        let mut clusters: Vec<(String, bool)> = Vec::new();
        for r in &rows {
            let engine = r.get(2).map(text).unwrap_or_default();
            if let Some(c) = distributed_cluster(&engine) {
                if !clusters.iter().any(|(x, _)| *x == c) {
                    let n = self
                        .rows(LOCAL_CLUSTER_SQL, &[("c", &c)])
                        .await
                        .ok()
                        .and_then(|r| r.first().and_then(|r| crate::jobs::num(r.first())))
                        .unwrap_or(0.0);
                    clusters.push((c, n > 0.0));
                }
            }
        }
        let local = |c: &str| clusters.iter().any(|(x, l)| x == c && *l);
        add_rows(
            &mut g,
            &db,
            &rows,
            &scan.dialect,
            &scan.written_kinds,
            &local,
        );
        Ok(g)
    }
}

/// `Distributed('cluster', …)`: the cluster's name.
fn distributed_cluster(engine_full: &str) -> Option<String> {
    let rest = engine_full.strip_prefix("Distributed(")?;
    let rest = rest.trim_start().trim_start_matches('\'');
    Some(rest.split(['\'', ',']).next()?.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn o(kind: &str, name: &str) -> ObjectRef {
        ObjectRef {
            kind: kind.into(),
            schema: None,
            name: name.into(),
        }
    }

    fn row(name: &str, engine: &str, full: &str, create: &str, deps: &[&str]) -> Vec<Value> {
        vec![
            json!(name),
            json!(engine),
            json!(full),
            json!(create),
            json!(deps.iter().map(|_| "ventas").collect::<Vec<_>>()),
            json!(deps),
            json!([]),
            json!([]),
        ]
    }

    #[test]
    fn rows_become_edges_and_external_sources() {
        let mut g = LineageGraph {
            objects: vec![
                o("table", "eventos"),
                o("table", "resumen"),
                o("materialized_view", "mv_resumen"),
                o("view", "v_eventos"),
                o("table", "suelta"),
            ],
            ..Default::default()
        };
        let rows = vec![
            row("eventos", "MergeTree", "MergeTree ORDER BY id", "", &["mv_resumen"]),
            row("resumen", "MergeTree", "MergeTree ORDER BY d", "", &[]),
            row(
                "mv_resumen",
                "MaterializedView",
                "MaterializedView",
                "CREATE MATERIALIZED VIEW ventas.mv_resumen TO ventas.resumen AS SELECT d, count() FROM ventas.eventos GROUP BY d",
                &[],
            ),
            row(
                "v_eventos",
                "View",
                "View",
                "CREATE VIEW ventas.v_eventos AS SELECT * FROM ventas.eventos e JOIN otra.t ON t.id = e.id",
                &[],
            ),
            row("suelta", "MergeTree", "MergeTree ORDER BY id", "", &[]),
        ];
        let written = vec!["view".to_string(), "materialized_view".to_string()];
        add_rows(
            &mut g,
            "ventas",
            &rows,
            &crate::dialect(),
            &written,
            &|_| false,
        );
        let mut got: Vec<String> = g
            .edges
            .iter()
            .map(|e| format!("{}>{}:{:?}", e.from.name, e.to.name, e.confidence))
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                "eventos>mv_resumen:Confirmed",
                "eventos>v_eventos:Probable",
                "resumen>mv_resumen:Confirmed",
            ]
        );
        assert_eq!(g.external.len(), 1);
        assert_eq!(g.external[0].to.name, "v_eventos");
        assert_eq!(g.external[0].source.database.as_deref(), Some("otra"));
        assert_eq!(g.scanned, 2);
    }

    #[test]
    fn the_cluster_of_a_distributed_table() {
        assert_eq!(
            distributed_cluster("Distributed('c1', 'ventas', 'eventos', rand())").as_deref(),
            Some("c1")
        );
        assert_eq!(distributed_cluster("MergeTree"), None);
    }
}
