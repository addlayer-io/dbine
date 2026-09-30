//! One [`Dialect`] per engine family: how it names types, spells defaults
//! and what its tables can hold. Driver ids map to a dialect in
//! [`for_driver`]; variants of an engine share it (Aurora, AlloyDB and
//! Cloud SQL speak PostgreSQL).

use crate::default::DefaultValue;
use crate::issue::{IssueCode, Report, Severity};
use crate::logical::LogicalType;
use crate::parse::TypeSpec;
use dbine_driver::TableSchema;

mod bigquery;
mod cassandra;
mod clickhouse;
mod cosmosdb;
mod couchbase;
mod couchdb;
mod databend;
mod db2;
mod dremio;
mod duckdb;
mod dynamodb;
mod elasticsearch;
mod exasol;
mod firebird;
mod graph;
mod greptimedb;
mod hana;
mod influxdb;
mod informix;
mod iotdb;
mod ksqldb;
mod manticore;
mod mongodb;
mod mssql;
mod mysql;
mod netezza;
mod odbc_engines;
mod oracle;
mod phoenix;
mod postgres;
mod snowflake;
mod solr;
mod spanner;
mod spark;
mod sqlite;
mod starrocks;
mod sybase;
mod tdengine;
mod teradata;
mod trino;
mod vertica;

/// A native type chosen for a logical one, and what didn't carry over.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    pub native: String,
    /// Empty when the target type holds every value of the logical one.
    pub notes: Vec<Note>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Note {
    pub severity: Severity,
    pub code: IssueCode,
    pub message: String,
}

impl Rendered {
    pub fn exact(native: impl Into<String>) -> Self {
        Self { native: native.into(), notes: Vec::new() }
    }

    pub fn with(mut self, severity: Severity, code: IssueCode, message: impl Into<String>) -> Self {
        self.notes.push(Note { severity, code, message: message.into() });
        self
    }
}

/// How unquoted identifiers are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentCase {
    /// Folded to lower case (PostgreSQL).
    Lower,
    /// Folded to upper case (Oracle, Db2, Firebird, Snowflake).
    Upper,
    /// Kept as written (SQL Server, MySQL on most systems, SQLite).
    Preserve,
}

/// What a family's tables can hold. The IDE narrows it further with the
/// target driver's `DesignerSpec` ([`Caps::narrow`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Caps {
    pub foreign_keys: bool,
    /// `ON DELETE` actions it accepts, upper case.
    pub on_delete: &'static [&'static str],
    /// `ON UPDATE` actions it accepts (empty: no ON UPDATE clause).
    pub on_update: &'static [&'static str],
    pub indexes: bool,
    /// Partial (filtered) indexes.
    pub partial_indexes: bool,
    /// `INCLUDE (…)` columns on indexes.
    pub supports_include: bool,
    pub auto_increment: bool,
    pub defaults: bool,
    /// NOT NULL columns.
    pub nullability: bool,
    pub comments: bool,
    /// Longest identifier, in bytes.
    pub max_identifier: usize,
    pub case: IdentCase,
}

pub const ALL_ACTIONS: &[&str] = &["CASCADE", "SET NULL", "SET DEFAULT", "RESTRICT", "NO ACTION"];

impl Caps {
    /// Switch off what the driver's designer says the engine lacks.
    pub fn narrow(mut self, spec: &dbine_driver::DesignerSpec) -> Self {
        self.foreign_keys &= spec.foreign_keys;
        self.indexes &= spec.indexes;
        self.auto_increment &= spec.auto_increment;
        self.defaults &= spec.defaults;
        self.nullability &= spec.nullability;
        self.comments &= spec.comments;
        self
    }
}

pub trait Dialect: Send + Sync {
    /// Family id (`postgres`, `mysql`, `mssql`…).
    fn id(&self) -> &'static str;

    /// Classify a native type. Never fails: unknown names come back as
    /// [`LogicalType::Other`].
    fn parse_type(&self, t: &TypeSpec) -> LogicalType;

    /// Native spelling for a logical type in this family.
    fn render_type(&self, t: &LogicalType) -> Rendered;

    /// Native spelling for a default, or `None` when the family has no way
    /// to express it for a column of type `ty`.
    fn render_default(&self, d: &DefaultValue, ty: &LogicalType) -> Option<String>;

    fn caps(&self) -> Caps;

    /// Whether a column typed `t` is an auto-increment column by its type
    /// alone (`serial`, `bigserial`, SQL Server `identity` modifier).
    fn implies_auto_increment(&self, t: &TypeSpec) -> bool {
        let _ = t;
        false
    }

    /// Why tables can't be created on this engine as a conversion target
    /// (a read-only service, no DDL with columns), or `None` when they can.
    /// Such engines still work as a source.
    fn target_refusal(&self, driver_id: &str) -> Option<&'static str> {
        let _ = driver_id;
        None
    }

    /// Final touches: table options the family needs (ClickHouse ENGINE and
    /// ORDER BY, Cassandra partition key…) from what's already there.
    fn finalize(&self, t: &mut TableSchema, report: &mut Report) {
        let _ = (t, report);
    }
}

/// Whether an index of the target accepts `INCLUDE (…)` columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncludeSupport {
    No,
    Yes,
    /// Only unique indexes (Db2 for LUW and z/OS).
    UniqueOnly,
}

