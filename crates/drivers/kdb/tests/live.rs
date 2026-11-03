//! Against a real kdb+ (`dbine-test-kdb`). kdb+ needs a license from KX
//! (free for personal and non-commercial use, by registration): without
//! one there's no container, so these run by hand.
//!
//! ```sh
//! # KX's image, with the license file (kc.lic / k4.lic) KX emails:
//! docker run -d --name dbine-test-kdb -p 15000:5000 \
//!   -v "$HOME/kx/license:/opt/kx/lic" -e QLIC=/opt/kx/lic \
//!   <KX image> -p 5000 -u /opt/kx/users.txt   # users.txt: "u:p"
//! DBINE_TEST_KDB_URL=127.0.0.1:15000:u:p \
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
        "dbine_t:([id:1 2 3] sym:`a`b`c; px:1.5 0n 3.0; ts:3#.z.p; d:2024.01.31 0Nd 2024.02.01; note:(\"x\";\"\";\"z\"))",
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
