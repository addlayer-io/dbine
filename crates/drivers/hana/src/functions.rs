//! The functions the editor completes on SAP HANA
//! ([`dbine_driver::Session::function_catalog`]).
//!
//! HANA lists no built-ins in its catalog: they are a static catalog
//! (`functions/hana.txt`, from the SQL Reference) kept for the server's
//! version. The schema's own functions come from `SYS.FUNCTIONS` with
//! `SYS.FUNCTION_PARAMETERS`, in one query: scalar functions and table
//! functions (`RETURNS TABLE`), with `schema` set.

use crate::{format_type, int, text, HanaSession};
use dbine_driver::functions::parse_catalog;
use dbine_driver::{FunctionInfo, FunctionKind, Result, Session};
use hdbconnect_async::HdbValue;

const BUILTINS: &str = include_str!("functions/hana.txt");

/// The arguments of a function whose parameters weren't read.
const UNKNOWN_ARGS: &str = "(...)";

/// The schema's functions with their parameters, one row per parameter.
const FUNCTIONS: &str = "SELECT f.FUNCTION_NAME, f.FUNCTION_OID, p.PARAMETER_TYPE, p.POSITION,
       p.PARAMETER_NAME, p.DATA_TYPE_NAME, p.LENGTH, p.SCALE, p.TABLE_TYPE_NAME, p.HAS_DEFAULT_VALUE
  FROM SYS.FUNCTIONS f
  LEFT JOIN SYS.FUNCTION_PARAMETERS p ON p.FUNCTION_OID = f.FUNCTION_OID
 WHERE f.SCHEMA_NAME = ?
 ORDER BY f.FUNCTION_NAME, f.FUNCTION_OID, p.POSITION";

/// Names only, should the parameters view not answer.
const FUNCTION_NAMES: &str =
    "SELECT FUNCTION_NAME, FUNCTION_OID FROM SYS.FUNCTIONS WHERE SCHEMA_NAME = ?
 ORDER BY FUNCTION_NAME, FUNCTION_OID";

/// One `FUNCTIONS` row.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ParamRow {
    pub function: String,
    /// Tells overloads apart.
    pub oid: String,
    /// `IN`, `OUT`, `INOUT` or `RETURN`; `None`: no parameter row.
    pub role: Option<String>,
    pub name: Option<String>,
    pub data_type: String,
    /// The table type of a `RETURNS TABLE (…)` or a table parameter.
    pub table_type: Option<String>,
    pub defaulted: bool,
}

/// The built-ins of `version` (`SAP HANA 2.00.070…`; unknown: all).
pub(crate) fn builtins(version: Option<&str>) -> Vec<FunctionInfo> {
    parse_catalog(BUILTINS, version)
}

