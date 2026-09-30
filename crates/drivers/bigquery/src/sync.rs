//! Schema sync for BigQuery: `ADD COLUMN` (always NULLABLE), `DROP COLUMN`,
//! `ALTER COLUMN … SET DATA TYPE` (coercible widening only), `DROP NOT
//! NULL` (there is no SET NOT NULL), `SET/DROP DEFAULT`, descriptions with
//! `SET OPTIONS`, and informational keys: `ADD PRIMARY KEY (…) NOT
//! ENFORCED`, `DROP PRIMARY KEY`, `DROP CONSTRAINT` for foreign keys. No
//! indexes.
//!
//! The generic planner runs on a copy of the changes trimmed to that; the
//! rest is added after it or becomes a warning.

use crate::ddl::{self, ident, lit};
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

/// Leaves the search and vector indexes (BigQuery's only ones) when `keep`.
fn no_indexes(t: &mut TableSchema, known: &[String], keep: bool, warnings: &mut Vec<String>) {
    let tname = display(t);
    for ix in t.indexes.iter().filter(|i| !crate::indexes::supported(i) && !known.iter().any(|k| k.eq_ignore_ascii_case(&i.name))) {
        warnings.push(format!("BigQuery solo tiene índices de búsqueda y vectoriales: se omite {tname}.{}.", ix.name));
    }
    t.indexes.retain(|i| keep && crate::indexes::supported(i));
}

