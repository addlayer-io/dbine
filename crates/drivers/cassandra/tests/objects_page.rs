//! The explorer's pages (`list_objects_page`) against real servers: for
//! every filter (each kind, no schema, a schema, "Otros", searches,
//! lookups) each page equals what the app pages out of `list_objects`
//! (`objects::page_of`), and the 26 tables come in pages of 10. The objects
//! live in a scratch keyspace (`dbine_pages`), dropped at the end even when
//! a check fails. Skipped without the variables:
//!
//! ```sh
//! docker start dbine-test-cassandra dbine-test-scylladb
//! DBINE_TEST_CASSANDRA_URL=localhost:25402 DBINE_TEST_SCYLLADB_URL=localhost:25413 \
//!   cargo test -p dbine-driver-cassandra --test objects_page -- --ignored --nocapture
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{kinds, ConnectionConfig, DbObject, ObjectQuery, QueryOutcome, Session};

const KS: &str = "dbine_pages";

async fn open(driver: &str, url: &str, ks: Option<&str>) -> Box<dyn Session> {
    let (host, port) = url.rsplit_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().unwrap(),
        ..Default::default()
    };
    let d = dbine_driver_cassandra::drivers()
        .into_iter()
        .find(|d| d.info().id == driver)
        .unwrap();
    d.connect(&cfg, ks).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, cql: &str) -> dbine_driver::Result<()> {
    let mut out = QueryOutcome::default();
    s.execute(cql, 10, &mut out).await
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

/// The scratch keyspace's objects, then every filter in pages of 7 and the
/// tables in pages of 10.
async fn check(driver: String, url: String) {
    let mut s = open(&driver, &url, Some(KS)).await;
    // 25 tables, in mixed case, plus a table and a type with one name.
    let mut cql: Vec<String> = (0..25)
        .map(|i| {
            let name = if i % 2 == 0 {
                format!("\"Ev_{i:02}\"")
            } else {
                format!("ev_{i:02}_x")
            };
            format!("CREATE TABLE {name} (id int PRIMARY KEY, v text)")
        })
        .collect();
    cql.extend(
        [
            "CREATE TABLE dup (id int PRIMARY KEY, v text)",
            "CREATE TYPE dup (a int)",
            "CREATE TYPE \"Addr\" (street text)",
            "CREATE INDEX ix_ev ON ev_01_x (v)",
        ]
        .map(String::from),
    );
    run(&mut s, &cql.join(";\n"))
        .await
        .expect("the test objects");
    // Switched off by default (Cassandra 5, Scylla); checked when on.
    let mv = run(
        &mut s,
        "CREATE MATERIALIZED VIEW \"Mv_Ev\" AS SELECT id, v FROM ev_03_x \
         WHERE v IS NOT NULL AND id IS NOT NULL PRIMARY KEY (v, id)",
    )
    .await;
    let udf = if driver == "cassandra" {
        "CREATE FUNCTION dup(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE java AS 'return x;';\n\
         CREATE FUNCTION dup(x text) RETURNS NULL ON NULL INPUT RETURNS text LANGUAGE java AS 'return x;'"
    } else {
        "CREATE FUNCTION dup(x int) RETURNS NULL ON NULL INPUT RETURNS int LANGUAGE lua AS 'return x';\n\
         CREATE FUNCTION dup(x text) RETURNS NULL ON NULL INPUT RETURNS text LANGUAGE lua AS 'return x'"
    };
    let udf = run(&mut s, udf).await;
    eprintln!(
        "{driver}: materialized view {}, functions {}",
        mv.as_ref()
            .map_or_else(|e| e.to_string(), |_| "made".into()),
        udf.as_ref()
            .map_or_else(|e| e.to_string(), |_| "made".into()),
    );

    let all = s.list_objects().await.expect("list_objects");
    let count = |k: &str| all.iter().filter(|o| o.kind == k).count();
    assert_eq!(count(kinds::TABLE), 26, "{all:?}");
    assert_eq!(count("type"), 2, "{all:?}");
    assert_eq!(count(kinds::MATERIALIZED_VIEW), usize::from(mv.is_ok()));
    assert_eq!(count(kinds::FUNCTION), usize::from(udf.is_ok()));
    assert!(!all.iter().any(|o| o.name.ends_with("_index")), "{all:?}");

    let tables = q(|q| q.kind = Some(kinds::TABLE.into()));
    assert_eq!(
        same_pages(&mut s, &all, tables, 10).await,
        3,
        "26 tables by 10"
    );
    let declared: Vec<String> = ["table", "materialized_view", "type", "function"]
        .map(String::from)
        .to_vec();
    let mut queries = vec![
        ObjectQuery::default(),
        q(|q| q.not_kinds = declared.clone()),
        q(|q| q.not_kinds = vec![kinds::TABLE.into()]),
        q(|q| q.no_schema = true),
        q(|q| q.schema = Some(KS.into())),
        q(|q| {
            q.no_schema = true;
            q.schema = Some(KS.into());
            q.kind = Some("type".into());
        }),
        q(|q| q.kind = Some("collection".into())),
        q(|q| {
            q.kind = Some(kinds::TABLE.into());
            q.pattern = Some("EV_1".into());
        }),
    ];
    for k in &declared {
        queries.push(q(|q| q.kind = Some(k.clone())));
    }
    for p in ["ev", "EV", "_", "%", "é", "", " ", "zzz", "\"", "x"] {
        queries.push(q(|q| q.pattern = Some(p.into())));
    }
    for n in [
        "dup", "DUP", "ev_01_x", "EV_00", "addr", "mv_ev", "nope", "",
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

/// Runs the check in a scratch keyspace and drops it whatever happens.
async fn on_scratch_keyspace(driver: &str, url: &str) {
    let mut admin = open(driver, url, None).await;
    let drop = format!("DROP KEYSPACE IF EXISTS {KS}");
    run(&mut admin, &drop).await.unwrap();
    // Without tablets: Scylla's materialized views and indexes need vnodes.
    let tablets = if driver == "scylladb" {
        " AND tablets = {'enabled': false}"
    } else {
        ""
    };
    run(
        &mut admin,
        &format!("CREATE KEYSPACE {KS} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}{tablets}"),
    )
    .await
    .unwrap();
    let outcome = tokio::spawn(check(driver.into(), url.into())).await;
    run(&mut admin, &drop).await.unwrap();
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

#[tokio::test]
#[ignore]
async fn cassandra_pages_are_the_whole_lists_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_CASSANDRA_URL") else {
        return;
    };
    on_scratch_keyspace("cassandra", &url).await;
}

#[tokio::test]
#[ignore]
async fn scylladb_pages_are_the_whole_lists_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_SCYLLADB_URL") else {
        return;
    };
    on_scratch_keyspace("scylladb", &url).await;
}
