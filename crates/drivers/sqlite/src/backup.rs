//! Native backups (docs/backups.md): `VACUUM INTO` writes a consistent,
//! compacted copy of the database to a new file, even from a read-only
//! connection. There's no catalog of copies, and no SQL restores one: a
//! copy is a database file that opens as it is.

use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{BackupAction, BackupSpec, Error, Field, FieldKind, Result};

pub fn spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![Field::new("path", "Archivo de la copia", FieldKind::Text)
            .required()
            .placeholder("/ruta/copias/base-2026-09-29.sqlite")
            .help("Ruta completa en esta computadora. El archivo no tiene que existir: SQLite no pisa uno que ya tenga datos.")],
        restore: false,
        restore_options: vec![],
        delete: false,
        history: false,
        server_wide: false,
        script_database: "",
        note: "VACUUM INTO escribe una copia consistente y compactada de la base en un archivo nuevo. \
               Para volver a ella, abrí la copia con una conexión SQLite o reemplazá el archivo original \
               con la base cerrada: SQLite no restaura por SQL.",
    }
}

pub fn script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { database, options } => {
            let path = options.get("path").map(|p| p.trim()).filter(|p| !p.is_empty());
            let path = path.ok_or_else(|| Error::Query("falta el archivo de la copia".into()))?;
            let schema = match database.as_deref().filter(|d| !d.is_empty()) {
                Some(d) => format!("{} ", quote_ident(Quote::Double, d)),
                None => String::new(),
            };
            Ok(format!("VACUUM {schema}INTO '{}';", path.replace('\'', "''")))
        }
        _ => Err(Error::Unsupported(
            "SQLite no restaura ni borra copias por SQL: la copia es un archivo de base que se abre como cualquier otro".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn backup(db: Option<&str>, path: &str) -> Result<String> {
        script(&BackupAction::Backup {
            database: db.map(Into::into),
            options: [("path".to_string(), path.to_string())].into_iter().collect(),
        })
    }

    #[test]
    fn writes_the_script() {
        assert_eq!(backup(Some("main"), "/tmp/o'k.db").unwrap(), "VACUUM \"main\" INTO '/tmp/o''k.db';");
        assert_eq!(backup(None, " c.db ").unwrap(), "VACUUM INTO 'c.db';");
        assert!(backup(Some("main"), "  ").is_err());
        assert!(script(&BackupAction::Delete { source: "x".into() }).is_err());
    }

    /// The copy is a complete database; a second one onto it fails.
    #[test]
    fn the_copy_opens_as_a_database() {
        let dir = std::env::temp_dir().join(format!("dbine-sqlite-bk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("co'py.sqlite");
        let _ = std::fs::remove_file(&copy);
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT); INSERT INTO t (s) VALUES ('a'), ('b');").unwrap();
        let sql = backup(Some("main"), copy.to_str().unwrap()).unwrap();
        c.execute_batch(&sql).unwrap();
        let n: i64 = Connection::open(&copy).unwrap().query_row("SELECT count(*) FROM t", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
        assert!(c.execute_batch(&sql).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
