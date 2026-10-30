//! SSH tunnel profiles (docs/ssh-tunnels.md, "Profiles"): the SSH side of a
//! tunnel (its servers, user, authentication and accepted server keys)
//! saved once under a name and used by many connections.
//!
//! A connection that uses one keeps `ssh.enabled` and `ssh.profile` (the
//! profile's id) in its options, and its own server (host and port) as
//! always: where the tunnel goes stays per connection. The profile is read
//! when the connection connects ([`StateStore::resolve_ssh_profile`]), so an
//! edit applies on its next connect.
//!
//! The profile's secrets (the password, the key's passphrase and the Vault
//! tokens of their sources) are in the keychain under the profile's id, as a
//! connection's are under its own; its other settings and the definitions
//! of its secrets' sources are in the state (`ssh_profiles`).

use crate::secret_sources::{self as defs, SecretSources};
use crate::secrets::Secrets;
use crate::state::{bump, db_err, now, SavedConnection, StateChange, StateStore};
use dbine_driver::{ConnectionConfig, Error, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The table, created on open (older state files gain it then).
pub(crate) const TABLE: &str = "CREATE TABLE IF NOT EXISTS ssh_profiles (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    options_json TEXT NOT NULL,
    updated_at   TEXT NOT NULL
)";

/// The option naming the profile a connection's tunnel uses.
pub const PROFILE: &str = "ssh.profile";
/// The option that turns a connection's tunnel on.
pub const ENABLED: &str = "ssh.enabled";
/// What a profile holds besides its secrets: the SSH servers (the jump
/// hosts and the one that reaches the database), the user, the
/// authentication and the accepted server keys.
pub const SETTINGS: [&str; 7] = [
    "ssh.host",
    "ssh.port",
    "ssh.user",
    "ssh.auth",
    "ssh.key_path",
    "ssh.jump",
    "ssh.trusted",
];
/// The tunnel's secret fields.
pub const SECRETS: [&str; 2] = ["ssh.password", "ssh.passphrase"];

/// The id prefix of profiles: their keychain entries never meet a
/// connection's.
const ID_PREFIX: &str = "sshp-";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshProfile {
    pub id: String,
    pub name: String,
    /// The [`SETTINGS`] and the definitions of the secrets' sources
    /// (`secret_sources`, only the tunnel's fields). Never a secret value.
    #[serde(default)]
    pub options: BTreeMap<String, String>,
    #[serde(default)]
    pub updated_at: String,
}

/// The profile a connection's tunnel uses (only while the tunnel is on).
pub fn profile_of(cfg: &ConnectionConfig) -> Option<&str> {
    if cfg.option(ENABLED) != Some("true") {
        return None;
    }
    cfg.option(PROFILE).map(str::trim).filter(|p| !p.is_empty())
}

/// A key of the tunnel's secrets: a secret field, or a token of its source.
pub fn is_tunnel_secret(key: &str) -> bool {
    SECRETS.contains(&key)
        || defs::token_parts(key).is_some_and(|(field, _)| SECRETS.contains(&field))
}

fn options_cfg(options: &BTreeMap<String, String>) -> ConnectionConfig {
    ConnectionConfig {
        options: options.clone(),
        ..Default::default()
    }
}

/// The sources of the tunnel's secrets among `options`' definitions.
pub fn tunnel_sources(options: &BTreeMap<String, String>) -> Result<SecretSources> {
    let mut all = defs::read(&options_cfg(options))?;
    all.retain(|k, _| SECRETS.contains(&k.as_str()));
    Ok(all)
}

/// `options`' definitions with the tunnel's replaced by `tunnel` (the
/// driver's fields' stay).
fn set_tunnel_sources(options: &mut BTreeMap<String, String>, tunnel: &SecretSources) {
    let mut cfg = ConnectionConfig {
        options: std::mem::take(options),
        ..Default::default()
    };
    let mut all = defs::read(&cfg).unwrap_or_default();
    all.retain(|k, _| !SECRETS.contains(&k.as_str()));
    all.extend(tunnel.clone());
    defs::write(&mut cfg, &all);
    *options = cfg.options;
}

