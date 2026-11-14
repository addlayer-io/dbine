//! Entra ID sign-in for Power BI (spec §3). Owned by part B.
//!
//! Every flow ends in a bearer token for the cloud's Power BI resource
//! ([`Audience::PowerBi`]); the definition API and the model's rename and
//! description (Fabric) take a token for `api.fabric.microsoft.com`
//! ([`Audience::Fabric`]), asked only when needed. Tokens and refresh tokens
//! live in the driver host's memory only, in a host-wide cache keyed by the
//! signed-in account (`tid` + `oid`, Critique S2), cloud, client and scope
//! set, renewed 5 minutes before they expire; one sign-in at a time per
//! connection and audience.
//!
//! Each connection is bound to the account it signed in with: sessions of
//! the same connection (same mode, cloud, tenant, client, user and secret)
//! reuse that account's tokens, and a connection that names another user or
//! tenant signs in on its own (`prompt=select_account` when it names no
//! user). Two connections that name no user (same cloud, tenant and client)
//! are one identity and share the account. A token whose `tid` isn't the
//! connection's tenant (when that's a GUID) is refused.
//!
//! A new sign-in or credential (the browser, a client secret, a managed
//! identity, the default chain after `az login`) that comes back as another
//! account rebinds a connection that names no user: every session of it
//! follows the new account. A silent renewal (refresh token) that comes
//! back as another account, or a new sign-in that doesn't match the user
//! the connection names, is refused and the binding is forgotten, so the
//! next attempt (or a reconnect) signs in afresh.
//!
//! Adapted from `sqlserver/src/entra.rs` and `databricks/src/u2m.rs` (driver
//! crates can't share code). Nothing here logs a token, a refresh token or a
//! secret, not even through `Debug`.

pub mod browser;
pub mod certificate;
pub mod cloud;
pub mod service;

use crate::config::{AuthMode, ClientApp, Secret, Settings};
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use cloud::{Cloud, FABRIC_RESOURCE};
use dbine_driver::{Error, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// What a token is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Audience {
    /// The cloud's Power BI REST API (`{res}` of [`cloud::Cloud::resource`]).
    PowerBi,
    /// The Fabric REST API ([`cloud::FABRIC_RESOURCE`]).
    Fabric,
}

/// A bearer token and when it stops working.
#[derive(Debug, Clone)]
pub struct AccessToken {
    pub token: Secret,
    pub expires_at: Option<SystemTime>,
}

/// The token endpoint's answer, as the cache keeps it.
#[derive(Debug, Clone)]
pub struct TokenSet {
    pub access: AccessToken,
    /// Browser sign-in only (`offline_access`).
    pub refresh: Option<Secret>,
}

/// Who signed in (from the token's claims, decoded without verification:
/// the service checks the token; this only names the account).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// `tid`.
    pub tenant_id: String,
    /// `oid`.
    pub object_id: String,
    /// `upn` / `preferred_username` (what `configuredBy` holds); `None` for
    /// a service principal.
    pub upn: Option<String>,
    /// `scp`, space separated (delegated scopes in the token).
    pub scopes: Vec<String>,
}

/// Where the sign-in and the APIs are: the cloud's hosts, or a fake
/// server's in tests (release builds take them from the cloud table only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// `https://login.microsoftonline.com` (no trailing `/`).
    pub login: String,
    /// The REST API base (`https://api.powerbi.com/v1.0/myorg`).
    pub api: String,
    /// The Fabric API base, where it's offered.
    pub fabric: Option<String>,
}

impl Endpoints {
    pub fn of(cloud: Cloud) -> Endpoints {
        Endpoints {
            login: cloud.login().to_string(),
            api: cloud.api_base().to_string(),
            fabric: cloud.fabric_api().map(str::to_string),
        }
    }

