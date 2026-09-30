//! AWS connection settings shared by the DynamoDB, Aurora DSQL and Athena
//! drivers. The other crates include this file with `#[path]`, so it only
//! depends on what all three declare: `aws-config`, `aws-credential-types`
//! and `dbine-driver`.

// Each crate uses a different subset.
#![allow(dead_code)]

use aws_config::timeout::TimeoutConfig;
use aws_config::{BehaviorVersion, Region, SdkConfig};
use aws_credential_types::Credentials;
use dbine_driver::{ConnectionConfig, Error, Field, FieldKind, Result};
use std::time::Duration;

/// Region and credentials, in form order.
pub fn fields() -> Vec<Field> {
    vec![
        Field::new("region", "Región", FieldKind::Text)
            .placeholder("us-east-1")
            .help("Si se deja vacía, se toma la del perfil o de AWS_REGION."),
        Field::new(
            "auth_mode",
            "Autenticación",
            FieldKind::Select(vec![
                ("default", "Cadena predeterminada de AWS"),
                ("profile", "Perfil de ~/.aws"),
                ("keys", "Claves de acceso"),
            ]),
        )
        .default_value("default")
        .help("La cadena predeterminada usa variables de entorno, ~/.aws, SSO o el rol de la instancia."),
        Field::new("profile", "Perfil", FieldKind::Text).placeholder("default"),
        Field::new("access_key_id", "Access key ID", FieldKind::Text),
        Field::new("secret_access_key", "Secret access key", FieldKind::Password).secret(),
        Field::new("session_token", "Session token", FieldKind::Password)
            .secret()
            .help("Solo para credenciales temporales."),
    ]
}

/// `fields()` with each credential field shown only for the auth mode that
/// reads it (mirrors `auth`). A separate function so the other crates that
/// include this file keep their form until they opt in.
pub fn fields_by_auth() -> Vec<Field> {
    fields()
        .into_iter()
        .map(|f| match f.key {
            "profile" => f.when("auth_mode", &["profile"]),
            "access_key_id" | "secret_access_key" | "session_token" => f.when("auth_mode", &["keys"]),
            _ => f,
        })
        .collect()
}

/// Endpoint override, for local emulators.
pub fn endpoint_field(placeholder: &'static str) -> Field {
    Field::new("endpoint_url", "Endpoint personalizado", FieldKind::Text)
        .placeholder(placeholder)
        .help("Para emuladores locales. Vacío = el endpoint de AWS de la región.")
        .advanced()
}

/// What the loader gets from the form; separate so it can be tested
/// without touching the environment.
#[derive(Debug, PartialEq)]
pub enum Auth {
    DefaultChain,
    Profile(String),
    Keys { access_key_id: String, secret_access_key: String, session_token: Option<String> },
}

pub fn auth(cfg: &ConnectionConfig) -> Result<Auth> {
    match cfg.option("auth_mode").unwrap_or("default") {
        "profile" => Ok(Auth::Profile(cfg.option("profile").unwrap_or("default").to_string())),
        "keys" => {
            let (Some(ak), Some(sk)) = (cfg.option("access_key_id"), cfg.option("secret_access_key")) else {
                return Err(Error::AuthFailed("faltan el access key ID o el secret access key".into()));
            };
            Ok(Auth::Keys {
                access_key_id: ak.trim().to_string(),
                secret_access_key: sk.trim().to_string(),
                session_token: cfg.option("session_token").map(|t| t.trim().to_string()),
            })
        }
        _ => Ok(Auth::DefaultChain),
    }
}

/// The SDK configuration for a connection: region, credentials, endpoint
/// override and a connect timeout.
pub async fn sdk_config(cfg: &ConnectionConfig) -> Result<SdkConfig> {
    let mut loader = aws_config::defaults(BehaviorVersion::latest())
        .timeout_config(TimeoutConfig::builder().connect_timeout(Duration::from_secs(15)).build());
    if let Some(r) = cfg.option("region") {
        loader = loader.region(Region::new(r.trim().to_string()));
    }
    match auth(cfg)? {
        Auth::DefaultChain => {}
        Auth::Profile(p) => loader = loader.profile_name(p),
        Auth::Keys { access_key_id, secret_access_key, session_token } => {
            loader =
                loader.credentials_provider(Credentials::new(access_key_id, secret_access_key, session_token, None, "dbine"));
        }
    }
    if let Some(url) = cfg.option("endpoint_url") {
        loader = loader.endpoint_url(url.trim());
    }
    let conf = tokio::time::timeout(Duration::from_secs(20), loader.load())
        .await
        .map_err(|_| Error::Connect("tiempo de espera agotado al cargar la configuración de AWS".into()))?;
    if conf.region().is_none() {
        return Err(Error::Connect("falta la región de AWS".into()));
    }
    Ok(conf)
}

/// Maps an AWS error by its code: rejected credentials are `AuthFailed`,
/// unreachable endpoints `Connect`, everything else `Query`.
pub fn classify(code: Option<&str>, message: Option<&str>, unreachable: bool, fallback: String) -> Error {
    let text = match (code, message) {
        (Some(c), Some(m)) => format!("{c}: {m}"),
        (None, Some(m)) => m.to_string(),
        _ => fallback,
    };
    if unreachable {
        return Error::Connect(text);
    }
    match code {
        Some(
            "UnrecognizedClientException"
            | "InvalidSignatureException"
            | "ExpiredTokenException"
            | "ExpiredToken"
            | "MissingAuthenticationToken"
            | "MissingAuthenticationTokenException"
            | "InvalidClientTokenId"
            | "SignatureDoesNotMatch"
            | "IncompleteSignature"
            | "IncompleteSignatureException",
        ) => Error::AuthFailed(text),
        _ => Error::Query(text),
    }
}

#[cfg(test)]
mod aws_tests {
    use super::*;

    fn cfg(pairs: &[(&str, &str)]) -> ConnectionConfig {
        ConnectionConfig {
            options: pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn auth_modes() {
        assert_eq!(auth(&cfg(&[])).unwrap(), Auth::DefaultChain);
        assert_eq!(auth(&cfg(&[("auth_mode", "profile")])).unwrap(), Auth::Profile("default".into()));
        assert_eq!(
            auth(&cfg(&[("auth_mode", "keys"), ("access_key_id", "AK"), ("secret_access_key", "SK")])).unwrap(),
            Auth::Keys { access_key_id: "AK".into(), secret_access_key: "SK".into(), session_token: None }
        );
        assert!(matches!(auth(&cfg(&[("auth_mode", "keys")])), Err(Error::AuthFailed(_))));
    }

    #[test]
    fn error_codes() {
        assert!(matches!(
            classify(Some("UnrecognizedClientException"), Some("bad"), false, String::new()),
            Error::AuthFailed(m) if m == "UnrecognizedClientException: bad"
        ));
        assert!(matches!(classify(None, None, true, "dns".into()), Error::Connect(m) if m == "dns"));
        assert!(matches!(classify(Some("ValidationException"), Some("x"), false, String::new()), Error::Query(_)));
    }
}
