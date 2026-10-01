//! The server's own backups (docs/backups.md) for Delta tables in Unity
//! Catalog. Databricks backs up tables, not schemas or catalogs, so a
//! backup of a schema is a new schema (`<schema>_bkp_<date>`, marked by
//! its comment) with a `DEEP CLONE` of each of its Delta tables: an
//! independent copy of the data and the metadata. Restoring clones each
//! table back (`CREATE OR REPLACE TABLE … DEEP CLONE …`, which keeps the
//! target's history, so a restore can be undone with `RESTORE TABLE`);
//! deleting drops the backup schema.
//!
//! The scripts are SQL scripting blocks (`BEGIN … END`, a `FOR` over
//! `information_schema.tables` and `EXECUTE IMMEDIATE`), so they clone
//! the tables there are when they run; `execute` sends such a block
//! whole.
//!
//! A single table's own versions (`DESCRIBE HISTORY`, `RESTORE TABLE …
//! TO VERSION AS OF …`) stay in the editor: the tab is of a catalog.

use crate::ddl::{lit, q, Row};
use crate::DatabricksSession;
use dbine_driver::{BackupAction, BackupEntry, BackupSpec, Error, Field, FieldKind, Result};
use std::collections::BTreeMap;

/// The comment of a backup schema: this, then the source schema's name.
const MARK: &str = "DBine backup of ";

pub fn spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![
            Field::new("schema", "Esquema", FieldKind::Text)
                .help("El esquema del catálogo a respaldar: cada una de sus tablas Delta se copia con DEEP CLONE."),
            Field::new("target", "Esquema del backup", FieldKind::Text)
                .placeholder("(<esquema>_bkp_fecha)")
                .help("Se crea en el mismo catálogo; no tiene que existir."),
        ],
        restore: true,
        restore_options: vec![Field::new("schema", "Esquema de destino", FieldKind::Text)
            .placeholder("(el de origen del backup)")
            .help("Cada tabla del backup se copia ahí con DEEP CLONE y reemplaza la que ya exista (su historial se conserva, así que se puede volver atrás con RESTORE TABLE). Las tablas que no están en el backup quedan como están.")],
        delete: true,
        history: true,
        server_wide: false,
        script_database: "",
        note: "Un backup es un esquema nuevo del catálogo con una copia completa (DEEP CLONE) de cada tabla \
               Delta del esquema: los datos quedan en el almacenamiento del catálogo. Las vistas, funciones y \
               tablas que no son Delta no se copian. Los scripts usan SQL scripting (Databricks SQL o DBR 16.3 \
               o posterior). Cada tabla también guarda sus versiones: DESCRIBE HISTORY y RESTORE TABLE … TO \
               VERSION AS OF desde el editor.",
    }
}

fn opt<'a>(o: &'a BTreeMap<String, String>, k: &str) -> Option<&'a str> {
    o.get(k).map(|s| s.trim()).filter(|s| !s.is_empty())
}

/// `20260929_154500` (UTC).
fn stamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let (days, rest) = ((secs / 86_400) as i64, secs % 86_400);
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}_{:02}{:02}{:02}", rest / 3600, rest % 3600 / 60, rest % 60)
}

/// The block that deep-clones every Delta table of `cat.from` into
/// `cat.to`; `create` opens it (the schema's creation).
fn clone_block(cat: &str, from: &str, to: &str, create: &str, replace: bool) -> String {
    let verb = if replace { "CREATE OR REPLACE TABLE" } else { "CREATE TABLE" };
    let target = lit(&format!("{verb} {}.{}.", q(cat), q(to)));
    let source = lit(&format!(" DEEP CLONE {}.{}.", q(cat), q(from)));
    // EXECUTE IMMEDIATE takes a variable, not any expression.
    format!(
        "BEGIN\n  DECLARE stmt STRING;\n  {create};\n  FOR t AS SELECT table_name FROM {}.information_schema.tables\n      \
         WHERE table_schema = {} AND data_source_format = 'DELTA' AND table_type IN ('MANAGED', 'EXTERNAL')\n  DO\n    \
         SET stmt = {target} || '`' || replace(t.table_name, '`', '``') || '`'\n      \
         || {source} || '`' || replace(t.table_name, '`', '``') || '`';\n    \
         EXECUTE IMMEDIATE stmt;\n  END FOR;\nEND;",
        q(cat),
        lit(from),
    )
}

