//! The explorer's tree (catalogs, schemas, tables, views and functions)
//! from Unity Catalog's REST API (`/api/2.1/unity-catalog/…`): nothing runs
//! on the SQL warehouse, so a stopped one isn't woken up or waited for, and
//! a big catalog isn't read through `information_schema`.
//!
//! The legacy `hive_metastore`, foreign catalogs (Lakehouse Federation, whose
//! tables Unity Catalog only learns from the remote database) and any API
//! error keep the SQL path (`sql_*`), as before.
//!
//! Connecting doesn't wait for a stopped warehouse either
//! ([`DatabricksSession::check_access`]): its `SELECT 1` only goes in, which
//! still checks the login and the access to the warehouse, and starts it.

use crate::health::escape;
use crate::stats::kind_of;
use crate::{Api, DatabricksSession};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{kinds, DbObject, Error, Result, SchemaInfo};
use serde_json::{json, Value as Json};
use std::time::Duration;

/// The legacy per-workspace metastore: not in Unity Catalog's lists.
pub(crate) const HIVE_METASTORE: &str = "hive_metastore";
/// Function lists (one per schema) asked for at the same time.
const FUNCTION_LISTS: usize = 8;
/// The system schema every Unity Catalog catalog has.
const INFORMATION_SCHEMA: &str = "information_schema";

/// Every page of a Unity Catalog list (`key`: the array in each page), up
/// to about `max` items. A page may come empty with a next token: only its
/// absence ends the list.
pub(crate) async fn uc_pages(api: &Api, path: &str, key: &str, max: usize) -> Result<Vec<Json>> {
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let sep = if path.contains('?') { '&' } else { '?' };
        let url = match &token {
            Some(t) => format!("{path}{sep}page_token={}", escape(t)),
            None => path.to_string(),
        };
        let resp = api.get(&url).await?;
        if let Some(items) = resp.get(key).and_then(Json::as_array) {
            out.extend(items.iter().cloned());
        }
        match resp.get("next_page_token").and_then(Json::as_str) {
            Some(t) if !t.is_empty() && out.len() < max => token = Some(t.to_string()),
            _ => break,
        }
    }
    Ok(out)
}

/// A table summary (`full_name` `catalog.schema.table`, `table_type`) as
/// the explorer's object; Unity Catalog names have no dots.
fn summary_object(t: &Json) -> Option<DbObject> {
    let mut parts = t.get("full_name")?.as_str()?.splitn(3, '.');
    let (_, schema, name) = (parts.next()?, parts.next()?, parts.next()?);
    Some(DbObject {
        kind: kind_of(t.get("table_type").and_then(Json::as_str).unwrap_or("")).into(),
        schema: Some(schema.to_string()),
        name: name.to_string(),
        parent: None,
    })
}

fn by_schema_and_name(a: &DbObject, b: &DbObject) -> std::cmp::Ordering {
    (&a.schema, &a.name).cmp(&(&b.schema, &b.name))
}

fn names(items: &[Json]) -> Vec<String> {
    items
        .iter()
        .filter_map(|c| c.get("name")?.as_str().map(str::to_string))
        .collect()
}

impl DatabricksSession {
    /// Whether the SQL warehouse is running; `None` when the API doesn't
    /// say (no permission to view it, an old workspace…).
    pub(crate) async fn warehouse_running(&self) -> Option<bool> {
        let w = self
            .api
            .get(&format!(
                "/api/2.0/sql/warehouses/{}",
                escape(&self.warehouse)
            ))
            .await
            .ok()?;
        Some(w.get("state")?.as_str()? == "RUNNING")
    }

