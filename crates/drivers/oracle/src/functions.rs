//! The functions the editor completes on Oracle
//! ([`dbine_driver::Session::function_catalog`]).
//!
//! Built-ins come from the documented catalog ([`shared::ORACLE`]), kept
//! for the server's release. `V$SQLFN_METADATA` (when the user can read
//! it) says which functions this server really has: a documented function
//! it lists is offered even where the catalog's `since` says otherwise (a
//! backport to a release update). The view alone is no catalog: most of
//! its rows are undocumented internals (`SYS_OP_*`, `OPT*`…), its `DESCR`
//! repeats the name, and it leaves out what the parser handles itself
//! (`SYSDATE`, `XMLELEMENT`, `JSON_TABLE`).
//!
//! The schema's own functions come from `ALL_PROCEDURES` and
//! `ALL_ARGUMENTS`: standalone functions (in `schema`, the owner) and the
//! functions of its packages (in `schema`, the package), aggregates
//! (`AGGREGATE`) and pipelined table functions.

use crate::{err, OracleSession};
use dbine_driver::functions::{parse_catalog, shared};
use dbine_driver::{FunctionInfo, FunctionKind, Result};
use oracledb::Connection;
use std::collections::HashSet;

/// The function names this server knows.
const SERVER_NAMES: &str = "SELECT DISTINCT name FROM v$sqlfn_metadata";

/// The schema's functions with their arguments, one row per argument;
/// position 0 is the result (a group without it is a procedure).
const USER_FUNCTIONS: &str = "SELECT p.object_name, p.procedure_name, p.object_type, p.aggregate,
       p.pipelined, p.subprogram_id, a.position, a.argument_name, a.data_type, a.in_out,
       a.defaulted, a.type_name
  FROM all_procedures p
  JOIN all_arguments a
    ON a.owner = p.owner AND a.object_id = p.object_id AND a.subprogram_id = p.subprogram_id
 WHERE p.owner = :1 AND p.object_type IN ('FUNCTION', 'PACKAGE') AND a.data_level = 0
 ORDER BY p.object_name, p.procedure_name, p.subprogram_id, a.sequence";

/// One `USER_FUNCTIONS` row.
#[derive(Debug, Clone, Default)]
pub(crate) struct ArgRow {
    pub object: String,
    /// The subprogram of a package; `None` for a standalone function.
    pub procedure: Option<String>,
    pub aggregate: bool,
    pub pipelined: bool,
    pub subprogram: i64,
    pub position: i64,
    pub name: Option<String>,
    pub data_type: Option<String>,
    pub in_out: Option<String>,
    pub defaulted: bool,
    pub type_name: Option<String>,
}

/// The documented built-ins for `version`, plus those the catalog dates
/// later but `on_server` (the view's names, upper case) lists.
pub(crate) fn builtins(
    version: Option<&str>,
    on_server: Option<&HashSet<String>>,
) -> Vec<FunctionInfo> {
    let kept: HashSet<(String, String)> = parse_catalog(shared::ORACLE, version)
        .into_iter()
        .map(|f| (f.name, f.args))
        .collect();
    parse_catalog(shared::ORACLE, None)
        .into_iter()
        .filter(|f| {
            kept.contains(&(f.name.clone(), f.args.clone()))
                || on_server.is_some_and(|s| s.contains(&f.name.to_ascii_uppercase()))
        })
        .collect()
}

/// `USER_FUNCTIONS` rows (ordered by subprogram) as catalog entries.
pub(crate) fn user_functions(owner: &str, rows: &[ArgRow]) -> Vec<FunctionInfo> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let first = &rows[i];
        let mut j = i;
        while j < rows.len()
            && rows[j].object == first.object
            && rows[j].procedure == first.procedure
            && rows[j].subprogram == first.subprogram
        {
            j += 1;
        }
        let group = &rows[i..j];
        i = j;
        // Position 0 is the result: without it, a procedure.
        let Some(ret) = group.iter().find(|a| a.position == 0) else {
            continue;
        };
        let args: Vec<String> = group
            .iter()
            .filter(|a| a.position > 0)
            .map(|a| {
                let ty = type_of(a);
                let mode = match a.in_out.as_deref() {
                    Some("OUT") => " OUT",
                    Some("IN/OUT") => " IN OUT",
                    _ => "",
                };
                let arg = match a.name.as_deref() {
                    Some(n) => format!("{}{mode} {ty}", n.to_ascii_lowercase()),
                    None => ty,
                };
                if a.defaulted {
                    format!("[{arg}]")
                } else {
                    arg
                }
            })
            .collect();
        let kind = if first.aggregate {
            FunctionKind::Aggregate
        } else if first.pipelined {
            FunctionKind::Table
        } else {
            FunctionKind::Scalar
        };
        let (name, schema) = match &first.procedure {
            Some(sub) => (sub.clone(), first.object.clone()),
            None => (first.object.clone(), owner.to_string()),
        };
        out.push(
            FunctionInfo::new(name, kind)
                .args(format!("({})", args.join(", ")))
                .returns(type_of(ret))
                .schema(schema),
        );
    }
    out
}

