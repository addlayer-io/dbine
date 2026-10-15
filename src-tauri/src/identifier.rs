//! The app identifier changed from `com.addlayer.dbine` to
//! `io.addlayer.dbine`. Tauri, WebKit and WebView2 name the app's folders
//! after it, so at startup, before Tauri or a webview exists (and in the
//! headless task runner too), each folder with the old name is **copied** to
//! the new one (docs/identifier-migration.md). The old folders are never
//! changed: an installed older version keeps working on them, and going back
//! to it loses nothing.
//!
//! - it copies only when the old folder exists and the new one doesn't;
//! - into a temporary sibling, checked file by file, marked with [`COPIED`]
//!   and renamed into place: a copy interrupted before its rename starts over
//!   on the next start, and one that finished (the marker is there) never
//!   runs again;
//! - on macOS each file is an APFS clone (no extra space; a plain copy where
//!   cloning fails). Elsewhere the large re-downloadable folders ([`LARGE`]:
//!   AI models, downloaded drivers and native libraries) are left out when
//!   they're big, and downloaded again when needed;
//! - while another DBine process is running (the old version still closing
//!   after an update, an older version open, a scheduled task), nothing is
//!   copied: its SQLite files may be mid-write. That start uses the old
//!   folders, as before the change, and the new ones Tauri will create are
//!   marked [`PROVISIONAL`]: the next start sets them aside (never deletes
//!   them) and copies. A copy that fails is handled the same way.
//!
//! The keychain items (dbine-core `secrets`) are copied on their own; the
//! macOS LaunchAgents move (`tasks::os`), since two would run a task twice.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub const OLD_ID: &str = "com.addlayer.dbine";
pub const NEW_ID: &str = "io.addlayer.dbine";

/// In a new folder: the copy from the old one is complete (kept for good, so
/// it never runs again).
const COPIED: &str = ".dbine-copied-from-com.addlayer.dbine";
/// In a new folder made while the old one couldn't be copied.
const PROVISIONAL: &str = ".dbine-provisional";
/// Subfolders of the state folder that are downloaded again on demand: the
/// AI models (and llama.cpp), and `components` (driver hosts, DuckDB).
const LARGE: [&str; 2] = ["models", "components"];
/// Outside macOS (no clones), a [`LARGE`] folder above this is not copied.
const LARGE_LIMIT: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// No old folder, or it was copied before.
    Nothing,
    /// Copied (the subfolders left out to download again, if any).
    Copied { skipped: Vec<String> },
    /// The provisional new folder was set aside and the old one copied.
    Replaced { skipped: Vec<String> },
    /// Both exist and the new one wasn't made by a copy: neither is touched.
    BothExist,
    /// Left for the next start.
    Failed(String),
    /// Another DBine process is running: left for the next start.
    Deferred,
}

/// The app's folders under each base, as Tauri, WebKit and WebView2 name
/// them: `(old, new)`.
fn pairs_in(bases: &[PathBuf]) -> Vec<(PathBuf, PathBuf)> {
    let mut seen = Vec::new();
    for b in bases {
        let pair = (b.join(OLD_ID), b.join(NEW_ID));
        if !seen.contains(&pair) {
            seen.push(pair);
        }
    }
    seen
}

/// macOS: Application Support (config and data), Caches, Logs, and
/// WKWebView's storage (localStorage, IndexedDB) under `WebKit/<bundle id>`
/// and `HTTPStorages/<bundle id>` (cookies).
#[allow(dead_code)]
fn macos_bases(home: &Path) -> Vec<PathBuf> {
    let lib = home.join("Library");
    ["Application Support", "Caches", "Logs", "WebKit", "HTTPStorages"].iter().map(|d| lib.join(d)).collect()
}

/// Windows: `%APPDATA%` (config and data) and `%LOCALAPPDATA%` (cache, logs
/// and WebView2's `EBWebView`).
#[allow(dead_code)]
fn windows_bases(appdata: &Path, local: &Path) -> Vec<PathBuf> {
    vec![appdata.to_path_buf(), local.to_path_buf()]
}

/// Linux: the XDG config, data (webkit2gtk's storage too) and cache folders.
#[allow(dead_code)]
fn linux_bases(config: &Path, data: &Path, cache: &Path) -> Vec<PathBuf> {
    vec![config.to_path_buf(), data.to_path_buf(), cache.to_path_buf()]
}

