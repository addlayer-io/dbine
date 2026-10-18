//! Which local files a "Resultados del notebook" cell may read
//! (docs/notebooks.md › Reading local files › Approval).
//!
//! - **Always blocked** ([`Policy`]): credentials, keys, shell history and
//!   rc files, browsers' profiles, system configuration and DBine's own
//!   folders. Checked on the canonical path (symlinks resolved), so a link
//!   into one of them is blocked too. Never asked: the run is refused.
//! - **Granted** ("Permitir siempre"): the pair (notebook file, file) or
//!   (notebook file, pattern) in the local state
//!   ([`dbine_core::file_grants`]), never in the `.ipynb`.
//! - **Approved for one run** ("Permitir"): the backend hands the UI a
//!   one-time token for the files it asked about ([`ask`]); the UI's
//!   decision goes to [`decide`], and the run that comes back with the
//!   token ([`take`]) may read those files and no others: the files are
//!   resolved again and checked against them ([`Access::gate`]).
//! - **Content change notice** ([`changes`]): size, modification time and
//!   SHA-256 of each file read, per notebook; the hash is computed only
//!   when the size or the time changed.
//!
//! Only the notebook view reaches this: no MCP tool or assistant tool runs
//! a results cell.

use super::files::{pattern_matches, DataFile};
use crate::error::{CommandError, CommandResult};
use dbine_core::file_grants::{FileGrant, FileRecord};
use dbine_core::StateStore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Always blocked

/// Folders under the home folder no notebook reads (any file below them).
pub const HOME_DIRS: &[&str] = &[
    ".ssh",
    ".aws",
    ".gnupg",
    ".config",
    ".kube",
    ".docker",
    ".azure",
    ".gcloud",
    ".oci",
    ".password-store",
    ".mozilla",
    ".local/share/keyrings",
    "Library/Keychains",
    "Library/Cookies",
    "Library/Safari",
    "Library/Containers/com.apple.Safari",
    "Library/Application Support/Google/Chrome",
    "Library/Application Support/Chromium",
    "Library/Application Support/BraveSoftware",
    "Library/Application Support/Microsoft Edge",
    "Library/Application Support/Firefox",
    "Library/Application Support/Vivaldi",
    "Library/Application Support/Arc",
    "Library/Application Support/com.operasoftware.Opera",
    // Windows, under %USERPROFILE%.
    "AppData/Roaming/Microsoft/Credentials",
    "AppData/Local/Microsoft/Credentials",
    "AppData/Roaming/Microsoft/Protect",
    "AppData/Roaming/Microsoft/Crypto",
    "AppData/Local/Microsoft/Vault",
    "AppData/Local/Google/Chrome/User Data",
    "AppData/Local/Microsoft/Edge/User Data",
    "AppData/Local/BraveSoftware",
    "AppData/Roaming/Mozilla",
    "AppData/Roaming/Opera Software",
    "AppData/Roaming/postgresql",
];

/// Folders of the system no notebook reads.
#[cfg(unix)]
pub const SYSTEM_DIRS: &[&str] = &["/etc", "/private/etc", "/var", "/private/var"];
#[cfg(not(unix))]
pub const SYSTEM_DIRS: &[&str] = &[];

/// Windows' own folders under `%APPDATA%` / `%LOCALAPPDATA%` (they may be
/// redirected away from the home folder).
const ROAMING_DIRS: &[&str] = &["Microsoft/Credentials", "Microsoft/Protect", "Microsoft/Crypto", "Mozilla", "postgresql"];
const LOCAL_DIRS: &[&str] = &["Microsoft/Credentials", "Microsoft/Vault", "Google/Chrome/User Data", "Microsoft/Edge/User Data", "BraveSoftware"];

/// Credential files blocked wherever they are (lowercase).
pub const CREDENTIAL_NAMES: &[&str] = &[".netrc", "_netrc", ".npmrc", ".pypirc", ".git-credentials", ".pgpass", "pgpass.conf", ".my.cnf"];

/// A file directly in the home folder that is shell history or an rc file:
/// `.*_history`, `.zsh*`, `.bash*`, `.profile`…
fn home_file_blocked(name: &str) -> bool {
    let n = name.to_lowercase();
    (n.starts_with('.') && n.ends_with("_history"))
        || n.starts_with(".zsh")
        || n.starts_with(".bash")
        || matches!(n.as_str(), ".profile" | ".zprofile" | ".zlogin" | ".zlogout" | ".history" | ".lesshst" | ".viminfo")
}