/// Take the secret values typed for a profile (the password, the
/// passphrase, the tokens of their sources) out of `options`: they go to
/// the keychain. Only non-empty values are returned.
pub fn take_secrets(options: &mut BTreeMap<String, String>) -> Secrets {
    let keys: Vec<String> = options
        .keys()
        .filter(|k| is_tunnel_secret(k))
        .cloned()
        .collect();
    keys.into_iter()
        .filter_map(|k| options.remove(&k).filter(|v| !v.is_empty()).map(|v| (k, v)))
        .collect()
}

/// What a profile keeps of `options`: the [`SETTINGS`] that have a value
/// (trimmed) and the definitions of the tunnel's secrets' sources. Secret
/// values never: take them first ([`take_secrets`]).
pub fn settings_of(options: &BTreeMap<String, String>) -> Result<BTreeMap<String, String>> {
    let mut out: BTreeMap<String, String> = SETTINGS
        .iter()
        .filter_map(|k| {
            options
                .get(*k)
                .map(|v| v.trim())
                .filter(|v| !v.is_empty())
                .map(|v| (k.to_string(), v.to_string()))
        })
        .collect();
    set_tunnel_sources(&mut out, &tunnel_sources(options)?);
    Ok(out)
}

/// Drop a connection's own tunnel settings, secret values and sources of
/// the tunnel's secrets: what the profile it uses gives. `ssh.enabled` and
/// `ssh.profile` stay.
pub fn detach(cfg: &mut ConnectionConfig) {
    cfg.options
        .retain(|k, _| !SETTINGS.contains(&k.as_str()) && !is_tunnel_secret(k));
    set_tunnel_sources(&mut cfg.options, &SecretSources::new());
}

/// The profile's settings in a connection's config, in place of its own
/// (connect time). The definitions of the profile's secrets' sources are
/// not merged: they're resolved as the profile's (its confirmations, its
/// keychain tokens).
pub fn apply(cfg: &mut ConnectionConfig, profile: &SshProfile) {
    detach(cfg);
    for k in SETTINGS {
        if let Some(v) = profile.options.get(k) {
            cfg.options.insert(k.to_string(), v.clone());
        }
    }
}

/// The profile copied into the connection, which no longer names it (the
/// profile deleted): its settings and its sources' definitions. The secret
/// values go separately (keychain to keychain).
pub fn inline(cfg: &mut ConnectionConfig, profile: &SshProfile) {
    apply(cfg, profile);
    let sources = tunnel_sources(&profile.options).unwrap_or_default();
    set_tunnel_sources(&mut cfg.options, &sources);
    cfg.options.remove(PROFILE);
}

/// Where profiles' and connections' secrets are kept (the keychain in the
/// app; memory in tests).
pub trait SecretStore {
    fn get(&self, id: &str) -> Result<Secrets>;
    /// Empty: the entry is removed.
    fn set(&self, id: &str, secrets: &Secrets) -> Result<()>;
}

/// The system keychain ([`crate::secrets`]).
pub struct Keychain;

impl SecretStore for Keychain {
    fn get(&self, id: &str) -> Result<Secrets> {
        crate::secrets::get(id)
    }
    fn set(&self, id: &str, secrets: &Secrets) -> Result<()> {
        crate::secrets::set(id, secrets)
    }
}

/// The secrets the profile keeps given its sources: a field with a source
/// keeps no value, only the tokens its source uses.
fn keep_secret(sources: &SecretSources, key: &str) -> bool {
    if let Some((field, slot)) = defs::token_parts(key) {
        return sources
            .get(field)
            .is_some_and(|s| s.token_slots().contains(&slot));
    }
    SECRETS.contains(&key) && !sources.contains_key(key)
}

fn new_id() -> String {
    format!("{ID_PREFIX}{}", hex::encode(rand::random::<[u8; 16]>()))
}

