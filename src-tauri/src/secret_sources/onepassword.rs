//! 1Password: `op read <op://vault/item/field>` with the 1Password CLI,
//! run directly (no shell) with the reference as one argument.

use super::cloud_auth;
use super::exec::{self, Failure};
use std::path::PathBuf;
use std::time::Duration;

/// `op` may wait for the desktop app's unlock (Touch ID, Windows Hello).
const LIMIT: Duration = Duration::from_secs(60);

/// The `op` binary: `PATH`, then the usual install folders. On Windows only
/// an `.exe` (a `.cmd` would go through cmd.exe and its parsing).
fn find_op() -> Option<PathBuf> {
    let p = cloud_auth::find_cli("op")?;
    (!cfg!(windows) || p.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe"))).then_some(p)
}

pub async fn read(reference: &str, account: &str) -> Result<String, String> {
    if cloud_auth::sandboxed() {
        return Err(cloud_auth::sandbox_message("op"));
    }
    let op = find_op().ok_or_else(|| "la CLI de 1Password («op») no está instalada: instalala desde 1password.com/downloads/command-line".to_string())?;
    let mut cmd = tokio::process::Command::new(op);
    cmd.arg("read").arg(reference.trim());
    if !account.is_empty() {
        cmd.arg(format!("--account={account}"));
    }
    match exec::run(cmd, LIMIT).await {
        Ok(out) => Ok(exec::trim_newline(out)),
        Err(Failure::Exit { stderr, .. }) => Err(explain(&stderr, reference.trim())),
        Err(Failure::Timeout(d)) => Err(format!("«op» no respondió en {} s (¿quedó esperando que desbloquees 1Password?)", d.as_secs())),
        Err(Failure::Spawn(e)) => Err(format!("«op» no se pudo ejecutar: {e}")),
        Err(Failure::TooLong) => Err("el valor de 1Password es demasiado largo".into()),
        Err(Failure::NotText) => Err("el valor de 1Password no es texto".into()),
    }
}

/// `op`'s error, said plainly when it's a known one.
fn explain(stderr: &str, reference: &str) -> String {
    let s = stderr.to_ascii_lowercase();
    let why = stderr.strip_prefix("[ERROR]").map(str::trim).unwrap_or(stderr);
    // `2026/10/18 10:00:00 ` before the message.
    let why = match why.get(..20).map(str::as_bytes) {
        Some(p) if p[4] == b'/' && p[7] == b'/' && p[13] == b':' => why[20..].trim(),
        _ => why,
    };
    if s.contains("not currently signed in") || s.contains("not signed in") || s.contains("no accounts configured") || s.contains("account is not signed in") || s.contains("session expired") {
        "la CLI de 1Password no tiene una sesión iniciada: ejecutá «op signin» o activá la integración de la CLI en la app de 1Password (Ajustes › Desarrollador)".into()
    } else if s.contains("authorization prompt dismissed") || s.contains("authorization timeout") {
        "se canceló el desbloqueo de 1Password".into()
    } else if s.contains("isn't an item") || s.contains("could not find item") || s.contains("isn't a vault") || s.contains("could not find vault") || s.contains("isn't a field") || s.contains("does not have a field") || s.contains("could not find field") {
        format!("1Password no encontró «{reference}»: {why}")
    } else if why.is_empty() {
        "«op» falló sin explicar por qué".into()
    } else {
        format!("«op» falló: {why}")
    }
}

/// The reference of the same item's user field: `op://<vault>/<item>/` and
/// `user_field` (`[section/]field`), else `username`.
pub fn user_reference(reference: &str, user_field: &str) -> String {
    let r = reference.trim().trim_start_matches("op://");
    let item: Vec<&str> = r.splitn(3, '/').take(2).collect();
    let field = Some(user_field.trim().trim_matches('/')).filter(|f| !f.is_empty()).unwrap_or("username");
    format!("op://{}/{field}", item.join("/"))
}

// ---- browse ------------------------------------------------------------------

/// A listing's output can be long (an item list); a secret can't.
const LIST_LIMIT: u64 = 8 * 1024 * 1024;

/// Run `op <args> --format json` and parse it.
async fn op_json(args: &[&str], account: &str) -> Result<serde_json::Value, String> {
    if cloud_auth::sandboxed() {
        return Err(cloud_auth::sandbox_message("op"));
    }
    let op = find_op().ok_or_else(|| "la CLI de 1Password («op») no está instalada: instalala desde 1password.com/downloads/command-line".to_string())?;
    let mut cmd = tokio::process::Command::new(op);
    cmd.args(args).arg("--format=json");
    if !account.trim().is_empty() {
        cmd.arg(format!("--account={}", account.trim()));
    }
    let mut out = match exec::run_capped(cmd, LIMIT, LIST_LIMIT).await {
        Ok(out) => out,
        Err(Failure::Exit { stderr, .. }) => return Err(explain(&stderr, &args.join(" "))),
        Err(Failure::Timeout(d)) => return Err(format!("«op» no respondió en {} s (¿quedó esperando que desbloquees 1Password?)", d.as_secs())),
        Err(Failure::Spawn(e)) => return Err(format!("«op» no se pudo ejecutar: {e}")),
        Err(Failure::TooLong) => return Err("la lista de 1Password es demasiado larga".into()),
        Err(Failure::NotText) => return Err("la respuesta de 1Password no es texto".into()),
    };
    let parsed = serde_json::from_str(&out).map_err(|_| "«op» no devolvió JSON".to_string());
    dbine_core::secret_sources::wipe(&mut out);
    parsed
}

/// Whether the 1Password CLI is installed (the account wizard's first step).
pub fn installed() -> bool {
    find_op().is_some()
}

/// `op account list`: the accounts the CLI knows, as `{url, email,
/// user_uuid, account_uuid, shorthand}` (whichever it gives; nothing secret).
pub async fn accounts() -> Result<Vec<serde_json::Value>, String> {
    let v = op_json(&["account", "list"], "").await?;
    let keep = ["url", "email", "user_uuid", "account_uuid", "shorthand"];
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .map(|a| serde_json::Value::Object(keep.iter().filter_map(|k| a.get(*k).filter(|v| v.is_string()).map(|v| (k.to_string(), v.clone()))).collect()))
        .collect())
}

