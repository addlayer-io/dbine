//! The server's own backups: `BACKUP DATABASE` / `BACKUP LOG` to a file on
//! the server (or to a URL: Azure Blob Storage, S3 on 2022+), the history
//! msdb keeps (`backupset` + `backupmediafamily`) and `RESTORE`, which
//! relocates the files when the database is new and takes the database
//! to single-user while it's replaced.
//!
//! Only SQL Server itself (and Managed Instance, which connects as SQL
//! Server): Azure SQL Database's backups are the service's own
//! (point-in-time restore from the portal or the API), and neither Fabric
//! nor Babelfish run BACKUP / RESTORE.
//!
//! The scripts are one T-SQL batch each: what the form leaves to the server
//! (the default backup folder, the data and log folders, the files inside
//! the backup) is looked up when it runs.

use crate::SqlServerSession;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{BackupAction, BackupEntry, BackupSpec, Error, Field, FieldKind, Result};
use std::collections::BTreeMap;
use tiberius::Row;

pub fn spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![
            Field::new("kind", "Tipo", FieldKind::Select(vec![
                ("full", "Completo"),
                ("differential", "Diferencial (lo cambiado desde el último completo)"),
                ("log", "Log de transacciones"),
            ]))
            .default_value("full")
            .help("El log necesita el modelo de recuperación FULL o BULK_LOGGED y un backup completo previo."),
            Field::new("destination", "Destino", FieldKind::Select(vec![
                ("disk", "Archivo en el servidor"),
                ("url", "URL (Azure Blob Storage o S3)"),
            ]))
            .default_value("disk"),
            Field::new("path", "Archivo", FieldKind::Text)
                .placeholder("vacío: la carpeta de backups del servidor")
                .help("Ruta en el servidor (no en esta máquina). Vacío, o una carpeta terminada en / o \\: el archivo \
                       se llama <base>_<tipo>_<fecha>.bak (.trn para el log).")
                .when("destination", &["disk"]),
            Field::new("url", "URL", FieldKind::Text)
                .placeholder("https://cuenta.blob.core.windows.net/contenedor/")
                .help("Terminada en /: se le agrega <base>_<tipo>_<fecha>.bak. Hace falta una credencial del servidor \
                       con el nombre del contenedor (SAS) o la indicada abajo.")
                .when("destination", &["url"]),
            Field::new("credential", "Credencial", FieldKind::Text)
                .placeholder("vacío: la del contenedor (SAS)")
                .help("Solo para una credencial con la clave de la cuenta de almacenamiento (WITH CREDENTIAL).")
                .when("destination", &["url"]),
            Field::new("overwrite", "Reemplazar los backups que tenga el archivo", FieldKind::Bool)
                .default_value("false")
                .help("INIT (FORMAT en una URL). Si no, el backup se agrega al archivo (NOINIT)."),
            Field::new("copy_only", "Solo copia (COPY_ONLY)", FieldKind::Bool)
                .default_value("false")
                .help("No altera la secuencia de backups (diferenciales y logs). Managed Instance solo acepta éstos, a una URL."),
            Field::new("compression", "Comprimir", FieldKind::Bool)
                .default_value("true")
                .help("SQL Server Express no comprime: destildalo ahí."),
            Field::new("checksum", "Verificar las páginas (CHECKSUM)", FieldKind::Bool).default_value("true"),
            Field::new("verify", "Comprobar el backup al terminar (RESTORE VERIFYONLY)", FieldKind::Bool).default_value("false"),
            Field::new("name", "Nombre del backup", FieldKind::Text).placeholder("vacío: <base> - <tipo>"),
        ],
        restore: true,
        restore_options: vec![
            Field::new("position", "Número de backup dentro del archivo (FILE)", FieldKind::Number)
                .placeholder("el del historial, o 1")
                .help("Un archivo puede tener varios backups (NOINIT). Se usa cuando el origen no viene del historial."),
            Field::new("recovery", "Al terminar", FieldKind::Select(vec![
                ("RECOVERY", "Dejar la base lista para usar (RECOVERY)"),
                ("NORECOVERY", "Dejarla restaurando, para aplicar después un diferencial o un log (NORECOVERY)"),
            ]))
            .default_value("RECOVERY"),
            Field::new("replace", "Reemplazar la base si ya existe (REPLACE)", FieldKind::Bool).default_value("true"),
            Field::new("relocate", "Ubicar los archivos de datos y de log (MOVE)", FieldKind::Bool)
                .default_value("true")
                .help("En una base nueva van a las carpetas predeterminadas del servidor, con el nombre de la base; en \
                       una existente, sobre sus archivos. Si no, a las rutas que dice el backup."),
            Field::new("single_user", "Desconectar a los demás antes (SINGLE_USER)", FieldKind::Bool)
                .default_value("true")
                .help("Si la base existe, se pasa a SINGLE_USER WITH ROLLBACK IMMEDIATE y vuelve a MULTI_USER al terminar."),
            Field::new("credential", "Credencial (origen en una URL)", FieldKind::Text)
                .placeholder("vacío: la del contenedor (SAS)"),
        ],
        delete: false,
        history: true,
        server_wide: false,
        script_database: "master",
        note: "BACKUP y RESTORE del servidor: los archivos quedan en el disco del servidor (la carpeta de backups \
               predeterminada si no indicás otra) o en una URL de Azure Blob Storage o S3 con una credencial del \
               servidor. Hace falta el rol db_backupoperator (o sysadmin) para el backup y dbcreator o sysadmin para \
               restaurar. El historial es el de msdb. Borrar un archivo de backup no se puede por SQL: se hace en el \
               servidor. Managed Instance solo acepta backups COPY_ONLY a una URL y restaurar desde una URL a una \
               base nueva.",
    }
}

