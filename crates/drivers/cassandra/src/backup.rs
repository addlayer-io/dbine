//! Native backups (docs/backups.md). Only Amazon Keyspaces has them in
//! CQL: point-in-time recovery (PITR), turned on per table with
//! `ALTER TABLE … WITH custom_properties = {'point_in_time_recovery':
//! {'status': 'enabled'}}`, keeps continuous backups of the last 35 days;
//! `RESTORE TABLE new FROM TABLE old [WITH restore_timestamp = '…']`
//! restores one into a new table, and turning PITR off discards them. The
//! history is the tables with PITR on (`system_schema_mcs.tables`).
//!
//! Cassandra's and ScyllaDB's snapshots are taken with nodetool (JMX) or
//! Scylla's REST API, not CQL.

use crate::{cql, text, value, CassandraSession, Flavor};
use dbine_driver::{BackupAction, BackupEntry, BackupSpec, Error, Field, FieldKind, Result};
use serde_json::Value as J;

pub fn spec(f: Flavor) -> Option<BackupSpec> {
    (f == Flavor::Keyspaces).then(|| BackupSpec {
        backup_options: vec![Field::new("tables", "Tablas", FieldKind::Text)
            .required()
            .placeholder("clientes, pedidos")
            .help("Separadas por coma. Activa la recuperación a un punto en el tiempo (PITR) en cada una: desde ese momento Keyspaces guarda backups continuos de los últimos 35 días.")],
        restore: true,
        restore_options: vec![
            Field::new("target", "Tabla nueva", FieldKind::Text)
                .placeholder("<tabla>_restaurada")
                .help("Keyspaces restaura en una tabla nueva, que no tiene que existir. Vacío: el nombre de la tabla con «_restaurada»."),
            Field::new("timestamp", "Momento a restaurar", FieldKind::Text)
                .placeholder("2026-09-29T12:00:00Z")
                .help("ISO 8601, entre el más antiguo restaurable y ahora. Vacío: ahora."),
        ],
        delete: true,
        history: true,
        server_wide: false,
        script_database: "",
        note: "Keyspaces guarda backups continuos (PITR) de las tablas que lo tienen activado, durante 35 días, en AWS. Restaurar crea una tabla nueva con los datos de ese momento (queda en estado RESTORING hasta terminar). Eliminar desactiva PITR en la tabla y descarta sus backups. El usuario necesita permisos de IAM para restaurar.",
    })
}

/// `ks.table` as CQL needs it.
fn table(ks: Option<&str>, name: &str) -> String {
    cql::qualified(ks.filter(|k| !k.is_empty()), name)
}

/// A backup's id: `keyspace.table` (CQL names can't hold a dot).
fn split_id(id: &str) -> (Option<&str>, &str) {
    match id.trim().split_once('.') {
        Some((k, t)) => (Some(k), t),
        None => (None, id.trim()),
    }
}

fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn pitr(status: &str) -> String {
    format!("custom_properties = {{'point_in_time_recovery': {{'status': '{status}'}}}}")
}

pub fn script(f: Flavor, action: &BackupAction) -> Result<String> {
    if f != Flavor::Keyspaces {
        return Err(Error::Unsupported(
            "Cassandra y ScyllaDB hacen snapshots con nodetool (JMX) o la API REST de Scylla, no con CQL".into(),
        ));
    }
    match action {
        BackupAction::Backup { database, options } => {
            let names: Vec<&str> =
                options.get("tables").map(String::as_str).unwrap_or("").split(',').map(str::trim).filter(|t| !t.is_empty()).collect();
            if names.is_empty() {
                return Err(Error::Query("Indicá al menos una tabla.".into()));
            }
            let ks = database.as_deref();
            Ok(names.iter().map(|t| format!("ALTER TABLE {} WITH {};\n", table(ks, t), pitr("enabled"))).collect())
        }
        BackupAction::Restore { source, database, options } => {
            let (src_ks, src) = split_id(source);
            if src.is_empty() {
                return Err(Error::Query("Falta la tabla a restaurar.".into()));
            }
            let target_ks = database.as_deref().filter(|d| !d.is_empty()).or(src_ks);
            let target = options.get("target").map(|t| t.trim()).filter(|t| !t.is_empty()).map(str::to_string).unwrap_or_else(|| format!("{src}_restaurada"));
            let mut s = format!("RESTORE TABLE {}\nFROM TABLE {}", table(target_ks, &target), table(src_ks, src));
            if let Some(ts) = options.get("timestamp").map(|t| t.trim()).filter(|t| !t.is_empty()) {
                s.push_str(&format!("\nWITH restore_timestamp = {}", literal(ts)));
            }
            s.push(';');
            Ok(s)
        }
        BackupAction::Delete { source } => {
            let (ks, t) = split_id(source);
            if t.is_empty() {
                return Err(Error::Query("Falta la tabla.".into()));
            }
            Ok(format!("ALTER TABLE {} WITH {};", table(ks, t), pitr("disabled")))
        }
    }
}

