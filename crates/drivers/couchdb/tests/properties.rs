//! "Propiedades" of a database against a real server
//! (`DBINE_TEST_COUCHDB_URL`, `http://user:pass@host:port`), skipped
//! without it:
//!
//! ```sh
//! DBINE_TEST_COUCHDB_URL=http://admin:secret@localhost:25202 \
//!   cargo test -p dbine-driver-couchdb --test properties -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};
use serde_json::Value;
use std::collections::BTreeMap;

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_COUCHDB_URL").ok()?;
    let rest = url.strip_prefix("http://")?;
    let (auth, host) = rest.split_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (h, p) = host.trim_end_matches('/').split_once(':')?;
    Some(ConnectionConfig {
        driver: "couchdb".into(),
        host: h.into(),
        port: p.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    })
}

fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test]
#[ignore]
async fn couchdb_properties() {
    let Some(cfg) = cfg() else {
        eprintln!("DBINE_TEST_COUCHDB_URL not set; skipping");
        return;
    };
    let d = dbine_driver_couchdb::drivers().into_iter().next().unwrap();
    assert!(d.capabilities().database_properties);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let db = "dbine_props";
    let _ = s.drop_database(db).await;
    s.create_database(db).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&format!("POST /{db} {{\"_id\": \"a\", \"n\": 1}}"), 10, &mut out).await.unwrap();

    let p = s.database_properties(db).await.unwrap();
    eprintln!("{:?}\n{:?}", p.values, p.info);
    assert_eq!(p.values.get("revs_limit").map(String::as_str), Some("1000"));
    assert!(p.values.contains_key("purged_infos_limit"));
    assert!(p.info.iter().any(|i| i.label == "Documentos" && i.value == "1"), "{:?}", p.info);
    assert!(p.warnings.contains_key("security"));
    let sec: Value = serde_json::from_str(&p.values["security"]).unwrap();
    assert!(sec["members"]["names"].is_array());

    let security = r#"{"admins": {"names": [], "roles": ["ops"]}, "members": {"names": ["ana"], "roles": ["_admin"]}}"#;
    let changes = c(&[("revs_limit", "250"), ("purged_infos_limit", "500"), ("security", security)]);
    eprintln!("{}", d.alter_database_script(db, &changes).unwrap());
    s.alter_database(db, &changes).await.unwrap();

    let p = s.database_properties(db).await.unwrap();
    assert_eq!(p.values.get("revs_limit").map(String::as_str), Some("250"));
    assert_eq!(p.values.get("purged_infos_limit").map(String::as_str), Some("500"));
    let sec: Value = serde_json::from_str(&p.values["security"]).unwrap();
    assert_eq!(sec["members"]["names"][0], "ana");
    assert_eq!(sec["admins"]["roles"][0], "ops");

    // The server's refusal comes back; bad values never reach it.
    let e = s.alter_database("dbine_props_missing", &c(&[("revs_limit", "10")])).await.unwrap_err();
    eprintln!("{e}");
    assert!(d.alter_database_script(db, &c(&[("security", r#"{"members": {"names": [1]}}"#)])).is_err());

    // Back to public, then drop.
    s.alter_database(db, &c(&[("security", "{}")])).await.unwrap();
    let p = s.database_properties(db).await.unwrap();
    let sec: Value = serde_json::from_str(&p.values["security"]).unwrap();
    assert_eq!(sec["members"]["names"], serde_json::json!([]));
    s.drop_database(db).await.unwrap();
}
