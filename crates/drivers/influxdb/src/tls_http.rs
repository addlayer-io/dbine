//! The engine's TLS options on a reqwest client: the scheme the mode asks
//! for, the skip options, the CA file and, through an SSH tunnel, the real
//! server's name for the certificate check. Shared by the HTTP drivers of
//! TLS batch 5 (InfluxDB, IoTDB, ksqlDB, TDengine, Phoenix, Cosmos DB,
//! libSQL) with `#[path]`. Not every driver uses every function.
#![allow(dead_code)]

use dbine_driver::{
    ConnectFailure, ConnectFailureKind, ConnectionConfig, Error, Field, Result, TlsLevel, TlsRole,
    TUNNEL_ORIGIN_HOST,
};
use std::net::{IpAddr, SocketAddr};

/// `https` unless the TLS mode is off; a value that isn't one of the mode's
/// own is `https` too. No mode field (or no value): the legacy `encrypt`.
pub fn https(cfg: &ConnectionConfig, fields: &[Field]) -> bool {
    let mode = fields
        .iter()
        .find(|f| matches!(f.tls, Some(TlsRole::Mode { .. })));
    match mode.and_then(|f| Some((f, cfg.tls_value(f)?))) {
        Some((f, v)) => f.tls_level_of(v).is_none_or(|l| l > TlsLevel::Off),
        None => cfg.encrypt,
    }
}

/// The certificate checks a connection turns off: `(chain, host name)`.
/// A skip option counts whatever the mode says (a saved connection with
/// `trust_server_certificate` and no `encrypt` skipped the checks of an
/// `https://` URL typed in the host), and so does a verification select
/// below the full check (Avatica `hostname_verification=NONE`).
pub fn skips(cfg: &ConnectionConfig, fields: &[Field]) -> (bool, bool) {
    let (mut chain, mut host) = (false, false);
    for f in fields {
        let Some(v) = cfg.tls_value(f) else {
            continue;
        };
        match &f.tls {
            Some(TlsRole::SkipVerify { .. }) if f.tls_skips(v) => chain = true,
            Some(TlsRole::SkipHostname { .. }) if f.tls_skips(v) => host = true,
            Some(TlsRole::Verify { .. }) => match f.tls_level_of(v) {
                Some(TlsLevel::VerifyFull) => {}
                Some(TlsLevel::VerifyCa) => host = true,
                Some(_) => chain = true,
                // A value that isn't one of its own checks everything.
                None => {}
            },
            _ => {}
        }
    }
    (chain, host)
}

/// The CA certificates of the connection's CA file option, if it has one.
pub fn ca_certs(cfg: &ConnectionConfig, fields: &[Field]) -> Result<Vec<reqwest::Certificate>> {
    let Some(path) = fields
        .iter()
        .filter(|f| matches!(f.tls, Some(TlsRole::CaFile { .. })))
        .find_map(|f| cfg.tls_value(f))
    else {
        return Ok(vec![]);
    };
    let pem = std::fs::read(path).map_err(|e| {
        Error::Connect(format!(
            "no se pudo leer el certificado de CA «{path}»: {e}"
        ))
    })?;
    let invalid = |e: String| {
        Error::Connect(format!(
            "el certificado de CA «{path}» no es un PEM válido: {e}"
        ))
    };
    match reqwest::Certificate::from_pem_bundle(&pem) {
        Ok(certs) if !certs.is_empty() => Ok(certs),
        Ok(_) => Err(invalid("empty PEM bundle".into())),
        Err(e) => Err(invalid(e.to_string())),
    }
}

/// Through an SSH tunnel an `https` URL names the tunnel's end (127.0.0.1):
/// the URL gets the real server's name back, so the certificate is checked
/// against it, and the client resolves that name to the tunnel. Otherwise
/// (no tunnel, plain `http`, which checks nothing, or a server known by its
/// IP address) the URL as it is.
pub fn tunnel(
    cfg: &ConnectionConfig,
    url: String,
    b: reqwest::ClientBuilder,
) -> (String, reqwest::ClientBuilder) {
    let Some(origin) = cfg
        .option(TUNNEL_ORIGIN_HOST)
        .map(str::trim)
        .filter(|h| !h.is_empty())
    else {
        return (url, b);
    };
    // A name is needed to resolve; an IP address stays as dialed.
    if tunnelled_ip(cfg).is_some() {
        return (url, b);
    }
    let Ok(mut u) = reqwest::Url::parse(&url) else {
        return (url, b);
    };
    if u.scheme() != "https" {
        return (url, b);
    }
    let Some(local) = u
        .host_str()
        .and_then(|h| h.trim_matches(['[', ']']).parse::<IpAddr>().ok())
    else {
        return (url, b);
    };
    let port = u.port_or_known_default();
    if u.set_host(Some(origin)).is_err() {
        return (url, b);
    }
    let _ = u.set_port(port);
    let mut out = u.to_string();
    if !url.ends_with('/') && out.ends_with('/') {
        out.pop();
    }
    (out, b.resolve(origin, SocketAddr::new(local, 0)))
}

