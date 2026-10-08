//! "Renombrar…" on Athena, conservatively:
//!
//! - Tables: `ALTER TABLE … RENAME TO`, which Athena takes only for Iceberg
//!   tables (Glue keeps external Hive tables by name and Athena lists the
//!   statement as unsupported for them): the server refuses the others.
//! - Columns of Iceberg tables: `ALTER TABLE … CHANGE COLUMN old new type`
//!   (Iceberg tracks columns by id, so the data follows). External tables
//!   are refused here: the change is metadata only, and a Parquet or ORC
//!   file read by name would then read the column as NULL.
//! - Views: no `ALTER VIEW`, so the view is created with the new name and
//!   the old one dropped.
//!
//! Views bind names when queried: the app rewrites the ones that name the
//! target and puts them back with `CREATE OR REPLACE VIEW`. The DDL (Hive)
//! takes backticks; views run on Trino and take double quotes. Glue keeps
//! names in lower case, with letters, digits and underscores only, so the
//! new name is checked for that and never needs quotes in the DDL.

use crate::ddl::{column, q, TABLE_TYPE};
use dbine_driver::rename::{quote_new, rename_header, Fold, RenameRequest, RenameSpec, RenameTarget, ReferenceStyle, ReplaceStyle};
use dbine_driver::sql::{qualified_name, Quote, ScriptDialect};
use dbine_driver::{kinds, ColumnDef, Error, ObjectRef, Result, SyncScript};

pub const NOTE: &str = "Athena renombra tablas y columnas solo si son Iceberg: con una tabla externa (Hive) el servidor rechaza el cambio de nombre de la tabla, y DBine no renombra sus columnas porque en Parquet u ORC dejarían de leer sus datos. Las vistas se renombran creándolas con el nombre nuevo y borrando la anterior. Las vistas que usan el objeto no se actualizan solas: DBine las reescribe y las repone con CREATE OR REPLACE VIEW. Las sentencias no son transaccionales.";

pub const VIEW_RECREATED: &str = "La vista se crea con el nombre nuevo y se borra la anterior: se pierden los permisos de Lake Formation otorgados sobre ella.";

pub fn spec() -> Option<RenameSpec> {
    Some(RenameSpec {
        kinds: vec![kinds::TABLE.into(), kinds::VIEW.into()],
        columns: true,
        indexes: false,
        constraints: false,
        schemas: false,
        tracked: Vec::new(),
        replace: ReplaceStyle::CreateOrReplace,
        references: ReferenceStyle::Sql,
        fold: Fold::Lower,
        transactional: false,
        note: Some(NOTE.into()),
        ..Default::default()
    })
}

fn dialect() -> ScriptDialect {
    ScriptDialect::for_hint("trino")
}

/// `` `db`.`name` `` for the Hive DDL.
fn ddl_name(o: &ObjectRef) -> String {
    qualified_name(Quote::Backtick, o.schema().filter(|s| !s.is_empty()), &o.name)
}

pub fn script(req: &RenameRequest) -> Result<SyncScript> {
    let new = req.new_name.as_str();
    if new.is_empty() || !new.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        return Err(Error::Unsupported(format!(
            "Athena (el catálogo de Glue) solo acepta nombres en minúsculas, con letras, números y guiones bajos: probá con «{}»",
            suggestion(new)
        )));
    }
    match &req.target {
        RenameTarget::Object { object, .. } if object.kind == kinds::TABLE => {
            let to = ObjectRef { name: new.into(), ..object.clone() };
            Ok(SyncScript { statements: vec![format!("ALTER TABLE {} RENAME TO {};", ddl_name(object), ddl_name(&to))], warnings: vec![] })
        }
        RenameTarget::Object { object, .. } if object.kind == kinds::VIEW => {
            let def = req.definition.as_deref().ok_or_else(|| Error::Query(format!("no se pudo leer la definición de la vista «{}»", object.name)))?;
            let renamed = rename_header(def, &dialect(), Fold::Lower, new)
                .ok_or_else(|| Error::Query(format!("no se reconoce el encabezado de la definición de la vista «{}»", object.name)))?;
            let create = format!("{};", renamed.trim().trim_end_matches(';').trim_end());
            // Bare where Glue's names allow it: DROP VIEW reads them on either engine.
            let bare = |n: &str| quote_new(n, &dialect(), Fold::Lower, false);
            let old = match object.schema().filter(|s| !s.is_empty()) {
                Some(s) => format!("{}.{}", bare(s), bare(&object.name)),
                None => bare(&object.name),
            };
            Ok(SyncScript { statements: vec![create, format!("DROP VIEW {old};")], warnings: vec![VIEW_RECREATED.into()] })
        }
        RenameTarget::Object { .. } => Err(Error::Unsupported("Athena solo renombra tablas, vistas y columnas".into())),
        RenameTarget::Column { table, column: name } => {
            let t = req.table.as_ref().ok_or_else(|| Error::Query(format!("no se pudo leer la definición de la tabla «{}» para renombrar la columna", table.name)))?;
            let kind = t.options.get(TABLE_TYPE).map(|v| v.trim().to_ascii_lowercase()).filter(|v| !v.is_empty()).unwrap_or_else(|| "iceberg".into());
            if kind != "iceberg" {
                return Err(Error::Unsupported(format!(
                    "Athena solo renombra columnas de tablas Iceberg: «{}» es una tabla externa ({kind}) y en ella el cambio es solo de metadatos, así que en Parquet u ORC la columna dejaría de leer sus datos",
                    table.name
                )));
            }
            let c = t
                .columns
                .iter()
                .find(|c| c.name == *name)
                .or_else(|| t.columns.iter().find(|c| c.name.eq_ignore_ascii_case(name)))
                .ok_or_else(|| Error::Query(format!("la tabla «{}» no tiene la columna «{name}»", table.name)))?;
            let renamed = column(&ColumnDef { name: new.into(), ..c.clone() });
            Ok(SyncScript { statements: vec![format!("ALTER TABLE {} CHANGE COLUMN {} {renamed};", ddl_name(table), q(&c.name))], warnings: vec![] })
        }
        RenameTarget::Index { .. } => Err(Error::Unsupported("Athena no tiene índices".into())),
        RenameTarget::Constraint { .. } => Err(Error::Unsupported("Athena no tiene restricciones".into())),
        RenameTarget::Schema { .. } => Err(Error::Unsupported("Athena no renombra bases de datos".into())),
    }
}

