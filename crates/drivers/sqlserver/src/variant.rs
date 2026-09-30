//! The engines that speak TDS through this crate: SQL Server itself, Azure
//! SQL Database, Microsoft Fabric Data Warehouse and Babelfish for
//! PostgreSQL. They share the protocol and most of the catalog; what
//! differs is the login (Microsoft Entra ID, forced encryption), a few
//! catalog views and, for Babelfish, the plans and the monitor (it's
//! PostgreSQL underneath).

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

    /// The login used when the form doesn't say.
    pub fn default_auth(self) -> &'static str {
        match self {
            Variant::Fabric => "entra_password",
            _ => "sql",
        }
    }
}

fn auth_field(v: Variant) -> Field {
    let mut options = vec![];
    if v != Variant::Fabric {
        options.push(("sql", "SQL Server (usuario y contraseña)"));
    }
    options.extend([
        ("entra_password", "Microsoft Entra ID: usuario y contraseña"),
        ("entra_sp", "Microsoft Entra ID: entidad de servicio"),
        ("entra_token", "Microsoft Entra ID: token de acceso"),
    ]);
    Field::new("auth", "Autenticación", FieldKind::Select(options))
        .default_value(v.default_auth())
        .help("Entra ID con usuario y contraseña no admite cuentas con MFA: usá una entidad de servicio o un token.")
}

/// The auth methods that sign in with a user and a password.
const USER_AUTH: &[&str] = &["sql", "entra_password"];

fn entra_fields() -> Vec<Field> {
    vec![
        Field::new("tenant_id", "Inquilino (tenant ID)", FieldKind::Text)
            .placeholder("00000000-0000-0000-0000-000000000000 o contoso.onmicrosoft.com")
            .help("Obligatorio para la entidad de servicio; opcional con usuario y contraseña.")
            .when("auth", &["entra_password", "entra_sp"]),
        Field::new("client_id", "ID de la aplicación (client ID)", FieldKind::Text)
            .help("Entidad de servicio: el ID de la aplicación registrada. Con usuario y contraseña, opcional.")
            .when("auth", &["entra_password", "entra_sp"]),
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
            f.push(Field::username().help("Con Entra ID, la cuenta (usuario@dominio).").when("auth", USER_AUTH));
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
                Field::username().help("Con Entra ID, la cuenta (usuario@dominio).").when("auth", USER_AUTH),
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
                Field::username().help("La cuenta de Entra ID (usuario@dominio).").when("auth", USER_AUTH),
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
const SQL_SCOPE: &str = "https://database.windows.net/.default";
/// Public client Microsoft.Data.SqlClient uses for `ActiveDirectoryPassword`.
const SQL_CLIENT_APP: &str = "2fd908ad-0664-4344-b9be-cd3e8b574c38";

/// How to log in, resolved from the form.
pub enum Login {
    Sql { user: String, password: String },
    Token(String),
}

pub async fn login(cfg: &ConnectionConfig, v: Variant) -> Result<Login> {
    let auth = cfg.option("auth").unwrap_or(v.default_auth());
    if auth != "sql" && !v.entra() {
        return Err(Error::AuthFailed("este motor solo admite usuario y contraseña".into()));
    }
    let user = cfg.username.as_deref().map(str::trim).filter(|u| !u.is_empty());
    match auth {
        "sql" => {
            let user = user.ok_or_else(|| Error::AuthFailed("falta el usuario".into()))?;
            Ok(Login::Sql { user: user.to_string(), password: cfg.password_or_empty().to_string() })
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

/// An access token from the Microsoft identity platform (OAuth 2.0 token
/// endpoint, v2).
async fn token(tenant: &str, form: &[(&str, &str)]) -> Result<String> {
    if !tenant.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_')) {
        return Err(Error::AuthFailed("el inquilino (tenant ID) no es válido".into()));
    }
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
    token_from(&body)
}

fn token_from(body: &serde_json::Value) -> Result<String> {
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
