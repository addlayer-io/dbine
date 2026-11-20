//! The explorer's pages (`list_objects_page`) against the emulator: for
//! every filter (the containers, no schema, a schema, "Otros", searches,
//! lookups) each page equals what the app pages out of `list_objects`
//! (`objects::page_of`), and the 28 containers come in pages of 10. They
//! live in a scratch database (`dbine_pages`), deleted at the end even
//! when a check fails. Skipped without the variable:
//!
//! ```sh
//! docker start dbine-test-cosmosdb
//! DBINE_TEST_COSMOSDB_URL=https://localhost:25213 \
//!   cargo test -p dbine-driver-cosmosdb --test objects_page -- --ignored --nocapture
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{kinds, ConnectionConfig, DbObject, ObjectQuery, Session};
use dbine_driver_cosmosdb::auth_header;
use serde_json::{json, Value};

const EMULATOR_KEY: &str =
    "C2y6yDjf5/R+ob0N8A7Cgv30VRDJIWEHLM+4QDU5DE2nQ9nDuVTqobD4b8mGGyPMbIZnqyMsEcaGQy67XIw/Jw==";
const DB: &str = "dbine_pages";

/// A signed REST call: its status and body.
// Test helper: each argument is one part of the REST call.
#[allow(clippy::too_many_arguments)]
async fn rest(
    url: &str,
    key: &str,
    method: &str,
    rtype: &str,
    link: &str,
    path: &str,
    body: Option<Value>,
    query: bool,
) -> (u16, Value) {
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let date = chrono::Utc::now()
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    let mut rq = http
        .request(method.parse().unwrap(), format!("{url}{path}"))
        .header(
            "Authorization",
            auth_header(key, method, rtype, link, &date).unwrap(),
        )
        .header("x-ms-date", date)
        .header("x-ms-version", "2018-12-31");
    if let Some(b) = body {
        let ct = if query {
            "application/query+json"
        } else {
            "application/json"
        };
        rq = rq.header("Content-Type", ct).body(b.to_string());
    }
    if query {
        rq = rq.header("x-ms-documentdb-isquery", "True");
    }
    let r = rq.send().await.unwrap();
    let status = r.status().as_u16();
    (status, r.json().await.unwrap_or(Value::Null))
}

/// Pages `q` both ways, `size` at a time: the same objects on each page,
/// and the same pages. Gives how many there were.
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
        let got = s
            .list_objects_page(&got_q)
            .await
            .unwrap_or_else(|e| panic!("{got_q:?}: {e}"));
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

/// The containers, then every filter in pages of 7 and the folder in
/// pages of 10.
async fn check(url: String, key: String) {
    let mut names: Vec<String> = (0..25)
        .map(|i| {
            if i % 2 == 0 {
                format!("Pedidos_{i:02}")
            } else {
                format!("pedidos_{i:02}_x")
            }
        })
        .collect();
    names.extend(["Dup", "dup", "a'b"].map(String::from));
    for n in &names {
        let (status, body) = rest(
            &url,
            &key,
            "POST",
            "colls",
            &format!("dbs/{DB}"),
            &format!("/dbs/{DB}/colls"),
            Some(json!({ "id": n, "partitionKey": { "paths": ["/pk"], "kind": "Hash" } })),
            false,
        )
        .await;
        assert_eq!(status, 201, "{n}: {body}");
    }
    // Whether this gateway answers a query on its containers (else the
    // driver reads the feed).
    let (status, body) = rest(
        &url,
        &key,
        "POST",
        "colls",
        &format!("dbs/{DB}"),
        &format!("/dbs/{DB}/colls"),
        Some(json!({
            "query": "SELECT * FROM root r WHERE CONTAINS(LOWER(r.id), @v)",
            "parameters": [{ "name": "@v", "value": "dup" }],
        })),
        true,
    )
    .await;
    let found = body
        .get("DocumentCollections")
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(|c| c["id"].as_str())
                .collect::<Vec<_>>()
        });
    eprintln!("containers query: {status} {found:?}");

    let mut cfg = ConnectionConfig {
        driver: "cosmosdb".into(),
        host: url.clone(),
        database: DB.into(),
        trust_server_certificate: true,
        ..Default::default()
    };
    cfg.options.insert("account_key".into(), key.clone());
    let d = dbine_driver_cosmosdb::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let all = s.list_objects().await.expect("list_objects");
    assert_eq!(all.len(), 28, "{all:?}");

    let folder = q(|q| q.kind = Some(kinds::COLLECTION.into()));
    assert_eq!(same_pages(&mut s, &all, folder, 10).await, 3, "28 by 10");
    let mut queries = vec![
        ObjectQuery::default(),
        q(|q| q.not_kinds = vec![kinds::COLLECTION.into()]),
        q(|q| q.no_schema = true),
        q(|q| q.schema = Some(DB.into())),
        q(|q| {
            q.no_schema = true;
            q.schema = Some(DB.into());
        }),
        q(|q| q.kind = Some(kinds::TABLE.into())),
        q(|q| {
            q.kind = Some(kinds::COLLECTION.into());
            q.pattern = Some("PEDIDOS_1".into());
        }),
    ];
    for p in ["pedidos", "_X", "dup", "'", "%", "é", "", " ", "zzz"] {
        queries.push(q(|q| q.pattern = Some(p.into())));
    }
    for n in ["dup", "DUP", "a'b", "A'B", "pedidos_01_x", "nope", ""] {
        queries.push(q(|q| q.name = Some(n.into())));
    }
    let mut pages = 0;
    for q in &queries {
        pages += same_pages(&mut s, &all, q.clone(), 7).await;
    }
    eprintln!(
        "cosmosdb: {} containers, {} queries, {pages} pages of 7: same as page_of",
        all.len(),
        queries.len()
    );
}

#[tokio::test]
#[ignore]
async fn pages_are_the_whole_lists_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_COSMOSDB_URL") else {
        return;
    };
    let url = url.trim_end_matches('/').to_string();
    let key = std::env::var("DBINE_TEST_COSMOSDB_KEY").unwrap_or_else(|_| EMULATOR_KEY.into());
    let (link, path) = (format!("dbs/{DB}"), format!("/dbs/{DB}"));
    let drop = || rest(&url, &key, "DELETE", "dbs", &link, &path, None, false);
    drop().await;
    let (status, body) = rest(
        &url,
        &key,
        "POST",
        "dbs",
        "",
        "/dbs",
        Some(json!({ "id": DB })),
        false,
    )
    .await;
    assert_eq!(status, 201, "{body}");
    let outcome = tokio::spawn(check(url.clone(), key.clone())).await;
    let (status, _) = drop().await;
    assert!(status < 300, "delete: {status}");
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}
