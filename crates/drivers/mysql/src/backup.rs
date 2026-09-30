//! The server's own backups (the Backups tab), per engine:
//!
//! - MySQL: `CLONE LOCAL DATA DIRECTORY` (clone plugin, 8.0.17+), a
//!   physical copy of the whole server into a folder of the server; the
//!   last clone is in `performance_schema.clone_status`.
//! - TiDB: `BACKUP DATABASE … TO` / `RESTORE DATABASE … FROM` a storage URL
//!   (BR inside TiDB); `SHOW BACKUPS` / `SHOW RESTORES` list the recent jobs.
//! - SingleStore: `BACKUP DATABASE … TO` / `RESTORE DATABASE … FROM` a path
//!   or a bucket; `information_schema.MV_BACKUP_HISTORY`.
//! - OceanBase: the tenant's physical backup (`ALTER SYSTEM BACKUP
//!   DATABASE`) and its jobs.
//! - StarRocks and Apache Doris: `BACKUP SNAPSHOT` / `RESTORE SNAPSHOT`
//!   in a repository; `SHOW SNAPSHOT ON <repository>`.
//! - Manticore Search: `BACKUP TO <folder>` of the server.
//! - GreptimeDB: `COPY DATABASE … TO` / `FROM` a folder or a bucket.
//!
//! MariaDB, Databend and the managed services (Aurora, Cloud SQL, VeloDB)
//! have none reachable by SQL.

use crate::session::{at, lit, named, MySqlSession};
use crate::Variant;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{BackupAction, BackupEntry, BackupSpec, Error, Field, FieldKind, Result};
use mysql_async::Row;
use std::collections::BTreeMap;

/// What the engine offers; `None`: only DBine's copies.
pub(crate) fn spec(product: Variant) -> Option<BackupSpec> {
    Some(match product {
        Variant::MySql => mysql_spec(),
        Variant::TiDb => tidb_spec(),
        Variant::SingleStore => singlestore_spec(),
        Variant::OceanBase => oceanbase_spec(),
        Variant::StarRocks | Variant::Doris => snapshot_spec(),
        Variant::Manticore => manticore_spec(),
        Variant::GreptimeDb => greptime_spec(),
        // MariaDB (mariadb-backup is a separate tool), Databend (bendsave),
        // and the managed services, whose backups are the provider's.
        _ => return None,
    })
}

pub(crate) fn script(product: Variant, action: &BackupAction) -> Result<String> {
    match product {
        Variant::MySql => mysql_script(action),
        Variant::TiDb => tidb_script(action),
        Variant::SingleStore => singlestore_script(action),
        Variant::OceanBase => oceanbase_script(action),
        Variant::StarRocks | Variant::Doris => snapshot_script(action),
        Variant::Manticore => manticore_script(action),
        Variant::GreptimeDb => greptime_script(action),
        _ => Err(Error::Unsupported("este motor no tiene backups propios".into())),
    }
}

pub(crate) async fn history(s: &mut MySqlSession, database: Option<&str>) -> Result<Vec<BackupEntry>> {
    let mut entries = match s.product {
        Variant::MySql => mysql_history(s).await?,
        Variant::TiDb => tidb_history(s).await?,
        Variant::SingleStore => singlestore_history(s, database).await?,
        Variant::OceanBase => oceanbase_history(s).await?,
        Variant::StarRocks | Variant::Doris => snapshot_history(s, database).await?,
        _ => return Err(Error::Unsupported("este motor no lista sus backups".into())),
    };
    // Newest first (ISO 8601 sorts as text).
    entries.sort_by(|a, b| b.started.cmp(&a.started));
    Ok(entries)
}

// ------------------------------------------------------------- helpers

fn ident(name: &str) -> String {
    quote_ident(Quote::Backtick, name)
}

/// A non-empty option, trimmed.
fn opt<'a>(options: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    options.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

fn flag(options: &BTreeMap<String, String>, key: &str) -> bool {
    opt(options, key) == Some("true")
}

fn required<'a>(options: &'a BTreeMap<String, String>, key: &str, what: &str) -> Result<&'a str> {
    opt(options, key).ok_or_else(|| Error::Query(format!("Falta {what}.")))
}

fn database(db: &Option<String>) -> Result<&str> {
    db.as_deref().map(str::trim).filter(|d| !d.is_empty()).ok_or_else(|| Error::Query("Falta la base de datos.".into()))
}

/// `"text"`, for engines whose PROPERTIES take double-quoted strings.
fn dq(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A positive whole number (rates, TSOs, replica counts).
fn number<'a>(v: &'a str, what: &str) -> Result<&'a str> {
    if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) {
        Ok(v)
    } else {
        Err(Error::Query(format!("{what} tiene que ser un número entero.")))
    }
}

fn unsupported(what: &str) -> Error {
    Error::Unsupported(what.into())
}

/// "2026-09-29 12:40:26.050" → "2026-09-29T12:40:26.050".
fn iso(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.is_empty() && !v.starts_with("0000")).map(|v| v.trim().replacen(' ', "T", 1))
}