pub(crate) fn insert(c: &Connection, p: &SshProfile) -> Result<()> {
    let ts = if p.updated_at.is_empty() {
        now()
    } else {
        p.updated_at.clone()
    };
    c.execute(
        "INSERT INTO ssh_profiles (id, name, options_json, updated_at) VALUES (?1, ?2, ?3, ?4)",
        params![p.id, p.name, serde_json::to_string(&p.options)?, ts],
    )
    .map_err(db_err)?;
    Ok(())
}

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(String, String, String, String)> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
}

fn profile((id, name, json, updated_at): (String, String, String, String)) -> Result<SshProfile> {
    Ok(SshProfile {
        id,
        name,
        options: serde_json::from_str(&json)?,
        updated_at,
    })
}

impl StateStore {
    /// The profiles, by name.
    pub fn list_ssh_profiles(&self) -> Result<Vec<SshProfile>> {
        let rows = {
            let c = self.lock()?;
            let mut st = c
                .prepare("SELECT id, name, options_json, updated_at FROM ssh_profiles ORDER BY name COLLATE NOCASE, id")
                .map_err(db_err)?;
            let rows = st.query_map([], row).map_err(db_err)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_err)?
        };
        rows.into_iter().map(profile).collect()
    }

    pub fn get_ssh_profile(&self, id: &str) -> Result<Option<SshProfile>> {
        let r = self
            .lock()?
            .query_row(
                "SELECT id, name, options_json, updated_at FROM ssh_profiles WHERE id = ?1",
                [id],
                row,
            )
            .optional()
            .map_err(db_err)?;
        r.map(profile).transpose()
    }

    /// Insert or update the profile's row as it is (see [`save`] for what a
    /// profile form saves).
    fn put_ssh_profile(&self, p: &SshProfile) -> Result<SshProfile> {
        let ts = now();
        {
            let c = self.lock()?;
            c.execute(
                "INSERT INTO ssh_profiles (id, name, options_json, updated_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET name = ?2, options_json = ?3, updated_at = ?4",
                params![p.id, p.name, serde_json::to_string(&p.options)?, ts],
            )
            .map_err(db_err)?;
            bump(&c)?;
        }
        self.notify(changed(&p.id));
        Ok(SshProfile {
            updated_at: ts,
            ..p.clone()
        })
    }

    /// The connections (saved, and files opened this run) that name the
    /// profile, by name.
    pub fn ssh_profile_users(&self, id: &str) -> Result<Vec<SavedConnection>> {
        let mut all = self.list_connections()?;
        all.extend(self.list_temporary_connections()?);
        all.retain(|c| c.config.option(PROFILE) == Some(id));
        Ok(all)
    }

    /// The profile the connection's tunnel uses put in its config, in place
    /// of its own settings (connect time; see [`apply`]). `None`: it uses no
    /// profile (its own settings, unchanged). A profile that no longer
    /// exists is an error, never a tunnel with half its settings.
    pub fn resolve_ssh_profile(&self, cfg: &mut ConnectionConfig) -> Result<Option<SshProfile>> {
        let Some(id) = profile_of(cfg).map(str::to_string) else {
            return Ok(None);
        };
        let p = self.get_ssh_profile(&id)?.ok_or_else(|| {
            Error::Connect(
                "el perfil SSH de la conexión ya no existe: elegí otro en su túnel SSH".into(),
            )
        })?;
        apply(cfg, &p);
        Ok(Some(p))
    }

    /// Replace a connection's config only (its name, folder, order and
    /// temporary flag stay).
    fn set_connection_config(&self, id: &str, cfg: &ConnectionConfig) -> Result<()> {
        let mut cfg = cfg.clone();
        cfg.password = None;
        {
            let c = self.lock()?;
            c.execute(
                "UPDATE connections SET config_json = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, serde_json::to_string(&cfg)?, now()],
            )
            .map_err(db_err)?;
            bump(&c)?;
        }
        self.notify(StateChange {
            kind: "connection".into(),
            id: Some(id.into()),
            connection_id: None,
            database: None,
        });
        Ok(())
    }
}