/// An argument's type: the object or collection type's name when it has
/// one, else the scalar type.
fn type_of(a: &ArgRow) -> String {
    a.type_name
        .clone()
        .or_else(|| a.data_type.clone())
        .unwrap_or_default()
}

fn server_names(c: &Connection) -> Result<HashSet<String>> {
    let mut out = HashSet::new();
    for row in c.query(SERVER_NAMES, &[]).map_err(err)? {
        let row = row.map_err(err)?;
        if let Some(n) = row.get::<Option<String>>(0).map_err(err)? {
            out.insert(n.to_ascii_uppercase());
        }
    }
    Ok(out)
}

fn arg_rows(c: &Connection, owner: &str) -> Result<Vec<ArgRow>> {
    let yes = |s: Option<String>| s.as_deref() == Some("YES");
    let mut out = Vec::new();
    for row in c.query(USER_FUNCTIONS, &[&owner]).map_err(err)? {
        let row = row.map_err(err)?;
        out.push(ArgRow {
            object: row.get(0).map_err(err)?,
            procedure: row.get(1).map_err(err)?,
            aggregate: yes(row.get(3).map_err(err)?),
            pipelined: yes(row.get(4).map_err(err)?),
            subprogram: row.get::<Option<i64>>(5).map_err(err)?.unwrap_or(0),
            position: row.get::<Option<i64>>(6).map_err(err)?.unwrap_or(-1),
            name: row.get(7).map_err(err)?,
            data_type: row.get(8).map_err(err)?,
            in_out: row.get(9).map_err(err)?,
            defaulted: row.get::<Option<String>>(10).map_err(err)?.as_deref() == Some("Y"),
            type_name: row.get(11).map_err(err)?,
        });
    }
    Ok(out)
}

