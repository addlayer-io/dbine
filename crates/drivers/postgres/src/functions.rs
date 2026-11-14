//! The function catalog (issue #97) of each variant.
//!
//! - PostgreSQL's catalog (most variants, CockroachDB and YugabyteDB too):
//!   `pg_proc` with its comments, so the version, the extensions and the
//!   user's functions count; PostgreSQL's static catalog if that fails
//!   ([`pg_functions`]).
//! - Materialize: `mz_catalog.mz_functions`.
//! - CrateDB: its `pg_proc`, which lists the built-ins, with the type names
//!   from `pg_type`.
//! - Materialize and CrateDB fall back to the SQL standard's functions if
//!   their catalog can't be read.
//! - RisingWave, H2 and Redshift: a static catalog of the built-ins for
//!   the server's version (their `pg_proc` is empty, missing or PostgreSQL
//!   8.0's) plus the user's functions from `rw_catalog.rw_functions`,
//!   `INFORMATION_SCHEMA.ROUTINES` and `pg_proc_info`.
//! - Denodo: a static catalog of VQL's functions.

use crate::catalog::user_schema;
use crate::pg_functions;
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::functions::parse_catalog;
use dbine_driver::{FunctionInfo, FunctionKind, Result};
use std::collections::HashMap;
use tokio_postgres::SimpleQueryRow;

const REDSHIFT: &str = include_str!("functions/redshift.txt");
const RISINGWAVE: &str = include_str!("functions/risingwave.txt");
const H2: &str = include_str!("functions/h2.txt");
const DENODO: &str = include_str!("functions/denodo.txt");

/// Window functions the catalogs that don't say so (Materialize's) list as
/// plain functions.
const WINDOW: &[&str] = &[
    "row_number",
    "rank",
    "dense_rank",
    "percent_rank",
    "cume_dist",
    "ntile",
    "lag",
    "lead",
    "first_value",
    "last_value",
    "nth_value",
];

fn cell(r: &SimpleQueryRow, i: usize) -> &str {
    r.get(i).unwrap_or_default()
}

pub(crate) async fn catalog(s: &PgSession) -> Result<Vec<FunctionInfo>> {
    match s.variant {
        Variant::Materialize => materialize(s).await,
        Variant::CrateDb => cratedb(s).await,
        Variant::RisingWave => {
            // Its PostgreSQL banner carries its own version after `RisingWave-`.
            let version = risingwave_version(s).await;
            Ok(
                with_user_functions(s, RISINGWAVE, version.as_deref(), &[RW_FUNCTIONS], rw_entry)
                    .await,
            )
        }
        Variant::H2 => {
            // Its PostgreSQL banner names PostgreSQL's version, not H2's.
            let version = match s.text("SELECT H2VERSION()").await {
                Ok(rows) => rows.first().map(|r| cell(r, 0).to_string()),
                Err(e) => {
                    tracing::debug!("h2: no version: {e}");
                    None
                }
            };
            Ok(with_user_functions(s, H2, version.as_deref(), &[H2_ROUTINES], h2_entry).await)
        }
        Variant::Redshift => {
            let [info, plain] = redshift_functions(s.variant);
            Ok(with_user_functions(s, REDSHIFT, None, &[&info, &plain], redshift_entry).await)
        }
        Variant::Denodo => Ok(parse_catalog(DENODO, None)),
        _ => Ok(pg(s).await),
    }
}

/// What a variant whose own catalog can't be read gets: the SQL standard's
/// functions. Not an error: the editor doesn't keep one, so it would run
/// the failing read again and again and never complete a function.
fn standard_fallback(s: &PgSession, what: &str, e: dbine_driver::Error) -> Vec<FunctionInfo> {
    tracing::debug!("{:?}: {what} unavailable: {e}", s.variant);
    parse_catalog(dbine_driver::functions::shared::ANSI, None)
}

/// `pg_proc` (the forms the version may have, in order), or the static
/// catalog for the version when none reads or none lists a function.
async fn pg(s: &PgSession) -> Vec<FunctionInfo> {
    for sql in pg_functions::pg_proc_queries(s.version) {
        match s.text(&sql).await {
            Ok(rows) => {
                let out: Vec<FunctionInfo> = rows
                    .iter()
                    .filter_map(|r| {
                        pg_functions::pg_entry(
                            cell(r, 0),
                            cell(r, 1),
                            cell(r, 2),
                            cell(r, 3),
                            cell(r, 4),
                            cell(r, 5),
                            cell(r, 6) == "t",
                        )
                    })
                    .collect();
                if !out.is_empty() {
                    return out;
                }
                tracing::debug!("{:?}: pg_proc listed no functions", s.variant);
            }
            Err(e) => tracing::debug!("{:?}: pg_proc unavailable: {e}", s.variant),
        }
    }
    pg_functions::fallback(s.version)
}