    /// The connection's check: a `SELECT 1` on the warehouse. A running
    /// warehouse (or one whose state the API doesn't give) answers it as
    /// before, which also checks the catalog. A stopped or starting one
    /// isn't waited for: the statement only goes in (`wait_timeout` 0s),
    /// so the login and the access to the warehouse are still checked, and
    /// it starts the warehouse while the explorer reads Unity Catalog.
    pub(crate) async fn check_access(&self) -> Result<()> {
        let as_connect = |e| match e {
            Error::Query(m) => Error::Connect(m),
            other => other,
        };
        if self.warehouse_running().await == Some(false) {
            let mut body = self.request("SELECT 1", None);
            body["wait_timeout"] = json!("0s");
            if let Some(b) = body.as_object_mut() {
                b.remove("on_wait_timeout");
            }
            let resp = self
                .api
                .post("/api/2.0/sql/statements", &body)
                .await
                .map_err(as_connect)?;
            if resp.pointer("/status/state").and_then(Json::as_str) == Some("FAILED") {
                let msg = resp
                    .pointer("/status/error/message")
                    .and_then(Json::as_str)
                    .unwrap_or("la sentencia falló");
                return Err(Error::Connect(msg.to_string()));
            }
            return Ok(());
        }
        tokio::time::timeout(Duration::from_secs(300), self.run("SELECT 1", 1, None))
            .await
            .map_err(|_| {
                Error::Connect("tiempo de espera agotado (¿el warehouse está iniciando?)".into())
            })?
            .map_err(as_connect)?;
        Ok(())
    }

    /// The catalogs. A running warehouse answers `SHOW CATALOGS` at once,
    /// exactly as the editor sees them; a stopped one isn't woken up for
    /// it: they come from Unity Catalog, plus the legacy `hive_metastore`
    /// its list leaves out.
    pub(crate) async fn catalogs(&self) -> Result<Vec<String>> {
        if self.warehouse_running().await == Some(false) {
            match uc_pages(
                &self.api,
                "/api/2.1/unity-catalog/catalogs?max_results=0",
                "catalogs",
                usize::MAX,
            )
            .await
            {
                Ok(items) => {
                    let mut out = names(&items);
                    if !out.iter().any(|n| n.eq_ignore_ascii_case(HIVE_METASTORE)) {
                        out.push(HIVE_METASTORE.into());
                    }
                    out.sort();
                    return Ok(out);
                }
                Err(e) => {
                    tracing::warn!("Unity Catalog catalog list failed, using SHOW CATALOGS: {e}")
                }
            }
        }
        self.sql_catalogs().await
    }

    /// Whether Unity Catalog lists `cat`'s tables itself: not the legacy
    /// metastore, nor a foreign catalog.
    async fn uc_lists(&self, cat: &str) -> Result<bool> {
        if cat.eq_ignore_ascii_case(HIVE_METASTORE) {
            return Ok(false);
        }
        let c = self
            .api
            .get(&format!("/api/2.1/unity-catalog/catalogs/{}", escape(cat)))
            .await?;
        Ok(c.get("catalog_type").and_then(Json::as_str) != Some("FOREIGN_CATALOG"))
    }

    async fn uc_schema_names(&self, cat: &str) -> Result<Vec<String>> {
        let items = uc_pages(
            &self.api,
            &format!(
                "/api/2.1/unity-catalog/schemas?catalog_name={}&max_results=0",
                escape(cat)
            ),
            "schemas",
            usize::MAX,
        )
        .await?;
        let mut out = names(&items);
        out.sort();
        Ok(out)
    }

    /// The catalog's functions, one list per schema, a few at a time; a
    /// schema whose list fails is left out (as the SQL path leaves them
    /// all out when `information_schema.routines` fails).
    async fn uc_functions(&self, cat: &str, schemas: &[String]) -> Vec<DbObject> {
        let mut out = Vec::new();
        let schemas: Vec<&String> = schemas
            .iter()
            .filter(|s| !s.eq_ignore_ascii_case(INFORMATION_SCHEMA))
            .collect();
        for chunk in schemas.chunks(FUNCTION_LISTS) {
            let mut set = tokio::task::JoinSet::new();
            for schema in chunk {
                let api = self.api.clone();
                let schema = schema.to_string();
                let path = format!(
                    "/api/2.1/unity-catalog/functions?catalog_name={}&schema_name={}&max_results=0",
                    escape(cat),
                    escape(&schema)
                );
                set.spawn(async move {
                    let r = uc_pages(&api, &path, "functions", usize::MAX).await;
                    (schema, r)
                });
            }
            while let Some(done) = set.join_next().await {
                match done {
                    Ok((schema, Ok(items))) => {
                        out.extend(names(&items).into_iter().map(|name| DbObject {
                            kind: kinds::FUNCTION.into(),
                            schema: Some(schema.clone()),
                            name,
                            parent: None,
                        }))
                    }
                    Ok((schema, Err(e))) => {
                        tracing::warn!("Unity Catalog function list of {cat}.{schema} failed: {e}")
                    }
                    Err(e) => tracing::warn!("Unity Catalog function list failed: {e}"),
                }
            }
        }
        out.sort_by(by_schema_and_name);
        out
    }

