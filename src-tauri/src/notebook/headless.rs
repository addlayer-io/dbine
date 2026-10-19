//! A whole notebook run with no window: the "Ejecutar notebook" step of a
//! scheduled task (docs/scheduled-tasks.md › Run a notebook).
//!
//! It runs the cells in order, as **Ejecutar todo** does in the notebook
//! tab (`web/src/views/NotebookView.vue`), and stops at the first one that
//! fails. What it reuses and what it does again:
//!
//! - A connection cell runs on one session per connection and database
//!   (kept for the whole run, as the tab keeps them; the cells that write,
//!   which the task approved, on a writable one of their own and the rest
//!   on a read-only one), through the same
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

/// The parameters the notebook declares (names), for the task editor
/// (`metadata.dbine.parameters`, see [`super::params`]).
pub fn declared_parameters(nb: &Notebook) -> Vec<String> {
    nb.parameters.iter().map(|p| p.name.clone()).collect()
}

/// Each SQL cell ready to run (cell id → prepared cell), with the step's
/// parameter values: missing ones take their defaults; a required one with
/// neither, a value of the wrong type or a parameter used where it can't go
/// fails the run before anything runs ([`super::params::prepare_notebook`]).
/// A value for a parameter the notebook doesn't declare is refused too, so
/// a report never runs with a value silently ignored. `driver_of`: each
/// connection cell's driver (an unresolved cell is left out: the run
/// reports its target).
pub fn executable_cells(
    nb: &Notebook,
    params: &BTreeMap<String, Value>,
    driver_of: &dyn Fn(&NotebookCell) -> Option<Arc<dyn dbine_driver::Driver>>,
) -> CommandResult<HashMap<String, super::params::PreparedCell>> {
    let declared = declared_parameters(nb);
    if let Some(name) = params.keys().find(|k| !declared.iter().any(|d| d.eq_ignore_ascii_case(k))) {
        return Err(CommandError::BadRequest(format!("el notebook no tiene el parámetro «{name}»: quitalo del paso")));
    }
    Ok(super::params::prepare_notebook(nb, params, driver_of)?.into_iter().map(|c| (c.id.clone(), c)).collect())
}

