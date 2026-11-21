//! The lineage of a whole catalog (`Session::lineage_graph`).
//!
//! Unity Catalog's lineage system table, `system.access.table_lineage`, in
//! one query for the catalog (Confirmed): what each table was read from, by
//! which notebook, job, pipeline, query or dashboard, over the last year.
//! A source in another catalog or a path is `external`, and so are the
//! notebooks, jobs, pipelines, queries and dashboards that wrote a table
//! (with a link to them in the workspace). The lineage is captured from what
//! ran, so the contract's scan still reads the definitions (views,
//! functions) and adds what hasn't run yet as Probable, and the foreign keys.
//! Without the system table (the legacy `hive_metastore`, no SELECT on
//! `system.access`, a workspace without Unity Catalog) only that scan runs,
//! and the graph says so.

use crate::catalog_api::HIVE_METASTORE;
use crate::references::{add_external, clean_location, entity_url, storage_system, Entity};
use crate::DatabricksSession;
use dbine_driver::dependencies::{
    graph_scan, Confidence, DependencyScan, ExternalRef, LineageEdge, LineageExternal,
    LineageGraph, LineageScope, Relation,
};
use dbine_driver::{kinds, ObjectRef, Result};

/// `:p0` the catalog. One row per source, target and notebook, job…
/// that carried it; the last year, at most 200,000 rows.
pub(crate) const LINEAGE_SQL: &str =
    "SELECT source_table_catalog, source_table_schema, source_table_name, source_path,
       target_table_schema, target_table_name, entity_type, entity_id
  FROM system.access.table_lineage
 WHERE target_table_catalog = :p0 AND target_table_name IS NOT NULL
   AND event_date >= date_sub(current_date(), 365)
 GROUP BY ALL
 LIMIT 200000";

/// Lineage couldn't be read.
pub(crate) const NO_SYSTEM_LINEAGE: &str = "El linaje de Unity Catalog (system.access.table_lineage) no está disponible para este usuario o catálogo (hace falta SELECT sobre esa tabla del sistema y un catálogo de Unity Catalog): las líneas salen de las definiciones, sin confirmar.";

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn node(g: &LineageGraph, schema: &str, name: &str) -> Option<ObjectRef> {
    g.objects
        .iter()
        .find(|o| {
            matches!(
                o.kind.as_str(),
                kinds::TABLE | kinds::VIEW | kinds::MATERIALIZED_VIEW
            ) && eq(&o.name, name)
                && o.schema.as_deref().is_none_or(|s| eq(s, schema))
        })
        .cloned()
}

/// The kind of what ran, as the workspace's links name it.
fn entity_kind(entity_type: &str) -> String {
    match entity_type.to_ascii_uppercase().as_str() {
        "DBSQL_QUERY" => "query".into(),
        "DBSQL_DASHBOARD" => "dashboard".into(),
        "LAKEVIEW_DASHBOARD" | "DASHBOARD_V3" => "dashboard".into(),
        other => other.to_ascii_lowercase(),
    }
}

fn outside(g: &mut LineageGraph, source: ExternalRef, to: &ObjectRef) {
    let mut list: Vec<ExternalRef> = g
        .external
        .iter()
        .filter(|x| x.to.kind == to.kind && x.to.schema == to.schema && x.to.name == to.name)
        .map(|x| x.source.clone())
        .collect();
    let before = list.len();
    add_external(&mut list, source);
    if list.len() > before {
        if let Some(s) = list.pop() {
            g.external.push(LineageExternal {
                source: s,
                to: to.clone(),
            });
        }
    }
}

