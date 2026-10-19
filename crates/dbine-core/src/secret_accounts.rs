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
//! Every secret manager has accounts ([`AccountConfig`], tagged by kind):
//! Zoho Vault, Bitwarden, Bitwarden Secrets Manager, HashiCorp Vault (its
//! KV and database credentials), Delinea, CyberArk Conjur and its Central
//! Credential Provider, 1Password, AWS (Secrets Manager and Parameter
//! Store), Azure Key Vault, Google Secret Manager and KeePass. For the
//! cloud ones and 1Password the account is a named target (a profile and a
//! region, a tenant and a vault, a project, a CLI account): their
//! credentials stay with the provider's own tools, nothing is kept here.

use crate::secret_sources::{
    azure_vault_host, bitwarden_urls, check_master_command, check_vault_auth, delinea_platform_host, is_uuid, service_url, url_segment, valid_aws_profile, valid_aws_region,
    valid_azure_tenant, valid_gcloud_account, MasterSource, VaultAuth,
};
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

/// Every slot an account may have in the keychain (deleting one clears them
/// all): Zoho's client secret and refresh token, a master passphrase (also
/// Bitwarden's and KeePass's, and a CCP key's), Bitwarden's client secret, a
/// token (Vault, Secrets Manager), AppRole's secret ID, Delinea's password
/// (or client secret) and Conjur's API key.
pub const ALL_SLOTS: &[&str] = &["client_secret", "refresh_token", "passphrase", "token", "secret_id", "password", "api_key"];

fn is_keychain(m: &MasterSource) -> bool {
    *m == MasterSource::Keychain
}

fn is_token_auth(a: &VaultAuth) -> bool {
    *a == VaultAuth::Token
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
    /// A Bitwarden user: the personal API key (its `client_secret` in the
    /// keychain) and the master password.
    Bitwarden {
        /// `bitwarden.com`, `bitwarden.eu` or a self-hosted server's URL.
        server: String,
        /// `user.<uuid>`.
        client_id: String,
        #[serde(default, skip_serializing_if = "is_keychain")]
        master: MasterSource,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        master_command: String,
    },
    /// A Bitwarden Secrets Manager machine account (its access token in the
    /// keychain, slot `token`).
    BitwardenSm { server: String },
    /// A HashiCorp Vault (or OpenBao) sign-in: a token (slot `token`; none
    /// kept: `VAULT_TOKEN` / `~/.vault-token`) or AppRole (slot `secret_id`).
    Vault {
        address: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        namespace: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        ca_file: String,
        #[serde(default, skip_serializing_if = "is_token_auth")]
        auth: VaultAuth,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        approle_mount: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        role_id: String,
    },
    /// Delinea Secret Server (or Delinea Platform's tenant for the sign-in):
    /// a user and its password, or a client ID and its secret (slot
    /// `password` when kept).
    Delinea {
        url: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        platform_url: String,
        username: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        domain: String,
        #[serde(default, skip_serializing_if = "is_keychain")]
        master: MasterSource,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        master_command: String,
    },
    /// A CyberArk Conjur host or user (its API key in the keychain, slot
    /// `api_key`).
    Conjur {
        url: String,
        conjur_account: String,
        login: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        ca_file: String,
    },
    /// CyberArk's Central Credential Provider as an application: its AppID
    /// and client certificate. The paths are kept, never copies of the key;
    /// its passphrase (when it has one) is in the keychain, slot `passphrase`.
    Ccp {
        url: String,
        app_id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        client_cert: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        client_key: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        ca_file: String,
    },
    /// A 1Password CLI account (`op --account`): its sign-in address, ID or
    /// shorthand. `op` keeps its own session; nothing is kept here.
    #[serde(rename = "onepassword")]
    OnePassword { account: String },
    /// AWS (Secrets Manager and Parameter Store): a profile (empty: the
    /// default chain) and a region. The credentials stay with AWS's files
    /// and tools.
    Aws {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        profile: String,
        region: String,
    },
    /// Azure Key Vault: a vault and the Microsoft Entra tenant (empty: the
    /// CLI's current one). The token comes from the Azure CLI.
    Azure {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        tenant: String,
        vault_url: String,
    },
    /// Google Secret Manager: a project and the gcloud account the token is
    /// asked for (an e-mail; empty, as saved before it existed: gcloud's
    /// active one). The token comes from gcloud.
    Gcp {
        project: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        account: String,
    },
    /// A KeePass database: the file, its key file and where the master
    /// password comes from (slot `passphrase` when kept).
    #[serde(rename = "keepass")]
    KeePass {
        file: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        key_file: String,
        #[serde(default, skip_serializing_if = "is_keychain")]
        master: MasterSource,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        master_command: String,
    },
}

