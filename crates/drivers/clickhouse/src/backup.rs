//! The server's own backups (docs/backups.md): `BACKUP DATABASE … TO
//! Disk(…) | File(…) | S3(…)` and `RESTORE DATABASE … FROM …`, with the
//! history from `system.backups` (in memory: it starts empty on every
//! restart). ClickHouse has no SQL to delete a backup.
//!
//! Timeplus Proton parses `BACKUP`, but (3.0.31) writes empty metadata
//! files that its `RESTORE` can't read: it gets no native backups.

use crate::schema::{q, string_literal as lit};
use crate::{text, ClickHouseSession, Flavor};
use dbine_driver::{BackupAction, BackupEntry, BackupSpec, Error, Field, FieldKind, Result};
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) fn spec(flavor: Flavor) -> Option<BackupSpec> {
    if flavor != Flavor::ClickHouse {
        return None;
    }
    Some(BackupSpec {
        backup_options: vec![
            Field::new(
                "destination",
                "Destino",
                FieldKind::Select(vec![
                    ("disk", "Disco de backups del servidor"),
                    ("file", "Ruta del servidor"),
                    ("s3", "S3 (o compatible)"),
                ]),
            )
            .default_value("disk"),
            Field::new("disk", "Disco", FieldKind::Text)
                .default_value("backups")
                .help("Un disco listado en backups.allowed_disk de la configuración del servidor.")
                .when("destination", &["disk"]),
            Field::new("name", "Nombre", FieldKind::Text)
                .placeholder("(base-fecha.zip)")
                .help("Con .zip o .tar queda en un solo archivo; terminado en / queda como carpeta. En Ruta del servidor, dentro de backups.allowed_path.")
                .when("destination", &["disk", "file"]),
            Field::new("s3_url", "URL de S3", FieldKind::Text)
                .placeholder("https://mi-bucket.s3.amazonaws.com/backups/ventas.zip")
                .when("destination", &["s3"]),
            Field::new("s3_access_key", "Access key", FieldKind::Text)
                .help("Vacío: las credenciales que ya tiene el servidor.")
                .when("destination", &["s3"]),
            Field::new("s3_secret_key", "Secret key", FieldKind::Password).when("destination", &["s3"]),
            Field::new("base_backup", "Incremental sobre", FieldKind::Text)
                .placeholder("Disk('backups', 'ventas-full.zip')")
                .help("Vacío: backup completo. Si no, solo guarda lo que cambió desde ese backup."),
            Field::new("async", "En segundo plano (ASYNC)", FieldKind::Bool)
                .default_value("false")
                .help("Vuelve enseguida; el avance se ve en el historial."),
        ],
        restore: true,
        restore_options: vec![
            Field::new("from_database", "Base dentro del backup", FieldKind::Text)
                .placeholder("(la de destino)")
                .help("Completala para restaurar la base con otro nombre."),
            Field::new("disk", "Disco", FieldKind::Text)
                .default_value("backups")
                .help("Si el origen es solo un nombre de archivo, el disco donde está."),
            Field::new("allow_non_empty_tables", "Agregar a tablas que ya tienen datos", FieldKind::Bool)
                .default_value("false")
                .help("Si no, falla cuando una tabla ya existe con filas."),
            Field::new("async", "En segundo plano (ASYNC)", FieldKind::Bool).default_value("false"),
        ],
        delete: false,
        history: true,
        server_wide: false,
        script_database: "",
        note: "Los backups quedan donde los escribe el servidor: en un disco declarado en backups.allowed_disk, \
               en una ruta de backups.allowed_path o en S3. Sin esa configuración, el servidor rechaza el BACKUP. \
               El historial (system.backups) se vacía cuando el servidor se reinicia, y ClickHouse no borra \
               backups por SQL: se borran del disco o del bucket.",
    })
}

fn opt<'a>(o: &'a BTreeMap<String, String>, k: &str) -> Option<&'a str> {
    o.get(k).map(|s| s.trim()).filter(|s| !s.is_empty())
}

