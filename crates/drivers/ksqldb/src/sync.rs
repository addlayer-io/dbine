//! Schema sync in ksqlDB: streams and tables are created and dropped (the
//! Kafka topic stays), and value columns are added with `ALTER STREAM |
//! TABLE … ADD COLUMN`. ksqlDB can't drop columns, change their types or
//! the key, switch a stream to a table, or change the `WITH` properties:
//! those are warnings.

use crate::ddl::{is_table, q, table_ddl};
use dbine_driver::{ColumnDef, DdlParts, Result, SyncScript, TableChange, TableSchema};

const CREATE: DdlParts = DdlParts { drop: false, if_exists: false, create: true, indexes: false, foreign_keys: false };
const DROP: DdlParts = DdlParts { drop: true, if_exists: false, create: false, indexes: false, foreign_keys: false };

/// `WITH` properties fixed at creation.
const PROPERTIES: [&str; 5] = ["KAFKA_TOPIC", "KEY_FORMAT", "VALUE_FORMAT", "PARTITIONS", "TIMESTAMP"];

fn eq_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn squash(t: &str) -> String {
    t.to_uppercase().split_whitespace().collect()
}

fn object(t: &TableSchema) -> &'static str {
    if is_table(t) {
        "TABLE"
    } else {
        "STREAM"
    }
}

fn keys(t: &TableSchema) -> Vec<String> {
    t.primary_key.iter().flat_map(|k| k.columns.iter().map(|c| c.to_uppercase())).collect()
}

fn prop(t: &TableSchema, k: &str) -> Option<String> {
    t.options.get(k).map(|v| v.trim().to_uppercase()).filter(|v| !v.is_empty())
}

pub fn sync_script(changes: &[TableChange]) -> Result<SyncScript> {
    let (mut drops, mut alters, mut creates, mut warnings) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ch in changes {
        match ch {
            TableChange::Create { table } => creates.push(table_ddl(table, CREATE)?),
            TableChange::Drop { table } => {
                let what = if is_table(table) { "la tabla" } else { "el stream" };
                warnings.push(format!(
                    "Se borra {what} {} (el topic de Kafka y sus mensajes quedan). Falla si hay consultas persistentes que lo usan: terminalas antes.",
                    table.name
                ));
                drops.push(table_ddl(table, DROP)?);
            }
            TableChange::Alter { old, new } => {
                let t = new.name.as_str();
                if object(old) != object(new) {
                    warnings.push(format!("{t} pasa de {} a {}: hay que borrarlo y crearlo de nuevo; se deja como está.", object(old), object(new)));
                    continue;
                }
                if keys(old) != keys(new) {
                    warnings.push(format!("La clave de {t} cambia y ksqlDB no la modifica: hay que borrarlo y crearlo de nuevo. Se deja como está."));
                }
                let (ok, nk) = (keys(old), keys(new));
                let is_key = |c: &ColumnDef| ok.contains(&c.name.to_uppercase()) || nk.contains(&c.name.to_uppercase());
                for c in old.columns.iter().filter(|c| !is_key(c) && !new.columns.iter().any(|n| eq_name(&n.name, &c.name))) {
                    warnings.push(format!("{t}.{}: ksqlDB no borra columnas; se deja como está.", c.name));
                }
                let added: Vec<&ColumnDef> =
                    new.columns.iter().filter(|c| !is_key(c) && !old.columns.iter().any(|o| eq_name(&o.name, &c.name))).collect();
                for n in new.columns.iter().filter(|c| !is_key(c)) {
                    if let Some(o) = old.columns.iter().find(|o| eq_name(&o.name, &n.name)) {
                        if squash(&o.data_type) != squash(&n.data_type) {
                            warnings.push(format!("{t}.{}: {} → {}. ksqlDB no cambia el tipo de una columna; se deja como está.", n.name, o.data_type, n.data_type));
                        }
                    }
                }
                if !added.is_empty() {
                    let cols: Vec<String> = added.iter().map(|c| format!("ADD COLUMN {} {}", q(&c.name), c.data_type.trim())).collect();
                    alters.push(format!("ALTER {} {} {};", object(new), q(t), cols.join(", ")));
                }
                let changed: Vec<&str> = PROPERTIES.iter().copied().filter(|k| prop(old, k) != prop(new, k)).collect();
                if !changed.is_empty() {
                    warnings.push(format!("{t}: {} se fija al crearlo; se deja como está.", changed.join(", ")));
                }
            }
        }
    }
    let statements = [drops, alters, creates].into_iter().flatten().filter(|s: &String| !s.trim().is_empty()).collect();
    Ok(SyncScript { statements, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::KeyDef;

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
    }

    fn stream(name: &str, cols: Vec<ColumnDef>) -> TableSchema {
        let mut t = TableSchema { kind: "stream".into(), name: name.into(), columns: cols, primary_key: Some(KeyDef { name: None, columns: vec!["ID".into()] }), ..Default::default() };
        t.options.insert("object".into(), "STREAM".into());
        t.options.insert("KAFKA_TOPIC".into(), name.to_lowercase());
        t.options.insert("VALUE_FORMAT".into(), "JSON".into());
        t
    }

    #[test]
    fn alter_adds_columns() {
        let old = stream("CLICKS", vec![col("ID", "STRING"), col("URL", "STRING"), col("MS", "INTEGER"), col("OLD", "STRING")]);
        let new = stream("CLICKS", vec![col("ID", "STRING"), col("URL", "STRING"), col("MS", "BIGINT"), col("USER", "STRING"), col("TAGS", "ARRAY<STRING>")]);
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert_eq!(s.statements, vec!["ALTER STREAM `CLICKS` ADD COLUMN `USER` STRING, ADD COLUMN `TAGS` ARRAY<STRING>;"]);
        assert_eq!(
            s.warnings,
            vec![
                "CLICKS.OLD: ksqlDB no borra columnas; se deja como está.",
                "CLICKS.MS: INTEGER → BIGINT. ksqlDB no cambia el tipo de una columna; se deja como está.",
            ]
        );
    }

    #[test]
    fn create_and_drop() {
        let s = sync_script(&[
            TableChange::Create { table: stream("CLICKS", vec![col("ID", "STRING"), col("URL", "STRING")]) },
            TableChange::Drop { table: stream("OLD", vec![col("ID", "STRING")]) },
        ])
        .unwrap();
        assert_eq!(
            s.statements,
            vec![
                "DROP STREAM `OLD`;",
                "CREATE STREAM `CLICKS` (\n    `ID` STRING KEY,\n    `URL` STRING\n) WITH (KAFKA_TOPIC='clicks', VALUE_FORMAT='JSON');",
            ]
        );
        assert_eq!(
            s.warnings,
            vec!["Se borra el stream OLD (el topic de Kafka y sus mensajes quedan). Falla si hay consultas persistentes que lo usan: terminalas antes."]
        );
    }
}