/// `LINEAGE_SQL`'s rows. `names`: jobs' and pipelines' names by id; `base`:
/// the workspace's URL.
pub(crate) fn add_lineage(
    g: &mut LineageGraph,
    catalog: &str,
    raw: &[Vec<Option<String>>],
    base: &str,
    names: &[(String, String)],
) {
    // The notebooks, jobs… of one edge are its detail.
    let mut via: Vec<(usize, String)> = Vec::new();
    for r in raw {
        let s = |i: usize| r.get(i).cloned().flatten().filter(|v| !v.is_empty());
        let (Some(tschema), Some(tname)) = (s(4), s(5)) else {
            continue;
        };
        let Some(to) = node(g, &tschema, &tname) else {
            continue;
        };
        let entity = s(6).zip(s(7)).map(|(t, id)| Entity {
            kind: entity_kind(&t),
            id,
            v3: t.to_ascii_uppercase().contains("LAKEVIEW"),
        });
        let source = match (s(0), s(1), s(2), s(3)) {
            (Some(c), Some(sc), Some(n), _) if eq(&c, catalog) => node(g, &sc, &n),
            (Some(c), sc, Some(n), _) => {
                outside(
                    g,
                    ExternalRef {
                        system: Some("Databricks".into()),
                        database: Some(c),
                        schema: sc,
                        name: Some(n),
                        ..Default::default()
                    },
                    &to,
                );
                None
            }
            (_, _, _, Some(path)) => {
                outside(
                    g,
                    ExternalRef {
                        system: Some(storage_system(&path).into()),
                        name: Some(clean_location(&path)),
                        ..Default::default()
                    },
                    &to,
                );
                None
            }
            _ => None,
        };
        if let Some(e) = &entity {
            let name = names
                .iter()
                .find(|(id, _)| *id == e.id)
                .map_or_else(|| e.id.clone(), |(_, n)| n.clone());
            outside(
                g,
                ExternalRef {
                    system: Some(format!("Databricks {}", e.kind)),
                    name: Some(name),
                    detail: entity_url(base, e),
                    ..Default::default()
                },
                &to,
            );
        }
        let Some(from) = source else { continue };
        if from.schema == to.schema && eq(&from.name, &to.name) {
            continue;
        }
        let idx = match g.edges.iter().position(|e| {
            e.from.schema == from.schema
                && eq(&e.from.name, &from.name)
                && e.to.schema == to.schema
                && eq(&e.to.name, &to.name)
        }) {
            Some(i) => {
                g.edges[i].confidence = Confidence::Confirmed;
                i
            }
            None => {
                g.edges.push(LineageEdge {
                    from,
                    to,
                    relation: Relation::Code,
                    confidence: Confidence::Confirmed,
                    detail: None,
                    mentions: Vec::new(),
                });
                g.edges.len() - 1
            }
        };
        if let Some(e) = entity {
            via.push((idx, format!("{} {}", e.kind, e.id)));
        }
    }
    for (i, _) in via.clone() {
        let mut parts: Vec<String> = Vec::new();
        for (j, v) in &via {
            if *j == i && !parts.contains(v) {
                parts.push(v.clone());
            }
        }
        g.edges[i].detail = Some(parts.join(" · "));
    }
}

