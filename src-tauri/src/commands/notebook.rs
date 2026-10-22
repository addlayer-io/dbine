//! Notebooks (`.ipynb`): parse, serialize, read and write standalone files,
//! export (crate::notebook; docs/notebooks.md). Project files go through
//! projects' read/write with the text these give.

use crate::commands::projects::{atomic_write, sha256_hex};
use crate::error::{CommandError, CommandResult};
use crate::notebook::{self, export, python, Notebook, PreviewLimits};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParseArgs {
    pub text: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_parse(args: ParseArgs) -> CommandResult<Notebook> {
    notebook::parse(&args.text).map_err(CommandError::BadRequest)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializeArgs {
    pub notebook: Notebook,
    #[serde(default)]
    pub rows_per_cell: Option<u32>,
    #[serde(default)]
    pub bytes_per_cell: Option<u64>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_serialize(args: SerializeArgs) -> CommandResult<String> {
    Ok(notebook::serialize(
        &args.notebook,
        PreviewLimits::new(args.rows_per_cell, args.bytes_per_cell),
    ))
}

/// Only `.ipynb` files are read or written here.
fn notebook_path(path: &str) -> CommandResult<&Path> {
    let p = Path::new(path);
    let ok = p.is_absolute()
        && p.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("ipynb"));
    if ok {
        Ok(p)
    } else {
        Err(CommandError::BadRequest(format!(
            "«{path}» no es un notebook (.ipynb)"
        )))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadArgs {
    pub path: String,
}

#[derive(Serialize, Debug)]
pub struct ReadOut {
    pub notebook: Notebook,
    /// sha256 (hex) of the bytes on disk, as projects' files use.
    pub text_hash: String,
}

/// A notebook bigger than this isn't opened (its previews are capped per
/// cell; a file this size isn't one DBine wrote).
const MAX_READ_BYTES: u64 = 64 * 1024 * 1024;

pub fn read(path: &str) -> CommandResult<ReadOut> {
    read_capped(path, MAX_READ_BYTES)
}

fn read_capped(path: &str, max: u64) -> CommandResult<ReadOut> {
    let p = notebook_path(path)?;
    let io = |e: std::io::Error| match e.kind() {
        std::io::ErrorKind::NotFound => {
            CommandError::NotFound(format!("no existe el archivo «{path}»"))
        }
        _ => CommandError::Internal(format!("no se pudo leer «{path}»: {e}")),
    };
    if std::fs::metadata(p).map_err(io)?.len() > max {
        return Err(CommandError::BadRequest(format!(
            "el notebook «{path}» es demasiado grande para abrirlo (más de 64 MB)"
        )));
    }
    let bytes = std::fs::read(p).map_err(io)?;
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| CommandError::BadRequest("el notebook no está en UTF-8".into()))?;
    let notebook = notebook::parse(text).map_err(CommandError::BadRequest)?;
    Ok(ReadOut {
        notebook,
        text_hash: sha256_hex(&bytes),
    })
}

#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_read(args: ReadArgs) -> CommandResult<ReadOut> {
    tauri::async_runtime::spawn_blocking(move || read(&args.path))
        .await
        .map_err(|e| CommandError::Internal(e.to_string()))?
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteArgs {
    pub path: String,
    pub notebook: Notebook,
    #[serde(default)]
    pub rows_per_cell: Option<u32>,
    #[serde(default)]
    pub bytes_per_cell: Option<u64>,
}

pub fn write(args: &WriteArgs) -> CommandResult<()> {
    let p = notebook_path(&args.path)?;
    let text = notebook::serialize(
        &args.notebook,
        PreviewLimits::new(args.rows_per_cell, args.bytes_per_cell),
    );
    atomic_write(p, text.as_bytes())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_write(args: WriteArgs) -> CommandResult<()> {
    tauri::async_runtime::spawn_blocking(move || write(&args))
        .await
        .map_err(|e| CommandError::Internal(e.to_string()))?
}

/// A chart the UI rendered for the export: the cell it belongs to, its title
/// and the PNG (base64).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportImage {
    pub cell_id: String,
    #[serde(default)]
    pub title: String,
    pub png: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportArgs {
    pub notebook: Notebook,
    /// "html" | "markdown" | "python" (a Python .ipynb).
    pub format: String,
    /// The previews' limits, as when saving (Settings › Notebooks).
    #[serde(default)]
    pub rows_per_cell: Option<u32>,
    #[serde(default)]
    pub bytes_per_cell: Option<u64>,
    /// Each cell's charts as images (optional; older callers send none).
    #[serde(default)]
    pub images: Vec<ExportImage>,
    /// Markdown with images: the .md file to write (absolute). The images go
    /// in `<name>_files/` next to it, named by DBine. Without it, the text is
    /// only returned (and images aren't accepted).
    #[serde(default)]
    pub path: Option<String>,
    /// "python": each SQL cell's connection (cell id → its resolved target).
    #[serde(default)]
    pub targets: HashMap<String, ExportTarget>,
    /// "python": each cell's stored preview as its output.
    #[serde(default)]
    pub previews: bool,
}

/// A cell's connection, as the UI resolved it.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportTarget {
    pub connection_id: String,
    #[serde(default)]
    pub database: String,
}

/// The export's view of each cell's connection: its non-secret fields only
/// (notebook::python::PyConn). A connection that no longer exists is left
/// out (its cells are exported commented).
fn python_connections(
    store: &dbine_core::StateStore,
    targets: &HashMap<String, ExportTarget>,
) -> CommandResult<HashMap<String, python::PyConn>> {
    let mut out = HashMap::new();
    for (cell, t) in targets {
        let Some(saved) = store
            .get_connection(&t.connection_id)
            .map_err(|e| CommandError::Internal(e.to_string()))?
        else {
            continue;
        };
        let info = dbine_drivers::find(&saved.config.driver).map(|d| d.info());
        let key = format!("{}\u{0}{}", saved.id, t.database);
        out.insert(
            cell.clone(),
            python::PyConn::new(&key, &saved.name, &saved.config, info, &t.database),
        );
    }
    Ok(out)
}

/// Images per export, at most (with MAX_IMAGES_PER_CELL per cell).
const MAX_EXPORT_IMAGES: usize = 500;

fn decode_images(list: &[ExportImage]) -> CommandResult<export::CellImages> {
    if list.len() > MAX_EXPORT_IMAGES {
        return Err(CommandError::BadRequest(
            "demasiados gráficos para exportar".into(),
        ));
    }
    let mut out = export::CellImages::new();
    for i in list {
        let png = export::decode_png(&i.png).map_err(CommandError::BadRequest)?;
        let title: String = i
            .title
            .chars()
            .filter(|c| !c.is_control())
            .take(200)
            .collect();
        out.entry(i.cell_id.clone())
            .or_default()
            .push(export::ChartImage { title, png });
    }
    Ok(out)
}

/// Only an absolute `.md` / `.markdown` path in an existing folder.
fn markdown_path(path: &str) -> CommandResult<&Path> {
    let p = Path::new(path);
    let ext_ok = p
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown"));
    if !(p.is_absolute() && ext_ok && p.parent().is_some_and(Path::is_dir)) {
        return Err(CommandError::BadRequest(format!(
            "«{path}» no es un archivo Markdown (.md) válido"
        )));
    }
    Ok(p)
}

/// The Markdown and its images written: `<name>_files/` (created, never
/// followed when it's a link or a file) gets each image, then the .md.
fn write_markdown(path: &Path, text: &str, files: &export::ExportFiles) -> CommandResult<()> {
    if !files.is_empty() {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| CommandError::BadRequest("la ruta no es válida".into()))?;
        let dir = path.with_file_name(export::files_dir_name(name));
        match std::fs::symlink_metadata(&dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(CommandError::BadRequest(format!(
                    "«{}» existe y no es una carpeta",
                    dir.display()
                )))
            }
            Err(_) => std::fs::create_dir(&dir).map_err(|e| {
                CommandError::Internal(format!("no se pudo crear «{}»: {e}", dir.display()))
            })?,
        }
        for (file, bytes) in files {
            // Names come from export::markdown_with ("chart-<cell>-<n>.png"); checked anyway.
            if file.contains(['/', '\\']) || file.contains("..") {
                return Err(CommandError::Internal(format!(
                    "nombre de imagen inválido: {file}"
                )));
            }
            let target = dir.join(file);
            if std::fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(CommandError::BadRequest(format!(
                    "«{}» es un enlace: no se sobrescribe",
                    target.display()
                )));
            }
            atomic_write(&target, bytes)?;
        }
    }
    atomic_write(path, text.as_bytes())
}

