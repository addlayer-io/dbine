//! Secret-manager accounts (docs/secret-sources.md, "Accounts"): what signs
//! DBine in to a secret manager, set up once on this computer and used by
//! any number of connections, whose source definitions name the account
//! (`"account": "<id>"`) instead of carrying the sign-in themselves.
//!
//! - The non-secret part (kind, label, the account's e-mail, the data centre,
//!   the client ID, where the master passphrase comes from) is a row of the
//!   state DB's `secret_accounts` table. That table is this machine's only:
//!   it isn't in the snapshot, so no cloud backup, export or restore carries
//!   it.
//! - The secrets (a client secret, a refresh token, a master passphrase kept
//!   in the keychain) are in the system keychain under
//!   `secret_account:<id>#<slot>` ([`secret_key`]), never in a connection's
//!   own secrets: the backup of a connection's secrets doesn't reach them.
//!
//! Only Zoho Vault has accounts for now; [`AccountConfig`] is tagged by kind
//! so other managers add a variant.

use crate::secret_sources::MasterSource;
use crate::state::StateStore;
use dbine_driver::{Error, Result};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

/// The keychain prefix of an account's secrets.
pub const SECRET_PREFIX: &str = "secret_account:";

/// The keychain name of one of an account's secrets
/// (`secret_account:<id>#client_secret`).
pub fn secret_key(account_id: &str, slot: &str) -> String {
    format!("{SECRET_PREFIX}{account_id}#{slot}")
}

/// Every slot an account may have in the keychain (deleting one clears them all).
pub const ALL_SLOTS: &[&str] = &["client_secret", "refresh_token", "passphrase"];

fn is_keychain(m: &MasterSource) -> bool {
    *m == MasterSource::Keychain
}

/// What an account signs in with, by kind. Secrets aren't here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AccountConfig {
    /// A Zoho Vault user, through the user's own OAuth Self Client.
    Zoho {
        /// `com`, `eu`, `in`… (crate::secret_sources::ZOHO_DATA_CENTERS),
        /// found by DBine when the account was connected.
        data_center: String,
        /// The Self Client's ID (not secret).
        client_id: String,
        /// Where the master passphrase comes from: the keychain (the
        /// `passphrase` slot), asked when connecting, or a command.
        #[serde(default, skip_serializing_if = "is_keychain")]
        master: MasterSource,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        master_command: String,
    },
}

impl AccountConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Zoho { .. } => "zoho",
        }
    }

    /// The keychain slots this account uses as set now.
    pub fn slots(&self) -> &'static [&'static str] {
        match self {
            Self::Zoho { master: MasterSource::Keychain, .. } => &["client_secret", "refresh_token", "passphrase"],
            Self::Zoho { .. } => &["client_secret", "refresh_token"],
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Zoho { data_center, client_id, master, master_command } => {
                if crate::secret_sources::zoho_hosts(data_center).is_none() {
                    return Err(bad(format!("el centro de datos de Zoho «{}» no es válido", data_center.trim())));
                }
                if !valid_zoho_client_id(client_id) {
                    return Err(bad("falta el ID del cliente de Zoho (1000.XXXX)"));
                }
                match master {
                    MasterSource::None => return Err(bad("Zoho Vault necesita la frase de contraseña maestra")),
                    MasterSource::Command if master_command.trim().is_empty() => return Err(bad("falta el comando que devuelve la contraseña maestra")),
                    _ => {}
                }
                if master_command.contains(['\0', '\n', '\r']) {
                    return Err(bad("el comando tiene que ser una sola línea"));
                }
            }
        }
        Ok(())
    }
}

