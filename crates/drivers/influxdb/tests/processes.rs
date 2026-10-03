//! The running-query list and `KILL QUERY`, against real servers (see
//! tests/integration.rs for the containers). Each test is skipped without
//! its variable:
//!
//! ```sh
//! DBINE_TEST_INFLUXDB1_URL=http://localhost:25404 DBINE_TEST_INFLUXDB3_URL=http://localhost:25409 \
//!   cargo test -p dbine-driver-influxdb --test processes -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, Error, QueryOutcome, Session};
use std::time::Duration;

async fn open(driver: &str, url: &str, db: Option<&str>) -> Box<dyn Session> {
    let d = dbine_driver_influxdb::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    let cfg = ConnectionConfig { driver: driver.into(), host: url.into(), ..Default::default() };
    d.connect(&cfg, db).await.unwrap()
}

/// A slow query (millions of filled buckets over a few thousand points)
/// shows up running with its text and database, and `KILL QUERY` stops it.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn influxdb1() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB1_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB1_URL not set; skipping");
        return;
    };
    let d = dbine_driver_influxdb::drivers().into_iter().find(|d| d.info().id == "influxdb1").unwrap();
    assert!(d.capabilities().processes && d.capabilities().cancel_query && !d.capabilities().kill_session);
    let http = reqwest::Client::new();
    http.post(format!("{url}/query")).form(&[("q", "CREATE DATABASE dbine_processes")]).send().await.unwrap();
    let lines: Vec<String> = (1..=5000).map(|i| format!("m,t=a v={i} {i}000000000")).collect();
    let r = http.post(format!("{url}/write?db=dbine_processes")).body(lines.join("\n")).send().await.unwrap();
    assert!(r.status().is_success());

    let mut admin = open("influxdb1", &url, None).await;
    let mut worker = open("influxdb1", &url, Some("dbine_processes")).await;
    let slow = tokio::spawn(async move {
        let mut o = QueryOutcome::default();
        let r = worker.execute("SELECT count(v) FROM m WHERE time >= 0 AND time < 3000s GROUP BY time(1ms) fill(0)", 10, &mut o).await;
        r.is_err() || o.error.is_some() || !o.errors.is_empty()
    });
    tokio::time::sleep(Duration::from_millis(700)).await;

    let list = admin.processes().await.unwrap();
    let w = list.iter().find(|p| p.sql.as_deref().unwrap_or("").contains("GROUP BY time(1ms)")).unwrap_or_else(|| panic!("the query in {list:#?}"));
    eprintln!("{w:#?}");
    assert!(w.active && !w.own);
    assert_eq!((w.database.as_deref(), w.command.as_deref()), (Some("dbine_processes"), Some("SELECT")));
    assert!(list.iter().any(|p| p.own), "its own SHOW QUERIES is flagged");

    admin.cancel_query(&w.id).await.unwrap();
    let failed = tokio::time::timeout(Duration::from_secs(20), slow).await.expect("the query stopped").unwrap();
    assert!(failed, "the query was killed");
    assert!(admin.cancel_query(&w.id).await.is_err(), "nothing left to kill");
    assert!(admin.cancel_query("1; DROP DATABASE x").await.is_err(), "the id is validated");
    http.post(format!("{url}/query")).form(&[("q", "DROP DATABASE dbine_processes")]).send().await.unwrap();
}

/// The list reads `system.queries` (its own query is there, running), and
/// cancelling isn't offered.
#[tokio::test]
#[ignore]
async fn influxdb3() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB3_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB3_URL not set; skipping");
        return;
    };
    let d = dbine_driver_influxdb::drivers().into_iter().find(|d| d.info().id == "influxdb3").unwrap();
    assert!(d.capabilities().processes && !d.capabilities().cancel_query);
    let http = reqwest::Client::new();
    let r = http.post(format!("{url}/api/v3/write_lp?db=dbine_processes")).body("m,t=a v=1").send().await.unwrap();
    assert!(r.status().is_success());
    let mut s = open("influxdb3", &url, None).await;
    let list = s.processes().await.unwrap();
    eprintln!("{list:#?}");
    assert!(list.iter().any(|p| p.own && p.active), "its own query is listed");
    assert!(matches!(s.cancel_query("x").await, Err(Error::Unsupported(_))));
}
