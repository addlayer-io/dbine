//! `Session::run_read_only` against real servers. Reads
//! `DBINE_TEST_MYSQL_URL` / `DBINE_TEST_MARIADB_URL`
//! (`mysql://root:pw@localhost:25011`, see `integration.rs`) and skips
//! without them:
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//!   cargo test -p dbine-driver-mysql --test read_only -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, Driver, Error, QueryOutcome, Session, TxState};
use serde_json::{json, Value};
use std::sync::Arc;

fn parse_url(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostpart.rsplit_once(':').map_or((hostpart, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port,
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_mysql::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<QueryOutcome, Error> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out).await.map(|_| out)
}

async fn read(s: &mut Box<dyn Session>, sql: &str, max: usize) -> Result<QueryOutcome, Error> {
    let mut out = QueryOutcome::default();
    s.run_read_only(sql, max, &mut out).await.map(|_| out)
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> Value {
    run(s, sql).await.unwrap().results[0].rows[0][0].clone()
}

/// Columns and rows of every result, to compare runs.
fn shape(out: &QueryOutcome) -> Value {
    json!(out
        .results
        .iter()
        .map(|r| json!({"columns": serde_json::to_value(&r.columns).unwrap(), "rows": r.rows, "total": r.total_rows, "truncated": r.truncated}))
        .collect::<Vec<_>>())
}

async fn exercise(id: &str, env: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let mut cfg = parse_url(id, &url);
    let d = driver(id);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    run(&mut admin, "DROP DATABASE IF EXISTS dbine_ro; CREATE DATABASE dbine_ro").await.unwrap();
    let mut s = d.connect(&cfg, Some("dbine_ro")).await.unwrap();
    run(
        &mut s,
        "CREATE TABLE ti (id INT PRIMARY KEY, name VARCHAR(20), f FLOAT, dt DATETIME(3), d DATE, t TIME(2), b VARBINARY(8), m DECIMAL(10,2), u BIGINT UNSIGNED, j JSON) ENGINE=InnoDB;
         INSERT INTO ti VALUES (1, 'a', 1.1, '2024-03-09 13:05:07.120', '2024-03-09', '-26:03:04.5', x'CAFE', 1.50, 18446744073709551615, '{\"k\": 1}'),
                               (2, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL), (3, 'c', -0.5, '2000-01-01 00:00:00', '1999-12-31', '00:00:00', x'', 0, 0, '[]');
         CREATE TABLE tm (id INT PRIMARY KEY) ENGINE=MyISAM;
         INSERT INTO tm VALUES (1);",
    )
    .await
    .unwrap();
    for f in [
        "CREATE FUNCTION wi() RETURNS INT DETERMINISTIC MODIFIES SQL DATA BEGIN INSERT INTO ti (id) VALUES (100); RETURN 1; END",
        "CREATE FUNCTION wm() RETURNS INT DETERMINISTIC MODIFIES SQL DATA BEGIN INSERT INTO tm VALUES (100); RETURN 1; END",
        "CREATE FUNCTION wt() RETURNS INT DETERMINISTIC MODIFIES SQL DATA BEGIN CREATE TEMPORARY TABLE IF NOT EXISTS tt (a INT); INSERT INTO tt VALUES (1); RETURN 1; END",
    ] {
        run(&mut s, f).await.unwrap();
    }

    // Reads: the same results as `execute`, max_rows included.
    for (sql, max) in [
        ("SELECT * FROM ti ORDER BY id", 1000),
        ("SELECT * FROM ti ORDER BY id", 2),
        ("SELECT id, name FROM ti WHERE id > 1 ORDER BY id", 1000),
        ("SELECT COUNT(*) AS n, SUM(m), NOW() > '2000-01-01', 1.5e0, CAST(1.25 AS DECIMAL(5,3)) FROM ti", 1000),
        ("WITH c AS (SELECT id FROM ti) SELECT * FROM c ORDER BY id", 1000),
        ("SHOW TABLES", 1000),
        ("EXPLAIN SELECT * FROM ti", 1000),
        ("SELECT * FROM ti WHERE id = 99", 1000),
    ] {
        let mut by_execute = QueryOutcome::default();
        s.execute(sql, max, &mut by_execute).await.unwrap();
        let by_read = read(&mut s, sql, max).await.unwrap_or_else(|e| panic!("{id}: {sql}: {e}"));
        assert_eq!(shape(&by_read), shape(&by_execute), "{id}: {sql}");
    }
    let out = read(&mut s, "SELECT * FROM ti ORDER BY id", 2).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2);
    assert!(out.results[0].truncated, "{id}");
    let row = &out.results[0].rows[0];
    eprintln!("{id}: {row:?}");
    assert_eq!(row[2], json!(1.1), "{id}: FLOAT");
    assert_eq!(row[3], json!("2024-03-09 13:05:07.120"), "{id}: DATETIME(3)");
    assert_eq!(row[6], json!("0xCAFE"), "{id}: VARBINARY");
    assert_eq!(row[7], json!("1.50"), "{id}: DECIMAL");

    // Refused before reaching the server, or by it.
    for sql in [
        "DELETE FROM ti",
        "UPDATE ti SET name = 'x'",
        "INSERT INTO ti (id) VALUES (50)",
        "CREATE TABLE x (a INT)",
        "DROP TABLE ti",
        "TRUNCATE TABLE tm",
        "COMMIT",
        "SET SESSION TRANSACTION READ WRITE",
        "SELECT * FROM ti INTO OUTFILE '/var/lib/mysql-files/dbine_ro.txt'",
        "SELECT * FROM ti INTO DUMPFILE '/tmp/dbine_ro.bin'",
        "SELECT * FROM ti FOR UPDATE",
        "SELECT * FROM ti LOCK IN SHARE MODE",
        "SELECT GET_LOCK('dbine_ro', 0)",
        "SELECT 1; DELETE FROM ti",
        "/*!CREATE*/ TABLE x (a INT)",
        // Only the server stops these: one statement per prepare, and the
        // read-only transaction.
        "SELECT 1; SELECT 2",
        "SELECT wi()",
        "SELECT wm()",
        "SELECT wt()",
    ] {
        let e = read(&mut s, sql, 100).await.expect_err(sql);
        eprintln!("{id}: {sql} -> {e}");
        assert!(!matches!(e, Error::Unsupported(_)), "{id}: {sql}: {e}");
    }
    let e = read(&mut s, "SELECT wi()", 100).await.unwrap_err().to_string();
    assert!(e.contains("READ ONLY"), "{id}: {e}");

    // Nothing changed, and the session is as it was: read-write, no open
    // transaction, usable for both kinds of runs.
    assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM ti").await, json!(3), "{id}");
    assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM tm").await, json!(1), "{id}");
    assert_eq!(run(&mut s, "SHOW TABLES").await.unwrap().results[0].rows.len(), 2, "{id}");
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle), "{id}");
    assert_eq!(scalar(&mut s, "SELECT @@session.transaction_read_only").await, json!(0), "{id}");
    run(&mut s, "INSERT INTO ti (id) VALUES (10); DELETE FROM ti WHERE id = 10").await.unwrap();
    assert_eq!(read(&mut s, "SELECT COUNT(*) FROM ti", 10).await.unwrap().results[0].rows[0][0], json!(3));

    // Cancelled (KILL QUERY): rolled back, the session as it was.
    let stop = s.interrupter().expect("interrupter");
    let t = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        stop();
    });
    let started = std::time::Instant::now();
    let e = read(&mut s, "SELECT SLEEP(10)", 10).await.unwrap_err();
    t.await.unwrap();
    assert!(matches!(e, Error::Cancelled), "{id}: {e}");
    assert!(started.elapsed() < std::time::Duration::from_secs(8), "{id}");
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle), "{id}");
    assert_eq!(scalar(&mut s, "SELECT @@session.transaction_read_only").await, json!(0), "{id}");

    // Dropped half-way (the app's timeout): the next call settles it first.
    let dropped = tokio::time::timeout(std::time::Duration::from_millis(300), read(&mut s, "SELECT SLEEP(2)", 10)).await;
    assert!(dropped.is_err(), "{id}");
    assert_eq!(scalar(&mut s, "SELECT @@session.transaction_read_only").await, json!(0), "{id}: after a dropped read");
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle), "{id}");
    run(&mut s, "INSERT INTO ti (id) VALUES (12); DELETE FROM ti WHERE id = 12").await.unwrap();
    let dropped = tokio::time::timeout(std::time::Duration::from_millis(300), read(&mut s, "SELECT SLEEP(2)", 10)).await;
    assert!(dropped.is_err(), "{id}");
    assert_eq!(read(&mut s, "SELECT COUNT(*) FROM ti", 10).await.unwrap().results[0].rows[0][0], json!(3), "{id}");
    // The new connection keeps the database, and cancelling reaches it.
    assert_eq!(scalar(&mut s, "SELECT DATABASE()").await, json!("dbine_ro"), "{id}");
    let stop = s.interrupter().expect("interrupter");
    let t = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        stop();
    });
    let started = std::time::Instant::now();
    assert!(matches!(read(&mut s, "SELECT SLEEP(10)", 10).await, Err(Error::Cancelled)), "{id}");
    t.await.unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(8), "{id}");

    // A transaction the user left open isn't committed by START TRANSACTION.
    s.set_autocommit(false).await.unwrap();
    run(&mut s, "INSERT INTO ti (id) VALUES (11)").await.unwrap();
    let e = read(&mut s, "SELECT 1", 10).await.unwrap_err();
    eprintln!("{id}: open transaction -> {e}");
    s.rollback().await.unwrap();
    s.set_autocommit(true).await.unwrap();
    assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM ti").await, json!(3), "{id}");

    // A read-only connection stays read-only after a protected read.
    cfg.read_only = true;
    let mut ro = d.connect(&cfg, Some("dbine_ro")).await.unwrap();
    read(&mut ro, "SELECT 1", 10).await.unwrap();
    assert_eq!(scalar(&mut ro, "SELECT @@session.transaction_read_only").await, json!(1), "{id}");

    run(&mut admin, "DROP DATABASE dbine_ro").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn mysql_read_only_reads() {
    exercise("mysql", "DBINE_TEST_MYSQL_URL").await;
}

#[tokio::test]
#[ignore]
async fn mariadb_read_only_reads() {
    exercise("mariadb", "DBINE_TEST_MARIADB_URL").await;
}
