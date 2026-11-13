//! The functions the editor completes through ODBC
//! ([`dbine_driver::Session::function_catalog`]), per engine (the generic
//! preset goes by the DBMS name):
//!
//! - Db2 LUW: `SYSCAT.ROUTINES` with `SYSCAT.ROUTINEPARMS` (the `SYSIBM`
//!   and `SYSFUN` built-ins and the user's functions), plus the static
//!   catalog's built-ins it doesn't list (special syntax such as `CAST`,
//!   `XMLELEMENT` or `JSON_TABLE`, window functions, special registers).
//! - Hive, Cloudera and Spark (Kyuubi too): `SHOW FUNCTIONS`, with kinds
//!   and arguments from the static catalog where it knows the name.
//! - Impala: `SHOW [AGGREGATE | ANALYTIC] FUNCTIONS IN _impala_builtins`
//!   (signature and result type) and the current database's UDFs.
//! - MonetDB: `sys.functions` with `sys.args`, without the internal
//!   helpers (`sql_add`, `zero_or_one`…).
//! - Every other dialect: its static catalog, kept for the server's
//!   version (`SQL_DBMS_VER`): Db2 for i and z/OS (Db2 LUW's without its
//!   LUW-only functions), Informix and GBase 8s, Teradata, Vertica, Exasol,
//!   Netezza, Altibase, CUBRID, SQL Anywhere, Access and dBase; SAP ASE,
//!   SQL Server (all of it on Azure SQL, whose version says 12), and Oracle
//!   and SQLite behind the generic preset the shared catalogs; Dameng the
//!   Oracle functions its manual documents.
//! - Anything else (and every fallback without a catalog): the SQL
//!   standard's functions ([`shared::ANSI`]) plus the ODBC scalar functions
//!   the driver says it supports (SQLGetInfo bitmasks) that engines name
//!   natively; the escape-only ones (`UCASE`, `IFNULL`…) are left out.
//!
//! Where the built-ins come from a static or generic catalog, the user's
//! functions come from SQLProcedures (`PROCEDURE_TYPE` 2) with their
//! arguments from SQLProcedureColumns; SQL Server and ASE, whose drivers
//! call every procedure a function there, from their system catalogs.
//!
//! A server read that fails falls back to the static catalog (or the
//! generic one) instead of failing. A function whose arguments aren't
//! known has `(...)` as its arguments: an empty `args` would read as a
//! function called without parentheses (`CURRENT_DATE`).

use crate::design::{self, Eng};
use crate::odbc::{Conn, StmtSlot};
use crate::presets::Preset;
use crate::{ffi, monitor, strip_version, OdbcSession, Rows};
use dbine_driver::functions::{parse_catalog, shared};
use dbine_driver::{FunctionInfo, FunctionKind, Result};
use std::collections::{HashMap, HashSet};

const DB2: &str = include_str!("functions/db2.txt");
const DB2_LUW: &str = include_str!("functions/db2_luw.txt");
const INFORMIX: &str = include_str!("functions/informix.txt");
const TERADATA: &str = include_str!("functions/teradata.txt");
const VERTICA: &str = include_str!("functions/vertica.txt");
const EXASOL: &str = include_str!("functions/exasol.txt");
const NETEZZA: &str = include_str!("functions/netezza.txt");
const ALTIBASE: &str = include_str!("functions/altibase.txt");
const CUBRID: &str = include_str!("functions/cubrid.txt");
const SQLANYWHERE: &str = include_str!("functions/sqlanywhere.txt");
const ACCESS: &str = include_str!("functions/access.txt");
const HIVE: &str = include_str!("functions/hive.txt");
const SPARK: &str = include_str!("functions/spark.txt");

/// What a function whose arguments aren't known gets as `args`.
const UNKNOWN_ARGS: &str = "(...)";

/// The release of Oracle whose functions Dameng (DM8) follows.
const DAMENG_AS_ORACLE: &str = "12.1";

/// The Oracle functions the DM8 SQL manual documents too: Dameng gets
/// their entries of [`shared::ORACLE`] (at [`DAMENG_AS_ORACLE`]). Oracle's
/// others (data mining, multitenant `CON_*`, the model clause, `ORA_*`…)
/// are not DM8's; DM8's own functions that Oracle lacks wait for a catalog
/// of their own.
const DAMENG_FROM_ORACLE: &str = "
    ABS ACOS ASIN ATAN ATAN2 BITAND CEIL COS COSH EXP FLOOR LN LOG MOD POWER ROUND SIGN SIN SINH
    SQRT TAN TANH TRUNC
    ASCII CHR CONCAT INITCAP INSTR INSTRB LENGTH LENGTHB LOWER LPAD LTRIM REGEXP_INSTR REGEXP_LIKE
    REGEXP_REPLACE REGEXP_SUBSTR REPLACE RPAD RTRIM SOUNDEX SUBSTR SUBSTRB TRANSLATE TRIM UPPER
    ADD_MONTHS CURRENT_DATE CURRENT_TIMESTAMP DBTIMEZONE EXTRACT FROM_TZ LAST_DAY LOCALTIMESTAMP
    MONTHS_BETWEEN NEXT_DAY NUMTODSINTERVAL NUMTOYMINTERVAL SESSIONTIMEZONE SYSDATE SYSTIMESTAMP
    TO_TIMESTAMP TZ_OFFSET
    CAST COALESCE DECODE GREATEST HEXTORAW LEAST NULLIF NVL NVL2 RAWTOHEX TO_BLOB TO_CHAR TO_DATE
    TO_NUMBER
    EMPTY_BLOB EMPTY_CLOB SYS_CONNECT_BY_PATH SYS_CONTEXT SYS_GUID UID USER USERENV
    AVG CORR COUNT COVAR_POP COVAR_SAMP GROUPING LISTAGG MAX MEDIAN MIN PERCENTILE_CONT
    PERCENTILE_DISC STDDEV STDDEV_POP STDDEV_SAMP SUM VAR_POP VAR_SAMP VARIANCE
    CUME_DIST DENSE_RANK FIRST_VALUE LAG LAST_VALUE LEAD NTH_VALUE NTILE PERCENT_RANK RANK
    RATIO_TO_REPORT ROW_NUMBER
";

/// SQL Server's `EngineEdition`s that run the newest engine whatever
/// `SQL_DBMS_VER` says (12.00.2000): 5 Azure SQL Database, 6 Synapse
/// dedicated pool, 8 Managed Instance, 11 Synapse serverless, 12 SQL
/// database in Fabric. The same list as the SQL Server driver's
/// (`crates/drivers/sqlserver/src/functions.rs`).
const MSSQL_EVERGREEN: &[i32] = &[5, 6, 8, 11, 12];

const MSSQL_EDITION: &str = "SELECT CAST(SERVERPROPERTY('EngineEdition') AS int)";

