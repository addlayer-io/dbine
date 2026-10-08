//! "Propiedades" of a bucket against a real, provisioned node (see
//! `integration.rs`, which provisions it), skipped without
//! `DBINE_TEST_COUCHBASE_URL`:
//!
//! ```sh
//! DBINE_TEST_COUCHBASE_URL=http://localhost:25893 DBINE_TEST_COUCHBASE_MGMT_PORT=25891 \
//!   cargo test -p dbine-driver-couchbase --test properties -- --ignored
//! ```

use dbine_driver::ConnectionConfig;
use std::collections::BTreeMap;

const USER: &str = "Administrator";
const PASS: &str = "secreto1";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_COUCHBASE_URL").ok()?).expect("URL");
    let mut c = ConnectionConfig {
        driver: "couchbase".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(USER.into()),
        password: Some(PASS.into()),
        ..Default::default()
    };
    c.options.insert("mgmt_port".into(), std::env::var("DBINE_TEST_COUCHBASE_MGMT_PORT").unwrap_or_else(|_| "8091".into()));
    Some(c)
}

fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn couchbase_properties() {
    let Some(cfg) = cfg() else {
        eprintln!("DBINE_TEST_COUCHBASE_URL not set; skipping");
        return;
    };
    let d = dbine_driver_couchbase::drivers().into_iter().next().unwrap();
    assert!(d.capabilities().database_properties);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let b = "dbine_props";
    let _ = s.drop_database(b).await;
    s.create_database_with(b, &c(&[("ram_quota", "100"), ("replicas", "1"), ("eviction", "valueOnly")])).await.unwrap();

    let p = s.database_properties(b).await.unwrap();
    eprintln!("{:?}\n{:?}", p.values, p.info);
    assert_eq!(p.values.get("ram_quota").map(String::as_str), Some("100"));
    assert_eq!(p.values.get("replicas").map(String::as_str), Some("1"));
    assert_eq!(p.values.get("flush").map(String::as_str), Some(""));
    assert_eq!(p.values.get("eviction").map(String::as_str), Some("valueOnly"));
    assert!(p.info.iter().any(|i| i.label == "Documentos"));
    assert!(p.warnings.contains_key("replicas"));

    let mut changes = c(&[("ram_quota", "128"), ("replicas", "0"), ("flush", "true"), ("eviction", "fullEviction"), ("durability", "majority")]);
    // The ones this server reports (version and edition dependent).
    for (k, v) in [("rank", "10"), ("memory_low_watermark", "70"), ("memory_high_watermark", "80"), ("expiry_pager", "300"), ("access_scanner", "")] {
        if p.values.contains_key(k) {
            changes.insert(k.into(), v.into());
        }
    }
    eprintln!("{}", d.alter_database_script(b, &changes).unwrap());
    s.alter_database(b, &changes).await.unwrap();

    let p = s.database_properties(b).await.unwrap();
    eprintln!("{:?}", p.values);
    for (k, v) in &changes {
        assert_eq!(p.values.get(k), Some(v), "{k}");
    }

    // The server's refusal comes back as is.
    let e = s.alter_database(b, &c(&[("ram_quota", "1048576")])).await.unwrap_err();
    eprintln!("{e}");
    assert!(d.alter_database_script(b, &c(&[("replicas", "4")])).is_err());
    s.drop_database(b).await.unwrap();
}
