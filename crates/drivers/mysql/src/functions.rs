//! The functions the editor completes ([`dbine_driver::Session::function_catalog`],
//! issue #97), per engine:
//!
//! - MySQL and MariaDB (Aurora, Cloud SQL): the server's help tables
//!   (`mysql.help_topic`), which follow its version and carry the
//!   description; the static catalog adds what they lack, and stands in
//!   whole when they are empty or unreadable.
//! - TiDB: `SHOW BUILTINS`, the scalar functions this build has, with the
//!   arguments, aggregates and window functions of the static catalog.
//! - Doris and StarRocks (VeloDB): `SHOW FULL BUILTIN FUNCTIONS`
//!   (StarRocks with every signature, Doris 4 by name, with the kind and
//!   arguments of the static catalog), plus the database's and the global
//!   user functions.
//! - Databend: `system.functions` without its operators and internals,
//!   with the server's syntax and description where it has them (the
//!   aggregates) and the static catalog's arguments elsewhere, plus the
//!   static window and table functions (which it doesn't list) and
//!   `system.user_functions`.
//! - OceanBase, SingleStore, Manticore, GreptimeDB: static catalogs from
//!   their documentation, filtered by the engine's own version.
//!
//! The user's functions come from `information_schema.ROUTINES` (with
//! their schema) where the engine keeps them, and the loadable functions
//! from `performance_schema.user_defined_functions` (MySQL 8.0 and later)
//! or `mysql.func` (MariaDB, and MySQL 5.7 or without the performance
//! schema). Every read is one query; one that fails falls back to the
//! static catalog or is left out, never an error.

use crate::session::{at, MySqlSession};
use crate::Variant;
use dbine_driver::functions::{parse_catalog, FunctionInfo, FunctionKind};
use dbine_driver::Result;
use std::collections::HashSet;

const MYSQL: &str = include_str!("functions/mysql.txt");
const MARIADB: &str = include_str!("functions/mariadb.txt");
const TIDB: &str = include_str!("functions/tidb.txt");
const OCEANBASE: &str = include_str!("functions/oceanbase.txt");
const SINGLESTORE: &str = include_str!("functions/singlestore.txt");
const DORIS: &str = include_str!("functions/doris.txt");
const STARROCKS: &str = include_str!("functions/starrocks.txt");
const DATABEND: &str = include_str!("functions/databend.txt");
const MANTICORE: &str = include_str!("functions/manticore.txt");
const GREPTIMEDB: &str = include_str!("functions/greptimedb.txt");

/// The function topics of the help tables, with their category path (the
/// category, its parent and grandparent: MySQL nests the spatial ones one
/// level deeper, MariaDB files them under "Geographic Features").
const HELP_SQL: &str = "SELECT c.name, IFNULL(p.name, ''), IFNULL(g.name, ''), t.description
  FROM mysql.help_topic t
  JOIN mysql.help_category c ON c.help_category_id = t.help_category_id
  LEFT JOIN mysql.help_category p ON p.help_category_id = c.parent_category_id
  LEFT JOIN mysql.help_category g ON g.help_category_id = p.parent_category_id
 WHERE CONCAT_WS(' ', c.name, p.name, g.name) LIKE '%Functions%'
    OR p.name = 'Geographic Features'
    OR c.name IN ('Comparison Operators', 'Sequences')";

/// Stored functions with their arguments, in one query.
const ROUTINES_SQL: &str = "SELECT r.ROUTINE_SCHEMA, r.ROUTINE_NAME, MAX(r.DTD_IDENTIFIER),
       MAX(r.ROUTINE_COMMENT),
       GROUP_CONCAT(CONCAT(p.PARAMETER_NAME, ' ', p.DTD_IDENTIFIER)
                    ORDER BY p.ORDINAL_POSITION SEPARATOR ', ')
  FROM information_schema.ROUTINES r
  LEFT JOIN information_schema.PARAMETERS p
    ON p.SPECIFIC_SCHEMA = r.ROUTINE_SCHEMA AND p.SPECIFIC_NAME = r.SPECIFIC_NAME
   AND p.ROUTINE_TYPE = 'FUNCTION' AND p.ORDINAL_POSITION > 0
 WHERE r.ROUTINE_TYPE = 'FUNCTION'
   AND r.ROUTINE_SCHEMA NOT IN ('mysql', 'sys', 'information_schema', 'performance_schema')
 GROUP BY r.ROUTINE_SCHEMA, r.ROUTINE_NAME";

/// Help categories that are about functions by name but list statements
/// or functions most servers don't have (MySQL Enterprise's, and MariaDB's
/// Spider ones, which exist only once Spider's install script creates
/// them, and then come from `mysql.func`).
const SKIPPED_CATEGORIES: &[&str] = &[
    "Internal Functions",
    "Enterprise Encryption Functions",
    "Loadable Functions",
    "User-Defined Functions",
    "Spider Functions",
];

/// MySQL 8.0's loadable functions, those of components included.
const MYSQL_LOADABLE_SQL: &str =
    "SELECT UDF_NAME, UDF_RETURN_TYPE, UDF_TYPE FROM performance_schema.user_defined_functions";

/// The loadable functions `CREATE FUNCTION … SONAME` registered.
const FUNC_TABLE_SQL: &str = "SELECT name, ret, type FROM mysql.func";

/// Aggregates the help tables file under other categories.
const AGGREGATES: &[&str] = &["JSON_ARRAYAGG", "JSON_OBJECTAGG", "ST_COLLECT"];

impl MySqlSession {
    pub(crate) async fn function_catalog_impl(&mut self) -> Result<Vec<FunctionInfo>> {
        let version = self.version_text().await;
        let mut out = match self.variant {
            Variant::MySql => self.help_catalog(MYSQL, &version).await,
            Variant::MariaDb => self.help_catalog(MARIADB, &version).await,
            Variant::TiDb => self.tidb_builtins(&version).await,
            Variant::StarRocks => self.olap_builtins(STARROCKS).await,
            Variant::Doris => self.olap_builtins(DORIS).await,
            Variant::Databend => self.databend_functions().await,
            v => parse_catalog(static_catalog(v), Some(&engine_version(v, &version))),
        };
        out.extend(self.user_functions().await);
        Ok(out)
    }

    async fn version_text(&mut self) -> String {
        self.optional_rows("SELECT VERSION()")
            .await
            .first()
            .and_then(|r| at(r, 0))
            .unwrap_or_default()
    }

