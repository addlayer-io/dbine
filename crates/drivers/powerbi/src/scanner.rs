//! Where every model of a tenant reads from, without opening a single model: Power BI's admin scanner API.
//!
//! `POST admin/workspaces/getInfo?datasourceDetails=true` (up to 100 workspaces per scan) starts a scan,
//! `GET admin/workspaces/scanStatus/{id}` says when it is done and `GET admin/workspaces/scanResult/{id}`
//! returns each workspace's datasets with their data sources (`connectionDetails`: server, database, url,
//! path). All of it is read-only; the POST only asks the service to prepare a result, so it is allowed on
//! read-only connections ([`crate::http`] counts it among the reads that are POSTs).
//!
//! It needs the tenant admin right (Power BI administrator, or a service principal allowed by the tenant
//! setting) and a token with `Tenant.Read.All`. The sign-in asks for no new scope for it: a user without
//! the right, or whose token lacks the scope, gets a 401 or 403, and the answer is
//! [`Error::Unsupported`] so the caller reads the models one by one, as before.
//!
//! The scanner names the server and the database of each data source, not the tables a model reads, so it
//! is a pre-filter: a model can only read an object of a connection if one of its data sources points to
//! that connection's server and database. The result is a [`LineageGraph`]: its `objects` are the models
//! the scan covers and its `external` the data sources each names.

use crate::catalog::Model;
use crate::http::{Method, Priority};
use crate::meta::service::Service;
use crate::MODEL;
use dbine_driver::dependencies::{ExternalRef, LineageExternal, LineageGraph};
use dbine_driver::{Error, ObjectRef, Result};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

/// Workspaces per scan (the service's limit).
pub const BATCH: usize = 100;
/// Scans running at once (the service allows 16 per tenant).
const AT_ONCE: usize = 8;
const POLL: Duration = Duration::from_secs(3);
/// The longest one scan is waited for.
const WAIT: Duration = Duration::from_secs(10 * 60);

/// What the caller is told when the user can't use the admin API (and nothing else is said).
pub const NOT_AVAILABLE: &str =
    "la API de administración de Power BI no está disponible para este usuario";

/// Whether an error means "this user can't call the admin API" (a 401 or a 403) and not a failure.
pub(crate) fn is_denied(e: &Error) -> bool {
    match e {
        Error::AuthFailed(_) => true,
        Error::Query(m) => m.contains("HTTP 403") || m.contains("respondió 403"),
        _ => false,
    }
}

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The system name the lineage uses for a scanner data source type, for the engines whose sources can
/// match a saved connection. `None` for the rest (files, the web, analysis services…): the source is kept
/// but can't be decided on, so the model that has it is always read.
pub(crate) fn system_of(kind: &str, connection_kind: Option<&str>) -> Option<&'static str> {
    let k = kind.trim().to_lowercase();
    let ck = connection_kind.unwrap_or("").to_lowercase();
    match k.as_str() {
        "sql" => Some("SQL Server"),
        "postgresql" => Some("PostgreSQL"),
        "mysql" => Some("MySQL"),
        "oracle" => Some("Oracle"),
        "snowflake" => Some("Snowflake"),
        "databricks" => Some("Databricks"),
        _ if ck.contains("snowflake") => Some("Snowflake"),
        _ if ck.contains("databricks") => Some("Databricks"),
        _ => None,
    }
}

/// One scanner data source as the lineage's external reference.
fn source_of(ds: &Value, instances: &HashMap<String, Value>) -> Option<ExternalRef> {
    let mut d = ds;
    // A data source that only names its instance: the instance has the details.
    if d.get("connectionDetails").is_none_or(Value::is_null) {
        if let Some(i) = text(ds, "datasourceId").and_then(|id| instances.get(&id.to_lowercase())) {
            d = i;
        }
    }
    let kind = text(d, "datasourceType").or_else(|| text(ds, "datasourceType"))?;
    let c = d.get("connectionDetails").unwrap_or(&Value::Null);
    let connection_kind = text(c, "kind");
    let url = text(c, "url");
    let path = text(c, "path");
    Some(ExternalRef {
        system: Some(
            system_of(&kind, connection_kind.as_deref())
                .map(str::to_string)
                .unwrap_or(kind),
        ),
        server: text(c, "server"),
        database: text(c, "database"),
        detail: url.or(path),
        ..Default::default()
    })
}

