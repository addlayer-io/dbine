//! "Renombrar…": in kdb+ every object is a value under a name, so a rename
//! copies the value to the new name and deletes the old one, in a single
//! statement (one request) whose checks run on the server first: the old
//! object is still there and of its kind, the new name is free, nothing on
//! disk. A race can't half-apply it, and nothing is replaced silently.
//!
//! - A table in memory (`.ns.u:.ns.t`) and its columns (`xcol`).
//! - A function or another variable, within its namespace: a function
//!   resolves its globals in the namespace it was defined in, wherever it
//!   is copied, so it never moves to another one.
//! - A view, in the root only (over IPC kdb+ defines a view only there):
//!   its definition is read on the server (`view`), defined under the new
//!   name as the clone does ([`crate::clone::VIEW`]) and the old one
//!   deleted.
//! - A top-level namespace (not kdb+'s own nor DBine's): its dictionary
//!   copied to the new name, the old one deleted. Refused when it holds
//!   views or tables on disk, or what kdb+ binds to it by name: functions
//!   (they would keep resolving their globals in the old namespace, which
//!   no longer exists) and columns linked to its tables (an enumeration
//!   keeps its domain's name), inside it or outside.
//!
//! q code that names the renamed object is listed, never rewritten
//! (`ReferenceStyle::None`).

use crate::clone::VIEW;
use crate::edit::{plain, qref, qstr, qsym};
use crate::sync::{assign, column_name, exists_message, on_disk_guard, refuse_on_disk, IN_MEMORY};
use crate::VARIABLE;
use dbine_driver::rename::ReferenceStyle;
use dbine_driver::{kinds, Error, RenameRequest, RenameSpec, RenameTarget, Result, SyncScript};

/// What the dialog says first.
pub const NOTE: &str = "kdb+ renombra copiando el valor al nombre nuevo y borrando el viejo, en un solo pedido que antes revisa que el nombre nuevo esté libre. Una función o variable se renombra dentro de su espacio de nombres (una función busca sus variables en el espacio de nombres donde se definió) y una vista, solo en el raíz. Un espacio de nombres se renombra si no tiene funciones, vistas, tablas en disco ni columnas enlazadas a sus tablas. El código q que nombra lo renombrado se lista, no se reescribe.";

/// Said with a rename of code or of a namespace.
pub const IN_PROCESS: &str = "kdb+ cambia el proceso en memoria: si lo renombrado se define en un script q que el proceso carga al arrancar, cambialo también ahí.";

/// `[f;p;x]`: value `x` holds a function (when `f`) or a vector or column
/// enumerated over a domain named like `p` (`.ns.*`), at any depth of its
/// dictionaries, tables and general lists. Vectors of simple types are
/// skipped without looking at their items.
pub const BOUND: &str = "{[f;p;x] $[(::)~x;0b;100h<=t:type x;f;(abs t) within 20 76h;(string key x) like p;98h=t;any .z.s[f;p] each value flip x;99h=t;any .z.s[f;p] each (key x;value x);0h=t;any .z.s[f;p] each x where not (abs type each x) within 1 19;0b]}";

/// `[w;o;n;m]`: namespace `o` renamed `n` ([`BOUND`] is `w`; `m` the two
/// refusals, each followed by the first five names that cause it: kdb+
/// cuts what it signals at 254 bytes, so they are short and the names go
/// last): refused when it holds views
/// or tables on disk (`m 0`), or functions, or columns linked to its tables
/// inside it or in the root and the other namespaces (`m 1`); otherwise its
/// dictionary is copied and the old one deleted. Views are never read as
/// values (that would compute them).
pub const NAMESPACE: &str = concat!(
    "{[w;o;n;m] l:{(\", \" sv string 5 sublist x),$[5<count x;\", …\";\"\"]}; ",
    "vw:@[system;\"b \",string o;`$()]; d:get o; k:(key d) except `,vw; v:d k; q:` sv/:o,/:k; ",
    "b:(` sv/:o,/:vw),q where not 0~/:@[.Q.qp;;0] each v; if[count b;'(m 0),l b]; ",
    "p:(string o),\".*\"; x:q where w[1b;p] each v; ",
    "r:(key `) except `q`Q`h`j`o`z`dbine_stage,(`$\"\"),`$1_string o; ",
    "x,:raze {[w;p;s] d:@[get;s;0]; if[not 99h=type d;:`$()]; k:(key d) except `,@[system;\"b \",string s;`$()]; ",
    "k:k where {[w;p;v] $[0~@[.Q.qp;v;0];w[0b;p] v;any @[{exec f from meta x};v;`$()] like p]}[w;p] each d k; ",
    "$[s~`.;k;` sv/:s,/:k]}[w;p] each `.,` sv/:`,/:r; ",
    "if[count x;'(m 1),l x]; n set d; ![`;();0b;enlist `$1_string o]}"
);

