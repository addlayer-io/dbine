//! The explorer's pages (`list_objects_page`) against DynamoDB Local: for
//! every filter (each kind, one table's indexes, no schema, "Otros",
//! searches, lookups) each page equals what the app pages out of
//! `list_objects` (`objects::page_of`); the 25 tables and their 27 indexes
//! come in pages of 10. A search or lookup among every table's indexes
//! answers `Unsupported` (the app pages its whole list then). The tables
//! (`DbinePg_*`, `dbinepg_*`) are dropped at the end even when a check
//! fails. Skipped without the variable:
//!
//! ```sh
//! docker start dbine-test-dynamodb
//! DBINE_TEST_DYNAMODB_URL=http://localhost:25300 \
//!   cargo test -p dbine-driver-dynamodb --test objects_page -- --ignored --nocapture
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{kinds, ConnectionConfig, DbObject, Error, ObjectQuery, QueryOutcome, Session};

fn tables() -> Vec<String> {
    (0..25)
        .map(|i| {
            if i % 2 == 0 {
                format!("DbinePg_{i:02}")
            } else {
                format!("dbinepg_{i:02}_x")
            }
        })
        .collect()
}

async fn open(url: &str) -> Box<dyn Session> {
    let mut c = ConnectionConfig {
        driver: "dynamodb".into(),
        ..Default::default()
    };
    for (k, v) in [
        ("region", "us-east-1"),
        ("auth_mode", "keys"),
        ("access_key_id", "dummy"),
        ("secret_access_key", "dummy"),
        ("endpoint_url", url),
    ] {
        c.options.insert(k.into(), v.into());
    }
    dbine_driver_dynamodb::drivers()
        .pop()
        .unwrap()
        .connect(&c, None)
        .await
        .expect("connect")
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> dbine_driver::Result<()> {
    let mut out = QueryOutcome::default();
    s.execute(text, 10, &mut out).await
}

/// A search or a lookup among every table's indexes: `Unsupported`.
fn whole_list_search(q: &ObjectQuery) -> bool {
    let indexes = q.kind.as_deref().is_none_or(|k| k == kinds::INDEX)
        && !q.not_kinds.iter().any(|k| k == kinds::INDEX);
    indexes
        && !q.no_schema
        && q.schema.is_none()
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
            Err(Error::Unsupported(_)) if whole_list_search(&q) && pages == 0 => return 0,
            r => r.unwrap_or_else(|e| panic!("{got_q:?}: {e}")),
        };
        assert!(!whole_list_search(&q), "{q:?} answered");
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

/// The tables (every third with two global and one local index), then
/// every filter in pages of 7 and the folders in pages of 10.
async fn check(url: String) {
    let mut s = open(&url).await;
    for (i, t) in tables().iter().enumerate() {
        // The key's attributes, and those of the indexes.
        let (attrs, indexes) = if i % 3 == 0 {
            (
                r#", {"AttributeName": "f", "AttributeType": "S"}, {"AttributeName": "c", "AttributeType": "S"}"#,
                r#", "GlobalSecondaryIndexes": [
                 {"IndexName": "ByFecha", "KeySchema": [{"AttributeName": "f", "KeyType": "HASH"}], "Projection": {"ProjectionType": "ALL"}},
                 {"IndexName": "porCliente", "KeySchema": [{"AttributeName": "c", "KeyType": "HASH"}], "Projection": {"ProjectionType": "KEYS_ONLY"}}],
               "LocalSecondaryIndexes": [
                 {"IndexName": "lsi_A", "KeySchema": [{"AttributeName": "pk", "KeyType": "HASH"}, {"AttributeName": "f", "KeyType": "RANGE"}], "Projection": {"ProjectionType": "ALL"}}]"#,
            )
        } else {
            ("", "")
        };
        let ddl = format!(
            r#"CREATE TABLE "{t}" {{"AttributeDefinitions": [
                 {{"AttributeName": "pk", "AttributeType": "S"}}, {{"AttributeName": "sk", "AttributeType": "S"}}{attrs}],
               "KeySchema": [{{"AttributeName": "pk", "KeyType": "HASH"}}, {{"AttributeName": "sk", "KeyType": "RANGE"}}],
               "BillingMode": "PAY_PER_REQUEST"{indexes}}}"#
        );
        run(&mut s, &ddl).await.expect(t);
    }
    let all = s.list_objects().await.expect("list_objects");
    let mine = |o: &&DbObject| {
        let t = o.schema.as_deref().unwrap_or(&o.name);
        t.to_lowercase().starts_with("dbinepg_")
    };
    let count = |k: &str| all.iter().filter(mine).filter(|o| o.kind == k).count();
    assert_eq!(count(kinds::TABLE), 25);
    assert_eq!(count(kinds::INDEX), 27);
    eprintln!("{} objects in the region", all.len());

    let folder = |k: &str| q(|q| q.kind = Some(k.into()));
    assert!(same_pages(&mut s, &all, folder(kinds::TABLE), 10).await >= 3);
    assert!(same_pages(&mut s, &all, folder(kinds::INDEX), 10).await >= 3);
    let declared = vec![kinds::TABLE.to_string(), kinds::INDEX.to_string()];
    let mut queries = vec![
        ObjectQuery::default(),
        q(|q| q.not_kinds = declared.clone()),
        q(|q| q.not_kinds = vec![kinds::TABLE.into()]),
        q(|q| q.not_kinds = vec![kinds::INDEX.into()]),
        q(|q| q.no_schema = true),
        q(|q| q.schema = Some("DbinePg_00".into())),
        q(|q| q.schema = Some("dbinepg_00".into())),
        q(|q| q.schema = Some("dbinepg_03_x".into())),
        q(|q| q.schema = Some("DbinePg_02".into())),
        q(|q| q.schema = Some("no such table".into())),
        q(|q| {
            q.schema = Some("DbinePg_06".into());
            q.pattern = Some("BY".into());
        }),
        q(|q| {
            q.schema = Some("DbinePg_06".into());
            q.name = Some("bycliente".into());
        }),
        q(|q| {
            q.schema = Some("DbinePg_06".into());
            q.name = Some("PORCLIENTE".into());
        }),
        q(|q| {
            q.no_schema = true;
            q.schema = Some("DbinePg_06".into());
        }),
        q(|q| q.kind = Some(kinds::VIEW.into())),
        q(|q| {
            q.kind = Some(kinds::TABLE.into());
            q.pattern = Some("PG_1".into());
        }),
        q(|q| {
            q.kind = Some(kinds::TABLE.into());
            q.name = Some("dbinepg_02".into());
        }),
        q(|q| {
            q.kind = Some(kinds::INDEX.into());
            q.pattern = Some("lsi".into());
        }),
    ];
    for k in &declared {
        queries.push(q(|q| q.kind = Some(k.clone())));
    }
    for p in ["dbinepg", "_X", "fecha", "é", "", " ", "zzz"] {
        queries.push(q(|q| q.pattern = Some(p.into())));
    }
    for n in ["DBINEPG_01_X", "dbinepg_00", "lsi_a", "nope", ""] {
        queries.push(q(|q| q.name = Some(n.into())));
    }
    let (mut pages, mut unsupported) = (0, 0);
    for q in &queries {
        match same_pages(&mut s, &all, q.clone(), 7).await {
            0 => unsupported += 1,
            n => pages += n,
        }
    }
    eprintln!(
        "dynamodb: {} queries, {pages} pages of 7 same as page_of, {unsupported} searches among every table's indexes left to the app",
        queries.len()
    );
}

#[tokio::test]
#[ignore]
async fn pages_are_the_whole_lists_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_DYNAMODB_URL") else {
        return;
    };
    let mut admin = open(&url).await;
    let drop_all = tables()
        .iter()
        .map(|t| format!("DROP TABLE IF EXISTS \"{t}\""))
        .collect::<Vec<_>>()
        .join(";\n");
    run(&mut admin, &drop_all).await.unwrap();
    let outcome = tokio::spawn(check(url.clone())).await;
    run(&mut admin, &drop_all).await.unwrap();
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}