/// `N'text'`, quotes doubled.
fn lit(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

fn opt<'a>(options: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    options.get(key).map(|v| v.trim()).filter(|v| !v.is_empty())
}

fn flag(options: &BTreeMap<String, String>, key: &str, default: bool) -> bool {
    opt(options, key).map_or(default, |v| v == "true")
}

fn database(db: &Option<String>) -> Result<&str> {
    db.as_deref().filter(|d| !d.trim().is_empty()).ok_or_else(|| Error::Query("Falta la base de datos.".into()))
}

fn is_url(location: &str) -> bool {
    let l = location.trim_start().to_ascii_lowercase();
    l.starts_with("https://") || l.starts_with("http://") || l.starts_with("s3://")
}

/// A backup of the history is `<file or URL>|<position>`; anything else is
/// a location the user typed.
fn source(id: &str) -> (&str, Option<u32>) {
    match id.rsplit_once('|') {
        Some((path, n)) if !path.is_empty() => match n.trim().parse() {
            Ok(n) => (path, Some(n)),
            Err(_) => (id, None),
        },
        _ => (id, None),
    }
}

/// `@var` ends in the folder separator the path already uses.
fn ensure_separator(var: &str) -> String {
    format!(
        "IF RIGHT({var}, 1) NOT IN (N'/', N'\\') SET {var} += CASE WHEN CHARINDEX(N'/', {var}) > 0 THEN N'/' ELSE N'\\' END;\n"
    )
}

pub fn script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { database: db, options } => backup(database(db)?, options),
        BackupAction::Restore { source, database: db, options } => restore(source, database(db)?, options),
        BackupAction::Delete { .. } => Err(Error::Unsupported(
            "SQL Server no borra archivos de backup por SQL: borralo en el servidor (el historial de msdb se limpia \
             con msdb.dbo.sp_delete_backuphistory)."
                .into(),
        )),
    }
}

