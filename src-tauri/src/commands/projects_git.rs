//! Git for projects (docs/projects.md): status, diff, commit, pull, push,
//! sync, conflicts and cancel, on the user's own working copy.
//!
//! Unlike the Library's git, these are the user's files: a pull that stops on
//! a conflict leaves the repo mid-merge (or mid-rebase) for the user to
//! settle, and commits go out under the user's own name and email.
//!
//! One mutating or network operation at a time per project (`Busy`);
//! `status` and `diff` don't wait for it (`--no-optional-locks`). Network
//! phases can be cancelled through their `op_id` (`project_git_cancel`).

use super::git_cli::{self, RunOpts, NET_LIMIT};
use super::project_paths::{canonical_root, rel_ok, resolve_entry, resolve_existing};
use super::projects::{decode_text, files_changed, get, progress_sink, remove_entry, root_of, ProjectEvents};
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dashmap::DashMap;
use dbine_core::StateStore;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use tauri::{AppHandle, State};
use tokio::sync::Notify;

const MAX_CHANGES: usize = 5000;
const DIFF_MAX: usize = 2 * 1024 * 1024;

// -- operations: cancel and the per-project lock ----------------------------------------

/// Cancellable phases running now: op id → its cancel.
static OPS: LazyLock<DashMap<String, Arc<Notify>>> = LazyLock::new(DashMap::new);
/// Projects with a git operation running: project id → its name.
static BUSY: LazyLock<DashMap<String, &'static str>> = LazyLock::new(DashMap::new);

/// The project's lock, released on drop.
pub struct Busy(String);

impl Busy {
    pub fn take(project_id: &str, op: &'static str) -> CommandResult<Busy> {
        match BUSY.entry(project_id.to_string()) {
            dashmap::Entry::Occupied(e) => {
                Err(CommandError::BadRequest(format!("ya hay una operación de git en curso en este proyecto ({})", e.get())))
            }
            dashmap::Entry::Vacant(v) => {
                v.insert(op);
                Ok(Busy(project_id.to_string()))
            }
        }
    }
}

impl Drop for Busy {
    fn drop(&mut self) {
        BUSY.remove(&self.0);
    }
}

/// A registered cancellable phase; it stops being cancellable on `done` or
/// drop.
pub struct Op {
    id: Option<String>,
    cancel: Option<Arc<Notify>>,
}

impl Op {
    pub fn register(op_id: Option<&str>) -> Op {
        match op_id.filter(|s| !s.is_empty()) {
            Some(id) => {
                let n = Arc::new(Notify::new());
                OPS.insert(id.to_string(), n.clone());
                Op { id: Some(id.to_string()), cancel: Some(n) }
            }
            None => Op { id: None, cancel: None },
        }
    }

    pub fn cancel(&self) -> Option<Arc<Notify>> {
        self.cancel.clone()
    }

    /// The cancellable phase is over.
    pub fn done(&mut self) {
        if let Some(id) = self.id.take() {
            OPS.remove(&id);
        }
    }
}

impl Drop for Op {
    fn drop(&mut self) {
        self.done();
    }
}

/// Cancel a running network phase. False when there's none under that id
/// (it ended, or it's past the point where it can stop).
pub fn cancel_op(op_id: &str) -> bool {
    match OPS.get(op_id) {
        Some(n) => {
            n.notify_one();
            true
        }
        None => false,
    }
}

#[derive(Debug, Deserialize)]
pub struct CancelArgs {
    pub op_id: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_git_cancel(args: CancelArgs) -> CommandResult<bool> {
    Ok(cancel_op(&args.op_id))
}

// -- status -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FileChange {
    pub path: String,
    pub orig_path: Option<String>,
    /// Porcelain X and Y (`?` for untracked).
    pub index: String,
    pub worktree: String,
    /// `M`, `A`, `U` (untracked), `D`, `R`, `C` (conflict).
    pub mark: char,
}

/// `git status --porcelain=v2 --branch -z`, parsed.
#[derive(Debug, Default, PartialEq)]
pub struct Porcelain {
    pub head: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub changes: Vec<FileChange>,
    pub truncated: bool,
}

