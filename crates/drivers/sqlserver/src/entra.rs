//! Microsoft Entra ID sign-ins beyond a password, a service principal or
//! a pasted token, as SQL Server Management Studio offers them:
//!
//! - [`interactive`]: the browser sign-in (authorization code with PKCE),
//!   so accounts with MFA work.
//! - [`integrated`]: Integrated Windows Authentication against a federated
//!   (ADFS) tenant, as MSAL's `AcquireTokenByIntegratedWindowsAuth`.
//! - [`managed_identity`]: the identity of the Azure VM or service DBine
//!   runs on.
//! - [`default_chain`]: environment variables, managed identity, Azure CLI
//!   and Azure Developer CLI, like the Azure SDKs' `DefaultAzureCredential`.
//!
//! Each one ends in an access token for `https://database.windows.net/`.
//! Tokens are cached in this process's memory only (the driver host lives
//! as long as DBine): never on disk and never in the logs.

use crate::variant::{token_body, token_from, valid_tenant, SQL_CLIENT_APP, SQL_SCOPE};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use dbine_driver::{Error, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Browser sign-in (MFA).
pub const MFA: &str = "entra_mfa";
/// Integrated Windows Authentication against ADFS.
pub const INTEGRATED: &str = "entra_integrated";
/// Managed identity (Azure VM, App Service, Functions…).
pub const MSI: &str = "entra_msi";
/// The `DefaultAzureCredential`-like chain.
pub const DEFAULT: &str = "entra_default";

/// The resource, for the endpoints that take a resource instead of a scope.
const SQL_RESOURCE: &str = "https://database.windows.net/";
const LOGIN_HOST: &str = "https://login.microsoftonline.com";
/// Scopes for the interactive sign-in: SQL plus a refresh token.
const SQL_SCOPE_OFFLINE: &str = "https://database.windows.net/.default offline_access openid profile";

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn auth(msg: impl Into<String>) -> Error {
    Error::AuthFailed(msg.into())
}

/// The first line of a message, at most `max` characters.
fn short(msg: &str, max: usize) -> String {
    let line = msg.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    match line.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}

/// A number that may come as JSON number or string (`"3599"`).
fn number(v: Option<&Value>) -> Option<u64> {
    let v = v?;
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

// ------------------------------------------------------------ token cache

/// An access token, when it expires (Unix seconds) and, from the browser
/// or ADFS sign-ins, a refresh token.
#[derive(Clone)]
struct Token {
    access: String,
    expires_on: u64,
    refresh: Option<String>,
}

// Tokens never reach a log, not even through `{:?}`.
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token").field("expires_on", &self.expires_on).finish_non_exhaustive()
    }
}

impl Token {
    /// From an OAuth 2.0 token response (`expires_in` in seconds).
    fn from_oauth(body: &Value, now: u64) -> Result<Token> {
        let access = token_from(body)?;
        let expires_in = number(body.get("expires_in")).unwrap_or(3600);
        let refresh = body.get("refresh_token").and_then(Value::as_str).map(str::to_string);
        Ok(Token { access, expires_on: now + expires_in, refresh })
    }
}

/// Who the token is for: the method, tenant, client and user.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Key {
    method: &'static str,
    tenant: String,
    client: String,
    user: String,
}

impl Key {
    fn new(method: &'static str, tenant: &str, client: &str, user: &str) -> Key {
        Key { method, tenant: tenant.to_string(), client: client.to_string(), user: user.to_lowercase() }
    }
}

/// A cached token is reused until this long before it expires.
const MARGIN: u64 = 5 * 60;

#[derive(Debug, PartialEq)]
enum Cached {
    /// Still good: use it.
    Fresh(String),
    /// Expired or about to, with a refresh token to renew it.
    Refresh(String),
    Miss,
}

fn lookup(cache: &HashMap<Key, Token>, key: &Key, now: u64) -> Cached {
    match cache.get(key) {
        Some(t) if now + MARGIN < t.expires_on => Cached::Fresh(t.access.clone()),
        Some(Token { refresh: Some(r), .. }) => Cached::Refresh(r.clone()),
        _ => Cached::Miss,
    }
}

static CACHE: LazyLock<Mutex<HashMap<Key, Token>>> = LazyLock::new(Mutex::default);
/// One sign-in at a time: sessions that open together (explorer, editor)
/// wait for the first one and take its token instead of opening the
/// browser again.
static ACQUIRE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The cached token for `key`, or a new one from `acquire`, which gets the
/// refresh token when there's one.
async fn with_cache<F, Fut>(key: Key, acquire: F) -> Result<String>
where
    F: FnOnce(Option<String>) -> Fut,
    Fut: Future<Output = Result<Token>>,
{
    let _one = ACQUIRE.lock().await;
    let cached = lookup(&CACHE.lock().unwrap_or_else(|e| e.into_inner()), &key, now());
    let refresh = match cached {
        Cached::Fresh(t) => return Ok(t),
        Cached::Refresh(r) => Some(r),
        Cached::Miss => None,
    };
    let token = acquire(refresh).await?;
    let access = token.access.clone();
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(key, token);
    Ok(access)
}

/// A new token from a refresh token, or `None` (the caller signs in
/// again). Keeps the old refresh token when the answer has none.
async fn refreshed(tenant: &str, client: &str, refresh: Option<String>) -> Option<Token> {
    let refresh = refresh?;
    let form = [
        ("grant_type", "refresh_token"),
        ("client_id", client),
        ("refresh_token", refresh.as_str()),
        ("scope", SQL_SCOPE_OFFLINE),
    ];
    let body = token_body(tenant, &form).await.ok()?;
    match Token::from_oauth(&body, now()) {
        Ok(mut t) => {
            t.refresh.get_or_insert(refresh);
            Some(t)
        }
        Err(e) => {
            tracing::debug!("Entra ID refresh failed, signing in again: {e}");
            None
        }
    }
}

// ------------------------------------------------- interactive (browser)

/// Signs in through the system browser (authorization code with PKCE and
/// a loopback redirect), so accounts with MFA work.
pub async fn interactive(tenant: Option<&str>, client: Option<&str>, user: Option<&str>) -> Result<String> {
    let tenant = tenant.unwrap_or("organizations");
    valid_tenant(tenant)?;
    let client = client.unwrap_or(SQL_CLIENT_APP);
    let key = Key::new(MFA, tenant, client, user.unwrap_or(""));
    with_cache(key, |refresh| async move {
        if let Some(t) = refreshed(tenant, client, refresh).await {
            return Ok(t);
        }
        browser_sign_in(tenant, client, user).await
    })
    .await
}

/// How long the browser sign-in may take.
const BROWSER_WAIT: Duration = Duration::from_secs(5 * 60);

async fn browser_sign_in(tenant: &str, client: &str, user: Option<&str>) -> Result<Token> {
    let verifier = random_b64(32)?;
    let state = random_b64(16)?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| auth(format!("no se pudo esperar la respuesta del navegador: {e}")))?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://localhost:{port}");
    let url = authorize_url(tenant, client, &redirect, &state, &pkce_challenge(&verifier), user);
    open_browser(&url)?;
    let code = match tokio::time::timeout(BROWSER_WAIT, wait_for_code(&listener, &state)).await {
        Ok(code) => code?,
        Err(_) => return Err(auth("pasaron 5 minutos sin completar el inicio de sesión en el navegador; volvé a conectar")),
    };
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", client),
        ("code", code.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("code_verifier", verifier.as_str()),
        ("scope", SQL_SCOPE_OFFLINE),
    ];
    Token::from_oauth(&token_body(tenant, &form).await?, now())
}

