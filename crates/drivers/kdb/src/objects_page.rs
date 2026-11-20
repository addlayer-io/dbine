//! The explorer's pages (`Session::list_objects_page`): a folder of the
//! tree, a name search or a lookup as one call of [`PAGE`] on the server.
//! It reads the namespaces [`crate::EXPLORER`] reads, but only the one asked
//! for (or the root), keeps the names asked for before telling what each one
//! is (which evaluates it), keeps the kinds asked for, sorts as the tree
//! shows them (the root first, then by namespace and name ignoring case,
//! then as written) and answers only the page: the rest of the catalog never
//! crosses the wire. The cursor is the offset, as `objects::page_of`'s.
//!
//! The filters travel as typed arguments ([`crate::ipc::apply`]), never as
//! q text; a search's `like` wildcards are escaped. Names fold with q's
//! `lower`, which folds only ASCII.

use crate::ipc::{Vector, K};
use crate::{PARTITIONED, SPLAYED, VARIABLE};
use dbine_driver::{kinds, DbObject, ObjectPage, ObjectQuery};

/// `[sc;ks;nm;pt;pg]`: the namespace (`"."` the root, `".ns"` one, `()`
/// any), the kinds as [`RAW`] names them, the name and the `like` pattern
/// (`()`: none) and `(offset; count)`. A table `([] ns; k; n)` like
/// [`crate::EXPLORER`]'s, sorted and cut.
pub const PAGE: &str = concat!(
    "{[sc;ks;nm;pt;pg] e:([] ns:`$(); k:`$(); n:`$());",
    "nsl:(enlist enlist \".\"),\".\",/:string (key `) except `q`Q`h`j`o`z`dbine_stage,`$\"\";",
    "if[10h=type sc; nsl:nsl where nsl~\\:sc];",
    // The names asked for, as a mask over `x`.
    "w:{[nm;pt;x] if[not count x; :`boolean$()]; l:lower string x; r:count[x]#1b;",
    "if[10h=type nm; r:r&l~\\:lower nm]; if[10h=type pt; r:r&l like lower pt]; r}[nm;pt];",
    "f:{[w;ks;e;ns] s:`$ns; bv:@[system;\"b \",ns;`$()]; ns0:(key s) except bv,`$\"\";",
    "ns0:ns0 where w ns0; bv:bv where w bv;",
    "kk:{$[100h<=type x;`function;@[.Q.qt;x;0b];$[1b~p:@[.Q.qp;x;0];`partitioned;0b~p;`splayed;`table];`variable]} each s@ns0;",
    "t:([] ns:(count[ns0]+count bv)#`$ns; k:(`$()),kk,count[bv]#`view; n:(`$()),ns0,bv);",
    "t where t[`k] in ks}[w;ks;e];",
    "t:e,raze {[f;e;ns] @[f;ns;{[e;x] e}[e]]}[f;e] each nsl;",
    "if[not count t; :t];",
    "u:t,'([] r:not t[`ns]=`.; ls:lower string t`ns; ln:lower string t`n; s:string t`ns; m:string t`n);",
    "pg sublist `ns`k`n#`r`ls`ln`s`m xasc u}"
);

/// [`crate::EXPLORER`]'s kinds and the explorer's, in `list_objects`'
/// order within a namespace. Anything else is a variable.
pub const RAW: &[(&str, &str)] = &[
    ("table", kinds::TABLE),
    ("partitioned", PARTITIONED),
    ("splayed", SPLAYED),
    ("view", kinds::VIEW),
    ("function", kinds::FUNCTION),
    ("variable", VARIABLE),
];

/// The explorer's kind of `raw` and its rank in `list_objects`' order.
pub fn kind(raw: &str) -> (usize, &'static str) {
    RAW.iter()
        .position(|(r, _)| *r == raw)
        .map_or((RAW.len() - 1, VARIABLE), |i| (i, RAW[i].1))
}

/// An object of [`crate::EXPLORER`]'s or [`PAGE`]'s table: a namespace's
/// are of that schema (`.ns`), the root's of none.
pub fn object(ns: &str, raw: &str, name: String) -> DbObject {
    DbObject {
        kind: kind(raw).1.into(),
        schema: (ns != "." && !ns.is_empty()).then(|| ns.to_string()),
        name,
        parent: None,
    }
}

/// Whether `q` keeps objects of `kind`.
fn wants(q: &ObjectQuery, kind: &str) -> bool {
    q.kind.as_deref().is_none_or(|k| k == kind) && !q.not_kinds.iter().any(|k| k == kind)
}

/// `s` as a `like` pattern matching it anywhere: its wildcards in brackets.
fn contains_pattern(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('*');
    for c in s.chars() {
        match c {
            '*' | '?' | '[' => {
                out.push('[');
                out.push(c);
                out.push(']');
            }
            _ => out.push(c),
        }
    }
    out.push('*');
    out
}

fn string(s: &str) -> K {
    K::Vector(0, Vector::Char(s.as_bytes().to_vec()))
}

/// The empty list: "no filter".
fn none() -> K {
    K::List(Vec::new())
}

/// A page's call: [`PAGE`]'s arguments and the slice of the sorted list it
/// asks for.
#[derive(Debug)]
pub struct PageQuery {
    pub args: Vec<K>,
    offset: usize,
    size: usize,
}