/// `op whoami` for the account: who the CLI is signed in as there (its
/// e-mail, else the sign-in address).
pub async fn whoami(account: &str) -> Result<String, String> {
    let v = op_json(&["whoami"], account).await?;
    let s = |k: &str| v.get(k).and_then(serde_json::Value::as_str).map(str::trim).filter(|x| !x.is_empty()).map(str::to_string);
    s("email").or_else(|| s("url")).or_else(|| s("user_uuid")).ok_or_else(|| "«op whoami» no dijo con qué cuenta inició sesión".to_string())
}

/// A segment of an `op://` reference: the name when it can be one (no
/// slash, nothing `op` would read otherwise, not repeated), else the ID.
fn segment(name: &str, id: &str, unique: bool) -> String {
    let plain = unique && !name.is_empty() && name.trim() == name && name.chars().all(|c| c.is_alphanumeric() || " -_.".contains(c));
    if plain { name.to_string() } else { id.to_string() }
}

/// The vaults (no `path`), a vault's items (`path`: its ID), or an item's
/// field names as they go after `op://<vault>/<item>/` (`item`: its
/// entry's reference, `path`: the vault's ID).
pub async fn browse(account: &str, at: super::browse::At<'_>) -> Result<super::browse::Listing, String> {
    use super::browse::{unique_names, wipe_json, Entry, Listing};
    use serde_json::Value;
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    if let Some(item) = at.item {
        // The entry's reference (`op://<vault>/<item>`): its item, in the vault browsed.
        let item = item.strip_prefix("op://").and_then(|r| r.split_once('/')).map(|(_, i)| i).unwrap_or(item);
        let mut json = op_json(&["item", "get", item, "--vault", at.path], account).await?;
        let fields = fields_of(&json);
        wipe_json(&mut json);
        return Ok(Listing::fields(fields));
    }
    let vaults = op_json(&["vault", "list"], account).await?;
    let vaults: Vec<Value> = vaults.as_array().cloned().unwrap_or_default();
    let vault_names = unique_names(vaults.iter().map(|v| v.get("name").and_then(Value::as_str).unwrap_or("")));
    if at.path.is_empty() {
        let mut out: Vec<Entry> = vaults.iter().map(|v| Entry::group(s(v, "id"), s(v, "name"), "")).collect();
        out.sort_by_key(|e| e.name.to_lowercase());
        return Ok(Listing::level(out));
    }
    let vault = vaults.iter().find(|v| s(v, "id") == at.path).ok_or_else(|| format!("1Password no tiene la bóveda «{}»", at.path))?;
    let (vid, vname) = (s(vault, "id"), s(vault, "name"));
    let vseg = segment(&vname, &vid, vault_names.contains(&vname));
    let items = op_json(&["item", "list", "--vault", &vid], account).await?;
    let items: Vec<Value> = items.as_array().cloned().unwrap_or_default();
    let titles = unique_names(items.iter().map(|i| i.get("title").and_then(Value::as_str).unwrap_or("")));
    let mut out: Vec<Entry> = items
        .iter()
        .map(|i| {
            let (id, title) = (s(i, "id"), s(i, "title"));
            let reference = format!("op://{vseg}/{}", segment(&title, &id, titles.contains(&title)));
            Entry::item(reference, if title.is_empty() { id.clone() } else { title }, vname.clone(), s(i, "category").to_lowercase())
        })
        .collect();
    out.sort_by_key(|e| e.name.to_lowercase());
    Ok(Listing::level(out))
}