    /// The help tables' functions plus what only the static catalog has;
    /// the static catalog alone when the tables are empty or unreadable.
    async fn help_catalog(&mut self, fallback: &str, version: &str) -> Vec<FunctionInfo> {
        let rows: Vec<[String; 4]> = self
            .optional_rows(HELP_SQL)
            .await
            .iter()
            .map(|r| [0, 1, 2, 3].map(|i| at(r, i).unwrap_or_default()))
            .collect();
        let help = from_help_rows(&rows);
        complete(help, parse_catalog(fallback, Some(version)))
    }

    /// TiDB: the scalar functions `SHOW BUILTINS` lists, with the static
    /// catalog's arguments, plus its aggregates and window functions.
    async fn tidb_builtins(&mut self, version: &str) -> Vec<FunctionInfo> {
        let names: Vec<String> = self
            .optional_rows("SHOW BUILTINS")
            .await
            .iter()
            .filter_map(|r| at(r, 0))
            .collect();
        if names.is_empty() {
            return parse_catalog(TIDB, Some(&engine_version(Variant::TiDb, version)));
        }
        tidb_catalog(&names, parse_catalog(TIDB, None))
    }

    /// Doris and StarRocks: the built-ins the frontend lists (StarRocks
    /// and older Doris with their signatures; Doris 4 by name, completed
    /// from the static catalog), the current database's user functions and
    /// the global ones.
    async fn olap_builtins(&mut self, fallback: &str) -> Vec<FunctionInfo> {
        let listed: Vec<FunctionInfo> = self
            .optional_rows("SHOW FULL BUILTIN FUNCTIONS")
            .await
            .iter()
            .filter_map(|r| olap_row(r, ""))
            .collect();
        let known = parse_catalog(fallback, None);
        let mut out = if listed.is_empty() {
            known
        } else {
            olap_catalog(listed, &known)
        };
        let mut user = Vec::new();
        if let Some(db) = self.current_database().await {
            for r in self.optional_rows("SHOW FULL FUNCTIONS").await {
                user.extend(olap_row(&r, &db));
            }
        }
        let global = if self.variant == Variant::Doris {
            "SHOW GLOBAL FULL FUNCTIONS"
        } else {
            "SHOW FULL GLOBAL FUNCTIONS"
        };
        for r in self.optional_rows(global).await {
            user.extend(olap_row(&r, ""));
        }
        out.extend(user);
        out
    }

    /// Databend: `system.functions` (columns read by name, they changed
    /// across versions) and the user's lambda and external functions.
    async fn databend_functions(&mut self) -> Vec<FunctionInfo> {
        let rows = self.optional_rows("SELECT * FROM system.functions").await;
        let listed: Vec<FunctionInfo> = rows.iter().filter_map(databend_row).collect();
        let known = parse_catalog(DATABEND, None);
        let mut out = if listed.is_empty() {
            known
        } else {
            databend_catalog(listed, &known)
        };
        for r in self
            .optional_rows("SELECT name, arguments, description FROM system.user_functions")
            .await
        {
            if let Some(name) = at(&r, 0) {
                out.push(
                    FunctionInfo::new(name, FunctionKind::Scalar)
                        .args(databend_args(&at(&r, 1).unwrap_or_default()))
                        .description(at(&r, 2).unwrap_or_default()),
                );
            }
        }
        out
    }

    /// Stored functions (with their schema) and loadable functions.
    async fn user_functions(&mut self) -> Vec<FunctionInfo> {
        let mut out = Vec::new();
        if self.variant.has_routines() {
            for r in self.optional_rows(ROUTINES_SQL).await {
                let cell = |i| at(&r, i).unwrap_or_default();
                if let Some(f) = routine(&cell(0), &cell(1), &cell(2), &cell(3), &cell(4)) {
                    out.push(f);
                }
            }
        }
        let mut loadable = match self.variant {
            Variant::MySql => self.optional_rows(MYSQL_LOADABLE_SQL).await,
            Variant::MariaDb => Vec::new(),
            _ => return out,
        };
        // MariaDB, and MySQL 5.7 (no `user_defined_functions`) or without
        // the performance schema: the table `CREATE FUNCTION … SONAME`
        // registers them in.
        if loadable.is_empty() {
            loadable = self.optional_rows(FUNC_TABLE_SQL).await;
        }
        for r in loadable {
            let cell = |i| at(&r, i).unwrap_or_default();
            if let Some(f) = loadable_function(&cell(0), &cell(1), &cell(2)) {
                out.push(f);
            }
        }
        out
    }
}

/// The static catalog of an engine without a server catalog.
fn static_catalog(v: Variant) -> &'static str {
    match v {
        Variant::MariaDb => MARIADB,
        Variant::TiDb => TIDB,
        Variant::OceanBase => OCEANBASE,
        Variant::SingleStore => SINGLESTORE,
        Variant::StarRocks => STARROCKS,
        Variant::Doris => DORIS,
        Variant::Databend => DATABEND,
        Variant::Manticore => MANTICORE,
        Variant::GreptimeDb => GREPTIMEDB,
        _ => MYSQL,
    }
}

/// The engine's own version in `VERSION()`, which MySQL-compatible engines
/// prefix with the MySQL release they mimic: `8.0.11-TiDB-v8.5.3` → `v8.5.3`,
/// `5.7.25-OceanBase_CE-v4.2.1.0` → `4.2.1.0`, `8.4.2-GreptimeDB-1.2.1` →
/// `1.2.1`.
fn engine_version(v: Variant, version: &str) -> String {
    let own = match v {
        Variant::TiDb => version.split_once("TiDB-").map(|(_, r)| r),
        Variant::OceanBase => version.split_once("-v").map(|(_, r)| r),
        Variant::GreptimeDb => version.split_once("GreptimeDB-").map(|(_, r)| r),
        _ => None,
    };
    own.unwrap_or(version).to_string()
}

/// `server` plus the entries of `fallback` whose name it lacks.
fn complete(server: Vec<FunctionInfo>, fallback: Vec<FunctionInfo>) -> Vec<FunctionInfo> {
    if server.is_empty() {
        return fallback;
    }
    let names: HashSet<String> = server.iter().map(|f| f.name.to_ascii_uppercase()).collect();
    let mut out = server;
    out.extend(
        fallback
            .into_iter()
            .filter(|f| !names.contains(&f.name.to_ascii_uppercase())),
    );
    out
}

