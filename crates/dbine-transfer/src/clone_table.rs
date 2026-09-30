//! "Clonar tabla": a copy of one table next to it, in the same database,
//! under a new name: its structure (columns, primary key, checks, indexes,
//! outgoing foreign keys) and, optionally, its rows.
//!
//! The clone is exact or it doesn't exist: every step that could leave it
//! different from the original (a structure the engine doesn't report
//! whole, a column that comes out different, an index or foreign key that
//! can't be created, a copy cut halfway, a cancel) drops what was created
//! and says why. The only thing never touched is the original.
//!
//! Steps ([`clone_table`]):
//! 1. the engine and the object's kind must be cloneable ([`check_cloneable`]);
//! 2. the table's [`TableSchema`] (the driver's `database_schema`, or its
//!    columns when the engine reports nothing else);
//! 3. [`plan_clone`]: the schema under the new name, with every named
//!    constraint and index renamed so it never collides with the
//!    original's (engines where those names are schema-wide), within the
//!    engine's identifier length; all the DDL is generated before anything
//!    is written;
//! 4. the new name must be free (compared ignoring case);
//! 5. `CREATE` (columns, primary key, checks), then its columns are
//!    compared with the original's;
//! 6. the rows, as one [`TransferJob`] of the bulk transfer engine (native
//!    copy, bulk load or `insert_script`), keeping identity values; then
//!    the identity / sequence moves past them;
//! 7. indexes, then outgoing foreign keys (pointing to the same parents; a
//!    self reference points to the clone).

use crate::engine::{Control, Endpoints, Engine};
use crate::event::{Event, LogLevel};
use crate::job::{RunOptions, TransferJob, TransferMode};
use crate::state::{Store, TableStatus};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{
    async_trait, kinds, ColumnDef, ConnectionConfig, DdlParts, Driver, Error, Family, KeyDef, Language, LoadSpec, ObjectRef, QueryOutcome, ReadSpec,
    Result, Session, TableSchema, TransferColumn,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

mod analytics;
pub use analytics::{definition_differences, engine_columns, EngineColumns};
mod documents;
mod embedded;
mod mssql;
mod mysql;
mod oracle;
mod pg;
mod triggers;
mod timeseries;

/// What to clone besides the structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CloneOptions {
    /// Copy the rows ("Copiar los datos").
    pub with_data: bool,
    /// Create the indexes and unique constraints ("Incluir índices").
    pub with_indexes: bool,
}

impl Default for CloneOptions {
    fn default() -> Self {
        CloneOptions { with_data: true, with_indexes: true }
    }
}

/// One clone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloneRequest {
    /// The table (collection…) to clone.
    pub source: ObjectRef,
    /// The clone's name, in the same schema.
    pub new_name: String,
    #[serde(default)]
    pub options: CloneOptions,
}

/// Where a clone is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClonePhase {
    /// Reading the original's structure.
    Read,
    Create,
    Copy,
    /// Moving the identity / sequence past the copied values.
    Identity,
    Indexes,
    ForeignKeys,
    /// Dropping a clone left halfway.
    Cleanup,
}

/// A clone's news, in order.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum CloneEvent {
    Phase { phase: ClonePhase },
    /// Committed rows (at most one every [`crate::PROGRESS_EVERY`], plus the
    /// final one).
    Progress { rows_done: u64, rows_total: Option<u64>, rows_per_s: f64 },
    Log { level: LogLevel, text: String },
}

/// A name the clone gave to one of its constraints or indexes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rename {
    pub from: String,
    pub to: String,
    /// Shortened (with a hash) to fit the engine's identifier length.
    pub shortened: bool,
}

/// How a clone ended.
#[derive(Debug, Clone, Serialize)]
pub struct CloneReport {
    /// The clone.
    pub table: ObjectRef,
    pub rows: u64,
    pub elapsed_ms: u64,
    /// What the user should know (renamed constraints, omitted parts…),
    /// in Spanish.
    pub notes: Vec<String>,
    pub renames: Vec<Rename>,
}

/// Cancels a running clone. Cheap to clone.
#[derive(Clone, Default)]
pub struct CloneControl {
    inner: Arc<ControlInner>,
}

#[derive(Default)]
struct ControlInner {
    cancelled: AtomicBool,
    engine: Mutex<Option<Control>>,
}

impl CloneControl {
    /// Stop: the clone is dropped (the original is never touched).
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        if let Some(c) = crate::lock(&self.inner.engine).as_ref() {
            c.cancel_all();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }

    fn set_engine(&self, c: Option<Control>) {
        let cancelled = self.is_cancelled();
        if let (true, Some(c)) = (cancelled, c.as_ref()) {
            c.cancel_all();
        }
        *crate::lock(&self.inner.engine) = c;
    }
}

/// The name the UI proposes: `<name>_yyyyMMdd_HHmmss`.
pub fn default_clone_name(name: &str, at: chrono::NaiveDateTime) -> String {
    format!("{name}_{}", at.format("%Y%m%d_%H%M%S"))
}

/// [`default_clone_name`] within the engine's identifier limit: a long
/// name is cut before the suffix (the UI mirrors this in
/// `web/src/api/cloneTable.ts`), so the proposal is never refused.
pub fn default_clone_name_for(driver: &dyn Driver, name: &str, at: chrono::NaiveDateTime) -> String {
    let suffix = format!("_{}", at.format("%Y%m%d_%H%M%S"));
    let limit = identifier_limit(driver);
    if limit == 0 || name_length(driver, &format!("{name}{suffix}")) <= limit {
        return format!("{name}{suffix}");
    }
    let room = limit.saturating_sub(suffix.len());
    let base: String = if counts_characters(driver) { name.chars().take(room).collect() } else { cut(name, room).to_string() };
    format!("{base}{suffix}")
}

/// Engines whose identifier limit is in characters (SQL Server's sysname,
/// MySQL's 64, Firebird's 63 from Firebird 4 on; Firebird 3 refuses a
/// longer name itself); elsewhere it's in bytes (PostgreSQL's 63).
fn counts_characters(driver: &dyn Driver) -> bool {
    matches!(driver.info().dialect, "mssql" | "mysql") || driver.info().id == "firebird"
}

/// A name's length as the engine's limit counts it.
fn name_length(driver: &dyn Driver, name: &str) -> usize {
    if counts_characters(driver) {
        name.chars().count()
    } else {
        name.len()
    }
}

// -- what can be cloned ---------------------------------------------------------------------------

/// Kinds that are not stored rows of their own (their clone would be a
/// different thing): views, files, aliases, streams, dictionaries…
const NOT_TABLES: &[(&str, &str)] = &[
    (kinds::VIEW, "una vista no guarda filas propias: se copiaría su resultado, no la vista"),
    (kinds::MATERIALIZED_VIEW, "una vista materializada depende de su consulta: se copiaría su resultado, no la vista"),
    (kinds::STREAM, "un stream depende de su origen: clonarlo no copia sus datos de forma fiel"),
    (kinds::TOPIC, "un topic no es una tabla"),
    ("virtual_table", "una tabla virtual depende de su módulo (FTS, R*Tree…) y de sus tablas internas"),
    ("alias", "un alias apunta a otros índices: no tiene datos propios"),
    ("dictionary", "un diccionario se carga desde su origen: no tiene datos propios"),
    ("source", "una fuente lee de un sistema externo: no tiene datos propios"),
    ("file", "un archivo no es una tabla de la base"),
];

/// Whether this engine can clone objects of `kind` (before touching
/// anything). `Unsupported` with the reason, in Spanish.
pub fn check_cloneable(driver: &dyn Driver, kind: &str) -> Result<()> {
    let info = driver.info();
    let why = |r: &str| Err(Error::Unsupported(format!("no se puede clonar: {r}")));
    if let Some((_, r)) = NOT_TABLES.iter().find(|(k, _)| *k == kind) {
        return why(r);
    }
    if let Some(r) = timeseries::refused(info.id, kind) {
        return why(r);
    }
    match info.family {
        Family::Graph => return why("en un motor de grafos, los nodos de una etiqueta no se copian sin sus relaciones"),
        Family::KeyValue => return why("un motor clave-valor no tiene tablas; cada clave es un valor suelto"),
        Family::Streaming => return why("los datos de este motor viven en sus topics: clonar el objeto no copia sus mensajes de forma fiel"),
        _ => {}
    }
    if info.id == "couchdb" {
        // Its only "table" (`_all_docs`) is the database itself.
        return why("en CouchDB los documentos son la base entera, no una tabla dentro de ella; para copiarlos, creá otra base y usá «Migrar…»");
    }
    if let Some(k) = info.object_kinds.iter().find(|k| k.id == kind) {
        if !k.has_columns || !k.browsable {
            return why("este tipo de objeto no tiene filas que copiar");
        }
    }
    Ok(())
}

// -- names ------------------------------------------------------------------------------------------

/// The longest identifier the engine takes (bytes), 0 for no practical
/// limit. Used to refuse a clone name that the engine would cut silently
/// (PostgreSQL truncates to 63 bytes with only a notice).
pub fn identifier_limit(driver: &dyn Driver) -> usize {
    let info = driver.info();
    match info.id {
        "firebird" => return 63,
        // Its dialect is "standard" (it would get 128 below).
        "duckdb" => return 0,
        "cassandra" | "scylladb" => return 48,
        "altibase" => return 40,
        // StarRocks takes 1024 characters; GreptimeDB has no limit.
        "starrocks" => return 1024,
        "greptimedb" => return 0,
        _ => {}
    }
    match info.dialect {
        "postgres" => 63,
        "mysql" => 64,
        "access" => 64,
        "oracle" => 128,
        "hana" => 127,
        "sqlite" | "clickhouse" | "duckdb" => 0,
        "snowflake" => 255,
        "bigquery" => 1024,
        _ if matches!(info.language, Language::Sql | Language::Cql) => 128,
        _ => 0,
    }
}

/// The limit for the names the clone makes up (constraints, indexes):
/// conservative where older versions of the engine took less (Oracle
/// before 12.2: 30; Firebird 3: 31). Shorter is always valid.
fn generated_limit(driver: &dyn Driver) -> usize {
    let info = driver.info();
    if info.id == "firebird" {
        return 31;
    }
    if info.id == "tidb" {
        // A functional index's hidden column is `_V$_<index>_<n>` and
        // must fit in 64 too.
        return 56;
    }
    match info.dialect {
        "oracle" => 30,
        _ => identifier_limit(driver),
    }
}

