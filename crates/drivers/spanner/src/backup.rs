//! Native backups (docs/backups.md): Spanner's backups, made, restored and
//! deleted through the Database Admin API (CreateBackup, RestoreDatabase,
//! DeleteBackup) and listed with ListBackups. GoogleSQL has no statements
//! for them, so DBine accepts three of its own, run by the driver like any
//! other DDL:
//!
//! ```sql
//! CREATE BACKUP `ventas-20260929` FROM DATABASE `ventas` EXPIRE AFTER 7 DAYS [AS OF '2026-09-29T10:00:00Z'];
//! RESTORE DATABASE `ventas_copia` FROM BACKUP `ventas-20260929`;
//! DROP BACKUP `ventas-20260929`;
//! ```
//!
//! Backups live in the connection's instance. Creating and restoring are
//! long-running operations: the statement returns once Spanner accepts it
//! and the history shows the backup's state.

use crate::{bq, lit, monitor::rfc3339, SpannerSession};
use dbine_driver::{BackupAction, BackupEntry, BackupSpec, Error, Field, FieldKind, QueryOutcome, Result};
use serde_json::{json, Value as Json};
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![
            Field::new("backup_id", "ID del backup", FieldKind::Text)
                .placeholder("vacío = <base>-<fecha y hora>")
                .help("Minúsculas, números, - y _; empieza con letra y tiene de 2 a 60 caracteres."),
            Field::new("expire_days", "Vence a los (días)", FieldKind::Number)
                .default_value("7")
                .help("Spanner lo borra al vencer. Entre 1 y 366 días."),
            Field::new("version_time", "Versión de los datos", FieldKind::Text)
                .placeholder("vacío = ahora (2026-09-29T10:00:00Z)")
                .help("Un momento pasado dentro del período de retención de versiones de la base (1 hora por defecto)."),
        ],
        restore: true,
        restore_options: Vec::new(),
        delete: true,
        history: true,
        server_wide: false,
        script_database: "",
        note: "Los backups quedan en la instancia de la conexión (no en esta máquina) y se restauran en una base nueva de la misma instancia. Crear y restaurar son operaciones largas: el script vuelve enseguida y el historial muestra el estado. El emulador de Spanner no tiene backups.",
    }
}

fn opt<'a>(options: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    options.get(key).map(|v| v.trim()).unwrap_or_default()
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// A backup or database id: `[a-z][a-z0-9_-]*[a-z0-9]`, 2 to 60 characters
/// (30 for a database, which Spanner checks itself).
fn check_id(id: &str, what: &str) -> Result<()> {
    let ok = (2..=60).contains(&id.len())
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && id.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(Error::Query(format!("'{id}' no es un ID de {what} válido: minúsculas, números, - y _, empezando con letra")))
    }
}

/// `<database>-YYYYMMDD-HHMMSS` (UTC), cut to 60 characters.
fn default_id(database: &str, secs: u64) -> String {
    let t = rfc3339(secs);
    let stamp = format!("{}-{}", t[..10].replace('-', ""), t[11..19].replace(':', ""));
    let head: String = database.chars().take(60 - stamp.len() - 1).collect();
    format!("{}-{stamp}", head.trim_end_matches(['-', '_']))
}

pub fn script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { database, options } => {
            let db = database.as_deref().map(str::trim).filter(|d| !d.is_empty()).ok_or_else(|| Error::Query("Falta la base".into()))?;
            let id = match opt(options, "backup_id") {
                "" => default_id(db, now()),
                id => id.to_string(),
            };
            check_id(&id, "backup")?;
            let days: u32 = match opt(options, "expire_days") {
                "" => 7,
                d => d.parse().ok().filter(|d| (1..=366).contains(d)).ok_or_else(|| Error::Query(format!("'{d}' no es una cantidad de días entre 1 y 366")))?,
            };
            let mut s = format!("CREATE BACKUP {} FROM DATABASE {} EXPIRE AFTER {days} DAYS", bq(&id), bq(db));
            let version = opt(options, "version_time");
            if !version.is_empty() {
                s.push_str(&format!(" AS OF {}", lit(version)));
            }
            s.push(';');
            Ok(s)
        }
        BackupAction::Restore { source, database, .. } => {
            let target = database.as_deref().map(str::trim).filter(|d| !d.is_empty()).ok_or_else(|| Error::Query("Falta la base nueva donde restaurar".into()))?;
            check_id(target, "base")?;
            let source = source.trim();
            check_id(source, "backup")?;
            Ok(format!("RESTORE DATABASE {} FROM BACKUP {};", bq(target), bq(source)))
        }
        BackupAction::Delete { source } => {
            let source = source.trim();
            check_id(source, "backup")?;
            Ok(format!("DROP BACKUP {};", bq(source)))
        }
    }
}