/// A variant's static catalog for its `version` plus the user's functions
/// from the first of `queries` that runs, mapped by `entry`; the static
/// catalog alone if none does.
async fn with_user_functions(
    s: &PgSession,
    builtins: &str,
    version: Option<&str>,
    queries: &[&str],
    entry: fn(&SimpleQueryRow) -> Option<FunctionInfo>,
) -> Vec<FunctionInfo> {
    let mut out = parse_catalog(builtins, version);
    for sql in queries {
        match s.text(sql).await {
            Ok(rows) => {
                out.extend(rows.iter().filter_map(entry));
                break;
            }
            Err(e) => tracing::debug!("{:?}: user functions unavailable: {e}", s.variant),
        }
    }
    out
}

/// RisingWave's version: what follows `RisingWave-` in its `version()`
/// (`PostgreSQL 13.14.0-RisingWave-3.1.0 (…)`).
async fn risingwave_version(s: &PgSession) -> Option<String> {
    match s.text("SELECT version()").await {
        Ok(rows) => rows.first().and_then(|r| risingwave_release(cell(r, 0))),
        Err(e) => {
            tracing::debug!("risingwave: no version: {e}");
            None
        }
    }
}

fn risingwave_release(banner: &str) -> Option<String> {
    let rest = banner.split_once("RisingWave-")?.1;
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.'))
        .unwrap_or(rest.len());
    Some(rest[..end].to_string()).filter(|v| !v.is_empty())
}

/// RisingWave's user-defined functions: schema, name, kind, argument and
/// result types.
const RW_FUNCTIONS: &str = "SELECT n.nspname, f.name, f.type,
        (SELECT string_agg(format_type(u.t, NULL), ', ' ORDER BY u.i)
         FROM unnest(f.arg_type_ids) WITH ORDINALITY AS u(t, i)),
        format_type(f.return_type_id, NULL)
 FROM rw_catalog.rw_functions f JOIN pg_catalog.pg_namespace n ON n.oid = f.schema_id";

fn rw_entry(r: &SimpleQueryRow) -> Option<FunctionInfo> {
    let kind = match cell(r, 2) {
        "AGGREGATE" => FunctionKind::Aggregate,
        "TABLE" => FunctionKind::Table,
        _ => FunctionKind::Scalar,
    };
    user_entry(cell(r, 0), cell(r, 1), kind, cell(r, 3), cell(r, 4), "")
}

/// H2's user routines (`CREATE ALIAS`, `CREATE AGGREGATE`): schema, name,
/// type, result, parameters, remarks.
const H2_ROUTINES: &str = "SELECT r.ROUTINE_SCHEMA, r.ROUTINE_NAME, r.ROUTINE_TYPE, r.DATA_TYPE,
        (SELECT LISTAGG(COALESCE(p.PARAMETER_NAME || ' ', '') || p.DATA_TYPE, ', ')
                  WITHIN GROUP (ORDER BY p.ORDINAL_POSITION)
         FROM INFORMATION_SCHEMA.PARAMETERS p
         WHERE p.SPECIFIC_SCHEMA = r.SPECIFIC_SCHEMA AND p.SPECIFIC_NAME = r.SPECIFIC_NAME),
        r.REMARKS
 FROM INFORMATION_SCHEMA.ROUTINES r
 WHERE r.ROUTINE_TYPE <> 'PROCEDURE'";

fn h2_entry(r: &SimpleQueryRow) -> Option<FunctionInfo> {
    let kind = if cell(r, 2) == "AGGREGATE" {
        FunctionKind::Aggregate
    } else {
        FunctionKind::Scalar
    };
    user_entry(
        cell(r, 0),
        cell(r, 1),
        kind,
        cell(r, 4),
        cell(r, 3),
        cell(r, 5),
    )
}

/// Redshift's user-defined functions (SQL, Python, Lambda): from
/// `pg_proc_info`, whose `prokind` tells them from the stored procedures
/// (`pg_proc` keeps both); from `pg_proc` if that view can't be read.
fn redshift_functions(v: Variant) -> [String; 2] {
    let query = |from: &str, kind: &str| {
        format!(
            "SELECT n.nspname, p.proname, CASE WHEN p.proretset THEN 't' ELSE 's' END,
                    oidvectortypes(p.proargtypes), format_type(p.prorettype, NULL)
             FROM {from} p JOIN pg_namespace n ON n.oid = p.pronamespace
             WHERE {kind}{}",
            user_schema(v, "n.nspname")
        )
    };
    [
        query("pg_proc_info", "p.prokind = 'f' AND "),
        query("pg_proc", ""),
    ]
}

