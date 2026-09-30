//! The server's own backups: Data Pump through its PL/SQL API
//! (`DBMS_DATAPUMP`). A schema is exported to a `.dmp` file in a
//! DIRECTORY object of the server (DATA_PUMP_DIR by default) and imported
//! back from it, remapped when the target is another schema; a dump file
//! is deleted with `UTL_FILE.FREMOVE`. RMAN isn't reachable by SQL.
//!
//! The history is what the server can tell: the Data Pump jobs it keeps
//! (running, or stopped with their master table) and, where
//! `DBMS_CLOUD.LIST_FILES` exists (Autonomous Database), the dump files
//! in DATA_PUMP_DIR. A plain directory can't be listed from SQL.

use crate::{db_code, err};
use dbine_driver::{BackupAction, BackupEntry, BackupSpec, Error, Field, FieldKind, Result};
use oracledb::Connection;
use std::collections::BTreeMap;

const DEFAULT_DIR: &str = "DATA_PUMP_DIR";

pub(crate) fn spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![
            Field::new("directory", "Directorio (objeto DIRECTORY)", FieldKind::Text)
                .default_value(DEFAULT_DIR)
                .help("Un objeto DIRECTORY del servidor sobre el que tengas READ y WRITE (SELECT directory_name FROM all_directories)."),
            Field::new("file", "Archivo", FieldKind::Text)
                .placeholder("vacío: <ESQUEMA>_<fecha>.dmp")
                .help("Nombre del archivo .dmp; al lado se deja el .log de Data Pump."),
            Field::new("consistent", "Consistente (a un mismo instante)", FieldKind::Bool)
                .default_value("true")
                .help("FLASHBACK_TIME = SYSTIMESTAMP: todas las tablas como estaban al empezar."),
        ],
        restore: true,
        restore_options: vec![
            Field::new("source_schema", "Esquema dentro del backup", FieldKind::Text)
                .help("Vacío: el mismo que el de destino. Si es otro, se reasigna con REMAP_SCHEMA."),
            Field::new("table_exists", "Si la tabla ya existe", FieldKind::Select(vec![
                ("SKIP", "Dejarla como está"),
                ("APPEND", "Agregar las filas"),
                ("TRUNCATE", "Vaciarla y cargar"),
                ("REPLACE", "Reemplazarla"),
            ]))
            .default_value("SKIP"),
        ],
        delete: true,
        history: true,
        server_wide: false,
        script_database: "",
        note: "Data Pump (DBMS_DATAPUMP) exporta el esquema a un archivo .dmp en un directorio del servidor \
               (DATA_PUMP_DIR si no se indica otro) y lo importa desde ahí. Hace falta READ y WRITE sobre el \
               directorio; para exportar o importar otro esquema, DATAPUMP_EXP_FULL_DATABASE / \
               DATAPUMP_IMP_FULL_DATABASE, y borrar un archivo usa UTL_FILE. Un backup se indica como \
               DIRECTORIO/archivo.dmp. El historial muestra los trabajos de Data Pump que conserva el servidor y, en \
               Autonomous Database, los .dmp de DATA_PUMP_DIR: un directorio común no se puede listar por SQL. RMAN \
               no se usa: no es SQL.",
    }
}

/// `'text'`, quotes doubled.
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn opt<'a>(options: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    options.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

fn schema(db: &Option<String>) -> Result<&str> {
    db.as_deref().filter(|d| !d.trim().is_empty()).ok_or_else(|| Error::Query("Falta el esquema.".into()))
}

/// A DIRECTORY object's name as the dictionary has it: simple names in
/// upper case, anything else as written.
fn directory(name: &str) -> String {
    let name = name.trim().trim_matches('"');
    if name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '#')) {
        name.to_ascii_uppercase()
    } else {
        name.to_string()
    }
}

/// A file Data Pump can take: a bare name, no folders.
fn file_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() || name.contains(['/', '\\']) || name == "." || name == ".." {
        return Err(Error::Query("El archivo tiene que ser un nombre, sin carpetas (la carpeta la da el directorio).".into()));
    }
    Ok(name)
}

