//! The app identifier changed from `com.addlayer.dbine` to
//! `io.addlayer.dbine`. Tauri, WebKit and WebView2 name the app's folders
//! after it, so at startup, before Tauri or a webview exists (and in the
//! headless task runner too), each folder with the old name moves to the new
//! one (docs/identifier-migration.md):
//!
//! - it moves only when the old folder exists and the new one doesn't;
//! - a rename first (atomic: old and new are siblings, one filesystem);
//!   across filesystems, a copy into a temporary sibling, checked, renamed
//!   into place and only then the old folder deleted. A copy interrupted
//!   before its rename starts over; one interrupted after it (the new folder
//!   carries [`COPIED`]) only deletes the old folder;
//! - while another DBine process is running (the old version still closing
//!   after an update, a scheduled task), nothing moves: its SQLite files may
//!   be open. This start uses the old folders for the state instead, and the
//!   new ones Tauri will create are marked [`PROVISIONAL`]: the next start
//!   sets them aside (never deletes them) and moves the old ones in.
//!
//! The keychain items (dbine-core `secrets`) and the macOS LaunchAgents
//! (`tasks::os`) move on their own.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub const OLD_ID: &str = "com.addlayer.dbine";
pub const NEW_ID: &str = "io.addlayer.dbine";

/// In a folder made by a copy: the copy is complete.
const COPIED: &str = ".dbine-copied-from-old-identifier";
/// In a new folder made while the old one couldn't move.
const PROVISIONAL: &str = ".dbine-provisional";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// No old folder.
    Nothing,
    /// Renamed.
    Moved,
    /// Copied across filesystems, then the old one deleted.
    Copied,
    /// A copy interrupted after its rename: the old folder is deleted now.
    Finished,
    /// The provisional new folder was set aside and the old one moved in.
    Replaced,
    /// Both exist and the new one is real: neither is touched.
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

/// Move `old` to `new` (see the module's rules).
pub fn migrate_dir(old: &Path, new: &Path) -> Outcome {
    // A copy that never reached its rename: our own partial work.
    let tmp = sibling(new, ".migrating");
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
        (true, false) => move_dir(old, new),
        (true, true) if new.join(COPIED).exists() => match std::fs::remove_dir_all(old) {
            Ok(()) => {
                let _ = std::fs::remove_file(new.join(COPIED));
                Outcome::Finished
            }
            Err(e) => Outcome::Failed(format!("removing the old folder after a copy: {e}")),
        },
        (true, true) if new.join(PROVISIONAL).exists() => {
            let stamp = chrono::Utc::now().format("%Y%m%d%H%M%S");
            let aside = sibling(new, &format!(".provisional-{stamp}"));
            if let Err(e) = std::fs::rename(new, &aside) {
                return Outcome::Failed(format!("setting the provisional folder aside: {e}"));
            }
            let _ = std::fs::remove_file(aside.join(PROVISIONAL));
            match move_dir(old, new) {
                Outcome::Moved | Outcome::Copied => Outcome::Replaced,
                other => other,
            }
        }
        (true, true) => Outcome::BothExist,
    }
}

fn move_dir(old: &Path, new: &Path) -> Outcome {
    match std::fs::rename(old, new) {
        Ok(()) => Outcome::Moved,
        Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => copy_move(old, new),
        // A file held open (Windows) or no permission: next start.
        Err(e) => Outcome::Failed(format!("rename: {e}")),
    }
}

/// Across filesystems: copy into a sibling, check it, rename it into place,
/// then delete the old folder.
fn copy_move(old: &Path, new: &Path) -> Outcome {
    let tmp = sibling(new, ".migrating");
    let copied = copy_tree(old, &tmp).and_then(|()| {
        if tree_listing(old)? == tree_listing(&tmp)? {
            Ok(())
        } else {
            Err(std::io::Error::other("the copy doesn't match the original"))
        }
    });
    let placed = copied
        .and_then(|()| std::fs::write(tmp.join(COPIED), b""))
        .and_then(|()| std::fs::rename(&tmp, new));
    if let Err(e) = placed {
        let _ = std::fs::remove_dir_all(&tmp);
        return Outcome::Failed(format!("copy: {e}"));
    }
    match std::fs::remove_dir_all(old) {
        Ok(()) => {
            let _ = std::fs::remove_file(new.join(COPIED));
            Outcome::Copied
        }
        // The marker stays: the next start finishes deleting.
        Err(e) => Outcome::Failed(format!("removing the old folder after a copy: {e}")),
    }
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        let kind = entry.file_type()?;
        if kind.is_dir() {
            copy_tree(&src, &dst)?;
        } else if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(&src)?, &dst)?;
            #[cfg(not(unix))]
            {
                std::fs::copy(&src, &dst)?;
            }
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    let _ = std::fs::set_permissions(to, std::fs::metadata(from)?.permissions());
    Ok(())
}

