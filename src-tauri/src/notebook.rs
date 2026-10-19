//! Notebooks: Jupyter's `.ipynb` (nbformat 4.5), read and written
//! (docs/notebooks.md).
//!
//! - Read: DBine notebooks (`metadata.dbine`), Azure Data Studio's (SQL
//!   kernel), Jupyter's with `%%sql` cells (jupysql, ipython-sql) and any
//!   other one, whose cells in other languages are kept as they are
//!   (`Foreign`, written back unchanged).
//! - Written: the result of a cell is only a preview, capped per cell (rows
//!   and bytes), stored three ways: `text/html`, Table Schema
//!   (`application/vnd.dataresource+json`) and `text/plain`.
//! - Credentials never come in or go out: DBine's own metadata keeps only the
//!   fields it knows, targets are names, and anything that looks like a
//!   credential or a connection's details is dropped, with a warning.

pub mod access;
pub mod export;
pub mod files;
pub mod headless;
pub mod local;
pub mod params;
pub mod python;
pub mod remote;
pub mod write;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::HashSet;

/// Defaults for the preview limits (Settings › Notebooks).
pub const DEFAULT_ROWS: u32 = 100;
pub const DEFAULT_BYTES: u64 = 1_000_000;
/// A value longer than this (chars) is cut in the stored preview.
const MAX_VALUE_CHARS: usize = 4000;
/// `text/plain`, the last resort: rows and column width it shows.
const TEXT_ROWS: usize = 50;
const TEXT_COL_WIDTH: usize = 60;
const DATARESOURCE: &str = "application/vnd.dataresource+json";

/// Where a notebook or a cell runs: names only, never ids or credentials.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct NotebookTarget {
    /// The connection's NAME.
    #[serde(default)]
    pub connection: Option<String>,
    #[serde(default)]
    pub database: Option<String>,
    /// A project environment's name (`.dbine.json`).
    #[serde(default)]
    pub environment: Option<String>,
    /// "DataFrame SQL": the cell runs DuckDB SQL over the results of the
    /// cells above (each one a DataFrame) and local files
    /// (`notebook::local`), on no connection. Written as
    /// `{"dataframes": true}`, with no connection name; the earlier
    /// `{"results": true}` is still read.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub results: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CellKind {
    Markdown,
    Sql,
    /// A cell from elsewhere (Python, R, a raw cell…): read-only, written
    /// back unchanged.
    Foreign,
    /// Writes a DataFrame into a table ([`WriteCell`], `notebook::write`).
    Write,
}

/// A write cell's definition, as the file keeps it: the DataFrame (a cell's
/// name), the target by NAME (like any cell's, never ids, hosts or
/// credentials), the table, the mode, the keys and the mapping. Never rows.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct WriteCell {
    /// The cell whose DataFrame is written.
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub target: Option<NotebookTarget>,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub table: Option<String>,
    /// "Crear tabla nueva".
    #[serde(default)]
    pub create: bool,
    /// `insert`, `update`, `upsert` or `replace`.
    #[serde(default = "insert_mode")]
    pub mode: dbine_driver::WriteMode,
    /// Target columns.
    #[serde(default)]
    pub keys: Vec<String>,
    /// `None`: by name.
    #[serde(default)]
    pub mapping: Option<Vec<write::ColumnPair>>,
}

fn insert_mode() -> dbine_driver::WriteMode {
    dbine_driver::WriteMode::Insert
}

