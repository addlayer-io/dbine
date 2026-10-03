//! The process list and cancelling another operation, against real
//! servers (see tests/integration.rs for the containers):
//!
//! ```sh
//! DBINE_TEST_MONGODB_URL=mongodb://root:secret@localhost:25201/?authSource=admin \
//! DBINE_TEST_FERRETDB_URL=mongodb://root:secret@localhost:25203/ \
//!   cargo test -p dbine-driver-mongodb --test processes -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, Error, QueryOutcome, Session};
use std::time::Duration;

fn cfg(driver: &str, url: &str, read_only: bool) -> ConnectionConfig {
    let mut c = ConnectionConfig { driver: driver.into(), database: "dbine_proc".into(), read_only, ..Default::default() };
    c.options.insert("connection_string".into(), url.into());
    c
}

async fn open(driver: &str, url: &str, read_only: bool) -> Box<dyn Session> {
    let d = dbine_driver_mongodb::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    d.connect(&cfg(driver, url, read_only), None).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, q: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(q, 100, &mut out).await.map(|_| out)
}

/// A slow find shows up running with its command, the listing itself is
/// flagged as DBine's own, and cancelling stops the find but leaves the
/// session usable.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mongodb_lists_and_cancels() {
    let Ok(url) = std::env::var("DBINE_TEST_MONGODB_URL") else {
        eprintln!("DBINE_TEST_MONGODB_URL not set; skipping");
        return;
    };
    let d = &dbine_driver_mongodb::drivers()[0];
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    let mut admin = open("mongodb", &url, false).await;
    let mut worker = open("mongodb", &url, false).await;
    run(&mut worker, "db.c.insertOne({ _id: 1 })").await.ok();
    let sleeping = tokio::spawn(async move {
        let r = run(&mut worker, "db.c.find({ $where: 'sleep(30000) || \"dbine_processes_test\"' })").await;
        (r, worker)
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let list = admin.processes().await.unwrap();
    let w = list
        .iter()
        .find(|p| p.sql.as_deref().is_some_and(|s| s.contains("dbine_processes_test")))
        .unwrap_or_else(|| panic!("the worker in {list:#?}"));
    eprintln!("{w:#?}");
    assert!(w.active && !w.own && !w.system, "{w:?}");
    assert_eq!(w.command.as_deref(), Some("find"));
    assert_eq!(w.database.as_deref(), Some("dbine_proc"));
    assert!(w.elapsed_ms.unwrap_or(0) >= 500, "{w:?}");
    assert!(w.id.parse::<i64>().is_ok(), "an opid: {}", w.id);
    let own = list.iter().find(|p| p.own).expect("the listing is DBine's own");
    assert!(own.active);
    assert!(list.iter().any(|p| p.system), "server threads");
    assert!(list.iter().any(|p| !p.active && p.id.starts_with("conn")), "idle connections");

    let mut ro = open("mongodb", &url, true).await;
    assert!(!ro.processes().await.unwrap().is_empty());
    assert!(ro.cancel_query(&w.id).await.is_err(), "read-only");

    admin.cancel_query(&w.id).await.unwrap();
    let (r, mut worker) = tokio::time::timeout(Duration::from_secs(10), sleeping).await.expect("the find stopped").unwrap();
    assert!(r.is_err(), "the find was cancelled");
    assert!(run(&mut worker, "db.c.countDocuments({})").await.is_ok(), "the session is still usable");
    assert!(admin.cancel_query(&w.id).await.is_err(), "it already ended");
    assert!(admin.cancel_query("1; db.dropDatabase()").await.is_err(), "the id is validated");
    let idle = list.iter().find(|p| p.id.starts_with("conn")).unwrap();
    assert!(admin.cancel_query(&idle.id).await.is_err(), "an idle connection");
    run(&mut admin, "db.c.drop()").await.ok();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn ferretdb_lists_without_cancel() {
    let Ok(url) = std::env::var("DBINE_TEST_FERRETDB_URL") else {
        eprintln!("DBINE_TEST_FERRETDB_URL not set; skipping");
        return;
    };
    let d = &dbine_driver_mongodb::drivers()[1];
    assert!(d.capabilities().processes && !d.capabilities().cancel_query);
    let mut s = open("ferretdb", &url, false).await;
    let list = s.processes().await.unwrap();
    eprintln!("{list:#?}");
    assert!(list.iter().any(|p| p.active), "the listing runs");
    assert!(matches!(s.cancel_query("1").await, Err(Error::Unsupported(_))));
}
