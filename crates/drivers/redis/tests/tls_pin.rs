//! `insecure` pins the server's certificate (trust on first use), against
//! the TLS test servers (`scripts/tls-test-servers.sh`, tests/tls/README.md),
//! skipped without `DBINE_TEST_TLS=1`:
//!
//! ```sh
//! DBINE_TEST_TLS=1 \
//!   cargo test -p dbine-driver-redis --test tls_pin -- --ignored
//! ```
//!
//! `dbine-test-tls-redis`: 26030 (test CA's certificate);
//! `dbine-test-tls-ss-redis`: 26530 (self-signed, another certificate).

use dbine_driver::{ConnectFailureKind, ConnectionConfig, Error};

const ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../tests/tls");

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

fn fingerprint(file: &str) -> String {
    let certs = dbine_tls::read_ca_file(&format!("{ROOT}/{file}"), file).unwrap();
    dbine_tls::fingerprint(&certs[0])
}

/// The first connection accepts any certificate and reports it, a pinned
/// one accepts only it (the profiler's own `MONITOR` connection too), and
/// another server's certificate fails as a changed one.
#[tokio::test]
#[ignore]
async fn insecure_pins_the_certificate() {
    if !on() {
        return;
    }
    let d = dbine_driver_redis::drivers()
        .into_iter()
        .find(|d| d.info().id == "redis")
        .unwrap();
    let (server, other) = (fingerprint("server.pem"), fingerprint("selfsigned.pem"));
    let first = [("tls", "true"), ("insecure", "true")];
    let mut s = d
        .connect(&cfg("127.0.0.1", 26030, &first), None)
        .await
        .unwrap();
    assert_eq!(s.tls_pin_seen(), Some(server.clone()));
    s.server_version().await.unwrap();
    let ss = d
        .connect(&cfg("127.0.0.1", 26530, &first), None)
        .await
        .unwrap();
    assert_eq!(ss.tls_pin_seen(), Some(other.clone()));
    // Pinned: the same certificate passes, with nothing new to save.
    let pinned = [
        ("tls", "true"),
        ("insecure", "true"),
        ("tls_pin", server.as_str()),
    ];
    let mut p = d
        .connect(&cfg("localhost", 26030, &pinned), None)
        .await
        .unwrap();
    assert_eq!(p.tls_pin_seen(), None);
    p.server_version().await.unwrap();
    let opts = dbine_driver::ProfilerOptions {
        database: String::new(),
        change_server: false,
    };
    p.profiler_start(&opts)
        .await
        .expect("profiler over a pinned connection");
    p.profiler_poll().await.unwrap();
    p.profiler_stop().await.unwrap();
    // Another certificate: refused, naming both.
    match d.connect(&cfg("127.0.0.1", 26530, &pinned), None).await {
        Err(Error::ConnectFailed(f)) if f.kind == ConnectFailureKind::TlsPinChanged => {
            assert!(f.message.contains(&dbine_tls::display_fingerprint(&other)));
            assert!(f.message.contains(&dbine_tls::display_fingerprint(&server)));
        }
        Err(e) => panic!("expected a changed certificate, got {e:?}"),
        Ok(_) => panic!("connected with another certificate"),
    }
}
