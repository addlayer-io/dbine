//! The function catalog (issue #97): `duckdb_functions()`, which lists the
//! built-ins with their parameters, return type and description, the
//! functions of the extensions loaded on the connection, and the user's
//! macros. DuckDB's library always has it, so there is no static catalog
//! to fall back to. Window functions live in the binder, not in that list,
//! so they come from [`WINDOW`].

use dbine_driver::functions::parse_catalog;
use dbine_driver::{FunctionInfo, FunctionKind};

/// Built-ins and the session catalog's macros (`?1`), one row per overload:
/// schema, name, type, description, return type, parameter names, their
/// types, the type of the trailing variadic arguments, and whether it's a
/// built-in.
pub const LIST: &str = "SELECT schema_name, function_name, function_type, description, return_type,
        parameters, parameter_types, varargs, internal
 FROM duckdb_functions()
 WHERE (internal OR database_name = ?1)
   AND function_type IN ('scalar', 'aggregate', 'table', 'macro', 'table_macro')
   AND function_name NOT LIKE '\\_\\_%' ESCAPE '\\'";

/// DuckDB's window functions (duckdb.org/docs/sql/functions/window_functions).
pub const WINDOW: &str = include_str!("functions/duckdb_window.txt");

/// One row of [`LIST`].
#[derive(Debug, Default, Clone)]
pub struct Row {
    pub schema: String,
    pub name: String,
    pub kind: String,
    pub description: String,
    pub returns: String,
    pub params: Vec<String>,
    pub types: Vec<String>,
    pub varargs: String,
    pub internal: bool,
}

/// Parameters shown before `...`: table functions list every named
/// option (`read_csv` has dozens).
const MAX_ARGS: usize = 6;

/// `(a VARCHAR, b INTEGER, ANY...)`: each parameter with its type, the
/// type alone where DuckDB names the parameter `col0`, `col1`…
fn args(r: &Row) -> String {
    let mut parts: Vec<String> = Vec::new();
    for (i, p) in r.params.iter().enumerate() {
        if i == MAX_ARGS {
            parts.push("...".into());
            break;
        }
        let ty = r.types.get(i).map(String::as_str).unwrap_or("");
        let generic = p
            .strip_prefix("col")
            .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        parts.push(match (generic, ty.is_empty()) {
            (true, false) => ty.to_string(),
            (_, true) => p.clone(),
            (false, false) => format!("{p} {ty}"),
        });
    }
    if !r.varargs.is_empty() && r.params.len() <= MAX_ARGS {
        parts.push(format!("{}...", r.varargs));
    }
    format!("({})", parts.join(", "))
}

/// A row as an entry; `None` for operators (`+`, `~~`, `->>`), which have
/// a name but aren't called by it.
pub fn entry(r: &Row) -> Option<FunctionInfo> {
    let ident = !r.name.is_empty()
        && r.name.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !r.name.starts_with(|c: char| c.is_ascii_digit());
    if !ident {
        return None;
    }
    let kind = match r.kind.as_str() {
        "aggregate" => FunctionKind::Aggregate,
        "table" | "table_macro" => FunctionKind::Table,
        _ => FunctionKind::Scalar,
    };
    let mut f = FunctionInfo::new(r.name.clone(), kind)
        .args(args(r))
        .returns(r.returns.clone())
        .description(r.description.trim());
    // Built-ins are reached by their bare name; the user's macros by their
    // schema.
    if !r.internal {
        f = f.schema(r.schema.clone());
    }
    Some(f)
}

/// The catalog from the rows of [`LIST`], plus the window functions.
pub fn catalog(rows: &[Row], version: &str) -> Vec<FunctionInfo> {
    let mut out: Vec<FunctionInfo> = rows.iter().filter_map(entry).collect();
    out.extend(parse_catalog(WINDOW, Some(version)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_catalog_parses_line_by_line() {
        for line in WINDOW.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let e = parse_catalog(line, None);
            assert_eq!(e.len(), 1, "unreadable: {line}");
            assert_eq!(e[0].kind, FunctionKind::Window, "{line}");
        }
    }

    #[test]
    fn rows_map_to_entries() {
        let builtin = Row {
            schema: "main".into(),
            name: "string_split".into(),
            kind: "scalar".into(),
            description: " Splits the string. ".into(),
            returns: "VARCHAR[]".into(),
            params: vec!["string".into(), "separator".into()],
            types: vec!["VARCHAR".into(), "VARCHAR".into()],
            internal: true,
            ..Default::default()
        };
        let f = entry(&builtin).unwrap();
        assert_eq!(f.kind, FunctionKind::Scalar);
        assert_eq!(f.args, "(string VARCHAR, separator VARCHAR)");
        assert_eq!(f.returns, "VARCHAR[]");
        assert_eq!(f.description, "Splits the string.");
        assert_eq!(f.schema, "");

        let unnamed = Row {
            name: "concat".into(),
            kind: "scalar".into(),
            params: vec!["col0".into()],
            types: vec!["ANY".into()],
            varargs: "ANY".into(),
            internal: true,
            ..Default::default()
        };
        assert_eq!(entry(&unnamed).unwrap().args, "(ANY, ANY...)");

        let options = Row {
            name: "read_csv".into(),
            kind: "table".into(),
            params: (0..9).map(|i| format!("p{i}")).collect(),
            types: vec!["ANY".into(); 9],
            internal: true,
            ..Default::default()
        };
        let f = entry(&options).unwrap();
        assert_eq!(f.kind, FunctionKind::Table);
        assert!(f.args.ends_with("p5 ANY, ...)"), "{}", f.args);

        let macro_ = Row {
            schema: "sales".into(),
            name: "twice".into(),
            kind: "macro".into(),
            params: vec!["a".into()],
            types: vec![String::new()],
            ..Default::default()
        };
        let m = entry(&macro_).unwrap();
        assert_eq!((m.args.as_str(), m.schema.as_str()), ("(a)", "sales"));
        let table_macro = Row {
            kind: "table_macro".into(),
            ..macro_
        };
        assert_eq!(entry(&table_macro).unwrap().kind, FunctionKind::Table);
        let agg = Row {
            name: "sum".into(),
            kind: "aggregate".into(),
            internal: true,
            ..Default::default()
        };
        assert_eq!(entry(&agg).unwrap().kind, FunctionKind::Aggregate);

        let op = Row {
            name: "->>".into(),
            kind: "scalar".into(),
            ..Default::default()
        };
        assert!(entry(&op).is_none());
    }
}