/// FNV-1a, 32 bits: a short, stable suffix for shortened names.
fn fnv(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// `s` cut to at most `max` bytes, on a character boundary.
fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// `s` cut to at most `max` characters (`chars`) or bytes.
fn cut_to(s: &str, max: usize, chars: bool) -> &str {
    match s.char_indices().nth(max).filter(|_| chars) {
        Some((i, _)) => &s[..i],
        None if chars => s,
        None => cut(s, max),
    }
}

/// A constraint's / index's name for the clone: the original table's name
/// inside it replaced by the clone's (`pk_clientes` → `pk_clientes_2026…`),
/// or the clone's name put in front when it isn't there. Longer than `max`
/// bytes (0: no limit): cut and ended with `_` and a hash of the whole name,
/// so two long names never end up equal.
pub fn rename_constraint(name: &str, old_table: &str, new_table: &str, max: usize) -> (String, bool) {
    rename_within(name, old_table, new_table, max, false)
}

/// [`rename_constraint`], with `max` in characters when `chars` (SQL
/// Server's sysname).
fn rename_within(name: &str, old_table: &str, new_table: &str, max: usize, chars: bool) -> (String, bool) {
    rename_capped(name, old_table, new_table, max, chars, 0)
}

/// TiDB takes 64 characters for tables, indexes and foreign keys, but only
/// 64 bytes for a CHECK's name: every name the clone makes up there fits
/// both (shorter is always valid).
pub(super) fn byte_cap(driver_id: &str) -> usize {
    if driver_id == "tidb" { 64 } else { 0 }
}

/// `s` within `max` (characters when `chars`, else bytes; 0: none) and
/// within `bytes` bytes (0: none).
pub(super) fn fit(s: &str, max: usize, chars: bool, bytes: usize) -> &str {
    let s = if max > 0 { cut_to(s, max, chars) } else { s };
    if bytes > 0 { cut(s, bytes) } else { s }
}

/// [`rename_within`], also within `bytes` bytes (0: none; see [`byte_cap`]).
pub(super) fn rename_capped(name: &str, old_table: &str, new_table: &str, max: usize, chars: bool, bytes: usize) -> (String, bool) {
    let lower = name.to_ascii_lowercase();
    let full = match lower.find(&old_table.to_ascii_lowercase()).filter(|_| !old_table.is_empty()) {
        Some(i) => format!("{}{}{}", &name[..i], new_table, &name[i + old_table.len()..]),
        None => format!("{new_table}_{name}"),
    };
    let len = if chars { full.chars().count() } else { full.len() };
    if (max == 0 || len <= max) && (bytes == 0 || full.len() <= bytes) {
        return (full, false);
    }
    let suffix = format!("_{:08x}", fnv(&full));
    let base = fit(&full, max.saturating_sub(suffix.len()), chars, bytes.saturating_sub(suffix.len()));
    (format!("{base}{suffix}"), true)
}

/// The unit of the limit in the "acortado" notes.
pub(super) fn limit_text(max: usize, chars: bool, bytes: usize) -> String {
    match (chars, bytes) {
        (true, b) if b > 0 => format!("{max} caracteres y {b} bytes"),
        (true, _) => format!("{max} caracteres"),
        _ => format!("{max} bytes"),
    }
}

/// The original's structure under the new name, ready to create.
#[derive(Debug, Clone)]
pub struct ClonePlan {
    pub table: TableSchema,
    pub renames: Vec<Rename>,
    pub notes: Vec<String>,
}

/// [`ColumnDef::options`] key with the name of the column's DEFAULT
/// constraint (the SQL Server driver writes it as `CONSTRAINT [n] DEFAULT`):
/// renamed like the other constraints.
pub const DEFAULT_NAME_OPTION: &str = "default_constraint";

/// The structure of the clone: `source` in the same schema as `new_name`,
/// its named constraints and indexes renamed (engines whose names can be
/// schema-wide: SQL and CQL; elsewhere they belong to the table and stay),
/// and a foreign key to the table itself pointed at the clone.
pub fn plan_clone(driver: &dyn Driver, source: &TableSchema, new_name: &str) -> Result<ClonePlan> {
    plan_clone_with(driver, source, new_name, &[])
}

/// [`plan_clone`], never giving a constraint or index a name in `reserved`
/// (other objects of the schema have them; compared ignoring case).
pub fn plan_clone_with(driver: &dyn Driver, source: &TableSchema, new_name: &str, reserved: &[String]) -> Result<ClonePlan> {
    plan_clone_within(driver, source, new_name, reserved, generated_limit(driver))
}

/// [`plan_clone_with`], with `max` (bytes, 0: none) as the limit for the
/// names the clone makes up, when the server takes more than the
/// conservative one (Oracle 12.2 and later: 128, see
/// [`server_generated_limit`]).
pub fn plan_clone_within(driver: &dyn Driver, source: &TableSchema, new_name: &str, reserved: &[String], max: usize) -> Result<ClonePlan> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        return Err(Error::State("falta el nombre de la tabla nueva".into()));
    }
    if new_name == source.name {
        return Err(Error::State("el nombre nuevo es igual al de la tabla original".into()));
    }
    let info = driver.info();
    if let Some(r) = timeseries::name_problem(info, new_name) {
        return Err(Error::State(r));
    }
    if let Some(r) = analytics::name_problem(info, new_name) {
        return Err(Error::State(r));
    }
    if let Some(r) = embedded::name_problem(info, new_name) {
        return Err(Error::State(r));
    }
    let limit = identifier_limit(driver);
    // SQL Server's and MySQL's limits are in characters (sysname is
    // nvarchar(128); MySQL, MariaDB and TiDB take 64 characters); elsewhere
    // in bytes (PostgreSQL's 63).
    let len = name_length(driver, new_name);
    if limit > 0 && len > limit {
        return Err(Error::State(if counts_characters(driver) {
            format!("el nombre «{new_name}» es demasiado largo para {}: admite hasta {limit} caracteres", info.name)
        } else {
            // Bytes: a name with accents or ñ holds fewer characters.
            format!(
                "el nombre «{new_name}» es demasiado largo para {}: admite hasta {limit} bytes y tiene {len}{}",
                info.name,
                if new_name.is_ascii() { "" } else { " (las letras con tilde y la ñ ocupan 2)" }
            )
        }));
    }
    let rename_names = matches!(info.language, Language::Sql | Language::Cql);
    // SQL Server, MySQL, MariaDB and TiDB count their made-up names in
    // characters too.
    let chars = matches!(info.dialect, "mssql" | "mysql");
    let bytes = byte_cap(info.id);
    let old = source.name.clone();
    let mut t = source.clone();
    t.name = new_name.to_string();
    let mut renames: Vec<Rename> = Vec::new();
    let mut notes = Vec::new();
    let mut used: Vec<String> = Vec::new();
    let mut rename = |name: &str, renames: &mut Vec<Rename>| -> String {
        if !rename_names {
            return name.to_string();
        }
        // The same name in two places (MySQL reports a foreign key and the
        // index behind it alike): one rename for both.
        if let Some(r) = renames.iter().find(|r| r.from == name) {
            return r.to.clone();
        }
        let (mut to, shortened) = rename_capped(name, &old, new_name, max, chars, bytes);
        // Two originals that end up alike (only after shortening, or
        // differing in case): the later one gets its own hash.
        let mut n = 0u32;
        while used.iter().chain(reserved).any(|u| u.eq_ignore_ascii_case(&to)) {
            n += 1;
            let suffix = format!("_{:08x}", fnv(&format!("{name}#{n}")));
            let base = fit(&to, max.saturating_sub(suffix.len()), chars, bytes.saturating_sub(suffix.len())).to_string();
            to = format!("{base}{suffix}");
        }
        used.push(to.clone());
        renames.push(Rename { from: name.to_string(), to: to.clone(), shortened });
        to
    };

    if let Some(pk) = t.primary_key.as_mut() {
        // MySQL's primary key is always `PRIMARY` (the name is ignored).
        if let Some(n) = pk.name.clone().filter(|n| !n.is_empty() && !n.eq_ignore_ascii_case("PRIMARY")) {
            pk.name = Some(rename(&n, &mut renames));
        }
    }
    for ix in &mut t.indexes {
        // SQL Server's full-text index has no name of its own (one per
        // table; the driver reports it as `fulltext`): nothing to rename.
        if !ix.name.is_empty() && !mssql::unnamed_index(info.dialect, ix) {
            ix.name = rename(&ix.name.clone(), &mut renames);
        }
    }
    for c in &mut t.checks {
        if let Some(n) = c.name.clone().filter(|n| !n.is_empty()) {
            c.name = Some(rename(&n, &mut renames));
        }
    }
    for c in &mut t.columns {
        if let Some(n) = c.options.get(DEFAULT_NAME_OPTION).cloned().filter(|n| !n.is_empty()) {
            let to = rename(&n, &mut renames);
            c.options.insert(DEFAULT_NAME_OPTION.into(), to);
        }
    }
    let same_schema = |s: Option<&str>| {
        let s = s.filter(|s| !s.is_empty());
        s.is_none() || s.map(str::to_ascii_lowercase) == source.schema.as_deref().map(str::to_ascii_lowercase)
    };
    for fk in &mut t.foreign_keys {
        if let Some(n) = fk.name.clone().filter(|n| !n.is_empty()) {
            fk.name = Some(rename(&n, &mut renames));
        }
        if fk.ref_table == old && same_schema(fk.ref_schema.as_deref()) {
            fk.ref_table = new_name.to_string();
            notes.push(format!(
                "la clave foránea sobre ({}) apunta a la misma tabla: en el clon apunta al clon",
                fk.columns.join(", ")
            ));
        }
    }
    // Options that name another index of the table (SQL Server's full-text
    // KEY INDEX) name the clone's.
    mssql::rename_key_index(&mut t, &renames);
    // Firebird: a column qualified with the original's name (`T.V`) in a
    // CHECK, a computed column or an index expression is the clone's there
    // (SQLite, libSQL and DuckDB: in their own CREATE, see `embedded`).
    if info.id == "firebird" {
        let bare = dbine_driver::sql::quote_ident(Quote::Double, new_name);
        let rq = |e: &str| embedded::requalify(e, &old, &bare);
        let mut reads: Vec<String> = Vec::new();
        for c in &t.checks {
            if embedded::reads_table(&c.expression, &old) {
                reads.push(format!("la restricción CHECK {}", c.name.as_deref().unwrap_or("")).trim_end().to_string());
            }
        }
        for c in t.columns.iter().filter(|c| generated(c, info.dialect) && embedded::reads_table(&c.data_type, &old)) {
            reads.push(format!("la columna calculada {}", c.name));
        }
        for ix in t.indexes.iter().filter(|ix| ix.columns.iter().chain(ix.filter.iter()).any(|e| embedded::reads_table(e, &old))) {
            reads.push(format!("el índice {}", ix.name));
        }
        if !reads.is_empty() {
            let (v, n) = if reads.len() == 1 { ("lee", "lee") } else { ("leen", "leen") };
            notes.push(format!("{} {v} la misma tabla: en el clon {n} el clon", reads.join(", ")));
        }
        for c in &mut t.checks {
            c.expression = rq(&c.expression);
        }
        for c in t.columns.iter_mut().filter(|c| generated(c, info.dialect)) {
            c.data_type = rq(&c.data_type);
        }
        for ix in &mut t.indexes {
            if ix.kind.as_deref().is_some_and(|k| k.to_ascii_uppercase().contains("COMPUTED")) {
                ix.columns = ix.columns.iter().map(|c| rq(c)).collect();
            }
            ix.filter = ix.filter.as_deref().map(rq);
        }
    }
    let moved: Vec<String> = renames
        .iter()
        .filter(|r| reserved.iter().any(|x| x.eq_ignore_ascii_case(&rename_capped(&r.from, &old, new_name, max, chars, bytes).0)))
        .map(|r| format!("{} → {}", r.from, r.to))
        .collect();
    if !moved.is_empty() {
        notes.push(format!("nombres que ya usaba otro objeto del esquema, cambiados: {}", moved.join(", ")));
    }
    let short: Vec<String> = renames.iter().filter(|r| r.shortened).map(|r| format!("{} → {}", r.from, r.to)).collect();
    if !short.is_empty() {
        notes.push(format!("nombres acortados para entrar en el límite de {} del motor: {}", limit_text(max, chars, bytes), short.join(", ")));
    }
    Ok(ClonePlan { table: t, renames, notes })
}

// -- SQL bits ---------------------------------------------------------------------------------------

fn quote_of(dialect: &str) -> Quote {
    match dialect {
        "mssql" | "sybase" => Quote::Bracket,
        "mysql" | "bigquery" | "hive" | "clickhouse" | "sparksql" | "databricks" | "tdengine" => Quote::Backtick,
        _ => Quote::Double,
    }
}

fn sql_name(driver: &dyn Driver, t: &TableSchema) -> String {
    qualified_name(quote_of(driver.info().dialect), t.schema.as_deref().filter(|s| !s.is_empty()), &t.name)
}

/// A table's name as the engine's SQL takes it (quoted, with its schema).
pub fn quoted_name(driver: &dyn Driver, t: &ObjectRef) -> String {
    qualified_name(quote_of(driver.info().dialect), t.schema(), &t.name)
}

/// A column the engine fills itself (computed, row version): never loaded.
/// Firebird reports its computed columns as `COMPUTED BY (…)`.
pub fn generated(c: &ColumnDef, dialect: &str) -> bool {
    let t = c.data_type.trim().to_ascii_lowercase();
    t.starts_with("as ")
        || t.starts_with("as(")
        || t.contains("generated always as")
        || t.starts_with("computed by")
        || (matches!(dialect, "mssql" | "sybase") && matches!(t.as_str(), "rowversion" | "timestamp"))
        // ClickHouse: MATERIALIZED / ALIAS / EPHEMERAL (can't be inserted).
        || c.options.get("default_kind").is_some_and(|k| matches!(k.to_ascii_uppercase().as_str(), "MATERIALIZED" | "ALIAS" | "EPHEMERAL"))
        || c.options.contains_key(analytics::COMPUTED)
}

/// The statement that empties the clone, should a copy attempt have to
/// start over (the engine retries transient errors). `None`: no way.
fn truncate_sql(driver: &dyn Driver, t: &TableSchema) -> Option<String> {
    let info = driver.info();
    if info.dialect == "influxql" {
        // The next point makes the measurement again.
        return Some(timeseries::influxql_drop(&t.name));
    }
    match info.language {
        Language::Sql | Language::Cql => {
            let name = sql_name(driver, t);
            Some(match info.id {
                "sqlite" | "libsql" | "firebird" | "access" | "dbase" | "dsql" | "iris" | "cache" | "openedge" | "zen" | "tdengine" => {
                    format!("DELETE FROM {name}")
                }
                "spanner" => format!("DELETE FROM {name} WHERE TRUE"),
                _ => format!("TRUNCATE TABLE {name}"),
            })
        }
        _ => driver
            .table_ddl(t, DdlParts { drop: true, if_exists: true, create: true, ..Default::default() })
            .ok()
            .filter(|s| !s.trim().is_empty()),
    }
}

async fn exec(s: &mut dyn Session, sql: &str) -> Result<()> {
    crate::copy::exec(s, sql).await
}

/// The first value of a query's first row, as an integer.
async fn scalar(s: &mut dyn Session, sql: &str) -> Result<Option<i64>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1, &mut out).await?;
    if let Some(e) = out.error {
        return Err(Error::Query(e));
    }
    Ok(out
        .results
        .iter()
        .rev()
        .find_map(|r| r.rows.first())
        .and_then(|row| row.first())
        .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| int_text(s.trim())))))
}

/// A number the driver gave as text: exact when it's an integer (a BIGINT
/// past 2^53 must not go through `f64`), else through `f64` ("12.0").
fn int_text(s: &str) -> Option<i64> {
    s.parse::<i64>().ok().or_else(|| s.parse::<f64>().ok().filter(|f| f.fract() == 0.0 && f.abs() < 9.0e15).map(|f| f as i64))
}

/// The first result with rows, as text (`None`: NULL).
async fn strings(s: &mut dyn Session, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10_000, &mut out).await?;
    if let Some(e) = out.error {
        return Err(Error::Query(e));
    }
    let text = |v: &serde_json::Value| match v {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    };
    Ok(out.results.iter().find(|r| !r.rows.is_empty()).map(|r| r.rows.iter().map(|row| row.iter().map(text).collect()).collect()).unwrap_or_default())
}

fn mssql_literal(t: &TableSchema) -> String {
    qualified_name(Quote::Bracket, t.schema.as_deref().filter(|s| !s.is_empty()), &t.name).replace('\'', "''")
}

/// An integer as SQL Server writes a decimal(38,0) (optional minus, digits).
fn mssql_integer(v: &str) -> bool {
    let d = v.strip_prefix('-').unwrap_or(v);
    !d.is_empty() && d.len() <= 38 && d.bytes().all(|b| b.is_ascii_digit())
}

/// SQL Server: the constraints whose name the server made up (`PK__t__…`)
/// lose it, so the clone's are made up by the server too (named ones stay
/// named). Best effort: unread, they are renamed like the others.
/// Returns the UNIQUE constraints named by the server (they're indexes in
/// the plan, named until the clone's are made: see
/// [`mssql::server_named_uniques`]).
async fn mssql_system_names(s: &mut dyn Session, t: &mut TableSchema) -> Vec<String> {
    let lit = mssql_literal(t);
    let sql = format!(
        "SELECT N'PK', name FROM sys.key_constraints WHERE parent_object_id = OBJECT_ID(N'{lit}') AND type = 'PK' AND is_system_named = 1 \
         UNION ALL SELECT N'UQ', name FROM sys.key_constraints WHERE parent_object_id = OBJECT_ID(N'{lit}') AND type = 'UQ' AND is_system_named = 1 \
         UNION ALL SELECT N'C', name FROM sys.check_constraints WHERE parent_object_id = OBJECT_ID(N'{lit}') AND is_system_named = 1 \
         UNION ALL SELECT N'F', name FROM sys.foreign_keys WHERE parent_object_id = OBJECT_ID(N'{lit}') AND is_system_named = 1"
    );
    let Ok(rows) = strings(s, &sql).await else { return Vec::new() };
    let mut unique = Vec::new();
    for r in rows {
        let [Some(kind), Some(name)] = r.as_slice() else { continue };
        let is = |n: &Option<String>| n.as_deref() == Some(name.as_str());
        match kind.as_str() {
            "PK" => {
                if let Some(pk) = t.primary_key.as_mut().filter(|pk| is(&pk.name)) {
                    pk.name = None;
                }
            }
            "UQ" => {
                if t.indexes.iter().any(|i| i.name == *name) {
                    unique.push(name.clone());
                }
            }
            "C" => t.checks.iter_mut().filter(|c| is(&c.name)).for_each(|c| c.name = None),
            _ => t.foreign_keys.iter_mut().filter(|f| is(&f.name)).for_each(|f| f.name = None),
        }
    }
    unique
}

/// SQL Server: the names given to the columns' DEFAULT constraints (not
/// the ones the server made up), into [`DEFAULT_NAME_OPTION`], so the
/// clone's are named alike. Best effort: without them the server names
/// them (same expressions).
async fn mssql_default_names(s: &mut dyn Session, t: &mut TableSchema) {
    let sql = format!(
        "SELECT c.name, dc.name FROM sys.default_constraints dc \
         JOIN sys.columns c ON c.object_id = dc.parent_object_id AND c.column_id = dc.parent_column_id \
         WHERE dc.parent_object_id = OBJECT_ID(N'{}') AND dc.is_system_named = 0",
        mssql_literal(t)
    );
    let Ok(rows) = strings(s, &sql).await else { return };
    for r in rows {
        if let [Some(col), Some(name)] = r.as_slice() {
            if let Some(c) = t.columns.iter_mut().find(|c| &c.name == col && c.default_value.as_deref().is_some_and(|d| !d.is_empty())) {
                c.options.insert(DEFAULT_NAME_OPTION.into(), name.clone());
            }
        }
    }
}

