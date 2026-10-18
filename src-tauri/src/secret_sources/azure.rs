//! Azure Key Vault: a secret's value over its REST API, with an Entra ID
//! token from the same chain as the Entra ID sign-in (cloud_auth.rs: Azure
//! CLI, Azure Developer CLI, managed identity) for the vault's cloud. The
//! vault URL must be a Key Vault host (`*.vault.azure.net` or another
//! cloud's), so the token never goes anywhere else.

use super::cloud_auth::{self, uri_encode};
use serde_json::Value;
use std::time::Duration;

const API_VERSION: &str = "7.4";

pub async fn get_secret(vault_url: &str, name: &str, version: &str) -> Result<String, String> {
    let (host, resource) = dbine_core::secret_sources::azure_vault_host(vault_url).ok_or_else(|| "la URL del almacén tiene que ser https://<nombre>.vault.azure.net".to_string())?;
    let (token, _) = cloud_auth::azure_token_for(resource, None).await.map_err(|e| e.to_string())?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    get_secret_at(&client, &format!("https://{host}"), name.trim(), version.trim(), &token).await
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
pub async fn browse(vault_url: &str, at: super::browse::At<'_>) -> Result<super::browse::Listing, String> {
    let (host, resource) = dbine_core::secret_sources::azure_vault_host(vault_url).ok_or_else(|| "la URL del almacén tiene que ser https://<nombre>.vault.azure.net".to_string())?;
    let (token, _) = cloud_auth::azure_token_for(resource, None).await.map_err(|e| e.to_string())?;
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
}
