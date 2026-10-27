//! TLS against real servers: the throwaway `dbine-test-tls-*` ones
//! (`scripts/tls-test-servers.sh up ENGINES`, tests/tls/README.md: CA-signed
//! certificate for `localhost` / `127.0.0.1`). Skipped without the
//! variable:
//!
//! ```sh
//! DBINE_TEST_TLS_DIR="$PWD/tests/tls" cargo test -p dbine-driver-couchdb --test tls -- --ignored --nocapture
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
    dbine_driver_couchdb::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

fn config(d: &dyn Driver, case: &Case, dir: &str) -> ConnectionConfig {
    let mut c = ConnectionConfig {
        driver: d.info().id.into(),
        host: case.host.into(),
        port: case.port,
        username: Some("admin".into()),
        password: Some("Dbine-Tls-2026".into()),
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
async fn couchdb_tls_options() {
    run(
        "couchdb",
        &[
            case(
                "https + cacert",
                "localhost",
                26080,
                &[("cacert", "@ca")],
                Ok,
            ),
            case(
                "https, system roots only",
                "localhost",
                26080,
                &[],
                Untrusted,
            ),
            case(
                "https + another CA",
                "localhost",
                26080,
                &[("cacert", "@ss")],
                Untrusted,
            ),
            case("insecure", "localhost", 26080, &[("insecure", "true")], Ok),
            tunnel(
                "tunnel, real name in the SAN",
                26080,
                &[("cacert", "@ca")],
                "localhost",
                Ok,
            ),
            tunnel(
                "tunnel, wrong name",
                26080,
                &[("cacert", "@ca")],
                "wrong.example",
                Untrusted,
            ),
            case(
                "URL in the host + cacert",
                "https://localhost:26080",
                0,
                &[("scheme", "http"), ("cacert", "@ca")],
                Ok,
            ),
            // CouchDB doesn't answer a TLS hello on its HTTP port: the request times out.
            case(
                "https on the plain port",
                "localhost",
                26081,
                &[("cacert", "@ca")],
                Refused,
            ),
            case("scheme=http", "localhost", 26081, &[("scheme", "http")], Ok),
            legacy("legacy ff (http)", "localhost", 26081, false, false, Ok),
            legacy("legacy ft (http)", "localhost", 26081, false, true, Ok),
            legacy(
                "legacy tf (https verified)",
                "localhost",
                26080,
                true,
                false,
                Untrusted,
            ),
            legacy(
                "legacy tt (https, not verified)",
                "localhost",
                26080,
                true,
                true,
                Ok,
            ),
            legacy(
                "legacy ft, https URL in the host (not verified)",
                "https://localhost:26080",
                0,
                false,
                true,
                Ok,
            ),
        ],
    )
    .await;
}

/// `insecure` pins the certificate on first use: the session reports it,
/// the same certificate passes with the pin, and another one fails as
/// `TlsPinChanged` (the self-signed certificate's fingerprint as the pin
/// stands for a server whose certificate changed).
#[tokio::test]
#[ignore]
async fn couchdb_insecure_pins_the_certificate() {
    let Some(dir) = dir() else { return };
    let fp = |f: &str| {
        let c = dbine_tls::read_ca_file(&format!("{dir}/{f}"), f).unwrap();
        dbine_tls::fingerprint(&c[0])
    };
    let (server, other) = (fp("server.pem"), fp("selfsigned.pem"));
    let d = driver("couchdb");
    let base = config(
        d.as_ref(),
        &case("insecure", "localhost", 26080, &[("insecure", "true")], Ok),
        &dir,
    );
    // First use: accepted and reported.
    let s = d.connect(&base, None).await.unwrap();
    assert_eq!(s.tls_pin_seen().as_deref(), Some(server.as_str()));
    // Pinned to it: accepted, nothing new to save.
    let mut pinned = base.clone();
    pinned
        .options
        .insert(dbine_driver::TLS_PIN.into(), server.clone());
    let s = d.connect(&pinned, None).await.unwrap();
    assert_eq!(s.tls_pin_seen(), None);
    // Pinned to another certificate: refused, naming both.
    pinned.options.insert(dbine_driver::TLS_PIN.into(), other);
    let e = d.connect(&pinned, None).await.err().unwrap();
    let f = e.connect_failure().unwrap();
    assert_eq!(f.kind, ConnectFailureKind::TlsPinChanged, "{e}");
    assert!(
        f.message.contains(&dbine_tls::display_fingerprint(&server)),
        "{e}"
    );
    // A verified mode never pins.
    let ca = config(
        d.as_ref(),
        &case("cacert", "localhost", 26080, &[("cacert", "@ca")], Ok),
        &dir,
    );
    let s = d.connect(&ca, None).await.unwrap();
    assert_eq!(s.tls_pin_seen(), None);
}
