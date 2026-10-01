//! Hrana over HTTP (`POST /v2/pipeline`), the protocol of libSQL's server
//! (sqld) and Turso. A pipeline carries requests on a stream; the server
//! answers with a `baton` that the next pipeline sends back to stay on the
//! same stream, which keeps the connection state (open transaction, temp
//! tables, PRAGMAs). An idle stream expires on the server: the client then
//! starts a new one and says so.

use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use base64::Engine;
use dbine_driver::{json_bytes, json_f64, json_i64, Error, Result};
use serde_json::{json, Value};
use std::time::Duration;

/// Blobs come base64-encoded, with or without padding.
pub(crate) const B64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// One statement's result.
#[derive(Debug, Default, Clone)]
pub struct StmtResult {
    /// (name, declared type).
    pub cols: Vec<(String, String)>,
    pub rows: Vec<Vec<Value>>,
    pub affected: u64,
}

pub struct Client {
    http: reqwest::Client,
    /// `https://host[:port]` (or the `base_url` the server redirected to).
    base: String,
    token: Option<String>,
    baton: Option<String>,
    /// Things to tell the user (a stream that expired…).
    pub notices: Vec<String>,
    /// Statements that set up every new stream (PRAGMAs of the session).
    pub init: Vec<String>,
    /// The server speaks Hrana 3 (`/v3/pipeline`): it says whether the
    /// stream is in a transaction (`get_autocommit`) and can run a step only
    /// outside one (`is_autocommit`). Streams are the same as v2's.
    pub v3: bool,
    /// The stream's autocommit after the last script (`None`: unknown, or
    /// the server is older than Hrana 3).
    pub autocommit: Option<bool>,
}

/// A statement the server rejected: its message and Hrana's error code
/// (`SQLITE_CONSTRAINT`, `SQL_PARSE_ERROR`…).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepError {
    pub message: String,
    pub code: Option<String>,
}

impl StepError {
    fn from_value(e: &Value) -> Self {
        Self { message: error_message(e), code: e.get("code").and_then(Value::as_str).filter(|c| !c.is_empty()).map(str::to_string) }
    }
}

/// `libsql://db-org.turso.io` → `https://db-org.turso.io`; `ws(s)://` →
/// `http(s)://`; a bare host gets `https://`.
pub fn http_url(url: &str) -> Result<String> {
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        return Err(Error::Connect("falta la URL de la base".into()));
    }
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        None => ("https".to_string(), url),
    };
    let scheme = match scheme.as_str() {
        "libsql" | "wss" | "https" => "https",
        "ws" | "http" => "http",
        "file" => {
            return Err(Error::Connect("para un archivo local usá la conexión SQLite (libSQL local es un archivo SQLite)".into()));
        }
        other => return Err(Error::Connect(format!("esquema de URL no admitido: {other}://"))),
    };
    // Query parameters (`?authToken=…` in some tools) aren't part of the base.
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    Ok(format!("{scheme}://{rest}"))
}

/// The URL's host is this machine or a private network (RFC 1918, link-local, `.local`).
fn is_local(base: &str) -> bool {
    let host = base.split("://").nth(1).unwrap_or(base).split('/').next().unwrap_or("");
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(h, p)| if p.chars().all(|c| c.is_ascii_digit()) { h } else { host }),
    };
    if host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".local") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => ip.is_loopback() || ip.is_private() || ip.is_link_local(),
        Ok(std::net::IpAddr::V6(ip)) => ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00 || (ip.segments()[0] & 0xffc0) == 0xfe80,
        Err(_) => false,
    }
}

/// A Hrana value as a DBine cell.
pub fn cell(v: &Value) -> Value {
    match v.get("type").and_then(Value::as_str) {
        Some("integer") => match v.get("value") {
            Some(Value::String(s)) => s.parse::<i64>().map_or_else(|_| Value::String(s.clone()), json_i64),
            Some(n @ Value::Number(_)) => n.clone(),
            _ => Value::Null,
        },
        Some("float") => v.get("value").and_then(Value::as_f64).map_or(Value::Null, json_f64),
        Some("text") => v.get("value").cloned().unwrap_or(Value::Null),
        Some("blob") => match v.get("base64").and_then(Value::as_str).map(|b| B64.decode(b)) {
            Some(Ok(bytes)) => json_bytes(&bytes),
            _ => Value::Null,
        },
        _ => Value::Null,
    }
}