pub fn sync_script(changes: &[TableChange]) -> Result<SyncScript> {
    let mut warnings = Vec::new();
    let mut after = Vec::new();
    let mut planned = Vec::with_capacity(changes.len());
    for ch in changes {
        planned.push(match ch {
            TableChange::Create { table } => {
                let mut table = table.clone();
                no_indexes(&mut table, &[], true, &mut warnings);
                TableChange::Create { table }
            }
            TableChange::Drop { table } => TableChange::Drop { table: table.clone() },
            TableChange::Alter { old, new } => {
                let (mut old, mut new) = (old.clone(), new.clone());
                let known: Vec<String> = old.indexes.iter().map(|i| i.name.clone()).collect();
                // Search and vector indexes are planned apart (`indexes::plan`).
                old.indexes.clear();
                no_indexes(&mut new, &known, false, &mut warnings);
                // The key has no name in BigQuery.
                for k in [&mut old.primary_key, &mut new.primary_key].into_iter().flatten() {
                    k.name = None;
                }
                let tname = display(&new);
                let name = ddl::table_name(&new);
                for n in new.columns.iter_mut() {
                    let col = ident(&n.name);
                    match old.columns.iter().find(|o| o.name.eq_ignore_ascii_case(&n.name)) {
                        None => {
                            if !n.nullable {
                                warnings.push(format!(
                                    "BigQuery no agrega columnas NOT NULL a una tabla existente: {tname}.{} se agrega como NULLABLE.",
                                    n.name
                                ));
                                n.nullable = true;
                            }
                            // ADD COLUMN takes no default: it goes right after.
                            if let Some(d) = text(&n.default_value) {
                                warnings.push(format!(
                                    "{tname}.{}: el valor por defecto rige para las filas nuevas; las que ya están quedan en NULL.",
                                    n.name
                                ));
                                after.push(format!("ALTER TABLE {name} ALTER COLUMN {col} SET DEFAULT {d};"));
                                n.default_value = None;
                            }
                        }
                        Some(o) => {
                            if squash(&o.data_type) != squash(&n.data_type) {
                                warnings.push(format!(
                                    "{tname}.{}: BigQuery solo cambia a tipos más amplios (INT64 → NUMERIC → BIGNUMERIC → FLOAT64, más largo en STRING(n) / BYTES(n), más precisión); otro cambio falla.",
                                    n.name
                                ));
                            }
                            if o.nullable && !n.nullable {
                                warnings.push(format!(
                                    "BigQuery no pasa una columna existente a NOT NULL: {tname}.{} se deja NULLABLE.",
                                    n.name
                                ));
                                n.nullable = true;
                            }
                            n.auto_increment = o.auto_increment;
                            if text(&o.comment) != text(&n.comment) {
                                let v = text(&n.comment).map(|c| lit(&c)).unwrap_or_else(|| "NULL".into());
                                after.push(format!("ALTER TABLE {name} ALTER COLUMN {col} SET OPTIONS (description={v});"));
                            }
                        }
                    }
                }
                if text(&old.comment) != text(&new.comment) {
                    let v = text(&new.comment).map(|c| lit(&c)).unwrap_or_else(|| "NULL".into());
                    after.push(format!("ALTER TABLE {name} SET OPTIONS (description={v});"));
                }
                TableChange::Alter { old, new }
            }
        });
    }

    // Added columns go without their default (above).
    let cd = |_: &TableSchema, c: &ColumnDef| ddl::column_def(&ColumnDef { default_value: None, ..c.clone() });
    let dd = |t: &TableSchema, p: DdlParts| Ok(ddl::table_ddl(t, p));
    let st = AlterStyle {
        quote: Quote::Backtick,
        column: ColumnAlter::Standard { set_data_type: true, using_cast: false },
        add_column: "ADD COLUMN",
        drop_index: DropIndex::Plain,
        drop_fk: "DROP CONSTRAINT",
        drop_pk_keyword: true,
        fk_inline: false,
        comment_on: false,
        column_def: &cd,
        table_ddl: &dd,
    };
    let mut s = alter::sync_script(&st, &planned)?;
    crate::indexes::plan(changes, &mut s);
    for stmt in s.statements.iter_mut() {
        if stmt.starts_with("ALTER TABLE ") && stmt.contains(" ADD PRIMARY KEY (") && stmt.ends_with(");") {
            stmt.pop();
            stmt.push_str(" NOT ENFORCED;");
        }
    }
    s.statements.append(&mut after);
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
        TableSchema {
            kind: "table".into(),
            schema: Some("ds".into()),
            name: name.into(),
            columns: cols,
            primary_key: Some(KeyDef { name: None, columns: vec!["id".into()] }),
            ..Default::default()
        }
    }

    fn fk(name: &str) -> ForeignKeyDef {
        ForeignKeyDef { name: Some(name.into()), columns: vec!["id".into()], ref_schema: None, ref_table: "otra".into(), ref_columns: vec!["id".into()], on_delete: None, on_update: None }
    }

    #[test]
    fn alter_columns_keys_and_options() {
        let mut old = table("clientes", vec![col("id", "INT64", false), col("nombre", "STRING(10)", false), col("baja", "DATE", true), col("monto", "INT64", true)]);
        old.foreign_keys.push(fk("fk_vieja"));
        let mut new = table("clientes", vec![col("id", "INT64", false), col("nombre", "STRING(20)", true), col("email", "STRING", false), col("monto", "INT64", false)]);
        new.columns[1].default_value = Some("'s/n'".into());
        new.columns[1].comment = Some("El nombre".into());
        new.columns[2].default_value = Some("'x@y'".into());
        new.foreign_keys.push(ForeignKeyDef { ref_table: "otra2".into(), ..fk("fk_nueva") });
        new.indexes.push(IndexDef { name: "ix".into(), columns: vec!["email".into()], unique: false, kind: None, filter: None, ..Default::default() });
        new.comment = Some("Clientes".into());
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE `ds`.`clientes` DROP CONSTRAINT `fk_vieja`;",
                "ALTER TABLE `ds`.`clientes` DROP COLUMN `baja`;",
                "ALTER TABLE `ds`.`clientes` ADD COLUMN `email` STRING;",
                "ALTER TABLE `ds`.`clientes` ALTER COLUMN `nombre` SET DATA TYPE STRING(20);",
                "ALTER TABLE `ds`.`clientes` ALTER COLUMN `nombre` DROP NOT NULL;",
                "ALTER TABLE `ds`.`clientes` ALTER COLUMN `nombre` SET DEFAULT 's/n';",
                "ALTER TABLE `ds`.`clientes` ADD CONSTRAINT `fk_nueva` FOREIGN KEY (`id`) REFERENCES `ds`.`otra2`(`id`) NOT ENFORCED;",
                "ALTER TABLE `ds`.`clientes` ALTER COLUMN `nombre` SET OPTIONS (description='El nombre');",
                "ALTER TABLE `ds`.`clientes` ALTER COLUMN `email` SET DEFAULT 'x@y';",
                "ALTER TABLE `ds`.`clientes` SET OPTIONS (description='Clientes');",
            ]
        );
        let w = s.warnings.join("\n");
        assert!(w.contains("solo tiene índices de búsqueda y vectoriales: se omite ds.clientes.ix"), "{w}");
        assert!(w.contains("ds.clientes.email se agrega como NULLABLE"), "{w}");
        assert!(w.contains("ds.clientes.monto se deja NULLABLE"), "{w}");
        assert!(w.contains("ds.clientes.nombre: BigQuery solo cambia a tipos más amplios"), "{w}");
        assert!(w.contains("Se borra la columna ds.clientes.baja"), "{w}");
    }

    #[test]
    fn primary_key_is_not_enforced() {
        let old = table("t", vec![col("id", "INT64", false), col("b", "INT64", false)]);
        let mut new = old.clone();
        new.primary_key = Some(KeyDef { name: Some("pk".into()), columns: vec!["id".into(), "b".into()] });
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec!["ALTER TABLE `ds`.`t` DROP PRIMARY KEY;", "ALTER TABLE `ds`.`t` ADD PRIMARY KEY (`id`, `b`) NOT ENFORCED;"]
        );
    }

    #[test]
    fn create_and_drop() {
        let mut a = table("nueva", vec![col("id", "INT64", false)]);
        a.foreign_keys.push(fk("fk"));
        let b = table("vieja", vec![col("id", "INT64", false)]);
        let s = sync_script(&[TableChange::Create { table: a }, TableChange::Drop { table: b }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP TABLE `ds`.`vieja`;",
                "CREATE TABLE `ds`.`nueva` (\n  `id` INT64 NOT NULL,\n  PRIMARY KEY (`id`) NOT ENFORCED\n);",
                "ALTER TABLE `ds`.`nueva` ADD CONSTRAINT `fk` FOREIGN KEY (`id`) REFERENCES `ds`.`otra`(`id`) NOT ENFORCED;",
            ]
        );
    }
}
