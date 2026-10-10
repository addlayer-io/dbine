//! "Renombrar…": `ALTER TABLE | VIEW | MATERIALIZED VIEW … RENAME TO`,
//! `ALTER TABLE … RENAME COLUMN` and `ALTER SCHEMA … RENAME TO`.
//!
//! Trino parses them all, but the connector decides: memory and Iceberg
//! take them, Hive refuses some, read-only connectors none. The spec is
//! broad and the server refuses what its catalog can't do. Presto has no
//! materialized view rename and takes `ALTER VIEW … RENAME` only on recent
//! versions; its memory connector renames tables and views, not columns or
//! schemas.
//!
//! Views keep their SQL text and bind names when queried: a rename leaves
//! them naming the old one. They are only listed for the user to fix
//! (`ReferenceStyle::None`), never put back: `CREATE OR REPLACE VIEW` (or
//! `MATERIALIZED VIEW`) makes the session user the owner, and a `SECURITY
//! DEFINER` view (the default) then reads with the renaming user's rights,
//! past the row filters, masks and grants its owner had. The renamed
//! object itself goes through `ALTER … RENAME TO`, which keeps its owner.
//! The script starts with `USE` of the target's schema, so the session
//! ends where the object is. Names are stored in lower case, and DDL isn't
//! transactional on the usual connectors ("Catalog only supports writes
//! using autocommit").

use crate::Flavor;
use dbine_driver::rename::{quote_new, Fold, RenameRequest, RenameSpec, RenameTarget, ReferenceStyle, ReplaceStyle};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{kinds, Error, ObjectRef, Result, SyncScript};

pub const NOTE: &str = "Lo que se puede renombrar lo decide el conector del catálogo (memory e Iceberg renombran tablas, vistas y columnas; Hive no renombra algunas cosas): si el conector no puede, el servidor rechaza la sentencia. Las vistas y vistas materializadas que nombran lo renombrado no se actualizan solas, y DBine solo las lista para corregirlas a mano: recrearlas las pasaría al usuario que renombra, y una vista SECURITY DEFINER leería con sus permisos en lugar de los de su dueño. Las sentencias no son transaccionales: si una falla, las anteriores quedan hechas.";

pub const SCHEMA_VIEWS: &str = "Las vistas del esquema que nombran tablas sin calificar dejan de funcionar: Trino las resuelve con el esquema en que se crearon, que deja de existir. Volvé a crearlas (SHOW CREATE VIEW) en el esquema nuevo después de renombrar.";

pub fn spec(flavor: Flavor) -> Option<RenameSpec> {
    let mut kinds = vec![kinds::TABLE.to_string(), kinds::VIEW.to_string()];
    if flavor != Flavor::Presto {
        kinds.push(kinds::MATERIALIZED_VIEW.to_string());
    }
    Some(RenameSpec {
        kinds,
        columns: true,
        indexes: false,
        constraints: false,
        schemas: true,
        tracked: Vec::new(),
        replace: ReplaceStyle::CreateOrReplace,
        // Dependents are listed, never put back (see the module's doc).
        references: ReferenceStyle::None,
        fold: Fold::Lower,
        transactional: false,
        note: Some(NOTE.into()),
        ..Default::default()
    })
}

