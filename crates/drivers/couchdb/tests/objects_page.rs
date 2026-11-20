//! The explorer's pages (`list_objects_page`) against a real server: for
//! every filter (each kind, no schema, a schema, "Otros", searches,
//! lookups) each page equals what the app pages out of `list_objects`
//! (`objects::page_of`), and the views of 25 design documents come in pages
//! of 10. They live in a scratch database (`dbine_pages`), deleted at the
//! end even when a check fails. Skipped without the variable:
//!
//! ```sh
//! docker start dbine-test-couchdb
//! DBINE_TEST_COUCHDB_URL=http://admin:secret@localhost:25202 \
//!   cargo test -p dbine-driver-couchdb --test objects_page -- --ignored --nocapture
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{kinds, ConnectionConfig, DbObject, ObjectQuery, Session};
use serde_json::json;

const DB: &str = "dbine_pages";

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

/// `http://user:pass@host:port` as the base URL and the login.
fn parts(url: &str) -> (String, String, String, String, u16) {
    let rest = url.strip_prefix("http://").unwrap();
    let (auth, host) = rest.split_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (h, p) = host.trim_end_matches('/').split_once(':').unwrap();
    (
        format!("http://{h}:{p}/{DB}"),
        user.into(),
        pass.into(),
        h.into(),
        p.parse().unwrap(),
    )
}

/// The design documents, then every filter in pages of 7 and the views in
/// pages of 10.
async fn check(url: String) {
    let (base, user, pass, host, port) = parts(&url);
    let http = reqwest::Client::new();
    // 25 design documents in mixed case, with one or two views each; two
    // that differ only in case; one without views; names with a slash.
    let mut docs: Vec<serde_json::Value> = (0..25)
        .map(|i| {
            let id = if i % 2 == 0 {
                format!("_design/Pedidos_{i:02}")
            } else {
                format!("_design/pedidos_{i:02}")
            };
            let views = if i % 3 == 0 {
                json!({ "by_fecha": { "map": "function (d) { emit(d.f, 1); }" } })
            } else {
                json!({ "by_fecha": { "map": "function (d) { emit(d.f, 1); }" },
                        "Por_Cliente": { "map": "function (d) { emit(d.c, 1); }" } })
            };
            json!({ "_id": id, "views": views })
        })
        .collect();
    docs.extend([
        json!({ "_id": "_design/Dup", "views": { "a": { "map": "function (d) { emit(1, 1); }" } } }),
        json!({ "_id": "_design/dup", "views": { "a": { "map": "function (d) { emit(1, 1); }" } } }),
        json!({ "_id": "_design/no_views", "language": "javascript" }),
        json!({ "_id": "_design/a/b", "views": { "c": { "map": "function (d) { emit(1, 1); }" } } }),
        json!({ "_id": "plain", "f": 1 }),
    ]);
    let r = http
        .post(format!("{base}/_bulk_docs"))
        .basic_auth(&user, Some(&pass))
        .json(&json!({ "docs": docs }))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{:?}", r.text().await);

    let cfg = ConnectionConfig {
        driver: "couchdb".into(),
        host,
        port,
        username: Some(user),
        password: Some(pass),
        database: DB.into(),
        ..Default::default()
    };
    let d = dbine_driver_couchdb::drivers().pop().unwrap();
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let all = s.list_objects().await.expect("list_objects");
    let views = all.iter().filter(|o| o.kind == kinds::VIEW).count();
    assert_eq!(views, 9 + 16 * 2 + 3, "{all:?}");

    let folder = q(|q| q.kind = Some(kinds::VIEW.into()));
    assert_eq!(
        same_pages(&mut s, &all, folder, 10).await,
        5,
        "44 views by 10"
    );
    let declared = vec![kinds::COLLECTION.to_string(), kinds::VIEW.to_string()];
    let mut queries = vec![
        ObjectQuery::default(),
        q(|q| q.not_kinds = declared.clone()),
        q(|q| q.not_kinds = vec![kinds::COLLECTION.into()]),
        q(|q| q.no_schema = true),
        q(|q| q.schema = Some(DB.into())),
        q(|q| {
            q.no_schema = true;
            q.schema = Some(DB.into());
            q.kind = Some(kinds::VIEW.into());
        }),
        q(|q| q.kind = Some(kinds::TABLE.into())),
        q(|q| {
            q.kind = Some(kinds::VIEW.into());
            q.pattern = Some("CLIENTE".into());
        }),
        q(|q| {
            q.kind = Some(kinds::VIEW.into());
            q.name = Some("pedidos_01/por_cliente".into());
        }),
    ];
    for k in &declared {
        queries.push(q(|q| q.kind = Some(k.clone())));
    }
    for p in [
        "pedidos",
        "PEDIDOS_1",
        "/",
        "_all",
        "s_0",
        "a/b",
        "é",
        "",
        " ",
        "zzz",
    ] {
        queries.push(q(|q| q.pattern = Some(p.into())));
    }
    for n in [
        "dup/a",
        "DUP/A",
        "a/b/c",
        "A/B/C",
        "_ALL_DOCS",
        "pedidos_00/BY_FECHA",
        "pedidos_00",
        "no_views",
        "nope/x",
        "",
    ] {
        queries.push(q(|q| q.name = Some(n.into())));
    }
    let mut pages = 0;
    for q in &queries {
        pages += same_pages(&mut s, &all, q.clone(), 7).await;
    }
    eprintln!(
        "couchdb: {} objects, {} queries, {pages} pages of 7: same as page_of",
        all.len(),
        queries.len()
    );
}

#[tokio::test]
#[ignore]
async fn pages_are_the_whole_lists_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_COUCHDB_URL") else {
        return;
    };
    let (base, user, pass, ..) = parts(&url);
    let http = reqwest::Client::new();
    let _ = http
        .delete(&base)
        .basic_auth(&user, Some(&pass))
        .send()
        .await;
    let r = http
        .put(&base)
        .basic_auth(&user, Some(&pass))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{:?}", r.text().await);
    let outcome = tokio::spawn(check(url.clone())).await;
    let r = http
        .delete(&base)
        .basic_auth(&user, Some(&pass))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{:?}", r.text().await);
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}
