//! The Library in a git repository (docs/library.md): a backup that can be
//! shared, with commit, pull, push and sync from the Library's git window.
//!
//! - The repo holds each script as a file (`<folder>/<name>.<ext>`, the
//!   extension from its engines' language: `.sql`, `.js` for MongoDB,
//!   `.json`, `.cql`, `.redis`, `.flux`, `.cypher`) plus `.dbine/library.json` with what a file can't say (id,
//!   engines, description, empty folders). Other files in the repo (a README)
//!   are left alone; hand-added script files are imported as new scripts.
//! - Before every git step the Library is written into the working copy; after
//!   a pull the working copy is read back into the Library (new, changed and
//!   deleted scripts).
//! - It runs the git installed on the machine, with the user's own
//!   credentials (credential helper, SSH agent). DBine stores none, and git
//!   never prompts (`GIT_TERMINAL_PROMPT=0`): a missing credential is an error.
//! - The working copy lives in the app's data folder (`library-git/`); the
//!   remote and branch are a local setting (not synced to the cloud).

use super::git_cli::{identity, run as git};
use crate::commands::library::safe_name;
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::LibraryScript;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use tauri::{AppHandle, Manager, State};

const SETTING: &str = "local.library_git";
const MANIFEST: &str = ".dbine/library.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Config {
    remote: String,
    branch: String,
}

/// What the repo knows that a script file can't say.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    #[serde(default)]
    folders: Vec<String>,
    #[serde(default)]
    scripts: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    id: String,
    /// The file, relative to the repo, with `/`.
    path: String,
    name: String,
    #[serde(default)]
    folder: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    engines: Vec<String>,
}

// -- git -----------------------------------------------------------------------------

fn repo_dir(app: &AppHandle) -> CommandResult<PathBuf> {
    let base = app.path().app_data_dir().map_err(|e| CommandError::Internal(e.to_string()))?;
    Ok(base.join("library-git"))
}

fn config(state: &AppState) -> CommandResult<Option<Config>> {
    Ok(state.store.get_setting(SETTING)?.and_then(|v| serde_json::from_value(v).ok()).filter(|c: &Config| !c.remote.is_empty()))
}

fn configured(state: &AppState, app: &AppHandle) -> CommandResult<(Config, PathBuf)> {
    let cfg = config(state)?.ok_or_else(|| CommandError::BadRequest("la biblioteca no está vinculada a un repositorio".into()))?;
    let dir = repo_dir(app)?;
    if !dir.join(".git").is_dir() {
        return Err(CommandError::BadRequest("falta la copia local del repositorio: volvé a vincularlo".into()));
    }
    Ok((cfg, dir))
}

// -- Library <-> working copy --------------------------------------------------------