/// Every entry under `root`: relative path, and size for files.
fn tree_listing(root: &Path) -> std::io::Result<Vec<(PathBuf, Option<u64>)>> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Option<u64>)>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            if rel.as_os_str() == COPIED {
                continue;
            }
            let kind = entry.file_type()?;
            if kind.is_dir() {
                out.push((rel, None));
                walk(root, &path, out)?;
            } else {
                out.push((rel, Some(std::fs::symlink_metadata(&path)?.len())));
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, root, &mut out)?;
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
static REPORT: OnceLock<Vec<(String, Outcome)>> = OnceLock::new();
/// This start runs on the old folders (the migration was deferred).
static ON_OLD_DIRS: AtomicBool = AtomicBool::new(false);

/// Move the folders. `wait`: how long to wait for another DBine process to
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
    let report = migrate_pairs(&pairs, wait, provisional, others_running);
    if report.iter().any(|(_, o)| *o == Outcome::Deferred) {
        ON_OLD_DIRS.store(true, Ordering::SeqCst);
    }
    let _ = REPORT.set(report);
}

fn migrate_pairs(pairs: &[(PathBuf, PathBuf)], wait: Duration, provisional: bool, others: impl Fn() -> bool) -> Vec<(String, Outcome)> {
    let pending = |(old, new): &&(PathBuf, PathBuf)| old.exists() || sibling(new, ".migrating").exists();
    if !pairs.iter().any(|p| pending(&p)) {
        return Vec::new();
    }
    let start = Instant::now();
    let mut busy = others();
    while busy && start.elapsed() < wait {
        std::thread::sleep(Duration::from_millis(250));
        busy = others();
    }
    let mut report = Vec::new();
    for pair in pairs {
        let (old, new) = pair;
        if !pending(&pair) && !new.join(PROVISIONAL).exists() {
            continue;
        }
        let outcome = if busy && old.exists() {
            if provisional && !new.exists() && std::fs::create_dir_all(new).is_ok() {
                let _ = std::fs::write(new.join(PROVISIONAL), b"");
            }
            Outcome::Deferred
        } else {
            migrate_dir(old, new)
        };
        report.push((display(old), outcome));
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

/// Whether this start runs on the old folders (the migration was deferred).
pub fn on_old_dirs() -> bool {
    ON_OLD_DIRS.load(Ordering::SeqCst) && state_dirs(OLD_ID).is_some_and(|(c, _)| c.exists())
}

/// The state and data folders for this start: the old ones while the
/// migration is deferred.
pub fn state_dirs_now() -> Option<(PathBuf, PathBuf)> {
    if ON_OLD_DIRS.load(Ordering::SeqCst) {
        if let Some((config, data)) = state_dirs(OLD_ID).filter(|(c, _)| c.exists()) {
            return Some((config, data));
        }
    }
    state_dirs(NEW_ID)
}

/// Log what moved (once tracing is set up).
pub fn log_report() {
    for (old, outcome) in REPORT.get().map(|r| r.as_slice()).unwrap_or_default() {
        match outcome {
            Outcome::Nothing => {}
            Outcome::Moved | Outcome::Copied | Outcome::Finished | Outcome::Replaced => {
                tracing::info!(folder = %old, ?outcome, "identifier migration: {OLD_ID} -> {NEW_ID}")
            }
            Outcome::Deferred => tracing::warn!(folder = %old, "identifier migration deferred: another DBine process is running; this start uses the old folders"),
            Outcome::BothExist => tracing::warn!(folder = %old, "identifier migration: both the {OLD_ID} and {NEW_ID} folders exist; neither touched"),
            Outcome::Failed(e) => tracing::warn!(folder = %old, "identifier migration failed, retried next start: {e}"),
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
        std::fs::write(dir.join("dbine-state.sqlite"), b"state").unwrap();
        std::fs::write(dir.join("logs/app.log"), b"log line").unwrap();
    }

    fn pair(t: &Tmp) -> (PathBuf, PathBuf) {
        (t.0.join(OLD_ID), t.0.join(NEW_ID))
    }

    #[test]
    fn old_only_moves() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&old);
        assert_eq!(migrate_dir(&old, &new), Outcome::Moved);
        assert!(!old.exists());
        assert_eq!(std::fs::read(new.join("dbine-state.sqlite")).unwrap(), b"state");
        assert_eq!(std::fs::read(new.join("logs/app.log")).unwrap(), b"log line");
        // Idempotent.
        assert_eq!(migrate_dir(&old, &new), Outcome::Nothing);
        assert!(new.join("dbine-state.sqlite").exists());
    }

    #[test]
    fn new_only_is_left_alone() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&new);
        assert_eq!(migrate_dir(&old, &new), Outcome::Nothing);
        assert!(new.join("dbine-state.sqlite").exists());
    }

    #[test]
    fn both_touch_neither() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&old);
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("dbine-state.sqlite"), b"newer").unwrap();
        assert_eq!(migrate_dir(&old, &new), Outcome::BothExist);
        assert_eq!(std::fs::read(old.join("dbine-state.sqlite")).unwrap(), b"state");
        assert_eq!(std::fs::read(new.join("dbine-state.sqlite")).unwrap(), b"newer");
    }

    #[test]
    fn neither_does_nothing() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        assert_eq!(migrate_dir(&old, &new), Outcome::Nothing);
        assert!(!old.exists() && !new.exists());
    }

    #[test]
    fn copy_across_filesystems() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&old);
        #[cfg(unix)]
        std::os::unix::fs::symlink("dbine-state.sqlite", old.join("link")).unwrap();
        assert_eq!(copy_move(&old, &new), Outcome::Copied);
        assert!(!old.exists());
        assert!(!new.join(COPIED).exists());
        assert!(!sibling(&new, ".migrating").exists());
        assert_eq!(std::fs::read(new.join("logs/app.log")).unwrap(), b"log line");
        #[cfg(unix)]
        assert_eq!(std::fs::read_link(new.join("link")).unwrap(), PathBuf::from("dbine-state.sqlite"));
    }

    #[test]
    fn copy_interrupted_before_its_rename_starts_over() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&old);
        let tmp = sibling(&new, ".migrating");
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("dbine-state.sqlite"), b"sta").unwrap();
        assert_eq!(migrate_dir(&old, &new), Outcome::Moved);
        assert!(!tmp.exists());
        assert_eq!(std::fs::read(new.join("dbine-state.sqlite")).unwrap(), b"state");
    }

    #[test]
    fn copy_interrupted_after_its_rename_finishes() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&old);
        fill(&new);
        std::fs::write(new.join(COPIED), b"").unwrap();
        assert_eq!(migrate_dir(&old, &new), Outcome::Finished);
        assert!(!old.exists());
        assert!(!new.join(COPIED).exists());
        assert!(new.join("dbine-state.sqlite").exists());
    }

    #[test]
    fn deferred_while_another_process_runs_then_replaces_the_provisional_folder() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&old);
        let pairs = vec![(old.clone(), new.clone())];
        let report = migrate_pairs(&pairs, Duration::ZERO, true, || true);
        assert_eq!(report[0].1, Outcome::Deferred);
        assert!(old.join("dbine-state.sqlite").exists());
        assert!(new.join(PROVISIONAL).exists());
        // The deferred start wrote something in the new folder.
        std::fs::write(new.join("window-state.json"), b"{}").unwrap();
        let report = migrate_pairs(&pairs, Duration::ZERO, true, || false);
        assert_eq!(report[0].1, Outcome::Replaced);
        assert!(!old.exists());
        assert_eq!(std::fs::read(new.join("dbine-state.sqlite")).unwrap(), b"state");
        assert!(!new.join(PROVISIONAL).exists());
        // The provisional folder is kept aside, not deleted.
        let aside: Vec<_> = std::fs::read_dir(&t.0).unwrap().filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().contains(".provisional-")).collect();
        assert_eq!(aside.len(), 1);
        assert!(aside[0].path().join("window-state.json").exists());
    }

    #[test]
    fn waits_for_the_other_process() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&old);
        let calls = std::cell::Cell::new(0);
        let report = migrate_pairs(&[(old.clone(), new.clone())], Duration::from_secs(5), false, || {
            calls.set(calls.get() + 1);
            calls.get() < 3
        });
        assert_eq!(report[0].1, Outcome::Moved);
    }

    #[test]
    fn nothing_to_do_never_lists_processes() {
        let t = Tmp::new();
        let (old, new) = pair(&t);
        fill(&new);
        let report = migrate_pairs(&[(old, new)], Duration::ZERO, true, || panic!("not asked"));
        assert!(report.is_empty());
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
        let before: Vec<_> = pairs.iter().map(|(o, _)| o.exists().then(|| tree_listing(o).unwrap())).collect();
        for (old, outcome) in migrate_pairs(&pairs, Duration::ZERO, false, || false) {
            println!("{old}: {outcome:?}");
        }
        for ((old, new), listing) in pairs.iter().zip(before) {
            if let Some(listing) = listing {
                assert!(!old.exists());
                assert_eq!(tree_listing(new).unwrap(), listing, "{}", new.display());
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