/// Where an engine's catalog comes from.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Source {
    /// The driver's ODBC scalar functions plus the SQL standard's.
    Generic,
    /// A static catalog, kept for the server's version when `gate`.
    Static { text: &'static str, gate: bool },
    /// [`shared::MSSQL`] for the server's version, or all of it on the
    /// cloud editions ([`MSSQL_EVERGREEN`]).
    SqlServer,
    /// The Oracle functions DM8 documents ([`DAMENG_FROM_ORACLE`]).
    Dameng,
    /// `SYSCAT.ROUTINES`, else the static Db2 catalog.
    Db2Luw,
    /// `SHOW FUNCTIONS` named with `text`'s kinds and arguments, else
    /// `text`.
    Show { text: &'static str },
    /// `SHOW … FUNCTIONS IN _impala_builtins`, else generic.
    Impala,
    /// `sys.functions`, else generic.
    MonetDb,
}

fn source(eng: Eng) -> Source {
    let fixed = |text| Source::Static { text, gate: true };
    match eng {
        Eng::Db2 => Source::Db2Luw,
        // Their releases don't follow Db2 LUW's: the whole catalog.
        Eng::Db2i | Eng::Db2zos => Source::Static {
            text: DB2,
            gate: false,
        },
        Eng::Ase => fixed(shared::SYBASE),
        Eng::SqlServer => Source::SqlServer,
        Eng::Dameng => Source::Dameng,
        Eng::Sqla => fixed(SQLANYWHERE),
        Eng::Hive => Source::Show { text: HIVE },
        Eng::Spark => Source::Show { text: SPARK },
        Eng::Impala => Source::Impala,
        Eng::Informix => fixed(INFORMIX),
        Eng::Teradata => fixed(TERADATA),
        Eng::Vertica => fixed(VERTICA),
        Eng::Exasol => fixed(EXASOL),
        Eng::Netezza => fixed(NETEZZA),
        Eng::Altibase => fixed(ALTIBASE),
        Eng::Cubrid => fixed(CUBRID),
        Eng::Access | Eng::DBase => Source::Static {
            text: ACCESS,
            gate: false,
        },
        Eng::MonetDb => Source::MonetDb,
        _ => Source::Generic,
    }
}

/// The source for `eng`; behind the generic preset, Oracle and SQLite
/// (which have no [`Eng`] of their own) by the DBMS name.
fn source_for(eng: Eng, dbms: &str) -> Source {
    let dbms = dbms.to_ascii_lowercase();
    match eng {
        Eng::Generic if dbms.contains("oracle") => Source::Static {
            text: shared::ORACLE,
            gate: true,
        },
        Eng::Generic if dbms.contains("sqlite") => Source::Static {
            text: shared::SQLITE,
            gate: true,
        },
        _ => source(eng),
    }
}

// ------------------------------------------------------------ ODBC scalars

/// The ODBC scalar and aggregate functions, by the SQLGetInfo mask and
/// bit that announce them (ODBC 3.8 Appendix E; the aggregate bits past
/// SUM are ODBC 4.0's). A bit says the driver takes the function inside
/// the escape `{fn NAME(...)}`, while the editor inserts a bare `NAME(...)`:
/// so only the names engines also have natively are here. The escape-only
/// ones (`UCASE`, `LCASE`, `IFNULL`, `DAYOFWEEK`, `DAYOFMONTH`, `CURDATE`,
/// `CURTIME`, `DATABASE()`, `USER()`, `CONVERT(value, SQL_type)` and
/// `TIMESTAMPADD` / `TIMESTAMPDIFF` with `SQL_TSI_*`) are left out.
const ODBC_FUNCTIONS: &[(u16, u32, &str)] = &[
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0000_0001,
        "s CONCAT(string_exp1, string_exp2)",
    ),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0000_0002,
        "s INSERT(string_exp1, start, length, string_exp2)",
    ),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0000_0004,
        "s LEFT(string_exp, count)",
    ),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0000_0008,
        "s LTRIM(string_exp)",
    ),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0000_0010,
        "s LENGTH(string_exp) -> INTEGER",
    ),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0000_0020,
        "s LOCATE(string_exp1, string_exp2 [, start]) -> INTEGER",
    ),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0000_0080,
        "s REPEAT(string_exp, count)",
    ),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0000_0100,
        "s REPLACE(string_exp1, string_exp2, string_exp3)",
    ),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0000_0200,
        "s RIGHT(string_exp, count)",
    ),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0000_0400,
        "s RTRIM(string_exp)",
    ),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0000_0800,
        "s SUBSTRING(string_exp, start, length)",
    ),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0000_2000,
        "s ASCII(string_exp) -> INTEGER",
    ),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0000_4000,
        "s CHAR(code) -> CHAR",
    ),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0000_8000,
        "s DIFFERENCE(string_exp1, string_exp2) -> INTEGER",
    ),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0001_0000,
        "s LOCATE(string_exp1, string_exp2) -> INTEGER",
    ),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0002_0000,
        "s SOUNDEX(string_exp)",
    ),
    (ffi::SQL_STRING_FUNCTIONS, 0x0004_0000, "s SPACE(count)"),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0008_0000,
        "s BIT_LENGTH(string_exp) -> INTEGER",
    ),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0010_0000,
        "s CHAR_LENGTH(string_exp) -> INTEGER",
    ),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0020_0000,
        "s CHARACTER_LENGTH(string_exp) -> INTEGER",
    ),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0040_0000,
        "s OCTET_LENGTH(string_exp) -> INTEGER",
    ),
    (
        ffi::SQL_STRING_FUNCTIONS,
        0x0080_0000,
        "s POSITION(character_exp IN character_exp) -> INTEGER",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0000_0001,
        "s ABS(numeric_exp)",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0000_0002,
        "s ACOS(float_exp) -> FLOAT",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0000_0004,
        "s ASIN(float_exp) -> FLOAT",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0000_0008,
        "s ATAN(float_exp) -> FLOAT",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0000_0010,
        "s ATAN2(float_exp1, float_exp2) -> FLOAT",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0000_0020,
        "s CEILING(numeric_exp)",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0000_0040,
        "s COS(float_exp) -> FLOAT",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0000_0080,
        "s COT(float_exp) -> FLOAT",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0000_0100,
        "s EXP(float_exp) -> FLOAT",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0000_0200,
        "s FLOOR(numeric_exp)",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0000_0400,
        "s LOG(float_exp) -> FLOAT",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0000_0800,
        "s MOD(integer_exp1, integer_exp2) -> INTEGER",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0000_1000,
        "s SIGN(numeric_exp) -> INTEGER",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0000_2000,
        "s SIN(float_exp) -> FLOAT",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0000_4000,
        "s SQRT(float_exp) -> FLOAT",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0000_8000,
        "s TAN(float_exp) -> FLOAT",
    ),
    (ffi::SQL_NUMERIC_FUNCTIONS, 0x0001_0000, "s PI() -> FLOAT"),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0002_0000,
        "s RAND([integer_exp]) -> FLOAT",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0004_0000,
        "s DEGREES(numeric_exp)",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0008_0000,
        "s LOG10(float_exp) -> FLOAT",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0010_0000,
        "s POWER(numeric_exp, integer_exp)",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0020_0000,
        "s RADIANS(numeric_exp)",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0040_0000,
        "s ROUND(numeric_exp, integer_exp)",
    ),
    (
        ffi::SQL_NUMERIC_FUNCTIONS,
        0x0080_0000,
        "s TRUNCATE(numeric_exp, integer_exp)",
    ),
    (
        ffi::SQL_TIMEDATE_FUNCTIONS,
        0x0000_0001,
        "s NOW() -> TIMESTAMP",
    ),
    (
        ffi::SQL_TIMEDATE_FUNCTIONS,
        0x0000_0010,
        "s DAYOFYEAR(date_exp) -> INTEGER",
    ),
    (
        ffi::SQL_TIMEDATE_FUNCTIONS,
        0x0000_0020,
        "s MONTH(date_exp) -> INTEGER",
    ),
    (
        ffi::SQL_TIMEDATE_FUNCTIONS,
        0x0000_0040,
        "s QUARTER(date_exp) -> INTEGER",
    ),
    (
        ffi::SQL_TIMEDATE_FUNCTIONS,
        0x0000_0080,
        "s WEEK(date_exp) -> INTEGER",
    ),
    (
        ffi::SQL_TIMEDATE_FUNCTIONS,
        0x0000_0100,
        "s YEAR(date_exp) -> INTEGER",
    ),
    (
        ffi::SQL_TIMEDATE_FUNCTIONS,
        0x0000_0400,
        "s HOUR(time_exp) -> INTEGER",
    ),
    (
        ffi::SQL_TIMEDATE_FUNCTIONS,
        0x0000_0800,
        "s MINUTE(time_exp) -> INTEGER",
    ),
    (
        ffi::SQL_TIMEDATE_FUNCTIONS,
        0x0000_1000,
        "s SECOND(time_exp) -> INTEGER",
    ),
    (
        ffi::SQL_TIMEDATE_FUNCTIONS,
        0x0000_8000,
        "s DAYNAME(date_exp)",
    ),
    (
        ffi::SQL_TIMEDATE_FUNCTIONS,
        0x0001_0000,
        "s MONTHNAME(date_exp)",
    ),
    (
        ffi::SQL_TIMEDATE_FUNCTIONS,
        0x0002_0000,
        "s CURRENT_DATE -> DATE",
    ),
    (
        ffi::SQL_TIMEDATE_FUNCTIONS,
        0x0004_0000,
        "s CURRENT_TIME -> TIME",
    ),
    (
        ffi::SQL_TIMEDATE_FUNCTIONS,
        0x0008_0000,
        "s CURRENT_TIMESTAMP -> TIMESTAMP",
    ),
    (
        ffi::SQL_TIMEDATE_FUNCTIONS,
        0x0010_0000,
        "s EXTRACT(extract_field FROM extract_source) -> INTEGER",
    ),
    (
        ffi::SQL_CONVERT_FUNCTIONS,
        0x0000_0002,
        "s CAST(value_exp AS data_type)",
    ),
    (
        ffi::SQL_AGGREGATE_FUNCTIONS,
        0x0000_0001,
        "a AVG([DISTINCT | ALL] expr)",
    ),
    (
        ffi::SQL_AGGREGATE_FUNCTIONS,
        0x0000_0002,
        "a COUNT(* | [DISTINCT | ALL] expr)",
    ),
    (
        ffi::SQL_AGGREGATE_FUNCTIONS,
        0x0000_0004,
        "a MAX([DISTINCT | ALL] expr)",
    ),
    (
        ffi::SQL_AGGREGATE_FUNCTIONS,
        0x0000_0008,
        "a MIN([DISTINCT | ALL] expr)",
    ),
    (
        ffi::SQL_AGGREGATE_FUNCTIONS,
        0x0000_0010,
        "a SUM([DISTINCT | ALL] expr)",
    ),
    (
        ffi::SQL_AGGREGATE_FUNCTIONS,
        0x0000_0080,
        "a EVERY(boolean_exp)",
    ),
    (
        ffi::SQL_AGGREGATE_FUNCTIONS,
        0x0000_0200,
        "a STDDEV_POP(expr)",
    ),
    (
        ffi::SQL_AGGREGATE_FUNCTIONS,
        0x0000_0400,
        "a STDDEV_SAMP(expr)",
    ),
    (
        ffi::SQL_AGGREGATE_FUNCTIONS,
        0x0000_0800,
        "a VAR_SAMP(expr)",
    ),
    (ffi::SQL_AGGREGATE_FUNCTIONS, 0x0000_1000, "a VAR_POP(expr)"),
    (
        ffi::SQL_AGGREGATE_FUNCTIONS,
        0x0000_2000,
        "a ARRAY_AGG(expr)",
    ),
];

