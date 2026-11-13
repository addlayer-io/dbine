//! The function catalog of each API (issue #97).
//!
//! - InfluxQL (v1): static, from the InfluxQL reference, filtered by the
//!   server's version.
//! - Flux (v2): the prelude from the server's `/api/v2/query/suggestions`
//!   (the functions its Flux has, with their parameters), plus the standard
//!   library's packages from a static catalog filtered by the server's Flux
//!   version (`runtime.version()`). Without the suggestions, all static.
//! - SQL (v3): `information_schema.routines`, which carries each function's
//!   kind, result type, description and syntax; static when it can't be read.

use dbine_driver::functions::parse_catalog;
use dbine_driver::{FunctionInfo, FunctionKind};
use serde_json::Value as J;
use std::collections::{BTreeMap, HashMap, HashSet};

const INFLUXQL: &str = include_str!("functions/influxql.txt");
const FLUX: &str = include_str!("functions/flux.txt");
const SQL: &str = include_str!("functions/influxdb3.txt");

/// The line that starts a Flux package's section in `flux.txt`.
const PACKAGE: &str = "# package:";

/// InfluxQL's functions on a server of `version` (`InfluxDB 1.8.10`).
pub fn influxql(version: Option<&str>) -> Vec<FunctionInfo> {
    parse_catalog(INFLUXQL, version)
}

/// The static Flux catalog for a server whose Flux is `flux_version`
/// (`v0.200.0`): the prelude with no schema, each package's functions with
/// its import path as the schema.
pub fn flux_static(flux_version: Option<&str>) -> Vec<FunctionInfo> {
    let mut out = Vec::new();
    for (package, text) in flux_sections() {
        out.extend(
            parse_catalog(text, flux_version)
                .into_iter()
                .map(|f| f.schema(package)),
        );
    }
    out
}

/// `flux.txt` cut at its `# package:` lines: (import path, its lines); the
/// prelude's path is empty.
fn flux_sections() -> Vec<(&'static str, &'static str)> {
    let mut out = Vec::new();
    let mut package = "";
    let mut start = 0;
    let mut at = 0;
    for line in FLUX.split_inclusive('\n') {
        if let Some(p) = line.trim().strip_prefix(PACKAGE) {
            out.push((package, &FLUX[start..at]));
            package = p.trim();
            start = at + line.len();
        }
        at += line.len();
    }
    out.push((package, &FLUX[start..]));
    out
}

/// The Flux catalog with the server's prelude: every function the
/// suggestions list (`{"funcs": [{"name", "params": {name: type}}]}`),
/// detailed from the static catalog where it knows the function (its kind
/// and its parameters in order, optional ones marked), plus the static
/// packages. `None` when the answer lists nothing.
pub fn flux_with_suggestions(
    suggestions: &J,
    flux_version: Option<&str>,
) -> Option<Vec<FunctionInfo>> {
    let funcs = suggestions.get("funcs")?.as_array()?;
    // Unfiltered, to detail a function the server has even where the static
    // catalog's version says otherwise.
    let prelude = parse_catalog(flux_sections()[0].1, None);
    let by_name: BTreeMap<&str, &FunctionInfo> =
        prelude.iter().map(|f| (f.name.as_str(), f)).collect();
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for f in funcs {
        let Some(name) = f.get("name").and_then(J::as_str) else {
            continue;
        };
        // `_`-prefixed names are the prelude's internals.
        if name.starts_with('_') || !seen.insert(name.to_string()) {
            continue;
        }
        out.push(match by_name.get(name) {
            Some(known) => (*known).clone(),
            None => {
                // Sorted: the answer is a JSON object, whose key order is
                // not something to rely on (nor is serde_json's, which
                // depends on its `preserve_order` feature).
                let mut params: Vec<&str> = f
                    .get("params")
                    .and_then(J::as_object)
                    .map(|p| {
                        p.keys()
                            .map(String::as_str)
                            .filter(|k| *k != "tables")
                            .collect()
                    })
                    .unwrap_or_default();
                params.sort_unstable();
                FunctionInfo::new(name, FunctionKind::Scalar)
                    .args(format!("({})", params.join(", ")))
            }
        });
    }
    if out.is_empty() {
        return None;
    }
    out.extend(
        flux_static(flux_version)
            .into_iter()
            .filter(|f| !f.schema.is_empty()),
    );
    Some(out)
}

