//! Editor scripts on InfluxDB 1.x (`influxdb:1.8`, see `integration.rs`):
//!
//! ```sh
//! DBINE_TEST_INFLUXDB1_URL=http://localhost:25404 cargo test -p dbine-driver-influxdb --test script -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, Error, QueryOutcome, ScriptMode};

#[tokio::test]
#[ignore]
async fn influxql_use_and_errors() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB1_URL") else { return };
    let d = dbine_driver_influxdb::drivers().into_iter().find(|d| d.info().id == "influxdb1").unwrap();
    assert_eq!(d.script_mode(), ScriptMode::Whole);
    let cfg = ConnectionConfig { driver: "influxdb1".into(), host: url.clone(), ..Default::default() };
    let mut s = d.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("DROP DATABASE dbine_script; CREATE DATABASE dbine_script", 10, &mut out).await.unwrap();
    let r = reqwest::Client::new().post(format!("{url}/write?db=dbine_script&precision=s")).body("cpu,host=a value=1 1700000000").send().await.unwrap();
    assert!(r.status().is_success());

    // USE switches the database for what follows, in this run and the next.
    let mut out = QueryOutcome::default();
    s.execute("SHOW DATABASES;\nUSE dbine_script;\nSELECT * FROM cpu", 10, &mut out).await.unwrap();
    assert_eq!(out.results.len(), 3);
    assert_eq!(out.results[1].tag.as_deref(), Some("USE"));
    assert_eq!(out.results[2].rows.len(), 1);
    assert!(out.log.iter().any(|m| m.text == "Base de datos: dbine_script"));
    let mut out = QueryOutcome::default();
    s.execute("SHOW MEASUREMENTS", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows, vec![vec![serde_json::json!("cpu")]]);
    // With a retention policy.
    let mut out = QueryOutcome::default();
    s.execute("USE dbine_script.autogen; SELECT count(value) FROM cpu", 10, &mut out).await.unwrap();
    assert_eq!(out.results.len(), 2);

    // A parse error: nothing of that request runs; placed in the script.
    let script = "SELECT * FROM cpu;\nSELECT * FROM";
    let mut out = QueryOutcome::default();
    let Error::Statement(e) = s.execute(script, 10, &mut out).await.unwrap_err() else { panic!() };
    assert_eq!(e.line, Some(2), "{}", e.message);
    assert_eq!(e.offset, Some(script.len()));
    assert!(out.results.is_empty());

    // A statement the server refuses: the ones before it ran; it is placed.
    let script = "SELECT * FROM cpu;\nSELECT * FROM \"nope_db\".\"autogen\".\"cpu\";\nSHOW DATABASES";
    let mut out = QueryOutcome::default();
    let err = s.execute(script, 10, &mut out).await.unwrap_err();
    let Error::Statement(e) = err else { panic!("{err:?}") };
    assert_eq!((e.line, e.offset), (Some(2), Some(script.find("SELECT * FROM \"nope").unwrap())), "{}", e.message);
    assert_eq!(out.results.len(), 1);

    s.execute("DROP DATABASE dbine_script", 10, &mut QueryOutcome::default()).await.unwrap();
}
