//! CyberArk:
//!
//! - **Conjur** (Open Source, Enterprise, Conjur Cloud): the API key of a
//!   host or user (kept in the keychain) is traded for a short-lived access
//!   token (`POST /authn/{account}/{login}/authenticate`), which reads the
//!   variable (`GET /secrets/{account}/variable/{id}`) and, optionally,
//!   another one with the user. The token lives in memory for this read only.
//! - **Central Credential Provider** (CyberArk PAM):
//!   `GET /AIMWebService/api/Accounts?AppID=…&Safe=…&Object=…`, usually with
//!   a client certificate. It answers the account's `Content` (the password)
//!   and `UserName`.
//!
//! Both only talk to the URL the user gave, over HTTPS (HTTP only to this
//! computer), without following redirects. Errors carry CyberArk's codes and
//! messages, never a key, a token or a value.

use super::cloud_auth::uri_encode;
use super::https;
use base64::Engine;
use dbine_core::secret_sources::wipe;
use serde_json::Value;

// ---- Conjur --------------------------------------------------------------------

pub struct Conjur<'a> {
    pub url: &'a str,
    pub account: &'a str,
    pub login: &'a str,
    pub variable: &'a str,
    pub username_variable: &'a str,
    pub ca_file: &'a str,
}

/// `(value, username)`.
pub async fn conjur_read(req: &Conjur<'_>, api_key: Option<&str>) -> Result<(String, Option<String>), String> {
    let client = https::client(req.url, "Conjur", req.ca_file, None)?;
    conjur_read_with(&client, req, api_key).await
}

pub async fn conjur_read_with(client: &reqwest::Client, req: &Conjur<'_>, api_key: Option<&str>) -> Result<(String, Option<String>), String> {
    let key = api_key.map(str::trim).filter(|k| !k.is_empty()).ok_or("falta la clave de API de Conjur: escribila en el formulario")?;
    let mut token = conjur_token(client, req, key).await?;
    let value = conjur_variable(client, req, &token, req.variable).await;
    let username = match (&value, req.username_variable.trim()) {
        (Ok(_), u) if !u.is_empty() => Some(conjur_variable(client, req, &token, u).await),
        _ => None,
    };
    wipe(&mut token);
    let value = value?;
    let username = username.transpose()?;
    Ok((value, username))
}

/// The access token, as the `Authorization` header wants it (base64).
async fn conjur_token(client: &reqwest::Client, req: &Conjur<'_>, api_key: &str) -> Result<String, String> {
    let url = format!("{}/authn/{}/{}/authenticate", https::base(req.url), uri_encode(req.account.trim()), uri_encode(req.login.trim()));
    let resp = client
        .post(url)
        .header("Accept-Encoding", "base64")
        .header("Content-Type", "text/plain")
        .body(api_key.to_string())
        .send()
        .await
        .map_err(|e| format!("no se pudo conectar con Conjur en {}: {}", https::base(req.url), e.without_url()))?;
    let status = resp.status().as_u16();
    let mut body = resp.text().await.unwrap_or_default();
    match status {
        200 => {
            let t = body.trim();
            // Asked in base64; a server that ignores it answers the JSON.
            let token = if t.starts_with('{') { base64::engine::general_purpose::STANDARD.encode(t.as_bytes()) } else { t.to_string() };
            wipe(&mut body);
            if token.is_empty() {
                return Err("Conjur no devolvió un token de acceso".into());
            }
            Ok(token)
        }
        401 => Err(format!("Conjur rechazó el login «{}» o su clave de API", req.login.trim())),
        404 => Err(format!("Conjur no tiene la cuenta «{}» (o la URL no es la de la API)", req.account.trim())),
        _ => Err(conjur_error(status, &body)),
    }
}

