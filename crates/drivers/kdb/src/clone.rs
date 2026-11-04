//! Same-engine clone ("clonar") between kdb+ processes, as a
//! [`CloneScript`]: the tables in memory with their columns, types and key
//! (the designer's DDL), their attributes and links, and the process's
//! functions and views.
//!
//! The source is read in two requests (inside `reval` on its read-only
//! connection): the explorer's list, then a catalog of the chosen tables
//! (`meta`, `keys`, `.Q.qp`), every function's value and every root view's
//! definition. Nothing runs on the target here. The script, in order:
//!
//! 1. per table: `create`, the designer's `t:([k:`long$()] c:`symbol$())`
//!    only when nothing has that name yet (so it's safe to run again on a
//!    resume); its data (the migration's typed read and bulk load); then
//!    `after_data`: its attributes (`` `s# `` `` `u# `` `` `p# `` `` `g# ``),
//!    once the rows are in their order.
//! 2. `after`: linked columns linked again (`` `t2$c `` over a key of one
//!    column, `` `t2!c `` over a compound one, whose cells come as row
//!    indices), each function defined from its source in its own namespace
//!    (`parse`, refused unless it's one lambda), then the root's views
//!    (`v::…`, refused unless it's one view).
//!
//! What a kdb+ target can't get identical is left out and said in the
//! notes: tables on disk (partitioned or splayed: their columns are files
//! DBine doesn't write), views outside the root namespace (over IPC kdb+
//! defines a view only in the root), functions without source text of
//! their own (projections, compositions), links to tables not cloned, and
//! the variables that aren't tables, views or functions.

use crate::edit::{plain, qstr, qsym, table_ddl};
use crate::ipc::K;
use crate::read::{fresh, read, symbols};
use crate::sync::{column_name, IN_MEMORY};
use crate::{explorer_objects, KdbSession, EXPLORER, VARIABLE};
use dbine_driver::transfer::{CloneScript, CloneTable};
use dbine_driver::{
    kinds, ColumnDef, DdlParts, Error, KeyDef, ObjectRef, Result, Session, TableSchema,
};

/// `[ts;fu;vw]`: per table `(kind; columns; meta letters; linked tables;
/// attributes; key)` (`` ,`missing `` for one that can't be read), each
/// function's value and each root view's definition.
pub const CATALOG: &str = "{[ts;fu;vw] (({@[{q:@[.Q.qp;get x;0]; m:0!meta x; ($[1b~q;`partitioned;0b~q;`splayed;`table]; m`c; m`t; m`f; m`a; @[keys;x;`$()])};x;{[e] enlist `missing}]} each ts); get each fu; view each vw)}";

/// What a function's or a view's statement signals when the source's text
/// isn't one (it never runs).
macro_rules! not_code {
    () => {
        "'\"el texto del origen no es una función ni una vista de q\""
    };
}

/// `[n;c;s]`: function `n` defined from its source `s`, parsed in its
/// namespace `c` (`.`: the root) so its globals resolve as on the source.
pub const FUNCTION: &str = concat!(
    "{[n;c;s] system \"d \",c; p:@[parse;s;{x}]; system \"d .\"; if[not 100h=type p; ",
    not_code!(),
    "]; n set p}"
);

/// `[n;d]`: root view `n` defined as `d`.
pub const VIEW: &str = concat!(
    "{[n;d] s:(string n),\"::\",d; p:parse s; if[not (3=count p)&((::)~first p)&n~p 1; ",
    not_code!(),
    "]; value s}"
);

/// `[t;c;l]`: column `c` of table `t` linked again to keyed table `l`, by
/// its values (a key of one column) or its row indices (`!`, a compound
/// one); left alone when it's linked already.
const LINK_BY_VALUE: &str = "{[t;c;l] if[not (type (0!get t) c) within 20 76h; ![t;();0b;(enlist c)!enlist ($;enlist l;c)]]}";
const LINK_BY_INDEX: &str = "{[t;c;l] if[not (type (0!get t) c) within 20 76h; ![t;();0b;(enlist c)!enlist (!;enlist l;c)]]}";

const CREATE: DdlParts = DdlParts {
    drop: false,
    if_exists: false,
    create: true,
    indexes: false,
    foreign_keys: false,
};

