//! TLS for a reqwest client, from the connection's TLS options: the
//! driver's own fields (a mode, a verification select or skip options, a CA
//! file), read through `ConnectionConfig::tls_value` so a connection saved
//! before them keeps its legacy table. Through an SSH tunnel the URL names
//! the real server (`ConnectionConfig::tls_server_name`) and the client
//! dials the tunnel's end, so the certificate is checked against the real
//! name.
//!
//! The same file is in every reqwest driver crate (a driver owns its folder
//! and the contract crate has no HTTP client); keep the copies in step.

use dbine_driver::{
    ConnectFailure, ConnectFailureKind, ConnectionConfig, Error, Field, Result, TlsLevel, TlsRole,
    TUNNEL_ORIGIN_HOST,
};
use std::net::{IpAddr, SocketAddr};

/// How the HTTP client talks to the server.
#[derive(Debug, Clone)]
pub struct HttpTls {
    /// The effective level: `Off` is plain `http`.
    pub level: TlsLevel,
    /// The CA file option's key and its path (only used when verifying).
    ca: Option<(&'static str, String)>,
    /// Through an SSH tunnel: the real server's name and the tunnel's end.
    tunnel: Option<(String, IpAddr)>,
    /// What to change when the certificate can't be verified, and when the
    /// server doesn't speak TLS: the engine's own options.
    untrusted_hint: String,
    plain_hint: String,
}

fn mode_field(fields: &[Field]) -> Option<&Field> {
    fields
        .iter()
        .find(|f| matches!(f.tls, Some(TlsRole::Mode { .. })))
}

/// The value of a mode or verification field that gives `pick` among its
/// levels (the highest, or the lowest).
fn value_at(f: &Field, highest: bool) -> Option<&'static str> {
    let Some(TlsRole::Mode { levels } | TlsRole::Verify { levels }) = &f.tls else {
        return None;
    };
    let it = levels.iter();
    let pick = if highest {
        it.max_by_key(|(_, l)| *l)
    } else {
        it.min_by_key(|(_, l)| *l)
    };
    pick.map(|(v, _)| *v)
}

impl HttpTls {
    /// The client settings for `cfg` by the driver's TLS `fields`.
    /// `url_https`: the server's address is a URL whose scheme decides
    /// (typed in the host, or a cloud endpoint): `Some(true)` for `https`,
    /// TLS whatever the mode says, with the connection's verification
    /// options; `Some(false)` for `http`.
    pub fn new(cfg: &ConnectionConfig, fields: &[Field], url_https: Option<bool>) -> Result<Self> {
        let forced;
        let cfg = match (url_https, mode_field(fields)) {
            (Some(on), Some(f)) => {
                let mut c = cfg.clone();
                if let Some(v) = value_at(f, on) {
                    c.options.insert(f.key.to_string(), v.to_string());
                }
                forced = c;
                &forced
            }
            _ => cfg,
        };
        let level = cfg.tls_level(fields).ok_or_else(|| {
            let keys: Vec<&str> = fields
                .iter()
                .filter(|f| f.tls.is_some())
                .map(|f| f.key)
                .collect();
            Error::Connect(format!(
                "Una opción de TLS tiene un valor que este driver no conoce: revisá {}.",
                keys.join(", ")
            ))
        })?;
        if level.verified() {
            cfg.require_tls_ca(fields)?;
        }
        let ca = fields
            .iter()
            .find(|f| matches!(f.tls, Some(TlsRole::CaFile { .. })))
            .and_then(|f| Some((f.key, cfg.tls_value(f)?.trim().to_string())))
            .filter(|(_, p)| !p.is_empty());
        let tunnel = cfg
            .option(TUNNEL_ORIGIN_HOST)
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .and_then(|name| {
                let dial = cfg
                    .host
                    .trim()
                    .trim_start_matches('[')
                    .trim_end_matches(']');
                let dial = dial
                    .split_once("://")
                    .map_or(dial, |(_, r)| r.split(['/', ':']).next().unwrap_or(r));
                Some((name.to_string(), dial.parse().ok()?))
            });
        // The engine's own options for the hints.
        let skip = fields
            .iter()
            .find_map(|f| match &f.tls {
                Some(TlsRole::Verify { levels }) => levels
                    .iter()
                    .find(|(_, l)| *l == TlsLevel::Unverified)
                    .map(|(v, _)| format!("{}={v}", f.key)),
                _ => None,
            })
            .or_else(|| {
                fields.iter().find_map(|f| match &f.tls {
                    Some(TlsRole::SkipVerify { inverted }) => {
                        Some(format!("{}={}", f.key, !inverted))
                    }
                    _ => None,
                })
            });
        let ca_key = fields
            .iter()
            .find(|f| matches!(f.tls, Some(TlsRole::CaFile { .. })))
            .map(|f| f.key);
        let untrusted_hint = match (ca_key, skip) {
            (Some(ca), Some(skip)) => format!(
                "El certificado del servidor no se pudo verificar: indicá la CA en {ca} o, si confiás en el servidor, usá {skip}."
            ),
            _ => String::new(),
        };
        let plain_hint = match mode_field(fields).and_then(|f| Some((f.key, value_at(f, false)?)))
        {
            Some((key, off)) => format!(
                "El servidor no respondió con TLS en ese puerto: usá el puerto TLS del servidor o {key}={off}."
            ),
            None => String::new(),
        };
        Ok(Self {
            level,
            ca,
            tunnel,
            untrusted_hint,
            plain_hint,
        })
    }

