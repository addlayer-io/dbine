//! Cloud sign-in for managed PostgreSQL and MySQL: AWS IAM (RDS and
//! Aurora), Google Cloud IAM (Cloud SQL, AlloyDB) and Microsoft Entra ID
//! (Azure Database for PostgreSQL / MySQL). Each one ends in a short-lived
//! token that goes as the password; DBine makes a new one before every
//! connect, so nobody pastes tokens that expire in 15 minutes.
//!
//! - AWS: the RDS auth token is a SigV4 presigned URL, signed here
//!   (`rds-db`, `Action=connect`) with the credentials of the standard
//!   chain: environment variables, the profile's static keys,
//!   `credential_process`, and for everything else (SSO, assumed roles…)
//!   `aws configure export-credentials`.
//! - Google Cloud: `gcloud sql generate-login-token` (a token scoped to the
//!   Cloud SQL login) or, for AlloyDB, `gcloud auth print-access-token`;
//!   then the application default credentials and the metadata server.
//! - Azure: `az account get-access-token --resource-type oss-rdbms`, the
//!   Azure Developer CLI, then the managed identity.
//!
//! Tokens and credentials live in this process's memory only: never on
//! disk, in the state file or in the logs, and the phone and MCP never see
//! them (they only reach the database's login).
//!
//! The PostgreSQL crate owns this file; the MySQL crate and the app (the
//! secret managers of src-tauri/src/secret_sources reuse the credential
//! chains and SigV4) include it with `#[path]`, so it only depends on what
//! all of them declare: `dbine-driver`, `serde_json`, `tokio` and `sha2`
//! (0.10 or 0.11, same API here).

// Each crate uses a different subset.
#![allow(dead_code)]

use dbine_driver::{ConnectionConfig, Error, Field, FieldKind, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The option that picks the sign-in.
pub const AUTH: &str = "auth";
pub const PASSWORD: &str = "password";
pub const AWS: &str = "aws_iam";
pub const GCP: &str = "gcp_iam";
pub const AZURE: &str = "azure_entra";
/// Set by the app's "Probar" only: the session then says who signed in.
pub const DESCRIBE: &str = "describe_identity";
/// Where an SSH tunnel goes (the app sets them when it rewrites the host
/// to the tunnel's local end): the token is signed for the real endpoint.
pub const ORIGIN_HOST: &str = "tunnel.origin_host";
pub const ORIGIN_PORT: &str = "tunnel.origin_port";

/// Amazon's RDS CA bundle (all commercial regions): RDS certificates are
/// signed by Amazon RDS roots that no system or webpki store has.
/// <https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem>
pub const RDS_CA_BUNDLE: &[u8] = include_bytes!("rds-global-bundle.pem");

/// A cached token is reused until this long before it expires.
const MARGIN: u64 = 5 * 60;
/// How long a CLI may take (az and gcloud start Python; SSO may refresh).
const CLI_WAIT: Duration = Duration::from_secs(60);
/// The RDS token's lifetime (AWS's maximum); it's only needed to log in.
const RDS_TOKEN_TTL: u64 = 900;
/// Tokens whose expiry nobody tells (gcloud) are kept this long.
const UNKNOWN_TTL: u64 = 10 * 60;

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn auth(msg: impl Into<String>) -> Error {
    Error::AuthFailed(msg.into())
}

/// The first line of a message, at most `max` characters.
pub fn short(msg: &str, max: usize) -> String {
    let line = msg.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    match line.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}

// ------------------------------------------------------------------ form

/// Which clouds a variant offers.
#[derive(Debug, Clone, Copy, Default)]
pub struct Offer {
    pub aws: bool,
    pub gcp: bool,
    pub azure: bool,
}

impl Offer {
    pub fn any(self) -> bool {
        self.aws || self.gcp || self.azure
    }
}

/// The auth select and each method's fields, to go before the username;
/// the password field gets `when(AUTH, [PASSWORD])` from `adapt`.
pub fn fields(offer: Offer) -> Vec<Field> {
    let mut options = vec![(PASSWORD, "Usuario y contraseña")];
    let mut out = vec![];
    if offer.aws {
        options.push((AWS, "AWS IAM"));
        out.push(
            Field::new("aws_profile", "Perfil de AWS", FieldKind::Text)
                .placeholder("default")
                .help(
                    "Opcional: un perfil de ~/.aws/config (también con SSO o credential_process). Vacío: las \
                     variables AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY, AWS_PROFILE o el perfil default.",
                )
                .when(AUTH, &[AWS]),
        );
        out.push(
            Field::new("aws_region", "Región de AWS", FieldKind::Text)
                .placeholder("us-east-1")
                .help("Vacía: se toma del endpoint (….us-east-1.rds.amazonaws.com), de AWS_REGION o del perfil.")
                .when(AUTH, &[AWS]),
        );
    }
    if offer.gcp {
        options.push((GCP, "Google Cloud IAM"));
        out.push(
            Field::new("gcp_account", "Cuenta de Google Cloud", FieldKind::Text)
                .placeholder("ana@empresa.com")
                .help(
                    "Opcional: la cuenta de gcloud que pide el token. Vacía: la activa en gcloud (gcloud auth \
                     login), las credenciales predeterminadas de la aplicación o la cuenta de servicio de la máquina.",
                )
                .when(AUTH, &[GCP]),
        );
    }
    if offer.azure {
        options.push((AZURE, "Microsoft Entra ID"));
        out.push(
            Field::new("azure_tenant", "Inquilino de Entra ID (tenant)", FieldKind::Text)
                .placeholder("00000000-0000-0000-0000-000000000000 o contoso.onmicrosoft.com")
                .help("Opcional: sin él vale el inquilino de la cuenta de az login.")
                .when(AUTH, &[AZURE]),
        );
    }
    let select = Field::new(AUTH, "Autenticación", FieldKind::Select(options)).default_value(PASSWORD).help(
        "Con AWS IAM, Google Cloud IAM o Entra ID, DBine pide un token nuevo antes de cada conexión con las \
         credenciales de esta máquina (aws, gcloud o az): no hace falta pegar tokens que vencen. Exigen TLS con el \
         certificado del servidor verificado. El usuario es el de la base (el usuario IAM, la cuenta o el grupo).",
    );
    let mut all = vec![select];
    all.extend(out);
    all
}

/// The form with the auth fields: they go right before the username, and
/// the password only shows for the password sign-in.
pub fn adapt(fields: &mut Vec<Field>, offer: Offer) {
    if !offer.any() {
        return;
    }
    let at = fields.iter().position(|f| f.key == "username").unwrap_or(fields.len());
    for (i, f) in self::fields(offer).into_iter().enumerate() {
        fields.insert(at + i, f);
    }
    for f in fields.iter_mut().filter(|f| f.key == "password") {
        *f = f.clone().when(AUTH, &[PASSWORD]);
    }
}

// ----------------------------------------------------------------- login

/// What the driver tells about itself.
#[derive(Debug, Clone, Copy)]
pub struct Spec {
    pub default_port: u16,
    /// AlloyDB takes an OAuth access token; Cloud SQL, its login token.
    pub gcp_access_token: bool,
    /// MySQL users of a Google account are the part before the `@`.
    pub mysql: bool,
}

/// A cloud sign-in's outcome: the token that goes as the password, the
/// database user when the form left it empty and it can be told from the
/// identity (Google and Entra ID), and who signed in (only for "Probar").
#[derive(Clone)]
pub struct Login {
    pub password: String,
    pub user: Option<String>,
    pub identity: Option<String>,
}

// The token never reaches a log, not even through `{:?}`.
impl std::fmt::Debug for Login {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Login").field("user", &self.user).field("identity", &self.identity).finish_non_exhaustive()
    }
}

/// The cloud sign-in the connection asks for, if any.
pub fn method(cfg: &ConnectionConfig) -> Option<&'static str> {
    match cfg.option(AUTH)? {
        AWS => Some(AWS),
        GCP => Some(GCP),
        AZURE => Some(AZURE),
        _ => None,
    }
}

