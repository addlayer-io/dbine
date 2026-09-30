//! Schema sync for Spanner (GoogleSQL): `ADD COLUMN`, `DROP COLUMN`,
//! `ALTER COLUMN c <type> [NOT NULL] [DEFAULT (…)]` (the whole column: what
//! is left out is removed), `DROP INDEX`, `DROP CONSTRAINT` for foreign
//! keys. The primary key can't change, nor identity or generated columns.
//!
//! The generic planner (MODIFY style, with `ALTER COLUMN`) runs on a copy
//! of the changes trimmed to that; what is left out becomes a warning.

use dbine_driver::alter::{self, AlterStyle, ColumnAlter, DropIndex};
use dbine_driver::sql::Quote;
use dbine_driver::{ColumnDef, DdlParts, Result, SyncScript, TableChange, TableSchema};

fn display(t: &TableSchema) -> String {
    match t.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    }
}

fn text(s: &Option<String>) -> Option<String> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn squash(t: &str) -> String {
    t.to_lowercase().split_whitespace().collect()
}

fn key_cols(t: &TableSchema) -> Vec<String> {
    t.primary_key.as_ref().map(|k| k.columns.iter().map(|c| c.to_lowercase()).collect()).unwrap_or_default()
}

fn generated(c: &ColumnDef) -> bool {
    c.options.get(crate::OPT_GENERATED).is_some_and(|g| !g.is_empty())
}