/// The models of a scan result and their data sources: `(dataset id, sources)`, every dataset once.
fn datasets_of(result: &Value) -> Vec<(String, Vec<ExternalRef>)> {
    let instances: HashMap<String, Value> = result
        .get("datasourceInstances")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|i| Some((text(i, "datasourceId")?.to_lowercase(), i.clone())))
                .collect()
        })
        .unwrap_or_default();
    let mut out = Vec::new();
    for w in result
        .get("workspaces")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        for d in w
            .get("datasets")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let Some(id) = text(d, "id") else { continue };
            let mut sources: Vec<ExternalRef> = Vec::new();
            for ds in d
                .get("datasources")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or_default()
            {
                if let Some(s) = source_of(ds, &instances) {
                    if !sources.contains(&s) {
                        sources.push(s);
                    }
                }
            }
            out.push((id, sources));
        }
    }
    out
}

/// The graph of a scan result for the models of `models` (the listing): a node per model the scan
/// covers and a source per data source. Datasets of the scan that aren't listed are left out.
pub(crate) fn graph_of(results: &[Value], models: &[Model]) -> LineageGraph {
    let by_id: HashMap<String, &Model> = models.iter().map(|m| (m.id.to_lowercase(), m)).collect();
    let mut g = LineageGraph::default();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for (id, sources) in results.iter().flat_map(datasets_of) {
        let Some(m) = by_id.get(&id.to_lowercase()) else {
            continue;
        };
        if !seen.insert(m.listed_as.clone()) {
            continue;
        }
        let node = ObjectRef {
            kind: MODEL.into(),
            schema: None,
            name: m.listed_as.clone(),
        };
        g.external
            .extend(sources.into_iter().map(|source| LineageExternal {
                source,
                to: node.clone(),
            }));
        g.objects.push(node);
    }
    g
}

