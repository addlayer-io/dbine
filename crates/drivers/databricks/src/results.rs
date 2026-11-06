//! Results over the inline limit, for the queries people run (tables are
//! read in `transfer`).
//!
//! The Statement Execution API aborts an `INLINE` statement whose result
//! passes 25 MiB, however few rows the client keeps. So:
//!
//! - A read carries `row_limit` with the rows asked for: a query over a huge
//!   table gets those rows, marked as partial, and the warehouse doesn't
//!   produce the rest.
//! - A read still over the limit (very wide rows) runs again with
//!   `EXTERNAL_LINKS`, the chunks downloaded as in `transfer`: presigned URLs
//!   to the workspace's cloud storage, fetched without the workspace's token.
//! - A read with a sink (an export, a chart, a notebook's whole result) goes
//!   by links from the start, row by row, with nothing kept in memory. A
//!   client that can't reach the storage falls back to `INLINE`, which still
//!   serves up to 25 MiB.
//!
//! Only reads run a second time: a statement that may write is left as it
//! is, with no `row_limit` and no retry.

use crate::transfer::{chunk_count, first_chunk, IN_FLIGHT};
use crate::{cell, result_columns, script, DatabricksSession};
use dbine_driver::sql::leading_keyword;
use dbine_driver::{Error, QueryOutcome, Result, ResultColumn};
use serde_json::{json, Value as Json};

/// Whether a statement only reads. `WITH … INSERT` and `FROM t INSERT …`
/// write: a `with` or `from` naming a write isn't one, even if the word was
/// in a string (it just takes the plain path).
pub(crate) fn only_reads(stmt: &str) -> bool {
    let Some(first) = leading_keyword(stmt, &script::dialect()) else {
        return false;
    };
    match first.as_str() {
        "select" | "values" | "table" | "show" | "describe" | "desc" | "explain" | "list" => true,
        "with" | "from" => !stmt
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .any(|w| {
                ["insert", "update", "delete", "merge"]
                    .iter()
                    .any(|k| w.eq_ignore_ascii_case(k))
            }),
        _ => false,
    }
}

/// The server's limit for a statement: only on reads, and only for a real
/// cap (`usize::MAX` asks for all of it).
pub(crate) fn row_limit(stmt: &str, max_rows: usize) -> Option<i64> {
    if max_rows == 0 || max_rows == usize::MAX || !only_reads(stmt) {
        return None;
    }
    i64::try_from(max_rows).ok()
}

/// The server's answer to an `INLINE` result over its limit.
fn over_inline_limit(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("inline byte limit") || m.contains("disposition=external_links")
}

/// A statement's error that is about the disposition, not about its SQL.
fn names_links(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("external_links") || m.contains("external links")
}

/// A result over the inline limit that the links can't bring either.
fn too_big(why: &str) -> Error {
    Error::Query(format!(
        "El resultado pasa los 25 MiB que Databricks entrega en la respuesta, y no se pudo leer de su almacenamiento en la nube: {why}. Probá con un LIMIT o con menos columnas."
    ))
}

/// Why a read by links ended.
enum Failed {
    /// No links, or none could be read. Nothing reached the outcome, so the
    /// plain path can still serve the statement.
    NoLinks(String),
    /// Anything else: a failed statement, a cancel, a download cut halfway.
    Error(Error),
}

