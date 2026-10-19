//! "Examinar" next to a source's item input (docs/secret-sources.md): what a
//! provider can list, so the user picks the entry and its field instead of
//! typing a reference.
//!
//! - Only names, paths, IDs and field names come back, never a value. Where
//!   the field names are the keys of a JSON value (AWS, Azure, Google,
//!   Parameter Store, Conjur, Bitwarden Secrets Manager) or of a Vault
//!   secret, the value is fetched here, its keys taken and the rest wiped.
//! - It's a Tauri command for the connection form only: MCP, the assistant
//!   and the phone have no way to it. The definition counts as typed by the
//!   user, like the form's Test; one that came from outside (an import, a
//!   restored backup) is confirmed first, and so is the saved definition of
//!   the connection being edited.
//! - Passwords and tokens typed for a browse (a master password asked for
//!   it) are used for that call and wiped; they're kept only where the user
//!   chose the keychain, when the connection is saved.

use super::*;
use dbine_core::secret_sources::wipe;
use serde_json::Value;

/// Entries one browse returns at most (the list says when it was cut).
pub const MAX_ENTRIES: usize = 5000;

/// One thing in a listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// What goes in the form's input (a group: the `path` to list next).
    pub id: String,
    pub name: String,
    /// Where it is, `/`-separated (empty: at the top).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub path: String,
    pub kind: EntryKind,
    /// Secondary text (an ID, a UUID, a type).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Group,
    Item,
}

impl Entry {
    pub fn item(id: impl Into<String>, name: impl Into<String>, path: impl Into<String>, detail: impl Into<String>) -> Self {
        Self { id: id.into(), name: name.into(), path: path.into(), kind: EntryKind::Item, detail: detail.into() }
    }
    pub fn group(id: impl Into<String>, name: impl Into<String>, path: impl Into<String>) -> Self {
        Self { id: id.into(), name: name.into(), path: path.into(), kind: EntryKind::Group, detail: String::new() }
    }
    /// An item named by a `/`-separated name (`prod/myapp/db`): shown as
    /// `db` under `prod/myapp`, chosen as the whole name.
    pub fn named(full: &str, detail: impl Into<String>) -> Self {
        let trimmed = full.trim_start_matches('/');
        let (path, name) = match trimmed.rsplit_once('/') {
            Some((p, n)) if !n.is_empty() => (p.to_string(), n.to_string()),
            _ => (String::new(), trimmed.to_string()),
        };
        Self::item(full, name, path, detail)
    }
}

/// What a browse returns.
#[derive(Debug, Default, Serialize)]
pub struct Listing {
    pub entries: Vec<Entry>,
    /// The chosen item's field names (or its JSON keys); `None` when the
    /// provider has none to choose.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<String>>,
    /// Everything below the path came at once: the UI walks the groups and
    /// filters itself. Else it's one level, and a group is browsed again.
    pub complete: bool,
    /// There were more than [`MAX_ENTRIES`].
    pub truncated: bool,
    /// The provider searches by text (`query`) rather than listing all.
    pub search: bool,
}

impl Listing {
    pub fn complete(mut entries: Vec<Entry>) -> Self {
        let truncated = entries.len() > MAX_ENTRIES;
        entries.truncate(MAX_ENTRIES);
        Self { entries, complete: true, truncated, ..Default::default() }
    }
    pub fn level(mut entries: Vec<Entry>) -> Self {
        let truncated = entries.len() > MAX_ENTRIES;
        entries.truncate(MAX_ENTRIES);
        Self { entries, truncated, ..Default::default() }
    }
    pub fn fields(fields: Vec<String>) -> Self {
        Self { fields: Some(fields), complete: true, ..Default::default() }
    }
}

/// What to list: a group (`path`), an item's fields (`item`), or what
/// matches a text (`query`, providers that search).
#[derive(Debug, Default, Clone, Copy)]
pub struct At<'a> {
    pub path: &'a str,
    pub item: Option<&'a str>,
    pub query: &'a str,
}

