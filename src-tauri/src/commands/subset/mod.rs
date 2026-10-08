//! "Copiar un subconjunto…" on a table (docs/subconjunto-de-datos.md): some
//! rows of a table, every parent row they need (recursively) and,
//! optionally, the rows that hang from them, copied to another database
//! (same engine or not) with the personal data masked on the way.
//!
//! 1. [`prepare`]: the source's structure, the tables in the subset
//!    ([`graph::scope`]), their order with parents first (FK cycles cut,
//!    [`graph::order`]) and where each goes in the target: an existing
//!    table, or one created from the source's structure (converted with
//!    `dbine_schema` when the engines differ).
//! 2. [`Collector`]: the rows, read on a read-only session of the source;
//!    keys gathered in Rust and fetched in chunks with the driver's own
//!    `IN` filter (`Driver::filtered_browse`).
//! 3. [`write`]: masked in DBine ([`mask`]), inserted in batches with the
//!    driver's `insert_script`, as test data is.
//!
//! The plan runs steps 1 and 2 to show what will be copied; the copy runs
//! them again (the data may have changed) and writes. The source is never
//! written: its session is read-only and the target can't be the same
//! database.
//!
//! Engines without foreign keys (documents, key-value, time series) copy
//! the one table or collection with its filter, masked.

mod graph;
mod mask;

use crate::commands::migration::{create_schema, default_schema, default_schema_rename};
use crate::commands::schema::driver_of;
use crate::error::{CommandError, CommandResult};
use crate::state::{AppState, SessionEntry};
use dbine_driver::{ColumnDef, ColumnFilter, DdlParts, Driver, FilterOp, KeyDef, ObjectRef, QueryOutcome, RowChange, Session, TableSchema};
use graph::{Edge, Role, Scope};
use mask::{MaskRule, Masker, Shape};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};

/// Keys per `IN (…)`.
const CHUNK: usize = 500;
/// Rows per INSERT script.
const BATCH: usize = 500;
/// Rows a key chunk may bring (a composite key filters on one column and
/// the rest in Rust).
const FETCH_LIMIT: u32 = 200_000;
/// Rows of the starting table read at most ("all", or the base of a %).
const SCAN_LIMIT: u32 = 1_000_000;
/// Rows in a subset at most: past this it isn't a subset.
const MAX_ROWS: usize = 2_000_000;
const PROGRESS_EVERY: Duration = Duration::from_millis(300);

/// Progress events, already tagged with the run id (`subset-progress`).
pub type Emit = Arc<dyn Fn(Value) + Send + Sync>;

// -- arguments -------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct SubsetArgs {
    /// The UI's id: events carry it, and `cancel_query` stops the run on
    /// `subset:<id>:src` and `subset:<id>:tgt`.
    pub run_id: String,
    pub connection_id: String,
    pub database: String,
    pub table: ObjectRef,
    #[serde(default)]
    pub filter: StartFilter,
    /// Also the rows that hang from the chosen ones; `None`: only parents.
    #[serde(default)]
    pub children: Option<ChildrenOptions>,
    pub target_connection_id: String,
    #[serde(default)]
    pub target_database: String,
}