fn env_path(k: &str) -> Option<PathBuf> {
    std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn home() -> Option<PathBuf> {
    env_path("HOME").or_else(|| env_path("USERPROFILE"))
}

/// The bases on this OS, from the environment (as Tauri resolves them).
fn bases() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    return home().map(|h| macos_bases(&h)).unwrap_or_default();
    #[cfg(windows)]
    {
        let home = home();
        let appdata = env_path("APPDATA").or_else(|| home.as_ref().map(|h| h.join("AppData/Roaming")));
        let local = env_path("LOCALAPPDATA").or_else(|| home.as_ref().map(|h| h.join("AppData/Local")));
        return match (appdata, local) {
            (Some(a), Some(l)) => windows_bases(&a, &l),
            _ => Vec::new(),
        };
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let home = home();
        let xdg = |k: &str, rel: &str| env_path(k).or_else(|| home.as_ref().map(|h| h.join(rel)));
        return match (xdg("XDG_CONFIG_HOME", ".config"), xdg("XDG_DATA_HOME", ".local/share"), xdg("XDG_CACHE_HOME", ".cache")) {
            (Some(c), Some(d), Some(k)) => linux_bases(&c, &d, &k),
            _ => Vec::new(),
        };
    }
    #[allow(unreachable_code)]
    Vec::new()
}

