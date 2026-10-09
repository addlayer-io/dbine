//! The engines' own backups (the Backups tab, docs/backups.md), for the
//! presets whose SQL can make one:
//!
//! - **Db2 for LUW**: `CALL SYSPROC.ADMIN_CMD('BACKUP DATABASE … ONLINE
//!   TO …')` and the history of `SYSIBMADM.DB_HISTORY`. ADMIN_CMD can't
//!   restore (RESTORE is a CLP command) nor delete a single backup (PRUNE
//!   HISTORY cuts the history by date).
//! - **SAP ASE**: `DUMP DATABASE … TO '<file>'` and `LOAD DATABASE` +
//!   `ONLINE DATABASE`, from `master`.
//! - **SQL Anywhere**: `BACKUP DATABASE DIRECTORY '<folder>'` (an image
//!   backup) or `BACKUP DATABASE TO '<archive>'`.
//! - **Informix / GBase 8s**: the SQL admin API, `task('ontape archive …')`
//!   or `task('onbar backup …')`, run in `sysadmin`; ON-Bar's history from
//!   `sysutils:bar_action` and the last level 0 archive of each dbspace.
//! - **IBM i**: `SAVLIB` to a save file, `RSTLIB` and `DLTF` through
//!   `QSYS2.QCMDEXC`, and the save files of `QSYS2.SAVE_FILE_INFO`.
//! - **Vertica** (Eon Mode): restore points (`SAVE RESTORE POINT TO
//!   ARCHIVE`), listed from `ARCHIVE_RESTORE_POINTS` and removed with
//!   `REMOVE RESTORE POINT`.
//! - **MonetDB**: `sys.hot_snapshot('<file.tar>')`.
//! - **Virtuoso**: `backup_online('<prefix>', <pages>)`.
//! - **Mimer SQL**: `START BACKUP; CREATE BACKUP IN '<file>' FOR DATABANK
//!   …; COMMIT BACKUP`, and each databank's last backup.
//! - **Dameng**: `BACKUP DATABASE … BACKUPSET '<folder>'` and `V$BACKUPSET`.
//! - **Machbase**: `BACKUP DATABASE INTO DISK = '<folder>'`.
//!
//! [`unsupported`] says why the rest have only DBine's copies.

use crate::presets::Preset;
use crate::OdbcSession;
use dbine_driver::{BackupAction, BackupEntry, BackupSpec, Error, Field, FieldKind, Result};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    Db2,
    Ase,
    SqlAnywhere,
    Informix,
    Db2i,
    Vertica,
    MonetDb,
    Virtuoso,
    Mimer,
    Dameng,
    Machbase,
}

pub fn engine(p: &Preset) -> Option<Engine> {
    Some(match p.id {
        "db2" => Engine::Db2,
        "sybase" => Engine::Ase,
        "sqlanywhere" => Engine::SqlAnywhere,
        "informix" | "gbase8s" => Engine::Informix,
        "db2i" => Engine::Db2i,
        "vertica" => Engine::Vertica,
        "monetdb" => Engine::MonetDb,
        "virtuoso" => Engine::Virtuoso,
        "mimer" => Engine::Mimer,
        "dameng" => Engine::Dameng,
        "machbase" => Engine::Machbase,
        _ => return None,
    })
}

/// Why a preset has no native backups in DBine (docs/engine-support.md).
pub fn unsupported(p: &Preset) -> &'static str {
    match p.id {
        "odbc" => "el motor detrás de un ODBC genérico es desconocido: DBine no sabe si tiene backups propios ni cómo se piden",
        "db2zos" => "los backups de Db2 for z/OS son utilidades (COPY) que se llaman con SYSPROC.DSNUTILU, cuyo código de retorno es un parámetro OUT que la ejecución de scripts no enlaza; se hacen desde un JCL o una herramienta del sistema",
        "teradata" => "Teradata no tiene sentencias de backup en SQL: se hacen con las herramientas de BAR (DSA o ARC)",
        "netezza" => "Netezza respalda con las herramientas nzbackup y nzrestore, no con SQL",
        "exasol" => "Exasol respalda desde la administración del clúster (ConfD o EXAoperation), no con SQL",
        "hive" | "cloudera" => "Hive no tiene backups de la base: solo EXPORT TABLE / IMPORT TABLE por tabla, que no reemplazan un backup",
        "impala" | "spark" | "kyuubi" => "este motor no tiene backups propios: los datos son archivos del almacenamiento distribuido (HDFS, S3…)",
        "access" | "dbase" => "la base es un archivo: se respalda copiándolo",
        "netsuite" => "SuiteAnalytics Connect es de solo lectura y NetSuite no ofrece backups por SQL",
        "iris" | "cache" => "InterSystems respalda con la clase Backup.General y las rutinas del sistema, no con SQL",
        "openedge" => "OpenEdge respalda con la herramienta probkup, no con SQL",
        "ingres" => "Ingres respalda con la herramienta ckpdb, no con SQL",
        "cubrid" => "CUBRID respalda con la herramienta cubrid backupdb, no con SQL",
        "zen" => "Actian Zen respalda con la herramienta butil (-startbu / -endbu), no con SQL",
        "maxdb" => "MaxDB respalda con el servidor de administración (dbmcli), no con SQL",
        "nuodb" => "NuoDB respalda con la herramienta nuocmd (hot copy), no con SQL",
        "ignite" => "Ignite 2 hace snapshots con control.sh, JMX o la API de Java, no con SQL",
        "ignite3" => "Ignite 3 no ofrece backups por SQL",
        "ocient" | "sqream" => "este motor no ofrece backups por SQL",
        "heavydb" => "HeavyDB respalda tabla por tabla (DUMP TABLE / RESTORE TABLE), no una base entera",
        "altibase" => "el backup de Altibase (ALTER DATABASE BACKUP) solo lo hace SYS conectado en modo SYSDBA y restaurar se hace con la base en fase MOUNT",
        _ => "este motor no tiene backups propios que DBine pueda pedir por SQL",
    }
}

fn opt<'a>(o: &'a BTreeMap<String, String>, k: &str) -> Option<&'a str> {
    o.get(k).map(|s| s.trim()).filter(|s| !s.is_empty())
}

fn on(o: &BTreeMap<String, String>, k: &str, default: bool) -> bool {
    opt(o, k).map_or(default, |v| v == "true")
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn need<'a>(o: &'a BTreeMap<String, String>, k: &str, what: &str) -> Result<&'a str> {
    opt(o, k).ok_or_else(|| Error::Query(format!("falta {what}")))
}

/// `(yyyy, mm, dd, hh, mi, ss)` of now, UTC.
fn now() -> (i64, i64, i64, u64, u64, u64) {
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
    (y, m, d, rest / 3600, rest % 3600 / 60, rest % 60)
}

