//! The function catalog for the editor's completion (#97), from
//! `sys.functions` (Drill 1.15 and later): built-ins and the user's UDF
//! jars. It lists one row per type overload and no kinds, so overloads are
//! folded per name and number of arguments (`any` where their types
//! differ), implementation names (`castBIGINT`, `extractDay`, `equal`…) are
//! left out, and kinds come from the static catalog, which also adds the
//! window functions the planner provides. If the read fails, the static
//! catalog alone.

use crate::{text, DrillSession};
use dbine_driver::functions::{parse_catalog, FunctionInfo, FunctionKind};
use dbine_driver::Result;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Drill's built-ins, generated from `sys.functions` on Drill 1.22 with the
/// same folding, plus the window functions of Drill's documentation.
const DRILL: &str = include_str!("functions/drill.txt");

const SQL: &str =
    "SELECT name, signature, returnType, source FROM sys.functions WHERE internal = false";

/// Implementations of operators and casts that `sys.functions` lists
/// without marking them internal.
const INTERNAL: &[&str] = &[
    "equal",
    "not_equal",
    "greater_than",
    "greater_than_or_equal_to",
    "less_than",
    "less_than_or_equal_to",
    "add",
    "subtract",
    "multiply",
    "divide",
    "negative",
    "and",
    "or",
    "not",
    "xor",
    "concatOperator",
    "booleanAnd",
    "booleanOr",
    "orNoShortCircuit",
    "compareType",
    "compare_to_nulls_high",
    "compare_to_nulls_low",
    "alternate",
    "alternate3",
    "casthigh",
    "newPartitionValue",
    "isnull",
    "isnotnull",
    "istrue",
    "isfalse",
    "isnottrue",
    "isnotfalse",
    "single_value",
    "nonnullrowcount",
    "rowcount",
    "avg_width",
    "sum_width",
    "datetype",
    "timetype",
    "timestamptype",
    "intervaltype",
    "interval_day",
    "interval_year",
    "repeatstr",
    "substr2",
    "substring2",
    "charsubstring",
    "bytesubstring",
];

/// Prefixes of the per-type implementations: `castBIGINT`,
/// `convert_fromINT_BE`, `extractDay`, `timestampdiffHour`,
/// `date_trunc_Month`. The functions users call (`convert_from`,
/// `date_trunc`) have nothing after the prefix.
const IMPLEMENTATION_PREFIXES: &[&str] = &[
    "cast",
    "convert_from",
    "convert_to",
    "extract",
    "timestampdiff",
    "date_trunc_",
];

impl DrillSession {
    pub(crate) async fn function_list(&mut self) -> Result<Vec<FunctionInfo>> {
        let fixed = parse_catalog(DRILL, None);
        match self.records(SQL).await {
            Ok(rows) => {
                let list = from_rows(&rows, &fixed);
                if !list.is_empty() {
                    return Ok(list);
                }
            }
            Err(e) => tracing::debug!("drill: sys.functions unreadable: {e}"),
        }
        Ok(fixed)
    }
}

/// Whether a `sys.functions` row is left out. A built-in's name must be a
/// lowercase ASCII word (`ASSERT_BIGINT`, `>=` and the per-type
/// implementations are Drill's own); a UDF jar's function is kept whatever
/// its case or alphabet, as long as it can be called by name.
fn internal(name: &str, source: &str) -> bool {
    if !source.is_empty() && source != "built-in" {
        let callable = name
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_alphanumeric() || c == '_');
        return !callable;
    }
    let mut chars = name.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_lowercase())
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return true;
    }
    IMPLEMENTATION_PREFIXES.iter().any(|p| {
        name.strip_prefix(p)
            .and_then(|rest| rest.chars().next())
            .is_some_and(|c| c.is_ascii_uppercase())
    }) || INTERNAL.contains(&name)
}

/// One `sys.functions` type: `VARCHAR-REQUIRED` → `varchar`,
/// `LATE-REQUIRED...` → `any...` (LATE is resolved at run time).
fn arg_type(t: &str) -> String {
    let (t, varargs) = match t.strip_suffix("...") {
        Some(t) => (t, "..."),
        None => (t, ""),
    };
    let t = ["-REQUIRED", "-OPTIONAL", "-REPEATED"]
        .iter()
        .find_map(|m| t.strip_suffix(m))
        .unwrap_or(t)
        .to_ascii_lowercase();
    let t = if t == "late" { "any".to_string() } else { t };
    format!("{t}{varargs}")
}

