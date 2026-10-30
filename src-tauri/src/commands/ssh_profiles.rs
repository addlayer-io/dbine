//! SSH tunnel profiles (dbine_core::ssh_profiles; docs/ssh-tunnels.md,
//! "Profiles"): Settings › Perfiles SSH and the profile picker of a
//! connection's tunnel. Secret values only ever go in (to the keychain,
//! under the profile's id); nothing here reads them back.

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::ssh_profiles::{self, Keychain, SshProfile};
use dbine_driver::ConnectionConfig;
use serde::{Deserialize, Serialize};
use tauri::State;

/// A profile as the UI lists it: with the connections that use it.
#[derive(Serialize)]
pub struct ListedSshProfile {
    #[serde(flatten)]
    pub profile: SshProfile,
    /// The connections that use it, by name (the delete warning names them).
    pub used_by: Vec<ProfileUser>,
}

#[derive(Serialize)]
pub struct ProfileUser {
    pub id: String,
    pub name: String,
}

#[tauri::command]
pub async fn list_ssh_profiles(state: State<'_, AppState>) -> CommandResult<Vec<ListedSshProfile>> {
    listed(&state.store)
}

fn listed(store: &dbine_core::StateStore) -> CommandResult<Vec<ListedSshProfile>> {
    let mut connections = store.list_connections()?;
    connections.extend(store.list_temporary_connections()?);
    Ok(store
        .list_ssh_profiles()?
        .into_iter()
        .map(|profile| {
            let used_by = connections
                .iter()
                .filter(|c| c.config.option(ssh_profiles::PROFILE) == Some(profile.id.as_str()))
                .map(|c| ProfileUser {
                    id: c.id.clone(),
                    name: c.name.clone(),
                })
                .collect();
            ListedSshProfile { profile, used_by }
        })
        .collect())
}