fn changed(id: &str) -> StateChange {
    StateChange {
        kind: "ssh_profile".into(),
        id: Some(id.into()),
        connection_id: None,
        database: None,
    }
}

/// Save a profile from its form (a new one when `profile.id` is empty).
/// Secret values typed in `profile.options` go to the keychain under the
/// profile's id; a secret left empty keeps the stored one or, with
/// `from_connection` ("Guardar como perfil"), is copied from that
/// connection's keychain entry, never through the UI.
///
/// A source definition typed here is confirmed by the save; one copied from
/// a connection only if it was confirmed there; one over a stored definition
/// still unconfirmed, or that came from outside (an import, a restored
/// backup), isn't: it asks when it first runs.
pub fn save(
    store: &StateStore,
    io: &dyn SecretStore,
    mut profile: SshProfile,
    from_connection: Option<&str>,
) -> Result<SshProfile> {
    profile.name = profile.name.trim().to_string();
    if profile.name.is_empty() {
        return Err(Error::State("el perfil SSH necesita un nombre".into()));
    }
    let old = if profile.id.is_empty() {
        profile.id = new_id();
        None
    } else {
        Some(
            store
                .get_ssh_profile(&profile.id)?
                .ok_or_else(|| Error::State(format!("no existe el perfil SSH '{}'", profile.id)))?,
        )
    };
    let typed = take_secrets(&mut profile.options);
    let sources = tunnel_sources(&profile.options)?;
    profile.options = settings_of(&profile.options)?;

    let mut stored = match &old {
        Some(_) => io.get(&profile.id)?,
        None => Secrets::new(),
    };
    if let Some(cid) = from_connection.filter(|c| !c.is_empty()) {
        for (k, v) in io.get(cid)? {
            if is_tunnel_secret(&k) {
                stored.entry(k).or_insert(v);
            }
        }
    }
    stored.extend(typed);
    stored.retain(|k, _| keep_secret(&sources, k));
    io.set(&profile.id, &stored)?;

    let old_sources = match &old {
        Some(p) => tunnel_sources(&p.options).unwrap_or_default(),
        None => SecretSources::new(),
    };
    let connections = store.list_connections()?;
    for (field, src) in &sources {
        let digest = defs::digest(&profile.id, field, src);
        if defs::is_approved(&digest) {
            continue;
        }
        let confirm = match from_connection.filter(|c| !c.is_empty()) {
            Some(cid) => defs::is_approved(&defs::digest(cid, field, src)),
            None => {
                let pending = old_sources
                    .get(field)
                    .is_some_and(|s| !defs::is_approved(&defs::digest(&profile.id, field, s)));
                !pending && !defs::from_outside(&connections, field, src)
            }
        };
        if confirm {
            defs::approve(&digest)?;
        }
    }
    store.put_ssh_profile(&profile)
}

/// Delete a profile. The connections that use it get a copy of it first
/// (settings, sources and secret values, keychain to keychain), so none is
/// left without a tunnel; then the profile's row, keychain entry and
/// confirmations go. Returns the ids of the connections that got the copy.
pub fn delete(store: &StateStore, io: &dyn SecretStore, id: &str) -> Result<Vec<String>> {
    let Some(p) = store.get_ssh_profile(id)? else {
        return Ok(Vec::new());
    };
    let secrets: Secrets = io
        .get(id)?
        .into_iter()
        .filter(|(k, _)| is_tunnel_secret(k))
        .collect();
    let sources = tunnel_sources(&p.options).unwrap_or_default();
    let mut moved = Vec::new();
    for c in store.ssh_profile_users(id)? {
        let mut cfg = c.config.clone();
        inline(&mut cfg, &p);
        let mut kept = io.get(&c.id)?;
        kept.retain(|k, _| !is_tunnel_secret(k));
        kept.extend(secrets.clone());
        io.set(&c.id, &kept)?;
        for (field, src) in &sources {
            if defs::is_approved(&defs::digest(id, field, src)) {
                defs::approve(&defs::digest(&c.id, field, src))?;
            }
        }
        store.set_connection_config(&c.id, &cfg)?;
        moved.push(c.id);
    }
    let digests: Vec<String> = sources
        .iter()
        .map(|(f, s)| defs::digest(id, f, s))
        .collect();
    defs::forget(&digests)?;
    io.set(id, &Secrets::new())?;
    {
        let c = store.lock()?;
        c.execute("DELETE FROM ssh_profiles WHERE id = ?1", [id])
            .map_err(db_err)?;
        bump(&c)?;
    }
    store.notify(changed(id));
    Ok(moved)
}

