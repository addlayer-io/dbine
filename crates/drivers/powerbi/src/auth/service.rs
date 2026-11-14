//! Sign-ins without a browser (spec §3.3–§3.5). Owned by part B.
//!
//! - `entra_sp`: client credentials, `scope={res}/.default`, with the
//!   client secret, or (`entra_sp_cert`) a client assertion signed with the
//!   certificate ([`super::certificate`]).
//! - `entra_msi`: IMDS / App Service endpoint with `resource={res}`.
//! - `entra_default`: `AZURE_TENANT_ID`/`AZURE_CLIENT_ID`/
//!   `AZURE_CLIENT_SECRET` (or `AZURE_CLIENT_CERTIFICATE_PATH` and
//!   `AZURE_CLIENT_CERTIFICATE_PASSWORD`), then managed identity, then `az account
//!   get-access-token --resource {res}`, then `azd auth token --scope
//!   {res}/.default`.
//! - `token`: the pasted token as is; its `aud` is checked only to say
//!   clearly that it isn't for Power BI (the resource URI or Power BI's app
//!   id are both accepted, S10), and an expired one is said too. The
//!   service is what checks the token.
//!
//! Adapted from `sqlserver/src/entra.rs` (managed identity, the default
//! chain and the CLIs: no shell, fixed arguments, a 30 s limit).

