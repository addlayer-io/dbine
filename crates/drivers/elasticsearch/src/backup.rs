//! Native backups (docs/backups.md): the snapshot API, the same in
//! Elasticsearch, OpenSearch and Open Distro. A snapshot lives in a
//! registered repository (a shared folder listed in `path.repo`, S3, GCS,
//! Azure…) and covers the chosen indices or all of them, so it's
//! server-wide. The scripts are console requests; an entry's id is
//! `<repository>/<snapshot>`.

use crate::ddl::path_segment;
use dbine_driver::{BackupAction, BackupEntry, BackupSpec, Error, Field, FieldKind, Result};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

pub fn spec() -> BackupSpec {
    BackupSpec {
        backup_options: vec![
            Field::new("repository", "Repositorio", FieldKind::Text)
                .required()
                .placeholder("backups")
                .help("Repositorio de snapshots registrado en el cluster (GET /_snapshot los lista)."),
            Field::new("location", "Registrar el repositorio en", FieldKind::Text)
                .placeholder("/mnt/backups (vacío = ya está registrado)")
                .help("Si se completa, primero registra el repositorio como tipo fs en esa carpeta del servidor, que tiene que estar en path.repo de cada nodo."),
            Field::new("snapshot", "Nombre del snapshot", FieldKind::Text)
                .placeholder("vacío = dbine-<fecha y hora>")
                .help("En minúsculas, sin espacios ni \\ / * ? \" < > | , #."),
            Field::new("indices", "Índices", FieldKind::Text)
                .placeholder("vacío = todos")
                .help("Índices o data streams separados por coma; admite comodines (logs-*)."),
            Field::new("include_global_state", "Incluir el estado global", FieldKind::Bool)
                .default_value("false")
                .help("Plantillas, pipelines, configuración persistente del cluster y los índices del sistema."),
            Field::new("wait", "Esperar a que termine", FieldKind::Bool).default_value("true"),
        ],
        restore: true,
        restore_options: vec![
            Field::new("indices", "Índices", FieldKind::Text)
                .placeholder("vacío = todos los del snapshot")
                .help("Índices o data streams separados por coma; admite comodines."),
            Field::new("rename_pattern", "Patrón de renombre", FieldKind::Text)
                .default_value("(.+)")
                .help("Expresión regular sobre el nombre de cada índice restaurado. Vacío = mismo nombre (el índice existente tiene que estar cerrado o borrado)."),
            Field::new("rename_replacement", "Reemplazo", FieldKind::Text)
                .default_value("restored-$1")
                .help("Nombre nuevo; $1, $2… son los grupos del patrón."),
            Field::new("include_global_state", "Restaurar el estado global", FieldKind::Bool).default_value("false"),
            Field::new("wait", "Esperar a que termine", FieldKind::Bool).default_value("true"),
        ],
        delete: true,
        history: true,
        server_wide: true,
        script_database: "",
        note: "Los snapshots quedan en un repositorio registrado en el cluster (una carpeta compartida listada en path.repo de cada nodo, o S3, GCS, Azure…). Sin repositorio, se puede registrar uno de tipo fs desde las opciones.",
    }
}

fn opt<'a>(options: &'a BTreeMap<String, String>, key: &str) -> &'a str {
    options.get(key).map(|v| v.trim()).unwrap_or_default()
}

fn flag(options: &BTreeMap<String, String>, key: &str, default: bool) -> bool {
    match opt(options, key) {
        "" => default,
        v => v == "true",
    }
}

fn wait_query(options: &BTreeMap<String, String>) -> &'static str {
    if flag(options, "wait", true) {
        "?wait_for_completion=true"
    } else {
        ""
    }
}

/// `repository/snapshot` → its URL path.
fn snapshot_path(source: &str) -> Result<String> {
    match source.trim().split_once('/') {
        Some((repo, snap)) if !repo.is_empty() && !snap.is_empty() => {
            Ok(format!("/_snapshot/{}/{}", path_segment(repo), path_segment(snap)))
        }
        _ => Err(Error::Query(format!("'{source}' no es un snapshot: tiene que ser repositorio/snapshot"))),
    }
}

