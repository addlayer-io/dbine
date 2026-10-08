//! "Propiedades" of a database against real servers, each test skipped
//! without its variable (see `integration.rs` for the containers):
//!
//! ```sh
//! DBINE_TEST_INFLUXDB1_URL=http://localhost:25404 \
//! DBINE_TEST_INFLUXDB_URL=http://localhost:25403 \
//! DBINE_TEST_INFLUXDB3_URL=http://localhost:25409 \
//!   cargo test -p dbine-driver-influxdb --test properties -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, Driver, Session};
use std::collections::BTreeMap;
use std::sync::Arc;

fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

async fn open(driver: &str, cfg: &ConnectionConfig) -> (Arc<dyn Driver>, Box<dyn Session>) {
    let d = dbine_driver_influxdb::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(d.capabilities().database_properties);
    let s = d.connect(cfg, None).await.unwrap();
    (d, s)
}

async fn apply(d: &Arc<dyn Driver>, s: &mut Box<dyn Session>, db: &str, changes: &BTreeMap<String, String>) {
    eprintln!("{}", d.alter_database_script(db, changes).unwrap());
    s.alter_database(db, changes).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn influxdb1_properties() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB1_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB1_URL not set; skipping");
        return;
    };
    let cfg = ConnectionConfig { driver: "influxdb1".into(), host: url, ..Default::default() };
    let (d, mut s) = open("influxdb1", &cfg).await;
    let db = "dbine_props";
    let _ = s.drop_database(db).await;
    s.create_database_with(db, &c(&[("duration", "30d"), ("rp_name", "mensual")])).await.unwrap();

    let p = s.database_properties(db).await.unwrap();
    eprintln!("{:?}\n{:?}", p.values, p.info);
    assert_eq!(p.values.get("rp:mensual:duration").map(String::as_str), Some("720h"));
    assert_eq!(p.values.get("rp:mensual:default").map(String::as_str), Some("true"));
    assert!(p.fields.iter().any(|f| f.key == "rp:mensual:shard_duration"));
    assert!(p.warnings.contains_key("rp:mensual:duration"));
    assert!(p.info.iter().any(|i| i.label == "Política predeterminada" && i.value == "mensual"));

    apply(&d, &mut s, db, &c(&[("rp:mensual:duration", "INF"), ("rp:mensual:shard_duration", "2h"), ("rp:mensual:replication", "1")])).await;
    let p = s.database_properties(db).await.unwrap();
    assert_eq!(p.values["rp:mensual:duration"], "INF");
    assert_eq!(p.values["rp:mensual:shard_duration"], "2h");
    assert_eq!(p.values["rp:mensual:replication"], "1");

    // A second policy takes the default.
    let mut out = dbine_driver::QueryOutcome::default();
    s.execute(&format!("CREATE RETENTION POLICY \"semanal\" ON \"{db}\" DURATION 7d REPLICATION 1"), 10, &mut out).await.unwrap();
    apply(&d, &mut s, db, &c(&[("rp:semanal:default", "true"), ("rp:semanal:duration", "14d")])).await;
    let p = s.database_properties(db).await.unwrap();
    assert_eq!(p.values["rp:semanal:default"], "true");
    assert_eq!(p.values["rp:semanal:duration"], "336h");
    assert_eq!(p.values["rp:mensual:default"], "");
    assert!(d.alter_database_script(db, &c(&[("rp:semanal:default", "")])).is_err());

    s.drop_database(db).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn influxdb2_properties() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB_URL not set; skipping");
        return;
    };
    let mut cfg = ConnectionConfig { driver: "influxdb".into(), host: url, ..Default::default() };
    cfg.options.insert("org".into(), std::env::var("DBINE_TEST_INFLUXDB_ORG").unwrap_or("dbine".into()));
    cfg.options.insert("token".into(), std::env::var("DBINE_TEST_INFLUXDB_TOKEN").unwrap_or("dbinetoken".into()));
    let (d, mut s) = open("influxdb", &cfg).await;
    let db = "dbine_props";
    let _ = s.drop_database(db).await;
    s.create_database_with(db, &c(&[("retention", "30d"), ("description", "prueba")])).await.unwrap();

    let p = s.database_properties(db).await.unwrap();
    eprintln!("{:?}\n{:?}", p.values, p.info);
    assert_eq!(p.values.get("retention").map(String::as_str), Some("30d"));
    assert_eq!(p.values.get("description").map(String::as_str), Some("prueba"));
    assert!(p.info.iter().any(|i| i.label == "Id" && !i.value.is_empty()));
    assert!(p.warnings.contains_key("retention"));

    apply(&d, &mut s, db, &c(&[("retention", "7d"), ("shard_duration", "1d"), ("description", "otra")])).await;
    let p = s.database_properties(db).await.unwrap();
    assert_eq!(p.values["retention"], "7d");
    assert_eq!(p.values["shard_duration"], "1d");
    assert_eq!(p.values["description"], "otra");

    // Only the shard duration: the retention stays.
    apply(&d, &mut s, db, &c(&[("shard_duration", "2h")])).await;
    let p = s.database_properties(db).await.unwrap();
    assert_eq!((p.values["retention"].as_str(), p.values["shard_duration"].as_str()), ("7d", "2h"));

    apply(&d, &mut s, db, &c(&[("retention", ""), ("description", "")])).await;
    let p = s.database_properties(db).await.unwrap();
    assert_eq!((p.values["retention"].as_str(), p.values["description"].as_str()), ("0", ""));

    s.drop_database(db).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn influxdb3_properties() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB3_URL") else {
        eprintln!("DBINE_TEST_INFLUXDB3_URL not set; skipping");
        return;
    };
    let cfg = ConnectionConfig { driver: "influxdb3".into(), host: url, ..Default::default() };
    let (d, mut s) = open("influxdb3", &cfg).await;
    let db = "dbine_props";
    let _ = s.drop_database(db).await;
    s.create_database_with(db, &c(&[("retention", "30d")])).await.unwrap();

    let p = s.database_properties(db).await.unwrap();
    eprintln!("{:?}\n{:?}", p.values, p.info);
    assert_eq!(p.values.get("retention").map(String::as_str), Some("30d"));
    assert!(p.fields.iter().any(|f| f.key == "retention"), "3.2+ changes the retention");
    assert!(p.info.iter().any(|i| i.label == "Tablas (measurements)"));

    apply(&d, &mut s, db, &c(&[("retention", "7d")])).await;
    assert_eq!(s.database_properties(db).await.unwrap().values["retention"], "7d");
    apply(&d, &mut s, db, &c(&[("retention", "")])).await;
    assert_eq!(s.database_properties(db).await.unwrap().values["retention"], "");

    s.drop_database(db).await.unwrap();
}
