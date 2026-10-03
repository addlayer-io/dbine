//! The process list (queries in flight), cancelling one and ending its
//! session, against a real Snowflake account (no emulator; same variables as
//! tests/blocking.rs, with a warehouse that is running):
//!
//! ```sh
//! DBINE_TEST_SNOWFLAKE_ACCOUNT=… DBINE_TEST_SNOWFLAKE_USER=… DBINE_TEST_SNOWFLAKE_TOKEN=… \
//! DBINE_TEST_SNOWFLAKE_WAREHOUSE=COMPUTE_WH \
//!   cargo test -p dbine-driver-snowflake --test processes -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, ServerProcess, Session};
use std::time::Duration;

fn config() -> Option<ConnectionConfig> {
    let var = |k: &str| std::env::var(format!("DBINE_TEST_SNOWFLAKE_{k}")).ok();
    let mut options = std::collections::HashMap::from([("account".to_string(), var("ACCOUNT")?), ("token".to_string(), var("TOKEN")?)]);
    options.insert("warehouse".into(), var("WAREHOUSE")?);
    Some(ConnectionConfig { driver: "snowflake".into(), username: var("USER"), options: options.into_iter().collect(), ..Default::default() })
}

async fn run(s: &mut Box<dyn Session>, q: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(q, 100, &mut out).await.map(|_| out)
}

/// Waits until a query with `marker` in its text is listed.
async fn listed(admin: &mut Box<dyn Session>, marker: &str) -> ServerProcess {
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if let Some(p) = admin.processes().await.unwrap().into_iter().find(|p| p.sql.as_deref().unwrap_or("").contains(marker)) {
            return p;
        }
    }
    panic!("{marker} is not listed");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn lists_cancels_and_ends_the_session() {
    let Some(cfg) = config() else {
        eprintln!("sin DBINE_TEST_SNOWFLAKE_*: se omite");
        return;
    };
    let d = dbine_driver_snowflake::drivers().remove(0);
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    let mut admin = d.connect(&cfg, None).await.unwrap();
    run(&mut admin, "SELECT 1").await.unwrap(); // resumes the warehouse

    // Cancel: the statement stops and the session goes on.
    let mut worker = d.connect(&cfg, None).await.unwrap();
    let running = tokio::spawn(async move {
        let r = run(&mut worker, "SELECT SYSTEM$WAIT(60) AS dbine_processes_test").await;
        (r, worker)
    });
    let w = listed(&mut admin, "dbine_processes_test").await;
    eprintln!("{w:#?}");
    assert!(w.active && !w.own, "{w:?}");
    admin.cancel_query(&w.id).await.unwrap();
    let (r, mut worker) = tokio::time::timeout(Duration::from_secs(20), running).await.expect("the query stopped").unwrap();
    assert!(r.is_err(), "the query was cancelled");
    run(&mut worker, "SELECT 1").await.expect("the session still works");
    assert!(admin.cancel_query("x'); DROP").await.is_err(), "the id is validated");

    // Kill: the query's session ends.
    let running = tokio::spawn(async move {
        let r = run(&mut worker, "SELECT SYSTEM$WAIT(60) AS dbine_processes_kill").await;
        (r, worker)
    });
    let w = listed(&mut admin, "dbine_processes_kill").await;
    admin.kill_session(&w.id).await.unwrap();
    let (r, mut worker) = tokio::time::timeout(Duration::from_secs(20), running).await.expect("the query stopped").unwrap();
    assert!(r.is_err(), "the session was ended");
    let _ = run(&mut worker, "SELECT 1").await;
}