/// The names the plan gives its constraints and indexes that another
/// object of the schema already has. SQL Server (a constraint is a
/// schema-wide object there, `sys.objects`), Oracle (constraint names
/// are unique per schema, and indexes share the tables' namespace),
/// Firebird (index and constraint names are unique per database) and
/// MySQL, MariaDB, TiDB (foreign key names are unique per schema in
/// InnoDB, and MySQL's CHECK names too); elsewhere, or when the catalog
/// can't be read, none: a clash then fails when creating, drops the clone
/// and is told in Spanish ([`in_spanish`]).
async fn taken_names(driver: &dyn Driver, s: &mut dyn Session, p: &ClonePlan) -> Vec<String> {
    if p.renames.is_empty() {
        return Vec::new();
    }
    if driver.info().dialect == "oracle" {
        let names: Vec<String> = p.renames.iter().map(|r| r.to.clone()).collect();
        return oracle_taken(s, p.table.schema.as_deref(), &names, true).await.unwrap_or_default();
    }
    if driver.info().id == "firebird" {
        // Index and constraint names are unique per database.
        let names: Vec<String> = p.renames.iter().map(|r| r.to.clone()).collect();
        return strings(s, &embedded::firebird_taken_sql(&names)).await.map(|rows| rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect()).unwrap_or_default();
    }
    // TiDB takes the same constraint name on two tables.
    if driver.info().dialect == "mysql" && driver.info().id != "tidb" {
        let sql = mysql_taken_sql(driver.info().id, p.table.schema.as_deref(), &p.renames.iter().map(|r| r.to.clone()).collect::<Vec<_>>());
        return strings(s, &sql).await.map(|rows| rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect()).unwrap_or_default();
    }
    if driver.info().dialect != "mssql" {
        return Vec::new();
    }
    let list: Vec<String> = p.renames.iter().map(|r| format!("N'{}'", r.to.replace('\'', "''"))).collect();
    let schema = match p.table.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(x) => format!("SCHEMA_ID(N'{}')", x.replace('\'', "''")),
        None => "SCHEMA_ID()".into(),
    };
    let sql = format!("SELECT name FROM sys.objects WHERE schema_id = {schema} AND name IN ({})", list.join(", "));
    strings(s, &sql).await.map(|rows| rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect()).unwrap_or_default()
}

/// MySQL family: which of `names` another table of the schema (`None`: the
/// session's database) uses for a constraint whose name is schema-wide:
/// a foreign key (InnoDB) or, except on MariaDB (per table there), a
/// CHECK. Compared ignoring case, as the server does.
fn mysql_taken_sql(id: &str, schema: Option<&str>, names: &[String]) -> String {
    let lit = |v: &str| format!("'{}'", v.to_lowercase().replace('\\', "\\\\").replace('\'', "''"));
    let db = match schema.filter(|s| !s.is_empty()) {
        Some(x) => format!("'{}'", x.replace('\\', "\\\\").replace('\'', "''")),
        None => "DATABASE()".into(),
    };
    let kinds = if id == "mariadb" { "'FOREIGN KEY'" } else { "'FOREIGN KEY', 'CHECK'" };
    let list = names.iter().map(|n| lit(n)).collect::<Vec<_>>().join(", ");
    format!(
        "SELECT CONSTRAINT_NAME FROM information_schema.TABLE_CONSTRAINTS WHERE CONSTRAINT_SCHEMA = {db} \
         AND CONSTRAINT_TYPE IN ({kinds}) AND LOWER(CONSTRAINT_NAME) IN ({list})"
    )
}

/// Oracle: which of `names` the schema (`None`: the session's) already
/// uses for an object (ALL_OBJECTS: tables, views, indexes, sequences,
/// synonyms… share one namespace; a primary key's or unique constraint's
/// index takes the constraint's name) or, with `constraints`, for a
/// constraint (ALL_CONSTRAINTS: unique per schema, whatever the table).
async fn oracle_taken(s: &mut dyn Session, schema: Option<&str>, names: &[String], constraints: bool) -> Result<Vec<String>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let sql = oracle_taken_sql(schema, names, constraints);
    Ok(strings(s, &sql).await?.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect())
}

fn oracle_taken_sql(schema: Option<&str>, names: &[String], constraints: bool) -> String {
    let lit = |v: &str| format!("'{}'", v.replace('\'', "''"));
    let owner = match schema.filter(|s| !s.is_empty()) {
        Some(x) => lit(x),
        None => "SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')".into(),
    };
    let list = names.iter().map(|n| lit(n)).collect::<Vec<_>>().join(", ");
    let objects = format!("SELECT object_name FROM all_objects WHERE owner = {owner} AND object_name IN ({list})");
    if constraints {
        format!("{objects} UNION SELECT constraint_name FROM all_constraints WHERE owner = {owner} AND constraint_name IN ({list})")
    } else {
        objects
    }
}

/// Oracle's "name is already used by an existing object" (ORA-00955) and
/// "… by an existing constraint" (ORA-02264) say neither which name nor
/// in Spanish: the catalog tells which of the clone's names is taken
/// (`table`: the table's own too, before it exists). `None`: another error.
async fn oracle_clash(s: &mut dyn Session, e: &Error, plan: &ClonePlan, table: bool) -> Option<Error> {
    let m = e.to_string();
    if !["ORA-00955", "ORA-02264"].iter().any(|c| m.contains(c)) {
        return None;
    }
    let schema = plan.table.schema.as_deref();
    if table && oracle_taken(s, schema, std::slice::from_ref(&plan.table.name), false).await.is_ok_and(|t| !t.is_empty()) {
        return Some(Error::State(format!(
            "ya existe en el esquema otro objeto llamado «{}» (puede ser uno que el explorador no muestra, como una secuencia o un sinónimo); elegí otro nombre",
            plan.table.name
        )));
    }
    let names: Vec<String> = plan.renames.iter().map(|r| r.to.clone()).collect();
    let taken = oracle_taken(s, schema, &names, true).await.unwrap_or_default();
    Some(Error::State(oracle_clash_text(&plan.table.name, &names, &taken)))
}

fn oracle_clash_text(table: &str, names: &[String], taken: &[String]) -> String {
    let q = |v: &[String]| v.iter().map(|n| format!("«{n}»")).collect::<Vec<_>>().join(", ");
    if taken.is_empty() {
        let all: Vec<String> = std::iter::once(table.to_string()).chain(names.iter().cloned()).collect();
        return format!("ya existe en el esquema otro objeto o restricción con uno de los nombres del clon ({}); elegí otro nombre para la tabla", q(&all));
    }
    format!(
        "otros objetos del esquema ya usan {} {} que tendría el clon para sus restricciones o índices (se crearon mientras se clonaba); volvé a intentarlo o elegí otro nombre para la tabla",
        if taken.len() == 1 { "el nombre" } else { "los nombres" },
        q(taken)
    )
}

/// SQL Server: the clone's IDENTITY must start and step like the
/// original's (a clone that numbers new rows differently is not the same
/// table).
async fn mssql_check_identity(s: &mut dyn Session, original: &TableSchema, clone: &TableSchema) -> Result<()> {
    let (a, b) = (mssql_literal(original), mssql_literal(clone));
    // NOT FOR REPLICATION: CREATE TABLE leaves it off; the original's is
    // set on the clone the way SSMS does it.
    let nfr = |t: &str| format!("(SELECT CAST(MAX(CAST(is_not_for_replication AS int)) AS nvarchar(1)) FROM sys.identity_columns WHERE object_id = OBJECT_ID(N'{t}'))");
    let flags = strings(s, &format!("SELECT {}, {}", nfr(&a), nfr(&b))).await.map_err(|e| Error::Query(format!("identidad: {e}")))?;
    if let Some([Some(x), Some(y)]) = flags.first().map(Vec::as_slice) {
        if x != y && matches!(x.as_str(), "0" | "1") {
            exec(s, &format!("DECLARE @o int = OBJECT_ID(N'{b}'); EXEC sys.sp_identitycolumnforreplication @o, {x};"))
                .await
                .map_err(|e| Error::State(format!("no se pudo dar al IDENTITY del clon el NOT FOR REPLICATION del original ({e}); no se clona")))?;
        }
    }
    let sql = format!(
        "SELECT CAST(IDENT_SEED(N'{a}') AS nvarchar(40)), CAST(IDENT_INCR(N'{a}') AS nvarchar(40)), \
                CAST(IDENT_SEED(N'{b}') AS nvarchar(40)), CAST(IDENT_INCR(N'{b}') AS nvarchar(40)), {}, {}",
        nfr(&a),
        nfr(&b)
    );
    let rows = strings(s, &sql).await.map_err(|e| Error::Query(format!("identidad: {e}")))?;
    if let Some([_, _, _, _, n1, n2]) = rows.first().map(Vec::as_slice) {
        if n1 != n2 {
            return Err(Error::State(
                "el clon no quedó igual al original: el IDENTITY del original es NOT FOR REPLICATION y el del clon no; no se clona".into(),
            ));
        }
    }
    if let Some([s1, i1, s2, i2, ..]) = rows.first().map(Vec::as_slice) {
        if s1 != s2 || i1 != i2 {
            let v = |x: &Option<String>| x.clone().unwrap_or_else(|| "?".into());
            return Err(Error::State(format!(
                "el clon no quedó igual al original: su IDENTITY empieza en {} y avanza de a {}, el del original empieza en {} y avanza de a {}; no se clona",
                v(s2),
                v(i2),
                v(s1),
                v(i1)
            )));
        }
    }
    Ok(())
}

/// A CREATE that failed because the name is taken: that object is someone
/// else's (made meanwhile) and is never dropped. SQL Server says "There is
/// already an object named…" (2714); others, "… already exists".
fn name_taken(e: &Error) -> bool {
    let m = e.to_string().to_lowercase();
    m.contains("exist") || m.contains("already an object named")
}

/// SQL Server's "There is already an object named 'X' in the database."
/// in Spanish; any other error as it came.
fn in_spanish(e: Error) -> Error {
    const EN: &str = "There is already an object named '";
    let m = e.to_string();
    // PostgreSQL / CockroachDB: `relation "X" already exists`.
    const PG: &str = "relation \"";
    let name = m
        .find(EN)
        .map(|i| &m[i + EN.len()..])
        .and_then(|r| r.find("' in the database").map(|j| r[..j].to_string()))
        .or_else(|| m.find(PG).map(|i| &m[i + PG.len()..]).and_then(|r| r.find("\" already exists").map(|j| r[..j].to_string())));
    if let Some(n) = name {
        return Error::State(format!("ya existe en el esquema otro objeto llamado «{n}» (tabla, restricción o índice); no se clona"));
    }
    // MySQL: `Duplicate check constraint name 'X'.` (3822), `Duplicate
    // foreign key constraint name 'X'` (1826); MariaDB and older MySQL,
    // for a foreign key: `Can't create table … (errno: 121 "Duplicate key
    // on write or update")`. Names another table took meanwhile (the ones
    // taken before cloning were avoided).
    for (en, what) in [("Duplicate check constraint name '", "restricción CHECK"), ("Duplicate foreign key constraint name '", "clave foránea")] {
        if let Some(r) = m.find(en).map(|i| &m[i + en.len()..]) {
            let n = &r[..r.find('\'').unwrap_or(r.len())];
            return Error::State(format!("otra tabla del esquema ya tiene una {what} llamada «{n}» (se creó mientras se clonaba); volvé a intentarlo o elegí otro nombre para la tabla"));
        }
    }
    if m.contains("errno: 121") {
        return Error::State(
            "otra tabla del esquema ya tiene una clave foránea con uno de los nombres del clon (se creó mientras se clonaba); volvé a intentarlo o elegí otro nombre para la tabla".into(),
        );
    }
    // Elasticsearch / OpenSearch: `resource_already_exists_exception:
    // index [X/uuid] already exists`.
    if m.contains("resource_already_exists_exception") {
        if let Some(r) = m.find("index [").map(|i| &m[i + "index [".len()..]) {
            let n = &r[..r.find(['/', ']']).unwrap_or(r.len())];
            return Error::State(format!("ya existe un objeto llamado «{n}» (se creó mientras se clonaba); elegí otro nombre"));
        }
    }
    analytics::in_spanish(oracle::in_spanish(e))
}

/// Same schema (a missing one is the default) and name, ignoring case:
/// engines with case-insensitive names would take them for the same.
fn same_object(schema: Option<&str>, name: &str, t: &ObjectRef) -> bool {
    let a = schema.filter(|s| !s.is_empty()).map(str::to_lowercase);
    let b = t.schema().map(str::to_lowercase);
    (a.is_none() || b.is_none() || a == b) && name.to_lowercase() == t.name.to_lowercase()
}

// -- the clone --------------------------------------------------------------------------------------

/// The session's current schema, on engines whose `database_schema` may
/// leave it unnamed (Oracle reports its tables with no schema).
fn current_schema_sql(dialect: &str) -> Option<&'static str> {
    match dialect {
        "oracle" => Some("SELECT SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') FROM dual"),
        "postgres" => Some("SELECT current_schema()"),
        "mssql" => Some("SELECT SCHEMA_NAME()"),
        "mysql" => Some("SELECT DATABASE()"),
        _ => None,
    }
}

/// Which of the reported tables is `obj`: same name and kind, and the same
/// schema; or no schema reported when `obj`'s is the session's current one
/// (`current`, exact).
fn find_table(all: Vec<TableSchema>, obj: &ObjectRef, current: Option<&str>) -> Option<TableSchema> {
    let kind_ok = |t: &TableSchema| {
        t.kind == obj.kind || (matches!(t.kind.as_str(), kinds::TABLE | kinds::COLLECTION) && matches!(obj.kind.as_str(), kinds::TABLE | kinds::COLLECTION))
    };
    let mut candidates: Vec<TableSchema> = all.into_iter().filter(|t| t.name == obj.name && kind_ok(t)).collect();
    if let Some(i) = candidates.iter().position(|t| t.schema.as_deref().filter(|x| !x.is_empty()) == obj.schema()) {
        return Some(candidates.swap_remove(i));
    }
    if obj.schema().is_none() && candidates.len() == 1 {
        // Engines that list their objects without the schema the structure
        // carries (IoTDB: the session's database).
        return candidates.pop();
    }
    let wanted = obj.schema()?;
    if current.filter(|c| !c.is_empty()) != Some(wanted) {
        return None;
    }
    let i = candidates.iter().position(|t| t.schema.as_deref().filter(|x| !x.is_empty()).is_none())?;
    Some(candidates.swap_remove(i))
}

/// Whether the engine's `database_schema` reports more than columns: then
/// a table missing from it can't be cloned from its columns alone without
/// losing parts (indexes, constraints, computed columns).
fn reports_full_structure(all: &[TableSchema], dialect: &str) -> bool {
    all.iter().any(|t| {
        !t.indexes.is_empty()
            || !t.foreign_keys.is_empty()
            || !t.checks.is_empty()
            || t.primary_key.as_ref().is_some_and(|k| k.name.is_some())
            || t.columns.iter().any(|c| generated(c, dialect))
    })
}

