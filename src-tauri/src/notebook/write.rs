//! A notebook's write cell: a DataFrame (a cell's whole result) written
//! into a table of any engine that writes (docs/notebooks.md › Writing a
//! DataFrame to a table).
//!
//! Two steps, both commands of the app only (no MCP tool or assistant
//! tool reaches them):
//!
//! 1. [`prepare`]: the source cell's DataFrame is loaded into the tab's
//!    DuckDB session with the same rules as DataFrame SQL
//!    ([`super::local::stage`]: its complete run in this session or a
//!    read-only run again, files only with their approval) and copied into
//!    a **snapshot** table of its own. The DataFrame must be whole (no
//!    table on the way cut at the row cap). The target is checked (exists,
//!    or will be created; not read-only; the mode is one the engine does),
//!    the columns are paired (by name, or as the user mapped them), the
//!    keys checked (no repeated or empty key in the DataFrame) and a
//!    preview made. Nothing is written. It returns a **one-time token**
//!    bound to the tab, the cell, the target, the mode, the keys, the
//!    mapping and the row count.
//! 2. [`run`]: only with that token, after the user confirmed (the UI's
//!    dialog shows target, mode, rows and keys). Everything is checked
//!    again (read-only, the snapshot's rows), then the snapshot is read in
//!    pages (typed cells, DuckDB's `read_batches`) and streamed to the
//!    driver's [`Session::write_rows`](dbine_driver::Session::write_rows),
//!    or to its bulk load for a plain insert on engines without modes.
//!
//! Values never go into SQL text here: they travel as cells; the only SQL
//! this module writes is DuckDB's over its own snapshot (identifiers quoted).

use super::local::{
    exec, first_value, session_key, stage, LoadedTable, LocalSource, Staged, ROW_CAP_RANGE,
};
use crate::commands::projects::sha256_hex;
use crate::error::{CommandError, CommandResult};
use crate::state::{AppState, SessionEntry};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::transfer::{BatchSource, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{
    BatchSink, ColumnDef, DdlParts, Driver, KeyDef, ObjectRef, QueryOutcome, Session, TableSchema,
    WriteMode, WriteResult, WriteSpec,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// How long a confirmation's token is good for.
const TTL: Duration = Duration::from_secs(15 * 60);
/// Rows of the snapshot read per page (each page goes in 1,000-row batches).
const PAGE_ROWS: u64 = 20_000;
/// Rows the preview shows.
const PREVIEW_ROWS: usize = 20;

/// Where the DataFrame goes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WriteTarget {
    pub connection_id: String,
    #[serde(default)]
    pub database: String,
    #[serde(default)]
    pub schema: Option<String>,
    pub table: String,
    /// "Crear tabla nueva": the table is created with the DataFrame's
    /// columns (it must not exist).
    #[serde(default)]
    pub create: bool,
}

/// A DataFrame column written into a target column.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ColumnPair {
    pub source: String,
    pub target: String,
}

