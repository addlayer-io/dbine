//! "Chequeo de salud" of a ClickHouse database against a real server
//! (`DBINE_TEST_CLICKHOUSE_URL`): a database with problems made on purpose
//! (a partition with hundreds of parts, a detached part, a mutation that
//! fails) shows each one, with its fix. Replication needs Keeper and the
//! TTL hint a 10 GB table: those are covered by the unit tests.
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//!   cargo test -p dbine-driver-clickhouse --test health -- --ignored --nocapture
//! ```

use dbine_driver::health::Severity;
use dbine_driver::{ConnectionConfig, QueryOutcome, Session};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_CLICKHOUSE_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "clickhouse".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn finds_what_was_broken() {
    let Some(cfg) = cfg() else {
        eprintln!("DBINE_TEST_CLICKHOUSE_URL not set; skipping");
        return;
    };
    let d = dbine_driver_clickhouse::drivers().into_iter().find(|d| d.info().id == "clickhouse").unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    let _ = s.drop_database("dbine_health").await;
    s.create_database("dbine_health").await.unwrap();

    // One part per row, with merges stopped: 600 parts in one partition.
    run(&mut s, "CREATE TABLE dbine_health.eventos (id UInt64) ENGINE = MergeTree ORDER BY id").await;
    run(&mut s, "SYSTEM STOP MERGES dbine_health.eventos").await;
    run(
        &mut s,
        "INSERT INTO dbine_health.eventos SELECT number FROM numbers(600)
         SETTINGS max_block_size = 1, max_insert_block_size = 1, min_insert_block_size_rows = 0, min_insert_block_size_bytes = 0",
    )
    .await;
    // A part set aside by hand.
    run(&mut s, "CREATE TABLE dbine_health.ventas (id UInt64) ENGINE = MergeTree ORDER BY id").await;
    run(&mut s, "INSERT INTO dbine_health.ventas VALUES (1)").await;
    run(&mut s, "ALTER TABLE dbine_health.ventas DETACH PART 'all_1_1_0'").await;
    // A mutation that fails on every try.
    run(&mut s, "CREATE TABLE dbine_health.precios (id UInt64, v UInt64) ENGINE = MergeTree ORDER BY id").await;
    run(&mut s, "INSERT INTO dbine_health.precios VALUES (1, 1)").await;
    run(&mut s, "ALTER TABLE dbine_health.precios UPDATE v = throwIf(id > 0, 'dbine falla') WHERE 1").await;
    let mut failing = false;
    for _ in 0..30 {
        let mut out = QueryOutcome::default();
        s.execute(
            "SELECT count() FROM system.mutations WHERE database = 'dbine_health' AND latest_fail_reason != ''",
            10,
            &mut out,
        )
        .await
        .unwrap();
        if out.results.first().and_then(|r| r.rows.first()).and_then(|r| r.first()).map(|v| v.to_string().trim_matches('"') != "0") == Some(true) {
            failing = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    assert!(failing, "the mutation never failed");

    let checks = s.health_checks("dbine_health").await.unwrap();
    for c in &checks {
        eprintln!("{:?} [{}] {} {:?}\n  fix: {:?}", c.severity, c.id, c.title, c.objects, c.fix);
    }
    let get = |id: &str| checks.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("{id}"));
    assert!(get("too_many_parts").severity >= Severity::Warning);
    assert!(get("too_many_parts").objects.iter().any(|o| o.starts_with("eventos (")));
    assert!(get("too_many_parts").fix.as_deref().unwrap().contains("OPTIMIZE TABLE `dbine_health`.`eventos`"));
    assert_eq!(get("detached_parts").severity, Severity::Info);
    assert!(get("detached_parts").objects.iter().any(|o| o.starts_with("ventas · all_1_1_0")));
    assert_eq!(get("stuck_mutations").severity, Severity::Warning);
    assert!(get("stuck_mutations").objects.iter().any(|o| o.starts_with("precios · ") && o.contains("dbine falla")));
    assert!(!checks.iter().any(|c| c.id == "replication"), "no replicated tables");

    // Kill the mutation with the fix: it goes away.
    run(&mut s, get("stuck_mutations").fix.clone().unwrap().as_str()).await;
    let again = s.health_checks("dbine_health").await.unwrap();
    assert_eq!(again.iter().find(|c| c.id == "stuck_mutations").unwrap().severity, Severity::Ok);

    s.drop_database("dbine_health").await.unwrap();
}
