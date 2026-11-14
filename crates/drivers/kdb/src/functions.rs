//! The function catalog (issue #97), read from the server in one request
//! ([`CATALOG`]):
//!
//! - q's keywords (`.Q.res`) that are functions, without q-sql's words and
//!   the control words, which the editor completes as keywords;
//! - the named functions of `.q` (`key .q`), without those in `.Q.res`;
//! - the user's functions, per namespace (`system "f"`), with their
//!   parameters (`(value f) 1`) and the namespace as the schema (`.u` for
//!   `.u.upd`; none in the root). kdb+'s own namespaces (`.q`, `.Q`, `.h`,
//!   `.j`, `.o`, `.s` (its SQL, `.s.init`), `.z`) and DBine's staging one
//!   are left out.
//!
//! Parameters are written as q applies them: `[x;y]`. When the server can't
//! be read, a static catalog of `.Q.res` and `.q` stands in.

use crate::ipc::K;
use dbine_driver::functions::parse_catalog;
use dbine_driver::{FunctionInfo, FunctionKind};

const STATIC: &str = include_str!("functions/q.txt");

/// One table, `([] ns; n; k; p)`: each function's namespace (`` ` `` for a
/// built-in, `` `. `` for the root), name, source (`` `res ``, `` `q ``,
/// `` `user ``) and parameters joined with `;` (empty when it has none to
/// show: a primitive, a projection).
pub const CATALOG: &str = concat!(
    "{skip:`select`exec`update`delete`from`by`do`if`while;",
    "r:.Q.res except skip;",
    "pa:{$[100h=type x;\";\" sv string (value x) 1;\"\"]};",
    "q:(key .q) except r,`;",
    "q:q where 100h<=type each .q q;",
    "t1:([] ns:count[r]#`; n:r; k:count[r]#`res; p:count[r]#enlist \"\");",
    "t2:([] ns:count[q]#`; n:q; k:count[q]#`q; p:pa each .q q);",
    "nsl:(enlist `.),`$\".\",/:string (key `) except `q`Q`h`j`o`s`z`dbine_stage,`$\"\";",
    "u:{[pa;ns] f:$[ns=`.;system \"f\";system \"f \",string ns];",
    "v:$[ns=`.;get each f;get each `$(string[ns],\".\"),/:string f];",
    "([] ns:count[f]#ns; n:f; k:count[f]#`user; p:pa each v)};",
    "t3:raze {[u;pa;ns] @[u[pa];ns;{([] ns:`$(); n:`$(); k:`$(); p:())}]}[u;pa] each nsl;",
    "t1,t2,t3}[]"
);

/// q's aggregates: they reduce a list to one value.
const AGGREGATES: &[&str] = &[
    "all", "any", "avg", "cor", "count", "cov", "dev", "first", "last", "max", "med", "min", "prd",
    "scov", "sdev", "sum", "svar", "var", "wavg", "wsum",
];

/// `.q`'s undocumented helpers (`ww` is `wj`'s).
const INTERNAL: &[&str] = &["ww"];

fn kind(name: &str, builtin: bool) -> FunctionKind {
    if builtin && AGGREGATES.contains(&name) {
        FunctionKind::Aggregate
    } else {
        FunctionKind::Scalar
    }
}

/// [`CATALOG`]'s answer as the catalog. Empty when it isn't that table.
pub fn from_server(k: &K) -> Vec<FunctionInfo> {
    let K::Table(names, cols) = k else {
        return Vec::new();
    };
    let col = |n: &str| names.iter().position(|c| c == n).map(|i| &cols[i]);
    let (Some(ns), Some(n), Some(src), Some(p)) = (col("ns"), col("n"), col("k"), col("p")) else {
        return Vec::new();
    };
    let (ns, n, src) = (
        ns.symbols().unwrap_or_default(),
        n.symbols().unwrap_or_default(),
        src.symbols().unwrap_or_default(),
    );
    let mut out = Vec::new();
    for (i, name) in n.iter().enumerate() {
        if name.is_empty() {
            continue;
        }
        let builtin = src.get(i).is_some_and(|s| s != "user");
        if builtin && INTERNAL.contains(&name.as_str()) {
            continue;
        }
        let params = p.item(i).and_then(|v| v.text()).unwrap_or_default();
        let space = ns.get(i).map(String::as_str).unwrap_or("");
        let schema = if builtin || space == "." { "" } else { space };
        out.push(
            FunctionInfo::new(name.clone(), kind(name, builtin))
                .args(if params.is_empty() {
                    String::new()
                } else {
                    format!("[{params}]")
                })
                .schema(schema),
        );
    }
    out
}

