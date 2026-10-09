//! `Session::run_read_only` against real servers: one statement in a
//! read-only transaction the server enforces, rolled back afterwards.
//! Reads `DBINE_TEST_POSTGRES_URL` and `DBINE_TEST_COCKROACH_URL`; each
//! engine is skipped without its variable:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//!   cargo test -p dbine-driver-postgres --test read_only -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, Driver, Error, QueryOutcome, Session, TxState};
use std::sync::Arc;
use std::time::Duration;

fn cfg(env: &str, driver: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    out
}

async fn read(s: &mut Box<dyn Session>, sql: &str, max_rows: usize) -> (dbine_driver::Result<()>, QueryOutcome) {
    let mut out = QueryOutcome::default();
    let res = s.run_read_only(sql, max_rows, &mut out).await;
    (res, out)
}

async fn count(s: &mut Box<dyn Session>, t: &str) -> String {
    let out = run(s, &format!("SELECT count(*) FROM {t}")).await;
    out.results[0].rows[0][0].as_str().unwrap().to_string()
}

/// The statement must fail and leave the table as it was.
async fn refused(s: &mut Box<dyn Session>, t: &str, sql: &str) -> String {
    let before = count(s, t).await;
    let (res, _) = read(s, sql, 100).await;
    let e = res.expect_err(sql);
    assert!(!matches!(e, Error::Unsupported(_)), "{sql}: {e}");
    assert_eq!(count(s, t).await, before, "{sql} changed {t}");
    e.to_string()
}

/// The checks every engine passes; `write_fn` creates a function that
/// inserts into `t` (`None` where the engine can't).
async fn exercise(id: &str, cfg: ConnectionConfig, t: &str, write_fn: Option<String>) {
    let d = driver(id);
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(&mut s, &format!("DROP TABLE IF EXISTS {t}")).await;
    run(&mut s, &format!("CREATE TABLE {t} (id int PRIMARY KEY, name text)")).await;
    run(&mut s, &format!("INSERT INTO {t} VALUES (1, 'uno'), (2, 'dos'), (3, 'tres')")).await;

    // A read: rows, columns with their types, max_rows, as `execute` gives them.
    let (res, out) = read(&mut s, &format!("SELECT id, name FROM {t} ORDER BY id"), 2).await;
    res.unwrap();
    let mut exec = QueryOutcome::default();
    s.execute(&format!("SELECT id, name FROM {t} ORDER BY id"), 2, &mut exec).await.unwrap();
    assert_eq!(out.results.len(), 1, "{out:?}");
    let r = &out.results[0];
    assert_eq!(r.rows, exec.results[0].rows);
    assert_eq!(r.rows.len(), 2);
    assert!(r.truncated, "max_rows truncates");
    assert_eq!(r.rows[0], vec![serde_json::json!("1"), serde_json::json!("uno")]);
    let names: Vec<_> = r.columns.iter().map(|c| (c.name.as_str(), c.type_name.as_str())).collect();
    assert_eq!(names, exec.results[0].columns.iter().map(|c| (c.name.as_str(), c.type_name.as_str())).collect::<Vec<_>>());
    assert_eq!(names[0].0, "id");
    assert!(names[0].1.starts_with("int"), "{names:?}");
    // Reads that aren't SELECT.
    for sql in ["SHOW search_path", "VALUES (1, 'a')", &format!("TABLE {t}"), &format!("EXPLAIN SELECT * FROM {t}")] {
        let (res, out) = read(&mut s, sql, 100).await;
        res.unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert!(!out.results.is_empty() && !out.results[0].rows.is_empty(), "{sql}: {out:?}");
    }

    // Writes, through any door.
    let e = refused(&mut s, t, &format!("DELETE FROM {t}")).await;
    assert!(e.contains("read-only") || e.contains("read only"), "{e}");
    refused(&mut s, t, &format!("UPDATE {t} SET name = 'x'")).await;
    refused(&mut s, t, &format!("WITH d AS (DELETE FROM {t} RETURNING id) SELECT * FROM d")).await;
    refused(&mut s, t, &format!("SELECT 1; DELETE FROM {t}")).await;
    refused(&mut s, t, &format!("COMMIT; DELETE FROM {t}")).await;
    refused(&mut s, t, &format!("COPY {t} TO STDOUT")).await;
    refused(&mut s, t, "SET TRANSACTION READ WRITE").await;
    refused(&mut s, t, "SELECT dblink_exec('dbname=x', 'DELETE FROM t')").await;
    refused(&mut s, t, "SELECT set_config('transaction_read_only', 'off', false)").await;
    refused(&mut s, t, "SELECT set_config('transaction_read_only', 'off', true)").await;
    refused(
        &mut s,
        t,
        &format!("WITH c AS (SELECT set_config('transaction_read_only', 'off', true)) INSERT INTO {t} SELECT 9, 'x' FROM c"),
    )
    .await;
    if let Some(f) = write_fn {
        run(&mut s, &f).await;
        let e = refused(&mut s, t, &format!("SELECT {t}_w()")).await;
        assert!(e.contains("read-only") || e.contains("read only"), "{e}");
        run(&mut s, &format!("DROP FUNCTION {t}_w")).await;
    }

    // The session is out of the transaction: it writes again.
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    run(&mut s, &format!("INSERT INTO {t} VALUES (4, 'cuatro')")).await;
    assert_eq!(count(&mut s, t).await, "4");

    // A read dropped half-way (a timeout) still ends its transaction.
    let mut out = QueryOutcome::default();
    let slow = tokio::time::timeout(Duration::from_millis(200), s.run_read_only("SELECT pg_sleep(1)", 10, &mut out)).await;
    assert!(slow.is_err(), "the read is dropped");
    run(&mut s, &format!("INSERT INTO {t} VALUES (5, 'cinco')")).await;
    let other = d.connect(&cfg, None).await.unwrap();
    let mut other = other;
    assert_eq!(count(&mut other, t).await, "5", "the insert after the dropped read is committed");

    // In a transaction the user opened, the read is refused and the
    // transaction is untouched.
    s.set_autocommit(false).await.unwrap();
    run(&mut s, &format!("INSERT INTO {t} VALUES (6, 'seis')")).await;
    let (res, _) = read(&mut s, "SELECT 1", 10).await;
    assert!(matches!(res, Err(Error::State(_))), "{res:?}");
    s.commit().await.unwrap();
    s.set_autocommit(true).await.unwrap();
    assert_eq!(count(&mut other, t).await, "6");

    run(&mut s, &format!("DROP TABLE {t}")).await;
}