    /// Every endpoint on one test server.
    #[cfg(test)]
    pub fn test(base: &str) -> Endpoints {
        Endpoints {
            login: format!("{base}/login"),
            api: format!("{base}/v1.0/myorg"),
            fabric: Some(format!("{base}/fabric/v1")),
        }
    }
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn auth(msg: impl Into<String>) -> Error {
    Error::AuthFailed(msg.into())
}

/// The first non-empty line of a message, at most `max` characters.
pub(crate) fn short(msg: &str, max: usize) -> String {
    let line = msg
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    match line.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}

/// A number that may come as a JSON number or a string (`"3599"`).
pub(crate) fn number(v: Option<&Value>) -> Option<u64> {
    let v = v?;
    v.as_u64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

/// The resource a token is for.
pub(crate) fn resource(cloud: Cloud, audience: Audience) -> &'static str {
    match audience {
        Audience::PowerBi => cloud.resource(),
        Audience::Fabric => FABRIC_RESOURCE,
    }
}

impl TokenSet {
    /// From an OAuth 2.0 token response (`expires_in` in seconds); `None`
    /// without an `access_token`.
    pub(crate) fn from_oauth(body: &Value, now: u64) -> Option<TokenSet> {
        let access = body.get("access_token")?.as_str()?;
        let expires_in = number(body.get("expires_in")).unwrap_or(3600);
        Some(TokenSet {
            access: AccessToken {
                token: Secret::new(access),
                expires_at: Some(UNIX_EPOCH + Duration::from_secs(now + expires_in)),
            },
            refresh: body
                .get("refresh_token")
                .and_then(Value::as_str)
                .map(Secret::new),
        })
    }
}

/// `{login}/{tenant}/oauth2/v2.0/token` (also a client assertion's `aud`).
pub(crate) fn token_url(login: &str, tenant: &str) -> Result<reqwest::Url> {
    crate::config::valid_tenant(tenant)?;
    let mut url =
        reqwest::Url::parse(login).map_err(|_| auth("dirección de inicio de sesión no válida"))?;
    url.path_segments_mut()
        .map_err(|_| auth("dirección de inicio de sesión no válida"))?
        .extend([tenant, "oauth2", "v2.0", "token"]);
    Ok(url)
}

/// The token endpoint of the connection's tenant (`organizations` without
/// one): the token set, or the sign-in error in Spanish.
pub(crate) async fn token_endpoint(
    settings: &Settings,
    login: &str,
    tenant: &str,
    form: &[(&str, &str)],
) -> Result<TokenSet> {
    let url = token_url(login, tenant)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| Error::Connect(e.to_string()))?;
    let resp = client
        .post(url)
        .form(form)
        .send()
        .await
        .map_err(|e| Error::connect_failed(dbine_driver::ConnectFailure::from_error(&e)))?;
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.map_err(|e| {
        Error::Connect(format!(
            "no se pudo leer la respuesta de Microsoft Entra ID: {e}"
        ))
    })?;
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    match TokenSet::from_oauth(&body, now()) {
        Some(t) => Ok(t),
        None if body.is_null() => Err(auth(format!(
            "Microsoft Entra ID respondió HTTP {status} sin un token"
        ))),
        None => Err(browser::sign_in_error_for(&body, &settings.client_app)),
    }
}

// ------------------------------------------------------------ the cache

/// What a cached token is for: the account, its cloud, the client that
/// asked and the scopes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TokenKey {
    cloud: Cloud,
    client: String,
    scopes: String,
    tid: String,
    oid: String,
}

#[derive(Clone)]
struct Cached {
    access: Secret,
    expires_on: u64,
    refresh: Option<Secret>,
}

// Tokens never reach a log, not even through `{:?}`.
impl std::fmt::Debug for Cached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Cached")
            .field("expires_on", &self.expires_on)
            .finish_non_exhaustive()
    }
}

/// A cached token is reused until this long before it expires.
const MARGIN: u64 = 5 * 60;

static TOKENS: LazyLock<Mutex<HashMap<TokenKey, Cached>>> = LazyLock::new(Mutex::default);
/// Each connection identity's account (see [`TokenProvider`]).
static BINDINGS: LazyLock<Mutex<HashMap<String, Account>>> = LazyLock::new(Mutex::default);
/// One sign-in at a time per connection identity and audience: sessions
/// that open together wait for the first and take its token.
static SIGN_INS: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(Mutex::default);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// How a token set was obtained.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// A refresh token's renewal: the same account, silently.
    Renewal,
    /// A new sign-in or credential: the browser, a client secret, a managed
    /// identity, the default chain.
    SignIn,
}

fn same_account(a: &Account, b: &Account) -> bool {
    a.tenant_id == b.tenant_id && a.object_id == b.object_id
}

#[derive(Debug, PartialEq)]
enum Lookup {
    Fresh(Secret, u64),
    Refresh(Secret),
    Miss,
}

