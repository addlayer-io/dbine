//! The engines that speak TDS through this crate: SQL Server itself, Azure
//! SQL Database, Microsoft Fabric Data Warehouse and Babelfish for
//! PostgreSQL. They share the protocol and most of the catalog; what
//! differs is the login (Microsoft Entra ID, forced encryption), a few
//! catalog views and, for Babelfish, the plans and the monitor (it's
//! PostgreSQL underneath).

use crate::entra;
use dbine_driver::{ConnectionConfig, DriverInfo, Error, Family, Field, FieldKind, Language, ObjectKindInfo, Result};
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 1433;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Variant {
    SqlServer,
    AzureSql,
    Fabric,
    Babelfish,
}

impl Variant {
    pub const ALL: [Variant; 4] = [Variant::SqlServer, Variant::AzureSql, Variant::Fabric, Variant::Babelfish];

    /// The server always encrypts (the login refuses plain TDS).
    pub fn forces_encryption(self) -> bool {
        matches!(self, Variant::AzureSql | Variant::Fabric)
    }

    /// Takes TDS bulk loads (`INSERT BULK`) as the transfer uses them.
    pub fn bulk_load(self) -> bool {
        matches!(self, Variant::SqlServer | Variant::AzureSql)
    }

    /// Accepts Microsoft Entra ID logins.
    pub fn entra(self) -> bool {
        !matches!(self, Variant::Babelfish)
    }

    /// Accepts Windows logins (the current user, or a domain user and
    /// password). Azure SQL Database, Fabric and Babelfish have no Active
    /// Directory logins.
    pub fn windows(self) -> bool {
        self == Variant::SqlServer
    }

    /// The login used when the form doesn't say.
    pub fn default_auth(self) -> &'static str {
        match self {
            Variant::Fabric => "entra_password",
            _ => "sql",
        }
    }
}

/// Windows authentication as the user signed in to the computer: SSPI on
/// Windows, a Kerberos ticket (GSSAPI) on macOS and Linux.
pub const WINDOWS_INTEGRATED: &str = "windows_integrated";
/// Windows authentication with a domain user and password (NTLMv2).
pub const WINDOWS_NTLM: &str = "windows_ntlm";

/// NTLM with explicit domain credentials works on every OS: the vendored
/// tiberius uses winauth's pure-Rust NTLMv2 off Windows too (PATCHES.md).
pub const NTLM_SUPPORTED: bool = true;

/// What the auth select explains about the Entra ID methods.
macro_rules! entra_help {
    () => {
        "Entra ID interactivo abre el navegador y admite MFA; con usuario y contraseña no hay MFA. La integrada \
         usa la sesión de Windows (o el ticket de Kerberos) con el ADFS de la organización. La identidad \
         administrada solo funciona si DBine corre en Azure. La predeterminada prueba, en orden, las variables \
         AZURE_TENANT_ID/AZURE_CLIENT_ID/AZURE_CLIENT_SECRET, la identidad administrada, Azure CLI (az login) y \
         Azure Developer CLI (azd auth login)."
    };
}

fn auth_options(v: Variant) -> Vec<(&'static str, &'static str)> {
    let mut options = vec![];
    if v != Variant::Fabric {
        options.push(("sql", "SQL Server (usuario y contraseña)"));
    }
    if v.windows() {
        options.push((WINDOWS_INTEGRATED, "Windows: usuario actual"));
        if NTLM_SUPPORTED {
            options.push((WINDOWS_NTLM, "Windows: usuario y contraseña de dominio"));
        }
    }
    if v.entra() {
        options.extend([
            (entra::MFA, "Microsoft Entra ID: interactivo (MFA)"),
            ("entra_password", "Microsoft Entra ID: usuario y contraseña"),
            (entra::INTEGRATED, "Microsoft Entra ID: integrada (Windows)"),
            ("entra_sp", "Microsoft Entra ID: entidad de servicio"),
            (entra::MSI, "Microsoft Entra ID: identidad administrada"),
            (entra::DEFAULT, "Microsoft Entra ID: predeterminada"),
            ("entra_token", "Microsoft Entra ID: token de acceso"),
        ]);
    }
    options
}

