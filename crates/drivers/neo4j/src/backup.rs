//! Native backups (docs/backups.md). Only Memgraph has them in its query
//! language: `CREATE SNAPSHOT` writes a snapshot of the database into the
//! server's snapshot folder, `SHOW SNAPSHOTS` lists them and
//! `RECOVER SNAPSHOT "<path>" [FORCE]` replaces the database with one
//! (FORCE when the storage isn't empty, which is almost always). There is
//! no command that deletes a snapshot: the server drops old ones by its
//! retention setting.
//!
//! Neo4j backs up with `neo4j-admin database backup` (Enterprise), outside
//! Cypher; Neptune's snapshots are an AWS API (RDS), not openCypher.

use crate::{as_text, cypher, Flavor, GraphSession, MEMGRAPH_DEFAULT_DB};
use dbine_driver::{BackupAction, BackupEntry, BackupSpec, Error, Field, FieldKind, Result};

pub fn spec(f: Flavor) -> Option<BackupSpec> {
    (f == Flavor::Memgraph).then(|| BackupSpec {
        backup_options: Vec::new(),
        restore: true,
        restore_options: vec![Field::new("force", "Reemplazar los datos actuales (FORCE)", FieldKind::Bool)
            .default_value("true")
            .help("Memgraph solo recupera un snapshot sobre una base vacía; con FORCE borra lo que haya y carga el snapshot.")],
        delete: false,
        history: true,
        server_wide: false,
        script_database: "",
        note: "CREATE SNAPSHOT guarda una instantánea de la base en la carpeta de snapshots del servidor (<data-directory>/snapshots). Restaurar carga una de esas instantáneas en la base de la pestaña y reemplaza sus datos. Memgraph borra las viejas según --storage-snapshot-retention-count; no hay un comando para eliminarlas.",
    })
}

pub fn script(f: Flavor, action: &BackupAction) -> Result<String> {
    if f != Flavor::Memgraph {
        return Err(Error::Unsupported(unsupported(f).into()));
    }
    match action {
        BackupAction::Backup { .. } => Ok("CREATE SNAPSHOT;".into()),
        BackupAction::Restore { source, options, .. } => {
            let path = source.trim();
            if path.is_empty() {
                return Err(Error::Query("Falta la ruta del snapshot en el servidor.".into()));
            }
            let force = options.get("force").is_none_or(|v| v != "false");
            Ok(format!("RECOVER SNAPSHOT {}{};", cypher::string(path), if force { " FORCE" } else { "" }))
        }
        BackupAction::Delete { .. } => Err(Error::Unsupported(
            "Memgraph no tiene un comando para borrar un snapshot: los viejos se descartan según --storage-snapshot-retention-count".into(),
        )),
    }
}

fn unsupported(f: Flavor) -> &'static str {
    match f {
        Flavor::Neo4j => "Neo4j hace backups con neo4j-admin (Enterprise), fuera de Cypher",
        Flavor::Neptune => "los snapshots de Neptune se hacen con la API de AWS (RDS), no con openCypher",
        Flavor::Memgraph => "",
    }
}

/// `SHOW SNAPSHOTS` (Memgraph 3.x), newest first.
pub async fn history(s: &mut GraphSession, database: Option<&str>) -> Result<Vec<BackupEntry>> {
    if s.flavor != Flavor::Memgraph {
        return Err(Error::Unsupported(unsupported(s.flavor).into()));
    }
    let current = if s.db.is_empty() { MEMGRAPH_DEFAULT_DB.to_string() } else { s.db.clone() };
    // Snapshots are per database (Enterprise's multi-tenancy): switch only
    // when asked for another one, so Community's single database works.
    let db = match database.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) if d != current => {
            s.use_database(d).await?;
            d.to_string()
        }
        _ => current,
    };
    let rows = s.records("SHOW SNAPSHOTS").await?;
    let mut out: Vec<BackupEntry> = rows
        .iter()
        .filter_map(|r| {
            let path = r.get("path").map(as_text).filter(|p| !p.is_empty())?;
            let created = r.get("creation_time").map(as_text).filter(|t| !t.is_empty());
            let mut details = Vec::new();
            if let Some(ts) = r.get("timestamp").map(as_text).filter(|t| !t.is_empty()) {
                details.push(("Timestamp de la transacción".to_string(), ts));
            }
            Some(BackupEntry {
                id: path.clone(),
                database: Some(db.clone()),
                kind: Some("Snapshot".into()),
                started: created.clone(),
                finished: created,
                size: r.get("size").map(as_text).and_then(|t| size(&t)),
                location: Some(path),
                status: Some("completo".into()),
                details,
                restorable: true,
            })
        })
        .collect();
    out.sort_by(|a, b| b.started.cmp(&a.started));
    Ok(out)
}

/// Memgraph's human sizes ("483B", "1.50KiB", "12.00MiB") in bytes.
fn size(t: &str) -> Option<u64> {
    let t = t.trim();
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let n: f64 = t[..split].parse().ok()?;
    let unit = match t[split..].trim() {
        "" | "B" => 1u64,
        "KiB" | "KB" => 1 << 10,
        "MiB" | "MB" => 1 << 20,
        "GiB" | "GB" => 1 << 30,
        "TiB" | "TB" => 1 << 40,
        _ => return None,
    };
    Some((n * unit as f64).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn only_memgraph() {
        assert!(spec(Flavor::Neo4j).is_none());
        assert!(spec(Flavor::Neptune).is_none());
        let s = spec(Flavor::Memgraph).unwrap();
        assert!(s.restore && s.history && !s.delete && !s.server_wide);
        let b = BackupAction::Backup { database: None, options: BTreeMap::new() };
        assert!(matches!(script(Flavor::Neo4j, &b), Err(Error::Unsupported(_))));
        assert!(matches!(script(Flavor::Neptune, &b), Err(Error::Unsupported(_))));
    }

    #[test]
    fn memgraph_scripts() {
        let b = BackupAction::Backup { database: Some("memgraph".into()), options: BTreeMap::new() };
        assert_eq!(script(Flavor::Memgraph, &b).unwrap(), "CREATE SNAPSHOT;");
        let path = "/var/lib/memgraph/snapshots/20260929131451823773_timestamp_390";
        let r = BackupAction::Restore { source: path.into(), database: None, options: BTreeMap::new() };
        assert_eq!(script(Flavor::Memgraph, &r).unwrap(), format!("RECOVER SNAPSHOT '{path}' FORCE;"));
        let opts = BTreeMap::from([("force".to_string(), "false".to_string())]);
        let r = BackupAction::Restore { source: "/tmp/it's".into(), database: None, options: opts };
        assert_eq!(script(Flavor::Memgraph, &r).unwrap(), "RECOVER SNAPSHOT '/tmp/it\\'s';");
        let r = BackupAction::Restore { source: " ".into(), database: None, options: BTreeMap::new() };
        assert!(script(Flavor::Memgraph, &r).is_err());
        assert!(matches!(script(Flavor::Memgraph, &BackupAction::Delete { source: path.into() }), Err(Error::Unsupported(_))));
    }

    #[test]
    fn sizes() {
        assert_eq!(size("483B"), Some(483));
        assert_eq!(size("1.50KiB"), Some(1536));
        assert_eq!(size("2.00MiB"), Some(2 << 20));
        assert_eq!(size("?"), None);
    }
}
