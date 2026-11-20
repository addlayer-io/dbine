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
use crate::paging::{self, Binds, Source};
use crate::stats::kind_of;
use crate::{Api, DatabricksSession};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{kinds, DbObject, Error, ObjectPage, ObjectQuery, Result, SchemaInfo};
use serde_json::{json, Value as Json};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The legacy per-workspace metastore: not in Unity Catalog's lists.
pub(crate) const HIVE_METASTORE: &str = "hive_metastore";
/// Function lists (one per schema) asked for at the same time.
const FUNCTION_LISTS: usize = 8;
/// The system schema every Unity Catalog catalog has.
const INFORMATION_SCHEMA: &str = "information_schema";
/// The kinds a table summary (or `information_schema.tables`) gives.
const TABLE_KINDS: &[&str] = &[kinds::TABLE, kinds::VIEW, kinds::MATERIALIZED_VIEW];
/// How long a catalog's functions serve searches across schemas (as long
/// as the app keeps a whole list for drivers without pages).
const FUNCTIONS_TTL: Duration = Duration::from_secs(60);

/// A catalog's functions, when they were read, and the catalog.
pub(crate) type Functions = (String, Instant, Arc<Vec<DbObject>>);

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

    // -- the explorer's pages (see crate::paging) -------------------------------------

    /// A page of the catalog's objects: the same tables, views and
    /// functions as `list_objects`, narrowed to the query. From Unity
    /// Catalog where it lists the catalog, else (and when it fails) one
    /// `information_schema` query on the warehouse.
    ///
    /// Unity Catalog's lists promise no order (and a page may come empty
    /// with a next token), so their tokens can't be the tree's cursor: each
    /// page reads the narrow list whole (one schema's `table-summaries`,
    /// the name as `table_name_pattern`; one schema's functions) and
    /// orders and pages it here, after the cursor.
    pub(crate) async fn objects_page(&mut self, q: &ObjectQuery) -> Result<ObjectPage> {
        let Ok(cat) = self.catalog().map(str::to_string) else {
            return Ok(ObjectPage::default());
        };
        // Every object is in a schema, never information_schema's.
        if q.no_schema || q.schema.as_deref() == Some(INFORMATION_SCHEMA) {
            return Ok(ObjectPage::default());
        }
        let after = paging::after(q);
        match self.uc_lists(&cat).await {
            Ok(true) => match self.uc_page(&cat, q).await {
                Ok(found) => return Ok(paging::merge(q, after.as_ref(), found)),
                Err(e) => tracing::warn!("Unity Catalog page of {cat} failed, using SQL: {e}"),
            },
            Ok(false) => {}
            Err(e) => tracing::warn!("Unity Catalog listing of {cat} failed, using SQL: {e}"),
        }
        self.sql_page(&cat, q, after.as_ref()).await
    }

    /// Everything Unity Catalog lists for the query, filtered by it.
    async fn uc_page(&mut self, cat: &str, q: &ObjectQuery) -> Result<Vec<DbObject>> {
        let mut found = Vec::new();
        if !paging::wanted(q, TABLE_KINDS).is_empty() {
            // Unity Catalog stores names in lowercase; the patterns match
            // at least what's asked, and `matches` keeps the exact ones.
            let mut path = format!(
                "/api/2.1/unity-catalog/table-summaries?catalog_name={}&max_results=0",
                escape(cat)
            );
            if let Some(s) = &q.schema {
                path += &format!(
                    "&schema_name_pattern={}",
                    escape(&paging::widen(s).to_lowercase())
                );
            }
            if let Some(p) = paging::like(q) {
                path += &format!("&table_name_pattern={}", escape(&p.to_lowercase()));
            }
            let tables = uc_pages(&self.api, &path, "tables", usize::MAX).await?;
            found.extend(
                tables
                    .iter()
                    .filter_map(summary_object)
                    .filter(|o| o.schema.as_deref() != Some(INFORMATION_SCHEMA) && q.matches(o)),
            );
        }
        if !paging::wanted(q, &[kinds::FUNCTION]).is_empty() {
            let functions = match &q.schema {
                Some(s) => Arc::new(self.uc_functions(cat, std::slice::from_ref(s)).await),
                None => self.catalog_functions(cat).await?,
            };
            found.extend(functions.iter().filter(|o| q.matches(o)).cloned());
        }
        Ok(found)
    }

    /// Every schema's functions, for a search across schemas: kept
    /// [`FUNCTIONS_TTL`], since there's one list per schema.
    async fn catalog_functions(&mut self, cat: &str) -> Result<Arc<Vec<DbObject>>> {
        if let Some((c, at, f)) = &self.uc_functions {
            if c == cat && at.elapsed() < FUNCTIONS_TTL {
                return Ok(f.clone());
            }
        }
        let schemas = self.uc_schema_names(cat).await?;
        let f = Arc::new(self.uc_functions(cat, &schemas).await);
        self.uc_functions = Some((cat.to_string(), Instant::now(), f.clone()));
        Ok(f)
    }

    /// The page from the catalog's `information_schema`: tables and
    /// routines in one query; if it fails, the tables alone (as
    /// `list_objects`, which leaves out routines it can't read).
    async fn sql_page(
        &self,
        cat: &str,
        q: &ObjectQuery,
        after: Option<&DbObject>,
    ) -> Result<ObjectPage> {
        let cat = quote_ident(Quote::Backtick, cat);
        let (tables_from, routines_from) = (
            format!("{cat}.information_schema.tables WHERE table_schema <> 'information_schema'"),
            format!(
                "{cat}.information_schema.routines WHERE routine_schema <> 'information_schema'"
            ),
        );
        let tables = Source {
            distinct: false,
            schema: "table_schema",
            name: "table_name",
            kind: "CASE table_type WHEN 'VIEW' THEN 'view' \
                   WHEN 'MATERIALIZED_VIEW' THEN 'materialized_view' ELSE 'table' END",
            from: &tables_from,
            kinds: TABLE_KINDS,
        };
        let routines = Source {
            distinct: false,
            schema: "routine_schema",
            name: "routine_name",
            kind: "'function'",
            from: &routines_from,
            kinds: &[kinds::FUNCTION],
        };
        let wants_tables = !paging::wanted(q, TABLE_KINDS).is_empty();
        let found = match self.sql_rows(&[&tables, &routines], q, after).await {
            Ok(rows) => rows,
            Err(e) if wants_tables && paging::wanted(q, &[kinds::FUNCTION]).is_empty() => {
                return Err(e)
            }
            Err(e) if !wants_tables => {
                tracing::debug!("routines page of {cat} failed: {e}");
                Vec::new()
            }
            Err(_) => self.sql_rows(&[&tables], q, after).await?,
        };
        Ok(paging::merge(q, after, found))
    }

    async fn sql_rows(
        &self,
        sources: &[&Source<'_>],
        q: &ObjectQuery,
        after: Option<&DbObject>,
    ) -> Result<Vec<DbObject>> {
        let mut b = Binds::default();
        let Some(sql) = paging::query(sources, q, after, &mut b) else {
            return Ok(Vec::new());
        };
        let args: Vec<&str> = b.0.iter().map(String::as_str).collect();
        let rows = self.text_rows(&sql, &args).await?;
        Ok(rows.iter().filter_map(|r| paging::object(r)).collect())
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

    // ---- the explorer's pages ----------------------------------------------------

    use dbine_driver::objects::page_of;
    use dbine_driver::{DbObject, ObjectQuery};
    use serde_json::json;

    /// `%xx` → the byte.
    fn decode(s: &str) -> String {
        let b = s.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%' {
                out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    /// SQL `LIKE` (`%`, `_`), as Unity Catalog's patterns.
    fn like(p: &[char], s: &[char]) -> bool {
        match p.split_first() {
            None => s.is_empty(),
            Some(('%', rest)) => (0..=s.len()).any(|i| like(rest, &s[i..])),
            Some((c, rest)) => {
                s.first().is_some_and(|x| *c == '_' || x == c) && like(rest, &s[1..])
            }
        }
    }

    fn param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
    }

    /// Unity Catalog's lists of the fake catalog `main`: (schema, name,
    /// table_type) and (schema, function).
    const UC_TABLES: &[(&str, &str, &str)] = &[
        ("sales", "orders", "MANAGED"),
        ("sales", "v_orders", "VIEW"),
        ("sales", "mv", "MATERIALIZED_VIEW"),
        ("sales", "st", "STREAMING_TABLE"),
        ("ana", "orders", "EXTERNAL"),
        ("ana", "x_y", "VIEW"),
        ("my_s", "a", "MANAGED"),
        ("myxs", "b", "MANAGED"),
        ("information_schema", "tables", "VIEW"),
    ];
    const UC_FUNCTIONS: &[(&str, &str)] = &[("sales", "f_orders"), ("sales", "g"), ("ana", "h")];

    /// A Unity Catalog server for `main`: table-summaries filtered by its
    /// LIKE patterns and given back in pages of 3, last first, with an
    /// empty page among them; schemas; functions per schema. Records every
    /// path; counts the warehouse's statements.
    async fn unity(paths: Arc<Mutex<Vec<String>>>, statements: Arc<AtomicUsize>) -> String {
        serve(Arc::new(move |m, p, _| {
            paths.lock().unwrap().push(p.to_string());
            if m == "POST" {
                statements.fetch_add(1, Ordering::SeqCst);
                return (200, DONE.into());
            }
            let (path, query) = p.split_once('?').unwrap_or((p, ""));
            let token: usize = param(query, "page_token").map_or(0, |t| t.parse().unwrap());
            let pages = |items: Vec<Json>, key: &str| {
                // Last first, three a page, an empty page after the first.
                let mut items = items;
                items.reverse();
                let mut chunks: Vec<Vec<Json>> = items.chunks(3).map(<[Json]>::to_vec).collect();
                if chunks.len() > 1 {
                    chunks.insert(1, Vec::new());
                }
                let page = chunks.get(token).cloned().unwrap_or_default();
                let mut body = json!({ key: page });
                if token + 1 < chunks.len() {
                    body["next_page_token"] = json!((token + 1).to_string());
                }
                (200, body.to_string())
            };
            match path {
                "/api/2.1/unity-catalog/catalogs/main" => (
                    200,
                    r#"{"name":"main","catalog_type":"MANAGED_CATALOG"}"#.into(),
                ),
                "/api/2.1/unity-catalog/table-summaries" => {
                    let pat = |k: &str| param(query, k).map(decode).map(|p| p.chars().collect::<Vec<_>>());
                    let (sp, tp) = (pat("schema_name_pattern"), pat("table_name_pattern"));
                    let mut items: Vec<Json> = Vec::new();
                    for (s, n, t) in UC_TABLES {
                        let ok = |p: &Option<Vec<char>>, v: &str| {
                            p.as_ref().is_none_or(|p| like(p, &v.chars().collect::<Vec<_>>()))
                        };
                        if ok(&sp, s) && ok(&tp, n) {
                            items.push(json!({ "full_name": format!("main.{s}.{n}"), "table_type": t }));
                        }
                    }
                    // A second page's worth of tables in sales.
                    for i in 0..12 {
                        let n = format!("t{i:02}");
                        if ok_name(&sp, "sales") && ok_name(&tp, &n) {
                            items.push(json!({ "full_name": format!("main.sales.{n}"), "table_type": "MANAGED" }));
                        }
                    }
                    pages(items, "tables")
                }
                "/api/2.1/unity-catalog/schemas" => {
                    let names = ["sales", "ana", "my_s", "myxs", "information_schema"];
                    pages(names.iter().map(|n| json!({ "name": n })).collect(), "schemas")
                }
                "/api/2.1/unity-catalog/functions" => {
                    let schema = param(query, "schema_name").map(decode).unwrap_or_default();
                    let items = UC_FUNCTIONS
                        .iter()
                        .filter(|(s, _)| *s == schema)
                        .map(|(s, n)| json!({ "name": n, "schema_name": s }))
                        .collect();
                    pages(items, "functions")
                }
                _ => (404, r#"{"message":"unexpected"}"#.into()),
            }
        }))
        .await
    }

    fn ok_name(p: &Option<Vec<char>>, v: &str) -> bool {
        p.as_ref()
            .is_none_or(|p| like(p, &v.chars().collect::<Vec<_>>()))
    }

    fn names(objs: &[DbObject]) -> Vec<String> {
        objs.iter()
            .map(|o| {
                format!(
                    "{}:{}.{}",
                    o.kind,
                    o.schema.as_deref().unwrap_or("-"),
                    o.name
                )
            })
            .collect()
    }

    /// Every page of `q` from the driver and out of the whole list (as
    /// the app pages list_objects): the same objects in the same pages.
    async fn same_pages(
        s: &mut crate::DatabricksSession,
        mut q: ObjectQuery,
        whole: &[DbObject],
        set: Option<&Arc<Mutex<ObjectQuery>>>,
    ) -> usize {
        let mut offset = q.clone();
        let mut pages = 0;
        loop {
            if let Some(set) = set {
                *set.lock().unwrap() = q.clone();
            }
            let got = s.objects_page(&q).await.unwrap();
            let want = page_of(whole, &offset);
            assert_eq!(names(&got.objects), names(&want.objects), "{q:?}");
            assert_eq!(got.next.is_some(), want.next.is_some(), "{q:?}");
            pages += 1;
            match (got.next, want.next) {
                (Some(a), Some(b)) => {
                    q.after = Some(a);
                    offset.after = Some(b);
                }
                _ => return pages,
            }
        }
    }

    fn with(f: impl Fn(&mut ObjectQuery)) -> ObjectQuery {
        let mut q = ObjectQuery {
            limit: 3,
            ..Default::default()
        };
        f(&mut q);
        q
    }

    /// Unity Catalog's pages: folders, searches, lookups and "Otros" equal
    /// what the app pages out of the whole list, although its lists come
    /// in no order and with an empty page; a folder asks for its schema
    /// only (the name as a lowercase pattern), functions of one schema
    /// for a function folder, every schema's once a minute for searches;
    /// nothing runs on the warehouse.
    #[tokio::test]
    async fn unity_catalog_pages_are_narrow_and_ordered() {
        let paths = Arc::new(Mutex::new(Vec::new()));
        let statements = Arc::new(AtomicUsize::new(0));
        let base = unity(paths.clone(), statements.clone()).await;
        let mut s = session(base);
        s.catalog = Some("main".into());
        let whole = s.objects("main").await.unwrap();
        assert_eq!(whole.len(), 23, "{:?}", names(&whole));
        let cases = [
            with(|q| {
                q.schema = Some("sales".into());
                q.kind = Some(kinds::TABLE.into());
            }),
            with(|q| {
                q.schema = Some("sales".into());
                q.kind = Some(kinds::FUNCTION.into());
                q.limit = 1;
            }),
            with(|q| q.schema = Some("my_s".into())),
            with(|q| q.schema = Some("Sales".into())),
            with(|q| q.schema = Some("information_schema".into())),
            with(|q| q.kind = Some(kinds::VIEW.into())),
            with(|q| q.pattern = Some("ORDERS".into())),
            with(|q| q.pattern = Some("x_".into())),
            with(|q| q.name = Some("Orders".into())),
            with(|q| q.no_schema = true),
            with(|q| q.limit = 4),
        ];
        let mut longest = 0;
        for q in cases {
            longest = longest.max(same_pages(&mut s, q, &whole, None).await);
        }
        assert!(longest >= 5, "the sales tables span pages: {longest}");

        // What a folder, a lookup and "Otros" ask.
        paths.lock().unwrap().clear();
        let folder = with(|q| {
            q.schema = Some("sales".into());
            q.kind = Some(kinds::TABLE.into());
        });
        s.objects_page(&folder).await.unwrap();
        let lookup = with(|q| q.name = Some("V_Orders".into()));
        s.objects_page(&lookup).await.unwrap();
        let others = with(|q| {
            q.not_kinds = [
                kinds::TABLE,
                kinds::VIEW,
                kinds::MATERIALIZED_VIEW,
                kinds::FUNCTION,
            ]
            .map(String::from)
            .to_vec()
        });
        assert!(s.objects_page(&others).await.unwrap().objects.is_empty());
        let functions = with(|q| {
            q.schema = Some("ana".into());
            q.kind = Some(kinds::FUNCTION.into());
        });
        s.objects_page(&functions).await.unwrap();
        let seen = paths.lock().unwrap().clone();
        let lists: Vec<&String> = seen
            .iter()
            .filter(|p| !p.contains("/catalogs/main"))
            .collect();
        assert!(lists.iter().any(|p| p.contains(
            "table-summaries?catalog_name=main&max_results=0&schema_name_pattern=sales"
        )));
        assert!(lists.iter().any(|p| p.contains(
            "table-summaries?catalog_name=main&max_results=0&table_name_pattern=v_orders"
        )));
        assert!(
            lists
                .iter()
                .all(|p| !p.contains("functions") || p.contains("schema_name=ana")),
            "{lists:?}"
        );
        assert!(lists
            .iter()
            .any(|p| p.contains("functions?catalog_name=main&schema_name=ana")));
        // Searches across schemas read every schema's functions once a minute.
        s.uc_functions = None;
        paths.lock().unwrap().clear();
        for p in ["o", "or", "g"] {
            s.objects_page(&with(|q| q.pattern = Some(p.into())))
                .await
                .unwrap();
        }
        let seen = paths.lock().unwrap().clone();
        let sales_fns = seen
            .iter()
            .filter(|p| p.contains("functions?catalog_name=main&schema_name=sales"))
            .count();
        assert_eq!(sales_fns, 1, "{seen:?}");
        assert_eq!(statements.load(Ordering::SeqCst), 0);
    }

    /// A catalog Unity Catalog doesn't list: one information_schema query
    /// per page, with `:pN` markers and the page's LIMIT; pages equal the
    /// whole list's; routines that fail leave out only the functions.
    #[tokio::test]
    async fn sql_pages_bind_their_values_and_survive_failing_routines() {
        let whole: Vec<DbObject> = {
            let mut v: Vec<DbObject> = (0..10)
                .map(|i| DbObject {
                    kind: kinds::TABLE.into(),
                    schema: Some("Sales".into()),
                    name: format!("T{i}"),
                    parent: None,
                })
                .collect();
            for (k, s, n) in [
                (kinds::VIEW, "Sales", "v_orders"),
                (kinds::TABLE, "ana", "Orders"),
                (kinds::FUNCTION, "Sales", "orders_fn"),
                (kinds::FUNCTION, "ana", "h"),
            ] {
                v.push(DbObject {
                    kind: k.into(),
                    schema: Some(s.into()),
                    name: n.into(),
                    parent: None,
                });
            }
            v
        };
        for broken in [false, true] {
            let asked = Arc::new(Mutex::new(ObjectQuery::default()));
            let sent = Arc::new(Mutex::new(Vec::<Json>::new()));
            let (a2, s2, w2) = (asked.clone(), sent.clone(), whole.clone());
            let base = serve(Arc::new(move |m, p, body| match (m, p) {
                (_, "/api/2.1/unity-catalog/catalogs/fed") => (
                    200,
                    r#"{"name":"fed","catalog_type":"FOREIGN_CATALOG"}"#.into(),
                ),
                ("POST", "/api/2.0/sql/statements") => {
                    let b: Json = serde_json::from_str(body).unwrap();
                    s2.lock().unwrap().push(b.clone());
                    let stmt = b["statement"].as_str().unwrap();
                    let routines = stmt.contains(".information_schema.routines ");
                    if broken && routines {
                        return (400, r#"{"message":"no routines"}"#.into());
                    }
                    let tables = stmt.contains(".information_schema.tables ");
                    let q = a2.lock().unwrap().clone();
                    let limit: usize = stmt.rsplit("LIMIT ").next().unwrap().parse().unwrap();
                    let mut offset = q.clone();
                    offset.limit = limit as u32;
                    // The server's answer: the page_of order past the cursor.
                    let from = crate::paging::after(&q);
                    let hits: Vec<&DbObject> = w2
                        .iter()
                        .filter(|o| {
                            if o.kind == kinds::FUNCTION {
                                routines
                            } else {
                                tables
                            }
                        })
                        .filter(|o| q.matches(o))
                        .collect();
                    let mut hits: Vec<&DbObject> = hits
                        .into_iter()
                        .filter(|o| {
                            from.as_ref().is_none_or(|f| {
                                let k = |o: &DbObject| {
                                    let s = o.schema.clone().unwrap_or_default();
                                    (
                                        s.to_lowercase(),
                                        o.name.to_lowercase(),
                                        s,
                                        o.name.clone(),
                                        o.kind.clone(),
                                    )
                                };
                                k(o) > k(f)
                            })
                        })
                        .collect();
                    hits.sort_by_key(|o| {
                        let s = o.schema.clone().unwrap_or_default();
                        (
                            s.to_lowercase(),
                            o.name.to_lowercase(),
                            s,
                            o.name.clone(),
                            o.kind.clone(),
                        )
                    });
                    let rows: Vec<Json> = hits
                        .iter()
                        .take(limit)
                        .map(|o| json!([o.schema, o.name, o.kind]))
                        .collect();
                    (
                        200,
                        json!({ "statement_id": "s", "status": { "state": "SUCCEEDED" },
                                "result": { "data_array": rows } })
                        .to_string(),
                    )
                }
                _ => (404, r#"{"message":"unexpected"}"#.into()),
            }))
            .await;
            let mut s = session(base);
            s.catalog = Some("fed".into());
            let expected: Vec<DbObject> = whole
                .iter()
                .filter(|o| !broken || o.kind != kinds::FUNCTION)
                .cloned()
                .collect();
            for q in [
                with(|q| {
                    q.schema = Some("Sales".into());
                    q.kind = Some(kinds::TABLE.into());
                }),
                with(|q| q.pattern = Some("ORDERS".into())),
                with(|q| q.kind = Some(kinds::FUNCTION.into())),
                with(|q| q.name = Some("orders".into())),
                with(|q| q.limit = 4),
            ] {
                same_pages(&mut s, q, &expected, Some(&asked)).await;
            }
            let sent = sent.lock().unwrap();
            let folder = &sent[0];
            let stmt = folder["statement"].as_str().unwrap();
            assert!(stmt.contains("FROM `fed`.information_schema.tables WHERE table_schema <> 'information_schema' AND table_schema = :p0"), "{stmt}");
            assert!(
                !stmt.contains("routines") && stmt.ends_with("LIMIT 4"),
                "{stmt}"
            );
            assert_eq!(
                folder["parameters"][0],
                json!({ "name": "p0", "value": "Sales" })
            );
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
