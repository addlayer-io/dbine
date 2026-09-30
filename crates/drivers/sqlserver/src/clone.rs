//! Same-engine clone ("clonar"): everything that makes a SQL Server target
//! identical to the source beyond columns and key, as a [`CloneScript`].
//!
//! The source is read from its catalog (READ UNCOMMITTED, bounded lock
//! waits, independent queries in parallel on extra connections so the
//! session itself keeps its state); the target is only asked what it
//! supports (edition, version, filegroups, In-Memory OLTP). Nothing is
//! written anywhere here: the script runs later, in this order:
//!
//! 1. `before`: schemas, filegroups, partition functions and schemes, XML
//!    schema collections, user types, sequences, the functions a column
//!    uses (computed columns, defaults).
//! 2. per table: `create` (columns, primary key and storage; a
//!    memory-optimized table with all its indexes; a system-versioned one
//!    as a plain table with its period columns), its data, then
//!    `after_data` (every other index in creation order, statistics,
//!    disabled indexes, identity reseed).
//! 3. `after`: sequences' current values, synonyms, views (and their
//!    indexes), functions and procedures in dependency order, triggers,
//!    CHECK constraints (after the code: a CHECK may call a function),
//!    foreign keys, system versioning back on, extended properties.
//!
//! Every statement is idempotent (`IF NOT EXISTS`, `IF OBJECT_ID(...) IS
//! NULL`), so a resumed clone runs the script again from the start, and
//! each one is a single batch (no `GO`).
//!
//! Fabric and Babelfish don't clone: a Fabric warehouse has no
//! filegroups, partition schemes, rowstore indexes to speak of, triggers,
//! temporal or memory-optimized tables; Babelfish runs T-SQL on top of
//! PostgreSQL and its `sys` catalog is a partial emulation (no partition
//! functions, filegroups, columnstore, In-Memory OLTP, temporal tables nor
//! DBCC CHECKIDENT). For both, the generic migration (columns and key) is
//! the faithful one.

use crate::delta::UNTRUSTED_MARK;
use crate::variant::Variant;
use crate::{connect_once, SqlServerSession};
use dbine_driver::transfer::{CloneScript, CloneTable};
use dbine_driver::{Error, ObjectRef, Result, Session};
use std::collections::{BTreeMap, HashMap, HashSet};
use tiberius::{Client, Row};
use tokio::net::TcpStream;
use tokio_util::compat::Compat;

type Conn = Client<Compat<TcpStream>>;

/// Catalog reads never wait behind production traffic for long.
const PREAMBLE: &str = "SET TRANSACTION ISOLATION LEVEL READ UNCOMMITTED; SET LOCK_TIMEOUT 10000;";
/// Extra connections for the parallel catalog reads.
const EXTRA_CONNECTIONS: usize = 3;
/// Options an index on a view, a computed column or with a filter needs.
const INDEX_SET: &str =
    "SET ANSI_NULLS, ANSI_PADDING, ANSI_WARNINGS, ARITHABORT, CONCAT_NULL_YIELDS_NULL, QUOTED_IDENTIFIER ON; SET NUMERIC_ROUNDABORT OFF;";

/// Entry point of [`dbine_driver::Driver::clone_script`].
pub(crate) async fn clone_script(variant: Variant, source: &mut dyn Session, target: &mut dyn Session, tables: &[ObjectRef]) -> Result<CloneScript> {
    fn own(s: &mut dyn Session) -> Option<&mut SqlServerSession> {
        s.as_any()?.downcast_mut::<SqlServerSession>()
    }
    let clones = |v: Variant| matches!(v, Variant::SqlServer | Variant::AzureSql);
    if !clones(variant) {
        return Err(Error::Unsupported(
            "este motor no clona bases: Fabric y Babelfish no tienen el catálogo completo de SQL Server".into(),
        ));
    }
    let (Some(src), Some(dst)) = (own(source), own(target)) else {
        return Err(Error::Unsupported("clonar necesita dos sesiones de SQL Server".into()));
    };
    if !clones(src.variant) || !clones(dst.variant) {
        return Err(Error::Unsupported("solo se clona entre SQL Server y Azure SQL".into()));
    }
    let mut target = read_target(dst).await?;
    let mut source = read_source(src).await?;
    // Which users the source's modules run as (WITH EXECUTE AS 'user') the
    // target has: a module can only be created bound to one that exists.
    // The name asked is the one its text declares (the CREATE runs that
    // text), looked up by the target's collation.
    let mut asked: HashSet<String> = HashSet::new();
    for m in &mut source.modules {
        let Some(user) = executes_as_named(m) else { continue };
        // Spelled apart from the catalog's name: the same user (the source's
        // collation) or one renamed since the module was created.
        if let Some(catalog) = m.execute_as.clone().filter(|c| *c != user) {
            m.execute_as_renamed = src
                .rows("SELECT 1 WHERE DATABASE_PRINCIPAL_ID(@P1) = DATABASE_PRINCIPAL_ID(@P2)", &[user.as_str(), catalog.as_str()])
                .await?
                .is_empty();
        }
        if user == "dbo" || !asked.insert(user.clone()) {
            continue;
        }
        let principal = |cmp: &str| format!("SELECT TOP 1 name FROM sys.database_principals WHERE {cmp} AND type NOT IN ('R', 'A')");
        if !dst.rows(&principal("name = @P1"), &[user.as_str()]).await?.is_empty() {
            target.users.insert(user);
        } else if let Some(there) = dst.rows(&principal("UPPER(name) = UPPER(@P1)"), &[user.as_str()]).await?.first().and_then(|r| s(r, 0)) {
            target.users_spelled.insert(user, there);
        }
    }
    // DBCC CHECKIDENT doesn't take memory-optimized tables: their identity
    // continues from the largest value copied. Whether that's where the
    // source is (their rows are in memory: cheap to ask).
    for t in source.tables.iter_mut().filter(|t| t.memory.is_some() && t.identity_last.is_some()) {
        if let Some(c) = t.columns.iter().find(|c| c.identity.is_some()) {
            let sql = format!("SELECT CAST(MAX({}) AS nvarchar(40)) FROM {}", q(&c.name), t.qname());
            t.identity_max = src.rows(&sql, &[]).await?.first().and_then(|r| s(r, 0));
        }
    }
    build(source, &target, tables)
}

// ---------------------------------------------------------------------------
// Model (what the catalog says), public to the crate for tests.
// ---------------------------------------------------------------------------

/// Which catalog columns (so which features) a server has.
#[derive(Debug, Clone, Default)]
pub(crate) struct Caps {
    pub edition: i32,
    pub temporal: bool,
    pub retention: bool,
    pub memory_optimized: bool,
    pub sequential_key: bool,
    pub ordered_columnstore: bool,
    pub last_used_value: bool,
    pub hidden: bool,
    pub masked: bool,
    pub compression_delay: bool,
    pub xtp: bool,
    /// Graph tables (`sys.tables.is_node`, SQL Server 2017).
    pub graph: bool,
    /// Ledger tables (`sys.tables.ledger_type`, SQL Server 2022).
    pub ledger: bool,
    /// ORDER on a nonclustered columnstore index (SQL Server 2025, Azure
    /// SQL Database).
    pub ordered_nonclustered_columnstore: bool,
}

impl Caps {
    /// Azure SQL Database (only PRIMARY; the service places files).
    fn azure_db(&self) -> bool {
        self.edition == 5
    }
}

const CAPS_SQL: &str = "SELECT CAST(SERVERPROPERTY('EngineEdition') AS int),
        CAST(CASE WHEN COL_LENGTH('sys.tables', 'temporal_type') IS NULL THEN 0 ELSE 1 END AS bit),
        CAST(CASE WHEN COL_LENGTH('sys.tables', 'history_retention_period') IS NULL THEN 0 ELSE 1 END AS bit),
        CAST(CASE WHEN COL_LENGTH('sys.tables', 'is_memory_optimized') IS NULL THEN 0 ELSE 1 END AS bit),
        CAST(CASE WHEN COL_LENGTH('sys.indexes', 'optimize_for_sequential_key') IS NULL THEN 0 ELSE 1 END AS bit),
        CAST(CASE WHEN COL_LENGTH('sys.index_columns', 'column_store_order_ordinal') IS NULL THEN 0 ELSE 1 END AS bit),
        CAST(CASE WHEN COL_LENGTH('sys.sequences', 'last_used_value') IS NULL THEN 0 ELSE 1 END AS bit),
        CAST(CASE WHEN COL_LENGTH('sys.columns', 'is_hidden') IS NULL THEN 0 ELSE 1 END AS bit),
        CAST(CASE WHEN OBJECT_ID('sys.masked_columns') IS NULL THEN 0 ELSE 1 END AS bit),
        CAST(CASE WHEN COL_LENGTH('sys.indexes', 'compression_delay') IS NULL THEN 0 ELSE 1 END AS bit),
        CAST(ISNULL(DATABASEPROPERTYEX(DB_NAME(), 'IsXTPSupported'), 0) AS int),
        CAST(CASE WHEN COL_LENGTH('sys.tables', 'is_node') IS NULL THEN 0 ELSE 1 END AS bit),
        CAST(CASE WHEN COL_LENGTH('sys.tables', 'ledger_type') IS NULL THEN 0 ELSE 1 END AS bit),
        CAST(CASE WHEN CAST(SERVERPROPERTY('EngineEdition') AS int) = 5
                    OR CAST(SERVERPROPERTY('ProductMajorVersion') AS int) >= 17 THEN 1 ELSE 0 END AS bit)";

fn caps_of(r: &Row) -> Caps {
    Caps {
        edition: n(r, 0),
        temporal: b(r, 1),
        retention: b(r, 2),
        memory_optimized: b(r, 3),
        sequential_key: b(r, 4),
        ordered_columnstore: b(r, 5),
        last_used_value: b(r, 6),
        hidden: b(r, 7),
        masked: b(r, 8),
        compression_delay: b(r, 9),
        xtp: n(r, 10) == 1,
        graph: b(r, 11),
        ledger: b(r, 12),
        ordered_nonclustered_columnstore: b(r, 13),
    }
}

/// A filegroup or a partition scheme (with its partitioning column).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Space {
    pub name: String,
    pub column: Option<String>,
}

/// Compression shared by every partition, or each partition's when they
/// differ.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Compression {
    pub all: Option<String>,
    pub parts: Vec<(i32, String)>,
}

