//! The functions the editor completes on Firebird
//! ([`dbine_driver::Session::function_catalog`]).
//!
//! The built-ins are a static catalog (`functions/firebird.txt`, from the
//! Language Reference) kept for the server's engine version: Firebird lists
//! none of them in its catalog. `RDB$FUNCTIONS` adds the database's own
//! functions (PSQL, UDR and legacy UDF), standalone or in packages, and the
//! functions of the system packages (`RDB$BLOB_UTIL`, `RDB$TIME_ZONE_UTIL`…),
//! with their arguments and `COMMENT ON` text, in one query. A package's
//! private functions (declared only in its body) are left out: they can't
//! be called from outside the package.

use crate::{field_type, int, text, FirebirdSession};
use dbine_driver::functions::parse_catalog;
use dbine_driver::{FunctionInfo, FunctionKind, Result, Session};
use rsfbclient_core::Column;

const BUILTINS: &str = include_str!("functions/firebird.txt");

/// Every function with its arguments, one row per argument (one row with
/// NULL arguments for a function without any).
const FUNCTIONS: &str = "
SELECT TRIM(f.RDB$FUNCTION_NAME), TRIM(f.RDB$PACKAGE_NAME), COALESCE(f.RDB$RETURN_ARGUMENT, 0),
       f.RDB$DESCRIPTION, a.RDB$ARGUMENT_POSITION, TRIM(a.RDB$ARGUMENT_NAME),
       COALESCE(fl.RDB$FIELD_TYPE, a.RDB$FIELD_TYPE), COALESCE(fl.RDB$FIELD_SUB_TYPE, a.RDB$FIELD_SUB_TYPE),
       COALESCE(fl.RDB$FIELD_LENGTH, a.RDB$FIELD_LENGTH), COALESCE(fl.RDB$CHARACTER_LENGTH, a.RDB$CHARACTER_LENGTH),
       COALESCE(fl.RDB$FIELD_PRECISION, a.RDB$FIELD_PRECISION), COALESCE(fl.RDB$FIELD_SCALE, a.RDB$FIELD_SCALE),
       CASE WHEN a.RDB$DEFAULT_SOURCE IS NULL THEN 0 ELSE 1 END
  FROM RDB$FUNCTIONS f
  LEFT JOIN RDB$FUNCTION_ARGUMENTS a
    ON a.RDB$FUNCTION_NAME = f.RDB$FUNCTION_NAME
   AND a.RDB$PACKAGE_NAME IS NOT DISTINCT FROM f.RDB$PACKAGE_NAME
  LEFT JOIN RDB$FIELDS fl ON fl.RDB$FIELD_NAME = a.RDB$FIELD_SOURCE
 WHERE (COALESCE(f.RDB$SYSTEM_FLAG, 0) = 0 OR f.RDB$PACKAGE_NAME IS NOT NULL)
   AND COALESCE(f.RDB$PRIVATE_FLAG, 0) = 0
 ORDER BY f.RDB$PACKAGE_NAME NULLS FIRST, f.RDB$FUNCTION_NAME, a.RDB$ARGUMENT_POSITION";

/// One `FUNCTIONS` row.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ArgRow {
    pub function: String,
    pub package: Option<String>,
    /// The position of the result (0: its own; n: the n-th argument, a
    /// legacy UDF's `RETURNS PARAMETER n`).
    pub returns_at: i64,
    pub description: Option<String>,
    /// `None`: the function has no arguments at all.
    pub position: Option<i64>,
    pub name: Option<String>,
    pub data_type: String,
    pub defaulted: bool,
}

/// The built-ins of `version` (`Firebird 5.0.1`; unknown: all).
pub(crate) fn builtins(version: Option<&str>) -> Vec<FunctionInfo> {
    parse_catalog(BUILTINS, version)
}

/// `FUNCTIONS` rows (grouped by package and function) as catalog entries.
pub(crate) fn database_functions(rows: &[ArgRow]) -> Vec<FunctionInfo> {
    let mut out = Vec::new();
    for group in rows.chunk_by(|a, b| a.function == b.function && a.package == b.package) {
        let f = &group[0];
        let at = f.returns_at;
        let returns = group
            .iter()
            .find(|a| a.position == Some(at))
            .map(|a| a.data_type.clone())
            .unwrap_or_default();
        let args: Vec<String> = group
            .iter()
            .filter(|a| a.position.is_some_and(|p| p >= 1 && p != at))
            .map(|a| {
                let arg = match a.name.as_deref().filter(|n| !n.is_empty()) {
                    Some(n) => format!("{} {}", n.to_ascii_lowercase(), a.data_type),
                    None => a.data_type.clone(),
                };
                if a.defaulted {
                    format!("[{arg}]")
                } else {
                    arg
                }
            })
            .collect();
        let mut info = FunctionInfo::new(f.function.clone(), FunctionKind::Scalar)
            .args(format!("({})", args.join(", ")))
            .returns(returns)
            .schema(f.package.clone().unwrap_or_default());
        if let Some(d) = f
            .description
            .as_deref()
            .map(str::trim)
            .filter(|d| !d.is_empty())
        {
            info = info.description(d);
        }
        out.push(info);
    }
    out
}

