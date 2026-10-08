//! "Renombrar…" in TDengine: columns of normal tables
//! (`ALTER TABLE db.t RENAME COLUMN a b`, the timestamp one included) and
//! tags of supertables (`ALTER STABLE db.m RENAME TAG a b`, its tag index
//! follows it). Supertable columns, subtables, tables, views, streams,
//! topics and databases can't be renamed.
//!
//! The streams that name the column or tag are rewritten, dropped before
//! the rename and created again after it (`DropCreate`) over the same
//! output table, which the server allows. The server refuses a tag a
//! stream uses, so dropping it first is what lets the rename through. A
//! column or tag a topic uses is still refused: topics aren't rewritten.

use crate::ddl::{q, qualified, SUBTABLE, SUPERTABLE, TAG};
use dbine_driver::rename::{quote_new, Fold, ReferenceStyle, RenameRequest, RenameSpec, RenameTarget, ReplaceStyle};
use dbine_driver::{kinds, Error, Result, ScriptDialect, SyncScript};

const NOTE: &str = "TDengine renombra columnas de tablas comunes y tags de supertablas; no renombra tablas, supertablas, \
columnas de supertablas ni bases de datos. Los streams que usan el nombre se borran antes del cambio y se vuelven a crear después, \
reescritos, sobre la misma tabla de salida; lo que llegue mientras tanto no lo procesan.";
const TAG_USERS: &str =
    "Si un tópico usa el tag, el servidor rechaza el cambio: hay que borrarlo, renombrar y volver a crearlo con el nombre nuevo.";
const COLUMN_USERS: &str =
    "Si un tópico usa la columna, el servidor rechaza el cambio: hay que borrarlo, renombrar y volver a crearlo con el nombre nuevo.";

/// Backticks for names (strings take backslash escapes in TDengine).
fn dialect() -> ScriptDialect {
    ScriptDialect { backslash_escapes: true, ..ScriptDialect::generic() }
}

pub(crate) fn spec() -> RenameSpec {
    RenameSpec {
        kinds: Vec::new(),
        columns: true,
        indexes: false,
        constraints: false,
        schemas: false,
        tracked: Vec::new(),
        // No CREATE OR REPLACE STREAM: dropped, renamed, created again.
        replace: ReplaceStyle::DropCreate,
        references: ReferenceStyle::Sql,
        fold: Fold::Lower,
        transactional: false,
        note: Some(NOTE.into()),
        ..Default::default()
    }
}