/// Now in UTC as `YYYYMMDD_HHMMSS`, for default labels.
fn stamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}_{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Rows of a query, or an explanation of what the server lacks.
async fn rows_or(s: &mut MySqlSession, sql: &str, missing: &str) -> Result<Vec<Row>> {
    s.rows(sql).await.map_err(|e| match e {
        Error::Query(m) => Error::Query(format!("{missing} ({m})")),
        other => other,
    })
}

fn details(r: &Row, cols: &[(&str, &str)]) -> Vec<(String, String)> {
    cols.iter()
        .filter_map(|(col, label)| named(r, &[col]).filter(|v| !v.is_empty() && v != "0").map(|v| (label.to_string(), v)))
        .collect()
}

// --------------------------------------------------------------- MySQL

fn mysql_spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![Field::new("directory", "Carpeta en el servidor", FieldKind::Text)
            .required()
            .placeholder("/var/lib/mysql-files/dbine-backup")
            .help("Una carpeta nueva (no tiene que existir) en el servidor, donde MySQL pueda escribir.")],
        restore: false,
        restore_options: vec![],
        delete: false,
        history: true,
        server_wide: true,
        script_database: "",
        note: "CLONE LOCAL (MySQL 8.0.17 o posterior) hace una copia física de todo el servidor en una carpeta del \
               propio servidor. Necesita el plugin clone (INSTALL PLUGIN clone SONAME 'mysql_clone.so') y el \
               privilegio BACKUP_ADMIN. Restaurar no se hace por SQL: se detiene MySQL y se lo arranca con \
               --datadir apuntando a esa carpeta (o se la copia en lugar del directorio de datos). El historial \
               muestra la última clonación.",
    }
}

fn mysql_script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { options, .. } => {
            let dir = required(options, "directory", "la carpeta en el servidor")?;
            Ok(format!("CLONE LOCAL DATA DIRECTORY = {};", lit(dir)))
        }
        BackupAction::Restore { .. } => Err(unsupported(
            "MySQL no restaura una clonación por SQL: se arranca el servidor con --datadir en esa carpeta",
        )),
        BackupAction::Delete { .. } => Err(unsupported("MySQL no borra clonaciones por SQL: se borra la carpeta en el servidor")),
    }
}

async fn mysql_history(s: &mut MySqlSession) -> Result<Vec<BackupEntry>> {
    let rows = rows_or(
        s,
        "SELECT ID, STATE, BEGIN_TIME, END_TIME, SOURCE, DESTINATION, ERROR_NO, ERROR_MESSAGE, \
         BINLOG_FILE, BINLOG_POSITION FROM performance_schema.clone_status",
        "No se puede leer el historial de clonaciones: ¿está instalado el plugin clone?",
    )
    .await?;
    let size: Option<u64> = s
        .optional_rows("SELECT SUM(DATA) FROM performance_schema.clone_progress")
        .await
        .first()
        .and_then(|r| at(r, 0))
        .and_then(|v| v.parse().ok());
    Ok(rows
        .iter()
        .map(|r| {
            let source = named(r, &["SOURCE"]).unwrap_or_default();
            let local = source.eq_ignore_ascii_case("LOCAL INSTANCE");
            let mut d = details(r, &[("ERROR_NO", "Código de error"), ("ERROR_MESSAGE", "Error"), ("BINLOG_FILE", "Binlog")]);
            if !local && !source.is_empty() {
                d.insert(0, ("Origen".into(), source));
            }
            BackupEntry {
                id: named(r, &["DESTINATION"]).unwrap_or_default(),
                database: None,
                kind: Some(if local { "Clonación local" } else { "Clonación remota" }.into()),
                started: iso(named(r, &["BEGIN_TIME"])),
                finished: iso(named(r, &["END_TIME"])),
                size,
                location: named(r, &["DESTINATION"]),
                status: named(r, &["STATE"]),
                details: d,
                restorable: false,
            }
        })
        .collect())
}

// ---------------------------------------------------------------- TiDB

fn tidb_spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![
            Field::new("destination", "Destino", FieldKind::Text)
                .required()
                .placeholder("s3://bucket/carpeta?region=us-east-1")
                .help("URL de almacenamiento: s3://, gcs://, azure:// o local:// (local:// escribe en el disco de cada nodo TiKV)."),
            Field::new("rate_limit", "Límite de velocidad (MB/s por nodo TiKV)", FieldKind::Number)
                .help("Vacío: sin límite."),
            Field::new("last_backup", "TSO del backup anterior", FieldKind::Text)
                .help("Para un backup incremental: el TSO con el que terminó el backup anterior (BackupTS). Vacío: completo."),
            Field::new("checksum", "Verificar con checksum", FieldKind::Bool).default_value("true"),
        ],
        restore: true,
        restore_options: vec![
            Field::new("rate_limit", "Límite de velocidad (MB/s por nodo TiKV)", FieldKind::Number)
                .help("Vacío: sin límite."),
            Field::new("checksum", "Verificar con checksum", FieldKind::Bool).default_value("true"),
        ],
        delete: false,
        history: true,
        server_wide: false,
        script_database: "",
        note: "BACKUP y RESTORE de TiDB copian la base al almacenamiento indicado (S3, GCS, Azure o local:// en cada \
               nodo TiKV) y necesitan un clúster con TiKV y el privilegio BACKUP_ADMIN / RESTORE_ADMIN. La base se \
               restaura con su mismo nombre y sus tablas no tienen que existir. El historial muestra los trabajos \
               recientes de esta instancia de TiDB; borrar un backup es borrar su carpeta en el almacenamiento.",
    }
}