/// A Zoho client ID as the API console shows it (`1000.XXXX`).
pub fn valid_zoho_client_id(id: &str) -> bool {
    let id = id.trim();
    !id.is_empty() && id.len() <= 200 && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// One account, as the state DB keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretAccount {
    pub id: String,
    /// The name the user sees ("Zoho · ana@example.com").
    pub label: String,
    /// The account's e-mail or user name, as the manager reported it ('' if
    /// it didn't).
    #[serde(default)]
    pub identity: String,
    #[serde(flatten)]
    pub config: AccountConfig,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

fn bad(msg: impl Into<String>) -> Error {
    Error::State(msg.into())
}

fn db_err(e: rusqlite::Error) -> Error {
    Error::State(e.to_string())
}

/// An account's ID: letters, digits and `-` (DBine makes UUIDs).
pub fn valid_account_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// A label: some text, one line, at most 200 characters.
pub fn check_label(label: &str) -> Result<()> {
    let l = label.trim();
    if l.is_empty() {
        return Err(bad("falta el nombre de la cuenta"));
    }
    if l.chars().count() > 200 || l.chars().any(char::is_control) {
        return Err(bad("el nombre de la cuenta no es válido"));
    }
    Ok(())
}

impl SecretAccount {
    pub fn validate(&self) -> Result<()> {
        if !valid_account_id(&self.id) {
            return Err(bad("el ID de la cuenta no es válido"));
        }
        check_label(&self.label)?;
        if self.identity.len() > 320 || self.identity.chars().any(char::is_control) {
            return Err(bad("el usuario de la cuenta no es válido"));
        }
        self.config.validate()
    }
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

type Row = (String, String, String, String, String, String);

fn from_row((id, label, identity, config, created_at, updated_at): Row) -> Result<SecretAccount> {
    let config: AccountConfig = serde_json::from_str(&config).map_err(|e| bad(format!("la cuenta «{label}» no se pudo leer: {e}")))?;
    Ok(SecretAccount { id, label, identity, config, created_at, updated_at })
}

const COLUMNS: &str = "id, label, identity, config_json, created_at, updated_at";

impl StateStore {
    /// The accounts, by label.
    pub fn list_secret_accounts(&self) -> Result<Vec<SecretAccount>> {
        let rows: Vec<Row> = {
            let c = self.lock()?;
            let mut st = c.prepare(&format!("SELECT {COLUMNS} FROM secret_accounts ORDER BY label COLLATE NOCASE, created_at")).map_err(db_err)?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).map_err(db_err)?;
            rows.collect::<rusqlite::Result<_>>().map_err(db_err)?
        };
        // One that can't be read (a newer version's kind) is left out.
        Ok(rows.into_iter().filter_map(|r| from_row(r).ok()).collect())
    }

    pub fn get_secret_account(&self, id: &str) -> Result<Option<SecretAccount>> {
        let row: Option<Row> = self
            .lock()?
            .query_row(&format!("SELECT {COLUMNS} FROM secret_accounts WHERE id = ?1"), [id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
            .optional()
            .map_err(db_err)?;
        row.map(from_row).transpose()
    }

    /// Insert or replace (by ID); `created_at` stays as first saved.
    pub fn save_secret_account(&self, a: &SecretAccount) -> Result<SecretAccount> {
        a.validate()?;
        let ts = now();
        let config = serde_json::to_string(&a.config)?;
        let c = self.lock()?;
        c.execute(
            "INSERT INTO secret_accounts (id, kind, label, identity, config_json, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
             ON CONFLICT(id) DO UPDATE SET kind = ?2, label = ?3, identity = ?4, config_json = ?5, updated_at = ?6",
            params![a.id, a.config.kind(), a.label.trim(), a.identity.trim(), config, ts],
        )
        .map_err(db_err)?;
        drop(c);
        self.get_secret_account(&a.id)?.ok_or_else(|| bad("la cuenta no se guardó"))
    }

    pub fn rename_secret_account(&self, id: &str, label: &str) -> Result<SecretAccount> {
        check_label(label)?;
        let n = self.lock()?.execute("UPDATE secret_accounts SET label = ?2, updated_at = ?3 WHERE id = ?1", params![id, label.trim(), now()]).map_err(db_err)?;
        if n == 0 {
            return Err(bad(format!("no existe la cuenta «{id}»")));
        }
        self.get_secret_account(id)?.ok_or_else(|| bad("la cuenta no se guardó"))
    }

    /// Remove the row (its keychain items: [`delete_secrets`]). Whether it existed.
    pub fn delete_secret_account(&self, id: &str) -> Result<bool> {
        Ok(self.lock()?.execute("DELETE FROM secret_accounts WHERE id = ?1", [id]).map_err(db_err)? > 0)
    }
}

// ---- keychain ------------------------------------------------------------------

pub fn get_secret(account_id: &str, slot: &str) -> Result<Option<String>> {
    crate::secrets::get_raw(&secret_key(account_id, slot))
}

pub fn set_secret(account_id: &str, slot: &str, value: &str) -> Result<()> {
    crate::secrets::set_raw(&secret_key(account_id, slot), value)
}

pub fn delete_secret(account_id: &str, slot: &str) -> Result<()> {
    crate::secrets::delete_raw(&secret_key(account_id, slot))
}

/// Delete every keychain item of an account.
pub fn delete_secrets(account_id: &str) -> Result<()> {
    for slot in ALL_SLOTS {
        delete_secret(account_id, slot)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zoho(id: &str, label: &str) -> SecretAccount {
        SecretAccount {
            id: id.into(),
            label: label.into(),
            identity: "ana@example.com".into(),
            config: AccountConfig::Zoho { data_center: "eu".into(), client_id: "1000.ABCDEF".into(), master: MasterSource::Keychain, master_command: String::new() },
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    /// Saved, listed, renamed and removed; the row has the metadata only,
    /// and nothing of it is in the snapshot (backups, exports).
    #[test]
    fn accounts_round_trip_without_secrets_in_the_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sqlite");
        let store = StateStore::open(&path).unwrap();
        let a = store.save_secret_account(&zoho("6f1c2d4e-0000-4000-8000-000000000001", "Zoho · trabajo")).unwrap();
        assert!(!a.created_at.is_empty());
        let mut b = zoho("6f1c2d4e-0000-4000-8000-000000000002", "Ana personal");
        b.config = AccountConfig::Zoho { data_center: "com".into(), client_id: "1000.B".into(), master: MasterSource::Command, master_command: "pass show zoho".into() };
        store.save_secret_account(&b).unwrap();
        let all = store.list_secret_accounts().unwrap();
        assert_eq!(all.iter().map(|a| a.label.as_str()).collect::<Vec<_>>(), ["Ana personal", "Zoho · trabajo"], "by label");
        assert_eq!(store.get_secret_account(&a.id).unwrap().unwrap().config, a.config);
        // Renaming keeps the rest; saving again keeps created_at.
        let r = store.rename_secret_account(&a.id, "  Zoho Vault  ").unwrap();
        assert_eq!((r.label.as_str(), r.created_at.as_str()), ("Zoho Vault", a.created_at.as_str()));
        assert!(store.rename_secret_account(&a.id, " ").is_err());
        assert!(store.rename_secret_account("missing", "x").is_err());
        let again = store.save_secret_account(&r).unwrap();
        assert_eq!(again.created_at, a.created_at);
        // The JSON a row keeps: no secret slot, and the default master left out.
        let json = serde_json::to_string(&again).unwrap();
        assert_eq!(serde_json::from_str::<SecretAccount>(&json).unwrap(), again);
        assert!(json.contains("\"kind\":\"zoho\"") && !json.contains("master") && !json.contains("secret\"") && !json.contains("refresh"), "{json}");
        // Not in what backups and exports carry.
        let snap = serde_json::to_string(&store.snapshot().unwrap()).unwrap();
        assert!(!snap.contains("1000.ABCDEF") && !snap.contains(&a.id) && !snap.contains("Zoho Vault"), "{snap}");
        let rev = store.revision().unwrap();
        store.save_secret_account(&b).unwrap();
        assert_eq!(store.revision().unwrap(), rev, "not a change a backup has to carry");
        // Removed.
        assert!(store.delete_secret_account(&a.id).unwrap());
        assert!(!store.delete_secret_account(&a.id).unwrap());
        assert!(store.get_secret_account(&a.id).unwrap().is_none());
        // Reopened: still there (a file on disk).
        drop(store);
        let store = StateStore::open(&path).unwrap();
        assert_eq!(store.list_secret_accounts().unwrap().len(), 1);
    }

    #[test]
    fn what_an_account_may_be() {
        let store = StateStore::open_in_memory().unwrap();
        for bad_id in ["", "a/b", "../x", "id with space", &"x".repeat(65)] {
            assert!(store.save_secret_account(&zoho(bad_id, "x")).is_err(), "{bad_id}");
        }
        assert!(store.save_secret_account(&zoho("ok-1", "")).is_err(), "a label");
        assert!(store.save_secret_account(&zoho("ok-1", "a\nb")).is_err());
        let mut z = zoho("ok-1", "x");
        z.config = AccountConfig::Zoho { data_center: "evil.com".into(), client_id: "1000.A".into(), master: MasterSource::Keychain, master_command: String::new() };
        assert!(store.save_secret_account(&z).is_err(), "only Zoho's data centres");
        z.config = AccountConfig::Zoho { data_center: "eu".into(), client_id: "1000.A".into(), master: MasterSource::None, master_command: String::new() };
        assert!(store.save_secret_account(&z).is_err(), "Zoho needs the passphrase");
        z.config = AccountConfig::Zoho { data_center: "eu".into(), client_id: "1000.A".into(), master: MasterSource::Command, master_command: String::new() };
        assert!(store.save_secret_account(&z).is_err(), "the command is missing");
        z.config = AccountConfig::Zoho { data_center: "eu".into(), client_id: "1000 A".into(), master: MasterSource::Ask, master_command: String::new() };
        assert!(store.save_secret_account(&z).is_err());
    }

    #[test]
    fn keychain_keys_and_slots() {
        assert_eq!(secret_key("6f1c", "client_secret"), "secret_account:6f1c#client_secret");
        assert_eq!(secret_key("6f1c", "refresh_token"), "secret_account:6f1c#refresh_token");
        assert_eq!(secret_key("6f1c", "passphrase"), "secret_account:6f1c#passphrase");
        // Never a connection's: those are its ID, or `source_token:` inside it.
        assert!(!secret_key("x", "passphrase").starts_with(crate::secret_sources::TOKEN_PREFIX));
        let kept = zoho("a", "x").config;
        assert_eq!(kept.slots(), &["client_secret", "refresh_token", "passphrase"]);
        let asked = AccountConfig::Zoho { data_center: "eu".into(), client_id: "1000.A".into(), master: MasterSource::Ask, master_command: String::new() };
        assert_eq!(asked.slots(), &["client_secret", "refresh_token"], "an asked passphrase is never kept");
        for s in kept.slots() {
            assert!(ALL_SLOTS.contains(s));
        }
    }
}