/// Where the app keeps its state (Tauri's `app_config_dir`) and its
/// downloaded components and models (`app_data_dir`), for `id`.
pub fn state_dirs(id: &str) -> Option<(PathBuf, PathBuf)> {
    #[cfg(target_os = "macos")]
    let (config, data) = {
        let base = home()?.join("Library/Application Support");
        (base.clone(), base)
    };
    #[cfg(windows)]
    let (config, data) = {
        let base = env_path("APPDATA").or_else(|| home().map(|h| h.join("AppData/Roaming")))?;
        (base.clone(), base)
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let (config, data) = (
        env_path("XDG_CONFIG_HOME").or_else(|| home().map(|h| h.join(".config")))?,
        env_path("XDG_DATA_HOME").or_else(|| home().map(|h| h.join(".local/share")))?,
    );
    Some((config.join(id), data.join(id)))
}

fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// How files are copied: an APFS clone first where there is one.
#[derive(Clone, Copy)]
struct Copier {
    clone: fn(&Path, &Path) -> std::io::Result<()>,
    /// Clones cost no space: the large folders are copied too.
    clones: bool,
}

impl Copier {
    fn for_this_os() -> Self {
        Copier { clone: clone_file, clones: cfg!(target_os = "macos") }
    }

    fn copy_file(&self, src: &Path, dst: &Path) -> std::io::Result<()> {
        if (self.clone)(src, dst).is_ok() {
            return Ok(());
        }
        // Not APFS (or another volume): a plain copy.
        let _ = std::fs::remove_file(dst);
        std::fs::copy(src, dst).map(|_| ())
    }
}

/// An APFS clone of one file (`clonefile(2)`): shares the blocks until either
/// side changes.
#[cfg(target_os = "macos")]
fn clone_file(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    extern "C" {
        fn clonefile(src: *const std::ffi::c_char, dst: *const std::ffi::c_char, flags: u32) -> std::ffi::c_int;
    }
    const CLONE_NOFOLLOW: u32 = 1;
    let s = CString::new(src.as_os_str().as_bytes())?;
    let d = CString::new(dst.as_os_str().as_bytes())?;
    // SAFETY: two valid NUL-terminated paths; clonefile only reads them.
    if unsafe { clonefile(s.as_ptr(), d.as_ptr(), CLONE_NOFOLLOW) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
fn clone_file(_src: &Path, _dst: &Path) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| match e.file_type() {
            Ok(t) if t.is_dir() => dir_size(&e.path()),
            Ok(t) if t.is_file() => e.metadata().map(|m| m.len()).unwrap_or(0),
            _ => 0,
        })
        .sum()
}

/// The [`LARGE`] top-level subfolders of `old` left out of the copy.
fn left_out(old: &Path, copier: Copier, limit: u64) -> Vec<String> {
    if copier.clones {
        return Vec::new();
    }
    LARGE.iter().filter(|n| old.join(n).is_dir() && dir_size(&old.join(n)) > limit).map(|n| n.to_string()).collect()
}

/// Copy `old` to `new` (see the module's rules). `old` is only read.
fn migrate_dir(old: &Path, new: &Path, copier: Copier, limit: u64) -> Outcome {
    // A copy that never reached its rename: our own partial work.
    let tmp = sibling(new, ".copying");
    if tmp.exists() {
        if let Err(e) = std::fs::remove_dir_all(&tmp) {
            return Outcome::Failed(format!("removing an unfinished copy: {e}"));
        }
    }
    match (old.exists(), new.exists()) {
        (false, _) => {
            // Nothing is coming: a provisional folder is the real one now.
            let _ = std::fs::remove_file(new.join(PROVISIONAL));
            Outcome::Nothing
        }
        (true, false) => match copy_dir(old, new, copier, limit) {
            Ok(skipped) => Outcome::Copied { skipped },
            Err(e) => Outcome::Failed(format!("copy: {e}")),
        },
        (true, true) if new.join(COPIED).exists() => Outcome::Nothing,
        (true, true) if new.join(PROVISIONAL).exists() => {
            let stamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
            let aside = sibling(new, &format!(".provisional-{stamp}"));
            if let Err(e) = std::fs::rename(new, &aside) {
                return Outcome::Failed(format!("setting the provisional folder aside: {e}"));
            }
            let _ = std::fs::remove_file(aside.join(PROVISIONAL));
            match copy_dir(old, new, copier, limit) {
                Ok(skipped) => Outcome::Replaced { skipped },
                Err(e) => Outcome::Failed(format!("copy: {e}")),
            }
        }
        (true, true) => Outcome::BothExist,
    }
}

/// Copy into a sibling, check it, mark it and rename it into place. Returns
/// the subfolders left out.
fn copy_dir(old: &Path, new: &Path, copier: Copier, limit: u64) -> std::io::Result<Vec<String>> {
    let tmp = sibling(new, ".copying");
    let skipped = left_out(old, copier, limit);
    let skip: Vec<PathBuf> = skipped.iter().map(PathBuf::from).collect();
    let done = copy_tree(old, &tmp, &tmp, &skip, copier)
        .and_then(|()| {
            if tree_listing(old, &skip)? == tree_listing(&tmp, &[])? {
                Ok(())
            } else {
                Err(std::io::Error::other("the copy doesn't match the original"))
            }
        })
        .and_then(|()| std::fs::write(tmp.join(COPIED), format!("{OLD_ID}\n{}\n", chrono::Utc::now().to_rfc3339())))
        .and_then(|()| std::fs::rename(&tmp, new));
    if let Err(e) = done {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    Ok(skipped)
}

/// Copy `from` into `to` (rooted at `root`), leaving out `skip` (relative to
/// the root).
fn copy_tree(from: &Path, to: &Path, root: &Path, skip: &[PathBuf], copier: Copier) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if skip.iter().any(|s| dst.strip_prefix(root).is_ok_and(|rel| rel == s)) {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&src, &dst, root, skip, copier)?;
        } else if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(&src)?, &dst)?;
            #[cfg(not(unix))]
            {
                std::fs::copy(&src, &dst)?;
            }
        } else {
            copier.copy_file(&src, &dst)?;
        }
    }
    let _ = std::fs::set_permissions(to, std::fs::metadata(from)?.permissions());
    Ok(())
}

