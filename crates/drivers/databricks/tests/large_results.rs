//! Results over the API's inline 25 MiB, against a real workspace. They come
//! from `range`, so it needs no table and leaves nothing behind. Skipped
//! without the variables (as in tests/transfer.rs):
//!
//! ```sh
//! DBINE_TEST_DATABRICKS_HOST=dbc-….cloud.databricks.com \
//! DBINE_TEST_DATABRICKS_WAREHOUSE=abcdef1234567890 \
//! DBINE_TEST_DATABRICKS_TOKEN=dapi… DBINE_TEST_DATABRICKS_CATALOG=main \
//!   cargo test -p dbine-driver-databricks --test large_results -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, ResultColumn, RowSink, RowSinkRef};
use std::sync::{Arc, Mutex};

fn env(k: &str) -> Option<String> {
    std::env::var(format!("DBINE_TEST_DATABRICKS_{k}"))
        .ok()
        .filter(|v| !v.is_empty())
}

fn cfg() -> Option<ConnectionConfig> {
    let mut c = ConnectionConfig {
        driver: "databricks".into(),
        host: env("HOST")?,
        database: env("CATALOG")?,
        ..Default::default()
    };
    c.options.insert("warehouse".into(), env("WAREHOUSE")?);
    c.options.insert("token".into(), env("TOKEN")?);
    c.options.insert(
        "schema".into(),
        env("SCHEMA").unwrap_or_else(|| "default".into()),
    );
    Some(c)
}

#[derive(Default)]
struct Counted {
    begun: usize,
    rows: u64,
}

impl RowSink for Counted {
    fn begin(&mut self, _: usize, _: &[ResultColumn]) -> std::io::Result<()> {
        self.begun += 1;
        Ok(())
    }
    fn row(&mut self, _: usize, _: &[serde_json::Value]) -> std::io::Result<()> {
        self.rows += 1;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn databricks() {
    let Some(cfg) = cfg() else {
        eprintln!("sin DBINE_TEST_DATABRICKS_*: se omite");
        return;
    };
    let d = dbine_driver_databricks::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.expect("connect");

    // Ten million short rows: over the limit whole, not with the row limit.
    let mut out = QueryOutcome::default();
    s.execute("SELECT id FROM range(10000000)", 5000, &mut out)
        .await
        .expect("many rows");
    let r = &out.results[0];
    assert_eq!(r.rows.len(), 5000);
    assert!(r.truncated, "there were more");

    // 5,000 rows of 10 KiB: 50 MiB even with the row limit, so by links.
    let wide = "SELECT id, repeat('x', 10240) AS txt FROM range(100000)";
    let mut out = QueryOutcome::default();
    s.execute(wide, 5000, &mut out).await.expect("wide rows");
    let r = &out.results[0];
    assert_eq!(r.rows.len(), 5000);
    assert!(r.truncated, "there were more");

    // A sink takes all of it, whatever its size.
    let sink = Arc::new(Mutex::new(Counted::default()));
    let mut out = QueryOutcome {
        sink: Some(RowSinkRef(sink.clone())),
        ..Default::default()
    };
    s.execute(
        "SELECT id, repeat('x', 1024) AS txt FROM range(100000)",
        usize::MAX,
        &mut out,
    )
    .await
    .expect("the whole result");
    let got = sink.lock().unwrap();
    assert_eq!((got.begun, got.rows), (1, 100_000));
    assert_eq!(out.results[0].total_rows, 100_000);
    assert!(!out.results[0].truncated);
}