// -- DBine's statements ------------------------------------------------------

#[derive(Debug, PartialEq)]
pub enum Command {
    Create { backup: String, database: String, days: u64, version: Option<String> },
    Restore { database: String, backup: String },
    Drop { backup: String },
}

#[derive(Debug, PartialEq)]
enum Tok {
    Word(String),
    Str(String),
}

fn tokens(stmt: &str) -> Result<Vec<Tok>> {
    let mut out = Vec::new();
    let mut it = stmt.trim().trim_end_matches(';').chars().peekable();
    while let Some(&c) = it.peek() {
        if c.is_whitespace() {
            it.next();
        } else if c == '`' || c == '\'' || c == '"' {
            it.next();
            let mut s = String::new();
            loop {
                match it.next() {
                    None => return Err(Error::Query(format!("Falta cerrar {c} en: {stmt}"))),
                    Some('\\') if c != '`' => match it.next() {
                        Some('x') => {
                            let hex: String = it.by_ref().take(2).collect();
                            s.push(u8::from_str_radix(&hex, 16).map(char::from).unwrap_or('?'));
                        }
                        Some('n') => s.push('\n'),
                        Some(e) => s.push(e),
                        None => {}
                    },
                    Some(ch) if ch == c => break,
                    Some(ch) => s.push(ch),
                }
            }
            out.push(if c == '`' { Tok::Word(s) } else { Tok::Str(s) });
        } else {
            let mut w = String::new();
            while let Some(&ch) = it.peek() {
                if ch.is_whitespace() || ch == '`' || ch == '\'' || ch == '"' {
                    break;
                }
                w.push(ch);
                it.next();
            }
            out.push(Tok::Word(w));
        }
    }
    Ok(out)
}

fn kw(t: Option<&Tok>, k: &str) -> bool {
    matches!(t, Some(Tok::Word(w)) if w.eq_ignore_ascii_case(k))
}

/// One of DBine's backup statements, `None` for anything else.
pub fn parse(stmt: &str) -> Result<Option<Command>> {
    let head: Vec<String> = stmt.split_whitespace().take(2).map(str::to_ascii_uppercase).collect();
    let which = match (head.first().map(String::as_str), head.get(1).map(String::as_str)) {
        (Some("CREATE"), Some("BACKUP")) => 0,
        (Some("RESTORE"), Some("DATABASE")) => 1,
        (Some("DROP"), Some("BACKUP")) => 2,
        _ => return Ok(None),
    };
    let t = tokens(stmt)?;
    let bad = || {
        Error::Query(
            "Sintaxis: CREATE BACKUP <id> FROM DATABASE <base> EXPIRE AFTER <n> DAYS [AS OF '<momento>'] · \
             RESTORE DATABASE <base nueva> FROM BACKUP <id> · DROP BACKUP <id>"
                .into(),
        )
    };
    let word = |i: usize| match t.get(i) {
        Some(Tok::Word(w)) if !w.is_empty() => Ok(w.clone()),
        _ => Err(bad()),
    };
    let cmd = match which {
        0 => {
            if !(kw(t.get(3), "FROM") && kw(t.get(4), "DATABASE") && kw(t.get(6), "EXPIRE") && kw(t.get(7), "AFTER") && kw(t.get(9), "DAYS")) {
                return Err(bad());
            }
            let days: u64 = word(8)?.parse().map_err(|_| bad())?;
            let version = match (t.get(10), t.get(11), t.get(12)) {
                (None, _, _) => None,
                (Some(_), _, Some(Tok::Str(v))) if kw(t.get(10), "AS") && kw(t.get(11), "OF") && t.len() == 13 => Some(v.clone()),
                _ => return Err(bad()),
            };
            Command::Create { backup: word(2)?, database: word(5)?, days, version }
        }
        1 if kw(t.get(3), "FROM") && kw(t.get(4), "BACKUP") && t.len() == 6 => Command::Restore { database: word(2)?, backup: word(5)? },
        2 if t.len() == 3 => Command::Drop { backup: word(2)? },
        _ => return Err(bad()),
    };
    Ok(Some(cmd))
}