/// What the UI sends to [`prepare`] and [`run`].
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteArgs {
    pub tab_id: String,
    /// The write cell (its id).
    pub cell_id: String,
    /// The cell whose DataFrame is written (its name).
    pub source: String,
    /// The SQL cells above the write cell, as for DataFrame SQL.
    pub sources: Vec<LocalSource>,
    #[serde(default)]
    pub row_cap: Option<u64>,
    #[serde(default)]
    pub notebook_path: Option<String>,
    /// The file approval the user just answered (as for DataFrame SQL).
    #[serde(default)]
    pub approval: Option<String>,
    /// The AWS profile for `s3://` reads (Settings › Notebooks), as for DataFrame SQL.
    #[serde(default)]
    pub aws_profile: Option<String>,
    pub target: WriteTarget,
    pub mode: WriteMode,
    /// Target columns.
    #[serde(default)]
    pub keys: Vec<String>,
    /// `None`: by name.
    #[serde(default)]
    pub mapping: Option<Vec<ColumnPair>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannedColumn {
    pub source: String,
    pub source_type: String,
    pub target: String,
    pub target_type: String,
    /// The value changes type on the way (text to number, date to text…).
    pub converted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Preview {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

/// What [`prepare`] found: shown in the cell and in the confirmation.
#[derive(Debug, Clone, Serialize)]
pub struct WritePlan {
    /// For [`run`], once.
    pub token: String,
    pub rows: u64,
    /// "Ventas prod › ventas › dbo.resumen".
    pub target_label: String,
    /// The connection's name, the database and the table as written
    /// (the table as the engine will name it when it's created).
    pub connection: String,
    pub database: String,
    pub table: String,
    pub engine: String,
    pub columns: Vec<PlannedColumn>,
    /// DataFrame columns that aren't written.
    pub unmatched_source: Vec<String>,
    /// Target columns nothing is written into (they keep their default).
    pub unmatched_target: Vec<String>,
    pub keys: Vec<String>,
    pub preview: Preview,
    pub warnings: Vec<String>,
    /// The `CREATE TABLE` that runs first ("crear tabla nueva").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create: Option<String>,
    /// One transaction: a failure leaves the table as it was.
    pub atomic: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PrepareResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<WritePlan>,
    pub loaded: Vec<LoadedTable>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval: Option<super::access::ApprovalRequest>,
}

/// What [`run`] did.
#[derive(Debug, Clone, Serialize)]
pub struct WriteOutcome {
    pub inserted: u64,
    pub updated: u64,
    pub deleted: u64,
    pub atomic: bool,
    pub elapsed_ms: u64,
    pub target_label: String,
    pub warnings: Vec<String>,
    /// The table was created first.
    pub created: bool,
}

// ---------------------------------------------------------------------------
// The confirmation's token

#[derive(Debug, Clone)]
struct Pending {
    tab: String,
    cell: String,
    bind: String,
    snapshot: String,
    rows: u64,
    pairs: Vec<PlannedColumn>,
    table: ObjectRef,
    create: Option<Vec<String>>,
    label: String,
    warnings: Vec<String>,
    at: Instant,
}

static PENDING: LazyLock<Mutex<HashMap<String, Pending>>> = LazyLock::new(Default::default);

fn pending() -> CommandResult<std::sync::MutexGuard<'static, HashMap<String, Pending>>> {
    PENDING
        .lock()
        .map_err(|_| CommandError::Internal("escrituras pendientes".into()))
}

/// What a token is bound to: the tab, the cell, the target, the mode, the
/// keys, the mapping as the user set it, and the row count confirmed.
pub fn binding(a: &WriteArgs, rows: u64) -> String {
    let v = serde_json::json!([
        a.tab_id,
        a.cell_id,
        a.source.to_ascii_lowercase(),
        a.target,
        a.mode,
        a.keys,
        a.mapping,
        rows
    ]);
    sha256_hex(v.to_string().as_bytes())
}

/// A new token for `p`; the cell's earlier one goes (its snapshot is
/// returned, to drop).
fn issue(p: Pending) -> CommandResult<(String, Option<String>)> {
    let token = uuid::Uuid::new_v4().simple().to_string();
    let mut all = pending()?;
    let mut old = None;
    all.retain(|_, x| {
        let keep = x.at.elapsed() < TTL && !(x.tab == p.tab && x.cell == p.cell);
        if !keep && x.tab == p.tab {
            old = Some(x.snapshot.clone());
        }
        keep
    });
    all.insert(token.clone(), p);
    Ok((token, old))
}

/// The pending write `token` stands for, if it's this tab's and cell's and
/// matches `bind` exactly. Spent either way: a token works once.
fn take(token: &str, tab: &str, cell: &str, bind: &str) -> CommandResult<Pending> {
    let mut all = pending()?;
    all.retain(|_, x| x.at.elapsed() < TTL);
    let p = all.remove(token).ok_or_else(|| {
        CommandError::BadRequest(
            "la confirmación de la escritura ya no vale: ejecutá la celda de nuevo".into(),
        )
    })?;
    if p.tab != tab || p.cell != cell || p.bind != bind {
        return Err(CommandError::BadRequest(
            "la escritura cambió desde que se confirmó: ejecutá la celda de nuevo".into(),
        ));
    }
    Ok(p)
}

// ---------------------------------------------------------------------------
// Columns, keys and types

/// Pairs DataFrame columns with target columns: as the user mapped them,
/// else by name (ignoring case). `free`: a schemaless target (documents,
/// keys…), where a column without a match becomes a field of its name.
pub fn pair_columns(
    source: &[String],
    target: &[String],
    mapping: Option<&[ColumnPair]>,
    free: bool,
) -> CommandResult<Vec<ColumnPair>> {
    let find = |list: &[String], n: &str| list.iter().find(|c| c.eq_ignore_ascii_case(n)).cloned();
    let pairs: Vec<ColumnPair> = match mapping {
        None => source
            .iter()
            .filter_map(|s| match find(target, s) {
                Some(t) => Some(ColumnPair {
                    source: s.clone(),
                    target: t,
                }),
                None if free => Some(ColumnPair {
                    source: s.clone(),
                    target: s.clone(),
                }),
                None => None,
            })
            .collect(),
        Some(m) => {
            let mut out = Vec::new();
            for p in m {
                let s = find(source, &p.source).ok_or_else(|| {
                    CommandError::BadRequest(format!(
                        "el DataFrame no tiene la columna «{}»",
                        p.source
                    ))
                })?;
                let t = match find(target, &p.target) {
                    Some(t) => t,
                    None if free && !p.target.trim().is_empty() => p.target.clone(),
                    None => {
                        return Err(CommandError::BadRequest(format!(
                            "la tabla no tiene la columna «{}»",
                            p.target
                        )))
                    }
                };
                out.push(ColumnPair {
                    source: s,
                    target: t,
                });
            }
            out
        }
    };
    let mut seen = std::collections::HashSet::new();
    for p in &pairs {
        if !seen.insert(p.target.to_lowercase()) {
            return Err(CommandError::BadRequest(format!(
                "dos columnas del DataFrame van a «{}»",
                p.target
            )));
        }
    }
    if pairs.is_empty() {
        return Err(CommandError::BadRequest("ninguna columna del DataFrame coincide con una de la tabla: elegí a qué columna va cada una".into()));
    }
    Ok(pairs)
}

/// A type's broad family, to tell when a value changes kind on the way.
pub fn family(type_name: &str) -> &'static str {
    let t = type_name.trim().to_ascii_lowercase();
    let base = t.split(['(', ' ']).next().unwrap_or("");
    match base {
        "bool" | "boolean" | "bit" => "bool",
        "tinyint" | "smallint" | "int" | "integer" | "bigint" | "hugeint" | "ubigint"
        | "uinteger" | "usmallint" | "utinyint" | "uhugeint" | "int2" | "int4" | "int8"
        | "serial" | "bigserial" | "mediumint" | "long" | "int32" | "int64" => "integer",
        "decimal" | "numeric" | "number" | "money" | "smallmoney" | "dec" => "decimal",
        "real" | "float" | "float4" | "float8" | "double" | "binary_float" | "binary_double" => {
            "float"
        }
        "date" => "date",
        "time" | "timetz" => "time",
        "timestamp" | "datetime" | "datetime2" | "smalldatetime" | "timestamptz"
        | "datetimeoffset" | "date32" | "datetime64" => "timestamp",
        "uuid" | "uniqueidentifier" => "uuid",
        "json" | "jsonb" | "object" | "array" | "struct" | "map" | "list" => "json",
        "blob" | "bytea" | "binary" | "varbinary" | "raw" | "image" | "longblob" => "binary",
        _ => "text",
    }
}

fn converted(src: &str, dst: &str) -> bool {
    let (a, b) = (family(src), family(dst));
    if dst.trim().is_empty() {
        return false;
    }
    let numeric = |f: &str| matches!(f, "integer" | "decimal" | "float");
    !(a == b || (numeric(a) && numeric(b) && !(a != "integer" && b == "integer")))
}

/// `(n)` of `varchar(n)`, `nvarchar(n)`, `char(n)`, `varchar2(n char)`…
fn text_length(type_name: &str) -> Option<u64> {
    let t = type_name.to_ascii_lowercase();
    if family(&t) != "text" || t.contains("max") {
        return None;
    }
    let inner = t.split_once('(')?.1.split(')').next()?;
    inner
        .split([',', ' '])
        .next()?
        .trim()
        .parse()
        .ok()
        .filter(|n| *n > 0)
}

/// `(p, s)` of a decimal type.
fn decimal_scale(type_name: &str) -> Option<u32> {
    let t = type_name.to_ascii_lowercase();
    if family(&t) != "decimal" {
        return None;
    }
    let inner = t.split_once('(')?.1.split(')').next()?;
    let mut parts = inner.split(',').map(|x| x.trim().parse::<u32>().ok());
    let _p = parts.next()??;
    Some(parts.next().flatten().unwrap_or(0))
}

/// What the user should know before writing: values longer than a text
/// column, decimals rounded, fractions or zones dropped.
fn type_warnings(c: &PlannedColumn, max_len: Option<u64>) -> Vec<String> {
    let mut w = Vec::new();
    if let (Some(n), Some(m)) = (text_length(&c.target_type), max_len) {
        if m > n {
            w.push(format!("«{}»: hay valores de hasta {m} caracteres y «{}» es {}: el motor los rechaza o los corta", c.source, c.target, c.target_type));
        }
    }
    let (sf, tf) = (family(&c.source_type), family(&c.target_type));
    if let (Some(s), Some(t)) = (decimal_scale(&c.source_type), decimal_scale(&c.target_type)) {
        if s > t {
            w.push(format!(
                "«{}»: tiene {s} decimales y «{}» guarda {t}: se redondean",
                c.source, c.target
            ));
        }
    }
    if matches!(sf, "float" | "decimal") && tf == "integer" {
        w.push(format!(
            "«{}»: es {} y «{}» es entero: se pierden los decimales",
            c.source, c.source_type, c.target
        ));
    }
    let tz = |t: &str| {
        let t = t.to_ascii_lowercase();
        t.contains("tz") || t.contains("with time zone") || t.contains("offset")
    };
    if sf == "timestamp" && tf == "timestamp" && tz(&c.source_type) && !tz(&c.target_type) {
        w.push(format!(
            "«{}»: tiene zona horaria y «{}» no: se guarda sin ella",
            c.source, c.target
        ));
    }
    w
}

/// The modes the target engine writes: its own, else a plain insert
/// where it bulk loads.
pub fn modes_of(driver: &dyn Driver) -> Vec<WriteMode> {
    let m = driver.write_modes();
    if m.is_empty() && driver.supports_bulk_load() {
        vec![WriteMode::Insert]
    } else {
        m
    }
}

/// "<engine> doesn't write in this mode", one whole message per mode so the
/// mode's name isn't a Spanish word glued into a translated sentence.
fn mode_refused(engine: &str, m: WriteMode) -> String {
    match m {
        WriteMode::Insert => format!("{engine} no escribe en modo «insertar» desde un notebook"),
        WriteMode::Update => {
            format!("{engine} no escribe en modo «actualizar por clave» desde un notebook")
        }
        WriteMode::Upsert => {
            format!("{engine} no escribe en modo «insertar o actualizar» desde un notebook")
        }
        WriteMode::Replace => {
            format!("{engine} no escribe en modo «reemplazar el contenido» desde un notebook")
        }
    }
}

// ---------------------------------------------------------------------------
// The DataFrame and its snapshot

fn q(n: &str) -> String {
    quote_ident(Quote::Double, n)
}

async fn describe(s: &mut dyn Session, table: &str) -> CommandResult<Vec<(String, String)>> {
    let out = exec(s, &format!("DESCRIBE main.{}", q(table))).await?;
    let r = out
        .results
        .iter()
        .find(|r| !r.columns.is_empty())
        .ok_or_else(|| CommandError::Internal("DESCRIBE sin columnas".into()))?;
    let col = |n: &str| {
        r.columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(n))
    };
    let (ni, ti) = (
        col("column_name").unwrap_or(0),
        col("column_type").unwrap_or(1),
    );
    Ok(r.rows
        .iter()
        .map(|row| {
            let t = |i: usize| row.get(i).and_then(Value::as_str).unwrap_or("").to_string();
            (t(ni), t(ti))
        })
        .collect())
}