fn read_manifest(dir: &Path) -> Manifest {
    std::fs::read_to_string(dir.join(MANIFEST)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn io(e: std::io::Error) -> CommandError {
    CommandError::Internal(format!("no se pudo escribir la copia del repositorio: {e}"))
}

/// Write the Library into the working copy: one file per script, the
/// manifest, and the files of scripts that are gone deleted.
fn write_library(dir: &Path, scripts: &[LibraryScript], folders: &[String]) -> CommandResult<()> {
    let old = read_manifest(dir);
    let mut taken = HashSet::new();
    let mut entries = Vec::new();
    for s in scripts {
        let mut base: Vec<String> = s.folder.split('/').filter(|p| !p.is_empty()).map(safe_name).collect();
        let name = safe_name(&s.name);
        let ext = crate::commands::library::script_ext(&s.engines);
        let mut file = format!("{name}.{ext}");
        let mut n = 2;
        while taken.contains(&path_of(&base, &file).to_lowercase()) {
            file = format!("{name} ({n}).{ext}");
            n += 1;
        }
        base.push(file);
        let rel = base.join("/");
        taken.insert(rel.to_lowercase());
        let full = dir.join(&rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        // Only when it changed: git sees no change otherwise anyway, but the
        // file's times stay put.
        if std::fs::read_to_string(&full).ok().as_deref() != Some(s.text.as_str()) {
            std::fs::write(&full, &s.text).map_err(io)?;
        }
        entries.push(Entry {
            id: s.id.clone(),
            path: rel,
            name: s.name.clone(),
            folder: s.folder.clone(),
            description: s.description.clone(),
            engines: s.engines.clone(),
        });
    }
    // Files of scripts that were in the repo and aren't in the Library.
    for e in &old.scripts {
        if !taken.contains(&e.path.to_lowercase()) {
            let _ = std::fs::remove_file(dir.join(&e.path));
            remove_empty_parents(dir, &dir.join(&e.path));
        }
    }
    let mut all_folders: BTreeSet<String> = folders.iter().filter(|f| !f.is_empty()).cloned().collect();
    all_folders.extend(scripts.iter().map(|s| s.folder.clone()).filter(|f| !f.is_empty()));
    let manifest = Manifest { version: 1, folders: all_folders.into_iter().collect(), scripts: entries };
    std::fs::create_dir_all(dir.join(".dbine")).map_err(io)?;
    let json = serde_json::to_string_pretty(&manifest).map_err(|e| CommandError::Internal(e.to_string()))?;
    std::fs::write(dir.join(MANIFEST), json + "\n").map_err(io)
}

fn path_of(base: &[String], file: &str) -> String {
    base.iter().cloned().chain(std::iter::once(file.to_string())).collect::<Vec<_>>().join("/")
}

fn remove_empty_parents(root: &Path, file: &Path) {
    let mut p = file.parent();
    while let Some(d) = p {
        if d == root || std::fs::remove_dir(d).is_err() {
            break;
        }
        p = d.parent();
    }
}

/// Script files in the repo that the manifest doesn't know (added by hand).
fn loose_files(dir: &Path, known: &HashSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), String::new(), 0)];
    while let Some((d, rel, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
            let p = e.path();
            if p.is_dir() && depth < 12 {
                stack.push((p, r, depth + 1));
            } else if p.extension().and_then(|x| x.to_str()).is_some_and(|x| crate::commands::library::SCRIPT_EXTS.contains(&x.to_ascii_lowercase().as_str()))
                && !known.contains(&r.to_lowercase())
            {
                out.push(r);
            }
        }
    }
    out.sort();
    out
}

/// What reading the working copy into the Library did.
#[derive(Debug, Default, Serialize)]
pub struct Applied {
    pub added: usize,
    pub updated: usize,
    pub deleted: usize,
}