/// What [`crate::KdbDriver::rename_spec`] answers.
pub fn spec() -> RenameSpec {
    RenameSpec {
        kinds: vec![
            kinds::TABLE.into(),
            kinds::VIEW.into(),
            kinds::FUNCTION.into(),
            VARIABLE.into(),
        ],
        columns: true,
        schemas: true,
        references: ReferenceStyle::None,
        note: Some(NOTE.into()),
        grants_on_objects: false,
        ..RenameSpec::default()
    }
}

fn changed_message(name: &str) -> String {
    format!("«{name}» no existe o cambió: actualizá el explorador.")
}

fn not_a_name(name: &str) -> Error {
    Error::Unsupported(format!(
        "«{name}» no es un nombre de q: empieza con una letra y sigue con letras, dígitos y _."
    ))
}

/// `name` is a name of one part q can write (`f`, not `.ns.f` nor `a b`).
fn base_name(name: &str) -> Result<()> {
    if plain(name) && !name.contains('.') {
        Ok(())
    } else {
        Err(not_a_name(name))
    }
}

/// A q name's namespace and its own name: `.ns.f` → (`.ns`, `f`), `.a.b.f`
/// → (`.a.b`, `f`), a root name → (`.`, as is). The namespace must be a
/// q name.
fn parent(q: &str) -> Result<(String, &str)> {
    match q.rfind('.') {
        Some(i) if i > 0 && q.starts_with('.') => {
            let ns = crate::names::namespace(Some(&q[..i]))?.unwrap_or_else(|| ".".into());
            Ok((ns, &q[i + 1..]))
        }
        _ => Ok((".".into(), q)),
    }
}

/// "Renombrar…", on q names ([`crate::names::resolved_rename`]).
pub fn script(req: &RenameRequest) -> Result<SyncScript> {
    let new = req.new_name.as_str();
    let (statement, warning) = match &req.target {
        RenameTarget::Column { table, column } => {
            refuse_on_disk(&table.kind, &table.name)?;
            (column_rename(&table.name, column, new)?, IN_MEMORY)
        }
        RenameTarget::Object { object, .. } => match object.kind.as_str() {
            kinds::TABLE => (value_rename(&object.name, new, false)?, IN_MEMORY),
            kinds::FUNCTION => (value_rename(&object.name, new, true)?, IN_PROCESS),
            VARIABLE => (value_rename(&object.name, new, true)?, IN_PROCESS),
            kinds::VIEW => (view_rename(&object.name, new)?, IN_PROCESS),
            k => {
                refuse_on_disk(k, &object.name)?;
                return Err(Error::Unsupported("este motor no renombra objetos".into()));
            }
        },
        RenameTarget::Schema { schema, .. } => {
            return Ok(SyncScript {
                statements: vec![namespace_rename(schema, new)?],
                warnings: vec![IN_MEMORY.into(), IN_PROCESS.into()],
            })
        }
        _ => return Err(Error::Unsupported("este motor no renombra objetos".into())),
    };
    Ok(SyncScript {
        statements: vec![statement],
        warnings: vec![warning.into()],
    })
}

/// A table's column (`xcol`), if the table isn't on disk and has no
/// column called `new` yet.
fn column_rename(table: &str, column: &str, new: &str) -> Result<String> {
    let (old, new) = (column_name(column)?, column_name(new)?);
    let r = qref(table);
    Ok([
        on_disk_guard(table),
        format!("if[`{new} in cols {r};'{}]", qstr(&exists_message(new))),
        assign(
            table,
            &format!("@[cols {r};where cols[{r}]=`{old};:;`{new}] xcol {r}"),
        ),
    ]
    .join(";"))
}

