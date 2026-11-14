//! Interactive browser sign-in (`entra_mfa`, spec §3.2). Owned by part B.
//!
//! Authorization code + PKCE (S256) against
//! `{login}/{tenant|organizations}/oauth2/v2.0/authorize`, loopback
//! redirect `http://localhost:{port}`, `state` checked, `login_hint` from
//! the username (`prompt=select_account` without one), five-minute limit.
//! The browser opens only the selected cloud's authorize URL, compared by
//! parsed host (S3), without a shell. Consent errors (`AADSTS650057`,
//! `AADSTS65001`, `AADSTS90094`, `AADSTS53003`) become the Spanish message
//! that names the Azure CLI client as the fallback.

use super::{auth, scopes, short, token_endpoint, Account, Audience, Endpoints, TokenSet};
use crate::auth::cloud::Cloud;
use crate::config::{ClientApp, Secret, Settings};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use dbine_driver::{Error, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// DBine Database Sign-in: a public client (its id is no secret).
pub const DBINE_APP: &str = "b46a5260-9219-40ba-9c1b-5e45a1a6afe7";
/// The Azure CLI's first-party public client.
pub const AZURE_CLI_APP: &str = "04b07795-8ddb-461a-bbee-02f9e1bf7b46";

/// How long the browser sign-in may take.
const BROWSER_WAIT: Duration = Duration::from_secs(5 * 60);

fn tenant(settings: &Settings) -> &str {
    settings.tenant.as_deref().unwrap_or("organizations")
}

/// Sign in with the browser and get the tokens for `audience`.
pub async fn sign_in(settings: &Settings, audience: Audience) -> Result<TokenSet> {
    sign_in_at(settings, &Endpoints::of(settings.cloud), audience, None).await
}

pub(crate) async fn sign_in_at(
    settings: &Settings,
    ep: &Endpoints,
    audience: Audience,
    account: Option<Account>,
) -> Result<TokenSet> {
    let (verifier, challenge) = pkce()?;
    let state = random_b64(16)?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| {
            auth(format!(
                "no se pudo esperar la respuesta del navegador: {e}"
            ))
        })?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://localhost:{port}");
    let hint = account.and_then(|a| a.upn);
    let url = authorize_url_with(
        settings,
        ep,
        audience,
        &redirect,
        &state,
        &challenge,
        hint.as_deref(),
    )?;
    open_browser(settings.cloud, tenant(settings), &url)?;
    let code = match tokio::time::timeout(BROWSER_WAIT, wait_for_code(&listener, &state)).await {
        Ok(code) => code.map_err(|e| match e {
            Callback::Failed(body) => sign_in_error_for(&body, &settings.client_app),
            _ => auth("el inicio de sesión no terminó"),
        })?,
        Err(_) => return Err(auth(
            "pasaron 5 minutos sin completar el inicio de sesión en el navegador; volvé a conectar",
        )),
    };
    exchange_code(settings, ep, audience, &code, &redirect, &verifier).await
}

/// The authorization code for tokens.
pub(crate) async fn exchange_code(
    settings: &Settings,
    ep: &Endpoints,
    audience: Audience,
    code: &str,
    redirect: &str,
    verifier: &Secret,
) -> Result<TokenSet> {
    let client = super::client_id(settings);
    let scope = scopes(settings, audience).join(" ");
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", client.as_str()),
        ("code", code),
        ("redirect_uri", redirect),
        ("code_verifier", verifier.expose()),
        ("scope", scope.as_str()),
    ];
    token_endpoint(settings, &ep.login, tenant(settings), &form).await
}

/// A new access token from a refresh token (same client and scopes).
pub async fn refresh(
    settings: &Settings,
    audience: Audience,
    refresh: &Secret,
) -> Result<TokenSet> {
    refresh_at(settings, &Endpoints::of(settings.cloud), audience, refresh).await
}

