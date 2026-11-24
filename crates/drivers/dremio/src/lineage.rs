//! What feeds a table or view and what reads it (`Session::references` and
//! `Session::dependents`), from Dremio's own lineage: the catalog API's
//! `GET /api/v3/catalog/{id}/graph` (`parents` and `children` of one dataset),
//! asked for that one object, never a scan of every view's definition. The id
//! comes from `GET /api/v3/catalog/by-path/…`. Both go through the session's
//! own REST login, the one every other call uses.
//!
//! Dremio documents the graph call in the Enterprise and Cloud editions; the
//! open-source one may answer 404, and a login without access to the dataset
//! gets a refusal. In either case (and for a column, a function or any kind
//! but a table or view) the contract's scan runs instead, with a note.
//!
//! A parent or child in another space or source than the target's is
//! outside the database (DBine's database is a top-level container): it is
//! an [`ExternalRef`], not an item.

use crate::{encode, Conn, DremioSession};
use dbine_driver::dependencies::{
    references_scan, scan, Confidence, DependencyReport, DependencyScan, DependencyTarget,
    Dependent, ExternalRef, Relation,
};
use dbine_driver::{kinds, Error, ObjectRef, Result};
use serde_json::Value;

/// The graph isn't available for this object.
pub(crate) const NO_LINEAGE: &str = "El linaje de Dremio no está disponible para este objeto (la API de linaje es de Dremio Enterprise y Cloud, y hace falta acceso al dataset): lo que sale es de las definiciones, sin confirmar.";

/// A dataset of the graph, by its path (`["space", "folder", "view"]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Node {
    pub path: Vec<String>,
    /// `VIRTUAL` (a view); otherwise a table (`PROMOTED`).
    pub view: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Graph {
    /// What the dataset reads.
    pub parents: Vec<Node>,
    /// What reads the dataset.
    pub children: Vec<Node>,
}

/// `GET` path of a dataset in the catalog: `schema` is Dremio's dotted
/// `space.folder` (the one `list_objects` reports).
pub(crate) fn by_path(schema: &str, name: &str) -> String {
    let parts: Vec<String> = schema
        .split('.')
        .filter(|p| !p.is_empty())
        .chain(std::iter::once(name))
        .map(encode)
        .collect();
    format!("/api/v3/catalog/by-path/{}", parts.join("/"))
}

pub(crate) fn graph_path(id: &str) -> String {
    format!("/api/v3/catalog/{}/graph", encode(id))
}

