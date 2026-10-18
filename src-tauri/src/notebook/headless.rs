//! A whole notebook run with no window: the "Ejecutar notebook" step of a
//! scheduled task (docs/scheduled-tasks.md › Run a notebook).
//!
//! It runs the cells in order, as **Ejecutar todo** does in the notebook
//! tab (`web/src/views/NotebookView.vue`), and stops at the first one that
//! fails. What it reuses and what it does again:
//!
//! - A connection cell runs on one session per connection and database
//!   (kept for the whole run, as the tab keeps them), through the same
//!   script runner as the scheduled scripts
//!   ([`crate::commands::query::run_on_entry`]): split as the editor's Auto
//!   mode, the connection's read-only setting kept.
//! - A DataFrame SQL cell goes through [`super::local::run_local`], the
//!   function behind `notebook_run_local`: same lockdown, same reader
//!   sessions, same row cap, same file gate. Its sources are the cells above
//!   with the rows of their run here, else their query runs again read-only,
//!   exactly as in the tab.
//! - What it does itself (the tab does it in TypeScript): resolving each
//!   cell's target ([`resolve`], like `notebookTargets.ts`) and turning a
//!   run into the cell's result ([`outcome_to_result`], like
//!   `outcomeToResult` in `notebookModel.ts`).
//!
//! Nobody is there to answer, so nothing is asked (binding):
//!
//! - **Files**: only those the notebook may always read on this machine
//!   ("Permitir siempre"); any other one fails the cell with the list, and
//!   nothing is read. The always-blocked folders stay blocked.
//! - **Confirmations**: a target whose environment has `confirm_run`, or a
//!   connection tagged as production, runs only when the step carries the
//!   user's opt-in for that connection and database ([`Options::unconfirmed`]);
//!   otherwise the cell fails.
//! - **Writes**: cells whose SQL writes are listed in the task's approval
//!   ([`writing_cells`], part of `tasks::write_scope`), so the task doesn't
//!   run them until the user approves them, and again after they change.
//!   Write cells (`nb-write`) run only when the step authorizes each one
//!   ([`Options::write_cells`]).
//! - **Credentials**: from the connections, as in the app (keychain, secret
//!   managers). A secret-source definition not confirmed on this machine
//!   fails the cell with its usual message.

use super::access::Policy;
use super::files::DataFile;
use super::local::{self, LiveRows, LocalSource};
use super::{CellKind, CellResult, Notebook, NotebookCell, NotebookTarget, StoredColumn, StoredStatement};
use crate::error::{CommandError, CommandResult};
use crate::state::{AppState, SessionEntry};
use dbine_core::state::SavedConnection;
use dbine_driver::QueryOutcome;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The connection a name in the file stands for on this machine, chosen in
/// the task editor (a name no local connection has, or several share).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Binding {
    pub connection_id: String,
    #[serde(default)]
    pub database: String,
}

/// "Permitir ejecutar en <entorno> sin confirmar": a connection and
/// database the user allowed to run unattended although it asks first
/// (an environment with `confirm_run`, or a production connection).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Unconfirmed {
    pub connection_id: String,
    #[serde(default)]
    pub database: String,
}

/// A write cell the task may run unattended: the cell, its target table and
/// its mode, as the task editor showed them. A change in any of them
/// voids it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WriteAuth {
    pub cell_id: String,
    pub target: String,
    pub mode: String,
}

/// What the step says about the run (its config, without the outputs).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Options {
    /// The notebook file (absolute); a project's notebook too.
    #[serde(default)]
    pub path: String,
    /// Parameter values (see [`executable_sources`]).
    #[serde(default)]
    pub params: BTreeMap<String, Value>,
    /// Connection name in the file → this machine's connection.
    #[serde(default)]
    pub targets: BTreeMap<String, Binding>,
    #[serde(default)]
    pub unconfirmed: Vec<Unconfirmed>,
    #[serde(default)]
    pub write_cells: Vec<WriteAuth>,
}

// ---------------------------------------------------------------------------
// Hooks for the notebook features that come in other branches

/// The parameters the notebook declares (names), for the task editor.
/// Notebook parameters (`nb-params`) fill this; until then a notebook
/// declares none.
pub fn declared_parameters(_nb: &Notebook) -> Vec<String> {
    Vec::new()
}