/// The SQLGetInfo masks [`ODBC_FUNCTIONS`] reads.
pub(crate) const FUNCTION_MASKS: [u16; 5] = [
    ffi::SQL_STRING_FUNCTIONS,
    ffi::SQL_NUMERIC_FUNCTIONS,
    ffi::SQL_TIMEDATE_FUNCTIONS,
    ffi::SQL_CONVERT_FUNCTIONS,
    ffi::SQL_AGGREGATE_FUNCTIONS,
];

/// The ODBC functions `masks` (SQLGetInfo type → bitmask) announce, then
/// the SQL standard's, without repeating a name and signature.
pub(crate) fn generic(masks: &HashMap<u16, u32>) -> Vec<FunctionInfo> {
    let mut out: Vec<FunctionInfo> = ODBC_FUNCTIONS
        .iter()
        .filter(|(what, bit, _)| masks.get(what).is_some_and(|m| m & bit != 0))
        .flat_map(|(_, _, line)| parse_catalog(line, None))
        .collect();
    out.extend(parse_catalog(shared::ANSI, None));
    dedupe(out)
}

/// Drops repeated entries (same kind, name ignoring case, and arguments).
fn dedupe(entries: Vec<FunctionInfo>) -> Vec<FunctionInfo> {
    let mut seen = HashSet::new();
    entries
        .into_iter()
        .filter(|f| {
            seen.insert((
                f.kind,
                f.schema.to_ascii_uppercase(),
                f.name.to_ascii_uppercase(),
                f.args.clone(),
            ))
        })
        .collect()
}

/// A name a completion can insert: letters, digits, `_`, `$` and `#`, not
/// starting with a digit (operators such as `+` or `<=>` are left out).
fn is_identifier(name: &str) -> bool {
    name.starts_with(|c: char| c.is_alphabetic() || c == '_')
        && name
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '$' | '#'))
}