/// Which rows of the starting table.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StartFilter {
    /// A condition in the engine's language (`WHERE` without the word).
    #[serde(default)]
    pub expression: Option<String>,
    /// The data grid's column filters (engines without a condition
    /// language: MongoDB, CouchDB…).
    #[serde(default)]
    pub columns: Vec<ColumnFilter>,
    #[serde(default)]
    pub limit: Limit,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Limit {
    #[default]
    All,
    Rows {
        count: u64,
    },
    Percent {
        percent: f64,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChildrenOptions {
    /// Levels down from the starting table (1 = its direct children).
    pub depth: u32,
    /// Rows at most per child table.
    pub max_rows: u64,
}

/// A table's masking rules, by column.
#[derive(Debug, Clone, Deserialize)]
pub struct TableMask {
    #[serde(default)]
    pub schema: Option<String>,
    pub name: String,
    #[serde(default)]
    pub columns: BTreeMap<String, MaskRule>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RunArgs {
    #[serde(flatten)]
    pub subset: SubsetArgs,
    #[serde(default)]
    pub masks: Vec<TableMask>,
    /// The target database's name, typed by the user (production targets).
    #[serde(default)]
    pub confirm: String,
    /// The masking seed (tests); a random one per run otherwise.
    #[serde(default)]
    pub seed: Option<u64>,
}

// -- results ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct SubsetPlan {
    /// In write order.
    pub tables: Vec<PlanTable>,
    pub total_rows: u64,
    /// FK cycles cut, as sentences.
    pub cycles: Vec<String>,
    pub notes: Vec<String>,
    /// The target is tagged as production: the copy asks to type this.
    pub confirm_label: Option<String>,
    pub target_engine: String,
}

#[derive(Debug, Serialize)]
pub struct PlanTable {
    pub schema: Option<String>,
    pub name: String,
    /// `start`, `parent` or `child`.
    pub role: &'static str,
    pub depth: u32,
    pub rows: u64,
    /// The child table hit its row cap.
    pub capped: bool,
    pub target: String,
    pub exists: bool,
    /// The DDL that creates it in the target (when it doesn't exist).
    pub create_ddl: Option<String>,
    /// Why it can't be copied.
    pub error: Option<String>,
    pub columns: Vec<PlanColumn>,
}

#[derive(Debug, Serialize)]
pub struct PlanColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    /// Part of the primary key or of a foreign key.
    pub key: bool,
    /// The rule suggested for personal data (`keep` for the rest).
    pub suggested: MaskRule,
    /// Why it isn't copied (calculated, missing in the target).
    pub skipped: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SubsetReport {
    pub tables: Vec<TableReport>,
    pub notes: Vec<String>,
    pub elapsed_ms: u64,
    pub cancelled: bool,
}

#[derive(Debug, Serialize)]
pub struct TableReport {
    pub table: String,
    pub target: String,
    pub created: bool,
    pub rows: u64,
    pub written: u64,
    pub masked: Vec<String>,
    /// `done`, `error`, `cancelled` or `skipped` (not reached).
    pub status: &'static str,
    pub error: Option<String>,
    pub notes: Vec<String>,
}

// -- helpers ---------------------------------------------------------------------------

fn label(schema: Option<&str>, name: &str) -> String {
    match schema.filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{name}"),
        None => name.to_string(),
    }
}

fn table_label(t: &TableSchema) -> String {
    label(t.schema.as_deref(), &t.name)
}

fn obj_of(t: &TableSchema) -> ObjectRef {
    ObjectRef { kind: if t.kind.is_empty() { "table".into() } else { t.kind.clone() }, schema: t.schema.clone(), name: t.name.clone() }
}

/// A key tuple as one comparable text.
fn tuple_key(values: &[Value]) -> String {
    values.iter().map(mask::canonical).collect::<Vec<_>>().join("\u{1}")
}

/// One statement on a dedicated session, stopping on `cancel_query`.
async fn exec(s: &mut Box<dyn Session>, entry: &SessionEntry, sql: &str, max_rows: usize) -> CommandResult<QueryOutcome> {
    if entry.cancelled.load(Ordering::SeqCst) {
        return Err(CommandError::Cancelled);
    }
    let mut out = QueryOutcome::default();
    let r = tokio::select! {
        r = s.execute(sql, max_rows, &mut out) => r.map_err(CommandError::from),
        _ = entry.cancel.notified() => Err(CommandError::Cancelled),
    };
    r?;
    if let Some(e) = out.error.take() {
        return Err(CommandError::Sql(e));
    }
    if let Some(e) = out.errors.first() {
        return Err(CommandError::Sql(e.message.clone()));
    }
    Ok(out)
}

/// A table's structure from its columns, for engines whose
/// `database_schema` doesn't report it (no foreign keys anyway).
fn schema_from_columns(obj: &ObjectRef, cols: Vec<dbine_driver::ColumnInfo>) -> TableSchema {
    let pk: Vec<String> = cols.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
    TableSchema {
        kind: obj.kind.clone(),
        schema: obj.schema.clone(),
        name: obj.name.clone(),
        primary_key: (!pk.is_empty()).then_some(KeyDef { name: None, columns: pk }),
        columns: cols
            .into_iter()
            .map(|c| ColumnDef { name: c.name, data_type: c.data_type, nullable: c.nullable, default_value: c.default_value, auto_increment: c.auto_increment, ..Default::default() })
            .collect(),
        ..Default::default()
    }
}

fn pk_of(t: &TableSchema) -> Vec<String> {
    t.primary_key.as_ref().map(|k| k.columns.clone()).unwrap_or_default()
}

// -- step 1: the plan's structure -------------------------------------------------------

/// Where a table's rows go.
#[derive(Debug, Clone)]
struct Target {
    obj: ObjectRef,
    /// Its structure: the existing table's, or the one to create.
    def: TableSchema,
    exists: bool,
    create: Option<String>,
    /// `CREATE SCHEMA` before creating it.
    schema_stmt: Option<String>,
    /// Indexes and foreign keys of a created table, after its data.
    after: Vec<String>,
    error: Option<String>,
    /// Source column → target column; the ones left out are in `skipped`.
    columns: Vec<(String, String)>,
    skipped: BTreeMap<String, String>,
}

impl Target {
    fn column(&self, source: &str) -> Option<&str> {
        self.columns.iter().find(|(s, _)| s.eq_ignore_ascii_case(source)).map(|(_, t)| t.as_str())
    }
}

struct Prepared {
    src_driver: &'static Arc<dyn Driver>,
    tgt_driver: &'static Arc<dyn Driver>,
    tables: Vec<TableSchema>,
    edges: Vec<Edge>,
    scope: Scope,
    start: usize,
    order: Vec<usize>,
    broken: Vec<usize>,
    targets: BTreeMap<usize, Target>,
    notes: Vec<String>,
}

impl Prepared {
    fn cycles(&self) -> Vec<String> {
        self.broken
            .iter()
            .map(|&i| {
                let e = &self.edges[i];
                let (c, p) = (table_label(&self.tables[e.child]), table_label(&self.tables[e.parent]));
                if e.nullable {
                    format!("Ciclo de claves foráneas: «{c}» ({}) se copia con NULL y se completa al final, después de «{p}».", e.child_cols.join(", "))
                } else {
                    format!(
                        "Ciclo de claves foráneas entre «{c}» y «{p}» sin columnas que acepten NULL: si el destino valida las claves foráneas, la copia de «{c}» puede fallar."
                    )
                }
            })
            .collect()
    }
}

async fn prepare(src: &mut Box<dyn Session>, tgt: &mut Box<dyn Session>, state: &AppState, args: &SubsetArgs) -> CommandResult<Prepared> {
    let src_driver = driver_of(state, &args.connection_id)?;
    let tgt_driver = driver_of(state, &args.target_connection_id)?;
    let mut notes = Vec::new();

    // The source's tables with their keys.
    let mut tables: Vec<TableSchema> = Vec::new();
    if src_driver.capabilities().foreign_keys {
        match src.database_schema().await {
            Ok(t) => tables = t,
            Err(e) => notes.push(format!("No se pudo leer la estructura de la base ({e}): se copia solo «{}», sin seguir claves foráneas.", args.table.name)),
        }
    }
    let start = match graph::find(&tables, &args.table.schema, &args.table.name) {
        Some(i) => i,
        None => {
            let cols = src.columns(&args.table).await?;
            if cols.is_empty() {
                return Err(CommandError::BadRequest(format!("«{}» no tiene columnas conocidas", args.table.name)));
            }
            tables.push(schema_from_columns(&args.table, cols));
            tables.len() - 1
        }
    };
    if tables[start].columns.is_empty() {
        let cols = src.columns(&args.table).await?;
        tables[start].columns = schema_from_columns(&args.table, cols).columns;
    }
    let edges = graph::edges(&tables);
    let children = args.children.as_ref().map(|c| c.depth.clamp(1, 10));
    let scope = graph::scope(&edges, start, children);
    let (order, broken) = graph::order(&scope, &edges);

    // Where each goes.
    let src_info = src_driver.info();
    let tgt_info = tgt_driver.info();
    let in_order: Vec<TableSchema> = order.iter().map(|&i| tables[i].clone()).collect();
    let opts = dbine_schema::Options { rename_schemas: default_schema_rename(src_info.dialect, tgt_info.dialect), ..Default::default() };
    let conv = dbine_schema::convert(&in_order, src_info.id, tgt_info.id, &opts);
    let existing = tgt.list_objects().await.unwrap_or_default();
    let dflt = default_schema(tgt_info.dialect);
    let mut targets = BTreeMap::new();
    let mut schemas_done: HashSet<String> = HashSet::new();
    for (k, &i) in order.iter().enumerate() {
        let src_t = &tables[i];
        let (def, mapping): (TableSchema, Vec<(String, String)>) = match &conv {
            Ok(c) => (
                c.tables[k].clone(),
                c.columns.iter().filter(|m| m.table == src_t.name && !m.column.is_empty()).map(|m| (m.column.clone(), m.target_column.clone())).collect(),
            ),
            Err(_) => (src_t.clone(), src_t.columns.iter().map(|c| (c.name.clone(), c.name.clone())).collect()),
        };
        let wanted_schema = def.schema.clone();
        let schema_ok = |o: &Option<String>| match (&wanted_schema, o) {
            (Some(w), Some(x)) => w.eq_ignore_ascii_case(x),
            (Some(_), None) => true,
            (None, None) => true,
            (None, Some(x)) => dflt.is_none_or(|d| d.eq_ignore_ascii_case(x)),
        };
        let found = existing
            .iter()
            .filter(|o| o.name.eq_ignore_ascii_case(&def.name) && schema_ok(&o.schema))
            .find(|o| o.kind == def.kind || o.kind == "table" || o.kind == "collection");
        let generated: HashSet<String> =
            src_t.columns.iter().filter(|c| dbine_transfer::clone_table::generated(c, src_info.dialect)).map(|c| c.name.to_lowercase()).collect();
        let mut target = Target {
            obj: ObjectRef { kind: def.kind.clone(), schema: def.schema.clone(), name: def.name.clone() },
            def: def.clone(),
            exists: found.is_some(),
            create: None,
            schema_stmt: None,
            after: Vec::new(),
            error: None,
            columns: Vec::new(),
            skipped: BTreeMap::new(),
        };
        let target_cols: Vec<String> = match found {
            Some(o) => {
                target.obj = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
                match tgt.columns(&target.obj).await {
                    Ok(cols) => {
                        target.def = schema_from_columns(&target.obj, cols);
                        target.def.columns.iter().map(|c| c.name.clone()).collect()
                    }
                    Err(e) => {
                        target.error = Some(format!("no se pudieron leer sus columnas en el destino: {e}"));
                        Vec::new()
                    }
                }
            }
            None => {
                match &conv {
                    Err(e) if src_info.id != tgt_info.id => target.error = Some(format!("no existe en el destino y no se puede crear: {e}")),
                    _ => match tgt_driver.table_ddl(&def, DdlParts { create: true, ..Default::default() }) {
                        Ok(ddl) => target.create = Some(ddl),
                        Err(e) => target.error = Some(format!("no existe en el destino y no se puede crear: {e}")),
                    },
                }
                if let Some(sc) = def.schema.as_deref().filter(|s| !s.is_empty() && tgt_info.has_schemas) {
                    if schemas_done.insert(sc.to_lowercase()) {
                        target.schema_stmt = create_schema(tgt_info.dialect, sc);
                    }
                }
                if !def.indexes.is_empty() {
                    if let Ok(ix) = tgt_driver.table_ddl(&def, DdlParts { indexes: true, ..Default::default() }) {
                        target.after.extend(Some(ix).filter(|s| !s.trim().is_empty()));
                    }
                }
                if !def.foreign_keys.is_empty() {
                    if let Ok(fk) = tgt_driver.table_ddl(&def, DdlParts { foreign_keys: true, ..Default::default() }) {
                        target.after.extend(Some(fk).filter(|s| !s.trim().is_empty()));
                    }
                }
                def.columns.iter().map(|c| c.name.clone()).collect()
            }
        };
        for c in &src_t.columns {
            if generated.contains(&c.name.to_lowercase()) {
                target.skipped.insert(c.name.clone(), "columna calculada: la calcula el destino".into());
                continue;
            }
            let want = mapping.iter().find(|(s, _)| s == &c.name).map(|(_, t)| t.clone()).unwrap_or_else(|| c.name.clone());
            match target_cols.iter().find(|t| t.eq_ignore_ascii_case(&want) || t.eq_ignore_ascii_case(&c.name)) {
                Some(t) => target.columns.push((c.name.clone(), t.clone())),
                None if target.error.is_none() => {
                    target.skipped.insert(c.name.clone(), "no existe en la tabla del destino".into());
                }
                None => {}
            }
        }
        targets.insert(i, target);
    }
    Ok(Prepared { src_driver, tgt_driver, tables, edges, scope, start, order, broken, targets, notes })
}

// -- step 2: the rows ------------------------------------------------------------------

/// A table's rows read so far, without repeats.
#[derive(Debug, Default)]
struct TableRows {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
    ids: HashSet<String>,
    /// Rows added as children (they have a cap).
    from_children: u64,
    capped: bool,
}

impl TableRows {
    fn pos(&self, col: &str) -> Option<usize> {
        self.columns.iter().position(|c| c == col).or_else(|| self.columns.iter().position(|c| c.eq_ignore_ascii_case(col)))
    }

    /// The row's values at `cols`; `None` if one is missing or NULL.
    fn tuple(&self, row: &[Value], cols: &[String]) -> Option<Vec<Value>> {
        cols.iter().map(|c| self.pos(c).and_then(|i| row.get(i)).filter(|v| !v.is_null()).cloned()).collect()
    }

    fn identity(&self, row: &[Value], key: &[String]) -> String {
        match (!key.is_empty()).then(|| self.tuple(row, key)).flatten() {
            Some(t) => tuple_key(&t),
            None => serde_json::to_string(row).unwrap_or_default(),
        }
    }

    /// Adds the rows not read yet (by primary key, or whole); with a cap,
    /// stops there. Returns the new rows' indexes.
    fn add(&mut self, cols: &[String], rows: Vec<Vec<Value>>, key: &[String], cap: Option<u64>) -> Vec<usize> {
        let map: Vec<usize> = cols
            .iter()
            .map(|c| match self.pos(c) {
                Some(i) => i,
                None => {
                    self.columns.push(c.clone());
                    self.columns.len() - 1
                }
            })
            .collect();
        let width = self.columns.len();
        for r in &mut self.rows {
            r.resize(width, Value::Null);
        }
        let mut added = Vec::new();
        for row in rows {
            if let Some(c) = cap {
                if self.from_children >= c {
                    self.capped = true;
                    break;
                }
            }
            let mut full = vec![Value::Null; width];
            for (v, &i) in row.into_iter().zip(&map) {
                full[i] = v;
            }
            if self.ids.insert(self.identity(&full, key)) {
                self.rows.push(full);
                added.push(self.rows.len() - 1);
                if cap.is_some() {
                    self.from_children += 1;
                }
            }
        }
        added
    }
}

/// Reads the subset's rows on the source's session.
struct Collector<'a> {
    s: &'a mut Box<dyn Session>,
    entry: &'a SessionEntry,
    driver: &'a dyn Driver,
    tables: &'a [TableSchema],
    data: BTreeMap<usize, TableRows>,
    /// Key tuples already asked for, per (table, columns).
    asked: HashMap<(usize, String), HashSet<String>>,
    total: usize,
    notes: Vec<String>,
    progress: &'a (dyn Fn(&str, u64) + Send + Sync),
    last: Instant,
}