/// Each SQL cell's text as it runs (cell id → text), with the step's
/// parameter values applied. Notebook parameters (`nb-params`) replace the
/// body with their function; until then a value for a parameter the
/// notebook doesn't declare is refused, so a report never runs with a
/// value silently ignored.
pub fn executable_sources(nb: &Notebook, params: &BTreeMap<String, Value>) -> CommandResult<HashMap<String, String>> {
    let declared = declared_parameters(nb);
    if let Some(name) = params.keys().find(|k| !declared.contains(k)) {
        return Err(CommandError::BadRequest(format!("el notebook no tiene el parámetro «{name}»: quitalo del paso")));
    }
    Ok(nb.cells.iter().filter(|c| c.kind == CellKind::Sql).map(|c| (c.id.clone(), c.source.clone())).collect())
}

/// A cell that writes a DataFrame to a table (`nb-write`): its target table
/// and mode, as the task editor shows them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WriteCell {
    pub cell_id: String,
    pub name: String,
    pub target: String,
    pub mode: String,
}

/// The cell as a write cell, if it is one. Write cells (`nb-write`) fill
/// this; until then no cell is.
pub fn write_cell(_c: &NotebookCell) -> Option<WriteCell> {
    None
}

/// Whether the step authorizes this write cell exactly as it is now.
pub fn write_authorized(w: &WriteCell, auth: &[WriteAuth]) -> bool {
    auth.iter().any(|a| a.cell_id == w.cell_id && a.target == w.target && a.mode == w.mode)
}

// ---------------------------------------------------------------------------
// Targets

/// A cell's target on this machine.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Resolved {
    pub connection_id: String,
    pub database: String,
    /// "Ventas prod › ventas" (or "prod · Ventas prod › ventas").
    pub label: String,
    /// The project environment it came from.
    pub environment: Option<String>,
    /// That environment asks before running on it.
    pub confirm_run: bool,
    /// The connection carries a production tag.
    pub production: bool,
    pub read_only: bool,
}

impl Resolved {
    /// It would ask before running in the app (or is production): an
    /// unattended run needs the user's opt-in.
    pub fn needs_opt_in(&self) -> bool {
        self.confirm_run || self.production
    }

    fn allowed(&self, opts: &Options) -> bool {
        !self.needs_opt_in() || opts.unconfirmed.iter().any(|u| u.connection_id == self.connection_id && u.database == self.database)
    }