/// Read the working copy into the Library. `mirror`: scripts the repo doesn't
/// have are deleted (after a pull); otherwise only added and updated (when
/// linking a repo that already has scripts).
fn read_into_library(state: &AppState, dir: &Path, mirror: bool) -> CommandResult<Applied> {
    let manifest = read_manifest(dir);
    let current: HashMap<String, LibraryScript> = state.store.list_library()?.into_iter().map(|s| (s.id.clone(), s)).collect();
    let mut applied = Applied::default();
    let mut seen = HashSet::new();
    for e in &manifest.scripts {
        let Ok(bytes) = std::fs::read(dir.join(&e.path)) else { continue };
        let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
        seen.insert(e.id.clone());
        let script = LibraryScript {
            id: e.id.clone(),
            name: e.name.clone(),
            folder: e.folder.clone(),
            description: e.description.clone(),
            engines: e.engines.clone(),
            text,
            updated_at: String::new(),
        };
        match current.get(&e.id) {
            Some(c)
                if c.name == script.name
                    && c.folder == script.folder
                    && c.description == script.description
                    && c.engines == script.engines
                    && c.text == script.text => {}
            Some(_) => {
                state.store.save_library_script(&script)?;
                applied.updated += 1;
            }
            None => {
                state.store.save_library_script(&script)?;
                applied.added += 1;
            }
        }
    }
    // Hand-added files: new scripts for any SQL engine.
    let known: HashSet<String> = manifest.scripts.iter().map(|e| e.path.to_lowercase()).collect();
    for rel in loose_files(dir, &known) {
        let Ok(bytes) = std::fs::read(dir.join(&rel)) else { continue };
        let (folder, file) = rel.rsplit_once('/').unwrap_or(("", rel.as_str()));
        let name = Path::new(file).file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let id = uuid::Uuid::new_v4().to_string();
        seen.insert(id.clone());
        state.store.save_library_script(&LibraryScript {
            id,
            name,
            folder: folder.to_string(),
            description: String::new(),
            engines: crate::commands::library::engines_for_ext(Path::new(file).extension().and_then(|x| x.to_str()).unwrap_or("sql")),
            text: String::from_utf8_lossy(&bytes).replace("\r\n", "\n"),
            updated_at: String::new(),
        })?;
        applied.added += 1;
    }
    if mirror && dir.join(MANIFEST).is_file() {
        for id in current.keys().filter(|id| !seen.contains(*id)) {
            state.store.delete_library_script(id)?;
            applied.deleted += 1;
        }
    }
    // Folders (empty ones included) travel in the manifest.
    let mut folders: BTreeSet<String> = library_folders(state)?.into_iter().collect();
    folders.extend(manifest.folders.iter().cloned());
    state.store.set_setting("library.folders", Some(&serde_json::json!(folders.into_iter().collect::<Vec<_>>())))?;
    Ok(applied)
}

fn library_folders(state: &AppState) -> CommandResult<Vec<String>> {
    Ok(state.store.get_setting("library.folders")?.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default())
}

fn sync_working_copy(state: &AppState, dir: &Path) -> CommandResult<()> {
    write_library(dir, &state.store.list_library()?, &library_folders(state)?)
}

// -- status --------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct Change {
    /// `added`, `modified`, `deleted`, `renamed`, `conflict`.
    pub state: &'static str,
    pub path: String,
}

#[derive(Debug, Serialize)]
pub struct GitStatus {
    /// `git --version`, or `None` when git isn't installed.
    pub git: Option<String>,
    pub remote: Option<String>,
    pub branch: Option<String>,
    /// The working copy, to open it.
    pub dir: Option<String>,
    /// Library changes not committed yet.
    pub changes: Vec<Change>,
    /// Commits to push / to pull (after a fetch).
    pub ahead: u32,
    pub behind: u32,
    pub last_commit: Option<String>,
    /// Why the fetch failed (credentials, network), if it did.
    pub fetch_error: Option<String>,
}

async fn status_of(dir: &Path, fetch: bool) -> CommandResult<(Vec<Change>, u32, u32, Option<String>, Option<String>)> {
    git(dir, &["add", "-A"]).await?;
    let porcelain = git(dir, &["status", "--porcelain=v1", "-uall"]).await?;
    let changes = porcelain
        .lines()
        .filter(|l| l.len() > 3)
        .map(|l| {
            let (code, path) = l.split_at(3);
            let code = code.trim();
            let state = match code {
                c if c.contains('U') || c == "AA" || c == "DD" => "conflict",
                c if c.starts_with('A') || c == "??" => "added",
                c if c.starts_with('D') => "deleted",
                c if c.starts_with('R') => "renamed",
                _ => "modified",
            };
            Change { state, path: path.trim_matches('"').to_string() }
        })
        .filter(|c| c.path != MANIFEST)
        .collect();
    let fetch_error = if fetch { git(dir, &["fetch", "--quiet", "origin"]).await.err().map(|e| e.to_string()) } else { None };
    let (mut ahead, mut behind) = (0, 0);
    if let Ok(counts) = git(dir, &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"]).await {
        let mut it = counts.split_whitespace().map(|n| n.parse().unwrap_or(0));
        ahead = it.next().unwrap_or(0);
        behind = it.next().unwrap_or(0);
    }
    let last = git(dir, &["log", "-1", "--date=format:%d/%m/%Y %H:%M", "--format=%s · %an · %cd"]).await.ok().filter(|s| !s.is_empty());
    Ok((changes, ahead, behind, last, fetch_error))
}

