//! `list_objects_page` (see `dbine_driver::objects` and docs/drivers.md,
//! "Objects by page"): the tree's order and cursor, and one catalog query
//! for a page of the sources a query needs.
//!
//! BigQuery's lists (`tables.list`, `routines.list`) filter nothing but a
//! routine type and promise no order (and a table's name is case-
//! sensitive, while the tree orders ignoring case), so a page from them
//! would mean reading the whole dataset each time. The dataset's
//! `INFORMATION_SCHEMA.TABLES` and `ROUTINES` are read instead, in one
//! query narrowed to the page. Where they can't be read (no permission to
//! run jobs, an emulator without them) the driver answers `Unsupported`
//! from then on, and the app pages `list_objects` as before.
//!
//! - **Order:** name ignoring case, then as written, then the kind (objects
//!   have no schema): `ORDER BY LOWER(s), LOWER(n), s, n, k` on the
//!   server, the same key here.
//! - **Cursor:** the page's last object (`[schema, name, kind]`); the next
//!   page asks for what sorts after it (keyset), so pages neither skip nor
//!   repeat objects even when the dataset changes in between.
//! - **Page:** the query asks for one object more than the page, which
//!   tells whether more remain.

use crate::BigQuerySession;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{kinds, DbObject, Error, ObjectPage, ObjectQuery, Result};
use serde_json::{json, Value as Json};

/// The largest page asked of the server: a bigger `limit` gets pages of
/// this size (a page holds at most the limit).
pub(crate) const MAX_PAGE: usize = 10_000;

/// A bound value's placeholder (the `n`th, from 0; `text_rows` names them).
fn placeholder(n: usize) -> String {
    format!("@p{n}")
}

/// `needle` inside `hay` (both already lowercased).
fn contains(hay: &str, needle: &str) -> String {
    format!("STRPOS({hay}, {needle}) > 0")
}

/// The objects a page holds.
pub(crate) fn size(q: &ObjectQuery) -> usize {
    q.page_size().min(MAX_PAGE)
}

type Key = (bool, String, String, String, String, String);

/// The tree's order (`dbine_driver::objects::page_of`'s), with the kind last.
fn key(o: &DbObject) -> Key {
    let s = o.schema.clone().unwrap_or_default();
    (
        o.schema.is_some(),
        s.to_lowercase(),
        o.name.to_lowercase(),
        s,
        o.name.clone(),
        o.kind.clone(),
    )
}

/// The object the query's cursor points at (the previous page's last). A
/// cursor this driver didn't write starts from the top, as `page_of` does.
pub(crate) fn after(q: &ObjectQuery) -> Option<DbObject> {
    let (schema, name, kind): (Option<String>, String, String) =
        serde_json::from_str(q.after.as_deref()?).ok()?;
    Some(DbObject {
        kind,
        schema,
        name,
        parent: None,
    })
}

fn cursor(o: &DbObject) -> String {
    serde_json::json!([o.schema, o.name, o.kind]).to_string()
}