fn flag(o: &BTreeMap<String, String>, k: &str) -> bool {
    opt(o, k) == Some("true")
}

/// A location as the server writes it in `system.backups.name`
/// (`Disk('backups', 'x.zip')`, `File('…')`, `S3('…', …)`): one call,
/// its quotes and parentheses balanced, nothing after it.
fn is_location(s: &str) -> bool {
    let Some(open) = s.find('(') else { return false };
    if !matches!(&s[..open], "Disk" | "File" | "S3" | "AzureBlobStorage") {
        return false;
    }
    let (mut depth, mut quoted, mut escaped) = (0usize, false, false);
    for (i, c) in s.char_indices().skip_while(|(i, _)| *i < open) {
        if quoted {
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '\'' => quoted = false,
                _ => {}
            }
            continue;
        }
        match c {
            '\'' => quoted = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return i == s.len() - 1;
                }
            }
            ';' => return false,
            _ => {}
        }
    }
    false
}

fn location(o: &BTreeMap<String, String>, database: &str) -> Result<String> {
    match opt(o, "destination").unwrap_or("disk") {
        "s3" => {
            let url = opt(o, "s3_url").ok_or_else(|| Error::Query("falta la URL de S3".into()))?;
            Ok(match (opt(o, "s3_access_key"), opt(o, "s3_secret_key")) {
                (Some(k), Some(s)) => format!("S3({}, {}, {})", lit(url), lit(k), lit(s)),
                _ => format!("S3({})", lit(url)),
            })
        }
        dest => {
            let name = opt(o, "name").map(str::to_string).unwrap_or_else(|| default_name(database));
            if dest == "file" {
                Ok(format!("File({})", lit(&name)))
            } else {
                Ok(format!("Disk({}, {})", lit(opt(o, "disk").unwrap_or("backups")), lit(&name)))
            }
        }
    }
}

/// `ventas-20260929-154500.zip` (UTC).
fn default_name(database: &str) -> String {
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
    let safe: String = database.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    format!("{safe}-{y:04}{m:02}{d:02}-{:02}{:02}{:02}.zip", rest / 3600, rest % 3600 / 60, rest % 60)
}

pub(crate) fn script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { database, options } => {
            let db = database.as_deref().filter(|d| !d.is_empty()).ok_or_else(|| Error::Query("falta la base".into()))?;
            let mut sql = format!("BACKUP DATABASE {} TO {}", q(db), location(options, db)?);
            if let Some(base) = opt(options, "base_backup") {
                if !is_location(base) {
                    return Err(Error::Query(format!(
                        "«{base}» no es un backup: tiene que ser como Disk('backups', 'archivo.zip')"
                    )));
                }
                sql.push_str(&format!(" SETTINGS base_backup = {base}"));
            }
            if flag(options, "async") {
                sql.push_str(" ASYNC");
            }
            Ok(sql + ";")
        }
        BackupAction::Restore { source, database, options } => {
            let source = source.trim();
            if source.is_empty() {
                return Err(Error::Query("falta el backup a restaurar".into()));
            }
            let from = if is_location(source) {
                source.to_string()
            } else {
                format!("Disk({}, {})", lit(opt(options, "disk").unwrap_or("backups")), lit(source))
            };
            let what = match database.as_deref().filter(|d| !d.is_empty()) {
                Some(db) => match opt(options, "from_database").filter(|f| *f != db) {
                    Some(src) => format!("DATABASE {} AS {}", q(src), q(db)),
                    None => format!("DATABASE {}", q(db)),
                },
                None => "ALL".to_string(),
            };
            let mut sql = format!("RESTORE {what} FROM {from}");
            if flag(options, "allow_non_empty_tables") {
                sql.push_str(" SETTINGS allow_non_empty_tables = true");
            }
            if flag(options, "async") {
                sql.push_str(" ASYNC");
            }
            Ok(sql + ";")
        }
        BackupAction::Delete { .. } => {
            Err(Error::Unsupported("ClickHouse no borra backups por SQL: se borran del disco o del bucket".into()))
        }
    }
}