impl Collector<'_> {
    fn obj(&self, t: usize) -> ObjectRef {
        obj_of(&self.tables[t])
    }

    async fn query(&mut self, obj: &ObjectRef, filters: &[ColumnFilter], limit: u32) -> CommandResult<(Vec<String>, Vec<Vec<Value>>)> {
        let browse = self.s.browse_query(obj, limit);
        let sql = if filters.is_empty() { browse } else { self.driver.filtered_browse(&browse, filters)? };
        let out = exec(self.s, self.entry, &sql, limit as usize).await.map_err(|e| match e {
            CommandError::Sql(m) => CommandError::Sql(format!("«{}»: {m}", obj.name)),
            other => other,
        })?;
        Ok(match out.results.into_iter().find(|r| !r.columns.is_empty()) {
            Some(r) => (r.columns.into_iter().map(|c| c.name).collect(), r.rows),
            None => (Vec::new(), Vec::new()),
        })
    }

    /// Rows of `t` whose `cols` take one of `tuples` (those not asked for
    /// before), in chunks. A composite key filters on its most varied
    /// column and keeps the exact matches here.
    async fn fetch(&mut self, t: usize, cols: &[String], tuples: Vec<Vec<Value>>) -> CommandResult<Vec<(Vec<String>, Vec<Vec<Value>>)>> {
        let asked = self.asked.entry((t, cols.join("\u{1}").to_lowercase())).or_default();
        let mut wanted: Vec<Vec<Value>> = Vec::new();
        let mut keys: HashSet<String> = HashSet::new();
        for tu in tuples {
            let k = tuple_key(&tu);
            if asked.insert(k.clone()) {
                keys.insert(k);
                wanted.push(tu);
            }
        }
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let pivot = (0..cols.len())
            .max_by_key(|&i| wanted.iter().map(|tu| mask::canonical(&tu[i])).collect::<HashSet<_>>().len())
            .unwrap_or(0);
        let mut seen = HashSet::new();
        let values: Vec<Value> = wanted.iter().map(|tu| tu[pivot].clone()).filter(|v| seen.insert(mask::canonical(v))).collect();
        let obj = self.obj(t);
        let mut out = Vec::new();
        for chunk in values.chunks(CHUNK) {
            let f = ColumnFilter { column: cols[pivot].clone(), op: FilterOp::In, values: chunk.to_vec(), sql: None };
            let (rcols, mut rows) = self.query(&obj, &[f], FETCH_LIMIT).await?;
            if cols.len() > 1 {
                let at: Option<Vec<usize>> = cols.iter().map(|c| rcols.iter().position(|x| x.eq_ignore_ascii_case(c))).collect();
                let Some(at) = at else { continue };
                rows.retain(|r| keys.contains(&tuple_key(&at.iter().map(|&i| r[i].clone()).collect::<Vec<_>>())));
            }
            out.push((rcols, rows));
        }
        Ok(out)
    }

    fn add(&mut self, t: usize, parts: Vec<(Vec<String>, Vec<Vec<Value>>)>, cap: Option<u64>) -> CommandResult<Vec<usize>> {
        let key = pk_of(&self.tables[t]);
        let rows = self.data.entry(t).or_default();
        let mut added = Vec::new();
        for (cols, r) in parts {
            added.extend(rows.add(&cols, r, &key, cap));
        }
        self.total += added.len();
        if self.total > MAX_ROWS {
            return Err(CommandError::BadRequest(format!(
                "el subconjunto pasa de {MAX_ROWS} filas: achicá el filtro del inicio, la profundidad o el tope por tabla"
            )));
        }
        if self.last.elapsed() >= PROGRESS_EVERY {
            self.last = Instant::now();
            (self.progress)(&self.tables[t].name, self.total as u64);
        }
        Ok(added)
    }

    /// Values at `cols` of some rows of `t` (the ones with no NULL).
    fn tuples(&self, t: usize, idx: impl Iterator<Item = usize>, cols: &[String]) -> Vec<Vec<Value>> {
        let Some(d) = self.data.get(&t) else { return Vec::new() };
        idx.filter_map(|i| d.rows.get(i).and_then(|r| d.tuple(r, cols))).collect()
    }

    async fn start(&mut self, start: usize, obj: &ObjectRef, filter: &StartFilter) -> CommandResult<Vec<usize>> {
        let mut filters = filter.columns.clone();
        if let Some(e) = filter.expression.as_deref().map(str::trim).filter(|e| !e.is_empty()) {
            let column = self.tables[start].columns.first().map(|c| c.name.clone()).unwrap_or_default();
            filters.push(ColumnFilter { column, op: FilterOp::Sql, values: vec![], sql: Some(e.to_string()) });
        }
        let limit = match filter.limit {
            Limit::Rows { count } => count.clamp(1, SCAN_LIMIT as u64) as u32,
            _ => SCAN_LIMIT,
        };
        let (cols, mut rows) = self.query(obj, &filters, limit).await?;
        if rows.len() as u32 >= SCAN_LIMIT && !matches!(filter.limit, Limit::Rows { .. }) {
            self.notes.push(format!("«{}» tiene más de {SCAN_LIMIT} filas: se tomaron las primeras {SCAN_LIMIT}.", obj.name));
        }
        if let Limit::Percent { percent } = filter.limit {
            let n = rows.len();
            let keep = ((n as f64) * percent.clamp(0.0, 100.0) / 100.0).ceil() as usize;
            let picked: HashSet<usize> = (0..keep.min(n)).map(|i| i * n / keep.max(1)).collect();
            rows = rows.into_iter().enumerate().filter(|(i, _)| picked.contains(i)).map(|(_, r)| r).collect();
        }
        self.add(start, vec![(cols, rows)], None)
    }

    async fn collect(&mut self, p: &Prepared, start_obj: &ObjectRef, filter: &StartFilter, children: Option<&ChildrenOptions>) -> CommandResult<()> {
        let first = self.start(p.start, start_obj, filter).await?;
        for &t in p.scope.members.keys() {
            self.data.entry(t).or_default();
        }

        // Down: the rows that hang from the chosen ones, level by level.
        if let Some(ch) = children {
            let mut frontier: BTreeMap<usize, Vec<usize>> = BTreeMap::from([(p.start, first)]);
            for _ in 0..ch.depth.clamp(1, 10) {
                let mut next: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
                for (&t, idx) in &frontier {
                    for e in p.edges.iter().filter(|e| e.parent == t && e.child != t && p.scope.members.contains_key(&e.child)) {
                        let tuples = self.tuples(t, idx.iter().copied(), &e.parent_cols);
                        let parts = self.fetch(e.child, &e.child_cols, tuples).await?;
                        let added = self.add(e.child, parts, Some(ch.max_rows.max(1)))?;
                        next.entry(e.child).or_default().extend(added);
                    }
                }
                next.retain(|_, v| !v.is_empty());
                if next.is_empty() {
                    break;
                }
                frontier = next;
            }
        }

        // Up: every parent the rows read need, until nothing new comes.
        let mut done: BTreeMap<usize, usize> = BTreeMap::new();
        loop {
            let mut moved = false;
            let pending: Vec<(usize, usize, usize)> =
                self.data.iter().map(|(&t, d)| (t, done.get(&t).copied().unwrap_or(0), d.rows.len())).filter(|(_, from, to)| from < to).collect();
            for (t, from, to) in pending {
                done.insert(t, to);
                moved = true;
                for e in p.edges.iter().filter(|e| e.child == t && p.scope.members.contains_key(&e.parent)) {
                    let tuples = self.tuples(t, from..to, &e.child_cols);
                    let parts = self.fetch(e.parent, &e.parent_cols, tuples).await?;
                    self.add(e.parent, parts, None)?;
                }
            }
            if !moved {
                break;
            }
        }

        // Rows that point to parents that don't exist.
        for e in p.edges.iter().filter(|e| self.data.contains_key(&e.child) && self.data.contains_key(&e.parent)) {
            let (c, pa) = (&self.data[&e.child], &self.data[&e.parent]);
            let have: HashSet<String> = pa.rows.iter().filter_map(|r| pa.tuple(r, &e.parent_cols)).map(|t| tuple_key(&t)).collect();
            let missing = c.rows.iter().filter_map(|r| c.tuple(r, &e.child_cols)).filter(|t| !have.contains(&tuple_key(t))).count();
            if missing > 0 {
                self.notes.push(format!(
                    "{missing} filas de «{}» apuntan a filas de «{}» que no existen en el origen: si el destino valida las claves foráneas, van a fallar.",
                    table_label(&self.tables[e.child]),
                    table_label(&self.tables[e.parent])
                ));
            }
        }
        for (&t, d) in &self.data {
            if d.capped {
                self.notes.push(format!("«{}» llegó al tope de filas por tabla: no se copian todas las que cuelgan.", table_label(&self.tables[t])));
            }
        }
        Ok(())
    }
}