/// The function topics of `HELP_SQL`'s rows, one entry per signature.
fn from_help_rows(rows: &[[String; 4]]) -> Vec<FunctionInfo> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for [category, parent, grand, text] in rows {
        if SKIPPED_CATEGORIES.contains(&category.as_str()) {
            continue;
        }
        let (signatures, description) = help_topic(text);
        for (name, args) in signatures {
            if !seen.insert((name.to_ascii_uppercase(), args.clone())) {
                continue;
            }
            let kind = help_kind(category, parent, grand, &name);
            out.push(
                FunctionInfo::new(name, kind)
                    .args(args)
                    .description(description.clone()),
            );
        }
    }
    out
}

fn help_kind(category: &str, parent: &str, grand: &str, name: &str) -> FunctionKind {
    let upper = name.to_ascii_uppercase();
    if upper == "JSON_TABLE" {
        FunctionKind::Table
    } else if AGGREGATES.contains(&upper.as_str())
        || [category, parent, grand]
            .iter()
            .any(|c| c.contains("GROUP BY") || c.starts_with("Aggregate"))
    {
        FunctionKind::Aggregate
    } else if category == "Window Functions" {
        FunctionKind::Window
    } else {
        FunctionKind::Scalar
    }
}

/// A help topic's signatures (`(name, args)`) and the first paragraph of
/// its description, as the server writes them. MySQL writes `Syntax:` and
/// the syntax (or the syntax alone on the first line), then the text;
/// MariaDB writes `Syntax` and `Description` as underlined headings.
/// Operators (`expr LIKE pat`), statements and topics without a call
/// syntax give no signature.
fn help_topic(text: &str) -> (Vec<(String, String)>, String) {
    let paras = paragraphs(text);
    let heading = |p: &[&str]| p.len() > 1 && p[1].trim_start().starts_with("---");
    let titled = |p: &[&str], word: &str| heading(p) && p[0].trim() == word;
    let (syntax, prose): (Vec<String>, Option<String>) = if paras.iter().any(|p| heading(p)) {
        // MariaDB.
        let start = paras.iter().position(|p| titled(p, "Syntax"));
        let syntax = match start {
            Some(s) => {
                let lines: Vec<&str> = paras[s + 1..]
                    .iter()
                    .take_while(|p| !heading(p))
                    .flatten()
                    .copied()
                    .collect();
                logical_lines(&lines)
            }
            None => Vec::new(),
        };
        let prose = paras
            .iter()
            .position(|p| titled(p, "Description"))
            .and_then(|d| paras.get(d + 1))
            .map(|p| joined(p));
        (syntax, prose)
    } else if let Some(s) = paras.iter().position(|p| p[0].trim() == "Syntax:") {
        (logical_lines(&paras[s][1..]), first_prose(&paras[s + 1..]))
    } else if let Some(first) = paras.first() {
        (
            logical_lines(first).into_iter().take(1).collect(),
            first_prose(&paras[1..]),
        )
    } else {
        (Vec::new(), None)
    };
    let mut signatures: Vec<(String, String)> = Vec::new();
    for line in &syntax {
        for s in call_items(line) {
            if !signatures.contains(&s) {
                signatures.push(s);
            }
        }
    }
    (signatures, prose.map(|p| summary(&p)).unwrap_or_default())
}

/// The text split at blank lines, each paragraph as its lines.
fn paragraphs(text: &str) -> Vec<Vec<&str>> {
    let mut out: Vec<Vec<&str>> = Vec::new();
    let mut cur = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        } else {
            cur.push(line);
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn joined(lines: &[&str]) -> String {
    lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" ")
}

/// Lines joined while a parenthesis is open or the line ends in a comma:
/// one signature (or list of them) per entry.
fn logical_lines(lines: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut depth = 0i32;
    for line in lines {
        let line = line.trim();
        cur.push(line);
        depth += line.matches('(').count() as i32 - line.matches(')').count() as i32;
        if depth <= 0 && !line.ends_with(',') {
            out.push(cur.join(" "));
            cur.clear();
            depth = 0;
        }
    }
    if !cur.is_empty() {
        out.push(cur.join(" "));
    }
    out
}

/// The first paragraph of prose after the syntax: not the `URL:` line nor
/// a grammar rule (`on_empty:`, `*Note*:`).
fn first_prose(paras: &[Vec<&str>]) -> Option<String> {
    paras
        .iter()
        .take_while(|p| !p[0].starts_with("URL:"))
        .find(|p| {
            !p[0]
                .split_whitespace()
                .next()
                .is_some_and(|w| w.ends_with(':'))
        })
        .map(|p| joined(p))
}

/// A paragraph that ends announcing what follows (`…is as follows:`)
/// loses that sentence.
fn summary(p: &str) -> String {
    let p = p.trim();
    if p.ends_with(':') {
        if let Some(end) = p.rfind(". ") {
            return p[..=end].to_string();
        }
    }
    p.to_string()
}

fn ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$'
}

/// The calls a syntax line starts with, separated by commas:
/// `ROUND(X), ROUND(X,D)`, `CURRENT_DATE, CURRENT_DATE()`, `LAG (expr) OVER (…)`.
/// A name must have a capital letter: the help tables write functions in
/// capitals or camel case, and placeholders (`function_name(…)`) in lower
/// case.
fn call_items(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = line.trim();
    loop {
        let len = rest.find(|c: char| !ident_char(c)).unwrap_or(rest.len());
        let name = &rest[..len];
        if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
            break;
        }
        let mut after = rest[len..].trim_start();
        if after.starts_with('(') {
            let Some(close) = closing_paren(after) else {
                break;
            };
            if name.bytes().any(|b| b.is_ascii_uppercase()) {
                out.push((name.to_string(), tidy_args(&after[..close])));
            }
            after = after[close..].trim_start();
        }
        match after.strip_prefix(',') {
            Some(next) => rest = next.trim_start(),
            None => break,
        }
    }
    out
}

/// The byte just past the parenthesis that closes the one `s` starts with.
fn closing_paren(s: &str) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Whitespace collapsed, and none just inside the parentheses.
fn tidy_args(args: &str) -> String {
    args.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("( ", "(")
        .replace(" )", ")")
}