impl DatabricksSession {
    /// One statement, its result into `out`, by the path its size asks for
    /// (see the module). Gives its statement id.
    pub(crate) async fn run_into(
        &self,
        sql: &str,
        max_rows: usize,
        out: &mut QueryOutcome,
    ) -> Result<String> {
        let reads = only_reads(sql);
        let mut no_links = None;
        if reads && out.sink.is_some() {
            match self.by_links(sql, max_rows, out).await {
                Ok(id) => return Ok(id),
                Err(Failed::Error(e)) => return Err(e),
                Err(Failed::NoLinks(why)) => {
                    tracing::debug!("databricks external links unavailable, reading inline: {why}");
                    no_links = Some(why);
                }
            }
        }
        match self.run_id(sql, max_rows, None).await {
            Ok((id, st)) => {
                Self::push_statement(&st, max_rows, out);
                if let Some(n) = st
                    .columns
                    .iter()
                    .position(|(c, _)| c == "num_affected_rows")
                {
                    let n = st
                        .rows
                        .first()
                        .and_then(|r| r.get(n))
                        .and_then(|v| v.as_u64().or_else(|| v.as_str()?.parse().ok()));
                    if let Some(last) = out.results.last_mut() {
                        last.rows_affected = n;
                    }
                }
                Ok(id)
            }
            Err(Error::Query(m)) if reads && over_inline_limit(&m) => {
                if let Some(why) = no_links {
                    return Err(too_big(&why));
                }
                match self.by_links(sql, max_rows, out).await {
                    Ok(id) => Ok(id),
                    Err(Failed::Error(e)) => Err(e),
                    Err(Failed::NoLinks(why)) => Err(too_big(&why)),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// A read with `EXTERNAL_LINKS`, row by row into `out`.
    async fn by_links(
        &self,
        sql: &str,
        max_rows: usize,
        out: &mut QueryOutcome,
    ) -> std::result::Result<String, Failed> {
        let mut body = self.request(sql, None);
        body["disposition"] = json!("EXTERNAL_LINKS");
        let limit = row_limit(sql, max_rows);
        if let Some(n) = limit {
            body["row_limit"] = json!(n);
        }
        // A workspace can have links disabled. Were it a real sign-in or SQL
        // problem, the plain path gives it again, as the server says it.
        let resp = match self.api.post("/api/2.0/sql/statements", &body).await {
            Ok(r) => r,
            Err(Error::Query(m) | Error::AuthFailed(m)) => return Err(Failed::NoLinks(m)),
            Err(e) => return Err(Failed::Error(e)),
        };
        let r = self.read_links(resp, limit.is_some(), max_rows, out).await;
        self.set_running(None);
        r
    }

    async fn read_links(
        &self,
        resp: Json,
        limited: bool,
        max_rows: usize,
        out: &mut QueryOutcome,
    ) -> std::result::Result<String, Failed> {
        let (id, mut resp) = match self.follow(resp).await {
            Ok(r) => r,
            // Refused once submitted instead of on submitting it.
            Err(Error::Query(m)) if names_links(&m) => return Err(Failed::NoLinks(m)),
            Err(e) => return Err(Failed::Error(e)),
        };
        let cut = resp
            .pointer("/manifest/truncated")
            .and_then(Json::as_bool)
            .unwrap_or(false);
        if cut && !limited {
            // The whole result was asked for and the server stopped short
            // (100 GiB by default): never a shorter export.
            return Err(Failed::Error(Error::Query(
                "Databricks cortó el resultado por su tamaño (el límite es de 100 GiB): no se entrega incompleto. Probá con un filtro o con menos columnas.".into(),
            )));
        }
        let columns = result_columns(&resp);
        if columns.is_empty() {
            out.push_affected(0);
            return Ok(id);
        }
        let Some((chunks, expected)) = chunk_count(&resp) else {
            return Err(Failed::NoLinks(
                "la respuesta no dice cuántos bloques tiene el resultado".into(),
            ));
        };
        let first = first_chunk(&mut resp);
        let shown: Vec<ResultColumn> = columns
            .iter()
            .map(|(n, t)| ResultColumn {
                name: n.clone(),
                type_name: t.clone(),
            })
            .collect();
        let before = out.results.len();
        // The result starts with its first row: until then a sink hasn't
        // heard of it, and the plain path can still serve the statement.
        let mut begun = false;
        let mut rows = 0u64;
        let mut stopped = false;
        let read = self
            .each_block(&id, first, chunks, IN_FLIGHT, &mut |block| {
                block.each_row(|values| {
                    if !begun {
                        out.begin_result(shown.clone());
                        begun = true;
                    }
                    let mut values = values.into_iter();
                    let row = columns
                        .iter()
                        .map(|(_, t)| {
                            let v = values.next().flatten().map_or(Json::Null, Json::String);
                            cell(t, &v)
                        })
                        .collect();
                    out.push_row(row, max_rows);
                    rows += 1;
                    // Nothing more to keep, or nowhere to send it.
                    stopped =
                        out.sink_error.is_some() || (out.sink.is_none() && rows >= max_rows as u64);
                    Ok(!stopped)
                })
            })
            .await;
        if let Err(e) = read {
            if rows == 0 && !matches!(e, Error::Cancelled) {
                return Err(Failed::NoLinks(e.to_string()));
            }
            if out.sink.is_none() {
                out.results.truncate(before);
            }
            return Err(Failed::Error(e));
        }
        if !begun {
            out.begin_result(shown);
        }
        if !stopped {
            if let Some(n) = expected.filter(|n| *n != rows) {
                return Err(Failed::Error(Error::Query(format!(
                    "Databricks anunció {n} filas del resultado y llegaron {rows}: no se entrega incompleto."
                ))));
            }
        }
        if let Some(last) = out.results.last_mut() {
            let total = expected.unwrap_or(rows).max(rows);
            last.total_rows = last.total_rows.max(total);
            last.truncated |= cut || (out.sink.is_none() && total > rows);
        }
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::tests::{serve, session};
    use dbine_driver::{RowSink, RowSinkRef, Session};
    use std::sync::{Arc, Mutex};

    const OVER: &str = "Inline byte limit exceeded. Statements executed with disposition=INLINE can have a result size of at most 26214400 bytes. Please execute the statement with disposition=EXTERNAL_LINKS if you want to download the full result.";

    fn succeeded(manifest: Json, result: Json) -> String {
        json!({ "statement_id": "s1", "status": { "state": "SUCCEEDED" }, "manifest": manifest, "result": result }).to_string()
    }

    fn failed() -> String {
        json!({ "statement_id": "s0", "status": { "state": "FAILED", "error": { "message": OVER } } })
            .to_string()
    }

    fn manifest(rows: u64, chunks: u64, truncated: bool) -> Json {
        json!({
            "schema": { "columns": [
                { "name": "id", "type_name": "INT" },
                { "name": "txt", "type_name": "STRING" },
            ] },
            "total_row_count": rows,
            "total_chunk_count": chunks,
            "truncated": truncated,
        })
    }

    /// What the server saw, and its answers by disposition. The chunks live at
    /// `/data/<n>?sig=SECRET`, the links `links` builds.
    #[derive(Clone, Default)]
    struct Seen(Arc<Mutex<Vec<Json>>>);

    impl Seen {
        fn posts(&self) -> Vec<Json> {
            self.0.lock().unwrap().clone()
        }
    }

    fn links(base: &str, n: u64) -> Json {
        json!({
            "chunk_index": n,
            "external_links": [{ "external_link": format!("{base}/data/{n}?sig=SECRET"), "http_headers": {} }],
        })
    }

    /// A server whose `INLINE` is over the limit (or `inline`, when given) and
    /// whose links are `manifest` and these chunks.
    async fn server(
        seen: &Seen,
        inline: Option<String>,
        by_links: Option<(Json, Vec<&'static str>)>,
        base_of_links: Option<&'static str>,
    ) -> String {
        let seen = seen.clone();
        let base = Arc::new(Mutex::new(String::new()));
        let shared = base.clone();
        let url = serve(Arc::new(move |method: &str, path: &str, body: &str| {
            let base = base_of_links
                .map(str::to_string)
                .unwrap_or_else(|| shared.lock().unwrap().clone());
            if method == "POST" && path == "/api/2.0/sql/statements" {
                let b: Json = serde_json::from_str(body).unwrap();
                let external = b["disposition"] == "EXTERNAL_LINKS";
                seen.0.lock().unwrap().push(b);
                return if external {
                    match &by_links {
                        Some((m, _)) => (200, succeeded(m.clone(), links(&base, 0))),
                        None => (
                            400,
                            json!({ "message": "EXTERNAL_LINKS is disabled" }).to_string(),
                        ),
                    }
                } else {
                    (200, inline.clone().unwrap_or_else(failed))
                };
            }
            if let Some(n) = path.strip_prefix("/api/2.0/sql/statements/s1/result/chunks/") {
                return (200, links(&base, n.parse().unwrap()).to_string());
            }
            if let Some(rest) = path.strip_prefix("/data/") {
                let n: usize = rest.split('?').next().unwrap().parse().unwrap();
                let chunks = &by_links.as_ref().unwrap().1;
                return (200, chunks[n].to_string());
            }
            (
                404,
                json!({ "message": format!("nada en {path}") }).to_string(),
            )
        }))
        .await;
        *base.lock().unwrap() = url.clone();
        url
    }

    #[derive(Default)]
    struct Collected {
        begun: Vec<usize>,
        rows: Vec<Vec<Json>>,
    }

    impl RowSink for Collected {
        fn begin(&mut self, index: usize, _: &[ResultColumn]) -> std::io::Result<()> {
            self.begun.push(index);
            Ok(())
        }
        fn row(&mut self, _: usize, row: &[Json]) -> std::io::Result<()> {
            self.rows.push(row.to_vec());
            Ok(())
        }
    }

    fn with_sink() -> (QueryOutcome, Arc<Mutex<Collected>>) {
        let sink = Arc::new(Mutex::new(Collected::default()));
        let out = QueryOutcome {
            sink: Some(RowSinkRef(sink.clone())),
            ..Default::default()
        };
        (out, sink)
    }

    #[test]
    fn only_reads_take_a_limit() {
        for read in [
            "SELECT 1",
            "  select * from t",
            "-- note\nSELECT 1",
            "WITH a AS (SELECT 1) SELECT * FROM a",
            "FROM t SELECT id",
            "VALUES (1)",
            "TABLE t",
            "SHOW TABLES",
            "DESCRIBE t",
            "DESC t",
            "EXPLAIN SELECT 1",
        ] {
            assert!(only_reads(read), "{read}");
            assert_eq!(row_limit(read, 5000), Some(5000), "{read}");
            assert_eq!(row_limit(read, usize::MAX), None, "{read}");
            assert_eq!(row_limit(read, 0), None, "{read}");
        }
        for other in [
            "INSERT INTO t VALUES (1)",
            "WITH a AS (SELECT 1) INSERT INTO t SELECT * FROM a",
            "FROM s INSERT INTO t SELECT id",
            "MERGE INTO t USING s ON a = b WHEN MATCHED THEN DELETE",
            "UPDATE t SET a = 1",
            "DELETE FROM t",
            "CALL p()",
            "CREATE TABLE t AS SELECT 1",
            "OPTIMIZE t",
            "USE c.s",
            "",
        ] {
            assert!(!only_reads(other), "{other}");
            assert_eq!(row_limit(other, 5000), None, "{other}");
        }
    }

    #[test]
    fn the_servers_refusal_is_recognized() {
        assert!(over_inline_limit(OVER));
        assert!(!over_inline_limit(
            "[TABLE_OR_VIEW_NOT_FOUND] The table or view `keys` cannot be found."
        ));
    }

    #[tokio::test]
    async fn a_result_within_the_limit_stays_inline_with_its_row_limit() {
        let seen = Seen::default();
        let inline = succeeded(
            manifest(2, 1, true),
            json!({ "chunk_index": 0, "data_array": [["1", "a"], ["2", null]] }),
        );
        let base = server(&seen, Some(inline), None, None).await;
        let mut out = QueryOutcome::default();
        session(base)
            .execute("SELECT id, txt FROM t", 2, &mut out)
            .await
            .unwrap();
        let posts = seen.posts();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0]["disposition"], "INLINE");
        assert_eq!(posts[0]["row_limit"], 2);
        let r = &out.results[0];
        assert_eq!(
            r.rows,
            vec![vec![json!(1), json!("a")], vec![json!(2), Json::Null]]
        );
        assert!(r.truncated, "the server said there were more");
    }

    #[tokio::test]
    async fn a_write_never_gets_a_limit_or_runs_twice() {
        let seen = Seen::default();
        let base = server(&seen, None, None, None).await;
        let mut out = QueryOutcome::default();
        let e = session(base)
            .execute("CALL p()", 5000, &mut out)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("Inline byte limit"), "{e}");
        let posts = seen.posts();
        assert_eq!(posts.len(), 1, "it must run only once");
        assert!(posts[0].get("row_limit").is_none());
        assert_eq!(posts[0]["disposition"], "INLINE");
    }

    #[tokio::test]
    async fn a_result_over_the_limit_comes_by_links() {
        let seen = Seen::default();
        let chunks = vec![r#"[["1","a"],["2",null]]"#, r#"[["3","c"]]"#];
        let base = server(&seen, None, Some((manifest(3, 2, false), chunks)), None).await;
        let mut out = QueryOutcome::default();
        session(base)
            .execute("SELECT id, txt FROM t", 5000, &mut out)
            .await
            .unwrap();
        let posts = seen.posts();
        assert_eq!(posts.len(), 2);
        assert_eq!(posts[0]["disposition"], "INLINE");
        assert_eq!(posts[1]["disposition"], "EXTERNAL_LINKS");
        assert_eq!(posts[1]["row_limit"], 5000);
        let r = &out.results[0];
        assert_eq!(
            r.rows,
            vec![
                vec![json!(1), json!("a")],
                vec![json!(2), Json::Null],
                vec![json!(3), json!("c")],
            ]
        );
        assert_eq!(r.total_rows, 3);
        assert!(!r.truncated);
    }

    #[tokio::test]
    async fn links_stop_at_the_rows_asked_for_and_say_it_was_partial() {
        let seen = Seen::default();
        let chunks = vec![r#"[["1","a"],["2","b"],["3","c"]]"#];
        let base = server(&seen, None, Some((manifest(2, 1, true), chunks)), None).await;
        let mut out = QueryOutcome::default();
        session(base)
            .execute("SELECT id, txt FROM t", 2, &mut out)
            .await
            .unwrap();
        let r = &out.results[0];
        assert_eq!(r.rows.len(), 2);
        assert!(r.truncated);
    }

    #[tokio::test]
    async fn a_sink_goes_by_links_from_the_start_with_all_the_rows() {
        let seen = Seen::default();
        let chunks = vec![r#"[["1","a"]]"#, r#"[["2","b"]]"#];
        let base = server(&seen, None, Some((manifest(2, 2, false), chunks)), None).await;
        let (mut out, sink) = with_sink();
        session(base)
            .execute("SELECT id, txt FROM t", usize::MAX, &mut out)
            .await
            .unwrap();
        let posts = seen.posts();
        assert_eq!(posts.len(), 1, "no INLINE attempt first");
        assert_eq!(posts[0]["disposition"], "EXTERNAL_LINKS");
        assert!(posts[0].get("row_limit").is_none());
        let sink = sink.lock().unwrap();
        assert_eq!(sink.begun, vec![0]);
        assert_eq!(sink.rows.len(), 2);
        assert_eq!(sink.rows[1], vec![json!(2), json!("b")]);
        assert!(out.results[0].rows.is_empty(), "a sink keeps nothing here");
        assert_eq!(out.results[0].total_rows, 2);
        assert!(!out.results[0].truncated);
    }

    #[tokio::test]
    async fn a_sink_that_cant_reach_the_storage_falls_back_inline_once() {
        let seen = Seen::default();
        let inline = succeeded(
            manifest(1, 1, false),
            json!({ "chunk_index": 0, "data_array": [["1", "a"]] }),
        );
        let base = server(
            &seen,
            Some(inline),
            Some((manifest(1, 1, false), vec![])),
            Some("http://127.0.0.1:1"),
        )
        .await;
        let (mut out, sink) = with_sink();
        session(base)
            .execute("SELECT id, txt FROM t", usize::MAX, &mut out)
            .await
            .unwrap();
        let sink = sink.lock().unwrap();
        assert_eq!(sink.begun, vec![0], "only one header");
        assert_eq!(sink.rows, vec![vec![json!(1), json!("a")]]);
    }

    #[tokio::test]
    async fn links_refused_and_too_big_says_what_to_do() {
        let seen = Seen::default();
        let base = server(&seen, None, None, None).await;
        let mut out = QueryOutcome::default();
        let e = session(base)
            .execute("SELECT * FROM t", 5000, &mut out)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("25 MiB") && e.contains("LIMIT"), "{e}");
        assert!(e.contains("EXTERNAL_LINKS is disabled"), "{e}");
        assert!(out.results.is_empty());
    }

    #[tokio::test]
    async fn links_disabled_still_serve_a_small_export_inline() {
        for refusal in [
            (400, json!({ "message": "EXTERNAL_LINKS is disabled" }).to_string()),
            (403, json!({ "message": "not allowed" }).to_string()),
            (
                200,
                json!({ "statement_id": "s0", "status": { "state": "FAILED", "error": { "message": "disposition EXTERNAL_LINKS is not enabled" } } })
                    .to_string(),
            ),
        ] {
            let inline = succeeded(
                manifest(1, 1, false),
                json!({ "chunk_index": 0, "data_array": [["1", "a"]] }),
            );
            let base = serve(Arc::new(move |_: &str, _: &str, body: &str| {
                let b: Json = serde_json::from_str(body).unwrap();
                if b["disposition"] == "EXTERNAL_LINKS" {
                    refusal.clone()
                } else {
                    (200, inline.clone())
                }
            }))
            .await;
            let (mut out, sink) = with_sink();
            session(base)
                .execute("SELECT id, txt FROM t", usize::MAX, &mut out)
                .await
                .unwrap();
            let sink = sink.lock().unwrap();
            assert_eq!(sink.begun, vec![0]);
            assert_eq!(sink.rows.len(), 1);
        }
    }

    #[tokio::test]
    async fn a_failed_query_by_links_says_its_own_error_and_runs_once() {
        let seen = Seen::default();
        let shown = seen.clone();
        let base = serve(Arc::new(move |_: &str, _: &str, body: &str| {
            seen.0
                .lock()
                .unwrap()
                .push(serde_json::from_str(body).unwrap());
            (
                200,
                json!({ "statement_id": "s0", "status": { "state": "FAILED", "error": { "message": "[TABLE_OR_VIEW_NOT_FOUND] no t" } } })
                    .to_string(),
            )
        }))
        .await;
        let (mut out, _sink) = with_sink();
        let e = session(base)
            .execute("SELECT * FROM t", usize::MAX, &mut out)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("TABLE_OR_VIEW_NOT_FOUND"), "{e}");
        assert_eq!(shown.posts().len(), 1);
    }

    #[tokio::test]
    async fn a_signed_url_never_reaches_the_message() {
        let seen = Seen::default();
        let base = server(
            &seen,
            None,
            Some((manifest(1, 1, false), vec![])),
            Some("http://127.0.0.1:1"),
        )
        .await;
        let mut out = QueryOutcome::default();
        let e = session(base)
            .execute("SELECT * FROM t", 5000, &mut out)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("25 MiB"), "{e}");
        assert!(!e.contains("SECRET") && !e.contains("sig="), "{e}");
    }

    #[tokio::test]
    async fn a_short_answer_is_an_error() {
        let seen = Seen::default();
        let chunks = vec![r#"[["1","a"]]"#];
        let base = server(&seen, None, Some((manifest(3, 1, false), chunks)), None).await;
        let (mut out, _sink) = with_sink();
        let e = session(base)
            .execute("SELECT id, txt FROM t", usize::MAX, &mut out)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains('3') && e.contains("incompleto"), "{e}");
    }

    #[tokio::test]
    async fn an_export_the_server_cut_is_refused() {
        let seen = Seen::default();
        let chunks = vec![r#"[["1","a"]]"#];
        let base = server(&seen, None, Some((manifest(1, 1, true), chunks)), None).await;
        let (mut out, sink) = with_sink();
        let e = session(base)
            .execute("SELECT id, txt FROM t", usize::MAX, &mut out)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("100 GiB"), "{e}");
        assert!(sink.lock().unwrap().begun.is_empty());
    }
}
