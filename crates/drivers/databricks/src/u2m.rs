//! Databricks OAuth user-to-machine (U2M): the user's own browser sign-in,
//! as the Databricks CLI and SDKs do it (`auth_type = external-browser`,
//! `databricks auth login`).
//!
//! Authorization code with PKCE (S256) against the workspace's
//! `/oidc/v1/authorize` and `/oidc/v1/token`, with Databricks' public
//! client `databricks-cli`, scopes `all-apis offline_access` and the
//! loopback redirect `http://localhost:8020` that client accepts. The
//! workspace sends the user to its identity provider (Microsoft Entra ID on
//! Azure, the organization's SSO on AWS and GCP), so SSO and MFA work as on
//! the web.
//!
//! The approach is the one of SQL Server's Entra ID browser sign-in
//! (`crates/drivers/sqlserver/src/entra.rs`): the system browser opened from
//! the driver host without a shell, the `state` checked on the loopback, a
//! five-minute limit, and tokens kept in this process's memory only (never
//! on disk and never in the logs), renewed with the refresh token.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use dbine_driver::{Error, Result};
use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The `auth_mode` value.
pub const MODE: &str = "oauth_u2m";
/// Databricks' public OAuth client, enabled in every account.
const CLIENT_ID: &str = "databricks-cli";
const SCOPES: &str = "all-apis offline_access";
/// The `databricks-cli` client only takes this redirect (the CLI and SDKs
/// use the same one), so the port is fixed.
const PORT: u16 = 8020;
/// How long the browser sign-in may take.
const BROWSER_WAIT: Duration = Duration::from_secs(5 * 60);
/// A cached token is reused until this long before it expires.
const MARGIN: u64 = 5 * 60;

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn auth(msg: impl Into<String>) -> Error {
    Error::AuthFailed(msg.into())
}

/// The first line of a message, at most `max` characters.
fn short(msg: &str, max: usize) -> String {
    let line = msg
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    match line.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}

// ------------------------------------------------------------ token cache

/// An access token, when it expires (Unix seconds) and the refresh token.
#[derive(Clone)]
struct Token {
    access: String,
    expires_on: u64,
    refresh: Option<String>,
}

// Tokens never reach a log, not even through `{:?}`.
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("expires_on", &self.expires_on)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, PartialEq)]
enum Cached {
    /// Still good: use it.
    Fresh(String),
    /// Expired or about to, with a refresh token to renew it.
    Refresh(String),
    Miss,
}

fn lookup(cache: &HashMap<String, Token>, key: &str, now: u64) -> Cached {
    match cache.get(key) {
        Some(t) if now + MARGIN < t.expires_on => Cached::Fresh(t.access.clone()),
        Some(Token {
            refresh: Some(r), ..
        }) => Cached::Refresh(r.clone()),
        _ => Cached::Miss,
    }
}

/// Tokens per workspace (`https://host`, lowercase).
static CACHE: LazyLock<Mutex<HashMap<String, Token>>> = LazyLock::new(Mutex::default);
/// One sign-in at a time: sessions that open together (explorer, editor)
/// wait for the first one and take its token, and only one listener can
/// hold port 8020 anyway.
static ACQUIRE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn key(base: &str) -> String {
    base.trim().trim_end_matches('/').to_ascii_lowercase()
}

fn cache() -> std::sync::MutexGuard<'static, HashMap<String, Token>> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The workspace's OAuth endpoints.
struct Endpoints {
    authorize: String,
    token: String,
}

impl Endpoints {
    fn of(base: &str) -> Endpoints {
        let base = base.trim().trim_end_matches('/');
        Endpoints {
            authorize: format!("{base}/oidc/v1/authorize"),
            token: format!("{base}/oidc/v1/token"),
        }
    }
}

/// An access token for the workspace at `base` (`https://host`): the cached
/// one, a renewed one (refresh token) or a new browser sign-in.
pub async fn access_token(http: &reqwest::Client, base: &str) -> Result<String> {
    let _one = ACQUIRE.lock().await;
    let key = key(base);
    let refresh = match lookup(&cache(), &key, now()) {
        Cached::Fresh(t) => return Ok(t),
        Cached::Refresh(r) => Some(r),
        Cached::Miss => None,
    };
    let ep = Endpoints::of(base);
    let token = match refreshed(http, &ep, refresh).await {
        Some(t) => t,
        None => browser_sign_in(http, &ep).await?,
    };
    let access = token.access.clone();
    cache().insert(key, token);
    Ok(access)
}

