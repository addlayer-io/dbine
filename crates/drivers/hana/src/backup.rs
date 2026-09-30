//! The server's own backups (docs/backups.md): `BACKUP DATA [DIFFERENTIAL |
//! INCREMENTAL] [FOR <tenant>] USING FILE | BACKINT ('<prefix>')`, the
//! history from the backup catalog (`M_BACKUP_CATALOG` and its files; from
//! SYSTEMDB, `SYS_DATABASES.*` for every tenant), `RECOVER DATA FOR
//! <tenant> USING BACKUP_ID <id> CLEAR LOG` (from SYSTEMDB, with the tenant
//! stopped) and `BACKUP CATALOG DELETE … BACKUP_ID <id> COMPLETE`.
//!
//! A backup is of a whole database (a tenant or SYSTEMDB), not of a
//! schema, which is what DBine calls a database here: the tab opens from
//! the connection.

use crate::{quote, text, HanaSession};
use dbine_driver::{BackupAction, BackupEntry, BackupSpec, Error, Field, FieldKind, Result};
use hdbconnect_async::HdbValue;
use std::collections::BTreeMap;

pub fn spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![
            Field::new(
                "kind",
                "Tipo",
                FieldKind::Select(vec![
                    ("complete", "Completo"),
                    ("differential", "Diferencial (lo que cambió desde el último completo)"),
                    ("incremental", "Incremental (lo que cambió desde el último backup de datos)"),
                ]),
            )
            .default_value("complete"),
            Field::new("tenant", "Base tenant", FieldKind::Text)
                .placeholder("(la de la conexión)")
                .help("Solo conectado a SYSTEMDB: el tenant a respaldar. Vacío: la base a la que está conectada la sesión."),
            Field::new(
                "destination",
                "Destino",
                FieldKind::Select(vec![("file", "Archivos del servidor"), ("backint", "Backint (herramienta de backups)")]),
            )
            .default_value("file"),
            Field::new("prefix", "Prefijo", FieldKind::Text)
                .placeholder("(DBINE_fecha)")
                .help("Nombre de los archivos. Sin ruta, van a la carpeta de backups de datos del servidor (basepath_databackup); también puede ser una ruta absoluta del servidor."),
            Field::new("comment", "Comentario", FieldKind::Text),
        ],
        restore: true,
        restore_options: vec![],
        delete: true,
        history: true,
        server_wide: true,
        script_database: "",
        note: "Hacer un backup pide BACKUP ADMIN o BACKUP OPERATOR; los archivos quedan en el servidor \
               (basepath_databackup) o en la herramienta de Backint. Restaurar se hace conectado a SYSTEMDB \
               (puerto 3<instancia>13, sin tenant): el script detiene el tenant, lo recupera desde un backup \
               completo sin aplicar logs y el tenant vuelve a arrancar solo. SYSTEMDB no se restaura por SQL. \
               Eliminar saca el backup del catálogo y borra sus archivos; el último backup de datos no se \
               puede borrar.",
    }
}

fn opt<'a>(o: &'a BTreeMap<String, String>, k: &str) -> Option<&'a str> {
    o.get(k).map(|s| s.trim()).filter(|s| !s.is_empty())
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Tenant names are case-insensitive and kept in uppercase.
fn tenant(name: &str) -> String {
    quote(&name.trim().to_uppercase())
}

/// `DBINE_20260929_154500` (UTC).
fn default_prefix() -> String {
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
    format!("DBINE_{y:04}{m:02}{d:02}_{:02}{:02}{:02}", rest / 3600, rest % 3600 / 60, rest % 60)
}

/// A history id: `<backup id>` of the connected database, or
/// `<backup id>@<tenant>` of another one (read from SYSTEMDB).
fn parse_id(source: &str) -> Option<(u64, Option<&str>)> {
    let (id, db) = match source.trim().split_once('@') {
        Some((id, db)) => (id, Some(db.trim()).filter(|d| !d.is_empty())),
        None => (source.trim(), None),
    };
    Some((id.trim().parse().ok()?, db))
}

