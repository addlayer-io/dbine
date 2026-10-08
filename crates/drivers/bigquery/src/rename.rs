//! "Renombrar…" on BigQuery: tables (`ALTER TABLE … RENAME TO`) and their
//! columns (`ALTER TABLE … RENAME COLUMN`). Views, materialized views and
//! routines can't be renamed, and neither can datasets.
//!
//! BigQuery doesn't update the views or routines that name what was
//! renamed: the app puts them back with `CREATE OR REPLACE`. Partitioning
//! and clustering columns, and the columns of a primary or foreign key,
//! can't be renamed: they're refused here from the table the app reads.
//! DDL doesn't run inside a transaction.

use crate::ddl::{self, ident};
use dbine_driver::rename::{quote_new, Fold, ReferenceStyle, RenameRequest, RenameSpec, RenameTarget, ReplaceStyle};
use dbine_driver::{kinds, Error, ObjectRef, Result, SyncScript, TableSchema};

pub(crate) fn spec() -> RenameSpec {
    RenameSpec {
        kinds: vec![kinds::TABLE.to_string()],
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
            "BigQuery no actualiza las vistas ni las rutinas que nombran lo renombrado: DBine las vuelve a crear con CREATE OR REPLACE. \
             Las sentencias DDL no corren dentro de una transacción. Al renombrar una tabla se pierden sus índices de búsqueda y \
             vectoriales; una tabla que recibe datos por streaming no se puede renombrar hasta que el streaming termina. \
             Las referencias escritas con el camino entero entre un solo par de comillas invertidas (`proyecto.dataset.tabla`) \
             no se detectan: hay que revisarlas."
                .into(),
        ),
        ..Default::default()
    }
}

fn table(o: &ObjectRef) -> String {
    match o.schema().filter(|s| !s.is_empty()) {
        Some(s) => format!("{}.{}", ident(s), ident(&o.name)),
        None => ident(&o.name),
    }
}

/// `expr` (a partitioning expression, a clustering list…) names `column`.
fn mentions(expr: &str, column: &str) -> bool {
    expr.split(|c: char| !(c.is_alphanumeric() || c == '_')).any(|w| w.eq_ignore_ascii_case(column))
}

/// Why BigQuery refuses to rename that column, if it does.
fn refused(t: &TableSchema, column: &str) -> Option<&'static str> {
    let opt = |k: &str| t.options.get(k).is_some_and(|e| mentions(e, column));
    let eq = |c: &String| c.eq_ignore_ascii_case(column);
    if opt(ddl::PARTITION_BY) {
        Some("es la columna de partición")
    } else if opt(ddl::CLUSTER_BY) {
        Some("es una columna de clustering")
    } else if t.primary_key.as_ref().is_some_and(|k| k.columns.iter().any(eq)) || t.foreign_keys.iter().any(|f| f.columns.iter().any(eq)) {
        Some("es parte de una clave primaria o foránea")
    } else {
        None
    }
}