/// Every entry under `root` but `skip` and the marker: relative path, and
/// size for files.
fn tree_listing(root: &Path, skip: &[PathBuf]) -> std::io::Result<Vec<(PathBuf, Option<u64>)>> {
    fn walk(root: &Path, dir: &Path, skip: &[PathBuf], out: &mut Vec<(PathBuf, Option<u64>)>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            if rel.as_os_str() == COPIED || skip.contains(&rel) {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                out.push((rel, None));
                walk(root, &path, skip, out)?;
            } else {
                out.push((rel, Some(std::fs::symlink_metadata(&path)?.len())));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, root, skip, &mut out)?;
    out.sort();
    Ok(out)
}

/// Other processes of this executable (the old version closing after an
/// update, a scheduled task), not this one.
fn others_running() -> bool {
    let Some(name) = std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())) else {
        return false;
    };
    let me = std::process::id();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let Ok(out) = std::process::Command::new("tasklist")
            .args(["/FO", "CSV", "/NH", "/FI", &format!("IMAGENAME eq {name}")])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
        else {
            return false;
        };
        // "dbine.exe","1234",...
        return String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.split(',').nth(1))
            .filter_map(|pid| pid.trim_matches('"').parse::<u32>().ok())
            .any(|pid| pid != me);
    }
    #[cfg(not(windows))]
    {
        let Ok(out) = std::process::Command::new("ps").args(["-A", "-o", "pid=,comm="]).output() else {
            return false;
        };
        String::from_utf8_lossy(&out.stdout).lines().any(|l| {
            let l = l.trim_start();
            let (pid, comm) = l.split_once(char::is_whitespace).unwrap_or((l, ""));
            let comm = comm.trim();
            // macOS prints the executable's path, Linux its first 15 bytes.
            let base = comm.rsplit('/').next().unwrap_or(comm);
            pid.parse::<u32>().is_ok_and(|p| p != me) && (base == name || name.get(..15).is_some_and(|short| name.len() > 15 && base == short))
        })
    }
}

/// What happened, logged once tracing is up (`log_report`).
static REPORT: OnceLock<Vec<(PathBuf, Outcome)>> = OnceLock::new();
/// This start runs on the old folders (the state's copy was deferred or
/// failed).
static ON_OLD_DIRS: AtomicBool = AtomicBool::new(false);

/// Copy the folders. `wait`: how long to wait for another DBine process to
/// exit before deferring (the GUI waits for an updater relaunch; the
/// headless runner doesn't). `provisional`: mark the new folders this start
/// will create when deferring (the GUI's webview and plugins create them).
pub fn run(wait: Duration, provisional: bool) {
    let pairs = pairs_in(&bases());
    if pairs.is_empty() {
        return;
    }
    // One migrating process at a time (the app and a scheduled task).
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(std::env::temp_dir().join(format!("{NEW_ID}.migration.lock")));
    let _guard = lock.ok().filter(|f| f.lock().is_ok());
    let report = migrate_pairs(&pairs, wait, provisional, Copier::for_this_os(), LARGE_LIMIT, others_running);
    let state = state_dirs(OLD_ID).map(|(config, _)| config);
    if report.iter().any(|(old, o)| Some(old) == state.as_ref() && matches!(o, Outcome::Deferred | Outcome::Failed(_))) {
        ON_OLD_DIRS.store(true, Ordering::SeqCst);
    }
    let _ = REPORT.set(report);
}

fn migrate_pairs(
    pairs: &[(PathBuf, PathBuf)],
    wait: Duration,
    provisional: bool,
    copier: Copier,
    limit: u64,
    others: impl Fn() -> bool,
) -> Vec<(PathBuf, Outcome)> {
    let pending = |old: &Path, new: &Path| (old.exists() && !new.join(COPIED).exists()) || sibling(new, ".copying").exists() || new.join(PROVISIONAL).exists();
    if !pairs.iter().any(|(o, n)| pending(o, n)) {
        return Vec::new();
    }
    let start = Instant::now();
    let mut busy = others();
    while busy && start.elapsed() < wait {
        std::thread::sleep(Duration::from_millis(250));
        busy = others();
    }
    let mut report = Vec::new();
    for (old, new) in pairs {
        if !pending(old, new) {
            continue;
        }
        let outcome = if busy && old.exists() { Outcome::Deferred } else { migrate_dir(old, new, copier, limit) };
        // The new folder this start creates is replaced on the next one.
        if provisional && matches!(outcome, Outcome::Deferred | Outcome::Failed(_)) && !new.exists() && std::fs::create_dir_all(new).is_ok() {
            let _ = std::fs::write(new.join(PROVISIONAL), b"");
        }
        report.push((old.clone(), outcome));
    }
    report
}

/// A folder for the log: the home folder as `~`.
fn display(p: &Path) -> String {
    match home() {
        Some(h) if p.starts_with(&h) => format!("~/{}", p.strip_prefix(&h).unwrap_or(p).display()),
        _ => p.display().to_string(),
    }
}

