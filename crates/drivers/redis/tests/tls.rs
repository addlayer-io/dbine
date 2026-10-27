//! The TLS options (`tls`, `insecure`, `cacert`) against the TLS test
//! servers (`scripts/tls-test-servers.sh`, tests/tls/README.md), skipped
//! without `DBINE_TEST_TLS=1`:
//!
//! ```sh
//! DBINE_TEST_TLS=1 \
//!   cargo test -p dbine-driver-redis --test tls -- --ignored --test-threads 1
//! ```
//!
//! `dbine-test-tls-redis`: 26030 TLS (test CA), 26031 plain;
//! `dbine-test-tls-ss-redis`: 26530 TLS (self-signed). `DBINE_TEST_REDIS_URL`
//! (the plain `dbine-test-redis`, `redis://localhost:25400`) adds the plain
//! check. The host-name check dials `0.0.0.0` (see [`other_host`]).

use dbine_driver::{ConnectFailureKind, ConnectionConfig, Error};

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../tests/tls");

/// An address of this machine that isn't in the test certificate (its SAN
/// has `localhost` and `127.0.0.1`): `0.0.0.0` reaches the local ports on
/// macOS and Linux. `DBINE_TEST_TLS_OTHER_HOST` overrides it.
fn other_host() -> String {
    std::env::var("DBINE_TEST_TLS_OTHER_HOST").unwrap_or_else(|_| "0.0.0.0".into())
}

fn on() -> bool {
    let on = std::env::var("DBINE_TEST_TLS").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("DBINE_TEST_TLS not set; skipping");
    }
    on
}

fn cfg(host: &str, port: u16, options: &[(&str, &str)]) -> ConnectionConfig {
    ConnectionConfig {
        driver: "redis".into(),
        host: host.into(),
        port,
        password: Some("Dbine-Tls-2026".into()),
        options: options
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    }
}

async fn connect(c: &ConnectionConfig) -> dbine_driver::Result<()> {
    let d = dbine_driver_redis::drivers()
        .into_iter()
        .find(|d| d.info().id == "redis")
        .unwrap();
    d.connect(c, None).await.map(|_| ())
}

fn untrusted(r: dbine_driver::Result<()>) -> String {
    match r {
        Err(Error::ConnectFailed(f)) if f.kind == ConnectFailureKind::TlsUntrusted => f.message,
        other => panic!("expected an untrusted certificate, got {other:?}"),
    }
}

#[tokio::test]
#[ignore]
async fn tls_options() {
    if !on() {
        return;
    }
    let ca = format!("{ROOT}/ca.pem");
    let wrong = format!("{ROOT}/selfsigned.pem");
    let verified = [("tls", "true"), ("cacert", ca.as_str())];
    // Verified with the test CA.
    connect(&cfg("localhost", 26030, &verified)).await.unwrap();
    connect(&cfg("127.0.0.1", 26030, &verified)).await.unwrap();
    // Without it (public roots), or with another CA: refused.
    let m = untrusted(connect(&cfg("127.0.0.1", 26030, &[("tls", "true")])).await);
    assert!(m.contains("cacert"), "{m}");
    untrusted(
        connect(&cfg(
            "127.0.0.1",
            26030,
            &[("tls", "true"), ("cacert", wrong.as_str())],
        ))
        .await,
    );
    // A name the certificate doesn't have: refused.
    {
        let other = other_host();
        untrusted(connect(&cfg(&other, 26030, &verified)).await);
        // Through a tunnel the client checks the dialled address: the
        // message says so.
        let mut t = cfg(&other, 26030, &verified);
        t.options
            .insert(dbine_driver::TUNNEL_ORIGIN_HOST.into(), "localhost".into());
        let m = untrusted(connect(&t).await);
        assert!(m.contains("127.0.0.1") && m.contains("insecure"), "{m}");
    }
    // Self-signed: verification fails, `insecure` connects.
    untrusted(connect(&cfg("127.0.0.1", 26530, &verified)).await);
    connect(&cfg(
        "127.0.0.1",
        26530,
        &[("tls", "true"), ("insecure", "true")],
    ))
    .await
    .unwrap();
    // Legacy tt (encrypt + trust) is `insecure` too; tf verifies.
    let mut tt = cfg("127.0.0.1", 26530, &[]);
    tt.encrypt = true;
    tt.trust_server_certificate = true;
    connect(&tt).await.unwrap();
    let mut tf = cfg("127.0.0.1", 26530, &[]);
    tf.encrypt = true;
    untrusted(connect(&tf).await);
    // Plain: `tls=false` and the legacy ff.
    connect(&cfg("127.0.0.1", 26031, &[("tls", "false")]))
        .await
        .unwrap();
    connect(&cfg("127.0.0.1", 26031, &[])).await.unwrap();
    // TLS against the plain port fails, as a TLS failure.
    match connect(&cfg("127.0.0.1", 26031, &verified)).await {
        Err(e) => eprintln!("TLS against plain: {e}"),
        Ok(()) => panic!("TLS against a plain port connected"),
    }
    if let Ok(url) = std::env::var("DBINE_TEST_REDIS_URL") {
        let rest = url.trim_start_matches("redis://");
        let (host, port) = rest.split_once(':').unwrap_or((rest, "6379"));
        let mut c = cfg(
            host,
            port.trim_end_matches('/').parse().unwrap(),
            &[("tls", "false")],
        );
        c.password = None;
        connect(&c).await.unwrap();
    }
}