/// Run one of DBine's statements against the admin API.
pub async fn run(s: &SpannerSession, cmd: Command, out: &mut QueryOutcome) -> Result<()> {
    let instance = &s.instance;
    match cmd {
        Command::Create { backup, database, days, version } => {
            let mut body = json!({
                "database": format!("{instance}/databases/{database}"),
                "expireTime": rfc3339(now() + days * 86_400),
            });
            if let Some(v) = version {
                body["versionTime"] = json!(v);
            }
            s.api.post(&format!("{instance}/backups?backupId={}", enc(&backup)), &body).await.map_err(unimplemented)?;
            out.messages.push(format!("Spanner está creando el backup {backup}; el historial muestra cuándo está listo."));
        }
        Command::Restore { database, backup } => {
            let body = json!({ "databaseId": database, "backup": format!("{instance}/backups/{backup}") });
            s.api.post(&format!("{instance}/databases:restore"), &body).await.map_err(unimplemented)?;
            out.messages.push(format!("Spanner está restaurando el backup {backup} en la base {database}."));
        }
        Command::Drop { backup } => {
            let url = format!("{}/v1/{instance}/backups/{}", s.api.base, enc(&backup));
            s.api.send(s.api.http.delete(url)).await.map_err(unimplemented)?;
            out.messages.push(format!("Se borró el backup {backup}."));
        }
    }
    out.push_affected(0);
    Ok(())
}

/// The emulator answers `{"code": 12}` (UNIMPLEMENTED) to every backup call.
fn unimplemented(e: Error) -> Error {
    match e {
        Error::Query(m) if m.contains("\"code\":12") || m.contains("UNIMPLEMENTED") => {
            Error::Unsupported("Este servidor no tiene backups (el emulador de Spanner no los implementa).".into())
        }
        other => other,
    }
}