fn tidb_options(options: &BTreeMap<String, String>) -> Result<String> {
    let mut sql = String::new();
    if let Some(r) = opt(options, "rate_limit") {
        sql += &format!(" RATE_LIMIT = {} MB/SECOND", number(r, "El límite de velocidad")?);
    }
    if opt(options, "checksum") == Some("false") {
        sql += " CHECKSUM = FALSE";
    }
    Ok(sql)
}

fn tidb_script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { database: db, options } => {
            let dest = required(options, "destination", "el destino")?;
            let mut sql = format!("BACKUP DATABASE {} TO {}", ident(database(db)?), lit(dest));
            sql += &tidb_options(options)?;
            if let Some(ts) = opt(options, "last_backup") {
                sql += &format!(" LAST_BACKUP = {}", number(ts, "El TSO del backup anterior")?);
            }
            Ok(sql + ";")
        }
        BackupAction::Restore { source, database: db, options } => Ok(format!(
            "RESTORE DATABASE {} FROM {}{};",
            ident(database(db)?),
            lit(source.trim()),
            tidb_options(options)?
        )),
        BackupAction::Delete { .. } => {
            Err(unsupported("TiDB no borra backups por SQL: se borra su carpeta en el almacenamiento"))
        }
    }
}

async fn tidb_history(s: &mut MySqlSession) -> Result<Vec<BackupEntry>> {
    let mut out = Vec::new();
    for (sql, backup) in [("SHOW BACKUPS", true), ("SHOW RESTORES", false)] {
        for r in rows_or(s, sql, "No se pueden listar los trabajos de backup de TiDB").await? {
            // A job that ended keeps its last stage (e.g. "Checksum") with
            // a finish time and 100 % progress.
            let finished = iso(named(&r, &["Finish_Time"]));
            let done = finished.is_some() && named(&r, &["Progress"]).and_then(|p| p.parse::<f64>().ok()) == Some(100.0);
            let state = named(&r, &["State"]);
            let done = done || state.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("Finished"));
            let state = if done { Some("Finished".to_string()) } else { state };
            out.push(BackupEntry {
                id: named(&r, &["Destination"]).unwrap_or_default(),
                database: None,
                kind: Some(if backup { "Backup" } else { "Restauración" }.into()),
                started: iso(named(&r, &["Execution_Time", "Queue_Time"])),
                finished,
                size: None,
                location: named(&r, &["Destination"]),
                restorable: backup && done,
                status: state,
                details: details(&r, &[("Id", "Trabajo"), ("Progress", "Progreso (%)"), ("Connection", "Conexión"), ("Message", "Mensaje")]),
            });
        }
    }
    Ok(out)
}

// --------------------------------------------------------- SingleStore

fn storage_fields(restore: bool) -> Vec<Field> {
    let cloud = ["s3", "gcs", "azure"];
    vec![
        Field::new("storage", "Almacenamiento", FieldKind::Select(vec![
            ("local", "Carpeta en el servidor"),
            ("s3", "Amazon S3 (o compatible)"),
            ("gcs", "Google Cloud Storage"),
            ("azure", "Azure Blob Storage"),
        ]))
        .default_value("local"),
        Field::new("path", if restore { "Carpeta del backup" } else { "Carpeta de destino" }, FieldKind::Text)
            .placeholder("/var/lib/memsql/backups o bucket/carpeta")
            .help("En el servidor, cada nodo escribe su parte en esa carpeta; en la nube, bucket/carpeta."),
        Field::new("config", "Configuración (JSON)", FieldKind::Textarea)
            .placeholder("{\"region\": \"us-east-1\"}")
            .when("storage", &cloud),
        Field::new("credentials", "Credenciales (JSON)", FieldKind::Textarea)
            .secret()
            .placeholder("{\"aws_access_key_id\": \"…\", \"aws_secret_access_key\": \"…\"}")
            .when("storage", &cloud),
    ]
}

fn singlestore_spec() -> BackupSpec {
    let mut backup_options = storage_fields(false);
    backup_options.insert(0, Field::new("type", "Tipo", FieldKind::Select(vec![
        ("full", "Completo"),
        ("init", "Completo, base de incrementales (WITH INIT)"),
        ("differential", "Incremental (WITH DIFFERENTIAL)"),
    ]))
    .default_value("full"));
    let mut restore_options = storage_fields(true);
    restore_options.push(
        Field::new("source_database", "Base dentro del backup", FieldKind::Text)
            .help("Vacío: la que indica la carpeta del backup (…/base.backup) o, si no, la de destino."),
    );
    BackupSpec {
        backup_options,
        restore: true,
        restore_options,
        delete: false,
        history: true,
        server_wide: false,
        script_database: "",
        note: "BACKUP DATABASE de SingleStore escribe la base en una carpeta de cada nodo del clúster o en un bucket \
               (S3, GCS o Azure). Para restaurar sobre una base existente, primero hay que borrarla. Borrar un backup \
               es borrar su carpeta.",
    }
}

/// `TO|FROM [S3|GCS|AZURE] 'path' [CONFIG '…'] [CREDENTIALS '…']`.
fn singlestore_target(options: &BTreeMap<String, String>, path: &str) -> String {
    let storage = opt(options, "storage").unwrap_or("local");
    let mut sql = match storage {
        "s3" => format!("S3 {}", lit(path)),
        "gcs" => format!("GCS {}", lit(path)),
        "azure" => format!("AZURE {}", lit(path)),
        _ => lit(path),
    };
    if storage != "local" {
        sql += &format!(" CONFIG {}", lit(opt(options, "config").unwrap_or("{}")));
        sql += &format!(" CREDENTIALS {}", lit(opt(options, "credentials").unwrap_or("{}")));
    }
    sql
}

