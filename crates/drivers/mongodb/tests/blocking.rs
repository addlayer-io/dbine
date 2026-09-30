//! Blocking chains and ending operations, against real servers.
//!
//! Lock waits need a transaction, so a replica set (one node is enough):
//!
//! ```sh
//! docker run -d --name dbine-test-mongodb-rs -p 25209:27017 mongo:7 mongod --replSet rs0 --bind_ip_all
//! docker exec dbine-test-mongodb-rs mongosh --quiet --eval 'rs.initiate()'
//! DBINE_TEST_MONGODB_RS_URL='mongodb://localhost:25209/?directConnection=true' \
//! DBINE_TEST_MONGODB_URL=mongodb://root:secret@localhost:25201/?authSource=admin \
//!   cargo test -p dbine-driver-mongodb --test blocking -- --ignored
//! ```
//!
//! `DBINE_TEST_MONGODB_URL` is the standalone server of tests/integration.rs.

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use mongodb::bson::doc;
use std::time::Duration;

fn cfg(url: &str, read_only: bool) -> ConnectionConfig {
    let mut c = ConnectionConfig { driver: "mongodb".into(), database: "dbine_lock".into(), read_only, ..Default::default() };
    c.options.insert("connection_string".into(), url.into());
    c
}

async fn open(url: &str, read_only: bool) -> Box<dyn Session> {
    dbine_driver_mongodb::drivers()[0].connect(&cfg(url, read_only), None).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, q: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(q, 100, &mut out).await.map(|_| out)
}

/// A transaction left open holding `{_id: 1}` of dbine_lock.c.
async fn open_transaction(url: &str) -> (mongodb::Client, mongodb::ClientSession) {
    let client = mongodb::Client::with_uri_str(url).await.unwrap();
    let mut session = client.start_session().await.unwrap();
    session.start_transaction().await.unwrap();
    let c = client.database("dbine_lock").collection::<mongodb::bson::Document>("c");
    c.update_one(doc! { "_id": 1 }, doc! { "$set": { "v": 1 } }).session(&mut session).await.unwrap();
    (client, session)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn replica_set_chains_and_kills() {
    let Ok(url) = std::env::var("DBINE_TEST_MONGODB_RS_URL") else { return };
    let mut admin = open(&url, false).await;
    run(&mut admin, "db.c.drop()").await.ok();
    run(&mut admin, "db.c.insertOne({ _id: 1, v: 0 })").await.unwrap();
    assert!(admin.blocking().await.unwrap().is_empty(), "nothing blocked yet");

    // 1. A plain write to the document the transaction changed: it retries
    //    on write conflicts until the transaction ends.
    let (_c, head) = open_transaction(&url).await;
    let mut w = open(&url, false).await;
    let waiting = tokio::spawn(async move { run(&mut w, "db.c.updateOne({ _id: 1 }, { $set: { v: 2 } })").await.is_ok() });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let chain = admin.blocking().await.unwrap();
    let waiter = chain.iter().find(|s| s.blocked_by.is_some()).unwrap_or_else(|| panic!("a waiter in {chain:?}"));
    assert!(waiter.wait.as_deref().unwrap_or("").contains("Conflicto de escritura"), "{waiter:?}");
    assert_eq!(waiter.object.as_deref(), Some("dbine_lock.c"));
    assert!(waiter.sql.as_deref().unwrap_or("").contains("\"v\":2"), "{waiter:?}");
    let head_id = waiter.blocked_by.clone().unwrap();
    assert!(head_id.starts_with("lsid:"), "{head_id}");
    let h = chain.iter().find(|s| s.id == head_id).expect("the head is in the chain");
    assert_eq!(h.blocked_by, None);
    assert_eq!(h.wait.as_deref(), Some("inactiva con transacción abierta"));

    // A read-only connection can look but not kill.
    let mut ro = open(&url, true).await;
    assert!(!ro.blocking().await.unwrap().is_empty());
    assert!(ro.kill_session(&head_id).await.is_err());

    // Killing the transaction's session frees the write.
    admin.kill_session(&head_id).await.unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(10), waiting).await.expect("the write got through").unwrap());
    assert!(admin.blocking().await.unwrap().is_empty());
    drop(head);

    // 2. A real lock wait: createIndex wants the collection exclusively
    //    while the transaction holds it (IX). Kill the waiter with killOp.
    let (_c, mut head) = open_transaction(&url).await;
    let mut w = open(&url, false).await;
    let waiting = tokio::spawn(async move { run(&mut w, "db.c.createIndex({ v: 1 })").await.is_ok() });
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let chain = admin.blocking().await.unwrap();
    let waiter = chain.iter().find(|s| s.blocked_by.is_some()).unwrap_or_else(|| panic!("a waiter in {chain:?}"));
    assert!(waiter.wait.as_deref().unwrap_or("").contains("Esperando un bloqueo X (Collection)"), "{waiter:?}");
    assert!(waiter.waited_ms.unwrap_or(0) >= 500, "{waiter:?}");
    assert!(waiter.blocked_by.as_deref().unwrap().starts_with("lsid:"));
    assert!(waiter.id.parse::<i64>().is_ok(), "an opid: {}", waiter.id);
    admin.kill_session(&waiter.id).await.unwrap();
    let ok = tokio::time::timeout(Duration::from_secs(10), waiting).await.expect("the waiter ended").unwrap();
    assert!(!ok, "the createIndex was killed");
    head.abort_transaction().await.unwrap();
    assert!(admin.blocking().await.unwrap().is_empty());

    assert!(admin.kill_session("1; db.dropDatabase()").await.is_err());
    assert!(admin.kill_session("lsid:not-a-uuid").await.is_err());
    run(&mut admin, "db.c.drop()").await.ok();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn standalone_reports_nothing_and_kills_an_operation() {
    let Ok(url) = std::env::var("DBINE_TEST_MONGODB_URL") else { return };
    let mut admin = open(&url, false).await;
    assert!(admin.blocking().await.unwrap().is_empty());

    // A long operation, found with $currentOp and ended with killOp.
    let mut slow = open(&url, false).await;
    run(&mut slow, "db.c.insertOne({ _id: 1 })").await.ok();
    let running = tokio::spawn(async move { run(&mut slow, "db.c.find({ $where: 'sleep(20000) || true' })").await.is_ok() });
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let client = mongodb::Client::with_uri_str(&url).await.unwrap();
    let r = client
        .database("admin")
        .run_command(doc! { "currentOp": 1, "ns": "dbine_lock.c", "command.filter.$where": { "$exists": true } })
        .await
        .unwrap();
    let op = r.get_array("inprog").unwrap().first().and_then(|o| o.as_document()).expect("the slow find").get("opid").unwrap().clone();
    admin.kill_session(&op.to_string()).await.unwrap();
    let ok = tokio::time::timeout(Duration::from_secs(10), running).await.expect("the find ended").unwrap();
    assert!(!ok, "the find was killed");
    assert!(open(&url, true).await.kill_session("1").await.is_err(), "read-only");
}