fn col(r: &[Option<String>], i: usize) -> Option<String> {
    r.get(i)
        .cloned()
        .flatten()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ------------------------------------------------------------------- Db2

/// The `SYSIBM` and `SYSFUN` built-ins and the user's functions, one row
/// per parameter (`P` input, `C` result after casting, `R` result or a
/// table function's column).
const DB2_ROUTINES: &str = "SELECT r.ROUTINESCHEMA, r.ROUTINENAME, r.SPECIFICNAME, r.FUNCTIONTYPE,
       p.ROWTYPE, p.ORDINAL, p.PARMNAME, p.TYPENAME, r.REMARKS
  FROM SYSCAT.ROUTINES r
  LEFT JOIN SYSCAT.ROUTINEPARMS p
    ON p.ROUTINESCHEMA = r.ROUTINESCHEMA AND p.SPECIFICNAME = r.SPECIFICNAME
 WHERE r.ROUTINETYPE = 'F' AND r.ORIGIN <> 'S'
   AND (r.ROUTINESCHEMA IN ('SYSIBM', 'SYSFUN')
        OR (r.ROUTINESCHEMA NOT LIKE 'SYS%' AND r.ROUTINESCHEMA NOT IN ('SQLJ', 'NULLID')))
 ORDER BY r.ROUTINESCHEMA, r.ROUTINENAME, r.SPECIFICNAME, p.ROWTYPE, p.ORDINAL";

/// `DB2_ROUTINES` rows as catalog entries: built-ins without a schema,
/// the user's with theirs.
pub(crate) fn db2_routines(rows: &Rows) -> Vec<FunctionInfo> {
    let mut out = Vec::new();
    let key = |r: &Vec<Option<String>>| (col(r, 0), col(r, 2));
    for group in rows.chunk_by(|a, b| key(a) == key(b)) {
        let r = &group[0];
        let (Some(schema), Some(name)) = (col(r, 0), col(r, 1)) else {
            continue;
        };
        if !is_identifier(&name) {
            continue;
        }
        let kind = match col(r, 3).as_deref() {
            Some("C") => FunctionKind::Aggregate,
            Some("T") => FunctionKind::Table,
            _ => FunctionKind::Scalar,
        };
        let role = |p: &Vec<Option<String>>| col(p, 4).unwrap_or_default();
        let args: Vec<String> = group
            .iter()
            .filter(|p| role(p) == "P")
            .map(|p| {
                let ty = col(p, 7).unwrap_or_default();
                match col(p, 6) {
                    Some(n) => format!("{} {ty}", n.to_ascii_lowercase()),
                    None => ty,
                }
            })
            .collect();
        let returns = if kind == FunctionKind::Table {
            "TABLE".to_string()
        } else {
            group
                .iter()
                .find(|p| role(p) == "C")
                .or_else(|| group.iter().find(|p| role(p) == "R"))
                .and_then(|p| col(p, 7))
                .unwrap_or_default()
        };
        let builtin = matches!(schema.as_str(), "SYSIBM" | "SYSFUN");
        let mut f = FunctionInfo::new(name, kind)
            .args(format!("({})", args.join(", ")))
            .returns(returns);
        if !builtin {
            f = f.schema(schema);
        }
        if let Some(d) = col(r, 8) {
            f = f.description(d);
        }
        out.push(f);
    }
    out
}

/// Db2's static catalog for `version`: Db2 LUW's whole (`luw`), or the
/// part Db2 for i and z/OS share.
fn db2_static(version: Option<&str>, luw: bool) -> Vec<FunctionInfo> {
    let mut out = parse_catalog(DB2, version);
    if luw {
        out.extend(parse_catalog(DB2_LUW, version));
    }
    out
}

/// The static entries whose name the server's built-ins (`have` without a
/// schema) don't list: what it parses as syntax rather than catalogs
/// (`CAST`, `XMLELEMENT`, `JSON_TABLE`, window functions, special
/// registers…). Every static entry is documented, so nothing invented
/// comes in.
fn missing_builtins(statics: Vec<FunctionInfo>, have: &[FunctionInfo]) -> Vec<FunctionInfo> {
    let names: HashSet<String> = have
        .iter()
        .filter(|f| f.schema.is_empty())
        .map(|f| f.name.to_ascii_uppercase())
        .collect();
    statics
        .into_iter()
        .filter(|f| !names.contains(&f.name.to_ascii_uppercase()))
        .collect()
}

// ------------------------------------------------------ Hive, Spark, Impala

/// What `SHOW FUNCTIONS` lists that is syntax, not a function.
const NOT_FUNCTIONS: &[&str] = &["and", "or", "not", "in", "between", "case", "when"];

/// `SHOW FUNCTIONS` rows (Hive, Spark) as catalog entries: kinds and
/// arguments from `text` where it has the name, a scalar otherwise; a
/// qualified name (`db.fn`, `spark_catalog.db.fn`) is a user's function.
pub(crate) fn shown_functions(rows: &Rows, text: &str) -> Vec<FunctionInfo> {
    let mut known: HashMap<String, Vec<FunctionInfo>> = HashMap::new();
    for f in parse_catalog(text, None) {
        known
            .entry(f.name.to_ascii_lowercase())
            .or_default()
            .push(f);
    }
    let mut out = Vec::new();
    for r in rows {
        let Some(full) = col(r, 0) else {
            continue;
        };
        let (schema, name) = match full.rsplit_once('.') {
            Some((s, n)) => (s.to_string(), n.to_string()),
            None => (String::new(), full.clone()),
        };
        if !is_identifier(&name) || (schema.is_empty() && NOT_FUNCTIONS.contains(&name.as_str())) {
            continue;
        }
        match known
            .get(&name.to_ascii_lowercase())
            .filter(|_| schema.is_empty())
        {
            Some(entries) => out.extend(entries.iter().cloned()),
            None => out.push(
                FunctionInfo::new(name, FunctionKind::Scalar)
                    .args(UNKNOWN_ARGS)
                    .schema(schema),
            ),
        }
    }
    dedupe(out)
}

const IMPALA_BUILTINS: [(&str, FunctionKind); 3] = [
    ("SHOW FUNCTIONS IN _impala_builtins", FunctionKind::Scalar),
    (
        "SHOW AGGREGATE FUNCTIONS IN _impala_builtins",
        FunctionKind::Aggregate,
    ),
    (
        "SHOW ANALYTIC FUNCTIONS IN _impala_builtins",
        FunctionKind::Window,
    ),
];

/// The current database's UDFs.
const IMPALA_UDFS: [(&str, FunctionKind); 2] = [
    ("SHOW FUNCTIONS", FunctionKind::Scalar),
    ("SHOW AGGREGATE FUNCTIONS", FunctionKind::Aggregate),
];

/// Impala's `SHOW … FUNCTIONS` rows (return type, signature, …) as
/// `kind` entries in `schema` (empty for the built-ins).
pub(crate) fn impala_functions(rows: &Rows, kind: FunctionKind, schema: &str) -> Vec<FunctionInfo> {
    rows.iter()
        .filter_map(|r| {
            let returns = col(r, 0).unwrap_or_default();
            let sig = col(r, 1)?;
            let (name, args) = match sig.find('(') {
                Some(i) => (sig[..i].trim().to_string(), sig[i..].trim().to_string()),
                None => (sig.clone(), String::new()),
            };
            (is_identifier(&name) && !name.starts_with('_')).then(|| {
                FunctionInfo::new(name, kind)
                    .args(args)
                    .returns(returns)
                    .schema(schema)
            })
        })
        .collect()
}

// ----------------------------------------------------------------- MonetDB

/// MonetDB's functions (1 scalar, 3 aggregate, 5 table, 6 window) with
/// their arguments (`inout` 0: a result, 1: an argument).
const MONETDB_FUNCTIONS: &str =
    "SELECT s.name, f.name, f.id, f.type, a.inout, a.number, a.name, a.type
  FROM sys.functions f
  JOIN sys.schemas s ON s.id = f.schema_id
  LEFT JOIN sys.args a ON a.func_id = f.id
 WHERE f.type IN (1, 3, 5, 6)
 ORDER BY s.name, f.name, f.id, a.inout, a.number";

/// What `sys.functions` lists that MonetDB doesn't document: the
/// functions behind operators (`sql_add`, `sql_sub`, `sql_neg`, `sql_min`…
/// all start with `sql_`) and the helpers of subqueries, booleans, `LIKE`
/// and windows.
const MONETDB_INTERNAL: &[&str] = &[
    "zero_or_one",
    "not_unique",
    "all",
    "null",
    "exist",
    "not_exist",
    "anyequal",
    "allnotequal",
    "identity",
    "rowid",
    "hash",
    "rotate_xor_hash",
    "ifthenelse",
    "diff",
    "window_bound",
    "and",
    "or",
    "xor",
    "not",
    "like",
    "ilike",
    "not_like",
    "not_ilike",
];

fn monetdb_internal(schema: &str, name: &str) -> bool {
    schema == "sys" && (name.starts_with("sql_") || MONETDB_INTERNAL.contains(&name))
}

/// `MONETDB_FUNCTIONS` rows as catalog entries: `sys` and `tmp` without a
/// schema, the others (`profiler`, `logging`, `json`, the user's) with
/// theirs, since MonetDB only finds them qualified (`profiler.getlimit()`).
pub(crate) fn monetdb_functions(rows: &Rows) -> Vec<FunctionInfo> {
    let mut out = Vec::new();
    let key = |r: &Vec<Option<String>>| (col(r, 0), col(r, 2));
    for group in rows.chunk_by(|a, b| key(a) == key(b)) {
        let r = &group[0];
        let (Some(schema), Some(name)) = (col(r, 0), col(r, 1)) else {
            continue;
        };
        if !is_identifier(&name) || monetdb_internal(&schema, &name) {
            continue;
        }
        let kind = match col(r, 3).as_deref() {
            Some("3") => FunctionKind::Aggregate,
            Some("5") => FunctionKind::Table,
            Some("6") => FunctionKind::Window,
            _ => FunctionKind::Scalar,
        };
        let inout = |p: &Vec<Option<String>>| col(p, 4);
        let arg = |p: &Vec<Option<String>>| {
            let ty = col(p, 7).unwrap_or_default();
            match col(p, 6).filter(|n| !n.starts_with("arg_") && n != "res_0") {
                Some(n) => format!("{n} {ty}"),
                None => ty,
            }
        };
        let args: Vec<String> = group
            .iter()
            .filter(|p| inout(p).as_deref() == Some("1"))
            .map(arg)
            .collect();
        let returns = if kind == FunctionKind::Table {
            "TABLE".to_string()
        } else {
            group
                .iter()
                .find(|p| inout(p).as_deref() == Some("0"))
                .and_then(|p| col(p, 7))
                .unwrap_or_default()
        };
        let mut f = FunctionInfo::new(name, kind)
            .args(format!("({})", args.join(", ")))
            .returns(returns);
        if !matches!(schema.as_str(), "sys" | "tmp") {
            f = f.schema(schema);
        }
        out.push(f);
    }
    dedupe(out)
}

// ---------------------------------------------------------- user functions

/// SQL Server's own functions: scalar, inline and multi-statement
/// table-valued, CLR (its driver's SQLProcedures calls every procedure a
/// function).
const MSSQL_USER_FUNCTIONS: &str = "SELECT SCHEMA_NAME(schema_id), name, RTRIM(type)
  FROM sys.objects
 WHERE type IN ('FN', 'FS', 'IF', 'TF', 'FT', 'AF') AND is_ms_shipped = 0";

/// ASE's SQL functions (`CREATE FUNCTION`, 15.0.2 on), for the same reason.
const ASE_USER_FUNCTIONS: &str =
    "SELECT user_name(uid), name, 'FN' FROM sysobjects WHERE type = 'SF'";

/// Above this many schemas with functions, SQLProcedureColumns is called
/// once for every schema instead of once per schema.
const MAX_SCHEMA_READS: usize = 16;

/// A user's function before its arguments are known.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UserFunction {
    pub schema: String,
    pub name: String,
    pub kind: FunctionKind,
    pub description: String,
}

/// SQLProcedures rows: the functions (`PROCEDURE_TYPE` 2, SQL_PT_FUNCTION)
/// outside the `system` schemas.
pub(crate) fn listed_functions(rows: &Rows, system: impl Fn(&str) -> bool) -> Vec<UserFunction> {
    rows.iter()
        .filter_map(|r| {
            if col(r, 7).as_deref() != Some("2") {
                return None;
            }
            let schema = col(r, 1).unwrap_or_default();
            if !schema.is_empty() && system(&schema) {
                return None;
            }
            Some(UserFunction {
                schema,
                name: strip_version(&col(r, 2)?).to_string(),
                kind: FunctionKind::Scalar,
                description: col(r, 6).unwrap_or_default(),
            })
        })
        .collect()
}

