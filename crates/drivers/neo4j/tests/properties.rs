//! "Propiedades" of a database against real servers, each skipped without
//! its variable (`user:pass@host:port`, or `host:port` for Memgraph):
//!
//! ```sh
//! DBINE_TEST_NEO4J_EE_URL=neo4j:dbine-test-pass@localhost:17688 \
//! DBINE_TEST_NEO4J_URL=neo4j:dbine-test-pass@localhost:17687 \
//! DBINE_TEST_MEMGRAPH_URL=localhost:27687 \
//!   cargo test -p dbine-driver-neo4j --test properties -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, Error};
use std::collections::BTreeMap;

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let (auth, hp) = url.rsplit_once('@').map_or((None, url.as_str()), |(a, h)| (Some(a), h));
    let (host, port) = hp.rsplit_once(':')?;
    let (user, pass) = auth.and_then(|a| a.split_once(':')).map_or((None, None), |(u, p)| (Some(u.to_string()), Some(p.to_string())));
    Some(ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().ok()?, username: user, password: pass, ..Default::default() })
}

fn changes(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

fn fact<'a>(p: &'a dbine_driver::DatabaseProperties, label: &str) -> Option<&'a str> {
    p.info.iter().find(|i| i.label == label).map(|i| i.value.as_str())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn neo4j_enterprise_properties() {
    let Some(cfg) = cfg("neo4j", "DBINE_TEST_NEO4J_EE_URL") else {
        eprintln!("DBINE_TEST_NEO4J_EE_URL not set; skipping");
        return;
    };
    let d = dbine_driver_neo4j::drivers().into_iter().find(|d| d.info().id == "neo4j").unwrap();
    assert!(d.capabilities().database_properties);
    let mut s = d.connect(&cfg, Some("system")).await.unwrap();
    let _ = s.drop_database("dbine-props").await;
    s.create_database("dbine-props").await.unwrap();

    let p = s.database_properties("dbine-props").await.unwrap();
    eprintln!("{:#?}", p.info);
    assert_eq!(fact(&p, "Estado"), Some("online"));
    assert_eq!(fact(&p, "Acceso"), Some("read-write"));
    assert_eq!(fact(&p, "Nodos"), Some("0"));
    assert_eq!(p.values.get("read_only").map(String::as_str), Some(""));
    assert_eq!(p.values.get("primaries").map(String::as_str), Some("1"));
    assert_eq!(p.values.get("tx_log_enrichment").map(String::as_str), Some("OFF"));
    assert!(p.warnings.contains_key("read_only") && p.warnings.contains_key("primaries"));

    let ch = changes(&[("read_only", "true"), ("tx_log_enrichment", "DIFF"), ("primaries", "1"), ("secondaries", "0")]);
    eprintln!("{}", d.alter_database_script("dbine-props", &ch).unwrap());
    s.alter_database("dbine-props", &ch).await.unwrap();
    let p = s.database_properties("dbine-props").await.unwrap();
    for (k, v) in [("read_only", "true"), ("tx_log_enrichment", "DIFF"), ("primaries", "1"), ("secondaries", "0")] {
        assert_eq!(p.values.get(k).map(String::as_str), Some(v), "{k}");
    }
    assert_eq!(fact(&p, "Acceso"), Some("read-only"));

    // A topology the single server can't hold fails with the server's reason.
    let e = s.alter_database("dbine-props", &changes(&[("primaries", "2")])).await.unwrap_err();
    eprintln!("{e}");

    // A failure after the first statement says how many were applied.
    let e = s.alter_database("dbine-props", &changes(&[("read_only", ""), ("primaries", "3")])).await.unwrap_err();
    assert!(e.to_string().contains("se aplicaron 1 de 2 cambios"), "{e}");
    let p = s.database_properties("dbine-props").await.unwrap();
    assert_eq!(p.values.get("read_only").map(String::as_str), Some(""));

    s.alter_database("dbine-props", &changes(&[("tx_log_enrichment", "OFF")])).await.unwrap();
    assert!(s.alter_database("system", &changes(&[("read_only", "true")])).await.is_err());
    let p = s.database_properties("system").await.unwrap();
    assert!(p.fields.is_empty());
    s.drop_database("dbine-props").await.unwrap();
}

/// Community: facts, and no changes.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn neo4j_community_properties() {
    let Some(cfg) = cfg("neo4j", "DBINE_TEST_NEO4J_URL") else {
        eprintln!("DBINE_TEST_NEO4J_URL not set; skipping");
        return;
    };
    let d = dbine_driver_neo4j::drivers().into_iter().find(|d| d.info().id == "neo4j").unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    if !s.server_version().await.unwrap().to_lowercase().contains("community") {
        eprintln!("not a Community server; skipping");
        return;
    }
    let p = s.database_properties("neo4j").await.unwrap();
    eprintln!("{:#?}", p.info);
    assert!(p.fields.is_empty());
    assert_eq!(fact(&p, "Estado"), Some("online"));
    assert!(fact(&p, "Edición").is_some());
    let e = s.alter_database("neo4j", &changes(&[("read_only", "true")])).await.unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "{e}");
}

/// Memgraph: facts from SHOW STORAGE INFO.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn memgraph_properties() {
    let Some(cfg) = cfg("memgraph", "DBINE_TEST_MEMGRAPH_URL") else {
        eprintln!("DBINE_TEST_MEMGRAPH_URL not set; skipping");
        return;
    };
    let d = dbine_driver_neo4j::drivers().into_iter().find(|d| d.info().id == "memgraph").unwrap();
    assert!(d.capabilities().database_properties);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let p = s.database_properties("memgraph").await.unwrap();
    eprintln!("{:#?}", p.info);
    assert!(p.fields.is_empty());
    assert!(fact(&p, "Nodos (vertex_count)").is_some());
    assert!(s.alter_database("memgraph", &changes(&[("read_only", "true")])).await.is_err());
}