pub(crate) fn script(req: &RenameRequest) -> Result<SyncScript> {
    let d = crate::script::dialect();
    let to = quote_new(&req.new_name, &d, Fold::None, false);
    let one = |sql: String| Ok(SyncScript { statements: vec![sql], warnings: Vec::new() });
    match &req.target {
        RenameTarget::Object { object, .. } => match object.kind.as_str() {
            kinds::TABLE => one(format!("ALTER TABLE {} RENAME TO {to};", table(object))),
            kinds::VIEW | kinds::MATERIALIZED_VIEW => Err(Error::Unsupported(
                "BigQuery no renombra vistas: hay que crearla con el nombre nuevo y borrar la anterior.".into(),
            )),
            k => Err(Error::Unsupported(format!("BigQuery no renombra objetos de tipo «{k}»."))),
        },
        RenameTarget::Column { table: t, column } => {
            if column.contains('.') {
                return Err(Error::Unsupported("BigQuery no renombra campos de un STRUCT, solo columnas de la tabla.".into()));
            }
            if let Some(why) = req.table.as_ref().and_then(|s| refused(s, column)) {
                return Err(Error::Unsupported(format!("BigQuery no renombra la columna «{column}»: {why}.")));
            }
            Ok(SyncScript {
                statements: vec![format!("ALTER TABLE {} RENAME COLUMN {} TO {to};", table(t), ident(column))],
                warnings: vec![
                    "Después de renombrar una columna, la tabla ya no se puede consultar con SQL heredado (legacy SQL) ni como parte de una tabla comodín. BigQuery tampoco renombra columnas de tablas con políticas de acceso por fila.".into(),
                ],
            })
        }
        RenameTarget::Schema { .. } => Err(Error::Unsupported("BigQuery no renombra datasets.".into())),
        RenameTarget::Index { .. } => Err(Error::Unsupported("BigQuery no renombra índices.".into())),
        RenameTarget::Constraint { .. } => Err(Error::Unsupported("BigQuery no renombra restricciones.".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ForeignKeyDef, KeyDef};

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: schema.map(Into::into), name: name.into() }
    }

    fn req(target: RenameTarget, new: &str, table: Option<TableSchema>) -> RenameRequest {
        RenameRequest { target, new_name: new.into(), table, definition: None }
    }

    fn col(table: Option<TableSchema>, column: &str, new: &str) -> Result<SyncScript> {
        script(&req(RenameTarget::Column { table: obj(kinds::TABLE, Some("ventas"), "pedidos"), column: column.into() }, new, table))
    }

    #[test]
    fn spec_renames_tables_and_columns() {
        let s = spec();
        assert_eq!(s.kinds, ["table"]);
        assert!(s.columns && !s.schemas && !s.indexes && !s.constraints && !s.transactional);
        assert_eq!((s.fold, s.replace), (Fold::None, ReplaceStyle::CreateOrReplace));
    }

    #[test]
    fn tables_keep_case_and_quote_with_backticks() {
        let r = |schema, new| req(RenameTarget::Object { object: obj(kinds::TABLE, schema, "Pedidos"), parent: None }, new, None);
        assert_eq!(script(&r(Some("ventas"), "Pedidos2")).unwrap().statements, ["ALTER TABLE `ventas`.`Pedidos` RENAME TO Pedidos2;"]);
        assert_eq!(script(&r(None, "mis-pedidos")).unwrap().statements, ["ALTER TABLE `Pedidos` RENAME TO `mis-pedidos`;"]);
        assert_eq!(script(&r(Some("ventas"), "select")).unwrap().statements, ["ALTER TABLE `ventas`.`Pedidos` RENAME TO `select`;"]);
    }

    #[test]
    fn columns() {
        let s = col(None, "pepe", "Pepa").unwrap();
        assert_eq!(s.statements, ["ALTER TABLE `ventas`.`pedidos` RENAME COLUMN `pepe` TO Pepa;"]);
        assert_eq!(s.warnings.len(), 1);
        assert!(matches!(col(None, "dir.calle", "c"), Err(Error::Unsupported(_))));
    }

    #[test]
    fn refuses_partition_cluster_and_key_columns() {
        let mut t = TableSchema { name: "pedidos".into(), schema: Some("ventas".into()), ..Default::default() };
        t.options.insert(ddl::PARTITION_BY.into(), "DATE(creado)".into());
        t.options.insert(ddl::CLUSTER_BY.into(), "cliente_id, estado".into());
        t.primary_key = Some(KeyDef { columns: vec!["id".into()], ..Default::default() });
        t.foreign_keys = vec![ForeignKeyDef { columns: vec!["vendedor".into()], ..Default::default() }];
        for c in ["creado", "estado", "id", "vendedor"] {
            assert!(matches!(col(Some(t.clone()), c, "x"), Err(Error::Unsupported(_))), "{c}");
        }
        assert!(col(Some(t), "pepe", "x").is_ok());
    }

    #[test]
    fn refuses_views_datasets_and_the_rest() {
        let v = req(RenameTarget::Object { object: obj(kinds::VIEW, Some("ventas"), "v"), parent: None }, "w", None);
        assert!(matches!(script(&v), Err(Error::Unsupported(_))));
        let f = req(RenameTarget::Object { object: obj(kinds::FUNCTION, Some("ventas"), "f"), parent: None }, "g", None);
        assert!(matches!(script(&f), Err(Error::Unsupported(_))));
        let ds = req(RenameTarget::Schema { database: None, schema: "ventas".into() }, "comercial", None);
        assert!(matches!(script(&ds), Err(Error::Unsupported(_))));
    }
}