/// `…/name.backup` (or `.incr_backup`) → (`…`, `name`).
fn backup_folder(source: &str) -> Option<(&str, &str)> {
    let s = source.trim_end_matches('/');
    let (parent, last) = s.rsplit_once('/')?;
    let db = last.strip_suffix(".backup").or_else(|| last.strip_suffix(".incr_backup"))?;
    (!db.is_empty()).then_some((if parent.is_empty() { "/" } else { parent }, db))
}

fn singlestore_script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { database: db, options } => {
            let path = required(options, "path", "la carpeta de destino")?;
            let with = match opt(options, "type") {
                Some("init") => " WITH INIT",
                Some("differential") => " WITH DIFFERENTIAL",
                _ => "",
            };
            Ok(format!("BACKUP DATABASE {}{with} TO {};", ident(database(db)?), singlestore_target(options, path)))
        }
        BackupAction::Restore { source, database: db, options } => {
            let target = database(db)?;
            let typed = source.trim();
            let typed = if typed.is_empty() { required(options, "path", "la carpeta del backup")? } else { typed };
            let (path, from_folder) = match backup_folder(typed) {
                Some((parent, name)) => (parent, Some(name)),
                None => (typed, None),
            };
            let source_db = opt(options, "source_database").or(from_folder).unwrap_or(target);
            let rename = if source_db == target { String::new() } else { format!(" AS {}", ident(target)) };
            Ok(format!("RESTORE DATABASE {}{rename} FROM {};", ident(source_db), singlestore_target(options, path)))
        }
        BackupAction::Delete { .. } => Err(unsupported("SingleStore no borra backups por SQL: se borra su carpeta")),
    }
}

async fn singlestore_history(s: &mut MySqlSession, database: Option<&str>) -> Result<Vec<BackupEntry>> {
    let mut sql = "SELECT * FROM information_schema.MV_BACKUP_HISTORY".to_string();
    if let Some(db) = database {
        sql += &format!(" WHERE DATABASE_NAME = {}", lit(db));
    }
    let rows = rows_or(s, &sql, "No se puede leer information_schema.MV_BACKUP_HISTORY").await?;
    Ok(rows
        .iter()
        .map(|r| {
            let db = named(r, &["DATABASE_NAME"]);
            let path = named(r, &["BACKUP_PATH"]).unwrap_or_default();
            let status = named(r, &["STATUS"]);
            // The folder RESTORE reads: …/<database>.backup.
            let id = match (&db, backup_folder(&path)) {
                (_, Some(_)) => path.clone(),
                (Some(db), None) => format!("{}/{db}.backup", path.trim_end_matches('/')),
                (None, None) => path.clone(),
            };
            BackupEntry {
                id,
                database: db,
                kind: named(r, &["INCR_BACKUP_TYPE", "BACKUP_TYPE", "TYPE"]).or_else(|| Some("Completo".into())),
                started: iso(named(r, &["START_TIMESTAMP"])),
                finished: iso(named(r, &["END_TIMESTAMP"])),
                size: named(r, &["SIZE", "BACKUP_SIZE"]).and_then(|v| v.parse().ok()),
                location: Some(path),
                restorable: status.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("Success")),
                status,
                details: details(r, &[
                    ("BACKUP_ID", "Id"),
                    ("NUM_PARTITIONS", "Particiones"),
                    ("INCR_BACKUP_ID", "Id incremental"),
                    ("ERROR_CODE", "Código de error"),
                    ("ERROR_MESSAGE", "Error"),
                ]),
            }
        })
        .collect())
}

// ----------------------------------------------------------- OceanBase

fn oceanbase_spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![
            Field::new("incremental", "Incremental", FieldKind::Bool).default_value("false"),
            Field::new("plus_archivelog", "Incluir los logs archivados", FieldKind::Bool)
                .default_value("false")
                .help("PLUS ARCHIVELOG: el backup se puede restaurar sin el archivo de logs."),
            Field::new("destination", "Destino", FieldKind::Text)
                .placeholder("file:///data/backup o s3://bucket/carpeta?host=…")
                .help("Vacío: el DATA_BACKUP_DEST que ya tiene el tenant."),
        ],
        restore: false,
        restore_options: vec![],
        delete: false,
        history: true,
        server_wide: true,
        script_database: "",
        note: "El backup físico de OceanBase es de todo el tenant y necesita el archivado de logs activo (ALTER SYSTEM \
               ARCHIVELOG). Restaurar crea un tenant nuevo desde el tenant sys (ALTER SYSTEM RESTORE … WITH \
               'pool_list=…'), así que no se hace desde esta conexión; los backups viejos se borran con la política de \
               limpieza del tenant.",
    }
}