/// `INCLUDE` support of the engine behind a driver id. The dialect's
/// [`Caps::supports_include`] is the default (`caps_default`, which the IDE
/// may have narrowed); engines that share a dialect but differ are decided
/// here by id.
///
/// - PostgreSQL family: yes where the engine tracks PostgreSQL 11+
///   (PostgreSQL, TimescaleDB, EDB, Fujitsu, AlloyDB, Cloud SQL, Aurora,
///   Aurora DSQL, YugabyteDB, Greenplum 7, Cloudberry). No for Redshift and
///   Yellowbrick (no user indexes), CrateDB, H2, Materialize, RisingWave,
///   Denodo (no such indexes), CockroachDB (`STORING`, which the driver's
///   DDL doesn't write), openGauss, KingbaseES and Greengage (PostgreSQL
///   9.x-based lines, not confirmed to have it).
/// - SQL Server family: Fabric warehouses have no secondary indexes.
/// - Db2: LUW and z/OS only on unique indexes; Db2 for i has none.
pub fn include_support(driver_id: &str, caps_default: bool) -> IncludeSupport {
    match driver_id {
        // openGauss: only on ubtree (Ustore) indexes. Greenplum: only from 7,
        // and the id doesn't say which. CockroachDB writes it as STORING.
        "redshift" | "yellowbrick" | "cratedb" | "h2" | "materialize" | "denodo" | "opengauss" | "greengage"
        | "greenplum" | "fabric" | "db2i" => IncludeSupport::No,
        "db2" | "db2zos" => IncludeSupport::UniqueOnly,
        _ if caps_default => IncludeSupport::Yes,
        _ => IncludeSupport::No,
    }
}

/// The dialect that serves a driver id, if the crate knows its family.
/// Each module answers for its own ids.
pub fn for_driver(driver_id: &str) -> Option<&'static dyn Dialect> {
    LOOKUPS.iter().find_map(|f| f(driver_id))
}

type Lookup = fn(&str) -> Option<&'static dyn Dialect>;

const LOOKUPS: &[Lookup] = &[
    core,
    bigquery::lookup,
    cassandra::lookup,
    clickhouse::lookup,
    cosmosdb::lookup,
    couchbase::lookup,
    couchdb::lookup,
    databend::lookup,
    db2::lookup,
    dremio::lookup,
    duckdb::lookup,
    dynamodb::lookup,
    elasticsearch::lookup,
    exasol::lookup,
    firebird::lookup,
    graph::lookup,
    greptimedb::lookup,
    hana::lookup,
    influxdb::lookup,
    informix::lookup,
    iotdb::lookup,
    ksqldb::lookup,
    manticore::lookup,
    mongodb::lookup,
    netezza::lookup,
    odbc_engines::lookup,
    phoenix::lookup,
    snowflake::lookup,
    solr::lookup,
    spanner::lookup,
    spark::lookup,
    starrocks::lookup,
    sybase::lookup,
    tdengine::lookup,
    teradata::lookup,
    trino::lookup,
    vertica::lookup,
];

/// The five families the conversion started with.
fn core(driver_id: &str) -> Option<&'static dyn Dialect> {
    static PG: postgres::Postgres = postgres::Postgres;
    static MY: mysql::MySql = mysql::MySql;
    static MS: mssql::MsSql = mssql::MsSql;
    static OR: oracle::Oracle = oracle::Oracle;
    static LITE: sqlite::Sqlite = sqlite::Sqlite;
    Some(match driver_id {
        "postgres" | "timescaledb" | "yugabytedb" | "kingbase" | "alloydb" | "cloudsql_postgres" | "aurora_postgres"
        | "edb" | "fujitsu" | "opengauss" | "cockroachdb" | "greenplum" | "cloudberry" | "greengage" | "dsql"
        | "yellowbrick" | "redshift" | "risingwave" | "materialize" | "cratedb" | "h2" | "denodo" => &PG,
        "mysql" | "mariadb" | "tidb" | "oceanbase" | "singlestore" | "aurora-mysql" | "cloudsql-mysql" => &MY,
        "sqlserver" | "azuresql" | "fabric" | "babelfish" => &MS,
        "oracle" | "oracle_adb" => &OR,
        "sqlite" | "libsql" => &LITE,
        _ => return None,
    })
}

/// Shared renderer for defaults most SQL engines spell the same way.
pub(crate) fn standard_default(d: &DefaultValue, ty: &LogicalType, now: &str, uuid: Option<&str>, bool_as_int: bool) -> Option<String> {
    Some(match d {
        DefaultValue::Null => "NULL".into(),
        DefaultValue::Number(n) => n.clone(),
        DefaultValue::Text(s) => crate::default::quote(s),
        DefaultValue::Bool(b) if bool_as_int || !matches!(ty, LogicalType::Bool) => if *b { "1" } else { "0" }.into(),
        DefaultValue::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        DefaultValue::CurrentTimestamp => now.into(),
        DefaultValue::CurrentDate => "CURRENT_DATE".into(),
        DefaultValue::CurrentTime => "CURRENT_TIME".into(),
        DefaultValue::NewUuid => uuid?.into(),
        DefaultValue::NextVal(_) | DefaultValue::Expr(_) => return None,
    })
}