/// The workspace refused `access` (401): the next request renews it with the
/// refresh token, or signs in again. A token another session already
/// renewed is left alone.
pub fn expire(base: &str, access: &str) {
    if let Some(t) = cache().get_mut(&key(base)) {
        if t.access == access {
            t.expires_on = 0;
        }
    }
}

/// A new token from a refresh token, or `None` (the caller signs in again).
async fn refreshed(
    http: &reqwest::Client,
    ep: &Endpoints,
    refresh: Option<String>,
) -> Option<Token> {
    let refresh = refresh?;
    let form = [
        ("grant_type", "refresh_token"),
        ("client_id", CLIENT_ID),
        ("refresh_token", refresh.as_str()),
        ("scope", SCOPES),
    ];
    match post_token(http, &ep.token, &form, Some(&refresh)).await {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::debug!("Databricks token refresh failed, signing in again: {e}");
            None
        }
    }
}

async fn post_token(
    http: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
    old_refresh: Option<&str>,
) -> Result<Token> {
    let resp = http.post(url).form(form).send().await.map_err(|e| {
        Error::Connect(format!(
            "no se pudo pedir el token al workspace de Databricks: {e}"
        ))
    })?;
    let status = resp.status().as_u16();
    let text = resp
        .text()
        .await
        .map_err(|e| Error::Connect(e.to_string()))?;
    parse_token(status, &text, now(), old_refresh)
}

/// The token endpoint's answer. Keeps the old refresh token when a refresh
/// answer brings none.
fn parse_token(status: u16, body: &str, now: u64, old_refresh: Option<&str>) -> Result<Token> {
    let json: Json = serde_json::from_str(body).unwrap_or(Json::Null);
    let access = json
        .get("access_token")
        .and_then(Json::as_str)
        .filter(|t| (200..300).contains(&status) && !t.is_empty());
    let Some(access) = access else {
        if status == 404 {
            return Err(auth(
                "el host no tiene el inicio de sesión OAuth de Databricks (/oidc/v1/token): revisá que sea la dirección del workspace",
            ));
        }
        let why = json
            .get("error_description")
            .or_else(|| json.get("error"))
            .or_else(|| json.get("message"))
            .and_then(Json::as_str)
            .map(|m| short(m, 300))
            .unwrap_or_else(|| format!("HTTP {status}"));
        return Err(auth(format!(
            "Databricks no entregó el token de acceso: {why}"
        )));
    };
    let expires_in = json
        .get("expires_in")
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        })
        .unwrap_or(3600);
    let refresh = json
        .get("refresh_token")
        .and_then(Json::as_str)
        .filter(|r| !r.is_empty())
        .or(old_refresh)
        .map(str::to_string);
    Ok(Token {
        access: access.to_string(),
        expires_on: now + expires_in,
        refresh,
    })
}

// ------------------------------------------------------- browser sign-in

async fn browser_sign_in(http: &reqwest::Client, ep: &Endpoints) -> Result<Token> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", PORT))
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                auth(format!(
                    "el puerto {PORT} de este equipo está ocupado y el inicio de sesión de Databricks necesita ese puerto (¿hay otro inicio de sesión abierto, como databricks auth login?): cerralo y volvé a conectar"
                ))
            } else {
                auth(format!(
                    "no se pudo esperar la respuesta del navegador: {e}"
                ))
            }
        })?;
    let redirect = format!("http://localhost:{PORT}");
    sign_in(http, ep, &listener, &redirect, open_browser).await
}

