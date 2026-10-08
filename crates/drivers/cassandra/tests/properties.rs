//! "Propiedades" of a keyspace against a real server
//! (`DBINE_TEST_CASSANDRA_URL`, `host:port`; `DBINE_TEST_SCYLLADB_URL` for
//! ScyllaDB), skipped without it:
//!
//! ```sh
//! DBINE_TEST_CASSANDRA_URL=localhost:25402 \
//!   cargo test -p dbine-driver-cassandra --test properties -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, Session};
use std::collections::BTreeMap;

async fn open(driver: &str, env: &str) -> Option<(std::sync::Arc<dyn dbine_driver::Driver>, Box<dyn Session>)> {
    let url = std::env::var(env).ok()?;
    let (host, port) = url.rsplit_once(':')?;
    let cfg = ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().ok()?, ..Default::default() };
    let d = dbine_driver_cassandra::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    let s = d.connect(&cfg, None).await.unwrap();
    Some((d, s))
}

fn c(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

async fn check(driver: &str, env: &str, tablets: Option<&str>) {
    let Some((d, mut s)) = open(driver, env).await else {
        eprintln!("{env} not set; skipping");
        return;
    };
    assert!(d.capabilities().database_properties);
    let ks = "dbine_props";
    let _ = s.drop_database(ks).await;
    let choices = s.create_database_choices().await.unwrap();
    let dc = choices.iter().find(|c| c.key == "datacenters").unwrap().values.first().expect("the local datacenter").clone();
    let mut o = c(&[("datacenters", &format!("{dc}:1"))]);
    if let Some(t) = tablets {
        o.insert("tablets".into(), t.into());
    }
    s.create_database_with(ks, &o).await.unwrap();
    s.execute(&format!("CREATE TABLE {ks}.t (id int PRIMARY KEY)"), 1, &mut Default::default()).await.unwrap();

    let p = s.database_properties(ks).await.unwrap();
    eprintln!("{:?}\n{:?}", p.values, p.info);
    assert_eq!(p.values.get("durable_writes").map(String::as_str), Some("true"));
    assert_eq!(p.values.get("datacenters"), Some(&format!("{dc}:1")));
    assert!(p.info.iter().any(|i| i.label == "Tablas" && i.value == "1"), "{:?}", p.info);
    assert!(p.warnings.contains_key("datacenters"));
    let tablets = p.info.iter().any(|i| i.label == "Tablets" && i.value == "Sí");

    // durable_writes off, and the replication rewritten with the same factor.
    let changes = c(&[("durable_writes", ""), ("datacenters", &format!("{dc}:1"))]);
    eprintln!("{}", d.alter_database_script(ks, &changes).unwrap());
    s.alter_database(ks, &changes).await.unwrap();
    let p = s.database_properties(ks).await.unwrap();
    assert_eq!(p.values.get("durable_writes").map(String::as_str), Some(""));
    // ScyllaDB: the replication keys keep durable_writes = false.
    let k_dcs = if driver == "scylladb" { "nd:datacenters" } else { "datacenters" };
    assert_eq!(p.values.get(k_dcs), Some(&format!("{dc}:1")), "{:?}", p.values);
    let changes = c(&[(k_dcs, &format!("{dc}:1"))]);
    eprintln!("{}", d.alter_database_script(ks, &changes).unwrap());
    s.alter_database(ks, &changes).await.unwrap();
    let p = s.database_properties(ks).await.unwrap();
    assert_eq!(p.values.get("durable_writes").map(String::as_str), Some(""), "a replication change keeps durable_writes");

    if !tablets {
        // To SimpleStrategy and back.
        let nd = if driver == "scylladb" { "nd:" } else { "" };
        let k = |key: &str| format!("{nd}{key}");
        let changes = c(&[(&k("class"), "SimpleStrategy"), (&k("replication_factor"), "1")]);
        eprintln!("{}", d.alter_database_script(ks, &changes).unwrap());
        s.alter_database(ks, &changes).await.unwrap();
        let p = s.database_properties(ks).await.unwrap();
        assert_eq!(p.values.get(&k("class")).map(String::as_str), Some("SimpleStrategy"));
        assert_eq!(p.values.get(&k("replication_factor")).map(String::as_str), Some("1"));
        assert_eq!(p.values.get("durable_writes").map(String::as_str), Some(""));
        s.alter_database(ks, &c(&[(&k("class"), "NetworkTopologyStrategy"), (&k("datacenters"), &format!("{dc}:1")), ("durable_writes", "true")]))
            .await
            .unwrap();
        let p = s.database_properties(ks).await.unwrap();
        assert_eq!(p.values.get("class").map(String::as_str), Some("NetworkTopologyStrategy"));
        assert_eq!(p.values.get("durable_writes").map(String::as_str), Some("true"));
    }

    // A failure after the first statement says how far it got.
    let e = s.alter_database(ks, &c(&[("durable_writes", "true"), ("datacenters", "no_such_dc:1")])).await;
    eprintln!("{e:?}");
    if let (Err(e), "cassandra") = (e, driver) {
        assert!(e.to_string().contains("se aplicaron 1 de 2"), "{e}");
    }
    // System keyspaces are refused.
    assert!(s.alter_database("system_auth", &c(&[("durable_writes", "true")])).await.is_err());
    s.drop_database(ks).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cassandra_properties() {
    check("cassandra", "DBINE_TEST_CASSANDRA_URL", None).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn scylladb_properties() {
    // With tablets (the default) and without (vnodes: any class).
    check("scylladb", "DBINE_TEST_SCYLLADB_URL", None).await;
    check("scylladb", "DBINE_TEST_SCYLLADB_URL", Some("false")).await;
}