fn stmt_result(r: &Value) -> StmtResult {
    let cols = r
        .get("cols")
        .and_then(Value::as_array)
        .map(|c| {
            c.iter()
                .map(|c| {
                    let name = c.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                    let ty = c.get("decltype").and_then(Value::as_str).unwrap_or("").to_string();
                    (name, ty)
                })
                .collect()
        })
        .unwrap_or_default();
    let rows = r
        .get("rows")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().map(|row| row.as_array().map(|v| v.iter().map(cell).collect()).unwrap_or_default()).collect())
        .unwrap_or_default();
    let affected = r.get("affected_row_count").and_then(Value::as_u64).unwrap_or(0);
    StmtResult { cols, rows, affected }
}

fn error_message(e: &Value) -> String {
    let msg = e.get("message").and_then(Value::as_str).unwrap_or("error del servidor");
    msg.strip_prefix("SQLite error: ").unwrap_or(msg).to_string()
}

fn stmt(sql: &str) -> Value {
    json!({ "sql": sql, "want_rows": true })
}

enum Failure {
    /// The stream (baton) is gone: retry on a new one.
    Expired,
    Other(Error),
}

impl Client {
    pub fn new(url: &str, token: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|e| Error::Connect(e.to_string()))?;
        Ok(Self { http, base: http_url(url)?, token, baton: None, notices: Vec::new(), init: Vec::new(), v3: false, autocommit: None })
    }

    /// Whether the server speaks Hrana 3 (Turso and current sqld do): asks
    /// for the autocommit of a throwaway stream, closed right away.
    pub async fn detect_v3(&mut self) {
        let body = json!({ "baton": null, "requests": [{ "type": "get_autocommit" }, { "type": "close" }] });
        let mut req = self.http.post(format!("{}/v3/pipeline", self.base)).json(&body).timeout(Duration::from_secs(15));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let Ok(resp) = req.send().await else { return };
        if !resp.status().is_success() {
            return;
        }
        let Ok(v) = resp.json::<Value>().await else { return };
        let first = v.get("results").and_then(Value::as_array).and_then(|r| r.first()).cloned().unwrap_or(Value::Null);
        self.v3 = first.get("type").and_then(Value::as_str) == Some("ok");
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// `POST /v3/cursor` with one statement, on a stream of its own (left
    /// to expire): the response streams one JSON entry per line as the
    /// server steps the statement, so a big read is never held whole.
    /// `None`: the server doesn't speak Hrana 3.
    pub async fn cursor(&self, sql: &str) -> Result<Option<reqwest::Response>> {
        let body = json!({ "baton": null, "batch": { "steps": [{ "stmt": stmt(sql) }] } });
        let mut req = self.http.post(format!("{}/v3/cursor", self.base)).json(&body);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        // A read's JSON compresses ~20×, but gzip halves sqld's rate (1M
        // rows: 10 s plain, 20 s gzip, measured): worth it over the
        // internet, not on this machine or the local network.
        if is_local(&self.base) {
            req = req.header(reqwest::header::ACCEPT_ENCODING, "identity");
        }
        let resp = req.send().await.map_err(|e| Error::Connect(format!("no se pudo llegar al servidor: {e}")))?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::AuthFailed(format!("el servidor rechazó el token ({status}): {}", text.trim())));
        }
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Query(format!("HTTP {status}: {}", text.trim())));
        }
        Ok(Some(resp))
    }

    /// The HTTP client, `/v2/pipeline` URL and token, for pipelines sent
    /// outside [`Client::pipeline`]: the bulk transfer decodes its own
    /// responses, and its writes must be able to finish (and be rolled
    /// back) after the caller's future is dropped.
    pub(crate) fn wire(&self) -> (reqwest::Client, String, Option<String>) {
        (self.http.clone(), format!("{}/v2/pipeline", self.base), self.token.clone())
    }

    pub(crate) fn baton(&self) -> Option<&str> {
        self.baton.as_deref()
    }

    /// The stream a pipeline sent outside [`Client::pipeline`] left.
    pub(crate) fn set_stream(&mut self, baton: Option<String>, base_url: Option<&str>) {
        self.baton = baton;
        if let Some(b) = base_url.filter(|b| !b.is_empty()) {
            self.base = b.trim_end_matches('/').to_string();
        }
    }

    async fn send(&mut self, requests: &[Value]) -> std::result::Result<Vec<Value>, Failure> {
        let body = json!({ "baton": self.baton, "requests": requests });
        let version = if self.v3 { 3 } else { 2 };
        let mut req = self.http.post(format!("{}/v{version}/pipeline", self.base)).json(&body);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.map_err(|e| Failure::Other(Error::Connect(format!("no se pudo llegar al servidor: {e}"))))?;
        let status = resp.status();
        let text = resp.text().await.map_err(|e| Failure::Other(Error::Connect(e.to_string())))?;
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(Failure::Other(Error::AuthFailed(format!("el servidor rechazó el token ({status}): {}", text.trim()))));
        }
        if !status.is_success() {
            // "Received an invalid baton" or {"code": "STREAM_EXPIRED"}.
            let lower = text.to_ascii_lowercase();
            if self.baton.is_some() && (lower.contains("baton") || lower.contains("stream_expired")) {
                return Err(Failure::Expired);
            }
            return Err(Failure::Other(Error::Query(format!("HTTP {status}: {}", text.trim()))));
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| Failure::Other(Error::Query(format!("respuesta inválida: {e}"))))?;
        self.baton = v.get("baton").and_then(Value::as_str).map(str::to_string);
        if let Some(b) = v.get("base_url").and_then(Value::as_str).filter(|b| !b.is_empty()) {
            self.base = b.trim_end_matches('/').to_string();
        }
        Ok(v.get("results").and_then(Value::as_array).cloned().unwrap_or_default())
    }

    /// The requests, after the session's setup when the stream is new.
    fn with_init(&self, requests: &[Value]) -> (Vec<Value>, usize) {
        if self.baton.is_some() {
            return (requests.to_vec(), 0);
        }
        let mut all: Vec<Value> = self.init.iter().map(|s| json!({ "type": "execute", "stmt": { "sql": s } })).collect();
        let skip = all.len();
        all.extend_from_slice(requests);
        (all, skip)
    }

    /// Run a pipeline; each request's `Ok(response)` or `Err(message)`.
    pub async fn pipeline(&mut self, requests: Vec<Value>) -> Result<Vec<std::result::Result<Value, String>>> {
        Ok(self.pipeline_raw(requests).await?.into_iter().map(|r| r.map_err(|e| e.message)).collect())
    }

    /// Run a pipeline; each request's `Ok(response)` or its error.
    async fn pipeline_raw(&mut self, requests: Vec<Value>) -> Result<Vec<std::result::Result<Value, StepError>>> {
        let (first, mut skip) = self.with_init(&requests);
        let results = match self.send(&first).await {
            Ok(r) => r,
            Err(Failure::Expired) => {
                // The request never ran: the same one on a new stream.
                self.baton = None;
                let (again, s) = self.with_init(&requests);
                skip = s;
                self.notices.push(
                    "La sesión en el servidor venció por inactividad y se abrió otra: se perdieron las transacciones \
                     abiertas y las tablas temporales."
                        .into(),
                );
                self.send(&again).await.map_err(|f| match f {
                    Failure::Expired => Error::Query("el servidor rechazó la sesión".into()),
                    Failure::Other(e) => e,
                })?
            }
            Err(Failure::Other(e)) => return Err(e),
        };
        Ok(results
            .into_iter()
            .skip(skip)
            .map(|r| match r.get("type").and_then(Value::as_str) {
                Some("ok") => Ok(r.get("response").cloned().unwrap_or(Value::Null)),
                _ => Err(r.get("error").map(StepError::from_value).unwrap_or_else(|| StepError { message: "error del servidor".into(), code: None })),
            })
            .collect())
    }

    /// One statement.
    pub async fn execute(&mut self, sql: &str) -> Result<StmtResult> {
        let mut r = self.pipeline(vec![json!({ "type": "execute", "stmt": stmt(sql) })]).await?;
        match r.pop() {
            Some(Ok(resp)) => Ok(stmt_result(resp.get("result").unwrap_or(&Value::Null))),
            Some(Err(m)) => Err(Error::Query(m)),
            None => Err(Error::Query("el servidor no respondió la sentencia".into())),
        }
    }

    /// Statements that don't depend on each other, in one round trip; each
    /// one's rows or error.
    pub async fn execute_each(&mut self, sqls: &[String]) -> Result<Vec<std::result::Result<StmtResult, String>>> {
        let reqs = sqls.iter().map(|s| json!({ "type": "execute", "stmt": stmt(s) })).collect();
        Ok(self
            .pipeline(reqs)
            .await?
            .into_iter()
            .map(|r| r.map(|resp| stmt_result(resp.get("result").unwrap_or(&Value::Null))))
            .collect())
    }

    /// A script in one round trip, as a Hrana batch where each step runs
    /// only if the previous one succeeded: the results of the steps that
    /// ran, and the first error (statement, error). `begin[i]`: open a
    /// transaction before statement `i` when none is open (manual
    /// transactions; needs Hrana 3). On Hrana 3 servers the stream's
    /// autocommit after the script is kept in [`Client::autocommit`].
    pub async fn script(&mut self, sqls: &[String], begin: &[bool]) -> Result<(Vec<StmtResult>, Option<(usize, StepError)>)> {
        let steps = script_steps(sqls, begin);
        let mut requests = vec![json!({ "type": "batch", "batch": { "steps": steps.0 } })];
        if self.v3 {
            requests.push(json!({ "type": "get_autocommit" }));
        }
        let mut r = self.pipeline_raw(requests).await?;
        if self.v3 {
            self.autocommit = match r.pop() {
                Some(Ok(resp)) => resp.get("is_autocommit").and_then(Value::as_bool),
                _ => None,
            };
        }
        let resp = match r.pop() {
            Some(Ok(resp)) => resp,
            // The whole batch was refused (sqld parses every step first):
            // nothing ran.
            Some(Err(e)) => return Ok((Vec::new(), Some((0, e)))),
            None => return Err(Error::Query("el servidor no respondió el lote".into())),
        };
        let result = resp.get("result").cloned().unwrap_or(Value::Null);
        let results = result.get("step_results").and_then(Value::as_array).cloned().unwrap_or_default();
        let errors = result.get("step_errors").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut out = Vec::new();
        for (step, r) in results.iter().enumerate() {
            // The statement this step is (or opens a transaction for).
            let Some(&(i, user)) = steps.1.get(step) else { break };
            if let Some(e) = errors.get(step).filter(|e| !e.is_null()) {
                return Ok((out, Some((i, StepError::from_value(e)))));
            }
            if !user {
                continue;
            }
            if r.is_null() {
                break;
            }
            out.push(stmt_result(r));
        }
        Ok((out, None))
    }

    /// `COMMIT` or `ROLLBACK`, only when a transaction is open (Hrana 3).
    pub async fn end_transaction(&mut self, sql: &str) -> Result<()> {
        if !self.v3 {
            return Err(Error::Unsupported("este servidor libSQL no informa transacciones (necesita Hrana 3)".into()));
        }
        let steps = [json!({ "stmt": { "sql": sql }, "condition": { "type": "not", "cond": { "type": "is_autocommit" } } })];
        let mut r = self.pipeline(vec![json!({ "type": "batch", "batch": { "steps": steps } }), json!({ "type": "get_autocommit" })]).await?;
        self.autocommit = match r.pop() {
            Some(Ok(resp)) => resp.get("is_autocommit").and_then(Value::as_bool),
            _ => None,
        };
        match r.pop() {
            Some(Ok(resp)) => {
                let errors = resp.pointer("/result/step_errors").and_then(Value::as_array).cloned().unwrap_or_default();
                match errors.first().filter(|e| !e.is_null()) {
                    Some(e) => Err(Error::Query(error_message(e))),
                    None => Ok(()),
                }
            }
            Some(Err(m)) => Err(Error::Query(m)),
            None => Err(Error::Query("el servidor no respondió".into())),
        }
    }

    /// Server version from `GET /version` (sqld), if it answers.
    pub async fn version(&self) -> Option<String> {
        let mut req = self.http.get(format!("{}/version", self.base)).timeout(Duration::from_secs(5));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.ok()?;
        resp.status().is_success().then_some(())?;
        let v = resp.text().await.ok()?;
        let v = v.trim();
        (!v.is_empty() && v.len() < 200 && !v.starts_with('<')).then(|| v.to_string())
    }
}

