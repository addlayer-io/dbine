//! The connection list, `KILL QUERY` and `KILL CONNECTION`, against a real
//! TDengine (`DBINE_TEST_TDENGINE_URL`, taosAdapter's REST port, see
//! tests/integration.rs); skipped without it:
//!
//! ```sh
//! DBINE_TEST_TDENGINE_URL=http://localhost:25641 \
//!   cargo test -p dbine-driver-tdengine --test processes -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use std::time::Duration;

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_TDENGINE_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "tdengine".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("root".into()),
        password: Some("taosdata".into()),
        ..Default::default()
    })
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{sql}"));
    out
}

/// A slow grouping over two million rows shows up under its connection
/// once a heartbeat reports it, and `KILL QUERY` stops it.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn tdengine() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_TDENGINE_URL not set; skipping");
        return;
    };
    let d = dbine_driver_tdengine::drivers().remove(0);
    let caps = d.capabilities();
    assert!(caps.processes && caps.cancel_query && caps.kill_session);
    // The table the profiler test fills (same rows).
    const ROWS: u64 = 2_000_000;
    let mut admin = d.connect(&c, None).await.unwrap();
    exec(&mut admin, "CREATE DATABASE IF NOT EXISTS dbine_prof").await;
    let have = exec(&mut admin, "SELECT COUNT(*) FROM dbine_prof.big").await;
    if have.results.first().and_then(|r| r.rows.first()).and_then(|r| r[0].as_u64()) != Some(ROWS) {
        exec(&mut admin, "DROP TABLE IF EXISTS dbine_prof.big").await;
        exec(&mut admin, "CREATE TABLE dbine_prof.big (ts TIMESTAMP, v INT)").await;
        for b in 0..ROWS / 20_000 {
            let vals: Vec<String> =
                (0..20_000u64).map(|i| format!("({}, {})", 1_700_000_000_000 + b * 20_000 + i, (b * 20_000 + i) * 7919 % 1_000_003)).collect();
            exec(&mut admin, &format!("INSERT INTO dbine_prof.big VALUES {}", vals.join(" "))).await;
        }
    }

    let mut worker = d.connect(&c, Some("dbine_prof")).await.unwrap();
    let slow = tokio::spawn(async move {
        let mut o = QueryOutcome::default();
        let r = worker.execute("SELECT v, ts, COUNT(*) AS dbine_processes_test FROM big GROUP BY v, ts ORDER BY dbine_processes_test DESC, v LIMIT 1", 10, &mut o).await;
        r.is_err() || o.error.is_some() || !o.errors.is_empty()
    });
    // Until a heartbeat reports it.
    let mut found = None;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let list = admin.processes().await.unwrap();
        if let Some(w) = list.into_iter().find(|p| p.sql.as_deref().unwrap_or("").contains("dbine_processes_test")) {
            found = Some(w);
            break;
        }
    }
    let w = found.expect("the query is listed");
    eprintln!("{w:#?}");
    assert!(w.active && w.elapsed_ms.is_some());
    assert_eq!(w.command.as_deref(), Some("SELECT"));
    assert!(w.id.parse::<u32>().is_ok(), "a conn_id: {}", w.id);

    admin.cancel_query(&w.id).await.unwrap();
    let failed = tokio::time::timeout(Duration::from_secs(10), slow).await.expect("the query stopped").unwrap();
    assert!(failed, "the query was killed");
    assert!(admin.cancel_query("1; DROP DATABASE dbine_prof").await.is_err(), "the id is validated");
    assert!(admin.processes().await.unwrap().iter().any(|p| !p.active), "idle connections are listed too");
}