#[derive(Deserialize, Default)]
pub struct StatusArgs {
    /// Ask the remote for new commits first.
    #[serde(default)]
    pub fetch: bool,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn library_git_status(app: AppHandle, state: State<'_, AppState>, args: Option<StatusArgs>) -> CommandResult<GitStatus> {
    let version = super::git_cli::version().await;
    let empty = |git: Option<String>| GitStatus {
        git,
        remote: None,
        branch: None,
        dir: None,
        changes: vec![],
        ahead: 0,
        behind: 0,
        last_commit: None,
        fetch_error: None,
    };
    let Some(cfg) = config(&state)? else { return Ok(empty(version)) };
    let dir = repo_dir(&app)?;
    if version.is_none() || !dir.join(".git").is_dir() {
        return Ok(GitStatus { remote: Some(cfg.remote), branch: Some(cfg.branch), ..empty(version) });
    }
    sync_working_copy(&state, &dir)?;
    let (changes, ahead, behind, last_commit, fetch_error) = status_of(&dir, args.unwrap_or_default().fetch).await?;
    Ok(GitStatus {
        git: version,
        remote: Some(cfg.remote),
        branch: Some(cfg.branch),
        dir: Some(dir.to_string_lossy().to_string()),
        changes,
        ahead,
        behind,
        last_commit,
        fetch_error,
    })
}

// -- linking -------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct LinkArgs {
    pub remote: String,
    #[serde(default)]
    pub branch: String,
}

#[derive(Serialize)]
pub struct LinkOut {
    /// Scripts that came from the repo.
    pub applied: Applied,
    /// The Library's scripts were pushed to the repo.
    pub pushed: bool,
}

/// Link the Library to a repo: clone it, bring in the scripts it has (none of
/// the Library's are deleted), and push the Library's there.
#[tauri::command(rename_all = "camelCase")]
pub async fn library_git_link(app: AppHandle, state: State<'_, AppState>, args: LinkArgs) -> CommandResult<LinkOut> {
    link(&state, &repo_dir(&app)?, args).await
}

async fn link(state: &AppState, dir: &Path, args: LinkArgs) -> CommandResult<LinkOut> {
    let remote = args.remote.trim().to_string();
    if remote.is_empty() {
        return Err(CommandError::BadRequest("falta la URL del repositorio".into()));
    }
    let branch = if args.branch.trim().is_empty() { "main".to_string() } else { args.branch.trim().to_string() };
    let dir = dir.to_path_buf();
    if dir.exists() {
        // A previous link: start over from the remote.
        std::fs::remove_dir_all(&dir).map_err(io)?;
    }
    let parent = dir.parent().map(Path::to_path_buf).unwrap_or_else(std::env::temp_dir);
    std::fs::create_dir_all(&parent).map_err(io)?;
    let target = dir.to_string_lossy().to_string();
    git(&parent, &["clone", "--quiet", &remote, &target]).await?;
    // The branch: the remote's if it has it, else a new one.
    let has_branch = git(&dir, &["ls-remote", "--exit-code", "--heads", "origin", &branch]).await.is_ok();
    if has_branch {
        git(&dir, &["checkout", "--quiet", "-B", &branch, &format!("origin/{branch}")]).await?;
    } else if git(&dir, &["rev-parse", "--verify", "HEAD"]).await.is_ok() {
        git(&dir, &["checkout", "--quiet", "-B", &branch]).await?;
    } else {
        // An empty repo: the first commit starts the branch.
        git(&dir, &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")]).await?;
    }
    state.store.set_setting(SETTING, Some(&serde_json::json!(Config { remote: remote.clone(), branch: branch.clone() })))?;
    let applied = read_into_library(state, &dir, false)?;
    sync_working_copy(state, &dir)?;
    git(&dir, &["add", "-A"]).await?;
    let mut pushed = false;
    if !git(&dir, &["status", "--porcelain"]).await?.is_empty() {
        commit(&dir, "Biblioteca de DBine").await?;
    }
    if git(&dir, &["rev-parse", "--verify", "HEAD"]).await.is_ok() {
        git(&dir, &["push", "--quiet", "-u", "origin", &branch]).await?;
        pushed = true;
    }
    Ok(LinkOut { applied, pushed })
}

#[tauri::command]
pub async fn library_git_unlink(app: AppHandle, state: State<'_, AppState>) -> CommandResult<()> {
    state.store.set_setting(SETTING, None)?;
    let dir = repo_dir(&app)?;
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(io)?;
    }
    Ok(())
}

