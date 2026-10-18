//! Azure Key Vault: a secret's value over its REST API, with an Entra ID
//! token from the same chain as the Entra ID sign-in (cloud_auth.rs: Azure
//! CLI, Azure Developer CLI, managed identity) for the vault's cloud. The
//! vault URL must be a Key Vault host (`*.vault.azure.net` or another
//! cloud's), so the token never goes anywhere else.

use super::cloud_auth::{self, uri_encode};
use serde_json::Value;
use std::time::Duration;

const API_VERSION: &str = "7.4";

pub async fn get_secret(vault_url: &str, name: &str, version: &str, tenant: &str) -> Result<String, String> {
    let (host, resource) = dbine_core::secret_sources::azure_vault_host(vault_url).ok_or_else(|| "la URL del almacén tiene que ser https://<nombre>.vault.azure.net".to_string())?;
    let (token, _) = token_for(resource, tenant).await?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    get_secret_at(&client, &format!("https://{host}"), name.trim(), version.trim(), &token).await
}

/// The tenant to ask the token for (`None`: the CLI's current one); only a
/// well-formed one goes to `az` as an argument.
fn tenant_arg(tenant: &str) -> Result<Option<&str>, String> {
    let t = tenant.trim();
    if t.is_empty() {
        return Ok(None);
    }
    if !dbine_core::secret_sources::valid_azure_tenant(t) {
        return Err("el inquilino de Microsoft Entra no es válido (su ID o su dominio)".into());
    }
    Ok(Some(t))
}

/// A token for `resource` from the Azure CLI chain, for `tenant` when one
/// is set. When the CLI has no sign-in in that tenant, the error says how
/// to sign in there ([`no_sign_in`]): DBine never runs `az login` itself.
async fn token_for(resource: &str, tenant: &str) -> Result<(String, u64), String> {
    let t = tenant_arg(tenant)?;
    cloud_auth::azure_token_for(resource, t).await.map_err(|e| {
        let e = e.to_string();
        match t {
            Some(t) if needs_tenant_sign_in(&e) => no_sign_in(t),
            _ => e,
        }
    })
}

/// The Azure CLI has no usable sign-in in the tenant (a valid one: it goes
/// into the command the user copies).
pub fn no_sign_in(tenant: &str) -> String {
    format!("la CLI de Azure no tiene una sesión iniciada en ese inquilino: iniciá sesión con «az login --tenant {tenant}» y probá de nuevo")
}

/// What `az` says when the token for a tenant needs a new sign-in there:
/// MFA or Conditional Access of that tenant (AADSTS50076, 50079, 53003,
/// 50078), an expired or revoked refresh token (700082, 70043, 50173),
/// a user that isn't in the tenant (50020), or MSAL asking for an
/// interactive login.
const SIGN_IN_MARKERS: &[&str] = &[
    "aadsts50076",
    "aadsts50079",
    "aadsts53003",
    "aadsts50078",
    "aadsts700082",
    "aadsts70043",
    "aadsts50173",
    "aadsts50020",
    "interaction_required",
    "invalid_grant",
    "az login",
    "--tenant",
    "re-authenticate",
    "msal cache",
];

/// Whether a token error (cloud_auth's chain) is the Azure CLI asking for a
/// sign-in: only its own line, without the hint cloud_auth adds to every
/// CLI failure («(¿falta az login?)»).
pub fn needs_tenant_sign_in(err: &str) -> bool {
    let line = err.lines().map(str::trim).find_map(|l| l.strip_prefix("- Azure CLI:")).unwrap_or(if err.contains("\n") { "" } else { err });
    let line = line.trim();
    let line = line.strip_suffix("(¿falta az login?)").unwrap_or(line).to_lowercase();
    SIGN_IN_MARKERS.iter().any(|m| line.contains(m))
}

/// A tenant the Azure CLI knows (the wizard's list): nothing secret.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Tenant {
    pub id: String,
    pub display_name: String,
    pub default_domain: String,
}

/// At most this many tenants go to the wizard.
const MAX_TENANTS: usize = 500;