async fn count(s: &mut dyn Session, sql: &str) -> CommandResult<u64> {
    let out = exec(s, sql).await?;
    Ok(first_value(&out)
        .and_then(|v| {
            v.as_u64()
                .or_else(|| v.as_str().and_then(|x| x.parse().ok()))
        })
        .unwrap_or(0))
}

/// The key columns (DataFrame side) have no repeated and no empty values.
async fn check_keys(s: &mut dyn Session, snapshot: &str, keys: &[String]) -> CommandResult<()> {
    let cols = keys.iter().map(|k| q(k)).collect::<Vec<_>>().join(", ");
    let nulls = keys
        .iter()
        .map(|k| format!("{} IS NULL", q(k)))
        .collect::<Vec<_>>()
        .join(" OR ");
    let n = count(
        s,
        &format!("SELECT count(*) FROM main.{} WHERE {nulls}", q(snapshot)),
    )
    .await?;
    if n > 0 {
        return Err(CommandError::BadRequest(format!(
            "{n} filas del DataFrame no tienen clave ({}): no se pueden buscar en la tabla",
            keys.join(", ")
        )));
    }
    let d = count(
        s,
        &format!(
            "SELECT count(*) FROM (SELECT {cols} FROM main.{} GROUP BY {cols} HAVING count(*) > 1)",
            q(snapshot)
        ),
    )
    .await?;
    if d > 0 {
        return Err(CommandError::BadRequest(format!(
            "el DataFrame repite {d} claves ({}): cada clave tiene que estar una sola vez",
            keys.join(", ")
        )));
    }
    Ok(())
}

