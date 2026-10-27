//! The HTTP side shared by the REST drivers (Elasticsearch, OpenSearch,
//! Solr): base URL, client with auth and TLS options, and sending.

use crate::tls::{self, HttpTls};
use base64::Engine;
use dbine_driver::{ConnectionConfig, Error, Field, Result};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use std::time::Duration;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// The scheme of a URL typed in the host: it decides over the TLS mode
/// (`Some(true)` for `https`). `None`: a plain host name.
pub fn url_https(host: &str) -> Option<bool> {
    let (scheme, _) = host.trim().split_once("://")?;
    Some(scheme.eq_ignore_ascii_case("https"))
}

/// The TLS settings of a connection whose host may be a URL.
pub fn tls_for(cfg: &ConnectionConfig, fields: &[Field]) -> Result<HttpTls> {
    HttpTls::new(cfg, fields, url_https(&cfg.host))
}

/// `http(s)://host:port` from the form. A host that's already a URL is
/// used as is; the port field only fills in a URL without a port. Through
/// an SSH tunnel with TLS the host is the real server's name.
pub fn base_url(cfg: &ConnectionConfig, default_port: u16, tls: &HttpTls) -> String {
    tls.url(raw_base_url(cfg, default_port, tls.https()))
}

fn raw_base_url(cfg: &ConnectionConfig, default_port: u16, https: bool) -> String {
    let host = cfg.host.trim().trim_end_matches('/');
    let host = if host.is_empty() { "localhost" } else { host };
    if let Some((scheme, rest)) = host.split_once("://") {
        let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
        let has_port = authority
            .rsplit_once(']')
            .map_or(authority, |(_, r)| r)
            .contains(':');
        if cfg.port != 0 && !has_port {
            let path = if path.is_empty() {
                String::new()
            } else {
                format!("/{path}")
            };
            return format!("{scheme}://{authority}:{}{path}", cfg.port);
        }
        return host.to_string();
    }
    let scheme = if https { "https" } else { "http" };
    format!("{scheme}://{host}:{}", cfg.port_or(default_port))
}

pub enum Auth<'a> {
    None,
    Basic(&'a str, &'a str),
    /// `Authorization: <scheme> <token>` (e.g. `ApiKey …`).
    Token(&'a str, &'a str),
}

pub fn auth_from(cfg: &ConnectionConfig) -> Auth<'_> {
    match cfg.username.as_deref().filter(|u| !u.is_empty()) {
        Some(u) => Auth::Basic(u, cfg.password_or_empty()),
        None => Auth::None,
    }
}

/// A client that sends the credentials on every request, with the
/// connection's TLS settings.
pub fn client(tls: &HttpTls, auth: Auth<'_>) -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    let value = match auth {
        Auth::None => None,
        Auth::Basic(u, p) => Some(format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"))
        )),
        Auth::Token(scheme, t) => Some(format!("{scheme} {t}")),
    };
    if let Some(v) = value {
        let mut hv = HeaderValue::from_str(&v)
            .map_err(|_| Error::Connect("Credenciales con caracteres inválidos.".into()))?;
        hv.set_sensitive(true);
        headers.insert(AUTHORIZATION, hv);
    }
    tls.builder(
        reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(CONNECT_TIMEOUT),
    )?
    .build()
    .map_err(|e| Error::Connect(format!("No se pudo crear el cliente HTTP: {e}")))
}

/// Send and read the whole body. Network failures are `Connect` errors.
pub async fn send(rb: reqwest::RequestBuilder) -> Result<(u16, String)> {
    let resp = rb.send().await.map_err(net_err)?;
    let status = resp.status().as_u16();
    let body = resp.text().await.map_err(net_err)?;
    Ok((status, body))
}

pub fn net_err(e: reqwest::Error) -> Error {
    if e.is_timeout() {
        Error::Connect(format!("Tiempo de espera agotado: {e}"))
    } else if e.is_connect() {
        tls::net_error(&e)
    } else {
        Error::Connect(format!("Error de comunicación con el servidor: {e}"))
    }
}

/// The first `n` characters of `s`.
pub fn clip(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().count() > n {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_urls() {
        let fields = crate::tls_fields();
        let url = |cfg: &ConnectionConfig| base_url(cfg, 9200, &tls_for(cfg, &fields).unwrap());
        let mut cfg = ConnectionConfig {
            host: "es.local".into(),
            ..Default::default()
        };
        assert_eq!(url(&cfg), "http://es.local:9200");
        cfg.encrypt = true;
        cfg.port = 9300;
        assert_eq!(url(&cfg), "https://es.local:9300");
        cfg.host = "https://x.example.com/".into();
        assert_eq!(url(&cfg), "https://x.example.com:9300");
        cfg.host = "https://x.example.com:443".into();
        assert_eq!(url(&cfg), "https://x.example.com:443");
        // The URL's scheme decides over the mode.
        cfg.host = "http://x.example.com".into();
        cfg.options.insert("protocol".into(), "https".into());
        assert_eq!(url(&cfg), "http://x.example.com:9300");
        assert!(!tls_for(&cfg, &fields).unwrap().https());
        // Through a tunnel with TLS the URL names the real server.
        cfg.host = "127.0.0.1".into();
        cfg.port = 40000;
        cfg.options.insert(
            dbine_driver::TUNNEL_ORIGIN_HOST.into(),
            "es.example.com".into(),
        );
        assert_eq!(url(&cfg), "https://es.example.com:40000");
        cfg.host = "https://127.0.0.1:40000/base".into();
        assert_eq!(url(&cfg), "https://es.example.com:40000/base");
        cfg.options.insert("protocol".into(), "http".into());
        cfg.host = "127.0.0.1".into();
        assert_eq!(url(&cfg), "http://127.0.0.1:40000");
    }
}