    async fn uc_tree(&self, cat: &str) -> Result<Option<Vec<DbObject>>> {
        if !self.uc_lists(cat).await? {
            return Ok(None);
        }
        let tables = uc_pages(
            &self.api,
            &format!(
                "/api/2.1/unity-catalog/table-summaries?catalog_name={}&max_results=0",
                escape(cat)
            ),
            "tables",
            usize::MAX,
        )
        .await?;
        let mut out: Vec<DbObject> = tables
            .iter()
            .filter_map(summary_object)
            .filter(|o| o.schema.as_deref() != Some(INFORMATION_SCHEMA))
            .collect();
        out.sort_by(by_schema_and_name);
        let schemas = self.uc_schema_names(cat).await?;
        out.extend(self.uc_functions(cat, &schemas).await);
        Ok(Some(out))
    }

    /// The catalog's tables, views and functions: from Unity Catalog, or
    /// through SQL where it doesn't list them or fails.
    pub(crate) async fn objects(&self, cat: &str) -> Result<Vec<DbObject>> {
        match self.uc_tree(cat).await {
            Ok(Some(out)) => return Ok(out),
            Ok(None) => {}
            Err(e) => tracing::warn!("Unity Catalog listing of {cat} failed, using SQL: {e}"),
        }
        self.sql_objects(cat).await
    }

    /// The catalog's schemas: from Unity Catalog, or through SQL where it
    /// doesn't list them or fails. `information_schema` is the system one.
    pub(crate) async fn schemas(&self, cat: &str) -> Result<Vec<SchemaInfo>> {
        let listed = match self.uc_lists(cat).await {
            Ok(true) => self.uc_schema_names(cat).await.map(Some),
            Ok(false) => Ok(None),
            Err(e) => Err(e),
        };
        let names = match listed {
            Ok(Some(n)) => n,
            Ok(None) => self.sql_schema_names(cat).await?,
            Err(e) => {
                tracing::warn!("Unity Catalog schema list of {cat} failed, using SQL: {e}");
                self.sql_schema_names(cat).await?
            }
        };
        Ok(names
            .into_iter()
            .map(|name| SchemaInfo {
                system: name.eq_ignore_ascii_case(INFORMATION_SCHEMA),
                name,
            })
            .collect())
    }

    // -- the SQL path (on the warehouse) ---------------------------------------------

