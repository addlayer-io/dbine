//! The explorer's pages (`list_objects_page`) against GizmoSQL (DuckDB
//! behind Flight SQL): for every filter (each kind, a schema, no schema,
//! "Otros", searches, lookups) and page size, each page equals the app's own
//! paging of `list_objects` (`objects::page_of`), cursor included, with no
//! catalog chosen (every catalog's objects) and in each catalog. The objects
//! live in two scratch schemas (`dbine_pages`, `dbine_pages_b`) and an
//! attached in-memory catalog (`dbine_pages_att`), dropped at the end even
//! when a check fails. Skipped without the variable:
//!
//! ```sh
//! DBINE_TEST_FLIGHTSQL_URL=http://localhost:25337 \
//!   cargo test -p dbine-driver-flightsql --test objects_page -- --ignored --nocapture
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, Driver, ObjectQuery, QueryOutcome, Session};
use std::sync::Arc;

const A: &str = "dbine_pages";
const B: &str = "dbine_pages_b";
/// An attached in-memory catalog.
const ATT: &str = "dbine_pages_att";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_FLIGHTSQL_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "flightsql".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(
            std::env::var("DBINE_TEST_FLIGHTSQL_USER").unwrap_or_else(|_| "gizmosql_user".into()),
        ),
        password: Some(
            std::env::var("DBINE_TEST_FLIGHTSQL_PASSWORD").unwrap_or_else(|_| "secreto1".into()),
        ),
        ..Default::default()
    })
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

fn setup() -> Vec<String> {
    let mut v = vec![
        format!("CREATE SCHEMA {A}"),
        format!("CREATE SCHEMA \"{B}\""),
    ];
    // 25 numbered tables (a folder of three pages of 10), mixed case, LIKE's
    // wildcards and a non-ASCII letter; views; names the other schema has.
    v.extend((0..25).map(|i| format!("CREATE TABLE {A}.\"T_{i:02}\" (id INTEGER)")));
    for n in ["a%b", "axb", "dup", "Ünïcode", "orders_old"] {
        v.push(format!("CREATE TABLE {A}.\"{n}\" (id INTEGER)"));
    }
    v.extend((0..9).map(|i| format!("CREATE VIEW {A}.\"v_{i}\" AS SELECT 1 AS x")));
    for n in ["dup", "Orders"] {
        v.push(format!("CREATE TABLE \"{B}\".\"{n}\" (id INTEGER)"));
    }
    v.push(format!("CREATE VIEW \"{B}\".v_orders AS SELECT 1 AS x"));
    // Another catalog (with no catalog chosen, `GetTables` lists every
    // one), with a schema and a table the first one also has.
    v.push(format!("ATTACH ':memory:' AS {ATT}"));
    v.push(format!("CREATE SCHEMA {ATT}.{A}"));
    v.push(format!("CREATE TABLE {ATT}.{A}.dup (id INTEGER)"));
    v.push(format!("CREATE TABLE {ATT}.main.att_t (id INTEGER)"));
    v
}

fn cleanup() -> Vec<String> {
    vec![
        format!("DROP SCHEMA IF EXISTS {A} CASCADE"),
        format!("DROP SCHEMA IF EXISTS \"{B}\" CASCADE"),
        format!("DETACH DATABASE IF EXISTS {ATT}"),
    ]
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
    for schema in [A, B, "DBINE_PAGES", "main", "information_schema", "nope"] {
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
    for p in ["T_1", "%", "_", "a%b", "ORDERS", "dup", "ÜNÏ", "zzz", ""] {
        v.push(ObjectQuery {
            pattern: Some(p.into()),
            ..Default::default()
        });
    }
    for n in ["t_05", "orders", "DUP", "a%b", "nope"] {
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

/// Every filter and size in the catalog `database` (`None`: none chosen).
async fn check_in(d: &Arc<dyn Driver>, c: &ConnectionConfig, database: Option<&str>) -> usize {
    let mut s = d.connect(c, database).await.expect("connect");
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
    eprintln!("{database:?}: {} objects, kinds {kinds:?}", all.len());
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
    // The attached catalog's `dbine_pages` holds one table.
    if database != Some(ATT) {
        assert!(pages >= 3, "a folder of more than one page: {pages}");
        let p = s.list_objects_page(&folder).await.unwrap();
        assert_eq!(p.objects.len(), 10);
    }
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
    eprintln!("{database:?}: {checked} filter/size combinations equal; the tables folder in {pages} pages of 10");
    checked
}

async fn check(d: Arc<dyn Driver>, c: ConnectionConfig) {
    let mut s = d.connect(&c, None).await.expect("connect");
    for sql in setup() {
        if let Err(e) = run(&mut s, &sql).await {
            panic!("{e}");
        }
    }
    check_in(&d, &c, None).await;
    let dbs = s.list_databases().await.unwrap();
    eprintln!("catalogs {dbs:?}");
    for db in dbs.iter().filter(|d| !d.is_empty()) {
        check_in(&d, &c, Some(db)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn gizmosql() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_FLIGHTSQL_URL not set; skipping");
        return;
    };
    let d = dbine_driver_flightsql::drivers().remove(0);
    let mut admin = d.connect(&c, None).await.expect("connect");
    eprintln!("{}", admin.server_version().await.unwrap());
    for sql in cleanup() {
        let _ = run(&mut admin, &sql).await;
    }
    let outcome = tokio::spawn(check(d.clone(), c)).await;
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
