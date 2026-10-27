//! Amazon Neptune over its openCypher HTTPS endpoint (`POST /openCypher`),
//! with SigV4 signing (service `neptune-db`) when IAM authentication is on.
//!
//! Replies are `{"results": [{column: value, …}, …]}`; column order is
//! kept as the server wrote it (serde_json's map would sort it), and
//! nodes / relationships already come in the `~id` / `~labels` /
//! `~properties` shape the Bolt engines are converted to.

#[path = "../../dynamodb/src/aws.rs"]
mod aws;

use aws_credential_types::provider::ProvideCredentials;
use dbine_driver::{ConnectionConfig, Error, Field, Result, TlsLevel};
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest::Method;
use serde::de::{Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;
use std::time::{Duration, SystemTime};

pub use aws::fields as aws_fields;

const TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
    /// `sslSkipCertValidation`: the certificate is pinned instead
    /// (`dbine_tls::PinVerifier`); what the handshakes saw.
    pub pin_watch: Option<dbine_tls::PinWatch>,
    /// SigV4 signing: (region, credentials provider).
    iam: Option<(
        String,
        aws_credential_types::provider::SharedCredentialsProvider,
    )>,
}

/// TinkerPop's `enableSsl`: HTTPS.
pub fn enable_ssl() -> Field {
    Field::tls_switch(
        "enableSsl",
        "enableSsl",
        TlsLevel::VerifyFull,
        TlsLevel::Off,
        true,
        ["", "", "true", "true"],
    )
    .help("Neptune solo acepta HTTPS; sin enableSsl la conexión va por http (un proxy o un túnel propio).")
}

/// TinkerPop's `sslSkipCertValidation`.
pub fn skip_cert_validation() -> Field {
    Field::tls_skip_verify(
        "sslSkipCertValidation",
        "sslSkipCertValidation",
        false,
        ["false", "true", "false", "true"],
    )
}

/// HTTPS: `enableSsl`, or (a connection saved before it, plain) IAM.
fn https(cfg: &ConnectionConfig) -> bool {
    match cfg.tls_value(&enable_ssl()) {
        Some(v) => v == "true",
        None => cfg.option("iam") == Some("true"),
    }
}

/// Through an SSH tunnel: the real server's name, to put in the URL (the
/// certificate is checked against it and SigV4 signs it as `Host`) while
/// the socket goes to the tunnel.
fn tunnel_name(cfg: &ConnectionConfig) -> Option<&str> {
    cfg.option(dbine_driver::TUNNEL_ORIGIN_HOST)
        .map(str::trim)
        .filter(|h| !h.is_empty())
}

/// A request that didn't reach Neptune, with its causes (the TLS one names
/// the option to change).
fn send_error(e: reqwest::Error) -> Error {
    let mut text = e.to_string();
    let mut cur = std::error::Error::source(&e);
    while let Some(s) = cur {
        text.push_str(&format!(": {s}"));
        cur = s.source();
    }
    if text.to_lowercase().contains("certificate") {
        Error::Connect(format!(
            "no se pudo verificar el certificado de Neptune ({text}): activá sslSkipCertValidation para no verificarlo"
        ))
    } else {
        Error::Connect(format!("no se pudo llegar a Neptune: {text}"))
    }
}

pub fn base_url(cfg: &ConnectionConfig) -> String {
    let host = cfg.host.trim();
    let host = if host.is_empty() { "localhost" } else { host };
    if host.starts_with("http://") || host.starts_with("https://") {
        return host.trim_end_matches('/').to_string();
    }
    // Neptune only takes TLS; plain http is left for proxies / tunnels.
    let scheme = if https(cfg) { "https" } else { "http" };
    let host = tunnel_name(cfg).unwrap_or(host);
    format!("{scheme}://{host}:{}", cfg.port_or(8182))
}