pub fn script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { options, .. } => {
            let kind = match opt(options, "kind").unwrap_or("complete") {
                "differential" => " DIFFERENTIAL",
                "incremental" => " INCREMENTAL",
                _ => "",
            };
            let target = opt(options, "tenant").map(|t| format!(" FOR {}", tenant(t))).unwrap_or_default();
            let using = if opt(options, "destination") == Some("backint") { "BACKINT" } else { "FILE" };
            let prefix = opt(options, "prefix").map_or_else(default_prefix, str::to_string);
            let mut sql = format!("BACKUP DATA{kind}{target} USING {using} ({})", lit(&prefix));
            if let Some(c) = opt(options, "comment") {
                sql.push_str(&format!(" COMMENT {}", lit(c)));
            }
            Ok(sql + ";")
        }
        BackupAction::Restore { source, database, .. } => {
            let source = source.trim();
            if source.is_empty() {
                return Err(Error::Query("falta el backup a restaurar".into()));
            }
            let parsed = parse_id(source);
            let db = database
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .or(parsed.and_then(|(_, db)| db))
                .ok_or_else(|| Error::Query("falta el tenant a restaurar".into()))?;
            if db.eq_ignore_ascii_case("SYSTEMDB") {
                return Err(Error::Unsupported(
                    "SYSTEMDB no se restaura por SQL: se recupera con el sistema detenido (HANA cockpit o recoverSys.py)".into(),
                ));
            }
            let using = match parsed {
                Some((id, from)) => {
                    if from.is_some_and(|f| !f.eq_ignore_ascii_case(db)) {
                        return Err(Error::Unsupported(format!(
                            "el backup {id} es de {}: DBine restaura cada backup en su propio tenant",
                            from.unwrap_or_default()
                        )));
                    }
                    format!("USING BACKUP_ID {id}")
                }
                // Not an id: the prefix (or path and prefix) of the files.
                None => format!("USING FILE ({})", lit(source)),
            };
            let t = tenant(db);
            Ok(format!("ALTER SYSTEM STOP DATABASE {t};\nRECOVER DATA FOR {t} {using} CLEAR LOG;"))
        }
        BackupAction::Delete { source } => {
            let (id, db) = parse_id(source)
                .ok_or_else(|| Error::Query(format!("«{}» no es un id de backup del catálogo", source.trim())))?;
            let target = db.map(|d| format!(" FOR {}", tenant(d))).unwrap_or_default();
            Ok(format!("BACKUP CATALOG DELETE{target} BACKUP_ID {id} COMPLETE;"))
        }
    }
}

// -- history -----------------------------------------------------------------

/// Data backups and snapshots (not the log backups, taken every few
/// minutes), with their files' total size, one of their paths and the
/// destination type.
fn history_sql(all: bool) -> String {
    let (catalog, files, db, on) = if all {
        (
            "SYS_DATABASES.M_BACKUP_CATALOG",
            "SYS_DATABASES.M_BACKUP_CATALOG_FILES",
            "C.DATABASE_NAME",
            " AND F.DATABASE_NAME = C.DATABASE_NAME",
        )
    } else {
        ("M_BACKUP_CATALOG", "M_BACKUP_CATALOG_FILES", "CAST(NULL AS NVARCHAR(256))", "")
    };
    format!(
        "SELECT TO_VARCHAR(C.BACKUP_ID), C.ENTRY_TYPE_NAME, C.STATE_NAME,
       TO_VARCHAR(C.UTC_START_TIME, 'YYYY-MM-DD HH24:MI:SS'), TO_VARCHAR(C.UTC_END_TIME, 'YYYY-MM-DD HH24:MI:SS'),
       C.COMMENT, C.MESSAGE, {db},
       (SELECT TO_VARCHAR(SUM(F.BACKUP_SIZE)) FROM {files} F WHERE F.BACKUP_ID = C.BACKUP_ID{on}),
       (SELECT MIN(F.DESTINATION_PATH) FROM {files} F WHERE F.BACKUP_ID = C.BACKUP_ID{on}),
       (SELECT MIN(F.DESTINATION_TYPE_NAME) FROM {files} F WHERE F.BACKUP_ID = C.BACKUP_ID{on})
  FROM {catalog} C
 WHERE C.ENTRY_TYPE_NAME <> 'log backup'
 ORDER BY C.UTC_START_TIME DESC
 LIMIT 1000"
    )
}

