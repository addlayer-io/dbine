//! The TLS options (`PROTOCOL`, `WALLET_LOCATION`). There is no Oracle
//! container with TCPS, so this checks:
//!
//! - plain TCP (`PROTOCOL=TCP`, and a saved connection without the option)
//!   against `dbine-test-oracle`;
//! - `PROTOCOL=TCPS` against that TCP-only listener: the error says to choose
//!   TCP;
//! - the certificate check against a TLS test server that isn't Oracle
//!   (`dbine-test-tls-elasticsearch`, HTTPS only, test CA): with no CA it
//!   fails as untrusted and names `WALLET_LOCATION`; with a wallet holding
//!   the test CA the handshake passes (and then Oracle's protocol fails, as
//!   expected: the peer answers HTTP);
//!   with a name that isn't in the certificate it fails again.
//!
//! ```sh
//! DBINE_TEST_TLS=1 DBINE_TEST_ORACLE_URL=oracle://dbine:Dbine123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test tls -- --ignored --test-threads 1
//! ```

use dbine_driver::{ConnectFailureKind, ConnectionConfig, Error};

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../tests/tls");
/// `dbine-test-tls-elasticsearch`: a TLS server with the test CA's
/// certificate (SAN localhost, 127.0.0.1) that closes on a bad request.
const TLS_PORT: u16 = 26070;

fn on() -> bool {
    let on = std::env::var("DBINE_TEST_TLS").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("DBINE_TEST_TLS not set; skipping");
    }
    on
}

fn plain() -> ConnectionConfig {
    let url = std::env::var("DBINE_TEST_ORACLE_URL").expect("DBINE_TEST_ORACLE_URL");
    let rest = url.strip_prefix("oracle://").unwrap();
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, service) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    cfg
}

fn with(mut cfg: ConnectionConfig, options: &[(&str, &str)]) -> ConnectionConfig {
    for (k, v) in options {
        cfg.options.insert(k.to_string(), v.to_string());
    }
    cfg
}

async fn connect(cfg: &ConnectionConfig) -> Result<(), Error> {
    let driver = dbine_driver_oracle::drivers().remove(0);
    driver.connect(cfg, None).await.map(|_| ())
}

fn failure(e: Error) -> (ConnectFailureKind, String) {
    match e {
        Error::ConnectFailed(f) => (f.kind, f.message),
        other => (ConnectFailureKind::Unknown, other.to_string()),
    }
}

/// A wallet folder whose `ewallet.pem` is `pem` (no key: trusted roots).
fn wallet(name: &str, pem: &str) -> String {
    let dir = std::env::temp_dir().join(format!("dbine-ora-tls-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(format!("{ROOT}/{pem}"), dir.join("ewallet.pem")).unwrap();
    dir.to_string_lossy().into_owned()
}

#[tokio::test]
#[ignore]
async fn tcp_connects_and_tcps_on_a_tcp_listener_says_to_choose_tcp() {
    if !on() {
        return;
    }
    connect(&plain()).await.expect("legacy ff (TCP)");
    connect(&with(plain(), &[("PROTOCOL", "TCP")]))
        .await
        .expect("PROTOCOL=TCP");
    let (kind, msg) = failure(
        connect(&with(plain(), &[("PROTOCOL", "TCPS")]))
            .await
            .unwrap_err(),
    );
    eprintln!("TCPS on the TCP listener: {kind:?}: {msg}");
    assert!(kind.is_tls(), "{kind:?}: {msg}");
    assert!(msg.contains("TCP en PROTOCOL"), "{msg}");
    // Nothing on 2484 (the TCPS default port).
    let mut c = with(plain(), &[("PROTOCOL", "TCPS")]);
    c.host = "127.0.0.1".into();
    c.port = 1;
    let (kind, msg) = failure(connect(&c).await.unwrap_err());
    eprintln!("TCPS, nothing listening: {kind:?}: {msg}");
    assert!(msg.contains("TCP en PROTOCOL"), "{msg}");
}

/// The handshake that passes leaves the client waiting for an Oracle
/// answer on a blocking thread until the connect timeout: the runtime is
/// shut down without waiting for it.
#[test]
#[ignore]
fn tcps_verifies_the_certificate_with_the_wallet_ca() {
    if !on() {
        return;
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(verifies());
    rt.shutdown_background();
}

async fn verifies() {
    let tls = |host: &str, options: &[(&str, &str)]| {
        let mut c = with(plain(), &[("PROTOCOL", "TCPS")]);
        c.host = host.into();
        c.port = TLS_PORT;
        with(c, options)
    };
    // No CA: the test CA isn't a public one.
    let (kind, msg) = failure(connect(&tls("127.0.0.1", &[])).await.unwrap_err());
    eprintln!("no CA: {kind:?}: {msg}");
    assert_eq!(kind, ConnectFailureKind::TlsUntrusted, "{msg}");
    assert!(msg.contains("WALLET_LOCATION"), "{msg}");
    // The wrong CA.
    let wrong = wallet("wrong", "selfsigned.pem");
    let (kind, msg) = failure(
        connect(&tls("127.0.0.1", &[("WALLET_LOCATION", &wrong)]))
            .await
            .unwrap_err(),
    );
    eprintln!("wrong CA: {kind:?}: {msg}");
    assert_eq!(kind, ConnectFailureKind::TlsUntrusted, "{msg}");
    // The right CA: the handshake passes, then the peer isn't Oracle.
    let ca = wallet("ca", "ca.pem");
    for host in ["localhost", "127.0.0.1"] {
        let (kind, msg) = failure(
            connect(&tls(host, &[("WALLET_LOCATION", &ca)]))
                .await
                .unwrap_err(),
        );
        eprintln!("right CA via {host}: {kind:?}: {msg}");
        // Past the handshake: the peer never answers TNS.
        assert_eq!(kind, ConnectFailureKind::Timeout, "{msg}");
        assert!(!msg.to_lowercase().contains("certificate"), "{msg}");
    }
    // The right CA, a name the certificate doesn't have.
    let other = std::env::var("DBINE_TEST_TLS_OTHER_HOST").unwrap_or_else(|_| "0.0.0.0".into());
    let (kind, msg) = failure(
        connect(&tls(&other, &[("WALLET_LOCATION", &ca)]))
            .await
            .unwrap_err(),
    );
    eprintln!("right CA, wrong name {other}: {kind:?}: {msg}");
    assert_eq!(kind, ConnectFailureKind::TlsUntrusted, "{msg}");
    // Through an SSH tunnel the check is against 127.0.0.1: the message says so.
    let mut t = tls("127.0.0.1", &[]);
    t.options.insert(
        dbine_driver::TUNNEL_ORIGIN_HOST.into(),
        "db.example.com".into(),
    );
    let (kind, msg) = failure(connect(&t).await.unwrap_err());
    eprintln!("tunnel: {kind:?}: {msg}");
    assert!(
        msg.contains("túnel SSH") && msg.contains("TCP en PROTOCOL"),
        "{msg}"
    );
    for d in [wrong, ca] {
        let _ = std::fs::remove_dir_all(d);
    }
}
