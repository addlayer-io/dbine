//! The functions the editor completes ([`dbine_driver::Session::function_catalog`],
//! issue #97).
//!
//! SQL Server keeps no catalog of its built-ins: they come from the shared
//! T-SQL catalog ([`shared::MSSQL`]), filtered by the boxed release
//! (`SERVERPROPERTY('ProductVersion')`). The cloud engines report 12.0
//! whatever they run, so they go by their `EngineEdition` instead:
//!
//! - Azure SQL Database and SQL database in Fabric run the newest engine:
//!   all of it.
//! - Managed Instance runs the newest engine on the always-up-to-date
//!   update policy, and its SQL Server release's on that release's policy
//!   (`ProductUpdateType` `CU`).
//! - Synapse's dedicated and serverless pools have SQL Server 2019's,
//!   plus the few newer ones they document (`GREATEST`, `PRODUCT`).
//! - Fabric Data Warehouse has the newest ones but the vector, AI and a
//!   few other functions, plus its own `MEDIAN`.
//!
//! Babelfish reports SQL Server's 12.0 too but implements its own subset:
//! the names it has come from its `sys` schema (`sys.all_objects`), with
//! the shared catalog's arguments.
//!
//! The database's own functions (scalar, inline and multi-statement
//! table-valued, CLR scalar, table and aggregate) come from `sys.objects`
//! and `sys.all_parameters` (Babelfish has no `sys.parameters`) in one
//! query, with their schema, their result type and their `MS_Description`.

use crate::{format_type, text, SqlServerSession, Variant};
use dbine_driver::functions::{parse_catalog, shared, version_numbers, FunctionInfo, FunctionKind};
use dbine_driver::Result;
use std::collections::HashSet;

/// The release, the edition and (Managed Instance only) the update policy.
const ENGINE_SQL: &str = "SELECT CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)),
       CAST(SERVERPROPERTY('EngineEdition') AS int),
       CAST(SERVERPROPERTY('ProductUpdateType') AS nvarchar(128))";

/// The database's functions, one row per parameter (`parameter_id` 0 is
/// the result of a scalar function), in order.
const USER_SQL: &str = "SELECT s.name, o.name, RTRIM(o.type), CAST(p.parameter_id AS int), p.name,
       TYPE_NAME(p.user_type_id), CAST(p.max_length AS int), CAST(p.precision AS int),
       CAST(p.scale AS int), CAST(ep.value AS nvarchar(4000))
  FROM sys.objects o
  JOIN sys.schemas s ON s.schema_id = o.schema_id
  LEFT JOIN sys.all_parameters p ON p.object_id = o.object_id
  LEFT JOIN sys.extended_properties ep
    ON ep.class = 1 AND ep.major_id = o.object_id AND ep.minor_id = 0
   AND ep.name = N'MS_Description'
 WHERE o.type IN ('FN', 'FS', 'IF', 'TF', 'FT', 'AF') AND o.is_ms_shipped = 0
 ORDER BY s.name, o.name, p.parameter_id";

/// Babelfish's functions: its T-SQL ones (schema `sys`) and PostgreSQL's
/// that T-SQL reaches by the same name (`pg_catalog`: `ACOS`,
/// `ROW_NUMBER`), from PostgreSQL's catalog, which T-SQL can read there.
const BABELFISH_SQL: &str = "SELECT DISTINCT n.nspname, UPPER(p.proname)
  FROM pg_catalog.pg_proc p
  JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
 WHERE n.nspname IN ('sys', 'pg_catalog')";

pub(crate) async fn catalog(s: &mut SqlServerSession) -> Result<Vec<FunctionInfo>> {
    let mut out = match s.variant {
        Variant::Babelfish => babelfish_builtins(s).await,
        Variant::Fabric => fabric_warehouse_builtins(),
        _ => {
            let engine = s.rows(ENGINE_SQL, &[]).await.unwrap_or_default();
            let row = engine.first();
            let version = row.and_then(|r| text(r, 0));
            let edition = row.and_then(|r| r.get::<i32, _>(1)).unwrap_or(0);
            let update = row.and_then(|r| text(r, 2));
            edition_builtins(version.as_deref(), edition, update.as_deref())
        }
    };
    match s.rows(USER_SQL, &[]).await {
        Ok(rows) => out.extend(user_functions(rows.iter().map(user_row))),
        Err(e) => tracing::debug!("function catalog: user functions: {e}"),
    }
    Ok(out)
}

