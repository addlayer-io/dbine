//! Google Cloud credentials shared by the BigQuery and Spanner drivers (the
//! Spanner crate includes this file with `#[path]`): a service-account key
//! pasted in the form, or Application Default Credentials (the
//! `GOOGLE_APPLICATION_CREDENTIALS` file, gcloud's user credentials or the
//! metadata server). Tokens are OAuth2 access tokens, cached until a minute
//! before they expire.

// Each crate uses a different subset.
#![allow(dead_code)]

use dbine_driver::{ConnectionConfig, Error, Field, FieldKind, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

const SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const METADATA_TOKEN: &str =
    "http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token";

/// Project, auth mode, service-account key.
pub fn fields() -> Vec<Field> {
    vec![
        Field::new("project_id", "ID del proyecto", FieldKind::Text).required().placeholder("mi-proyecto"),
        Field::new(
            "auth_mode",
            "Autenticación",
            FieldKind::Select(vec![
                ("service_account", "Clave de cuenta de servicio (JSON)"),
                ("adc", "Credenciales predeterminadas de la aplicación (ADC)"),
            ]),
        )
        .default_value("service_account")
        .help("ADC usa GOOGLE_APPLICATION_CREDENTIALS, `gcloud auth application-default login` o el servidor de metadatos."),
        Field::new("service_account_json", "Clave de la cuenta de servicio", FieldKind::Textarea)
            .secret()
            .placeholder("{\"type\": \"service_account\", …}")
            .when("auth_mode", &["service_account"]),
    ]
}

pub fn endpoint_field(placeholder: &'static str) -> Field {
    Field::new("endpoint_url", "Endpoint personalizado", FieldKind::Text)
        .placeholder(placeholder)
        .help("Para emuladores locales; sin autenticación. Vacío = la API de Google.")
        .advanced()
}

pub fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .user_agent("DBine")
        .build()
        .map_err(|e| Error::Connect(e.to_string()))
}

#[derive(Debug)]
enum Source {
    /// Emulator: no Authorization header.
    Anonymous,
    ServiceAccount { email: String, key: String, token_uri: String },
    AuthorizedUser { client_id: String, client_secret: String, refresh_token: String, token_uri: String },
    Metadata,
}

/// Hands out bearer tokens; cheap to clone (shared cache).
#[derive(Clone)]
pub struct Tokens {
    source: Arc<Source>,
    http: reqwest::Client,
    cache: Arc<Mutex<Option<(String, Instant)>>>,
}

impl Tokens {
    pub fn from_config(cfg: &ConnectionConfig, http: reqwest::Client) -> Result<Self> {
        let source = if cfg.option("endpoint_url").is_some() {
            Source::Anonymous
        } else if cfg.option("auth_mode") == Some("adc") {
            adc_source()?
        } else {
            let json = cfg
                .option("service_account_json")
                .ok_or_else(|| Error::AuthFailed("falta la clave JSON de la cuenta de servicio".into()))?;
            parse_key(json)?
        };
        Ok(Self { source: Arc::new(source), http, cache: Arc::new(Mutex::new(None)) })
    }

    /// `Authorization` header value, or `None` against an emulator.
    pub async fn bearer(&self) -> Result<Option<String>> {
        if matches!(*self.source, Source::Anonymous) {
            return Ok(None);
        }
        let mut cache = self.cache.lock().await;
        if let Some((t, until)) = cache.as_ref() {
            if Instant::now() < *until {
                return Ok(Some(format!("Bearer {t}")));
            }
        }
        let (token, ttl) = self.fetch().await?;
        let until = Instant::now() + Duration::from_secs(ttl.saturating_sub(60).max(30));
        *cache = Some((token.clone(), until));
        Ok(Some(format!("Bearer {token}")))
    }

    async fn fetch(&self) -> Result<(String, u64)> {
        let req = match &*self.source {
            Source::Anonymous => unreachable!("no token needed"),
            Source::ServiceAccount { email, key, token_uri } => {
                let assertion = sa_assertion(email, key, token_uri, now_secs())?;
                self.http.post(token_uri).form(&[
                    ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                    ("assertion", assertion.as_str()),
                ])
            }
            Source::AuthorizedUser { client_id, client_secret, refresh_token, token_uri } => {
                self.http.post(token_uri).form(&[
                    ("grant_type", "refresh_token"),
                    ("client_id", client_id.as_str()),
                    ("client_secret", client_secret.as_str()),
                    ("refresh_token", refresh_token.as_str()),
                ])
            }
            Source::Metadata => self.http.get(METADATA_TOKEN).header("Metadata-Flavor", "Google"),
        };
        let resp = req.send().await.map_err(|e| Error::Connect(format!("no se pudo pedir el token de Google: {e}")))?;
        let status = resp.status();
        let body: Json = resp.json().await.map_err(|e| Error::AuthFailed(e.to_string()))?;
        if !status.is_success() {
            let msg = body
                .get("error_description")
                .or_else(|| body.get("error"))
                .and_then(Json::as_str)
                .map_or_else(|| body.to_string(), str::to_string);
            return Err(Error::AuthFailed(format!("Google rechazó las credenciales: {msg}")));
        }
        let token = body
            .get("access_token")
            .and_then(Json::as_str)
            .ok_or_else(|| Error::AuthFailed("la respuesta de Google no trae access_token".into()))?;
        Ok((token.to_string(), body.get("expires_in").and_then(Json::as_u64).unwrap_or(3600)))
    }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Deserialize)]