// -- step 3: writing -------------------------------------------------------------------

fn rule_for<'a>(masks: &'a [TableMask], t: &TableSchema, col: &str) -> Option<&'a MaskRule> {
    masks
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case(&t.name) && graph::same(&m.schema, &t.schema))
        .and_then(|m| m.columns.iter().find(|(c, _)| c.eq_ignore_ascii_case(col)).map(|(_, r)| r))
        .filter(|r| !r.is_keep())
}

fn key_columns(p: &Prepared, t: usize) -> HashSet<String> {
    let mut keys: HashSet<String> = pk_of(&p.tables[t]).iter().map(|c| c.to_lowercase()).collect();
    for fk in &p.tables[t].foreign_keys {
        keys.extend(fk.columns.iter().map(|c| c.to_lowercase()));
    }
    keys
}

/// Masks that can't apply, and warnings for masked keys.
fn check_masks(p: &Prepared, masks: &[TableMask]) -> CommandResult<Vec<String>> {
    let mut notes = Vec::new();
    for &t in &p.order {
        let table = &p.tables[t];
        let keys = key_columns(p, t);
        for c in &table.columns {
            let Some(rule) = rule_for(masks, table, &c.name) else { continue };
            let target_nullable = p.targets.get(&t).and_then(|tg| tg.column(&c.name).and_then(|n| tg.def.columns.iter().find(|x| x.name == n))).is_none_or(|x| x.nullable);
            if *rule == MaskRule::Null && (!c.nullable || !target_nullable) {
                return Err(CommandError::BadRequest(format!("«{}.{}» no acepta nulos: elegí otra regla", table.name, c.name)));
            }
            if keys.contains(&c.name.to_lowercase()) {
                notes.push(format!(
                    "Se enmascara la clave «{}.{}»: las columnas que la referencian tienen que usar la misma regla para que las filas sigan relacionadas.",
                    table.name, c.name
                ));
            }
        }
    }
    Ok(notes)
}