/// The keys of a JSON object value (sorted), else none; the value and the
/// parsed copy are wiped.
pub fn json_keys(mut raw: String) -> Vec<String> {
    let parsed: Option<Value> = serde_json::from_str(raw.trim()).ok();
    wipe(&mut raw);
    let Some(mut v) = parsed else { return Vec::new() };
    let mut keys: Vec<String> = v.as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default();
    keys.sort_unstable();
    wipe_json(&mut v);
    keys
}

/// Overwrite every string in a parsed JSON value.
pub fn wipe_json(v: &mut Value) {
    match v {
        Value::String(s) => wipe(s),
        Value::Array(a) => a.iter_mut().for_each(wipe_json),
        Value::Object(o) => o.values_mut().for_each(wipe_json),
        _ => {}
    }
}

/// The names that appear once (exactly): those can be chosen by name;
/// the rest by their ID.
pub fn unique_names<'a>(names: impl Iterator<Item = &'a str>) -> std::collections::HashSet<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    for n in names {
        *seen.entry(n.to_string()).or_default() += 1;
    }
    seen.into_iter().filter(|(_, c)| *c == 1).map(|(n, _)| n).collect()
}

/// The input each kind fills with what's chosen (`(input, placeholder)`):
/// left empty in the form while browsing, so the definition gets a
/// placeholder to be read and checked. `None`: the kind can't list.
fn item_input(kind: &str) -> Option<&'static [(&'static str, &'static str)]> {
    Some(match kind {
        "keepass" => &[("entry", "-")],
        "onepassword" => &[("reference", "op://-/-/-")],
        "vault" => &[("path", "-"), ("key", "-")],
        "vault_db" => &[("role", "-")],
        "aws" => &[("secret_id", "-")],
        "ssm" => &[("parameter", "-")],
        "azure" => &[("name", "-")],
        "gcp" => &[("secret", "-")],
        "zoho" => &[("secret_name", "-")],
        "conjur" => &[("variable", "-")],
        "delinea" => &[("secret_name", "-")],
        "bitwarden" => &[("item", "-")],
        "bitwarden_sm" => &[("secret", "00000000-0000-4000-8000-000000000000")],
        _ => return None,
    })
}

/// The form's definition, its item input filled if it's empty.
pub fn definition(raw: Value) -> Result<SecretSource, String> {
    let mut raw = raw;
    let kind = raw.get("kind").and_then(Value::as_str).unwrap_or("").to_string();
    let inputs = item_input(&kind).ok_or_else(|| "este origen no tiene una lista para examinar".to_string())?;
    if let Some(o) = raw.as_object_mut() {
        for (k, placeholder) in inputs {
            let empty = o.get(*k).and_then(Value::as_str).is_none_or(|s| s.trim().is_empty());
            if empty {
                o.insert((*k).to_string(), Value::String((*placeholder).to_string()));
            }
        }
    }
    let src: SecretSource = serde_json::from_value(raw).map_err(|e| format!("el origen no es válido: {e}"))?;
    src.validate().map_err(|e| e.to_string())?;
    Ok(src)
}