use super::certificate::{self, Alg, Certificate, ASSERTION_TYPE};
use super::cloud::POWER_BI_APP_ID;
use super::{
    auth, claims, now, number, resource, short, token_endpoint, token_url, AccessToken, Audience,
    Endpoints, TokenSet,
};
use crate::config::{Secret, Settings};
use dbine_driver::{Error, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

fn token_set(access: &str, expires_on: u64) -> TokenSet {
    TokenSet {
        access: AccessToken {
            token: Secret::new(access),
            expires_at: Some(UNIX_EPOCH + Duration::from_secs(expires_on)),
        },
        refresh: None,
    }
}

// ------------------------------------------------- service principal

pub async fn client_credentials(settings: &Settings, audience: Audience) -> Result<TokenSet> {
    client_credentials_at(settings, &Endpoints::of(settings.cloud), audience).await
}

pub(crate) async fn client_credentials_at(
    settings: &Settings,
    ep: &Endpoints,
    audience: Audience,
) -> Result<TokenSet> {
    let (credential, missing) = match (&settings.client_certificate, &settings.client_secret) {
        (Some(c), _) => (
            Some(SpCredential::Certificate {
                path: c.path.clone(),
                password: c.password.clone(),
            }),
            "La entidad de servicio necesita el inquilino, el Client ID y el certificado.",
        ),
        (None, secret) => (
            secret.clone().map(SpCredential::Secret),
            "La entidad de servicio necesita el inquilino, el Client ID y el secreto.",
        ),
    };
    let (Some(tenant), Some(client), Some(credential)) = (
        settings.tenant.as_deref(),
        settings.client_id.as_deref(),
        credential,
    ) else {
        return Err(auth(missing));
    };
    sp_token(settings, ep, audience, tenant, client, &credential).await
}

/// How a service principal proves itself.
#[derive(Debug, PartialEq)]
enum SpCredential {
    Secret(Secret),
    /// A PEM or PFX file and its password ([`certificate`]).
    Certificate {
        path: String,
        password: Option<Secret>,
    },
}

/// Client credentials at the tenant's token endpoint, with the secret or
/// with a client assertion signed by the certificate.
async fn sp_token(
    settings: &Settings,
    ep: &Endpoints,
    audience: Audience,
    tenant: &str,
    client: &str,
    credential: &SpCredential,
) -> Result<TokenSet> {
    let scope = format!("{}/.default", resource(settings.cloud, audience));
    let mut form = vec![
        ("grant_type", "client_credentials".to_string()),
        ("client_id", client.to_string()),
        ("scope", scope),
    ];
    let thumbprint = match credential {
        SpCredential::Secret(secret) => {
            form.push(("client_secret", secret.expose().to_string()));
            None
        }
        SpCredential::Certificate { path, password } => {
            // The key lives only for this signature: dropped before the
            // request.
            let cert = Certificate::load(path, password.as_ref().map(Secret::expose))?;
            let aud = token_url(&ep.login, tenant)?;
            let assertion = cert.assertion(client, aud.as_str(), Alg::Ps256, now())?;
            form.push(("client_assertion_type", ASSERTION_TYPE.to_string()));
            form.push(("client_assertion", assertion));
            Some(cert.thumbprint())
        }
    };
    let form: Vec<(&str, &str)> = form.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let result = token_endpoint(settings, &ep.login, tenant, &form).await;
    match thumbprint {
        Some(t) => result.map_err(|e| certificate::explain_refusal(e, &t)),
        None => result,
    }
}

// ------------------------------------------------ managed identity (MSI)

const IMDS: &str = "http://169.254.169.254/metadata/identity/oauth2/token";
const NO_MSI: &str =
    "no se encontró una identidad administrada: esta opción solo funciona si DBine corre en una VM o servicio de Azure";

pub async fn managed_identity(settings: &Settings, audience: Audience) -> Result<TokenSet> {
    msi_token(
        resource(settings.cloud, audience),
        settings.client_id.as_deref(),
        Duration::from_secs(2),
    )
    .await
}

#[derive(Debug, PartialEq)]
struct MsiRequest {
    url: String,
    header: (&'static str, String),
    /// The VM's metadata service (IMDS), not App Service's endpoint.
    imds: bool,
}

/// The request for a token: App Service / Functions publish
/// `IDENTITY_ENDPOINT` and `IDENTITY_HEADER`; elsewhere, IMDS.
fn msi_request(
    app_service: Option<(String, String)>,
    resource: &str,
    client: Option<&str>,
) -> Result<MsiRequest> {
    let (base, version, header, imds) = match app_service {
        Some((endpoint, secret)) => (endpoint, "2019-08-01", ("X-IDENTITY-HEADER", secret), false),
        None => (
            IMDS.to_string(),
            "2018-02-01",
            ("Metadata", "true".to_string()),
            true,
        ),
    };
    let mut url = reqwest::Url::parse(&base)
        .map_err(|_| auth("IDENTITY_ENDPOINT no es una dirección válida"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(auth("IDENTITY_ENDPOINT no es una dirección válida"));
    }
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("api-version", version)
            .append_pair("resource", resource);
        if let Some(c) = client {
            q.append_pair("client_id", c);
        }
    }
    Ok(MsiRequest {
        url: url.into(),
        header,
        imds,
    })
}

fn app_service_env() -> Option<(String, String)> {
    let endpoint = std::env::var("IDENTITY_ENDPOINT")
        .ok()
        .filter(|v| !v.is_empty())?;
    let secret = std::env::var("IDENTITY_HEADER")
        .ok()
        .filter(|v| !v.is_empty())?;
    Some((endpoint, secret))
}

/// Reads the managed identity endpoint's answer. `expires_on` is Unix
/// seconds (a string on both endpoints); `expires_in` is the fallback.
fn parse_msi(status: u16, body: &str, now: u64) -> Result<TokenSet> {
    let json: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    if let Some(access) = json.get("access_token").and_then(Value::as_str) {
        let expires_on = number(json.get("expires_on"))
            .or_else(|| number(json.get("expires_in")).map(|s| now + s))
            .unwrap_or(now + 3600);
        return Ok(token_set(access, expires_on));
    }
    let desc = ["error_description", "message", "error"]
        .iter()
        .find_map(|k| json.get(k).and_then(Value::as_str))
        .map(|d| short(d, 200))
        .unwrap_or_else(|| format!("HTTP {status}"));
    Err(auth(format!(
        "la identidad administrada no dio un token: {desc}"
    )))
}

async fn msi_token(resource: &str, client: Option<&str>, connect: Duration) -> Result<TokenSet> {
    let req = msi_request(app_service_env(), resource, client)?;
    let http = reqwest::Client::builder()
        .connect_timeout(connect)
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        // IMDS is link-local: a proxy can't reach it.
        .no_proxy()
        .build()
        .map_err(|e| Error::Connect(e.to_string()))?;
    let resp = http
        .get(&req.url)
        .header(req.header.0, &req.header.1)
        .send()
        .await
        .map_err(|e| {
            if req.imds && (e.is_connect() || e.is_timeout()) {
                auth(NO_MSI)
            } else {
                auth(format!(
                    "no se pudo llegar a la identidad administrada: {e}"
                ))
            }
        })?;
    let status = resp.status().as_u16();
    let body = resp
        .text()
        .await
        .map_err(|e| Error::Connect(e.to_string()))?;
    parse_msi(status, &body, now())
}

// ------------------------------------------------- default (chain)

pub async fn default_chain(settings: &Settings, audience: Audience) -> Result<TokenSet> {
    default_chain_at(settings, &Endpoints::of(settings.cloud), audience).await
}

/// Tries, in order and without asking anything: the `AZURE_*` environment
/// variables (service principal), the managed identity, Azure CLI and
/// Azure Developer CLI. The first token wins.
pub(crate) async fn default_chain_at(
    settings: &Settings,
    ep: &Endpoints,
    audience: Audience,
) -> Result<TokenSet> {
    let res = resource(settings.cloud, audience);
    let tenant = settings.tenant.as_deref();
    let mut tried = vec![];
    match env_credential(settings, ep, audience, |k| std::env::var(k).ok()).await {
        Ok(t) => return Ok(t),
        Err(e) => tried.push(("variables de entorno", e)),
    }
    match msi_token(res, settings.client_id.as_deref(), Duration::from_secs(2)).await {
        Ok(t) => return Ok(t),
        Err(e) => tried.push(("identidad administrada", e)),
    }
    match az_cli(res, tenant).await {
        Ok(t) => return Ok(t),
        Err(e) => tried.push(("Azure CLI", e)),
    }
    match azd_cli(res, tenant).await {
        Ok(t) => return Ok(t),
        Err(e) => tried.push(("Azure Developer CLI", e)),
    }
    Err(chain_error(&tried))
}

/// One error that says what was tried and why each step failed.
fn chain_error(tried: &[(&str, Error)]) -> Error {
    let lines: Vec<String> = tried
        .iter()
        .map(|(what, e)| format!("- {what}: {}", short(&e.to_string(), 200)))
        .collect();
    auth(format!(
        "no se pudo obtener un token de Microsoft Entra ID con la credencial predeterminada. Se probó:\n{}",
        lines.join("\n")
    ))
}

/// `AZURE_TENANT_ID` and `AZURE_CLIENT_ID` with `AZURE_CLIENT_SECRET`, or
/// else with `AZURE_CLIENT_CERTIFICATE_PATH` (a PEM or PFX) and its
/// optional `AZURE_CLIENT_CERTIFICATE_PASSWORD`, as Azure Identity's
/// environment credential reads them.
fn env_service_principal(
    get: impl Fn(&str) -> Option<String>,
) -> Result<(String, String, SpCredential)> {
    let get = |k: &str| {
        get(k)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let missing = || {
        auth("no están definidas AZURE_TENANT_ID, AZURE_CLIENT_ID y AZURE_CLIENT_SECRET (o AZURE_CLIENT_CERTIFICATE_PATH)")
    };
    let (Some(tenant), Some(client)) = (get("AZURE_TENANT_ID"), get("AZURE_CLIENT_ID")) else {
        return Err(missing());
    };
    let credential = if let Some(secret) = get("AZURE_CLIENT_SECRET") {
        SpCredential::Secret(Secret::new(secret))
    } else if let Some(path) = get("AZURE_CLIENT_CERTIFICATE_PATH") {
        SpCredential::Certificate {
            path,
            password: get("AZURE_CLIENT_CERTIFICATE_PASSWORD").map(Secret::new),
        }
    } else {
        return Err(missing());
    };
    Ok((tenant, client, credential))
}

async fn env_credential(
    settings: &Settings,
    ep: &Endpoints,
    audience: Audience,
    get: impl Fn(&str) -> Option<String>,
) -> Result<TokenSet> {
    let (tenant, client, credential) = env_service_principal(get)?;
    sp_token(settings, ep, audience, &tenant, &client, &credential).await
}

/// How long a CLI may take (Azure CLI starts Python).
const CLI_WAIT: Duration = Duration::from_secs(30);

async fn az_cli(resource: &str, tenant: Option<&str>) -> Result<TokenSet> {
    let mut args = vec![
        "account",
        "get-access-token",
        "--resource",
        resource,
        "--output",
        "json",
    ];
    if let Some(t) = tenant {
        args.extend(["--tenant", t]);
    }
    let out = run_cli("az", &args, "az login").await?;
    parse_az(&out, &chrono::Local)
}

async fn azd_cli(resource: &str, tenant: Option<&str>) -> Result<TokenSet> {
    let scope = format!("{resource}/.default");
    let mut args = vec![
        "auth",
        "token",
        "--scope",
        scope.as_str(),
        "--output",
        "json",
    ];
    if let Some(t) = tenant {
        args.extend(["--tenant-id", t]);
    }
    let out = run_cli("azd", &args, "azd auth login").await?;
    parse_azd(&out)
}

/// Azure CLI: `accessToken` plus `expires_on` (Unix seconds, newer
/// versions) or `expiresOn` (local time, `2026-10-09 15:04:05.000000`).
fn parse_az<Tz: chrono::TimeZone>(stdout: &str, local: &Tz) -> Result<TokenSet> {
    let json: Value =
        serde_json::from_str(stdout).map_err(|_| auth("Azure CLI devolvió algo que no es JSON"))?;
    let access = json
        .get("accessToken")
        .and_then(Value::as_str)
        .ok_or_else(|| auth("Azure CLI no devolvió un token"))?;
    let expires_on = number(json.get("expires_on"))
        .or_else(|| {
            let s = json.get("expiresOn")?.as_str()?;
            let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f").ok()?;
            let at = local.from_local_datetime(&naive).earliest()?;
            u64::try_from(at.timestamp()).ok()
        })
        .ok_or_else(|| auth("Azure CLI no dijo cuándo vence el token"))?;
    Ok(token_set(access, expires_on))
}

/// Azure Developer CLI: `token` plus `expiresOn` (RFC 3339).
fn parse_azd(stdout: &str) -> Result<TokenSet> {
    let json: Value = serde_json::from_str(stdout)
        .map_err(|_| auth("Azure Developer CLI devolvió algo que no es JSON"))?;
    let access = json
        .get("token")
        .and_then(Value::as_str)
        .ok_or_else(|| auth("Azure Developer CLI no devolvió un token"))?;
    let expires_on = json
        .get("expiresOn")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .and_then(|d| u64::try_from(d.timestamp()).ok())
        .ok_or_else(|| auth("Azure Developer CLI no dijo cuándo vence el token"))?;
    Ok(token_set(access, expires_on))
}

/// Where a CLI may be: `PATH`, then the usual install folders (apps opened
/// from the Finder don't get the shell's `PATH`).
fn cli_candidates(name: &str, path: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    let (files, fixed): (Vec<String>, &[&str]) = if cfg!(windows) {
        (
            vec![format!("{name}.cmd"), format!("{name}.exe")],
            &[
                r"C:\Program Files\Microsoft SDKs\Azure\CLI2\wbin",
                r"C:\Program Files (x86)\Microsoft SDKs\Azure\CLI2\wbin",
                r"C:\Program Files\Azure Dev CLI",
            ],
        )
    } else {
        (
            vec![name.to_string()],
            &["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"],
        )
    };
    let dirs = path
        .map(|p| std::env::split_paths(p).collect::<Vec<_>>())
        .unwrap_or_default();
    let dirs = dirs
        .into_iter()
        .filter(|d| d.is_absolute())
        .chain(fixed.iter().map(PathBuf::from));
    dirs.flat_map(|d| files.iter().map(move |f| d.join(f)).collect::<Vec<_>>())
        .collect()
}

fn find_cli(name: &str) -> Option<PathBuf> {
    cli_candidates(name, std::env::var_os("PATH").as_deref())
        .into_iter()
        .find(|p| p.is_file())
}

/// An argument that's safe anywhere, also on a `cmd.exe` command line.
fn safe_arg(a: &str) -> bool {
    !a.is_empty()
        && a.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ':' | '/'))
}

/// The `cmd.exe` command line that runs a `.cmd` script: `/d` skips
/// AutoRun, `/v:off` keeps `!` literal, `/s /c "…"` strips only the outer
/// quotes. Refuses anything `cmd` would expand.
#[cfg_attr(not(windows), allow(dead_code))]
fn cmd_line(script: &Path, args: &[&str]) -> Result<String> {
    let script = script
        .to_str()
        .ok_or_else(|| auth("ruta del programa no válida"))?;
    if script
        .chars()
        .any(|c| matches!(c, '"' | '%' | '!' | '^' | '\r' | '\n'))
        || !args.iter().all(|a| safe_arg(a))
    {
        return Err(auth("ruta o argumentos del programa no válidos"));
    }
    Ok(format!(
        "/d /v:off /s /c \"\"{script}\" {}\"",
        args.join(" ")
    ))
}

#[cfg(windows)]
fn cli_command(program: &Path, args: &[&str]) -> Result<tokio::process::Command> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let batch = program
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
    let mut cmd = if batch {
        let line = cmd_line(program, args)?;
        let root = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let mut c = tokio::process::Command::new(root.join("System32").join("cmd.exe"));
        c.raw_arg(line);
        c
    } else {
        let mut c = tokio::process::Command::new(program);
        c.args(args);
        c
    };
    cmd.creation_flags(CREATE_NO_WINDOW);
    Ok(cmd)
}

