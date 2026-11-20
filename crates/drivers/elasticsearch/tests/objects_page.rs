//! The explorer's pages (`list_objects_page`) against real servers: for
//! every filter (each kind, no schema, a schema, "Otros", searches,
//! lookups) each page equals what the app pages out of `list_objects`
//! (`objects::page_of`), with and without the system indices, and the 28
//! indices come in pages of 10. The test's indices, aliases and data
//! stream (`dbine-pg*`, `.dbine-pg-dot`) are deleted at the end even when
//! a check fails. Each test is skipped without its variable:
//!
//! ```sh
//! docker start dbine-test-elasticsearch   # then the others, one at a time
//! DBINE_TEST_ELASTICSEARCH_URL=http://localhost:25520 \
//! DBINE_TEST_OPENSEARCH_URL=http://localhost:25521 \
//! DBINE_TEST_OPENDISTRO_URL=http://localhost:25524 \
//!   cargo test -p dbine-driver-elasticsearch --test objects_page -- --ignored --nocapture
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{kinds, ConnectionConfig, DbObject, Error, ObjectQuery, Session};
use serde_json::{json, Value};

/// 25 indices, one with a `+`, one with an `é`, a closed one, a hidden
/// one and one whose name starts with a dot.
fn indices() -> Vec<String> {
    let mut v: Vec<String> = (0..25).map(|i| format!("dbine-pg-{i:02}")).collect();
    v.extend(
        [
            "dbine-pg+x",
            "dbine-pg-é",
            "dbine-pg-closed",
            "dbine-pg-hidden",
            ".dbine-pg-dot",
        ]
        .map(String::from),
    );
    v
}

/// Aliases keep their case: two differ only in it.
const ALIASES: &[(&str, &str)] = &[
    ("Dbine_Pg_Alias_A", "dbine-pg-00"),
    ("dbine_pg_alias_b", "dbine-pg-01"),
    ("DbinePgDup", "dbine-pg-02"),
    ("dbinepgdup", "dbine-pg-03"),
    ("dbine-pg-alias-c", "dbine-pg-03"),
];

async fn send(http: &reqwest::Client, method: &str, url: String, body: Option<Value>) -> Value {
    let m = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
    let mut rq = http.request(m, &url);
    if let Some(b) = body {
        rq = rq.json(&b);
    }
    let r = rq.send().await.unwrap();
    let ok = r.status().is_success();
    let v: Value = r.json().await.unwrap_or(Value::Null);
    assert!(ok, "{method} {url}: {v}");
    v
}

/// A search or a lookup that can find indices: `Unsupported`.
fn index_search(q: &ObjectQuery) -> bool {
    let indices = q.kind.as_deref().is_none_or(|k| k == kinds::INDEX)
        && !q.not_kinds.iter().any(|k| k == kinds::INDEX);
    indices
        && (q.no_schema || q.schema.is_none())
        && (q.pattern.as_deref().is_some_and(|p| !p.is_empty()) || q.name.is_some())
}

/// Pages `q` both ways, `size` at a time: the same objects on each page,
/// and the same pages. Gives how many there were (0: `Unsupported`).
async fn same_pages(
    s: &mut Box<dyn Session>,
    all: &[DbObject],
    mut q: ObjectQuery,
    size: u32,
) -> usize {
    q.limit = size;
    let (mut got_q, mut want_q) = (q.clone(), q.clone());
    let mut pages = 0;
    loop {
        let want = page_of(all, &want_q);
        let got = match s.list_objects_page(&got_q).await {
            Err(Error::Unsupported(_)) if index_search(&q) && pages == 0 => return 0,
            r => r.unwrap_or_else(|e| panic!("{got_q:?}: {e}")),
        };
        assert!(!index_search(&q), "{q:?} answered");
        assert_eq!(got.objects, want.objects, "page {pages} of {q:?}");
        assert_eq!(
            got.next.is_some(),
            want.next.is_some(),
            "page {pages} of {q:?}"
        );
        pages += 1;
        match (got.next, want.next) {
            (Some(g), Some(w)) => {
                got_q.after = Some(g);
                want_q.after = Some(w);
            }
            _ => return pages,
        }
    }
}

fn q(f: impl FnOnce(&mut ObjectQuery)) -> ObjectQuery {
    let mut q = ObjectQuery::default();
    f(&mut q);
    q
}

fn queries(declared: &[String]) -> Vec<ObjectQuery> {
    let mut v = vec![
        ObjectQuery::default(),
        q(|q| q.not_kinds = declared.to_vec()),
        q(|q| q.not_kinds = vec![kinds::INDEX.into()]),
        q(|q| q.no_schema = true),
        q(|q| q.schema = Some("default".into())),
        q(|q| {
            q.no_schema = true;
            q.schema = Some("default".into());
            q.kind = Some("alias".into());
        }),
        q(|q| q.kind = Some(kinds::TABLE.into())),
        q(|q| {
            q.kind = Some(kinds::INDEX.into());
            q.pattern = Some("PG-1".into());
        }),
        q(|q| {
            q.kind = Some("alias".into());
            q.name = Some("DBINEPGDUP".into());
        }),
    ];
    for k in declared {
        v.push(q(|q| q.kind = Some(k.clone())));
    }
    for p in [
        "pg", "PG", "-", "+", "é", "É", "%", "*", " ", ",", "alias", "ALIAS_", "dup", ".", "ds", "",
    ] {
        // Everything (the app's), aliases, aliases and data streams.
        v.push(q(|q| q.pattern = Some(p.into())));
        v.push(q(|q| {
            q.kind = Some("alias".into());
            q.pattern = Some(p.into());
        }));
        v.push(q(|q| {
            q.not_kinds = vec![kinds::INDEX.into()];
            q.pattern = Some(p.into());
        }));
    }
    for n in [
        "dbine-pg-00",
        "DBINE-PG-00",
        "dbinepgdup",
        "DbinePgDup",
        "dbine-pg-ds",
        "dbine-pg-é",
        "DBINE-PG-É",
        "dbine-pg+x",
        "dbine-pg-hidden",
        ".dbine-pg-dot",
        "_all",
        "-x",
        "dbine-pg-alias-c",
        "nope",
        "",
    ] {
        v.push(q(|q| q.name = Some(n.into())));
        v.push(q(|q| {
            q.not_kinds = vec![kinds::INDEX.into()];
            q.name = Some(n.into());
        }));
    }
    v
}