fn auth_field(v: Variant) -> Field {
    let help = if v.windows() {
        concat!(
            "Windows: usuario actual entra con la sesión de Windows o, en macOS y Linux, con el ticket de Kerberos \
             (kinit usuario@DOMINIO). ",
            entra_help!()
        )
    } else {
        entra_help!()
    };
    Field::new("auth", "Autenticación", FieldKind::Select(auth_options(v))).default_value(v.default_auth()).help(help)
}

/// The auth methods that sign in with a user and a password.
const USER_AUTH: &[&str] = &["sql", "entra_password", WINDOWS_NTLM];
/// The auth methods that take a user: those with a password, the browser
/// sign-in (a hint, optional) and the integrated one (the UPN, required).
const ACCOUNT_AUTH: &[&str] = &["sql", "entra_password", WINDOWS_NTLM, entra::MFA, entra::INTEGRATED];

fn entra_fields() -> Vec<Field> {
    vec![
        Field::new("tenant_id", "Inquilino (tenant ID)", FieldKind::Text)
            .placeholder("00000000-0000-0000-0000-000000000000 o contoso.onmicrosoft.com")
            .help(
                "Obligatorio para la entidad de servicio. Con los demás métodos, opcional: sin él vale el \
                 inquilino de la cuenta.",
            )
            .when("auth", &["entra_password", "entra_sp", entra::MFA, entra::INTEGRATED, entra::DEFAULT]),
        Field::new("client_id", "ID de la aplicación (client ID)", FieldKind::Text)
            .help(
                "Entidad de servicio: el ID de la aplicación registrada. Identidad administrada (también en la \
                 predeterminada): el client ID de una identidad asignada por el usuario; vacío, la del sistema. \
                 En los demás casos, opcional.",
            )
            .when("auth", &["entra_password", "entra_sp", entra::MFA, entra::INTEGRATED, entra::MSI, entra::DEFAULT]),
        Field::new("client_secret", "Secreto de la aplicación", FieldKind::Password).secret().when("auth", &["entra_sp"]),
        Field::new("access_token", "Token de acceso", FieldKind::Textarea)
            .secret()
            .when("auth", &["entra_token"])
            .help("Un token para https://database.windows.net/ (por ejemplo, de `az account get-access-token --resource https://database.windows.net/`). Vence en una hora."),
    ]
}

fn object_kinds(v: Variant) -> Vec<ObjectKindInfo> {
    let mut k = vec![ObjectKindInfo::tables(), ObjectKindInfo::views(), ObjectKindInfo::procedures(), ObjectKindInfo::functions()];
    if v != Variant::Fabric {
        k.push(ObjectKindInfo::triggers());
    }
    k.extend(crate::structure::object_kinds(v));
    k
}