/// `n` random bytes, base64url without padding.
fn random_b64(n: usize) -> Result<String> {
    let mut bytes = vec![0u8; n];
    getrandom::getrandom(&mut bytes).map_err(|e| auth(format!("no hay números aleatorios del sistema: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The PKCE S256 challenge for a verifier (RFC 7636).
fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn authorize_url(tenant: &str, client: &str, redirect: &str, state: &str, challenge: &str, user: Option<&str>) -> String {
    let mut url = reqwest::Url::parse(LOGIN_HOST).expect("constant URL");
    url.path_segments_mut().expect("https URL").extend([tenant, "oauth2", "v2.0", "authorize"]);
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", client)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect)
            .append_pair("response_mode", "query")
            .append_pair("scope", SQL_SCOPE_OFFLINE)
            .append_pair("state", state)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256");
        if let Some(user) = user {
            q.append_pair("login_hint", user);
        }
    }
    url.into()
}

/// The program and arguments that open `url` in the default browser.
/// Never a shell: the URL has `&` in it.
fn browser_argv(url: &str) -> (PathBuf, Vec<String>) {
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        (root.join("System32").join("rundll32.exe"), vec!["url.dll,FileProtocolHandler".into(), url.into()])
    } else if cfg!(target_os = "macos") {
        (PathBuf::from("/usr/bin/open"), vec![url.into()])
    } else {
        (PathBuf::from("xdg-open"), vec![url.into()])
    }
}

fn open_browser(url: &str) -> Result<()> {
    // Only the sign-in page, never something a form could slip in.
    if !url.starts_with("https://login.microsoftonline.com/") {
        return Err(auth("dirección de inicio de sesión inesperada"));
    }
    let (program, args) = browser_argv(url);
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| auth(format!("no se pudo abrir el navegador para iniciar sesión: {e}")))?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

/// What the browser brought to the loopback listener.
#[derive(Debug, PartialEq)]
enum Callback {
    Code(String),
    /// Entra ID answered with an error (the user cancelled, no consent…).
    Failed(String),
    /// A redirect with someone else's `state`: answered and ignored.
    BadState,
    /// Anything else (`/favicon.ico`…).
    Ignore,
}

/// Reads the request line of the browser's redirect (`GET /?code=…&state=…
/// HTTP/1.1`).
fn parse_callback(head: &str, state: &str) -> Callback {
    let mut parts = head.lines().next().unwrap_or("").split(' ');
    let (Some("GET"), Some(target)) = (parts.next(), parts.next()) else {
        return Callback::Ignore;
    };
    let Ok(url) = reqwest::Url::parse("http://localhost").and_then(|b| b.join(target)) else {
        return Callback::Ignore;
    };
    if url.path() != "/" {
        return Callback::Ignore;
    }
    let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
    if !q.contains_key("code") && !q.contains_key("error") {
        return Callback::Ignore;
    }
    if q.get("state").map(String::as_str) != Some(state) {
        return Callback::BadState;
    }
    if let Some(error) = q.get("error") {
        let desc = q.get("error_description").unwrap_or(error);
        return Callback::Failed(short(desc, 300));
    }
    Callback::Code(q["code"].clone())
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

fn page(status: &str, title: &str, text: &str) -> String {
    let body = format!(
        "<!doctype html><html lang=\"es\"><head><meta charset=\"utf-8\"><title>DBine</title></head>\
         <body style=\"font-family:system-ui,sans-serif;text-align:center;margin-top:15vh\">\
         <h2>{}</h2><p>{}</p></body></html>",
        html_escape(title),
        html_escape(text)
    );
    format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Answers the browser's requests until the redirect with this sign-in's
/// `state` arrives.
async fn wait_for_code(listener: &tokio::net::TcpListener, state: &str) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = Vec::with_capacity(2048);
        let read = tokio::time::timeout(Duration::from_secs(10), async {
            let mut chunk = [0u8; 2048];
            while buf.len() < 16 * 1024 && !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = stream.read(&mut chunk).await?;
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            std::io::Result::Ok(())
        })
        .await;
        if !matches!(read, Ok(Ok(()))) {
            continue;
        }
        let callback = parse_callback(&String::from_utf8_lossy(&buf), state);
        let answer = match &callback {
            Callback::Code(_) => page("200 OK", "Listo", "Ya iniciaste sesión: podés cerrar esta ventana y volver a DBine."),
            Callback::Failed(m) => page("200 OK", "No se pudo iniciar sesión", &format!("{m}. Podés cerrar esta ventana.")),
            Callback::BadState => page("400 Bad Request", "Respuesta inesperada", "Esta respuesta no corresponde al inicio de sesión de DBine."),
            Callback::Ignore => page("404 Not Found", "DBine", ""),
        };
        let _ = stream.write_all(answer.as_bytes()).await;
        let _ = stream.shutdown().await;
        match callback {
            Callback::Code(code) => return Ok(code),
            Callback::Failed(m) => return Err(auth(format!("Microsoft Entra ID rechazó el inicio de sesión: {m}"))),
            Callback::BadState | Callback::Ignore => {}
        }
    }
}

// ------------------------------------------------ managed identity (MSI)

const IMDS: &str = "http://169.254.169.254/metadata/identity/oauth2/token";
const NO_MSI: &str =
    "no se encontró una identidad administrada: esta opción solo funciona si DBine corre en una VM o servicio de Azure";

/// Signs in as the managed identity of the Azure VM or service DBine runs
/// on; `client` picks a user-assigned identity.
pub async fn managed_identity(client: Option<&str>) -> Result<String> {
    let key = Key::new(MSI, "", client.unwrap_or(""), "");
    with_cache(key, |_| msi_token(client, Duration::from_secs(2))).await
}

#[derive(Debug, PartialEq)]
struct MsiRequest {
    url: String,
    header: (&'static str, String),
    /// The VM's metadata service (IMDS), not App Service's endpoint.
    imds: bool,
}

/// The request for a token: App Service / Functions publish
/// `IDENTITY_ENDPOINT` and `IDENTITY_HEADER`; elsewhere, IMDS.
fn msi_request(app_service: Option<(String, String)>, client: Option<&str>) -> Result<MsiRequest> {
    let (base, version, header, imds) = match app_service {
        Some((endpoint, secret)) => (endpoint, "2019-08-01", ("X-IDENTITY-HEADER", secret), false),
        None => (IMDS.to_string(), "2018-02-01", ("Metadata", "true".to_string()), true),
    };
    let mut url = reqwest::Url::parse(&base).map_err(|_| auth("IDENTITY_ENDPOINT no es una dirección válida"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(auth("IDENTITY_ENDPOINT no es una dirección válida"));
    }
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("api-version", version).append_pair("resource", SQL_RESOURCE);
        if let Some(c) = client {
            q.append_pair("client_id", c);
        }
    }
    Ok(MsiRequest { url: url.into(), header, imds })
}

fn app_service_env() -> Option<(String, String)> {
    let endpoint = std::env::var("IDENTITY_ENDPOINT").ok().filter(|v| !v.is_empty())?;
    let secret = std::env::var("IDENTITY_HEADER").ok().filter(|v| !v.is_empty())?;
    Some((endpoint, secret))
}

/// Reads the managed identity endpoint's answer. `expires_on` is Unix
/// seconds (a string on both endpoints); `expires_in` is the fallback.
fn parse_msi(status: u16, body: &str, now: u64) -> Result<Token> {
    let json: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    if let Some(access) = json.get("access_token").and_then(Value::as_str) {
        let expires_on = number(json.get("expires_on"))
            .or_else(|| number(json.get("expires_in")).map(|s| now + s))
            .unwrap_or(now + 3600);
        return Ok(Token { access: access.to_string(), expires_on, refresh: None });
    }
    let desc = ["error_description", "message", "error"]
        .iter()
        .find_map(|k| json.get(k).and_then(Value::as_str))
        .map(|d| short(d, 200))
        .unwrap_or_else(|| format!("HTTP {status}"));
    Err(auth(format!("la identidad administrada no dio un token: {desc}")))
}

async fn msi_token(client: Option<&str>, connect: Duration) -> Result<Token> {
    let req = msi_request(app_service_env(), client)?;
    let http = reqwest::Client::builder()
        .connect_timeout(connect)
        .timeout(Duration::from_secs(15))
        // IMDS is link-local: a proxy can't reach it.
        .no_proxy()
        .build()
        .map_err(|e| Error::Connect(e.to_string()))?;
    let resp = http.get(&req.url).header(req.header.0, &req.header.1).send().await.map_err(|e| {
        if req.imds && (e.is_connect() || e.is_timeout()) {
            auth(NO_MSI)
        } else {
            auth(format!("no se pudo llegar a la identidad administrada: {e}"))
        }
    })?;
    let status = resp.status().as_u16();
    let body = resp.text().await.map_err(|e| Error::Connect(e.to_string()))?;
    parse_msi(status, &body, now())
}

// ------------------------------------------------- default (chain)

/// Tries, in order and without asking anything: the `AZURE_*` environment
/// variables (service principal), the managed identity, Azure CLI and
/// Azure Developer CLI. The first token wins.
pub async fn default_chain(tenant: Option<&str>, client: Option<&str>) -> Result<String> {
    if let Some(t) = tenant {
        valid_tenant(t)?;
    }
    let key = Key::new(DEFAULT, tenant.unwrap_or(""), client.unwrap_or(""), "");
    with_cache(key, |_| default_token(tenant, client)).await
}

async fn default_token(tenant: Option<&str>, client: Option<&str>) -> Result<Token> {
    let mut tried = vec![];
    match env_credential(|k| std::env::var(k).ok()).await {
        Ok(t) => return Ok(t),
        Err(e) => tried.push(("variables de entorno", e)),
    }
    match msi_token(client, Duration::from_secs(2)).await {
        Ok(t) => return Ok(t),
        Err(e) => tried.push(("identidad administrada", e)),
    }
    match az_cli(tenant).await {
        Ok(t) => return Ok(t),
        Err(e) => tried.push(("Azure CLI", e)),
    }
    match azd_cli(tenant).await {
        Ok(t) => return Ok(t),
        Err(e) => tried.push(("Azure Developer CLI", e)),
    }
    Err(chain_error(&tried))
}

/// One error that says what was tried and why each step failed.
fn chain_error(tried: &[(&str, Error)]) -> Error {
    let lines: Vec<String> = tried.iter().map(|(what, e)| format!("- {what}: {}", short(&e.to_string(), 200))).collect();
    auth(format!(
        "no se pudo obtener un token de Microsoft Entra ID con la autenticación predeterminada. Se probó:\n{}",
        lines.join("\n")
    ))
}

/// `AZURE_TENANT_ID`, `AZURE_CLIENT_ID` and `AZURE_CLIENT_SECRET`, all set.
fn env_triplet(get: impl Fn(&str) -> Option<String>) -> Result<(String, String, String)> {
    let get = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    match (get("AZURE_TENANT_ID"), get("AZURE_CLIENT_ID"), get("AZURE_CLIENT_SECRET")) {
        (Some(t), Some(c), Some(s)) => Ok((t, c, s)),
        _ => Err(auth("no están definidas AZURE_TENANT_ID, AZURE_CLIENT_ID y AZURE_CLIENT_SECRET")),
    }
}

async fn env_credential(get: impl Fn(&str) -> Option<String>) -> Result<Token> {
    let (tenant, client, secret) = env_triplet(get)?;
    let form = [
        ("grant_type", "client_credentials"),
        ("client_id", client.as_str()),
        ("client_secret", secret.as_str()),
        ("scope", SQL_SCOPE),
    ];
    Token::from_oauth(&token_body(&tenant, &form).await?, now())
}

/// How long a CLI may take (Azure CLI starts Python).
const CLI_WAIT: Duration = Duration::from_secs(30);

async fn az_cli(tenant: Option<&str>) -> Result<Token> {
    let mut args = vec!["account", "get-access-token", "--resource", SQL_RESOURCE, "--output", "json"];
    if let Some(t) = tenant {
        args.extend(["--tenant", t]);
    }
    let out = run_cli("az", &args, "az login").await?;
    parse_az(&out, &chrono::Local)
}

async fn azd_cli(tenant: Option<&str>) -> Result<Token> {
    let mut args = vec!["auth", "token", "--scope", SQL_SCOPE, "--output", "json"];
    if let Some(t) = tenant {
        args.extend(["--tenant-id", t]);
    }
    let out = run_cli("azd", &args, "azd auth login").await?;
    parse_azd(&out)
}

/// Azure CLI: `accessToken` plus `expires_on` (Unix seconds, newer
/// versions) or `expiresOn` (local time, `2026-10-09 15:04:05.000000`).
fn parse_az<Tz: chrono::TimeZone>(stdout: &str, local: &Tz) -> Result<Token> {
    let json: Value = serde_json::from_str(stdout).map_err(|_| auth("Azure CLI devolvió algo que no es JSON"))?;
    let access = json.get("accessToken").and_then(Value::as_str).ok_or_else(|| auth("Azure CLI no devolvió un token"))?;
    let expires_on = number(json.get("expires_on"))
        .or_else(|| {
            let s = json.get("expiresOn")?.as_str()?;
            let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f").ok()?;
            let at = local.from_local_datetime(&naive).earliest()?;
            u64::try_from(at.timestamp()).ok()
        })
        .ok_or_else(|| auth("Azure CLI no dijo cuándo vence el token"))?;
    Ok(Token { access: access.to_string(), expires_on, refresh: None })
}

/// Azure Developer CLI: `token` plus `expiresOn` (RFC 3339).
fn parse_azd(stdout: &str) -> Result<Token> {
    let json: Value = serde_json::from_str(stdout).map_err(|_| auth("Azure Developer CLI devolvió algo que no es JSON"))?;
    let access = json.get("token").and_then(Value::as_str).ok_or_else(|| auth("Azure Developer CLI no devolvió un token"))?;
    let expires_on = json
        .get("expiresOn")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .and_then(|d| u64::try_from(d.timestamp()).ok())
        .ok_or_else(|| auth("Azure Developer CLI no dijo cuándo vence el token"))?;
    Ok(Token { access: access.to_string(), expires_on, refresh: None })
}

/// Where a CLI may be: `PATH`, then the usual install folders (apps opened
/// from the Finder don't get the shell's `PATH`).
fn cli_candidates(name: &str, path: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let (files, fixed): (Vec<String>, &[&str]) = if cfg!(windows) {
        (
            vec![format!("{name}.cmd"), format!("{name}.exe")],
            &[
                r"C:\Program Files\Microsoft SDKs\Azure\CLI2\wbin",
                r"C:\Program Files (x86)\Microsoft SDKs\Azure\CLI2\wbin",
                r"C:\Program Files\Azure Dev CLI",
            ],
        )
    } else {
        (vec![name.to_string()], &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"])
    };
    let dirs = path.map(|p| std::env::split_paths(p).collect::<Vec<_>>()).unwrap_or_default();
    let dirs = dirs.into_iter().filter(|d| d.is_absolute()).chain(fixed.iter().map(PathBuf::from));
    dirs.flat_map(|d| files.iter().map(move |f| d.join(f)).collect::<Vec<_>>()).collect()
}

fn find_cli(name: &str) -> Option<PathBuf> {
    cli_candidates(name, std::env::var_os("PATH").as_deref()).into_iter().find(|p| p.is_file())
}

/// An argument that's safe anywhere, also on a `cmd.exe` command line.
fn safe_arg(a: &str) -> bool {
    !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ':' | '/'))
}

/// The `cmd.exe` command line that runs a `.cmd` script: `/d` skips
/// AutoRun, `/v:off` keeps `!` literal, `/s /c "…"` strips only the outer
/// quotes. Refuses anything `cmd` would expand.
#[cfg_attr(not(windows), allow(dead_code))]
fn cmd_line(script: &Path, args: &[&str]) -> Result<String> {
    let script = script.to_str().ok_or_else(|| auth("ruta del programa no válida"))?;
    if script.chars().any(|c| matches!(c, '"' | '%' | '!' | '^' | '\r' | '\n')) || !args.iter().all(|a| safe_arg(a)) {
        return Err(auth("ruta o argumentos del programa no válidos"));
    }
    Ok(format!("/d /v:off /s /c \"\"{script}\" {}\"", args.join(" ")))
}

#[cfg(windows)]
fn cli_command(program: &Path, args: &[&str]) -> Result<tokio::process::Command> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let batch = program.extension().is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
    let mut cmd = if batch {
        let line = cmd_line(program, args)?;
        let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let mut c = tokio::process::Command::new(root.join("System32").join("cmd.exe"));
        c.raw_arg(line);
        c
    } else {
        let mut c = tokio::process::Command::new(program);
        c.args(args);
        c
    };
    cmd.creation_flags(CREATE_NO_WINDOW);
    Ok(cmd)
}

#[cfg(not(windows))]
fn cli_command(program: &Path, args: &[&str]) -> Result<tokio::process::Command> {
    let mut c = tokio::process::Command::new(program);
    c.args(args);
    Ok(c)
}

/// Runs a CLI (no shell) and returns its standard output.
async fn run_cli(name: &str, args: &[&str], login: &str) -> Result<String> {
    let program = find_cli(name).ok_or_else(|| auth("no está instalada"))?;
    if !args.iter().all(|a| safe_arg(a)) {
        return Err(auth("argumentos no válidos"));
    }
    let mut cmd = cli_command(&program, args)?;
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
    let out = match tokio::time::timeout(CLI_WAIT, cmd.output()).await {
        Ok(out) => out.map_err(|e| auth(format!("no se pudo ejecutar: {e}")))?,
        Err(_) => return Err(auth(format!("no respondió en {} s", CLI_WAIT.as_secs()))),
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = short(err.trim_start_matches("ERROR:").trim(), 200);
        let line = if line.is_empty() { format!("terminó con {}", out.status) } else { line };
        return Err(auth(format!("{line} (¿falta {login}?)")));
    }
    String::from_utf8(out.stdout).map_err(|_| auth("salida no válida"))
}

// --------------------------------- integrated (Windows / Kerberos + ADFS)

/// Integrated Windows Authentication: the current OS user (Windows session
/// or Kerberos ticket) signs in to the tenant's ADFS over WS-Trust, and
/// the SAML assertion is exchanged for an access token. Only for
/// federated accounts.
pub async fn integrated(user: &str, tenant: Option<&str>, client: Option<&str>) -> Result<String> {
    let tenant = tenant.unwrap_or("organizations");
    valid_tenant(tenant)?;
    let client = client.unwrap_or(SQL_CLIENT_APP);
    let key = Key::new(INTEGRATED, tenant, client, user);
    with_cache(key, |refresh| async move {
        if let Some(t) = refreshed(tenant, client, refresh).await {
            return Ok(t);
        }
        integrated_sign_in(user, tenant, client).await
    })
    .await
}

async fn integrated_sign_in(user: &str, tenant: &str, client: &str) -> Result<Token> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Connect(e.to_string()))?;
    // 1. Is the account federated, and where's its ADFS?
    let realm: Value = http
        .get(realm_url(user)?)
        .send()
        .await
        .map_err(|e| Error::Connect(format!("no se pudo llegar a Microsoft Entra ID: {e}")))?
        .json()
        .await
        .map_err(|e| Error::Connect(e.to_string()))?;
    let mex = match parse_realm(&realm)? {
        Realm::Federated { mex } => mex,
        Realm::Managed => {
            return Err(auth(format!(
                "la cuenta {user} no es federada (no inicia sesión con ADFS), así que la autenticación integrada \
                 no está disponible. Usá «Microsoft Entra ID: interactivo (MFA)» o «Microsoft Entra ID: predeterminada»."
            )))
        }
        Realm::Unknown => return Err(auth(format!("Microsoft Entra ID no reconoce la cuenta {user}: revisá el usuario (usuario@dominio)"))),
    };
    // 2. ADFS's WS-Trust endpoint for Windows authentication.
    let mex_xml = http
        .get(&mex)
        .send()
        .await
        .map_err(|e| Error::Connect(format!("no se pudo leer los metadatos de ADFS: {e}")))?
        .text()
        .await
        .map_err(|e| Error::Connect(e.to_string()))?;
    let endpoint = mex_endpoint(&mex_xml)?;
    // 3. The SAML assertion, signing in to ADFS as the current OS user.
    let rst = rst_envelope(&endpoint, &message_id()?);
    let rstr = negotiate_post(&endpoint, rst).await?;
    let saml = saml_from_rstr(&rstr)?;
    // 4. The assertion for an access token.
    let assertion = STANDARD.encode(saml.assertion.as_bytes());
    let form = [
        ("grant_type", saml.grant),
        ("assertion", assertion.as_str()),
        ("client_id", client),
        ("scope", SQL_SCOPE_OFFLINE),
    ];
    Token::from_oauth(&token_body(tenant, &form).await?, now())
}