/// The server the token is for: the tunnel's far end, or the form's.
pub fn endpoint(cfg: &ConnectionConfig, default_port: u16) -> (String, u16) {
    if let Some(h) = cfg.option(ORIGIN_HOST) {
        let port = cfg.option(ORIGIN_PORT).and_then(|p| p.trim().parse().ok()).unwrap_or(default_port);
        return (h.trim().to_string(), port);
    }
    let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
    (host.to_string(), cfg.port_or(default_port))
}

/// The session's version line, with who signed in when "Probar" asked.
pub fn with_identity(version: String, identity: Option<&str>) -> String {
    match identity {
        Some(who) => format!("{version} — identidad: {who}"),
        None => version,
    }
}

/// The token for a cloud sign-in, or `None` for the password one.
pub async fn login(cfg: &ConnectionConfig, spec: Spec) -> Result<Option<Login>> {
    let Some(m) = method(cfg) else { return Ok(None) };
    if cfg.trust_server_certificate {
        return Err(auth(
            "el inicio de sesión con la nube manda un token que no puede ir a un servidor sin verificar: desmarcá \
             «Confiar en el certificado del servidor» e indicá la CA si hace falta",
        ));
    }
    let describe = cfg.option(DESCRIBE).is_some();
    let user_given = cfg.username.as_deref().is_some_and(|u| !u.trim().is_empty());
    match m {
        AWS => {
            if !user_given {
                return Err(auth("falta el usuario de la base (el que tiene el permiso rds_iam / AWSAuthenticationPlugin)"));
            }
            aws_login(cfg, spec, describe).await.map(Some)
        }
        GCP => gcp_login(cfg, spec, describe || !user_given).await.map(Some),
        _ => azure_login(cfg, describe || !user_given).await.map(Some),
    }
}

// ------------------------------------------------------- token caching

static TOKENS: LazyLock<Mutex<HashMap<String, (String, u64)>>> = LazyLock::new(Mutex::default);
/// One CLI run at a time: sessions that open together (explorer, editor)
/// wait for the first one and take its token.
static ACQUIRE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn cached(key: &str, now: u64) -> Option<String> {
    let map = TOKENS.lock().unwrap_or_else(|e| e.into_inner());
    map.get(key).filter(|(_, exp)| now + MARGIN < *exp).map(|(t, _)| t.clone())
}

fn remember(key: String, token: &str, expires_on: u64) {
    TOKENS.lock().unwrap_or_else(|e| e.into_inner()).insert(key, (token.to_string(), expires_on));
}

// ------------------------------------------------------------- AWS IAM

#[derive(Clone, PartialEq)]
pub struct AwsCreds {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    /// Unix seconds; `None` for long-term keys.
    pub expires_on: Option<u64>,
}

impl std::fmt::Debug for AwsCreds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AwsCreds").field("expires_on", &self.expires_on).finish_non_exhaustive()
    }
}

static AWS_CREDS: LazyLock<Mutex<HashMap<String, AwsCreds>>> = LazyLock::new(Mutex::default);

async fn aws_login(cfg: &ConnectionConfig, spec: Spec, describe: bool) -> Result<Login> {
    let (host, port) = endpoint(cfg, spec.default_port);
    let profile_field = cfg.option("aws_profile").map(str::trim).filter(|p| !p.is_empty());
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let files = AwsFiles::load(&env);
    let profile = profile_field.map(str::to_string).or_else(|| env("AWS_PROFILE")).unwrap_or_else(|| "default".into());
    let region = cfg
        .option("aws_region")
        .map(|r| r.trim().to_string())
        .or_else(|| region_from_host(&host))
        .or_else(|| env("AWS_REGION"))
        .or_else(|| env("AWS_DEFAULT_REGION"))
        .or_else(|| files.get(&profile, "region"))
        .ok_or_else(|| auth("falta la región de AWS (no se pudo deducir del endpoint): indicala en el formulario"))?;
    if !valid_region(&region) {
        return Err(auth(format!("la región de AWS «{region}» no es válida")));
    }
    let (creds, source) = aws_credentials(profile_field.is_some(), &profile, &files, &env).await?;
    let user = cfg.username.as_deref().unwrap_or("").trim().to_string();
    let token = rds_token(&host, port, &user, &region, &creds, now());
    let identity = if describe { Some(aws_identity(&creds, &region, &source).await) } else { None };
    Ok(Login { password: token, user: None, identity })
}

/// `<name>.<id>.<region>.rds.amazonaws.com` (also `cluster-`, `proxy-`
/// and the `.com.cn` endpoints) → `<region>`.
pub fn region_from_host(host: &str) -> Option<String> {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let labels: Vec<&str> = host.split('.').collect();
    let i = labels.windows(2).position(|w| w[0] == "rds" && w[1] == "amazonaws")?;
    let region = labels.get(i.checked_sub(1)?)?;
    valid_region(region).then(|| region.to_string())
}

/// `us-east-1`, `eu-central-2`, `us-gov-west-1`, `cn-north-1`…
fn valid_region(r: &str) -> bool {
    let parts: Vec<&str> = r.split('-').collect();
    parts.len() >= 3
        && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()))
        && parts.last().is_some_and(|p| p.chars().all(|c| c.is_ascii_digit()))
}

/// Where the credentials came from, for "Probar".
#[derive(Debug, Clone, PartialEq)]
pub enum AwsSource {
    Env,
    Profile(String),
}

/// The standard chain: environment keys (unless the form names a
/// profile), the profile's static keys, its `credential_process`, and
/// whatever else the AWS CLI resolves (SSO, assumed roles, the instance or
/// container role).
pub async fn aws_credentials(
    profile_named: bool,
    profile: &str,
    files: &AwsFiles,
    env: &impl Fn(&str) -> Option<String>,
) -> Result<(AwsCreds, AwsSource)> {
    if !profile_named {
        if let (Some(ak), Some(sk)) = (env("AWS_ACCESS_KEY_ID"), env("AWS_SECRET_ACCESS_KEY")) {
            let creds = AwsCreds { access_key_id: ak, secret_access_key: sk, session_token: env("AWS_SESSION_TOKEN"), expires_on: None };
            return Ok((creds, AwsSource::Env));
        }
    }
    let source = AwsSource::Profile(profile.to_string());
    if let Some(c) = files.static_keys(profile) {
        return Ok((c, source));
    }
    let _one = ACQUIRE.lock().await;
    let key = format!("aws:{profile}");
    if let Some(c) = AWS_CREDS.lock().unwrap_or_else(|e| e.into_inner()).get(&key).filter(|c| c.expires_on.is_none_or(|e| now() + MARGIN < e)) {
        return Ok((c.clone(), source));
    }
    let creds = if let Some(command) = files.get(profile, "credential_process") {
        credential_process(&command).await?
    } else if files.has_profile(profile) || !profile_named {
        // SSO, role_arn, web identity… or no profile at all (the instance
        // or container role): the AWS CLI knows them all.
        let mut args = vec!["configure", "export-credentials", "--format", "process"];
        if files.has_profile(profile) || profile_named {
            args.extend(["--profile", profile]);
        }
        let out = run_cli("aws", &args, &[("AWS_PAGER", "")], &[], "aws sso login o aws login").await.map_err(|e| {
            auth(format!("no se pudieron obtener credenciales de AWS para el perfil «{profile}»: {e}"))
        })?;
        parse_process_credentials(&out)?
    } else {
        return Err(auth(format!("no existe el perfil de AWS «{profile}» en ~/.aws/config ni en ~/.aws/credentials")));
    };
    // Long-term keys from a process are re-read now and then anyway.
    let mut keep = creds.clone();
    keep.expires_on.get_or_insert(now() + MARGIN + UNKNOWN_TTL);
    AWS_CREDS.lock().unwrap_or_else(|e| e.into_inner()).insert(key, keep);
    Ok((creds, source))
}

/// `credential_process`: the profile's command, through the shell as the
/// AWS SDKs do; it prints the credentials as JSON (Version 1).
async fn credential_process(command: &str) -> Result<AwsCreds> {
    if sandboxed() {
        return Err(auth(SANDBOX_AWS));
    }
    let mut cmd = if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let mut c = tokio::process::Command::new(root.join("System32").join("cmd.exe"));
        c.args(["/C", command]);
        c
    } else {
        let mut c = tokio::process::Command::new("/bin/sh");
        c.args(["-c", command]);
        c
    };
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
    let out = match tokio::time::timeout(CLI_WAIT, cmd.output()).await {
        Ok(out) => out.map_err(|e| auth(format!("credential_process no se pudo ejecutar: {e}")))?,
        Err(_) => return Err(auth(format!("credential_process no respondió en {} s", CLI_WAIT.as_secs()))),
    };
    if !out.status.success() {
        let err = short(&String::from_utf8_lossy(&out.stderr), 200);
        return Err(auth(format!("credential_process falló: {err}")));
    }
    parse_process_credentials(&String::from_utf8_lossy(&out.stdout))
}