pub fn info(v: Variant) -> DriverInfo {
    let (id, name, fields) = match v {
        Variant::SqlServer => {
            let mut f = vec![Field::host(), Field::port(), Field::database(), auth_field(v)];
            f.push(
                Field::username()
                    .help(
                        "Con Entra ID, la cuenta (usuario@dominio): obligatoria con la integrada, opcional con la \
                         interactiva. Con Windows, DOMINIO\\usuario.",
                    )
                    .when("auth", ACCOUNT_AUTH),
            );
            f.push(Field::password().when("auth", USER_AUTH));
            f.extend(entra_fields());
            f.extend([Field::encrypt(), Field::trust_cert(), Field::read_only()]);
            ("sqlserver", "SQL Server", f)
        }
        Variant::AzureSql => {
            let mut f = vec![
                Field::host().placeholder("miservidor.database.windows.net"),
                Field::port().placeholder("1433"),
                Field::database().required().placeholder("mibase").help("Azure SQL no permite cambiar de base en la misma conexión."),
                auth_field(v),
                Field::username()
                    .help("Con Entra ID, la cuenta (usuario@dominio): obligatoria con la integrada, opcional con la interactiva.")
                    .when("auth", ACCOUNT_AUTH),
                Field::password().when("auth", USER_AUTH),
            ];
            f.extend(entra_fields());
            f.extend([Field::trust_cert(), Field::read_only()]);
            ("azuresql", "Azure SQL Database", f)
        }
        Variant::Fabric => {
            let mut f = vec![
                Field::host()
                    .placeholder("xxxxxxxx.datawarehouse.fabric.microsoft.com")
                    .help("El «SQL connection string» del almacén (Configuración → Cadenas de conexión)."),
                Field::port().placeholder("1433"),
                Field::database().required().placeholder("MiAlmacen").help("El nombre del almacén o del punto de conexión SQL."),
                auth_field(v),
                Field::username()
                    .help("La cuenta de Entra ID (usuario@dominio): obligatoria con la integrada, opcional con la interactiva.")
                    .when("auth", ACCOUNT_AUTH),
                Field::password().when("auth", USER_AUTH),
            ];
            f.extend(entra_fields());
            f.push(Field::read_only());
            ("fabric", "Microsoft Fabric Data Warehouse", f)
        }
        Variant::Babelfish => {
            let mut f = Field::server_set();
            f[3] = Field::username().required();
            f[1] = Field::port().placeholder("1433");
            ("babelfish", "Babelfish for PostgreSQL", f)
        }
    };
    DriverInfo {
        id,
        name,
        family: if v == Variant::Fabric { Family::Analytical } else { Family::Relational },
        language: Language::Sql,
        dialect: "mssql",
        default_port: DEFAULT_PORT,
        fields,
        databases_label: "Bases de datos",
        has_schemas: true,
        object_kinds: object_kinds(v),
    }
}

// ------------------------------------------------------ Microsoft Entra ID

/// Resource every Azure SQL / Fabric SQL endpoint accepts tokens for.
pub(crate) const SQL_SCOPE: &str = "https://database.windows.net/.default";
/// Public client Microsoft.Data.SqlClient uses for `ActiveDirectoryPassword`.
pub(crate) const SQL_CLIENT_APP: &str = "2fd908ad-0664-4344-b9be-cd3e8b574c38";

/// How to log in, resolved from the form.
pub enum Login {
    Sql { user: String, password: String },
    Token(String),
    /// The user signed in to the computer (SSPI / Kerberos).
    Integrated,
    /// A domain user (`DOMINIO\usuario`) and password, over NTLMv2. Built
    /// only where tiberius can use it (see [`NTLM_SUPPORTED`]).
    #[cfg_attr(not(windows), allow(dead_code))]
    Windows { user: String, password: String },
}