pub fn spec(p: &Preset) -> Option<BackupSpec> {
    Some(match engine(p)? {
        Engine::Db2 => BackupSpec {
            backup_options: vec![
                Field::new(
                    "kind",
                    "Tipo",
                    FieldKind::Select(vec![
                        ("full", "Completo"),
                        ("incremental", "Incremental (lo que cambió desde el último completo)"),
                        ("delta", "Delta (lo que cambió desde el último backup de cualquier tipo)"),
                    ]),
                )
                .default_value("full")
                .help("Los incrementales piden el parámetro TRACKMOD de la base en YES."),
                Field::new("path", "Carpeta del servidor", FieldKind::Text)
                    .required()
                    .placeholder("/db2/backups")
                    .help("Carpeta del servidor de Db2 donde se escribe la imagen del backup; tiene que existir y el usuario de la instancia tiene que poder escribir en ella."),
                Field::new("compress", "Comprimir", FieldKind::Bool).default_value("true"),
                Field::new("logs", "Incluir los logs", FieldKind::Bool)
                    .default_value("true")
                    .help("Guarda con la imagen los logs necesarios para dejarla consistente al restaurarla."),
            ],
            restore: false,
            restore_options: vec![],
            delete: false,
            history: true,
            server_wide: true,
            script_database: "",
            note: "El backup es en línea (ADMIN_CMD no hace backups fuera de línea) y pide que la base tenga logging \
                   de archivo (LOGARCHMETH1); lo hace un usuario con SYSADM, SYSCTRL o SYSMAINT. Restaurar se hace \
                   con el comando RESTORE DATABASE del procesador de línea de comandos de Db2, con la base sin uso: \
                   no se puede por SQL.",
        },
        Engine::Ase => BackupSpec {
            backup_options: vec![
                Field::new("path", "Archivo", FieldKind::Text)
                    .required()
                    .placeholder("/sybase/dumps/ventas.dmp")
                    .help("Ruta del archivo del dump en la máquina del Backup Server."),
                Field::new(
                    "compression",
                    "Compresión",
                    FieldKind::Select(vec![
                        ("", "Sin comprimir"),
                        ("100", "Rápida (100)"),
                        ("101", "Mayor compresión (101)"),
                        ("1", "Nivel 1"),
                        ("6", "Nivel 6"),
                        ("9", "Nivel 9"),
                    ]),
                )
                .default_value(""),
            ],
            restore: true,
            restore_options: vec![],
            delete: false,
            history: false,
            server_wide: false,
            script_database: "master",
            note: "El dump lo escribe el Backup Server en su máquina; hace falta que esté corriendo. Restaurar carga el \
                   dump en una base que ya tiene que existir, con espacio suficiente y sin nadie conectado, y la \
                   vuelve a poner en línea. Los dumps no se borran por SQL.",
        },
        Engine::SqlAnywhere => BackupSpec {
            backup_options: vec![
                Field::new(
                    "mode",
                    "Tipo",
                    FieldKind::Select(vec![("image", "Imagen (copia de los archivos de la base)"), ("archive", "Archivo único")]),
                )
                .default_value("image"),
                Field::new("path", "Destino", FieldKind::Text)
                    .required()
                    .placeholder("/sqlany/backups/ventas")
                    .help("Imagen: carpeta del servidor donde se copian los archivos. Archivo único: ruta y nombre base del archivo (el servidor le agrega la extensión)."),
                Field::new(
                    "log",
                    "Log de transacciones",
                    FieldKind::Select(vec![
                        ("", "Dejarlo como está"),
                        ("rename", "Renombrarlo y empezar uno nuevo"),
                        ("truncate", "Truncarlo"),
                    ]),
                )
                .default_value("")
                .when("mode", &["image"]),
                Field::new("comment", "Comentario", FieldKind::Text),
            ],
            restore: false,
            restore_options: vec![],
            delete: false,
            history: false,
            server_wide: true,
            script_database: "",
            note: "Lo hace un usuario con el permiso BACKUP DATABASE; los archivos quedan en la máquina del servidor. \
                   Una imagen se restaura copiando sus archivos con la base detenida; un archivo único, con RESTORE \
                   DATABASE conectado a utility_db. El servidor anota cada backup en backup.syb, que no se lee por SQL.",
        },
        Engine::Informix => BackupSpec {
            backup_options: vec![
                Field::new(
                    "tool",
                    "Herramienta",
                    FieldKind::Select(vec![("ontape", "ontape (a una carpeta)"), ("onbar", "ON-Bar (al gestor de almacenamiento)")]),
                )
                .default_value("ontape"),
                Field::new(
                    "level",
                    "Nivel",
                    FieldKind::Select(vec![("0", "0: completo"), ("1", "1: lo que cambió desde el nivel 0"), ("2", "2: lo que cambió desde el nivel 1")]),
                )
                .default_value("0"),
                Field::new("path", "Carpeta del servidor", FieldKind::Text)
                    .required()
                    .placeholder("/informix/backups/")
                    .help("Carpeta de la máquina del servidor donde ontape deja el archivo; tiene que existir.")
                    .when("tool", &["ontape"]),
            ],
            restore: false,
            restore_options: vec![],
            delete: false,
            history: true,
            server_wide: true,
            script_database: "sysadmin",
            note: "El backup es de toda la instancia y lo hace un usuario con permiso sobre la base sysadmin (informix \
                   o un administrador). ON-Bar necesita un gestor de almacenamiento configurado (PSM). Restaurar se \
                   hace con ontape -r u onbar -r, en general con el servidor detenido: no se puede por SQL.",
        },
        Engine::Db2i => BackupSpec {
            backup_options: vec![
                Field::new("library", "Biblioteca", FieldKind::Text)
                    .required()
                    .placeholder("VENTAS")
                    .help("La biblioteca (esquema) que se respalda."),
                Field::new("savf_library", "Biblioteca del archivo de salvar", FieldKind::Text).default_value("QGPL"),
                Field::new("savf", "Archivo de salvar", FieldKind::Text)
                    .placeholder("(DB y la fecha)")
                    .help("Nombre del archivo de salvar (*SAVF) que se crea, de hasta 10 caracteres."),
                Field::new(
                    "compress",
                    "Compresión",
                    FieldKind::Select(vec![("*YES", "Sí"), ("*NO", "No"), ("*MEDIUM", "Media"), ("*HIGH", "Alta")]),
                )
                .default_value("*YES"),
                Field::new(
                    "active",
                    "Guardar mientras se usa",
                    FieldKind::Select(vec![("*NO", "No (los objetos en uso fallan)"), ("*LIB", "Sí, por biblioteca"), ("*SYNCLIB", "Sí, sincronizado")]),
                )
                .default_value("*LIB"),
            ],
            restore: true,
            restore_options: vec![Field::new("replace", "Reemplazar todos los miembros", FieldKind::Bool)
                .default_value("false")
                .help("MBROPT(*ALL): reemplaza también los miembros de archivos que ya existen.")],
            delete: true,
            history: true,
            server_wide: true,
            script_database: "",
            note: "SAVLIB guarda una biblioteca en un archivo de salvar del sistema; hace falta la autorización *SAVSYS \
                   o permisos sobre los objetos. Restaurar con otro nombre crea la biblioteca nueva, sin sus diarios. \
                   El historial lista los archivos de salvar (IBM i 7.4 TR8 o 7.5 TR2 en adelante).",
        },
        Engine::Vertica => BackupSpec {
            backup_options: vec![
                Field::new("archive", "Archivo de puntos de restauración", FieldKind::Text)
                    .required()
                    .placeholder("diario")
                    .help("El archivo (ARCHIVE) donde se guarda el punto de restauración."),
                Field::new("create", "Crear el archivo", FieldKind::Bool)
                    .default_value("false")
                    .help("CREATE ARCHIVE antes de guardar, la primera vez."),
                Field::new("limit", "Máximo de puntos", FieldKind::Number)
                    .placeholder("(sin límite)")
                    .help("Solo al crearlo: al llegar al máximo se borra el punto más viejo.")
                    .when("create", &["true"]),
            ],
            restore: false,
            restore_options: vec![],
            delete: true,
            history: true,
            server_wide: true,
            script_database: "",
            note: "Solo en Eon Mode (Vertica 24.1 en adelante) y con un superusuario: el punto de restauración queda \
                   en el almacenamiento comunal de la base. La base se revive desde un punto con admintools o el \
                   operador de Kubernetes, no por SQL. En Enterprise Mode los backups se hacen con vbr.",
        },
        Engine::MonetDb => BackupSpec {
            backup_options: vec![Field::new("path", "Archivo del servidor", FieldKind::Text)
                .required()
                .placeholder("/backups/ventas.tar.gz")
                .help("Ruta en la máquina del servidor. La extensión elige la compresión: .tar, .tar.gz, .tar.lz4, .tar.bz2 o .tar.xz.")],
            restore: false,
            restore_options: vec![],
            delete: false,
            history: false,
            server_wide: true,
            script_database: "",
            note: "Lo hace el administrador (monetdb) o quien tenga EXECUTE sobre sys.hot_snapshot, desde MonetDB \
                   Jun2020. Restaurar es desempaquetar el .tar en la granja de bases con el servidor detenido.",
        },
        Engine::Virtuoso => BackupSpec {
            backup_options: vec![
                Field::new("prefix", "Prefijo", FieldKind::Text)
                    .required()
                    .placeholder("dbine_")
                    .help("Los archivos se llaman <prefijo><n>.bp."),
                Field::new("dir", "Carpeta del servidor", FieldKind::Text)
                    .placeholder("(la carpeta del servidor)")
                    .help("Carpeta de la máquina del servidor donde se escriben; tiene que estar permitida en DirsAllowed."),
                Field::new("pages", "Páginas por archivo", FieldKind::Number).default_value("100000"),
                Field::new("new", "Empezar una serie nueva (completo)", FieldKind::Bool)
                    .default_value("true")
                    .help("Sin tildar, sigue la serie anterior con un backup incremental."),
            ],
            restore: false,
            restore_options: vec![],
            delete: false,
            history: false,
            server_wide: true,
            script_database: "",
            note: "Lo hace dba; mientras corre no hay checkpoints. Restaurar se hace con el servidor detenido \
                   (virtuoso-t +restore-backup <prefijo>), no por SQL.",
        },
        Engine::Mimer => BackupSpec {
            backup_options: vec![
                Field::new("databank", "Databank", FieldKind::Text)
                    .required()
                    .placeholder("VENTAS")
                    .help("El databank que se respalda."),
                Field::new("path", "Carpeta del servidor", FieldKind::Text)
                    .required()
                    .placeholder("/mimer/backups")
                    .help("Carpeta de la máquina del servidor; el archivo se llama <databank>_<fecha>.dbk."),
                Field::new("logdb", "Incluir LOGDB", FieldKind::Bool)
                    .default_value("true")
                    .help("Recomendado: el backup de LOGDB guarda los cambios hechos desde el anterior y permite restaurar hasta ese momento."),
            ],
            restore: false,
            restore_options: vec![],
            delete: false,
            history: true,
            server_wide: true,
            script_database: "",
            note: "Lo hace el creador del databank o quien tenga el permiso BACKUP. Restaurar pide copiar antes el \
                   archivo del backup en lugar del databank (a mano, en el servidor) y después aplicar el log con \
                   ALTER DATABANK … RESTORE: no se hace solo por SQL. El historial muestra el último backup de cada \
                   databank.",
        },
        Engine::Dameng => BackupSpec {
            backup_options: vec![
                Field::new("kind", "Tipo", FieldKind::Select(vec![("full", "Completo"), ("increment", "Incremental")])).default_value("full"),
                Field::new("path", "Carpeta del conjunto", FieldKind::Text)
                    .required()
                    .placeholder("/dm/backups/full_1")
                    .help("Carpeta del servidor donde se crea el conjunto de backup (BACKUPSET); no tiene que existir."),
                Field::new("compress", "Comprimir", FieldKind::Bool).default_value("true"),
            ],
            restore: false,
            restore_options: vec![],
            delete: false,
            history: true,
            server_wide: true,
            script_database: "",
            note: "El backup en línea pide la base en modo archivo (ARCHIVELOG) y un usuario con DBA o BACKUP ANY \
                   DATABASE. Restaurar la base se hace con DMRMAN y el servidor detenido. El historial lista los \
                   conjuntos de la carpeta de backups predeterminada.",
        },
        Engine::Machbase => BackupSpec {
            backup_options: vec![Field::new("path", "Carpeta del servidor", FieldKind::Text)
                .required()
                .placeholder("/machbase/backups/2026_09_29")
                .help("Carpeta de la máquina del servidor donde se escribe el backup.")],
            restore: false,
            restore_options: vec![],
            delete: false,
            history: false,
            server_wide: true,
            script_database: "",
            note: "Lo hace SYS. Un backup se puede montar de solo lectura (MOUNT DATABASE) para consultarlo; la base \
                   se restaura con machadmin -r y el servidor detenido.",
        },
    })
}