/// `.env` or `.env.*` (lowercase).
fn env_file(name: &str) -> bool {
    let n = name.to_lowercase();
    n == ".env" || n.starts_with(".env.")
}

/// The files no notebook may read (see the module docs).
#[derive(Debug, Clone, Default)]
pub struct Policy {
    /// Each blocked folder (as written and canonical) with what the
    /// message calls it (`~/.ssh`).
    dirs: Vec<(PathBuf, String)>,
    /// The home folder (canonical), for the files directly in it.
    home: Option<PathBuf>,
    /// Canonical targets of the home folder's blocked files that are
    /// symlinks (a `~/.zshrc` linked into a dotfiles repo).
    linked: Vec<(PathBuf, String)>,
}

/// Lowercase components, to compare paths ignoring case (macOS and
/// Windows file systems are case-insensitive: `~/.SSH` is `~/.ssh`).
fn parts(p: &Path) -> Vec<String> {
    p.components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect()
}

fn under(path: &[String], dir: &[String]) -> bool {
    !dir.is_empty() && path.len() > dir.len() && path[..dir.len()] == *dir
}

impl Policy {
    /// This machine's: the home folder, the system's folders and DBine's
    /// own (`app_dirs`: its state, data, cache and logs).
    pub fn current(app_dirs: &[PathBuf]) -> Self {
        let home = super::files::home_dir();
        let mut extra: Vec<(PathBuf, String)> = Vec::new();
        for (var, list) in [("APPDATA", ROAMING_DIRS), ("LOCALAPPDATA", LOCAL_DIRS)] {
            if let Some(base) = std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()) {
                for d in list {
                    let p = base.join(d);
                    extra.push((p.clone(), format!("%{var}%\\{}", d.replace('/', "\\"))));
                }
            }
        }
        Self::new(home.as_deref(), app_dirs, SYSTEM_DIRS, extra)
    }

    /// The rules for `home` (none: only the system's and DBine's folders),
    /// `app_dirs`, `system` folders and `extra` ones.
    pub fn new(home: Option<&Path>, app_dirs: &[PathBuf], system: &[&str], extra: Vec<(PathBuf, String)>) -> Self {
        let mut dirs: Vec<(PathBuf, String)> = Vec::new();
        let mut add = |p: PathBuf, label: String| {
            if let Ok(c) = std::fs::canonicalize(&p) {
                let c = super::files::plain_path(c);
                if c != p {
                    dirs.push((c, label.clone()));
                }
            }
            dirs.push((p, label));
        };
        let home_canon = home.map(|h| std::fs::canonicalize(h).map(super::files::plain_path).unwrap_or_else(|_| h.to_path_buf()));
        if let Some(h) = home {
            for d in HOME_DIRS {
                add(h.join(d), format!("~/{d}"));
            }
        }
        for s in system {
            add(PathBuf::from(s), (*s).to_string());
        }
        for d in app_dirs {
            add(d.clone(), d.display().to_string());
        }
        for (p, label) in extra {
            add(p, label);
        }
        // The home folder's blocked files that link elsewhere.
        let mut linked = Vec::new();
        if let Some(h) = home {
            if let Ok(rd) = std::fs::read_dir(h) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    let blocked = home_file_blocked(&name) || env_file(&name) || CREDENTIAL_NAMES.contains(&name.to_lowercase().as_str());
                    if blocked && e.file_type().is_ok_and(|t| t.is_symlink()) {
                        if let Ok(c) = std::fs::canonicalize(e.path()) {
                            linked.push((super::files::plain_path(c), format!("~/{name}")));
                        }
                    }
                }
            }
        }
        Self { dirs, home: home_canon, linked }
    }

    /// Why no notebook may read `path` (canonical): the rule, as the
    /// message names it (`~/.ssh`, `.env`). `None`: not blocked.
    pub fn blocked(&self, path: &Path) -> Option<String> {
        let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if env_file(&name) {
            return Some(".env".into());
        }
        if CREDENTIAL_NAMES.contains(&name.to_lowercase().as_str()) {
            return Some(name);
        }
        let p = parts(path);
        if let Some((_, label)) = self.dirs.iter().find(|(d, _)| under(&p, &parts(d))) {
            return Some(label.clone());
        }
        if let Some(h) = &self.home {
            let h = parts(h);
            if p.len() == h.len() + 1 && under(&p, &h) && home_file_blocked(&name) {
                return Some(format!("~/{name}"));
            }
        }
        self.linked.iter().find(|(l, _)| parts(l) == p).map(|(_, label)| label.clone())
    }
}