/// The home realm discovery URL for a UPN.
fn realm_url(user: &str) -> Result<reqwest::Url> {
    let ok = user.split('@').count() == 2
        && !user.starts_with('@')
        && !user.ends_with('@')
        && !user.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '\\' | '?' | '#'));
    if !ok {
        return Err(auth("la autenticación integrada necesita la cuenta como usuario@dominio"));
    }
    let mut url = reqwest::Url::parse(LOGIN_HOST).expect("constant URL");
    url.path_segments_mut().expect("https URL").extend(["common", "userrealm", user]);
    url.query_pairs_mut().append_pair("api-version", "1.0");
    Ok(url)
}

#[derive(Debug, PartialEq)]
enum Realm {
    /// Signs in at the organization's ADFS; `mex` is its metadata URL.
    Federated { mex: String },
    /// Signs in at Entra ID itself (no ADFS).
    Managed,
    Unknown,
}

fn parse_realm(body: &Value) -> Result<Realm> {
    match body.get("account_type").and_then(Value::as_str) {
        Some(t) if t.eq_ignore_ascii_case("federated") => {
            let mex = body
                .get("federation_metadata_url")
                .and_then(Value::as_str)
                .filter(|u| u.starts_with("https://"))
                .ok_or_else(|| auth("la cuenta es federada pero ADFS no publica sus metadatos (MEX)"))?;
            Ok(Realm::Federated { mex: mex.to_string() })
        }
        Some(t) if t.eq_ignore_ascii_case("managed") => Ok(Realm::Managed),
        _ => Ok(Realm::Unknown),
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Trust {
    /// WS-Trust 1.3 (`/adfs/services/trust/13/windowstransport`).
    V13,
    /// WS-Trust February 2005 (`/adfs/services/trust/2005/windowstransport`).
    V2005,
}

impl Trust {
    fn ns(self) -> &'static str {
        match self {
            Trust::V13 => "http://docs.oasis-open.org/ws-sx/ws-trust/200512",
            Trust::V2005 => "http://schemas.xmlsoap.org/ws/2005/02/trust",
        }
    }
    fn action(self) -> String {
        format!("{}/RST/Issue", self.ns())
    }
    fn key_type(self) -> &'static str {
        match self {
            Trust::V13 => "http://docs.oasis-open.org/ws-sx/ws-trust/200512/Bearer",
            Trust::V2005 => "http://schemas.xmlsoap.org/ws/2005/05/identity/NoProofKey",
        }
    }
}

