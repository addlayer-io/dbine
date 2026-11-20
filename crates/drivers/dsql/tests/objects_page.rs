//! The explorer's pages (`list_objects_page`) through the password test
//! hook against a plain PostgreSQL (DSQL has no emulator): for every filter
//! combination, the pages, read with a small limit, must be exactly the
//! app's own pages of the whole list (`objects::page_of` over
//! `list_objects`). A scratch schema adds a folder of more than one page
//! (25 views in pages of 10), names that differ only in case, a table and a
//! function with the same name and a name with LIKE wildcards; it is dropped
//! at the end, also when a check fails.
//!   docker run -d --name dbine-test-dsqlpg -e POSTGRES_PASSWORD=dbine -p 25301:5432 postgres:16
//!   DBINE_TEST_DSQL_URL=localhost:25301 cargo test -p dbine-driver-dsql --test objects_page -- --ignored

use dbine_driver::objects::page_of;
use dbine_driver::{ConnectionConfig, DbObject, ObjectQuery, QueryOutcome, Session};
use futures::FutureExt;
use std::collections::BTreeSet;
use std::panic::AssertUnwindSafe;

const SCHEMA: &str = "dbine_pages";

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    match s.execute(sql, 10, &mut out).await {
        Ok(_) if out.error.is_none() => Ok(()),
        Ok(_) => Err(format!("{:?}", out.error)),
        Err(e) => Err(e.to_string()),
    }
}

/// Every page of `q`, by `list_objects_page`.
async fn driver_pages(s: &mut Box<dyn Session>, q: &ObjectQuery) -> Vec<Vec<DbObject>> {
    let mut q = q.clone();
    let mut out = Vec::new();
    loop {
        let p = s
            .list_objects_page(&q)
            .await
            .unwrap_or_else(|e| panic!("{q:?}: {e}"));
        assert!(p.objects.len() <= q.page_size(), "{q:?}: page too big");
        out.push(p.objects);
        assert!(out.len() < 5000, "{q:?}: pages without end");
        match p.next {
            Some(n) => q.after = Some(n),
            None => return out,
        }
    }
}

/// Every page of `q`, as the app makes them out of the whole list.
fn app_pages(all: &[DbObject], q: &ObjectQuery) -> Vec<Vec<DbObject>> {
    let mut q = q.clone();
    let mut out = Vec::new();
    loop {
        let p = page_of(all, &q);
        out.push(p.objects);
        match p.next {
            Some(n) => q.after = Some(n),
            None => return out,
        }
    }
}

/// The pages of `q` with a small limit are the app's; their objects.
async fn same(s: &mut Box<dyn Session>, all: &[DbObject], q: ObjectQuery) -> Vec<DbObject> {
    let q = ObjectQuery {
        limit: if q.limit > 0 { q.limit } else { 7 },
        ..q
    };
    let want = app_pages(all, &q);
    let got = driver_pages(s, &q).await;
    assert_eq!(got, want, "{q:?}");
    want.concat()
}

fn query(f: impl FnOnce(&mut ObjectQuery)) -> ObjectQuery {
    let mut q = ObjectQuery::default();
    f(&mut q);
    q
}

async fn exercise(s: &mut Box<dyn Session>) {
    let sc = SCHEMA;
    run(s, &format!("CREATE SCHEMA {sc}")).await.unwrap();
    for i in 0..25 {
        run(s, &format!("CREATE VIEW {sc}.v{i:02} AS SELECT 1 AS a"))
            .await
            .unwrap();
    }
    for sql in [
        format!("CREATE TABLE {sc}.\"Orders\" (a int)"),
        format!("CREATE VIEW {sc}.orders AS SELECT 1 AS a"),
        format!("CREATE TABLE {sc}.\"pct%_x\" (a int)"),
        format!("CREATE TABLE {sc}.pctax (a int)"),
        format!("CREATE TABLE {sc}.t_same (a int)"),
        format!("CREATE FUNCTION {sc}.t_same() RETURNS int LANGUAGE sql AS 'SELECT 1'"),
        format!("CREATE FUNCTION {sc}.t_same(x int) RETURNS int LANGUAGE sql AS 'SELECT 2'"),
        format!("CREATE SEQUENCE {sc}.s_one"),
        format!("CREATE DOMAIN {sc}.d_one AS int"),
        format!("CREATE MATERIALIZED VIEW {sc}.mv_one AS SELECT 1 AS a"),
    ] {
        run(s, &sql).await.unwrap();
    }
    let all = s.list_objects().await.unwrap();

    // A folder of more than one page.
    let folder = query(|q| {
        q.schema = Some(SCHEMA.into());
        q.kind = Some("view".into());
        q.limit = 10;
    });
    let views = same(s, &all, folder.clone()).await;
    assert_eq!(views.len(), 27, "25 views, orders and mv_one");
    assert_eq!(driver_pages(s, &folder).await.len(), 3);

    let declared = ["table", "view", "sequence", "type", "function"].map(String::from);
    let mut queries = Vec::new();
    for k in &declared {
        queries.push(query(|q| q.kind = Some(k.clone())));
        queries.push(query(|q| {
            q.kind = Some(k.clone());
            q.schema = Some(SCHEMA.into());
        }));
    }
    queries.extend([
        query(|_| {}),
        query(|q| q.schema = Some(SCHEMA.into())),
        query(|q| q.no_schema = true),
        query(|q| q.not_kinds = declared.to_vec()),
        query(|q| {
            q.not_kinds = vec!["view".into(), "table".into()];
            q.schema = Some(SCHEMA.into());
        }),
        query(|q| q.pattern = Some("ORDER".into())),
        query(|q| q.pattern = Some("%_".into())),
        query(|q| q.pattern = Some("_".into())),
        query(|q| {
            q.pattern = Some("V1".into());
            q.kind = Some("view".into());
            q.schema = Some(SCHEMA.into());
        }),
        query(|q| q.name = Some("ORDERS".into())),
        query(|q| q.name = Some("t_same".into())),
        query(|q| q.name = Some("nothing_called_this".into())),
    ]);
    let mut checked = 0;
    for q in queries {
        checked += same(s, &all, q).await.len();
    }
    let mine = same(s, &all, query(|q| q.schema = Some(SCHEMA.into()))).await;
    let names: BTreeSet<&str> = mine.iter().map(|o| o.name.as_str()).collect();
    assert!(
        names.contains("Orders") && names.contains("orders"),
        "{names:?}"
    );
    let same_name: Vec<&str> = mine
        .iter()
        .filter(|o| o.name == "t_same")
        .map(|o| o.kind.as_str())
        .collect();
    assert_eq!(same_name, ["function", "table"], "one function per name");
    let pct = same(s, &all, query(|q| q.pattern = Some("%_".into()))).await;
    assert_eq!(pct.len(), 1, "a wildcard matched {pct:?}");
    eprintln!(
        "dsql: {} objects in the database, {checked} objects checked page by page",
        all.len()
    );
}

#[tokio::test]
#[ignore]
async fn pages_are_the_whole_lists_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_DSQL_URL") else {
        return;
    };
    let (host, port) = url.split_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: "dsql".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some("postgres".into()),
        ..Default::default()
    };
    let mut s = dbine_driver_dsql::connect_with_password(&cfg, "dbine")
        .await
        .unwrap();
    let drop = format!("DROP SCHEMA IF EXISTS {SCHEMA} CASCADE");
    run(&mut s, &drop).await.unwrap();
    let r = AssertUnwindSafe(exercise(&mut s)).catch_unwind().await;
    run(&mut s, &drop).await.unwrap();
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}