pub fn sync_script(changes: &[TableChange]) -> Result<SyncScript> {
    let mut warnings = Vec::new();
    let mut planned = Vec::with_capacity(changes.len());
    // Generated CHECK names pair by condition.
    let changes = &crate::structure::prepare_checks(changes);
    for ch in changes {
        planned.push(match ch {
            TableChange::Alter { old, new } => {
                let (old, mut new) = (old.clone(), new.clone());
                let tname = display(&new);
                let keys = key_cols(&old);
                if keys != key_cols(&new) {
                    warnings.push(format!(
                        "Spanner no cambia la clave primaria de {tname}: hay que crear otra tabla, copiar los datos y borrar esta."
                    ));
                }
                new.primary_key = old.primary_key.clone();
                for o in old.columns.iter().filter(|o| keys.contains(&o.name.to_lowercase())) {
                    if !new.columns.iter().any(|n| n.name.eq_ignore_ascii_case(&o.name)) {
                        warnings.push(format!("{tname}.{} es parte de la clave primaria: Spanner no la borra.", o.name));
                        new.columns.push(o.clone());
                    }
                }
                for n in new.columns.iter_mut() {
                    let Some(o) = old.columns.iter().find(|o| o.name.eq_ignore_ascii_case(&n.name)) else { continue };
                    let ty = squash(&o.data_type) != squash(&n.data_type);
                    let changes = ty || o.nullable != n.nullable || text(&o.default_value) != text(&n.default_value);
                    if keys.contains(&n.name.to_lowercase()) && changes {
                        warnings.push(format!("{tname}.{} es parte de la clave primaria: Spanner no la modifica; se deja como está.", n.name));
                    } else if (generated(o) || o.auto_increment) && changes {
                        warnings.push(format!("{tname}.{}: DBine no modifica columnas generadas o identidad en Spanner; se deja como está.", n.name));
                    } else {
                        if ty {
                            warnings.push(format!(
                                "{tname}.{}: Spanner solo cambia entre STRING y BYTES y el largo de STRING / BYTES (también en ARRAY); otro cambio de tipo falla.",
                                n.name
                            ));
                        }
                        if o.auto_increment != n.auto_increment || generated(o) != generated(n) {
                            warnings.push(format!("{tname}.{}: Spanner no convierte una columna en identidad o generada ni al revés; se deja como está.", n.name));
                        }
                        n.auto_increment = o.auto_increment;
                        n.options = o.options.clone();
                        continue;
                    }
                    *n = o.clone();
                }
                TableChange::Alter { old, new }
            }
            other => other.clone(),
        });
    }

    let cd = |_: &TableSchema, c: &ColumnDef| crate::column_def(c);
    let dd = |t: &TableSchema, p: DdlParts| Ok(crate::table_ddl(t, p));
    let st = AlterStyle {
        quote: Quote::Backtick,
        // The whole column after ALTER COLUMN: type, NOT NULL and DEFAULT go together.
        column: ColumnAlter::Modify { keyword: "ALTER COLUMN" },
        add_column: "ADD COLUMN",
        drop_index: DropIndex::Plain,
        drop_fk: "DROP CONSTRAINT",
        drop_pk_keyword: false,
        fk_inline: false,
        comment_on: false,
        column_def: &cd,
        table_ddl: &dd,
    };
    let mut s = alter::sync_script(&st, &planned)?;
    crate::structure::fix_script(&mut s, &planned);
    warnings.append(&mut s.warnings);
    s.warnings = warnings;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ForeignKeyDef, IndexDef, KeyDef};

    fn col(name: &str, ty: &str, nullable: bool) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable, ..Default::default() }
    }

    fn table(name: &str, cols: Vec<ColumnDef>) -> TableSchema {
        TableSchema { kind: "table".into(), name: name.into(), columns: cols, primary_key: Some(KeyDef { name: None, columns: vec!["Id".into()] }), ..Default::default() }
    }

    fn fk(name: &str, col: &str) -> ForeignKeyDef {
        ForeignKeyDef { name: Some(name.into()), columns: vec![col.into()], ref_schema: None, ref_table: "Otra".into(), ref_columns: vec!["Id".into()], on_delete: None, on_update: None }
    }

    #[test]
    fn alter_columns_indexes_and_fks() {
        let mut old = table("Clientes", vec![col("Id", "INT64", false), col("Nombre", "STRING(10)", true), col("Baja", "DATE", true), col("Estado", "STRING(5)", true), col("Ref", "INT64", true)]);
        old.columns[3].default_value = Some("'a'".into());
        old.indexes.push(IndexDef { name: "IxBaja".into(), columns: vec!["Baja".into()], unique: false, kind: None, filter: None, ..Default::default() });
        old.foreign_keys.push(fk("FkRef", "Ref"));
        let mut new = table("Clientes", vec![col("Id", "INT64", false), col("Nombre", "STRING(20)", false), col("Email", "STRING(MAX)", true), col("Estado", "STRING(5)", true), col("Ref", "INT64", true)]);
        new.columns[1].default_value = Some("'s/n'".into());
        new.indexes.push(IndexDef { name: "IxEmail".into(), columns: vec!["Email".into()], unique: true, kind: None, filter: None, ..Default::default() });
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE `Clientes` DROP CONSTRAINT `FkRef`;",
                "DROP INDEX `IxBaja`;",
                "ALTER TABLE `Clientes` DROP COLUMN `Baja`;",
                "ALTER TABLE `Clientes` ADD COLUMN `Email` STRING(MAX);",
                "ALTER TABLE `Clientes` ALTER COLUMN `Nombre` STRING(20) NOT NULL DEFAULT ('s/n');",
                "ALTER TABLE `Clientes` ALTER COLUMN `Estado` STRING(5);",
                "CREATE UNIQUE INDEX `IxEmail` ON `Clientes` (`Email`);",
            ]
        );
        let w = s.warnings.join("\n");
        assert!(w.contains("Clientes.Nombre pasa a NOT NULL"), "{w}");
        assert!(w.contains("Spanner solo cambia entre STRING y BYTES"), "{w}");
        assert!(w.contains("Se borra la columna Clientes.Baja"), "{w}");
    }

    #[test]
    fn primary_key_and_identity_stay() {
        let mut old = table("T", vec![col("Id", "INT64", false), col("N", "INT64", false)]);
        old.columns[1].auto_increment = true;
        let mut new = table("T", vec![col("Id", "STRING(10)", false), col("N", "STRING(10)", false)]);
        new.primary_key = Some(KeyDef { name: None, columns: vec!["N".into()] });
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert!(s.statements.is_empty(), "{:?}", s.statements);
        let w = s.warnings.join("\n");
        assert!(w.contains("no cambia la clave primaria de T"), "{w}");
        assert!(w.contains("T.Id es parte de la clave primaria"), "{w}");
        assert!(w.contains("T.N: DBine no modifica columnas generadas o identidad"), "{w}");
    }

    #[test]
    fn create_and_drop() {
        let mut a = table("Nueva", vec![col("Id", "INT64", false)]);
        a.indexes.push(IndexDef { name: "IxId".into(), columns: vec!["Id".into()], unique: false, kind: None, filter: None, ..Default::default() });
        a.foreign_keys.push(fk("FkN", "Id"));
        let mut b = table("Vieja", vec![col("Id", "INT64", false)]);
        b.indexes.push(IndexDef { name: "IxV".into(), columns: vec!["Id".into()], unique: false, kind: None, filter: None, ..Default::default() });
        let s = sync_script(&[TableChange::Create { table: a }, TableChange::Drop { table: b }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP INDEX `IxV`;\nDROP TABLE `Vieja`;",
                "CREATE TABLE `Nueva` (\n  `Id` INT64 NOT NULL\n) PRIMARY KEY (`Id`);",
                "CREATE INDEX `IxId` ON `Nueva` (`Id`);",
                "ALTER TABLE `Nueva` ADD CONSTRAINT `FkN` FOREIGN KEY (`Id`) REFERENCES `Otra` (`Id`);",
            ]
        );
    }
}