/// A write cell's last run, as the file keeps it: counts, when, how long,
/// where, and the error (cut).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct WriteSummary {
    #[serde(default)]
    pub inserted: u64,
    #[serde(default)]
    pub updated: u64,
    #[serde(default)]
    pub deleted: u64,
    #[serde(default)]
    pub ran_at: String,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub target_label: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// Shown in this session only; not written to the file.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StoredColumn {
    pub name: String,
    #[serde(default)]
    pub type_name: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StoredStatement {
    #[serde(default)]
    pub columns: Vec<StoredColumn>,
    /// Cells: null, bool, number or string.
    #[serde(default)]
    pub rows: Vec<Vec<Value>>,
    /// Rows the query returned (before any cap).
    #[serde(default)]
    pub rows_total: u64,
    /// Rows stored in the file.
    #[serde(default)]
    pub rows_kept: u64,
    /// The preview was cut (row cap, byte cap, or the run itself was capped).
    #[serde(default)]
    pub truncated: bool,
    #[serde(default)]
    pub rows_affected: Option<u64>,
    #[serde(default)]
    pub statement: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct CellResult {
    /// One per statement that returned rows or a count.
    #[serde(default)]
    pub results: Vec<StoredStatement>,
    /// Server messages (PRINT, NOTICE…).
    #[serde(default)]
    pub messages: Vec<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// RFC 3339, UTC.
    #[serde(default)]
    pub ran_at: String,
    #[serde(default)]
    pub duration_ms: u64,
    /// The driver's id.
    #[serde(default)]
    pub engine: Option<String>,
    /// "Ventas prod › ventas", for the summary line.
    #[serde(default)]
    pub target_label: Option<String>,
    /// The stored messages or error were cut to fit the cell's byte cap.
    #[serde(default)]
    pub truncated: bool,
    /// The notebook parameters' values that produced it (the ones its cell
    /// uses), by name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Map<String, Value>>,
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct NotebookCell {
    /// nbformat 4.5's cell id.
    #[serde(default)]
    pub id: String,
    pub kind: CellKind,
    #[serde(default)]
    pub source: String,
    /// Foreign: its language; Sql: "sql" or the engine's language.
    #[serde(default)]
    pub language: Option<String>,
    /// Overrides the notebook's target.
    #[serde(default)]
    pub target: Option<NotebookTarget>,
    /// SQL cells: its name as a table for the cells on the notebook's
    /// results (`metadata.dbine.name`; see `local::valid_name`). Files
    /// without it get one from the UI.
    #[serde(default)]
    pub name: Option<String>,
    /// false: "Don't save result" (the summary is kept, the rows aren't).
    #[serde(default = "yes")]
    pub keep_result: bool,
    /// Chart settings and its aggregated points: opaque here, except that
    /// `points` / `data` are left out when the cell's rows aren't saved.
    #[serde(default)]
    pub chart: Option<Value>,
    #[serde(default)]
    pub execution_count: Option<u32>,
    #[serde(default)]
    pub result: Option<CellResult>,
    /// Foreign: the original nbformat cell, written back unchanged.
    /// Markdown: `{"attachments": …}` when it had any.
    #[serde(default)]
    pub raw: Option<Value>,
    /// Write cells: what they write and where.
    #[serde(default)]
    pub write: Option<WriteCell>,
    /// Write cells: their last run.
    #[serde(default)]
    pub write_result: Option<WriteSummary>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Notebook {
    #[serde(default)]
    pub target: Option<NotebookTarget>,
    /// false: "Save without results".
    #[serde(default = "yes")]
    pub save_results: bool,
    /// The file said `save_results` itself (a foreign notebook doesn't: the
    /// UI then applies the project's default). Not written to the file.
    #[serde(default)]
    pub save_results_set: bool,
    #[serde(default)]
    pub cells: Vec<NotebookCell>,
    /// Values asked for when it runs, used by the cells as `{{name}}`
    /// (`metadata.dbine.parameters`; see [`params`]).
    #[serde(default)]
    pub parameters: Vec<params::NotebookParameter>,
    /// "dbine" | "ads" | "jupyter": how it was read.
    #[serde(default)]
    pub origin: String,
    /// The notebook-level metadata DBine doesn't own, kept on save.
    #[serde(default)]
    pub extra_metadata: Value,
    /// Things ignored on read (credentials dropped…), in Spanish.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// The preview a cell keeps in the file.
#[derive(Clone, Copy, Debug)]
pub struct PreviewLimits {
    /// Rows per cell, shared by its statements (the first ones first).
    pub rows: u32,
    /// The whole cell's outputs.
    pub bytes: u64,
}

impl PreviewLimits {
    pub fn new(rows: Option<u32>, bytes: Option<u64>) -> Self {
        PreviewLimits {
            rows: rows.unwrap_or(DEFAULT_ROWS).clamp(1, 1000),
            bytes: bytes.unwrap_or(DEFAULT_BYTES).clamp(64 * 1024, 8 * 1024 * 1024),
        }
    }
}

impl Default for PreviewLimits {
    fn default() -> Self {
        Self::new(None, None)
    }
}

// ---------------------------------------------------------------------------
// Credentials

/// A key that holds (or points at) a credential or a server's address.
fn is_credential_key(key: &str) -> bool {
    let n: String = key.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect();
    const EXACT: &[&str] = &[
        "user", "username", "userid", "uid", "login", "host", "hostname", "server", "servername", "port", "url",
        "uri", "dsn", "address", "endpoint",
    ];
    const CONTAINS: &[&str] = &[
        "password", "passwd", "pwd", "secret", "token", "apikey", "credential", "connectionstring", "connstr",
        "accesskey", "privatekey", "authentication",
    ];
    EXACT.contains(&n.as_str()) || CONTAINS.iter().any(|c| n.contains(c))
}

/// Metadata DBine doesn't own: credentials and also any connection details
/// (Azure Data Studio's `connection_name`…).
fn is_foreign_sensitive_key(key: &str) -> bool {
    is_credential_key(key) || key.to_ascii_lowercase().contains("connection")
}

/// Drops the keys `bad` matches, in every nested object (and, with
/// `arrays`, inside arrays too). True when something was dropped.
fn scrub(v: &mut Value, bad: fn(&str) -> bool, arrays: bool) -> bool {
    let mut dropped = false;
    match v {
        Value::Object(m) => {
            let before = m.len();
            m.retain(|k, _| !bad(k));
            dropped |= m.len() != before;
            for x in m.values_mut() {
                dropped |= scrub(x, bad, arrays);
            }
        }
        Value::Array(a) if arrays => {
            for x in a {
                dropped |= scrub(x, bad, arrays);
            }
        }
        _ => {}
    }
    dropped
}

#[derive(Default)]
struct Warnings {
    credentials: bool,
    magic_args: usize,
}

impl Warnings {
    fn into_messages(self) -> Vec<String> {
        let mut out = Vec::new();
        if self.credentials {
            out.push(
                "El archivo traía datos de conexión o credenciales: DBine los ignoró y no los guarda.".to_string(),
            );
        }
        if self.magic_args > 0 {
            out.push(if self.magic_args == 1 {
                "Se ignoró la conexión indicada en una celda %%sql: elegí en DBine dónde se ejecuta.".to_string()
            } else {
                format!("Se ignoró la conexión indicada en {} celdas %%sql: elegí en DBine dónde se ejecuta.", self.magic_args)
            });
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Reading

fn source_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

fn str_of<'a>(m: &'a Map<String, Value>, k: &str) -> Option<&'a str> {
    m.get(k).and_then(Value::as_str)
}

/// A target from DBine's metadata: the three names, nothing else.
fn read_target(v: Option<&Value>, w: &mut Warnings) -> Option<NotebookTarget> {
    let m = v?.as_object()?;
    if m.keys().any(|k| is_credential_key(k)) {
        w.credentials = true;
    }
    // DataFrame SQL (`results` is how it was first written): nothing else applies.
    if ["dataframes", "results"].iter().any(|k| m.get(*k).and_then(Value::as_bool) == Some(true)) {
        return Some(NotebookTarget { results: true, ..Default::default() });
    }
    let name = |k: &str| str_of(m, k).map(str::trim).filter(|s| !s.is_empty()).map(String::from);
    let mut connection = name("connection");
    if connection.as_deref().is_some_and(looks_like_connection_string) {
        w.credentials = true;
        connection = None;
    }
    let t = NotebookTarget { connection, database: name("database"), environment: name("environment"), results: false };
    (t != NotebookTarget::default()).then_some(t)
}

/// A value that is a connection string or URL, not a connection's name:
/// `scheme://…`, or `key=value` pairs with `;` or `@`.
fn looks_like_connection_string(v: &str) -> bool {
    v.contains("://") || (v.contains('=') && (v.contains(';') || v.contains('@')))
}

fn flag_credentials(m: &Map<String, Value>, w: &mut Warnings) {
    if m.keys().any(|k| is_credential_key(k)) {
        w.credentials = true;
    }
}

fn valid_id(id: &str) -> bool {
    (1..=64).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn is_sql_kernel(meta: &Map<String, Value>) -> bool {
    let ks = meta.get("kernelspec").and_then(Value::as_object);
    let li = meta.get("language_info").and_then(Value::as_object);
    let sql = |s: Option<&str>| s.is_some_and(|s| s.eq_ignore_ascii_case("sql"));
    sql(ks.and_then(|k| str_of(k, "name"))) || sql(ks.and_then(|k| str_of(k, "language"))) || sql(li.and_then(|l| str_of(l, "name")))
}

fn kernel_language(meta: &Map<String, Value>) -> Option<String> {
    let li = meta.get("language_info").and_then(Value::as_object).and_then(|l| str_of(l, "name"));
    let ks = meta.get("kernelspec").and_then(Value::as_object).and_then(|k| str_of(k, "language"));
    li.or(ks).map(str::to_string)
}

/// `%%sql …` on the first line: the SQL under it, and whether the line had
/// anything after the magic (a connection string, options…), ignored.
fn strip_sql_magic(src: &str) -> Option<(String, bool)> {
    let (first, rest) = src.split_once('\n').unwrap_or((src, ""));
    let after = first.trim_start().strip_prefix("%%sql")?;
    if !after.is_empty() && !after.starts_with(char::is_whitespace) {
        return None;
    }
    Some((rest.to_string(), !after.trim().is_empty()))
}

/// Parses `.ipynb` text. Errors (in Spanish): not JSON, not nbformat 4.
pub fn parse(text: &str) -> Result<Notebook, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let root: Value = serde_json::from_str(text).map_err(|e| format!("el archivo no es un notebook válido: {e}"))?;
    let obj = root.as_object().ok_or_else(|| "el archivo no es un notebook de Jupyter".to_string())?;
    if obj.get("nbformat").and_then(Value::as_u64) != Some(4) {
        return Err("el notebook no usa la versión 4 del formato de Jupyter (nbformat 4)".into());
    }
    let mut w = Warnings::default();
    let mut meta = obj.get("metadata").and_then(Value::as_object).cloned().unwrap_or_default();
    let dbine = meta.remove("dbine");
    let dbine = dbine.as_ref().and_then(Value::as_object);
    let sql_kernel = is_sql_kernel(&meta);
    let kernel_lang = kernel_language(&meta);
    let origin = if dbine.is_some() {
        "dbine"
    } else if sql_kernel {
        "ads"
    } else {
        "jupyter"
    };

    let (target, saved, parameters) = match dbine {
        Some(d) => {
            flag_credentials(d, &mut w);
            let (parameters, dropped) = params::read_parameters(d.get("parameters"));
            if dropped {
                w.credentials = true;
            }
            (read_target(d.get("target"), &mut w), d.get("save_results").and_then(Value::as_bool), parameters)
        }
        None => (None, None, Vec::new()),
    };

    // Ours, rewritten on save.
    meta.remove("kernelspec");
    meta.remove("language_info");
    let mut extra = Value::Object(meta);
    if scrub(&mut extra, is_foreign_sensitive_key, true) {
        w.credentials = true;
    }

    let mut ids = HashSet::new();
    let ctx = Ctx { sql_kernel, kernel_lang };
    let cells = obj
        .get("cells")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter(|c| c.is_object()).map(|c| read_cell(c, &ctx, &mut ids, &mut w)).collect())
        .unwrap_or_default();
    let cells = unique_cell_names(cells);

    Ok(Notebook {
        target,
        save_results: saved.unwrap_or(true),
        save_results_set: saved.is_some(),
        cells,
        parameters,
        origin: origin.into(),
        extra_metadata: extra,
        warnings: w.into_messages(),
    })
}

struct Ctx {
    sql_kernel: bool,
    kernel_lang: Option<String>,
}

fn read_cell(c: &Value, ctx: &Ctx, ids: &mut HashSet<String>, w: &mut Warnings) -> NotebookCell {
    let empty = Map::new();
    let m = c.as_object().unwrap_or(&empty);
    let meta = m.get("metadata").and_then(Value::as_object).unwrap_or(&empty);
    let mut id = [str_of(m, "id"), str_of(meta, "azdata_cell_guid")]
        .into_iter()
        .flatten()
        .find(|i| valid_id(i) && !ids.contains(*i))
        .map(String::from)
        .unwrap_or_else(new_id);
    while !ids.insert(id.clone()) {
        id = new_id();
    }
    let source = source_text(m.get("source"));
    let execution_count = m.get("execution_count").and_then(Value::as_u64).map(|n| n.min(u32::MAX as u64) as u32);
    let mut cell = NotebookCell {
        id,
        kind: CellKind::Foreign,
        source,
        language: None,
        target: None,
        keep_result: true,
        chart: None,
        execution_count,
        result: None,
        raw: None,
        name: None,
        write: None,
        write_result: None,
    };
    let lang = str_of(meta, "language");
    match str_of(m, "cell_type") {
        Some("markdown") => {
            cell.kind = CellKind::Markdown;
            cell.execution_count = None;
            cell.raw = m.get("attachments").map(|a| json!({ "attachments": a }));
        }
        Some("code") => {
            if let Some((sql, had_args)) = strip_sql_magic(&cell.source) {
                // The rest of the magic line (a connection string, maybe
                // with a password) is never kept; its outputs are Python's.
                if had_args {
                    w.magic_args += 1;
                }
                cell.kind = CellKind::Sql;
                cell.source = sql;
                cell.language = Some("sql".into());
            } else if let Some(d) = meta.get("dbine").and_then(Value::as_object).filter(|d| str_of(d, "kind") == Some("write")) {
                flag_credentials(d, w);
                cell.kind = CellKind::Write;
                cell.language = None;
                cell.write = Some(read_write_cell(d.get("write"), w));
                cell.write_result = d.get("write_result").and_then(|r| read_write_summary(r));
            } else if let Some(d) = meta.get("dbine").and_then(Value::as_object) {
                flag_credentials(d, w);
                cell.kind = CellKind::Sql;
                cell.language = Some(lang.unwrap_or("sql").to_string());
                cell.target = read_target(d.get("target"), w);
                cell.name = str_of(d, "name").map(str::trim).filter(|n| local::valid_name(n)).map(String::from);
                cell.keep_result = d.get("keep_result").and_then(Value::as_bool).unwrap_or(true);
                cell.chart = d.get("chart").filter(|c| !c.is_null()).cloned().map(|mut c| {
                    if scrub(&mut c, is_credential_key, false) {
                        w.credentials = true;
                    }
                    c
                });
                let run = d.get("run").and_then(Value::as_object);
                cell.result = read_outputs(m.get("outputs"), run, w);
            } else if ctx.sql_kernel && lang.is_none_or(|l| l.eq_ignore_ascii_case("sql")) {
                // Azure Data Studio: its connection details are never read.
                cell.kind = CellKind::Sql;
                cell.language = Some("sql".into());
                cell.result = read_outputs(m.get("outputs"), None, w);
            } else {
                cell.language = lang.map(String::from).or_else(|| ctx.kernel_lang.clone());
                cell.raw = Some(c.clone());
            }
        }
        Some("raw") => {
            cell.language = Some("raw".into());
            cell.raw = Some(c.clone());
        }
        _ => cell.raw = Some(c.clone()),
    }
    cell
}

/// A write cell's definition from the file: only the fields it knows, the
/// target through [`read_target`] (names only), names as plain strings.
fn read_write_cell(v: Option<&Value>, w: &mut Warnings) -> WriteCell {
    let empty = Map::new();
    let m = v.and_then(Value::as_object).unwrap_or(&empty);
    flag_credentials(m, w);
    let text = |k: &str| str_of(m, k).map(str::trim).filter(|s| !s.is_empty() && s.len() <= 256).map(String::from);
    let names = |v: Option<&Value>| -> Vec<String> {
        v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).take(64).map(String::from).collect()).unwrap_or_default()
    };
    let mapping = m.get("mapping").and_then(Value::as_array).map(|a| {
        a.iter()
            .filter_map(|p| {
                let (s, t) = (p.get("source")?.as_str()?.trim(), p.get("target")?.as_str()?.trim());
                (!s.is_empty() && !t.is_empty()).then(|| write::ColumnPair { source: s.into(), target: t.into() })
            })
            .take(4096)
            .collect()
    });
    WriteCell {
        source: text("source").filter(|n| local::valid_name(n)),
        target: read_target(m.get("target"), w).filter(|t| !t.results),
        schema: text("schema"),
        table: text("table"),
        create: m.get("create").and_then(Value::as_bool).unwrap_or(false),
        // An unknown mode reads as the one that changes no existing row.
        mode: m.get("mode").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or(dbine_driver::WriteMode::Insert),
        keys: names(m.get("keys")),
        mapping,
    }
}

fn read_write_summary(v: &Value) -> Option<WriteSummary> {
    let m = v.as_object()?;
    let n = |k: &str| m.get(k).and_then(Value::as_u64).unwrap_or(0);
    let t = |k: &str| str_of(m, k).map(|s| s.chars().take(2000).collect::<String>());
    Some(WriteSummary {
        inserted: n("inserted"),
        updated: n("updated"),
        deleted: n("deleted"),
        ran_at: t("ran_at").unwrap_or_default(),
        duration_ms: n("duration_ms"),
        target_label: t("target_label"),
        error: t("error"),
        warnings: Vec::new(),
    })
}

/// "120 inserted, 30 updated · {ran_at} · 1.2 s · {target}", or the error.
pub fn write_summary_line(r: &WriteSummary) -> String {
    let mut parts = Vec::new();
    match &r.error {
        Some(e) => parts.push(format!("Write failed: {e}")),
        None => {
            let mut counts = vec![format!("{} inserted", r.inserted), format!("{} updated", r.updated)];
            if r.deleted > 0 {
                counts.push(format!("{} deleted", r.deleted));
            }
            parts.push(counts.join(", "));
        }
    }
    if !r.ran_at.is_empty() {
        parts.push(r.ran_at.clone());
    }
    parts.push(format_duration(r.duration_ms));
    if let Some(t) = r.target_label.as_deref().filter(|t| !t.is_empty()) {
        parts.push(t.to_string());
    }
    parts.join(" · ")
}

/// What a write cell's `source` says to someone reading the file elsewhere.
pub fn write_cell_text(w: &WriteCell) -> String {
    let target = match &w.target {
        Some(t) if t.environment.is_some() => t.environment.clone().unwrap_or_default(),
        Some(t) => [t.connection.clone(), t.database.clone()].into_iter().flatten().collect::<Vec<_>>().join(" › "),
        None => String::new(),
    };
    let table = [w.schema.clone(), w.table.clone()].into_iter().flatten().collect::<Vec<_>>().join(".");
    let mode = match w.mode {
        dbine_driver::WriteMode::Insert => "insert".to_string(),
        dbine_driver::WriteMode::Update => format!("update by {}", w.keys.join(", ")),
        dbine_driver::WriteMode::Upsert => format!("insert or update by {}", w.keys.join(", ")),
        dbine_driver::WriteMode::Replace => "replace the table's rows".to_string(),
    };
    format!(
        "-- DBine write cell (edit it in DBine)\n-- DataFrame: {}\n-- Into: {}{}{}\n-- Mode: {mode}{}\n",
        w.source.as_deref().unwrap_or("?"),
        target,
        if target.is_empty() || table.is_empty() { "" } else { " › " },
        table,
        if w.create { " (creates the table)" } else { "" }
    )
}

/// A cell name used twice (ignoring case, as DuckDB does): the later
/// cells lose it (the UI gives them a new one).
fn unique_cell_names(mut cells: Vec<NotebookCell>) -> Vec<NotebookCell> {
    let mut seen = HashSet::new();
    for c in &mut cells {
        if let Some(n) = &c.name {
            if !seen.insert(n.to_ascii_lowercase()) {
                c.name = None;
            }
        }
    }
    cells
}

/// Text without tags, for Azure Data Studio's messages ("(1 row affected)").
fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").replace("&amp;", "&")
}

fn push_lines(out: &mut Vec<String>, text: &str) {
    out.extend(text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from));
}