fn redshift_entry(r: &SimpleQueryRow) -> Option<FunctionInfo> {
    let kind = if cell(r, 2) == "t" {
        FunctionKind::Table
    } else {
        FunctionKind::Scalar
    };
    user_entry(cell(r, 0), cell(r, 1), kind, cell(r, 3), cell(r, 4), "")
}

/// A user's function: always with its schema.
fn user_entry(
    schema: &str,
    name: &str,
    kind: FunctionKind,
    args: &str,
    returns: &str,
    description: &str,
) -> Option<FunctionInfo> {
    if name.is_empty() {
        return None;
    }
    Some(
        FunctionInfo::new(name, kind)
            .args(format!("({args})"))
            .returns(returns)
            .description(description.trim())
            .schema(schema),
    )
}

/// Materialize's functions: schema, name, kind (`a` when `pg_aggregate`
/// has it, `t` for set-returning), argument types, the variadic type, and
/// the result. Its internal and unsafe schemas are left out.
const MZ_FUNCTIONS: &str = "SELECT s.name, f.name,
        CASE WHEN f.oid IN (SELECT a.aggfnoid::oid FROM pg_catalog.pg_aggregate a) THEN 'a'
             WHEN f.returns_set THEN 't' ELSE 's' END,
        (SELECT string_agg(t.name, ', ' ORDER BY x.ord)
         FROM unnest(f.argument_type_ids) WITH ORDINALITY AS x(id, ord)
         JOIN mz_catalog.mz_types t ON t.id = x.id),
        vt.name, rt.name
 FROM mz_catalog.mz_functions f
 JOIN mz_catalog.mz_schemas s ON s.id = f.schema_id
 LEFT JOIN mz_catalog.mz_types rt ON rt.id = f.return_type_id
 LEFT JOIN mz_catalog.mz_types vt ON vt.id = f.variadic_argument_type_id
 WHERE s.name NOT IN ('mz_internal', 'mz_unsafe', 'information_schema')";

async fn materialize(s: &PgSession) -> Result<Vec<FunctionInfo>> {
    let rows = match s.text(MZ_FUNCTIONS).await {
        Ok(rows) => rows,
        Err(e) => return Ok(standard_fallback(s, "mz_functions", e)),
    };
    Ok(rows
        .iter()
        .filter_map(|r| {
            mz_entry(
                cell(r, 0),
                cell(r, 1),
                cell(r, 2),
                cell(r, 3),
                cell(r, 4),
                cell(r, 5),
            )
        })
        .collect())
}

/// A row of [`MZ_FUNCTIONS`]. The variadic argument is the last one.
fn mz_entry(
    schema: &str,
    name: &str,
    kind: &str,
    args: &str,
    variadic: &str,
    returns: &str,
) -> Option<FunctionInfo> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let kind = match kind {
        "a" => FunctionKind::Aggregate,
        "t" => FunctionKind::Table,
        _ if WINDOW.contains(&name) => FunctionKind::Window,
        _ => FunctionKind::Scalar,
    };
    let mut args: Vec<String> = args
        .split(", ")
        .filter(|a| !a.is_empty())
        .map(str::to_string)
        .collect();
    if !variadic.is_empty() {
        match args.last_mut() {
            Some(last) => *last = format!("VARIADIC {last}"),
            None => args.push(format!("VARIADIC {variadic}")),
        }
    }
    let f = FunctionInfo::new(name, kind)
        .args(format!("({})", args.join(", ")))
        .returns(returns);
    Some(if matches!(schema, "pg_catalog" | "mz_catalog") {
        f
    } else {
        f.schema(schema)
    })
}

/// CrateDB's set-returning functions (its `proretset` marks the ones that
/// return arrays instead).
const CRATE_TABLE: &[&str] = &[
    "unnest",
    "generate_series",
    "generate_subscripts",
    "regexp_matches",
    "pg_get_keywords",
    "empty_row",
];

/// CrateDB's own names for operators and syntax (`op_and`, `subscript`,
/// `_cast`…), which `pg_proc` lists next to the functions.
const CRATE_INTERNAL: &[&str] = &[
    "add",
    "and",
    "case",
    "cast",
    "divide",
    "modulus",
    "multiply",
    "or",
    "subscript",
    "subscript_obj",
    "subtract",
    "xor",
];