// -- scripts -------------------------------------------------------------------

/// An IBM i object name (`VENTAS`, `$TMP#1`), in upper case.
fn cl_name(v: &str, what: &str) -> Result<String> {
    let n = v.trim().to_ascii_uppercase();
    let ok = (1..=10).contains(&n.len())
        && n.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || "$#@".contains(c))
        && n.chars().all(|c| c.is_ascii_alphanumeric() || "$#@_.".contains(c));
    if ok {
        Ok(n)
    } else {
        Err(Error::Query(format!("«{v}» no es un nombre válido de {what} de IBM i")))
    }
}

/// `LIB/FILE`.
fn savf(v: &str) -> Result<(String, String)> {
    let (l, f) = v.trim().split_once('/').ok_or_else(|| Error::Query(format!("«{v}» no es BIBLIOTECA/ARCHIVO")))?;
    Ok((cl_name(l, "biblioteca")?, cl_name(f, "archivo")?))
}

fn qcmdexc(cmd: &str) -> String {
    format!("CALL QSYS2.QCMDEXC({});", lit(cmd))
}

pub fn script(p: &Preset, a: &BackupAction) -> Result<String> {
    let Some(e) = engine(p) else { return Err(Error::Unsupported(unsupported(p).into())) };
    match (e, a) {
        (Engine::Db2, BackupAction::Backup { database, options }) => {
            let path = need(options, "path", "la carpeta del servidor")?;
            let mut tail = String::from(" ONLINE");
            match opt(options, "kind") {
                Some("incremental") => tail.push_str(" INCREMENTAL"),
                Some("delta") => tail.push_str(" INCREMENTAL DELTA"),
                _ => {}
            }
            // The path is a literal inside ADMIN_CMD's literal.
            tail.push_str(&format!(" TO {}", path.replace('\'', "''")));
            if on(options, "compress", true) {
                tail.push_str(" COMPRESS");
            }
            tail.push_str(if on(options, "logs", true) { " INCLUDE LOGS" } else { " EXCLUDE LOGS" });
            // ADMIN_CMD backs up only the connected database.
            let db = database.as_deref().map(str::trim).filter(|d| !d.is_empty());
            Ok(match db {
                Some(db) if db.chars().all(|c| c.is_ascii_alphanumeric() || "_@#$".contains(c)) => {
                    format!("CALL SYSPROC.ADMIN_CMD({});", lit(&format!("BACKUP DATABASE {db}{tail}")))
                }
                Some(db) => return Err(Error::Query(format!("«{db}» no es un nombre de base de Db2"))),
                None => format!("CALL SYSPROC.ADMIN_CMD('BACKUP DATABASE ' || CURRENT SERVER || {});", lit(&tail)),
            })
        }
        (Engine::Ase, BackupAction::Backup { database, options }) => {
            let db = ase_db(database.as_deref())?;
            let path = need(options, "path", "la ruta del archivo")?;
            let with = match opt(options, "compression") {
                Some(n) if n.chars().all(|c| c.is_ascii_digit()) => format!(" WITH COMPRESSION = {n}"),
                Some(n) => return Err(Error::Query(format!("«{n}» no es un nivel de compresión"))),
                None => String::new(),
            };
            Ok(format!("DUMP DATABASE {db} TO {}{with}", lit(path)))
        }
        (Engine::Ase, BackupAction::Restore { source, database, .. }) => {
            let db = ase_db(database.as_deref())?;
            let source = source.trim();
            if source.is_empty() {
                return Err(Error::Query("falta el archivo del dump".into()));
            }
            Ok(format!("LOAD DATABASE {db} FROM {}\ngo\nONLINE DATABASE {db}", lit(source)))
        }
        (Engine::SqlAnywhere, BackupAction::Backup { options, .. }) => {
            let path = need(options, "path", "el destino")?;
            let mut sql = if opt(options, "mode") == Some("archive") {
                format!("BACKUP DATABASE TO {}", lit(path))
            } else {
                let mut s = format!("BACKUP DATABASE DIRECTORY {}", lit(path));
                match opt(options, "log") {
                    Some("rename") => s.push_str(" TRANSACTION LOG RENAME"),
                    Some("truncate") => s.push_str(" TRANSACTION LOG TRUNCATE"),
                    _ => {}
                }
                s
            };
            if let Some(c) = opt(options, "comment") {
                sql.push_str(&format!(" WITH COMMENT {}", lit(c)));
            }
            Ok(sql + ";")
        }
        (Engine::Informix, BackupAction::Backup { options, .. }) => {
            let level = match opt(options, "level").unwrap_or("0") {
                l @ ("0" | "1" | "2") => l,
                l => return Err(Error::Query(format!("«{l}» no es un nivel de backup (0, 1 o 2)"))),
            };
            Ok(if opt(options, "tool") == Some("onbar") {
                format!("EXECUTE FUNCTION task('onbar backup whole system level {level}');")
            } else {
                let mut dir = need(options, "path", "la carpeta del servidor")?.to_string();
                if !dir.ends_with('/') && !dir.ends_with('\\') {
                    dir.push('/');
                }
                format!("EXECUTE FUNCTION task('ontape archive directory level {level}', {});", lit(&dir))
            })
        }
        (Engine::Db2i, BackupAction::Backup { options, .. }) => {
            let lib = cl_name(need(options, "library", "la biblioteca")?, "biblioteca")?;
            let flib = cl_name(opt(options, "savf_library").unwrap_or("QGPL"), "biblioteca")?;
            let file = match opt(options, "savf") {
                Some(f) => cl_name(f, "archivo")?,
                None => {
                    let (_, m, d, h, mi, _) = now();
                    format!("DB{m:02}{d:02}{h:02}{mi:02}")
                }
            };
            let pick = |k: &str, allowed: &[&str], default: &'static str| -> Result<String> {
                match opt(options, k) {
                    None => Ok(default.to_string()),
                    Some(v) if allowed.iter().any(|a| a.eq_ignore_ascii_case(v)) => Ok(v.to_ascii_uppercase()),
                    Some(v) => Err(Error::Query(format!("«{v}» no es un valor válido"))),
                }
            };
            let dtacpr = pick("compress", &["*YES", "*NO", "*LOW", "*MEDIUM", "*HIGH", "*DEV"], "*YES")?;
            let savact = pick("active", &["*NO", "*LIB", "*SYNCLIB", "*SYSDFN"], "*LIB")?;
            Ok(format!(
                "{}\n{}",
                qcmdexc(&format!("CRTSAVF FILE({flib}/{file})")),
                qcmdexc(&format!("SAVLIB LIB({lib}) DEV(*SAVF) SAVF({flib}/{file}) DTACPR({dtacpr}) SAVACT({savact})"))
            ))
        }
        (Engine::Db2i, BackupAction::Restore { source, database, options }) => {
            // `LIB/FILE@SAVEDLIB` (from the history) or `LIB/FILE`.
            let (file, saved) = match source.trim().split_once('@') {
                Some((f, l)) => (f, Some(cl_name(l, "biblioteca")?)),
                None => (source.trim(), None),
            };
            let (flib, f) = savf(file)?;
            let target = database.as_deref().map(str::trim).filter(|d| !d.is_empty()).map(|d| cl_name(d, "biblioteca")).transpose()?;
            let saved = saved.or_else(|| target.clone()).ok_or_else(|| Error::Query("falta la biblioteca que se restaura".into()))?;
            let mut cmd = format!("RSTLIB SAVLIB({saved}) DEV(*SAVF) SAVF({flib}/{f})");
            if let Some(t) = target.filter(|t| *t != saved) {
                cmd.push_str(&format!(" RSTLIB({t})"));
            }
            if on(options, "replace", false) {
                cmd.push_str(" MBROPT(*ALL)");
            }
            Ok(qcmdexc(&cmd))
        }
        (Engine::Db2i, BackupAction::Delete { source }) => {
            let file = source.trim().split_once('@').map_or(source.trim(), |(f, _)| f);
            let (flib, f) = savf(file)?;
            Ok(qcmdexc(&format!("DLTF FILE({flib}/{f})")))
        }
        (Engine::Vertica, BackupAction::Backup { options, .. }) => {
            let archive = vertica_ident(need(options, "archive", "el archivo de puntos de restauración")?);
            let mut sql = String::new();
            if on(options, "create", false) {
                sql.push_str(&format!("CREATE ARCHIVE {archive}"));
                match opt(options, "limit") {
                    Some(n) if n.parse::<u32>().is_ok() => sql.push_str(&format!(" LIMIT {n}")),
                    Some(n) => return Err(Error::Query(format!("«{n}» no es un máximo válido"))),
                    None => {}
                }
                sql.push_str(";\n");
            }
            sql.push_str(&format!("SAVE RESTORE POINT TO ARCHIVE {archive};"));
            Ok(sql)
        }
        (Engine::Vertica, BackupAction::Delete { source }) => {
            // `<archive>#<id>`, as the history lists them.
            let (archive, id) = source
                .trim()
                .rsplit_once('#')
                .filter(|(a, i)| !a.is_empty() && !i.is_empty())
                .ok_or_else(|| Error::Query(format!("«{}» no es un punto de restauración (archivo#id)", source.trim())))?;
            Ok(format!("REMOVE RESTORE POINT FROM ARCHIVE {} ID {};", vertica_ident(archive), lit(id)))
        }
        (Engine::MonetDb, BackupAction::Backup { options, .. }) => {
            Ok(format!("CALL sys.hot_snapshot({});", lit(need(options, "path", "el archivo del servidor")?)))
        }
        (Engine::Virtuoso, BackupAction::Backup { options, .. }) => {
            let prefix = need(options, "prefix", "el prefijo")?;
            let pages = match opt(options, "pages") {
                Some(n) => n.parse::<u64>().ok().filter(|n| *n > 100).ok_or_else(|| Error::Query(format!("«{n}» no es un número de páginas válido (más de 100)")))?,
                None => 100_000,
            };
            let mut sql = String::new();
            if on(options, "new", true) {
                sql.push_str("backup_context_clear();\n");
            }
            match opt(options, "dir") {
                Some(d) => sql.push_str(&format!("backup_online({}, {pages}, 0, vector({}));", lit(prefix), lit(d))),
                None => sql.push_str(&format!("backup_online({}, {pages});", lit(prefix))),
            }
            Ok(sql)
        }
        (Engine::Mimer, BackupAction::Backup { options, .. }) => {
            let bank = need(options, "databank", "el databank")?;
            let dir = need(options, "path", "la carpeta del servidor")?.trim_end_matches(['/', '\\']);
            let (y, m, d, h, mi, sec) = now();
            let stamp = format!("{y:04}{m:02}{d:02}_{h:02}{mi:02}{sec:02}");
            let file = |b: &str| lit(&format!("{dir}/{}_{stamp}.dbk", b.replace(|c: char| !c.is_ascii_alphanumeric() && c != '_', "_")));
            let mut sql = format!("START BACKUP;\nCREATE BACKUP IN {} FOR DATABANK {};\n", file(bank), mimer_ident(bank));
            if on(options, "logdb", true) && !bank.eq_ignore_ascii_case("LOGDB") {
                sql.push_str(&format!("CREATE BACKUP IN {} FOR DATABANK LOGDB;\n", file("LOGDB")));
            }
            sql.push_str("COMMIT BACKUP;");
            Ok(sql)
        }
        (Engine::Dameng, BackupAction::Backup { options, .. }) => {
            let kind = if opt(options, "kind") == Some("increment") { "INCREMENT" } else { "FULL" };
            let path = need(options, "path", "la carpeta del conjunto")?;
            let compress = if on(options, "compress", true) { " COMPRESSED" } else { "" };
            Ok(format!("BACKUP DATABASE {kind} BACKUPSET {}{compress};", lit(path)))
        }
        (Engine::Machbase, BackupAction::Backup { options, .. }) => {
            Ok(format!("BACKUP DATABASE INTO DISK = {};", lit(need(options, "path", "la carpeta del servidor")?)))
        }
        (Engine::Db2 | Engine::SqlAnywhere | Engine::Informix, BackupAction::Restore { .. }) => Err(Error::Unsupported(
            match e {
                Engine::Db2 => "Db2 restaura con el comando RESTORE DATABASE del procesador de línea de comandos, con la base sin uso: ADMIN_CMD no lo ejecuta",
                Engine::SqlAnywhere => "SQL Anywhere restaura copiando los archivos de la imagen con la base detenida, o con RESTORE DATABASE conectado a utility_db",
                _ => "Informix restaura con ontape -r u onbar -r, en general con el servidor detenido: no hay un comando de SQL",
            }
            .into(),
        )),
        (_, BackupAction::Delete { .. }) => Err(Error::Unsupported(
            match e {
                Engine::Db2 => "Db2 no borra un backup suelto por SQL: PRUNE HISTORY recorta el historial por fecha",
                Engine::Ase => "ASE no borra dumps por SQL: son archivos de la máquina del Backup Server",
                Engine::Informix => "Informix no borra backups por SQL: ontape deja archivos en el servidor y ON-Bar los vence con onsmsync",
                Engine::SqlAnywhere => "SQL Anywhere no borra backups por SQL: son archivos del servidor",
                Engine::MonetDb | Engine::Virtuoso | Engine::Mimer | Engine::Machbase => "los backups de este motor son archivos del servidor: no se borran por SQL",
                Engine::Dameng => "DBine no borra conjuntos de backup de Dameng (SF_BAKSET_REMOVE cambia de firma entre versiones)",
                _ => "este motor no borra backups por SQL",
            }
            .into(),
        )),
        (_, BackupAction::Restore { .. }) => Err(Error::Unsupported(
            match e {
                Engine::Vertica => "Vertica revive la base desde un punto de restauración con admintools o el operador de Kubernetes, no por SQL",
                Engine::MonetDb => "MonetDB restaura desempaquetando el snapshot con el servidor detenido",
                Engine::Virtuoso => "Virtuoso restaura con el servidor detenido (virtuoso-t +restore-backup)",
                Engine::Mimer => "Mimer restaura copiando el archivo del backup en lugar del databank y aplicando el log con ALTER DATABANK … RESTORE: la copia no se hace por SQL",
                Engine::Dameng => "Dameng restaura la base con DMRMAN y el servidor detenido",
                Engine::Machbase => "Machbase restaura la base con machadmin -r y el servidor detenido",
                _ => "este motor no restaura backups por SQL",
            }
            .into(),
        )),
    }
}