fn backup(db: &str, options: &BTreeMap<String, String>) -> Result<String> {
    let (kind, label, ext) = match opt(options, "kind").unwrap_or("full") {
        "full" => ("full", "completo", "bak"),
        "differential" => ("diff", "diferencial", "bak"),
        "log" => ("log", "log", "trn"),
        other => return Err(Error::Query(format!("Tipo de backup desconocido: {other}."))),
    };
    let url = opt(options, "destination") == Some("url");
    // The generated name: <db>_<kind>_<yyyymmdd_hhmmss>.<ext>, from the
    // server's clock.
    let safe: String = db.chars().map(|c| if c.is_alphanumeric() || matches!(c, '_' | '-') { c } else { '_' }).collect();
    let file_name = format!(
        "{} + REPLACE(REPLACE(REPLACE(CONVERT(nvarchar(19), GETDATE(), 120), N'-', N''), N':', N''), N' ', N'_') + {}",
        lit(&format!("{safe}_{kind}_")),
        lit(&format!(".{ext}"))
    );
    let mut s = format!("-- Backup {label} de {}\n", quote_ident(Quote::Bracket, db));
    s += &format!("DECLARE @db sysname = {};\n", lit(db));
    if url {
        let target = opt(options, "url").ok_or_else(|| Error::Query("Falta la URL del backup.".into()))?;
        if !is_url(target) {
            return Err(Error::Query("La URL tiene que empezar con https:// o s3://.".into()));
        }
        s += &format!("DECLARE @file nvarchar(4000) = {};\n", lit(target));
        s += &format!("IF RIGHT(@file, 1) = N'/' SET @file += {file_name};\n");
    } else {
        match opt(options, "path") {
            Some(p) if !p.ends_with(['/', '\\']) => s += &format!("DECLARE @file nvarchar(4000) = {};\n", lit(p)),
            folder => {
                match folder {
                    Some(dir) => s += &format!("DECLARE @dir nvarchar(4000) = {};\n", lit(dir)),
                    None => {
                        // SERVERPROPERTY has it from 2019 on; before, the
                        // instance's registry (also emulated on Linux).
                        s += "DECLARE @dir nvarchar(4000) = CAST(SERVERPROPERTY('InstanceDefaultBackupPath') AS nvarchar(4000));\n\
                              IF @dir IS NULL\n\
                              \x20 EXEC master.dbo.xp_instance_regread N'HKEY_LOCAL_MACHINE', N'Software\\Microsoft\\MSSQLServer\\MSSQLServer', N'BackupDirectory', @dir OUTPUT;\n\
                              IF @dir IS NULL\n\
                              \x20 THROW 50000, N'No se encontró la carpeta de backups del servidor: indicá una ruta.', 1;\n";
                    }
                }
                s += &ensure_separator("@dir");
                s += &format!("DECLARE @file nvarchar(4000) = @dir + {file_name};\n");
            }
        }
    }
    let name = match opt(options, "name") {
        Some(n) => n.to_string(),
        None => format!("{db} - {label}"),
    };
    let credential = opt(options, "credential").filter(|_| url);

    let mut with = vec![];
    if kind == "diff" {
        with.push("DIFFERENTIAL".to_string());
    }
    if flag(options, "copy_only", false) {
        with.push("COPY_ONLY".into());
    }
    if let Some(c) = credential {
        with.push(format!("CREDENTIAL = {}", lit(c)));
    }
    let overwrite = flag(options, "overwrite", false);
    match (url, overwrite) {
        (true, true) => with.push("FORMAT".into()),
        (true, false) => {}
        (false, true) => with.push("INIT".into()),
        (false, false) => with.push("NOINIT".into()),
    }
    with.push(if flag(options, "compression", true) { "COMPRESSION" } else { "NO_COMPRESSION" }.into());
    let checksum = flag(options, "checksum", true);
    if checksum {
        with.push("CHECKSUM".into());
    }
    with.push(format!("NAME = {}", lit(&name)));
    with.push("STATS = 10".into());

    let device = if url { "URL" } else { "DISK" };
    let what = if kind == "log" { "LOG" } else { "DATABASE" };
    s += &format!("BACKUP {what} {} TO {device} = @file\n  WITH {};\n", quote_ident(Quote::Bracket, db), with.join(", "));
    if flag(options, "verify", false) {
        let mut verify = vec!["FILE = @position".to_string()];
        if let Some(c) = credential {
            verify.push(format!("CREDENTIAL = {}", lit(c)));
        }
        if checksum {
            verify.push("CHECKSUM".into());
        }
        s += "DECLARE @position int = (\n\
              \x20 SELECT TOP (1) b.position FROM msdb.dbo.backupset b\n\
              \x20 JOIN msdb.dbo.backupmediafamily m ON m.media_set_id = b.media_set_id\n\
              \x20 WHERE m.physical_device_name = @file AND b.database_name = @db ORDER BY b.backup_set_id DESC);\n";
        s += &format!("RESTORE VERIFYONLY FROM {device} = @file WITH {};\n", verify.join(", "));
    }
    s += "SELECT @db AS base, @file AS archivo;";
    Ok(s)
}