impl DatabricksSession {
    pub(crate) async fn lineage_of(
        &mut self,
        database: &str,
        scope: &LineageScope,
        scan: &DependencyScan,
    ) -> Result<LineageGraph> {
        let catalog = if database.is_empty() {
            self.catalog()?.to_string()
        } else {
            database.to_string()
        };
        // Foreign keys run on the warehouse: if they can't be read the graph
        // still has the rest.
        let mut g = match graph_scan(self, &catalog, scope, scan).await {
            Ok(g) => g,
            Err(e) => {
                tracing::debug!("databricks lineage scan: {e}");
                let mut ctx = scan.clone();
                ctx.foreign_keys = false;
                graph_scan(self, &catalog, scope, &ctx).await?
            }
        };
        if eq(&catalog, HIVE_METASTORE) {
            g.notes.push(NO_SYSTEM_LINEAGE.into());
            return Ok(g);
        }
        match self.text_rows(LINEAGE_SQL, &[&catalog]).await {
            Ok(raw) => {
                let entities: Vec<Entity> = raw
                    .iter()
                    .filter_map(|r| {
                        let t = r.get(6).cloned().flatten()?;
                        Some(Entity {
                            kind: entity_kind(&t),
                            id: r.get(7).cloned().flatten()?,
                            v3: false,
                        })
                    })
                    .collect();
                let names = self.entity_names(&entities).await;
                let base = self.api.base.clone();
                add_lineage(&mut g, &catalog, &raw, &base, &names);
            }
            Err(e) => {
                tracing::debug!("databricks system.access.table_lineage: {e}");
                g.notes.push(NO_SYSTEM_LINEAGE.into());
            }
        }
        Ok(g)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(kind: &str, schema: &str, name: &str) -> ObjectRef {
        ObjectRef {
            kind: kind.into(),
            schema: Some(schema.into()),
            name: name.into(),
        }
    }

    fn raw(rows: &[&[&str]]) -> Vec<Vec<Option<String>>> {
        rows.iter()
            .map(|r| {
                r.iter()
                    .map(|v| (!v.is_empty()).then(|| v.to_string()))
                    .collect()
            })
            .collect()
    }

    #[test]
    fn lineage_rows_are_confirmed_edges_and_external_sources() {
        let mut g = LineageGraph {
            objects: vec![
                o("table", "bronze", "orders"),
                o("table", "silver", "orders"),
                o("view", "gold", "v_sales"),
            ],
            ..Default::default()
        };
        // source catalog, schema, name, path, target schema, name, entity type, id
        let data = raw(&[
            &[
                "main", "bronze", "orders", "", "silver", "orders", "JOB", "774",
            ],
            &[
                "main", "bronze", "orders", "", "silver", "orders", "NOTEBOOK", "111",
            ],
            &["main", "silver", "orders", "", "gold", "v_sales", "", ""],
            &["crm", "public", "leads", "", "gold", "v_sales", "", ""],
            &[
                "",
                "",
                "",
                "s3://bucket/raw/?x=1",
                "bronze",
                "orders",
                "PIPELINE",
                "p1",
            ],
            // A target that is not in the graph.
            &["main", "bronze", "orders", "", "silver", "gone", "", ""],
        ]);
        add_lineage(
            &mut g,
            "main",
            &data,
            "https://w.example.com",
            &[("774".into(), "nightly".into())],
        );
        let mut got: Vec<String> = g
            .edges
            .iter()
            .map(|e| {
                format!(
                    "{}.{}>{}.{}:{:?}",
                    e.from.schema.as_deref().unwrap(),
                    e.from.name,
                    e.to.schema.as_deref().unwrap(),
                    e.to.name,
                    e.confidence
                )
            })
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                "bronze.orders>silver.orders:Confirmed",
                "silver.orders>gold.v_sales:Confirmed"
            ]
        );
        let e = g
            .edges
            .iter()
            .find(|e| e.to.schema.as_deref() == Some("silver"))
            .unwrap();
        assert_eq!(e.detail.as_deref(), Some("job 774 · notebook 111"));
        let ext: Vec<String> = g
            .external
            .iter()
            .map(|x| {
                format!(
                    "{}:{}>{}",
                    x.source.system.as_deref().unwrap(),
                    x.source.name.as_deref().unwrap(),
                    x.to.name
                )
            })
            .collect();
        assert!(
            ext.contains(&"Databricks:leads>v_sales".to_string()),
            "{ext:?}"
        );
        assert!(
            ext.contains(&"Databricks job:nightly>orders".to_string()),
            "{ext:?}"
        );
        assert!(
            ext.contains(&"Databricks pipeline:p1>orders".to_string()),
            "{ext:?}"
        );
        assert!(
            ext.contains(&"Amazon S3:s3://bucket/raw/>orders".to_string()),
            "{ext:?}"
        );
    }

    #[test]
    fn the_sql_reads_the_catalogs_lineage() {
        assert!(LINEAGE_SQL.contains("FROM system.access.table_lineage"));
        assert!(LINEAGE_SQL.contains("target_table_catalog = :p0"));
    }
}