/// Whether this start runs on the old folders (the state's copy was
/// deferred or failed): nothing that belongs to the new identifier only
/// (the LaunchAgents' labels) changes then.
pub fn on_old_dirs() -> bool {
    ON_OLD_DIRS.load(Ordering::SeqCst) && state_dirs(OLD_ID).is_some_and(|(c, _)| c.exists())
}

/// The state and data folders for this start: the old ones while the copy
/// is pending.
pub fn state_dirs_now() -> Option<(PathBuf, PathBuf)> {
    if on_old_dirs() {
        return state_dirs(OLD_ID);
    }
    state_dirs(NEW_ID)
}

/// Log what was copied (once tracing is set up). Folder paths only.
pub fn log_report() {
    for (old, outcome) in REPORT.get().map(|r| r.as_slice()).unwrap_or_default() {
        let folder = display(old);
        match outcome {
            Outcome::Nothing => {}
            Outcome::Copied { skipped } | Outcome::Replaced { skipped } => {
                tracing::info!(%folder, ?outcome, "identifier migration: copied {OLD_ID} -> {NEW_ID} (the old folder is kept)");
                if !skipped.is_empty() {
                    tracing::info!(%folder, ?skipped, "identifier migration: large folders not copied, downloaded again when needed");
                }
            }
            Outcome::Deferred => tracing::warn!(%folder, "identifier migration deferred: another DBine process is running; this start uses the old folders"),
            Outcome::BothExist => tracing::warn!(%folder, "identifier migration: both the {OLD_ID} and {NEW_ID} folders exist; neither touched"),
            Outcome::Failed(e) => tracing::warn!(%folder, "identifier migration failed, retried next start: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("dbine-idmig-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fill(dir: &Path) {
        std::fs::create_dir_all(dir.join("logs")).unwrap();
        std::fs::create_dir_all(dir.join("models")).unwrap();
        std::fs::write(dir.join("dbine-state.sqlite"), b"state").unwrap();
        std::fs::write(dir.join("logs/app.log"), b"log line").unwrap();
        std::fs::write(dir.join("models/m.gguf"), vec![7u8; 4096]).unwrap();
    }

    fn pair(t: &Tmp) -> (PathBuf, PathBuf) {
        (t.0.join(OLD_ID), t.0.join(NEW_ID))
    }

    fn failing_clone(_: &Path, _: &Path) -> std::io::Result<()> {
        Err(std::io::ErrorKind::Unsupported.into())
    }

    /// No clones: the large folders are left out above the limit.
    const PLAIN: Copier = Copier { clone: failing_clone, clones: false };
    const CLONING: Copier = Copier { clone: failing_clone, clones: true };
    const NO_LIMIT: u64 = u64::MAX;

    #[test]
    fn old_only_is_copied_and_kept() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&old);
        let before = tree_listing(&old, &[]).unwrap();
        assert_eq!(migrate_dir(&old, &new, PLAIN, NO_LIMIT), Outcome::Copied { skipped: vec![] });
        // The old folder is untouched.
        assert_eq!(tree_listing(&old, &[]).unwrap(), before);
        assert_eq!(std::fs::read(old.join("dbine-state.sqlite")).unwrap(), b"state");
        assert_eq!(std::fs::read(new.join("dbine-state.sqlite")).unwrap(), b"state");
        assert_eq!(std::fs::read(new.join("logs/app.log")).unwrap(), b"log line");
        assert!(new.join(COPIED).exists());
        assert!(!sibling(&new, ".copying").exists());
    }

    #[test]
    fn copies_only_once() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&old);
        migrate_dir(&old, &new, PLAIN, NO_LIMIT);
        // The new app changes its copy; the old app its own.
        std::fs::write(new.join("dbine-state.sqlite"), b"newer").unwrap();
        std::fs::write(old.join("dbine-state.sqlite"), b"older app").unwrap();
        assert_eq!(migrate_dir(&old, &new, PLAIN, NO_LIMIT), Outcome::Nothing);
        assert_eq!(std::fs::read(new.join("dbine-state.sqlite")).unwrap(), b"newer");
        assert_eq!(std::fs::read(old.join("dbine-state.sqlite")).unwrap(), b"older app");
        // And no process listing is needed any more.
        assert!(migrate_pairs(&[(old, new)], Duration::ZERO, true, PLAIN, NO_LIMIT, || panic!("not asked")).is_empty());
    }

    #[test]
    fn new_only_is_left_alone() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&new);
        assert_eq!(migrate_dir(&old, &new, PLAIN, NO_LIMIT), Outcome::Nothing);
        assert!(new.join("dbine-state.sqlite").exists());
        assert!(!old.exists());
    }

    #[test]
    fn both_touch_neither() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&old);
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("dbine-state.sqlite"), b"newer").unwrap();
        assert_eq!(migrate_dir(&old, &new, PLAIN, NO_LIMIT), Outcome::BothExist);
        assert_eq!(std::fs::read(old.join("dbine-state.sqlite")).unwrap(), b"state");
        assert_eq!(std::fs::read(new.join("dbine-state.sqlite")).unwrap(), b"newer");
    }

    #[test]
    fn neither_does_nothing() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        assert_eq!(migrate_dir(&old, &new, PLAIN, NO_LIMIT), Outcome::Nothing);
        assert!(!old.exists() && !new.exists());
    }

    #[test]
    fn interrupted_copy_starts_over() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&old);
        let tmp = sibling(&new, ".copying");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("dbine-state.sqlite"), b"sta").unwrap();
        assert_eq!(migrate_dir(&old, &new, PLAIN, NO_LIMIT), Outcome::Copied { skipped: vec![] });
        assert!(!tmp.exists());
        assert_eq!(std::fs::read(new.join("dbine-state.sqlite")).unwrap(), b"state");
    }

    #[test]
    fn large_folders_left_out_without_clones() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&old);
        // models/ is 4 KiB: above a 1 KiB limit.
        assert_eq!(migrate_dir(&old, &new, PLAIN, 1024), Outcome::Copied { skipped: vec!["models".into()] });
        assert!(!new.join("models").exists());
        assert!(new.join("dbine-state.sqlite").exists());
        assert!(old.join("models/m.gguf").exists());
        // Small: copied.
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&old);
        assert_eq!(migrate_dir(&old, &new, PLAIN, 1 << 20), Outcome::Copied { skipped: vec![] });
        assert!(new.join("models/m.gguf").exists());
        // With clones nothing is left out, whatever the size.
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&old);
        assert_eq!(migrate_dir(&old, &new, CLONING, 1024), Outcome::Copied { skipped: vec![] });
        assert_eq!(std::fs::read(new.join("models/m.gguf")).unwrap(), vec![7u8; 4096]);
    }

    #[test]
    fn clone_failure_falls_back_to_a_copy() {
        let t = Tmp::new();
        std::fs::write(t.0.join("a"), b"data").unwrap();
        // A clone that fails after leaving something behind.
        fn half_clone(_: &Path, dst: &Path) -> std::io::Result<()> {
            std::fs::write(dst, b"x")?;
            Err(std::io::Error::other("clone failed"))
        }
        Copier { clone: half_clone, clones: true }.copy_file(&t.0.join("a"), &t.0.join("b")).unwrap();
        assert_eq!(std::fs::read(t.0.join("b")).unwrap(), b"data");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn apfs_clone_copies_the_file() {
        let t = Tmp::new();
        std::fs::write(t.0.join("a"), b"data").unwrap();
        Copier::for_this_os().copy_file(&t.0.join("a"), &t.0.join("b")).unwrap();
        assert_eq!(std::fs::read(t.0.join("b")).unwrap(), b"data");
        // Independent copies.
        std::fs::write(t.0.join("b"), b"changed").unwrap();
        assert_eq!(std::fs::read(t.0.join("a")).unwrap(), b"data");
    }

    #[test]
    fn symlinks_are_kept() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&old);
        #[cfg(unix)]
        std::os::unix::fs::symlink("dbine-state.sqlite", old.join("link")).unwrap();
        assert!(matches!(migrate_dir(&old, &new, PLAIN, NO_LIMIT), Outcome::Copied { .. }));
        #[cfg(unix)]
        assert_eq!(std::fs::read_link(new.join("link")).unwrap(), PathBuf::from("dbine-state.sqlite"));
    }

    #[test]
    fn deferred_while_another_process_runs_then_replaces_the_provisional_folder() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&old);
        let pairs = vec![(old.clone(), new.clone())];
        let report = migrate_pairs(&pairs, Duration::ZERO, true, PLAIN, NO_LIMIT, || true);
        assert_eq!(report[0].1, Outcome::Deferred);
        assert!(old.join("dbine-state.sqlite").exists());
        assert!(new.join(PROVISIONAL).exists());
        assert!(!new.join("dbine-state.sqlite").exists());
        // The deferred start wrote something in the new folder.
        std::fs::write(new.join("window-state.json"), b"{}").unwrap();
        let report = migrate_pairs(&pairs, Duration::ZERO, true, PLAIN, NO_LIMIT, || false);
        assert_eq!(report[0].1, Outcome::Replaced { skipped: vec![] });
        assert!(old.join("dbine-state.sqlite").exists());
        assert_eq!(std::fs::read(new.join("dbine-state.sqlite")).unwrap(), b"state");
        assert!(!new.join(PROVISIONAL).exists());
        // The provisional folder is kept aside, not deleted.
        let aside: Vec<_> = std::fs::read_dir(&t.0).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().contains(".provisional-")).collect();
        assert_eq!(aside.len(), 1);
        assert!(aside[0].path().join("window-state.json").exists());
    }

    #[test]
    fn deferred_headless_creates_nothing() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&old);
        let report = migrate_pairs(&[(old, new.clone())], Duration::ZERO, false, PLAIN, NO_LIMIT, || true);
        assert_eq!(report[0].1, Outcome::Deferred);
        assert!(!new.exists());
    }

    #[test]
    fn waits_for_the_other_process() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&old);
        let calls = std::cell::Cell::new(0);
        let report = migrate_pairs(&[(old, new)], Duration::from_secs(5), false, PLAIN, NO_LIMIT, || {
            calls.set(calls.get() + 1);
            calls.get() < 3
        });
        assert_eq!(report[0].1, Outcome::Copied { skipped: vec![] });
    }

    #[test]
    fn folders_per_os() {
        let mac = pairs_in(&macos_bases(Path::new("/Users/u")));
        assert!(mac.contains(&(PathBuf::from("/Users/u/Library/Application Support/com.addlayer.dbine"), PathBuf::from("/Users/u/Library/Application Support/io.addlayer.dbine"))));
        assert!(mac.iter().any(|(o, _)| o == Path::new("/Users/u/Library/WebKit/com.addlayer.dbine")));
        assert!(mac.iter().any(|(o, _)| o == Path::new("/Users/u/Library/Caches/com.addlayer.dbine")));
        assert_eq!(pairs_in(&windows_bases(Path::new("C:/a"), Path::new("C:/l"))).len(), 2);
        // Config and data may be the same folder: listed once.
        assert_eq!(pairs_in(&linux_bases(Path::new("/h/.config"), Path::new("/h/.config"), Path::new("/h/.cache"))).len(), 2);
    }

    /// A dry run on copies of real folders: `DBINE_MIGRATION_HOME=<a HOME
    /// with copies of ~/Library/...> cargo test -p dbine --lib
    /// identifier_dry_run -- --ignored --nocapture`. Never the real HOME.
    #[test]
    #[ignore]
    fn identifier_dry_run() {
        let home = PathBuf::from(std::env::var_os("DBINE_MIGRATION_HOME").expect("DBINE_MIGRATION_HOME"));
        assert_ne!(Some(home.clone()), super::home(), "never the real HOME");
        let pairs = pairs_in(&macos_bases(&home));
        let before: Vec<_> = pairs.iter().map(|(o, _)| o.exists().then(|| tree_listing(o, &[]).unwrap())).collect();
        for (old, outcome) in migrate_pairs(&pairs, Duration::ZERO, false, Copier::for_this_os(), LARGE_LIMIT, || false) {
            println!("{}: {outcome:?}", old.display());
        }
        for ((old, new), listing) in pairs.iter().zip(before) {
            if let Some(listing) = listing {
                assert_eq!(tree_listing(old, &[]).unwrap(), listing, "old changed: {}", old.display());
                assert_eq!(tree_listing(new, &[]).unwrap(), listing, "{}", new.display());
                println!("{} -> {}: {} entries, identical", old.display(), new.display(), listing.len());
            }
        }
    }

    #[test]
    fn identifier_matches_the_config() {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(conf["identifier"], NEW_ID);
    }
}