async fn conjur_variable(client: &reqwest::Client, req: &Conjur<'_>, token: &str, id: &str) -> Result<String, String> {
    let id = id.trim();
    let url = format!("{}/secrets/{}/variable/{}", https::base(req.url), uri_encode(req.account.trim()), uri_encode(id));
    let resp = client
        .get(url)
        .header("Authorization", format!("Token token=\"{token}\""))
        .send()
        .await
        .map_err(|e| format!("no se pudo conectar con Conjur en {}: {}", https::base(req.url), e.without_url()))?;
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.unwrap_or_default();
    match status {
        200 => String::from_utf8(bytes.to_vec()).map_err(|e| {
            let mut b = e.into_bytes();
            dbine_core::secret_sources::wipe_bytes(&mut b);
            format!("la variable «{id}» de Conjur no es texto")
        }),
        401 => Err("Conjur rechazó el token de acceso".into()),
        403 => Err(format!("Conjur: permiso denegado para leer «{id}» (falta el privilegio execute)")),
        404 => Err(format!("Conjur: no existe la variable «{id}» o todavía no tiene valor")),
        _ => Err(conjur_error(status, &String::from_utf8_lossy(&bytes))),
    }
}

/// The account wizard's check: the API key's access token, then `GET
/// /whoami`. `login (account)`.
pub async fn conjur_whoami(req: &Conjur<'_>, api_key: &str) -> Result<String, String> {
    let client = https::client(req.url, "Conjur", req.ca_file, None)?;
    conjur_whoami_with(&client, req, api_key).await
}

pub async fn conjur_whoami_with(client: &reqwest::Client, req: &Conjur<'_>, api_key: &str) -> Result<String, String> {
    let key = Some(api_key.trim()).filter(|k| !k.is_empty()).ok_or("falta la clave de API de Conjur")?;
    let mut token = conjur_token(client, req, key).await?;
    let resp = client.get(format!("{}/whoami", https::base(req.url))).header("Authorization", format!("Token token=\"{token}\"")).send().await;
    wipe(&mut token);
    let resp = resp.map_err(|e| format!("no se pudo conectar con Conjur en {}: {}", https::base(req.url), e.without_url()))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    match status {
        200 => {
            let json: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let s = |k: &str| json.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
            let user = Some(s("username")).filter(|u| !u.is_empty()).unwrap_or_else(|| req.login.trim().to_string());
            let account = Some(s("account")).filter(|a| !a.is_empty()).unwrap_or_else(|| req.account.trim().to_string());
            Ok(format!("{user} ({account})"))
        }
        401 => Err("Conjur rechazó el token de acceso".into()),
        _ => Err(conjur_error(status, &body)),
    }
}

pub fn conjur_error(status: u16, body: &str) -> String {
    let json: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let msg = json.pointer("/error/message").and_then(Value::as_str).unwrap_or("");
    let msg = super::exec::first_lines(msg, 300);
    if msg.is_empty() { format!("Conjur respondió {status}") } else { format!("Conjur respondió {status}: {msg}") }
}

/// The variables the login can see (`GET /resources/{account}?kind=variable`,
/// paged), or a variable's JSON keys (`item`; read, keys taken, wiped).
pub async fn conjur_browse(req: &Conjur<'_>, api_key: Option<&str>, at: super::browse::At<'_>) -> Result<super::browse::Listing, String> {
    let client = https::client(req.url, "Conjur", req.ca_file, None)?;
    conjur_browse_with(&client, req, api_key, at).await
}