/// The `credential_process` / `export-credentials --format process` JSON.
fn parse_process_credentials(stdout: &str) -> Result<AwsCreds> {
    let json: Value = serde_json::from_str(stdout.trim()).map_err(|_| auth("las credenciales de AWS no vinieron en JSON"))?;
    let get = |k: &str| json.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    let (Some(ak), Some(sk)) = (get("AccessKeyId"), get("SecretAccessKey")) else {
        return Err(auth("las credenciales de AWS no traen AccessKeyId y SecretAccessKey"));
    };
    let expires_on = match get("Expiration") {
        Some(e) => Some(parse_rfc3339(&e).ok_or_else(|| auth("las credenciales de AWS traen una fecha de vencimiento no válida"))?),
        None => None,
    };
    Ok(AwsCreds { access_key_id: ak, secret_access_key: sk, session_token: get("SessionToken"), expires_on })
}

/// "Probar": the ARN, from `aws sts get-caller-identity` with exactly
/// these credentials; without the CLI, the access key (masked).
async fn aws_identity(c: &AwsCreds, region: &str, source: &AwsSource) -> String {
    let mut envs = vec![
        ("AWS_ACCESS_KEY_ID", c.access_key_id.as_str()),
        ("AWS_SECRET_ACCESS_KEY", c.secret_access_key.as_str()),
        ("AWS_REGION", region),
        ("AWS_PAGER", ""),
    ];
    if let Some(t) = &c.session_token {
        envs.push(("AWS_SESSION_TOKEN", t.as_str()));
    }
    let remove = if c.session_token.is_none() { vec!["AWS_PROFILE", "AWS_DEFAULT_PROFILE", "AWS_SESSION_TOKEN"] } else { vec!["AWS_PROFILE", "AWS_DEFAULT_PROFILE"] };
    let args = ["sts", "get-caller-identity", "--output", "json"];
    let arn = match run_cli("aws", &args, &envs, &remove, "aws sso login o aws login").await {
        Ok(out) => serde_json::from_str::<Value>(&out).ok().and_then(|j| j.get("Arn")?.as_str().map(str::to_string)),
        Err(_) => None,
    };
    let who = match arn {
        Some(arn) => format!("AWS IAM: {arn}"),
        None => format!("AWS IAM: clave {}", mask(&c.access_key_id)),
    };
    match source {
        AwsSource::Profile(p) => format!("{who} (perfil {p})"),
        AwsSource::Env => format!("{who} (variables de entorno)"),
    }
}

/// `AKIA…WXYZ`.
fn mask(key: &str) -> String {
    if key.len() <= 8 || !key.is_ascii() {
        return "…".into();
    }
    format!("{}…{}", &key[..4], &key[key.len() - 4..])
}

/// `~/.aws/config` and `~/.aws/credentials` (or `AWS_CONFIG_FILE` and
/// `AWS_SHARED_CREDENTIALS_FILE`), by profile.
#[derive(Debug, Default)]
pub struct AwsFiles {
    config: HashMap<String, HashMap<String, String>>,
    credentials: HashMap<String, HashMap<String, String>>,
}

impl AwsFiles {
    pub fn load(env: &impl Fn(&str) -> Option<String>) -> AwsFiles {
        let home = env("HOME").or_else(|| env("USERPROFILE")).map(PathBuf::from);
        let path = |var: &str, file: &str| env(var).map(PathBuf::from).or_else(|| home.as_ref().map(|h| h.join(".aws").join(file)));
        let read = |p: Option<PathBuf>| p.and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        AwsFiles::parse(&read(path("AWS_CONFIG_FILE", "config")), &read(path("AWS_SHARED_CREDENTIALS_FILE", "credentials")))
    }

    fn parse(config: &str, credentials: &str) -> AwsFiles {
        AwsFiles { config: ini(config, true), credentials: ini(credentials, false) }
    }

    fn has_profile(&self, p: &str) -> bool {
        self.config.contains_key(p) || self.credentials.contains_key(p)
    }

    /// A profile's setting; the credentials file wins over the config.
    pub fn get(&self, p: &str, key: &str) -> Option<String> {
        let from = |m: &HashMap<String, HashMap<String, String>>| m.get(p)?.get(key).filter(|v| !v.is_empty()).cloned();
        from(&self.credentials).or_else(|| from(&self.config))
    }

    fn static_keys(&self, p: &str) -> Option<AwsCreds> {
        Some(AwsCreds {
            access_key_id: self.get(p, "aws_access_key_id")?,
            secret_access_key: self.get(p, "aws_secret_access_key")?,
            session_token: self.get(p, "aws_session_token"),
            expires_on: None,
        })
    }
}

/// An AWS INI file: `[default]`, `[profile name]` (config) or `[name]`
/// (credentials); `key = value`; `#` / `;` comments; indented lines are
/// sub-settings (`s3 =` blocks) and are skipped.
fn ini(text: &str, config: bool) -> HashMap<String, HashMap<String, String>> {
    let mut out: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut current: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let name = name.trim();
            current = if !config || name == "default" {
                Some(name.to_string())
            } else {
                name.strip_prefix("profile").filter(|r| r.starts_with(char::is_whitespace)).map(|r| r.trim().to_string())
            };
            if let Some(c) = &current {
                out.entry(c.clone()).or_default();
            }
            continue;
        }
        if raw.starts_with(char::is_whitespace) {
            continue;
        }
        let (Some(section), Some((k, v))) = (&current, line.split_once('=')) else { continue };
        out.entry(section.clone()).or_default().insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
    }
    out
}

// ----------------------------------------------------------- SigV4 / RDS

pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    let digest = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// HMAC-SHA256 (RFC 2104).
pub fn hmac(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&sha256(&[key]));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let (mut ipad, mut opad) = ([0x36u8; 64], [0x5cu8; 64]);
    for i in 0..64 {
        ipad[i] ^= k[i];
        opad[i] ^= k[i];
    }
    let inner = sha256(&[&ipad, msg]);
    sha256(&[&opad, &inner])
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// SigV4's URI encoding: everything but `A-Z a-z 0-9 - _ . ~`.
pub fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `20150830T123600Z` and `20150830` for Unix seconds (UTC).
pub fn amz_date(unix: u64) -> (String, String) {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    let (y, m, d) = civil_from_days(days);
    let date = format!("{y:04}{m:02}{d:02}");
    (format!("{date}T{:02}{:02}{:02}Z", secs / 3600, secs / 60 % 60, secs % 60), date)
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `2026-10-18T12:34:56Z`, with fractions and `±hh:mm` offsets too.
fn parse_rfc3339(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date, rest) = s.split_once(['T', 't', ' '])?;
    let mut d = date.split('-');
    let (y, mo, da): (i64, u32, u32) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
    let (time, offset) = match rest.find(['Z', 'z', '+', '-']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "Z"),
    };
    let mut t = time.split(':');
    let (h, mi): (i64, i64) = (t.next()?.parse().ok()?, t.next()?.parse().ok()?);
    let sec: i64 = t.next().map(|s| s.split('.').next().unwrap_or("0")).unwrap_or("0").parse().ok()?;
    let off = match offset.as_bytes().first() {
        Some(b'+' | b'-') => {
            let (oh, om) = offset[1..].split_once(':').unwrap_or((&offset[1..], "0"));
            let v = oh.parse::<i64>().ok()? * 3600 + om.parse::<i64>().ok()? * 60;
            if offset.starts_with('-') { -v } else { v }
        }
        _ => 0,
    };
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) {
        return None;
    }
    let unix = days_from_civil(y, mo, da) * 86_400 + h * 3600 + mi * 60 + sec - off;
    u64::try_from(unix).ok()
}