fn kind(t: &str) -> String {
    match t {
        "complete data backup" => "Completo",
        "differential data backup" => "Diferencial",
        "incremental data backup" => "Incremental",
        "data snapshot" => "Snapshot",
        other => other,
    }
    .to_string()
}

fn at(r: &[HdbValue<'static>], i: usize) -> Option<String> {
    r.get(i).and_then(text).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn when(s: Option<String>) -> Option<String> {
    s.filter(|s| !s.starts_with("1970")).map(|s| format!("{}Z", s.replacen(' ', "T", 1)))
}

/// One catalog row; `current` is the connected database's name.
fn entry(r: &[HdbValue<'static>], current: &str) -> BackupEntry {
    let backup_id = at(r, 0).unwrap_or_default();
    let database = at(r, 7).unwrap_or_else(|| current.to_string());
    let other = !database.eq_ignore_ascii_case(current);
    let type_name = at(r, 1).unwrap_or_default();
    let status = at(r, 2);
    let mut details = vec![("Id".to_string(), backup_id.clone())];
    if let Some(d) = at(r, 10) {
        details.push(("Destino".into(), d));
    }
    if let Some(c) = at(r, 5) {
        details.push(("Comentario".into(), c));
    }
    if let Some(m) = at(r, 6) {
        details.push(("Mensaje".into(), m));
    }
    BackupEntry {
        id: if other && !database.is_empty() { format!("{backup_id}@{database}") } else { backup_id },
        restorable: status.as_deref() == Some("successful")
            && type_name == "complete data backup"
            && !database.eq_ignore_ascii_case("SYSTEMDB"),
        database: Some(database).filter(|d| !d.is_empty()),
        kind: Some(kind(&type_name)),
        started: when(at(r, 3)),
        finished: when(at(r, 4)),
        size: at(r, 8).and_then(|s| s.parse().ok()),
        location: at(r, 9),
        status,
        details,
    }
}

impl HanaSession {
    /// From SYSTEMDB, every database's; from a tenant, its own.
    pub(crate) async fn backup_history(&self) -> Result<Vec<BackupEntry>> {
        let current = self
            .rows("SELECT DATABASE_NAME FROM M_DATABASE", &[])
            .await?
            .first()
            .and_then(|r| at(r, 0))
            .unwrap_or_default();
        let rows = if current.eq_ignore_ascii_case("SYSTEMDB") {
            match self.rows(&history_sql(true), &[]).await {
                Ok(r) => r,
                Err(_) => self.rows(&history_sql(false), &[]).await?,
            }
        } else {
            self.rows(&history_sql(false), &[]).await?
        };
        Ok(rows.iter().map(|r| entry(r, &current)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn backup(kv: &[(&str, &str)]) -> String {
        script(&BackupAction::Backup { database: None, options: opts(kv) }).unwrap()
    }

    fn restore(source: &str, db: Option<&str>) -> Result<String> {
        script(&BackupAction::Restore { source: source.into(), database: db.map(Into::into), options: opts(&[]) })
    }

    #[test]
    fn backup_scripts() {
        assert_eq!(backup(&[("prefix", "full_1")]), "BACKUP DATA USING FILE ('full_1');");
        assert_eq!(
            backup(&[("kind", "differential"), ("tenant", "h01"), ("prefix", "/b/o'k"), ("comment", "a'b")]),
            "BACKUP DATA DIFFERENTIAL FOR \"H01\" USING FILE ('/b/o''k') COMMENT 'a''b';"
        );
        assert_eq!(
            backup(&[("kind", "incremental"), ("destination", "backint"), ("prefix", "x")]),
            "BACKUP DATA INCREMENTAL USING BACKINT ('x');"
        );
        let auto = backup(&[]);
        assert!(auto.starts_with("BACKUP DATA USING FILE ('DBINE_2"), "{auto}");
        assert_eq!(backup(&[("tenant", "a\"b"), ("prefix", "p")]), "BACKUP DATA FOR \"A\"\"B\" USING FILE ('p');");
    }

    #[test]
    fn restore_and_delete_scripts() {
        assert_eq!(
            restore("1727600000123", Some("h01")).unwrap(),
            "ALTER SYSTEM STOP DATABASE \"H01\";\nRECOVER DATA FOR \"H01\" USING BACKUP_ID 1727600000123 CLEAR LOG;"
        );
        assert_eq!(
            restore("42@H01", None).unwrap(),
            "ALTER SYSTEM STOP DATABASE \"H01\";\nRECOVER DATA FOR \"H01\" USING BACKUP_ID 42 CLEAR LOG;"
        );
        assert_eq!(
            restore("/hana/backup/o'k", Some("H02")).unwrap(),
            "ALTER SYSTEM STOP DATABASE \"H02\";\nRECOVER DATA FOR \"H02\" USING FILE ('/hana/backup/o''k') CLEAR LOG;"
        );
        assert!(restore("42@H01", Some("H02")).is_err());
        assert!(restore("42", Some("systemdb")).is_err());
        assert!(restore("42", None).is_err());
        assert!(restore(" ", Some("H01")).is_err());

        let del = |s: &str| script(&BackupAction::Delete { source: s.into() });
        assert_eq!(del("42").unwrap(), "BACKUP CATALOG DELETE BACKUP_ID 42 COMPLETE;");
        assert_eq!(del("42@h01").unwrap(), "BACKUP CATALOG DELETE FOR \"H01\" BACKUP_ID 42 COMPLETE;");
        assert!(del("42; DROP").is_err());
        let s = spec();
        assert!(s.restore && s.delete && s.history && s.server_wide);
    }

    #[test]
    fn catalog_rows() {
        let row = |db: Option<&str>, ty: &str, state: &str| -> Vec<HdbValue<'static>> {
            let s = |v: &str| HdbValue::STRING(v.into());
            vec![
                s("42"),
                s(ty),
                s(state),
                s("2026-09-29 15:45:00"),
                s("2026-09-29 15:46:10"),
                HdbValue::NULL,
                HdbValue::NULL,
                db.map_or(HdbValue::NULL, s),
                s("1048576"),
                s("/hana/backup/data/DB_H01/x_databackup_0_1"),
                s("file"),
            ]
        };
        let e = entry(&row(None, "complete data backup", "successful"), "H01");
        assert_eq!(e.id, "42");
        assert_eq!(e.database.as_deref(), Some("H01"));
        assert_eq!(e.kind.as_deref(), Some("Completo"));
        assert_eq!(e.started.as_deref(), Some("2026-09-29T15:45:00Z"));
        assert_eq!(e.size, Some(1_048_576));
        assert!(e.restorable);
        let e = entry(&row(Some("H02"), "differential data backup", "successful"), "SYSTEMDB");
        assert_eq!(e.id, "42@H02");
        assert!(!e.restorable);
        let e = entry(&row(Some("SYSTEMDB"), "complete data backup", "successful"), "SYSTEMDB");
        assert_eq!(e.id, "42");
        assert!(!e.restorable);
        assert!(history_sql(true).contains("SYS_DATABASES.M_BACKUP_CATALOG C"));
    }
}