fn oceanbase_script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { options, .. } => {
            let mut sql = String::new();
            if let Some(dest) = opt(options, "destination") {
                sql += &format!("ALTER SYSTEM SET DATA_BACKUP_DEST = {};\n", lit(dest));
            }
            sql += "ALTER SYSTEM BACKUP ";
            if flag(options, "incremental") {
                sql += "INCREMENTAL ";
            }
            sql += "DATABASE";
            if flag(options, "plus_archivelog") {
                sql += " PLUS ARCHIVELOG";
            }
            Ok(sql + ";")
        }
        BackupAction::Restore { .. } => Err(unsupported(
            "OceanBase restaura en un tenant nuevo desde el tenant sys (ALTER SYSTEM RESTORE), no desde esta conexión",
        )),
        BackupAction::Delete { .. } => Err(unsupported("OceanBase borra los backups con la política de limpieza del tenant")),
    }
}

async fn oceanbase_history(s: &mut MySqlSession) -> Result<Vec<BackupEntry>> {
    let mut rows = s.optional_rows("SELECT * FROM oceanbase.DBA_OB_BACKUP_JOBS").await;
    rows.extend(rows_or(s, "SELECT * FROM oceanbase.DBA_OB_BACKUP_JOB_HISTORY", "No se puede leer el historial de backups del tenant").await?);
    Ok(rows
        .iter()
        .map(|r| {
            let incremental = named(r, &["BACKUP_TYPE"]).is_some_and(|t| t.to_ascii_uppercase().starts_with("INC"));
            BackupEntry {
                id: named(r, &["BACKUP_SET_ID", "JOB_ID"]).unwrap_or_default(),
                database: None,
                kind: Some(if incremental { "Incremental" } else { "Completo" }.into()),
                started: iso(named(r, &["START_TIMESTAMP"])),
                finished: iso(named(r, &["END_TIMESTAMP"])),
                size: named(r, &["OUTPUT_BYTES", "OUTPUT_BYTES_DISPLAY"]).and_then(|v| v.parse().ok()),
                location: named(r, &["PATH", "BACKUP_DEST"]),
                status: named(r, &["STATUS"]),
                details: details(r, &[
                    ("JOB_ID", "Trabajo"),
                    ("PLUS_ARCHIVELOG", "Con logs archivados"),
                    ("RESULT", "Resultado"),
                    ("COMMENT", "Comentario"),
                    ("DESCRIPTION", "Descripción"),
                ]),
                restorable: false,
            }
        })
        .collect())
}

// ---------------------------------------------------- StarRocks / Doris

fn snapshot_spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![
            Field::new("repository", "Repositorio", FieldKind::Text)
                .required()
                .help("Un repositorio creado con CREATE REPOSITORY (SHOW REPOSITORIES los lista)."),
            Field::new("label", "Nombre del snapshot", FieldKind::Text).placeholder("vacío: <base>_<fecha>"),
            Field::new("tables", "Tablas", FieldKind::Text).placeholder("vacío: todas; si no, separadas por comas"),
        ],
        restore: true,
        restore_options: vec![
            Field::new("tables", "Tablas", FieldKind::Text).placeholder("vacío: todas; si no, separadas por comas"),
            Field::new("replication_num", "Réplicas", FieldKind::Number)
                .help("Vacío: las del backup. En un clúster con menos nodos, 1."),
        ],
        delete: false,
        history: true,
        server_wide: false,
        script_database: "",
        note: "BACKUP SNAPSHOT copia la base (o algunas tablas) a un repositorio en almacenamiento remoto (S3, HDFS…) \
               que hay que crear antes con CREATE REPOSITORY. Una restauración no puede pisar tablas que ya existen \
               con otro esquema. Borrar un snapshot es borrar su carpeta en el repositorio.",
    }
}

fn table_list(options: &BTreeMap<String, String>) -> String {
    let tables: Vec<String> = opt(options, "tables")
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(ident)
        .collect();
    if tables.is_empty() {
        String::new()
    } else {
        format!(" ON ({})", tables.join(", "))
    }
}

/// `repository/label/timestamp`, the id of a snapshot.
fn snapshot_id(source: &str) -> Result<(&str, &str, &str)> {
    let mut parts = source.trim().splitn(3, '/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(r), Some(l), Some(t)) if !r.is_empty() && !l.is_empty() && !t.is_empty() => Ok((r, l, t)),
        _ => Err(Error::Query("Indicá el snapshot como repositorio/snapshot/fecha (la fecha como la muestra SHOW SNAPSHOT).".into())),
    }
}

fn snapshot_script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { database: db, options } => {
            let db = database(db)?;
            let repo = required(options, "repository", "el repositorio")?;
            let label = opt(options, "label").map_or_else(|| format!("{db}_{}", stamp()), str::to_string);
            Ok(format!("BACKUP SNAPSHOT {}.{} TO {}{};", ident(db), ident(&label), ident(repo), table_list(options)))
        }
        BackupAction::Restore { source, database: db, options } => {
            let (repo, label, ts) = snapshot_id(source)?;
            let mut props = vec![format!("{} = {}", dq("backup_timestamp"), dq(ts))];
            if let Some(n) = opt(options, "replication_num") {
                props.push(format!("{} = {}", dq("replication_num"), dq(number(n, "La cantidad de réplicas")?)));
            }
            Ok(format!(
                "RESTORE SNAPSHOT {}.{} FROM {}{} PROPERTIES ({});",
                ident(database(db)?),
                ident(label),
                ident(repo),
                table_list(options),
                props.join(", ")
            ))
        }
        BackupAction::Delete { .. } => Err(unsupported("este motor no borra snapshots por SQL: se borra su carpeta en el repositorio")),
    }
}

