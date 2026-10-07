//! "Nueva base de datos" (a keyspace) with options, against a real server
//! (`DBINE_TEST_CASSANDRA_URL`, `host:port`; `DBINE_TEST_SCYLLADB_URL` for
//! ScyllaDB), skipped without it:
//!
//! ```sh
//! DBINE_TEST_CASSANDRA_URL=localhost:25402 \
//!   cargo test -p dbine-driver-cassandra --test create_database -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
use std::collections::BTreeMap;

async fn open(driver: &str, env: &str) -> Option<(std::sync::Arc<dyn dbine_driver::Driver>, Box<dyn Session>)> {
    let url = std::env::var(env).ok()?;
    let (host, port) = url.rsplit_once(':')?;
    let cfg = ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().ok()?, ..Default::default() };
    let d = dbine_driver_cassandra::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    let s = d.connect(&cfg, None).await.unwrap();
    Some((d, s))
}

async fn row(s: &mut Box<dyn Session>, cql: &str) -> Vec<Value> {
    let mut out = QueryOutcome::default();
    s.execute(cql, 10, &mut out).await.unwrap();
    out.results.last().unwrap().rows[0].clone()
}

fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

async fn check(driver: &str, env: &str) {
    let Some((d, mut s)) = open(driver, env).await else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let _ = s.drop_database("dbine_create_opts").await;
    let choices = s.create_database_choices().await.unwrap();
    let dcs = &choices.iter().find(|c| c.key == "datacenters").unwrap().values;
    let dc = dcs.first().expect("the local datacenter").clone();

    let o = opts(&[("datacenters", &format!("{dc}:1")), ("durable_writes", "false")]);
    eprintln!("{}", d.create_database_script("dbine_create_opts", &o).unwrap());
    s.create_database_with("dbine_create_opts", &o).await.unwrap();
    let r = row(&mut s, "SELECT replication, durable_writes FROM system_schema.keyspaces WHERE keyspace_name = 'dbine_create_opts'").await;
    eprintln!("{r:?}");
    assert!(r[0].as_str().unwrap_or_default().contains(&format!("\"{dc}\":\"1\"")), "{r:?}");
    assert_eq!(r[1], Value::Bool(false));
    s.drop_database("dbine_create_opts").await.unwrap();

    if driver == "cassandra" {
        let o = opts(&[("class", "SimpleStrategy"), ("replication_factor", "1")]);
        s.create_database_with("dbine_create_opts", &o).await.unwrap();
        let r = row(&mut s, "SELECT replication FROM system_schema.keyspaces WHERE keyspace_name = 'dbine_create_opts'").await;
        assert!(r[0].as_str().unwrap_or_default().contains("SimpleStrategy"), "{r:?}");
        s.drop_database("dbine_create_opts").await.unwrap();
    }

    // Without options it's the plain create.
    s.create_database_with("dbine_create_plain", &BTreeMap::new()).await.unwrap();
    s.drop_database("dbine_create_plain").await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cassandra_options() {
    check("cassandra", "DBINE_TEST_CASSANDRA_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn scylladb_options() {
    check("scylladb", "DBINE_TEST_SCYLLADB_URL").await;
}
