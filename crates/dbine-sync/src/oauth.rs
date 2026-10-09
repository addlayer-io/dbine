//! OAuth 2.0 for a desktop app (RFC 8252): the system browser signs the user
//! in with Google or Microsoft and redirects to a one-shot listener on the
//! loopback interface; the code is exchanged with PKCE (RFC 7636). DBine
//! never sees the account's password, only a token limited to its own app
//! folder, kept in the OS keychain.

use crate::provider::{net_error, ProviderKind};
use crate::{Result, SyncError};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

/// The app registrations (docs/sync.md explains how to create
/// them). Build-time env vars, overridable by a JSON file at runtime.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientIds {
    #[serde(default)]
    pub google_client_id: Option<String>,
    /// Google issues one to "Desktop app" clients; it isn't secret there
    /// (it ships in the app), PKCE is what protects the exchange.
    #[serde(default)]
    pub google_client_secret: Option<String>,
    #[serde(default)]
    pub microsoft_client_id: Option<String>,
}

impl ClientIds {
    pub fn from_build() -> Self {
        let s = |v: Option<&'static str>| v.map(str::to_string).filter(|v| !v.trim().is_empty());
        Self {
            google_client_id: s(option_env!("DBINE_GOOGLE_CLIENT_ID")),
            google_client_secret: s(option_env!("DBINE_GOOGLE_CLIENT_SECRET")),
            microsoft_client_id: s(option_env!("DBINE_MICROSOFT_CLIENT_ID")),
        }
    }

    /// Values from `other` win where set.
    pub fn overridden_by(self, other: ClientIds) -> Self {
        Self {
            google_client_id: other.google_client_id.or(self.google_client_id),
            google_client_secret: other.google_client_secret.or(self.google_client_secret),
            microsoft_client_id: other.microsoft_client_id.or(self.microsoft_client_id),
        }
    }