/// `RESTORE FILELISTONLY`'s columns (2008 on; `SnapshotURL` from 2016).
const FILELIST_COLUMNS: &str = "LogicalName, PhysicalName, Type, FileGroupName, Size, MaxSize, FileID, CreateLSN, DropLSN, \
UniqueID, ReadOnlyLSN, ReadWriteLSN, BackupSizeInBytes, SourceBlockSize, FileGroupID, LogGroupGUID, DifferentialBaseLSN, \
DifferentialBaseGUID, IsReadOnly, IsPresent, TDEThumbprint";

fn restore(id: &str, db: &str, options: &BTreeMap<String, String>) -> Result<String> {
    let (path, position) = source(id.trim());
    if path.trim().is_empty() {
        return Err(Error::Query("Falta el archivo del backup.".into()));
    }
    let position = match (position, opt(options, "position")) {
        (Some(n), _) => n,
        (None, Some(p)) => p.parse::<u32>().ok().filter(|n| *n > 0).ok_or_else(|| Error::Query("El número de backup tiene que ser 1 o más.".into()))?,
        (None, None) => 1,
    };
    let url = is_url(path);
    let device = if url { "URL" } else { "DISK" };
    let credential = opt(options, "credential").filter(|_| url);
    let cred = credential.map(|c| format!(", CREDENTIAL = {}", lit(c))).unwrap_or_default();
    let recovery = match opt(options, "recovery").unwrap_or("RECOVERY") {
        r @ ("RECOVERY" | "NORECOVERY") => r,
        other => return Err(Error::Query(format!("Opción de recuperación desconocida: {other}."))),
    };
    let q = quote_ident(Quote::Bracket, db);
    let single_user = flag(options, "single_user", true);

    let mut s = format!("-- Restaurar {q} desde un backup\n");
    s += &format!(
        "DECLARE @db sysname = {};\nDECLARE @src nvarchar(4000) = {};\nDECLARE @position int = {position};\n",
        lit(db),
        lit(path)
    );
    // A log backup is restored with RESTORE LOG: msdb says which one it is.
    s += "DECLARE @kind char(1) = 'D';\n\
          SELECT TOP (1) @kind = b.type FROM msdb.dbo.backupset b\n\
          \x20 JOIN msdb.dbo.backupmediafamily m ON m.media_set_id = b.media_set_id\n\
          \x20 WHERE m.physical_device_name = @src AND b.position = @position ORDER BY b.backup_set_id DESC;\n\
          DECLARE @mi bit = CASE WHEN CAST(SERVERPROPERTY('EngineEdition') AS int) = 8 THEN 1 ELSE 0 END;\n";
    s += &format!(
        "DECLARE @sql nvarchar(max) = CASE WHEN @kind = 'L' THEN N'RESTORE LOG ' ELSE N'RESTORE DATABASE ' END\n\
         \x20 + QUOTENAME(@db) + N' FROM {device} = @src WITH FILE = @position';\n"
    );
    if flag(options, "relocate", true) {
        // Each file of the backup: over the existing database's file of the
        // same logical name, or to the default folders with the new name.
        let filelist = format!("N'RESTORE FILELISTONLY FROM {device} = @src WITH FILE = @position{}'", cred.replace('\'', "''"));
        s += &format!(
            "IF @kind <> 'L' AND @mi = 0\n\
             BEGIN\n\
             \x20 CREATE TABLE #files (LogicalName nvarchar(128), PhysicalName nvarchar(260), Type char(1), FileGroupName nvarchar(128),\n\
             \x20   Size numeric(20, 0), MaxSize numeric(20, 0), FileID bigint, CreateLSN numeric(25, 0), DropLSN numeric(25, 0),\n\
             \x20   UniqueID uniqueidentifier, ReadOnlyLSN numeric(25, 0), ReadWriteLSN numeric(25, 0), BackupSizeInBytes bigint,\n\
             \x20   SourceBlockSize int, FileGroupID int, LogGroupGUID uniqueidentifier, DifferentialBaseLSN numeric(25, 0),\n\
             \x20   DifferentialBaseGUID uniqueidentifier, IsReadOnly bit, IsPresent bit, TDEThumbprint varbinary(32), SnapshotURL nvarchar(360));\n\
             \x20 IF CAST(PARSENAME(CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(32)), 4) AS int) >= 13\n\
             \x20   INSERT INTO #files EXEC sp_executesql {filelist}, N'@src nvarchar(4000), @position int', @src, @position;\n\
             \x20 ELSE\n\
             \x20   INSERT INTO #files ({FILELIST_COLUMNS})\n\
             \x20     EXEC sp_executesql {filelist}, N'@src nvarchar(4000), @position int', @src, @position;\n\
             \x20 DECLARE @data nvarchar(4000) = CAST(SERVERPROPERTY('InstanceDefaultDataPath') AS nvarchar(4000));\n\
             \x20 DECLARE @log nvarchar(4000) = CAST(SERVERPROPERTY('InstanceDefaultLogPath') AS nvarchar(4000));\n\
             \x20 IF @data IS NULL\n\
             \x20   EXEC master.dbo.xp_instance_regread N'HKEY_LOCAL_MACHINE', N'Software\\Microsoft\\MSSQLServer\\MSSQLServer', N'DefaultData', @data OUTPUT;\n\
             \x20 IF @log IS NULL\n\
             \x20   EXEC master.dbo.xp_instance_regread N'HKEY_LOCAL_MACHINE', N'Software\\Microsoft\\MSSQLServer\\MSSQLServer', N'DefaultLog', @log OUTPUT;\n\
             \x20 IF @data IS NULL -- the folder of master's data file\n\
             \x20   SELECT @data = LEFT(physical_name, LEN(physical_name) - PATINDEX(N'%[/\\]%', REVERSE(physical_name)) + 1)\n\
             \x20     FROM sys.master_files WHERE database_id = 1 AND file_id = 1;\n\
             \x20 IF @log IS NULL SET @log = @data;\n\
             \x20 {}\
             \x20 {}\
             \x20 DECLARE @logs int = (SELECT COUNT(*) FROM #files WHERE Type = 'L');\n\
             \x20 DECLARE @id bigint = (SELECT MIN(FileID) FROM #files);\n\
             \x20 WHILE @id IS NOT NULL\n\
             \x20 BEGIN\n\
             \x20   SELECT @sql += N', MOVE N''' + REPLACE(f.LogicalName, N'''', N'''''') + N''' TO N'''\n\
             \x20     + REPLACE(COALESCE(cur.physical_name,\n\
             \x20         CASE WHEN f.Type = 'L' THEN @log ELSE @data END + @db\n\
             \x20         + CASE WHEN f.Type = 'L' THEN CASE WHEN @logs = 1 THEN N'_log.ldf' ELSE N'_' + f.LogicalName + N'.ldf' END\n\
             \x20                WHEN f.Type = 'D' THEN CASE WHEN f.FileID = 1 THEN N'.mdf' ELSE N'_' + f.LogicalName + N'.ndf' END\n\
             \x20                ELSE N'_' + f.LogicalName END), N'''', N'''''') + N''''\n\
             \x20     FROM #files f\n\
             \x20     LEFT JOIN sys.master_files cur ON cur.database_id = DB_ID(@db) AND cur.name = f.LogicalName\n\
             \x20     WHERE f.FileID = @id;\n\
             \x20   SET @id = (SELECT MIN(FileID) FROM #files WHERE FileID > @id);\n\
             \x20 END\n\
             \x20 DROP TABLE #files;\n\
             END\n",
            ensure_separator("@data"),
            ensure_separator("@log")
        );
    }
    let mut with = String::new();
    if flag(options, "replace", true) {
        with += ", REPLACE";
    }
    with += &format!(", {recovery}, STATS = 10{cred}");
    s += &format!("SET @sql += N'{}';\n", with.replace('\'', "''"));
    // Managed Instance restores a URL to a new database, without options.
    s += &format!("IF @mi = 1 SET @sql = N'RESTORE DATABASE ' + QUOTENAME(@db) + N' FROM URL = @src{}';\n", cred.replace('\'', "''"));
    if single_user {
        s += &format!(
            "DECLARE @single bit = 0;\n\
             IF DB_ID(@db) IS NOT NULL AND DATABASEPROPERTYEX(@db, 'Status') = N'ONLINE' AND DATABASEPROPERTYEX(@db, 'UserAccess') = N'MULTI_USER'\n\
             BEGIN\n\
             \x20 ALTER DATABASE {q} SET SINGLE_USER WITH ROLLBACK IMMEDIATE;\n\
             \x20 SET @single = 1;\n\
             END\n"
        );
    }
    s += "EXEC sp_executesql @sql, N'@src nvarchar(4000), @position int', @src, @position;\n";
    if single_user {
        // Also when the restore failed: the database is left as it was.
        s += &format!(
            "IF @single = 1 AND DATABASEPROPERTYEX(@db, 'Status') = N'ONLINE' AND DATABASEPROPERTYEX(@db, 'UserAccess') = N'SINGLE_USER'\n\
             \x20 ALTER DATABASE {q} SET MULTI_USER;\n"
        );
    }
    s += "SELECT @db AS base, @src AS origen, @position AS backup_numero;";
    Ok(s)
}

