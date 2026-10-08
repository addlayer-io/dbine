//! "Propiedades" of a database against real servers (see `integration.rs`
//! for the containers; user root / root), each skipped without its
//! variable:
//!
//! ```sh
//! DBINE_TEST_IOTDB_URL=http://localhost:25405 DBINE_TEST_IOTDB2_URL=http://localhost:27150 \
//!   cargo test -p dbine-driver-iotdb --test properties -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};
use std::collections::BTreeMap;

fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

async fn check(env: &str, v1: bool) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let cfg = ConnectionConfig {
        driver: "iotdb".into(),
        host: url,
        username: Some("root".into()),
        password: Some("root".into()),
        ..Default::default()
    };
    let d = dbine_driver_iotdb::drivers().into_iter().find(|d| d.info().id == "iotdb").unwrap();
    assert!(d.capabilities().database_properties);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let db = "dbine_props";
    let _ = s.drop_database(db).await;
    if !v1 {
        // IoTDB 2 keeps the TTL rules of a dropped database.
        let mut out = QueryOutcome::default();
        let _ = s.execute("UNSET TTL TO root.dbine_props", 10, &mut out).await;
    }
    s.create_database_with(db, &c(&[("ttl", "1h")])).await.unwrap();

    let p = s.database_properties(db).await.unwrap();
    eprintln!("{:?}\n{:?}", p.values, p.info);
    assert_eq!(p.values.get("ttl").map(String::as_str), Some("1h"));
    assert!(p.values.contains_key("data_region_group_num"));
    assert!(p.info.iter().any(|i| i.label.starts_with("Réplicas de los datos")));
    assert!(p.warnings.contains_key("ttl"));

    let changes = c(&[("ttl", "2d"), ("schema_region_group_num", "2"), ("data_region_group_num", "3")]);
    eprintln!("{}", d.alter_database_script(db, &changes).unwrap());
    s.alter_database(db, &changes).await.unwrap();
    let p = s.database_properties(db).await.unwrap();
    for (k, v) in [("ttl", "2d"), ("schema_region_group_num", "2"), ("data_region_group_num", "3")] {
        assert_eq!(p.values.get(k).map(String::as_str), Some(v), "{k}");
    }

    s.alter_database(db, &c(&[("ttl", "")])).await.unwrap();
    assert_eq!(s.database_properties(db).await.unwrap().values["ttl"], "");

    s.drop_database(db).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn iotdb1_properties() {
    check("DBINE_TEST_IOTDB_URL", true).await;
}

#[tokio::test]
#[ignore]
async fn iotdb2_properties() {
    check("DBINE_TEST_IOTDB2_URL", false).await;
}
