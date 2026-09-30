//! Schema sync for Snowflake: `ALTER COLUMN … SET DATA TYPE`, `SET/DROP NOT
//! NULL`, `DROP DEFAULT`; keys are informational constraints and the only
//! "indexes" are UNIQUE constraints (standard tables have no indexes).
//!
//! The generic planner does the work on a copy of the changes trimmed to
//! what Snowflake can do; what's left out becomes a warning.

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

fn default_of(c: &ColumnDef) -> Option<String> {
    c.default_value.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn is_sequence(d: &Option<String>) -> bool {
    d.as_deref().is_some_and(|d| d.to_ascii_lowercase().contains("nextval"))
}

fn squash(t: &str) -> String {
    t.to_lowercase().split_whitespace().collect()
}

/// Non-unique indexes don't exist in Snowflake: they are left out, with a
/// warning when the source table has them.
fn only_unique(t: &mut TableSchema, warnings: &mut Vec<String>) {
    let tname = display(t);
    for ix in t.indexes.iter().filter(|i| !i.unique) {
        warnings.push(format!("Snowflake no tiene índices: se omite {tname}.{}.", ix.name));
    }
    t.indexes.retain(|i| i.unique);
}

/// Snowflake has no CHECK constraints: left out, with a warning.
fn no_checks(t: &mut TableSchema, warnings: &mut Vec<String>) {
    if !t.checks.is_empty() {
        warnings.push(format!("Snowflake no tiene restricciones CHECK: se omiten las de {}.", display(t)));
        t.checks.clear();
    }
}

/// Clustering key changes (`ALTER TABLE … CLUSTER BY / DROP CLUSTERING KEY`);
/// a table can't turn transient or permanent in place.
fn table_options(old: &TableSchema, new: &TableSchema, post: &mut Vec<String>, warnings: &mut Vec<String>) {
    let opt = |t: &TableSchema, k: &str| t.options.get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let name = dbine_driver::sql::qualified_name(Quote::Double, new.schema.as_deref().filter(|s| !s.is_empty()), &new.name);
    let (oc, nc) = (opt(old, ddl::CLUSTER_BY), opt(new, ddl::CLUSTER_BY));
    if oc.as_deref().map(squash) != nc.as_deref().map(squash) {
        post.push(match nc {
            Some(k) => format!("ALTER TABLE {name} CLUSTER BY ({k});"),
            None => format!("ALTER TABLE {name} DROP CLUSTERING KEY;"),
        });
    }
    let transient = |t: &TableSchema| opt(t, ddl::TABLE_TYPE).is_some_and(|v| v.eq_ignore_ascii_case("transient"));
    if transient(old) != transient(new) {
        warnings.push(format!(
            "{}: Snowflake no convierte una tabla entre transitoria y permanente; hay que recrearla (CREATE TABLE … AS SELECT) a mano.",
            display(new)
        ));
    }
}

pub fn sync_script(changes: &[TableChange]) -> Result<SyncScript> {
    let mut warnings = Vec::new();
    let mut options = Vec::new();
    let mut planned = Vec::with_capacity(changes.len());
    for ch in changes {
        planned.push(match ch {
            TableChange::Create { table } => {
                let mut table = table.clone();
                only_unique(&mut table, &mut warnings);
                no_checks(&mut table, &mut warnings);
                TableChange::Create { table }
            }
            TableChange::Drop { table } => TableChange::Drop { table: table.clone() },
            TableChange::Alter { old, new } => {
                let (mut old, mut new) = (old.clone(), new.clone());
                old.indexes.retain(|i| i.unique);
                only_unique(&mut new, &mut warnings);
                old.checks.clear();
                no_checks(&mut new, &mut warnings);
                table_options(&old, &new, &mut options, &mut warnings);
                let tname = display(&new);
                for n in new.columns.iter_mut() {
                    let Some(o) = old.columns.iter().find(|o| o.name.eq_ignore_ascii_case(&n.name)) else { continue };
                    if squash(&o.data_type) != squash(&n.data_type) {
                        warnings.push(format!(
                            "{tname}.{}: Snowflake solo agranda VARCHAR o la precisión de NUMBER (sin cambiar la escala); otro cambio de tipo falla.",
                            n.name
                        ));
                    }
                    let (od, nd) = (default_of(o), default_of(n));
                    // DROP DEFAULT works; SET DEFAULT only swaps one sequence for another.
                    if od != nd && nd.is_some() && !(is_sequence(&od) && is_sequence(&nd)) {
                        warnings.push(format!(
                            "{tname}.{}: Snowflake no agrega ni cambia valores por defecto en columnas existentes (solo la secuencia de una que ya la tiene); se deja como está.",
                            n.name
                        ));
                        n.default_value = o.default_value.clone();
                    }
                }
                TableChange::Alter { old, new }
            }
        });
    }

    let cd = |t: &TableSchema, c: &ColumnDef| ddl::column_def(t, c);
    let dd = |t: &TableSchema, p: DdlParts| Ok(ddl::table_ddl(t, p));
    let st = AlterStyle {
        quote: Quote::Double,
        column: ColumnAlter::Standard { set_data_type: true, using_cast: false },
        add_column: "ADD COLUMN",
        // Rewritten below: UNIQUE constraints go with DROP CONSTRAINT.
        drop_index: DropIndex::AlterTable,
        drop_fk: "DROP CONSTRAINT",
        drop_pk_keyword: true,
        fk_inline: false,
        comment_on: true,
        column_def: &cd,
        table_ddl: &dd,
    };
    let mut s = alter::sync_script(&st, &planned)?;
    for stmt in s.statements.iter_mut() {
        if stmt.starts_with("ALTER TABLE ") && stmt.contains(" DROP INDEX ") {
            *stmt = stmt.replacen(" DROP INDEX ", " DROP CONSTRAINT ", 1);
        }
    }
    s.statements.extend(options);
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
            schema: Some("PUBLIC".into()),
            name: name.into(),
            columns: cols,
            primary_key: Some(KeyDef { name: Some("PK_C".into()), columns: vec!["ID".into()] }),
            ..Default::default()
        }
    }

    #[test]
    fn alter_columns_constraints_and_defaults() {
        let mut old = table("CLIENTES", vec![col("ID", "NUMBER(38,0)", false), col("NOMBRE", "VARCHAR(10)", true), col("BAJA", "DATE", true)]);
        old.columns[1].default_value = Some("'x'".into());
        old.indexes.push(IndexDef { name: "UQ_NOMBRE".into(), columns: vec!["NOMBRE".into()], unique: true, kind: None, filter: None, ..Default::default() });
        old.foreign_keys.push(ForeignKeyDef { name: Some("FK_V".into()), columns: vec!["ID".into()], ref_schema: None, ref_table: "V".into(), ref_columns: vec!["ID".into()], on_delete: None, on_update: None });
        let mut new = table("CLIENTES", vec![col("ID", "NUMBER(38,0)", false), col("NOMBRE", "VARCHAR(20)", false), col("EMAIL", "VARCHAR", true)]);
        new.columns[1].default_value = Some("'y'".into());
        new.indexes.push(IndexDef { name: "IX_EMAIL".into(), columns: vec!["EMAIL".into()], unique: false, kind: None, filter: None, ..Default::default() });
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE \"PUBLIC\".\"CLIENTES\" DROP CONSTRAINT \"FK_V\";",
                "ALTER TABLE \"PUBLIC\".\"CLIENTES\" DROP CONSTRAINT \"UQ_NOMBRE\";",
                "ALTER TABLE \"PUBLIC\".\"CLIENTES\" DROP COLUMN \"BAJA\";",
                "ALTER TABLE \"PUBLIC\".\"CLIENTES\" ADD COLUMN \"EMAIL\" VARCHAR;",
                "ALTER TABLE \"PUBLIC\".\"CLIENTES\" ALTER COLUMN \"NOMBRE\" SET DATA TYPE VARCHAR(20);",
                "ALTER TABLE \"PUBLIC\".\"CLIENTES\" ALTER COLUMN \"NOMBRE\" SET NOT NULL;",
            ]
        );
        let w = s.warnings.join("\n");
        assert!(w.contains("no tiene índices: se omite PUBLIC.CLIENTES.IX_EMAIL"), "{w}");
        assert!(w.contains("no agrega ni cambia valores por defecto"), "{w}");
        assert!(w.contains("solo agranda VARCHAR"), "{w}");
        assert!(w.contains("Se borra la columna PUBLIC.CLIENTES.BAJA"), "{w}");
    }

    #[test]
    fn drop_default_primary_key_and_unique() {
        let mut old = table("T", vec![col("ID", "NUMBER(38,0)", false), col("A", "VARCHAR", true)]);
        old.columns[1].default_value = Some("'x'".into());
        old.columns[1].comment = Some("viejo".into());
        let mut new = old.clone();
        new.columns[1].default_value = None;
        new.columns[1].comment = Some("nuevo".into());
        new.primary_key = Some(KeyDef { name: None, columns: vec!["ID".into(), "A".into()] });
        new.indexes.push(IndexDef { name: "UQ_A".into(), columns: vec!["A".into()], unique: true, kind: None, filter: None, ..Default::default() });
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE \"PUBLIC\".\"T\" DROP PRIMARY KEY;",
                "ALTER TABLE \"PUBLIC\".\"T\" ALTER COLUMN \"A\" DROP DEFAULT;",
                "COMMENT ON COLUMN \"PUBLIC\".\"T\".\"A\" IS 'nuevo';",
                "ALTER TABLE \"PUBLIC\".\"T\" ADD PRIMARY KEY (\"ID\", \"A\");",
                "ALTER TABLE \"PUBLIC\".\"T\" ADD CONSTRAINT \"UQ_A\" UNIQUE (\"A\");",
            ]
        );
    }

    #[test]
    fn sequence_default_can_change() {
        let mut old = table("T", vec![col("ID", "NUMBER(38,0)", false)]);
        old.columns[0].default_value = Some("DB.PUBLIC.S1.NEXTVAL".into());
        let mut new = old.clone();
        new.columns[0].default_value = Some("DB.PUBLIC.S2.NEXTVAL".into());
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(s.statements, vec!["ALTER TABLE \"PUBLIC\".\"T\" ALTER COLUMN \"ID\" SET DEFAULT DB.PUBLIC.S2.NEXTVAL;"]);
    }

    #[test]
    fn clustering_keys_and_checks() {
        let mut old = table("T", vec![col("ID", "NUMBER(38,0)", false), col("F", "DATE", true)]);
        old.options.insert(ddl::CLUSTER_BY.into(), "F".into());
        let mut new = old.clone();
        new.options.insert(ddl::CLUSTER_BY.into(), "F, ID".into());
        new.checks.push(dbine_driver::CheckDef { name: Some("CK".into()), expression: "ID > 0".into() });
        let s = sync_script(&[TableChange::Alter { old: old.clone(), new: new.clone() }]).unwrap();
        assert_eq!(s.statements, vec!["ALTER TABLE \"PUBLIC\".\"T\" CLUSTER BY (F, ID);"]);
        assert!(s.warnings.join("\n").contains("no tiene restricciones CHECK"), "{:?}", s.warnings);

        new.options.remove(ddl::CLUSTER_BY);
        new.options.insert(ddl::TABLE_TYPE.into(), "transient".into());
        let s = sync_script(&[TableChange::Alter { old: old.clone(), new }]).unwrap();
        assert_eq!(s.statements, vec!["ALTER TABLE \"PUBLIC\".\"T\" DROP CLUSTERING KEY;"]);
        assert!(s.warnings.join("\n").contains("transitoria y permanente"), "{:?}", s.warnings);

        // Same key written differently: no change.
        let mut same = old.clone();
        same.options.insert(ddl::CLUSTER_BY.into(), " f ".into());
        assert!(sync_script(&[TableChange::Alter { old, new: same }]).unwrap().statements.is_empty());
    }

    #[test]
    fn create_and_drop() {
        let mut a = table("NUEVA", vec![col("ID", "NUMBER(38,0)", false)]);
        a.foreign_keys.push(ForeignKeyDef { name: Some("FK".into()), columns: vec!["ID".into()], ref_schema: None, ref_table: "VIEJA".into(), ref_columns: vec!["ID".into()], on_delete: None, on_update: None });
        let b = table("VIEJA", vec![col("ID", "NUMBER(38,0)", false)]);
        let s = sync_script(&[TableChange::Create { table: a }, TableChange::Drop { table: b }]).unwrap();
        assert_eq!(s.statements[0], "DROP TABLE \"PUBLIC\".\"VIEJA\";");
        assert!(s.statements[1].starts_with("CREATE TABLE \"PUBLIC\".\"NUEVA\" (\n    \"ID\" NUMBER(38,0) NOT NULL,\n    CONSTRAINT \"PK_C\" PRIMARY KEY (\"ID\")\n);"), "{}", s.statements[1]);
        assert_eq!(s.statements[2], "ALTER TABLE \"PUBLIC\".\"NUEVA\" ADD CONSTRAINT \"FK\" FOREIGN KEY (\"ID\") REFERENCES \"PUBLIC\".\"VIEJA\" (\"ID\");");
        assert_eq!(s.warnings, vec!["Se borra la tabla PUBLIC.VIEJA con todos sus datos."]);
    }
}
