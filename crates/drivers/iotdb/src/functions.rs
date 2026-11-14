//! The function catalog (issue #97): `SHOW FUNCTIONS`, which lists the
//! built-ins and the user's UDFs with their kind; the static catalog gives
//! the built-ins their arguments and stands in, filtered by the server's
//! version, when the list can't be read.

use dbine_driver::functions::parse_catalog;
use dbine_driver::{FunctionInfo, FunctionKind};
use serde_json::Value as J;
use std::collections::{HashMap, HashSet};

const CATALOG: &str = include_str!("functions/iotdb.txt");

pub const SHOW_FUNCTIONS: &str = "SHOW FUNCTIONS";

/// The static catalog for a server of `version`.
pub fn static_catalog(version: Option<&str>) -> Vec<FunctionInfo> {
    parse_catalog(CATALOG, version)
}

/// `SHOW FUNCTIONS`'s rows (columns `FunctionName`, `FunctionType` and, on
/// 2.x, `State`) as the catalog. `FunctionType` is `native` for the
/// aggregates, `built-in UDTF` / `built-in scalar` for the other built-ins
/// and `external UDTF` / `external UDAF` for the user's. A UDF that isn't
/// `AVAILABLE` (2.x) can't be called and is left out. Empty when the
/// columns aren't there.
pub fn from_show_functions(columns: &[String], rows: &[Vec<J>]) -> Vec<FunctionInfo> {
    let col = |name: &str| columns.iter().position(|c| c.eq_ignore_ascii_case(name));
    let (Some(name_at), Some(type_at)) = (col("FunctionName"), col("FunctionType")) else {
        return Vec::new();
    };
    let state_at = col("State");
    let text = |row: &Vec<J>, at: usize| {
        row.get(at)
            .and_then(J::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let known: HashMap<String, FunctionInfo> = parse_catalog(CATALOG, None)
        .into_iter()
        .map(|f| (f.name.to_ascii_uppercase(), f))
        .collect();
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for row in rows {
        let name = text(row, name_at);
        if name.is_empty() || !seen.insert(name.to_ascii_uppercase()) {
            continue;
        }
        if let Some(at) = state_at {
            let state = text(row, at);
            if !state.is_empty() && !state.eq_ignore_ascii_case("AVAILABLE") {
                continue;
            }
        }
        let kind_text = text(row, type_at).to_ascii_lowercase();
        let kind = if kind_text == "native" || kind_text.contains("udaf") {
            FunctionKind::Aggregate
        } else {
            FunctionKind::Scalar
        };
        let args = known
            .get(&name.to_ascii_uppercase())
            .map(|f| f.args.clone())
            .unwrap_or_default();
        out.push(FunctionInfo::new(name, kind).args(args));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_static_catalog_parses() {
        for line in CATALOG.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
    }

    #[test]
    fn the_static_catalog_follows_the_version() {
        let has = |v: &str, name: &str| static_catalog(Some(v)).iter().any(|f| f.name == name);
        assert!(has("Apache IoTDB 1.3.2", "MAX_BY"));
        assert!(!has("Apache IoTDB 1.3.1", "MAX_BY"));
        assert!(has("Apache IoTDB 1.3.1", "STDDEV"));
        assert!(!has("Apache IoTDB 1.0.1", "DIFF"));
        assert!(has("Apache IoTDB 2.0.5", "JEXL"));
        assert!(!has("Apache IoTDB 2.0.7", "JEXL"));
    }

    #[test]
    fn show_functions_rows_map_to_entries() {
        let cols: Vec<String> = ["FunctionName", "FunctionType", "ClassName(UDF)", "State"]
            .map(String::from)
            .to_vec();
        let row = |n: &str, t: &str, s: &str| vec![json!(n), json!(t), json!(""), json!(s)];
        let rows = vec![
            row("AVG", "native", "AVAILABLE"),
            row("TOP_K", "built-in UDTF", "AVAILABLE"),
            row("ROUND", "built-in scalar", "AVAILABLE"),
            row("my_udf", "external UDTF", "AVAILABLE"),
            row("my_agg", "external UDAF", "AVAILABLE"),
            row("broken", "external UDTF", "UNAVAILABLE"),
            row("AVG", "native", "AVAILABLE"),
        ];
        let c = from_show_functions(&cols, &rows);
        let names: Vec<_> = c.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["AVG", "TOP_K", "ROUND", "my_udf", "my_agg"]);
        assert_eq!(c[0].kind, FunctionKind::Aggregate);
        assert_eq!(c[0].args, "(ts)");
        assert_eq!(c[1].kind, FunctionKind::Scalar);
        assert_eq!(c[1].args, "(ts, 'k'='…')");
        assert_eq!(c[3].kind, FunctionKind::Scalar);
        assert_eq!(c[3].args, "");
        assert_eq!(c[4].kind, FunctionKind::Aggregate);
        // 1.x has no State column.
        let c = from_show_functions(&cols[..3], &[vec![json!("ABS"), json!("built-in UDTF")]]);
        assert_eq!(c.len(), 1);
        assert!(from_show_functions(&["x".into()], &rows).is_empty());
    }
}
