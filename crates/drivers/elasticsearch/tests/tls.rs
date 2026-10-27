//! TLS against real servers: the throwaway `dbine-test-tls-*` ones
//! (`scripts/tls-test-servers.sh up ENGINES`, tests/tls/README.md: CA-signed
//! certificate for `localhost` / `127.0.0.1`) and `dbine-test-opensearch` (plain HTTP on 25521). Skipped without the
//! variable:
//!
//! ```sh
//! DBINE_TEST_TLS_DIR="$PWD/tests/tls" cargo test -p dbine-driver-elasticsearch --test tls -- --ignored --nocapture
//! ```
//!
//! Every case connects with the driver's own options: a new connection
//! starts from the fields' defaults (TLS on, verified), a saved one from
//! before them has only `encrypt` / `trust_server_certificate`. A wrong
//! host name is checked through the SSH tunnel path: the driver dials
//! 127.0.0.1 and verifies the name in `tunnel.origin_host`.

// Not every engine uses every kind of case.
#![allow(dead_code)]

use dbine_driver::{ConnectFailureKind, ConnectionConfig, Driver, Error, TUNNEL_ORIGIN_HOST};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Expect {
    Ok,
    /// Refused: the certificate isn't trusted (wrong CA, wrong name).
    Untrusted,
    /// Refused: TLS asked for and the server speaks plain HTTP.
    NotOffered,
    /// Any other connect failure.
    Refused,
}

struct Case {
    what: &'static str,
    host: &'static str,
    port: u16,
    /// `Some((encrypt, trust))`: a connection saved before the TLS options.
    legacy: Option<(bool, bool)>,
    /// Over the fields' defaults; `@ca` is the test CA, `@ss` a
    /// self-signed certificate that didn't sign the server's.
    opts: &'static [(&'static str, &'static str)],
    /// Dial 127.0.0.1 as an SSH tunnel would and verify this name.
    origin: Option<&'static str>,
    expect: Expect,
}