/// CrateDB's functions: `pg_proc` with the arguments as type oids, which
/// `pg_type` names (CrateDB has no `pg_get_function_arguments`). Without
/// the names the arguments show as `any`.
async fn cratedb(s: &PgSession) -> Result<Vec<FunctionInfo>> {
    let types: HashMap<String, String> = match s
        .text("SELECT oid, format_type(oid, NULL) FROM pg_catalog.pg_type")
        .await
    {
        Ok(rows) => rows
            .iter()
            .map(|r| (cell(r, 0).to_string(), cell(r, 1).to_string()))
            .collect(),
        Err(e) => {
            tracing::debug!("cratedb: type names unavailable: {e}");
            HashMap::new()
        }
    };
    let rows = match s
        .text(
            "SELECT n.nspname, p.proname, p.prokind, p.proargtypes, format_type(p.prorettype, NULL), p.provariadic
             FROM pg_catalog.pg_proc p LEFT JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace",
        )
        .await
    {
        Ok(rows) => rows,
        Err(e) => return Ok(standard_fallback(s, "pg_proc", e)),
    };
    Ok(rows
        .iter()
        .filter_map(|r| {
            crate_entry(
                cell(r, 0),
                cell(r, 1),
                cell(r, 2),
                cell(r, 3),
                cell(r, 4),
                cell(r, 5),
                &types,
            )
        })
        .collect())
}