const HISTORY: &str = "
SELECT TOP (500) b.database_name, b.type,
       CONVERT(nvarchar(30), b.backup_start_date, 126), CONVERT(nvarchar(30), b.backup_finish_date, 126),
       CAST(b.backup_size AS bigint), CAST(b.compressed_backup_size AS bigint),
       m.physical_device_name, b.position, CAST(b.is_copy_only AS int), b.recovery_model,
       b.server_name, b.user_name, b.name, b.description, CAST(b.has_backup_checksums AS int),
       CAST(m.device_type AS int), CAST(b.is_damaged AS int),
       (SELECT COUNT(*) FROM msdb.dbo.backupmediafamily f WHERE f.media_set_id = b.media_set_id AND f.mirror = 0),
       CAST(b.first_lsn AS nvarchar(40)), CAST(b.last_lsn AS nvarchar(40)),
       CAST(b.software_major_version AS nvarchar(10)) + N'.' + CAST(b.software_minor_version AS nvarchar(10))
                                                   + N'.' + CAST(b.software_build_version AS nvarchar(10))
  FROM msdb.dbo.backupset b
  JOIN msdb.dbo.backupmediafamily m ON m.media_set_id = b.media_set_id AND m.family_sequence_number = 1 AND m.mirror = 0";

fn text(r: &Row, i: usize) -> Option<String> {
    r.try_get::<&str, _>(i).ok().flatten().map(str::to_string).filter(|s| !s.is_empty())
}
fn int(r: &Row, i: usize) -> Option<i64> {
    r.try_get::<i64, _>(i)
        .ok()
        .flatten()
        .or_else(|| r.try_get::<i32, _>(i).ok().flatten().map(i64::from))
        .or_else(|| r.try_get::<u8, _>(i).ok().flatten().map(i64::from))
}

