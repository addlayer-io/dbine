//! What a view uses (`Session::references`).
//!
//! - A view: `listCollections` gives its stored definition, `options.viewOn`
//!   and `options.pipeline`. `viewOn` is the collection or view it reads;
//!   the pipeline's `$lookup.from`, `$graphLookup.from`, `$unionWith`
//!   (`coll`, or the plain name), `$merge.into` and `$out` name the rest,
//!   also inside sub-pipelines (`$lookup.pipeline`, `$unionWith.pipeline`,
//!   `$facet`). All of them are `Confirmed`: the server keeps that
//!   definition. A target in another database (`{db, coll}`) goes in
//!   `external`.
//! - A collection uses nothing: documents reference each other only by
//!   convention (no foreign keys). The generic scan answers it.
//!
//! What the generic scan finds besides is added (never less than it).
//! FerretDB has no views.

use crate::MongoSession;
use dbine_driver::dependencies::{references_scan, ExternalRef};
use dbine_driver::{
    kinds, Confidence, DbObject, DependencyReport, DependencyScan, DependencyTarget, Dependent,
    Relation, Result,
};
use mongodb::bson::{doc, Bson, Document};

/// What the report says when the view's definition can't be read.
pub(crate) const NO_LIST: &str = "No se pudo leer la definición de la vista (listCollections): se muestra solo lo que encuentra la búsqueda en su código.";

/// One name a view's definition reads.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Used {
    /// `viewOn`, `$lookup`…
    pub stage: &'static str,
    /// Another database (`{db, coll}`).
    pub db: Option<String>,
    pub coll: String,
}

/// The `listCollections` that reads a view's stored definition.
pub(crate) fn view_command(name: &str) -> Document {
    doc! { "listCollections": 1, "filter": { "name": name } }
}

/// Every collection or view a view's `options` name, in order.
pub(crate) fn view_uses(options: &Document) -> Vec<Used> {
    let mut out = Vec::new();
    if let Ok(on) = options.get_str("viewOn") {
        if !on.is_empty() {
            out.push(Used {
                stage: "viewOn",
                db: None,
                coll: on.to_string(),
            });
        }
    }
    if let Some(p) = options.get("pipeline") {
        walk(p, &mut out);
    }
    out
}

/// The stages anywhere in `b` that read or write another collection.
fn walk(b: &Bson, out: &mut Vec<Used>) {
    match b {
        Bson::Document(d) => {
            for (k, v) in d {
                let (stage, at) = match k.as_str() {
                    "$lookup" => ("$lookup", field(v, "from")),
                    "$graphLookup" => ("$graphLookup", field(v, "from")),
                    "$unionWith" => ("$unionWith", plain_or(v, "coll")),
                    "$merge" => ("$merge", plain_or(v, "into")),
                    "$out" => ("$out", target(v)),
                    _ => ("", None),
                };
                if let Some((db, coll)) = at {
                    out.push(Used { stage, db, coll });
                }
                walk(v, out);
            }
        }
        Bson::Array(a) => a.iter().for_each(|x| walk(x, out)),
        _ => {}
    }
}

/// A collection named as `"c"` or `{db: "d", coll: "c"}`.
fn target(b: &Bson) -> Option<(Option<String>, String)> {
    match b {
        Bson::String(c) if !c.is_empty() => Some((None, c.clone())),
        Bson::Document(d) => {
            let coll = d.get_str("coll").ok().filter(|c| !c.is_empty())?;
            Some((d.get_str("db").ok().map(str::to_string), coll.to_string()))
        }
        _ => None,
    }
}

/// `{from: …}`: its `key`.
fn field(v: &Bson, key: &str) -> Option<(Option<String>, String)> {
    v.as_document().and_then(|d| d.get(key)).and_then(target)
}

/// `$unionWith: "c"` or `{coll: "c"}`; `$merge: "c"` or `{into: …}`.
fn plain_or(v: &Bson, key: &str) -> Option<(Option<String>, String)> {
    match v {
        Bson::String(_) => target(v),
        Bson::Document(d) if key == "coll" => target(v).or_else(|| d.get(key).and_then(target)),
        _ => field(v, key),
    }
}

