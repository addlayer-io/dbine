//! The connection's options, read once at connect (spec §2). Owned by part
//! B; written in phase 1 because it binds the form's keys (`lib.rs`'s
//! fields) to what the rest reads.
//!
//! Secrets ([`Secret`]) never show in `Debug`, errors or logs.

use crate::auth::cloud::Cloud;
use dbine_driver::{ConnectionConfig, Error, Result};
use std::fmt;
use std::time::Duration;

/// The option keys [`Settings::from_config`] reads from `cfg.options` (the
/// form declares each one; a test checks it).
pub const OPTION_KEYS: &[&str] = &[
    "auth_mode",
    "tenant",
    "client_id",
    "client_secret",
    "client_certificate",
    "client_certificate_password",
    "access_token",
    "workspace",
    "cloud",
    "client_app",
    "app_client_id",
    "query_api",
    "query_timeout",
    "rls_user",
    "rls_roles",
    "custom_data",
    "definition_api",
];

/// How the session gets its token (spec §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// Interactive browser sign-in with PKCE (MFA works).
    EntraMfa,
    /// Client credentials of a service principal: its secret, or its
    /// certificate ([`Settings::client_certificate`], the form's
    /// `entra_sp_cert`).
    EntraSp,
    /// Environment, managed identity, Azure CLI, azd (in that order).
    EntraDefault,
    /// Azure managed identity (system or user-assigned).
    EntraMsi,
    /// A token pasted in the form; never refreshed.
    Token,
}

/// The public client the browser sign-in uses (spec §3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientApp {
    /// DBine Database Sign-in (`b46a5260-9219-40ba-9c1b-5e45a1a6afe7`).
    Dbine,
    /// The Azure CLI's first-party client (`.default` scopes).
    AzureCli,
    /// The user's own app registration.
    Custom(String),
}

/// Which query API runs DAX (spec §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryApi {
    Auto,
    /// `executeQueries`.
    Json,
    /// `executeDaxQueries`.
    Arrow,
}

/// A secret (client secret, pasted token): its text only through
/// [`Secret::expose`], never in `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Secret(s.into())
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(…)")
    }
}

/// A service principal's certificate: a PEM (certificate and private key)
/// or a PFX / PKCS#12 file, read at each sign-in
/// ([`crate::auth::certificate`]). The path is no secret; the password is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientCertificate {
    pub path: String,
    pub password: Option<Secret>,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub auth: AuthMode,
    pub cloud: Cloud,
    /// GUID or domain, checked as SQL Server's driver does; `None`:
    /// `organizations` for the browser sign-in.
    pub tenant: Option<String>,
    /// The browser sign-in's `login_hint`.
    pub username: Option<String>,
    /// Service principal or user-assigned managed identity.
    pub client_id: Option<String>,
    /// The service principal's secret (`entra_sp` only).
    pub client_secret: Option<Secret>,
    /// The service principal's certificate (`entra_sp_cert` only).
    pub client_certificate: Option<ClientCertificate>,
    pub access_token: Option<Secret>,
    pub client_app: ClientApp,
    /// Name or id; `None`: every workspace the account sees.
    pub workspace: Option<String>,
    /// The model to open when `connect` gets no database.
    pub database: Option<String>,
    pub read_only: bool,
    pub query_api: QueryApi,
    pub query_timeout: Duration,
    pub rls_user: Option<String>,
    pub rls_roles: Vec<String>,
    pub custom_data: Option<String>,
    /// Read the full definition through Fabric's API (decision D2).
    pub definition_api: bool,
}

fn bad(what: &str) -> Error {
    Error::Connect(what.to_string())
}

/// A tenant as Entra takes it in a URL path: letters, digits, `-`, `.`,
/// `_` (a GUID or a domain).
pub fn valid_tenant(tenant: &str) -> Result<()> {
    if tenant.is_empty()
        || !tenant
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
    {
        return Err(Error::AuthFailed(
            "el inquilino (tenant) no es válido".into(),
        ));
    }
    Ok(())
}