/// A table in memory, a function or a variable: the old one is in its
/// namespace and isn't a view, isn't on disk, the new name is free; then
/// the value is copied and the old name deleted. `same_space`: functions
/// and variables stay in their namespace (a table may move).
fn value_rename(old: &str, new: &str, same_space: bool) -> Result<String> {
    let (os, ob) = parent(old)?;
    let (ns, nb) = parent(new)?;
    if same_space {
        if ns != os {
            return Err(Error::Unsupported(format!(
                "«{new}» está en otro espacio de nombres: una función busca sus variables en el espacio de nombres donde se definió, así que DBine renombra funciones y variables dentro del suyo (escribí solo el nombre nuevo)."
            )));
        }
        base_name(nb)?;
    }
    let ob = qsym(ob);
    Ok([
        format!(
            "if[(not {ob} in key `{os})|{ob} in @[system;{};`$()];'{}]",
            qstr(&format!("b {os}")),
            qstr(&changed_message(old))
        ),
        on_disk_guard(old),
        format!(
            "if[{} in key `{ns};'{}]",
            qsym(nb),
            qstr(&exists_message(new))
        ),
        assign(new, &qref(old)),
        format!("![`{os};();0b;enlist {ob}]"),
    ]
    .join(";"))
}

/// A root view: still one, the new name free, its definition read on the
/// server and defined under the new name (checked as one view), the old
/// one deleted.
fn view_rename(old: &str, new: &str) -> Result<String> {
    for n in [old, new] {
        if n.starts_with('.') {
            return Err(Error::Unsupported(format!(
                "«{n}»: kdb+ define una vista solo en el espacio de nombres raíz (fuera de él, solo desde un script con \\d), así que DBine renombra una vista del raíz a otro nombre del raíz."
            )));
        }
        base_name(n)?;
    }
    Ok(format!(
        "if[not `{old} in system \"b .\";'{}];if[`{new} in key `.;'{}];{VIEW}[`{new};view `{old}];![`.;();0b;enlist `{old}]",
        qstr(&changed_message(old)),
        qstr(&exists_message(new))
    ))
}