#[derive(Deserialize)]
pub struct SaveSshProfileArgs {
    /// `id` empty: a new profile. Secret values may come in `options`
    /// (`ssh.password`, `ssh.passphrase`, a source's tokens): they're moved
    /// to the keychain; one left empty keeps the stored value.
    pub profile: SshProfile,
    /// "Guardar como perfil" in a saved connection's form: the secrets the
    /// form left empty are copied from that connection's keychain entry.
    #[serde(default)]
    pub from_connection: Option<String>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn save_ssh_profile(
    state: State<'_, AppState>,
    args: SaveSshProfileArgs,
) -> CommandResult<SshProfile> {
    let p = &args.profile;
    let bad = |m: &str| CommandError::BadRequest(format!("el perfil SSH no se puede guardar: {m}"));
    if p.name.trim().is_empty() {
        return Err(bad("falta el nombre"));
    }
    let has = |k: &str| p.options.get(k).is_some_and(|v| !v.trim().is_empty());
    if !has("ssh.host") {
        return Err(bad("falta el servidor SSH"));
    }
    if !has("ssh.user") {
        return Err(bad("falta el usuario SSH"));
    }
    // The servers have to read as the tunnel reads them.
    let probe = ConnectionConfig {
        options: p.options.clone(),
        ..Default::default()
    };
    crate::tunnels::hops(&probe)?;
    let sources = ssh_profiles::tunnel_sources(&p.options)
        .map_err(|e| CommandError::BadRequest(e.to_string()))?;
    for (field, src) in &sources {
        src.validate().map_err(|e| {
            CommandError::BadRequest(format!(
                "el origen de «{}» no es válido: {e}",
                crate::secret_sources::field_label(None, field)
            ))
        })?;
    }
    if let Some(cid) = args.from_connection.as_deref().filter(|c| !c.is_empty()) {
        if state.store.get_connection(cid)?.is_none() {
            return Err(CommandError::NotFound(format!(
                "no existe la conexión '{cid}'"
            )));
        }
    }
    let saved = ssh_profiles::save(
        &state.store,
        &Keychain,
        args.profile,
        args.from_connection.as_deref(),
    )?;
    forget_cached(&state, &saved.id);
    Ok(saved)
}

#[derive(Deserialize)]
pub struct IdArgs {
    pub id: String,
}

/// Delete a profile. The connections that use it (the UI named them and the
/// user confirmed) keep working: each gets a copy of the profile's settings
/// and secrets as its own. Returns their ids.
#[tauri::command(rename_all = "camelCase")]
pub async fn delete_ssh_profile(
    state: State<'_, AppState>,
    args: IdArgs,
) -> CommandResult<Vec<String>> {
    let moved = ssh_profiles::delete(&state.store, &Keychain, &args.id)?;
    forget_cached(&state, &args.id);
    for id in &moved {
        state.keychain_cache.remove(id);
        crate::secret_sources::forget_cached(id);
    }
    Ok(moved)
}

#[derive(Deserialize)]
pub struct TrustSshProfileHostArgs {
    pub profile_id: String,
    /// `[host]:port SHA256:…`, as for `trust_ssh_host`.
    pub fingerprint: String,
}

/// Trust an SSH server's key in a profile (a connection form testing with
/// it): the server has to be one of the profile's and have no other key
/// accepted.
#[tauri::command(rename_all = "camelCase")]
pub async fn trust_ssh_profile_host(
    state: State<'_, AppState>,
    args: TrustSshProfileHostArgs,
) -> CommandResult<()> {
    trust_in_profile(&state.store, &args.profile_id, &args.fingerprint)
}

/// [`trust_ssh_profile_host`]; also what `trust_ssh_host` does for a
/// connection whose tunnel uses a profile.
pub fn trust_in_profile(
    store: &dbine_core::StateStore,
    profile_id: &str,
    fingerprint: &str,
) -> CommandResult<()> {
    let p = store
        .get_ssh_profile(profile_id)?
        .ok_or_else(|| CommandError::NotFound(format!("no existe el perfil SSH '{profile_id}'")))?;
    let mut cfg = ConnectionConfig {
        options: p.options,
        ..Default::default()
    };
    crate::tunnels::trust(&mut cfg, fingerprint)?;
    ssh_profiles::set_trusted(store, profile_id, cfg.option("ssh.trusted").unwrap_or(""))?;
    Ok(())
}

/// Read again on next use: the profile's keychain entry and resolved sources.
fn forget_cached(state: &AppState, profile_id: &str) {
    state.keychain_cache.remove(profile_id);
    state.typed_secrets.remove(profile_id);
    crate::secret_sources::forget_cached(profile_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(c: char) -> String {
        format!("SHA256:{}", c.to_string().repeat(43))
    }

    #[test]
    fn a_profiles_server_key_is_trusted_in_the_profile() {
        let store = dbine_core::StateStore::open_in_memory().unwrap();
        struct NoKeychain;
        impl ssh_profiles::SecretStore for NoKeychain {
            fn get(&self, _: &str) -> dbine_core::Result<dbine_core::secrets::Secrets> {
                Ok(Default::default())
            }
            fn set(&self, _: &str, _: &dbine_core::secrets::Secrets) -> dbine_core::Result<()> {
                Ok(())
            }
        }
        let p = ssh_profiles::save(
            &store,
            &NoKeychain,
            SshProfile {
                id: String::new(),
                name: "p".into(),
                options: [
                    ("ssh.host".to_string(), "bastion".to_string()),
                    ("ssh.user".to_string(), "ops".to_string()),
                ]
                .into(),
                updated_at: String::new(),
            },
            None,
        )
        .unwrap();
        let entry = dbine_tunnel::trusted_entry("bastion", 22, &fp('a'));
        trust_in_profile(&store, &p.id, &entry).unwrap();
        let got = store.get_ssh_profile(&p.id).unwrap().unwrap();
        assert_eq!(got.options["ssh.trusted"], entry);
        // Another key for the same server, or a server that isn't the
        // profile's: refused.
        assert!(trust_in_profile(
            &store,
            &p.id,
            &dbine_tunnel::trusted_entry("bastion", 22, &fp('b'))
        )
        .is_err());
        assert!(trust_in_profile(
            &store,
            &p.id,
            &dbine_tunnel::trusted_entry("elsewhere", 22, &fp('b'))
        )
        .is_err());
        // A connection using it is listed with it.
        let mut c = dbine_core::SavedConnection {
            id: "c1".into(),
            name: "Prod".into(),
            color: None,
            config: ConnectionConfig::default(),
            save_password: false,
            folder_id: None,
            tags: vec![],
            mcp_level: None,
            updated_at: String::new(),
        };
        c.config.options.insert("ssh.enabled".into(), "true".into());
        c.config.options.insert("ssh.profile".into(), p.id.clone());
        store.save_connection(&c).unwrap();
        let l = listed(&store).unwrap();
        assert_eq!(l[0].used_by.len(), 1);
        assert_eq!(l[0].used_by[0].name, "Prod");
    }
}
