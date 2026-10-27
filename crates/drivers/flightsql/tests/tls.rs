//! TLS against the GizmoSQL of `scripts/tls-test-servers.sh` (Flight SQL
//! over gRPC TLS only, certificate signed by the throwaway test CA in
//! `tests/tls/`, names `localhost` and `127.0.0.1`). Skipped without the
//! variable:
//!
//! ```sh
//! scripts/tls-test-servers.sh up flightsql
//! DBINE_TEST_FLIGHTSQL_TLS_PORT=26130 cargo test -p dbine-driver-flightsql --test tls -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, Driver, Error, TUNNEL_ORIGIN_HOST};
use std::sync::Arc;

fn setup() -> Option<ConnectionConfig> {
    let port = std::env::var("DBINE_TEST_FLIGHTSQL_TLS_PORT").ok()?;
    Some(ConnectionConfig {
        driver: "flightsql".into(),
        host: "localhost".into(),
        port: port.parse().unwrap(),
        username: Some("gizmosql_username".into()),
        password: Some("Dbine-Tls-2026".into()),
        ..Default::default()
    })
}

fn driver() -> Arc<dyn Driver> {
    dbine_driver_flightsql::drivers().remove(0)
}

fn ca() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../../tests/tls/ca.pem").into()
}

/// A new connection's options: every TLS field at the form's default.
fn new_connection(mut cfg: ConnectionConfig) -> ConnectionConfig {
    for f in driver().info().fields.iter().filter(|f| f.tls.is_some()) {
        if !f.default.is_empty() {
            cfg.options.insert(f.key.into(), f.default.into());
        }
    }
    cfg
}

async fn connects(cfg: &ConnectionConfig) -> Result<(), Error> {
    driver().connect(cfg, None).await.map(|_| ())
}

#[tokio::test]
#[ignore]
async fn new_connections_verify() {
    let Some(base) = setup() else { return };
    // useEncryption=true verifies: the test CA is unknown…
    let cfg = new_connection(base);
    let e = connects(&cfg).await.unwrap_err();
    assert_eq!(
        e.connect_failure().map(|f| f.kind),
        Some(dbine_driver::ConnectFailureKind::TlsUntrusted),
        "{e}"
    );
    // …and trusted with tlsRootCerts.
    let mut with_ca = cfg.clone();
    with_ca.options.insert("tlsRootCerts".into(), ca());
    connects(&with_ca).await.unwrap();
    // useEncryption=false: plaintext, which this server doesn't speak.
    let mut plain = with_ca.clone();
    plain.options.insert("useEncryption".into(), "false".into());
    assert!(connects(&plain).await.is_err());
}

/// Saved connections behave as they did: encrypt (with or without trust,
/// tonic always verified) checks the certificate, so the test CA is refused;
/// neither is plaintext.
#[tokio::test]
#[ignore]
async fn saved_connections_keep_their_behaviour() {
    let Some(base) = setup() else { return };
    for (encrypt, trust) in [(false, false), (false, true), (true, false), (true, true)] {
        let cfg = ConnectionConfig {
            encrypt,
            trust_server_certificate: trust,
            ..base.clone()
        };
        assert!(connects(&cfg).await.is_err(), "{encrypt} {trust}");
        // With the CA (an option a saved connection didn't have) the
        // encrypted ones connect, so they did verify.
        let mut with_ca = cfg.clone();
        with_ca.options.insert("tlsRootCerts".into(), ca());
        assert_eq!(
            connects(&with_ca).await.is_ok(),
            encrypt,
            "{encrypt} {trust} + CA"
        );
    }
}

/// Through an SSH tunnel the driver dials 127.0.0.1 and checks the
/// certificate against the real server's name.
#[tokio::test]
#[ignore]
async fn a_tunnel_checks_the_real_servers_name() {
    let Some(base) = setup() else { return };
    let mut cfg = new_connection(ConnectionConfig {
        host: "127.0.0.1".into(),
        ..base
    });
    cfg.options.insert("tlsRootCerts".into(), ca());
    cfg.options
        .insert(TUNNEL_ORIGIN_HOST.into(), "localhost".into());
    connects(&cfg).await.unwrap();
    cfg.options
        .insert(TUNNEL_ORIGIN_HOST.into(), "db.wrong.example".into());
    assert!(connects(&cfg).await.is_err());
}