/// `sys.functions`' rows as catalog entries. `fixed` is the static catalog,
/// which gives the kinds and the window functions.
pub(crate) fn from_rows(rows: &[Map<String, Value>], fixed: &[FunctionInfo]) -> Vec<FunctionInfo> {
    let get = |r: &Map<String, Value>, k: &str| r.get(k).map(text).unwrap_or_default();
    // Name and arity → the overloads' types, return types and source.
    type Overloads = Vec<(Vec<String>, String)>;
    let mut groups: BTreeMap<(String, usize), (Overloads, String)> = BTreeMap::new();
    for r in rows {
        let name = get(r, "name");
        if internal(&name, &get(r, "source")) {
            continue;
        }
        let args: Vec<String> = get(r, "signature")
            .split(',')
            .filter(|t| !t.is_empty())
            .map(arg_type)
            .collect();
        let returns = get(r, "returnType").to_ascii_lowercase();
        let returns = if returns == "late" {
            String::new()
        } else {
            returns
        };
        let entry = groups
            .entry((name, args.len()))
            .or_insert_with(|| (Vec::new(), get(r, "source")));
        entry.0.push((args, returns));
    }
    let kinds: HashMap<&str, FunctionKind> =
        fixed.iter().map(|f| (f.name.as_str(), f.kind)).collect();
    let mut out = Vec::new();
    let mut names = HashSet::new();
    for ((name, arity), (overloads, source)) in groups {
        let args: Vec<String> = (0..arity)
            .map(|i| {
                let first = &overloads[0].0[i];
                if overloads.iter().all(|o| &o.0[i] == first) {
                    first.clone()
                } else {
                    "any".to_string()
                }
            })
            .collect();
        let first = &overloads[0].1;
        let returns = if overloads.iter().all(|o| &o.1 == first) {
            first.clone()
        } else {
            String::new()
        };
        let kind = kinds
            .get(name.as_str())
            .copied()
            .unwrap_or(FunctionKind::Scalar);
        let mut f = FunctionInfo::new(name.clone(), kind)
            .args(format!("({})", args.join(", ")))
            .returns(returns);
        // A UDF jar's functions are reached by their bare name too; the
        // jar shows where they come from.
        if !source.is_empty() && source != "built-in" {
            f = f.description(source);
        }
        names.insert(name);
        out.push(f);
    }
    if out.is_empty() {
        return out;
    }
    out.extend(
        fixed
            .iter()
            .filter(|f| f.kind == FunctionKind::Window && !names.contains(&f.name))
            .cloned(),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(name: &str, signature: &str, returns: &str, source: &str) -> Map<String, Value> {
        let Value::Object(m) = json!({
            "name": name,
            "signature": signature,
            "returnType": returns,
            "source": source,
        }) else {
            unreachable!()
        };
        m
    }

    #[test]
    fn the_static_catalog_parses() {
        let mut n = 0;
        for line in DRILL.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            n += 1;
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
        assert!(n > 200, "{n}");
        let list = parse_catalog(DRILL, None);
        let kind = |name: &str| list.iter().find(|f| f.name == name).map(|f| f.kind);
        assert_eq!(kind("sum"), Some(FunctionKind::Aggregate));
        assert_eq!(kind("row_number"), Some(FunctionKind::Window));
        assert_eq!(kind("lower"), Some(FunctionKind::Scalar));
        assert_eq!(kind("castBIGINT"), None);
    }

    #[test]
    fn server_rows_map_into_entries() {
        let fixed = parse_catalog(DRILL, None);
        let rows = vec![
            row("count", "BIGINT-OPTIONAL", "BIGINT", "built-in"),
            row("count", "VARCHAR-REQUIRED", "BIGINT", "built-in"),
            row("lower", "VARCHAR-REQUIRED", "VARCHAR", "built-in"),
            row("now", "", "TIMESTAMP", "built-in"),
            row("collect_to_list", "LATE-REQUIRED...", "LATE", "built-in"),
            row("castBIGINT", "VARCHAR-REQUIRED", "BIGINT", "built-in"),
            row(
                "convert_fromINT_BE",
                "VARBINARY-REQUIRED",
                "INT",
                "built-in",
            ),
            row("convert_from", "", "VARBINARY", "built-in"),
            row(
                "greater_than",
                "INT-REQUIRED,INT-REQUIRED",
                "BIT",
                "built-in",
            ),
            row(">=", "INT-REQUIRED,INT-REQUIRED", "BIT", "built-in"),
            row("ASSERT_BIGINT", "BIGINT-REQUIRED", "BIGINT", "built-in"),
            row(
                "my_mask",
                "VARCHAR-REQUIRED",
                "VARCHAR",
                "masking-udfs-1.0.jar",
            ),
            // A jar's functions keep their case and alphabet.
            row(
                "MaskEmail",
                "VARCHAR-REQUIRED",
                "VARCHAR",
                "masking-udfs-1.0.jar",
            ),
            row(
                "máscara",
                "VARCHAR-REQUIRED",
                "VARCHAR",
                "masking-udfs-1.0.jar",
            ),
            row(
                "<=>",
                "INT-REQUIRED,INT-REQUIRED",
                "BIT",
                "ops-udfs-1.0.jar",
            ),
        ];
        let list = from_rows(&rows, &fixed);
        let get = |name: &str| list.iter().find(|f| f.name == name);
        let count = get("count").unwrap();
        assert_eq!(count.kind, FunctionKind::Aggregate);
        assert_eq!(count.args, "(any)");
        assert_eq!(count.returns, "bigint");
        assert_eq!(get("lower").unwrap().args, "(varchar)");
        assert_eq!(get("now").unwrap().args, "()");
        assert_eq!(get("collect_to_list").unwrap().args, "(any...)");
        assert_eq!(get("collect_to_list").unwrap().returns, "");
        assert!(get("convert_from").is_some());
        for absent in [
            "castBIGINT",
            "convert_fromINT_BE",
            "greater_than",
            ">=",
            "ASSERT_BIGINT",
        ] {
            assert!(get(absent).is_none(), "{absent}");
        }
        let udf = get("my_mask").unwrap();
        assert_eq!(udf.description, "masking-udfs-1.0.jar");
        assert!(get("MaskEmail").is_some());
        assert!(get("máscara").is_some());
        assert!(get("<=>").is_none());
        assert_eq!(get("row_number").unwrap().kind, FunctionKind::Window);
        assert_eq!(list.iter().filter(|f| f.name == "count").count(), 1);
    }
}