/// Store an accepted server key in the profile (`ssh.trusted`): the keys
/// are the SSH servers', so they belong with the profile.
pub fn set_trusted(store: &StateStore, id: &str, trusted: &str) -> Result<()> {
    let mut p = store
        .get_ssh_profile(id)?
        .ok_or_else(|| Error::State(format!("no existe el perfil SSH '{id}'")))?;
    if trusted.trim().is_empty() {
        p.options.remove("ssh.trusted");
    } else {
        p.options
            .insert("ssh.trusted".into(), trusted.trim().to_string());
    }
    store.put_ssh_profile(&p)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret_sources::SecretSource;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Mem(RefCell<BTreeMap<String, Secrets>>);

    impl SecretStore for Mem {
        fn get(&self, id: &str) -> Result<Secrets> {
            Ok(self.0.borrow().get(id).cloned().unwrap_or_default())
        }
        fn set(&self, id: &str, s: &Secrets) -> Result<()> {
            if s.is_empty() {
                self.0.borrow_mut().remove(id);
            } else {
                self.0.borrow_mut().insert(id.into(), s.clone());
            }
            Ok(())
        }
    }

    fn opts(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn form(name: &str, kv: &[(&str, &str)]) -> SshProfile {
        SshProfile {
            id: String::new(),
            name: name.into(),
            options: opts(kv),
            updated_at: String::new(),
        }
    }

    fn conn(id: &str, kv: &[(&str, &str)]) -> SavedConnection {
        SavedConnection {
            id: id.into(),
            name: id.into(),
            color: None,
            config: ConnectionConfig {
                driver: "postgres".into(),
                host: format!("{id}.db.internal"),
                port: 5432,
                options: opts(kv),
                ..Default::default()
            },
            save_password: true,
            folder_id: None,
            tags: vec![],
            mcp_level: None,
            updated_at: String::new(),
        }
    }

    fn cmd(c: &str) -> SecretSource {
        SecretSource::Command {
            command: c.into(),
            json_key: String::new(),
        }
    }

    fn sources_json(field: &str, s: &SecretSource) -> String {
        serde_json::to_string(&SecretSources::from([(field.to_string(), s.clone())])).unwrap()
    }

    #[test]
    fn old_state_files_gain_the_profiles_table() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE connections (id TEXT PRIMARY KEY, name TEXT NOT NULL, color TEXT, config_json TEXT NOT NULL,
             save_password INTEGER NOT NULL DEFAULT 0, sort_order INTEGER NOT NULL DEFAULT 0,
             created_at TEXT NOT NULL, updated_at TEXT NOT NULL)",
        )
        .unwrap();
        let s = StateStore::init(c).unwrap();
        assert!(s.list_ssh_profiles().unwrap().is_empty());
        let io = Mem::default();
        let p = save(&s, &io, form("Bastión", &[("ssh.host", "b")]), None).unwrap();
        assert_eq!(s.list_ssh_profiles().unwrap(), vec![p]);
    }

    #[test]
    fn profiles_are_created_edited_and_listed_by_name() {
        let s = StateStore::open_in_memory().unwrap();
        let io = Mem::default();
        let rev = s.revision().unwrap();
        let b = save(
            &s,
            &io,
            form(
                "  prod bastion ",
                &[
                    ("ssh.host", " bastion.corp "),
                    ("ssh.user", "ops"),
                    ("ssh.port", ""),
                    ("ssh.enabled", "true"),
                    ("other", "x"),
                ],
            ),
            None,
        )
        .unwrap();
        assert!(b.id.starts_with(ID_PREFIX));
        assert_eq!(b.name, "prod bastion");
        // Only the profile's settings, trimmed; nothing empty or foreign.
        assert_eq!(
            b.options,
            opts(&[("ssh.host", "bastion.corp"), ("ssh.user", "ops")])
        );
        assert!(s.revision().unwrap() > rev, "a change a backup carries");
        let a = save(&s, &io, form("Aux", &[("ssh.host", "aux")]), None).unwrap();
        let names: Vec<String> = s
            .list_ssh_profiles()
            .unwrap()
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, ["Aux", "prod bastion"]);
        // Edited in place, same id.
        let mut edit = b.clone();
        edit.name = "Prod".into();
        edit.options.insert("ssh.port".into(), "2222".into());
        let b2 = save(&s, &io, edit, None).unwrap();
        assert_eq!(b2.id, b.id);
        assert_eq!(
            s.get_ssh_profile(&b.id).unwrap().unwrap().options["ssh.port"],
            "2222"
        );
        // A name is required; an unknown id isn't created.
        assert!(save(&s, &io, form("  ", &[]), None).is_err());
        let mut ghost = form("x", &[]);
        ghost.id = "sshp-missing".into();
        assert!(save(&s, &io, ghost, None).is_err());
        assert_eq!(s.list_ssh_profiles().unwrap().len(), 2);
        // In the backup's snapshot, and back from it.
        let snap = s.snapshot().unwrap();
        assert_eq!(snap.ssh_profiles.len(), 2);
        let other = StateStore::open_in_memory().unwrap();
        other.replace_all(&snap).unwrap();
        assert_eq!(
            other.list_ssh_profiles().unwrap(),
            s.list_ssh_profiles().unwrap()
        );
        let _ = a;
    }

    #[test]
    fn secrets_go_to_the_keychain_under_the_profile_id() {
        let s = StateStore::open_in_memory().unwrap();
        let io = Mem::default();
        let p = save(
            &s,
            &io,
            form(
                "p",
                &[
                    ("ssh.host", "b"),
                    ("ssh.auth", "password"),
                    ("ssh.password", "s3cret"),
                ],
            ),
            None,
        )
        .unwrap();
        // Never in the state.
        assert!(!p.options.contains_key("ssh.password"));
        let json = serde_json::to_string(&s.snapshot().unwrap()).unwrap();
        assert!(!json.contains("s3cret"));
        assert_eq!(io.get(&p.id).unwrap()["ssh.password"], "s3cret");
        // Left empty when editing: kept.
        save(&s, &io, p.clone(), None).unwrap();
        assert_eq!(io.get(&p.id).unwrap()["ssh.password"], "s3cret");
        // A source for the field: the value goes.
        let mut sourced = p.clone();
        sourced.options.insert(
            defs::OPTION.into(),
            sources_json("ssh.password", &cmd("pass ssh")),
        );
        save(&s, &io, sourced, None).unwrap();
        assert!(io.get(&p.id).unwrap().is_empty());
        let _ = defs::forget(&[defs::digest(&p.id, "ssh.password", &cmd("pass ssh"))]);
    }

    #[test]
    fn saving_a_connections_tunnel_as_a_profile_copies_its_secrets() {
        let s = StateStore::open_in_memory().unwrap();
        let io = Mem::default();
        let c = conn(
            "c1",
            &[
                ("ssh.enabled", "true"),
                ("ssh.host", "bastion"),
                ("ssh.user", "ops"),
                ("ssh.auth", "key"),
                ("ssh.key_path", "~/.ssh/id"),
                ("ssh.trusted", "[bastion]:22 SHA256:abc"),
            ],
        );
        s.save_connection(&c).unwrap();
        io.set(
            "c1",
            &opts(&[("ssh.passphrase", "frase"), ("password", "dbpass")]),
        )
        .unwrap();
        let mut f = form("from c1", &[]);
        f.options = c.config.options.clone();
        let p = save(&s, &io, f, Some("c1")).unwrap();
        assert_eq!(p.options["ssh.trusted"], "[bastion]:22 SHA256:abc");
        assert!(!p.options.contains_key(ENABLED));
        // Only the tunnel's secret, copied server side.
        assert_eq!(io.get(&p.id).unwrap(), opts(&[("ssh.passphrase", "frase")]));
    }

    #[test]
    fn a_connection_resolves_the_profile_and_keeps_its_own_server() {
        let s = StateStore::open_in_memory().unwrap();
        let io = Mem::default();
        let p = save(
            &s,
            &io,
            form(
                "p",
                &[
                    ("ssh.host", "bastion"),
                    ("ssh.user", "ops"),
                    ("ssh.jump", "jump:2200"),
                ],
            ),
            None,
        )
        .unwrap();
        let mut cfg = conn(
            "c1",
            &[
                ("ssh.enabled", "true"),
                ("ssh.profile", &p.id),
                // Stale own settings never win over the profile's.
                ("ssh.host", "old"),
                ("ssh.password", "old-secret"),
                ("sslmode", "require"),
            ],
        )
        .config;
        let got = s.resolve_ssh_profile(&mut cfg).unwrap().unwrap();
        assert_eq!(got.id, p.id);
        assert_eq!(cfg.option("ssh.host"), Some("bastion"));
        assert_eq!(cfg.option("ssh.jump"), Some("jump:2200"));
        assert_eq!(cfg.option("ssh.password"), None);
        assert_eq!(cfg.option("sslmode"), Some("require"));
        assert_eq!(
            (cfg.host.as_str(), cfg.port),
            ("c1.db.internal", 5432),
            "where the tunnel goes stays the connection's"
        );
        // An edit applies on the next resolve.
        let mut edit = p.clone();
        edit.options.insert("ssh.host".into(), "bastion2".into());
        save(&s, &io, edit, None).unwrap();
        let mut again = conn("c1", &[("ssh.enabled", "true"), ("ssh.profile", &p.id)]).config;
        s.resolve_ssh_profile(&mut again).unwrap();
        assert_eq!(again.option("ssh.host"), Some("bastion2"));
        // Tunnel off or no profile: untouched.
        let mut off = conn("c2", &[("ssh.profile", &p.id), ("ssh.host", "own")]).config;
        assert!(s.resolve_ssh_profile(&mut off).unwrap().is_none());
        assert_eq!(off.option("ssh.host"), Some("own"));
        // A profile that's gone: an error, not half a tunnel.
        let mut gone = conn(
            "c3",
            &[("ssh.enabled", "true"), ("ssh.profile", "sshp-gone")],
        )
        .config;
        assert!(s.resolve_ssh_profile(&mut gone).is_err());
    }

    #[test]
    fn deleting_a_profile_in_use_inlines_it_into_its_connections() {
        let s = StateStore::open_in_memory().unwrap();
        let io = Mem::default();
        let p = save(
            &s,
            &io,
            form(
                "p",
                &[
                    ("ssh.host", "bastion"),
                    ("ssh.user", "ops"),
                    ("ssh.auth", "password"),
                    ("ssh.password", "tunnel-pw"),
                    ("ssh.trusted", "[bastion]:22 SHA256:abc"),
                ],
            ),
            None,
        )
        .unwrap();
        let user = conn("c1", &[("ssh.enabled", "true"), ("ssh.profile", &p.id)]);
        let other = conn("c2", &[("ssh.enabled", "true"), ("ssh.host", "own")]);
        s.save_connection(&user).unwrap();
        s.save_connection(&other).unwrap();
        io.set("c1", &opts(&[("password", "db")])).unwrap();
        let users: Vec<String> = s
            .ssh_profile_users(&p.id)
            .unwrap()
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert_eq!(users, ["c1"]);

        let moved = delete(&s, &io, &p.id).unwrap();
        assert_eq!(moved, ["c1"]);
        assert!(s.get_ssh_profile(&p.id).unwrap().is_none());
        // The profile's keychain entry is gone…
        assert!(io.get(&p.id).unwrap().is_empty());
        // …and the connection works on its own: settings and secret copied.
        let c1 = s.get_connection("c1").unwrap().unwrap().config;
        assert_eq!(c1.option(PROFILE), None);
        assert_eq!(c1.option(ENABLED), Some("true"));
        assert_eq!(c1.option("ssh.host"), Some("bastion"));
        assert_eq!(c1.option("ssh.trusted"), Some("[bastion]:22 SHA256:abc"));
        assert_eq!(c1.option("ssh.password"), None, "never in the state");
        assert_eq!(
            io.get("c1").unwrap(),
            opts(&[("password", "db"), ("ssh.password", "tunnel-pw")])
        );
        // The connection that didn't use it is untouched.
        assert_eq!(
            s.get_connection("c2").unwrap().unwrap().config.options,
            other.config.options
        );
        // Deleting it again: nothing to do.
        assert!(delete(&s, &io, &p.id).unwrap().is_empty());
    }

    #[test]
    fn a_typed_source_is_confirmed_an_outside_one_is_not() {
        let s = StateStore::open_in_memory().unwrap();
        let io = Mem::default();
        let uid = hex::encode(rand::random::<[u8; 8]>());
        let typed = cmd(&format!("pass typed-{uid}"));
        let outside = cmd(&format!("touch /tmp/never-{uid}"));
        // An imported connection with an unconfirmed definition.
        s.save_connection(&conn(
            &format!("imp-{uid}"),
            &[(defs::OPTION, &sources_json("ssh.password", &outside))],
        ))
        .unwrap();
        let with = |src: &SecretSource| {
            form(
                "p",
                &[
                    ("ssh.host", "b"),
                    (defs::OPTION, &sources_json("ssh.password", src)),
                ],
            )
        };
        let p = save(&s, &io, with(&typed), None).unwrap();
        assert!(defs::is_approved(&defs::digest(
            &p.id,
            "ssh.password",
            &typed
        )));
        let q = save(&s, &io, with(&outside), None).unwrap();
        assert!(!defs::is_approved(&defs::digest(
            &q.id,
            "ssh.password",
            &outside
        )));
        // Inlined on delete: the confirmation follows to the connection.
        let user = format!("u-{uid}");
        s.save_connection(&conn(
            &user,
            &[("ssh.enabled", "true"), ("ssh.profile", &p.id)],
        ))
        .unwrap();
        delete(&s, &io, &p.id).unwrap();
        assert!(defs::is_approved(&defs::digest(
            &user,
            "ssh.password",
            &typed
        )));
        assert!(!defs::is_approved(&defs::digest(
            &p.id,
            "ssh.password",
            &typed
        )));
        let c = s.get_connection(&user).unwrap().unwrap();
        assert_eq!(
            defs::read(&c.config).unwrap().get("ssh.password"),
            Some(&typed)
        );
        let _ = defs::forget(&[defs::digest(&user, "ssh.password", &typed)]);
    }

    #[test]
    fn trusted_keys_are_kept_in_the_profile() {
        let s = StateStore::open_in_memory().unwrap();
        let io = Mem::default();
        let p = save(&s, &io, form("p", &[("ssh.host", "b")]), None).unwrap();
        set_trusted(&s, &p.id, "[b]:22 SHA256:x").unwrap();
        assert_eq!(
            s.get_ssh_profile(&p.id).unwrap().unwrap().options["ssh.trusted"],
            "[b]:22 SHA256:x"
        );
    }
}