fn lookup(key: &TokenKey, now: u64) -> Lookup {
    let cache = lock(&TOKENS);
    match cache.get(key) {
        Some(t) if now + MARGIN < t.expires_on => Lookup::Fresh(t.access.clone(), t.expires_on),
        Some(Cached {
            refresh: Some(r), ..
        }) => Lookup::Refresh(r.clone()),
        _ => Lookup::Miss,
    }
}

/// A refresh token of the account for the same client, whatever its
/// scopes (Entra's refresh tokens serve every resource the client was
/// consented for).
fn any_refresh(cloud: Cloud, client: &str, tid: &str, oid: &str) -> Option<Secret> {
    lock(&TOKENS)
        .iter()
        .filter(|(k, _)| k.cloud == cloud && k.client == client && k.tid == tid && k.oid == oid)
        .find_map(|(_, v)| v.refresh.clone())
}

/// A connection's sign-in: hands out tokens per audience from the host-wide
/// cache, signing in (or refreshing) when needed.
pub struct TokenProvider {
    settings: Settings,
    endpoints: Endpoints,
    /// The connection's identity: mode, cloud, tenant, client, user and a
    /// hash of the secret.
    fingerprint: String,
    /// The account this connection signed in with.
    bound: Mutex<Option<Account>>,
}

impl std::fmt::Debug for TokenProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenProvider")
            .field("auth", &self.settings.auth)
            .field("cloud", &self.settings.cloud)
            .finish_non_exhaustive()
    }
}

