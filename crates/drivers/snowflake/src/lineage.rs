//! The lineage of a whole database (`Session::lineage_graph`).
//!
//! One read of `SNOWFLAKE.ACCOUNT_USAGE.OBJECT_DEPENDENCIES` for the
//! database (Confirmed: views, materialized views, functions, streams,
//! dynamic and external tables, tasks…), one of
//! `INFORMATION_SCHEMA.EXTERNAL_TABLES` for the files external tables read,
//! and the contract's scan for the rest: foreign keys, and the code of what
//! the catalog doesn't track (procedures, tasks) or doesn't have yet. The
//! view lags up to three hours and needs IMPORTED PRIVILEGES on the
//! SNOWFLAKE database; without it (or while it is empty) every definition
//! is read, as the contract's scan does, and the graph says so. Tasks are
//! objects of their own here (kind `task`), not jobs.

use crate::references::{
    domain_kind, external_table, rows as catalog_rows, Row, LATENCY, NO_ACCOUNT_USAGE,
};
use crate::SnowflakeSession;
use dbine_driver::dependencies::{
    graph_scan, Confidence, DependencyScan, ExternalRef, LineageEdge, LineageExternal,
    LineageGraph, LineageScope, Relation,
};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{kinds, ObjectRef, Result};

/// `?` the database; one row per dependency.
pub(crate) const DATABASE_SQL: &str = "SELECT referenced_database, referenced_schema, referenced_object_name,
       referenced_object_domain, referencing_object_domain, referencing_schema, referencing_object_name
  FROM SNOWFLAKE.ACCOUNT_USAGE.OBJECT_DEPENDENCIES
 WHERE referencing_database = ?";

/// [`DATABASE_SQL`] narrowed to the objects of these schemas (`?` each after
/// the database); no schemas, the whole database.
pub(crate) fn database_sql(schemas: &[String]) -> String {
    if schemas.is_empty() {
        return DATABASE_SQL.to_string();
    }
    let marks = vec!["?"; schemas.len()].join(", ");
    format!("{DATABASE_SQL}\n   AND referencing_schema IN ({marks})")
}

