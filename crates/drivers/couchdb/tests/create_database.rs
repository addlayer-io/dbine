//! "Nueva base de datos" with options, against a real server
//! (`DBINE_TEST_COUCHDB_URL`, `http://user:pass@host:port`), skipped
//! without it:
//!
//! ```sh
//! DBINE_TEST_COUCHDB_URL=http://admin:secret@localhost:25202 \
//!   cargo test -p dbine-driver-couchdb --test create_database -- --ignored
//! ```

use dbine_driver::ConnectionConfig;
use std::collections::BTreeMap;

fn cfg() -> Option<(ConnectionConfig, String)> {
    let url = std::env::var("DBINE_TEST_COUCHDB_URL").ok()?;
    let rest = url.strip_prefix("http://")?;
    let (auth, host) = rest.split_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (h, p) = host.trim_end_matches('/').split_once(':')?;
    let c = ConnectionConfig {
        driver: "couchdb".into(),
        host: h.into(),
        port: p.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    Some((c, url.trim_end_matches('/').to_string()))
}

#[tokio::test]
#[ignore]
async fn couchdb_options() {
    let Some((c, url)) = cfg() else {
        eprintln!("DBINE_TEST_COUCHDB_URL not set; skipping");
        return;
    };
    let d = dbine_driver_couchdb::drivers().into_iter().next().unwrap();
    let mut s = d.connect(&c, None).await.unwrap();
    let _ = s.drop_database("dbine_create_opts").await;
    // `q` / `n` come only when the node's config sets them.
    eprintln!("{:?}", s.create_database_choices().await.unwrap());

    let o: BTreeMap<String, String> = [("q", "4"), ("n", "1"), ("partitioned", "true")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    eprintln!("{}", d.create_database_script("dbine_create_opts", &o).unwrap());
    s.create_database_with("dbine_create_opts", &o).await.unwrap();
    // The database's own info says how it was made.
    let info: serde_json::Value = reqwest::get(format!("{url}/dbine_create_opts")).await.unwrap().json().await.unwrap();
    eprintln!("{info}");
    assert_eq!(info["cluster"]["q"], 4);
    assert_eq!(info["cluster"]["n"], 1);
    assert_eq!(info["props"]["partitioned"], true);
    s.drop_database("dbine_create_opts").await.unwrap();

    // Without options it's the plain create.
    s.create_database_with("dbine_create_plain", &BTreeMap::new()).await.unwrap();
    s.drop_database("dbine_create_plain").await.unwrap();
}