/// The name as Glue would take it.
fn suggestion(name: &str) -> String {
    name.trim().to_lowercase().chars().map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() { c } else { '_' }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::TableSchema;

    fn obj(kind: &str, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: None, name: name.into() }
    }

    fn req(target: RenameTarget, new: &str) -> RenameRequest {
        RenameRequest { target, new_name: new.into(), table: None, definition: None }
    }

    fn table(kind: &str) -> TableSchema {
        let mut t = TableSchema {
            name: "ventas".into(),
            columns: vec![ColumnDef { name: "importe".into(), data_type: "decimal(10,2)".into(), comment: Some("El importe".into()), ..Default::default() }],
            ..Default::default()
        };
        t.options.insert(TABLE_TYPE.into(), kind.into());
        t
    }

    #[test]
    fn spec_is_tables_views_and_columns() {
        let s = spec().unwrap();
        assert_eq!(s.kinds, ["table", "view"]);
        assert!(s.columns && !s.indexes && !s.constraints && !s.schemas && !s.transactional);
        assert_eq!((s.replace, s.fold), (ReplaceStyle::CreateOrReplace, Fold::Lower));
    }

    #[test]
    fn tables() {
        assert_eq!(script(&req(RenameTarget::Object { object: obj("table", "ventas"), parent: None }, "ventas_2024")).unwrap().statements, ["ALTER TABLE `ventas` RENAME TO `ventas_2024`;"]);
        let qualified = ObjectRef { schema: Some("crudo".into()), ..obj("table", "ventas") };
        assert_eq!(script(&req(RenameTarget::Object { object: qualified, parent: None }, "v2")).unwrap().statements, ["ALTER TABLE `crudo`.`ventas` RENAME TO `crudo`.`v2`;"]);
    }

    #[test]
    fn names_glue_takes() {
        for bad in ["Ventas", "ventas netas", "ventas-2024", ""] {
            assert!(matches!(script(&req(RenameTarget::Object { object: obj("table", "t"), parent: None }, bad)), Err(Error::Unsupported(_))), "{bad}");
        }
        assert_eq!(suggestion("Ventas Netas-2024"), "ventas_netas_2024");
    }

    #[test]
    fn views_are_created_again() {
        let mut r = req(RenameTarget::Object { object: obj("view", "v"), parent: None }, "select");
        r.definition = Some("CREATE VIEW \"v\" AS\nSELECT importe FROM ventas".into());
        let s = script(&r).unwrap();
        assert_eq!(s.statements, ["CREATE VIEW \"select\" AS\nSELECT importe FROM ventas;", "DROP VIEW v;"]);
        assert_eq!(s.warnings, [VIEW_RECREATED]);
        r.definition = Some("CREATE VIEW crudo.v AS SELECT 1".into());
        r.new_name = "w".into();
        assert_eq!(script(&r).unwrap().statements[0], "CREATE VIEW crudo.w AS SELECT 1;");
        r.definition = None;
        assert!(matches!(script(&r), Err(Error::Query(_))));
    }

    #[test]
    fn columns_of_iceberg_tables_only() {
        let target = || RenameTarget::Column { table: obj("table", "ventas"), column: "importe".into() };
        let mut r = req(target(), "monto");
        assert!(matches!(script(&r), Err(Error::Query(_))));
        r.table = Some(table("iceberg"));
        assert_eq!(script(&r).unwrap().statements, ["ALTER TABLE `ventas` CHANGE COLUMN `importe` `monto` decimal(10,2) COMMENT 'El importe';"]);
        r.table = Some(table("parquet"));
        assert!(matches!(script(&r), Err(Error::Unsupported(_))));
    }

    #[test]
    fn refused() {
        for t in [
            RenameTarget::Object { object: obj("function", "f"), parent: None },
            RenameTarget::Index { table: obj("table", "t"), index: "i".into() },
            RenameTarget::Constraint { table: obj("table", "t"), constraint: "c".into() },
            RenameTarget::Schema { database: None, schema: "crudo".into() },
        ] {
            assert!(matches!(script(&req(t, "x")), Err(Error::Unsupported(_))));
        }
    }
}
