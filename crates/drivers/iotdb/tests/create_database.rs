//! "Nueva base de datos" with options, against real servers (see
//! `integration.rs` for the containers; user root / root), each skipped
//! without its variable:
//!
//! ```sh
//! DBINE_TEST_IOTDB_URL=http://localhost:25405 DBINE_TEST_IOTDB2_URL=http://localhost:27150 \
//!   cargo test -p dbine-driver-iotdb --test create_database -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
use std::collections::BTreeMap;

fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// `SHOW DATABASES root.x`'s row, by column name.
async fn details(s: &mut Box<dyn Session>, db: &str) -> BTreeMap<String, Value> {
    let mut out = QueryOutcome::default();
    s.execute(&format!("SHOW DATABASES {db}"), 10, &mut out).await.unwrap();
    let r = out.results.last().unwrap();
    r.columns.iter().map(|c| c.name.clone()).zip(r.rows[0].iter().cloned()).collect()
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
    let mut s = d.connect(&cfg, None).await.unwrap();
    let _ = s.drop_database("dbine_create_opts").await;
    let choices = s.create_database_choices().await.unwrap();
    eprintln!("{choices:?}");
    assert!(choices.iter().any(|c| c.key == "time_partition_interval" && c.default.is_some()));

    let mut o = opts(&[("ttl", "1h"), ("time_partition_interval", "1d"), ("schema_region_group_num", "1"), ("data_region_group_num", "2")]);
    if v1 {
        o.insert("data_replication_factor".into(), "1".into());
    }
    eprintln!("{}", d.create_database_script("dbine_create_opts", &o).unwrap());
    s.create_database_with("dbine_create_opts", &o).await.unwrap();
    let row = details(&mut s, "root.dbine_create_opts").await;
    eprintln!("{row:?}");
    assert_eq!(row["TimePartitionInterval"].to_string(), "86400000");
    if v1 {
        assert_eq!(row["TTL"].to_string(), "3600000");
    } else {
        let mut out = QueryOutcome::default();
        s.execute("SHOW TTL ON root.dbine_create_opts", 10, &mut out).await.unwrap();
        assert_eq!(out.results.last().unwrap().rows[0][1], Value::String("3600000".into()));
    }
    s.drop_database("dbine_create_opts").await.unwrap();
    if !v1 {
        // IoTDB 2 keeps the TTL rules of a dropped database.
        let mut out = QueryOutcome::default();
        let _ = s.execute("UNSET TTL TO root.dbine_create_opts; UNSET TTL TO root.dbine_create_opts.**", 10, &mut out).await;
    }
    if !v1 {
        // IoTDB 2 refuses the replication factors: the server says so.
        let e = s.create_database_with("dbine_create_opts", &opts(&[("data_replication_factor", "1")])).await.unwrap_err();
        eprintln!("{e}");
    }

    // Without options it's the plain create.
    s.create_database_with("dbine_create_plain", &BTreeMap::new()).await.unwrap();
    s.drop_database("dbine_create_plain").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn iotdb1_options() {
    check("DBINE_TEST_IOTDB_URL", true).await;
}

#[tokio::test]
#[ignore]
async fn iotdb2_options() {
    check("DBINE_TEST_IOTDB2_URL", false).await;
}
