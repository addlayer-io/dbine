//! `bolt+ssc` pins the server's certificate (trust on first use), against
//! `dbine-test-tls-neo4j` of `scripts/tls-test-servers.sh` (Bolt, TLS
//! optional, a certificate signed by `tests/tls/ca.pem`):
//!
//! ```sh
//! DBINE_TEST_TLS_NEO4J=localhost:26050 \
//!   cargo test -p dbine-driver-neo4j --test tls_pin -- --ignored
//! ```
//!
//! There's no second Neo4j with another certificate: a pin of another
//! certificate (`selfsigned.pem`'s) stands in for a server that changed.

use dbine_driver::{ConnectFailureKind, ConnectionConfig, Error};

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../tests/tls");

fn fingerprint(file: &str) -> String {
    let certs = dbine_tls::read_ca_file(&format!("{ROOT}/{file}"), file).unwrap();
    dbine_tls::fingerprint(&certs[0])
}

#[tokio::test]
#[ignore]
async fn bolt_ssc_pins_the_certificate() {
    let Ok(url) = std::env::var("DBINE_TEST_TLS_NEO4J") else {
        eprintln!("DBINE_TEST_TLS_NEO4J not set; skipping");
        return;
    };
    let (host, port) = url.rsplit_once(':').unwrap();
    let cfg = |pin: Option<&str>| ConnectionConfig {
        driver: "neo4j".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some("neo4j".into()),
        password: Some("Dbine-Tls-2026".into()),
        options: [("scheme", "bolt+ssc")]
            .into_iter()
            .chain(pin.map(|p| ("tls_pin", p)))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    };
    let d = dbine_driver_neo4j::drivers()
        .into_iter()
        .find(|d| d.info().id == "neo4j")
        .unwrap();
    let (server, other) = (fingerprint("server.pem"), fingerprint("selfsigned.pem"));
    // First use: accepted, and the certificate to pin is reported.
    let mut s = d.connect(&cfg(None), None).await.unwrap();
    assert_eq!(s.tls_pin_seen(), Some(server.clone()));
    s.server_version().await.unwrap();
    // Pinned: the same certificate passes, nothing new to save.
    let mut p = d.connect(&cfg(Some(&server)), None).await.unwrap();
    assert_eq!(p.tls_pin_seen(), None);
    p.server_version().await.unwrap();
    // Pinned to another certificate: refused as a changed one.
    match d.connect(&cfg(Some(&other)), None).await {
        Err(Error::ConnectFailed(f)) if f.kind == ConnectFailureKind::TlsPinChanged => {
            assert!(f.message.contains(&dbine_tls::display_fingerprint(&server)));
        }
        Err(e) => panic!("expected a changed certificate, got {e:?}"),
        Ok(_) => panic!("connected with another certificate"),
    }
}