/// The built-ins of the engine `EngineEdition` names: 5 Azure SQL
/// Database, 6 Synapse dedicated pool, 8 Managed Instance, 11 Synapse
/// serverless pool (or Fabric reached as SQL Server, which can't be told
/// apart), 12 SQL database in Fabric; any other, the boxed release.
fn edition_builtins(
    version: Option<&str>,
    edition: i32,
    update: Option<&str>,
) -> Vec<FunctionInfo> {
    match edition {
        5 | 12 => parse_catalog(shared::MSSQL, None),
        8 if update.is_some_and(|u| u.eq_ignore_ascii_case("CU")) => {
            parse_catalog(shared::MSSQL, Some(update_policy_release(version)))
        }
        8 => parse_catalog(shared::MSSQL, None),
        6 | 11 => synapse_builtins(),
        _ => parse_catalog(shared::MSSQL, version),
    }
}

/// The SQL Server release a Managed Instance on a release's update policy
/// follows: the one it reports, or SQL Server 2022 (the policy Microsoft
/// documents) when it reports the cloud's 12.0.
fn update_policy_release(version: Option<&str>) -> &str {
    match version {
        Some(v) if version_numbers(v).first().is_some_and(|major| *major >= 16) => v,
        _ => "16",
    }
}

/// Synapse's pools: SQL Server 2019's functions (`STRING_AGG`,
/// `APPROX_COUNT_DISTINCT`), plus the newer ones Microsoft documents for
/// Azure Synapse Analytics.
fn synapse_builtins() -> Vec<FunctionInfo> {
    let mut out = parse_catalog(shared::MSSQL, Some("15"));
    out.extend(
        parse_catalog(shared::MSSQL, None)
            .into_iter()
            .filter(|f| SYNAPSE_NEWER.contains(&f.name.as_str())),
    );
    out
}

/// Functions newer than SQL Server 2019 that Synapse has.
const SYNAPSE_NEWER: &[&str] = &["GREATEST", "LEAST", "JSON_PATH_EXISTS", "PRODUCT"];

/// Fabric Data Warehouse (and its SQL analytics endpoint): the whole
/// catalog but what it lacks, plus its own `MEDIAN`.
fn fabric_warehouse_builtins() -> Vec<FunctionInfo> {
    let mut out: Vec<FunctionInfo> = parse_catalog(shared::MSSQL, None)
        .into_iter()
        .filter(|f| !FABRIC_WAREHOUSE_LACKS.contains(&f.name.as_str()))
        .collect();
    out.push(FunctionInfo::new("MEDIAN", FunctionKind::Aggregate).args("(numeric_expression)"));
    out
}

/// SQL Server 2022 and 2025 functions Microsoft doesn't document for
/// Fabric Data Warehouse.
const FABRIC_WAREHOUSE_LACKS: &[&str] = &[
    "CURRENT_TIMEZONE_ID",
    "JSON_CONTAINS",
    "REGEXP_MATCHES",
    "REGEXP_SPLIT_TO_TABLE",
    "VECTOR_DISTANCE",
    "VECTOR_NORM",
    "VECTOR_NORMALIZE",
    "VECTORPROPERTY",
    "VECTOR_SEARCH",
    "AI_GENERATE_EMBEDDINGS",
    "AI_GENERATE_CHUNKS",
];

/// Babelfish: the shared catalog's entries it implements, by name; the
/// catalog up to SQL Server 2017 when its catalog can't be read.
async fn babelfish_builtins(s: &mut SqlServerSession) -> Vec<FunctionInfo> {
    let (mut tsql, mut postgres) = (HashSet::new(), HashSet::new());
    match s.rows(BABELFISH_SQL, &[]).await {
        Ok(rows) => {
            for r in &rows {
                if let (Some(schema), Some(name)) = (text(r, 0), text(r, 1)) {
                    if schema == "sys" {
                        &mut tsql
                    } else {
                        &mut postgres
                    }
                    .insert(name);
                }
            }
        }
        Err(e) => tracing::debug!("function catalog: babelfish: {e}"),
    }
    if tsql.is_empty() {
        return parse_catalog(shared::MSSQL, Some(BEFORE_POSTGRES_NAMESAKES));
    }
    babelfish_catalog(&tsql, &postgres)
}

/// The T-SQL release whose functions PostgreSQL's namesakes stand for:
/// later T-SQL names in `pg_catalog` (`REGEXP_LIKE`) are PostgreSQL's own
/// functions, with PostgreSQL's arguments and behavior.
const BEFORE_POSTGRES_NAMESAKES: &str = "14";