    pub fn config(&self, kind: ProviderKind) -> Option<OAuthConfig> {
        match kind {
            ProviderKind::GoogleDrive => self.google_client_id.as_ref().map(|id| OAuthConfig::google(id, self.google_client_secret.clone())),
            ProviderKind::Onedrive => self.microsoft_client_id.as_ref().map(|id| OAuthConfig::microsoft(id)),
            ProviderKind::Folder => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OAuthConfig {
    pub provider: ProviderKind,
    pub auth_url: String,
    pub token_url: String,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub scopes: Vec<&'static str>,
    /// Host in the redirect URI: Google wants `127.0.0.1`, Microsoft's
    /// desktop registrations use `localhost` (any port).
    pub redirect_host: &'static str,
    pub extra_params: Vec<(&'static str, &'static str)>,
}

impl OAuthConfig {
    pub fn google(client_id: &str, client_secret: Option<String>) -> Self {
        Self {
            provider: ProviderKind::GoogleDrive,
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
            client_id: client_id.into(),
            client_secret,
            // Only the app's hidden folder: DBine can't see any other file.
            scopes: vec!["https://www.googleapis.com/auth/drive.appdata"],
            redirect_host: "127.0.0.1",
            // A refresh token every time (Google only sends it on consent).
            extra_params: vec![("access_type", "offline"), ("prompt", "consent")],
        }
    }

    pub fn microsoft(client_id: &str) -> Self {
        Self {
            provider: ProviderKind::Onedrive,
            auth_url: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize".into(),
            token_url: "https://login.microsoftonline.com/common/oauth2/v2.0/token".into(),
            client_id: client_id.into(),
            client_secret: None,
            // Only the app's own folder (Apps/DBine), plus the name to show.
            scopes: vec!["Files.ReadWrite.AppFolder", "User.Read", "offline_access"],
            redirect_host: "localhost",
            extra_params: vec![("prompt", "select_account")],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix seconds.
    pub expires_at: i64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

fn random_b64(n: usize) -> String {
    let mut b = vec![0u8; n];
    rand::thread_rng().fill_bytes(&mut b);
    URL_SAFE_NO_PAD.encode(b)
}

/// A sign-in in progress: open `auth_url` in the browser, then `finish`.
pub struct PendingLogin {
    pub auth_url: String,
    cfg: OAuthConfig,
    listeners: Vec<TcpListener>,
    redirect_uri: String,
    verifier: String,
    state: String,
}

pub async fn begin(cfg: OAuthConfig) -> Result<PendingLogin> {
    let v4 = TcpListener::bind("127.0.0.1:0").await?;
    let port = v4.local_addr()?.port();
    let mut listeners = vec![v4];
    // `localhost` may resolve to ::1 in the browser: listen there too.
    if cfg.redirect_host == "localhost" {
        if let Ok(v6) = TcpListener::bind(("::1", port)).await {
            listeners.push(v6);
        }
    }
    let redirect_uri = format!("http://{}:{port}", cfg.redirect_host);
    let verifier = random_b64(48);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_b64(16);
    let mut url = url::Url::parse(&cfg.auth_url).map_err(|e| SyncError::Format(e.to_string()))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", &cfg.client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("response_type", "code")
            .append_pair("scope", &cfg.scopes.join(" "))
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state);
        for (k, v) in &cfg.extra_params {
            q.append_pair(k, v);
        }
    }
    Ok(PendingLogin { auth_url: url.into(), cfg, listeners, redirect_uri, verifier, state })
}

const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>DBine</title>\
<body style=\"font-family:-apple-system,Segoe UI,sans-serif;background:#1e1f22;color:#ddd;display:grid;place-items:center;height:90vh\">\
<div style=\"text-align:center\"><h2>Listo</h2><p>La cuenta quedó conectada. Ya podés cerrar esta pestaña y volver a DBine.</p></div>";
const FAIL_PAGE: &str = "<!doctype html><meta charset=utf-8><title>DBine</title>\
<body style=\"font-family:-apple-system,Segoe UI,sans-serif;background:#1e1f22;color:#ddd;display:grid;place-items:center;height:90vh\">\
<div style=\"text-align:center\"><h2>No se conectó la cuenta</h2><p>Volvé a DBine para ver el motivo.</p></div>";

impl PendingLogin {
    /// Wait for the browser's redirect (up to `timeout`) and exchange the
    /// code for tokens.
    pub async fn finish(self, http: &reqwest::Client, timeout: Duration) -> Result<Tokens> {
        let code = tokio::time::timeout(timeout, self.wait_code())
            .await
            .map_err(|_| SyncError::Remote("se agotó el tiempo esperando el inicio de sesión en el navegador".into()))??;
        let mut form = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code),
            ("redirect_uri", self.redirect_uri.clone()),
            ("client_id", self.cfg.client_id.clone()),
            ("code_verifier", self.verifier.clone()),
        ];
        if let Some(s) = &self.cfg.client_secret {
            form.push(("client_secret", s.clone()));
        }
        token_request(http, &self.cfg, &form, None).await
    }

    async fn wait_code(&self) -> Result<String> {
        loop {
            let (mut sock, _) = match self.listeners.as_slice() {
                [a] => a.accept().await?,
                [a, b, ..] => tokio::select! { r = a.accept() => r?, r = b.accept() => r? },
                [] => return Err(SyncError::Local("sin puerto local".into())),
            };
            let mut buf = vec![0u8; 8192];
            let mut n = 0;
            while n < buf.len() {
                let read = sock.read(&mut buf[n..]).await?;
                if read == 0 {
                    break;
                }
                n += read;
                if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let req = String::from_utf8_lossy(&buf[..n]);
            let target = req.split_whitespace().nth(1).unwrap_or("/");
            let Ok(url) = url::Url::parse(&format!("http://h{target}")) else { continue };
            let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
            // The browser also asks for /favicon.ico and such.
            if !q.contains_key("code") && !q.contains_key("error") {
                let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                continue;
            }
            let ok = q.get("state") == Some(&self.state) && q.contains_key("code");
            let page = if ok { DONE_PAGE } else { FAIL_PAGE };
            let _ = sock
                .write_all(
                    format!("HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}", page.len())
                        .as_bytes(),
                )
                .await;
            let _ = sock.shutdown().await;
            if let Some(err) = q.get("error") {
                let desc = q.get("error_description").cloned().unwrap_or_default();
                return Err(if err == "access_denied" {
                    SyncError::Cancelled
                } else {
                    SyncError::Remote(format!("{} rechazó el inicio de sesión: {err} {desc}", self.cfg.provider.label()))
                });
            }
            if q.get("state") != Some(&self.state) {
                return Err(SyncError::Remote("respuesta de inicio de sesión inválida (state)".into()));
            }
            return Ok(q["code"].clone());
        }
    }
}

async fn token_request(http: &reqwest::Client, cfg: &OAuthConfig, form: &[(&str, String)], old_refresh: Option<&str>) -> Result<Tokens> {
    let resp = http.post(&cfg.token_url).form(form).send().await.map_err(|e| net_error(cfg.provider, e))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body: serde_json::Value = resp.json().await.unwrap_or_default();
        let code = body.get("error").and_then(|v| v.as_str()).unwrap_or_default();
        // A revoked or expired refresh token: the user has to sign in again.
        if code == "invalid_grant" || status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(SyncError::AuthRequired(cfg.provider.label().into()));
        }
        let desc = body.get("error_description").and_then(|v| v.as_str()).unwrap_or(code);
        return Err(SyncError::Remote(format!("{}: {desc}", cfg.provider.label())));
    }
    let t: TokenResponse = resp.json().await.map_err(|e| net_error(cfg.provider, e))?;
    Ok(Tokens {
        access_token: t.access_token,
        // Google doesn't send a new one on refresh: keep the old.
        refresh_token: t.refresh_token.or_else(|| old_refresh.map(str::to_string)),
        expires_at: chrono::Utc::now().timestamp() + t.expires_in.unwrap_or(3600),
    })
}

pub async fn refresh(http: &reqwest::Client, cfg: &OAuthConfig, refresh_token: &str) -> Result<Tokens> {
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("refresh_token", refresh_token.to_string()),
        ("client_id", cfg.client_id.clone()),
    ];
    if let Some(s) = &cfg.client_secret {
        form.push(("client_secret", s.clone()));
    }
    if cfg.provider == ProviderKind::Onedrive {
        form.push(("scope", cfg.scopes.join(" ")));
    }
    token_request(http, cfg, &form, Some(refresh_token)).await
}