/// The kinds among `kinds` the query admits.
pub(crate) fn wanted(q: &ObjectQuery, kinds: &[&'static str]) -> Vec<&'static str> {
    kinds
        .iter()
        .copied()
        .filter(|k| {
            q.kind.as_deref().is_none_or(|w| w == *k) && !q.not_kinds.iter().any(|n| n == k)
        })
        .collect()
}

/// The page out of what the query found (at most one more than the page):
/// sorted, past the cursor, without repeats, cut at the page; the cursor
/// is the last object kept. What's shown is checked against the query once
/// more (a server's lowercase may differ in rare letters).
pub(crate) fn merge(q: &ObjectQuery, after: Option<&DbObject>, found: Vec<DbObject>) -> ObjectPage {
    let from = after.map(key);
    let mut found: Vec<(Key, DbObject)> = found
        .into_iter()
        .map(|o| (key(&o), o))
        .filter(|(k, _)| from.as_ref().is_none_or(|f| k > f))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.dedup_by(|a, b| a.0 == b.0);
    let size = size(q);
    let next = (found.len() > size).then(|| cursor(&found[size - 1].1));
    found.truncate(size);
    ObjectPage {
        objects: found
            .into_iter()
            .map(|(_, o)| o)
            .filter(|o| q.matches(o))
            .collect(),
        next,
    }
}

/// A catalog's list of objects: its rows as the schema (`''` for none), the
/// name and the kind's id.
pub(crate) struct Source<'a> {
    pub distinct: bool,
    pub schema: &'a str,
    pub name: &'a str,
    /// The row's kind id (an SQL expression).
    pub kind: &'a str,
    /// What follows `FROM`, up to a `WHERE` with `list_objects`'
    /// exclusions (`WHERE TRUE` when there are none).
    pub from: &'a str,
    /// The kinds it gives.
    pub kinds: &'static [&'static str],
}

/// The values bound to a query, in the order of their placeholders.
#[derive(Default)]
pub(crate) struct Binds(pub Vec<String>);

impl Binds {
    fn bind(&mut self, v: &str) -> String {
        self.0.push(v.to_string());
        placeholder(self.0.len() - 1)
    }
}

/// One query for the page out of `sources` (those the query admits, joined
/// with `UNION ALL`): rows `s, n, k` past the cursor, in the tree's order,
/// one more than the page. `None` when no source has a kind the query
/// admits. Every value is bound.
pub(crate) fn query(
    sources: &[&Source],
    q: &ObjectQuery,
    after: Option<&DbObject>,
    b: &mut Binds,
) -> Option<String> {
    if !q.no_schema && q.schema.as_deref() == Some("") {
        return None;
    }
    let mut parts = Vec::new();
    for src in sources {
        let kinds = wanted(q, src.kinds);
        if kinds.is_empty() {
            continue;
        }
        let mut sql = format!(
            "SELECT {}{} AS s, {} AS n, {} AS k FROM {}",
            if src.distinct { "DISTINCT " } else { "" },
            src.schema,
            src.name,
            src.kind,
            src.from
        );
        if q.no_schema {
            sql += &format!(" AND {} = ''", src.schema);
        } else if let Some(s) = &q.schema {
            sql += &format!(" AND {} = {}", src.schema, b.bind(s));
        }
        if kinds.len() < src.kinds.len() {
            let list: Vec<String> = kinds.iter().map(|k| format!("'{k}'")).collect();
            sql += &format!(" AND {} IN ({})", src.kind, list.join(", "));
        }
        if let Some(p) = q.pattern.as_deref().filter(|p| !p.is_empty()) {
            let needle = format!("LOWER({})", b.bind(p));
            sql += &format!(
                " AND {}",
                contains(&format!("LOWER({})", src.name), &needle)
            );
        }
        if let Some(n) = &q.name {
            sql += &format!(" AND LOWER({}) = LOWER({})", src.name, b.bind(n));
        }
        parts.push(sql);
    }
    if parts.is_empty() {
        return None;
    }
    let mut sql = format!("SELECT s, n, k FROM ({}) t", parts.join(" UNION ALL "));
    if let Some(a) = after {
        let s = a.schema.as_deref().unwrap_or("");
        let keys = [
            ("LOWER(s)", s, true),
            ("LOWER(n)", a.name.as_str(), true),
            ("s", s, false),
            ("n", a.name.as_str(), false),
            ("k", a.kind.as_str(), false),
        ];
        // (c1 > v1 OR (c1 = v1 AND (c2 > v2 OR (… AND cN > vN)))).
        let mut cond = String::new();
        let mut close = String::new();
        for (i, (col, v, lower)) in keys.iter().enumerate() {
            let mut val = || {
                let p = b.bind(v);
                if *lower {
                    format!("LOWER({p})")
                } else {
                    p
                }
            };
            if i + 1 == keys.len() {
                cond += &format!("{col} > {}", val());
            } else {
                let gt = val();
                let eq = val();
                cond += &format!("({col} > {gt} OR ({col} = {eq} AND ");
                close += "))";
            }
        }
        sql += &format!(" WHERE {cond}{close}");
    }
    sql += &format!(
        " ORDER BY LOWER(s), LOWER(n), s, n, k LIMIT {}",
        size(q) + 1
    );
    Some(sql)
}

