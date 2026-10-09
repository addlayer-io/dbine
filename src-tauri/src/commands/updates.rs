//! Updates. The check reads the updater manifest (`latest.json`, published
//! with each release, signed artifacts per platform) through
//! `tauri-plugin-updater`. When this install can update itself, the UI
//! offers "Actualizar ahora": `update_download` fetches and verifies the
//! package (minisign, against the public key in `tauri.conf.json`) and
//! `update_install_and_restart` installs it and relaunches, after the UI's
//! quit guard. Otherwise (no manifest yet, a deb/rpm install, the app run
//! from the DMG, a build without the real key) it falls back to GitHub's
//! latest release and the UI offers the release page, which
//! `open_release_page` opens in the system browser. docs/actualizaciones.md.
//!
//! Only one window drives an update: the one whose check found it (the
//! owner). Progress events go to it alone; another window that checks while
//! a download runs is told which window has it.

use crate::error::{CommandError, CommandResult};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager, WebviewWindow};
use tauri_plugin_updater::{Update, UpdaterExt};
use tokio::sync::Notify;

const LATEST_URL: &str = "https://api.github.com/repos/addlayer-io/dbine/releases/latest";
/// The only pages `open_release_page` opens.
const RELEASES_PREFIX: &str = "https://github.com/addlayer-io/dbine/releases/";
/// Release notes past this many characters are cut (the dialog shows a summary).
const NOTES_MAX: usize = 8000;
/// The `pubkey` a build carries until the real one is set: it can't verify
/// anything, so such a build never tries to install.
const PUBKEY_PLACEHOLDER: &str = "DBINE_UPDATER_PUBKEY_PLACEHOLDER";
/// How often the download's progress reaches the UI, at most.
const PROGRESS_EVERY: Duration = Duration::from_millis(100);

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CheckForUpdateArgs {
    /// Asked by the user (Ayuda › Buscar actualizaciones…), not the startup check.
    #[serde(default)]
    pub manual: bool,
}

/// What `check_for_update` answers.
#[derive(Serialize, Debug, PartialEq)]
pub struct UpdateInfo {
    /// The running app's version.
    pub current: String,
    /// The latest release's version (the tag without its leading `v`).
    pub latest: String,
    /// `latest` is newer than `current`.
    pub available: bool,
    /// The release page.
    pub url: String,
    /// The release notes (Markdown as written, cut to a sane length).
    pub notes: String,
    pub published_at: Option<String>,
    /// This install can download and install `latest` by itself.
    pub installable: bool,
    /// Why it can't (`installable` false): `unsigned` (a build without the
    /// updater key), `location` (macOS: not run from an installed .app),
    /// `package` (a deb/rpm or MSI install), `no_manifest` (the release has
    /// no updater manifest for this platform, or it couldn't be read).
    pub reason: Option<String>,
    /// `idle`, `downloading` or `ready` (downloaded and verified, waiting
    /// for the restart).
    pub phase: String,
    /// The window driving the update (its label), when one is.
    pub owner: Option<String>,
}

/// The fields used from GitHub's release object.
#[derive(Deserialize, Debug, Clone)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Idle,
    Downloading,
    Ready,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Downloading => "downloading",
            Phase::Ready => "ready",
        }
    }
}

/// The update found by the last check, for the rest of this run.
struct Pending {
    owner: Option<String>,
    update: Option<Update>,
    /// The verified package, between the download and the restart.
    bytes: Option<Vec<u8>>,
    phase: Phase,
    cancel: Option<Arc<Notify>>,
}

static PENDING: Mutex<Pending> =
    Mutex::new(Pending { owner: None, update: None, bytes: None, phase: Phase::Idle, cancel: None });

