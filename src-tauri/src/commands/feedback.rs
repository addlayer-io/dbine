//! "Enviar comentarios": the UI builds a prefilled GitHub issue-form URL
//! (`web/src/composables/feedbackUrl.ts`) and the user creates the issue in
//! the browser. Nothing is sent from the app and there's no token in it: the
//! labels come from the issue form (`.github/ISSUE_TEMPLATE/*.yml`), never
//! from the URL. See docs/feedback.md.
//!
//! The backend only does what the web side can't: tell the app version and
//! the OS for the optional diagnostics, and open the URL, which has to be one
//! of this repository's issue forms (the web side can't open arbitrary links
//! through this).

use crate::error::{CommandError, CommandResult};
use serde::{Deserialize, Serialize};

const NEW_ISSUE: &str = "https://github.com/addlayer-io/dbine/issues/new?template=";
const TEMPLATES: &[&str] = &[
    "bug.yml",
    "enhancement.yml",
    "driver-request.yml",
    "question.yml",
];
/// GitHub answers 414 a bit past 8 KB; the UI keeps the URL under 7500.
const MAX_URL: usize = 8000;

/// What the diagnostics can say about the machine: nothing else is exposed.
#[derive(Serialize)]
pub struct FeedbackSystemInfo {
    pub app_version: String,
    /// OS name and version, e.g. "macOS 15.1".
    pub os: String,
    /// CPU architecture, e.g. "aarch64".
    pub arch: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn feedback_system_info(app: tauri::AppHandle) -> CommandResult<FeedbackSystemInfo> {
    let (name, version) = super::telemetry::os();
    Ok(FeedbackSystemInfo {
        app_version: app.package_info().version.to_string(),
        os: format!("{name} {version}").trim().to_string(),
        arch: std::env::consts::ARCH.to_string(),
    })
}

#[derive(Deserialize)]
pub struct OpenIssueFormArgs {
    pub url: String,
}

/// Open the prefilled issue form in the browser.
#[tauri::command(rename_all = "camelCase")]
pub async fn feedback_open_issue_form(
    app: tauri::AppHandle,
    args: OpenIssueFormArgs,
) -> CommandResult<()> {
    use tauri_plugin_opener::OpenerExt;
    if !is_issue_form_url(&args.url) {
        return Err(CommandError::BadRequest(
            "dirección de GitHub inválida".into(),
        ));
    }
    app.opener()
        .open_url(&args.url, None::<&str>)
        .map_err(|e| CommandError::Internal(format!("no se pudo abrir el navegador: {e}")))
}

/// One of this repository's issue forms, with an already encoded query.
fn is_issue_form_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix(NEW_ISSUE) else {
        return false;
    };
    let template = rest.split('&').next().unwrap_or_default();
    url.len() <= MAX_URL
        && TEMPLATES.contains(&template)
        // Encoded by the UI: nothing that could end the URL or reach a shell
        // or the opener as something else.
        && url.bytes().all(|b| b.is_ascii_graphic() && !matches!(b, b'"' | b'<' | b'>' | b'\\' | b'`' | b'#'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_this_repos_issue_forms_open() {
        assert!(is_issue_form_url(
            "https://github.com/addlayer-io/dbine/issues/new?template=bug.yml&title=Hola%20mundo"
        ));
        assert!(is_issue_form_url(
            "https://github.com/addlayer-io/dbine/issues/new?template=question.yml"
        ));
        assert!(!is_issue_form_url(
            "https://github.com/addlayer-io/dbine/issues/new?template=evil.yml"
        ));
        assert!(!is_issue_form_url(
            "https://github.com/addlayer-io/dbine/issues/new?template=bug.yml.evil"
        ));
        assert!(!is_issue_form_url(
            "https://github.com/other/dbine/issues/new?template=bug.yml"
        ));
        assert!(!is_issue_form_url(
            "https://github.com/addlayer-io/dbine/issues/new?template=bug.yml&title=a b"
        ));
        assert!(!is_issue_form_url(
            "https://github.com/addlayer-io/dbine/issues/new?template=bug.yml&x=\"y\""
        ));
        // What encodeURIComponent leaves as is.
        assert!(is_issue_form_url(
            "https://github.com/addlayer-io/dbine/issues/new?template=bug.yml&title=it's%20(ok)!*~"
        ));
        assert!(!is_issue_form_url("file:///etc/passwd"));
        let long = format!("{NEW_ISSUE}bug.yml&description={}", "a".repeat(MAX_URL));
        assert!(!is_issue_form_url(&long));
    }
}
