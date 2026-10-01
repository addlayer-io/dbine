//! Update check: asks GitHub for the latest published release and compares
//! its version with the running app's. Nothing is downloaded or installed
//! here (the binaries aren't signed): the UI offers the release page, which
//! `open_release_page` opens in the system browser.

use crate::error::{CommandError, CommandResult};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::sync::OnceLock;
use std::time::Duration;

const LATEST_URL: &str = "https://api.github.com/repos/addlayer-io/dbine/releases/latest";
/// The only pages `open_release_page` opens.
const RELEASES_PREFIX: &str = "https://github.com/addlayer-io/dbine/releases/";
/// Release notes past this many characters are cut (the dialog shows a summary).
const NOTES_MAX: usize = 2000;

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct CheckForUpdateArgs {}

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

#[tauri::command(rename_all = "camelCase")]
pub async fn check_for_update(app: tauri::AppHandle, args: CheckForUpdateArgs) -> CommandResult<UpdateInfo> {
    let _ = args;
    let release = fetch_latest().await?;
    evaluate(&app.package_info().version.to_string(), release)
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
        notes: truncate(release.body.as_deref().unwrap_or("").trim(), NOTES_MAX),
        published_at: release.published_at,
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
    fn truncates_long_notes_on_a_char_boundary() {
        assert_eq!(truncate("ñandú", 10), "ñandú");
        assert_eq!(truncate("ñandú ñandú", 6), "ñandú…");
        assert_eq!(truncate(&"á".repeat(3000), NOTES_MAX).chars().count(), NOTES_MAX + 1);
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
