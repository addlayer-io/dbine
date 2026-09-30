//! Blocking chains and aborting a transaction, against a real Snowflake
//! account (no emulator; same variables as tests/integration.rs, a role
//! that can create a database and a warehouse to run the updates):
//!
//! ```sh
//! DBINE_TEST_SNOWFLAKE_ACCOUNT=… DBINE_TEST_SNOWFLAKE_USER=… DBINE_TEST_SNOWFLAKE_TOKEN=… \
//! DBINE_TEST_SNOWFLAKE_WAREHOUSE=COMPUTE_WH \
//!   cargo test -p dbine-driver-snowflake --test blocking -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use std::time::Duration;

const DB: &str = "DBINE_LOCK_TEST";

fn config() -> Option<ConnectionConfig> {
    let var = |k: &str| std::env::var(format!("DBINE_TEST_SNOWFLAKE_{k}")).ok();
    let mut options = std::collections::HashMap::from([("account".to_string(), var("ACCOUNT")?), ("token".to_string(), var("TOKEN")?)]);
    if let Some(w) = var("WAREHOUSE") {
        options.insert("warehouse".into(), w);
    }
    Some(ConnectionConfig {
        driver: "snowflake".into(),
        username: var("USER"),
        database: DB.into(),
        options: options.into_iter().collect(),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, q: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(q, 100, &mut out).await.map(|_| out)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn reports_the_chain_and_aborts_the_head() {
    let Some(cfg) = config() else { return };
    let d = dbine_driver_snowflake::drivers().remove(0);
    assert!(d.capabilities().blocking && d.capabilities().kill_session);
    let boot = ConnectionConfig { database: String::new(), ..cfg.clone() };
    let mut admin = d.connect(&boot, None).await.expect("connect");
    run(&mut admin, &format!("CREATE DATABASE IF NOT EXISTS {DB}")).await.unwrap();
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    run(&mut admin, "CREATE OR REPLACE TABLE PUBLIC.LOCKED (ID INT, V INT); INSERT INTO PUBLIC.LOCKED VALUES (1, 0)").await.unwrap();

    // The head: a transaction that updates the table and then waits.
    let mut head = d.connect(&cfg, None).await.unwrap();
    let holding = tokio::spawn(async move {
        run(&mut head, "BEGIN; UPDATE PUBLIC.LOCKED SET V = 1 WHERE ID = 1; CALL SYSTEM$WAIT(60); COMMIT;").await.is_ok()
    });
    tokio::time::sleep(Duration::from_secs(8)).await;

    // The waiter: another update of the same table.
    let mut waiter = d.connect(&cfg, None).await.unwrap();
    let waiting = tokio::spawn(async move { run(&mut waiter, "UPDATE PUBLIC.LOCKED SET V = 2 WHERE ID = 1").await.is_ok() });
    tokio::time::sleep(Duration::from_secs(8)).await;

    let chain = admin.blocking().await.unwrap();
    println!("{chain:#?}");
    let w = chain.iter().find(|s| s.blocked_by.is_some()).unwrap_or_else(|| panic!("a waiter in {chain:?}"));
    assert!(w.object.as_deref().unwrap_or("").ends_with("LOCKED"), "{w:?}");
    let head_id = w.blocked_by.clone().unwrap();
    let h = chain.iter().find(|s| s.id == head_id).expect("the head is in the chain");
    assert_eq!(h.blocked_by, None);

    // Aborting the head frees the waiter.
    admin.kill_session(&head_id).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(60), waiting).await.expect("the waiter got through").unwrap());
    let _ = holding.await;
    assert!(admin.blocking().await.unwrap().is_empty());
    assert!(admin.kill_session("1); DROP TABLE x --").await.is_err(), "the id is a number");
    run(&mut admin, &format!("DROP DATABASE IF EXISTS {DB}")).await.unwrap();
}