/// The schema a backup named `<schema>_bkp_<yyyymmdd>_<hhmmss>` is of.
fn source_of(backup: &str) -> Option<&str> {
    let (base, ts) = backup.rsplit_once("_bkp_")?;
    let ok = ts.len() == 15 && ts.char_indices().all(|(i, c)| if i == 8 { c == '_' } else { c.is_ascii_digit() });
    (ok && !base.is_empty()).then_some(base)
}

pub fn script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { database, options } => {
            let cat = database.as_deref().filter(|d| !d.is_empty()).ok_or_else(|| Error::Query("falta el catálogo".into()))?;
            let schema = opt(options, "schema").ok_or_else(|| Error::Query("falta el esquema a respaldar".into()))?;
            let target = opt(options, "target").map_or_else(|| format!("{schema}_bkp_{}", stamp()), str::to_string);
            if target == schema {
                return Err(Error::Query("el backup tiene que ir a otro esquema".into()));
            }
            let create =
                format!("CREATE SCHEMA {}.{} COMMENT {}", q(cat), q(&target), lit(&format!("{MARK}{schema}")));
            Ok(clone_block(cat, schema, &target, &create, false))
        }
        BackupAction::Restore { source, database, options } => {
            let cat = database.as_deref().filter(|d| !d.is_empty()).ok_or_else(|| Error::Query("falta el catálogo".into()))?;
            let backup = source.trim();
            if backup.is_empty() {
                return Err(Error::Query("falta el esquema del backup".into()));
            }
            let schema = opt(options, "schema").or_else(|| source_of(backup)).ok_or_else(|| {
                Error::Query("falta el esquema de destino (el nombre del backup no dice de cuál es)".into())
            })?;
            if schema == backup {
                return Err(Error::Query("el destino no puede ser el mismo backup".into()));
            }
            let create = format!("CREATE SCHEMA IF NOT EXISTS {}.{}", q(cat), q(schema));
            Ok(clone_block(cat, backup, schema, &create, true))
        }
        BackupAction::Delete { source } => {
            let backup = source.trim();
            if backup.is_empty() {
                return Err(Error::Query("falta el esquema del backup".into()));
            }
            // The catalog is the tab's, where the script runs.
            Ok(format!("DROP SCHEMA {} CASCADE;", q(backup)))
        }
    }
}

/// The statements of a script, compound `BEGIN … END` blocks (not
/// `BEGIN TRANSACTION`) whole.
#[cfg(test)]
pub(crate) fn statements(text: &str) -> Vec<String> {
    crate::script::units(text).into_iter().map(|u| u.text).collect()
}

// -- history -----------------------------------------------------------------

