//! The REST client every module goes through (spec §4.2 "Rate limit and
//! backoff", §3.7). Owned by part B.
//!
//! - Paths only, relative to the cloud's API base ([`Api::rest_send`]) or
//!   to the Fabric API ([`Api::fabric_send`]): the host never comes from
//!   the caller, and each token goes only to its own host (`https`, default
//!   port). Redirects are off, so `Authorization` never follows one.
//! - A per-user sliding window shared by every session in the host, keyed
//!   by `(cloud, tenant, oid)`: 120 query calls per 60 s (40 for Arrow on
//!   Pro/PPU or an unknown SKU); [`Priority::Background`] calls never take
//!   the last 20 %. A wait over 1 s says so (`out.info`) and is
//!   cancellable.
//! - 429: `Retry-After` (seconds or HTTP date), else 2/4/8/16 s with jitter,
//!   at most 4 retries or 60 s; a `Retry-After` past the budget fails at
//!   once with the wait. 502/503/504: two retries for GETs and query POSTs.
//!   Writes (`POST …/refreshes`, `DELETE …/refreshes/…`, `PATCH`, the
//!   model's administration) retry only on 429, and a 429 on a refresh is
//!   the shared capacity's daily quota: reported, not retried.
//! - 401: renew the token once and retry; then `Error::AuthFailed`.
//! - Read-only connections: every non-GET request is refused before it's
//!   sent, except the reads that are POSTs (`executeQueries`,
//!   `executeDaxQueries`, Fabric `getDefinition` and its operation).
//! - Error bodies: at most 64 KB read; logs get only the status, the error
//!   code and the request id, never a body, a token or model data.

use crate::auth::cloud::Cloud;
use crate::auth::{Account, Audience, Endpoints, TokenProvider};
use crate::config::Settings;
use crate::Ctx;
use dbine_driver::{ConnectFailure, ConnectFailureKind, Error, Result};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::watch;

/// Who's waiting for a call: the editor goes first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    /// Editor runs, refresh and administration actions.
    Editor,
    /// Explorer metadata, the function catalog, the monitor, MCP and the
    /// assistant's reads (S5).
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    fn http(self) -> reqwest::Method {
        match self {
            Method::Get => reqwest::Method::GET,
            Method::Post => reqwest::Method::POST,
            Method::Put => reqwest::Method::PUT,
            Method::Patch => reqwest::Method::PATCH,
            Method::Delete => reqwest::Method::DELETE,
        }
    }
}

/// A REST answer.
#[derive(Debug, Clone, Default)]
pub struct RestResponse {
    pub status: u16,
    /// `Location`, as the service sent it (never followed here; a refresh's
    /// request id is parsed from it, a Fabric operation goes through
    /// [`Api::fabric_follow`]).
    pub location: Option<String>,
    /// `x-ms-request-id` / `RequestId`.
    pub request_id: Option<String>,
    pub retry_after: Option<Duration>,
    /// The JSON body (`Null` when empty).
    pub body: Value,
}

/// How much of the query window is used: what the monitor shows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WindowUsage {
    /// Query calls in the last 60 s.
    pub used: u32,
    /// 120, or 40 for Arrow on Pro/PPU (or an unknown SKU).
    pub limit: u32,
}

/// How a request may be retried and whether a read-only connection sends
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// GET.
    Read,
    /// `executeQueries`, `executeDaxQueries`, Fabric `getDefinition`: reads
    /// that are POSTs.
    Query,
    /// `POST …/refreshes`: never retried (a 429 is the daily quota).
    Refresh,
    /// Every other write: retried only on 429.
    Write,
}

fn kind_of(method: Method, path: &str) -> Kind {
    let path = path.split('?').next().unwrap_or("");
    match method {
        Method::Get => Kind::Read,
        Method::Post
            if path.ends_with("/executeQueries")
                || path.ends_with("/executeDaxQueries")
                || path.ends_with("/getDefinition") =>
        {
            Kind::Query
        }
        Method::Post if path.ends_with("/refreshes") => Kind::Refresh,
        _ => Kind::Write,
    }
}

/// Most of an error body read.
const ERROR_BODY_MAX: usize = 64 * 1024;
/// Most of a REST answer read (a model's TMSL definition is the largest).
const REST_BODY_MAX: usize = 64 * 1024 * 1024;
/// The 429 retries' budget.
const RETRY_BUDGET: Duration = Duration::from_secs(60);
const MAX_429_RETRIES: u32 = 4;

/// The session's client: tokens, the rate window and cancellation.
#[derive(Debug)]
pub struct Api {
    pub tokens: TokenProvider,
    pub read_only: bool,
    http: reqwest::Client,
    endpoints: Endpoints,
    cloud: Cloud,
    /// Bumped by the interrupter: requests and waits in flight stop.
    cancel: Arc<watch::Sender<u64>>,
    /// Arrow runs at the full 120 per minute: the capacity's SKU is known
    /// and isn't Premium Per User (G4: unknown → 40).
    arrow_full_rate: AtomicBool,
    /// The last query call counted against the 40 per minute.
    last_arrow_limited: AtomicBool,
    query_timeout: Duration,
}

/// A request that came back without success, for the caller to read.
#[derive(Debug, Clone)]
pub struct Failure {
    pub status: u16,
    /// At most 64 KB of it, parsed (`Null` when it isn't JSON).
    pub body: Value,
    pub request_id: Option<String>,
}

/// What a query POST answered.
#[derive(Debug)]
pub enum QueryReply {
    /// 2xx: the body, read as it arrives.
    Stream(QueryStream),
    /// Anything the query module maps itself (a DAX error, the Arrow API
    /// not being available…).
    Failed(Failure),
}

/// A query's answer body, chunk by chunk; a chunk waits for the
/// interrupter too.
#[derive(Debug)]
pub struct QueryStream {
    resp: reqwest::Response,
    rx: watch::Receiver<u64>,
    pub content_type: Option<String>,
    /// Bytes read so far.
    pub read: usize,
}