fn pending() -> std::sync::MutexGuard<'static, Pending> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// The owner, if its window is still open.
fn live_owner(app: &AppHandle, p: &Pending) -> Option<String> {
    p.owner.clone().filter(|l| app.get_webview_window(l).is_some())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn check_for_update(window: WebviewWindow, args: CheckForUpdateArgs) -> CommandResult<UpdateInfo> {
    let app = window.app_handle().clone();
    let me = window.label().to_string();
    let current = app.package_info().version.to_string();
    tracing::info!(manual = args.manual, window = %me, "checking for updates");

    // A download running or done: answer with it instead of checking again.
    {
        let mut p = pending();
        if p.phase != Phase::Idle {
            if let Some(u) = p.update.clone() {
                let owner = live_owner(&app, &p);
                // Downloaded: any window can restart (the bytes are here).
                // Downloading: the owner keeps it while its window is open.
                let take = p.phase == Phase::Ready || owner.is_none();
                if take && owner.as_deref() != Some(me.as_str()) {
                    set_owner(&app, &mut p, &me);
                }
                let owner = if take { Some(me.clone()) } else { owner };
                return Ok(offer_info(&current, &u, p.phase, owner));
            }
        }
    }

    if let Some(reason) = self_update_block(&app) {
        return github_check(&current, reason).await;
    }
    let updater = match build_updater(&app) {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!("updater unavailable: {e}");
            return github_check(&current, "no_manifest").await;
        }
    };
    match updater.check().await {
        Ok(Some(u)) => {
            let mut p = pending();
            // A check from another window may have started a download meanwhile.
            if p.phase != Phase::Idle {
                if let Some(running) = p.update.clone() {
                    return Ok(offer_info(&current, &running, p.phase, live_owner(&app, &p)));
                }
            }
            p.update = Some(u.clone());
            p.bytes = None;
            set_owner(&app, &mut p, &me);
            Ok(offer_info(&current, &u, Phase::Idle, Some(me)))
        }
        Ok(None) => Ok(UpdateInfo {
            current: current.clone(),
            latest: current,
            available: false,
            url: format!("{RELEASES_PREFIX}latest"),
            notes: String::new(),
            published_at: None,
            installable: false,
            reason: None,
            phase: Phase::Idle.as_str().into(),
            owner: None,
        }),
        // No manifest (releases up to 0.1.3, or the moment between creating
        // a release and uploading it), this platform missing from it, the
        // network, a bad JSON: the release page still works.
        Err(tauri_plugin_updater::Error::ReleaseNotFound) => {
            // The plugin's own log is filtered out (lib.rs): it calls this an error.
            tracing::info!("no latest.json in the latest release (or HTTP error), asking GitHub");
            github_check(&current, "no_manifest").await
        }
        Err(e) => {
            tracing::warn!("updater manifest unavailable, asking GitHub: {e}");
            github_check(&current, "no_manifest").await
        }
    }
}

/// `owner` becomes `me`; the previous owner's dialog closes.
fn set_owner(app: &AppHandle, p: &mut Pending, me: &str) {
    if let Some(old) = p.owner.replace(me.to_string()) {
        if old != me {
            let _ = app.emit_to(old.as_str(), "update-owner-changed", me);
        }
    }
}

fn offer_info(current: &str, u: &Update, phase: Phase, owner: Option<String>) -> UpdateInfo {
    UpdateInfo {
        current: current.to_string(),
        latest: u.version.clone(),
        available: true,
        url: tag_url(&u.version),
        notes: truncate(&notes_since(u.body.as_deref().unwrap_or(""), current), NOTES_MAX),
        published_at: u.raw_json.get("pub_date").and_then(|d| d.as_str()).map(str::to_string),
        installable: true,
        reason: None,
        phase: phase.as_str().into(),
        owner,
    }
}

fn tag_url(version: &str) -> String {
    format!("{RELEASES_PREFIX}tag/v{version}")
}

/// The pre-updater check: GitHub's latest release, offered as a page.
async fn github_check(current: &str, reason: &str) -> CommandResult<UpdateInfo> {
    let release = fetch_latest().await?;
    let mut info = evaluate(current, release)?;
    info.reason = Some(reason.to_string());
    Ok(info)
}