struct KeyFile {
    #[serde(rename = "type")]
    kind: Option<String>,
    client_email: Option<String>,
    private_key: Option<String>,
    token_uri: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    refresh_token: Option<String>,
}

/// A service-account key or gcloud user credentials file.
fn parse_key(json: &str) -> Result<Source> {
    let k: KeyFile = serde_json::from_str(json.trim())
        .map_err(|e| Error::AuthFailed(format!("la clave de la cuenta de servicio no es un JSON válido: {e}")))?;
    let token_uri = k.token_uri.unwrap_or_else(|| TOKEN_URI.to_string());
    match k.kind.as_deref() {
        Some("authorized_user") => match (k.client_id, k.client_secret, k.refresh_token) {
            (Some(client_id), Some(client_secret), Some(refresh_token)) => {
                Ok(Source::AuthorizedUser { client_id, client_secret, refresh_token, token_uri })
            }
            _ => Err(Error::AuthFailed("credenciales de usuario incompletas".into())),
        },
        _ => match (k.client_email, k.private_key) {
            (Some(email), Some(key)) => {
                // Fail now on a broken key rather than at the first query.
                jsonwebtoken::EncodingKey::from_rsa_pem(key.as_bytes())
                    .map_err(|e| Error::AuthFailed(format!("clave privada inválida: {e}")))?;
                Ok(Source::ServiceAccount { email, key, token_uri })
            }
            _ => Err(Error::AuthFailed("a la clave le faltan client_email o private_key".into())),
        },
    }
}

fn adc_source() -> Result<Source> {
    if let Ok(path) = std::env::var("GOOGLE_APPLICATION_CREDENTIALS") {
        let json = std::fs::read_to_string(&path)
            .map_err(|e| Error::AuthFailed(format!("no se pudo leer GOOGLE_APPLICATION_CREDENTIALS ({path}): {e}")))?;
        return parse_key(&json);
    }
    let well_known = if cfg!(windows) {
        std::env::var("APPDATA").ok().map(|d| std::path::PathBuf::from(d).join("gcloud"))
    } else {
        std::env::var("HOME").ok().map(|h| std::path::PathBuf::from(h).join(".config").join("gcloud"))
    }
    .map(|d| d.join("application_default_credentials.json"));
    if let Some(json) = well_known.and_then(|p| std::fs::read_to_string(p).ok()) {
        return parse_key(&json);
    }
    Ok(Source::Metadata)
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    scope: &'a str,
    aud: &'a str,
    iat: u64,
    exp: u64,
}

/// The signed JWT a service account trades for an access token.
fn sa_assertion(email: &str, pem: &str, token_uri: &str, now: u64) -> Result<String> {
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes())
        .map_err(|e| Error::AuthFailed(format!("clave privada inválida: {e}")))?;
    let claims = Claims { iss: email, scope: SCOPE, aud: token_uri, iat: now, exp: now + 3600 };
    jsonwebtoken::encode(&jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256), &claims, &key)
        .map_err(|e| Error::AuthFailed(e.to_string()))
}

/// A Google API error body (`{"error": {"code", "message", "status"}}`) as
/// our error: 401/403 on login are auth failures, the rest the server
/// rejecting the request.
pub fn api_error(status: u16, body: &str) -> Error {
    let parsed: Option<Json> = serde_json::from_str(body).ok();
    let msg = parsed
        .as_ref()
        .and_then(|j| j.pointer("/error/message").and_then(Json::as_str).map(str::to_string))
        .unwrap_or_else(|| if body.is_empty() { format!("HTTP {status}") } else { body.to_string() });
    match status {
        401 => Error::AuthFailed(msg),
        _ => Error::Query(msg),
    }
}

#[cfg(test)]
pub(crate) mod gcp_tests {
    use super::*;

    /// A throwaway 2048-bit key, only for signing tests.
    pub const TEST_KEY: &str = include_str!("../tests/test_key.pem");

    #[test]
    fn service_account_key_is_parsed_and_signs() {
        let json = serde_json::json!({
            "type": "service_account", "client_email": "a@p.iam.gserviceaccount.com", "private_key": TEST_KEY
        })
        .to_string();
        let Source::ServiceAccount { email, key, token_uri } = parse_key(&json).unwrap() else { panic!() };
        assert_eq!((email.as_str(), token_uri.as_str()), ("a@p.iam.gserviceaccount.com", TOKEN_URI));
        let jwt = sa_assertion(&email, &key, &token_uri, 1_700_000_000).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        use base64::Engine;
        let claims: Json =
            serde_json::from_slice(&base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["exp"], 1_700_003_600);
        assert_eq!(claims["scope"], SCOPE);
    }

    #[test]
    fn bad_keys_fail_as_auth() {
        assert!(matches!(parse_key("nope"), Err(Error::AuthFailed(_))));
        assert!(matches!(
            parse_key(r#"{"type":"service_account","client_email":"a","private_key":"x"}"#),
            Err(Error::AuthFailed(_))
        ));
        assert!(matches!(
            parse_key(r#"{"type":"authorized_user","client_id":"a","client_secret":"b","refresh_token":"c"}"#),
            Ok(Source::AuthorizedUser { .. })
        ));
    }

    #[test]
    fn api_errors() {
        assert!(matches!(api_error(401, r#"{"error":{"message":"bad token"}}"#), Error::AuthFailed(m) if m == "bad token"));
        assert!(matches!(api_error(400, r#"{"error":{"message":"Syntax error"}}"#), Error::Query(m) if m == "Syntax error"));
    }
}
