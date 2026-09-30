//! Native backups (docs/backups.md) with table snapshots. BigQuery
//! snapshots one table at a time (`CREATE SNAPSHOT TABLE … CLONE …`), so a
//! backup of a dataset is a snapshot of each of its tables, all taken at
//! the same moment (`FOR SYSTEM_TIME AS OF`) into a snapshot dataset
//! (`<dataset>_backups` unless another is chosen). The scripts are
//! GoogleSQL scripts that walk INFORMATION_SCHEMA with `FOR … IN` and
//! `EXECUTE IMMEDIATE`, so they pick up the tables the dataset has when they
//! run.
//!
//! The history groups `INFORMATION_SCHEMA.TABLE_SNAPSHOTS` of the dataset's
//! region by snapshot dataset and snapshot time: one entry per backup,
//! including snapshots made outside DBine. An entry's id is
//! `<snapshot dataset>/<dataset>@<snapshot time>`.

use crate::ddl::{ident, lit};
use crate::BigQuerySession;
use dbine_driver::{BackupAction, BackupEntry, BackupSpec, Error, Field, FieldKind, Result};
use std::collections::BTreeMap;

pub fn spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![
            Field::new("snapshot_dataset", "Dataset de los snapshots", FieldKind::Text)
                .placeholder("vacío = <dataset>_backups")
                .help("Se crea si no existe, en la misma región del dataset."),
            Field::new("expire_days", "Vencen a los (días)", FieldKind::Number)
                .placeholder("vacío = no vencen")
                .help("BigQuery borra cada snapshot al vencer."),
            Field::new("as_of", "Momento de los datos", FieldKind::Text)
                .placeholder("vacío = ahora (2026-09-29 10:00:00 UTC)")
                .help("Un momento pasado dentro de la ventana de time travel del dataset (7 días por defecto)."),
        ],
        restore: true,
        restore_options: vec![Field::new("replace", "Reemplazar las tablas que ya existan", FieldKind::Bool)
            .default_value("false")
            .help("Sin esto, restaurar en el mismo dataset falla con las tablas que siguen ahí.")],
        delete: true,
        history: true,
        server_wide: false,
        script_database: "",
        note: "Cada backup es un snapshot de cada tabla del dataset, todas al mismo momento, en un dataset de snapshots de la misma región. Solo las tablas: las vistas, las rutinas y las tablas externas quedan afuera (van en las copias de DBine). Un snapshot solo paga lo que cambia la tabla después.",
    }
}

fn opt<'a>(options: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    options.get(key).map(|v| v.trim()).unwrap_or_default()
}

/// A dataset id: letters, digits and `_`, up to 1024.
fn check_dataset(d: &str) -> Result<&str> {
    if !d.is_empty() && d.len() <= 1024 && d.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(d)
    } else {
        Err(Error::Query(format!("'{d}' no es un dataset válido: letras, números y _")))
    }
}

/// A snapshot time as the history writes it (`2026-09-29T14:00:00.123456Z`).
fn check_time(t: &str) -> Result<&str> {
    if !t.is_empty() && t.chars().all(|c| c.is_ascii_digit() || "-:.TZ ".contains(c)) {
        Ok(t)
    } else {
        Err(Error::Query(format!("'{t}' no es un momento válido (2026-09-29T14:00:00Z)")))
    }
}

/// `<snapshot dataset>/<dataset>@<snapshot time>`.
fn parse_source(source: &str) -> Result<(&str, &str, &str)> {
    let bad = || Error::Query(format!("'{source}' no es un backup: tiene que ser <dataset de snapshots>/<dataset>@<momento>"));
    let (sets, time) = source.trim().split_once('@').ok_or_else(bad)?;
    let (snap, base) = sets.split_once('/').ok_or_else(bad)?;
    Ok((check_dataset(snap.trim())?, check_dataset(base.trim())?, check_time(time.trim())?))
}

/// The snapshots of one backup, as the `FOR` loop's query.
fn snapshots_of(snap: &str, base: &str, time: &str) -> String {
    format!(
        "SELECT table_name, base_table_name FROM {}.INFORMATION_SCHEMA.TABLE_SNAPSHOTS\n  WHERE base_table_schema = {} AND snapshot_time = TIMESTAMP {}\n  ORDER BY base_table_name",
        ident(snap),
        lit(base),
        lit(time)
    )
}

