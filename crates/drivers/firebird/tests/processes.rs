//! The process list, cancelling another attachment's statement and ending
//! an attachment, against a real server. Skipped without the URL:
//!
//! ```sh
//! DBINE_TEST_FIREBIRD_URL=firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb \
//!   cargo test -p dbine-driver-firebird --test processes -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
use std::time::Duration;

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_FIREBIRD_URL").ok()?;
    let rest = url.strip_prefix("firebird://")?;
    let (cred, addr) = rest.split_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (hostport, path) = addr.split_once('/')?;
    let (host, port) = hostport.split_once(':')?;
    Some(ConnectionConfig {
        driver: "firebird".into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: path.into(),
        username: Some(user.into()),
        password: Some(pass.into()),
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
        Value::String(v) => v.trim().to_string(),
        v => v.to_string(),
    }
}

/// Long enough to be caught running: a cartesian product of a system table.
const BUSY: &str = "SELECT COUNT(*) AS dbine_processes_test FROM RDB$TYPES a, RDB$TYPES b, RDB$TYPES c, RDB$TYPES d";

/// A busy attachment shows up active with its text, the lister's own row
/// is flagged, cancelling stops the statement but keeps the attachment,
/// and ending the attachment drops it from the list.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn lists_cancels_and_kills() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_FIREBIRD_URL not set; skipping");
        return;
    };
    let d = dbine_driver_firebird::drivers().remove(0);
    let caps = d.capabilities();
    assert!(caps.processes && caps.cancel_query && caps.kill_session);
    let mut admin = d.connect(&cfg, None).await.unwrap();
    let own = scalar(&mut admin, "SELECT CURRENT_CONNECTION FROM RDB$DATABASE").await;

    let mut worker = d.connect(&cfg, None).await.unwrap();
    let worker_id = scalar(&mut worker, "SELECT CURRENT_CONNECTION FROM RDB$DATABASE").await;
    let busy = tokio::spawn(async move {
        let r = run(&mut worker, BUSY).await;
        (r, worker)
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let list = admin.processes().await.unwrap();
    let w = list.iter().find(|p| p.id == worker_id).expect("the worker is listed");
    eprintln!("{w:#?}");
    assert!(w.active, "{w:?}");
    assert!(w.sql.as_deref().unwrap_or("").contains("dbine_processes_test"), "{w:?}");
    assert!(w.elapsed_ms.unwrap_or(0) >= 500, "{w:?}");
    assert_eq!(w.command.as_deref(), Some("SELECT"));
    assert!(!w.own && !w.system);
    assert!(list.iter().find(|p| p.id == own).expect("its own attachment is listed").own);

    admin.cancel_query(&worker_id).await.unwrap();
    let (r, mut worker) = tokio::time::timeout(Duration::from_secs(10), busy).await.expect("the statement stopped").unwrap();
    assert!(r.is_err(), "the statement was cancelled");
    assert_eq!(scalar(&mut worker, "SELECT 1 FROM RDB$DATABASE").await, "1", "the attachment is still open");
    assert!(admin.cancel_query("1; DROP TABLE x").await.is_err(), "the id is validated");
    assert!(admin.cancel_query(&own).await.is_err(), "not its own attachment");
    assert!(admin.kill_session(&own).await.is_err(), "not its own attachment");

    admin.kill_session(&worker_id).await.unwrap();
    let list = admin.processes().await.unwrap();
    assert!(list.iter().all(|p| p.id != worker_id), "the attachment is gone");
    assert!(run(&mut worker, "SELECT 1 FROM RDB$DATABASE").await.is_err(), "the worker was disconnected");
}
