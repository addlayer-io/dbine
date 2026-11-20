//! The explorer's pages (`list_objects_page`) against real servers: for
//! every filter (each kind, no schema, a schema, "Otros", searches with
//! regular-expression characters, lookups) each page equals what the app
//! pages out of `list_objects` (`objects::page_of`), and the 27
//! collections come in pages of 10. The collections live in a scratch
//! database (`dbine_pages`), dropped at the end even when a check fails.
//! Each test is skipped without its variable:
//!
//! ```sh
//! docker start dbine-test-mongodb dbine-test-atlas dbine-test-ferretdb
//! DBINE_TEST_MONGODB_URL='mongodb://root:secret@localhost:25201/?authSource=admin' \
//! DBINE_TEST_ATLAS_URL='mongodb://localhost:27205/?directConnection=true' \
//! DBINE_TEST_FERRETDB_URL='mongodb://root:secret@localhost:25203/' \
//!   cargo test -p dbine-driver-mongodb --test objects_page -- --ignored --nocapture
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{kinds, ConnectionConfig, DbObject, ObjectQuery, Session};
use mongodb::bson::doc;

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

/// The scratch database's collections, then every filter in pages of 7
/// and the collections in pages of 10.
async fn check(driver: String, url: String) {
    let db = mongodb::Client::with_uri_str(&url)
        .await
        .unwrap()
        .database(DB);
    // 25 collections in mixed case, two that differ only in case, and
    // names with regular-expression characters.
    let mut names: Vec<String> = (0..25)
        .map(|i| {
            if i % 2 == 0 {
                format!("Pedidos_{i:02}")
            } else {
                format!("pedidos_{i:02}_x")
            }
        })
        .collect();
    names.extend(["Dup", "dup", "a.b(1)", "x+y[2]"].map(String::from));
    for n in &names {
        db.create_collection(n).await.expect(n);
    }
    // Some servers have no views or time series: checked where they do.
    let view = db
        .run_command(doc! { "create": "V_pedidos", "viewOn": "Pedidos_00", "pipeline": [] })
        .await;
    let series = db
        .run_command(doc! { "create": "medidas", "timeseries": { "timeField": "t" } })
        .await;
    eprintln!(
        "{driver}: view {}, time series {}",
        view.as_ref()
            .map_or_else(|e| e.to_string(), |_| "made".into()),
        series
            .as_ref()
            .map_or_else(|e| e.to_string(), |_| "made".into()),
    );

    let mut cfg = ConnectionConfig {
        driver: driver.clone(),
        database: DB.into(),
        ..Default::default()
    };
    cfg.options.insert("connection_string".into(), url.clone());
    let d = dbine_driver_mongodb::drivers()
        .into_iter()
        .find(|d| d.info().id == driver)
        .unwrap();
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let all = s.list_objects().await.expect("list_objects");
    let count = |k: &str| all.iter().filter(|o| o.kind == k).count();
    // FerretDB takes `viewOn` but makes a collection.
    assert_eq!(
        all.len(),
        29 + usize::from(series.is_ok()) + usize::from(view.is_ok()),
        "{all:?}"
    );
    eprintln!(
        "{driver}: {} collections, {} views",
        count(kinds::COLLECTION),
        count(kinds::VIEW)
    );
    assert!(!all.iter().any(|o| o.name.starts_with("system.")));

    let colls = q(|q| q.kind = Some(kinds::COLLECTION.into()));
    assert!(same_pages(&mut s, &all, colls, 10).await >= 3, "by 10");
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
            q.kind = Some(kinds::COLLECTION.into());
            q.pattern = Some("PEDIDOS_1".into());
        }),
        q(|q| {
            q.kind = Some(kinds::VIEW.into());
            q.name = Some("v_pedidos".into());
        }),
    ];
    for k in &declared {
        queries.push(q(|q| q.kind = Some(k.clone())));
    }
    for p in [
        "pedidos", "PEDIDOS", "_", ".", "(1)", "+y[", "$", "^", "a.b", "é", "", " ", "zzz", "\\",
    ] {
        queries.push(q(|q| q.pattern = Some(p.into())));
    }
    for n in [
        "dup",
        "DUP",
        "a.b(1)",
        "A.B(1)",
        "X+Y[2]",
        "pedidos_01_x",
        "medidas",
        "nope",
        "",
        "Dup ",
    ] {
        queries.push(q(|q| q.name = Some(n.into())));
    }
    let mut pages = 0;
    for q in &queries {
        pages += same_pages(&mut s, &all, q.clone(), 7).await;
    }
    eprintln!(
        "{driver}: {} objects, {} queries, {pages} pages of 7: same as page_of",
        all.len(),
        queries.len()
    );
}

/// Runs the check in a scratch database and drops it whatever happens.
async fn on_scratch_database(driver: &str, url: &str) {
    let db = mongodb::Client::with_uri_str(url)
        .await
        .unwrap()
        .database(DB);
    db.drop().await.unwrap();
    let outcome = tokio::spawn(check(driver.into(), url.into())).await;
    db.drop().await.unwrap();
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

#[tokio::test]
#[ignore]
async fn mongodb_pages_are_the_whole_lists_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_MONGODB_URL") else {
        return;
    };
    on_scratch_database("mongodb", &url).await;
}

#[tokio::test]
#[ignore]
async fn atlas_pages_are_the_whole_lists_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_ATLAS_URL") else {
        return;
    };
    on_scratch_database("mongodb", &url).await;
}

#[tokio::test]
#[ignore]
async fn ferretdb_pages_are_the_whole_lists_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_FERRETDB_URL") else {
        return;
    };
    on_scratch_database("ferretdb", &url).await;
}