pub fn script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { database, options } => {
            let ds = check_dataset(database.as_deref().map(str::trim).unwrap_or_default())?;
            let default_snap = format!("{ds}_backups");
            let snap = match opt(options, "snapshot_dataset") {
                "" => default_snap.as_str(),
                s => check_dataset(s)?,
            };
            if snap == ds {
                return Err(Error::Query("El dataset de los snapshots tiene que ser otro".into()));
            }
            let at = match opt(options, "as_of") {
                "" => "CURRENT_TIMESTAMP()".to_string(),
                t => format!("TIMESTAMP {}", lit(t)),
            };
            let expire = match opt(options, "expire_days") {
                "" => String::new(),
                d => {
                    let days: u32 = d.parse().ok().filter(|d| *d > 0).ok_or_else(|| Error::Query(format!("'{d}' no es una cantidad de días válida")))?;
                    format!(" OPTIONS(expiration_timestamp = TIMESTAMP_ADD(CURRENT_TIMESTAMP(), INTERVAL {days} DAY))")
                }
            };
            let stmt = format!("CREATE SNAPSHOT TABLE {}.`%s_%s` CLONE {}.`%s` FOR SYSTEM_TIME AS OF %T{expire}", ident(snap), ident(ds));
            Ok(format!(
                "-- Snapshot de cada tabla de {ds} en {snap}, todas al mismo momento.\n\
                 DECLARE snapshot_time TIMESTAMP DEFAULT {at};\n\
                 DECLARE stamp STRING DEFAULT FORMAT_TIMESTAMP('%Y%m%d_%H%M%S', snapshot_time);\n\
                 CREATE SCHEMA IF NOT EXISTS {};\n\
                 FOR t IN (\n  SELECT table_name FROM {}.INFORMATION_SCHEMA.TABLES\n  WHERE table_type IN ('BASE TABLE', 'CLONE')\n  ORDER BY table_name\n)\nDO\n  \
                 EXECUTE IMMEDIATE FORMAT({}, t.table_name, stamp, t.table_name, snapshot_time);\n\
                 END FOR;\n",
                ident(snap),
                ident(ds),
                lit(&stmt)
            ))
        }
        BackupAction::Restore { source, database, options } => {
            let (snap, base, time) = parse_source(source)?;
            let target = match database.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
                Some(t) => check_dataset(t)?,
                None => base,
            };
            let create = if opt(options, "replace") == "true" { "CREATE OR REPLACE TABLE" } else { "CREATE TABLE" };
            let stmt = format!("{create} {}.`%s` CLONE {}.`%s`", ident(target), ident(snap));
            Ok(format!(
                "-- Restaura el backup de {base} del {time} en {target}.\n\
                 IF NOT EXISTS ({q}) THEN\n  RAISE USING MESSAGE = {missing};\nEND IF;\n\
                 CREATE SCHEMA IF NOT EXISTS {};\n\
                 FOR s IN (\n  {q}\n)\nDO\n  \
                 EXECUTE IMMEDIATE FORMAT({}, s.base_table_name, s.table_name);\n\
                 END FOR;\n",
                ident(target),
                lit(&stmt),
                q = snapshots_of(snap, base, time),
                missing = lit(&format!("No hay snapshots de {base} del {time} en {snap}")),
            ))
        }
        BackupAction::Delete { source } => {
            let (snap, base, time) = parse_source(source)?;
            let stmt = format!("DROP SNAPSHOT TABLE IF EXISTS {}.`%s`", ident(snap));
            Ok(format!(
                "-- Borra los snapshots del backup de {base} del {time}.\n\
                 FOR s IN (\n  {}\n)\nDO\n  \
                 EXECUTE IMMEDIATE FORMAT({}, s.table_name);\n\
                 END FOR;\n",
                snapshots_of(snap, base, time),
                lit(&stmt)
            ))
        }
    }
}

// -- history -----------------------------------------------------------------

/// The history query over a region's TABLE_SNAPSHOTS for one dataset.
fn history_sql(region: &str, dataset: &str) -> String {
    format!(
        "SELECT table_schema, base_table_schema,\n  \
         FORMAT_TIMESTAMP('%Y-%m-%dT%H:%M:%E6SZ', snapshot_time) AS snapshot_time,\n  \
         COUNT(*) AS tables, STRING_AGG(base_table_name, ', ' ORDER BY base_table_name) AS names\n\
         FROM {}.INFORMATION_SCHEMA.TABLE_SNAPSHOTS\n\
         WHERE base_table_schema = {}\n\
         GROUP BY 1, 2, 3\n\
         ORDER BY 3 DESC",
        ident(&format!("region-{}", region.to_ascii_lowercase())),
        lit(dataset)
    )
}

/// History rows (lowercase column names) as entries.
pub fn entries(rows: &[crate::ddl::Row]) -> Vec<BackupEntry> {
    rows.iter()
        .filter_map(|r| {
            let get = |k: &str| r.get(k).cloned();
            let (snap, base, time) = (get("table_schema")?, get("base_table_schema")?, get("snapshot_time")?);
            let mut details = Vec::new();
            if let Some(n) = get("tables") {
                details.push(("Tablas".to_string(), n));
            }
            if let Some(n) = get("names") {
                details.push(("Incluye".to_string(), n));
            }
            Some(BackupEntry {
                id: format!("{snap}/{base}@{time}"),
                database: Some(base),
                kind: Some("Snapshot".into()),
                started: Some(time.clone()),
                finished: Some(time),
                size: None,
                location: Some(snap),
                status: Some("completo".into()),
                details,
                restorable: true,
            })
        })
        .collect()
}