/// List what `at` asks for. `slots`: the field's tokens (typed now, else
/// kept), as for [`fetch`].
pub async fn list(src: &SecretSource, slots: &Secrets, at: At<'_>) -> Result<Listing, String> {
    let token = slots.get("").map(String::as_str);
    match src {
        SecretSource::KeePass { file, key_file, .. } => {
            let master = master_password(src, slots).await?;
            let key_file = Some(key_file.trim()).filter(|k| !k.is_empty()).map(expand_home);
            keepass::browse(expand_home(file.trim()), key_file, master, at.item.map(str::to_string)).await
        }
        SecretSource::OnePassword { account, .. } => onepassword::browse(account, at).await,
        SecretSource::Vault { address, namespace, kv_version, ca_file, auth, approle_mount, role_id, .. } => {
            let req = vault::Request { address, path: "", key: "", namespace, kv_version: *kv_version, ca_file };
            vault::browse(&req, &vault::Auth::of(*auth, approle_mount, role_id, slots), at).await
        }
        SecretSource::VaultDb { .. } => {
            let s = vault_db_source(src, slots);
            vault_db::browse(&s).await
        }
        SecretSource::Aws { region, profile, .. } => aws::browse(region, profile, at).await,
        SecretSource::Ssm { region, profile, .. } => ssm::browse(region, profile, at).await,
        SecretSource::Azure { vault_url, tenant, .. } => azure::browse(vault_url, tenant, at).await,
        SecretSource::Gcp { project, account, .. } => gcp::browse(project, account, at).await,
        SecretSource::Zoho { data_center, client_id, .. } => {
            let creds = zoho::Creds {
                client_id: client_id.trim().to_string(),
                client_secret: slots.get("client_secret").cloned().unwrap_or_default(),
                refresh_token: slots.get("refresh_token").cloned().unwrap_or_default(),
                // Names and field names aren't encrypted: no passphrase.
                passphrase: String::new(),
            };
            zoho::browse(data_center, &creds, at).await
        }
        SecretSource::Conjur { url, account, login, ca_file, .. } => {
            let req = cyberark::Conjur { url, account, login, variable: "", username_variable: "", ca_file };
            cyberark::conjur_browse(&req, token, at).await
        }
        SecretSource::Delinea { url, platform_url, username, domain, .. } => {
            let mut password = master_password(src, slots).await?.unwrap_or_default();
            let req = delinea::Request { url, platform_url, username, domain, secret: "", field: "", fill_username: false, user_field: "" };
            let r = delinea::browse(&req, &password, at).await;
            wipe(&mut password);
            r
        }
        SecretSource::Bitwarden { server, client_id, .. } => {
            let password = master_password(src, slots).await?.unwrap_or_default();
            let creds = bitwarden::Creds { client_id: client_id.trim().to_string(), client_secret: slots.get("client_secret").cloned().unwrap_or_default(), password };
            bitwarden::browse(server, &creds, at).await
        }
        SecretSource::BitwardenSm { server, .. } => bitwarden::browse_secrets(server, token, at).await,
        SecretSource::Command { .. } | SecretSource::Ccp { .. } => Err("este origen no tiene una lista para examinar".into()),
    }
}

// ---- command -----------------------------------------------------------------

/// Whether the form may browse `src` for `field`: what the user typed may,
/// a definition that came from outside and isn't confirmed may not (the
/// error asks to confirm it), and neither may anything while the edited
/// connection's own definition for the field is such a one (its kept
/// tokens would go along).
pub fn may_browse(store: &StateStore, saved: Option<&dbine_core::SavedConnection>, info: Option<&DriverInfo>, field: &str, src: &SecretSource) -> CommandResult<()> {
    let name = saved.map(|c| c.name.as_str()).unwrap_or("");
    if let Some(c) = saved {
        if let Some(stored) = defs::read(&c.config).ok().and_then(|s| s.get(field).cloned()) {
            check_trust(store, Origin::Saved { id: &c.id, name }, info, field, &stored)?;
        }
    }
    check_trust(store, Origin::Form { id: saved.map(|c| c.id.as_str()), name }, info, field, src)
}

#[derive(Deserialize)]
pub struct BrowseSecretSourceArgs {
    /// The connection being edited, if it's saved.
    pub connection_id: Option<String>,
    /// The form's engine, for the field's name in messages.
    #[serde(default)]
    pub driver: Option<String>,
    pub field: String,
    /// The form's definition, its item input possibly empty.
    pub source: Value,
    /// Tokens typed in the form (or asked for this browse), by slot.
    #[serde(default)]
    pub tokens: Option<Secrets>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub item: Option<String>,
    #[serde(default)]
    pub query: Option<String>,
}