#[derive(Debug, PartialEq)]
struct WsTrustEndpoint {
    url: String,
    version: Trust,
}

const WSDL_NS: &str = "http://schemas.xmlsoap.org/wsdl/";
const SOAP12_NS: &str = "http://schemas.xmlsoap.org/wsdl/soap12/";

/// The endpoint for Windows authentication in ADFS's WS-Trust metadata
/// (MEX): a port whose binding's policy asks for HTTP Negotiate over
/// TLS. WS-Trust 1.3 wins over 2005.
fn mex_endpoint(xml: &str) -> Result<WsTrustEndpoint> {
    let doc = roxmltree::Document::parse(xml).map_err(|_| auth("los metadatos de ADFS (MEX) no son XML válido"))?;
    let elems = || doc.descendants().filter(|n| n.is_element());
    // Policies with Negotiate (Windows) authentication over TLS.
    let policies: Vec<&str> = elems()
        .filter(|n| n.tag_name().name() == "Policy")
        .filter(|p| {
            let has = |local: &str| p.descendants().any(|d| d.tag_name().name() == local);
            has("NegotiateAuthentication") && has("TransportBinding")
        })
        .filter_map(|p| p.attributes().find(|a| a.name() == "Id").map(|a| a.value()))
        .collect();
    // Bindings that use one of them, with their WS-Trust version.
    let bindings: Vec<(&str, Trust)> = elems()
        .filter(|n| n.tag_name().name() == "binding" && n.tag_name().namespace() == Some(WSDL_NS))
        .filter(|b| {
            b.descendants().any(|d| {
                d.tag_name().name() == "PolicyReference"
                    && d.attribute("URI").is_some_and(|u| policies.contains(&u.trim_start_matches('#')))
            })
        })
        .filter_map(|b| {
            let action = b
                .descendants()
                .filter(|d| d.tag_name().name() == "operation" && d.tag_name().namespace() == Some(SOAP12_NS))
                .find_map(|d| d.attribute("soapAction"))?;
            let version = if action == Trust::V13.action() {
                Trust::V13
            } else if action == Trust::V2005.action() {
                Trust::V2005
            } else {
                return None;
            };
            Some((b.attribute("name")?, version))
        })
        .collect();
    // The ports that implement those bindings.
    let mut found: Vec<WsTrustEndpoint> = elems()
        .filter(|n| n.tag_name().name() == "port" && n.tag_name().namespace() == Some(WSDL_NS))
        .filter_map(|p| {
            let binding = p.attribute("binding")?;
            let binding = binding.rsplit(':').next().unwrap_or(binding);
            let (_, version) = bindings.iter().find(|(name, _)| *name == binding)?;
            let url = p
                .descendants()
                .find(|d| d.tag_name().name() == "address" && d.tag_name().namespace() == Some(SOAP12_NS))
                .and_then(|a| a.attribute("location"))
                .or_else(|| p.descendants().find(|d| d.tag_name().name() == "Address").and_then(|a| a.text()))?
                .trim();
            url.starts_with("https://").then(|| WsTrustEndpoint { url: url.to_string(), version: *version })
        })
        .collect();
    found.sort_by_key(|e| e.version != Trust::V13);
    found
        .into_iter()
        .next()
        .ok_or_else(|| auth("ADFS no publica un punto de autenticación de Windows (windowstransport)"))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// A random `urn:uuid:` for the WS-Addressing MessageID.
fn message_id() -> Result<String> {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).map_err(|e| auth(format!("no hay números aleatorios del sistema: {e}")))?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!("urn:uuid:{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..]))
}

/// The WS-Trust RequestSecurityToken (SOAP 1.2) for a bearer token
/// that Microsoft Entra ID accepts (`urn:federation:MicrosoftOnline`). No
/// credentials inside: HTTP Negotiate carries them.
fn rst_envelope(ep: &WsTrustEndpoint, message_id: &str) -> String {
    let v = ep.version;
    format!(
        "<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\" xmlns:a=\"http://www.w3.org/2005/08/addressing\">\
         <s:Header>\
         <a:Action s:mustUnderstand=\"1\">{action}</a:Action>\
         <a:MessageID>{id}</a:MessageID>\
         <a:ReplyTo><a:Address>http://www.w3.org/2005/08/addressing/anonymous</a:Address></a:ReplyTo>\
         <a:To s:mustUnderstand=\"1\">{to}</a:To>\
         </s:Header>\
         <s:Body>\
         <trust:RequestSecurityToken xmlns:trust=\"{ns}\">\
         <wsp:AppliesTo xmlns:wsp=\"http://schemas.xmlsoap.org/ws/2004/09/policy\">\
         <a:EndpointReference><a:Address>urn:federation:MicrosoftOnline</a:Address></a:EndpointReference>\
         </wsp:AppliesTo>\
         <trust:KeyType>{key}</trust:KeyType>\
         <trust:RequestType>{ns}/Issue</trust:RequestType>\
         </trust:RequestSecurityToken>\
         </s:Body>\
         </s:Envelope>",
        action = v.action(),
        id = xml_escape(message_id),
        to = xml_escape(&ep.url),
        ns = v.ns(),
        key = v.key_type(),
    )
}

const SAML1_NS: &str = "urn:oasis:names:tc:SAML:1.0:assertion";
const SAML2_NS: &str = "urn:oasis:names:tc:SAML:2.0:assertion";
const SAML1_GRANT: &str = "urn:ietf:params:oauth:grant-type:saml1_1-bearer";
const SAML2_GRANT: &str = "urn:ietf:params:oauth:grant-type:saml2-bearer";

#[derive(Debug, PartialEq)]
struct Saml {
    grant: &'static str,
    /// The assertion's XML, exactly as ADFS signed it.
    assertion: String,
}

/// The SAML assertion in ADFS's RequestSecurityTokenResponse, and the
/// OAuth grant type that goes with its version. A SOAP fault becomes the
/// error.
fn saml_from_rstr(xml: &str) -> Result<Saml> {
    let doc = roxmltree::Document::parse(xml).map_err(|_| auth("ADFS respondió algo que no es XML válido"))?;
    let find = |local: &str| doc.descendants().find(|n| n.is_element() && n.tag_name().name() == local);
    if find("Fault").is_some() {
        let reason = find("Text").or_else(|| find("Reason")).and_then(|n| n.text()).unwrap_or("error sin detalle");
        return Err(auth(format!("ADFS rechazó la autenticación de Windows: {}", short(reason, 200))));
    }
    let assertion = find("RequestedSecurityToken")
        .and_then(|t| t.children().find(|c| c.is_element()))
        .filter(|a| a.tag_name().name() == "Assertion")
        .ok_or_else(|| auth("ADFS no devolvió una aserción SAML"))?;
    let token_type = find("TokenType").and_then(|n| n.text()).map(str::trim).unwrap_or("");
    let grant = match (token_type, assertion.tag_name().namespace()) {
        ("urn:oasis:names:tc:SAML:2.0:assertion", _) | ("", Some(SAML2_NS)) => SAML2_GRANT,
        ("urn:oasis:names:tc:SAML:1.0:assertion", _) | ("", Some(SAML1_NS)) => SAML1_GRANT,
        // WS-Trust 2005 names SAML 1.1 by its profile URI.
        (t, _) if t.ends_with("#SAMLV1.1") => SAML1_GRANT,
        (t, _) if t.ends_with("#SAMLV2.0") => SAML2_GRANT,
        _ => return Err(auth("ADFS devolvió un token que no es SAML 1.1 ni 2.0")),
    };
    Ok(Saml { grant, assertion: xml[assertion.range()].to_string() })
}

/// The TLS channel binding (`tls-server-end-point`, RFC 5929) for the
/// server's certificate, which ADFS checks when Extended Protection is on.
/// SHA-256, the hash for every certificate signed with SHA-256 or weaker.
fn channel_binding(cert_der: &[u8]) -> Vec<u8> {
    let mut cb = b"tls-server-end-point:".to_vec();
    cb.extend_from_slice(&Sha256::digest(cert_der));
    cb
}

/// The token in a `WWW-Authenticate: Negotiate <base64>` header, if any.
fn negotiate_token<'a>(headers: impl IntoIterator<Item = &'a str>) -> Option<Vec<u8>> {
    headers.into_iter().find_map(|h| {
        let (scheme, rest) = h.trim().split_once(' ')?;
        scheme.eq_ignore_ascii_case("Negotiate").then(|| STANDARD.decode(rest.trim()).ok()).flatten()
    })
}

