//! Schema sync with the driver's `CREATE CONTAINER` / `DROP CONTAINER`.
//! A container's partition key and unique keys are fixed when it's
//! created, and the script language has no statement that replaces an
//! existing container's settings (indexing policy, TTL, throughput):
//! those differences are warnings. Documents have no schema: field
//! changes are warnings too.

use crate::ddl::{opt, path, table_ddl, DEFAULT_TTL, INDEXING_POLICY, PARTITION_KEY, THROUGHPUT, THROUGHPUT_MODE};
use dbine_driver::{ColumnDef, DdlParts, IndexDef, Result, SyncScript, TableChange, TableSchema};

const CREATE: DdlParts = DdlParts { drop: false, if_exists: false, create: true, indexes: true, foreign_keys: false };
const DROP: DdlParts = DdlParts { drop: true, if_exists: false, create: false, indexes: false, foreign_keys: false };

fn eq_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn squash(t: &str) -> String {
    t.to_lowercase().split_whitespace().collect()
}

/// Per-field warnings: documents keep what they have.
fn field_warnings(t: &str, old: &[ColumnDef], new: &[ColumnDef], out: &mut Vec<String>) {
    for c in old.iter().filter(|c| !new.iter().any(|n| eq_name(&n.name, &c.name))) {
        out.push(format!("Los documentos no tienen esquema fijo: el campo {t}.{} no se borra de los documentos que ya existen.", c.name));
    }
    for c in new.iter().filter(|c| !old.iter().any(|o| eq_name(&o.name, &c.name))) {
        out.push(format!("Los documentos no tienen esquema fijo: el campo {t}.{} no se agrega a los documentos que ya existen.", c.name));
    }
    for n in new {
        if let Some(o) = old.iter().find(|o| eq_name(&o.name, &n.name)) {
            if squash(&o.data_type) != squash(&n.data_type) || o.nullable != n.nullable {
                out.push(format!("Los documentos no tienen esquema fijo: el campo {t}.{} no se cambia.", n.name));
            }
        }
    }
}

fn partition_paths(t: &TableSchema) -> Vec<String> {
    opt(t, PARTITION_KEY).unwrap_or("").split(',').map(str::trim).filter(|p| !p.is_empty()).map(|p| path(p).to_lowercase()).collect()
}

/// Unique keys or composite indexes, by content (their names are made up).
fn index_set(t: &TableSchema, unique: bool) -> Vec<Vec<String>> {
    let mut v: Vec<Vec<String>> = t
        .indexes
        .iter()
        .filter(|i: &&IndexDef| i.unique == unique)
        .map(|i| {
            let mut k: Vec<String> = i.columns.iter().map(|c| c.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()).collect();
            // Spatial, full-text and vector indexes: their kind and settings too.
            k.push(i.kind.as_deref().unwrap_or("").to_lowercase());
            k.extend(i.options.iter().map(|(o, v)| format!("{o}={v}")));
            k
        })
        .collect();
    v.sort();
    v
}

fn policy(t: &TableSchema) -> Option<serde_json::Value> {
    opt(t, INDEXING_POLICY).and_then(|p| serde_json::from_str(p).ok())
}

