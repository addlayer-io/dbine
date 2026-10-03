//! The process list and cancelling another session's statement, against
//! real servers. Each test reads `DBINE_TEST_<ENGINE>_URL`
//! (`postgres://user:pass@host:port/db`, see tests/integration.rs) and is
//! skipped without it:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//! DBINE_TEST_CRATEDB_URL=postgres://crate@localhost:25021/doc \
//! DBINE_TEST_RISINGWAVE_URL=postgres://root@localhost:25023/dev \
//! DBINE_TEST_MATERIALIZE_URL=postgres://materialize@localhost:25024/materialize \
//! DBINE_TEST_H2_URL=postgres://sa:sa@localhost:25025/test \
//!   cargo test -p dbine-driver-postgres --test processes -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
use std::time::Duration;

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
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

async fn run(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map(|_| out)
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> String {
    let o = run(s, sql).await.unwrap();
    match &o.results.last().unwrap().rows[0][0] {
        Value::String(v) => v.clone(),
        v => v.to_string(),
    }
}

/// How each engine names a session and keeps one busy.
struct Engine {
    driver: &'static str,
    env: &'static str,
    /// The session's own id, as `processes` lists it (None: found by the
    /// statement's text).
    id_sql: Option<&'static str>,
    /// A statement that runs for tens of seconds, tagged `dbine_processes_test`.
    busy_sql: &'static str,
    /// Whether the engine shows the running statement (Materialize doesn't).
    shows_sql: bool,
}

const PG: Engine = Engine {
    driver: "postgres",
    env: "",
    id_sql: Some("SELECT pg_backend_pid()::text"),
    busy_sql: "SELECT pg_sleep(30) AS dbine_processes_test",
    shows_sql: true,
};

/// The id `processes` gives the session whose raw id is `raw`: RisingWave
/// prefixes it with the frontend's worker id.
fn find<'a>(list: &'a [dbine_driver::ServerProcess], e: &Engine, raw: &str) -> Option<&'a dbine_driver::ServerProcess> {
    if e.driver == "risingwave" {
        list.iter().find(|p| p.id.rsplit_once(':').map(|(_, n)| n) == Some(raw))
    } else {
        list.iter().find(|p| p.id == raw)
    }
}

/// A session busy in a statement shows up active with its text, the
/// lister's own row is flagged, and cancelling stops the statement but
/// leaves the session usable.
async fn list_and_cancel(e: Engine) {
    let Some(cfg) = cfg(e.driver, e.env) else {
        eprintln!("{} not set; skipping", e.env);
        return;
    };
    let driver = e.driver;
    let d = dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    let mut admin = d.connect(&cfg, None).await.unwrap();
    let own_raw = match e.id_sql {
        Some(sql) => Some(scalar(&mut admin, sql).await),
        None => None,
    };

    let mut worker = d.connect(&cfg, None).await.unwrap();
    let worker_raw = match e.id_sql {
        Some(sql) => Some(scalar(&mut worker, sql).await),
        None => None,
    };
    let busy = e.busy_sql;
    let running = tokio::spawn(async move {
        let r = run(&mut worker, busy).await;
        (r, worker)
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let list = admin.processes().await.unwrap();
    let w = match &worker_raw {
        Some(raw) => find(&list, &e, raw),
        None => list.iter().find(|p| p.sql.as_deref().is_some_and(|q| q.contains("dbine_processes_test") && !q.contains("processes()"))),
    }
    .expect("the worker is listed");
    eprintln!("{driver}: {w:#?}");
    assert!(!w.own && !w.system, "{w:?}");
    if e.shows_sql {
        assert!(w.active, "{w:?}");
        assert!(w.sql.as_deref().unwrap_or("").contains("dbine_processes_test"), "{w:?}");
        assert!(w.elapsed_ms.unwrap_or(0) >= 500, "{w:?}");
        assert_eq!(w.command.as_deref(), Some("SELECT"));
    }
    let own = list.iter().find(|p| p.own).expect("its own session is flagged");
    if let Some(raw) = &own_raw {
        assert_eq!(find(&list, &e, raw).map(|p| &p.id), Some(&own.id));
    }
    let (worker_id, own_id) = (w.id.clone(), own.id.clone());

    admin.cancel_query(&worker_id).await.unwrap();
    let (r, mut worker) = tokio::time::timeout(Duration::from_secs(10), running).await.expect("the statement stopped").unwrap();
    assert!(r.is_err(), "the statement was cancelled");
    assert_eq!(scalar(&mut worker, "SELECT 1").await, "1", "the session is still open");
    assert!(admin.cancel_query("1; DROP TABLE x").await.is_err(), "the id is validated");
    assert!(admin.cancel_query(&own_id).await.is_err(), "not its own session");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn postgres() {
    list_and_cancel(Engine { env: "DBINE_TEST_POSTGRES_URL", ..PG }).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn timescaledb() {
    list_and_cancel(Engine { driver: "timescaledb", env: "DBINE_TEST_TIMESCALEDB_URL", ..PG }).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn yugabytedb() {
    list_and_cancel(Engine { driver: "yugabytedb", env: "DBINE_TEST_YUGABYTEDB_URL", ..PG }).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn opengauss() {
    list_and_cancel(Engine { driver: "opengauss", env: "DBINE_TEST_OPENGAUSS_URL", ..PG }).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cockroachdb() {
    list_and_cancel(Engine { driver: "cockroachdb", env: "DBINE_TEST_COCKROACH_URL", id_sql: Some("SHOW session_id"), ..PG }).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn risingwave() {
    list_and_cancel(Engine { driver: "risingwave", env: "DBINE_TEST_RISINGWAVE_URL", id_sql: Some("SELECT pg_backend_pid()::varchar"), ..PG }).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn h2() {
    list_and_cancel(Engine {
        driver: "h2",
        env: "DBINE_TEST_H2_URL",
        id_sql: Some("SELECT CAST(SESSION_ID() AS VARCHAR)"),
        busy_sql: "SELECT SUM(\"X\") AS dbine_processes_test FROM SYSTEM_RANGE(1, 3000000000)",
        shows_sql: true,
    })
    .await;
}

/// CrateDB has no "my session id" function: the worker is found by its
/// statement.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cratedb() {
    list_and_cancel(Engine {
        driver: "cratedb",
        env: "DBINE_TEST_CRATEDB_URL",
        id_sql: None,
        busy_sql: "SELECT sum(a) AS dbine_processes_test FROM generate_series(1, 2000000000) AS t(a)",
        shows_sql: true,
    })
    .await;
}

/// Materialize lists sessions without their statements.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn materialize() {
    list_and_cancel(Engine {
        driver: "materialize",
        env: "DBINE_TEST_MATERIALIZE_URL",
        id_sql: Some("SELECT pg_backend_pid()::text"),
        busy_sql: "SELECT sum(a) AS dbine_processes_test FROM generate_series(1, 2000000000) AS a",
        shows_sql: false,
    })
    .await;
}