fn offers_negotiate<'a>(headers: impl IntoIterator<Item = &'a str>) -> bool {
    headers.into_iter().any(|h| h.trim().split(' ').next().is_some_and(|s| s.eq_ignore_ascii_case("Negotiate")))
}

/// POSTs the RST to ADFS with HTTP Negotiate as the current OS user, and
/// returns the response body.
async fn negotiate_post(ep: &WsTrustEndpoint, body: String) -> Result<String> {
    let url = reqwest::Url::parse(&ep.url).map_err(|_| auth("dirección de ADFS no válida"))?;
    let host = url.host_str().ok_or_else(|| auth("dirección de ADFS no válida"))?.to_string();
    // HTTP/1.1 and one connection: NTLM authenticates the connection.
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .http1_only()
        .pool_max_idle_per_host(1)
        .tls_info(true)
        .build()
        .map_err(|e| Error::Connect(e.to_string()))?;
    let post = |auth_header: Option<String>| {
        let mut req = http
            .post(url.clone())
            .header("Content-Type", "application/soap+xml; charset=utf-8")
            .header("SOAPAction", ep.version.action())
            .body(body.clone());
        if let Some(h) = auth_header {
            req = req.header("Authorization", h);
        }
        req.send()
    };
    let send_err = |e: reqwest::Error| Error::Connect(format!("no se pudo llegar a ADFS: {e}"));
    let header_values = |r: &reqwest::Response| -> Vec<String> {
        r.headers().get_all("WWW-Authenticate").iter().filter_map(|v| v.to_str().ok()).map(str::to_string).collect()
    };

    // Unauthenticated first: the 401 says Negotiate is welcome, and the
    // TLS session gives the certificate for the channel binding.
    let first = post(None).await.map_err(send_err)?;
    if first.status().is_success() {
        return first.text().await.map_err(send_err);
    }
    let offered = header_values(&first);
    if first.status().as_u16() != 401 || !offers_negotiate(offered.iter().map(String::as_str)) {
        return Err(auth(format!("ADFS no ofrece autenticación de Windows (HTTP {})", first.status().as_u16())));
    }
    let cbt = first.extensions().get::<reqwest::tls::TlsInfo>().and_then(|i| i.peer_certificate()).map(channel_binding);
    let _ = first.bytes().await; // drain it so the connection is reused

    let mut negotiator = negotiate::Negotiator::new(&host, cbt)?;
    let mut input: Option<Vec<u8>> = None;
    for _ in 0..4 {
        let Some(out) = negotiator.step(input.as_deref())? else { break };
        let resp = post(Some(format!("Negotiate {}", STANDARD.encode(out)))).await.map_err(send_err)?;
        let status = resp.status().as_u16();
        if status == 401 {
            let headers = header_values(&resp);
            input = negotiate_token(headers.iter().map(String::as_str));
            let _ = resp.bytes().await;
            if input.is_none() {
                break;
            }
            continue;
        }
        // 200, or 500 with a SOAP fault that says why.
        if resp.status().is_success() || status == 500 {
            return resp.text().await.map_err(send_err);
        }
        return Err(auth(format!("ADFS respondió HTTP {status}")));
    }
    Err(auth(
        "ADFS no aceptó las credenciales de Windows. En Windows, iniciá sesión con una cuenta del dominio; \
         en macOS y Linux, sacá un ticket de Kerberos (kinit usuario@DOMINIO).",
    ))
}

/// HTTP Negotiate as the current OS user: GSSAPI with SPNEGO (the Kerberos
/// ticket from `kinit`) on macOS and Linux, through the libgssapi that
/// tiberius already links for TDS.
#[cfg(unix)]
mod negotiate {
    use super::auth;
    use dbine_driver::Result;
    use libgssapi::context::{ClientCtx, CtxFlags};
    use libgssapi::name::Name;
    use libgssapi::oid::{GSS_MECH_SPNEGO, GSS_NT_HOSTBASED_SERVICE};

    pub struct Negotiator {
        ctx: ClientCtx,
        cbt: Option<Vec<u8>>,
    }

    fn gss_error(e: libgssapi::error::Error) -> dbine_driver::Error {
        auth(format!(
            "no se pudo usar el ticket de Kerberos para ADFS ({e}); sacá uno con kinit usuario@DOMINIO"
        ))
    }

    impl Negotiator {
        pub fn new(host: &str, cbt: Option<Vec<u8>>) -> Result<Negotiator> {
            let target = Name::new(format!("HTTP@{host}").as_bytes(), Some(GSS_NT_HOSTBASED_SERVICE)).map_err(gss_error)?;
            let ctx = ClientCtx::new(None, target, CtxFlags::GSS_C_MUTUAL_FLAG, Some(GSS_MECH_SPNEGO));
            Ok(Negotiator { ctx, cbt })
        }

        pub fn step(&mut self, input: Option<&[u8]>) -> Result<Option<Vec<u8>>> {
            let out = self.ctx.step(input, self.cbt.as_deref()).map_err(gss_error)?;
            Ok(out.map(|b| b.to_vec()))
        }
    }
}

/// HTTP Negotiate as the current OS user on Windows: SSPI through winauth,
/// which tiberius already links for TDS (its SSPI client is NTLM).
#[cfg(windows)]
mod negotiate {
    use super::auth;
    use dbine_driver::Result;
    use winauth::windows::{NtlmSspi, NtlmSspiBuilder};
    use winauth::NextBytes;

    pub struct Negotiator(NtlmSspi);

    impl Negotiator {
        pub fn new(host: &str, cbt: Option<Vec<u8>>) -> Result<Negotiator> {
            let mut b = NtlmSspiBuilder::new().outbound().target_spn(&format!("HTTP/{host}"));
            if let Some(cbt) = &cbt {
                b = b.channel_bindings(cbt);
            }
            let sspi = b.build().map_err(|e| auth(format!("no se pudo usar la sesión de Windows: {e}")))?;
            Ok(Negotiator(sspi))
        }