pub fn parse_porcelain(raw: &[u8]) -> Porcelain {
    let mut out = Porcelain::default();
    let tokens: Vec<String> = raw.split(|b| *b == 0).map(|t| String::from_utf8_lossy(t).to_string()).collect();
    let mut it = tokens.into_iter();
    let push = |out: &mut Porcelain, c: FileChange| {
        if out.changes.len() < MAX_CHANGES {
            out.changes.push(c);
        } else {
            out.truncated = true;
        }
    };
    while let Some(t) = it.next() {
        if let Some(h) = t.strip_prefix("# ") {
            let (key, val) = h.split_once(' ').unwrap_or((h, ""));
            match key {
                "branch.oid" if val != "(initial)" => out.head = Some(val.chars().take(7).collect()),
                "branch.head" if val == "(detached)" => out.detached = true,
                "branch.head" => out.branch = Some(val.to_string()),
                "branch.upstream" => out.upstream = Some(val.to_string()),
                "branch.ab" => {
                    for n in val.split_whitespace() {
                        if let Some(a) = n.strip_prefix('+') {
                            out.ahead = a.parse().unwrap_or(0);
                        } else if let Some(b) = n.strip_prefix('-') {
                            out.behind = b.parse().unwrap_or(0);
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        let xy = |s: &str| {
            let mut c = s.chars();
            (c.next().unwrap_or('.').to_string(), c.next().unwrap_or('.').to_string())
        };
        match t.chars().next() {
            Some('1') => {
                let f: Vec<&str> = t.splitn(9, ' ').collect();
                if f.len() < 9 {
                    continue;
                }
                let (x, y) = xy(f[1]);
                let mark = if x == "A" || y == "A" {
                    'A'
                } else if x == "D" || y == "D" {
                    'D'
                } else {
                    'M'
                };
                push(&mut out, FileChange { path: f[8].to_string(), orig_path: None, index: x, worktree: y, mark });
            }
            Some('2') => {
                let f: Vec<&str> = t.splitn(10, ' ').collect();
                let orig = it.next();
                if f.len() < 10 {
                    continue;
                }
                let (x, y) = xy(f[1]);
                push(&mut out, FileChange { path: f[9].to_string(), orig_path: orig, index: x, worktree: y, mark: 'R' });
            }
            Some('u') => {
                let f: Vec<&str> = t.splitn(11, ' ').collect();
                if f.len() < 11 {
                    continue;
                }
                let (x, y) = xy(f[1]);
                push(&mut out, FileChange { path: f[10].to_string(), orig_path: None, index: x, worktree: y, mark: 'C' });
            }
            Some('?') if t.len() > 2 => {
                push(&mut out, FileChange { path: t[2..].to_string(), orig_path: None, index: "?".into(), worktree: "?".into(), mark: 'U' });
            }
            _ => {}
        }
    }
    out
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ProjectStatus {
    /// Git is installed.
    pub git: bool,
    pub exists: bool,
    pub is_repo: bool,
    pub branch: Option<String>,
    pub detached: bool,
    /// Short oid; `None` before the first commit.
    pub head: Option<String>,
    pub upstream: Option<String>,
    pub remote: Option<String>,
    pub has_remote: bool,
    pub ahead: u32,
    pub behind: u32,
    pub changes: Vec<FileChange>,
    pub truncated: bool,
    /// `merge`, `rebase`, `cherry-pick` or `revert` in progress.
    pub operation: Option<String>,
    pub last_commit: Option<String>,
    pub fetch_error: Option<String>,
    pub identity_missing: bool,
}

async fn porcelain(root: &Path) -> CommandResult<Porcelain> {
    let raw = git_cli::run_with(
        root,
        &["--no-optional-locks", "status", "--porcelain=v2", "--branch", "-z", "--untracked-files=all"],
        RunOpts { raw: true, ..Default::default() },
    )
    .await?;
    Ok(parse_porcelain(&raw))
}

/// The merge, rebase, cherry-pick or revert in progress (worktrees too).
async fn operation(root: &Path) -> Option<String> {
    const MARKERS: [(&str, &str); 5] =
        [("MERGE_HEAD", "merge"), ("rebase-merge", "rebase"), ("rebase-apply", "rebase"), ("CHERRY_PICK_HEAD", "cherry-pick"), ("REVERT_HEAD", "revert")];
    let mut args = vec!["rev-parse"];
    for (m, _) in MARKERS {
        args.extend(["--git-path", m]);
    }
    let out = git_cli::run(root, &args).await.ok()?;
    out.lines().zip(MARKERS).find(|(p, _)| root.join(p).exists()).map(|(_, (_, op))| op.to_string())
}

/// The remote pull and push use: the branch's, else `origin`, else the only one.
async fn remote_of(root: &Path, branch: Option<&str>) -> (Option<String>, bool) {
    let remotes: Vec<String> = git_cli::run(root, &["remote"]).await.unwrap_or_default().lines().map(str::to_string).collect();
    if remotes.is_empty() {
        return (None, false);
    }
    if let Some(b) = branch {
        if let Ok(r) = git_cli::run(root, &["config", &format!("branch.{b}.remote")]).await {
            if !r.is_empty() {
                return (Some(r), true);
            }
        }
    }
    let r = if remotes.iter().any(|r| r == "origin") {
        Some("origin".to_string())
    } else if remotes.len() == 1 {
        remotes.first().cloned()
    } else {
        None
    };
    (r, true)
}

async fn last_commit(root: &Path) -> Option<String> {
    git_cli::run(root, &["log", "-1", "--date=format:%d/%m/%Y %H:%M", "--format=%s · %an · %cd"]).await.ok().filter(|s| !s.is_empty())
}

#[derive(Debug, Deserialize, Default)]
pub struct StatusArgs {
    pub id: String,
    /// Ask the remote for new commits first.
    #[serde(default)]
    pub fetch: bool,
    #[serde(default)]
    pub op_id: Option<String>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_status(app: AppHandle, state: State<'_, AppState>, args: StatusArgs) -> CommandResult<ProjectStatus> {
    status(&state.store, &app, args).await
}

pub async fn status(store: &StateStore, ev: &dyn ProjectEvents, args: StatusArgs) -> CommandResult<ProjectStatus> {
    let p = get(store, &args.id)?;
    let mut st = ProjectStatus { git: git_cli::git_path().is_ok(), ..Default::default() };
    let Ok(root) = canonical_root(Path::new(&p.path)) else { return Ok(st) };
    st.exists = true;
    st.is_repo = root.join(".git").exists();
    if !st.git || !st.is_repo {
        return Ok(st);
    }
    if args.fetch {
        // A fetch while another operation runs is skipped, not an error: the
        // periodic refresh shouldn't fail because a push is under way.
        if let Ok(_busy) = Busy::take(&p.id, "Fetch") {
            let pre = porcelain(&root).await?;
            if let (Some(remote), _) = remote_of(&root, pre.branch.as_deref()).await {
                let op = Op::register(args.op_id.as_deref());
                let sink = progress_sink(ev, args.op_id.as_deref().unwrap_or(""));
                let progress: Option<&(dyn Fn(&str) + Send + Sync)> = if args.op_id.is_some() { Some(&sink) } else { None };
                let res = git_cli::run_with(
                    &root,
                    &["fetch", "--quiet", "--prune", &remote],
                    RunOpts { limit: NET_LIMIT, cancel: op.cancel(), progress, ..Default::default() },
                )
                .await;
                st.fetch_error = res.err().map(|e| git_cli::remap(e).to_string());
            }
        }
    }
    let pc = porcelain(&root).await?;
    let (remote, has_remote) = remote_of(&root, pc.branch.as_deref()).await;
    st.branch = pc.branch;
    st.detached = pc.detached;
    st.head = pc.head;
    st.upstream = pc.upstream;
    st.ahead = pc.ahead;
    st.behind = pc.behind;
    st.changes = pc.changes;
    st.truncated = pc.truncated;
    st.remote = remote;
    st.has_remote = has_remote;
    st.operation = operation(&root).await;
    st.last_commit = last_commit(&root).await;
    st.identity_missing = !git_cli::has_identity(&root).await;
    Ok(st)
}

/// The project's root, for a git command (git must be there, and a repo).
async fn repo_root(store: &StateStore, id: &str) -> CommandResult<(String, PathBuf)> {
    git_cli::git_path()?;
    let (p, root) = root_of(store, id)?;
    if !root.join(".git").exists() {
        return Err(CommandError::BadRequest("la carpeta no es un repositorio git".into()));
    }
    Ok((p.id, root))
}

// -- diff -------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FileDiff {
    pub path: String,
    pub orig_path: Option<String>,
    pub mark: char,
    /// `None`: no such side (added, deleted, no commits yet), or binary or too large.
    pub before: Option<String>,
    pub after: Option<String>,
    pub binary: bool,
    pub too_large: bool,
}

#[derive(Debug, Deserialize)]
pub struct DiffArgs {
    pub id: String,
    pub path: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_diff(state: State<'_, AppState>, args: DiffArgs) -> CommandResult<FileDiff> {
    diff(&state.store, args).await
}

async fn show(root: &Path, spec: &str) -> Option<Vec<u8>> {
    git_cli::run_with(root, &["--no-optional-locks", "show", spec], RunOpts { raw: true, ..Default::default() }).await.ok()
}

pub async fn diff(store: &StateStore, args: DiffArgs) -> CommandResult<FileDiff> {
    let (_, root) = repo_root(store, &args.id).await?;
    rel_ok(&args.path)?;
    let pc = porcelain(&root).await?;
    let change = pc.changes.into_iter().find(|c| c.path == args.path);
    let (mark, orig) = change.as_ref().map(|c| (c.mark, c.orig_path.clone())).unwrap_or(('M', None));
    let working = || -> Option<Vec<u8>> { resolve_existing(&root, &args.path).ok().and_then(|p| std::fs::read(p).ok()) };
    let (before, after) = match mark {
        'A' | 'U' => (None, working()),
        'D' => (show(&root, &format!("HEAD:{}", args.path)).await, None),
        'R' => (show(&root, &format!("HEAD:{}", orig.clone().unwrap_or_default())).await, working()),
        'C' => (show(&root, &format!(":2:{}", args.path)).await, show(&root, &format!(":3:{}", args.path)).await),
        _ => (show(&root, &format!("HEAD:{}", args.path)).await, working()),
    };
    let too_large = before.as_ref().is_some_and(|b| b.len() > DIFF_MAX) || after.as_ref().is_some_and(|b| b.len() > DIFF_MAX);
    let text = |b: Option<Vec<u8>>| b.map(|b| decode_text(&b).map(|(t, _, _)| t));
    let (before, after) = if too_large { (None, None) } else { (text(before), text(after)) };
    // Some(None): a side that's there but isn't text.
    let binary = !too_large && (matches!(before, Some(None)) || matches!(after, Some(None)));
    let flat = |s: Option<Option<String>>| if binary { None } else { s.flatten() };
    Ok(FileDiff { path: args.path, orig_path: orig, mark, before: flat(before), after: flat(after), binary, too_large })
}

// -- a file's commits (its tab's timeline, docs/history.md) ----------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FileCommit {
    pub hash: String,
    pub short: String,
    pub author: String,
    /// The author's date, ISO 8601.
    pub date: String,
    pub subject: String,
    /// The file's path in that commit (a rename may have changed it since).
    pub path: String,
}

#[derive(Debug, Deserialize)]
pub struct FileLogArgs {
    pub id: String,
    pub path: String,
    pub limit: Option<u32>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_file_log(state: State<'_, AppState>, args: FileLogArgs) -> CommandResult<Vec<FileCommit>> {
    file_log(&state.store, args).await
}

/// The commits that touched a file, newest first, following renames. Empty
/// when there's no git, the folder isn't a repo or nothing is committed yet.
pub async fn file_log(store: &StateStore, args: FileLogArgs) -> CommandResult<Vec<FileCommit>> {
    rel_ok(&args.path)?;
    let (_, root) = root_of(store, &args.id)?;
    if git_cli::git_path().is_err() || !root.join(".git").exists() {
        return Ok(Vec::new());
    }
    let n = format!("-n{}", args.limit.unwrap_or(100).clamp(1, 1000));
    let out = git_cli::output(
        &root,
        &[
            "--no-optional-locks",
            "--literal-pathspecs",
            "log",
            "--follow",
            &n,
            "--name-only",
            "--format=%x1e%H%x1f%h%x1f%an%x1f%aI%x1f%s",
            "--",
            &args.path,
        ],
        RunOpts::default(),
    )
    .await?;
    // No commits yet (or an unborn branch): nothing to show.
    if !out.ok() {
        return Ok(Vec::new());
    }
    Ok(parse_file_log(&String::from_utf8_lossy(&out.stdout), &args.path))
}

/// `git log --name-only` with the format above: one record per commit, the
/// file's name in it after the header line.
fn parse_file_log(raw: &str, path: &str) -> Vec<FileCommit> {
    raw.split('\u{1e}')
        .filter_map(|rec| {
            let mut lines = rec.lines();
            let head: Vec<&str> = lines.next()?.splitn(5, '\u{1f}').collect();
            let [hash, short, author, date, subject] = head[..] else { return None };
            let name = lines.map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or(path);
            Some(FileCommit {
                hash: hash.to_string(),
                short: short.to_string(),
                author: author.to_string(),
                date: date.to_string(),
                subject: subject.to_string(),
                path: name.to_string(),
            })
        })
        .collect()
}

#[derive(Debug, Deserialize)]
pub struct FileAtArgs {
    pub id: String,
    /// A full or abbreviated commit hash.
    pub commit: String,
    /// The file's path in that commit ([`FileCommit::path`]).
    pub path: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FileAtCommit {
    /// `None`: not in that commit, binary or too large.
    pub text: Option<String>,
    pub binary: bool,
    pub too_large: bool,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_file_at(state: State<'_, AppState>, args: FileAtArgs) -> CommandResult<FileAtCommit> {
    file_at(&state.store, args).await
}

/// A file as it was in a commit.
pub async fn file_at(store: &StateStore, args: FileAtArgs) -> CommandResult<FileAtCommit> {
    rel_ok(&args.path)?;
    // Only a hash: nothing git could read as an option or a revision expression.
    if !(4..=64).contains(&args.commit.len()) || !args.commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(CommandError::BadRequest("el commit no es válido".into()));
    }
    let (_, root) = repo_root(store, &args.id).await?;
    let Some(bytes) = show(&root, &format!("{}:{}", args.commit, args.path.replace('\\', "/"))).await else {
        return Ok(FileAtCommit { text: None, binary: false, too_large: false });
    };
    if bytes.len() > DIFF_MAX {
        return Ok(FileAtCommit { text: None, binary: false, too_large: true });
    }
    Ok(match decode_text(&bytes) {
        Some((text, _, _)) => FileAtCommit { text: Some(text), binary: false, too_large: false },
        None => FileAtCommit { text: None, binary: true, too_large: false },
    })
}

// -- commit -----------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CommitArgs {
    pub id: String,
    pub message: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_commit(state: State<'_, AppState>, args: CommitArgs) -> CommandResult<String> {
    commit(&state.store, args).await
}

pub async fn commit(store: &StateStore, args: CommitArgs) -> CommandResult<String> {
    let (id, root) = repo_root(store, &args.id).await?;
    let message = args.message.trim();
    if message.is_empty() {
        return Err(CommandError::BadRequest("escribí un mensaje para el commit".into()));
    }
    let _busy = Busy::take(&id, "Confirmar")?;
    let op = operation(&root).await;
    if op.as_deref() == Some("rebase") {
        return Err(CommandError::BadRequest("hay un rebase en curso: continualo o abortalo".into()));
    }
    let pc = porcelain(&root).await?;
    if pc.detached {
        return Err(CommandError::BadRequest("HEAD desacoplado: cambiá a una rama antes de confirmar".into()));
    }
    if pc.changes.iter().any(|c| c.mark == 'C') {
        return Err(CommandError::BadRequest("hay conflictos sin resolver: resolvelos antes de confirmar".into()));
    }
    if !git_cli::has_identity(&root).await {
        return Err(CommandError::BadRequest("git no tiene tu nombre y email configurados".into()));
    }
    git_cli::run(&root, &["add", "-A"]).await?;
    let staged = git_cli::output(&root, &["diff", "--cached", "--quiet"], RunOpts::default()).await?;
    // A merge commit concludes the merge even when it brings nothing new.
    if staged.ok() && op.as_deref() != Some("merge") {
        return Err(CommandError::BadRequest("no hay cambios para confirmar".into()));
    }
    git_cli::run(&root, &["commit", "-q", "-m", message]).await.map_err(git_cli::remap)?;
    git_cli::run(&root, &["rev-parse", "--short", "HEAD"]).await
}

// -- pull, push, sync -------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ProjectPullOut {
    pub up_to_date: bool,
    /// Files the pull changed.
    pub updated: Vec<String>,
    /// Files left in conflict (the repo stays in `operation`).
    pub conflicts: Vec<String>,
    pub operation: Option<String>,
    /// Something to tell the user (the branch isn't on the remote yet).
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct ProjectSyncOut {
    #[serde(flatten)]
    pub pull: ProjectPullOut,
    pub pushed: bool,
}

#[derive(Debug, Deserialize)]
pub struct OpArgs {
    pub id: String,
    #[serde(default)]
    pub op_id: String,
}

fn nul_list(b: &[u8]) -> Vec<String> {
    b.split(|x| *x == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).to_string()).collect()
}

async fn conflicts(root: &Path) -> Vec<String> {
    git_cli::run_with(root, &["diff", "--name-only", "--diff-filter=U", "-z"], RunOpts { raw: true, ..Default::default() })
        .await
        .map(|b| nul_list(&b))
        .unwrap_or_default()
}

/// Branch and remote for pull and push, with the user's message when missing.
async fn branch_and_remote(root: &Path) -> CommandResult<(String, String)> {
    let pc = porcelain(root).await?;
    let Some(branch) = pc.branch.filter(|_| !pc.detached) else {
        return Err(CommandError::BadRequest("HEAD desacoplado: Pull/Push necesitan una rama".into()));
    };
    match remote_of(root, Some(&branch)).await {
        (Some(r), _) => Ok((branch, r)),
        _ => Err(CommandError::BadRequest("el repositorio no tiene remoto: agregá uno (Agregar remoto…)".into())),
    }
}

async fn has_upstream(root: &Path) -> bool {
    git_cli::run(root, &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"]).await.is_ok()
}

async fn rebase_mode(root: &Path, branch: &str) -> bool {
    let get = |k: String| async move { git_cli::run(root, &["config", "--get", &k]).await.ok().filter(|v| !v.is_empty()) };
    let v = match get(format!("branch.{branch}.rebase")).await {
        Some(v) => Some(v),
        None => get("pull.rebase".into()).await,
    };
    matches!(v.as_deref().map(str::to_ascii_lowercase).as_deref(), Some("true" | "merges" | "interactive" | "i" | "m"))
}

async fn pull_inner(root: &Path, ev: &dyn ProjectEvents, op_id: &str) -> CommandResult<ProjectPullOut> {
    let (branch, remote) = branch_and_remote(root).await?;
    // Phase 1, cancellable: fetch.
    let mut op = Op::register(Some(op_id));
    let sink = progress_sink(ev, op_id);
    git_cli::run_with(root, &["fetch", "--prune", "--progress", &remote], RunOpts { limit: NET_LIMIT, cancel: op.cancel(), progress: Some(&sink), ..Default::default() })
        .await
        .map_err(git_cli::remap)?;
    op.done();
    // Phase 2: local, not cancellable.
    if !has_upstream(root).await {
        let tracking = format!("refs/remotes/{remote}/{branch}");
        if git_cli::run(root, &["rev-parse", "--verify", "--quiet", &tracking]).await.is_ok() {
            git_cli::run(root, &["branch", &format!("--set-upstream-to={remote}/{branch}")]).await?;
        } else {
            return Ok(ProjectPullOut { up_to_date: true, note: Some("la rama no existe en el remoto: hacé Push".into()), ..Default::default() });
        }
    }
    let old = git_cli::run(root, &["rev-parse", "--verify", "--quiet", "HEAD"]).await.ok();
    let behind = match &old {
        Some(_) => git_cli::run(root, &["rev-list", "--count", "HEAD..@{upstream}"]).await.ok().and_then(|n| n.parse::<u32>().ok()).unwrap_or(0),
        None => 1,
    };
    if behind == 0 {
        return Ok(ProjectPullOut { up_to_date: true, ..Default::default() });
    }
    let res = if rebase_mode(root, &branch).await {
        git_cli::output(root, &["rebase", "--autostash", "@{upstream}"], RunOpts { env: &[("GIT_EDITOR", "true")], ..Default::default() }).await?
    } else {
        let o = git_cli::output(root, &["merge", "--no-edit", "--autostash", "@{upstream}"], RunOpts::default()).await?;
        if !o.ok() && o.stderr.contains("autostash") && (o.stderr.contains("unknown option") || o.stderr.contains("usage:")) {
            // git older than 2.27: a plain merge only when nothing would be overwritten.
            if !porcelain(root).await?.changes.iter().all(|c| c.mark == 'U') {
                return Err(CommandError::BadRequest("actualizá git a 2.27 o confirmá tus cambios antes del Pull".into()));
            }
            git_cli::output(root, &["merge", "--no-edit", "@{upstream}"], RunOpts::default()).await?
        } else {
            o
        }
    };
    let conflicts = conflicts(root).await;
    let updated = match &old {
        Some(old) => git_cli::run_with(root, &["diff", "--name-only", "-z", old, "HEAD"], RunOpts { raw: true, ..Default::default() }).await,
        None => git_cli::run_with(root, &["ls-tree", "-r", "--name-only", "-z", "HEAD"], RunOpts { raw: true, ..Default::default() }).await,
    }
    .map(|b| nul_list(&b))
    .unwrap_or_default();
    if !res.ok() && conflicts.is_empty() {
        return Err(git_cli::map_error(&res.stderr));
    }
    let operation = operation(root).await;
    Ok(ProjectPullOut { up_to_date: false, updated, conflicts, operation, note: None })
}

fn emit_pull(ev: &dyn ProjectEvents, id: &str, out: &ProjectPullOut) {
    let paths: BTreeSet<String> = out.updated.iter().chain(&out.conflicts).cloned().collect();
    files_changed(ev, id, paths.into_iter().collect(), "pull");
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_pull(app: AppHandle, state: State<'_, AppState>, args: OpArgs) -> CommandResult<ProjectPullOut> {
    pull(&state.store, &app, args).await
}

pub async fn pull(store: &StateStore, ev: &dyn ProjectEvents, args: OpArgs) -> CommandResult<ProjectPullOut> {
    let (id, root) = repo_root(store, &args.id).await?;
    let _busy = Busy::take(&id, "Pull")?;
    let out = pull_inner(&root, ev, &args.op_id).await?;
    emit_pull(ev, &id, &out);
    Ok(out)
}

async fn push_inner(root: &Path, ev: &dyn ProjectEvents, op_id: &str) -> CommandResult<bool> {
    let (branch, remote) = branch_and_remote(root).await?;
    let op = Op::register(Some(op_id));
    let sink = progress_sink(ev, op_id);
    let opts = || RunOpts { limit: NET_LIMIT, cancel: op.cancel(), progress: Some(&sink), ..Default::default() };
    if has_upstream(root).await {
        git_cli::run_with(root, &["push", "--progress"], opts()).await.map_err(git_cli::remap)?;
        Ok(false)
    } else {
        git_cli::run_with(root, &["push", "--progress", "-u", &remote, &branch], opts()).await.map_err(git_cli::remap)?;
        Ok(true)
    }
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_push(app: AppHandle, state: State<'_, AppState>, args: OpArgs) -> CommandResult<bool> {
    push(&state.store, &app, args).await
}

/// True when the push set the branch's upstream.
pub async fn push(store: &StateStore, ev: &dyn ProjectEvents, args: OpArgs) -> CommandResult<bool> {
    let (id, root) = repo_root(store, &args.id).await?;
    let _busy = Busy::take(&id, "Push")?;
    push_inner(&root, ev, &args.op_id).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_sync(app: AppHandle, state: State<'_, AppState>, args: OpArgs) -> CommandResult<ProjectSyncOut> {
    sync(&state.store, &app, args).await
}

/// Pull, then push when there's something to send and no conflict.
pub async fn sync(store: &StateStore, ev: &dyn ProjectEvents, args: OpArgs) -> CommandResult<ProjectSyncOut> {
    let (id, root) = repo_root(store, &args.id).await?;
    let _busy = Busy::take(&id, "Sincronizar")?;
    let pull = pull_inner(&root, ev, &args.op_id).await?;
    emit_pull(ev, &id, &pull);
    if !pull.conflicts.is_empty() {
        return Ok(ProjectSyncOut { pull, pushed: false });
    }
    let pc = porcelain(&root).await?;
    let pushed = if pc.upstream.is_none() || pc.ahead > 0 {
        // Nothing committed yet: nothing to push.
        if pc.head.is_none() {
            false
        } else {
            push_inner(&root, ev, &args.op_id).await?;
            true
        }
    } else {
        false
    };
    Ok(ProjectSyncOut { pull, pushed })
}

// -- conflicts and operations -----------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ConflictArgs {
    pub id: String,
    pub path: String,
    /// `ours` (the user's version), `theirs` (the remote's) or `resolved`.
    pub action: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_conflict(app: AppHandle, state: State<'_, AppState>, args: ConflictArgs) -> CommandResult<()> {
    conflict(&state.store, &app, args).await
}

pub async fn conflict(store: &StateStore, ev: &dyn ProjectEvents, args: ConflictArgs) -> CommandResult<()> {
    let (id, root) = repo_root(store, &args.id).await?;
    rel_ok(&args.path)?;
    let _busy = Busy::take(&id, "Conflicto")?;
    let path = args.path.as_str();
    match args.action.as_str() {
        "ours" | "theirs" => {
            // In a rebase git's "ours" is the upstream: «mine» is the user's
            // side whatever the operation.
            let rebase = operation(&root).await.as_deref() == Some("rebase");
            let mine = (args.action == "ours") != rebase;
            let side = if mine { "--ours" } else { "--theirs" };
            match git_cli::run(&root, &["checkout", side, "--", path]).await {
                Ok(_) => {
                    git_cli::run(&root, &["add", "--", path]).await?;
                }
                // That side deleted the file: keeping it means deleting.
                Err(CommandError::BadRequest(m)) if m.contains("does not have") => {
                    git_cli::run(&root, &["rm", "--quiet", "--", path]).await?;
                }
                Err(e) => return Err(e),
            }
        }
        "resolved" => {
            let exists = resolve_entry(&root, path).is_ok();
            let (add, rm) = (["add", "--", path], ["rm", "--quiet", "--cached", "--", path]);
            git_cli::run(&root, if exists { &add[..] } else { &rm[..] }).await?;
        }
        _ => return Err(CommandError::BadRequest("acción desconocida".into())),
    }
    files_changed(ev, &id, vec![args.path], "operation");
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct OperationArgs {
    pub id: String,
    /// `continue` or `abort`.
    pub action: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_operation(app: AppHandle, state: State<'_, AppState>, args: OperationArgs) -> CommandResult<ProjectPullOut> {
    operation_cmd(&state.store, &app, args).await
}

pub async fn operation_cmd(store: &StateStore, ev: &dyn ProjectEvents, args: OperationArgs) -> CommandResult<ProjectPullOut> {
    let (id, root) = repo_root(store, &args.id).await?;
    let _busy = Busy::take(&id, if args.action == "abort" { "Abortar" } else { "Continuar" })?;
    let Some(op) = operation(&root).await else {
        return Err(CommandError::BadRequest("no hay ninguna operación en curso".into()));
    };
    let before: BTreeSet<String> = porcelain(&root).await?.changes.into_iter().map(|c| c.path).collect();
    let old = git_cli::run(&root, &["rev-parse", "--verify", "--quiet", "HEAD"]).await.ok();
    let editor: &[(&str, &str)] = &[("GIT_EDITOR", "true")];
    let res = match (args.action.as_str(), op.as_str()) {
        ("continue", "merge") => git_cli::output(&root, &["-c", "core.editor=true", "commit", "--no-edit"], RunOpts::default()).await?,
        ("continue", o) => git_cli::output(&root, &[o, "--continue"], RunOpts { env: editor, ..Default::default() }).await?,
        ("abort", o) => git_cli::output(&root, &[o, "--abort"], RunOpts::default()).await?,
        _ => return Err(CommandError::BadRequest("acción desconocida".into())),
    };
    let conflicts = conflicts(&root).await;
    let mut paths = before;
    paths.extend(porcelain(&root).await?.changes.into_iter().map(|c| c.path));
    let updated = match &old {
        Some(old) => git_cli::run_with(&root, &["diff", "--name-only", "-z", old, "HEAD"], RunOpts { raw: true, ..Default::default() }).await.map(|b| nul_list(&b)).unwrap_or_default(),
        None => vec![],
    };
    paths.extend(updated.iter().cloned());
    files_changed(ev, &id, paths.into_iter().collect(), "operation");
    if !res.ok() && conflicts.is_empty() {
        return Err(git_cli::map_error(&res.stderr));
    }
    Ok(ProjectPullOut { up_to_date: false, updated, conflicts, operation: operation(&root).await, note: None })
}

#[derive(Debug, Deserialize)]
pub struct DiscardArgs {
    pub id: String,
    pub paths: Vec<String>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_discard(app: AppHandle, state: State<'_, AppState>, args: DiscardArgs) -> CommandResult<()> {
    discard(&state.store, &app, args).await
}

/// Drop the changes of these files: tracked ones go back to HEAD, untracked
/// ones are deleted.
pub async fn discard(store: &StateStore, ev: &dyn ProjectEvents, args: DiscardArgs) -> CommandResult<()> {
    let (id, root) = repo_root(store, &args.id).await?;
    for p in &args.paths {
        rel_ok(p)?;
    }
    let _busy = Busy::take(&id, "Descartar")?;
    let pc = porcelain(&root).await?;
    let has_head = pc.head.is_some();
    let mut tracked: Vec<String> = Vec::new();
    let mut new_in_index: Vec<String> = Vec::new();
    let mut untracked: Vec<String> = Vec::new();
    for p in &args.paths {
        match pc.changes.iter().find(|c| &c.path == p) {
            Some(c) if c.mark == 'U' => untracked.push(p.clone()),
            Some(c) if c.index == "A" || !has_head => new_in_index.push(p.clone()),
            Some(c) => {
                tracked.push(p.clone());
                if let Some(o) = &c.orig_path {
                    tracked.push(o.clone());
                }
            }
            None => {}
        }
    }
    if !tracked.is_empty() {
        let mut a = vec!["restore", "--source=HEAD", "--staged", "--worktree", "--"];
        a.extend(tracked.iter().map(String::as_str));
        git_cli::run(&root, &a).await?;
    }
    if !new_in_index.is_empty() {
        // Added and never committed: out of the index, then like untracked.
        let mut a = vec!["rm", "--cached", "--quiet", "--force", "--"];
        a.extend(new_in_index.iter().map(String::as_str));
        git_cli::run(&root, &a).await?;
        untracked.extend(new_in_index);
    }
    for p in &untracked {
        if let Ok(abs) = resolve_entry(&root, p) {
            remove_entry(&abs).map_err(|e| CommandError::Internal(format!("no se pudo eliminar «{p}»: {e}")))?;
        }
    }
    files_changed(ev, &id, args.paths, "discard");
    Ok(())
}

// -- tests ------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::projects::tests::{temp_dir, Recorder};
    use crate::commands::projects::{self, CloneArgs, LinkArgs, WriteArgs};

    #[test]
    fn porcelain_v2_is_parsed() {
        let raw = [
            "# branch.oid 1234567890abcdef",
            "# branch.head main",
            "# branch.upstream origin/main",
            "# branch.ab +2 -3",
            "1 .M N... 100644 100644 100644 aaa bbb sql/a b.sql",
            "1 A. N... 000000 100644 100644 000 bbb nuevo.sql",
            "1 .D N... 100644 100644 000000 aaa aaa borrado.sql",
            "2 R. N... 100644 100644 100644 aaa aaa R100 nuevo nombre.sql",
            "viejo.sql",
            "u UU N... 100644 100644 100644 100644 a b c conflicto.sql",
            "? sin seguir/ñ.sql",
            "",
        ]
        .join("\0");
        let p = parse_porcelain(raw.as_bytes());
        assert_eq!(p.head.as_deref(), Some("1234567"));
        assert_eq!(p.branch.as_deref(), Some("main"));
        assert_eq!(p.upstream.as_deref(), Some("origin/main"));
        assert_eq!((p.ahead, p.behind, p.detached), (2, 3, false));
        let marks: Vec<(char, &str)> = p.changes.iter().map(|c| (c.mark, c.path.as_str())).collect();
        assert_eq!(
            marks,
            [('M', "sql/a b.sql"), ('A', "nuevo.sql"), ('D', "borrado.sql"), ('R', "nuevo nombre.sql"), ('C', "conflicto.sql"), ('U', "sin seguir/ñ.sql")]
        );
        assert_eq!(p.changes[3].orig_path.as_deref(), Some("viejo.sql"));
        assert_eq!((p.changes[0].index.as_str(), p.changes[0].worktree.as_str()), (".", "M"));

        let initial = parse_porcelain(b"# branch.oid (initial)\0# branch.head (detached)\0");
        assert_eq!((initial.head, initial.branch, initial.detached), (None, None, true));

        let many: String = (0..5001).map(|i| format!("? f{i}.sql\0")).collect();
        let big = parse_porcelain(many.as_bytes());
        assert_eq!((big.changes.len(), big.truncated), (5000, true));
    }

    #[test]
    fn one_operation_per_project() {
        let a = Busy::take("p-busy", "Pull").unwrap();
        let e = Busy::take("p-busy", "Push").err().unwrap();
        assert!(e.to_string().contains("(Pull)"));
        assert!(Busy::take("p-other", "Push").is_ok());
        drop(a);
        assert!(Busy::take("p-busy", "Push").is_ok());

        let mut op = Op::register(Some("op-1"));
        assert!(cancel_op("op-1"));
        op.done();
        assert!(!cancel_op("op-1"), "past its cancellable phase");
        assert!(!cancel_op("nope"));
    }

    // -- integration: real git, a bare remote and two working copies --------------------

    async fn g(dir: &Path, args: &[&str]) -> String {
        git_cli::run(dir, args).await.unwrap_or_else(|e| panic!("git {args:?}: {e}"))
    }

    /// Local config so the user's global one (if any) doesn't matter.
    async fn configure(dir: &Path) {
        for (k, v) in [("user.name", "Test"), ("user.email", "t@example.com"), ("pull.rebase", "false"), ("commit.gpgsign", "false"), ("core.autocrlf", "false")] {
            g(dir, &["config", k, v]).await;
        }
    }

    struct World {
        base: PathBuf,
        remote: PathBuf,
        store: StateStore,
        ev: Recorder,
    }

    impl World {
        async fn new() -> Option<World> {
            if git_cli::git_path().is_err() {
                eprintln!("git not installed; skipping");
                return None;
            }
            let base = temp_dir("git");
            let remote = base.join("remote.git");
            std::fs::create_dir_all(&remote).unwrap();
            g(&remote, &["init", "--quiet", "--bare"]).await;
            g(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]).await;
            Some(World { base, remote, store: StateStore::open_in_memory().unwrap(), ev: Recorder::default() })
        }

        fn url(&self) -> String {
            self.remote.to_string_lossy().to_string()
        }

        /// Clone the remote through `project_clone` and configure it.
        async fn clone(&self, name: &str) -> (String, PathBuf) {
            let info = projects::clone(
                &self.store,
                &self.ev,
                CloneArgs { url: self.url(), parent_dir: self.base.to_string_lossy().to_string(), name: Some(name.into()), op_id: format!("clone-{name}"), ..Default::default() },
            )
            .await
            .unwrap();
            let root = PathBuf::from(&info.project.path);
            configure(&root).await;
            (info.project.id, root)
        }

        async fn status(&self, id: &str) -> ProjectStatus {
            status(&self.store, &self.ev, StatusArgs { id: id.into(), fetch: false, op_id: None }).await.unwrap()
        }

        fn write(&self, id: &str, path: &str, text: &str) {
            projects::write_file(&self.store, &self.ev, WriteArgs { id: id.into(), path: path.into(), text: text.into(), eol: "lf".into(), bom: false, expected_hash: None }).unwrap();
        }

        async fn commit(&self, id: &str, msg: &str) -> String {
            commit(&self.store, CommitArgs { id: id.into(), message: msg.into() }).await.unwrap()
        }

        fn op(&self, id: &str) -> OpArgs {
            OpArgs { id: id.into(), op_id: uuid::Uuid::new_v4().to_string() }
        }
    }

    impl Drop for World {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn marks(st: &ProjectStatus) -> Vec<(char, String)> {
        let mut v: Vec<(char, String)> = st.changes.iter().map(|c| (c.mark, c.path.clone())).collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn link_init_status_and_unlink() {
        let Some(w) = World::new().await else { return };
        // A plain folder: refused without init, a repo with it.
        let plain = w.base.join("plain");
        std::fs::create_dir_all(plain.join("sub")).unwrap();
        std::fs::write(plain.join("sub/a.sql"), "SELECT 1;\n").unwrap();
        let p = plain.to_string_lossy().to_string();
        assert!(projects::link(&w.store, LinkArgs { path: p.clone(), ..Default::default() }).await.unwrap_err().to_string().contains("no es un repositorio"));
        let info = projects::link(&w.store, LinkArgs { path: p.clone(), init: true, ..Default::default() }).await.unwrap();
        assert!(info.is_repo && info.exists);
        assert_eq!(g(&plain, &["symbolic-ref", "HEAD"]).await, "refs/heads/main");
        configure(&plain).await;
        // Linked twice: refused. A subfolder links to the repo's root, so it's the same.
        assert!(projects::link(&w.store, LinkArgs { path: p.clone(), ..Default::default() }).await.is_err());
        let sub = projects::inspect_folder(&w.store, &plain.join("sub").to_string_lossy()).await.unwrap();
        assert!(sub.is_repo && sub.already_linked.as_deref() == Some(info.project.id.as_str()));
        assert_eq!(sub.repo_root.as_deref(), Some(info.project.path.as_str()));
        assert!(projects::link(&w.store, LinkArgs { path: plain.join("sub").to_string_lossy().to_string(), ..Default::default() }).await.is_err());

        // Initial repo, no remote.
        let id = info.project.id.clone();
        let st = w.status(&id).await;
        assert!(st.git && st.is_repo && st.head.is_none() && !st.has_remote && st.remote.is_none());
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!(marks(&st), [('U', "sub/a.sql".to_string())]);
        assert!(!st.identity_missing);
        // Diff of an untracked file in an initial repo: no before.
        let d = diff(&w.store, DiffArgs { id: id.clone(), path: "sub/a.sql".into() }).await.unwrap();
        assert_eq!((d.mark, d.before, d.after.as_deref()), ('U', None, Some("SELECT 1;\n")));
        // Pull and push need a remote.
        assert!(pull(&w.store, &w.ev, w.op(&id)).await.unwrap_err().to_string().contains("no tiene remoto"));

        w.commit(&id, "primero").await;
        assert!(commit(&w.store, CommitArgs { id: id.clone(), message: "otra".into() }).await.unwrap_err().to_string().contains("no hay cambios"));
        assert!(commit(&w.store, CommitArgs { id: id.clone(), message: "  ".into() }).await.is_err());

        // Detached HEAD: shown, commit refused.
        g(&plain, &["checkout", "--quiet", "--detach"]).await;
        let st = w.status(&id).await;
        assert!(st.detached && st.branch.is_none() && st.head.is_some());
        w.write(&id, "sub/a.sql", "SELECT 2;\n");
        assert!(commit(&w.store, CommitArgs { id: id.clone(), message: "x".into() }).await.unwrap_err().to_string().contains("desacoplado"));
        g(&plain, &["checkout", "--quiet", "main"]).await;

        // Unlink forgets the project and leaves every file.
        projects::unlink(&w.store, &id).unwrap();
        assert!(w.store.list_projects().unwrap().is_empty());
        assert!(plain.join("sub/a.sql").is_file() && plain.join(".git").is_dir());
        let st = status(&w.store, &w.ev, StatusArgs { id: id.clone(), ..Default::default() }).await;
        assert!(st.is_err(), "unlinked");
    }

    #[test]
    fn file_log_is_parsed() {
        let raw = "\u{1e}aaaa1111\u{1f}aaaa\u{1f}Ana\u{1f}2026-10-08T10:00:00-03:00\u{1f}fix: x\u{1f}y\n\nsql/nuevo.sql\n\u{1e}bbbb\u{1f}bbb\u{1f}Leo\u{1f}2026-10-01T09:00:00+00:00\u{1f}base\n\nviejo.sql\n\u{1e}cccc\u{1f}ccc\u{1f}M\u{1f}2026-09-01T09:00:00+00:00\u{1f}merge\n";
        let log = parse_file_log(raw, "sql/nuevo.sql");
        assert_eq!(log.len(), 3);
        assert_eq!((log[0].author.as_str(), log[0].subject.as_str(), log[0].path.as_str()), ("Ana", "fix: x\u{1f}y", "sql/nuevo.sql"));
        assert_eq!(log[1].path, "viejo.sql");
        assert_eq!(log[2].path, "sql/nuevo.sql", "no name (a merge): the asked path");
    }

    #[tokio::test]
    async fn a_files_commits_follow_renames() {
        let Some(w) = World::new().await else { return };
        let (id, root) = w.clone("log").await;
        let log = |p: &str| file_log(&w.store, FileLogArgs { id: id.clone(), path: p.into(), limit: None });
        assert!(log("a.sql").await.unwrap().is_empty(), "nothing committed yet");
        w.write(&id, "a.sql", "select 1;\n");
        w.commit(&id, "primero").await;
        w.write(&id, "a.sql", "select 2;\n");
        w.commit(&id, "segundo").await;
        g(&root, &["mv", "a.sql", "b.sql"]).await;
        w.commit(&id, "renombrado").await;
        let commits = log("b.sql").await.unwrap();
        assert_eq!(commits.iter().map(|c| (c.subject.as_str(), c.path.as_str())).collect::<Vec<_>>(), [("renombrado", "b.sql"), ("segundo", "a.sql"), ("primero", "a.sql")]);
        assert_eq!(commits[0].author, "Test");
        let at = |c: &str, p: &str| file_at(&w.store, FileAtArgs { id: id.clone(), commit: c.into(), path: p.into() });
        assert_eq!(at(&commits[2].hash, "a.sql").await.unwrap().text.as_deref(), Some("select 1;\n"));
        assert_eq!(at(&commits[1].short, "a.sql").await.unwrap().text.as_deref(), Some("select 2;\n"));
        assert_eq!(at(&commits[1].hash, "b.sql").await.unwrap().text, None, "not there yet");
        assert!(at("HEAD", "b.sql").await.is_err() && at("--output=x", "b.sql").await.is_err(), "only a hash");
        assert!(at(&commits[0].hash, "../x").await.is_err() && log("../x").await.is_err());
    }

    #[tokio::test]
    async fn status_marks_and_diffs() {
        let Some(w) = World::new().await else { return };
        let (id, root) = w.clone("a").await;
        w.write(&id, "keep.sql", "uno\n");
        w.write(&id, "gone.sql", "x\n");
        w.write(&id, "old.sql", "rename me\n");
        std::fs::write(root.join("crlf.sql"), "a\r\nb\r\n").unwrap();
        w.commit(&id, "base").await;
        w.write(&id, "keep.sql", "dos\n");
        std::fs::remove_file(root.join("gone.sql")).unwrap();
        g(&root, &["mv", "old.sql", "new.sql"]).await;
        w.write(&id, "added.sql", "nuevo\n");
        g(&root, &["add", "added.sql"]).await;
        w.write(&id, "loose.sql", "suelto\n");
        // Only the line endings change: the diff shows no difference.
        std::fs::write(root.join("crlf.sql"), "a\nb\n").unwrap();
        let st = w.status(&id).await;
        assert_eq!(
            marks(&st),
            [
                ('A', "added.sql".into()),
                ('D', "gone.sql".into()),
                ('M', "crlf.sql".into()),
                ('M', "keep.sql".into()),
                ('R', "new.sql".into()),
                ('U', "loose.sql".into())
            ]
        );
        assert!(st.has_remote && st.remote.as_deref() == Some("origin"));
        // Cloned empty: the upstream is configured but not on the remote yet.
        assert_eq!((st.ahead, st.behind), (0, 0));

        let d = |p: &str| diff(&w.store, DiffArgs { id: id.clone(), path: p.into() });
        let m = d("keep.sql").await.unwrap();
        assert_eq!((m.before.as_deref(), m.after.as_deref()), (Some("uno\n"), Some("dos\n")));
        let del = d("gone.sql").await.unwrap();
        assert_eq!((del.before.as_deref(), del.after), (Some("x\n"), None));
        let r = d("new.sql").await.unwrap();
        assert_eq!((r.orig_path.as_deref(), r.before.as_deref(), r.after.as_deref()), (Some("old.sql"), Some("rename me\n"), Some("rename me\n")));
        let a = d("added.sql").await.unwrap();
        assert_eq!((a.before, a.after.as_deref()), (None, Some("nuevo\n")));
        let e = d("crlf.sql").await.unwrap();
        assert_eq!(e.before, e.after);
        std::fs::write(root.join("bin.dat"), b"\0\x01").unwrap();
        assert!(d("bin.dat").await.unwrap().binary);
        assert!(d("../x").await.is_err());

        // Discard: tracked back to HEAD, staged-new and untracked gone.
        discard(&w.store, &w.ev, DiscardArgs { id: id.clone(), paths: vec!["keep.sql".into(), "gone.sql".into(), "new.sql".into(), "added.sql".into(), "loose.sql".into(), "bin.dat".into(), "crlf.sql".into()] })
            .await
            .unwrap();
        assert!(w.status(&id).await.changes.is_empty(), "{:?}", w.status(&id).await.changes);
        assert_eq!(std::fs::read_to_string(root.join("keep.sql")).unwrap(), "uno\n");
        assert!(root.join("gone.sql").is_file() && root.join("old.sql").is_file());
        assert!(!root.join("loose.sql").exists() && !root.join("added.sql").exists() && !root.join("new.sql").exists());
    }

    /// A file named like a glob (`*`, `:(glob)**`) is a path, not a pattern:
    /// discarding it restores that file only, never the whole tree.
    #[cfg(unix)]
    #[tokio::test]
    async fn discard_takes_paths_literally() {
        let Some(w) = World::new().await else { return };
        let (id, root) = w.clone("literal").await;
        for f in ["*", "a.sql", "b.sql"] {
            std::fs::write(root.join(f), "v1\n").unwrap();
        }
        w.commit(&id, "uno").await;
        for f in ["*", "a.sql", "b.sql"] {
            std::fs::write(root.join(f), "v2\n").unwrap();
        }
        discard(&w.store, &w.ev, DiscardArgs { id: id.clone(), paths: vec!["*".into()] }).await.unwrap();
        assert_eq!(std::fs::read_to_string(root.join("*")).unwrap(), "v1\n");
        assert_eq!(std::fs::read_to_string(root.join("a.sql")).unwrap(), "v2\n", "a.sql kept its change");
        assert_eq!(std::fs::read_to_string(root.join("b.sql")).unwrap(), "v2\n", "b.sql kept its change");
        assert_eq!(marks(&w.status(&id).await), vec![('M', "a.sql".to_string()), ('M', "b.sql".to_string())]);

        // Pathspec magic is literal too.
        std::fs::write(root.join(":(glob)**"), "v1\n").unwrap();
        g(&root, &["add", "--", ":(glob)**"]).await;
        discard(&w.store, &w.ev, DiscardArgs { id: id.clone(), paths: vec![":(glob)**".into()] }).await.unwrap();
        assert!(!root.join(":(glob)**").exists());
        assert_eq!(std::fs::read_to_string(root.join("a.sql")).unwrap(), "v2\n");
    }

    #[tokio::test]
    async fn push_pull_sync_and_conflicts() {
        let Some(w) = World::new().await else { return };
        let (a, ra) = w.clone("a").await;
        assert!(w.ev.progress.lock().unwrap().iter().all(|p| p.op_id == "clone-a"));

        // A: first commit, first push sets the upstream.
        w.write(&a, "sql/v.sql", "1\n");
        w.commit(&a, "uno").await;
        assert!(push(&w.store, &w.ev, w.op(&a)).await.unwrap(), "upstream set");
        let (b, rb) = w.clone("b").await;
        assert_eq!(std::fs::read_to_string(rb.join("sql/v.sql")).unwrap(), "1\n");

        // A pushes again; B is behind after a fetch and the pull lists the file.
        w.write(&a, "sql/v.sql", "2\n");
        w.write(&a, "sql/w.sql", "w\n");
        w.commit(&a, "dos").await;
        assert!(!push(&w.store, &w.ev, w.op(&a)).await.unwrap());
        let st = status(&w.store, &w.ev, StatusArgs { id: b.clone(), fetch: true, op_id: Some("f".into()) }).await.unwrap();
        assert_eq!((st.behind, st.ahead, st.fetch_error.clone()), (1, 0, None));
        w.ev.reasons();
        let out = pull(&w.store, &w.ev, w.op(&b)).await.unwrap();
        assert!(!out.up_to_date && out.conflicts.is_empty());
        assert_eq!(out.updated, ["sql/v.sql", "sql/w.sql"]);
        assert_eq!(w.ev.files.lock().unwrap().last().unwrap().reason, "pull");
        assert!(pull(&w.store, &w.ev, w.op(&b)).await.unwrap().up_to_date);

        // B pushes while A has a commit of its own: rejected with the hint, then sync settles it.
        w.write(&b, "sql/b.sql", "b\n");
        w.commit(&b, "de b").await;
        w.write(&a, "sql/a.sql", "a\n");
        w.commit(&a, "de a").await;
        assert!(!push(&w.store, &w.ev, w.op(&a)).await.unwrap());
        let e = push(&w.store, &w.ev, w.op(&b)).await.unwrap_err();
        assert!(e.to_string().contains("hacé Pull o Sincronizar"), "{e}");
        let s = sync(&w.store, &w.ev, w.op(&b)).await.unwrap();
        assert!(s.pushed && s.pull.conflicts.is_empty() && s.pull.updated.contains(&"sql/a.sql".to_string()));
        assert_eq!(w.status(&b).await.ahead, 0);

        // Both edit the same line: B's pull stops in a merge with the conflict.
        sync(&w.store, &w.ev, w.op(&a)).await.unwrap();
        w.write(&a, "sql/v.sql", "A\n");
        w.commit(&a, "A").await;
        push(&w.store, &w.ev, w.op(&a)).await.unwrap();
        w.write(&b, "sql/v.sql", "B\n");
        w.commit(&b, "B").await;
        let out = pull(&w.store, &w.ev, w.op(&b)).await.unwrap();
        assert_eq!((out.conflicts.clone(), out.operation.as_deref()), (vec!["sql/v.sql".to_string()], Some("merge")));
        let st = w.status(&b).await;
        assert_eq!(st.operation.as_deref(), Some("merge"));
        assert!(st.changes.iter().any(|c| c.mark == 'C' && c.path == "sql/v.sql"));
        let d = diff(&w.store, DiffArgs { id: b.clone(), path: "sql/v.sql".into() }).await.unwrap();
        assert_eq!((d.mark, d.before.as_deref(), d.after.as_deref()), ('C', Some("B\n"), Some("A\n")));
        assert!(commit(&w.store, CommitArgs { id: b.clone(), message: "x".into() }).await.unwrap_err().to_string().contains("conflictos"));
        // Keep the remote's version, continue: the merge is concluded.
        conflict(&w.store, &w.ev, ConflictArgs { id: b.clone(), path: "sql/v.sql".into(), action: "theirs".into() }).await.unwrap();
        let done = operation_cmd(&w.store, &w.ev, OperationArgs { id: b.clone(), action: "continue".into() }).await.unwrap();
        assert!(done.conflicts.is_empty() && done.operation.is_none());
        assert_eq!(std::fs::read_to_string(rb.join("sql/v.sql")).unwrap(), "A\n");
        let st = w.status(&b).await;
        assert!(st.changes.is_empty() && st.operation.is_none() && st.ahead == 2);
        push(&w.store, &w.ev, w.op(&b)).await.unwrap();

        // Rebase mode (pull.rebase=true): a conflict leaves a rebase; «ours»
        // keeps the user's side, and continuing ends with a linear history.
        pull(&w.store, &w.ev, w.op(&a)).await.unwrap();
        g(&ra, &["config", "pull.rebase", "true"]).await;
        w.write(&b, "sql/v.sql", "B2\n");
        w.commit(&b, "B2").await;
        push(&w.store, &w.ev, w.op(&b)).await.unwrap();
        w.write(&a, "sql/v.sql", "A2\n");
        w.commit(&a, "A2").await;
        let out = pull(&w.store, &w.ev, w.op(&a)).await.unwrap();
        assert_eq!((out.conflicts.len(), out.operation.as_deref()), (1, Some("rebase")));
        assert!(commit(&w.store, CommitArgs { id: a.clone(), message: "x".into() }).await.unwrap_err().to_string().contains("rebase"));
        conflict(&w.store, &w.ev, ConflictArgs { id: a.clone(), path: "sql/v.sql".into(), action: "ours".into() }).await.unwrap();
        assert_eq!(std::fs::read_to_string(ra.join("sql/v.sql")).unwrap(), "A2\n", "ours = the user's side in a rebase too");
        let done = operation_cmd(&w.store, &w.ev, OperationArgs { id: a.clone(), action: "continue".into() }).await.unwrap();
        assert!(done.conflicts.is_empty() && done.operation.is_none(), "{done:?}");
        assert_eq!(g(&ra, &["rev-list", "--merges", "--count", "HEAD~2..HEAD"]).await, "0");
        assert_eq!(std::fs::read_to_string(ra.join("sql/v.sql")).unwrap(), "A2\n");
        push(&w.store, &w.ev, w.op(&a)).await.unwrap();

        // Abort: back to where the user was.
        pull(&w.store, &w.ev, w.op(&b)).await.unwrap();
        w.write(&b, "sql/v.sql", "B3\n");
        w.commit(&b, "B3").await;
        push(&w.store, &w.ev, w.op(&b)).await.unwrap();
        w.write(&a, "sql/v.sql", "A3\n");
        w.commit(&a, "A3").await;
        assert_eq!(pull(&w.store, &w.ev, w.op(&a)).await.unwrap().operation.as_deref(), Some("rebase"));
        let ab = operation_cmd(&w.store, &w.ev, OperationArgs { id: a.clone(), action: "abort".into() }).await.unwrap();
        assert!(ab.operation.is_none() && ab.conflicts.is_empty());
        assert_eq!(std::fs::read_to_string(ra.join("sql/v.sql")).unwrap(), "A3\n");
        assert!(w.status(&a).await.changes.is_empty());

        // A branch the remote doesn't have: pull says to push; push sets it up.
        g(&ra, &["checkout", "--quiet", "-b", "feature"]).await;
        let out = pull(&w.store, &w.ev, w.op(&a)).await.unwrap();
        assert!(out.up_to_date && out.note.unwrap().contains("Push"));
        assert!(push(&w.store, &w.ev, w.op(&a)).await.unwrap());
        assert_eq!(w.status(&a).await.upstream.as_deref(), Some("origin/feature"));

        // The lock: a second operation on the same project is refused.
        let held = Busy::take(&a, "Pull").unwrap();
        assert!(push(&w.store, &w.ev, w.op(&a)).await.unwrap_err().to_string().contains("en curso"));
        // Status still works, and a fetch is skipped quietly.
        assert!(status(&w.store, &w.ev, StatusArgs { id: a.clone(), fetch: true, op_id: None }).await.is_ok());
        drop(held);
    }

    #[tokio::test]
    async fn a_cancelled_clone_leaves_nothing() {
        let Some(w) = World::new().await else { return };
        // Give the remote something to clone.
        let (a, _) = w.clone("seed").await;
        w.write(&a, "x.sql", "x\n");
        w.commit(&a, "x").await;
        push(&w.store, &w.ev, w.op(&a)).await.unwrap();

        // The cancel arrives right away (from another task, like the UI's).
        let op_id = "clone-cancel";
        let canceller = tokio::spawn(async move {
            for _ in 0..200 {
                if cancel_op(op_id) {
                    return true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
            false
        });
        let res = projects::clone(
            &w.store,
            &w.ev,
            CloneArgs { url: format!("file://{}", w.url()), parent_dir: w.base.to_string_lossy().to_string(), name: Some("cancelled".into()), op_id: op_id.into(), ..Default::default() },
        )
        .await;
        let cancelled = canceller.await.unwrap();
        match res {
            Err(e) if cancelled => {
                assert_eq!(e.to_string(), "operación cancelada");
                assert!(!w.base.join("cancelled").exists(), "the target it created is removed");
            }
            // The clone finished before the cancel landed: still a valid project.
            other => assert!(other.is_ok()),
        }
        assert!(!cancel_op(op_id), "no longer registered");

        // A target that exists and isn't empty is refused.
        std::fs::create_dir_all(w.base.join("full")).unwrap();
        std::fs::write(w.base.join("full/f"), "x").unwrap();
        let e = projects::clone(&w.store, &w.ev, CloneArgs { url: w.url(), parent_dir: w.base.to_string_lossy().to_string(), name: Some("full".into()), op_id: "o".into(), ..Default::default() })
            .await
            .unwrap_err();
        assert!(e.to_string().contains("no está vacía"));
        assert!(w.base.join("full/f").is_file());
        // A bad URL fails and leaves no folder.
        assert!(projects::clone(&w.store, &w.ev, CloneArgs { url: w.base.join("nope.git").to_string_lossy().to_string(), parent_dir: w.base.to_string_lossy().to_string(), name: Some("bad".into()), op_id: "o2".into(), ..Default::default() })
            .await
            .is_err());
        assert!(!w.base.join("bad").exists());
    }
}