    /// `https` (else `http`).
    pub fn https(&self) -> bool {
        self.level >= TlsLevel::Unverified
    }

    pub fn scheme(&self) -> &'static str {
        if self.https() {
            "https"
        } else {
            "http"
        }
    }

    /// The host for the URL: the real server's name through a tunnel (the
    /// client dials the tunnel, see [`Self::builder`]), else `host`.
    pub fn url_host<'a>(&'a self, host: &'a str) -> &'a str {
        match &self.tunnel {
            Some((name, _)) if self.https() => name,
            _ => host,
        }
    }

    /// A whole URL with its host replaced like [`Self::url_host`] (for a URL
    /// typed in the host). Unchanged when not tunnelled or not https.
    // Only the drivers that take a URL in the host use it.
    #[allow(dead_code)]
    pub fn url(&self, url: String) -> String {
        let Some((name, ip)) = self.tunnel.as_ref().filter(|_| self.https()) else {
            return url;
        };
        let Ok(mut u) = reqwest::Url::parse(&url) else {
            return url;
        };
        let same = u
            .host_str()
            .map(|h| h.trim_start_matches('[').trim_end_matches(']'))
            .and_then(|h| h.parse::<IpAddr>().ok())
            == Some(*ip);
        if !same || u.set_host(Some(name)).is_err() {
            return url;
        }
        let mut s = u.to_string();
        if !url.ends_with('/') && s.ends_with('/') && u.path() == "/" {
            s.pop();
        }
        s
    }

    /// The client builder with these settings: the CA file as the only
    /// trust anchors (like curl's `--cacert`), the skipped checks, and the
    /// tunnel's address for the real name.
    pub fn builder(&self, mut b: reqwest::ClientBuilder) -> Result<reqwest::ClientBuilder> {
        if !self.https() {
            return Ok(b);
        }
        if self.level.verified() {
            if let Some((key, path)) = &self.ca {
                let pem = std::fs::read(path)
                    .map_err(|e| Error::Connect(format!("No se pudo leer {key} ({path}): {e}")))?;
                let certs = reqwest::Certificate::from_pem_bundle(&pem)
                    .ok()
                    .filter(|c| !c.is_empty())
                    .ok_or_else(|| {
                        Error::Connect(format!("{key} ({path}) no tiene certificados PEM."))
                    })?;
                b = b.tls_certs_only(certs);
            }
        }
        match self.level {
            TlsLevel::Unverified => b = b.danger_accept_invalid_certs(true),
            TlsLevel::VerifyCa => b = b.tls_danger_accept_invalid_hostnames(true),
            _ => {}
        }
        if let Some((name, ip)) = &self.tunnel {
            // The URL's port wins over this one.
            b = b.resolve(name, SocketAddr::new(*ip, 0));
        }
        Ok(b)
    }

    /// A connect error with the engine's options to change when it's a TLS
    /// one (see [`net_error`]).
    pub fn explain(&self, e: Error) -> Error {
        let Error::ConnectFailed(mut f) = e else {
            return e;
        };
        let hint = match f.kind {
            ConnectFailureKind::TlsUntrusted => &self.untrusted_hint,
            ConnectFailureKind::TlsNotOffered => &self.plain_hint,
            _ => return Error::ConnectFailed(f),
        };
        if !hint.is_empty() && !f.message.contains(hint.as_str()) {
            f.message = format!("{} {hint}", f.message);
        }
        Error::ConnectFailed(f)
    }
}