pub fn export_notebook(args: &ExportArgs, store: &dbine_core::StateStore) -> CommandResult<String> {
    let limits = PreviewLimits::new(args.rows_per_cell, args.bytes_per_cell);
    if args.format == "python" {
        let conns = python_connections(store, &args.targets)?;
        let opts = python::PyExport {
            previews: args.previews,
            limits,
            parameters: python::parameters(&args.notebook),
        };
        return Ok(python::python(&args.notebook, &conns, &opts));
    }
    let images = decode_images(&args.images)?;
    match args.format.as_str() {
        "html" => Ok(export::html_with(&args.notebook, limits, &images)),
        "markdown" | "md" => match args.path.as_deref() {
            Some(path) => {
                let p = markdown_path(path)?;
                let name = p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("notebook.md");
                let (text, files) = export::markdown_with(
                    &args.notebook,
                    limits,
                    &images,
                    &export::files_dir_name(name),
                );
                write_markdown(p, &text, &files)?;
                Ok(text)
            }
            None if images.is_empty() => Ok(export::markdown(&args.notebook, limits)),
            None => Err(CommandError::BadRequest(
                "una exportación Markdown con gráficos necesita la ruta del archivo".into(),
            )),
        },
        f => Err(CommandError::BadRequest(format!(
            "formato de exportación desconocido: «{f}»"
        ))),
    }
}