/// The original's structure: from `database_schema`, or from its columns
/// on engines that report nothing else.
async fn read_schema(driver: &dyn Driver, s: &mut dyn Session, obj: &ObjectRef) -> Result<(TableSchema, Vec<String>)> {
    let all = s.database_schema().await?;
    let dialect = driver.info().dialect;
    let full = reports_full_structure(&all, dialect);
    let unnamed = obj.schema().is_some() && all.iter().any(|t| t.name == obj.name && t.schema.as_deref().filter(|x| !x.is_empty()).is_none());
    let current = match current_schema_sql(dialect).filter(|_| unnamed) {
        Some(sql) => strings(s, sql).await.ok().and_then(|r| r.into_iter().next()).and_then(|r| r.into_iter().next().flatten()),
        None => None,
    };
    if let Some(t) = find_table(all, obj, current.as_deref()) {
        return Ok((t, Vec::new()));
    }
    // Gone (dropped or renamed since the explorer listed it): said so, not
    // the server's "Table … doesn't exist".
    let gone = || {
        Error::State(format!(
            "no se encontró la tabla «{}»: puede que la hayan borrado o renombrado; actualizá el explorador y volvé a intentar",
            obj.name
        ))
    };
    // Engines that list and describe only the session's database (none
    // selected: nothing): a table in another one is looked up in its own.
    let in_catalog = match obj.schema().and_then(|db| analytics::exists_sql(driver.info(), db, &obj.name)) {
        Some(sql) => scalar(s, &sql).await.ok().flatten(),
        None => None,
    };
    if in_catalog == Some(0) {
        return Err(gone());
    }
    if in_catalog.is_some() {
        // There: read from its columns below.
    } else if let Ok(objects) = s.list_objects().await {
        // Only when the listing covers the table's schema: engines that
        // list just the session's schema, unnamed (MySQL family), say
        // nothing about a table in another one.
        let covered = match obj.schema() {
            Some(wanted) if !objects.iter().any(|o| o.schema.as_deref() == Some(wanted)) => match current_schema_sql(dialect) {
                Some(sql) => {
                    let cur = match &current {
                        Some(c) => Some(c.clone()),
                        None => strings(s, sql).await.ok().and_then(|r| r.into_iter().next()).and_then(|r| r.into_iter().next().flatten()),
                    };
                    cur.is_some_and(|c| c == wanted)
                }
                None => true,
            },
            _ => true,
        };
        let exists = !covered
            || objects.iter().any(|o| o.name == obj.name && (obj.schema().is_none() || o.schema.as_deref().filter(|x| !x.is_empty()).is_none_or(|x| Some(x) == obj.schema())));
        if !exists {
            return Err(gone());
        }
    }
    // StarRocks, Doris, ClickHouse, GreptimeDB: the clone is the engine's
    // own copy of the definition, checked against the original's, and
    // `analytics::prepare` reads the rest from the table's own database
    // (computed columns, rollups): its columns are enough.
    if full && !analytics::copies_definition(driver.info()) {
        return Err(Error::Unsupported(format!(
            "no se puede clonar: no se pudo leer la estructura completa de «{}» (índices, restricciones, columnas calculadas) desde esta conexión; abrí la conexión en el esquema de la tabla y volvé a intentar",
            obj.name
        )));
    }
    if !matches!(obj.kind.as_str(), kinds::TABLE | kinds::COLLECTION) {
        return Err(Error::Unsupported(
            "no se puede clonar: el motor no informa la estructura completa de este tipo de objeto".into(),
        ));
    }
    let cols = s.columns(obj).await?;
    if cols.is_empty() {
        return Err(Error::State(format!("no se encontró «{}» o no tiene columnas", obj.name)));
    }
    let pk: Vec<String> = cols.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
    let t = TableSchema {
        kind: obj.kind.clone(),
        schema: obj.schema.clone(),
        name: obj.name.clone(),
        primary_key: (!pk.is_empty()).then(|| KeyDef { name: None, columns: pk }),
        columns: cols
            .into_iter()
            .map(|c| ColumnDef {
                name: c.name,
                data_type: c.data_type,
                nullable: c.nullable,
                default_value: c.default_value,
                auto_increment: c.auto_increment,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    };
    Ok((t, vec!["el motor solo informa las columnas y la clave primaria: el clon no lleva índices ni claves foráneas".into()]))
}

/// Parts the reported structure leaves out that the clone would lose.
async fn check_complete(driver: &dyn Driver, s: &mut dyn Session, t: &TableSchema) -> Result<()> {
    timeseries::check(driver, s, t).await?;
    if driver.info().dialect == "oracle" {
        let invisible = oracle_invisible_columns(s, t).await;
        if !invisible.is_empty() {
            let list = invisible.iter().map(|c| format!("«{c}»")).collect::<Vec<_>>().join(", ");
            return Err(Error::Unsupported(format!(
                "no se puede clonar: {} {list} {} INVISIBLE y el clon {}; hacela visible (ALTER TABLE … MODIFY … VISIBLE) o clonala a mano",
                if invisible.len() == 1 { "la columna" } else { "las columnas" },
                if invisible.len() == 1 { "es" } else { "son" },
                if invisible.len() == 1 { "la perdería" } else { "las perdería" },
            )));
        }
    }
    // SQLite's computed columns, hidden from its catalog pragmas, come
    // with the original's own CREATE (`embedded`).
    Ok(())
}

/// Columns that differ between the original and the clone (name, type,
/// nullability), as the engine reports them.
fn column_differences(a: &[dbine_driver::ColumnInfo], b: &[dbine_driver::ColumnInfo]) -> Vec<String> {
    let mut out = Vec::new();
    if a.len() != b.len() {
        let missing = |x: &[dbine_driver::ColumnInfo], y: &[dbine_driver::ColumnInfo]| {
            x.iter().filter(|c| !y.iter().any(|d| d.name == c.name)).map(|c| format!("«{}»", c.name)).collect::<Vec<_>>()
        };
        let mut s = format!("{} columnas en el original, {} en el clon", a.len(), b.len());
        let (lost, extra) = (missing(a, b), missing(b, a));
        if !lost.is_empty() {
            s.push_str(&format!("; faltan en el clon: {}", lost.join(", ")));
        }
        if !extra.is_empty() {
            s.push_str(&format!("; sobran en el clon: {}", extra.join(", ")));
        }
        out.push(s);
        return out;
    }
    for (x, y) in a.iter().zip(b) {
        if x.name != y.name {
            out.push(format!("columna «{}» / «{}»", x.name, y.name));
        } else if !x.data_type.trim().eq_ignore_ascii_case(y.data_type.trim()) {
            out.push(format!("{}: tipo {} / {}", x.name, x.data_type, y.data_type));
        } else if x.nullable != y.nullable {
            out.push(format!("{}: {} / {}", x.name, null_text(x.nullable), null_text(y.nullable)));
        }
    }
    out
}

fn null_text(n: bool) -> &'static str {
    if n {
        "admite NULL"
    } else {
        "NOT NULL"
    }
}

/// Everything generated before anything is written.
struct Ddl {
    create: String,
    indexes: Option<String>,
    foreign_keys: Option<String>,
    drop: String,
}

/// Without indexes the clone has no unique constraints: a foreign key to
/// the table itself on columns other than the primary key's would have
/// nothing to point at (the engines refuse it). Refused before writing.
fn self_references_without_indexes(t: &TableSchema) -> Result<()> {
    let same_schema = |s: Option<&str>| s.filter(|s| !s.is_empty()).is_none_or(|s| t.schema.as_deref().is_none_or(|o| o.is_empty() || o == s));
    let mut pk: Vec<&str> = t.primary_key.iter().flat_map(|k| k.columns.iter().map(String::as_str)).collect();
    pk.sort_unstable();
    for fk in t.foreign_keys.iter().filter(|f| f.ref_table == t.name && same_schema(f.ref_schema.as_deref())) {
        let mut to: Vec<&str> = fk.ref_columns.iter().map(String::as_str).collect();
        to.sort_unstable();
        if to != pk {
            return Err(Error::Unsupported(format!(
                "no se puede clonar sin índices: la clave foránea «{}» apunta a la misma tabla ({}) y necesita la restricción única de esas columnas, que sin índices no se crea; cloná la tabla con índices",
                fk.name.as_deref().unwrap_or("sin nombre"),
                fk.ref_columns.join(", ")
            )));
        }
    }
    Ok(())
}

fn ddl_of(driver: &dyn Driver, t: &TableSchema, o: &CloneOptions) -> Result<Ddl> {
    if let Some(d) = timeseries::implicit_ddl(driver, t) {
        return Ok(d);
    }
    let create = driver.table_ddl(t, DdlParts { create: true, ..Default::default() })?;
    if create.trim().is_empty() {
        return Err(Error::Unsupported("no se puede clonar: este motor no genera el CREATE de la tabla".into()));
    }
    let drop = driver
        .table_ddl(t, DdlParts { drop: true, if_exists: true, ..Default::default() })
        .ok()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| Error::Unsupported("no se puede clonar: este motor no permite borrar un clon que quede a medias".into()))?;
    let indexes = if o.with_indexes && !t.indexes.is_empty() {
        Some(driver.table_ddl(t, DdlParts { indexes: true, ..Default::default() })?).filter(|s| !s.trim().is_empty())
    } else {
        None
    };
    let foreign_keys = if t.foreign_keys.is_empty() {
        None
    } else {
        Some(driver.table_ddl(t, DdlParts { foreign_keys: true, ..Default::default() })?).filter(|s| !s.trim().is_empty())
    };
    Ok(Ddl { create, indexes, foreign_keys, drop })
}

/// Endpoints for the clone: the caller's, with SQLite's foreign keys off on
/// the loading connections (a self reference would fail for rows loaded
/// before the row they point to; the original already satisfied them).
struct CloneEndpoints(Arc<dyn Endpoints>);

#[async_trait]
impl Endpoints for CloneEndpoints {
    fn source_driver(&self) -> Arc<dyn Driver> {
        self.0.source_driver()
    }
    fn target_driver(&self) -> Arc<dyn Driver> {
        self.0.target_driver()
    }
    async fn open_source(&self) -> Result<Box<dyn Session>> {
        self.0.open_source().await
    }
    async fn open_target(&self) -> Result<Box<dyn Session>> {
        let mut s = self.0.open_target().await?;
        if matches!(self.0.target_driver().info().id, "sqlite" | "libsql") {
            exec(&mut *s, "PRAGMA foreign_keys = OFF").await?;
        }
        if self.0.target_driver().info().id == "tidb" {
            // AUTO_RANDOM columns take the original's values only with this.
            exec(&mut *s, "SET @@allow_auto_random_explicit_insert = 1").await?;
        }
        Ok(s)
    }
    fn native_copy_allowed(&self) -> bool {
        self.0.native_copy_allowed()
    }
}