const HISTORY: &str = "SELECT b.id, b.name, b.base_backup_name, toString(b.status), b.error,
       toString(b.start_time), toString(b.end_time), b.num_files, b.total_size, b.compressed_size, l.query
  FROM system.backups AS b
  LEFT JOIN (SELECT query_id, any(query) AS query FROM system.query_log
              WHERE query_kind IN ('Backup', 'Restore') AND type = 'QueryStart'
                AND event_date >= toDate((SELECT min(start_time) FROM system.backups))
              GROUP BY query_id) AS l USING (query_id)
 ORDER BY b.start_time DESC";

/// Without `system.query_log` (disabled in the server's configuration).
const HISTORY_PLAIN: &str = "SELECT id, name, base_backup_name, toString(status), error,
       toString(start_time), toString(end_time), num_files, total_size, compressed_size, ''
  FROM system.backups ORDER BY start_time DESC";

/// What a `BACKUP` / `RESTORE` statement is of: (`DATABASE`, `TABLE`,
/// `ALL`…, the database when it names one).
fn target(query: &str) -> (Option<String>, Option<String>) {
    let mut words = query.split_whitespace().skip(1);
    let Some(what) = words.next().map(str::to_uppercase) else { return (None, None) };
    let db = if what == "DATABASE" {
        let rest = query.trim_start();
        let rest = rest[rest.to_uppercase().find("DATABASE").map_or(0, |i| i + 8)..].trim_start();
        if let Some(r) = rest.strip_prefix('`') {
            let mut name = String::new();
            let mut chars = r.chars().peekable();
            while let Some(c) = chars.next() {
                match c {
                    '`' if chars.peek() == Some(&'`') => {
                        chars.next();
                        name.push('`');
                    }
                    '`' => break,
                    '\\' => name.extend(chars.next()),
                    c => name.push(c),
                }
            }
            Some(name)
        } else {
            rest.split(|c: char| c.is_whitespace() || c == ';').next().filter(|s| !s.is_empty()).map(str::to_string)
        }
    } else {
        None
    };
    (Some(what), db)
}

fn when(v: &Value) -> Option<String> {
    let s = text(v);
    (!s.is_empty() && !s.starts_with("1970-01-01")).then(|| s.replacen(' ', "T", 1))
}

fn number(v: &Value) -> u64 {
    text(v).parse().unwrap_or(0)
}

fn entry(r: &[Value]) -> BackupEntry {
    let status = text(&r[3]);
    let restore = status.starts_with("RESTOR");
    let (what, database) = target(&text(&r[10]));
    let base = text(&r[2]);
    let kind = if restore {
        "Restauración"
    } else if !base.is_empty() {
        "Incremental"
    } else {
        "Completo"
    };
    let mut details = vec![("Id".to_string(), text(&r[0]))];
    if let Some(w) = what {
        details.push(("Alcance".into(), w));
    }
    if !base.is_empty() {
        details.push(("Sobre el backup".into(), base));
    }
    let files = number(&r[7]);
    if files > 0 {
        details.push(("Archivos".into(), files.to_string()));
    }
    let error = text(&r[4]);
    if !error.is_empty() {
        details.push(("Error".into(), error));
    }
    let (total, compressed) = (number(&r[8]), number(&r[9]));
    BackupEntry {
        id: text(&r[1]),
        database,
        kind: Some(kind.into()),
        started: when(&r[5]),
        finished: when(&r[6]),
        size: [compressed, total].into_iter().find(|n| *n > 0),
        location: Some(text(&r[1])),
        restorable: status == "BACKUP_CREATED",
        status: Some(status),
        details,
    }
}