/// Every kind of account, as stored.
pub const KINDS: &[&str] = &["zoho", "bitwarden", "bitwarden_sm", "vault", "delinea", "conjur", "ccp", "onepassword", "aws", "azure", "gcp", "keepass"];

impl AccountConfig {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Zoho { .. } => "zoho",
            Self::Bitwarden { .. } => "bitwarden",
            Self::BitwardenSm { .. } => "bitwarden_sm",
            Self::Vault { .. } => "vault",
            Self::Delinea { .. } => "delinea",
            Self::Conjur { .. } => "conjur",
            Self::Ccp { .. } => "ccp",
            Self::OnePassword { .. } => "onepassword",
            Self::Aws { .. } => "aws",
            Self::Azure { .. } => "azure",
            Self::Gcp { .. } => "gcp",
            Self::KeePass { .. } => "keepass",
        }
    }

    /// The keychain slots this account may use as set now (the others are
    /// deleted when it's saved). Optional ones (a Vault token, a key's
    /// passphrase) are listed too.
    pub fn slots(&self) -> &'static [&'static str] {
        match self {
            Self::Zoho { master: MasterSource::Keychain, .. } => &["client_secret", "refresh_token", "passphrase"],
            Self::Zoho { .. } => &["client_secret", "refresh_token"],
            Self::Bitwarden { master: MasterSource::Keychain, .. } => &["client_secret", "passphrase"],
            Self::Bitwarden { .. } => &["client_secret"],
            Self::BitwardenSm { .. } => &["token"],
            Self::Vault { auth: VaultAuth::Approle, .. } => &["secret_id"],
            Self::Vault { .. } => &["token"],
            Self::Delinea { master: MasterSource::Keychain, .. } => &["password"],
            Self::Delinea { .. } => &[],
            Self::Conjur { .. } => &["api_key"],
            Self::Ccp { .. } => &["passphrase"],
            Self::KeePass { master: MasterSource::Keychain, .. } => &["passphrase"],
            Self::KeePass { .. } | Self::OnePassword { .. } | Self::Aws { .. } | Self::Azure { .. } | Self::Gcp { .. } => &[],
        }
    }

    /// The slots that can't be missing once connected.
    pub fn required_slots(&self) -> &'static [&'static str] {
        match self {
            Self::Zoho { .. } => &["client_secret", "refresh_token"],
            Self::Bitwarden { master: MasterSource::Keychain, .. } => &["client_secret", "passphrase"],
            Self::Bitwarden { .. } => &["client_secret"],
            Self::BitwardenSm { .. } => &["token"],
            Self::Delinea { master: MasterSource::Keychain, .. } => &["password"],
            Self::Conjur { .. } => &["api_key"],
            Self::KeePass { master: MasterSource::Keychain, .. } => &["passphrase"],
            _ => &[],
        }
    }

    /// The master password's slot (Delinea's password's), where it comes
    /// from (kept there, asked or a command's output) and the command.
    pub fn master(&self) -> Option<(&'static str, MasterSource, &str)> {
        match self {
            Self::Zoho { master, master_command, .. } | Self::Bitwarden { master, master_command, .. } | Self::KeePass { master, master_command, .. } => Some(("passphrase", *master, master_command)),
            Self::Delinea { master, master_command, .. } => Some(("password", *master, master_command)),
            _ => None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        let no_control = |s: &str| !s.chars().any(char::is_control);
        let service = |url: &str, what: &str| -> Result<()> {
            if service_url(url).is_none() {
                return Err(bad(format!("la URL de {what} tiene que ser https:// (http:// solo para esta computadora)")));
            }
            Ok(())
        };
        let path_ok = |p: &str| p.len() <= 1024 && no_control(p);
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
            Self::Bitwarden { server, client_id, master, master_command } => {
                if bitwarden_urls(server).is_none() {
                    return Err(bad("el servidor de Bitwarden tiene que ser bitwarden.com, bitwarden.eu o la URL https:// del servidor propio"));
                }
                if !client_id.trim().strip_prefix("user.").is_some_and(is_uuid) {
                    return Err(bad("falta el client_id de la clave de API de Bitwarden (user.xxxxxxxx-…)"));
                }
                if *master == MasterSource::None {
                    return Err(bad("Bitwarden necesita la contraseña maestra"));
                }
                check_master_command(*master, master_command)?;
            }
            Self::BitwardenSm { server } => {
                if bitwarden_urls(server).is_none() {
                    return Err(bad("el servidor de Bitwarden tiene que ser bitwarden.com, bitwarden.eu o la URL https:// del servidor propio"));
                }
            }
            Self::Vault { address, namespace, ca_file, auth, approle_mount, role_id } => {
                // Stricter than an inline definition: plain HTTP only to this computer.
                service(address, "Vault")?;
                if namespace.len() > 255 || !no_control(namespace) {
                    return Err(bad("el espacio de nombres de Vault no es válido"));
                }
                if !path_ok(ca_file) {
                    return Err(bad("la ruta del certificado de la CA no es válida"));
                }
                check_vault_auth(*auth, approle_mount, role_id)?;
            }
            Self::Delinea { url, platform_url, username, domain, master, master_command } => {
                service(url, "Secret Server")?;
                if !platform_url.trim().is_empty() && delinea_platform_host(platform_url).is_none() {
                    return Err(bad("la URL de Delinea Platform tiene que ser https://<inquilino>.delinea.app"));
                }
                if username.trim().is_empty() || username.len() > 255 || !no_control(username) {
                    return Err(bad("falta el usuario de Delinea (o el ID del cliente en Delinea Platform)"));
                }
                if domain.len() > 255 || !no_control(domain) {
                    return Err(bad("el dominio de Delinea no es válido"));
                }
                if *master == MasterSource::None {
                    return Err(bad("Delinea necesita la contraseña (o el secreto del cliente)"));
                }
                check_master_command(*master, master_command)?;
            }
            Self::Conjur { url, conjur_account, login, ca_file } => {
                service(url, "Conjur")?;
                if conjur_account.trim().is_empty() || conjur_account.len() > 255 || !no_control(conjur_account) || conjur_account.contains(['/', '?', '#']) {
                    return Err(bad("falta la cuenta de Conjur (o no es válida)"));
                }
                if login.trim().is_empty() || login.len() > 1024 || !no_control(login) {
                    return Err(bad("falta el login de Conjur (host/<id> o un usuario)"));
                }
                if !path_ok(ca_file) {
                    return Err(bad("la ruta del certificado de la CA no es válida"));
                }
            }
            Self::Ccp { url, app_id, client_cert, client_key, ca_file } => {
                service(url, "Central Credential Provider")?;
                if app_id.trim().is_empty() || app_id.len() > 1024 || !no_control(app_id) {
                    return Err(bad("falta el AppID del Central Credential Provider"));
                }
                if ![client_cert, client_key, ca_file].iter().all(|p| path_ok(p)) {
                    return Err(bad("un dato del Central Credential Provider no es válido"));
                }
                if client_cert.trim().is_empty() && !client_key.trim().is_empty() {
                    return Err(bad("la clave privada necesita su certificado de cliente"));
                }
            }
            Self::OnePassword { account } => {
                let a = account.trim();
                if a.is_empty() || a.len() > 255 || a.starts_with('-') || !a.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '@')) {
                    return Err(bad("la cuenta de 1Password no es válida"));
                }
            }
            Self::Aws { profile, region } => {
                if !profile.is_empty() && !valid_aws_profile(profile) {
                    return Err(bad("el perfil de AWS no es válido"));
                }
                if !valid_aws_region(region.trim()) {
                    return Err(bad(format!("la región de AWS «{}» no es válida", region.trim())));
                }
            }
            Self::Azure { tenant, vault_url } => {
                if !tenant.is_empty() && !valid_azure_tenant(tenant) {
                    return Err(bad("el inquilino de Microsoft Entra no es válido (su ID o su dominio)"));
                }
                if azure_vault_host(vault_url).is_none() {
                    return Err(bad("la URL del almacén tiene que ser https://<nombre>.vault.azure.net"));
                }
            }
            Self::Gcp { project, account } => {
                if !url_segment(project.trim()) || project.contains(':') {
                    return Err(bad("falta el proyecto de Google Cloud (o no es válido)"));
                }
                if !account.is_empty() && !valid_gcloud_account(account) {
                    return Err(bad("la cuenta de gcloud tiene que ser un correo (ana@example.com)"));
                }
            }
            Self::KeePass { file, key_file, master, master_command } => {
                if file.trim().is_empty() || !path_ok(file) {
                    return Err(bad("falta el archivo de la base de KeePass (.kdbx)"));
                }
                if !path_ok(key_file) {
                    return Err(bad("la ruta del archivo de clave no es válida"));
                }
                if *master == MasterSource::None && key_file.trim().is_empty() {
                    return Err(bad("sin contraseña maestra, la base de KeePass necesita un archivo de clave"));
                }
                check_master_command(*master, master_command)?;
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

    /// One account of every kind, as the wizards make them.
    pub fn every_kind() -> Vec<AccountConfig> {
        vec![
            AccountConfig::Zoho { data_center: "eu".into(), client_id: "1000.ABCDEF".into(), master: MasterSource::Keychain, master_command: String::new() },
            AccountConfig::Bitwarden { server: "bitwarden.eu".into(), client_id: "user.6f1c2a9e-0d1b-4c55-9a51-2f7d8c3b1e04".into(), master: MasterSource::Ask, master_command: String::new() },
            AccountConfig::BitwardenSm { server: "https://vw.example.com".into() },
            AccountConfig::Vault { address: "https://vault.example.com:8200".into(), namespace: "admin".into(), ca_file: "~/ca.pem".into(), auth: VaultAuth::Approle, approle_mount: String::new(), role_id: "r-1".into() },
            AccountConfig::Delinea { url: "https://acme.secretservercloud.com".into(), platform_url: String::new(), username: "svc_dbine".into(), domain: "CORP".into(), master: MasterSource::Keychain, master_command: String::new() },
            AccountConfig::Conjur { url: "https://conjur.example.com".into(), conjur_account: "myorg".into(), login: "host/apps/dbine".into(), ca_file: String::new() },
            AccountConfig::Ccp { url: "https://ccp.example.com".into(), app_id: "DBine".into(), client_cert: "~/dbine.p12".into(), client_key: String::new(), ca_file: "~/ca.pem".into() },
            AccountConfig::OnePassword { account: "my.1password.com".into() },
            AccountConfig::Aws { profile: "prod".into(), region: "eu-west-1".into() },
            AccountConfig::Azure { tenant: "contoso.onmicrosoft.com".into(), vault_url: "https://kv1.vault.azure.net".into() },
            AccountConfig::Gcp { project: "my-proj".into(), account: "ana@example.com".into() },
            AccountConfig::KeePass { file: "~/db.kdbx".into(), key_file: "~/db.keyx".into(), master: MasterSource::Command, master_command: "pass kp".into() },
        ]
    }

    /// Every kind saves, lists and reads back as it was; the row is the
    /// metadata only (no slot, nothing secret) and isn't in the snapshot.
    #[test]
    fn every_kind_round_trips_without_secrets_in_the_state() {
        let store = StateStore::open_in_memory().unwrap();
        let all = every_kind();
        assert_eq!(all.iter().map(AccountConfig::kind).collect::<Vec<_>>(), KINDS, "one of each");
        for (i, config) in all.iter().enumerate() {
            let a = SecretAccount { id: format!("6f1c2d4e-0000-4000-8000-0000000001{i:02}"), label: format!("{} {i}", config.kind()), identity: "ana@example.com".into(), config: config.clone(), created_at: String::new(), updated_at: String::new() };
            let saved = store.save_secret_account(&a).unwrap();
            assert_eq!(&saved.config, config);
            let json = serde_json::to_string(&saved).unwrap();
            assert!(json.contains(&format!("\"kind\":\"{}\"", config.kind())), "{json}");
            assert_eq!(serde_json::from_str::<SecretAccount>(&json).unwrap(), saved);
            for slot in ALL_SLOTS {
                assert!(!json.contains(&format!("\"{slot}\"")), "no slot in the row: {json}");
            }
            assert_eq!(store.get_secret_account(&a.id).unwrap().unwrap().config, *config);
        }
        assert_eq!(store.list_secret_accounts().unwrap().len(), all.len());
        let rev = store.revision().unwrap();
        let snap = serde_json::to_string(&store.snapshot().unwrap()).unwrap();
        assert!(!snap.contains("6f1c2d4e-0000-4000-8000-0000000001") && !snap.contains("kv1.vault.azure.net") && !snap.contains("svc_dbine"), "{snap}");
        assert_eq!(store.revision().unwrap(), rev);
    }

    #[test]
    fn slots_of_every_kind() {
        for c in every_kind() {
            for s in c.slots().iter().chain(c.required_slots()) {
                assert!(ALL_SLOTS.contains(s), "{}: {s}", c.kind());
            }
            for s in c.required_slots() {
                assert!(c.slots().contains(s), "{}: a required slot is kept", c.kind());
            }
            if let Some((slot, master, _)) = c.master() {
                assert_eq!(c.slots().contains(&slot), master == MasterSource::Keychain, "{}: only a kept master password has a slot", c.kind());
            }
            c.validate().unwrap_or_else(|e| panic!("{}: {e}", c.kind()));
        }
        let kinds = |c: AccountConfig| c.slots();
        assert_eq!(kinds(AccountConfig::Vault { address: "https://v".into(), namespace: String::new(), ca_file: String::new(), auth: VaultAuth::Token, approle_mount: String::new(), role_id: String::new() }), &["token"]);
        assert_eq!(kinds(AccountConfig::BitwardenSm { server: String::new() }), &["token"]);
        assert_eq!(kinds(AccountConfig::Conjur { url: "https://c".into(), conjur_account: "a".into(), login: "l".into(), ca_file: String::new() }), &["api_key"]);
        assert!(kinds(AccountConfig::Aws { profile: String::new(), region: "us-east-1".into() }).is_empty(), "ambient credentials: nothing kept");
        assert!(kinds(AccountConfig::OnePassword { account: "my".into() }).is_empty(), "op keeps its own session");
        assert_eq!(secret_key("6f1c", "api_key"), "secret_account:6f1c#api_key");
    }

    /// Each kind refuses what isn't its own: other hosts, plain HTTP to
    /// another computer, flags, a master password it can't do without.
    #[test]
    fn what_each_kind_may_be() {
        let bad = [
            AccountConfig::Bitwarden { server: "http://vw.example.com".into(), client_id: "user.6f1c2a9e-0d1b-4c55-9a51-2f7d8c3b1e04".into(), master: MasterSource::Keychain, master_command: String::new() },
            AccountConfig::Bitwarden { server: "bitwarden.com".into(), client_id: "6f1c2a9e".into(), master: MasterSource::Keychain, master_command: String::new() },
            AccountConfig::Bitwarden { server: "bitwarden.com".into(), client_id: "user.6f1c2a9e-0d1b-4c55-9a51-2f7d8c3b1e04".into(), master: MasterSource::None, master_command: String::new() },
            AccountConfig::BitwardenSm { server: "https://a@b".into() },
            AccountConfig::Vault { address: "http://vault.example.com:8200".into(), namespace: String::new(), ca_file: String::new(), auth: VaultAuth::Token, approle_mount: String::new(), role_id: String::new() },
            AccountConfig::Vault { address: "https://vault".into(), namespace: String::new(), ca_file: String::new(), auth: VaultAuth::Approle, approle_mount: String::new(), role_id: String::new() },
            AccountConfig::Delinea { url: "https://ss".into(), platform_url: "https://evil.example.com".into(), username: "u".into(), domain: String::new(), master: MasterSource::Keychain, master_command: String::new() },
            AccountConfig::Delinea { url: "https://ss".into(), platform_url: String::new(), username: "u".into(), domain: String::new(), master: MasterSource::Command, master_command: String::new() },
            AccountConfig::Conjur { url: "ftp://c".into(), conjur_account: "a".into(), login: "l".into(), ca_file: String::new() },
            AccountConfig::Conjur { url: "https://c".into(), conjur_account: "a/b".into(), login: "l".into(), ca_file: String::new() },
            AccountConfig::Ccp { url: "https://ccp".into(), app_id: " ".into(), client_cert: String::new(), client_key: String::new(), ca_file: String::new() },
            AccountConfig::Ccp { url: "https://ccp".into(), app_id: "a".into(), client_cert: String::new(), client_key: "~/k.pem".into(), ca_file: String::new() },
            AccountConfig::OnePassword { account: "--debug".into() },
            AccountConfig::OnePassword { account: String::new() },
            AccountConfig::Aws { profile: "--debug".into(), region: "us-east-1".into() },
            AccountConfig::Aws { profile: String::new(), region: "evil.com/".into() },
            AccountConfig::Azure { tenant: "--debug".into(), vault_url: "https://kv1.vault.azure.net".into() },
            AccountConfig::Azure { tenant: String::new(), vault_url: "https://kv1.vault.azure.net.evil.com".into() },
            AccountConfig::Gcp { project: "a/b".into(), account: String::new() },
            AccountConfig::Gcp { project: "p".into(), account: "--debug".into() },
            AccountConfig::Gcp { project: "p".into(), account: "ana".into() },
            AccountConfig::KeePass { file: String::new(), key_file: String::new(), master: MasterSource::Keychain, master_command: String::new() },
            AccountConfig::KeePass { file: "~/db.kdbx".into(), key_file: String::new(), master: MasterSource::None, master_command: String::new() },
        ];
        for c in bad {
            assert!(c.validate().is_err(), "{c:?}");
        }
        // A Google account saved before the gcloud account was kept: the
        // active one, as before; and it saves back the same.
        let old: AccountConfig = serde_json::from_str(r#"{"kind":"gcp","project":"my-proj"}"#).unwrap();
        assert_eq!(old, AccountConfig::Gcp { project: "my-proj".into(), account: String::new() });
        assert!(old.validate().is_ok());
        assert_eq!(serde_json::to_string(&old).unwrap(), r#"{"kind":"gcp","project":"my-proj"}"#);
        let local = AccountConfig::Vault { address: "http://127.0.0.1:8200".into(), namespace: String::new(), ca_file: String::new(), auth: VaultAuth::Token, approle_mount: String::new(), role_id: String::new() };
        assert!(local.validate().is_ok(), "plain HTTP to this computer");
    }
}