/// A stored function of `ROUTINES_SQL`'s row.
fn routine(
    schema: &str,
    name: &str,
    returns: &str,
    comment: &str,
    params: &str,
) -> Option<FunctionInfo> {
    (!name.is_empty()).then(|| {
        FunctionInfo::new(name, FunctionKind::Scalar)
            .args(format!("({params})"))
            .returns(returns)
            .description(comment.trim())
            .schema(schema)
    })
}

/// A loadable function: MySQL's `UDF_RETURN_TYPE` is a name,
/// `mysql.func.ret` the server's `Item_result` (0 string, 1 real,
/// 2 integer, 4 decimal; 3 is a row, never a function's result).
/// The X Plugin's `mysqlx_*` are its own internals.
fn loadable_function(name: &str, ret: &str, kind: &str) -> Option<FunctionInfo> {
    if name.is_empty() || name.starts_with("mysqlx_") {
        return None;
    }
    let returns = match ret {
        "0" => "string",
        "1" => "real",
        "2" => "integer",
        "4" => "decimal",
        other => other,
    };
    let kind = if kind.eq_ignore_ascii_case("aggregate") {
        FunctionKind::Aggregate
    } else {
        FunctionKind::Scalar
    };
    Some(FunctionInfo::new(name, kind).returns(returns))
}

/// Names in TiDB's `SHOW BUILTINS` that are operators, not functions.
const TIDB_OPERATORS: &[&str] = &[
    "and",
    "or",
    "xor",
    "not",
    "eq",
    "ne",
    "lt",
    "le",
    "gt",
    "ge",
    "nulleq",
    "plus",
    "minus",
    "mul",
    "div",
    "intdiv",
    "unaryminus",
    "bitand",
    "bitor",
    "bitxor",
    "bitneg",
    "leftshift",
    "rightshift",
    "in",
    "like",
    "ilike",
    "regexp",
    "istrue",
    "isfalse",
    "case",
    "setvar",
    "getparam",
    "json_memberof",
];

/// TiDB's internal names of the functions named like a keyword.
const TIDB_RENAMED: &[(&str, &str)] = &[
    ("char_func", "char"),
    ("insert_func", "insert"),
    ("default_func", "default"),
    ("password_func", "password"),
];

/// TiDB's catalog: the functions `SHOW BUILTINS` lists (in lower case),
/// with the arguments of the static entries of the same name, plus the
/// static aggregates and window functions, which it doesn't list.
fn tidb_catalog(names: &[String], known: Vec<FunctionInfo>) -> Vec<FunctionInfo> {
    let listed: Vec<String> = names
        .iter()
        .map(|n| n.trim().to_ascii_lowercase())
        .filter(|n| !n.is_empty() && !TIDB_OPERATORS.contains(&n.as_str()))
        .map(|n| {
            TIDB_RENAMED
                .iter()
                .find(|(internal, _)| *internal == n)
                .map_or(n.clone(), |(_, name)| name.to_string())
                .to_ascii_uppercase()
        })
        .collect();
    let mut out: Vec<FunctionInfo> = Vec::new();
    let mut covered = HashSet::new();
    for f in known {
        let upper = f.name.to_ascii_uppercase();
        if f.kind != FunctionKind::Scalar || listed.contains(&upper) {
            covered.insert(upper);
            out.push(f);
        }
    }
    for n in listed {
        if covered.insert(n.clone()) {
            out.push(FunctionInfo::new(n, FunctionKind::Scalar));
        }
    }
    out
}

/// The window functions of Doris and StarRocks, which their listings
/// file as aggregates.
const OLAP_WINDOW: &[&str] = &[
    "cume_dist",
    "dense_rank",
    "first_value",
    "lag",
    "last_value",
    "lead",
    "nth_value",
    "ntile",
    "percent_rank",
    "rank",
    "row_number",
    "session_number",
];

/// A row of Doris' or StarRocks' `SHOW FULL [BUILTIN] FUNCTIONS`, read
/// by column name: `Signature` (`abs(DOUBLE)`, or the name alone on
/// Doris 4), `Return Type` and `Function Type` (`Scalar`, `Aggregate`,
/// `Table`). Names starting with `_` are internal.
fn olap_row(r: &mysql_async::Row, schema: &str) -> Option<FunctionInfo> {
    olap_entry(|c| crate::session::named(r, &[c]), schema)
}

fn olap_entry(cell: impl Fn(&str) -> Option<String>, schema: &str) -> Option<FunctionInfo> {
    let sig = cell("Signature")?;
    let (name, args) = match sig.find('(') {
        Some(i) => (sig[..i].trim(), sig[i..].trim()),
        None => (sig.trim(), ""),
    };
    if name.is_empty() || name.starts_with('_') {
        return None;
    }
    let kind = match cell("Function Type")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        _ if OLAP_WINDOW.contains(&name) => FunctionKind::Window,
        "aggregate" => FunctionKind::Aggregate,
        "analytic" | "window" => FunctionKind::Window,
        "table" => FunctionKind::Table,
        _ => FunctionKind::Scalar,
    };
    let returns = cell("Return Type")
        .filter(|t| !matches!(t.as_str(), "INVALID_TYPE" | "NULL"))
        .unwrap_or_default();
    Some(
        FunctionInfo::new(name, kind)
            .args(args)
            .returns(returns)
            .schema(schema),
    )
}

/// The listed built-ins, one signature per name and number of arguments
/// (StarRocks lists one per argument type), with the static entries
/// standing in for a name listed without its signature (Doris 4).
fn olap_catalog(listed: Vec<FunctionInfo>, known: &[FunctionInfo]) -> Vec<FunctionInfo> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for f in listed {
        let named: Vec<&FunctionInfo> = if f.args.is_empty() {
            known.iter().filter(|k| k.name == f.name).collect()
        } else {
            Vec::new()
        };
        let entries = if named.is_empty() {
            vec![f]
        } else {
            named.into_iter().cloned().collect()
        };
        for e in entries {
            if seen.insert((e.name.to_ascii_lowercase(), arity(&e.args))) {
                out.push(e);
            }
        }
    }
    out
}

/// How many arguments a signature has: `()` 0, `(INT,INT)` 2, `` none.
fn arity(args: &str) -> Option<usize> {
    let inner = args.strip_prefix('(')?.strip_suffix(')')?.trim();
    Some(if inner.is_empty() {
        0
    } else {
        inner.matches(',').count() + 1
    })
}