fn request(method: &str, path: &str, body: &Map<String, Value>) -> String {
    // Keys sorted, whatever `Map` does (serde_json's `preserve_order` may be
    // on through another crate in the build): the same script every time.
    let sorted: BTreeMap<&String, &Value> = body.iter().collect();
    let body = serde_json::to_string_pretty(&sorted).unwrap_or_default();
    format!("{method} {path}\n{body}\n")
}

/// `dbine-YYYYMMDD-HHMMSS` (UTC) for a snapshot with no name.
fn default_name() -> String {
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let t = crate::profiler::utc_ms(ms);
    format!("dbine-{}-{}", t[..10].replace('-', ""), t[11..19].replace(':', ""))
}

pub fn script(action: &BackupAction) -> Result<String> {
    match action {
        BackupAction::Backup { options, .. } => {
            let repo = opt(options, "repository");
            if repo.is_empty() {
                return Err(Error::Query("Falta el repositorio de snapshots".into()));
            }
            let snap = match opt(options, "snapshot") {
                "" => default_name(),
                s => s.to_string(),
            };
            let mut out = String::new();
            let location = opt(options, "location");
            if !location.is_empty() {
                let mut body = Map::new();
                body.insert("type".into(), json!("fs"));
                body.insert("settings".into(), json!({ "location": location }));
                out.push_str(&request("PUT", &format!("/_snapshot/{}", path_segment(repo)), &body));
                out.push('\n');
            }
            let mut body = Map::new();
            let indices = opt(options, "indices");
            if !indices.is_empty() {
                body.insert("indices".into(), json!(indices));
            }
            body.insert("include_global_state".into(), json!(flag(options, "include_global_state", false)));
            let path = format!("{}{}", snapshot_path(&format!("{repo}/{snap}"))?, wait_query(options));
            out.push_str(&request("PUT", &path, &body));
            Ok(out)
        }
        BackupAction::Restore { source, options, .. } => {
            let mut body = Map::new();
            let indices = opt(options, "indices");
            if !indices.is_empty() {
                body.insert("indices".into(), json!(indices));
            }
            let (pattern, replacement) = (opt(options, "rename_pattern"), opt(options, "rename_replacement"));
            if !pattern.is_empty() && !replacement.is_empty() {
                body.insert("rename_pattern".into(), json!(pattern));
                body.insert("rename_replacement".into(), json!(replacement));
            }
            body.insert("include_global_state".into(), json!(flag(options, "include_global_state", false)));
            let path = format!("{}/_restore{}", snapshot_path(source)?, wait_query(options));
            Ok(request("POST", &path, &body))
        }
        BackupAction::Delete { source } => Ok(format!("DELETE {}\n", snapshot_path(source)?)),
    }
}

/// The repositories (`GET /_snapshot`) as `(name, type)`.
pub fn repositories(v: &Value) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = v
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, r)| (name.clone(), r.get("type").and_then(Value::as_str).unwrap_or_default().to_string()))
        .collect();
    out.sort();
    out
}

