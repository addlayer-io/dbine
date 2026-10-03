//! Projects ("Proyectos", docs/proyectos.md): git working copies of the
//! user's SQL files, linked to DBine. A project is a folder and a local row
//! (`dbine_core::Project`); it never owns connections and isn't synced. The
//! repo may carry a `.dbine.json` naming its environments (never
//! credentials); which connection and database each one means is this
//! machine's choice (`ProjectBinding`).
//!
//! Every file operation goes through here, with the path checked by
//! `project_paths` to stay inside the project. The git side is in
//! `projects_git.rs`.

use super::git_cli::{self, RunOpts, NET_LIMIT};
use super::project_paths::{canonical_root, rel_ok, resolve_entry, resolve_existing, resolve_new, to_rel};
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::{Project, ProjectBinding, StateStore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager, State};

pub const MANIFEST_FILE: &str = ".dbine.json";
const MANIFEST_MAX: u64 = 64 * 1024;
const READ_MAX: u64 = 5 * 1024 * 1024;
const STAT_MAX: usize = 200;

// -- events -----------------------------------------------------------------------------

/// `project-files-changed`: the working tree changed (every window listens).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectFilesChanged {
    pub project_id: String,
    pub paths: Vec<String>,
    /// `write`, `create`, `rename`, `delete`, `pull`, `discard`, `operation`, `checkout`.
    pub reason: String,
}

/// `project-git-progress`: a `--progress` line of a long git command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitProgress {
    pub op_id: String,
    pub phase: String,
    pub percent: Option<u32>,
}

/// Where the commands' events go: the app, or a recorder in tests.
pub trait ProjectEvents: Send + Sync {
    fn files_changed(&self, e: ProjectFilesChanged);
    fn progress(&self, p: GitProgress);
}

impl ProjectEvents for AppHandle {
    fn files_changed(&self, e: ProjectFilesChanged) {
        let _ = self.emit("project-files-changed", e);
    }
    fn progress(&self, p: GitProgress) {
        let _ = self.emit("project-git-progress", p);
    }
}

pub fn files_changed(ev: &dyn ProjectEvents, id: &str, paths: Vec<String>, reason: &str) {
    ev.files_changed(ProjectFilesChanged { project_id: id.to_string(), paths, reason: reason.to_string() });
}

/// A progress line as the UI shows it: the phase before `:`, the `NN%`.
pub fn parse_progress(op_id: &str, line: &str) -> Option<GitProgress> {
    let line = line.trim();
    let line = line.strip_prefix("remote:").map(str::trim).unwrap_or(line);
    if line.is_empty() {
        return None;
    }
    let phase = line.split_once(':').map(|(p, _)| p).unwrap_or(line).trim();
    let percent = line.find('%').and_then(|i| {
        let digits: String = line[..i].chars().rev().take_while(|c| c.is_ascii_digit()).collect();
        digits.chars().rev().collect::<String>().parse().ok()
    });
    Some(GitProgress { op_id: op_id.to_string(), phase: git_cli::redact_url(phase), percent })
}

/// A progress callback for `RunOpts`, about ten events a second at most
/// (a new phase and 100% always go through).
pub fn progress_sink<'a>(ev: &'a dyn ProjectEvents, op_id: &'a str) -> impl Fn(&str) + Send + Sync + 'a {
    let last: Mutex<Option<(Instant, String)>> = Mutex::new(None);
    move |line: &str| {
        let Some(p) = parse_progress(op_id, line) else { return };
        let mut l = last.lock().unwrap_or_else(|e| e.into_inner());
        let due = match &*l {
            None => true,
            Some((at, phase)) => at.elapsed() >= Duration::from_millis(100) || *phase != p.phase || p.percent == Some(100),
        };
        if due {
            *l = Some((Instant::now(), p.phase.clone()));
            ev.progress(p);
        }
    }
}

// -- manifest ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProjectEnvironment {
    pub name: String,
    /// The driver id; the root's when the environment doesn't say.
    #[serde(default)]
    pub engine: Option<String>,
    /// Running against it asks first.
    #[serde(default)]
    pub confirm_run: bool,
    #[serde(default)]
    pub description: String,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ProjectManifest {
    #[serde(default = "one")]
    pub version: u32,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub engine: Option<String>,
    #[serde(default)]
    pub environments: Vec<ProjectEnvironment>,
    #[serde(default)]
    pub default_environment: Option<String>,
}

/// What reading `.dbine.json` gave: the manifest, or why not, and what was
/// ignored.
#[derive(Debug, Default, PartialEq)]
pub struct ManifestRead {
    pub manifest: Option<ProjectManifest>,
    pub error: Option<String>,
    pub warnings: Vec<String>,
}

const CREDENTIAL_KEYS: &[&str] =
    &["password", "pwd", "secret", "token", "user", "username", "host", "server", "port", "connection_string", "uri", "url", "dsn"];

