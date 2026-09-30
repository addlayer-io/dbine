//! The HTTP side shared by the REST drivers (Elasticsearch, OpenSearch,
//! Solr): base URL, client with auth and TLS options, and sending.

use base64::Engine;
use dbine_driver::{ConnectionConfig, Error, Result};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use std::time::Duration;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// `http(s)://host:port` from the form. A host that's already a URL is
/// used as is; the port field only fills in a URL without a port.
pub fn base_url(cfg: &ConnectionConfig, default_port: u16) -> String {
    let host = cfg.host.trim().trim_end_matches('/');
    let host = if host.is_empty() { "localhost" } else { host };
    if let Some((scheme, rest)) = host.split_once("://") {
        let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
        let has_port = authority.rsplit_once(']').map_or(authority, |(_, r)| r).contains(':');
        if cfg.port != 0 && !has_port {
            let path = if path.is_empty() { String::new() } else { format!("/{path}") };
            return format!("{scheme}://{authority}:{}{path}", cfg.port);
        }
        return host.to_string();
    }
    let scheme = if cfg.encrypt { "https" } else { "http" };
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

/// A client that sends the credentials on every request.
pub fn client(cfg: &ConnectionConfig, auth: Auth<'_>) -> Result<reqwest::Client> {
    let mut headers = HeaderMap::new();
    let value = match auth {
        Auth::None => None,
        Auth::Basic(u, p) => {
            Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"))))
        }
        Auth::Token(scheme, t) => Some(format!("{scheme} {t}")),
    };
    if let Some(v) = value {
        let mut hv = HeaderValue::from_str(&v).map_err(|_| Error::Connect("Credenciales con caracteres inválidos.".into()))?;
        hv.set_sensitive(true);
        headers.insert(AUTHORIZATION, hv);
    }
    reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(CONNECT_TIMEOUT)
        .danger_accept_invalid_certs(cfg.trust_server_certificate)
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
        Error::Connect(format!("No se pudo conectar con el servidor: {e}"))
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
        let mut cfg = ConnectionConfig { host: "es.local".into(), ..Default::default() };
        assert_eq!(base_url(&cfg, 9200), "http://es.local:9200");
        cfg.encrypt = true;
        cfg.port = 9300;
        assert_eq!(base_url(&cfg, 9200), "https://es.local:9300");
        cfg.host = "https://x.example.com/".into();
        assert_eq!(base_url(&cfg, 9200), "https://x.example.com:9300");
        cfg.host = "https://x.example.com:443".into();
        assert_eq!(base_url(&cfg, 9200), "https://x.example.com:443");
    }
}
