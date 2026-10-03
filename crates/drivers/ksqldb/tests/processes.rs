//! The query list and `TERMINATE`, against a real ksqlDB
//! (`DBINE_TEST_KSQLDB_URL`, see tests/integration.rs for the containers);
//! skipped without it:
//!
//! ```sh
//! DBINE_TEST_KSQLDB_URL=http://localhost:25188 \
//!   cargo test -p dbine-driver-ksqldb --test processes -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use std::time::{Duration, Instant};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_KSQLDB_URL").ok()?).expect("URL");
    let mut c = ConnectionConfig { driver: "ksqldb".into(), host: url.host_str()?.into(), port: url.port().unwrap_or(0), ..Default::default() };
    c.options.insert("push_timeout".into(), "30".into());
    Some(c)
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{sql}"));
    assert!(out.error.is_none(), "{:?}\n{sql}", out.error);
}

/// A persistent query and an open push query are listed running, and
/// terminating them stops both.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn ksqldb() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_KSQLDB_URL not set; skipping");
        return;
    };
    let d = dbine_driver_ksqldb::drivers().remove(0);
    assert!(d.capabilities().processes && d.capabilities().cancel_query && !d.capabilities().kill_session);
    let mut admin = d.connect(&c, None).await.unwrap();
    let _ = admin.execute("DROP STREAM IF EXISTS DBINE_PROC_X DELETE TOPIC; DROP STREAM IF EXISTS DBINE_PROC_S DELETE TOPIC;", 10, &mut QueryOutcome::default()).await;
    exec(&mut admin, "CREATE STREAM DBINE_PROC_S (ID INT KEY, V VARCHAR) WITH (KAFKA_TOPIC='dbine_proc_s', VALUE_FORMAT='JSON', PARTITIONS=1);").await;
    exec(&mut admin, "CREATE STREAM DBINE_PROC_X AS SELECT * FROM DBINE_PROC_S EMIT CHANGES;").await;

    let mut worker = d.connect(&c, None).await.unwrap();
    let push = tokio::spawn(async move {
        let t = Instant::now();
        let mut o = QueryOutcome::default();
        let _ = worker.execute("SELECT * FROM DBINE_PROC_S EMIT CHANGES;", 10, &mut o).await;
        t.elapsed()
    });
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let list = admin.processes().await.unwrap();
    eprintln!("{list:#?}");
    let persistent = list.iter().find(|p| p.database.as_deref() == Some("DBINE_PROC_X")).expect("the persistent query");
    assert!(persistent.active);
    assert_eq!(persistent.command.as_deref(), Some("PERSISTENT"));
    let transient = list
        .iter()
        .find(|p| p.command.as_deref() == Some("PUSH") && p.sql.as_deref().unwrap_or("").contains("DBINE_PROC_S"))
        .expect("the push query");
    assert!(transient.active && !transient.own);

    admin.cancel_query(&transient.id).await.unwrap();
    let took = tokio::time::timeout(Duration::from_secs(10), push).await.expect("the push query ended").unwrap();
    assert!(took < Duration::from_secs(20), "ended before its timeout: {took:?}");
    admin.cancel_query(&persistent.id).await.unwrap();
    assert!(!admin.processes().await.unwrap().iter().any(|p| p.id == persistent.id), "terminated");
    assert!(admin.cancel_query(&persistent.id).await.is_err(), "nothing left to terminate");
    assert!(admin.cancel_query("X; DROP STREAM DBINE_PROC_S").await.is_err(), "the id is validated");
    exec(&mut admin, "DROP STREAM IF EXISTS DBINE_PROC_X DELETE TOPIC;").await;
    exec(&mut admin, "DROP STREAM IF EXISTS DBINE_PROC_S DELETE TOPIC;").await;
}