    async fn sql_catalogs(&self) -> Result<Vec<String>> {
        let rows = self.text_rows("SHOW CATALOGS", &[]).await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| r.into_iter().next().flatten())
            .collect())
    }

    async fn sql_objects(&self, cat: &str) -> Result<Vec<DbObject>> {
        let cat = quote_ident(Quote::Backtick, cat);
        let mut out = Vec::new();
        let tables = self
            .text_rows(
                &format!(
                    "SELECT table_schema, table_name, table_type FROM {cat}.information_schema.tables
                     WHERE table_schema <> 'information_schema' ORDER BY 1, 2"
                ),
                &[],
            )
            .await?;
        for r in tables {
            out.push(DbObject {
                kind: kind_of(r.get(2).cloned().flatten().as_deref().unwrap_or("")).into(),
                schema: r.first().cloned().flatten(),
                name: r.get(1).cloned().flatten().unwrap_or_default(),
                parent: None,
            });
        }
        if let Ok(rows) = self
            .text_rows(
                &format!(
                    "SELECT routine_schema, routine_name FROM {cat}.information_schema.routines
                     WHERE routine_schema <> 'information_schema' ORDER BY 1, 2"
                ),
                &[],
            )
            .await
        {
            out.extend(rows.into_iter().map(|r| DbObject {
                kind: kinds::FUNCTION.into(),
                schema: r.first().cloned().flatten(),
                name: r.get(1).cloned().flatten().unwrap_or_default(),
                parent: None,
            }));
        }
        Ok(out)
    }

    async fn sql_schema_names(&self, cat: &str) -> Result<Vec<String>> {
        let cat = quote_ident(Quote::Backtick, cat);
        let rows = self
            .text_rows(
                &format!("SELECT schema_name FROM {cat}.information_schema.schemata ORDER BY 1"),
                &[],
            )
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|r| r.into_iter().next().flatten())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use crate::transfer::tests::{serve, session};
    use dbine_driver::{kinds, ConnectionConfig, Error};
    use serde_json::Value as Json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const DONE: &str =
        r#"{"statement_id":"s","status":{"state":"SUCCEEDED"},"result":{"data_array":[]}}"#;

    fn objects(v: &[dbine_driver::DbObject]) -> Vec<(String, String, String)> {
        v.iter()
            .map(|o| {
                (
                    o.kind.clone(),
                    o.schema.clone().unwrap_or_default(),
                    o.name.clone(),
                )
            })
            .collect()
    }

    fn expected(v: &[(&str, &str, &str)]) -> Vec<(String, String, String)> {
        v.iter()
            .map(|(k, s, n)| (k.to_string(), s.to_string(), n.to_string()))
            .collect()
    }

    /// A catalog's tree from Unity Catalog: every page, the kinds the SQL
    /// path gives, information_schema left out, sorted by schema and name
    /// with the functions last, and not one statement on the warehouse.
    #[tokio::test]
    async fn the_tree_comes_from_unity_catalog_without_the_warehouse() {
        let statements = Arc::new(AtomicUsize::new(0));
        let st = statements.clone();
        let base = serve(Arc::new(move |m, p, _| match (m, p) {
            ("POST", _) => {
                st.fetch_add(1, Ordering::SeqCst);
                (200, DONE.into())
            }
            (_, "/api/2.1/unity-catalog/catalogs/main") => (
                200,
                r#"{"name":"main","catalog_type":"MANAGED_CATALOG"}"#.into(),
            ),
            (_, p) if p.starts_with("/api/2.1/unity-catalog/table-summaries?catalog_name=main&max_results=0&page_token=p2") => (
                200,
                r#"{"tables":[{"full_name":"main.sales.v","table_type":"VIEW"},{"full_name":"main.ana.mv","table_type":"MATERIALIZED_VIEW"},{"full_name":"main.sales.st","table_type":"STREAMING_TABLE"}]}"#.into(),
            ),
            (_, p) if p.starts_with("/api/2.1/unity-catalog/table-summaries?catalog_name=main&max_results=0") => (
                200,
                r#"{"tables":[{"full_name":"main.sales.orders","table_type":"MANAGED"},{"full_name":"main.information_schema.tables","table_type":"VIEW"}],"next_page_token":"p2"}"#.into(),
            ),
            (_, p) if p.starts_with("/api/2.1/unity-catalog/schemas?catalog_name=main") => (
                200,
                r#"{"schemas":[{"name":"sales"},{"name":"information_schema"},{"name":"ana"}]}"#.into(),
            ),
            (_, p) if p.contains("functions?catalog_name=main&schema_name=sales") => (
                200,
                r#"{"functions":[{"name":"f1","schema_name":"sales"}]}"#.into(),
            ),
            (_, p) if p.contains("functions?catalog_name=main&schema_name=ana") => {
                (403, r#"{"message":"no USE SCHEMA"}"#.into())
            }
            _ => (404, r#"{"message":"unexpected"}"#.into()),
        }))
        .await;
        let mut s = session(base);
        s.catalog = Some("main".into());
        let got = s.objects("main").await.unwrap();
        assert_eq!(
            objects(&got),
            expected(&[
                (kinds::MATERIALIZED_VIEW, "ana", "mv"),
                (kinds::TABLE, "sales", "orders"),
                (kinds::TABLE, "sales", "st"),
                (kinds::VIEW, "sales", "v"),
                (kinds::FUNCTION, "sales", "f1"),
            ])
        );
        let schemas = s.schemas("main").await.unwrap();
        assert_eq!(
            schemas
                .iter()
                .map(|x| (x.name.as_str(), x.system))
                .collect::<Vec<_>>(),
            [
                ("ana", false),
                ("information_schema", true),
                ("sales", false)
            ]
        );
        assert_eq!(statements.load(Ordering::SeqCst), 0);
    }

    /// The legacy hive_metastore, a foreign catalog and a failing API keep
    /// the SQL path, with the same answer as before.
    #[tokio::test]
    async fn hive_metastore_foreign_catalogs_and_api_errors_use_sql() {
        for cat in ["hive_metastore", "fed", "broken"] {
            let statements = Arc::new(AtomicUsize::new(0));
            let st = statements.clone();
            let base = serve(Arc::new(move |m, p, body| match (m, p) {
                ("POST", "/api/2.0/sql/statements") => {
                    st.fetch_add(1, Ordering::SeqCst);
                    let rows = if body.contains("information_schema.routines") {
                        r#"[["sales","f"]]"#
                    } else if body.contains("information_schema.schemata") {
                        r#"[["sales"]]"#
                    } else {
                        r#"[["sales","t","MANAGED"],["sales","v","VIEW"]]"#
                    };
                    let done = format!(
                        r#"{{"statement_id":"s","status":{{"state":"SUCCEEDED"}},"result":{{"data_array":{rows}}}}}"#
                    );
                    (200, done)
                }
                (_, "/api/2.1/unity-catalog/catalogs/fed") => (
                    200,
                    r#"{"name":"fed","catalog_type":"FOREIGN_CATALOG"}"#.into(),
                ),
                (_, "/api/2.1/unity-catalog/catalogs/broken") => (
                    200,
                    r#"{"name":"broken","catalog_type":"MANAGED_CATALOG"}"#.into(),
                ),
                (_, p) if p.contains("table-summaries") || p.contains("schemas?") => {
                    (500, r#"{"message":"internal"}"#.into())
                }
                _ => (404, r#"{"message":"unexpected"}"#.into()),
            }))
            .await;
            let mut s = session(base);
            s.catalog = Some(cat.into());
            let got = s.objects(cat).await.unwrap();
            assert_eq!(
                objects(&got),
                expected(&[
                    (kinds::TABLE, "sales", "t"),
                    (kinds::VIEW, "sales", "v"),
                    (kinds::FUNCTION, "sales", "f"),
                ]),
                "{cat}"
            );
            let schemas = s.schemas(cat).await.unwrap();
            assert_eq!(schemas.len(), 1, "{cat}");
            assert_eq!(statements.load(Ordering::SeqCst), 3, "{cat}");
        }
    }

    fn config(base: &str) -> ConnectionConfig {
        let mut cfg = ConnectionConfig {
            driver: "databricks".into(),
            host: base.into(),
            ..Default::default()
        };
        cfg.options
            .insert("warehouse".into(), "/sql/1.0/warehouses/w".into());
        cfg.options.insert("token".into(), "t".into());
        cfg
    }

    /// A server whose warehouse is in `state` ("401": the token is refused)
    /// and whose statements are answered `answer` (status, body): the
    /// statements it got (their bodies) and the status polls.
    async fn warehouse(
        state: &'static str,
        answer: (u16, &'static str),
    ) -> (String, Arc<Mutex<Vec<Json>>>, Arc<AtomicUsize>) {
        let posted = Arc::new(Mutex::new(Vec::new()));
        let polls = Arc::new(AtomicUsize::new(0));
        let (p2, n2) = (posted.clone(), polls.clone());
        let base = serve(Arc::new(move |m, p, body| match (m, p) {
            ("GET", "/api/2.0/sql/warehouses/w") => match state {
                "401" => (401, r#"{"message":"invalid token"}"#.into()),
                s => (200, format!(r#"{{"id":"w","state":"{s}"}}"#)),
            },
            ("POST", "/api/2.0/sql/statements") => {
                p2.lock().unwrap().push(serde_json::from_str(body).unwrap());
                (answer.0, answer.1.into())
            }
            ("GET", p) if p.starts_with("/api/2.0/sql/statements/") => {
                n2.fetch_add(1, Ordering::SeqCst);
                (200, DONE.into())
            }
            ("GET", p) if p.starts_with("/api/2.1/unity-catalog/catalogs?max_results=0") => (
                200,
                r#"{"catalogs":[{"name":"system"},{"name":"main"}]}"#.into(),
            ),
            _ => (404, r#"{"message":"unexpected"}"#.into()),
        }))
        .await;
        (base, posted, polls)
    }

    async fn connect(base: &str) -> dbine_driver::Result<Box<dyn dbine_driver::Session>> {
        crate::drivers()[0].connect(&config(base), None).await
    }

    /// A stopped warehouse isn't waited for: the connection's SELECT 1
    /// only goes in (it starts the warehouse), the version and the
    /// catalogs need no statement.
    #[tokio::test]
    async fn connecting_does_not_wait_for_a_stopped_warehouse() {
        for state in ["STOPPED", "STARTING"] {
            let pending = r#"{"statement_id":"s","status":{"state":"PENDING"}}"#;
            let (base, posted, polls) = warehouse(state, (200, pending)).await;
            let mut s = connect(&base).await.unwrap();
            {
                let posted = posted.lock().unwrap();
                assert_eq!(posted.len(), 1, "{state}");
                assert_eq!(posted[0]["statement"], "SELECT 1");
                assert_eq!(posted[0]["wait_timeout"], "0s");
                assert!(posted[0].get("on_wait_timeout").is_none());
            }
            assert_eq!(s.server_version().await.unwrap(), "Databricks SQL");
            assert_eq!(
                s.list_databases().await.unwrap(),
                ["hive_metastore", "main", "system"]
            );
            assert_eq!(posted.lock().unwrap().len(), 1, "{state}");
            assert_eq!(polls.load(Ordering::SeqCst), 0, "{state}");
        }
    }

    /// A running warehouse answers SELECT 1 as before, and SHOW CATALOGS
    /// gives the catalogs.
    #[tokio::test]
    async fn a_running_warehouse_is_checked_as_before() {
        let (base, posted, _) = warehouse("RUNNING", (200, DONE)).await;
        let mut s = connect(&base).await.unwrap();
        s.list_databases().await.unwrap();
        let posted = posted.lock().unwrap();
        assert_eq!(posted[0]["statement"], "SELECT 1");
        assert_eq!(posted[0]["wait_timeout"], "30s");
        assert_eq!(posted[1]["statement"], "SHOW CATALOGS");
    }

    /// Refusals still fail the connection with the same kinds of errors:
    /// a bad token, no access to a stopped warehouse, a statement refused
    /// on the way in or failed at once.
    #[tokio::test]
    async fn refusals_still_fail_the_connection() {
        let (base, _, _) = warehouse("401", (401, r#"{"message":"invalid token"}"#)).await;
        assert!(matches!(
            connect(&base).await.err(),
            Some(Error::AuthFailed(_))
        ));
        let (base, _, _) = warehouse("STOPPED", (403, r#"{"message":"no CAN_USE"}"#)).await;
        assert!(
            matches!(connect(&base).await.err(), Some(Error::AuthFailed(m)) if m == "no CAN_USE")
        );
        let (base, _, _) = warehouse("STOPPED", (400, r#"{"message":"bad warehouse"}"#)).await;
        assert!(
            matches!(connect(&base).await.err(), Some(Error::Connect(m)) if m == "bad warehouse")
        );
        let failed =
            r#"{"statement_id":"s","status":{"state":"FAILED","error":{"message":"no catalog"}}}"#;
        let (base, _, _) = warehouse("STOPPED", (200, failed)).await;
        assert!(matches!(connect(&base).await.err(), Some(Error::Connect(m)) if m == "no catalog"));
    }
}
