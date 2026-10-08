//! "Renombrar…" on Databricks: tables (`ALTER TABLE … RENAME TO`), views
//! (`ALTER VIEW … RENAME TO`) and columns (`ALTER TABLE … RENAME COLUMN`).
//! Schemas, catalogs, functions and materialized views have no rename.
//!
//! The new name is qualified with the old one's schema: unqualified, it
//! would land in the session's current schema. Databricks doesn't update
//! the views or functions that name what was renamed: the app puts them back
//! with `CREATE OR REPLACE`. A column rename needs a Delta table with column
//! mapping (`delta.columnMapping.mode` = `name` or `id`), which the table
//! the app reads doesn't say: the statement carries a warning, and the
//! server refuses it before anything else in the script runs.

use crate::ddl::q;
use dbine_driver::rename::{quote_new, Fold, ReferenceStyle, RenameRequest, RenameSpec, RenameTarget, ReplaceStyle};
use dbine_driver::{kinds, Error, ObjectRef, Result, SyncScript};

/// The table property that turns column mapping on, if `database_schema`
/// ever reports it among a table's options.
const COLUMN_MAPPING: &str = "delta.columnMapping.mode";

pub(crate) fn spec() -> RenameSpec {
    RenameSpec {
        kinds: vec![kinds::TABLE.to_string(), kinds::VIEW.to_string()],
        columns: true,
        indexes: false,
        constraints: false,
        schemas: false,
        tracked: Vec::new(),
        replace: ReplaceStyle::CreateOrReplace,
        references: ReferenceStyle::Sql,
        fold: Fold::None,
        transactional: false,
        note: Some(
            "Databricks no actualiza las vistas ni las funciones que nombran lo renombrado: DBine las vuelve a crear con CREATE OR REPLACE. \
             Las sentencias DDL no corren dentro de una transacción. Renombrar una columna necesita una tabla Delta con column mapping \
             (delta.columnMapping.mode = 'name' o 'id'). Con AWS Glue como metastore no se puede renombrar."
                .into(),
        ),
        ..Default::default()
    }
}

fn qualified(o: &ObjectRef) -> String {
    crate::ddl::table_name(o.schema(), &o.name)
}

/// The new name in the old one's schema.
fn new_qualified(o: &ObjectRef, to: &str) -> String {
    match o.schema().filter(|s| !s.is_empty()) {
        Some(s) => format!("{}.{to}", q(s)),
        None => to.to_string(),
    }
}

