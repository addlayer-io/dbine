//! Schema sync for Phoenix: columns are only added (`ALTER TABLE t ADD …`)
//! or dropped (`DROP COLUMN`), indexes go with `DROP INDEX ix ON t`. The
//! primary key is the HBase row key and doesn't change; there are no
//! foreign keys nor comments.
//!
//! The generic planner runs on a copy of the changes trimmed to that; what
//! is left out becomes a warning.

use crate::ddl;
use dbine_driver::alter::{self, AlterStyle, ColumnAlter, DropIndex};
use dbine_driver::sql::Quote;
use dbine_driver::{ColumnDef, DdlParts, Result, SyncScript, TableChange, TableSchema};

fn display(t: &TableSchema) -> String {
    match t.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    }
}

fn key_cols(t: &TableSchema) -> Vec<String> {
    t.primary_key.as_ref().map(|k| k.columns.iter().map(|c| c.to_lowercase()).collect()).unwrap_or_default()
}

pub fn sync_script(changes: &[TableChange]) -> Result<SyncScript> {
    let mut warnings = Vec::new();
    let mut planned = Vec::with_capacity(changes.len());
    for ch in changes {
        planned.push(match ch {
            TableChange::Create { table } => {
                let mut table = table.clone();
                if !table.foreign_keys.is_empty() {
                    warnings.push(format!("Phoenix no tiene claves foráneas: se omiten las de {}.", display(&table)));
                    table.foreign_keys.clear();
                }
                TableChange::Create { table }
            }
            TableChange::Drop { table } => TableChange::Drop { table: table.clone() },
            TableChange::Alter { old, new } => {
                let (old, mut new) = (old.clone(), new.clone());
                let tname = display(&new);
                if key_cols(&old) != key_cols(&new) {
                    warnings.push(format!(
                        "La clave primaria de {tname} no se cambia en Phoenix (es la clave de fila de HBase): hay que recrear la tabla y copiar los datos."
                    ));
                }
                new.primary_key = old.primary_key.clone();
                // Key columns can't be dropped: they stay.
                for o in old.columns.iter().filter(|o| key_cols(&old).contains(&o.name.to_lowercase())) {
                    if !new.columns.iter().any(|n| n.name.eq_ignore_ascii_case(&o.name)) {
                        warnings.push(format!("{tname}.{} es parte de la clave primaria: Phoenix no la borra.", o.name));
                        new.columns.push(o.clone());
                    }
                }
                if !new.foreign_keys.is_empty() {
                    warnings.push(format!("Phoenix no tiene claves foráneas: se omiten las de {tname}."));
                    new.foreign_keys.clear();
                }
                let immutable = old.options.get("IMMUTABLE_ROWS").is_some_and(|v| v.eq_ignore_ascii_case("true"));
                for n in new.columns.iter_mut().filter(|n| !old.columns.iter().any(|o| o.name.eq_ignore_ascii_case(&n.name))) {
                    if !n.nullable && !immutable {
                        warnings.push(format!(
                            "Phoenix solo acepta NOT NULL en la clave primaria (o con IMMUTABLE_ROWS): {tname}.{} se agrega aceptando NULL.",
                            n.name
                        ));
                        n.nullable = true;
                    }
                }
                new.options = old.options.clone();
                TableChange::Alter { old, new }
            }
        });
    }

    let cd = |t: &TableSchema, c: &ColumnDef| ddl::column_def(t, c);
    let dd = |t: &TableSchema, p: DdlParts| ddl::table_ddl(t, p);
    let st = AlterStyle {
        quote: Quote::Double,
        column: ColumnAlter::None,
        add_column: "ADD",
        drop_index: DropIndex::OnTable,
        drop_fk: "DROP CONSTRAINT",
        drop_pk_keyword: false,
        fk_inline: false,
        comment_on: false,
        column_def: &cd,
        table_ddl: &dd,
    };
    let mut s = alter::sync_script(&st, &planned)?;
    warnings.append(&mut s.warnings);
    s.warnings = warnings;
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{IndexDef, KeyDef};

    fn col(name: &str, ty: &str, nullable: bool) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable, ..Default::default() }
    }

    fn table(name: &str, cols: Vec<ColumnDef>) -> TableSchema {
        TableSchema {
            kind: "table".into(),
            schema: Some("DBINE".into()),
            name: name.into(),
            columns: cols,
            primary_key: Some(KeyDef { name: Some("PK".into()), columns: vec!["ID".into()] }),
            ..Default::default()
        }
    }

    fn ix(name: &str, col: &str) -> IndexDef {
        IndexDef { name: name.into(), columns: vec![col.into()], unique: false, kind: Some("global".into()), filter: None, ..Default::default() }
    }

    #[test]
    fn alter_adds_drops_and_reindexes() {
        let mut old = table("CLIENTES", vec![col("ID", "INTEGER", false), col("NOMBRE", "VARCHAR(10)", true), col("BAJA", "DATE", true)]);
        old.indexes.push(ix("IX_NOMBRE", "NOMBRE"));
        old.indexes.push(ix("IX_BAJA", "BAJA"));
        let mut new = table("CLIENTES", vec![col("ID", "INTEGER", false), col("NOMBRE", "VARCHAR(20)", true), col("EMAIL", "VARCHAR", false)]);
        new.columns[2].options.insert("family".into(), "CF1".into());
        new.columns[2].default_value = Some("'-'".into());
        new.indexes.push(ix("IX_NOMBRE", "EMAIL"));
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP INDEX \"IX_NOMBRE\" ON \"DBINE\".\"CLIENTES\";",
                "DROP INDEX \"IX_BAJA\" ON \"DBINE\".\"CLIENTES\";",
                "ALTER TABLE \"DBINE\".\"CLIENTES\" DROP COLUMN \"BAJA\";",
                "ALTER TABLE \"DBINE\".\"CLIENTES\" ADD \"CF1\".\"EMAIL\" VARCHAR DEFAULT '-';",
                "CREATE INDEX \"IX_NOMBRE\" ON \"DBINE\".\"CLIENTES\" (\"EMAIL\");",
            ]
        );
        let w = s.warnings.join("\n");
        assert!(w.contains("DBINE.CLIENTES.EMAIL se agrega aceptando NULL"), "{w}");
        assert!(w.contains("DBINE.CLIENTES.NOMBRE: este motor no modifica columnas"), "{w}");
        assert!(w.contains("Se borra la columna DBINE.CLIENTES.BAJA"), "{w}");
    }

    #[test]
    fn primary_key_stays() {
        let old = table("T", vec![col("ID", "INTEGER", false), col("B", "INTEGER", true)]);
        let mut new = table("T", vec![col("B", "INTEGER", true)]);
        new.primary_key = Some(KeyDef { name: Some("PK".into()), columns: vec!["B".into()] });
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert!(s.statements.is_empty(), "{:?}", s.statements);
        let w = s.warnings.join("\n");
        assert!(w.contains("La clave primaria de DBINE.T no se cambia"), "{w}");
        assert!(w.contains("DBINE.T.ID es parte de la clave primaria"), "{w}");
    }

    #[test]
    fn create_and_drop() {
        let mut a = table("NUEVA", vec![col("ID", "INTEGER", false)]);
        a.indexes.push(ix("IX_ID", "ID"));
        let b = table("VIEJA", vec![col("ID", "INTEGER", false)]);
        let s = sync_script(&[TableChange::Create { table: a }, TableChange::Drop { table: b }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP TABLE \"DBINE\".\"VIEJA\";",
                "CREATE TABLE \"DBINE\".\"NUEVA\" (\n    \"ID\" INTEGER NOT NULL,\n    CONSTRAINT \"PK\" PRIMARY KEY (\"ID\")\n);",
                "CREATE INDEX \"IX_ID\" ON \"DBINE\".\"NUEVA\" (\"ID\");",
            ]
        );
    }
}