/// The batch steps of a script: each statement runs only if the one before
/// succeeded; with `begin[i]`, a `BEGIN` that runs only outside a
/// transaction goes before statement `i`. Also, per step, the statement it
/// belongs to and whether it's the statement itself.
fn script_steps(sqls: &[String], begin: &[bool]) -> (Vec<Value>, Vec<(usize, bool)>) {
    let mut steps = Vec::new();
    let mut owner = Vec::new();
    let mut prev: Option<usize> = None;
    for (i, s) in sqls.iter().enumerate() {
        let after = prev.map(|p| json!({ "type": "ok", "step": p }));
        if begin.get(i).copied().unwrap_or(false) {
            let outside = json!({ "type": "is_autocommit" });
            let cond = match &after {
                Some(a) => json!({ "type": "and", "conds": [a, outside] }),
                None => outside,
            };
            steps.push(json!({ "stmt": { "sql": "BEGIN" }, "condition": cond }));
            owner.push((i, false));
        }
        steps.push(match after {
            Some(a) => json!({ "stmt": stmt(s), "condition": a }),
            None => json!({ "stmt": stmt(s) }),
        });
        owner.push((i, true));
        prev = Some(steps.len() - 1);
    }
    (steps, owner)
}

impl Drop for Client {
    /// Close the stream on the server (it would expire anyway).
    fn drop(&mut self) {
        let (Some(baton), Ok(rt)) = (self.baton.take(), tokio::runtime::Handle::try_current()) else { return };
        let mut req = self
            .http
            .post(format!("{}/v2/pipeline", self.base))
            .json(&json!({ "baton": baton, "requests": [{ "type": "close" }] }));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        rt.spawn(async move {
            let _ = req.send().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(http_url("libsql://db-org.turso.io").unwrap(), "https://db-org.turso.io");
        assert_eq!(http_url("ws://localhost:8080/").unwrap(), "http://localhost:8080");
        assert_eq!(http_url("db-org.turso.io?authToken=x").unwrap(), "https://db-org.turso.io");
        assert!(http_url("file:local.db").is_err() || http_url("file://local.db").is_err());
        assert!(http_url("").is_err());
        assert!(is_local("http://localhost:25880") && is_local("http://127.0.0.1:8080") && is_local("http://[::1]:8080"));
        assert!(is_local("http://192.168.1.4") && is_local("http://10.0.0.2:80") && is_local("http://nas.local:8080"));
        assert!(!is_local("https://db-org.turso.io") && !is_local("http://8.8.8.8:8080"));
    }

    #[test]
    fn values() {
        assert_eq!(cell(&json!({"type": "integer", "value": "42"})), json!(42));
        assert_eq!(cell(&json!({"type": "integer", "value": "9007199254740993"})), json!("9007199254740993"));
        assert_eq!(cell(&json!({"type": "float", "value": 1.5})), json!(1.5));
        assert_eq!(cell(&json!({"type": "text", "value": "a"})), json!("a"));
        assert_eq!(cell(&json!({"type": "blob", "base64": "AP8"})), json!("0x00FF"));
        assert_eq!(cell(&json!({"type": "null"})), Value::Null);
    }

    #[test]
    fn results() {
        let r = stmt_result(&json!({
            "cols": [{"name": "a", "decltype": "INTEGER"}, {"name": "b", "decltype": null}],
            "rows": [[{"type": "integer", "value": "1"}, {"type": "null"}]],
            "affected_row_count": 0
        }));
        assert_eq!(r.cols, vec![("a".into(), "INTEGER".into()), ("b".into(), String::new())]);
        assert_eq!(r.rows, vec![vec![json!(1), Value::Null]]);
        assert_eq!(error_message(&json!({"message": "SQLite error: no such table: x"})), "no such table: x");
        let e = StepError::from_value(&json!({"message": "SQLite error: UNIQUE constraint failed: t.id", "code": "SQLITE_CONSTRAINT"}));
        assert_eq!(e, StepError { message: "UNIQUE constraint failed: t.id".into(), code: Some("SQLITE_CONSTRAINT".into()) });
    }

    #[test]
    fn steps_open_a_transaction_only_where_asked() {
        let (steps, owner) = script_steps(&["select 1".into(), "insert into t values (1)".into()], &[false, true]);
        assert_eq!(owner, vec![(0, true), (1, false), (1, true)]);
        assert_eq!(steps[1]["stmt"]["sql"], "BEGIN");
        assert_eq!(steps[1]["condition"]["conds"][1]["type"], "is_autocommit");
        assert_eq!(steps[2]["condition"], json!({"type": "ok", "step": 0}));
        let (steps, _) = script_steps(&["insert into t values (1)".into()], &[true]);
        assert_eq!(steps[0]["condition"], json!({"type": "is_autocommit"}));
        assert!(steps[1].get("condition").is_none());
    }
}
