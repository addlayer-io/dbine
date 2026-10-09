//! Configuración › Correo (docs/scheduled-tasks.md): the SMTP server the
//! "Enviar un mail" steps use, and "Enviar un mail de prueba".

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use crate::tasks::mail::{self, MailSettings, Outgoing};
use serde::{Deserialize, Serialize};
use tauri::State;

#[derive(Serialize)]
pub struct MailView {
    /// `None`: not configured yet.
    pub settings: Option<MailSettings>,
    /// The vault has its password ("guardada").
    pub password_saved: bool,
}

fn view(state: &AppState) -> CommandResult<MailView> {
    Ok(MailView { settings: mail::load(state)?, password_saved: mail::password()?.is_some() })
}

#[tauri::command(rename_all = "camelCase")]
pub async fn mail_settings_get(state: State<'_, AppState>) -> CommandResult<MailView> {
    view(&state)
}

#[derive(Deserialize)]
pub struct SaveArgs {
    pub settings: MailSettings,
    /// Empty or `None`: the saved one stays.
    #[serde(default)]
    pub password: Option<String>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn mail_settings_save(state: State<'_, AppState>, args: SaveArgs) -> CommandResult<MailView> {
    if let Some(p) = args.settings.problem() {
        return Err(CommandError::BadRequest(p));
    }
    state.store.set_setting(mail::SETTING, Some(&serde_json::to_value(&args.settings).map_err(|e| CommandError::Internal(e.to_string()))?))?;
    match args.password.filter(|p| !p.is_empty()) {
        // No user: no login, so no password to keep.
        _ if args.settings.user.trim().is_empty() => dbine_core::secrets::delete_raw(mail::PASSWORD)?,
        Some(p) => dbine_core::secrets::set_raw(mail::PASSWORD, &p)?,
        None => {}
    }
    view(&state)
}

#[derive(Deserialize)]
pub struct TestArgs {
    /// The form as it is (saved or not).
    pub settings: MailSettings,
    /// Empty or `None`: the saved one.
    #[serde(default)]
    pub password: Option<String>,
    pub to: String,
}

/// Send a test mail with the form's server; the error says what failed
/// (login, TLS, refused connection, timeout).
#[tauri::command(rename_all = "camelCase")]
pub async fn mail_test(args: TestArgs) -> CommandResult<String> {
    if let Some(p) = args.settings.problem() {
        return Err(CommandError::BadRequest(p));
    }
    let password = match args.password.filter(|p| !p.is_empty()) {
        Some(p) => Some(p),
        None if args.settings.user.trim().is_empty() => None,
        None => mail::password()?,
    };
    let to = mail::addresses(&args.to);
    if to.is_empty() {
        return Err(CommandError::BadRequest("escribí a quién mandar el mail de prueba".into()));
    }
    let message = Outgoing {
        to,
        subject: "DBine: mail de prueba".into(),
        body: "Este es un mail de prueba de DBine. Si te llegó, las tareas programadas pueden enviar mails con este servidor.".into(),
        ..Default::default()
    };
    let summary = mail::deliver(&args.settings, password, &message).await.map_err(CommandError::Connect)?;
    tracing::info!("mail test sent to {}", message.to.join(", "));
    Ok(summary)
}