#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_export(
    state: tauri::State<'_, crate::state::AppState>,
    args: ExportArgs,
) -> CommandResult<String> {
    let store = state.store.clone();
    tauri::async_runtime::spawn_blocking(move || export_notebook(&args, &store))
        .await
        .map_err(|e| CommandError::Internal(e.to_string()))?
}

// -- "DataFrame SQL" (notebook::local) -----------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalRefsArgs {
    pub sql: String,
    /// The SQL cells above, in order (their names, text and whether they
    /// run on the notebook's results).
    pub sources: Vec<notebook::local::LocalSource>,
}

/// The cells above that a "results" cell's SQL needs, in loading order
/// (through the "results" cells it names): the UI resolves and connects
/// their targets before running it.
#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_local_refs(args: LocalRefsArgs) -> CommandResult<Vec<String>> {
    Ok(notebook::local::needed(&args.sql, &args.sources)
        .into_iter()
        .map(|i| args.sources[i].name.clone())
        .collect())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunLocalArgs {
    /// The notebook tab (its DuckDB session is `nbx:<tab>`, closed with it).
    pub tab_id: String,
    pub sql: String,
    pub max_rows: usize,
    /// Rows per loaded table (Settings › Notebooks).
    #[serde(default)]
    pub row_cap: Option<u64>,
    pub sources: Vec<notebook::local::LocalSource>,
    /// The notebook's file (its folder is where relative paths in
    /// `read_csv('…')` are read from); none while it's unsaved.
    #[serde(default)]
    pub notebook_path: Option<String>,
    /// The token of the file approval the user just answered
    /// ([`notebook::access`]): this run may read the files it asked about.
    #[serde(default)]
    pub approval: Option<String>,
    /// The AWS profile for `s3://` (Settings › Notebooks); none: the
    /// environment's or `default`.
    #[serde(default)]
    pub aws_profile: Option<String>,
    /// The notebook's parameters (`{{name}}` in the cell and in the cells
    /// it reads): each value as the engine's literal (DuckDB's here, each
    /// source's own engine for the ones that run again).
    #[serde(default)]
    pub params: Option<crate::commands::query::RunParams>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenderParamsArgs {
    pub sql: String,
    /// The cell's connection; none: a DataFrame SQL cell (DuckDB).
    #[serde(default)]
    pub connection_id: Option<String>,
    pub params: crate::commands::query::RunParams,
}

/// A cell's text with each parameter's value as the engine's literal: what
/// a whole-result chart or a full export runs again (the run itself binds
/// where the driver can, see `execute_query`).
#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_render_params(
    state: tauri::State<'_, crate::state::AppState>,
    args: RenderParamsArgs,
) -> CommandResult<String> {
    use notebook::params::{self, Engine};
    let values = params::resolve(&args.params.definitions, &args.params.values)?;
    let driver = match args.connection_id.as_deref() {
        Some(id) => Some(
            state
                .store
                .get_connection(id)
                .ok()
                .flatten()
                .and_then(|c| dbine_drivers::find(&c.config.driver))
                .ok_or_else(|| CommandError::NotFound("la conexión no existe".into()))?,
        ),
        None => None,
    };
    let engine = match driver {
        Some(d) => Engine::Driver(d.as_ref()),
        None => Engine::DataFrames,
    };
    Ok(params::prepare_text(&args.sql, &engine, &args.params.definitions, &values, None)?.text)
}

