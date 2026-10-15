//! A connection's secret values (password, API keys, service-account
//! JSON…) and the app's own (the cloud backup's passphrase and tokens) never
//! go in the state file.
//!
//! They live in an encrypted vault file next to the state
//! (`set_vault_path`), sealed with XChaCha20-Poly1305 under one random
//! 256-bit key kept in the OS keychain (macOS Keychain, Windows Credential
//! Manager, Secret Service on Linux). One keychain item for the whole app
//! instead of one per connection: macOS asks for access once, not once per
//! connection.
//!
//! Older versions kept one keychain item per connection; those move into the
//! vault the first time they're read (and the item is deleted), so each asks
//! for access at most one last time.
//!
//! Without a vault path (tests, tools) secrets go straight to the keychain,
//! one item per name, as before.
//!
//! The keychain service was the app's old identifier, `com.addlayer.dbine`,
//! up to 0.1.11. An item not found under the current service is looked up
//! under the old one and, when it's there, **copied** to the current one:
//! the old item is never changed or deleted, so an older version installed
//! next to this one keeps working. A read of the old item that fails (a
//! macOS prompt denied) writes nothing: the next start tries again. No
//! migration when `DBINE_KEYCHAIN_SERVICE` is set (a test instance).

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use dbine_driver::{Error, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const SERVICE: &str = "io.addlayer.dbine";
/// The service (and the app's identifier) up to 0.1.11.
const OLD_SERVICE: &str = "com.addlayer.dbine";
/// The keychain item holding the vault's key.
const KEY_ITEM: &str = "vault-key";
const AAD: &[u8] = b"dbine-secrets-v1";

/// The keychain service: `DBINE_KEYCHAIN_SERVICE` lets a test instance keep
/// its secrets apart from the real app's (and not trigger its prompts).
fn service() -> &'static str {
    static S: OnceLock<String> = OnceLock::new();
    S.get_or_init(|| std::env::var("DBINE_KEYCHAIN_SERVICE").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| SERVICE.to_string()))
}

/// Where items of an older version may still be: none with an override.
fn legacy_service() -> Option<&'static str> {
    let overridden = std::env::var("DBINE_KEYCHAIN_SERVICE").ok().is_some_and(|s| !s.is_empty());
    (!overridden).then_some(OLD_SERVICE)
}

pub type Secrets = BTreeMap<String, String>;

fn secrets_err(e: impl std::fmt::Display) -> Error {
    Error::Secrets(e.to_string())
}

// ---- Keychain (one item per name) ----------------------------------------

/// The OS keychain, by service and name (a trait so the migration between
/// services is tested without one).
trait Keychain {
    fn get(&self, service: &str, name: &str) -> Result<Option<String>>;
    fn set(&self, service: &str, name: &str, value: &str) -> Result<()>;
    fn delete(&self, service: &str, name: &str) -> Result<()>;
}

struct OsKeychain;

impl Keychain for OsKeychain {
    fn get(&self, service: &str, name: &str) -> Result<Option<String>> {
        match keyring::Entry::new(service, name).map_err(secrets_err)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(secrets_err(e)),
        }
    }

    fn set(&self, service: &str, name: &str, value: &str) -> Result<()> {
        keyring::Entry::new(service, name).map_err(secrets_err)?.set_password(value).map_err(secrets_err)
    }

    fn delete(&self, service: &str, name: &str) -> Result<()> {
        match keyring::Entry::new(service, name).map_err(secrets_err)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(secrets_err(e)),
        }
    }
}

/// Read `name` under `service`, copying it from `legacy` when only there.
/// The legacy item is only read.
fn get_copying(kc: &dyn Keychain, service: &str, legacy: Option<&str>, name: &str) -> Result<Option<String>> {
    if let Some(v) = kc.get(service, name)? {
        return Ok(Some(v));
    }
    let Some(legacy) = legacy.filter(|l| *l != service) else { return Ok(None) };
    // A failed read (denied) returns here: nothing is written.
    let Some(v) = kc.get(legacy, name)? else { return Ok(None) };
    kc.set(service, name, &v)?;
    Ok(Some(v))
}