struct Writer<'a> {
    s: &'a mut Box<dyn Session>,
    entry: &'a SessionEntry,
    p: &'a Prepared,
    emit: &'a (dyn Fn(&str, &str, u64, u64) + Send + Sync),
}

/// A cut cycle's columns, set after every table is written.
struct Deferred {
    t: usize,
    /// (key, set, whole row) in target column names, masked.
    changes: Vec<RowChange>,
}

impl Writer<'_> {
    async fn run(&mut self, sql: &str) -> CommandResult<()> {
        exec(self.s, self.entry, sql, 1).await.map(|_| ())
    }

    /// One table's rows, masked, in batches. Returns the rows written and
    /// the columns set later (cut cycles).
    async fn table(&mut self, t: usize, d: &TableRows, masks: &[TableMask], masker: &Masker, report: &mut TableReport) -> CommandResult<Option<Deferred>> {
        let p = self.p;
        let table = &p.tables[t];
        let tg = &p.targets[&t];
        // Columns read that go to the target, with their rule and shape.
        struct Col<'r> {
            at: usize,
            name: String,
            rule: Option<&'r MaskRule>,
            shape: Shape,
            nullable: bool,
        }
        let mut cols: Vec<Col> = Vec::new();
        for (at, name) in d.columns.iter().enumerate() {
            let Some(target) = tg.column(name) else { continue };
            let src_col = table.columns.iter().find(|c| c.name.eq_ignore_ascii_case(name));
            let tgt_col = tg.def.columns.iter().find(|c| c.name == target);
            let ty = tgt_col.map(|c| c.data_type.as_str()).filter(|t| !t.is_empty()).or(src_col.map(|c| c.data_type.as_str())).unwrap_or("");
            let rule = rule_for(masks, table, name);
            if rule.is_some() {
                report.masked.push(name.clone());
            }
            cols.push(Col { at, name: target.to_string(), rule, shape: Shape::of(ty), nullable: tgt_col.is_none_or(|c| c.nullable) });
        }
        if cols.is_empty() {
            return Err(CommandError::BadRequest("ninguna columna coincide con la tabla del destino".into()));
        }
        let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
        let row_of = |r: &[Value]| -> Vec<Value> {
            cols.iter()
                .map(|c| {
                    let v = r.get(c.at).cloned().unwrap_or(Value::Null);
                    match c.rule {
                        Some(rule) => masker.apply(rule, &v, c.shape),
                        None => v,
                    }
                })
                .collect()
        };

        // Cut cycles: their columns go NULL now and are set at the end,
        // by the table's key.
        let key: Vec<usize> = pk_of(table).iter().filter_map(|k| names.iter().position(|n| tg.column(k).is_some_and(|t| t == n))).collect();
        let mut nulled: Vec<usize> = Vec::new();
        for &b in &p.broken {
            let e = &p.edges[b];
            if e.child != t || !e.nullable {
                continue;
            }
            let at: Vec<usize> = e.child_cols.iter().filter_map(|c| tg.column(c).and_then(|n| names.iter().position(|x| x == n))).collect();
            if key.is_empty() || key.len() != pk_of(table).len() || at.iter().any(|&i| !cols[i].nullable) {
                report.notes.push(format!("La columna {} cierra un ciclo y la tabla no tiene clave primaria: se copia tal cual.", e.child_cols.join(", ")));
                continue;
            }
            nulled.extend(at);
        }

        // A table that references itself: each row after its parent row.
        let mut order: Vec<usize> = (0..d.rows.len()).collect();
        if let Some(e) = p.edges.iter().find(|e| e.child == t && e.parent == t) {
            let index: HashMap<String, usize> = d.rows.iter().enumerate().filter_map(|(i, r)| d.tuple(r, &e.parent_cols).map(|k| (tuple_key(&k), i))).collect();
            let parent_of: Vec<Option<usize>> = d.rows.iter().map(|r| d.tuple(r, &e.child_cols).and_then(|k| index.get(&tuple_key(&k)).copied())).collect();
            order = graph::rows_parents_first(&parent_of);
        }

        let (before, after) = p.tgt_driver.data_load_wrap(&tg.def);
        if !before.trim().is_empty() {
            self.run(&before).await?;
        }
        let total = d.rows.len() as u64;
        let mut deferred = Deferred { t, changes: Vec::new() };
        let mut last = Instant::now();
        (self.emit)("insert", &table.name, 0, total);
        for chunk in order.chunks(BATCH) {
            let mut batch = Vec::with_capacity(chunk.len());
            for &i in chunk {
                let mut row = row_of(&d.rows[i]);
                if !nulled.is_empty() {
                    let set: Vec<(String, Value)> = nulled.iter().map(|&c| (names[c].clone(), row[c].clone())).filter(|(_, v)| !v.is_null()).collect();
                    if !set.is_empty() {
                        deferred.changes.push(RowChange {
                            key: key.iter().map(|&k| (names[k].clone(), row[k].clone())).collect(),
                            set,
                            row: names.iter().cloned().zip(row.iter().cloned()).collect(),
                        });
                    }
                    for &c in &nulled {
                        row[c] = Value::Null;
                    }
                }
                batch.push(row);
            }
            let script = p.tgt_driver.insert_script(&tg.obj, &names, &batch)?;
            self.run(&script).await.map_err(|e| match e {
                CommandError::Sql(m) => CommandError::Sql(format!("filas {}–{}: {m}", report.written + 1, report.written + batch.len() as u64)),
                other => other,
            })?;
            report.written += batch.len() as u64;
            if last.elapsed() >= PROGRESS_EVERY {
                last = Instant::now();
                (self.emit)("insert", &table.name, report.written, total);
            }
        }
        if !after.trim().is_empty() {
            self.run(&after).await?;
        }
        (self.emit)("insert", &table.name, report.written, total);
        Ok((!deferred.changes.is_empty()).then_some(deferred))
    }
}