// ---------------------------------------------------------------------------
// What a run may read

/// Why a run can't read its files yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gate {
    /// A file of the always-blocked list: the message names it.
    Blocked(String),
    /// Files nobody allowed yet: the UI asks.
    Ask(Vec<DataFile>),
}

/// What a run may read: the always-blocked list, the notebook's grants and
/// the files approved for this run.
#[derive(Clone, Default)]
pub struct Access<'a> {
    pub policy: Policy,
    /// The notebook file, canonical; `None` while it's unsaved (no grants,
    /// no change notice).
    pub notebook: Option<String>,
    /// This notebook's grants.
    pub grants: Vec<FileGrant>,
    /// The files approved for this run ([`take`]).
    pub approved: Vec<String>,
    /// Where the files' records are kept (the change notice); `None`: none.
    pub store: Option<&'a StateStore>,
}

impl Access<'_> {
    fn granted(&self, path: &str) -> bool {
        self.approved.iter().any(|a| a == path) || self.grants.iter().any(|g| if g.pattern { pattern_matches(&g.target, path) } else { g.target == path })
    }

    /// `files` if every one may be read; else why not (blocked first).
    pub fn gate(&self, files: &[DataFile]) -> Result<Vec<DataFile>, Gate> {
        let blocked: Vec<String> = files.iter().filter_map(|f| self.policy.blocked(Path::new(&f.path)).map(|why| format!("«{}» ({why})", f.path))).collect();
        if !blocked.is_empty() {
            return Err(Gate::Blocked(format!(
                "DBine nunca deja que un notebook lea {}: son credenciales, claves, historial o configuración del sistema",
                blocked.join(", ")
            )));
        }
        let pending: Vec<DataFile> = files.iter().filter(|f| !self.granted(&f.path)).cloned().collect();
        if pending.is_empty() {
            Ok(files.to_vec())
        } else {
            Err(Gate::Ask(pending))
        }
    }
}

/// The notebook file as grants name it: canonical, an existing file.
pub fn notebook_key(path: Option<&str>) -> Option<String> {
    let p = path.map(str::trim).filter(|p| !p.is_empty())?;
    let c = std::fs::canonicalize(p).ok().filter(|c| c.is_file())?;
    Some(super::files::plain_path(c).to_string_lossy().to_string())
}

// ---------------------------------------------------------------------------
// Asking

/// How long the UI has to answer (and to run with the answer).
const TTL: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Clone)]
struct Pending {
    tab: String,
    notebook: Option<String>,
    files: Vec<DataFile>,
    approved: bool,
    at: Instant,
}

static PENDING: LazyLock<Mutex<HashMap<String, Pending>>> = LazyLock::new(Default::default);

/// What the UI shows: "Este notebook quiere leer estos archivos".
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalRequest {
    /// For [`decide`] and the run that follows.
    pub token: String,
    pub files: Vec<DataFile>,
    /// The notebook is saved ("Permitir siempre" can be remembered).
    pub saved: bool,
}

fn pending() -> CommandResult<std::sync::MutexGuard<'static, HashMap<String, Pending>>> {
    PENDING.lock().map_err(|_| CommandError::Internal("aprobaciones de archivos".into()))
}

/// A new request for `files`; the tab's earlier ones are dropped (one
/// dialog per run).
pub fn ask(tab: &str, notebook: Option<&str>, files: Vec<DataFile>) -> CommandResult<ApprovalRequest> {
    let token = uuid::Uuid::new_v4().simple().to_string();
    let mut all = pending()?;
    all.retain(|_, p| p.at.elapsed() < TTL && p.tab != tab);
    all.insert(token.clone(), Pending { tab: tab.into(), notebook: notebook.map(Into::into), files: files.clone(), approved: false, at: Instant::now() });
    Ok(ApprovalRequest { token, files, saved: notebook.is_some() })
}