fn arg_row(r: &[Column]) -> Option<ArgRow> {
    let g = |i: usize| r.get(i).and_then(int);
    let t = |i: usize| {
        r.get(i)
            .and_then(text)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let position = g(4);
    Some(ArgRow {
        function: t(0)?,
        package: t(1),
        returns_at: g(2).unwrap_or(0),
        description: t(3),
        position,
        name: t(5),
        data_type: if position.is_some() {
            field_type(g(6), g(7), g(8), g(9), g(10), g(11))
        } else {
            String::new()
        },
        defaulted: g(12) == Some(1),
    })
}

impl FirebirdSession {
    pub(crate) async fn function_catalog_impl(&mut self) -> Result<Vec<FunctionInfo>> {
        let version = Session::server_version(self).await.ok();
        let mut out = builtins(version.as_deref());
        match self.rows(FUNCTIONS, vec![]).await {
            Ok(rows) => {
                let rows: Vec<ArgRow> = rows.iter().filter_map(|r| arg_row(r)).collect();
                out.extend(database_functions(&rows));
            }
            Err(e) => tracing::debug!("firebird: RDB$FUNCTIONS not read: {e}"),
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
        assert!(builtins(None).len() > 130);
    }

    #[test]
    fn the_engine_version_decides() {
        let names = |v| -> Vec<String> { builtins(v).into_iter().map(|f| f.name).collect() };
        let v3 = names(Some("Firebird 3.0.12"));
        assert!(v3.iter().any(|n| n == "LIST") && v3.iter().any(|n| n == "ROW_NUMBER"));
        assert!(!v3
            .iter()
            .any(|n| n == "CRYPT_HASH" || n == "NTILE" || n == "BLOB_APPEND"));
        let v4 = names(Some("Firebird 4.0.5"));
        assert!(v4.iter().any(|n| n == "CRYPT_HASH") && v4.iter().any(|n| n == "NTILE"));
        assert!(!v4.iter().any(|n| n == "BLOB_APPEND"));
        let v5 = names(Some("Firebird 5.0.4"));
        assert!(v5.iter().any(|n| n == "BLOB_APPEND"));
        assert!(v5.iter().any(|n| n == "UNICODE_CHAR") && v5.iter().any(|n| n == "UNICODE_VAL"));
        assert!(!v4.iter().any(|n| n == "UNICODE_CHAR"));
        // HASH(value) in every version, HASH(value USING CRC32) from 4.0.
        assert_eq!(v3.iter().filter(|n| *n == "HASH").count(), 1);
        assert_eq!(v4.iter().filter(|n| *n == "HASH").count(), 2);
        let c = builtins(Some("Firebird 5.0.4"));
        let now = c.iter().find(|f| f.name == "CURRENT_TIMESTAMP").unwrap();
        assert_eq!(now.args, "");
        assert_eq!(
            c.iter().find(|f| f.name == "LIST").unwrap().kind,
            FunctionKind::Aggregate
        );
        assert!(c
            .iter()
            .all(|f| f.description.is_empty() && f.schema.is_empty()));
    }

    fn row(
        function: &str,
        package: Option<&str>,
        position: Option<i64>,
        name: Option<&str>,
        ty: &str,
    ) -> ArgRow {
        ArgRow {
            function: function.into(),
            package: package.map(String::from),
            position,
            name: name.map(String::from),
            data_type: ty.into(),
            ..Default::default()
        }
    }

    #[test]
    fn server_rows_map_to_functions() {
        let mut rate = row("ADD_TAX", None, Some(2), Some("RATE"), "NUMERIC(9,2)");
        rate.defaulted = true;
        let mut ret = row("ADD_TAX", None, Some(0), None, "NUMERIC(18,2)");
        ret.description = Some("Price with tax".into());
        let mut first = row("ADD_TAX", None, Some(1), Some("AMOUNT"), "NUMERIC(18,2)");
        first.description = Some("Price with tax".into());
        rate.description = Some("Price with tax".into());
        // A legacy UDF returning its second parameter.
        let mut udf = vec![
            row("BLOB_UPPER", None, Some(1), None, "BLOB SUB_TYPE TEXT"),
            row("BLOB_UPPER", None, Some(2), None, "BLOB SUB_TYPE TEXT"),
        ];
        for r in &mut udf {
            r.returns_at = 2;
        }
        let mut rows = vec![ret, first, rate];
        rows.extend(udf);
        rows.push(row(
            "NEW_BLOB",
            Some("RDB$BLOB_UTIL"),
            Some(0),
            None,
            "BLOB SUB_TYPE BINARY",
        ));
        rows.push(row(
            "NEW_BLOB",
            Some("RDB$BLOB_UTIL"),
            Some(1),
            Some("SEGMENTED"),
            "BOOLEAN",
        ));
        rows.push(row("NOW_UTC", None, Some(0), None, "TIMESTAMP"));
        let c = database_functions(&rows);
        assert_eq!(c.len(), 4, "{c:#?}");
        assert_eq!(c[0].name, "ADD_TAX");
        assert_eq!(c[0].args, "(amount NUMERIC(18,2), [rate NUMERIC(9,2)])");
        assert_eq!(c[0].returns, "NUMERIC(18,2)");
        assert_eq!(c[0].description, "Price with tax");
        assert_eq!(c[0].schema, "");
        assert_eq!(c[1].args, "(BLOB SUB_TYPE TEXT)");
        assert_eq!(c[1].returns, "BLOB SUB_TYPE TEXT");
        assert_eq!(c[2].name, "NEW_BLOB");
        assert_eq!(c[2].schema, "RDB$BLOB_UTIL");
        assert_eq!(c[2].args, "(segmented BOOLEAN)");
        assert_eq!(
            (c[3].args.as_str(), c[3].returns.as_str()),
            ("()", "TIMESTAMP")
        );
        assert!(c.iter().all(|f| f.kind == FunctionKind::Scalar));
    }
}
