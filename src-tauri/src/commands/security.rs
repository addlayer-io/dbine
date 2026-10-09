//! Users and permissions (docs/users-and-permissions.md): what the server has,
//! and the code for a change, which the UI shows (password hidden) and runs
//! only on the user's click, without keeping it in the history.

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_driver::{Grant, Principal, SecurityAction};
use serde::{Deserialize, Serialize};
use tauri::State;

#[derive(Deserialize)]
pub struct PrincipalsArgs {
    pub connection_id: String,
    /// For engines whose users are per database; "" otherwise.
    #[serde(default)]
    pub database: String,
}

fn key(a: &str, db: &str) -> String {
    format!("security:{a}:{db}")
}

#[tauri::command(rename_all = "camelCase")]
pub async fn security_principals(state: State<'_, AppState>, args: PrincipalsArgs) -> CommandResult<Vec<Principal>> {
    let entry = state.session(&key(&args.connection_id, &args.database), &args.connection_id, &args.database).await?;
    let r = entry.session.lock().await.principals().await;
    Ok(r?)
}

/// "Asignar login…": the server's logins with no user in `database`.
/// `Unsupported` where the engine can't list them from there (the dialog
/// lets the user type the login).
#[tauri::command(rename_all = "camelCase")]
pub async fn security_unmapped_logins(state: State<'_, AppState>, args: PrincipalsArgs) -> CommandResult<Vec<String>> {
    let entry = state.session(&key(&args.connection_id, &args.database), &args.connection_id, &args.database).await?;
    let r = entry.session.lock().await.unmapped_logins().await;
    Ok(r?)
}

#[derive(Deserialize)]
pub struct GrantsArgs {
    pub connection_id: String,
    #[serde(default)]
    pub database: String,
    pub principal: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn security_grants(state: State<'_, AppState>, args: GrantsArgs) -> CommandResult<Vec<Grant>> {
    let entry = state.session(&key(&args.connection_id, &args.database), &args.connection_id, &args.database).await?;
    let r = entry.session.lock().await.grants(&args.principal).await;
    Ok(r?)
}

#[derive(Deserialize)]
pub struct ScriptArgs {
    pub connection_id: String,
    pub action: SecurityAction,
}

#[derive(Serialize)]
pub struct SecurityScript {
    pub script: String,
    /// The script with the password replaced by ••••••, for the preview.
    pub shown: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn security_script(state: State<'_, AppState>, args: ScriptArgs) -> CommandResult<SecurityScript> {
    let cfg = state.store.get_connection(&args.connection_id)?.ok_or_else(|| CommandError::NotFound("la conexión ya no existe".into()))?.config;
    let driver = dbine_drivers::find(&cfg.driver).ok_or_else(|| CommandError::BadRequest(format!("no hay driver '{}'", cfg.driver)))?;
    let script = driver.security_script(&args.action)?;
    let shown = match args.action.password().filter(|p| !p.is_empty()) {
        Some(p) => mask(&script, p),
        None => script.clone(),
    };
    Ok(SecurityScript { script, shown })
}

#[derive(Deserialize)]
pub struct MapLoginArgs {
    pub connection_id: String,
    pub login: String,
    pub user: String,
    #[serde(default)]
    pub default_schema: Option<String>,
}

/// "Asignar login…" (`Driver::supports_map_login`): the statement that maps
/// a server login to a user of the tab's database. No password in it, so
/// `shown` is the script itself; it runs like the other changes.
#[tauri::command(rename_all = "camelCase")]
pub async fn security_map_login_script(state: State<'_, AppState>, args: MapLoginArgs) -> CommandResult<SecurityScript> {
    let cfg = state.store.get_connection(&args.connection_id)?.ok_or_else(|| CommandError::NotFound("la conexión ya no existe".into()))?.config;
    let driver = dbine_drivers::find(&cfg.driver).ok_or_else(|| CommandError::BadRequest(format!("no hay driver '{}'", cfg.driver)))?;
    let script = driver.map_login_script(&args.login, &args.user, args.default_schema.as_deref())?;
    Ok(SecurityScript { shown: script.clone(), script })
}

/// The script with the password hidden, however the engine escaped it
/// (`''`, `\'`, JSON), longest form first.
pub(crate) fn mask(script: &str, password: &str) -> String {
    let json = serde_json::to_string(password).unwrap_or_default();
    let mut forms = vec![
        password.to_string(),
        password.replace('\'', "''"),
        password.replace('\\', "\\\\").replace('\'', "\\'"),
        password.replace('"', "\"\""),
        json.trim_matches('"').to_string(),
    ];
    forms.sort_by_key(|f| std::cmp::Reverse(f.len()));
    forms.dedup();
    let mut out = script.to_string();
    for f in forms.iter().filter(|f| !f.is_empty()) {
        out = out.replace(f.as_str(), "••••••");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::mask;

    #[test]
    fn hides_the_password_however_it_was_escaped() {
        let p = r#"a'b\c"d"#;
        for script in [
            format!("CREATE USER x PASSWORD '{}';", p.replace('\'', "''")),
            format!("CREATE USER x IDENTIFIED BY '{}';", p.replace('\\', "\\\\").replace('\'', "\\'")),
            format!("db.createUser({{ pwd: {} }})", serde_json::to_string(p).unwrap()),
        ] {
            let m = mask(&script, p);
            assert!(m.contains("••••••") && !m.contains("a'b") && !m.contains("a''b") && !m.contains("a\\'b"), "{m}");
        }
    }
}
