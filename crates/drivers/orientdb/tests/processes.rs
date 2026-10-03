//! The process list (connections), interrupting a command and closing a
//! connection, against a real server (see tests/integration.rs, which
//! creates the `dbine_admin` database):
//!
//! ```sh
//! DBINE_TEST_ORIENTDB_URL=root:dbine-test-pass@localhost:22480 \
//!   cargo test -p dbine-driver-orientdb --test processes -- --ignored
//! ```
//!
//! A `SLEEP` ignores interrupts (it only ends when its connection
//! closes), so the interrupt is checked as accepted and the kill as
//! effective.

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use std::time::Duration;

fn cfg(url: &str, read_only: bool) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hp.rsplit_once(':').unwrap();
    ConnectionConfig {
        driver: "orientdb".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        read_only,
        ..Default::default()
    }
}

async fn open(url: &str, read_only: bool) -> Box<dyn Session> {
    let d = dbine_driver_orientdb::drivers().remove(0);
    let caps = d.capabilities();
    assert!(caps.processes && caps.cancel_query && caps.kill_session);
    let c = cfg(url, read_only);
    match d.connect(&c, Some("dbine_admin")).await {
        Ok(s) => s,
        Err(_) => {
            let _ = d.connect(&c, None).await.expect("connect").create_database("dbine_admin").await;
            d.connect(&c, Some("dbine_admin")).await.expect("connect")
        }
    }
}

async fn run(s: &mut Box<dyn Session>, q: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(q, 100, &mut out).await.map(|_| out)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn lists_interrupts_and_kills() {
    let Ok(url) = std::env::var("DBINE_TEST_ORIENTDB_URL") else {
        eprintln!("DBINE_TEST_ORIENTDB_URL not set; skipping");
        return;
    };
    let mut admin = open(&url, false).await;
    let mut worker = open(&url, false).await;
    let started = std::time::Instant::now();
    let sleeping = tokio::spawn(async move { run(&mut worker, "SLEEP 20000").await.is_ok() });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let list = admin.processes().await.unwrap();
    eprintln!("{list:#?}");
    let w = list.iter().find(|p| p.active && p.database.as_deref() == Some("dbine_admin")).expect("the sleeping connection");
    assert!(!w.own && !w.system && w.user.as_deref() == Some("root"), "{w:?}");
    let own = list.iter().find(|p| p.own).expect("the listing's connection");
    assert!(admin.cancel_query(&own.id).await.is_err(), "not its own");

    assert!(open(&url, true).await.cancel_query(&w.id).await.is_err(), "read-only");
    admin.cancel_query(&w.id).await.unwrap();
    admin.kill_session(&w.id).await.unwrap();
    let ok = tokio::time::timeout(Duration::from_secs(10), sleeping).await.expect("the sleep ended").unwrap();
    assert!(!ok && started.elapsed() < Duration::from_secs(15), "closed before its 20 s");
    assert!(admin.cancel_query(&w.id).await.is_err(), "the connection is gone");
    assert!(admin.kill_session("1/../x").await.is_err(), "the id is validated");
    let idle = admin.processes().await.unwrap().into_iter().find(|p| !p.active && !p.own);
    if let Some(idle) = idle {
        assert!(admin.cancel_query(&idle.id).await.is_err(), "nothing to cancel");
    }
}