fn get(r: &Row, k: &str) -> Option<String> {
    r.get(k).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn entry(r: &Row, cat: &str) -> BackupEntry {
    let name = get(r, "schema_name").unwrap_or_default();
    let from = get(r, "comment").and_then(|c| c.strip_prefix(MARK).map(str::to_string));
    let mut details = vec![];
    if let Some(f) = &from {
        details.push(("Esquema de origen".to_string(), f.clone()));
    }
    if let Some(n) = get(r, "tables") {
        details.push(("Tablas".into(), n));
    }
    if let Some(b) = get(r, "created_by") {
        details.push(("Creado por".into(), b));
    }
    BackupEntry {
        location: Some(format!("{}.{}", q(cat), q(&name))),
        id: name,
        database: Some(cat.to_string()),
        kind: Some("DEEP CLONE".into()),
        started: get(r, "created").map(|t| t.replacen(' ', "T", 1)),
        finished: None,
        size: None,
        status: None,
        details,
        restorable: true,
    }
}

impl DatabricksSession {
    /// The backup schemas of `catalog` (the session's when `None`).
    pub(crate) async fn backup_history(&self, catalog: Option<&str>) -> Result<Vec<BackupEntry>> {
        let cat = match catalog.filter(|c| !c.is_empty()) {
            Some(c) => c.to_string(),
            None => self.catalog()?.to_string(),
        };
        let c = q(&cat);
        let sql = format!(
            "SELECT s.schema_name, s.comment, s.created_by, date_format(s.created, 'yyyy-MM-dd HH:mm:ss') AS created,
                    CAST(count(t.table_name) AS STRING) AS tables
               FROM {c}.information_schema.schemata s
               LEFT JOIN {c}.information_schema.tables t ON t.table_schema = s.schema_name
              WHERE s.comment LIKE {}
              GROUP BY s.schema_name, s.comment, s.created_by, s.created
              ORDER BY s.created DESC",
            lit(&format!("{MARK}%"))
        );
        Ok(self.named_rows(&sql).await?.iter().map(|r| entry(r, &cat)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn backup_script() {
        let s = script(&BackupAction::Backup {
            database: Some("main".into()),
            options: opts(&[("schema", "ven`tas"), ("target", "ven'tas_bkp")]),
        })
        .unwrap();
        assert_eq!(
            s,
            "BEGIN\n  DECLARE stmt STRING;\n  CREATE SCHEMA `main`.`ven'tas_bkp` COMMENT 'DBine backup of ven`tas';\n  \
             FOR t AS SELECT table_name FROM `main`.information_schema.tables\n      \
             WHERE table_schema = 'ven`tas' AND data_source_format = 'DELTA' AND table_type IN ('MANAGED', 'EXTERNAL')\n  DO\n    \
             SET stmt = 'CREATE TABLE `main`.`ven\\'tas_bkp`.' || '`' || replace(t.table_name, '`', '``') || '`'\n      \
             || ' DEEP CLONE `main`.`ven``tas`.' || '`' || replace(t.table_name, '`', '``') || '`';\n    \
             EXECUTE IMMEDIATE stmt;\n  END FOR;\nEND;"
        );
        assert_eq!(statements(&s).len(), 1);
        let auto = script(&BackupAction::Backup { database: Some("main".into()), options: opts(&[("schema", "sales")]) })
            .unwrap();
        assert!(auto.contains("CREATE SCHEMA `main`.`sales_bkp_2"), "{auto}");
        let name = auto.split('`').nth(3).unwrap();
        assert_eq!(source_of(name), Some("sales"));
        assert!(script(&BackupAction::Backup { database: Some("main".into()), options: opts(&[]) }).is_err());
        assert!(script(&BackupAction::Backup {
            database: Some("main".into()),
            options: opts(&[("schema", "a"), ("target", "a")])
        })
        .is_err());
    }

    #[test]
    fn restore_and_delete_scripts() {
        let r = |src: &str, kv: &[(&str, &str)]| {
            script(&BackupAction::Restore { source: src.into(), database: Some("main".into()), options: opts(kv) })
        };
        let s = r("sales_bkp_20260929_154500", &[]).unwrap();
        assert!(s.contains("CREATE SCHEMA IF NOT EXISTS `main`.`sales`;"), "{s}");
        assert!(s.contains("WHERE table_schema = 'sales_bkp_20260929_154500'"), "{s}");
        assert!(s.contains("'CREATE OR REPLACE TABLE `main`.`sales`.'"), "{s}");
        assert!(s.contains("' DEEP CLONE `main`.`sales_bkp_20260929_154500`.'"), "{s}");
        assert!(r("custom", &[]).is_err());
        assert!(r("custom", &[("schema", "sales")]).unwrap().contains("`main`.`sales`"));
        assert!(r("x", &[("schema", "x")]).is_err());
        assert_eq!(
            script(&BackupAction::Delete { source: "s`_bkp".into() }).unwrap(),
            "DROP SCHEMA `s``_bkp` CASCADE;"
        );
        assert_eq!(source_of("a_bkp_2026092_1545001"), None);
        assert_eq!(source_of("_bkp_20260929_154500"), None);
    }

    #[test]
    fn scripting_blocks_go_whole() {
        assert_eq!(statements("SELECT 1; SELECT 2;").len(), 2);
        assert_eq!(statements("BEGIN\n SELECT 1;\n SELECT 2;\nEND;").len(), 1);
        assert_eq!(statements("begin\n select 1;\nend"), vec!["begin\n select 1;\nend"]);
        assert_eq!(statements("BEGIN TRANSACTION; INSERT INTO t VALUES (1); COMMIT;").len(), 3);
        assert_eq!(statements("BEGIN; SELECT 1; COMMIT;").len(), 3);
        assert_eq!(statements("BEGIN SELECT 1; END; SELECT 2;").len(), 2);
    }

    #[test]
    fn history_rows() {
        let row: Row = [
            ("schema_name", "sales_bkp_20260929_154500"),
            ("comment", "DBine backup of sales"),
            ("created", "2026-09-29 15:45:00"),
            ("tables", "3"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let e = entry(&row, "main");
        assert_eq!(e.id, "sales_bkp_20260929_154500");
        assert_eq!(e.started.as_deref(), Some("2026-09-29T15:45:00"));
        assert_eq!(e.location.as_deref(), Some("`main`.`sales_bkp_20260929_154500`"));
        assert!(e.details.contains(&("Esquema de origen".into(), "sales".into())));
    }
}