/// Creates what's missing, writes every table in order and completes the
/// cut cycles. Stops at the first table that fails; the report says how
/// far it got.
async fn write(
    s: &mut Box<dyn Session>,
    entry: &SessionEntry,
    p: &Prepared,
    data: &BTreeMap<usize, TableRows>,
    masks: &[TableMask],
    masker: &Masker,
    emit: &(dyn Fn(&str, &str, u64, u64) + Send + Sync),
) -> (Vec<TableReport>, bool) {
    let mut reports: Vec<TableReport> = p
        .order
        .iter()
        .map(|&t| {
            let tg = &p.targets[&t];
            TableReport {
                table: table_label(&p.tables[t]),
                target: label(tg.obj.schema.as_deref(), &tg.obj.name),
                created: false,
                rows: data.get(&t).map_or(0, |d| d.rows.len() as u64),
                written: 0,
                masked: Vec::new(),
                status: "skipped",
                error: None,
                notes: Vec::new(),
            }
        })
        .collect();
    let mut w = Writer { s, entry, p, emit };
    let mut deferred: Vec<Deferred> = Vec::new();
    let mut failed = false;
    let mut cancelled = false;
    let fail = |r: &mut TableReport, e: CommandError, cancelled: &mut bool| {
        if matches!(e, CommandError::Cancelled) {
            r.status = "cancelled";
            *cancelled = true;
        } else {
            r.status = "error";
            r.error = Some(e.to_string());
        }
    };

    // Every table first: a cut cycle's reference may point to a table
    // later in the order (SQLite checks it on each insert).
    for (k, &t) in p.order.iter().enumerate() {
        let tg = &p.targets[&t];
        let r = &mut reports[k];
        if let Some(e) = &tg.error {
            r.status = "error";
            r.error = Some(e.clone());
            failed = true;
            break;
        }
        if let Some(ddl) = &tg.create {
            emit("create", &p.tables[t].name, 0, 0);
            if let Some(sc) = &tg.schema_stmt {
                if let Err(e) = w.run(sc).await {
                    r.notes.push(format!("No se pudo crear el esquema: {e}"));
                }
            }
            if let Err(e) = w.run(ddl).await {
                fail(r, e, &mut cancelled);
                failed = true;
                break;
            }
            r.created = true;
        }
    }
    for (k, &t) in p.order.iter().enumerate() {
        if failed {
            break;
        }
        let r = &mut reports[k];
        let empty = TableRows::default();
        let d = data.get(&t).unwrap_or(&empty);
        if d.rows.is_empty() {
            r.status = "done";
            continue;
        }
        match w.table(t, d, masks, masker, r).await {
            Ok(def) => {
                r.status = "done";
                deferred.extend(def);
            }
            Err(e) => {
                fail(r, e, &mut cancelled);
                failed = true;
            }
        }
    }
    if failed {
        return (reports, cancelled);
    }

    // The columns of cut cycles, now that their parents are there.
    for d in deferred {
        let k = p.order.iter().position(|&t| t == d.t).unwrap_or(0);
        let tg = &p.targets[&d.t];
        emit("cycles", &p.tables[d.t].name, 0, d.changes.len() as u64);
        for chunk in d.changes.chunks(200) {
            let res = match p.tgt_driver.update_script(&tg.obj, chunk) {
                Ok(sql) => w.run(&sql).await,
                Err(e) => Err(e.into()),
            };
            if let Err(e) = res {
                let r = &mut reports[k];
                if matches!(e, CommandError::Cancelled) {
                    r.status = "cancelled";
                    return (reports, true);
                }
                r.notes.push(format!("No se pudieron completar las columnas del ciclo: {e}"));
                break;
            }
        }
    }

    // Indexes and foreign keys of the tables created.
    for (k, &t) in p.order.iter().enumerate() {
        if !reports[k].created {
            continue;
        }
        for sql in &p.targets[&t].after {
            emit("constraints", &p.tables[t].name, 0, 0);
            if let Err(e) = w.run(sql).await {
                if matches!(e, CommandError::Cancelled) {
                    return (reports, true);
                }
                reports[k].notes.push(format!("No se pudo crear un índice o clave foránea: {e}"));
            }
        }
    }
    (reports, cancelled)
}