/// Clone `req.source` as `req.new_name` in the same database. `endpoints`
/// reach that database on both sides (the source side only reads).
pub async fn clone_table(
    endpoints: Arc<dyn Endpoints>,
    req: CloneRequest,
    control: &CloneControl,
    events: impl Fn(CloneEvent) + Send + Sync + 'static,
) -> Result<CloneReport> {
    let start = Instant::now();
    let events: Arc<dyn Fn(CloneEvent) + Send + Sync> = Arc::new(events);
    let phase = |p: ClonePhase| events(CloneEvent::Phase { phase: p });
    let driver = endpoints.target_driver();
    let info = driver.info();
    check_cloneable(&*driver, &req.source.kind)?;
    let new_name = req.new_name.trim().to_string();

    // 2-4: read, plan, check. Nothing is written yet.
    phase(ClonePhase::Read);
    let mut src = endpoints.open_source().await?;
    // PostgreSQL's catalog: what `database_schema` flattens (identity
    // kinds, collations, serials, computed columns), what a plain table
    // can't be (partitioned, hypertable, inheritance), and every name the
    // schema already uses (indexes and sequences too). First: a partition
    // isn't in `database_schema`, and the reason to refuse it is this one.
    let pg_t = if pg::applies(&*driver) {
        Some(pg::inspect(&mut *src, &*driver, req.source.schema(), &req.source.name).await?)
    } else {
        None
    };
    // SQL Server: a graph table (AS NODE / AS EDGE) is refused before the
    // read, which fails on its internal columns.
    if info.dialect == "mssql" {
        mssql::refuse_graph(&mut *src, req.source.schema(), &req.source.name).await?;
    }
    let (mut source_t, mut notes) = read_schema(&*driver, &mut *src, &req.source).await?;
    check_complete(&*driver, &mut *src, &source_t).await?;
    // Oracle: each constraint's state (DISABLE, NOVALIDATE, DEFERRABLE,
    // RELY) and the keys' own indexes (see `oracle`).
    let ora_t = if info.dialect == "oracle" { Some(oracle::inspect(&mut *src, &source_t).await?) } else { None };
    // Oracle: a global temporary table's rows are each session's.
    let mut req = req;
    notes.extend(ora_t.as_ref().and_then(|o| oracle::session_rows(o, &mut req.options)));
    // MySQL, MariaDB, TiDB: the clone comes from the original's own
    // definition (collations, INVISIBLE, partitions, AUTO_RANDOM…).
    let my_t = if mysql::applies(&*driver) { Some(mysql::inspect(&*driver, &mut *src, &req.source.name).await?) } else { None };
    // SQL Server: the primary key's clustering, key order and storage
    // options (`KeyDef` carries only its name and columns), and the
    // UNIQUE constraints the server named.
    let mut mssql_pk = None;
    let mut mssql_uq: Vec<String> = Vec::new();
    // SQL Server: sparse columns, column set, PERIOD / SYSTEM_VERSIONING
    // (`database_schema` reports them as plain columns).
    let mut mssql_x = mssql::Extras::default();
    let mut mssql_layout: Option<mssql::Layout> = None;
    if info.dialect == "mssql" {
        mssql_x = mssql::extras(&mut *src, &source_t).await?;
        mssql_default_names(&mut *src, &mut source_t).await;
        mssql_uq = mssql_system_names(&mut *src, &mut source_t).await;
        match mssql::primary_key(&mut *src, &source_t).await {
            Ok(p) => mssql_pk = p,
            Err(_) => notes.push(
                "no se pudieron leer las opciones de la clave primaria del original (orden de las columnas, FILLFACTOR, compresión…): el clon la tiene con las de por omisión".into(),
            ),
        }
        if let Some(p) = &mssql_pk {
            mssql::check_primary_key(p)?;
        }
        // Where the heap or clustered index and the other indexes are
        // (filegroup, partition scheme): refused now when the clone can't.
        mssql_layout = mssql::layout(&mut *src, &source_t).await?;
        if mssql_layout.is_none() {
            notes.push(
                "no se pudo leer dónde se guardan la tabla y sus índices (filegroups, esquemas de partición): el clon queda en el filegroup por omisión".into(),
            );
        }
    }
    // Constraint / index names another object already has are avoided
    // before writing anything (not found out halfway, after the rows).
    let mut reserved: Vec<String> = pg_t.as_ref().map(|p| p.taken.clone()).unwrap_or_default();
    let mut attempt = 0;
    let max_generated = server_generated_limit(&*driver, &mut *src).await;
    let mut my_notes = Vec::new();
    // The clone's names (placeholders) of the UNIQUE constraints the
    // server named: not renamed, the server names them again.
    let mut server_uq: Vec<String> = Vec::new();
    let mut plan = loop {
        let mut p = plan_clone_within(&*driver, &source_t, &new_name, &reserved, max_generated)?;
        if !mssql_uq.is_empty() {
            server_uq = p.renames.iter().filter(|r| mssql_uq.contains(&r.from)).map(|r| r.to.clone()).collect();
            p.renames.retain(|r| !mssql_uq.contains(&r.from));
        }
        // MySQL, MariaDB, TiDB: the definition's names the catalog didn't
        // report are renamed now, so they're checked too.
        if let Some(m) = &my_t {
            my_notes = mysql::complete(m, &mut p.renames, &source_t.name, &new_name, max_generated, &reserved);
        }
        if let Some(o) = &ora_t {
            oracle::complete(o, &mut p, &source_t.name, &new_name, max_generated, &reserved);
        }
        let taken = taken_names(&*driver, &mut *src, &p).await;
        if taken.is_empty() {
            break p;
        }
        attempt += 1;
        if attempt > 3 {
            return Err(Error::State(format!(
                "otros objetos del esquema ya usan los nombres que tendrían las restricciones del clon ({}); elegí otro nombre para la tabla",
                taken.join(", ")
            )));
        }
        reserved.extend(taken);
    };
    let prelude = match &pg_t {
        Some(p) => pg::adjust(&mut plan.table, p, &mut plan.notes)?,
        None => String::new(),
    };
    // Document and search engines: memberships left out, write settings
    // deferred, MongoDB's building name (see `documents`).
    let prepared = documents::prepare(&*driver, &mut plan, &mut notes)?;
    if req.options.with_data {
        documents::check_whole_source(&*driver, &source_t)?;
    }
    notes.extend(plan.notes.iter().cloned());
    // PostgreSQL: CHECKs / foreign keys NOT VALID or NO INHERIT go after
    // the rows as the original has them; UNLOGGED stays.
    let pg_deferred = match &pg_t {
        Some(p) => Some(pg::defer(&*driver, &mut plan, p)?),
        None => None,
    };
    let mut ddl = ddl_of(&*driver, &plan.table, &req.options)?;
    if let Some(d) = &pg_deferred {
        d.apply(&mut ddl.create, &mut ddl.foreign_keys, &mut ddl.indexes, req.options.with_indexes)?;
    }
    if !prelude.is_empty() {
        ddl.create = format!("{prelude}{}", ddl.create);
    }
    if let Some(m) = &my_t {
        let auto: Vec<String> = source_t.columns.iter().filter(|c| c.auto_increment).map(|c| c.name.clone()).collect();
        let d = mysql::ddl(m, &mut plan.renames, &source_t.name, &new_name, &sql_name(&*driver, &plan.table), &auto, req.options.with_indexes, max_generated);
        (ddl.create, ddl.indexes, ddl.foreign_keys) = (d.create, d.indexes, d.foreign_keys);
        notes.extend(std::mem::take(&mut my_notes));
        notes.extend(d.notes);
    }
    if info.dialect == "mssql" {
        // Added unchecked (the original's rows may not satisfy a disabled
        // key); `mssql::constraints` validates the trusted ones.
        ddl.foreign_keys = ddl.foreign_keys.map(|s| mssql::foreign_keys_unchecked(&s, &plan.table));
        let pk_placed = match &mssql_pk {
            Some(p) => mssql::patch_primary_key(&mut ddl.create, &plan.table, p)? && p.clustered(),
            None => false,
        };
        if let Some(l) = &mssql_layout {
            mssql::place_table(&mut ddl.create, l, pk_placed)?;
            if let Some(sql) = ddl.indexes.as_mut() {
                mssql::place_indexes(&*driver, sql, &plan.table, l)?;
            }
        }
        mssql::patch_sparse(&mut ddl.create, &plan.table, &mssql_x)?;
        mssql::plan_history(&mut *src, &mut mssql_x, &source_t, &plan.table).await?;
        if mssql_x.period.as_ref().is_some_and(|p| p.history.is_some()) {
            ddl.drop = format!("{}{}", mssql::drop_temporal_sql(&plan.table), ddl.drop);
        }
    }
    // Oracle: the constraints in the original's states, the keys with
    // their own indexes.
    let ora_after = match &ora_t {
        Some(o) => oracle::adjust(&*driver, o, &source_t, &plan, &req.options, &mut ddl)?,
        None => Default::default(),
    };
    let native = timeseries::prepare(&*driver, &mut *src, &req.source, &source_t, &plan.table, &req.options, &mut ddl, &mut notes).await?;
    // StarRocks, Doris, ClickHouse, GreptimeDB: the engine's own copy of the
    // definition; Trino too: the rows inside the server (see `analytics`).
    let analytic = analytics::prepare(&*driver, &mut *src, &req.source, &mut source_t, &mut plan, &req.options, &mut ddl, &mut notes).await?;
    if !req.options.with_indexes {
        let unique: Vec<&str> = source_t.indexes.iter().filter(|i| i.unique).map(|i| i.name.as_str()).collect();
        if !unique.is_empty() {
            notes.push(format!("sin índices: tampoco se crean las restricciones únicas ({})", unique.join(", ")));
        }
        // PostgreSQL's EXCLUDE constraints live on an index too.
        let exclude: Vec<&str> =
            source_t.indexes.iter().filter(|i| i.kind.as_deref().is_some_and(|k| k.eq_ignore_ascii_case("EXCLUDE"))).map(|i| i.name.as_str()).collect();
        if !exclude.is_empty() {
            notes.push(format!("sin índices: tampoco se crean las restricciones de exclusión (EXCLUDE: {})", exclude.join(", ")));
        }
        self_references_without_indexes(&source_t)?;
    }
    let target_ref = ObjectRef { kind: source_t.kind.clone(), schema: source_t.schema.clone(), name: new_name.clone() };
    // The MySQL family and ClickHouse list only the session's database
    // (none selected: an error), unnamed: a table in another one is looked
    // up in its own, and the session's listing says nothing about it.
    let other_db = source_t.schema.as_deref().filter(|s| !s.is_empty()).and_then(|db| analytics::exists_sql(info, db, &new_name));
    let objects = match src.list_objects().await {
        Ok(o) => o,
        Err(_) if other_db.is_some() => Vec::new(),
        Err(e) => return Err(e),
    };
    if let Some(sql) = &other_db {
        if scalar(&mut *src, sql).await.ok().flatten().is_some_and(|n| n > 0) {
            return Err(Error::State(format!("ya existe un objeto llamado «{new_name}»; elegí otro nombre")));
        }
    }
    if let Some(o) = objects.iter().filter(|_| other_db.is_none()).find(|o| {
        same_object(source_t.schema.as_deref(), &new_name, &ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() })
    }) {
        return Err(Error::State(format!("ya existe un objeto llamado «{}»; elegí otro nombre", o.name)));
    }
    if let Some(p) = &pg_t {
        if p.taken.iter().any(|t| *t == new_name) {
            return Err(Error::State(format!(
                "ya existe en el esquema otro objeto llamado «{new_name}» (índice, secuencia o tipo); elegí otro nombre"
            )));
        }
        let seqs = pg::sequence_collisions(p, &new_name);
        if !seqs.is_empty() {
            return Err(Error::State(format!(
                "la secuencia del clon se llamaría {} y ese nombre ya existe en el esquema; elegí otro nombre",
                seqs.join(", ")
            )));
        }
    }
    // Oracle: a sequence, synonym, type… the explorer doesn't list, found
    // before creating (the engine's ORA-00955 names nothing).
    if info.dialect == "oracle"
        && oracle_taken(&mut *src, plan.table.schema.as_deref(), std::slice::from_ref(&new_name), false).await.is_ok_and(|t| !t.is_empty())
    {
        return Err(Error::State(format!(
            "ya existe en el esquema otro objeto llamado «{new_name}» (puede ser uno que el explorador no muestra, como una secuencia o un sinónimo); elegí otro nombre"
        )));
    }
    // SQLite, libSQL, DuckDB: the original's own CREATE, renamed (the
    // catalog leaves out collations, computed columns…; see `embedded`).
    if let Some(n) = embedded::native_ddl(&*driver, &mut *src, &source_t, &new_name, &objects, req.options.with_indexes).await? {
        embedded::apply(n, &mut ddl, &mut plan, &mut notes);
    }
    // Firebird: identity ALWAYS / BY DEFAULT, START WITH, INCREMENT BY.
    if info.id == "firebird" {
        embedded::firebird_identity_ddl(&mut *src, &source_t, &plan.table, &mut ddl).await?;
    }
    let source_cols = src.columns(&req.source).await?;
    // From the plan: its columns say which ones the engine computes
    // (CockroachDB's come as plain columns in `database_schema`).
    // Documents move whole (every field, not the sampled ones): no list.
    let loaded: Vec<String> = if matches!(info.id, "elasticsearch" | "opensearch" | "opendistro") {
        // The whole `_source` in one column, its keys in their order (a
        // full read splits the mapped fields into columns of their own).
        ["_id", "_routing", "_source"].map(str::to_string).to_vec()
    } else if documents::whole_documents(&*driver) {
        Vec::new()
    } else {
        // ClickHouse's MATERIALIZED columns are stored: copied as they are
        // (`now()`, `rand()` wouldn't compute the same values again).
        // SQL Server's column set: its values are the sparse columns'.
        plan.table
            .columns
            .iter()
            .filter(|c| !generated(c, info.dialect) || analytics::stored_computed(info, c))
            .filter(|c| mssql_x.column_set.as_deref() != Some(c.name.as_str()))
            .map(|c| c.name.clone())
            .collect()
    };
    let skipped: Vec<&str> =
        plan.table.columns.iter().filter(|c| generated(c, info.dialect) && !analytics::stored_computed(info, c)).map(|c| c.name.as_str()).collect();
    if req.options.with_data && !skipped.is_empty() {
        notes.push(format!("columnas calculadas por el motor (no se copian, se recalculan): {}", skipped.join(", ")));
    }
    let rows_total = if req.options.with_data && info.language == Language::Sql {
        scalar(&mut *src, &format!("SELECT COUNT(*) FROM {}", sql_name(&*driver, &source_t))).await.ok().flatten().map(|n| n.max(0) as u64)
    } else {
        None
    };
    notes.extend(triggers::note(&*driver, &mut *src, &source_t).await);
    control.check()?;

    // 5: create. From here on, any failure drops the clone.
    phase(ClonePhase::Create);
    let mut tgt = endpoints.open_target().await?;
    if let Err(e) = timeseries::create(&mut *tgt, &ddl).await {
        // A CREATE that failed because the name was taken meanwhile is
        // someone else's table: never dropped.
        if !name_taken(&e) {
            cleanup(&mut *tgt, &ddl.drop, &*events).await;
        }
        // Oracle: the table's name, or one of its constraints' or indexes'
        // (taken meanwhile), named from the catalog.
        if let Some(c) = oracle_clash(&mut *tgt, &e, &plan, true).await {
            return Err(c);
        }
        return Err(in_spanish(e));
    }
    let result = async {
        if (info.language == Language::Sql || info.language == Language::Cql) && !native.implicit {
            let clone_cols = tgt.columns(&target_ref).await?;
            // Firebird: a computed column's `T.V` is the clone's `"T_C".V`
            // (see `plan_clone_within`), the same column.
            let expected: Vec<dbine_driver::ColumnInfo> = if info.id == "firebird" {
                let bare = dbine_driver::sql::quote_ident(Quote::Double, &new_name);
                source_cols.iter().cloned().map(|mut c| {
                    c.data_type = embedded::requalify(&c.data_type, &source_t.name, &bare);
                    c
                }).collect()
            } else {
                source_cols.clone()
            };
            let diffs = column_differences(&expected, &clone_cols);
            if !diffs.is_empty() {
                return Err(Error::State(format!("el clon no quedó igual al original ({}); no se clona", diffs.join("; "))));
            }
        }
        timeseries::after_create(&mut *tgt, &native, &target_ref).await?;
        if let Some(p) = &pg_t {
            // Storage options and column settings, before the rows.
            pg::after_create(&mut *tgt, p, &plan.table, req.options.with_data).await?;
        }
        if let Some(a) = &analytic {
            a.check_clone(&mut *tgt, &target_ref).await?;
        }
        if let Some(m) = &my_t {
            // Before the rows: a clone that lost something is refused now.
            mysql::verify(&mut *tgt, m, &plan.renames, &source_t.name, &new_name, mysql::Stage::Created).await?;
        }
        if info.dialect == "mssql" && source_t.columns.iter().any(|c| c.auto_increment) {
            mssql_check_identity(&mut *tgt, &source_t, &plan.table).await?;
        }
        if let Some(p) = &mssql_pk {
            // Before the rows: a key that came out different is refused now.
            mssql::verify_primary_key(&mut *tgt, &plan.table, p).await?;
        }
        if !mssql_x.sparse.is_empty() || mssql_x.column_set.is_some() {
            mssql::verify_sparse(&mut *tgt, &plan.table, &mssql_x).await?;
        }
        if let Some(l) = &mssql_layout {
            // The heap (or the clustered key) where the original's table is.
            mssql::verify_layout(&mut *tgt, &plan.table, l, false).await?;
        }
        control.check()?;

        // 6: the rows.
        let mut rows = 0u64;
        if req.options.with_data {
            phase(ClonePhase::Copy);
            if info.dialect == "mssql" && !plan.table.checks.is_empty() {
                // Rows the original keeps against a disabled or untrusted
                // CHECK: each one gets the original's state afterwards.
                mssql::checks_off(&mut *tgt, &plan.table).await?;
            }
            let building = ObjectRef { name: plan.table.name.clone(), ..target_ref.clone() };
            rows = if analytic.is_some() {
                analytics::copy_in_server(&*driver, &mut *tgt, &req.source, &building, &loaded, rows_total, control, &*events).await?
            } else {
                copy_rows(&endpoints, &req, &plan.table, &building, &loaded, rows_total, control, events.clone()).await?
            };
            control.check()?;
            timeseries::after_copy(&mut *tgt, &native, &req.source, &target_ref, &source_cols, rows).await?;
            // SQLite: AUTOINCREMENT's counter may be past zero with no rows;
            // Firebird: the identity's counter too (rows deleted), and its
            // kind and options are checked. SQL Server: the original's
            // counter may be past its seed with no rows (all deleted).
            if rows > 0 || matches!(info.id, "sqlite" | "libsql" | "firebird") || info.dialect == "mssql" {
                phase(ClonePhase::Identity);
                if let Some(n) = resync_identity(&*driver, &mut *src, &mut *tgt, &source_t, &plan.table).await? {
                    notes.push(n);
                }
            }
        }
        if let Some(p) = &pg_t {
            // The sequences' options and current value, identity ALWAYS.
            if p.columns.iter().any(|c| c.generated.is_empty() && c.sequence.is_some()) {
                phase(ClonePhase::Identity);
            }
            pg::after_load(&mut *src, &mut *tgt, p, &plan.table, req.options.with_data)
                .await
                .map_err(|e| Error::Query(format!("identidad y secuencias: {e}")))?;
        }
        for sql in pg_deferred.iter().flat_map(|d| &d.checks) {
            exec(&mut *tgt, sql).await.map_err(|e| Error::Query(format!("restricciones CHECK: {}", in_spanish(e))))?;
        }
        oracle::after_load(&mut *tgt, &ora_after).await?;
        if restores_oracle_identity(&*driver) && plan.table.columns.iter().any(|c| c.auto_increment) {
            // The CREATE (and the load) leave the identity BY DEFAULT with
            // default options: back to the original's.
            phase(ClonePhase::Identity);
            restore_oracle_identity(&mut *src, &mut *tgt, &source_t, &plan.table, rows > 0).await?;
        }
        control.check()?;

        // 7: indexes, then foreign keys.
        if info.dialect == "mssql" && ddl.indexes.is_some() && mssql::resolve_key_index(&mut *tgt, &mut plan.table).await? {
            // The full-text index's KEY INDEX: the clone's primary key,
            // named by the server.
            ddl.indexes = Some(driver.table_ddl(&plan.table, DdlParts { indexes: true, ..Default::default() })?).filter(|s| !s.trim().is_empty());
            if let (Some(sql), Some(l)) = (ddl.indexes.as_mut(), &mssql_layout) {
                mssql::place_indexes(&*driver, sql, &plan.table, l)?;
            }
        }
        let mut indexes_started = false;
        if info.dialect == "mssql" && !server_uq.is_empty() && ddl.indexes.is_some() {
            // UNIQUE constraints named by the server: added unnamed; the
            // full-text index after them, on their server names.
            phase(ClonePhase::Indexes);
            indexes_started = true;
            ddl.indexes = mssql::server_named_uniques(&*driver, &mut *tgt, &mut plan.table, &server_uq, mssql_layout.as_ref()).await?;
        }
        if let Some(sql) = &ddl.indexes {
            if !indexes_started {
                phase(ClonePhase::Indexes);
            }
            if let Err(e) = exec(&mut *tgt, sql).await {
                let e = match oracle_clash(&mut *tgt, &e, &plan, false).await {
                    Some(c) => c,
                    None => in_spanish(e),
                };
                return Err(Error::Query(format!("índices: {e}")));
            }
        }
        if let Some(a) = &analytic {
            // StarRocks / Doris: rollups finished, synchronous materialized
            // views created, both checked against the original's.
            if a.has_views() && ddl.indexes.is_none() {
                phase(ClonePhase::Indexes);
            }
            a.finish(&*driver, &mut *tgt, &target_ref, control).await?;
        }
        control.check()?;
        if let Some(sql) = &ddl.foreign_keys {
            phase(ClonePhase::ForeignKeys);
            if let Err(e) = exec(&mut *tgt, sql).await {
                let e = match oracle_clash(&mut *tgt, &e, &plan, false).await {
                    Some(c) => c,
                    None => in_spanish(e),
                };
                return Err(Error::Query(format!("claves foráneas: {e}")));
            }
        }
        if info.dialect == "mssql" {
            if let Some(l) = &mssql_layout {
                // Each index where the original's is, with its partitions.
                mssql::verify_layout(&mut *tgt, &plan.table, l, ddl.indexes.is_some()).await?;
            }
            // CHECKs and foreign keys disabled, untrusted or NOT FOR
            // REPLICATION as the original's.
            mssql::constraints(&mut *src, &mut *tgt, &source_t, &plan.table, &plan.renames).await?;
            // Last: PERIOD and SYSTEM_VERSIONING, the history's rows.
            if let Some(n) = mssql::make_temporal(&mut *tgt, &plan.table, &mssql_x, req.options.with_data).await? {
                notes.push(n);
            }
            notes.extend(mssql::history_indexes(&*driver, &mut *tgt, &plan.table, &mssql_x, req.options.with_indexes).await?);
        }
        if let Some(o) = &ora_t {
            oracle::verify(&mut *tgt, o, &plan, req.options.with_indexes).await?;
        }
        if let Some(p) = &pg_t {
            // Comments on indexes and constraints (`database_schema` only
            // carries the table's and the columns'), on the renamed ones.
            // Row-level security and CockroachDB's zone configurations.
            notes.extend(pg::finish(&mut *tgt, p, &plan.table, &plan.renames, req.options.with_indexes).await?);
            notes.extend(pg::copy_comments(&mut *tgt, p, &plan.table, &plan.renames).await?);
            if p.comments.is_none() {
                notes.push("no se pudieron leer los comentarios de los índices y las restricciones del original: el clon no los tiene".into());
            }
            pg::verify(&mut *tgt, p, &plan.table, &plan.renames, req.options.with_indexes).await?;
        }
        if let Some(m) = &my_t {
            let stage = mysql::Stage::Done { with_indexes: req.options.with_indexes };
            mysql::verify(&mut *tgt, m, &plan.renames, &source_t.name, &new_name, stage).await?;
        }
        documents::finish(&mut *tgt, &plan, &prepared).await?;
        Ok(rows)
    }
    .await;
    match result {
        Ok(rows) => Ok(CloneReport { table: target_ref, rows, elapsed_ms: start.elapsed().as_millis() as u64, notes, renames: plan.renames }),
        Err(e) => {
            // A fresh connection: the one in use may be the one that failed.
            drop(tgt);
            match endpoints.open_target().await {
                Ok(mut t) => cleanup(&mut *t, &ddl.drop, &*events).await,
                Err(ce) => events(CloneEvent::Log { level: LogLevel::Error, text: format!("no se pudo borrar el clon a medias «{new_name}»: {ce}") }),
            }
            Err(e)
        }
    }
}