impl Settings {
    pub fn from_config(cfg: &ConnectionConfig) -> Result<Settings> {
        let opt = |k: &str| cfg.option(k).map(str::trim).filter(|v| !v.is_empty());
        let text = |k: &str| opt(k).map(str::to_string);
        let mode = opt("auth_mode").unwrap_or("entra_mfa");
        let auth = match mode {
            "entra_mfa" => AuthMode::EntraMfa,
            "entra_sp" | "entra_sp_cert" => AuthMode::EntraSp,
            "entra_default" => AuthMode::EntraDefault,
            "entra_msi" => AuthMode::EntraMsi,
            "token" => AuthMode::Token,
            _ => return Err(bad("modo de autenticación desconocido")),
        };
        let cloud = Cloud::from_option(opt("cloud").unwrap_or("public"))?;
        let tenant = text("tenant");
        if let Some(t) = &tenant {
            valid_tenant(t)?;
        }
        let client_app = match opt("client_app").unwrap_or("dbine") {
            "dbine" => ClientApp::Dbine,
            "azure_cli" => ClientApp::AzureCli,
            "custom" => ClientApp::Custom(text("app_client_id").ok_or_else(|| {
                bad("Falta el Client ID de la aplicación propia (Opciones avanzadas).")
            })?),
            _ => return Err(bad("aplicación de inicio de sesión desconocida")),
        };
        let client_id = text("client_id");
        // Each service principal mode reads only its own credential: a
        // value left in the other field from before the switch is ignored.
        let with_certificate = mode == "entra_sp_cert";
        let client_secret = opt("client_secret")
            .filter(|_| !with_certificate)
            .map(Secret::new);
        let client_certificate =
            text("client_certificate")
                .filter(|_| with_certificate)
                .map(|path| ClientCertificate {
                    path,
                    // A password may begin or end with spaces: taken as typed.
                    password: cfg
                        .option("client_certificate_password")
                        .filter(|p| !p.is_empty())
                        .map(Secret::new),
                });
        let access_token = opt("access_token").map(Secret::new);
        match auth {
            AuthMode::EntraSp => {
                if tenant.is_none() {
                    return Err(bad(
                        "Falta el inquilino (tenant): la entidad de servicio lo necesita.",
                    ));
                }
                if with_certificate && (client_id.is_none() || client_certificate.is_none()) {
                    return Err(Error::AuthFailed(
                        "Faltan el Client ID o el certificado de la entidad de servicio.".into(),
                    ));
                }
                if !with_certificate && (client_id.is_none() || client_secret.is_none()) {
                    return Err(Error::AuthFailed(
                        "Faltan el Client ID o el secreto de la entidad de servicio.".into(),
                    ));
                }
            }
            AuthMode::Token if access_token.is_none() => {
                return Err(Error::AuthFailed("Falta el token de acceso.".into()));
            }
            AuthMode::EntraMfa
                if !cloud.dbine_app() && !matches!(client_app, ClientApp::Custom(_)) =>
            {
                return Err(bad(
                    "En esta nube hace falta una aplicación propia registrada en ella (Opciones avanzadas: Aplicación para iniciar sesión).",
                ));
            }
            _ => {}
        }
        let query_api = match opt("query_api").unwrap_or("auto") {
            "auto" => QueryApi::Auto,
            "json" => QueryApi::Json,
            "arrow" => QueryApi::Arrow,
            _ => return Err(bad("API de consultas desconocida")),
        };
        let query_timeout = match opt("query_timeout") {
            None => Duration::from_secs(600),
            Some(s) => match s.parse::<u64>() {
                Ok(n) if (1..=3600).contains(&n) => Duration::from_secs(n),
                _ => return Err(bad("El tiempo máximo de consulta va de 1 a 3600 segundos.")),
            },
        };
        let rls_roles = opt("rls_roles")
            .map(|r| {
                r.split(',')
                    .map(str::trim)
                    .filter(|r| !r.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Ok(Settings {
            auth,
            cloud,
            tenant,
            username: cfg
                .username
                .as_deref()
                .map(str::trim)
                .filter(|u| !u.is_empty())
                .map(str::to_string),
            client_id,
            client_secret,
            client_certificate,
            access_token,
            client_app,
            workspace: text("workspace"),
            database: Some(cfg.database.trim())
                .filter(|d| !d.is_empty())
                .map(str::to_string),
            read_only: cfg.read_only,
            query_api,
            query_timeout,
            rls_user: text("rls_user"),
            rls_roles,
            custom_data: text("custom_data"),
            definition_api: opt("definition_api").is_none_or(|v| v == "true"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(opts: &[(&str, &str)]) -> ConnectionConfig {
        ConnectionConfig {
            driver: "powerbi".into(),
            read_only: true,
            options: opts
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn defaults() {
        let s = Settings::from_config(&cfg(&[])).unwrap();
        assert_eq!(s.auth, AuthMode::EntraMfa);
        assert_eq!(s.cloud, Cloud::Public);
        assert_eq!(s.client_app, ClientApp::Dbine);
        assert_eq!(s.query_api, QueryApi::Auto);
        assert_eq!(s.query_timeout, Duration::from_secs(600));
        assert!(s.read_only && s.definition_api);
        assert!(s.tenant.is_none() && s.workspace.is_none() && s.database.is_none());
    }

    #[test]
    fn secrets_never_show_in_debug() {
        let s = Settings::from_config(&cfg(&[
            ("auth_mode", "entra_sp"),
            ("tenant", "contoso.onmicrosoft.com"),
            ("client_id", "11111111-2222-3333-4444-555555555555"),
            ("client_secret", "hunter2-secret"),
            ("access_token", "hunter2-token"),
        ]))
        .unwrap();
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("hunter2"), "the password shows in Debug");
        assert_eq!(s.client_secret.unwrap().expose(), "hunter2-secret");
    }

    #[test]
    fn each_service_principal_mode_reads_its_own_credential() {
        let both = |mode: &str| {
            Settings::from_config(&cfg(&[
                ("auth_mode", mode),
                ("tenant", "contoso.onmicrosoft.com"),
                ("client_id", "c"),
                ("client_secret", "old-secret"),
                ("client_certificate", " /certs/sp.pfx "),
                ("client_certificate_password", " hunter2 "),
            ]))
            .unwrap()
        };
        let cert = both("entra_sp_cert");
        assert_eq!(cert.auth, AuthMode::EntraSp);
        assert!(cert.client_secret.is_none());
        let c = cert.client_certificate.clone().unwrap();
        assert_eq!(c.path, "/certs/sp.pfx");
        // The password as typed.
        assert_eq!(c.password.unwrap().expose(), " hunter2 ");
        let dbg = format!("{cert:?}");
        assert!(!dbg.contains("hunter2"), "the password shows in Debug");
        let secret = both("entra_sp");
        assert!(secret.client_certificate.is_none());
        assert_eq!(secret.client_secret.unwrap().expose(), "old-secret");
        // No password: none.
        let s = Settings::from_config(&cfg(&[
            ("auth_mode", "entra_sp_cert"),
            ("tenant", "t"),
            ("client_id", "c"),
            ("client_certificate", "/certs/sp.pem"),
            ("client_certificate_password", ""),
        ]))
        .unwrap();
        assert!(s.client_certificate.unwrap().password.is_none());
    }

    #[test]
    fn refuses_what_cant_work() {
        for opts in [
            vec![("auth_mode", "entra_sp")],
            vec![
                ("auth_mode", "entra_sp"),
                ("tenant", "t"),
                ("client_id", "c"),
            ],
            vec![("auth_mode", "token")],
            vec![("auth_mode", "nope")],
            // The certificate mode doesn't take the secret instead.
            vec![
                ("auth_mode", "entra_sp_cert"),
                ("tenant", "t"),
                ("client_id", "c"),
                ("client_secret", "s"),
            ],
            vec![
                ("auth_mode", "entra_sp"),
                ("tenant", "t"),
                ("client_id", "c"),
                ("client_certificate", "/certs/sp.pem"),
            ],
            vec![("tenant", "contoso.com/../x")],
            vec![("cloud", "mars")],
            vec![("query_api", "xmla")],
            vec![("query_timeout", "0")],
            vec![("client_app", "custom")],
            // GCC High's sign-in needs an app registered there.
            vec![("cloud", "gcc_high")],
        ] {
            assert!(Settings::from_config(&cfg(&opts)).is_err(), "{opts:?}");
        }
        let s = Settings::from_config(&cfg(&[
            ("cloud", "gcc_high"),
            ("client_app", "custom"),
            ("app_client_id", "abc"),
            ("rls_roles", " Norte, ,Sur "),
            ("definition_api", "false"),
        ]))
        .unwrap();
        assert_eq!(s.client_app, ClientApp::Custom("abc".into()));
        assert_eq!(s.rls_roles, ["Norte", "Sur"]);
        assert!(!s.definition_api);
    }
}
