//! Editor scripts against a real Dremio OSS (see `integration.rs` for the
//! container; the first user must exist):
//!
//! ```sh
//! DBINE_TEST_DREMIO_URL=http://localhost:25947 cargo test -p dbine-driver-dremio --test script -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, Error, QueryOutcome, ScriptMode};

#[tokio::test]
#[ignore]
async fn statements_one_by_one() {
    let Ok(url) = std::env::var("DBINE_TEST_DREMIO_URL") else { return };
    let url = reqwest::Url::parse(&url).unwrap();
    let c = ConnectionConfig {
        driver: "dremio".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(9047),
        username: Some("dbine".into()),
        password: Some("secreto123".into()),
        ..Default::default()
    };
    let d = dbine_driver_dremio::drivers().remove(0);
    assert_eq!(d.script_mode(), ScriptMode::PerStatement);
    let mut s = d.connect(&c, None).await.unwrap();

    // As the app runs a script: one unit at a time, the session in between.
    let script = "USE \"$scratch\";\nSELECT 1 AS a;\nSELECT 1 fron x";
    let units = d.split_script(script);
    assert_eq!(units.len(), 3);
    let mut out = QueryOutcome::default();
    s.execute(&units[0].text, 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].tag.as_deref(), Some("USE"));
    assert!(out.log.iter().any(|m| m.text == "Contexto: $scratch"), "{:?}", out.log);
    assert_eq!(out.database.as_deref(), Some("$scratch"));
    s.execute(&units[1].text, 10, &mut out).await.unwrap();
    assert_eq!(out.results.len(), 2);
    let err = s.execute(&units[2].text, 10, &mut out).await.unwrap_err();
    let Error::Statement(e) = err else { panic!("{err:?}") };
    assert_eq!((e.line, e.offset), (Some(1), Some(14)), "{}", e.message);

    // The whole script in one call (the Backups / Users screens): the error
    // is placed in it.
    let mut out = QueryOutcome::default();
    let Error::Statement(e) = s.execute(script, 10, &mut out).await.unwrap_err() else { panic!() };
    assert_eq!((e.line, e.offset), (Some(3), Some(script.len() - 1)));
    assert_eq!(out.results.len(), 2);
}