/// A request that got no answer, as a connect error: with the whole chain of
/// reasons (reqwest's own text stops at "error sending request"), classified,
/// so a certificate the client doesn't trust or a server without TLS reads
/// as such. `message` words it from that text.
pub fn connect_error(e: &reqwest::Error, message: impl FnOnce(String) -> String) -> Error {
    let mut text = e.to_string();
    let mut cur = std::error::Error::source(e);
    while let Some(x) = cur {
        let s = x.to_string();
        if !text.contains(&s) {
            text.push_str(": ");
            text.push_str(&s);
        }
        cur = x.source();
    }
    let mut f = ConnectFailure::from_error(e);
    // rustls reading a plain HTTP answer: the server has no TLS there.
    if text.contains("InvalidContentType") {
        f.kind = ConnectFailureKind::TlsNotOffered;
    }
    f.message = message(text);
    Error::connect_failed(f)
}

/// The client and the base URL for a connection: the skip options, the CA
/// file and the tunnel's server name applied to `b`.
pub fn client(
    cfg: &ConnectionConfig,
    fields: &[Field],
    base: String,
    b: reqwest::ClientBuilder,
) -> Result<(String, reqwest::Client)> {
    let (chain, host) = skips(cfg, fields);
    let mut b = b
        .tls_danger_accept_invalid_certs(chain)
        .tls_danger_accept_invalid_hostnames(host);
    for c in ca_certs(cfg, fields)? {
        b = b.add_root_certificate(c);
    }
    let (base, b) = tunnel(cfg, base, b);
    check_tunnel(cfg, &base, chain || host)?;
    Ok((base, b.build().map_err(Error::connect)?))
}

/// Through a tunnel to a server known by its IP address the client can only
/// check the tunnel's 127.0.0.1: refused instead of checking the wrong name,
/// unless the name check is off (`unchecked`) or the URL is plain `http`.
pub fn check_tunnel(cfg: &ConnectionConfig, url: &str, unchecked: bool) -> Result<()> {
    match tunnelled_ip(cfg) {
        Some(ip) if url.starts_with("https://") && !unchecked => {
            Err(Error::connect_failed(ConnectFailure::new(
                ConnectFailureKind::TlsUntrusted,
                format!(
                    "por un túnel SSH el certificado se verifica contra el nombre del servidor, y {ip} es una dirección IP: usá el nombre de host que figura en el certificado"
                ),
            )))
        }
        _ => Ok(()),
    }
}

/// The real server's address when the connection goes through an SSH tunnel
/// and it's an IP address, not a name.
fn tunnelled_ip(cfg: &ConnectionConfig) -> Option<IpAddr> {
    cfg.option(TUNNEL_ORIGIN_HOST)?
        .trim()
        .trim_matches(['[', ']'])
        .parse()
        .ok()
}

/// For the drivers' tests: what a connection gets from the TLS fields,
/// `(https, skips the chain, skips the host name)`.
#[cfg(test)]
pub fn effect(cfg: &ConnectionConfig, fields: &[Field]) -> (bool, bool, bool) {
    let (chain, host) = skips(cfg, fields);
    (https(cfg, fields), chain, host)
}

/// For the drivers' tests: a new connection, with every TLS field at the
/// form's default.
#[cfg(test)]
pub fn new_connection(fields: &[Field]) -> ConnectionConfig {
    let mut cfg = ConnectionConfig::default();
    for f in fields
        .iter()
        .filter(|f| f.tls.is_some() && !f.default.is_empty())
    {
        cfg.options.insert(f.key.into(), f.default.into());
    }
    cfg
}