/// Drop a clone left halfway (best effort; says so when it can't).
async fn cleanup(s: &mut dyn Session, drop_sql: &str, events: &(dyn Fn(CloneEvent) + Send + Sync)) {
    events(CloneEvent::Phase { phase: ClonePhase::Cleanup });
    if let Err(e) = exec(s, drop_sql).await {
        events(CloneEvent::Log { level: LogLevel::Error, text: format!("no se pudo borrar el clon a medias: {e}") });
    }
}

/// The rows, through the bulk transfer engine (one table, one job).
#[allow(clippy::too_many_arguments)]
async fn copy_rows(
    endpoints: &Arc<dyn Endpoints>,
    req: &CloneRequest,
    clone: &TableSchema,
    target_ref: &ObjectRef,
    loaded: &[String],
    rows_total: Option<u64>,
    control: &CloneControl,
    events: Arc<dyn Fn(CloneEvent) + Send + Sync>,
) -> Result<u64> {
    let driver = endpoints.target_driver();
    let (before, after) = driver.data_load_wrap(clone);
    // Oracle: the driver's identity restart assumes an ascending identity
    // (the clone's, until `restore_oracle_identity` gives it the original's
    // options): with a descending one the loaded values fall below its
    // MINVALUE and the restart fails. `restore_oracle_identity` restarts it.
    // PostgreSQL's catalog: the same with its `setval(max + 1)` (a sequence
    // with default options, below the original's range: negative ids);
    // `pg::after_load` sets the original's options and current value.
    let after = if restores_oracle_identity(&*driver) || pg::applies(&*driver) { String::new() } else { after };
    let job = TransferJob {
        name: target_ref.name.clone(),
        // No columns: whole documents, and the load takes the read's.
        source: ReadSpec { table: req.source.clone(), columns: (!loaded.is_empty()).then(|| loaded.to_vec()), filter: None },
        target: LoadSpec {
            table: target_ref.clone(),
            columns: loaded.to_vec(),
            table_lock: true,
            keep_identity: true,
            commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
            commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
        },
        row_estimate: rows_total,
        truncate: truncate_sql(&*driver, clone),
        empty_first: false,
        before,
        after,
        post: Vec::new(),
        preexisting: false,
        expected_columns: Vec::new(),
        mode: TransferMode::Copy,
    };
    let dir = std::env::temp_dir();
    let tag = format!("dbine-clone-{}-{}", std::process::id(), chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default());
    let path = dir.join(format!("{tag}.sqlite"));
    let store = Arc::new(Store::open(&path)?);
    let engine = Engine::new(store.clone(), tag.clone());
    control.set_engine(Some(engine.control()));
    let options = RunOptions { parallel: 1, fail_fast: true, keep_identity: true, table_lock: true, ..Default::default() };
    let ev = events.clone();
    let wrapped: Arc<dyn Endpoints> = Arc::new(CloneEndpoints(endpoints.clone()));
    let report = engine
        .run(vec![job], options, wrapped, move |e| match e {
            Event::TableProgress { rows_done, rows_total, rows_per_s, .. } => ev(CloneEvent::Progress { rows_done, rows_total, rows_per_s }),
            Event::Log { level, text } => ev(CloneEvent::Log { level, text }),
            _ => {}
        })
        .await;
    control.set_engine(None);
    drop(engine);
    drop(store);
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(dir.join(format!("{tag}.sqlite{suffix}")));
    }
    let report = report?;
    let t = report.tables.into_iter().next().ok_or_else(|| Error::State("la copia no informó la tabla".into()))?;
    match t.status {
        TableStatus::Done => Ok(t.rows_done),
        TableStatus::Cancelled => Err(Error::Cancelled),
        _ if control.is_cancelled() => Err(Error::Cancelled),
        _ => Err(Error::State(t.error.unwrap_or_else(|| "la copia de las filas no terminó".into()))),
    }
}

/// Move the clone's identity / sequence past the copied values where the
/// driver's `data_load_wrap` doesn't (or the engine doesn't on its own),
/// so the clone's next insert doesn't collide. A note when it can't.
async fn resync_identity(driver: &dyn Driver, src: &mut dyn Session, tgt: &mut dyn Session, original: &TableSchema, clone: &TableSchema) -> Result<Option<String>> {
    if matches!(driver.info().id, "sqlite" | "libsql") {
        // AUTOINCREMENT's counter, where the original's is (see `embedded`).
        return embedded::copy_sqlite_sequence(src, tgt, original, clone).await.map(|_| None);
    }
    let Some(col) = clone.columns.iter().find(|c| c.auto_increment) else { return Ok(None) };
    let info = driver.info();
    let q = quote_of(info.dialect);
    let name = sql_name(driver, clone);
    let col_q = dbine_driver::sql::quote_ident(q, &col.name);
    match (info.dialect, info.id) {
        // `data_load_wrap` already did it (sequence setval, identity
        // restart), or the engine moves the counter past explicit values
        // by itself (AUTO_INCREMENT, SQLite's rowid / sqlite_sequence,
        // Informix SERIAL, Sybase identity).
        // StarRocks / Doris don't move AUTO_INCREMENT past explicit values.
        (_, "starrocks" | "doris" | "velodb") => analytics::resync(driver, tgt, &name, &col.name, &col_q).await,
        ("postgres" | "oracle" | "mysql" | "sqlite" | "informix" | "sybase", _) => Ok(None),
        ("mssql", _) => {
            // Where the original's identity stands (it may be past its
            // largest value, after deletes): the clone continues from there.
            let lit = qualified_name(Quote::Bracket, original.schema.as_deref().filter(|s| !s.is_empty()), &original.name).replace('\'', "''");
            // As text: a decimal(38,0) identity goes past bigint.
            let current = strings(src, &format!("SELECT CAST(IDENT_CURRENT(N'{lit}') AS nvarchar(50))"))
                .await
                .map_err(|e| Error::Query(format!("identidad: {e}")))?
                .into_iter()
                .next()
                .and_then(|r| r.into_iter().next().flatten())
                .map(|v| v.trim().to_string());
            if let Some(v) = current.as_deref().filter(|v| !mssql_integer(v)) {
                return Err(Error::State(format!("el motor informó un valor actual de IDENTITY que no se entiende («{v}»); no se clona")));
            }
            if current.is_none() {
                let target = name.replace('\'', "''");
                exec(tgt, &format!("DBCC CHECKIDENT (N'{target}', RESEED) WITH NO_INFOMSGS"))
                    .await
                    .map_err(|_| Error::State("no se pudo mover el IDENTITY del clon después de las filas copiadas; no se clona".into()))?;
                return Ok(None);
            }
            // Where the next row goes, whether or not the original and the
            // clone ever had a row (RESEED differs), checked afterwards.
            mssql::resync_identity(src, tgt, original, clone).await?;
            Ok(None)
        }
        // Its own sequences, created where the original's are (`embedded`).
        (_, "duckdb") => Ok(None),
        (_, "firebird") => embedded::firebird_identity(src, tgt, original, clone, &name).await,
        ("db2", _) => {
            let max = scalar(tgt, &format!("SELECT MAX({col_q}) FROM {name}")).await?.unwrap_or(0);
            // Db2 continues at the value: max + 1 never collides.
            exec(tgt, &format!("ALTER TABLE {name} ALTER COLUMN {col_q} RESTART WITH {}", max + 1))
                .await
                .map_err(|e| Error::Query(format!("identidad: {e}")))?;
            Ok(None)
        }
        _ => Ok(Some(format!(
            "la columna autoincremental «{}» conserva los valores copiados; revisá que su contador siga después del mayor antes de insertar filas nuevas",
            col.name
        ))),
    }
}

// -- Oracle ------------------------------------------------------------------------------------------

/// Oracle (and Autonomous Database): the clone's identity gets the
/// original's options and restarts past the loaded rows after the load
/// ([`restore_oracle_identity`]), not through the driver's `data_load_wrap`.
fn restores_oracle_identity(driver: &dyn Driver) -> bool {
    driver.info().dialect == "oracle"
}

/// Oracle's INVISIBLE columns: the reported structure leaves them out, so
/// the clone would lose them. Not readable: none (the column check after
/// the CREATE still catches it).
async fn oracle_invisible_columns(s: &mut dyn Session, t: &TableSchema) -> Vec<String> {
    let lit = |v: &str| format!("'{}'", v.replace('\'', "''"));
    let owner = t.schema.as_deref().filter(|s| !s.is_empty()).map(lit).unwrap_or_else(|| "SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')".into());
    // user_generated = 'NO': the engine's own hidden columns (unused ones,
    // function-based indexes' expressions), not the table's.
    let sql = format!(
        "SELECT column_name FROM all_tab_cols WHERE owner = {owner} AND table_name = {} \
         AND hidden_column = 'YES' AND user_generated = 'YES' ORDER BY internal_column_id",
        lit(&t.name)
    );
    strings(s, &sql).await.map(|rows| rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect()).unwrap_or_default()
}

/// The limit for the names the clone makes up, from the server: Oracle
/// takes 128 bytes when COMPATIBLE is 12.2 or more. Not readable (no access
/// to V$PARAMETER) or another engine: [`generated_limit`].
async fn server_generated_limit(driver: &dyn Driver, s: &mut dyn Session) -> usize {
    if driver.info().id == "firebird" {
        return embedded::firebird_generated_limit(s).await;
    }
    if driver.info().id != "oracle" {
        return generated_limit(driver);
    }
    let compatible = strings(s, "SELECT value FROM v$parameter WHERE name = 'compatible'")
        .await
        .ok()
        .and_then(|r| r.into_iter().next())
        .and_then(|r| r.into_iter().next().flatten());
    match compatible.as_deref().map(oracle_long_names) {
        Some(true) => 128,
        _ => generated_limit(driver),
    }
}

/// Whether an Oracle COMPATIBLE setting (`19.0.0`, `12.2.0.1`) takes
/// 128-byte identifiers (12.2 and later).
fn oracle_long_names(compatible: &str) -> bool {
    let mut parts = compatible.trim().split('.').map(|p| p.parse::<u32>().ok());
    match (parts.next().flatten(), parts.next().flatten()) {
        (Some(major), _) if major > 12 => true,
        (Some(12), Some(minor)) => minor >= 2,
        _ => false,
    }
}

/// Oracle's `ALL_TAB_IDENTITY_COLS.IDENTITY_OPTIONS` (`START WITH: 100,
/// INCREMENT BY: 5, MAX_VALUE: …, CACHE_SIZE: 20, …`) as (key, value) pairs.
fn oracle_identity_options(s: &str) -> Vec<(String, String)> {
    s.split(',')
        .filter_map(|p| p.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_uppercase(), v.trim().to_string()))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

/// The identity clause that gives a column the original's identity:
/// ALWAYS / BY DEFAULT [ON NULL] and its options, starting at `start`.
fn oracle_identity_clause(generation: &str, on_null: bool, options: &[(String, String)], start: &str) -> Result<String> {
    let get = |k: &str| options.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str());
    let number = |k: &str| get(k).filter(|v| !v.is_empty() && v.trim_start_matches('-').chars().all(|c| c.is_ascii_digit()));
    let flag = |k: &str| get(k) == Some("Y");
    let generation = match generation.trim().to_ascii_uppercase().as_str() {
        "ALWAYS" => "ALWAYS",
        "BY DEFAULT" if on_null => "BY DEFAULT ON NULL",
        "BY DEFAULT" => "BY DEFAULT",
        other => return Err(Error::Unsupported(format!("no se puede clonar: DBine no sabe recrear una identidad «{other}»"))),
    };
    let mut o = vec![format!("START WITH {start}")];
    if let Some(v) = number("INCREMENT BY") {
        o.push(format!("INCREMENT BY {v}"));
    }
    if let Some(v) = number("MAX_VALUE") {
        o.push(format!("MAXVALUE {v}"));
    }
    if let Some(v) = number("MIN_VALUE") {
        o.push(format!("MINVALUE {v}"));
    }
    o.push(if flag("CYCLE_FLAG") { "CYCLE" } else { "NOCYCLE" }.into());
    match number("CACHE_SIZE").and_then(|v| v.parse::<i64>().ok()) {
        Some(n) if n >= 2 => o.push(format!("CACHE {n}")),
        _ => o.push("NOCACHE".into()),
    }
    o.push(if flag("ORDER_FLAG") { "ORDER" } else { "NOORDER" }.into());
    if flag("SCALE_FLAG") {
        o.push(if flag("EXTEND_FLAG") { "SCALE EXTEND" } else { "SCALE NOEXTEND" }.into());
    }
    if flag("KEEP_VALUE") {
        o.push("KEEP".into());
    }
    Ok(format!("GENERATED {generation} AS IDENTITY ({})", o.join(" ")))
}

