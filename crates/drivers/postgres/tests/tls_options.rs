//! libpq's TLS options against real servers: the `dbine-test-tls-postgres`
//! containers (tests/tls/README.md; `scripts/tls-test-servers.sh up postgres`
//! and `CERT=selfsigned scripts/tls-test-servers.sh up postgres`) and the
//! plain `dbine-test-postgres` (no TLS, password `pw`).
//!
//! ```sh
//! cargo test -p dbine-driver-postgres --test tls_options -- --ignored --test-threads=1
//! ```

use dbine_driver::{ConnectionConfig, Driver, Error, QueryOutcome, TUNNEL_ORIGIN_HOST};
use std::sync::Arc;

/// The test CA's server certificate (names localhost and 127.0.0.1).
const CA_SIGNED: u16 = 26010;
/// The same server with a self-signed certificate.
const SELF_SIGNED: u16 = 26510;
/// `dbine-test-postgres`: no TLS.
const PLAIN: u16 = 25010;

fn pem(name: &str) -> String {
    format!("{}/../../../tests/tls/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn cfg(port: u16, encrypt: bool, trust: bool, options: &[(&str, &str)]) -> ConnectionConfig {
    let mut c = ConnectionConfig {
        driver: "postgres".into(),
        host: "localhost".into(),
        port,
        database: "postgres".into(),
        username: Some("postgres".into()),
        password: Some(
            if port == PLAIN {
                "pw"
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

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

/// Connects and says whether the session is encrypted (`pg_stat_ssl`).
async fn ssl(id: &str, c: &ConnectionConfig) -> Result<bool, Error> {
    let mut s = driver(id).connect(c, None).await?;
    let mut out = QueryOutcome::default();
    s.execute(
        "SELECT ssl::text FROM pg_stat_ssl WHERE pid = pg_backend_pid()",
        10,
        &mut out,
    )
    .await?;
    Ok(out.results.last().unwrap().rows[0][0] == "true")
}

fn tls_failure(r: Result<bool, Error>) -> String {
    match r {
        Err(Error::ConnectFailed(f)) if f.kind.is_tls() => f.message,
        other => panic!("expected a TLS failure, got {other:?}"),
    }
}

#[tokio::test]
#[ignore]
async fn verified_modes_need_the_right_ca_and_name() {
    let ca = pem("ca.pem");
    let wrong = pem("selfsigned.pem");
    for mode in ["verify-full", "verify-ca"] {
        let ok = cfg(
            CA_SIGNED,
            false,
            false,
            &[("sslmode", mode), ("ssl_root_cert", &ca)],
        );
        assert!(ssl("postgres", &ok).await.unwrap(), "{mode}");
        // Another CA: refused.
        let bad = cfg(
            CA_SIGNED,
            false,
            false,
            &[("sslmode", mode), ("ssl_root_cert", &wrong)],
        );
        tls_failure(ssl("postgres", &bad).await);
    }
    // No CA file: the test CA isn't a system root.
    let m = tls_failure(
        ssl(
            "postgres",
            &cfg(CA_SIGNED, false, false, &[("sslmode", "verify-full")]),
        )
        .await,
    );
    assert!(m.contains("sslrootcert"), "{m}");
    // verify-ca needs the CA file before connecting.
    assert!(matches!(
        ssl(
            "postgres",
            &cfg(CA_SIGNED, false, false, &[("sslmode", "verify-ca")])
        )
        .await,
        Err(Error::Connect(_))
    ));
    // The self-signed server fails every verified mode with the test CA.
    for mode in ["verify-full", "verify-ca"] {
        let c = cfg(
            SELF_SIGNED,
            false,
            false,
            &[("sslmode", mode), ("ssl_root_cert", &ca)],
        );
        tls_failure(ssl("postgres", &c).await);
    }
}

/// Through an SSH tunnel the driver dials 127.0.0.1 and checks the real
/// server's name: a name the certificate doesn't carry fails verify-full
/// (127.0.0.1 alone would pass, it's in the SAN), and passes verify-ca.
#[tokio::test]
#[ignore]
async fn the_name_checked_is_the_tunnels_far_end() {
    let ca = pem("ca.pem");
    let tunnel = |mode: &str, origin: &str| {
        let mut c = cfg(
            CA_SIGNED,
            false,
            false,
            &[
                ("sslmode", mode),
                ("ssl_root_cert", &ca),
                (TUNNEL_ORIGIN_HOST, origin),
            ],
        );
        c.host = "127.0.0.1".into();
        c
    };
    assert!(ssl("postgres", &tunnel("verify-full", "localhost"))
        .await
        .unwrap());
    tls_failure(ssl("postgres", &tunnel("verify-full", "wrong.example")).await);
    assert!(ssl("postgres", &tunnel("verify-ca", "wrong.example"))
        .await
        .unwrap());
}

#[tokio::test]
#[ignore]
async fn unverified_and_plain_modes() {
    // require / prefer take the self-signed certificate, encrypted.
    for mode in ["require", "prefer"] {
        assert!(
            ssl(
                "postgres",
                &cfg(SELF_SIGNED, false, false, &[("sslmode", mode)])
            )
            .await
            .unwrap(),
            "{mode}"
        );
    }
    // disable: plaintext even where TLS is offered.
    assert!(!ssl(
        "postgres",
        &cfg(CA_SIGNED, true, false, &[("sslmode", "disable")])
    )
    .await
    .unwrap());
    // A server without TLS: disable and prefer connect in plaintext, a
    // verified mode says TLS isn't offered.
    for mode in ["disable", "prefer"] {
        assert!(
            !ssl("postgres", &cfg(PLAIN, false, false, &[("sslmode", mode)]))
                .await
                .unwrap()
        );
    }
    match ssl(
        "postgres",
        &cfg(PLAIN, false, false, &[("sslmode", "verify-full")]),
    )
    .await
    {
        Err(Error::ConnectFailed(f)) => {
            assert_eq!(
                f.kind,
                dbine_driver::ConnectFailureKind::TlsNotOffered,
                "{f:?}"
            );
            assert!(f.message.contains("sslmode=prefer"), "{}", f.message);
        }
        other => panic!("{other:?}"),
    }
}

/// Connections saved before `sslmode` behave as they did.
#[tokio::test]
#[ignore]
async fn legacy_connections_behave_as_before() {
    let ca = pem("ca.pem");
    // ff: TLS when offered, then verified: the self-signed server fails, the
    // CA-signed one passes with its CA, a server without TLS goes plain.
    tls_failure(ssl("postgres", &cfg(SELF_SIGNED, false, false, &[])).await);
    assert!(ssl(
        "postgres",
        &cfg(CA_SIGNED, false, false, &[("ssl_root_cert", &ca)])
    )
    .await
    .unwrap());
    assert!(!ssl("postgres", &cfg(PLAIN, false, false, &[]))
        .await
        .unwrap());
    // ft: TLS when offered, not checked.
    assert!(ssl("postgres", &cfg(SELF_SIGNED, false, true, &[]))
        .await
        .unwrap());
    assert!(!ssl("postgres", &cfg(PLAIN, false, true, &[]))
        .await
        .unwrap());
    // tf: TLS required and verified.
    assert!(ssl(
        "postgres",
        &cfg(CA_SIGNED, true, false, &[("ssl_root_cert", &ca)])
    )
    .await
    .unwrap());
    tls_failure(
        ssl(
            "postgres",
            &cfg(SELF_SIGNED, true, false, &[("ssl_root_cert", &ca)]),
        )
        .await,
    );
    assert!(ssl("postgres", &cfg(PLAIN, true, false, &[]))
        .await
        .is_err());
    // tt: TLS required, not checked.
    assert!(ssl("postgres", &cfg(SELF_SIGNED, true, true, &[]))
        .await
        .unwrap());
}

/// A new connection with the form's defaults verifies: with the CA it
/// connects encrypted, without it (or against a self-signed server) it
/// refuses.
#[tokio::test]
#[ignore]
async fn new_connections_verify_by_default() {
    let ca = pem("ca.pem");
    for id in ["postgres", "cloudsql_postgres", "timescaledb"] {
        let mode = driver(id)
            .info()
            .fields
            .iter()
            .find(|f| f.key == "sslmode")
            .unwrap()
            .default;
        let mut c = cfg(
            CA_SIGNED,
            true,
            false,
            &[("sslmode", mode), ("ssl_root_cert", &ca)],
        );
        c.driver = id.into();
        assert!(ssl(id, &c).await.unwrap(), "{id}");
        c.port = SELF_SIGNED;
        tls_failure(ssl(id, &c).await);
    }
}

/// The modes that don't verify the chain pin the certificate: the first
/// connection reports it, the same certificate passes, another server's
/// fails with `TlsPinChanged`. A saved connection without a pin (made
/// before pinning) connects and pins.
#[tokio::test]
#[ignore]
async fn unverified_modes_pin_the_certificate() {
    use dbine_driver::{ConnectFailureKind, TLS_PIN};
    let fp = |name: &str| {
        let pem = std::fs::read(pem(name)).unwrap();
        dbine_tls::fingerprint(&dbine_tls::pem_certs(&pem, name).unwrap()[0])
    };
    let server = fp("server.pem");
    let other_fp = dbine_tls::display_fingerprint(&fp("selfsigned.pem"));
    for (encrypt, trust, opts) in [
        (false, false, vec![("sslmode", "require")]),
        (false, false, vec![("sslmode", "prefer")]),
        // Legacy tt (shown as sslmode=require), saved before pinning.
        (true, true, vec![]),
    ] {
        let first = cfg(CA_SIGNED, encrypt, trust, &opts);
        let s = driver("postgres").connect(&first, None).await.unwrap();
        assert_eq!(
            s.tls_pin_seen().as_deref(),
            Some(server.as_str()),
            "{opts:?}"
        );
        let mut opts = opts.clone();
        opts.push((TLS_PIN, &server));
        let pinned = cfg(CA_SIGNED, encrypt, trust, &opts);
        let s = driver("postgres").connect(&pinned, None).await.unwrap();
        assert_eq!(s.tls_pin_seen(), None);
        let other = cfg(SELF_SIGNED, encrypt, trust, &opts);
        match driver("postgres").connect(&other, None).await {
            Err(Error::ConnectFailed(f)) => {
                assert_eq!(f.kind, ConnectFailureKind::TlsPinChanged, "{f:?}");
                assert!(f.message.contains(&other_fp), "{}", f.message);
            }
            Err(e) => panic!("{e}"),
            Ok(_) => panic!("another certificate passed the pin"),
        }
    }
    // Verified modes don't pin.
    let ca = pem("ca.pem");
    let v = cfg(
        CA_SIGNED,
        false,
        false,
        &[("sslmode", "verify-ca"), ("ssl_root_cert", &ca)],
    );
    let s = driver("postgres").connect(&v, None).await.unwrap();
    assert_eq!(s.tls_pin_seen(), None);
}