fn read_outputs(outputs: Option<&Value>, run: Option<&Map<String, Value>>, w: &mut Warnings) -> Option<CellResult> {
    let outputs = outputs.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    let mut info = run.cloned();
    let mut res = CellResult::default();
    let mut any = info.is_some();
    for o in outputs {
        let Some(o) = o.as_object() else { continue };
        match str_of(o, "output_type") {
            Some("stream") => {
                push_lines(&mut res.messages, &source_text(o.get("text")));
                any = true;
            }
            Some("error") => {
                let msg = str_of(o, "evalue").map(str::trim).filter(|s| !s.is_empty()).map(String::from).unwrap_or_else(|| {
                    o.get("traceback").and_then(Value::as_array).map(|t| t.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("\n")).unwrap_or_default()
                });
                let msg = if msg.trim().is_empty() { "Error".to_string() } else { msg };
                res.error = Some(match res.error.take() {
                    Some(prev) => format!("{prev}\n{msg}"),
                    None => msg,
                });
                any = true;
            }
            Some("execute_result") | Some("display_data") => {
                let empty = Map::new();
                let data = o.get("data").and_then(Value::as_object).unwrap_or(&empty);
                let md = o.get("metadata").and_then(Value::as_object).and_then(|m| m.get("dbine")).and_then(Value::as_object);
                if let Some(md) = md {
                    flag_credentials(md, w);
                    if info.is_none() {
                        info = Some(md.clone());
                    }
                }
                if data.contains_key(DATARESOURCE) || md.is_some() {
                    res.results.push(read_statement(data.get(DATARESOURCE), md));
                } else if let Some(t) = data.get("text/plain") {
                    push_lines(&mut res.messages, &source_text(Some(t)));
                } else if let Some(h) = data.get("text/html") {
                    push_lines(&mut res.messages, &strip_tags(&source_text(Some(h))));
                }
                any = true;
            }
            _ => {}
        }
    }
    if !any {
        return None;
    }
    if let Some(i) = info {
        res.ran_at = str_of(&i, "ran_at").unwrap_or_default().to_string();
        res.duration_ms = i.get("duration_ms").and_then(Value::as_u64).unwrap_or(0);
        res.engine = str_of(&i, "engine").map(String::from);
        res.target_label = str_of(&i, "target_label").map(String::from);
        res.truncated = i.get("truncated").and_then(Value::as_bool).unwrap_or(false);
        res.params = read_params(i.get("params"), w);
    }
    Some(res)
}

/// A result's parameter values: scalars under valid names; a credential's
/// name drops the value, with the warning.
fn read_params(v: Option<&Value>, w: &mut Warnings) -> Option<Map<String, Value>> {
    let m = v?.as_object()?;
    let mut out = Map::new();
    for (k, v) in m {
        if !params::valid_name(k) || !(v.is_null() || v.is_string() || v.is_number() || v.is_boolean()) {
            continue;
        }
        if is_credential_param(k) && !v.is_null() {
            w.credentials = true;
            continue;
        }
        out.insert(k.clone(), v.clone());
    }
    (!out.is_empty()).then_some(out)
}

/// A parameter whose value a result doesn't record: a credential's or a
/// server's (a user name or a port is fine).
fn is_credential_param(name: &str) -> bool {
    is_credential_key(name) && !matches!(name.to_ascii_lowercase().as_str(), "user" | "username" | "login" | "port")
}

/// A result's parameter values as they are written (the file, its
/// summary line, the exports): without the credentials', which the file
/// never holds (on read they are dropped too).
fn recorded_params(r: &CellResult) -> Option<Map<String, Value>> {
    let p: Map<String, Value> = r.params.as_ref()?.iter().filter(|(k, _)| !is_credential_param(k)).map(|(k, v)| (k.clone(), v.clone())).collect();
    (!p.is_empty()).then_some(p)
}

/// A cell value as the model keeps it: null, bool, number or string.
fn scalar(v: Value) -> Value {
    match v {
        Value::Array(_) | Value::Object(_) => Value::String(v.to_string()),
        v => v,
    }
}

fn read_statement(dr: Option<&Value>, md: Option<&Map<String, Value>>) -> StoredStatement {
    let fields: Vec<&Map<String, Value>> = dr
        .and_then(|d| d.get("schema"))
        .and_then(|s| s.get("fields"))
        .and_then(Value::as_array)
        .map(|f| f.iter().filter_map(Value::as_object).collect())
        .unwrap_or_default();
    // The names as the query returned them (duplicates were renamed).
    let original: Option<Vec<&str>> = md
        .and_then(|m| m.get("column_names"))
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .filter(|a: &Vec<&str>| a.len() == fields.len());
    let keys: Vec<String> = fields.iter().map(|f| str_of(f, "name").unwrap_or_default().to_string()).collect();
    let columns = fields
        .iter()
        .enumerate()
        .map(|(i, f)| StoredColumn {
            name: original.as_ref().map(|o| o[i].to_string()).unwrap_or_else(|| keys[i].clone()),
            type_name: str_of(f, "dbine_type").or_else(|| str_of(f, "type")).unwrap_or_default().to_string(),
        })
        .collect();
    let rows: Vec<Vec<Value>> = dr
        .and_then(|d| d.get("data"))
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|r| match r {
                    Value::Array(a) => (0..keys.len()).map(|i| scalar(a.get(i).cloned().unwrap_or(Value::Null))).collect(),
                    // Keyed by name (Table Schema) or by position (Azure Data Studio).
                    Value::Object(o) => keys
                        .iter()
                        .enumerate()
                        .map(|(i, k)| scalar(o.get(k).or_else(|| o.get(&i.to_string())).cloned().unwrap_or(Value::Null)))
                        .collect(),
                    _ => vec![Value::Null; keys.len()],
                })
                .collect()
        })
        .unwrap_or_default();
    let kept = rows.len() as u64;
    let num = |k: &str| md.and_then(|m| m.get(k)).and_then(Value::as_u64);
    let rows_total = num("rows_total").unwrap_or(kept).max(kept);
    StoredStatement {
        columns,
        rows,
        rows_total,
        rows_kept: kept,
        truncated: md.and_then(|m| m.get("truncated")).and_then(Value::as_bool).unwrap_or(false) || rows_total > kept,
        rows_affected: num("rows_affected"),
        statement: md.and_then(|m| str_of(m, "statement")).map(String::from),
    }
}

// ---------------------------------------------------------------------------
// The preview

fn cut_value(v: &Value) -> Value {
    match v {
        Value::String(s) if s.chars().count() > MAX_VALUE_CHARS => {
            let mut t: String = s.chars().take(MAX_VALUE_CHARS).collect();
            t.push('…');
            Value::String(t)
        }
        Value::Array(_) | Value::Object(_) => cut_value(&scalar(v.clone())),
        v => v.clone(),
    }
}

/// What the file keeps of a cell's result.
#[derive(Clone, Debug, PartialEq)]
pub struct Preview {
    pub statements: Vec<StoredStatement>,
    /// Empty when the rows aren't kept.
    pub messages: Vec<String>,
    pub error: Option<String>,
    /// The messages or the error were cut to fit.
    pub cut: bool,
}

/// An error keeps at least this much of its text (chars), whatever the cap.
const MIN_ERROR_CHARS: usize = 2000;