fn build_updater(app: &AppHandle) -> Result<tauri_plugin_updater::Updater, tauri_plugin_updater::Error> {
    let cleanup = app.clone();
    #[allow(unused_mut)]
    let mut builder = app
        .updater_builder()
        .timeout(Duration::from_secs(10))
        // Windows only: the plugin starts the installer and ends the process
        // itself, so `RunEvent::Exit` never comes. This replaces the
        // plugin's own hook (`cleanup_before_exit`), so it's called here too.
        .on_before_exit(move || {
            crate::prepare_exit(&cleanup);
            crate::exit_cleanup(&cleanup);
            cleanup.cleanup_before_exit();
        });
    // Tests only: a local manifest and a test key (docs/actualizaciones.md).
    #[cfg(debug_assertions)]
    {
        if let Some(url) = debug_env("DBINE_UPDATE_ENDPOINT") {
            let url = url.parse().map_err(tauri_plugin_updater::Error::UrlParse)?;
            builder = builder.endpoints(vec![url])?;
        }
        if let Some(key) = debug_env("DBINE_UPDATE_PUBKEY") {
            builder = builder.pubkey(key);
        }
    }
    builder.build()
}

#[cfg(debug_assertions)]
fn debug_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// The updater public key this build verifies with.
fn configured_pubkey(app: &AppHandle) -> String {
    #[cfg(debug_assertions)]
    if let Some(key) = debug_env("DBINE_UPDATE_PUBKEY") {
        return key;
    }
    app.config()
        .plugins
        .0
        .get("updater")
        .and_then(|u| u.get("pubkey"))
        .and_then(|k| k.as_str())
        .unwrap_or("")
        .to_string()
}

fn is_placeholder_pubkey(key: &str) -> bool {
    let key = key.trim();
    key.is_empty() || key == PUBKEY_PLACEHOLDER
}

/// Why this install can't update itself, if it can't.
fn self_update_block(app: &AppHandle) -> Option<&'static str> {
    if is_placeholder_pubkey(&configured_pubkey(app)) {
        return Some("unsigned");
    }
    platform_block(app)
}

#[cfg(target_os = "macos")]
fn platform_block(_app: &AppHandle) -> Option<&'static str> {
    let exe = std::env::current_exe().ok()?;
    (!mac_location_ok(&exe.to_string_lossy())).then_some("location")
}

#[cfg(windows)]
fn platform_block(_app: &AppHandle) -> Option<&'static str> {
    use tauri::utils::{config::BundleType, platform::bundle_type};
    // The MSI (and an unknown package) would need another manifest entry.
    (bundle_type() != Some(BundleType::Nsis)).then_some("package")
}

#[cfg(target_os = "linux")]
fn platform_block(app: &AppHandle) -> Option<&'static str> {
    use tauri::utils::{config::BundleType, platform::bundle_type};
    // deb/rpm: the system's package manager owns the files. Both checks: a
    // deb/rpm install started from an AppImage's process inherits `APPIMAGE`.
    (app.env().appimage.is_none() || bundle_type() != Some(BundleType::AppImage)).then_some("package")
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
fn platform_block(_app: &AppHandle) -> Option<&'static str> {
    Some("package")
}

