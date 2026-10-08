//! Row estimates against a real InfluxDB 3, skipped without
//! `DBINE_TEST_INFLUXDB3_URL`:
//!
//! ```sh
//! DBINE_TEST_INFLUXDB3_URL=http://localhost:25409 \
//!   cargo test -p dbine-driver-influxdb --test stats -- --ignored --nocapture
//! ```
//!
//! The estimates come from `system.parquet_files`; points written seconds
//! ago are still in the WAL, so the test checks that the catalog read
//! works and that whatever it returns names the database's measurements.
//! InfluxDB 1.x and 2.x keep no row statistics, and no version has
//! comments on objects.

use dbine_driver::{ConnectionConfig, QueryOutcome};

#[tokio::test]
#[ignore]
async fn influxdb3_row_estimates() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB3_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB3_URL not set; skipping");
        return;
    };
    let cfg = ConnectionConfig { driver: "influxdb3".into(), host: url, ..Default::default() };
    let d = dbine_driver_influxdb::drivers().into_iter().find(|d| d.info().id == "influxdb3").unwrap();
    let db = "dbine_stats";
    {
        let mut s = d.connect(&cfg, None).await.unwrap();
        let _ = s.drop_database(db).await;
        s.create_database(db).await.unwrap();
    }
    let mut s = d.connect(&cfg, Some(db)).await.unwrap();
    let mut out = QueryOutcome::default();
    let lp: Vec<String> = (0..50).map(|i| format!("cpu,h=h{} v={i} {}", i % 3, 1_700_000_000_000_000_000i64 + i)).collect();
    reqwest::Client::new()
        .post(format!("{}/api/v3/write_lp?db={db}&precision=nanosecond", cfg.host.trim_end_matches('/')))
        .body(lp.join("\n"))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();

    // The catalog read itself works with this token.
    s.execute("SELECT table_name, sum(row_count) AS row_count FROM system.parquet_files GROUP BY table_name", 10, &mut out)
        .await
        .unwrap();
    let objects = s.list_objects().await.unwrap();
    let got = s.row_estimates().await.unwrap();
    eprintln!("{got:?}");
    for e in &got {
        assert!(objects.iter().any(|o| o.kind == e.object.kind && o.name == e.object.name), "{e:?} not in {objects:?}");
    }
    assert!(s.object_comments().await.unwrap().is_empty());

    drop(s);
    let mut s = d.connect(&cfg, None).await.unwrap();
    s.drop_database(db).await.unwrap();
}