// -- commands --------------------------------------------------------------------------

fn valid_id(id: &str) -> CommandResult<()> {
    if id.is_empty() || id.len() > 100 {
        return Err(CommandError::BadRequest("identificador de copia inválido".into()));
    }
    Ok(())
}

/// The target is the source itself (same saved connection and database, or
/// the same server and database through another connection).
fn same_database(state: &AppState, args: &SubsetArgs) -> CommandResult<bool> {
    let get = |id: &str| state.store.get_connection(id).map_err(CommandError::from)?.ok_or_else(|| CommandError::NotFound("conexión inexistente".into()));
    let (a, b) = (get(&args.connection_id)?, get(&args.target_connection_id)?);
    let db = |given: &str, cfg: &dbine_driver::ConnectionConfig| if given.is_empty() { cfg.database.clone() } else { given.to_string() };
    let (da, dbb) = (db(&args.database, &a.config), db(&args.target_database, &b.config));
    if a.id == b.id {
        return Ok(da.eq_ignore_ascii_case(&dbb));
    }
    Ok(a.config.driver == b.config.driver && a.config.host.eq_ignore_ascii_case(&b.config.host) && a.config.port == b.config.port && da.eq_ignore_ascii_case(&dbb))
}

/// What the user types to copy to a production target (`None` when it isn't one).
fn confirm_label(state: &AppState, args: &SubsetArgs) -> CommandResult<Option<String>> {
    let conn = state.store.get_connection(&args.target_connection_id)?.ok_or_else(|| CommandError::NotFound("conexión inexistente".into()))?;
    let production = conn.tags.iter().any(|t| crate::tasks::PROD_TAGS.contains(&t.trim().to_lowercase().as_str()));
    Ok(production.then(|| {
        [args.target_database.as_str(), conn.config.database.as_str(), conn.name.as_str()].into_iter().find(|s| !s.is_empty()).unwrap_or_default().to_string()
    }))
}

fn tagged(run_id: &str, mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        o.insert("runId".into(), json!(run_id));
    }
    v
}

