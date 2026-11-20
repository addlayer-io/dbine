//! The explorer's pages (`list_objects_page`) against the Spanner
//! emulator: folders, searches, lookups and "Otros" with small pages, each
//! equal to what the app pages out of `list_objects` (`page_of`). Creates
//! and drops its own database. Skipped without the variable:
//!
//! ```sh
//! docker start dbine-test-spanner   # -p 25303:9020
//! DBINE_TEST_SPANNER_URL=http://localhost:25303 cargo test -p dbine-driver-spanner --test objects_page -- --ignored
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{kinds, ConnectionConfig, DbObject, ObjectQuery, QueryOutcome, Session};
use serde_json::json;

const DB: &str = "objpage";

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

/// Every page of `q` from the driver and out of the whole list: the same
/// objects in the same pages. Gives how many pages.
async fn same_pages(s: &mut Box<dyn Session>, mut q: ObjectQuery, whole: &[DbObject]) -> usize {
    let mut offset = q.clone();
    let mut pages = 0;
    loop {
        let got = s.list_objects_page(&q).await.unwrap();
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
        limit: 4,
        ..Default::default()
    };
    f(&mut q);
    q
}

#[tokio::test]
#[ignore]
async fn pages_match_the_whole_list() {
    let Ok(url) = std::env::var("DBINE_TEST_SPANNER_URL") else {
        return;
    };
    let http = reqwest::Client::new();
    let _ = http
        .post(format!("{url}/v1/projects/test/instances"))
        .json(&json!({ "instanceId": "i1", "instance": { "config": "projects/test/instanceConfigs/emulator-config", "displayName": "i1", "nodeCount": 1 } }))
        .send()
        .await;
    let db_url = format!("{url}/v1/projects/test/instances/i1/databases/{DB}");
    let _ = http.delete(&db_url).send().await;
    http.post(format!("{url}/v1/projects/test/instances/i1/databases"))
        .json(&json!({ "createStatement": format!("CREATE DATABASE `{DB}`") }))
        .send()
        .await
        .unwrap();
    let mut cfg = ConnectionConfig {
        driver: "spanner".into(),
        database: DB.into(),
        ..Default::default()
    };
    for (k, v) in [
        ("project_id", "test"),
        ("instance", "i1"),
        ("endpoint_url", url.as_str()),
    ] {
        cfg.options.insert(k.into(), v.into());
    }
    let mut s = dbine_driver_spanner::drivers()
        .pop()
        .unwrap()
        .connect(&cfg, None)
        .await
        .unwrap();

    // 25 tables in the default schema (mixed case), views, sequences and
    // a named schema.
    let mut ddl: Vec<String> = (0..25)
        .map(|i| {
            let name = if i % 2 == 0 {
                format!("T{i:02}")
            } else {
                format!("t{i:02}_orders")
            };
            format!("CREATE TABLE {name} (id INT64 NOT NULL) PRIMARY KEY (id)")
        })
        .collect();
    ddl.extend(
        [
            "CREATE VIEW V_Orders SQL SECURITY INVOKER AS SELECT T00.id AS id FROM T00",
            "CREATE VIEW a_view SQL SECURITY INVOKER AS SELECT T00.id AS id FROM T00",
            "CREATE SEQUENCE seq_orders OPTIONS (sequence_kind = 'bit_reversed_positive')",
            "CREATE SEQUENCE Seq_B OPTIONS (sequence_kind = 'bit_reversed_positive')",
            "CREATE SCHEMA ventas",
            "CREATE TABLE ventas.orders (id INT64 NOT NULL) PRIMARY KEY (id)",
            "CREATE TABLE ventas.Notas (id INT64 NOT NULL) PRIMARY KEY (id)",
            "CREATE VIEW ventas.v_notas SQL SECURITY INVOKER AS SELECT n.id AS id FROM ventas.Notas AS n",
        ]
        .map(String::from),
    );
    let mut out = QueryOutcome::default();
    s.execute(&ddl.join(";\n"), 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{:?}", out.error);

    let whole = s.list_objects().await.unwrap();
    assert_eq!(whole.len(), 32, "{:?}", names(&whole));
    let cases = [
        with(|q| {
            q.no_schema = true;
            q.kind = Some(kinds::TABLE.into());
        }),
        with(|q| {
            q.no_schema = true;
            q.kind = Some(kinds::VIEW.into());
        }),
        with(|q| q.schema = Some("ventas".into())),
        with(|q| {
            q.schema = Some("ventas".into());
            q.kind = Some(kinds::TABLE.into());
        }),
        with(|q| q.schema = Some("VENTAS".into())),
        with(|q| q.pattern = Some("ORDERS".into())),
        with(|q| q.name = Some("T01_ORDERS".into())),
        with(|q| q.name = Some("orders".into())),
        with(|q| q.kind = Some(kinds::SEQUENCE.into())),
        with(|q| {
            q.not_kinds = vec![
                kinds::TABLE.into(),
                kinds::VIEW.into(),
                kinds::SEQUENCE.into(),
            ];
        }),
        with(|q| q.not_kinds = vec![kinds::TABLE.into()]),
        with(|q| q.pattern = Some("o%".into())),
        with(|q| q.schema = Some("INFORMATION_SCHEMA".into())),
        with(|q| q.limit = 5),
        ObjectQuery::default(),
    ];
    let mut pages = 0;
    let mut longest = 0;
    for q in cases {
        let n = same_pages(&mut s, q, &whole).await;
        pages += n;
        longest = longest.max(n);
    }
    assert!(longest >= 7, "the tables folder spans pages: {longest}");
    assert!(pages > 25, "{pages}");

    drop(s);
    http.delete(&db_url).send().await.unwrap();
}