/// One reply: the columns in server order and the rows.
#[derive(Debug, Default, PartialEq)]
pub struct Reply {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

/// `{"results": [ {…}, … ]}` read keeping each object's key order.
pub fn parse_results(text: &str) -> std::result::Result<Reply, String> {
    struct Top;
    struct Rows;
    struct Row;
    impl<'de> Visitor<'de> for Top {
        type Value = Vec<Vec<(String, Value)>>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("un objeto con results")
        }
        fn visit_map<A: MapAccess<'de>>(
            self,
            mut m: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(k) = m.next_key::<String>()? {
                if k == "results" {
                    out = m.next_value_seed(Rows)?;
                } else {
                    m.next_value::<serde::de::IgnoredAny>()?;
                }
            }
            Ok(out)
        }
    }
    impl<'de> serde::de::DeserializeSeed<'de> for Rows {
        type Value = Vec<Vec<(String, Value)>>;
        fn deserialize<D: Deserializer<'de>>(
            self,
            d: D,
        ) -> std::result::Result<Self::Value, D::Error> {
            d.deserialize_seq(self)
        }
    }
    impl<'de> Visitor<'de> for Rows {
        type Value = Vec<Vec<(String, Value)>>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("una lista de filas")
        }
        fn visit_seq<A: SeqAccess<'de>>(
            self,
            mut s: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(r) = s.next_element_seed(Row)? {
                out.push(r);
            }
            Ok(out)
        }
    }
    impl<'de> serde::de::DeserializeSeed<'de> for Row {
        type Value = Vec<(String, Value)>;
        fn deserialize<D: Deserializer<'de>>(
            self,
            d: D,
        ) -> std::result::Result<Self::Value, D::Error> {
            d.deserialize_map(self)
        }
    }
    impl<'de> Visitor<'de> for Row {
        type Value = Vec<(String, Value)>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("una fila")
        }
        fn visit_map<A: MapAccess<'de>>(
            self,
            mut m: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some((k, v)) = m.next_entry::<String, Value>()? {
                out.push((k, v));
            }
            Ok(out)
        }
    }
    let mut de = serde_json::Deserializer::from_str(text);
    let rows = de
        .deserialize_map(Top)
        .map_err(|e| format!("respuesta inesperada de Neptune: {e}"))?;
    let mut reply = Reply::default();
    for r in &rows {
        for (k, _) in r {
            if !reply.columns.contains(k) {
                reply.columns.push(k.clone());
            }
        }
    }
    reply.rows = rows
        .into_iter()
        .map(|r| {
            reply
                .columns
                .iter()
                .map(|c| {
                    r.iter()
                        .find(|(k, _)| k == c)
                        .map(|(_, v)| v.clone())
                        .unwrap_or(Value::Null)
                })
                .collect()
        })
        .collect();
    Ok(reply)
}

