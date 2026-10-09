//! Anonymous usage telemetry (Aptabase), on by default: the UI tells once
//! what's sent and Configuración › General turns it off; `DO_NOT_TRACK=1` or
//! `DBINE_TELEMETRY=0` in the environment turn it off for a whole machine
//! (managed installs). What can leave is fixed
//! here, not by the caller: the event name from a closed list, the engine id
//! of a connection (never host, user, database or queries), the workbench
//! module opened (a tab kind or side panel from a closed list), the feature
//! used (a closed list, plus the kind of AI provider for the assistant), the app version,
//! the OS name and version and the UI language. There is no install or user
//! id: Aptabase groups events by a random session that lives while the app is
//! used. The country is derived by Aptabase from the request; the IP isn't
//! stored. What's sent is documented in docs/telemetry.md.

use crate::error::CommandResult;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const APP_KEY: &str = "A-US-9260684417";
const INGEST_URL: &str = "https://us.aptabase.com/api/v0/events";
/// After this long without events, the next one starts a new session.
const SESSION_IDLE: Duration = Duration::from_secs(4 * 60 * 60);

/// The workbench modules `module_opened` can name: the tab kinds of the UI
/// (`web/src/stores/tabs.ts`) and the side panels that aren't tabs.
const MODULES: &[&str] = &[
    "query", "object", "designer", "diagram", "monitor", "profiler", "migration", "connection", "compare",
    "dataCompare", "security", "backups", "indexes", "dependencies", "file", "fileDiff",
    // side panels
    "ai", "projects", "library", "history",
];

/// What `feature_used` can name: an action done, not a screen opened.
const FEATURES: &[&str] = &["ai_message", "mcp_tool", "schema_sync", "data_sync", "migration_run", "multi_db_run"];

/// The kind of AI provider an `ai_message` used (never the model or the text).
const PROVIDERS: &[&str] = &["embedded", "ollama", "lm_studio", "claude_code", "codex"];

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackEventArgs {
    /// `app_started`, `connection_opened`, `module_opened` or `feature_used`.
    pub event: String,
    /// The driver id, for `connection_opened`.
    pub engine: Option<String>,
    /// The module, for `module_opened`.
    #[serde(default)]
    pub module: Option<String>,
    /// The feature, for `feature_used`.
    #[serde(default)]
    pub feature: Option<String>,
    /// The AI provider kind, for `feature_used` / `ai_message`.
    #[serde(default)]
    pub provider: Option<String>,
    /// The UI language (`es`, `en`…).
    pub locale: String,
}

/// Send one event in the background. Failures (offline, blocked) are dropped:
/// telemetry never gets in the way.
#[tauri::command(rename_all = "camelCase")]
pub async fn track_event(app: tauri::AppHandle, args: TrackEventArgs) -> CommandResult<()> {
    if disabled_by_env() {
        return Ok(());
    }
    let mut props = Map::new();
    match args.event.as_str() {
        "feature_used" => match feature_props(args.feature.as_deref(), args.provider.as_deref()) {
            Some(p) => props = p,
            None => return Ok(()),
        },
        "app_started" => {}
        "connection_opened" => {
            let engine = args.engine.unwrap_or_default();
            if dbine_drivers::find(&engine).is_none() {
                return Ok(());
            }
            props.insert("engine".into(), Value::String(engine));
        }
        "module_opened" => {
            let module = args.module.unwrap_or_default();
            if !MODULES.contains(&module.as_str()) {
                return Ok(());
            }
            props.insert("module".into(), Value::String(module));
        }
        // Anything else isn't on the list: it never leaves.
        _ => return Ok(()),
    }
    post(&args.event, props, &args.locale, &app.package_info().version.to_string());
    Ok(())
}

/// `feature_used`'s props, or `None` when something isn't on the lists.
fn feature_props(feature: Option<&str>, provider: Option<&str>) -> Option<Map<String, Value>> {
    let feature = feature.filter(|f| FEATURES.contains(f))?;
    let mut props = Map::new();
    props.insert("feature".into(), Value::String(feature.into()));
    if feature == "ai_message" {
        let provider = provider.filter(|p| PROVIDERS.contains(p))?;
        props.insert("provider".into(), Value::String(provider.into()));
    }
    Some(props)
}

/// A feature used where there's no UI to send it from (the MCP server):
/// once per run, only when the user hasn't turned telemetry off.
pub fn track_backend_feature(state: &crate::state::AppState, feature: &'static str) {
    static SENT: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    // Tests run the MCP server: they never report.
    if cfg!(test) || disabled_by_env() || state.store.get_setting("telemetry.consent").ok().flatten() == Some(Value::Bool(false)) {
        return;
    }
    {
        let mut sent = SENT.lock().unwrap_or_else(|e| e.into_inner());
        if sent.contains(&feature) {
            return;
        }
        sent.push(feature);
    }
    if let Some(props) = feature_props(Some(feature), None) {
        post("feature_used", props, "", env!("CARGO_PKG_VERSION"));
    }
}

