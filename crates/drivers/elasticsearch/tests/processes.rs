//! The process list (running tasks) and cancelling another session's
//! request, against real servers (see tests/integration.rs):
//!
//! ```sh
//! DBINE_TEST_ELASTICSEARCH_URL=http://localhost:25520 DBINE_TEST_OPENSEARCH_URL=http://localhost:25521 \
//!   cargo test -p dbine-driver-elasticsearch --test processes -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, Error, QueryOutcome, Session};
use std::time::Duration;

async fn session(id: &str, url: &str, read_only: bool) -> Box<dyn Session> {
    let d = dbine_driver_elasticsearch::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    assert!(d.capabilities().processes && d.capabilities().cancel_query);
    let cfg = ConnectionConfig { driver: id.into(), host: url.into(), read_only, ..Default::default() };
    d.connect(&cfg, None).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> Result<QueryOutcome, Error> {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.map(|_| out)
}

/// A throttled reindex (one document per second) shows up as a running
/// task with its description, the listing is DBine's own, and cancelling
/// ends the reindex early.
async fn list_and_cancel(id: &str, env: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let mut admin = session(id, &url, false).await;
    let mut worker = session(id, &url, false).await;
    run(&mut admin, "DELETE /dbine_proc_dst").await.ok();
    let mut seed = String::from("POST /dbine_proc_src/_bulk?refresh=true\n");
    for i in 0..30 {
        seed.push_str(&format!("{{\"index\":{{\"_id\":\"{i}\"}}}}\n{{\"n\":{i}}}\n"));
    }
    run(&mut admin, &seed).await.unwrap();
    let reindex = "POST /_reindex?requests_per_second=1\n{\"source\":{\"index\":\"dbine_proc_src\",\"size\":1},\"dest\":{\"index\":\"dbine_proc_dst\"}}";
    let started = std::time::Instant::now();
    let slow = tokio::spawn(async move {
        let r = run(&mut worker, reindex).await;
        (r, worker)
    });
    tokio::time::sleep(Duration::from_millis(2000)).await;

    let list = admin.processes().await.unwrap();
    let w = list
        .iter()
        .find(|p| p.command.as_deref() == Some("indices:data/write/reindex"))
        .unwrap_or_else(|| panic!("the reindex in {list:#?}"));
    eprintln!("{id}: {w:#?}");
    assert!(w.active && !w.own && !w.system, "{w:?}");
    assert!(w.sql.as_deref().unwrap_or("").contains("dbine_proc_src"), "{w:?}");
    assert!(w.elapsed_ms.unwrap_or(0) >= 1000, "{w:?}");
    assert!(list.iter().any(|p| p.own), "the listing is DBine's own");
    assert!(!list.iter().any(|p| p.command.as_deref() == Some("cluster:monitor/tasks/lists[n]")), "children folded");

    let mut ro = session(id, &url, true).await;
    assert!(ro.cancel_query(&w.id).await.is_err(), "read-only");

    admin.cancel_query(&w.id).await.unwrap();
    let (_, mut worker) = tokio::time::timeout(Duration::from_secs(10), slow).await.expect("the reindex stopped").unwrap();
    assert!(started.elapsed() < Duration::from_secs(20), "cancelled before its 30 s");
    assert!(run(&mut worker, "GET /_cluster/health").await.is_ok(), "the session still works");
    assert!(admin.cancel_query(&w.id).await.is_err(), "it already ended");
    assert!(admin.cancel_query("x/../_cluster:1").await.is_err(), "the id is validated");
    let own = admin.processes().await.unwrap().into_iter().find(|p| p.own).unwrap();
    // The listing is over by now: either it ended or it's refused as DBine's own.
    assert!(admin.cancel_query(&own.id).await.is_err());
    run(&mut admin, "DELETE /dbine_proc_src").await.ok();
    run(&mut admin, "DELETE /dbine_proc_dst").await.ok();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn elasticsearch() {
    list_and_cancel("elasticsearch", "DBINE_TEST_ELASTICSEARCH_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn opensearch() {
    list_and_cancel("opensearch", "DBINE_TEST_OPENSEARCH_URL").await;
}
