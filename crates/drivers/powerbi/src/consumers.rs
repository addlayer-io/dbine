//! What reads a semantic model from outside it: the reports and dashboards
//! of its workspace, as the lineage's `consumers` and a dependents report's
//! `external_users`.
//!
//! REST only, with the workspace read access a viewer already has:
//! `GET groups/{g}/reports` (each names its `datasetId`),
//! `GET groups/{g}/dashboards` and `GET groups/{g}/dashboards/{d}/tiles`
//! (each tile names its `reportId` or `datasetId`). "My workspace" has the
//! same calls without `groups/{g}`. A login that can't read them gets a
//! note and the rest of the lineage still draws.
//!
//! A dashboard reads the model through a report (dashboard -> report ->
//! model) or, for a tile pinned straight from the model, directly. Each
//! path is one [`ExternalRef`]: a dashboard through a report names it in
//! `schema`, one that reads the model directly has no `schema`.
//!
//! Reports of other workspaces that read the model (a shared model) are not
//! listed: only the model's own workspace is read.

use crate::catalog::Model;
use crate::http::Priority;
use crate::meta::service::Service;
use dbine_driver::dependencies::ExternalRef;
use dbine_driver::{Error, Result};
use serde_json::Value;

pub const REPORT: &str = "Power BI report";
pub const DASHBOARD: &str = "Power BI dashboard";

/// Reports and dashboards read from a workspace, each.
pub const MAX_ITEMS: usize = 500;
/// Dashboards whose tiles are read (one request each).
pub const MAX_DASHBOARDS_READ: usize = 100;

pub const NOT_READABLE: &str = "Con los permisos de esta conexión no se pudieron leer los reportes y tableros del área de trabajo: el linaje no muestra quién lee el modelo.";
/// The sign-in has no scope for reports and dashboards (HTTP 401 on them while the model reads fine).
pub const NO_SCOPE: &str = "El inicio de sesión de esta conexión no tiene permiso para leer los reportes y tableros (Report.Read.All y Dashboard.Read.All): el linaje no muestra quién lee el modelo. Volvé a iniciar sesión con esos permisos o usá otro cliente de Microsoft.";
pub const TOO_MANY: &str =
    "El área de trabajo tiene más de 500 reportes o tableros: se muestran los primeros 500.";
