//! Editor scripts on TDengine (see `integration.rs` for the container):
//!
//! ```sh
//! DBINE_TEST_TDENGINE_URL=http://localhost:25641 cargo test -p dbine-driver-tdengine --test script -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, Error, QueryOutcome, ScriptMode};

#[tokio::test]
#[ignore]
async fn statements_one_by_one() {
    let Ok(url) = std::env::var("DBINE_TEST_TDENGINE_URL") else { return };
    let url = reqwest::Url::parse(&url).unwrap();
    let cfg = ConnectionConfig {
        driver: "tdengine".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(6041),
        username: Some("root".into()),
        password: Some("taosdata".into()),
        ..Default::default()
    };
    let d = dbine_driver_tdengine::drivers().remove(0);
    assert_eq!(d.script_mode(), ScriptMode::PerStatement);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("CREATE DATABASE IF NOT EXISTS dbine_script; USE dbine_script; CREATE TABLE IF NOT EXISTS t (ts TIMESTAMP, v INT)", 10, &mut out)
        .await
        .unwrap();
    assert!(out.log.iter().any(|m| m.text == "Base de datos: dbine_script"), "{:?}", out.log);
    // USE lasts for the next runs.
    let mut out = QueryOutcome::default();
    s.execute("INSERT INTO t VALUES (NOW, 1); SELECT COUNT(*) FROM t", 10, &mut out).await.unwrap();
    assert_eq!(out.results.len(), 2);

    // An error with TDengine's code, placed at what it quotes.
    let script = "SELECT 1;\nselect *\nfron t";
    let mut out = QueryOutcome::default();
    let err = s.execute(script, 10, &mut out).await.unwrap_err();
    let Error::Statement(e) = err else { panic!("{err:?}") };
    assert_eq!(e.code.as_deref(), Some("0x2600"), "{}", e.message);
    assert_eq!((e.line, e.offset), (Some(3), Some(script.find("fron").unwrap())));
    assert_eq!(out.results.len(), 1);
    s.execute("DROP DATABASE dbine_script", 10, &mut QueryOutcome::default()).await.unwrap();
}