/// The static catalog, with the same kinds as the server's. Its lines are
/// `s name` or `s name[x;y]`: q's brackets, which [`parse_catalog`] keeps
/// in the name, are split off here.
pub fn static_catalog() -> Vec<FunctionInfo> {
    parse_catalog(STATIC, None)
        .into_iter()
        .map(|f| {
            let (name, args) = match f.name.find('[') {
                Some(i) => (f.name[..i].to_string(), f.name[i..].to_string()),
                None => (f.name.clone(), String::new()),
            };
            FunctionInfo {
                kind: kind(&name, true),
                name,
                args,
                ..f
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::Vector;

    #[test]
    fn the_static_catalog_parses() {
        for line in STATIC.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
        let c = static_catalog();
        assert_eq!(
            c.iter().find(|f| f.name == "avg").unwrap().kind,
            FunctionKind::Aggregate
        );
        let xbar = c.iter().find(|f| f.name == "xbar").unwrap();
        assert_eq!(xbar.args, "[x;y]");
        assert_eq!(c.iter().find(|f| f.name == "abs").unwrap().args, "");
        assert!(!c.iter().any(|f| f.name == "select" || f.name.contains('[')));
    }

    fn syms(v: &[&str]) -> K {
        K::Vector(0, Vector::Symbol(v.iter().map(|s| s.to_string()).collect()))
    }

    fn strs(v: &[&str]) -> K {
        K::List(
            v.iter()
                .map(|s| K::Vector(0, Vector::Char(s.as_bytes().to_vec())))
                .collect(),
        )
    }

    #[test]
    fn server_rows_map_to_entries() {
        let k = K::Table(
            vec!["ns".into(), "n".into(), "k".into(), "p".into()],
            vec![
                syms(&["", "", "", ".", ".u", ".u", ""]),
                syms(&["avg", "xbar", "neg", "vwap", "upd", "end", "ww"]),
                syms(&["res", "q", "q", "user", "user", "user", "q"]),
                strs(&["", "x;y", "", "t;s", "t;x", "x", "a;w;f;y;z"]),
            ],
        );
        let c = from_server(&k);
        assert_eq!(c.len(), 6);
        assert_eq!(
            (c[0].kind, c[0].args.as_str()),
            (FunctionKind::Aggregate, "")
        );
        assert_eq!(
            (c[1].kind, c[1].args.as_str()),
            (FunctionKind::Scalar, "[x;y]")
        );
        assert_eq!(c[2].args, "");
        assert_eq!((c[3].name.as_str(), c[3].schema.as_str()), ("vwap", ""));
        assert_eq!(c[3].args, "[t;s]");
        assert_eq!((c[4].name.as_str(), c[4].schema.as_str()), ("upd", ".u"));
        assert_eq!(c[5].args, "[x]");
        // A user's function named like an aggregate is a plain function.
        let user_avg = K::Table(
            vec!["ns".into(), "n".into(), "k".into(), "p".into()],
            vec![
                syms(&[".my"]),
                syms(&["avg"]),
                syms(&["user"]),
                strs(&["x"]),
            ],
        );
        assert_eq!(from_server(&user_avg)[0].kind, FunctionKind::Scalar);
        assert!(from_server(&K::Nil).is_empty());
    }
}