/// `az account tenant list --output json`: `tenantId` (or `id`,
/// `/tenants/<id>`), `displayName`, `defaultDomain`.
pub fn parse_tenant_list(stdout: &str) -> Option<Vec<Tenant>> {
    let json: Value = serde_json::from_str(stdout).ok()?;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let list = json.as_array()?.iter().map(|t| {
        let id = Some(s(t, "tenantId")).filter(|v| !v.is_empty()).unwrap_or_else(|| s(t, "id").trim_start_matches("/tenants/").to_string());
        Tenant { id, display_name: s(t, "displayName"), default_domain: s(t, "defaultDomain") }
    });
    Some(tidy(list))
}

/// `az account list --all --output json`: the subscriptions, grouped by
/// `tenantId` (newer CLIs say `tenantDisplayName` and `tenantDefaultDomain`).
pub fn parse_account_list(stdout: &str) -> Option<Vec<Tenant>> {
    let json: Value = serde_json::from_str(stdout).ok()?;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    let list = json.as_array()?.iter().map(|a| Tenant { id: s(a, "tenantId"), display_name: s(a, "tenantDisplayName"), default_domain: s(a, "tenantDefaultDomain") });
    Some(tidy(list))
}

/// Well-formed IDs only, once each (the entry that names the tenant wins),
/// by name.
fn tidy(list: impl Iterator<Item = Tenant>) -> Vec<Tenant> {
    let mut out: Vec<Tenant> = Vec::new();
    for t in list.filter(|t| dbine_core::secret_sources::valid_azure_tenant(&t.id)) {
        match out.iter().position(|o| o.id.eq_ignore_ascii_case(&t.id)) {
            Some(i) => {
                let o = &mut out[i];
                if o.display_name.is_empty() {
                    o.display_name = t.display_name;
                }
                if o.default_domain.is_empty() {
                    o.default_domain = t.default_domain;
                }
            }
            None if out.len() < MAX_TENANTS => out.push(t),
            None => {}
        }
    }
    out.sort_by(|a, b| (a.display_name.is_empty(), a.display_name.to_lowercase(), &a.id).cmp(&(b.display_name.is_empty(), b.display_name.to_lowercase(), &b.id)));
    out
}

/// The tenants the Azure CLI knows: `az account tenant list`, else the
/// tenants of `az account list --all`.
pub async fn tenants() -> Result<Vec<Tenant>, String> {
    let first = cloud_auth::run_cli("az", &["account", "tenant", "list", "--output", "json"], &[], &[], "az login").await;
    if let Some(list) = first.as_deref().ok().and_then(parse_tenant_list) {
        return Ok(list);
    }
    let out = cloud_auth::run_cli("az", &["account", "list", "--all", "--output", "json"], &[], &[], "az login").await.map_err(|e| e.to_string())?;
    parse_account_list(&out).ok_or_else(|| "«az account list» no devolvió JSON".to_string())
}

/// Who a token is for (the account wizard's check): the user's name or
/// the app's ID, and the tenant, from its claims (not verified: Azure's
/// tools issued it and it only names who signed in).
pub fn token_identity(token: &str) -> Option<(String, String)> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    let s = |k: &str| claims.get(k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_string);
    let who = s("upn").or_else(|| s("preferred_username")).or_else(|| s("unique_name")).or_else(|| s("appid").map(|a| format!("app {a}")))?;
    Some((who, s("tid").unwrap_or_default()))
}

/// The account wizard's check: a token for the vault's cloud from the
/// Azure CLI chain, for the tenant when one is set. `(who, tenant)`.
pub async fn whoami(vault_url: &str, tenant: &str) -> Result<(String, String), String> {
    let (_, resource) = dbine_core::secret_sources::azure_vault_host(vault_url).ok_or_else(|| "la URL del almacén tiene que ser https://<nombre>.vault.azure.net".to_string())?;
    let (mut token, _) = token_for(resource, tenant).await?;
    let who = token_identity(&token);
    dbine_core::secret_sources::wipe(&mut token);
    let (who, tid) = who.ok_or("Microsoft Entra ID no dijo para quién es el token")?;
    let wanted = tenant.trim();
    if dbine_core::secret_sources::is_uuid(wanted) && !tid.is_empty() && !tid.eq_ignore_ascii_case(wanted) {
        return Err(format!("el token es del inquilino {tid}, no de {wanted}"));
    }
    Ok((who, tid))
}