/// `MSSQL_USER_FUNCTIONS` and `ASE_USER_FUNCTIONS` rows (schema, name,
/// object type).
pub(crate) fn catalog_functions(rows: &Rows) -> Vec<UserFunction> {
    rows.iter()
        .filter_map(|r| {
            let kind = match col(r, 2).as_deref() {
                Some("IF" | "TF" | "FT") => FunctionKind::Table,
                Some("AF") => FunctionKind::Aggregate,
                _ => FunctionKind::Scalar,
            };
            Some(UserFunction {
                schema: col(r, 0).unwrap_or_default(),
                name: col(r, 1)?,
                kind,
                description: String::new(),
            })
        })
        .collect()
}

/// `functions` as catalog entries, with their arguments and result from
/// the SQLProcedureColumns rows of their schemas (`COLUMN_TYPE` 1 input, 2
/// in/out, 4 output, 5 the result, 3 a result column: a table function).
/// Not read (`None`), or ambiguous (overloads under one name), the
/// arguments are [`UNKNOWN_ARGS`].
pub(crate) fn with_arguments(
    functions: Vec<UserFunction>,
    columns: Option<&Rows>,
) -> Vec<FunctionInfo> {
    fn role(r: &[Option<String>]) -> String {
        col(r, 4).unwrap_or_default()
    }
    fn ordinal(r: &[Option<String>]) -> Option<i64> {
        col(r, 17).and_then(|o| o.parse().ok())
    }
    let mut by_function: HashMap<(String, String), Vec<&Vec<Option<String>>>> = HashMap::new();
    for r in columns.into_iter().flatten() {
        let Some(name) = col(r, 2) else {
            continue;
        };
        let schema = col(r, 1).unwrap_or_default().to_ascii_uppercase();
        let name = strip_version(&name).to_ascii_uppercase();
        by_function.entry((schema, name)).or_default().push(r);
    }
    let mut out = Vec::new();
    for f in functions {
        let key = (f.schema.to_ascii_uppercase(), f.name.to_ascii_uppercase());
        let rows = by_function.get(&key).map(Vec::as_slice).unwrap_or_default();
        let mut params: Vec<&Vec<Option<String>>> = rows
            .iter()
            .copied()
            .filter(|r| matches!(role(r).as_str(), "1" | "2" | "4"))
            .collect();
        params.sort_by_key(|r| ordinal(r));
        // Overloads under one name: their parameters share positions.
        let mut positions: Vec<i64> = params.iter().filter_map(|r| ordinal(r)).collect();
        let numbered = positions.len();
        positions.dedup();
        let overloaded =
            positions.len() < numbered || rows.iter().filter(|r| role(r) == "5").count() > 1;
        let kind = if rows.iter().any(|r| role(r) == "3") {
            FunctionKind::Table
        } else {
            f.kind
        };
        let args = if columns.is_none() || overloaded {
            UNKNOWN_ARGS.to_string()
        } else {
            let list: Vec<String> = params
                .iter()
                .map(|r| {
                    let mode = match role(r).as_str() {
                        "2" => "INOUT ",
                        "4" => "OUT ",
                        _ => "",
                    };
                    let ty = col(r, 6).unwrap_or_default();
                    let arg = match col(r, 3) {
                        Some(n) => format!("{mode}{} {ty}", n.to_ascii_lowercase()),
                        None => format!("{mode}{ty}"),
                    };
                    arg.trim_end().to_string()
                })
                .collect();
            format!("({})", list.join(", "))
        };
        let returns = if kind == FunctionKind::Table {
            "TABLE".to_string()
        } else {
            rows.iter()
                .find(|r| role(r) == "5")
                .and_then(|r| col(r, 6))
                .unwrap_or_default()
        };
        out.push(
            FunctionInfo::new(f.name, kind)
                .args(args)
                .returns(returns)
                .description(f.description)
                .schema(f.schema),
        );
    }
    out
}

/// The user's functions: their names from SQLProcedures (from the system
/// catalog on SQL Server and ASE), their arguments from
/// SQLProcedureColumns. Nothing when the driver can't say.
fn user_functions(c: &Conn, slot: &StmtSlot, preset: &Preset, eng: Eng) -> Vec<FunctionInfo> {
    let found = match eng {
        Eng::SqlServer => query(c, slot, MSSQL_USER_FUNCTIONS).map(|r| catalog_functions(&r)),
        Eng::Ase => query(c, slot, ASE_USER_FUNCTIONS).map(|r| catalog_functions(&r)),
        _ => c
            .stmt(slot)
            .and_then(|st| {
                st.procedures()?;
                st.text_rows()
            })
            .map(|r| listed_functions(&r, |s| preset.is_system_schema(s))),
    };
    let functions = match found {
        Ok(f) if !f.is_empty() => f,
        Ok(_) => return Vec::new(),
        Err(e) => {
            tracing::debug!("odbc: the user's functions not read: {e}");
            return Vec::new();
        }
    };
    let mut schemas: Vec<&str> = functions.iter().map(|f| f.schema.as_str()).collect();
    schemas.sort_unstable();
    schemas.dedup();
    if schemas.len() > MAX_SCHEMA_READS || schemas.contains(&"") {
        schemas = vec!["%"];
    }
    let mut columns = Vec::new();
    let mut read = true;
    for schema in schemas {
        let rows = c.stmt(slot).and_then(|st| {
            st.procedure_columns(schema)?;
            st.text_rows()
        });
        match rows {
            Ok(rows) => columns.extend(rows),
            Err(e) => {
                tracing::debug!("odbc: SQLProcedureColumns failed: {e}");
                read = false;
                break;
            }
        }
    }
    let mut out = with_arguments(functions, read.then_some(&columns));
    if !preset.has_schemas {
        for f in &mut out {
            f.schema.clear();
        }
    }
    out
}

// ----------------------------------------------------------------- session

fn query(c: &Conn, slot: &StmtSlot, sql: &str) -> Result<Rows> {
    let st = c.stmt(slot)?;
    st.exec(sql)?;
    st.text_rows()
}

fn masks(c: &Conn) -> HashMap<u16, u32> {
    FUNCTION_MASKS
        .iter()
        .filter_map(|&what| c.info_u32(what).map(|m| (what, m)))
        .collect()
}

/// `text` for `version`; the generic catalog when it has nothing (a
/// shared catalog not filled yet).
fn static_or_generic(c: &Conn, text: &str, version: Option<&str>) -> Vec<FunctionInfo> {
    let out = parse_catalog(text, version);
    if out.is_empty() {
        generic(&masks(c))
    } else {
        out
    }
}

/// Dameng's built-ins: the Oracle entries DM8 documents too.
fn dameng() -> Vec<FunctionInfo> {
    parse_catalog(shared::ORACLE, Some(DAMENG_AS_ORACLE))
        .into_iter()
        .filter(|f| DAMENG_FROM_ORACLE.split_whitespace().any(|n| n == f.name))
        .collect()
}

/// The version SQL Server's catalog is kept for: none (all of it) on the
/// cloud editions, whose `SQL_DBMS_VER` stays at 12.
fn mssql_version(version: Option<&str>, edition: Option<i32>) -> Option<&str> {
    version.filter(|_| !edition.is_some_and(|e| MSSQL_EVERGREEN.contains(&e)))
}

/// The catalog from `source`, read on `c`, for `eng` behind `preset`.
fn catalog(
    c: &Conn,
    slot: &StmtSlot,
    preset: &Preset,
    eng: Eng,
    source: Source,
    database: &str,
) -> Vec<FunctionInfo> {
    let dbms_ver = c.info_string(ffi::SQL_DBMS_VER);
    // GBase 8s's version doesn't follow Informix's.
    let gate = preset.id != "gbase8s";
    let version = Some(dbms_ver.as_str()).filter(|v| gate && !v.trim().is_empty());
    let fallback = |what: &str, e: dbine_driver::Error| {
        tracing::debug!("odbc: {what} not read, static catalog instead: {e}");
    };
    // The built-ins, and whether the user's functions are among them.
    let (mut out, with_users) = match source {
        Source::Generic => (generic(&masks(c)), false),
        Source::Static { text, gate } => {
            (static_or_generic(c, text, version.filter(|_| gate)), false)
        }
        Source::SqlServer => {
            let edition = query(c, slot, MSSQL_EDITION)
                .ok()
                .and_then(|rows| col(rows.first()?, 0))
                .and_then(|e| e.parse::<i32>().ok());
            (
                static_or_generic(c, shared::MSSQL, mssql_version(version, edition)),
                false,
            )
        }
        Source::Dameng => (dameng(), false),
        Source::Db2Luw => match query(c, slot, DB2_ROUTINES) {
            Ok(rows) => {
                let mut out = db2_routines(&rows);
                let extra = missing_builtins(db2_static(version, true), &out);
                out.extend(extra);
                (out, true)
            }
            Err(e) => {
                fallback("SYSCAT.ROUTINES", e);
                (db2_static(version, true), false)
            }
        },
        Source::Show { text } => match query(c, slot, "SHOW FUNCTIONS") {
            Ok(rows) => (shown_functions(&rows, text), true),
            Err(e) => {
                fallback("SHOW FUNCTIONS", e);
                (parse_catalog(text, None), true)
            }
        },
        Source::Impala => (impala(c, slot, database), true),
        Source::MonetDb => match query(c, slot, MONETDB_FUNCTIONS) {
            Ok(rows) => (monetdb_functions(&rows), true),
            Err(e) => {
                fallback("sys.functions", e);
                (generic(&masks(c)), true)
            }
        },
    };
    if !with_users && preset.procedures && preset.functions {
        out.extend(user_functions(c, slot, preset, eng));
    }
    out
}