/// A row `s, n, k` as an object (`''`: no schema).
pub(crate) fn object(r: &[Option<String>]) -> Option<DbObject> {
    let at = |i: usize| r.get(i).cloned().flatten();
    Some(DbObject {
        schema: at(0).filter(|s| !s.is_empty()),
        name: at(1)?,
        kind: at(2)?,
        parent: None,
    })
}

// -- BigQuery's sources --------------------------------------------------------------

impl BigQuerySession {
    /// A page of the dataset's objects: the same tables, views,
    /// materialized views, functions and procedures as `list_objects`,
    /// from the dataset's `INFORMATION_SCHEMA` (see the module's notes).
    pub(crate) async fn objects_page(&mut self, q: &ObjectQuery) -> Result<ObjectPage> {
        let Some(ds) = self.dataset.clone() else {
            return Ok(ObjectPage::default());
        };
        // No object has a schema.
        if !q.no_schema && q.schema.is_some() {
            return Ok(ObjectPage::default());
        }
        if self.no_catalog_pages {
            return Err(unsupported());
        }
        let base = format!(
            "{}.{}.INFORMATION_SCHEMA",
            quote_ident(Quote::Backtick, &self.api.project),
            quote_ident(Quote::Backtick, &ds)
        );
        match self
            .page_from(&format!("{base}.TABLES"), &format!("{base}.ROUTINES"), q)
            .await
        {
            Err(Error::Query(e)) => {
                tracing::info!(
                    "bigquery: INFORMATION_SCHEMA pages unavailable, listing whole: {e}"
                );
                self.no_catalog_pages = true;
                Err(unsupported())
            }
            r => r,
        }
    }

    /// The page out of `tables` and `routines` (the INFORMATION_SCHEMA
    /// views, or tables with their columns in the tests).
    pub(crate) async fn page_from(
        &self,
        tables: &str,
        routines: &str,
        q: &ObjectQuery,
    ) -> Result<ObjectPage> {
        let (tables, routines) = (
            format!("{tables} WHERE TRUE"),
            format!("{routines} WHERE TRUE"),
        );
        let t = Source {
            distinct: false,
            schema: "''",
            name: "table_name",
            kind: "CASE table_type WHEN 'VIEW' THEN 'view' \
                   WHEN 'MATERIALIZED VIEW' THEN 'materialized_view' ELSE 'table' END",
            from: &tables,
            kinds: &[kinds::TABLE, kinds::VIEW, kinds::MATERIALIZED_VIEW],
        };
        let r = Source {
            distinct: false,
            schema: "''",
            name: "routine_name",
            kind: "IF(routine_type = 'PROCEDURE', 'procedure', 'function')",
            from: &routines,
            kinds: &[kinds::FUNCTION, kinds::PROCEDURE],
        };
        let after = after(q);
        let mut b = Binds::default();
        let Some(sql) = query(&[&t, &r], q, after.as_ref(), &mut b) else {
            return Ok(ObjectPage::default());
        };
        let params = (!b.0.is_empty()).then(|| {
            Json::Array(
                b.0.iter()
                    .enumerate()
                    .map(|(i, v)| {
                        json!({ "name": format!("p{i}"), "parameterType": { "type": "STRING" },
                                "parameterValue": { "value": v } })
                    })
                    .collect(),
            )
        });
        let res = self.query(&sql, size(q) + 1, params).await?;
        let found = res
            .rows
            .iter()
            .filter_map(|row| {
                let cells: Vec<Option<String>> = (0..3)
                    .map(|i| {
                        row.pointer(&format!("/f/{i}/v"))
                            .and_then(Json::as_str)
                            .map(str::to_string)
                    })
                    .collect();
                object(&cells)
            })
            .collect();
        Ok(merge(q, after.as_ref(), found))
    }
}

