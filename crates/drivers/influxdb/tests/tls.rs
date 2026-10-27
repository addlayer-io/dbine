//! TLS against the HTTPS-only InfluxDB 2 of `scripts/tls-test-servers.sh`
//! (certificate signed by the throwaway test CA in `tests/tls/`, names
//! `localhost` and `127.0.0.1`). Skipped without the variable:
//!
//! ```sh
//! scripts/tls-test-servers.sh up influxdb
//! DBINE_TEST_INFLUXDB_TLS_PORT=26110 cargo test -p dbine-driver-influxdb --test tls -- --ignored
//! ```

use dbine_driver::{ConnectFailureKind, ConnectionConfig, Driver, Error, TUNNEL_ORIGIN_HOST};
use std::sync::Arc;

fn setup() -> Option<ConnectionConfig> {
    let port = std::env::var("DBINE_TEST_INFLUXDB_TLS_PORT").ok()?;
    let mut cfg = ConnectionConfig {
        driver: "influxdb".into(),
        host: "localhost".into(),
        port: port.parse().unwrap(),
        ..Default::default()
    };
    cfg.options.insert("org".into(), "dbine".into());
    cfg.options.insert("token".into(), "dbine-tls-token".into());
    Some(cfg)
}

fn driver() -> Arc<dyn Driver> {
    dbine_driver_influxdb::drivers()
        .into_iter()
        .find(|d| d.info().id == "influxdb")
        .unwrap()
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

fn kind(r: Result<(), Error>) -> Option<ConnectFailureKind> {
    r.err().and_then(|e| e.connect_failure()).map(|f| f.kind)
}

#[tokio::test]
#[ignore]
async fn new_connections_verify_and_every_check_can_be_turned_off() {
    let Some(base) = setup() else { return };
    // The default (https, verified) doesn't know the test CA…
    let cfg = new_connection(base.clone());
    assert_eq!(
        kind(connects(&cfg).await),
        Some(ConnectFailureKind::TlsUntrusted)
    );
    // …and does with it (curl's cacert).
    let mut with_ca = cfg.clone();
    with_ca.options.insert("cacert".into(), ca());
    connects(&with_ca).await.unwrap();
    // --skip-verify connects without it.
    let mut skip = cfg.clone();
    skip.options.insert("skip-verify".into(), "true".into());
    connects(&skip).await.unwrap();
    // Plain http to an HTTPS-only server fails, as it should.
    let mut plain = cfg.clone();
    plain.options.insert("scheme".into(), "http".into());
    assert!(connects(&plain).await.is_err());
}

/// Saved connections from before the TLS fields behave as they did:
/// encrypt → https verified (the test CA is unknown, so refused), trust →
/// the check skipped, neither → http.
#[tokio::test]
#[ignore]
async fn saved_connections_keep_their_behaviour() {
    let Some(base) = setup() else { return };
    for (encrypt, trust, ok) in [
        (false, false, false),
        (false, true, false),
        (true, false, false),
        (true, true, true),
    ] {
        let cfg = ConnectionConfig {
            encrypt,
            trust_server_certificate: trust,
            ..base.clone()
        };
        assert_eq!(connects(&cfg).await.is_ok(), ok, "{encrypt} {trust}");
    }
    // An https URL typed in the host with only `trust` skipped the check too.
    let cfg = ConnectionConfig {
        host: format!("https://localhost:{}", base.port),
        port: 0,
        trust_server_certificate: true,
        ..base.clone()
    };
    connects(&cfg).await.unwrap();
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
    cfg.options.insert("cacert".into(), ca());
    cfg.options
        .insert(TUNNEL_ORIGIN_HOST.into(), "localhost".into());
    connects(&cfg).await.unwrap();
    // A name the certificate doesn't carry is refused, though 127.0.0.1 is
    // in it.
    cfg.options
        .insert(TUNNEL_ORIGIN_HOST.into(), "db.wrong.example".into());
    assert_eq!(
        kind(connects(&cfg).await),
        Some(ConnectFailureKind::TlsUntrusted)
    );
    // An IP address can't be checked through the tunnel: refused.
    cfg.options
        .insert(TUNNEL_ORIGIN_HOST.into(), "10.1.2.3".into());
    assert_eq!(
        kind(connects(&cfg).await),
        Some(ConnectFailureKind::TlsUntrusted)
    );
}
