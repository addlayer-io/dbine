//! Table designer, DDL, INSERT scripts and create templates per preset.
//!
//! Most engines go through the shared [`ddl`] builder with their own
//! [`SqlFlavor`]; the rest adjust its output (Sybase ASE identity, Informix
//! SERIAL and constraint names, Teradata MULTISET / PRIMARY INDEX) or write
//! their own (Hive and Impala).

use crate::presets::Preset;
use dbine_driver::ddl::{self, AutoIncrement, SqlFlavor};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{
    kinds, Capabilities, CreateTemplate, DdlParts, DesignerSpec, Error, Field, FieldKind, Result, RowChange, TableSchema,
};
use dbine_driver::filter::{insert_where, sql_condition, ColumnFilter, FilterOp, SqlFilterStyle};
use serde_json::Value;

/// The engine behind a preset, for the parts that differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eng {
    Generic,
    Db2,
    Db2i,
    Db2zos,
    Ase,
    Sqla,
    Hive,
    Impala,
    /// Informix and GBase 8s.
    Informix,
    Teradata,
    Vertica,
    Exasol,
    Netezza,
    Altibase,
    Cubrid,
    Dameng,
    Ocient,
    /// Spark Thrift Server and Kyuubi (Hive DDL, Spark SQL).
    Spark,
    MonetDb,
    Virtuoso,
    Ingres,
    Mimer,
    /// InterSystems IRIS and Caché.
    Iris,
    OpenEdge,
    Zen,
    Sqream,
    MaxDb,
    Access,
    DBase,
    NuoDb,
    HeavyDb,
    Machbase,
    Ignite,
    Ignite3,
    NetSuite,
    /// Only behind the generic preset (the monitor goes by the DBMS name).
    SqlServer,
}

pub fn eng(p: &Preset) -> Eng {
    match p.id {
        "db2" => Eng::Db2,
        "db2i" => Eng::Db2i,
        "db2zos" => Eng::Db2zos,
        "sybase" => Eng::Ase,
        "sqlanywhere" => Eng::Sqla,
        "hive" => Eng::Hive,
        "impala" => Eng::Impala,
        "informix" | "gbase8s" => Eng::Informix,
        "teradata" => Eng::Teradata,
        "vertica" => Eng::Vertica,
        "exasol" => Eng::Exasol,
        "netezza" => Eng::Netezza,
        "altibase" => Eng::Altibase,
        "cubrid" => Eng::Cubrid,
        "dameng" => Eng::Dameng,
        "ocient" => Eng::Ocient,
        "spark" | "kyuubi" => Eng::Spark,
        "cloudera" => Eng::Hive,
        "monetdb" => Eng::MonetDb,
        "virtuoso" => Eng::Virtuoso,
        "ingres" => Eng::Ingres,
        "mimer" => Eng::Mimer,
        "iris" | "cache" => Eng::Iris,
        "openedge" => Eng::OpenEdge,
        "zen" => Eng::Zen,
        "sqream" => Eng::Sqream,
        "maxdb" => Eng::MaxDb,
        "access" => Eng::Access,
        "dbase" => Eng::DBase,
        "nuodb" => Eng::NuoDb,
        "heavydb" => Eng::HeavyDb,
        "machbase" => Eng::Machbase,
        "ignite" => Eng::Ignite,
        "ignite3" => Eng::Ignite3,
        "netsuite" => Eng::NetSuite,
        _ => Eng::Generic,
    }
}

/// Quoting outside a session (DDL and scripts): the preset's, or ANSI
/// double quotes for the generic preset.
pub fn quote(p: &Preset) -> Quote {
    p.quote.unwrap_or(Quote::Double)
}

pub fn flavor(p: &Preset) -> SqlFlavor {
    let base = SqlFlavor { quote: quote(p), ..SqlFlavor::ansi() };
    let ones = SqlFlavor { true_literal: "1", false_literal: "0", ..base };
    match eng(p) {
        // Unknown engine: nothing beyond SQL-92. Identity columns keep the
        // type name the driver reports (`int identity` on SQL Server).
        Eng::Generic | Eng::SqlServer | Eng::NetSuite => {
            SqlFlavor { auto_increment: AutoIncrement::None, comment_on: false, if_exists: false, ..ones }
        }
        Eng::Db2 | Eng::Db2i => SqlFlavor { if_exists: false, ..base },
        Eng::Db2zos => SqlFlavor { if_exists: false, multi_row_insert: false, ..ones },
        Eng::Ase => SqlFlavor {
            auto_increment: AutoIncrement::None,
            comment_on: false,
            if_exists: false,
            multi_row_insert: false,
            ..ones
        },
        Eng::Sqla => SqlFlavor { auto_increment: AutoIncrement::None, multi_row_insert: false, ..ones },
        Eng::Hive | Eng::Impala | Eng::Spark => SqlFlavor { auto_increment: AutoIncrement::None, comment_on: false, ..base },
        Eng::Informix => SqlFlavor {
            auto_increment: AutoIncrement::None,
            comment_on: false,
            multi_row_insert: false,
            true_literal: "'t'",
            false_literal: "'f'",
            ..base
        },
        Eng::Teradata => SqlFlavor { if_exists: false, multi_row_insert: false, ..ones },
        Eng::Vertica => SqlFlavor { auto_increment: AutoIncrement::None, comment_on: false, multi_row_insert: false, ..base },
        Eng::Exasol => SqlFlavor { auto_increment: AutoIncrement::None, ..base },
        Eng::Netezza => SqlFlavor { auto_increment: AutoIncrement::None, multi_row_insert: false, ..base },
        Eng::Altibase => SqlFlavor { auto_increment: AutoIncrement::None, if_exists: false, multi_row_insert: false, ..ones },
        Eng::Cubrid => SqlFlavor {
            auto_increment: AutoIncrement::AutoIncrementKeyword,
            comment_on: false,
            inline_comments: true,
            ..ones
        },
        Eng::Dameng => SqlFlavor { auto_increment: AutoIncrement::Identity, if_exists: false, ..ones },
        Eng::Ocient => SqlFlavor { auto_increment: AutoIncrement::None, comment_on: false, ..base },
        Eng::MonetDb => SqlFlavor { auto_increment: AutoIncrement::AutoIncrementKeyword, ..base },
        // Identity goes into the type (see `table_ddl`).
        Eng::Virtuoso | Eng::Iris | Eng::Zen | Eng::OpenEdge | Eng::Machbase => SqlFlavor {
            auto_increment: AutoIncrement::None,
            comment_on: false,
            if_exists: false,
            multi_row_insert: false,
            ..ones
        },
        Eng::Ingres => SqlFlavor { if_exists: false, multi_row_insert: false, ..base },
        Eng::Mimer => SqlFlavor { auto_increment: AutoIncrement::None, if_exists: false, multi_row_insert: false, ..base },
        Eng::Sqream => SqlFlavor { auto_increment: AutoIncrement::None, comment_on: false, ..base },
        Eng::MaxDb => SqlFlavor { auto_increment: AutoIncrement::None, if_exists: false, multi_row_insert: false, ..base },
        Eng::Access | Eng::DBase => SqlFlavor {
            auto_increment: AutoIncrement::None,
            comment_on: false,
            if_exists: false,
            multi_row_insert: false,
            ..base
        },
        Eng::NuoDb => SqlFlavor { comment_on: false, ..base },
        Eng::HeavyDb => SqlFlavor { auto_increment: AutoIncrement::None, comment_on: false, multi_row_insert: false, ..base },
        Eng::Ignite | Eng::Ignite3 => SqlFlavor { auto_increment: AutoIncrement::None, comment_on: false, ..base },
    }
}

/// The type name the catalog reports carries the identity marker
/// (`int identity`, `serial`): drop it when the flavor writes its own.
pub fn strips_identity_suffix(p: &Preset) -> bool {
    eng(p) != Eng::Generic
}

pub fn capabilities(p: &Preset) -> Capabilities {
    // Only where "database" is what the explorer lists and SQL creates it.
    let db = matches!(eng(p), Eng::Generic | Eng::Ase | Eng::Netezza);
    Capabilities {
        create_database: db,
        drop_database: db,
        foreign_keys: reports_foreign_keys(p),
        monitor: crate::monitor::supports(p),
        blocking: crate::blocking::supports_blocking(p),
        kill_session: crate::blocking::supports_kill(p),
        processes: false,
        cancel_query: false,
    }
}

/// Foreign keys exist in the engine (enforced or informational) and the
/// ODBC catalog reports them.
pub fn reports_foreign_keys(p: &Preset) -> bool {
    !matches!(
        eng(p),
        Eng::Hive
            | Eng::Impala
            | Eng::Ocient
            | Eng::Spark
            | Eng::Sqream
            | Eng::HeavyDb
            | Eng::Machbase
            | Eng::Ignite
            | Eng::Ignite3
            | Eng::NetSuite
            | Eng::DBase
    )
}

/// The engine has user-defined indexes.
pub fn has_indexes(p: &Preset) -> bool {
    !matches!(
        eng(p),
        Eng::Hive
            | Eng::Impala
            | Eng::Vertica
            | Eng::Exasol
            | Eng::Netezza
            | Eng::Spark
            | Eng::Sqream
            | Eng::HeavyDb
            | Eng::NetSuite
    )
}

pub fn script_separator(p: &Preset) -> &'static str {
    match eng(p) {
        Eng::Ase | Eng::Sqla => "GO",
        _ => "",
    }
}

fn select(key: &'static str, label: &'static str, options: Vec<(&'static str, &'static str)>, help: &'static str) -> Field {
    Field::new(key, label, FieldKind::Select(options)).help(help)
}