/// The shared catalog's entries Babelfish has: its own T-SQL functions,
/// PostgreSQL's that stand for older T-SQL ones, and its grammar.
fn babelfish_catalog(tsql: &HashSet<String>, postgres: &HashSet<String>) -> Vec<FunctionInfo> {
    let older: HashSet<String> = parse_catalog(shared::MSSQL, Some(BEFORE_POSTGRES_NAMESAKES))
        .into_iter()
        .map(|f| f.name)
        .collect();
    parse_catalog(shared::MSSQL, None)
        .into_iter()
        .filter(|f| {
            BABELFISH_SYNTAX.contains(&f.name.as_str())
                || tsql.contains(&f.name)
                || (postgres.contains(&f.name) && older.contains(&f.name))
        })
        .collect()
}

/// What Babelfish's parser turns into something else (`IIF` into `CASE`),
/// so no function has the name. The niladic `CURRENT_TIMESTAMP` and
/// `*_USER` are not here: the catalog leaves them to the SQL keywords.
const BABELFISH_SYNTAX: &[&str] = &[
    "CAST",
    "CONVERT",
    "TRY_CAST",
    "TRY_CONVERT",
    "COALESCE",
    "NULLIF",
    "IIF",
    "CHOOSE",
    "GREATEST",
    "LEAST",
    "OPENJSON",
];

/// One row of `USER_SQL`.
struct UserRow {
    schema: String,
    name: String,
    kind: String,
    /// `None`: the function has no parameters at all (a LEFT JOIN miss).
    param: Option<i32>,
    param_name: String,
    param_type: String,
    description: String,
}

fn user_row(r: &tiberius::Row) -> UserRow {
    let int = |i| r.get::<i32, _>(i);
    let ty = text(r, 5).unwrap_or_default();
    UserRow {
        schema: text(r, 0).unwrap_or_default(),
        name: text(r, 1).unwrap_or_default(),
        kind: text(r, 2).unwrap_or_default(),
        param: int(3),
        param_name: text(r, 4).unwrap_or_default(),
        param_type: if ty.is_empty() {
            ty
        } else {
            format_type(
                &ty,
                int(6).unwrap_or(0),
                int(7).unwrap_or(0),
                int(8).unwrap_or(0),
            )
        },
        description: text(r, 9).unwrap_or_default(),
    }
}

