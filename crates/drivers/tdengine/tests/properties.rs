//! "Propiedades" of a database against a real TDengine (taosAdapter's REST
//! port), skipped without `DBINE_TEST_TDENGINE_URL`:
//!
//! ```sh
//! DBINE_TEST_TDENGINE_URL=http://localhost:25641 \
//!   cargo test -p dbine-driver-tdengine --test properties -- --ignored --nocapture
//! ```

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{ConnectionConfig, Session};
use std::collections::BTreeMap;

fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test]
#[ignore]
async fn tdengine_properties() {
    let Some(url) = std::env::var("DBINE_TEST_TDENGINE_URL").ok() else {
        eprintln!("DBINE_TEST_TDENGINE_URL not set; skipping");
        return;
    };
    let url = reqwest::Url::parse(&url).expect("URL");
    let cfg = ConnectionConfig {
        driver: "tdengine".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        username: Some("root".into()),
        password: Some("taosdata".into()),
        ..Default::default()
    };
    let d = dbine_driver_tdengine::drivers().into_iter().next().unwrap();
    assert!(d.capabilities().database_properties);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let db = "dbine_props";
    let _ = s.drop_database(db).await;
    s.create_database(db).await.unwrap();

    let p = s.database_properties(db).await.unwrap();
    eprintln!("{:?}\n{:?}", p.values, p.info);
    assert_eq!(p.values.get("keep").map(String::as_str), Some("3650d"));
    assert_eq!(p.values.get("cachemodel").map(String::as_str), Some("none"));
    assert_eq!(p.values.get("replica").map(String::as_str), Some("1"));
    assert!(p.info.iter().any(|i| i.label == "Estado" && i.value == "ready"));
    assert!(p.warnings.contains_key("keep") && p.warnings.contains_key("replica"));

    let changes = c(&[
        ("keep", "365d"),
        ("cachemodel", "last_row"),
        ("cachesize", "2"),
        ("buffer", "128"),
        ("wal_level", "2"),
        ("wal_fsync_period", "1000"),
        ("minrows", "200"),
        ("stt_trigger", "4"),
    ]);
    eprintln!("{}", d.alter_database_script(db, &changes).unwrap());
    s.alter_database(db, &changes).await.unwrap();
    let p = s.database_properties(db).await.unwrap();
    for (k, v) in &changes {
        assert_eq!(p.values.get(k), Some(v), "{k}");
    }

    // A refused change after an applied one says how far it got.
    let e = s.alter_database(db, &c(&[("buffer", "64"), ("keep", "1d")])).await.unwrap_err().to_string();
    eprintln!("{e}");
    assert!(e.contains("se aplicaron 1 de 2 cambios"), "{e}");

    // Read-only connections refuse it.
    let mut ro = ReadOnlySession::new(d.connect(&cfg, None).await.unwrap());
    assert!(ro.alter_database(db, &c(&[("buffer", "96")])).await.is_err());

    s.drop_database(db).await.unwrap();
}
