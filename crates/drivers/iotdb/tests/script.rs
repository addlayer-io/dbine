//! Editor scripts on IoTDB (see `integration.rs` for the container):
//!
//! ```sh
//! DBINE_TEST_IOTDB_URL=http://localhost:25405 cargo test -p dbine-driver-iotdb --test script -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, Error, QueryOutcome, ScriptMode};

#[tokio::test]
#[ignore]
async fn statements_one_by_one() {
    let Ok(url) = std::env::var("DBINE_TEST_IOTDB_URL") else { return };
    let cfg = ConnectionConfig {
        driver: "iotdb".into(),
        host: url,
        username: Some("root".into()),
        password: Some("root".into()),
        ..Default::default()
    };
    let d = dbine_driver_iotdb::drivers().remove(0);
    assert_eq!(d.script_mode(), ScriptMode::PerStatement);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let script = "SHOW DATABASES;\n-- a comment\nselect * fron root.x";
    let units = d.split_script(script);
    assert_eq!(units.len(), 2);
    let mut out = QueryOutcome::default();
    s.execute(&units[0].text, 10, &mut out).await.unwrap();
    let err = s.execute(&units[1].text, 10, &mut out).await.unwrap_err();
    let Error::Statement(e) = err else { panic!("{err:?}") };
    assert_eq!((e.line, e.offset), (Some(1), Some(14)), "{}", e.message);
    // The whole script at once: placed in it.
    let mut out = QueryOutcome::default();
    let Error::Statement(e) = s.execute(script, 10, &mut out).await.unwrap_err() else { panic!() };
    assert_eq!((e.line, e.offset), (Some(3), Some(script.find("root.x").unwrap())));
    assert_eq!(out.results.len(), 1);
}
