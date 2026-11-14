//! The function catalog for the editor's completion (#97). BigQuery doesn't
//! list its built-ins, so they come from a static catalog of Google's
//! GoogleSQL documentation; the user's functions (SQL, JavaScript, table and
//! aggregate UDFs) come from the region's `INFORMATION_SCHEMA.ROUTINES`, one
//! query for every dataset of the project, with their dataset as the
//! schema. Without access to the region's view, or when it reads empty and no
//! `location` was set (the `US` view of a project in another region), the
//! session's dataset's.

use crate::{ddl, monitor, BigQuerySession};
use dbine_driver::functions::{parse_catalog, FunctionInfo, FunctionKind};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::Result;

/// BigQuery's GoogleSQL built-ins, from Google's documentation (the "All
/// functions" list and each function's syntax).
const BIGQUERY: &str = include_str!("functions/bigquery.txt");

impl BigQuerySession {
    pub(crate) async fn function_list(&mut self) -> Result<Vec<FunctionInfo>> {
        let mut list = built_ins();
        let project = quote_ident(Quote::Backtick, &self.api.project);
        let region = monitor::region_qualifier(self.api.location.as_deref());
        let mut scopes = vec![format!("{project}.{region}")];
        if let Some(d) = &self.dataset {
            scopes.push(format!("{project}.{}", quote_ident(Quote::Backtick, d)));
        }
        let explicit_region = self.api.location.is_some();
        for (i, scope) in scopes.iter().enumerate() {
            let sql = format!(
                "SELECT routine_schema, routine_name, routine_type, data_type, ddl \
                 FROM {scope}.INFORMATION_SCHEMA.ROUTINES"
            );
            match self.named_rows(&sql).await {
                Ok(rows) => {
                    let found = rows.iter().filter_map(from_routine).collect::<Vec<_>>();
                    if settled(i, found.len(), explicit_region) {
                        list.extend(found);
                        break;
                    }
                }
                Err(e) => tracing::debug!("bigquery: {scope} routines unreadable: {e}"),
            }
        }
        Ok(list)
    }
}

/// Whether the routines read from scope `i` (0: the region's view, 1: the
/// session's dataset's) are the answer. The region's view with no `location`
/// set is the `US` one: on a project whose datasets live elsewhere it reads
/// empty without failing, so an empty answer there goes on to the dataset's
/// view, whose location BigQuery infers.
pub(crate) fn settled(i: usize, found: usize, explicit_region: bool) -> bool {
    i > 0 || found > 0 || explicit_region
}

/// The static catalog, with namespaced names (`NET.HOST`, `AEAD.ENCRYPT`)
/// split into their namespace and name.
pub(crate) fn built_ins() -> Vec<FunctionInfo> {
    parse_catalog(BIGQUERY, None)
        .into_iter()
        .map(|f| match f.name.rsplit_once('.') {
            Some((ns, name)) => {
                let (ns, name) = (ns.to_string(), name.to_string());
                FunctionInfo { name, ..f }.schema(ns)
            }
            None => f,
        })
        .collect()
}

/// A row of `INFORMATION_SCHEMA.ROUTINES` (lowercase column names). The
/// arguments come from its DDL; procedures aren't functions.
pub(crate) fn from_routine(r: &ddl::Row) -> Option<FunctionInfo> {
    let get = |k: &str| r.get(k).cloned().unwrap_or_default();
    let name = get("routine_name");
    let kind = match get("routine_type").as_str() {
        "FUNCTION" => FunctionKind::Scalar,
        "AGGREGATE FUNCTION" => FunctionKind::Aggregate,
        "TABLE FUNCTION" => FunctionKind::Table,
        _ => return None,
    };
    if name.is_empty() {
        return None;
    }
    Some(
        FunctionInfo::new(name, kind)
            .args(ddl_args(&get("ddl")))
            .returns(get("data_type"))
            .schema(get("routine_schema")),
    )
}

/// The parameter list of `CREATE … FUNCTION <name>(<params>) …`: the first
/// balanced parentheses after the name.
pub(crate) fn ddl_args(ddl: &str) -> String {
    let upper = ddl.to_ascii_uppercase();
    let Some(at) = upper.find("FUNCTION") else {
        return String::new();
    };
    let rest = &ddl[at + "FUNCTION".len()..];
    let mut in_quote = false;
    let mut start = None;
    for (i, c) in rest.char_indices() {
        match c {
            '`' => in_quote = !in_quote,
            '(' if !in_quote => {
                start = Some(i);
                break;
            }
            _ => {}
        }
    }
    let Some(start) = start else {
        return String::new();
    };
    let mut depth = 0;
    let mut quote: Option<char> = None;
    for (i, c) in rest[start..].char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '`' | '\'' | '"') => quote = Some(c),
            (None, '(' | '<') => depth += 1,
            (None, ')' | '>') => {
                depth -= 1;
                if depth == 0 {
                    return rest[start..start + i + 1].to_string();
                }
            }
            _ => {}
        }
    }
    String::new()
}