impl QueryStream {
    /// The next chunk; `None` at the end.
    pub async fn chunk(&mut self) -> Result<Option<arrow_buffer::Buffer>> {
        let next = tokio::select! {
            r = self.resp.chunk() => r,
            _ = self.rx.changed() => return Err(Error::Cancelled),
        };
        match next {
            Ok(Some(b)) => {
                self.read += b.len();
                Ok(Some(arrow_buffer::Buffer::from(b)))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(transport_error(&e, "leer la respuesta")),
        }
    }

    /// The whole body (bounded by `max` bytes: past it, the rest is left
    /// unread and `true` says so).
    pub async fn bytes(&mut self, max: usize) -> Result<(Vec<u8>, bool)> {
        let mut out = Vec::new();
        while let Some(c) = self.chunk().await? {
            out.extend_from_slice(&c);
            if out.len() > max {
                return Ok((out, true));
            }
        }
        Ok((out, false))
    }
}

/// A network failure as the driver's error (never with a token: reqwest's
/// errors name the URL, which holds no secret).
fn transport_error(e: &reqwest::Error, doing: &str) -> Error {
    if e.is_timeout() {
        return Error::connect_failed(ConnectFailure::new(
            ConnectFailureKind::Timeout,
            format!("Power BI no respondió a tiempo al {doing}"),
        ));
    }
    let mut f = ConnectFailure::from_error(e);
    f.message = format!("no se pudo {doing} de Power BI: {}", f.message);
    Error::connect_failed(f)
}

fn header(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// `Retry-After`: seconds, or an HTTP date.
pub(crate) fn retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let v = value.trim();
    if let Ok(s) = v.parse::<u64>() {
        return Some(Duration::from_secs(s));
    }
    let at = chrono::DateTime::parse_from_rfc2822(v).ok()?;
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(u64::try_from(at.timestamp()).ok()?);
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

/// 2/4/8/16 s for the `attempt`-th retry, ±25 %.
fn backoff(attempt: u32) -> Duration {
    let base = 2u64 << attempt.min(3);
    let mut r = [0u8; 2];
    let _ = getrandom::getrandom(&mut r);
    let jitter = f64::from(u16::from_le_bytes(r)) / f64::from(u16::MAX) * 0.5 - 0.25;
    Duration::from_millis(((base as f64) * 1000.0 * (1.0 + jitter)) as u64)
}

/// The service's error code and message, from the shapes Power BI and
/// Fabric answer (`{"error":{"code","message","pbi.error"}}`,
/// `{"errorCode","message"}`).
pub(crate) fn error_parts(body: &Value) -> (Option<String>, Option<String>) {
    let s = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);
    let e = body.get("error").unwrap_or(body);
    let code = s(e.get("code"))
        .or_else(|| s(body.get("errorCode")))
        .or_else(|| s(e.get("pbi.error").and_then(|p| p.get("code"))));
    let message = s(e.get("message"))
        .or_else(|| s(body.get("message")))
        .or_else(|| {
            e.get("pbi.error")
                .and_then(|p| p.get("details"))
                .and_then(Value::as_array)
                .and_then(|d| {
                    d.iter()
                        .find(|x| x.get("code").and_then(Value::as_str) == Some("DetailsMessage"))
                })
                .and_then(|x| s(x.get("detail").and_then(|d| d.get("value"))))
        });
    (code, message.map(|m| crate::auth::short(&m, 500)))
}

/// A failed request as the driver's error (spec §4.2's table).
pub(crate) fn error_for(f: &Failure, query: bool) -> Error {
    let (code, message) = error_parts(&f.body);
    tracing::debug!(
        status = f.status,
        code = code.as_deref().unwrap_or(""),
        request_id = f.request_id.as_deref().unwrap_or(""),
        "Power BI request failed"
    );
    let tag = match &code {
        Some(c) => format!("HTTP {}, {c}", f.status),
        None => format!("HTTP {}", f.status),
    };
    let detail = message.map(|m| format!(": {m}")).unwrap_or_default();
    match f.status {
        401 => Error::AuthFailed(format!(
            "Power BI no aceptó el token ({tag}): volvé a iniciar sesión."
        )),
        403 if query => Error::Query(format!(
            "Power BI rechazó la consulta ({tag}){detail}. Revisá que tengas permiso de compilación (Build) en el modelo y acceso a su área de trabajo, que el inquilino tenga activada «Semantic Model Execute Queries REST API» y, con una entidad de servicio, que el modelo no tenga RLS ni SSO."
        )),
        403 => Error::Query(format!(
            "Power BI rechazó la solicitud ({tag}){detail}: tu cuenta no tiene permiso para esto."
        )),
        404 => Error::Query(
            "No se encontró el modelo o el área de trabajo, o tu cuenta no tiene acceso".into(),
        ),
        429 => Error::Query(format!(
            "Power BI está limitando las solicitudes ({tag}): volvé a intentar en un minuto."
        )),
        500..=599 => Error::Query(format!(
            "Power BI no pudo responder ahora ({tag}){detail}; volvé a intentar en unos minutos."
        )),
        _ => Error::Query(format!("Power BI rechazó la solicitud ({tag}){detail}")),
    }
}

/// `path` under `base`: a path only (`/…`), no scheme, host, `..`,
/// fragment, backslash or control character, and the result on the
/// base's own origin (S3).
fn url_for(base: &str, path: &str) -> Result<reqwest::Url> {
    let bad = || Error::Query("ruta de la API no válida".into());
    let (p, _) = path.split_once('?').unwrap_or((path, ""));
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains("://")
        || path.contains('\\')
        || path.contains('#')
        || path.chars().any(char::is_control)
        || p.to_ascii_lowercase().contains("%2e")
        || p.split('/').any(|seg| seg == ".." || seg == ".")
    {
        return Err(bad());
    }
    let base_url = reqwest::Url::parse(base).map_err(|_| bad())?;
    let url = reqwest::Url::parse(&format!("{base}{path}")).map_err(|_| bad())?;
    if !same_origin(&url, &base_url) || !url.path().starts_with(base_url.path()) {
        return Err(bad());
    }
    Ok(url)
}

fn same_origin(a: &reqwest::Url, b: &reqwest::Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str().is_some()
        && a.host_str() == b.host_str()
        && a.port() == b.port()
        && a.username().is_empty()
        && a.password().is_none()
}

// ------------------------------------------------------------ the window

/// Query calls in the last minute, per user.
#[derive(Debug, Default)]
struct Window {
    /// When each call went, and whether it counted against the 40.
    calls: VecDeque<(Instant, bool)>,
}

const WINDOW: Duration = Duration::from_secs(60);
const FULL_RATE: usize = 120;
const ARROW_PRO_RATE: usize = 40;

impl Window {
    fn prune(&mut self, now: Instant) {
        while self
            .calls
            .front()
            .is_some_and(|(t, _)| now.duration_since(*t) >= WINDOW)
        {
            self.calls.pop_front();
        }
    }

