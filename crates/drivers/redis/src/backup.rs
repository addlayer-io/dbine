//! Native backups (docs/backups.md): an RDB snapshot of the whole server
//! with `BGSAVE` (or the blocking `SAVE`). The file stays on the server, in
//! `CONFIG GET dir` / `dbfilename` (Dragonfly: its own `.dfs` format by
//! default, the last file in `INFO persistence`); the history is the last save the
//! server knows of (`LASTSAVE` and `INFO persistence`). Restoring means
//! stopping the server and putting the file in place, which no command
//! does, so DBine doesn't offer it.

use crate::monitor::parse_info;
use crate::shape::text_of;
use crate::RedisSession;
use dbine_driver::{BackupAction, BackupEntry, BackupSpec, Error, Field, FieldKind, Result};
use redis::Value;

pub fn spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![Field::new(
            "command",
            "Comando",
            FieldKind::Select(vec![("BGSAVE", "BGSAVE (en segundo plano)"), ("SAVE", "SAVE (bloquea el servidor)")]),
        )
        .default_value("BGSAVE")
        .help("BGSAVE guarda en un proceso aparte y el servidor sigue atendiendo; SAVE lo frena hasta terminar.")],
        restore: false,
        restore_options: Vec::new(),
        delete: false,
        history: true,
        server_wide: true,
        script_database: "",
        note: "El snapshot (RDB; en Dragonfly, .dfs) abarca todas las bases y queda en el servidor, en la carpeta `dir` con el nombre `dbfilename` (CONFIG GET). Cada backup reemplaza al anterior. Para restaurarlo hay que detener el servidor, poner el archivo en esa carpeta y volver a iniciarlo: no se hace desde DBine.",
    }
}

pub fn script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { options, .. } => match options.get("command").map(String::as_str).unwrap_or("BGSAVE") {
            "SAVE" => Ok("SAVE".into()),
            "BGSAVE" | "" => Ok("BGSAVE".into()),
            other => Err(Error::Query(format!("'{other}' no es un comando de backup de Redis (BGSAVE o SAVE)"))),
        },
        BackupAction::Restore { .. } => Err(Error::Unsupported(
            "Redis no restaura un RDB con un comando: hay que detener el servidor, copiar el archivo a su carpeta `dir` y volver a iniciarlo".into(),
        )),
        BackupAction::Delete { .. } => Err(Error::Unsupported(
            "Redis no borra su RDB con un comando: el archivo está en el disco del servidor".into(),
        )),
    }
}

/// `CONFIG GET <name>`'s value (RESP2 flat array or RESP3 map).
async fn config(s: &mut RedisSession, name: &str) -> Option<String> {
    let v = s.run(&[b"CONFIG", b"GET", name.as_bytes()]).await.ok()?;
    let value = match v {
        Value::Map(m) => m.into_iter().next().map(|(_, v)| v),
        Value::Array(a) => a.into_iter().nth(1),
        _ => None,
    }?;
    Some(text_of(&value)).filter(|t| !t.is_empty())
}

/// Unix seconds as ISO 8601 (UTC).
fn iso(secs: i64) -> Option<String> {
    let s = crate::profiler::stamp(&secs.to_string())?;
    Some(format!("{}Z", s.replacen(' ', "T", 1).trim_end_matches(".000")))
}

/// `dir` + `dbfilename`, the way the server joins them.
fn rdb_path(dir: Option<String>, file: Option<String>) -> Option<String> {
    match (dir, file) {
        (Some(d), Some(f)) => Some(format!("{}/{f}", d.trim_end_matches('/'))),
        (None, f) => f,
        (d, None) => d,
    }
}

pub async fn history(s: &mut RedisSession) -> Result<Vec<BackupEntry>> {
    let info = parse_info(&text_of(&s.run(&[b"INFO", b"persistence"]).await?));
    let last = s.int(&[b"LASTSAVE"]).await.filter(|t| *t > 0);
    // Dragonfly's `dbfilename` is a template (`dump-{timestamp}`); the file
    // it wrote last is in `last_saved_file`.
    let file = match info.get("last_saved_file").filter(|f| !f.is_empty()) {
        Some(f) => Some(f.clone()),
        None => config(s, "dbfilename").await,
    };
    let path = rdb_path(config(s, "dir").await, file);
    let kind = if path.as_deref().is_some_and(|p| p.ends_with(".dfs")) { "DFS" } else { "RDB" };
    let in_progress = info.get("rdb_bgsave_in_progress").is_some_and(|v| v == "1");
    let status = if in_progress || info.get("saving").is_some_and(|v| v == "1") {
        Some("en curso".to_string())
    } else {
        info.get("rdb_last_bgsave_status").map(|v| if v == "ok" { "completado".to_string() } else { v.clone() })
    };
    let mut details = Vec::new();
    let labels = [
        ("rdb_changes_since_last_save", "Cambios desde el último guardado"),
        ("rdb_changes_since_last_success_save", "Cambios desde el último guardado"),
        ("rdb_last_bgsave_time_sec", "Duración del último BGSAVE (s)"),
        ("last_success_save_duration_sec", "Duración del último guardado (s)"),
        ("last_error", "Último error"),
        ("rdb_saves", "Guardados desde el inicio"),
        ("aof_enabled", "AOF activado"),
    ];
    for (key, label) in labels {
        if let Some(v) = info.get(key).filter(|v| !v.is_empty() && *v != "-1") {
            details.push((label.to_string(), v.clone()));
        }
    }
    if last.is_none() && path.is_none() {
        return Ok(Vec::new());
    }
    Ok(vec![BackupEntry {
        id: path.clone().unwrap_or_default(),
        database: None,
        kind: Some(kind.into()),
        started: None,
        finished: last.and_then(iso),
        size: None,
        location: path,
        status,
        details,
        restorable: false,
    }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn backup(command: &str) -> Result<String> {
        let options = BTreeMap::from([("command".to_string(), command.to_string())]);
        script(&BackupAction::Backup { database: None, options })
    }

    #[test]
    fn scripts() {
        assert_eq!(backup("BGSAVE").unwrap(), "BGSAVE");
        assert_eq!(backup("SAVE").unwrap(), "SAVE");
        assert_eq!(script(&BackupAction::Backup { database: None, options: BTreeMap::new() }).unwrap(), "BGSAVE");
        assert!(backup("FLUSHALL").is_err());
        assert!(matches!(script(&BackupAction::Delete { source: "x".into() }), Err(Error::Unsupported(_))));
        let restore = BackupAction::Restore { source: "x".into(), database: None, options: BTreeMap::new() };
        assert!(matches!(script(&restore), Err(Error::Unsupported(_))));
    }

    #[test]
    fn times_and_paths() {
        assert_eq!(iso(1_706_708_700).as_deref(), Some("2024-01-31T13:45:00Z"));
        assert_eq!(rdb_path(Some("/data/".into()), Some("dump.rdb".into())).as_deref(), Some("/data/dump.rdb"));
        assert_eq!(rdb_path(None, Some("dump.rdb".into())).as_deref(), Some("dump.rdb"));
    }
}