/// What `az account show` says (the wizard's starting point): the tenant,
/// the user and the subscription's name, plus the tenants the CLI knows
/// ([`tenants`]). Nothing secret.
pub async fn current_account() -> Result<Value, String> {
    let out = cloud_auth::run_cli("az", &["account", "show", "--output", "json"], &[], &[], "az login").await.map_err(|e| e.to_string())?;
    let json: Value = serde_json::from_str(&out).map_err(|_| "«az account show» no devolvió JSON".to_string())?;
    let s = |p: &str| json.pointer(p).and_then(Value::as_str).unwrap_or("").to_string();
    let tenant = s("/tenantId");
    // The tenants to choose from, with the current one always among them.
    let mut list = tenants().await.unwrap_or_default();
    if dbine_core::secret_sources::valid_azure_tenant(&tenant) && !list.iter().any(|t| t.id.eq_ignore_ascii_case(&tenant)) {
        list.insert(0, Tenant { id: tenant.clone(), display_name: s("/tenantDisplayName"), default_domain: s("/tenantDefaultDomain") });
    }
    Ok(serde_json::json!({ "tenant": tenant, "user": s("/user/name"), "subscription": s("/name"), "tenants": list }))
}

pub fn secret_url(base: &str, name: &str, version: &str) -> String {
    let base = base.trim_end_matches('/');
    match version {
        "" => format!("{base}/secrets/{}?api-version={API_VERSION}", uri_encode(name)),
        v => format!("{base}/secrets/{}/{}?api-version={API_VERSION}", uri_encode(name), uri_encode(v)),
    }
}

pub async fn get_secret_at(client: &reqwest::Client, base: &str, name: &str, version: &str, token: &str) -> Result<String, String> {
    let resp = client
        .get(secret_url(base, name, version))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("no se pudo conectar con Azure Key Vault: {}", e.without_url()))?;
    let status = resp.status().as_u16();
    let json: Value = serde_json::from_str(&resp.text().await.unwrap_or_default()).unwrap_or(Value::Null);
    if status == 200 {
        return json.get("value").and_then(Value::as_str).map(str::to_string).ok_or_else(|| format!("Azure Key Vault no devolvió un valor para «{name}»"));
    }
    let msg = json.pointer("/error/message").and_then(Value::as_str).unwrap_or("");
    let msg = super::exec::first_lines(msg, 300);
    Err(match status {
        401 => format!("Azure Key Vault rechazó el token (¿«az login» con la cuenta correcta?): {msg}"),
        403 => format!("Azure Key Vault: permiso denegado (hace falta el rol «Key Vault Secrets User» o una directiva de acceso que permita get): {msg}"),
        404 => format!("Azure Key Vault: no existe el secreto «{name}»"),
        _ if msg.is_empty() => format!("Azure Key Vault respondió {status}"),
        _ => format!("Azure Key Vault respondió {status}: {msg}"),
    })
}

// ---- browse ------------------------------------------------------------------

/// The vault's secret names (`GET /secrets`, paged), or a secret's JSON
/// keys (`item`).
pub async fn browse(vault_url: &str, tenant: &str, at: super::browse::At<'_>) -> Result<super::browse::Listing, String> {
    let (host, resource) = dbine_core::secret_sources::azure_vault_host(vault_url).ok_or_else(|| "la URL del almacén tiene que ser https://<nombre>.vault.azure.net".to_string())?;
    let (token, _) = token_for(resource, tenant).await?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    browse_at(&client, &format!("https://{host}"), &token, at).await
}