    /// Take a slot now, or how long until one frees.
    fn admit(
        &mut self,
        now: Instant,
        limited: bool,
        priority: Priority,
    ) -> std::result::Result<(), Duration> {
        self.prune(now);
        // Background calls leave the last 20 % to the editor.
        let cap = |n: usize| match priority {
            Priority::Editor => n,
            Priority::Background => n * 4 / 5,
        };
        let total_cap = cap(FULL_RATE);
        let arrow_cap = cap(ARROW_PRO_RATE);
        let arrow: Vec<Instant> = self.calls.iter().filter(|c| c.1).map(|c| c.0).collect();
        let mut wait = Duration::ZERO;
        if self.calls.len() >= total_cap {
            let t = self.calls[self.calls.len() - total_cap].0;
            wait = wait.max(WINDOW.saturating_sub(now.duration_since(t)));
        }
        if limited && arrow.len() >= arrow_cap {
            let t = arrow[arrow.len() - arrow_cap];
            wait = wait.max(WINDOW.saturating_sub(now.duration_since(t)));
        }
        if wait.is_zero() {
            self.calls.push_back((now, limited));
            Ok(())
        } else {
            Err(wait + Duration::from_millis(10))
        }
    }

    fn used(&mut self, now: Instant) -> u32 {
        self.prune(now);
        self.calls.len() as u32
    }
}

static WINDOWS: LazyLock<Mutex<HashMap<String, Window>>> = LazyLock::new(Mutex::default);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Api {
    /// No network.
    pub fn new(settings: &Settings) -> Result<Api> {
        Self::with_endpoints(settings, Endpoints::of(settings.cloud))
    }

    pub(crate) fn with_endpoints(settings: &Settings, endpoints: Endpoints) -> Result<Api> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            // A query may take its whole timeout before the first byte.
            .read_timeout(settings.query_timeout + Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("DBine/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::Connect(e.to_string()))?;
        Ok(Api {
            tokens: TokenProvider::with_endpoints(settings, endpoints.clone()),
            read_only: settings.read_only,
            http,
            endpoints,
            cloud: settings.cloud,
            cancel: Arc::new(watch::channel(0).0),
            arrow_full_rate: AtomicBool::new(false),
            last_arrow_limited: AtomicBool::new(false),
            query_timeout: settings.query_timeout,
        })
    }

    /// A client of its own for the same connection (the same endpoints and
    /// sign-in: the host-wide token cache), for work that outlives the
    /// session's request (a listing refreshed in the background).
    pub(crate) fn sibling(&self, settings: &Settings) -> Result<Api> {
        Self::with_endpoints(settings, self.endpoints.clone())
    }

    /// The signed-in account (owner checks: `configuredBy`).
    pub fn account(&self) -> Option<Account> {
        self.tokens.account()
    }

    /// The model's capacity SKU, once known: Arrow runs at 120 per minute
    /// on a known capacity that isn't Premium Per User (`PP…`), at 40
    /// otherwise (G4).
    pub fn set_capacity_sku(&self, sku: Option<&str>) {
        let full =
            sku.is_some_and(|s| !s.trim().is_empty() && !s.to_ascii_uppercase().starts_with("PP"));
        self.arrow_full_rate.store(full, Ordering::Relaxed);
    }

    fn window_key(&self) -> String {
        match self.account() {
            Some(a) => format!("{:?}|{}|{}", self.cloud, a.tenant_id, a.object_id),
            None => format!("{:?}||", self.cloud),
        }
    }

    async fn cancellable<F: Future>(
        &self,
        rx: &mut watch::Receiver<u64>,
        f: F,
    ) -> Result<F::Output> {
        tokio::select! {
            r = f => Ok(r),
            _ = rx.changed() => Err(Error::Cancelled),
        }
    }

    /// Send with the token for `audience`, retrying as the module docs say.
    #[allow(clippy::too_many_arguments)]
    async fn send(
        &self,
        audience: Audience,
        method: Method,
        url: reqwest::Url,
        body: Option<&Value>,
        kind: Kind,
        timeout: Option<Duration>,
        rx: &mut watch::Receiver<u64>,
    ) -> Result<std::result::Result<reqwest::Response, Failure>> {
        if self.read_only && !matches!(kind, Kind::Read | Kind::Query) {
            return Err(Error::Query(
                "La conexión es de solo lectura: no se cambia nada en Power BI.".into(),
            ));
        }
        let mut renewed = false;
        let mut attempts_429 = 0u32;
        let mut attempts_5xx = 0u32;
        let mut waited = Duration::ZERO;
        loop {
            let token = if renewed {
                self.tokens.token(audience).await?
            } else {
                self.cancellable(rx, self.tokens.token(audience)).await??
            };
            let mut req = self
                .http
                .request(method.http(), url.clone())
                .bearer_auth(token.token.expose())
                .header(
                    "Accept",
                    "application/json, application/vnd.apache.arrow.stream",
                );
            if let Some(b) = body {
                req = req.json(b);
            } else if method != Method::Get {
                // An empty body: `Content-Length: 0` (POST / DELETE with
                // nothing to send).
                req = req.body("");
            }
            if let Some(t) = timeout {
                req = req.timeout(t);
            }
            let resp = self
                .cancellable(rx, req.send())
                .await?
                .map_err(|e| transport_error(&e, "conectar"))?;
            let status = resp.status().as_u16();
            if (200..300).contains(&status) {
                return Ok(Ok(resp));
            }
            if status == 401 && !renewed {
                renewed = true;
                self.cancellable(rx, self.tokens.renew(audience)).await??;
                continue;
            }
            if status == 429 && kind != Kind::Refresh {
                let wait = header(&resp, "Retry-After")
                    .and_then(|v| retry_after(&v, SystemTime::now()))
                    .unwrap_or_else(|| backoff(attempts_429));
                if attempts_429 < MAX_429_RETRIES && waited + wait <= RETRY_BUDGET {
                    attempts_429 += 1;
                    waited += wait;
                    self.cancellable(rx, tokio::time::sleep(wait)).await?;
                    continue;
                }
                return Err(Error::Query(format!(
                    "Power BI está limitando las solicitudes (HTTP 429): volvé a intentar en {} s.",
                    wait.as_secs().max(1)
                )));
            }
            if status == 429 {
                return Err(Error::Query(
                    "Power BI no aceptó la actualización (HTTP 429). En capacidad compartida el límite es de 8 actualizaciones por día, contando las programadas.".into(),
                ));
            }
            if matches!(status, 502..=504)
                && matches!(kind, Kind::Read | Kind::Query)
                && attempts_5xx < 2
            {
                let wait = backoff(attempts_5xx);
                attempts_5xx += 1;
                self.cancellable(rx, tokio::time::sleep(wait)).await?;
                continue;
            }
            let request_id =
                header(&resp, "RequestId").or_else(|| header(&resp, "x-ms-request-id"));
            let mut bytes = Vec::new();
            let mut resp = resp;
            while bytes.len() < ERROR_BODY_MAX {
                match self.cancellable(rx, resp.chunk()).await? {
                    Ok(Some(c)) => bytes.extend_from_slice(&c),
                    _ => break,
                }
            }
            bytes.truncate(ERROR_BODY_MAX);
            return Ok(Err(Failure {
                status,
                body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
                request_id,
            }));
        }
    }

    async fn rest(
        &self,
        audience: Audience,
        method: Method,
        url: reqwest::Url,
        body: Option<&Value>,
        kind: Kind,
    ) -> Result<RestResponse> {
        let mut rx = self.cancel.subscribe();
        let timeout = Some(Duration::from_secs(120));
        let resp = match self
            .send(audience, method, url, body, kind, timeout, &mut rx)
            .await?
        {
            Ok(r) => r,
            Err(f) => return Err(error_for(&f, kind == Kind::Query)),
        };
        let status = resp.status().as_u16();
        let location = header(&resp, "Location");
        let request_id = header(&resp, "RequestId").or_else(|| header(&resp, "x-ms-request-id"));
        let retry = header(&resp, "Retry-After").and_then(|v| retry_after(&v, SystemTime::now()));
        let mut resp = resp;
        let mut bytes = Vec::new();
        loop {
            match self.cancellable(&mut rx, resp.chunk()).await? {
                Ok(Some(c)) => {
                    bytes.extend_from_slice(&c);
                    if bytes.len() > REST_BODY_MAX {
                        return Err(Error::Query(
                            "La respuesta de Power BI es demasiado grande".into(),
                        ));
                    }
                }
                Ok(None) => break,
                Err(e) => return Err(transport_error(&e, "leer la respuesta")),
            }
        }
        let body = if bytes.iter().all(u8::is_ascii_whitespace) {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .map_err(|_| Error::Query("Power BI respondió algo que no es JSON".into()))?
        };
        Ok(RestResponse {
            status,
            location,
            request_id,
            retry_after: retry,
            body,
        })
    }

    /// `GET {api base}{path}`: the JSON body, or the error mapped (403 with
    /// its likely cause, 404 "no se encontró…", 429 after the budget…).
    pub async fn rest_get(&self, path: &str, priority: Priority) -> Result<Value> {
        Ok(self
            .rest_send(Method::Get, path, None, priority)
            .await?
            .body)
    }

    /// Any method on `{api base}{path}` with a JSON body. Writes are
    /// refused on read-only connections before anything is sent.
    pub async fn rest_send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        priority: Priority,
    ) -> Result<RestResponse> {
        let _ = priority;
        let url = url_for(&self.endpoints.api, path)?;
        self.rest(Audience::PowerBi, method, url, body, kind_of(method, path))
            .await
    }

    /// Any method on `https://api.fabric.microsoft.com/v1{path}` with the
    /// Fabric token (public cloud only). Same rules as
    /// [`Api::rest_send`]; `POST …/getDefinition` is a read.
    pub async fn fabric_send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        priority: Priority,
    ) -> Result<RestResponse> {
        let _ = priority;
        let base = self.endpoints.fabric.as_deref().ok_or_else(|| {
            Error::Unsupported("La API de Fabric solo está disponible en la nube pública.".into())
        })?;
        let url = url_for(base, path)?;
        self.rest(Audience::Fabric, method, url, body, kind_of(method, path))
            .await
    }