/// The largest `n` in `lo..=hi` with `ok(n)`, given `ok(lo)` (and that `ok`
/// is true up to some point and false after it).
fn largest(lo: usize, hi: usize, ok: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (lo, hi);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if ok(mid) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

/// The first `n` chars of the messages (the last one kept ends in "…").
fn cut_messages(msgs: &[String], n: usize) -> Vec<String> {
    let mut left = n;
    let mut out = Vec::new();
    for m in msgs {
        if left == 0 {
            break;
        }
        let len = m.chars().count();
        if len <= left {
            out.push(m.clone());
            left -= len;
        } else {
            out.push(cut_chars(m, left));
            break;
        }
    }
    out
}

/// The result as the file stores it. Rows: one budget per cell
/// (`limits.rows`), the first statements first. Bytes (`limits.bytes`, the
/// cell's outputs): the summaries and at least the start of the error
/// always; then the rest of the error, the rows (the trailing ones dropped)
/// and the messages, in that order, while they fit. `keep` false: no rows
/// and no messages.
pub fn preview(result: &CellResult, keep: bool, limits: PreviewLimits, execution_count: Option<u32>) -> Preview {
    let mut budget = if keep { limits.rows as usize } else { 0 };
    let capped: Vec<StoredStatement> = result
        .results
        .iter()
        .map(|s| {
            let n = s.rows.len().min(budget);
            budget -= n;
            StoredStatement {
                rows: s.rows[..n].iter().map(|r| r.iter().map(cut_value).collect()).collect(),
                rows_total: s.rows_total.max(s.rows.len() as u64),
                ..s.clone()
            }
        })
        .collect();
    let total: usize = capped.iter().map(|s| s.rows.len()).sum();
    let make = |k: usize| -> Vec<StoredStatement> {
        let mut left = k;
        capped
            .iter()
            .map(|s| {
                let n = s.rows.len().min(left);
                left -= n;
                let mut s2 = StoredStatement { rows: s.rows[..n].to_vec(), ..s.clone() };
                s2.rows_kept = n as u64;
                s2.truncated = if keep { s.truncated || s2.rows_kept < s2.rows_total } else { s2.rows_total > 0 };
                s2
            })
            .collect()
    };
    let messages = if keep { result.messages.clone() } else { Vec::new() };
    let fits = |p: &Preview| {
        let size = serde_json::to_vec(&result_outputs(p, result, keep, execution_count)).map(|v| v.len()).unwrap_or(usize::MAX);
        size as u64 <= limits.bytes
    };
    let full = Preview { statements: make(total), messages: messages.clone(), error: result.error.clone(), cut: false };
    if fits(&full) {
        return full;
    }
    let error_at = |n: usize| result.error.as_deref().map(|e| cut_chars(e, n));
    let error_len = result.error.as_deref().map(|e| e.chars().count()).unwrap_or(0);
    let mut p = Preview { statements: make(0), messages: Vec::new(), error: error_at(MIN_ERROR_CHARS), cut: true };
    if error_len > MIN_ERROR_CHARS {
        let n = largest(MIN_ERROR_CHARS, error_len, |n| fits(&Preview { error: error_at(n), ..p.clone() }));
        p.error = error_at(n);
    }
    let k = largest(0, total, |k| fits(&Preview { statements: make(k), ..p.clone() }));
    p.statements = make(k);
    let chars: usize = messages.iter().map(|m| m.chars().count()).sum();
    let n = largest(0, chars, |n| fits(&Preview { messages: cut_messages(&messages, n), ..p.clone() }));
    p.messages = cut_messages(&messages, n);
    p.cut = p.error != result.error || p.messages != messages;
    p
}

/// "412 ms", "41.0 s", "3 min 5 s".
pub fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{:.1} s", ms as f64 / 1000.0)
    } else {
        format!("{} min {} s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// "{kept} / {total} rows · {ran_at} · {duration} · {target}".
pub fn summary_line(s: &StoredStatement, r: &CellResult) -> String {
    let head = if s.columns.is_empty() && s.rows_affected.is_some() {
        format!("{} rows affected", s.rows_affected.unwrap_or(0))
    } else {
        format!("{} / {} rows", s.rows_kept, s.rows_total)
    };
    let mut parts = vec![head];
    if !r.ran_at.is_empty() {
        parts.push(r.ran_at.clone());
    }
    parts.push(format_duration(r.duration_ms));
    if let Some(t) = r.target_label.as_deref().filter(|t| !t.is_empty()) {
        parts.push(t.to_string());
    }
    if let Some(p) = recorded_params(r) {
        parts.push(p.iter().map(|(k, v)| format!("{k} = {}", display_value(v))).collect::<Vec<_>>().join(", "));
    }
    parts.join(" · ")
}

/// A cell value as text: NULL, or the value.
pub fn display_value(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// Field names for Table Schema: unique (a repeated or empty name gets a
/// suffix). True when any was changed.
fn unique_names(cols: &[StoredColumn]) -> (Vec<String>, bool) {
    let all: HashSet<&str> = cols.iter().map(|c| c.name.as_str()).collect();
    let mut used = HashSet::new();
    let mut changed = false;
    let names = cols
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let base = if c.name.is_empty() { format!("column_{}", i + 1) } else { c.name.clone() };
            let mut name = base.clone();
            let mut n = 2;
            while used.contains(&name) || (name != c.name && all.contains(name.as_str())) {
                name = format!("{base}_{n}");
                n += 1;
            }
            changed |= name != c.name;
            used.insert(name.clone());
            name
        })
        .collect();
    (names, changed)
}

/// Table Schema's type for a column, from its values.
fn schema_type(rows: &[Vec<Value>], i: usize) -> &'static str {
    let vals: Vec<&Value> = rows.iter().filter_map(|r| r.get(i)).filter(|v| !v.is_null()).collect();
    if vals.is_empty() {
        "string"
    } else if vals.iter().all(|v| v.is_boolean()) {
        "boolean"
    } else if vals.iter().all(|v| v.is_i64() || v.is_u64()) {
        "integer"
    } else if vals.iter().all(|v| v.is_number()) {
        "number"
    } else {
        "string"
    }
}

fn cut_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    }
}

/// An aligned text table (the first rows, narrow columns).
pub fn text_table(cols: &[StoredColumn], rows: &[Vec<Value>]) -> String {
    let cell = |s: String| cut_chars(&s.replace(['\r', '\n', '\t'], " "), TEXT_COL_WIDTH);
    let head: Vec<String> = cols.iter().map(|c| cell(c.name.clone())).collect();
    let body: Vec<Vec<String>> = rows
        .iter()
        .take(TEXT_ROWS)
        .map(|r| (0..cols.len()).map(|i| cell(display_value(r.get(i).unwrap_or(&Value::Null)))).collect())
        .collect();
    let widths: Vec<usize> = (0..cols.len())
        .map(|i| body.iter().map(|r| r[i].chars().count()).chain([head[i].chars().count()]).max().unwrap_or(0))
        .collect();
    let line = |cells: &[String]| {
        let parts: Vec<String> = cells.iter().zip(&widths).map(|(c, w)| format!("{c:<w$}")).collect();
        parts.join(" | ").trim_end().to_string()
    };
    let mut out = vec![line(&head), widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("-+-")];
    out.extend(body.iter().map(|r| line(r)));
    if rows.len() > TEXT_ROWS {
        out.push("…".into());
    }
    out.join("\n")
}

/// An HTML table, every text escaped.
pub fn html_table(cols: &[StoredColumn], rows: &[Vec<Value>]) -> String {
    let mut h = String::from("<table><thead><tr>");
    for c in cols {
        h.push_str(&format!("<th>{}</th>", escape_html(&c.name)));
    }
    h.push_str("</tr></thead><tbody>");
    for r in rows {
        h.push_str("<tr>");
        for i in 0..cols.len() {
            match r.get(i).unwrap_or(&Value::Null) {
                Value::Null => h.push_str("<td><i>NULL</i></td>"),
                v => h.push_str(&format!("<td>{}</td>", escape_html(&display_value(v)))),
            }
        }
        h.push_str("</tr>");
    }
    h.push_str("</tbody></table>");
    h
}

/// Splits text as nbformat stores sources: lines, each with its `\n`.
pub(crate) fn lines(s: &str) -> Value {
    Value::Array(s.split_inclusive('\n').map(|l| Value::String(l.into())).collect())
}

/// The outputs of a SQL cell's result (already previewed).
pub(crate) fn result_outputs(p: &Preview, r: &CellResult, keep: bool, execution_count: Option<u32>) -> Vec<Value> {
    let mut out = Vec::new();
    for s in &p.statements {
        let summary = summary_line(s, r);
        let mut md = json!({
            "rows_total": s.rows_total,
            "rows_kept": s.rows_kept,
            "truncated": s.truncated,
            "ran_at": r.ran_at,
            "duration_ms": r.duration_ms,
            "engine": r.engine,
            "target_label": r.target_label,
            "rows_affected": s.rows_affected,
            "statement": s.statement,
        });
        if let Some(p) = recorded_params(r) {
            md["params"] = Value::Object(p);
        }
        let mut data = Map::new();
        if keep && !s.columns.is_empty() {
            let (names, renamed) = unique_names(&s.columns);
            if renamed {
                md["column_names"] = json!(s.columns.iter().map(|c| &c.name).collect::<Vec<_>>());
            }
            let fields: Vec<Value> = s
                .columns
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let mut f = json!({ "name": names[i], "type": schema_type(&s.rows, i) });
                    if !c.type_name.is_empty() {
                        f["dbine_type"] = json!(c.type_name);
                    }
                    f
                })
                .collect();
            let rows: Vec<Value> = s
                .rows
                .iter()
                .map(|row| {
                    let mut o = Map::new();
                    for (i, n) in names.iter().enumerate() {
                        o.insert(n.clone(), row.get(i).cloned().unwrap_or(Value::Null));
                    }
                    Value::Object(o)
                })
                .collect();
            data.insert(
                "text/html".into(),
                Value::String(format!("<p>{}</p>\n{}", escape_html(&summary), html_table(&s.columns, &s.rows))),
            );
            data.insert(DATARESOURCE.into(), json!({ "schema": { "fields": fields }, "data": rows }));
            data.insert("text/plain".into(), Value::String(format!("{summary}\n\n{}", text_table(&s.columns, &s.rows))));
        } else {
            data.insert("text/plain".into(), Value::String(summary));
        }
        out.push(json!({
            "output_type": "execute_result",
            "execution_count": execution_count,
            "metadata": { "dbine": md },
            "data": data,
        }));
    }
    if !p.messages.is_empty() {
        out.push(json!({ "output_type": "stream", "name": "stdout", "text": lines(&(p.messages.join("\n") + "\n")) }));
    }
    if let Some(e) = &p.error {
        out.push(json!({ "output_type": "error", "ename": "Error", "evalue": e, "traceback": [e] }));
    }
    out
}

// ---------------------------------------------------------------------------
// Writing

fn target_json(t: &NotebookTarget) -> Option<Value> {
    if t.results {
        return Some(json!({ "dataframes": true }));
    }
    let mut m = Map::new();
    for (k, v) in [("connection", &t.connection), ("database", &t.database), ("environment", &t.environment)] {
        if let Some(v) = v.as_deref().map(str::trim).filter(|v| !v.is_empty() && !(k == "connection" && looks_like_connection_string(v))) {
            m.insert(k.into(), Value::String(v.into()));
        }
    }
    (!m.is_empty()).then_some(Value::Object(m))
}