/// A SigV4 query-string signature (presigned GET of `/`, only `host`
/// signed, no body): returns the canonical query plus `X-Amz-Signature`.
fn presign(host: &str, params: &[(&str, &str)], c: &AwsCreds, region: &str, service: &str, unix: u64, expires: u64) -> String {
    let (stamp, date) = amz_date(unix);
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let credential = format!("{}/{scope}", c.access_key_id);
    let expires = expires.to_string();
    let mut q: Vec<(String, String)> = params.iter().map(|(k, v)| (uri_encode(k), uri_encode(v))).collect();
    q.push(("X-Amz-Algorithm".into(), "AWS4-HMAC-SHA256".into()));
    q.push(("X-Amz-Credential".into(), uri_encode(&credential)));
    q.push(("X-Amz-Date".into(), stamp.clone()));
    q.push(("X-Amz-Expires".into(), expires));
    if let Some(t) = &c.session_token {
        q.push(("X-Amz-Security-Token".into(), uri_encode(t)));
    }
    q.push(("X-Amz-SignedHeaders".into(), "host".into()));
    q.sort();
    let query = q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
    let empty = hex(&sha256(&[b""]));
    let canonical = format!("GET\n/\n{query}\nhost:{host}\n\nhost\n{empty}");
    let to_sign = format!("AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}", hex(&sha256(&[canonical.as_bytes()])));
    let mut key = hmac(format!("AWS4{}", c.secret_access_key).as_bytes(), date.as_bytes());
    for part in [region, service, "aws4_request"] {
        key = hmac(&key, part.as_bytes());
    }
    format!("{query}&X-Amz-Signature={}", hex(&hmac(&key, to_sign.as_bytes())))
}

/// The RDS IAM auth token, as `aws rds generate-db-auth-token` makes it:
/// `host:port/?Action=connect&DBUser=…&X-Amz-…&X-Amz-Signature=…`.
fn rds_token(host: &str, port: u16, user: &str, region: &str, c: &AwsCreds, unix: u64) -> String {
    let host = format!("{}:{port}", host.trim().trim_end_matches('.').to_ascii_lowercase());
    let query = presign(&host, &[("Action", "connect"), ("DBUser", user)], c, region, "rds-db", unix, RDS_TOKEN_TTL);
    format!("{host}/?{query}")
}

// --------------------------------------------------------- Google Cloud

async fn gcp_login(cfg: &ConnectionConfig, spec: Spec, want_email: bool) -> Result<Login> {
    let account = cfg.option("gcp_account").map(str::trim).filter(|a| !a.is_empty());
    if let Some(a) = account {
        if !safe_arg(a) {
            return Err(auth(format!("la cuenta de Google Cloud «{a}» no es válida")));
        }
    }
    let _one = ACQUIRE.lock().await;
    let key = format!("gcp:{}:{}", account.unwrap_or(""), spec.gcp_access_token);
    let email_key = format!("{key}:email");
    let (token, email) = match cached(&key, now()) {
        Some(t) => (t, cached(&email_key, now())),
        None => {
            let (token, expires_on, email) = gcp_token(account, spec.gcp_access_token).await?;
            remember(key.clone(), &token, expires_on);
            if let Some(e) = &email {
                remember(email_key.clone(), e, expires_on);
            }
            (token, email)
        }
    };
    let email = match (email, want_email) {
        (Some(e), _) => Some(e),
        (None, true) => {
            let e = account.map(str::to_string).or(gcloud_account().await);
            if let Some(e) = &e {
                remember(email_key, e, now() + MARGIN + UNKNOWN_TTL);
            }
            e
        }
        (None, false) => None,
    };
    let user = email.as_deref().map(|e| gcp_db_user(e, spec.mysql));
    let identity = email.map(|e| format!("Google Cloud: {e}")).or_else(|| Some("Google Cloud".into()));
    Ok(Login { password: token, user, identity: identity.filter(|_| cfg.option(DESCRIBE).is_some()) })
}

/// The database user of a Google identity: the email for PostgreSQL (a
/// service account without `.gserviceaccount.com`), the part before the
/// `@` for MySQL.
fn gcp_db_user(email: &str, mysql: bool) -> String {
    if mysql {
        return email.split('@').next().unwrap_or(email).to_string();
    }
    email.strip_suffix(".gserviceaccount.com").unwrap_or(email).to_string()
}

/// gcloud with the account (or the active one), its application default
/// credentials, then the metadata server: the token, when it expires and,
/// from the metadata server, the service account's email.
pub async fn gcp_token(account: Option<&str>, access_token: bool) -> Result<(String, u64, Option<String>)> {
    let mut tried = vec![];
    let mut args: Vec<&str> = if access_token { vec!["auth", "print-access-token"] } else { vec!["sql", "generate-login-token"] };
    if let Some(a) = account {
        args.extend(["--account", a]);
    }
    args.push("--quiet");
    match run_cli("gcloud", &args, &[], &[], "gcloud auth login").await.and_then(|o| bare_token(&o)) {
        Ok(t) => return Ok((t, now() + UNKNOWN_TTL, None)),
        Err(e) => tried.push(("gcloud", e)),
    }
    if account.is_none() {
        let args = ["auth", "application-default", "print-access-token", "--quiet"];
        match run_cli("gcloud", &args, &[], &[], "gcloud auth application-default login").await.and_then(|o| bare_token(&o)) {
            Ok(t) => return Ok((t, now() + UNKNOWN_TTL, None)),
            Err(e) => tried.push(("credenciales predeterminadas de la aplicación", e)),
        }
        match gce_metadata().await {
            Ok(t) => return Ok(t),
            Err(e) => tried.push(("servidor de metadatos", e)),
        }
    }
    Err(chain_error("Google Cloud", &tried))
}

/// A CLI that prints only the token.
fn bare_token(stdout: &str) -> Result<String> {
    let t = stdout.trim();
    if t.is_empty() || t.contains(char::is_whitespace) {
        return Err(auth("no devolvió un token"));
    }
    Ok(t.to_string())
}

async fn gcloud_account() -> Option<String> {
    let out = run_cli("gcloud", &["config", "get-value", "account", "--quiet"], &[], &[], "gcloud auth login").await.ok()?;
    let a = out.trim();
    (!a.is_empty() && a.contains('@')).then(|| a.to_string())
}

const METADATA: &str = "169.254.169.254:80";

async fn gce_metadata() -> Result<(String, u64, Option<String>)> {
    let base = "/computeMetadata/v1/instance/service-accounts/default";
    let headers = [("Metadata-Flavor", "Google")];
    let (status, body) = http_get(METADATA, &format!("{base}/token"), &headers).await?;
    if status != 200 {
        return Err(auth(format!("respondió {status}")));
    }
    let json: Value = serde_json::from_str(&body).map_err(|_| auth("no devolvió JSON"))?;
    let token = json.get("access_token").and_then(Value::as_str).ok_or_else(|| auth("no devolvió un token"))?;
    let expires_in = number(json.get("expires_in")).unwrap_or(UNKNOWN_TTL);
    let email = match http_get(METADATA, &format!("{base}/email"), &headers).await {
        Ok((200, e)) if e.contains('@') => Some(e.trim().to_string()),
        _ => None,
    };
    Ok((token.to_string(), now() + expires_in, email))
}

// ------------------------------------------------- Microsoft Entra ID

/// The resource of Azure Database for PostgreSQL and MySQL.
const OSS_RDBMS: &str = "https://ossrdbms-aad.database.windows.net";

async fn azure_login(cfg: &ConnectionConfig, want_user: bool) -> Result<Login> {
    let tenant = cfg.option("azure_tenant").map(str::trim).filter(|t| !t.is_empty());
    if let Some(t) = tenant {
        if !safe_arg(t) || t.contains('@') {
            return Err(auth(format!("el inquilino «{t}» no es válido")));
        }
    }
    let _one = ACQUIRE.lock().await;
    let key = format!("azure:{}", tenant.unwrap_or(""));
    let token = match cached(&key, now()) {
        Some(t) => t,
        None => {
            let (t, expires_on) = azure_token(tenant).await?;
            remember(key, &t, expires_on);
            t
        }
    };
    let claims = jwt_claims(&token);
    let name = claims.as_ref().and_then(|c| {
        ["upn", "preferred_username", "unique_name"].iter().find_map(|k| c.get(*k)?.as_str().filter(|v| !v.is_empty()).map(str::to_string))
    });
    let identity = match (&name, claims.as_ref().and_then(|c| c.get("appid").or(c.get("oid"))?.as_str().map(str::to_string))) {
        (Some(n), _) => format!("Microsoft Entra ID: {n}"),
        (None, Some(id)) => format!("Microsoft Entra ID: identidad {id}"),
        _ => "Microsoft Entra ID".to_string(),
    };
    let user = name.filter(|_| want_user);
    Ok(Login { password: token, user, identity: cfg.option(DESCRIBE).is_some().then_some(identity) })
}