        pub fn step(&mut self, input: Option<&[u8]>) -> Result<Option<Vec<u8>>> {
            self.0.next_bytes(input).map_err(|e| auth(format!("no se pudo usar la sesión de Windows: {e}")))
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod negotiate {
    use dbine_driver::{Error, Result};

    pub struct Negotiator;

    impl Negotiator {
        pub fn new(_host: &str, _cbt: Option<Vec<u8>>) -> Result<Negotiator> {
            Err(Error::Unsupported("la autenticación integrada no está disponible en este sistema".into()))
        }

        pub fn step(&mut self, _input: Option<&[u8]>) -> Result<Option<Vec<u8>>> {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pkce_matches_rfc_7636() {
        // RFC 7636, appendix B.
        assert_eq!(pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        let v = random_b64(32).unwrap();
        // 43 characters of the unreserved set, and never the same twice.
        assert_eq!(v.len(), 43);
        assert!(v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(v, random_b64(32).unwrap());
    }

    #[test]
    fn authorize_url_carries_pkce_state_and_hint() {
        let u = authorize_url("organizations", SQL_CLIENT_APP, "http://localhost:5123", "st", "ch", Some("ana@contoso.com"));
        let url = reqwest::Url::parse(&u).unwrap();
        assert_eq!(url.host_str(), Some("login.microsoftonline.com"));
        assert_eq!(url.path(), "/organizations/oauth2/v2.0/authorize");
        let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["redirect_uri"], "http://localhost:5123");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["code_challenge"], "ch");
        assert_eq!(q["state"], "st");
        assert_eq!(q["login_hint"], "ana@contoso.com");
        assert!(q["scope"].contains("https://database.windows.net/.default") && q["scope"].contains("offline_access"));
        let u = authorize_url("t", "c", "http://localhost:1", "s", "c", None);
        assert!(!u.contains("login_hint"));
    }

    #[test]
    fn browser_opens_without_a_shell() {
        let url = "https://login.microsoftonline.com/x/oauth2/v2.0/authorize?a=1&b=2";
        let (program, args) = browser_argv(url);
        // The URL travels as one argument, `&` and all.
        assert_eq!(args.last().map(String::as_str), Some(url));
        let name = program.file_name().unwrap().to_string_lossy().to_lowercase();
        assert!(["open", "xdg-open", "rundll32.exe"].contains(&name.as_str()), "{name}");
        assert!(!["sh", "bash", "zsh", "cmd.exe", "powershell.exe"].contains(&name.as_str()));
        assert!(open_browser("https://evil.example/").is_err());
    }

    #[test]
    fn redirects_are_checked() {
        let ok = "GET /?code=0.AXo%2Bz&state=abc&session_state=x HTTP/1.1\r\nHost: localhost:5123\r\n\r\n";
        assert_eq!(parse_callback(ok, "abc"), Callback::Code("0.AXo+z".into()));
        // Someone else's state, also on an error.
        assert_eq!(parse_callback(ok, "abd"), Callback::BadState);
        assert_eq!(parse_callback("GET /?code=x HTTP/1.1\r\n\r\n", "abc"), Callback::BadState);
        assert_eq!(parse_callback("GET /?error=access_denied&state=zzz HTTP/1.1\r\n\r\n", "abc"), Callback::BadState);
        let denied = "GET /?error=access_denied&error_description=AADSTS65004%3A+User+declined+to+consent.%0D%0ATrace&state=abc HTTP/1.1\r\n\r\n";
        assert_eq!(parse_callback(denied, "abc"), Callback::Failed("AADSTS65004: User declined to consent.".into()));
        // Not the redirect.
        assert_eq!(parse_callback("GET /favicon.ico HTTP/1.1\r\n\r\n", "abc"), Callback::Ignore);
        assert_eq!(parse_callback("GET / HTTP/1.1\r\n\r\n", "abc"), Callback::Ignore);
        assert_eq!(parse_callback("POST /?code=x&state=abc HTTP/1.1\r\n\r\n", "abc"), Callback::Ignore);
        assert_eq!(parse_callback("", "abc"), Callback::Ignore);
    }

    #[test]
    fn the_browser_page_escapes_and_has_a_length() {
        let p = page("200 OK", "No se pudo", "<script>x</script>");
        assert!(p.starts_with("HTTP/1.1 200 OK\r\n") && !p.contains("<script>"));
        let (head, body) = p.split_once("\r\n\r\n").unwrap();
        assert!(head.contains(&format!("Content-Length: {}", body.len())));
    }

    #[tokio::test]
    async fn loopback_listener_waits_for_the_right_state() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let browser = tokio::spawn(async move {
            let mut answers = vec![];
            for req in ["GET /favicon.ico HTTP/1.1\r\n\r\n", "GET /?code=bad&state=nope HTTP/1.1\r\n\r\n", "GET /?code=good&state=s1 HTTP/1.1\r\n\r\n"] {
                let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
                s.write_all(req.as_bytes()).await.unwrap();
                let mut out = String::new();
                s.read_to_string(&mut out).await.unwrap();
                answers.push(out);
            }
            answers
        });
        assert_eq!(wait_for_code(&listener, "s1").await.unwrap(), "good");
        let answers = browser.await.unwrap();
        assert!(answers[0].starts_with("HTTP/1.1 404"));
        assert!(answers[1].starts_with("HTTP/1.1 400"));
        assert!(answers[2].starts_with("HTTP/1.1 200") && answers[2].contains("podés cerrar esta ventana"));
    }

    fn tok(expires_on: u64, refresh: Option<&str>) -> Token {
        Token { access: "at".into(), expires_on, refresh: refresh.map(str::to_string) }
    }

    #[test]
    fn cache_reuses_until_five_minutes_before_expiry() {
        let key = Key::new(MFA, "t", "c", "Ana@Contoso.com");
        let mut cache = HashMap::new();
        assert_eq!(lookup(&cache, &key, 1000), Cached::Miss);
        cache.insert(key.clone(), tok(1000 + 3600, Some("rt")));
        assert_eq!(lookup(&cache, &key, 1000), Cached::Fresh("at".into()));
        // The user is case-insensitive; the method, tenant and client aren't.
        assert_eq!(lookup(&cache, &Key::new(MFA, "t", "c", "ana@contoso.com"), 1000), Cached::Fresh("at".into()));
        assert_eq!(lookup(&cache, &Key::new(DEFAULT, "t", "c", "ana@contoso.com"), 1000), Cached::Miss);
        assert_eq!(lookup(&cache, &Key::new(MFA, "t2", "c", "ana@contoso.com"), 1000), Cached::Miss);
        // Inside the last five minutes: refresh.
        assert_eq!(lookup(&cache, &key, 1000 + 3600 - MARGIN - 1), Cached::Fresh("at".into()));
        assert_eq!(lookup(&cache, &key, 1000 + 3600 - MARGIN), Cached::Refresh("rt".into()));
        assert_eq!(lookup(&cache, &key, 1000 + 7200), Cached::Refresh("rt".into()));
        // Without a refresh token, an old token is a miss.
        cache.insert(key.clone(), tok(1000, None));
        assert_eq!(lookup(&cache, &key, 1000), Cached::Miss);
    }

    #[tokio::test]
    async fn with_cache_acquires_once() {
        let key = Key::new("test", "cache", &now().to_string(), "");
        let first = with_cache(key.clone(), |r| async move {
            assert!(r.is_none());
            Ok(Token { access: "one".into(), expires_on: now() + 3600, refresh: None })
        })
        .await
        .unwrap();
        let second = with_cache(key, |_| async { Err::<Token, _>(auth("no debería pedir otro")) }).await.unwrap();
        assert_eq!((first.as_str(), second.as_str()), ("one", "one"));
    }

    #[test]
    fn oauth_tokens_carry_expiry_and_refresh() {
        let t = Token::from_oauth(&json!({"access_token": "a", "expires_in": 3599, "refresh_token": "r"}), 100).unwrap();
        assert_eq!((t.access.as_str(), t.expires_on, t.refresh.as_deref()), ("a", 3699, Some("r")));
        let t = Token::from_oauth(&json!({"access_token": "a", "expires_in": "60"}), 100).unwrap();
        assert_eq!((t.expires_on, t.refresh), (160, None));
        assert!(Token::from_oauth(&json!({"error": "invalid_grant"}), 0).is_err());
        // Debug never shows the tokens.
        let t = tok(1, Some("refresh-secret"));
        let shown = format!("{t:?}");
        assert!(!shown.contains("\"at\"") && !shown.contains("refresh-secret"), "{shown}");
    }

    #[test]
    fn msi_requests() {
        let imds = msi_request(None, None).unwrap();
        assert!(imds.imds);
        assert_eq!(
            imds.url,
            "http://169.254.169.254/metadata/identity/oauth2/token?api-version=2018-02-01&resource=https%3A%2F%2Fdatabase.windows.net%2F"
        );
        assert_eq!(imds.header, ("Metadata", "true".to_string()));
        let user = msi_request(None, Some("1111-22")).unwrap();
        assert!(user.url.ends_with("&client_id=1111-22"));
        let app = msi_request(Some(("http://127.0.0.1:41056/msi/token".into(), "s3cr3t".into())), Some("c")).unwrap();
        assert!(!app.imds);
        assert_eq!(
            app.url,
            "http://127.0.0.1:41056/msi/token?api-version=2019-08-01&resource=https%3A%2F%2Fdatabase.windows.net%2F&client_id=c"
        );
        assert_eq!(app.header, ("X-IDENTITY-HEADER", "s3cr3t".to_string()));
        assert!(msi_request(Some(("file:///etc/passwd".into(), "x".into())), None).is_err());
        assert!(msi_request(Some(("no es url".into(), "x".into())), None).is_err());
    }

    #[test]
    fn msi_responses() {
        // IMDS: strings for the numbers.
        let imds = r#"{"access_token":"eyJ0","client_id":"c","expires_in":"86399","expires_on":"1760000000","ext_expires_in":"86399","not_before":"1759913300","resource":"https://database.windows.net/","token_type":"Bearer"}"#;
        let t = parse_msi(200, imds, 5).unwrap();
        assert_eq!((t.access.as_str(), t.expires_on), ("eyJ0", 1_760_000_000));
        let t = parse_msi(200, r#"{"access_token":"x","expires_in":"600"}"#, 1000).unwrap();
        assert_eq!(t.expires_on, 1600);
        let e = parse_msi(400, r#"{"error":"invalid_request","error_description":"Identity not found"}"#, 0).unwrap_err();
        assert!(matches!(e, Error::AuthFailed(m) if m.ends_with("Identity not found")));
        let e = parse_msi(500, "<html>", 0).unwrap_err();
        assert!(e.to_string().contains("HTTP 500"));
    }

    #[test]
    fn az_and_azd_json() {
        // Newer Azure CLI: expires_on in Unix seconds.
        let az = r#"{"accessToken":"eyJa","expiresOn":"2026-10-09 15:04:05.000000","expires_on":1791558245,"subscription":"s","tenant":"t","tokenType":"Bearer"}"#;
        let t = parse_az(az, &chrono::Utc).unwrap();
        assert_eq!((t.access.as_str(), t.expires_on), ("eyJa", 1_791_558_245));
        // Older: only expiresOn, in local time.
        let old = r#"{"accessToken":"eyJb","expiresOn":"2026-10-09 15:04:05.123456","tokenType":"Bearer"}"#;
        assert_eq!(parse_az(old, &chrono::Utc).unwrap().expires_on, 1_791_558_245);
        let minus3 = chrono::FixedOffset::west_opt(3 * 3600).unwrap();
        assert_eq!(parse_az(old, &minus3).unwrap().expires_on, 1_791_558_245 + 3 * 3600);
        assert!(parse_az(r#"{"accessToken":"x"}"#, &chrono::Utc).is_err());
        assert!(parse_az("Please run 'az login'", &chrono::Utc).is_err());
        let azd = r#"{"token":"eyJc","expiresOn":"2026-10-09T15:04:05Z"}"#;
        assert_eq!(parse_azd(azd).unwrap().expires_on, 1_791_558_245);
        let azd = r#"{"token":"eyJc","expiresOn":"2026-10-09T12:04:05-03:00"}"#;
        assert_eq!(parse_azd(azd).unwrap().expires_on, 1_791_558_245);
        assert!(parse_azd(r#"{"expiresOn":"2026-10-09T15:04:05Z"}"#).is_err());
    }

    #[test]
    fn cli_lookup_and_command_lines() {
        let path = std::env::join_paths(["/custom/bin", "relative/bin"]).unwrap();
        let c = cli_candidates("az", Some(&path));
        if cfg!(windows) {
            assert!(c.iter().any(|p| p.ends_with(r"Microsoft SDKs\Azure\CLI2\wbin\az.cmd")));
        } else {
            assert_eq!(c[0], PathBuf::from("/custom/bin/az"));
            assert!(c.contains(&PathBuf::from("/opt/homebrew/bin/az")) && c.contains(&PathBuf::from("/usr/local/bin/az")));
        }
        // A relative PATH entry is never searched.
        assert!(!c.iter().any(|p| p.starts_with("relative")));
        assert!(!cli_candidates("az", None).is_empty());

        let script = Path::new(r"C:\Program Files (x86)\Microsoft SDKs\Azure\CLI2\wbin\az.cmd");
        let args = ["account", "get-access-token", "--resource", SQL_RESOURCE, "--output", "json", "--tenant", "contoso.onmicrosoft.com"];
        assert_eq!(
            cmd_line(script, &args).unwrap(),
            "/d /v:off /s /c \"\"C:\\Program Files (x86)\\Microsoft SDKs\\Azure\\CLI2\\wbin\\az.cmd\" account get-access-token \
             --resource https://database.windows.net/ --output json --tenant contoso.onmicrosoft.com\""
        );
        assert!(cmd_line(script, &["--tenant", "x&calc"]).is_err());
        assert!(cmd_line(script, &["a b"]).is_err());
        assert!(cmd_line(Path::new(r"C:\%PATH%\az.cmd"), &["x"]).is_err());
        assert!(safe_arg(SQL_SCOPE) && !safe_arg("") && !safe_arg("a\"b"));
    }

    #[tokio::test]
    async fn default_chain_reports_every_step() {
        assert!(env_triplet(|_| None).is_err());
        assert!(env_triplet(|k| (k != "AZURE_CLIENT_SECRET").then(|| "x".to_string())).is_err());
        assert_eq!(env_triplet(|k| Some(format!(" {k} "))).unwrap().2, "AZURE_CLIENT_SECRET");
        assert!(env_credential(|_| None).await.is_err());

        let e = chain_error(&[
            ("variables de entorno", auth("no están definidas AZURE_TENANT_ID, AZURE_CLIENT_ID y AZURE_CLIENT_SECRET")),
            ("identidad administrada", auth(NO_MSI)),
            ("Azure CLI", auth("Please run 'az login' to setup account.\nmore detail (¿falta az login?)")),
            ("Azure Developer CLI", auth("no está instalada")),
        ]);
        let Error::AuthFailed(m) = e else { panic!() };
        let lines: Vec<&str> = m.lines().collect();
        assert_eq!(lines.len(), 5, "{m}");
        assert!(lines[0].contains("autenticación predeterminada"));
        assert!(lines[1].starts_with("- variables de entorno: no están definidas"));
        assert!(lines[2].starts_with("- identidad administrada: no se encontró"));
        assert_eq!(lines[3], "- Azure CLI: Please run 'az login' to setup account.");
        assert_eq!(lines[4], "- Azure Developer CLI: no está instalada");
    }

    #[test]
    fn realm_discovery() {
        assert_eq!(
            realm_url("ana@contoso.com").unwrap().as_str(),
            "https://login.microsoftonline.com/common/userrealm/ana@contoso.com?api-version=1.0"
        );
        for bad in ["ana", "@contoso.com", "ana@", "a@b@c", "ana @contoso.com", "../x@y", "a@b/c"] {
            assert!(realm_url(bad).is_err(), "{bad}");
        }
        let federated = json!({
            "ver": "1.0", "account_type": "Federated", "domain_name": "contoso.com",
            "federation_protocol": "WSTrust",
            "federation_metadata_url": "https://sts.contoso.com/adfs/services/trust/mex",
            "federation_active_auth_url": "https://sts.contoso.com/adfs/services/trust/2005/usernamemixed",
            "cloud_instance_name": "microsoftonline.com", "cloud_audience_urn": "urn:federation:MicrosoftOnline"
        });
        assert_eq!(parse_realm(&federated).unwrap(), Realm::Federated { mex: "https://sts.contoso.com/adfs/services/trust/mex".into() });
        let managed = json!({"ver": "1.0", "account_type": "Managed", "domain_name": "contoso.onmicrosoft.com", "cloud_instance_name": "microsoftonline.com"});
        assert_eq!(parse_realm(&managed).unwrap(), Realm::Managed);
        assert_eq!(parse_realm(&json!({"ver": "1.0", "account_type": "Unknown"})).unwrap(), Realm::Unknown);
        assert!(parse_realm(&json!({"account_type": "Federated"})).is_err());
        assert!(parse_realm(&json!({"account_type": "Federated", "federation_metadata_url": "http://sts/mex"})).is_err());
    }

    /// ADFS's MEX, cut down to the parts that matter: Windows transport
    /// (2005 and 1.3) and username/password (1.3), each with its policy.
    const MEX: &str = r##"<?xml version="1.0" encoding="utf-8"?>
<wsdl:definitions name="SecurityTokenService" targetNamespace="http://tempuri.org/"
  xmlns:wsdl="http://schemas.xmlsoap.org/wsdl/" xmlns:tns="http://tempuri.org/"
  xmlns:soap12="http://schemas.xmlsoap.org/wsdl/soap12/"
  xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd"
  xmlns:wsp="http://schemas.xmlsoap.org/ws/2004/09/policy"
  xmlns:sp="http://docs.oasis-open.org/ws-sx/ws-securitypolicy/200702"
  xmlns:wsa10="http://www.w3.org/2005/08/addressing">
  <wsp:Policy wsu:Id="CustomBinding_IWSTrustFeb2005Async_policy">
    <wsp:ExactlyOne><wsp:All>
      <http:NegotiateAuthentication xmlns:http="http://schemas.microsoft.com/ws/06/2004/policy/http"/>
      <sp:TransportBinding><wsp:Policy><sp:TransportToken><wsp:Policy><sp:HttpsToken/></wsp:Policy></sp:TransportToken></wsp:Policy></sp:TransportBinding>
    </wsp:All></wsp:ExactlyOne>
  </wsp:Policy>
  <wsp:Policy wsu:Id="CustomBinding_IWSTrust13Async_policy">
    <wsp:ExactlyOne><wsp:All>
      <http:NegotiateAuthentication xmlns:http="http://schemas.microsoft.com/ws/06/2004/policy/http"/>
      <sp:TransportBinding><wsp:Policy><sp:TransportToken><wsp:Policy><sp:HttpsToken/></wsp:Policy></sp:TransportToken></wsp:Policy></sp:TransportBinding>
    </wsp:All></wsp:ExactlyOne>
  </wsp:Policy>
  <wsp:Policy wsu:Id="UserNameWSTrustBinding_IWSTrust13Async_policy">
    <wsp:ExactlyOne><wsp:All>
      <sp:TransportBinding><wsp:Policy><sp:TransportToken><wsp:Policy><sp:HttpsToken/></wsp:Policy></sp:TransportToken></wsp:Policy></sp:TransportBinding>
      <sp:SignedEncryptedSupportingTokens><wsp:Policy><sp:UsernameToken/></wsp:Policy></sp:SignedEncryptedSupportingTokens>
    </wsp:All></wsp:ExactlyOne>
  </wsp:Policy>
  <wsdl:binding name="CustomBinding_IWSTrustFeb2005Async" type="tns:IWSTrustFeb2005Async">
    <wsp:PolicyReference URI="#CustomBinding_IWSTrustFeb2005Async_policy"/>
    <soap12:binding transport="http://schemas.xmlsoap.org/soap/http"/>
    <wsdl:operation name="TrustFeb2005IssueAsync">
      <soap12:operation soapAction="http://schemas.xmlsoap.org/ws/2005/02/trust/RST/Issue" style="document"/>
    </wsdl:operation>
  </wsdl:binding>
  <wsdl:binding name="CustomBinding_IWSTrust13Async" type="tns:IWSTrust13Async">
    <wsp:PolicyReference URI="#CustomBinding_IWSTrust13Async_policy"/>
    <soap12:binding transport="http://schemas.xmlsoap.org/soap/http"/>
    <wsdl:operation name="Trust13IssueAsync">
      <soap12:operation soapAction="http://docs.oasis-open.org/ws-sx/ws-trust/200512/RST/Issue" style="document"/>
    </wsdl:operation>
  </wsdl:binding>
  <wsdl:binding name="UserNameWSTrustBinding_IWSTrust13Async" type="tns:IWSTrust13Async">
    <wsp:PolicyReference URI="#UserNameWSTrustBinding_IWSTrust13Async_policy"/>
    <soap12:binding transport="http://schemas.xmlsoap.org/soap/http"/>
    <wsdl:operation name="Trust13IssueAsync">
      <soap12:operation soapAction="http://docs.oasis-open.org/ws-sx/ws-trust/200512/RST/Issue" style="document"/>
    </wsdl:operation>
  </wsdl:binding>
  <wsdl:service name="SecurityTokenService">
    <wsdl:port name="CustomBinding_IWSTrustFeb2005Async" binding="tns:CustomBinding_IWSTrustFeb2005Async">
      <soap12:address location="https://sts.contoso.com/adfs/services/trust/2005/windowstransport"/>
      <wsa10:EndpointReference><wsa10:Address>https://sts.contoso.com/adfs/services/trust/2005/windowstransport</wsa10:Address></wsa10:EndpointReference>
    </wsdl:port>
    <wsdl:port name="UserNameWSTrustBinding_IWSTrust13Async" binding="tns:UserNameWSTrustBinding_IWSTrust13Async">
      <soap12:address location="https://sts.contoso.com/adfs/services/trust/13/usernamemixed"/>
    </wsdl:port>
    <wsdl:port name="CustomBinding_IWSTrust13Async" binding="tns:CustomBinding_IWSTrust13Async">
      <soap12:address location="https://sts.contoso.com/adfs/services/trust/13/windowstransport"/>
    </wsdl:port>
  </wsdl:service>
</wsdl:definitions>"##;

    #[test]
    fn mex_picks_the_windows_transport() {
        let ep = mex_endpoint(MEX).unwrap();
        assert_eq!(ep, WsTrustEndpoint { url: "https://sts.contoso.com/adfs/services/trust/13/windowstransport".into(), version: Trust::V13 });
        // Without 1.3, the 2005 one.
        let only2005 = MEX.replace(r#"<soap12:address location="https://sts.contoso.com/adfs/services/trust/13/windowstransport"/>"#, "");
        let ep = mex_endpoint(&only2005).unwrap();
        assert_eq!((ep.url.as_str(), ep.version), ("https://sts.contoso.com/adfs/services/trust/2005/windowstransport", Trust::V2005));
        // Username/password only: no endpoint.
        let none = MEX.replace("http:NegotiateAuthentication", "http:BasicAuthentication");
        assert!(mex_endpoint(&none).is_err());
        // Plain HTTP is never used.
        assert!(mex_endpoint(&MEX.replace("https://sts", "http://sts")).is_err());
        assert!(mex_endpoint("<not xml").is_err());
    }

    #[test]
    fn rst_envelopes() {
        let ep = WsTrustEndpoint { url: "https://sts.contoso.com/adfs/services/trust/13/windowstransport?a=1&b=2".into(), version: Trust::V13 };
        let id = message_id().unwrap();
        assert!(id.starts_with("urn:uuid:") && id.len() == 45 && &id[23..24] == "4", "{id}");
        let rst = rst_envelope(&ep, &id);
        let doc = roxmltree::Document::parse(&rst).unwrap();
        let text = |local: &str| doc.descendants().find(|n| n.tag_name().name() == local).and_then(|n| n.text()).map(str::to_string);
        assert_eq!(text("Action").unwrap(), "http://docs.oasis-open.org/ws-sx/ws-trust/200512/RST/Issue");
        assert_eq!(text("To").unwrap(), ep.url);
        assert_eq!(text("MessageID").unwrap(), id);
        assert_eq!(text("KeyType").unwrap(), "http://docs.oasis-open.org/ws-sx/ws-trust/200512/Bearer");
        assert_eq!(text("RequestType").unwrap(), "http://docs.oasis-open.org/ws-sx/ws-trust/200512/Issue");
        let applies = doc.descendants().find(|n| n.tag_name().name() == "AppliesTo").unwrap();
        assert!(applies.descendants().any(|n| n.text() == Some("urn:federation:MicrosoftOnline")));
        let rst = doc.descendants().find(|n| n.tag_name().name() == "RequestSecurityToken").unwrap();
        assert_eq!(rst.tag_name().namespace(), Some(Trust::V13.ns()));
        // No credentials in the body: Negotiate carries them.
        assert!(!rst_envelope(&ep, &id).contains("UsernameToken"));

        let ep = WsTrustEndpoint { url: "https://sts/adfs/services/trust/2005/windowstransport".into(), version: Trust::V2005 };
        let rst = rst_envelope(&ep, &id);
        let doc = roxmltree::Document::parse(&rst).unwrap();
        let key = doc.descendants().find(|n| n.tag_name().name() == "KeyType").unwrap();
        assert_eq!(key.text(), Some("http://schemas.xmlsoap.org/ws/2005/05/identity/NoProofKey"));
        assert_eq!(key.tag_name().namespace(), Some("http://schemas.xmlsoap.org/ws/2005/02/trust"));
    }

    const RSTR_SAML1: &str = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:a="http://www.w3.org/2005/08/addressing"><s:Header><a:Action s:mustUnderstand="1">http://docs.oasis-open.org/ws-sx/ws-trust/200512/RSTRC/IssueFinal</a:Action></s:Header><s:Body><trust:RequestSecurityTokenResponseCollection xmlns:trust="http://docs.oasis-open.org/ws-sx/ws-trust/200512"><trust:RequestSecurityTokenResponse><trust:Lifetime><wsu:Created xmlns:wsu="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd">2026-10-09T12:00:00.000Z</wsu:Created></trust:Lifetime><wsp:AppliesTo xmlns:wsp="http://schemas.xmlsoap.org/ws/2004/09/policy"><wsa:EndpointReference xmlns:wsa="http://www.w3.org/2005/08/addressing"><wsa:Address>urn:federation:MicrosoftOnline</wsa:Address></wsa:EndpointReference></wsp:AppliesTo><trust:RequestedSecurityToken><saml:Assertion MajorVersion="1" MinorVersion="1" AssertionID="_a1" Issuer="http://sts.contoso.com/adfs/services/trust" IssueInstant="2026-10-09T12:00:00.000Z" xmlns:saml="urn:oasis:names:tc:SAML:1.0:assertion"><saml:AttributeStatement><saml:Subject><saml:NameIdentifier Format="urn:oasis:names:tc:SAML:1.1:nameid-format:unspecified">ana@contoso.com</saml:NameIdentifier></saml:Subject></saml:AttributeStatement><ds:Signature xmlns:ds="http://www.w3.org/2000/09/xmldsig#"><ds:SignatureValue>c2ln</ds:SignatureValue></ds:Signature></saml:Assertion></trust:RequestedSecurityToken><trust:TokenType>urn:oasis:names:tc:SAML:1.0:assertion</trust:TokenType><trust:RequestType>http://docs.oasis-open.org/ws-sx/ws-trust/200512/Issue</trust:RequestType><trust:KeyType>http://docs.oasis-open.org/ws-sx/ws-trust/200512/Bearer</trust:KeyType></trust:RequestSecurityTokenResponse></trust:RequestSecurityTokenResponseCollection></s:Body></s:Envelope>"#;

    #[test]
    fn saml_assertions_come_out_verbatim() {
        let saml = saml_from_rstr(RSTR_SAML1).unwrap();
        assert_eq!(saml.grant, SAML1_GRANT);
        assert!(saml.assertion.starts_with(r#"<saml:Assertion MajorVersion="1""#));
        assert!(saml.assertion.ends_with("</saml:Assertion>"));
        assert!(saml.assertion.contains("<ds:SignatureValue>c2ln</ds:SignatureValue>"));
        // SAML 2.0, told by the token type…
        let saml2 = RSTR_SAML1
            .replace("<trust:TokenType>urn:oasis:names:tc:SAML:1.0:assertion", "<trust:TokenType>urn:oasis:names:tc:SAML:2.0:assertion")
            .replace("urn:oasis:names:tc:SAML:1.0:assertion\"", "urn:oasis:names:tc:SAML:2.0:assertion\"");
        assert_eq!(saml_from_rstr(&saml2).unwrap().grant, SAML2_GRANT);
        // …or, without one, by the assertion's namespace.
        let no_type = saml2.replace("<trust:TokenType>urn:oasis:names:tc:SAML:2.0:assertion</trust:TokenType>", "");
        assert_eq!(saml_from_rstr(&no_type).unwrap().grant, SAML2_GRANT);
        // WS-Trust 2005 names SAML 1.1 by its profile.
        let v2005 = RSTR_SAML1.replace(
            "<trust:TokenType>urn:oasis:names:tc:SAML:1.0:assertion",
            "<trust:TokenType>http://docs.oasis-open.org/wss/oasis-wss-saml-token-profile-1.1#SAMLV1.1",
        );
        assert_eq!(saml_from_rstr(&v2005).unwrap().grant, SAML1_GRANT);
        assert!(saml_from_rstr("<s:Envelope xmlns:s=\"http://www.w3.org/2003/05/soap-envelope\"><s:Body/></s:Envelope>").is_err());
    }

    #[test]
    fn soap_faults_explain_themselves() {
        let fault = r#"<s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope"><s:Body><s:Fault><s:Code><s:Value>s:Sender</s:Value><s:Subcode><s:Value xmlns:a="http://docs.oasis-open.org/ws-sx/ws-trust/200512">a:FailedAuthentication</s:Value></s:Subcode></s:Code><s:Reason><s:Text xml:lang="en-US">MSIS3127: The specified request failed.</s:Text></s:Reason></s:Fault></s:Body></s:Envelope>"#;
        let e = saml_from_rstr(fault).unwrap_err();
        assert!(matches!(e, Error::AuthFailed(m) if m.ends_with("MSIS3127: The specified request failed.")));
    }

    #[test]
    fn negotiate_headers_and_channel_binding() {
        assert!(offers_negotiate(["NTLM", "Negotiate"]));
        assert!(offers_negotiate(["negotiate abc="]));
        assert!(!offers_negotiate(["Basic realm=\"x\"", "NTLM"]));
        assert_eq!(negotiate_token(["NTLM", "Negotiate oYG3MIG0"]), Some(STANDARD.decode("oYG3MIG0").unwrap()));
        assert_eq!(negotiate_token(["Negotiate"]), None);
        assert_eq!(negotiate_token(["Negotiate !!!"]), None);
        let cb = channel_binding(b"cert");
        assert!(cb.starts_with(b"tls-server-end-point:"));
        assert_eq!(cb.len(), "tls-server-end-point:".len() + 32);
    }

    /// The claims of a JWT (unverified).
    fn jwt_claims(token: &str) -> Value {
        let payload = token.split('.').nth(1).expect("a JWT");
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).unwrap()).unwrap()
    }

    /// Needs Azure CLI (or another link of the chain) signed in on this
    /// machine. Prints only the audience and the expiry.
    #[tokio::test]
    #[ignore]
    async fn live_default_chain_returns_a_sql_token() {
        let token = default_chain(None, None).await.unwrap();
        let claims = jwt_claims(&token);
        let aud = claims["aud"].as_str().unwrap();
        let exp = claims["exp"].as_u64().unwrap();
        println!("aud = {aud}, exp = {exp} (en {} min)", exp.saturating_sub(now()) / 60);
        assert!(aud == "https://database.windows.net" || aud == "https://database.windows.net/", "{aud}");
        assert!(exp > now());
        // The second call comes from the cache.
        assert_eq!(default_chain(None, None).await.unwrap(), token);
    }
}
