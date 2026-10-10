//! "Renombrar…" on Dremio: columns of Iceberg tables with `ALTER TABLE …
//! CHANGE COLUMN old new type`, and views by creating them again with the
//! new name and dropping the old one (Dremio has no `RENAME` for tables,
//! views, folders or spaces: its parser stops at `RENAME`).
//!
//! Views keep their SQL text and bind names when queried: the app rewrites
//! the ones that name the target and puts them back with `CREATE OR
//! REPLACE VIEW`. Dremio's DDL isn't transactional.

use crate::ddl::{path, q};
use dbine_driver::rename::{quote_new, rename_header, Fold, RenameRequest, RenameSpec, RenameTarget, ReferenceStyle, ReplaceStyle};
use dbine_driver::sql::split_script;
use dbine_driver::{kinds, Error, Result, SyncScript};

pub const NOTE: &str = "Dremio no tiene RENAME: las columnas se renombran con CHANGE COLUMN, solo en tablas Iceberg (como las de $scratch o de un catálogo Nessie); en archivos Parquet, JSON o de otros orígenes el servidor rechaza el cambio. Las vistas se renombran creándolas con el nombre nuevo y borrando la anterior. Las vistas que usan el objeto no se actualizan solas: DBine las reescribe y las repone con CREATE OR REPLACE VIEW. Las sentencias no son transaccionales. Solo se buscan dependientes en el mismo espacio u origen y sus carpetas: revisá después los de otros espacios.";

pub const VIEW_RECREATED: &str = "La vista se crea con el nombre nuevo y se borra la anterior: pierde sus reflexiones, su wiki, sus etiquetas y los permisos otorgados sobre ella.";

pub fn spec() -> Option<RenameSpec> {
    Some(RenameSpec {
        kinds: vec![kinds::VIEW.into()],
        columns: true,
        indexes: false,
        constraints: false,
        schemas: false,
        tracked: Vec::new(),
        replace: ReplaceStyle::CreateOrReplace,
        references: ReferenceStyle::Sql,
        fold: Fold::None,
        transactional: false,
        note: Some(NOTE.into()),
        ..Default::default()
    })
}

pub fn script(req: &RenameRequest) -> Result<SyncScript> {
    let dialect = crate::dialect();
    match &req.target {
        RenameTarget::Object { object, .. } if object.kind == kinds::VIEW => {
            let def = req.definition.as_deref().ok_or_else(|| Error::Query(format!("no se pudo leer la definición de la vista «{}»", object.name)))?;
            let renamed = rename_header(def, &dialect, Fold::None, &req.new_name)
                .ok_or_else(|| Error::Query(format!("no se reconoce el encabezado de la definición de la vista «{}»", object.name)))?;
            let create = format!("{};", plain_create(&renamed).trim().trim_end_matches(';').trim_end());
            let statements = vec![create, format!("DROP VIEW {};", path(object.schema(), &object.name))];
            one_unit_each(&statements, &format!("la definición de la vista «{}»", object.name))?;
            Ok(SyncScript { statements, warnings: vec![VIEW_RECREATED.into()] })
        }
        RenameTarget::Object { object, .. } if object.kind == kinds::TABLE => Err(Error::Unsupported(
            "Dremio no renombra tablas: creá una nueva con CREATE TABLE … AS SELECT y borrá la anterior".into(),
        )),
        RenameTarget::Object { .. } => Err(Error::Unsupported("Dremio solo renombra vistas y columnas".into())),
        RenameTarget::Column { table, column } => {
            let t = req.table.as_ref().ok_or_else(|| Error::Query(format!("no se pudo leer la definición de la tabla «{}» para renombrar la columna", table.name)))?;
            let c = t
                .columns
                .iter()
                .find(|c| c.name == *column)
                .or_else(|| t.columns.iter().find(|c| c.name.eq_ignore_ascii_case(column)))
                .ok_or_else(|| Error::Query(format!("la tabla «{}» no tiene la columna «{column}»", table.name)))?;
            // CHANGE COLUMN restates the type, and the catalog gives only
            // the outer word of a nested one.
            if ["STRUCT", "LIST", "MAP", "ROW", "ARRAY", "UNION"].contains(&c.data_type.trim().to_ascii_uppercase().as_str()) {
                return Err(Error::Unsupported(format!(
                    "la columna «{}» es de un tipo compuesto ({}) y CHANGE COLUMN necesita el tipo completo: renombrala con ALTER TABLE … CHANGE COLUMN escribiendo el tipo a mano",
                    c.name, c.data_type
                )));
            }
            let new = quote_new(&req.new_name, &dialect, Fold::None, false);
            let statements = vec![format!("ALTER TABLE {} CHANGE COLUMN {} {new} {};", path(table.schema(), &table.name), q(&c.name), c.data_type)];
            one_unit_each(&statements, &format!("el tipo de la columna «{}»", c.name))?;
            Ok(SyncScript { statements, warnings: vec![] })
        }
        RenameTarget::Index { .. } => Err(Error::Unsupported("Dremio no tiene índices: las reflexiones no se renombran".into())),
        RenameTarget::Constraint { .. } => Err(Error::Unsupported("Dremio no tiene restricciones con nombre".into())),
        RenameTarget::Schema { .. } => Err(Error::Unsupported("Dremio no renombra espacios, orígenes ni carpetas desde SQL".into())),
    }
}