#[cfg(not(windows))]
fn cli_command(program: &Path, args: &[&str]) -> Result<tokio::process::Command> {
    let mut c = tokio::process::Command::new(program);
    c.args(args);
    Ok(c)
}

/// Runs a CLI (no shell) and returns its standard output.
async fn run_cli(name: &str, args: &[&str], login: &str) -> Result<String> {
    let program = find_cli(name).ok_or_else(|| auth("no está instalada"))?;
    if !args.iter().all(|a| safe_arg(a)) {
        return Err(auth("argumentos no válidos"));
    }
    let mut cmd = cli_command(&program, args)?;
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
    let out = match tokio::time::timeout(CLI_WAIT, cmd.output()).await {
        Ok(out) => out.map_err(|e| auth(format!("no se pudo ejecutar: {e}")))?,
        Err(_) => return Err(auth(format!("no respondió en {} s", CLI_WAIT.as_secs()))),
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let line = short(err.trim_start_matches("ERROR:").trim(), 200);
        let line = if line.is_empty() {
            format!("terminó con {}", out.status)
        } else {
            line
        };
        return Err(auth(format!("{line} (¿falta {login}?)")));
    }
    String::from_utf8(out.stdout).map_err(|_| auth("salida no válida"))
}

// ------------------------------------------------- pasted token