/// The report for a view of database `db`: its uses in this database as
/// items (their kind from `objects`, a collection when it doesn't exist),
/// the others as external sources.
pub(crate) fn report(db: &str, uses: &[Used], objects: &[DbObject]) -> DependencyReport {
    let mut r = DependencyReport {
        scanned: 1,
        ..Default::default()
    };
    for u in uses {
        match u.db.as_deref().filter(|d| *d != db) {
            Some(other) => {
                let detail = Some(u.stage.to_string());
                let x = ExternalRef {
                    system: Some("MongoDB".into()),
                    database: Some(other.to_string()),
                    name: Some(u.coll.clone()),
                    detail,
                    ..Default::default()
                };
                if !r.external.contains(&x) {
                    r.external.push(x);
                }
            }
            None => {
                let kind = objects
                    .iter()
                    .find(|o| o.name == u.coll)
                    .map_or(kinds::COLLECTION, |o| o.kind.as_str());
                match r
                    .items
                    .iter_mut()
                    .find(|d| d.name == u.coll && d.kind == kind)
                {
                    Some(d) => {
                        let detail = d.detail.get_or_insert_with(String::new);
                        if !detail.split(", ").any(|s| s == u.stage) {
                            detail.push_str(", ");
                            detail.push_str(u.stage);
                        }
                    }
                    None => r.items.push(Dependent {
                        kind: kind.into(),
                        schema: None,
                        name: u.coll.clone(),
                        parent: None,
                        relation: Relation::Code,
                        confidence: Confidence::Confirmed,
                        detail: Some(u.stage.to_string()),
                        mentions: Vec::new(),
                    }),
                }
            }
        }
    }
    r
}

/// `extra`'s items that `r` doesn't have yet (the generic scan's).
pub(crate) fn add_missing(r: &mut DependencyReport, extra: DependencyReport) {
    for d in extra.items {
        if !r
            .items
            .iter()
            .any(|x| x.kind == d.kind && x.name == d.name && x.schema == d.schema)
        {
            r.items.push(d);
        }
    }
    r.unreadable.extend(extra.unreadable);
}

