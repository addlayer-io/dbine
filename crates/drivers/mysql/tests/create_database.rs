//! "Nueva base de datos" with options, against real servers
//! (`DBINE_TEST_<ENGINE>_URL`, `mysql://user:pass@host:port`), each skipped
//! without its variable:
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//! DBINE_TEST_TIDB_URL=mysql://root@localhost:25044 \
//! DBINE_TEST_STARROCKS_URL=mysql://root@localhost:25030 \
//! DBINE_TEST_GREPTIMEDB_URL=mysql://root@localhost:25017 \
//!   cargo test -p dbine-driver-mysql --test create_database -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, Driver, QueryOutcome, Session};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_mysql::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

async fn text(s: &mut Box<dyn Session>, sql: &str, col: usize) -> String {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    match &out.results.last().unwrap().rows[0][col] {
        Value::String(v) => v.clone(),
        v => v.to_string(),
    }
}

/// Create `db` with `options`, check it with `verify`, drop it; then the
/// plain create.
async fn round_trip(id: &str, env: &str, options: &[(&str, &str)], verify: &str, col: usize, expect: &[&str]) {
    let Some(cfg) = cfg(id, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver(id);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let db = "dbine_create_opts";
    let _ = s.drop_database(db).await;
    let choices = s.create_database_choices().await.unwrap();
    eprintln!("{id}: {:?}", choices.iter().map(|c| (&c.key, &c.default, c.values.len())).collect::<Vec<_>>());
    let options = opts(options);
    eprintln!("{}", d.create_database_script(db, &options).unwrap());
    s.create_database_with(db, &options).await.unwrap();
    let got = text(&mut s, verify, col).await;
    eprintln!("{got}");
    for e in expect {
        assert!(got.contains(e), "{id}: {e} not in {got}");
    }
    s.drop_database(db).await.unwrap();
    s.create_database_with("dbine_create_plain", &BTreeMap::new()).await.unwrap();
    s.drop_database("dbine_create_plain").await.unwrap();
}

const SCHEMATA: &str = "SELECT CONCAT(DEFAULT_CHARACTER_SET_NAME, '|', DEFAULT_COLLATION_NAME) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = 'dbine_create_opts'";

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mysql_options() {
    if let Some(cfg) = cfg("mysql", "DBINE_TEST_MYSQL_URL") {
        let mut s = driver("mysql").connect(&cfg, None).await.unwrap();
        let choices = s.create_database_choices().await.unwrap();
        let get = |k: &str| choices.iter().find(|c| c.key == k).unwrap();
        assert!(get("charset").values.iter().any(|v| v == "latin1") && get("charset").default.is_some());
        assert!(get("collation").values.iter().any(|v| v == "utf8mb4_bin") && get("collation").default.is_some());
    }
    round_trip("mysql", "DBINE_TEST_MYSQL_URL", &[("charset", "latin1"), ("collation", "latin1_swedish_ci")], SCHEMATA, 0, &["latin1|latin1_swedish_ci"]).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mariadb_options() {
    round_trip(
        "mariadb",
        "DBINE_TEST_MARIADB_URL",
        &[("charset", "utf8mb4"), ("collation", "utf8mb4_bin"), ("comment", "ventas 'año'")],
        "SELECT CONCAT(DEFAULT_CHARACTER_SET_NAME, '|', DEFAULT_COLLATION_NAME, '|', SCHEMA_COMMENT) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = 'dbine_create_opts'",
        0,
        &["utf8mb4|utf8mb4_bin|ventas 'año'"],
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn tidb_options() {
    let Some(cfg) = cfg("tidb", "DBINE_TEST_TIDB_URL") else {
        eprintln!("DBINE_TEST_TIDB_URL not set; skipping");
        return;
    };
    let mut s = driver("tidb").connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("CREATE PLACEMENT POLICY IF NOT EXISTS dbine_pp FOLLOWERS=1", 10, &mut out).await.unwrap();
    let choices = s.create_database_choices().await.unwrap();
    assert!(choices.iter().any(|c| c.key == "placement_policy" && c.values.iter().any(|v| v == "dbine_pp")), "{choices:?}");
    round_trip(
        "tidb",
        "DBINE_TEST_TIDB_URL",
        &[("charset", "utf8mb4"), ("collation", "utf8mb4_general_ci"), ("placement_policy", "dbine_pp")],
        "SELECT CONCAT(DEFAULT_CHARACTER_SET_NAME, '|', DEFAULT_COLLATION_NAME, '|', TIDB_PLACEMENT_POLICY_NAME) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = 'dbine_create_opts'",
        0,
        &["utf8mb4|utf8mb4_general_ci|dbine_pp"],
    )
    .await;
    s.execute("DROP PLACEMENT POLICY dbine_pp", 10, &mut out).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn starrocks_options() {
    // A shared-nothing cluster has no storage volumes: only the plain
    // create applies.
    round_trip("starrocks", "DBINE_TEST_STARROCKS_URL", &[], "SHOW CREATE DATABASE dbine_create_opts", 1, &["dbine_create_opts"]).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn greptimedb_options() {
    round_trip("greptimedb", "DBINE_TEST_GREPTIMEDB_URL", &[("ttl", "7d")], "SHOW CREATE DATABASE dbine_create_opts", 1, &["ttl", "7d"]).await;
}