#[cfg(test)]
mod examples;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_static_catalog_parses() {
        let mut n = 0;
        for line in BIGQUERY.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            n += 1;
            let parsed = parse_catalog(line, None);
            assert_eq!(parsed.len(), 1, "unreadable: {line}");
            assert!(!parsed[0].args.is_empty(), "no arguments: {line}");
            assert!(
                !examples::is_example(&parsed[0].args),
                "an example, not a syntax form: {line}"
            );
        }
        assert!(n > 300, "{n}");
        let list = built_ins();
        let find = |name: &str| list.iter().find(|f| f.name == name);
        // Bare names only: `AI.IF` isn't `IF`.
        let kinds = |name: &str| {
            list.iter()
                .filter(|f| f.name == name && f.schema.is_empty())
                .map(|f| f.kind)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            find("APPROX_COUNT_DISTINCT").unwrap().kind,
            FunctionKind::Aggregate
        );
        assert_eq!(find("ROW_NUMBER").unwrap().kind, FunctionKind::Window);
        assert_eq!(find("APPENDS").unwrap().kind, FunctionKind::Table);
        assert_eq!(find("HOST").unwrap().schema, "NET");
        assert!(list.iter().all(|f| !f.name.contains('.')));
        // Spanner's, not BigQuery's.
        assert!(find("PENDING_COMMIT_TIMESTAMP").is_none());
        for agg in ["ST_CENTROID_AGG", "ST_EXTENT", "ST_UNION_AGG"] {
            assert_eq!(kinds(agg), [FunctionKind::Aggregate], "{agg}");
        }
        assert_eq!(kinds("ST_CLUSTERDBSCAN"), [FunctionKind::Window]);
        assert_eq!(kinds("RANGE_BUCKET").len(), 1);
        assert_eq!(kinds("FORMAT").len(), 1);
        for name in [
            "COALESCE",
            "IF",
            "IFNULL",
            "NULLIF",
            "FLOAT64",
            "LAX_FLOAT64",
            "PATH_LENGTH",
        ] {
            assert_eq!(kinds(name), [FunctionKind::Scalar], "{name}");
        }
        assert_eq!(
            find("DATETIME").unwrap().args,
            "(year, month, day, hour, minute, second)"
        );
    }

    #[test]
    fn an_empty_default_region_falls_through_to_the_dataset() {
        // No location: the `US` view read empty goes on to the dataset.
        assert!(!settled(0, 0, false));
        assert!(settled(0, 3, false));
        // An explicit location is trusted even when empty.
        assert!(settled(0, 0, true));
        // The dataset's view is the last word.
        assert!(settled(1, 0, false));
    }

    #[test]
    fn routine_rows_map_into_entries() {
        let row = |t: &str, ddl: &str| -> ddl::Row {
            [
                ("routine_schema", "sales"),
                ("routine_name", "twice"),
                ("routine_type", t),
                ("data_type", "INT64"),
                ("ddl", ddl),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
        };
        let f = from_routine(&row(
            "FUNCTION",
            "CREATE FUNCTION `p.sales.twice`(x INT64, s STRUCT<a INT64, b STRING>)\nRETURNS INT64 AS (x * 2);",
        ))
        .unwrap();
        assert_eq!(f.kind, FunctionKind::Scalar);
        assert_eq!(f.args, "(x INT64, s STRUCT<a INT64, b STRING>)");
        assert_eq!(f.returns, "INT64");
        assert_eq!(f.schema, "sales");
        let t = from_routine(&row(
            "TABLE FUNCTION",
            "CREATE TABLE FUNCTION sales.top(n INT64) AS SELECT 1",
        ))
        .unwrap();
        assert_eq!(t.kind, FunctionKind::Table);
        assert_eq!(t.args, "(n INT64)");
        assert!(from_routine(&row("PROCEDURE", "CREATE PROCEDURE sales.p() BEGIN END")).is_none());
        assert_eq!(ddl_args("CREATE FUNCTION `a(b`.f() AS (1)"), "()");
    }
}