impl OracleSession {
    pub(crate) async fn function_catalog_impl(&mut self) -> Result<Vec<FunctionInfo>> {
        let owner = self.schema.clone();
        self.run(move |c| {
            let version = c.version().ok().map(|v| v.to_string());
            let on_server = server_names(c)
                .inspect_err(|e| tracing::debug!("oracle: V$SQLFN_METADATA not read: {e}"))
                .ok();
            let mut out = builtins(version.as_deref(), on_server.as_ref());
            match arg_rows(c, &owner) {
                Ok(rows) => out.extend(user_functions(&owner, &rows)),
                Err(e) => tracing::debug!("oracle: user functions not read: {e}"),
            }
            Ok(out)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(c: &[FunctionInfo]) -> HashSet<&str> {
        c.iter().map(|f| f.name.as_str()).collect()
    }

    #[test]
    fn the_catalog_parses_line_by_line() {
        for line in shared::ORACLE.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
        assert!(parse_catalog(shared::ORACLE, None).len() > 300);
    }

    #[test]
    fn the_release_decides_without_the_view() {
        let v11 = builtins(Some("11.2.0.4.0"), None);
        let n11 = names(&v11);
        assert!(n11.contains("LISTAGG") && n11.contains("NTH_VALUE") && n11.contains("SYSDATE"));
        assert!(!n11.contains("JSON_VALUE") && !n11.contains("APPROX_COUNT_DISTINCT"));
        // Zone maps came with 12.1.
        assert!(!n11.contains("SYS_OP_ZONE_ID"));
        assert!(names(&builtins(Some("12.1.0.2.0"), None)).contains("SYS_OP_ZONE_ID"));
        let v19 = builtins(Some("19.24.0.0.0"), None);
        let n19 = names(&v19);
        assert!(n19.contains("JSON_SERIALIZE") && n19.contains("APPROX_COUNT"));
        assert!(!n19.contains("JSON_TRANSFORM") && !n19.contains("VECTOR_DISTANCE"));
        let v23 = builtins(Some("23.26.3.0.0"), None);
        let n23 = names(&v23);
        assert!(n23.contains("VECTOR_DISTANCE") && n23.contains("BOOLEAN_AND_AGG"));
        // Unknown release: everything.
        assert_eq!(
            builtins(None, None).len(),
            parse_catalog(shared::ORACLE, None).len()
        );
        // Kinds and niladic functions.
        let count = v19.iter().find(|f| f.name == "COUNT").unwrap();
        assert_eq!(count.kind, FunctionKind::Aggregate);
        let sysdate = v19.iter().find(|f| f.name == "SYSDATE").unwrap();
        assert_eq!(
            (sysdate.args.as_str(), sysdate.returns.as_str()),
            ("", "DATE")
        );
        let rn = v19.iter().find(|f| f.name == "ROW_NUMBER").unwrap();
        assert_eq!(rn.kind, FunctionKind::Window);
        let jt = v19.iter().find(|f| f.name == "JSON_TABLE").unwrap();
        assert_eq!(jt.kind, FunctionKind::Table);
        assert!(v19
            .iter()
            .all(|f| f.description.is_empty() && f.schema.is_empty()));
    }

    #[test]
    fn a_function_the_server_lists_is_offered_whatever_its_since() {
        // A 19c release update with ANY_VALUE backported (21c in the docs).
        let on: HashSet<String> = ["ANY_VALUE", "SUBSTR", "SYS_OP_INTERNAL"]
            .into_iter()
            .map(String::from)
            .collect();
        let c = builtins(Some("19.8.0.0.0"), Some(&on));
        let n = names(&c);
        assert!(n.contains("ANY_VALUE"));
        // What the view lists but the docs don't stays out.
        assert!(!n.contains("SYS_OP_INTERNAL"));
        // What the parser handles is kept by release.
        assert!(n.contains("SYSDATE") && !n.contains("JSON_TRANSFORM"));
    }

    fn arg(
        object: &str,
        procedure: Option<&str>,
        sub: i64,
        pos: i64,
        name: Option<&str>,
        ty: &str,
    ) -> ArgRow {
        ArgRow {
            object: object.into(),
            procedure: procedure.map(String::from),
            subprogram: sub,
            position: pos,
            name: name.map(String::from),
            data_type: Some(ty.into()),
            in_out: Some(if pos == 0 { "OUT" } else { "IN" }.into()),
            ..Default::default()
        }
    }

    #[test]
    fn user_rows_map_to_functions() {
        let mut b = arg("F_ADD", None, 1, 2, Some("B"), "NUMBER");
        b.defaulted = true;
        let mut out_arg = arg("PK", Some("G"), 1, 2, Some("Z"), "NUMBER");
        out_arg.in_out = Some("IN/OUT".into());
        let mut agg = arg("SECOND_MAX", None, 1, 0, None, "NUMBER");
        agg.aggregate = true;
        let mut agg_in = arg("SECOND_MAX", None, 1, 1, Some("V"), "NUMBER");
        agg_in.aggregate = true;
        let mut pipe = arg("PK", Some("ROWS_OF"), 4, 0, None, "TABLE");
        pipe.pipelined = true;
        pipe.type_name = Some("T_ROWS".into());
        let rows = vec![
            arg("F_ADD", None, 1, 0, None, "NUMBER"),
            arg("F_ADD", None, 1, 1, Some("A"), "NUMBER"),
            b,
            arg("PK", Some("G"), 1, 0, None, "VARCHAR2"),
            arg("PK", Some("G"), 1, 1, Some("X"), "VARCHAR2"),
            out_arg,
            // A procedure: no result.
            arg("PK", Some("P"), 2, 1, Some("Y"), "NUMBER"),
            arg("PK", Some("H"), 3, 0, None, "DATE"),
            pipe,
            agg,
            agg_in,
        ];
        let c = user_functions("SALES", &rows);
        assert_eq!(c.len(), 5, "{c:#?}");
        assert_eq!(c[0].name, "F_ADD");
        assert_eq!(c[0].schema, "SALES");
        assert_eq!(c[0].args, "(a NUMBER, [b NUMBER])");
        assert_eq!(c[0].returns, "NUMBER");
        assert_eq!(c[0].kind, FunctionKind::Scalar);
        assert_eq!(c[1].name, "G");
        assert_eq!(c[1].schema, "PK");
        assert_eq!(c[1].args, "(x VARCHAR2, z IN OUT NUMBER)");
        assert_eq!((c[2].name.as_str(), c[2].args.as_str()), ("H", "()"));
        assert_eq!(c[3].kind, FunctionKind::Table);
        assert_eq!(c[3].returns, "T_ROWS");
        assert_eq!(c[4].kind, FunctionKind::Aggregate);
        assert_eq!(c[4].args, "(v NUMBER)");
        assert!(c.iter().all(|f| f.description.is_empty()));
    }
}