/// A plain identifier as is, anything else in double quotes.
fn quoted(name: &str, fold_up: bool) -> String {
    let n = name.trim();
    let plain = n.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !(fold_up && n.chars().any(|c| c.is_ascii_lowercase()));
    if plain {
        n.to_string()
    } else {
        format!("\"{}\"", n.replace('"', "\"\""))
    }
}

/// Vertica compares names case-insensitively.
fn vertica_ident(name: &str) -> String {
    quoted(name, false)
}

/// Mimer folds to upper case.
fn mimer_ident(name: &str) -> String {
    quoted(name, true)
}

/// An ASE database name, in brackets.
fn ase_db(db: Option<&str>) -> Result<String> {
    let db = db.map(str::trim).filter(|d| !d.is_empty()).ok_or_else(|| Error::Query("falta la base de datos".into()))?;
    Ok(format!("[{}]", db.replace(']', "]]")))
}

// -- history -------------------------------------------------------------------

type Row = HashMap<String, String>;

fn get<'a>(r: &'a Row, k: &str) -> &'a str {
    r.get(k).map(String::as_str).unwrap_or("").trim()
}

fn some(v: &str) -> Option<String> {
    Some(v.trim().to_string()).filter(|v| !v.is_empty())
}

async fn rows(s: &OdbcSession, sql: &str) -> Result<Vec<Row>> {
    let sql = sql.to_string();
    s.run(move |c, slot| {
        let st = c.stmt(slot)?;
        st.exec(&sql)?;
        let n = st.num_cols()?;
        let names: Vec<String> = st.describe(n)?.into_iter().map(|c| c.name.to_ascii_lowercase()).collect();
        let data = st.text_rows()?;
        Ok(data.into_iter().map(|r| names.iter().cloned().zip(r.into_iter().map(Option::unwrap_or_default)).collect()).collect())
    })
    .await
}

