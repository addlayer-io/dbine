//! Schema sync for Aurora DSQL: PostgreSQL's ALTER TABLE, cut down to what
//! DSQL takes. `ADD COLUMN` goes with name and type only (the default is
//! set right after), `DROP COLUMN` (not on key columns), `DROP NOT NULL`,
//! `SET/DROP DEFAULT`, `DROP INDEX` and `CREATE INDEX ASYNC`. There is no
//! type change, SET NOT NULL nor primary key change; foreign keys are left
//! out, as in the rest of the driver.
//!
//! The generic planner runs on a copy of the changes trimmed to that; what
//! is left out becomes a warning.

use dbine_driver::alter::{self, AlterStyle, ColumnAlter};
use dbine_driver::ddl::SqlFlavor;
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
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

fn no_fks(t: &mut TableSchema, warnings: &mut Vec<String>) {
    if !t.foreign_keys.is_empty() {
        warnings.push(format!("DBine no crea claves foráneas en DSQL: se omiten las de {}.", display(t)));
        t.foreign_keys.clear();
    }
}

pub fn sync_script(changes: &[TableChange]) -> Result<SyncScript> {
    let mut warnings = Vec::new();
    let mut after = Vec::new();
    let mut planned = Vec::with_capacity(changes.len());
    for ch in changes {
        planned.push(match ch {
            TableChange::Create { table } => {
                let mut table = table.clone();
                no_fks(&mut table, &mut warnings);
                TableChange::Create { table }
            }
            TableChange::Drop { table } => TableChange::Drop { table: table.clone() },
            TableChange::Alter { old, new } => {
                let (mut old, mut new) = (old.clone(), new.clone());
                old.foreign_keys.clear();
                no_fks(&mut new, &mut warnings);
                let tname = display(&new);
                let name = qualified_name(Quote::Double, new.schema.as_deref().filter(|s| !s.is_empty()), &new.name);
                let keys = key_cols(&old);
                if keys != key_cols(&new) {
                    warnings.push(format!("DSQL no cambia la clave primaria de {tname}: hay que recrear la tabla y copiar los datos."));
                }
                new.primary_key = old.primary_key.clone();
                for o in old.columns.iter().filter(|o| keys.contains(&o.name.to_lowercase())) {
                    if !new.columns.iter().any(|n| n.name.eq_ignore_ascii_case(&o.name)) {
                        warnings.push(format!("{tname}.{} es parte de la clave primaria: DSQL no la borra.", o.name));
                        new.columns.push(o.clone());
                    }
                }
                for n in new.columns.iter_mut() {
                    match old.columns.iter().find(|o| o.name.eq_ignore_ascii_case(&n.name)) {
                        None => {
                            if n.auto_increment {
                                warnings.push(format!("{tname}.{}: DBine no crea columnas identidad en DSQL; se agrega sin identidad.", n.name));
                                n.auto_increment = false;
                            }
                            if !n.nullable {
                                warnings.push(format!("DSQL no agrega columnas NOT NULL: {tname}.{} se agrega aceptando NULL.", n.name));
                                n.nullable = true;
                            }
                            // ADD COLUMN takes name and type only: the default goes right after.
                            if let Some(d) = text(&n.default_value) {
                                warnings.push(format!(
                                    "{tname}.{}: el valor por defecto rige para las filas nuevas; las que ya están quedan en NULL.",
                                    n.name
                                ));
                                after.push(format!("ALTER TABLE {name} ALTER COLUMN {} SET DEFAULT {d};", quote_ident(Quote::Double, &n.name)));
                                n.default_value = None;
                            }
                        }
                        Some(o) => {
                            if squash(&o.data_type) != squash(&n.data_type) {
                                warnings.push(format!(
                                    "DSQL no cambia el tipo de una columna: {tname}.{} queda {} (para {} hay que recrear la tabla).",
                                    n.name, o.data_type, n.data_type
                                ));
                                n.data_type = o.data_type.clone();
                            }
                            if o.nullable && !n.nullable {
                                warnings.push(format!("DSQL no pasa una columna existente a NOT NULL: {tname}.{} sigue aceptando NULL.", n.name));
                                n.nullable = true;
                            }
                            if keys.contains(&n.name.to_lowercase()) {
                                n.nullable = o.nullable;
                            }
                            if o.auto_increment != n.auto_increment {
                                warnings.push(format!("{tname}.{}: DBine no cambia la identidad de columnas en DSQL; se deja como está.", n.name));
                                n.auto_increment = o.auto_increment;
                            }
                        }
                    }
                }
                TableChange::Alter { old, new }
            }
        });
    }

    let flavor = SqlFlavor { comment_on: false, ..SqlFlavor::ansi() };
    // ADD COLUMN: name and type (normalized above).
    let cd = |_: &TableSchema, c: &ColumnDef| format!("{} {}", quote_ident(Quote::Double, &c.name), c.data_type);
    let dd = |t: &TableSchema, p: DdlParts| Ok(crate::table_ddl(t, p));
    let st = AlterStyle::from_flavor(&flavor, ColumnAlter::Standard { set_data_type: false, using_cast: false }, &cd, &dd);
    let mut s = alter::sync_script(&st, &planned)?;
    crate::structure::fix_script(&mut s);
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
            schema: Some("public".into()),
            name: name.into(),
            columns: cols,
            primary_key: Some(KeyDef { name: Some("pk".into()), columns: vec!["id".into()] }),
            ..Default::default()
        }
    }

    fn ix(name: &str, col: &str) -> IndexDef {
        IndexDef { name: name.into(), columns: vec![col.into()], unique: false, kind: None, filter: None, ..Default::default() }
    }

    #[test]
    fn alter_trims_to_what_dsql_takes() {
        let mut old = table("clientes", vec![col("id", "integer", false), col("nombre", "varchar(10)", false), col("baja", "date", true), col("nota", "text", true)]);
        old.columns[3].default_value = Some("'x'".into());
        old.indexes.push(ix("ix_nombre", "nombre"));
        let mut new = table("clientes", vec![col("id", "integer", false), col("nombre", "varchar(20)", true), col("email", "text", false), col("nota", "text", false)]);
        new.columns[2].default_value = Some("'s/n'".into());
        new.indexes.push(ix("ix_email", "email"));
        new.foreign_keys.push(ForeignKeyDef { name: Some("fk".into()), columns: vec!["id".into()], ref_schema: None, ref_table: "o".into(), ref_columns: vec!["id".into()], on_delete: None, on_update: None });
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP INDEX \"public\".\"ix_nombre\";",
                "ALTER TABLE \"public\".\"clientes\" DROP COLUMN \"baja\";",
                "ALTER TABLE \"public\".\"clientes\" ADD COLUMN \"email\" text;",
                "ALTER TABLE \"public\".\"clientes\" ALTER COLUMN \"nombre\" DROP NOT NULL;",
                "ALTER TABLE \"public\".\"clientes\" ALTER COLUMN \"nota\" DROP DEFAULT;",
                "CREATE INDEX ASYNC \"ix_email\" ON \"public\".\"clientes\" (\"email\");",
                "ALTER TABLE \"public\".\"clientes\" ALTER COLUMN \"email\" SET DEFAULT 's/n';",
            ]
        );
        let w = s.warnings.join("\n");
        assert!(w.contains("se omiten las de public.clientes"), "{w}");
        assert!(w.contains("public.clientes.nombre queda varchar(10)"), "{w}");
        assert!(w.contains("public.clientes.email se agrega aceptando NULL"), "{w}");
        assert!(w.contains("public.clientes.nota sigue aceptando NULL"), "{w}");
    }

    #[test]
    fn primary_key_stays() {
        let old = table("t", vec![col("id", "integer", false), col("b", "integer", false)]);
        let mut new = table("t", vec![col("b", "integer", false)]);
        new.primary_key = Some(KeyDef { name: Some("pk".into()), columns: vec!["b".into()] });
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert!(s.statements.is_empty(), "{:?}", s.statements);
        assert_eq!(s.warnings.len(), 2, "{:?}", s.warnings);
    }

    #[test]
    fn create_and_drop() {
        let mut a = table("nueva", vec![col("id", "integer", false)]);
        a.indexes.push(ix("ix_id", "id"));
        let b = table("vieja", vec![col("id", "integer", false)]);
        let s = sync_script(&[TableChange::Create { table: a }, TableChange::Drop { table: b }]).unwrap();
        assert_eq!(s.statements[0], "DROP TABLE \"public\".\"vieja\";");
        assert!(s.statements[1].starts_with("CREATE TABLE \"public\".\"nueva\""), "{}", s.statements[1]);
        assert_eq!(s.statements[2], "CREATE INDEX ASYNC \"ix_id\" ON \"public\".\"nueva\" (\"id\");");
    }
}