/// The whole authorization code flow on `listener`, with `open` showing the
/// sign-in page (the system browser; a fake one in the tests).
async fn sign_in(
    http: &reqwest::Client,
    ep: &Endpoints,
    listener: &tokio::net::TcpListener,
    redirect: &str,
    open: impl FnOnce(&str) -> Result<()>,
) -> Result<Token> {
    let verifier = random_b64(32)?;
    let state = random_b64(16)?;
    let url = authorize_url(&ep.authorize, redirect, &state, &pkce_challenge(&verifier))?;
    open(&url)?;
    let code = match tokio::time::timeout(BROWSER_WAIT, wait_for_code(listener, &state)).await {
        Ok(code) => code?,
        Err(_) => return Err(auth(
            "pasaron 5 minutos sin completar el inicio de sesión en el navegador (¿se cerró la ventana?); volvé a conectar",
        )),
    };
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code.as_str()),
        ("redirect_uri", redirect),
        ("code_verifier", verifier.as_str()),
        ("scope", SCOPES),
    ];
    post_token(http, &ep.token, &form, None).await
}

/// `n` random bytes, base64url without padding (32 bytes: a 43-character
/// PKCE verifier).
fn random_b64(n: usize) -> Result<String> {
    let mut bytes = vec![0u8; n];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| auth(format!("no hay números aleatorios del sistema: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The PKCE S256 challenge for a verifier (RFC 7636).
fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn authorize_url(endpoint: &str, redirect: &str, state: &str, challenge: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(endpoint)
        .map_err(|e| Error::Connect(format!("dirección del workspace no válida: {e}")))?;
    url.query_pairs_mut()
        .append_pair("client_id", CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", redirect)
        .append_pair("scope", SCOPES)
        .append_pair("state", state)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256");
    Ok(url.into())
}

/// The program and arguments that open `url` in the default browser.
/// Never a shell: the URL has `&` in it.
fn browser_argv(url: &str) -> (PathBuf, Vec<String>) {
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        (
            root.join("System32").join("rundll32.exe"),
            vec!["url.dll,FileProtocolHandler".into(), url.into()],
        )
    } else if cfg!(target_os = "macos") {
        (PathBuf::from("/usr/bin/open"), vec![url.into()])
    } else {
        (PathBuf::from("xdg-open"), vec![url.into()])
    }
}

/// Only an https workspace's own sign-in page, never something else a form
/// could slip in.
fn is_sign_in_page(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str().is_some()
            && u.path() == "/oidc/v1/authorize"
            && u.username().is_empty()
    })
}

fn open_browser(url: &str) -> Result<()> {
    if !is_sign_in_page(url) {
        return Err(auth("dirección de inicio de sesión inesperada"));
    }
    let (program, args) = browser_argv(url);
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| {
            auth(format!(
                "no se pudo abrir el navegador para iniciar sesión: {e}"
            ))
        })?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

/// What the browser brought to the loopback listener.
#[derive(Debug, PartialEq)]
enum Callback {
    Code(String),
    /// The workspace answered with an error: `error` and its description.
    Failed(String, String),
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
        return Callback::Failed(error.clone(), short(desc, 300));
    }
    Callback::Code(q["code"].clone())
}