/// The statements that rename the target.
pub(crate) fn script(req: &RenameRequest) -> Result<SyncScript> {
    match &req.target {
        RenameTarget::Column { table, column } => {
            let new = quote_new(&req.new_name, &dialect(), Fold::Lower, false);
            let name = qualified(table.schema(), &table.name);
            if table.kind == SUBTABLE {
                return Err(Error::Unsupported(format!(
                    "«{}» es una subtabla: sus columnas y tags son los de su supertabla. Los tags se renombran en la supertabla.",
                    table.name
                )));
            }
            let def = req.table.as_ref().and_then(|t| t.columns.iter().find(|c| &c.name == column));
            let is_tag = def.map(|c| c.options.get(TAG).is_some_and(|v| v == "true" || v == "1"));
            let stable = table.kind == SUPERTABLE || req.table.as_ref().is_some_and(|t| t.kind == SUPERTABLE);
            if stable {
                if is_tag == Some(false) {
                    return Err(Error::Unsupported(format!(
                        "TDengine solo renombra los tags de una supertabla; «{column}» es una columna. Las columnas se renombran solo en tablas comunes."
                    )));
                }
                let statement = format!("ALTER STABLE {name} RENAME TAG {} {new};", q(column));
                return Ok(SyncScript { statements: vec![statement], warnings: vec![TAG_USERS.into()] });
            }
            let statement = format!("ALTER TABLE {name} RENAME COLUMN {} {new};", q(column));
            Ok(SyncScript { statements: vec![statement], warnings: vec![COLUMN_USERS.into()] })
        }
        RenameTarget::Object { object, .. } if object.kind == kinds::TABLE || object.kind == SUPERTABLE || object.kind == SUBTABLE => {
            Err(Error::Unsupported("TDengine no renombra tablas: hay que crear una con el nombre nuevo y copiar los datos.".into()))
        }
        RenameTarget::Object { .. } => {
            Err(Error::Unsupported("TDengine no renombra vistas, streams ni tópicos: hay que crearlos con el nombre nuevo y borrar los anteriores.".into()))
        }
        RenameTarget::Index { .. } => Err(Error::Unsupported("TDengine no renombra índices: hay que borrarlo y crearlo con el nombre nuevo.".into())),
        RenameTarget::Constraint { .. } => Err(Error::Unsupported("TDengine no tiene restricciones con nombre.".into())),
        RenameTarget::Schema { .. } => Err(Error::Unsupported("TDengine no renombra bases de datos.".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ColumnDef, ObjectRef, TableSchema};

    fn obj(kind: &str, name: &str) -> ObjectRef {
        ObjectRef { kind: kind.into(), schema: Some("db".into()), name: name.into() }
    }

    fn stable() -> TableSchema {
        let col = |n: &str, tag: bool| ColumnDef {
            name: n.into(),
            data_type: "INT".into(),
            options: if tag { [(TAG.to_string(), "true".to_string())].into() } else { Default::default() },
            ..Default::default()
        };
        TableSchema { schema: Some("db".into()), name: "m".into(), kind: SUPERTABLE.into(), columns: vec![col("ts", false), col("v", false), col("loc", true)], ..Default::default() }
    }

    fn column(table: ObjectRef, c: &str, new: &str, schema: Option<TableSchema>) -> RenameRequest {
        RenameRequest { target: RenameTarget::Column { table, column: c.into() }, new_name: new.into(), table: schema, definition: None }
    }

    #[test]
    fn spec_renames_columns_only() {
        let s = spec();
        assert!(s.columns && s.kinds.is_empty() && !s.indexes && !s.constraints && !s.schemas && !s.transactional);
        assert_eq!((s.references, s.replace), (ReferenceStyle::Sql, ReplaceStyle::DropCreate));
        assert_eq!(s.fold, Fold::Lower);
    }

    #[test]
    fn normal_table_column() {
        let r = script(&column(obj(kinds::TABLE, "n"), "pepe", "pepa", None)).unwrap();
        assert_eq!(r.statements, vec!["ALTER TABLE `db`.`n` RENAME COLUMN `pepe` pepa;"]);
        assert_eq!(r.warnings, vec![COLUMN_USERS.to_string()]);
        // Mixed case and spaces keep their case in backticks; unquoted names fold to lower case.
        let r = script(&column(obj(kinds::TABLE, "n"), "Old`x", "Mixed Name", None)).unwrap();
        assert_eq!(r.statements, vec!["ALTER TABLE `db`.`n` RENAME COLUMN `Old``x` `Mixed Name`;"]);
        let r = script(&column(obj(kinds::TABLE, "n"), "a", "Pepa", None)).unwrap();
        assert_eq!(r.statements, vec!["ALTER TABLE `db`.`n` RENAME COLUMN `a` `Pepa`;"]);
    }

    #[test]
    fn supertable_tag_and_refused_column() {
        let r = script(&column(obj(SUPERTABLE, "m"), "loc", "lugar", Some(stable()))).unwrap();
        assert_eq!(r.statements, vec!["ALTER STABLE `db`.`m` RENAME TAG `loc` lugar;"]);
        assert_eq!(r.warnings, vec![TAG_USERS.to_string()]);
        assert!(matches!(script(&column(obj(SUPERTABLE, "m"), "v", "w", Some(stable()))), Err(Error::Unsupported(m)) if m.contains("«v» es una columna")));
    }

    #[test]
    fn subtable_and_other_targets_refused() {
        assert!(matches!(script(&column(obj(SUBTABLE, "c1"), "loc", "x", None)), Err(Error::Unsupported(m)) if m.contains("subtabla")));
        for target in [
            RenameTarget::Object { object: obj(kinds::TABLE, "n"), parent: None },
            RenameTarget::Object { object: obj(SUPERTABLE, "m"), parent: None },
            RenameTarget::Object { object: obj(kinds::STREAM, "s"), parent: None },
            RenameTarget::Index { table: obj(SUPERTABLE, "m"), index: "ix".into() },
            RenameTarget::Constraint { table: obj(kinds::TABLE, "n"), constraint: "c".into() },
            RenameTarget::Schema { database: Some("db".into()), schema: "db".into() },
        ] {
            let req = RenameRequest { target, new_name: "x".into(), table: None, definition: None };
            assert!(matches!(script(&req), Err(Error::Unsupported(_))));
        }
    }
}