/// What the default answers: the app then pages `list_objects` itself.
fn unsupported() -> Error {
    Error::Unsupported("este motor todavía no lista sus objetos por páginas".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{gcp, Api};
    use dbine_driver::objects::page_of;
    use dbine_driver::ConnectionConfig;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn obj(kind: &str, name: &str) -> DbObject {
        DbObject {
            kind: kind.into(),
            schema: None,
            name: name.into(),
            parent: None,
        }
    }

    fn session(base: &str, project: &str, dataset: &str) -> BigQuerySession {
        let http = reqwest::Client::new();
        let cfg = ConnectionConfig {
            options: [("endpoint_url".to_string(), base.to_string())].into(),
            ..Default::default()
        };
        BigQuerySession {
            api: Api {
                tokens: gcp::Tokens::from_config(&cfg, http.clone()).unwrap(),
                http,
                base: base.into(),
                project: project.into(),
                location: None,
                emulator: true,
            },
            dataset: Some(dataset.into()),
            session_id: None,
            job: Arc::new(Mutex::new(None)),
            storage: Default::default(),
            profiler: None,
            no_catalog_pages: false,
        }
    }

    type Handler = Arc<dyn Fn(&str, &str) -> (u16, Json) + Send + Sync>;

    /// A one-request-per-connection HTTP server: `h(path, body)`.
    async fn serve(h: Handler) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = l.accept().await {
                let h = h.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 8192];
                    loop {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                            continue;
                        };
                        let head = String::from_utf8_lossy(&buf[..end]).to_string();
                        let len: usize = head
                            .to_ascii_lowercase()
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:").map(str::to_string))
                            .map_or(0, |v| v.trim().parse().unwrap());
                        if buf.len() >= end + 4 + len {
                            let path = head.split(' ').nth(1).unwrap_or("").to_string();
                            let body = String::from_utf8_lossy(&buf[end + 4..end + 4 + len]);
                            let (status, json) = h(&path, &body);
                            let text = json.to_string();
                            let resp = format!(
                                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                                text.len()
                            );
                            let _ = sock.write_all(resp.as_bytes()).await;
                            return;
                        }
                    }
                });
            }
        });
        format!("http://{addr}")
    }

    fn row(cells: &[&str]) -> Json {
        json!({ "f": cells.iter().map(|c| json!({ "v": c })).collect::<Vec<_>>() })
    }

    /// One query on the dataset's INFORMATION_SCHEMA with every value as a
    /// named parameter; its rows become the page and the extra row the
    /// cursor. A query the server refuses (no INFORMATION_SCHEMA, no
    /// permission for jobs) answers Unsupported, then and from then on
    /// without asking; a schema's page is empty without asking.
    #[tokio::test]
    async fn one_query_per_page_and_unsupported_where_it_cannot_run() {
        let seen = Arc::new(Mutex::new(Vec::<Json>::new()));
        let fail = Arc::new(Mutex::new(false));
        let (s2, f2) = (seen.clone(), fail.clone());
        let base = serve(Arc::new(move |path, body| {
            assert_eq!(path, "/bigquery/v2/projects/p/queries");
            s2.lock().unwrap().push(serde_json::from_str(body).unwrap());
            if *f2.lock().unwrap() {
                return (400, json!({ "error": { "message": "Table not found" } }));
            }
            let rows = [row(&["", "x'%a", "table"]), row(&["", "X'%B", "view"]), row(&["", "x'%c", "function"])];
            (200, json!({ "jobComplete": true, "jobReference": { "jobId": "j" },
                          "schema": { "fields": [{ "name": "s" }, { "name": "n" }, { "name": "k" }] },
                          "rows": rows }))
        }))
        .await;
        let mut s = session(&base, "p", "ds");
        let q = ObjectQuery {
            pattern: Some("x'%".into()),
            not_kinds: vec![kinds::PROCEDURE.into()],
            limit: 2,
            ..Default::default()
        };
        let page = s.objects_page(&q).await.unwrap();
        let names: Vec<&str> = page.objects.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(names, ["x'%a", "X'%B"]);
        assert_eq!(page.next.as_deref(), Some(r#"[null,"X'%B","view"]"#));
        {
            let seen = seen.lock().unwrap();
            let sql = seen[0]["query"].as_str().unwrap();
            assert!(sql.contains("FROM `p`.`ds`.INFORMATION_SCHEMA.TABLES WHERE TRUE AND STRPOS(LOWER(table_name), LOWER(@p0)) > 0"), "{sql}");
            assert!(sql.contains("FROM `p`.`ds`.INFORMATION_SCHEMA.ROUTINES WHERE TRUE AND IF(routine_type = 'PROCEDURE', 'procedure', 'function') IN ('function')"), "{sql}");
            assert!(sql.ends_with("LIMIT 3") && !sql.contains("x'"), "{sql}");
            assert_eq!(seen[0]["parameterMode"], "NAMED");
            assert_eq!(
                seen[0]["queryParameters"][0]["parameterValue"]["value"],
                "x'%"
            );
        }
        // A schema: nothing has one, and nothing is asked.
        let none = s
            .objects_page(&ObjectQuery {
                schema: Some("ds".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert!(none.objects.is_empty() && none.next.is_none());
        assert_eq!(seen.lock().unwrap().len(), 1);
        // Refused: Unsupported, and the next pages don't ask again.
        *fail.lock().unwrap() = true;
        for _ in 0..2 {
            let e = s.objects_page(&ObjectQuery::default()).await.unwrap_err();
            assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
        }
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    /// The same query against bigquery-emulator, over tables shaped as the
    /// INFORMATION_SCHEMA views (the emulator has none), page by page with
    /// small pages: equal to what the app pages out of the whole list. The
    /// real views answer Unsupported there, so the app lists as before.
    ///
    ///   docker start dbine-test-bigquery   # -p 25302:9050, --project=test --dataset=ds1
    ///   DBINE_TEST_BIGQUERY_URL=http://localhost:25302 cargo test -p dbine-driver-bigquery paging -- --ignored
    #[tokio::test]
    #[ignore]
    async fn pages_match_the_whole_list_on_the_emulator() {
        let Ok(url) = std::env::var("DBINE_TEST_BIGQUERY_URL") else {
            return;
        };
        let mut s = session(&url, "test", "ds1");
        let (tv, rv) = ("dbine_page_tables", "dbine_page_routines");
        for t in [tv, rv] {
            let _ = s
                .query(&format!("DROP TABLE IF EXISTS ds1.{t}"), 1, None)
                .await;
        }
        s.query(
            &format!("CREATE TABLE ds1.{tv} (table_name STRING, table_type STRING)"),
            1,
            None,
        )
        .await
        .unwrap();
        s.query(
            &format!("CREATE TABLE ds1.{rv} (routine_name STRING, routine_type STRING)"),
            1,
            None,
        )
        .await
        .unwrap();
        let mut whole = Vec::new();
        let mut rows = Vec::new();
        for i in 0..23 {
            let name = if i % 2 == 0 {
                format!("Events_{i:02}")
            } else {
                format!("events_{i:02}_x")
            };
            rows.push(format!("('{name}', 'BASE TABLE')"));
            whole.push(obj(kinds::TABLE, &name));
        }
        for (name, ty, kind) in [
            ("V_events", "VIEW", kinds::VIEW),
            ("mv_events", "MATERIALIZED VIEW", kinds::MATERIALIZED_VIEW),
            ("snap", "SNAPSHOT", kinds::TABLE),
            ("ext_%_x", "EXTERNAL", kinds::TABLE),
        ] {
            rows.push(format!("('{name}', '{ty}')"));
            whole.push(obj(kind, name));
        }
        s.query(
            &format!(
                "INSERT INTO ds1.{tv} (table_name, table_type) VALUES {}",
                rows.join(", ")
            ),
            1,
            None,
        )
        .await
        .unwrap();
        // A function and a procedure that share a name (the function first,
        // as the tree orders them).
        let routines = [
            ("f_events", "FUNCTION", kinds::FUNCTION),
            ("p", "FUNCTION", kinds::FUNCTION),
            ("p", "PROCEDURE", kinds::PROCEDURE),
            ("tvf", "TABLE FUNCTION", kinds::FUNCTION),
        ];
        let values: Vec<String> = routines
            .iter()
            .map(|(n, t, _)| format!("('{n}', '{t}')"))
            .collect();
        s.query(
            &format!(
                "INSERT INTO ds1.{rv} (routine_name, routine_type) VALUES {}",
                values.join(", ")
            ),
            1,
            None,
        )
        .await
        .unwrap();
        whole.extend(routines.iter().map(|(n, _, k)| obj(k, n)));

        let names = |objs: &[DbObject]| -> Vec<String> {
            objs.iter()
                .map(|o| format!("{}:{}", o.kind, o.name))
                .collect()
        };
        let with = |f: &dyn Fn(&mut ObjectQuery)| {
            let mut q = ObjectQuery {
                limit: 4,
                ..Default::default()
            };
            f(&mut q);
            q
        };
        let cases = [
            with(&|q| q.kind = Some(kinds::TABLE.into())),
            with(&|q| {
                q.no_schema = true;
                q.kind = Some(kinds::VIEW.into());
            }),
            with(&|q| q.pattern = Some("EVENTS".into())),
            with(&|q| q.pattern = Some("%".into())),
            with(&|q| q.name = Some("P".into())),
            with(&|q| {
                q.name = Some("p".into());
                q.limit = 1;
            }),
            with(&|q| q.not_kinds = vec![kinds::TABLE.into(), kinds::VIEW.into()]),
            with(&|q| q.kind = Some(kinds::PROCEDURE.into())),
            with(&|q| q.schema = Some("ds1".into())),
            with(&|q| q.limit = 5),
        ];
        let (tq, rq) = (
            format!("`test`.`ds1`.`{tv}`"),
            format!("`test`.`ds1`.`{rv}`"),
        );
        let mut longest = 0;
        for q in cases {
            let mut q = q;
            let mut offset = q.clone();
            let mut pages = 0;
            loop {
                let got = if !q.no_schema && q.schema.is_some() {
                    s.objects_page(&q).await.unwrap()
                } else {
                    s.page_from(&tq, &rq, &q).await.unwrap()
                };
                let want = page_of(&whole, &offset);
                assert_eq!(names(&got.objects), names(&want.objects), "{q:?}");
                assert_eq!(got.next.is_some(), want.next.is_some(), "{q:?}");
                pages += 1;
                match (got.next, want.next) {
                    (Some(a), Some(b)) => {
                        q.after = Some(a);
                        offset.after = Some(b);
                    }
                    _ => break,
                }
            }
            longest = longest.max(pages);
        }
        assert!(longest >= 6, "the tables span pages: {longest}");

        // The emulator has no INFORMATION_SCHEMA: Unsupported, so the app
        // pages list_objects.
        let e = s.objects_page(&ObjectQuery::default()).await.unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{e:?}");

        for t in [tv, rv] {
            s.query(&format!("DROP TABLE ds1.{t}"), 1, None)
                .await
                .unwrap();
        }
    }
}