    /// `GET` a Fabric long-running operation's `Location`: only `https`
    /// on `api.fabric.microsoft.com`, default port (S3); anything else is
    /// refused without a request.
    pub async fn fabric_follow(&self, location: &str, priority: Priority) -> Result<RestResponse> {
        let _ = priority;
        let base = self.endpoints.fabric.as_deref().ok_or_else(|| {
            Error::Unsupported("La API de Fabric solo está disponible en la nube pública.".into())
        })?;
        let url = fabric_location(base, location)?;
        self.rest(Audience::Fabric, Method::Get, url, None, Kind::Read)
            .await
    }

    /// `POST {api base}{path}` of a query (`executeQueries` /
    /// `executeDaxQueries`), its answer read as it arrives. Doesn't take a
    /// slot of the window: [`Api::query_slot`] does, before.
    pub async fn post_query(
        &self,
        path: &str,
        body: &Value,
        timeout: Option<Duration>,
    ) -> Result<QueryReply> {
        let url = url_for(&self.endpoints.api, path)?;
        if kind_of(Method::Post, path) != Kind::Query {
            return Err(Error::Query("ruta de la API no válida".into()));
        }
        let mut rx = self.cancel.subscribe();
        match self
            .send(
                Audience::PowerBi,
                Method::Post,
                url,
                Some(body),
                Kind::Query,
                timeout,
                &mut rx,
            )
            .await?
        {
            Ok(resp) => Ok(QueryReply::Stream(QueryStream {
                content_type: header(&resp, "Content-Type"),
                resp,
                rx,
                read: 0,
            })),
            Err(f) => Ok(QueryReply::Failed(f)),
        }
    }