/// `20260929154500` → `2026-09-29T15:45:00`.
fn db2_time(v: &str) -> Option<String> {
    let v = v.trim();
    (v.len() >= 14 && v.chars().take(14).all(|c| c.is_ascii_digit()))
        .then(|| format!("{}-{}-{}T{}:{}:{}", &v[0..4], &v[4..6], &v[6..8], &v[8..10], &v[10..12], &v[12..14]))
}

/// `2026-09-29 15:45:00[.fff]` → `2026-09-29T15:45:00[.fff]`.
fn sql_time(v: &str) -> Option<String> {
    some(v).map(|v| v.replacen(' ', "T", 1))
}

pub(crate) fn db2_entry(r: &Row, database: &str) -> BackupEntry {
    let kind = match get(r, "operationtype") {
        "F" => "Completo (fuera de línea)",
        "N" => "Completo (en línea)",
        "I" => "Incremental (fuera de línea)",
        "O" => "Incremental (en línea)",
        "D" => "Delta (fuera de línea)",
        "E" => "Delta (en línea)",
        other => other,
    };
    let sqlcode: i64 = get(r, "sqlcode").parse().unwrap_or(0);
    let status = match (sqlcode, get(r, "entry_status")) {
        (c, _) if c < 0 => format!("falló (SQL{c})"),
        (_, "E") => "vencido".to_string(),
        (_, "D") => "borrado".to_string(),
        (_, "I") => "inactivo".to_string(),
        _ => "completado".to_string(),
    };
    let mut details = Vec::new();
    if get(r, "objecttype") == "P" {
        details.push(("Alcance".into(), format!("{} espacios de tablas", get(r, "num_tbsps"))));
    }
    let device = match get(r, "devicetype") {
        "D" => "disco",
        "A" => "TSM",
        "X" => "XBSA",
        "F" => "snapshot",
        "T" => "cinta",
        "O" => "otro proveedor",
        "" => "",
        d => d,
    };
    if !device.is_empty() {
        details.push(("Dispositivo".into(), device.to_string()));
    }
    if let Some(c) = some(get(r, "comment")) {
        details.push(("Comentario".into(), c));
    }
    BackupEntry {
        id: get(r, "start_time").to_string(),
        database: some(database),
        kind: some(kind),
        started: db2_time(get(r, "start_time")),
        finished: db2_time(get(r, "end_time")),
        size: None,
        location: some(get(r, "location")),
        status: Some(status),
        details,
        restorable: false,
    }
}