/// The tables with PITR on, of `database` (or of the session's keyspace;
/// with neither, of every keyspace).
pub async fn history(s: &mut CassandraSession, database: Option<&str>) -> Result<Vec<BackupEntry>> {
    if s.flavor != Flavor::Keyspaces {
        return Err(Error::Unsupported("Cassandra y ScyllaDB no listan sus snapshots por CQL".into()));
    }
    let ks = database.filter(|d| !d.is_empty()).map(str::to_string).or_else(|| s.keyspace.clone());
    let cols = "keyspace_name, table_name, custom_properties, status";
    let rows = match &ks {
        Some(k) => s.rows(&format!("SELECT {cols} FROM system_schema_mcs.tables WHERE keyspace_name = ?"), (k,)).await?,
        None => s.rows(&format!("SELECT {cols} FROM system_schema_mcs.tables"), ()).await?,
    };
    let mut out = Vec::new();
    for r in rows {
        let props = r.columns.get(2).and_then(|v| v.as_ref()).map(value::to_json).unwrap_or(J::Null);
        let p = &props["point_in_time_recovery"];
        if !p["status"].as_str().is_some_and(|v| v.eq_ignore_ascii_case("enabled")) {
            continue;
        }
        let (k, t, status) = (text(&r, 0), text(&r, 1), text(&r, 3));
        let earliest = p["earliest_restorable_timestamp"].as_str().map(str::to_string);
        let mut details = vec![("Tabla".to_string(), t.clone())];
        if let Some(e) = &earliest {
            details.push(("Restaurable desde".to_string(), e.clone()));
        }
        out.push(BackupEntry {
            id: format!("{k}.{t}"),
            database: Some(k),
            kind: Some("PITR (continuo)".into()),
            started: earliest,
            finished: None,
            size: None,
            location: Some("AWS (Keyspaces)".into()),
            restorable: status.is_empty() || status.eq_ignore_ascii_case("ACTIVE"),
            status: Some(if status.is_empty() { "activo".into() } else { status }),
            details,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn only_keyspaces() {
        assert!(spec(Flavor::Cassandra).is_none());
        assert!(spec(Flavor::Scylla).is_none());
        let s = spec(Flavor::Keyspaces).unwrap();
        assert!(s.restore && s.delete && s.history && !s.server_wide);
        let b = BackupAction::Backup { database: None, options: opts(&[("tables", "t")]) };
        assert!(matches!(script(Flavor::Cassandra, &b), Err(Error::Unsupported(_))));
        assert!(matches!(script(Flavor::Scylla, &b), Err(Error::Unsupported(_))));
    }

    #[test]
    fn enables_pitr() {
        let b = BackupAction::Backup { database: Some("tienda".into()), options: opts(&[("tables", "clientes, Pedidos ,")]) };
        assert_eq!(
            script(Flavor::Keyspaces, &b).unwrap(),
            "ALTER TABLE tienda.clientes WITH custom_properties = {'point_in_time_recovery': {'status': 'enabled'}};\n\
             ALTER TABLE tienda.\"Pedidos\" WITH custom_properties = {'point_in_time_recovery': {'status': 'enabled'}};\n"
        );
        let b = BackupAction::Backup { database: None, options: opts(&[("tables", " ")]) };
        assert!(script(Flavor::Keyspaces, &b).is_err());
    }

    #[test]
    fn restores_into_a_new_table() {
        let r = BackupAction::Restore { source: "tienda.clientes".into(), database: None, options: BTreeMap::new() };
        assert_eq!(script(Flavor::Keyspaces, &r).unwrap(), "RESTORE TABLE tienda.clientes_restaurada\nFROM TABLE tienda.clientes;");
        let r = BackupAction::Restore {
            source: "tienda.clientes".into(),
            database: Some("otra".into()),
            options: opts(&[("target", "copia"), ("timestamp", "2026-09-29T12:00:00Z' x")]),
        };
        assert_eq!(
            script(Flavor::Keyspaces, &r).unwrap(),
            "RESTORE TABLE otra.copia\nFROM TABLE tienda.clientes\nWITH restore_timestamp = '2026-09-29T12:00:00Z'' x';"
        );
    }

    #[test]
    fn delete_turns_pitr_off() {
        let d = BackupAction::Delete { source: "tienda.clientes".into() };
        assert_eq!(
            script(Flavor::Keyspaces, &d).unwrap(),
            "ALTER TABLE tienda.clientes WITH custom_properties = {'point_in_time_recovery': {'status': 'disabled'}};"
        );
    }
}