pub async fn login(cfg: &ConnectionConfig, v: Variant) -> Result<Login> {
    let auth = cfg.option("auth").unwrap_or(v.default_auth());
    let windows = matches!(auth, WINDOWS_INTEGRATED | WINDOWS_NTLM);
    if windows && !v.windows() {
        return Err(Error::AuthFailed("este motor no admite la autenticación de Windows".into()));
    }
    if auth != "sql" && !windows && !v.entra() {
        return Err(Error::AuthFailed("este motor solo admite usuario y contraseña".into()));
    }
    let user = cfg.username.as_deref().map(str::trim).filter(|u| !u.is_empty());
    match auth {
        "sql" => {
            let user = user.ok_or_else(|| Error::AuthFailed("falta el usuario".into()))?;
            Ok(Login::Sql { user: user.to_string(), password: cfg.password_or_empty().to_string() })
        }
        WINDOWS_INTEGRATED => Ok(Login::Integrated),
        WINDOWS_NTLM if !NTLM_SUPPORTED => Err(Error::AuthFailed(
            "Windows con usuario y contraseña de dominio solo está disponible en Windows. En macOS y Linux \
             usá «Windows: usuario actual» con un ticket de Kerberos (kinit usuario@DOMINIO)."
                .into(),
        )),
        WINDOWS_NTLM => {
            let user = user.ok_or_else(|| Error::AuthFailed("falta el usuario de dominio (DOMINIO\\usuario)".into()))?;
            Ok(Login::Windows { user: user.to_string(), password: cfg.password_or_empty().to_string() })
        }
        "entra_token" => {
            let t = cfg.option("access_token").map(str::trim).filter(|t| !t.is_empty());
            let t = t.ok_or_else(|| Error::AuthFailed("falta el token de acceso".into()))?;
            Ok(Login::Token(t.trim_start_matches("Bearer ").to_string()))
        }
        "entra_sp" => {
            let tenant = cfg.option("tenant_id").ok_or_else(|| Error::AuthFailed("falta el inquilino (tenant ID)".into()))?;
            let client = cfg.option("client_id").ok_or_else(|| Error::AuthFailed("falta el ID de la aplicación".into()))?;
            let secret = cfg.option("client_secret").ok_or_else(|| Error::AuthFailed("falta el secreto de la aplicación".into()))?;
            let form = [("grant_type", "client_credentials"), ("client_id", client), ("client_secret", secret), ("scope", SQL_SCOPE)];
            Ok(Login::Token(token(tenant, &form).await?))
        }
        entra::MFA => Ok(Login::Token(entra::interactive(option(cfg, "tenant_id"), option(cfg, "client_id"), user).await?)),
        entra::INTEGRATED => {
            let user = user.ok_or_else(|| Error::AuthFailed("falta la cuenta de Entra ID (usuario@dominio)".into()))?;
            Ok(Login::Token(entra::integrated(user, option(cfg, "tenant_id"), option(cfg, "client_id")).await?))
        }
        entra::MSI => Ok(Login::Token(entra::managed_identity(option(cfg, "client_id")).await?)),
        entra::DEFAULT => Ok(Login::Token(entra::default_chain(option(cfg, "tenant_id"), option(cfg, "client_id")).await?)),
        "entra_password" => {
            let user = user.ok_or_else(|| Error::AuthFailed("falta la cuenta de Entra ID (usuario@dominio)".into()))?;
            let tenant = cfg.option("tenant_id").unwrap_or("organizations");
            let client = cfg.option("client_id").unwrap_or(SQL_CLIENT_APP);
            let form = [
                ("grant_type", "password"),
                ("client_id", client),
                ("username", user),
                ("password", cfg.password_or_empty()),
                ("scope", SQL_SCOPE),
            ];
            Ok(Login::Token(token(tenant, &form).await?))
        }
        other => Err(Error::AuthFailed(format!("autenticación desconocida: {other}"))),
    }
}

/// A form option, trimmed; `None` when empty.
fn option<'a>(cfg: &'a ConnectionConfig, key: &str) -> Option<&'a str> {
    cfg.option(key).map(str::trim).filter(|v| !v.is_empty())
}

/// A tenant ID or domain, safe to put in a URL path.
pub(crate) fn valid_tenant(tenant: &str) -> Result<()> {
    if tenant.is_empty() || !tenant.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_')) {
        return Err(Error::AuthFailed("el inquilino (tenant ID) no es válido".into()));
    }
    Ok(())
}

/// An access token from the Microsoft identity platform (OAuth 2.0 token
/// endpoint, v2).
async fn token(tenant: &str, form: &[(&str, &str)]) -> Result<String> {
    token_from(&token_body(tenant, form).await?)
}

/// The token endpoint's whole answer (the token, its expiry, a refresh
/// token…), or the error to show.
pub(crate) async fn token_body(tenant: &str, form: &[(&str, &str)]) -> Result<serde_json::Value> {
    valid_tenant(tenant)?;
    let url = format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| Error::Connect(e.to_string()))?;
    let resp = client
        .post(url)
        .form(form)
        .send()
        .await
        .map_err(|e| Error::Connect(format!("no se pudo llegar a Microsoft Entra ID: {e}")))?;
    let body: serde_json::Value = resp.json().await.map_err(|e| Error::Connect(e.to_string()))?;
    token_from(&body)?;
    Ok(body)
}

