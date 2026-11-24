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

/// How far back the catalog's lineage reads (days). The lineage table keeps a year; the
/// diagram of a database draws what ran lately, and a line that hasn't run for longer still
/// comes from the definitions (as Probable). Narrower than a year because the table is read
/// for the whole catalog on a warehouse.
pub(crate) const LINEAGE_DAYS: u32 = 90;
/// Rows the lineage query returns at most.
pub(crate) const LINEAGE_ROWS: u32 = 200_000;

/// The catalog's lineage (`:p0` the catalog, `:p1…` the schemas asked for, when there are):
/// one row per source, target and notebook, job… that carried it in the last
/// [`LINEAGE_DAYS`] days, at most [`LINEAGE_ROWS`] rows.
pub(crate) fn lineage_sql(schemas: usize) -> String {
    let in_schemas = if schemas == 0 {
        String::new()
    } else {
        let list: Vec<String> = (1..=schemas).map(|i| format!(":p{i}")).collect();
        format!(" AND target_table_schema IN ({})", list.join(", "))
    };
    format!(
        "SELECT source_table_catalog, source_table_schema, source_table_name, source_path,
       target_table_schema, target_table_name, entity_type, entity_id
  FROM system.access.table_lineage
 WHERE target_table_catalog = :p0 AND target_table_name IS NOT NULL{in_schemas}
   AND event_date >= date_sub(current_date(), {LINEAGE_DAYS})
 GROUP BY ALL
 LIMIT {LINEAGE_ROWS}"
    )
}

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
pub(crate) fn entity_kind(entity_type: &str) -> String {
    match entity_type.to_ascii_uppercase().as_str() {
        "DBSQL_QUERY" => "query".into(),
        "DBSQL_DASHBOARD" => "dashboard".into(),
        "LAKEVIEW_DASHBOARD" | "DASHBOARD_V3" => "dashboard".into(),
        other => other.to_ascii_lowercase(),
    }
}