fn informix_bar_entry(r: &Row) -> BackupEntry {
    let kind = match get(r, "act_type") {
        "1" => "ON-Bar",
        "5" => "ON-Bar (todo el sistema)",
        other => other,
    };
    let status = if get(r, "act_status") == "0" { "completado".to_string() } else { format!("falló ({})", get(r, "act_status")) };
    let mut details = Vec::new();
    if let Some(l) = some(get(r, "act_type_level")) {
        details.push(("Nivel".into(), l));
    }
    BackupEntry {
        id: format!("onbar:{}", get(r, "act_aid")),
        database: some(get(r, "obj_name")),
        kind: some(kind),
        started: sql_time(get(r, "act_start")),
        finished: sql_time(get(r, "act_end")),
        status: Some(status),
        details,
        ..Default::default()
    }
}

fn db2i_entry(r: &Row) -> BackupEntry {
    let file = format!("{}/{}", get(r, "save_file_library"), get(r, "save_file"));
    let lib = get(r, "library_name");
    let mut details = Vec::new();
    for (k, label) in [("objects_saved", "Objetos"), ("data_compressed", "Comprimido"), ("save_while_active", "Mientras se usaba"), ("save_command", "Comando")] {
        if let Some(v) = some(get(r, k)) {
            details.push((label.to_string(), v));
        }
    }
    BackupEntry {
        id: if lib.is_empty() { file.clone() } else { format!("{file}@{lib}") },
        database: some(lib),
        kind: Some("SAVLIB".into()),
        started: sql_time(get(r, "save_timestamp")),
        finished: None,
        size: get(r, "objsize").parse().ok(),
        location: Some(file),
        status: Some("completado".into()),
        details,
        restorable: !lib.is_empty(),
    }
}

fn vertica_entry(r: &Row) -> BackupEntry {
    let archive = get(r, "archive");
    let mut details = Vec::new();
    for (k, label) in [("index", "Índice"), ("vertica_version", "Versión")] {
        if let Some(v) = some(get(r, k)) {
            details.push((label.to_string(), v));
        }
    }
    BackupEntry {
        id: format!("{archive}#{}", get(r, "id")),
        database: None,
        kind: Some("Punto de restauración".into()),
        started: sql_time(get(r, "save_time")),
        location: some(archive),
        status: some(&get(r, "state").to_lowercase()),
        details,
        ..Default::default()
    }
}

fn dameng_entry(r: &Row) -> BackupEntry {
    let kind = match (get(r, "type"), get(r, "level")) {
        (_, "1") => "Incremental",
        ("0", _) | ("", _) => "Completo",
        (t, _) => t,
    };
    let path = some(get(r, "backup_path"));
    BackupEntry {
        id: path.clone().unwrap_or_else(|| get(r, "backup_name").to_string()),
        database: some(get(r, "object_name")),
        kind: some(kind),
        started: sql_time(get(r, "backup_time")),
        location: path,
        status: Some("completado".into()),
        details: some(get(r, "backup_name")).map(|n| vec![("Nombre".to_string(), n)]).unwrap_or_default(),
        ..Default::default()
    }
}