/// A repository's snapshots (`GET /_snapshot/<repo>/_all`).
pub fn entries(repo: &str, repo_type: &str, v: &Value) -> Vec<(i64, BackupEntry)> {
    let text = |s: &Value, k: &str| s.get(k).and_then(Value::as_str).map(str::to_string);
    let mut out = Vec::new();
    for s in v.get("snapshots").and_then(Value::as_array).into_iter().flatten() {
        let Some(name) = text(s, "snapshot") else { continue };
        let state = text(s, "state");
        let indices: Vec<&str> = s.get("indices").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
        let mut details = vec![("Índices".to_string(), indices.join(", "))];
        if let Some(g) = s.get("include_global_state").and_then(Value::as_bool) {
            details.push(("Estado global".into(), if g { "sí" } else { "no" }.into()));
        }
        if let Some(shards) = s.get("shards") {
            let n = |k: &str| shards.get(k).and_then(Value::as_u64).unwrap_or(0);
            details.push(("Shards".into(), format!("{} de {} ({} fallidos)", n("successful"), n("total"), n("failed"))));
        }
        if let Some(v) = text(s, "version") {
            details.push(("Versión".into(), v));
        }
        if let Some(r) = text(s, "reason") {
            details.push(("Motivo".into(), r));
        }
        let restorable = matches!(state.as_deref(), Some("SUCCESS" | "PARTIAL"));
        out.push((
            s.get("start_time_in_millis").and_then(Value::as_i64).unwrap_or(0),
            BackupEntry {
                id: format!("{repo}/{name}"),
                database: None,
                kind: Some("snapshot".into()),
                started: text(s, "start_time"),
                finished: text(s, "end_time"),
                size: None,
                location: Some(if repo_type.is_empty() { repo.to_string() } else { format!("{repo} ({repo_type})") }),
                status: state,
                details,
                restorable,
            },
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn backup_scripts() {
        let s = script(&BackupAction::Backup {
            database: None,
            options: opts(&[("repository", "my repo"), ("snapshot", "snap-1"), ("indices", "a,b"), ("location", "/mnt/b\"k")]),
        })
        .unwrap();
        assert!(s.starts_with("PUT /_snapshot/my%20repo\n{\n  \"settings\": {\n    \"location\": \"/mnt/b\\\"k\"\n  },\n  \"type\": \"fs\"\n}\n\n"), "{s}");
        assert!(s.contains("PUT /_snapshot/my%20repo/snap-1?wait_for_completion=true\n{\n  \"include_global_state\": false,\n  \"indices\": \"a,b\"\n}"), "{s}");
        let s = script(&BackupAction::Backup { database: None, options: opts(&[("repository", "r"), ("wait", "false")]) }).unwrap();
        assert!(s.starts_with("PUT /_snapshot/r/dbine-") && !s.contains("wait_for"), "{s}");
        assert!(script(&BackupAction::Backup { database: None, options: opts(&[]) }).is_err());
    }

    #[test]
    fn restore_and_delete_scripts() {
        let s = script(&BackupAction::Restore {
            source: "r/s1".into(),
            database: None,
            options: opts(&[("rename_pattern", "(.+)"), ("rename_replacement", "restored-$1"), ("indices", "books")]),
        })
        .unwrap();
        assert_eq!(
            s,
            "POST /_snapshot/r/s1/_restore?wait_for_completion=true\n{\n  \"include_global_state\": false,\n  \"indices\": \"books\",\n  \"rename_pattern\": \"(.+)\",\n  \"rename_replacement\": \"restored-$1\"\n}\n"
        );
        assert_eq!(script(&BackupAction::Delete { source: "r/s#1".into() }).unwrap(), "DELETE /_snapshot/r/s%231\n");
        assert!(script(&BackupAction::Delete { source: "nothing".into() }).is_err());
    }

    #[test]
    fn history_entries() {
        let v = json!({"snapshots": [{"snapshot": "s1", "version": "8.15.0", "indices": ["a", "b"], "include_global_state": false,
            "state": "SUCCESS", "start_time": "2026-09-27T21:02:18.660Z", "start_time_in_millis": 1, "end_time": "2026-09-27T21:02:19.000Z",
            "shards": {"total": 2, "failed": 0, "successful": 2}}]});
        let e = entries("r", "fs", &v);
        assert_eq!(e.len(), 1);
        let e = &e[0].1;
        assert_eq!((e.id.as_str(), e.status.as_deref(), e.restorable), ("r/s1", Some("SUCCESS"), true));
        assert_eq!(e.location.as_deref(), Some("r (fs)"));
        assert_eq!(repositories(&json!({"b": {"type": "s3"}, "a": {"type": "fs"}})), [("a".to_string(), "fs".to_string()), ("b".into(), "s3".into())]);
    }
}
