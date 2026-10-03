//! The process list (jobs in flight) and cancelling one, against a real
//! Dremio OSS. Skipped without `DBINE_TEST_DREMIO_URL` (the user is the one
//! tests/integration.rs creates):
//!
//! ```sh
//! DBINE_TEST_DREMIO_URL=http://localhost:25947 \
//!   cargo test -p dbine-driver-dremio --test processes -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};
use serde_json::json;
use std::time::Duration;

const USER: &str = "dbine";
const PASS: &str = "secreto123";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_DREMIO_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "dremio".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(USER.into()),
        password: Some(PASS.into()),
        ..Default::default()
    })
}

/// A fresh Dremio has no users: create the first one (ignored if it exists).
async fn bootstrap(c: &ConnectionConfig) {
    let http = reqwest::Client::new();
    let base = format!("http://{}:{}", c.host, c.port);
    for _ in 0..60 {
        if http.get(format!("{base}/apiv2/server_status")).send().await.map(|r| r.status().is_success()).unwrap_or(false) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let _ = http
        .put(format!("{base}/apiv2/bootstrap/firstuser"))
        .header("Authorization", "_dremionull")
        .json(&json!({"userName": USER, "firstName": "DB", "lastName": "Ine", "email": "dbine@example.com", "createdAt": 1700000000000u64, "password": PASS}))
        .send()
        .await;
}

/// A cross join of system tables runs for a while: it shows up in the list,
/// and cancelling it stops it while the session keeps working.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn dremio() {
    let Some(cfg) = cfg() else {
        eprintln!("DBINE_TEST_DREMIO_URL not set; skipping");
        return;
    };
    bootstrap(&cfg).await;
    let d = dbine_driver_dremio::drivers().remove(0);
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    let mut admin = d.connect(&cfg, None).await.unwrap();
    let mut worker = d.connect(&cfg, None).await.unwrap();
    let running = tokio::spawn(async move {
        let mut out = QueryOutcome::default();
        let r = worker
            .execute(
                "SELECT count(*) AS dbine_processes_test FROM sys.options a, sys.options b, sys.options c
                 WHERE length(a.name) + length(b.name) + length(c.name) > 0",
                100,
                &mut out,
            )
            .await;
        (r, worker)
    });
    tokio::time::sleep(Duration::from_millis(3000)).await;

    let list = admin.processes().await.unwrap();
    let w = list
        .iter()
        .find(|p| p.sql.as_deref().unwrap_or("").contains("dbine_processes_test"))
        .expect("the running job is listed");
    eprintln!("{w:#?}");
    assert!(w.active && !w.own && !w.system, "{w:?}");
    assert_eq!(w.command.as_deref(), Some("SELECT"));
    assert_eq!(w.user.as_deref(), Some(USER));
    assert!(!list.iter().any(|p| p.sql.as_deref().unwrap_or("").contains("dbine-processes-list")), "the list leaves itself out");

    admin.cancel_query(&w.id).await.unwrap();
    let (r, mut worker) = tokio::time::timeout(Duration::from_secs(30), running).await.expect("the job stopped").unwrap();
    assert!(r.is_err(), "the job was cancelled");
    let mut out = QueryOutcome::default();
    worker.execute("SELECT 1", 10, &mut out).await.expect("the session still works");
    assert!(admin.cancel_query("../../catalog").await.is_err(), "the id is validated");
    assert!(admin.cancel_query("1b2c3d4e-0000-0000-0000-000000000000").await.is_err(), "an unknown job");
}