/// macOS: the executable is inside an installed `.app` the updater can
/// replace. Not from `cargo tauri dev`, the mounted DMG or a translocated
/// copy (an app opened from Downloads without moving it).
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn mac_location_ok(exe: &str) -> bool {
    exe.contains(".app/Contents/MacOS/") && !exe.contains("/AppTranslocation/") && !exe.starts_with("/Volumes/")
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct UpdateDownloadArgs {}

#[derive(Serialize, Clone)]
struct Progress {
    /// `downloading` or `verifying`.
    phase: &'static str,
    downloaded: u64,
    total: Option<u64>,
}

/// Downloads and verifies the update the last check found. Progress goes to
/// the owner window as `update-progress`; the bytes stay here for
/// `update_install_and_restart`.
#[tauri::command(rename_all = "camelCase")]
pub async fn update_download(window: WebviewWindow, args: UpdateDownloadArgs) -> CommandResult<()> {
    let _ = args;
    let app = window.app_handle().clone();
    let (update, cancel) = {
        let mut p = pending();
        match p.phase {
            Phase::Ready => return Ok(()),
            Phase::Downloading => return Err(CommandError::BadRequest("la actualización ya se está descargando".into())),
            Phase::Idle => {}
        }
        let Some(update) = p.update.clone() else {
            return Err(CommandError::BadRequest("no hay ninguna actualización para descargar".into()));
        };
        let cancel = Arc::new(Notify::new());
        p.phase = Phase::Downloading;
        p.cancel = Some(cancel.clone());
        set_owner(&app, &mut p, window.label());
        (update, cancel)
    };

    let emit = |app: &AppHandle, progress: Progress| {
        if let Some(owner) = live_owner(app, &pending()) {
            let _ = app.emit_to(owner.as_str(), "update-progress", progress);
        }
    };
    let mut downloaded = 0u64;
    let mut last: Option<Instant> = None;
    let result = tokio::select! {
        r = update.download(
            |chunk, total| {
                downloaded += chunk as u64;
                if last.is_none_or(|t| t.elapsed() >= PROGRESS_EVERY) || Some(downloaded) == total {
                    last = Some(Instant::now());
                    emit(&app, Progress { phase: "downloading", downloaded, total });
                }
            },
            || emit(&app, Progress { phase: "verifying", downloaded: 0, total: None }),
        ) => Some(r),
        _ = cancel.notified() => None,
    };

    let mut p = pending();
    p.cancel = None;
    let outcome = match result {
        Some(Ok(bytes)) => {
            tracing::info!(version = %update.version, size = bytes.len(), "update downloaded and verified");
            p.bytes = Some(bytes);
            p.phase = Phase::Ready;
            Ok(())
        }
        Some(Err(e)) => {
            p.phase = Phase::Idle;
            tracing::warn!("update download failed: {e}");
            Err(download_error(&e))
        }
        None => {
            p.phase = Phase::Idle;
            tracing::info!("update download cancelled");
            Err(CommandError::Cancelled)
        }
    };
    // The owner may not be the window awaiting this command (it was closed
    // and another one took the download over): tell it how it ended.
    if let Some(owner) = live_owner(&app, &p) {
        let _ = app.emit_to(owner.as_str(), "update-finished", Finished::of(&outcome));
    }
    outcome
}

/// `update-finished`: how a download ended (to the owner window).
#[derive(Serialize, Clone)]
struct Finished {
    ok: bool,
    cancelled: bool,
    /// The error, as the command would return it (`{kind, message}`).
    error: Option<serde_json::Value>,
}

impl Finished {
    fn of(outcome: &CommandResult<()>) -> Self {
        match outcome {
            Ok(()) => Finished { ok: true, cancelled: false, error: None },
            Err(CommandError::Cancelled) => Finished { ok: false, cancelled: true, error: None },
            Err(e) => Finished { ok: false, cancelled: false, error: serde_json::to_value(e).ok() },
        }
    }
}

fn download_error(e: &tauri_plugin_updater::Error) -> CommandError {
    use tauri_plugin_updater::Error as E;
    match e {
        E::Minisign(_)
        | E::Base64(_)
        | E::SignatureUtf8(_)
        | E::SignedVersionMismatch { .. }
        | E::MissingSignedVersion => CommandError::Internal("la firma de la actualización no es válida".into()),
        other => CommandError::Connect(format!("no se pudo descargar la actualización: {}", chain(other))),
    }
}

/// Stops a running download.
#[tauri::command(rename_all = "camelCase")]
pub fn update_cancel() {
    if let Some(c) = pending().cancel.as_ref() {
        c.notify_one();
    }
}

/// Installs the downloaded update and relaunches DBine. Called by the UI's
/// quit guard once running tasks were dealt with (quitGuard.ts). Windows:
/// the plugin starts the installer (passive, relaunching the app) and ends
/// the process. macOS: it replaces the .app, asking for an administrator's
/// password when it can't write there. Linux: it rewrites the AppImage.
#[tauri::command(rename_all = "camelCase")]
pub async fn update_install_and_restart(app: AppHandle) -> CommandResult<()> {
    let (update, bytes) = {
        let mut p = pending();
        match (p.phase, p.update.clone(), p.bytes.take()) {
            (Phase::Ready, Some(u), Some(b)) => (u, b),
            (_, _, bytes) => {
                p.bytes = bytes;
                return Err(CommandError::BadRequest("la actualización todavía no se descargó".into()));
            }
        }
    };
    crate::prepare_exit(&app);
    // Off the async workers: macOS's admin prompt runs on the main thread
    // and this waits for it.
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let r = update.install(&bytes);
        (r, bytes)
    })
    .await;
    let err = match joined {
        Ok((Ok(()), _)) => {
            tracing::info!("update installed, restarting");
            app.request_restart();
            return Ok(());
        }
        Ok((Err(e), bytes)) => {
            pending().bytes = Some(bytes);
            chain(&e)
        }
        Err(e) => {
            pending().phase = Phase::Idle;
            e.to_string()
        }
    };
    crate::cancel_exit();
    tracing::warn!("installing the update failed: {err}");
    Err(CommandError::Internal(format!("no se pudo instalar la actualización: {err}")))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenReleasePageArgs {
    pub url: String,
}

