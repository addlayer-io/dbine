//! "Propiedades" of a database against real servers
//! (`DBINE_TEST_<ENGINE>_URL`, `mysql://user:pass@host:port`), each skipped
//! without its variable:
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//! DBINE_TEST_TIDB_URL=mysql://root@localhost:25014 \
//!   cargo test -p dbine-driver-mysql --test properties -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, Driver, Session};
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

fn changes(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

/// Create a temp database, check what "Propiedades" reads, apply
/// `apply`, read again and expect `apply` back; then drop it.
async fn round_trip(id: &str, env: &str, expect_fields: &[&str], apply: &[(&str, &str)]) -> Option<(Box<dyn Session>, Arc<dyn Driver>)> {
    let Some(cfg) = cfg(id, env) else {
        eprintln!("{env} not set; skipping");
        return None;
    };
    let d = driver(id);
    assert!(d.capabilities().database_properties);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let _ = s.drop_database("dbine_props").await;
    s.create_database_with("dbine_props", &changes(&[("charset", "utf8mb4"), ("collation", "utf8mb4_general_ci")])).await.unwrap();

    let p = s.database_properties("dbine_props").await.unwrap();
    eprintln!("{id}: {:?}\n{:?}", p.values, p.info);
    for k in expect_fields {
        assert!(p.fields.iter().any(|f| f.key == *k), "{id}: field {k}");
        assert!(p.values.contains_key(*k), "{id}: value of {k}");
    }
    assert_eq!(p.values["charset"], "utf8mb4");
    assert!(p.info.iter().any(|i| i.label == "Tablas"), "{id}: facts");
    assert!(p.choices.iter().any(|c| c.key == "collation" && !c.values.is_empty()));

    let ch = changes(apply);
    eprintln!("{}", d.alter_database_script("dbine_props", &ch).unwrap());
    s.alter_database("dbine_props", &ch).await.unwrap();
    let p = s.database_properties("dbine_props").await.unwrap();
    for (k, v) in apply {
        assert_eq!(p.values.get(*k).map(String::as_str), Some(*v), "{id}: {k}");
    }
    assert!(s.database_properties("dbine_nope").await.is_err());
    Some((s, d))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mysql_properties() {
    let Some((mut s, _)) = round_trip(
        "mysql",
        "DBINE_TEST_MYSQL_URL",
        &["charset", "collation", "read_only", "encryption"],
        &[("collation", "utf8mb4_bin"), ("read_only", "true")],
    )
    .await
    else {
        return;
    };
    // Read-only blocks DDL: back to writable (first), with a charset change.
    s.alter_database("dbine_props", &changes(&[("read_only", ""), ("charset", "latin1")])).await.unwrap();
    let p = s.database_properties("dbine_props").await.unwrap();
    assert_eq!((p.values["read_only"].as_str(), p.values["charset"].as_str()), ("", "latin1"));
    s.drop_database("dbine_props").await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mariadb_properties() {
    let Some((mut s, _)) = round_trip(
        "mariadb",
        "DBINE_TEST_MARIADB_URL",
        &["charset", "collation", "comment"],
        &[("comment", "ventas: it's"), ("collation", "utf8mb4_bin")],
    )
    .await
    else {
        return;
    };
    s.drop_database("dbine_props").await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn tidb_properties() {
    let Some((mut s, d)) = round_trip("tidb", "DBINE_TEST_TIDB_URL", &["charset", "collation", "placement_policy"], &[("collation", "utf8mb4_bin")]).await
    else {
        return;
    };
    // A placement policy, then back to none.
    let mut out = dbine_driver::QueryOutcome::default();
    s.execute("CREATE PLACEMENT POLICY IF NOT EXISTS dbine_props_p FOLLOWERS = 1", 10, &mut out).await.unwrap();
    s.alter_database("dbine_props", &changes(&[("placement_policy", "dbine_props_p")])).await.unwrap();
    assert_eq!(s.database_properties("dbine_props").await.unwrap().values["placement_policy"], "dbine_props_p");
    eprintln!("{}", d.alter_database_script("dbine_props", &changes(&[("placement_policy", "")])).unwrap());
    s.alter_database("dbine_props", &changes(&[("placement_policy", "")])).await.unwrap();
    assert_eq!(s.database_properties("dbine_props").await.unwrap().values["placement_policy"], "");
    s.drop_database("dbine_props").await.unwrap();
    let _ = s.execute("DROP PLACEMENT POLICY IF EXISTS dbine_props_p", 10, &mut out).await;
}
