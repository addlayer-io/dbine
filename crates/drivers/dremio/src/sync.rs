//! Schema sync ("Comparar esquemas") in Dremio's SQL, for Iceberg tables:
//! `ALTER TABLE t ADD COLUMNS (…)`, `DROP COLUMN c` and `ALTER COLUMN c c
//! type` (Iceberg only widens: INT → BIGINT, FLOAT → DOUBLE, a larger
//! DECIMAL precision). No keys, indexes, defaults or NOT NULL.

use crate::ddl::{path, q, table_ddl};
use dbine_driver::{DdlParts, Result, SyncScript, TableChange, TableSchema};

fn display(t: &TableSchema) -> String {
    match t.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    }
}

fn squash(s: &str) -> String {
    let s: String = s.to_uppercase().split_whitespace().collect();
    match s.as_str() {
        "INTEGER" => "INT".into(),
        "CHARACTERVARYING" => "VARCHAR".into(),
        _ => s,
    }
}

pub fn sync_script(changes: &[TableChange]) -> Result<SyncScript> {
    let (mut drops, mut alters, mut creates, mut warnings) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ch in changes {
        match ch {
            TableChange::Create { table } => creates.push(table_ddl(table, DdlParts { create: true, ..Default::default() })),
            TableChange::Drop { table } => {
                warnings.push(format!("Se borra la tabla {} con todos sus datos.", display(table)));
                drops.push(table_ddl(table, DdlParts { drop: true, ..Default::default() }));
            }
            TableChange::Alter { old, new } => alter(old, new, &mut alters, &mut warnings),
        }
    }
    if !alters.is_empty() {
        warnings.push("Dremio solo modifica tablas Iceberg ($scratch, Nessie, Arctic, catálogos Iceberg): en otros orígenes el ALTER falla.".into());
    }
    Ok(SyncScript { statements: [drops, alters, creates].concat(), warnings })
}

fn alter(old: &TableSchema, new: &TableSchema, out: &mut Vec<String>, warnings: &mut Vec<String>) {
    let name = path(new.schema.as_deref(), &new.name);
    let tname = display(new);
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    for o in old.columns.iter().filter(|o| !new.columns.iter().any(|n| eq(&n.name, &o.name))) {
        warnings.push(format!("Se borra la columna {tname}.{} con sus datos.", o.name));
        out.push(format!("ALTER TABLE {name} DROP COLUMN {};", q(&o.name)));
    }
    let added: Vec<String> = new
        .columns
        .iter()
        .filter(|n| !old.columns.iter().any(|o| eq(&o.name, &n.name)))
        .map(|c| format!("{} {}", q(&c.name), c.data_type))
        .collect();
    if !added.is_empty() {
        out.push(format!("ALTER TABLE {name} ADD COLUMNS ({});", added.join(", ")));
    }
    for n in &new.columns {
        let Some(o) = old.columns.iter().find(|o| eq(&o.name, &n.name)) else { continue };
        if squash(&o.data_type) != squash(&n.data_type) {
            warnings.push(format!(
                "{tname}.{}: {} → {}. Iceberg solo agranda tipos (INT → BIGINT, FLOAT → DOUBLE, más precisión en DECIMAL): otro cambio falla.",
                n.name, o.data_type, n.data_type
            ));
            out.push(format!("ALTER TABLE {name} ALTER COLUMN {} {} {};", q(&n.name), q(&n.name), n.data_type));
        }
    }
    let opt = |t: &TableSchema, k: &str| t.options.get(k).map(|v| squash(v).trim_matches(|c| c == '(' || c == ')').to_string()).unwrap_or_default();
    for (k, what) in [("partition_by", "la partición (ALTER TABLE … ADD / DROP PARTITION FIELD)"), ("localsort_by", "el orden local (LOCALSORT BY)")] {
        if !opt(new, k).is_empty() && opt(old, k) != opt(new, k) {
            warnings.push(format!("{tname}: {what} no se sincroniza; cambiala a mano."));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ColumnDef;

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
    }

    fn t() -> TableSchema {
        TableSchema {
            kind: "table".into(),
            schema: Some("$scratch.ventas".into()),
            name: "pedidos".into(),
            columns: vec![col("id", "INTEGER"), col("nota", "VARCHAR"), col("baja", "DATE")],
            ..Default::default()
        }
    }

    #[test]
    fn alter_columns() {
        let mut new = t();
        new.columns[0].data_type = "BIGINT".into();
        new.columns[1].data_type = "varchar".into();
        new.columns.remove(2);
        new.columns.push(col("total", "DECIMAL(18,2)"));
        new.columns.push(col("cant", "INT"));
        new.options.insert("partition_by".into(), "cant".into());
        let s = sync_script(&[TableChange::Alter { old: t(), new }]).unwrap();
        assert_eq!(
            s.statements,
            vec![
                "ALTER TABLE \"$scratch\".\"ventas\".\"pedidos\" DROP COLUMN \"baja\";",
                "ALTER TABLE \"$scratch\".\"ventas\".\"pedidos\" ADD COLUMNS (\"total\" DECIMAL(18,2), \"cant\" INT);",
                "ALTER TABLE \"$scratch\".\"ventas\".\"pedidos\" ALTER COLUMN \"id\" \"id\" BIGINT;",
            ]
        );
        assert_eq!(s.warnings.len(), 4, "{:?}", s.warnings);
    }

    #[test]
    fn create_and_drop() {
        let mut old = t();
        old.name = "viejos".into();
        let s = sync_script(&[TableChange::Create { table: t() }, TableChange::Drop { table: old }]).unwrap();
        assert_eq!(s.statements[0], "DROP TABLE \"$scratch\".\"ventas\".\"viejos\";");
        assert!(s.statements[1].starts_with("CREATE TABLE \"$scratch\".\"ventas\".\"pedidos\" ("));
        assert_eq!(s.warnings.len(), 1);
    }
}