    /// The query timeout of the connection.
    pub fn query_timeout(&self) -> Duration {
        self.query_timeout
    }

    /// Take a slot of the query window (the query module calls it before
    /// each `executeQueries` / `executeDaxQueries`).
    pub async fn query_slot(&self, arrow: bool, priority: Priority) -> Result<()> {
        self.query_slot_notify(arrow, priority, &mut |_| {}).await
    }

    /// [`Api::query_slot`], calling `notify` once with the wait when it's
    /// longer than a second (the editor says so).
    pub async fn query_slot_notify(
        &self,
        arrow: bool,
        priority: Priority,
        notify: &mut (dyn FnMut(Duration) + Send),
    ) -> Result<()> {
        let limited = arrow && !self.arrow_full_rate.load(Ordering::Relaxed);
        self.last_arrow_limited.store(limited, Ordering::Relaxed);
        let mut rx = self.cancel.subscribe();
        // The window is the signed-in user's: sign in first.
        self.cancellable(&mut rx, self.tokens.token(Audience::PowerBi))
            .await??;
        let key = self.window_key();
        let mut told = false;
        loop {
            let admitted = lock(&WINDOWS).entry(key.clone()).or_default().admit(
                Instant::now(),
                limited,
                priority,
            );
            match admitted {
                Ok(()) => return Ok(()),
                Err(wait) => {
                    if !told && wait > Duration::from_secs(1) {
                        told = true;
                        notify(wait);
                    }
                    self.cancellable(&mut rx, tokio::time::sleep(wait)).await?;
                }
            }
        }
    }

    /// Query calls per minute this connection gets on one API.
    pub fn query_rate(&self, arrow: bool) -> u32 {
        if arrow && !self.arrow_full_rate.load(Ordering::Relaxed) {
            ARROW_PRO_RATE as u32
        } else {
            FULL_RATE as u32
        }
    }

    pub fn window_usage(&self) -> WindowUsage {
        let used = lock(&WINDOWS)
            .entry(self.window_key())
            .or_default()
            .used(Instant::now());
        WindowUsage {
            used,
            limit: if self.last_arrow_limited.load(Ordering::Relaxed) {
                ARROW_PRO_RATE as u32
            } else {
                FULL_RATE as u32
            },
        }
    }
}

/// A Fabric operation's `Location`, accepted only on the Fabric API's own
/// origin (S3).
fn fabric_location(base: &str, location: &str) -> Result<reqwest::Url> {
    let bad =
        || Error::Query("Fabric respondió con una dirección de seguimiento inesperada".into());
    let base_url = reqwest::Url::parse(base).map_err(|_| bad())?;
    let url = reqwest::Url::parse(location.trim()).map_err(|_| bad())?;
    if !same_origin(&url, &base_url) || url.fragment().is_some() {
        return Err(bad());
    }
    Ok(url)
}

/// Stops the request in flight and any wait for a rate-limit slot (spec
/// §4.2 "Cancel"): the tab stops at once; the service finishes the query.
pub fn interrupter(cx: &Ctx) -> Option<Arc<dyn Fn() + Send + Sync>> {
    let cancel = cx.api.cancel.clone();
    Some(Arc::new(move || {
        cancel.send_modify(|g| *g = g.wrapping_add(1));
    }))
}