/// The DataFrame's rows, page by page from the snapshot, as typed cells.
/// A read that fails never ends the source (that would commit what's
/// written): it raises `failed` and waits, and the write is dropped.
struct Pages<'a> {
    s: &'a mut dyn Session,
    table: String,
    columns: Vec<String>,
    next_row: u64,
    total: u64,
    buf: VecDeque<RowBatch>,
    failed: Arc<(Notify, Mutex<Option<String>>)>,
}

struct Collect(Vec<RowBatch>);
impl BatchSink for Collect {
    fn begin(&mut self, _: &[TransferColumn]) -> std::io::Result<()> {
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> std::io::Result<()> {
        self.0.push(b);
        Ok(())
    }
}

#[dbine_driver::async_trait]
impl BatchSource for Pages<'_> {
    async fn next(&mut self) -> Option<RowBatch> {
        loop {
            if let Some(b) = self.buf.pop_front() {
                return Some(b);
            }
            if self.next_row >= self.total {
                return None;
            }
            let (lo, hi) = (self.next_row, self.next_row + PAGE_ROWS);
            let spec = ReadSpec {
                table: ObjectRef {
                    kind: "table".into(),
                    schema: Some("main".into()),
                    name: self.table.clone(),
                },
                columns: Some(self.columns.clone()),
                filter: Some(format!("rowid >= {lo} AND rowid < {hi}")),
            };
            let sink = Arc::new(Mutex::new(Collect(Vec::new())));
            match self.s.read_batches(&spec, sink.clone()).await {
                Ok(_) => {
                    self.next_row = hi;
                    let got = std::mem::take(
                        &mut sink
                            .lock()
                            .map(|mut g| std::mem::take(&mut g.0))
                            .unwrap_or_default(),
                    );
                    self.buf.extend(got.into_iter().filter(|b| !b.is_empty()));
                }
                Err(e) => {
                    if let Ok(mut g) = self.failed.1.lock() {
                        *g = Some(e.to_string());
                    }
                    self.failed.0.notify_one();
                    std::future::pending::<()>().await;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The target

fn table_ref(t: &WriteTarget) -> ObjectRef {
    ObjectRef {
        kind: "table".into(),
        schema: t.schema.clone().filter(|s| !s.trim().is_empty()),
        name: t.table.trim().to_string(),
    }
}

fn qualified(t: &ObjectRef) -> String {
    match t.schema() {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    }
}

/// The connection may be written to: it exists and isn't read-only.
fn writable(
    state: &AppState,
    connection_id: &str,
) -> CommandResult<dbine_core::state::SavedConnection> {
    let c = state
        .store
        .get_connection(connection_id)?
        .ok_or_else(|| CommandError::NotFound("conexión inexistente".into()))?;
    if c.config.read_only {
        return Err(CommandError::BadRequest(format!(
            "«{}» es de solo lectura: un notebook no escribe en ella",
            c.name
        )));
    }
    Ok(c)
}

/// What the write cell's form needs of a target: its modes and, for an
/// existing table, its columns.
#[derive(Debug, Clone, Serialize)]
pub struct TargetInfo {
    pub modes: Vec<WriteMode>,
    pub atomic: bool,
    /// Documents or keys: any field name goes.
    pub schemaless: bool,
    /// `None`: no table given, or it doesn't exist.
    pub columns: Option<Vec<dbine_driver::ColumnInfo>>,
}

pub async fn target_info(
    state: &AppState,
    tab: &str,
    target: &WriteTarget,
) -> CommandResult<TargetInfo> {
    writable(state, &target.connection_id)?;
    let driver = crate::commands::schema::driver_of(state, &target.connection_id)?;
    let modes = modes_of(driver.as_ref());
    let schemaless = driver.info().language != dbine_driver::Language::Sql;
    let atomic = !driver.write_modes().is_empty() && driver.supports_manual_transactions();
    let mut columns = None;
    if !target.table.trim().is_empty() {
        let key = format!("nbw-info:{tab}");
        let entry = state
            .dedicated_session(&key, &target.connection_id, &target.database, true)
            .await?;
        let r = entry.session.lock().await.columns(&table_ref(target)).await;
        state
            .sessions
            .remove_if(&key, |_, e| Arc::ptr_eq(e, &entry));
        columns = r.ok().filter(|c| !c.is_empty());
    }
    Ok(TargetInfo {
        modes,
        atomic,
        schemaless,
        columns,
    })
}

/// The table "crear tabla nueva" makes: the DataFrame's columns in the
/// target's types (dbine-schema's conversion from DuckDB), the keys as its
/// primary key. Returns the statements, the table as created and each
/// DataFrame column's name and type there.
fn create_plan(
    driver: &dyn Driver,
    target: &WriteTarget,
    cols: &[(String, String)],
    keys: &[String],
) -> CommandResult<(Vec<String>, ObjectRef, Vec<(String, String, String)>)> {
    let t = table_ref(target);
    let table = TableSchema {
        kind: "table".into(),
        schema: t.schema.clone(),
        name: t.name.clone(),
        columns: cols
            .iter()
            .map(|(n, ty)| ColumnDef {
                name: n.clone(),
                data_type: ty.clone(),
                nullable: !keys.iter().any(|k| k.eq_ignore_ascii_case(n)),
                ..Default::default()
            })
            .collect(),
        primary_key: (!keys.is_empty()).then(|| KeyDef {
            name: None,
            columns: keys.to_vec(),
        }),
        ..Default::default()
    };
    let opts = dbine_schema::Options {
        target_schema: t.schema.clone(),
        ..Default::default()
    };
    let conv = dbine_schema::convert(&[table], "duckdb", driver.info().id, &opts)
        .map_err(|e| CommandError::BadRequest(e.to_string()))?;
    let made = conv
        .tables
        .into_iter()
        .next()
        .ok_or_else(|| CommandError::Internal("la conversión no dio la tabla".into()))?;
    let ddl = driver
        .table_ddl(
            &made,
            DdlParts {
                create: true,
                ..Default::default()
            },
        )
        .map_err(|e| {
            CommandError::BadRequest(format!("no se puede crear la tabla en este motor: {e}"))
        })?;
    let stmts: Vec<String> = driver
        .split_script(&ddl)
        .into_iter()
        .map(|s| s.text.trim().trim_end_matches(';').trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let mapped: Vec<(String, String, String)> = cols
        .iter()
        .map(|(n, _)| {
            let to = conv
                .columns
                .iter()
                .find(|m| m.column == *n)
                .map(|m| m.target_column.clone())
                .unwrap_or_else(|| n.clone());
            let ty = made
                .columns
                .iter()
                .find(|c| c.name == to)
                .map(|c| c.data_type.clone())
                .unwrap_or_default();
            (n.clone(), to, ty)
        })
        .collect();
    let created = ObjectRef {
        kind: "table".into(),
        schema: made.schema.clone().or(t.schema),
        name: made.name.clone(),
    };
    Ok((stmts, created, mapped))
}

// ---------------------------------------------------------------------------
// Prepare

/// Load the DataFrame, check the target and the mapping, and issue the
/// confirmation's token (see the module docs). Nothing is written.
pub async fn prepare(
    state: &AppState,
    a: &WriteArgs,
    policy: super::access::Policy,
    net: super::remote::Net,
) -> CommandResult<PrepareResponse> {
    let conn = writable(state, &a.target.connection_id)?;
    let driver = crate::commands::schema::driver_of(state, &a.target.connection_id)?;
    let modes = modes_of(driver.as_ref());
    if !modes.contains(&a.mode) {
        return Err(CommandError::BadRequest(if modes.is_empty() {
            format!("{} no escribe datos desde un notebook", driver.info().name)
        } else {
            mode_refused(&driver.info().name, a.mode)
        }));
    }
    if a.target.table.trim().is_empty() {
        return Err(CommandError::BadRequest("elegí la tabla de destino".into()));
    }
    if a.mode.needs_key() && a.keys.is_empty() {
        return Err(CommandError::BadRequest(
            "elegí la columna clave para actualizar por clave".into(),
        ));
    }
    if !super::local::valid_name(&a.source)
        || !a
            .sources
            .iter()
            .any(|s| s.name.eq_ignore_ascii_case(&a.source))
    {
        return Err(CommandError::BadRequest(format!(
            "no hay una celda «{}» arriba de esta",
            a.source
        )));
    }
    let cap = a
        .row_cap
        .unwrap_or(super::local::DEFAULT_ROW_CAP)
        .clamp(ROW_CAP_RANGE.0, ROW_CAP_RANGE.1);
    let sql = format!("SELECT * FROM {}", q(&a.source));
    let (entry, loaded) = match stage(
        state,
        &a.tab_id,
        &sql,
        cap,
        &a.sources,
        a.notebook_path.as_deref(),
        a.approval.as_deref(),
        policy,
        net,
    )
    .await?
    {
        Staged::Ready { entry, loaded, .. } => (entry, loaded),
        Staged::Ask(r) => {
            return Ok(PrepareResponse {
                plan: None,
                loaded: Vec::new(),
                approval: Some(r),
            })
        }
    };
    // The whole DataFrame, or nothing.
    if let Some(t) = loaded.iter().find(|t| t.capped) {
        return Err(CommandError::BadRequest(format!(
            "«{}» se cargó cortada en {} filas (el tope por tabla de Configuración › Notebooks): no se escribe un DataFrame incompleto. Subí el tope o filtrá antes",
            t.name, t.rows
        )));
    }
    let snapshot = format!(
        "__dbine_w_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..16]
    );
    let mut warnings = Vec::new();
    let r = {
        let mut s = entry.session.lock().await;
        snapshot_and_check(&mut **s, state, a, &driver, &conn, &snapshot, &mut warnings).await
    };
    let (plan_pending, preview, unmatched_source, unmatched_target) = match r {
        Ok(x) => x,
        Err(e) => {
            drop_snapshot(&entry, &snapshot).await;
            return Err(e);
        }
    };
    let atomic = !driver.write_modes().is_empty() && driver.supports_manual_transactions();
    let p = plan_pending;
    let plan = WritePlan {
        token: String::new(),
        rows: p.rows,
        target_label: p.label.clone(),
        connection: conn.name.clone(),
        database: a.target.database.clone(),
        table: qualified(&p.table),
        engine: driver.info().id.to_string(),
        columns: p.pairs.clone(),
        unmatched_source,
        unmatched_target,
        keys: a.keys.clone(),
        preview,
        warnings: p.warnings.clone(),
        create: p.create.as_ref().map(|c| c.join(";\n")),
        atomic,
    };
    let (token, old) = issue(p)?;
    if let Some(old) = old {
        drop_snapshot(&entry, &old).await;
    }
    Ok(PrepareResponse {
        plan: Some(WritePlan { token, ..plan }),
        loaded,
        approval: None,
    })
}

type Checked = (Pending, Preview, Vec<String>, Vec<String>);

#[allow(clippy::too_many_arguments)]
async fn snapshot_and_check(
    s: &mut dyn Session,
    state: &AppState,
    a: &WriteArgs,
    driver: &Arc<dyn Driver>,
    conn: &dbine_core::state::SavedConnection,
    snapshot: &str,
    warnings: &mut Vec<String>,
) -> CommandResult<Checked> {
    exec(
        s,
        &format!(
            "CREATE OR REPLACE TABLE main.{} AS SELECT * FROM main.{}",
            q(snapshot),
            q(&a.source)
        ),
    )
    .await?;
    let rows = count(s, &format!("SELECT count(*) FROM main.{}", q(snapshot))).await?;
    let src_cols = describe(s, snapshot).await?;
    let src_names: Vec<String> = src_cols.iter().map(|(n, _)| n.clone()).collect();
    let schemaless = driver.info().language != dbine_driver::Language::Sql;

    // The target's columns (or the table "crear tabla nueva" makes).
    let key = format!("nbw-plan:{}", a.tab_id);
    let entry = state
        .dedicated_session(&key, &a.target.connection_id, &a.target.database, true)
        .await?;
    let existing = entry
        .session
        .lock()
        .await
        .columns(&table_ref(&a.target))
        .await;
    state
        .sessions
        .remove_if(&key, |_, e| Arc::ptr_eq(e, &entry));
    let existing = existing.ok().filter(|c| !c.is_empty());

    let (table, create, pairs): (ObjectRef, Option<Vec<String>>, Vec<PlannedColumn>) = if a
        .target
        .create
    {
        if existing.is_some() && !schemaless {
            return Err(CommandError::BadRequest(format!(
                "la tabla «{}» ya existe: elegila como tabla existente",
                qualified(&table_ref(&a.target))
            )));
        }
        // Only the columns the mapping keeps (all of them by default).
        let keep: Vec<(String, String)> = match &a.mapping {
            None => src_cols.clone(),
            Some(m) => src_cols
                .iter()
                .filter(|(n, _)| m.iter().any(|p| p.source.eq_ignore_ascii_case(n)))
                .cloned()
                .collect(),
        };
        let key_src: Vec<String> = a
            .keys
            .iter()
            .map(|k| {
                src_names
                    .iter()
                    .find(|n| n.eq_ignore_ascii_case(k))
                    .cloned()
                    .unwrap_or_else(|| k.clone())
            })
            .collect();
        let (stmts, created, mapped) = create_plan(driver.as_ref(), &a.target, &keep, &key_src)?;
        let pairs = mapped
            .into_iter()
            .map(|(src, to, ty)| {
                let st = src_cols
                    .iter()
                    .find(|(n, _)| *n == src)
                    .map(|(_, t)| t.clone())
                    .unwrap_or_default();
                PlannedColumn {
                    converted: converted(&st, &ty),
                    source: src,
                    source_type: st,
                    target: to,
                    target_type: ty,
                }
            })
            .collect();
        // A schemaless target that exists already (a collection) is written as it is.
        match existing {
            None => (created, Some(stmts), pairs),
            Some(_) => (table_ref(&a.target), None, pairs),
        }
    } else {
        let Some(cols) = existing.as_ref() else {
            if schemaless {
                // A collection that doesn't exist yet is created by the first write.
                let pairs = pair_columns(&src_names, &[], a.mapping.as_deref(), true)?;
                let planned = pairs
                    .into_iter()
                    .map(|p| {
                        let st = src_cols
                            .iter()
                            .find(|(n, _)| *n == p.source)
                            .map(|(_, t)| t.clone())
                            .unwrap_or_default();
                        PlannedColumn {
                            source: p.source,
                            source_type: st,
                            target: p.target,
                            target_type: String::new(),
                            converted: false,
                        }
                    })
                    .collect();
                return finish(
                    s,
                    a,
                    conn,
                    snapshot,
                    rows,
                    table_ref(&a.target),
                    None,
                    planned,
                    &src_names,
                    &[],
                    warnings,
                )
                .await;
            }
            return Err(CommandError::BadRequest(format!("no existe la tabla «{}» (o no se puede leer): elegí otra o marcá «crear tabla nueva»", qualified(&table_ref(&a.target)))));
        };
        let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
        let pairs = pair_columns(&src_names, &names, a.mapping.as_deref(), schemaless)?;
        let planned = pairs
            .into_iter()
            .map(|p| {
                let st = src_cols
                    .iter()
                    .find(|(n, _)| *n == p.source)
                    .map(|(_, t)| t.clone())
                    .unwrap_or_default();
                let tt = cols
                    .iter()
                    .find(|c| c.name == p.target)
                    .map(|c| c.data_type.clone())
                    .unwrap_or_default();
                PlannedColumn {
                    converted: converted(&st, &tt),
                    source: p.source,
                    source_type: st,
                    target: p.target,
                    target_type: tt,
                }
            })
            .collect();
        (table_ref(&a.target), None, planned)
    };
    let target_names: Vec<String> = existing
        .as_ref()
        .map(|c| c.iter().map(|x| x.name.clone()).collect())
        .unwrap_or_default();
    finish(
        s,
        a,
        conn,
        snapshot,
        rows,
        table,
        create,
        pairs,
        &src_names,
        &target_names,
        warnings,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn finish(
    s: &mut dyn Session,
    a: &WriteArgs,
    conn: &dbine_core::state::SavedConnection,
    snapshot: &str,
    rows: u64,
    table: ObjectRef,
    create: Option<Vec<String>>,
    pairs: Vec<PlannedColumn>,
    src_names: &[String],
    target_names: &[String],
    warnings: &mut Vec<String>,
) -> CommandResult<Checked> {
    // The keys: target columns that are written; the DataFrame's side has
    // each key once and never empty.
    let mut key_src = Vec::new();
    for k in &a.keys {
        let p = pairs
            .iter()
            .find(|p| {
                p.target.eq_ignore_ascii_case(k)
                    || (create.is_some() && p.source.eq_ignore_ascii_case(k))
            })
            .ok_or_else(|| {
                CommandError::BadRequest(format!(
                    "la clave «{k}» no es una de las columnas que se escriben"
                ))
            })?;
        key_src.push(p.source.clone());
    }
    if a.mode.needs_key() {
        check_keys(s, snapshot, &key_src).await?;
    }
    for p in &pairs {
        let max_len = if text_length(&p.target_type).is_some() {
            Some(
                count(
                    s,
                    &format!(
                        "SELECT coalesce(max(length(CAST({} AS VARCHAR))), 0) FROM main.{}",
                        q(&p.source),
                        q(snapshot)
                    ),
                )
                .await?,
            )
        } else {
            None
        };
        warnings.extend(type_warnings(p, max_len));
    }
    if rows == 0 {
        warnings.push(match a.mode {
            WriteMode::Replace => {
                "el DataFrame no tiene filas: reemplazar deja la tabla vacía".to_string()
            }
            _ => "el DataFrame no tiene filas: no se escribe nada".to_string(),
        });
    }
    let cols = pairs
        .iter()
        .map(|p| q(&p.source))
        .collect::<Vec<_>>()
        .join(", ");
    let out = exec(
        s,
        &format!(
            "SELECT {cols} FROM main.{} ORDER BY rowid LIMIT {PREVIEW_ROWS}",
            q(snapshot)
        ),
    )
    .await?;
    let preview_rows = out
        .results
        .iter()
        .find(|r| !r.columns.is_empty())
        .map(|r| r.rows.clone())
        .unwrap_or_default();
    let preview = Preview {
        columns: pairs.iter().map(|p| p.target.clone()).collect(),
        rows: preview_rows,
    };
    let unmatched_source = src_names
        .iter()
        .filter(|n| !pairs.iter().any(|p| p.source == **n))
        .cloned()
        .collect();
    let unmatched_target = target_names
        .iter()
        .filter(|n| !pairs.iter().any(|p| p.target == **n))
        .cloned()
        .collect();
    let db = (!a.target.database.is_empty())
        .then(|| format!(" › {}", a.target.database))
        .unwrap_or_default();
    let label = format!("{}{db} › {}", conn.name, qualified(&table));
    let p = Pending {
        tab: a.tab_id.clone(),
        cell: a.cell_id.clone(),
        bind: binding(a, rows),
        snapshot: snapshot.to_string(),
        rows,
        pairs,
        table,
        create,
        label,
        warnings: warnings.clone(),
        at: Instant::now(),
    };
    Ok((p, preview, unmatched_source, unmatched_target))
}

async fn drop_snapshot(entry: &SessionEntry, snapshot: &str) {
    let mut s = entry.session.lock().await;
    let _ = exec(
        &mut **s,
        &format!("DROP TABLE IF EXISTS main.{}", q(snapshot)),
    )
    .await;
}

// ---------------------------------------------------------------------------
// Run

/// The session key a write runs on (Cancel stops it there).
pub fn write_session_key(tab: &str) -> String {
    format!("nbw:{tab}")
}

/// Write what `token` stands for (see the module docs). `rows`: the count
/// the user confirmed.
pub async fn run(
    state: &AppState,
    a: &WriteArgs,
    token: &str,
    rows: u64,
) -> CommandResult<WriteOutcome> {
    let p = take(token, &a.tab_id, &a.cell_id, &binding(a, rows))?;
    let started = Instant::now();
    let duck = state
        .sessions
        .get(&session_key(&a.tab_id))
        .map(|e| e.clone());
    let Some(duck) = duck else {
        return Err(CommandError::BadRequest("el DataFrame ya no está cargado (se cerró la sesión del notebook): ejecutá la celda de nuevo".into()));
    };
    let r = write(state, a, &p, &duck).await;
    drop_snapshot(&duck, &p.snapshot).await;
    let (res, created) = r?;
    let mut warnings = p.warnings.clone();
    warnings.extend(res.notes);
    Ok(WriteOutcome {
        inserted: res.inserted,
        updated: res.updated,
        deleted: res.deleted,
        atomic: res.atomic,
        elapsed_ms: started.elapsed().as_millis() as u64,
        target_label: p.label,
        warnings,
        created,
    })
}

async fn write(
    state: &AppState,
    a: &WriteArgs,
    p: &Pending,
    duck: &Arc<SessionEntry>,
) -> CommandResult<(WriteResult, bool)> {
    // Checked again: the connection may have turned read-only since.
    writable(state, &a.target.connection_id)?;
    let driver = crate::commands::schema::driver_of(state, &a.target.connection_id)?;
    let modes = modes_of(driver.as_ref());
    if !modes.contains(&a.mode) {
        return Err(CommandError::BadRequest(mode_refused(
            &driver.info().name,
            a.mode,
        )));
    }
    let mut ds = duck.session.lock().await;
    let n = count(
        &mut **ds,
        &format!("SELECT count(*) FROM main.{}", q(&p.snapshot)),
    )
    .await?;
    if n != p.rows {
        return Err(CommandError::BadRequest(format!("el DataFrame cambió desde que se confirmó ({} filas, ahora {n}): ejecutá la celda de nuevo", p.rows)));
    }
    let key_src: Vec<String> = a
        .keys
        .iter()
        .filter_map(|k| {
            p.pairs
                .iter()
                .find(|c| c.target.eq_ignore_ascii_case(k) || c.source.eq_ignore_ascii_case(k))
                .map(|c| c.source.clone())
        })
        .collect();
    if a.mode.needs_key() {
        check_keys(&mut **ds, &p.snapshot, &key_src).await?;
    }
    let types: HashMap<String, String> = describe(&mut **ds, &p.snapshot)
        .await?
        .into_iter()
        .collect();
    let columns: Vec<TransferColumn> = p
        .pairs
        .iter()
        .map(|c| TransferColumn {
            name: c.source.clone(),
            type_name: types.get(&c.source).cloned().unwrap_or_default(),
            nullable: true,
        })
        .collect();
    let spec = WriteSpec {
        table: p.table.clone(),
        columns: p.pairs.iter().map(|c| c.target.clone()).collect(),
        key: a
            .keys
            .iter()
            .filter_map(|k| {
                p.pairs
                    .iter()
                    .find(|c| c.target.eq_ignore_ascii_case(k) || c.source.eq_ignore_ascii_case(k))
                    .map(|c| c.target.clone())
            })
            .collect(),
        mode: a.mode,
    };
    spec.check()
        .map_err(|e| CommandError::BadRequest(e.to_string()))?;

    let key = write_session_key(&a.tab_id);
    let entry = state
        .dedicated_session(&key, &a.target.connection_id, &a.target.database, false)
        .await?;
    let result = async {
        let mut ts = entry.session.lock().await;
        let mut created = false;
        if let Some(stmts) = &p.create {
            for st in stmts {
                let mut out = QueryOutcome::default();
                ts.execute(st, 0, &mut out)
                    .await
                    .map_err(|e| CommandError::Sql(format!("no se pudo crear la tabla: {e}")))?;
                if let Some(e) = out
                    .error
                    .take()
                    .or_else(|| out.errors.first().map(|e| e.message.clone()))
                {
                    return Err(CommandError::Sql(format!("no se pudo crear la tabla: {e}")));
                }
            }
            created = true;
        }
        let failed: Arc<(Notify, Mutex<Option<String>>)> =
            Arc::new((Notify::new(), Mutex::new(None)));
        let mut src = Pages {
            s: &mut **ds,
            table: p.snapshot.clone(),
            columns: p.pairs.iter().map(|c| c.source.clone()).collect(),
            next_row: 0,
            total: p.rows,
            buf: VecDeque::new(),
            failed: failed.clone(),
        };
        let written = std::sync::atomic::AtomicU64::new(0);
        let progress = |n: u64| written.store(n, std::sync::atomic::Ordering::Relaxed);
        let staged = !driver.write_modes().is_empty();
        let work = async {
            if staged {
                ts.write_rows(&spec, &columns, &mut src, &progress).await
            } else {
                let load = dbine_driver::LoadSpec {
                    table: spec.table.clone(),
                    columns: spec.columns.clone(),
                    table_lock: false,
                    keep_identity: true,
                    commit_rows: dbine_driver::LoadSpec::DEFAULT_COMMIT_ROWS,
                    commit_bytes: dbine_driver::LoadSpec::DEFAULT_COMMIT_BYTES,
                };
                ts.bulk_load(&load, &columns, &mut src, &progress)
                    .await
                    .map(|n| WriteResult {
                        inserted: n,
                        atomic: false,
                        ..Default::default()
                    })
            }
        };
        let r = tokio::select! {
            r = work => Some(r),
            _ = failed.0.notified() => None,
            _ = entry.cancel.notified() => None,
        };
        let so_far = written.load(std::sync::atomic::Ordering::Relaxed);
        let read_error = failed.1.lock().ok().and_then(|g| g.clone());
        let lost = |what: String| {
            if staged && driver.supports_manual_transactions() {
                format!("No se escribió nada (la transacción se deshizo). {what}")
            } else if so_far > 0 {
                format!("Ya se habían escrito {so_far} filas, que quedan en la tabla. {what}")
            } else {
                what
            }
        };
        match r {
            Some(Ok(res)) => Ok((res, created)),
            Some(Err(dbine_driver::Error::Cancelled)) => {
                Err(CommandError::Sql(lost("se canceló la escritura".into())))
            }
            Some(Err(e)) => Err(CommandError::Sql(lost(e.to_string()))),
            None => match read_error {
                Some(e) => Err(CommandError::Sql(lost(format!(
                    "no se pudo leer el DataFrame: {e}"
                )))),
                None => Err(CommandError::Sql(lost("se canceló la escritura".into()))),
            },
        }
    }
    .await;
    // The session goes: a write cut short leaves no transaction open (the
    // server rolls it back when the connection closes).
    state
        .sessions
        .remove_if(&key, |_, e| Arc::ptr_eq(e, &entry));
    result
}

#[cfg(test)]
#[path = "write_tests.rs"]
mod tests;