/// Opens both sessions, prepares and reads the rows; the callers write (or not).
async fn plan_and_collect(
    state: &AppState,
    args: &SubsetArgs,
    write: bool,
    emit: &Emit,
) -> CommandResult<(Prepared, BTreeMap<usize, TableRows>, Vec<String>, Arc<SessionEntry>)> {
    valid_id(&args.run_id)?;
    if same_database(state, args)? {
        return Err(CommandError::BadRequest("el destino es la misma base que el origen: elegí otra base (el origen nunca se modifica)".into()));
    }
    let src_key = format!("subset:{}:src", args.run_id);
    let tgt_key = format!("subset:{}:tgt", args.run_id);
    let src_entry = state.dedicated_session(&src_key, &args.connection_id, &args.database, true).await?;
    let tgt_entry = match state.dedicated_session(&tgt_key, &args.target_connection_id, &args.target_database, !write).await {
        Ok(e) => e,
        Err(e) => {
            state.sessions.remove(&src_key);
            return Err(e);
        }
    };
    let res = async {
        emit(json!({ "phase": "read", "rows": 0 }));
        let p = {
            let mut src = src_entry.session.lock().await;
            let mut tgt = tgt_entry.session.lock().await;
            prepare(&mut src, &mut tgt, state, args).await?
        };
        let mut src = src_entry.session.lock().await;
        let progress = |table: &str, rows: u64| emit(json!({ "phase": "collect", "table": table, "rows": rows }));
        let mut c = Collector {
            s: &mut src,
            entry: &src_entry,
            driver: &**p.src_driver,
            tables: &p.tables,
            data: BTreeMap::new(),
            asked: HashMap::new(),
            total: 0,
            notes: Vec::new(),
            progress: &progress,
            last: Instant::now(),
        };
        c.collect(&p, &args.table, &args.filter, args.children.as_ref()).await?;
        let (data, notes) = (std::mem::take(&mut c.data), std::mem::take(&mut c.notes));
        Ok::<_, CommandError>((p, data, notes))
    }
    .await;
    state.sessions.remove(&src_key);
    match res {
        Ok((p, data, notes)) => Ok((p, data, notes, tgt_entry)),
        Err(e) => {
            state.sessions.remove(&tgt_key);
            Err(e)
        }
    }
}

pub async fn plan(state: &AppState, args: &SubsetArgs, emit: Emit) -> CommandResult<SubsetPlan> {
    let (p, data, collect_notes, _) = plan_and_collect(state, args, false, &emit).await?;
    state.sessions.remove(&format!("subset:{}:tgt", args.run_id));
    let mut notes = p.notes.clone();
    notes.extend(collect_notes);
    let tables = p
        .order
        .iter()
        .map(|&t| {
            let table = &p.tables[t];
            let tg = &p.targets[&t];
            let (role, depth) = p.scope.members.get(&t).copied().unwrap_or((Role::Parent, 0));
            let keys = key_columns(&p, t);
            let d = data.get(&t);
            PlanTable {
                schema: table.schema.clone(),
                name: table.name.clone(),
                role: role.id(),
                depth,
                rows: d.map_or(0, |d| d.rows.len() as u64),
                capped: d.is_some_and(|d| d.capped),
                target: label(tg.obj.schema.as_deref(), &tg.obj.name),
                exists: tg.exists,
                create_ddl: tg.create.clone(),
                error: tg.error.clone(),
                columns: table
                    .columns
                    .iter()
                    .map(|c| {
                        let key = keys.contains(&c.name.to_lowercase());
                        PlanColumn {
                            name: c.name.clone(),
                            data_type: c.data_type.clone(),
                            nullable: c.nullable,
                            key,
                            suggested: if key { MaskRule::Keep } else { mask::suggest(&c.name, &c.data_type).unwrap_or_default() },
                            skipped: tg.skipped.get(&c.name).cloned(),
                        }
                    })
                    .collect(),
            }
        })
        .collect::<Vec<_>>();
    Ok(SubsetPlan {
        total_rows: tables.iter().map(|t| t.rows).sum(),
        tables,
        cycles: p.cycles(),
        notes,
        confirm_label: confirm_label(state, args)?,
        target_engine: p.tgt_driver.info().name.to_string(),
    })
}

pub async fn run(state: &AppState, args: &RunArgs, emit: Emit) -> CommandResult<SubsetReport> {
    let started = Instant::now();
    let a = &args.subset;
    let conn = state.store.get_connection(&a.target_connection_id)?.ok_or_else(|| CommandError::NotFound("conexión inexistente".into()))?;
    if conn.config.read_only {
        return Err(CommandError::BadRequest(format!("«{}» es de solo lectura: no se pueden copiar datos ahí", conn.name)));
    }
    if let Some(want) = confirm_label(state, a)? {
        if args.confirm.trim() != want {
            return Err(CommandError::BadRequest(format!("«{}» es de producción: escribí «{want}» para confirmar la copia", conn.name)));
        }
    }
    let (p, data, mut notes, tgt_entry) = plan_and_collect(state, a, true, &emit).await?;
    let tgt_key = format!("subset:{}:tgt", a.run_id);
    let res = async {
        notes.extend(p.cycles());
        notes.extend(check_masks(&p, &args.masks)?);
        let masker = Masker::new(args.seed.unwrap_or_else(|| uuid::Uuid::new_v4().as_u64_pair().0));
        let progress = |phase: &str, table: &str, rows: u64, total: u64| emit(json!({ "phase": phase, "table": table, "rows": rows, "total": total }));
        let mut tgt = tgt_entry.session.lock().await;
        Ok::<_, CommandError>(write(&mut tgt, &tgt_entry, &p, &data, &args.masks, &masker, &progress).await)
    }
    .await;
    state.sessions.remove(&tgt_key);
    let (tables, cancelled) = res?;
    emit(json!({ "phase": "done", "rows": tables.iter().map(|t| t.written).sum::<u64>() }));
    Ok(SubsetReport { tables, notes, elapsed_ms: started.elapsed().as_millis() as u64, cancelled })
}

fn app_emit(app: AppHandle, run_id: String) -> Emit {
    Arc::new(move |v| {
        let _ = app.emit("subset-progress", tagged(&run_id, v));
    })
}

/// The subset's plan: tables, rows, order, what's created in the target and
/// the masking suggested. Reads only (both sides). Progress:
/// `subset-progress` events.
#[tauri::command(rename_all = "camelCase")]
pub async fn subset_plan(app: AppHandle, state: State<'_, AppState>, args: SubsetArgs) -> CommandResult<SubsetPlan> {
    let emit = app_emit(app, args.run_id.clone());
    plan(&state, &args, emit).await
}

/// Copy the subset (the user confirmed the plan). Stops with
/// `cancel_query` on `subset:<id>:src` / `subset:<id>:tgt`.
#[tauri::command(rename_all = "camelCase")]
pub async fn subset_run(app: AppHandle, state: State<'_, AppState>, args: RunArgs) -> CommandResult<SubsetReport> {
    let emit = app_emit(app, args.subset.run_id.clone());
    run(&state, &args, emit).await
}

#[cfg(test)]
mod tests;