fn enc(v: &str) -> String {
    v.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~/".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

// -- history -----------------------------------------------------------------

fn short(name: &str) -> String {
    name.rsplit('/').next().unwrap_or(name).to_string()
}

/// A ListBackups page as entries.
pub fn entries(page: &Json) -> Vec<BackupEntry> {
    let s = |b: &Json, k: &str| b.get(k).and_then(Json::as_str).filter(|v| !v.is_empty()).map(str::to_string);
    page.get("backups")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .map(|b| {
            let name = s(b, "name").unwrap_or_default();
            let state = s(b, "state");
            let mut details = Vec::new();
            if let Some(v) = s(b, "expireTime") {
                details.push(("Vence".to_string(), v));
            }
            if let Some(v) = s(b, "versionTime") {
                details.push(("Versión de los datos".to_string(), v));
            }
            let refs: Vec<String> = b.get("referencingDatabases").and_then(Json::as_array).into_iter().flatten().filter_map(Json::as_str).map(short).collect();
            if !refs.is_empty() {
                details.push(("Bases que lo usan".to_string(), refs.join(", ")));
            }
            if let Some(v) = b.pointer("/encryptionInfo/encryptionType").and_then(Json::as_str) {
                details.push(("Cifrado".to_string(), v.to_string()));
            }
            BackupEntry {
                id: short(&name),
                database: s(b, "database").map(|d| short(&d)),
                kind: Some("Completo".into()),
                started: s(b, "createTime"),
                finished: None,
                size: b.get("sizeBytes").and_then(|v| v.as_str().and_then(|x| x.parse().ok()).or(v.as_u64())),
                location: Some(name.rsplit_once("/backups/").map_or(name.clone(), |(i, _)| i.to_string())),
                status: state.as_deref().map(|st| match st {
                    "READY" => "listo".to_string(),
                    "CREATING" => "creando".to_string(),
                    other => other.to_ascii_lowercase(),
                }),
                details,
                restorable: state.as_deref() == Some("READY"),
            }
        })
        .collect()
}

pub async fn history(s: &SpannerSession, database: Option<&str>) -> Result<Vec<BackupEntry>> {
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut url = format!("{}/backups?pageSize=500", s.instance);
        if let Some(db) = database.map(str::trim).filter(|d| !d.is_empty()) {
            url.push_str(&format!("&filter={}", enc(&format!("database:{}/databases/{db}", s.instance))));
        }
        if let Some(t) = token.take() {
            url.push_str(&format!("&pageToken={}", enc(&t)));
        }
        let r = s.api.get(&url).await.map_err(unimplemented)?;
        out.extend(entries(&r));
        match r.get("nextPageToken").and_then(Json::as_str) {
            Some(t) if !t.is_empty() => token = Some(t.to_string()),
            _ => break,
        }
    }
    out.sort_by(|a, b| b.started.cmp(&a.started));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn scripts() {
        let s = script(&BackupAction::Backup { database: Some("ventas".into()), options: opts(&[("backup_id", "v-1"), ("expire_days", "30")]) }).unwrap();
        assert_eq!(s, "CREATE BACKUP `v-1` FROM DATABASE `ventas` EXPIRE AFTER 30 DAYS;");
        let s = script(&BackupAction::Backup {
            database: Some("ventas".into()),
            options: opts(&[("backup_id", "v-1"), ("version_time", "2026-09-29T10:00:00Z")]),
        })
        .unwrap();
        assert_eq!(s, "CREATE BACKUP `v-1` FROM DATABASE `ventas` EXPIRE AFTER 7 DAYS AS OF '2026-09-29T10:00:00Z';");
        let s = script(&BackupAction::Backup { database: Some("ventas".into()), options: BTreeMap::new() }).unwrap();
        assert!(s.starts_with("CREATE BACKUP `ventas-2"), "{s}");
        assert!(script(&BackupAction::Backup { database: Some("v".into()), options: opts(&[("backup_id", "Bad Id")]) }).is_err());
        assert!(script(&BackupAction::Backup { database: Some("v".into()), options: opts(&[("backup_id", "ok1"), ("expire_days", "400")]) }).is_err());
        let r = script(&BackupAction::Restore { source: "v-1".into(), database: Some("ventas_copia".into()), options: BTreeMap::new() }).unwrap();
        assert_eq!(r, "RESTORE DATABASE `ventas_copia` FROM BACKUP `v-1`;");
        assert!(script(&BackupAction::Restore { source: "v-1".into(), database: None, options: BTreeMap::new() }).is_err());
        assert_eq!(script(&BackupAction::Delete { source: "v-1".into() }).unwrap(), "DROP BACKUP `v-1`;");
        assert!(script(&BackupAction::Delete { source: "x`; DROP".into() }).is_err());
    }

    #[test]
    fn default_ids() {
        assert_eq!(default_id("ventas", 1_790_690_400), "ventas-20260929-140000");
        let long = "a".repeat(60);
        let id = default_id(&long, 0);
        assert_eq!(id.len(), 60);
        check_id(&id, "backup").unwrap();
    }

    #[test]
    fn statements_round_trip() {
        let b = script(&BackupAction::Backup { database: Some("ventas".into()), options: opts(&[("backup_id", "v-1"), ("version_time", "2026-09-29T10:00:00Z")]) }).unwrap();
        assert_eq!(
            parse(&b).unwrap(),
            Some(Command::Create { backup: "v-1".into(), database: "ventas".into(), days: 7, version: Some("2026-09-29T10:00:00Z".into()) })
        );
        assert_eq!(
            parse("restore database copia from backup `v-1`").unwrap(),
            Some(Command::Restore { database: "copia".into(), backup: "v-1".into() })
        );
        assert_eq!(parse("DROP BACKUP v1;").unwrap(), Some(Command::Drop { backup: "v1".into() }));
        assert_eq!(parse("DROP TABLE t").unwrap(), None);
        assert_eq!(parse("SELECT 1").unwrap(), None);
        assert!(parse("CREATE BACKUP b FROM DATABASE d").is_err());
        assert!(parse("DROP BACKUP a b").is_err());
    }

    #[test]
    fn list_backups_page() {
        let page = json!({ "backups": [
            { "name": "projects/p/instances/i/backups/v-1", "database": "projects/p/instances/i/databases/ventas", "state": "READY",
              "createTime": "2026-09-29T10:00:00Z", "versionTime": "2026-09-29T10:00:00Z", "expireTime": "2026-10-06T10:00:00Z",
              "sizeBytes": "1234", "referencingDatabases": ["projects/p/instances/i/databases/copia"],
              "encryptionInfo": { "encryptionType": "GOOGLE_DEFAULT_ENCRYPTION" } },
            { "name": "projects/p/instances/i/backups/v-2", "database": "projects/p/instances/i/databases/ventas", "state": "CREATING" }
        ]});
        let e = entries(&page);
        assert_eq!(e.len(), 2);
        assert_eq!(e[0].id, "v-1");
        assert_eq!(e[0].database.as_deref(), Some("ventas"));
        assert_eq!(e[0].size, Some(1234));
        assert_eq!(e[0].location.as_deref(), Some("projects/p/instances/i"));
        assert_eq!(e[0].status.as_deref(), Some("listo"));
        assert!(e[0].restorable);
        assert!(e[0].details.contains(&("Bases que lo usan".into(), "copia".into())));
        assert_eq!(e[1].status.as_deref(), Some("creando"));
        assert!(!e[1].restorable);
    }
}
