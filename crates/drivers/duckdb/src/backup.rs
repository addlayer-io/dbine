//! Native backups (docs/backups.md): `EXPORT DATABASE` writes the schema
//! (`schema.sql`), the load script (`load.sql`) and one Parquet or CSV
//! file per table to a folder; `IMPORT DATABASE` loads that folder into
//! the current database, which must not have those objects yet. DuckDB
//! keeps no catalog of exports.

use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{BackupAction, BackupSpec, Error, Field, FieldKind, Result};

pub fn spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![
            Field::new("dir", "Carpeta de destino", FieldKind::Text)
                .required()
                .placeholder("/ruta/copias/ventas-2026-09-29")
                .help("Ruta completa en esta computadora. DuckDB crea la última carpeta (la de arriba tiene que existir); si ya existe, tiene que estar vacía."),
            Field::new("format", "Formato de los datos", FieldKind::Select(vec![("parquet", "Parquet"), ("csv", "CSV")]))
                .default_value("parquet")
                .help("Parquet ocupa menos y conserva los tipos; CSV se abre con cualquier programa."),
        ],
        restore: true,
        restore_options: vec![],
        delete: false,
        history: false,
        server_wide: false,
        script_database: "",
        note: "EXPORT DATABASE escribe en una carpeta de esta computadora el esquema, un script de carga y un archivo \
               por tabla. IMPORT DATABASE la carga en la base elegida, que tiene que estar vacía (o al menos sin \
               esas tablas). Para restaurar, indicá la carpeta del backup.",
    }
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub fn script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { database, options } => {
            let dir = options.get("dir").map(|d| d.trim()).filter(|d| !d.is_empty());
            let dir = dir.ok_or_else(|| Error::Query("falta la carpeta de destino".into()))?;
            let format = match options.get("format").map(|f| f.trim()) {
                Some("csv") => "CSV",
                _ => "PARQUET",
            };
            let db = match database.as_deref().filter(|d| !d.is_empty()) {
                Some(d) => format!("{} TO ", quote_ident(Quote::Double, d)),
                None => String::new(),
            };
            Ok(format!("EXPORT DATABASE {db}{} (FORMAT {format});", lit(dir)))
        }
        BackupAction::Restore { source, database, .. } => {
            let dir = source.trim();
            if dir.is_empty() {
                return Err(Error::Query("falta la carpeta del backup".into()));
            }
            let target = match database.as_deref().filter(|d| !d.is_empty()) {
                Some(d) => format!("USE {};\n", quote_ident(Quote::Double, d)),
                None => String::new(),
            };
            Ok(format!("{target}IMPORT DATABASE {};", lit(dir)))
        }
        BackupAction::Delete { .. } => {
            Err(Error::Unsupported("DuckDB no borra exportaciones: son carpetas de esta computadora".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::memory;
    use std::collections::BTreeMap;

    fn opts(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn writes_the_scripts() {
        let b = |db: Option<&str>, kv: &[(&str, &str)]| script(&BackupAction::Backup { database: db.map(Into::into), options: opts(kv) });
        assert_eq!(b(Some("my\"db"), &[("dir", "/b/o'k")]).unwrap(), "EXPORT DATABASE \"my\"\"db\" TO '/b/o''k' (FORMAT PARQUET);");
        assert_eq!(b(None, &[("dir", "x"), ("format", "csv")]).unwrap(), "EXPORT DATABASE 'x' (FORMAT CSV);");
        assert!(b(Some("d"), &[]).is_err());
        let r = script(&BackupAction::Restore { source: " /b/x ".into(), database: Some("nueva".into()), options: opts(&[]) });
        assert_eq!(r.unwrap(), "USE \"nueva\";\nIMPORT DATABASE '/b/x';");
        assert!(script(&BackupAction::Delete { source: "x".into() }).is_err());
    }

    /// Export an attached database and import it into another, empty one.
    #[test]
    fn export_then_import() {
        let dir = std::env::temp_dir().join(format!("dbine-duckdb-bk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let c = memory();
        c.execute_batch(
            "ATTACH ':memory:' AS src; ATTACH ':memory:' AS dst;
             CREATE TABLE src.main.t (id INTEGER PRIMARY KEY, s VARCHAR); INSERT INTO src.main.t VALUES (1, 'a'), (2, 'b');
             CREATE VIEW src.main.v AS SELECT id FROM src.main.t;",
        )
        .unwrap();
        for format in ["parquet", "csv"] {
            let out = dir.join(format);
            let export = script(&BackupAction::Backup {
                database: Some("src".into()),
                options: opts(&[("dir", out.to_str().unwrap()), ("format", format)]),
            })
            .unwrap();
            c.execute_batch(&export).unwrap();
            let target = format!("dst_{format}");
            c.execute_batch(&format!("ATTACH ':memory:' AS {target}")).unwrap();
            let import = script(&BackupAction::Restore {
                source: out.to_str().unwrap().into(),
                database: Some(target.clone()),
                options: opts(&[]),
            })
            .unwrap();
            c.execute_batch(&import).unwrap();
            let n: i64 = c.query_row(&format!("SELECT count(*) FROM {target}.main.v"), [], |r| r.get(0)).unwrap();
            assert_eq!(n, 2, "{format}");
            c.execute_batch("USE memory").unwrap();
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