/// The pasted token (`token` mode): never refreshed; a wrong audience or an
/// expired token is said clearly (a token that isn't a JWT is passed as is:
/// the service decides).
pub fn pasted(settings: &Settings, audience: Audience) -> Result<TokenSet> {
    if audience == Audience::Fabric {
        return Err(Error::Unsupported(
            "Con un token pegado no se usa la API de Fabric (definición, nombre y descripción del modelo).".into(),
        ));
    }
    let token = settings
        .access_token
        .as_ref()
        .ok_or_else(|| auth("Falta el token de acceso."))?;
    let token = token.expose().trim();
    let Ok(c) = claims(token) else {
        return Ok(token_set(token, now() + 3600));
    };
    let res = settings.cloud.resource();
    let ours = |aud: &str| {
        let aud = aud.trim_end_matches('/');
        aud.eq_ignore_ascii_case(res) || aud.eq_ignore_ascii_case(POWER_BI_APP_ID)
    };
    let auds: Vec<&str> = match c.get("aud") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if !auds.is_empty() && !auds.iter().any(|a| ours(a)) {
        return Err(auth(format!(
            "El token pegado no es para Power BI: su audiencia es «{}» y tiene que ser {res}.",
            short(auds[0], 120)
        )));
    }
    let exp = number(c.get("exp"));
    if exp.is_some_and(|e| e <= now()) {
        return Err(auth("El token pegado venció: pegá uno nuevo."));
    }
    Ok(token_set(token, exp.unwrap_or(now() + 3600)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::certificate::tests::{fixture, PASSWORD};
    use crate::auth::tests::{jwt, settings};
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[test]
    fn msi_requests_carry_the_power_bi_resource() {
        let res = "https://analysis.windows.net/powerbi/api";
        let imds = msi_request(None, res, None).unwrap();
        assert!(imds.imds);
        assert_eq!(
            imds.url,
            "http://169.254.169.254/metadata/identity/oauth2/token?api-version=2018-02-01&resource=https%3A%2F%2Fanalysis.windows.net%2Fpowerbi%2Fapi"
        );
        assert_eq!(imds.header, ("Metadata", "true".to_string()));
        let user = msi_request(None, res, Some("1111-22")).unwrap();
        assert!(user.url.ends_with("&client_id=1111-22"));
        let app = msi_request(
            Some(("http://127.0.0.1:41056/msi/token".into(), "s3cr3t".into())),
            res,
            None,
        )
        .unwrap();
        assert!(!app.imds && app.url.contains("api-version=2019-08-01"));
        assert!(msi_request(Some(("file:///etc/passwd".into(), "x".into())), res, None).is_err());
    }

    #[test]
    fn msi_az_and_azd_answers() {
        let imds = r#"{"access_token":"eyJ0","expires_in":"86399","expires_on":"1760000000","resource":"https://analysis.windows.net/powerbi/api","token_type":"Bearer"}"#;
        let t = parse_msi(200, imds, 5).unwrap();
        assert_eq!(t.access.token.expose(), "eyJ0");
        assert_eq!(
            t.access.expires_at,
            Some(UNIX_EPOCH + Duration::from_secs(1_760_000_000))
        );
        let e = parse_msi(
            400,
            r#"{"error":"invalid_request","error_description":"Identity not found"}"#,
            0,
        )
        .unwrap_err();
        assert!(e.to_string().ends_with("Identity not found"));
        assert!(parse_msi(500, "<html>", 0)
            .unwrap_err()
            .to_string()
            .contains("HTTP 500"));
        let az = r#"{"accessToken":"eyJa","expiresOn":"2026-10-09 15:04:05.000000","expires_on":1791558245,"subscription":"s","tenant":"t","tokenType":"Bearer"}"#;
        let t = parse_az(az, &chrono::Utc).unwrap();
        assert_eq!(
            t.access.expires_at,
            Some(UNIX_EPOCH + Duration::from_secs(1_791_558_245))
        );
        let old = r#"{"accessToken":"eyJb","expiresOn":"2026-10-09 15:04:05.123456","tokenType":"Bearer"}"#;
        assert_eq!(
            parse_az(old, &chrono::Utc).unwrap().access.expires_at,
            Some(UNIX_EPOCH + Duration::from_secs(1_791_558_245))
        );
        assert!(parse_az("Please run 'az login'", &chrono::Utc).is_err());
        let azd = r#"{"token":"eyJc","expiresOn":"2026-10-09T12:04:05-03:00"}"#;
        assert_eq!(
            parse_azd(azd).unwrap().access.expires_at,
            Some(UNIX_EPOCH + Duration::from_secs(1_791_558_245))
        );
        assert!(parse_azd(r#"{"expiresOn":"2026-10-09T15:04:05Z"}"#).is_err());
    }

    #[test]
    fn cli_arguments_are_safe() {
        assert!(safe_arg("https://analysis.windows.net/powerbi/api"));
        assert!(safe_arg("contoso.onmicrosoft.com"));
        for bad in ["", "a b", "a&b", "a\"b", "%PATH%", "a|b"] {
            assert!(!safe_arg(bad), "{bad}");
        }
        assert!(cmd_line(Path::new(r"C:\az.cmd"), &["account", "x&y"]).is_err());
        let found = cli_candidates("az", Some(std::ffi::OsStr::new("relative:/abs")));
        assert!(found.iter().all(|p| p.is_absolute()));
    }

    #[test]
    fn env_credentials_need_a_secret_or_a_certificate() {
        let get = |k: &str| match k {
            "AZURE_TENANT_ID" => Some("t".to_string()),
            "AZURE_CLIENT_ID" => Some(" c ".to_string()),
            _ => None,
        };
        assert!(env_service_principal(get).is_err());
        // The secret first, as Azure Identity does.
        let all = |k: &str| Some(format!("{k}-v"));
        assert_eq!(
            env_service_principal(all).unwrap(),
            (
                "AZURE_TENANT_ID-v".into(),
                "AZURE_CLIENT_ID-v".into(),
                SpCredential::Secret(Secret::new("AZURE_CLIENT_SECRET-v"))
            )
        );
        let cert = |k: &str| match k {
            "AZURE_TENANT_ID" => Some("t".to_string()),
            "AZURE_CLIENT_ID" => Some("c".to_string()),
            "AZURE_CLIENT_CERTIFICATE_PATH" => Some("/certs/sp.pfx".to_string()),
            "AZURE_CLIENT_CERTIFICATE_PASSWORD" => Some("pw".to_string()),
            _ => None,
        };
        assert_eq!(
            env_service_principal(cert).unwrap().2,
            SpCredential::Certificate {
                path: "/certs/sp.pfx".into(),
                password: Some(Secret::new("pw")),
            }
        );
    }

    /// An app the fake endpoint has no certificate for.
    const UNREGISTERED: &str = "app-without-this-certificate";

    /// The fake token endpoint for a certificate: it checks the assertion
    /// as Entra does (type, `aud` = its own URL, `iss` = `sub` = client,
    /// `x5t#S256`, the PS256 signature with the certificate's public key)
    /// and that no secret is sent.
    async fn assertion_endpoint(seen: Arc<Mutex<Vec<String>>>) -> String {
        use crate::http::testing::{serve, Reply};
        use aws_lc_rs::signature::{UnparsedPublicKey, RSA_PSS_2048_8192_SHA256};
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        let public = std::fs::read(fixture("sp_public.der")).unwrap();
        let base = Arc::new(Mutex::new(String::new()));
        let at = base.clone();
        let url = serve(Arc::new(move |req| {
            let form: HashMap<String, String> = reqwest::Url::parse(&format!("http://x/?{}", req.body))
                .unwrap()
                .query_pairs()
                .into_owned()
                .collect();
            let jwt = form.get("client_assertion").cloned().unwrap_or_default();
            let parts: Vec<&str> = jwt.split('.').collect();
            let part = |i: usize| -> Value {
                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[i]).unwrap()).unwrap()
            };
            let (header, claims) = (part(0), part(1));
            let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
            let signed = format!("{}.{}", parts[0], parts[1]);
            let verified = UnparsedPublicKey::new(&RSA_PSS_2048_8192_SHA256, &public)
                .verify(signed.as_bytes(), &signature)
                .is_ok();
            let me = format!("{}{}", at.lock().unwrap(), req.path);
            let ok = verified
                && form.get("client_assertion_type").map(String::as_str) == Some(ASSERTION_TYPE)
                && !form.contains_key("client_secret")
                && header["alg"] == "PS256"
                && header["x5t#S256"] == "VaSm7tyz8X-YzuC4-Omv_-rdaLJ_CQjI9Ck0rpI9uC0"
                && claims["aud"] == me.as_str()
                && claims["iss"] == form["client_id"].as_str()
                && claims["sub"] == form["client_id"].as_str()
                && form["client_id"] != UNREGISTERED;
            seen.lock().unwrap().push(format!("{} {}", req.path, req.body));
            if ok {
                Reply::json(200, r#"{"access_token":"cert-token","expires_in":3599}"#)
            } else {
                Reply::json(
                    401,
                    r#"{"error":"invalid_client","error_description":"AADSTS700027: The certificate with identifier used to sign the client assertion is not registered on application."}"#,
                )
            }
        }))
        .await;
        *base.lock().unwrap() = url.clone();
        url
    }

    #[tokio::test]
    async fn a_certificate_signs_in_with_a_client_assertion() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let base = assertion_endpoint(seen.clone()).await;
        let ep = Endpoints::test(&base);
        let tenant = "contoso.onmicrosoft.com";
        for (file, password) in [
            ("sp.pem", ""),
            ("sp_encrypted.pem", PASSWORD),
            ("sp.pfx", PASSWORD),
        ] {
            let path = fixture(file);
            let s = settings(
                &[
                    ("auth_mode", "entra_sp_cert"),
                    ("tenant", tenant),
                    ("client_id", "app-cert"),
                    // Left from the secret mode: never sent.
                    ("client_secret", "leftover-secret"),
                    ("client_certificate", &path),
                    ("client_certificate_password", password),
                ],
                true,
            );
            let t = client_credentials_at(&s, &ep, Audience::PowerBi)
                .await
                .unwrap_or_else(|e| panic!("{file}: {e}"));
            assert_eq!(t.access.token.expose(), "cert-token");
        }
        let seen = seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 3);
        for s in &seen {
            assert!(
                s.starts_with("/login/contoso.onmicrosoft.com/oauth2/v2.0/token "),
                "the request went to another path"
            );
            assert!(
                !s.contains("leftover-secret") && !s.contains("client_secret"),
                "a secret was sent"
            );
            assert!(
                s.contains("grant_type=client_credentials"),
                "not a client-credentials request"
            );
        }
        // The environment's certificate (default chain), on the same path.
        let env = |k: &str| match k {
            "AZURE_TENANT_ID" => Some(tenant.to_string()),
            "AZURE_CLIENT_ID" => Some("app-env".to_string()),
            "AZURE_CLIENT_CERTIFICATE_PATH" => Some(fixture("sp.pfx")),
            "AZURE_CLIENT_CERTIFICATE_PASSWORD" => Some(PASSWORD.to_string()),
            _ => None,
        };
        let s = settings(&[("auth_mode", "entra_default")], true);
        let t = env_credential(&s, &ep, Audience::PowerBi, env)
            .await
            .unwrap();
        assert_eq!(t.access.token.expose(), "cert-token");
    }

    #[tokio::test]
    async fn a_certificate_entra_doesnt_know_names_its_thumbprint() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let base = assertion_endpoint(seen).await;
        let ep = Endpoints::test(&base);
        // The fake endpoint doesn't know this app's certificate.
        let s = settings(
            &[
                ("auth_mode", "entra_sp_cert"),
                ("tenant", "contoso.onmicrosoft.com"),
                ("client_id", UNREGISTERED),
                ("client_certificate", &fixture("sp_pkcs1.pem")),
            ],
            true,
        );
        let e = client_credentials_at(&s, &ep, Audience::PowerBi)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("AADSTS700027") && e.contains("B6814EA4B718AF44C86D4FC24FC59D45D98A88B8"),
            "{e}"
        );
        // A file that can't be read fails before any request.
        let mut missing = s.clone();
        missing.client_certificate = Some(crate::config::ClientCertificate {
            path: "/nonexistent/dbine/sp.pem".into(),
            password: None,
        });
        let e = client_credentials_at(&missing, &ep, Audience::PowerBi)
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains("no se pudo leer el certificado"),
            "{e}"
        );
        // The default chain cuts each step's error short: the environment's
        // certificate still shows its thumbprint.
        let env = |k: &str| match k {
            "AZURE_TENANT_ID" => Some("contoso.onmicrosoft.com".to_string()),
            "AZURE_CLIENT_ID" => Some(UNREGISTERED.to_string()),
            "AZURE_CLIENT_CERTIFICATE_PATH" => Some(fixture("sp.pem")),
            _ => None,
        };
        let s = settings(&[("auth_mode", "entra_default")], true);
        let step = env_credential(&s, &ep, Audience::PowerBi, env)
            .await
            .unwrap_err();
        let e = chain_error(&[("variables de entorno", step)]).to_string();
        assert!(
            e.contains("AADSTS700027") && e.contains("B6814EA4B718AF44C86D4FC24FC59D45D98A88B8"),
            "{e}"
        );
    }

    #[test]
    fn pasted_tokens_are_checked_for_clear_errors_only() {
        let with = |claims: Value| {
            settings(
                &[("auth_mode", "token"), ("access_token", &jwt(&claims))],
                true,
            )
        };
        let future = now() + 3600;
        // The resource URI (with or without `/`) or Power BI's app id.
        for aud in [
            "https://analysis.windows.net/powerbi/api",
            "https://analysis.windows.net/powerbi/api/",
            POWER_BI_APP_ID,
        ] {
            let t = pasted(&with(json!({"aud": aud, "exp": future})), Audience::PowerBi).unwrap();
            assert_eq!(
                t.access.expires_at,
                Some(UNIX_EPOCH + Duration::from_secs(future))
            );
        }
        let e = pasted(
            &with(json!({"aud": "https://database.windows.net/", "exp": future})),
            Audience::PowerBi,
        )
        .unwrap_err();
        assert!(e.to_string().contains("no es para Power BI"), "{e}");
        let e = pasted(
            &with(json!({"aud": POWER_BI_APP_ID, "exp": 1})),
            Audience::PowerBi,
        )
        .unwrap_err();
        assert!(e.to_string().contains("venció"), "{e}");
        // Not a JWT: the service decides.
        let opaque = settings(&[("auth_mode", "token"), ("access_token", "opaque")], true);
        assert!(pasted(&opaque, Audience::PowerBi).is_ok());
        assert!(matches!(
            pasted(&opaque, Audience::Fabric),
            Err(Error::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn client_credentials_against_a_fake_token_endpoint() {
        use crate::http::testing::{serve, Reply};
        let seen = Arc::new(Mutex::new(String::new()));
        let log = seen.clone();
        let base = serve(Arc::new(move |req| {
            *log.lock().unwrap() = format!("{} {}", req.path, req.body);
            Reply::json(
                200,
                r#"{"access_token":"sp-token","expires_in":3599,"token_type":"Bearer"}"#,
            )
        }))
        .await;
        let s = settings(
            &[
                ("auth_mode", "entra_sp"),
                ("tenant", "contoso.onmicrosoft.com"),
                ("client_id", "app-1"),
                ("client_secret", "s3cret"),
            ],
            true,
        );
        let t = client_credentials_at(&s, &Endpoints::test(&base), Audience::PowerBi)
            .await
            .unwrap();
        assert_eq!(t.access.token.expose(), "sp-token");
        assert!(t.refresh.is_none());
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.starts_with("/login/contoso.onmicrosoft.com/oauth2/v2.0/token "),
            "{seen}"
        );
        assert!(
            seen.contains("grant_type=client_credentials")
                && seen.contains(
                    "scope=https%3A%2F%2Fanalysis.windows.net%2Fpowerbi%2Fapi%2F.default"
                ),
            "{seen}"
        );
    }
}