/// The fake server and contexts the tests use (no tenant: every answer is
/// shaped like Microsoft Learn's samples).
#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::auth::tests::jwt;
    use dbine_driver::ConnectionConfig;
    use std::sync::atomic::AtomicU64;

    #[derive(Debug, Clone)]
    pub(crate) struct Request {
        pub method: String,
        /// Path and query, as sent.
        pub path: String,
        /// Header names lowercased.
        pub headers: HashMap<String, String>,
        pub body: String,
    }

    #[derive(Debug, Clone)]
    pub(crate) struct Reply {
        pub status: u16,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
        /// Answer only after this long.
        pub delay: Option<Duration>,
    }

    impl Reply {
        pub fn json(status: u16, body: &str) -> Reply {
            Reply {
                status,
                headers: vec![("Content-Type".into(), "application/json".into())],
                body: body.as_bytes().to_vec(),
                delay: None,
            }
        }
        pub fn arrow(body: Vec<u8>) -> Reply {
            Reply {
                status: 200,
                headers: vec![(
                    "Content-Type".into(),
                    "application/vnd.apache.arrow.stream".into(),
                )],
                body,
                delay: None,
            }
        }
        pub fn header(mut self, k: &str, v: &str) -> Reply {
            self.headers.push((k.into(), v.into()));
            self
        }
        pub fn after(mut self, d: Duration) -> Reply {
            self.delay = Some(d);
            self
        }
    }

    pub(crate) type Handler = Arc<dyn Fn(&Request) -> Reply + Send + Sync>;

    /// A tiny HTTP/1.1 server: one request per connection, answered by `h`.
    pub(crate) async fn serve(h: Handler) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = l.accept().await {
                let h = h.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = vec![0u8; 1 << 16];
                    let (head_end, len) = loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..p]).to_ascii_lowercase();
                            let len = head
                                .lines()
                                .find_map(|l| {
                                    l.strip_prefix("content-length:")
                                        .and_then(|v| v.trim().parse().ok())
                                })
                                .unwrap_or(0);
                            break (p + 4, len);
                        }
                    };
                    while buf.len() < head_end + len {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let mut lines = head.lines();
                    let mut first = lines.next().unwrap_or("").split(' ');
                    let method = first.next().unwrap_or("").to_string();
                    let path = first.next().unwrap_or("").to_string();
                    let headers = lines
                        .filter_map(|l| l.split_once(':'))
                        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
                        .collect();
                    let body = String::from_utf8_lossy(&buf[head_end..head_end + len]).to_string();
                    let reply = h(&Request {
                        method,
                        path,
                        headers,
                        body,
                    });
                    if let Some(d) = reply.delay {
                        tokio::time::sleep(d).await;
                    }
                    let mut resp = format!("HTTP/1.1 {} X\r\n", reply.status);
                    for (k, v) in &reply.headers {
                        resp.push_str(&format!("{k}: {v}\r\n"));
                    }
                    resp.push_str(&format!(
                        "content-length: {}\r\nconnection: close\r\n\r\n",
                        reply.body.len()
                    ));
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.write_all(&reply.body).await;
                });
            }
        });
        format!("http://{addr}")
    }

    static NEXT: AtomicU64 = AtomicU64::new(1);

    /// A token for the public cloud's Power BI resource, of an account of
    /// its own (the host-wide caches never mix two tests).
    pub(crate) fn token() -> String {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        jwt(&serde_json::json!({
            "aud": "https://analysis.windows.net/powerbi/api",
            "tid": "11111111-1111-1111-1111-111111111111",
            "oid": format!("00000000-0000-0000-0000-{n:012}"),
            "upn": format!("user{n}@contoso.com"),
            "exp": crate::auth::now() + 3600,
        }))
    }

    /// Settings of a pasted-token connection plus `opts`.
    pub(crate) fn settings(opts: &[(&str, &str)], read_only: bool) -> Settings {
        let token = token();
        let mut all: Vec<(&str, &str)> = vec![("auth_mode", "token"), ("access_token", &token)];
        all.extend_from_slice(opts);
        let cfg = ConnectionConfig {
            driver: "powerbi".into(),
            read_only,
            options: all
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        };
        Settings::from_config(&cfg).unwrap()
    }

    /// A session context on the fake server at `base`.
    pub(crate) fn ctx_at(base: &str, settings: Settings) -> Ctx {
        let api = Api::with_endpoints(&settings, Endpoints::test(base)).unwrap();
        Ctx {
            settings,
            api,
            model: None,
            query: crate::query::QueryState::default(),
            meta: crate::meta::MetaState::default(),
        }
    }

    /// A context with no server behind it (pure helpers).
    pub(crate) fn ctx(opts: &[(&str, &str)]) -> Ctx {
        ctx_at("http://127.0.0.1:9", settings(opts, true))
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn paths_stay_on_the_base() {
        let base = "https://api.powerbi.com/v1.0/myorg";
        assert_eq!(
            url_for(base, "/groups?$filter=name%20eq%20'x'")
                .unwrap()
                .as_str(),
            "https://api.powerbi.com/v1.0/myorg/groups?$filter=name%20eq%20%27x%27"
        );
        for bad in [
            "groups",
            "//evil.example/x",
            "/../../x",
            "/groups/%2e%2e/x",
            "/x#frag",
            "/x\\y",
            "/x\ny",
            "@evil.example/x",
            "https://evil.example/x",
            "/./x",
        ] {
            assert!(url_for(base, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn fabric_locations_only_on_fabric() {
        let base = "https://api.fabric.microsoft.com/v1";
        assert!(fabric_location(base, "https://api.fabric.microsoft.com/v1/operations/x").is_ok());
        assert!(
            fabric_location(base, "https://api.fabric.microsoft.com:443/v1/operations/x").is_ok()
        );
        for bad in [
            "https://api.fabric.microsoft.com.example.net/v1/operations/x",
            "http://api.fabric.microsoft.com/v1/operations/x",
            "https://api.fabric.microsoft.com:8443/v1/operations/x",
            "https://user@api.fabric.microsoft.com/v1/operations/x",
            "https://api.powerbi.com/v1.0/myorg/x",
            "/v1/operations/x",
            "",
        ] {
            assert!(fabric_location(base, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn what_a_read_only_connection_may_send() {
        assert_eq!(kind_of(Method::Get, "/groups"), Kind::Read);
        assert_eq!(
            kind_of(Method::Post, "/groups/g/datasets/d/executeQueries"),
            Kind::Query
        );
        assert_eq!(
            kind_of(Method::Post, "/datasets/d/executeDaxQueries"),
            Kind::Query
        );
        assert_eq!(
            kind_of(
                Method::Post,
                "/workspaces/g/semanticModels/d/getDefinition?format=TMSL"
            ),
            Kind::Query
        );
        assert_eq!(
            kind_of(Method::Post, "/groups/g/datasets/d/refreshes"),
            Kind::Refresh
        );
        assert_eq!(
            kind_of(Method::Delete, "/groups/g/datasets/d/refreshes/r"),
            Kind::Write
        );
        assert_eq!(
            kind_of(Method::Patch, "/groups/g/datasets/d/refreshSchedule"),
            Kind::Write
        );
        assert_eq!(
            kind_of(Method::Post, "/groups/g/datasets/d/executeQueries/x"),
            Kind::Write
        );
    }

    #[test]
    fn retry_after_seconds_and_dates() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(784_111_777);
        assert_eq!(retry_after("7", now), Some(Duration::from_secs(7)));
        // Sun, 06 Nov 1994 08:49:37 GMT = 784111777.
        assert_eq!(
            retry_after("Sun, 06 Nov 1994 08:50:07 GMT", now),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            retry_after("Sun, 06 Nov 1994 08:49:00 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(retry_after("soon", now), None);
        for a in 0..6 {
            let b = backoff(a).as_millis();
            let base = 2000u128 << a.min(3);
            assert!(b >= base * 3 / 4 && b <= base * 5 / 4, "{a}: {b}");
        }
    }

    #[test]
    fn the_window_counts_per_minute() {
        let mut w = Window::default();
        let t0 = Instant::now();
        for _ in 0..120 {
            assert!(w.admit(t0, false, Priority::Editor).is_ok());
        }
        // The 121st waits for the first to leave the minute.
        let wait = w
            .admit(t0 + Duration::from_secs(10), false, Priority::Editor)
            .unwrap_err();
        assert!(
            wait > Duration::from_secs(49) && wait <= Duration::from_secs(51),
            "{wait:?}"
        );
        assert!(w
            .admit(t0 + Duration::from_secs(60), false, Priority::Editor)
            .is_ok());
        // Arrow on Pro / PPU / an unknown SKU: 40.
        let mut w = Window::default();
        for _ in 0..40 {
            assert!(w.admit(t0, true, Priority::Editor).is_ok());
        }
        assert!(w.admit(t0, true, Priority::Editor).is_err());
        assert!(w.admit(t0, false, Priority::Editor).is_ok());
        // Background calls leave the last 20 % to the editor.
        let mut w = Window::default();
        for _ in 0..96 {
            assert!(w.admit(t0, false, Priority::Background).is_ok());
        }
        assert!(w.admit(t0, false, Priority::Background).is_err());
        assert!(w.admit(t0, false, Priority::Editor).is_ok());
        assert_eq!(w.used(t0), 97);
        assert_eq!(w.used(t0 + WINDOW), 0);
    }

    #[test]
    fn errors_say_why_and_never_quote_the_body_into_logs() {
        let f = |status: u16, body: Value| Failure {
            status,
            body,
            request_id: Some("rid".into()),
        };
        let e = error_for(&f(404, json!({"error": {"code": "ItemNotFound"}})), false);
        assert_eq!(
            e.to_string(),
            "No se encontró el modelo o el área de trabajo, o tu cuenta no tiene acceso"
        );
        let e = error_for(
            &f(
                403,
                json!({"error": {"code": "PowerBINotAuthorizedException"}}),
            ),
            true,
        );
        assert!(
            e.to_string().contains("Build")
                && e.to_string().contains("PowerBINotAuthorizedException")
        );
        let e = error_for(&f(401, Value::Null), false);
        assert!(matches!(e, Error::AuthFailed(_)));
        let e = error_for(
            &f(400, json!({"errorCode": "InvalidInput", "message": "bad"})),
            false,
        );
        assert_eq!(
            e.to_string(),
            "Power BI rechazó la solicitud (HTTP 400, InvalidInput): bad"
        );
        let (code, msg) = error_parts(
            &json!({"error":{"code":"DatasetExecuteQueriesError","pbi.error":{"code":"DatasetExecuteQueriesError","details":[{"code":"DetailsMessage","detail":{"type":1,"value":"Query (1, 10) bad"}}]}}}),
        );
        assert_eq!(code.as_deref(), Some("DatasetExecuteQueriesError"));
        assert_eq!(msg.as_deref(), Some("Query (1, 10) bad"));
    }

    #[tokio::test]
    async fn a_read_only_connection_sends_no_write() {
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let base = serve(Arc::new(move |_| {
            h.fetch_add(1, Ordering::SeqCst);
            Reply::json(200, "{}")
        }))
        .await;
        let cx = ctx_at(&base, settings(&[], true));
        let body = json!({"notifyOption": "NoNotification"});
        for (m, p) in [
            (Method::Post, "/groups/g/datasets/d/refreshes"),
            (Method::Delete, "/groups/g/datasets/d/refreshes/r"),
            (Method::Patch, "/groups/g/datasets/d/refreshSchedule"),
            (Method::Put, "/groups/g/datasets/d/users"),
            (Method::Delete, "/groups/g/datasets/d"),
        ] {
            let e = cx
                .api
                .rest_send(m, p, Some(&body), Priority::Editor)
                .await
                .unwrap_err();
            assert!(e.to_string().contains("solo lectura"), "{e}");
        }
        let e = cx
            .api
            .fabric_send(
                Method::Patch,
                "/workspaces/g/semanticModels/d",
                Some(&body),
                Priority::Editor,
            )
            .await
            .unwrap_err();
        assert!(e.to_string().contains("solo lectura"));
        assert_eq!(hits.load(Ordering::SeqCst), 0);
        // Reads that are POSTs do go.
        cx.api
            .rest_send(
                Method::Post,
                "/groups/g/datasets/d/executeQueries",
                Some(&body),
                Priority::Editor,
            )
            .await
            .unwrap();
        cx.api
            .rest_get("/groups", Priority::Background)
            .await
            .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn the_token_goes_only_to_its_host_and_redirects_arent_followed() {
        let seen = Arc::new(Mutex::new(Vec::<(String, Option<String>)>::new()));
        let log = seen.clone();
        let other = serve(Arc::new(move |req: &Request| {
            log.lock()
                .unwrap()
                .push((req.path.clone(), req.headers.get("authorization").cloned()));
            Reply::json(200, "{}")
        }))
        .await;
        let target = format!("{other}/stolen");
        let base = serve(Arc::new(move |_| {
            Reply::json(302, "").header("Location", &target)
        }))
        .await;
        let cx = ctx_at(&base, settings(&[], true));
        let e = cx
            .api
            .rest_get("/groups", Priority::Editor)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("302"), "{e}");
        assert!(seen.lock().unwrap().is_empty(), "the redirect was followed");
        // Fabric follow-ups to any other origin: refused, no request.
        let e = cx
            .api
            .fabric_follow(&format!("{other}/fabric/v1/operations/x"), Priority::Editor)
            .await
            .unwrap_err();
        assert!(e.to_string().contains("inesperada"), "{e}");
        assert!(seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn throttling_retries_with_retry_after_then_gives_up() {
        let n = Arc::new(AtomicUsize::new(0));
        let c = n.clone();
        let base = serve(Arc::new(move |req: &Request| {
            let i = c.fetch_add(1, Ordering::SeqCst);
            if req.path.ends_with("/refreshes") {
                return Reply::json(429, "{}").header("Retry-After", "0");
            }
            if req.path.starts_with("/v1.0/myorg/far") {
                return Reply::json(429, "{}").header("Retry-After", "3600");
            }
            match i {
                0 => Reply::json(429, "{}").header("Retry-After", "0"),
                1 => Reply::json(429, "{}").header("Retry-After", "Sun, 06 Nov 1994 08:49:37 GMT"),
                2 => Reply::json(503, "{}"),
                _ => Reply::json(200, r#"{"value":[]}"#),
            }
        }))
        .await;
        let cx = ctx_at(&base, settings(&[], false));
        let started = Instant::now();
        let v = cx.api.rest_get("/groups", Priority::Editor).await.unwrap();
        assert_eq!(v, json!({"value": []}));
        assert_eq!(n.load(Ordering::SeqCst), 4);
        assert!(started.elapsed() < Duration::from_secs(10));
        // Past the budget: at once, saying the wait.
        let started = Instant::now();
        let e = cx.api.rest_get("/far", Priority::Editor).await.unwrap_err();
        assert!(e.to_string().contains("3600 s"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(5));
        // A refresh is never retried: the daily quota.
        let before = n.load(Ordering::SeqCst);
        let e = cx
            .api
            .rest_send(
                Method::Post,
                "/groups/g/datasets/d/refreshes",
                Some(&json!({})),
                Priority::Editor,
            )
            .await
            .unwrap_err();
        assert!(e.to_string().contains("8 actualizaciones por día"), "{e}");
        assert_eq!(n.load(Ordering::SeqCst), before + 1);
    }

    #[tokio::test]
    async fn writes_arent_retried_on_server_errors() {
        let n = Arc::new(AtomicUsize::new(0));
        let c = n.clone();
        let base = serve(Arc::new(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
            Reply::json(
                500,
                r#"{"error":{"code":"InternalError","message":"boom"}}"#,
            )
        }))
        .await;
        let cx = ctx_at(&base, settings(&[], false));
        let e = cx
            .api
            .rest_send(
                Method::Post,
                "/groups/g/datasets/d/refreshes",
                Some(&json!({})),
                Priority::Editor,
            )
            .await
            .unwrap_err();
        assert!(e.to_string().contains("InternalError"), "{e}");
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_401_renews_once_then_fails() {
        // A service principal against the fake token endpoint: each renewal
        // is a new token.
        let issued = Arc::new(AtomicUsize::new(0));
        let i = issued.clone();
        let calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = calls.clone();
        let base = serve(Arc::new(move |req: &Request| {
            if req.path.contains("/oauth2/v2.0/token") {
                let n = i.fetch_add(1, Ordering::SeqCst);
                let tok = crate::auth::tests::jwt(&json!({"tid": "22222222-2222-2222-2222-222222222222", "oid": "sp-401", "n": n}));
                return Reply::json(200, &format!(r#"{{"access_token":"{tok}","expires_in":3599}}"#));
            }
            let auth = req.headers.get("authorization").cloned().unwrap_or_default();
            log.lock().unwrap().push(auth);
            if req.path.ends_with("/once") && issued.load(Ordering::SeqCst) < 2 {
                return Reply::json(401, "{}");
            }
            if req.path.ends_with("/always") {
                return Reply::json(401, "{}");
            }
            Reply::json(200, "{}")
        }))
        .await;
        let cfg = dbine_driver::ConnectionConfig {
            driver: "powerbi".into(),
            read_only: true,
            options: [
                ("auth_mode", "entra_sp"),
                ("tenant", "22222222-2222-2222-2222-222222222222"),
                ("client_id", "sp-401-client"),
                ("client_secret", "s"),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            ..Default::default()
        };
        let cx = ctx_at(&base, Settings::from_config(&cfg).unwrap());
        cx.api.rest_get("/once", Priority::Editor).await.unwrap();
        let calls_once = calls.lock().unwrap().clone();
        assert_eq!(calls_once.len(), 2);
        assert_ne!(
            calls_once[0], calls_once[1],
            "the retry carries the new token"
        );
        let e = cx
            .api
            .rest_get("/always", Priority::Editor)
            .await
            .unwrap_err();
        assert!(matches!(e, Error::AuthFailed(_)), "{e}");
        assert_eq!(calls.lock().unwrap().len(), 4);
        // A token that names another tenant than the connection's is refused.
        let cfg2 = dbine_driver::ConnectionConfig {
            options: [
                ("auth_mode", "entra_sp"),
                ("tenant", "33333333-3333-3333-3333-333333333333"),
                ("client_id", "sp-401-client"),
                ("client_secret", "s"),
            ]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            ..cfg
        };
        let cx2 = ctx_at(&base, Settings::from_config(&cfg2).unwrap());
        let e = cx2.api.rest_get("/x", Priority::Editor).await.unwrap_err();
        assert!(e.to_string().contains("otro inquilino"), "{e}");
    }

    #[tokio::test]
    async fn the_interrupter_stops_a_request_and_a_wait() {
        let base = serve(Arc::new(|_| {
            Reply::json(200, "{}").after(Duration::from_millis(1500))
        }))
        .await;
        let cx = ctx_at(&base, settings(&[], true));
        let stop = interrupter(&cx).unwrap();
        let started = Instant::now();
        let run = cx.api.rest_get("/slow", Priority::Editor);
        tokio::pin!(run);
        tokio::select! {
            _ = &mut run => panic!("finished before the interrupt"),
            _ = tokio::time::sleep(Duration::from_millis(100)) => stop(),
        }
        assert!(matches!(run.await, Err(Error::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(1));
        // A wait for a slot: fill this account's window, then interrupt.
        let key = cx.api.window_key();
        {
            let mut w = lock(&WINDOWS);
            let win = w.entry(key).or_default();
            for _ in 0..120 {
                let _ = win.admit(Instant::now(), false, Priority::Editor);
            }
        }
        let told = Mutex::new(None);
        let mut notify = |d: Duration| *told.lock().unwrap() = Some(d);
        let wait = cx
            .api
            .query_slot_notify(false, Priority::Editor, &mut notify);
        tokio::pin!(wait);
        tokio::select! {
            _ = &mut wait => panic!("got a slot in a full window"),
            _ = tokio::time::sleep(Duration::from_millis(100)) => stop(),
        }
        assert!(matches!(wait.await, Err(Error::Cancelled)));
        assert!(told
            .lock()
            .unwrap()
            .is_some_and(|d: Duration| d > Duration::from_secs(50)));
        assert_eq!(
            cx.api.window_usage(),
            WindowUsage {
                used: 120,
                limit: 120
            }
        );
    }

    #[test]
    fn capacity_skus_set_the_arrow_rate() {
        let cx = ctx(&[]);
        for (sku, full) in [
            (None, false),
            (Some("PP3"), false),
            (Some("F64"), true),
            (Some("P1"), true),
            (Some(""), false),
        ] {
            cx.api.set_capacity_sku(sku);
            assert_eq!(
                cx.api.arrow_full_rate.load(Ordering::Relaxed),
                full,
                "{sku:?}"
            );
        }
    }
}