impl Compression {
    fn from_parts(parts: Vec<(i32, String)>) -> Self {
        match parts.first() {
            None => Compression::default(),
            Some((_, first)) if parts.iter().all(|(_, c)| c == first) => Compression { all: Some(first.clone()), parts: Vec::new() },
            Some(_) => Compression { all: None, parts },
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Identity {
    pub seed: String,
    pub increment: String,
    pub not_for_replication: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Column {
    pub name: String,
    /// The system type underneath (alias types included).
    pub base: String,
    /// The declared type's name and schema (alias and CLR types).
    pub type_name: String,
    pub type_schema: String,
    pub user_defined: bool,
    pub assembly: bool,
    pub max_length: i32,
    pub precision: i32,
    pub scale: i32,
    pub nullable: bool,
    pub identity: Option<Identity>,
    /// Expression, persisted.
    pub computed: Option<(String, bool)>,
    /// Constraint name, expression.
    pub default: Option<(String, String)>,
    pub collation: Option<String>,
    pub sparse: bool,
    pub rowguidcol: bool,
    pub column_set: bool,
    pub filestream: bool,
    /// Typed XML: collection schema, name, DOCUMENT.
    pub xml_schema: Option<(String, String, bool)>,
    pub hidden: bool,
    pub masked: Option<String>,
}

impl Column {
    fn alias(&self) -> bool {
        !self.assembly && !self.type_name.is_empty() && !self.type_name.eq_ignore_ascii_case(&self.base)
    }

    /// Stored as LOB data (TEXTIMAGE_ON needs one): (max) types, text,
    /// ntext, image, xml and the large CLR types.
    fn is_lob(&self) -> bool {
        matches!(self.base.to_ascii_lowercase().as_str(), "text" | "ntext" | "image" | "xml") || self.max_length == -1
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Spatial {
    pub scheme: String,
    pub bounding_box: Option<[f64; 4]>,
    pub grids: Option<[String; 4]>,
    pub cells_per_object: Option<i32>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Index {
    pub name: String,
    /// sys.indexes.type: 1 clustered, 2 nonclustered, 3 XML, 4 spatial,
    /// 5 clustered columnstore, 6 nonclustered columnstore, 7 hash.
    pub kind: i32,
    pub primary: bool,
    pub unique: bool,
    pub unique_constraint: bool,
    pub filter: Option<String>,
    /// Key columns in key order (name, descending).
    pub keys: Vec<(String, bool)>,
    pub include: Vec<String>,
    /// Columnstore: its columns (nonclustered) or ORDER columns (clustered).
    pub columns: Vec<String>,
    /// Nonclustered columnstore: its ORDER columns.
    pub cs_order: Vec<String>,
    pub fill_factor: i32,
    pub pad_index: bool,
    pub ignore_dup_key: bool,
    pub row_locks: bool,
    pub page_locks: bool,
    pub disabled: bool,
    pub no_recompute: bool,
    pub sequential_key: bool,
    pub compression: Compression,
    pub compression_delay: i32,
    pub space: Option<Space>,
    /// XML: the primary XML index a secondary one uses, and its FOR.
    pub xml_using: Option<String>,
    pub xml_for: Option<String>,
    pub spatial: Option<Spatial>,
    pub bucket_count: Option<i64>,
}

impl Index {
    /// Creation order the server requires: clustered first, then the rest,
    /// primary XML indexes before the secondary ones built on them.
    fn rank(&self) -> u8 {
        match self.kind {
            1 | 5 => 0,
            3 if self.xml_using.is_none() => 2,
            3 => 3,
            _ => 1,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Stat {
    pub name: String,
    pub columns: Vec<String>,
    pub filter: Option<String>,
    pub no_recompute: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Check {
    /// The constraint's object id (what its dependencies are keyed by).
    pub id: i32,
    pub name: String,
    pub definition: String,
    pub disabled: bool,
    pub not_trusted: bool,
    pub not_for_replication: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ForeignKey {
    pub name: String,
    pub columns: Vec<String>,
    pub ref_schema: String,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
    pub on_delete: String,
    pub on_update: String,
    pub not_for_replication: bool,
    pub disabled: bool,
    pub not_trusted: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Temporal {
    pub start: String,
    pub end: String,
    pub history_schema: String,
    pub history_table: String,
    /// `6 MONTHS`, `INFINITE`… (`None`: the server's default).
    pub retention: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Table {
    pub id: i32,
    pub schema: String,
    pub name: String,
    pub columns: Vec<Column>,
    pub pk: Option<Index>,
    pub indexes: Vec<Index>,
    pub stats: Vec<Stat>,
    pub space: Option<Space>,
    pub lob_space: Option<String>,
    /// A heap's own compression.
    pub heap_compression: Compression,
    pub temporal: Option<Temporal>,
    /// Durability of a memory-optimized table.
    pub memory: Option<String>,
    pub identity_last: Option<String>,
    /// Memory-optimized: the largest identity value there is.
    pub identity_max: Option<String>,
    pub checks: Vec<Check>,
    pub fks: Vec<ForeignKey>,
    /// Graph table: `NODE` or `EDGE`.
    pub graph: Option<String>,
    /// sys.tables.ledger_type (0: not a ledger table).
    pub ledger: i32,
}

impl Table {
    fn qname(&self) -> String {
        qn(&self.schema, &self.name)
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PartitionFunction {
    pub name: String,
    pub range_right: bool,
    pub param: Column,
    pub boundaries: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PartitionScheme {
    pub name: String,
    pub function: String,
    pub filegroups: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct UserType {
    pub schema: String,
    pub name: String,
    /// Alias type: its base (a column-like description).
    pub base: Column,
    pub table: Option<TableType>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TableType {
    pub columns: Vec<Column>,
    pub indexes: Vec<Index>,
    pub checks: Vec<String>,
    pub memory_optimized: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Sequence {
    pub schema: String,
    pub name: String,
    pub type_sql: String,
    pub start: String,
    pub increment: String,
    pub min: String,
    pub max: String,
    pub cycle: bool,
    pub cached: bool,
    pub cache_size: Option<i32>,
    pub current: String,
    pub used: bool,
    /// A source without `last_used_value` (before SQL Server 2017) whose
    /// current value is still its start: it can't say whether that value
    /// was handed out.
    pub unknown_use: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Module {
    pub id: i32,
    pub schema: String,
    pub name: String,
    /// sys.objects.type: V, P, FN, IF, TF, TR.
    pub kind: String,
    pub definition: Option<String>,
    pub ansi_nulls: bool,
    pub quoted_identifier: bool,
    pub schema_bound: bool,
    /// Trigger: parent object (id, schema, name), disabled, orders
    /// (`First` / `Last`, statement type).
    pub parent: Option<(i32, String, String)>,
    pub disabled: bool,
    pub orders: Vec<(String, String)>,
    /// The user it's bound to run as (`EXECUTE AS SELF` or `'user'`);
    /// `None` for CALLER and OWNER.
    pub execute_as: Option<String>,
    /// The `EXECUTE AS 'user'` its text declares names a user renamed since
    /// (`execute_as` is the current name).
    pub execute_as_renamed: bool,
}

impl Module {
    fn qname(&self) -> String {
        qn(&self.schema, &self.name)
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct ExtendedProperty {
    pub name: String,
    pub value: String,
    /// The value's sql_variant base type.
    pub base_type: String,
    /// `sp_addextendedproperty` levels: (type, name).
    pub levels: Vec<(String, String)>,
}

/// Everything read from the source.
#[derive(Debug, Clone, Default)]
pub(crate) struct Source {
    pub schemas: Vec<String>,
    pub tables: Vec<Table>,
    /// Indexes of views, by view object id.
    pub view_indexes: HashMap<i32, Vec<Index>>,
    pub functions: Vec<PartitionFunction>,
    pub schemes: Vec<PartitionScheme>,
    pub xml_collections: Vec<(String, String, String)>,
    pub types: Vec<UserType>,
    pub sequences: Vec<Sequence>,
    pub synonyms: Vec<(String, String, String)>,
    pub modules: Vec<Module>,
    /// Module / table dependencies: referencing id → referenced ids.
    pub deps: HashMap<i32, HashSet<i32>>,
    /// Functions a table's columns use (computed columns, defaults), by table id.
    pub column_functions: HashMap<i32, Vec<ColumnUse>>,
    /// User statistics of indexed views, by view object id.
    pub view_stats: HashMap<i32, Vec<Stat>>,
    pub properties: Vec<ExtendedProperty>,
    pub memory_filegroup: Option<String>,
    pub history_retention: bool,
    pub ddl_triggers: i32,
    pub principals: i32,
    /// CLR objects (schema.name) and user assemblies.
    pub clr_objects: Vec<String>,
}

/// A function a column uses: in its computed expression or its default.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ColumnUse {
    pub column: String,
    pub default: bool,
    pub function: i32,
}

/// What the target is.
#[derive(Debug, Clone, Default)]
pub(crate) struct Target {
    pub caps: Caps,
    /// The cloning user's default schema (where an unqualified CREATE lands).
    pub default_schema: String,
    /// Row filegroups (type FG).
    pub filegroups: Vec<String>,
    pub memory_filegroup: Option<String>,
    pub history_retention: bool,
    /// The source's `WITH EXECUTE AS 'user'` users that exist on the target
    /// (spelled as the module's text declares them; looked up by the
    /// target's collation).
    pub users: HashSet<String>,
    /// Those it lacks but has spelled otherwise (letter case): declared →
    /// the target's name.
    pub users_spelled: HashMap<String, String>,
}

// ---------------------------------------------------------------------------
// Reading.
// ---------------------------------------------------------------------------

fn s(r: &Row, i: usize) -> Option<String> {
    r.try_get::<&str, _>(i).ok().flatten().map(str::to_string)
}
fn st(r: &Row, i: usize) -> String {
    s(r, i).unwrap_or_default()
}
fn b(r: &Row, i: usize) -> bool {
    r.try_get::<bool, _>(i).ok().flatten().unwrap_or(false)
}
fn n(r: &Row, i: usize) -> i32 {
    r.try_get::<i32, _>(i).ok().flatten().unwrap_or(0)
}
fn ni(r: &Row, i: usize) -> Option<i32> {
    r.try_get::<i32, _>(i).ok().flatten()
}
fn f(r: &Row, i: usize) -> Option<f64> {
    r.try_get::<f64, _>(i).ok().flatten()
}

async fn simple(c: &mut Conn, sql: &str) -> tiberius::Result<Vec<Row>> {
    c.simple_query(sql).await?.into_first_result().await
}

async fn exec(c: &mut Conn, sql: &str) -> tiberius::Result<()> {
    c.simple_query(sql).await?.into_results().await.map(|_| ())
}

fn catalog_err(e: tiberius::error::Error) -> Error {
    match &e {
        tiberius::error::Error::Server(t) => Error::Query(format!("catálogo del origen: {}", t.message())),
        _ => Error::Query(format!("catálogo del origen: {e}")),
    }
}

async fn read_target(dst: &mut SqlServerSession) -> Result<Target> {
    let caps = dst.rows(CAPS_SQL, &[]).await?.first().map(caps_of).unwrap_or_default();
    let default_schema = dst.rows("SELECT SCHEMA_NAME()", &[]).await?.first().and_then(|r| s(r, 0)).unwrap_or_else(|| "dbo".into());
    let mut t = Target { caps, default_schema, ..Default::default() };
    for r in dst.rows("SELECT name, CAST(type AS nvarchar(2)) FROM sys.filegroups", &[]).await? {
        match st(&r, 1).as_str() {
            "FX" => t.memory_filegroup = s(&r, 0),
            "FG" => t.filegroups.push(st(&r, 0)),
            _ => {}
        }
    }
    if t.caps.retention {
        t.history_retention = dst
            .rows("SELECT is_temporal_history_retention_enabled FROM sys.databases WHERE database_id = DB_ID()", &[])
            .await?
            .first()
            .is_some_and(|r| b(r, 0));
    }
    Ok(t)
}

/// Extra connections to the source's database, for parallel reads.
async fn extra_connections(src: &SqlServerSession, db: &str) -> Vec<Conn> {
    let opens = (0..EXTRA_CONNECTIONS).map(|_| async {
        let mut c = connect_once(src.config.clone()).await.ok()?;
        let here = simple(&mut c, "SELECT DB_NAME()").await.ok()?.first().and_then(|r| s(r, 0));
        if here.as_deref() != Some(db) {
            exec(&mut c, &format!("USE {}", q(db))).await.ok()?;
        }
        exec(&mut c, PREAMBLE).await.ok()?;
        Some(c)
    });
    futures::future::join_all(opens).await.into_iter().flatten().collect()
}

async fn read_source(src: &mut SqlServerSession) -> Result<Source> {
    let db = src.rows("SELECT DB_NAME()", &[]).await?.first().and_then(|r| s(r, 0)).unwrap_or_default();
    let mut extra = extra_connections(src, &db).await;
    if !extra.is_empty() {
        let mut conns: Vec<&mut Conn> = extra.iter_mut().collect();
        return collect(&mut conns).await;
    }
    // No extra connection: the session itself, its isolation level and
    // lock timeout put back afterwards.
    tracing::debug!("sqlserver clone: reading the catalog on the session itself");
    let saved = src
        .rows("SELECT CAST(transaction_isolation_level AS int), CAST(@@LOCK_TIMEOUT AS int) FROM sys.dm_exec_sessions WHERE session_id = @@SPID", &[])
        .await
        .ok()
        .and_then(|r| r.first().map(|r| (n(r, 0), n(r, 1))))
        .unwrap_or((2, -1));
    exec(&mut src.client, PREAMBLE).await.map_err(catalog_err)?;
    let r = collect(&mut [&mut src.client]).await;
    let level = match saved.0 {
        1 => "READ UNCOMMITTED",
        3 => "REPEATABLE READ",
        4 => "SERIALIZABLE",
        5 => "SNAPSHOT",
        _ => "READ COMMITTED",
    };
    if let Err(e) = exec(&mut src.client, &format!("SET TRANSACTION ISOLATION LEVEL {level}; SET LOCK_TIMEOUT {};", saved.1)).await {
        tracing::debug!("sqlserver clone: could not restore the session: {e}");
    }
    r
}

/// Run the queries spread over the connections, each connection one at a
/// time; results in the queries' order.
async fn run_parallel(conns: &mut [&mut Conn], sqls: &[String]) -> Result<Vec<Vec<Row>>> {
    let k = conns.len().max(1);
    let futs = conns.iter_mut().enumerate().map(|(j, c)| {
        let mine: Vec<(usize, &String)> = sqls.iter().enumerate().filter(|(i, _)| i % k == j).collect();
        async move {
            let mut out = Vec::new();
            for (i, sql) in mine {
                let rows = simple(c, sql).await.map_err(|e| {
                    tracing::debug!("sqlserver clone: catalog query failed: {e}\n{sql}");
                    catalog_err(e)
                })?;
                out.push((i, rows));
            }
            Ok::<_, Error>(out)
        }
    });
    let mut slots: Vec<Option<Vec<Row>>> = (0..sqls.len()).map(|_| None).collect();
    for part in futures::future::join_all(futs).await {
        for (i, rows) in part? {
            slots[i] = Some(rows);
        }
    }
    Ok(slots.into_iter().map(Option::unwrap_or_default).collect())
}

/// The objects whose details are read: user tables, views, table types
/// (whose objects are flagged as shipped).
const OBJECTS: &str = "SELECT o.object_id FROM sys.objects o WHERE (o.is_ms_shipped = 0 AND o.type IN ('U', 'V')) OR o.type = 'TT'";

fn source_queries(c: &Caps) -> Vec<String> {
    let temporal = if c.temporal {
        "CAST(t.temporal_type AS int), OBJECT_SCHEMA_NAME(t.history_table_id), OBJECT_NAME(t.history_table_id),
         COL_NAME(p.object_id, p.start_column_id), COL_NAME(p.object_id, p.end_column_id)"
    } else {
        "0, NULL, NULL, NULL, NULL"
    };
    let periods = if c.temporal { "LEFT JOIN sys.periods p ON p.object_id = t.object_id" } else { "" };
    let retention = if c.retention { "CAST(t.history_retention_period AS int), t.history_retention_period_unit_desc" } else { "NULL, NULL" };
    let memory = if c.memory_optimized { "t.is_memory_optimized, t.durability_desc" } else { "CAST(0 AS bit), NULL" };
    let graph = if c.graph { "t.is_node, t.is_edge" } else { "CAST(0 AS bit), CAST(0 AS bit)" };
    let ledger = if c.ledger { "CAST(t.ledger_type AS int)" } else { "0" };
    let hidden = if c.hidden { "c.is_hidden" } else { "CAST(0 AS bit)" };
    let masked = if c.masked { "(SELECT mc.masking_function FROM sys.masked_columns mc WHERE mc.object_id = c.object_id AND mc.column_id = c.column_id)" } else { "NULL" };
    let seqkey = if c.sequential_key { "i.optimize_for_sequential_key" } else { "CAST(0 AS bit)" };
    let delay = if c.compression_delay { "CAST(ISNULL(i.compression_delay, 0) AS int)" } else { "0" };
    let order = if c.ordered_columnstore { "CAST(ISNULL(ic.column_store_order_ordinal, 0) AS int)" } else { "0" };
    // Before SQL Server 2017 there's no last_used_value: a current value
    // past the start says it was used; one still at the start can't tell.
    let used = if c.last_used_value {
        "CAST(CASE WHEN seq.last_used_value IS NULL THEN 0 ELSE 1 END AS bit), CAST(0 AS bit)"
    } else {
        "CAST(CASE WHEN seq.current_value <> seq.start_value THEN 1 ELSE 0 END AS bit),
         CAST(CASE WHEN seq.current_value = seq.start_value THEN 1 ELSE 0 END AS bit)"
    };
    let retention_db = if c.retention {
        "(SELECT is_temporal_history_retention_enabled FROM sys.databases WHERE database_id = DB_ID())"
    } else {
        "CAST(0 AS bit)"
    };
    vec![
        // 0 schemas
        "SELECT name FROM sys.schemas WHERE schema_id > 4 AND schema_id < 16384 ORDER BY name".into(),
        // 1 tables
        format!(
            "SELECT t.object_id, SCHEMA_NAME(t.schema_id), t.name, ds.name, CAST(ds.type AS nvarchar(2)),
                    (SELECT COL_NAME(pc.object_id, pc.column_id) FROM sys.index_columns pc
                      WHERE pc.object_id = t.object_id AND pc.index_id = h.index_id AND pc.partition_ordinal = 1),
                    lob.name, {temporal}, {retention}, {memory}, {graph}, {ledger}
               FROM sys.tables t
               OUTER APPLY (SELECT TOP 1 i.index_id, i.data_space_id FROM sys.indexes i
                             WHERE i.object_id = t.object_id AND i.index_id IN (0, 1)) h
               LEFT JOIN sys.data_spaces ds ON ds.data_space_id = h.data_space_id
               -- A partitioned table's LOB data space is its partition scheme: only a
               -- filegroup is a TEXTIMAGE_ON place (the scheme is cloned as a scheme).
               LEFT JOIN sys.data_spaces lob ON lob.data_space_id = t.lob_data_space_id AND t.lob_data_space_id <> 0 AND lob.type = 'FG'
               {periods}
              WHERE t.is_ms_shipped = 0"
        ),
        // 2 columns (tables and table types)
        format!(
            "SELECT c.object_id, c.name, TYPE_NAME(c.system_type_id), ty.name, SCHEMA_NAME(ty.schema_id),
                    ty.is_user_defined, ty.is_assembly_type,
                    CAST(c.max_length AS int), CAST(c.precision AS int), CAST(c.scale AS int),
                    c.is_nullable, c.is_identity,
                    CAST(idc.seed_value AS nvarchar(40)), CAST(idc.increment_value AS nvarchar(40)),
                    CAST(ISNULL(idc.is_not_for_replication, 0) AS bit), CAST(idc.last_value AS nvarchar(40)),
                    cc.definition, CAST(ISNULL(cc.is_persisted, 0) AS bit),
                    dc.name, dc.definition, c.collation_name, c.is_sparse, c.is_rowguidcol, c.is_column_set,
                    c.is_filestream, CAST(c.xml_collection_id AS int), c.is_xml_document, {hidden}, {masked}
               FROM sys.columns c
               JOIN ({OBJECTS}) o ON o.object_id = c.object_id
               JOIN sys.types ty ON ty.user_type_id = c.user_type_id
               LEFT JOIN sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id
               LEFT JOIN sys.identity_columns idc ON idc.object_id = c.object_id AND idc.column_id = c.column_id
               LEFT JOIN sys.default_constraints dc ON dc.object_id = c.default_object_id
              ORDER BY c.object_id, c.column_id"
        ),
        // 3 indexes
        format!(
            "SELECT i.object_id, i.index_id, i.name, CAST(i.type AS int), i.is_primary_key, i.is_unique, i.is_unique_constraint,
                    i.filter_definition, CAST(i.fill_factor AS int), i.is_padded, i.ignore_dup_key,
                    i.allow_row_locks, i.allow_page_locks, i.is_disabled, CAST(ISNULL(sx.no_recompute, 0) AS bit),
                    {seqkey}, {delay}, ds.name, CAST(ds.type AS nvarchar(2)),
                    pxi.name, xi.secondary_type_desc,
                    tes.tessellation_scheme, tes.bounding_box_xmin, tes.bounding_box_ymin, tes.bounding_box_xmax, tes.bounding_box_ymax,
                    tes.level_1_grid_desc, tes.level_2_grid_desc, tes.level_3_grid_desc, tes.level_4_grid_desc,
                    CAST(tes.cells_per_object AS int), CAST(hx.bucket_count AS bigint)
               FROM sys.indexes i
               JOIN ({OBJECTS}) o ON o.object_id = i.object_id
               LEFT JOIN sys.stats sx ON sx.object_id = i.object_id AND sx.stats_id = i.index_id
               LEFT JOIN sys.data_spaces ds ON ds.data_space_id = i.data_space_id
               LEFT JOIN sys.xml_indexes xi ON xi.object_id = i.object_id AND xi.index_id = i.index_id
               LEFT JOIN sys.indexes pxi ON pxi.object_id = xi.object_id AND pxi.index_id = xi.using_xml_index_id
               LEFT JOIN sys.spatial_index_tessellations tes ON tes.object_id = i.object_id AND tes.index_id = i.index_id
               LEFT JOIN sys.hash_indexes hx ON hx.object_id = i.object_id AND hx.index_id = i.index_id
              WHERE i.type > 0 AND i.is_hypothetical = 0
              ORDER BY i.object_id, i.index_id"
        ),
        // 4 index columns
        format!(
            "SELECT ic.object_id, ic.index_id, c.name, CAST(ic.key_ordinal AS int), ic.is_descending_key,
                    ic.is_included_column, CAST(ic.partition_ordinal AS int), {order}
               FROM sys.index_columns ic
               JOIN ({OBJECTS}) o ON o.object_id = ic.object_id
               JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id
              ORDER BY ic.object_id, ic.index_id, ic.key_ordinal, ic.index_column_id"
        ),
        // 5 partitions' compression
        format!(
            "SELECT p.object_id, p.index_id, p.partition_number, p.data_compression_desc
               FROM sys.partitions p
               JOIN ({OBJECTS}) o ON o.object_id = p.object_id
              ORDER BY p.object_id, p.index_id, p.partition_number"
        ),
        // 6 user-created statistics (tables and indexed views)
        "SELECT sx.object_id, sx.name, sx.filter_definition, sx.no_recompute, c.name
           FROM sys.stats sx
           JOIN sys.objects t ON t.object_id = sx.object_id AND t.is_ms_shipped = 0 AND t.type IN ('U', 'V')
           JOIN sys.stats_columns sc ON sc.object_id = sx.object_id AND sc.stats_id = sx.stats_id
           JOIN sys.columns c ON c.object_id = sc.object_id AND c.column_id = sc.column_id
          WHERE sx.user_created = 1
          ORDER BY sx.object_id, sx.stats_id, sc.stats_column_id"
            .into(),
        // 7 checks (tables and table types)
        format!(
            "SELECT cc.parent_object_id, cc.name, cc.definition, cc.is_disabled, cc.is_not_trusted, cc.is_not_for_replication,
                    cc.object_id
               FROM sys.check_constraints cc
               JOIN ({OBJECTS}) o ON o.object_id = cc.parent_object_id
              ORDER BY cc.parent_object_id, cc.name"
        ),
        // 8 foreign keys, one row per column
        "SELECT fk.parent_object_id, fk.name, pc.name, OBJECT_SCHEMA_NAME(fk.referenced_object_id), OBJECT_NAME(fk.referenced_object_id),
                rc.name, fk.delete_referential_action_desc, fk.update_referential_action_desc,
                fk.is_not_for_replication, fk.is_disabled, fk.is_not_trusted
           FROM sys.foreign_keys fk
           JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id
           JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id
           JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id
          WHERE fk.is_ms_shipped = 0
          ORDER BY fk.parent_object_id, fk.name, fkc.constraint_column_id"
            .into(),
        // 9 modules
        "SELECT m.object_id, SCHEMA_NAME(o.schema_id), o.name, RTRIM(o.type), m.definition,
                m.uses_ansi_nulls, m.uses_quoted_identifier,
                tr.parent_id, OBJECT_SCHEMA_NAME(tr.parent_id), OBJECT_NAME(tr.parent_id), CAST(ISNULL(tr.is_disabled, 0) AS bit),
                m.is_schema_bound,
                CASE WHEN m.execute_as_principal_id > 0 THEN USER_NAME(m.execute_as_principal_id) END
           FROM sys.sql_modules m
           JOIN sys.objects o ON o.object_id = m.object_id
           LEFT JOIN sys.triggers tr ON tr.object_id = m.object_id
          WHERE o.is_ms_shipped = 0 AND o.type IN ('V', 'P', 'FN', 'IF', 'TF', 'TR')
          ORDER BY o.object_id"
            .into(),
        // 10 trigger order
        "SELECT te.object_id, te.type_desc, te.is_first
           FROM sys.trigger_events te
           JOIN sys.triggers t ON t.object_id = te.object_id AND t.parent_class = 1
          WHERE te.is_first = 1 OR te.is_last = 1"
            .into(),
        // 11 object dependencies (a column's or default's with its table and
        // column)
        "SELECT CASE WHEN ro.type = 'D' THEN ro.parent_object_id ELSE d.referencing_id END,
                CAST(CASE WHEN ro.type = 'D' OR (ro.type = 'U' AND d.referencing_minor_id > 0) THEN 1 ELSE 0 END AS bit),
                d.referenced_id,
                CASE WHEN ro.type = 'D' THEN COL_NAME(dc.parent_object_id, dc.parent_column_id)
                     WHEN ro.type = 'U' THEN COL_NAME(d.referencing_id, d.referencing_minor_id) END,
                CAST(CASE WHEN ro.type = 'D' THEN 1 ELSE 0 END AS bit)
           FROM sys.sql_expression_dependencies d
           JOIN sys.objects ro ON ro.object_id = d.referencing_id
           LEFT JOIN sys.default_constraints dc ON dc.object_id = d.referencing_id
          WHERE d.referenced_id IS NOT NULL AND d.referenced_class = 1"
            .into(),
        // 12 user types
        format!(
            "SELECT ty.user_type_id, SCHEMA_NAME(ty.schema_id), ty.name, ty.is_table_type, ty.is_assembly_type,
                    TYPE_NAME(ty.system_type_id), CAST(ty.max_length AS int), CAST(ty.precision AS int), CAST(ty.scale AS int),
                    ty.is_nullable, tt.type_table_object_id, {}
               FROM sys.types ty
               LEFT JOIN sys.table_types tt ON tt.user_type_id = ty.user_type_id
              WHERE ty.is_user_defined = 1
              ORDER BY ty.is_table_type, SCHEMA_NAME(ty.schema_id), ty.name",
            if c.memory_optimized { "CAST(ISNULL(tt.is_memory_optimized, 0) AS bit)" } else { "CAST(0 AS bit)" }
        ),
        // 13 sequences
        format!(
            "SELECT SCHEMA_NAME(seq.schema_id), seq.name, ty.name, CASE WHEN ty.is_user_defined = 1 THEN SCHEMA_NAME(ty.schema_id) END,
                    CAST(seq.precision AS int),
                    CAST(seq.start_value AS nvarchar(40)), CAST(seq.increment AS nvarchar(40)),
                    CAST(seq.minimum_value AS nvarchar(40)), CAST(seq.maximum_value AS nvarchar(40)),
                    seq.is_cycling, seq.is_cached, CAST(seq.cache_size AS int), CAST(seq.current_value AS nvarchar(40)), {used}
               FROM sys.sequences seq
               JOIN sys.types ty ON ty.user_type_id = seq.user_type_id
              WHERE seq.is_ms_shipped = 0
              ORDER BY 1, 2"
        ),
        // 14 synonyms
        "SELECT SCHEMA_NAME(schema_id), name, base_object_name FROM sys.synonyms WHERE is_ms_shipped = 0 ORDER BY 1, 2".into(),
        // 15 partition functions, boundaries as constants that don't depend on the session's language
        "SELECT pf.name, pf.boundary_value_on_right, TYPE_NAME(pp.system_type_id),
                CAST(pp.max_length AS int), CAST(pp.precision AS int), CAST(pp.scale AS int),
                CAST(SQL_VARIANT_PROPERTY(rv.value, 'BaseType') AS nvarchar(128)),
                CASE WHEN CAST(SQL_VARIANT_PROPERTY(rv.value, 'BaseType') AS nvarchar(128)) IN
                          ('date', 'time', 'datetime', 'datetime2', 'smalldatetime', 'datetimeoffset')
                          THEN CONVERT(nvarchar(4000), rv.value, 126)
                     WHEN CAST(SQL_VARIANT_PROPERTY(rv.value, 'BaseType') AS nvarchar(128)) IN ('binary', 'varbinary')
                          THEN CONVERT(nvarchar(4000), CAST(rv.value AS varbinary(8000)), 1)
                     WHEN CAST(SQL_VARIANT_PROPERTY(rv.value, 'BaseType') AS nvarchar(128)) IN ('money', 'smallmoney')
                          THEN CONVERT(nvarchar(4000), CAST(rv.value AS money), 2)
                     WHEN CAST(SQL_VARIANT_PROPERTY(rv.value, 'BaseType') AS nvarchar(128)) IN ('float', 'real')
                          THEN CONVERT(nvarchar(4000), CAST(rv.value AS float), 3)
                     ELSE CAST(rv.value AS nvarchar(4000)) END
           FROM sys.partition_functions pf
           JOIN sys.partition_parameters pp ON pp.function_id = pf.function_id
           LEFT JOIN sys.partition_range_values rv ON rv.function_id = pf.function_id
          ORDER BY pf.name, rv.boundary_id"
            .into(),
        // 16 partition schemes (the NEXT USED filegroup comes last)
        "SELECT ps.name, pf.name, fg.name
           FROM sys.partition_schemes ps
           JOIN sys.partition_functions pf ON pf.function_id = ps.function_id
           JOIN sys.destination_data_spaces dds ON dds.partition_scheme_id = ps.data_space_id
           JOIN sys.data_spaces fg ON fg.data_space_id = dds.data_space_id
          ORDER BY ps.name, dds.destination_id"
            .into(),
        // 17 extended properties, with the levels that address each
        "SELECT CAST(ep.class AS int), ep.name, CAST(ep.value AS nvarchar(max)),
                CAST(SQL_VARIANT_PROPERTY(ep.value, 'BaseType') AS nvarchar(128)),
                CASE ep.class WHEN 3 THEN SCHEMA_NAME(ep.major_id)
                              WHEN 6 THEN SCHEMA_NAME(ty.schema_id)
                              WHEN 10 THEN SCHEMA_NAME(xc.schema_id)
                              ELSE SCHEMA_NAME(tp.schema_id) END,
                CASE ep.class WHEN 6 THEN N'TYPE' WHEN 10 THEN N'XML SCHEMA COLLECTION'
                    ELSE CASE RTRIM(tp.type) WHEN 'U' THEN N'TABLE' WHEN 'V' THEN N'VIEW'
                                       WHEN 'P' THEN N'PROCEDURE' WHEN 'FN' THEN N'FUNCTION' WHEN 'IF' THEN N'FUNCTION'
                                       WHEN 'TF' THEN N'FUNCTION' WHEN 'SN' THEN N'SYNONYM' WHEN 'SO' THEN N'SEQUENCE' END END,
                CASE ep.class WHEN 6 THEN ty.name WHEN 10 THEN xc.name ELSE tp.name END,
                CASE WHEN ep.class = 1 AND o.parent_object_id <> 0
                          THEN CASE WHEN o.type = 'TR' THEN N'TRIGGER' ELSE N'CONSTRAINT' END
                     WHEN ep.class = 1 AND ep.minor_id > 0 THEN N'COLUMN'
                     WHEN ep.class = 2 THEN N'PARAMETER'
                     WHEN ep.class = 7 THEN N'INDEX' END,
                CASE WHEN ep.class = 1 AND o.parent_object_id <> 0 THEN o.name
                     WHEN ep.class = 1 AND ep.minor_id > 0 THEN COL_NAME(ep.major_id, ep.minor_id)
                     WHEN ep.class = 2 THEN (SELECT p.name FROM sys.parameters p
                                              WHERE p.object_id = ep.major_id AND p.parameter_id = ep.minor_id)
                     WHEN ep.class = 7 THEN (SELECT i.name FROM sys.indexes i
                                              WHERE i.object_id = ep.major_id AND i.index_id = ep.minor_id) END
           FROM sys.extended_properties ep
           LEFT JOIN sys.objects o ON ep.class IN (1, 2, 7) AND o.object_id = ep.major_id
           LEFT JOIN sys.objects tp
                  ON tp.object_id = CASE WHEN o.parent_object_id <> 0 THEN o.parent_object_id ELSE o.object_id END
           LEFT JOIN sys.types ty ON ep.class = 6 AND ty.user_type_id = ep.major_id
           LEFT JOIN sys.xml_schema_collections xc ON ep.class = 10 AND xc.xml_collection_id = ep.major_id
          WHERE ep.class IN (0, 3, 6, 10) OR (ep.class IN (1, 2, 7) AND tp.is_ms_shipped = 0)
          ORDER BY ep.class, ep.major_id, ep.minor_id, ep.name"
            .into(),
        // 18 database-wide facts
        format!(
            "SELECT (SELECT TOP 1 name FROM sys.filegroups WHERE type = 'FX'), {retention_db},
                    (SELECT COUNT(*) FROM sys.triggers WHERE parent_class = 0),
                    (SELECT COUNT(*) FROM sys.database_principals
                      WHERE principal_id > 4 AND is_fixed_role = 0 AND name <> N'public' AND type IN ('S', 'U', 'G', 'R', 'E', 'X', 'C', 'K'))"
        ),
        // 19 XML schema collections
        "SELECT xc.xml_collection_id, SCHEMA_NAME(xc.schema_id), xc.name,
                CAST(XML_SCHEMA_NAMESPACE(SCHEMA_NAME(xc.schema_id), xc.name) AS nvarchar(max))
           FROM sys.xml_schema_collections xc
          WHERE xc.xml_collection_id > 1
          ORDER BY xc.xml_collection_id"
            .into(),
        // 20 CLR objects and user assemblies (not cloned)
        "SELECT SCHEMA_NAME(schema_id) + N'.' + name FROM sys.objects
          WHERE is_ms_shipped = 0 AND type IN ('PC', 'FS', 'FT', 'TA', 'AF')
         UNION ALL
         SELECT N'assembly ' + name FROM sys.assemblies WHERE is_user_defined = 1"
            .into(),
    ]
}

async fn collect(conns: &mut [&mut Conn]) -> Result<Source> {
    let caps = {
        let c = conns.first_mut().ok_or_else(|| Error::State("sin conexión al origen".into()))?;
        simple(c, CAPS_SQL).await.map_err(catalog_err)?.first().map(caps_of).unwrap_or_default()
    };
    let rows = run_parallel(conns, &source_queries(&caps)).await?;
    Ok(parse(rows))
}

fn identity_of(r: &Row) -> Option<Identity> {
    b(r, 11).then(|| Identity { seed: st(r, 12), increment: st(r, 13), not_for_replication: b(r, 14) })
}

fn column_of(r: &Row, xml: &HashMap<i32, (String, String)>) -> Column {
    Column {
        name: st(r, 1),
        base: st(r, 2),
        type_name: st(r, 3),
        type_schema: st(r, 4),
        user_defined: b(r, 5),
        assembly: b(r, 6),
        max_length: n(r, 7),
        precision: n(r, 8),
        scale: n(r, 9),
        nullable: b(r, 10),
        identity: identity_of(r),
        computed: s(r, 16).map(|d| (d, b(r, 17))),
        default: match (s(r, 18), s(r, 19)) {
            (Some(name), Some(def)) => Some((name, def)),
            _ => None,
        },
        collation: s(r, 20),
        sparse: b(r, 21),
        rowguidcol: b(r, 22),
        column_set: b(r, 23),
        filestream: b(r, 24),
        xml_schema: xml.get(&n(r, 25)).map(|(sc, nm)| (sc.clone(), nm.clone(), b(r, 26))),
        hidden: b(r, 27),
        masked: s(r, 28),
    }
}

/// A boundary value as a constant of the function's parameter type.
fn boundary_literal(base: &str, text: &str, ty: &str) -> String {
    match base {
        "date" | "time" | "datetime" | "datetime2" | "smalldatetime" | "datetimeoffset" => format!("CONVERT({ty}, {}, 126)", lit(text)),
        "binary" | "varbinary" => text.to_string(),
        "char" | "varchar" => format!("'{}'", text.replace('\'', "''")),
        "nchar" | "nvarchar" => lit(text),
        "uniqueidentifier" => format!("CAST({} AS uniqueidentifier)", lit(text)),
        _ => text.to_string(),
    }
}

fn parse(mut rows: Vec<Vec<Row>>) -> Source {
    let mut take = |i: usize| std::mem::take(&mut rows[i]);
    let mut src = Source { schemas: take(0).iter().map(|r| st(r, 0)).collect(), ..Default::default() };

    let xml: HashMap<i32, (String, String)> = take(19)
        .iter()
        .map(|r| {
            src.xml_collections.push((st(r, 1), st(r, 2), st(r, 3)));
            (n(r, 0), (st(r, 1), st(r, 2)))
        })
        .collect();

    let mut tables: BTreeMap<i32, Table> = BTreeMap::new();
    for r in take(1) {
        let id = n(&r, 0);
        let temporal = (n(&r, 7) == 2).then(|| Temporal {
            start: st(&r, 10),
            end: st(&r, 11),
            history_schema: st(&r, 8),
            history_table: st(&r, 9),
            retention: match (ni(&r, 12), s(&r, 13)) {
                (_, Some(u)) if u.eq_ignore_ascii_case("INFINITE") => Some("INFINITE".into()),
                (Some(p), Some(u)) if p > 0 => Some(format!("{p} {}S", u.to_ascii_uppercase())),
                _ => None,
            },
        });
        let space = s(&r, 3).map(|name| Space { name, column: if st(&r, 4) == "PS" { s(&r, 5) } else { None } });
        tables.insert(
            id,
            Table {
                id,
                schema: st(&r, 1),
                name: st(&r, 2),
                space,
                lob_space: s(&r, 6),
                temporal,
                memory: b(&r, 14).then(|| s(&r, 15).unwrap_or_else(|| "SCHEMA_AND_DATA".into())),
                graph: if b(&r, 16) {
                    Some("NODE".into())
                } else if b(&r, 17) {
                    Some("EDGE".into())
                } else {
                    None
                },
                ledger: n(&r, 18),
                ..Default::default()
            },
        );
    }

    // Table types: object id → (type key).
    let mut table_types: HashMap<i32, TableType> = HashMap::new();
    let mut tt_slots: Vec<(usize, i32)> = Vec::new();
    for r in take(12) {
        let tt_object = ni(&r, 10);
        let ut = UserType {
            schema: st(&r, 1),
            name: st(&r, 2),
            base: Column { base: st(&r, 5), max_length: n(&r, 6), precision: n(&r, 7), scale: n(&r, 8), nullable: b(&r, 9), ..Default::default() },
            table: None,
        };
        if b(&r, 4) {
            // CLR type: needs its assembly.
            src.clr_objects.push(format!("tipo {}.{}", ut.schema, ut.name));
            continue;
        }
        if let (true, Some(o)) = (b(&r, 3), tt_object) {
            table_types.insert(o, TableType { memory_optimized: b(&r, 11), ..Default::default() });
            tt_slots.push((src.types.len(), o));
            src.types.push(UserType { table: Some(TableType::default()), base: Column::default(), ..ut });
        } else {
            src.types.push(ut);
        }
    }

    for r in take(2) {
        let id = n(&r, 0);
        let col = column_of(&r, &xml);
        if let Some(t) = tables.get_mut(&id) {
            if col.identity.is_some() {
                t.identity_last = s(&r, 15);
            }
            t.columns.push(col);
        } else if let Some(tt) = table_types.get_mut(&id) {
            tt.columns.push(col);
        }
    }

    // Indexes, their columns and compression.
    let mut idx: BTreeMap<(i32, i32), Index> = BTreeMap::new();
    for r in take(3) {
        let kind = n(&r, 3);
        let space = s(&r, 17).map(|name| Space { name, column: None });
        let is_ps = st(&r, 18) == "PS";
        let spatial = (kind == 4).then(|| Spatial {
            scheme: st(&r, 21),
            bounding_box: match (f(&r, 22), f(&r, 23), f(&r, 24), f(&r, 25)) {
                (Some(a), Some(b), Some(c), Some(d)) => Some([a, b, c, d]),
                _ => None,
            },
            grids: match (s(&r, 26), s(&r, 27), s(&r, 28), s(&r, 29)) {
                (Some(a), Some(b), Some(c), Some(d)) => Some([a, b, c, d]),
                _ => None,
            },
            cells_per_object: ni(&r, 30),
        });
        let mut i = Index {
            name: st(&r, 2),
            kind,
            primary: b(&r, 4),
            unique: b(&r, 5),
            unique_constraint: b(&r, 6),
            filter: s(&r, 7),
            fill_factor: n(&r, 8),
            pad_index: b(&r, 9),
            ignore_dup_key: b(&r, 10),
            row_locks: b(&r, 11),
            page_locks: b(&r, 12),
            disabled: b(&r, 13),
            no_recompute: b(&r, 14),
            sequential_key: b(&r, 15),
            compression_delay: n(&r, 16),
            // XML indexes live with their table: no ON of their own.
            space: if kind == 3 { None } else { space },
            xml_using: s(&r, 19),
            xml_for: s(&r, 20),
            spatial,
            bucket_count: r.try_get::<i64, _>(31).ok().flatten(),
            ..Default::default()
        };
        if is_ps {
            // Its partitioning column comes with the index columns.
            if let Some(sp) = &mut i.space {
                sp.column = Some(String::new());
            }
        }
        idx.insert((n(&r, 0), n(&r, 1)), i);
    }
    let mut cs_order: HashMap<(i32, i32), Vec<(i32, String)>> = HashMap::new();
    for r in take(4) {
        let key = (n(&r, 0), n(&r, 1));
        let Some(i) = idx.get_mut(&key) else { continue };
        let (col, key_ordinal, desc, included, part, order) = (st(&r, 2), n(&r, 3), b(&r, 4), b(&r, 5), n(&r, 6), n(&r, 7));
        if part == 1 {
            if let Some(sp) = &mut i.space {
                sp.column = Some(col.clone());
            }
        }
        match i.kind {
            5 => {
                if order > 0 {
                    cs_order.entry(key).or_default().push((order, col));
                }
            }
            6 => {
                if order > 0 {
                    cs_order.entry(key).or_default().push((order, col.clone()));
                }
                if key_ordinal > 0 || included || part == 0 {
                    i.columns.push(col);
                }
            }
            3 | 4 => {
                if key_ordinal > 0 || part == 0 {
                    i.keys.push((col, false));
                }
            }
            _ => {
                if key_ordinal > 0 {
                    i.keys.push((col, desc));
                } else if included {
                    i.include.push(col);
                }
            }
        }
    }
    for (key, mut cols) in cs_order {
        cols.sort();
        if let Some(i) = idx.get_mut(&key) {
            let cols = cols.into_iter().map(|(_, c)| c).collect();
            if i.kind == 5 {
                i.columns = cols;
            } else {
                i.cs_order = cols;
            }
        }
    }
    let mut parts: BTreeMap<(i32, i32), Vec<(i32, String)>> = BTreeMap::new();
    for r in take(5) {
        parts.entry((n(&r, 0), n(&r, 1))).or_default().push((n(&r, 2), st(&r, 3)));
    }
    for (key, p) in parts {
        let c = Compression::from_parts(p);
        if key.1 == 0 {
            if let Some(t) = tables.get_mut(&key.0) {
                t.heap_compression = c;
            }
        } else if let Some(i) = idx.get_mut(&key) {
            i.compression = c;
        }
    }
    for ((obj, _), i) in idx {
        // A partition scheme whose column wasn't found (shouldn't happen)
        // is left as a plain data space name.
        let i = Index {
            space: i.space.map(|sp| Space { column: sp.column.filter(|c| !c.is_empty()), ..sp }),
            ..i
        };
        if let Some(t) = tables.get_mut(&obj) {
            if i.primary {
                t.pk = Some(i);
            } else {
                t.indexes.push(i);
            }
        } else if let Some(tt) = table_types.get_mut(&obj) {
            tt.indexes.push(i);
        } else {
            src.view_indexes.entry(obj).or_default().push(i);
        }
    }

    for r in take(6) {
        let id = n(&r, 0);
        let list = match tables.get_mut(&id) {
            Some(t) => &mut t.stats,
            None => src.view_stats.entry(id).or_default(),
        };
        let name = st(&r, 1);
        if list.last().map(|x| &x.name) != Some(&name) {
            list.push(Stat { name, filter: s(&r, 2), no_recompute: b(&r, 3), columns: Vec::new() });
        }
        if let Some(x) = list.last_mut() {
            x.columns.push(st(&r, 4));
        }
    }

    for r in take(7) {
        let id = n(&r, 0);
        let ck = Check {
            id: n(&r, 6),
            name: st(&r, 1),
            definition: st(&r, 2),
            disabled: b(&r, 3),
            not_trusted: b(&r, 4),
            not_for_replication: b(&r, 5),
        };
        if let Some(t) = tables.get_mut(&id) {
            t.checks.push(ck);
        } else if let Some(tt) = table_types.get_mut(&id) {
            tt.checks.push(ck.definition);
        }
    }

    for r in take(8) {
        let Some(t) = tables.get_mut(&n(&r, 0)) else { continue };
        let name = st(&r, 1);
        if t.fks.last().map(|x| &x.name) != Some(&name) {
            t.fks.push(ForeignKey {
                name,
                ref_schema: st(&r, 3),
                ref_table: st(&r, 4),
                on_delete: st(&r, 6),
                on_update: st(&r, 7),
                not_for_replication: b(&r, 8),
                disabled: b(&r, 9),
                not_trusted: b(&r, 10),
                ..Default::default()
            });
        }
        if let Some(fk) = t.fks.last_mut() {
            fk.columns.push(st(&r, 2));
            fk.ref_columns.push(st(&r, 5));
        }
    }

    let mut orders: HashMap<i32, Vec<(String, String)>> = HashMap::new();
    for r in take(10) {
        orders.entry(n(&r, 0)).or_default().push((if b(&r, 2) { "First" } else { "Last" }.into(), st(&r, 1)));
    }
    for r in take(9) {
        let id = n(&r, 0);
        src.modules.push(Module {
            id,
            schema: st(&r, 1),
            name: st(&r, 2),
            kind: st(&r, 3),
            definition: s(&r, 4).filter(|d| !d.trim().is_empty()),
            ansi_nulls: b(&r, 5),
            quoted_identifier: b(&r, 6),
            parent: ni(&r, 7).map(|p| (p, st(&r, 8), st(&r, 9))),
            disabled: b(&r, 10),
            schema_bound: b(&r, 11),
            orders: orders.remove(&id).unwrap_or_default(),
            execute_as: s(&r, 12),
            execute_as_renamed: false,
        });
    }
    for r in take(11) {
        let (from, column_level, to) = (n(&r, 0), b(&r, 1), n(&r, 2));
        if column_level {
            let u = ColumnUse { column: st(&r, 3), default: b(&r, 4), function: to };
            let list = src.column_functions.entry(from).or_default();
            if !list.contains(&u) {
                list.push(u);
            }
        } else {
            src.deps.entry(from).or_default().insert(to);
        }
    }

    // Table types got their details: put them back into the types list.
    for (slot, object) in tt_slots {
        if let Some(full) = table_types.remove(&object) {
            src.types[slot].table = Some(full);
        }
    }

    for r in take(13) {
        let (ty, ty_schema, precision) = (st(&r, 2), s(&r, 3), n(&r, 4));
        let type_sql = match ty_schema {
            Some(sc) => qn(&sc, &ty),
            None if matches!(ty.as_str(), "decimal" | "numeric") => format!("{ty}({precision}, 0)"),
            None => ty,
        };
        src.sequences.push(Sequence {
            schema: st(&r, 0),
            name: st(&r, 1),
            type_sql,
            start: st(&r, 5),
            increment: st(&r, 6),
            min: st(&r, 7),
            max: st(&r, 8),
            cycle: b(&r, 9),
            cached: b(&r, 10),
            cache_size: ni(&r, 11),
            current: st(&r, 12),
            used: b(&r, 13),
            unknown_use: b(&r, 14),
        });
    }
    src.synonyms = take(14).iter().map(|r| (st(r, 0), st(r, 1), st(r, 2))).collect();

    for r in take(15) {
        let name = st(&r, 0);
        if src.functions.last().map(|f| &f.name) != Some(&name) {
            src.functions.push(PartitionFunction {
                name,
                range_right: b(&r, 1),
                param: Column { base: st(&r, 2), max_length: n(&r, 3), precision: n(&r, 4), scale: n(&r, 5), ..Default::default() },
                boundaries: Vec::new(),
            });
        }
        if let (Some(base), Some(text), Some(pf)) = (s(&r, 6), s(&r, 7), src.functions.last_mut()) {
            let ty = type_sql(&pf.param);
            pf.boundaries.push(boundary_literal(&base, &text, &ty));
        }
    }
    for r in take(16) {
        let name = st(&r, 0);
        if src.schemes.last().map(|p| &p.name) != Some(&name) {
            src.schemes.push(PartitionScheme { name, function: st(&r, 1), filegroups: Vec::new() });
        }
        if let Some(ps) = src.schemes.last_mut() {
            ps.filegroups.push(st(&r, 2));
        }
    }

    for r in take(17) {
        let class = n(&r, 0);
        let (schema, l1t, l1n, l2t, l2n) = (s(&r, 4), s(&r, 5), s(&r, 6), s(&r, 7), s(&r, 8));
        let mut levels = Vec::new();
        match class {
            0 => {}
            3 => levels.push(("SCHEMA".to_string(), schema.unwrap_or_default())),
            _ => {
                // An object sp_addextendedproperty can't address: skipped.
                let (Some(schema), Some(l1t), Some(l1n)) = (schema, l1t, l1n) else { continue };
                levels.push(("SCHEMA".into(), schema));
                levels.push((l1t, l1n));
                if let (Some(t), Some(nm)) = (l2t, l2n) {
                    levels.push((t, nm));
                }
            }
        }
        src.properties.push(ExtendedProperty { name: st(&r, 1), value: st(&r, 2), base_type: st(&r, 3), levels });
    }

    if let Some(r) = take(18).first() {
        src.memory_filegroup = s(r, 0);
        src.history_retention = b(r, 1);
        src.ddl_triggers = n(r, 2);
        src.principals = n(r, 3);
    }
    src.clr_objects.extend(take(20).iter().map(|r| st(r, 0)));
    src.tables = tables.into_values().collect();
    src
}

// ---------------------------------------------------------------------------
// Building the script.
// ---------------------------------------------------------------------------

fn q(ident: &str) -> String {
    format!("[{}]", ident.replace(']', "]]"))
}

fn qn(schema: &str, name: &str) -> String {
    format!("{}.{}", q(schema), q(name))
}

/// `N'…'` literal.
fn lit(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

fn key_eq(a: (&str, &str), b: (&str, &str)) -> bool {
    a.0.eq_ignore_ascii_case(b.0) && a.1.eq_ignore_ascii_case(b.1)
}

fn build(mut src: Source, target: &Target, requested: &[ObjectRef]) -> Result<CloneScript> {
    let mut notes = Vec::new();
    let tc = &target.caps;

    // The tables asked for, as the source has them.
    let mut picked: Vec<Table> = Vec::new();
    for r in requested {
        let schema = r.schema().unwrap_or("dbo");
        let pos = src
            .tables
            .iter()
            .position(|t| key_eq((&t.schema, &t.name), (schema, &r.name)))
            .ok_or_else(|| Error::Query(format!("La tabla «{schema}.{}» no existe en el origen", r.name)))?;
        picked.push(src.tables.swap_remove(pos));
    }
    // Graph and ledger tables: the engine writes their internal columns
    // ($node_id / $from_id / $to_id; the ledger's transaction and sequence
    // columns and its verifiable history), so a copy can't be the same
    // table. They're left out, and said.
    let mut excluded: Vec<Table> = Vec::new();
    picked.retain(|t| {
        let out = t.graph.is_some() || t.ledger != 0;
        if out {
            excluded.push(t.clone());
        }
        !out
    });
    for t in &excluded {
        notes.push(match &t.graph {
            Some(kind) => format!(
                "{}: es una tabla de grafo (AS {kind}) y no se clona: el motor genera sus columnas internas \
                 ($node_id, $from_id, $to_id) y no se pueden copiar tal cual",
                t.qname()
            ),
            None => format!(
                "{}: es una tabla ledger y no se clona: el motor genera sus columnas GENERATED ALWAYS AS TRANSACTION_ID / \
                 SEQUENCE_NUMBER y su historial verificable, así que una copia no sería el mismo ledger",
                t.qname()
            ),
        });
    }
    let cloned = |schema: &str, name: &str, list: &[Table]| list.iter().any(|t| key_eq((&t.schema, &t.name), (schema, name)));
    let table_ids: HashSet<i32> = picked.iter().map(|t| t.id).collect();
    // Every source table's id (the ones not cloned included).
    let all_table_ids: HashSet<i32> = picked.iter().chain(&excluded).chain(src.tables.iter()).map(|t| t.id).collect();
    // Every name the source uses in the objects' namespace, lowercase.
    let taken: HashSet<(String, String)> = picked
        .iter()
        .chain(&excluded)
        .chain(src.tables.iter())
        .map(|t| (t.schema.to_lowercase(), t.name.to_lowercase()))
        .chain(src.modules.iter().map(|m| (m.schema.to_lowercase(), m.name.to_lowercase())))
        .chain(src.synonyms.iter().map(|(sc, nm, _)| (sc.to_lowercase(), nm.to_lowercase())))
        .chain(src.sequences.iter().map(|q| (q.schema.to_lowercase(), q.name.to_lowercase())))
        .collect();
    let place = Placement { default_schema: &target.default_schema, taken: &taken };

    // ---- what the target can't take: adapted, and said.
    adapt_memory_optimized(&mut picked, target, &mut notes);
    let view_names: HashMap<i32, String> = src.modules.iter().filter(|m| m.kind == "V").map(|m| (m.id, m.qname())).collect();
    adapt_features(&mut picked, &mut src.view_indexes, &view_names, target, &mut notes);

    let mut before = Vec::new();
    for sc in &src.schemas {
        before.push(format!("IF SCHEMA_ID({}) IS NULL EXEC({});", lit(sc), lit(&format!("CREATE SCHEMA {}", q(sc)))));
    }

    // Filegroups and partition schemes the cloned tables use.
    let used_schemes: Vec<PartitionScheme> = {
        let mut names: Vec<String> = Vec::new();
        for t in picked.iter().filter(|t| t.memory.is_none()) {
            for sp in spaces(t) {
                if sp.column.is_some() && !names.iter().any(|x| x.eq_ignore_ascii_case(&sp.name)) {
                    names.push(sp.name.clone());
                }
            }
        }
        for i in src.view_indexes.values().flatten() {
            if let Some(sp) = i.space.as_ref().filter(|sp| sp.column.is_some()) {
                if !names.iter().any(|x| x.eq_ignore_ascii_case(&sp.name)) {
                    names.push(sp.name.clone());
                }
            }
        }
        src.schemes.iter().filter(|p| names.iter().any(|x| x.eq_ignore_ascii_case(&p.name))).cloned().collect()
    };
    let mut schemes = used_schemes;
    let missing = adapt_filegroups(&mut picked, &mut src.view_indexes, &mut schemes, target, &mut notes);
    for fg in &missing {
        before.push(create_filegroup(fg));
    }
    if picked.iter().any(|t| t.memory.is_some()) && target.memory_filegroup.is_none() && !tc.azure_db() {
        before.push(create_memory_filegroup(src.memory_filegroup.as_deref().unwrap_or("MEMORY_OPTIMIZED_DATA")));
    }
    for pf in src.functions.iter().filter(|f| schemes.iter().any(|p| p.function.eq_ignore_ascii_case(&f.name))) {
        before.push(partition_function(pf));
    }
    for ps in &schemes {
        before.push(partition_scheme(ps));
    }
    for (sc, name, body) in &src.xml_collections {
        before.push(format!(
            "IF NOT EXISTS (SELECT 1 FROM sys.xml_schema_collections WHERE name = {} AND schema_id = SCHEMA_ID({}))\n    CREATE XML SCHEMA COLLECTION {} AS {};",
            lit(name),
            lit(sc),
            qn(sc, name),
            lit(body)
        ));
    }
    for t in &src.types {
        before.push(user_type(t));
    }
    for sq in &src.sequences {
        before.push(sequence(sq));
    }

    // ---- code: which objects come along, in dependency order.
    let module_ids: HashMap<i32, usize> = src.modules.iter().enumerate().map(|(i, m)| (m.id, i)).collect();
    let mut skipped: HashSet<i32> = HashSet::new();
    // Why a module isn't cloned, when it's the module itself (not tables
    // left out): the module where it starts and its cause, repeated by
    // whatever needs it.
    let mut why: HashMap<i32, (String, String)> = HashMap::new();
    for m in &src.modules {
        if m.definition.is_none() {
            let cause = "su definición está cifrada (WITH ENCRYPTION) o no se puede leer".to_string();
            notes.push(format!("{} ({}): {cause}; no se clona", m.qname(), kind_name(&m.kind)));
            skipped.insert(m.id);
            why.insert(m.id, (m.qname(), cause));
        } else if let Some((user, cause)) = executes_as_named(m).and_then(|u| execute_as_problem(m, &u, target).map(|c| (u, c))) {
            notes.push(format!(
                "{} ({}): se declara WITH EXECUTE AS {} y no se clona: {cause}",
                m.qname(),
                kind_name(&m.kind),
                lit_plain(&user)
            ));
            skipped.insert(m.id);
            why.insert(m.id, (m.qname(), cause));
        }
    }
    // Views and functions over tables that aren't cloned can't be
    // created; nor triggers of tables that aren't. Procedures resolve
    // their names when they run: they always come.
    loop {
        let mut changed = false;
        for m in &src.modules {
            if skipped.contains(&m.id) {
                continue;
            }
            let missing_table = matches!(m.kind.as_str(), "V" | "FN" | "IF" | "TF")
                && src.deps.get(&m.id).is_some_and(|d| d.iter().any(|x| all_table_ids.contains(x) && !table_ids.contains(x)));
            let missing_module = src.deps.get(&m.id).is_some_and(|d| d.iter().any(|x| skipped.contains(x)))
                && m.kind != "P";
            let orphan_trigger = m.kind == "TR"
                && m.parent.as_ref().is_some_and(|(p, _, _)| (all_table_ids.contains(p) && !table_ids.contains(p)) || skipped.contains(p));
            if missing_table || missing_module || orphan_trigger {
                skipped.insert(m.id);
                changed = true;
                // Left out for a module with a cause of its own: that cause
                // is said (and passed on), adding the tables when those
                // are missing too (fixing the cause alone wouldn't bring
                // it); otherwise, the tables.
                let skipped_deps: Vec<i32> =
                    src.deps.get(&m.id).into_iter().flatten().copied().filter(|x| skipped.contains(x) && *x != m.id).collect();
                let also_tables = missing_table || skipped_deps.iter().any(|x| !why.contains_key(x));
                let cause = skipped_deps
                    .iter()
                    .filter(|x| why.contains_key(x))
                    .min()
                    .and_then(|x| why.get(x))
                    .filter(|_| !orphan_trigger)
                    .map(|(root, c)| {
                        let c = if also_tables { format!("{c}; además, {} usa tablas u objetos que no se clonan", m.qname()) } else { c.clone() };
                        (root.clone(), c)
                    });
                if m.definition.is_some() && !orphan_trigger {
                    match &cause {
                        Some((root, c)) => notes.push(format!(
                            "{} ({}): no se crea porque usa {root}, que no se clona: {c}",
                            m.qname(),
                            kind_name(&m.kind)
                        )),
                        None => {
                            notes.push(format!("{} ({}): usa tablas u objetos que no se clonan; no se crea", m.qname(), kind_name(&m.kind)))
                        }
                    }
                }
                if let Some(c) = cause {
                    why.insert(m.id, c);
                }
            }
        }
        if !changed {
            break;
        }
    }
    // Functions a cloned table's columns use (computed columns, defaults)
    // must exist before its CREATE TABLE. Those that bind to no table go in
    // `before`; those that bind to other cloned tables (schema-bound, inline
    // or through a view) are created right before the table, which then
    // comes after those tables. A default whose function needs its own
    // table (or a cycle of them) is added once the code exists.
    let binds = |m: &Module| matches!(m.kind.as_str(), "V" | "IF") || m.schema_bound;
    let needs = |start: i32| -> (HashSet<i32>, HashSet<i32>) {
        let (mut mods, mut tabs) = (HashSet::new(), HashSet::new());
        let mut stack = vec![start];
        while let Some(id) = stack.pop() {
            let Some(&i) = module_ids.get(&id) else { continue };
            if !mods.insert(id) {
                continue;
            }
            let m = &src.modules[i];
            for &d in src.deps.get(&id).into_iter().flatten() {
                if all_table_ids.contains(&d) {
                    if binds(m) {
                        tabs.insert(d);
                    }
                } else if let Some(&j) = module_ids.get(&d) {
                    if binds(m) || matches!(src.modules[j].kind.as_str(), "FN" | "IF" | "TF") {
                        stack.push(d);
                    }
                }
            }
        }
        (mods, tabs)
    };
    let mut early: HashSet<i32> = HashSet::new();
    let mut prefix: HashMap<i32, HashSet<i32>> = HashMap::new();
    let mut wait_for: HashMap<i32, HashSet<i32>> = HashMap::new();
    let mut late_uses: HashMap<i32, Vec<ColumnUse>> = HashMap::new();
    let mut detach: Vec<(i32, String)> = Vec::new();
    let mut dropped_defaults: Vec<(i32, String)> = Vec::new();
    // Why a function that isn't cloned isn't: its own cause (or the one of
    // what it needs), when it has one.
    let reason_of = |f: &Module| {
        why.get(&f.id).map(|(root, c)| {
            if root == &f.qname() {
                format!("{root}, que no se clona: {c}")
            } else {
                format!("{}, que depende de {root}, y {root} no se clona: {c}", f.qname())
            }
        })
    };
    for t in &picked {
        for u in src.column_functions.get(&t.id).into_iter().flatten() {
            let Some(&fi) = module_ids.get(&u.function) else { continue };
            let f = &src.modules[fi];
            let (mods, tabs) = needs(u.function);
            if mods.iter().any(|x| skipped.contains(x)) {
                let reason = reason_of(f);
                if let Some(reason) = &reason {
                    if !u.default {
                        return Err(Error::Unsupported(format!(
                            "{}: la tabla no se puede crear igual porque la columna calculada [{}] usa {reason}",
                            t.qname(),
                            u.column
                        )));
                    }
                    notes.push(format!("{}: la columna [{}] queda sin su DEFAULT porque usa {reason}", t.qname(), u.column));
                    dropped_defaults.push((t.id, u.column.clone()));
                    continue;
                }
                if !u.default {
                    return Err(Error::Unsupported(format!(
                        "{}: la columna calculada [{}] usa {}, que lee tablas u objetos que no se clonan; \
                         hay que clonarlos también para poder crear la tabla igual",
                        t.qname(),
                        u.column,
                        f.qname()
                    )));
                }
                notes.push(format!(
                    "{}: el DEFAULT de la columna [{}] usa {}, que no se clona (usa tablas u objetos que no se clonan); \
                     la columna queda sin ese DEFAULT",
                    t.qname(),
                    u.column,
                    f.qname()
                ));
                dropped_defaults.push((t.id, u.column.clone()));
                continue;
            }
            let has_view = mods.iter().any(|x| module_ids.get(x).is_some_and(|&i| src.modules[i].kind == "V"));
            if tabs.is_empty() && !has_view {
                early.extend(mods);
            } else if tabs.contains(&t.id) {
                if !u.default {
                    return Err(Error::Unsupported(format!(
                        "{}: la columna calculada [{}] usa {}, que necesita la propia tabla para crearse; \
                         no se puede crear la tabla igual que en el origen",
                        t.qname(),
                        u.column,
                        f.qname()
                    )));
                }
                detach.push((t.id, u.column.clone()));
            } else {
                prefix.entry(t.id).or_default().extend(mods);
                wait_for.entry(t.id).or_default().extend(tabs);
                late_uses.entry(t.id).or_default().push(u.clone());
            }
        }
    }
    // Tables in an order that creates those first (stable otherwise); in a
    // cycle, the first one waits for nothing and its defaults come later.
    let mut remaining: Vec<Table> = std::mem::take(&mut picked);
    let mut placed: HashSet<i32> = HashSet::new();
    while !remaining.is_empty() {
        let ready = remaining.iter().position(|t| wait_for.get(&t.id).is_none_or(|d| d.iter().all(|x| placed.contains(x))));
        let t = match ready {
            Some(p) => remaining.remove(p),
            None => {
                let t = remaining.remove(0);
                prefix.remove(&t.id);
                wait_for.remove(&t.id);
                for u in late_uses.remove(&t.id).unwrap_or_default() {
                    if !u.default {
                        return Err(Error::Unsupported(format!(
                            "{}: la columna calculada [{}] usa funciones que leen tablas que a su vez dependen de esta; \
                             no se puede crear la tabla igual que en el origen",
                            t.qname(),
                            u.column
                        )));
                    }
                    detach.push((t.id, u.column.clone()));
                }
                t
            }
        };
        placed.insert(t.id);
        picked.push(t);
    }
    // Defaults added after the code (or not at all), out of CREATE TABLE.
    let mut detached_sql = Vec::new();
    for t in picked.iter_mut() {
        for c in t.columns.iter_mut() {
            let is = |list: &[(i32, String)]| list.iter().any(|(id, col)| *id == t.id && *col == c.name);
            if is(&dropped_defaults) {
                c.default = None;
            } else if is(&detach) {
                if let Some((name, def)) = c.default.take() {
                    detached_sql.push(format!(
                        "IF OBJECT_ID({}, 'D') IS NULL\n    ALTER TABLE {} ADD CONSTRAINT {} DEFAULT {def} FOR {};",
                        lit(&qn(&t.schema, &name)),
                        qn(&t.schema, &t.name),
                        q(&name),
                        q(&c.name)
                    ));
                }
            }
        }
    }
    let late: HashSet<i32> = prefix.values().flatten().copied().filter(|id| !early.contains(id)).collect();
    let order = module_order(&src.modules, &src.deps);
    for &i in &order {
        let m = &src.modules[i];
        if early.contains(&m.id) {
            before.extend(module_sql(m, &place, &mut notes));
        }
    }

    // ---- tables.
    let mut out_tables = Vec::new();
    for t in &picked {
        let mut create: Vec<String> = Vec::new();
        if let Some(mods) = prefix.get(&t.id) {
            for &i in &order {
                let m = &src.modules[i];
                if mods.contains(&m.id) && !early.contains(&m.id) {
                    create.extend(module_sql(m, &place, &mut notes));
                }
            }
        }
        create.push(create_table(t));
        out_tables.push(CloneTable {
            table: ObjectRef { kind: "table".into(), schema: Some(t.schema.clone()), name: t.name.clone() },
            create: create.join("\n"),
            before_data: Vec::new(),
            after_data: after_data(t, &mut notes),
        });
    }

    // ---- after.
    let mut after = Vec::new();
    for sq in &src.sequences {
        if let Some(sql) = sequence_value(sq) {
            if sq.current.trim() != sq.start.trim() {
                notes.push(format!(
                    "{}: la secuencia sigue desde su valor actual como en el origen, pero su START WITH pasa a ser {} \
                     (en el origen {}): solo cambia a qué valor vuelve un RESTART sin valor",
                    qn(&sq.schema, &sq.name),
                    sq.current,
                    sq.start
                ));
            }
            after.push(sql);
        }
        if sq.unknown_use {
            notes.push(format!(
                "{}: el origen es anterior a SQL Server 2017 y no dice si la secuencia ya entregó su valor inicial ({}); \
                 se clona como sin usar: en el destino el próximo valor es {}, y si en el origen ya se usó, allá es el siguiente \
                 ({} más {})",
                qn(&sq.schema, &sq.name),
                sq.start,
                sq.start,
                sq.start,
                sq.increment
            ));
        }
    }
    for (sc, name, base) in &src.synonyms {
        after.push(format!("IF OBJECT_ID({}, 'SN') IS NULL\n    CREATE SYNONYM {} FOR {base};", lit(&qn(sc, name)), qn(sc, name)));
    }
    let mut triggers = Vec::new();
    for &i in &order {
        let m = &src.modules[i];
        if skipped.contains(&m.id) {
            continue;
        }
        if m.kind == "TR" {
            triggers.push(m);
            continue;
        }
        // Created already (before the tables, or right before one).
        if !early.contains(&m.id) && !late.contains(&m.id) {
            after.extend(module_sql(m, &place, &mut notes));
        }
        if m.kind == "V" {
            let mut indexes: Vec<&Index> = src.view_indexes.get(&m.id).into_iter().flatten().collect();
            indexes.sort_by_key(|i| i.rank());
            for i in indexes {
                after.push(create_index(&m.qname(), i));
            }
            for x in src.view_stats.get(&m.id).into_iter().flatten() {
                after.push(statistics(&m.qname(), x));
            }
        }
    }
    for m in triggers {
        after.extend(module_sql(m, &place, &mut notes));
    }
    after.extend(detached_sql);
    // A CHECK over a function that isn't cloned can't be created: left
    // out, saying why.
    for t in &picked {
        for ck in &t.checks {
            let gone = src.deps.get(&ck.id).into_iter().flatten().filter(|x| skipped.contains(x)).min().and_then(|x| module_ids.get(x));
            if let Some(&fi) = gone {
                let f = &src.modules[fi];
                let reason =
                    reason_of(f).unwrap_or_else(|| format!("{}, que no se clona (usa tablas u objetos que no se clonan)", f.qname()));
                notes.push(format!("{}: la restricción CHECK [{}] no se crea porque usa {reason}", t.qname(), ck.name));
                continue;
            }
            after.push(check_constraint(t, ck));
        }
    }
    for t in &picked {
        for fk in &t.fks {
            if !cloned(&fk.ref_schema, &fk.ref_table, &picked) {
                notes.push(format!(
                    "{}: la clave foránea [{}] apunta a {}.{}, que no se clona; no se crea",
                    t.qname(),
                    fk.name,
                    fk.ref_schema,
                    fk.ref_table
                ));
                continue;
            }
            after.push(foreign_key(t, fk));
        }
    }
    // System versioning back on.
    let temporal: Vec<&Table> = picked.iter().filter(|t| t.temporal.is_some()).collect();
    if !temporal.is_empty() && src.history_retention && !target.history_retention {
        after.push("IF EXISTS (SELECT 1 FROM sys.databases WHERE database_id = DB_ID() AND is_temporal_history_retention_enabled = 0)\n    ALTER DATABASE CURRENT SET TEMPORAL_HISTORY_RETENTION ON;".into());
    }
    for t in temporal {
        let Some(tmp) = &t.temporal else { continue };
        if !cloned(&tmp.history_schema, &tmp.history_table, &picked) {
            notes.push(format!(
                "{}: su tabla de historia {}.{} no se clona; se crea vacía al activar el versionado",
                t.qname(),
                tmp.history_schema,
                tmp.history_table
            ));
        }
        after.extend(enable_temporal(t, tmp));
    }
    // A disabled primary key or unique index goes off once the foreign keys
    // that reference it exist (disabling it disables them too, as there).
    for t in &picked {
        after.extend(deferred_disables(t));
    }
    // Descriptions last: every object they describe exists by now.
    let mut emitted: HashSet<(String, String)> = HashSet::new();
    let key = |a: &str, b: &str| (a.to_lowercase(), b.to_lowercase());
    for t in &picked {
        emitted.insert(key(&t.schema, &t.name));
    }
    for m in src.modules.iter().filter(|m| !skipped.contains(&m.id) && m.kind != "TR") {
        emitted.insert(key(&m.schema, &m.name));
    }
    for (sc, name, _) in &src.synonyms {
        emitted.insert(key(sc, name));
    }
    for sq in &src.sequences {
        emitted.insert(key(&sq.schema, &sq.name));
    }
    for p in &src.properties {
        let object_level = p.levels.len() >= 2 && !matches!(p.levels[1].0.as_str(), "TYPE" | "XML SCHEMA COLLECTION");
        // DBine's own bookkeeping (delta's untrusted-key mark) belongs to
        // the source's sync state, not to its schema.
        if (object_level && !emitted.contains(&key(&p.levels[0].1, &p.levels[1].1))) || p.name.eq_ignore_ascii_case(UNTRUSTED_MARK) {
            continue;
        }
        after.push(extended_property(p));
    }

    // ---- what isn't cloned at all.
    if src.ddl_triggers > 0 {
        notes.push(format!(
            "{} trigger(s) de base (DDL) no se clonan: se dispararían con el propio script al reanudar",
            src.ddl_triggers
        ));
    }
    if !src.clr_objects.is_empty() {
        notes.push(format!("objetos CLR que no se clonan (necesitan su assembly): {}", src.clr_objects.join(", ")));
    }
    if src.principals > 0 {
        notes.push("los usuarios, roles y permisos de la base no forman parte del clon".into());
    }
    let mut seen = HashSet::new();
    notes.retain(|n| seen.insert(n.clone()));
    Ok(CloneScript { before, tables: out_tables, after, notes })
}

fn kind_name(kind: &str) -> &'static str {
    match kind {
        "V" => "vista",
        "P" => "procedimiento",
        "TR" => "trigger",
        _ => "función",
    }
}

/// Modules in dependency order (what a module uses first); ties by kind
/// (functions, views, procedures, triggers) and name. A cycle keeps the
/// source's order for what's left.
fn module_order(modules: &[Module], deps: &HashMap<i32, HashSet<i32>>) -> Vec<usize> {
    let rank = |m: &Module| match m.kind.as_str() {
        "FN" | "IF" | "TF" => 0,
        "V" => 1,
        "P" => 2,
        _ => 3,
    };
    let ids: HashMap<i32, usize> = modules.iter().enumerate().map(|(i, m)| (m.id, i)).collect();
    let mut sorted: Vec<usize> = (0..modules.len()).collect();
    sorted.sort_by(|&a, &b| {
        (rank(&modules[a]), modules[a].schema.to_lowercase(), modules[a].name.to_lowercase()).cmp(&(
            rank(&modules[b]),
            modules[b].schema.to_lowercase(),
            modules[b].name.to_lowercase(),
        ))
    });
    // A trigger also waits for its parent (a view with INSTEAD OF).
    let needs = |i: usize| -> HashSet<usize> {
        let m = &modules[i];
        let mut v: HashSet<usize> = deps.get(&m.id).into_iter().flatten().filter_map(|d| ids.get(d).copied()).filter(|&d| d != i).collect();
        if let Some((p, _, _)) = &m.parent {
            v.extend(ids.get(p).copied());
        }
        v
    };
    // Kahn's algorithm, the ready module that sorts first going next.
    let pos: Vec<usize> = {
        let mut p = vec![0; modules.len()];
        for (k, &i) in sorted.iter().enumerate() {
            p[i] = k;
        }
        p
    };
    let mut waiting: Vec<usize> = vec![0; modules.len()];
    let mut users: Vec<Vec<usize>> = vec![Vec::new(); modules.len()];
    for (i, w) in waiting.iter_mut().enumerate() {
        for d in needs(i) {
            *w += 1;
            users[d].push(i);
        }
    }
    let mut ready: std::collections::BinaryHeap<std::cmp::Reverse<(usize, usize)>> =
        (0..modules.len()).filter(|&i| waiting[i] == 0).map(|i| std::cmp::Reverse((pos[i], i))).collect();
    let mut done = vec![false; modules.len()];
    let mut out = Vec::with_capacity(modules.len());
    while let Some(std::cmp::Reverse((_, i))) = ready.pop() {
        done[i] = true;
        out.push(i);
        for &u in &users[i] {
            waiting[u] -= 1;
            if waiting[u] == 0 {
                ready.push(std::cmp::Reverse((pos[u], u)));
            }
        }
    }
    out.extend(sorted.into_iter().filter(|&i| !done[i]));
    out
}

/// Every data space a (disk-based) table's storage names.
fn spaces(t: &Table) -> impl Iterator<Item = &Space> {
    t.space.iter().chain(t.pk.iter().flat_map(|p| p.space.iter())).chain(t.indexes.iter().flat_map(|i| i.space.iter()))
}

/// Memory-optimized tables need In-Memory OLTP on the target; without it
/// they're created as regular tables, their hash indexes as regular
/// nonclustered ones.
fn adapt_memory_optimized(tables: &mut [Table], target: &Target, notes: &mut Vec<String>) {
    if target.caps.xtp && target.caps.memory_optimized {
        return;
    }
    let mut moved = Vec::new();
    for t in tables.iter_mut().filter(|t| t.memory.is_some()) {
        t.memory = None;
        t.space = None;
        for i in t.pk.iter_mut().chain(t.indexes.iter_mut()) {
            i.space = None;
            if i.kind == 7 {
                i.kind = 2;
                i.bucket_count = None;
                i.keys = i.keys.iter().map(|(c, _)| (c.clone(), false)).collect();
            }
        }
        moved.push(t.qname());
    }
    if !moved.is_empty() {
        notes.push(format!(
            "el destino no soporta In-Memory OLTP: {} se crean como tablas comunes (los índices hash como índices comunes)",
            moved.join(", ")
        ));
    }
}

/// Options newer than the target: dropped, and said.
fn adapt_features(
    tables: &mut [Table],
    view_indexes: &mut HashMap<i32, Vec<Index>>,
    view_names: &HashMap<i32, String>,
    target: &Target,
    notes: &mut Vec<String>,
) {
    let tc = &target.caps;
    let mut ordered = Vec::new();
    let mut ordered_nc = Vec::new();
    let mut seqkey = Vec::new();
    let mut delay = Vec::new();
    let mut fix = |owner: &str, i: &mut Index| {
        if !tc.ordered_columnstore && i.kind == 5 && !i.columns.is_empty() {
            i.columns.clear();
            ordered.push(format!("{owner}.{}", i.name));
        }
        if !tc.ordered_nonclustered_columnstore && i.kind == 6 && !i.cs_order.is_empty() {
            i.cs_order.clear();
            ordered_nc.push(format!("{owner}.{}", i.name));
        }
        if !tc.sequential_key && i.sequential_key {
            i.sequential_key = false;
            seqkey.push(format!("{owner}.{}", i.name));
        }
        if !tc.compression_delay && i.compression_delay > 0 {
            i.compression_delay = 0;
            delay.push(format!("{owner}.{}", i.name));
        }
    };
    for t in tables.iter_mut() {
        let owner = t.qname();
        for i in t.pk.iter_mut().chain(t.indexes.iter_mut()) {
            fix(&owner, i);
        }
    }
    for (id, list) in view_indexes.iter_mut() {
        for i in list {
            fix(view_names.get(id).map_or("vista", String::as_str), i);
        }
    }
    if !ordered.is_empty() {
        notes.push(format!("el destino no tiene columnstore ordenado (SQL Server 2022): sin ORDER en {}", ordered.join(", ")));
    }
    if !ordered_nc.is_empty() {
        notes.push(format!(
            "el destino no tiene columnstore no agrupado ordenado (SQL Server 2025, Azure SQL Database): sin ORDER en {}",
            ordered_nc.join(", ")
        ));
    }
    if !seqkey.is_empty() {
        notes.push(format!("el destino no tiene OPTIMIZE_FOR_SEQUENTIAL_KEY (SQL Server 2019): se omite en {}", seqkey.join(", ")));
    }
    if !delay.is_empty() {
        notes.push(format!("el destino no tiene COMPRESSION_DELAY: se omite en {}", delay.join(", ")));
    }
    let mut no_temporal = Vec::new();
    let mut no_retention = Vec::new();
    for t in tables.iter_mut() {
        if t.temporal.is_some() && !tc.temporal {
            t.temporal = None;
            no_temporal.push(t.qname());
        }
        if let Some(tmp) = &mut t.temporal {
            if tmp.retention.is_some() && !tc.retention {
                tmp.retention = None;
                no_retention.push(t.qname());
            }
        }
    }
    if !no_temporal.is_empty() {
        notes.push(format!(
            "el destino no tiene tablas temporales: {} quedan como tablas comunes, sin versionado",
            no_temporal.join(", ")
        ));
    }
    if !no_retention.is_empty() {
        notes.push(format!("el destino no tiene retención de historia: {} se versionan sin HISTORY_RETENTION_PERIOD", no_retention.join(", ")));
    }
}

/// Filegroups the cloned storage uses and the target lacks. Azure SQL
/// Database only has PRIMARY: there everything goes to PRIMARY
/// (explicitly: an index left without ON on a partitioned table would
/// follow its scheme) and the partitioning is kept. Elsewhere they're
/// created (returned). A memory-optimized table lives in its own
/// filegroup and isn't looked at.
fn adapt_filegroups(
    tables: &mut [Table],
    view_indexes: &mut HashMap<i32, Vec<Index>>,
    schemes: &mut [PartitionScheme],
    target: &Target,
    notes: &mut Vec<String>,
) -> Vec<String> {
    let exists = |fg: &str| target.filegroups.iter().any(|d| d.eq_ignore_ascii_case(fg));
    // Filegroups and partition schemes share one namespace: a scheme's name
    // is never a filegroup to create (it'd take the name from the scheme).
    let scheme_names: Vec<String> = schemes.iter().map(|ps| ps.name.clone()).collect();
    let is_scheme = |n: &str| scheme_names.iter().any(|s| s.eq_ignore_ascii_case(n));
    let mut missing: Vec<String> = Vec::new();
    let mut add = |fg: &str| {
        if !exists(fg) && !is_scheme(fg) && !missing.iter().any(|m| m.eq_ignore_ascii_case(fg)) {
            missing.push(fg.to_string());
        }
    };
    for ps in schemes.iter() {
        ps.filegroups.iter().for_each(|g| add(g));
    }
    for t in tables.iter().filter(|t| t.memory.is_none()) {
        for sp in spaces(t).filter(|sp| sp.column.is_none()) {
            add(&sp.name);
        }
        if let Some(l) = &t.lob_space {
            add(l);
        }
    }
    for i in view_indexes.values().flatten() {
        if let Some(sp) = i.space.as_ref().filter(|sp| sp.column.is_none()) {
            add(&sp.name);
        }
    }
    if missing.is_empty() || !target.caps.azure_db() {
        return missing;
    }
    let gone = |fg: &str| missing.iter().any(|m| m.eq_ignore_ascii_case(fg));
    let mut affected = Vec::new();
    for ps in schemes.iter_mut() {
        let mut moved = false;
        for fg in &mut ps.filegroups {
            if gone(fg) {
                *fg = "PRIMARY".into();
                moved = true;
            }
        }
        if moved {
            affected.push(format!("esquema {}", ps.name));
        }
    }
    let to_primary = |sp: &mut Option<Space>, what: String, affected: &mut Vec<String>| {
        if let Some(d) = sp.as_mut().filter(|d| d.column.is_none() && gone(&d.name)) {
            d.name = "PRIMARY".into();
            affected.push(what);
        }
    };
    for t in tables.iter_mut().filter(|t| t.memory.is_none()) {
        let owner = t.qname();
        to_primary(&mut t.space, owner.clone(), &mut affected);
        for i in t.pk.iter_mut().chain(t.indexes.iter_mut()) {
            to_primary(&mut i.space, format!("{owner}.{}", i.name), &mut affected);
        }
        if t.lob_space.as_deref().is_some_and(gone) {
            t.lob_space = Some("PRIMARY".into());
            affected.push(format!("{owner} (datos LOB)"));
        }
    }
    for i in view_indexes.values_mut().flatten() {
        let name = i.name.clone();
        to_primary(&mut i.space, name, &mut affected);
    }
    notes.push(format!(
        "Azure SQL Database solo tiene el filegroup PRIMARY: {} no existen en el destino y lo que estaba ahí va a PRIMARY \
         (el particionado se conserva); afectados: {}",
        missing.join(", "),
        affected.join(", ")
    ));
    Vec::new()
}

/// ADD FILEGROUP plus one file next to the database's data file (or in the
/// instance's default data folder), uniquely named; Managed Instance
/// places the file itself. If the file can't be added the filegroup is
/// removed again.
fn create_filegroup(name: &str) -> String {
    add_filegroup(name, "", ".ndf")
}

/// The MEMORY_OPTIMIZED_DATA filegroup with its container (a folder).
fn create_memory_filegroup(name: &str) -> String {
    add_filegroup(name, " CONTAINS MEMORY_OPTIMIZED_DATA", "")
}

fn add_filegroup(name: &str, contains: &str, ext: &str) -> String {
    let fg = name.replace('\'', "''");
    let guard = if contains.is_empty() { format!("name = N'{fg}'") } else { "type = 'FX'".into() };
    format!(
        "IF NOT EXISTS (SELECT 1 FROM sys.filegroups WHERE {guard})
BEGIN
    DECLARE @fg sysname = N'{fg}';
    DECLARE @edition int = CAST(SERVERPROPERTY('EngineEdition') AS int);
    DECLARE @db sysname = DB_NAME();
    DECLARE @logical sysname = LEFT(@db + N'_' + @fg, 100) + N'_' + LEFT(REPLACE(CAST(NEWID() AS nvarchar(36)), N'-', N''), 8);
    DECLARE @sql nvarchar(max) = N'ALTER DATABASE ' + QUOTENAME(@db) + N' ADD FILEGROUP ' + QUOTENAME(@fg) + N'{contains};';
    EXEC sp_executesql @sql;
    BEGIN TRY
        IF @edition = 8
            SET @sql = N'ALTER DATABASE ' + QUOTENAME(@db) + N' ADD FILE (NAME = ' + QUOTENAME(@logical) + N') TO FILEGROUP ' + QUOTENAME(@fg) + N';';
        ELSE
        BEGIN
            DECLARE @p nvarchar(4000) = (SELECT TOP 1 physical_name FROM sys.database_files WHERE type = 0 ORDER BY file_id);
            DECLARE @dir nvarchar(4000) = CASE
                WHEN @p IS NOT NULL THEN LEFT(@p, LEN(@p) - PATINDEX(N'%[\\/]%', REVERSE(@p)) + 1)
                ELSE CAST(SERVERPROPERTY('InstanceDefaultDataPath') AS nvarchar(4000)) END;
            SET @sql = N'ALTER DATABASE ' + QUOTENAME(@db) + N' ADD FILE (NAME = ' + QUOTENAME(@logical)
                     + N', FILENAME = N''' + REPLACE(@dir + @logical + N'{ext}', N'''', N'''''') + N''') TO FILEGROUP ' + QUOTENAME(@fg) + N';';
        END
        EXEC sp_executesql @sql;
    END TRY
    BEGIN CATCH
        SET @sql = N'ALTER DATABASE ' + QUOTENAME(@db) + N' REMOVE FILEGROUP ' + QUOTENAME(@fg) + N';';
        EXEC sp_executesql @sql;
        THROW;
    END CATCH
END"
    )
}

fn partition_function(f: &PartitionFunction) -> String {
    format!(
        "IF NOT EXISTS (SELECT 1 FROM sys.partition_functions WHERE name = {})\n    CREATE PARTITION FUNCTION {} ({}) AS RANGE {} FOR VALUES ({});",
        lit(&f.name),
        q(&f.name),
        type_sql(&f.param),
        if f.range_right { "RIGHT" } else { "LEFT" },
        f.boundaries.join(", "),
    )
}

/// With each partition's filegroup and the NEXT USED one (listed last).
/// A filegroup with the scheme's name (same namespace) stops it with a
/// clear message instead of SQL Server's "already an object named".
fn partition_scheme(ps: &PartitionScheme) -> String {
    let clash = format!("Ya hay un filegroup llamado {} en el destino: no se puede crear el esquema de partición con ese nombre.", ps.name);
    format!(
        "IF NOT EXISTS (SELECT 1 FROM sys.partition_schemes WHERE name = {name})\nBEGIN\n    IF EXISTS (SELECT 1 FROM sys.data_spaces WHERE name = {name})\n        THROW 50000, {clash}, 1;\n    CREATE PARTITION SCHEME {} AS PARTITION {} TO ({});\nEND",
        q(&ps.name),
        q(&ps.function),
        ps.filegroups.iter().map(|g| q(g)).collect::<Vec<_>>().join(", "),
        name = lit(&ps.name),
        clash = lit(&clash),
    )
}

/// The system type with its length / precision / scale.
fn type_sql(c: &Column) -> String {
    let t = c.base.to_ascii_lowercase();
    let len = |per_char: i32| match c.max_length {
        -1 => "max".to_string(),
        n if n > 0 => (n / per_char).to_string(),
        _ => "1".to_string(),
    };
    match t.as_str() {
        "char" | "varchar" | "binary" | "varbinary" => format!("{t}({})", len(1)),
        "nchar" | "nvarchar" => format!("{t}({})", len(2)),
        "decimal" | "numeric" => format!("{t}({}, {})", c.precision, c.scale),
        "datetime2" | "datetimeoffset" | "time" => format!("{t}({})", c.scale),
        "float" if c.precision > 0 && c.precision != 53 => format!("float({})", c.precision),
        _ => t,
    }
}

/// The column's declared type: its alias or CLR type as on the source,
/// typed XML with its collection.
fn declared_type(c: &Column) -> String {
    if c.assembly {
        return if c.user_defined { qn(&c.type_schema, &c.type_name) } else { c.type_name.clone() };
    }
    if c.alias() {
        return if c.user_defined { qn(&c.type_schema, &c.type_name) } else { c.type_name.clone() };
    }
    if let Some((sc, name, document)) = &c.xml_schema {
        return format!("xml({} {})", if *document { "DOCUMENT" } else { "CONTENT" }, qn(sc, name));
    }
    type_sql(c)
}

fn column_sql(c: &Column) -> String {
    if let Some((def, persisted)) = &c.computed {
        let mut s = format!("{} AS {def}", q(&c.name));
        if *persisted {
            s.push_str(" PERSISTED");
            if !c.nullable {
                s.push_str(" NOT NULL");
            }
        }
        return s;
    }
    if c.column_set {
        return format!("{} xml COLUMN_SET FOR ALL_SPARSE_COLUMNS", q(&c.name));
    }
    let mut s = format!("{} {}", q(&c.name), declared_type(c));
    if c.filestream {
        s.push_str(" FILESTREAM");
    }
    if let (Some(coll), false) = (c.collation.as_deref(), c.alias()) {
        if coll.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_') {
            s.push_str(&format!(" COLLATE {coll}"));
        }
    }
    if c.sparse {
        s.push_str(" SPARSE");
    }
    if let Some(m) = &c.masked {
        s.push_str(&format!(" MASKED WITH (FUNCTION = '{}')", m.replace('\'', "''")));
    }
    if let Some(id) = &c.identity {
        let num = |v: &str| {
            let v = v.trim();
            if !v.is_empty() && v.chars().all(|ch| ch.is_ascii_digit() || ch == '-' || ch == '.') {
                v.to_string()
            } else {
                "1".into()
            }
        };
        s.push_str(&format!(" IDENTITY({}, {})", num(&id.seed), num(&id.increment)));
        if id.not_for_replication {
            s.push_str(" NOT FOR REPLICATION");
        }
    }
    if c.rowguidcol {
        s.push_str(" ROWGUIDCOL");
    }
    s.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
    if let Some((name, def)) = &c.default {
        s.push_str(&format!(" CONSTRAINT {} DEFAULT {def}", q(name)));
    }
    s
}

/// ` ON [scheme]([column])` or ` ON [filegroup]`.
fn on_clause(sp: Option<&Space>) -> String {
    match sp {
        Some(Space { name, column: Some(c) }) => format!(" ON {}({})", q(name), q(c)),
        Some(Space { name, column: None }) => format!(" ON {}", q(name)),
        None => String::new(),
    }
}

fn key_list(i: &Index) -> String {
    i.keys.iter().map(|(c, d)| format!("{} {}", q(c), if *d { "DESC" } else { "ASC" })).collect::<Vec<_>>().join(", ")
}

fn plain_list(cols: &[String]) -> String {
    cols.iter().map(|c| q(c)).collect::<Vec<_>>().join(", ")
}

/// `DATA_COMPRESSION = X ON PARTITIONS (…)` per compression, when the
/// partitions differ.
fn compression_on_partitions(parts: &[(i32, String)]) -> Vec<String> {
    let mut groups: Vec<(String, Vec<i32>)> = Vec::new();
    for (n, c) in parts {
        let c = c.to_ascii_uppercase();
        match groups.iter_mut().find(|(g, _)| *g == c) {
            Some((_, ns)) => ns.push(*n),
            None => groups.push((c, vec![*n])),
        }
    }
    groups
        .into_iter()
        .map(|(c, ns)| format!("DATA_COMPRESSION = {c} ON PARTITIONS ({})", ns.iter().map(i32::to_string).collect::<Vec<_>>().join(", ")))
        .collect()
}

/// Rowstore compression options.
fn rowstore_compression(c: &Compression) -> Vec<String> {
    match &c.all {
        Some(x) if x.eq_ignore_ascii_case("ROW") || x.eq_ignore_ascii_case("PAGE") => vec![format!("DATA_COMPRESSION = {}", x.to_ascii_uppercase())],
        Some(_) => Vec::new(),
        None => compression_on_partitions(&c.parts),
    }
}

/// The rowstore options that differ from the defaults.
fn index_options(i: &Index, compression: bool) -> Vec<String> {
    let mut w = Vec::new();
    if i.pad_index {
        w.push("PAD_INDEX = ON".to_string());
    }
    if i.fill_factor > 0 && i.fill_factor < 100 {
        w.push(format!("FILLFACTOR = {}", i.fill_factor));
    }
    if i.ignore_dup_key {
        w.push("IGNORE_DUP_KEY = ON".into());
    }
    if i.no_recompute {
        w.push("STATISTICS_NORECOMPUTE = ON".into());
    }
    if !i.row_locks {
        w.push("ALLOW_ROW_LOCKS = OFF".into());
    }
    if !i.page_locks {
        w.push("ALLOW_PAGE_LOCKS = OFF".into());
    }
    if i.sequential_key {
        w.push("OPTIMIZE_FOR_SEQUENTIAL_KEY = ON".into());
    }
    if compression {
        w.extend(rowstore_compression(&i.compression));
    }
    w
}

fn with(w: Vec<String>) -> String {
    if w.is_empty() {
        String::new()
    } else {
        format!(" WITH ({})", w.join(", "))
    }
}

/// An index of a memory-optimized table, declared in its CREATE TABLE.
fn memory_index(i: &Index) -> String {
    match i.bucket_count {
        Some(n) if i.kind == 7 => {
            format!("NONCLUSTERED HASH ({}) WITH (BUCKET_COUNT = {n})", plain_list(&i.keys.iter().map(|(c, _)| c.clone()).collect::<Vec<_>>()))
        }
        _ => format!("NONCLUSTERED ({})", key_list(i)),
    }
}

fn clustering(i: &Index) -> &'static str {
    if i.kind == 1 {
        "CLUSTERED"
    } else {
        "NONCLUSTERED"
    }
}

/// The table with its columns, primary key and storage; a memory-optimized
/// one with all its indexes; a system-versioned one as a plain table (its
/// period columns are ordinary ones until the versioning goes back on).
pub(crate) fn create_table(t: &Table) -> String {
    let mut parts: Vec<String> = t.columns.iter().map(column_sql).collect();
    let guard = format!("IF OBJECT_ID({}, 'U') IS NULL\n", lit(&t.qname()));
    if let Some(durability) = &t.memory {
        if let Some(pk) = &t.pk {
            parts.push(format!("CONSTRAINT {} PRIMARY KEY {}", q(&pk.name), memory_index(pk)));
        }
        for i in &t.indexes {
            parts.push(if i.unique_constraint {
                format!("CONSTRAINT {} UNIQUE {}", q(&i.name), memory_index(i))
            } else if i.kind == 5 {
                format!("INDEX {} CLUSTERED COLUMNSTORE", q(&i.name))
            } else {
                format!("INDEX {} {}{}", q(&i.name), if i.unique { "UNIQUE " } else { "" }, memory_index(i))
            });
        }
        return format!(
            "{guard}CREATE TABLE {} (\n    {}\n) WITH (MEMORY_OPTIMIZED = ON, DURABILITY = {});",
            t.qname(),
            parts.join(",\n    "),
            durability.to_ascii_uppercase()
        );
    }
    if let Some(pk) = &t.pk {
        parts.push(format!(
            "CONSTRAINT {} PRIMARY KEY {} ({}){}{}",
            q(&pk.name),
            clustering(pk),
            key_list(pk),
            with(index_options(pk, true)),
            on_clause(pk.space.as_ref()),
        ));
    }
    let mut storage = on_clause(t.space.as_ref());
    // TEXTIMAGE_ON only on a filegroup and with LOB columns: the server
    // keeps a table's LOB filegroup after its last LOB column is dropped,
    // but refuses the clause without one.
    if let (Some(lob), Some(Space { column: None, .. }), true) = (&t.lob_space, &t.space, t.columns.iter().any(|c| c.computed.is_none() && c.is_lob())) {
        storage.push_str(&format!(" TEXTIMAGE_ON {}", q(lob)));
    }
    // A heap's own compression (a clustered table's is its index's).
    let heap = rowstore_compression(&t.heap_compression);
    format!("{guard}CREATE TABLE {} (\n    {}\n){storage}{};", t.qname(), parts.join(",\n    "), with(heap))
}

/// `CREATE … INDEX` (or the UNIQUE constraint) on `owner`, idempotent.
pub(crate) fn create_index(owner: &str, i: &Index) -> String {
    let name = q(&i.name);
    let filter = i.filter.as_deref().map(|f| format!(" WHERE {f}")).unwrap_or_default();
    let on = on_clause(i.space.as_ref());
    let create = match i.kind {
        5 | 6 => {
            let mut w = Vec::new();
            match &i.compression.all {
                Some(c) if c.eq_ignore_ascii_case("COLUMNSTORE_ARCHIVE") => w.push("DATA_COMPRESSION = COLUMNSTORE_ARCHIVE".into()),
                Some(_) => {}
                None => w.extend(compression_on_partitions(&i.compression.parts)),
            }
            if i.compression_delay > 0 {
                w.push(format!("COMPRESSION_DELAY = {}", i.compression_delay));
            }
            if i.kind == 5 {
                let order = if i.columns.is_empty() { String::new() } else { format!(" ORDER ({})", plain_list(&i.columns)) };
                format!("CREATE CLUSTERED COLUMNSTORE INDEX {name} ON {owner}{order}{}{on}", with(w))
            } else {
                let order = if i.cs_order.is_empty() { String::new() } else { format!(" ORDER ({})", plain_list(&i.cs_order)) };
                format!("CREATE NONCLUSTERED COLUMNSTORE INDEX {name} ON {owner} ({}){order}{filter}{}{on}", plain_list(&i.columns), with(w))
            }
        }
        3 => {
            let col = i.keys.first().map(|(c, _)| q(c)).unwrap_or_default();
            let w = with(index_options(i, false));
            match (&i.xml_using, &i.xml_for) {
                (Some(primary), Some(kind)) => {
                    format!("CREATE XML INDEX {name} ON {owner} ({col}) USING XML INDEX {} FOR {}{w}", q(primary), kind.to_ascii_uppercase())
                }
                _ => format!("CREATE PRIMARY XML INDEX {name} ON {owner} ({col}){w}"),
            }
        }
        4 => {
            let sp = i.spatial.clone().unwrap_or_default();
            let scheme = sp.scheme.to_ascii_uppercase();
            let mut w = Vec::new();
            if scheme.starts_with("GEOMETRY") {
                if let Some([x0, y0, x1, y1]) = sp.bounding_box {
                    w.push(format!("BOUNDING_BOX = ({x0}, {y0}, {x1}, {y1})"));
                }
            }
            if !scheme.contains("AUTO") {
                if let Some([l1, l2, l3, l4]) = &sp.grids {
                    w.push(format!("GRIDS = (LEVEL_1 = {l1}, LEVEL_2 = {l2}, LEVEL_3 = {l3}, LEVEL_4 = {l4})"));
                }
            }
            if let Some(n) = sp.cells_per_object {
                w.push(format!("CELLS_PER_OBJECT = {n}"));
            }
            w.extend(index_options(i, true));
            // A spatial index sits on a filegroup, never on a scheme.
            let on = match &i.space {
                Some(s @ Space { column: None, .. }) => on_clause(Some(s)),
                _ => String::new(),
            };
            let col = i.keys.first().map(|(c, _)| q(c)).unwrap_or_default();
            format!("CREATE SPATIAL INDEX {name} ON {owner} ({col}) USING {scheme}{}{on}", with(w))
        }
        // Only on a memory-optimized table, whose indexes normally come in
        // its CREATE TABLE; added afterwards the only way it allows.
        7 => format!("ALTER TABLE {owner} ADD INDEX {name} {}", memory_index(i)),
        _ if i.unique_constraint => {
            format!("ALTER TABLE {owner} ADD CONSTRAINT {name} UNIQUE {} ({}){}{on}", clustering(i), key_list(i), with(index_options(i, true)))
        }
        _ => {
            let include = if i.include.is_empty() { String::new() } else { format!(" INCLUDE ({})", plain_list(&i.include)) };
            format!(
                "CREATE {}{} INDEX {name} ON {owner} ({}){include}{filter}{}{on}",
                if i.unique { "UNIQUE " } else { "" },
                clustering(i),
                key_list(i),
                with(index_options(i, true)),
            )
        }
    };
    let mut out = format!(
        "{INDEX_SET}\nIF NOT EXISTS (SELECT 1 FROM sys.indexes WHERE object_id = OBJECT_ID({}) AND name = {})\n    {create};",
        lit(owner),
        lit(&i.name)
    );
    if i.disabled {
        out.push('\n');
        out.push_str(&disable_index(owner, &i.name));
    }
    out
}

fn disable_index(owner: &str, name: &str) -> String {
    format!(
        "IF EXISTS (SELECT 1 FROM sys.indexes WHERE object_id = OBJECT_ID({}) AND name = {} AND is_disabled = 0)\n    ALTER INDEX {} ON {owner} DISABLE;",
        lit(owner),
        lit(name),
        q(name)
    )
}

/// A key foreign keys can reference: disabled only after they exist.
fn referenceable(i: &Index) -> bool {
    i.primary || i.unique || i.unique_constraint
}

/// The table's disabled primary key and unique indexes, disabled (they are
/// created enabled so the foreign keys that reference them can be added).
pub(crate) fn deferred_disables(t: &Table) -> Vec<String> {
    if t.memory.is_some() {
        return Vec::new();
    }
    let owner = t.qname();
    t.pk.iter().chain(t.indexes.iter()).filter(|i| i.disabled && referenceable(i)).map(|i| disable_index(&owner, &i.name)).collect()
}

fn statistics(owner: &str, x: &Stat) -> String {
    let filter = x.filter.as_deref().map(|f| format!(" WHERE {f}")).unwrap_or_default();
    let norecompute = if x.no_recompute { " WITH NORECOMPUTE" } else { "" };
    format!(
        "{INDEX_SET}\nIF NOT EXISTS (SELECT 1 FROM sys.stats WHERE object_id = OBJECT_ID({}) AND name = {})\n    CREATE STATISTICS {} ON {owner} ({}){filter}{norecompute};",
        lit(owner),
        lit(&x.name),
        q(&x.name),
        plain_list(&x.columns)
    )
}

/// Right after a table's data: its other indexes (clustered first, primary
/// XML before secondary), its statistics, its identity where the source
/// left it.
pub(crate) fn after_data(t: &Table, notes: &mut Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    let owner = t.qname();
    if t.memory.is_none() {
        let mut indexes: Vec<&Index> = t.indexes.iter().collect();
        indexes.sort_by_key(|i| i.rank());
        for i in indexes {
            if i.disabled && referenceable(i) {
                out.push(create_index(&owner, &Index { disabled: false, ..i.clone() }));
            } else {
                out.push(create_index(&owner, i));
            }
        }
        for x in &t.stats {
            out.push(statistics(&owner, x));
        }
    }
    if let Some(last) = &t.identity_last {
        if t.memory.is_some() {
            if t.identity_max.as_deref() != Some(last.as_str()) {
                notes.push(format!(
                    "{owner}: tabla memory-optimized (DBCC CHECKIDENT no la acepta); su identidad sigue desde el mayor valor copiado \
                     ({}) y no desde {last} como en el origen",
                    t.identity_max.as_deref().unwrap_or("ninguno")
                ));
            }
        } else if let Some(sql) = reseed(&owner, last) {
            out.push(sql);
        }
    }
    out
}

/// `DBCC CHECKIDENT … RESEED, v` makes the next value `v + increment` on a
/// table that got rows since it was created, but `v` itself on one that
/// didn't: an empty clone is reseeded one increment ahead. Deciding by
/// whether it has rows keeps it right when run again.
pub(crate) fn reseed(owner: &str, last: &str) -> Option<String> {
    let last = last.trim();
    if last.is_empty() || !last.chars().all(|c| c.is_ascii_digit() || c == '-') {
        return None;
    }
    let l = lit(owner);
    Some(format!(
        "DECLARE @v decimal(38, 0) = {last};\nIF NOT EXISTS (SELECT 1 FROM {owner}) SET @v = @v + IDENT_INCR({l});\nDBCC CHECKIDENT({l}, RESEED, @v) WITH NO_INFOMSGS;"
    ))
}

fn fk_action(what: &str, desc: &str) -> String {
    match desc.to_ascii_uppercase().as_str() {
        "CASCADE" => format!(" ON {what} CASCADE"),
        "SET_NULL" => format!(" ON {what} SET NULL"),
        "SET_DEFAULT" => format!(" ON {what} SET DEFAULT"),
        _ => String::new(),
    }
}

/// Trusted on the source → validated here (WITH CHECK); untrusted there →
/// added WITH NOCHECK; disabled there → disabled here too.
pub(crate) fn foreign_key(t: &Table, fk: &ForeignKey) -> String {
    let owner = t.qname();
    let check = if fk.not_trusted || fk.disabled { "NOCHECK" } else { "CHECK" };
    let disable = if fk.disabled { format!("\n    ALTER TABLE {owner} NOCHECK CONSTRAINT {};", q(&fk.name)) } else { String::new() };
    format!(
        "IF OBJECT_ID({}, 'F') IS NULL\nBEGIN\n    ALTER TABLE {owner} WITH {check} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}){}{}{};{disable}\nEND",
        lit(&qn(&t.schema, &fk.name)),
        q(&fk.name),
        plain_list(&fk.columns),
        qn(&fk.ref_schema, &fk.ref_table),
        plain_list(&fk.ref_columns),
        fk_action("DELETE", &fk.on_delete),
        fk_action("UPDATE", &fk.on_update),
        if fk.not_for_replication { " NOT FOR REPLICATION" } else { "" },
    )
}

pub(crate) fn check_constraint(t: &Table, ck: &Check) -> String {
    let owner = t.qname();
    let check = if ck.not_trusted || ck.disabled { "NOCHECK" } else { "CHECK" };
    let disable = if ck.disabled { format!("\n    ALTER TABLE {owner} NOCHECK CONSTRAINT {};", q(&ck.name)) } else { String::new() };
    format!(
        "IF OBJECT_ID({}, 'C') IS NULL\nBEGIN\n    ALTER TABLE {owner} WITH {check} ADD CONSTRAINT {} CHECK {}{};{disable}\nEND",
        lit(&qn(&t.schema, &ck.name)),
        q(&ck.name),
        if ck.not_for_replication { "NOT FOR REPLICATION " } else { "" },
        ck.definition,
    )
}

/// Period, hidden period columns and versioning on its history table with
/// the same retention; each its own batch, each skipped when in place.
pub(crate) fn enable_temporal(t: &Table, tmp: &Temporal) -> Vec<String> {
    let owner = t.qname();
    let l = lit(&owner);
    // Through EXEC: the server checks an ALTER TABLE on the period when it
    // compiles the batch, even under an IF that skips it.
    let mut out = vec![format!(
        "IF NOT EXISTS (SELECT 1 FROM sys.periods WHERE object_id = OBJECT_ID({l}))\n    EXEC({});",
        lit(&format!("ALTER TABLE {owner} ADD PERIOD FOR SYSTEM_TIME ({}, {})", q(&tmp.start), q(&tmp.end)))
    )];
    for c in t.columns.iter().filter(|c| c.hidden) {
        out.push(format!(
            "IF COLUMNPROPERTY(OBJECT_ID({l}), {}, 'IsHidden') = 0\n    EXEC({});",
            lit(&c.name),
            lit(&format!("ALTER TABLE {owner} ALTER COLUMN {} ADD HIDDEN", q(&c.name)))
        ));
    }
    let retention = tmp.retention.as_deref().map(|r| format!(", HISTORY_RETENTION_PERIOD = {r}")).unwrap_or_default();
    out.push(format!(
        "IF OBJECTPROPERTY(OBJECT_ID({l}), 'TableTemporalType') <> 2\n    EXEC({});",
        lit(&format!(
            "ALTER TABLE {owner} SET (SYSTEM_VERSIONING = ON (HISTORY_TABLE = {}, DATA_CONSISTENCY_CHECK = ON{retention}))",
            qn(&tmp.history_schema, &tmp.history_table)
        ))
    ));
    out
}

pub(crate) fn user_type(t: &UserType) -> String {
    let name = qn(&t.schema, &t.name);
    let body = match &t.table {
        Some(tt) => {
            // Table types take unnamed constraints and defaults.
            let mut parts: Vec<String> = tt.columns.iter().map(|c| column_sql(&Column { default: None, ..c.clone() })).collect();
            for c in &tt.columns {
                if let Some((_, def)) = &c.default {
                    // Put back unnamed, right after the column.
                    if let Some(p) = parts.iter_mut().find(|p| p.starts_with(&format!("{} ", q(&c.name)))) {
                        p.push_str(&format!(" DEFAULT {def}"));
                    }
                }
            }
            for i in &tt.indexes {
                let kind = if i.kind == 7 { "NONCLUSTERED HASH" } else { clustering(i) };
                let keys = if i.kind == 7 { plain_list(&i.keys.iter().map(|(c, _)| c.clone()).collect::<Vec<_>>()) } else { key_list(i) };
                let bucket = i.bucket_count.filter(|_| i.kind == 7).map(|n| format!(" WITH (BUCKET_COUNT = {n})")).unwrap_or_default();
                parts.push(if i.primary {
                    format!("PRIMARY KEY {kind} ({keys}){bucket}")
                } else if i.unique_constraint {
                    format!("UNIQUE {kind} ({keys}){bucket}")
                } else {
                    format!("INDEX {} {}{kind} ({keys}){bucket}", q(&i.name), if i.unique { "UNIQUE " } else { "" })
                });
            }
            for ck in &tt.checks {
                parts.push(format!("CHECK {ck}"));
            }
            let mo = if tt.memory_optimized { " WITH (MEMORY_OPTIMIZED = ON)" } else { "" };
            format!("AS TABLE (\n    {}\n){mo}", parts.join(",\n    "))
        }
        None => format!("FROM {} {}", type_sql(&t.base), if t.base.nullable { "NULL" } else { "NOT NULL" }),
    };
    format!("IF TYPE_ID({}) IS NULL\n    CREATE TYPE {name} {body};", lit(&name))
}

pub(crate) fn sequence(sq: &Sequence) -> String {
    let name = qn(&sq.schema, &sq.name);
    let cache = match (sq.cached, sq.cache_size) {
        (false, _) => "NO CACHE".to_string(),
        (true, Some(n)) => format!("CACHE {n}"),
        (true, None) => "CACHE".into(),
    };
    format!(
        "IF OBJECT_ID({}, 'SO') IS NULL\n    CREATE SEQUENCE {name} AS {} START WITH {} INCREMENT BY {} MINVALUE {} MAXVALUE {} {} {cache};",
        lit(&name),
        sq.type_sql,
        sq.start,
        sq.increment,
        sq.min,
        sq.max,
        if sq.cycle { "CYCLE" } else { "NO CYCLE" },
    )
}

/// Where the source's sequence is: restarted at its current value and that
/// value taken once, so the target hands out the same next value and shows
/// the same current one. `None` when never used (it's as created).
pub(crate) fn sequence_value(sq: &Sequence) -> Option<String> {
    let cur = sq.current.trim();
    if !sq.used || cur.is_empty() || !cur.chars().all(|c| c.is_ascii_digit() || c == '-') {
        return None;
    }
    let name = qn(&sq.schema, &sq.name);
    Some(format!("ALTER SEQUENCE {name} RESTART WITH {cur};\nDECLARE @v sql_variant = NEXT VALUE FOR {name};"))
}

/// The user a module whose stored CREATE names no schema is created as,
/// so it lands in the schema it has on the source (dropped right after).
const CLONE_USER: &str = "dbine_clone_owner";

/// What deciding where a module is created needs.
pub(crate) struct Placement<'a> {
    /// The cloning user's default schema on the target.
    pub default_schema: &'a str,
    /// Every name the source's objects use (schema, name), lowercase.
    pub taken: &'a HashSet<(String, String)>,
}

/// A module as the source has it: its stored definition run verbatim (so
/// `sys.sql_modules` reads the same), under the ANSI_NULLS /
/// QUOTED_IDENTIFIER it had (they are fixed at creation: SET inside a
/// dynamic batch that then runs the CREATE as its own batch, so the options
/// apply and revert by themselves).
///
/// The stored text keeps the name the module was created with. One renamed
/// (sp_rename) or moved (ALTER SCHEMA TRANSFER) since is created under that
/// name and then renamed or moved the same way; if another object has that
/// name now, its CREATE is pointed at the current name instead (said). One
/// whose CREATE names no schema is created as a user whose default schema
/// is the module's, so it lands there and resolves its names as it did. A
/// trigger gets back its disabled state and its order. `None` (said) when it
/// can't be created.
pub(crate) fn module_sql(m: &Module, place: &Placement, notes: &mut Vec<String>) -> Option<String> {
    let on = |x: bool| if x { "ON" } else { "OFF" };
    let mut def = m.definition.clone().unwrap_or_default();
    let header = parse_header(&def);
    let (mut schema, mut name) = match &header {
        Some(h) => (h.schema.clone().unwrap_or_else(|| m.schema.clone()), h.name.clone()),
        None => (m.schema.clone(), m.name.clone()),
    };
    let mut as_user = header.as_ref().is_some_and(|h| h.schema.is_none()) && !m.schema.eq_ignore_ascii_case(place.default_schema);
    let moved = !schema.eq_ignore_ascii_case(&m.schema);
    let renamed = name != m.name;
    if moved || renamed {
        let old = qn(&schema, &name);
        if m.kind == "TR" && moved {
            notes.push(format!(
                "{} (trigger): su definición guardada en el origen lo nombra {old}, en otro esquema (su tabla cambió de esquema); \
                 no se clona",
                m.qname()
            ));
            return None;
        }
        let me = (m.schema.to_lowercase(), m.name.to_lowercase());
        let busy = |sc: &str, nm: &str| {
            let k = (sc.to_lowercase(), nm.to_lowercase());
            k != me && place.taken.contains(&k)
        };
        if busy(&schema, &name) || (moved && renamed && busy(&m.schema, &name)) {
            if let Some(h) = &header {
                def.replace_range(h.start..h.end, &m.qname());
            }
            notes.push(format!(
                "{} ({}): su definición guardada en el origen lo nombra {old} (se renombró o cambió de esquema) y ese nombre \
                 lo usa otro objeto; se crea con la definición apuntando al nombre actual",
                m.qname(),
                kind_name(&m.kind)
            ));
            (schema, name) = (m.schema.clone(), m.name.clone());
            as_user = false;
        }
    }
    // EXECUTE AS SELF binds the module to whoever creates it: the helper
    // user would become its execution context (and couldn't be dropped,
    // staying behind in db_owner). Users aren't cloned, so the source's
    // principal isn't there to create it as either.
    let as_self = executes_as_self(&def);
    if as_user && as_self {
        notes.push(format!(
            "{} ({}): se declara WITH EXECUTE AS SELF y su CREATE no nombra esquema; para crearlo en {} habría que crearlo \
             como un usuario temporal, que quedaría como su contexto de ejecución (en el origen es {}). No se clona: \
             crealo a mano en el destino como el usuario que corresponda",
            m.qname(),
            kind_name(&m.kind),
            q(&m.schema),
            m.execute_as.as_deref().map(q).unwrap_or_else(|| "el usuario que lo creó".into())
        ));
        return None;
    }
    if as_self {
        notes.push(format!(
            "{} ({}): se declara WITH EXECUTE AS SELF, así que en el destino se ejecuta como el usuario con el que se clona \
             (en el origen, como {}); los usuarios no forman parte del clon",
            m.qname(),
            kind_name(&m.kind),
            m.execute_as.as_deref().map(q).unwrap_or_else(|| "el usuario que lo creó".into())
        ));
    }
    let inner = format!(
        "{}SET ANSI_NULLS {}; SET QUOTED_IDENTIFIER {}; EXEC({});",
        if as_user { format!("EXECUTE AS USER = N'{CLONE_USER}'; ") } else { String::new() },
        on(m.ansi_nulls),
        on(m.quoted_identifier),
        lit(&def)
    );
    let mut create = format!("EXEC({});", guard_go(&lit(&inner)));
    if as_user {
        create = format!(
            "IF USER_ID(N'{CLONE_USER}') IS NOT NULL DROP USER [{CLONE_USER}];
CREATE USER [{CLONE_USER}] WITHOUT LOGIN WITH DEFAULT_SCHEMA = {};
ALTER ROLE [db_owner] ADD MEMBER [{CLONE_USER}];
BEGIN TRY
    {create}
END TRY
BEGIN CATCH
    DROP USER [{CLONE_USER}];
    THROW;
END CATCH;
IF EXISTS (SELECT 1 FROM sys.sql_modules WHERE execute_as_principal_id = USER_ID(N'{CLONE_USER}'))
BEGIN
    EXEC({});
    DROP USER [{CLONE_USER}];
    THROW 50000, {}, 1;
END;
DROP USER [{CLONE_USER}];",
            q(&m.schema),
            lit(&format!("DROP {} {}", drop_kind(&m.kind), qn(&m.schema, &name))),
            lit(&format!(
                "{} ({}): quedó ligado al usuario temporal del clon como contexto de ejecución (EXECUTE AS SELF); se descartó",
                m.qname(),
                kind_name(&m.kind)
            ))
        );
    }
    let guard = format!("IF OBJECT_ID({}) IS NULL", lit(&m.qname()));
    let mut out = if !schema.eq_ignore_ascii_case(&m.schema) || name != m.name {
        let mut s = format!("{guard}\nBEGIN\nIF OBJECT_ID({}) IS NULL\nBEGIN\n{create}\nEND", lit(&qn(&schema, &name)));
        if !schema.eq_ignore_ascii_case(&m.schema) {
            s.push_str(&format!("\nEXEC({});", lit(&format!("ALTER SCHEMA {} TRANSFER {}", q(&m.schema), qn(&schema, &name)))));
        }
        if name != m.name {
            s.push_str(&format!(
                "\nEXEC sp_rename @objname = {}, @newname = {}, @objtype = N'OBJECT';",
                lit(&qn(&m.schema, &name)),
                lit(&m.name)
            ));
        }
        s.push_str("\nEND");
        s
    } else if as_user {
        format!("{guard}\nBEGIN\n{create}\nEND")
    } else {
        format!("{guard}\n    {create}")
    };
    if let Some((_, ps, pn)) = &m.parent {
        if m.disabled {
            out.push_str(&format!("\nDISABLE TRIGGER {} ON {};", m.qname(), qn(ps, pn)));
        }
        for (order, stmt) in &m.orders {
            out.push_str(&format!(
                "\nEXEC sp_settriggerorder @triggername = {}, @order = N'{order}', @stmttype = N'{stmt}';",
                lit(&m.qname())
            ));
        }
    }
    Some(out)
}

/// `DROP` keyword for a module kind.
fn drop_kind(kind: &str) -> &'static str {
    match kind {
        "V" => "VIEW",
        "P" => "PROCEDURE",
        "TR" => "TRIGGER",
        _ => "FUNCTION",
    }
}

/// The database user a module is bound to with `WITH EXECUTE AS 'user'`
/// (a principal that isn't SELF: SELF is the creator, handled apart;
/// CALLER and OWNER bind to none), as its text names it: the CREATE runs
/// that text, so that's the name the target must have (the catalog's name
/// differs when the user was renamed since). Users aren't cloned: the
/// module can only be created where that user exists
/// ([`execute_as_problem`]).
pub(crate) fn executes_as_named(m: &Module) -> Option<String> {
    let catalog = m.execute_as.as_deref()?;
    match execute_as_clause(m.definition.as_deref()?) {
        Some(ExecAs::SelfUser) => None,
        Some(ExecAs::User(user)) => Some(user),
        None => Some(catalog.to_string()),
    }
}

/// Why a module whose text says `WITH EXECUTE AS 'user'` can't be created
/// on the target (`None`: it can).
fn execute_as_problem(m: &Module, user: &str, target: &Target) -> Option<String> {
    if m.execute_as_renamed {
        let now = m.execute_as.as_deref().unwrap_or_default();
        return Some(format!(
            "su texto nombra al usuario «{user}», que en el origen ahora se llama «{now}»; \
             actualizá el módulo en el origen para que nombre a «{now}» y volvé a clonar"
        ));
    }
    if target_has_user(target, user) {
        return None;
    }
    Some(match target.users_spelled.get(user) {
        Some(there) => format!(
            "el usuario «{user}» de EXECUTE AS no existe en el destino, que distingue mayúsculas y minúsculas (ahí está «{there}»); \
             corregí el nombre en el módulo del origen o creá ese usuario, y volvé a clonar"
        ),
        None => format!("el usuario «{user}» de EXECUTE AS no existe en el destino; crealo y volvé a clonar"),
    })
}

/// Whether a `WITH EXECUTE AS 'user'` user exists on the target (dbo is
/// in every database).
fn target_has_user(target: &Target, user: &str) -> bool {
    user == "dbo" || target.users.contains(user)
}

/// `'text'` with its quotes doubled, for notes.
fn lit_plain(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Whether the text declares `WITH EXEC[UTE] AS SELF` (outside comments,
/// strings and quoted names; as a statement `EXECUTE AS` never takes SELF).
pub(crate) fn executes_as_self(def: &str) -> bool {
    matches!(execute_as_clause(def), Some(ExecAs::SelfUser))
}

/// A module's `WITH EXEC[UTE] AS` principal, when it's SELF or a user.
#[derive(Debug, PartialEq)]
pub(crate) enum ExecAs {
    SelfUser,
    User(String),
}

/// The first `EXEC[UTE] AS SELF` or `EXEC[UTE] AS '[N]user'` in the text,
/// outside comments, strings and quoted names: the header's clause, which
/// comes before the body (as a statement, `EXECUTE AS` takes neither).
pub(crate) fn execute_as_clause(def: &str) -> Option<ExecAs> {
    let b = def.as_bytes();
    let (mut i, mut prev) = (0, [String::new(), String::new()]);
    while i < b.len() {
        i = skip_blank(b, i);
        if i >= b.len() {
            break;
        }
        let c = b[i];
        let word = c.is_ascii_alphabetic() || matches!(c, b'_' | b'@' | b'#');
        let mut tok = String::new();
        if word {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || matches!(b[j], b'_' | b'@' | b'#' | b'$')) {
                j += 1;
            }
            tok = String::from_utf8_lossy(&b[i..j]).to_ascii_uppercase();
            let exec_as = prev[1] == "AS" && matches!(prev[0].as_str(), "EXEC" | "EXECUTE");
            if tok == "SELF" && exec_as {
                return Some(ExecAs::SelfUser);
            }
            i = j;
            // N'...': the prefix isn't a word of its own.
            if tok == "N" && b.get(j) == Some(&b'\'') {
                continue;
            }
        } else if matches!(c, b'\'' | b'[' | b'"') {
            let close = if c == b'[' { b']' } else { c };
            let start = i + 1;
            i += 1;
            while i < b.len() {
                if b[i] == close {
                    if b.get(i + 1) == Some(&close) {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            if c == b'\'' && prev[1] == "AS" && matches!(prev[0].as_str(), "EXEC" | "EXECUTE") {
                return Some(ExecAs::User(def[start..i.min(b.len())].replace("''", "'")));
            }
            i += 1;
        } else {
            i += 1;
        }
        prev = [std::mem::take(&mut prev[1]), tok];
    }
    None
}

/// A module where its stored text says, the target's default schema being dbo.
#[cfg(test)]
pub(crate) fn module(m: &Module) -> String {
    module_sql(m, &Placement { default_schema: "dbo", taken: &HashSet::new() }, &mut Vec::new()).unwrap_or_default()
}

/// The name in a stored `CREATE [OR ALTER] VIEW | PROC[EDURE] | FUNCTION |
/// TRIGGER`: where it is in the text (bytes) and its parts, unquoted.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Header {
    pub start: usize,
    pub end: usize,
    pub schema: Option<String>,
    pub name: String,
}

/// Past whitespace and comments (`--`, nested `/* */`).
fn skip_blank(b: &[u8], mut i: usize) -> usize {
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if b[i..].starts_with(b"--") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i..].starts_with(b"/*") {
            let mut depth = 0;
            while i < b.len() {
                if b[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if b[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else {
            return i;
        }
    }
}

/// A keyword at `i`, uppercase, and where it ends.
fn keyword(b: &[u8], i: usize) -> (usize, String) {
    let mut j = i;
    while j < b.len() && b[j].is_ascii_alphabetic() {
        j += 1;
    }
    (j, String::from_utf8_lossy(&b[i..j]).to_ascii_uppercase())
}

/// One identifier part at `i` (`[..]`, `".."` or regular), unquoted, and
/// where it ends.
fn identifier(def: &str, i: usize) -> Option<(usize, String)> {
    let rest = def.get(i..)?;
    let first = rest.chars().next()?;
    if first == '[' || first == '"' {
        let close = if first == '[' { ']' } else { '"' };
        let mut out = String::new();
        let mut it = rest.char_indices().skip(1).peekable();
        while let Some((k, c)) = it.next() {
            if c != close {
                out.push(c);
            } else if it.peek().is_some_and(|&(_, n)| n == close) {
                it.next();
                out.push(close);
            } else {
                return Some((i + k + 1, out));
            }
        }
        return None;
    }
    let regular = |c: char| c.is_alphanumeric() || matches!(c, '_' | '@' | '#' | '$');
    if !regular(first) {
        return None;
    }
    let len = rest.find(|c: char| !regular(c)).unwrap_or(rest.len());
    Some((i + len, rest[..len].to_string()))
}

pub(crate) fn parse_header(def: &str) -> Option<Header> {
    let b = def.as_bytes();
    let (j, w) = keyword(b, skip_blank(b, 0));
    if w != "CREATE" {
        return None;
    }
    let (mut j, mut w) = keyword(b, skip_blank(b, j));
    if w == "OR" {
        let (k, alter) = keyword(b, skip_blank(b, j));
        if alter != "ALTER" {
            return None;
        }
        (j, w) = keyword(b, skip_blank(b, k));
    }
    if !matches!(w.as_str(), "VIEW" | "PROC" | "PROCEDURE" | "FUNCTION" | "TRIGGER") {
        return None;
    }
    let start = skip_blank(b, j);
    let mut i = start;
    let mut parts = Vec::new();
    let end = loop {
        let (k, part) = identifier(def, i)?;
        if part.is_empty() {
            return None;
        }
        parts.push(part);
        let n = skip_blank(b, k);
        if n < b.len() && b[n] == b'.' {
            i = skip_blank(b, n + 1);
        } else {
            break k;
        }
    };
    let mut parts = parts.into_iter().rev();
    let name = parts.next()?;
    let schema = parts.next();
    if parts.nth(1).is_some() {
        return None;
    }
    Some(Header { start, end, schema, name })
}

/// A line reading just `GO` inside a literal would split the batch in a
/// client that splits on it: the literal is cut there and concatenated
/// back (`N'…' + N'GO…'`).
fn guard_go(literal: &str) -> String {
    literal
        .split('\n')
        .map(|line| if line.trim().eq_ignore_ascii_case("go") { format!("' + N'{line}") } else { line.to_string() })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Add the property, or update it when it's there (a resumed clone),
/// keeping the value's type.
pub(crate) fn extended_property(p: &ExtendedProperty) -> String {
    let mut list = Vec::new();
    let mut args = vec![format!("@name = {}", lit(&p.name)), "@value = @v".to_string()];
    for i in 0..3 {
        match p.levels.get(i) {
            Some((t, nm)) => {
                list.push(format!("{}, {}", lit(t), lit(nm)));
                args.push(format!("@level{i}type = {}, @level{i}name = {}", lit(t), lit(nm)));
            }
            None => list.push("NULL, NULL".into()),
        }
    }
    let value = match p.base_type.to_ascii_lowercase().as_str() {
        "varchar" | "char" => format!("CAST({} AS varchar(7500))", lit(&p.value)),
        t @ ("int" | "bigint" | "smallint" | "tinyint" | "bit" | "float" | "real" | "date" | "datetime" | "datetime2" | "uniqueidentifier") => {
            format!("CAST({} AS {t})", lit(&p.value))
        }
        _ => lit(&p.value),
    };
    let args = args.join(", ");
    let body = format!(
        "IF NOT EXISTS (SELECT 1 FROM sys.fn_listextendedproperty({}, {}))\n    EXEC sys.sp_addextendedproperty {args};\nELSE\n    EXEC sys.sp_updateextendedproperty {args};",
        lit(&p.name),
        list.join(", ")
    );
    // A constraint or trigger the clone left out (said in the notes):
    // its description is skipped rather than failing.
    match (p.levels.first(), p.levels.get(2)) {
        (Some((_, schema)), Some((t, name))) if t == "CONSTRAINT" || t == "TRIGGER" => format!(
            "DECLARE @v sql_variant = {value};\nIF OBJECT_ID({}) IS NOT NULL\nBEGIN\n{body}\nEND",
            lit(&qn(schema, name))
        ),
        _ => format!("DECLARE @v sql_variant = {value};\n{body}"),
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, base: &str) -> Column {
        Column { name: name.into(), base: base.into(), type_name: base.into(), max_length: 4, precision: 10, ..Default::default() }
    }

    fn table(schema: &str, name: &str) -> Table {
        Table { id: 1, schema: schema.into(), name: name.into(), columns: vec![col("Id", "int")], ..Default::default() }
    }

    fn index(name: &str, kind: i32, keys: &[(&str, bool)]) -> Index {
        Index {
            name: name.into(),
            kind,
            keys: keys.iter().map(|(c, d)| ((*c).to_string(), *d)).collect(),
            row_locks: true,
            page_locks: true,
            ..Default::default()
        }
    }

    fn fg(name: &str) -> Option<Space> {
        Some(Space { name: name.into(), column: None })
    }

    fn ps(name: &str, column: &str) -> Option<Space> {
        Some(Space { name: name.into(), column: Some(column.into()) })
    }

    #[test]
    fn columns_keep_every_attribute() {
        let id = Column {
            identity: Some(Identity { seed: "1000".into(), increment: "5".into(), not_for_replication: true }),
            ..col("Id", "bigint")
        };
        assert_eq!(column_sql(&id), "[Id] bigint IDENTITY(1000, 5) NOT FOR REPLICATION NOT NULL");
        let g = Column {
            rowguidcol: true,
            default: Some(("DF_t_G".into(), "(newsequentialid())".into())),
            ..col("G", "uniqueidentifier")
        };
        assert_eq!(column_sql(&g), "[G] uniqueidentifier ROWGUIDCOL NOT NULL CONSTRAINT [DF_t_G] DEFAULT (newsequentialid())");
        let name = Column { max_length: 200, collation: Some("Latin1_General_CS_AS".into()), nullable: true, sparse: true, ..col("N", "nvarchar") };
        assert_eq!(column_sql(&name), "[N] nvarchar(100) COLLATE Latin1_General_CS_AS SPARSE NULL");
        let calc = Column { computed: Some(("([dbo].[f]([Id]))".into(), true)), ..col("C", "int") };
        assert_eq!(column_sql(&calc), "[C] AS ([dbo].[f]([Id])) PERSISTED NOT NULL");
        let calc = Column { computed: Some(("([a]*(2))".into(), false)), nullable: true, ..col("C", "int") };
        assert_eq!(column_sql(&calc), "[C] AS ([a]*(2))");
        let alias = Column { type_name: "Code".into(), type_schema: "ven".into(), user_defined: true, max_length: 20, collation: Some("X".into()), ..col("Code", "varchar") };
        assert_eq!(column_sql(&alias), "[Code] [ven].[Code] NOT NULL");
        let sysname = Column { type_name: "sysname".into(), max_length: 256, ..col("S", "nvarchar") };
        assert_eq!(column_sql(&sysname), "[S] sysname NOT NULL");
        let masked = Column { masked: Some("partial(1,\"X\",0)".into()), max_length: 50, nullable: true, ..col("E", "varchar") };
        assert_eq!(column_sql(&masked), "[E] varchar(50) MASKED WITH (FUNCTION = 'partial(1,\"X\",0)') NULL");
        let doc = Column { xml_schema: Some(("ven".into(), "S".into(), true)), max_length: -1, nullable: true, ..col("D", "xml") };
        assert_eq!(column_sql(&doc), "[D] xml(DOCUMENT [ven].[S]) NULL");
        let geo = Column { type_name: "geography".into(), assembly: true, max_length: -1, nullable: true, ..col("L", "geography") };
        assert_eq!(column_sql(&geo), "[L] geography NULL");
        let set = Column { column_set: true, nullable: true, ..col("cs", "xml") };
        assert_eq!(column_sql(&set), "[cs] xml COLUMN_SET FOR ALL_SPARSE_COLUMNS");
        for (base, len, p, sc, want) in [
            ("varchar", -1, 0, 0, "varchar(max)"),
            ("decimal", 9, 18, 2, "decimal(18, 2)"),
            ("datetime2", 8, 27, 3, "datetime2(3)"),
            ("float", 8, 53, 0, "float"),
            ("float", 4, 24, 0, "float(24)"),
            ("binary", 16, 0, 0, "binary(16)"),
        ] {
            assert_eq!(type_sql(&Column { base: base.into(), max_length: len, precision: p, scale: sc, ..Default::default() }), want);
        }
    }

    #[test]
    fn primary_key_and_storage_as_on_the_source() {
        let mut t = table("pt", "Docs");
        t.columns.push(Column { max_length: -1, nullable: true, ..col("Body", "nvarchar") });
        t.space = fg("FG_NEW");
        t.lob_space = Some("FG_LOB".into());
        t.pk = Some(Index { primary: true, fill_factor: 90, space: fg("FG_NEW"), ..index("PK_Docs", 2, &[("Id", true)]) });
        let sql = create_table(&t);
        assert!(sql.starts_with("IF OBJECT_ID(N'[pt].[Docs]', 'U') IS NULL\nCREATE TABLE [pt].[Docs] ("), "{sql}");
        assert!(sql.contains("CONSTRAINT [PK_Docs] PRIMARY KEY NONCLUSTERED ([Id] DESC) WITH (FILLFACTOR = 90) ON [FG_NEW]"), "{sql}");
        assert!(sql.ends_with(") ON [FG_NEW] TEXTIMAGE_ON [FG_LOB];"), "{sql}");
        // A LOB filegroup without LOB columns left: no TEXTIMAGE_ON.
        let no_lob = Table { columns: vec![t.columns[0].clone()], ..t.clone() };
        assert!(create_table(&no_lob).ends_with(") ON [FG_NEW];"));
        // Partitioned: rows on the scheme, no TEXTIMAGE_ON; a heap's own
        // per-partition compression.
        let heap = Table {
            space: ps("ps_region", "Region"),
            pk: None,
            heap_compression: Compression { all: None, parts: vec![(1, "PAGE".into()), (2, "NONE".into()), (3, "PAGE".into())] },
            ..t.clone()
        };
        assert!(create_table(&heap).ends_with(
            ") ON [ps_region]([Region]) WITH (DATA_COMPRESSION = PAGE ON PARTITIONS (1, 3), DATA_COMPRESSION = NONE ON PARTITIONS (2));"
        ));
        let row = Table { heap_compression: Compression { all: Some("ROW".into()), parts: vec![] }, pk: None, ..t };
        assert!(create_table(&row).contains("TEXTIMAGE_ON [FG_LOB] WITH (DATA_COMPRESSION = ROW);"));
    }

    #[test]
    fn memory_optimized_table_declares_its_indexes() {
        let mut t = table("dbo", "Sessions");
        t.memory = Some("SCHEMA_ONLY".into());
        t.pk = Some(Index { primary: true, bucket_count: Some(1024), ..index("PK_S", 7, &[("Id", false)]) });
        t.indexes = vec![
            Index { unique: true, unique_constraint: true, bucket_count: Some(2048), ..index("UQ_T", 7, &[("Token", false)]) },
            index("IX_L", 2, &[("LastSeen", true)]),
            index("CCI", 5, &[]),
        ];
        let sql = create_table(&t);
        assert!(sql.contains("CONSTRAINT [PK_S] PRIMARY KEY NONCLUSTERED HASH ([Id]) WITH (BUCKET_COUNT = 1024)"), "{sql}");
        assert!(sql.contains("CONSTRAINT [UQ_T] UNIQUE NONCLUSTERED HASH ([Token]) WITH (BUCKET_COUNT = 2048)"), "{sql}");
        assert!(sql.contains("INDEX [IX_L] NONCLUSTERED ([LastSeen] DESC)"), "{sql}");
        assert!(sql.contains("INDEX [CCI] CLUSTERED COLUMNSTORE"), "{sql}");
        assert!(sql.ends_with(") WITH (MEMORY_OPTIMIZED = ON, DURABILITY = SCHEMA_ONLY);"), "{sql}");
        // Nothing after the data but the identity, which CHECKIDENT refuses.
        t.identity_last = Some("10".into());
        t.identity_max = Some("8".into());
        let mut notes = Vec::new();
        assert!(after_data(&t, &mut notes).is_empty());
        assert!(notes[0].contains("(8) y no desde 10"), "{notes:?}");

        // A target without In-Memory OLTP: a regular table, hash → nonclustered.
        let mut list = vec![t];
        let mut notes = Vec::new();
        adapt_memory_optimized(&mut list, &Target::default(), &mut notes);
        let sql = create_table(&list[0]);
        assert!(!sql.contains("MEMORY_OPTIMIZED") && sql.contains("CONSTRAINT [PK_S] PRIMARY KEY NONCLUSTERED ([Id] ASC)"), "{sql}");
        assert!(after_data(&list[0], &mut notes)[0].contains("CREATE UNIQUE NONCLUSTERED INDEX [UQ_T]") || list[0].indexes[0].unique_constraint);
        assert!(notes[0].contains("In-Memory OLTP") && notes[0].contains("[dbo].[Sessions]"), "{notes:?}");
    }

    #[test]
    fn indexes_in_creation_order_with_every_option() {
        let mut t = table("s", "t");
        let mut nc = index("IX_f", 2, &[("A", false), ("B", true)]);
        nc.unique = true;
        nc.include = vec!["C".into(), "D".into()];
        nc.filter = Some("([Deleted]=(0))".into());
        nc.ignore_dup_key = true;
        nc.page_locks = false;
        nc.pad_index = true;
        nc.fill_factor = 80;
        nc.no_recompute = true;
        nc.sequential_key = true;
        nc.compression = Compression { all: Some("PAGE".into()), parts: vec![] };
        nc.space = ps("ps_day", "Day");
        let uq = Index { unique: true, unique_constraint: true, row_locks: false, ..index("UQ_x", 1, &[("K", false)]) };
        let px = index("PX", 3, &[("Body", false)]);
        let sx = Index { xml_using: Some("PX".into()), xml_for: Some("PATH".into()), ..index("SX", 3, &[("Body", false)]) };
        let off = Index { disabled: true, ..index("IX_off", 2, &[("Z", false)]) };
        t.indexes = vec![sx, nc, px, off, uq];
        t.stats = vec![Stat { name: "ST".into(), columns: vec!["A".into(), "B".into()], filter: Some("([A]>(0))".into()), no_recompute: true }];
        t.identity_last = Some("41".into());
        let out = after_data(&t, &mut Vec::new());
        let names: Vec<&str> = out.iter().map(|s| s.split("AND name = N'").nth(1).map_or("?", |r| r.split('\'').next().unwrap())).collect();
        assert_eq!(names, ["UQ_x", "IX_f", "IX_off", "PX", "SX", "ST", "?"]);
        assert!(out.iter().all(|s| s.starts_with(INDEX_SET) || s.starts_with("DECLARE")));
        assert!(out[0].contains("ALTER TABLE [s].[t] ADD CONSTRAINT [UQ_x] UNIQUE CLUSTERED ([K] ASC) WITH (ALLOW_ROW_LOCKS = OFF);"), "{}", out[0]);
        assert!(
            out[1].contains(
                "CREATE UNIQUE NONCLUSTERED INDEX [IX_f] ON [s].[t] ([A] ASC, [B] DESC) INCLUDE ([C], [D]) WHERE ([Deleted]=(0)) \
                 WITH (PAD_INDEX = ON, FILLFACTOR = 80, IGNORE_DUP_KEY = ON, STATISTICS_NORECOMPUTE = ON, ALLOW_PAGE_LOCKS = OFF, \
                 OPTIMIZE_FOR_SEQUENTIAL_KEY = ON, DATA_COMPRESSION = PAGE) ON [ps_day]([Day]);"
            ),
            "{}",
            out[1]
        );
        assert!(out[2].contains("ALTER INDEX [IX_off] ON [s].[t] DISABLE;"), "{}", out[2]);
        assert!(out[3].contains("CREATE PRIMARY XML INDEX [PX] ON [s].[t] ([Body]);"), "{}", out[3]);
        assert!(out[4].contains("CREATE XML INDEX [SX] ON [s].[t] ([Body]) USING XML INDEX [PX] FOR PATH;"), "{}", out[4]);
        assert!(out[5].contains("CREATE STATISTICS [ST] ON [s].[t] ([A], [B]) WHERE ([A]>(0)) WITH NORECOMPUTE;"), "{}", out[5]);
        assert!(out[6].contains("DBCC CHECKIDENT(N'[s].[t]', RESEED, @v)"), "{}", out[6]);
    }

    #[test]
    fn identity_reseed_one_ahead_when_empty() {
        let sql = reseed("[s].[t]", "41").unwrap();
        assert!(sql.starts_with("DECLARE @v decimal(38, 0) = 41;"));
        assert!(sql.contains("IF NOT EXISTS (SELECT 1 FROM [s].[t]) SET @v = @v + IDENT_INCR(N'[s].[t]');"), "{sql}");
        assert!(reseed("[s].[t]", "4; DROP TABLE x").is_none());
    }

    #[test]
    fn columnstore_spatial_and_per_partition_compression() {
        let cci = Index {
            columns: vec!["Day".into(), "Store".into()],
            compression: Compression { all: Some("COLUMNSTORE_ARCHIVE".into()), parts: vec![] },
            compression_delay: 30,
            space: ps("ps", "Day"),
            ..index("CCI", 5, &[])
        };
        let sql = create_index("[s].[t]", &cci);
        assert!(sql.contains(
            "CREATE CLUSTERED COLUMNSTORE INDEX [CCI] ON [s].[t] ORDER ([Day], [Store]) WITH (DATA_COMPRESSION = COLUMNSTORE_ARCHIVE, COMPRESSION_DELAY = 30) ON [ps]([Day]);"
        ), "{sql}");
        let parts = Index {
            compression: Compression { all: None, parts: vec![(1, "COLUMNSTORE_ARCHIVE".into()), (2, "COLUMNSTORE".into())] },
            ..index("CCI2", 5, &[])
        };
        assert!(create_index("[s].[t]", &parts).contains(
            "WITH (DATA_COMPRESSION = COLUMNSTORE_ARCHIVE ON PARTITIONS (1), DATA_COMPRESSION = COLUMNSTORE ON PARTITIONS (2));"
        ));
        let ncci = Index { columns: vec!["Qty".into()], filter: Some("([Deleted]=(0))".into()), ..index("NCCI", 6, &[]) };
        assert!(create_index("[s].[t]", &ncci).contains("CREATE NONCLUSTERED COLUMNSTORE INDEX [NCCI] ON [s].[t] ([Qty]) WHERE ([Deleted]=(0));"));
        let g = Index {
            spatial: Some(Spatial {
                scheme: "GEOMETRY_GRID".into(),
                bounding_box: Some([0.0, 0.0, 500.0, 200.5]),
                grids: Some(["LOW".into(), "MEDIUM".into(), "HIGH".into(), "MEDIUM".into()]),
                cells_per_object: Some(32),
            }),
            space: fg("FG_A"),
            ..index("SG", 4, &[("Shape", false)])
        };
        assert!(create_index("[s].[t]", &g).contains(
            "CREATE SPATIAL INDEX [SG] ON [s].[t] ([Shape]) USING GEOMETRY_GRID WITH (BOUNDING_BOX = (0, 0, 500, 200.5), \
             GRIDS = (LEVEL_1 = LOW, LEVEL_2 = MEDIUM, LEVEL_3 = HIGH, LEVEL_4 = MEDIUM), CELLS_PER_OBJECT = 32) ON [FG_A];"
        ));
        let a = Index {
            spatial: Some(Spatial {
                scheme: "GEOGRAPHY_AUTO_GRID".into(),
                bounding_box: Some([1.0, 2.0, 3.0, 4.0]),
                grids: Some(["LOW".into(), "LOW".into(), "LOW".into(), "LOW".into()]),
                cells_per_object: Some(12),
            }),
            space: ps("ps", "Day"),
            ..index("SA", 4, &[("Loc", false)])
        };
        assert!(create_index("[s].[t]", &a).contains("USING GEOGRAPHY_AUTO_GRID WITH (CELLS_PER_OBJECT = 12);"));
        assert_eq!(Compression::from_parts(vec![(1, "PAGE".into()), (2, "PAGE".into())]), Compression { all: Some("PAGE".into()), parts: vec![] });
    }

    #[test]
    fn partition_functions_and_schemes() {
        let f = PartitionFunction {
            name: "pf_day".into(),
            range_right: true,
            param: Column { base: "datetime2".into(), scale: 3, ..Default::default() },
            boundaries: vec![boundary_literal("datetime2", "2024-01-01T00:00:00", "datetime2(3)"), boundary_literal("datetime2", "2025-01-01T00:00:00", "datetime2(3)")],
        };
        let sql = partition_function(&f);
        assert!(sql.starts_with("IF NOT EXISTS (SELECT 1 FROM sys.partition_functions WHERE name = N'pf_day')"));
        assert!(sql.ends_with(
            "CREATE PARTITION FUNCTION [pf_day] (datetime2(3)) AS RANGE RIGHT FOR VALUES (CONVERT(datetime2(3), N'2024-01-01T00:00:00', 126), CONVERT(datetime2(3), N'2025-01-01T00:00:00', 126));"
        ), "{sql}");
        assert_eq!(boundary_literal("nvarchar", "O'Hara", "nvarchar(10)"), "N'O''Hara'");
        assert_eq!(boundary_literal("int", "100", "int"), "100");
        assert_eq!(boundary_literal("uniqueidentifier", "00000000-0000-0000-0000-000000000001", "uniqueidentifier"), "CAST(N'00000000-0000-0000-0000-000000000001' AS uniqueidentifier)");
        // The NEXT USED filegroup is the one listed past the partitions.
        let s = PartitionScheme { name: "ps_day".into(), function: "pf_day".into(), filegroups: vec!["FG_OLD".into(), "PRIMARY".into(), "PRIMARY".into(), "FG_NEXT".into()] };
        let sql = partition_scheme(&s);
        assert!(sql.contains("CREATE PARTITION SCHEME [ps_day] AS PARTITION [pf_day] TO ([FG_OLD], [PRIMARY], [PRIMARY], [FG_NEXT]);"), "{sql}");
        assert!(sql.contains("IF EXISTS (SELECT 1 FROM sys.data_spaces WHERE name = N'ps_day')") && sql.contains("THROW 50000"), "{sql}");
    }

    #[test]
    fn missing_filegroups_created_or_moved_to_primary_on_azure() {
        let mut t = table("s", "t");
        t.space = fg("FG_A");
        t.lob_space = Some("FG_LOB".into());
        t.indexes = vec![Index { space: fg("FG_IX"), ..index("IX", 2, &[("Id", false)]) }, Index { space: ps("ps", "Id"), ..index("IX2", 2, &[("Id", false)]) }];
        let schemes = vec![PartitionScheme { name: "ps".into(), function: "pf".into(), filegroups: vec!["FG_P".into(), "PRIMARY".into()] }];
        let target = Target { filegroups: vec!["PRIMARY".into(), "fg_ix".into()], ..Default::default() };
        let (mut tables, mut s2, mut notes) = (vec![t.clone()], schemes.clone(), Vec::new());
        let missing = adapt_filegroups(&mut tables, &mut HashMap::new(), &mut s2, &target, &mut notes);
        assert_eq!(missing, ["FG_P", "FG_A", "FG_LOB"]);
        // A scheme's name is never taken for a filegroup (a partitioned
        // table whose LOB space came back as its scheme).
        let mut on_scheme = t.clone();
        on_scheme.lob_space = Some("ps".into());
        let mut none = Vec::new();
        assert_eq!(adapt_filegroups(&mut [on_scheme], &mut HashMap::new(), &mut schemes.clone(), &target, &mut none), ["FG_P", "FG_A"]);
        assert!(notes.is_empty());
        let fg_sql = create_filegroup("FG_A");
        assert!(fg_sql.starts_with("IF NOT EXISTS (SELECT 1 FROM sys.filegroups WHERE name = N'FG_A')"));
        assert!(fg_sql.contains("ADD FILEGROUP ' + QUOTENAME(@fg) + N';'") && fg_sql.contains("N'.ndf'") && fg_sql.contains("IF @edition = 8"));
        let mo = create_memory_filegroup("MOD");
        assert!(mo.starts_with("IF NOT EXISTS (SELECT 1 FROM sys.filegroups WHERE type = 'FX')") && mo.contains("CONTAINS MEMORY_OPTIMIZED_DATA"));

        // Azure SQL Database: PRIMARY only; partitioning kept.
        let azure = Target { caps: Caps { edition: 5, ..Default::default() }, ..target };
        let (mut tables, mut s2, mut notes) = (vec![t], schemes, Vec::new());
        assert!(adapt_filegroups(&mut tables, &mut HashMap::new(), &mut s2, &azure, &mut notes).is_empty());
        assert_eq!(s2[0].filegroups, ["PRIMARY", "PRIMARY"]);
        assert_eq!(tables[0].space, fg("PRIMARY"));
        assert_eq!(tables[0].lob_space.as_deref(), Some("PRIMARY"));
        assert_eq!(tables[0].indexes[1].space, ps("ps", "Id"));
        assert!(notes[0].contains("Azure SQL Database") && notes[0].contains("FG_P, FG_A, FG_LOB"), "{notes:?}");
    }

    #[test]
    fn constraints_keep_trust_and_state() {
        let t = table("s", "t");
        let fk = ForeignKey {
            name: "FK_x".into(),
            columns: vec!["A".into(), "B".into()],
            ref_schema: "s".into(),
            ref_table: "p".into(),
            ref_columns: vec!["Id".into(), "R".into()],
            on_delete: "CASCADE".into(),
            on_update: "SET_NULL".into(),
            not_for_replication: true,
            disabled: true,
            not_trusted: true,
        };
        let sql = foreign_key(&t, &fk);
        assert!(sql.starts_with("IF OBJECT_ID(N'[s].[FK_x]', 'F') IS NULL"), "{sql}");
        assert!(sql.contains(
            "ALTER TABLE [s].[t] WITH NOCHECK ADD CONSTRAINT [FK_x] FOREIGN KEY ([A], [B]) REFERENCES [s].[p] ([Id], [R]) ON DELETE CASCADE ON UPDATE SET NULL NOT FOR REPLICATION;"
        ), "{sql}");
        assert!(sql.contains("ALTER TABLE [s].[t] NOCHECK CONSTRAINT [FK_x];"));
        let trusted = ForeignKey { disabled: false, not_trusted: false, not_for_replication: false, on_delete: "NO_ACTION".into(), on_update: "CASCADE".into(), ..fk.clone() };
        let sql = foreign_key(&t, &trusted);
        assert!(sql.contains("WITH CHECK ADD CONSTRAINT [FK_x]") && sql.contains("REFERENCES [s].[p] ([Id], [R]) ON UPDATE CASCADE;") && !sql.contains("NOCHECK"), "{sql}");
        let untrusted = ForeignKey { disabled: false, ..fk };
        let sql = foreign_key(&t, &untrusted);
        assert!(sql.contains("WITH NOCHECK ADD") && !sql.contains("NOCHECK CONSTRAINT"), "{sql}");

        let ck = Check { id: 0, name: "CK_a".into(), definition: "([dbo].[f]([A])=(1))".into(), disabled: true, not_trusted: true, not_for_replication: true };
        let sql = check_constraint(&t, &ck);
        assert!(sql.contains("ALTER TABLE [s].[t] WITH NOCHECK ADD CONSTRAINT [CK_a] CHECK NOT FOR REPLICATION ([dbo].[f]([A])=(1));"), "{sql}");
        assert!(sql.contains("NOCHECK CONSTRAINT [CK_a];"));
        let ok = Check { disabled: false, not_trusted: false, not_for_replication: false, ..ck };
        assert!(check_constraint(&t, &ok).contains("WITH CHECK ADD CONSTRAINT [CK_a] CHECK ([dbo].[f]([A])=(1));"));
    }

    #[test]
    fn temporal_created_plain_then_versioned() {
        let mut t = table("dbo", "Price");
        t.columns.push(Column { hidden: true, scale: 7, ..col("ValidFrom", "datetime2") });
        t.columns.push(Column { hidden: true, scale: 7, ..col("ValidTo", "datetime2") });
        let tmp = Temporal { start: "ValidFrom".into(), end: "ValidTo".into(), history_schema: "hist".into(), history_table: "PriceHistory".into(), retention: Some("6 MONTHS".into()) };
        t.temporal = Some(tmp.clone());
        let create = create_table(&t);
        assert!(!create.contains("PERIOD") && !create.contains("GENERATED") && !create.contains("HIDDEN"), "{create}");
        assert!(create.contains("[ValidFrom] datetime2(7) NOT NULL"), "{create}");
        let script = CloneTable { table: ObjectRef { kind: "table".into(), schema: None, name: "x".into() }, create, before_data: vec![], after_data: after_data(&t, &mut Vec::new()) };
        assert!(script.before_data.is_empty() && script.after_data.is_empty());
        let on = enable_temporal(&t, &tmp);
        assert_eq!(on.len(), 4, "{on:?}");
        assert!(on[0].contains("EXEC(N'ALTER TABLE [dbo].[Price] ADD PERIOD FOR SYSTEM_TIME ([ValidFrom], [ValidTo])');"), "{}", on[0]);
        assert!(on[1].contains("ALTER COLUMN [ValidFrom] ADD HIDDEN"));
        assert!(on[3].contains(
            "SET (SYSTEM_VERSIONING = ON (HISTORY_TABLE = [hist].[PriceHistory], DATA_CONSISTENCY_CHECK = ON, HISTORY_RETENTION_PERIOD = 6 MONTHS))"
        ), "{}", on[3]);
        // A target without temporal tables: plain, said.
        let mut list = vec![t];
        let mut notes = Vec::new();
        adapt_features(&mut list, &mut HashMap::new(), &HashMap::new(), &Target::default(), &mut notes);
        assert!(list[0].temporal.is_none() && notes.iter().any(|n| n.contains("tablas temporales")), "{notes:?}");
    }

    #[test]
    fn newer_index_options_dropped_on_older_targets() {
        let mut t = table("s", "t");
        t.indexes = vec![
            Index { columns: vec!["Day".into()], compression_delay: 5, ..index("CCI", 5, &[]) },
            Index { sequential_key: true, ..index("IX", 2, &[("Id", false)]) },
        ];
        let mut list = vec![t];
        let mut notes = Vec::new();
        adapt_features(&mut list, &mut HashMap::new(), &HashMap::new(), &Target::default(), &mut notes);
        assert!(list[0].indexes[0].columns.is_empty() && list[0].indexes[0].compression_delay == 0 && !list[0].indexes[1].sequential_key);
        assert_eq!(notes.len(), 3, "{notes:?}");
        let full = Target { caps: Caps { ordered_columnstore: true, sequential_key: true, compression_delay: true, temporal: true, retention: true, ..Default::default() }, ..Default::default() };
        let mut t2 = table("s", "t");
        t2.indexes = vec![Index { columns: vec!["Day".into()], ..index("CCI", 5, &[]) }];
        let mut list = vec![t2];
        let mut notes = Vec::new();
        adapt_features(&mut list, &mut HashMap::new(), &HashMap::new(), &full, &mut notes);
        assert!(notes.is_empty() && !list[0].indexes[0].columns.is_empty());
    }

    #[test]
    fn types_sequences_and_synonym_values() {
        let alias = UserType { schema: "ven".into(), name: "Code".into(), base: Column { base: "varchar".into(), max_length: 20, ..Default::default() }, table: None };
        assert_eq!(user_type(&alias), "IF TYPE_ID(N'[ven].[Code]') IS NULL\n    CREATE TYPE [ven].[Code] FROM varchar(20) NOT NULL;");
        let tt = UserType {
            schema: "ven".into(),
            name: "Lines".into(),
            base: Column::default(),
            table: Some(TableType {
                columns: vec![col("Id", "int"), Column { nullable: true, default: Some(("DF__x".into(), "(N'x')".into())), max_length: 20, ..col("Note", "nvarchar") }],
                indexes: vec![
                    Index { primary: true, ..index("PK__x", 1, &[("Id", false)]) },
                    index("IX_Note", 2, &[("Note", true)]),
                    Index { unique_constraint: true, bucket_count: Some(64), ..index("UQ__y", 7, &[("Note", false)]) },
                ],
                checks: vec!["([Id]>(0))".into()],
                memory_optimized: true,
            }),
        };
        let sql = user_type(&tt);
        assert!(sql.contains("[Note] nvarchar(10) NULL DEFAULT (N'x')"), "{sql}");
        assert!(sql.contains("PRIMARY KEY CLUSTERED ([Id] ASC)") && sql.contains("INDEX [IX_Note] NONCLUSTERED ([Note] DESC)"), "{sql}");
        assert!(sql.contains("UNIQUE NONCLUSTERED HASH ([Note]) WITH (BUCKET_COUNT = 64)") && sql.contains("CHECK ([Id]>(0))"), "{sql}");
        assert!(sql.ends_with(") WITH (MEMORY_OPTIMIZED = ON);") && !sql.contains("DF__x"), "{sql}");

        let sq = Sequence {
            schema: "s".into(),
            name: "q".into(),
            type_sql: "int".into(),
            start: "1".into(),
            increment: "5".into(),
            min: "1".into(),
            max: "100".into(),
            cycle: false,
            cached: true,
            cache_size: Some(20),
            current: "41".into(),
            used: true,
            unknown_use: false,
        };
        assert!(sequence(&sq).ends_with("CREATE SEQUENCE [s].[q] AS int START WITH 1 INCREMENT BY 5 MINVALUE 1 MAXVALUE 100 NO CYCLE CACHE 20;"));
        assert_eq!(sequence_value(&sq).unwrap(), "ALTER SEQUENCE [s].[q] RESTART WITH 41;\nDECLARE @v sql_variant = NEXT VALUE FOR [s].[q];");
        assert!(sequence_value(&Sequence { used: false, ..sq.clone() }).is_none());
        assert!(sequence(&Sequence { cached: false, cycle: true, ..sq }).contains("CYCLE NO CACHE;"));
    }

    #[test]
    fn modules_keep_their_options_triggers_their_state() {
        let v = Module { id: 1, schema: "s".into(), name: "v".into(), kind: "V".into(), definition: Some("CREATE VIEW s.v AS SELECT 'x' AS a".into()), ..Default::default() };
        let sql = module(&v);
        assert_eq!(
            sql,
            "IF OBJECT_ID(N'[s].[v]') IS NULL\n    EXEC(N'SET ANSI_NULLS OFF; SET QUOTED_IDENTIFIER OFF; EXEC(N''CREATE VIEW s.v AS SELECT ''''x'''' AS a'');');"
        );
        let tr = Module {
            id: 2,
            schema: "s".into(),
            name: "tr".into(),
            kind: "TR".into(),
            definition: Some("CREATE TRIGGER s.tr ON s.t AFTER INSERT AS\nGO\nSELECT 1".into()),
            ansi_nulls: true,
            quoted_identifier: true,
            parent: Some((9, "s".into(), "t".into())),
            disabled: true,
            orders: vec![("First".into(), "INSERT".into())],
            ..Default::default()
        };
        let sql = module(&tr);
        assert!(sql.contains("SET ANSI_NULLS ON; SET QUOTED_IDENTIFIER ON;"), "{sql}");
        // The GO line can't split the batch in a client that splits on it.
        assert!(sql.lines().all(|l| !l.trim().eq_ignore_ascii_case("go")), "{sql}");
        assert!(sql.contains("AS\n' + N'GO\nSELECT 1"), "{sql}");
        assert!(sql.contains("\nDISABLE TRIGGER [s].[tr] ON [s].[t];"), "{sql}");
        assert!(sql.contains("EXEC sp_settriggerorder @triggername = N'[s].[tr]', @order = N'First', @stmttype = N'INSERT';"), "{sql}");
    }

    #[test]
    fn modules_in_dependency_order() {
        let m = |id, kind: &str, name: &str| Module { id, kind: kind.into(), schema: "s".into(), name: name.into(), definition: Some("x".into()), ..Default::default() };
        let mut tr = m(5, "TR", "a_trigger");
        tr.parent = Some((3, "s".into(), "v_z".into()));
        let modules = vec![m(1, "P", "a_proc"), m(2, "FN", "f_b"), m(3, "V", "v_z"), m(4, "V", "v_a"), tr, m(6, "FN", "f_a")];
        let deps: HashMap<i32, HashSet<i32>> = [(4, HashSet::from([3])), (3, HashSet::from([2])), (2, HashSet::from([6])), (1, HashSet::from([4]))].into();
        let order: Vec<&str> = module_order(&modules, &deps).into_iter().map(|i| modules[i].name.as_str()).collect();
        assert_eq!(order, ["f_a", "f_b", "v_z", "v_a", "a_proc", "a_trigger"]);
    }

    #[test]
    fn extended_properties_add_or_update_keeping_the_type() {
        let p = ExtendedProperty {
            name: "MS_Description".into(),
            value: "Monto (sin IVA)".into(),
            base_type: "nvarchar".into(),
            levels: vec![("SCHEMA".into(), "ven".into()), ("TABLE".into(), "Parent".into()), ("COLUMN".into(), "Amount".into())],
        };
        let sql = extended_property(&p);
        assert!(sql.starts_with("DECLARE @v sql_variant = N'Monto (sin IVA)';"), "{sql}");
        assert!(sql.contains("fn_listextendedproperty(N'MS_Description', N'SCHEMA', N'ven', N'TABLE', N'Parent', N'COLUMN', N'Amount')"), "{sql}");
        assert!(sql.contains("sp_addextendedproperty @name = N'MS_Description', @value = @v, @level0type = N'SCHEMA', @level0name = N'ven'"), "{sql}");
        assert!(sql.contains("ELSE\n    EXEC sys.sp_updateextendedproperty"), "{sql}");
        let db = ExtendedProperty { levels: vec![], base_type: "int".into(), value: "42".into(), ..p.clone() };
        let sql = extended_property(&db);
        assert!(sql.starts_with("DECLARE @v sql_variant = CAST(N'42' AS int);") && sql.contains("NULL, NULL, NULL, NULL, NULL, NULL"), "{sql}");
        let ck = ExtendedProperty { levels: vec![("SCHEMA".into(), "ven".into()), ("TABLE".into(), "t".into()), ("CONSTRAINT".into(), "CK_x".into())], ..p };
        assert!(extended_property(&ck).contains("IF OBJECT_ID(N'[ven].[CK_x]') IS NOT NULL\nBEGIN"));
    }

    /// The whole script from a fixture source: order, scope, notes.
    #[test]
    fn script_from_a_fixture_source() {
        let mut parent = table("s", "Parent");
        parent.id = 10;
        parent.pk = Some(Index { primary: true, ..index("PK_P", 1, &[("Id", false)]) });
        let mut child = table("s", "Child");
        child.id = 11;
        child.columns.push(Column { computed: Some(("([s].[f_calc]([Id]))".into(), false)), nullable: true, ..col("Calc", "int") });
        child.checks = vec![Check { name: "CK_c".into(), definition: "([s].[f_ck]([Id])=(1))".into(), ..Default::default() }];
        child.fks = vec![
            ForeignKey { name: "FK_p".into(), columns: vec!["Id".into()], ref_schema: "s".into(), ref_table: "Parent".into(), ref_columns: vec!["Id".into()], ..Default::default() },
            ForeignKey { name: "FK_o".into(), columns: vec!["Id".into()], ref_schema: "s".into(), ref_table: "Other".into(), ref_columns: vec!["Id".into()], ..Default::default() },
        ];
        child.space = ps("ps", "Id");
        let mut other = table("s", "Other");
        other.id = 12;
        let def = |s: &str| Some(s.to_string());
        let m = |id, kind: &str, name: &str, d: Option<String>| Module { id, kind: kind.into(), schema: "s".into(), name: name.into(), definition: d, ansi_nulls: true, quoted_identifier: true, ..Default::default() };
        let mut trig = m(24, "TR", "tr_other", def("CREATE TRIGGER s.tr_other ON s.Other AFTER INSERT AS SELECT 1"));
        trig.parent = Some((12, "s".into(), "Other".into()));
        let src = Source {
            schemas: vec!["s".into()],
            tables: vec![parent, child, other],
            functions: vec![PartitionFunction { name: "pf".into(), param: col("", "int"), boundaries: vec!["10".into()], ..Default::default() }],
            schemes: vec![
                PartitionScheme { name: "ps".into(), function: "pf".into(), filegroups: vec!["PRIMARY".into(), "PRIMARY".into()] },
                PartitionScheme { name: "ps_unused".into(), function: "pf_unused".into(), filegroups: vec!["FG_X".into()] },
            ],
            modules: vec![
                m(20, "FN", "f_calc", def("CREATE FUNCTION s.f_calc(@x int) RETURNS int AS BEGIN RETURN @x END")),
                m(21, "FN", "f_ck", def("CREATE FUNCTION s.f_ck(@x int) RETURNS int AS BEGIN RETURN 1 END")),
                m(22, "V", "v_other", def("CREATE VIEW s.v_other AS SELECT * FROM s.Other")),
                m(23, "P", "p_secret", None),
                trig,
                m(25, "V", "v_parent", def("CREATE VIEW s.v_parent AS SELECT * FROM s.Parent")),
            ],
            deps: [(22, HashSet::from([12])), (25, HashSet::from([10]))].into(),
            column_functions: [(11, vec![ColumnUse { column: "Calc".into(), default: false, function: 20 }])].into(),
            sequences: vec![Sequence { schema: "s".into(), name: "q".into(), type_sql: "int".into(), current: "5".into(), start: "1".into(), used: true, ..Default::default() }],
            synonyms: vec![("s".into(), "syn".into(), "[s].[Parent]".into())],
            properties: vec![
                ExtendedProperty { name: "d".into(), value: "x".into(), levels: vec![("SCHEMA".into(), "s".into()), ("TABLE".into(), "Other".into())], ..Default::default() },
                ExtendedProperty { name: "d".into(), value: "y".into(), levels: vec![("SCHEMA".into(), "s".into()), ("TABLE".into(), "Parent".into())], ..Default::default() },
            ],
            ddl_triggers: 1,
            principals: 2,
            ..Default::default()
        };
        let target = Target { caps: Caps { xtp: true, memory_optimized: true, temporal: true, ..Default::default() }, filegroups: vec!["PRIMARY".into()], ..Default::default() };
        let tables = [ObjectRef { kind: "table".into(), schema: Some("s".into()), name: "Child".into() }, ObjectRef { kind: "table".into(), schema: None, name: "Parent".into() }]
            .map(|mut r| {
                if r.schema.is_none() {
                    r.schema = Some("s".into());
                }
                r
            });
        let script = build(src.clone(), &target, &tables).unwrap();
        let find = |list: &[String], what: &str| list.iter().position(|s| s.contains(what));
        // Before: schema, the used partition function and scheme only, the
        // sequence, and the function a column uses.
        assert!(script.before[0].contains("CREATE SCHEMA [s]"));
        assert!(find(&script.before, "PARTITION FUNCTION [pf]").is_some() && find(&script.before, "ps_unused").is_none());
        assert!(find(&script.before, "FG_X").is_none());
        assert!(find(&script.before, "CREATE SEQUENCE [s].[q]").is_some());
        assert!(find(&script.before, "CREATE FUNCTION s.f_calc").is_some());
        assert_eq!(script.tables.iter().map(|t| t.table.name.as_str()).collect::<Vec<_>>(), ["Child", "Parent"]);
        assert!(script.tables[0].create.contains(") ON [ps]([Id]);"));
        // After: sequence value, synonym, code, then CHECKs (they may call
        // it), then FKs; the view over a table not cloned and the trigger
        // of one are left out; descriptions only of what was cloned.
        let a = &script.after;
        let seq = find(a, "RESTART WITH 5").unwrap();
        let syn = find(a, "CREATE SYNONYM [s].[syn]").unwrap();
        let f_ck = find(a, "CREATE FUNCTION s.f_ck").unwrap();
        let v = find(a, "CREATE VIEW s.v_parent").unwrap();
        let ck = find(a, "CONSTRAINT [CK_c] CHECK").unwrap();
        let fk = find(a, "CONSTRAINT [FK_p] FOREIGN KEY").unwrap();
        assert!(seq < syn && syn < f_ck && f_ck < v && v < ck && ck < fk, "{a:#?}");
        assert!(find(a, "f_calc").is_none() && find(a, "v_other").is_none() && find(a, "tr_other").is_none() && find(a, "FK_o").is_none());
        assert!(find(a, "N'Parent'").is_some() && find(a, "N'Other'").is_none());
        let notes = script.notes.join("\n");
        for want in ["[s].[p_secret] (procedimiento): su definición está cifrada", "[s].[v_other] (vista): usa tablas", "[FK_o] apunta a s.Other", "trigger(s) de base (DDL)", "usuarios, roles y permisos", "[s].[q]: la secuencia"] {
            assert!(notes.contains(want), "missing «{want}» in:\n{notes}");
        }
        // A table the source doesn't have.
        let bad = [ObjectRef { kind: "table".into(), schema: Some("s".into()), name: "Nope".into() }];
        assert!(build(src, &target, &bad).unwrap_err().to_string().contains("s.Nope"));
    }

    fn tref(schema: &str, name: &str) -> ObjectRef {
        ObjectRef { kind: "table".into(), schema: Some(schema.into()), name: name.into() }
    }

    fn full_target() -> Target {
        Target {
            caps: Caps { xtp: true, memory_optimized: true, temporal: true, ..Default::default() },
            default_schema: "dbo".into(),
            filegroups: vec!["PRIMARY".into()],
            ..Default::default()
        }
    }

    fn code(id: i32, kind: &str, schema: &str, name: &str, def: &str) -> Module {
        Module {
            id,
            kind: kind.into(),
            schema: schema.into(),
            name: name.into(),
            definition: Some(def.into()),
            ansi_nulls: true,
            quoted_identifier: true,
            ..Default::default()
        }
    }

    #[test]
    fn header_names_found_past_comments_and_quotes() {
        let h = parse_header("-- x\n/* a /* nested */ b */ create or  alter\tVIEW [ven].[v]]x] AS SELECT 1").unwrap();
        assert_eq!((h.schema.as_deref(), h.name.as_str()), (Some("ven"), "v]x"));
        let def = "CREATE PROC dbo . \"p q\" @a int AS SELECT 1";
        let h = parse_header(def).unwrap();
        assert_eq!((h.schema.as_deref(), h.name.as_str(), &def[h.start..h.end]), (Some("dbo"), "p q", "dbo . \"p q\""));
        let h = parse_header("CREATE FUNCTION fñ(@x int) RETURNS int AS BEGIN RETURN 1 END").unwrap();
        assert_eq!((h.schema, h.name.as_str()), (None, "fñ"));
        let h = parse_header("CREATE TRIGGER db.s.tr ON s.t AFTER INSERT AS SELECT 1").unwrap();
        assert_eq!((h.schema.as_deref(), h.name.as_str()), (Some("s"), "tr"));
        assert!(parse_header("ALTER VIEW v AS SELECT 1").is_none());
        assert!(parse_header("CREATE TABLE t (a int)").is_none());
    }

    /// sp_rename / ALTER SCHEMA TRANSFER leave the stored text with the old
    /// name: created under it, then renamed or moved as on the source.
    #[test]
    fn renamed_and_moved_modules_keep_their_text() {
        let taken = HashSet::new();
        let place = Placement { default_schema: "dbo", taken: &taken };
        let mut notes = Vec::new();
        let v = code(1, "V", "ven", "vNew", "CREATE VIEW ven.vOld AS SELECT Id FROM ven.T");
        let sql = module_sql(&v, &place, &mut notes).unwrap();
        assert!(sql.starts_with("IF OBJECT_ID(N'[ven].[vNew]') IS NULL\nBEGIN\nIF OBJECT_ID(N'[ven].[vOld]') IS NULL\nBEGIN\n"), "{sql}");
        assert!(sql.contains("CREATE VIEW ven.vOld AS SELECT Id FROM ven.T"), "{sql}");
        assert!(sql.ends_with("EXEC sp_rename @objname = N'[ven].[vOld]', @newname = N'vNew', @objtype = N'OBJECT';\nEND"), "{sql}");
        assert!(!sql.contains("TRANSFER"));
        let p = code(2, "P", "other", "pMoved", "CREATE PROCEDURE ven.pMoved AS SELECT 1 AS one");
        let sql = module_sql(&p, &place, &mut notes).unwrap();
        assert!(sql.contains("IF OBJECT_ID(N'[ven].[pMoved]') IS NULL"), "{sql}");
        assert!(sql.contains("EXEC(N'ALTER SCHEMA [other] TRANSFER [ven].[pMoved]');") && !sql.contains("sp_rename"), "{sql}");
        assert!(notes.is_empty(), "{notes:?}");

        // The old name belongs to another object now: pointed at the current one.
        let taken: HashSet<(String, String)> = [("ven".to_string(), "vold".to_string())].into();
        let place = Placement { default_schema: "dbo", taken: &taken };
        let sql = module_sql(&v, &place, &mut notes).unwrap();
        assert!(sql.contains("CREATE VIEW [ven].[vNew] AS SELECT Id FROM ven.T") && !sql.contains("sp_rename"), "{sql}");
        assert!(notes[0].contains("[ven].[vNew] (vista)") && notes[0].contains("[ven].[vOld]"), "{notes:?}");

        // A trigger whose table changed schema can't be created from its text.
        let mut tr = code(3, "TR", "other", "tr", "CREATE TRIGGER ven.tr ON ven.T AFTER INSERT AS SELECT 1");
        tr.parent = Some((9, "other".into(), "T".into()));
        let mut notes = Vec::new();
        assert!(module_sql(&tr, &place, &mut notes).is_none());
        assert!(notes[0].contains("[other].[tr] (trigger)") && notes[0].contains("no se clona"), "{notes:?}");
    }

    /// A CREATE with no schema lands in the creator's default schema and
    /// resolves its names there: created as a user whose default schema is
    /// the module's.
    #[test]
    fn unqualified_modules_are_created_in_their_schema() {
        let taken = HashSet::new();
        let place = Placement { default_schema: "dbo", taken: &taken };
        let v = code(1, "V", "app", "vT", "CREATE VIEW vT AS SELECT Id FROM T");
        let sql = module_sql(&v, &place, &mut Vec::new()).unwrap();
        assert!(sql.starts_with("IF OBJECT_ID(N'[app].[vT]') IS NULL\nBEGIN\nIF USER_ID(N'dbine_clone_owner') IS NOT NULL DROP USER"), "{sql}");
        assert!(sql.contains("CREATE USER [dbine_clone_owner] WITHOUT LOGIN WITH DEFAULT_SCHEMA = [app];"), "{sql}");
        assert!(sql.contains("EXECUTE AS USER = N''dbine_clone_owner''; SET ANSI_NULLS ON;"), "{sql}");
        assert!(sql.contains("CREATE VIEW vT AS SELECT Id FROM T") && sql.ends_with("DROP USER [dbine_clone_owner];\nEND"), "{sql}");
        // In the cloning user's own default schema: as it is.
        let d = code(2, "V", "dbo", "vD", "CREATE VIEW vD AS SELECT 1 AS a");
        assert_eq!(
            module_sql(&d, &place, &mut Vec::new()).unwrap(),
            "IF OBJECT_ID(N'[dbo].[vD]') IS NULL\n    EXEC(N'SET ANSI_NULLS ON; SET QUOTED_IDENTIFIER ON; EXEC(N''CREATE VIEW vD AS SELECT 1 AS a'');');"
        );
    }

    /// EXECUTE AS SELF binds a module to its creator: never the helper user.
    #[test]
    fn execute_as_self_is_never_bound_to_the_helper() {
        assert!(executes_as_self("CREATE PROCEDURE pS WITH EXECUTE AS SELF AS SELECT 1"));
        assert!(executes_as_self("CREATE PROC p\nWITH RECOMPILE, EXEC /* c */ AS\n  self AS SELECT 1"));
        assert!(executes_as_self("CREATE FUNCTION f() RETURNS int WITH EXECUTE AS SELF AS BEGIN RETURN 1 END"));
        assert!(!executes_as_self("CREATE PROCEDURE p WITH EXECUTE AS OWNER AS SELECT 'EXECUTE AS SELF'"));
        assert!(!executes_as_self("CREATE PROCEDURE p AS -- EXECUTE AS SELF\nSELECT [EXECUTE AS SELF] = 1"));
        assert!(!executes_as_self("CREATE PROCEDURE p WITH EXECUTE AS 'self' AS SELECT 1"));
        assert!(!executes_as_self("CREATE PROCEDURE p AS SELECT 'it''s EXECUTE AS SELF' AS x"));

        let taken = HashSet::new();
        let place = Placement { default_schema: "dbo", taken: &taken };
        let mut p = code(1, "P", "app", "pS", "CREATE PROCEDURE pS WITH EXECUTE AS SELF AS SELECT Id FROM T");
        p.execute_as = Some("cv2_u".into());
        let mut notes = Vec::new();
        assert!(module_sql(&p, &place, &mut notes).is_none());
        assert!(notes[0].contains("[app].[pS] (procedimiento): se declara WITH EXECUTE AS SELF") && notes[0].contains("[cv2_u]") && notes[0].contains("No se clona"), "{notes:?}");

        // Qualified: created as the cloning user, which it runs as (said).
        let mut q = code(2, "P", "app", "pQ", "CREATE PROCEDURE app.pQ WITH EXECUTE AS SELF AS SELECT 1");
        q.execute_as = Some("cv2_u".into());
        let mut notes = Vec::new();
        let sql = module_sql(&q, &place, &mut notes).unwrap();
        assert!(!sql.contains(CLONE_USER), "{sql}");
        assert!(notes[0].contains("se ejecuta como el usuario con el que se clona (en el origen, como [cv2_u])"), "{notes:?}");

        // Any module created as the helper drops itself (and the helper) if
        // it still ended up bound to it.
        let v = code(3, "V", "app", "vT", "CREATE VIEW vT AS SELECT Id FROM T");
        let sql = module_sql(&v, &place, &mut Vec::new()).unwrap();
        assert!(sql.contains("IF EXISTS (SELECT 1 FROM sys.sql_modules WHERE execute_as_principal_id = USER_ID(N'dbine_clone_owner'))"), "{sql}");
        assert!(sql.contains("EXEC(N'DROP VIEW [app].[vT]');\n    DROP USER [dbine_clone_owner];\n    THROW 50000"), "{sql}");
    }

    /// A schema-bound function over a table, used by another table's
    /// default: created right before that table, which goes after the one
    /// it reads.
    #[test]
    fn column_functions_over_tables_wait_for_them() {
        let mut item = table("dbo", "Item");
        item.id = 1;
        item.columns.push(Column { default: Some(("DF_Item_V".into(), "([dbo].[fnCfg]())".into())), ..col("V", "int") });
        let mut cfg = table("dbo", "Cfg");
        cfg.id = 2;
        let mut f = code(10, "FN", "dbo", "fnCfg", "CREATE FUNCTION dbo.fnCfg() RETURNS int WITH SCHEMABINDING AS BEGIN RETURN (SELECT V FROM dbo.Cfg) END");
        f.schema_bound = true;
        let src = Source {
            tables: vec![item, cfg],
            modules: vec![f],
            deps: [(10, HashSet::from([2]))].into(),
            column_functions: [(1, vec![ColumnUse { column: "V".into(), default: true, function: 10 }])].into(),
            ..Default::default()
        };
        let script = build(src.clone(), &full_target(), &[tref("dbo", "Item"), tref("dbo", "Cfg")]).unwrap();
        assert_eq!(script.tables.iter().map(|t| t.table.name.as_str()).collect::<Vec<_>>(), ["Cfg", "Item"]);
        let create = &script.tables[1].create;
        let (f_at, t_at) = (create.find("CREATE FUNCTION dbo.fnCfg").unwrap(), create.find("CREATE TABLE [dbo].[Item]").unwrap());
        assert!(f_at < t_at && create.contains("CONSTRAINT [DF_Item_V] DEFAULT ([dbo].[fnCfg]())"), "{create}");
        assert!(script.before.iter().chain(&script.after).all(|s| !s.contains("fnCfg")), "{script:#?}");

        // Not schema-bound: it binds at run time, so it goes before everything.
        let mut loose = src.clone();
        loose.modules[0].schema_bound = false;
        let script = build(loose, &full_target(), &[tref("dbo", "Item"), tref("dbo", "Cfg")]).unwrap();
        assert!(script.before.iter().any(|s| s.contains("CREATE FUNCTION dbo.fnCfg")));
        assert_eq!(script.tables[0].table.name, "Item");

        // The function reads the very table: its default comes after the code.
        let mut own = src.clone();
        own.deps = [(10, HashSet::from([1]))].into();
        let script = build(own, &full_target(), &[tref("dbo", "Item"), tref("dbo", "Cfg")]).unwrap();
        assert!(!script.tables[0].create.contains("DF_Item_V"), "{}", script.tables[0].create);
        let f_at = script.after.iter().position(|s| s.contains("CREATE FUNCTION dbo.fnCfg")).unwrap();
        let df_at = script.after.iter().position(|s| s.contains("ADD CONSTRAINT [DF_Item_V] DEFAULT ([dbo].[fnCfg]()) FOR [V]")).unwrap();
        assert!(f_at < df_at, "{:#?}", script.after);
        assert!(script.after[df_at].starts_with("IF OBJECT_ID(N'[dbo].[DF_Item_V]', 'D') IS NULL"));

        // Cfg isn't cloned: the function can't be created, nor the default (said).
        let script = build(src.clone(), &full_target(), &[tref("dbo", "Item")]).unwrap();
        assert!(!script.tables[0].create.contains("DF_Item_V") && script.after.iter().all(|s| !s.contains("fnCfg")));
        assert!(script.notes.iter().any(|n| n.contains("[dbo].[Item]: el DEFAULT de la columna [V] usa [dbo].[fnCfg]")), "{:?}", script.notes);
        // A computed column can't do without it: refused, saying why.
        let mut computed = src;
        computed.column_functions = [(1, vec![ColumnUse { column: "V".into(), default: false, function: 10 }])].into();
        let err = build(computed, &full_target(), &[tref("dbo", "Item")]).unwrap_err().to_string();
        assert!(err.contains("la columna calculada [V] usa [dbo].[fnCfg]"), "{err}");
    }

    /// WITH EXECUTE AS 'user' binds a module to a user, and users aren't
    /// cloned: left out when the target lacks that user (qualified or not,
    /// before any SQL), said with that cause, and so is what needs it; dbo,
    /// a user the target has, SELF and OWNER are created.
    #[test]
    fn execute_as_a_named_user_is_left_out_only_when_missing() {
        let mut t = table("app", "T");
        t.id = 1;
        let mut pn = code(10, "P", "app", "pN", "CREATE PROCEDURE app.pN WITH EXECUTE AS 'cv3_u' AS SELECT Id FROM app.T");
        pn.execute_as = Some("cv3_u".into());
        let mut pnu = code(11, "P", "app", "pNU", "CREATE PROCEDURE pNU WITH EXECUTE AS 'cv3_u' AS SELECT Id FROM T");
        pnu.execute_as = Some("cv3_u".into());
        let mut f = code(12, "FN", "app", "fN", "CREATE FUNCTION app.fN() RETURNS int WITH EXECUTE AS N'o''k' AS BEGIN RETURN 1 END");
        f.execute_as = Some("o'k".into());
        let v = code(13, "V", "app", "vF", "CREATE VIEW app.vF AS SELECT app.fN() AS x");
        let mut s = code(14, "P", "app", "pS", "CREATE PROCEDURE app.pS WITH EXECUTE AS SELF AS SELECT 1");
        s.execute_as = Some("cv3_u".into());
        let o = code(15, "P", "app", "pO", "CREATE PROCEDURE app.pO WITH EXECUTE AS OWNER AS SELECT 1");
        let mut d = code(16, "P", "app", "pD", "CREATE PROCEDURE app.pD WITH EXECUTE AS 'dbo' AS SELECT 1");
        d.execute_as = Some("dbo".into());
        let mut h = code(17, "P", "app", "pH", "CREATE PROCEDURE app.pH WITH EXECUTE AS 'here_u' AS SELECT 1");
        h.execute_as = Some("here_u".into());
        let src = Source { tables: vec![t], modules: vec![pn, pnu, f, v, s, o, d, h], deps: [(13, HashSet::from([12]))].into(), ..Default::default() };
        let target = Target { users: HashSet::from(["here_u".to_string()]), ..full_target() };
        let script = build(src.clone(), &target, &[tref("app", "T")]).unwrap();
        let sql = script.before.iter().chain(&script.after).chain(script.tables.iter().map(|t| &t.create)).cloned().collect::<Vec<_>>().join("\n");
        for gone in ["pN ", "pNU", "fN", "vF", CLONE_USER] {
            assert!(!sql.contains(gone), "{gone} in:\n{sql}");
        }
        for kept in ["app.pS", "app.pO", "app.pD WITH EXECUTE AS", "app.pH WITH EXECUTE AS"] {
            assert!(sql.contains(kept), "{kept} missing in:\n{sql}");
        }
        let notes = script.notes.join("\n");
        for want in [
            "[app].[pN] (procedimiento): se declara WITH EXECUTE AS 'cv3_u' y no se clona: el usuario «cv3_u» de EXECUTE AS no existe en el destino; crealo y volvé a clonar",
            "[app].[pNU] (procedimiento): se declara WITH EXECUTE AS 'cv3_u' y no se clona",
            "[app].[fN] (función): se declara WITH EXECUTE AS 'o''k' y no se clona: el usuario «o'k» de EXECUTE AS no existe en el destino",
            "[app].[vF] (vista): no se crea porque usa [app].[fN], que no se clona: el usuario «o'k» de EXECUTE AS no existe en el destino; crealo y volvé a clonar",
        ] {
            assert!(notes.contains(want), "missing «{want}» in:\n{notes}");
        }
        assert!(!notes.contains("pD") && !notes.contains("pH") && !notes.contains("tablas u objetos"), "{notes}");

        // With every user on the target, everything comes.
        let all = Target { users: HashSet::from(["here_u".to_string(), "cv3_u".to_string(), "o'k".to_string()]), ..full_target() };
        let script = build(src, &all, &[tref("app", "T")]).unwrap();
        assert!(script.notes.iter().all(|n| !n.contains("EXECUTE AS '")), "{:?}", script.notes);
        let sql = script.after.join("\n");
        for kept in ["pN ", "pNU", "fN", "vF"] {
            assert!(sql.contains(kept), "{kept} missing in:\n{sql}");
        }
    }

    /// A computed column or a DEFAULT over a function left out for its
    /// EXECUTE AS user: the refusal and the note say that cause.
    #[test]
    fn column_functions_left_out_for_their_user_say_so() {
        let mut t = table("dbo", "Item");
        t.columns.push(col("V", "int"));
        let mut f = code(10, "FN", "dbo", "fnU", "CREATE FUNCTION dbo.fnU() RETURNS int WITH EXECUTE AS 'gone_u' AS BEGIN RETURN 1 END");
        f.execute_as = Some("gone_u".into());
        let w = code(11, "FN", "dbo", "fnW", "CREATE FUNCTION dbo.fnW() RETURNS int AS BEGIN RETURN dbo.fnU() END");
        let src = Source {
            tables: vec![t],
            modules: vec![f, w],
            deps: [(11, HashSet::from([10]))].into(),
            column_functions: [(1, vec![ColumnUse { column: "V".into(), default: true, function: 11 }])].into(),
            ..Default::default()
        };
        let script = build(src.clone(), &full_target(), &[tref("dbo", "Item")]).unwrap();
        let notes = script.notes.join("\n");
        assert!(
            notes.contains("[dbo].[Item]: la columna [V] queda sin su DEFAULT porque usa [dbo].[fnW], que depende de [dbo].[fnU], y [dbo].[fnU] no se clona: el usuario «gone_u» de EXECUTE AS no existe en el destino; crealo y volvé a clonar"),
            "{notes}"
        );
        assert!(!notes.contains("tablas u objetos") && !notes.contains("que no se clona: usa"), "{notes}");
        let mut computed = src;
        computed.column_functions = [(1, vec![ColumnUse { column: "V".into(), default: false, function: 10 }])].into();
        let err = build(computed, &full_target(), &[tref("dbo", "Item")]).unwrap_err().to_string();
        assert!(
            err.contains("[dbo].[Item]: la tabla no se puede crear igual porque la columna calculada [V] usa [dbo].[fnU], que no se clona: el usuario «gone_u» de EXECUTE AS no existe en el destino; crealo y volvé a clonar"),
            "{err}"
        );
    }

    /// A CHECK over a function that isn't cloned is left out saying why
    /// (its EXECUTE AS user, or the tables it reads); and a function left
    /// out both for a module's user and for tables says both causes.
    #[test]
    fn checks_and_mixed_causes_say_why() {
        let mut t = table("dbo", "T");
        t.columns.push(col("D", "int"));
        t.checks = vec![
            Check { id: 30, name: "CK_T".into(), definition: "([dbo].[fC]([Id])=(1))".into(), ..Default::default() },
            Check { id: 31, name: "CK_R".into(), definition: "([dbo].[fR]([Id])=(1))".into(), ..Default::default() },
            Check { id: 32, name: "CK_ok".into(), definition: "([Id]>(0))".into(), ..Default::default() },
        ];
        let mut other = table("dbo", "Other");
        other.id = 2;
        let mut fc = code(10, "FN", "dbo", "fC", "CREATE FUNCTION dbo.fC(@x int) RETURNS bit WITH EXECUTE AS 'cv9_u' AS BEGIN RETURN 1 END");
        fc.execute_as = Some("cv9_u".into());
        let fr = code(11, "FN", "dbo", "fR", "CREATE FUNCTION dbo.fR(@x int) RETURNS bit AS BEGIN RETURN (SELECT 1 FROM dbo.Other) END");
        let g = code(12, "FN", "dbo", "g", "CREATE FUNCTION dbo.g() RETURNS int AS BEGIN RETURN dbo.fC(1) + (SELECT 1 FROM dbo.Other) END");
        let src = Source {
            tables: vec![t, other],
            modules: vec![fc, fr, g],
            deps: [(30, HashSet::from([10, 1])), (31, HashSet::from([11])), (11, HashSet::from([2])), (12, HashSet::from([10, 2]))].into(),
            column_functions: [(1, vec![ColumnUse { column: "D".into(), default: true, function: 12 }])].into(),
            ..Default::default()
        };
        let script = build(src, &full_target(), &[tref("dbo", "T")]).unwrap();
        let notes = script.notes.join("\n");
        let user = "el usuario «cv9_u» de EXECUTE AS no existe en el destino; crealo y volvé a clonar";
        for want in [
            format!("[dbo].[T]: la restricción CHECK [CK_T] no se crea porque usa [dbo].[fC], que no se clona: {user}"),
            "[dbo].[T]: la restricción CHECK [CK_R] no se crea porque usa [dbo].[fR], que no se clona (usa tablas u objetos que no se clonan)".into(),
            format!("[dbo].[g] (función): no se crea porque usa [dbo].[fC], que no se clona: {user}; además, [dbo].[g] usa tablas u objetos que no se clonan"),
            format!(
                "[dbo].[T]: la columna [D] queda sin su DEFAULT porque usa [dbo].[g], que depende de [dbo].[fC], y [dbo].[fC] no se clona: {user}; \
                 además, [dbo].[g] usa tablas u objetos que no se clonan"
            ),
        ] {
            assert!(notes.contains(&want), "{want}\n---\n{notes}");
        }
        let after = script.after.join("\n");
        assert!(!after.contains("CK_T") && !after.contains("CK_R") && after.contains("ADD CONSTRAINT [CK_ok]"), "{after}");
    }

    /// The user checked is the one the module's text names (what its
    /// CREATE runs), not the catalog's: renamed on the source since, or
    /// spelled otherwise on a target that tells letter case apart, the
    /// module is left out saying so.
    #[test]
    fn execute_as_checks_the_name_the_text_declares() {
        assert_eq!(execute_as_clause("CREATE PROC p WITH EXECUTE AS N'o''k' AS SELECT 'x'"), Some(ExecAs::User("o'k".into())));
        assert_eq!(
            execute_as_clause("CREATE PROC p WITH RECOMPILE, EXEC AS /* 'no' */ 'CV7_U' AS EXECUTE AS USER = 'z'"),
            Some(ExecAs::User("CV7_U".into()))
        );
        assert_eq!(execute_as_clause("CREATE PROC p WITH EXECUTE AS OWNER AS EXECUTE AS USER = 'z'; SELECT 'EXECUTE AS ''q'' x'"), None);
        assert_eq!(execute_as_clause("CREATE PROC p WITH EXECUTE AS SELF AS SELECT 1"), Some(ExecAs::SelfUser));

        let mut t = table("app", "T");
        t.id = 1;
        let mut r = code(10, "P", "app", "pR", "CREATE PROCEDURE app.pR WITH EXECUTE AS 'cv6_old' AS SELECT 1");
        r.execute_as = Some("cv6_new".into());
        r.execute_as_renamed = true;
        let mut c = code(11, "P", "app", "pC", "CREATE PROCEDURE app.pC WITH EXECUTE AS 'CV7_U' AS SELECT 1");
        c.execute_as = Some("cv7_u".into());
        assert_eq!(executes_as_named(&r).as_deref(), Some("cv6_old"));
        assert_eq!(executes_as_named(&c).as_deref(), Some("CV7_U"));
        let src = Source { tables: vec![t], modules: vec![r, c], ..Default::default() };
        let all_sql = |s: &CloneScript| s.before.iter().chain(&s.after).chain(s.tables.iter().map(|t| &t.create)).cloned().collect::<Vec<_>>().join("\n");

        // The target has the catalog's names only (and tells case apart).
        let target = Target {
            users: HashSet::from(["cv6_new".to_string()]),
            users_spelled: [("CV7_U".to_string(), "cv7_u".to_string())].into(),
            ..full_target()
        };
        let script = build(src.clone(), &target, &[tref("app", "T")]).unwrap();
        let sql = all_sql(&script);
        assert!(!sql.contains("pR") && !sql.contains("pC"), "{sql}");
        let notes = script.notes.join("\n");
        for want in [
            "[app].[pR] (procedimiento): se declara WITH EXECUTE AS 'cv6_old' y no se clona: su texto nombra al usuario «cv6_old», \
             que en el origen ahora se llama «cv6_new»; actualizá el módulo en el origen para que nombre a «cv6_new» y volvé a clonar",
            "[app].[pC] (procedimiento): se declara WITH EXECUTE AS 'CV7_U' y no se clona: el usuario «CV7_U» de EXECUTE AS no existe en el destino, \
             que distingue mayúsculas y minúsculas (ahí está «cv7_u»); corregí el nombre en el módulo del origen o creá ese usuario, y volvé a clonar",
        ] {
            assert!(notes.contains(want), "missing «{want}» in:\n{notes}");
        }

        // A target that takes the text's spelling: pC comes; pR never does
        // (its text names a user the source no longer runs it as).
        let ci = Target { users: HashSet::from(["cv6_new".to_string(), "cv6_old".to_string(), "CV7_U".to_string()]), ..full_target() };
        let script = build(src, &ci, &[tref("app", "T")]).unwrap();
        let sql = all_sql(&script);
        assert!(sql.contains("CREATE PROCEDURE app.pC WITH EXECUTE AS ''''CV7_U''''") && !sql.contains("pR"), "{sql}");
        assert!(script.notes.iter().any(|n| n.contains("[app].[pR]")) && !script.notes.iter().any(|n| n.contains("[app].[pC]")), "{:?}", script.notes);
    }

    /// Delta's untrusted-key mark is DBine's bookkeeping: never cloned.
    #[test]
    fn delta_mark_is_not_cloned() {
        let mut t = table("dbo", "T");
        t.fks = vec![ForeignKey { name: "FK_T".into(), columns: vec!["Id".into()], ref_schema: "dbo".into(), ref_table: "T".into(), ref_columns: vec!["Id".into()], ..Default::default() }];
        let levels = vec![("SCHEMA".into(), "dbo".into()), ("TABLE".into(), "T".into()), ("CONSTRAINT".into(), "FK_T".into())];
        let mark = ExtendedProperty { name: UNTRUSTED_MARK.into(), value: "[dbo].[T]".into(), base_type: "nvarchar".into(), levels: levels.clone() };
        let doc = ExtendedProperty { name: "MS_Description".into(), ..mark.clone() };
        let src = Source { tables: vec![t], properties: vec![mark, doc], ..Default::default() };
        let script = build(src, &full_target(), &[tref("dbo", "T")]).unwrap();
        let sql = script.after.join("\n");
        assert!(!sql.contains(UNTRUSTED_MARK) && sql.contains("N'MS_Description'"), "{sql}");
    }

    #[test]
    fn graph_and_ledger_tables_are_left_out_and_said() {
        let mut node = table("dbo", "Person");
        node.id = 1;
        node.graph = Some("NODE".into());
        let mut edge = table("dbo", "Knows");
        edge.id = 2;
        edge.graph = Some("EDGE".into());
        let mut ledger = table("dbo", "L");
        ledger.id = 3;
        ledger.ledger = 3;
        let mut plain = table("dbo", "P");
        plain.id = 4;
        plain.fks = vec![ForeignKey { name: "FK_P_L".into(), columns: vec!["Id".into()], ref_schema: "dbo".into(), ref_table: "L".into(), ref_columns: vec!["Id".into()], ..Default::default() }];
        let src = Source {
            tables: vec![node, edge, ledger, plain],
            modules: vec![code(10, "V", "dbo", "L_Ledger", "CREATE VIEW [dbo].[L_Ledger] AS SELECT Id FROM [dbo].[L]")],
            deps: [(10, HashSet::from([3]))].into(),
            ..Default::default()
        };
        let script = build(src, &full_target(), &[tref("dbo", "Person"), tref("dbo", "Knows"), tref("dbo", "L"), tref("dbo", "P")]).unwrap();
        assert_eq!(script.tables.iter().map(|t| t.table.name.as_str()).collect::<Vec<_>>(), ["P"]);
        let notes = script.notes.join("\n");
        for want in ["[dbo].[Person]: es una tabla de grafo (AS NODE)", "[dbo].[Knows]: es una tabla de grafo (AS EDGE)", "[dbo].[L]: es una tabla ledger", "[FK_P_L] apunta a dbo.L"] {
            assert!(notes.contains(want), "missing «{want}» in:\n{notes}");
        }
        assert!(script.after.iter().all(|s| !s.contains("L_Ledger")));
    }

    #[test]
    fn indexed_view_statistics_follow_its_indexes() {
        let mut t = table("s", "T");
        t.id = 1;
        let v = code(10, "V", "s", "v", "CREATE VIEW s.v WITH SCHEMABINDING AS SELECT Id, COUNT_BIG(*) AS n FROM s.T GROUP BY Id");
        let src = Source {
            tables: vec![t],
            modules: vec![v],
            deps: [(10, HashSet::from([1]))].into(),
            view_indexes: [(10, vec![Index { unique: true, ..index("CIX", 1, &[("Id", false)]) }])].into(),
            view_stats: [(10, vec![Stat { name: "ST ]v'".into(), columns: vec!["n".into()], filter: None, no_recompute: false }])].into(),
            ..Default::default()
        };
        let script = build(src, &full_target(), &[tref("s", "T")]).unwrap();
        let ix = script.after.iter().position(|s| s.contains("CREATE UNIQUE CLUSTERED INDEX [CIX] ON [s].[v]")).unwrap();
        let st = script.after.iter().position(|s| s.contains("CREATE STATISTICS [ST ]]v'] ON [s].[v] ([n]);")).unwrap();
        assert!(ix < st, "{:#?}", script.after);
    }

    /// Before SQL Server 2017 a sequence still at its start can't say whether
    /// it handed that value out: cloned as unused, and said.
    #[test]
    fn sequence_use_unknown_before_2017_is_said() {
        let sq = Sequence {
            schema: "s".into(),
            name: "q".into(),
            type_sql: "int".into(),
            start: "10".into(),
            increment: "5".into(),
            current: "10".into(),
            used: false,
            unknown_use: true,
            ..Default::default()
        };
        let src = Source { sequences: vec![sq], ..Default::default() };
        let script = build(src, &full_target(), &[]).unwrap();
        assert!(script.after.iter().all(|s| !s.contains("RESTART")));
        assert!(script.notes.iter().any(|n| n.contains("[s].[q]: el origen es anterior a SQL Server 2017") && n.contains("(10 más 5)")), "{:?}", script.notes);
    }

    /// A disabled primary key or unique index is created enabled and
    /// disabled after the foreign keys that reference it.
    #[test]
    fn disabled_keys_go_off_after_the_foreign_keys() {
        let mut parent = table("s", "P");
        parent.id = 1;
        parent.pk = Some(Index { primary: true, disabled: true, ..index("PK_P", 2, &[("Id", false)]) });
        parent.indexes = vec![
            Index { unique: true, unique_constraint: true, disabled: true, ..index("UQ_P", 2, &[("Code", false)]) },
            Index { disabled: true, ..index("IX_P", 2, &[("Code", false)]) },
        ];
        let mut child = table("s", "C");
        child.id = 2;
        child.fks = vec![ForeignKey { name: "FK_C_P".into(), columns: vec!["Id".into()], ref_schema: "s".into(), ref_table: "P".into(), ref_columns: vec!["Id".into()], disabled: true, not_trusted: true, ..Default::default() }];
        let src = Source { tables: vec![parent, child], ..Default::default() };
        let script = build(src, &full_target(), &[tref("s", "P"), tref("s", "C")]).unwrap();
        let p = &script.tables[0];
        assert!(p.create.contains("CONSTRAINT [PK_P] PRIMARY KEY NONCLUSTERED"), "{}", p.create);
        assert!(p.after_data.iter().any(|s| s.contains("ADD CONSTRAINT [UQ_P] UNIQUE") && !s.contains("DISABLE")), "{:#?}", p.after_data);
        assert!(p.after_data.iter().any(|s| s.contains("ALTER INDEX [IX_P] ON [s].[P] DISABLE")), "{:#?}", p.after_data);
        let a = &script.after;
        let fk = a.iter().position(|s| s.contains("CONSTRAINT [FK_C_P] FOREIGN KEY")).unwrap();
        let pk = a.iter().position(|s| s.contains("ALTER INDEX [PK_P] ON [s].[P] DISABLE")).unwrap();
        let uq = a.iter().position(|s| s.contains("ALTER INDEX [UQ_P] ON [s].[P] DISABLE")).unwrap();
        assert!(fk < pk && fk < uq, "{a:#?}");
        assert!(a.iter().all(|s| !s.contains("[IX_P]")));
    }

    #[test]
    fn ordered_nonclustered_columnstore() {
        let ncci = Index { columns: vec!["Qty".into(), "Day".into()], cs_order: vec!["Day".into()], ..index("NCCI", 6, &[]) };
        assert!(create_index("[s].[t]", &ncci).contains("CREATE NONCLUSTERED COLUMNSTORE INDEX [NCCI] ON [s].[t] ([Qty], [Day]) ORDER ([Day]);"));
        let mut t = table("s", "t");
        t.indexes = vec![ncci];
        let mut list = vec![t];
        let mut notes = Vec::new();
        // SQL Server 2022: ordered clustered columnstore only.
        let t2022 = Target { caps: Caps { ordered_columnstore: true, sequential_key: true, compression_delay: true, ..Default::default() }, ..Default::default() };
        adapt_features(&mut list, &mut HashMap::new(), &HashMap::new(), &t2022, &mut notes);
        assert!(list[0].indexes[0].cs_order.is_empty() && notes.len() == 1 && notes[0].contains("no agrupado ordenado") && notes[0].contains("[s].[t].NCCI"), "{notes:?}");
    }
}
