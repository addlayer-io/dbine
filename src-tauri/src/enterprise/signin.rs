//! Signing in to DBine Enterprise as a native app (RFC 8252): the system
//! browser opens the server's sign-in with a PKCE challenge and a loopback
//! redirect to this machine; the company's SSO happens there; the browser
//! comes back with a one-time code, which only this run's verifier redeems.

use crate::error::{CommandError, CommandResult};
use base64::engine::general_purpose::STANDARD as STANDARD_B64;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tauri::AppHandle;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long the browser has to come back.
const WAIT: Duration = Duration::from_secs(300);

pub struct SignedIn {
    pub token: String,
    pub expires_at: DateTime<Utc>,
    pub email: String,
    pub name: String,
}

fn random(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("the OS's random numbers");
    URL_SAFE_NO_PAD.encode(b)
}

pub async fn run(
    app: &AppHandle,
    http: &reqwest::Client,
    server: &str,
    done_page: &str,
    device_public: &[u8],
    device_name: &str,
    fresh: bool,
) -> CommandResult<SignedIn> {
    let verifier = random(48);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random(24);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| CommandError::Internal(format!("no se pudo esperar al navegador: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| CommandError::Internal(e.to_string()))?
        .port();
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let mut start = url::Url::parse(&format!("{server}/auth/dbine/start"))
        .map_err(|_| CommandError::BadRequest("la dirección del servidor no es válida".into()))?;
    start
        .query_pairs_mut()
        .append_pair("redirect_uri", &redirect)
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");
    if fresh {
        start.query_pairs_mut().append_pair("fresh", "1");
    }
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(start.as_str(), None::<&str>)
        .map_err(|e| CommandError::Internal(format!("no se pudo abrir el navegador: {e}")))?;

    let code = tokio::time::timeout(WAIT, wait_for_code(&listener, &state, done_page))
        .await
        .map_err(|_| {
            CommandError::BadRequest("el inicio de sesión no terminó a tiempo".into())
        })??;

    let r = http
        .post(format!("{server}/api/v1/auth/token"))
        .json(&serde_json::json!({
            "code": code,
            "code_verifier": verifier,
            // The token works only from this computer (DBine Bastion checks it).
            "device_key": STANDARD_B64.encode(device_public),
            "device_name": device_name,
        }))
        .send()
        .await
        .map_err(|e| CommandError::Connect(format!("DBine Enterprise no responde ({e})")))?;
    if !r.status().is_success() {
        return Err(CommandError::AuthFailed(
            "DBine Enterprise no aceptó el inicio de sesión".into(),
        ));
    }
    let v: serde_json::Value = r
        .json()
        .await
        .map_err(|e| CommandError::Internal(e.to_string()))?;
    let token = v["token"]
        .as_str()
        .ok_or_else(|| CommandError::Internal("DBine Enterprise no devolvió un token".into()))?
        .to_string();
    let expires_at = v["expires_at"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|| Utc::now() + chrono::Duration::days(7));
    Ok(SignedIn {
        token,
        expires_at,
        email: v["email"].as_str().unwrap_or_default().to_string(),
        name: v["name"].as_str().unwrap_or_default().to_string(),
    })
}

/// Answers the browser's requests to the loopback address until one brings
/// this sign-in's code. Anything else (a favicon, another state) gets a 404
/// and is ignored.
async fn wait_for_code(
    listener: &tokio::net::TcpListener,
    state: &str,
    done_page: &str,
) -> CommandResult<String> {
    loop {
        let (mut sock, _) = listener
            .accept()
            .await
            .map_err(|e| CommandError::Internal(e.to_string()))?;
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]);
        let path = head
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("");
        let params = url::Url::parse(&format!("http://127.0.0.1{path}"))
            .ok()
            .filter(|u| u.path() == "/callback")
            .map(|u| {
                u.query_pairs()
                    .into_owned()
                    .collect::<std::collections::HashMap<String, String>>()
            });
        let found = params
            .as_ref()
            .filter(|p| p.get("state").map(String::as_str) == Some(state))
            .and_then(|p| p.get("code").cloned());
        let (status, body) = match &found {
            Some(_) => ("200 OK", page(done_page)),
            None => ("404 Not Found", String::new()),
        };
        let reply = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = sock.write_all(reply.as_bytes()).await;
        let _ = sock.shutdown().await;
        if let Some(code) = found {
            return Ok(code);
        }
    }
}

/// The browser's page once signed in: the UI's text, escaped.
fn page(text: &str) -> String {
    let t = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>DBine</title></head>\
         <body style=\"font-family:system-ui,sans-serif;display:flex;align-items:center;justify-content:center;height:90vh;color:#333\">\
         <p style=\"font-size:18px\">{t}</p></body></html>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn only_this_sign_ins_code_is_taken() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let waiting =
            tokio::spawn(async move { wait_for_code(&listener, "s1", "Listo <b>").await });
        let get = |path: &str| {
            let path = path.to_string();
            async move {
                let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .unwrap();
                s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
                let mut out = String::new();
                s.read_to_string(&mut out).await.unwrap();
                out
            }
        };
        assert!(get("/favicon.ico").await.starts_with("HTTP/1.1 404"));
        assert!(get("/callback?code=evil&state=other")
            .await
            .starts_with("HTTP/1.1 404"));
        let ok = get("/callback?code=abc&state=s1").await;
        assert!(ok.starts_with("HTTP/1.1 200"));
        assert!(ok.contains("Listo &lt;b&gt;"));
        assert_eq!(waiting.await.unwrap().unwrap(), "abc");
    }
}