/// `DIRECTORY/file.dmp` (or `file.dmp`, in DATA_PUMP_DIR).
fn location(source: &str) -> Result<(String, &str)> {
    let source = source.trim();
    match source.split_once('/') {
        Some((dir, file)) if !dir.trim().is_empty() => Ok((directory(dir), file_name(file)?)),
        _ => Ok((DEFAULT_DIR.to_string(), file_name(source)?)),
    }
}

/// `IN ('NAME')` as the PL/SQL literal METADATA_FILTER takes.
fn schema_filter(name: &str) -> String {
    lit(&format!("IN ({})", lit(name)))
}

/// The job around `body`: opened, started, waited for; a job that fails
/// before starting is stopped so it leaves no master table behind.
fn job(operation: &str, files: &str, body: &str) -> String {
    format!(
        "DECLARE\n\
         \x20 h NUMBER;\n\
         \x20 state VARCHAR2(30);\n\
         {files}\
         BEGIN\n\
         \x20 h := DBMS_DATAPUMP.OPEN('{operation}', 'SCHEMA');\n\
         {body}\
         \x20 DBMS_DATAPUMP.START_JOB(h);\n\
         \x20 DBMS_DATAPUMP.WAIT_FOR_JOB(h, state);\n\
         \x20 IF state <> 'COMPLETED' THEN\n\
         \x20   RAISE_APPLICATION_ERROR(-20001, 'Data Pump terminó en estado ' || state);\n\
         \x20 END IF;\n\
         \x20 DBMS_OUTPUT.PUT_LINE('Data Pump: ' || state || ', ' || dump_file);\n\
         EXCEPTION\n\
         \x20 WHEN OTHERS THEN\n\
         \x20   IF h IS NOT NULL THEN\n\
         \x20     BEGIN DBMS_DATAPUMP.STOP_JOB(h); EXCEPTION WHEN OTHERS THEN NULL; END;\n\
         \x20   END IF;\n\
         \x20   RAISE;\n\
         END;\n\
         /"
    )
}

pub(crate) fn script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { database, options } => {
            let schema = schema(database)?;
            let dir = directory(opt(options, "directory").unwrap_or(DEFAULT_DIR));
            // The default name comes from the server's clock.
            let file = match opt(options, "file") {
                Some(f) => lit(file_name(f)?),
                None => format!(
                    "{} || TO_CHAR(SYSDATE, 'YYYYMMDD_HH24MISS') || '.dmp'",
                    lit(&format!("{}_", schema.replace(|c: char| !c.is_ascii_alphanumeric() && c != '_', "_")))
                ),
            };
            let files = format!(
                "\x20 dump_file VARCHAR2(4000) := {file};\n\
                 \x20 log_file VARCHAR2(4000) := REGEXP_REPLACE(dump_file, '\\.dmp$', '', 1, 1, 'i') || '.log';\n"
            );
            let mut body = format!(
                "\x20 DBMS_DATAPUMP.ADD_FILE(h, dump_file, {d}, NULL, DBMS_DATAPUMP.KU$_FILE_TYPE_DUMP_FILE);\n\
                 \x20 DBMS_DATAPUMP.ADD_FILE(h, log_file, {d}, NULL, DBMS_DATAPUMP.KU$_FILE_TYPE_LOG_FILE, 1);\n\
                 \x20 DBMS_DATAPUMP.METADATA_FILTER(h, 'SCHEMA_EXPR', {f});\n",
                d = lit(&dir),
                f = schema_filter(schema)
            );
            if opt(options, "consistent") != Some("false") {
                body += "  DBMS_DATAPUMP.SET_PARAMETER(h, 'FLASHBACK_TIME', 'SYSTIMESTAMP');\n";
            }
            Ok(job("EXPORT", &files, &body))
        }
        BackupAction::Restore { source, database, options } => {
            let target = schema(database)?;
            let (dir, file) = location(source)?;
            let from = opt(options, "source_schema").unwrap_or(target);
            let exists = opt(options, "table_exists")
                .filter(|a| matches!(*a, "APPEND" | "TRUNCATE" | "REPLACE"))
                .unwrap_or("SKIP");
            let log = format!("{}_import.log", file.strip_suffix(".dmp").or_else(|| file.strip_suffix(".DMP")).unwrap_or(file));
            let files = format!("\x20 dump_file VARCHAR2(4000) := {};\n", lit(file));
            let mut body = format!(
                "\x20 DBMS_DATAPUMP.ADD_FILE(h, dump_file, {d}, NULL, DBMS_DATAPUMP.KU$_FILE_TYPE_DUMP_FILE);\n\
                 \x20 DBMS_DATAPUMP.ADD_FILE(h, {log}, {d}, NULL, DBMS_DATAPUMP.KU$_FILE_TYPE_LOG_FILE, 1);\n\
                 \x20 DBMS_DATAPUMP.METADATA_FILTER(h, 'SCHEMA_EXPR', {f});\n",
                d = lit(&dir),
                log = lit(&log),
                f = schema_filter(from)
            );
            if from != target {
                body += &format!("  DBMS_DATAPUMP.METADATA_REMAP(h, 'REMAP_SCHEMA', {}, {});\n", lit(from), lit(target));
            }
            body += &format!("  DBMS_DATAPUMP.SET_PARAMETER(h, 'TABLE_EXISTS_ACTION', '{exists}');\n");
            Ok(job("IMPORT", &files, &body))
        }
        BackupAction::Delete { source } => {
            let (dir, file) = location(source)?;
            Ok(format!("BEGIN\n  UTL_FILE.FREMOVE({}, {});\nEND;\n/", lit(&dir), lit(file)))
        }
    }
}

