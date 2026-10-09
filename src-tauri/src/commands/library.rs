//! The script Library (docs/library.md): reusable scripts per engine,
//! not tied to a database. Opening one copies it into a query; the Library
//! keeps the original.

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::LibraryScript;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tauri::State;

#[tauri::command]
pub async fn list_library(state: State<'_, AppState>) -> CommandResult<Vec<LibraryScript>> {
    Ok(state.store.list_library()?)
}

#[derive(Deserialize)]
pub struct SaveArgs {
    pub script: LibraryScript,
}

fn clean_folder(f: &str) -> String {
    f.split('/').map(str::trim).filter(|p| !p.is_empty()).collect::<Vec<_>>().join("/")
}

#[tauri::command(rename_all = "camelCase")]
pub async fn save_library_script(state: State<'_, AppState>, args: SaveArgs) -> CommandResult<LibraryScript> {
    let mut s = args.script;
    s.name = s.name.trim().to_string();
    if s.name.is_empty() {
        return Err(CommandError::BadRequest("el script necesita un nombre".into()));
    }
    if s.id.is_empty() {
        s.id = uuid::Uuid::new_v4().to_string();
    }
    s.folder = clean_folder(&s.folder);
    s.engines.sort();
    s.engines.dedup();
    Ok(state.store.save_library_script(&s)?)
}

#[derive(Deserialize)]
pub struct IdArgs {
    pub id: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn delete_library_script(state: State<'_, AppState>, args: IdArgs) -> CommandResult<()> {
    Ok(state.store.delete_library_script(&args.id)?)
}

#[derive(Deserialize)]
pub struct ImportArgs {
    /// Files, or folders (their `.sql` / `.js` / `.txt` … files, recursively,
    /// keeping subfolders as Library folders).
    pub paths: Vec<String>,
    pub engines: Vec<String>,
    #[serde(default)]
    pub folder: String,
}

#[derive(Serialize)]
pub struct ImportOut {
    pub imported: usize,
    pub skipped: Vec<String>,
}

/// Extensions read as scripts (import, and the git copy).
pub(crate) const SCRIPT_EXTS: &[&str] = &["sql", "cql", "js", "json", "txt", "redis", "cypher", "flux", "ksql", "n1ql", "psql"];

/// A script's file extension, from the query language of the engines it's
/// for (the first one that's known): every driver says its language, so
/// this covers all of them. `*sql` or nothing: `.sql`.
pub(crate) fn script_ext(engines: &[String]) -> &'static str {
    use dbine_driver::Language;
    let info = engines.iter().filter(|e| !e.starts_with('*')).find_map(|e| crate::state::driver_info(e).ok());
    let Some(info) = info else { return "sql" };
    match info.language {
        Language::Sql => "sql",
        Language::Cql => "cql",
        Language::Redis => "redis",
        Language::Flux => "flux",
        Language::Cypher => "cypher",
        Language::Json => match info.dialect {
            // PartiQL (DynamoDB) and Cosmos DB's SQL are queries, not documents.
            "partiql" | "cosmos" => "sql",
            // MongoDB's shell syntax: `db.coll.find({…})`.
            _ if MONGO_SHELL.contains(&info.id) => "js",
            _ => "json",
        },
    }
}

/// The engines a script file is for, from its extension (files added by
/// hand to the Library's git repo). JSON could be any document engine:
/// no engine, so it shows for all.
pub(crate) fn engines_for_ext(ext: &str) -> Vec<String> {
    let one = |e: &str| vec![e.to_string()];
    match ext.to_ascii_lowercase().as_str() {
        "js" => one("mongodb"),
        "cql" => one("cassandra"),
        "redis" => one("redis"),
        "cypher" => one("neo4j"),
        "flux" => one("influxdb"),
        "json" => Vec::new(),
        _ => one("*sql"),
    }
}

/// Drivers written in MongoDB's shell syntax (the mongodb crate's variants).
const MONGO_SHELL: &[&str] = &["mongodb", "documentdb", "ferretdb"];

/// How a line comment starts in scripts with that extension (`None`: the
/// language has none, like JSON or Redis commands).
pub(crate) fn line_comment(ext: &str) -> Option<&'static str> {
    match ext {
        "sql" | "cql" | "psql" | "ksql" | "n1ql" => Some("--"),
        "js" | "cypher" | "flux" => Some("//"),
        _ => None,
    }
}
/// Bigger than this isn't a script to keep in the Library (a dump).
const MAX_BYTES: u64 = 2 * 1024 * 1024;

fn collect(path: &Path, rel_folder: &str, out: &mut Vec<(PathBuf, String)>, depth: usize) {
    if path.is_dir() {
        if depth > 8 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(path) else { return };
        let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
        entries.sort();
        for p in entries {
            if p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')) {
                continue;
            }
            let sub = if p.is_dir() {
                let n = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                if rel_folder.is_empty() { n } else { format!("{rel_folder}/{n}") }
            } else {
                rel_folder.to_string()
            };
            collect(&p, &sub, out, depth + 1);
        }
    } else if path.extension().and_then(|e| e.to_str()).is_some_and(|e| SCRIPT_EXTS.contains(&e.to_ascii_lowercase().as_str())) {
        out.push((path.to_path_buf(), rel_folder.to_string()));
    }
}