fn keychain_get(name: &str) -> Result<Option<String>> {
    get_copying(&OsKeychain, service(), legacy_service(), name)
}

fn keychain_set(name: &str, value: &str) -> Result<()> {
    OsKeychain.set(service(), name, value)
}

/// Under the current service only: the old one belongs to older versions.
fn keychain_delete(name: &str) -> Result<()> {
    OsKeychain.delete(service(), name)
}

/// The app's own vault names carried its identifier (the cloud backup's
/// `com.addlayer.dbine.sync.passphrase` and tokens): the current name of
/// an old one, or `None`.
fn renamed(name: &str) -> Option<String> {
    name.strip_prefix(OLD_SERVICE).filter(|rest| rest.starts_with('.')).map(|rest| format!("{SERVICE}{rest}"))
}

/// The old name of a current one (`renamed`'s inverse).
fn old_name(name: &str) -> Option<String> {
    name.strip_prefix(SERVICE).filter(|rest| rest.starts_with('.')).map(|rest| format!("{OLD_SERVICE}{rest}"))
}

/// In a vault whose old names were copied: once only, so a secret deleted
/// under its current name doesn't come back from the old one.
const NAMES_COPIED: &str = "dbine.vault.names-copied-from-com.addlayer.dbine";

/// Copy the vault's old names to their current one (an existing current one
/// wins), once. The old names stay: a vault an older version still opens
/// (a start that ran on the old folders) keeps working for it. Whether
/// anything changed.
fn copy_old_names(values: &mut BTreeMap<String, String>) -> bool {
    if values.contains_key(NAMES_COPIED) {
        return false;
    }
    let old: Vec<(String, String)> = values.iter().filter_map(|(k, v)| Some((renamed(k)?, v.clone()))).collect();
    for (k, v) in old {
        values.entry(k).or_insert(v);
    }
    values.insert(NAMES_COPIED.to_string(), "1".to_string());
    true
}

// ---- Vault file ------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct Envelope {
    v: u32,
    nonce: String,
    data: String,
}

fn seal(key: &[u8; 32], values: &BTreeMap<String, String>) -> Result<Vec<u8>> {
    let plain = serde_json::to_vec(values)?;
    let mut nonce = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut nonce);
    let data = XChaCha20Poly1305::new(key.into())
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: &plain, aad: AAD })
        .map_err(|_| secrets_err("no se pudieron cifrar las credenciales"))?;
    Ok(serde_json::to_vec(&Envelope { v: 1, nonce: B64.encode(nonce), data: B64.encode(data) })?)
}

fn open(key: &[u8; 32], bytes: &[u8]) -> Result<BTreeMap<String, String>> {
    let env: Envelope = serde_json::from_slice(bytes)?;
    let nonce = B64.decode(env.nonce).map_err(secrets_err)?;
    let data = B64.decode(env.data).map_err(secrets_err)?;
    if env.v != 1 || nonce.len() != 24 {
        return Err(secrets_err("formato de credenciales desconocido"));
    }
    let plain = XChaCha20Poly1305::new(key.into())
        .decrypt(XNonce::from_slice(&nonce), Payload { msg: &data, aad: AAD })
        .map_err(|_| secrets_err("no se pudieron descifrar las credenciales"))?;
    Ok(serde_json::from_slice(&plain)?)
}

/// Write through a temp file, so a crash never leaves half a vault.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(secrets_err)?;
    }
    let tmp = path.with_extension("vault.tmp");
    std::fs::write(&tmp, bytes).map_err(secrets_err)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path).map_err(secrets_err)
}

struct Vault {
    path: PathBuf,
    key: [u8; 32],
    values: BTreeMap<String, String>,
    /// Names already looked up among the old keychain items (and not there).
    checked: HashSet<String>,
}

