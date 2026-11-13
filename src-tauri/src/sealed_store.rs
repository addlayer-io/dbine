//! Small encrypted files for what the UI keeps between sessions that can
//! quote database content: the AI assistant's conversation and its history.
//! Nothing of a database is kept on disk in the clear.
//!
//! - One file per [`Slot`], in the vault's folder (`sealed/`). The UI names
//!   a slot, never a path.
//! - AES-256-GCM (AWS-LC), with a random nonce on every write and the slot's
//!   name as associated data, so a file copied over another slot's doesn't
//!   open.
//! - The key is 256 random bits, created once and kept in DBine's vault
//!   (`sealed-store-key`). It is read only from the vault, never from an
//!   older keychain item of that name. Without a vault nothing is written:
//!   the UI keeps the data in memory and says so.
//! - A file this key can't open (the vault was reset, or the file was
//!   changed) is deleted and reads as empty.

use crate::error::{CommandError, CommandResult};
use aws_lc_rs::aead::{Aad, Nonce, RandomizedNonceKey, AES_256_GCM, NONCE_LEN};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The vault item holding the key.
const KEY_NAME: &str = "sealed-store-key";
/// What every file starts with (the format's version).
const MAGIC: &[u8; 4] = b"DBS1";
/// The most a slot may hold.
pub const MAX_BYTES: usize = 32 * 1024 * 1024;
/// AES-GCM's tag.
const TAG_LEN: usize = 16;

/// What can be kept. Each one is a fixed file: the UI can't name a path.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Slot {
    /// The assistant's current conversation.
    AiConversation,
    /// The assistant's past conversations ("Historial").
    AiHistory,
}

impl Slot {
    fn name(self) -> &'static str {
        match self {
            Slot::AiConversation => "ai-conversation",
            Slot::AiHistory => "ai-history",
        }
    }

    fn file(self) -> String {
        format!("{}.sealed", self.name())
    }

    fn aad(self) -> Vec<u8> {
        format!("dbine-sealed-v1:{}", self.name()).into_bytes()
    }
}

/// One read or write at a time, and one key created at most.
static LOCK: Mutex<()> = Mutex::new(());

fn dir() -> Option<PathBuf> {
    dbine_core::secrets::vault_dir().map(|d| d.join("sealed"))
}

enum Key {
    /// The vault couldn't be read: nothing is touched.
    Unavailable,
    /// The vault has no key (yet).
    Missing,
    Ready([u8; 32]),
}

fn parse_key(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(k)
}

/// The key from the vault; with `create`, a new one when there's none.
fn key(create: bool) -> Key {
    match dbine_core::secrets::get_vault_only(KEY_NAME) {
        Ok(Some(hex)) => match parse_key(&hex) {
            Some(k) => Key::Ready(k),
            None if create => new_key(),
            None => Key::Missing,
        },
        Ok(None) if create => new_key(),
        Ok(None) => Key::Missing,
        Err(_) => Key::Unavailable,
    }
}

/// 256 bits from the OS generator, kept in the vault as hex.
fn new_key() -> Key {
    let mut k = [0u8; 32];
    if getrandom::fill(&mut k).is_err() {
        return Key::Unavailable;
    }
    let hex: String = k.iter().map(|b| format!("{b:02x}")).collect();
    match dbine_core::secrets::set_raw(KEY_NAME, &hex) {
        Ok(()) => Key::Ready(k),
        Err(_) => Key::Unavailable,
    }
}

fn io_err(e: std::io::Error) -> CommandError {
    CommandError::Internal(format!("no se pudo guardar el dato cifrado: {e}"))
}

fn seal(key: &[u8; 32], slot: Slot, value: &str) -> CommandResult<Vec<u8>> {
    let k = RandomizedNonceKey::new(&AES_256_GCM, key)
        .map_err(|_| CommandError::Internal("no se pudo cifrar el dato".into()))?;
    let mut body = value.as_bytes().to_vec();
    let nonce = k
        .seal_in_place_append_tag(Aad::from(slot.aad()), &mut body)
        .map_err(|_| CommandError::Internal("no se pudo cifrar el dato".into()))?;
    let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + body.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(nonce.as_ref());
    out.extend_from_slice(&body);
    Ok(out)
}

/// `None`: not a file of this slot sealed with this key.
fn open(key: &[u8; 32], slot: Slot, bytes: &[u8]) -> Option<String> {
    let rest = bytes.strip_prefix(MAGIC)?;
    if rest.len() < NONCE_LEN + TAG_LEN {
        return None;
    }
    let (nonce, body) = rest.split_at(NONCE_LEN);
    let k = RandomizedNonceKey::new(&AES_256_GCM, key).ok()?;
    let nonce = Nonce::try_assume_unique_for_key(nonce).ok()?;
    let mut body = body.to_vec();
    let plain = k
        .open_in_place(nonce, Aad::from(slot.aad()), &mut body)
        .ok()?;
    String::from_utf8(plain.to_vec()).ok()
}

/// Write `bytes` to `path` through a temporary file in the same folder,
/// readable only by the user.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

fn create_dir(dir: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(dir)
}

fn remove(path: &Path) {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            tracing::warn!(%e, "sealed store: couldn't delete an unreadable file");
        }
        _ => {}
    }
}

/// What `slot` holds in `dir`; a file `key` can't open is deleted.
fn read_in(dir: &Path, key: &[u8; 32], slot: Slot) -> Option<String> {
    let path = dir.join(slot.file());
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(_) => return None,
    };
    let value = if bytes.len() <= MAGIC.len() + NONCE_LEN + MAX_BYTES + TAG_LEN {
        open(key, slot, &bytes)
    } else {
        None
    };
    if value.is_none() {
        tracing::warn!(
            slot = slot.name(),
            "sealed store: a file didn't open, deleted"
        );
        remove(&path);
    }
    value
}

