//! `list_objects_page` (see `dbine_driver::objects` and docs/drivers.md,
//! "Objects by page"): the tree's order and cursor, and one catalog query
//! for a page of the sources a query needs.
//!
//! - **Order:** schema (none first), then name, both ignoring case, then as
//!   written, then the kind (a function and a procedure may share a name):
//!   `ORDER BY LOWER(s), LOWER(n), s, n, k` on the server, the same key here.
//! - **Cursor:** the page's last object (`[schema, name, kind]`); the next
//!   page asks for what sorts after it (keyset), so pages neither skip nor
//!   repeat objects even when the catalog changes in between.
//! - **Page:** each query asks for one object more than the page, which
//!   tells whether more remain.

use dbine_driver::{DbObject, ObjectPage, ObjectQuery};

/// The largest page asked of the server: a bigger `limit` gets pages of
/// this size (a page holds at most the limit).
pub(crate) const MAX_PAGE: usize = 10_000;

/// A bound value's placeholder (the `n`th, from 0).
fn placeholder(_n: usize) -> String {
    "?".into()
}

/// `needle` inside `hay` (both already lowercased).
fn contains(hay: &str, needle: &str) -> String {
    format!("POSITION({needle} IN {hay}) > 0")
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

/// The page out of what the sources found: each catalog query's rows (at
/// most one more than the page) and whole lists already filtered with
/// `q.matches`. Sorted, past the cursor, without repeats, cut at the page;
/// the cursor is the last object kept. What's shown is checked against the
/// query once more (a server's lowercase may differ in rare letters).
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

/// A name or a search as a `LIKE` pattern that matches at least what it
/// asks for: `%`, `_` and `\` become `_` (any one character), so no escape
/// is needed and the exact check is [`ObjectQuery::matches`]'.
pub(crate) fn like(q: &ObjectQuery) -> Option<String> {
    let widen = |t: &str| -> String {
        t.chars()
            .map(|c| {
                if matches!(c, '%' | '_' | '\\') {
                    '_'
                } else {
                    c
                }
            })
            .collect()
    };
    match (&q.name, q.pattern.as_deref().filter(|p| !p.is_empty())) {
        (Some(n), _) => Some(widen(n)),
        (None, Some(p)) => Some(format!("%{}%", widen(p))),
        (None, None) => None,
    }
}

// -- Snowflake's sources -------------------------------------------------------------

use crate::{ddl, SnowflakeSession};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{kinds, Result};

const TABLE_KINDS: &[&str] = &[kinds::TABLE, kinds::VIEW, kinds::MATERIALIZED_VIEW];
const TABLE_KIND: &str = "CASE table_type WHEN 'VIEW' THEN 'view' \
     WHEN 'MATERIALIZED VIEW' THEN 'materialized_view' ELSE 'table' END";

impl SnowflakeSession {
    /// A page of the database's objects: the same lists as `list_objects`
    /// (the `INFORMATION_SCHEMA` views in one query; streams and tasks with
    /// `SHOW … IN SCHEMA`), narrowed to the query on the server.
    pub(crate) async fn objects_page(&self, q: &ObjectQuery) -> Result<ObjectPage> {
        // Every object is in a schema.
        let Some(db) = self.ctx.database.clone().filter(|_| !q.no_schema) else {
            return Ok(ObjectPage::default());
        };
        let after = after(q);
        let dbq = qualified_name(Quote::Double, None, &db);
        let is = format!("{dbq}.INFORMATION_SCHEMA");
        let from =
            |view: &str, col: &str| format!("{is}.{view} WHERE {col} <> 'INFORMATION_SCHEMA'");
        let (tables, functions, procedures, sequences) = (
            from("TABLES", "table_schema"),
            from("FUNCTIONS", "function_schema"),
            from("PROCEDURES", "procedure_schema"),
            from("SEQUENCES", "sequence_schema"),
        );
        // list_objects reads the tables or fails; the rest are optional.
        let sources = [
            (
                Source {
                    distinct: false,
                    schema: "table_schema",
                    name: "table_name",
                    kind: TABLE_KIND,
                    from: &tables,
                    kinds: TABLE_KINDS,
                },
                true,
            ),
            (
                Source {
                    distinct: true,
                    schema: "function_schema",
                    name: "function_name",
                    kind: "'function'",
                    from: &functions,
                    kinds: &[kinds::FUNCTION],
                },
                false,
            ),
            (
                Source {
                    distinct: true,
                    schema: "procedure_schema",
                    name: "procedure_name",
                    kind: "'procedure'",
                    from: &procedures,
                    kinds: &[kinds::PROCEDURE],
                },
                false,
            ),
            (
                Source {
                    distinct: false,
                    schema: "sequence_schema",
                    name: "sequence_name",
                    kind: "'sequence'",
                    from: &sequences,
                    kinds: &[kinds::SEQUENCE],
                },
                false,
            ),
        ];
        let active: Vec<&(Source, bool)> = sources
            .iter()
            .filter(|(s, _)| !wanted(q, s.kinds).is_empty())
            .collect();
        let all: Vec<&Source> = active.iter().map(|(s, _)| s).collect();
        let mut found = match self.page_rows(&all, q, after.as_ref()).await {
            Ok(rows) => rows,
            // One source at a time, so a view that fails leaves out only
            // its objects (as in list_objects).
            Err(e) if active.len() == 1 && !active[0].1 => {
                tracing::debug!("snowflake objects page: {e}");
                Vec::new()
            }
            Err(e) if active.len() == 1 => return Err(e),
            Err(_) => {
                let mut rows = Vec::new();
                for (src, required) in &active {
                    match self.page_rows(&[src], q, after.as_ref()).await {
                        Ok(r) => rows.extend(r),
                        Err(e) if *required => return Err(e),
                        Err(e) => tracing::debug!("snowflake objects page: {e}"),
                    }
                }
                rows
            }
        };
        // Streams and tasks: few per schema; SHOW narrowed to the schema
        // and the name (LIKE ignores case), ordered and paged here.
        let scope = match &q.schema {
            Some(s) => format!("SCHEMA {}", qualified_name(Quote::Double, Some(&db), s)),
            None => format!("DATABASE {dbq}"),
        };
        let like = like(q)
            .map(|p| format!(" LIKE '{}'", p.replace('\'', "''")))
            .unwrap_or_default();
        for (what, kind) in [("STREAMS", kinds::STREAM), ("TASKS", ddl::TASK)] {
            if wanted(q, &[kind]).is_empty() {
                continue;
            }
            match self
                .named_rows(&format!("SHOW {what}{like} IN {scope}"))
                .await
            {
                Ok(rows) => found.extend(
                    rows.iter()
                        .map(|r| DbObject {
                            kind: kind.into(),
                            schema: r.get("schema_name").cloned(),
                            name: r.get("name").cloned().unwrap_or_default(),
                            parent: None,
                        })
                        .filter(|o| q.matches(o)),
                ),
                Err(e) => tracing::debug!("snowflake SHOW {what}: {e}"),
            }
        }
        Ok(merge(q, after.as_ref(), found))
    }

    async fn page_rows(
        &self,
        sources: &[&Source<'_>],
        q: &ObjectQuery,
        after: Option<&DbObject>,
    ) -> Result<Vec<DbObject>> {
        let mut b = Binds::default();
        let Some(sql) = query(sources, q, after, &mut b) else {
            return Ok(Vec::new());
        };
        let args: Vec<&str> = b.0.iter().map(String::as_str).collect();
        let rows = self.text_rows(&sql, &args).await?;
        Ok(rows.iter().filter_map(|r| object(r)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> DbObject {
        DbObject {
            kind: kind.into(),
            schema: schema.map(Into::into),
            name: name.into(),
            parent: None,
        }
    }

    const TABLES: Source = Source {
        distinct: false,
        schema: "table_schema",
        name: "table_name",
        kind: "CASE table_type WHEN 'VIEW' THEN 'view' ELSE 'table' END",
        from: "X.TABLES WHERE table_schema <> 'INFORMATION_SCHEMA'",
        kinds: &["table", "view"],
    };
    const FUNCTIONS: Source = Source {
        distinct: true,
        schema: "function_schema",
        name: "function_name",
        kind: "'function'",
        from: "X.FUNCTIONS WHERE TRUE",
        kinds: &["function"],
    };

    /// Every filter becomes a condition with its value bound, the cursor a
    /// keyset in the tree's order, the page one more than asked.
    #[test]
    fn a_query_binds_every_filter_and_the_cursor() {
        let q = ObjectQuery {
            schema: Some("S'1".into()),
            kind: Some("view".into()),
            pattern: Some("o'%".into()),
            name: Some("Ord".into()),
            limit: 2,
            ..Default::default()
        };
        let a = obj("view", Some("S'1"), "a");
        let mut b = Binds::default();
        let sql = query(&[&TABLES, &FUNCTIONS], &q, Some(&a), &mut b).unwrap();
        assert_eq!(
            sql,
            "SELECT s, n, k FROM (SELECT table_schema AS s, table_name AS n, \
             CASE table_type WHEN 'VIEW' THEN 'view' ELSE 'table' END AS k \
             FROM X.TABLES WHERE table_schema <> 'INFORMATION_SCHEMA' AND table_schema = ? \
             AND CASE table_type WHEN 'VIEW' THEN 'view' ELSE 'table' END IN ('view') \
             AND POSITION(LOWER(?) IN LOWER(table_name)) > 0 AND LOWER(table_name) = LOWER(?)) t \
             WHERE (LOWER(s) > LOWER(?) OR (LOWER(s) = LOWER(?) AND (LOWER(n) > LOWER(?) OR \
             (LOWER(n) = LOWER(?) AND (s > ? OR (s = ? AND (n > ? OR (n = ? AND k > ?)))))))) \
             ORDER BY LOWER(s), LOWER(n), s, n, k LIMIT 3"
        );
        assert_eq!(
            b.0,
            ["S'1", "o'%", "Ord", "S'1", "S'1", "a", "a", "S'1", "S'1", "a", "a", "view"]
        );
    }

    #[test]
    fn sources_without_a_wanted_kind_are_left_out() {
        let mut q = ObjectQuery {
            not_kinds: vec!["table".into(), "view".into(), "function".into()],
            ..Default::default()
        };
        assert!(query(&[&TABLES, &FUNCTIONS], &q, None, &mut Binds::default()).is_none());
        q.not_kinds = vec!["table".into()];
        let mut b = Binds::default();
        let sql = query(&[&TABLES, &FUNCTIONS], &q, None, &mut b).unwrap();
        assert!(
            sql.contains(" IN ('view') UNION ALL SELECT DISTINCT function_schema"),
            "{sql}"
        );
        assert!(b.0.is_empty());
        // An empty schema name is no schema's: nothing has it.
        q.schema = Some(String::new());
        assert!(query(&[&TABLES], &q, None, &mut Binds::default()).is_none());
    }

    /// The merge orders what every source found, ignoring case, cuts at
    /// the page and gives its last object as the cursor, which the next
    /// page starts after.
    #[test]
    fn pages_merge_sources_and_follow_the_cursor() {
        let q = ObjectQuery {
            limit: 2,
            ..Default::default()
        };
        let found = vec![
            obj("table", Some("b"), "x"),
            obj("function", Some("A"), "f"),
            obj("procedure", Some("A"), "f"),
            obj("table", Some("a"), "Z"),
            obj("table", None, "loose"),
        ];
        let p = merge(&q, None, found.clone());
        let names: Vec<_> = p
            .objects
            .iter()
            .map(|o| (o.kind.as_str(), o.name.as_str()))
            .collect();
        assert_eq!(names, [("table", "loose"), ("function", "f")]);
        let mut q2 = q.clone();
        q2.after = p.next.clone();
        let a = after(&q2).unwrap();
        assert_eq!(
            (a.kind.as_str(), a.schema.as_deref()),
            ("function", Some("A"))
        );
        let p2 = merge(&q2, Some(&a), found.clone());
        let names: Vec<_> = p2
            .objects
            .iter()
            .map(|o| (o.kind.as_str(), o.name.as_str()))
            .collect();
        assert_eq!(names, [("procedure", "f"), ("table", "Z")]);
        q2.after = p2.next.clone();
        let p3 = merge(&q2, after(&q2).as_ref(), found);
        assert_eq!(p3.objects.len(), 1);
        assert_eq!(p3.next, None);
        // A cursor from elsewhere starts from the top.
        q2.after = Some("12".into());
        assert!(after(&q2).is_none());
    }

    // ---- against a fake SQL API ----------------------------------------------------

    use serde_json::{json, Value as Json};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    type Handler = Arc<dyn Fn(&str) -> (u16, Json) + Send + Sync>;

    /// A one-request-per-connection HTTP server answering each statement
    /// (the request's JSON body) with `h`.
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
                        let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                        let len: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .map_or(0, |v| v.trim().parse().unwrap());
                        if buf.len() >= end + 4 + len {
                            let body = String::from_utf8_lossy(&buf[end + 4..end + 4 + len]);
                            let (status, json) = h(&body);
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

    fn session(base: String) -> SnowflakeSession {
        SnowflakeSession {
            api: crate::Api {
                http: reqwest::Client::new(),
                base,
                auth: crate::Auth::Pat("t".into()),
                last_error: Default::default(),
            },
            ctx: crate::Context {
                database: Some("DB".into()),
                schema: Some("PUBLIC".into()),
                warehouse: None,
                role: None,
            },
            carry: Default::default(),
            handle: Arc::new(Mutex::new(None)),
            mon: Default::default(),
            profiler: None,
            read_only: false,
        }
    }

    /// What list_objects gives for the fake database (function F before
    /// procedure F, as the tree orders a shared name).
    fn catalog() -> Vec<DbObject> {
        let mut v: Vec<DbObject> = (0..25)
            .map(|i| {
                let name = if i % 2 == 0 {
                    format!("T{i:02}")
                } else {
                    format!("t{i:02}_orders")
                };
                obj(kinds::TABLE, Some("PUBLIC"), &name)
            })
            .collect();
        v.extend([
            obj(kinds::VIEW, Some("PUBLIC"), "V_ORDERS"),
            obj(kinds::MATERIALIZED_VIEW, Some("PUBLIC"), "mv"),
            obj(kinds::TABLE, Some("Sales"), "orders"),
            obj(kinds::TABLE, Some("Sales"), "ORDERS2"),
            obj(kinds::FUNCTION, Some("PUBLIC"), "F"),
            obj(kinds::PROCEDURE, Some("PUBLIC"), "F"),
            obj(kinds::SEQUENCE, Some("PUBLIC"), "SEQ_ORDERS"),
            obj(kinds::STREAM, Some("PUBLIC"), "ST_ORDERS"),
            obj(ddl::TASK, Some("Sales"), "NIGHTLY"),
        ]);
        v
    }

    const SQL_KINDS: &[&str] = &[
        kinds::TABLE,
        kinds::VIEW,
        kinds::MATERIALIZED_VIEW,
        kinds::FUNCTION,
        kinds::PROCEDURE,
        kinds::SEQUENCE,
    ];

    /// The fake server: each `SELECT` answers what the query (`q`, set by
    /// the test) asks of the views it names, past the cursor, in the
    /// tree's order, up to its LIMIT; `SHOW` gives every stream or task of
    /// its scope (LIKE is left to the driver's own check). FUNCTIONS
    /// fails when `broken`. Records every request body.
    fn fake(q: Arc<Mutex<ObjectQuery>>, broken: bool, calls: Arc<Mutex<Vec<Json>>>) -> Handler {
        Arc::new(move |body: &str| {
            let b: Json = serde_json::from_str(body).unwrap();
            calls.lock().unwrap().push(b.clone());
            let stmt = b["statement"].as_str().unwrap().to_string();
            let q = q.lock().unwrap().clone();
            if let Some(rest) = stmt.strip_prefix("SHOW ") {
                let kind = if rest.starts_with("STREAMS") {
                    kinds::STREAM
                } else {
                    ddl::TASK
                };
                let schema = rest
                    .split("IN SCHEMA \"DB\".\"")
                    .nth(1)
                    .map(|s| s.trim_end_matches('"'));
                let data: Vec<Json> = catalog()
                    .into_iter()
                    .filter(|o| {
                        o.kind == kind && schema.is_none_or(|s| o.schema.as_deref() == Some(s))
                    })
                    .map(|o| json!(["2024-01-01", o.name, "DB", o.schema]))
                    .collect();
                let cols = ["created_on", "name", "database_name", "schema_name"];
                let row_type: Vec<Json> = cols.iter().map(|c| json!({ "name": c })).collect();
                return (
                    200,
                    json!({ "resultSetMetaData": { "rowType": row_type }, "data": data }),
                );
            }
            let views = [
                (".TABLES ", TABLE_KINDS),
                (".FUNCTIONS ", &[kinds::FUNCTION][..]),
                (".PROCEDURES ", &[kinds::PROCEDURE][..]),
                (".SEQUENCES ", &[kinds::SEQUENCE][..]),
            ];
            if broken && stmt.contains(".FUNCTIONS ") {
                return (422, json!({ "message": "FUNCTIONS unavailable" }));
            }
            let limit: usize = stmt.rsplit("LIMIT ").next().unwrap().parse().unwrap();
            let from = after(&q).map(|a| key(&a));
            let mut hits: Vec<DbObject> = catalog()
                .into_iter()
                .filter(|o| {
                    views
                        .iter()
                        .any(|(v, ks)| stmt.contains(v) && ks.contains(&o.kind.as_str()))
                        && wanted(&q, SQL_KINDS).contains(&o.kind.as_str())
                        && q.matches(o)
                        && from.as_ref().is_none_or(|f| key(o) > *f)
                })
                .collect();
            hits.sort_by_key(key);
            let data: Vec<Json> = hits
                .iter()
                .take(limit)
                .map(|o| json!([o.schema, o.name, o.kind]))
                .collect();
            (
                200,
                json!({ "resultSetMetaData": { "rowType": [] }, "data": data }),
            )
        })
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

    /// Every page of `q` from the driver, and from `page_of` over the
    /// whole list: the same objects, in the same pages. Gives the pages.
    async fn same_pages(
        s: &SnowflakeSession,
        shared: &Arc<Mutex<ObjectQuery>>,
        mut q: ObjectQuery,
        whole: &[DbObject],
    ) -> usize {
        let mut offset = q.clone();
        let mut pages = 0;
        loop {
            *shared.lock().unwrap() = q.clone();
            let got = s.objects_page(&q).await.unwrap();
            let want = dbine_driver::objects::page_of(whole, &offset);
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
            limit: 4,
            ..Default::default()
        };
        f(&mut q);
        q
    }

    /// Folders, searches, lookups and "Otros", page by page with small
    /// pages, equal to what the app pages out of list_objects; a folder
    /// sends one statement with its schema bound.
    #[tokio::test]
    async fn pages_match_the_whole_list_and_bind_the_filters() {
        let shared = Arc::new(Mutex::new(ObjectQuery::default()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let s = session(serve(fake(shared.clone(), false, calls.clone())).await);
        let whole = catalog();
        let cases = [
            with(|q| {
                q.schema = Some("PUBLIC".into());
                q.kind = Some(kinds::TABLE.into());
            }),
            with(|q| q.kind = Some(kinds::VIEW.into())),
            with(|q| q.schema = Some("Sales".into())),
            with(|q| q.pattern = Some("ORDERS".into())),
            with(|q| q.name = Some("orders".into())),
            with(|q| {
                q.pattern = Some("f".into());
                q.limit = 1;
            }),
            with(|q| q.not_kinds = vec![kinds::TABLE.into(), kinds::VIEW.into()]),
            with(|q| q.kind = Some(ddl::TASK.into())),
            with(|q| q.no_schema = true),
            with(|q| q.schema = Some("NOPE".into())),
        ];
        let mut pages = 0;
        for c in cases {
            pages += same_pages(&s, &shared, c, &whole).await;
        }
        assert!(pages > 15, "{pages}");

        calls.lock().unwrap().clear();
        let folder = with(|q| {
            q.schema = Some("PUBLIC".into());
            q.kind = Some(kinds::TABLE.into());
        });
        *shared.lock().unwrap() = folder.clone();
        s.objects_page(&folder).await.unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let stmt = calls[0]["statement"].as_str().unwrap();
        assert!(
            stmt.contains("\"DB\".INFORMATION_SCHEMA.TABLES WHERE"),
            "{stmt}"
        );
        assert!(
            !stmt.contains("FUNCTIONS") && !stmt.contains("PUBLIC"),
            "{stmt}"
        );
        assert!(
            stmt.contains("IN ('table')") && stmt.ends_with("LIMIT 5"),
            "{stmt}"
        );
        assert_eq!(
            calls[0]["bindings"]["1"],
            json!({ "type": "TEXT", "value": "PUBLIC" })
        );
    }

    /// A search's SHOW carries its pattern quoted; a view that fails
    /// leaves out only its objects, as in list_objects.
    #[tokio::test]
    async fn show_patterns_are_quoted_and_a_failing_view_is_left_out() {
        let shared = Arc::new(Mutex::new(ObjectQuery::default()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let s = session(serve(fake(shared.clone(), true, calls.clone())).await);
        let q = ObjectQuery {
            pattern: Some("o'r_".into()),
            ..Default::default()
        };
        *shared.lock().unwrap() = q.clone();
        s.objects_page(&q).await.unwrap();
        let shows: Vec<String> = calls
            .lock()
            .unwrap()
            .iter()
            .filter_map(|c| {
                c["statement"]
                    .as_str()
                    .filter(|t| t.starts_with("SHOW"))
                    .map(str::to_string)
            })
            .collect();
        assert_eq!(
            shows,
            [
                "SHOW STREAMS LIKE '%o''r_%' IN DATABASE \"DB\"",
                "SHOW TASKS LIKE '%o''r_%' IN DATABASE \"DB\""
            ]
        );
        let whole: Vec<DbObject> = catalog()
            .into_iter()
            .filter(|o| o.kind != kinds::FUNCTION)
            .collect();
        for c in [
            with(|q| q.limit = 3),
            with(|q| q.kind = Some(kinds::FUNCTION.into())),
        ] {
            same_pages(&s, &shared, c, &whole).await;
        }
    }

    #[test]
    fn like_patterns_match_at_least_what_is_asked() {
        let q = ObjectQuery {
            pattern: Some("a_b%c\\".into()),
            ..Default::default()
        };
        assert_eq!(like(&q).as_deref(), Some("%a_b_c_%"));
        let q = ObjectQuery {
            name: Some("t%".into()),
            pattern: Some("x".into()),
            ..Default::default()
        };
        assert_eq!(like(&q).as_deref(), Some("t_"));
        assert_eq!(like(&ObjectQuery::default()), None);
    }
}
