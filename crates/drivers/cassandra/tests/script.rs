//! Editor scripts against a real server, as in tests/integration.rs:
//! cqlsh's commands (CONSISTENCY, PAGING), batches, and errors with their
//! code and place.
//!
//! `DBINE_TEST_SCYLLADB_URL=localhost:25413 cargo test -p dbine-driver-cassandra --test script -- --ignored`
//! (or `DBINE_TEST_CASSANDRA_URL`).

use dbine_driver::{ConnectionConfig, Driver, QueryOutcome, Session};
use serde_json::json;
use std::sync::Arc;

fn find(driver: &str) -> Arc<dyn Driver> {
    dbine_driver_cassandra::drivers()
        .into_iter()
        .find(|d| d.info().id == driver)
        .unwrap()
}

async fn open(driver: &str, url: &str) -> Box<dyn Session> {
    let (host, port) = url.rsplit_once(':').unwrap();
    let c = ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().unwrap(),
        ..Default::default()
    };
    find(driver).connect(&c, None).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.map(|_| out)
}

async fn check(driver: &str, url: &str) {
    let mut s = open(driver, url).await;
    run(
        &mut s,
        "CREATE KEYSPACE IF NOT EXISTS dbine_script WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1};
         USE dbine_script;
         DROP TABLE IF EXISTS t;
         CREATE TABLE t (k int, c int, v text, PRIMARY KEY (k, c))",
    )
    .await
    .unwrap();

    // cqlsh commands on their own line, no `;`; a batch whole.
    let text = "CONSISTENCY ONE\nBEGIN BATCH\n  INSERT INTO t (k, c, v) VALUES (1, 1, 'a;b');\n  INSERT INTO t (k, c, v) VALUES (1, 2, 'c');\nAPPLY BATCH;\nSELECT count(*) FROM t";
    let out = run(&mut s, text).await.unwrap();
    assert!(
        out.log
            .iter()
            .any(|m| m.text == "Nivel de consistencia: ONE."),
        "{:?}",
        out.log
    );
    assert_eq!(out.results.last().unwrap().rows[0][0], json!(2));
    assert_eq!(
        (
            out.results.last().unwrap().statement,
            out.results.last().unwrap().line
        ),
        (Some(2), Some(6))
    );
    // The editor's units are the statements execute numbers, at the same
    // places (the run lens, the statement at the cursor, named results).
    let units = find(driver).split_script(text);
    assert_eq!(units.len(), 3);
    assert_eq!(out.results.len(), 2, "the batch and the SELECT");
    for r in &out.results {
        let u = &units[r.statement.unwrap()];
        assert_eq!((r.offset, r.line), (Some(u.start), Some(u.line)));
    }
    let text =
        "PAGING OFF\nSELECT count(*) FROM t;\n\n-- c\nSELECT v FROM t WHERE k = 1 AND c = 1;";
    let out = run(&mut s, text).await.unwrap();
    let units = find(driver).split_script(text);
    assert_eq!(units.len(), 3);
    let placed: Vec<_> = out
        .results
        .iter()
        .map(|r| (r.statement, r.offset, r.line))
        .collect();
    assert_eq!(
        placed,
        [
            (Some(1), Some(units[1].start), Some(2)),
            (Some(2), Some(units[2].start), Some(5))
        ]
    );
    assert_eq!(out.results[1].rows[0][0], json!("a;b"));
    // Each unit alone runs as it does in the script.
    for u in &units {
        run(&mut s, &u.text).await.unwrap();
    }
    run(&mut s, "PAGING ON").await.unwrap();
    // The level lasts across runs.
    let out = run(&mut s, "CONSISTENCY").await.unwrap();
    assert_eq!(out.log[0].text, "Nivel de consistencia actual: ONE.");
    // THREE can't be met with one replica: the server's Unavailable.
    run(&mut s, "CONSISTENCY THREE").await.unwrap();
    let e = run(&mut s, "SELECT * FROM t")
        .await
        .unwrap_err()
        .to_script_error();
    assert_eq!(e.code.as_deref(), Some("1000"), "unavailable: {e:?}");
    run(&mut s, "consistency local_quorum;").await.unwrap();

    // PAGING n: pages of n rows, the row limit still holds.
    for c in 3..30 {
        run(
            &mut s,
            &format!("INSERT INTO t (k, c, v) VALUES (1, {c}, 'x')"),
        )
        .await
        .unwrap();
    }
    run(&mut s, "PAGING 5").await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("SELECT * FROM t WHERE k = 1", 12, &mut out)
        .await
        .unwrap();
    assert_eq!(out.results[0].rows.len(), 12);
    assert!(out.results[0].truncated);
    run(&mut s, "PAGING OFF").await.unwrap();
    assert!(run(&mut s, "PAGING maybe").await.is_err());

    // A syntax error: code 2000 and its place in the script.
    let mut out = QueryOutcome::default();
    let e = s
        .execute("SELECT * FROM t LIMIT 1;\nSELEC * FROM t", 10, &mut out)
        .await
        .unwrap_err()
        .to_script_error();
    assert_eq!(
        (e.code.as_deref(), e.line),
        (Some("2000"), Some(2)),
        "{e:?}"
    );
    assert!(e.offset.is_some_and(|o| o >= 25), "{e:?}");
    assert_eq!(out.results.len(), 1, "stops at the error");
    // An invalid query: 2200.
    let e = run(&mut s, "SELECT nope FROM t")
        .await
        .unwrap_err()
        .to_script_error();
    assert_eq!(e.code.as_deref(), Some("2200"), "{e:?}");
    // Other cqlsh commands are refused on their line.
    let e = run(&mut s, "SELECT * FROM t LIMIT 1;\nCOPY t TO 'x.csv'")
        .await
        .unwrap_err()
        .to_script_error();
    assert_eq!(e.line, Some(2));
    run(&mut s, "DROP KEYSPACE dbine_script").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn scylladb_scripts() {
    let Ok(url) = std::env::var("DBINE_TEST_SCYLLADB_URL") else {
        return;
    };
    check("scylladb", &url).await;
}

#[tokio::test]
#[ignore]
async fn cassandra_scripts() {
    let Ok(url) = std::env::var("DBINE_TEST_CASSANDRA_URL") else {
        return;
    };
    check("cassandra", &url).await;
}