/// An Oracle identity column's (generation, ON NULL, options).
async fn oracle_identity(s: &mut dyn Session, t: &TableSchema, column: &str) -> Result<Option<(String, bool, Vec<(String, String)>)>> {
    let lit = |v: &str| format!("'{}'", v.replace('\'', "''"));
    let owner = t.schema.as_deref().filter(|s| !s.is_empty()).map(lit).unwrap_or_else(|| "SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')".into());
    let sql = format!(
        "SELECT i.generation_type, i.identity_options, c.default_on_null FROM all_tab_identity_cols i \
         JOIN all_tab_columns c ON c.owner = i.owner AND c.table_name = i.table_name AND c.column_name = i.column_name \
         WHERE i.owner = {owner} AND i.table_name = {} AND i.column_name = {}",
        lit(&t.name),
        lit(column)
    );
    Ok(strings(s, &sql).await?.into_iter().next().map(|r| {
        let mut r = r.into_iter();
        let generation = r.next().flatten().unwrap_or_default();
        let options = oracle_identity_options(&r.next().flatten().unwrap_or_default());
        let on_null = r.next().flatten().as_deref() == Some("YES");
        (generation, on_null, options)
    }))
}

/// Oracle: give the clone's identity columns the original's generation
/// (ALWAYS / BY DEFAULT [ON NULL]) and options (increment, cache, limits…),
/// starting one step past the loaded values (`loaded`) or where the
/// original's definition starts; then check they came out the same, or
/// refuse.
async fn restore_oracle_identity(src: &mut dyn Session, tgt: &mut dyn Session, original: &TableSchema, clone: &TableSchema, loaded: bool) -> Result<()> {
    let name = qualified_name(Quote::Double, clone.schema.as_deref().filter(|s| !s.is_empty()), &clone.name);
    for col in clone.columns.iter().filter(|c| c.auto_increment) {
        let fail = |why: String| Error::State(format!("la identidad de «{}» no quedó igual a la del original ({why}); no se clona", col.name));
        let (generation, on_null, options) = oracle_identity(src, original, &col.name).await?.ok_or_else(|| fail("el motor no la informa".into()))?;
        let col_q = dbine_driver::sql::quote_ident(Quote::Double, &col.name);
        let definition = options.iter().find(|(k, _)| k == "START WITH").map(|(_, v)| v.clone()).ok_or_else(|| fail("falta su valor inicial".into()))?;
        // Past the loaded values by one step, as the original would go on
        // (`START WITH LIMIT VALUE` restarts at the largest + 1).
        let increment = options.iter().find(|(k, _)| k == "INCREMENT BY").map(|(_, v)| v.clone()).filter(|v| v.parse::<i128>().is_ok()).unwrap_or_else(|| "1".into());
        let edge = if increment.starts_with('-') { "MIN" } else { "MAX" };
        let next = if loaded {
            strings(tgt, &format!("SELECT TO_CHAR({edge}({col_q}) + ({increment})) FROM {name}"))
                .await?
                .into_iter()
                .next()
                .and_then(|r| r.into_iter().next().flatten())
        } else {
            None
        };
        let start = next.unwrap_or(definition);
        let clause = oracle_identity_clause(&generation, on_null, &options, &start)?;
        exec(tgt, &format!("ALTER TABLE {name} MODIFY ({col_q} {clause});")).await.map_err(|e| {
            let m = e.to_string();
            if ["ORA-04006", "ORA-04008"].iter().any(|c| m.contains(c)) {
                // The original's own identity is used up too: it can't
                // number another row, and the clone can't be made that way.
                fail(format!(
                    "después de las filas copiadas le toca {start}, fuera de sus límites (MINVALUE / MAXVALUE): el original ya no puede numerar filas nuevas"
                ))
            } else {
                fail(format!("el motor no aceptó sus opciones: {}", in_spanish(e)))
            }
        })?;
        let (g2, n2, o2) = oracle_identity(tgt, clone, &col.name).await?.ok_or_else(|| fail("el clon no la informa".into()))?;
        // START WITH is where each one is now (the clone, past its rows).
        let rest = |o: &[(String, String)]| o.iter().filter(|(k, _)| k != "START WITH").cloned().collect::<Vec<_>>();
        if !g2.eq_ignore_ascii_case(&generation) || n2 != on_null || rest(&o2) != rest(&options) {
            let show = |g: &str, n: bool, o: &[(String, String)]| {
                format!("{g}{} {}", if n { " ON NULL" } else { "" }, rest(o).iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join(", "))
            };
            return Err(fail(format!("original: {}; clon: {}", show(&generation, on_null, &options), show(&g2, n2, &o2))));
        }
    }
    Ok(())
}

// -- connections from a config (tests, tools) -------------------------------------------------------

/// [`Endpoints`] over a driver and a connection config, both sides in the
/// same database (the source side opened read-only). The app has its own
/// (tunnels, keychain); this one is for tests and tools.
pub struct ConfigEndpoints {
    pub driver: Arc<dyn Driver>,
    pub config: ConnectionConfig,
    pub database: Option<String>,
}

#[async_trait]
impl Endpoints for ConfigEndpoints {
    fn source_driver(&self) -> Arc<dyn Driver> {
        self.driver.clone()
    }
    fn target_driver(&self) -> Arc<dyn Driver> {
        self.driver.clone()
    }
    async fn open_source(&self) -> Result<Box<dyn Session>> {
        let mut cfg = self.config.clone();
        cfg.read_only = true;
        let s = self.driver.connect(&cfg, self.database.as_deref()).await?;
        Ok(if self.driver.info().language == Language::Sql { Box::new(dbine_driver::read_only::ReadOnlySession::new(s)) } else { s })
    }
    async fn open_target(&self) -> Result<Box<dyn Session>> {
        self.driver.connect(&self.config, self.database.as_deref()).await
    }
}