pub async fn history(s: &BigQuerySession, database: Option<&str>) -> Result<Vec<BackupEntry>> {
    let Some(ds) = database.map(str::trim).filter(|d| !d.is_empty()).map(str::to_string).or_else(|| s.dataset.clone()) else {
        return Ok(Vec::new());
    };
    let meta = s.api.get(&["datasets", &ds], &[]).await?;
    let region = meta.get("location").and_then(|l| l.as_str()).map(str::to_string).or_else(|| s.api.location.clone()).unwrap_or_else(|| "US".into());
    Ok(entries(&s.named_rows(&history_sql(&region, &ds)).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn backup_script() {
        let s = script(&BackupAction::Backup { database: Some("ventas".into()), options: opts(&[("expire_days", "30")]) }).unwrap();
        assert!(s.contains("DECLARE snapshot_time TIMESTAMP DEFAULT CURRENT_TIMESTAMP();"), "{s}");
        assert!(s.contains("CREATE SCHEMA IF NOT EXISTS `ventas_backups`;"), "{s}");
        assert!(s.contains("FROM `ventas`.INFORMATION_SCHEMA.TABLES"), "{s}");
        assert!(s.contains(
            "EXECUTE IMMEDIATE FORMAT('CREATE SNAPSHOT TABLE `ventas_backups`.`%s_%s` CLONE `ventas`.`%s` FOR SYSTEM_TIME AS OF %T OPTIONS(expiration_timestamp = TIMESTAMP_ADD(CURRENT_TIMESTAMP(), INTERVAL 30 DAY))', t.table_name, stamp, t.table_name, snapshot_time);"
        ), "{s}");
        let s = script(&BackupAction::Backup { database: Some("ventas".into()), options: opts(&[("snapshot_dataset", "bk"), ("as_of", "2026-09-29 10:00:00 UTC")]) }).unwrap();
        assert!(s.contains("DEFAULT TIMESTAMP '2026-09-29 10:00:00 UTC';"), "{s}");
        assert!(s.contains("`bk`.`%s_%s`") && !s.contains("OPTIONS"), "{s}");
        assert!(script(&BackupAction::Backup { database: Some("ventas".into()), options: opts(&[("snapshot_dataset", "ventas")]) }).is_err());
        assert!(script(&BackupAction::Backup { database: Some("ven`tas".into()), options: BTreeMap::new() }).is_err());
        assert!(script(&BackupAction::Backup { database: Some("v".into()), options: opts(&[("expire_days", "0")]) }).is_err());
    }

    #[test]
    fn restore_and_delete_scripts() {
        let src = "ventas_backups/ventas@2026-09-29T14:00:00.123456Z";
        let r = script(&BackupAction::Restore { source: src.into(), database: Some("ventas_copia".into()), options: BTreeMap::new() }).unwrap();
        assert!(r.contains("FROM `ventas_backups`.INFORMATION_SCHEMA.TABLE_SNAPSHOTS"), "{r}");
        assert!(r.contains("WHERE base_table_schema = 'ventas' AND snapshot_time = TIMESTAMP '2026-09-29T14:00:00.123456Z'"), "{r}");
        assert!(r.contains("CREATE SCHEMA IF NOT EXISTS `ventas_copia`;"), "{r}");
        assert!(r.contains("FORMAT('CREATE TABLE `ventas_copia`.`%s` CLONE `ventas_backups`.`%s`', s.base_table_name, s.table_name)"), "{r}");
        assert!(r.contains("RAISE USING MESSAGE"), "{r}");
        let r = script(&BackupAction::Restore { source: src.into(), database: None, options: opts(&[("replace", "true")]) }).unwrap();
        assert!(r.contains("CREATE OR REPLACE TABLE `ventas`.`%s`"), "{r}");
        let d = script(&BackupAction::Delete { source: src.into() }).unwrap();
        assert!(d.contains("FORMAT('DROP SNAPSHOT TABLE IF EXISTS `ventas_backups`.`%s`', s.table_name)"), "{d}");
        assert!(script(&BackupAction::Delete { source: "ventas@x".into() }).is_err());
        assert!(script(&BackupAction::Delete { source: "a/b@2026'; DROP".into() }).is_err());
    }

    #[test]
    fn history_rows() {
        let q = history_sql("EU", "ventas");
        assert!(q.contains("FROM `region-eu`.INFORMATION_SCHEMA.TABLE_SNAPSHOTS") && q.contains("WHERE base_table_schema = 'ventas'"), "{q}");
        let row: crate::ddl::Row = [
            ("table_schema", "ventas_backups"),
            ("base_table_schema", "ventas"),
            ("snapshot_time", "2026-09-29T14:00:00.123456Z"),
            ("tables", "2"),
            ("names", "clientes, pedidos"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let e = entries(&[row]);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].id, "ventas_backups/ventas@2026-09-29T14:00:00.123456Z");
        assert_eq!(e[0].location.as_deref(), Some("ventas_backups"));
        assert!(e[0].details.contains(&("Incluye".into(), "clientes, pedidos".into())));
        // The id round-trips into the restore script.
        assert!(parse_source(&e[0].id).is_ok());
    }
}
