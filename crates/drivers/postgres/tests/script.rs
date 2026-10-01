//! Editor scripts statement by statement, against real servers, the way
//! the app runs them: `split_script`, then `execute` per unit. Each test
//! reads `DBINE_TEST_<ENGINE>_URL` and is skipped without it:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_COCKROACHDB_URL=postgres://root@localhost:26014/defaultdb \
//! DBINE_TEST_TIMESCALEDB_URL=postgres://postgres:pw@localhost:25015/postgres \
//! DBINE_TEST_YUGABYTEDB_URL=postgres://yugabyte@localhost:25016/yugabyte \
//!   cargo test -p dbine-driver-postgres --test script -- --ignored --test-threads 1
//! ```

use dbine_driver::sql::ScriptMode;
use dbine_driver::{ConnectionConfig, Driver, Error, MessageLevel, MessageSinkRef, QueryOutcome, Session, TxState};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn parse_url(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

/// What the app's statement loop does: one `execute` per unit, going on
/// after errors. Returns the errors by statement.
async fn script(d: &dyn Driver, s: &mut Box<dyn Session>, sql: &str, out: &mut QueryOutcome) -> Vec<(usize, Error)> {
    let mut errors = Vec::new();
    for (i, u) in d.split_script(sql).iter().enumerate() {
        out.current_statement = Some(i);
        if let Err(e) = s.execute(&u.text, 100, out).await {
            errors.push((i, e));
        }
    }
    errors
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<QueryOutcome, Error> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map(|_| out)
}

async fn count(s: &mut Box<dyn Session>, table: &str) -> String {
    let out = run(s, &format!("SELECT count(*) FROM {table}")).await.unwrap();
    out.results[0].rows[0][0].as_str().unwrap().to_string()
}

async fn exercise(id: &str, env: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let cfg = parse_url(id, &url);
    let d = driver(id);
    assert_eq!(d.script_mode(), ScriptMode::PerStatement);
    let pg_like = id != "cockroachdb";
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let mut other = d.connect(&cfg, None).await.expect("connect");
    let _ = run(&mut s, "DROP TABLE IF EXISTS dbine_script_t").await;
    if pg_like {
        let _ = run(&mut s, "DROP DATABASE IF EXISTS dbine_script_db").await;
    }

    // Statements that refuse a transaction block, a dollar-quoted body
    // with `;` inside, a failure in the middle that doesn't undo the rest.
    let mut sql = String::from(
        "CREATE TABLE dbine_script_t (id int PRIMARY KEY, name text);\n\
         INSERT INTO dbine_script_t VALUES (1, 'a'), (2, 'b');\n\
         SELECT nope FROM dbine_script_t;\n\
         INSERT INTO dbine_script_t VALUES (3, 'c');\n",
    );
    if pg_like {
        sql.push_str(
            "CREATE DATABASE dbine_script_db;\nVACUUM dbine_script_t;\n\
             CREATE INDEX CONCURRENTLY dbine_script_i ON dbine_script_t (name);\n\
             DO $$ BEGIN RAISE NOTICE 'uno; dos'; RAISE WARNING 'cuidado'; END $$;\n",
        );
    }
    sql.push_str("SELECT id, name FROM dbine_script_t ORDER BY id");
    let mut out = QueryOutcome::default();
    let errors = script(d.as_ref(), &mut s, &sql, &mut out).await;
    assert_eq!(errors.len(), 1, "{id}: {errors:?}");
    let (at, Error::Statement(e)) = &errors[0] else { panic!("{id}: {:?}", errors[0]) };
    assert_eq!(*at, 2);
    assert_eq!(e.sqlstate.as_deref(), Some("42703"), "{id}: {e:?}");
    assert_eq!(e.code, e.sqlstate);
    eprintln!("{id}: error {e:?}");
    if pg_like {
        // "SELECT nope": the server points at `nope`.
        assert_eq!(e.offset, Some(7), "{id}: {e:?}");
        assert_eq!(e.line, Some(1));
    }
    assert_eq!(count(&mut s, "dbine_script_t").await, "3", "{id}: the statements around the error stand");
    let tags: Vec<_> = out.results.iter().map(|r| (r.tag.clone(), r.rows_affected)).collect();
    eprintln!("{id}: tags {tags:?}");
    assert_eq!(tags[0], (Some("CREATE TABLE".into()), None));
    assert_eq!(tags[1], (Some("INSERT 0 2".into()), Some(2)));
    let last = out.results.last().unwrap();
    assert_eq!(last.tag.as_deref(), Some("SELECT 3"));
    let types: Vec<_> = last.columns.iter().map(|c| c.type_name.as_str()).collect();
    // CockroachDB's INT is 64-bit.
    assert_eq!(types, [if pg_like { "int4" } else { "int8" }, "text"], "{id}");
    if pg_like {
        assert!(tags.contains(&(Some("CREATE DATABASE".into()), None)));
        assert!(tags.contains(&(Some("VACUUM".into()), None)));
        assert!(tags.contains(&(Some("CREATE INDEX".into()), None)));
        let notices: Vec<_> = out.log.iter().filter(|m| m.level != MessageLevel::Error).map(|m| (m.level, m.text.as_str(), m.statement)).collect();
        eprintln!("{id}: notices {notices:?}");
        assert!(notices.iter().any(|(l, t, st)| *l == MessageLevel::Info && t.contains("uno; dos") && *st == Some(7)));
        assert!(out.log.iter().any(|m| m.level == MessageLevel::Warning && m.text.contains("cuidado") && m.code.as_deref() == Some("01000")));
        run(&mut s, "DROP DATABASE dbine_script_db").await.unwrap();

        // Notices reach the sink while the statement still runs.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let started = Instant::now();
        let mut out = QueryOutcome::default();
        let sink = seen.clone();
        out.message_sink = Some(MessageSinkRef(Arc::new(move |m| sink.lock().unwrap().push((started.elapsed(), m.text.clone())))));
        s.execute("DO $$ BEGIN RAISE NOTICE 'antes'; PERFORM pg_sleep(1.5); RAISE NOTICE 'después'; END $$", 10, &mut out).await.unwrap();
        let total = started.elapsed();
        let seen = seen.lock().unwrap();
        assert!(seen[0].1.contains("antes") && seen[0].0 + Duration::from_millis(1000) < total, "{id}: {seen:?} / {total:?}");
    }

    // A transaction the user opens in autocommit mode.
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    let mut out = QueryOutcome::default();
    script(d.as_ref(), &mut s, "BEGIN; INSERT INTO dbine_script_t VALUES (4, 'd')", &mut out).await;
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Open), "{id}");
    // CockroachDB readers wait for the open transaction's writes.
    if pg_like {
        assert_eq!(count(&mut other, "dbine_script_t").await, "3");
    }
    let errors = script(d.as_ref(), &mut s, "SELECT 1/0; SELECT 1", &mut out).await;
    assert_eq!(errors.len(), 2, "{id}: {errors:?}");
    let Error::Statement(e) = &errors[1].1 else { panic!() };
    assert_eq!(e.sqlstate.as_deref(), Some("25P02"), "{id}");
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Failed), "{id}");
    // COMMIT of an aborted transaction rolls it back: said, not hidden.
    let mut out = QueryOutcome::default();
    assert!(script(d.as_ref(), &mut s, "COMMIT", &mut out).await.is_empty());
    eprintln!("{id}: commit of a failed tx {:?} / {:?}", out.results.last().map(|r| &r.tag), out.log);
    assert_eq!(out.results.last().unwrap().tag.as_deref(), Some("ROLLBACK"));
    assert!(out.log.iter().any(|m| m.level == MessageLevel::Warning));
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    assert_eq!(count(&mut s, "dbine_script_t").await, "3");

    // Manual mode: each run joins the transaction the first one opened.
    assert!(d.supports_manual_transactions());
    s.set_autocommit(false).await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    let mut out = QueryOutcome::default();
    assert!(script(d.as_ref(), &mut s, "INSERT INTO dbine_script_t VALUES (5, 'e')", &mut out).await.is_empty());
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Open), "{id}");
    assert!(script(d.as_ref(), &mut s, "UPDATE dbine_script_t SET name = 'z' WHERE id = 1", &mut out).await.is_empty());
    if pg_like {
        assert_eq!(count(&mut other, "dbine_script_t").await, "3", "{id}: not committed yet");
    }
    s.rollback().await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    assert_eq!(count(&mut s, "dbine_script_t").await, "3");
    // count() itself opened a transaction (manual mode): commit it.
    s.commit().await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));

    let mut out = QueryOutcome::default();
    assert!(script(d.as_ref(), &mut s, "INSERT INTO dbine_script_t VALUES (6, 'f')", &mut out).await.is_empty());
    s.commit().await.unwrap();
    assert_eq!(count(&mut other, "dbine_script_t").await, "4", "{id}: committed");
    s.commit().await.unwrap();

    // An error leaves it aborted; Commit then says nothing was committed.
    let mut out = QueryOutcome::default();
    let errors = script(d.as_ref(), &mut s, "INSERT INTO dbine_script_t VALUES (7, 'g'); INSERT INTO dbine_script_t VALUES (1, 'dup')", &mut out).await;
    assert_eq!(errors.len(), 1);
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Failed), "{id}");
    let e = s.commit().await.unwrap_err();
    assert!(matches!(e, Error::State(_)), "{e:?}");
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    assert_eq!(count(&mut other, "dbine_script_t").await, "4");

    if pg_like {
        // What can't run in a transaction block runs outside it.
        s.commit().await.unwrap();
        let mut out = QueryOutcome::default();
        assert!(script(d.as_ref(), &mut s, "VACUUM dbine_script_t", &mut out).await.is_empty(), "{id}");
    }
    // Back to autocommit: the open transaction is committed.
    let mut out = QueryOutcome::default();
    script(d.as_ref(), &mut s, "INSERT INTO dbine_script_t VALUES (8, 'h')", &mut out).await;
    s.set_autocommit(true).await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    assert_eq!(count(&mut other, "dbine_script_t").await, "5");

    // Several statements in one text (other callers): the server is asked.
    run(&mut s, "BEGIN; SELECT 1").await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Open), "{id}");
    run(&mut s, "SELECT 1; COMMIT").await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle), "{id}");

    run(&mut s, "DROP TABLE dbine_script_t").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn postgres() {
    exercise("postgres", "DBINE_TEST_POSTGRES_URL").await;
}