pub async fn browse_at(client: &reqwest::Client, base: &str, token: &str, at: super::browse::At<'_>) -> Result<super::browse::Listing, String> {
    use super::browse::{Entry, Listing, MAX_ENTRIES};
    if let Some(item) = at.item {
        let raw = get_secret_at(client, base, item, "", token).await?;
        return Ok(Listing::fields(super::browse::json_keys(raw)));
    }
    let base = base.trim_end_matches('/');
    let mut url = format!("{base}/secrets?api-version={API_VERSION}&maxresults=25");
    let mut out = Vec::new();
    loop {
        let resp = client.get(&url).bearer_auth(token).send().await.map_err(|e| format!("no se pudo conectar con Azure Key Vault: {}", e.without_url()))?;
        let status = resp.status().as_u16();
        let json: Value = serde_json::from_str(&resp.text().await.unwrap_or_default()).unwrap_or(Value::Null);
        if status != 200 {
            let msg = super::exec::first_lines(json.pointer("/error/message").and_then(Value::as_str).unwrap_or(""), 300);
            return Err(match status {
                401 => format!("Azure Key Vault rechazó el token (¿«az login» con la cuenta correcta?): {msg}"),
                403 => format!("Azure Key Vault: permiso denegado para listar los secretos (hace falta un rol o una directiva de acceso que permita list): {msg}"),
                _ if msg.is_empty() => format!("Azure Key Vault respondió {status}"),
                _ => format!("Azure Key Vault respondió {status}: {msg}"),
            });
        }
        for s in json.get("value").and_then(Value::as_array).into_iter().flatten() {
            let id = s.get("id").and_then(Value::as_str).unwrap_or("");
            if let Some(name) = id.rsplit('/').next().filter(|n| !n.is_empty()) {
                out.push(Entry::item(name, name, "", s.get("contentType").and_then(Value::as_str).unwrap_or("")));
            }
        }
        // The next page, only on the same vault (the token goes nowhere else).
        match json.get("nextLink").and_then(Value::as_str) {
            Some(n) if n.starts_with(&format!("{base}/")) && out.len() <= MAX_ENTRIES => url = n.to_string(),
            _ => break,
        }
    }
    out.sort_by_key(|e| e.name.to_lowercase());
    Ok(Listing::complete(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(secret_url("https://kv1.vault.azure.net/", "db-pw", ""), "https://kv1.vault.azure.net/secrets/db-pw?api-version=7.4");
        assert_eq!(secret_url("https://kv1.vault.azure.net", "db-pw", "abc123"), "https://kv1.vault.azure.net/secrets/db-pw/abc123?api-version=7.4");
    }

    /// Against Lowkey Vault as `dbine-test-lowkey-vault` (port 28443,
    /// self-signed; it takes any bearer token) with a secret `db-pw` = `pw`:
    /// `DBINE_TEST_LOWKEY=https://127.0.0.1:28443 cargo test -p dbine secret_sources::azure -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_lowkey_vault() {
        let Ok(base) = std::env::var("DBINE_TEST_LOWKEY") else { return };
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder().tls_danger_accept_invalid_certs(true).build().unwrap();
        assert_eq!(get_secret_at(&client, &base, "db-pw", "", "dummy").await.unwrap(), "pw");
        let e = get_secret_at(&client, &base, "nope", "", "dummy").await.unwrap_err();
        assert!(e.contains("no existe el secreto «nope»"), "{e}");
        let l = browse_at(&client, &base, "dummy", Default::default()).await.unwrap();
        assert!(l.entries.iter().any(|e| e.id == "db-pw"), "{:?}", l.entries);
    }

    #[test]
    fn who_a_token_is_for() {
        use base64::Engine;
        let jwt = |claims: serde_json::Value| format!("x.{}.y", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string()));
        assert_eq!(token_identity(&jwt(serde_json::json!({ "upn": "ana@contoso.com", "tid": "t-1" }))), Some(("ana@contoso.com".into(), "t-1".into())));
        assert_eq!(token_identity(&jwt(serde_json::json!({ "appid": "a-1", "tid": "t-1" }))), Some(("app a-1".into(), "t-1".into())));
        assert_eq!(token_identity("not-a-jwt"), None);
        assert!(tenant_arg("--debug").is_err());
        assert_eq!(tenant_arg(" contoso.onmicrosoft.com ").unwrap(), Some("contoso.onmicrosoft.com"));
        assert_eq!(tenant_arg("").unwrap(), None);
    }

    #[test]
    fn tenant_lists() {
        // `az account tenant list`: a repeated tenant once, a malformed ID out.
        let out = r#"[
            {"id":"/tenants/22222222-2222-4333-8444-555555555555","tenantId":"22222222-2222-4333-8444-555555555555","displayName":"Fabrikam","defaultDomain":"fabrikam.onmicrosoft.com","tenantCategory":"Home"},
            {"id":"/tenants/11111111-2222-4333-8444-555555555555","displayName":"Contoso","defaultDomain":"contoso.onmicrosoft.com"},
            {"tenantId":"22222222-2222-4333-8444-555555555555","displayName":"","defaultDomain":""},
            {"tenantId":"--debug","displayName":"Evil"}
        ]"#;
        let t = parse_tenant_list(out).unwrap();
        assert_eq!(t.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), ["11111111-2222-4333-8444-555555555555", "22222222-2222-4333-8444-555555555555"]);
        assert_eq!((t[0].display_name.as_str(), t[0].default_domain.as_str()), ("Contoso", "contoso.onmicrosoft.com"));
        assert_eq!(t[1].default_domain, "fabrikam.onmicrosoft.com");
        assert!(parse_tenant_list("ERROR").is_none());
        assert_eq!(parse_tenant_list("[]"), Some(vec![]));
        // `az account list --all`: subscriptions grouped by tenant.
        let out = r#"[
            {"name":"Prod","tenantId":"11111111-2222-4333-8444-555555555555","tenantDisplayName":"Contoso","tenantDefaultDomain":"contoso.onmicrosoft.com"},
            {"name":"Dev","tenantId":"11111111-2222-4333-8444-555555555555"},
            {"name":"Other","tenantId":"33333333-2222-4333-8444-555555555555"}
        ]"#;
        let t = parse_account_list(out).unwrap();
        assert_eq!(t, vec![
            Tenant { id: "11111111-2222-4333-8444-555555555555".into(), display_name: "Contoso".into(), default_domain: "contoso.onmicrosoft.com".into() },
            Tenant { id: "33333333-2222-4333-8444-555555555555".into(), display_name: String::new(), default_domain: String::new() },
        ]);
    }

    #[test]
    fn sign_in_errors() {
        let chain = |cli: &str| format!("no se pudo obtener un token de Microsoft Entra ID. Se probó:\n- Azure CLI: {cli} (¿falta az login?)\n- Azure Developer CLI: «azd» no está instalada\n- identidad administrada: no responde (DBine no corre en la nube)");
        for cli in [
            "AADSTS50076: Due to a configuration change made by your administrator, or because you moved to a new location, you must use multi-factor authentication",
            "AADSTS50079: Due to a configuration change made by your administrator, you must enroll in multi-factor authentication",
            "AADSTS53003: Access has been blocked by Conditional Access policies.",
            "AADSTS700082: The refresh token has expired due to inactivity.",
            "The refresh token has expired. Please run 'az login --tenant 11111111-2222-4333-8444-555555555555'.",
            "Run the command below to authenticate interactively; additional arguments may be added as needed: interaction_required",
        ] {
            assert!(needs_tenant_sign_in(&chain(cli)), "{cli}");
        }
        // The hint cloud_auth adds to every failure doesn't count.
        assert!(!needs_tenant_sign_in(&chain("«az» no está instalada")));
        assert!(!needs_tenant_sign_in(&chain("(Forbidden) The user does not have access to the resource")));
        assert!(!needs_tenant_sign_in("no se pudo obtener un token de Microsoft Entra ID. Se probó:\n- Azure Developer CLI: run az login"));
        assert_eq!(
            no_sign_in("contoso.onmicrosoft.com"),
            "la CLI de Azure no tiene una sesión iniciada en ese inquilino: iniciá sesión con «az login --tenant contoso.onmicrosoft.com» y probá de nuevo"
        );
    }

    #[test]
    fn tenant_validation() {
        use dbine_core::secret_sources::valid_azure_tenant as ok;
        for t in ["11111111-2222-4333-8444-555555555555", "contoso.onmicrosoft.com", "a"] {
            assert!(ok(t) && tenant_arg(t).is_ok(), "{t}");
        }
        for t in ["-x", ".x", "a b", "a;b", "a/b", "ñ", &"a".repeat(256)] {
            assert!(!ok(t) && tenant_arg(t).is_err(), "{t}");
        }
        assert!(ok(&"a".repeat(255)));
    }
}
