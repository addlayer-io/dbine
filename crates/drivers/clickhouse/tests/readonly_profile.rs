//! Users whose settings profile has `readonly = 1` (no setting may be
//! changed per query) or `readonly = 2`, with and without DBine's own
//! read-only mode. Against a real server:
//!
//! ```sh
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//!   cargo test -p dbine-driver-clickhouse --test readonly_profile -- --ignored
//! ```

use dbine_driver::transfer::{BatchSink, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session};
use serde_json::{json, Value};
use std::io;
use std::sync::{Arc, Mutex};

fn admin() -> Option<ConnectionConfig> {
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

fn driver() -> Arc<dyn Driver> {
    dbine_driver_clickhouse::drivers().into_iter().find(|d| d.info().id == "clickhouse").unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Vec<Vec<Value>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    out.results.pop().map(|r| r.rows).unwrap_or_default()
}

#[derive(Default)]
struct Count(u64);

impl BatchSink for Count {
    fn begin(&mut self, _: &[TransferColumn]) -> io::Result<()> {
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> io::Result<()> {
        self.0 += b.rows.len() as u64;
        Ok(())
    }
}

#[tokio::test]
#[ignore]
async fn readonly_profiles() {
    let Some(c) = admin() else { return };
    let d = driver();
    let mut a = d.connect(&c, None).await.unwrap();
    run(
        &mut a,
        "DROP USER IF EXISTS dbine_ro1_it, dbine_ro2_it;
         DROP SETTINGS PROFILE IF EXISTS dbine_ro1_it, dbine_ro2_it;
         CREATE SETTINGS PROFILE dbine_ro1_it SETTINGS readonly = 1;
         CREATE SETTINGS PROFILE dbine_ro2_it SETTINGS readonly = 2;
         CREATE USER dbine_ro1_it IDENTIFIED BY 'x' SETTINGS PROFILE 'dbine_ro1_it';
         CREATE USER dbine_ro2_it IDENTIFIED BY 'x' SETTINGS PROFILE 'dbine_ro2_it';
         GRANT SELECT ON *.* TO dbine_ro1_it, dbine_ro2_it;
         DROP DATABASE IF EXISTS dbine_ro_it; CREATE DATABASE dbine_ro_it;
         CREATE TABLE dbine_ro_it.t (id UInt8, big Int64, u UInt64, h Int128, d Decimal(38, 3), a Array(Int128), f Float64)
           ENGINE = MergeTree ORDER BY id;
         INSERT INTO dbine_ro_it.t VALUES (1, 9007199254740993, 18446744073709551615,
           170141183460469231731687303715884105727, '12345678901234567890.123', [170141183460469231731687303715884105727], 1.5)",
    )
    .await;
    let expected = vec![vec![
        json!(1),
        json!("9007199254740993"),
        json!("18446744073709551615"),
        json!("170141183460469231731687303715884105727"),
        json!("12345678901234567890.123"),
        json!("[170141183460469231731687303715884105727]"),
        json!(1.5),
    ]];
    // Servers that quote 64-bit integers in JSON by default quote the array's.
    let unquote = |mut rows: Vec<Vec<Value>>| {
        if let Some(Value::String(a)) = rows[0].get_mut(5) {
            *a = a.replace('"', "");
        }
        rows
    };
    assert_eq!(unquote(run(&mut a, "SELECT * FROM dbine_ro_it.t").await), expected);
    // Nested 64-bit integers and decimals look the same for every user.
    let nested = "SELECT [1.25::Decimal(16, 2), 99999999999999.99], map('k', 18446744073709551615::UInt64)";
    let nested_rows = run(&mut a, nested).await;

    // DBine's read-only mode on an admin: a statement's own `readonly = 0`
    // must not turn the server-side `readonly = 1` off.
    let mut ro = d.connect(&ConnectionConfig { read_only: true, ..c.clone() }, None).await.unwrap();
    assert_eq!(run(&mut ro, "SELECT getSetting('readonly')").await, vec![vec![json!(1)]]);
    let mut out = QueryOutcome::default();
    assert!(ro.execute("SELECT 1 SETTINGS readonly = 0", 10, &mut out).await.is_err());
    assert_eq!(run(&mut ro, "SELECT getSetting('readonly')").await, vec![vec![json!(1)]]);
    let e = ro
        .execute("# c\nCREATE TABLE dbine_ro_it.bypass (x UInt8) ENGINE = Memory", 10, &mut out)
        .await
        .expect_err("write in read-only mode");
    assert!(e.to_string().contains("readonly"), "{e}");

    for user in ["dbine_ro1_it", "dbine_ro2_it"] {
        for read_only in [false, true] {
            let cfg = ConnectionConfig {
                username: Some(user.into()),
                password: Some("x".into()),
                database: "dbine_ro_it".into(),
                read_only,
                ..c.clone()
            };
            let ctx = format!("{user}, read_only = {read_only}");
            let mut s = d.connect(&cfg, None).await.unwrap_or_else(|e| panic!("{ctx}: {e}"));
            assert_eq!(unquote(run(&mut s, "SELECT * FROM dbine_ro_it.t").await), expected, "{ctx}");
            assert_eq!(run(&mut s, nested).await, nested_rows, "{ctx}");
            assert!(s.list_databases().await.unwrap().contains(&"dbine_ro_it".to_string()), "{ctx}");
            assert_eq!(s.list_objects().await.unwrap().len(), 1, "{ctx}");
            let t = ObjectRef { kind: "table".into(), schema: Some("dbine_ro_it".into()), name: "t".into() };
            assert_eq!(s.columns(&t).await.unwrap().len(), 7, "{ctx}");
            assert_eq!(s.database_schema().await.unwrap().len(), 1, "{ctx}");
            let mut out = QueryOutcome::default();
            s.explain("SELECT * FROM dbine_ro_it.t", false, 10, &mut out).await.unwrap_or_else(|e| panic!("{ctx}: {e}"));
            assert_eq!(out.plans.len(), 1, "{ctx}");
            // Errors still come through.
            let mut out = QueryOutcome::default();
            assert!(s.execute("SELECT * FROM dbine_ro_it.missing", 10, &mut out).await.is_err(), "{ctx}");
            assert!(s.execute("INSERT INTO dbine_ro_it.t (id) VALUES (2)", 10, &mut out).await.is_err(), "{ctx}");
            // Transfers read with RowBinary.
            let sink = Arc::new(Mutex::new(Count::default()));
            let spec = ReadSpec { table: t.clone(), columns: None, filter: None };
            assert_eq!(s.read_batches(&spec, sink.clone()).await.unwrap_or_else(|e| panic!("{ctx}: {e}")), 1);
            assert_eq!(sink.lock().unwrap().0, 1, "{ctx}");
            s.permissions(None).await.unwrap_or_else(|e| panic!("{ctx}: {e}"));
        }
    }

    run(
        &mut a,
        "DROP DATABASE dbine_ro_it; DROP USER dbine_ro1_it, dbine_ro2_it; DROP SETTINGS PROFILE dbine_ro1_it, dbine_ro2_it",
    )
    .await;
}
