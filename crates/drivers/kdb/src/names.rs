//! Object names: DBine's (schema, name) and q's. A namespace is a schema:
//! `ObjectRef { schema: Some(".ns"), name: "t" }` is q's `.ns.t`, and the
//! root namespace's objects have none (`t`). References saved before
//! namespaces were schemas name the object whole (`schema: None, name:
//! ".ns.t"`, in tabs, favorites, notebooks, tasks…): they resolve as
//! written.
//!
//! Every hook that gets an object turns it into its q name here, once
//! ([`resolved`], [`resolved_table`]); the rest of the crate works with q
//! names. A namespace must be a q name (`.ns`, `.a.b`); the object's own
//! name goes into q as a symbol when it isn't plain ([`crate::edit::qsym`]),
//! so it never becomes q code.

use crate::edit::plain;
use dbine_driver::{Error, ObjectRef, RenameRequest, RenameTarget, Result, TableSchema};

/// kdb+'s own namespaces and DBine's staging one: DBine creates, drops
/// and writes nothing in them.
pub const RESERVED: [&str; 7] = ["q", "Q", "h", "j", "o", "z", "dbine_stage"];

fn not_a_namespace(s: &str) -> Error {
    Error::Unsupported(format!(
        "«{s}» no es un espacio de nombres de q: empieza con un punto y sigue con letras, dígitos y _ (por ejemplo .ns)."
    ))
}

fn reserved_message(ns: &str) -> String {
    format!("{ns} es un espacio de nombres propio de kdb+ (o el de DBine): DBine no crea, borra ni cambia nada en él.")
}

/// The namespace `schema` names, with its dot: `.ns` (also given as `ns`,
/// as typed in a write cell or a migration's target schema), `.a.b`.
/// `None`: the root (no schema, an empty one or `.`).
pub fn namespace(schema: Option<&str>) -> Result<Option<String>> {
    let s = schema.map(str::trim).unwrap_or("");
    if s.is_empty() || s == "." {
        return Ok(None);
    }
    let ns = if s.starts_with('.') {
        s.to_string()
    } else {
        format!(".{s}")
    };
    if plain(&ns) {
        Ok(Some(ns))
    } else {
        Err(not_a_namespace(s))
    }
}

/// The q name of `name` in `schema`: `.ns.t`, or `name` as is in the root
/// (an old reference's `.ns.t` included). A name already written whole
/// in its namespace (`.ns.t` with schema `.ns`) stays so; whole in another
/// one, it's an error.
pub fn q_name(schema: Option<&str>, name: &str) -> Result<String> {
    if name.is_empty() {
        return Err(Error::Unsupported("falta el nombre del objeto".into()));
    }
    let Some(ns) = namespace(schema)? else {
        return Ok(name.to_string());
    };
    if name.starts_with('.') {
        return match name.strip_prefix(ns.as_str()) {
            Some(rest) if rest.starts_with('.') && rest.len() > 1 => Ok(name.to_string()),
            _ => Err(Error::Unsupported(format!(
                "«{name}» no está en el espacio de nombres {ns}."
            ))),
        };
    }
    Ok(format!("{ns}.{name}"))
}

/// `name` in the namespace of the explorer's object `(schema, name)`,
/// which is already a q name: for DBine's own lists, never fails.
pub fn joined(schema: Option<&str>, name: &str) -> String {
    q_name(schema, name).unwrap_or_else(|_| name.to_string())
}

/// A q name as the explorer names it: `.ns.t` → (`.ns`, `t`), anything
/// else (a root name) → (`None`, as is). The namespace is the first part
/// only, as the explorer lists them.
pub fn split(q: &str) -> (Option<String>, String) {
    if let Some(rest) = q.strip_prefix('.') {
        if let Some(i) = rest.find('.') {
            let (ns, name) = (&rest[..i], &rest[i + 1..]);
            if !ns.is_empty() && !name.is_empty() {
                return (Some(format!(".{ns}")), name.to_string());
            }
        }
    }
    (None, q.to_string())
}

/// `r` with its q name and no schema.
pub fn resolved(r: &ObjectRef) -> Result<ObjectRef> {
    Ok(ObjectRef {
        kind: r.kind.clone(),
        schema: None,
        name: q_name(r.schema(), &r.name)?,
    })
}