// -- commit, pull, push, sync --------------------------------------------------------

async fn commit(dir: &Path, message: &str) -> CommandResult<()> {
    let mut args = identity(dir).await;
    args.extend(["commit".into(), "--quiet".into(), "-m".into(), message.to_string()]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    git(dir, &refs).await.map(|_| ())
}

#[derive(Deserialize)]
pub struct CommitArgs {
    pub message: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn library_git_commit(app: AppHandle, state: State<'_, AppState>, args: CommitArgs) -> CommandResult<()> {
    let (_, dir) = configured(&state, &app)?;
    sync_working_copy(&state, &dir)?;
    git(&dir, &["add", "-A"]).await?;
    if git(&dir, &["status", "--porcelain"]).await?.is_empty() {
        return Err(CommandError::BadRequest("no hay cambios para confirmar".into()));
    }
    let message = if args.message.trim().is_empty() { "Cambios en la biblioteca" } else { args.message.trim() };
    commit(&dir, message).await
}

/// A pull that stopped on a conflict: the scripts involved.
#[derive(Serialize)]
pub struct PullOut {
    pub applied: Applied,
    /// Files changed on both sides; empty when it went through.
    pub conflicts: Vec<String>,
}

async fn pull(state: &AppState, dir: &Path, branch: &str) -> CommandResult<PullOut> {
    // Local changes go in a commit first, so the rebase can replay them.
    sync_working_copy(state, dir)?;
    git(dir, &["add", "-A"]).await?;
    if !git(dir, &["status", "--porcelain"]).await?.is_empty() {
        commit(dir, "Cambios en la biblioteca").await?;
    }
    let mut args = identity(dir).await;
    args.extend(["pull".into(), "--quiet".into(), "--rebase".into(), "origin".into(), branch.to_string()]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    if let Err(e) = git(dir, &refs).await {
        let conflicts: Vec<String> = git(dir, &["diff", "--name-only", "--diff-filter=U"])
            .await
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        let _ = git(dir, &["rebase", "--abort"]).await;
        if conflicts.is_empty() {
            return Err(e);
        }
        return Ok(PullOut { applied: Applied::default(), conflicts });
    }
    let applied = read_into_library(state, dir, true)?;
    Ok(PullOut { applied, conflicts: vec![] })
}

#[tauri::command]
pub async fn library_git_pull(app: AppHandle, state: State<'_, AppState>) -> CommandResult<PullOut> {
    let (cfg, dir) = configured(&state, &app)?;
    pull(&state, &dir, &cfg.branch).await
}

#[tauri::command]
pub async fn library_git_push(app: AppHandle, state: State<'_, AppState>) -> CommandResult<()> {
    let (cfg, dir) = configured(&state, &app)?;
    git(&dir, &["push", "--quiet", "-u", "origin", &cfg.branch]).await.map(|_| ())
}

#[derive(Deserialize)]
pub struct SyncArgs {
    #[serde(default)]
    pub message: String,
}

/// Commit what changed, pull, and push: the Library and the repo end equal.
#[tauri::command(rename_all = "camelCase")]
pub async fn library_git_sync(app: AppHandle, state: State<'_, AppState>, args: SyncArgs) -> CommandResult<PullOut> {
    let (cfg, dir) = configured(&state, &app)?;
    sync(&state, &dir, &cfg.branch, &args.message).await
}

async fn sync(state: &AppState, dir: &Path, branch: &str, message: &str) -> CommandResult<PullOut> {
    let (dir, message) = (dir.to_path_buf(), message.to_string());
    sync_working_copy(state, &dir)?;
    git(&dir, &["add", "-A"]).await?;
    if !git(&dir, &["status", "--porcelain"]).await?.is_empty() {
        let message = if message.trim().is_empty() { "Cambios en la biblioteca" } else { message.trim() };
        commit(&dir, message).await?;
    }
    let remote_has_branch = git(&dir, &["ls-remote", "--exit-code", "--heads", "origin", branch]).await.is_ok();
    let out = if remote_has_branch { pull(state, &dir, branch).await? } else { PullOut { applied: Applied::default(), conflicts: vec![] } };
    if out.conflicts.is_empty() {
        git(&dir, &["push", "--quiet", "-u", "origin", branch]).await?;
    }
    Ok(out)
}

#[derive(Deserialize)]
pub struct ResolveArgs {
    /// `remote`: take the repo's version (the Library's pending changes are
    /// dropped); `local`: overwrite the repo with the Library's.
    pub keep: String,
}

/// Settle a conflicting pull by keeping one side whole.
#[tauri::command(rename_all = "camelCase")]
pub async fn library_git_resolve(app: AppHandle, state: State<'_, AppState>, args: ResolveArgs) -> CommandResult<Applied> {
    let (cfg, dir) = configured(&state, &app)?;
    resolve(&state, &dir, &cfg.branch, &args.keep).await
}

async fn resolve(state: &AppState, dir: &Path, branch: &str, keep: &str) -> CommandResult<Applied> {
    let dir = dir.to_path_buf();
    match keep {
        "remote" => {
            git(&dir, &["fetch", "--quiet", "origin"]).await?;
            // The working copy is DBine's own clone of the Library, never
            // the user's files.
            git(&dir, &["reset", "--quiet", "--hard", &format!("origin/{branch}")]).await?;
            git(&dir, &["clean", "-fdq"]).await?;
            read_into_library(state, &dir, true)
        }
        "local" => {
            sync_working_copy(state, &dir)?;
            git(&dir, &["add", "-A"]).await?;
            if !git(&dir, &["status", "--porcelain"]).await?.is_empty() {
                commit(&dir, "Cambios en la biblioteca").await?;
            }
            git(&dir, &["push", "--quiet", "--force-with-lease", "-u", "origin", branch]).await?;
            Ok(Applied::default())
        }
        _ => Err(CommandError::BadRequest("elegí qué versión conservar".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::git_cli::git_path;

    fn script(id: &str, folder: &str, name: &str, engines: &[&str], text: &str) -> LibraryScript {
        LibraryScript {
            id: id.into(),
            name: name.into(),
            folder: folder.into(),
            description: String::new(),
            engines: engines.iter().map(|e| e.to_string()).collect(),
            text: text.into(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn working_copy_follows_the_library() {
        let d = std::env::temp_dir().join(format!("dbine-libgit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("README.md"), "mine").unwrap();
        let a = script("1", "Mantenimiento/Índices", "Reindexar", &["sqlserver"], "ALTER INDEX ALL ON t REBUILD;");
        let b = script("2", "", "Colecciones", &["mongodb"], "db.getCollectionNames()");
        let c = script("3", "Mantenimiento/Índices", "Reindexar", &["postgres"], "REINDEX TABLE t;");
        write_library(&d, &[a.clone(), b.clone(), c], &["Vacía".into()]).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("Mantenimiento/Índices/Reindexar.sql")).unwrap(), a.text);
        assert!(d.join("Mantenimiento/Índices/Reindexar (2).sql").is_file(), "same name, second file");
        assert!(d.join("Colecciones.js").is_file());
        let m = read_manifest(&d);
        assert_eq!(m.scripts.len(), 3);
        assert!(m.folders.contains(&"Vacía".to_string()));
        // A script leaves the Library: its file goes, the README stays.
        write_library(&d, &[a], &[]).unwrap();
        assert!(!d.join("Colecciones.js").exists());
        assert!(!d.join("Mantenimiento/Índices/Reindexar (2).sql").exists());
        assert!(d.join("README.md").is_file());
        // Hand-added files are found; hidden folders are not.
        std::fs::write(d.join("nuevo.sql"), "SELECT 1").unwrap();
        let known: HashSet<String> = read_manifest(&d).scripts.iter().map(|e| e.path.to_lowercase()).collect();
        assert_eq!(loose_files(&d, &known), vec!["nuevo.sql".to_string()]);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Two machines, one repo: link, sync edits and deletions both ways, and
    /// settle a conflict. Needs git (skipped without it).
    #[tokio::test]
    async fn two_libraries_share_a_repo() {
        if git_path().is_err() {
            eprintln!("git not installed; skipping");
            return;
        }
        let root = std::env::temp_dir().join(format!("dbine-libgit-{}", uuid::Uuid::new_v4()));
        let remote = root.join("remote.git");
        std::fs::create_dir_all(&remote).unwrap();
        git(&remote, &["init", "--quiet", "--bare"]).await.unwrap();
        let url = remote.to_string_lossy().to_string();
        let machine = || AppState::new(dbine_core::StateStore::open_in_memory().unwrap());
        let (a, b) = (machine(), machine());
        let (da, db) = (root.join("a"), root.join("b"));
        let names = |s: &AppState| {
            let mut v: Vec<(String, String)> = s.store.list_library().unwrap().into_iter().map(|s| (s.name, s.text)).collect();
            v.sort();
            v
        };

        // A has two scripts and links the empty repo: they're pushed.
        a.store.save_library_script(&script("s1", "Índices", "Reindexar", &["sqlserver"], "REBUILD 1")).unwrap();
        a.store.save_library_script(&script("s2", "", "Colecciones", &["mongodb"], "db.x.find()")).unwrap();
        let out = link(&a, &da, LinkArgs { remote: url.clone(), branch: "main".into() }).await.unwrap();
        assert!(out.pushed);

        // B links it: gets both, engines included.
        link(&b, &db, LinkArgs { remote: url.clone(), branch: "main".into() }).await.unwrap();
        assert_eq!(names(&b), names(&a));
        assert_eq!(b.store.list_library().unwrap().iter().find(|s| s.id == "s2").unwrap().engines, vec!["mongodb".to_string()]);

        // B edits one and deletes the other; A gets both changes.
        b.store.save_library_script(&script("s1", "Índices", "Reindexar", &["sqlserver"], "REBUILD 2")).unwrap();
        b.store.delete_library_script("s2").unwrap();
        sync(&b, &db, "main", "editar").await.unwrap();
        let got = sync(&a, &da, "main", "").await.unwrap();
        assert!(got.conflicts.is_empty());
        assert_eq!((got.applied.updated, got.applied.deleted), (1, 1));
        assert_eq!(names(&a), vec![("Reindexar".to_string(), "REBUILD 2".to_string())]);

        // Both edit the same script: A's sync stops on the conflict;
        // keeping the repo's version leaves A like B.
        b.store.save_library_script(&script("s1", "Índices", "Reindexar", &["sqlserver"], "REBUILD B")).unwrap();
        sync(&b, &db, "main", "").await.unwrap();
        a.store.save_library_script(&script("s1", "Índices", "Reindexar", &["sqlserver"], "REBUILD A")).unwrap();
        let clash = sync(&a, &da, "main", "").await.unwrap();
        assert_eq!(clash.conflicts, vec!["Índices/Reindexar.sql".to_string()]);
        resolve(&a, &da, "main", "remote").await.unwrap();
        assert_eq!(names(&a), vec![("Reindexar".to_string(), "REBUILD B".to_string())]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