/// A row of CrateDB's `pg_proc`: `argtypes` as the server prints its
/// `oidvector` (`23 1043`; `[23, 1043]` in its own console), `variadic` the oid of the
/// variadic argument's type (`0` if none).
fn crate_entry(
    schema: &str,
    name: &str,
    prokind: &str,
    argtypes: &str,
    returns: &str,
    variadic: &str,
    types: &HashMap<String, String>,
) -> Option<FunctionInfo> {
    let ident = name.starts_with(|c: char| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !ident
        || schema == "information_schema"
        || name.starts_with("op_")
        || name.starts_with("any_")
        || name.ends_with("recv")
        || name.ends_with("send")
        || CRATE_INTERNAL.contains(&name)
    {
        return None;
    }
    let kind = match prokind {
        "a" => FunctionKind::Aggregate,
        "w" => FunctionKind::Window,
        _ if CRATE_TABLE.contains(&name) => FunctionKind::Table,
        _ => FunctionKind::Scalar,
    };
    let type_name = |oid: &str| match types.get(oid) {
        Some(t) if !t.is_empty() && t != "undefined" => t.clone(),
        _ => "any".to_string(),
    };
    let mut args: Vec<String> = argtypes
        .trim_matches(|c| matches!(c, '[' | ']' | '{' | '}'))
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|o| !o.is_empty())
        .map(type_name)
        .collect();
    if !matches!(variadic, "" | "0") {
        if let Some(last) = args.last_mut() {
            *last = format!("VARIADIC {last}");
        }
    }
    // CrateDB names its "any" type `undefined`.
    let returns = returns.replace("undefined", "any");
    let f = FunctionInfo::new(name, kind)
        .args(format!("({})", args.join(", ")))
        .returns(returns);
    // Built-ins have no namespace (or pg_catalog's); a user's function
    // lives in its schema (`doc`).
    Some(if matches!(schema, "" | "pg_catalog") {
        f
    } else {
        f.schema(schema)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_variant_catalogs_parse_line_by_line() {
        for (engine, text, min) in [
            ("redshift", REDSHIFT, 200),
            ("risingwave", RISINGWAVE, 200),
            ("h2", H2, 200),
            ("denodo", DENODO, 100),
        ] {
            let mut entries = 0;
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
                entries += 1;
            }
            assert!(entries >= min, "{engine}: {entries}");
        }
    }

    #[test]
    fn materialize_rows_map_to_entries() {
        let f = mz_entry("pg_catalog", "string_agg", "a", "text, text", "", "text").unwrap();
        assert_eq!(
            (
                f.kind,
                f.args.as_str(),
                f.returns.as_str(),
                f.schema.as_str()
            ),
            (FunctionKind::Aggregate, "(text, text)", "text", "")
        );
        let concat = mz_entry("pg_catalog", "concat", "s", "any", "any", "text").unwrap();
        assert_eq!(concat.args, "(VARIADIC any)");
        assert_eq!(
            mz_entry("pg_catalog", "lag", "s", "anyelement", "", "anyelement")
                .unwrap()
                .kind,
            FunctionKind::Window
        );
        let series = mz_entry("mz_catalog", "unnest", "t", "list", "", "any").unwrap();
        assert_eq!(
            (series.kind, series.schema.as_str()),
            (FunctionKind::Table, "")
        );
        assert_eq!(
            mz_entry("mz_catalog", "mz_now", "s", "", "", "mz_timestamp")
                .unwrap()
                .args,
            "()"
        );
        assert!(mz_entry("pg_catalog", "+", "s", "", "", "").is_none());
    }

    #[test]
    fn cratedb_rows_map_to_entries() {
        let types: HashMap<String, String> = [
            ("23", "integer"),
            ("1043", "text"),
            ("1184", "timestamp with time zone"),
            ("2276", "undefined"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let f = crate_entry(
            "",
            "date_trunc",
            "f",
            "[1043, 1043, 1184]",
            "timestamp with time zone",
            "0",
            &types,
        )
        .unwrap();
        assert_eq!(f.kind, FunctionKind::Scalar);
        assert_eq!(f.args, "(text, text, timestamp with time zone)");
        assert_eq!(f.schema, "");
        let concat = crate_entry("", "concat", "f", "1043", "text", "1043", &types).unwrap();
        let two = crate_entry("", "concat", "f", "1043 23", "text", "0", &types).unwrap();
        assert_eq!(two.args, "(text, integer)");
        assert_eq!(concat.args, "(VARIADIC text)");
        let count = crate_entry("", "count", "a", "[2276]", "bigint", "0", &types).unwrap();
        assert_eq!(
            (count.kind, count.args.as_str()),
            (FunctionKind::Aggregate, "(any)")
        );
        assert_eq!(
            crate_entry("", "row_number", "w", "[]", "integer", "0", &types)
                .unwrap()
                .kind,
            FunctionKind::Window
        );
        assert_eq!(
            crate_entry(
                "",
                "generate_series",
                "f",
                "[23, 23]",
                "integer",
                "0",
                &types
            )
            .unwrap()
            .kind,
            FunctionKind::Table
        );
        let array =
            crate_entry("", "array_agg", "a", "[2276]", "undefined[]", "0", &types).unwrap();
        assert_eq!(array.returns, "any[]");
        let mine = crate_entry("doc", "my_fn", "f", "[23]", "integer", "0", &types).unwrap();
        assert_eq!(mine.schema, "doc");
        for name in [
            "op_and",
            "any_=",
            "_cast",
            "int4recv",
            "textsend",
            "subscript",
            "extract_YEAR",
            "add",
        ] {
            assert!(
                crate_entry("", name, "f", "[]", "integer", "0", &types).is_none(),
                "{name}"
            );
        }
        assert!(crate_entry(
            "information_schema",
            "_pg_expandarray",
            "f",
            "[]",
            "",
            "0",
            &types
        )
        .is_none());
    }

    #[test]
    fn risingwave_reports_its_version_in_the_banner() {
        assert_eq!(
            risingwave_release(
                "PostgreSQL 13.14.0-RisingWave-2.6.1 (4c1a4bc6f2d33cab1b4bc2ad5a1b0bd5a3e8b4a1)"
            )
            .as_deref(),
            Some("2.6.1")
        );
        assert_eq!(
            risingwave_release("PostgreSQL 13.14.0-RisingWave-1.10.0-alpha (x)").as_deref(),
            Some("1.10.0")
        );
        assert_eq!(risingwave_release("PostgreSQL 16.2"), None);
        // The static catalog follows it.
        let names = |v| {
            parse_catalog(RISINGWAVE, v)
                .into_iter()
                .map(|f| f.name)
                .collect::<Vec<_>>()
        };
        assert!(names(Some("1.9.0")).iter().all(|n| n != "map_keys"));
        assert!(names(Some("2.6.1")).iter().any(|n| n == "map_keys"));
        assert!(names(Some("1.9.0")).iter().any(|n| n == "tumble"));
    }

    #[test]
    fn redshift_reads_functions_without_the_procedures() {
        let [info, plain] = redshift_functions(Variant::Redshift);
        assert!(info.contains("FROM pg_proc_info p") && info.contains("p.prokind = 'f' AND"));
        assert!(plain.contains("FROM pg_proc p") && !plain.contains("prokind"));
    }

    #[test]
    fn user_entries_carry_their_schema() {
        let f = user_entry(
            "public",
            "add_one",
            FunctionKind::Scalar,
            "integer",
            "integer",
            " adds ",
        )
        .unwrap();
        assert_eq!(
            (f.args.as_str(), f.schema.as_str(), f.description.as_str()),
            ("(integer)", "public", "adds")
        );
    }
}
