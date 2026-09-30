//! `Session::permissions` against real servers (ignored by default):
//!
//! - 1.8 with auth on (see tests/security.rs for the container): the admin
//!   and a user with READ on one database; `DBINE_TEST_INFLUXDB1_URL`
//!   (auth off) allows everything.
//! - 2.x (tests/integration.rs): the operator token may create and drop
//!   buckets; a token that can't read authorizations leaves them unknown.
//! - 3 Core with auth on and an admin token (`influxdb3 create token
//!   --admin` in a `serve --node-id n1 --object-store memory` container);
//!   `DBINE_TEST_INFLUXDB3_URL` (auth off) allows everything.
//!
//! ```sh
//! DBINE_TEST_INFLUXDB1_AUTH_URL=http://admin:Dbine_pw1@localhost:27142 \
//! DBINE_TEST_INFLUXDB1_URL=http://localhost:25404 \
//! DBINE_TEST_INFLUXDB_URL=http://localhost:25403 \
//! DBINE_TEST_INFLUXDB3_URL=http://localhost:25409 \
//! DBINE_TEST_INFLUXDB3_AUTH_URL=http://localhost:27143 DBINE_TEST_INFLUXDB3_TOKEN=apiv3_… \
//!   cargo test -p dbine-driver-influxdb --test permissions -- --ignored --nocapture
//! ```

use dbine_driver::{Access, ConnectionConfig, Driver, QueryOutcome, Session};
use std::sync::Arc;

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_influxdb::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

fn denied(a: &Access, what: &str) -> bool {
    matches!(a, Access::Denied { missing } if missing.contains(what))
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
    assert!(out.error.is_none(), "{text}: {:?}", out.error);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn influxdb1() {
    let d = driver("influxdb1");
    if let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB1_URL") {
        let cfg = ConnectionConfig { driver: "influxdb1".into(), host: url, ..Default::default() };
        let p = d.connect(&cfg, None).await.unwrap().permissions(Some("_internal")).await.unwrap();
        eprintln!("auth off: {p:?}");
        assert_eq!((&p.profiler, &p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed));
    }
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB1_AUTH_URL") else { return };
    let (scheme, rest) = url.split_once("://").unwrap();
    let (auth, host) = rest.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let base = ConnectionConfig {
        driver: "influxdb1".into(),
        host: format!("{scheme}://{host}"),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    let mut admin = d.connect(&base, None).await.unwrap();
    let p = admin.permissions(Some("dbine_perm")).await.unwrap();
    eprintln!("admin: {p:?}");
    assert_eq!((&p.profiler, &p.create_database, &p.drop_database, &p.manage_security), (&Access::Allowed, &Access::Allowed, &Access::Allowed, &Access::Allowed));
    let _ = admin.execute("DROP USER dbine_perm_ltd", 10, &mut QueryOutcome::default()).await;
    run(&mut admin, "CREATE DATABASE dbine_perm").await;
    run(&mut admin, "CREATE USER dbine_perm_ltd WITH PASSWORD 'ltd_pw1'").await;
    run(&mut admin, "GRANT READ ON dbine_perm TO dbine_perm_ltd").await;

    let ltd = ConnectionConfig { username: Some("dbine_perm_ltd".into()), password: Some("ltd_pw1".into()), ..base.clone() };
    let mut s = d.connect(&ltd, Some("dbine_perm")).await.unwrap();
    let p = s.permissions(Some("dbine_perm")).await.unwrap();
    eprintln!("reader: {p:?}");
    assert!(denied(&p.profiler, "ALL PRIVILEGES") && denied(&p.create_database, "ALL PRIVILEGES"), "{p:?}");
    assert!(denied(&p.drop_database, "ALL PRIVILEGES") && denied(&p.manage_security, "ALL PRIVILEGES"), "{p:?}");
    // The server refuses it too.
    assert!(s.execute("DROP DATABASE dbine_perm", 10, &mut QueryOutcome::default()).await.is_err());

    run(&mut admin, "DROP USER dbine_perm_ltd").await;
    run(&mut admin, "DROP DATABASE dbine_perm").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn influxdb2() {
    let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB_URL") else { return };
    let org = std::env::var("DBINE_TEST_INFLUXDB_ORG").unwrap_or("dbine".into());
    let token = std::env::var("DBINE_TEST_INFLUXDB_TOKEN").unwrap_or("dbinetoken".into());
    let mut cfg = ConnectionConfig { driver: "influxdb".into(), host: url.clone(), ..Default::default() };
    cfg.options.insert("org".into(), org.clone());
    cfg.options.insert("token".into(), token.clone());
    let d = driver("influxdb");
    let p = d.connect(&cfg, Some("test")).await.unwrap().permissions(Some("test")).await.unwrap();
    eprintln!("operator: {p:?}");
    assert_eq!((&p.create_database, &p.drop_database), (&Access::Allowed, &Access::Allowed));
    assert_eq!((&p.profiler, &p.manage_security), (&Access::Unknown, &Access::Unknown));

    // A token that reads one bucket: it can't see the authorizations.
    let http = reqwest::Client::new();
    let auth = format!("Token {token}");
    let orgs: serde_json::Value = http.get(format!("{url}/api/v2/orgs")).query(&[("org", &org)]).header("Authorization", &auth).send().await.unwrap().json().await.unwrap();
    let org_id = orgs["orgs"][0]["id"].as_str().unwrap().to_string();
    let buckets: serde_json::Value = http.get(format!("{url}/api/v2/buckets")).query(&[("name", "test")]).header("Authorization", &auth).send().await.unwrap().json().await.unwrap();
    let bucket_id = buckets["buckets"][0]["id"].as_str().unwrap().to_string();
    let body = serde_json::json!({
        "orgID": org_id,
        "description": "dbine_perm",
        "permissions": [{ "action": "read", "resource": { "type": "buckets", "orgID": org_id, "id": bucket_id } }],
    });
    let created: serde_json::Value = http.post(format!("{url}/api/v2/authorizations")).header("Authorization", &auth).json(&body).send().await.unwrap().json().await.unwrap();
    let mut ltd = cfg.clone();
    ltd.options.insert("token".into(), created["token"].as_str().unwrap().to_string());
    let p = d.connect(&ltd, Some("test")).await.unwrap().permissions(Some("test")).await.unwrap();
    eprintln!("bucket reader: {p:?}");
    assert_eq!(p, dbine_driver::Permissions::default());
    let id = created["id"].as_str().unwrap();
    http.delete(format!("{url}/api/v2/authorizations/{id}")).header("Authorization", &auth).send().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn influxdb3() {
    let d = driver("influxdb3");
    let mut servers = Vec::new();
    if let Ok(url) = std::env::var("DBINE_TEST_INFLUXDB3_URL") {
        servers.push(ConnectionConfig { driver: "influxdb3".into(), host: url, ..Default::default() });
    }
    if let (Ok(url), Ok(token)) = (std::env::var("DBINE_TEST_INFLUXDB3_AUTH_URL"), std::env::var("DBINE_TEST_INFLUXDB3_TOKEN")) {
        let mut cfg = ConnectionConfig { driver: "influxdb3".into(), host: url, ..Default::default() };
        cfg.options.insert("token".into(), token);
        servers.push(cfg);
    }
    for cfg in servers {
        let p = d.connect(&cfg, None).await.unwrap().permissions(Some("x")).await.unwrap();
        eprintln!("{}: {p:?}", cfg.host);
        assert_eq!((&p.profiler, &p.create_database, &p.drop_database), (&Access::Allowed, &Access::Allowed, &Access::Allowed));
        assert_eq!(p.manage_security, Access::Unknown);
    }
}
