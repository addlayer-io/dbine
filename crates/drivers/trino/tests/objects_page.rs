//! The explorer's pages (`list_objects_page`) against real servers: for
//! every filter (each kind, a schema, no schema, "Otros", searches, lookups)
//! and page size, each page equals the app's own paging of `list_objects`
//! (`objects::page_of`), cursor included. The objects live in two scratch
//! schemas of the `memory` catalog (`dbine_pages`, `dbine_pages_b`),
//! dropped at the end even when a check fails. Each test reads its variable
//! and is skipped without it:
//!
//! ```sh
//! DBINE_TEST_TRINO_URL=http://localhost:25180 \
//! DBINE_TEST_PRESTO_URL=http://localhost:25181 \
//!   cargo test -p dbine-driver-trino --test objects_page -- --ignored --nocapture --test-threads 1
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, Driver, ObjectQuery, QueryOutcome, Session};
use std::sync::Arc;

const A: &str = "dbine_pages";
const B: &str = "dbine_pages_b";

fn cfg(env: &str, id: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(env).ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: id.into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("dbine".into()),
        database: "memory".into(),
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_trino::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out)
        .await
        .map_err(|e| format!("{sql}: {e}"))?;
    match out.error {
        Some(e) => Err(format!("{sql}: {e}")),
        None => Ok(()),
    }
}

/// The tables of each schema: in `A` 25 numbered (a folder of three pages
/// of 10), LIKE's wildcards, a non-ASCII letter, and names `B` also has.
fn tables() -> Vec<(&'static str, String)> {
    let mut v: Vec<(&str, String)> = (0..25).map(|i| (A, format!("t_{i:02}"))).collect();
    for n in ["a%b", "axb", "dup", "ünïcode", "orders_old"] {
        v.push((A, n.into()));
    }
    for n in ["dup", "orders"] {
        v.push((B, n.into()));
    }
    v
}

fn views() -> Vec<(&'static str, String)> {
    let mut v: Vec<(&str, String)> = (0..9).map(|i| (A, format!("v_{i}"))).collect();
    v.push((B, "v_orders".into()));
    v
}

fn setup() -> Vec<String> {
    let mut v = vec![
        format!("CREATE SCHEMA memory.{A}"),
        format!("CREATE SCHEMA memory.{B}"),
    ];
    v.extend(
        tables()
            .iter()
            .map(|(s, t)| format!("CREATE TABLE memory.{s}.\"{t}\" (id integer)")),
    );
    v.extend(
        views()
            .iter()
            .map(|(s, t)| format!("CREATE VIEW memory.{s}.\"{t}\" AS SELECT 1 AS x")),
    );
    v
}

fn cleanup() -> Vec<String> {
    let mut v: Vec<String> = views()
        .iter()
        .map(|(s, t)| format!("DROP VIEW IF EXISTS memory.{s}.\"{t}\""))
        .collect();
    v.extend(
        tables()
            .iter()
            .map(|(s, t)| format!("DROP TABLE IF EXISTS memory.{s}.\"{t}\"")),
    );
    v.push(format!("DROP SCHEMA IF EXISTS memory.{A}"));
    v.push(format!("DROP SCHEMA IF EXISTS memory.{B}"));
    v
}

/// The filters the explorer sends: folders, "Otros", searches, lookups.
fn queries(kinds: &[String], declared: &[String]) -> Vec<ObjectQuery> {
    let mut v = vec![
        ObjectQuery::default(),
        ObjectQuery {
            no_schema: true,
            ..Default::default()
        },
        ObjectQuery {
            not_kinds: declared.to_vec(),
            ..Default::default()
        },
        ObjectQuery {
            not_kinds: vec!["table".into()],
            ..Default::default()
        },
        ObjectQuery {
            kind: Some("nope".into()),
            ..Default::default()
        },
    ];
    for schema in [A, B, "DBINE_PAGES", "information_schema", "nope"] {
        v.push(ObjectQuery {
            schema: Some(schema.into()),
            ..Default::default()
        });
        for k in kinds {
            v.push(ObjectQuery {
                schema: Some(schema.into()),
                kind: Some(k.clone()),
                ..Default::default()
            });
        }
    }
    for k in kinds {
        v.push(ObjectQuery {
            kind: Some(k.clone()),
            ..Default::default()
        });
    }
    for p in ["T_1", "%", "_", "a%b", "ORDERS", "dup", "ÜNÏ", "zzz", ""] {
        v.push(ObjectQuery {
            pattern: Some(p.into()),
            ..Default::default()
        });
        v.push(ObjectQuery {
            schema: Some(A.into()),
            kind: Some("table".into()),
            pattern: Some(p.into()),
            ..Default::default()
        });
    }
    for n in ["T_05", "orders", "DUP", "a%b", "nope"] {
        v.push(ObjectQuery {
            name: Some(n.into()),
            ..Default::default()
        });
    }
    v
}