/// Open a release page in the browser. Only this project's release pages:
/// the web side can't open arbitrary links through this.
#[tauri::command(rename_all = "camelCase")]
pub async fn open_release_page(app: tauri::AppHandle, args: OpenReleasePageArgs) -> CommandResult<()> {
    use tauri_plugin_opener::OpenerExt;
    if !args.url.starts_with(RELEASES_PREFIX) {
        return Err(CommandError::BadRequest("dirección de descarga inválida".into()));
    }
    app.opener()
        .open_url(&args.url, None::<&str>)
        .map_err(|e| CommandError::Internal(format!("no se pudo abrir el navegador: {e}")))
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("DBine/", env!("CARGO_PKG_VERSION")))
            .build()
            .unwrap_or_default()
    })
}

async fn fetch_latest() -> CommandResult<Release> {
    let resp = client()
        .get(LATEST_URL)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .map_err(|e| CommandError::Connect(format!("no se pudo consultar GitHub: {}", chain(&e))))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(CommandError::Connect(format!("GitHub respondió {status}")));
    }
    let text = resp
        .text()
        .await
        .map_err(|e| CommandError::Connect(format!("no se pudo consultar GitHub: {}", chain(&e))))?;
    parse_release(&text)
}

fn parse_release(json: &str) -> CommandResult<Release> {
    serde_json::from_str(json)
        .map_err(|e| CommandError::Internal(format!("respuesta de GitHub inválida: {e}")))
}

fn evaluate(current: &str, release: Release) -> CommandResult<UpdateInfo> {
    let latest = release.tag_name.trim().trim_start_matches(['v', 'V']).to_string();
    let newer = compare_versions(&latest, current)
        .ok_or_else(|| CommandError::Internal(format!("versión inválida: {latest}")))?;
    Ok(UpdateInfo {
        current: current.to_string(),
        available: newer == Ordering::Greater,
        latest,
        url: release.html_url,
        notes: truncate(&notes_since(release.body.as_deref().unwrap_or(""), current), NOTES_MAX),
        published_at: release.published_at,
        installable: false,
        reason: None,
        phase: Phase::Idle.as_str().into(),
        owner: None,
    })
}