/// `t` with its q name and no schema. Its links (foreign keys) stay as
/// they are: q's DDL has none (a link is an enumeration of the data, which
/// the clone makes apart, by q name).
pub fn resolved_table(t: &TableSchema) -> Result<TableSchema> {
    let mut out = t.clone();
    out.name = q_name(t.schema.as_deref(), &t.name)?;
    out.schema = None;
    Ok(out)
}

/// The namespace of q name `q` is one of [`RESERVED`]: refused for
/// anything that writes.
pub fn writable(q: &str) -> Result<()> {
    match split(q).0 {
        Some(ns) if RESERVED.contains(&&ns[1..]) => Err(Error::Unsupported(reserved_message(&ns))),
        _ => Ok(()),
    }
}

/// A namespace "Nuevo esquema…" and "Borrar esquema…" take: one of the
/// root's (`.ns`, or `ns`), not kdb+'s own nor DBine's.
pub fn top_namespace(name: &str) -> Result<String> {
    let ns = namespace(Some(name))?.ok_or_else(|| not_a_namespace(name.trim()))?;
    if ns[1..].contains('.') {
        return Err(Error::Unsupported(format!(
            "«{ns}»: desde DBine se crea o se borra un espacio de nombres de primer nivel, como .ns."
        )));
    }
    if RESERVED.contains(&&ns[1..]) {
        return Err(Error::Unsupported(reserved_message(&ns)));
    }
    Ok(ns)
}

