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
use dbine_driver::{ConnectionConfig, Error, Result};
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
    /// SigV4 signing: (region, credentials provider).
    iam: Option<(String, aws_credential_types::provider::SharedCredentialsProvider)>,
}

pub fn base_url(cfg: &ConnectionConfig) -> String {
    let host = cfg.host.trim();
    let host = if host.is_empty() { "localhost" } else { host };
    if host.starts_with("http://") || host.starts_with("https://") {
        return host.trim_end_matches('/').to_string();
    }
    // Neptune only takes TLS; plain http is left for proxies / tunnels.
    let scheme = if cfg.encrypt || cfg.option("iam") == Some("true") { "https" } else { "http" };
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
        fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> std::result::Result<Self::Value, A::Error> {
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
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> std::result::Result<Self::Value, D::Error> {
            d.deserialize_seq(self)
        }
    }
    impl<'de> Visitor<'de> for Rows {
        type Value = Vec<Vec<(String, Value)>>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("una lista de filas")
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> std::result::Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some(r) = s.next_element_seed(Row)? {
                out.push(r);
            }
            Ok(out)
        }
    }
    impl<'de> serde::de::DeserializeSeed<'de> for Row {
        type Value = Vec<(String, Value)>;
        fn deserialize<D: Deserializer<'de>>(self, d: D) -> std::result::Result<Self::Value, D::Error> {
            d.deserialize_map(self)
        }
    }
    impl<'de> Visitor<'de> for Row {
        type Value = Vec<(String, Value)>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("una fila")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> std::result::Result<Self::Value, A::Error> {
            let mut out = Vec::new();
            while let Some((k, v)) = m.next_entry::<String, Value>()? {
                out.push((k, v));
            }
            Ok(out)
        }
    }
    let mut de = serde_json::Deserializer::from_str(text);
    let rows = de.deserialize_map(Top).map_err(|e| format!("respuesta inesperada de Neptune: {e}"))?;
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
        .map(|r| reply.columns.iter().map(|c| r.iter().find(|(k, _)| k == c).map(|(_, v)| v.clone()).unwrap_or(Value::Null)).collect())
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
    let msg = v.get("detailedMessage").or_else(|| v.get("message")).and_then(Value::as_str).unwrap_or(body);
    let text = if code.is_empty() { format!("HTTP {status}: {msg}") } else { format!("{code}: {msg}") };
    if status.as_u16() == 401 || status.as_u16() == 403 || code.contains("AccessDenied") || code.contains("Signature") {
        Error::AuthFailed(text)
    } else {
        Error::Query(text)
    }
}

impl Client {
    pub async fn new(cfg: &ConnectionConfig) -> Result<Client> {
        let http = reqwest::Client::builder()
            .connect_timeout(TIMEOUT)
            .danger_accept_invalid_certs(cfg.trust_server_certificate)
            .user_agent("DBine")
            .build()
            .map_err(Error::connect)?;
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
        Ok(Client { http, base: base_url(cfg), iam })
    }

