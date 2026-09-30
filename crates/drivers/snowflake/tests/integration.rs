//! Against a real Snowflake account (there's no emulator). Run with:
//!
//! ```sh
//! DBINE_TEST_SNOWFLAKE_ACCOUNT=miorg-micuenta DBINE_TEST_SNOWFLAKE_USER=JDOE DBINE_TEST_SNOWFLAKE_TOKEN=<PAT> \
//! DBINE_TEST_SNOWFLAKE_WAREHOUSE=COMPUTE_WH DBINE_TEST_SNOWFLAKE_DATABASE=SNOWFLAKE_SAMPLE_DATA \
//!   cargo test -p dbine-driver-snowflake --test integration -- --ignored --nocapture
//! ```

use dbine_driver::ConnectionConfig;

fn config() -> ConnectionConfig {
    let var = |k: &str| std::env::var(format!("DBINE_TEST_SNOWFLAKE_{k}")).ok();
    let mut options: std::collections::HashMap<String, String> = [
        ("account".to_string(), var("ACCOUNT").expect("DBINE_TEST_SNOWFLAKE_ACCOUNT")),
        ("token".to_string(), var("TOKEN").expect("DBINE_TEST_SNOWFLAKE_TOKEN")),
    ]
    .into();
    if let Some(w) = var("WAREHOUSE") {
        options.insert("warehouse".into(), w);
    }
    ConnectionConfig {
        driver: "snowflake".into(),
        username: var("USER"),
        database: var("DATABASE").unwrap_or_default(),
        options: options.into_iter().collect(),
        ..Default::default()
    }
}

#[tokio::test]
#[ignore]
async fn monitor() {
    let driver = dbine_driver_snowflake::drivers().remove(0);
    assert!(driver.capabilities().monitor);
    let mut s = driver.connect(&config(), None).await.expect("connect");
    let snap = s.monitor().await.expect("monitor");
    for m in &snap.metrics {
        println!("{:<20} {:?} max {:?}", m.key, m.value, m.max);
    }
    for t in &snap.tables {
        println!("tabla {} ({} filas)", t.key, t.rows.len());
    }
    println!("info: {:?}\nnotas: {:?}", snap.info, snap.notes);
    assert!(snap.info.iter().any(|(l, _)| l == "Versión"));
    assert!(snap.tables.iter().any(|t| t.key == "warehouses" && !t.rows.is_empty()));
    // A second snapshot right away reuses the warehouse-bound parts.
    let again = s.monitor().await.expect("monitor");
    assert_eq!(again.tables.len(), snap.tables.len());
}

/// The profiler sees a statement run from another session, once.
#[tokio::test]
#[ignore]
async fn profile() {
    let driver = dbine_driver_snowflake::drivers().remove(0);
    assert!(driver.supports_profiler());
    let cfg = config();
    let mut p = driver.connect(&cfg, None).await.expect("connect");
    let mut w = driver.connect(&cfg, None).await.expect("connect");
    let opts = dbine_driver::ProfilerOptions { database: cfg.database.clone(), change_server: false };
    let started = p.profiler_start(&opts).await.expect("profiler_start");
    println!("{started:?}");
    let marker = format!("dbine_prof_{}", std::process::id());
    let mut out = dbine_driver::QueryOutcome::default();
    w.execute(&format!("SELECT 1 AS {marker}"), 10, &mut out).await.expect("run");
    let mut got = Vec::new();
    let until = std::time::Instant::now() + std::time::Duration::from_secs(90);
    while std::time::Instant::now() < until && !got.iter().any(|s: &dbine_driver::ProfiledStatement| s.text.contains(&marker)) {
        got.extend(p.profiler_poll().await.expect("profiler_poll"));
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    // A few more reads: the overlap must not repeat it.
    for _ in 0..4 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        got.extend(p.profiler_poll().await.expect("profiler_poll"));
    }
    p.profiler_stop().await.expect("profiler_stop");
    let mine: Vec<_> = got.iter().filter(|s| s.text.contains(&marker)).collect();
    println!("{mine:#?}");
    assert_eq!(mine.len(), 1, "seen once");
    assert!(!got.iter().any(|s| s.text.contains("QUERY_HISTORY(END_TIME_RANGE_START")), "its own statements are left out");
}