/// Pages `q` through the driver and through `page_of` over `all`: the same
/// objects each time, to the last page. Returns how many pages.
async fn same_pages(s: &mut Box<dyn Session>, all: &[DbObject], q: &ObjectQuery) -> usize {
    let mut q = q.clone();
    let mut offset = q.clone();
    let mut pages = 0;
    loop {
        let got = s
            .list_objects_page(&q)
            .await
            .unwrap_or_else(|e| panic!("{q:?}: {e}"));
        let want = page_of(all, &offset);
        assert_eq!(got.objects, want.objects, "{q:?}");
        assert_eq!(got.next.is_some(), want.next.is_some(), "{q:?}");
        pages += 1;
        assert!(pages < 100, "{q:?}: no last page");
        match (got.next, want.next) {
            (Some(a), Some(b)) => {
                q.after = Some(a);
                offset.after = Some(b);
            }
            _ => return pages,
        }
    }
}

async fn check(d: Arc<dyn Driver>, cfg: ConnectionConfig) {
    let mut s = d.connect(&cfg, None).await.expect("connect");
    for sql in setup() {
        if let Err(e) = run(&mut s, &sql).await {
            panic!("{e}");
        }
    }
    let all = s.list_objects().await.unwrap();
    let declared: Vec<String> = d
        .info()
        .object_kinds
        .iter()
        .map(|k| k.id.to_string())
        .collect();
    let mut kinds = declared.clone();
    for o in &all {
        if !kinds.contains(&o.kind) {
            kinds.push(o.kind.clone());
        }
    }
    eprintln!("{}: {} objects, kinds {kinds:?}", d.info().id, all.len());
    let mut checked = 0;
    for q in queries(&kinds, &declared) {
        for limit in [7, 10, 0] {
            same_pages(&mut s, &all, &ObjectQuery { limit, ..q.clone() }).await;
            checked += 1;
        }
    }
    let folder = ObjectQuery {
        schema: Some(A.into()),
        kind: Some("table".into()),
        limit: 10,
        ..Default::default()
    };
    let pages = same_pages(&mut s, &all, &folder).await;
    assert!(pages >= 3, "a folder of more than one page: {pages}");
    let p = s.list_objects_page(&folder).await.unwrap();
    assert_eq!(p.objects.len(), 10);
    // A cursor that isn't the driver's (the app's own offset) goes back to
    // the app.
    let foreign = ObjectQuery {
        after: Some("10".into()),
        ..folder
    };
    assert!(matches!(
        s.list_objects_page(&foreign).await,
        Err(dbine_driver::Error::Unsupported(_))
    ));
    eprintln!(
        "{}: {checked} filter/size combinations equal; the tables folder in {pages} pages of 10",
        d.info().id
    );
}

/// Creates the objects, checks every page and drops them, also when a check
/// fails.
async fn exercise(id: &str, env: &str) {
    let Some(cfg) = cfg(env, id) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver(id);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    eprintln!("{id}: {}", admin.server_version().await.unwrap());
    for sql in cleanup() {
        let _ = run(&mut admin, &sql).await;
    }
    let outcome = tokio::spawn(check(d.clone(), cfg)).await;
    let mut dropped = Ok(());
    for sql in cleanup() {
        if let Err(e) = run(&mut admin, &sql).await {
            dropped = Err(e);
        }
    }
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
    dropped.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn trino() {
    exercise("trino", "DBINE_TEST_TRINO_URL").await;
    // Starburst is Trino.
    exercise("starburst", "DBINE_TEST_TRINO_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn presto() {
    exercise("presto", "DBINE_TEST_PRESTO_URL").await;
}