fn dir() -> Option<String> {
    std::env::var("DBINE_TEST_TLS_DIR").ok()
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_elasticsearch::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

fn config(d: &dyn Driver, case: &Case, dir: &str) -> ConnectionConfig {
    let mut c = ConnectionConfig {
        driver: d.info().id.into(),
        host: case.host.into(),
        port: case.port,
        username: Some("elastic".into()),
        password: Some("Dbine-Tls-2026".into()),
        ..Default::default()
    };
    if case.port == 25521 {
        c.username = None;
        c.password = None;
    }
    match case.legacy {
        Some((encrypt, trust)) => {
            c.encrypt = encrypt;
            c.trust_server_certificate = trust;
        }
        None => {
            for f in d.info().fields.iter().filter(|f| f.tls.is_some()) {
                if !f.default.is_empty() {
                    c.options.insert(f.key.into(), f.default.into());
                }
            }
        }
    }
    for (k, v) in case.opts {
        let v = v
            .replace("@ca", &format!("{dir}/ca.pem"))
            .replace("@ss", &format!("{dir}/selfsigned.pem"));
        c.options.insert((*k).into(), v);
    }
    if let Some(name) = case.origin {
        c.host = "127.0.0.1".into();
        c.options.insert(TUNNEL_ORIGIN_HOST.into(), name.into());
    }
    c
}

async fn run(id: &str, cases: &[Case]) {
    let Some(dir) = dir() else { return };
    let d = driver(id);
    let mut failed = vec![];
    for case in cases {
        let cfg = config(d.as_ref(), case, &dir);
        let got = match d.connect(&cfg, None).await {
            Result::Ok(_) => Expect::Ok,
            Err(e) => match e.connect_failure().map(|f| f.kind) {
                Some(ConnectFailureKind::TlsUntrusted) => Expect::Untrusted,
                Some(ConnectFailureKind::TlsNotOffered) => Expect::NotOffered,
                _ if matches!(e, Error::Connect(_) | Error::ConnectFailed(_)) => {
                    println!("    ({}: {e})", case.what);
                    Expect::Refused
                }
                _ => {
                    println!("    ({}: {e:?})", case.what);
                    Expect::Refused
                }
            },
        };
        let mark = if got == case.expect { "ok  " } else { "FAIL" };
        println!("{mark} {id}: {}: {got:?}", case.what);
        if got != case.expect {
            failed.push(format!(
                "{}: expected {:?}, got {got:?}",
                case.what, case.expect
            ));
        }
    }
    assert!(failed.is_empty(), "{id}:\n{}", failed.join("\n"));
}

const fn case(
    what: &'static str,
    host: &'static str,
    port: u16,
    opts: &'static [(&'static str, &'static str)],
    expect: Expect,
) -> Case {
    Case {
        what,
        host,
        port,
        legacy: None,
        opts,
        origin: None,
        expect,
    }
}

const fn legacy(
    what: &'static str,
    host: &'static str,
    port: u16,
    encrypt: bool,
    trust: bool,
    expect: Expect,
) -> Case {
    Case {
        what,
        host,
        port,
        legacy: Some((encrypt, trust)),
        opts: &[],
        origin: None,
        expect,
    }
}

const fn tunnel(
    what: &'static str,
    port: u16,
    opts: &'static [(&'static str, &'static str)],
    origin: &'static str,
    expect: Expect,
) -> Case {
    Case {
        what,
        host: "127.0.0.1",
        port,
        legacy: None,
        opts,
        origin: Some(origin),
        expect,
    }
}

use Expect::*;

#[tokio::test]
#[ignore]
async fn elasticsearch_tls_options() {
    run(
        "elasticsearch",
        &[
            case(
                "https + certificate_authorities",
                "localhost",
                26070,
                &[("certificate_authorities", "@ca")],
                Ok,
            ),
            case(
                "https, system roots only",
                "localhost",
                26070,
                &[],
                Untrusted,
            ),
            case(
                "https + another CA",
                "localhost",
                26070,
                &[("certificate_authorities", "@ss")],
                Untrusted,
            ),
            case(
                "verification_mode=none",
                "localhost",
                26070,
                &[("verification_mode", "none")],
                Ok,
            ),
            tunnel(
                "tunnel, real name in the SAN",
                26070,
                &[("certificate_authorities", "@ca")],
                "localhost",
                Ok,
            ),
            tunnel(
                "tunnel, wrong name",
                26070,
                &[("certificate_authorities", "@ca")],
                "wrong.example",
                Untrusted,
            ),
            tunnel(
                "verification_mode=certificate, wrong name",
                26070,
                &[
                    ("verification_mode", "certificate"),
                    ("certificate_authorities", "@ca"),
                ],
                "wrong.example",
                Ok,
            ),
            case(
                "verification_mode=certificate without a CA",
                "localhost",
                26070,
                &[("verification_mode", "certificate")],
                Untrusted,
            ),
            case(
                "URL in the host + CA",
                "https://localhost:26070",
                0,
                &[("protocol", "http"), ("certificate_authorities", "@ca")],
                Ok,
            ),
            case(
                "https on the plain port",
                "localhost",
                25521,
                &[],
                NotOffered,
            ),
            case(
                "protocol=http",
                "localhost",
                25521,
                &[("protocol", "http")],
                Ok,
            ),
            legacy("legacy ff (http)", "localhost", 25521, false, false, Ok),
            legacy(
                "legacy tf (https verified)",
                "localhost",
                26070,
                true,
                false,
                Untrusted,
            ),
            legacy(
                "legacy tt (https, not verified)",
                "localhost",
                26070,
                true,
                true,
                Ok,
            ),
            legacy(
                "legacy ft, https URL in the host (not verified)",
                "https://localhost:26070",
                0,
                false,
                true,
                Ok,
            ),
        ],
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn opensearch_tls_options() {
    run(
        "opensearch",
        &[
            case(
                "https + certificate_authorities",
                "localhost",
                26070,
                &[("certificate_authorities", "@ca")],
                Ok,
            ),
            case(
                "https, system roots only",
                "localhost",
                26070,
                &[],
                Untrusted,
            ),
            case(
                "https + another CA",
                "localhost",
                26070,
                &[("certificate_authorities", "@ss")],
                Untrusted,
            ),
            case(
                "verification_mode=none",
                "localhost",
                26070,
                &[("verification_mode", "none")],
                Ok,
            ),
            tunnel(
                "tunnel, real name in the SAN",
                26070,
                &[("certificate_authorities", "@ca")],
                "localhost",
                Ok,
            ),
            tunnel(
                "tunnel, wrong name",
                26070,
                &[("certificate_authorities", "@ca")],
                "wrong.example",
                Untrusted,
            ),
            tunnel(
                "verification_mode=certificate, wrong name",
                26070,
                &[
                    ("verification_mode", "certificate"),
                    ("certificate_authorities", "@ca"),
                ],
                "wrong.example",
                Ok,
            ),
            case(
                "verification_mode=certificate without a CA",
                "localhost",
                26070,
                &[("verification_mode", "certificate")],
                Untrusted,
            ),
            case(
                "URL in the host + CA",
                "https://localhost:26070",
                0,
                &[("protocol", "http"), ("certificate_authorities", "@ca")],
                Ok,
            ),
            case(
                "https on the plain port",
                "localhost",
                25521,
                &[],
                NotOffered,
            ),
            case(
                "protocol=http",
                "localhost",
                25521,
                &[("protocol", "http")],
                Ok,
            ),
            legacy("legacy ff (http)", "localhost", 25521, false, false, Ok),
            legacy(
                "legacy tf (https verified)",
                "localhost",
                26070,
                true,
                false,
                Untrusted,
            ),
            legacy(
                "legacy tt (https, not verified)",
                "localhost",
                26070,
                true,
                true,
                Ok,
            ),
            legacy(
                "legacy ft, https URL in the host (not verified)",
                "https://localhost:26070",
                0,
                false,
                true,
                Ok,
            ),
        ],
    )
    .await;
}
