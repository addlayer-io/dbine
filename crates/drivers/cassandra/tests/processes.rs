//! The process list against real servers (see tests/integration.rs):
//!
//! ```sh
//! DBINE_TEST_CASSANDRA_URL=localhost:25402 DBINE_TEST_SCYLLADB_URL=localhost:25413 \
//!   cargo test -p dbine-driver-cassandra --test processes -- --ignored
//! ```
//!
//! CQL can't make a request run for long on demand, so the running list is
//! checked through the listing itself (Cassandra 4.1+).

use dbine_driver::{ConnectionConfig, Error, Session};

async fn open(driver: &str, url: &str) -> Box<dyn Session> {
    let d = dbine_driver_cassandra::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(d.capabilities().processes && !d.capabilities().cancel_query);
    let cfg = ConnectionConfig { driver: driver.into(), host: url.into(), ..Default::default() };
    d.connect(&cfg, None).await.expect("connect")
}

/// Both sessions' connections are listed, only the lister's as its own.
async fn lists_connections(driver: &str, env: &str, queries: bool) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let mut admin = open(driver, &url).await;
    let _other = open(driver, &url).await;
    let list = admin.processes().await.unwrap();
    eprintln!("{driver}: {list:#?}");
    let conns: Vec<_> = list.iter().filter(|p| !p.active).collect();
    assert!(conns.iter().any(|p| p.own), "the lister's connection");
    assert!(conns.iter().any(|p| !p.own && p.program.as_deref() == Some("DBine")), "the other session's");
    assert!(conns.iter().all(|p| p.id.rsplit_once(':').is_some_and(|(_, port)| port.parse::<u16>().is_ok())));
    if queries {
        let q = list.iter().find(|p| p.active).expect("the listing is running");
        assert!(q.own && q.sql.as_deref().unwrap_or("").contains("system_views.queries"), "{q:?}");
    }
    assert!(matches!(admin.cancel_query(&conns[0].id).await, Err(Error::Unsupported(_))));
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cassandra() {
    lists_connections("cassandra", "DBINE_TEST_CASSANDRA_URL", true).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn scylladb() {
    lists_connections("scylladb", "DBINE_TEST_SCYLLADB_URL", false).await;
}