pub async fn conjur_browse_with(client: &reqwest::Client, req: &Conjur<'_>, api_key: Option<&str>, at: super::browse::At<'_>) -> Result<super::browse::Listing, String> {
    use super::browse::{Entry, Listing, MAX_ENTRIES};
    let key = api_key.map(str::trim).filter(|k| !k.is_empty()).ok_or("falta la clave de API de Conjur: escribila en el formulario")?;
    let mut token = conjur_token(client, req, key).await?;
    let out = async {
        if let Some(item) = at.item {
            let raw = conjur_variable(client, req, &token, item).await?;
            return Ok(Listing::fields(super::browse::json_keys(raw)));
        }
        const PAGE: usize = 1000;
        let prefix = format!("{}:variable:", req.account.trim());
        let mut out = Vec::new();
        for page in 0.. {
            let url = format!("{}/resources/{}?kind=variable&limit={PAGE}&offset={}", https::base(req.url), uri_encode(req.account.trim()), page * PAGE);
            let resp = client
                .get(url)
                .header("Authorization", format!("Token token=\"{token}\""))
                .send()
                .await
                .map_err(|e| format!("no se pudo conectar con Conjur en {}: {}", https::base(req.url), e.without_url()))?;
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            match status {
                200 => {}
                401 => return Err("Conjur rechazó el token de acceso".into()),
                403 => return Err("Conjur: permiso denegado para listar las variables".into()),
                _ => return Err(conjur_error(status, &body)),
            }
            let list: Vec<Value> = serde_json::from_str(&body).ok().and_then(|v: Value| v.as_array().cloned()).unwrap_or_default();
            let n = list.len();
            out.extend(list.iter().filter_map(|r| r.get("id").and_then(Value::as_str)).filter_map(|id| id.strip_prefix(&prefix)).map(|v| Entry::named(v, "")));
            if n < PAGE || out.len() > MAX_ENTRIES {
                break;
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Listing::complete(out))
    }
    .await;
    wipe(&mut token);
    out
}

// ---- Central Credential Provider ---------------------------------------------------

pub struct Ccp<'a> {
    pub url: &'a str,
    pub app_id: &'a str,
    pub safe: &'a str,
    pub folder: &'a str,
    pub object: &'a str,
    pub account_user: &'a str,
    pub account_address: &'a str,
    pub client_cert: &'a str,
    pub client_key: &'a str,
    pub ca_file: &'a str,
    pub username: bool,
}

/// The Accounts URL: `<url>/AIMWebService/api/Accounts`, or
/// `<url>/api/Accounts` when the URL already names the service's folder.
pub fn ccp_endpoint(url: &str) -> String {
    let base = https::base(url);
    let after_scheme = base.split_once("://").map(|(_, r)| r).unwrap_or(base);
    if base.to_ascii_lowercase().ends_with("/api/accounts") {
        base.to_string()
    } else if after_scheme.contains('/') {
        format!("{base}/api/Accounts")
    } else {
        format!("{base}/AIMWebService/api/Accounts")
    }
}

/// `(password, username)`. `passphrase`: the client key's or PKCS#12's.
pub async fn ccp_read(req: &Ccp<'_>, passphrase: Option<&str>) -> Result<(String, Option<String>), String> {
    let identity = match req.client_cert.trim() {
        "" => None,
        cert => Some(https::identity(cert, req.client_key, passphrase)?),
    };
    let client = https::client(req.url, "Central Credential Provider", req.ca_file, identity)?;
    ccp_read_with(&client, req).await
}

pub async fn ccp_read_with(client: &reqwest::Client, req: &Ccp<'_>) -> Result<(String, Option<String>), String> {
    let mut q: Vec<(&str, &str)> = vec![("AppID", req.app_id.trim())];
    for (k, v) in [("Safe", req.safe), ("Folder", req.folder), ("Object", req.object), ("UserName", req.account_user), ("Address", req.account_address)] {
        if !v.trim().is_empty() {
            q.push((k, v.trim()));
        }
    }
    let resp = client
        .get(ccp_endpoint(req.url))
        .query(&q)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| format!("no se pudo conectar con el Central Credential Provider en {}: {}", https::base(req.url), e.without_url()))?;
    let status = resp.status().as_u16();
    let mut body = resp.text().await.unwrap_or_default();
    let json: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    wipe(&mut body);
    if status != 200 {
        return Err(ccp_error(status, &json));
    }
    let content = json.get("Content").and_then(Value::as_str).unwrap_or("").to_string();
    if content.is_empty() {
        return Err("el Central Credential Provider no devolvió la contraseña (Content)".into());
    }
    let username = if req.username {
        match json.get("UserName").and_then(Value::as_str).filter(|u| !u.is_empty()) {
            Some(u) => Some(u.to_string()),
            None => return Err("la cuenta del Central Credential Provider no tiene usuario (UserName)".into()),
        }
    } else {
        None
    };
    Ok((content, username))
}