/// A top-level namespace ([`NAMESPACE`]), if it is one and the new name
/// (top-level too) is free.
fn namespace_rename(old: &str, new: &str) -> Result<String> {
    let o = crate::names::top_namespace(old)?;
    let n = match crate::names::namespace(Some(new))? {
        Some(n) if n[1..].contains('.') => {
            return Err(Error::Unsupported(format!(
                "«{n}»: un espacio de nombres se renombra a otro de primer nivel, como .ns2."
            )))
        }
        Some(_) => crate::names::top_namespace(new)?,
        None => return Err(not_a_name(new.trim())),
    };
    if n == o {
        return Err(Error::Unsupported(
            "el nombre nuevo es igual al actual".into(),
        ));
    }
    let refusals = [
        format!("No se renombra {o}: DBine no renombra vistas fuera del raíz ni tablas en disco. Son: "),
        format!("No se renombra {o}: sus funciones y enlaces quedan atados a {o} por nombre y en {n} dejarían de andar. Son: "),
    ];
    Ok(format!(
        "if[not @[{{99h=type get x}};`{o};0b];'{}];if[`{} in key `;'{}];{NAMESPACE}[{BOUND};`{o};`{n};({};{})]",
        qstr(&format!("No existe el espacio de nombres {o}.")),
        &n[1..],
        qstr(&exists_message(&n)),
        qstr(&refusals[0]),
        qstr(&refusals[1]),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PARTITIONED, SPLAYED};
    use dbine_driver::ObjectRef;

    fn obj(kind: &str, name: &str) -> RenameTarget {
        RenameTarget::Object {
            object: ObjectRef {
                kind: kind.into(),
                schema: None,
                name: name.into(),
            },
            parent: None,
        }
    }

    fn req(target: RenameTarget, new: &str) -> RenameRequest {
        RenameRequest {
            target,
            new_name: new.into(),
            table: None,
            definition: None,
        }
    }

    fn one(r: Result<SyncScript>) -> String {
        let s = r.unwrap();
        assert_eq!(s.statements.len(), 1, "{:?}", s.statements);
        s.statements[0].clone()
    }

    fn refused(r: Result<SyncScript>) -> String {
        r.unwrap_err().to_string()
    }

    #[test]
    fn the_spec_offers_every_kind_and_namespaces() {
        let s = spec();
        for k in [kinds::TABLE, kinds::VIEW, kinds::FUNCTION, VARIABLE] {
            assert!(s.kinds.contains(&k.to_string()), "{k}");
        }
        assert!(s.columns && s.schemas && !s.indexes && !s.transactional);
        assert_eq!(s.references, ReferenceStyle::None);
        assert!(s.note.as_deref().unwrap().contains("dentro de su espacio"));
    }

    #[test]
    fn tables_and_columns_in_one_statement() {
        let s = one(script(&req(
            RenameTarget::Column {
                table: ObjectRef {
                    kind: kinds::TABLE.into(),
                    schema: None,
                    name: "t".into(),
                },
                column: "px".into(),
            },
            "price",
        )));
        assert_eq!(
            s,
            "if[not 0~@[{.Q.qp get x};`t;0];'\"«t» es una tabla en disco (particionada o splayed): DBine no cambia su estructura, porque cada columna es un archivo. Cambiala con q en el servidor (por ejemplo con dbmaint o .Q.dpft).\"];if[`price in cols t;'\"Ya existe «price»: no se reemplaza.\"];t:@[cols t;where cols[t]=`px;:;`price] xcol t"
        );
        let s = one(script(&req(obj(kinds::TABLE, ".ns.t"), ".ns.u")));
        assert!(s.starts_with(
            "if[(not `t in key `.ns)|`t in @[system;\"b .ns\";`$()];'\"«.ns.t» no existe o cambió: actualizá el explorador.\"];if[not 0~@[{.Q.qp get x};`.ns.t;0];'"
        ), "{s}");
        assert!(
            s.ends_with(";if[`u in key `.ns;'\"Ya existe «.ns.u»: no se reemplaza.\"];.ns.u:.ns.t;![`.ns;();0b;enlist `t]"),
            "{s}"
        );
        // A table may move to another namespace.
        let s = one(script(&req(obj(kinds::TABLE, "t"), ".x.u")));
        assert!(s.ends_with("if[`u in key `.x;'\"Ya existe «.x.u»: no se reemplaza.\"];.x.u:t;![`.;();0b;enlist `t]"), "{s}");
        // A name that isn't plain goes as a symbol.
        let s = one(script(&req(obj(kinds::TABLE, "my t"), "u")));
        assert!(
            s.starts_with(
                "if[(not (`$ \"my t\") in key `.)|(`$ \"my t\") in @[system;\"b .\";`$()];"
            ),
            "{s}"
        );
        assert!(
            s.ends_with("u:(get `$ \"my t\");![`.;();0b;enlist (`$ \"my t\")]"),
            "{s}"
        );
        // On disk: refused before anything is sent.
        for k in [PARTITIONED, SPLAYED] {
            assert!(refused(script(&req(obj(k, "q"), "r"))).contains("tabla en disco"));
        }
    }

    #[test]
    fn functions_and_variables_stay_in_their_namespace() {
        let s = one(script(&req(obj(kinds::FUNCTION, ".ns.f"), ".ns.g")));
        assert_eq!(
            s,
            "if[(not `f in key `.ns)|`f in @[system;\"b .ns\";`$()];'\"«.ns.f» no existe o cambió: actualizá el explorador.\"];if[not 0~@[{.Q.qp get x};`.ns.f;0];'\"«.ns.f» es una tabla en disco (particionada o splayed): DBine no cambia su estructura, porque cada columna es un archivo. Cambiala con q en el servidor (por ejemplo con dbmaint o .Q.dpft).\"];if[`g in key `.ns;'\"Ya existe «.ns.g»: no se reemplaza.\"];.ns.g:.ns.f;![`.ns;();0b;enlist `f]"
        );
        let s = script(&req(obj(VARIABLE, "x"), "y")).unwrap();
        assert!(s.statements[0].ends_with(";y:x;![`.;();0b;enlist `x]"));
        assert_eq!(s.warnings, [IN_PROCESS]);
        // Nested namespaces: the dictionary that holds it.
        let s = one(script(&req(obj(kinds::FUNCTION, ".a.b.f"), ".a.b.g")));
        assert!(s.ends_with(".a.b.g:.a.b.f;![`.a.b;();0b;enlist `f]"), "{s}");
        // Another namespace, or a name q can't write: refused.
        for (old, new) in [
            (".ns.f", ".x.g"),
            ("f", ".ns.g"),
            (".ns.f", "g"),
            (".a.b.f", ".a.g"),
        ] {
            let e = refused(script(&req(obj(kinds::FUNCTION, old), new)));
            assert!(e.contains("otro espacio de nombres"), "{old} {new}: {e}");
        }
        for new in ["a b", "1x", "g;system\"x\""] {
            let e = refused(script(&req(obj(VARIABLE, "x"), new)));
            assert!(e.contains("no es un nombre de q"), "{new}: {e}");
        }
    }

    #[test]
    fn root_views_only() {
        let s = one(script(&req(obj(kinds::VIEW, "v"), "w")));
        assert_eq!(
            s,
            format!("if[not `v in system \"b .\";'\"«v» no existe o cambió: actualizá el explorador.\"];if[`w in key `.;'\"Ya existe «w»: no se reemplaza.\"];{VIEW}[`w;view `v];![`.;();0b;enlist `v]")
        );
        for (old, new) in [(".ns.v", ".ns.w"), ("v", ".ns.w")] {
            let e = refused(script(&req(obj(kinds::VIEW, old), new)));
            assert!(e.contains("solo en el espacio de nombres raíz"), "{e}");
        }
        assert!(
            refused(script(&req(obj(kinds::VIEW, "v"), "a b"))).contains("no es un nombre de q")
        );
    }

    #[test]
    fn namespaces_checked_and_moved_on_the_server() {
        let ns = |old: &str, new: &str| {
            script(&req(
                RenameTarget::Schema {
                    database: None,
                    schema: old.into(),
                },
                new,
            ))
        };
        let s = ns(".ns", "ns2").unwrap();
        assert_eq!(s.statements.len(), 1);
        assert_eq!(s.warnings, [IN_MEMORY, IN_PROCESS]);
        let st = &s.statements[0];
        assert!(st.starts_with(
            "if[not @[{99h=type get x};`.ns;0b];'\"No existe el espacio de nombres .ns.\"];if[`ns2 in key `;'\"Ya existe «.ns2»: no se reemplaza.\"];"
        ), "{st}");
        assert!(st.ends_with(&format!("{NAMESPACE}[{BOUND};`.ns;`.ns2;(\"No se renombra .ns: DBine no renombra vistas fuera del raíz ni tablas en disco. Son: \";\"No se renombra .ns: sus funciones y enlaces quedan atados a .ns por nombre y en .ns2 dejarían de andar. Son: \")]")), "{st}");
        // The same name either way, both shapes.
        assert_eq!(ns("ns", ".ns2").unwrap().statements, s.statements);
        assert!(refused(ns(".ns", "ns")).contains("igual al actual"));
        // kdb+'s own, DBine's, nested and names that aren't namespaces.
        for (old, new) in [
            (".q", ".x"),
            (".ns", ".z"),
            (".dbine_stage", ".x"),
            (".ns", ".Q"),
        ] {
            assert!(
                refused(ns(old, new)).contains("propio de kdb+"),
                "{old} {new}"
            );
        }
        assert!(refused(ns(".ns", ".a.b")).contains("primer nivel"));
        assert!(refused(ns(".a.b", ".c")).contains("primer nivel"));
        for new in ["a b", "", ".", "x;y"] {
            assert!(ns(".ns", new).is_err(), "{new}");
        }
        // The q that refuses functions, links, views and tables on disk.
        assert!(BOUND.starts_with("{[f;p;x] $[(::)~x;0b;100h<=t:type x;f;"));
        assert!(NAMESPACE.contains("vw:@[system;\"b \",string o;`$()]"));
        assert!(NAMESPACE.ends_with("n set d; ![`;();0b;enlist `$1_string o]}"));
    }
}
