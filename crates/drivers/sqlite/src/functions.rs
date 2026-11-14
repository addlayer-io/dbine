//! The function catalog (issue #97) of SQLite, shared with the libSQL
//! driver: what `pragma_function_list` lists on the connection (so the
//! build's options and loaded extensions count), with the argument names of
//! the static catalog [`shared::SQLITE`] where it knows the function. A build
//! without the pragma (before 3.30, or compiled out) gets the static catalog
//! for its version.

use dbine_driver::functions::{parse_catalog, shared};
use dbine_driver::{FunctionInfo, FunctionKind};
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// One query: the functions (`s` scalar, `a` aggregate, `w` window) with
/// their argument counts, the virtual table modules as `m` (for the
/// table-valued functions), and the library's version on every row.
pub const LIST: &str = "SELECT name, type, narg, sqlite_version() FROM pragma_function_list \
     UNION ALL SELECT name, 'm', -1, sqlite_version() FROM pragma_module_list";

/// Keywords SQLite lists as functions but that are written without
/// parentheses.
const KEYWORDS: [&str; 3] = ["current_date", "current_time", "current_timestamp"];

/// Whether a server row's kind and a catalog entry's kind are the same
/// kind of call: scalar, or aggregate (SQLite lists the aggregates that
/// also run as window functions as `w`).
fn same_class(server: &str, kind: FunctionKind) -> bool {
    match server {
        "s" => kind == FunctionKind::Scalar,
        _ => matches!(kind, FunctionKind::Aggregate | FunctionKind::Window),
    }
}

/// `(arg1, arg2)` for a function the static catalog doesn't know, from its
/// argument count; `(...)` when it takes any number.
fn args_of(narg: i64) -> String {
    if narg < 0 {
        return "(...)".into();
    }
    let names: Vec<String> = (1..=narg).map(|i| format!("arg{i}")).collect();
    format!("({})", names.join(", "))
}

/// The catalog from the rows of [`LIST`] (name, kind, argument count) and
/// the server's version.
pub fn from_server(rows: &[(String, String, i64)], version: &str) -> Vec<FunctionInfo> {
    let known = parse_catalog(shared::SQLITE, Some(version));
    let mut groups: BTreeMap<(String, &str), BTreeSet<i64>> = BTreeMap::new();
    let mut modules: HashSet<String> = HashSet::new();
    for (name, kind, narg) in rows {
        let name = name.to_ascii_lowercase();
        if kind == "m" {
            modules.insert(name);
            continue;
        }
        let ident = name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !name.starts_with(|c: char| c.is_ascii_digit());
        if name.is_empty() || !ident || KEYWORDS.contains(&name.as_str()) {
            continue;
        }
        let class = if kind == "s" { "s" } else { "a" };
        groups.entry((name, class)).or_default().insert(*narg);
    }
    let mut out = Vec::new();
    for ((name, class), nargs) in &groups {
        let documented: Vec<&FunctionInfo> = known
            .iter()
            .filter(|f| f.name.eq_ignore_ascii_case(name) && same_class(class, f.kind))
            .collect();
        if documented.is_empty() {
            let kind = if *class == "s" {
                FunctionKind::Scalar
            } else {
                FunctionKind::Aggregate
            };
            out.extend(
                nargs
                    .iter()
                    .map(|&n| FunctionInfo::new(name.clone(), kind).args(args_of(n))),
            );
        } else {
            out.extend(documented.into_iter().cloned());
        }
    }
    // Table-valued functions are virtual table modules, not functions:
    // the PRAGMA ones are always there, JSON's come with JSON.
    let listed = |n: &str| groups.keys().any(|(name, _)| name == n);
    out.extend(
        known
            .iter()
            .filter(|f| f.kind == FunctionKind::Table)
            .filter(|f| {
                let n = f.name.as_str();
                n.starts_with("pragma_")
                    || modules.contains(n)
                    || (n.starts_with("json_") && listed("json"))
                    || (n.starts_with("jsonb_") && listed("jsonb"))
            })
            .cloned(),
    );
    out
}

/// The static catalog for the server's `version`, when the pragma isn't
/// there.
pub fn fallback(version: &str) -> Vec<FunctionInfo> {
    parse_catalog(shared::SQLITE, Some(version))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, kind: &str, narg: i64) -> (String, String, i64) {
        (name.into(), kind.into(), narg)
    }

    #[test]
    fn the_shared_catalog_parses_line_by_line() {
        let mut entries = 0;
        for line in shared::SQLITE.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
            entries += 1;
        }
        assert!(entries > 150, "{entries}");
    }

    #[test]
    fn server_rows_keep_what_the_build_has() {
        let rows = [
            row("abs", "s", 1),
            row("abs", "s", 1), // another text encoding
            row("count", "w", 0),
            row("count", "w", 1),
            row("max", "s", -1),
            row("max", "w", 1),
            row("row_number", "w", 0),
            row("->>", "s", 2),
            row("current_date", "s", 0),
            row("vector_distance_cos", "s", 2),
            row("stats_median", "a", 1),
            row("json", "s", 1),
            row("json_each", "m", -1),
        ];
        let c = from_server(&rows, "3.47.0");
        let find = |n: &str| c.iter().filter(|f| f.name == n).collect::<Vec<_>>();
        assert_eq!(find("abs").len(), 1);
        assert_eq!(find("abs")[0].args, "(x)");
        let count = find("count");
        assert_eq!(count.len(), 2);
        assert!(count.iter().all(|f| f.kind == FunctionKind::Aggregate));
        assert!(count.iter().any(|f| f.args == "(*)"));
        // max is both a scalar (several values) and an aggregate.
        let max = find("max");
        assert_eq!(max.len(), 2);
        assert!(max.iter().any(|f| f.kind == FunctionKind::Scalar));
        assert!(max.iter().any(|f| f.kind == FunctionKind::Aggregate));
        assert_eq!(find("row_number")[0].kind, FunctionKind::Window);
        // Operators and keywords aren't functions.
        assert!(find("->>").is_empty());
        assert!(find("current_date").is_empty());
        // Not in the static catalog: from the argument count.
        let v = find("vector_distance_cos");
        assert_eq!(
            (v[0].kind, v[0].args.as_str()),
            (FunctionKind::Scalar, "(arg1, arg2)")
        );
        assert_eq!(find("stats_median")[0].kind, FunctionKind::Aggregate);
        // Not listed by this build.
        assert!(find("acos").is_empty());
        assert!(find("upper").is_empty());
        // Table-valued: the PRAGMA ones always, JSON's with JSON.
        assert!(!find("pragma_table_info").is_empty());
        assert_eq!(find("json_each").len(), 2);
        assert_eq!(find("json_each")[0].kind, FunctionKind::Table);
        assert!(find("jsonb_each").is_empty());
        assert!(c
            .iter()
            .all(|f| f.description.is_empty() && f.schema.is_empty()));
    }

    #[test]
    fn the_fallback_follows_the_version() {
        let old = fallback("3.32.3");
        assert!(old.iter().any(|f| f.name == "iif"));
        assert!(!old.iter().any(|f| f.name == "concat"));
        assert!(!old.iter().any(|f| f.name == "acos"));
        let new = fallback("3.45.1");
        assert!(new.iter().any(|f| f.name == "concat"));
        assert!(new.iter().any(|f| f.name == "jsonb"));
        assert!(!new.iter().any(|f| f.name == "json_pretty"));
    }
}