/// Azure CLI, Azure Developer CLI, then the managed identity.
async fn azure_token(tenant: Option<&str>) -> Result<(String, u64)> {
    azure_token_for(OSS_RDBMS, tenant).await
}

/// A token for `resource` (Azure Database's, Key Vault's…): Azure CLI,
/// Azure Developer CLI, then the managed identity.
pub async fn azure_token_for(resource: &str, tenant: Option<&str>) -> Result<(String, u64)> {
    let mut tried = vec![];
    let mut args = vec!["account", "get-access-token"];
    if resource == OSS_RDBMS {
        args.extend(["--resource-type", "oss-rdbms"]);
    } else {
        args.extend(["--resource", resource]);
    }
    args.extend(["--output", "json"]);
    if let Some(t) = tenant {
        args.extend(["--tenant", t]);
    }
    match run_cli("az", &args, &[], &[], "az login").await.and_then(|o| parse_az(&o)) {
        Ok(t) => return Ok(t),
        Err(e) => tried.push(("Azure CLI", e)),
    }
    let scope = format!("{resource}/.default");
    let mut args = vec!["auth", "token", "--scope", scope.as_str(), "--output", "json"];
    if let Some(t) = tenant {
        args.extend(["--tenant-id", t]);
    }
    match run_cli("azd", &args, &[], &[], "azd auth login").await.and_then(|o| parse_azd(&o)) {
        Ok(t) => return Ok(t),
        Err(e) => tried.push(("Azure Developer CLI", e)),
    }
    match azure_msi(resource).await {
        Ok(t) => return Ok(t),
        Err(e) => tried.push(("identidad administrada", e)),
    }
    Err(chain_error("Microsoft Entra ID", &tried))
}

/// Azure CLI: `accessToken` plus `expires_on` (Unix seconds). Older
/// versions only say `expiresOn` in local time: then the token is kept a
/// short while.
fn parse_az(stdout: &str) -> Result<(String, u64)> {
    let json: Value = serde_json::from_str(stdout).map_err(|_| auth("devolvió algo que no es JSON"))?;
    let access = json.get("accessToken").and_then(Value::as_str).ok_or_else(|| auth("no devolvió un token"))?;
    let expires_on = number(json.get("expires_on")).unwrap_or(now() + MARGIN + UNKNOWN_TTL);
    Ok((access.to_string(), expires_on))
}

/// Azure Developer CLI: `token` plus `expiresOn` (RFC 3339).
fn parse_azd(stdout: &str) -> Result<(String, u64)> {
    let json: Value = serde_json::from_str(stdout).map_err(|_| auth("devolvió algo que no es JSON"))?;
    let access = json.get("token").and_then(Value::as_str).ok_or_else(|| auth("no devolvió un token"))?;
    let expires_on = json.get("expiresOn").and_then(Value::as_str).and_then(parse_rfc3339).unwrap_or(now() + MARGIN + UNKNOWN_TTL);
    Ok((access.to_string(), expires_on))
}

async fn azure_msi(resource: &str) -> Result<(String, u64)> {
    let path = format!("/metadata/identity/oauth2/token?api-version=2018-02-01&resource={}", uri_encode(resource));
    let (status, body) = http_get(METADATA, &path, &[("Metadata", "true")]).await?;
    let json: Value = serde_json::from_str(&body).map_err(|_| auth(format!("respondió {status}")))?;
    if status != 200 {
        let why = json.get("error_description").and_then(Value::as_str).unwrap_or("");
        return Err(auth(format!("respondió {status}: {}", short(why, 200))));
    }
    let access = json.get("access_token").and_then(Value::as_str).ok_or_else(|| auth("no devolvió un token"))?;
    let expires_on = number(json.get("expires_on")).unwrap_or(now() + MARGIN + UNKNOWN_TTL);
    Ok((access.to_string(), expires_on))
}

/// A JWT's payload (unverified: only to say who signed in).
fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&base64url(payload)?).ok()
}

fn base64url(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.trim_end_matches('=').bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// A number that may come as JSON number or string (`"3599"`).
fn number(v: Option<&Value>) -> Option<u64> {
    let v = v?;
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// One error that says what was tried and why each step failed.
pub fn chain_error(cloud: &str, tried: &[(&str, Error)]) -> Error {
    let lines: Vec<String> = tried.iter().map(|(what, e)| format!("- {what}: {}", short(&e.to_string(), 200))).collect();
    auth(format!("no se pudo obtener un token de {cloud}. Se probó:\n{}", lines.join("\n")))
}

// ------------------------------------------------------------ plain HTTP

/// A GET to a link-local metadata endpoint (plain HTTP, as the clouds
/// serve them), with a short connect timeout: off the cloud nothing
/// answers there.
async fn http_get(addr: &str, path: &str, headers: &[(&str, &str)]) -> Result<(u16, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::time::timeout(Duration::from_secs(1), tokio::net::TcpStream::connect(addr))
        .await
        .map_err(|_| auth("no responde (DBine no corre en la nube)"))?
        .map_err(|e| auth(format!("no responde: {e}")))?;
    let host = addr.split(':').next().unwrap_or(addr);
    let mut req = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    let mut buf = Vec::new();
    let io = async {
        s.write_all(req.as_bytes()).await?;
        (&mut s).take(1 << 20).read_to_end(&mut buf).await
    };
    tokio::time::timeout(Duration::from_secs(5), io)
        .await
        .map_err(|_| auth("no respondió a tiempo"))?
        .map_err(|e| auth(format!("falló la lectura: {e}")))?;
    parse_http(&buf).ok_or_else(|| auth("respuesta HTTP no válida"))
}

fn parse_http(buf: &[u8]) -> Option<(u16, String)> {
    let text = String::from_utf8_lossy(buf);
    let (head, body) = text.split_once("\r\n\r\n")?;
    let status = head.lines().next()?.split_whitespace().nth(1)?.parse().ok()?;
    Some((status, body.to_string()))
}

// ------------------------------------------------------------------ CLIs

/// The Flathub build: the sandbox has no aws, gcloud or az and can't run
/// the system's (`/.flatpak-info` exists in every Flatpak sandbox).
pub fn sandboxed() -> bool {
    cfg!(target_os = "linux") && Path::new("/.flatpak-info").exists()
}

const SANDBOX_AWS: &str = "la versión de Flathub de DBine no puede usar la CLI de AWS ni credential_process: el sandbox de Flatpak no tiene acceso a los programas del sistema. Usá claves en AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY o en ~/.aws/credentials, o la versión de DBine de dbine.com";

pub fn sandbox_message(name: &str) -> String {
    format!(
        "la versión de Flathub de DBine no puede usar «{name}»: el sandbox de Flatpak no tiene acceso a los programas del sistema. Usá la versión de DBine de dbine.com, o un token como contraseña"
    )
}

/// Where a CLI may be: `PATH`, then the usual install folders (apps opened
/// from the Finder don't get the shell's `PATH`).
fn cli_candidates(name: &str, path: Option<&std::ffi::OsStr>, home: Option<&Path>) -> Vec<PathBuf> {
    let (files, mut fixed): (Vec<String>, Vec<PathBuf>) = if cfg!(windows) {
        (
            vec![format!("{name}.cmd"), format!("{name}.exe")],
            [
                r"C:\Program Files\Microsoft SDKs\Azure\CLI2\wbin",
                r"C:\Program Files (x86)\Microsoft SDKs\Azure\CLI2\wbin",
                r"C:\Program Files\Azure Dev CLI",
                r"C:\Program Files\Amazon\AWSCLIV2",
                r"C:\Program Files (x86)\Google\Cloud SDK\google-cloud-sdk\bin",
                r"C:\Program Files\Google\Cloud SDK\google-cloud-sdk\bin",
            ]
            .iter()
            .map(PathBuf::from)
            .collect(),
        )
    } else {
        (
            vec![name.to_string()],
            ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/opt/homebrew/share/google-cloud-sdk/bin", "/usr/lib/google-cloud-sdk/bin", "/snap/bin"]
                .iter()
                .map(PathBuf::from)
                .collect(),
        )
    };
    if let Some(h) = home {
        if cfg!(windows) {
            fixed.push(h.join(r"AppData\Local\Google\Cloud SDK\google-cloud-sdk\bin"));
        } else {
            fixed.push(h.join("google-cloud-sdk/bin"));
        }
    }
    let dirs = path.map(|p| std::env::split_paths(p).collect::<Vec<_>>()).unwrap_or_default();
    let dirs = dirs.into_iter().filter(|d| d.is_absolute()).chain(fixed);
    dirs.flat_map(|d| files.iter().map(move |f| d.join(f)).collect::<Vec<_>>()).collect()
}

pub fn find_cli(name: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from);
    cli_candidates(name, std::env::var_os("PATH").as_deref(), home.as_deref()).into_iter().find(|p| p.is_file())
}

/// An argument that's safe anywhere, also on a `cmd.exe` command line.
fn safe_arg(a: &str) -> bool {
    !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ':' | '/' | '@' | '+'))
}