/// Entry point of [`dbine_driver::Driver::clone_script`].
pub(crate) async fn script(source: &mut dyn Session, tables: &[ObjectRef]) -> Result<CloneScript> {
    let Some(src) = source.as_any().and_then(|a| a.downcast_mut::<KdbSession>()) else {
        return Err(Error::Unsupported(
            "clonar necesita dos sesiones de KDB+".into(),
        ));
    };
    fresh(src);
    let objects = explorer_objects(read(src, EXPLORER).await?);
    let of = |kind: &str| -> Vec<String> {
        objects
            .iter()
            .filter(|o| o.kind == kind)
            .map(|o| o.name.clone())
            .collect()
    };
    let functions = of(kinds::FUNCTION);
    let (views, ns_views): (Vec<String>, Vec<String>) =
        of(kinds::VIEW).into_iter().partition(|v| root_name(v));
    let variables = of(VARIABLE);
    let names: Vec<String> = tables.iter().map(|t| t.name.clone()).collect();
    let catalog = read(
        src,
        &format!(
            "{CATALOG}[{};{};{}]",
            symbols(&names),
            symbols(&functions),
            symbols(&views)
        ),
    )
    .await?;
    Ok(plan(&Source {
        tables: names,
        catalog,
        functions,
        views,
        ns_views,
        variables,
    }))
}

/// A name of the root namespace a view can have.
fn root_name(n: &str) -> bool {
    plain(n) && !n.contains('.')
}

/// What was read from the source.
struct Source {
    tables: Vec<String>,
    catalog: K,
    functions: Vec<String>,
    views: Vec<String>,
    ns_views: Vec<String>,
    variables: Vec<String>,
}

/// A table's entry of [`CATALOG`].
struct Info {
    kind: String,
    /// Name, `meta` letter, linked table, attribute.
    columns: Vec<(String, char, String, String)>,
    key: Vec<String>,
}

fn info(k: Option<K>) -> Option<Info> {
    let k = k?;
    let kind = k.item(0)?.text()?;
    if kind == "missing" {
        return Some(Info {
            kind,
            columns: Vec::new(),
            key: Vec::new(),
        });
    }
    let names = k.item(1)?.symbols()?;
    let letters = k.item(2)?;
    let links = k.item(3).and_then(|l| l.symbols()).unwrap_or_default();
    let attrs = k.item(4).and_then(|a| a.symbols()).unwrap_or_default();
    let key = k.item(5).and_then(|a| a.symbols()).unwrap_or_default();
    let columns = names
        .into_iter()
        .enumerate()
        .map(|(i, n)| {
            let letter = match letters.item(i) {
                Some(K::Char(c)) => c as char,
                _ => ' ',
            };
            (
                n,
                letter,
                links.get(i).cloned().unwrap_or_default(),
                attrs.get(i).cloned().unwrap_or_default(),
            )
        })
        .collect();
    Some(Info { kind, columns, key })
}

/// The table's statements: its create, and its attributes after its data.
fn table(name: &str, info: &Info) -> Result<CloneTable> {
    for (c, ..) in &info.columns {
        column_name(c)?;
    }
    for k in &info.key {
        column_name(k)?;
    }
    let schema = TableSchema {
        kind: kinds::TABLE.into(),
        name: name.to_string(),
        columns: info
            .columns
            .iter()
            .map(|(c, letter, ..)| ColumnDef {
                name: c.clone(),
                data_type: crate::cells::meta_type(*letter),
                ..Default::default()
            })
            .collect(),
        primary_key: (!info.key.is_empty()).then(|| KeyDef {
            name: None,
            columns: info.key.clone(),
        }),
        ..Default::default()
    };
    let create = format!(
        "if[not @[{{get x;1b}};{};0b];{}]",
        qsym(name),
        table_ddl(&schema, CREATE)?
    );
    let attrs: Vec<(&str, &str)> = info
        .columns
        .iter()
        .filter(|(.., a)| matches!(a.as_str(), "s" | "u" | "p" | "g"))
        .map(|(c, .., a)| (c.as_str(), a.as_str()))
        .collect();
    let after_data = if attrs.is_empty() {
        Vec::new()
    } else {
        let tree = |(c, a): &(&str, &str)| format!("(#;enlist `{a};`{c})");
        let dict = match attrs.as_slice() {
            [one] => format!("(enlist `{})!enlist {}", one.0, tree(one)),
            many => format!(
                "{}!({})",
                many.iter()
                    .map(|(c, _)| format!("`{c}"))
                    .collect::<String>(),
                many.iter().map(tree).collect::<Vec<_>>().join(";")
            ),
        };
        vec![format!("![{};();0b;{dict}]", qsym(name))]
    };
    Ok(CloneTable {
        table: ObjectRef {
            kind: kinds::TABLE.into(),
            schema: None,
            name: name.to_string(),
        },
        create,
        before_data: Vec::new(),
        after_data,
    })
}

/// A list of names for a note (the first ten).
fn listed(names: &[String]) -> String {
    let mut s = names
        .iter()
        .take(10)
        .map(|n| format!("«{n}»"))
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > 10 {
        s.push_str(", …");
    }
    s
}

