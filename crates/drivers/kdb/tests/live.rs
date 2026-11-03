//! Against a real kdb+ (`dbine-test-kdb`). kdb+ needs a license from KX
//! (KDB-X Community Edition, free by registration): the portal token and
//! the license go in the repo's `.env` as `KX_PORTAL_TOKEN` and
//! `KX_LICENSE_B64`, and `tests/docker/Dockerfile` installs it.
//!
//! ```sh
//! set -a; . ./.env; set +a
//! docker build -t dbine-test-kdb-img crates/drivers/kdb/tests/docker \
//!   --secret id=kxtoken,env=KX_PORTAL_TOKEN --secret id=kxlic,env=KX_LICENSE_B64
//! docker run -d --name dbine-test-kdb -p 15000:5000 -e KX_LICENSE_B64 dbine-test-kdb-img
//! DBINE_TEST_KDB_URL=127.0.0.1:15000:dbine:dbinepass \
//!   cargo test -p dbine-driver-kdb --test live -- --ignored
//! docker rm -f dbine-test-kdb
//! ```

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome};

/// `host:port[:user:password]` from `DBINE_TEST_KDB_URL`.
fn cfg(read_only: bool) -> ConnectionConfig {
    let url =
        std::env::var("DBINE_TEST_KDB_URL").expect("DBINE_TEST_KDB_URL=host:port[:user:password]");
    let mut parts = url.splitn(4, ':');
    let host = parts.next().unwrap_or("127.0.0.1").to_string();
    let port = parts.next().and_then(|p| p.parse().ok()).unwrap_or(5000);
    ConnectionConfig {
        host,
        port,
        username: parts.next().map(str::to_string),
        password: parts.next().map(str::to_string),
        read_only,
        options: [("tcps".to_string(), "false".to_string())].into(),
        ..Default::default()
    }
}

async fn run(s: &mut Box<dyn dbine_driver::Session>, q: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(q, 1000, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    out
}

#[tokio::test]
#[ignore]
async fn tables_types_and_edits() {
    let d = dbine_driver_kdb::drivers().remove(0);
    let mut s = d.connect(&cfg(false), None).await.unwrap();
    assert!(s.server_version().await.unwrap().starts_with("kdb+ "));
    run(
        &mut s,
        "dbine_t:([id:1 2 3] sym:`a`b`c; px:1.5 0n 3.0; ts:3#.z.p; d:2024.01.31 0N 2024.02.01; note:(\"x\";\"\";\"z\"))",
    )
    .await;
    let t = ObjectRef {
        kind: kinds::TABLE.into(),
        schema: None,
        name: "dbine_t".into(),
    };
    assert!(s
        .list_objects()
        .await
        .unwrap()
        .iter()
        .any(|o| o.name == "dbine_t"));
    let cols = s.columns(&t).await.unwrap();
    assert_eq!(cols.iter().filter(|c| c.primary_key).count(), 1);
    let browse = s.browse_query(&t, 10);
    let out = run(&mut s, &browse).await;
    assert_eq!(out.results[0].rows.len(), 3);

    // Filters, sort, edits and inserts are q the server accepts.
    let f = d
        .filtered_browse(
            &browse,
            &[dbine_driver::ColumnFilter {
                column: "sym".into(),
                op: dbine_driver::FilterOp::Eq,
                values: vec![serde_json::json!("b")],
                sql: None,
            }],
        )
        .unwrap();
    assert_eq!(run(&mut s, &f).await.results[0].rows.len(), 1);
    let up = d
        .update_script(
            &t,
            &[dbine_driver::RowChange {
                key: vec![("id".into(), serde_json::json!(2))],
                set: vec![
                    ("px".into(), serde_json::json!(9.5)),
                    ("note".into(), serde_json::json!("y")),
                ],
                row: vec![],
            }],
        )
        .unwrap();
    run(&mut s, &up).await;
    let ins = d
        .insert_script(
            &t,
            &[
                "id".into(),
                "sym".into(),
                "px".into(),
                "ts".into(),
                "d".into(),
                "note".into(),
            ],
            &[vec![
                serde_json::json!(4),
                serde_json::json!("d"),
                serde_json::json!(4.5),
                serde_json::json!("2024-01-31 13:45:00"),
                serde_json::json!("2024-01-31"),
                serde_json::json!("w"),
            ]],
        )
        .unwrap();
    run(&mut s, &ins).await;
    let del = d
        .delete_script(&t, &[vec![("id".into(), serde_json::json!(1))]])
        .unwrap();
    run(&mut s, &del).await;
    let out = run(&mut s, "count dbine_t").await;
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(3));
    run(&mut s, "delete dbine_t from `.").await;
}

