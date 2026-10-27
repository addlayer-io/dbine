//! The TLS options (`encrypt`, `validateCertificate`, `trustStore`). There
//! is no SAP HANA container, so this checks the client's certificate
//! handling against TLS test servers that aren't HANA
//! (`dbine-test-tls-redis`'s TLS ports): the handshake either fails on the
//! certificate (untrusted, the message names the option) or passes, and then
//! HANA's protocol fails, which is expected.
//!
//! ```sh
//! DBINE_TEST_TLS=1 cargo test -p dbine-driver-hana --test tls -- --ignored --test-threads 1
//! ```

use dbine_driver::{ConnectFailureKind, ConnectionConfig, Error};

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../tests/tls");
/// `dbine-test-tls-redis`'s TLS port (test CA; SAN localhost, 127.0.0.1).
const CA_PORT: u16 = 26030;
/// `dbine-test-tls-ss-redis`'s TLS port (self-signed).
const SELF_SIGNED_PORT: u16 = 26530;

fn on() -> bool {
    let on = std::env::var("DBINE_TEST_TLS").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("DBINE_TEST_TLS not set; skipping");
    }
    on
}

fn cfg(host: &str, port: u16, options: &[(&str, &str)]) -> ConnectionConfig {
    ConnectionConfig {
        driver: "hana".into(),
        host: host.into(),
        port,
        username: Some("SYSTEM".into()),
        password: Some("x".into()),
        options: options
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    }
}

async fn connect(cfg: &ConnectionConfig) -> (ConnectFailureKind, String) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let driver = dbine_driver_hana::drivers().remove(0);
    match driver.connect(cfg, None).await {
        Ok(_) => panic!("not a HANA server: the connect can't succeed"),
        Err(Error::ConnectFailed(f)) => (f.kind, f.message),
        Err(e) => (ConnectFailureKind::Unknown, e.to_string()),
    }
}

fn untrusted((kind, msg): (ConnectFailureKind, String), what: &str) {
    eprintln!("{what}: {kind:?}: {msg}");
    assert_eq!(kind, ConnectFailureKind::TlsUntrusted, "{what}: {msg}");
}

fn handshake_passed((kind, msg): (ConnectFailureKind, String), what: &str) {
    eprintln!("{what}: {kind:?}: {msg}");
    assert_ne!(kind, ConnectFailureKind::TlsUntrusted, "{what}: {msg}");
    assert!(!msg.to_lowercase().contains("certificate"), "{what}: {msg}");
}

#[tokio::test]
#[ignore]
async fn encrypt_verifies_with_the_trust_store() {
    if !on() {
        return;
    }
    let ca = format!("{ROOT}/ca.pem");
    let wrong = format!("{ROOT}/selfsigned.pem");
    let enc = |host: &str, port: u16, more: &[(&str, &str)]| {
        let mut o = vec![("hana_encrypt", "true")];
        o.extend_from_slice(more);
        cfg(host, port, &o)
    };
    // The default (public roots) doesn't know the test CA.
    let r = connect(&enc("127.0.0.1", CA_PORT, &[])).await;
    assert!(r.1.contains("trustStore"), "{}", r.1);
    untrusted(r, "no trustStore");
    // Saved with the legacy "encrypt" checkbox only: verified too.
    let mut legacy = cfg("127.0.0.1", CA_PORT, &[]);
    legacy.encrypt = true;
    untrusted(connect(&legacy).await, "legacy tf");
    untrusted(
        connect(&enc("127.0.0.1", CA_PORT, &[("trustStore", &wrong)])).await,
        "wrong CA",
    );
    for host in ["localhost", "127.0.0.1"] {
        handshake_passed(
            connect(&enc(host, CA_PORT, &[("trustStore", &ca)])).await,
            &format!("trustStore ca.pem via {host}"),
        );
    }
    let other = std::env::var("DBINE_TEST_TLS_OTHER_HOST").unwrap_or_else(|_| "0.0.0.0".into());
    untrusted(
        connect(&enc(&other, CA_PORT, &[("trustStore", &ca)])).await,
        &format!("trustStore ca.pem, wrong name {other}"),
    );
    // Through an SSH tunnel the message says why.
    let mut t = enc("127.0.0.1", CA_PORT, &[]);
    t.options.insert(
        dbine_driver::TUNNEL_ORIGIN_HOST.into(),
        "hana.example.com".into(),
    );
    let r = connect(&t).await;
    assert!(r.1.contains("túnel SSH"), "{}", r.1);
    untrusted(r, "tunnel");
}

#[tokio::test]
#[ignore]
async fn validate_certificate_off_accepts_a_self_signed_server() {
    if !on() {
        return;
    }
    untrusted(
        connect(&cfg(
            "127.0.0.1",
            SELF_SIGNED_PORT,
            &[("hana_encrypt", "true")],
        ))
        .await,
        "self-signed, verified",
    );
    handshake_passed(
        connect(&cfg(
            "127.0.0.1",
            SELF_SIGNED_PORT,
            &[("hana_encrypt", "true"), ("validateCertificate", "false")],
        ))
        .await,
        "self-signed, validateCertificate=false",
    );
    // Saved with "encrypt" and "trust": unverified, as before.
    let mut legacy = cfg("127.0.0.1", SELF_SIGNED_PORT, &[]);
    legacy.encrypt = true;
    legacy.trust_server_certificate = true;
    handshake_passed(connect(&legacy).await, "legacy tt");
}