async fn snapshot_history(s: &mut MySqlSession, database: Option<&str>) -> Result<Vec<BackupEntry>> {
    let repos: Vec<(String, Option<String>)> = rows_or(s, "SHOW REPOSITORIES", "No se pueden listar los repositorios")
        .await?
        .iter()
        .filter_map(|r| Some((named(r, &["RepoName", "Name"])?, named(r, &["Location"]))))
        .collect();
    let mut out = Vec::new();
    for (repo, location) in repos {
        for r in s.optional_rows(&format!("SHOW SNAPSHOT ON {}", ident(&repo))).await {
            let (Some(label), Some(ts)) = (named(&r, &["Snapshot"]), named(&r, &["Timestamp"])) else { continue };
            let db = named(&r, &["Database", "DbName"]);
            if let (Some(want), Some(have)) = (database, db.as_deref()) {
                if !want.eq_ignore_ascii_case(have) {
                    continue;
                }
            }
            let status = named(&r, &["Status"]);
            out.push(BackupEntry {
                id: format!("{repo}/{label}/{ts}"),
                database: db,
                kind: Some("Snapshot".into()),
                started: snapshot_time(&ts),
                finished: None,
                size: None,
                location: location.clone().map(|l| format!("{l} ({repo})")).or_else(|| Some(repo.clone())),
                restorable: status.as_deref().is_none_or(|s| s.eq_ignore_ascii_case("OK")),
                status,
                details: vec![("Snapshot".into(), label), ("Repositorio".into(), repo.clone())],
            });
        }
    }
    // The job still running (finished ones are in the repository).
    if let Some(db) = database {
        for r in s.optional_rows(&format!("SHOW BACKUP FROM {}", ident(db))).await {
            let state = named(&r, &["State"]).unwrap_or_default();
            if state.eq_ignore_ascii_case("FINISHED") {
                continue;
            }
            out.push(BackupEntry {
                id: named(&r, &["SnapshotName"]).unwrap_or_default(),
                database: Some(db.to_string()),
                kind: Some("Snapshot (en curso)".into()),
                started: iso(named(&r, &["CreateTime"])),
                finished: iso(named(&r, &["FinishedTime"])),
                size: None,
                location: named(&r, &["RepoName"]),
                status: Some(state),
                details: details(&r, &[("JobId", "Trabajo"), ("Progress", "Progreso"), ("TaskErrMsg", "Error"), ("Status", "Detalle")]),
                restorable: false,
            });
        }
    }
    Ok(out)
}

/// "2026-09-29-12-40-50" (StarRocks adds "-608", the milliseconds) →
/// "2026-09-29T12:40:50[.608]".
fn snapshot_time(ts: &str) -> Option<String> {
    let p: Vec<&str> = ts.split('-').collect();
    let base = || format!("{}-{}-{}T{}:{}:{}", p[0], p[1], p[2], p[3], p[4], p[5]);
    match p.len() {
        6 => Some(base()),
        7 => Some(format!("{}.{}", base(), p[6])),
        _ => None,
    }
}

// ------------------------------------------------------------ Manticore

fn manticore_spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![
            Field::new("directory", "Carpeta en el servidor", FieldKind::Text)
                .required()
                .placeholder("/var/lib/manticore/backups")
                .help("Tiene que existir y el servidor tiene que poder escribir en ella; adentro se crea backup-<fecha>."),
            Field::new("tables", "Tablas", FieldKind::Text).placeholder("vacío: todas; si no, separadas por comas"),
            Field::new("compression", "Comprimir", FieldKind::Bool).default_value("false"),
        ],
        restore: false,
        restore_options: vec![],
        delete: false,
        history: false,
        server_wide: true,
        script_database: "",
        note: "BACKUP TO copia las tablas y la configuración a una carpeta del servidor (Manticore 6 o posterior, con \
               Manticore Buddy). Restaurar no se hace por SQL: con el servidor detenido, manticore-backup --restore \
               sobre esa carpeta.",
    }
}

/// A folder or table name Manticore takes unquoted.
fn bare<'a>(v: &'a str, what: &str) -> Result<&'a str> {
    if !v.is_empty() && !v.chars().any(|c| c.is_whitespace() || matches!(c, ';' | '\'' | '"' | '`' | ',' | '\\')) {
        Ok(v)
    } else {
        Err(Error::Query(format!("{what} no puede tener espacios, comillas, comas ni «;».")))
    }
}

fn manticore_script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { options, .. } => {
            let dir = bare(required(options, "directory", "la carpeta en el servidor")?, "La carpeta")?;
            let tables = opt(options, "tables")
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(|t| bare(t, "El nombre de la tabla"))
                .collect::<Result<Vec<_>>>()?;
            let mut sql = "BACKUP ".to_string();
            if !tables.is_empty() {
                sql += &format!("TABLE {} ", tables.join(", "));
            }
            sql += &format!("TO {dir}");
            if flag(options, "compression") {
                sql += " OPTION compression = yes";
            }
            Ok(sql + ";")
        }
        BackupAction::Restore { .. } => Err(unsupported(
            "Manticore restaura con manticore-backup --restore, con el servidor detenido; no por SQL",
        )),
        BackupAction::Delete { .. } => Err(unsupported("Manticore no borra backups por SQL: se borra su carpeta")),
    }
}

// ------------------------------------------------------------ GreptimeDB

