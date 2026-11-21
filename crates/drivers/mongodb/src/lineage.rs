//! The lineage of a whole database (`Session::lineage_graph`).
//!
//! Collections have no dependencies (documents point at each other only by
//! convention), so the edges are the views': one `listCollections` for every
//! view of the database gives its `viewOn` and its pipeline, read as
//! `references` reads one (`$lookup.from`, `$graphLookup.from`,
//! `$unionWith`, also inside sub-pipelines), all Confirmed because the
//! server keeps that definition. A collection of another database is an
//! external source. The nodes and the limit come from the contract's scan,
//! which here reads no definitions.

use crate::references::{report, view_uses};
use crate::MongoSession;
use dbine_driver::dependencies::{
    graph_scan, DependencyReport, DependencyScan, LineageEdge, LineageExternal, LineageGraph,
    LineageScope, Relation,
};
use dbine_driver::{kinds, DbObject, Result};
use mongodb::bson::{doc, Document};

/// Every view's stored definition.
pub(crate) fn views_command() -> Document {
    doc! { "listCollections": 1, "filter": { "type": "view" } }
}

/// The edges and external sources of `views` (`listCollections` rows) in the
/// graph. A used name that is no node (out of the scope) is left out.
pub(crate) fn add_views(g: &mut LineageGraph, db: &str, views: &[Document]) {
    let objects: Vec<DbObject> = g
        .objects
        .iter()
        .map(|o| DbObject {
            kind: o.kind.clone(),
            schema: None,
            name: o.name.clone(),
            parent: None,
        })
        .collect();
    for v in views {
        let Ok(name) = v.get_str("name") else {
            continue;
        };
        let Some(to) = g
            .objects
            .iter()
            .find(|o| o.kind == kinds::VIEW && o.name == name)
            .cloned()
        else {
            continue;
        };
        let Ok(options) = v.get_document("options") else {
            continue;
        };
        g.scanned += 1;
        let r: DependencyReport = report(db, &view_uses(options), &objects);
        for d in r.items {
            let Some(from) = g
                .objects
                .iter()
                .find(|o| o.kind == d.kind && o.name == d.name)
                .cloned()
            else {
                continue;
            };
            g.edges.push(LineageEdge {
                from,
                to: to.clone(),
                relation: Relation::Code,
                confidence: d.confidence,
                detail: d.detail,
                mentions: Vec::new(),
            });
        }
        for source in r.external {
            g.external.push(LineageExternal {
                source,
                to: to.clone(),
            });
        }
    }
}

impl MongoSession {
    pub(crate) async fn lineage_of(
        &mut self,
        database: &str,
        scope: &LineageScope,
        scan: &DependencyScan,
    ) -> Result<LineageGraph> {
        let db = self.db.name().to_string();
        let mut ctx = scan.clone();
        ctx.written_kinds.clear();
        ctx.foreign_keys = false;
        let mut g = graph_scan(self, database, scope, &ctx).await?;
        match self.first_batch(views_command(), 1_000_000).await {
            Ok(views) => add_views(&mut g, &db, &views),
            Err(e) => {
                tracing::debug!("mongodb listCollections of views: {e}");
                g.notes.push(crate::references::NO_LIST.into());
            }
        }
        Ok(g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(kind: &str, name: &str) -> dbine_driver::ObjectRef {
        dbine_driver::ObjectRef {
            kind: kind.into(),
            schema: None,
            name: name.into(),
        }
    }

    #[test]
    fn views_become_confirmed_edges() {
        let mut g = LineageGraph {
            objects: vec![
                o("collection", "pedidos"),
                o("collection", "clientes"),
                o("view", "v_pedidos"),
                o("view", "v_resumen"),
                o("collection", "suelta"),
            ],
            ..Default::default()
        };
        let views = vec![
            doc! { "name": "v_pedidos", "type": "view", "options": {
            "viewOn": "pedidos",
            "pipeline": [
                { "$lookup": { "from": "clientes", "localField": "c", "foreignField": "_id", "as": "c" } },
                { "$unionWith": { "db": "archivo", "coll": "pedidos_viejos" } },
            ] } },
            doc! { "name": "v_resumen", "type": "view", "options": { "viewOn": "v_pedidos", "pipeline": [] } },
            doc! { "name": "no_node", "type": "view", "options": { "viewOn": "pedidos" } },
        ];
        add_views(&mut g, "ventas", &views);
        let mut got: Vec<String> = g
            .edges
            .iter()
            .map(|e| format!("{}>{}:{:?}", e.from.name, e.to.name, e.confidence))
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                "clientes>v_pedidos:Confirmed",
                "pedidos>v_pedidos:Confirmed",
                "v_pedidos>v_resumen:Confirmed"
            ]
        );
        assert_eq!(g.external.len(), 1);
        assert_eq!(g.external[0].source.database.as_deref(), Some("archivo"));
        assert_eq!(g.external[0].to.name, "v_pedidos");
        assert_eq!(g.scanned, 2);
    }

    #[test]
    fn the_command_reads_the_views() {
        assert_eq!(
            views_command(),
            doc! { "listCollections": 1, "filter": { "type": "view" } }
        );
    }
}