/// "Examinar": list a source's groups and items, or an item's field names.
#[tauri::command(rename_all = "camelCase")]
pub async fn browse_secret_source(state: State<'_, AppState>, args: BrowseSecretSourceArgs) -> CommandResult<Listing> {
    let src = definition(args.source).map_err(CommandError::BadRequest)?;
    let id = args.connection_id.as_deref().filter(|i| !i.is_empty());
    let saved = match id {
        Some(i) => state.store.get_connection(i)?,
        None => None,
    };
    let name = saved.as_ref().map(|c| c.name.clone()).unwrap_or_default();
    let driver = args.driver.clone().or_else(|| saved.as_ref().map(|c| c.config.driver.clone()));
    let info = driver.as_deref().and_then(|d| driver_info(d).ok());
    let origin = Origin::Form { id, name: &name };
    may_browse(&state.store, saved.as_ref(), info, &args.field, &src)?;
    let slots = form_slots(&state, id, &args.field, &src, args.tokens.unwrap_or_default(), None);
    let at = At { path: args.path.as_deref().unwrap_or("").trim(), item: args.item.as_deref().map(str::trim).filter(|i| !i.is_empty()), query: args.query.as_deref().unwrap_or("").trim() };
    // A definition naming an account browses with the account's sign-in.
    let x = super::accounts::expand(&state.store, &src, slots).map_err(|m| CommandError::SecretSource(format!("no se pudo examinar {}: {m}", provider(&src))))?;
    let r = list(&x.src, &x.slots, at).await;
    r.map_err(|m| match asks_again(&x.src, &m) {
        Some(wrong) => unlock_error(origin, info, &args.field, &src, wrong),
        None => CommandError::SecretSource(format!("no se pudo examinar {}: {m}", provider(&src))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn definitions_get_a_placeholder_item() {
        let kp = definition(json!({ "kind": "keepass", "file": "~/db.kdbx", "master": "ask" })).unwrap();
        assert!(matches!(&kp, SecretSource::KeePass { entry, .. } if entry == "-"));
        let kept = definition(json!({ "kind": "keepass", "file": "~/db.kdbx", "entry": "Databases/pg" })).unwrap();
        assert!(matches!(&kept, SecretSource::KeePass { entry, .. } if entry == "Databases/pg"), "a typed item stays");
        assert!(definition(json!({ "kind": "bitwarden_sm", "server": "bitwarden.com" })).is_ok());
        assert!(definition(json!({ "kind": "vault", "address": "https://vault.example.com" })).is_ok());
        // What can't list, and what's still not valid.
        assert!(definition(json!({ "kind": "command", "command": "pass x" })).is_err());
        assert!(definition(json!({ "kind": "ccp", "url": "https://ccp", "app_id": "a", "object": "o" })).is_err());
        assert!(definition(json!({ "kind": "keepass" })).is_err(), "no file");
    }

    #[test]
    fn json_keys_never_values() {
        assert_eq!(json_keys(r#"{"password":"pw","username":"u"}"#.into()), vec!["password", "username"]);
        assert!(json_keys("plain-secret".into()).is_empty());
        assert!(json_keys("[1,2]".into()).is_empty());
    }

    #[test]
    fn named_entries() {
        let e = Entry::named("prod/myapp/db", "");
        assert_eq!((e.id.as_str(), e.name.as_str(), e.path.as_str()), ("prod/myapp/db", "db", "prod/myapp"));
        let e = Entry::named("/prod/db/password", "SecureString");
        assert_eq!((e.id.as_str(), e.name.as_str(), e.path.as_str()), ("/prod/db/password", "password", "prod/db"));
        assert_eq!(Entry::named("flat", "").path, "");
        let u = unique_names(["a", "b", "a"].into_iter());
        assert!(u.contains("b") && !u.contains("a"));
    }
}
