//! "Renombrar…" on Cloud Spanner (GoogleSQL): `ALTER TABLE … RENAME TO`
//! for tables, and views dropped and created again with the new name
//! (there is no `ALTER VIEW`).
//!
//! Spanner moves a table's indexes, foreign keys, interleaved children,
//! change streams and fine-grained grants to the new name by itself, but
//! not its views, and it refuses the rename while a view still names the
//! old one. So the views are dropped before the rename and created after
//! it, rewritten (`DropCreate`). Columns, indexes, sequences and schemas
//! can't be renamed. Each DDL statement is applied on its own: no atomic
//! run.

use dbine_driver::rename::{quote_new, rename_header, Fold, RenameRequest, RenameSpec, RenameTarget, ReplaceStyle};
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{kinds, Error, ReferenceStyle, Result, SyncScript};

const ENGINE: &str = "Spanner";

pub(crate) fn spec() -> RenameSpec {
    RenameSpec {
        kinds: vec![kinds::TABLE.into(), kinds::VIEW.into()],
        columns: false,
        indexes: false,
        constraints: false,
        schemas: false,
        tracked: Vec::new(),
        replace: ReplaceStyle::DropCreate,
        references: ReferenceStyle::Sql,
        fold: Fold::None,
        transactional: false,
        note: Some(
            "Spanner no renombra una tabla mientras una vista la use: DBine borra esas vistas antes del cambio y las vuelve a crear después. Índices, claves foráneas, tablas intercaladas y change streams siguen a la tabla solos. Cada sentencia DDL se aplica por separado: si una falla, las anteriores quedan aplicadas."
                .into(),
        ),
        ..Default::default()
    }
}

pub(crate) fn script(req: &RenameRequest) -> Result<SyncScript> {
    let spec = spec();
    if !spec.allows(&req.target) {
        let what = match &req.target {
            RenameTarget::Object { object, .. } if object.kind == kinds::SEQUENCE => "secuencias".to_string(),
            RenameTarget::Object { object, .. } => format!("objetos de tipo «{}»", object.kind),
            RenameTarget::Column { .. } => "columnas".into(),
            RenameTarget::Index { .. } => "índices (hay que borrarlos y crearlos con otro nombre)".into(),
            RenameTarget::Constraint { .. } => "restricciones".into(),
            RenameTarget::Schema { .. } => "esquemas".into(),
        };
        return Err(Error::Unsupported(format!("{ENGINE} no renombra {what}")));
    }
    let dialect = crate::script::dialect();
    let new = quote_new(&req.new_name, &dialect, spec.fold, false);
    let RenameTarget::Object { object, .. } = &req.target else { unreachable!("only objects are allowed") };
    let schema = object.schema().filter(|s| !s.is_empty());
    let old = qualified_name(Quote::Backtick, schema, &object.name);
    let statements = if object.kind == kinds::VIEW {
        let def = req
            .definition
            .as_deref()
            .ok_or_else(|| Error::Unsupported(format!("no se leyó la definición de la vista «{}»", object.name)))?;
        let create = rename_header(def.trim_end().trim_end_matches(';'), &dialect, spec.fold, &req.new_name)
            .ok_or_else(|| Error::Unsupported(format!("no se encontró el nombre en la definición de la vista «{}»", object.name)))?;
        // Created first: if it fails, the old view is still there.
        vec![format!("{create};"), format!("DROP VIEW {old};")]
    } else {
        // An unqualified new name would move the table to the default schema.
        let target = match schema {
            Some(s) => format!("{}.{new}", qualified_name(Quote::Backtick, None, s)),
            None => new,
        };
        vec![format!("ALTER TABLE {old} RENAME TO {target};")]
    };
    let warnings = if object.kind == kinds::VIEW {
        vec!["La vista se crea con el nombre nuevo y se borra la anterior: se pierden los permisos otorgados sobre ella.".into()]
    } else {
        Vec::new()
    };
    Ok(SyncScript { statements, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ObjectRef;

    fn obj(kind: &str, schema: &str, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: (!schema.is_empty()).then(|| schema.into()), name: name.into() }
    }

    fn req(target: RenameTarget, new: &str, definition: Option<&str>) -> RenameRequest {
        RenameRequest { target, new_name: new.into(), table: None, definition: definition.map(Into::into) }
    }

    fn object(kind: &str, schema: &str, name: &str, new: &str, def: Option<&str>) -> RenameRequest {
        req(RenameTarget::Object { object: obj(kind, schema, name), parent: None }, new, def)
    }

    fn stmts(r: &RenameRequest) -> Vec<String> {
        script(r).unwrap_or_else(|e| panic!("{e}")).statements
    }

    #[test]
    fn tables_keep_their_schema_and_case() {
        assert_eq!(stmts(&object(kinds::TABLE, "", "Singers", "Cantantes", None)), ["ALTER TABLE `Singers` RENAME TO Cantantes;"]);
        assert_eq!(stmts(&object(kinds::TABLE, "ventas", "T", "U", None)), ["ALTER TABLE `ventas`.`T` RENAME TO `ventas`.U;"]);
        // Reserved words and odd characters are quoted.
        assert_eq!(stmts(&object(kinds::TABLE, "", "t", "select", None)), ["ALTER TABLE `t` RENAME TO `select`;"]);
        assert_eq!(stmts(&object(kinds::TABLE, "", "t", "mi tabla", None)), ["ALTER TABLE `t` RENAME TO `mi tabla`;"]);
    }

    #[test]
    fn views_are_created_again_with_the_new_name() {
        let def = "CREATE VIEW `ventas`.`V` SQL SECURITY INVOKER AS\nSELECT T.id, T.pepe FROM ventas.T";
        let s = script(&object(kinds::VIEW, "ventas", "V", "Vista2", Some(def))).unwrap();
        assert_eq!(
            s.statements,
            ["CREATE VIEW `ventas`.`Vista2` SQL SECURITY INVOKER AS\nSELECT T.id, T.pepe FROM ventas.T;", "DROP VIEW `ventas`.`V`;"]
        );
        assert_eq!(s.warnings.len(), 1);
        let def = "CREATE VIEW `V` SQL SECURITY DEFINER AS\nSELECT 1 AS x";
        assert_eq!(stmts(&object(kinds::VIEW, "", "V", "w", Some(def))), ["CREATE VIEW `w` SQL SECURITY DEFINER AS\nSELECT 1 AS x;", "DROP VIEW `V`;"]);
        // Without its definition there's nothing to create.
        assert!(matches!(script(&object(kinds::VIEW, "", "V", "w", None)), Err(Error::Unsupported(_))));
    }

    #[test]
    fn refuses_what_spanner_cant_rename() {
        let t = obj(kinds::TABLE, "", "T");
        for r in [
            req(RenameTarget::Column { table: t.clone(), column: "pepe".into() }, "juan", None),
            req(RenameTarget::Index { table: t.clone(), index: "ix".into() }, "iy", None),
            req(RenameTarget::Constraint { table: t, constraint: "ck".into() }, "ck2", None),
            req(RenameTarget::Schema { database: None, schema: "ventas".into() }, "v2", None),
            object(kinds::SEQUENCE, "", "s", "s2", None),
        ] {
            assert!(matches!(script(&r), Err(Error::Unsupported(_))), "{:?}", r.target);
        }
        let s = spec();
        assert_eq!(s.kinds, ["table", "view"]);
        assert!(!s.columns && !s.indexes && !s.constraints && !s.schemas && !s.transactional && s.tracked.is_empty());
        assert_eq!(s.replace, ReplaceStyle::DropCreate);
        assert_eq!(s.fold, Fold::None);
    }
}