/// Each SQL cell's text as it runs (cell id → text, the values as the
/// engine's literals); see [`executable_cells`].
#[cfg(test)]
pub fn executable_sources(
    nb: &Notebook,
    params: &BTreeMap<String, Value>,
    driver_of: &dyn Fn(&NotebookCell) -> Option<Arc<dyn dbine_driver::Driver>>,
) -> CommandResult<HashMap<String, String>> {
    Ok(executable_cells(nb, params, driver_of)?.into_iter().map(|(id, c)| (id, c.prepared.text)).collect())
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

/// The cell as a write cell, if it is one: the DataFrame it writes, its
/// target as the file names it (connection or environment › database ›
/// table, "(nueva)" when it creates it) and its mode with the keys. Any
/// change in them voids the step's authorization.
pub fn write_cell(c: &NotebookCell) -> Option<WriteCell> {
    if c.kind != CellKind::Write {
        return None;
    }
    let w = c.write.clone().unwrap_or_default();
    let table = [w.schema.clone(), w.table.clone()].into_iter().flatten().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(".");
    let mut target = [stored_text(w.target.as_ref()), table].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" › ");
    if w.create {
        target.push_str(" (nueva)");
    }
    let mode = serde_json::to_value(w.mode).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
    let mode = if w.mode.needs_key() { format!("{mode}: {}", w.keys.join(", ")) } else { mode };
    Some(WriteCell { cell_id: c.id.clone(), name: w.source.clone().unwrap_or_default(), target, mode })
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
    /// A connection cell's driver (none for DataFrame SQL or an unresolved target).
    pub fn driver_of(&self, state: &AppState, cell: &NotebookCell, nb: &Notebook, bindings: &BTreeMap<String, Binding>) -> Option<Arc<dyn dbine_driver::Driver>> {
        match self.resolve(cell, nb, bindings) {
            Ok(Where::Connection(r)) => crate::commands::schema::driver_of(state, &r.connection_id).ok().cloned(),
            _ => None,
        }
    }

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
    out.write_cells.extend(nb.cells.iter().filter_map(write_cell));
    for c in nb.cells.iter().filter(|c| c.kind == CellKind::Sql && !c.source.trim().is_empty()) {
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
    // Unattended: the default cloud credential chain (the AWS profile of
    // Settings › Notebooks lives in the app's settings, not here).
    let access = super::access::Access { policy, notebook, grants, approved: Vec::new(), store: None, net: super::remote::Net::current(None) };
    for f in files {
        match access.gate(std::slice::from_ref(&f)) {
            Ok(_) => out.files.push(f),
            Err(super::access::Gate::Ask(p)) => out.pending.extend(p),
            Err(super::access::Gate::Blocked(m)) => out.blocked.push(m),
        }
    }
    Ok(out)
}

/// The notebook a step runs, read once: a task's run checks the approval
/// on this copy and runs this same copy ([`run_loaded`]), never a second
/// read of the file (which could be another notebook by then).
pub fn load(opts: &Options) -> CommandResult<Notebook> {
    read_notebook(&opts.path)
}

/// The cells of `nb` (the notebook a step runs, see [`load`]) whose SQL
/// writes, on connections that aren't read-only: (cell name, target, first
/// write, what the approval's fingerprint covers). The fingerprint covers
/// the cell's source with its `{{markers}}` and the values of the
/// parameters it uses that aren't credentials: a credential's value never
/// enters it (it would be a digest of the secret kept in the task's JSON).
pub fn writing_cells(state: &AppState, opts: &Options, nb: &Notebook) -> Vec<(String, Resolved, String, String)> {
    let Ok(ctx) = Context::new(state, Path::new(opts.path.trim())) else { return Vec::new() };
    let cells = executable_cells(nb, &opts.params, &|c| ctx.driver_of(state, c, nb, &opts.targets)).unwrap_or_default();
    let mut out = Vec::new();
    for c in nb.cells.iter().filter(|c| c.kind == CellKind::Sql) {
        let prepared = cells.get(&c.id);
        let sql = prepared.map_or_else(|| c.source.clone(), |p| p.prepared.text.clone());
        if sql.trim().is_empty() {
            continue;
        }
        if let Ok(Where::Connection(r)) = ctx.resolve(c, nb, &opts.targets) {
            if r.read_only {
                continue;
            }
            if let Some(what) = first_write(&ctx, &r.connection_id, &sql) {
                // Without parameters the text is the source, as before.
                let covered = match prepared.filter(|p| !p.prepared.used.is_empty()) {
                    None => sql,
                    Some(p) => {
                        // Only true secrets stay out (their values come from the
                        // vault, never the file); every other value, and the
                        // declarations of the parameters used (a default in
                        // the file included), is covered.
                        let values: serde_json::Map<String, Value> = p.values.iter().filter(|(k, _)| !super::params::secret_name(k)).map(|(k, v)| (k.clone(), v.clone())).collect();
                        let used: Vec<Value> = nb
                            .parameters
                            .iter()
                            .filter(|d| p.prepared.used.iter().any(|u| u.eq_ignore_ascii_case(&d.name)))
                            .map(|d| serde_json::to_value(d).unwrap_or(Value::Null))
                            .collect();
                        format!("{}\u{0}{}\u{0}{}", c.source, Value::Object(values), Value::Array(used))
                    }
                };
                out.push((c.name.clone().unwrap_or_default(), r, what, covered));
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
        params: None,
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
    run_loaded(state, key, opts, policy, read_notebook(&opts.path)?).await
}

/// [`run`] on a notebook already read ([`load`]): the copy whose writes
/// the task approved.
pub async fn run_loaded(state: &AppState, key: &str, opts: &Options, policy: Policy, mut nb: Notebook) -> CommandResult<Ran> {
    let path = opts.path.trim().to_string();
    let ctx = Context::new(state, Path::new(&path))?;
    let cells = executable_cells(&nb, &opts.params, &|c| ctx.driver_of(state, c, &nb, &opts.targets))?;
    let sources: HashMap<String, String> = cells.iter().map(|(id, c)| (id.clone(), c.prepared.text.clone())).collect();
    let max_rows = setting_u64(state, "query.maxRows").unwrap_or(5000) as usize;
    let cap = setting_u64(state, "notebook.resultsRowCap").unwrap_or(local::DEFAULT_ROW_CAP);
    let tab = format!("{key}:nb");
    let mut sessions: Sessions = HashMap::new();
    // The connections and databases this run writes on (approved cells).
    let write_pairs: HashSet<(String, String)> = writing_cells(state, opts, &nb).into_iter().map(|(_, r, _, _)| (r.connection_id, r.database)).collect();
    let mut live: HashMap<String, Live> = HashMap::new();
    let mut counter = 0u32;
    let mut run_seq = 0u64;
    let mut ran = 0usize;
    let mut failure: Option<(String, String)> = None;
    let mut reached: HashSet<String> = HashSet::new();

    let result = async {
        for i in 0..nb.cells.len() {
            let cell = nb.cells[i].clone();
            if cell.kind == CellKind::Write {
                // A write cell runs only as the task authorizes it: that
                // cell, that target, that mode (and keys).
                reached.insert(cell.id.clone());
                counter += 1;
                let Some(w) = write_cell(&cell) else { continue };
                let ran_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                let started = std::time::Instant::now();
                let r = if write_authorized(&w, &opts.write_cells) {
                    write_unattended(state, &tab, &ctx, &nb, i, &sources, &live, cap, &path, opts, policy.clone()).await
                } else {
                    Err(format!("la celda escribe en «{}» ({}) y la tarea no la autorizó: abrí la tarea y autorizala en el paso", w.target, w.mode))
                };
                let c = &mut nb.cells[i];
                c.execution_count = Some(counter);
                match r {
                    Ok(o) => {
                        ran += 1;
                        c.write_result = Some(super::WriteSummary {
                            inserted: o.inserted,
                            updated: o.updated,
                            deleted: o.deleted,
                            ran_at,
                            duration_ms: o.elapsed_ms,
                            target_label: Some(o.target_label),
                            error: None,
                            warnings: o.warnings,
                        });
                    }
                    Err(msg) => {
                        c.write_result = Some(super::WriteSummary {
                            ran_at,
                            duration_ms: started.elapsed().as_millis() as u64,
                            target_label: Some(w.target.clone()),
                            error: Some(msg.clone()),
                            ..Default::default()
                        });
                        failure = Some((if w.name.is_empty() { format!("#{}", i + 1) } else { format!("{} → {}", w.name, w.target) }, msg));
                        break;
                    }
                }
                continue;
            }
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
                    // Only the cells the task's approval covers (those that
                    // write, see `writing_cells`) run on a writable session,
                    // their own; every other cell runs on a read-only one
                    // (the guard, and the server where it enforces it). A
                    // cell the guard reads as harmless (a function with side
                    // effects, `USE other`) can then neither change data nor
                    // move the approved writes to another database.
                    let writes = !r.read_only && first_write(&ctx, &r.connection_id, &script).is_some();
                    let pair = (r.connection_id.clone(), r.database.clone());
                    // An engine that keeps one open database per file
                    // (DuckDB) opens it as the first session asks: the
                    // writable one goes first when this run writes there.
                    let entry = match (writes, write_pairs.contains(&pair)) {
                        (false, true) => match session(state, key, &mut sessions, &r, true).await {
                            Ok(_) => session(state, key, &mut sessions, &r, false).await,
                            Err(e) => Err(e),
                        },
                        _ => session(state, key, &mut sessions, &r, writes).await,
                    };
                    let o = match entry {
                        Ok(e) => {
                            let prepared = cells.get(&cell.id).map(|c| &c.prepared).filter(|p| p.text.trim() == script);
                            crate::commands::query::run_on_entry(driver.as_ref(), &e, &script, None, max_rows, prepared).await
                        }
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
            // The values that produced it (those of the parameters it used).
            if let (Some(r), Some(p)) = (c.result.as_mut(), cells.get(&cell.id).filter(|p| !p.values.is_empty())) {
                r.params = Some(p.values.clone());
            }
            // With parameters, each statement as the file keeps it: from the
            // source, with its `{{markers}}` (never the values written in).
            if let (Some(r), Some(_)) = (c.result.as_mut(), cells.get(&cell.id).filter(|p| !p.prepared.used.is_empty())) {
                let driver = source.as_ref().and_then(|(id, _)| crate::commands::schema::driver_of(state, id).ok().cloned());
                marker_statements(r, &outcome, &script, cell.source.trim(), driver.as_deref());
            }
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
    for c in nb.cells.iter_mut().filter(|c| matches!(c.kind, CellKind::Sql | CellKind::Write) && !reached.contains(&c.id)) {
        c.result = None;
        c.write_result = None;
        c.execution_count = None;
    }
    // Defence in depth: what a server echoed of a credential's value (an
    // error, a message) never reaches the copy, its export or the run's log.
    let secrets = credential_values(&opts.params, cells.values().map(|c| &c.values));
    scrub_credentials(&mut nb, &mut failure, &secrets);
    Ok(Ran { notebook: nb, ran, failure })
}

/// The sessions of a run: one per connection, database and whether it
/// writes (key, entry).
type Sessions = HashMap<(String, String, bool), (String, Arc<SessionEntry>)>;

/// The run's session for `r`: writable (`writes`) or read-only, opened on
/// first use and kept for the run.
async fn session(state: &AppState, key: &str, sessions: &mut Sessions, r: &Resolved, writes: bool) -> CommandResult<Arc<SessionEntry>> {
    let id = (r.connection_id.clone(), r.database.clone(), writes);
    if let Some((_, e)) = sessions.get(&id) {
        return Ok(e.clone());
    }
    let skey = format!("{key}:{}", sessions.len());
    let e = state.dedicated_session(&skey, &r.connection_id, &r.database, !writes).await?;
    sessions.insert(id, (skey, e.clone()));
    Ok(e)
}

/// Each result's statement from the cell's `source` (with its markers)
/// instead of `script` (the text that ran, the values as literals): the
/// unit at the same place in the driver's cut of both. The whole source
/// for a single result; no text when the cuts don't match.
fn marker_statements(r: &mut CellResult, o: &QueryOutcome, script: &str, source: &str, driver: Option<&dyn dbine_driver::Driver>) {
    let kept: Vec<&dbine_driver::StatementResult> = o.results.iter().filter(|r| !r.columns.is_empty() || r.rows_affected.is_some()).collect();
    let single = kept.len() == 1 && kept[0].offset.is_none();
    let units = driver.map(|d| (d.split_script(script), d.split_script(source))).filter(|(a, b)| a.len() == b.len());
    for (s, k) in r.results.iter_mut().zip(&kept) {
        let text = if single {
            Some(source.to_string())
        } else {
            units.as_ref().zip(k.offset).and_then(|((ran, src), o)| ran.iter().position(|u| u.start == o).map(|i| src[i].text.clone()))
        };
        s.statement = text.map(|t| t.trim().trim_end_matches(';').trim_end().chars().take(4000).collect::<String>()).filter(|t| !t.is_empty());
    }
}

/// The values of the credential parameters (a password's, a token's) a
/// run had: the step's and each cell's.
fn credential_values<'a>(params: &BTreeMap<String, Value>, cells: impl Iterator<Item = &'a serde_json::Map<String, Value>>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let all = params.iter().chain(cells.flat_map(|m| m.iter()));
    for (_, v) in all.filter(|(k, _)| super::is_credential_param(k)) {
        let text = match v {
            Value::Null => continue,
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if !text.is_empty() && !out.contains(&text) {
            out.push(text);
        }
    }
    // The longest first: a value inside another is replaced after it.
    out.sort_by_key(|s| std::cmp::Reverse(s.len()));
    out
}

/// `***` for each of `secrets` (as given and as a SQL literal writes it,
/// quotes doubled) in the statements, messages and errors of every
/// result, and in the failure.
fn scrub_credentials(nb: &mut Notebook, failure: &mut Option<(String, String)>, secrets: &[String]) {
    if secrets.is_empty() {
        return;
    }
    let scrub = |t: &mut String| {
        for s in secrets {
            // As given, and as the engines' literals spell it: quotes
            // doubled, or backslashes doubled with `\'` or `''` quotes.
            let bs = s.replace('\\', "\\\\");
            for form in [bs.replace('\'', "\\'"), bs.replace('\'', "''"), s.replace('\'', "''"), s.clone()] {
                if t.contains(&form) {
                    *t = t.replace(&form, "***");
                }
            }
        }
    };
    for c in &mut nb.cells {
        if let Some(r) = c.result.as_mut() {
            r.results.iter_mut().filter_map(|s| s.statement.as_mut()).for_each(scrub);
            r.messages.iter_mut().for_each(scrub);
            r.error.iter_mut().for_each(scrub);
        }
        if let Some(w) = c.write_result.as_mut() {
            w.error.iter_mut().for_each(scrub);
            w.warnings.iter_mut().for_each(scrub);
        }
    }
    if let Some((_, msg)) = failure.as_mut() {
        scrub(msg);
    }
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
    let base = frame_sources(ctx, nb, at, sources, live, script, opts)?;
    let r = local::run_local(state, tab, script, max_rows, cap, &base, Some(path), None, policy, super::remote::Net::current(None)).await.map_err(|e| e.to_string())?;
    if let Some(a) = r.approval {
        return Err(unapproved(&a));
    }
    Ok(r.outcome)
}

/// A run that would read files without "Permitir siempre": the message.
fn unapproved(a: &super::access::ApprovalRequest) -> String {
    let list: Vec<String> = a.files.iter().map(|f| f.path.clone()).collect();
    format!(
        "el notebook lee archivos que no tienen «Permitir siempre» para él en esta máquina: {}. Abrilo en DBine, ejecutá la celda y elegí «Permitir siempre»; la tarea no pregunta ni los lee",
        list.join(", ")
    )
}

/// The SQL cells above `at` as DataFrame sources of `script`: the ones it
/// needs resolved (each one's opt-in checked), with the rows of their run
/// here when complete.
fn frame_sources(ctx: &Context, nb: &Notebook, at: usize, sources: &HashMap<String, String>, live: &HashMap<String, Live>, script: &str, opts: &Options) -> Result<Vec<LocalSource>, String> {
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
    Ok(base)
}

/// A write cell the step authorized: its DataFrame (the sources resolved
/// as for a DataFrame SQL cell, files only with "Permitir siempre"), its
/// target resolved by name with the step's choices (a production or
/// `confirm_run` target needs the step's opt-in too), then the same
/// prepare and run as in the app, token included. Nothing asks: what the
/// app would ask about fails the cell.
#[allow(clippy::too_many_arguments)]
async fn write_unattended(
    state: &AppState,
    tab: &str,
    ctx: &Context,
    nb: &Notebook,
    at: usize,
    sources: &HashMap<String, String>,
    live: &HashMap<String, Live>,
    cap: u64,
    path: &str,
    opts: &Options,
    policy: Policy,
) -> Result<super::write::WriteOutcome, String> {
    use super::write::{prepare, run as write_run, WriteArgs, WriteTarget};
    let cell = &nb.cells[at];
    let w = cell.write.clone().unwrap_or_default();
    let source = w.source.clone().filter(|s| !s.is_empty()).ok_or_else(|| "la celda de escritura no dice qué DataFrame escribe".to_string())?;
    let table = w.table.clone().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).ok_or_else(|| "la celda de escritura no tiene tabla de destino".to_string())?;
    let target = w.target.clone().filter(|t| !empty(t) && !t.results).ok_or_else(|| "la celda de escritura no tiene dónde escribir".to_string())?;
    let r = match ctx.named(&target, &opts.targets) {
        Ok(Where::Connection(r)) => r,
        Ok(Where::DataFrames) => return Err("la celda de escritura no tiene dónde escribir".into()),
        Err(p) => return Err(p.message),
    };
    if !r.allowed(opts) {
        return Err(r.refusal());
    }
    let script = format!("SELECT * FROM {}", dbine_driver::sql::quote_ident(dbine_driver::sql::Quote::Double, &source));
    let base = frame_sources(ctx, nb, at, sources, live, &script, opts)?;
    let args = WriteArgs {
        tab_id: tab.to_string(),
        cell_id: cell.id.clone(),
        source,
        sources: base,
        row_cap: Some(cap),
        notebook_path: Some(path.to_string()),
        approval: None,
        aws_profile: None,
        target: WriteTarget { connection_id: r.connection_id.clone(), database: r.database.clone(), schema: w.schema.clone(), table, create: w.create },
        mode: w.mode,
        keys: w.keys.clone(),
        mapping: w.mapping.clone(),
    };
    let prepared = prepare(state, &args, policy, super::remote::Net::current(None)).await.map_err(|e| e.to_string())?;
    if let Some(a) = prepared.approval {
        return Err(unapproved(&a));
    }
    let plan = prepared.plan.ok_or_else(|| "no se pudo preparar la escritura".to_string())?;
    write_run(state, &args, &plan.token, plan.rows).await.map_err(|e| e.to_string())
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
