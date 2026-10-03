//! The process list (running queries) and cancelling one, against a real
//! server. Skipped without `DBINE_TEST_CLICKHOUSE_URL`:
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//!   cargo test -p dbine-driver-clickhouse --test processes -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};
use std::time::Duration;

fn cfg(env: &str, id: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(env).ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: id.into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
}

/// A long query shows up running with its text, and cancelling it stops
/// it while the session keeps working.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn clickhouse() {
    let Some(cfg) = cfg("DBINE_TEST_CLICKHOUSE_URL", "clickhouse") else {
        eprintln!("DBINE_TEST_CLICKHOUSE_URL not set; skipping");
        return;
    };
    let d = dbine_driver_clickhouse::drivers().into_iter().find(|d| d.info().id == "clickhouse").unwrap();
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    let mut admin = d.connect(&cfg, None).await.unwrap();
    let mut worker = d.connect(&cfg, None).await.unwrap();
    let running = tokio::spawn(async move {
        let mut out = QueryOutcome::default();
        // `sleep` caps at 3 s; a long scan runs for as long as needed.
        let r = worker
            .execute(
                "SELECT count() AS dbine_processes_test FROM numbers(100000000000) WHERE NOT ignore(sipHash64(number))",
                100,
                &mut out,
            )
            .await;
        (r, worker)
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let list = admin.processes().await.unwrap();
    let w = list
        .iter()
        .find(|p| p.sql.as_deref().unwrap_or("").contains("dbine_processes_test"))
        .expect("the running query is listed");
    eprintln!("{w:#?}");
    assert!(w.active && !w.own && !w.system, "{w:?}");
    assert!(w.elapsed_ms.unwrap_or(0) >= 500, "{w:?}");
    assert_eq!(w.command.as_deref(), Some("SELECT"));
    assert!(!list.iter().any(|p| p.sql.as_deref().unwrap_or("").contains("system.processes")), "the list leaves itself out");

    admin.cancel_query(&w.id).await.unwrap();
    let (r, mut worker) = tokio::time::timeout(Duration::from_secs(15), running).await.expect("the query stopped").unwrap();
    assert!(r.is_err(), "the query was cancelled");
    let mut out = QueryOutcome::default();
    worker.execute("SELECT 1", 10, &mut out).await.expect("the session still works");
    assert!(admin.cancel_query("x' OR 1=1 --").await.is_err(), "the id is validated");
    assert!(admin.cancel_query("00000000-0000-0000-0000-000000000000").await.is_err(), "an unknown query");
}