/// A Lakeview dashboard (its page and its API are not the legacy dashboards').
pub(crate) fn is_v3(entity_type: &str) -> bool {
    let t = entity_type.to_ascii_uppercase();
    t.contains("LAKEVIEW") || t == "DASHBOARD_V3"
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
            v3: is_v3(&t),
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
        let mut args: Vec<&str> = vec![catalog.as_str()];
        args.extend(scope.schemas.iter().map(String::as_str));
        match self
            .text_rows(&lineage_sql(scope.schemas.len()), &args)
            .await
        {
            Ok(raw) => {
                let entities: Vec<Entity> = raw
                    .iter()
                    .filter_map(|r| {
                        let t = r.get(6).cloned().flatten()?;
                        Some(Entity {
                            v3: is_v3(&t),
                            kind: entity_kind(&t),
                            id: r.get(7).cloned().flatten()?,
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
        let sql = lineage_sql(0);
        assert!(sql.contains("FROM system.access.table_lineage"));
        assert!(sql.contains("target_table_catalog = :p0"));
        // The window is the constant's, not a year; no schema filter without schemas.
        assert!(sql.contains(&format!("date_sub(current_date(), {LINEAGE_DAYS})")));
        assert!(LINEAGE_DAYS < 365 && !sql.contains("365"));
        assert!(!sql.contains("target_table_schema IN"));
        // With schemas: one parameter each, after the catalog's.
        let sql = lineage_sql(2);
        assert!(sql.contains("target_table_schema IN (:p1, :p2)"));
        assert!(sql.ends_with("LIMIT 200000"));
    }

    // -- the pieces of a row --------------------------------------------------------------------

    #[test]
    fn what_ran_gets_its_kind_and_a_link_to_it() {
        assert_eq!(entity_kind("DBSQL_QUERY"), "query");
        assert_eq!(entity_kind("DBSQL_DASHBOARD"), "dashboard");
        assert_eq!(entity_kind("LAKEVIEW_DASHBOARD"), "dashboard");
        assert_eq!(entity_kind("DASHBOARD_V3"), "dashboard");
        assert_eq!(entity_kind("Notebook"), "notebook");
        assert_eq!(entity_kind("JOB"), "job");
        let mut g = LineageGraph {
            objects: vec![o("table", "gold", "kpis")],
            ..Default::default()
        };
        let data = raw(&[
            &["", "", "", "", "gold", "kpis", "LAKEVIEW_DASHBOARD", "d3"],
            &["", "", "", "", "gold", "kpis", "DBSQL_DASHBOARD", "d1"],
            &["", "", "", "", "gold", "kpis", "DBSQL_QUERY", "q1"],
            &["", "", "", "", "gold", "kpis", "NOTEBOOK", "n1"],
            &["", "", "", "", "gold", "kpis", "PIPELINE", "p1"],
        ]);
        add_lineage(&mut g, "main", &data, "https://w.example.com/", &[]);
        let links: Vec<(String, String)> = g
            .external
            .iter()
            .map(|x| {
                (
                    x.source.system.clone().unwrap(),
                    x.source.detail.clone().unwrap_or_default(),
                )
            })
            .collect();
        for want in [
            (
                "Databricks dashboard",
                "https://w.example.com/dashboardsv3/d3",
            ),
            (
                "Databricks dashboard",
                "https://w.example.com/sql/dashboards/d1",
            ),
            ("Databricks query", "https://w.example.com/sql/editor/q1"),
            ("Databricks notebook", "https://w.example.com/#notebook/n1"),
            ("Databricks pipeline", "https://w.example.com/pipelines/p1"),
        ] {
            assert!(
                links.contains(&(want.0.to_string(), want.1.to_string())),
                "{want:?} in {links:?}"
            );
        }
        assert!(
            g.edges.is_empty(),
            "what ran is a source outside, not an edge"
        );
    }

    #[test]
    fn names_match_without_case_and_a_table_never_feeds_itself() {
        let mut g = LineageGraph {
            objects: vec![
                o("table", "Bronze", "Orders"),
                o("table", "silver", "orders"),
            ],
            ..Default::default()
        };
        let data = raw(&[
            &["MAIN", "bronze", "ORDERS", "", "SILVER", "Orders", "", ""],
            // A MERGE into itself.
            &[
                "main", "silver", "orders", "", "silver", "orders", "JOB", "1",
            ],
        ]);
        add_lineage(&mut g, "main", &data, "https://w", &[]);
        assert_eq!(g.edges.len(), 1);
        assert_eq!(g.edges[0].from.name, "Orders", "the graph's own name");
        assert_eq!(g.edges[0].to.name, "orders");
    }

    #[test]
    fn a_line_the_code_guessed_is_confirmed_with_every_job_that_carried_it_once() {
        let (from, to) = (
            o("table", "bronze", "orders"),
            o("table", "silver", "orders"),
        );
        let mut g = LineageGraph {
            objects: vec![from.clone(), to.clone()],
            edges: vec![LineageEdge {
                from,
                to,
                relation: Relation::Code,
                confidence: Confidence::Probable,
                detail: None,
                mentions: Vec::new(),
            }],
            ..Default::default()
        };
        let data = raw(&[
            &[
                "main", "bronze", "orders", "", "silver", "orders", "JOB", "7",
            ],
            &[
                "main", "bronze", "orders", "", "silver", "orders", "JOB", "7",
            ],
            &[
                "main", "bronze", "orders", "", "silver", "orders", "NOTEBOOK", "9",
            ],
        ]);
        add_lineage(&mut g, "main", &data, "https://w", &[]);
        assert_eq!(g.edges.len(), 1, "no second edge for the same pair");
        assert_eq!(g.edges[0].confidence, Confidence::Confirmed);
        assert_eq!(g.edges[0].detail.as_deref(), Some("job 7 · notebook 9"));
        let jobs = g
            .external
            .iter()
            .filter(|x| x.source.system.as_deref() == Some("Databricks job"))
            .count();
        assert_eq!(jobs, 1, "the job is one source outside, however many rows");
    }

    #[test]
    fn storage_paths_name_their_service() {
        let mut g = LineageGraph {
            objects: vec![o("table", "bronze", "orders")],
            ..Default::default()
        };
        let data = raw(&[
            &[
                "",
                "",
                "",
                "abfss://raw@lake.dfs.core.windows.net/orders",
                "bronze",
                "orders",
                "",
                "",
            ],
            &["", "", "", "gs://bucket/orders", "bronze", "orders", "", ""],
            &[
                "",
                "",
                "",
                "/Volumes/main/raw/files/orders.csv",
                "bronze",
                "orders",
                "",
                "",
            ],
        ]);
        add_lineage(&mut g, "main", &data, "https://w", &[]);
        let systems: Vec<&str> = g
            .external
            .iter()
            .filter_map(|x| x.source.system.as_deref())
            .collect();
        assert!(systems.contains(&"Azure Data Lake Storage"), "{systems:?}");
        assert!(systems.contains(&"Google Cloud Storage"), "{systems:?}");
        assert_eq!(systems.len(), 3, "{systems:?}");
    }

    // -- the whole read, against a fake workspace ------------------------------------------------

    use crate::transfer::tests::{serve, session};
    use std::sync::{Arc, Mutex};

    const DONE: &str =
        r#"{"statement_id":"s","status":{"state":"SUCCEEDED"},"result":{"data_array":[]}}"#;
    const FAILED: &str = r#"{"statement_id":"s","status":{"state":"FAILED","error":{"error_code":"PERMISSION_DENIED","message":"no SELECT on system.access.table_lineage"}}}"#;

    /// The catalog `main` (bronze.orders, silver.orders, the view gold.v_sales) and its lineage
    /// rows (`None`: the system table can't be read). `fk_fail`: the foreign keys' query fails.
    /// Gives the base URL and the statements asked.
    async fn workspace(
        lineage: Option<&'static str>,
        fk_fail: bool,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let asked = Arc::new(Mutex::new(Vec::new()));
        let seen = asked.clone();
        let base = serve(Arc::new(move |m, p, body| {
            let (path, _) = p.split_once('?').unwrap_or((p, ""));
            match (m, path) {
                ("GET", "/api/2.1/unity-catalog/catalogs/main") => (200, r#"{"name":"main"}"#.into()),
                ("GET", "/api/2.1/unity-catalog/table-summaries") => (
                    200,
                    r#"{"tables":[{"full_name":"main.bronze.orders","table_type":"MANAGED"},{"full_name":"main.silver.orders","table_type":"MANAGED"},{"full_name":"main.gold.v_sales","table_type":"VIEW"}]}"#.into(),
                ),
                ("GET", "/api/2.1/unity-catalog/schemas") => (
                    200,
                    r#"{"schemas":[{"name":"bronze"},{"name":"silver"},{"name":"gold"}]}"#.into(),
                ),
                ("GET", "/api/2.1/jobs/get") => (200, r#"{"job_id":774,"settings":{"name":"nightly load"}}"#.into()),
                ("POST", "/api/2.0/sql/statements") => {
                    let st: serde_json::Value = serde_json::from_str(body).unwrap();
                    let text = st["statement"].as_str().unwrap_or("").to_string();
                    seen.lock().unwrap().push(body.to_string());
                    if text.contains("system.access.table_lineage") {
                        match lineage {
                            Some(rows) => (
                                200,
                                format!(r#"{{"statement_id":"s","status":{{"state":"SUCCEEDED"}},"result":{{"data_array":{rows}}}}}"#),
                            ),
                            None => (200, FAILED.into()),
                        }
                    } else if fk_fail && text.contains("update_rule") {
                        (200, FAILED.into())
                    } else {
                        (200, DONE.into())
                    }
                }
                _ => (404, r#"{"message":"unexpected"}"#.into()),
            }
        }))
        .await;
        (base, asked)
    }

    fn scan() -> DependencyScan {
        DependencyScan {
            source_kinds: vec![kinds::VIEW.into()],
            dialect: crate::script::dialect(),
            foreign_keys: true,
            written_kinds: vec![
                kinds::VIEW.into(),
                kinds::MATERIALIZED_VIEW.into(),
                kinds::FUNCTION.into(),
            ],
            has_schemas: true,
        }
    }

    fn lineage_asked(asked: &Arc<Mutex<Vec<String>>>) -> Vec<serde_json::Value> {
        asked
            .lock()
            .unwrap()
            .iter()
            .map(|b| serde_json::from_str::<serde_json::Value>(b).unwrap())
            .filter(|b| {
                b["statement"]
                    .as_str()
                    .unwrap_or("")
                    .contains("table_lineage")
            })
            .collect()
    }

    const ROWS: &str = r#"[
        ["main","bronze","orders",null,"silver","orders","JOB","774"],
        ["main","silver","orders",null,"gold","v_sales",null,null],
        ["crm","public","leads",null,"gold","v_sales",null,null]
    ]"#;

    #[tokio::test]
    async fn the_catalogs_lineage_confirms_the_lines_and_names_what_ran() {
        let (base, asked) = workspace(Some(ROWS), false).await;
        let mut s = session(base.clone());
        s.catalog = Some("main".into());
        let g = s
            .lineage_of("main", &LineageScope::default(), &scan())
            .await
            .unwrap();
        assert_eq!(g.objects.len(), 3, "{:?}", g.objects);
        assert!(
            !g.notes.iter().any(|n| n == NO_SYSTEM_LINEAGE),
            "{:?}",
            g.notes
        );
        let mut edges: Vec<String> = g
            .edges
            .iter()
            .map(|e| format!("{}>{}:{:?}", e.from.name, e.to.name, e.confidence))
            .collect();
        edges.sort();
        assert_eq!(
            edges,
            ["orders>orders:Confirmed", "orders>v_sales:Confirmed"]
        );
        let job = g
            .external
            .iter()
            .find(|x| x.source.system.as_deref() == Some("Databricks job"))
            .expect("the job that loads silver.orders");
        assert_eq!(
            job.source.name.as_deref(),
            Some("nightly load"),
            "named from the Jobs API"
        );
        assert_eq!(
            job.source.detail.as_deref(),
            Some(format!("{base}/jobs/774").as_str())
        );
        assert!(g
            .external
            .iter()
            .any(|x| x.source.database.as_deref() == Some("crm") && x.to.name == "v_sales"));
        // One query for the catalog: the last year, with the catalog as a parameter.
        let q = lineage_asked(&asked);
        assert_eq!(q.len(), 1);
        assert_eq!(q[0]["parameters"][0]["value"], "main");
        assert!(q[0]["statement"]
            .as_str()
            .unwrap()
            .contains(&format!("{LINEAGE_DAYS})")));
        assert!(q[0]["statement"].as_str().unwrap().contains("LIMIT 200000"));
        assert_eq!(q[0]["parameters"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_scope_with_schemas_narrows_the_lineage_query() {
        let (base, asked) = workspace(Some(ROWS), false).await;
        let mut s = session(base);
        s.catalog = Some("main".into());
        let scope = LineageScope {
            schemas: vec!["gold".into(), "silver".into()],
            ..Default::default()
        };
        s.lineage_of("main", &scope, &scan()).await.unwrap();
        let q = lineage_asked(&asked);
        assert_eq!(q.len(), 1);
        let st = q[0]["statement"].as_str().unwrap();
        assert!(st.contains("target_table_schema IN (:p1, :p2)"), "{st}");
        let values: Vec<_> = q[0]["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["value"].as_str().unwrap())
            .collect();
        assert_eq!(values, ["main", "gold", "silver"]);
    }

    #[tokio::test]
    async fn without_the_system_table_the_definitions_and_a_note() {
        let (base, _) = workspace(None, false).await;
        let mut s = session(base);
        s.catalog = Some("main".into());
        let g = s
            .lineage_of("main", &LineageScope::default(), &scan())
            .await
            .unwrap();
        assert_eq!(g.objects.len(), 3, "the graph is still there");
        assert!(
            g.notes.iter().any(|n| n == NO_SYSTEM_LINEAGE),
            "{:?}",
            g.notes
        );
        assert!(g
            .edges
            .iter()
            .all(|e| e.confidence != Confidence::Confirmed));
    }

    #[tokio::test]
    async fn hive_metastore_has_no_system_lineage_and_is_not_asked() {
        let (base, asked) = workspace(Some(ROWS), false).await;
        let mut s = session(base);
        s.catalog = Some(HIVE_METASTORE.into());
        let g = s
            .lineage_of(HIVE_METASTORE, &LineageScope::default(), &scan())
            .await
            .unwrap();
        assert!(
            g.notes.iter().any(|n| n == NO_SYSTEM_LINEAGE),
            "{:?}",
            g.notes
        );
        assert!(lineage_asked(&asked).is_empty());
    }

    #[tokio::test]
    async fn foreign_keys_that_cannot_be_read_leave_the_rest() {
        let (base, asked) = workspace(Some(ROWS), true).await;
        let mut s = session(base);
        s.catalog = Some("main".into());
        let g = s
            .lineage_of("main", &LineageScope::default(), &scan())
            .await
            .unwrap();
        assert_eq!(g.objects.len(), 3);
        assert_eq!(g.edges.len(), 2, "the catalog's lineage still comes");
        let fk_queries = asked
            .lock()
            .unwrap()
            .iter()
            .filter(|b| b.contains("update_rule"))
            .count();
        assert_eq!(
            fk_queries, 1,
            "the foreign keys are asked once, then left out"
        );
    }
}