pub async fn history(s: &OdbcSession, p: &Preset, database: Option<&str>) -> Result<Vec<BackupEntry>> {
    let Some(e) = engine(p) else { return Err(Error::Unsupported(unsupported(p).into())) };
    match e {
        Engine::Db2 => {
            let current = database.map(str::to_string).unwrap_or_else(|| s.database.clone());
            let rs = rows(
                s,
                "SELECT START_TIME, END_TIME, OPERATIONTYPE, OBJECTTYPE, NUM_TBSPS, LOCATION, DEVICETYPE, SQLCODE, ENTRY_STATUS, COMMENT
   FROM SYSIBMADM.DB_HISTORY WHERE OPERATION = 'B' ORDER BY START_TIME DESC FETCH FIRST 500 ROWS ONLY",
            )
            .await?;
            Ok(rs.iter().map(|r| db2_entry(r, &current)).collect())
        }
        Engine::Informix => {
            let mut out: Vec<BackupEntry> = rows(
                s,
                "SELECT FIRST 500 a.act_aid, a.act_type, a.act_status, a.act_start, a.act_end, o.obj_name
   FROM sysutils:bar_action a JOIN sysutils:bar_object o ON o.obj_oid = a.act_oid
  WHERE a.act_type IN (1, 5) ORDER BY a.act_start DESC",
            )
            .await
            .unwrap_or_default()
            .iter()
            .map(informix_bar_entry)
            .collect();
            // ontape keeps no history: the last level 0 archive of each dbspace.
            for r in rows(
                s,
                "SELECT d.name, DBINFO('utc_to_datetime', t.level0) AS level0 FROM sysmaster:sysdbstab t
   JOIN sysmaster:sysdbspaces d ON d.dbsnum = t.dbsnum WHERE t.level0 > 0",
            )
            .await
            .unwrap_or_default()
            {
                out.push(BackupEntry {
                    id: format!("level0:{}", get(&r, "name")),
                    database: some(get(&r, "name")),
                    kind: Some("Último nivel 0 del dbspace".into()),
                    started: sql_time(get(&r, "level0")),
                    status: Some("completado".into()),
                    ..Default::default()
                });
            }
            Ok(out)
        }
        Engine::Db2i => {
            let rs = rows(
                s,
                "SELECT i.*, o.OBJSIZE FROM QSYS2.SAVE_FILE_INFO i
   LEFT JOIN TABLE(QSYS2.OBJECT_STATISTICS(i.SAVE_FILE_LIBRARY, '*FILE', i.SAVE_FILE)) o ON 1 = 1
  WHERE i.SAVE_COMMAND = 'SAVLIB' ORDER BY i.SAVE_TIMESTAMP DESC FETCH FIRST 500 ROWS ONLY",
            )
            .await?;
            Ok(rs.iter().map(db2i_entry).collect())
        }
        Engine::Vertica => {
            let rs = rows(s, "SELECT * FROM ARCHIVE_RESTORE_POINTS ORDER BY save_time DESC LIMIT 500").await?;
            Ok(rs.iter().map(vertica_entry).collect())
        }
        Engine::Mimer => {
            let rs = rows(s, "SELECT * FROM INFORMATION_SCHEMA.EXT_DATABANKS WHERE BACKUP_DATE IS NOT NULL ORDER BY BACKUP_DATE DESC").await?;
            Ok(rs
                .iter()
                .map(|r| BackupEntry {
                    id: get(r, "databank_name").to_string(),
                    database: some(get(r, "databank_name")),
                    kind: Some("Último backup del databank".into()),
                    started: sql_time(get(r, "backup_date")),
                    location: some(get(r, "file_name")),
                    status: Some("completado".into()),
                    ..Default::default()
                })
                .collect())
        }
        Engine::Dameng => {
            let rs = rows(s, "SELECT * FROM V$BACKUPSET").await?;
            let mut out: Vec<BackupEntry> = rs.iter().map(dameng_entry).collect();
            out.sort_by(|a, b| b.started.cmp(&a.started));
            Ok(out)
        }
        Engine::Ase | Engine::SqlAnywhere | Engine::MonetDb | Engine::Virtuoso | Engine::Machbase => {
            Err(Error::Unsupported("este motor no lista sus backups por SQL".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preset(id: &str) -> &'static Preset {
        crate::PRESETS.iter().find(|p| p.id == id).unwrap()
    }

    fn opts(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn backup(id: &str, db: Option<&str>, kv: &[(&str, &str)]) -> Result<String> {
        script(preset(id), &BackupAction::Backup { database: db.map(Into::into), options: opts(kv) })
    }

    fn restore(id: &str, source: &str, db: Option<&str>, kv: &[(&str, &str)]) -> Result<String> {
        script(preset(id), &BackupAction::Restore { source: source.into(), database: db.map(Into::into), options: opts(kv) })
    }

    fn delete(id: &str, source: &str) -> Result<String> {
        script(preset(id), &BackupAction::Delete { source: source.into() })
    }

    #[test]
    fn every_preset_has_backups_or_a_reason() {
        let with: Vec<&str> = crate::PRESETS.iter().filter(|p| spec(p).is_some()).map(|p| p.id).collect();
        assert_eq!(with, vec!["db2", "db2i", "sybase", "sqlanywhere", "informix", "vertica", "dameng", "gbase8s", "monetdb", "virtuoso", "mimer", "machbase"]);
        let generic = unsupported(preset("db2"));
        for p in crate::PRESETS.iter().filter(|p| spec(p).is_none()) {
            assert_ne!(unsupported(p), generic, "{} needs its own reason", p.id);
            assert!(matches!(script(p, &BackupAction::Delete { source: "x".into() }), Err(Error::Unsupported(_))));
        }
        for p in crate::PRESETS.iter().filter_map(|p| spec(p).map(|s| (p, s))) {
            let (p, s) = p;
            assert!(!s.note.is_empty(), "{}", p.id);
            if !s.restore {
                assert!(matches!(restore(p.id, "x", Some("y"), &[]), Err(Error::Unsupported(_))), "{}", p.id);
            }
            if !s.delete {
                assert!(matches!(delete(p.id, "x"), Err(Error::Unsupported(_))), "{}", p.id);
            }
        }
    }

    #[test]
    fn db2() {
        assert_eq!(
            backup("db2", Some("SAMPLE"), &[("path", "/db2/bk")]).unwrap(),
            "CALL SYSPROC.ADMIN_CMD('BACKUP DATABASE SAMPLE ONLINE TO /db2/bk COMPRESS INCLUDE LOGS');"
        );
        assert_eq!(
            backup("db2", None, &[("path", "/b"), ("kind", "delta"), ("compress", "false"), ("logs", "false")]).unwrap(),
            "CALL SYSPROC.ADMIN_CMD('BACKUP DATABASE ' || CURRENT SERVER || ' ONLINE INCREMENTAL DELTA TO /b EXCLUDE LOGS');"
        );
        assert!(backup("db2", Some("SAMPLE"), &[]).is_err());
        assert!(backup("db2", Some("x'); DROP"), &[("path", "/b")]).is_err());
        let e = db2_entry(
            &[("start_time", "20260929154500"), ("end_time", "20260929154610"), ("operationtype", "N"), ("location", "/db2/bk"), ("devicetype", "D"), ("sqlcode", "")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            "SAMPLE",
        );
        assert_eq!(e.id, "20260929154500");
        assert_eq!(e.started.as_deref(), Some("2026-09-29T15:45:00"));
        assert_eq!(e.kind.as_deref(), Some("Completo (en línea)"));
        assert_eq!(e.status.as_deref(), Some("completado"));
        assert!(!e.restorable);
    }

    #[test]
    fn ase_and_sql_anywhere() {
        assert_eq!(backup("sybase", Some("ventas"), &[("path", "/d/v'1.dmp"), ("compression", "101")]).unwrap(), "DUMP DATABASE [ventas] TO '/d/v''1.dmp' WITH COMPRESSION = 101");
        assert!(backup("sybase", Some("ventas"), &[("path", "/d"), ("compression", "1; x")]).is_err());
        assert!(backup("sybase", None, &[("path", "/d")]).is_err());
        assert_eq!(restore("sybase", "/d/v.dmp", Some("ventas2"), &[]).unwrap(), "LOAD DATABASE [ventas2] FROM '/d/v.dmp'\ngo\nONLINE DATABASE [ventas2]");
        assert_eq!(crate::split_go(&restore("sybase", "/d/v.dmp", Some("v"), &[]).unwrap()).unwrap().len(), 2);
        assert_eq!(spec(preset("sybase")).unwrap().script_database, "master");
        assert_eq!(
            backup("sqlanywhere", None, &[("path", "/bk"), ("log", "rename"), ("comment", "a'b")]).unwrap(),
            "BACKUP DATABASE DIRECTORY '/bk' TRANSACTION LOG RENAME WITH COMMENT 'a''b';"
        );
        assert_eq!(backup("sqlanywhere", None, &[("path", "/bk/v"), ("mode", "archive"), ("log", "rename")]).unwrap(), "BACKUP DATABASE TO '/bk/v';");
    }

    #[test]
    fn informix() {
        assert_eq!(backup("informix", None, &[("path", "/bk")]).unwrap(), "EXECUTE FUNCTION task('ontape archive directory level 0', '/bk/');");
        assert_eq!(backup("gbase8s", None, &[("tool", "onbar"), ("level", "1")]).unwrap(), "EXECUTE FUNCTION task('onbar backup whole system level 1');");
        assert!(backup("informix", None, &[("path", "/bk"), ("level", "3")]).is_err());
        assert_eq!(spec(preset("informix")).unwrap().script_database, "sysadmin");
    }

    #[test]
    fn ibm_i() {
        assert_eq!(
            backup("db2i", None, &[("library", "ventas"), ("savf", "bk1"), ("compress", "*high"), ("active", "*SYNCLIB")]).unwrap(),
            "CALL QSYS2.QCMDEXC('CRTSAVF FILE(QGPL/BK1)');\nCALL QSYS2.QCMDEXC('SAVLIB LIB(VENTAS) DEV(*SAVF) SAVF(QGPL/BK1) DTACPR(*HIGH) SAVACT(*SYNCLIB)');"
        );
        assert!(backup("db2i", None, &[("library", "VENTAS"), ("savf", "X) DLTLIB(Y")]).is_err());
        assert!(backup("db2i", None, &[("library", "TOOLONGNAME1")]).is_err());
        let auto = backup("db2i", None, &[("library", "VENTAS")]).unwrap();
        assert!(auto.contains("SAVF(QGPL/DB"), "{auto}");
        assert_eq!(restore("db2i", "QGPL/BK1@VENTAS", None, &[]).unwrap(), "CALL QSYS2.QCMDEXC('RSTLIB SAVLIB(VENTAS) DEV(*SAVF) SAVF(QGPL/BK1)');");
        assert_eq!(
            restore("db2i", "QGPL/BK1@VENTAS", Some("ventas2"), &[("replace", "true")]).unwrap(),
            "CALL QSYS2.QCMDEXC('RSTLIB SAVLIB(VENTAS) DEV(*SAVF) SAVF(QGPL/BK1) RSTLIB(VENTAS2) MBROPT(*ALL)');"
        );
        assert_eq!(restore("db2i", "QGPL/BK1", Some("VENTAS"), &[]).unwrap(), "CALL QSYS2.QCMDEXC('RSTLIB SAVLIB(VENTAS) DEV(*SAVF) SAVF(QGPL/BK1)');");
        assert!(restore("db2i", "QGPL/BK1", None, &[]).is_err());
        assert_eq!(delete("db2i", "QGPL/BK1@VENTAS").unwrap(), "CALL QSYS2.QCMDEXC('DLTF FILE(QGPL/BK1)');");
        assert!(delete("db2i", "BK1").is_err());
        let e = db2i_entry(
            &[("save_file_library", "QGPL"), ("save_file", "BK1"), ("library_name", "VENTAS"), ("save_timestamp", "2026-09-29 15:45:00.000000"), ("objsize", "1048576")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        );
        assert_eq!((e.id.as_str(), e.size, e.restorable), ("QGPL/BK1@VENTAS", Some(1_048_576), true));
        assert_eq!(e.started.as_deref(), Some("2026-09-29T15:45:00.000000"));
    }

    /// What `Session::execute` sends, one call per element, with the
    /// preset's default batch mode.
    fn sent(id: &str, script: &str) -> Vec<String> {
        crate::split(preset(id).batch, script).into_iter().map(|s| s.trim().to_string()).collect()
    }

    #[test]
    fn multi_statement_scripts_split_as_execute_sends_them() {
        assert_eq!(
            sent("sybase", &restore("sybase", "/d/v.dmp", Some("v"), &[]).unwrap()),
            vec!["LOAD DATABASE [v] FROM '/d/v.dmp'", "ONLINE DATABASE [v]"]
        );
        assert_eq!(sent("sybase", &backup("sybase", Some("v"), &[("path", "/d/v.dmp")]).unwrap()), vec!["DUMP DATABASE [v] TO '/d/v.dmp'"]);
        let i = sent("db2i", &backup("db2i", None, &[("library", "VENTAS"), ("savf", "BK1")]).unwrap());
        assert_eq!(i.len(), 2);
        assert!(i[0].starts_with("CALL QSYS2.QCMDEXC('CRTSAVF") && i[1].starts_with("CALL QSYS2.QCMDEXC('SAVLIB"), "{i:?}");
        let m = sent("mimer", &backup("mimer", None, &[("databank", "V"), ("path", "/bk")]).unwrap());
        assert_eq!(m.len(), 4, "{m:?}");
        assert_eq!((m[0].as_str(), m[3].as_str()), ("START BACKUP", "COMMIT BACKUP"));
        assert!(m[1].starts_with("CREATE BACKUP IN '/bk/V_") && m[2].ends_with("FOR DATABANK LOGDB"), "{m:?}");
        assert_eq!(
            sent("virtuoso", &backup("virtuoso", None, &[("prefix", "p")]).unwrap()),
            vec!["backup_context_clear()", "backup_online('p', 100000)"]
        );
        assert_eq!(
            sent("vertica", &backup("vertica", None, &[("archive", "a"), ("create", "true")]).unwrap()),
            vec!["CREATE ARCHIVE a", "SAVE RESTORE POINT TO ARCHIVE a"]
        );
        for (id, kv) in [("db2", vec![("path", "/b")]), ("informix", vec![("path", "/b")]), ("sqlanywhere", vec![("path", "/b")]), ("dameng", vec![("path", "/b")])] {
            assert_eq!(sent(id, &backup(id, Some("X"), &kv).unwrap()).len(), 1, "{id}");
        }
    }

    #[test]
    fn others() {
        assert_eq!(
            backup("vertica", None, &[("archive", "diario"), ("create", "true"), ("limit", "7")]).unwrap(),
            "CREATE ARCHIVE diario LIMIT 7;\nSAVE RESTORE POINT TO ARCHIVE diario;"
        );
        assert_eq!(backup("vertica", None, &[("archive", "Mi Archivo")]).unwrap(), "SAVE RESTORE POINT TO ARCHIVE \"Mi Archivo\";");
        assert_eq!(delete("vertica", "diario#a1b2").unwrap(), "REMOVE RESTORE POINT FROM ARCHIVE diario ID 'a1b2';");
        assert!(delete("vertica", "diario").is_err());
        let e = vertica_entry(&[("id", "a1b2"), ("archive", "diario"), ("index", "1"), ("state", "COMPLETE"), ("save_time", "2026-09-29 15:45:00")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect());
        assert_eq!((e.id.as_str(), e.status.as_deref()), ("diario#a1b2", Some("complete")));
        assert_eq!(backup("monetdb", None, &[("path", "/bk/v.tar.gz")]).unwrap(), "CALL sys.hot_snapshot('/bk/v.tar.gz');");
        assert_eq!(
            backup("virtuoso", None, &[("prefix", "dbine_"), ("dir", "/bk"), ("pages", "5000")]).unwrap(),
            "backup_context_clear();\nbackup_online('dbine_', 5000, 0, vector('/bk'));"
        );
        assert_eq!(backup("virtuoso", None, &[("prefix", "p"), ("new", "false")]).unwrap(), "backup_online('p', 100000);");
        assert!(backup("virtuoso", None, &[("prefix", "p"), ("pages", "10")]).is_err());
        let m = backup("mimer", None, &[("databank", "ventas"), ("path", "/bk/")]).unwrap();
        assert!(m.starts_with("START BACKUP;\nCREATE BACKUP IN '/bk/ventas_2"), "{m}");
        assert!(m.contains("FOR DATABANK \"ventas\";\nCREATE BACKUP IN '/bk/LOGDB_") && m.ends_with("FOR DATABANK LOGDB;\nCOMMIT BACKUP;"), "{m}");
        assert!(!backup("mimer", None, &[("databank", "V"), ("path", "/bk"), ("logdb", "false")]).unwrap().contains("LOGDB"));
        assert_eq!(backup("dameng", None, &[("path", "/dm/bk/f1")]).unwrap(), "BACKUP DATABASE FULL BACKUPSET '/dm/bk/f1' COMPRESSED;");
        assert_eq!(backup("dameng", None, &[("path", "/p"), ("kind", "increment"), ("compress", "false")]).unwrap(), "BACKUP DATABASE INCREMENT BACKUPSET '/p';");
        assert_eq!(backup("machbase", None, &[("path", "/bk/1")]).unwrap(), "BACKUP DATABASE INTO DISK = '/bk/1';");
    }
}