/// The account wizard's check. The Central Credential Provider has no
/// call that reads nothing (every Accounts query hands out a password), so
/// this checks the connection only: the TLS handshake with the client
/// certificate (when there is one) and the server's trust, with a request
/// to the URL's root that carries no AppID and no query. Any HTTP answer
/// means the TLS connection, client certificate included, was accepted.
pub async fn ccp_handshake(url: &str, client_cert: &str, client_key: &str, ca_file: &str, passphrase: Option<&str>) -> Result<u16, String> {
    let identity = match client_cert.trim() {
        "" => None,
        cert => Some(https::identity(cert, client_key, passphrase)?),
    };
    let client = https::client(url, "Central Credential Provider", ca_file, identity)?;
    ccp_handshake_with(&client, url).await
}

pub async fn ccp_handshake_with(client: &reqwest::Client, url: &str) -> Result<u16, String> {
    let base = https::base(url);
    let root = match base.split_once("://") {
        Some((scheme, rest)) => format!("{scheme}://{}/", rest.split('/').next().unwrap_or(rest)),
        None => format!("{base}/"),
    };
    let resp = client.get(root).send().await.map_err(|e| format!("no se pudo conectar con el Central Credential Provider en {base}: {}", e.without_url()))?;
    Ok(resp.status().as_u16())
}