/// The files every external table of the database reads.
pub(crate) fn external_tables_sql(database: &str) -> String {
    format!(
        "SELECT table_schema, table_name, location, file_format_name, file_format_type
  FROM {}.INFORMATION_SCHEMA.EXTERNAL_TABLES",
        qualified_name(Quote::Double, None, database)
    )
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn node(g: &LineageGraph, schema: &str, name: &str, kind: &str) -> Option<ObjectRef> {
    g.objects
        .iter()
        .find(|o| {
            o.kind == kind && eq(&o.name, name) && o.schema.as_deref().is_none_or(|s| eq(s, schema))
        })
        .cloned()
}

fn outside(g: &mut LineageGraph, source: ExternalRef, to: &ObjectRef) {
    let same = |x: &LineageExternal| {
        x.to.kind == to.kind
            && x.to.schema == to.schema
            && x.to.name == to.name
            && x.source.system == source.system
            && x.source.database == source.database
            && x.source.schema == source.schema
            && x.source.name == source.name
    };
    if !g.external.iter().any(same) {
        g.external.push(LineageExternal {
            source,
            to: to.clone(),
        });
    }
}

/// `DATABASE_SQL`'s rows: the dependency and the object that has it
/// (schema, name).
pub(crate) fn dependencies(raw: &[Vec<Option<String>>]) -> Vec<(Row, (String, String))> {
    raw.iter()
        .filter(|r| r.get(2).is_some_and(Option::is_some))
        .map(|r| {
            let s = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            (
                catalog_rows(std::slice::from_ref(r)).remove(0),
                (s(5), s(6)),
            )
        })
        .collect()
}

/// `DATABASE_SQL`'s rows: confirmed edges between the graph's objects, and
/// sources outside it. Returns how many rows named an object of the graph.
pub(crate) fn add_catalog(
    g: &mut LineageGraph,
    db: &str,
    deps: &[(Row, (String, String))],
) -> usize {
    let mut used = 0;
    for (r, (user_schema, user_name)) in deps {
        let Some(kind) = domain_kind(&r.referencing_domain) else {
            continue;
        };
        let Some(to) = node(g, user_schema, user_name, kind) else {
            continue;
        };
        used += 1;
        match domain_kind(&r.domain).filter(|_| eq(&r.database, db)) {
            Some(k) => {
                let Some(from) = node(g, &r.schema, &r.name, k) else {
                    continue;
                };
                if g.edges
                    .iter_mut()
                    .find(|e| {
                        e.from.kind == from.kind
                            && e.from.name == from.name
                            && e.from.schema == from.schema
                            && e.to.kind == to.kind
                            && e.to.name == to.name
                            && e.to.schema == to.schema
                    })
                    .map(|e| e.confidence = Confidence::Confirmed)
                    .is_none()
                {
                    g.edges.push(LineageEdge {
                        from,
                        to,
                        relation: Relation::Code,
                        confidence: Confidence::Confirmed,
                        detail: None,
                        mentions: Vec::new(),
                    });
                }
            }
            None => outside(
                g,
                ExternalRef {
                    system: Some("Snowflake".into()),
                    database: Some(r.database.clone()),
                    schema: Some(r.schema.clone()),
                    name: Some(r.name.clone()),
                    detail: Some(r.domain.clone()),
                    ..Default::default()
                },
                &to,
            ),
        }
    }
    used
}

/// `external_tables_sql`'s rows.
pub(crate) fn add_external_tables(g: &mut LineageGraph, raw: &[Vec<Option<String>>]) {
    for r in raw {
        let (Some(schema), Some(name)) =
            (r.first().cloned().flatten(), r.get(1).cloned().flatten())
        else {
            continue;
        };
        let Some(to) = node(g, &schema, &name, kinds::TABLE) else {
            continue;
        };
        if let Some(x) = external_table(&[r[2..].to_vec()]) {
            outside(g, x, &to);
        }
    }
}

impl SnowflakeSession {
    pub(crate) async fn lineage_of(
        &mut self,
        database: &str,
        scope: &LineageScope,
        scan: &DependencyScan,
    ) -> Result<LineageGraph> {
        let db = if database.is_empty() {
            self.database()?
        } else {
            database.to_string()
        };
        // Only the schemas asked for, when it asks for some.
        let mut args: Vec<&str> = vec![&db];
        args.extend(scope.schemas.iter().map(String::as_str));
        let raw = self.text_rows(&database_sql(&scope.schemas), &args).await;
        let deps = raw.as_ref().map(|r| dependencies(r)).unwrap_or_default();
        // Procedures and tasks are not tracked; with no catalog, nothing is.
        let mut ctx = scan.clone();
        if !deps.is_empty() {
            ctx.written_kinds
                .retain(|k| k == kinds::PROCEDURE || k == crate::ddl::TASK);
        }
        let mut g = graph_scan(self, &db, scope, &ctx).await?;
        match &raw {
            Ok(_) if deps.is_empty() => g.notes.push(LATENCY.into()),
            Ok(_) => {
                add_catalog(&mut g, &db, &deps);
            }
            Err(e) => {
                tracing::debug!("snowflake OBJECT_DEPENDENCIES: {e}");
                g.notes.push(NO_ACCOUNT_USAGE.into());
            }
        }
        if let Ok(raw) = self.text_rows(&external_tables_sql(&db), &[]).await {
            add_external_tables(&mut g, &raw);
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
    fn dependencies_become_confirmed_edges_and_external_sources() {
        let mut g = LineageGraph {
            objects: vec![
                o("table", "PUBLIC", "ORDERS"),
                o("table", "REF", "RATES"),
                o("view", "PUBLIC", "V_SALES"),
                o("materialized_view", "PUBLIC", "MV_SALES"),
                o("task", "PUBLIC", "T_LOAD"),
                o("table", "PUBLIC", "EXT"),
            ],
            ..Default::default()
        };
        // referenced db, schema, name, domain, referencing domain, schema, name
        let data = raw(&[
            &[
                "SALES", "PUBLIC", "ORDERS", "TABLE", "VIEW", "PUBLIC", "V_SALES",
            ],
            &[
                "SALES", "REF", "RATES", "TABLE", "VIEW", "PUBLIC", "V_SALES",
            ],
            &[
                "SALES",
                "PUBLIC",
                "V_SALES",
                "VIEW",
                "MATERIALIZED VIEW",
                "PUBLIC",
                "MV_SALES",
            ],
            &[
                "CRM", "PUBLIC", "LEADS", "TABLE", "VIEW", "PUBLIC", "V_SALES",
            ],
            &[
                "SALES",
                "PUBLIC",
                "RAW_STAGE",
                "STAGE",
                "TASK",
                "PUBLIC",
                "T_LOAD",
            ],
            // A table that is not a node of the scope.
            &[
                "SALES", "PUBLIC", "ORDERS", "TABLE", "VIEW", "PUBLIC", "V_GONE",
            ],
        ]);
        let used = add_catalog(&mut g, "sales", &dependencies(&data));
        assert_eq!(used, 5);
        let mut got: Vec<String> = g
            .edges
            .iter()
            .map(|e| format!("{}>{}:{:?}", e.from.name, e.to.name, e.confidence))
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                "ORDERS>V_SALES:Confirmed",
                "RATES>V_SALES:Confirmed",
                "V_SALES>MV_SALES:Confirmed"
            ]
        );
        let ext: Vec<String> = g
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
        assert_eq!(ext, ["CRM.LEADS>V_SALES", "SALES.RAW_STAGE>T_LOAD"]);
    }

    #[test]
    fn external_tables_read_their_location() {
        let mut g = LineageGraph {
            objects: vec![o("table", "PUBLIC", "EXT")],
            ..Default::default()
        };
        add_external_tables(
            &mut g,
            &raw(&[
                &["PUBLIC", "EXT", "s3://bucket/path/?token=1", "", "CSV"],
                &["PUBLIC", "GONE", "s3://x", "", ""],
            ]),
        );
        assert_eq!(g.external.len(), 1);
        assert_eq!(g.external[0].source.system.as_deref(), Some("Amazon S3"));
    }

    #[test]
    fn the_sql_sent() {
        assert!(DATABASE_SQL.contains("WHERE referencing_database = ?"));
        assert_eq!(database_sql(&[]), DATABASE_SQL);
        let narrowed = database_sql(&["SALES".into(), "REF".into()]);
        assert!(narrowed.starts_with(DATABASE_SQL));
        assert!(
            narrowed.ends_with("AND referencing_schema IN (?, ?)"),
            "{narrowed}"
        );
        assert!(
            external_tables_sql("Sales \"DB\"").contains("\"Sales \"\"DB\"\"\".INFORMATION_SCHEMA")
        );
    }
}