/// Send an event already checked against the lists, in the background.
fn post(event: &str, props: Map<String, Value>, locale: &str, version: &str) {
    let locale: String = locale.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(16).collect();
    let (os_name, os_version) = os();
    let event = json!([{
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        "sessionId": session_id(),
        "eventName": event,
        "systemProps": {
            "isDebug": cfg!(debug_assertions),
            "osName": os_name,
            "osVersion": os_version,
            "locale": locale,
            "appVersion": version,
            "sdkVersion": concat!("dbine@", env!("CARGO_PKG_VERSION")),
        },
        "props": props,
    }]);
    tauri::async_runtime::spawn(async move {
        let sent = client().post(INGEST_URL).header("App-Key", APP_KEY).json(&event).send().await;
        // Logged (not shown): the only way to tell why a machine's events
        // never arrive (proxy, firewall, TLS). The event carries nothing
        // identifying, and neither does the error.
        if let Err(e) = sent.and_then(|r| r.error_for_status()) {
            // The whole chain: "error sending request" alone hides the cause.
            let mut why = e.to_string();
            let mut src = std::error::Error::source(&e);
            while let Some(s) = src {
                why.push_str(": ");
                why.push_str(&s.to_string());
                src = s.source();
            }
            tracing::warn!("telemetry event not sent: {why}");
        }
    });
}

/// `DO_NOT_TRACK` (the common convention, any value but empty or `0`) or
/// `DBINE_TELEMETRY` set to `0`, `false` or `off`.
fn disabled_by_env() -> bool {
    let set = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_ascii_lowercase());
    set("DO_NOT_TRACK").is_some_and(|v| !v.is_empty() && v != "0" && v != "false")
        || set("DBINE_TELEMETRY").is_some_and(|v| matches!(v.as_str(), "0" | "false" | "off" | "no"))
}

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| reqwest::Client::builder().timeout(Duration::from_secs(15)).build().unwrap_or_default())
}

/// Aptabase's format: epoch seconds followed by 8 random digits.
fn session_id() -> String {
    static SESSION: Mutex<Option<(String, Instant)>> = Mutex::new(None);
    let mut s = SESSION.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    match s.as_mut() {
        Some((id, last)) if now.duration_since(*last) < SESSION_IDLE => {
            *last = now;
            id.clone()
        }
        _ => {
            let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
            let random = u64::from_le_bytes(uuid::Uuid::new_v4().as_bytes()[..8].try_into().unwrap_or_default()) % 100_000_000;
            let id = (secs * 100_000_000 + random).to_string();
            *s = Some((id.clone(), now));
            id
        }
    }
}

/// OS name and version, read once.
fn os() -> (&'static str, &'static str) {
    static OS: OnceLock<(&'static str, String)> = OnceLock::new();
    let (name, version) = OS.get_or_init(|| (os_name(), os_version()));
    (name, version)
}

fn os_name() -> &'static str {
    match std::env::consts::OS {
        "macos" => "macOS",
        "windows" => "Windows",
        "linux" => "Linux",
        other => other,
    }
}

#[cfg(target_os = "macos")]
fn os_version() -> String {
    std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

/// `ver` prints "Microsoft Windows [Version 10.0.22631.4460]".
#[cfg(target_os = "windows")]
fn os_version() -> String {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("cmd").args(["/C", "ver"]).creation_flags(CREATE_NO_WINDOW).output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    text.rsplit(' ').next().unwrap_or_default().trim().trim_end_matches(']').to_string()
}

/// The distribution and its version, e.g. "ubuntu 24.04".
#[cfg(target_os = "linux")]
fn os_version() -> String {
    let release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let field = |key: &str| {
        release
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
            .map(|v| v.trim_matches('"').to_string())
            .unwrap_or_default()
    };
    format!("{} {}", field("ID"), field("VERSION_ID")).trim().to_string()
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn os_version() -> String {
    String::new()
}

#[cfg(test)]
mod feature_tests {
    use super::*;

    #[test]
    fn only_listed_features_and_providers_leave() {
        let p = feature_props(Some("schema_sync"), None).unwrap();
        assert_eq!(p.get("feature"), Some(&Value::String("schema_sync".into())));
        assert!(p.get("provider").is_none());
        let p = feature_props(Some("ai_message"), Some("embedded")).unwrap();
        assert_eq!(p.get("provider"), Some(&Value::String("embedded".into())));
        assert!(feature_props(Some("ai_message"), Some("qwen2.5-coder-32b")).is_none(), "a model name never leaves");
        assert!(feature_props(Some("ai_message"), None).is_none());
        assert!(feature_props(Some("select * from people"), None).is_none());
        assert!(MODULES.contains(&"ai") && MODULES.contains(&"indexes"));
    }
}
