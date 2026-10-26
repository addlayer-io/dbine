//! SQL Server's own TLS options (`Encrypt`, `TrustServerCertificate`,
//! `ServerCertificate`, `HostNameInCertificate`) against real servers: the
//! `dbine-test-tls-sqlserver` containers (tests/tls/README.md;
//! `scripts/tls-test-servers.sh up sqlserver` and
//! `CERT=selfsigned … up sqlserver`) and the plain `dbine-test-sqlserver`
//! (its auto-generated self-signed certificate).
//!
//! ```sh
//! cargo test -p dbine-driver-sqlserver --test tls_options -- --ignored --test-threads=1
//! ```

use dbine_driver::{ConnectionConfig, Driver, Error, QueryOutcome, TUNNEL_ORIGIN_HOST};
use std::sync::Arc;

const CA_SIGNED: u16 = 26013;
const SELF_SIGNED: u16 = 26513;
/// `dbine-test-sqlserver`: SQL Server's own self-signed certificate.
const DEFAULT_CERT: u16 = 25013;

fn pem(name: &str) -> String {
    format!("{}/../../../tests/tls/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn cfg(port: u16, encrypt: bool, trust: bool, options: &[(&str, &str)]) -> ConnectionConfig {
    let mut c = ConnectionConfig {
        driver: "sqlserver".into(),
        host: "localhost".into(),
        port,
        username: Some("sa".into()),
        password: Some(
            if port == DEFAULT_CERT {
                "Pw_12345!"
            } else {
                "Dbine-Tls-2026"
            }
            .into(),
        ),
        encrypt,
        trust_server_certificate: trust,
        ..Default::default()
    };
    for (k, v) in options {
        c.options.insert((*k).into(), (*v).into());
    }
    c
}

fn driver() -> Arc<dyn Driver> {
    dbine_driver_sqlserver::drivers()
        .into_iter()
        .find(|d| d.info().id == "sqlserver")
        .unwrap()
}

/// Connects and says whether the whole connection is encrypted.
async fn encrypted(c: &ConnectionConfig) -> Result<bool, Error> {
    let mut s = driver().connect(c, None).await?;
    let mut out = QueryOutcome::default();
    s.execute(
        "SELECT encrypt_option FROM sys.dm_exec_connections WHERE session_id = @@SPID",
        10,
        &mut out,
    )
    .await?;
    Ok(out.results.last().unwrap().rows[0][0] == "TRUE")
}

fn tls_failure(r: Result<bool, Error>) -> String {
    match r {
        Err(Error::ConnectFailed(f)) if f.kind.is_tls() => f.message,
        other => panic!("expected a TLS failure, got {other:?}"),
    }
}

#[tokio::test]
#[ignore]
async fn mandatory_verifies_the_certificate() {
    let ca = pem("ca.pem");
    let ok = cfg(
        CA_SIGNED,
        false,
        false,
        &[("Encrypt", "Mandatory"), ("ServerCertificate", &ca)],
    );
    assert!(encrypted(&ok).await.unwrap());
    // Without the CA, with another one, or against a self-signed server.
    let m =
        tls_failure(encrypted(&cfg(CA_SIGNED, false, false, &[("Encrypt", "Mandatory")])).await);
    assert!(m.contains("TrustServerCertificate"), "{m}");
    let wrong = pem("selfsigned.pem");
    let bad = cfg(
        CA_SIGNED,
        false,
        false,
        &[("Encrypt", "Mandatory"), ("ServerCertificate", &wrong)],
    );
    tls_failure(encrypted(&bad).await);
    let ss = cfg(
        SELF_SIGNED,
        false,
        false,
        &[("Encrypt", "Mandatory"), ("ServerCertificate", &ca)],
    );
    tls_failure(encrypted(&ss).await);
    // SQL Server's own self-signed certificate fails the new default too.
    tls_failure(
        encrypted(&cfg(
            DEFAULT_CERT,
            false,
            false,
            &[("Encrypt", "Mandatory")],
        ))
        .await,
    );
}

#[tokio::test]
#[ignore]
async fn the_name_checked() {
    let ca = pem("ca.pem");
    let named = |name: &str| {
        cfg(
            CA_SIGNED,
            false,
            false,
            &[
                ("Encrypt", "Mandatory"),
                ("ServerCertificate", &ca),
                ("HostNameInCertificate", name),
            ],
        )
    };
    tls_failure(encrypted(&named("wrong.example")).await);
    assert!(encrypted(&named("localhost")).await.unwrap());
    // Through an SSH tunnel: the far end's name, not 127.0.0.1 (which is
    // in the SAN and would pass).
    let tunnel = |origin: &str| {
        let mut c = cfg(
            CA_SIGNED,
            false,
            false,
            &[
                ("Encrypt", "Mandatory"),
                ("ServerCertificate", &ca),
                (TUNNEL_ORIGIN_HOST, origin),
            ],
        );
        c.host = "127.0.0.1".into();
        c
    };
    tls_failure(encrypted(&tunnel("wrong.example")).await);
    assert!(encrypted(&tunnel("localhost")).await.unwrap());
}

#[tokio::test]
#[ignore]
async fn trust_and_optional() {
    // TrustServerCertificate takes the self-signed and the default
    // certificates, encrypted.
    for port in [SELF_SIGNED, DEFAULT_CERT] {
        let c = cfg(
            port,
            false,
            false,
            &[("Encrypt", "Mandatory"), ("TrustServerCertificate", "true")],
        );
        assert!(encrypted(&c).await.unwrap(), "{port}");
        // Optional: TLS only around the login, nothing checked.
        let c = cfg(port, true, false, &[("Encrypt", "Optional")]);
        assert!(!encrypted(&c).await.unwrap(), "{port}");
    }
}

/// Connections saved before the options behave as they did: ff and ft
/// login-only TLS unchecked, tf encrypted and verified, tt encrypted
/// unchecked.
#[tokio::test]
#[ignore]
async fn legacy_connections_behave_as_before() {
    for port in [SELF_SIGNED, DEFAULT_CERT] {
        assert!(
            !encrypted(&cfg(port, false, false, &[])).await.unwrap(),
            "{port} ff"
        );
        assert!(
            !encrypted(&cfg(port, false, true, &[])).await.unwrap(),
            "{port} ft"
        );
        tls_failure(encrypted(&cfg(port, true, false, &[])).await);
        assert!(
            encrypted(&cfg(port, true, true, &[])).await.unwrap(),
            "{port} tt"
        );
    }
}