fn kind_label(t: &str) -> &str {
    match t {
        "D" => "Completo",
        "I" => "Diferencial",
        "L" => "Log de transacciones",
        "F" => "Archivo o grupo de archivos",
        "G" => "Diferencial de archivo",
        "P" => "Parcial",
        "Q" => "Diferencial parcial",
        other => other,
    }
}

fn device_label(t: i64) -> &'static str {
    match t {
        2 => "Disco",
        5 => "Cinta",
        7 => "Dispositivo virtual (herramienta externa)",
        9 => "URL",
        _ => "Otro",
    }
}

fn bytes(n: i64) -> String {
    let n = n as f64;
    match n {
        n if n >= 1024.0 * 1024.0 * 1024.0 => format!("{:.1} GB", n / 1024.0 / 1024.0 / 1024.0),
        n if n >= 1024.0 * 1024.0 => format!("{:.1} MB", n / 1024.0 / 1024.0),
        n => format!("{:.0} KB", n / 1024.0),
    }
}

pub async fn history(s: &mut SqlServerSession, database: Option<&str>) -> Result<Vec<BackupEntry>> {
    let order = " ORDER BY b.backup_finish_date DESC, b.backup_set_id DESC";
    let rows = match database {
        Some(db) => s.rows(&format!("{HISTORY} WHERE b.database_name = @P1{order}"), &[db]).await?,
        None => s.rows(&format!("{HISTORY}{order}"), &[]).await?,
    };
    Ok(rows.iter().map(entry).collect())
}

