//! TLS options against the test server of `scripts/tls-test-servers.sh`
//! (`dbine-test-tls-cassandra`, TLS optional, certificate signed by
//! `tests/tls/ca.pem` for localhost / 127.0.0.1 / host.docker.internal):
//!
//! ```sh
//! DBINE_TEST_TLS_CASSANDRA=localhost:26040 \
//! DBINE_TEST_TLS_OTHER_IP=::1 \
//!   cargo test -p dbine-driver-cassandra --test tls -- --ignored --test-threads=1
//! ```
//!
//! `DBINE_TEST_TLS_OTHER_IP`: an address of this machine that reaches the
//! server but isn't in the certificate (the host-name checks must fail
//! through it). `::1` works with Docker Desktop / OrbStack; the Mac's LAN IP
//! doesn't reach Cassandra's native port there, even in plaintext.

use dbine_driver::{ConnectionConfig, Error, TUNNEL_ORIGIN_HOST};

fn tls_file(name: &str) -> String {
    format!("{}/../../../tests/tls/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn cfg(
    host: &str,
    port: u16,
    encrypt: bool,
    trust: bool,
    opts: &[(&str, &str)],
) -> ConnectionConfig {
    ConnectionConfig {
        driver: "cassandra".into(),
        host: host.into(),
        port,
        username: Some("cassandra".into()),
        password: Some("cassandra".into()),
        encrypt,
        trust_server_certificate: trust,
        options: opts
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    }
}

async fn connect(c: &ConnectionConfig) -> Result<(), String> {
    let d = dbine_driver_cassandra::drivers()
        .into_iter()
        .find(|d| d.info().id == "cassandra")
        .unwrap();
    match d.connect(c, None).await {
        Ok(mut s) => {
            let mut out = Default::default();
            s.execute("SELECT release_version FROM system.local", 10, &mut out)
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        }
        Err(Error::Connect(m)) => Err(m),
        Err(e) => Err(e.to_string()),
    }
}

async fn ok(what: &str, c: ConnectionConfig) {
    match connect(&c).await {
        Ok(()) => println!("PASS {what}: connected"),
        Err(e) => panic!("{what}: should connect: {e}"),
    }
}

async fn fails(what: &str, c: ConnectionConfig, needle: &str) {
    match connect(&c).await {
        Ok(()) => panic!("{what}: should have failed"),
        Err(e) => {
            println!("PASS {what}: refused: {e}");
            assert!(e.contains(needle), "{what}: {e} doesn't mention {needle}");
        }
    }
}

#[tokio::test]
#[ignore]
async fn tls_options() {
    let Ok(url) = std::env::var("DBINE_TEST_TLS_CASSANDRA") else {
        return;
    };
    let (host, port) = url.rsplit_once(':').unwrap();
    let port: u16 = port.parse().unwrap();
    let ca = tls_file("ca.pem");
    let wrong_ca = tls_file("selfsigned.pem");
    let c = |e, t, o: &[(&str, &str)]| cfg(host, port, e, t, o);

    // Saved connections (no native options): [ff, ft] plain, tt any
    // certificate, tf verified against the system roots (no test CA there).
    ok("legacy ff (plaintext)", c(false, false, &[])).await;
    ok("legacy ft (plaintext)", c(false, true, &[])).await;
    ok("legacy tt (TLS, not verified)", c(true, true, &[])).await;
    fails(
        "legacy tf (verified, system roots)",
        c(true, false, &[]),
        "certfile",
    )
    .await;

    // Native options.
    ok("ssl=false", c(false, false, &[("ssl", "false")])).await;
    ok(
        "ssl=true + certfile=ca.pem",
        c(
            false,
            false,
            &[("ssl", "true"), ("validate", "true"), ("certfile", &ca)],
        ),
    )
    .await;
    fails(
        "ssl=true without certfile",
        c(false, false, &[("ssl", "true"), ("validate", "true")]),
        "certfile",
    )
    .await;
    fails(
        "ssl=true + certfile=wrong CA",
        c(false, false, &[("ssl", "true"), ("certfile", &wrong_ca)]),
        "certfile",
    )
    .await;
    ok(
        "ssl=true + validate=false (untrusted certificate)",
        c(false, false, &[("ssl", "true"), ("validate", "false")]),
    )
    .await;
    fails(
        "certfile with a bad path",
        c(
            false,
            false,
            &[("ssl", "true"), ("certfile", "/nonexistent/ca.pem")],
        ),
        "certfile",
    )
    .await;

    if let Ok(ip) = std::env::var("DBINE_TEST_TLS_OTHER_IP") {
        let o = |opts: &[(&str, &str)]| cfg(&ip, port, false, false, opts);
        ok(
            "ssl=false through the other address",
            o(&[("ssl", "false")]),
        )
        .await;
        ok(
            "ssl=true + validate=false through the other address",
            o(&[("ssl", "true"), ("validate", "false")]),
        )
        .await;
        fails(
            "ssl=true + certfile, address not in the certificate",
            o(&[("ssl", "true"), ("certfile", &ca)]),
            "certfile",
        )
        .await;
        // As through an SSH tunnel: dial an address the certificate doesn't
        // name, check it against the real server's name.
        ok(
            "ssl=true + certfile, checked against the tunnel's origin name",
            o(&[
                ("ssl", "true"),
                ("certfile", &ca),
                (TUNNEL_ORIGIN_HOST, "localhost"),
            ]),
        )
        .await;
        fails(
            "ssl=true + certfile, a tunnel origin not in the certificate",
            o(&[
                ("ssl", "true"),
                ("certfile", &ca),
                (TUNNEL_ORIGIN_HOST, "db.example.com"),
            ]),
            "certfile",
        )
        .await;
    }
}