pub fn sync_script(changes: &[TableChange]) -> Result<SyncScript> {
    let (mut drops, mut creates, mut warnings) = (Vec::new(), Vec::new(), Vec::new());
    for ch in changes {
        match ch {
            TableChange::Create { table } => creates.push(table_ddl(table, CREATE)?),
            TableChange::Drop { table } => {
                warnings.push(format!("Se borra el contenedor {} con todos sus documentos.", table.name));
                drops.push(table_ddl(table, DROP)?);
            }
            TableChange::Alter { old, new } => {
                let t = new.name.as_str();
                if partition_paths(old) != partition_paths(new) {
                    warnings.push(format!(
                        "La clave de partición de {t} cambia y Cosmos DB no la modifica: hay que crear otro contenedor y copiar los documentos. Se deja como está."
                    ));
                }
                if index_set(old, true) != index_set(new, true) {
                    warnings.push(format!("Las claves únicas de {t} se fijan al crear el contenedor y no se cambian; se deja como está."));
                }
                let mut settings = Vec::new();
                if index_set(old, false) != index_set(new, false) || policy(old) != policy(new) {
                    settings.push("la política de indexación");
                }
                if opt(old, DEFAULT_TTL) != opt(new, DEFAULT_TTL) {
                    settings.push("el TTL");
                }
                if opt(old, THROUGHPUT_MODE) != opt(new, THROUGHPUT_MODE) || opt(old, THROUGHPUT) != opt(new, THROUGHPUT) {
                    settings.push("el rendimiento (RU/s)");
                }
                if !settings.is_empty() {
                    warnings.push(format!(
                        "{t}: {} no se cambia desde un script (no hay sentencia para modificar un contenedor); hacelo desde el portal de Azure o la CLI.",
                        settings.join(", ")
                    ));
                }
                field_warnings(t, &old.columns, &new.columns, &mut warnings);
            }
        }
    }
    let statements = drops.into_iter().chain(creates).map(|s: String| s.trim_end().to_string()).filter(|s| !s.is_empty()).collect();
    Ok(SyncScript { statements, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> ColumnDef {
        ColumnDef { name: name.into(), data_type: ty.into(), nullable: true, ..Default::default() }
    }

    fn container(name: &str, pk: &str, cols: Vec<ColumnDef>) -> TableSchema {
        let mut t = TableSchema { kind: "collection".into(), name: name.into(), columns: cols, ..Default::default() };
        t.options.insert(PARTITION_KEY.into(), pk.into());
        t
    }

    #[test]
    fn alter_is_warnings() {
        let old = container("orders", "/customer", vec![col("id", "string"), col("total", "number"), col("old", "string")]);
        let mut new = container("orders", "/region", vec![col("id", "string"), col("total", "string"), col("note", "string")]);
        new.indexes = vec![IndexDef { name: "uk".into(), columns: vec!["email".into()], unique: true, ..Default::default() }];
        new.options.insert(DEFAULT_TTL.into(), "60".into());
        let s = sync_script(&[TableChange::Alter { old, new }]).unwrap();
        assert!(s.statements.is_empty());
        assert_eq!(
            s.warnings,
            vec![
                "La clave de partición de orders cambia y Cosmos DB no la modifica: hay que crear otro contenedor y copiar los documentos. Se deja como está.",
                "Las claves únicas de orders se fijan al crear el contenedor y no se cambian; se deja como está.",
                "orders: el TTL no se cambia desde un script (no hay sentencia para modificar un contenedor); hacelo desde el portal de Azure o la CLI.",
                "Los documentos no tienen esquema fijo: el campo orders.old no se borra de los documentos que ya existen.",
                "Los documentos no tienen esquema fijo: el campo orders.note no se agrega a los documentos que ya existen.",
                "Los documentos no tienen esquema fijo: el campo orders.total no se cambia.",
            ]
        );
    }

    #[test]
    fn create_and_drop() {
        let s = sync_script(&[
            TableChange::Create { table: container("orders", "/customer", vec![]) },
            TableChange::Drop { table: container("legacy", "/id", vec![]) },
        ])
        .unwrap();
        assert_eq!(s.statements.len(), 2);
        assert_eq!(s.statements[0], "DROP CONTAINER \"legacy\";");
        // Key order depends on serde_json's `preserve_order` (a workspace feature): compare the body as JSON.
        let body = s.statements[1].strip_prefix("CREATE CONTAINER \"orders\" ").and_then(|b| b.strip_suffix(';')).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(body).unwrap(),
            serde_json::json!({ "partitionKey": { "kind": "Hash", "paths": ["/customer"] } })
        );
        assert_eq!(s.warnings, vec!["Se borra el contenedor legacy con todos sus documentos."]);
    }
}