/// The SQL catalog from `information_schema.routines` (columns
/// `routine_name`, `function_type`, `data_type`, `description`,
/// `syntax_example`, by name): one entry per name and argument list, the
/// arguments from the syntax example's first line (an alias's example names
/// the function it aliases), the result type when every overload agrees.
/// InfluxDB's internal `influx_*` functions are left out.
pub fn sql_from_routines(columns: &[String], rows: &[Vec<J>]) -> Vec<FunctionInfo> {
    let col = |name: &str| columns.iter().position(|c| c.eq_ignore_ascii_case(name));
    let (Some(name_at), Some(kind_at)) = (col("routine_name"), col("function_type")) else {
        return Vec::new();
    };
    let (type_at, desc_at, syntax_at) =
        (col("data_type"), col("description"), col("syntax_example"));
    let text = |row: &Vec<J>, at: Option<usize>| {
        at.and_then(|i| row.get(i))
            .and_then(J::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    // (name, kind, args) → (description, result types), in the server's
    // order.
    let mut order: Vec<(String, FunctionKind, String)> = Vec::new();
    let mut groups: HashMap<(String, FunctionKind, String), (String, Vec<String>)> = HashMap::new();
    for row in rows {
        let name = text(row, Some(name_at));
        if name.is_empty() || name.starts_with("influx_") {
            continue;
        }
        let kind = match text(row, Some(kind_at)).to_ascii_uppercase().as_str() {
            "AGGREGATE" => FunctionKind::Aggregate,
            "WINDOW" => FunctionKind::Window,
            "TABLE" => FunctionKind::Table,
            _ => FunctionKind::Scalar,
        };
        let syntax = text(row, syntax_at);
        let first = syntax.lines().next().unwrap_or("").trim();
        let args = first
            .find('(')
            .map_or_else(|| "()".to_string(), |i| first[i..].to_string());
        let key = (name, kind, args);
        let entry = groups.entry(key.clone()).or_insert_with(|| {
            order.push(key);
            (String::new(), Vec::new())
        });
        if entry.0.is_empty() {
            entry.0 = text(row, desc_at);
        }
        let returns = text(row, type_at);
        if !returns.is_empty() && !entry.1.contains(&returns) {
            entry.1.push(returns);
        }
    }
    order
        .into_iter()
        .map(|key| {
            let (description, returns) = &groups[&key];
            let (name, kind, args) = key;
            FunctionInfo::new(name, kind)
                .args(args)
                .returns(match returns.as_slice() {
                    [one] if !one.contains('{') => one.as_str(),
                    _ => "",
                })
                .description(description.as_str())
        })
        .collect()
}

/// The SQL catalog when the server's can't be read.
pub fn sql_static() -> Vec<FunctionInfo> {
    parse_catalog(SQL, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every entry line of a catalog yields exactly one entry.
    fn parses_whole(text: &str) {
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
    }

    #[test]
    fn the_static_catalogs_parse() {
        parses_whole(INFLUXQL);
        parses_whole(FLUX);
        parses_whole(SQL);
        assert!(sql_static().len() > 250);
    }

    #[test]
    fn influxql_follows_the_servers_version() {
        let names = |v| {
            influxql(Some(v))
                .into_iter()
                .map(|f| f.name)
                .collect::<HashSet<_>>()
        };
        let old = names("InfluxDB 1.5.4");
        assert!(old.contains("MEAN") && old.contains("INTEGRAL"));
        assert!(!old.contains("ABS") && !old.contains("RELATIVE_STRENGTH_INDEX"));
        let new = names("InfluxDB 1.8.10");
        assert!(new.contains("ABS") && new.contains("RELATIVE_STRENGTH_INDEX"));
        let mean = influxql(None)
            .into_iter()
            .find(|f| f.name == "MEAN")
            .unwrap();
        assert_eq!(mean.kind, FunctionKind::Aggregate);
        assert_eq!(mean.args, "(field_key)");
    }

    #[test]
    fn flux_packages_carry_their_import_path() {
        let all = flux_static(Some("v0.200.0"));
        let upper = all.iter().find(|f| f.name == "toUpper").unwrap();
        assert_eq!(upper.schema, "strings");
        assert_eq!(upper.args, "(v)");
        assert_eq!(upper.returns, "string");
        let mean = all.iter().find(|f| f.name == "mean").unwrap();
        assert_eq!(mean.schema, "");
        assert_eq!(mean.kind, FunctionKind::Aggregate);
        assert!(all
            .iter()
            .any(|f| f.name == "measurements" && f.schema == "influxdata/influxdb/schema"));
        // `bitwise` came with Flux 0.173. The universe `join`, deprecated in
        // 0.172, still runs, so it stays; date/boundaries' functions only
        // fail from 0.177.1 on (moved to experimental/date/boundaries).
        assert!(all.iter().any(|f| f.schema == "bitwise"));
        assert!(all.iter().any(|f| f.name == "join" && f.schema.is_empty()));
        assert!(!all.iter().any(|f| f.schema == "date/boundaries"));
        assert!(all
            .iter()
            .any(|f| f.name == "monday" && f.schema == "experimental/date/boundaries"));
        let old = flux_static(Some("v0.150.0"));
        assert!(!old.iter().any(|f| f.schema == "bitwise"));
        assert!(old.iter().any(|f| f.name == "join" && f.schema.is_empty()));
        let moved = flux_static(Some("v0.175.0"));
        assert!(moved
            .iter()
            .any(|f| f.name == "monday" && f.schema == "date/boundaries"));
        assert!(!moved
            .iter()
            .any(|f| f.schema == "experimental/date/boundaries"));
        // A stream of tables without Flux's type variable.
        let cmo = all
            .iter()
            .find(|f| f.name == "chandeMomentumOscillator")
            .unwrap();
        assert_eq!(cmo.returns, "stream");
        assert!(!all.iter().any(|f| f.returns.contains('[')));
        // No internal or testing packages.
        assert!(!all
            .iter()
            .any(|f| f.schema.starts_with("internal") || f.schema.starts_with("testing")));
    }

    #[test]
    fn flux_suggestions_give_the_prelude() {
        let answer = json!({"funcs": [
            {"name": "_fillEmpty", "params": {"createEmpty": "bool", "tables": "stream"}},
            {"name": "mean", "params": {"column": "string", "tables": "stream"}},
            {"name": "newThing", "params": {"b": "int", "a": "int", "tables": "stream"}},
        ]});
        let c = flux_with_suggestions(&answer, Some("v0.200.0")).unwrap();
        let prelude: Vec<_> = c.iter().filter(|f| f.schema.is_empty()).collect();
        assert_eq!(prelude.len(), 2);
        assert_eq!(prelude[0].name, "mean");
        assert_eq!(prelude[0].kind, FunctionKind::Aggregate);
        assert_eq!(prelude[0].args, "(?column)");
        assert_eq!(prelude[1].name, "newThing");
        assert_eq!(prelude[1].args, "(a, b)");
        assert!(c.iter().any(|f| f.schema == "strings"));
        // Entries as InfluxDB 2.9.1 (Flux 0.200.0) writes them.
        let real = json!({"funcs": [
            {"name": "_window", "params": {"createEmpty": "bool", "every": "duration",
                "location": "object", "offset": "duration", "period": "duration",
                "startColumn": "string", "stopColumn": "string", "tables": "stream",
                "timeColumn": "string"}},
            {"name": "from", "params": {"bucket": "string", "bucketID": "string",
                "host": "string", "org": "string", "orgID": "string", "token": "string"}},
            {"name": "join", "params": {"method": "string", "on": "array", "tables": "invalid"}},
            {"name": "now", "params": {}},
        ]});
        let c = flux_with_suggestions(&real, Some("v0.200.0")).unwrap();
        let prelude: Vec<_> = c
            .iter()
            .filter(|f| f.schema.is_empty())
            .map(|f| (f.name.as_str(), f.args.as_str()))
            .collect();
        assert_eq!(
            prelude,
            [
                ("from", "(?bucket, ?bucketID, ?org, ?orgID, ?host, ?token)"),
                ("join", "(?method, ?on)"),
                ("now", "()"),
            ]
        );
        assert!(flux_with_suggestions(&json!({"funcs": []}), None).is_none());
        assert!(flux_with_suggestions(&json!({"code": "not found"}), None).is_none());
    }

    #[test]
    fn routines_map_to_entries() {
        let cols: Vec<String> = [
            "routine_name",
            "function_type",
            "data_type",
            "description",
            "syntax_example",
        ]
        .map(String::from)
        .to_vec();
        let rows = vec![
            vec![
                json!("abs"),
                json!("SCALAR"),
                json!("Float64"),
                json!("Returns the absolute value of a number."),
                json!("abs(numeric_expression)"),
            ],
            vec![
                json!("abs"),
                json!("SCALAR"),
                json!("Int64"),
                json!("Returns the absolute value of a number."),
                json!("abs(numeric_expression)"),
            ],
            vec![
                json!("mean"),
                json!("AGGREGATE"),
                json!("Float64"),
                json!("Returns the average."),
                json!("avg(expression)"),
            ],
            vec![
                json!("row_number"),
                json!("WINDOW"),
                json!(null),
                json!(""),
                json!("row_number()\nmore"),
            ],
            vec![
                json!("influx_regex_match"),
                json!("SCALAR"),
                json!(null),
                json!(null),
                json!(null),
            ],
            vec![
                json!("sleep"),
                json!("SCALAR"),
                json!(null),
                json!(null),
                json!(null),
            ],
        ];
        let c = sql_from_routines(&cols, &rows);
        let names: Vec<_> = c.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["abs", "mean", "row_number", "sleep"]);
        assert_eq!(c[0].returns, "", "overloads disagree on the result type");
        assert_eq!(c[0].description, "Returns the absolute value of a number.");
        assert_eq!(c[1].kind, FunctionKind::Aggregate);
        assert_eq!(c[1].args, "(expression)");
        assert_eq!(c[1].returns, "Float64");
        assert_eq!(c[2].kind, FunctionKind::Window);
        assert_eq!(c[2].args, "()");
        assert_eq!(c[3].args, "()");
        assert!(sql_from_routines(&["x".to_string()], &rows).is_empty());
    }
}