fn greptime_fields(restore: bool) -> Vec<Field> {
    vec![
        Field::new("path", if restore { "Carpeta de la copia" } else { "Carpeta de destino" }, FieldKind::Text)
            .required()
            .placeholder("backups/mibase/ o s3://bucket/carpeta/")
            .help("Relativa a la carpeta de copias del servidor, o una URL s3://."),
        Field::new("format", "Formato", FieldKind::Select(vec![("parquet", "Parquet"), ("csv", "CSV"), ("json", "JSON")]))
            .default_value("parquet"),
        Field::new("storage", "Almacenamiento", FieldKind::Select(vec![("local", "El servidor"), ("s3", "S3 (o compatible)")]))
            .default_value("local"),
        Field::new("access_key_id", "Access key ID", FieldKind::Text).when("storage", &["s3"]),
        Field::new("secret_access_key", "Secret access key", FieldKind::Password).secret().when("storage", &["s3"]),
        Field::new("region", "Región", FieldKind::Text).when("storage", &["s3"]),
        Field::new("endpoint", "Endpoint", FieldKind::Text).placeholder("vacío: AWS").when("storage", &["s3"]),
    ]
}

fn greptime_spec() -> BackupSpec {
    BackupSpec {
        backup_options: greptime_fields(false),
        restore: true,
        restore_options: greptime_fields(true),
        delete: false,
        history: false,
        server_wide: false,
        script_database: "",
        note: "COPY DATABASE escribe los datos de cada tabla (Parquet, CSV o JSON) en una carpeta del servidor o en \
               S3, y COPY DATABASE … FROM los vuelve a cargar. La restauración carga en tablas que ya existen: si la \
               base es nueva, primero hay que crear sus tablas (por ejemplo, con la estructura de una copia de DBine).",
    }
}

fn greptime_copy(db: &str, dir: &str, path: &str, options: &BTreeMap<String, String>) -> String {
    let path = if path.ends_with('/') { path.to_string() } else { format!("{path}/") };
    let format = match opt(options, "format") {
        Some("csv") => "csv",
        Some("json") => "json",
        _ => "parquet",
    };
    let mut sql = format!("COPY DATABASE {} {dir} {} WITH (FORMAT = {})", ident(db), lit(&path), lit(format));
    if opt(options, "storage") == Some("s3") {
        let conn: Vec<String> = [
            ("ACCESS_KEY_ID", "access_key_id"),
            ("SECRET_ACCESS_KEY", "secret_access_key"),
            ("REGION", "region"),
            ("ENDPOINT", "endpoint"),
        ]
        .iter()
        .filter_map(|(k, key)| opt(options, key).map(|v| format!("{k} = {}", lit(v))))
        .collect();
        if !conn.is_empty() {
            sql += &format!(" CONNECTION ({})", conn.join(", "));
        }
    }
    sql + ";"
}