fn form(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={}", utf8_percent_encode(v, NON_ALPHANUMERIC)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The error text of a non-2xx reply (`{"code", "detailedMessage"}`).
fn error_of(status: reqwest::StatusCode, body: &str) -> Error {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let code = v.get("code").and_then(Value::as_str).unwrap_or_default();
    let msg = v
        .get("detailedMessage")
        .or_else(|| v.get("message"))
        .and_then(Value::as_str)
        .unwrap_or(body);
    let text = if code.is_empty() {
        format!("HTTP {status}: {msg}")
    } else {
        format!("{code}: {msg}")
    };
    if status.as_u16() == 401
        || status.as_u16() == 403
        || code.contains("AccessDenied")
        || code.contains("Signature")
    {
        Error::AuthFailed(text)
    } else {
        Error::Query(text)
    }
}

impl Client {
    pub async fn new(cfg: &ConnectionConfig) -> Result<Client> {
        let skip = cfg
            .tls_value(&skip_cert_validation())
            .is_some_and(|v| v == "true");
        let mut http = reqwest::Client::builder()
            .connect_timeout(TIMEOUT)
            .user_agent("DBine");
        // Not verifying the chain pins the certificate: the first one seen
        // (saved by the app) and only that one afterwards.
        let mut pin_watch = None;
        if skip {
            let (mut tls, watch) = dbine_tls::pinned_config(dbine_tls::pin_of(cfg))?;
            tls.alpn_protocols = vec![b"http/1.1".to_vec()];
            http = http.use_preconfigured_tls(tls);
            pin_watch = Some(watch);
        }
        let host = cfg.host.trim();
        if let (Some(name), false) = (
            tunnel_name(cfg),
            host.starts_with("http://") || host.starts_with("https://"),
        ) {
            // The URL names the real server; the socket goes to the tunnel
            // (the URL's port wins over this one).
            let local = format!("{}:0", if host.is_empty() { "127.0.0.1" } else { host });
            if let Ok(Some(addr)) =
                std::net::ToSocketAddrs::to_socket_addrs(&local.as_str()).map(|mut a| a.next())
            {
                http = http.resolve(name, addr);
            }
        }
        let http = http.build().map_err(Error::connect)?;
        let iam = if cfg.option("iam") == Some("true") {
            let conf = aws::sdk_config(cfg).await?;
            let region = conf.region().map(|r| r.to_string()).unwrap_or_default();
            let provider = conf
                .credentials_provider()
                .ok_or_else(|| Error::AuthFailed("no se encontraron credenciales de AWS".into()))?;
            Some((region, provider))
        } else {
            None
        };
        Ok(Client {
            http,
            base: base_url(cfg),
            pin_watch,
            iam,
        })
    }

    /// One request; `body` is form-encoded.
    pub async fn call(&self, method: Method, path: &str, body: Option<String>) -> Result<String> {
        let url = format!("{}{path}", self.base);
        let mut rq = self
            .http
            .request(method.clone(), &url)
            .header("Accept", "application/json");
        if body.is_some() {
            rq = rq.header("Content-Type", "application/x-www-form-urlencoded");
        }
        if let Some((region, provider)) = &self.iam {
            for (k, v) in self
                .sign(
                    region,
                    provider,
                    method.as_str(),
                    &url,
                    body.as_deref().unwrap_or(""),
                )
                .await?
            {
                rq = rq.header(k, v);
            }
        }
        if let Some(b) = body {
            rq = rq.body(b);
        }
        let resp = rq.send().await.map_err(send_error)?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| Error::Connect(e.to_string()))?;
        if status.is_success() {
            Ok(text)
        } else {
            Err(error_of(status, &text))
        }
    }

    async fn sign(
        &self,
        region: &str,
        provider: &aws_credential_types::provider::SharedCredentialsProvider,
        method: &str,
        url: &str,
        body: &str,
    ) -> Result<Vec<(String, String)>> {
        use aws_sigv4::http_request::{sign, SignableBody, SignableRequest, SigningSettings};
        use aws_sigv4::sign::v4;
        let creds = provider
            .provide_credentials()
            .await
            .map_err(|e| Error::AuthFailed(format!("credenciales de AWS: {e}")))?;
        let identity = creds.into();
        let params = v4::SigningParams::builder()
            .identity(&identity)
            .region(region)
            .name("neptune-db")
            .time(SystemTime::now())
            .settings(SigningSettings::default())
            .build()
            .map_err(|e| Error::AuthFailed(format!("firma SigV4: {e}")))?
            .into();
        let host = url
            .split("://")
            .nth(1)
            .and_then(|r| r.split('/').next())
            .unwrap_or_default()
            .to_string();
        let mut headers = vec![("host", host.as_str())];
        if !body.is_empty() {
            headers.push(("content-type", "application/x-www-form-urlencoded"));
        }
        let req = SignableRequest::new(
            method,
            url,
            headers.into_iter(),
            SignableBody::Bytes(body.as_bytes()),
        )
        .map_err(|e| Error::AuthFailed(format!("firma SigV4: {e}")))?;
        let (instructions, _) = sign(req, &params)
            .map_err(|e| Error::AuthFailed(format!("firma SigV4: {e}")))?
            .into_parts();
        Ok(instructions
            .headers()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect())
    }

    /// Run one openCypher statement.
    pub async fn query(&self, query: &str, params: Option<&Value>) -> Result<Reply> {
        let p = params.map(Value::to_string);
        let mut pairs = vec![("query", query)];
        if let Some(p) = &p {
            pairs.push(("parameters", p.as_str()));
        }
        let text = self
            .call(Method::POST, "/openCypher", Some(form(&pairs)))
            .await?;
        parse_results(&text).map_err(Error::Query)
    }

    /// `explain=static` (plan only) or `dynamic` (runs it, with figures).
    pub async fn explain(&self, query: &str, mode: &str) -> Result<String> {
        self.call(
            Method::POST,
            "/openCypher",
            Some(form(&[("query", query), ("explain", mode)])),
        )
        .await
    }

    pub async fn json(&self, path: &str) -> Result<Value> {
        let t = self.call(Method::GET, path, None).await?;
        serde_json::from_str(&t)
            .map_err(|e| Error::Query(format!("respuesta inesperada de {path}: {e}")))
    }

    /// Cancel one running query by its id (the process list).
    pub async fn cancel(&self, id: &str) -> Result<()> {
        self.call(
            Method::POST,
            "/openCypher/status",
            Some(form(&[("cancelQuery", ""), ("queryId", id)])),
        )
        .await
        .map(|_| ())
        .map_err(|e| match e {
            Error::Query(m) => Error::Query(format!("no se pudo cancelar la consulta {id}: {m}")),
            e => e,
        })
    }

    /// Cancel the running queries whose text is `query` (the interrupter).
    pub async fn cancel_matching(&self, query: &str) -> Result<()> {
        let st = self.json("/openCypher/status").await?;
        for q in st
            .get("queries")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if q.get("queryString").and_then(Value::as_str).map(str::trim) == Some(query.trim()) {
                if let Some(id) = q.get("queryId").and_then(Value::as_str) {
                    let _ = self
                        .call(
                            Method::POST,
                            "/openCypher/status",
                            Some(form(&[("cancelQuery", ""), ("queryId", id)])),
                        )
                        .await;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn results_keep_column_order() {
        let r = parse_results(
            r#"{"results":[{"z":1,"a":{"~id":"1","~labels":["P"]}},{"a":null,"b":true}]}"#,
        )
        .unwrap();
        assert_eq!(r.columns, ["z", "a", "b"]);
        assert_eq!(
            r.rows[0],
            vec![
                json!(1),
                json!({ "~id": "1", "~labels": ["P"] }),
                Value::Null
            ]
        );
        assert_eq!(r.rows[1], vec![Value::Null, Value::Null, json!(true)]);
        assert_eq!(
            parse_results(r#"{"results":[]}"#).unwrap(),
            Reply::default()
        );
        assert!(parse_results("[]").is_err());
    }

    #[test]
    fn urls_and_forms() {
        let mut c = ConnectionConfig {
            host: "db.cluster-x.us-east-1.neptune.amazonaws.com".into(),
            encrypt: true,
            ..Default::default()
        };
        assert_eq!(
            base_url(&c),
            "https://db.cluster-x.us-east-1.neptune.amazonaws.com:8182"
        );
        c.host = "http://localhost:8182/".into();
        assert_eq!(base_url(&c), "http://localhost:8182");
        c.host = "db".into();
        // Saved before enableSsl: [ff, ft] plain unless IAM, [tf, tt] https.
        let url = |encrypt: bool, trust: bool, iam: bool| {
            let mut c = ConnectionConfig {
                host: "db".into(),
                encrypt,
                trust_server_certificate: trust,
                ..Default::default()
            };
            if iam {
                c.options.insert("iam".into(), "true".into());
            }
            base_url(&c)
        };
        assert_eq!(url(false, false, false), "http://db:8182");
        assert_eq!(url(false, true, false), "http://db:8182");
        assert_eq!(url(false, false, true), "https://db:8182");
        assert_eq!(url(true, false, false), "https://db:8182");
        assert_eq!(url(true, true, false), "https://db:8182");
        // The option decides; a new connection is https.
        assert_eq!(enable_ssl().default, "true");
        c.options.insert("enableSsl".into(), "false".into());
        assert_eq!(base_url(&c), "http://db:8182");
        c.options.insert("enableSsl".into(), "true".into());
        c.encrypt = false;
        assert_eq!(base_url(&c), "https://db:8182");
        // Through a tunnel the URL names the real endpoint.
        c.host = "127.0.0.1".into();
        c.port = 40123;
        c.options.insert(
            dbine_driver::TUNNEL_ORIGIN_HOST.into(),
            "db.cluster-x.neptune.amazonaws.com".into(),
        );
        assert_eq!(
            base_url(&c),
            "https://db.cluster-x.neptune.amazonaws.com:40123"
        );
        // sslSkipCertValidation: [ff, ft, tf, tt] → false, true, false, true.
        let skip = skip_cert_validation();
        let skips = |encrypt: bool, trust: bool| {
            ConnectionConfig {
                encrypt,
                trust_server_certificate: trust,
                ..Default::default()
            }
            .tls_value(&skip)
                == Some("true")
        };
        assert!(!skips(false, false) && skips(false, true));
        assert!(!skips(true, false) && skips(true, true));
        assert_eq!(
            form(&[("query", "RETURN 1 AS a"), ("explain", "static")]),
            "query=RETURN%201%20AS%20a&explain=static"
        );
    }

    #[test]
    fn errors() {
        let e = error_of(
            reqwest::StatusCode::BAD_REQUEST,
            r#"{"code":"MalformedQueryException","detailedMessage":"bad"}"#,
        );
        assert!(matches!(e, Error::Query(m) if m == "MalformedQueryException: bad"));
        assert!(matches!(
            error_of(reqwest::StatusCode::FORBIDDEN, "{}"),
            Error::AuthFailed(_)
        ));
    }
}