/// `args`' texts with the parameters' values in them (see
/// [`RunLocalArgs::params`]).
fn with_params(state: &crate::state::AppState, args: &mut RunLocalArgs) -> CommandResult<()> {
    use notebook::params::{self, Engine};
    let Some(p) = args.params.take() else {
        return Ok(());
    };
    let values = params::resolve(&p.definitions, &p.values)?;
    args.sql = params::prepare_text(
        &args.sql,
        &Engine::DataFrames,
        &p.definitions,
        &values,
        None,
    )?
    .text;
    for s in &mut args.sources {
        let driver = match (&s.local, &s.connection_id) {
            (true, _) => None,
            (false, Some(id)) => state
                .store
                .get_connection(id)
                .ok()
                .flatten()
                .and_then(|c| dbine_drivers::find(&c.config.driver)),
            // Not read by this run: its text stays as it is.
            (false, None) => continue,
        };
        let engine = match driver {
            Some(d) => Engine::Driver(d.as_ref()),
            None if s.local => Engine::DataFrames,
            None => continue,
        };
        s.sql = params::prepare_text(&s.sql, &engine, &p.definitions, &values, Some(&s.name))?.text;
    }
    Ok(())
}

/// DBine's own folders (state, data, cache, logs, downloaded components):
/// no notebook reads files in them.
pub(crate) fn app_dirs(app: &tauri::AppHandle) -> Vec<std::path::PathBuf> {
    use tauri::Manager;
    let p = app.path();
    let mut dirs: Vec<std::path::PathBuf> = [
        p.app_config_dir(),
        p.app_data_dir(),
        p.app_local_data_dir(),
        p.app_cache_dir(),
        p.app_log_dir(),
    ]
    .into_iter()
    .flatten()
    .collect();
    if let Some(d) = dbine_driver::runtime::components_dir().parent() {
        dirs.push(d.to_path_buf());
    }
    dirs
}

/// Run a "DataFrame SQL" cell: DuckDB SQL over the results of the
/// cells above (docs/notebooks.md › DataFrame SQL cells). A cell that reads
/// local files nobody allowed yet runs nothing and returns `approval`: the
/// UI asks, answers with [`notebook_file_decision`] and runs again with the
/// token.
#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_run_local(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::state::AppState>,
    mut args: RunLocalArgs,
) -> CommandResult<notebook::local::RunLocalResponse> {
    with_params(&state, &mut args)?;
    let cap = args.row_cap.unwrap_or(notebook::local::DEFAULT_ROW_CAP);
    let policy = notebook::access::Policy::current(&app_dirs(&app));
    notebook::local::run_local(
        &state,
        &args.tab_id,
        &args.sql,
        args.max_rows,
        cap,
        &args.sources,
        args.notebook_path.as_deref(),
        args.approval.as_deref(),
        policy,
        notebook::remote::Net::current(args.aws_profile.as_deref()),
    )
    .await
}

// -- Write cells (notebook::write) ------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteTargetArgs {
    pub tab_id: String,
    pub target: notebook::write::WriteTarget,
}

