//! Oracle's "databases" in DBine are its schemas: a query in one reads
//! another as `schema.table`, which is what completion across databases
//! offers (`Driver::cross_database`, issue #20).
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test cross_database -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL").ok()?;
    let rest = url.strip_prefix("oracle://")?;
    let (cred, addr) = rest.split_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (hostport, service) = addr.split_once('/')?;
    let (host, port) = hostport.split_once(':')?;
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    Some(cfg)
}

#[tokio::test]
#[ignore]
async fn oracle() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_oracle::drivers()
        .into_iter()
        .find(|d| d.info().id == "oracle")
        .unwrap();
    assert!(d.cross_database());
    // The SYSTEM schema's session reads SYS's view by its schema's name.
    let mut s = d.connect(&c, Some("SYSTEM")).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "SELECT COUNT(*) FROM SYS.ALL_USERS WHERE USERNAME = 'SYSTEM'",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let n = &out.results.last().unwrap().rows[0][0];
    assert!(
        n == &serde_json::json!(1) || n == &serde_json::json!("1"),
        "{n}"
    );
}