/// A row of Databend's `system.functions`: `name`, `is_aggregate` and,
/// on recent servers, `description` and `syntax` (`abs(x)`), which they
/// fill for the aggregates only.
fn databend_row(r: &mysql_async::Row) -> Option<FunctionInfo> {
    databend_entry(|c| crate::session::named(r, &[c]))
}

fn databend_entry(cell: impl Fn(&str) -> Option<String>) -> Option<FunctionInfo> {
    let name = cell("name").filter(|n| !n.is_empty())?;
    let aggregate = cell("is_aggregate").is_some_and(|v| matches!(v.as_str(), "1" | "true"));
    let syntax = cell("syntax").unwrap_or_default();
    let syntax = syntax.trim();
    let args = syntax
        .get(..name.len())
        .filter(|head| head.eq_ignore_ascii_case(&name))
        .map(|_| syntax[name.len()..].trim_start())
        .filter(|a| a.starts_with('('))
        .and_then(|a| closing_paren(a).map(|end| tidy_args(&a[..end])))
        .unwrap_or_default();
    let kind = if aggregate {
        FunctionKind::Aggregate
    } else {
        FunctionKind::Scalar
    };
    Some(
        FunctionInfo::new(name, kind)
            .args(args)
            .description(cell("description").unwrap_or_default()),
    )
}

/// Scalar names in Databend's `system.functions` that are operators
/// (`a = b` is `eq`, `a & b` is `bit_and`) or the planner's internals,
/// not functions to call.
const DATABEND_OPERATORS: &[&str] = &[
    "eq",
    "noteq",
    "gt",
    "gte",
    "lt",
    "lte",
    "and",
    "or",
    "not",
    "xor",
    "and_filters",
    "or_filters",
    "is_true",
    "like",
    "ilike",
    "like_any",
    "ilike_any",
    "regexp",
    "rlike",
    "glob",
    "bit_and",
    "bit_or",
    "bit_xor",
    "bit_not",
    "bit_shift_left",
    "bit_shift_right",
    "get_by_keypath",
    "get_by_keypath_string",
    "delete_by_keypath",
    "json_contains_in_left",
    "json_contains_in_right",
];

/// Databend's catalog: the listed functions but the operators and
/// internals; a name listed without arguments (every scalar on current
/// servers, which document only the aggregates) takes the static entries
/// of that name, with the server's description. The static window and
/// table functions, which `system.functions` doesn't list, are added.
fn databend_catalog(listed: Vec<FunctionInfo>, known: &[FunctionInfo]) -> Vec<FunctionInfo> {
    let mut out = Vec::new();
    let mut covered = HashSet::new();
    for f in listed {
        let lower = f.name.to_ascii_lowercase();
        let internal = lower.starts_with('_')
            || (f.kind == FunctionKind::Scalar && DATABEND_OPERATORS.contains(&lower.as_str()));
        if internal || !covered.insert(lower) {
            continue;
        }
        let named: Vec<&FunctionInfo> = if f.args.is_empty() {
            known
                .iter()
                .filter(|k| k.name.eq_ignore_ascii_case(&f.name))
                .collect()
        } else {
            Vec::new()
        };
        if named.is_empty() {
            out.push(f);
        } else {
            out.extend(
                named
                    .into_iter()
                    .map(|k| k.clone().description(f.description.clone())),
            );
        }
    }
    out.extend(
        known
            .iter()
            .filter(|k| matches!(k.kind, FunctionKind::Window | FunctionKind::Table))
            .filter(|k| !covered.contains(&k.name.to_ascii_lowercase()))
            .cloned(),
    );
    out
}

