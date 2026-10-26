//! TLS against real servers: the throwaway `dbine-test-tls-*` ones
//! (`scripts/tls-test-servers.sh up ENGINES`, tests/tls/README.md: CA-signed
//! certificate for `localhost` / `127.0.0.1`). Skipped without the
//! variable:
//!
//! ```sh
//! DBINE_TEST_TLS_DIR="$PWD/tests/tls" cargo test -p dbine-driver-etcd --test tls -- --ignored --nocapture
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
    dbine_driver_etcd::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

fn config(d: &dyn Driver, case: &Case, dir: &str) -> ConnectionConfig {
    let mut c = ConnectionConfig {
        driver: d.info().id.into(),
        host: case.host.into(),
        port: case.port,

        ..Default::default()
    };

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
async fn etcd_tls_options() {
    run(
        "etcd",
        &[
            case("TLS + cacert", "localhost", 26090, &[("cacert", "@ca")], Ok),
            case("TLS, system roots only", "localhost", 26090, &[], Untrusted),
            case(
                "TLS + another CA",
                "localhost",
                26090,
                &[("cacert", "@ss")],
                Untrusted,
            ),
            case(
                "insecure-skip-tls-verify",
                "localhost",
                26090,
                &[("insecure-skip-tls-verify", "true")],
                Ok,
            ),
            tunnel(
                "tunnel, real name in the SAN",
                26090,
                &[("cacert", "@ca")],
                "localhost",
                Ok,
            ),
            tunnel(
                "tunnel, wrong name",
                26090,
                &[("cacert", "@ca")],
                "wrong.example",
                Untrusted,
            ),
            case(
                "TLS on the plain port",
                "localhost",
                26091,
                &[("cacert", "@ca")],
                NotOffered,
            ),
            case(
                "insecure-transport",
                "localhost",
                26091,
                &[("insecure-transport", "true")],
                Ok,
            ),
            legacy("legacy ff (http)", "localhost", 26091, false, false, Ok),
            legacy("legacy ft (http)", "localhost", 26091, false, true, Ok),
            legacy(
                "legacy tf (https verified)",
                "localhost",
                26090,
                true,
                false,
                Untrusted,
            ),
            legacy(
                "legacy tt (https, not verified)",
                "localhost",
                26090,
                true,
                true,
                Ok,
            ),
        ],
    )
    .await;
}
