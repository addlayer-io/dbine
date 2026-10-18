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
    let (token, _) = cloud_auth::azure_token_for(resource, tenant_arg(tenant)?).await.map_err(|e| e.to_string())?;
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
    let (mut token, _) = cloud_auth::azure_token_for(resource, tenant_arg(tenant)?).await.map_err(|e| e.to_string())?;
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
/// the user and the subscription's name. Nothing secret.
pub async fn current_account() -> Result<Value, String> {
    let out = cloud_auth::run_cli("az", &["account", "show", "--output", "json"], &[], &[], "az login").await.map_err(|e| e.to_string())?;
    let json: Value = serde_json::from_str(&out).map_err(|_| "«az account show» no devolvió JSON".to_string())?;
    let s = |p: &str| json.pointer(p).and_then(Value::as_str).unwrap_or("").to_string();
    Ok(serde_json::json!({ "tenant": s("/tenantId"), "user": s("/user/name"), "subscription": s("/name") }))
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
    let (token, _) = cloud_auth::azure_token_for(resource, tenant_arg(tenant)?).await.map_err(|e| e.to_string())?;
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
}