fn write_in(dir: &Path, key: &[u8; 32], slot: Slot, value: &str) -> CommandResult<()> {
    if value.len() > MAX_BYTES {
        return Err(CommandError::BadRequest(
            "el dato es demasiado grande para guardarlo".into(),
        ));
    }
    create_dir(dir).map_err(io_err)?;
    write_atomic(&dir.join(slot.file()), &seal(key, slot, value)?).map_err(io_err)
}

/// What a read finds.
#[derive(Serialize, Debug, Default)]
pub struct Read {
    /// `false`: there's no vault to keep anything with; the UI keeps it in
    /// memory and says so.
    pub available: bool,
    /// `None`: nothing kept.
    pub value: Option<String>,
}

/// What `slot` holds.
pub fn get(slot: Slot) -> Read {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(dir) = dir() else {
        return Read::default();
    };
    match key(false) {
        Key::Ready(k) => Read {
            available: true,
            value: read_in(&dir, &k, slot),
        },
        // Without a key no file can open: whatever is there is left over
        // from a vault that was reset. The first write creates the key.
        Key::Missing => {
            remove(&dir.join(slot.file()));
            Read {
                available: true,
                value: None,
            }
        }
        Key::Unavailable => Read::default(),
    }
}

/// Keep `value` in `slot`. `false`: there's no vault, nothing was written.
pub fn set(slot: Slot, value: &str) -> CommandResult<bool> {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let Some(dir) = dir() else {
        return Ok(false);
    };
    match key(true) {
        Key::Ready(k) => write_in(&dir, &k, slot, value).map(|()| true),
        Key::Missing | Key::Unavailable => Ok(false),
    }
}

/// Forget what `slot` holds.
pub fn delete(slot: Slot) {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(dir) = dir() {
        remove(&dir.join(slot.file()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh random key: tests never carry a fixed cryptographic key.
    fn random_key() -> [u8; 32] {
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).unwrap();
        k
    }

    fn temp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "dbine-sealed-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        create_dir(&d).unwrap();
        d
    }

    #[test]
    fn round_trip_and_no_plaintext_on_disk() {
        let dir = temp_dir();
        let key = random_key();
        let value = r#"[{"role":"user","content":"SELECT * FROM clientes WHERE email = 'ana@example.com'"}]"#;
        write_in(&dir, &key, Slot::AiHistory, value).unwrap();
        let raw = std::fs::read(dir.join(Slot::AiHistory.file())).unwrap();
        assert!(raw.starts_with(MAGIC));
        assert!(!String::from_utf8_lossy(&raw).contains("ana@example.com"));
        assert_eq!(read_in(&dir, &key, Slot::AiHistory).as_deref(), Some(value));
        // Two writes of the same value differ (a fresh nonce each time).
        write_in(&dir, &key, Slot::AiHistory, value).unwrap();
        assert_ne!(
            raw,
            std::fs::read(dir.join(Slot::AiHistory.file())).unwrap()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn another_key_a_changed_byte_or_another_slot_reads_empty_and_deletes() {
        let dir = temp_dir();
        let key = random_key();
        // Another key.
        write_in(&dir, &key, Slot::AiConversation, "hola").unwrap();
        assert_eq!(read_in(&dir, &random_key(), Slot::AiConversation), None);
        assert!(!dir.join(Slot::AiConversation.file()).exists());
        // A changed byte.
        write_in(&dir, &key, Slot::AiConversation, "hola").unwrap();
        let path = dir.join(Slot::AiConversation.file());
        let mut raw = std::fs::read(&path).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 1;
        std::fs::write(&path, &raw).unwrap();
        assert_eq!(read_in(&dir, &key, Slot::AiConversation), None);
        assert!(!path.exists());
        // One slot's file copied over another's.
        write_in(&dir, &key, Slot::AiHistory, "historial").unwrap();
        std::fs::copy(dir.join(Slot::AiHistory.file()), &path).unwrap();
        assert_eq!(read_in(&dir, &key, Slot::AiConversation), None);
        assert_eq!(
            read_in(&dir, &key, Slot::AiHistory).as_deref(),
            Some("historial")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn short_or_foreign_files_and_oversized_values_are_refused() {
        let key = random_key();
        assert_eq!(open(&key, Slot::AiHistory, b""), None);
        assert_eq!(open(&key, Slot::AiHistory, b"DBS1short"), None);
        assert_eq!(open(&key, Slot::AiHistory, b"[{\"role\":\"user\"}]"), None);
        let dir = temp_dir();
        let big = "x".repeat(MAX_BYTES + 1);
        assert!(write_in(&dir, &key, Slot::AiHistory, &big).is_err());
        assert!(!dir.join(Slot::AiHistory.file()).exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn only_the_user_can_read_the_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir().join("sealed");
        write_in(&dir, &random_key(), Slot::AiHistory, "x").unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(Slot::AiHistory.file())), 0o600);
        std::fs::remove_dir_all(dir.parent().unwrap()).unwrap();
    }

    #[test]
    fn the_ui_names_only_known_slots() {
        let ok: Slot = serde_json::from_str("\"ai_history\"").unwrap();
        assert_eq!(ok, Slot::AiHistory);
        assert!(serde_json::from_str::<Slot>("\"../vault\"").is_err());
        assert!(serde_json::from_str::<Slot>("\"ai-history\"").is_err());
    }

    #[test]
    fn keys_parse_only_as_64_hex_digits() {
        assert!(parse_key(&"ab".repeat(32)).is_some());
        assert!(parse_key(&"ab".repeat(31)).is_none());
        assert!(parse_key(&"zz".repeat(32)).is_none());
    }
}