/// A parsed `major.minor.patch[-pre][+build]` (minor and patch may be missing).
struct Version<'a> {
    nums: [u64; 3],
    pre: Option<&'a str>,
}

fn parse_version(v: &str) -> Option<Version<'_>> {
    let v = v.trim();
    let v = v.split_once('+').map_or(v, |(core, _)| core);
    let (core, pre) = match v.split_once('-') {
        Some((c, p)) if !p.is_empty() => (c, Some(p)),
        Some(_) => return None,
        None => (v, None),
    };
    let mut nums = [0u64; 3];
    let mut parts = core.split('.');
    for (i, n) in nums.iter_mut().enumerate() {
        match parts.next() {
            Some(p) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => *n = p.parse().ok()?,
            None if i > 0 => break,
            _ => return None,
        }
    }
    if parts.next().is_some() {
        return None;
    }
    Some(Version { nums, pre })
}

/// Semver order, numerically: `0.1.10 > 0.1.9`, and a release is newer
/// than its pre-releases (`1.0.0 > 1.0.0-beta.2`). `None` when either
/// isn't a version.
fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    let (a, b) = (parse_version(a)?, parse_version(b)?);
    Some(a.nums.cmp(&b.nums).then_with(|| match (a.pre, b.pre) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => compare_pre(x, y),
    }))
}

/// Pre-release identifiers, one by one: numbers numerically and below
/// words; more identifiers win when the shared ones are equal.
fn compare_pre(a: &str, b: &str) -> Ordering {
    let (mut a, mut b) = (a.split('.'), b.split('.'));
    loop {
        match (a.next(), b.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let o = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(m), Ok(n)) => m.cmp(&n),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    (Err(_), Err(_)) => x.cmp(y),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
        }
    }
}

/// The notes of the versions newer than `current`. A release carries the
/// changelog of its last few versions, each under `## Versión x.y.z`
/// (scripts/changelog.py recent), so someone several versions behind reads
/// all they're getting. Notes without those headings come back whole.
fn notes_since(body: &str, current: &str) -> String {
    let mut out = String::new();
    let (mut found, mut keep) = (false, true);
    for line in body.lines() {
        if let Some(v) = line.trim().strip_prefix("## Versión ") {
            found = true;
            keep = compare_versions(v.trim(), current) == Some(Ordering::Greater);
        }
        if keep {
            out.push_str(line);
            out.push('\n');
        }
    }
    if found { out.trim().to_string() } else { body.trim().to_string() }
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", s[..i].trim_end()),
        None => s.to_string(),
    }
}