/// A chart's stored points: aggregated data only. Dropped when the rows
/// aren't kept, for a scatter or without aggregation (they'd be the raw
/// rows; histograms, box plots and KPIs are always summaries), and when they
/// don't fit in what the cell's byte cap leaves after `used`; otherwise at
/// most `limits.rows` per series, labels cut like cell values. Returns the
/// bytes the kept points take.
fn chart_points(m: &mut Map<String, Value>, keep: bool, limits: PreviewLimits, used: usize) -> usize {
    m.remove("data");
    let kind = m.get("type").and_then(Value::as_str).unwrap_or("");
    let agg = m.get("agg").and_then(Value::as_str).unwrap_or("none");
    let summary = matches!(kind, "histogram" | "box" | "kpi");
    let raw = !summary && (kind == "scatter" || agg == "none");
    let Some(pts) = m.get_mut("points") else { return 0 };
    let mut size = 0;
    let ok = keep && !raw && pts.is_object() && {
        let max = limits.rows as usize;
        let label = |v: &mut Value| *v = cut_value(v);
        if let Some(cats) = pts.get_mut("categories").and_then(Value::as_array_mut) {
            cats.truncate(max);
            cats.iter_mut().for_each(label);
        }
        if let Some(series) = pts.get_mut("series").and_then(Value::as_array_mut) {
            series.truncate(max);
            for s in series.iter_mut() {
                if let Some(n) = s.get_mut("name") {
                    label(n);
                }
                if let Some(vals) = s.get_mut("values").and_then(Value::as_array_mut) {
                    vals.truncate(max);
                    vals.iter_mut().for_each(label);
                }
            }
        }
        size = serde_json::to_vec(pts).map(|v| v.len()).unwrap_or(usize::MAX);
        (used.saturating_add(size)) as u64 <= limits.bytes
    };
    if ok {
        size
    } else {
        m.remove("points");
        m.remove("aggregate");
        0
    }
}

/// A cell's charts: the older one-chart object, or `{ charts: [...] }`; all
/// the charts' points together within the cell's byte cap, in order.
fn cell_chart_points(m: &mut Map<String, Value>, keep: bool, limits: PreviewLimits, used: usize) {
    if let Some(list) = m.get_mut("charts").and_then(Value::as_array_mut) {
        let mut used = used;
        for c in list.iter_mut() {
            if let Some(cm) = c.as_object_mut() {
                used = used.saturating_add(chart_points(cm, keep, limits, used));
            }
        }
        m.remove("points");
        m.remove("data");
    } else {
        chart_points(m, keep, limits, used);
    }
}

fn write_cell(c: &NotebookCell, nb: &Notebook, limits: PreviewLimits, ids: &mut HashSet<String>) -> Value {
    let mut id = if valid_id(&c.id) { c.id.clone() } else { new_id() };
    while !ids.insert(id.clone()) {
        id = new_id();
    }
    match c.kind {
        CellKind::Markdown => {
            let mut v = json!({ "cell_type": "markdown", "id": id, "metadata": {}, "source": lines(&c.source) });
            if let Some(a) = c.raw.as_ref().and_then(|r| r.get("attachments")) {
                v["attachments"] = a.clone();
            }
            v
        }
        CellKind::Foreign => match c.raw.as_ref().filter(|r| r.is_object()) {
            Some(raw) => {
                let mut v = raw.clone();
                if !v.get("id").and_then(Value::as_str).is_some_and(|i| i == id) {
                    v["id"] = Value::String(id);
                }
                v
            }
            None => json!({ "cell_type": "raw", "id": id, "metadata": {}, "source": lines(&c.source) }),
        },
        CellKind::Write => {
            let def = c.write.clone().unwrap_or_default();
            let mut w = serde_json::to_value(&def).unwrap_or_else(|_| json!({}));
            if let Some(m) = w.as_object_mut() {
                m.remove("target");
                if let Some(t) = def.target.as_ref().and_then(target_json) {
                    m.insert("target".into(), t);
                }
                m.retain(|_, v| !v.is_null());
            }
            let mut d = Map::new();
            d.insert("kind".into(), json!("write"));
            d.insert("write".into(), w);
            let mut outputs = Vec::new();
            if let Some(r) = &c.write_result {
                let short = WriteSummary { error: r.error.as_ref().map(|e| e.chars().take(2000).collect()), warnings: Vec::new(), ..r.clone() };
                d.insert("write_result".into(), serde_json::to_value(&short).unwrap_or(Value::Null));
                let line = write_summary_line(&short);
                outputs.push(match &short.error {
                    Some(e) => json!({ "output_type": "error", "ename": "Error", "evalue": e, "traceback": [line] }),
                    None => json!({ "output_type": "stream", "name": "stdout", "text": lines(&(line + "\n")) }),
                });
            }
            json!({
                "cell_type": "code",
                "execution_count": c.execution_count,
                "id": id,
                "metadata": { "language": "dbine-write", "dbine": d },
                "outputs": outputs,
                "source": lines(&write_cell_text(&def)),
            })
        }
        CellKind::Sql => {
            let keep = nb.save_results && c.keep_result;
            let mut d = Map::new();
            if let Some(t) = c.target.as_ref().and_then(target_json) {
                d.insert("target".into(), t);
            }
            if let Some(n) = c.name.as_deref().map(str::trim).filter(|n| local::valid_name(n)) {
                d.insert("name".into(), Value::String(n.into()));
            }
            d.insert("keep_result".into(), Value::Bool(c.keep_result));
            let outputs = match &c.result {
                Some(r) => {
                    let p = preview(r, keep, limits, c.execution_count);
                    let mut run = json!({ "ran_at": r.ran_at, "duration_ms": r.duration_ms, "engine": r.engine, "target_label": r.target_label });
                    if let Some(p) = recorded_params(r) {
                        run["params"] = Value::Object(p);
                    }
                    if p.cut {
                        run["truncated"] = Value::Bool(true);
                    }
                    d.insert("run".into(), run);
                    result_outputs(&p, r, keep, c.execution_count)
                }
                None => Vec::new(),
            };
            if let Some(chart) = c.chart.as_ref().filter(|c| !c.is_null()) {
                let mut chart = chart.clone();
                scrub(&mut chart, is_credential_key, false);
                if let Some(m) = chart.as_object_mut() {
                    // The points count toward the cell's byte cap: dropped when they don't fit.
                    let used = serde_json::to_vec(&outputs).map(|v| v.len()).unwrap_or(usize::MAX);
                    cell_chart_points(m, keep, limits, used);
                }
                d.insert("chart".into(), chart);
            }
            json!({
                "cell_type": "code",
                "execution_count": c.execution_count,
                "id": id,
                "metadata": { "language": c.language.as_deref().unwrap_or("sql"), "dbine": d },
                "outputs": outputs,
                "source": lines(&c.source),
            })
        }
    }
}

/// The notebook as nbformat 4.5 JSON.
pub fn to_value(nb: &Notebook, limits: PreviewLimits) -> Value {
    let mut dbine = Map::new();
    dbine.insert("version".into(), json!(1));
    if let Some(t) = nb.target.as_ref().and_then(target_json) {
        dbine.insert("target".into(), t);
    }
    dbine.insert("save_results".into(), Value::Bool(nb.save_results));
    if !nb.parameters.is_empty() {
        dbine.insert("parameters".into(), params::parameters_json(&nb.parameters));
    }
    let mut meta = Map::new();
    meta.insert("kernelspec".into(), json!({ "name": "sql", "display_name": "SQL", "language": "sql" }));
    meta.insert("language_info".into(), json!({ "name": "sql" }));
    meta.insert("dbine".into(), Value::Object(dbine));
    let mut extra = nb.extra_metadata.clone();
    scrub(&mut extra, is_foreign_sensitive_key, true);
    if let Value::Object(extra) = extra {
        for (k, v) in extra {
            meta.entry(k).or_insert(v);
        }
    }
    let mut ids = HashSet::new();
    let cells: Vec<Value> = nb.cells.iter().map(|c| write_cell(c, nb, limits, &mut ids)).collect();
    json!({ "cells": cells, "metadata": meta, "nbformat": 4, "nbformat_minor": 5 })
}