impl MongoSession {
    pub(crate) async fn references_report(
        &mut self,
        target: &DependencyTarget,
        scan: &DependencyScan,
    ) -> Result<DependencyReport> {
        let t = &target.object;
        if t.kind != kinds::VIEW {
            return references_scan(self, target, scan).await;
        }
        let spec = match self.first_batch(view_command(&t.name), 1).await {
            Ok(s) => s,
            Err(_) => {
                let mut r = references_scan(self, target, scan).await?;
                r.note = Some(NO_LIST.into());
                return Ok(r);
            }
        };
        let Some(options) = spec
            .first()
            .and_then(|s| s.get_document("options").ok())
            .cloned()
        else {
            return references_scan(self, target, scan).await;
        };
        let uses = view_uses(&options);
        let objects = if uses.iter().any(|u| u.db.is_none()) {
            dbine_driver::Session::list_objects(self)
                .await
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let db = self.db.name().to_string();
        let mut r = report(&db, &uses, &objects);
        let generic = references_scan(self, target, scan).await?;
        add_missing(&mut r, generic);
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, name: &str) -> DbObject {
        DbObject {
            kind: kind.into(),
            schema: None,
            name: name.into(),
            parent: None,
        }
    }

    #[test]
    fn the_command_sent() {
        assert_eq!(
            view_command("v_pedidos"),
            doc! { "listCollections": 1, "filter": { "name": "v_pedidos" } }
        );
    }

    #[test]
    fn every_stage_that_names_a_collection() {
        let options = doc! {
            "viewOn": "pedidos",
            "pipeline": [
                { "$match": { "estado": "ok", "$expr": { "$gt": ["$total", 0] } } },
                { "$lookup": { "from": "clientes", "localField": "cliente", "foreignField": "_id", "as": "c" } },
                { "$lookup": {
                    "from": "productos",
                    "let": { "p": "$producto" },
                    "pipeline": [
                        { "$match": { "$expr": { "$eq": ["$_id", "$$p"] } } },
                        { "$lookup": { "from": "categorias", "localField": "cat", "foreignField": "_id", "as": "k" } },
                    ],
                    "as": "p",
                } },
                { "$graphLookup": { "from": "empleados", "startWith": "$jefe", "connectFromField": "jefe", "connectToField": "_id", "as": "cadena" } },
                { "$unionWith": "pedidos_viejos" },
                { "$unionWith": { "coll": "pedidos_archivo", "pipeline": [{ "$unionWith": "pedidos_2019" }] } },
                { "$facet": { "a": [{ "$lookup": { "from": { "db": "otra", "coll": "tasas" }, "pipeline": [], "as": "t" } }] } },
            ],
        };
        let got: Vec<(&str, Option<String>, String)> = view_uses(&options)
            .into_iter()
            .map(|u| (u.stage, u.db, u.coll))
            .collect();
        let s = |x: &str| x.to_string();
        assert_eq!(
            got,
            [
                ("viewOn", None, s("pedidos")),
                ("$lookup", None, s("clientes")),
                ("$lookup", None, s("productos")),
                ("$lookup", None, s("categorias")),
                ("$graphLookup", None, s("empleados")),
                ("$unionWith", None, s("pedidos_viejos")),
                ("$unionWith", None, s("pedidos_archivo")),
                ("$unionWith", None, s("pedidos_2019")),
                ("$lookup", Some(s("otra")), s("tasas")),
            ]
        );
        // $merge and $out, plain or in another database.
        let w = doc! { "viewOn": "a", "pipeline": [{ "$merge": { "into": { "db": "dw", "coll": "resumen" } } }, { "$out": "copia" }, { "$merge": "m" }] };
        let got: Vec<(&str, Option<String>, String)> = view_uses(&w)
            .into_iter()
            .map(|u| (u.stage, u.db, u.coll))
            .collect();
        assert_eq!(
            got,
            [
                ("viewOn", None, "a".to_string()),
                ("$merge", Some("dw".to_string()), "resumen".to_string()),
                ("$out", None, "copia".to_string()),
                ("$merge", None, "m".to_string()),
            ]
        );
        // Nothing: no viewOn, no pipeline, garbage.
        assert!(view_uses(&doc! {}).is_empty());
        assert!(view_uses(&doc! { "viewOn": 3, "pipeline": "x" }).is_empty());
        assert!(view_uses(
            &doc! { "pipeline": [{ "$lookup": { "from": 1 } }, { "$unionWith": {} }] }
        )
        .is_empty());
    }

    #[test]
    fn the_report_from_the_definition() {
        let uses = vec![
            Used {
                stage: "viewOn",
                db: None,
                coll: "pedidos".into(),
            },
            Used {
                stage: "$lookup",
                db: None,
                coll: "v_clientes".into(),
            },
            Used {
                stage: "$unionWith",
                db: None,
                coll: "pedidos".into(),
            },
            Used {
                stage: "$lookup",
                db: Some("tienda".into()),
                coll: "precios".into(),
            },
            Used {
                stage: "$lookup",
                db: Some("otra".into()),
                coll: "tasas".into(),
            },
            Used {
                stage: "$lookup",
                db: None,
                coll: "borrada".into(),
            },
        ];
        let objects = [
            obj(kinds::COLLECTION, "pedidos"),
            obj(kinds::VIEW, "v_clientes"),
            obj(kinds::COLLECTION, "precios"),
        ];
        let r = report("tienda", &uses, &objects);
        let got: Vec<(&str, &str, Option<&str>, Confidence)> = r
            .items
            .iter()
            .map(|d| {
                (
                    d.kind.as_str(),
                    d.name.as_str(),
                    d.detail.as_deref(),
                    d.confidence,
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                (
                    kinds::COLLECTION,
                    "pedidos",
                    Some("viewOn, $unionWith"),
                    Confidence::Confirmed
                ),
                (
                    kinds::VIEW,
                    "v_clientes",
                    Some("$lookup"),
                    Confidence::Confirmed
                ),
                (
                    kinds::COLLECTION,
                    "precios",
                    Some("$lookup"),
                    Confidence::Confirmed
                ),
                (
                    kinds::COLLECTION,
                    "borrada",
                    Some("$lookup"),
                    Confidence::Confirmed
                ),
            ]
        );
        assert_eq!(
            r.external,
            [ExternalRef {
                system: Some("MongoDB".into()),
                database: Some("otra".into()),
                name: Some("tasas".into()),
                detail: Some("$lookup".into()),
                ..Default::default()
            }]
        );
        // The generic scan's extras are added, never a duplicate.
        let mut r2 = r.clone();
        add_missing(
            &mut r2,
            DependencyReport {
                items: vec![
                    r.items[0].clone(),
                    Dependent {
                        name: "extra".into(),
                        confidence: Confidence::Probable,
                        ..r.items[0].clone()
                    },
                ],
                ..Default::default()
            },
        );
        assert_eq!(r2.items.len(), r.items.len() + 1);
    }
}