/// An error with its causes ("error sending request" alone hides why).
fn chain(e: &dyn std::error::Error) -> String {
    let mut why = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        why.push_str(": ");
        why.push_str(&s.to_string());
        src = s.source();
    }
    why
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions_numerically() {
        use Ordering::*;
        assert_eq!(compare_versions("0.1.10", "0.1.9"), Some(Greater));
        assert_eq!(compare_versions("0.2.0", "0.1.99"), Some(Greater));
        assert_eq!(compare_versions("1.0.0", "0.99.99"), Some(Greater));
        assert_eq!(compare_versions("0.1.2", "0.1.2"), Some(Equal));
        assert_eq!(compare_versions("0.1.1", "0.1.2"), Some(Less));
        assert_eq!(compare_versions("1.2", "1.2.0"), Some(Equal));
        assert_eq!(compare_versions("1.0.0+build.5", "1.0.0"), Some(Equal));
        assert_eq!(compare_versions("1.0.0", "1.0.0-beta.2"), Some(Greater));
        assert_eq!(compare_versions("1.0.0-beta.10", "1.0.0-beta.2"), Some(Greater));
        assert_eq!(compare_versions("1.0.0-beta", "1.0.0-alpha.1"), Some(Greater));
        assert_eq!(compare_versions("1.0.0-alpha", "1.0.0-alpha.1"), Some(Less));
        assert_eq!(compare_versions("1.0.0-1", "1.0.0-alpha"), Some(Less));
        assert_eq!(compare_versions("abc", "1.0.0"), None);
        assert_eq!(compare_versions("1.0.0.0", "1.0.0"), None);
        assert_eq!(compare_versions("1..0", "1.0.0"), None);
        assert_eq!(compare_versions("", "1.0.0"), None);
    }

    const SAMPLE: &str = r###"{
        "url": "https://api.github.com/repos/addlayer-io/dbine/releases/1",
        "html_url": "https://github.com/addlayer-io/dbine/releases/tag/v0.1.2",
        "id": 1,
        "tag_name": "v0.1.2",
        "name": "DBine 0.1.2",
        "draft": false,
        "prerelease": false,
        "published_at": "2026-09-30T12:00:00Z",
        "assets": [],
        "body": "## Novedades\n\n- Ejecución de scripts"
    }"###;

    #[test]
    fn parses_a_release_and_compares() {
        let info = evaluate("0.1.1", parse_release(SAMPLE).unwrap()).unwrap();
        assert_eq!(
            info,
            UpdateInfo {
                current: "0.1.1".into(),
                latest: "0.1.2".into(),
                available: true,
                url: "https://github.com/addlayer-io/dbine/releases/tag/v0.1.2".into(),
                notes: "## Novedades\n\n- Ejecución de scripts".into(),
                published_at: Some("2026-09-30T12:00:00Z".into()),
                installable: false,
                reason: None,
                phase: "idle".into(),
                owner: None,
            }
        );
        assert!(!evaluate("0.1.2", parse_release(SAMPLE).unwrap()).unwrap().available);
        assert!(!evaluate("0.2.0", parse_release(SAMPLE).unwrap()).unwrap().available);
    }

    #[test]
    fn tolerates_missing_notes_and_rejects_bad_payloads() {
        let r = parse_release(r#"{"tag_name":"0.3.0","html_url":"https://github.com/x","body":null}"#).unwrap();
        let info = evaluate("0.2.9", r).unwrap();
        assert!(info.available);
        assert_eq!(info.notes, "");
        assert_eq!(info.published_at, None);
        assert!(parse_release(r#"{"message":"Not Found"}"#).is_err());
        assert!(parse_release("<html>").is_err());
        let bad_tag = parse_release(r#"{"tag_name":"latest","html_url":"https://github.com/x"}"#).unwrap();
        assert!(evaluate("0.1.0", bad_tag).is_err());
    }

    #[test]
    fn keeps_the_notes_of_newer_versions() {
        let body = "## Versión 0.1.10\n\n### Nuevo\n- a\n\n## Versión 0.1.9\n\n- b\n\n## Versión 0.1.8\n\n- c";
        assert_eq!(notes_since(body, "0.1.8"), "## Versión 0.1.10\n\n### Nuevo\n- a\n\n## Versión 0.1.9\n\n- b");
        assert_eq!(notes_since(body, "0.1.9"), "## Versión 0.1.10\n\n### Nuevo\n- a");
        assert_eq!(notes_since("Instaladores.\n- x", "0.1.8"), "Instaladores.\n- x");
    }

    #[test]
    fn truncates_long_notes_on_a_char_boundary() {
        assert_eq!(truncate("ñandú", 10), "ñandú");
        assert_eq!(truncate("ñandú ñandú", 6), "ñandú…");
        assert_eq!(truncate(&"á".repeat(NOTES_MAX + 1000), NOTES_MAX).chars().count(), NOTES_MAX + 1);
    }

    #[test]
    fn detects_the_placeholder_key() {
        assert!(is_placeholder_pubkey(PUBKEY_PLACEHOLDER));
        assert!(is_placeholder_pubkey(""));
        assert!(is_placeholder_pubkey("  "));
        assert!(!is_placeholder_pubkey(
            "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEM2M0U0QzU3MTZBMTY2QUQKUldTdFpxRVdWMHcreGp2MWg2RDZaRVd1YXFwekNKMDdlV0VWb3JLMnZoeituaXRVdkR0andNU24K"
        ));
    }

    #[test]
    fn the_shipped_config_has_a_real_key_and_no_updater_artifacts() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../../tauri.conf.json")).unwrap();
        let updater = &conf["plugins"]["updater"];
        let key = updater["pubkey"].as_str().unwrap();
        assert!(!is_placeholder_pubkey(key), "tauri.conf.json carries the placeholder key");
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD.decode(key).unwrap();
        assert!(String::from_utf8(decoded).unwrap().starts_with("untrusted comment: minisign public key"));
        assert_eq!(
            updater["endpoints"][0],
            "https://github.com/addlayer-io/dbine/releases/latest/download/latest.json"
        );
        assert_eq!(updater["requireSignedVersion"], true);
        // What the plugin reads at startup (a bad config panics there).
        let parsed: tauri_plugin_updater::Config = serde_json::from_value(updater.clone()).unwrap();
        assert!(parsed.require_signed_version);
        assert_eq!(parsed.endpoints.len(), 1);
        // Signing needs the private key: only release builds turn it on
        // (tauri.updater.conf.json), or every other build would fail.
        assert_eq!(conf["bundle"]["createUpdaterArtifacts"], false);
        let release: serde_json::Value =
            serde_json::from_str(include_str!("../../tauri.updater.conf.json")).unwrap();
        assert_eq!(release["bundle"]["createUpdaterArtifacts"], true);
    }

    #[test]
    fn self_update_only_from_an_installed_mac_app() {
        assert!(mac_location_ok("/Applications/DBine.app/Contents/MacOS/dbine"));
        assert!(mac_location_ok("/Users/ana/Apps/DBine.app/Contents/MacOS/dbine"));
        assert!(!mac_location_ok("/Users/ana/src/DBine/target/debug/dbine"));
        assert!(!mac_location_ok("/Volumes/DBine/DBine.app/Contents/MacOS/dbine"));
        assert!(!mac_location_ok(
            "/private/var/folders/xy/T/AppTranslocation/1234-ABCD/d/DBine.app/Contents/MacOS/dbine"
        ));
    }

    #[test]
    fn tag_urls_are_release_pages() {
        assert_eq!(tag_url("0.1.4"), "https://github.com/addlayer-io/dbine/releases/tag/v0.1.4");
        assert!(tag_url("0.1.4").starts_with(RELEASES_PREFIX));
    }

    #[test]
    fn bad_signatures_read_as_such() {
        use tauri_plugin_updater::Error as E;
        let sig = download_error(&E::MissingSignedVersion);
        assert_eq!(sig.to_string(), "la firma de la actualización no es válida");
        let sig = download_error(&E::SignedVersionMismatch { signed: "0.1.3".into(), announced: "0.1.4".into() });
        assert_eq!(sig.to_string(), "la firma de la actualización no es válida");
        let net = download_error(&E::Network("Download request failed with status: 404".into()));
        assert!(net.to_string().starts_with("no se pudo descargar la actualización: "));
    }

    /// Against the real GitHub API: `cargo test -p dbine updates -- --ignored`.
    #[test]
    #[ignore = "network"]
    fn live_latest_release() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let release = tauri::async_runtime::block_on(fetch_latest()).unwrap();
        println!("latest release: {} {} {:?}", release.tag_name, release.html_url, release.published_at);
        let latest = release.tag_name.trim_start_matches('v').to_string();
        let again = || release.clone();
        let old = evaluate("0.1.1", again()).unwrap();
        println!("0.1.1 -> {old:?}");
        assert!(old.available);
        assert!(old.url.starts_with(RELEASES_PREFIX));
        let same = evaluate(&latest, again()).unwrap();
        println!("{latest} -> available={}", same.available);
        assert!(!same.available);
        if latest == "0.1.2" {
            assert!(!evaluate("0.1.2", again()).unwrap().available);
        }
    }
}