/// Each statement must reach the server whole: `execute` cuts what it gets
/// with [`crate::dialect`], so a stored text whose `;` sits outside what
/// that dialect reads as a string or a comment (Dremio reading it as one)
/// would run what follows as a statement of its own.
fn one_unit_each(statements: &[String], what: &str) -> Result<()> {
    if statements.iter().any(|s| split_script(s, &crate::dialect()).len() > 1) {
        return Err(Error::Unsupported(format!(
            "{what} se partiría en varias sentencias al ejecutar el cambio de nombre, así que DBine no lo renombra: hacelo a mano"
        )));
    }
    Ok(())
}

/// `CREATE OR REPLACE VIEW` → `CREATE VIEW`: the new name must not
/// overwrite a view that already has it.
fn plain_create(def: &str) -> String {
    let lead = def.len() - def.trim_start().len();
    let words: Vec<(usize, &str)> = def[lead..].split_whitespace().take(3).map(|w| (w.as_ptr() as usize - def.as_ptr() as usize, w)).collect();
    match words.as_slice() {
        [(start, c), (_, o), (r_at, r)] if c.eq_ignore_ascii_case("create") && o.eq_ignore_ascii_case("or") && r.eq_ignore_ascii_case("replace") => {
            format!("{}CREATE{}", &def[..*start], &def[r_at + r.len()..])
        }
        _ => def.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ColumnDef, ObjectRef, TableSchema};

    fn obj(kind: &str, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: Some("ventas.crudo".into()), name: name.into() }
    }

    fn req(target: RenameTarget, new: &str) -> RenameRequest {
        RenameRequest { target, new_name: new.into(), table: None, definition: None }
    }

    #[test]
    fn spec_is_views_and_columns() {
        let s = spec().unwrap();
        assert_eq!(s.kinds, ["view"]);
        assert!(s.columns && !s.indexes && !s.constraints && !s.schemas && !s.transactional);
        assert_eq!((s.replace, s.fold), (ReplaceStyle::CreateOrReplace, Fold::None));
    }

    #[test]
    fn views_are_created_again() {
        let mut r = req(RenameTarget::Object { object: obj("view", "v"), parent: None }, "Ventas Netas");
        r.definition = Some("CREATE OR REPLACE VIEW \"ventas\".\"crudo\".\"v\" AS\nSELECT id FROM \"$scratch\".t;".into());
        let s = script(&r).unwrap();
        assert_eq!(s.statements, ["CREATE VIEW \"ventas\".\"crudo\".\"Ventas Netas\" AS\nSELECT id FROM \"$scratch\".t;", "DROP VIEW \"ventas\".\"crudo\".\"v\";"]);
        assert_eq!(s.warnings, [VIEW_RECREATED]);
        // The header keeps the old name's quotes.
        r.new_name = "Netas".into();
        assert_eq!(script(&r).unwrap().statements[0], "CREATE VIEW \"ventas\".\"crudo\".\"Netas\" AS\nSELECT id FROM \"$scratch\".t;");
        r.definition = None;
        assert!(matches!(script(&r), Err(Error::Query(_))));
    }

    #[test]
    fn columns_keep_their_type() {
        let mut r = req(RenameTarget::Column { table: obj("table", "t"), column: "pepe".into() }, "Nuevo Pepe");
        assert!(matches!(script(&r), Err(Error::Query(_))));
        r.table = Some(TableSchema {
            name: "t".into(),
            columns: vec![ColumnDef { name: "Pepe".into(), data_type: "DECIMAL(10, 2)".into(), ..Default::default() }],
            ..Default::default()
        });
        assert_eq!(script(&r).unwrap().statements, ["ALTER TABLE \"ventas\".\"crudo\".\"t\" CHANGE COLUMN \"Pepe\" \"Nuevo Pepe\" DECIMAL(10, 2);"]);
        r.new_name = "importe".into();
        assert_eq!(script(&r).unwrap().statements, ["ALTER TABLE \"ventas\".\"crudo\".\"t\" CHANGE COLUMN \"Pepe\" importe DECIMAL(10, 2);"]);
        r.table.as_mut().unwrap().columns[0].data_type = "STRUCT".into();
        assert!(matches!(script(&r), Err(Error::Unsupported(_))));
        r.target = RenameTarget::Column { table: obj("table", "t"), column: "nada".into() };
        assert!(matches!(script(&r), Err(Error::Query(_))));
    }

    #[test]
    fn refused() {
        for t in [
            RenameTarget::Object { object: obj("table", "t"), parent: None },
            RenameTarget::Index { table: obj("table", "t"), index: "r".into() },
            RenameTarget::Constraint { table: obj("table", "t"), constraint: "c".into() },
            RenameTarget::Schema { database: None, schema: "ventas".into() },
        ] {
            assert!(matches!(script(&req(t, "x")), Err(Error::Unsupported(_))));
        }
    }

    #[test]
    fn stored_text_that_would_split_is_refused() {
        let mut r = req(RenameTarget::Object { object: obj("view", "v"), parent: None }, "w");
        // A `;` outside strings and comments: the second statement would run alone.
        r.definition = Some("CREATE VIEW \"ventas\".\"crudo\".\"v\" AS SELECT 1 AS x; DROP TABLE \"$scratch\".secreta".into());
        assert!(matches!(script(&r), Err(Error::Unsupported(m)) if m.contains("«v»")));
        // Dremio reads `//` as a comment: the splitter must too, or the
        // `'` in it would hide the `;` that follows from the splitter.
        r.definition = Some("CREATE VIEW \"ventas\".\"crudo\".\"v\" AS SELECT 1 AS x // it's\n; DROP TABLE \"$scratch\".secreta".into());
        assert!(matches!(script(&r), Err(Error::Unsupported(_))));
        // A `;` inside a string, a `--` or a `//` comment stays in the view.
        r.definition = Some("CREATE VIEW \"ventas\".\"crudo\".\"v\" AS\nSELECT 'a;b' AS x -- c;d\n// e;f\nFROM \"$scratch\".t".into());
        let s = script(&r).unwrap();
        assert_eq!(s.statements[0], "CREATE VIEW \"ventas\".\"crudo\".\"w\" AS\nSELECT 'a;b' AS x -- c;d\n// e;f\nFROM \"$scratch\".t;");
        assert!(crate::dialect().slash_comments && !crate::dialect().backtick_idents);
    }

    #[test]
    fn a_column_type_that_would_split_is_refused() {
        let mut r = req(RenameTarget::Column { table: obj("table", "t"), column: "pepe".into() }, "nuevo");
        r.table = Some(TableSchema {
            name: "t".into(),
            columns: vec![ColumnDef { name: "pepe".into(), data_type: "INT; DROP TABLE \"$scratch\".secreta".into(), ..Default::default() }],
            ..Default::default()
        });
        assert!(matches!(script(&r), Err(Error::Unsupported(m)) if m.contains("«pepe»")));
        r.table.as_mut().unwrap().columns[0].data_type = "VARCHAR".into();
        assert_eq!(script(&r).unwrap().statements, ["ALTER TABLE \"ventas\".\"crudo\".\"t\" CHANGE COLUMN \"pepe\" nuevo VARCHAR;"]);
    }

    #[test]
    fn or_replace_is_dropped() {
        assert_eq!(plain_create("  create  or replace VIEW a AS SELECT 1"), "  CREATE VIEW a AS SELECT 1");
        assert_eq!(plain_create("CREATE VIEW a AS SELECT 1"), "CREATE VIEW a AS SELECT 1");
    }
}