pub(crate) async fn refresh_at(
    settings: &Settings,
    ep: &Endpoints,
    audience: Audience,
    refresh: &Secret,
) -> Result<TokenSet> {
    let client = super::client_id(settings);
    let scope = scopes(settings, audience).join(" ");
    let form = [
        ("grant_type", "refresh_token"),
        ("client_id", client.as_str()),
        ("refresh_token", refresh.expose()),
        ("scope", scope.as_str()),
    ];
    token_endpoint(settings, &ep.login, tenant(settings), &form).await
}

/// `n` random bytes, base64url without padding.
fn random_b64(n: usize) -> Result<String> {
    let mut bytes = vec![0u8; n];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| auth(format!("no hay números aleatorios del sistema: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// The PKCE S256 challenge for a verifier (RFC 7636).
fn challenge_of(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// PKCE: a random verifier and its S256 challenge (RFC 7636).
pub fn pkce() -> Result<(Secret, String)> {
    let verifier = random_b64(32)?;
    let challenge = challenge_of(&verifier);
    Ok((Secret::new(verifier), challenge))
}

/// The authorize URL for the cloud, client and scopes, with `state` and
/// the PKCE challenge.
pub fn authorize_url(
    settings: &Settings,
    audience: Audience,
    redirect: &str,
    state: &str,
    challenge: &str,
) -> Result<String> {
    authorize_url_with(
        settings,
        &Endpoints::of(settings.cloud),
        audience,
        redirect,
        state,
        challenge,
        None,
    )
}

fn authorize_url_with(
    settings: &Settings,
    ep: &Endpoints,
    audience: Audience,
    redirect: &str,
    state: &str,
    challenge: &str,
    hint: Option<&str>,
) -> Result<String> {
    let tenant = tenant(settings);
    crate::config::valid_tenant(tenant)?;
    let bad = || auth("dirección de inicio de sesión no válida");
    let mut url = reqwest::Url::parse(&ep.login).map_err(|_| bad())?;
    url.path_segments_mut()
        .map_err(|_| bad())?
        .extend([tenant, "oauth2", "v2.0", "authorize"]);
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", &super::client_id(settings))
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect)
            .append_pair("response_mode", "query")
            .append_pair("scope", &scopes(settings, audience).join(" "))
            .append_pair("state", state)
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256");
        match hint.or(settings.username.as_deref()) {
            Some(user) => {
                q.append_pair("login_hint", user);
            }
            // Without a user, the account is chosen, never assumed (S2).
            None => {
                q.append_pair("prompt", "select_account");
            }
        }
    }
    Ok(url.into())
}

/// Whether `url` is the selected cloud's authorize page: `https`, the
/// cloud's login host exactly, the default port, `/{tenant}/oauth2/v2.0/
/// authorize` (S3: compared parsed, never by prefix).
pub(crate) fn is_authorize_url(cloud: Cloud, tenant: &str, url: &str) -> bool {
    let (Ok(u), Ok(login)) = (reqwest::Url::parse(url), reqwest::Url::parse(cloud.login())) else {
        return false;
    };
    u.scheme() == "https"
        && u.host_str().is_some()
        && u.host_str() == login.host_str()
        && u.port().is_none()
        && u.username().is_empty()
        && u.password().is_none()
        && u.path() == format!("/{tenant}/oauth2/v2.0/authorize")
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

fn open_browser(cloud: Cloud, tenant: &str, url: &str) -> Result<()> {
    // Only the cloud's sign-in page, never something a form could slip in.
    if !is_authorize_url(cloud, tenant, url) {
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
    /// Entra ID answered with an error (as `{error, error_description}`).
    Failed(Value),
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
        return Callback::Failed(json!({
            "error": error,
            "error_description": q.get("error_description").unwrap_or(error),
        }));
    }
    Callback::Code(q["code"].clone())
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
/// `state` arrives: the code, or `Callback::Failed` with Entra's error.
async fn wait_for_code(
    listener: &tokio::net::TcpListener,
    state: &str,
) -> std::result::Result<String, Callback> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            return Err(Callback::Ignore);
        };
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
            Callback::Failed(body) => page(
                "200 OK",
                "No se pudo iniciar sesión",
                &format!(
                    "{}. Podés cerrar esta ventana.",
                    short(
                        body.get("error_description")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        300
                    )
                ),
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
            f @ Callback::Failed(_) => return Err(f),
            Callback::BadState | Callback::Ignore => {}
        }
    }
}

/// The Spanish message for an Entra error answer (consent, conditional
/// access…), naming what to do.
pub fn sign_in_error(body: &Value) -> Error {
    sign_in_error_for(body, &ClientApp::Dbine)
}

/// [`sign_in_error`] for the client the connection uses: the Azure CLI
/// client is named as the fallback only when it isn't the one that failed.
pub(crate) fn sign_in_error_for(body: &Value, client: &ClientApp) -> Error {
    let desc = body
        .get("error_description")
        .or_else(|| body.get("error"))
        .and_then(Value::as_str)
        .unwrap_or("respuesta inesperada de Microsoft Entra ID");
    let has = |code: &str| desc.contains(code);
    if has("AADSTS650057") || has("AADSTS65001") || has("AADSTS90094") {
        let code = ["AADSTS650057", "AADSTS65001", "AADSTS90094"]
            .into_iter()
            .find(|c| has(c))
            .unwrap_or_default();
        return auth(match client {
            ClientApp::Dbine => format!(
                "La aplicación de DBine todavía no tiene permiso para Power BI en este inquilino ({code}). Pedile a un administrador que la apruebe, o elegí «Cliente de Azure CLI» en Opciones avanzadas."
            ),
            ClientApp::AzureCli => format!(
                "El cliente de Azure CLI no tiene permiso para Power BI en este inquilino ({code}). Pedile a un administrador que apruebe la aplicación de DBine y elegí «DBine» en Opciones avanzadas."
            ),
            ClientApp::Custom(_) => format!(
                "Tu aplicación no tiene los permisos delegados de Power BI aprobados en este inquilino ({code}). Pedile a un administrador que los apruebe."
            ),
        });
    }
    if has("AADSTS53003") {
        return auth(
            "El acceso condicional de tu organización bloqueó el inicio de sesión (AADSTS53003). Pedile a un administrador que permita esta aplicación para Power BI.",
        );
    }
    auth(format!(
        "Microsoft Entra ID rechazó el inicio de sesión: {}",
        short(desc, 300)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::tests::settings;

    #[test]
    fn pkce_matches_rfc_7636() {
        // RFC 7636, appendix B.
        assert_eq!(
            challenge_of("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let (v, c) = pkce().unwrap();
        assert_eq!(v.expose().len(), 43);
        assert_eq!(challenge_of(v.expose()), c);
        assert_ne!(v.expose(), pkce().unwrap().0.expose());
        assert!(!format!("{v:?}").contains(v.expose()));
    }

    #[test]
    fn authorize_urls_per_cloud_and_client() {
        let s = settings(&[], true);
        let u = authorize_url(&s, Audience::PowerBi, "http://localhost:5123", "st", "ch").unwrap();
        let url = reqwest::Url::parse(&u).unwrap();
        assert_eq!(url.host_str(), Some("login.microsoftonline.com"));
        assert_eq!(url.path(), "/organizations/oauth2/v2.0/authorize");
        let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], DBINE_APP);
        assert_eq!(q["redirect_uri"], "http://localhost:5123");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(
            (q["state"].as_str(), q["code_challenge"].as_str()),
            ("st", "ch")
        );
        assert!(q["scope"].contains("https://analysis.windows.net/powerbi/api/Dataset.Read.All"));
        assert!(!q["scope"].contains("ReadWrite"));
        // No user: the account is chosen, not assumed.
        assert_eq!(q["prompt"], "select_account");
        assert!(!q.contains_key("login_hint"));
        assert!(is_authorize_url(Cloud::Public, "organizations", &u));
        // A user: the hint.
        let mut s = settings(&[("tenant", "contoso.onmicrosoft.com")], false);
        s.username = Some("ana@contoso.com".into());
        let u = authorize_url(&s, Audience::PowerBi, "http://localhost:1", "s", "c").unwrap();
        let q: HashMap<_, _> = reqwest::Url::parse(&u)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        assert_eq!(q["login_hint"], "ana@contoso.com");
        assert!(!q.contains_key("prompt"));
        assert!(q["scope"].contains("Dataset.ReadWrite.All"));
        assert!(is_authorize_url(
            Cloud::Public,
            "contoso.onmicrosoft.com",
            &u
        ));
        // The Azure CLI client asks `.default`; a sovereign cloud its host.
        let cli = settings(&[("client_app", "azure_cli")], true);
        let u = authorize_url(&cli, Audience::PowerBi, "http://localhost:1", "s", "c").unwrap();
        let q: HashMap<_, _> = reqwest::Url::parse(&u)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        assert_eq!(q["client_id"], AZURE_CLI_APP);
        assert_eq!(
            q["scope"],
            "https://analysis.windows.net/powerbi/api/.default offline_access"
        );
        let dod = settings(
            &[
                ("cloud", "dod"),
                ("client_app", "custom"),
                ("app_client_id", "my-app"),
            ],
            true,
        );
        let u = authorize_url(&dod, Audience::PowerBi, "http://localhost:1", "s", "c").unwrap();
        assert!(u.starts_with(
            "https://login.microsoftonline.us/organizations/oauth2/v2.0/authorize?client_id=my-app"
        ));
        assert!(u.contains("mil.analysis.usgovcloudapi.net"));
        assert!(is_authorize_url(Cloud::Dod, "organizations", &u));
        assert!(!is_authorize_url(Cloud::Public, "organizations", &u));
    }

    #[test]
    fn the_browser_opens_only_the_clouds_sign_in_page() {
        let ok = "https://login.microsoftonline.com/organizations/oauth2/v2.0/authorize?a=1";
        assert!(is_authorize_url(Cloud::Public, "organizations", ok));
        for bad in [
            "https://login.microsoftonline.com.evil.example/organizations/oauth2/v2.0/authorize",
            "http://login.microsoftonline.com/organizations/oauth2/v2.0/authorize",
            "https://login.microsoftonline.com:8443/organizations/oauth2/v2.0/authorize",
            "https://user@login.microsoftonline.com/organizations/oauth2/v2.0/authorize",
            "https://login.microsoftonline.com/organizations/oauth2/v2.0/token",
            "https://login.microsoftonline.com/other/oauth2/v2.0/authorize",
            "https://evil.example/",
            "file:///etc/passwd",
        ] {
            assert!(
                !is_authorize_url(Cloud::Public, "organizations", bad),
                "{bad}"
            );
            assert!(open_browser(Cloud::Public, "organizations", bad).is_err());
        }
        // The URL travels as one argument, `&` and all, never through a shell.
        let (program, args) = browser_argv(ok);
        assert_eq!(args.last().map(String::as_str), Some(ok));
        let name = program
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_lowercase();
        assert!(
            ["open", "xdg-open", "rundll32.exe"].contains(&name.as_str()),
            "{name}"
        );
    }

    #[test]
    fn redirects_are_checked() {
        let ok = "GET /?code=0.AXo%2Bz&state=abc&session_state=x HTTP/1.1\r\nHost: localhost:5123\r\n\r\n";
        assert_eq!(parse_callback(ok, "abc"), Callback::Code("0.AXo+z".into()));
        assert_eq!(parse_callback(ok, "abd"), Callback::BadState);
        assert_eq!(
            parse_callback(
                "GET /?error=access_denied&state=zzz HTTP/1.1\r\n\r\n",
                "abc"
            ),
            Callback::BadState
        );
        let denied = "GET /?error=invalid_client&error_description=AADSTS65001%3A+The+user+or+administrator+has+not+consented.%0D%0ATrace&state=abc HTTP/1.1\r\n\r\n";
        let Callback::Failed(body) = parse_callback(denied, "abc") else {
            panic!("expected a failure");
        };
        let e = sign_in_error(&body).to_string();
        assert!(
            e.contains("todavía no tiene permiso para Power BI")
                && e.contains("Cliente de Azure CLI"),
            "{e}"
        );
        assert_eq!(
            parse_callback("GET /favicon.ico HTTP/1.1\r\n\r\n", "abc"),
            Callback::Ignore
        );
        assert_eq!(
            parse_callback("POST /?code=x&state=abc HTTP/1.1\r\n\r\n", "abc"),
            Callback::Ignore
        );
        assert_eq!(parse_callback("", "abc"), Callback::Ignore);
    }

    #[test]
    fn consent_and_access_errors_name_the_way_out() {
        for code in ["AADSTS650057", "AADSTS65001", "AADSTS90094"] {
            let body = json!({"error": "invalid_grant", "error_description": format!("{code}: no. Trace ID: x")});
            let e = sign_in_error(&body);
            assert!(
                matches!(&e, Error::AuthFailed(m) if m.contains("Cliente de Azure CLI") && m.contains(code)),
                "{e}"
            );
            // With the Azure CLI client it doesn't send you back to it.
            let e = sign_in_error_for(&body, &ClientApp::AzureCli).to_string();
            assert!(!e.contains("elegí «Cliente de Azure CLI»"), "{e}");
        }
        let e = sign_in_error(
            &json!({"error_description": "AADSTS53003: Access has been blocked by Conditional Access policies."}),
        );
        assert!(e.to_string().contains("acceso condicional"), "{e}");
        let e = sign_in_error(
            &json!({"error": "invalid_grant", "error_description": "AADSTS50126: bad password\r\nTrace ID: x"}),
        );
        assert_eq!(
            e.to_string(),
            "Microsoft Entra ID rechazó el inicio de sesión: AADSTS50126: bad password"
        );
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
        assert!(answers[2].starts_with("HTTP/1.1 200"));
    }

    #[tokio::test]
    async fn code_and_refresh_against_a_fake_token_endpoint() {
        use crate::http::testing::{serve, Reply};
        use std::sync::{Arc, Mutex};
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let log = seen.clone();
        let base = serve(Arc::new(move |req| {
            log.lock().unwrap().push(format!("{} {} {}", req.method, req.path, req.body));
            if req.body.contains("refresh_token=bad") {
                return Reply::json(400, r#"{"error":"invalid_grant","error_description":"AADSTS700082: The refresh token has expired."}"#);
            }
            Reply::json(200, r#"{"access_token":"at-1","expires_in":3599,"refresh_token":"rt-1","token_type":"Bearer"}"#)
        }))
        .await;
        let ep = Endpoints::test(&base);
        let s = settings(&[], true);
        let t = exchange_code(
            &s,
            &ep,
            Audience::PowerBi,
            "the-code",
            "http://localhost:9",
            &Secret::new("ver"),
        )
        .await
        .unwrap();
        assert_eq!(t.access.token.expose(), "at-1");
        assert_eq!(t.refresh.as_ref().map(Secret::expose), Some("rt-1"));
        let r = refresh_at(&s, &ep, Audience::Fabric, &Secret::new("rt-1"))
            .await
            .unwrap();
        assert_eq!(r.access.token.expose(), "at-1");
        let e = refresh_at(&s, &ep, Audience::PowerBi, &Secret::new("bad"))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("AADSTS700082"), "{e}");
        let seen = seen.lock().unwrap();
        assert!(seen[0].starts_with(
            "POST /login/organizations/oauth2/v2.0/token grant_type=authorization_code"
        ));
        assert!(seen[0].contains("code_verifier=ver") && seen[0].contains("code=the-code"));
        assert!(
            seen[1].contains("grant_type=refresh_token")
                && seen[1].contains("SemanticModel.ReadWrite.All")
        );
    }
}