fn greptime_script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { database: db, options } => {
            Ok(greptime_copy(database(db)?, "TO", required(options, "path", "la carpeta de destino")?, options))
        }
        BackupAction::Restore { source, database: db, options } => {
            let path = match source.trim() {
                "" => required(options, "path", "la carpeta de la copia")?,
                s => s,
            };
            Ok(greptime_copy(database(db)?, "FROM", path, options))
        }
        BackupAction::Delete { .. } => Err(unsupported("GreptimeDB no borra copias por SQL: se borra su carpeta")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }
    fn backup(db: &str, pairs: &[(&str, &str)]) -> BackupAction {
        BackupAction::Backup { database: Some(db.into()), options: opts(pairs) }
    }
    fn restore(source: &str, db: &str, pairs: &[(&str, &str)]) -> BackupAction {
        BackupAction::Restore { source: source.into(), database: Some(db.into()), options: opts(pairs) }
    }

    #[test]
    fn variants_without_native_backups() {
        for v in [Variant::MariaDb, Variant::Databend, Variant::AuroraMySql, Variant::CloudSqlMySql, Variant::VeloDb] {
            assert!(spec(v).is_none(), "{v:?}");
            assert!(script(v, &backup("x", &[])).is_err());
        }
        for v in [Variant::MySql, Variant::TiDb, Variant::SingleStore, Variant::OceanBase, Variant::StarRocks, Variant::Doris, Variant::Manticore, Variant::GreptimeDb] {
            assert!(spec(v).is_some(), "{v:?}");
        }
    }

    #[test]
    fn mysql_clones_locally() {
        let s = script(Variant::MySql, &backup("", &[("directory", "/data/o'k")])).unwrap();
        assert_eq!(s, "CLONE LOCAL DATA DIRECTORY = '/data/o''k';");
        assert!(script(Variant::MySql, &backup("", &[])).is_err());
        assert!(script(Variant::MySql, &restore("/x", "d", &[])).is_err());
        assert!(spec(Variant::MySql).unwrap().server_wide);
    }

    #[test]
    fn tidb_backup_and_restore() {
        let s = script(
            Variant::TiDb,
            &backup("my`db", &[("destination", "s3://b/p"), ("rate_limit", "120"), ("checksum", "false"), ("last_backup", "4156")]),
        )
        .unwrap();
        assert_eq!(s, "BACKUP DATABASE `my``db` TO 's3://b/p' RATE_LIMIT = 120 MB/SECOND CHECKSUM = FALSE LAST_BACKUP = 4156;");
        assert!(script(Variant::TiDb, &backup("d", &[("destination", "x"), ("rate_limit", "1; DROP")])).is_err());
        let r = script(Variant::TiDb, &restore("local:///tmp/b", "d", &[("checksum", "true")])).unwrap();
        assert_eq!(r, "RESTORE DATABASE `d` FROM 'local:///tmp/b';");
    }

    #[test]
    fn singlestore_backup_and_restore() {
        let s = script(Variant::SingleStore, &backup("d", &[("type", "init"), ("path", "/bk")])).unwrap();
        assert_eq!(s, "BACKUP DATABASE `d` WITH INIT TO '/bk';");
        let s = script(
            Variant::SingleStore,
            &backup("d", &[("storage", "s3"), ("path", "bucket/p"), ("config", "{\"region\":\"us-east-1\"}"), ("credentials", "{}")]),
        )
        .unwrap();
        assert_eq!(s, "BACKUP DATABASE `d` TO S3 'bucket/p' CONFIG '{\"region\":\"us-east-1\"}' CREDENTIALS '{}';");
        // From a history entry: the folder names the database.
        let r = script(Variant::SingleStore, &restore("/bk/orig.backup", "copy", &[])).unwrap();
        assert_eq!(r, "RESTORE DATABASE `orig` AS `copy` FROM '/bk';");
        let r = script(Variant::SingleStore, &restore("/bk", "d", &[])).unwrap();
        assert_eq!(r, "RESTORE DATABASE `d` FROM '/bk';");
        let r = script(Variant::SingleStore, &restore("/bk", "d", &[("source_database", "o")])).unwrap();
        assert_eq!(r, "RESTORE DATABASE `o` AS `d` FROM '/bk';");
    }

    #[test]
    fn oceanbase_tenant_backup() {
        let s = script(Variant::OceanBase, &backup("", &[("incremental", "true"), ("plus_archivelog", "true"), ("destination", "file:///b")])).unwrap();
        assert_eq!(s, "ALTER SYSTEM SET DATA_BACKUP_DEST = 'file:///b';\nALTER SYSTEM BACKUP INCREMENTAL DATABASE PLUS ARCHIVELOG;");
        assert_eq!(script(Variant::OceanBase, &backup("", &[])).unwrap(), "ALTER SYSTEM BACKUP DATABASE;");
    }

    #[test]
    fn snapshots_in_a_repository() {
        let s = script(Variant::StarRocks, &backup("d", &[("repository", "repo"), ("label", "l1"), ("tables", "a, b`c")])).unwrap();
        assert_eq!(s, "BACKUP SNAPSHOT `d`.`l1` TO `repo` ON (`a`, `b``c`);");
        let s = script(Variant::Doris, &backup("d", &[("repository", "repo")])).unwrap();
        assert!(s.starts_with("BACKUP SNAPSHOT `d`.`d_2") && s.ends_with("` TO `repo`;"), "{s}");
        let r = script(Variant::StarRocks, &restore("repo/l1/2026-09-29-12-40-50", "d2", &[("replication_num", "1")])).unwrap();
        assert_eq!(
            r,
            "RESTORE SNAPSHOT `d2`.`l1` FROM `repo` PROPERTIES (\"backup_timestamp\" = \"2026-09-29-12-40-50\", \"replication_num\" = \"1\");"
        );
        assert!(script(Variant::StarRocks, &restore("nope", "d", &[])).is_err());
        assert_eq!(snapshot_time("2026-09-29-12-40-50").as_deref(), Some("2026-09-29T12:40:50"));
        assert_eq!(snapshot_time("2026-09-29-13-14-31-608").as_deref(), Some("2026-09-29T13:14:31.608"));
        assert_eq!(snapshot_time(""), None);
    }

    #[test]
    fn manticore_backup() {
        let s = script(Variant::Manticore, &backup("", &[("directory", "/b"), ("tables", "a,b"), ("compression", "true")])).unwrap();
        assert_eq!(s, "BACKUP TABLE a, b TO /b OPTION compression = yes;");
        assert!(script(Variant::Manticore, &backup("", &[("directory", "/b; DROP")])).is_err());
    }

    #[test]
    fn greptime_copy_database() {
        let s = script(Variant::GreptimeDb, &backup("d", &[("path", "bk/d")])).unwrap();
        assert_eq!(s, "COPY DATABASE `d` TO 'bk/d/' WITH (FORMAT = 'parquet');");
        let r = script(
            Variant::GreptimeDb,
            &restore("s3://b/p/", "d", &[("format", "csv"), ("storage", "s3"), ("access_key_id", "k"), ("secret_access_key", "s'x")]),
        )
        .unwrap();
        assert_eq!(r, "COPY DATABASE `d` FROM 's3://b/p/' WITH (FORMAT = 'csv') CONNECTION (ACCESS_KEY_ID = 'k', SECRET_ACCESS_KEY = 's''x');");
    }

    #[test]
    fn stamps_look_like_dates() {
        let s = stamp();
        assert_eq!(s.len(), 15);
        assert!(s.starts_with("20") && s.as_bytes()[8] == b'_');
    }

    #[test]
    fn backup_folders() {
        assert_eq!(backup_folder("/a/b/db.backup/"), Some(("/a/b", "db")));
        assert_eq!(backup_folder("bucket/db.incr_backup"), Some(("bucket", "db")));
        assert_eq!(backup_folder("/a/b"), None);
    }
}
