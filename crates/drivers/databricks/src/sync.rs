//! Schema sync for Databricks (Delta): `ADD COLUMN` (nullable, without
//! default: Delta takes neither on a new column), `DROP COLUMN` (needs
//! column mapping), `ALTER COLUMN … TYPE` (widening only), `SET/DROP NOT
//! NULL`, `SET/DROP DEFAULT` (with the `allowColumnDefaults` table feature),
//! comments, and informational keys (`DROP PRIMARY KEY`, `DROP
//! CONSTRAINT`). No indexes.
//!
//! The generic planner runs on a copy of the changes trimmed to that; the
//! rest is added around it or becomes a warning.

use crate::ddl::{self, lit, q};
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

fn no_indexes(t: &mut TableSchema, known: &[String], warnings: &mut Vec<String>) {
    let tname = display(t);
    for ix in t.indexes.iter().filter(|i| !known.iter().any(|k| k.eq_ignore_ascii_case(&i.name))) {
        warnings.push(format!("Databricks no tiene índices: se omite {tname}.{}.", ix.name));
    }
    t.indexes.clear();
}

pub fn sync_script(changes: &[TableChange]) -> Result<SyncScript> {
    let mut warnings = Vec::new();
    let mut before = Vec::new();
    let mut after = Vec::new();
    let mut planned = Vec::with_capacity(changes.len());
    for ch in changes {
        planned.push(match ch {
            TableChange::Create { table } => {
                let mut table = table.clone();
                no_indexes(&mut table, &[], &mut warnings);
                TableChange::Create { table }
            }
            TableChange::Drop { table } => TableChange::Drop { table: table.clone() },
            TableChange::Alter { old, new } => {
                let (mut old, mut new) = (old.clone(), new.clone());
                let known: Vec<String> = old.indexes.iter().map(|i| i.name.clone()).collect();
                old.indexes.clear();
                no_indexes(&mut new, &known, &mut warnings);
                let tname = display(&new);
                let name = ddl::table_name(new.schema.as_deref(), &new.name);
                let mut defaults = false;
                if old.columns.iter().any(|o| !new.columns.iter().any(|n| n.name.eq_ignore_ascii_case(&o.name))) {
                    warnings.push(format!(
                        "{tname}: Delta solo borra columnas con column mapping activado; si no lo está, antes corré ALTER TABLE {name} SET TBLPROPERTIES ('delta.columnMapping.mode' = 'name')."
                    ));
                }
                for n in new.columns.iter_mut() {
                    let col = q(&n.name);
                    match old.columns.iter().find(|o| o.name.eq_ignore_ascii_case(&n.name)) {
                        None => {
                            if n.auto_increment {
                                warnings.push(format!("Delta no agrega columnas identidad a una tabla existente: {tname}.{} se agrega sin identidad.", n.name));
                                n.auto_increment = false;
                            }
                            if !n.nullable {
                                warnings.push(format!("Delta no agrega columnas NOT NULL: {tname}.{} se agrega aceptando NULL.", n.name));
                                n.nullable = true;
                            }
                            if let Some(d) = text(&n.default_value) {
                                warnings.push(format!(
                                    "{tname}.{}: el valor por defecto rige para las filas nuevas; las que ya están quedan en NULL.",
                                    n.name
                                ));
                                after.push(format!("ALTER TABLE {name} ALTER COLUMN {col} SET DEFAULT {d};"));
                                n.default_value = None;
                                defaults = true;
                            }
                        }
                        Some(o) => {
                            if squash(&o.data_type) != squash(&n.data_type) {
                                warnings.push(format!(
                                    "{tname}.{}: Delta solo cambia a tipos más amplios (con type widening activado); otro cambio de tipo falla.",
                                    n.name
                                ));
                            }
                            if text(&n.default_value).is_some() && text(&o.default_value) != text(&n.default_value) {
                                defaults = true;
                            }
                            if text(&o.comment) != text(&n.comment) {
                                after.push(format!("ALTER TABLE {name} ALTER COLUMN {col} COMMENT {};", lit(&text(&n.comment).unwrap_or_default())));
                            }
                        }
                    }
                }
                if defaults {
                    before.push(format!("ALTER TABLE {name} SET TBLPROPERTIES ('delta.feature.allowColumnDefaults' = 'supported');"));
                }
                if text(&old.comment) != text(&new.comment) {
                    after.push(format!("COMMENT ON TABLE {name} IS {};", text(&new.comment).map(|c| lit(&c)).unwrap_or_else(|| "NULL".into())));
                }
                TableChange::Alter { old, new }
            }
        });
    }

    // ADD COLUMN: name, type and comment (normalized above: no NOT NULL, default nor identity).
    let cd = |_: &TableSchema, c: &ColumnDef| {
        let mut l = format!("{} {}", q(&c.name), c.data_type);
        if let Some(cm) = text(&c.comment) {
            l.push_str(&format!(" COMMENT {}", lit(&cm)));
        }
        l
    };
    let dd = |t: &TableSchema, p: DdlParts| Ok(ddl::table_ddl(t, p));
    let st = AlterStyle {
        quote: Quote::Backtick,
        column: ColumnAlter::Standard { set_data_type: false, using_cast: false },
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
    before.append(&mut s.statements);
    before.append(&mut after);
    s.statements = before;
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
            schema: Some("ventas".into()),
            name: name.into(),
            columns: cols,
            primary_key: Some(KeyDef { name: Some("pk_c".into()), columns: vec!["id".into()] }),
            ..Default::default()
        }
    }

    #[test]
    fn alter_columns_defaults_and_comments() {
        let old = table("clientes", vec![col("id", "BIGINT", false), col("nombre", "INT", true), col("baja", "DATE", true)]);
        let mut new = table("clientes", vec![col("id", "BIGINT", false), col("nombre", "BIGINT", false), col("email", "STRING", false)]);
        new.columns[1].default_value = Some("0".into());
        new.columns[1].comment = Some("it's".into());
        new.columns[2].default_value = Some("'x'".into());
        new.indexes.push(IndexDef { name: "ix".into(), columns: vec!["email".into()], unique: false, kind: None, filter: None, ..Default::default() });
        new.comment = Some("Clientes".into());
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE `ventas`.`clientes` SET TBLPROPERTIES ('delta.feature.allowColumnDefaults' = 'supported');",
                "ALTER TABLE `ventas`.`clientes` DROP COLUMN `baja`;",
                "ALTER TABLE `ventas`.`clientes` ADD COLUMN `email` STRING;",
                "ALTER TABLE `ventas`.`clientes` ALTER COLUMN `nombre` TYPE BIGINT;",
                "ALTER TABLE `ventas`.`clientes` ALTER COLUMN `nombre` SET NOT NULL;",
                "ALTER TABLE `ventas`.`clientes` ALTER COLUMN `nombre` SET DEFAULT 0;",
                "ALTER TABLE `ventas`.`clientes` ALTER COLUMN `nombre` COMMENT 'it\\'s';",
                "ALTER TABLE `ventas`.`clientes` ALTER COLUMN `email` SET DEFAULT 'x';",
                "COMMENT ON TABLE `ventas`.`clientes` IS 'Clientes';",
            ]
        );
        let w = s.warnings.join("\n");
        assert!(w.contains("column mapping"), "{w}");
        assert!(w.contains("ventas.clientes.email se agrega aceptando NULL"), "{w}");
        assert!(w.contains("no tiene índices: se omite ventas.clientes.ix"), "{w}");
        assert!(w.contains("type widening"), "{w}");
    }

    #[test]
    fn keys() {
        let old = table("t", vec![col("id", "BIGINT", false), col("r", "BIGINT", true)]);
        let mut new = old.clone();
        new.primary_key = Some(KeyDef { name: Some("pk_t".into()), columns: vec!["id".into(), "r".into()] });
        new.foreign_keys.push(ForeignKeyDef { name: Some("fk_r".into()), columns: vec!["r".into()], ref_schema: None, ref_table: "o".into(), ref_columns: vec!["id".into()], on_delete: None, on_update: None });
        let s = sync_script(&[TableChange::Alter { old: new.clone(), new: old.clone() }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE `ventas`.`t` DROP CONSTRAINT `fk_r`;",
                "ALTER TABLE `ventas`.`t` DROP PRIMARY KEY;",
                "ALTER TABLE `ventas`.`t` ADD CONSTRAINT `pk_c` PRIMARY KEY (`id`);",
            ]
        );
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE `ventas`.`t` DROP PRIMARY KEY;",
                "ALTER TABLE `ventas`.`t` ADD CONSTRAINT `pk_t` PRIMARY KEY (`id`, `r`);",
                "ALTER TABLE `ventas`.`t` ADD CONSTRAINT `fk_r` FOREIGN KEY (`r`) REFERENCES `ventas`.`o` (`id`);",
            ]
        );
    }

    #[test]
    fn create_and_drop() {
        let a = table("nueva", vec![col("id", "BIGINT", false)]);
        let b = table("vieja", vec![col("id", "BIGINT", false)]);
        let s = sync_script(&[TableChange::Create { table: a }, TableChange::Drop { table: b }]).unwrap();
        assert_eq!(s.statements[0], "DROP TABLE `ventas`.`vieja`;");
        assert_eq!(s.statements[1], "CREATE TABLE `ventas`.`nueva` (\n  `id` BIGINT NOT NULL,\n  CONSTRAINT `pk_c` PRIMARY KEY (`id`)\n) USING DELTA;");
    }

    #[test]
    fn check_constraints() {
        let chk = |n: &str, e: &str| dbine_driver::CheckDef { name: Some(n.into()), expression: e.into() };
        let mut old = table("t", vec![col("id", "BIGINT", false), col("p", "INT", true)]);
        old.checks = vec![chk("p_pos", "p > 0"), chk("viejo", "id > 0")];
        let mut new = old.clone();
        new.checks = vec![chk("p_pos", "p >= 0"), chk("nuevo", "id < 100")];
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE `ventas`.`t` DROP CONSTRAINT `p_pos`;",
                "ALTER TABLE `ventas`.`t` DROP CONSTRAINT `viejo`;",
                "ALTER TABLE `ventas`.`t` ADD CONSTRAINT `p_pos` CHECK (p >= 0);",
                "ALTER TABLE `ventas`.`t` ADD CONSTRAINT `nuevo` CHECK (id < 100);",
            ]
        );
    }
}