/// For the drivers' tests: a saved connection from before the TLS fields
/// behaves as it did (`https` = `encrypt`, the checks skipped =
/// `trust_server_certificate`), and a new one is `https`, verified.
#[cfg(test)]
pub fn assert_legacy_and_defaults(fields: &[Field]) {
    for (encrypt, trust) in [(false, false), (false, true), (true, false), (true, true)] {
        let cfg = ConnectionConfig {
            encrypt,
            trust_server_certificate: trust,
            ..Default::default()
        };
        assert_eq!(
            effect(&cfg, fields),
            (encrypt, trust, false),
            "{encrypt} {trust}"
        );
    }
    let cfg = new_connection(fields);
    assert_eq!(effect(&cfg, fields), (true, false, false));
    assert_eq!(cfg.tls_level(fields), Some(TlsLevel::VerifyFull));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(origin: Option<&str>) -> ConnectionConfig {
        let mut c = ConnectionConfig {
            host: "127.0.0.1".into(),
            ..Default::default()
        };
        if let Some(o) = origin {
            c.options.insert(TUNNEL_ORIGIN_HOST.into(), o.into());
        }
        c
    }

    #[test]
    fn a_tunnel_checks_the_real_servers_name() {
        let b = reqwest::Client::builder;
        let (u, _) = tunnel(&cfg(None), "https://127.0.0.1:40000".into(), b());
        assert_eq!(u, "https://127.0.0.1:40000");
        let (u, _) = tunnel(
            &cfg(Some("db.example.com")),
            "https://127.0.0.1:40000".into(),
            b(),
        );
        assert_eq!(u, "https://db.example.com:40000");
        let (u, _) = tunnel(
            &cfg(Some("db.example.com")),
            "https://127.0.0.1:40000/a/".into(),
            b(),
        );
        assert_eq!(u, "https://db.example.com:40000/a/");
        // An IP address can't be resolved to the tunnel: kept as dialed.
        let (u, _) = tunnel(
            &cfg(Some("10.0.0.5")),
            "https://127.0.0.1:40000".into(),
            b(),
        );
        assert_eq!(u, "https://127.0.0.1:40000");
        // Plain HTTP checks no name: it keeps dialing the tunnel.
        let (u, _) = tunnel(
            &cfg(Some("db.example.com")),
            "http://127.0.0.1:40000".into(),
            b(),
        );
        assert_eq!(u, "http://127.0.0.1:40000");
    }

    #[test]
    fn a_tunnel_to_an_ip_address_refuses_to_check_the_wrong_name() {
        let fields = [Field::tls_insecure(["false", "true", "false", "true"])];
        let mut c = cfg(Some("10.0.0.5"));
        let r = client(
            &c,
            &fields,
            "https://127.0.0.1:40000".into(),
            reqwest::Client::builder(),
        );
        assert_eq!(
            r.err().and_then(|e| e.connect_failure()).map(|f| f.kind),
            Some(ConnectFailureKind::TlsUntrusted)
        );
        // Plain HTTP, or the check off: dialed as it is.
        assert!(client(
            &c,
            &fields,
            "http://127.0.0.1:40000".into(),
            reqwest::Client::builder()
        )
        .is_ok());
        c.options.insert("insecure".into(), "true".into());
        assert!(client(
            &c,
            &fields,
            "https://127.0.0.1:40000".into(),
            reqwest::Client::builder()
        )
        .is_ok());
    }

    #[test]
    fn skips_follow_the_fields() {
        let fields = [
            Field::tls_insecure(["false", "true", "false", "true"]),
            Field::tls_skip_hostname("verifyHost", "verifyHost", true, ["true"; 4]),
        ];
        let mut c = cfg(None);
        assert_eq!(skips(&c, &fields), (false, false));
        c.trust_server_certificate = true;
        assert_eq!(skips(&c, &fields), (true, false));
        c.options.insert("insecure".into(), "false".into());
        c.options.insert("verifyHost".into(), "false".into());
        assert_eq!(skips(&c, &fields), (false, true));
    }

    #[test]
    fn a_missing_ca_file_is_an_error() {
        let fields = [Field::tls_cacert()];
        let mut c = cfg(None);
        assert!(ca_certs(&c, &fields).unwrap().is_empty());
        c.options
            .insert("cacert".into(), "/nonexistent/ca.pem".into());
        assert!(ca_certs(&c, &fields).is_err());
    }
}