/// The test's objects, then every filter in pages of 7 and the indices in
/// pages of 10, without and with the system indices.
async fn check(id: String, url: String) {
    let http = reqwest::Client::new();
    for i in indices() {
        let body =
            (i == "dbine-pg-hidden").then(|| json!({ "settings": { "index.hidden": true } }));
        send(&http, "PUT", format!("{url}/{}", urlencode(&i)), body).await;
    }
    send(&http, "POST", format!("{url}/dbine-pg-closed/_close"), None).await;
    let actions: Vec<Value> = ALIASES
        .iter()
        .map(|(a, i)| json!({ "add": { "index": i, "alias": a } }))
        .collect();
    send(
        &http,
        "POST",
        format!("{url}/_aliases"),
        Some(json!({ "actions": actions })),
    )
    .await;
    let streams = id != "opendistro";
    if streams {
        send(
            &http,
            "PUT",
            format!("{url}/_index_template/dbine-pg-tpl"),
            Some(json!({ "index_patterns": ["dbine-pg-ds*"], "data_stream": {}, "priority": 500 })),
        )
        .await;
        send(
            &http,
            "PUT",
            format!("{url}/_data_stream/dbine-pg-ds"),
            None,
        )
        .await;
    }

    let d = dbine_driver_elasticsearch::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap();
    let declared: Vec<String> = d
        .info()
        .object_kinds
        .iter()
        .map(|k| k.id.to_string())
        .collect();
    let queries = queries(&declared);
    for show_system in [false, true] {
        let mut cfg = ConnectionConfig {
            driver: id.clone(),
            host: url.clone(),
            ..Default::default()
        };
        if show_system {
            cfg.options.insert("show_system".into(), "true".into());
        }
        let mut s = d.connect(&cfg, None).await.expect("connect");
        let all = s.list_objects().await.expect("list_objects");
        let mine = |k: &str| {
            all.iter()
                .filter(|o| o.kind == k && o.name.to_lowercase().contains("dbine"))
                .count()
        };
        // The hidden index and a data stream's backing ones (with a dot)
        // as the server lists them.
        assert!(mine(kinds::INDEX) >= 28, "{all:?}");
        assert_eq!(
            all.iter().any(|o| o.name == ".dbine-pg-dot"),
            show_system,
            "{all:?}"
        );
        assert_eq!(mine("alias"), ALIASES.len(), "{all:?}");
        assert_eq!(mine(kinds::STREAM), usize::from(streams), "{all:?}");

        let folder = q(|q| q.kind = Some(kinds::INDEX.into()));
        assert!(same_pages(&mut s, &all, folder, 10).await >= 3);
        let (mut pages, mut unsupported) = (0, 0);
        for q in &queries {
            match same_pages(&mut s, &all, q.clone(), 7).await {
                0 => unsupported += 1,
                n => pages += n,
            }
        }
        eprintln!(
            "{id} (system indices {show_system}): {} objects, {} queries, {pages} pages of 7 same as page_of, {unsupported} searches for indices left to the app",
            all.len(),
            queries.len()
        );
    }
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Deletes what the test made, whatever is there of it.
async fn clean(url: &str) {
    let http = reqwest::Client::new();
    let _ = http
        .delete(format!("{url}/_data_stream/dbine-pg-ds"))
        .send()
        .await;
    let _ = http
        .delete(format!("{url}/_index_template/dbine-pg-tpl"))
        .send()
        .await;
    for i in indices() {
        let _ = http.delete(format!("{url}/{}", urlencode(&i))).send().await;
    }
}

async fn on_scratch_indices(id: &str, url: &str) {
    clean(url).await;
    let outcome = tokio::spawn(check(id.into(), url.into())).await;
    clean(url).await;
    let left = reqwest::get(format!(
        "{url}/_cat/indices/*dbine-pg*?format=json&h=index&expand_wildcards=all"
    ))
    .await
    .unwrap()
    .text()
    .await
    .unwrap();
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
    assert_eq!(left, "[]", "left behind");
}

#[tokio::test]
#[ignore]
async fn elasticsearch_pages_are_the_whole_lists_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_ELASTICSEARCH_URL") else {
        return;
    };
    on_scratch_indices("elasticsearch", &url).await;
}

#[tokio::test]
#[ignore]
async fn opensearch_pages_are_the_whole_lists_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_OPENSEARCH_URL") else {
        return;
    };
    on_scratch_indices("opensearch", &url).await;
}

#[tokio::test]
#[ignore]
async fn opendistro_pages_are_the_whole_lists_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_OPENDISTRO_URL") else {
        return;
    };
    on_scratch_indices("opendistro", &url).await;
}