/// Columns as the transfer describes them.
pub fn transfer_columns(cols: Vec<dbine_driver::ColumnInfo>) -> Vec<TransferColumn> {
    cols.into_iter().map(|c| TransferColumn { name: c.name, type_name: c.data_type, nullable: c.nullable }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integers_as_text_stay_exact() {
        // Past 2^53: through f64 it would round to ...992.
        assert_eq!(int_text("9007199254740993"), Some(9007199254740993));
        assert_eq!(int_text("9223372036854775807"), Some(i64::MAX));
        assert_eq!(int_text("12.0"), Some(12));
        assert_eq!(int_text("12.5"), None);
        assert_eq!(int_text("9223372036854775808"), None);
    }
    use dbine_driver::{CheckDef, DriverInfo, ForeignKeyDef, IndexDef, ObjectKindInfo};

    struct D(DriverInfo);

    #[async_trait]
    impl Driver for D {
        fn info(&self) -> &DriverInfo {
            &self.0
        }
        async fn connect(&self, _: &ConnectionConfig, _: Option<&str>) -> Result<Box<dyn Session>> {
            Err(Error::Unsupported("test".into()))
        }
    }

    fn driver(id: &'static str, dialect: &'static str, family: Family, language: Language) -> D {
        D(DriverInfo {
            id,
            name: id,
            family,
            language,
            dialect,
            default_port: 0,
            fields: vec![],
            databases_label: "",
            has_schemas: true,
            object_kinds: vec![ObjectKindInfo::tables(), ObjectKindInfo::views(), ObjectKindInfo::procedures()],
        })
    }

    fn pg() -> D {
        driver("postgres", "postgres", Family::Relational, Language::Sql)
    }

    fn sample() -> TableSchema {
        TableSchema {
            kind: "table".into(),
            schema: Some("public".into()),
            name: "clientes".into(),
            columns: vec![
                ColumnDef { name: "id".into(), data_type: "integer".into(), nullable: false, auto_increment: true, ..Default::default() },
                ColumnDef { name: "padre".into(), data_type: "integer".into(), ..Default::default() },
                ColumnDef { name: "pais".into(), data_type: "integer".into(), ..Default::default() },
            ],
            primary_key: Some(KeyDef { name: Some("clientes_pkey".into()), columns: vec!["id".into()] }),
            foreign_keys: vec![
                ForeignKeyDef { name: Some("fk_padre".into()), columns: vec!["padre".into()], ref_schema: Some("public".into()), ref_table: "clientes".into(), ref_columns: vec!["id".into()], ..Default::default() },
                ForeignKeyDef { name: Some("clientes_pais_fkey".into()), columns: vec!["pais".into()], ref_schema: Some("public".into()), ref_table: "paises".into(), ref_columns: vec!["id".into()], ..Default::default() },
            ],
            indexes: vec![
                IndexDef { name: "ix_Clientes_pais".into(), columns: vec!["pais".into()], ..Default::default() },
                IndexDef { name: "otro".into(), columns: vec!["padre".into()], unique: true, ..Default::default() },
            ],
            checks: vec![CheckDef { name: Some("ck_id".into()), expression: "id > 0".into() }],
            ..Default::default()
        }
    }

    #[test]
    fn proposed_name() {
        let at = chrono::NaiveDate::from_ymd_opt(2026, 9, 30).unwrap().and_hms_opt(7, 5, 9).unwrap();
        assert_eq!(default_clone_name("clientes", at), "clientes_20260930_070509");
    }

    #[test]
    fn renames_every_named_constraint_and_index() {
        let p = plan_clone(&pg(), &sample(), "clientes_20260930_070509").unwrap();
        let t = &p.table;
        assert_eq!(t.name, "clientes_20260930_070509");
        assert_eq!(t.schema.as_deref(), Some("public"));
        assert_eq!(t.primary_key.as_ref().unwrap().name.as_deref(), Some("clientes_20260930_070509_pkey"));
        // The table's name inside, matched ignoring case, keeps the rest.
        assert_eq!(t.indexes[0].name, "ix_clientes_20260930_070509_pais");
        // Not inside: the clone's name goes in front.
        assert_eq!(t.indexes[1].name, "clientes_20260930_070509_otro");
        assert_eq!(t.checks[0].name.as_deref(), Some("clientes_20260930_070509_ck_id"));
        assert_eq!(t.foreign_keys[0].name.as_deref(), Some("clientes_20260930_070509_fk_padre"));
        assert_eq!(t.foreign_keys[1].name.as_deref(), Some("clientes_20260930_070509_pais_fkey"));
        // A self reference points to the clone; others to the same parent.
        assert_eq!(t.foreign_keys[0].ref_table, "clientes_20260930_070509");
        assert_eq!(t.foreign_keys[1].ref_table, "paises");
        assert_eq!(p.renames.len(), 6);
        assert!(p.renames.iter().all(|r| !r.shortened));
        // Columns untouched.
        assert_eq!(t.columns, sample().columns);
        // Deterministic.
        assert_eq!(plan_clone(&pg(), &sample(), "clientes_20260930_070509").unwrap().table, p.table);
    }

    #[test]
    fn long_names_fit_the_engine_with_a_hash() {
        let long = "a".repeat(60);
        let (n, short) = rename_constraint("clientes_pkey", "clientes", &long, 63);
        assert!(short);
        assert!(n.len() <= 63, "{n}");
        assert!(n.starts_with(&long[..40]));
        let (m, _) = rename_constraint("clientes_fkey", "clientes", &long, 63);
        assert_ne!(n, m, "two long names never end up equal");
        // Oracle: made-up names within 30 even though tables take 128.
        let ora = driver("oracle", "oracle", Family::Relational, Language::Sql);
        let mut t = sample();
        t.schema = Some("APP".into());
        let p = plan_clone(&ora, &t, "clientes_20260930_070509").unwrap();
        assert!(p.renames.iter().all(|r| r.to.len() <= 30), "{:?}", p.renames);
        assert!(p.renames.iter().any(|r| r.shortened));
        assert!(p.notes.iter().any(|n| n.contains("acortados")));
        // Unique after shortening.
        let mut names: Vec<&str> = p.renames.iter().map(|r| r.to.as_str()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), p.renames.len());
        // Multi-byte names are cut on a character boundary.
        let (u, _) = rename_constraint("ñandú_pkey", "ñandú", &"ñ".repeat(40), 63);
        assert!(u.len() <= 63 && u.is_char_boundary(u.len()));
    }

    #[test]
    fn odd_names_keep_their_characters() {
        let mut t = sample();
        t.name = "mis \"clientes\"".into();
        t.primary_key.as_mut().unwrap().name = Some("pk mis \"clientes\"".into());
        let p = plan_clone(&pg(), &t, "copia ]de[ `x`").unwrap();
        assert_eq!(p.table.primary_key.as_ref().unwrap().name.as_deref(), Some("pk copia ]de[ `x`"));
        assert_eq!(sql_name(&pg(), &p.table), "\"public\".\"copia ]de[ `x`\"");
        let ms = driver("sqlserver", "mssql", Family::Relational, Language::Sql);
        assert_eq!(sql_name(&ms, &p.table), "[public].[copia ]]de[ `x`]");
    }

    #[test]
    fn refuses_bad_names() {
        assert!(plan_clone(&pg(), &sample(), "  ").is_err());
        assert!(plan_clone(&pg(), &sample(), "clientes").is_err());
        // PostgreSQL would cut it silently: refused.
        let e = plan_clone(&pg(), &sample(), &"x".repeat(64)).unwrap_err().to_string();
        assert!(e.contains("63"), "{e}");
        assert!(plan_clone(&pg(), &sample(), &"x".repeat(63)).is_ok());
    }

    #[test]
    fn sql_server_counts_characters() {
        let ms = driver("sqlserver", "mssql", Family::Relational, Language::Sql);
        let mut t = sample();
        t.schema = Some("dbo".into());
        // 100 characters, 200 bytes: fits in sysname.
        assert!(plan_clone(&ms, &t, &"ñ".repeat(100)).is_ok());
        assert!(plan_clone(&ms, &t, &"ñ".repeat(128)).is_ok());
        let e = plan_clone(&ms, &t, &"ñ".repeat(129)).unwrap_err().to_string();
        assert!(e.contains("128"), "{e}");
        // PostgreSQL's limit is in bytes: 32 × 2 = 64 > 63.
        assert!(plan_clone(&pg(), &sample(), &"ñ".repeat(32)).is_err());
        // The made-up names are counted in characters too: 200 bytes, not cut.
        let p = plan_clone(&ms, &t, &"ñ".repeat(100)).unwrap();
        assert!(p.renames.iter().all(|r| !r.shortened), "{:?}", p.renames);
        assert!(p.notes.iter().all(|n| !n.contains("acortados")), "{:?}", p.notes);
        let p = plan_clone(&ms, &t, &"ñ".repeat(128)).unwrap();
        assert!(p.renames.iter().any(|r| r.shortened));
        assert!(p.renames.iter().all(|r| r.to.chars().count() <= 128), "{:?}", p.renames);
        assert!(p.notes.iter().any(|n| n.contains("128 caracteres")), "{:?}", p.notes);
    }

    #[test]
    fn sql_server_identity_values_are_integers() {
        assert!(mssql_integer("99999999999999999999999999999999"));
        assert!(mssql_integer("-5"));
        assert!(!mssql_integer("1.5"));
        assert!(!mssql_integer(""));
        assert!(!mssql_integer("1; DROP TABLE x"));
    }

    #[test]
    fn names_in_use_are_avoided() {
        let first = plan_clone(&pg(), &sample(), "clientes_2").unwrap();
        let pk = first.table.primary_key.as_ref().unwrap().name.clone().unwrap();
        assert_eq!(pk, "clientes_2_pkey");
        let p = plan_clone_with(&pg(), &sample(), "clientes_2", &["CLIENTES_2_PKEY".into()]).unwrap();
        let pk2 = p.table.primary_key.as_ref().unwrap().name.clone().unwrap();
        assert!(!pk2.eq_ignore_ascii_case(&pk) && pk2.starts_with("clientes_2_pkey_"), "{pk2}");
        assert!(p.notes.iter().any(|n| n.contains("ya usaba otro objeto") && n.contains("clientes_pkey →")), "{:?}", p.notes);
        // The others keep their first choice.
        assert_eq!(p.table.indexes, first.table.indexes);
    }

    #[test]
    fn named_defaults_are_renamed() {
        let ms = driver("sqlserver", "mssql", Family::Relational, Language::Sql);
        let mut t = sample();
        t.columns[2].default_value = Some("((0))".into());
        t.columns[2].options.insert(DEFAULT_NAME_OPTION.into(), "DF_clientes_pais".into());
        let p = plan_clone(&ms, &t, "clientes_2").unwrap();
        assert_eq!(p.table.columns[2].options.get(DEFAULT_NAME_OPTION).map(String::as_str), Some("DF_clientes_2_pais"));
        assert!(p.renames.iter().any(|r| r.from == "DF_clientes_pais"));
    }

    #[test]
    fn taken_names_in_sql_server_messages() {
        let e = Error::Query("There is already an object named 'UQ_x' in the database.".into());
        assert!(name_taken(&e));
        assert!(name_taken(&Error::Query("relation \"x\" already exists".into())));
        assert!(!name_taken(&Error::Query("Incorrect syntax near 'x'.".into())));
        let s = in_spanish(e).to_string();
        assert!(s.contains("«UQ_x»") && s.contains("ya existe"), "{s}");
        assert_eq!(in_spanish(Error::Query("otro".into())).to_string(), Error::Query("otro".into()).to_string());
        // Elasticsearch / OpenSearch, two clones with the same name at once.
        let es = Error::Query("resource_already_exists_exception: index [clonef_r/Xy9_aBc] already exists".into());
        assert!(name_taken(&es));
        let s = in_spanish(es).to_string();
        assert!(s.contains("ya existe un objeto llamado «clonef_r»"), "{s}");
    }

    #[test]
    fn mysql_names_in_use_are_looked_up_schema_wide() {
        let names = vec!["c3x_ck".to_string(), "FK_O'K".to_string()];
        let sql = mysql_taken_sql("mysql", Some("rv"), &names);
        assert!(sql.contains("CONSTRAINT_SCHEMA = 'rv'") && sql.contains("IN ('FOREIGN KEY', 'CHECK')"), "{sql}");
        assert!(sql.contains("LOWER(CONSTRAINT_NAME) IN ('c3x_ck', 'fk_o''k')"), "{sql}");
        // MariaDB: CHECK names belong to the table; no schema: the session's.
        let sql = mysql_taken_sql("mariadb", None, &names[..1]);
        assert!(sql.contains("CONSTRAINT_SCHEMA = DATABASE()") && sql.contains("IN ('FOREIGN KEY')") && !sql.contains("'CHECK'"), "{sql}");
        // A clash that slipped through (made meanwhile), told in Spanish.
        let s = in_spanish(Error::Query("Duplicate check constraint name 'c3x_ck'.".into())).to_string();
        assert!(s.contains("restricción CHECK llamada «c3x_ck»") && !s.contains("Duplicate"), "{s}");
        let s = in_spanish(Error::Query("Duplicate foreign key constraint name 'fk_c3x_p'".into())).to_string();
        assert!(s.contains("clave foránea llamada «fk_c3x_p»"), "{s}");
        let s = in_spanish(Error::Query("Can't create table `rv`.`c3x` (errno: 121 \"Duplicate key on write or update\")".into())).to_string();
        assert!(s.contains("ya tiene una clave foránea") && !s.contains("errno"), "{s}");
        // Taken names are avoided when planning.
        let my = driver("mysql", "mysql", Family::Relational, Language::Sql);
        let mut t = TableSchema { name: "c3".into(), ..Default::default() };
        t.checks.push(dbine_driver::CheckDef { name: Some("ck".into()), expression: "a > 0".into() });
        t.foreign_keys.push(dbine_driver::ForeignKeyDef {
            name: Some("fk_c3_p".into()),
            columns: vec!["p".into()],
            ref_table: "c3".into(),
            ref_columns: vec!["id".into()],
            ..Default::default()
        });
        let p = plan_clone_with(&my, &t, "c3x", &["C3X_CK".into(), "fk_c3x_p".into()]).unwrap();
        let to: Vec<&str> = p.renames.iter().map(|r| r.to.as_str()).collect();
        assert!(to.iter().all(|n| !n.eq_ignore_ascii_case("c3x_ck") && *n != "fk_c3x_p"), "{to:?}");
        assert!(p.notes.iter().any(|n| n.contains("ya usaba otro objeto")), "{:?}", p.notes);
    }

    #[test]
    fn oracle_names_in_use_are_looked_up_schema_wide() {
        let names = vec!["PK_CLONV_COL_X".to_string(), "IX_O'K".to_string()];
        let sql = oracle_taken_sql(Some("DBINE"), &names, true);
        assert!(sql.contains("FROM all_objects WHERE owner = 'DBINE' AND object_name IN ('PK_CLONV_COL_X', 'IX_O''K')"), "{sql}");
        assert!(sql.contains("FROM all_constraints WHERE owner = 'DBINE' AND constraint_name IN ('PK_CLONV_COL_X', 'IX_O''K')"), "{sql}");
        let sql = oracle_taken_sql(None, &names[..1], false);
        assert!(sql.contains("owner = SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')") && !sql.contains("all_constraints"), "{sql}");
        // A clash found in the catalog is named, in Spanish; the table's
        // name isn't blamed for a constraint's.
        let s = oracle_clash_text("CLONV_COL_X", &names, &names[..1]);
        assert!(s.contains("«PK_CLONV_COL_X»") && !s.contains("«CLONV_COL_X»") && !s.contains("ORA-"), "{s}");
        let s = oracle_clash_text("CLONV_COL_X", &names, &[]);
        assert!(s.contains("«CLONV_COL_X», «PK_CLONV_COL_X»"), "{s}");
    }

    #[test]
    fn mysql_primary_and_document_names_stay() {
        let my = driver("mysql", "mysql", Family::Relational, Language::Sql);
        let mut t = sample();
        t.primary_key.as_mut().unwrap().name = Some("PRIMARY".into());
        let p = plan_clone(&my, &t, "c2").unwrap();
        assert_eq!(p.table.primary_key.unwrap().name.as_deref(), Some("PRIMARY"));
        // Index names belong to the collection there: kept.
        let mongo = driver("mongodb", "", Family::Document, Language::Json);
        let p = plan_clone(&mongo, &t, "c2").unwrap();
        assert_eq!(p.table.indexes[0].name, "ix_Clientes_pais");
        assert!(p.renames.is_empty());
    }

    #[test]
    fn what_is_refused() {
        let d = pg();
        assert!(check_cloneable(&d, "table").is_ok());
        assert!(check_cloneable(&d, "view").is_err());
        assert!(check_cloneable(&d, "procedure").is_err());
        let redis = driver("redis", "", Family::KeyValue, Language::Redis);
        assert!(check_cloneable(&redis, "key").is_err());
        let neo = driver("neo4j", "neo4j", Family::Graph, Language::Cypher);
        assert!(check_cloneable(&neo, "label").is_err());
        let ksql = driver("ksqldb", "ksql", Family::Streaming, Language::Sql);
        assert!(check_cloneable(&ksql, "table").is_err());
    }

    #[test]
    fn existing_names_ignoring_case() {
        let o = ObjectRef { kind: "table".into(), schema: Some("Public".into()), name: "Clientes_2".into() };
        assert!(same_object(Some("public"), "clientes_2", &o));
        assert!(!same_object(Some("otro"), "clientes_2", &o));
        assert!(same_object(None, "CLIENTES_2", &o));
    }

    #[test]
    fn generated_columns_are_not_loaded() {
        let c = |t: &str| ColumnDef { name: "x".into(), data_type: t.into(), ..Default::default() };
        assert!(generated(&c("integer GENERATED ALWAYS AS (a + 1) STORED"), "postgres"));
        assert!(generated(&c("AS ([a] * 2)"), "mssql"));
        assert!(generated(&c("rowversion"), "mssql"));
        assert!(!generated(&c("timestamp"), "postgres"));
        assert!(!generated(&c("integer"), "postgres"));
    }

    #[test]
    fn oracle_identity_is_recreated_as_the_original() {
        let o = oracle_identity_options(
            "START WITH: 100, INCREMENT BY: 5, MAX_VALUE: 9999999999999999999999999999, MIN_VALUE: 1, CYCLE_FLAG: N, CACHE_SIZE: 20, ORDER_FLAG: N, SCALE_FLAG: N, EXTEND_FLAG: N, SESSION_FLAG: N, KEEP_VALUE: N",
        );
        assert_eq!(o[0], ("START WITH".to_string(), "100".to_string()));
        assert_eq!(
            oracle_identity_clause("ALWAYS", false, &o, "LIMIT VALUE").unwrap(),
            "GENERATED ALWAYS AS IDENTITY (START WITH LIMIT VALUE INCREMENT BY 5 MAXVALUE 9999999999999999999999999999 MINVALUE 1 NOCYCLE CACHE 20 NOORDER)"
        );
        // Empty clone: where the original's definition starts.
        let c = oracle_identity_clause("BY DEFAULT", true, &o, "100").unwrap();
        assert!(c.starts_with("GENERATED BY DEFAULT ON NULL AS IDENTITY (START WITH 100 INCREMENT BY 5 "), "{c}");
        let o = oracle_identity_options("START WITH: 1, INCREMENT BY: -1, MAX_VALUE: -1, MIN_VALUE: -99, CYCLE_FLAG: Y, CACHE_SIZE: 0, ORDER_FLAG: Y, SCALE_FLAG: Y, EXTEND_FLAG: Y, KEEP_VALUE: Y");
        assert_eq!(
            oracle_identity_clause("BY DEFAULT", false, &o, "LIMIT VALUE").unwrap(),
            "GENERATED BY DEFAULT AS IDENTITY (START WITH LIMIT VALUE INCREMENT BY -1 MAXVALUE -1 MINVALUE -99 CYCLE NOCACHE ORDER SCALE EXTEND KEEP)"
        );
        assert!(oracle_identity_clause("SOMETIMES", false, &o, "1").is_err());
    }

    #[test]
    fn oracle_generated_names_follow_compatible() {
        assert!(oracle_long_names("23.0.0"));
        assert!(oracle_long_names("19.0.0"));
        assert!(oracle_long_names("12.2.0.1"));
        assert!(!oracle_long_names("12.1.0.2"));
        assert!(!oracle_long_names("11.2.0"));
        assert!(!oracle_long_names(""));
        // With 128 allowed, the made-up names keep their words.
        let ora = driver("oracle", "oracle", Family::Relational, Language::Sql);
        let mut t = sample();
        t.schema = Some("APP".into());
        let p = plan_clone_within(&ora, &t, "clientes_20260930_070509", &[], 128).unwrap();
        assert!(p.renames.iter().all(|r| !r.shortened), "{:?}", p.renames);
        assert_eq!(p.table.indexes[0].name, "ix_clientes_20260930_070509_pais");
    }

    #[test]
    fn a_table_reported_without_its_schema_is_found_in_the_current_one() {
        let mut t = sample();
        t.schema = None;
        t.name = "CLIENTES".into();
        let obj = |s: Option<&str>| ObjectRef { kind: "table".into(), schema: s.map(Into::into), name: "CLIENTES".into() };
        // Oracle reports its tables with no schema: DBINE.CLIENTES is the
        // session's when DBINE is the current schema...
        assert!(find_table(vec![t.clone()], &obj(Some("DBINE")), Some("DBINE")).is_some());
        assert!(find_table(vec![t.clone()], &obj(None), None).is_some());
        // ...and never another schema's table with the same name.
        assert!(find_table(vec![t.clone()], &obj(Some("OTRO")), Some("DBINE")).is_none());
        assert!(find_table(vec![t.clone()], &obj(Some("DBINE")), None).is_none());
        assert!(find_table(vec![t.clone()], &obj(Some("dbine")), Some("DBINE")).is_none());
        // Not found where the engine reports full structures: refused, not
        // cloned from its columns alone.
        assert!(reports_full_structure(&[sample()], "postgres"));
        let mut bare = sample();
        bare.indexes.clear();
        bare.foreign_keys.clear();
        bare.checks.clear();
        bare.primary_key.as_mut().unwrap().name = None;
        assert!(!reports_full_structure(&[bare.clone()], "postgres"));
        bare.columns.push(ColumnDef { name: "MONTO".into(), data_type: "NUMBER GENERATED ALWAYS AS (1) VIRTUAL".into(), ..Default::default() });
        assert!(reports_full_structure(&[bare], "oracle"));
    }

    #[test]
    fn a_lost_column_is_named() {
        let c = |n: &str| dbine_driver::ColumnInfo {
            name: n.into(),
            data_type: "NUMBER".into(),
            nullable: true,
            primary_key: false,
            auto_increment: false,
            default_value: None,
        };
        let d = column_differences(&[c("ID"), c("SECRETO"), c("NOMBRE")], &[c("ID"), c("NOMBRE")]);
        assert_eq!(d, vec!["3 columnas en el original, 2 en el clon; faltan en el clon: «SECRETO»".to_string()]);
        let d = column_differences(&[c("ID")], &[c("ID"), c("OTRA")]);
        assert_eq!(d, vec!["1 columnas en el original, 2 en el clon; sobran en el clon: «OTRA»".to_string()]);
    }

    #[test]
    fn self_reference_to_a_unique_needs_indexes() {
        let fk = |to: &str, col: &str| ForeignKeyDef {
            name: Some("fk_parent".into()),
            columns: vec!["parent".into()],
            ref_schema: Some("public".into()),
            ref_table: to.into(),
            ref_columns: vec![col.into()],
            ..Default::default()
        };
        let mut t = TableSchema {
            schema: Some("public".into()),
            name: "cat".into(),
            primary_key: Some(dbine_driver::KeyDef { name: None, columns: vec!["id".into()] }),
            foreign_keys: vec![fk("cat", "id"), fk("other", "code")],
            ..Default::default()
        };
        // To the primary key (kept without indexes), or to another table.
        assert!(self_references_without_indexes(&t).is_ok());
        t.foreign_keys.push(fk("cat", "code"));
        let e = self_references_without_indexes(&t).unwrap_err().to_string();
        assert!(e.contains("fk_parent") && e.contains("con índices"), "{e}");
    }
}