    /// One request; `body` is form-encoded.
    pub async fn call(&self, method: Method, path: &str, body: Option<String>) -> Result<String> {
        let url = format!("{}{path}", self.base);
        let mut rq = self.http.request(method.clone(), &url).header("Accept", "application/json");
        if body.is_some() {
            rq = rq.header("Content-Type", "application/x-www-form-urlencoded");
        }
        if let Some((region, provider)) = &self.iam {
            for (k, v) in self.sign(region, provider, method.as_str(), &url, body.as_deref().unwrap_or("")).await? {
                rq = rq.header(k, v);
            }
        }
        if let Some(b) = body {
            rq = rq.body(b);
        }
        let resp = rq.send().await.map_err(|e| Error::Connect(format!("no se pudo llegar a Neptune: {e}")))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| Error::Connect(e.to_string()))?;
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
        let creds = provider.provide_credentials().await.map_err(|e| Error::AuthFailed(format!("credenciales de AWS: {e}")))?;
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
        let host = url.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or_default().to_string();
        let mut headers = vec![("host", host.as_str())];
        if !body.is_empty() {
            headers.push(("content-type", "application/x-www-form-urlencoded"));
        }
        let req = SignableRequest::new(method, url, headers.into_iter(), SignableBody::Bytes(body.as_bytes()))
            .map_err(|e| Error::AuthFailed(format!("firma SigV4: {e}")))?;
        let (instructions, _) = sign(req, &params).map_err(|e| Error::AuthFailed(format!("firma SigV4: {e}")))?.into_parts();
        Ok(instructions.headers().map(|(k, v)| (k.to_string(), v.to_string())).collect())
    }

    /// Run one openCypher statement.
    pub async fn query(&self, query: &str, params: Option<&Value>) -> Result<Reply> {
        let p = params.map(Value::to_string);
        let mut pairs = vec![("query", query)];
        if let Some(p) = &p {
            pairs.push(("parameters", p.as_str()));
        }
        let text = self.call(Method::POST, "/openCypher", Some(form(&pairs))).await?;
        parse_results(&text).map_err(Error::Query)
    }

    /// `explain=static` (plan only) or `dynamic` (runs it, with figures).
    pub async fn explain(&self, query: &str, mode: &str) -> Result<String> {
        self.call(Method::POST, "/openCypher", Some(form(&[("query", query), ("explain", mode)]))).await
    }

    pub async fn json(&self, path: &str) -> Result<Value> {
        let t = self.call(Method::GET, path, None).await?;
        serde_json::from_str(&t).map_err(|e| Error::Query(format!("respuesta inesperada de {path}: {e}")))
    }

    /// Cancel the running queries whose text is `query` (the interrupter).
    pub async fn cancel_matching(&self, query: &str) -> Result<()> {
        let st = self.json("/openCypher/status").await?;
        for q in st.get("queries").and_then(Value::as_array).cloned().unwrap_or_default() {
            if q.get("queryString").and_then(Value::as_str).map(str::trim) == Some(query.trim()) {
                if let Some(id) = q.get("queryId").and_then(Value::as_str) {
                    let _ = self.call(Method::POST, "/openCypher/status", Some(form(&[("cancelQuery", ""), ("queryId", id)]))).await;
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
        let r = parse_results(r#"{"results":[{"z":1,"a":{"~id":"1","~labels":["P"]}},{"a":null,"b":true}]}"#).unwrap();
        assert_eq!(r.columns, ["z", "a", "b"]);
        assert_eq!(r.rows[0], vec![json!(1), json!({ "~id": "1", "~labels": ["P"] }), Value::Null]);
        assert_eq!(r.rows[1], vec![Value::Null, Value::Null, json!(true)]);
        assert_eq!(parse_results(r#"{"results":[]}"#).unwrap(), Reply::default());
        assert!(parse_results("[]").is_err());
    }

    #[test]
    fn urls_and_forms() {
        let mut c = ConnectionConfig { host: "db.cluster-x.us-east-1.neptune.amazonaws.com".into(), encrypt: true, ..Default::default() };
        assert_eq!(base_url(&c), "https://db.cluster-x.us-east-1.neptune.amazonaws.com:8182");
        c.host = "http://localhost:8182/".into();
        assert_eq!(base_url(&c), "http://localhost:8182");
        assert_eq!(form(&[("query", "RETURN 1 AS a"), ("explain", "static")]), "query=RETURN%201%20AS%20a&explain=static");
    }

    #[test]
    fn errors() {
        let e = error_of(reqwest::StatusCode::BAD_REQUEST, r#"{"code":"MalformedQueryException","detailedMessage":"bad"}"#);
        assert!(matches!(e, Error::Query(m) if m == "MalformedQueryException: bad"));
        assert!(matches!(error_of(reqwest::StatusCode::FORBIDDEN, "{}"), Error::AuthFailed(_)));
    }
}