/// Impala's built-ins and the current database's UDFs; the generic catalog
/// when the built-ins can't be read.
fn impala(c: &Conn, slot: &StmtSlot, database: &str) -> Vec<FunctionInfo> {
    let mut out = Vec::new();
    for (sql, kind) in IMPALA_BUILTINS {
        match query(c, slot, sql) {
            Ok(rows) => out.extend(impala_functions(&rows, kind, "")),
            Err(e) => {
                tracing::debug!("odbc: {sql} not read, generic catalog instead: {e}");
                return generic(&masks(c));
            }
        }
    }
    for (sql, kind) in IMPALA_UDFS {
        match query(c, slot, sql) {
            Ok(rows) => out.extend(impala_functions(&rows, kind, database)),
            Err(e) => tracing::debug!("odbc: {sql}: {e}"),
        }
    }
    dedupe(out)
}

impl OdbcSession {
    pub(crate) async fn function_catalog_impl(&mut self) -> Result<Vec<FunctionInfo>> {
        let eng = match design::eng(self.preset) {
            Eng::Generic => monitor::eng_from_dbms(&self.dbms).unwrap_or(Eng::Generic),
            e => e,
        };
        let source = source_for(eng, &self.dbms);
        let preset = self.preset;
        let database = self.database.clone();
        self.run(move |c, slot| Ok(catalog(c, slot, preset, eng, source, &database)))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [(&str, &str); 13] = [
        ("db2", DB2),
        ("db2_luw", DB2_LUW),
        ("informix", INFORMIX),
        ("teradata", TERADATA),
        ("vertica", VERTICA),
        ("exasol", EXASOL),
        ("netezza", NETEZZA),
        ("altibase", ALTIBASE),
        ("cubrid", CUBRID),
        ("sqlanywhere", SQLANYWHERE),
        ("access", ACCESS),
        ("hive", HIVE),
        ("spark", SPARK),
    ];

    fn names(c: &[FunctionInfo]) -> HashSet<String> {
        c.iter().map(|f| f.name.to_ascii_uppercase()).collect()
    }

    #[test]
    fn every_catalog_parses_line_by_line() {
        for (engine, text) in ALL {
            let mut n = 0;
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                assert_eq!(
                    parse_catalog(line, None).len(),
                    1,
                    "{engine}: unreadable: {line}"
                );
                n += 1;
            }
            assert!(n >= 20, "{engine}: only {n} entries");
        }
        for (_, _, line) in ODBC_FUNCTIONS {
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
    }

    #[test]
    fn every_preset_has_a_source() {
        for p in crate::presets::PRESETS {
            if let Source::Static { text, .. } | Source::Show { text } = source(design::eng(p)) {
                assert!(!parse_catalog(text, None).is_empty(), "{}", p.id);
            }
        }
        assert_eq!(source(Eng::Db2), Source::Db2Luw);
        assert_eq!(source(Eng::SqlServer), Source::SqlServer);
        assert_eq!(source(Eng::Dameng), Source::Dameng);
        assert_eq!(source(Eng::Spark), Source::Show { text: SPARK });
        assert_eq!(source(Eng::Ocient), Source::Generic);
        assert_eq!(source(Eng::MonetDb), Source::MonetDb);
        // Behind the generic preset, Oracle and SQLite by the DBMS name.
        let oracle = source_for(Eng::Generic, "Oracle");
        assert_eq!(
            oracle,
            Source::Static {
                text: shared::ORACLE,
                gate: true
            }
        );
        assert_eq!(
            source_for(Eng::Generic, "SQLite"),
            Source::Static {
                text: shared::SQLITE,
                gate: true
            }
        );
        assert_eq!(source_for(Eng::Generic, "Ocient"), Source::Generic);
        assert_eq!(source_for(Eng::Teradata, "Teradata"), source(Eng::Teradata));
    }

    #[test]
    fn azure_sql_gets_the_whole_sql_server_catalog() {
        let has = |v, n: &str| parse_catalog(shared::MSSQL, v).iter().any(|f| f.name == n);
        // Azure SQL Database reports 12.00.2000 and has STRING_AGG (2017).
        let azure = mssql_version(Some("12.00.2000"), Some(5));
        assert_eq!(azure, None);
        assert!(has(azure, "STRING_AGG") && has(azure, "TRIM"));
        assert_eq!(mssql_version(Some("12.00.2000"), Some(8)), None);
        // A boxed SQL Server 2014 doesn't.
        let boxed = mssql_version(Some("12.00.2000"), Some(3));
        assert_eq!(boxed, Some("12.00.2000"));
        assert!(!has(boxed, "STRING_AGG"));
        assert_eq!(mssql_version(Some("16.00.4135"), None), Some("16.00.4135"));
    }

    #[test]
    fn the_driver_says_which_odbc_functions() {
        // CONCAT, LCASE, UCASE, SOUNDEX; DAYOFWEEK, QUARTER, TIMESTAMPADD;
        // CONVERT, CAST.
        let masks: HashMap<u16, u32> = [
            (ffi::SQL_STRING_FUNCTIONS, 0x1 | 0x40 | 0x1000 | 0x2_0000),
            (ffi::SQL_TIMEDATE_FUNCTIONS, 0x8 | 0x40 | 0x2000),
            (ffi::SQL_CONVERT_FUNCTIONS, 0x1 | 0x2),
        ]
        .into();
        let c = generic(&masks);
        let n = names(&c);
        assert!(n.contains("CONCAT") && n.contains("SOUNDEX") && n.contains("QUARTER"));
        // Escape-only: a bare call fails where the engine lacks the name.
        for escape in ["UCASE", "LCASE", "DAYOFWEEK", "TIMESTAMPADD"] {
            assert!(!n.contains(escape), "{escape}");
        }
        // ODBC's CONVERT(value, SQL_type); the standard's CONVERT(… USING …)
        // stays.
        assert!(!c.iter().any(|f| f.args == "(value_exp, data_type)"));
        assert!(!n.contains("LOCATE") && !n.contains("IFNULL") && !n.contains("DATABASE"));
        assert!(ODBC_FUNCTIONS
            .iter()
            .all(|(what, _, _)| FUNCTION_MASKS.contains(what)));
        // The standard's are always there.
        assert!(n.contains("COUNT") && n.contains("ROW_NUMBER") && n.contains("JSON_VALUE"));
        // Nothing from the driver: only the standard's.
        let bare = generic(&HashMap::new());
        assert!(!names(&bare).contains("UCASE"));
        assert_eq!(bare.len(), parse_catalog(shared::ANSI, None).len());
    }

    fn r(cols: &[Option<&str>]) -> Vec<Option<String>> {
        cols.iter().map(|c| c.map(String::from)).collect()
    }

    #[test]
    fn db2_routines_map_to_functions() {
        let rows = vec![
            r(&[
                Some("SYSIBM"),
                Some("COALESCE"),
                Some("COALESCE1"),
                Some("S"),
                Some("C"),
                Some("0"),
                None,
                Some("INTEGER"),
                None,
            ]),
            r(&[
                Some("SYSIBM"),
                Some("COALESCE"),
                Some("COALESCE1"),
                Some("S"),
                Some("P"),
                Some("1"),
                None,
                Some("INTEGER"),
                None,
            ]),
            r(&[
                Some("SYSIBM"),
                Some("COALESCE"),
                Some("COALESCE1"),
                Some("S"),
                Some("P"),
                Some("2"),
                None,
                Some("INTEGER"),
                None,
            ]),
            r(&[
                Some("SYSIBM"),
                Some("+"),
                Some("PLUS1"),
                Some("S"),
                Some("P"),
                Some("1"),
                None,
                Some("INTEGER"),
                None,
            ]),
            r(&[
                Some("SYSIBM"),
                Some("AVG"),
                Some("AVG1"),
                Some("C"),
                Some("C"),
                Some("0"),
                None,
                Some("DOUBLE"),
                None,
            ]),
            r(&[
                Some("SYSIBM"),
                Some("AVG"),
                Some("AVG1"),
                Some("C"),
                Some("P"),
                Some("1"),
                None,
                Some("DOUBLE"),
                None,
            ]),
            r(&[
                Some("SALES"),
                Some("ADD_TAX"),
                Some("SQL250101"),
                Some("S"),
                Some("C"),
                Some("0"),
                None,
                Some("DECIMAL"),
                Some("Price with tax"),
            ]),
            r(&[
                Some("SALES"),
                Some("ADD_TAX"),
                Some("SQL250101"),
                Some("S"),
                Some("P"),
                Some("1"),
                Some("AMOUNT"),
                Some("DECIMAL"),
                Some("Price with tax"),
            ]),
            r(&[
                Some("SALES"),
                Some("TOP_N"),
                Some("SQL250102"),
                Some("T"),
                Some("P"),
                Some("1"),
                Some("N"),
                Some("INTEGER"),
                None,
            ]),
            r(&[
                Some("SALES"),
                Some("TOP_N"),
                Some("SQL250102"),
                Some("T"),
                Some("R"),
                Some("1"),
                Some("ID"),
                Some("INTEGER"),
                None,
            ]),
            r(&[
                Some("SALES"),
                Some("NOW_UTC"),
                Some("SQL250103"),
                Some("S"),
                None,
                None,
                None,
                None,
                None,
            ]),
        ];
        let c = db2_routines(&rows);
        assert_eq!(c.len(), 5, "{c:#?}");
        assert_eq!(c[0].name, "COALESCE");
        assert_eq!(
            (c[0].args.as_str(), c[0].returns.as_str()),
            ("(INTEGER, INTEGER)", "INTEGER")
        );
        assert_eq!(c[0].schema, "");
        assert_eq!(c[1].kind, FunctionKind::Aggregate);
        assert_eq!(c[2].schema, "SALES");
        assert_eq!(c[2].args, "(amount DECIMAL)");
        assert_eq!(c[2].description, "Price with tax");
        assert_eq!(c[3].kind, FunctionKind::Table);
        assert_eq!(
            (c[3].args.as_str(), c[3].returns.as_str()),
            ("(n INTEGER)", "TABLE")
        );
        assert_eq!(c[4].args, "()");
        // What the server didn't list comes from the static catalog:
        // syntax (CAST, XMLELEMENT, JSON_TABLE), window functions, special
        // registers, LUW's own; a user's function of the same name doesn't
        // hide a built-in.
        let mut have = c.clone();
        have.push(FunctionInfo::new("AGE", FunctionKind::Scalar).schema("SALES"));
        let extra = missing_builtins(db2_static(Some("11.05.0900"), true), &have);
        let n = names(&extra);
        for name in [
            "ROW_NUMBER",
            "CURRENT_DATE",
            "CAST",
            "XMLELEMENT",
            "JSON_TABLE",
            "THIS_WEEK",
            "AGE",
        ] {
            assert!(n.contains(name), "{name}");
        }
        assert!(!n.contains("COALESCE"));
        // Db2 for i and z/OS: without LUW's own.
        let iz = names(&db2_static(None, false));
        assert!(iz.contains("LISTAGG") && !iz.contains("THIS_WEEK") && !iz.contains("YMD_BETWEEN"));
    }

    #[test]
    fn shown_functions_take_kinds_from_the_catalog() {
        let rows = vec![
            r(&[Some("!")]),
            r(&[Some("<=>")]),
            r(&[Some("and")]),
            r(&[Some("count")]),
            r(&[Some("row_number")]),
            r(&[Some("explode")]),
            r(&[Some("some_new_function")]),
            r(&[Some("spark_catalog.sales.add_tax")]),
            r(&[Some("default.myudf")]),
        ];
        let c = shown_functions(&rows, SPARK);
        let find = |n: &str| {
            c.iter()
                .find(|f| f.name == n)
                .unwrap_or_else(|| panic!("{n}"))
        };
        assert_eq!(find("count").kind, FunctionKind::Aggregate);
        assert_eq!(find("row_number").kind, FunctionKind::Window);
        // A generator goes in the SELECT list, where expressions go.
        assert_eq!(find("explode").kind, FunctionKind::Scalar);
        assert_eq!(find("some_new_function").kind, FunctionKind::Scalar);
        assert_eq!(find("some_new_function").args, UNKNOWN_ARGS);
        assert_eq!(find("myudf").args, UNKNOWN_ARGS);
        assert_eq!(find("add_tax").schema, "spark_catalog.sales");
        assert_eq!(find("myudf").schema, "default");
        assert!(!c
            .iter()
            .any(|f| f.name == "and" || f.name == "!" || f.name == "<=>"));
        // Hive's names.
        let h = shown_functions(
            &vec![r(&[Some("collect_set")]), r(&[Some("percentile_approx")])],
            HIVE,
        );
        assert!(
            h.iter().all(|f| f.kind == FunctionKind::Aggregate),
            "{h:#?}"
        );
    }

    #[test]
    fn impala_signatures_split() {
        let rows = vec![
            r(&[
                Some("BIGINT"),
                Some("abs(BIGINT)"),
                Some("BUILTIN"),
                Some("true"),
            ]),
            r(&[
                Some("STRING"),
                Some("concat(STRING...)"),
                Some("BUILTIN"),
                Some("true"),
            ]),
            r(&[
                Some("BOOLEAN"),
                Some("_internal(INT)"),
                Some("BUILTIN"),
                Some("true"),
            ]),
        ];
        let c = impala_functions(&rows, FunctionKind::Scalar, "");
        assert_eq!(c.len(), 2);
        assert_eq!(
            (
                c[0].name.as_str(),
                c[0].args.as_str(),
                c[0].returns.as_str()
            ),
            ("abs", "(BIGINT)", "BIGINT")
        );
        assert_eq!(c[1].args, "(STRING...)");
        let u = impala_functions(
            &vec![r(&[Some("DOUBLE"), Some("my_avg(DOUBLE)"), None, None])],
            FunctionKind::Aggregate,
            "sales",
        );
        assert_eq!(
            (u[0].kind, u[0].schema.as_str()),
            (FunctionKind::Aggregate, "sales")
        );
    }

    #[test]
    fn monetdb_rows_map_to_functions() {
        let rows = vec![
            r(&[
                Some("sys"),
                Some("abs"),
                Some("10"),
                Some("1"),
                Some("0"),
                Some("0"),
                Some("res_0"),
                Some("int"),
            ]),
            r(&[
                Some("sys"),
                Some("abs"),
                Some("10"),
                Some("1"),
                Some("1"),
                Some("1"),
                Some("arg_1"),
                Some("int"),
            ]),
            r(&[
                Some("sys"),
                Some("sum"),
                Some("20"),
                Some("3"),
                Some("0"),
                Some("0"),
                Some("res_0"),
                Some("bigint"),
            ]),
            r(&[
                Some("sys"),
                Some("sum"),
                Some("20"),
                Some("3"),
                Some("1"),
                Some("1"),
                Some("arg_1"),
                Some("int"),
            ]),
            r(&[
                Some("sys"),
                Some("sql_add"),
                Some("21"),
                Some("1"),
                None,
                None,
                None,
                None,
            ]),
            r(&[
                Some("sys"),
                Some("="),
                Some("22"),
                Some("1"),
                None,
                None,
                None,
                None,
            ]),
            r(&[
                Some("sales"),
                Some("top_n"),
                Some("30"),
                Some("5"),
                Some("1"),
                Some("1"),
                Some("n"),
                Some("int"),
            ]),
            r(&[
                Some("sys"),
                Some("rank"),
                Some("40"),
                Some("6"),
                Some("0"),
                Some("0"),
                Some("res_0"),
                Some("int"),
            ]),
            r(&[
                Some("sys"),
                Some("zero_or_one"),
                Some("41"),
                Some("3"),
                None,
                None,
                None,
                None,
            ]),
            r(&[
                Some("profiler"),
                Some("getlimit"),
                Some("50"),
                Some("1"),
                Some("0"),
                Some("0"),
                Some("res_0"),
                Some("int"),
            ]),
        ];
        let c = monetdb_functions(&rows);
        assert_eq!(c.len(), 5, "{c:#?}");
        assert_eq!(
            (c[0].args.as_str(), c[0].returns.as_str()),
            ("(int)", "int")
        );
        assert_eq!(c[0].schema, "");
        assert_eq!(c[1].kind, FunctionKind::Aggregate);
        assert_eq!(c[2].kind, FunctionKind::Table);
        assert_eq!(
            (c[2].schema.as_str(), c[2].args.as_str()),
            ("sales", "(n int)")
        );
        assert_eq!(c[3].kind, FunctionKind::Window);
        // MonetDB finds the profiler's functions only qualified.
        assert_eq!(
            (c[4].name.as_str(), c[4].schema.as_str(), c[4].args.as_str()),
            ("getlimit", "profiler", "()")
        );
        // Internal helpers are left out.
        assert!(!c
            .iter()
            .any(|f| f.name == "sql_add" || f.name == "zero_or_one"));
    }

    #[test]
    fn user_functions_come_with_their_arguments() {
        // SQLProcedures: catalog, schema, name, 3 reserved, remarks, type.
        let procs = vec![
            r(&[
                None,
                Some("SALES"),
                Some("ADD_TAX"),
                None,
                None,
                None,
                Some("Price with tax"),
                Some("2"),
            ]),
            r(&[
                None,
                Some("SALES"),
                Some("CLEANUP"),
                None,
                None,
                None,
                None,
                Some("1"),
            ]),
            r(&[
                None,
                Some("SYSIBM"),
                Some("SQLFN"),
                None,
                None,
                None,
                None,
                Some("2"),
            ]),
            r(&[
                None,
                Some("SALES"),
                Some("TOP_N"),
                None,
                None,
                None,
                None,
                Some("2"),
            ]),
            r(&[
                None,
                Some("SALES"),
                Some("NOW_UTC"),
                None,
                None,
                None,
                None,
                Some("2"),
            ]),
            r(&[
                None,
                Some("SALES"),
                Some("PRICE"),
                None,
                None,
                None,
                None,
                Some("2"),
            ]),
        ];
        let found = listed_functions(&procs, |s| s.starts_with("SYS"));
        let found_names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(found_names, ["ADD_TAX", "TOP_N", "NOW_UTC", "PRICE"]);
        // SQLProcedureColumns: catalog, schema, name, column, column type,
        // …, type name (6), …, ordinal position (17).
        let column = |name: &str, column: Option<&str>, role: &str, ty: &str, pos: &str| {
            let mut row = vec![None; 19];
            row[1] = Some("SALES".to_string());
            row[2] = Some(name.to_string());
            row[3] = column.map(String::from);
            row[4] = Some(role.to_string());
            row[6] = Some(ty.to_string());
            row[17] = Some(pos.to_string());
            row
        };
        let columns = vec![
            column("ADD_TAX", Some("RATE"), "1", "DECIMAL", "2"),
            column("ADD_TAX", Some("AMOUNT"), "1", "DECIMAL", "1"),
            column("ADD_TAX", None, "5", "DECIMAL", "0"),
            column("TOP_N", Some("N"), "1", "INTEGER", "1"),
            column("TOP_N", Some("ID"), "3", "INTEGER", "1"),
            column("NOW_UTC", None, "5", "TIMESTAMP", "0"),
            // Two overloads under one name.
            column("PRICE", Some("ID"), "1", "INTEGER", "1"),
            column("PRICE", None, "5", "DECIMAL", "0"),
            column("PRICE", Some("CODE"), "1", "VARCHAR", "1"),
            column("PRICE", None, "5", "DECIMAL", "0"),
        ];
        let c = with_arguments(found.clone(), Some(&columns));
        assert_eq!(c[0].args, "(amount DECIMAL, rate DECIMAL)");
        assert_eq!(
            (
                c[0].returns.as_str(),
                c[0].schema.as_str(),
                c[0].description.as_str()
            ),
            ("DECIMAL", "SALES", "Price with tax")
        );
        assert_eq!(
            (c[1].kind, c[1].args.as_str(), c[1].returns.as_str()),
            (FunctionKind::Table, "(n INTEGER)", "TABLE")
        );
        assert_eq!(
            (c[2].args.as_str(), c[2].returns.as_str()),
            ("()", "TIMESTAMP")
        );
        assert_eq!(c[3].args, UNKNOWN_ARGS);
        // Columns not read: the arguments are unknown, not empty.
        let c = with_arguments(found, None);
        assert!(c.iter().all(|f| f.args == UNKNOWN_ARGS));
        // SQL Server's and ASE's catalogs say the kind.
        let mssql = catalog_functions(&vec![
            r(&[Some("dbo"), Some("f_scalar"), Some("FN")]),
            r(&[Some("dbo"), Some("f_inline"), Some("IF")]),
            r(&[Some("dbo"), Some("f_clr_agg"), Some("AF")]),
        ]);
        let kinds: Vec<FunctionKind> = mssql.iter().map(|f| f.kind).collect();
        assert_eq!(
            kinds,
            [
                FunctionKind::Scalar,
                FunctionKind::Table,
                FunctionKind::Aggregate
            ]
        );
    }

    #[test]
    fn the_server_version_gates_static_catalogs() {
        let has = |text, v, n: &str| {
            parse_catalog(text, v)
                .iter()
                .any(|f| f.name.eq_ignore_ascii_case(n))
        };
        assert!(has(DB2, Some("11.05.0900"), "LISTAGG"));
        assert!(!has(DB2, Some("09.05.0000"), "LISTAGG"));
        assert!(has(TERADATA, Some("17.20.03.19"), "TRYCAST"));
        assert!(!has(TERADATA, Some("16.10.00.03"), "TRYCAST"));
    }

    #[test]
    fn kinds_follow_where_the_function_goes() {
        let kind = |text, n: &str| {
            parse_catalog(text, None)
                .into_iter()
                .find(|f| f.name.eq_ignore_ascii_case(n))
                .unwrap_or_else(|| panic!("{n}"))
                .kind
        };
        for n in [
            "explode",
            "inline",
            "json_tuple",
            "parse_url_tuple",
            "posexplode",
            "stack",
        ] {
            assert_eq!(kind(HIVE, n), FunctionKind::Scalar, "hive {n}");
        }
        for n in ["explode_outer", "inline_outer", "posexplode_outer", "stack"] {
            assert_eq!(kind(SPARK, n), FunctionKind::Scalar, "spark {n}");
        }
        assert_eq!(kind(SPARK, "range"), FunctionKind::Table);
        for n in ["EXPLODE", "MAPKEYS", "MAPITEMS"] {
            assert_eq!(kind(VERTICA, n), FunctionKind::Window, "vertica {n}");
        }
        assert_eq!(kind(TERADATA, "REGEXP_SPLIT_TO_TABLE"), FunctionKind::Table);
        // JSON type methods (`col.JSONExtractValue(…)`) are not functions.
        assert!(!parse_catalog(TERADATA, None)
            .iter()
            .any(|f| f.name.starts_with("JSONExtract")));
    }

    #[test]
    fn dameng_gets_the_oracle_functions_it_documents() {
        let dm = names(&dameng());
        for n in [
            "LISTAGG",
            "NVL",
            "TO_CHAR",
            "ROW_NUMBER",
            "ADD_MONTHS",
            "DECODE",
        ] {
            assert!(dm.contains(n), "{n}");
        }
        for n in [
            "VECTOR_DISTANCE",
            "PREDICTION",
            "CLUSTER_ID",
            "FEATURE_SET",
            "CON_NAME_TO_ID",
            "ORA_INVOKING_USER",
            "ORA_DST_CONVERT",
            "CUBE_TABLE",
            "CV",
            "PRESENTV",
            "PRESENTNNV",
        ] {
            assert!(!dm.contains(n), "{n}");
        }
        // Every name is one the Oracle catalog has (no typo drops one).
        let oracle = names(&parse_catalog(shared::ORACLE, Some(DAMENG_AS_ORACLE)));
        for n in DAMENG_FROM_ORACLE.split_whitespace() {
            assert!(oracle.contains(n), "{n}");
        }
    }
}