pub const TILES_CAPPED: &str = "Se leyeron los mosaicos de los primeros 100 tableros: los demás pueden leer este modelo y no aparecen.";

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Consumers {
    /// The reports and dashboards that read the model.
    pub items: Vec<ExternalRef>,
    /// What the UI tells the user, each a whole message.
    pub notes: Vec<String>,
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn base(model: &Model) -> String {
    match &model.workspace.id {
        Some(g) => format!("/groups/{g}"),
        None => String::new(),
    }
}

/// `value[]` of a list answer, cut at [`MAX_ITEMS`].
fn list(body: &Value, notes: &mut Vec<String>) -> Vec<Value> {
    let mut v = body
        .get("value")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if v.len() > MAX_ITEMS {
        v.truncate(MAX_ITEMS);
        if !notes.iter().any(|n| n == TOO_MANY) {
            notes.push(TOO_MANY.into());
        }
    }
    v
}

fn same_id(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// The reports and dashboards of the model's workspace that read it.
pub(crate) async fn read<S: Service>(s: &mut S, model: &Model) -> Result<Consumers> {
    // Kept for a minute: the details panel asks for each object it opens.
    if let Some((id, at, c)) = &s.meta_state().consumers {
        if *id == model.id && at.elapsed() < crate::meta::TTL {
            return Ok((**c).clone());
        }
    }
    let c = read_uncached(s, model).await?;
    s.meta_state().consumers = Some((
        model.id.clone(),
        std::time::Instant::now(),
        std::sync::Arc::new(c.clone()),
    ));
    Ok(c)
}

/// What a failed read of reports or dashboards means: only a lost connection ends the lineage. A 401 here
/// is the sign-in missing the scope for these endpoints (the model itself was read with the same token),
/// so it is a note and the rest of the lineage still draws.
fn degrade(e: Error, no_scope: &mut bool) -> Result<()> {
    match e {
        Error::AuthFailed(_) => {
            *no_scope = true;
            Ok(())
        }
        e if crate::meta::is_fatal(&e) => Err(e),
        _ => Ok(()),
    }
}

async fn read_uncached<S: Service>(s: &mut S, model: &Model) -> Result<Consumers> {
    let mut out = Consumers::default();
    let base = base(model);
    let workspace = Some(model.workspace.name.clone()).filter(|n| !n.is_empty());
    let mut readable = true;
    let mut no_scope = false;

    let mut reports: Vec<(String, String, Option<String>)> = Vec::new();
    match s
        .get(&format!("{base}/reports"), Priority::Background)
        .await
    {
        Ok(body) => {
            for r in list(&body, &mut out.notes) {
                let (Some(id), Some(name)) = (text(&r, "id"), text(&r, "name")) else {
                    continue;
                };
                // Reports of other models are not this model's consumers.
                if text(&r, "datasetId").is_some_and(|d| same_id(&d, &model.id)) {
                    reports.push((id, name, text(&r, "webUrl")));
                }
            }
        }
        Err(e) => {
            degrade(e, &mut no_scope)?;
            readable = false;
        }
    }
    for (_, name, url) in &reports {
        out.items.push(ExternalRef {
            system: Some(REPORT.into()),
            server: workspace.clone(),
            name: Some(name.clone()),
            detail: url.clone(),
            ..Default::default()
        });
    }

    match s
        .get(&format!("{base}/dashboards"), Priority::Background)
        .await
    {
        Ok(body) => {
            let dashboards = list(&body, &mut out.notes);
            if dashboards.len() > MAX_DASHBOARDS_READ {
                out.notes.push(TILES_CAPPED.into());
            }
            for d in dashboards.iter().take(MAX_DASHBOARDS_READ) {
                let (Some(id), Some(name)) = (text(d, "id"), text(d, "displayName")) else {
                    continue;
                };
                let tiles = match s
                    .get(
                        &format!("{base}/dashboards/{id}/tiles"),
                        Priority::Background,
                    )
                    .await
                {
                    Ok(t) => t,
                    Err(e) => {
                        degrade(e, &mut no_scope)?;
                        readable = false;
                        continue;
                    }
                };
                // The paths the dashboard reaches the model by: one per
                // report, and the model itself when a tile comes from it.
                let mut paths: Vec<Option<&str>> = Vec::new();
                for t in tiles
                    .get("value")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                {
                    let report = text(t, "reportId")
                        .and_then(|rid| reports.iter().find(|(id, _, _)| same_id(id, &rid)));
                    if let Some((_, rname, _)) = report {
                        if !paths.contains(&Some(rname.as_str())) {
                            paths.push(Some(rname.as_str()));
                        }
                    } else if text(t, "datasetId").is_some_and(|x| same_id(&x, &model.id))
                        && !paths.contains(&None)
                    {
                        paths.push(None);
                    }
                }
                for via in paths {
                    out.items.push(ExternalRef {
                        system: Some(DASHBOARD.into()),
                        server: workspace.clone(),
                        schema: via.map(str::to_string),
                        name: Some(name.clone()),
                        detail: text(d, "webUrl"),
                        ..Default::default()
                    });
                }
            }
        }
        Err(e) => {
            degrade(e, &mut no_scope)?;
            readable = false;
        }
    }
    if no_scope {
        out.notes.push(NO_SCOPE.into());
    } else if !readable {
        out.notes.push(NOT_READABLE.into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dependencies::dependents_in;
    use crate::http::Method;
    use crate::lineage::lineage_in;
    use crate::meta::fake::{Fake, GROUP};
    use crate::{MODEL, TABLE};
    use dbine_driver::dependencies::{DependencyTarget, LineageScope};
    use dbine_driver::{ObjectRef, Relation};
    use serde_json::json;

    const LINEAGE: &str = include_str!("../tests/fixtures/tmsl_lineage.json");
    const OTHER: &str = "11111111-2222-3333-4444-555555555555";

    /// A fake whose workspace has, besides the model's own readers, a report
    /// of another model and a dashboard that reads nothing of this one.
    fn workspace() -> Fake {
        let mut f = Fake::writer();
        f.definition(LINEAGE);
        let id = f.model.as_ref().unwrap().id.clone();
        f.consumers(
            &[
                json!({"id": "r1", "name": "Ventas mensuales", "datasetId": id, "webUrl": "https://app.powerbi.com/groups/g/reports/r1"}),
                json!({"id": "r2", "name": "Otro", "datasetId": OTHER}),
            ],
            &[
                json!({"id": "d1", "displayName": "Dirección", "webUrl": "https://app.powerbi.com/groups/g/dashboards/d1"}),
                json!({"id": "d2", "displayName": "Ajeno"}),
            ],
            &[
                (
                    "d1",
                    vec![
                        json!({"id": "t1", "reportId": "r1", "datasetId": id}),
                        json!({"id": "t2", "datasetId": id}),
                        json!({"id": "t3", "reportId": "r2", "datasetId": OTHER}),
                    ],
                ),
                ("d2", vec![json!({"id": "t4", "reportId": "r2"})]),
            ],
        );
        f
    }

    fn is(a: &ObjectRef, b: &ObjectRef) -> bool {
        a.kind == b.kind && a.name == b.name
    }

    fn who(c: &[ExternalRef]) -> Vec<(String, String, Option<String>)> {
        c.iter()
            .map(|e| {
                (
                    e.system.clone().unwrap(),
                    e.name.clone().unwrap(),
                    e.schema.clone(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn the_lineage_has_the_model_and_what_reads_it() {
        let mut f = workspace();
        let g = lineage_in(&mut f, &LineageScope::default()).await.unwrap();
        let model = ObjectRef {
            kind: MODEL.into(),
            schema: None,
            name: "Ventas".into(),
        };
        assert!(g.objects.iter().any(|o| is(o, &model)));
        // Each table is read by the model.
        let tables: Vec<_> = g.objects.iter().filter(|o| o.kind == TABLE).collect();
        assert!(!tables.is_empty());
        for t in &tables {
            assert!(
                g.edges
                    .iter()
                    .any(|e| is(&e.from, t) && is(&e.to, &model) && e.relation == Relation::Code),
                "{}",
                t.name
            );
        }
        assert!(g.consumers.iter().all(|c| is(&c.from, &model)));
        let c: Vec<ExternalRef> = g.consumers.iter().map(|c| c.consumer.clone()).collect();
        assert_eq!(
            who(&c),
            [
                (REPORT.into(), "Ventas mensuales".into(), None),
                (
                    DASHBOARD.into(),
                    "Dirección".into(),
                    Some("Ventas mensuales".into())
                ),
                (DASHBOARD.into(), "Dirección".into(), None),
            ]
        );
        assert_eq!(
            c[0].detail.as_deref(),
            Some("https://app.powerbi.com/groups/g/reports/r1")
        );
        assert_eq!(c[0].server.as_deref(), Some("Finanzas"));
        assert!(!g.notes.iter().any(|n| n == NOT_READABLE), "{:?}", g.notes);
        // Only reads.
        assert!(f.writes().is_empty());
    }

    #[tokio::test]
    async fn dependents_of_any_object_list_the_readers_of_the_model() {
        for object in [
            ObjectRef {
                kind: TABLE.into(),
                schema: None,
                name: "Sales".into(),
            },
            ObjectRef {
                kind: MODEL.into(),
                schema: None,
                name: "Ventas".into(),
            },
        ] {
            let mut f = workspace();
            let r = dependents_in(
                &mut f,
                &DependencyTarget {
                    object,
                    column: None,
                },
            )
            .await
            .unwrap();
            assert_eq!(r.external_users.len(), 3);
            assert_eq!(r.external_users[0].system.as_deref(), Some(REPORT));
        }
    }

    /// A login without workspace read: a note, and the rest still draws.
    #[tokio::test]
    async fn without_workspace_read_there_is_a_note_and_the_graph_still_draws() {
        let mut f = Fake::writer();
        f.definition(LINEAGE);
        f.routes.lock().unwrap().retain(|r| r.fabric);
        let g = lineage_in(&mut f, &LineageScope::default()).await.unwrap();
        assert!(g.consumers.is_empty());
        assert!(g.notes.iter().any(|n| n == NOT_READABLE));
        assert!(g.objects.len() > 1 && !g.edges.is_empty());
        let r = dependents_in(
            &mut f,
            &DependencyTarget {
                object: ObjectRef {
                    kind: TABLE.into(),
                    schema: None,
                    name: "Sales".into(),
                },
                column: None,
            },
        )
        .await
        .unwrap();
        assert!(r.external_users.is_empty());
        assert_eq!(r.note.as_deref(), Some(NOT_READABLE));
    }

    /// A 401 on reports and dashboards (the sign-in has no scope for them, while the model reads fine)
    /// used to end the whole lineage with "volvé a iniciar sesión". It is a note now, and the graph draws.
    #[tokio::test]
    async fn a_401_on_reports_is_a_note_not_the_end_of_the_lineage() {
        let mut f = Fake::writer();
        f.definition(LINEAGE);
        let g = GROUP;
        for path in [
            format!("/groups/{g}/reports"),
            format!("/groups/{g}/dashboards"),
        ] {
            f.routes.lock().unwrap().retain(|r| r.path != path);
            f.route(Method::Get, &path, 401, serde_json::json!({}));
        }
        let graph = lineage_in(&mut f, &LineageScope::default()).await.unwrap();
        assert!(graph.consumers.is_empty());
        assert!(
            graph.notes.iter().any(|n| n == NO_SCOPE),
            "{:?}",
            graph.notes
        );
        assert!(graph.objects.len() > 1 && !graph.edges.is_empty());
        let r = dependents_in(
            &mut f,
            &DependencyTarget {
                object: ObjectRef {
                    kind: TABLE.into(),
                    schema: None,
                    name: "Sales".into(),
                },
                column: None,
            },
        )
        .await
        .unwrap();
        assert!(r.external_users.is_empty());
        assert_eq!(r.note.as_deref(), Some(NO_SCOPE));
    }

    #[tokio::test]
    async fn my_workspace_uses_the_calls_without_a_group() {
        let mut f = Fake::new();
        let id = {
            let m = f.model.as_mut().unwrap();
            m.workspace.id = None;
            m.workspace.name = "Mi área de trabajo".into();
            m.id.clone()
        };
        f.route(
            crate::http::Method::Get,
            "/reports",
            200,
            json!({"value": [{"id": "r1", "name": "R", "datasetId": id}]}),
        );
        f.route(
            crate::http::Method::Get,
            "/dashboards",
            200,
            json!({"value": []}),
        );
        let model = f.model.clone().unwrap();
        let c = read(&mut f, &model).await.unwrap();
        assert_eq!(who(&c.items), [(REPORT.into(), "R".into(), None)]);
        assert!(c.notes.is_empty());
        assert!(f.requests().iter().all(|r| !r.path.contains("/groups/")));
    }

    #[tokio::test]
    async fn a_big_workspace_is_capped_with_a_note() {
        let mut f = Fake::new();
        let id = f.model.as_ref().unwrap().id.clone();
        let reports: Vec<_> = (0..MAX_ITEMS + 20)
            .map(|i| json!({"id": format!("r{i}"), "name": format!("R{i}"), "datasetId": id}))
            .collect();
        let dashboards: Vec<_> = (0..MAX_DASHBOARDS_READ + 5)
            .map(|i| json!({"id": format!("d{i}"), "displayName": format!("D{i}")}))
            .collect();
        f.consumers(&reports, &dashboards, &[]);
        let model = f.model.clone().unwrap();
        let c = read(&mut f, &model).await.unwrap();
        assert_eq!(c.items.len(), MAX_ITEMS);
        assert!(c.notes.contains(&TOO_MANY.to_string()));
        assert!(c.notes.contains(&TILES_CAPPED.to_string()));
        // Tiles of 100 dashboards, no more (the fake has no tile routes, so
        // each read fails: the note says so too).
        let tiles = f
            .requests()
            .iter()
            .filter(|r| r.path.ends_with("/tiles"))
            .count();
        assert_eq!(tiles, MAX_DASHBOARDS_READ);
    }

    #[tokio::test]
    async fn the_readers_are_kept_for_a_minute() {
        let mut f = workspace();
        let model = f.model.clone().unwrap();
        read(&mut f, &model).await.unwrap();
        let n = f.requests().len();
        read(&mut f, &model).await.unwrap();
        assert_eq!(f.requests().len(), n);
        let _ = GROUP;
    }
}