fn nodes(v: &Value, key: &str) -> Vec<Node> {
    let mut out: Vec<Node> = Vec::new();
    for e in v.get(key).and_then(Value::as_array).into_iter().flatten() {
        let path: Vec<String> = e
            .get("path")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|p| p.as_str().map(str::to_string))
            .collect();
        // A dataset has at least a container and a name.
        if path.len() < 2 {
            continue;
        }
        let n = Node {
            path,
            view: e.get("datasetType").and_then(Value::as_str) == Some("VIRTUAL"),
        };
        if !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

/// The `parents` and `children` of a graph answer.
pub(crate) fn parse(resp: &Value) -> Graph {
    Graph {
        parents: nodes(resp, "parents"),
        children: nodes(resp, "children"),
    }
}

/// The graph of one dataset: its id by path, then its graph.
pub(crate) async fn read(conn: &Conn, schema: &str, name: &str) -> Result<Graph> {
    let entry = conn
        .send(reqwest::Method::GET, &by_path(schema, name), None)
        .await?;
    let id = entry
        .get("id")
        .and_then(Value::as_str)
        .filter(|i| !i.is_empty())
        .ok_or_else(|| Error::Query(format!("{schema}.{name} has no id in the catalog")))?;
    let resp = conn
        .send(reqwest::Method::GET, &graph_path(id), None)
        .await?;
    Ok(parse(&resp))
}

/// The nodes as items (same top-level container as `top`) or sources outside it.
fn fill(nodes: &[Node], top: &str, items: &mut Vec<Dependent>, external: &mut Vec<ExternalRef>) {
    for n in nodes {
        let (name, container) = match n.path.split_last() {
            Some(p) => p,
            None => continue,
        };
        if container
            .first()
            .is_some_and(|c| c.eq_ignore_ascii_case(top))
        {
            items.push(Dependent {
                kind: if n.view { kinds::VIEW } else { kinds::TABLE }.into(),
                schema: Some(container.join(".")),
                name: name.clone(),
                parent: None,
                relation: Relation::Code,
                confidence: Confidence::Confirmed,
                detail: None,
                mentions: Vec::new(),
            });
        } else {
            external.push(ExternalRef {
                system: Some("Dremio".into()),
                database: container.first().cloned(),
                schema: Some(container[1..].join(".")).filter(|s| !s.is_empty()),
                name: Some(name.clone()),
                detail: Some(if n.view { "view" } else { "table" }.into()),
                ..Default::default()
            });
        }
    }
    items.sort_by(|a, b| {
        (a.schema.as_deref(), a.name.to_lowercase())
            .cmp(&(b.schema.as_deref(), b.name.to_lowercase()))
    });
    external
        .sort_by(|a, b| (&a.database, &a.schema, &a.name).cmp(&(&b.database, &b.schema, &b.name)));
}

/// What the graph says reads the target: views of its container are items,
/// the ones elsewhere are `external_users`.
pub(crate) fn dependents_report(g: &Graph, top: &str) -> DependencyReport {
    let mut r = DependencyReport::default();
    fill(&g.children, top, &mut r.items, &mut r.external_users);
    r
}

/// What the graph says the target reads: items of its container, `external`
/// for the rest.
pub(crate) fn references_report(g: &Graph, top: &str) -> DependencyReport {
    let mut r = DependencyReport::default();
    fill(&g.parents, top, &mut r.items, &mut r.external);
    r
}

/// A table or view itself, not one of its columns.
fn is_dataset(t: &DependencyTarget) -> bool {
    t.column.is_none() && matches!(t.object.kind.as_str(), kinds::TABLE | kinds::VIEW)
}

impl DremioSession {
    /// The graph of `obj`, through the session's REST login.
    async fn graph_of(&self, obj: &ObjectRef) -> Result<(Graph, String)> {
        let schema = self.schema_of(obj)?;
        let top = schema.split('.').next().unwrap_or_default().to_string();
        let g = self
            .cancel
            .run(read(&self.conn, &schema, &obj.name))
            .await?;
        Ok((g, top))
    }

    /// What reads a table or view: Dremio's graph for that one dataset
    /// (Confirmed). Without it, the contract's scan, with a note.
    pub(crate) async fn dependents_native(
        &mut self,
        target: &DependencyTarget,
        s: &DependencyScan,
    ) -> Result<DependencyReport> {
        if !is_dataset(target) {
            return scan(self, target, s).await;
        }
        match self.graph_of(&target.object).await {
            Ok((g, top)) => Ok(dependents_report(&g, &top)),
            Err(e) => {
                tracing::debug!("dremio graph of {}: {e}", target.object.name);
                let mut r = scan(self, target, s).await?;
                r.note = Some(NO_LINEAGE.into());
                Ok(r)
            }
        }
    }

    /// What a view reads: Dremio's graph (Confirmed). A table reads nothing
    /// here, so it and anything else take the contract's way; so does a view
    /// whose graph isn't available, with a note.
    pub(crate) async fn references_native(
        &mut self,
        target: &DependencyTarget,
        s: &DependencyScan,
    ) -> Result<DependencyReport> {
        if !is_dataset(target) || target.object.kind != kinds::VIEW {
            return references_scan(self, target, s).await;
        }
        match self.graph_of(&target.object).await {
            Ok((g, top)) => Ok(references_report(&g, &top)),
            Err(e) => {
                tracing::debug!("dremio graph of {}: {e}", target.object.name);
                let mut r = references_scan(self, target, s).await?;
                r.note = Some(NO_LINEAGE.into());
                Ok(r)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn answer() -> Value {
        json!({
            "sources": [{"id": "s", "path": ["Samples"], "type": "CONTAINER", "containerType": "SOURCE"}],
            "parents": [
                {"id": "1", "path": ["Samples", "samples.dremio.com", "zips.json"], "type": "DATASET", "datasetType": "PROMOTED"},
                {"id": "2", "path": ["mart", "dim", "city"], "type": "DATASET", "datasetType": "VIRTUAL"},
                {"id": "2", "path": ["mart", "dim", "city"], "type": "DATASET", "datasetType": "VIRTUAL"},
                {"id": "x", "path": ["lonely"], "type": "DATASET"}
            ],
            "children": [
                {"id": "3", "path": ["@dremio", "NYC_zip"], "type": "DATASET", "datasetType": "VIRTUAL"},
                {"id": "4", "path": ["mart", "rep", "Zips"], "type": "DATASET", "datasetType": "VIRTUAL"}
            ]
        })
    }

    #[test]
    fn paths_are_encoded_per_segment() {
        assert_eq!(
            by_path("mart.dim folder", "a/b"),
            "/api/v3/catalog/by-path/mart/dim%20folder/a%2Fb"
        );
        assert_eq!(by_path("mart", "v"), "/api/v3/catalog/by-path/mart/v");
        assert_eq!(graph_path("d6-9b"), "/api/v3/catalog/d6-9b/graph");
    }

    #[test]
    fn the_graph_is_parsed_once_per_dataset() {
        let g = parse(&answer());
        assert_eq!(g.parents.len(), 2, "duplicates and bare paths are dropped");
        assert!(!g.parents[0].view && g.parents[1].view);
        assert_eq!(g.children.len(), 2);
        assert_eq!(parse(&json!({})), Graph::default());
        assert_eq!(parse(&Value::Null), Graph::default());
    }

    #[test]
    fn dependents_split_by_container() {
        let r = dependents_report(&parse(&answer()), "mart");
        assert_eq!(r.items.len(), 1);
        let d = &r.items[0];
        assert_eq!(
            (d.kind.as_str(), d.schema.as_deref(), d.name.as_str()),
            ("view", Some("mart.rep"), "Zips")
        );
        assert_eq!(d.confidence, Confidence::Confirmed);
        assert_eq!(d.relation, Relation::Code);
        assert_eq!(r.external_users.len(), 1);
        let x = &r.external_users[0];
        assert_eq!(x.database.as_deref(), Some("@dremio"));
        assert_eq!(x.name.as_deref(), Some("NYC_zip"));
        assert!(r.external.is_empty());
    }

    #[test]
    fn references_keep_a_source_table_outside() {
        let r = references_report(&parse(&answer()), "MART");
        assert_eq!(r.items.len(), 1, "the container matches without case");
        assert_eq!(r.items[0].name, "city");
        assert_eq!(r.external.len(), 1);
        let x = &r.external[0];
        assert_eq!(x.database.as_deref(), Some("Samples"));
        assert_eq!(x.schema.as_deref(), Some("samples.dremio.com"));
        assert_eq!(x.name.as_deref(), Some("zips.json"));
        assert_eq!(x.detail.as_deref(), Some("table"));
    }

    /// A one-shot fake of Dremio's REST API: answers each request with the
    /// next `(status, body)` and records the request lines.
    async fn fake(replies: Vec<(u16, String)>) -> (String, std::sync::Arc<Mutex<Vec<String>>>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            for (status, body) in replies {
                let (mut s, _) = l.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let n = s.read(&mut buf).await.unwrap();
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                log.lock().unwrap().push(
                    text.lines().next().unwrap_or_default().to_string()
                        + if text
                            .to_ascii_lowercase()
                            .contains("authorization: bearer pat")
                        {
                            " [auth]"
                        } else {
                            ""
                        },
                );
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                s.write_all(resp.as_bytes()).await.unwrap();
            }
        });
        (base, seen)
    }

    fn conn(base: String) -> Conn {
        Conn {
            http: reqwest::Client::builder().no_proxy().build().unwrap(),
            base,
            user: String::new(),
            password: String::new(),
            pat: Some("pat".into()),
            auth: Mutex::new(None),
        }
    }

    #[tokio::test]
    async fn one_object_is_two_calls_with_the_session_login() {
        let (base, seen) = fake(vec![
            (
                200,
                json!({"id": "abc-1", "path": ["mart", "v"]}).to_string(),
            ),
            (200, answer().to_string()),
        ])
        .await;
        let c = conn(base);
        c.login().await.unwrap();
        let g = read(&c, "mart", "v").await.unwrap();
        assert_eq!((g.parents.len(), g.children.len()), (2, 2));
        let lines = seen.lock().unwrap().clone();
        assert_eq!(lines.len(), 2, "no scan of definitions: just id and graph");
        assert!(
            lines[0].starts_with("GET /api/v3/catalog/by-path/mart/v ")
                && lines[0].ends_with("[auth]")
        );
        assert!(lines[1].starts_with("GET /api/v3/catalog/abc-1/graph "));
    }

    #[tokio::test]
    async fn a_server_without_the_graph_is_an_error_to_fall_back_from() {
        let (base, _) = fake(vec![
            (200, json!({"id": "abc-1"}).to_string()),
            (404, json!({"errorMessage": "Not Found"}).to_string()),
        ])
        .await;
        let c = conn(base);
        assert!(read(&c, "mart", "v").await.is_err());
        let (base, _) = fake(vec![(200, json!({"path": ["x"]}).to_string())]).await;
        assert!(read(&conn(base), "mart", "v").await.is_err(), "no id");
    }
}
