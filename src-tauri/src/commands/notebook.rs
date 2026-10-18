//! Notebooks (`.ipynb`): parse, serialize, read and write standalone files,
//! export (crate::notebook; docs/notebooks.md). Project files go through
//! projects' read/write with the text these give.

use crate::commands::projects::{atomic_write, sha256_hex};
use crate::error::{CommandError, CommandResult};
use crate::notebook::{self, export, Notebook, PreviewLimits};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParseArgs {
    pub text: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_parse(args: ParseArgs) -> CommandResult<Notebook> {
    notebook::parse(&args.text).map_err(CommandError::BadRequest)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SerializeArgs {
    pub notebook: Notebook,
    #[serde(default)]
    pub rows_per_cell: Option<u32>,
    #[serde(default)]
    pub bytes_per_cell: Option<u64>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_serialize(args: SerializeArgs) -> CommandResult<String> {
    Ok(notebook::serialize(&args.notebook, PreviewLimits::new(args.rows_per_cell, args.bytes_per_cell)))
}

/// Only `.ipynb` files are read or written here.
fn notebook_path(path: &str) -> CommandResult<&Path> {
    let p = Path::new(path);
    let ok = p.is_absolute() && p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("ipynb"));
    if ok {
        Ok(p)
    } else {
        Err(CommandError::BadRequest(format!("«{path}» no es un notebook (.ipynb)")))
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadArgs {
    pub path: String,
}

#[derive(Serialize, Debug)]
pub struct ReadOut {
    pub notebook: Notebook,
    /// sha256 (hex) of the bytes on disk, as projects' files use.
    pub text_hash: String,
}

pub fn read(path: &str) -> CommandResult<ReadOut> {
    let p = notebook_path(path)?;
    let bytes = std::fs::read(p).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => CommandError::NotFound(format!("no existe el archivo «{path}»")),
        _ => CommandError::Internal(format!("no se pudo leer «{path}»: {e}")),
    })?;
    let text = std::str::from_utf8(&bytes).map_err(|_| CommandError::BadRequest("el notebook no está en UTF-8".into()))?;
    let notebook = notebook::parse(text).map_err(CommandError::BadRequest)?;
    Ok(ReadOut { notebook, text_hash: sha256_hex(&bytes) })
}

#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_read(args: ReadArgs) -> CommandResult<ReadOut> {
    tauri::async_runtime::spawn_blocking(move || read(&args.path))
        .await
        .map_err(|e| CommandError::Internal(e.to_string()))?
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteArgs {
    pub path: String,
    pub notebook: Notebook,
    #[serde(default)]
    pub rows_per_cell: Option<u32>,
    #[serde(default)]
    pub bytes_per_cell: Option<u64>,
}

pub fn write(args: &WriteArgs) -> CommandResult<()> {
    let p = notebook_path(&args.path)?;
    let text = notebook::serialize(&args.notebook, PreviewLimits::new(args.rows_per_cell, args.bytes_per_cell));
    atomic_write(p, text.as_bytes())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_write(args: WriteArgs) -> CommandResult<()> {
    tauri::async_runtime::spawn_blocking(move || write(&args)).await.map_err(|e| CommandError::Internal(e.to_string()))?
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportArgs {
    pub notebook: Notebook,
    /// "html" | "markdown".
    pub format: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn notebook_export(args: ExportArgs) -> CommandResult<String> {
    match args.format.as_str() {
        "html" => Ok(export::html(&args.notebook)),
        "markdown" | "md" => Ok(export::markdown(&args.notebook)),
        f => Err(CommandError::BadRequest(format!("formato de exportación desconocido: «{f}»"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_read_and_written_atomically_with_their_hash() {
        let dir = std::env::temp_dir().join(format!("dbine-notebook-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.ipynb").to_string_lossy().into_owned();
        let nb = notebook::parse(r#"{"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"cell_type":"markdown","id":"m","metadata":{},"source":"hola"}]}"#).unwrap();
        write(&WriteArgs { path: path.clone(), notebook: nb, rows_per_cell: None, bytes_per_cell: None }).unwrap();
        let out = read(&path).unwrap();
        assert_eq!(out.notebook.cells[0].source, "hola");
        assert_eq!(out.text_hash, sha256_hex(&std::fs::read(&path).unwrap()));
        assert!(matches!(read(&dir.join("b.ipynb").to_string_lossy()), Err(CommandError::NotFound(_))));
        assert!(matches!(read(&dir.join("x.txt").to_string_lossy()), Err(CommandError::BadRequest(_))));
        assert!(matches!(read("relative.ipynb"), Err(CommandError::BadRequest(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