#[tokio::test]
#[ignore]
async fn cockroachdb() {
    exercise("cockroachdb", "DBINE_TEST_COCKROACHDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn timescaledb() {
    exercise("timescaledb", "DBINE_TEST_TIMESCALEDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn yugabytedb() {
    exercise("yugabytedb", "DBINE_TEST_YUGABYTEDB_URL").await;
}

/// H2's PostgreSQL server: transactions tracked by DBine (no probe), and
/// an error doesn't abort them.
#[tokio::test]
#[ignore]
async fn h2() {
    let Ok(url) = std::env::var("DBINE_TEST_H2_URL") else {
        eprintln!("DBINE_TEST_H2_URL not set; skipping");
        return;
    };
    let cfg = parse_url("h2", &url);
    let d = driver("h2");
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let mut other = d.connect(&cfg, None).await.expect("connect");
    let _ = run(&mut s, "DROP TABLE IF EXISTS dbine_script_h2").await;
    let mut out = QueryOutcome::default();
    let errors = script(d.as_ref(), &mut s, "CREATE TABLE dbine_script_h2 (id int PRIMARY KEY); INSERT INTO dbine_script_h2 VALUES (1); SELECT nope FROM dbine_script_h2; INSERT INTO dbine_script_h2 VALUES (2)", &mut out).await;
    assert_eq!(errors.len(), 1, "{errors:?}");
    eprintln!("h2: {:?} / {:?}", errors[0].1, out.results.iter().map(|r| r.tag.clone()).collect::<Vec<_>>());
    assert_eq!(count(&mut s, "dbine_script_h2").await, "2");

    s.set_autocommit(false).await.unwrap();
    let mut out = QueryOutcome::default();
    let errors = script(d.as_ref(), &mut s, "INSERT INTO dbine_script_h2 VALUES (3); INSERT INTO dbine_script_h2 VALUES (1)", &mut out).await;
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Open));
    assert_eq!(count(&mut other, "dbine_script_h2").await, "2", "not committed yet");
    s.rollback().await.unwrap();
    assert_eq!(count(&mut other, "dbine_script_h2").await, "2");
    let mut out = QueryOutcome::default();
    script(d.as_ref(), &mut s, "INSERT INTO dbine_script_h2 VALUES (4)", &mut out).await;
    s.commit().await.unwrap();
    assert_eq!(count(&mut other, "dbine_script_h2").await, "3");
    s.set_autocommit(true).await.unwrap();
    run(&mut s, "DROP TABLE dbine_script_h2").await.unwrap();
}
