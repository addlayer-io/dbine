//! "Renombrar…": `ALTER TABLE | VIEW … RENAME TO` and `RENAME COLUMN`.
//!
//! DuckDB binds views and macros by name when they run: a rename leaves
//! them pointing at the old name, so the app rewrites them and puts them
//! back with `CREATE OR REPLACE` after the rename (nothing is `tracked`).
//! What it does track are indexes and foreign keys, and it refuses to
//! rename what they use: a table with an index or that a foreign key
//! references, and an indexed column or one in a foreign key. It has no
//! `RENAME` for indexes, sequences, macros, types or schemas. DDL is
//! transactional, so the script runs atomically.
//!
//! The files preset rebuilds its views from the folder on every
//! connection: a rename there wouldn't last, so it isn't offered.

use dbine_driver::rename::{quote_new, Fold, RenameRequest, RenameSpec, RenameTarget, ReferenceStyle, ReplaceStyle};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{kinds, Error, ObjectRef, Result, SyncScript};

pub const NOTE: &str = "DuckDB no actualiza las vistas ni las macros que usan el objeto: DBine las reescribe y las repone con CREATE OR REPLACE después del cambio. DuckDB rechaza renombrar una tabla que tiene índices o a la que apunta una clave foránea, y una columna que está en un índice o en una clave foránea: borralos antes y crealos de nuevo después.";

pub fn spec(files: bool) -> Option<RenameSpec> {
    if files {
        return None;
    }
    Some(RenameSpec {
        kinds: vec![kinds::TABLE.into(), kinds::VIEW.into()],
        columns: true,
        indexes: false,
        constraints: false,
        schemas: false,
        tracked: Vec::new(),
        replace: ReplaceStyle::CreateOrReplace,
        references: ReferenceStyle::Sql,
        fold: Fold::None,
        transactional: true,
        note: Some(NOTE.into()),
        ..Default::default()
    })
}

fn qn(o: &ObjectRef) -> String {
    qualified_name(Quote::Double, o.schema(), &o.name)
}

pub fn script(files: bool, req: &RenameRequest) -> Result<SyncScript> {
    if files {
        return Err(Error::Unsupported("las vistas de esta conexión salen de los archivos de la carpeta: para cambiarle el nombre a una, renombrá el archivo".into()));
    }
    let new = quote_new(&req.new_name, &crate::dialect(), Fold::None, false);
    let statement = match &req.target {
        RenameTarget::Object { object, .. } if object.kind == kinds::TABLE => format!("ALTER TABLE {} RENAME TO {new};", qn(object)),
        RenameTarget::Object { object, .. } if object.kind == kinds::VIEW => format!("ALTER VIEW {} RENAME TO {new};", qn(object)),
        RenameTarget::Object { .. } => {
            return Err(Error::Unsupported("DuckDB solo renombra tablas, vistas y columnas: las macros, las secuencias y los tipos se crean con el nombre nuevo".into()))
        }
        RenameTarget::Column { table, column } => {
            if let Some(t) = &req.table {
                if t.foreign_keys.iter().any(|f| f.columns.iter().any(|c| c.eq_ignore_ascii_case(column))) {
                    return Err(Error::Unsupported(format!(
                        "DuckDB no renombra la columna «{column}» porque está en una clave foránea: hay que reconstruir la tabla"
                    )));
                }
            }
            format!("ALTER TABLE {} RENAME COLUMN {} TO {new};", qn(table), quote_ident(Quote::Double, column))
        }
        RenameTarget::Index { .. } => return Err(Error::Unsupported("DuckDB no renombra índices: hay que borrarlo y crearlo con el nombre nuevo".into())),
        RenameTarget::Constraint { .. } => return Err(Error::Unsupported("DuckDB no renombra restricciones".into())),
        RenameTarget::Schema { .. } => return Err(Error::Unsupported("DuckDB no renombra esquemas".into())),
    };
    Ok(SyncScript { statements: vec![statement], warnings: vec![] })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ForeignKeyDef, TableSchema};

    fn req(target: RenameTarget, new: &str) -> RenameRequest {
        RenameRequest { target, new_name: new.into(), table: None, definition: None }
    }

    fn obj(kind: &str, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: Some("main".into()), name: name.into() }
    }

    fn object(kind: &str, name: &str) -> RenameTarget {
        RenameTarget::Object { object: obj(kind, name), parent: None }
    }

    #[test]
    fn spec_and_files() {
        let s = spec(false).unwrap();
        assert_eq!(s.kinds, ["table", "view"]);
        assert!(s.columns && !s.indexes && !s.constraints && !s.schemas && s.transactional);
        assert!(s.tracked.is_empty());
        assert_eq!(s.replace, ReplaceStyle::CreateOrReplace);
        assert!(spec(true).is_none());
        assert!(matches!(script(true, &req(object("view", "v"), "w")), Err(Error::Unsupported(_))));
    }

    #[test]
    fn tables_and_views() {
        assert_eq!(script(false, &req(object("table", "clientes"), "Clientes Viejos")).unwrap().statements, ["ALTER TABLE \"main\".\"clientes\" RENAME TO \"Clientes Viejos\";"]);
        assert_eq!(script(false, &req(object("view", "v\"x"), "w")).unwrap().statements, ["ALTER VIEW \"main\".\"v\"\"x\" RENAME TO w;"]);
        assert_eq!(script(false, &req(object("view", "v"), "select")).unwrap().statements, ["ALTER VIEW \"main\".\"v\" RENAME TO \"select\";"]);
        assert!(matches!(script(false, &req(object("function", "m"), "n")), Err(Error::Unsupported(_))));
        assert!(matches!(script(false, &req(object("sequence", "s"), "n")), Err(Error::Unsupported(_))));
    }

    #[test]
    fn columns() {
        let target = || RenameTarget::Column { table: obj("table", "t"), column: "pepe".into() };
        assert_eq!(script(false, &req(target(), "Pepe")).unwrap().statements, ["ALTER TABLE \"main\".\"t\" RENAME COLUMN \"pepe\" TO Pepe;"]);
        assert_eq!(script(false, &req(target(), "pepa")).unwrap().statements, ["ALTER TABLE \"main\".\"t\" RENAME COLUMN \"pepe\" TO pepa;"]);
        let mut r = req(target(), "pepa");
        r.table = Some(TableSchema {
            name: "t".into(),
            foreign_keys: vec![ForeignKeyDef { columns: vec!["PEPE".into()], ref_table: "u".into(), ref_columns: vec!["id".into()], ..Default::default() }],
            ..Default::default()
        });
        assert!(matches!(script(false, &r), Err(Error::Unsupported(_))));
    }

    #[test]
    fn refused() {
        let ix = RenameTarget::Index { table: obj("table", "t"), index: "ix".into() };
        assert!(matches!(script(false, &req(ix, "iy")), Err(Error::Unsupported(_))));
        let c = RenameTarget::Constraint { table: obj("table", "t"), constraint: "c".into() };
        assert!(matches!(script(false, &req(c, "d")), Err(Error::Unsupported(_))));
        let s = RenameTarget::Schema { database: None, schema: "s".into() };
        assert!(matches!(script(false, &req(s, "s2")), Err(Error::Unsupported(_))));
    }
}