/// Data Pump jobs (every user's with DBA views, else the user's own) and
/// the dump files DBMS_CLOUD can list.
pub(crate) fn history(c: &Connection, database: Option<&str>) -> Result<Vec<BackupEntry>> {
    let mut out = Vec::new();
    let jobs = match c.query(
        "SELECT owner_name, job_name, operation, job_mode, state, attached_sessions FROM dba_datapump_jobs \
         WHERE job_name NOT LIKE 'BIN$%' ORDER BY job_name",
        &[],
    ) {
        Ok(cur) => cur,
        // ORA-00942: no access to the DBA views.
        Err(e) if db_code(&e) == Some(942) => c
            .query(
                "SELECT USER, job_name, operation, job_mode, state, attached_sessions FROM user_datapump_jobs \
                 WHERE job_name NOT LIKE 'BIN$%' ORDER BY job_name",
                &[],
            )
            .map_err(err)?,
        Err(e) => return Err(err(e)),
    };
    for row in jobs {
        let row = row.map_err(err)?;
        let text = |i: usize| row.get::<Option<String>>(i).ok().flatten();
        let (owner, name) = (text(0).unwrap_or_default(), text(1).unwrap_or_default());
        let operation = text(2).unwrap_or_default();
        let mut details = vec![("Dueño".to_string(), owner.clone()), ("Modo".to_string(), text(3).unwrap_or_default())];
        if let Some(n) = row.get::<Option<i64>>(5).ok().flatten() {
            details.push(("Sesiones conectadas".into(), n.to_string()));
        }
        out.push(BackupEntry {
            id: format!("{owner}.{name}"),
            database: None,
            kind: Some(match operation.as_str() {
                "EXPORT" => "Exportación Data Pump".into(),
                "IMPORT" => "Importación Data Pump".into(),
                o => format!("Data Pump ({o})"),
            }),
            status: text(4),
            details,
            ..Default::default()
        });
    }
    // Autonomous Database: the dump files themselves.
    match c.query(
        "SELECT object_name, bytes, TO_CHAR(created, 'YYYY-MM-DD\"T\"HH24:MI:SS'), \
         TO_CHAR(last_modified, 'YYYY-MM-DD\"T\"HH24:MI:SS') \
         FROM TABLE(DBMS_CLOUD.LIST_FILES('DATA_PUMP_DIR')) WHERE LOWER(object_name) LIKE '%.dmp'",
        &[],
    ) {
        Ok(files) => {
            for row in files {
                let row = row.map_err(err)?;
                let text = |i: usize| row.get::<Option<String>>(i).ok().flatten();
                let name = text(0).unwrap_or_default();
                // Files named by DBine start with the schema.
                let of = database.filter(|db| name.to_ascii_uppercase().starts_with(&format!("{}_", db.to_ascii_uppercase())));
                out.push(BackupEntry {
                    id: format!("{DEFAULT_DIR}/{name}"),
                    database: of.map(str::to_string),
                    kind: Some("Archivo de Data Pump".into()),
                    started: text(2),
                    finished: text(3),
                    size: row.get::<Option<i64>>(1).ok().flatten().and_then(|n| u64::try_from(n).ok()),
                    location: Some(format!("{DEFAULT_DIR}/{name}")),
                    status: Some("Disponible".into()),
                    details: Vec::new(),
                    restorable: true,
                });
            }
        }
        Err(e) => tracing::debug!("oracle: DBMS_CLOUD.LIST_FILES: {e}"),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn export_a_schema() {
        let s = script(&BackupAction::Backup { database: Some("HR".into()), options: opts(&[("file", "hr.dmp")]) }).unwrap();
        assert!(s.contains("h := DBMS_DATAPUMP.OPEN('EXPORT', 'SCHEMA');"), "{s}");
        assert!(s.contains("dump_file VARCHAR2(4000) := 'hr.dmp';"));
        assert!(s.contains("DBMS_DATAPUMP.ADD_FILE(h, dump_file, 'DATA_PUMP_DIR', NULL, DBMS_DATAPUMP.KU$_FILE_TYPE_DUMP_FILE);"));
        assert!(s.contains("DBMS_DATAPUMP.METADATA_FILTER(h, 'SCHEMA_EXPR', 'IN (''HR'')');"));
        assert!(s.contains("'FLASHBACK_TIME', 'SYSTIMESTAMP'"));
        assert!(s.ends_with("END;\n/"));
        assert!(!crate::script::has_bind_like(&s));
        assert_eq!(crate::script::split(&s).len(), 1);
    }

    #[test]
    fn default_file_name_and_quoting() {
        let s = script(&BackupAction::Backup {
            database: Some("O'Brien".into()),
            options: opts(&[("directory", "my_dir"), ("consistent", "false")]),
        })
        .unwrap();
        assert!(s.contains("'O_Brien_' || TO_CHAR(SYSDATE, 'YYYYMMDD_HH24MISS') || '.dmp'"), "{s}");
        assert!(s.contains("'MY_DIR'"));
        assert!(s.contains("'IN (''O''''Brien'')'"));
        assert!(!s.contains("FLASHBACK_TIME"));
        assert!(script(&BackupAction::Backup { database: Some("X".into()), options: opts(&[("file", "../x.dmp")]) }).is_err());
    }

    #[test]
    fn import_with_remap() {
        let s = script(&BackupAction::Restore {
            source: "BK_DIR/hr_1.dmp".into(),
            database: Some("HR2".into()),
            options: opts(&[("source_schema", "HR"), ("table_exists", "REPLACE")]),
        })
        .unwrap();
        assert!(s.contains("DBMS_DATAPUMP.OPEN('IMPORT', 'SCHEMA')"));
        assert!(s.contains("'BK_DIR'") && s.contains("'hr_1_import.log'"));
        assert!(s.contains("DBMS_DATAPUMP.METADATA_REMAP(h, 'REMAP_SCHEMA', 'HR', 'HR2');"));
        assert!(s.contains("'TABLE_EXISTS_ACTION', 'REPLACE'"));
        let same = script(&BackupAction::Restore { source: "hr.dmp".into(), database: Some("HR".into()), options: opts(&[]) }).unwrap();
        assert!(same.contains("'DATA_PUMP_DIR'") && !same.contains("REMAP_SCHEMA") && same.contains("'SKIP'"));
    }

    #[test]
    fn delete_a_dump_file() {
        let s = script(&BackupAction::Delete { source: "DATA_PUMP_DIR/a'b.dmp".into() }).unwrap();
        assert_eq!(s, "BEGIN\n  UTL_FILE.FREMOVE('DATA_PUMP_DIR', 'a''b.dmp');\nEND;\n/");
        assert!(script(&BackupAction::Delete { source: "DIR/".into() }).is_err());
    }
}