fn entry(r: &Row) -> BackupEntry {
    let kind = text(r, 1).unwrap_or_default();
    let path = text(r, 6).unwrap_or_default();
    let position = int(r, 7).unwrap_or(1);
    let device = int(r, 15).unwrap_or(0);
    let damaged = int(r, 16).unwrap_or(0) != 0;
    let families = int(r, 17).unwrap_or(1);
    let size = int(r, 4);
    let compressed = int(r, 5);

    let mut details = vec![("Posición en el archivo".to_string(), position.to_string())];
    details.push(("Dispositivo".into(), device_label(device).into()));
    if let (Some(c), Some(s)) = (compressed, size) {
        if c > 0 && c < s {
            details.push(("Tamaño comprimido".into(), format!("{} ({:.0} %)", bytes(c), c as f64 * 100.0 / s as f64)));
        }
    }
    if int(r, 8).unwrap_or(0) != 0 {
        details.push(("Solo copia".into(), "Sí (COPY_ONLY)".into()));
    }
    if let Some(m) = text(r, 9) {
        details.push(("Modelo de recuperación".into(), m));
    }
    details.push(("Checksum".into(), if int(r, 14).unwrap_or(0) != 0 { "Sí" } else { "No" }.into()));
    if families > 1 {
        details.push(("Archivos del backup".into(), format!("{families} (repartido: restauralo desde el servidor)")));
    }
    for (label, i) in [("Nombre", 12), ("Descripción", 13), ("Servidor", 10), ("Usuario", 11), ("Primer LSN", 18), ("Último LSN", 19), ("Versión del servidor", 20)] {
        if let Some(v) = text(r, i) {
            details.push((label.into(), v));
        }
    }

    BackupEntry {
        id: format!("{path}|{position}"),
        database: text(r, 0),
        kind: Some(kind_label(&kind).to_string()),
        started: text(r, 2),
        finished: text(r, 3),
        size: size.and_then(|n| u64::try_from(n).ok()),
        location: Some(path),
        status: Some(if damaged { "Dañado" } else { "Completado" }.into()),
        details,
        restorable: !damaged && families == 1 && matches!(kind.as_str(), "D" | "I" | "L") && matches!(device, 2 | 9),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn b(db: &str, options: &[(&str, &str)]) -> String {
        script(&BackupAction::Backup { database: Some(db.into()), options: o(options) }).unwrap()
    }

    fn r(source: &str, db: &str, options: &[(&str, &str)]) -> String {
        script(&BackupAction::Restore { source: source.into(), database: Some(db.into()), options: o(options) }).unwrap()
    }

    #[test]
    fn full_backup_to_the_default_folder() {
        let s = b("ven]tas", &[]);
        assert!(s.contains("DECLARE @db sysname = N'ven]tas';"));
        assert!(s.contains("SERVERPROPERTY('InstanceDefaultBackupPath')"));
        assert!(s.contains("xp_instance_regread"));
        assert!(s.contains("N'ven_tas_full_' + REPLACE("));
        assert!(s.contains(
            "BACKUP DATABASE [ven]]tas] TO DISK = @file\n  WITH NOINIT, COMPRESSION, CHECKSUM, NAME = N'ven]tas - completo', STATS = 10;"
        ));
        assert!(!s.contains("VERIFYONLY"));
        assert!(!s.lines().any(|l| l.trim().eq_ignore_ascii_case("go")));
    }

    #[test]
    fn differential_and_log_backups() {
        let s = b("db", &[("kind", "differential"), ("path", "/backups/"), ("copy_only", "true"), ("compression", "false")]);
        assert!(s.contains("DECLARE @dir nvarchar(4000) = N'/backups/';"));
        assert!(!s.contains("InstanceDefaultBackupPath"));
        assert!(s.contains("WITH DIFFERENTIAL, COPY_ONLY, NOINIT, NO_COMPRESSION, CHECKSUM"));
        let s = b("db", &[("kind", "log"), ("path", "C:\\bk\\o'k.trn"), ("overwrite", "true"), ("verify", "true")]);
        assert!(s.contains("DECLARE @file nvarchar(4000) = N'C:\\bk\\o''k.trn';"));
        assert!(s.contains("BACKUP LOG [db] TO DISK = @file\n  WITH INIT,"));
        assert!(s.contains("RESTORE VERIFYONLY FROM DISK = @file WITH FILE = @position, CHECKSUM;"));
    }

    #[test]
    fn backup_to_url() {
        let s = b("db", &[("destination", "url"), ("url", "https://a.blob.core.windows.net/c/"), ("credential", "k"), ("overwrite", "true")]);
        assert!(s.contains("IF RIGHT(@file, 1) = N'/' SET @file += N'db_full_'"));
        assert!(s.contains("TO URL = @file\n  WITH CREDENTIAL = N'k', FORMAT, COMPRESSION"));
        assert!(script(&BackupAction::Backup { database: Some("db".into()), options: o(&[("destination", "url")]) }).is_err());
    }

    #[test]
    fn restore_relocates_and_goes_single_user() {
        let s = r("/var/opt/mssql/data/a'b.bak|3", "nue]va", &[]);
        assert!(s.contains("DECLARE @src nvarchar(4000) = N'/var/opt/mssql/data/a''b.bak';"));
        assert!(s.contains("DECLARE @position int = 3;"));
        assert!(s.contains("N'RESTORE FILELISTONLY FROM DISK = @src WITH FILE = @position'"));
        assert!(s.contains("InstanceDefaultDataPath"));
        assert!(s.contains("SET @sql += N', REPLACE, RECOVERY, STATS = 10';"));
        assert!(s.contains("ALTER DATABASE [nue]]va] SET SINGLE_USER WITH ROLLBACK IMMEDIATE;"));
        assert!(s.contains("ALTER DATABASE [nue]]va] SET MULTI_USER;"));
        assert!(!s.lines().any(|l| l.trim().eq_ignore_ascii_case("go")));
    }

    #[test]
    fn restore_options() {
        let s = r("/x.bak", "db", &[("position", "2"), ("recovery", "NORECOVERY"), ("replace", "false"), ("relocate", "false"), ("single_user", "false")]);
        assert!(s.contains("DECLARE @position int = 2;"));
        assert!(!s.contains("FILELISTONLY"));
        assert!(!s.contains("SINGLE_USER"));
        assert!(s.contains("SET @sql += N', NORECOVERY, STATS = 10';"));
        let s = r("https://a.blob.core.windows.net/c/x.bak", "db", &[("credential", "k'1")]);
        assert!(s.contains("FROM URL = @src"));
        assert!(s.contains("N'RESTORE FILELISTONLY FROM URL = @src WITH FILE = @position, CREDENTIAL = N''k''''1'''"));
        assert!(s.contains("SET @sql += N', REPLACE, RECOVERY, STATS = 10, CREDENTIAL = N''k''''1''';"));
        assert!(script(&BackupAction::Restore { source: "/x.bak".into(), database: Some("db".into()), options: o(&[("position", "0")]) }).is_err());
    }

    #[test]
    fn sources() {
        assert_eq!(source("/a/b.bak|2"), ("/a/b.bak", Some(2)));
        assert_eq!(source("/a/b|c.bak"), ("/a/b|c.bak", None));
        assert_eq!(source("/a/b.bak"), ("/a/b.bak", None));
    }

    #[test]
    fn no_file_deletion() {
        assert!(script(&BackupAction::Delete { source: "/x.bak|1".into() }).is_err());
    }
}