/// The client id the connection signs in with.
pub(crate) fn client_id(settings: &Settings) -> String {
    match settings.auth {
        AuthMode::EntraMfa => match &settings.client_app {
            ClientApp::Dbine => browser::DBINE_APP.to_string(),
            ClientApp::AzureCli => browser::AZURE_CLI_APP.to_string(),
            ClientApp::Custom(id) => id.clone(),
        },
        AuthMode::EntraSp | AuthMode::EntraMsi | AuthMode::EntraDefault => {
            settings.client_id.clone().unwrap_or_default()
        }
        AuthMode::Token => String::new(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn fingerprint(settings: &Settings) -> String {
    let hash = |s: &str| hex(&Sha256::digest(s.as_bytes())[..12]);
    let secret = match (&settings.client_certificate, &settings.client_secret) {
        (Some(c), _) => hash(&format!(
            "certificate\0{}\0{}",
            c.path,
            c.password.as_ref().map_or("", |p| p.expose())
        )),
        (None, Some(s)) => hash(s.expose()),
        (None, None) => String::new(),
    };
    format!(
        "{:?}|{:?}|{}|{}|{}|{}",
        settings.auth,
        settings.cloud,
        settings.tenant.as_deref().unwrap_or("").to_lowercase(),
        client_id(settings),
        settings.username.as_deref().unwrap_or("").to_lowercase(),
        secret
    )
}

impl TokenProvider {
    /// No network: the first [`TokenProvider::token`] signs in.
    pub fn new(settings: &Settings) -> Result<TokenProvider> {
        Ok(Self::with_endpoints(
            settings,
            Endpoints::of(settings.cloud),
        ))
    }

    pub(crate) fn with_endpoints(settings: &Settings, endpoints: Endpoints) -> TokenProvider {
        TokenProvider {
            settings: settings.clone(),
            endpoints,
            fingerprint: fingerprint(settings),
            bound: Mutex::new(None),
        }
    }

    fn key(&self, account: &Account, audience: Audience) -> TokenKey {
        let mut scopes = scopes(&self.settings, audience);
        scopes.sort();
        TokenKey {
            cloud: self.settings.cloud,
            client: client_id(&self.settings),
            scopes: scopes.join(" "),
            tid: account.tenant_id.clone(),
            oid: account.object_id.clone(),
        }
    }

    /// A token for `audience`, from the cache when it has more than 5
    /// minutes left. The browser sign-in asks the scopes of spec §3.2
    /// (`Dataset.ReadWrite.All` only when the connection isn't read-only;
    /// `SemanticModel.ReadWrite.All` for [`Audience::Fabric`]).
    pub async fn token(&self, audience: Audience) -> Result<AccessToken> {
        if audience == Audience::Fabric && self.endpoints.fabric.is_none() {
            return Err(Error::Unsupported(
                "La API de Fabric solo está disponible en la nube pública.".into(),
            ));
        }
        if self.settings.auth == AuthMode::Token {
            let set = service::pasted(&self.settings, audience)?;
            let account = self.account_of(&set.access);
            *lock(&self.bound) = Some(account);
            return Ok(set.access);
        }
        let gate = {
            let mut locks = lock(&SIGN_INS);
            locks
                .entry(format!("{}|{audience:?}", self.fingerprint))
                .or_default()
                .clone()
        };
        let _one = gate.lock().await;
        let mut refresh = None;
        if let Some(account) = self.account() {
            match lookup(&self.key(&account, audience), now()) {
                Lookup::Fresh(token, expires_on) => {
                    return Ok(AccessToken {
                        token,
                        expires_at: Some(UNIX_EPOCH + Duration::from_secs(expires_on)),
                    })
                }
                Lookup::Refresh(r) => refresh = Some(r),
                Lookup::Miss => {
                    refresh = any_refresh(
                        self.settings.cloud,
                        &client_id(&self.settings),
                        &account.tenant_id,
                        &account.object_id,
                    )
                }
            }
        }
        let (set, origin) = self.acquire(audience, refresh).await?;
        self.store(audience, set, origin)
    }

    async fn acquire(
        &self,
        audience: Audience,
        refresh: Option<Secret>,
    ) -> Result<(TokenSet, Origin)> {
        let s = &self.settings;
        let set = match s.auth {
            AuthMode::EntraMfa => {
                if let Some(r) = refresh {
                    match browser::refresh_at(s, &self.endpoints, audience, &r).await {
                        Ok(mut t) => {
                            t.refresh.get_or_insert(r);
                            return Ok((t, Origin::Renewal));
                        }
                        Err(_) => {
                            // Never the answer's text in a log (S4).
                            tracing::debug!("Power BI: the refresh token failed; signing in again");
                        }
                    }
                }
                browser::sign_in_at(s, &self.endpoints, audience, self.account()).await?
            }
            AuthMode::EntraSp => {
                service::client_credentials_at(s, &self.endpoints, audience).await?
            }
            AuthMode::EntraMsi => service::managed_identity(s, audience).await?,
            AuthMode::EntraDefault => {
                service::default_chain_at(s, &self.endpoints, audience).await?
            }
            AuthMode::Token => service::pasted(s, audience)?,
        };
        Ok((set, Origin::SignIn))
    }

    /// The account a token names (a stand-in tied to this connection when
    /// the token isn't a JWT).
    fn account_of(&self, access: &AccessToken) -> Account {
        match claims(access.token.expose()) {
            Ok(c) => account_from_claims(&c),
            Err(_) => Account {
                tenant_id: self.settings.tenant.clone().unwrap_or_default(),
                object_id: format!(
                    "local:{}",
                    hex(&Sha256::digest(self.fingerprint.as_bytes())[..8])
                ),
                upn: None,
                scopes: Vec::new(),
            },
        }
    }

    fn store(&self, audience: Audience, set: TokenSet, origin: Origin) -> Result<AccessToken> {
        let account = self.account_of(&set.access);
        if let Some(tenant) = self.settings.tenant.as_deref() {
            if crate::catalog::valid_guid(tenant)
                && !account.tenant_id.is_empty()
                && !account.tenant_id.eq_ignore_ascii_case(tenant)
            {
                return Err(auth(format!(
                    "La cuenta inició sesión en otro inquilino ({}) que el de la conexión ({tenant}).",
                    account.tenant_id
                )));
            }
        }
        if let Some(prev) = self.account() {
            if !same_account(&prev, &account) {
                let named =
                    self.settings.auth == AuthMode::EntraMfa && self.settings.username.is_some();
                if origin == Origin::Renewal || named {
                    // Forgotten, so the next attempt signs in afresh
                    // instead of meeting the same binding again.
                    self.forget(&prev);
                    return Err(auth(if named {
                        "Iniciaste sesión con otra cuenta que la del campo Usuario de la conexión: volvé a intentar e iniciá sesión con esa cuenta, o cambiá el usuario."
                    } else {
                        "Microsoft Entra ID renovó la sesión con otra cuenta que la de esta conexión: volvé a intentar para iniciar sesión de nuevo."
                    }));
                }
                // A new sign-in as another account, on a connection that
                // names no user: the connection (every session of it)
                // follows it.
                tracing::debug!("Power BI: the connection signed in as another account; rebound");
            }
        }
        let expires_on = set
            .access
            .expires_at
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(now() + 3600, |d| d.as_secs());
        lock(&TOKENS).insert(
            self.key(&account, audience),
            Cached {
                access: set.access.token.clone(),
                expires_on,
                refresh: set.refresh.clone(),
            },
        );
        lock(&BINDINGS).insert(self.fingerprint.clone(), account.clone());
        *lock(&self.bound) = Some(account);
        Ok(set.access)
    }

    /// Drops the connection's binding to `prev` (if it still holds it):
    /// the next token of this connection signs in afresh.
    fn forget(&self, prev: &Account) {
        {
            let mut all = lock(&BINDINGS);
            if all
                .get(&self.fingerprint)
                .is_some_and(|a| same_account(a, prev))
            {
                all.remove(&self.fingerprint);
            }
        }
        *lock(&self.bound) = None;
    }

    /// After a 401: forget the cached token for `audience` and get a new
    /// one (refresh token first, sign-in again only when it fails).
    pub async fn renew(&self, audience: Audience) -> Result<AccessToken> {
        if self.settings.auth == AuthMode::Token {
            return Err(auth(
                "El token pegado venció o no es para Power BI: pegá uno nuevo.",
            ));
        }
        if let Some(account) = self.account() {
            if let Some(c) = lock(&TOKENS).get_mut(&self.key(&account, audience)) {
                c.expires_on = 0;
            }
        }
        self.token(audience).await
    }

    /// The account the connection signed in with, once it did: the
    /// connection's binding (every session of it follows a rebind), else
    /// the last one this session used.
    pub fn account(&self) -> Option<Account> {
        let shared = lock(&BINDINGS).get(&self.fingerprint).cloned();
        match shared {
            Some(a) => {
                *lock(&self.bound) = Some(a.clone());
                Some(a)
            }
            None => lock(&self.bound).clone(),
        }
    }
}

/// `tid`, `oid` (or `sub`), the user's name and `scp` of a token's claims.
fn account_from_claims(c: &Value) -> Account {
    let s = |k: &str| c.get(k).and_then(Value::as_str).map(str::to_string);
    Account {
        tenant_id: s("tid").unwrap_or_default(),
        object_id: s("oid").or_else(|| s("sub")).unwrap_or_default(),
        upn: s("upn")
            .or_else(|| s("preferred_username"))
            .or_else(|| s("unique_name")),
        scopes: s("scp")
            .map(|x| x.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default(),
    }
}

/// The delegated scopes the browser sign-in asks for `audience` (spec
/// §3.2): per cloud, the read-only rule, `.default` for the Azure CLI's
/// client and for every flow that isn't a browser sign-in.
pub fn scopes(settings: &Settings, audience: Audience) -> Vec<String> {
    let res = resource(settings.cloud, audience);
    let default = || vec![format!("{res}/.default")];
    if settings.auth != AuthMode::EntraMfa {
        return default();
    }
    let mut out = match (&settings.client_app, audience) {
        (ClientApp::AzureCli, _) => {
            return vec![format!("{res}/.default"), "offline_access".into()]
        }
        (_, Audience::Fabric) => vec![format!("{res}/SemanticModel.ReadWrite.All")],
        (_, Audience::PowerBi) => {
            let mut v = vec![
                format!("{res}/Dataset.Read.All"),
                format!("{res}/Workspace.Read.All"),
                format!("{res}/Capacity.Read.All"),
            ];
            if !settings.read_only {
                v.push(format!("{res}/Dataset.ReadWrite.All"));
            }
            v
        }
    };
    out.extend(["offline_access", "openid", "profile"].map(String::from));
    out
}

/// The claims of a JWT (its payload, base64url-decoded, not verified): for
/// [`Account`] and the pasted token's audience check (S10).
pub fn claims(token: &str) -> Result<Value> {
    let mut parts = token.trim().split('.');
    let (Some(_), Some(payload), Some(_), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(auth("el token no es un JWT"));
    };
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .or_else(|_| URL_SAFE.decode(payload))
        .map_err(|_| auth("el token no es un JWT"))?;
    let v: Value = serde_json::from_slice(&bytes).map_err(|_| auth("el token no es un JWT"))?;
    if !v.is_object() {
        return Err(auth("el token no es un JWT"));
    }
    Ok(v)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use dbine_driver::ConnectionConfig;
    use serde_json::json;

    /// An unsigned JWT with these claims (the driver never verifies one).
    pub(crate) fn jwt(claims: &Value) -> String {
        format!(
            "{}.{}.sig",
            URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}"),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        )
    }

    pub(crate) fn settings(opts: &[(&str, &str)], read_only: bool) -> Settings {
        let cfg = ConnectionConfig {
            driver: "powerbi".into(),
            read_only,
            options: opts
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        };
        Settings::from_config(&cfg).unwrap()
    }

    #[test]
    fn scopes_per_client_and_read_only() {
        let res = "https://analysis.windows.net/powerbi/api";
        let ro = scopes(&settings(&[], true), Audience::PowerBi);
        assert!(ro.contains(&format!("{res}/Dataset.Read.All")));
        assert!(ro.contains(&format!("{res}/Workspace.Read.All")));
        assert!(ro.contains(&"offline_access".to_string()));
        // A read-only connection never asks for the write scope.
        assert!(!ro.iter().any(|s| s.ends_with("Dataset.ReadWrite.All")));
        let rw = scopes(&settings(&[], false), Audience::PowerBi);
        assert!(rw.contains(&format!("{res}/Dataset.ReadWrite.All")));
        let fabric = scopes(&settings(&[], true), Audience::Fabric);
        assert_eq!(
            fabric[0],
            "https://api.fabric.microsoft.com/SemanticModel.ReadWrite.All"
        );
        // The Azure CLI's first-party client takes `.default`.
        let cli = scopes(
            &settings(&[("client_app", "azure_cli")], true),
            Audience::PowerBi,
        );
        assert_eq!(cli, [format!("{res}/.default"), "offline_access".into()]);
        // Service principals and the rest: `.default` of the cloud's resource.
        let gov = settings(
            &[
                ("auth_mode", "entra_sp"),
                ("cloud", "gcc"),
                ("tenant", "t"),
                ("client_id", "c"),
                ("client_secret", "s"),
            ],
            false,
        );
        assert_eq!(
            scopes(&gov, Audience::PowerBi),
            ["https://analysis.usgovcloudapi.net/powerbi/api/.default"]
        );
    }

    #[test]
    fn claims_are_read_not_trusted() {
        let t = jwt(
            &json!({"tid": "T1", "oid": "O1", "upn": "ana@contoso.com", "scp": "Dataset.Read.All Workspace.Read.All"}),
        );
        let a = account_from_claims(&claims(&t).unwrap());
        assert_eq!(
            a,
            Account {
                tenant_id: "T1".into(),
                object_id: "O1".into(),
                upn: Some("ana@contoso.com".into()),
                scopes: vec!["Dataset.Read.All".into(), "Workspace.Read.All".into()],
            }
        );
        for bad in ["", "abc", "a.b", "a.!!!.c", "a.b.c.d"] {
            assert!(claims(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_cache_never_shows_a_token() {
        let c = Cached {
            access: Secret::new("access-secret"),
            expires_on: 1,
            refresh: Some(Secret::new("refresh-secret")),
        };
        let shown = format!("{c:?}");
        assert!(!shown.contains("secret"), "a token shows in Debug");
        let set = TokenSet::from_oauth(
            &json!({"access_token": "a-tok", "expires_in": "60", "refresh_token": "r-tok"}),
            100,
        )
        .unwrap();
        let shown = format!("{set:?}");
        assert!(
            !shown.contains("a-tok") && !shown.contains("r-tok"),
            "a token shows in Debug"
        );
        assert_eq!(
            set.access.expires_at,
            Some(UNIX_EPOCH + Duration::from_secs(160))
        );
        let p = TokenProvider::new(&settings(
            &[
                ("auth_mode", "entra_sp"),
                ("tenant", "t"),
                ("client_id", "c"),
                ("client_secret", "hunter2"),
            ],
            true,
        ))
        .unwrap();
        let shown = format!("{p:?}");
        assert!(!shown.contains("hunter2"), "a secret shows in Debug");
    }

    #[tokio::test]
    async fn sessions_of_a_connection_share_its_token() {
        use crate::http::testing::{serve, Reply};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let issued = Arc::new(AtomicUsize::new(0));
        let n = issued.clone();
        let base = serve(Arc::new(move |req| {
            let i = n.fetch_add(1, Ordering::SeqCst);
            let scope = if req.body.contains("api.fabric.microsoft.com") {
                "fabric"
            } else {
                "pbi"
            };
            let tok = jwt(&json!({"tid": "44444444-4444-4444-4444-444444444444", "oid": "cache-sp", "n": i, "s": scope}));
            Reply::json(200, &format!(r#"{{"access_token":"{tok}","expires_in":3599}}"#))
        }))
        .await;
        let sp = |secret: &str| {
            settings(
                &[
                    ("auth_mode", "entra_sp"),
                    ("tenant", "44444444-4444-4444-4444-444444444444"),
                    ("client_id", "cache-test-client"),
                    ("client_secret", secret),
                ],
                true,
            )
        };
        let ep = Endpoints::test(&base);
        let a = TokenProvider::with_endpoints(&sp("one"), ep.clone());
        let b = TokenProvider::with_endpoints(&sp("one"), ep.clone());
        let ta = a.token(Audience::PowerBi).await.unwrap();
        let tb = b.token(Audience::PowerBi).await.unwrap();
        assert_eq!(ta.token.expose(), tb.token.expose());
        assert_eq!(
            issued.load(Ordering::SeqCst),
            1,
            "the second session reused it"
        );
        assert_eq!(b.account().unwrap().object_id, "cache-sp");
        // Another scope set (Fabric) is another token.
        let tf = a.token(Audience::Fabric).await.unwrap();
        assert_ne!(tf.token.expose(), ta.token.expose());
        assert_eq!(issued.load(Ordering::SeqCst), 2);
        // Another secret is another connection: it signs in on its own.
        let c = TokenProvider::with_endpoints(&sp("two"), ep.clone());
        c.token(Audience::PowerBi).await.unwrap();
        assert_eq!(issued.load(Ordering::SeqCst), 3);
        // After a 401 the token is renewed, not reused.
        let renewed = a.renew(Audience::PowerBi).await.unwrap();
        assert_ne!(renewed.token.expose(), ta.token.expose());
        assert_eq!(issued.load(Ordering::SeqCst), 4);
        assert_eq!(
            b.token(Audience::PowerBi).await.unwrap().token.expose(),
            renewed.token.expose()
        );
    }

    fn set_for(tid: &str, oid: &str) -> TokenSet {
        TokenSet {
            access: AccessToken {
                token: Secret::new(jwt(&json!({"tid": tid, "oid": oid}))),
                expires_at: Some(UNIX_EPOCH + Duration::from_secs(now() + 3600)),
            },
            refresh: None,
        }
    }

    #[test]
    fn a_new_sign_in_rebinds_a_connection_without_a_user() {
        let tid = "66666666-6666-6666-6666-666666666601";
        let s = settings(&[("tenant", tid)], true);
        let a = TokenProvider::new(&s).unwrap();
        a.store(Audience::PowerBi, set_for(tid, "rb-a"), Origin::SignIn)
            .unwrap();
        // Picked another account at the next browser sign-in (or `az login`
        // changed): the connection follows it, every session of it.
        let other = TokenProvider::new(&s).unwrap();
        assert_eq!(other.account().unwrap().object_id, "rb-a");
        a.store(Audience::PowerBi, set_for(tid, "rb-b"), Origin::SignIn)
            .unwrap();
        assert_eq!(a.account().unwrap().object_id, "rb-b");
        assert_eq!(other.account().unwrap().object_id, "rb-b");
        assert_eq!(
            TokenProvider::new(&s).unwrap().account().unwrap().object_id,
            "rb-b"
        );
        // A silent renewal as another account is refused, and the binding
        // is forgotten: the next attempt signs in afresh.
        let e = a
            .store(Audience::PowerBi, set_for(tid, "rb-c"), Origin::Renewal)
            .unwrap_err();
        assert!(e.to_string().contains("renovó la sesión"), "{e}");
        assert!(a.account().is_none());
        assert!(TokenProvider::new(&s).unwrap().account().is_none());
        assert!(!lock(&BINDINGS).contains_key(&fingerprint(&s)));
    }

    #[test]
    fn a_connection_that_names_its_user_keeps_it() {
        let tid = "66666666-6666-6666-6666-666666666602";
        let mut s = settings(&[("tenant", tid)], true);
        s.username = Some("ana@contoso.com".into());
        let p = TokenProvider::new(&s).unwrap();
        p.store(Audience::PowerBi, set_for(tid, "named-a"), Origin::SignIn)
            .unwrap();
        let e = p
            .store(Audience::PowerBi, set_for(tid, "named-b"), Origin::SignIn)
            .unwrap_err();
        assert!(e.to_string().contains("campo Usuario"), "{e}");
        assert!(TokenProvider::new(&s).unwrap().account().is_none());
        // Signing in with the right account then binds it again.
        p.store(Audience::PowerBi, set_for(tid, "named-a"), Origin::SignIn)
            .unwrap();
        assert_eq!(p.account().unwrap().object_id, "named-a");
    }

    #[tokio::test]
    async fn a_service_principal_follows_its_new_credential() {
        use crate::http::testing::{serve, Reply};
        use std::sync::atomic::{AtomicUsize, Ordering};
        let tid = "66666666-6666-6666-6666-666666666603";
        let issued = Arc::new(AtomicUsize::new(0));
        let n = issued.clone();
        let base = serve(Arc::new(move |_req| {
            let i = n.fetch_add(1, Ordering::SeqCst);
            let tok = jwt(&json!({"tid": tid, "oid": format!("sp-{i}")}));
            Reply::json(
                200,
                &format!(r#"{{"access_token":"{tok}","expires_in":3599}}"#),
            )
        }))
        .await;
        let s = settings(
            &[
                ("auth_mode", "entra_sp"),
                ("tenant", tid),
                ("client_id", "rebind-client"),
                ("client_secret", "s"),
            ],
            true,
        );
        let ep = Endpoints::test(&base);
        let a = TokenProvider::with_endpoints(&s, ep.clone());
        a.token(Audience::PowerBi).await.unwrap();
        assert_eq!(a.account().unwrap().object_id, "sp-0");
        // After a 401 the new credential names another principal: no
        // "volvé a conectar" loop, the connection follows it.
        let t = a.renew(Audience::PowerBi).await.unwrap();
        assert_eq!(a.account().unwrap().object_id, "sp-1");
        let b = TokenProvider::with_endpoints(&s, ep);
        assert_eq!(
            b.token(Audience::PowerBi).await.unwrap().token.expose(),
            t.token.expose()
        );
        assert_eq!(issued.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn connection_identities() {
        let a = fingerprint(&settings(&[], true));
        // Another user, tenant, client or secret is another identity.
        let mut with_user = settings(&[], true);
        with_user.username = Some("Ana@Contoso.com".into());
        assert_ne!(a, fingerprint(&with_user));
        assert_ne!(
            a,
            fingerprint(&settings(&[("tenant", "contoso.com")], true))
        );
        assert_ne!(
            a,
            fingerprint(&settings(&[("client_app", "azure_cli")], true))
        );
        let sp = |secret: &str| {
            fingerprint(&settings(
                &[
                    ("auth_mode", "entra_sp"),
                    ("tenant", "t"),
                    ("client_id", "c"),
                    ("client_secret", secret),
                ],
                true,
            ))
        };
        assert_ne!(sp("one"), sp("two"));
        assert!(!sp("hunter2").contains("hunter2"));
        // Another certificate file or password is another identity, and a
        // certificate isn't the secret's.
        let cert = |path: &str, password: &str| {
            fingerprint(&settings(
                &[
                    ("auth_mode", "entra_sp_cert"),
                    ("tenant", "t"),
                    ("client_id", "c"),
                    ("client_certificate", path),
                    ("client_certificate_password", password),
                ],
                true,
            ))
        };
        assert_ne!(cert("/a.pfx", "hunter2"), cert("/b.pfx", "hunter2"));
        assert_ne!(cert("/a.pfx", "hunter2"), cert("/a.pfx", "other"));
        assert_ne!(cert("/a.pfx", "hunter2"), sp("hunter2"));
        assert_ne!(cert("/a.pfx", ""), cert("/a.pfx", "hunter2"));
        assert!(!cert("/a.pfx", "hunter2").contains("hunter2"));
        // The user is case-insensitive.
        let mut lower = settings(&[], true);
        lower.username = Some("ana@contoso.com".into());
        assert_eq!(fingerprint(&with_user), fingerprint(&lower));
    }
}