/// The rows of each function (they come grouped and in parameter order)
/// as one entry: `(@a int, @b nvarchar(50))`, its result (parameter 0 of
/// a scalar function; `TABLE` for a table-valued one) and description.
/// A "scalar" function without a result is table-valued: Babelfish files
/// inline table-valued functions as `FN`.
fn user_functions(rows: impl Iterator<Item = UserRow>) -> Vec<FunctionInfo> {
    let mut out: Vec<FunctionInfo> = Vec::new();
    let mut params: Vec<String> = Vec::new();
    let flush = |f: Option<FunctionInfo>, params: &mut Vec<String>, out: &mut Vec<_>| {
        if let Some(mut f) = f {
            if f.kind == FunctionKind::Scalar && f.returns.is_empty() {
                f.kind = FunctionKind::Table;
            }
            if f.kind == FunctionKind::Table {
                f.returns = "TABLE".into();
            }
            out.push(f.args(format!("({})", params.join(", "))));
        }
        params.clear();
    };
    let mut cur: Option<FunctionInfo> = None;
    for r in rows {
        if r.name.is_empty() {
            continue;
        }
        let same = cur
            .as_ref()
            .is_some_and(|c| c.name == r.name && c.schema == r.schema);
        if !same {
            flush(cur.take(), &mut params, &mut out);
            let kind = match r.kind.as_str() {
                "IF" | "TF" | "FT" => FunctionKind::Table,
                "AF" => FunctionKind::Aggregate,
                _ => FunctionKind::Scalar,
            };
            cur = Some(
                FunctionInfo::new(r.name.clone(), kind)
                    .description(r.description.trim())
                    .schema(r.schema.clone()),
            );
        }
        match r.param {
            Some(0) => {
                if let Some(c) = cur.as_mut() {
                    c.returns = r.param_type;
                }
            }
            Some(_) => params.push(format!("{} {}", r.param_name, r.param_type)),
            None => {}
        }
    }
    flush(cur, &mut params, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, kind: &str, param: Option<i32>, pname: &str, ty: &str) -> UserRow {
        UserRow {
            schema: "ventas".into(),
            name: name.into(),
            kind: kind.into(),
            param,
            param_name: pname.into(),
            param_type: ty.into(),
            description: if name == "total" {
                " Total con IVA ".into()
            } else {
                String::new()
            },
        }
    }

    #[test]
    fn user_functions_gather_their_parameters() {
        let got = user_functions(
            [
                row("total", "FN", Some(0), "", "decimal(18,2)"),
                row("total", "FN", Some(1), "@neto", "decimal(18,2)"),
                row("total", "FN", Some(2), "@pais", "nchar(2)"),
                row("pedidos_de", "IF", Some(1), "@cliente", "int"),
                row("ahora", "FN", Some(0), "", "datetime2(7)"),
                row("mediana", "AF", Some(0), "", "float"),
                row("mediana", "AF", Some(1), "@valor", "float"),
                row("sin_parametros", "TF", None, "", ""),
                // Babelfish: an inline table-valued function as `FN`.
                row("numeros", "FN", Some(1), "@hasta", "int"),
            ]
            .into_iter(),
        );
        let by = |n: &str| got.iter().find(|f| f.name == n).unwrap();
        assert_eq!(got.len(), 6);
        let numeros = by("numeros");
        assert_eq!(
            (numeros.kind, numeros.returns.as_str()),
            (FunctionKind::Table, "TABLE")
        );
        let total = by("total");
        assert_eq!(total.args, "(@neto decimal(18,2), @pais nchar(2))");
        assert_eq!(total.returns, "decimal(18,2)");
        assert_eq!(total.description, "Total con IVA");
        assert_eq!(total.schema, "ventas");
        assert_eq!(total.kind, FunctionKind::Scalar);
        let tvf = by("pedidos_de");
        assert_eq!(
            (tvf.kind, tvf.args.as_str()),
            (FunctionKind::Table, "(@cliente int)")
        );
        assert_eq!(tvf.returns, "TABLE");
        assert_eq!(by("ahora").args, "()");
        assert_eq!(by("mediana").kind, FunctionKind::Aggregate);
        assert_eq!(by("sin_parametros").args, "()");
    }

    fn name_set(c: Vec<FunctionInfo>) -> HashSet<String> {
        c.into_iter().map(|f| f.name).collect()
    }

    #[track_caller]
    fn all_in(set: &HashSet<String>, names: &[&str]) {
        for n in names {
            assert!(set.contains(*n), "{n} missing");
        }
    }

    #[track_caller]
    fn none_in(set: &HashSet<String>, names: &[&str]) {
        for n in names {
            assert!(!set.contains(*n), "{n} listed");
        }
    }

    #[test]
    fn the_catalog_follows_the_release() {
        let names = |v: Option<&str>, edition| name_set(edition_builtins(v, edition, None));
        let sql2008r2 = names(Some("10.50.6000.34"), 3);
        assert!(sql2008r2.contains("IS_ROLEMEMBER"));
        assert!(!sql2008r2.contains("IIF"));
        let sql2019 = names(Some("15.0.4153.1"), 3);
        assert!(sql2019.contains("STRING_AGG") && sql2019.contains("APPROX_COUNT_DISTINCT"));
        assert!(!sql2019.contains("GREATEST") && !sql2019.contains("DATETRUNC"));
        let sql2022 = names(Some("16.0.4135.4"), 3);
        assert!(sql2022.contains("GREATEST") && !sql2022.contains("REGEXP_LIKE"));
        all_in(&sql2022, &["CURRENT_TIMEZONE", "CURRENT_TIMEZONE_ID"]);
        none_in(&sql2019, &["CURRENT_TIMEZONE", "CURRENT_TIMEZONE_ID"]);
        // Azure SQL Database and SQL database in Fabric say 12.0 and have
        // everything.
        for edition in [5, 12] {
            let cloud = names(Some("12.0.2000.8"), edition);
            all_in(
                &cloud,
                &["REGEXP_LIKE", "JSON_OBJECTAGG", "VECTOR_DISTANCE"],
            );
        }
    }

    #[test]
    fn managed_instance_follows_its_update_policy() {
        let mi = |update| name_set(edition_builtins(Some("12.0.2000.8"), 8, update));
        // Always-up-to-date: the newest engine.
        for update in [Some("Continuous"), None] {
            all_in(&mi(update), &["REGEXP_LIKE", "VECTOR_DISTANCE", "GREATEST"]);
        }
        // SQL Server 2022 update policy: 2022's.
        let cu = mi(Some("CU"));
        all_in(&cu, &["GREATEST", "DATETRUNC", "GENERATE_SERIES"]);
        none_in(&cu, &["REGEXP_LIKE", "JSON_OBJECTAGG", "VECTOR_DISTANCE"]);
        // A release's policy that reports its release.
        let cu2025 = name_set(edition_builtins(Some("17.0.1000.7"), 8, Some("CU")));
        all_in(&cu2025, &["REGEXP_LIKE", "VECTOR_DISTANCE"]);
    }

    #[test]
    fn synapse_has_sql_server_2019s_and_what_it_documents() {
        for edition in [6, 11] {
            let synapse = name_set(edition_builtins(Some("10.0.15661.0"), edition, None));
            all_in(
                &synapse,
                &[
                    "STRING_AGG",
                    "APPROX_COUNT_DISTINCT",
                    "TRIM",
                    "GREATEST",
                    "LEAST",
                    "JSON_PATH_EXISTS",
                    "PRODUCT",
                ],
            );
            none_in(
                &synapse,
                &[
                    "DATE_BUCKET",
                    "DATETRUNC",
                    "GENERATE_SERIES",
                    "REGEXP_LIKE",
                    "JSON_OBJECTAGG",
                    "VECTOR_DISTANCE",
                    "CURRENT_TIMEZONE",
                    "APPROX_PERCENTILE_CONT",
                ],
            );
        }
        // One LTRIM: the one-argument form; the characters came in 2022.
        let ltrim: Vec<String> = synapse_builtins()
            .into_iter()
            .filter(|f| f.name == "LTRIM")
            .map(|f| f.args)
            .collect();
        assert_eq!(ltrim, ["(character_expression)"]);
    }

    #[test]
    fn fabric_warehouse_lacks_the_vector_and_ai_functions() {
        let fabric = name_set(fabric_warehouse_builtins());
        all_in(
            &fabric,
            &[
                "REGEXP_LIKE",
                "JSON_OBJECTAGG",
                "DATETRUNC",
                "GENERATE_SERIES",
                "CURRENT_TIMEZONE",
                "MEDIAN",
            ],
        );
        none_in(&fabric, FABRIC_WAREHOUSE_LACKS);
        // Every name it leaves out is in the catalog: no typo hides one.
        let all = name_set(parse_catalog(shared::MSSQL, None));
        all_in(&all, FABRIC_WAREHOUSE_LACKS);
        all_in(&all, SYNAPSE_NEWER);
    }

    /// The editor inserts an entry as a call, `name()`: a niladic function
    /// T-SQL writes without parentheses (`CURRENT_TIMESTAMP`) would be
    /// inserted wrong, so the T-SQL catalogs (SQL Server's and SAP ASE's)
    /// leave them to the SQL keywords.
    #[test]
    fn the_t_sql_catalogs_list_only_calls() {
        for (engine, text) in [("mssql", shared::MSSQL), ("ase", shared::SYBASE)] {
            for f in parse_catalog(text, None) {
                assert!(f.args.starts_with('('), "{engine}: {} {}", f.name, f.args);
            }
        }
        let all = name_set(parse_catalog(shared::MSSQL, None));
        none_in(
            &all,
            &[
                "CURRENT_TIMESTAMP",
                "CURRENT_DATE",
                "CURRENT_USER",
                "SESSION_USER",
                "SYSTEM_USER",
                "USER",
            ],
        );
    }

    #[test]
    fn babelfish_keeps_what_it_implements_and_its_grammar() {
        let set = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<HashSet<_>>();
        let got: HashSet<String> = babelfish_catalog(
            &set(&["LEN", "DATEADD", "STRING_AGG"]),
            &set(&["ACOS", "COUNT", "ROW_NUMBER", "REGEXP_LIKE", "BOOLIN"]),
        )
        .into_iter()
        .map(|f| f.name)
        .collect();
        for n in [
            "LEN",
            "DATEADD",
            "STRING_AGG",
            "ACOS",
            "COUNT",
            "ROW_NUMBER",
            "CAST",
            "IIF",
        ] {
            assert!(got.contains(n), "{n}");
        }
        // Not implemented, PostgreSQL's namesake of a newer one, newer,
        // PostgreSQL's own, or a niladic keyword the catalog leaves to the
        // SQL keywords.
        for n in [
            "CHECKSUM_AGG",
            "REGEXP_LIKE",
            "HASHBYTES",
            "VECTOR_DISTANCE",
            "BOOLIN",
            "CURRENT_TIMESTAMP",
            "CURRENT_USER",
        ] {
            assert!(!got.contains(n), "{n}");
        }
    }
}