pub(crate) fn script(req: &RenameRequest) -> Result<SyncScript> {
    let to = quote_new(&req.new_name, &crate::script::dialect(), Fold::None, false);
    let one = |sql: String| Ok(SyncScript { statements: vec![sql], warnings: Vec::new() });
    match &req.target {
        RenameTarget::Object { object, .. } => match object.kind.as_str() {
            kinds::TABLE => one(format!("ALTER TABLE {} RENAME TO {};", qualified(object), new_qualified(object, &to))),
            kinds::VIEW => one(format!("ALTER VIEW {} RENAME TO {};", qualified(object), new_qualified(object, &to))),
            k => Err(Error::Unsupported(format!("Databricks no renombra objetos de tipo «{k}»."))),
        },
        RenameTarget::Column { table, column } => {
            let mode = req.table.as_ref().and_then(|t| t.options.get(COLUMN_MAPPING)).map(|m| m.trim().to_ascii_lowercase());
            if mode.as_deref() == Some("none") {
                return Err(Error::Unsupported(format!(
                    "Databricks no renombra la columna «{column}»: la tabla no tiene column mapping. Se activa con ALTER TABLE … SET TBLPROPERTIES ('delta.columnMapping.mode' = 'name')."
                )));
            }
            let warnings = if matches!(mode.as_deref(), Some("name" | "id")) {
                Vec::new()
            } else {
                vec![
                    "Renombrar una columna necesita una tabla Delta con column mapping (delta.columnMapping.mode = 'name' o 'id'); si no lo tiene, Databricks rechaza la sentencia y el script se detiene antes de tocar nada. Se activa con ALTER TABLE … SET TBLPROPERTIES ('delta.columnMapping.mode' = 'name'), que puede afectar a quienes leen la tabla por streaming o con change data feed.".into(),
                ]
            };
            Ok(SyncScript { statements: vec![format!("ALTER TABLE {} RENAME COLUMN {} TO {to};", qualified(table), q(column))], warnings })
        }
        RenameTarget::Schema { .. } => Err(Error::Unsupported("Databricks no renombra esquemas.".into())),
        RenameTarget::Index { .. } => Err(Error::Unsupported("Databricks no tiene índices que se puedan renombrar.".into())),
        RenameTarget::Constraint { .. } => Err(Error::Unsupported("Databricks no renombra restricciones.".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::TableSchema;

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: schema.map(Into::into), name: name.into() }
    }

    fn req(target: RenameTarget, new: &str, table: Option<TableSchema>) -> RenameRequest {
        RenameRequest { target, new_name: new.into(), table, definition: None }
    }

    fn object(kind: &str, schema: Option<&str>, new: &str) -> Result<SyncScript> {
        script(&req(RenameTarget::Object { object: obj(kind, schema, "pedidos"), parent: None }, new, None))
    }

    #[test]
    fn spec_renames_tables_views_and_columns() {
        let s = spec();
        assert_eq!(s.kinds, ["table", "view"]);
        assert!(s.columns && !s.schemas && !s.indexes && !s.constraints && !s.transactional);
        assert_eq!((s.fold, s.replace), (Fold::None, ReplaceStyle::CreateOrReplace));
    }

    #[test]
    fn tables_stay_in_their_schema() {
        assert_eq!(object(kinds::TABLE, Some("ventas"), "Pedidos2").unwrap().statements, ["ALTER TABLE `ventas`.`pedidos` RENAME TO `ventas`.Pedidos2;"]);
        assert_eq!(object(kinds::TABLE, None, "mis pedidos").unwrap().statements, ["ALTER TABLE `pedidos` RENAME TO `mis pedidos`;"]);
        assert_eq!(object(kinds::TABLE, Some("ventas"), "table").unwrap().statements, ["ALTER TABLE `ventas`.`pedidos` RENAME TO `ventas`.`table`;"]);
    }

    #[test]
    fn views() {
        assert_eq!(object(kinds::VIEW, Some("ventas"), "v_pedidos").unwrap().statements, ["ALTER VIEW `ventas`.`pedidos` RENAME TO `ventas`.v_pedidos;"]);
    }

    #[test]
    fn columns_warn_about_column_mapping() {
        let col = |table: Option<TableSchema>| {
            script(&req(RenameTarget::Column { table: obj(kinds::TABLE, Some("ventas"), "pedidos"), column: "pepe".into() }, "Pepa", table))
        };
        let s = col(None).unwrap();
        assert_eq!(s.statements, ["ALTER TABLE `ventas`.`pedidos` RENAME COLUMN `pepe` TO Pepa;"]);
        assert_eq!(s.warnings.len(), 1);
        let with = |mode: &str| {
            let mut t = TableSchema { name: "pedidos".into(), ..Default::default() };
            t.options.insert(COLUMN_MAPPING.into(), mode.into());
            Some(t)
        };
        assert!(col(with("name")).unwrap().warnings.is_empty());
        assert!(matches!(col(with("none")), Err(Error::Unsupported(_))));
    }

    #[test]
    fn refuses_what_databricks_does_not_rename() {
        assert!(matches!(object(kinds::MATERIALIZED_VIEW, Some("ventas"), "x"), Err(Error::Unsupported(_))));
        assert!(matches!(object(kinds::FUNCTION, Some("ventas"), "x"), Err(Error::Unsupported(_))));
        let s = req(RenameTarget::Schema { database: Some("main".into()), schema: "ventas".into() }, "comercial", None);
        assert!(matches!(script(&s), Err(Error::Unsupported(_))));
    }
}