pub fn ccp_error(status: u16, json: &Value) -> String {
    let code = json.get("ErrorCode").and_then(Value::as_str).unwrap_or("");
    let msg = super::exec::first_lines(json.get("ErrorMsg").and_then(Value::as_str).unwrap_or(""), 300);
    match (status, code) {
        (_, "APPAP004E") => format!("CyberArk: no hay una cuenta que coincida con el pedido ({code}: {msg})"),
        (_, c) if !c.is_empty() => format!("el Central Credential Provider respondió {c}: {msg}"),
        (401 | 403, _) => format!("el Central Credential Provider rechazó el pedido ({status}): revisá el AppID y el certificado de cliente"),
        _ => format!("el Central Credential Provider respondió {status}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Bytes;
    use axum::extract::Path as AxPath;
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::Router;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn ccp_endpoints() {
        assert_eq!(ccp_endpoint("https://ccp.example.com/"), "https://ccp.example.com/AIMWebService/api/Accounts");
        assert_eq!(ccp_endpoint("https://ccp.example.com/AIMWebServiceProd"), "https://ccp.example.com/AIMWebServiceProd/api/Accounts");
        assert_eq!(ccp_endpoint("https://ccp/AIMWebService/api/Accounts"), "https://ccp/AIMWebService/api/Accounts");
    }

    #[test]
    fn ccp_errors() {
        let e = ccp_error(404, &json!({"ErrorCode": "APPAP004E", "ErrorMsg": "Password object matching query [Safe=DB;Object=x] was not found"}));
        assert!(e.contains("no hay una cuenta") && e.contains("APPAP004E"), "{e}");
        assert!(ccp_error(403, &Value::Null).contains("certificado de cliente"));
        assert_eq!(conjur_error(500, r#"{"error":{"code":"internal","message":"boom"}}"#), "Conjur respondió 500: boom");
    }

    // ---- a mock of Conjur's API ----------------------------------------------------

    const TOKEN_JSON: &str = r#"{"protected":"eyJhbGciOiJjb25qdXIub3JnL3Nsb3NpbG8vdjIifQ==","payload":"eyJzdWIiOiJob3N0L2FwcHMvZGJpbmUifQ==","signature":"c2ln"}"#;

    async fn conjur_mock() -> String {
        let app = Router::new()
            .route(
                "/authn/{account}/{login}/authenticate",
                post(|AxPath((account, login)): AxPath<(String, String)>, h: HeaderMap, body: Bytes| async move {
                    if account != "myorg" || login != "host/apps/dbine" || &body[..] != b"dummy-api-key" {
                        return (StatusCode::UNAUTHORIZED, String::new());
                    }
                    let b64 = h.get("accept-encoding").and_then(|v| v.to_str().ok()) == Some("base64");
                    (StatusCode::OK, if b64 { base64::engine::general_purpose::STANDARD.encode(TOKEN_JSON) } else { TOKEN_JSON.to_string() })
                }),
            )
            .route(
                "/whoami",
                get(|h: HeaderMap| async move {
                    let want = format!("Token token=\"{}\"", base64::engine::general_purpose::STANDARD.encode(TOKEN_JSON));
                    if h.get("authorization").and_then(|v| v.to_str().ok()) != Some(want.as_str()) {
                        return (StatusCode::UNAUTHORIZED, String::new());
                    }
                    (StatusCode::OK, json!({ "client_ip": "127.0.0.1", "user_agent": "x", "account": "myorg", "username": "host/apps/dbine", "token_issued_at": "2026-10-18T10:00:00.000+00:00" }).to_string())
                }),
            )
            .route(
                "/resources/{account}",
                get(|AxPath(account): AxPath<String>, h: HeaderMap| async move {
                    let want = format!("Token token=\"{}\"", base64::engine::general_purpose::STANDARD.encode(TOKEN_JSON));
                    if account != "myorg" || h.get("authorization").and_then(|v| v.to_str().ok()) != Some(want.as_str()) {
                        return (StatusCode::UNAUTHORIZED, String::new());
                    }
                    (StatusCode::OK, json!([{ "id": "myorg:variable:db/prod/password" }, { "id": "myorg:variable:db/prod/json" }, { "id": "myorg:variable:top" }]).to_string())
                }),
            )
            .route(
                "/secrets/{account}/variable/{id}",
                get(|AxPath((account, id)): AxPath<(String, String)>, h: HeaderMap| async move {
                    let want = format!("Token token=\"{}\"", base64::engine::general_purpose::STANDARD.encode(TOKEN_JSON));
                    if h.get("authorization").and_then(|v| v.to_str().ok()) != Some(want.as_str()) {
                        return (StatusCode::UNAUTHORIZED, String::new());
                    }
                    match (account.as_str(), id.as_str()) {
                        ("myorg", "db/prod/password") => (StatusCode::OK, "pg-secret-1".into()),
                        ("myorg", "db/prod/username") => (StatusCode::OK, "app_user".into()),
                        ("myorg", "db/prod/json") => (StatusCode::OK, r#"{"username":"j","password":"pj"}"#.into()),
                        ("myorg", "db/forbidden") => (StatusCode::FORBIDDEN, String::new()),
                        _ => (StatusCode::NOT_FOUND, r#"{"error":{"code":"not_found","message":"Variable 'x' is empty or not found."}}"#.into()),
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn conjur<'a>(url: &'a str, variable: &'a str, username_variable: &'a str) -> Conjur<'a> {
        Conjur { url, account: "myorg", login: "host/apps/dbine", variable, username_variable, ca_file: "" }
    }

    /// Against a mock of Conjur's API: the host's login with a slash goes
    /// encoded, the token goes base64 in `Authorization`, the user from a
    /// second variable, errors by status.
    #[tokio::test]
    async fn conjur_against_a_mock() {
        let url = conjur_mock().await;
        let key = Some("dummy-api-key");
        assert_eq!(conjur_read(&conjur(&url, "db/prod/password", ""), key).await.unwrap(), ("pg-secret-1".into(), None));
        assert_eq!(conjur_read(&conjur(&url, "db/prod/password", "db/prod/username"), key).await.unwrap(), ("pg-secret-1".into(), Some("app_user".into())));
        let e = conjur_read(&conjur(&url, "db/prod/password", ""), Some("wrong")).await.unwrap_err();
        assert!(e.contains("rechazó el login «host/apps/dbine»") && !e.contains("wrong"), "{e}");
        assert!(conjur_read(&conjur(&url, "db/forbidden", ""), key).await.unwrap_err().contains("permiso denegado"));
        assert!(conjur_read(&conjur(&url, "db/none", ""), key).await.unwrap_err().contains("no existe la variable «db/none»"));
        assert!(conjur_read(&conjur(&url, "db/prod/password", ""), None).await.unwrap_err().contains("clave de API"));
        // Browsing: the variables, then one's JSON keys (never values).
        let l = conjur_browse(&conjur(&url, "", ""), key, Default::default()).await.unwrap();
        let shown: Vec<(&str, &str, &str)> = l.entries.iter().map(|e| (e.id.as_str(), e.name.as_str(), e.path.as_str())).collect();
        assert_eq!(shown, [("db/prod/json", "json", "db/prod"), ("db/prod/password", "password", "db/prod"), ("top", "top", "")]);
        let at = crate::secret_sources::browse::At { item: Some("db/prod/json"), ..Default::default() };
        let l = conjur_browse(&conjur(&url, "", ""), key, at).await.unwrap();
        assert_eq!(l.fields.as_deref().unwrap(), ["password", "username"]);
        let at = crate::secret_sources::browse::At { item: Some("db/prod/password"), ..Default::default() };
        assert!(conjur_browse(&conjur(&url, "", ""), key, at).await.unwrap().fields.unwrap().is_empty(), "not JSON: no keys");
    }

    // ---- a mock of the CCP over TLS with client certificates -----------------------------

    pub struct Pki {
        pub ca_pem: String,
        pub server_cert: rustls::pki_types::CertificateDer<'static>,
        pub server_key: rustls::pki_types::PrivateKeyDer<'static>,
        /// The client's certificate and PKCS#8 key (DER).
        pub client_cert_der: Vec<u8>,
        pub client_cert_pem: String,
        pub client_key_der: Vec<u8>,
        pub client_key_pem: String,
    }

    /// A test CA, a server certificate for 127.0.0.1 and a client one.
    pub fn pki() -> Pki {
        use rcgen::{BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair};
        let ca_key = KeyPair::generate().unwrap();
        let mut ca = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca.distinguished_name.push(DnType::CommonName, "DBine test CA");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_cert = ca.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(ca, ca_key);
        let server_key = KeyPair::generate().unwrap();
        let mut sp = CertificateParams::new(vec!["127.0.0.1".to_string(), "localhost".to_string()]).unwrap();
        sp.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let server = sp.signed_by(&server_key, &issuer).unwrap();
        let client_key = KeyPair::generate().unwrap();
        let mut cp = CertificateParams::new(Vec::<String>::new()).unwrap();
        cp.distinguished_name.push(DnType::CommonName, "dbine-app");
        cp.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        let client = cp.signed_by(&client_key, &issuer).unwrap();
        Pki {
            ca_pem: ca_cert.pem(),
            server_cert: server.der().clone(),
            server_key: rustls::pki_types::PrivateKeyDer::Pkcs8(server_key.serialize_der().into()),
            client_cert_der: client.der().to_vec(),
            client_cert_pem: client.pem(),
            client_key_der: client_key.serialize_der(),
            client_key_pem: client_key.serialize_pem(),
        }
    }

    /// A one-thread HTTPS server that requires a client certificate signed
    /// by the test CA and answers the Accounts API.
    fn ccp_tls_mock(p: &Pki) -> String {
        use std::io::{Read, Write};
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut roots = rustls::RootCertStore::empty();
        for c in rustls_pki_types_from_pem(&p.ca_pem) {
            roots.add(c).unwrap();
        }
        let verifier = rustls::server::WebPkiClientVerifier::builder(roots.into()).build().unwrap();
        let config = std::sync::Arc::new(
            rustls::ServerConfig::builder().with_client_cert_verifier(verifier).with_single_cert(vec![p.server_cert.clone()], p.server_key.clone_key()).unwrap(),
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut tcp) = stream else { continue };
                let config = config.clone();
                std::thread::spawn(move || {
                    let mut conn = rustls::ServerConnection::new(config).unwrap();
                    let mut tls = rustls::Stream::new(&mut conn, &mut tcp);
                    let mut buf = [0u8; 8192];
                    let Ok(n) = tls.read(&mut buf) else { return };
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let line = req.lines().next().unwrap_or("").to_string();
                    let path = line.split(' ').nth(1).unwrap_or("");
                    let q: HashMap<String, String> = path.split_once('?').map(|(_, q)| url_pairs(q)).unwrap_or_default();
                    let (status, body) = if !path.starts_with("/AIMWebService/api/Accounts?") {
                        ("404 Not Found", String::new())
                    } else if q.get("AppID").map(String::as_str) != Some("DBine") {
                        ("403 Forbidden", json!({"ErrorCode": "APPAP306E", "ErrorMsg": "Authentication failed for App ID"}).to_string())
                    } else if q.get("Safe").map(String::as_str) == Some("DB") && q.get("Object").map(String::as_str) == Some("pg prod") {
                        ("200 OK", json!({"Content": "pg-secret-1", "UserName": "app_user", "Address": "db.example.com", "Safe": "DB", "Name": "pg prod"}).to_string())
                    } else {
                        ("404 Not Found", json!({"ErrorCode": "APPAP004E", "ErrorMsg": "Password object matching query was not found (Diagnostic Info: 5)"}).to_string())
                    };
                    let resp = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = tls.write_all(resp.as_bytes());
                    let _ = tls.flush();
                    tls.conn.send_close_notify();
                    let _ = tls.flush();
                });
            }
        });
        format!("https://127.0.0.1:{}", addr.port())
    }

    fn url_pairs(q: &str) -> HashMap<String, String> {
        q.split('&')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), percent_decode(v)))
            .collect()
    }

    fn percent_decode(s: &str) -> String {
        let b = s.replace('+', " ");
        let bytes = b.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                if let Ok(v) = u8::from_str_radix(&b[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
            }
            out.push(bytes[i]);
            i += 1;
        }
        String::from_utf8(out).unwrap()
    }

    fn rustls_pki_types_from_pem(pem: &str) -> Vec<rustls::pki_types::CertificateDer<'static>> {
        use rustls::pki_types::pem::PemObject;
        rustls::pki_types::CertificateDer::pem_slice_iter(pem.as_bytes()).map(|c| c.unwrap()).collect()
    }

    fn ccp<'a>(url: &'a str, cert: &'a str, key: &'a str, ca: &'a str, app: &'a str) -> Ccp<'a> {
        Ccp { url, app_id: app, safe: "DB", folder: "", object: "pg prod", account_user: "", account_address: "", client_cert: cert, client_key: key, ca_file: ca, username: true }
    }

    /// The CCP over TLS with client-certificate authentication, against a
    /// mock: PEM (certificate and key apart, together, and the key
    /// encrypted), PKCS#12 with a passphrase, no certificate (refused by
    /// the server), the server's CA, and errors.
    #[tokio::test]
    async fn ccp_with_client_certificates_against_a_mock() {
        let p = pki();
        let url = ccp_tls_mock(&p);
        let dir = super::super::tests::temp_dir("ccp");
        let w = |name: &str, data: &[u8]| {
            let f = dir.join(name);
            std::fs::write(&f, data).unwrap();
            f.display().to_string()
        };
        let ca = w("ca.pem", p.ca_pem.as_bytes());
        let cert = w("client.crt", p.client_cert_pem.as_bytes());
        let key = w("client.key", p.client_key_pem.as_bytes());
        let both = w("client.pem", format!("{}{}", p.client_cert_pem, p.client_key_pem).as_bytes());
        // PKCS#12, with a passphrase.
        let mut store = p12_keystore::KeyStore::new();
        let chain = p12_keystore::PrivateKeyChain::new(
            vec![1u8, 2, 3],
            p12_keystore::PrivateKey::from_der(&p.client_key_der).unwrap(),
            [p12_keystore::Certificate::from_der(&p.client_cert_der).unwrap()],
        );
        store.add_entry("dbine", p12_keystore::KeyStoreEntry::PrivateKeyChain(chain));
        let p12 = w("client.p12", &store.writer("dummy-p12-pass").write().unwrap());
        // An encrypted PKCS#8 key.
        let info = pkcs8::PrivateKeyInfoRef::try_from(p.client_key_der.as_slice()).unwrap();
        let params = pkcs8::pkcs5::pbes2::Parameters::generate_pbkdf2_sha256_aes256cbc(2048, &[7u8; 16], [9u8; 16]).unwrap();
        let enc = info.encrypt_with_params(params, "dummy-key-pass").unwrap();
        let enc_key = w("client-enc.key", super::super::https::tests_pem("ENCRYPTED PRIVATE KEY", enc.as_bytes()).as_bytes());

        let want = ("pg-secret-1".to_string(), Some("app_user".to_string()));
        assert_eq!(ccp_read(&ccp(&url, &cert, &key, &ca, "DBine"), None).await.unwrap(), want, "PEM, apart");
        assert_eq!(ccp_read(&ccp(&url, &both, "", &ca, "DBine"), None).await.unwrap(), want, "PEM, together");
        assert_eq!(ccp_read(&ccp(&url, &p12, "", &ca, "DBine"), Some("dummy-p12-pass")).await.unwrap(), want, "PKCS#12");
        assert_eq!(ccp_read(&ccp(&url, &cert, &enc_key, &ca, "DBine"), Some("dummy-key-pass")).await.unwrap(), want, "encrypted key");
        let e = ccp_read(&ccp(&url, &p12, "", &ca, "DBine"), Some("nope")).await.unwrap_err();
        assert!(e.contains("PKCS#12") && !e.contains("nope"), "{e}");
        assert!(ccp_read(&ccp(&url, &cert, &enc_key, &ca, "DBine"), Some("nope")).await.unwrap_err().contains("no es la correcta"));
        assert!(ccp_read(&ccp(&url, &cert, &enc_key, &ca, "DBine"), None).await.unwrap_err().contains("está cifrada"));
        // Without a certificate the server refuses the handshake.
        let e = ccp_read(&ccp(&url, "", "", &ca, "DBine"), None).await.unwrap_err();
        assert!(e.starts_with("no se pudo conectar con el Central Credential Provider"), "{e}");
        // Without the CA, the server isn't trusted.
        assert!(ccp_read(&ccp(&url, &cert, &key, "", "DBine"), None).await.is_err());
        // CyberArk's errors.
        let e = ccp_read(&ccp(&url, &cert, &key, &ca, "Other"), None).await.unwrap_err();
        assert!(e.contains("APPAP306E"), "{e}");
        let mut other = ccp(&url, &cert, &key, &ca, "DBine");
        other.object = "missing";
        assert!(ccp_read(&other, None).await.unwrap_err().contains("no hay una cuenta"));
        // The account wizard's check: the TLS handshake with the client
        // certificate, no AppID sent; any HTTP answer is a yes.
        assert_eq!(ccp_handshake(&url, &cert, &key, &ca, None).await.unwrap(), 404);
        assert_eq!(ccp_handshake(&url, &p12, "", &ca, Some("dummy-p12-pass")).await.unwrap(), 404);
        assert!(ccp_handshake(&url, &p12, "", &ca, Some("nope")).await.unwrap_err().contains("PKCS#12"));
        assert!(ccp_handshake(&url, "", "", &ca, None).await.unwrap_err().starts_with("no se pudo conectar con el Central Credential Provider"), "the server wants a certificate");
        assert!(ccp_handshake(&url, &cert, &key, "", None).await.is_err(), "a server not trusted");
        assert!(ccp_handshake(&url.replace("https://", "http://"), &cert, &key, &ca, None).await.is_err());
    }

    /// The account wizard's check for Conjur: authenticate, then whoami.
    #[tokio::test]
    async fn conjur_whoami_against_a_mock() {
        let url = conjur_mock().await;
        assert_eq!(conjur_whoami(&conjur(&url, "", ""), "dummy-api-key").await.unwrap(), "host/apps/dbine (myorg)");
        let e = conjur_whoami(&conjur(&url, "", ""), "wrong").await.unwrap_err();
        assert!(e.contains("rechazó el login") && !e.contains("wrong"), "{e}");
        assert!(conjur_whoami(&conjur(&url, "", ""), " ").await.unwrap_err().contains("clave de API"));
    }
}