/// The error for a redirect that came back with `error`.
fn denied(error: &str, desc: &str) -> Error {
    if error == "access_denied" {
        auth(format!(
            "se canceló el inicio de sesión o se negó el acceso en el navegador: {desc}"
        ))
    } else {
        auth(format!("Databricks rechazó el inicio de sesión: {desc}"))
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
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
            Callback::Code(_) => page(
                "200 OK",
                "Listo",
                "Ya iniciaste sesión: podés cerrar esta ventana y volver a DBine.",
            ),
            Callback::Failed(_, m) => page(
                "200 OK",
                "No se pudo iniciar sesión",
                &format!("{m}. Podés cerrar esta ventana."),
            ),
            Callback::BadState => page(
                "400 Bad Request",
                "Respuesta inesperada",
                "Esta respuesta no corresponde al inicio de sesión de DBine.",
            ),
            Callback::Ignore => page("404 Not Found", "DBine", ""),
        };
        let _ = stream.write_all(answer.as_bytes()).await;
        let _ = stream.shutdown().await;
        match callback {
            Callback::Code(code) => return Ok(code),
            Callback::Failed(error, desc) => return Err(denied(&error, &desc)),
            Callback::BadState | Callback::Ignore => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn pkce_matches_rfc_7636() {
        // RFC 7636, appendix B.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let v = random_b64(32).unwrap();
        // 43 characters of the unreserved set, and never the same twice.
        assert_eq!(v.len(), 43);
        assert!(v
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
        assert_ne!(v, random_b64(32).unwrap());
    }

    #[test]
    fn authorize_url_follows_the_databricks_docs() {
        let ep = Endpoints::of("https://adb-123.4.azuredatabricks.net/");
        assert_eq!(
            ep.token,
            "https://adb-123.4.azuredatabricks.net/oidc/v1/token"
        );
        let u = authorize_url(&ep.authorize, "http://localhost:8020", "st", "ch").unwrap();
        let url = reqwest::Url::parse(&u).unwrap();
        assert_eq!(url.host_str(), Some("adb-123.4.azuredatabricks.net"));
        assert_eq!(url.path(), "/oidc/v1/authorize");
        let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], "databricks-cli");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["redirect_uri"], "http://localhost:8020");
        assert_eq!(q["scope"], "all-apis offline_access");
        assert_eq!(q["state"], "st");
        assert_eq!(q["code_challenge"], "ch");
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(is_sign_in_page(&u));
    }

    #[test]
    fn browser_opens_only_the_sign_in_page_and_without_a_shell() {
        let url = "https://dbc-1.cloud.databricks.com/oidc/v1/authorize?a=1&b=2";
        let (program, args) = browser_argv(url);
        // The URL travels as one argument, `&` and all.
        assert_eq!(args.last().map(String::as_str), Some(url));
        let name = program
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_lowercase();
        assert!(
            ["open", "xdg-open", "rundll32.exe"].contains(&name.as_str()),
            "{name}"
        );
        for bad in [
            "https://evil.example/",
            "http://dbc-1.cloud.databricks.com/oidc/v1/authorize",
            "file:///oidc/v1/authorize",
            "https://a@dbc-1.cloud.databricks.com/oidc/v1/authorize",
        ] {
            assert!(!is_sign_in_page(bad), "{bad}");
            assert!(open_browser(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn redirects_are_checked() {
        let ok = "GET /?code=dc.AbC%2Bz&state=abc HTTP/1.1\r\nHost: localhost:8020\r\n\r\n";
        assert_eq!(parse_callback(ok, "abc"), Callback::Code("dc.AbC+z".into()));
        // Someone else's state, a missing one, also on an error.
        assert_eq!(parse_callback(ok, "abd"), Callback::BadState);
        assert_eq!(
            parse_callback("GET /?code=x HTTP/1.1\r\n\r\n", "abc"),
            Callback::BadState
        );
        assert_eq!(
            parse_callback(
                "GET /?error=access_denied&state=zzz HTTP/1.1\r\n\r\n",
                "abc"
            ),
            Callback::BadState
        );
        let denied_req = "GET /?error=access_denied&error_description=User+denied+access.%0D%0Amore&state=abc HTTP/1.1\r\n\r\n";
        assert_eq!(
            parse_callback(denied_req, "abc"),
            Callback::Failed("access_denied".into(), "User denied access.".into())
        );
        assert_eq!(
            parse_callback(
                "GET /?error=invalid_scope&state=abc HTTP/1.1\r\n\r\n",
                "abc"
            ),
            Callback::Failed("invalid_scope".into(), "invalid_scope".into())
        );
        // Not the redirect.
        for other in [
            "GET /favicon.ico HTTP/1.1\r\n\r\n",
            "GET / HTTP/1.1\r\n\r\n",
            "POST /?code=x&state=abc HTTP/1.1\r\n\r\n",
            "",
        ] {
            assert_eq!(parse_callback(other, "abc"), Callback::Ignore, "{other}");
        }
        assert!(denied("access_denied", "x")
            .to_string()
            .contains("se negó el acceso"));
        assert!(denied("server_error", "boom").to_string().contains("boom"));
    }

    #[test]
    fn the_browser_page_escapes_and_has_a_length() {
        let p = page("200 OK", "No se pudo", "<script>x</script>");
        assert!(p.starts_with("HTTP/1.1 200 OK\r\n") && !p.contains("<script>"));
        let (head, body) = p.split_once("\r\n\r\n").unwrap();
        assert!(head.contains(&format!("Content-Length: {}", body.len())));
    }

    #[test]
    fn token_responses() {
        let t = parse_token(
            200,
            r#"{"access_token":"at","refresh_token":"rt","token_type":"Bearer","expires_in":3600,"scope":"all-apis offline_access"}"#,
            1000,
            None,
        )
        .unwrap();
        assert_eq!(
            (t.access.as_str(), t.expires_on, t.refresh.as_deref()),
            ("at", 4600, Some("rt"))
        );
        // A refresh answer without a new refresh token keeps the old one;
        // `expires_in` may come as a string or be missing.
        let t = parse_token(
            200,
            r#"{"access_token":"a2","expires_in":"60"}"#,
            0,
            Some("old"),
        )
        .unwrap();
        assert_eq!((t.expires_on, t.refresh.as_deref()), (60, Some("old")));
        let t = parse_token(200, r#"{"access_token":"a3"}"#, 0, None).unwrap();
        assert_eq!((t.expires_on, t.refresh), (3600, None));
        // Errors say why, and never echo a token.
        let e = parse_token(
            400,
            r#"{"error":"invalid_grant","error_description":"Refresh token is invalid"}"#,
            0,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("Refresh token is invalid"), "{e}");
        let e = parse_token(400, r#"{"error":"invalid_client"}"#, 0, None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("invalid_client"), "{e}");
        let e = parse_token(502, "<html>bad gateway</html>", 0, None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("HTTP 502"), "{e}");
        let e = parse_token(404, "", 0, None).unwrap_err().to_string();
        assert!(e.contains("/oidc/v1/token"), "{e}");
        // A token in an error answer is not taken.
        assert!(parse_token(401, r#"{"access_token":"x"}"#, 0, None).is_err());
        // Debug output hides the tokens.
        let shown = format!(
            "{t:?}",
            t = parse_token(
                200,
                r#"{"access_token":"secret-at","refresh_token":"secret-rt"}"#,
                0,
                None
            )
            .unwrap()
        );
        assert!(!shown.contains("secret"), "{shown}");
    }

    fn tok(expires_on: u64, refresh: Option<&str>) -> Token {
        Token {
            access: "at".into(),
            expires_on,
            refresh: refresh.map(str::to_string),
        }
    }

    #[test]
    fn cache_reuses_until_five_minutes_before_expiry() {
        let mut c = HashMap::new();
        let k = key("HTTPS://Dbc-1.cloud.databricks.com/");
        assert_eq!(k, "https://dbc-1.cloud.databricks.com");
        assert_eq!(lookup(&c, &k, 0), Cached::Miss);
        c.insert(k.clone(), tok(10_000, Some("rt")));
        assert_eq!(
            lookup(&c, &k, 10_000 - MARGIN - 1),
            Cached::Fresh("at".into())
        );
        assert_eq!(
            lookup(&c, &k, 10_000 - MARGIN),
            Cached::Refresh("rt".into())
        );
        c.insert(k.clone(), tok(10_000, None));
        assert_eq!(lookup(&c, &k, 10_000), Cached::Miss);
    }

    #[test]
    fn a_refused_token_is_renewed_on_the_next_request() {
        let base = "https://expire-test.cloud.databricks.com";
        cache().insert(key(base), tok(now() + 3600, Some("rt")));
        // Another session's newer token is not touched.
        expire(base, "other");
        assert_eq!(
            lookup(&cache(), &key(base), now()),
            Cached::Fresh("at".into())
        );
        expire(base, "at");
        assert_eq!(
            lookup(&cache(), &key(base), now()),
            Cached::Refresh("rt".into())
        );
        cache().remove(&key(base));
    }

    #[tokio::test]
    async fn loopback_listener_waits_for_the_right_state() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let browser = tokio::spawn(async move {
            let mut answers = vec![];
            for req in [
                "GET /favicon.ico HTTP/1.1\r\n\r\n",
                "GET /?code=bad&state=nope HTTP/1.1\r\n\r\n",
                "GET /?code=good&state=s1 HTTP/1.1\r\n\r\n",
            ] {
                let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .unwrap();
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
        assert!(
            answers[2].starts_with("HTTP/1.1 200")
                && answers[2].contains("podés cerrar esta ventana")
        );
    }

    #[tokio::test]
    async fn a_denied_sign_in_is_reported() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            s.write_all(
                b"GET /?error=access_denied&error_description=Denied&state=s1 HTTP/1.1\r\n\r\n",
            )
            .await
            .unwrap();
            let mut out = String::new();
            let _ = s.read_to_string(&mut out).await;
        });
        let e = wait_for_code(&listener, "s1").await.unwrap_err();
        assert!(matches!(e, Error::AuthFailed(_)));
        assert!(e.to_string().contains("Denied"), "{e}");
    }

    /// A one-request token endpoint: hands the form it got to `check` and
    /// answers `status` with `body`.
    async fn fake_token_endpoint(
        check: impl FnOnce(HashMap<String, String>) + Send + 'static,
        status: &'static str,
        body: &'static str,
    ) -> String {
        let l = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = s.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len = head
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if body.len() >= len {
                        assert!(head.starts_with("POST /oidc/v1/token "), "{head}");
                        let form = reqwest::Url::parse(&format!("http://x/?{body}"))
                            .unwrap()
                            .query_pairs()
                            .into_owned()
                            .collect();
                        check(form);
                        break;
                    }
                }
                if n == 0 {
                    break;
                }
            }
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            s.write_all(resp.as_bytes()).await.unwrap();
            let _ = s.shutdown().await;
        });
        format!("http://127.0.0.1:{port}")
    }

    #[tokio::test]
    async fn full_sign_in_against_a_fake_workspace() {
        let challenge = Arc::new(Mutex::new(String::new()));
        let seen = challenge.clone();
        let base = fake_token_endpoint(
            move |form| {
                assert_eq!(form["grant_type"], "authorization_code");
                assert_eq!(form["client_id"], "databricks-cli");
                assert_eq!(form["code"], "the-code");
                assert_eq!(form["scope"], "all-apis offline_access");
                assert!(form["redirect_uri"].starts_with("http://localhost:"));
                // The verifier matches the challenge the browser got.
                assert_eq!(
                    pkce_challenge(&form["code_verifier"]),
                    *seen.lock().unwrap()
                );
            },
            "200 OK",
            r#"{"access_token":"dapi-at","refresh_token":"rt-1","token_type":"Bearer","expires_in":3600}"#,
        )
        .await;
        let ep = Endpoints::of(&base);
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let redirect = format!("http://localhost:{}", listener.local_addr().unwrap().port());
        let http = reqwest::Client::new();
        let got = challenge.clone();
        let token = sign_in(&http, &ep, &listener, &redirect, move |url| {
            // The "browser": the workspace signs the user in and redirects
            // back with the code and the same state.
            let u = reqwest::Url::parse(url).unwrap();
            let q: HashMap<_, _> = u.query_pairs().into_owned().collect();
            *got.lock().unwrap() = q["code_challenge"].clone();
            let back = format!(
                "{}/?code=the-code&state={}",
                q["redirect_uri"].replace("localhost", "127.0.0.1"),
                q["state"]
            );
            tokio::spawn(async move {
                let page = reqwest::get(back).await.unwrap().text().await.unwrap();
                assert!(page.contains("Ya iniciaste sesión"));
            });
            Ok(())
        })
        .await
        .unwrap();
        assert_eq!(token.access, "dapi-at");
        assert_eq!(token.refresh.as_deref(), Some("rt-1"));
        assert!(token.expires_on > now() + 3000);
    }

    #[tokio::test]
    async fn refresh_against_a_fake_workspace() {
        let base = fake_token_endpoint(
            |form| {
                assert_eq!(form["grant_type"], "refresh_token");
                assert_eq!(form["client_id"], "databricks-cli");
                assert_eq!(form["refresh_token"], "rt-1");
            },
            "200 OK",
            r#"{"access_token":"at-2","expires_in":3600}"#,
        )
        .await;
        let http = reqwest::Client::new();
        let t = refreshed(&http, &Endpoints::of(&base), Some("rt-1".into()))
            .await
            .unwrap();
        assert_eq!(
            (t.access.as_str(), t.refresh.as_deref()),
            ("at-2", Some("rt-1"))
        );
        // A refused refresh token means signing in again.
        let base = fake_token_endpoint(
            |_| {},
            "400 Bad Request",
            r#"{"error":"invalid_grant","error_description":"expired"}"#,
        )
        .await;
        assert!(refreshed(&http, &Endpoints::of(&base), Some("rt-1".into()))
            .await
            .is_none());
        assert!(refreshed(&http, &Endpoints::of(&base), None)
            .await
            .is_none());
    }
}