fn text(key: &'static str, label: &'static str, placeholder: &'static str, help: &'static str) -> Field {
    Field::new(key, label, FieldKind::Text).placeholder(placeholder).help(help)
}

fn location() -> Field {
    text("location", "Ubicación (LOCATION)", "hdfs:///ruta/tabla", "Directorio de los datos. Vacío: el del warehouse.")
}

pub fn designer(p: &Preset) -> DesignerSpec {
    let e = eng(p);
    let types: Vec<&'static str> = match e {
        Eng::Generic | Eng::SqlServer | Eng::NetSuite => vec![
            "INTEGER", "SMALLINT", "BIGINT", "DECIMAL(18,2)", "NUMERIC(18,2)", "REAL", "DOUBLE PRECISION", "FLOAT",
            "CHAR(10)", "VARCHAR(255)", "DATE", "TIME", "TIMESTAMP",
        ],
        Eng::Db2 | Eng::Db2i | Eng::Db2zos => vec![
            "INTEGER", "SMALLINT", "BIGINT", "DECIMAL(18,2)", "DECFLOAT", "REAL", "DOUBLE", "CHAR(10)", "VARCHAR(255)",
            "CLOB", "GRAPHIC(10)", "VARGRAPHIC(255)", "BLOB", "VARBINARY(255)", "DATE", "TIME", "TIMESTAMP", "BOOLEAN",
            "XML",
        ],
        Eng::Ase => vec![
            "int", "smallint", "tinyint", "bigint", "numeric(18,0)", "decimal(18,2)", "money", "float", "real", "char(10)",
            "varchar(255)", "univarchar(255)", "text", "unitext", "binary(16)", "varbinary(255)", "image", "bit", "date",
            "time", "datetime", "bigdatetime",
        ],
        Eng::Sqla => vec![
            "INTEGER", "SMALLINT", "TINYINT", "BIGINT", "NUMERIC(18,2)", "DECIMAL(18,2)", "MONEY", "DOUBLE", "REAL",
            "CHAR(10)", "VARCHAR(255)", "NVARCHAR(255)", "LONG VARCHAR", "BINARY(16)", "VARBINARY(255)",
            "LONG BINARY", "BIT", "DATE", "TIME", "TIMESTAMP", "UNIQUEIDENTIFIER", "XML",
        ],
        Eng::Hive | Eng::Spark => vec![
            "INT", "TINYINT", "SMALLINT", "BIGINT", "DECIMAL(18,2)", "FLOAT", "DOUBLE", "STRING", "VARCHAR(255)",
            "CHAR(10)", "BOOLEAN", "BINARY", "DATE", "TIMESTAMP", "ARRAY<STRING>", "MAP<STRING,STRING>",
            "STRUCT<a:INT,b:STRING>",
        ],
        Eng::Impala => vec![
            "INT", "TINYINT", "SMALLINT", "BIGINT", "DECIMAL(18,2)", "FLOAT", "DOUBLE", "STRING", "VARCHAR(255)",
            "CHAR(10)", "BOOLEAN", "BINARY", "DATE", "TIMESTAMP",
        ],
        Eng::Informix => vec![
            "INTEGER", "SMALLINT", "BIGINT", "INT8", "SERIAL", "BIGSERIAL", "DECIMAL(18,2)", "MONEY(16,2)", "FLOAT",
            "SMALLFLOAT", "CHAR(10)", "VARCHAR(255)", "LVARCHAR(2048)", "NCHAR(10)", "NVARCHAR(255)", "TEXT", "BYTE",
            "CLOB", "BLOB", "BOOLEAN", "DATE", "DATETIME YEAR TO SECOND", "DATETIME YEAR TO FRACTION(3)",
            "INTERVAL DAY TO SECOND",
        ],
        Eng::Teradata => vec![
            "INTEGER", "SMALLINT", "BYTEINT", "BIGINT", "DECIMAL(18,2)", "NUMBER", "FLOAT", "CHAR(10)",
            "VARCHAR(255)", "VARCHAR(255) CHARACTER SET UNICODE", "CLOB", "BYTE(16)", "VARBYTE(255)", "BLOB", "DATE",
            "TIME", "TIMESTAMP(6)", "INTERVAL DAY TO SECOND", "JSON", "XML",
        ],
        Eng::Vertica => vec![
            "INTEGER", "BIGINT", "NUMERIC(18,2)", "FLOAT", "CHAR(10)", "VARCHAR(255)", "LONG VARCHAR", "BOOLEAN",
            "BINARY(16)", "VARBINARY(255)", "LONG VARBINARY", "DATE", "TIME", "TIMESTAMP", "TIMESTAMPTZ", "INTERVAL",
            "UUID",
        ],
        Eng::Exasol => vec![
            "DECIMAL(18,0)", "DECIMAL(18,2)", "INTEGER", "BIGINT", "DOUBLE PRECISION", "CHAR(10)", "VARCHAR(2000000)",
            "VARCHAR(255)", "BOOLEAN", "DATE", "TIMESTAMP", "TIMESTAMP WITH LOCAL TIME ZONE",
            "INTERVAL DAY TO SECOND", "GEOMETRY", "HASHTYPE",
        ],
        Eng::Netezza => vec![
            "INTEGER", "SMALLINT", "BYTEINT", "BIGINT", "NUMERIC(18,2)", "REAL", "DOUBLE PRECISION", "CHAR(10)",
            "VARCHAR(255)", "NCHAR(10)", "NVARCHAR(255)", "BOOLEAN", "DATE", "TIME", "TIMESTAMP", "INTERVAL",
            "VARBINARY(255)",
        ],
        Eng::Altibase => vec![
            "INTEGER", "SMALLINT", "BIGINT", "NUMERIC(18,2)", "NUMBER", "FLOAT", "DOUBLE", "REAL", "CHAR(10)",
            "VARCHAR(255)", "NCHAR(10)", "NVARCHAR(255)", "CLOB", "BLOB", "BYTE(16)", "VARBYTE(255)", "DATE",
        ],
        Eng::Cubrid => vec![
            "INTEGER", "SMALLINT", "BIGINT", "NUMERIC(18,2)", "FLOAT", "DOUBLE", "MONETARY", "CHAR(10)", "VARCHAR(255)",
            "STRING", "BIT VARYING(256)", "CLOB", "BLOB", "DATE", "TIME", "DATETIME", "TIMESTAMP", "ENUM('a','b')",
            "JSON",
        ],
        Eng::Dameng => vec![
            "INT", "SMALLINT", "TINYINT", "BIGINT", "NUMBER(18,2)", "DECIMAL(18,2)", "FLOAT", "DOUBLE", "CHAR(10)",
            "VARCHAR(255)", "VARCHAR2(255)", "TEXT", "CLOB", "BLOB", "BIT", "DATE", "TIME", "TIMESTAMP", "DATETIME",
        ],
        Eng::Ocient => vec![
            "INT", "SMALLINT", "TINYINT", "BIGINT", "DECIMAL(18,2)", "FLOAT", "DOUBLE", "CHAR(10)", "VARCHAR(255)",
            "BOOLEAN", "BINARY(16)", "VARBINARY(255)", "DATE", "TIME", "TIMESTAMP", "UUID", "IP", "POINT",
        ],
        Eng::MonetDb => vec![
            "INT", "SMALLINT", "TINYINT", "BIGINT", "HUGEINT", "DECIMAL(18,2)", "REAL", "DOUBLE", "BOOLEAN", "CHAR(10)",
            "VARCHAR(255)", "CLOB", "BLOB", "DATE", "TIME", "TIMESTAMP", "TIMESTAMP WITH TIME ZONE",
            "INTERVAL SECOND", "UUID", "JSON", "INET", "URL",
        ],
        Eng::Virtuoso => vec![
            "INTEGER", "SMALLINT", "BIGINT", "DECIMAL(18,2)", "REAL", "DOUBLE PRECISION", "CHAR(10)", "VARCHAR(255)",
            "NVARCHAR(255)", "LONG VARCHAR", "LONG NVARCHAR", "VARBINARY(255)", "LONG VARBINARY", "DATE", "TIME",
            "DATETIME", "TIMESTAMP", "ANY",
        ],
        Eng::Ingres => vec![
            "INTEGER", "SMALLINT", "TINYINT", "BIGINT", "DECIMAL(18,2)", "FLOAT", "MONEY", "CHAR(10)", "VARCHAR(255)",
            "NCHAR(10)", "NVARCHAR(255)", "LONG VARCHAR", "BYTE(16)", "VARBYTE(255)", "LONG BYTE", "BOOLEAN",
            "ANSIDATE", "TIME", "TIMESTAMP", "INTERVAL DAY TO SECOND",
        ],
        Eng::Mimer => vec![
            "INTEGER", "SMALLINT", "BIGINT", "DECIMAL(18,2)", "REAL", "DOUBLE PRECISION", "FLOAT", "CHAR(10)",
            "VARCHAR(255)", "NCHAR(10)", "NVARCHAR(255)", "CLOB", "NCLOB", "BINARY(16)", "VARBINARY(255)", "BLOB",
            "BOOLEAN", "DATE", "TIME", "TIMESTAMP", "INTERVAL DAY TO SECOND",
        ],
        Eng::Iris => vec![
            "INTEGER", "SMALLINT", "TINYINT", "BIGINT", "NUMERIC(18,2)", "DOUBLE", "VARCHAR(255)", "LONGVARCHAR",
            "VARBINARY(255)", "LONGVARBINARY", "BIT", "DATE", "TIME", "TIMESTAMP", "POSIXTIME",
        ],
        Eng::OpenEdge => vec![
            "INTEGER", "SMALLINT", "TINYINT", "BIGINT", "NUMERIC(18,2)", "REAL", "FLOAT", "DOUBLE PRECISION",
            "CHARACTER(10)", "VARCHAR(255)", "LVARCHAR", "CLOB", "BLOB", "BIT", "DATE", "TIME", "TIMESTAMP",
            "TIMESTAMP WITH TIME ZONE", "VARBINARY(255)",
        ],
        Eng::Zen => vec![
            "INTEGER", "SMALLINT", "TINYINT", "BIGINT", "UBIGINT", "DECIMAL(18,2)", "NUMERIC(18,2)", "REAL", "DOUBLE",
            "MONEY", "CHAR(10)", "VARCHAR(255)", "NVARCHAR(255)", "LONGVARCHAR", "BINARY(16)", "LONGVARBINARY", "BIT",
            "DATE", "TIME", "TIMESTAMP", "DATETIME", "UNIQUEIDENTIFIER", "AUTOTIMESTAMP",
        ],
        Eng::Sqream => vec![
            "BOOL", "TINYINT", "SMALLINT", "INT", "BIGINT", "NUMERIC(18,2)", "REAL", "DOUBLE", "TEXT", "TEXT(255)",
            "DATE", "DATETIME", "DATETIME2",
        ],
        Eng::MaxDb => vec![
            "INTEGER", "SMALLINT", "FIXED(18,2)", "FLOAT(38)", "CHAR(10)", "VARCHAR(255)", "CHAR(10) BYTE",
            "VARCHAR(255) UNICODE", "LONG", "LONG UNICODE", "LONG BYTE", "BOOLEAN", "DATE", "TIME", "TIMESTAMP",
        ],
        Eng::Access => vec![
            "LONG", "INTEGER", "SHORT", "BYTE", "DECIMAL(18,2)", "CURRENCY", "DOUBLE", "SINGLE", "TEXT(255)",
            "LONGTEXT", "YESNO", "DATETIME", "GUID", "LONGBINARY",
        ],
        Eng::DBase => vec!["CHAR(10)", "VARCHAR(254)", "NUMERIC(10,2)", "FLOAT", "DATE", "BIT", "MEMO"],
        Eng::NuoDb => vec![
            "INTEGER", "SMALLINT", "BIGINT", "DECIMAL(18,2)", "DOUBLE", "BOOLEAN", "CHAR(10)", "VARCHAR(255)", "STRING",
            "CLOB", "BLOB", "BINARY(16)", "VARBINARY(255)", "DATE", "TIME", "TIMESTAMP",
        ],
        Eng::HeavyDb => vec![
            "SMALLINT", "INTEGER", "BIGINT", "DECIMAL(18,2)", "FLOAT", "DOUBLE", "BOOLEAN", "TEXT ENCODING DICT(32)",
            "TEXT ENCODING NONE", "DATE", "TIME", "TIMESTAMP", "POINT", "LINESTRING", "POLYGON", "MULTIPOLYGON",
        ],
        Eng::Machbase => vec![
            "SHORT", "INTEGER", "LONG", "FLOAT", "DOUBLE", "VARCHAR(255)", "TEXT", "BINARY", "DATETIME", "IPV4", "IPV6",
            "JSON",
        ],
        Eng::Ignite | Eng::Ignite3 => vec![
            "INT", "SMALLINT", "TINYINT", "BIGINT", "DECIMAL(18,2)", "REAL", "DOUBLE", "BOOLEAN", "CHAR(10)",
            "VARCHAR(255)", "VARBINARY(255)", "DATE", "TIME", "TIMESTAMP", "UUID",
        ],
    };
    let mut d = DesignerSpec::sql_table(types);
    d.schemas = p.has_schemas;
    d.comments = flavor(p).comment_on || flavor(p).inline_comments;
    d.indexes = has_indexes(p);
    d.foreign_keys = reports_foreign_keys(p);
    match e {
        // The generic builder can't know the engine's identity syntax.
        Eng::Generic | Eng::SqlServer | Eng::NetSuite | Eng::Altibase | Eng::OpenEdge | Eng::Mimer => {
            d.auto_increment = false
        }
        Eng::Sqream | Eng::HeavyDb | Eng::Machbase => {
            d.primary_key = false;
            d.auto_increment = false;
        }
        Eng::DBase => {
            d.primary_key = false;
            d.auto_increment = false;
            d.defaults = false;
        }
        Eng::Ignite => {
            d.auto_increment = false;
            d.table_options = vec![text(
                "with",
                "Opciones (WITH)",
                "template=partitioned,backups=1",
                "Parámetros de la caché de la tabla, sin comillas: template, backups, affinity_key, cache_name…",
            )];
        }
        Eng::Ignite3 => d.auto_increment = false,
        Eng::Hive | Eng::Spark => {
            d.primary_key = false;
            d.auto_increment = false;
            d.defaults = false;
            d.nullability = false;
            d.comments = true;
            d.table_options = vec![
                select(
                    "stored_as",
                    "Formato (STORED AS)",
                    vec![
                        ("", "(el predeterminado)"),
                        ("ORC", "ORC"),
                        ("PARQUET", "Parquet"),
                        ("TEXTFILE", "Texto"),
                        ("AVRO", "Avro"),
                        ("SEQUENCEFILE", "SequenceFile"),
                        ("RCFILE", "RCFile"),
                        ("JSONFILE", "JSON"),
                    ],
                    "",
                ),
                text(
                    "partitioned_by",
                    "Particionada por (PARTITIONED BY)",
                    "anio INT, mes INT",
                    "Columnas de partición con su tipo. No van en la lista de columnas.",
                ),
                location(),
            ];
        }
        Eng::Impala => {
            d.auto_increment = false;
            d.comments = true;
            d.table_options = vec![
                select(
                    "stored_as",
                    "Formato (STORED AS)",
                    vec![
                        ("", "(el predeterminado)"),
                        ("PARQUET", "Parquet"),
                        ("KUDU", "Kudu"),
                        ("ICEBERG", "Iceberg"),
                        ("TEXTFILE", "Texto"),
                        ("AVRO", "Avro"),
                        ("SEQUENCEFILE", "SequenceFile"),
                        ("RCFILE", "RCFile"),
                    ],
                    "Clave primaria, NOT NULL y valores por defecto solo se aplican a las tablas Kudu.",
                ),
                text(
                    "partitioned_by",
                    "Partición",
                    "anio INT, mes INT",
                    "Tablas HDFS: columnas de PARTITIONED BY con su tipo (no van en la lista de columnas). Kudu: lo que sigue a PARTITION BY, por ejemplo HASH (id) PARTITIONS 4.",
                ),
                location(),
            ];
        }
        Eng::Teradata => {
            d.table_options = vec![
                select(
                    "table_kind",
                    "Tipo de tabla",
                    vec![("MULTISET", "MULTISET (admite filas duplicadas)"), ("SET", "SET (sin filas duplicadas)")],
                    "",
                ),
                text(
                    "primary_index",
                    "Índice primario (PRIMARY INDEX)",
                    "id",
                    "Columnas separadas por coma. Vacío: el que elija Teradata (la clave primaria, si hay). NO para NO PRIMARY INDEX.",
                ),
            ];
        }
        Eng::Netezza => {
            d.auto_increment = false;
            d.table_options = vec![text(
                "distribute_on",
                "Distribución (DISTRIBUTE ON)",
                "id",
                "Columnas separadas por coma, o RANDOM. Vacío: la predeterminada.",
            )];
        }
        Eng::Ocient => {
            d.primary_key = false;
        }
        _ => {}
    }
    d
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Removes the explicit ` NULL` of nullable columns (engines where it isn't
/// valid syntax or is the default anyway).
fn strip_explicit_null(ddl: &str) -> String {
    ddl.lines()
        .map(|l| {
            if !l.starts_with("    ") {
                return l.to_string();
            }
            let (body, comma) = match l.strip_suffix(',') {
                Some(b) => (b, ","),
                None => (l, ""),
            };
            match body.strip_suffix(" NULL") {
                Some(b) if !b.ends_with(" NOT") => format!("{b}{comma}"),
                _ => l.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn cols(q: Quote, c: &[String]) -> String {
    c.iter().map(|c| quote_ident(q, c)).collect::<Vec<_>>().join(", ")
}

fn opt<'a>(t: &'a TableSchema, key: &str) -> Option<&'a str> {
    t.options.get(key).map(|s| s.trim()).filter(|s| !s.is_empty())
}

/// Identity written into the type (or as a default) where the flavor has
/// no keyword for it.
pub fn identity_in_type(e: Eng, c: &mut dbine_driver::ColumnDef) {
    if !c.auto_increment {
        return;
    }
    match e {
        Eng::Ase | Eng::Exasol | Eng::Virtuoso => c.data_type = format!("{} IDENTITY", c.data_type),
        Eng::Sqla => c.default_value = Some("AUTOINCREMENT".into()),
        Eng::MaxDb => c.default_value = Some("SERIAL".into()),
        Eng::Iris => c.data_type = "IDENTITY".into(),
        Eng::Access => c.data_type = "COUNTER".into(),
        Eng::Zen => {
            let big = c.data_type.to_ascii_lowercase().contains("big");
            c.data_type = if big { "BIGIDENTITY".into() } else { "IDENTITY".into() };
        }
        Eng::Informix => {
            let big = ["bigint", "int8", "bigserial", "serial8"].iter().any(|b| c.data_type.to_ascii_lowercase().contains(b));
            c.data_type = if big { "BIGSERIAL".into() } else { "SERIAL".into() };
        }
        Eng::Vertica => c.data_type = "IDENTITY".into(),
        _ => {}
    }
}

pub fn table_ddl(p: &Preset, t: &TableSchema, parts: DdlParts) -> String {
    let e = eng(p);
    if matches!(e, Eng::Hive | Eng::Impala | Eng::Spark) {
        return hive_ddl(if e == Eng::Impala { e } else { Eng::Hive }, t, parts);
    }
    let f = flavor(p);
    let mut t = t.clone();
    for c in t.columns.iter_mut() {
        identity_in_type(e, c);
    }
    if !has_indexes(p) {
        t.indexes.clear();
    }
    if !reports_foreign_keys(p) {
        t.foreign_keys.clear();
    }
    let pk_name = t.primary_key.as_ref().and_then(|k| k.name.clone()).filter(|n| !n.is_empty());
    if e == Eng::Informix {
        // Informix names constraints after them: `PRIMARY KEY (…) CONSTRAINT n`.
        if let Some(k) = t.primary_key.as_mut() {
            k.name = None;
        }
    }
    let custom_indexes = e == Eng::Teradata;
    let custom_fks = e == Eng::Informix;
    let base = DdlParts {
        indexes: parts.indexes && !custom_indexes,
        foreign_keys: parts.foreign_keys && !custom_fks,
        ..parts
    };
    let mut s = ddl::table_ddl(&f, &t, base);
    let q = f.quote;
    let name = qualified_name(q, t.schema.as_deref().filter(|s| !s.is_empty()), &t.name);

    match e {
        Eng::Ase => s = s.replace(" IDENTITY NOT NULL", " IDENTITY"),
        Eng::Informix if parts.create => {
            if let (Some(n), Some(k)) = (&pk_name, &t.primary_key) {
                let clause = format!("    PRIMARY KEY ({})", cols(q, &k.columns));
                s = s.replace(&clause, &format!("{clause} CONSTRAINT {}", quote_ident(q, n)));
            }
            // CHECK (…) CONSTRAINT n: the name goes after.
            if !t.checks.is_empty() {
                s = crate::structure::informix_checks(&s);
            }
        }
        Eng::Teradata if parts.create => {
            let kind = match opt(&t, "table_kind") {
                Some(k) if k.eq_ignore_ascii_case("SET") => "SET",
                _ => "MULTISET",
            };
            s = s.replacen("CREATE TABLE ", &format!("CREATE {kind} TABLE "), 1);
            if let Some(pi) = opt(&t, "primary_index") {
                let clause = if pi.eq_ignore_ascii_case("NO") {
                    " NO PRIMARY INDEX".to_string()
                } else {
                    let c: Vec<String> = pi.split(',').map(|c| c.trim()).filter(|c| !c.is_empty()).map(String::from).collect();
                    format!(" PRIMARY INDEX ({})", cols(q, &c))
                };
                s = s.replacen("\n);", &format!("\n){clause};"), 1);
            }
        }
        Eng::Ignite if parts.create => {
            if let Some(w) = opt(&t, "with") {
                let w = w.trim().trim_matches('"').replace('"', "");
                s = s.replacen("\n);", &format!("\n) WITH \"{w}\";"), 1);
            }
        }
        Eng::Netezza if parts.create => {
            if let Some(d) = opt(&t, "distribute_on") {
                let clause = if d.eq_ignore_ascii_case("RANDOM") {
                    " DISTRIBUTE ON RANDOM".to_string()
                } else {
                    let c: Vec<String> = d.split(',').map(|c| c.trim()).filter(|c| !c.is_empty()).map(String::from).collect();
                    format!(" DISTRIBUTE ON ({})", cols(q, &c))
                };
                s = s.replacen("\n);", &format!("\n){clause};"), 1);
            }
        }
        _ => {}
    }
    if !matches!(e, Eng::Generic | Eng::SqlServer | Eng::Ase | Eng::Sqla) {
        s = strip_explicit_null(&s);
    }

    let mut extra: Vec<String> = Vec::new();
    if parts.indexes && custom_indexes {
        // Teradata: CREATE [UNIQUE] INDEX name (cols) ON table.
        for ix in &t.indexes {
            extra.push(format!(
                "CREATE {}INDEX {} ({}) ON {name};",
                if ix.unique { "UNIQUE " } else { "" },
                quote_ident(q, &ix.name),
                cols(q, &ix.columns)
            ));
        }
    }
    if parts.foreign_keys && custom_fks {
        // Informix: ADD CONSTRAINT FOREIGN KEY … [CONSTRAINT name]; only
        // ON DELETE CASCADE exists.
        for fk in &t.foreign_keys {
            let target = qualified_name(q, fk.ref_schema.as_deref().or(t.schema.as_deref()).filter(|s| !s.is_empty()), &fk.ref_table);
            let mut st = format!(
                "ALTER TABLE {name} ADD CONSTRAINT FOREIGN KEY ({}) REFERENCES {target} ({})",
                cols(q, &fk.columns),
                cols(q, &fk.ref_columns)
            );
            if fk.on_delete.as_deref().is_some_and(|a| a.eq_ignore_ascii_case("CASCADE")) {
                st.push_str(" ON DELETE CASCADE");
            }
            if let Some(n) = fk.name.as_deref().filter(|n| !n.is_empty()) {
                st.push_str(&format!(" CONSTRAINT {}", quote_ident(q, n)));
            }
            st.push(';');
            extra.push(st);
        }
    }
    if !extra.is_empty() {
        if !s.is_empty() {
            s.push('\n');
        }
        s.push_str(&extra.join("\n"));
    }
    s
}

/// Hive and Impala: no constraints (except Kudu's primary key), storage
/// clauses after the columns.
fn hive_ddl(e: Eng, t: &TableSchema, parts: DdlParts) -> String {
    let q = Quote::Backtick;
    let name = qualified_name(q, t.schema.as_deref().filter(|s| !s.is_empty()), &t.name);
    let stored = opt(t, "stored_as").map(|s| s.to_ascii_uppercase());
    let kudu = e == Eng::Impala && stored.as_deref() == Some("KUDU");
    let mut out = Vec::new();
    if parts.drop {
        out.push(format!("DROP TABLE {}{name};", if parts.if_exists { "IF EXISTS " } else { "" }));
    }
    if parts.create {
        let pk: Vec<String> = t.primary_key.as_ref().map(|k| k.columns.clone()).unwrap_or_default();
        let mut lines: Vec<String> = t
            .columns
            .iter()
            .map(|c| {
                let mut l = format!("    {} {}", quote_ident(q, &c.name), c.data_type);
                if kudu {
                    if !c.nullable || pk.contains(&c.name) {
                        l.push_str(" NOT NULL");
                    }
                    if let Some(d) = c.default_value.as_deref().filter(|d| !d.is_empty()) {
                        l.push_str(&format!(" DEFAULT {d}"));
                    }
                }
                if let Some(cm) = c.comment.as_deref().filter(|s| !s.is_empty()) {
                    l.push_str(&format!(" COMMENT {}", lit(cm)));
                }
                l
            })
            .collect();
        if kudu && !pk.is_empty() {
            lines.push(format!("    PRIMARY KEY ({})", cols(q, &pk)));
        }
        let mut s = format!(
            "CREATE TABLE {}{name} (\n{}\n)",
            if parts.if_exists && !parts.drop { "IF NOT EXISTS " } else { "" },
            lines.join(",\n")
        );
        let comment = t.comment.as_deref().filter(|s| !s.is_empty()).map(|c| format!("\nCOMMENT {}", lit(c)));
        let part = opt(t, "partitioned_by");
        if e == Eng::Hive {
            // Hive: COMMENT, PARTITIONED BY, STORED AS, LOCATION.
            s.push_str(&comment.unwrap_or_default());
            if let Some(pb) = part {
                s.push_str(&format!("\nPARTITIONED BY ({pb})"));
            }
        } else {
            // Impala: PARTITIONED BY / PARTITION BY, COMMENT, STORED AS, LOCATION.
            if let Some(pb) = part {
                s.push_str(&if kudu { format!("\nPARTITION BY {pb}") } else { format!("\nPARTITIONED BY ({pb})") });
            }
            s.push_str(&comment.unwrap_or_default());
        }
        if let Some(st) = &stored {
            s.push_str(&format!("\nSTORED AS {st}"));
        }
        if let Some(loc) = opt(t, "location") {
            s.push_str(&format!("\nLOCATION {}", lit(loc)));
        }
        s.push(';');
        out.push(s);
    }
    out.join("\n")
}

/// A value as a literal of the preset. Hive, Impala and Spark read backslash
/// escapes in strings (and `''` would be two literals): a quote goes as `\'`.
fn literal(p: &Preset, v: &Value) -> String {
    let bs = |s: &str| format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"));
    match v {
        Value::String(s) if backslash_strings(p) => bs(s),
        Value::Array(_) | Value::Object(_) if backslash_strings(p) => bs(&v.to_string()),
        other => ddl::sql_literal(&flavor(p), other),
    }
}

fn backslash_strings(p: &Preset) -> bool {
    matches!(eng(p), Eng::Hive | Eng::Impala | Eng::Spark)
}

pub fn insert_script(p: &Preset, schema: Option<&str>, table: &str, columns: &[String], rows: &[Vec<Value>]) -> String {
    ddl::insert_script_with(&flavor(p), schema, table, columns, rows, 100, &|v| literal(p, v))
}

/// `UPDATE … WHERE <key>` per changed row, with the preset's quoting and
/// literals (as in [`insert_script`]). SuiteAnalytics Connect is read-only.
pub fn update_script(p: &Preset, schema: Option<&str>, table: &str, changes: &[RowChange]) -> Result<String> {
    if eng(p) == Eng::NetSuite {
        return Err(Error::Unsupported("SuiteAnalytics Connect de NetSuite es de solo lectura".into()));
    }
    Ok(ddl::update_script_with(flavor(p).quote, schema, table, changes, &|v| literal(p, v)))
}

/// `DELETE … WHERE <key>` per row key, with the preset's quoting and
/// literals (as in [`update_script`]). SuiteAnalytics Connect is read-only.
pub fn delete_script(p: &Preset, schema: Option<&str>, table: &str, keys: &[Vec<(String, Value)>]) -> Result<String> {
    if eng(p) == Eng::NetSuite {
        return Err(Error::Unsupported("SuiteAnalytics Connect de NetSuite es de solo lectura".into()));
    }
    Ok(ddl::delete_script_with(flavor(p).quote, schema, table, keys, &|v| literal(p, v)))
}

/// The browse query restricted by the grid's column filters, with the
/// preset's literals. The generic preset quotes identifiers as the ODBC
/// driver says, so the quote is taken from the browse query itself. Hive,
/// Impala and Spark read backslash escapes in string literals (and `''`
/// would be two literals): quotes go as `\'`, and LIKE relies on the
/// default `\` escape instead of an ESCAPE clause.
pub fn filtered_browse(p: &Preset, browse: &str, filters: &[ColumnFilter]) -> Result<String> {
    if filters.is_empty() {
        return Ok(browse.to_string());
    }
    let f = flavor(p);
    let quote = if p.quote.is_some() { f.quote } else { quote_after_from(browse) };
    let backslash = backslash_strings(p);
    let literal = |v: &Value| literal(p, v);
    let style = SqlFilterStyle { quote, literal: &literal, like: "LIKE", true_literal: f.true_literal, false_literal: f.false_literal };
    let mut parts = Vec::new();
    for c in filters {
        let cond = sql_condition(std::slice::from_ref(c), &style)?;
        let like = matches!(c.op, FilterOp::Contains | FilterOp::NotContains | FilterOp::StartsWith | FilterOp::EndsWith);
        parts.push(match cond.strip_suffix(" ESCAPE '\\'") {
            Some(s) if like && backslash => s.to_string(),
            _ => cond,
        });
    }
    insert_where(browse, &parts.join("\n  AND "))
        .ok_or_else(|| Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into()))
}

/// The identifier quote the table after `FROM` uses.
fn quote_after_from(browse: &str) -> Quote {
    let lower = browse.to_ascii_lowercase();
    match lower.find("from ").and_then(|i| browse[i + 5..].trim_start().chars().next()) {
        Some('`') => Quote::Backtick,
        Some('[') => Quote::Bracket,
        _ => Quote::Double,
    }
}

fn t(kind: &'static str, label: &'static str, template: &str) -> CreateTemplate {
    CreateTemplate { kind, label, template: template.trim_start().to_string() }
}

const WHOLE_SCRIPT: &str = "-- Ejecutalo con el modo de envío «Todo el texto de una vez»: el cuerpo lleva «;».\n";

pub fn create_templates(p: &Preset) -> Vec<CreateTemplate> {
    // `{schema}.` only where there are schemas.
    let (qo, qc) = match quote(p) {
        Quote::Double => ("\"", "\""),
        Quote::Bracket => ("[", "]"),
        Quote::Backtick => ("`", "`"),
    };
    let obj = if p.has_schemas {
        format!("{qo}{{schema}}{qc}.{qo}{{name}}{qc}")
    } else {
        format!("{qo}{{name}}{qc}")
    };
    let view = t(kinds::VIEW, "Nueva vista", &format!("CREATE VIEW {obj} AS\nSELECT 1 AS col\n"));
    let mut v = vec![view];
    let o = obj.as_str();
    match eng(p) {
        // Read-only, or no views.
        Eng::NetSuite | Eng::DBase => v.clear(),
        Eng::Generic | Eng::SqlServer | Eng::Ocient | Eng::Access | Eng::HeavyDb | Eng::Machbase | Eng::Ignite
        | Eng::Ignite3 | Eng::Sqream => {}
        Eng::Spark => {
            v.push(t(kinds::FUNCTION, "Nueva función", &format!(
                "CREATE FUNCTION {o} AS 'com.ejemplo.MiUDF'\nUSING JAR 'hdfs:///ruta/udf.jar'\n"
            )));
        }
        Eng::MonetDb => {
            v.push(t(kinds::FUNCTION, "Nueva función", &format!(
                "CREATE OR REPLACE FUNCTION {o} (p_x INT)\nRETURNS INT\nBEGIN\n    RETURN p_x * 2;\nEND\n"
            )));
            v.push(t(kinds::PROCEDURE, "Nuevo procedimiento", &format!(
                "{WHOLE_SCRIPT}CREATE OR REPLACE PROCEDURE {o} (p_id INT)\nBEGIN\n    DECLARE x INT;\n    SET x = p_id;\nEND\n"
            )));
            v.push(t(kinds::SEQUENCE, "Nueva secuencia", &format!("CREATE SEQUENCE {o} START WITH 1 INCREMENT BY 1\n")));
        }
        Eng::Virtuoso => {
            v.push(t(kinds::PROCEDURE, "Nuevo procedimiento", &format!(
                "{WHOLE_SCRIPT}CREATE PROCEDURE {o} (IN p_id INTEGER)\n{{\n    RETURN p_id * 2;\n}}\n"
            )));
        }
        Eng::Ingres => {
            v.push(t(kinds::PROCEDURE, "Nuevo procedimiento", &format!(
                "{WHOLE_SCRIPT}CREATE PROCEDURE {qo}{{name}}{qc} (p_id INTEGER NOT NULL) AS\nBEGIN\n    RETURN p_id * 2;\nEND\n"
            )));
            v.push(t(kinds::SEQUENCE, "Nueva secuencia", &format!("CREATE SEQUENCE {o} START WITH 1 INCREMENT BY 1\n")));
        }
        Eng::Mimer => {
            v.push(t(kinds::PROCEDURE, "Nuevo procedimiento", &format!(
                "{WHOLE_SCRIPT}CREATE PROCEDURE {o} (IN p_id INTEGER)\nMODIFIES SQL DATA\nBEGIN\n    -- …\nEND\n"
            )));
            v.push(t(kinds::FUNCTION, "Nueva función", &format!(
                "{WHOLE_SCRIPT}CREATE FUNCTION {o} (p_x INTEGER)\nRETURNS INTEGER\nBEGIN\n    RETURN p_x * 2;\nEND\n"
            )));
            v.push(t(kinds::SEQUENCE, "Nueva secuencia", &format!("CREATE SEQUENCE {o} START WITH 1 INCREMENT BY 1\n")));
        }
        Eng::Iris => {
            v.push(t(kinds::PROCEDURE, "Nuevo procedimiento", &format!(
                "{WHOLE_SCRIPT}CREATE PROCEDURE {o} (IN p_id INT)\nLANGUAGE SQL\nBEGIN\n    SELECT p_id;\nEND\n"
            )));
            v.push(t(kinds::FUNCTION, "Nueva función", &format!(
                "CREATE FUNCTION {o} (p_x INT)\nRETURNS INT\nLANGUAGE OBJECTSCRIPT\n{{\n    QUIT p_x * 2\n}}\n"
            )));
        }
        Eng::OpenEdge => {
            v.push(t(kinds::PROCEDURE, "Nuevo procedimiento (Java)", &format!(
                "CREATE PROCEDURE {o} (IN p_id INTEGER)\nBEGIN\n    // Código Java\nEND\n"
            )));
            v.push(t(kinds::SEQUENCE, "Nueva secuencia", &format!("CREATE SEQUENCE {o} START WITH 1, INCREMENT BY 1, NOCYCLE\n")));
        }
        Eng::Zen => {
            v.push(t(kinds::PROCEDURE, "Nuevo procedimiento", &format!(
                "{WHOLE_SCRIPT}CREATE PROCEDURE {qo}{{name}}{qc} (IN :p_id INTEGER);\nBEGIN\n    SELECT :p_id;\nEND;\n"
            )));
        }
        Eng::MaxDb => {
            v.push(t(kinds::PROCEDURE, "Nuevo procedimiento", &format!(
                "{WHOLE_SCRIPT}CREATE DBPROC {o} (IN p_id INTEGER) AS\nVAR x INTEGER;\nBEGIN\n    x = p_id;\nEND;\n"
            )));
            v.push(t(kinds::SEQUENCE, "Nueva secuencia", &format!("CREATE SEQUENCE {o} INCREMENT BY 1 START WITH 1\n")));
        }
        Eng::NuoDb => {
            v.push(t(kinds::PROCEDURE, "Nuevo procedimiento", &format!(
                "{WHOLE_SCRIPT}CREATE PROCEDURE {o} (IN p_id INTEGER)\nAS\n    VAR x INTEGER = p_id;\nEND_PROCEDURE\n"
            )));
            v.push(t(kinds::SEQUENCE, "Nueva secuencia", &format!("CREATE SEQUENCE {o} START WITH 1\n")));
        }
        Eng::Db2 | Eng::Db2i | Eng::Db2zos => {
            v.push(t(kinds::PROCEDURE, "Nuevo procedimiento", &format!(
                "{WHOLE_SCRIPT}CREATE OR REPLACE PROCEDURE {o} (IN p_id INTEGER)\nLANGUAGE SQL\nBEGIN\n    -- …\nEND\n"
            )));
            v.push(t(kinds::FUNCTION, "Nueva función", &format!(
                "CREATE OR REPLACE FUNCTION {o} (p_x INTEGER)\nRETURNS INTEGER\nLANGUAGE SQL\nRETURN p_x * 2\n"
            )));
            v.push(t(kinds::TRIGGER, "Nuevo trigger", &format!(
                "{WHOLE_SCRIPT}CREATE OR REPLACE TRIGGER {o}\nAFTER INSERT ON {qo}{{schema}}{qc}.{qo}tabla{qc}\nREFERENCING NEW AS n\nFOR EACH ROW\nBEGIN ATOMIC\n    -- …\nEND\n"
            )));
            v.push(t(kinds::SEQUENCE, "Nueva secuencia", &format!("CREATE SEQUENCE {o} AS BIGINT START WITH 1 INCREMENT BY 1\n")));
        }
        Eng::Ase => {
            v.push(t(kinds::PROCEDURE, "Nuevo procedimiento", &format!(
                "CREATE PROCEDURE {o}\n    @id int\nAS\nBEGIN\n    SELECT @id\nEND\n"
            )));
            v.push(t(kinds::FUNCTION, "Nueva función", &format!(
                "CREATE FUNCTION {o} (@x int)\nRETURNS int\nAS\nBEGIN\n    RETURN @x * 2\nEND\n"
            )));
            v.push(t(kinds::TRIGGER, "Nuevo trigger", &format!(
                "CREATE TRIGGER {o}\nON [{{schema}}].[tabla]\nFOR INSERT\nAS\nBEGIN\n    -- inserted / deleted\n    PRINT 'ok'\nEND\n"
            )));
        }
        Eng::Sqla => {
            v.push(t(kinds::PROCEDURE, "Nuevo procedimiento", &format!(
                "CREATE OR REPLACE PROCEDURE {o} (IN p_id INTEGER)\nBEGIN\n    SELECT p_id;\nEND\n"
            )));
            v.push(t(kinds::FUNCTION, "Nueva función", &format!(
                "CREATE OR REPLACE FUNCTION {o} (IN p_x INTEGER)\nRETURNS INTEGER\nBEGIN\n    RETURN p_x * 2;\nEND\n"
            )));
            v.push(t(kinds::TRIGGER, "Nuevo trigger", &format!(
                "CREATE OR REPLACE TRIGGER {qo}{{name}}{qc}\nAFTER INSERT ON {qo}{{schema}}{qc}.{qo}tabla{qc}\nREFERENCING NEW AS n\nFOR EACH ROW\nBEGIN\n    -- …\nEND\n"
            )));
            v.push(t(kinds::SEQUENCE, "Nueva secuencia", &format!("CREATE SEQUENCE {o} START WITH 1 INCREMENT BY 1\n")));
        }
        Eng::Hive => {
            v.push(t(kinds::MATERIALIZED_VIEW, "Nueva vista materializada", &format!(
                "CREATE MATERIALIZED VIEW {o} AS\nSELECT 1 AS col\n"
            )));
            v.push(t(kinds::FUNCTION, "Nueva función", &format!(
                "CREATE FUNCTION {o} AS 'com.ejemplo.MiUDF'\nUSING JAR 'hdfs:///ruta/udf.jar'\n"
            )));
        }
        Eng::Impala => {
            v.push(t(kinds::FUNCTION, "Nueva función", &format!(
                "CREATE FUNCTION {o} (INT) RETURNS INT\nLOCATION 'hdfs:///ruta/udf.so' SYMBOL='MiUdf'\n"
            )));
        }
        Eng::Informix => {
            v.push(t(kinds::PROCEDURE, "Nuevo procedimiento", &format!(
                "{WHOLE_SCRIPT}CREATE PROCEDURE {qo}{{name}}{qc} (p_id INTEGER)\n    -- …\nEND PROCEDURE\n"
            )));
            v.push(t(kinds::FUNCTION, "Nueva función", &format!(
                "{WHOLE_SCRIPT}CREATE FUNCTION {qo}{{name}}{qc} (p_x INTEGER)\nRETURNING INTEGER;\n    RETURN p_x * 2;\nEND FUNCTION\n"
            )));
            v.push(t(kinds::TRIGGER, "Nuevo trigger", &format!(
                "CREATE TRIGGER {qo}{{name}}{qc}\nINSERT ON {qo}tabla{qc}\nREFERENCING NEW AS n\nFOR EACH ROW (EXECUTE PROCEDURE {qo}proc{qc}(n.id))\n"
            )));
            v.push(t(kinds::SEQUENCE, "Nueva secuencia", &format!("CREATE SEQUENCE {qo}{{name}}{qc} START WITH 1 INCREMENT BY 1\n")));
        }
        Eng::Teradata => {
            v.push(t(kinds::PROCEDURE, "Nuevo procedimiento", &format!(
                "{WHOLE_SCRIPT}REPLACE PROCEDURE {o} (IN p_id INTEGER)\nBEGIN\n    -- …\nEND\n"
            )));
            v.push(t(kinds::FUNCTION, "Nueva función", &format!(
                "REPLACE FUNCTION {o} (p_x INTEGER)\nRETURNS INTEGER\nLANGUAGE SQL\nCONTAINS SQL\nDETERMINISTIC\nSQL SECURITY DEFINER\nCOLLATION INVOKER\nINLINE TYPE 1\nRETURN p_x * 2\n"
            )));
            v.push(t(kinds::TRIGGER, "Nuevo trigger", &format!(
                "REPLACE TRIGGER {o}\nAFTER INSERT ON {qo}{{schema}}{qc}.{qo}tabla{qc}\nREFERENCING NEW AS n\nFOR EACH ROW\n(\n    INSERT INTO {qo}{{schema}}{qc}.{qo}auditoria{qc} VALUES (n.id);\n)\n"
            )));
        }
        Eng::Vertica => {
            v.push(t(kinds::FUNCTION, "Nueva función", &format!(
                "CREATE OR REPLACE FUNCTION {o} (x INT)\nRETURN INT\nAS BEGIN\n    RETURN x * 2;\nEND\n"
            )));
            v.push(t(kinds::PROCEDURE, "Nuevo procedimiento", &format!(
                "CREATE OR REPLACE PROCEDURE {o} (p_id INT)\nLANGUAGE PLvSQL AS $$\nBEGIN\n    PERFORM 1;\nEND;\n$$\n"
            )));
            v.push(t(kinds::SEQUENCE, "Nueva secuencia", &format!("CREATE SEQUENCE {o} START 1 INCREMENT 1\n")));
        }
        Eng::Exasol => {
            v.push(t(kinds::FUNCTION, "Nueva función", &format!(
                "{WHOLE_SCRIPT}CREATE OR REPLACE FUNCTION {o} (p_x DECIMAL(18,0))\nRETURN DECIMAL(18,0)\nIS\nBEGIN\n    RETURN p_x * 2;\nEND {o}\n/\n"
            )));
            v.push(t(kinds::PROCEDURE, "Nuevo script", &format!(
                "{WHOLE_SCRIPT}CREATE OR REPLACE LUA SCRIPT {o} () RETURNS ROWCOUNT AS\n    query([[SELECT 1]])\n/\n"
            )));
        }
        Eng::Netezza => {
            v.push(t(kinds::PROCEDURE, "Nuevo procedimiento", &format!(
                "{WHOLE_SCRIPT}CREATE OR REPLACE PROCEDURE {o} (INTEGER)\nRETURNS INTEGER\nLANGUAGE NZPLSQL AS\nBEGIN_PROC\nBEGIN\n    RETURN $1 * 2;\nEND;\nEND_PROC\n"
            )));
            v.push(t(kinds::SEQUENCE, "Nueva secuencia", &format!("CREATE SEQUENCE {o} AS BIGINT START WITH 1 INCREMENT BY 1\n")));
        }
        Eng::Altibase | Eng::Dameng => {
            v.push(t(kinds::PROCEDURE, "Nuevo procedimiento", &format!(
                "{WHOLE_SCRIPT}CREATE OR REPLACE PROCEDURE {o} (p_id IN INTEGER)\nAS\nBEGIN\n    NULL;\nEND;\n"
            )));
            v.push(t(kinds::FUNCTION, "Nueva función", &format!(
                "{WHOLE_SCRIPT}CREATE OR REPLACE FUNCTION {o} (p_x IN INTEGER)\nRETURN INTEGER\nAS\nBEGIN\n    RETURN p_x * 2;\nEND;\n"
            )));
            v.push(t(kinds::TRIGGER, "Nuevo trigger", &format!(
                "{WHOLE_SCRIPT}CREATE OR REPLACE TRIGGER {o}\nAFTER INSERT ON {qo}{{schema}}{qc}.{qo}tabla{qc}\nFOR EACH ROW\nBEGIN\n    NULL;\nEND;\n"
            )));
            v.push(t(kinds::SEQUENCE, "Nueva secuencia", &format!("CREATE SEQUENCE {o} START WITH 1 INCREMENT BY 1\n")));
        }
        Eng::Cubrid => {
            v.push(t(kinds::SEQUENCE, "Nueva secuencia", &format!("CREATE SERIAL {o} START WITH 1 INCREMENT BY 1\n")));
            v.push(t(kinds::TRIGGER, "Nuevo trigger", &format!(
                "CREATE TRIGGER {o}\nAFTER INSERT ON {qo}tabla{qc}\nEXECUTE INSERT INTO {qo}auditoria{qc} (id) VALUES (obj.id)\n"
            )));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presets::PRESETS;
    use dbine_driver::{ColumnDef, ForeignKeyDef, IndexDef, KeyDef};
    use serde_json::json;

    fn preset(id: &str) -> &'static Preset {
        PRESETS.iter().find(|p| p.id == id).unwrap()
    }

    #[test]
    fn filtered_browse_per_preset() {
        let f = |column: &str, op: FilterOp, values: Vec<Value>| ColumnFilter { column: column.into(), op, values, sql: None };
        let filters = [
            f("nombre", FilterOp::Eq, vec![json!("O'Brien")]),
            f("nota", FilterOp::Contains, vec![json!("50%")]),
            f("n", FilterOp::Gt, vec![json!(3)]),
            f("baja", FilterOp::IsNull, vec![]),
            f("id", FilterOp::In, vec![json!(1), json!(2)]),
            f("activo", FilterOp::IsTrue, vec![]),
        ];
        assert_eq!(
            filtered_browse(preset("db2"), "SELECT *\nFROM \"S\".\"T\"\nFETCH FIRST 200 ROWS ONLY", &filters).unwrap(),
            "SELECT *\nFROM \"S\".\"T\"\nWHERE \"nombre\" = 'O''Brien'\n  AND \"nota\" LIKE '%50\\%%' ESCAPE '\\'\n  AND \"n\" > 3\n  AND \"baja\" IS NULL\n  AND \"id\" IN (1, 2)\n  AND \"activo\" = TRUE\nFETCH FIRST 200 ROWS ONLY"
        );
        assert_eq!(
            filtered_browse(preset("hive"), "SELECT *\nFROM `db`.`t`\nLIMIT 200", &filters[..2]).unwrap(),
            "SELECT *\nFROM `db`.`t`\nWHERE `nombre` = 'O\\'Brien'\n  AND `nota` LIKE '%50\\\\%%'\nLIMIT 200"
        );
        // Generic: the quote the driver gave the browse query.
        assert_eq!(
            filtered_browse(preset("odbc"), "SELECT *\nFROM `t`\nLIMIT 200", &filters[5..]).unwrap(),
            "SELECT *\nFROM `t`\nWHERE `activo` = 1\nLIMIT 200"
        );
        assert_eq!(
            filtered_browse(preset("informix"), "SELECT FIRST 200 *\nFROM \"t\"", &filters[5..]).unwrap(),
            "SELECT FIRST 200 *\nFROM \"t\"\nWHERE \"activo\" = 't'"
        );
    }

    #[test]
    fn update_script_per_preset() {
        let c = RowChange {
            key: vec![("id".into(), json!(7)), ("region".into(), Value::Null)],
            set: vec![("nombre".into(), json!("O'Brien")), ("activo".into(), json!(true)), ("baja".into(), Value::Null)], ..Default::default()
        };
        assert_eq!(
            update_script(preset("sybase"), Some("dbo"), "clientes", std::slice::from_ref(&c)).unwrap(),
            "UPDATE [dbo].[clientes] SET [nombre] = 'O''Brien', [activo] = 1, [baja] = NULL WHERE [id] = 7 AND [region] IS NULL;"
        );
        assert_eq!(
            update_script(preset("db2"), Some("APP"), "clientes", std::slice::from_ref(&c)).unwrap(),
            "UPDATE \"APP\".\"clientes\" SET \"nombre\" = 'O''Brien', \"activo\" = TRUE, \"baja\" = NULL WHERE \"id\" = 7 AND \"region\" IS NULL;"
        );
        assert!(update_script(preset("netsuite"), None, "clientes", &[c]).is_err());
    }

    #[test]
    fn delete_script_per_preset() {
        let keys = vec![vec![("nombre".into(), json!("O'Brien")), ("region".into(), Value::Null)], vec![]];
        assert_eq!(
            delete_script(preset("sybase"), Some("dbo"), "clientes", &keys).unwrap(),
            "DELETE FROM [dbo].[clientes] WHERE [nombre] = 'O''Brien' AND [region] IS NULL;"
        );
        assert_eq!(
            delete_script(preset("db2"), Some("APP"), "clientes", &keys).unwrap(),
            "DELETE FROM \"APP\".\"clientes\" WHERE \"nombre\" = 'O''Brien' AND \"region\" IS NULL;"
        );
        assert!(delete_script(preset("netsuite"), None, "clientes", &keys).is_err());
    }

    fn sample() -> TableSchema {
        TableSchema {
            kind: "table".into(),
            schema: Some("app".into()),
            name: "pedidos".into(),
            columns: vec![
                ColumnDef { name: "id".into(), data_type: "INTEGER".into(), nullable: false, auto_increment: true, ..Default::default() },
                ColumnDef { name: "cliente_id".into(), data_type: "INTEGER".into(), nullable: true, comment: Some("dueño".into()), ..Default::default() },
                ColumnDef { name: "estado".into(), data_type: "VARCHAR(20)".into(), nullable: false, default_value: Some("'nuevo'".into()), ..Default::default() },
            ],
            primary_key: Some(KeyDef { name: Some("pk_pedidos".into()), columns: vec!["id".into()] }),
            foreign_keys: vec![ForeignKeyDef {
                name: Some("fk_cliente".into()),
                columns: vec!["cliente_id".into()],
                ref_table: "clientes".into(),
                ref_columns: vec!["id".into()],
                on_delete: Some("CASCADE".into()),
                ..Default::default()
            }],
            indexes: vec![IndexDef { name: "ix_estado".into(), columns: vec!["estado".into()], ..Default::default() }],
            comment: Some("Pedidos".into()),
            ..Default::default()
        }
    }

    const ALL: DdlParts = DdlParts { drop: true, if_exists: true, create: true, indexes: true, foreign_keys: true };

    fn ddl_of(id: &str, t: &TableSchema) -> String {
        table_ddl(preset(id), t, ALL)
    }

    #[test]
    fn every_preset_has_a_designer_ddl_and_templates() {
        for p in PRESETS {
            let d = designer(p);
            assert!(!d.data_types.is_empty(), "{}", p.id);
            let s = table_ddl(p, &sample(), ALL);
            assert!(s.contains("CREATE "), "{}: {s}", p.id);
            assert!(s.contains("pedidos"), "{}", p.id);
            // dBase has no views and NetSuite is read-only.
            assert_eq!(create_templates(p).is_empty(), matches!(p.id, "dbase" | "netsuite"), "{}", p.id);
            let ins = insert_script(p, Some("app"), "pedidos", &["id".into()], &[vec![json!(1)], vec![json!(2)]]);
            assert!(ins.starts_with("INSERT INTO "), "{}", p.id);
        }
    }

    #[test]
    fn generic_is_plain_sql() {
        let mut t = sample();
        t.columns[0].data_type = "int identity".into();
        let s = ddl_of("odbc", &t);
        assert!(s.starts_with("DROP TABLE \"app\".\"pedidos\";"), "{s}");
        assert!(s.contains("\"id\" int identity NOT NULL,"), "{s}");
        assert!(s.contains("\"cliente_id\" INTEGER NULL,"), "{s}");
        assert!(s.contains("CONSTRAINT \"pk_pedidos\" PRIMARY KEY (\"id\")"));
        assert!(s.contains("CREATE INDEX \"ix_estado\" ON \"app\".\"pedidos\" (\"estado\");"));
        assert!(s.contains("ALTER TABLE \"app\".\"pedidos\" ADD CONSTRAINT \"fk_cliente\" FOREIGN KEY"));
        assert!(!s.contains("COMMENT"));
        let ins = insert_script(preset("odbc"), None, "t", &["a".into(), "b".into()], &[vec![json!(1), json!(true)], vec![json!(2), json!(false)]]);
        assert_eq!(ins, "INSERT INTO \"t\" (\"a\", \"b\") VALUES\n  (1, 1),\n  (2, 0);");
    }

    #[test]
    fn db2_identity_and_comments() {
        let s = ddl_of("db2", &sample());
        assert!(s.starts_with("DROP TABLE \"app\".\"pedidos\";"), "{s}");
        assert!(s.contains("\"id\" INTEGER GENERATED BY DEFAULT AS IDENTITY NOT NULL,"), "{s}");
        assert!(s.contains("\"cliente_id\" INTEGER,"), "{s}");
        assert!(s.contains("COMMENT ON COLUMN \"app\".\"pedidos\".\"cliente_id\" IS 'dueño';"));
        let rows = vec![vec![json!(1)], vec![json!(2)]];
        assert_eq!(insert_script(preset("db2"), None, "t", &["a".into()], &rows).lines().count(), 3);
        assert_eq!(insert_script(preset("db2zos"), None, "t", &["a".into()], &rows).lines().count(), 2);
    }

    #[test]
    fn sybase_ase_identity_and_single_row_inserts() {
        let s = ddl_of("sybase", &sample());
        assert!(s.contains("[id] INTEGER IDENTITY,"), "{s}");
        assert!(s.contains("[cliente_id] INTEGER NULL,"), "{s}");
        assert!(!s.contains("IF EXISTS"));
        assert!(!s.contains("COMMENT"));
        let ins = insert_script(preset("sybase"), Some("dbo"), "t", &["a".into()], &[vec![json!(true)], vec![json!("x")]]);
        assert_eq!(ins, "INSERT INTO [dbo].[t] ([a]) VALUES (1);\nINSERT INTO [dbo].[t] ([a]) VALUES ('x');");
        assert_eq!(script_separator(preset("sybase")), "GO");
    }

    #[test]
    fn sql_anywhere_autoincrement_default() {
        let s = ddl_of("sqlanywhere", &sample());
        assert!(s.contains("\"id\" INTEGER DEFAULT AUTOINCREMENT NOT NULL,"), "{s}");
        assert!(s.contains("DROP TABLE IF EXISTS"));
    }

    #[test]
    fn informix_serial_and_trailing_constraint_names() {
        let s = ddl_of("informix", &sample());
        assert!(s.contains("\"id\" SERIAL NOT NULL,"), "{s}");
        assert!(s.contains("\"cliente_id\" INTEGER,"), "{s}");
        assert!(s.contains("    PRIMARY KEY (\"id\") CONSTRAINT \"pk_pedidos\"\n);"), "{s}");
        assert!(s.contains(
            "ALTER TABLE \"app\".\"pedidos\" ADD CONSTRAINT FOREIGN KEY (\"cliente_id\") REFERENCES \"app\".\"clientes\" (\"id\") ON DELETE CASCADE CONSTRAINT \"fk_cliente\";"
        ), "{s}");
        let ins = insert_script(preset("gbase8s"), None, "t", &["a".into()], &[vec![json!(true)], vec![json!(false)]]);
        assert_eq!(ins, "INSERT INTO \"t\" (\"a\") VALUES ('t');\nINSERT INTO \"t\" (\"a\") VALUES ('f');");
    }

    #[test]
    fn teradata_multiset_primary_index_and_indexes() {
        let mut t = sample();
        t.options.insert("primary_index".into(), "id, estado".into());
        let s = ddl_of("teradata", &t);
        assert!(s.contains("CREATE MULTISET TABLE \"app\".\"pedidos\" ("), "{s}");
        assert!(s.contains("\n) PRIMARY INDEX (\"id\", \"estado\");"), "{s}");
        assert!(s.contains("CREATE INDEX \"ix_estado\" (\"estado\") ON \"app\".\"pedidos\";"), "{s}");
        assert!(!s.contains("IF EXISTS"));
        t.options.insert("table_kind".into(), "SET".into());
        t.options.insert("primary_index".into(), "NO".into());
        let s = ddl_of("teradata", &t);
        assert!(s.contains("CREATE SET TABLE") && s.contains(") NO PRIMARY INDEX;"), "{s}");
    }

    #[test]
    fn vertica_exasol_netezza() {
        let v = ddl_of("vertica", &sample());
        assert!(v.contains("\"id\" IDENTITY NOT NULL,"), "{v}");
        assert!(!v.contains("CREATE INDEX"));
        let e = ddl_of("exasol", &sample());
        assert!(e.contains("\"id\" INTEGER IDENTITY NOT NULL,"), "{e}");
        assert!(e.contains("COMMENT ON TABLE"));
        assert!(!e.contains("CREATE INDEX"));
        let mut t = sample();
        t.options.insert("distribute_on".into(), "id".into());
        let n = ddl_of("netezza", &t);
        assert!(n.contains("\"id\" INTEGER NOT NULL,"), "{n}");
        assert!(n.contains("\n) DISTRIBUTE ON (\"id\");"), "{n}");
        assert!(n.contains("FOREIGN KEY"));
    }

    #[test]
    fn cubrid_dameng_altibase_ocient() {
        let c = ddl_of("cubrid", &sample());
        assert!(c.contains("\"id\" INTEGER AUTO_INCREMENT NOT NULL,"), "{c}");
        assert!(c.contains("COMMENT 'dueño'"));
        let d = ddl_of("dameng", &sample());
        assert!(d.contains("\"id\" INTEGER IDENTITY(1,1) NOT NULL,"), "{d}");
        let a = ddl_of("altibase", &sample());
        assert!(a.contains("\"id\" INTEGER NOT NULL,"), "{a}");
        let o = ddl_of("ocient", &sample());
        assert!(!o.contains("FOREIGN KEY"), "{o}");
    }

    #[test]
    fn hive_family_strings_escape_quotes_with_a_backslash() {
        let row = vec![json!("O'Brien"), json!(1)];
        for id in ["hive", "impala", "spark"] {
            let ins = insert_script(preset(id), Some("db"), "t", &["a".into(), "b".into()], &[row.clone()]);
            assert!(ins.contains("'O\\'Brien'") && !ins.contains("''"), "{id}: {ins}");
            let key = vec![vec![("a".to_string(), json!("O'Brien"))]];
            let del = delete_script(preset(id), Some("db"), "t", &key).unwrap();
            assert!(del.contains("= 'O\\'Brien'"), "{id}: {del}");
        }
        // Everyone else keeps the standard doubled quote.
        let ins = insert_script(preset("db2"), None, "t", &["a".into()], &[vec![json!("O'Brien")]]);
        assert!(ins.contains("'O''Brien'"), "{ins}");
    }

    #[test]
    fn hive_and_impala_storage_clauses() {
        let mut t = sample();
        t.options.insert("stored_as".into(), "orc".into());
        t.options.insert("partitioned_by".into(), "anio INT".into());
        t.options.insert("location".into(), "/data/p".into());
        let h = ddl_of("hive", &t);
        assert_eq!(
            h,
            "DROP TABLE IF EXISTS `app`.`pedidos`;\nCREATE TABLE `app`.`pedidos` (\n    `id` INTEGER,\n    `cliente_id` INTEGER COMMENT 'dueño',\n    `estado` VARCHAR(20)\n)\nCOMMENT 'Pedidos'\nPARTITIONED BY (anio INT)\nSTORED AS ORC\nLOCATION '/data/p';"
        );
        t.options.insert("stored_as".into(), "KUDU".into());
        t.options.insert("partitioned_by".into(), "HASH (id) PARTITIONS 4".into());
        t.options.remove("location");
        let i = ddl_of("impala", &t);
        assert!(i.contains("`id` INTEGER NOT NULL,"), "{i}");
        assert!(i.contains("`estado` VARCHAR(20) NOT NULL DEFAULT 'nuevo',"), "{i}");
        assert!(i.contains("    PRIMARY KEY (`id`)\n)\nPARTITION BY HASH (id) PARTITIONS 4\nCOMMENT 'Pedidos'\nSTORED AS KUDU;"), "{i}");
        let ins = insert_script(preset("hive"), Some("db"), "t", &["a".into()], &[vec![json!(true)], vec![json!(false)]]);
        assert_eq!(ins, "INSERT INTO `db`.`t` (`a`) VALUES\n  (TRUE),\n  (FALSE);");
    }

    #[test]
    fn identity_per_new_engine() {
        let t = sample();
        let has = |id: &str, want: &str| {
            let s = ddl_of(id, &t);
            assert!(s.contains(want), "{id}: {s}");
        };
        has("access", "[id] COUNTER NOT NULL,");
        has("zen", "\"id\" IDENTITY NOT NULL,");
        has("iris", "\"id\" IDENTITY NOT NULL,");
        has("cache", "\"id\" IDENTITY NOT NULL,");
        has("virtuoso", "\"id\" INTEGER IDENTITY NOT NULL,");
        has("maxdb", "\"id\" INTEGER DEFAULT SERIAL NOT NULL,");
        has("monetdb", "\"id\" INTEGER AUTO_INCREMENT NOT NULL,");
        has("ingres", "GENERATED BY DEFAULT AS IDENTITY");
        has("nuodb", "GENERATED BY DEFAULT AS IDENTITY");
        // No identity syntax the builder knows: the flag is dropped.
        for id in ["openedge", "mimer", "sqream", "heavydb", "machbase", "ignite", "ignite3", "dbase"] {
            let s = ddl_of(id, &t);
            assert!(!s.contains("IDENTITY") && !s.contains("AUTO_INCREMENT"), "{id}: {s}");
        }
    }

    #[test]
    fn engines_without_constraints_or_indexes() {
        let t = sample();
        for id in ["sqream", "heavydb", "spark", "kyuubi"] {
            let s = ddl_of(id, &t);
            assert!(!s.contains("FOREIGN KEY") && !s.contains("CREATE INDEX"), "{id}: {s}");
        }
        let s = ddl_of("machbase", &t);
        assert!(!s.contains("FOREIGN KEY") && s.contains("CREATE INDEX"), "{s}");
        // Spark and Kyuubi write Hive DDL.
        let s = ddl_of("spark", &t);
        assert!(s.contains("CREATE TABLE `app`.`pedidos` ("), "{s}");
        assert!(!designer(preset("spark")).auto_increment);
    }

    #[test]
    fn ignite_cache_options_go_in_with() {
        let mut t = sample();
        t.options.insert("with".into(), "template=partitioned,backups=1".into());
        let s = ddl_of("ignite", &t);
        assert!(s.contains("\n) WITH \"template=partitioned,backups=1\";"), "{s}");
        assert!(!s.contains("FOREIGN KEY"), "{s}");
        assert!(s.contains("CREATE INDEX"), "{s}");
    }

    #[test]
    fn access_and_dbase_use_brackets_and_single_row_inserts() {
        let ins = insert_script(preset("access"), None, "t", &["a".into()], &[vec![json!(1)], vec![json!(2)]]);
        assert_eq!(ins, "INSERT INTO [t] ([a]) VALUES (1);\nINSERT INTO [t] ([a]) VALUES (2);");
        let d = designer(preset("dbase"));
        assert!(!d.primary_key && !d.foreign_keys && !d.auto_increment);
    }

    #[test]
    fn capabilities_per_preset() {
        assert!(capabilities(preset("sybase")).create_database);
        assert!(capabilities(preset("odbc")).drop_database);
        assert!(!capabilities(preset("db2")).create_database);
        assert!(!capabilities(preset("hive")).foreign_keys);
        assert!(capabilities(preset("teradata")).foreign_keys);
    }

    #[test]
    fn explicit_null_is_stripped_only_from_column_lines() {
        assert_eq!(strip_explicit_null("    a INT NULL,\n    b INT NOT NULL\n);"), "    a INT,\n    b INT NOT NULL\n);");
    }
}
