//! The process list (the warehouse's queries in flight) and cancelling one,
//! against a real workspace. Skipped without the variables (as in
//! tests/transfer.rs):
//!
//! ```sh
//! DBINE_TEST_DATABRICKS_HOST=dbc-….cloud.databricks.com \
//! DBINE_TEST_DATABRICKS_WAREHOUSE=abcdef1234567890 \
//! DBINE_TEST_DATABRICKS_TOKEN=dapi… DBINE_TEST_DATABRICKS_CATALOG=main \
//!   cargo test -p dbine-driver-databricks --test processes -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};
use std::time::Duration;

fn env(k: &str) -> Option<String> {
    std::env::var(format!("DBINE_TEST_DATABRICKS_{k}")).ok().filter(|v| !v.is_empty())
}

fn cfg() -> Option<ConnectionConfig> {
    let mut c = ConnectionConfig { driver: "databricks".into(), host: env("HOST")?, database: env("CATALOG")?, ..Default::default() };
    c.options.insert("warehouse".into(), env("WAREHOUSE")?);
    c.options.insert("token".into(), env("TOKEN")?);
    c.options.insert("schema".into(), env("SCHEMA").unwrap_or_else(|| "default".into()));
    Some(c)
}

/// A long `range` count shows up in the history as running, and
/// cancelling it stops it while the session keeps working.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn databricks() {
    let Some(cfg) = cfg() else {
        eprintln!("sin DBINE_TEST_DATABRICKS_*: se omite");
        return;
    };
    let d = dbine_driver_databricks::drivers().remove(0);
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    let mut admin = d.connect(&cfg, None).await.unwrap();
    let mut worker = d.connect(&cfg, None).await.unwrap();
    let running = tokio::spawn(async move {
        let mut out = QueryOutcome::default();
        let r = worker
            .execute("SELECT count(*) AS dbine_processes_test FROM range(1000000000000) WHERE hash(id) = 42", 100, &mut out)
            .await;
        (r, worker)
    });
    // The history shows a query a few seconds after it starts.
    let mut found = None;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let list = admin.processes().await.unwrap();
        if let Some(p) = list.into_iter().find(|p| p.sql.as_deref().unwrap_or("").contains("dbine_processes_test")) {
            found = Some(p);
            break;
        }
    }
    let w = found.expect("the running query is listed");
    eprintln!("{w:#?}");
    assert!(w.active && !w.own && !w.system, "{w:?}");

    admin.cancel_query(&w.id).await.unwrap();
    let (r, mut worker) = tokio::time::timeout(Duration::from_secs(60), running).await.expect("the query stopped").unwrap();
    assert!(r.is_err(), "the query was cancelled");
    let mut out = QueryOutcome::default();
    worker.execute("SELECT 1", 10, &mut out).await.expect("the session still works");
    assert!(admin.cancel_query("../../jobs").await.is_err(), "the id is validated");
}