/// The `cmd.exe` command line that runs a `.cmd` script: `/d` skips
/// AutoRun, `/v:off` keeps `!` literal, `/s /c "…"` strips only the outer
/// quotes. Refuses anything `cmd` would expand.
#[cfg_attr(not(windows), allow(dead_code))]
fn cmd_line(script: &Path, args: &[&str]) -> Result<String> {
    let script = script.to_str().ok_or_else(|| auth("ruta del programa no válida"))?;
    if script.chars().any(|c| matches!(c, '"' | '%' | '!' | '^' | '\r' | '\n')) || !args.iter().all(|a| safe_arg(a)) {
        return Err(auth("ruta o argumentos del programa no válidos"));
    }
    Ok(format!("/d /v:off /s /c \"\"{script}\" {}\"", args.join(" ")))
}

#[cfg(windows)]
fn cli_command(program: &Path, args: &[&str]) -> Result<tokio::process::Command> {
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let batch = program.extension().is_some_and(|e| e.eq_ignore_ascii_case("cmd") || e.eq_ignore_ascii_case("bat"));
    let mut cmd = if batch {
        let line = cmd_line(program, args)?;
        let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
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

/// Runs a CLI (no shell) and returns its standard output. `envs` are set
/// and `remove` taken out of the child's environment only.
pub async fn run_cli(name: &str, args: &[&str], envs: &[(&str, &str)], remove: &[&str], login: &str) -> Result<String> {
    if sandboxed() {
        return Err(auth(sandbox_message(name)));
    }
    let program = find_cli(name).ok_or_else(|| auth(format!("«{name}» no está instalada")))?;
    if !args.iter().all(|a| safe_arg(a)) {
        return Err(auth("argumentos no válidos"));
    }
    let mut cmd = cli_command(&program, args)?;
    for k in remove {
        cmd.env_remove(k);
    }
    cmd.envs(envs.iter().copied());
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
    let out = match tokio::time::timeout(CLI_WAIT, cmd.output()).await {
        Ok(out) => out.map_err(|e| auth(format!("«{name}» no se pudo ejecutar: {e}")))?,
        Err(_) => return Err(auth(format!("«{name}» no respondió en {} s", CLI_WAIT.as_secs()))),
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        let err = err.strip_prefix(&format!("{name}:")).unwrap_or(err).trim();
        let err = err.strip_prefix("[ERROR]:").or_else(|| err.strip_prefix("ERROR:")).unwrap_or(err);
        let line = short(err.trim(), 200);
        let line = if line.is_empty() { format!("terminó con {}", out.status) } else { line };
        return Err(auth(format!("{line} (¿falta {login}?)")));
    }
    String::from_utf8(out.stdout).map_err(|_| auth("salida no válida"))
}

// ------------------------------------------------------------------- TLS

/// The PEM blocks of a bundle, one certificate each (native-tls takes
/// them one at a time).
pub fn pem_certificates(pem: &[u8]) -> Vec<&[u8]> {
    const END: &[u8] = b"-----END CERTIFICATE-----";
    const BEGIN: &[u8] = b"-----BEGIN CERTIFICATE-----";
    let mut out = vec![];
    let mut rest = pem;
    while let Some(start) = find(rest, BEGIN) {
        let Some(end) = find(&rest[start..], END) else { break };
        let stop = start + end + END.len();
        out.push(&rest[start..stop]);
        rest = &rest[stop..];
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod cloud_auth_tests {
    use super::*;

    const AK: &str = "AKIDEXAMPLE";
    const SK: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

    fn creds(token: Option<&str>) -> AwsCreds {
        AwsCreds { access_key_id: AK.into(), secret_access_key: SK.into(), session_token: token.map(String::from), expires_on: None }
    }

    /// 2015-08-30T12:36:00Z, the AWS signing test suite's instant.
    const SUITE_TIME: u64 = 1_440_938_160;

    fn signature(q: &str) -> &str {
        q.rsplit_once("X-Amz-Signature=").unwrap().1
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        // Test case 2.
        assert_eq!(
            hex(&hmac(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Test case 6: a key longer than the block.
        assert_eq!(
            hex(&hmac(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First")),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn dates_are_utc() {
        assert_eq!(amz_date(SUITE_TIME), ("20150830T123600Z".into(), "20150830".into()));
        assert_eq!(amz_date(0).0, "19700101T000000Z");
        assert_eq!(amz_date(951_782_400).1, "20000229");
        assert_eq!(parse_rfc3339("2015-08-30T12:36:00Z"), Some(SUITE_TIME));
        assert_eq!(parse_rfc3339("2015-08-30T14:36:00.123+02:00"), Some(SUITE_TIME));
        assert_eq!(parse_rfc3339("2015-08-30T09:36:00-03:00"), Some(SUITE_TIME));
        assert_eq!(parse_rfc3339("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339("mañana"), None);
    }

    // The AWS signing test suite (aws-sigv4's aws-signing-test-suite/v4,
    // the query-string variants): get-vanilla, get-vanilla-query-order-key-case
    // and get-vanilla-with-session-token.
    #[test]
    fn presign_matches_the_aws_test_suite() {
        let q = presign("example.amazonaws.com", &[], &creds(None), "us-east-1", "service", SUITE_TIME, 3600);
        assert_eq!(signature(&q), "e93c787ed7f371d5c6b165c1b38ede9550f4dce4144713e844b25b7192d3865d");
        assert!(q.starts_with(
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIDEXAMPLE%2F20150830%2Fus-east-1%2Fservice%2Faws4_request&X-Amz-Date=20150830T123600Z&X-Amz-Expires=3600&X-Amz-SignedHeaders=host&"
        ));

        let q = presign("example.amazonaws.com", &[("Param2", "value2"), ("Param1", "value1")], &creds(None), "us-east-1", "service", SUITE_TIME, 3600);
        assert_eq!(signature(&q), "86012e2c9ad4d77369f5d81c11f75158aae4f895a085212cc6d3f923d300bed5");
        assert!(q.starts_with("Param1=value1&Param2=value2&"));

        let token = "6e86291e8372ff2a2260956d9b8aae1d763fbf315fa00fa31553b73ebf194267";
        let q = presign("example.amazonaws.com", &[], &creds(Some(token)), "us-east-1", "service", SUITE_TIME, 3600);
        assert_eq!(signature(&q), "7ff2b50b376cb4d151970630573d6291dc128cc5c2a12ffb237f73cc53f67b6c");
    }

    // Vectors made with the official AWS CLI (aws-cli/2.36.43,
    // `aws rds generate-db-auth-token`) and these example keys; the
    // instant is the one in its X-Amz-Date.
    #[test]
    fn rds_tokens_match_the_aws_cli() {
        let t = parse_rfc3339("2026-10-18T08:31:07Z").unwrap();
        let tok = rds_token(
            "mydb.c1abcdefghij.us-east-1.rds.amazonaws.com",
            5432,
            "db_user",
            "us-east-1",
            &creds(Some("FQoGZXIvYXdzEXAMPLE/token+with=chars")),
            t,
        );
        assert!(tok.starts_with("mydb.c1abcdefghij.us-east-1.rds.amazonaws.com:5432/?Action=connect&DBUser=db_user&X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIDEXAMPLE%2F20261018%2Fus-east-1%2Frds-db%2Faws4_request&X-Amz-Date=20261018T083107Z&X-Amz-Expires=900&"));
        assert!(tok.contains("&X-Amz-Security-Token=FQoGZXIvYXdzEXAMPLE%2Ftoken%2Bwith%3Dchars&"));
        assert_eq!(signature(&tok), "3cd658c1210fb756480562f918fdcd6917edbba42c77966143752b22576151a9");

        let t = parse_rfc3339("2026-10-18T08:31:14Z").unwrap();
        let tok = rds_token("Prod.cluster-ro-c9xyz.eu-west-1.rds.amazonaws.com", 3306, "ana.b@corp", "eu-west-1", &creds(None), t);
        assert_eq!(
            tok,
            "prod.cluster-ro-c9xyz.eu-west-1.rds.amazonaws.com:3306/?Action=connect&DBUser=ana.b%40corp&X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIDEXAMPLE%2F20261018%2Feu-west-1%2Frds-db%2Faws4_request&X-Amz-Date=20261018T083114Z&X-Amz-Expires=900&X-Amz-SignedHeaders=host&X-Amz-Signature=9183ecd834b7005a2d9227dcffc9df66f55f7249762076ae1949efefbfef0009"
        );
    }

    #[test]
    fn regions_come_from_rds_endpoints() {
        let r = |h: &str| region_from_host(h);
        assert_eq!(r("mydb.c1abcdefghij.us-east-1.rds.amazonaws.com").as_deref(), Some("us-east-1"));
        assert_eq!(r("prod.cluster-ro-c9xyz.eu-west-1.rds.amazonaws.com").as_deref(), Some("eu-west-1"));
        assert_eq!(r("px.proxy-c9xyz.ap-southeast-2.rds.amazonaws.com.").as_deref(), Some("ap-southeast-2"));
        assert_eq!(r("db.c9xyz.cn-north-1.rds.amazonaws.com.cn").as_deref(), Some("cn-north-1"));
        assert_eq!(r("DB.ABC.US-GOV-WEST-1.RDS.AMAZONAWS.COM").as_deref(), Some("us-gov-west-1"));
        assert_eq!(r("10.0.0.12"), None);
        assert_eq!(r("localhost"), None);
        assert_eq!(r("rds.amazonaws.com"), None);
        assert_eq!(r("db.example.rds.amazonaws.com"), None);
        assert!(valid_region("us-east-1") && !valid_region("us-east") && !valid_region("us east-1") && !valid_region("us-east-1;x"));
    }

    #[test]
    fn aws_files_are_read_by_profile() {
        let config = "# comment\n[default]\nregion = us-east-2\n\n[profile dev]\nsso_session = corp\nsso_account_id = 123\nregion=eu-west-1\ns3 =\n  max_concurrent_requests = 20\n[profile proc]\ncredential_process = /usr/local/bin/creds --role x\n[sso-session corp]\nsso_region = us-east-1\n[profilex]\nregion = nope\n";
        let credentials = "[default]\naws_access_key_id = AKIA1\naws_secret_access_key = s1\n; c\n[ci]\naws_access_key_id=AKIA2\naws_secret_access_key=s2\naws_session_token=t2\n";
        let f = AwsFiles::parse(config, credentials);
        assert_eq!(f.get("default", "region").as_deref(), Some("us-east-2"));
        assert_eq!(f.get("dev", "region").as_deref(), Some("eu-west-1"));
        assert_eq!(f.get("dev", "max_concurrent_requests"), None);
        assert_eq!(f.get("proc", "credential_process").as_deref(), Some("/usr/local/bin/creds --role x"));
        assert!(f.has_profile("dev") && f.has_profile("ci") && !f.has_profile("corp") && !f.has_profile("x"));
        let d = f.static_keys("default").unwrap();
        assert_eq!((d.access_key_id.as_str(), d.secret_access_key.as_str(), d.session_token), ("AKIA1", "s1", None));
        assert_eq!(f.static_keys("ci").unwrap().session_token.as_deref(), Some("t2"));
        assert!(f.static_keys("dev").is_none());
    }

    #[tokio::test]
    async fn env_keys_come_first_unless_a_profile_is_named() {
        let f = AwsFiles::parse("", "[default]\naws_access_key_id=AKIAFILE\naws_secret_access_key=s\n[named]\naws_access_key_id=AKIANAMED\naws_secret_access_key=s\n");
        let env = |k: &str| match k {
            "AWS_ACCESS_KEY_ID" => Some("AKIAENV".to_string()),
            "AWS_SECRET_ACCESS_KEY" => Some("senv".to_string()),
            _ => None,
        };
        let (c, s) = aws_credentials(false, "default", &f, &env).await.unwrap();
        assert_eq!((c.access_key_id.as_str(), s), ("AKIAENV", AwsSource::Env));
        let (c, s) = aws_credentials(true, "named", &f, &env).await.unwrap();
        assert_eq!((c.access_key_id.as_str(), s), ("AKIANAMED", AwsSource::Profile("named".into())));
        let none = |_: &str| None;
        let (c, _) = aws_credentials(false, "default", &f, &none).await.unwrap();
        assert_eq!(c.access_key_id, "AKIAFILE");
        let missing = aws_credentials(true, "nope", &f, &none).await.unwrap_err();
        assert!(missing.to_string().contains("no existe el perfil de AWS «nope»"), "{missing}");
    }

    #[test]
    fn process_credentials_are_parsed() {
        let c = parse_process_credentials(
            r#"{"Version": 1, "AccessKeyId": "ASIAX", "SecretAccessKey": "sec", "SessionToken": "tok", "Expiration": "2026-10-18T12:00:00+00:00"}"#,
        )
        .unwrap();
        assert_eq!(c.access_key_id, "ASIAX");
        assert_eq!(c.session_token.as_deref(), Some("tok"));
        assert_eq!(c.expires_on, parse_rfc3339("2026-10-18T12:00:00Z"));
        let long = parse_process_credentials(r#"{"Version":1,"AccessKeyId":"AKIAX","SecretAccessKey":"s"}"#).unwrap();
        assert_eq!((long.session_token, long.expires_on), (None, None));
        assert!(parse_process_credentials("not json").is_err());
        assert!(parse_process_credentials(r#"{"Version":1,"AccessKeyId":"AKIAX"}"#).is_err());
        assert!(parse_process_credentials(r#"{"AccessKeyId":"A","SecretAccessKey":"s","Expiration":"soon"}"#).is_err());
    }

    #[test]
    fn secrets_stay_out_of_debug() {
        let c = creds(Some("tok"));
        let shown = format!("{c:?}");
        assert!(!shown.contains(SK) && !shown.contains("tok"), "{shown}");
        let l = Login { password: "secret-token".into(), user: None, identity: None };
        assert!(!format!("{l:?}").contains("secret-token"));
        assert_eq!(mask("AKIAIOSFODNN7EXAMPLE"), "AKIA…MPLE");
        assert_eq!(mask("short"), "…");
    }

    #[test]
    fn azure_cli_output_is_parsed() {
        let (t, e) = parse_az(r#"{"accessToken":"eyJ0","expiresOn":"2026-10-18 11:30:00.000000","expires_on":1792236600,"subscription":"s","tenant":"t","tokenType":"Bearer"}"#).unwrap();
        assert_eq!((t.as_str(), e), ("eyJ0", 1_792_236_600));
        // Older az: only local time; kept a short while.
        let (_, e) = parse_az(r#"{"accessToken":"eyJ0","expiresOn":"2026-10-18 11:30:00.000000"}"#).unwrap();
        assert!(e > now() && e <= now() + MARGIN + UNKNOWN_TTL);
        assert!(parse_az(r#"{"error":"x"}"#).is_err());
        let (t, e) = parse_azd(r#"{"token":"eyJ1","expiresOn":"2026-10-18T11:30:00Z"}"#).unwrap();
        assert_eq!((t.as_str(), Some(e)), ("eyJ1", parse_rfc3339("2026-10-18T11:30:00Z")));
    }

    #[test]
    fn entra_claims_say_who_signed_in() {
        // {"upn":"ana@contoso.com","oid":"o1"} as base64url, unsigned.
        let token = "eyJhbGciOiJub25lIn0.eyJ1cG4iOiJhbmFAY29udG9zby5jb20iLCJvaWQiOiJvMSJ9.";
        let c = jwt_claims(token).unwrap();
        assert_eq!(c["upn"], "ana@contoso.com");
        assert_eq!(base64url("aGk").unwrap(), b"hi");
        assert_eq!(base64url("aGk=").unwrap(), b"hi");
        assert!(base64url("a*b").is_none());
        assert!(jwt_claims("opaque").is_none());
    }

    #[test]
    fn gcloud_output_and_users() {
        assert_eq!(bare_token("ya29.a0Af\n").unwrap(), "ya29.a0Af");
        assert!(bare_token("\n").is_err());
        assert!(bare_token("WARNING: something\nya29").is_err());
        assert_eq!(gcp_db_user("ana@empresa.com", false), "ana@empresa.com");
        assert_eq!(gcp_db_user("ana@empresa.com", true), "ana");
        assert_eq!(gcp_db_user("app@proj.iam.gserviceaccount.com", false), "app@proj.iam");
        assert_eq!(gcp_db_user("app@proj.iam.gserviceaccount.com", true), "app");
    }

    #[test]
    fn metadata_answers_are_parsed() {
        assert_eq!(parse_http(b"HTTP/1.0 200 OK\r\nContent-Type: x\r\n\r\n{\"a\":1}"), Some((200, "{\"a\":1}".into())));
        assert_eq!(parse_http(b"HTTP/1.1 404 Not Found\r\n\r\n").map(|r| r.0), Some(404));
        assert_eq!(parse_http(b"garbage"), None);
    }

    #[test]
    fn cli_arguments_stay_safe() {
        assert!(safe_arg("ana@empresa.com") && safe_arg("my-profile_2") && safe_arg("contoso.onmicrosoft.com"));
        for bad in ["", "a b", "x&y", "a|b", "$(id)", "a\"b", "%PATH%", "a;b", "a\nb"] {
            assert!(!safe_arg(bad), "{bad:?}");
        }
        assert!(cmd_line(Path::new(r"C:\gcloud.cmd"), &["sql", "generate-login-token", "--account", "ana@x.com"]).is_ok());
        assert!(cmd_line(Path::new(r"C:\gcloud.cmd"), &["a&b"]).is_err());
        let c = cli_candidates("gcloud", None, Some(Path::new("/home/ana")));
        assert!(c.iter().any(|p| p.ends_with("google-cloud-sdk/bin/gcloud") || p.ends_with(r"google-cloud-sdk\bin\gcloud.cmd")));
    }

    #[test]
    fn the_rds_bundle_splits_into_certificates() {
        let certs = pem_certificates(RDS_CA_BUNDLE);
        assert!(certs.len() > 50, "{}", certs.len());
        assert!(certs.iter().all(|c| c.starts_with(b"-----BEGIN CERTIFICATE-----") && c.ends_with(b"-----END CERTIFICATE-----")));
        assert!(pem_certificates(b"nothing").is_empty());
    }

    fn cfg(auth_mode: &str) -> ConnectionConfig {
        let mut c = ConnectionConfig { host: "db.c1.us-east-1.rds.amazonaws.com".into(), username: Some("ana".into()), ..Default::default() };
        c.options.insert(AUTH.into(), auth_mode.into());
        c
    }

    #[tokio::test]
    async fn the_password_sign_in_is_untouched() {
        let spec = Spec { default_port: 5432, gcp_access_token: false, mysql: false };
        assert!(login(&cfg(PASSWORD), spec).await.unwrap().is_none());
        assert!(login(&ConnectionConfig::default(), spec).await.unwrap().is_none());
        assert_eq!(method(&cfg(AWS)), Some(AWS));
        assert_eq!(method(&cfg("entra_default")), None);
    }

    #[tokio::test]
    async fn tokens_never_go_to_an_unverified_server() {
        let mut c = cfg(AWS);
        c.trust_server_certificate = true;
        let spec = Spec { default_port: 5432, gcp_access_token: false, mysql: false };
        let e = login(&c, spec).await.unwrap_err();
        assert!(matches!(e, Error::AuthFailed(_)) && e.to_string().contains("Confiar en el certificado"), "{e}");
        let mut c = cfg(AWS);
        c.username = None;
        assert!(login(&c, spec).await.unwrap_err().to_string().contains("falta el usuario"));
    }

    #[test]
    fn tunnels_sign_for_the_real_endpoint() {
        let mut c = cfg(AWS);
        c.port = 0;
        assert_eq!(endpoint(&c, 5432), ("db.c1.us-east-1.rds.amazonaws.com".into(), 5432));
        c.host = "127.0.0.1".into();
        c.port = 61000;
        c.options.insert(ORIGIN_HOST.into(), "db.c1.us-east-1.rds.amazonaws.com".into());
        c.options.insert(ORIGIN_PORT.into(), "5433".into());
        assert_eq!(endpoint(&c, 5432), ("db.c1.us-east-1.rds.amazonaws.com".into(), 5433));
    }

    #[test]
    fn the_form_offers_only_what_the_variant_has() {
        let mut f = Field::server_set();
        adapt(&mut f, Offer { aws: true, gcp: false, azure: false });
        let keys: Vec<&str> = f.iter().map(|f| f.key).collect();
        let at = keys.iter().position(|k| *k == AUTH).unwrap();
        assert_eq!(&keys[at..at + 4], &[AUTH, "aws_profile", "aws_region", "username"]);
        assert!(!keys.contains(&"gcp_account") && !keys.contains(&"azure_tenant"));
        let pw = f.iter().find(|f| f.key == "password").unwrap();
        assert_eq!(pw.when.as_ref().map(|w| (w.key, w.values.clone())), Some((AUTH, vec![PASSWORD])));
        let FieldKind::Select(options) = &f[at].kind else { panic!() };
        assert_eq!(options.iter().map(|o| o.0).collect::<Vec<_>>(), vec![PASSWORD, AWS]);

        let mut plain = Field::server_set();
        adapt(&mut plain, Offer::default());
        assert!(plain.iter().all(|f| f.key != AUTH) && plain.iter().find(|f| f.key == "password").unwrap().when.is_none());
    }

    // Live checks against this machine's own CLIs (no database): run with
    // `cargo test -p dbine-driver-postgres cloud_auth -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_azure_cli_signs_in() {
        let mut c = cfg(AZURE);
        c.username = None;
        c.options.insert(DESCRIBE.into(), "true".into());
        let l = azure_login(&c, true).await.unwrap();
        assert!(jwt_claims(&l.password).is_some(), "an Entra ID token is a JWT");
        let who = l.identity.unwrap();
        assert!(who.starts_with("Microsoft Entra ID"), "{who}");
        println!("identity: {who}; user: {:?}", l.user);
    }

    #[tokio::test]
    #[ignore]
    async fn live_aws_chain_signs_a_token() {
        let mut c = cfg(AWS);
        c.options.insert(DESCRIBE.into(), "true".into());
        let spec = Spec { default_port: 5432, gcp_access_token: false, mysql: false };
        let l = login(&c, spec).await.unwrap().unwrap();
        assert!(l.password.starts_with("db.c1.us-east-1.rds.amazonaws.com:5432/?Action=connect&DBUser=ana&"));
        println!("identity: {}", l.identity.unwrap());
    }

    #[test]
    fn identity_goes_after_the_version() {
        assert_eq!(with_identity("PostgreSQL 16.4".into(), None), "PostgreSQL 16.4");
        assert_eq!(with_identity("PostgreSQL 16.4".into(), Some("AWS IAM: arn:x")), "PostgreSQL 16.4 — identidad: AWS IAM: arn:x");
    }
}
