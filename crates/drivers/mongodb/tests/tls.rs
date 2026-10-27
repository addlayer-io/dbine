//! The TLS options (`tls`, `tlsAllowInvalidCertificates`, `tlsCAFile`)
//! against the TLS test servers (`scripts/tls-test-servers.sh`,
//! tests/tls/README.md), skipped without `DBINE_TEST_TLS=1`:
//!
//! ```sh
//! DBINE_TEST_TLS=1 \
//!   cargo test -p dbine-driver-mongodb --test tls -- --ignored --test-threads 1
//! ```
//!
//! `dbine-test-tls-mongodb` on 26020 (test CA, `preferTLS`: plain works
//! too), `dbine-test-tls-ss-mongodb` on 26520 (self-signed); user `root`,
//! password `Dbine-Tls-2026`. The plain `dbine-test-mongodb` on 25201
//! (`root` / `secret`) adds the plain check. The host-name check dials
//! `0.0.0.0` (see [`other_host`]).

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

fn cfg(driver: &str, host: &str, port: u16, options: &[(&str, &str)]) -> ConnectionConfig {
    let mut c = ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port,
        username: Some("root".into()),
        password: Some("Dbine-Tls-2026".into()),
        options: options
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    };
    c.options.insert("auth_source".into(), "admin".into());
    c
}

async fn connect(c: &ConnectionConfig) -> dbine_driver::Result<()> {
    let d = dbine_driver_mongodb::drivers()
        .into_iter()
        .find(|d| d.info().id == c.driver)
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
    let verified = [("tls", "true"), ("tlsCAFile", ca.as_str())];
    // Verified with the test CA.
    connect(&cfg("mongodb", "localhost", 26020, &verified))
        .await
        .unwrap();
    connect(&cfg("ferretdb", "127.0.0.1", 26020, &verified))
        .await
        .unwrap();
    // DocumentDB keeps its CA under `ca_file`.
    connect(&cfg(
        "documentdb",
        "localhost",
        26020,
        &[("tls", "true"), ("ca_file", ca.as_str())],
    ))
    .await
    .unwrap();
    // Without it (public roots), or with another CA: refused.
    let m = untrusted(connect(&cfg("mongodb", "localhost", 26020, &[("tls", "true")])).await);
    assert!(m.contains("tlsCAFile"), "{m}");
    untrusted(
        connect(&cfg(
            "mongodb",
            "localhost",
            26020,
            &[("tls", "true"), ("tlsCAFile", wrong.as_str())],
        ))
        .await,
    );
    // A name the certificate doesn't have: refused.
    {
        let other = other_host();
        untrusted(connect(&cfg("mongodb", &other, 26020, &verified)).await);
        let mut t = cfg("mongodb", &other, 26020, &verified);
        t.options
            .insert(dbine_driver::TUNNEL_ORIGIN_HOST.into(), "localhost".into());
        let m = untrusted(connect(&t).await);
        assert!(
            m.contains("127.0.0.1") && m.contains("tlsAllowInvalidCertificates"),
            "{m}"
        );
    }
    // Self-signed: verification fails, the skip option connects.
    untrusted(connect(&cfg("mongodb", "localhost", 26520, &verified)).await);
    connect(&cfg(
        "mongodb",
        "localhost",
        26520,
        &[("tls", "true"), ("tlsAllowInvalidCertificates", "true")],
    ))
    .await
    .unwrap();
    // Legacy tt skips; tf verifies.
    let mut tt = cfg("mongodb", "localhost", 26520, &[]);
    tt.encrypt = true;
    tt.trust_server_certificate = true;
    connect(&tt).await.unwrap();
    let mut tf = cfg("mongodb", "localhost", 26520, &[]);
    tf.encrypt = true;
    untrusted(connect(&tf).await);
    // Plain (`preferTLS` takes both): `tls=false` and the legacy ff.
    connect(&cfg("mongodb", "localhost", 26020, &[("tls", "false")]))
        .await
        .unwrap();
    connect(&cfg("mongodb", "localhost", 26020, &[]))
        .await
        .unwrap();
    // The plain container.
    let mut plain = cfg("mongodb", "localhost", 25201, &[("tls", "false")]);
    plain.password = Some("secret".into());
    match connect(&plain).await {
        Ok(()) => {}
        Err(e) => eprintln!("dbine-test-mongodb (25201) not reachable: {e}"),
    }
    // TLS against a plain-only server fails.
    let mut tls_plain = plain.clone();
    tls_plain.options.insert("tls".into(), "true".into());
    tls_plain.options.insert("tlsCAFile".into(), ca.clone());
    match connect(&tls_plain).await {
        Err(e) => eprintln!("TLS against plain: {e}"),
        Ok(()) => panic!("TLS against a plain server connected"),
    }
}
