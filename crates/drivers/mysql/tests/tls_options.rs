//! MySQL's `ssl-mode` and MariaDB's `ssl` / `ssl-verify-server-cert`
//! against real servers: the `dbine-test-tls-mysql` / `-mariadb` containers
//! (tests/tls/README.md; `scripts/tls-test-servers.sh up mysql mariadb`,
//! and `CERT=selfsigned … up mysql mariadb`) and `dbine-test-mysql-notls`, a
//! server without TLS:
//!
//! ```sh
//! docker run -d --name dbine-test-mysql-notls -e MARIADB_ROOT_PASSWORD=pw -p 26600:3306 mariadb:11 --skip-ssl
//! cargo test -p dbine-driver-mysql --test tls_options -- --ignored --test-threads=1
//! ```

use dbine_driver::{ConnectionConfig, Driver, Error, QueryOutcome, TUNNEL_ORIGIN_HOST};
use std::sync::Arc;

const MYSQL: u16 = 26011;
const MYSQL_SELF_SIGNED: u16 = 26511;
const MARIADB: u16 = 26012;
const MARIADB_SELF_SIGNED: u16 = 26512;
/// No TLS at all.
const NO_TLS: u16 = 26600;

fn pem(name: &str) -> String {
    format!("{}/../../../tests/tls/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn cfg(
    id: &str,
    port: u16,
    encrypt: bool,
    trust: bool,
    options: &[(&str, &str)],
) -> ConnectionConfig {
    let mut c = ConnectionConfig {
        driver: id.into(),
        host: "localhost".into(),
        port,
        username: Some("root".into()),
        password: Some(
            if port == NO_TLS {
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
    dbine_driver_mysql::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

/// Connects and says whether the session is encrypted (`Ssl_version`).
async fn ssl(c: &ConnectionConfig) -> Result<bool, Error> {
    let mut s = driver(&c.driver).connect(c, None).await?;
    let mut out = QueryOutcome::default();
    s.execute("SHOW SESSION STATUS LIKE 'Ssl_version'", 10, &mut out)
        .await?;
    let v = &out.results.last().unwrap().rows[0][1];
    Ok(v.as_str().is_some_and(|v| !v.is_empty()))
}

fn tls_failure(r: Result<bool, Error>) -> String {
    match r {
        Err(Error::ConnectFailed(f)) if f.kind.is_tls() => f.message,
        other => panic!("expected a TLS failure, got {other:?}"),
    }
}

#[tokio::test]
#[ignore]
async fn mysql_verified_modes_need_the_right_ca_and_name() {
    let ca = pem("ca.pem");
    let wrong = pem("selfsigned.pem");
    for mode in ["VERIFY_IDENTITY", "VERIFY_CA"] {
        let ok = cfg(
            "mysql",
            MYSQL,
            false,
            false,
            &[("ssl-mode", mode), ("ca_cert", &ca)],
        );
        assert!(ssl(&ok).await.unwrap(), "{mode}");
        let bad = cfg(
            "mysql",
            MYSQL,
            false,
            false,
            &[("ssl-mode", mode), ("ca_cert", &wrong)],
        );
        tls_failure(ssl(&bad).await);
        let ss = cfg(
            "mysql",
            MYSQL_SELF_SIGNED,
            false,
            false,
            &[("ssl-mode", mode), ("ca_cert", &ca)],
        );
        tls_failure(ssl(&ss).await);
    }
    // No CA: the test CA isn't a system root.
    let m = tls_failure(
        ssl(&cfg(
            "mysql",
            MYSQL,
            false,
            false,
            &[("ssl-mode", "VERIFY_IDENTITY")],
        ))
        .await,
    );
    assert!(m.contains("ssl-ca"), "{m}");
    // VERIFY_CA needs ssl-ca before connecting.
    assert!(matches!(
        ssl(&cfg(
            "mysql",
            MYSQL,
            false,
            false,
            &[("ssl-mode", "VERIFY_CA")]
        ))
        .await,
        Err(Error::Connect(_))
    ));
}

/// Through an SSH tunnel the client dials 127.0.0.1 and checks the real
/// server's name (127.0.0.1 alone would pass: it's in the SAN).
#[tokio::test]
#[ignore]
async fn the_name_checked_is_the_tunnels_far_end() {
    let ca = pem("ca.pem");
    let tunnel = |mode: &str, origin: &str| {
        let mut c = cfg(
            "mysql",
            MYSQL,
            false,
            false,
            &[
                ("ssl-mode", mode),
                ("ca_cert", &ca),
                (TUNNEL_ORIGIN_HOST, origin),
            ],
        );
        c.host = "127.0.0.1".into();
        c
    };
    assert!(ssl(&tunnel("VERIFY_IDENTITY", "localhost")).await.unwrap());
    tls_failure(ssl(&tunnel("VERIFY_IDENTITY", "wrong.example")).await);
    assert!(ssl(&tunnel("VERIFY_CA", "wrong.example")).await.unwrap());
}

#[tokio::test]
#[ignore]
async fn mysql_unverified_preferred_and_disabled() {
    for mode in ["REQUIRED", "PREFERRED"] {
        let c = cfg(
            "mysql",
            MYSQL_SELF_SIGNED,
            false,
            false,
            &[("ssl-mode", mode)],
        );
        assert!(ssl(&c).await.unwrap(), "{mode}");
    }
    assert!(!ssl(&cfg(
        "mysql",
        MYSQL,
        true,
        false,
        &[("ssl-mode", "DISABLED")]
    ))
    .await
    .unwrap());
    // A server without TLS: PREFERRED retries in plaintext (the server said
    // so before any credential went out), DISABLED is plaintext, the rest
    // say TLS isn't offered.
    for mode in ["PREFERRED", "DISABLED"] {
        assert!(
            !ssl(&cfg("mysql", NO_TLS, false, false, &[("ssl-mode", mode)]))
                .await
                .unwrap(),
            "{mode}"
        );
    }
    for mode in ["REQUIRED", "VERIFY_IDENTITY"] {
        match ssl(&cfg("mysql", NO_TLS, false, false, &[("ssl-mode", mode)])).await {
            Err(Error::ConnectFailed(f)) => {
                assert_eq!(
                    f.kind,
                    dbine_driver::ConnectFailureKind::TlsNotOffered,
                    "{mode}: {f:?}"
                );
                assert!(f.message.contains("PREFERRED"), "{}", f.message);
            }
            other => panic!("{mode}: {other:?}"),
        }
    }
}

#[tokio::test]
#[ignore]
async fn mariadb_names() {
    let ca = pem("ca.pem");
    let on = |port, verify: &str| {
        cfg(
            "mariadb",
            port,
            false,
            false,
            &[
                ("ssl", "true"),
                ("ssl-verify-server-cert", verify),
                ("ca_cert", &ca),
            ],
        )
    };
    assert!(ssl(&on(MARIADB, "true")).await.unwrap());
    tls_failure(ssl(&on(MARIADB_SELF_SIGNED, "true")).await);
    assert!(ssl(&on(MARIADB_SELF_SIGNED, "false")).await.unwrap());
    let m = tls_failure(
        ssl(&cfg(
            "mariadb",
            MARIADB,
            false,
            false,
            &[("ssl", "true"), ("ssl-verify-server-cert", "true")],
        ))
        .await,
    );
    assert!(m.contains("ssl-verify-server-cert"), "{m}");
    assert!(
        !ssl(&cfg("mariadb", MARIADB, true, false, &[("ssl", "false")]))
            .await
            .unwrap()
    );
    // OceanBase uses the same names (the MariaDB server stands in for it).
    let mut c = on(MARIADB, "true");
    c.driver = "oceanbase".into();
    assert!(ssl(&c).await.unwrap());
}

/// Connections saved before the options behave as they did: ff and ft
/// plaintext, tf verified, tt TLS unchecked.
#[tokio::test]
#[ignore]
async fn legacy_connections_behave_as_before() {
    let ca = pem("ca.pem");
    for (id, port, ss) in [
        ("mysql", MYSQL, MYSQL_SELF_SIGNED),
        ("mariadb", MARIADB, MARIADB_SELF_SIGNED),
    ] {
        assert!(
            !ssl(&cfg(id, port, false, false, &[])).await.unwrap(),
            "{id} ff"
        );
        assert!(
            !ssl(&cfg(id, port, false, true, &[])).await.unwrap(),
            "{id} ft"
        );
        assert!(
            ssl(&cfg(id, port, true, false, &[("ca_cert", &ca)]))
                .await
                .unwrap(),
            "{id} tf"
        );
        tls_failure(ssl(&cfg(id, ss, true, false, &[("ca_cert", &ca)])).await);
        assert!(ssl(&cfg(id, ss, true, true, &[])).await.unwrap(), "{id} tt");
        // A server without TLS: ff still connects.
        assert!(
            !ssl(&cfg(id, NO_TLS, false, false, &[])).await.unwrap(),
            "{id} ff plain"
        );
    }
}

/// A new connection with the form's defaults verifies.
#[tokio::test]
#[ignore]
async fn new_connections_verify_by_default() {
    let ca = pem("ca.pem");
    for (id, port, ss) in [
        ("mysql", MYSQL, MYSQL_SELF_SIGNED),
        ("tidb", MYSQL, MYSQL_SELF_SIGNED),
        ("cloudsql-mysql", MYSQL, MYSQL_SELF_SIGNED),
        ("mariadb", MARIADB, MARIADB_SELF_SIGNED),
    ] {
        let d = driver(id);
        let mut options: Vec<(&str, &str)> = d
            .info()
            .fields
            .iter()
            .filter(|f| f.tls.is_some() && !f.default.is_empty())
            .map(|f| (f.key, f.default))
            .collect();
        options.push(("ca_cert", &ca));
        assert!(
            ssl(&cfg(id, port, false, false, &options)).await.unwrap(),
            "{id}"
        );
        tls_failure(ssl(&cfg(id, ss, false, false, &options)).await);
    }
}