    fn refusal(&self) -> String {
        if self.confirm_run {
            format!(
                "«{}» pide confirmación antes de ejecutar y una tarea no puede confirmar: abrí la tarea y marcá «Permitir ejecutar en {} sin confirmar» en el paso",
                self.label, self.label
            )
        } else {
            format!(
                "«{}» es de producción y una tarea no puede confirmar: abrí la tarea y marcá «Permitir ejecutar en {} sin confirmar» en el paso",
                self.label, self.label
            )
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    pub id: String,
    pub name: String,
}

/// Why a cell's target isn't a local connection.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Problem {
    /// `none`, `connection` (no local connection by that name),
    /// `ambiguous` (several: `candidates`) or `environment` (not bound here).
    pub kind: &'static str,
    pub name: Option<String>,
    pub candidates: Vec<Candidate>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Where {
    DataFrames,
    Connection(Resolved),
}

/// What resolving needs: the connections, and the notebook's project (its
/// environments and their bindings), if it's in one.
pub struct Context {
    connections: Vec<SavedConnection>,
    project: Option<(dbine_core::state::Project, Option<crate::commands::projects::ProjectManifest>)>,
}

impl Context {
    pub fn new(state: &AppState, notebook: &Path) -> CommandResult<Self> {
        let connections = state.store.list_connections()?;
        let canon = std::fs::canonicalize(notebook).unwrap_or_else(|_| notebook.to_path_buf());
        let project = state.store.list_projects()?.into_iter().find(|p| {
            let root = std::fs::canonicalize(&p.path).unwrap_or_else(|_| PathBuf::from(&p.path));
            canon.starts_with(&root)
        });
        let project = project.map(|p| {
            let manifest = crate::commands::projects::read_manifest(Path::new(&p.path)).manifest;
            (p, manifest)
        });
        Ok(Self { connections, project })
    }

    fn conn(&self, id: &str) -> Option<&SavedConnection> {
        self.connections.iter().find(|c| c.id == id)
    }

    fn resolved(&self, c: &SavedConnection, database: String, environment: Option<(String, bool)>) -> Resolved {
        let base = if database.is_empty() { c.name.clone() } else { format!("{} › {database}", c.name) };
        let (env, confirm_run) = environment.map_or((None, false), |(n, r)| (Some(n), r));
        Resolved {
            connection_id: c.id.clone(),
            database,
            label: env.as_ref().map_or(base.clone(), |e| format!("{e} · {base}")),
            environment: env,
            confirm_run,
            production: c.tags.iter().any(|t| crate::tasks::PROD_TAGS.contains(&t.trim().to_lowercase().as_str())),
            read_only: c.config.read_only,
        }
    }

    fn environment(&self, env: &str) -> Result<Resolved, Problem> {
        let missing = || Problem {
            kind: "environment",
            name: Some(env.to_string()),
            candidates: Vec::new(),
            message: format!("el entorno «{env}» del proyecto no tiene una base asignada en esta máquina: asignala en el proyecto"),
        };
        let Some((p, manifest)) = &self.project else { return Err(missing()) };
        let def = manifest.as_ref().and_then(|m| m.environments.iter().find(|e| e.name.eq_ignore_ascii_case(env)));
        let name = def.map_or(env.to_string(), |d| d.name.clone());
        let bound = p.binding.environments.get(&name).or_else(|| p.binding.environments.get(env)).ok_or_else(missing)?;
        let c = self.conn(&bound.connection_id).ok_or_else(missing)?;
        Ok(self.resolved(c, bound.database.clone(), Some((name, def.is_some_and(|d| d.confirm_run)))))
    }

    /// A project's notebook whose cells name no target: the project's
    /// active environment, or its direct base.
    fn project_default(&self) -> Option<Result<Where, Problem>> {
        let (p, _) = self.project.as_ref()?;
        if let Some(env) = &p.binding.active_environment {
            return Some(self.environment(env).map(Where::Connection));
        }
        let d = p.binding.direct.as_ref()?;
        self.conn(&d.connection_id).map(|c| Ok(Where::Connection(self.resolved(c, d.database.clone(), None))))
    }

    /// A stored target that isn't DataFrame SQL: an environment, or a
    /// connection by name (the step's choice first, else the one local
    /// connection with that name).
    fn named(&self, t: &NotebookTarget, bindings: &BTreeMap<String, Binding>) -> Result<Where, Problem> {
        if t.results {
            return Ok(Where::DataFrames);
        }
        if let Some(env) = t.environment.as_deref().filter(|e| !e.is_empty()) {
            return self.environment(env).map(Where::Connection);
        }
        let name = t.connection.clone().unwrap_or_default();
        let db = |c: &SavedConnection, bound: &str| {
            t.database.clone().filter(|d| !d.is_empty()).or_else(|| (!bound.is_empty()).then(|| bound.to_string())).or_else(|| Some(c.config.database.clone()).filter(|d| !d.is_empty())).unwrap_or_default()
        };
        if let Some(b) = bindings.get(&name) {
            if let Some(c) = self.conn(&b.connection_id) {
                return Ok(Where::Connection(self.resolved(c, db(c, &b.database), None)));
            }
        }
        let same: Vec<&SavedConnection> = self.connections.iter().filter(|c| c.name == name).collect();
        match same.as_slice() {
            [c] => Ok(Where::Connection(self.resolved(c, db(c, ""), None))),
            [] => Err(Problem {
                kind: "connection",
                name: Some(name.clone()),
                candidates: Vec::new(),
                message: format!("no hay ninguna conexión «{name}» en esta máquina: elegí cuál es en el paso de la tarea"),
            }),
            many => Err(Problem {
                kind: "ambiguous",
                name: Some(name.clone()),
                candidates: many.iter().map(|c| Candidate { id: c.id.clone(), name: c.name.clone() }).collect(),
                message: format!("hay varias conexiones «{name}» en esta máquina: elegí cuál es en el paso de la tarea"),
            }),
        }
    }

    /// Where `cell` runs, with the step's choices.
    pub fn resolve(&self, cell: &NotebookCell, nb: &Notebook, bindings: &BTreeMap<String, Binding>) -> Result<Where, Problem> {
        let pick = |t: &Option<NotebookTarget>| t.clone().filter(|t| !empty(t));
        if let Some(t) = pick(&cell.target).or_else(|| pick(&nb.target)) {
            return self.named(&t, bindings);
        }
        match self.project_default() {
            Some(r) => r,
            None => Err(Problem {
                kind: "none",
                name: None,
                candidates: Vec::new(),
                message: "la celda no tiene dónde ejecutarse: elegí la base del notebook en DBine y guardalo".into(),
            }),
        }
    }
}

fn empty(t: &NotebookTarget) -> bool {
    !t.results && t.connection.as_deref().unwrap_or("").is_empty() && t.environment.as_deref().unwrap_or("").is_empty()
}

// ---------------------------------------------------------------------------
// What the task editor shows

#[derive(Debug, Clone, Serialize)]
pub struct PlanCell {
    pub id: String,
    pub name: String,
    pub dataframes: bool,
    /// How the file names its target ("Ventas prod › ventas", "prod").
    pub stored: String,
    pub target: Option<Resolved>,
    pub problem: Option<Problem>,
    /// The first statement that writes (`DELETE`…): the task asks to
    /// approve it.
    pub writes: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Plan {
    pub cells: Vec<PlanCell>,
    /// The distinct targets that need the user's opt-in.
    pub confirm: Vec<Resolved>,
    /// The files the DataFrame SQL cells read, as they are now, granted.
    pub files: Vec<DataFile>,
    /// Those without "Permitir siempre" for this notebook: the run fails.
    pub pending: Vec<DataFile>,
    /// A file of the always-blocked list (the message).
    pub blocked: Vec<String>,
    /// Paths that don't resolve now (a glob with no match, a missing file).
    pub file_problems: Vec<String>,
    pub parameters: Vec<String>,
    pub write_cells: Vec<WriteCell>,
    pub warnings: Vec<String>,
}

fn stored_text(t: Option<&NotebookTarget>) -> String {
    match t {
        None => String::new(),
        Some(t) if t.results => "DataFrame SQL".into(),
        Some(t) => t
            .environment
            .clone()
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| [t.connection.as_deref(), t.database.as_deref()].iter().flatten().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" › ")),
    }
}

/// The first statement of `sql` that writes on `connection_id` (any
/// script on an engine that isn't SQL).
fn first_write(ctx: &Context, connection_id: &str, sql: &str) -> Option<String> {
    let conn = ctx.conn(connection_id)?;
    let d = dbine_drivers::find(&conn.config.driver)?;
    if d.info().language == dbine_driver::Language::Sql {
        dbine_driver::read_only::first_write_in(sql, &d.script_dialect())
    } else {
        (!sql.trim().is_empty()).then(|| format!("script de {}", d.info().name))
    }
}

fn read_notebook(path: &str) -> CommandResult<Notebook> {
    if path.trim().is_empty() {
        return Err(CommandError::BadRequest("el paso no tiene notebook".into()));
    }
    Ok(crate::commands::notebook::read(path.trim())?.notebook)
}

/// What the task editor shows for a notebook: where each cell runs, what
/// needs an opt-in, the files it reads and whether they're allowed.
pub fn plan(state: &AppState, opts: &Options, policy: Policy) -> CommandResult<Plan> {
    let nb = read_notebook(&opts.path)?;
    let ctx = Context::new(state, Path::new(opts.path.trim()))?;
    let mut out = Plan { parameters: declared_parameters(&nb), warnings: nb.warnings.clone(), ..Default::default() };
    let base = Path::new(opts.path.trim()).parent().map(Path::to_path_buf);
    let home = super::files::home_dir();
    let mut files: Vec<DataFile> = Vec::new();
    for c in nb.cells.iter().filter(|c| c.kind == CellKind::Sql && !c.source.trim().is_empty()) {
        if let Some(w) = write_cell(c) {
            out.write_cells.push(w);
        }
        let stored = stored_text(c.target.as_ref().filter(|t| !empty(t)).or(nb.target.as_ref().filter(|t| !empty(t))));
        let mut cell = PlanCell { id: c.id.clone(), name: c.name.clone().unwrap_or_default(), dataframes: false, stored, target: None, problem: None, writes: None };
        match ctx.resolve(c, &nb, &opts.targets) {
            Ok(Where::DataFrames) => {
                cell.dataframes = true;
                match super::files::plan(&c.source, base.as_deref(), home.as_deref(), local::DEFAULT_ROW_CAP) {
                    Ok(reads) => files.extend(reads.into_iter().flat_map(|r| r.files)),
                    Err(e) => out.file_problems.push(format!("{}: {e}", cell.name)),
                }
            }
            Ok(Where::Connection(r)) => {
                if !r.read_only {
                    cell.writes = first_write(&ctx, &r.connection_id, &c.source);
                }
                if r.needs_opt_in() && !out.confirm.iter().any(|x| x.connection_id == r.connection_id && x.database == r.database) {
                    out.confirm.push(r.clone());
                }
                cell.target = Some(r);
            }
            Err(p) => cell.problem = Some(p),
        }
        out.cells.push(cell);
    }
    let mut seen = HashSet::new();
    files.retain(|f| seen.insert(f.path.clone()));
    let notebook = super::access::notebook_key(Some(opts.path.trim()));
    let grants = match &notebook {
        Some(n) => state.store.list_file_grants(Some(n))?,
        None => Vec::new(),
    };
    let access = super::access::Access { policy, notebook, grants, approved: Vec::new(), store: None };
    for f in files {
        match access.gate(std::slice::from_ref(&f)) {
            Ok(_) => out.files.push(f),
            Err(super::access::Gate::Ask(p)) => out.pending.extend(p),
            Err(super::access::Gate::Blocked(m)) => out.blocked.push(m),
        }
    }
    Ok(out)
}

/// The cells of the notebook a step runs whose SQL writes, on connections
/// that aren't read-only: (cell name, target, first write, SQL). A step
/// whose notebook can't be read has none (the run fails on it anyway).
pub fn writing_cells(state: &AppState, opts: &Options) -> Vec<(String, Resolved, String, String)> {
    let Ok(nb) = read_notebook(&opts.path) else { return Vec::new() };
    let Ok(ctx) = Context::new(state, Path::new(opts.path.trim())) else { return Vec::new() };
    let sources = executable_sources(&nb, &BTreeMap::new()).unwrap_or_default();
    let mut out = Vec::new();
    for c in nb.cells.iter().filter(|c| c.kind == CellKind::Sql) {
        let sql = sources.get(&c.id).cloned().unwrap_or_else(|| c.source.clone());
        if sql.trim().is_empty() {
            continue;
        }
        if let Ok(Where::Connection(r)) = ctx.resolve(c, &nb, &opts.targets) {
            if r.read_only {
                continue;
            }
            if let Some(what) = first_write(&ctx, &r.connection_id, &sql) {
                out.push((c.name.clone().unwrap_or_default(), r, what, sql));
            }
        }
    }
    out
}

/// The connections the step's cells run on (to read their saved passwords
/// up front, with a time limit).
pub fn connections(state: &AppState, opts: &Options) -> Vec<String> {
    let Ok(nb) = read_notebook(&opts.path) else { return Vec::new() };
    let Ok(ctx) = Context::new(state, Path::new(opts.path.trim())) else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    for c in nb.cells.iter().filter(|c| c.kind == CellKind::Sql && !c.source.trim().is_empty()) {
        if let Ok(Where::Connection(r)) = ctx.resolve(c, &nb, &opts.targets) {
            if !out.contains(&r.connection_id) {
                out.push(r.connection_id);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Results

fn statement_texts(results: &[&dbine_driver::StatementResult], script: &str) -> Vec<Option<String>> {
    if results.len() == 1 && results[0].offset.is_none() {
        return vec![Some(script.to_string())];
    }
    results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let start = r.offset?;
            let next = results[i + 1..].iter().filter_map(|x| x.offset).find(|&o| o > start).unwrap_or(script.len());
            let text = script.get(start..next.min(script.len()))?;
            Some(text.trim_end().trim_end_matches(';').to_string())
        })
        .collect()
}

/// What a run leaves on the cell, as the tab's `outcomeToResult` does
/// (the preview is cut when the file is written).
pub fn outcome_to_result(o: &QueryOutcome, script: &str, ran_at: &str, engine: Option<String>, target_label: Option<String>) -> CellResult {
    let messages: Vec<String> = if o.log.is_empty() {
        o.messages.clone()
    } else {
        o.log.iter().filter(|m| m.level != dbine_driver::MessageLevel::Error).map(|m| m.text.clone()).collect()
    }
    .into_iter()
    .map(|m| m.trim().to_string())
    .filter(|m| !m.is_empty())
    .collect();
    let errors: Vec<String> = if o.errors.is_empty() { o.error.iter().cloned().collect() } else { o.errors.iter().map(|e| e.message.clone()).collect() };
    let kept: Vec<&dbine_driver::StatementResult> = o.results.iter().filter(|r| !r.columns.is_empty() || r.rows_affected.is_some()).collect();
    let texts = statement_texts(&kept, script);
    let results = kept
        .iter()
        .zip(texts)
        .map(|(r, text)| StoredStatement {
            columns: r.columns.iter().map(|c| StoredColumn { name: c.name.clone(), type_name: c.type_name.clone() }).collect(),
            rows: r.rows.clone(),
            rows_total: r.total_rows.max(r.rows.len() as u64),
            rows_kept: r.rows.len() as u64,
            truncated: r.truncated || r.total_rows > r.rows.len() as u64,
            rows_affected: r.rows_affected,
            statement: text.map(|t| t.trim().chars().take(4000).collect::<String>()).filter(|t| !t.is_empty()),
        })
        .collect();
    CellResult {
        results,
        messages,
        error: (!errors.is_empty()).then(|| errors.join("\n")),
        ran_at: ran_at.to_string(),
        duration_ms: o.elapsed_ms,
        engine,
        target_label,
        truncated: false,
    }
}

/// A run's first result set when nothing cut it (the tab's `completeRows`).
fn complete_rows(o: &QueryOutcome) -> Option<LiveRows> {
    if o.error.is_some() || !o.errors.is_empty() {
        return None;
    }
    let r = o.results.iter().find(|r| !r.columns.is_empty())?;
    (!r.truncated && r.total_rows <= r.rows.len() as u64).then(|| LiveRows { columns: r.columns.clone(), rows: r.rows.clone() })
}

fn failed(o: &QueryOutcome) -> bool {
    o.error.is_some() || !o.errors.is_empty()
}

// ---------------------------------------------------------------------------
// The run

/// How the run went.
#[derive(Debug)]
pub struct Ran {
    /// The notebook with this run's results (cells not reached: none).
    pub notebook: Notebook,
    /// SQL cells that ran.
    pub ran: usize,
    /// The cell that failed and why (the run stopped there).
    pub failure: Option<(String, String)>,
}

struct Live {
    sql: String,
    connection_id: String,
    database: String,
    run: u64,
    outcome: QueryOutcome,
}

/// Settings › Notebooks and the editor's row limit, as the tab reads them.
fn setting_u64(state: &AppState, key: &str) -> Option<u64> {
    state.store.get_setting(key).ok().flatten().and_then(|v| v.as_f64()).filter(|n| *n >= 1.0).map(|n| n as u64)
}

pub fn preview_limits(state: &AppState) -> super::PreviewLimits {
    super::PreviewLimits::new(setting_u64(state, "notebook.rowsPerCell").map(|n| n as u32), setting_u64(state, "notebook.bytesPerCell"))
}

/// Run every SQL cell of the notebook in order; stop at the first that
/// fails. `key`: the run's session prefix (`cancel_query` takes its
/// sessions). Sessions opened here are closed at the end.
pub async fn run(state: &AppState, key: &str, opts: &Options, policy: Policy) -> CommandResult<Ran> {
    let path = opts.path.trim().to_string();
    let mut nb = read_notebook(&path)?;
    let sources = executable_sources(&nb, &opts.params)?;
    let ctx = Context::new(state, Path::new(&path))?;
    let max_rows = setting_u64(state, "query.maxRows").unwrap_or(5000) as usize;
    let cap = setting_u64(state, "notebook.resultsRowCap").unwrap_or(local::DEFAULT_ROW_CAP);
    let tab = format!("{key}:nb");
    let mut sessions: HashMap<(String, String), (String, Arc<SessionEntry>)> = HashMap::new();
    let mut live: HashMap<String, Live> = HashMap::new();
    let mut counter = 0u32;
    let mut run_seq = 0u64;
    let mut ran = 0usize;
    let mut failure: Option<(String, String)> = None;
    let mut reached: HashSet<String> = HashSet::new();

    let result = async {
        for i in 0..nb.cells.len() {
            let cell = nb.cells[i].clone();
            if cell.kind != CellKind::Sql {
                continue;
            }
            let script = sources.get(&cell.id).cloned().unwrap_or_else(|| cell.source.clone()).trim().to_string();
            if script.is_empty() {
                continue;
            }
            reached.insert(cell.id.clone());
            let name = cell.name.clone().unwrap_or_else(|| format!("#{}", i + 1));
            let ran_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            let started = std::time::Instant::now();
            counter += 1;
            run_seq += 1;
            let error_result = |msg: String, engine: Option<String>, label: Option<String>| CellResult {
                error: Some(msg),
                ran_at: ran_at.clone(),
                duration_ms: started.elapsed().as_millis() as u64,
                engine,
                target_label: label,
                ..Default::default()
            };
            // A write cell runs only as the task authorizes it.
            if let Some(w) = write_cell(&cell) {
                let msg = if write_authorized(&w, &opts.write_cells) {
                    "esta versión de DBine no ejecuta celdas de escritura en tareas".to_string()
                } else {
                    format!("la celda escribe en «{}» ({}) y la tarea no la autorizó: abrí la tarea y autorizala en el paso", w.target, w.mode)
                };
                let c = &mut nb.cells[i];
                c.execution_count = Some(counter);
                c.result = Some(error_result(msg.clone(), None, None));
                failure = Some((name, msg));
                break;
            }
            let target = match ctx.resolve(&cell, &nb, &opts.targets) {
                Ok(w) => w,
                Err(p) => {
                    let c = &mut nb.cells[i];
                    c.execution_count = Some(counter);
                    c.result = Some(error_result(p.message.clone(), None, None));
                    failure = Some((name, p.message));
                    break;
                }
            };
            let (outcome, engine, label, source) = match target {
                Where::Connection(r) => {
                    if !r.allowed(opts) {
                        let msg = r.refusal();
                        let c = &mut nb.cells[i];
                        c.execution_count = Some(counter);
                        c.result = Some(error_result(msg.clone(), None, Some(r.label.clone())));
                        failure = Some((name, msg));
                        break;
                    }
                    let driver = crate::commands::schema::driver_of(state, &r.connection_id)?;
                    let pair = (r.connection_id.clone(), r.database.clone());
                    let entry = match sessions.get(&pair) {
                        Some((_, e)) => Ok(e.clone()),
                        None => {
                            let skey = format!("{key}:{}", sessions.len());
                            match state.dedicated_session(&skey, &r.connection_id, &r.database, false).await {
                                Ok(e) => {
                                    sessions.insert(pair.clone(), (skey, e.clone()));
                                    Ok(e)
                                }
                                Err(e) => Err(e),
                            }
                        }
                    };
                    let o = match entry {
                        Ok(e) => crate::commands::query::run_on_entry(driver.as_ref(), &e, &script, None, max_rows).await,
                        Err(e) => QueryOutcome { error: Some(e.to_string()), elapsed_ms: started.elapsed().as_millis() as u64, ..Default::default() },
                    };
                    (o, Some(driver.info().id.to_string()), Some(r.label.clone()), Some((r.connection_id, r.database)))
                }
                Where::DataFrames => {
                    let o = match dataframes(state, &tab, &ctx, &nb, i, &sources, &live, &script, max_rows, cap, &path, opts, policy.clone()).await {
                        Ok(o) => o,
                        Err(msg) => QueryOutcome { error: Some(msg), elapsed_ms: started.elapsed().as_millis() as u64, ..Default::default() },
                    };
                    (o, Some("duckdb".to_string()), Some("DataFrame SQL".to_string()), None)
                }
            };
            ran += 1;
            let c = &mut nb.cells[i];
            c.execution_count = Some(counter);
            c.result = Some(outcome_to_result(&outcome, &script, &ran_at, engine, label));
            if failed(&outcome) {
                let msg = c.result.as_ref().and_then(|r| r.error.clone()).unwrap_or_default();
                failure = Some((name, msg));
                break;
            }
            if let Some((connection_id, database)) = source {
                live.insert(cell.id.clone(), Live { sql: script.clone(), connection_id, database, run: run_seq, outcome });
            } else {
                live.insert(cell.id.clone(), Live { sql: script.clone(), connection_id: String::new(), database: String::new(), run: run_seq, outcome: QueryOutcome::default() });
            }
        }
        Ok::<(), CommandError>(())
    }
    .await;

    for (skey, _) in sessions.values() {
        state.sessions.remove(skey);
    }
    state.sessions.remove(&local::session_key(&tab));
    result?;
    // Cells this run didn't reach keep no result: an old one would read as
    // this run's.
    for c in nb.cells.iter_mut().filter(|c| c.kind == CellKind::Sql && !reached.contains(&c.id)) {
        c.result = None;
        c.execution_count = None;
    }
    Ok(Ran { notebook: nb, ran, failure })
}

/// A DataFrame SQL cell: its sources resolved (each one's opt-in checked),
/// with the rows of their run here when complete; then `run_local`, which
/// reads only files this notebook may always read (a pending one fails it).
#[allow(clippy::too_many_arguments)]
async fn dataframes(
    state: &AppState,
    tab: &str,
    ctx: &Context,
    nb: &Notebook,
    at: usize,
    sources: &HashMap<String, String>,
    live: &HashMap<String, Live>,
    script: &str,
    max_rows: usize,
    cap: u64,
    path: &str,
    opts: &Options,
    policy: Policy,
) -> Result<QueryOutcome, String> {
    let above: Vec<&NotebookCell> = nb.cells[..at]
        .iter()
        .filter(|c| c.kind == CellKind::Sql && c.name.is_some() && !sources.get(&c.id).map_or(c.source.as_str(), String::as_str).trim().is_empty())
        .collect();
    let mut base: Vec<LocalSource> = Vec::new();
    for c in &above {
        let local = matches!(ctx.resolve(c, nb, &opts.targets), Ok(Where::DataFrames));
        let sql = sources.get(&c.id).cloned().unwrap_or_else(|| c.source.clone()).trim().to_string();
        base.push(LocalSource { name: c.name.clone().unwrap_or_default(), local, sql, connection_id: None, database: None, run: None, rows: None, files_key: String::new() });
    }
    let needed: HashSet<String> = local::needed(script, &base).into_iter().map(|i| base[i].name.to_lowercase()).collect();
    for (k, c) in above.iter().enumerate() {
        let s = &mut base[k];
        if s.local || !needed.contains(&s.name.to_lowercase()) {
            continue;
        }
        let r = match ctx.resolve(c, nb, &opts.targets) {
            Ok(Where::Connection(r)) => r,
            Ok(Where::DataFrames) => continue,
            Err(p) => return Err(format!("«{}»: {}", s.name, p.message)),
        };
        if !r.allowed(opts) {
            return Err(format!("«{}»: {}", s.name, r.refusal()));
        }
        if let Some(l) = live.get(&c.id).filter(|l| l.sql == s.sql && l.connection_id == r.connection_id && l.database == r.database) {
            if let Some(rows) = complete_rows(&l.outcome) {
                s.rows = Some(rows);
                s.run = Some(l.run);
            }
        }
        s.connection_id = Some(r.connection_id);
        s.database = Some(r.database);
    }
    let r = local::run_local(state, tab, script, max_rows, cap, &base, Some(path), None, policy).await.map_err(|e| e.to_string())?;
    if let Some(a) = r.approval {
        let list: Vec<String> = a.files.iter().map(|f| f.path.clone()).collect();
        return Err(format!(
            "el notebook lee archivos que no tienen «Permitir siempre» para él en esta máquina: {}. Abrilo en DBine, ejecutá la celda y elegí «Permitir siempre»; la tarea no pregunta ni los lee",
            list.join(", ")
        ));
    }
    Ok(r.outcome)
}

/// DBine's own folders, when there's no app handle (the OS scheduler's
/// windowless run): no notebook reads files in them.
pub fn own_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let ids = [crate::identifier::NEW_ID, crate::identifier::OLD_ID];
    if let Some((c, d)) = crate::identifier::state_dirs_now() {
        dirs.extend([c, d]);
    }
    for id in ids {
        if let Some((c, d)) = crate::identifier::state_dirs(id) {
            dirs.extend([c, d]);
        }
        if let Some(h) = super::files::home_dir() {
            dirs.extend([h.join("Library/Caches").join(id), h.join("Library/Logs").join(id), h.join(".cache").join(id), h.join(".local/share").join(id)]);
        }
        for var in ["LOCALAPPDATA", "APPDATA"] {
            if let Some(base) = std::env::var_os(var).map(PathBuf::from).filter(|p| p.is_absolute()) {
                dirs.push(base.join(id));
            }
        }
    }
    if let Some(d) = dbine_driver::runtime::components_dir().parent() {
        dirs.push(d.to_path_buf());
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

#[cfg(test)]
#[path = "headless_tests.rs"]
mod tests;