#[tokio::test]
#[ignore]
async fn read_only_is_enforced_by_the_server() {
    let d = dbine_driver_kdb::drivers().remove(0);
    let mut s = d.connect(&cfg(true), None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.run_read_only("til 3", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 3);
    // A write is refused (before sending it, and by reval on the server).
    let mut out = QueryOutcome::default();
    let r = s.execute("{x::1}[]", 10, &mut out).await;
    assert!(r.is_err());
    // And a plain read still runs.
    run(&mut s, "count til 10").await;
}

#[tokio::test]
#[ignore]
async fn hdb_namespaces_and_big_results() {
    let d = dbine_driver_kdb::drivers().remove(0);
    let mut s = d.connect(&cfg(false), None).await.unwrap();
    // A partitioned table (two dates) and a splayed one, loaded as an HDB
    // (relative paths: `-u` keeps the server inside its directory);
    // a view and a one-letter user namespace; a result over the grid's size.
    run(
        &mut s,
        "{[dt] .Q.dpft[`:dbine_hdb;dt;`sym;`dbine_q set ([] sym:`a`b`a; px:1 2 3f)]} each 2026.11.01 2026.11.02",
    )
    .await;
    run(
        &mut s,
        "`:dbine_hdb/dbine_s/ set .Q.en[`:dbine_hdb] ([] sym:`x`y; n:1 2)",
    )
    .await;
    run(&mut s, "\\l dbine_hdb").await;
    run(&mut s, "dbine_v::1+1").await;
    run(&mut s, ".u.dbine_t:([] a:1 2)").await;
    run(&mut s, "dbine_big:([] a:til 2000000)").await;

    let objs = s.list_objects().await.unwrap();
    let kind = |n: &str| {
        objs.iter()
            .find(|o| o.name == n)
            .map(|o| (o.kind.clone(), o.schema.clone()))
    };
    assert_eq!(kind("dbine_q").unwrap().0, dbine_driver_kdb::PARTITIONED);
    assert_eq!(kind("dbine_s").unwrap().0, dbine_driver_kdb::SPLAYED);
    assert_eq!(kind("dbine_big").unwrap().0, kinds::TABLE);
    assert!(objs.iter().any(|o| o.name.ends_with("dbine_t")), "{objs:?}");
    assert_eq!(kind("dbine_v").unwrap().0, kinds::VIEW);

    let q = ObjectRef {
        kind: dbine_driver_kdb::PARTITIONED.into(),
        schema: None,
        name: "dbine_q".into(),
    };
    let browse = s.browse_query(&q, 4);
    assert_eq!(run(&mut s, &browse).await.results[0].rows.len(), 4);
    let f = d
        .filtered_browse(
            &browse,
            &[dbine_driver::ColumnFilter {
                column: "date".into(),
                op: dbine_driver::FilterOp::Eq,
                values: vec![serde_json::json!("2026-11-02")],
                sql: None,
            }],
        )
        .unwrap();
    assert_eq!(run(&mut s, &f).await.results[0].rows.len(), 3);
    assert!(d
        .update_script(
            &q,
            &[dbine_driver::RowChange {
                key: vec![("sym".into(), serde_json::json!("a"))],
                set: vec![("px".into(), serde_json::json!(9.0))],
                row: vec![],
            }],
        )
        .is_err());

    // The grid gets the first rows and the count; export gets them all.
    let grid = run(&mut s, "dbine_big").await;
    assert_eq!(grid.results[0].rows.len(), 1000);
    assert!(grid.results[0].truncated);
    assert_eq!(grid.results[0].total_rows, 2_000_000);
    let mut all = QueryOutcome::default();
    s.execute("dbine_big", usize::MAX, &mut all).await.unwrap();
    assert_eq!(all.results[0].rows.len(), 2_000_000);
    run(&mut s, "delete dbine_big from `.").await;

    // A function over several lines, with a blank line and a comment, is
    // one statement; a system command runs; the Monitor reads the server.
    let out = run(
        &mut s,
        "dbine_f:{[x]\n  y:x+1;\n\n/ a comment\n  y*2 }\ndbine_f 3\n\\p",
    )
    .await;
    assert_eq!(out.results.len(), 3, "{:?}", out.results);
    assert_eq!(out.results[1].rows[0][0], serde_json::json!(8));
    assert!(!s.monitor().await.unwrap().metrics.is_empty());
}