impl Vault {
    fn load(path: PathBuf) -> Result<Self> {
        let stored = keychain_get(KEY_ITEM)?.and_then(|k| B64.decode(k).ok()).and_then(|k| <[u8; 32]>::try_from(k).ok());
        if let Some(key) = stored {
            match std::fs::read(&path) {
                Ok(bytes) => {
                    if let Ok(mut values) = open(&key, &bytes) {
                        let vault = if copy_old_names(&mut values) {
                            let v = Self { path, key, values, checked: HashSet::new() };
                            v.save()?;
                            v
                        } else {
                            Self { path, key, values, checked: HashSet::new() }
                        };
                        return Ok(vault);
                    }
                    set_aside(&path);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(secrets_err(e)),
            }
            return Ok(Self { path, key, values: BTreeMap::new(), checked: HashSet::new() });
        }
        // No key: a first start, or a vault this keychain can't open (another
        // machine's copy). The unreadable file is kept aside, never erased.
        if path.exists() {
            set_aside(&path);
        }
        let mut key = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut key);
        keychain_set(KEY_ITEM, &B64.encode(key))?;
        Ok(Self { path, key, values: BTreeMap::new(), checked: HashSet::new() })
    }

    fn save(&self) -> Result<()> {
        write_atomic(&self.path, &seal(&self.key, &self.values)?)
    }

    fn get(&mut self, name: &str) -> Result<Option<String>> {
        if let Some(v) = self.values.get(name) {
            return Ok(Some(v.clone()));
        }
        if self.checked.contains(name) {
            return Ok(None);
        }
        // An item from before the vault: move it in (under its old name
        // too, when the name carried the old identifier). One under the old
        // service is only copied (`keychain_get`) and stays for an older
        // version; the copy under the current service is dropped here.
        let mut found = None;
        for item in std::iter::once(name.to_string()).chain(old_name(name)) {
            if let Some(v) = keychain_get(&item)? {
                found = Some((item, v));
                break;
            }
        }
        self.checked.insert(name.to_string());
        let Some((item, v)) = found else { return Ok(None) };
        self.values.insert(name.to_string(), v.clone());
        self.save()?;
        let _ = keychain_delete(&item);
        Ok(Some(v))
    }

    fn set(&mut self, name: &str, value: &str) -> Result<()> {
        self.values.insert(name.to_string(), value.to_string());
        self.save()?;
        self.forget_old(name);
        Ok(())
    }

    fn delete(&mut self, name: &str) -> Result<()> {
        if self.values.remove(name).is_some() {
            self.save()?;
        }
        self.forget_old(name);
        Ok(())
    }

    /// Drop the name's old keychain item, if it wasn't looked up yet (under
    /// the current service only).
    fn forget_old(&mut self, name: &str) {
        if self.checked.insert(name.to_string()) {
            let _ = keychain_delete(name);
        }
    }
}

fn set_aside(path: &Path) {
    let stamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
    let _ = std::fs::rename(path, path.with_extension(format!("vault.unreadable-{stamp}")));
}

static VAULT_PATH: OnceLock<PathBuf> = OnceLock::new();
static VAULT: Mutex<Option<Vault>> = Mutex::new(None);

/// Where the vault lives (at startup, before any secret is read).
pub fn set_vault_path(path: PathBuf) {
    let _ = VAULT_PATH.set(path);
}

/// Run `f` on the vault (opened on first use), or return `None` when there's
/// no vault path.
fn with_vault<T>(f: impl FnOnce(&mut Vault) -> Result<T>) -> Option<Result<T>> {
    let path = VAULT_PATH.get()?;
    let mut guard = VAULT.lock().unwrap_or_else(|e| e.into_inner());
    if guard.is_none() {
        match Vault::load(path.clone()) {
            Ok(v) => *guard = Some(v),
            Err(e) => return Some(Err(e)),
        }
    }
    Some(f(guard.as_mut().expect("vault loaded")))
}

// ---- Public API ------------------------------------------------------------

pub fn set(connection_id: &str, secrets: &Secrets) -> Result<()> {
    if secrets.is_empty() {
        return delete(connection_id);
    }
    set_raw(connection_id, &serde_json::to_string(secrets)?)
}

pub fn get(connection_id: &str) -> Result<Secrets> {
    match get_raw(connection_id)? {
        Some(json) => Ok(serde_json::from_str(&json)?),
        None => Ok(Secrets::new()),
    }
}