pub(crate) fn token_from(body: &serde_json::Value) -> Result<String> {
    if let Some(t) = body.get("access_token").and_then(|t| t.as_str()) {
        return Ok(t.to_string());
    }
    let desc = body
        .get("error_description")
        .or_else(|| body.get("error"))
        .and_then(|d| d.as_str())
        .unwrap_or("respuesta inesperada de Microsoft Entra ID");
    // The first line; the rest is trace and correlation ids.
    Err(Error::AuthFailed(format!("Microsoft Entra ID rechazó el inicio de sesión: {}", desc.lines().next().unwrap_or(desc))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn token_errors_read_as_auth_failures() {
        assert_eq!(token_from(&json!({"access_token": "abc"})).unwrap(), "abc");
        let e = token_from(&json!({"error": "invalid_grant", "error_description": "AADSTS50126: bad password\r\nTrace ID: x"}));
        assert!(matches!(e, Err(Error::AuthFailed(m)) if m.ends_with("AADSTS50126: bad password")));
    }

    #[test]
    fn variants_declare_their_forms() {
        let ids: Vec<_> = Variant::ALL.iter().map(|v| info(*v).id).collect();
        assert_eq!(ids, ["sqlserver", "azuresql", "fabric", "babelfish"]);
        // Fabric has no SQL logins and Babelfish no Entra ID.
        let fabric = info(Variant::Fabric);
        let auth = fabric.fields.iter().find(|f| f.key == "auth").unwrap();
        assert!(matches!(&auth.kind, FieldKind::Select(o) if o.iter().all(|(k, _)| *k != "sql")));
        assert!(info(Variant::Babelfish).fields.iter().all(|f| f.key != "auth"));
        assert!(Variant::AzureSql.forces_encryption() && !Variant::Babelfish.forces_encryption());
    }

    fn auth_keys(v: Variant) -> Vec<&'static str> {
        auth_options(v).into_iter().map(|(k, _)| k).collect()
    }

    /// The fields a form shows for `auth` (no `when`, or a `when` on `auth`
    /// that lists it).
    fn shown(v: Variant, auth: &str) -> Vec<&'static str> {
        info(v)
            .fields
            .into_iter()
            .filter(|f| f.when.as_ref().is_none_or(|w| w.key != "auth" || w.values.contains(&auth)))
            .map(|f| f.key)
            .collect()
    }

    #[test]
    fn windows_logins_only_on_sql_server() {
        let ms = auth_keys(Variant::SqlServer);
        assert!(ms.contains(&WINDOWS_INTEGRATED));
        assert!(ms.contains(&WINDOWS_NTLM));
        for v in [Variant::AzureSql, Variant::Fabric] {
            let keys = auth_keys(v);
            assert!(!keys.contains(&WINDOWS_INTEGRATED) && !keys.contains(&WINDOWS_NTLM), "{v:?}");
        }
        // SQL logins stay the default.
        assert_eq!(Variant::SqlServer.default_auth(), "sql");
    }

    #[test]
    fn windows_logins_show_their_fields() {
        let v = Variant::SqlServer;
        // The current user: neither user nor password.
        let integrated = shown(v, WINDOWS_INTEGRATED);
        assert!(!integrated.contains(&"username") && !integrated.contains(&"password"), "{integrated:?}");
        assert!(!integrated.iter().any(|k| ["tenant_id", "client_id", "client_secret", "access_token"].contains(k)));
        assert!(integrated.contains(&"host") && integrated.contains(&"encrypt"));
        // A domain user: user and password (the password is a secret).
        let ntlm = shown(v, WINDOWS_NTLM);
        assert!(ntlm.contains(&"username") && ntlm.contains(&"password"), "{ntlm:?}");
        assert!(info(v).fields.iter().any(|f| f.key == "password" && f.secret));
        // SQL logins are unchanged.
        let sql = shown(v, "sql");
        assert!(sql.contains(&"username") && sql.contains(&"password"));
    }

    #[tokio::test]
    async fn windows_logins_resolve() {
        let mut cfg = ConnectionConfig::default();
        cfg.options.insert("auth".into(), WINDOWS_INTEGRATED.into());
        assert!(matches!(login(&cfg, Variant::SqlServer).await, Ok(Login::Integrated)));
        for v in [Variant::AzureSql, Variant::Fabric, Variant::Babelfish] {
            assert!(matches!(login(&cfg, v).await, Err(Error::AuthFailed(m)) if m.contains("Windows")), "{v:?}");
        }
        cfg.options.insert("auth".into(), WINDOWS_NTLM.into());
        if NTLM_SUPPORTED {
            assert!(matches!(login(&cfg, Variant::SqlServer).await, Err(Error::AuthFailed(m)) if m.contains("DOMINIO")));
            cfg.username = Some(" CONTOSO\\ana ".into());
            cfg.password = Some("x".into());
            assert!(matches!(login(&cfg, Variant::SqlServer).await, Ok(Login::Windows { user, .. }) if user == "CONTOSO\\ana"));
        } else {
            assert!(matches!(login(&cfg, Variant::SqlServer).await, Err(Error::AuthFailed(m)) if m.contains("solo está disponible en Windows")));
        }
    }

    #[test]
    fn entra_methods_match_ssms() {
        for v in [Variant::SqlServer, Variant::AzureSql, Variant::Fabric] {
            let keys = auth_keys(v);
            for k in [entra::MFA, "entra_password", entra::INTEGRATED, "entra_sp", entra::MSI, entra::DEFAULT, "entra_token"] {
                assert!(keys.contains(&k), "{v:?} {k}");
            }
        }
        assert!(auth_keys(Variant::Babelfish).iter().all(|k| !k.starts_with("entra")));
    }

    #[test]
    fn entra_methods_show_their_fields() {
        for v in [Variant::SqlServer, Variant::AzureSql, Variant::Fabric] {
            let mfa = shown(v, entra::MFA);
            assert!(mfa.contains(&"username") && mfa.contains(&"tenant_id") && mfa.contains(&"client_id"), "{mfa:?}");
            assert!(!mfa.contains(&"password") && !mfa.contains(&"client_secret") && !mfa.contains(&"access_token"));
            let integrated = shown(v, entra::INTEGRATED);
            assert!(integrated.contains(&"username") && integrated.contains(&"tenant_id") && !integrated.contains(&"password"));
            let msi = shown(v, entra::MSI);
            assert!(msi.contains(&"client_id") && !msi.contains(&"tenant_id") && !msi.contains(&"username"), "{msi:?}");
            let default = shown(v, entra::DEFAULT);
            assert!(default.contains(&"tenant_id") && default.contains(&"client_id") && !default.contains(&"username"));
            assert!(!default.contains(&"password") && !default.contains(&"client_secret"));
        }
    }

    #[tokio::test]
    async fn entra_logins_check_their_fields_first() {
        let mut cfg = ConnectionConfig::default();
        cfg.options.insert("auth".into(), entra::INTEGRATED.into());
        assert!(matches!(login(&cfg, Variant::AzureSql).await, Err(Error::AuthFailed(m)) if m.contains("usuario@dominio")));
        cfg.username = Some("sin-dominio".into());
        assert!(matches!(login(&cfg, Variant::AzureSql).await, Err(Error::AuthFailed(m)) if m.contains("usuario@dominio")));
        for auth in [entra::MFA, entra::INTEGRATED, entra::DEFAULT] {
            cfg.options.insert("auth".into(), auth.into());
            cfg.options.insert("tenant_id".into(), "contoso/../x".into());
            assert!(matches!(login(&cfg, Variant::Fabric).await, Err(Error::AuthFailed(m)) if m.contains("inquilino")), "{auth}");
            assert!(matches!(login(&cfg, Variant::Babelfish).await, Err(Error::AuthFailed(_))), "{auth}");
        }
    }

    #[tokio::test]
    async fn logins_need_their_fields() {
        let mut cfg = ConnectionConfig { username: Some("sa".into()), password: Some("x".into()), ..Default::default() };
        assert!(matches!(login(&cfg, Variant::SqlServer).await, Ok(Login::Sql { .. })));
        cfg.options.insert("auth".into(), "entra_token".into());
        assert!(matches!(login(&cfg, Variant::AzureSql).await, Err(Error::AuthFailed(_))));
        cfg.options.insert("access_token".into(), "Bearer eyJ".into());
        assert!(matches!(login(&cfg, Variant::AzureSql).await, Ok(Login::Token(t)) if t == "eyJ"));
        assert!(matches!(login(&cfg, Variant::Babelfish).await, Err(Error::AuthFailed(_))));
        cfg.options.insert("auth".into(), "entra_sp".into());
        assert!(matches!(login(&cfg, Variant::Fabric).await, Err(Error::AuthFailed(m)) if m.contains("tenant")));
    }
}