/// nbformat 4.5 text: 1-space indent like Jupyter, trailing newline.
pub fn serialize(nb: &Notebook, limits: PreviewLimits) -> String {
    let v = to_value(nb, limits);
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, serde_json::ser::PrettyFormatter::with_indent(b" "));
    // A Value always serializes.
    let _ = v.serialize(&mut ser);
    let mut s = String::from_utf8(buf).unwrap_or_default();
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stmt(cols: &[&str], rows: Vec<Vec<Value>>, total: u64) -> StoredStatement {
        StoredStatement {
            columns: cols.iter().map(|c| StoredColumn { name: c.to_string(), type_name: "int".into() }).collect(),
            rows_kept: rows.len() as u64,
            rows,
            rows_total: total,
            truncated: false,
            rows_affected: None,
            statement: Some("SELECT 1".into()),
        }
    }

    fn sql_cell(id: &str, source: &str, result: Option<CellResult>) -> NotebookCell {
        NotebookCell {
            id: id.into(),
            kind: CellKind::Sql,
            source: source.into(),
            language: Some("sql".into()),
            target: None,
            keep_result: true,
            chart: None,
            execution_count: Some(1),
            result,
            raw: None,
            name: None,
            write: None,
            write_result: None,
        }
    }

    fn result(stmts: Vec<StoredStatement>) -> CellResult {
        CellResult {
            results: stmts,
            messages: vec![],
            error: None,
            ran_at: "2026-10-18T14:32:05Z".into(),
            duration_ms: 412,
            engine: Some("postgres".into()),
            target_label: Some("Ventas prod › ventas".into()),
            truncated: false,
            params: None,
        }
    }

    fn notebook(cells: Vec<NotebookCell>) -> Notebook {
        Notebook {
            target: Some(NotebookTarget { connection: Some("Ventas prod".into()), database: Some("ventas".into()), environment: None, results: false }),
            save_results: true,
            save_results_set: true,
            cells,
            parameters: vec![],
            origin: "dbine".into(),
            extra_metadata: json!({}),
            warnings: vec![],
        }
    }

    fn rows(n: usize) -> Vec<Vec<Value>> {
        (0..n).map(|i| vec![json!(i), json!(format!("fila {i}"))]).collect()
    }

    #[test]
    fn credential_parameter_values_are_never_written() {
        let mut r = result(vec![stmt(&["id"], rows(1), 1)]);
        let p = json!({ "mes": "2026-10", "api_token": "tok-SECRET-1", "password": "pw-SECRET-2", "db_host": "h", "host": "host-SECRET-3", "user": "ana" });
        r.params = p.as_object().cloned();
        for save in [true, false] {
            let mut nb = notebook(vec![sql_cell("c1", "SELECT {{mes}}", Some(r.clone()))]);
            nb.save_results = save;
            let outs = [
                serialize(&nb, PreviewLimits::default()),
                export::html(&nb, PreviewLimits::default()),
                export::markdown(&nb, PreviewLimits::default()),
                summary_line(&r.results[0], &r),
            ];
            for text in outs {
                for secret in ["tok-SECRET-1", "pw-SECRET-2", "host-SECRET-3", "api_token", "password"] {
                    assert!(!text.contains(secret), "{secret} in {text}");
                }
                assert!(text.contains("2026-10") && text.contains("ana"), "{text}");
            }
        }
        // What is kept reads back.
        let text = serialize(&notebook(vec![sql_cell("c1", "SELECT 1", Some(r.clone()))]), PreviewLimits::default());
        let back = parse(&text).unwrap();
        assert!(back.warnings.is_empty());
        let kept = back.cells[0].result.as_ref().unwrap().params.clone().unwrap();
        let mut keys: Vec<_> = kept.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["db_host", "mes", "user"]);
    }

    #[test]
    fn a_dbine_notebook_round_trips() {
        let mut r = result(vec![stmt(&["id", "name"], rows(3), 3)]);
        r.messages = vec!["NOTICE: hola".into()];
        let mut c = sql_cell("c1", "SELECT id, name\nFROM t", Some(r));
        c.target = Some(NotebookTarget { connection: None, database: Some("otra".into()), environment: None, results: false });
        c.chart = Some(json!({ "type": "bar", "x": "name", "y": ["id"] }));
        let md = NotebookCell { kind: CellKind::Markdown, source: "# Título\n\nTexto".into(), language: None, execution_count: None, result: None, ..sql_cell("m1", "", None) };
        let mut err = sql_cell("c2", "SELECT nope", Some(CellResult { error: Some("column \"nope\" does not exist".into()), ..result(vec![]) }));
        err.execution_count = Some(2);
        let nb = notebook(vec![md, c, err]);
        let text = serialize(&nb, PreviewLimits::default());
        assert!(text.ends_with("}\n") && text.contains("\n \"cells\""));
        let back = parse(&text).unwrap();
        assert_eq!(back.origin, "dbine");
        assert!(back.warnings.is_empty());
        assert_eq!(back.target, nb.target);
        assert_eq!(back.cells.len(), 3);
        assert_eq!(back.cells[0], nb.cells[0]);
        let (a, b) = (&back.cells[1], &nb.cells[1]);
        assert_eq!((a.kind, &a.source, &a.target, &a.chart, a.execution_count), (b.kind, &b.source, &b.target, &b.chart, b.execution_count));
        assert_eq!(a.result, b.result);
        assert_eq!(back.cells[2].result.as_ref().unwrap().error.as_deref(), Some("column \"nope\" does not exist"));
        assert_eq!(back.cells[2].result.as_ref().unwrap().ran_at, "2026-10-18T14:32:05Z");
        // Same text again.
        assert_eq!(serialize(&back, PreviewLimits::default()), text);
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["nbformat_minor"], 5);
        assert_eq!(v["metadata"]["kernelspec"]["name"], "sql");
        assert_eq!(v["metadata"]["dbine"]["version"], 1);
        let out = &v["cells"][1]["outputs"][0];
        assert_eq!(out["data"][DATARESOURCE]["data"][0], json!({ "id": 0, "name": "fila 0" }));
        assert_eq!(out["data"][DATARESOURCE]["schema"]["fields"][0], json!({ "name": "id", "type": "integer", "dbine_type": "int" }));
        assert!(out["data"]["text/plain"].as_str().unwrap().starts_with("3 / 3 rows · 2026-10-18T14:32:05Z · 412 ms · Ventas prod › ventas"));
    }

    #[test]
    fn previews_are_capped_by_rows_and_bytes_per_cell() {
        let nb = notebook(vec![sql_cell("c", "SELECT", Some(result(vec![stmt(&["id", "name"], rows(500), 100_000), stmt(&["id", "name"], rows(50), 50)])))]);
        let limits = PreviewLimits::new(Some(100), Some(64 * 1024));
        let back = parse(&serialize(&nb, PreviewLimits::new(Some(100), Some(8 * 1024 * 1024)))).unwrap();
        let r = back.cells[0].result.as_ref().unwrap();
        assert_eq!((r.results[0].rows_kept, r.results[0].rows_total, r.results[0].truncated), (100, 100_000, true));
        // The row budget is the cell's: the first statement used it all.
        assert_eq!((r.results[1].rows_kept, r.results[1].rows_total, r.results[1].truncated), (0, 50, true));
        // A small byte budget: trailing rows dropped, the whole cell fits.
        let big: Vec<Vec<Value>> = (0..1000).map(|i| vec![json!(i), json!("x".repeat(300))]).collect();
        let nb = notebook(vec![sql_cell("c", "SELECT", Some(result(vec![stmt(&["id", "name"], big.clone(), 1000), stmt(&["id"], vec![vec![json!(1)]], 1)])))]);
        let v = to_value(&nb, PreviewLimits::new(Some(1000), Some(1)));
        assert_eq!(PreviewLimits::new(Some(5000), Some(1)).rows, 1000);
        let size = serde_json::to_vec(&v["cells"][0]["outputs"]).unwrap().len() as u64;
        assert!(size <= limits.bytes, "{size}");
        let back = parse(&serialize(&nb, limits)).unwrap();
        let r = back.cells[0].result.as_ref().unwrap();
        assert!(r.results[0].rows_kept > 0 && r.results[0].rows_kept < 1000 && r.results[0].truncated);
        assert_eq!((r.results[1].rows_kept, r.results[1].truncated), (0, true));
        // Even the header too big: no rows, summary kept.
        let huge = "n".repeat(70 * 1024);
        let nb = notebook(vec![sql_cell("c", "SELECT", Some(result(vec![stmt(&[&huge], vec![vec![json!(1)]; 10], 10)])))]);
        let back = parse(&serialize(&nb, limits)).unwrap();
        let s = &back.cells[0].result.as_ref().unwrap().results[0];
        assert_eq!((s.rows_kept, s.rows_total, s.truncated), (0, 10, true));
        // Long values are cut.
        let nb = notebook(vec![sql_cell("c", "SELECT", Some(result(vec![stmt(&["t"], vec![vec![json!("a".repeat(5000))]], 1)])))]);
        let back = parse(&serialize(&nb, limits)).unwrap();
        let v = back.cells[0].result.as_ref().unwrap().results[0].rows[0][0].as_str().unwrap().to_string();
        assert_eq!(v.chars().count(), MAX_VALUE_CHARS + 1);
        assert!(v.ends_with('…'));
    }

    #[test]
    fn results_off_keep_only_the_summary() {
        let r = result(vec![stmt(&["id", "name"], rows(3), 3)]);
        let mut nb = notebook(vec![sql_cell("c", "SELECT", Some(r.clone()))]);
        nb.cells[0].chart = Some(json!({ "type": "bar", "points": [[1, 2]] }));
        let check = |nb: &Notebook| {
            let text = serialize(nb, PreviewLimits::default());
            assert!(!text.contains("fila 0") && !text.contains("<table>") && !text.contains(DATARESOURCE) && !text.contains("points"));
            let v: Value = serde_json::from_str(&text).unwrap();
            let out = &v["cells"][0]["outputs"][0];
            assert_eq!(out["metadata"]["dbine"]["rows_kept"], 0);
            assert_eq!(out["metadata"]["dbine"]["rows_total"], 3);
            assert_eq!(out["metadata"]["dbine"]["truncated"], true);
            assert!(out["data"]["text/plain"].as_str().unwrap().starts_with("0 / 3 rows · "));
            let back = parse(&text).unwrap();
            let s = &back.cells[0].result.as_ref().unwrap().results[0];
            assert_eq!((s.rows_total, s.rows_kept, s.rows.len()), (3, 0, 0));
            assert_eq!(back.cells[0].chart, Some(json!({ "type": "bar" })));
        };
        nb.save_results = false;
        check(&nb);
        let back = parse(&serialize(&nb, PreviewLimits::default())).unwrap();
        assert!(!back.save_results);
        nb.save_results = true;
        nb.cells[0].keep_result = false;
        check(&nb);
        assert!(!parse(&serialize(&nb, PreviewLimits::default())).unwrap().cells[0].keep_result);
    }

    const ADS: &str = r##"{
 "metadata": {
  "kernelspec": { "name": "SQL", "display_name": "SQL", "language": "sql" },
  "language_info": { "name": "sql", "version": "" },
  "extensions": { "azuredatastudio": { "version": 1, "views": [] } },
  "connection_name": "prod-sql01.contoso.local"
 },
 "nbformat_minor": 2,
 "nbformat": 4,
 "cells": [
  { "cell_type": "markdown", "source": ["# Ventas\n", "Por región"], "metadata": { "azdata_cell_guid": "6d0e8a3c-6b4f-4d4e-9b0f-2a1d2b3c4d5e" } },
  {
   "cell_type": "code",
   "source": ["SELECT region, total\n", "FROM ventas"],
   "metadata": { "azdata_cell_guid": "a1b2c3d4-0000-4000-8000-000000000001", "language": "sql", "tags": [] },
   "outputs": [
    { "output_type": "display_data", "data": { "text/html": "(2 rows affected)" }, "metadata": {} },
    { "output_type": "display_data", "data": { "text/html": "Total execution time: 00:00:00.012" }, "metadata": {} },
    {
     "output_type": "execute_result",
     "metadata": {},
     "execution_count": 1,
     "data": {
      "application/vnd.dataresource+json": {
       "schema": { "fields": [ { "name": "region" }, { "name": "total" } ] },
       "data": [ { "0": "Norte", "1": "10" }, { "0": "Sur", "1": "20" } ]
      },
      "text/html": ["<table>", "<tr><th>region</th><th>total</th></tr>", "</table>"]
     }
    }
   ],
   "execution_count": 1
  },
  {
   "cell_type": "code",
   "source": "SELECT * FROM nope",
   "metadata": { "azdata_cell_guid": "a1b2c3d4-0000-4000-8000-000000000002" },
   "outputs": [ { "output_type": "error", "ename": "", "evalue": "Msg 208, Level 16, State 1, Line 1\r\nInvalid object name 'nope'.", "traceback": [] } ],
   "execution_count": 2
  }
 ]
}"##;

    #[test]
    fn azure_data_studio_notebooks_open_without_their_connection() {
        let nb = parse(ADS).unwrap();
        assert_eq!(nb.origin, "ads");
        assert_eq!(nb.target, None);
        assert_eq!(nb.cells.iter().map(|c| c.kind).collect::<Vec<_>>(), [CellKind::Markdown, CellKind::Sql, CellKind::Sql]);
        assert_eq!(nb.cells[0].id, "6d0e8a3c-6b4f-4d4e-9b0f-2a1d2b3c4d5e");
        assert_eq!(nb.cells[0].source, "# Ventas\nPor región");
        let r = nb.cells[1].result.as_ref().unwrap();
        assert_eq!(r.results[0].columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["region", "total"]);
        assert_eq!(r.results[0].rows, vec![vec![json!("Norte"), json!("10")], vec![json!("Sur"), json!("20")]]);
        assert_eq!(r.messages, ["(2 rows affected)", "Total execution time: 00:00:00.012"]);
        assert!(nb.cells[2].result.as_ref().unwrap().error.as_deref().unwrap().contains("Invalid object name 'nope'"));
        // Its connection is neither read nor written back.
        assert!(nb.warnings.iter().any(|w| w.contains("credenciales")));
        assert!(nb.extra_metadata.get("connection_name").is_none());
        let text = serialize(&nb, PreviewLimits::default());
        assert!(!text.contains("contoso"));
        assert!(text.contains("azuredatastudio"));
    }

    #[test]
    fn sql_magic_cells_lose_the_magic_and_the_connection_string() {
        let src = r#"{
 "metadata": { "kernelspec": { "name": "python3", "display_name": "Python 3", "language": "python" }, "language_info": { "name": "python" } },
 "nbformat": 4, "nbformat_minor": 5,
 "cells": [
  { "cell_type": "code", "id": "py1", "metadata": {}, "execution_count": 1, "outputs": [], "source": ["%load_ext sql"] },
  { "cell_type": "code", "id": "q1", "metadata": {}, "execution_count": 2,
    "outputs": [ { "output_type": "stream", "name": "stdout", "text": ["Connecting to postgresql://user:pass@host/db\n"] } ],
    "source": ["%%sql postgresql://user:pass@host/db\n", "SELECT * FROM ventas"] },
  { "cell_type": "code", "id": "q2", "metadata": {}, "execution_count": 3, "outputs": [], "source": ["%%sql\n", "SELECT 1"] }
 ]
}"#;
        let nb = parse(src).unwrap();
        assert_eq!(nb.origin, "jupyter");
        assert_eq!(nb.cells[0].kind, CellKind::Foreign);
        assert_eq!(nb.cells[0].language.as_deref(), Some("python"));
        assert_eq!((nb.cells[1].kind, nb.cells[1].source.as_str()), (CellKind::Sql, "SELECT * FROM ventas"));
        assert!(nb.cells[1].result.is_none());
        assert_eq!(nb.cells[2].source, "SELECT 1");
        assert_eq!(nb.warnings.len(), 1);
        assert!(nb.warnings[0].contains("%%sql"));
        let model = serde_json::to_string(&nb).unwrap();
        let text = serialize(&nb, PreviewLimits::default());
        for s in [&model, &text] {
            assert!(!s.contains("pass") && !s.contains("postgresql://") && !s.contains("host/db"), "{s}");
        }
    }

    #[test]
    fn foreign_cells_are_written_back_unchanged() {
        let py = r#"{"cell_type":"code","execution_count":7,"id":"abc-1","metadata":{"collapsed":false,"tags":["x"]},"outputs":[{"data":{"image/png":"iVBORw0KGgo=","text/plain":["<Figure>"]},"execution_count":7,"metadata":{"needs_background":"light"},"output_type":"execute_result"},{"name":"stderr","output_type":"stream","text":["warn 1.5e-7\n"]}],"source":["import pandas as pd\n","df = pd.DataFrame({'a': [1.10, 2]})"]}"#;
        let src = format!(r#"{{"metadata":{{"kernelspec":{{"name":"python3","language":"python"}}}},"nbformat":4,"nbformat_minor":5,"cells":[{py}]}}"#);
        let nb = parse(&src).unwrap();
        assert_eq!(nb.cells[0].kind, CellKind::Foreign);
        assert!(nb.cells[0].source.starts_with("import pandas"));
        let text = serialize(&nb, PreviewLimits::default());
        let v: Value = serde_json::from_str(&text).unwrap();
        let orig: Value = serde_json::from_str(py).unwrap();
        assert_eq!(v["cells"][0], orig);
        assert_eq!(serde_json::to_string(&v["cells"][0]).unwrap(), serde_json::to_string(&orig).unwrap());
    }

    #[test]
    fn cell_names_and_the_dataframe_sql_target_round_trip() {
        let mut nb = notebook(vec![sql_cell("a", "SELECT 1", None), sql_cell("b", "SELECT * FROM ventas", None)]);
        nb.cells[0].name = Some("ventas".into());
        nb.cells[1].name = Some("c2".into());
        nb.cells[1].target = Some(NotebookTarget { results: true, ..Default::default() });
        let text = serialize(&nb, PreviewLimits::default());
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["cells"][0]["metadata"]["dbine"]["name"], "ventas");
        // No connection name is made up for it.
        assert_eq!(v["cells"][1]["metadata"]["dbine"]["target"], json!({ "dataframes": true }));
        let back = parse(&text).unwrap();
        assert_eq!(back.cells[0].name.as_deref(), Some("ventas"));
        assert_eq!(back.cells[1].name.as_deref(), Some("c2"));
        assert_eq!(back.cells[1].target, Some(NotebookTarget { results: true, ..Default::default() }));
        // An older file has no names: read as before.
        let old = r#"{"nbformat":4,"nbformat_minor":5,"metadata":{"dbine":{"version":1}},"cells":[{"cell_type":"code","id":"x","metadata":{"dbine":{"keep_result":true}},"outputs":[],"source":"SELECT 1"}]}"#;
        assert_eq!(parse(old).unwrap().cells[0].name, None);
        // Invalid names and repeats (ignoring case) are dropped; "results" wins over a name.
        let bad = r#"{"nbformat":4,"nbformat_minor":5,"metadata":{"dbine":{"version":1}},"cells":[
          {"cell_type":"code","id":"x","metadata":{"dbine":{"name":"Ventas"}},"outputs":[],"source":"SELECT 1"},
          {"cell_type":"code","id":"y","metadata":{"dbine":{"name":"ventas"}},"outputs":[],"source":"SELECT 1"},
          {"cell_type":"code","id":"z","metadata":{"dbine":{"name":"x; DROP TABLE t","target":{"results":true,"connection":"Ventas"}}},"outputs":[],"source":"SELECT 1"}]}"#;
        let nb = parse(bad).unwrap();
        assert_eq!(nb.cells.iter().map(|c| c.name.as_deref()).collect::<Vec<_>>(), vec![Some("Ventas"), None, None]);
        assert_eq!(nb.cells[2].target, Some(NotebookTarget { results: true, ..Default::default() }));
    }

    #[test]
    fn the_earlier_results_target_is_still_read_and_written_as_dataframes() {
        let df = NotebookTarget { results: true, ..Default::default() };
        let file = |target: &str| {
            format!(r#"{{"nbformat":4,"nbformat_minor":5,"metadata":{{"dbine":{{"version":1}}}},"cells":[{{"cell_type":"code","id":"x","metadata":{{"dbine":{{"target":{target}}}}},"outputs":[],"source":"SELECT 1"}}]}}"#)
        };
        for target in [r#"{"results":true}"#, r#"{"dataframes":true}"#, r#"{"dataframes":true,"connection":"Ventas"}"#] {
            let nb = parse(&file(target)).unwrap();
            assert_eq!(nb.cells[0].target, Some(df.clone()), "{target}");
            // Saved again: the new form, nothing else.
            let v: Value = serde_json::from_str(&serialize(&nb, PreviewLimits::default())).unwrap();
            assert_eq!(v["cells"][0]["metadata"]["dbine"]["target"], json!({ "dataframes": true }), "{target}");
        }
        // `false` is no target of its own; a connection beside it still reads.
        let nb = parse(&file(r#"{"dataframes":false,"connection":"Ventas"}"#)).unwrap();
        assert_eq!(nb.cells[0].target, Some(NotebookTarget { connection: Some("Ventas".into()), ..Default::default() }));
    }

    #[test]
    fn credentials_in_dbine_metadata_are_dropped_with_a_warning() {
        let src = r#"{
 "metadata": { "dbine": { "version": 1, "password": "s3cr3t", "save_results": true,
   "target": { "connection": "Ventas", "host": "10.0.0.5", "user": "sa", "Password": "s3cr3t" } },
   "kernelspec": { "name": "sql" }, "other_tool": { "api_token": "tok123", "theme": "dark" } },
 "nbformat": 4, "nbformat_minor": 5,
 "cells": [ { "cell_type": "code", "id": "c", "execution_count": null, "outputs": [], "source": "SELECT 1",
   "metadata": { "dbine": { "keep_result": true, "connection_string": "Server=x;Pwd=s3cr3t", "chart": { "type": "bar", "token": "tok123" },
     "target": { "connection": "Otra", "dsn": "x" } } } } ]
}"#;
        let nb = parse(src).unwrap();
        assert_eq!(nb.target, Some(NotebookTarget { connection: Some("Ventas".into()), ..Default::default() }));
        assert_eq!(nb.cells[0].target, Some(NotebookTarget { connection: Some("Otra".into()), ..Default::default() }));
        assert_eq!(nb.cells[0].chart, Some(json!({ "type": "bar" })));
        assert_eq!(nb.extra_metadata, json!({ "other_tool": { "theme": "dark" } }));
        assert_eq!(nb.warnings.len(), 1);
        let all = format!("{}{}", serde_json::to_string(&nb).unwrap(), serialize(&nb, PreviewLimits::default()));
        assert!(!all.contains("s3cr3t") && !all.contains("tok123") && !all.contains("10.0.0.5") && !all.contains("\"sa\""));
    }

    #[test]
    fn duplicate_columns_and_array_rows() {
        let nb = notebook(vec![sql_cell("c", "SELECT", Some(result(vec![stmt(&["id", "id", "id_2"], vec![vec![json!(1), json!(2), json!(3)]], 1)])))]);
        let text = serialize(&nb, PreviewLimits::default());
        let v: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["cells"][0]["outputs"][0]["data"][DATARESOURCE]["data"][0], json!({ "id": 1, "id_3": 2, "id_2": 3 }));
        let back = parse(&text).unwrap();
        assert_eq!(back.cells[0].result, nb.cells[0].result);
        // Rows as arrays are read too.
        let src = r#"{"nbformat":4,"nbformat_minor":5,"metadata":{"dbine":{"version":1}},"cells":[{"cell_type":"code","id":"c","metadata":{"dbine":{}},"outputs":[{"output_type":"execute_result","metadata":{},"data":{"application/vnd.dataresource+json":{"schema":{"fields":[{"name":"a"},{"name":"b"}]},"data":[[1,{"x":1}]]}}}],"source":"x"}]}"#;
        let nb = parse(src).unwrap();
        assert_eq!(nb.cells[0].result.as_ref().unwrap().results[0].rows, vec![vec![json!(1), json!("{\"x\":1}")]]);
    }

    #[test]
    fn the_row_budget_is_shared_by_the_cells_statements() {
        let r = result(vec![stmt(&["id", "name"], rows(30), 30), stmt(&["id", "name"], rows(30), 30), stmt(&["id"], vec![], 0)]);
        let p = preview(&r, true, PreviewLimits::new(Some(50), None), Some(1));
        let kept: Vec<_> = p.statements.iter().map(|s| (s.rows_kept, s.rows.len(), s.truncated)).collect();
        assert_eq!(kept, [(30, 30, false), (20, 20, true), (0, 0, false)]);
        assert!(!p.cut);
        let back = parse(&serialize(&notebook(vec![sql_cell("c", "SELECT", Some(r))]), PreviewLimits::new(Some(50), None))).unwrap();
        let total: u64 = back.cells[0].result.as_ref().unwrap().results.iter().map(|s| s.rows_kept).sum();
        assert_eq!(total, 50);
    }

    #[test]
    fn messages_and_errors_fit_in_the_byte_cap() {
        let limits = PreviewLimits::new(Some(1000), Some(64 * 1024));
        let size = |nb: &Notebook| serde_json::to_vec(&to_value(nb, limits)["cells"][0]["outputs"]).unwrap().len() as u64;
        // Messages: cut to what's left, marked.
        let mut r = result(vec![stmt(&["id", "name"], rows(10), 10)]);
        r.messages = (0..2000).map(|i| format!("NOTICE {i}: {}", "m".repeat(100))).collect();
        let nb = notebook(vec![sql_cell("c", "SELECT", Some(r.clone()))]);
        assert!(size(&nb) <= limits.bytes);
        let back = parse(&serialize(&nb, limits)).unwrap();
        let b = back.cells[0].result.as_ref().unwrap();
        assert!(b.truncated && !b.messages.is_empty() && b.messages.len() < 2000);
        assert!(b.messages.last().unwrap().ends_with('…'));
        // The rows go before the messages.
        assert_eq!(b.results[0].rows_kept, 10);
        // A huge error: cut, but its start is always kept.
        let mut r = result(vec![]);
        r.error = Some(format!("ERROR: {}", "e".repeat(200_000)));
        let nb = notebook(vec![sql_cell("c", "SELECT", Some(r))]);
        assert!(size(&nb) <= limits.bytes);
        let back = parse(&serialize(&nb, limits)).unwrap();
        let b = back.cells[0].result.as_ref().unwrap();
        let e = b.error.as_deref().unwrap();
        assert!(b.truncated && e.starts_with("ERROR: ") && e.ends_with('…'));
        assert!(e.chars().count() >= MIN_ERROR_CHARS && e.chars().count() < 200_000);
        // Even when nothing else fits, at least the error's first 2,000 chars.
        let p = preview(&CellResult { error: Some("x".repeat(10_000)), ..result(vec![]) }, true, PreviewLimits { rows: 10, bytes: 10 }, None);
        assert_eq!(p.error.unwrap().chars().count(), MIN_ERROR_CHARS);
        // Small ones are untouched.
        let mut r = result(vec![stmt(&["id"], vec![vec![json!(1)]], 1)]);
        r.messages = vec!["hola".into()];
        r.error = Some("boom".into());
        let p = preview(&r, true, limits, None);
        assert_eq!((p.messages, p.error.as_deref(), p.cut), (vec!["hola".to_string()], Some("boom"), false));
    }

    #[test]
    fn chart_points_are_only_aggregates_within_the_caps() {
        let pts = |n: usize, label: &str| json!({ "categories": (0..n).map(|i| format!("{label}{i}")).collect::<Vec<_>>(), "series": [{ "name": "total", "values": (0..n).collect::<Vec<_>>() }] });
        let write = |chart: Value, limits: PreviewLimits| {
            let mut c = sql_cell("c", "SELECT", Some(result(vec![stmt(&["id", "name"], rows(3), 3)])));
            c.chart = Some(chart);
            to_value(&notebook(vec![c]), limits)["cells"][0]["metadata"]["dbine"]["chart"].clone()
        };
        let limits = PreviewLimits::new(Some(10), None);
        // Aggregated: kept, capped to the cell's rows, long labels cut.
        let long = "l".repeat(5000);
        let out = write(json!({ "type": "bar", "agg": "sum", "points": pts(50, &long) }), limits);
        assert_eq!(out["points"]["categories"].as_array().unwrap().len(), 10);
        assert_eq!(out["points"]["series"][0]["values"].as_array().unwrap().len(), 10);
        assert_eq!(out["points"]["categories"][0].as_str().unwrap().chars().count(), MAX_VALUE_CHARS + 1);
        // Raw rows: no aggregation, or a scatter.
        for chart in [json!({ "type": "bar", "agg": "none", "points": pts(3, "a") }), json!({ "type": "bar", "points": pts(3, "a") }), json!({ "type": "scatter", "agg": "sum", "points": pts(3, "a") })] {
            let out = write(chart, limits);
            assert!(out.get("points").is_none() && out.get("type").is_some(), "{out}");
        }
        // Points that don't fit in what the byte cap leaves: dropped.
        let big = PreviewLimits::new(Some(1000), Some(64 * 1024));
        let out = write(json!({ "type": "bar", "agg": "sum", "points": pts(1000, &"x".repeat(200)) }), big);
        assert!(out.get("points").is_none());
        assert_eq!(write(json!({ "type": "bar", "agg": "count", "points": pts(5, "a") }), big)["points"]["categories"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn several_charts_share_the_cells_byte_cap() {
        let pts = |n: usize| json!({ "categories": (0..n).map(|i| format!("c{i}")).collect::<Vec<_>>(), "series": [{ "name": "v", "values": (0..n).collect::<Vec<_>>() }] });
        let write = |chart: Value, limits: PreviewLimits| {
            let mut c = sql_cell("c", "SELECT", Some(result(vec![stmt(&["id", "name"], rows(3), 3)])));
            c.chart = Some(chart);
            to_value(&notebook(vec![c]), limits)["cells"][0]["metadata"]["dbine"]["chart"].clone()
        };
        let one = serde_json::to_vec(&pts(300)).unwrap().len() as u64;
        let limits = PreviewLimits::new(Some(1000), Some(64 * 1024));
        let out = write(json!({ "view": "chart", "active": 0, "points": pts(3), "charts": [
            { "title": "a", "type": "bar", "agg": "sum", "points": pts(300) },
            { "title": "raw", "type": "scatter", "agg": "sum", "points": pts(3) },
            { "title": "h", "type": "histogram", "agg": "none", "points": pts(5) },
            { "title": "k", "type": "kpi", "agg": "none", "points": pts(1), "whole": true, "aggregate": { "method": "server" } },
            { "title": "line", "type": "line", "agg": "none", "points": pts(3) },
        ] }), limits);
        let list = out["charts"].as_array().unwrap();
        assert!(out.get("points").is_none(), "no stray top-level points");
        assert_eq!(list[0]["points"]["categories"].as_array().unwrap().len(), 300);
        assert!(list[1].get("points").is_none() && list[4].get("points").is_none(), "raw rows aren't kept");
        assert!(list[2].get("points").is_some() && list[3].get("points").is_some(), "histograms and KPIs are summaries");
        assert_eq!(list[3]["aggregate"]["method"], "server");
        // ~40 KB each under a 64 KB cap: the first keeps its points, the second doesn't.
        let wide = json!({ "categories": (0..1000).map(|i| format!("{i:0>36}")).collect::<Vec<_>>(), "series": [{ "name": "v", "values": (0..1000).collect::<Vec<_>>() }] });
        let out = write(json!({ "charts": [{ "type": "bar", "agg": "sum", "points": wide.clone() }, { "type": "bar", "agg": "sum", "points": wide }] }), limits);
        let list = out["charts"].as_array().unwrap();
        assert!(list[0].get("points").is_some() && list[1].get("points").is_none(), "{one}");
        // Rows not kept: no chart keeps points.
        let mut c = sql_cell("c", "SELECT", Some(result(vec![stmt(&["id"], rows(3), 3)])));
        c.keep_result = false;
        c.chart = Some(json!({ "charts": [{ "type": "bar", "agg": "sum", "points": pts(3) }] }));
        let out = to_value(&notebook(vec![c]), limits)["cells"][0]["metadata"]["dbine"]["chart"].clone();
        assert!(out["charts"][0].get("points").is_none());
    }

    #[test]
    fn whether_the_file_said_save_results_is_known() {
        let back = parse(&serialize(&notebook(vec![]), PreviewLimits::default())).unwrap();
        assert!(back.save_results_set && back.save_results);
        let text = serialize(&back, PreviewLimits::default());
        assert!(!text.contains("save_results_set"));
        let foreign = parse(r#"{"nbformat":4,"nbformat_minor":5,"metadata":{"kernelspec":{"name":"python3"}},"cells":[]}"#).unwrap();
        assert!(!foreign.save_results_set && foreign.save_results);
        assert!(!parse(ADS).unwrap().save_results_set);
        let said = parse(r#"{"nbformat":4,"nbformat_minor":5,"metadata":{"dbine":{"version":1,"save_results":false}},"cells":[]}"#).unwrap();
        assert!(said.save_results_set && !said.save_results);
        let unsaid = parse(r#"{"nbformat":4,"nbformat_minor":5,"metadata":{"dbine":{"version":1}},"cells":[]}"#).unwrap();
        assert!(!unsaid.save_results_set);
    }

    #[test]
    fn connection_strings_in_a_target_are_dropped() {
        let src = r#"{"nbformat":4,"nbformat_minor":5,"metadata":{"dbine":{"version":1,"target":{"connection":"postgresql://sa:s3cr3t@db/x","database":"ventas"}}},
          "cells":[{"cell_type":"code","id":"c","metadata":{"dbine":{"target":{"connection":"Server=db;User Id=sa;Password=s3cr3t"}}},"outputs":[],"source":"SELECT 1"},
                   {"cell_type":"code","id":"d","metadata":{"dbine":{"target":{"connection":"Ventas (prod) = 2"}}},"outputs":[],"source":"SELECT 1"}]}"#;
        let nb = parse(src).unwrap();
        assert_eq!(nb.target, Some(NotebookTarget { database: Some("ventas".into()), ..Default::default() }));
        assert_eq!(nb.cells[0].target, None);
        // A name with "=" alone is still a name.
        assert_eq!(nb.cells[1].target.as_ref().unwrap().connection.as_deref(), Some("Ventas (prod) = 2"));
        assert_eq!(nb.warnings.len(), 1);
        assert!(!serde_json::to_string(&nb).unwrap().contains("s3cr3t"));
        // Never written either.
        let mut nb = notebook(vec![]);
        nb.target = Some(NotebookTarget { connection: Some("mysql://root:pw@h/db".into()), ..Default::default() });
        assert!(!serialize(&nb, PreviewLimits::default()).contains("pw@h"));
    }

    #[test]
    fn invalid_files_are_errors() {
        assert!(parse("not json").is_err());
        assert!(parse("[]").is_err());
        assert!(parse(r#"{"nbformat":3,"cells":[]}"#).is_err());
        assert!(parse(r#"{"nbformat":4,"nbformat_minor":0,"metadata":{},"cells":[]}"#).is_ok());
    }
}