pub fn delete(connection_id: &str) -> Result<()> {
    delete_raw(connection_id)
}

/// A secret of the app's own (not a connection's): the cloud backup's
/// passphrase and account tokens.
pub fn get_raw(name: &str) -> Result<Option<String>> {
    with_vault(|v| v.get(name)).unwrap_or_else(|| keychain_get(name))
}

pub fn set_raw(name: &str, value: &str) -> Result<()> {
    with_vault(|v| v.set(name, value)).unwrap_or_else(|| keychain_set(name, value))
}

pub fn delete_raw(name: &str) -> Result<()> {
    with_vault(|v| v.delete(name)).unwrap_or_else(|| keychain_delete(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_and_open_round_trip() {
        let key = [7u8; 32];
        let mut values = BTreeMap::new();
        values.insert("c1".to_string(), r#"{"password":"s3cr3t"}"#.to_string());
        values.insert("sync-passphrase".to_string(), "frase larga".to_string());
        let bytes = seal(&key, &values).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("s3cr3t"));
        assert_eq!(open(&key, &bytes).unwrap(), values);
    }

    #[test]
    fn wrong_key_or_tampered_file_fails() {
        let values = BTreeMap::from([("c1".to_string(), "x".to_string())]);
        let bytes = seal(&[1u8; 32], &values).unwrap();
        assert!(open(&[2u8; 32], &bytes).is_err());
        let mut env: Envelope = serde_json::from_slice(&bytes).unwrap();
        let mut data = B64.decode(&env.data).unwrap();
        data[0] ^= 1;
        env.data = B64.encode(data);
        assert!(open(&[1u8; 32], &serde_json::to_vec(&env).unwrap()).is_err());
    }

    /// Against the real keychain, under a service of its own:
    /// `cargo test -p dbine-core vault_with_keychain -- --ignored`.
    #[test]
    #[ignore]
    fn vault_with_keychain() {
        std::env::set_var("DBINE_KEYCHAIN_SERVICE", "io.addlayer.dbine.vault-test");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.vault");
        let _ = keychain_delete(KEY_ITEM);

        // An item from before the vault moves in on first read.
        keychain_set("old-conn", r#"{"password":"viejo"}"#).unwrap();
        let mut v = Vault::load(path.clone()).unwrap();
        assert_eq!(v.get("old-conn").unwrap().as_deref(), Some(r#"{"password":"viejo"}"#));
        assert_eq!(keychain_get("old-conn").unwrap(), None);
        v.set("new-conn", "nuevo").unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("nuevo"));

        // Reopened with the key from the keychain.
        let mut v = Vault::load(path.clone()).unwrap();
        assert_eq!(v.get("new-conn").unwrap().as_deref(), Some("nuevo"));
        v.delete("new-conn").unwrap();
        assert_eq!(Vault::load(path.clone()).unwrap().get("new-conn").unwrap(), None);

        // Without its key the file is set aside, not erased.
        keychain_delete(KEY_ITEM).unwrap();
        let v = Vault::load(path.clone()).unwrap();
        assert!(v.values.is_empty());
        assert!(std::fs::read_dir(dir.path()).unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains("unreadable")));
        keychain_delete(KEY_ITEM).unwrap();
    }

    /// An in-memory keychain; `deny` makes reads under a service fail (a
    /// macOS prompt the user denied), `fail_set` makes writes fail.
    #[derive(Default)]
    struct Mock {
        items: std::cell::RefCell<BTreeMap<(String, String), String>>,
        deny: Option<&'static str>,
        fail_set: bool,
    }

    impl Mock {
        fn with(items: &[(&str, &str, &str)]) -> Self {
            let m = Mock::default();
            for (s, n, v) in items {
                m.items.borrow_mut().insert((s.to_string(), n.to_string()), v.to_string());
            }
            m
        }
        fn has(&self, service: &str, name: &str) -> bool {
            self.items.borrow().contains_key(&(service.to_string(), name.to_string()))
        }
    }

    impl Keychain for Mock {
        fn get(&self, service: &str, name: &str) -> Result<Option<String>> {
            if self.deny == Some(service) {
                return Err(secrets_err("denied"));
            }
            Ok(self.items.borrow().get(&(service.to_string(), name.to_string())).cloned())
        }
        fn set(&self, service: &str, name: &str, value: &str) -> Result<()> {
            if self.fail_set {
                return Err(secrets_err("write failed"));
            }
            self.items.borrow_mut().insert((service.to_string(), name.to_string()), value.to_string());
            Ok(())
        }
        fn delete(&self, service: &str, name: &str) -> Result<()> {
            self.items.borrow_mut().remove(&(service.to_string(), name.to_string()));
            Ok(())
        }
    }

    #[test]
    fn old_service_item_is_copied_and_kept() {
        let kc = Mock::with(&[(OLD_SERVICE, KEY_ITEM, "k"), (OLD_SERVICE, "conn-1", "p")]);
        for name in [KEY_ITEM, "conn-1"] {
            let v = get_copying(&kc, SERVICE, Some(OLD_SERVICE), name).unwrap();
            assert!(v.is_some());
            assert!(kc.has(SERVICE, name));
            // An older version still finds it.
            assert!(kc.has(OLD_SERVICE, name));
        }
        assert_eq!(get_copying(&kc, SERVICE, Some(OLD_SERVICE), KEY_ITEM).unwrap().as_deref(), Some("k"));
        assert_eq!(get_copying(&kc, SERVICE, Some(OLD_SERVICE), "missing").unwrap(), None);
        assert!(!kc.has(SERVICE, "missing"));
    }

    #[test]
    fn current_service_wins() {
        let kc = Mock::with(&[(SERVICE, "n", "new"), (OLD_SERVICE, "n", "old")]);
        assert_eq!(get_copying(&kc, SERVICE, Some(OLD_SERVICE), "n").unwrap().as_deref(), Some("new"));
        assert_eq!(kc.items.borrow()[&(OLD_SERVICE.to_string(), "n".to_string())], "old");
    }

    #[test]
    fn denied_old_read_writes_nothing_and_retries() {
        let mut kc = Mock::with(&[(OLD_SERVICE, KEY_ITEM, "k")]);
        kc.deny = Some(OLD_SERVICE);
        assert!(get_copying(&kc, SERVICE, Some(OLD_SERVICE), KEY_ITEM).is_err());
        assert!(!kc.has(SERVICE, KEY_ITEM));
        assert!(kc.has(OLD_SERVICE, KEY_ITEM));
        // Allowed on the next start.
        kc.deny = None;
        assert_eq!(get_copying(&kc, SERVICE, Some(OLD_SERVICE), KEY_ITEM).unwrap().as_deref(), Some("k"));
        assert!(kc.has(SERVICE, KEY_ITEM) && kc.has(OLD_SERVICE, KEY_ITEM));
    }

    #[test]
    fn failed_write_keeps_the_old_item() {
        let mut kc = Mock::with(&[(OLD_SERVICE, KEY_ITEM, "k")]);
        kc.fail_set = true;
        assert!(get_copying(&kc, SERVICE, Some(OLD_SERVICE), KEY_ITEM).is_err());
        assert!(kc.has(OLD_SERVICE, KEY_ITEM));
        assert!(!kc.has(SERVICE, KEY_ITEM));
    }

    #[test]
    fn no_migration_with_an_override() {
        // `legacy_service()` is `None` under DBINE_KEYCHAIN_SERVICE.
        let kc = Mock::with(&[(OLD_SERVICE, KEY_ITEM, "k")]);
        assert_eq!(get_copying(&kc, "io.addlayer.dbine.e2etest", None, KEY_ITEM).unwrap(), None);
        assert!(kc.has(OLD_SERVICE, KEY_ITEM));
        assert!(!kc.has("io.addlayer.dbine.e2etest", KEY_ITEM));
    }

    #[test]
    fn vault_names_with_the_old_identifier_are_copied_once() {
        let mut values = BTreeMap::from([
            ("com.addlayer.dbine.sync.passphrase".to_string(), "frase".to_string()),
            ("com.addlayer.dbine.sync.token.gdrive".to_string(), "t-old".to_string()),
            ("io.addlayer.dbine.sync.token.gdrive".to_string(), "t-new".to_string()),
            ("com.addlayer.dbine.e2etest.sync.passphrase".to_string(), "e2e".to_string()),
            ("c1".to_string(), "conn".to_string()),
            ("smtp-password".to_string(), "mail".to_string()),
        ]);
        assert!(copy_old_names(&mut values));
        assert_eq!(values["io.addlayer.dbine.sync.passphrase"], "frase");
        assert_eq!(values["io.addlayer.dbine.sync.token.gdrive"], "t-new");
        assert_eq!(values["io.addlayer.dbine.e2etest.sync.passphrase"], "e2e");
        assert_eq!(values["c1"], "conn");
        assert_eq!(values["smtp-password"], "mail");
        // The old names stay for an older version that opens this vault.
        assert_eq!(values["com.addlayer.dbine.sync.passphrase"], "frase");
        assert_eq!(values["com.addlayer.dbine.sync.token.gdrive"], "t-old");
        // Once: a secret deleted under its new name doesn't come back.
        values.remove("io.addlayer.dbine.sync.passphrase");
        assert!(!copy_old_names(&mut values));
        assert!(!values.contains_key("io.addlayer.dbine.sync.passphrase"));
        assert_eq!(old_name("io.addlayer.dbine.sync.passphrase").as_deref(), Some("com.addlayer.dbine.sync.passphrase"));
        assert_eq!(old_name("c1"), None);
        assert_eq!(renamed("com.addlayer.dbinex"), None);
        assert_eq!(renamed(NAMES_COPIED), None);
    }

    #[test]
    fn the_old_vault_still_opens_for_an_older_version() {
        // 0.1.11's vault and key; the new app opens a copy of the file with
        // the key copied to its own service and adds its names.
        let key = [9u8; 32];
        let old_values = BTreeMap::from([
            ("c1".to_string(), r#"{"password":"p"}"#.to_string()),
            ("com.addlayer.dbine.sync.passphrase".to_string(), "frase".to_string()),
        ]);
        let old_file = seal(&key, &old_values).unwrap();
        let mut copy = open(&key, &old_file.clone()).unwrap();
        assert!(copy_old_names(&mut copy));
        let _new_file = seal(&key, &copy).unwrap();
        // The old file is a different file, never written: 0.1.11 reads it as before.
        assert_eq!(open(&key, &old_file).unwrap(), old_values);
        // Even a vault the new app rewrote (a start on the old folders) keeps
        // what 0.1.11 looks up.
        assert_eq!(copy["c1"], old_values["c1"]);
        assert_eq!(copy["com.addlayer.dbine.sync.passphrase"], "frase");
    }

    /// The copy between services against the real keychain, with test
    /// services: `cargo test -p dbine-core service_copy_with_keychain -- --ignored`.
    #[test]
    #[ignore]
    fn service_copy_with_keychain() {
        let (old, new) = ("io.addlayer.dbine.migtest-old", "io.addlayer.dbine.migtest-new");
        let kc = OsKeychain;
        let _ = kc.delete(new, KEY_ITEM);
        kc.set(old, KEY_ITEM, "clave").unwrap();
        assert_eq!(get_copying(&kc, new, Some(old), KEY_ITEM).unwrap().as_deref(), Some("clave"));
        assert_eq!(kc.get(old, KEY_ITEM).unwrap().as_deref(), Some("clave"));
        assert_eq!(kc.get(new, KEY_ITEM).unwrap().as_deref(), Some("clave"));
        kc.delete(new, KEY_ITEM).unwrap();
        kc.delete(old, KEY_ITEM).unwrap();
        assert_eq!(get_copying(&kc, new, Some(old), KEY_ITEM).unwrap(), None);
    }

    #[test]
    fn each_write_uses_a_new_nonce() {
        let values = BTreeMap::from([("c1".to_string(), "x".to_string())]);
        assert_ne!(seal(&[1u8; 32], &values).unwrap(), seal(&[1u8; 32], &values).unwrap());
    }
}