impl ClickHouseSession {
    /// Of `database` (and those whose statement is no longer in the query
    /// log, which can't be told apart), newest first.
    pub(crate) async fn backup_history(&self, database: Option<&str>) -> Result<Vec<BackupEntry>> {
        let rows = match self.rows(HISTORY, &[]).await {
            Ok(r) => r,
            Err(_) => self.rows(HISTORY_PLAIN, &[]).await?,
        };
        Ok(rows
            .iter()
            .filter(|r| r.len() >= 11)
            .map(|r| entry(r))
            .filter(|e| database.is_none() || e.database.is_none() || e.database.as_deref() == database)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(kv: &[(&str, &str)]) -> BTreeMap<String, String> {
        kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn writes_the_scripts() {
        let b = |db: &str, kv: &[(&str, &str)]| {
            script(&BackupAction::Backup { database: Some(db.into()), options: opts(kv) }).unwrap()
        };
        assert_eq!(
            b("ven`tas", &[("disk", "backups"), ("name", "o'k.zip")]),
            "BACKUP DATABASE `ven``tas` TO Disk('backups', 'o\\'k.zip');"
        );
        assert_eq!(
            b("v", &[("destination", "file"), ("name", "/b/v/"), ("async", "true")]),
            "BACKUP DATABASE `v` TO File('/b/v/') ASYNC;"
        );
        assert_eq!(
            b("v", &[("destination", "s3"), ("s3_url", "https://x/b.zip"), ("s3_access_key", "K"), ("s3_secret_key", "S")]),
            "BACKUP DATABASE `v` TO S3('https://x/b.zip', 'K', 'S');"
        );
        assert_eq!(
            b("v", &[("name", "i.zip"), ("base_backup", "Disk('backups', 'f.zip')")]),
            "BACKUP DATABASE `v` TO Disk('backups', 'i.zip') SETTINGS base_backup = Disk('backups', 'f.zip');"
        );
        let auto = b("mi base", &[]);
        assert!(auto.starts_with("BACKUP DATABASE `mi base` TO Disk('backups', 'mi_base-2"), "{auto}");
        assert!(script(&BackupAction::Backup {
            database: Some("v".into()),
            options: opts(&[("base_backup", "Disk('a'); DROP DATABASE v; SELECT ('')")])
        })
        .is_err());

        let r = |src: &str, db: Option<&str>, kv: &[(&str, &str)]| {
            script(&BackupAction::Restore { source: src.into(), database: db.map(Into::into), options: opts(kv) }).unwrap()
        };
        assert_eq!(r("Disk('backups', 'v.zip')", Some("v"), &[]), "RESTORE DATABASE `v` FROM Disk('backups', 'v.zip');");
        assert_eq!(
            r("v.zip", Some("v2"), &[("from_database", "v"), ("allow_non_empty_tables", "true")]),
            "RESTORE DATABASE `v` AS `v2` FROM Disk('backups', 'v.zip') SETTINGS allow_non_empty_tables = true;"
        );
        assert_eq!(r("File('/b/x/')", None, &[("async", "true")]), "RESTORE ALL FROM File('/b/x/') ASYNC;");
        // Not a location: taken as a file name, quoted.
        assert_eq!(r("Disk('a'); DROP", Some("v"), &[]), "RESTORE DATABASE `v` FROM Disk('backups', 'Disk(\\'a\\'); DROP');");
        assert!(script(&BackupAction::Delete { source: "x".into() }).is_err());
        assert!(spec(Flavor::Timeplus).is_none());
        assert!(spec(Flavor::ClickHouse).is_some_and(|s| s.restore && s.history && !s.delete));
    }

    #[test]
    fn reads_what_a_statement_is_of() {
        assert_eq!(target("BACKUP DATABASE `a``b` TO Disk('x', 'y')"), (Some("DATABASE".into()), Some("a`b".into())));
        assert_eq!(target("restore database v AS w FROM File('x')"), (Some("DATABASE".into()), Some("v".into())));
        assert_eq!(target("BACKUP TABLE v.t TO File('x')"), (Some("TABLE".into()), None));
        assert_eq!(target(""), (None, None));
        assert!(is_location("S3('https://x/a.zip', 'k', 's\\')')"));
        assert!(!is_location("Disk('a') x"));
        assert!(!is_location("Shell('a')"));
    }
}