/// `req` with q names: the object's or the table's, and the new name in
/// the object's namespace when it has one (a name that starts with a dot
/// is whole, as an old reference's).
pub fn resolved_rename(req: &RenameRequest) -> Result<RenameRequest> {
    let mut out = req.clone();
    out.target = match &req.target {
        RenameTarget::Object { object, parent } => {
            if !req.new_name.starts_with('.') {
                out.new_name = q_name(object.schema(), &req.new_name)?;
            }
            RenameTarget::Object {
                object: resolved(object)?,
                parent: parent.clone(),
            }
        }
        RenameTarget::Column { table, column } => RenameTarget::Column {
            table: resolved(table)?,
            column: column.clone(),
        },
        RenameTarget::Index { table, index } => RenameTarget::Index {
            table: resolved(table)?,
            index: index.clone(),
        },
        RenameTarget::Constraint { table, constraint } => RenameTarget::Constraint {
            table: resolved(table)?,
            constraint: constraint.clone(),
        },
        other => other.clone(),
    };
    if let Some(t) = &req.table {
        out.table = Some(resolved_table(t)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::kinds;

    fn r(schema: Option<&str>, name: &str) -> ObjectRef {
        ObjectRef {
            kind: kinds::TABLE.into(),
            schema: schema.map(String::from),
            name: name.into(),
        }
    }

    #[test]
    fn both_shapes_give_the_same_q_name() {
        // The new shape, an old reference and the root.
        assert_eq!(resolved(&r(Some(".ns"), "t")).unwrap().name, ".ns.t");
        assert_eq!(resolved(&r(None, ".ns.t")).unwrap().name, ".ns.t");
        assert_eq!(resolved(&r(None, "t")).unwrap().name, "t");
        assert!(resolved(&r(None, "t")).unwrap().schema.is_none());
        // An empty schema and `.` are the root; `ns` is `.ns`.
        for root in [Some(""), Some("."), Some("  ")] {
            assert_eq!(q_name(root, "t").unwrap(), "t");
        }
        assert_eq!(q_name(Some("ns"), "t").unwrap(), ".ns.t");
        assert_eq!(q_name(Some(".a.b"), "t").unwrap(), ".a.b.t");
        // Written whole in its own namespace, it stays; in another, refused.
        assert_eq!(q_name(Some(".ns"), ".ns.t").unwrap(), ".ns.t");
        assert!(q_name(Some(".ns"), ".other.t").is_err());
        assert!(q_name(Some(".ns"), ".nsx.t").is_err());
        assert!(q_name(Some(".ns"), ".ns.").is_err());
        assert!(q_name(Some(".ns"), "").is_err());
        // Resolving twice changes nothing.
        let once = resolved(&r(Some(".ns"), "t")).unwrap();
        assert_eq!(resolved(&once).unwrap().name, ".ns.t");
    }

    #[test]
    fn namespaces_that_arent_q_names_are_refused() {
        for bad in [
            ".ns;system\"rm -rf /\"",
            "ns]",
            ".ns t",
            "1ns",
            ".ns.",
            "..ns",
            ".n\"s",
            "`ns",
            ".ns\n.z.pg:{}",
        ] {
            let e = q_name(Some(bad), "t").unwrap_err().to_string();
            assert!(e.contains("espacio de nombres"), "{bad}: {e}");
        }
        // An object's own name goes into q as a symbol: no check needed,
        // and none of it becomes code.
        let odd = q_name(Some(".ns"), "t;system\"x\"").unwrap();
        assert_eq!(crate::edit::qsym(&odd), "(`$ \".ns.t;system\\\"x\\\"\")");
        assert_eq!(
            crate::edit::qref(&odd),
            "(get `$ \".ns.t;system\\\"x\\\"\")"
        );
    }

    #[test]
    fn q_names_split_as_the_explorer_lists_them() {
        assert_eq!(split(".ns.t"), (Some(".ns".into()), "t".into()));
        assert_eq!(split(".u.upd"), (Some(".u".into()), "upd".into()));
        assert_eq!(split(".a.b.c"), (Some(".a".into()), "b.c".into()));
        assert_eq!(split("trade"), (None, "trade".into()));
        assert_eq!(split(".foo"), (None, ".foo".into()));
        assert_eq!(split("."), (None, ".".into()));
        // Split and joined back, the same.
        for q in [".ns.t", "t", ".u.upd"] {
            let (s, n) = split(q);
            assert_eq!(joined(s.as_deref(), &n), q);
        }
    }

    #[test]
    fn tables_resolve_in_both_shapes() {
        let t = TableSchema {
            kind: kinds::TABLE.into(),
            schema: Some(".ns".into()),
            name: "t".into(),
            ..Default::default()
        };
        let q = resolved_table(&t).unwrap();
        assert_eq!(
            (q.schema, q.name.as_str(), q.kind.as_str()),
            (None, ".ns.t", "table")
        );
        let old = TableSchema {
            schema: None,
            name: ".ns.t".into(),
            ..t.clone()
        };
        assert_eq!(resolved_table(&old).unwrap().name, ".ns.t");
        let bad = TableSchema {
            schema: Some(".n s".into()),
            ..t
        };
        assert!(resolved_table(&bad).is_err());
    }

    #[test]
    fn kdbs_own_namespaces_take_no_writes() {
        for q in [".z.pg", ".q.x", ".Q.t", ".dbine_stage.w"] {
            assert!(writable(q).is_err(), "{q}");
        }
        for q in ["t", ".ns.t", ".zz.t", ".u.t"] {
            assert!(writable(q).is_ok(), "{q}");
        }
        assert_eq!(top_namespace("ns").unwrap(), ".ns");
        assert_eq!(top_namespace(" .ns ").unwrap(), ".ns");
        for bad in [
            ".z",
            "q",
            ".Q",
            ".dbine_stage",
            ".a.b",
            "",
            ".",
            "n s",
            ".ns;0",
        ] {
            assert!(top_namespace(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn renames_name_the_new_object_in_its_namespace() {
        let req = |object: ObjectRef, new: &str| RenameRequest {
            target: RenameTarget::Object {
                object,
                parent: None,
            },
            new_name: new.into(),
            table: None,
            definition: None,
        };
        let names = |q: RenameRequest| match q.target {
            RenameTarget::Object { object, .. } => (object.name, q.new_name),
            _ => unreachable!(),
        };
        assert_eq!(
            names(resolved_rename(&req(r(Some(".ns"), "t"), "u")).unwrap()),
            (".ns.t".to_string(), ".ns.u".to_string())
        );
        // A whole name stays whole; an old reference renames as written.
        assert_eq!(
            names(resolved_rename(&req(r(Some(".ns"), "t"), ".x.u")).unwrap()).1,
            ".x.u"
        );
        assert_eq!(
            names(resolved_rename(&req(r(None, ".ns.t"), ".ns.u")).unwrap()),
            (".ns.t".to_string(), ".ns.u".to_string())
        );
        let col = RenameRequest {
            target: RenameTarget::Column {
                table: r(Some(".ns"), "t"),
                column: "a".into(),
            },
            new_name: "b".into(),
            table: None,
            definition: None,
        };
        let out = resolved_rename(&col).unwrap();
        assert_eq!(out.new_name, "b");
        assert!(matches!(out.target, RenameTarget::Column { table, .. } if table.name == ".ns.t"));
    }
}