/// Databend's `system.user_functions.arguments`: `{"parameters":["x","y"]}`
/// (lambdas) or `{"arg_types":["Int32"],…}` (external functions).
fn databend_args(json: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
    let list = ["parameters", "arg_types"]
        .iter()
        .find_map(|k| v.get(k).and_then(|a| a.as_array()));
    match list {
        Some(items) => format!(
            "({})",
            items
                .iter()
                .map(|i| i.as_str().map_or_else(|| i.to_string(), str::to_string))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_static_catalogs_parse() {
        for (engine, text) in [
            ("mysql", MYSQL),
            ("mariadb", MARIADB),
            ("tidb", TIDB),
            ("oceanbase", OCEANBASE),
            ("singlestore", SINGLESTORE),
            ("doris", DORIS),
            ("starrocks", STARROCKS),
            ("databend", DATABEND),
            ("manticore", MANTICORE),
            ("greptimedb", GREPTIMEDB),
        ] {
            let mut entries = 0;
            for line in text.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let parsed = parse_catalog(line, None);
                assert_eq!(parsed.len(), 1, "{engine}: unreadable: {line}");
                assert!(parsed[0].description.is_empty(), "{engine}: {line}");
                entries += 1;
            }
            assert!(entries > 20, "{engine}: {entries} entries");
        }
    }

    fn names(c: &[FunctionInfo]) -> Vec<&str> {
        c.iter().map(|f| f.name.as_str()).collect()
    }

    #[test]
    fn mysql_help_topics_give_their_signatures_and_first_paragraph() {
        let (sigs, desc) = help_topic(
            "Syntax:\nROUND(X), ROUND(X,D)\n\nRounds the argument X to D decimal places. The\nrounding algorithm depends on the data type of X.\n\nURL: https://dev.mysql.com/doc/refman/8.4/en/mathematical-functions.html\n",
        );
        assert_eq!(
            sigs,
            [
                ("ROUND".into(), "(X)".into()),
                ("ROUND".into(), "(X,D)".into())
            ]
        );
        assert_eq!(
            desc,
            "Rounds the argument X to D decimal places. The rounding algorithm depends on the data type of X."
        );
        // Wrapped lists, niladic forms and window syntax.
        let (sigs, _) = help_topic(
            "Syntax:\nTRIM([{BOTH | LEADING | TRAILING} [remstr] FROM] str), TRIM([remstr\nFROM] str)\n\nReturns the string str.\n",
        );
        assert_eq!(sigs[1].1, "([remstr FROM] str)");
        let (sigs, _) =
            help_topic("Syntax:\nCURRENT_DATE, CURRENT_DATE()\n\nCURRENT_DATE and CURRENT_DATE() are synonyms.\n");
        assert_eq!(sigs, [("CURRENT_DATE".into(), "()".into())]);
        // No `Syntax:`: the first line is the syntax.
        let (sigs, desc) = help_topic(
            "ST_MLineFromWKB(wkb [, srid [, options]]),\nST_MultiLineStringFromWKB(wkb [, srid [, options]])\n\nConstructs a MultiLineString value.\n",
        );
        assert_eq!(
            names_of(&sigs),
            ["ST_MLineFromWKB", "ST_MultiLineStringFromWKB"]
        );
        assert_eq!(desc, "Constructs a MultiLineString value.");
        let (sigs, _) = help_topic("LAG(expr [, N[, default]]) [null_treatment] over_clause\n\nReturns the value of expr.\n");
        assert_eq!(sigs, [("LAG".into(), "(expr [, N[, default]])".into())]);
        // A grammar rule after the syntax isn't the description; an
        // announcement of what follows loses its last sentence.
        let (_, desc) = help_topic(
            "Syntax:\nCAST(expr AS type [ARRAY])\n\ntimezone_specifier: [INTERVAL] '+00:00' | 'UTC'\n\nWith CAST(expr AS type syntax, the CAST() function converts.\n",
        );
        assert_eq!(
            desc,
            "With CAST(expr AS type syntax, the CAST() function converts."
        );
        let (_, desc) = help_topic(
            "Syntax:\nGROUP_CONCAT(expr)\n\nThis function returns a string. It returns NULL if there are no values.\nThe full syntax is as follows:\n\nGROUP_CONCAT([DISTINCT] expr)\n",
        );
        assert_eq!(
            desc,
            "This function returns a string. It returns NULL if there are no values."
        );
        // Operators and statements give none.
        for text in [
            "Syntax:\nexpr LIKE pat [ESCAPE 'escape_char']\n\nPattern matching.\n",
            "Syntax:\nvalue MEMBER OF(json_array)\n\nReturns true.\n",
            "Syntax:\nDIV\n\nInteger division.\n",
            "Syntax:\nCREATE FUNCTION function_name RETURNS type SONAME 'lib'\n\nLoads it.\n",
        ] {
            assert!(help_topic(text).0.is_empty(), "{text}");
        }
    }

    fn names_of(sigs: &[(String, String)]) -> Vec<&str> {
        sigs.iter().map(|(n, _)| n.as_str()).collect()
    }

    #[test]
    fn mariadb_help_topics_give_their_signatures_and_description() {
        let (sigs, desc) = help_topic(
            "MariaDB starting with 10.8\n--------------------------\nAdded in 10.8.\n\nSyntax\n------\n\n<= MariaDB 10.7\n\nCRC32(expr)\n\nFrom MariaDB 10.8\n\nCRC32([par,]expr)\n\nDescription\n-----------\n\nComputes a cyclic redundancy check value.\n\nExamples\n--------\n\nSELECT CRC32('MariaDB');\n",
        );
        assert_eq!(
            sigs,
            [
                ("CRC32".into(), "(expr)".into()),
                ("CRC32".into(), "([par,]expr)".into())
            ]
        );
        assert_eq!(desc, "Computes a cyclic redundancy check value.");
        let (sigs, _) = help_topic(
            "Syntax\n------\n\nCURRENT_TIME\nCURRENT_TIME([precision])\n\nDescription\n-----------\n\nSynonyms for CURTIME().\n",
        );
        assert_eq!(sigs, [("CURRENT_TIME".into(), "([precision])".into())]);
        let (sigs, _) = help_topic(
            "Syntax\n------\n\nLAG (expr[, offset]) OVER ( \n [ PARTITION BY partition_expression ]\n < ORDER BY order_list >\n)\n\nDescription\n-----------\n\nThe LAG function.\n",
        );
        assert_eq!(sigs, [("LAG".into(), "(expr[, offset])".into())]);
        // Placeholders in lower case, prose without a syntax, synonyms.
        for text in [
            "Syntax\n------\n\nfunction_name ([expression [, expression ... ]]) OVER (window_spec)\n\nDescription\n-----------\n\nWindow functions.\n",
            "Parentheses are sometimes called precedence operators: an\nexpression(that is, they cannot be empty).\n",
            "A synonym for ST_BUFFER.\n\nURL: https://mariadb.com/kb/en/buffer/\n",
        ] {
            assert!(help_topic(text).0.is_empty(), "{text}");
        }
    }

    #[test]
    fn help_rows_map_to_kinds() {
        let row = |c: &str, p: &str, g: &str, t: &str| [c, p, g, t].map(String::from);
        let got = from_help_rows(&[
            row("Aggregate Functions and Modifiers", "Functions", "", "Syntax:\nCOUNT(expr) [over_clause]\n\nReturns a count.\n"),
            row("Functions and Modifiers for Use with GROUP BY", "Contents", "", "Syntax:\nSUM([DISTINCT] expr)\n\nReturns the sum.\n"),
            row("Window Functions", "Functions", "", "ROW_NUMBER() over_clause\n\nReturns the number.\n"),
            row("MBR Functions", "Spatial Functions", "Functions", "Syntax:\nJSON_TABLE(expr, path COLUMNS (column_list) [AS] alias)\n\nExtracts data.\n"),
            row("JSON Functions", "Functions", "", "Syntax\n------\n\nJSON_ARRAYAGG(column_or_expression)\n\nDescription\n-----------\n\nAggregates.\n"),
            row("Internal Functions", "Functions", "", "Syntax:\nCAN_ACCESS_TABLE(db, t)\n\nInternal.\n"),
            row("String Functions", "Functions", "", "Syntax:\nUPPER(str)\n\nReturns the string str in upper case.\n"),
            row("String Functions", "Functions", "", "Syntax:\nUPPER(str)\n\nReturns the string str in upper case.\n"),
        ]);
        let kinds: Vec<(&str, FunctionKind)> =
            got.iter().map(|f| (f.name.as_str(), f.kind)).collect();
        assert_eq!(
            kinds,
            [
                ("COUNT", FunctionKind::Aggregate),
                ("SUM", FunctionKind::Aggregate),
                ("ROW_NUMBER", FunctionKind::Window),
                ("JSON_TABLE", FunctionKind::Table),
                ("JSON_ARRAYAGG", FunctionKind::Aggregate),
                ("UPPER", FunctionKind::Scalar),
            ]
        );
        assert_eq!(got[5].description, "Returns the string str in upper case.");
    }

    #[test]
    fn the_static_catalog_completes_the_server() {
        let server = vec![FunctionInfo::new("UPPER", FunctionKind::Scalar).description("x")];
        let got = complete(
            server,
            parse_catalog("s upper(str)\ns VEC_DISTANCE(a, b)", None),
        );
        assert_eq!(names(&got), ["UPPER", "VEC_DISTANCE"]);
        assert_eq!(got[0].description, "x");
        let alone = complete(Vec::new(), parse_catalog("s UPPER(str)", None));
        assert_eq!(names(&alone), ["UPPER"]);
    }

    #[test]
    fn engines_versions_are_their_own() {
        assert_eq!(
            engine_version(Variant::TiDb, "8.0.11-TiDB-v8.5.3"),
            "v8.5.3"
        );
        assert_eq!(
            engine_version(Variant::OceanBase, "5.7.25-OceanBase_CE-v4.2.1.0"),
            "4.2.1.0"
        );
        assert_eq!(
            engine_version(Variant::GreptimeDb, "8.4.2-GreptimeDB-1.2.1"),
            "1.2.1"
        );
        assert_eq!(engine_version(Variant::MySql, "8.4.11"), "8.4.11");
        // TiDB's vector functions came in 8.4.
        let tidb = |v: &str| {
            parse_catalog(TIDB, Some(&engine_version(Variant::TiDb, v)))
                .iter()
                .any(|f| f.name == "VEC_DIMS")
        };
        assert!(tidb("8.0.11-TiDB-v8.5.3"));
        assert!(!tidb("8.0.11-TiDB-v7.5.1"));
    }

    #[test]
    fn mysql_and_mariadb_catalogs_follow_the_version() {
        let has = |text: &str, v: &str, name: &str| {
            parse_catalog(text, Some(v)).iter().any(|f| f.name == name)
        };
        assert!(has(MYSQL, "8.0.36", "JSON_VALUE"));
        assert!(!has(MYSQL, "8.0.20", "JSON_VALUE"));
        assert!(!has(MYSQL, "5.7.44", "ROW_NUMBER"));
        assert!(has(MYSQL, "5.7.44", "PASSWORD"));
        assert!(!has(MYSQL, "8.0.36", "PASSWORD"));
        assert!(has(MARIADB, "11.8.3-MariaDB", "VEC_DISTANCE_EUCLIDEAN"));
        assert!(!has(MARIADB, "10.11.9-MariaDB", "VEC_DISTANCE_EUCLIDEAN"));
        assert!(has(MARIADB, "10.6.18-MariaDB", "JSON_TABLE"));
        assert!(!has(MARIADB, "10.5.25-MariaDB", "JSON_TABLE"));
        // JSON_APPEND stayed through 5.7 as a deprecated alias.
        assert!(has(MYSQL, "5.7.44", "JSON_APPEND"));
        assert!(!has(MYSQL, "8.0.36", "JSON_APPEND"));
        // WAIT_UNTIL_SQL_THREAD_AFTER_GTIDS went in 8.2.
        assert!(has(MYSQL, "8.0.36", "WAIT_UNTIL_SQL_THREAD_AFTER_GTIDS"));
        assert!(!has(MYSQL, "8.4.11", "WAIT_UNTIL_SQL_THREAD_AFTER_GTIDS"));
        // JSON_PRETTY, JSON_DETAILED's alias, came in 10.4.28.
        assert!(has(MARIADB, "10.11.9-MariaDB", "JSON_PRETTY"));
        assert!(!has(MARIADB, "10.3.39-MariaDB", "JSON_PRETTY"));
    }

    #[test]
    fn user_and_loadable_functions_map() {
        let f = routine(
            "ventas",
            "con_iva",
            "decimal(10,2)",
            " Con IVA ",
            "neto decimal(10,2)",
        )
        .unwrap();
        assert_eq!(
            (f.schema.as_str(), f.args.as_str(), f.returns.as_str()),
            ("ventas", "(neto decimal(10,2))", "decimal(10,2)")
        );
        assert_eq!(f.description, "Con IVA");
        assert_eq!(
            routine("ventas", "ahora", "datetime", "", "").unwrap().args,
            "()"
        );
        assert!(routine("ventas", "", "", "", "").is_none());
        let udf = loadable_function("median_udf", "1", "aggregate").unwrap();
        assert_eq!(
            (udf.kind, udf.returns.as_str()),
            (FunctionKind::Aggregate, "real")
        );
        // `RETURNS DECIMAL` is stored as 4 (`DECIMAL_RESULT`).
        let udf = loadable_function("x", "4", "function").unwrap();
        assert_eq!(
            (udf.kind, udf.returns.as_str()),
            (FunctionKind::Scalar, "decimal")
        );
        assert_eq!(
            loadable_function("x", "0", "function").unwrap().returns,
            "string"
        );
        assert_eq!(
            loadable_function("x", "2", "function").unwrap().returns,
            "integer"
        );
        let udf =
            loadable_function("innodb_redo_log_archive_start", "integer", "function").unwrap();
        assert_eq!(
            (udf.kind, udf.returns.as_str()),
            (FunctionKind::Scalar, "integer")
        );
        assert!(loadable_function("mysqlx_error", "char", "function").is_none());
    }

    #[test]
    fn tidb_builtins_are_named_like_tidb_documents_them() {
        let listed: Vec<String> = ["abs", "char_func", "eq", "and", "tidb_version", "brand_new"]
            .map(String::from)
            .into();
        let got = tidb_catalog(
            &listed,
            parse_catalog(
                "s ABS(X)\ns CHAR(N,...)\ns JSON_VALUE(j, p)\na COUNT(expr)\ns TIDB_VERSION()",
                None,
            ),
        );
        assert_eq!(
            names(&got),
            ["ABS", "CHAR", "COUNT", "TIDB_VERSION", "BRAND_NEW"]
        );
        assert_eq!(got[1].args, "(N,...)");
        assert_eq!(got[4].args, "");
    }

    #[test]
    fn olap_rows_map_and_collapse_per_type() {
        let cells = |sig: &str, ret: &str, ty: &str| {
            let (sig, ret, ty) = (sig.to_string(), ret.to_string(), ty.to_string());
            move |c: &str| match c {
                "Signature" => Some(sig.clone()),
                "Return Type" => Some(ret.clone()),
                "Function Type" => Some(ty.clone()),
                _ => None,
            }
        };
        let listed: Vec<FunctionInfo> = [
            cells("abs(DOUBLE)", "DOUBLE", "Scalar"),
            cells("abs(BIGINT)", "LARGEINT", "Scalar"),
            cells("lag(BIGINT,BIGINT)", "BIGINT", "Aggregate"),
            cells("unnest(INVALID_TYPE, ...)", "INVALID_TYPE", "Table"),
            cells("__iceberg_transform_day(DATE)", "INT", "Scalar"),
            // Doris 4: the name alone.
            cells("explode_split", "", ""),
            cells("brand_new", "", ""),
        ]
        .into_iter()
        .filter_map(|c| olap_entry(c, ""))
        .collect();
        assert_eq!(listed.len(), 6);
        let got = olap_catalog(
            listed,
            &parse_catalog("t explode_split(str, delimiter)", None),
        );
        let view: Vec<(&str, FunctionKind, &str, &str)> = got
            .iter()
            .map(|f| (f.name.as_str(), f.kind, f.args.as_str(), f.returns.as_str()))
            .collect();
        assert_eq!(
            view,
            [
                ("abs", FunctionKind::Scalar, "(DOUBLE)", "DOUBLE"),
                ("lag", FunctionKind::Window, "(BIGINT,BIGINT)", "BIGINT"),
                ("unnest", FunctionKind::Table, "(INVALID_TYPE, ...)", ""),
                ("explode_split", FunctionKind::Table, "(str, delimiter)", ""),
                ("brand_new", FunctionKind::Scalar, "", ""),
            ]
        );
        let user = olap_entry(
            cells("enmascarar(INT)", "VARCHAR(65533)", "Alias"),
            "ventas",
        )
        .unwrap();
        assert_eq!(
            (user.schema.as_str(), user.args.as_str()),
            ("ventas", "(INT)")
        );
    }

    #[test]
    fn databend_rows_map() {
        let row = |name: &'static str, agg: &'static str, syntax: &'static str| {
            move |c: &str| {
                Some(
                    match c {
                        "name" => name,
                        "is_aggregate" => agg,
                        "syntax" => syntax,
                        "description" => "Returns the absolute value.",
                        _ => return None,
                    }
                    .to_string(),
                )
            }
        };
        let abs = databend_entry(row("abs", "0", "ABS(<x>)")).unwrap();
        assert_eq!(
            (abs.kind, abs.args.as_str()),
            (FunctionKind::Scalar, "(<x>)")
        );
        assert_eq!(abs.description, "Returns the absolute value.");
        let sum = databend_entry(row("sum", "1", "")).unwrap();
        assert_eq!((sum.kind, sum.args.as_str()), (FunctionKind::Aggregate, ""));
        assert_eq!(databend_args(r#"{"parameters":["x","y"]}"#), "(x, y)");
        assert_eq!(databend_args(r#"{"arg_types":["Int32"]}"#), "(Int32)");
        assert_eq!(databend_args("not json"), "");
    }

    #[test]
    fn databend_drops_its_operators_and_takes_the_static_arguments() {
        let scalar = |n: &str| FunctionInfo::new(n, FunctionKind::Scalar);
        let listed = vec![
            scalar("abs"),
            scalar("eq"),
            scalar("and_filters"),
            scalar("like_any"),
            scalar("_internal"),
            scalar("brand_new"),
            scalar("abs"),
            FunctionInfo::new("sum", FunctionKind::Aggregate)
                .args("(<expr>)")
                .description("Sums."),
            FunctionInfo::new("count", FunctionKind::Aggregate).description("Counts."),
        ];
        let known = parse_catalog(
            "a count(expr)\na sum(expr)\ns abs(x)\nw row_number()\nt generate_series(start, stop [, step])",
            None,
        );
        let got = databend_catalog(listed, &known);
        let view: Vec<(&str, FunctionKind, &str, &str)> = got
            .iter()
            .map(|f| {
                (
                    f.name.as_str(),
                    f.kind,
                    f.args.as_str(),
                    f.description.as_str(),
                )
            })
            .collect();
        assert_eq!(
            view,
            [
                ("abs", FunctionKind::Scalar, "(x)", ""),
                ("brand_new", FunctionKind::Scalar, "", ""),
                ("sum", FunctionKind::Aggregate, "(<expr>)", "Sums."),
                ("count", FunctionKind::Aggregate, "(expr)", "Counts."),
                ("row_number", FunctionKind::Window, "()", ""),
                (
                    "generate_series",
                    FunctionKind::Table,
                    "(start, stop [, step])",
                    ""
                ),
            ]
        );
        // None of the names it drops is a function Databend documents.
        let documented: HashSet<String> = parse_catalog(DATABEND, None)
            .into_iter()
            .map(|f| f.name.to_ascii_lowercase())
            .collect();
        for op in DATABEND_OPERATORS {
            assert!(!documented.contains(*op), "{op}");
        }
    }

    #[test]
    fn static_kinds_match_the_engines() {
        let kind = |text: &str, v: Option<&str>, name: &str| {
            parse_catalog(text, v)
                .into_iter()
                .find(|f| f.name == name)
                .map(|f| f.kind)
        };
        assert_eq!(
            kind(MYSQL, Some("8.4.11"), "ST_Collect"),
            Some(FunctionKind::Aggregate)
        );
        // SingleStore's SPLIT and JSON_TO_ARRAY return an array, which
        // TABLE() turns into rows.
        assert_eq!(kind(SINGLESTORE, None, "SPLIT"), Some(FunctionKind::Scalar));
        assert_eq!(
            kind(SINGLESTORE, None, "JSON_TO_ARRAY"),
            Some(FunctionKind::Scalar)
        );
        assert_eq!(kind(SINGLESTORE, None, "TABLE"), Some(FunctionKind::Table));
    }

    #[test]
    fn spider_functions_come_only_from_mysql_func() {
        let got = from_help_rows(&[[
            "Spider Functions".to_string(),
            "Functions".to_string(),
            String::new(),
            "Syntax\n------\n\nSPIDER_DIRECT_SQL('sql', 'tmp_table_list', 'parameters')\n\nDescription\n-----------\n\nExecutes a statement.\n".to_string(),
        ]]);
        assert!(got.is_empty(), "{:?}", names(&got));
        assert!(!parse_catalog(MARIADB, None)
            .iter()
            .any(|f| f.name.starts_with("SPIDER_")));
    }
}
