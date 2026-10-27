//! What the three APIs share: the base URL, the HTTP client and how HTTP
//! failures become DBine errors.

use crate::tls_http;
use dbine_driver::{ConnectionConfig, Error, Field, Result};
use std::time::Duration;

/// `http(s)://host:port`, from a bare host or a URL typed in the host field
/// (whose scheme wins over the TLS mode).
pub fn base_url(cfg: &ConnectionConfig, fields: &[Field], default_port: u16) -> String {
    let host = cfg.host.trim().trim_end_matches('/');
    let host = if host.is_empty() { "localhost" } else { host };
    if host.starts_with("http://") || host.starts_with("https://") {
        let has_port = host.split("://").nth(1).is_some_and(|h| h.contains(':'));
        return if has_port || cfg.port == 0 {
            host.to_string()
        } else {
            format!("{host}:{}", cfg.port)
        };
    }
    let scheme = if tls_http::https(cfg, fields) {
        "https"
    } else {
        "http"
    };
    format!("{scheme}://{host}:{}", cfg.port_or(default_port))
}

/// The client and the base URL, with the connection's TLS options.
pub fn client(
    cfg: &ConnectionConfig,
    fields: &[Field],
    default_port: u16,
) -> Result<(reqwest::Client, String, tls_http::Pin)> {
    let b = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600));
    let (base, http, pin) = tls_http::client(cfg, fields, base_url(cfg, fields, default_port), b)?;
    Ok((http, base, pin))
}

/// A request that didn't get an answer.
pub fn send_err(e: reqwest::Error) -> Error {
    if e.is_timeout() && !e.is_connect() {
        Error::Query(format!("tiempo de espera agotado: {e}"))
    } else {
        tls_http::connect_error(&e, |e| format!("no se pudo conectar con InfluxDB: {e}"))
    }
}

/// The body of a successful response, or the server's error.
pub async fn text(resp: reqwest::Response) -> Result<String> {
    let status = resp.status();
    let body = resp.text().await.map_err(send_err)?;
    if status.is_success() {
        return Ok(body);
    }
    let msg = error_message(&body)
        .or_else(|| Some(body.trim().to_string()).filter(|b| !b.is_empty() && !b.starts_with('<')))
        .unwrap_or_else(|| format!("HTTP {status}"));
    Err(match status.as_u16() {
        401 | 403 => Error::AuthFailed(msg),
        _ => Error::Query(msg),
    })
}

/// `{"error": …}` (v1, v3) or `{"code": …, "message": …}` (v2).
pub fn error_message(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    ["error", "message"]
        .iter()
        .find_map(|k| v.get(k)?.as_str().map(str::to_string))
}

/// RFC 3339 timestamps (`2024-01-31T13:45:00.5Z`) as DBine shows dates
/// (`2024-01-31 13:45:00.5`, UTC); anything else unchanged.
pub fn iso_time(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let looks = b.len() >= 19
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[13] == b':'
        && b[16] == b':';
    if !looks {
        return None;
    }
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(
            t.with_timezone(&chrono::Utc)
                .format("%Y-%m-%d %H:%M:%S%.f")
                .to_string(),
        );
    }
    Some(s.trim_end_matches('Z').replacen('T', " ", 1))
}

/// A Flux / InfluxQL / SQL string literal body: backslash and the quote
/// escaped (and Flux's `${` interpolation).
pub fn escape(s: &str, quote: char) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c == '\\' || c == quote {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Saved connections keep `encrypt` / `trust_server_certificate`'s
    /// scheme and checks; new ones are https, verified.
    #[test]
    fn tls_options() {
        for f in [
            crate::v1::info().fields,
            crate::v2::info().fields,
            crate::v3::info().fields,
        ] {
            tls_http::assert_legacy_and_defaults(&f);
            let cfg = ConnectionConfig {
                host: "db".into(),
                ..tls_http::new_connection(&f)
            };
            assert_eq!(base_url(&cfg, &f, 8086), "https://db:8086");
        }
        // A URL typed in the host keeps its scheme.
        let f = crate::v2::info().fields;
        let cfg = ConnectionConfig {
            host: "http://db:8086".into(),
            ..tls_http::new_connection(&f)
        };
        assert_eq!(base_url(&cfg, &f, 8086), "http://db:8086");
        // influxdb3's --tls-no-verify skips the checks.
        let f = crate::v3::info().fields;
        let mut cfg = tls_http::new_connection(&f);
        cfg.options.insert("tls-no-verify".into(), "true".into());
        assert_eq!(tls_http::effect(&cfg, &f), (true, true, false));
    }

    #[test]
    fn urls() {
        let f = crate::v2::info().fields;
        let mut cfg = ConnectionConfig {
            host: "db".into(),
            ..Default::default()
        };
        assert_eq!(base_url(&cfg, &f, 8086), "http://db:8086");
        cfg.encrypt = true;
        cfg.port = 443;
        assert_eq!(base_url(&cfg, &f, 8086), "https://db:443");
        cfg.host = "https://cloud.example.com/".into();
        cfg.port = 0;
        assert_eq!(base_url(&cfg, &f, 8086), "https://cloud.example.com");
        cfg.host = "http://x:9999".into();
        cfg.port = 1;
        assert_eq!(base_url(&cfg, &f, 8086), "http://x:9999");
    }

    #[test]
    fn times() {
        assert_eq!(
            iso_time("2024-01-31T13:45:00Z").unwrap(),
            "2024-01-31 13:45:00"
        );
        assert_eq!(
            iso_time("2024-01-31T13:45:00.123456789Z").unwrap(),
            "2024-01-31 13:45:00.123456789"
        );
        assert_eq!(
            iso_time("2024-01-31T13:45:00").unwrap(),
            "2024-01-31 13:45:00"
        );
        assert!(iso_time("cpu").is_none());
    }

    #[test]
    fn errors() {
        assert_eq!(
            error_message(r#"{"code":"invalid","message":"bad"}"#).unwrap(),
            "bad"
        );
        assert_eq!(error_message(r#"{"error":"boom"}"#).unwrap(), "boom");
        assert!(error_message("plain").is_none());
        assert_eq!(escape(r#"a"b\c"#, '"'), r#"a\"b\\c"#);
    }
}
