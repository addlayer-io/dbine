//! The process list (executions in flight) and cancelling one, against a
//! real Amazon Athena (no local emulator; ignored by default). Credentials
//! come from the default AWS chain:
//!
//! ```sh
//! DBINE_TEST_ATHENA_OUTPUT=s3://my-bucket/athena-results/ AWS_REGION=us-east-1 \
//!   cargo test -p dbine-driver-athena --test processes -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};
use std::time::Duration;

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

fn cfg() -> Option<ConnectionConfig> {
    let mut c = ConnectionConfig { driver: "athena".into(), ..Default::default() };
    c.options.insert("output_location".into(), env("DBINE_TEST_ATHENA_OUTPUT")?);
    if let Some(r) = env("AWS_REGION") {
        c.options.insert("region".into(), r);
    }
    Some(c)
}

/// A long `sequence` count shows up as running, and cancelling it stops it.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn athena() {
    let Some(cfg) = cfg() else {
        eprintln!("DBINE_TEST_ATHENA_OUTPUT not set; skipping");
        return;
    };
    let d = dbine_driver_athena::drivers().remove(0);
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    let mut admin = d.connect(&cfg, None).await.unwrap();
    let mut worker = d.connect(&cfg, None).await.unwrap();
    let running = tokio::spawn(async move {
        let mut out = QueryOutcome::default();
        let r = worker
            .execute(
                "SELECT count(*) AS dbine_processes_test FROM UNNEST(sequence(1, 10000)) a(x) CROSS JOIN UNNEST(sequence(1, 10000)) b(y) \
                 CROSS JOIN UNNEST(sequence(1, 1000)) c(z) WHERE x + y + z > 0",
                100,
                &mut out,
            )
            .await;
        (r, worker)
    });
    let mut found = None;
    for _ in 0..15 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if let Some(p) = admin.processes().await.unwrap().into_iter().find(|p| p.sql.as_deref().unwrap_or("").contains("dbine_processes_test")) {
            found = Some(p);
            break;
        }
    }
    let w = found.expect("the running execution is listed");
    eprintln!("{w:#?}");
    assert!(w.active && !w.own, "{w:?}");

    admin.cancel_query(&w.id).await.unwrap();
    let (r, _worker) = tokio::time::timeout(Duration::from_secs(60), running).await.expect("the query stopped").unwrap();
    assert!(r.is_err(), "the query was cancelled");
    assert!(admin.cancel_query(&w.id).await.is_err(), "it already ended");
    assert!(admin.cancel_query("../x").await.is_err(), "the id is validated");
}
