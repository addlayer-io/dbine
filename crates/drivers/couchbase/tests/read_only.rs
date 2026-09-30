//! DBine's read-only mode against a real Couchbase Server: bucket create and
//! drop are refused by the driver itself, before any request.
//!
//! ```sh
//! DBINE_TEST_COUCHBASE_URL=http://localhost:25893 DBINE_TEST_COUCHBASE_MGMT_PORT=25891 \
//!   cargo test -p dbine-driver-couchbase --test read_only -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, Error};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_COUCHBASE_URL").ok()?).expect("URL");
    let mut c = ConnectionConfig {
        driver: "couchbase".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("Administrator".into()),
        password: Some("secreto1".into()),
        read_only: true,
        ..Default::default()
    };
    c.options.insert("mgmt_port".into(), std::env::var("DBINE_TEST_COUCHBASE_MGMT_PORT").unwrap_or_else(|_| "8091".into()));
    Some(c)
}

#[tokio::test]
#[ignore]
async fn read_only_refuses_bucket_create_and_drop() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_COUCHBASE_URL not set; skipping");
        return;
    };
    let driver = dbine_driver_couchbase::drivers().remove(0);
    let mut s = driver.connect(&c, None).await.unwrap();
    let before = s.list_databases().await.unwrap();
    let probe = "dbine_ro_probe";
    assert!(!before.iter().any(|b| b == probe));

    let r = s.create_database(probe).await;
    assert!(matches!(&r, Err(Error::Query(m)) if m.contains("solo lectura")), "{r:?}");
    let victim = before.first().expect("a bucket to try to drop").clone();
    let r = s.drop_database(&victim).await;
    assert!(matches!(&r, Err(Error::Query(m)) if m.contains("solo lectura")), "{r:?}");

    let after = s.list_databases().await.unwrap();
    assert!(!after.iter().any(|b| b == probe), "the bucket was created");
    assert!(after.contains(&victim), "the bucket was dropped");
}