/// What the driver relies on, straight from the server: a Parse message
/// with two statements is refused, and (PostgreSQL) a read-only
/// transaction that took its snapshot can't turn read-write. CockroachDB
/// lets it (`SET TRANSACTION READ WRITE`, `set_config`) but not within the
/// statement that does it, and nothing runs after that statement: the
/// driver refuses both anyway.
async fn server_rules(url: &str, snapshot_fixes_mode: bool) {
    let (client, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(conn);
    assert!(client.prepare("SELECT 1; SELECT 2").await.is_err(), "two statements in one Parse");
    client.batch_execute("START TRANSACTION READ ONLY").await.unwrap();
    client.simple_query("SELECT current_setting('transaction_read_only')").await.unwrap();
    let e = client.batch_execute("SET TRANSACTION READ WRITE").await;
    client.batch_execute("ROLLBACK").await.unwrap();
    assert_eq!(e.is_err(), snapshot_fixes_mode, "SET TRANSACTION READ WRITE after the snapshot: {e:?}");
    client.batch_execute("START TRANSACTION READ ONLY").await.unwrap();
    client.simple_query("SELECT current_setting('transaction_read_only')").await.unwrap();
    let e = client.simple_query("SELECT set_config('transaction_read_only', 'off', true)").await;
    client.batch_execute("ROLLBACK").await.unwrap();
    assert_eq!(e.is_err(), snapshot_fixes_mode, "set_config read-write after the snapshot: {e:?}");
    // Within one statement, turning read-write doesn't let it write.
    client.batch_execute("DROP TABLE IF EXISTS dbine_ro_rules; CREATE TABLE dbine_ro_rules (id int)").await.unwrap();
    client.batch_execute("START TRANSACTION READ ONLY").await.unwrap();
    client.simple_query("SELECT 1").await.unwrap();
    let e = client
        .simple_query("WITH c AS (SELECT set_config('transaction_read_only', 'off', true)) INSERT INTO dbine_ro_rules SELECT 1 FROM c")
        .await;
    client.batch_execute("ROLLBACK").await.unwrap();
    assert!(e.is_err(), "write in the statement that turns read-write");
    client.batch_execute("DROP TABLE dbine_ro_rules").await.unwrap();
}

#[tokio::test]
#[ignore = "needs DBINE_TEST_POSTGRES_URL"]
async fn postgres_reads_are_read_only() {
    let Some(cfg) = cfg("DBINE_TEST_POSTGRES_URL", "postgres") else { return };
    server_rules(&std::env::var("DBINE_TEST_POSTGRES_URL").unwrap(), true).await;
    let t = "dbine_ro_t";
    let f = format!(
        "CREATE OR REPLACE FUNCTION {t}_w() RETURNS int LANGUAGE plpgsql AS $$ BEGIN INSERT INTO {t} VALUES (99, 'w'); RETURN 1; END $$"
    );
    exercise("postgres", cfg, t, Some(f)).await;
}

#[tokio::test]
#[ignore = "needs DBINE_TEST_COCKROACH_URL"]
async fn cockroach_reads_are_read_only() {
    let Some(cfg) = cfg("DBINE_TEST_COCKROACH_URL", "cockroachdb") else { return };
    server_rules(&std::env::var("DBINE_TEST_COCKROACH_URL").unwrap(), false).await;
    let t = "dbine_ro_t";
    let f = format!("CREATE OR REPLACE FUNCTION {t}_w() RETURNS INT LANGUAGE SQL AS $$ INSERT INTO {t} VALUES (99, 'w') RETURNING id $$");
    exercise("cockroachdb", cfg, t, Some(f)).await;
}