fn q(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

/// `"schema"."name"`, or the bare name.
fn qualified(schema: Option<&str>, name: &str) -> String {
    match schema {
        Some(s) => format!("{}.{name}", q(s)),
        None => name.to_string(),
    }
}

/// `USE "schema"`: unqualified names in the views put back resolve there.
fn use_schema(schema: Option<&str>) -> Vec<String> {
    schema.map(|s| format!("USE {};", q(s))).into_iter().collect()
}

pub fn script(flavor: Flavor, req: &RenameRequest) -> Result<SyncScript> {
    let lower = req.new_name.to_lowercase();
    if req.new_name != lower {
        return Err(Error::Unsupported(format!("Trino guarda los nombres en minúsculas: escribí el nombre nuevo como «{lower}»")));
    }
    let new = quote_new(&req.new_name, &crate::script_dialect(), Fold::Lower, false);
    let mut warnings = Vec::new();
    let statements = match &req.target {
        RenameTarget::Object { object, .. } => {
            let what = match object.kind.as_str() {
                kinds::TABLE => "TABLE",
                kinds::VIEW => "VIEW",
                kinds::MATERIALIZED_VIEW if flavor != Flavor::Presto => "MATERIALIZED VIEW",
                kinds::MATERIALIZED_VIEW => return Err(Error::Unsupported("Presto no renombra vistas materializadas".into())),
                _ => return Err(Error::Unsupported("Trino solo renombra tablas, vistas, vistas materializadas, columnas y esquemas".into())),
            };
            let mut s = use_schema(object.schema());
            // The new name carries the schema: unqualified, Trino would
            // put it in the session's schema.
            s.push(format!("ALTER {what} {} RENAME TO {};", old(object), qualified(object.schema(), &new)));
            s
        }
        RenameTarget::Column { table, column } => {
            let mut s = use_schema(table.schema());
            s.push(format!("ALTER TABLE {} RENAME COLUMN {} TO {new};", old(table), q(column)));
            s
        }
        RenameTarget::Schema { database, schema } => {
            let cat = database.as_deref().filter(|d| !d.is_empty());
            let at = |name: &str| match cat {
                Some(c) => format!("{}.{name}", q(c)),
                None => name.to_string(),
            };
            warnings.push(SCHEMA_VIEWS.to_string());
            vec![format!("ALTER SCHEMA {} RENAME TO {new};", at(&q(schema))), format!("USE {};", at(&new))]
        }
        RenameTarget::Index { .. } => return Err(Error::Unsupported("Trino no tiene índices".into())),
        RenameTarget::Constraint { .. } => return Err(Error::Unsupported("Trino no renombra restricciones".into())),
    };
    Ok(SyncScript { statements, warnings })
}

fn old(o: &ObjectRef) -> String {
    qualified(o.schema(), &q(&o.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(target: RenameTarget, new: &str) -> RenameRequest {
        RenameRequest { target, new_name: new.into(), table: None, definition: None }
    }

    fn obj(kind: &str, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: Some("ventas".into()), name: name.into() }
    }

    fn object(kind: &str, name: &str) -> RenameTarget {
        RenameTarget::Object { object: obj(kind, name), parent: None }
    }

    fn run(f: Flavor, t: RenameTarget, new: &str) -> Vec<String> {
        script(f, &req(t, new)).unwrap().statements
    }

    #[test]
    fn spec_per_flavor() {
        let s = spec(Flavor::Trino).unwrap();
        assert_eq!(s.kinds, ["table", "view", "materialized_view"]);
        assert!(s.columns && s.schemas && !s.indexes && !s.constraints && !s.transactional);
        assert!(s.tracked.is_empty());
        assert_eq!((s.replace, s.fold), (ReplaceStyle::CreateOrReplace, Fold::Lower));
        assert_eq!(spec(Flavor::Starburst).unwrap().kinds.len(), 3);
        assert_eq!(spec(Flavor::Presto).unwrap().kinds, ["table", "view"]);
    }

    #[test]
    fn dependents_are_listed_never_put_back() {
        // Re-created, a view or materialized view would belong to (and a
        // DEFINER one read as) the user running the rename.
        for f in [Flavor::Trino, Flavor::Presto, Flavor::Starburst] {
            let s = spec(f).unwrap();
            assert_eq!(s.references, ReferenceStyle::None, "{f:?}");
            assert!(s.note.as_deref().is_some_and(|n| n.contains("a mano") && n.contains("SECURITY DEFINER")));
        }
        // What the app runs on a dependent: nothing is rewritten.
        let s = spec(Flavor::Trino).unwrap();
        let target = dbine_driver::rename::RewriteTarget::Object { object: obj("table", "clientes") };
        let r = dbine_driver::rename::rewrite_references(
            "CREATE VIEW ventas.v SECURITY DEFINER AS SELECT * FROM ventas.clientes",
            &crate::script_dialect(),
            &target,
            "clientes_2",
            &s,
            &Default::default(),
        );
        assert!(r.edits.is_empty());
        // The renamed object keeps its owner: no CREATE in its own script.
        for t in [object("view", "v"), object("materialized_view", "mv"), object("table", "t")] {
            assert!(run(Flavor::Trino, t, "n").iter().all(|s| !s.contains("CREATE")));
        }
    }

    #[test]
    fn tables_views_and_materialized_views() {
        assert_eq!(run(Flavor::Trino, object("table", "clientes"), "clientes_viejos"), ["USE \"ventas\";", "ALTER TABLE \"ventas\".\"clientes\" RENAME TO \"ventas\".clientes_viejos;"]);
        assert_eq!(run(Flavor::Presto, object("view", "v\"x"), "select"), ["USE \"ventas\";", "ALTER VIEW \"ventas\".\"v\"\"x\" RENAME TO \"ventas\".\"select\";"]);
        assert_eq!(run(Flavor::Trino, object("materialized_view", "mv"), "mv 2"), ["USE \"ventas\";", "ALTER MATERIALIZED VIEW \"ventas\".\"mv\" RENAME TO \"ventas\".\"mv 2\";"]);
        let no_schema = RenameTarget::Object { object: ObjectRef { kind: "table".into(), schema: None, name: "t".into() }, parent: None };
        assert_eq!(run(Flavor::Trino, no_schema, "u"), ["ALTER TABLE \"t\" RENAME TO u;"]);
        assert!(matches!(script(Flavor::Presto, &req(object("materialized_view", "mv"), "n")), Err(Error::Unsupported(_))));
        assert!(matches!(script(Flavor::Trino, &req(object("function", "f"), "g")), Err(Error::Unsupported(_))));
    }

    #[test]
    fn columns() {
        let target = || RenameTarget::Column { table: obj("table", "T"), column: "Pepe".into() };
        assert_eq!(run(Flavor::Trino, target(), "nuevo"), ["USE \"ventas\";", "ALTER TABLE \"ventas\".\"T\" RENAME COLUMN \"Pepe\" TO nuevo;"]);
        assert_eq!(run(Flavor::Starburst, target(), "nuevo pepe"), ["USE \"ventas\";", "ALTER TABLE \"ventas\".\"T\" RENAME COLUMN \"Pepe\" TO \"nuevo pepe\";"]);
    }

    #[test]
    fn schemas() {
        let s = script(Flavor::Trino, &req(RenameTarget::Schema { database: Some("memory".into()), schema: "ventas".into() }, "ventas_2024")).unwrap();
        assert_eq!(s.statements, ["ALTER SCHEMA \"memory\".\"ventas\" RENAME TO ventas_2024;", "USE \"memory\".ventas_2024;"]);
        assert_eq!(s.warnings, [SCHEMA_VIEWS]);
        let s = run(Flavor::Presto, RenameTarget::Schema { database: None, schema: "ventas".into() }, "v2");
        assert_eq!(s, ["ALTER SCHEMA \"ventas\" RENAME TO v2;", "USE v2;"]);
    }

    #[test]
    fn upper_case_and_refused() {
        // Trino would store "pepe": the rewritten code and the tabs would not match.
        assert!(matches!(script(Flavor::Trino, &req(object("table", "t"), "Pepe")), Err(Error::Unsupported(_))));
        let ix = RenameTarget::Index { table: obj("table", "t"), index: "ix".into() };
        assert!(matches!(script(Flavor::Trino, &req(ix, "iy")), Err(Error::Unsupported(_))));
        let c = RenameTarget::Constraint { table: obj("table", "t"), constraint: "c".into() };
        assert!(matches!(script(Flavor::Presto, &req(c, "d")), Err(Error::Unsupported(_))));
    }
}