/// Hands out a valid access token, refreshing it when due; `on_update`
/// persists refreshed tokens (the keychain).
pub struct TokenSource {
    pub cfg: OAuthConfig,
    pub http: reqwest::Client,
    tokens: Mutex<Tokens>,
    on_update: Arc<dyn Fn(&Tokens) + Send + Sync>,
}

impl TokenSource {
    pub fn new(cfg: OAuthConfig, http: reqwest::Client, tokens: Tokens, on_update: Arc<dyn Fn(&Tokens) + Send + Sync>) -> Self {
        Self { cfg, http, tokens: Mutex::new(tokens), on_update }
    }

    /// A token good for at least another minute (`force` refreshes anyway,
    /// after a 401).
    pub async fn bearer(&self, force: bool) -> Result<String> {
        let mut t = self.tokens.lock().await;
        if force || t.expires_at - 60 <= chrono::Utc::now().timestamp() {
            let rt = t.refresh_token.clone().ok_or_else(|| SyncError::AuthRequired(self.cfg.provider.label().into()))?;
            *t = refresh(&self.http, &self.cfg, &rt).await?;
            (self.on_update)(&t);
        }
        Ok(t.access_token.clone())
    }
}

/// Send a request with the bearer token; on a 401, refresh once and retry.
pub(crate) async fn authed(
    src: &TokenSource,
    build: impl Fn(&str) -> reqwest::RequestBuilder,
) -> Result<reqwest::Response> {
    let token = src.bearer(false).await?;
    let resp = build(&token).send().await.map_err(|e| net_error(src.cfg.provider, e))?;
    if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Ok(resp);
    }
    let token = src.bearer(true).await?;
    build(&token).send().await.map_err(|e| net_error(src.cfg.provider, e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn loopback_receives_the_code() {
        let cfg = OAuthConfig::google("cid", None);
        let login = begin(cfg).await.unwrap();
        let url = url::Url::parse(&login.auth_url).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["scope"], "https://www.googleapis.com/auth/drive.appdata");
        let redirect = q["redirect_uri"].clone();
        let state = q["state"].clone();
        let browser = tokio::spawn(async move {
            let c = reqwest::Client::new();
            let _ = c.get(format!("{redirect}/favicon.ico")).send().await;
            c.get(format!("{redirect}/?state={state}&code=abc")).send().await.unwrap().text().await.unwrap()
        });
        assert_eq!(login.wait_code().await.unwrap(), "abc");
        assert!(browser.await.unwrap().contains("Listo"));
    }

    #[tokio::test]
    async fn a_forged_state_is_refused() {
        let login = begin(OAuthConfig::google("cid", None)).await.unwrap();
        let q: std::collections::HashMap<_, _> = url::Url::parse(&login.auth_url).unwrap().query_pairs().into_owned().collect();
        let redirect = q["redirect_uri"].clone();
        tokio::spawn(async move { reqwest::get(format!("{redirect}/?state=otro&code=abc")).await });
        assert!(login.wait_code().await.is_err());
    }

    #[tokio::test]
    async fn access_denied_is_a_cancel() {
        let login = begin(OAuthConfig::microsoft("cid")).await.unwrap();
        let q: std::collections::HashMap<_, _> = url::Url::parse(&login.auth_url).unwrap().query_pairs().into_owned().collect();
        assert!(q["redirect_uri"].starts_with("http://localhost:"));
        let redirect = q["redirect_uri"].replace("localhost", "127.0.0.1");
        let state = q["state"].clone();
        tokio::spawn(async move { reqwest::get(format!("{redirect}/?state={state}&error=access_denied")).await });
        assert!(matches!(login.wait_code().await, Err(SyncError::Cancelled)));
    }
}