/// A reqwest error that didn't reach the server, classified, with the
/// causes in its text (reqwest's own text only says "error sending
/// request"): an untrusted certificate, a server that doesn't speak TLS…
pub fn net_error(e: &reqwest::Error) -> Error {
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
    let lower = text.to_lowercase();
    if [
        "invalidcontenttype",
        "corrupt message",
        "wrong version number",
        // The server closed the connection on the TLS hello (etcd's HTTP
        // port).
        "tls handshake eof",
    ]
    .iter()
    .any(|w| lower.contains(w))
    {
        f.kind = ConnectFailureKind::TlsNotOffered;
    } else if f.kind == ConnectFailureKind::Unknown || f.kind == ConnectFailureKind::Tls {
        f.kind = dbine_driver::error::classify_message(&text);
    }
    f.message = text;
    Error::connect_failed(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields() -> Vec<Field> {
        vec![
            Field::tls_mode(
                "scheme",
                "scheme",
                &[
                    ("http", "http", TlsLevel::Off),
                    ("https", "https", TlsLevel::VerifyFull),
                ],
                "https",
                ["http", "http", "https", "https"],
            ),
            Field::tls_insecure(["false", "true", "false", "true"]),
            Field::tls_skip_hostname("checkHost", "checkHost", true, ["true"; 4]),
            Field::tls_cacert().ca_required_at(&[TlsLevel::VerifyCa]),
        ]
    }

    fn cfg(opts: &[(&str, &str)]) -> ConnectionConfig {
        ConnectionConfig {
            host: "127.0.0.1".into(),
            port: 40000,
            options: opts
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn the_url_scheme_decides_over_the_mode() {
        let f = fields();
        let on = HttpTls::new(&cfg(&[("scheme", "http")]), &f, Some(true)).unwrap();
        assert_eq!(on.level, TlsLevel::VerifyFull);
        let skip = HttpTls::new(&cfg(&[("insecure", "true")]), &f, Some(true)).unwrap();
        assert_eq!(skip.level, TlsLevel::Unverified);
        let off = HttpTls::new(&cfg(&[("scheme", "https")]), &f, Some(false)).unwrap();
        assert_eq!((off.level, off.scheme()), (TlsLevel::Off, "http"));
    }

    #[test]
    fn a_tunnel_names_the_real_server() {
        let f = fields();
        let t = HttpTls::new(
            &cfg(&[("scheme", "https"), (TUNNEL_ORIGIN_HOST, "db.example.com")]),
            &f,
            None,
        )
        .unwrap();
        assert_eq!(t.url_host("127.0.0.1"), "db.example.com");
        assert_eq!(
            t.url("https://127.0.0.1:40000".into()),
            "https://db.example.com:40000"
        );
        assert_eq!(
            t.url("https://127.0.0.1:40000/couch/".into()),
            "https://db.example.com:40000/couch/"
        );
        assert!(t.builder(reqwest::Client::builder()).is_ok());
        // Plain http keeps dialing the tunnel by its address.
        let p = HttpTls::new(
            &cfg(&[("scheme", "http"), (TUNNEL_ORIGIN_HOST, "db.example.com")]),
            &f,
            None,
        )
        .unwrap();
        assert_eq!(p.url_host("127.0.0.1"), "127.0.0.1");
        // Without a tunnel nothing changes.
        let d = HttpTls::new(&cfg(&[("scheme", "https")]), &f, None).unwrap();
        assert_eq!(d.url_host("db"), "db");
    }

    #[test]
    fn verifying_without_the_host_name_needs_the_ca_and_reads_it() {
        let f = fields();
        let e = HttpTls::new(
            &cfg(&[("scheme", "https"), ("checkHost", "false")]),
            &f,
            None,
        )
        .unwrap_err();
        assert!(e.to_string().contains("cacert"), "{e}");
        let missing = HttpTls::new(
            &cfg(&[
                ("scheme", "https"),
                ("checkHost", "false"),
                ("cacert", "/nonexistent/ca.pem"),
            ]),
            &f,
            None,
        )
        .unwrap();
        assert_eq!(missing.level, TlsLevel::VerifyCa);
        let e = missing.builder(reqwest::Client::builder()).unwrap_err();
        assert!(e.to_string().contains("/nonexistent/ca.pem"), "{e}");
        // Unverified never reads it.
        let skip = HttpTls::new(
            &cfg(&[
                ("scheme", "https"),
                ("insecure", "true"),
                ("cacert", "/nonexistent/ca.pem"),
            ]),
            &f,
            None,
        )
        .unwrap();
        assert!(skip.builder(reqwest::Client::builder()).is_ok());
    }

    #[test]
    fn tls_failures_name_the_engines_options() {
        let t = HttpTls::new(&cfg(&[]), &fields(), None).unwrap();
        let untrusted = t.explain(Error::connect_failed(ConnectFailure::new(
            ConnectFailureKind::TlsUntrusted,
            "invalid peer certificate: UnknownIssuer",
        )));
        let msg = untrusted.to_string();
        assert!(
            msg.contains("cacert") && msg.contains("insecure=true"),
            "{msg}"
        );
        let plain = t.explain(Error::connect_failed(ConnectFailure::new(
            ConnectFailureKind::TlsNotOffered,
            "corrupt message",
        )));
        assert!(plain.to_string().contains("scheme=http"), "{plain}");
        let other = t.explain(Error::Connect("refused".into()));
        assert_eq!(other.to_string(), "refused");
        // A value that isn't one of the field's own.
        assert!(HttpTls::new(&cfg(&[("scheme", "ftp")]), &fields(), None).is_err());
    }
}