/// The distinct workspace ids of `models`, in batches of [`BATCH`] (a model in "My workspace" has none).
pub(crate) fn batches(models: &[Model]) -> Vec<Vec<String>> {
    let ids: Vec<String> = models
        .iter()
        .filter_map(|m| m.workspace.id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    ids.chunks(BATCH).map(<[String]>::to_vec).collect()
}

/// Pacing of the polls: tests shorten it.
#[derive(Clone, Copy)]
pub(crate) struct Pace {
    pub poll: Duration,
    pub wait: Duration,
}

impl Default for Pace {
    fn default() -> Self {
        Pace {
            poll: POLL,
            wait: WAIT,
        }
    }
}

/// Run one wave of scans: start each, wait for them and return their results.
async fn scan_wave<S: Service>(s: &S, wave: &[Vec<String>], pace: Pace) -> Result<Vec<Value>> {
    let mut ids = Vec::new();
    for ws in wave {
        let started = s
            .send(
                Method::Post,
                "/admin/workspaces/getInfo?datasourceDetails=true",
                Some(&json!({ "workspaces": ws })),
                Priority::Background,
            )
            .await?;
        let id = text(&started.body, "id")
            .ok_or_else(|| Error::Query("Power BI no devolvió el id del escaneo".into()))?;
        ids.push(id);
    }
    let mut out = Vec::new();
    for id in ids {
        let began = Instant::now();
        loop {
            let status = s
                .get(
                    &format!("/admin/workspaces/scanStatus/{id}"),
                    Priority::Background,
                )
                .await?;
            match text(&status, "status").as_deref() {
                Some("Succeeded") => break,
                Some("Failed") => {
                    return Err(Error::Query("el escaneo de Power BI falló".into()));
                }
                _ => {}
            }
            if began.elapsed() >= pace.wait {
                return Err(Error::Query(
                    "el escaneo de Power BI tardó demasiado".into(),
                ));
            }
            tokio::time::sleep(pace.poll).await;
        }
        out.push(
            s.get(
                &format!("/admin/workspaces/scanResult/{id}"),
                Priority::Background,
            )
            .await?,
        );
    }
    Ok(out)
}

/// Where the listed models read from, by the admin scanner API ([`LineageScope::tenant_sources`]).
///
/// [`LineageScope::tenant_sources`]: dbine_driver::dependencies::LineageScope::tenant_sources
pub(crate) async fn tenant_sources<S: Service>(s: &mut S, pace: Pace) -> Result<LineageGraph> {
    let models = s.workspace_models().await?;
    let batches = batches(&models);
    if batches.is_empty() {
        return Ok(LineageGraph::default());
    }
    let mut results = Vec::new();
    for wave in batches.chunks(AT_ONCE) {
        match scan_wave(&*s, wave, pace).await {
            Ok(mut r) => results.append(&mut r),
            Err(e) if is_denied(&e) => return Err(Error::Unsupported(NOT_AVAILABLE.into())),
            Err(e) => return Err(e),
        }
    }
    Ok(graph_of(&results, &models))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Workspace;
    use crate::meta::fake::Fake;

    fn model(id: &str, listed: &str, workspace: Option<&str>) -> Model {
        Model {
            id: id.into(),
            name: listed.into(),
            listed_as: listed.into(),
            workspace: Workspace {
                id: workspace.map(str::to_string),
                name: "W".into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn fast() -> Pace {
        Pace {
            poll: Duration::from_millis(1),
            wait: Duration::from_millis(50),
        }
    }

    fn result() -> Value {
        json!({
            "workspaces": [{"id": "w1", "datasets": [
                {"id": "D1", "name": "Ventas", "datasources": [
                    {"datasourceType": "Sql", "connectionDetails": {"server": "srv1", "database": "dwh"}},
                    {"datasourceType": "Sql", "connectionDetails": {"server": "srv1", "database": "dwh"}},
                    {"datasourceType": "Excel", "connectionDetails": {"path": "C:\\x.xlsx"}}]},
                {"id": "d2", "name": "Vacío", "datasources": []},
                {"id": "d3", "name": "Instancia", "datasources": [{"datasourceId": "I1"}]},
                {"id": "d4", "name": "NoListado", "datasources": [
                    {"datasourceType": "Sql", "connectionDetails": {"server": "x", "database": "y"}}]}
            ]}],
            "datasourceInstances": [
                {"datasourceId": "i1", "datasourceType": "Extension",
                 "connectionDetails": {"server": "acme.snowflakecomputing.com", "database": "DB1", "kind": "Snowflake"}}
            ]
        })
    }

    #[test]
    fn the_types_the_lineage_can_match_get_its_system_names() {
        assert_eq!(system_of("Sql", None), Some("SQL Server"));
        assert_eq!(system_of("PostgreSql", None), Some("PostgreSQL"));
        assert_eq!(system_of("MySql", None), Some("MySQL"));
        assert_eq!(system_of("Oracle", None), Some("Oracle"));
        assert_eq!(system_of("Extension", Some("Snowflake")), Some("Snowflake"));
        assert_eq!(
            system_of("Extension", Some("DatabricksMultiCloud")),
            Some("Databricks")
        );
        assert_eq!(system_of("File", None), None);
        assert_eq!(system_of("AnalysisServices", None), None);
    }

    #[test]
    fn the_result_becomes_one_node_per_listed_model_and_its_distinct_sources() {
        let models = [
            model("d1", "Ventas", Some("w1")),
            model("d2", "Vacío", Some("w1")),
            model("d3", "Instancia", Some("w1")),
        ];
        let g = graph_of(&[result()], &models);
        let names: Vec<&str> = g.objects.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["Ventas", "Vacío", "Instancia"]);
        let of = |m: &str| -> Vec<&ExternalRef> {
            g.external
                .iter()
                .filter(|e| e.to.name == m)
                .map(|e| &e.source)
                .collect()
        };
        let v = of("Ventas");
        // The repeated source counts once; the file is kept, with no system the lineage can match.
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].system.as_deref(), Some("SQL Server"));
        assert_eq!(v[0].server.as_deref(), Some("srv1"));
        assert_eq!(v[0].database.as_deref(), Some("dwh"));
        assert_eq!(v[1].system.as_deref(), Some("Excel"));
        assert_eq!(v[1].detail.as_deref(), Some("C:\\x.xlsx"));
        // A model with no data source is still covered.
        assert!(of("Vacío").is_empty());
        // The instance's details complete a data source that only names it.
        let i = of("Instancia");
        assert_eq!(i[0].system.as_deref(), Some("Snowflake"));
        assert_eq!(i[0].database.as_deref(), Some("DB1"));
    }

    #[test]
    fn the_workspaces_go_in_batches_of_a_hundred_without_repeats() {
        let models: Vec<Model> = (0..250)
            .map(|i| {
                model(
                    &format!("d{i}"),
                    &format!("m{i}"),
                    Some(&format!("w{:03}", i / 2)),
                )
            })
            .chain([model("x", "mine", None)])
            .collect();
        let b = batches(&models);
        assert_eq!(b.iter().map(Vec::len).collect::<Vec<_>>(), [100, 25]);
        assert!(batches(&[model("x", "mine", None)]).is_empty());
    }

    fn scan_routes(f: &Fake, scan: &str, status_bodies: Vec<Value>, res: Value) {
        f.route(
            Method::Post,
            "/admin/workspaces/getInfo?datasourceDetails=true",
            202,
            json!({"id": scan}),
        );
        for b in status_bodies {
            f.push(crate::meta::fake::Route {
                method: Method::Get,
                path: format!("/admin/workspaces/scanStatus/{scan}"),
                fabric: false,
                status: 200,
                body: b,
                location: None,
                retry_after: None,
                times: Some(1),
            });
        }
        f.route(
            Method::Get,
            &format!("/admin/workspaces/scanResult/{scan}"),
            200,
            res,
        );
    }

    fn fake_with_models() -> Fake {
        let mut f = Fake::new();
        f.models = vec![
            model("d1", "Ventas", Some("w1")),
            model("d2", "Vacío", Some("w1")),
        ];
        f
    }

    #[tokio::test]
    async fn a_scan_is_started_polled_and_read_without_writing_anything() {
        let mut f = fake_with_models();
        scan_routes(
            &f,
            "s1",
            vec![json!({"status": "Running"}), json!({"status": "Succeeded"})],
            result(),
        );
        let g = tenant_sources(&mut f, fast()).await.unwrap();
        assert_eq!(g.objects.len(), 2);
        assert_eq!(g.external.len(), 2);
        let reqs = f.requests();
        let paths: Vec<&str> = reqs.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(
            paths,
            [
                "/admin/workspaces/getInfo?datasourceDetails=true",
                "/admin/workspaces/scanStatus/s1",
                "/admin/workspaces/scanStatus/s1",
                "/admin/workspaces/scanResult/s1",
            ]
        );
        // The only POST is the scan's own request, with the workspaces asked.
        assert_eq!(reqs[0].body, Some(json!({"workspaces": ["w1"]})));
        assert_eq!(f.writes().len(), 1);
    }

    #[tokio::test]
    async fn a_user_without_the_admin_right_gets_unsupported_not_a_failure() {
        for status in [401, 403] {
            let mut f = fake_with_models();
            f.route(
                Method::Post,
                "/admin/workspaces/getInfo?datasourceDetails=true",
                status,
                json!({"error": {"code": "PowerBINotAuthorizedException"}}),
            );
            let e = tenant_sources(&mut f, fast()).await.unwrap_err();
            assert!(
                matches!(&e, Error::Unsupported(m) if m == NOT_AVAILABLE),
                "{e:?}"
            );
        }
    }

    #[tokio::test]
    async fn other_failures_are_not_taken_for_a_missing_right() {
        let mut f = fake_with_models();
        f.route(
            Method::Post,
            "/admin/workspaces/getInfo?datasourceDetails=true",
            500,
            json!({}),
        );
        let e = tenant_sources(&mut f, fast()).await.unwrap_err();
        assert!(!matches!(e, Error::Unsupported(_)), "{e:?}");
        // A scan that fails or never ends is an error too.
        let mut f = fake_with_models();
        scan_routes(&f, "s2", vec![json!({"status": "Failed"})], result());
        assert!(tenant_sources(&mut f, fast()).await.is_err());
        let mut f = fake_with_models();
        f.route(
            Method::Post,
            "/admin/workspaces/getInfo?datasourceDetails=true",
            202,
            json!({"id": "s3"}),
        );
        f.route(
            Method::Get,
            "/admin/workspaces/scanStatus/s3",
            200,
            json!({"status": "Running"}),
        );
        assert!(tenant_sources(&mut f, fast()).await.is_err());
    }

    #[tokio::test]
    async fn models_outside_any_workspace_ask_nothing() {
        let mut f = Fake::new();
        f.models = vec![model("d1", "Mine", None)];
        let g = tenant_sources(&mut f, fast()).await.unwrap();
        assert!(g.objects.is_empty() && f.requests().is_empty());
    }
}