/// `FUNCTIONS` rows (grouped by function) as catalog entries in `schema`.
pub(crate) fn schema_functions(schema: &str, rows: &[ParamRow]) -> Vec<FunctionInfo> {
    let mut out = Vec::new();
    for group in rows.chunk_by(|a, b| a.function == b.function && a.oid == b.oid) {
        let f = &group[0];
        let is_return = |p: &&ParamRow| p.role.as_deref() == Some("RETURN");
        let table = group
            .iter()
            .filter(is_return)
            .any(|p| p.table_type.is_some() || p.data_type == "TABLE_TYPE");
        let returns = if table {
            "TABLE".to_string()
        } else {
            group
                .iter()
                .filter(is_return)
                .map(|p| p.data_type.clone())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let args: Vec<String> = group
            .iter()
            .filter(|p| p.role.is_some() && !is_return(p))
            .map(|p| {
                let ty = p.table_type.clone().unwrap_or_else(|| p.data_type.clone());
                let mode = match p.role.as_deref() {
                    Some("OUT") => "OUT ",
                    Some("INOUT") => "INOUT ",
                    _ => "",
                };
                let arg = match p.name.as_deref() {
                    Some(n) => format!("{mode}{} {ty}", n.to_ascii_lowercase()),
                    None => format!("{mode}{ty}"),
                };
                if p.defaulted {
                    format!("[{arg}]")
                } else {
                    arg
                }
            })
            .collect();
        let kind = if table {
            FunctionKind::Table
        } else {
            FunctionKind::Scalar
        };
        // Names only (the parameters view didn't answer): arguments unknown,
        // which is not the same as none (empty `args` reads as a function
        // called without parentheses).
        let known = group.iter().any(|p| p.role.is_some());
        out.push(
            FunctionInfo::new(f.function.clone(), kind)
                .args(if known {
                    format!("({})", args.join(", "))
                } else {
                    UNKNOWN_ARGS.to_string()
                })
                .returns(returns)
                .schema(schema),
        );
    }
    out
}

fn param_row(r: &[HdbValue]) -> Option<ParamRow> {
    let t = |i: usize| r.get(i).and_then(text).filter(|s| !s.is_empty());
    let g = |i: usize| r.get(i).and_then(int);
    let role = t(2);
    Some(ParamRow {
        function: t(0)?,
        oid: t(1).unwrap_or_default(),
        data_type: match (&role, t(5)) {
            (Some(_), Some(ty)) => format_type(&ty, g(6), g(7)),
            _ => String::new(),
        },
        role,
        name: t(4),
        table_type: t(8),
        defaulted: t(9).is_some_and(|v| v.eq_ignore_ascii_case("TRUE")),
    })
}

impl HanaSession {
    pub(crate) async fn function_catalog_impl(&mut self) -> Result<Vec<FunctionInfo>> {
        let version = Session::server_version(self).await.ok();
        let mut out = builtins(version.as_deref());
        let schema = self.schema.clone();
        let rows = match self.rows(FUNCTIONS, &[schema.as_str()]).await {
            Ok(rows) => Ok(rows),
            Err(e) => {
                tracing::debug!("hana: SYS.FUNCTION_PARAMETERS not read: {e}");
                self.rows(FUNCTION_NAMES, &[schema.as_str()]).await
            }
        };
        match rows {
            Ok(rows) => {
                let rows: Vec<ParamRow> = rows.iter().filter_map(|r| param_row(r)).collect();
                out.extend(schema_functions(&schema, &rows));
            }
            Err(e) => tracing::debug!("hana: SYS.FUNCTIONS not read: {e}"),
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_catalog_parses_line_by_line() {
        for line in BUILTINS.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
        assert!(builtins(None).len() > 200);
    }

    #[test]
    fn the_version_decides() {
        let has = |v, n: &str| builtins(v).iter().any(|f| f.name == n);
        assert!(has(Some("SAP HANA 2.00.070.00.1679989823"), "JSON_VALUE"));
        assert!(has(Some("SAP HANA 4.00.000.00.1700000000"), "JSON_TABLE"));
        assert!(!has(Some("SAP HANA 1.00.122.27.1568902538"), "JSON_VALUE"));
        assert!(has(Some("SAP HANA 1.00.122.27.1568902538"), "STRING_AGG"));
        let c = builtins(None);
        assert_eq!(
            c.iter().find(|f| f.name == "STRING_AGG").unwrap().kind,
            FunctionKind::Aggregate
        );
        assert_eq!(
            c.iter()
                .find(|f| f.name == "SERIES_GENERATE_INTEGER")
                .unwrap()
                .kind,
            FunctionKind::Table
        );
        assert_eq!(
            c.iter()
                .find(|f| f.name == "CURRENT_UTCTIMESTAMP")
                .unwrap()
                .args,
            ""
        );
        assert!(c
            .iter()
            .all(|f| f.description.is_empty() && f.schema.is_empty()));
        // SAP's currency and unit conversions.
        assert!(has(None, "CONVERT_CURRENCY") && has(None, "CONVERT_UNIT"));
    }

    fn row(
        function: &str,
        oid: &str,
        role: Option<&str>,
        name: Option<&str>,
        ty: &str,
    ) -> ParamRow {
        ParamRow {
            function: function.into(),
            oid: oid.into(),
            role: role.map(String::from),
            name: name.map(String::from),
            data_type: ty.into(),
            ..Default::default()
        }
    }

    #[test]
    fn server_rows_map_to_functions() {
        let mut rate = row("ADD_TAX", "1", Some("IN"), Some("RATE"), "DECIMAL(9,2)");
        rate.defaulted = true;
        let mut ret_table = row("TOP_CUSTOMERS", "2", Some("RETURN"), None, "TABLE_TYPE");
        ret_table.table_type = Some("_SYS_SS_TBL_123_RET".into());
        let rows = vec![
            row("ADD_TAX", "1", Some("IN"), Some("AMOUNT"), "DECIMAL(18,2)"),
            rate,
            row(
                "ADD_TAX",
                "1",
                Some("RETURN"),
                Some("RESULT"),
                "DECIMAL(18,2)",
            ),
            row("TOP_CUSTOMERS", "2", Some("IN"), Some("N"), "INTEGER"),
            ret_table,
            // Names only (the parameters view didn't answer).
            row("LEGACY_FN", "3", None, None, ""),
        ];
        let c = schema_functions("SALES", &rows);
        assert_eq!(c.len(), 3, "{c:#?}");
        assert_eq!(c[0].name, "ADD_TAX");
        assert_eq!(c[0].args, "(amount DECIMAL(18,2), [rate DECIMAL(9,2)])");
        assert_eq!(c[0].returns, "DECIMAL(18,2)");
        assert_eq!(c[0].kind, FunctionKind::Scalar);
        assert_eq!(c[0].schema, "SALES");
        assert_eq!(c[1].kind, FunctionKind::Table);
        assert_eq!(
            (c[1].args.as_str(), c[1].returns.as_str()),
            ("(n INTEGER)", "TABLE")
        );
        assert_eq!(
            (c[2].name.as_str(), c[2].args.as_str()),
            ("LEGACY_FN", UNKNOWN_ARGS)
        );
    }
}
