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

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use dbine_driver::{Error, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const SERVICE: &str = "com.addlayer.dbine";
/// The keychain item holding the vault's key.
const KEY_ITEM: &str = "vault-key";
const AAD: &[u8] = b"dbine-secrets-v1";

/// The keychain service: `DBINE_KEYCHAIN_SERVICE` lets a test instance keep
/// its secrets apart from the real app's (and not trigger its prompts).
fn service() -> &'static str {
    static S: OnceLock<String> = OnceLock::new();
    S.get_or_init(|| std::env::var("DBINE_KEYCHAIN_SERVICE").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| SERVICE.to_string()))
}

pub type Secrets = BTreeMap<String, String>;

fn secrets_err(e: impl std::fmt::Display) -> Error {
    Error::Secrets(e.to_string())
}

// ---- Keychain (one item per name) ----------------------------------------

fn entry(name: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(service(), name).map_err(secrets_err)
}

fn keychain_get(name: &str) -> Result<Option<String>> {
    match entry(name)?.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(secrets_err(e)),
    }
}

fn keychain_set(name: &str, value: &str) -> Result<()> {
    entry(name)?.set_password(value).map_err(secrets_err)
}

fn keychain_delete(name: &str) -> Result<()> {
    match entry(name)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(secrets_err(e)),
    }
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
                    if let Ok(values) = open(&key, &bytes) {
                        return Ok(Self { path, key, values, checked: HashSet::new() });
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
        // An item from before the vault: move it in.
        let old = keychain_get(name)?;
        self.checked.insert(name.to_string());
        if let Some(v) = &old {
            self.values.insert(name.to_string(), v.clone());
            self.save()?;
            let _ = keychain_delete(name);
        }
        Ok(old)
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

    /// Drop the name's old keychain item, if it wasn't looked up yet.
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
        std::env::set_var("DBINE_KEYCHAIN_SERVICE", "com.addlayer.dbine.vault-test");
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

    #[test]
    fn each_write_uses_a_new_nonce() {
        let values = BTreeMap::from([("c1".to_string(), "x".to_string())]);
        assert_ne!(seal(&[1u8; 32], &values).unwrap(), seal(&[1u8; 32], &values).unwrap());
    }
}