/// The user's answer in the dialog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Decision {
    /// "Permitir": this run only.
    Once,
    /// "Permitir siempre": this run, and remembered for this notebook.
    Always,
    /// "Denegar".
    Deny,
}

/// What "Permitir siempre" stores for `files`: a file found by a glob
/// whose canonical path is under that glob is granted as the pattern;
/// any other file as itself.
pub fn grant_targets(files: &[DataFile]) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    for f in files {
        let t = match &f.pattern {
            Some(p) if pattern_matches(p, &f.path) => (p.clone(), true),
            _ => (f.path.clone(), false),
        };
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Record the user's answer to request `token`. "Permitir siempre" writes
/// the grants (only for the files that request asked about, and only for a
/// saved notebook); "Denegar" drops the request.
pub fn decide(store: &StateStore, token: &str, decision: Decision) -> CommandResult<()> {
    let mut all = pending()?;
    all.retain(|_, p| p.at.elapsed() < TTL);
    let expired = || CommandError::BadRequest("la pregunta por los archivos ya no vale: ejecutá la celda de nuevo".into());
    if decision == Decision::Deny {
        all.remove(token);
        return Ok(());
    }
    let p = all.get_mut(token).ok_or_else(expired)?;
    if decision == Decision::Always {
        let Some(nb) = p.notebook.clone() else {
            return Err(CommandError::BadRequest("guardá el notebook para recordar qué archivos puede leer".into()));
        };
        store.add_file_grants(&nb, &grant_targets(&p.files))?;
    }
    p.approved = true;
    Ok(())
}

/// The files approved by `token` for this run of `tab` (once: the token
/// is spent).
pub fn take(token: &str, tab: &str, notebook: Option<&str>) -> CommandResult<Vec<String>> {
    let mut all = pending()?;
    all.retain(|_, p| p.at.elapsed() < TTL);
    let ok = all.get(token).is_some_and(|p| p.approved && p.tab == tab && p.notebook.as_deref() == notebook);
    if !ok {
        return Err(CommandError::BadRequest("la aprobación de los archivos ya no vale: ejecutá la celda de nuevo".into()));
    }
    Ok(all.remove(token).map(|p| p.files.into_iter().map(|f| f.path).collect()).unwrap_or_default())
}

// ---------------------------------------------------------------------------
// The content change notice

/// SHA-256 of a file, read in chunks.
pub fn hash_file(path: &Path) -> std::io::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// What the run found about each file: its new record (to keep once the
/// read went well) and, when its content changed since the notebook last
/// read it, the modification time it had then.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub path: String,
    pub record: FileRecord,
    pub previous_mtime_ms: Option<i64>,
    /// The record needs writing (new, or size / time / hash changed).
    pub dirty: bool,
}

/// Compare `files` with the notebook's records; a file is hashed only when
/// its size or modification time differs from its record (or it has none).
pub async fn changes(access: &Access<'_>, files: &[DataFile]) -> Vec<Seen> {
    let (Some(store), Some(nb)) = (access.store, access.notebook.as_deref()) else { return Vec::new() };
    let mut out = Vec::new();
    for f in files {
        let old = store.file_record(nb, &f.path).ok().flatten();
        if let Some(o) = old.as_ref().filter(|o| o.size == f.size && o.mtime_ms == f.mtime_ms) {
            out.push(Seen { path: f.path.clone(), record: o.clone(), previous_mtime_ms: None, dirty: false });
            continue;
        }
        let path = PathBuf::from(&f.path);
        let Ok(Ok(sha256)) = tokio::task::spawn_blocking(move || hash_file(&path)).await else { continue };
        let previous_mtime_ms = old.as_ref().filter(|o| o.sha256 != sha256).map(|o| o.mtime_ms);
        out.push(Seen { path: f.path.clone(), record: FileRecord { size: f.size, mtime_ms: f.mtime_ms, sha256 }, previous_mtime_ms, dirty: true });
    }
    out
}

/// Keep the records of a run that read its files.
pub fn remember(access: &Access<'_>, seen: &[Seen]) {
    let (Some(store), Some(nb)) = (access.store, access.notebook.as_deref()) else { return };
    for s in seen.iter().filter(|s| s.dirty) {
        if let Err(e) = store.put_file_record(nb, &s.path, &s.record) {
            tracing::warn!("notebook file record: {e}");
        }
    }
}

#[cfg(test)]
#[path = "access_tests.rs"]
mod tests;