/// Bring `.sql` files (a DBA's scripts folder) into the Library: the file
/// name is the script's name, a leading `-- descripción` comment line its
/// description.
#[tauri::command(rename_all = "camelCase")]
pub async fn import_library_files(state: State<'_, AppState>, args: ImportArgs) -> CommandResult<ImportOut> {
    let mut files = Vec::new();
    for p in &args.paths {
        let path = Path::new(p);
        // A chosen folder becomes a Library folder itself.
        let base = if path.is_dir() { path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default() } else { String::new() };
        collect(path, &base, &mut files, 0);
    }
    let existing = state.store.list_library()?;
    let mut imported = 0;
    let mut skipped = Vec::new();
    for (path, rel) in files {
        let name = path.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if std::fs::metadata(&path).map(|m| m.len() > MAX_BYTES).unwrap_or(true) {
            skipped.push(format!("{}: más de 2 MB", path.display()));
            continue;
        }
        let text = match std::fs::read(&path) {
            Ok(b) => String::from_utf8_lossy(&b).replace("\r\n", "\n"),
            Err(e) => {
                skipped.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let folder = clean_folder(&[args.folder.as_str(), rel.as_str()].join("/"));
        // Importing the same folder again updates instead of duplicating.
        let id = existing
            .iter()
            .find(|s| s.name == name && s.folder == folder)
            .map(|s| s.id.clone())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let description = text
            .lines()
            .next()
            .and_then(|l| l.trim().strip_prefix("--").or_else(|| l.trim().strip_prefix("//")))
            .map(|d| d.trim().to_string())
            .unwrap_or_default();
        let mut engines = args.engines.clone();
        engines.sort();
        engines.dedup();
        state.store.save_library_script(&LibraryScript { id, name, folder, description, engines, text, updated_at: String::new() })?;
        imported += 1;
    }
    Ok(ImportOut { imported, skipped })
}

#[derive(Deserialize)]
pub struct ExportArgs {
    pub dir: String,
    /// Only these; all when empty.
    #[serde(default)]
    pub ids: Vec<String>,
}

pub(crate) fn safe_name(s: &str) -> String {
    let n: String = s.chars().map(|c| if r#"/\:*?"<>|"#.contains(c) || c.is_control() { '_' } else { c }).collect();
    let n = n.trim().trim_matches('.').to_string();
    if n.is_empty() { "script".into() } else { n }
}

/// Write scripts as files, in subfolders like the Library's.
#[tauri::command(rename_all = "camelCase")]
pub async fn export_library(state: State<'_, AppState>, args: ExportArgs) -> CommandResult<usize> {
    let all = state.store.list_library()?;
    let root = PathBuf::from(&args.dir);
    let mut n = 0;
    for s in all.iter().filter(|s| args.ids.is_empty() || args.ids.contains(&s.id)) {
        let mut dir = root.clone();
        for part in s.folder.split('/').filter(|p| !p.is_empty()) {
            dir.push(safe_name(part));
        }
        std::fs::create_dir_all(&dir).map_err(|e| CommandError::Internal(e.to_string()))?;
        let ext = script_ext(&s.engines);
        // The description as a first comment line, where the language has comments.
        let body = match line_comment(ext) {
            Some(c) if !s.description.is_empty() && !s.text.trim_start().starts_with(c) => format!("{c} {}\n{}", s.description, s.text),
            _ => s.text.clone(),
        };
        std::fs::write(dir.join(format!("{}.{ext}", safe_name(&s.name))), body).map_err(|e| CommandError::Internal(e.to_string()))?;
        n += 1;
    }
    Ok(n)
}

#[cfg(test)]
mod ext_tests {
    use super::*;

    #[test]
    fn extensions_follow_the_engine_language() {
        let e = |ids: &[&str]| script_ext(&ids.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(e(&[]), "sql");
        assert_eq!(e(&["*sql"]), "sql");
        assert_eq!(e(&["postgres", "sqlserver"]), "sql");
        assert_eq!(e(&["mongodb"]), "js");
        assert_eq!(e(&["elasticsearch"]), "json");
        assert_eq!(e(&["dynamodb"]), "sql");
        assert_eq!(e(&["cassandra"]), "cql");
        assert_eq!(e(&["redis"]), "redis");
        assert_eq!(e(&["neo4j"]), "cypher");
        assert_eq!(e(&["influxdb"]), "flux");
        assert_eq!(line_comment("json"), None);
        assert_eq!(line_comment("js"), Some("//"));
        assert_eq!(engines_for_ext("JS"), vec!["mongodb".to_string()]);
        assert!(engines_for_ext("json").is_empty());
    }
}