/// What the write cell's form needs of a target: the modes its engine
/// writes and, for an existing table, its columns. A read-only connection
/// is refused.
#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_write_target(
    state: tauri::State<'_, crate::state::AppState>,
    args: WriteTargetArgs,
) -> CommandResult<notebook::write::TargetInfo> {
    notebook::write::target_info(&state, &args.tab_id, &args.target).await
}

/// A write cell's plan: its DataFrame loaded, the target and mapping
/// checked, a preview, and the one-time token the confirmation hands to
/// [`notebook_write_run`]. Nothing is written. Like a DataFrame SQL run,
/// files nobody allowed yet return `approval` instead.
#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_write_prepare(
    app: tauri::AppHandle,
    state: tauri::State<'_, crate::state::AppState>,
    args: notebook::write::WriteArgs,
) -> CommandResult<notebook::write::PrepareResponse> {
    let policy = notebook::access::Policy::current(&app_dirs(&app));
    let net = notebook::remote::Net::current(args.aws_profile.as_deref());
    notebook::write::prepare(&state, &args, policy, net).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteRunArgs {
    #[serde(flatten)]
    pub write: notebook::write::WriteArgs,
    /// From the plan the user confirmed.
    pub token: String,
    /// The rows the confirmation showed.
    pub rows: u64,
}

/// The confirmed write: only with the plan's token, bound to this tab,
/// cell, target, mode, keys, mapping and row count. No MCP or assistant
/// tool calls it.
#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_write_run(
    state: tauri::State<'_, crate::state::AppState>,
    args: WriteRunArgs,
) -> CommandResult<notebook::write::WriteOutcome> {
    notebook::write::run(&state, &args.write, &args.token, args.rows).await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileDecisionArgs {
    pub token: String,
    pub decision: notebook::access::Decision,
}

/// The user's answer to "Este notebook quiere leer estos archivos":
/// `once`, `always` (remembered for this notebook on this machine) or `deny`.
#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_file_decision(
    state: tauri::State<'_, crate::state::AppState>,
    args: FileDecisionArgs,
) -> CommandResult<()> {
    notebook::access::decide(&state.store, &args.token, args.decision)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileGrantsArgs {
    /// A notebook's file; none: every notebook's grants.
    #[serde(default)]
    pub notebook: Option<String>,
}

/// The files notebooks may read (Configuración › Notebooks; a notebook's
/// "Permisos de archivos…").
#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_file_grants(
    state: tauri::State<'_, crate::state::AppState>,
    args: FileGrantsArgs,
) -> CommandResult<Vec<dbine_core::file_grants::FileGrant>> {
    match args.notebook.as_deref() {
        None => Ok(state.store.list_file_grants(None)?),
        Some(p) => match notebook::access::notebook_key(Some(p)) {
            Some(nb) => Ok(state.store.list_file_grants(Some(&nb))?),
            None => Ok(Vec::new()),
        },
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileGrantRemoveArgs {
    /// As the list gave it (canonical).
    pub notebook: String,
    pub target: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_file_grant_remove(
    state: tauri::State<'_, crate::state::AppState>,
    args: FileGrantRemoveArgs,
) -> CommandResult<bool> {
    Ok(state
        .store
        .remove_file_grant(&args.notebook, &args.target)?)
}

/// "Quitar todos": a notebook's grants, or every one. How many went.
#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_file_grants_clear(
    state: tauri::State<'_, crate::state::AppState>,
    args: FileGrantsArgs,
) -> CommandResult<usize> {
    let nb = match args.notebook.as_deref() {
        None => None,
        Some(p) => match notebook::access::notebook_key(Some(p)) {
            Some(nb) => Some(nb),
            None => return Ok(0),
        },
    };
    Ok(state.store.clear_file_grants(nb.as_deref())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_read_and_written_atomically_with_their_hash() {
        let dir = std::env::temp_dir().join(format!("dbine-notebook-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.ipynb").to_string_lossy().into_owned();
        let nb = notebook::parse(r#"{"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"cell_type":"markdown","id":"m","metadata":{},"source":"hola"}]}"#).unwrap();
        write(&WriteArgs {
            path: path.clone(),
            notebook: nb,
            rows_per_cell: None,
            bytes_per_cell: None,
        })
        .unwrap();
        let out = read(&path).unwrap();
        assert_eq!(out.notebook.cells[0].source, "hola");
        assert_eq!(out.text_hash, sha256_hex(&std::fs::read(&path).unwrap()));
        assert!(matches!(
            read(&dir.join("b.ipynb").to_string_lossy()),
            Err(CommandError::NotFound(_))
        ));
        assert!(matches!(
            read(&dir.join("x.txt").to_string_lossy()),
            Err(CommandError::BadRequest(_))
        ));
        assert!(matches!(
            read("relative.ipynb"),
            Err(CommandError::BadRequest(_))
        ));
        // Too big: refused before reading it.
        assert!(
            matches!(read_capped(&path, 10), Err(CommandError::BadRequest(m)) if m.contains("64 MB"))
        );
        assert!(read_capped(&path, 1 << 20).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    const PNG_1PX: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

    fn export_args(format: &str, path: Option<String>, images: Vec<ExportImage>) -> ExportArgs {
        let nb = notebook::parse(r#"{"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"cell_type":"code","id":"c1","metadata":{"dbine":{"keep_result":true}},"outputs":[],"source":"SELECT 1"}]}"#).unwrap();
        ExportArgs {
            notebook: nb,
            format: format.into(),
            rows_per_cell: None,
            bytes_per_cell: None,
            images,
            path,
            targets: HashMap::new(),
            previews: false,
        }
    }

    fn export_notebook(args: &ExportArgs) -> CommandResult<String> {
        super::export_notebook(args, &dbine_core::StateStore::open_in_memory().unwrap())
    }

    #[test]
    fn python_export_reads_connections_without_their_secrets() {
        let store = dbine_core::StateStore::open_in_memory().unwrap();
        let mut config = dbine_driver::ConnectionConfig {
            driver: "postgres".into(),
            host: "pg.example.com".into(),
            port: 5432,
            database: "ventas".into(),
            username: Some("app".into()),
            ..Default::default()
        };
        // Whatever the saved config holds: the export keeps only names.
        for (k, v) in [
            ("ssh.enabled", "true"),
            ("ssh.host", "bastion"),
            ("ssh.password", "ZQXSECRET1"),
            ("source_token:password:token", "ZQXSECRET2"),
            (
                "secret_sources",
                r#"{"password":{"kind":"vault","path":"kv/ZQXSECRET3"}}"#,
            ),
        ] {
            config.options.insert(k.into(), v.into());
        }
        let saved = store
            .save_connection(&dbine_core::SavedConnection {
                id: "c-1".into(),
                name: "Ventas PG".into(),
                color: None,
                config,
                save_password: true,
                folder_id: None,
                tags: vec![],
                mcp_level: None,
                updated_at: String::new(),
            })
            .unwrap();
        let mut args = export_args("python", None, vec![]);
        args.notebook.cells[0].name = Some("ventas".into());
        args.targets.insert(
            "c1".into(),
            ExportTarget {
                connection_id: saved.id.clone(),
                database: "ventas".into(),
            },
        );
        args.targets.insert(
            "gone".into(),
            ExportTarget {
                connection_id: "no-such".into(),
                database: String::new(),
            },
        );
        let text = super::export_notebook(&args, &store).unwrap();
        assert!(!text.contains("ZQXSECRET"), "{text}");
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["metadata"]["kernelspec"]["name"], "python3");
        assert!(
            text.contains("DBINE_VENTAS_PG_PASSWORD")
                && text.contains("ventas = pd.read_sql(text("),
            "{text}"
        );
        assert!(text.contains("SSH tunnel") && text.contains("secret manager"));
    }

    #[test]
    fn markdown_with_charts_writes_the_images_next_to_it() {
        let dir = std::env::temp_dir().join(format!("dbine-nb-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let md = dir.join("Mi informe.md");
        let img = |title: &str| ExportImage {
            cell_id: "c1".into(),
            title: title.into(),
            png: PNG_1PX.into(),
        };
        let text = export_notebook(&export_args(
            "markdown",
            Some(md.to_string_lossy().into_owned()),
            vec![img("Ventas"), img("Mapa\u{7}")],
        ))
        .unwrap();
        assert!(
            text.contains("![Ventas](Mi%20informe_files/chart-1-1.png)"),
            "{text}"
        );
        assert!(
            text.contains("![Mapa](Mi%20informe_files/chart-1-2.png)"),
            "control characters dropped: {text}"
        );
        assert_eq!(std::fs::read_to_string(&md).unwrap(), text);
        let png = std::fs::read(dir.join("Mi informe_files").join("chart-1-1.png")).unwrap();
        assert!(png.starts_with(b"\x89PNG"));
        // Again: the folder is reused, the files replaced.
        export_notebook(&export_args(
            "markdown",
            Some(md.to_string_lossy().into_owned()),
            vec![img("Ventas")],
        ))
        .unwrap();
        // HTML embeds them and writes nothing.
        let html = export_notebook(&export_args("html", None, vec![img("Ventas")])).unwrap();
        assert!(html.contains("data:image/png;base64,"));
        // Refused: not a PNG, a relative or non-.md path, images without a path,
        // and a `_files` that's a file.
        let bad = ExportImage {
            cell_id: "c1".into(),
            title: String::new(),
            png: "PHN2Zz4=".into(),
        };
        assert!(matches!(
            export_notebook(&export_args("html", None, vec![bad])),
            Err(CommandError::BadRequest(_))
        ));
        assert!(matches!(
            export_notebook(&export_args(
                "markdown",
                Some("rel.md".into()),
                vec![img("x")]
            )),
            Err(CommandError::BadRequest(_))
        ));
        let txt = dir.join("x.txt").to_string_lossy().into_owned();
        assert!(matches!(
            export_notebook(&export_args("markdown", Some(txt), vec![img("x")])),
            Err(CommandError::BadRequest(_))
        ));
        let missing = dir
            .join("no-such-dir")
            .join("a.md")
            .to_string_lossy()
            .into_owned();
        assert!(matches!(
            export_notebook(&export_args("markdown", Some(missing), vec![img("x")])),
            Err(CommandError::BadRequest(_))
        ));
        assert!(matches!(
            export_notebook(&export_args("markdown", None, vec![img("x")])),
            Err(CommandError::BadRequest(_))
        ));
        std::fs::write(dir.join("b_files"), "x").unwrap();
        let b = dir.join("b.md").to_string_lossy().into_owned();
        assert!(matches!(
            export_notebook(&export_args("markdown", Some(b), vec![img("x")])),
            Err(CommandError::BadRequest(_))
        ));
        // Without images and path: the text only, as before.
        assert!(export_notebook(&export_args("markdown", None, vec![]))
            .unwrap()
            .contains("```sql"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenLinkArgs {
    pub url: String,
}

/// A link's address is opened outside the app only when it's http(s) or
/// mailto, with no spaces or control characters.
fn openable_link(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let scheme_ok = ["http://", "https://", "mailto:"]
        .iter()
        .any(|s| lower.starts_with(s) && lower.len() > s.len());
    scheme_ok && url.len() <= 2048 && !url.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Open a link from a notebook's text (or the assistant's answer) in the
/// system's browser or mail app.
#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_open_link(app: tauri::AppHandle, args: OpenLinkArgs) -> CommandResult<()> {
    use tauri_plugin_opener::OpenerExt;
    if !openable_link(&args.url) {
        return Err(CommandError::BadRequest(
            "solo se abren enlaces http, https o mailto".into(),
        ));
    }
    app.opener()
        .open_url(&args.url, None::<&str>)
        .map_err(|e| CommandError::Internal(format!("no se pudo abrir el enlace: {e}")))
}

#[cfg(test)]
mod link_tests {
    use super::openable_link;

    #[test]
    fn only_web_and_mail_links_open() {
        assert!(openable_link("https://dbine.com/docs"));
        assert!(openable_link("http://example.com"));
        assert!(openable_link("mailto:someone@example.com"));
        assert!(!openable_link("javascript:alert(1)"));
        assert!(!openable_link("file:///etc/passwd"));
        assert!(!openable_link("https://"));
        assert!(!openable_link("https://example.com/a b"));
        assert!(!openable_link("/Applications/Calculator.app"));
    }
}