/// Drop credential-looking keys at any level, naming each one once.
fn strip_credentials(v: &mut serde_json::Value, warnings: &mut Vec<String>) {
    match v {
        serde_json::Value::Object(map) => {
            let bad: Vec<String> = map.keys().filter(|k| CREDENTIAL_KEYS.contains(&k.to_ascii_lowercase().as_str())).cloned().collect();
            for k in bad {
                map.remove(&k);
                let w = format!("El archivo .dbine.json no debe tener credenciales; se ignoró «{k}»");
                if !warnings.contains(&w) {
                    warnings.push(w);
                }
            }
            map.values_mut().for_each(|x| strip_credentials(x, warnings));
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(|x| strip_credentials(x, warnings)),
        _ => {}
    }
}

fn env_name_ok(n: &str) -> bool {
    (1..=40).contains(&n.len()) && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn clean(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Check a manifest: environment names, no repeats, a known default.
fn validate_manifest(m: &mut ProjectManifest, warnings: &mut Vec<String>) -> Result<(), String> {
    m.name = clean(m.name.take());
    m.engine = clean(m.engine.take()).map(|e| e.to_ascii_lowercase());
    let mut seen: Vec<String> = Vec::new();
    for e in &mut m.environments {
        e.name = e.name.trim().to_string();
        if !env_name_ok(&e.name) {
            return Err(format!("Nombre de entorno inválido «{}»: usá letras, números, «_», «.» o «-» (hasta 40)", e.name));
        }
        if seen.contains(&e.name.to_ascii_lowercase()) {
            return Err(format!("El entorno «{}» está repetido", e.name));
        }
        seen.push(e.name.to_ascii_lowercase());
        e.engine = clean(e.engine.take()).map(|x| x.to_ascii_lowercase());
        e.description = e.description.trim().to_string();
    }
    m.default_environment = clean(m.default_environment.take());
    if let Some(d) = &m.default_environment {
        if !m.environments.iter().any(|e| &e.name == d) {
            warnings.push(format!("El entorno por defecto «{d}» no está en la lista de entornos"));
            m.default_environment = None;
        }
    }
    Ok(())
}

/// Parse `.dbine.json`. Unknown keys are ignored; credential keys are
/// dropped with a warning; the environments' engine falls back to the root's.
pub fn parse_manifest(bytes: &[u8]) -> ManifestRead {
    let mut out = ManifestRead::default();
    if bytes.len() as u64 > MANIFEST_MAX {
        out.error = Some("El archivo .dbine.json es demasiado grande (máximo 64 KB)".into());
        return out;
    }
    let mut value: serde_json::Value = match serde_json::from_slice(bytes) {
        Ok(v) => v,
        Err(e) => {
            out.error = Some(format!("El archivo .dbine.json no es JSON válido: {e}"));
            return out;
        }
    };
    if !value.is_object() {
        out.error = Some("El archivo .dbine.json tiene que ser un objeto JSON".into());
        return out;
    }
    strip_credentials(&mut value, &mut out.warnings);
    let mut m: ProjectManifest = match serde_json::from_value(value) {
        Ok(m) => m,
        Err(e) => {
            out.error = Some(format!("El archivo .dbine.json no tiene el formato esperado: {e}"));
            return out;
        }
    };
    if let Err(e) = validate_manifest(&mut m, &mut out.warnings) {
        out.error = Some(e);
        return out;
    }
    for e in &mut m.environments {
        if e.engine.is_none() {
            e.engine = m.engine.clone();
        }
    }
    out.manifest = Some(m);
    out
}

pub fn read_manifest(root: &Path) -> ManifestRead {
    let p = root.join(MANIFEST_FILE);
    match std::fs::metadata(&p) {
        Ok(m) if m.is_file() && m.len() > MANIFEST_MAX => {
            ManifestRead { error: Some("El archivo .dbine.json es demasiado grande (máximo 64 KB)".into()), ..Default::default() }
        }
        Ok(m) if m.is_file() => match std::fs::read(&p) {
            Ok(b) => parse_manifest(&b),
            Err(e) => ManifestRead { error: Some(format!("No se pudo leer .dbine.json: {e}")), ..Default::default() },
        },
        _ => ManifestRead::default(),
    }
}

/// The manifest as written: the root's engine isn't repeated per environment,
/// defaults are left out, and keys DBine doesn't know are kept.
fn manifest_json(m: &ProjectManifest, existing: Option<serde_json::Value>) -> serde_json::Value {
    use serde_json::{json, Map, Value};
    let mut obj = match existing {
        Some(Value::Object(o)) => o,
        _ => Map::new(),
    };
    let mut set = |k: &str, v: Option<Value>| match v {
        Some(v) => {
            obj.insert(k.to_string(), v);
        }
        None => {
            obj.remove(k);
        }
    };
    set("version", Some(json!(m.version.max(1))));
    set("name", m.name.clone().map(Value::from));
    set("engine", m.engine.clone().map(Value::from));
    let envs: Vec<Value> = m
        .environments
        .iter()
        .map(|e| {
            let mut o = Map::new();
            o.insert("name".into(), json!(e.name));
            if let Some(engine) = e.engine.as_ref().filter(|x| Some(*x) != m.engine.as_ref()) {
                o.insert("engine".into(), json!(engine));
            }
            if e.confirm_run {
                o.insert("confirm_run".into(), json!(true));
            }
            if !e.description.is_empty() {
                o.insert("description".into(), json!(e.description));
            }
            Value::Object(o)
        })
        .collect();
    set("environments", if envs.is_empty() { None } else { Some(Value::Array(envs)) });
    set("default_environment", m.default_environment.clone().map(Value::from));
    Value::Object(obj)
}

// -- registry ---------------------------------------------------------------------------

/// A project as the UI lists it: the row plus what's on disk.
#[derive(Debug, Clone, Serialize)]
pub struct ProjectInfo {
    #[serde(flatten)]
    pub project: Project,
    pub exists: bool,
    pub is_repo: bool,
    pub manifest: Option<ProjectManifest>,
    pub manifest_error: Option<String>,
    pub manifest_warnings: Vec<String>,
}

pub fn info(project: Project) -> ProjectInfo {
    let root = PathBuf::from(&project.path);
    let exists = root.is_dir();
    let is_repo = exists && root.join(".git").exists();
    let m = if exists { read_manifest(&root) } else { ManifestRead::default() };
    ProjectInfo { project, exists, is_repo, manifest: m.manifest, manifest_error: m.error, manifest_warnings: m.warnings }
}

pub fn get(store: &StateStore, id: &str) -> CommandResult<Project> {
    store.get_project(id)?.ok_or_else(|| CommandError::NotFound("el proyecto no existe".into()))
}

/// The project and its root, canonical (the folder must exist).
pub fn root_of(store: &StateStore, id: &str) -> CommandResult<(Project, PathBuf)> {
    let p = get(store, id)?;
    let root = canonical_root(Path::new(&p.path))?;
    Ok((p, root))
}

fn path_string(p: &Path) -> String {
    git_cli::cwd(p).to_string_lossy().to_string()
}

/// The repo's top level for a folder inside it, canonical; `None` when the
/// folder isn't in a repo (or git is missing).
async fn repo_top(dir: &Path) -> Option<PathBuf> {
    if git_cli::git_path().is_err() {
        return dir.join(".git").exists().then(|| dir.to_path_buf());
    }
    let top = git_cli::run(dir, &["rev-parse", "--show-toplevel"]).await.ok()?;
    PathBuf::from(top).canonicalize().ok()
}

fn folder_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "proyecto".into())
}

/// Set the active environment from the manifest when the binding has none:
/// the default one, or the first. An alias the manifest lacks is dropped.
fn settle_binding(mut b: ProjectBinding, m: Option<&ProjectManifest>) -> ProjectBinding {
    let envs: Vec<&str> = m.map(|m| m.environments.iter().map(|e| e.name.as_str()).collect()).unwrap_or_default();
    if b.active_environment.as_deref().is_some_and(|a| !envs.contains(&a)) {
        b.active_environment = None;
    }
    if b.active_environment.is_none() && !envs.is_empty() {
        // An environment the caller mapped (the Explorer's preset) wins.
        b.active_environment = b
            .environments
            .keys()
            .find(|k| envs.contains(&k.as_str()))
            .cloned()
            .or_else(|| m.and_then(|m| m.default_environment.clone()))
            .or_else(|| envs.first().map(|e| e.to_string()));
    }
    b
}

#[tauri::command]
pub async fn list_projects(state: State<'_, AppState>) -> CommandResult<Vec<ProjectInfo>> {
    Ok(state.store.list_projects()?.into_iter().map(info).collect())
}

#[tauri::command]
pub async fn project_default_dir(app: AppHandle) -> CommandResult<String> {
    let home = app.path().home_dir().map_err(|e| CommandError::Internal(e.to_string()))?;
    Ok(path_string(&home.join("DBine").join("Proyectos")))
}

#[derive(Debug, Deserialize)]
pub struct PathArgs {
    pub path: String,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct FolderInspect {
    pub path: String,
    pub exists: bool,
    pub is_repo: bool,
    pub repo_root: Option<String>,
    /// The project already linked to this folder (or its repo).
    pub already_linked: Option<String>,
    pub suggested_name: String,
    pub has_manifest: bool,
    pub empty: bool,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_inspect_folder(state: State<'_, AppState>, args: PathArgs) -> CommandResult<FolderInspect> {
    inspect_folder(&state.store, &args.path).await
}

pub async fn inspect_folder(store: &StateStore, path: &str) -> CommandResult<FolderInspect> {
    let given = PathBuf::from(path.trim());
    let Ok(dir) = canonical_root(&given) else {
        return Ok(FolderInspect {
            path: path.trim().to_string(),
            exists: false,
            is_repo: false,
            repo_root: None,
            already_linked: None,
            suggested_name: folder_name(&given),
            has_manifest: false,
            empty: true,
        });
    };
    let top = repo_top(&dir).await;
    let base = top.clone().unwrap_or_else(|| dir.clone());
    let key = path_string(&base);
    let already_linked = store.list_projects()?.into_iter().find(|p| p.path == key).map(|p| p.id);
    let m = read_manifest(&base);
    let suggested_name = m.manifest.as_ref().and_then(|m| m.name.clone()).unwrap_or_else(|| folder_name(&base));
    let empty = std::fs::read_dir(&dir).map(|mut d| d.next().is_none()).unwrap_or(true);
    Ok(FolderInspect {
        path: path_string(&dir),
        exists: true,
        is_repo: top.is_some(),
        repo_root: top.map(|t| path_string(&t)),
        already_linked,
        suggested_name,
        has_manifest: base.join(MANIFEST_FILE).is_file(),
        empty,
    })
}

#[derive(Debug, Deserialize, Default)]
pub struct LinkArgs {
    pub path: String,
    #[serde(default)]
    pub name: Option<String>,
    /// Not a repo yet: `git init` it.
    #[serde(default)]
    pub init: bool,
    #[serde(default)]
    pub binding: Option<ProjectBinding>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_link(state: State<'_, AppState>, args: LinkArgs) -> CommandResult<ProjectInfo> {
    link(&state.store, args).await
}

pub async fn link(store: &StateStore, args: LinkArgs) -> CommandResult<ProjectInfo> {
    let dir = canonical_root(Path::new(args.path.trim()))?;
    let root = match repo_top(&dir).await {
        Some(top) => top,
        None if args.init => {
            git_cli::run(&dir, &["init", "--quiet"]).await?;
            git_cli::run(&dir, &["symbolic-ref", "HEAD", "refs/heads/main"]).await?;
            dir
        }
        None => return Err(CommandError::BadRequest("la carpeta no es un repositorio git".into())),
    };
    let path = path_string(&root);
    if store.list_projects()?.iter().any(|p| p.path == path) {
        return Err(CommandError::BadRequest("esa carpeta ya está vinculada como proyecto".into()));
    }
    let m = read_manifest(&root);
    let name = clean(args.name)
        .or_else(|| m.manifest.as_ref().and_then(|m| m.name.clone()))
        .unwrap_or_else(|| folder_name(&root));
    let sort_order = store.list_projects()?.iter().map(|p| p.sort_order + 1).max().unwrap_or(0);
    let binding = settle_binding(args.binding.unwrap_or_default(), m.manifest.as_ref());
    let saved = store.save_project(&Project {
        id: uuid::Uuid::new_v4().to_string(),
        name,
        path,
        binding,
        sort_order,
        ..Default::default()
    })?;
    tracing::info!(project = %saved.id, "project linked");
    Ok(info(saved))
}

#[derive(Debug, Deserialize, Default)]
pub struct CloneArgs {
    pub url: String,
    pub parent_dir: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    pub op_id: String,
    #[serde(default)]
    pub binding: Option<ProjectBinding>,
}

/// The folder name a URL suggests: its last segment without `.git`.
pub fn repo_name_of(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    let last = u.rsplit(['/', ':', '\\']).next().unwrap_or("");
    let last = last.strip_suffix(".git").unwrap_or(last);
    crate::commands::library::safe_name(if last.is_empty() { "repo" } else { last })
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_clone(app: AppHandle, state: State<'_, AppState>, args: CloneArgs) -> CommandResult<ProjectInfo> {
    clone(&state.store, &app, args).await
}

pub async fn clone(store: &StateStore, ev: &dyn ProjectEvents, args: CloneArgs) -> CommandResult<ProjectInfo> {
    let url = args.url.trim().to_string();
    if url.is_empty() {
        return Err(CommandError::BadRequest("falta la URL del repositorio".into()));
    }
    let name = match clean(args.name.clone()) {
        Some(n) => crate::commands::library::safe_name(&n),
        None => repo_name_of(&url),
    };
    let parent = PathBuf::from(args.parent_dir.trim());
    if args.parent_dir.trim().is_empty() {
        return Err(CommandError::BadRequest("falta la carpeta donde clonar".into()));
    }
    std::fs::create_dir_all(&parent).map_err(|e| CommandError::BadRequest(format!("no se pudo crear la carpeta «{}»: {e}", parent.display())))?;
    let parent = canonical_root(&parent)?;
    let target = parent.join(&name);
    let created = !target.exists();
    if !created && std::fs::read_dir(&target).map(|mut d| d.next().is_some()).unwrap_or(true) {
        return Err(CommandError::BadRequest(format!("ya existe «{}» y no está vacía", target.display())));
    }
    let target_s = path_string(&target);
    let mut cmd = vec!["clone", "--progress"];
    let branch = clean(args.branch.clone());
    if let Some(b) = &branch {
        cmd.extend(["--branch", b.as_str()]);
    }
    cmd.extend(["--", url.as_str(), target_s.as_str()]);
    let op = super::projects_git::Op::register(Some(&args.op_id));
    let sink = progress_sink(ev, &args.op_id);
    let res = git_cli::run_with(&parent, &cmd, RunOpts { limit: NET_LIMIT, cancel: op.cancel(), progress: Some(&sink), ..Default::default() }).await;
    drop(op);
    if let Err(e) = res {
        if created && target.exists() {
            // A killed git may still have a helper writing for a moment.
            for _ in 0..5 {
                if std::fs::remove_dir_all(&target).is_ok() || !target.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        tracing::warn!(url = %git_cli::redact_url(&url), "project clone failed");
        return Err(git_cli::remap(e));
    }
    link(store, LinkArgs { path: target_s, name: args.name, init: false, binding: args.binding }).await
}

#[derive(Debug, Deserialize)]
pub struct UpdateArgs {
    pub id: String,
    pub name: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_update(state: State<'_, AppState>, args: UpdateArgs) -> CommandResult<ProjectInfo> {
    let name = args.name.trim();
    if name.is_empty() {
        return Err(CommandError::BadRequest("el proyecto necesita un nombre".into()));
    }
    let p = get(&state.store, &args.id)?;
    Ok(info(state.store.save_project(&Project { name: name.to_string(), ..p })?))
}

#[derive(Debug, Deserialize)]
pub struct RelocateArgs {
    pub id: String,
    pub path: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_relocate(state: State<'_, AppState>, args: RelocateArgs) -> CommandResult<ProjectInfo> {
    relocate(&state.store, args).await
}

pub async fn relocate(store: &StateStore, args: RelocateArgs) -> CommandResult<ProjectInfo> {
    let p = get(store, &args.id)?;
    let dir = canonical_root(Path::new(args.path.trim()))?;
    let root = repo_top(&dir).await.ok_or_else(|| CommandError::BadRequest("la carpeta no es un repositorio git".into()))?;
    let path = path_string(&root);
    if store.list_projects()?.iter().any(|o| o.path == path && o.id != p.id) {
        return Err(CommandError::BadRequest("esa carpeta ya está vinculada como proyecto".into()));
    }
    Ok(info(store.save_project(&Project { path, ..p })?))
}

#[derive(Debug, Deserialize)]
pub struct IdArgs {
    pub id: String,
}

/// Forget the project. Its folder and files stay as they are.
#[tauri::command(rename_all = "camelCase")]
pub async fn project_unlink(state: State<'_, AppState>, args: IdArgs) -> CommandResult<()> {
    unlink(&state.store, &args.id)
}

pub fn unlink(store: &StateStore, id: &str) -> CommandResult<()> {
    get(store, id)?;
    store.delete_project(id)?;
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct BindingArgs {
    pub id: String,
    pub binding: ProjectBinding,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_set_binding(state: State<'_, AppState>, args: BindingArgs) -> CommandResult<ProjectInfo> {
    set_binding(&state.store, args)
}

pub fn set_binding(store: &StateStore, args: BindingArgs) -> CommandResult<ProjectInfo> {
    let p = get(store, &args.id)?;
    if let Some(active) = &args.binding.active_environment {
        let m = read_manifest(Path::new(&p.path)).manifest;
        if !m.is_some_and(|m| m.environments.iter().any(|e| &e.name == active)) {
            return Err(CommandError::BadRequest(format!("el entorno «{active}» no está definido en .dbine.json")));
        }
    }
    Ok(info(store.set_project_binding(&args.id, &args.binding)?))
}

#[derive(Debug, Deserialize)]
pub struct ReorderArgs {
    pub ids: Vec<String>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_reorder(state: State<'_, AppState>, args: ReorderArgs) -> CommandResult<()> {
    Ok(state.store.reorder_projects(&args.ids)?)
}

#[derive(Debug, Deserialize)]
pub struct ManifestArgs {
    pub id: String,
    pub manifest: ProjectManifest,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_write_manifest(app: AppHandle, state: State<'_, AppState>, args: ManifestArgs) -> CommandResult<ProjectInfo> {
    write_manifest(&state.store, &app, args)
}

pub fn write_manifest(store: &StateStore, ev: &dyn ProjectEvents, args: ManifestArgs) -> CommandResult<ProjectInfo> {
    let (p, root) = root_of(store, &args.id)?;
    let mut m = args.manifest;
    let mut warnings = Vec::new();
    validate_manifest(&mut m, &mut warnings).map_err(CommandError::BadRequest)?;
    if let Some(d) = &warnings.first() {
        return Err(CommandError::BadRequest(d.to_string()));
    }
    let file = root.join(MANIFEST_FILE);
    let existing = std::fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok());
    let text = serde_json::to_string_pretty(&manifest_json(&m, existing)).map_err(|e| CommandError::Internal(e.to_string()))? + "\n";
    atomic_write(&file, text.as_bytes())?;
    files_changed(ev, &p.id, vec![MANIFEST_FILE.into()], "write");
    Ok(info(p))
}

#[derive(Debug, Deserialize)]
pub struct RevealArgs {
    pub id: String,
    #[serde(default)]
    pub path: Option<String>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_reveal(app: AppHandle, state: State<'_, AppState>, args: RevealArgs) -> CommandResult<()> {
    use tauri_plugin_opener::OpenerExt;
    let (_, root) = root_of(&state.store, &args.id)?;
    let target = match args.path.as_deref().filter(|p| !p.is_empty()) {
        Some(rel) => resolve_entry(&root, rel)?,
        None => root,
    };
    app.opener().reveal_item_in_dir(git_cli::cwd(&target)).map_err(|e| CommandError::Internal(e.to_string()))
}

#[derive(Debug, Deserialize)]
pub struct RemoteArgs {
    pub id: String,
    pub url: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_set_remote(state: State<'_, AppState>, args: RemoteArgs) -> CommandResult<()> {
    set_remote(&state.store, args).await
}

pub async fn set_remote(store: &StateStore, args: RemoteArgs) -> CommandResult<()> {
    let (_, root) = root_of(store, &args.id)?;
    let url = args.url.trim();
    if url.is_empty() {
        return Err(CommandError::BadRequest("falta la URL del remoto".into()));
    }
    let verb = if git_cli::run(&root, &["remote", "get-url", "origin"]).await.is_ok() { "set-url" } else { "add" };
    git_cli::run(&root, &["remote", verb, "origin", url]).await.map(|_| ()).map_err(git_cli::remap)
}

#[derive(Debug, Deserialize)]
pub struct IdentityArgs {
    pub id: String,
    pub name: String,
    pub email: String,
    #[serde(default)]
    pub global: bool,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_set_identity(state: State<'_, AppState>, args: IdentityArgs) -> CommandResult<()> {
    let (_, root) = root_of(&state.store, &args.id)?;
    let (name, email) = (args.name.trim(), args.email.trim());
    if name.is_empty() || email.is_empty() {
        return Err(CommandError::BadRequest("completá el nombre y el email".into()));
    }
    for (k, v) in [("user.name", name), ("user.email", email)] {
        let mut a = vec!["config"];
        if args.global {
            a.push("--global");
        }
        a.extend([k, v]);
        git_cli::run(&root, &a).await?;
    }
    Ok(())
}

// -- file system ------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FsEntry {
    pub name: String,
    /// Relative to the root, with `/`.
    pub path: String,
    pub is_dir: bool,
    pub symlink: bool,
    pub size: u64,
    /// Matched by `.gitignore`.
    pub ignored: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FileContent {
    pub path: String,
    /// `\r\n` normalized to `\n`, BOM stripped.
    pub text: String,
    /// `lf` or `crlf`: the first line ending's.
    pub eol: String,
    pub bom: bool,
    pub mtime_ms: u64,
    pub size: u64,
    /// sha256 of the bytes on disk.
    pub hash: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct FileStat {
    pub path: String,
    pub exists: bool,
    #[serde(default)]
    pub mtime_ms: u64,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WriteOut {
    pub written: bool,
    /// The file on disk isn't the one the editor loaded.
    pub conflict: bool,
    pub stat: FileStat,
}

fn io_err(what: &str, e: std::io::Error) -> CommandError {
    CommandError::Internal(format!("no se pudo {what}: {e}"))
}

fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

fn mtime_ms(m: &std::fs::Metadata) -> u64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Size, time and hash of a file; `exists: false` when it isn't one.
pub fn stat_of(abs: &Path, rel: &str) -> FileStat {
    match std::fs::metadata(abs) {
        Ok(m) if m.is_file() => FileStat {
            path: rel.to_string(),
            exists: true,
            mtime_ms: mtime_ms(&m),
            size: m.len(),
            hash: std::fs::read(abs).map(|b| sha256_hex(&b)).unwrap_or_default(),
        },
        _ => FileStat { path: rel.to_string(), exists: false, ..Default::default() },
    }
}

/// Write through a temp file in the same folder and a rename: a crash never
/// leaves half a file. The original's permissions are kept.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> CommandResult<()> {
    let dir = path.parent().ok_or_else(|| CommandError::BadRequest("la ruta no es válida".into()))?;
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.dbine-tmp-{}", uuid::Uuid::new_v4()));
    let res = (|| {
        std::fs::write(&tmp, bytes)?;
        if let Ok(m) = std::fs::metadata(path) {
            std::fs::set_permissions(&tmp, m.permissions())?;
        }
        std::fs::rename(&tmp, path)
    })();
    if let Err(e) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(io_err("guardar el archivo", e));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct ListDirArgs {
    pub id: String,
    /// `""` = the root.
    #[serde(default)]
    pub dir: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_list_dir(state: State<'_, AppState>, args: ListDirArgs) -> CommandResult<Vec<FsEntry>> {
    list_dir(&state.store, args).await
}

pub async fn list_dir(store: &StateStore, args: ListDirArgs) -> CommandResult<Vec<FsEntry>> {
    let (_, root) = root_of(store, &args.id)?;
    let dir_rel = args.dir.trim_end_matches('/');
    let dir = if dir_rel.is_empty() { root.clone() } else { resolve_existing(&root, dir_rel)? };
    if !dir.is_dir() {
        return Err(CommandError::BadRequest(format!("«{dir_rel}» no es una carpeta")));
    }
    let mut out = Vec::new();
    for e in std::fs::read_dir(&dir).map_err(|e| io_err("leer la carpeta", e))?.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.eq_ignore_ascii_case(".git") || name.contains(".dbine-tmp-") {
            continue;
        }
        let Ok(meta) = std::fs::symlink_metadata(e.path()) else { continue };
        let path = if dir_rel.is_empty() { name.clone() } else { format!("{}/{name}", to_rel(&root, &dir)) };
        out.push(FsEntry { name, path, is_dir: meta.is_dir(), symlink: meta.file_type().is_symlink(), size: if meta.is_file() { meta.len() } else { 0 }, ignored: false });
    }
    out.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    if !out.is_empty() && git_cli::git_path().is_ok() {
        let input: Vec<u8> = out.iter().flat_map(|e| e.path.bytes().chain(std::iter::once(0))).collect();
        if let Ok(o) = git_cli::output(&root, &["check-ignore", "-z", "--stdin"], RunOpts { input: Some(&input), ..Default::default() }).await {
            let ignored: std::collections::HashSet<String> =
                o.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).to_string()).collect();
            for e in &mut out {
                e.ignored = ignored.contains(&e.path);
            }
        }
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
pub struct FileArgs {
    pub id: String,
    pub path: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_read_file(state: State<'_, AppState>, args: FileArgs) -> CommandResult<FileContent> {
    read_file(&state.store, args)
}

/// A text file's bytes as the editor shows them: `None` when binary or not
/// UTF-8.
pub fn decode_text(bytes: &[u8]) -> Option<(String, bool, bool)> {
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return None;
    }
    let (bom, body) = match bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(rest) => (true, rest),
        None => (false, bytes),
    };
    let text = std::str::from_utf8(body).ok()?;
    let crlf = text.find('\n').is_some_and(|i| i > 0 && text.as_bytes()[i - 1] == b'\r');
    Some((text.replace("\r\n", "\n"), bom, crlf))
}

pub fn read_file(store: &StateStore, args: FileArgs) -> CommandResult<FileContent> {
    let (_, root) = root_of(store, &args.id)?;
    let abs = resolve_existing(&root, &args.path)?;
    let meta = std::fs::metadata(&abs).map_err(|e| io_err("leer el archivo", e))?;
    if !meta.is_file() {
        return Err(CommandError::BadRequest(format!("«{}» no es un archivo", args.path)));
    }
    if meta.len() > READ_MAX {
        return Err(CommandError::BadRequest(format!(
            "el archivo es demasiado grande para abrirlo ({:.1} MB)",
            meta.len() as f64 / (1024.0 * 1024.0)
        )));
    }
    let bytes = std::fs::read(&abs).map_err(|e| io_err("leer el archivo", e))?;
    let (text, bom, crlf) = decode_text(&bytes).ok_or_else(|| CommandError::BadRequest("no es un archivo de texto UTF-8".into()))?;
    Ok(FileContent {
        path: args.path,
        text,
        eol: if crlf { "crlf" } else { "lf" }.into(),
        bom,
        mtime_ms: mtime_ms(&meta),
        size: meta.len(),
        hash: sha256_hex(&bytes),
    })
}

#[derive(Debug, Deserialize)]
pub struct WriteArgs {
    pub id: String,
    pub path: String,
    pub text: String,
    #[serde(default)]
    pub eol: String,
    #[serde(default)]
    pub bom: bool,
    /// The hash the editor loaded: a different file on disk isn't overwritten.
    #[serde(default)]
    pub expected_hash: Option<String>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_write_file(app: AppHandle, state: State<'_, AppState>, args: WriteArgs) -> CommandResult<WriteOut> {
    write_file(&state.store, &app, args)
}

pub fn write_file(store: &StateStore, ev: &dyn ProjectEvents, args: WriteArgs) -> CommandResult<WriteOut> {
    let (p, root) = root_of(store, &args.id)?;
    let existing = match resolve_existing(&root, &args.path) {
        Ok(abs) => Some(abs),
        Err(CommandError::NotFound(_)) => None,
        Err(e) => return Err(e),
    };
    if let Some(abs) = &existing {
        if abs.is_dir() {
            return Err(CommandError::BadRequest(format!("«{}» es una carpeta", args.path)));
        }
    }
    if let Some(expected) = &args.expected_hash {
        let now = match &existing {
            Some(abs) => stat_of(abs, &args.path),
            None => FileStat { path: args.path.clone(), exists: false, ..Default::default() },
        };
        if !now.exists || &now.hash != expected {
            return Ok(WriteOut { written: false, conflict: true, stat: now });
        }
    }
    let abs = match existing {
        Some(abs) => abs,
        None => resolve_new(&root, &args.path)?,
    };
    let mut text = args.text.replace("\r\n", "\n");
    if args.eol == "crlf" {
        text = text.replace('\n', "\r\n");
    }
    let mut bytes = Vec::with_capacity(text.len() + 3);
    if args.bom {
        bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]);
    }
    bytes.extend_from_slice(text.as_bytes());
    atomic_write(&abs, &bytes)?;
    files_changed(ev, &p.id, vec![args.path.clone()], "write");
    Ok(WriteOut { written: true, conflict: false, stat: stat_of(&abs, &args.path) })
}

#[derive(Debug, Deserialize)]
pub struct StatArgs {
    pub id: String,
    pub paths: Vec<String>,
    /// What the caller last saw: unchanged size and time reuse its hash.
    #[serde(default)]
    pub known: Vec<FileStat>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_stat_files(state: State<'_, AppState>, args: StatArgs) -> CommandResult<Vec<FileStat>> {
    stat_files(&state.store, args)
}

pub fn stat_files(store: &StateStore, args: StatArgs) -> CommandResult<Vec<FileStat>> {
    if args.paths.len() > STAT_MAX {
        return Err(CommandError::BadRequest(format!("demasiados archivos (máximo {STAT_MAX})")));
    }
    let (_, root) = root_of(store, &args.id)?;
    let mut out = Vec::with_capacity(args.paths.len());
    for rel in &args.paths {
        let abs = match resolve_existing(&root, rel) {
            Ok(a) => a,
            Err(CommandError::NotFound(_)) => {
                out.push(FileStat { path: rel.clone(), exists: false, ..Default::default() });
                continue;
            }
            Err(e) => return Err(e),
        };
        let quick = std::fs::metadata(&abs).ok().filter(|m| m.is_file());
        let reuse = quick.as_ref().and_then(|m| {
            args.known.iter().find(|k| &k.path == rel && k.exists && k.size == m.len() && k.mtime_ms == mtime_ms(m) && !k.hash.is_empty())
        });
        out.push(match (reuse, quick) {
            (Some(k), _) => k.clone(),
            _ => stat_of(&abs, rel),
        });
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
pub struct CreateFileArgs {
    pub id: String,
    pub path: String,
    #[serde(default)]
    pub text: Option<String>,
}

fn exists_err(rel: &str) -> CommandError {
    CommandError::BadRequest(format!("ya existe «{rel}»"))
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_create_file(app: AppHandle, state: State<'_, AppState>, args: CreateFileArgs) -> CommandResult<FileStat> {
    create_file(&state.store, &app, args)
}

pub fn create_file(store: &StateStore, ev: &dyn ProjectEvents, args: CreateFileArgs) -> CommandResult<FileStat> {
    use std::io::Write;
    let (p, root) = root_of(store, &args.id)?;
    let abs = resolve_new(&root, &args.path)?;
    if std::fs::symlink_metadata(&abs).is_ok() {
        return Err(exists_err(&args.path));
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&abs).map_err(|e| io_err("crear el archivo", e))?;
    f.write_all(args.text.unwrap_or_default().as_bytes()).map_err(|e| io_err("crear el archivo", e))?;
    drop(f);
    files_changed(ev, &p.id, vec![args.path.clone()], "create");
    Ok(stat_of(&abs, &args.path))
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_create_dir(app: AppHandle, state: State<'_, AppState>, args: FileArgs) -> CommandResult<()> {
    create_dir(&state.store, &app, args)
}

pub fn create_dir(store: &StateStore, ev: &dyn ProjectEvents, args: FileArgs) -> CommandResult<()> {
    let (p, root) = root_of(store, &args.id)?;
    let abs = resolve_new(&root, &args.path)?;
    if std::fs::symlink_metadata(&abs).is_ok() {
        return Err(exists_err(&args.path));
    }
    std::fs::create_dir(&abs).map_err(|e| io_err("crear la carpeta", e))?;
    files_changed(ev, &p.id, vec![args.path], "create");
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct RenameArgs {
    pub id: String,
    pub from: String,
    pub to: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_rename(app: AppHandle, state: State<'_, AppState>, args: RenameArgs) -> CommandResult<()> {
    rename(&state.store, &app, args)
}

pub fn rename(store: &StateStore, ev: &dyn ProjectEvents, args: RenameArgs) -> CommandResult<()> {
    let (p, root) = root_of(store, &args.id)?;
    let from = resolve_entry(&root, &args.from)?;
    rel_ok(&args.to)?;
    let to = resolve_new(&root, &args.to)?;
    if std::fs::symlink_metadata(&to).is_ok() {
        // The same entry under another case (macOS, Windows): allowed.
        let same = from.canonicalize().ok().zip(to.canonicalize().ok()).is_some_and(|(a, b)| a == b) && args.from != args.to;
        if !same {
            return Err(exists_err(&args.to));
        }
    }
    if to.starts_with(&from) && to != from {
        return Err(CommandError::BadRequest("no se puede mover una carpeta adentro de sí misma".into()));
    }
    std::fs::rename(&from, &to).map_err(|e| io_err("renombrar", e))?;
    files_changed(ev, &p.id, vec![args.from, args.to], "rename");
    Ok(())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn project_delete(app: AppHandle, state: State<'_, AppState>, args: FileArgs) -> CommandResult<()> {
    delete(&state.store, &app, args)
}

/// Remove an entry: a symlink as a link (never what it points to), a folder
/// with everything in it.
pub fn remove_entry(abs: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(abs)?;
    if meta.file_type().is_symlink() {
        // Windows: a link to a folder is removed as a folder.
        std::fs::remove_file(abs).or_else(|_| std::fs::remove_dir(abs))
    } else if meta.is_dir() {
        std::fs::remove_dir_all(abs)
    } else {
        std::fs::remove_file(abs)
    }
}

pub fn delete(store: &StateStore, ev: &dyn ProjectEvents, args: FileArgs) -> CommandResult<()> {
    let (p, root) = root_of(store, &args.id)?;
    let abs = resolve_entry(&root, &args.path)?;
    remove_entry(&abs).map_err(|e| io_err("eliminar", e))?;
    files_changed(ev, &p.id, vec![args.path], "delete");
    Ok(())
}

// -- unsaved files (quit and close guards) ----------------------------------------------
// File tabs don't autosave: quitting asks about the ones with unsaved
// changes, in every window. Mirrors `windows::TaskRegistry`.

/// Each window's file tabs with unsaved changes, as its UI reports them.
#[derive(Default)]
pub struct UnsavedRegistry(Mutex<std::collections::HashMap<String, Vec<crate::windows::TaskSummary>>>);

impl UnsavedRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, Vec<crate::windows::TaskSummary>>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn report(&self, label: &str, files: Vec<crate::windows::TaskSummary>) {
        let mut m = self.lock();
        if files.is_empty() {
            m.remove(label);
        } else {
            m.insert(label.to_string(), files);
        }
    }

    /// Forget windows that are gone (`open` says which still are).
    pub fn retain(&self, open: impl Fn(&str) -> bool) {
        self.lock().retain(|l, _| open(l));
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Every unsaved file, `main`'s first, then by window label.
    pub fn all(&self) -> Vec<crate::windows::RunningTask> {
        let m = self.lock();
        let mut labels: Vec<&String> = m.keys().collect();
        labels.sort_by_key(|l| (l.as_str() != crate::windows::MAIN, l.len(), l.as_str()));
        labels
            .into_iter()
            .flat_map(|l| m[l].iter().map(move |t| crate::windows::RunningTask { label: l.clone(), id: t.id.clone(), title: t.title.clone() }))
            .collect()
    }
}

/// Some window has file tabs with unsaved changes (for `windows::quit_needs_ui`).
pub fn has_unsaved(app: &AppHandle) -> bool {
    app.try_state::<UnsavedRegistry>().is_some_and(|r| {
        r.retain(|l| app.get_webview_window(l).is_some());
        !r.is_empty()
    })
}

#[derive(Debug, Deserialize)]
pub struct FilesReportArgs {
    pub files: Vec<crate::windows::TaskSummary>,
}

/// The calling window's unsaved file tabs (the whole list, each time it changes).
#[tauri::command(rename_all = "camelCase")]
pub fn files_report(app: AppHandle, window: tauri::WebviewWindow, registry: State<'_, UnsavedRegistry>, args: FilesReportArgs) {
    if app.get_webview_window(window.label()).is_some() {
        registry.report(window.label(), args.files);
    }
}

/// Every window's unsaved file tabs.
#[tauri::command]
pub fn files_unsaved_all(app: AppHandle, registry: State<'_, UnsavedRegistry>) -> Vec<crate::windows::RunningTask> {
    registry.retain(|l| app.get_webview_window(l).is_some());
    registry.all()
}

/// Asks every window to save its file tabs (`files-save-all`).
#[tauri::command]
pub fn files_save_all_broadcast(app: AppHandle) {
    let _ = app.emit("files-save-all", ());
}

// -- tests ------------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The events a command sent.
    #[derive(Default)]
    pub struct Recorder {
        pub files: Mutex<Vec<ProjectFilesChanged>>,
        pub progress: Mutex<Vec<GitProgress>>,
    }

    impl ProjectEvents for Recorder {
        fn files_changed(&self, e: ProjectFilesChanged) {
            self.files.lock().unwrap().push(e);
        }
        fn progress(&self, p: GitProgress) {
            self.progress.lock().unwrap().push(p);
        }
    }

    impl Recorder {
        pub fn reasons(&self) -> Vec<String> {
            self.files.lock().unwrap().drain(..).map(|e| e.reason).collect()
        }
    }

    pub fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dbine-proj-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    #[test]
    fn manifest_is_read_leniently_and_without_credentials() {
        let ok = parse_manifest(
            br#"{ "version": 1, "name": "Ventas", "engine": "postgres", "extra": {"x": 1},
                  "environments": [ {"name": "dev"}, {"name": "qa", "password": "p", "host": "h"},
                                    {"name": "prod", "engine": "mysql", "confirm_run": true, "description": "Prod"} ],
                  "default_environment": "dev", "token": "t" }"#,
        );
        assert!(ok.error.is_none(), "{:?}", ok.error);
        let m = ok.manifest.unwrap();
        assert_eq!(m.name.as_deref(), Some("Ventas"));
        assert_eq!(m.environments.len(), 3);
        assert_eq!(m.environments[0].engine.as_deref(), Some("postgres"), "the root's engine");
        assert_eq!(m.environments[2].engine.as_deref(), Some("mysql"), "its own wins");
        assert!(m.environments[2].confirm_run);
        assert_eq!(m.default_environment.as_deref(), Some("dev"));
        assert_eq!(ok.warnings.len(), 3);
        assert!(ok.warnings.iter().any(|w| w.contains("«password»")));
        assert!(ok.warnings.iter().any(|w| w.contains("«token»")));

        let bad = parse_manifest(b"{ not json");
        assert!(bad.manifest.is_none() && bad.error.unwrap().contains("JSON"));
        assert!(parse_manifest(br#"{"environments":[{"name":"a b"}]}"#).error.is_some());
        assert!(parse_manifest(br#"{"environments":[{"name":"Dev"},{"name":"dev"}]}"#).error.unwrap().contains("repetido"));
        assert!(parse_manifest(br#"[1]"#).error.is_some());
        assert!(parse_manifest(&vec![b' '; 70 * 1024]).error.unwrap().contains("64 KB"));
        let unknown_default = parse_manifest(br#"{"environments":[{"name":"dev"}],"default_environment":"x"}"#);
        assert!(unknown_default.manifest.unwrap().default_environment.is_none());
        assert_eq!(unknown_default.warnings.len(), 1);
        let empty = parse_manifest(b"{}").manifest.unwrap();
        assert_eq!((empty.version, empty.environments.len()), (1, 0));
    }

    #[test]
    fn manifest_is_written_compact_keeping_unknown_keys() {
        let m = ProjectManifest {
            version: 1,
            name: Some("Ventas".into()),
            engine: Some("postgres".into()),
            environments: vec![
                ProjectEnvironment { name: "dev".into(), engine: Some("postgres".into()), ..Default::default() },
                ProjectEnvironment { name: "prod".into(), engine: Some("mysql".into()), confirm_run: true, description: "P".into() },
            ],
            default_environment: Some("dev".into()),
        };
        let v = manifest_json(&m, Some(serde_json::json!({"mine": 1, "name": "old"})));
        assert_eq!(v["mine"], 1);
        assert_eq!(v["name"], "Ventas");
        assert_eq!(v["environments"][0], serde_json::json!({"name": "dev"}));
        assert_eq!(v["environments"][1], serde_json::json!({"name": "prod", "engine": "mysql", "confirm_run": true, "description": "P"}));
        let back = parse_manifest(serde_json::to_string(&v).unwrap().as_bytes()).manifest.unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn progress_lines_are_parsed() {
        let p = parse_progress("o", "remote: Counting objects:  45% (45/100)").unwrap();
        assert_eq!((p.phase.as_str(), p.percent), ("Counting objects", Some(45)));
        let p = parse_progress("o", "Receiving objects: 100% (3/3), done.").unwrap();
        assert_eq!(p.percent, Some(100));
        assert_eq!(parse_progress("o", "Cloning into 'x'...").unwrap().percent, None);
        assert!(parse_progress("o", "  ").is_none());
        let rec = Recorder::default();
        let sink = progress_sink(&rec, "o");
        for i in 0..50 {
            sink(&format!("Receiving objects: {i}% ({i}/100)"));
        }
        sink("Receiving objects: 100% (100/100)");
        let n = rec.progress.lock().unwrap().len();
        assert!((2..10).contains(&n), "throttled: {n}");
    }

    #[test]
    fn repo_names_come_from_the_url() {
        assert_eq!(repo_name_of("https://github.com/acme/ventas-sql.git"), "ventas-sql");
        assert_eq!(repo_name_of("git@github.com:acme/ventas.git/"), "ventas");
        assert_eq!(repo_name_of("/srv/git/x"), "x");
        assert_eq!(repo_name_of(""), "repo");
    }

    #[test]
    fn text_files_are_decoded() {
        assert_eq!(decode_text(b"a\r\nb\r\n"), Some(("a\nb\n".into(), false, true)));
        assert_eq!(decode_text(b"\xEF\xBB\xBFa\nb"), Some(("a\nb".into(), true, false)));
        assert_eq!(decode_text(b"a\0b"), None);
        assert_eq!(decode_text(b"\xff\xfe"), None);
    }

    fn project_at(store: &StateStore, dir: &Path) -> String {
        let p = store
            .save_project(&Project { id: uuid::Uuid::new_v4().to_string(), name: "t".into(), path: path_string(dir), ..Default::default() })
            .unwrap();
        p.id
    }

    #[test]
    fn files_are_read_written_renamed_and_deleted_inside_the_root() {
        let store = StateStore::open_in_memory().unwrap();
        let ev = Recorder::default();
        let root = temp_dir("fs");
        let id = project_at(&store, &root);
        let fa = |path: &str| FileArgs { id: id.clone(), path: path.into() };

        // Create, read, write keeping CRLF and the BOM.
        let st = create_file(&store, &ev, CreateFileArgs { id: id.clone(), path: "sql/ventas.sql".into(), text: Some("x".into()) }).unwrap();
        assert!(st.exists && st.size == 1);
        assert!(create_file(&store, &ev, CreateFileArgs { id: id.clone(), path: "sql/ventas.sql".into(), text: None }).is_err());
        std::fs::write(root.join("sql/crlf.sql"), b"\xEF\xBB\xBFa\r\nb\r\n").unwrap();
        let c = read_file(&store, fa("sql/crlf.sql")).unwrap();
        assert_eq!((c.text.as_str(), c.eol.as_str(), c.bom), ("a\nb\n", "crlf", true));
        let w = write_file(&store, &ev, WriteArgs { id: id.clone(), path: "sql/crlf.sql".into(), text: "a\nb\nc\n".into(), eol: c.eol.clone(), bom: c.bom, expected_hash: Some(c.hash.clone()) }).unwrap();
        assert!(w.written && !w.conflict);
        assert_eq!(std::fs::read(root.join("sql/crlf.sql")).unwrap(), b"\xEF\xBB\xBFa\r\nb\r\nc\r\n");
        // The old hash no longer matches: nothing is written.
        let stale = write_file(&store, &ev, WriteArgs { id: id.clone(), path: "sql/crlf.sql".into(), text: "zzz".into(), eol: "lf".into(), bom: false, expected_hash: Some(c.hash) }).unwrap();
        assert!(!stale.written && stale.conflict && stale.stat.hash == w.stat.hash);
        assert_eq!(std::fs::read(root.join("sql/crlf.sql")).unwrap(), b"\xEF\xBB\xBFa\r\nb\r\nc\r\n");
        // No temp files left behind.
        assert_eq!(std::fs::read_dir(root.join("sql")).unwrap().count(), 2);

        // Binary and large files aren't opened.
        std::fs::write(root.join("bin.dat"), b"a\0b").unwrap();
        assert!(read_file(&store, fa("bin.dat")).unwrap_err().to_string().contains("UTF-8"));
        std::fs::write(root.join("big.sql"), vec![b'a'; 6 * 1024 * 1024]).unwrap();
        assert!(read_file(&store, fa("big.sql")).unwrap_err().to_string().contains("demasiado grande"));
        std::fs::remove_file(root.join("big.sql")).unwrap();

        // Listing: folders first, case-insensitive, .git skipped.
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("B.sql"), "").unwrap();
        std::fs::write(root.join("a.sql"), "").unwrap();
        let names: Vec<String> = tauri::async_runtime::block_on(list_dir(&store, ListDirArgs { id: id.clone(), dir: String::new() })).unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["sql", "a.sql", "B.sql", "bin.dat"]);
        let sub = tauri::async_runtime::block_on(list_dir(&store, ListDirArgs { id: id.clone(), dir: "sql".into() })).unwrap();
        assert_eq!(sub[0].path, "sql/crlf.sql");

        // Stat: missing files, and the hash reused when nothing changed.
        let stats = stat_files(&store, StatArgs { id: id.clone(), paths: vec!["sql/ventas.sql".into(), "nope.sql".into()], known: vec![] }).unwrap();
        assert!(stats[0].exists && !stats[1].exists);
        let mut known = stats[0].clone();
        known.hash = "cached".into();
        assert_eq!(stat_files(&store, StatArgs { id: id.clone(), paths: vec!["sql/ventas.sql".into()], known: vec![known] }).unwrap()[0].hash, "cached");
        assert!(stat_files(&store, StatArgs { id: id.clone(), paths: vec!["x".into(); 201], known: vec![] }).is_err());

        // Rename (into a new folder too), case-only rename, delete.
        rename(&store, &ev, RenameArgs { id: id.clone(), from: "sql/ventas.sql".into(), to: "otros/v.sql".into() }).unwrap();
        assert!(root.join("otros/v.sql").is_file() && !root.join("sql/ventas.sql").exists());
        assert!(rename(&store, &ev, RenameArgs { id: id.clone(), from: "a.sql".into(), to: "B.sql".into() }).is_err(), "target exists");
        rename(&store, &ev, RenameArgs { id: id.clone(), from: "a.sql".into(), to: "A.sql".into() }).unwrap();
        assert!(std::fs::read_dir(&root).unwrap().any(|e| e.unwrap().file_name() == "A.sql"));
        create_dir(&store, &ev, fa("vacía")).unwrap();
        delete(&store, &ev, fa("vacía")).unwrap();
        delete(&store, &ev, fa("otros")).unwrap();
        assert!(!root.join("otros").exists());
        assert_eq!(ev.reasons(), ["create", "write", "rename", "rename", "create", "delete", "delete"]);

        // Every command refuses paths out of the root or into .git.
        for bad in ["../x", "/etc/passwd", ".git/config", "a/../../x", ""] {
            assert!(read_file(&store, fa(bad)).is_err(), "read {bad}");
            assert!(write_file(&store, &ev, WriteArgs { id: id.clone(), path: bad.into(), text: "x".into(), eol: "lf".into(), bom: false, expected_hash: None }).is_err(), "write {bad}");
            assert!(create_file(&store, &ev, CreateFileArgs { id: id.clone(), path: bad.into(), text: None }).is_err(), "create {bad}");
            assert!(create_dir(&store, &ev, fa(bad)).is_err(), "mkdir {bad}");
            assert!(rename(&store, &ev, RenameArgs { id: id.clone(), from: "B.sql".into(), to: bad.into() }).is_err(), "rename to {bad}");
            assert!(rename(&store, &ev, RenameArgs { id: id.clone(), from: bad.into(), to: "z.sql".into() }).is_err(), "rename from {bad}");
            assert!(delete(&store, &ev, fa(bad)).is_err(), "delete {bad}");
            assert!(stat_files(&store, StatArgs { id: id.clone(), paths: vec![bad.into()], known: vec![] }).is_err(), "stat {bad}");
        }
        assert!(tauri::async_runtime::block_on(list_dir(&store, ListDirArgs { id: id.clone(), dir: "../".into() })).is_err());
        assert!(ev.reasons().is_empty());
        assert!(root.join(".git").is_dir() && root.join("B.sql").is_file());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_not_followed_out() {
        let store = StateStore::open_in_memory().unwrap();
        let ev = Recorder::default();
        let root = temp_dir("ln");
        let outside = temp_dir("out");
        std::fs::write(outside.join("secret.sql"), "s").unwrap();
        std::fs::write(root.join("in.sql"), "i").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.sql"), root.join("leak.sql")).unwrap();
        std::os::unix::fs::symlink(root.join("in.sql"), root.join("alias.sql")).unwrap();
        let id = project_at(&store, &root);
        let fa = |path: &str| FileArgs { id: id.clone(), path: path.into() };
        assert!(read_file(&store, fa("leak.sql")).is_err());
        assert!(read_file(&store, fa("out/secret.sql")).is_err());
        assert!(write_file(&store, &ev, WriteArgs { id: id.clone(), path: "leak.sql".into(), text: "x".into(), eol: "lf".into(), bom: false, expected_hash: None }).is_err());
        assert!(create_file(&store, &ev, CreateFileArgs { id: id.clone(), path: "out/new.sql".into(), text: None }).is_err());
        assert_eq!(read_file(&store, fa("alias.sql")).unwrap().text, "i");
        let l = tauri::async_runtime::block_on(list_dir(&store, ListDirArgs { id: id.clone(), dir: String::new() })).unwrap();
        let out = l.iter().find(|e| e.name == "out").unwrap();
        assert!(out.symlink && !out.is_dir);
        // Deleting a link removes the link only.
        delete(&store, &ev, fa("out")).unwrap();
        delete(&store, &ev, fa("leak.sql")).unwrap();
        assert!(outside.join("secret.sql").is_file());
        assert_eq!(std::fs::read_to_string(outside.join("secret.sql")).unwrap(), "s");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn unsaved_files_are_listed_per_window() {
        let t = |id: &str| crate::windows::TaskSummary { id: id.into(), title: format!("p › {id}.sql") };
        let r = UnsavedRegistry::default();
        r.report("win-2", vec![t("b")]);
        r.report("main", vec![t("a")]);
        let all: Vec<(String, String)> = r.all().into_iter().map(|f| (f.label, f.id)).collect();
        assert_eq!(all, [("main".to_string(), "a".to_string()), ("win-2".to_string(), "b".to_string())]);
        r.retain(|l| l == "main");
        assert_eq!(r.all().len(), 1);
        r.report("main", vec![]);
        assert!(r.is_empty());
        let args: FilesReportArgs = serde_json::from_value(serde_json::json!({ "files": [{ "id": "t1", "title": "x" }] })).unwrap();
        assert_eq!(args.files.len(), 1);
    }

    #[test]
    fn bindings_follow_the_manifest() {
        let store = StateStore::open_in_memory().unwrap();
        let root = temp_dir("bind");
        std::fs::write(root.join(MANIFEST_FILE), r#"{"environments":[{"name":"dev"},{"name":"prod"}],"default_environment":"prod"}"#).unwrap();
        let id = project_at(&store, &root);
        let b = |active: Option<&str>| ProjectBinding { active_environment: active.map(Into::into), ..Default::default() };
        assert!(set_binding(&store, BindingArgs { id: id.clone(), binding: b(Some("qa")) }).is_err());
        assert_eq!(set_binding(&store, BindingArgs { id: id.clone(), binding: b(Some("dev")) }).unwrap().project.binding.active_environment.as_deref(), Some("dev"));
        let m = read_manifest(&root).manifest;
        assert_eq!(settle_binding(b(None), m.as_ref()).active_environment.as_deref(), Some("prod"));
        assert_eq!(settle_binding(b(Some("gone")), m.as_ref()).active_environment.as_deref(), Some("prod"));
        assert_eq!(settle_binding(b(Some("x")), None).active_environment, None);

        // Writing the manifest validates and tells the windows.
        let ev = Recorder::default();
        let bad = ProjectManifest { environments: vec![ProjectEnvironment { name: "no válido".into(), ..Default::default() }], ..Default::default() };
        assert!(write_manifest(&store, &ev, ManifestArgs { id: id.clone(), manifest: bad }).is_err());
        let good = ProjectManifest { version: 1, environments: vec![ProjectEnvironment { name: "qa".into(), ..Default::default() }], ..Default::default() };
        let info = write_manifest(&store, &ev, ManifestArgs { id: id.clone(), manifest: good }).unwrap();
        assert_eq!(info.manifest.unwrap().environments[0].name, "qa");
        assert!(std::fs::read_to_string(root.join(MANIFEST_FILE)).unwrap().ends_with("}\n"));
        assert_eq!(ev.files.lock().unwrap()[0].paths, [MANIFEST_FILE]);
        let _ = std::fs::remove_dir_all(&root);
    }
}