/// An item's fields as `[section/]field` (labels when they can be read
/// back, else IDs); the values are never read here.
pub fn fields_of(item: &serde_json::Value) -> Vec<String> {
    use serde_json::Value;
    let fields: Vec<&Value> = item.get("fields").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    let label = |f: &Value| f.get("label").and_then(Value::as_str).unwrap_or("").to_string();
    let labels = super::browse::unique_names(fields.iter().map(|f| f.get("label").and_then(Value::as_str).unwrap_or("")));
    fields
        .iter()
        .filter_map(|f| {
            let id = f.get("id").and_then(Value::as_str).unwrap_or("");
            let name = segment(&label(f), id, labels.contains(&label(f)));
            if name.is_empty() {
                return None;
            }
            let section = f.get("section").and_then(|s| s.get("label")).and_then(Value::as_str).filter(|l| !l.is_empty());
            Some(match section {
                Some(sec) if f.get("purpose").is_none() => format!("{}/{name}", segment(sec, f.pointer("/section/id").and_then(Value::as_str).unwrap_or(""), true)),
                _ => name,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_errors_are_explained() {
        assert!(explain("[ERROR] 2026/10/18 10:00:00 You are not currently signed in. Please run `op signin --help`", "op://a/b/c").contains("op signin"));
        let e = explain("[ERROR] 2026/10/18 10:00:00 \"Nope\" isn't an item in the \"Private\" vault. Specify the item with its UUID", "op://Private/Nope/password");
        assert!(e.starts_with("1Password no encontró «op://Private/Nope/password»: \"Nope\" isn't an item"), "{e}");
        assert_eq!(explain("boom", "op://a/b/c"), "«op» falló: boom");
    }

    /// A fake `op` on PATH: `DBINE_TEST_FAKE_OP=1 cargo test -p dbine secret_sources::onepassword -- --ignored`.
    #[test]
    fn user_references() {
        assert_eq!(user_reference("op://Private/pg/password", ""), "op://Private/pg/username");
        assert_eq!(user_reference("op://Private/pg/Database/password", "Database/user"), "op://Private/pg/Database/user");
    }

    #[test]
    fn item_fields_without_values() {
        let item = serde_json::json!({ "id": "abc", "title": "pg", "fields": [
            { "id": "username", "type": "STRING", "purpose": "USERNAME", "label": "username", "value": "app" },
            { "id": "password", "type": "CONCEALED", "purpose": "PASSWORD", "label": "password", "value": "s3cret" },
            { "id": "k1", "section": { "id": "s1", "label": "Database" }, "type": "STRING", "label": "port", "value": "5432" },
            { "id": "k2", "type": "CONCEALED", "label": "api/token", "value": "t0k" },
            { "id": "notesPlain", "type": "STRING", "purpose": "NOTES", "label": "notesPlain" }
        ] });
        let f = fields_of(&item);
        assert_eq!(f, ["username", "password", "Database/port", "k2", "notesPlain"]);
        assert_eq!(segment("Private", "v1", true), "Private");
        assert_eq!(segment("a/b", "v1", true), "v1");
        assert_eq!(segment("Dup", "v2", false), "v2");
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore]
    async fn live_fake_op() {
        if std::env::var("DBINE_TEST_FAKE_OP").is_err() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = super::super::tests::temp_dir("fake-op");
        let op = dir.join("op");
        std::fs::write(
            &op,
            "#!/bin/sh\n[ \"$1\" = read ] || exit 9\ncase \"$2\" in\n  'op://Private Vault/DB/password') printf 'hunter2\\n' ;;\n  op://signed/out/x) echo '[ERROR] 2026/10/18 10:00:00 You are not currently signed in.' >&2; exit 1 ;;\n  *) echo \"[ERROR] 2026/10/18 10:00:00 \\\"x\\\" isn't an item in the \\\"y\\\" vault.\" >&2; exit 1 ;;\nesac\n",
        )
        .unwrap();
        std::fs::set_permissions(&op, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![dir.clone()];
        paths.extend(std::env::split_paths(&old));
        // This ignored test runs alone (`--test-threads=1`).
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        assert_eq!(read("op://Private Vault/DB/password", "").await.unwrap(), "hunter2", "a reference with spaces is one argument");
        assert!(read("op://signed/out/x", "").await.unwrap_err().contains("op signin"));
        assert!(read("op://nope/nope/nope", "").await.unwrap_err().starts_with("1Password no encontró"));
        std::env::set_var("PATH", old);
    }
}