/// The call for page `q`, or `None` when no object can match (a schema
/// that no namespace can be, or no kind left).
pub fn page_query(q: &ObjectQuery) -> Option<PageQuery> {
    let scope = if q.no_schema {
        string(".")
    } else {
        match q.schema.as_deref() {
            // The root's objects have no schema.
            Some(s) if s.is_empty() || s == "." => return None,
            Some(s) => string(s),
            None => none(),
        }
    };
    let wanted: Vec<String> = RAW
        .iter()
        .filter(|(_, k)| wants(q, k))
        .map(|(r, _)| r.to_string())
        .collect();
    if wanted.is_empty() {
        return None;
    }
    let offset = q
        .after
        .as_deref()
        .and_then(|a| a.parse::<usize>().ok())
        .unwrap_or(0);
    let size = q.page_size();
    let args = vec![
        scope,
        K::Vector(0, Vector::Symbol(wanted)),
        q.name.as_deref().map_or_else(none, string),
        // Every name contains "" (and `like` matches nothing to "**").
        q.pattern
            .as_deref()
            .filter(|p| !p.is_empty())
            .map_or_else(none, |p| string(&contains_pattern(p))),
        K::Vector(
            0,
            Vector::Long(vec![
                i64::try_from(offset).unwrap_or(i64::MAX),
                i64::try_from(size.saturating_add(1)).unwrap_or(i64::MAX),
            ]),
        ),
    ];
    Some(PageQuery { args, offset, size })
}

impl PageQuery {
    /// The page out of [`PAGE`]'s table (one more row than the page when
    /// more remain).
    pub fn page(&self, k: K) -> ObjectPage {
        let K::Table(names, cols) = k else {
            return ObjectPage::default();
        };
        let col = |n: &str| {
            names
                .iter()
                .position(|c| c == n)
                .and_then(|i| cols[i].symbols())
                .unwrap_or_default()
        };
        let (ns, kind, name) = (col("ns"), col("k"), col("n"));
        let more = name.len() > self.size;
        let objects = ns
            .iter()
            .zip(&kind)
            .zip(name)
            .take(self.size)
            .map(|((ns, kind), name)| object(ns, kind, name))
            .collect();
        ObjectPage {
            objects,
            next: more.then(|| (self.offset + self.size).to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(k: &K) -> Option<String> {
        match k {
            K::Vector(_, Vector::Char(c)) => Some(String::from_utf8_lossy(c).into_owned()),
            _ => None,
        }
    }

    #[test]
    fn a_folder_sends_its_namespace_and_kind() {
        let q = ObjectQuery {
            schema: Some(".trade".into()),
            kind: Some(PARTITIONED.into()),
            limit: 10,
            after: Some("20".into()),
            ..Default::default()
        };
        let p = page_query(&q).unwrap();
        assert_eq!(text(&p.args[0]).as_deref(), Some(".trade"));
        assert_eq!(
            p.args[1],
            K::Vector(0, Vector::Symbol(vec!["partitioned".into()]))
        );
        assert_eq!(p.args[2], K::List(Vec::new()));
        assert_eq!(p.args[3], K::List(Vec::new()));
        assert_eq!(p.args[4], K::Vector(0, Vector::Long(vec![20, 11])));
        // The root: no schema.
        let q = ObjectQuery {
            no_schema: true,
            schema: Some(".trade".into()),
            not_kinds: vec!["table".into(), "variable".into()],
            ..Default::default()
        };
        let p = page_query(&q).unwrap();
        assert_eq!(text(&p.args[0]).as_deref(), Some("."));
        assert_eq!(
            p.args[1],
            K::Vector(
                0,
                Vector::Symbol(
                    ["partitioned", "splayed", "view", "function"]
                        .map(String::from)
                        .to_vec()
                )
            )
        );
        assert_eq!(p.args[4], K::Vector(0, Vector::Long(vec![0, 1001])));
    }

    #[test]
    fn user_text_travels_as_arguments_with_wildcards_escaped() {
        let q = ObjectQuery {
            name: Some("x\";exit 0;\"".into()),
            pattern: Some("a*b?[c]".into()),
            ..Default::default()
        };
        let p = page_query(&q).unwrap();
        assert_eq!(text(&p.args[2]).as_deref(), Some("x\";exit 0;\""));
        assert_eq!(text(&p.args[3]).as_deref(), Some("*a[*]b[?][[]c]*"));
        assert!(!PAGE.contains("exit"));
    }

    #[test]
    fn nothing_to_ask_for() {
        for s in ["", "."] {
            let q = ObjectQuery {
                schema: Some(s.into()),
                ..Default::default()
            };
            assert!(page_query(&q).is_none(), "{s:?}");
        }
        let q = ObjectQuery {
            not_kinds: RAW.iter().map(|(_, k)| k.to_string()).collect(),
            ..Default::default()
        };
        assert!(page_query(&q).is_none());
    }

    #[test]
    fn the_page_reads_the_table_and_the_cursor() {
        let q = ObjectQuery {
            limit: 2,
            after: Some("4".into()),
            ..Default::default()
        };
        let p = page_query(&q).unwrap();
        let sym =
            |v: &[&str]| K::Vector(0, Vector::Symbol(v.iter().map(|s| s.to_string()).collect()));
        let t = K::Table(
            vec!["ns".into(), "k".into(), "n".into()],
            vec![
                sym(&[".", ".fx", ".fx"]),
                sym(&["partitioned", "view", "lambda?"]),
                sym(&["trade", "v", "x"]),
            ],
        );
        let page = p.page(t);
        assert_eq!(
            page.objects,
            [
                DbObject {
                    kind: PARTITIONED.into(),
                    schema: None,
                    name: "trade".into(),
                    parent: None,
                },
                DbObject {
                    kind: kinds::VIEW.into(),
                    schema: Some(".fx".into()),
                    name: "v".into(),
                    parent: None,
                },
            ]
        );
        assert_eq!(page.next.as_deref(), Some("6"));
        assert_eq!(kind("lambda?"), (5, VARIABLE));
        assert_eq!(p.page(K::List(Vec::new())), ObjectPage::default());
    }
}