fn plan(src: &Source) -> CloneScript {
    let mut out = CloneScript::default();
    let infos: Vec<Option<Info>> = (0..src.tables.len())
        .map(|i| info(src.catalog.item(0).and_then(|t| t.item(i))))
        .collect();
    // The tables cloned, and the key each one has.
    let mut cloned: Vec<(&str, usize)> = Vec::new();
    for (name, info) in src.tables.iter().zip(&infos) {
        let Some(info) = info.as_ref().filter(|i| i.kind != "missing") else {
            out.notes.push(format!(
                "«{name}» no se clona: no se pudo leer en el origen"
            ));
            continue;
        };
        if info.kind != "table" {
            out.notes.push(format!(
                "«{name}» es una tabla en disco (particionada o splayed) y no se clona: DBine no escribe los archivos de una base en disco. Copiala con q en el servidor (por ejemplo con .Q.dpft)."
            ));
            continue;
        }
        match table(name, info) {
            Ok(t) => {
                out.tables.push(t);
                cloned.push((name.as_str(), info.key.len()));
            }
            Err(e) => out.notes.push(format!("«{name}» no se clona: {e}")),
        }
    }

    // Linked columns, once every table has its rows.
    for (name, info) in src.tables.iter().zip(&infos) {
        let Some(info) = info.as_ref() else { continue };
        if !cloned.iter().any(|(t, _)| t == name) {
            continue;
        }
        for (c, _, link, _) in info.columns.iter().filter(|c| !c.2.is_empty()) {
            match cloned.iter().find(|(t, _)| t == link) {
                Some((_, keys)) => {
                    let f = if *keys > 1 {
                        LINK_BY_INDEX
                    } else {
                        LINK_BY_VALUE
                    };
                    out.after
                        .push(format!("{f}[{};`{c};{}]", qsym(name), qsym(link)));
                }
                None => out.notes.push(format!(
                    "{name}.{c} queda sin enlazar a «{link}»: esa tabla no se clona."
                )),
            }
        }
    }

    // Functions, from their own source and in their own namespace.
    let values = src.catalog.item(1);
    for (i, f) in src.functions.iter().enumerate() {
        match values.as_ref().and_then(|v| v.item(i)) {
            Some(K::Lambda(ctx, text)) => {
                let space = if ctx.is_empty() {
                    ".".to_string()
                } else {
                    format!(".{ctx}")
                };
                if space != "." && !plain(&space) {
                    out.notes.push(format!(
                        "«{f}» no se clona: es una proyección o una composición, sin texto de función propio."
                    ));
                    continue;
                }
                out.after.push(format!(
                    "{FUNCTION}[{};{};{}]",
                    qsym(f),
                    qstr(&space),
                    qstr(&text)
                ));
            }
            _ => out.notes.push(format!(
                "«{f}» no se clona: es una proyección o una composición, sin texto de función propio."
            )),
        }
    }

    // The root's views.
    let defs = src.catalog.item(2);
    for (i, v) in src.views.iter().enumerate() {
        if let Some(d) = defs.as_ref().and_then(|d| d.item(i)).and_then(|d| d.text()) {
            out.after.push(format!("{VIEW}[`{v};{}]", qstr(&d)));
        }
    }
    if !src.ns_views.is_empty() {
        out.notes.push(format!(
            "Las vistas {} no se clonan: kdb+ define una vista fuera del espacio de nombres raíz solo desde un script (con \\d).",
            listed(&src.ns_views)
        ));
    }
    if !src.variables.is_empty() {
        out.notes.push(format!(
            "Las variables que no son tablas, vistas ni funciones no se clonan: {}.",
            listed(&src.variables)
        ));
    }
    if !out.tables.is_empty() {
        out.notes.push(IN_MEMORY.into());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::Vector;

    fn syms(v: &[&str]) -> K {
        K::Vector(0, Vector::Symbol(v.iter().map(|s| s.to_string()).collect()))
    }

    fn entry(
        kind: &str,
        cols: &[&str],
        letters: &str,
        links: &[&str],
        attrs: &[&str],
        key: &[&str],
    ) -> K {
        K::List(vec![
            K::Symbol(kind.into()),
            syms(cols),
            K::Vector(0, Vector::Char(letters.as_bytes().to_vec())),
            syms(links),
            syms(attrs),
            syms(key),
        ])
    }

    fn source() -> Source {
        Source {
            tables: vec![
                "trade".into(),
                "ref".into(),
                "hdb".into(),
                "odd".into(),
                "gone".into(),
            ],
            catalog: K::List(vec![
                K::List(vec![
                    entry(
                        "table",
                        &["id", "sym", "r", "x", "note"],
                        "jsjf ",
                        &["", "", "ref", "", ""],
                        &["u", "g", "", "", ""],
                        &["id"],
                    ),
                    entry("table", &["k", "v"], "js", &["", ""], &["", ""], &["k"]),
                    entry(
                        "partitioned",
                        &["date", "px"],
                        "df",
                        &["", ""],
                        &["", ""],
                        &[],
                    ),
                    entry("table", &["a b"], "j", &[""], &[""], &[]),
                    K::Vector(0, Vector::Symbol(vec!["missing".into()])),
                ]),
                K::List(vec![
                    K::Lambda(String::new(), "{x+1}".into()),
                    K::Lambda("ns".into(), "{count t}".into()),
                    K::Function("{x+y}[1]".into()),
                ]),
                K::List(vec![K::Vector(
                    0,
                    Vector::Char(b"select from trade where x>1".to_vec()),
                )]),
            ]),
            functions: vec!["f".into(), ".ns.g".into(), "p".into()],
            views: vec!["v".into()],
            ns_views: vec![".ns.w".into()],
            variables: vec!["sym".into()],
        }
    }

    #[test]
    fn tables_are_created_only_when_absent_with_their_key_and_types() {
        let s = plan(&source());
        let names: Vec<&str> = s.tables.iter().map(|t| t.table.name.as_str()).collect();
        assert_eq!(names, ["trade", "ref"]);
        assert_eq!(
            s.tables[0].create,
            "if[not @[{get x;1b};`trade;0b];trade:([id:`long$()] sym:`symbol$(); r:`long$(); x:`float$(); note:())]"
        );
        assert_eq!(
            s.tables[0].after_data,
            ["![`trade;();0b;`id`sym!((#;enlist `u;`id);(#;enlist `g;`sym))]"]
        );
        assert!(s.tables[1].after_data.is_empty());
    }

    #[test]
    fn links_functions_and_views_come_after_the_data() {
        let s = plan(&source());
        assert_eq!(s.after[0], format!("{LINK_BY_VALUE}[`trade;`r;`ref]"));
        assert_eq!(s.after[1], format!("{FUNCTION}[`f;\".\";\"{{x+1}}\"]"));
        assert_eq!(
            s.after[2],
            format!("{FUNCTION}[`.ns.g;\".ns\";\"{{count t}}\"]")
        );
        assert_eq!(
            s.after[3],
            format!("{VIEW}[`v;\"select from trade where x>1\"]")
        );
        assert_eq!(s.after.len(), 4);
    }

    #[test]
    fn what_cant_be_identical_is_said() {
        let s = plan(&source());
        let notes = s.notes.join("\n");
        assert!(notes.contains("«hdb» es una tabla en disco"), "{notes}");
        assert!(
            notes.contains("«odd» no se clona: «a b» no es un nombre de columna"),
            "{notes}"
        );
        assert!(
            notes.contains("«gone» no se clona: no se pudo leer"),
            "{notes}"
        );
        assert!(
            notes.contains("«p» no se clona: es una proyección"),
            "{notes}"
        );
        assert!(notes.contains("Las vistas «.ns.w» no se clonan"), "{notes}");
        assert!(notes.contains("no se clonan: «sym»."), "{notes}");
        assert_eq!(s.notes.last().map(String::as_str), Some(IN_MEMORY));
        // A link to a table that isn't cloned is said, not run.
        let mut src = source();
        src.tables.remove(1);
        if let K::List(parts) = &mut src.catalog {
            if let K::List(t) = &mut parts[0] {
                t.remove(1);
            }
        }
        let s = plan(&src);
        assert!(!s.after.iter().any(|a| a.contains("`ref]")));
        assert!(s
            .notes
            .iter()
            .any(|n| n.contains("trade.r queda sin enlazar a «ref»")));
    }

    #[test]
    fn a_link_over_a_compound_key_goes_by_index() {
        let mut src = source();
        if let K::List(parts) = &mut src.catalog {
            if let K::List(t) = &mut parts[0] {
                t[1] = entry(
                    "table",
                    &["k", "j", "v"],
                    "jjs",
                    &["", "", ""],
                    &["", "", ""],
                    &["k", "j"],
                );
            }
        }
        let s = plan(&src);
        assert_eq!(s.after[0], format!("{LINK_BY_INDEX}[`trade;`r;`ref]"));
    }

    #[test]
    fn source_text_is_checked_on_the_target_before_it_is_code() {
        // A function only from one lambda; a view only from one `v::…`.
        assert!(FUNCTION.contains("if[not 100h=type p;"));
        assert!(VIEW.contains("(3=count p)&((::)~first p)&n~p 1"));
        assert!(root_name("v") && !root_name(".ns.v") && !root_name("a b"));
        assert_eq!(
            listed(&(0..12).map(|i| i.to_string()).collect::<Vec<_>>())
                .matches('«')
                .count(),
            10
        );
    }
}
