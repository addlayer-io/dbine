//! Importing data files into a table / collection (docs/api-commands.md).
//! The rows become the driver's own insert script (SQL INSERTs,
//! `insertMany`, `_bulk`…) run in batches, so every engine imports the same way.

use crate::commands::schema::driver_of;
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::import::{self, ImportFormat, ImportOptions, Preview};
use dbine_driver::{DdlParts, ObjectRef, QueryOutcome, TableSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};

#[derive(Deserialize)]
pub struct PreviewArgs {
    pub path: String,
    pub format: ImportFormat,
    #[serde(default)]
    pub options: ImportOptions,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn preview_import_file(args: PreviewArgs) -> CommandResult<Preview> {
    tokio::task::spawn_blocking(move || import::preview(&PathBuf::from(&args.path), args.format, &args.options, 50))
        .await
        .map_err(|e| CommandError::Internal(e.to_string()))?
        .map_err(|e| CommandError::BadRequest(format!("no se pudo leer el archivo: {e}")))
}

#[derive(Deserialize)]
pub struct Mapping {
    pub source: String,
    pub target: String,
}

#[derive(Deserialize)]
pub struct ImportArgs {
    pub import_id: String,
    pub connection_id: String,
    pub database: String,
    pub path: String,
    pub format: ImportFormat,
    #[serde(default)]
    pub options: ImportOptions,
    pub target: ObjectRef,
    /// Created before importing (the new-table option).
    pub create_table: Option<TableSchema>,
    pub mapping: Vec<Mapping>,
    #[serde(default = "default_batch")]
    pub batch: usize,
}

fn default_batch() -> usize {
    500
}

#[derive(Serialize)]
pub struct ImportResult {
    pub rows: u64,
    pub elapsed_ms: u64,
}

/// `import-progress`: rows inserted so far, and the file's row count once
/// it is known (in-memory formats: right after parsing; CSV/TSV: when a
/// background pass has counted the records). `phase` is a key the UI
/// translates: `reading` while the file is parsed, `inserting` after.
#[derive(Clone, Serialize)]
struct ImportProgress {
    id: String,
    rows: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<u64>,
    phase: &'static str,
}

/// Minimum time between two progress events of one import.
const PROGRESS_EVERY: Duration = Duration::from_millis(150);

/// Records in a delimited file, the way the import's CSV reader splits them:
/// quotes only open at the start of a field, `""` inside quotes is a quote,
/// `\r`, `\n` and `\r\n` end a record, empty lines don't count. Stops early
/// (returning `None`) when `stop` is set.
fn count_records(mut r: impl Read, delim: u8, stop: &AtomicBool) -> std::io::Result<Option<u64>> {
    #[derive(Clone, Copy, PartialEq)]
    enum St {
        FieldStart,
        Field,
        Quoted,
        QuoteInQuoted,
    }
    let mut buf = vec![0u8; 1 << 16];
    let (mut st, mut has, mut count, mut first) = (St::FieldStart, false, 0u64, true);
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let mut chunk = &buf[..n];
        if first {
            first = false;
            chunk = chunk.strip_prefix(b"\xEF\xBB\xBF".as_slice()).unwrap_or(chunk);
        }
        for &b in chunk {
            let term = b == b'\n' || b == b'\r';
            st = match st {
                St::Quoted => {
                    if b == b'"' { St::QuoteInQuoted } else { St::Quoted }
                }
                St::FieldStart if b == b'"' => {
                    has = true;
                    St::Quoted
                }
                St::QuoteInQuoted if b == b'"' => St::Quoted,
                _ if term => {
                    if has {
                        count += 1;
                        has = false;
                    }
                    St::FieldStart
                }
                _ => {
                    has = true;
                    if b == delim { St::FieldStart } else { St::Field }
                }
            };
        }
    }
    Ok(Some(count + has as u64))
}

/// The delimiter the import's CSV reader uses for this file (same rule:
/// the chosen one, the format's, or `;`/tab when the first line has more of
/// them than commas).
fn csv_delimiter(path: &Path, format: ImportFormat, opts: &ImportOptions) -> u8 {
    if let Some(d) = opts.delimiter.bytes().next() {
        return d;
    }
    match format {
        ImportFormat::Tsv => b'\t',
        ImportFormat::CsvSemicolon => b';',
        _ => {
            use std::io::BufRead;
            let mut line = String::new();
            let _ = std::fs::File::open(path).map(|f| std::io::BufReader::new(f).read_line(&mut line));
            let count = |c: char| line.matches(c).count();
            if count(';') > count(',') {
                b';'
            } else if count('\t') > count(',') {
                b'\t'
            } else {
                b','
            }
        }
    }
}

#[tauri::command(rename_all = "camelCase")]
pub async fn import_file(app: AppHandle, state: State<'_, AppState>, args: ImportArgs) -> CommandResult<ImportResult> {
    let started = std::time::Instant::now();
    let driver = driver_of(&state, &args.connection_id)?;
    let key = format!("import:{}", args.import_id);
    let entry = state.dedicated_session(&key, &args.connection_id, &args.database, false).await?;

    let path = PathBuf::from(&args.path);
    let emit = |rows: u64, total: Option<u64>, phase: &'static str| {
        let _ = app.emit("import-progress", ImportProgress { id: args.import_id.clone(), rows, total, phase });
    };
    // CSV/TSV are read as a stream: a background pass counts their records
    // for the total and stops when the import ends.
    let total = Arc::new(AtomicU64::new(u64::MAX));
    let done = Arc::new(AtomicU64::new(0));
    let stop_count = Arc::new(AtomicBool::new(false));
    let run = async {
        emit(0, None, "reading");
        let reader = tokio::task::block_in_place(|| import::open(&path, args.format, &args.options))
            .map_err(|e| CommandError::BadRequest(format!("no se pudo leer el archivo: {e}")))?;
        // Source positions of the mapped columns, in target order.
        let picks: Vec<(usize, String)> = args
            .mapping
            .iter()
            .filter(|m| !m.target.is_empty())
            .filter_map(|m| reader.columns.iter().position(|c| *c == m.source).map(|i| (i, m.target.clone())))
            .collect();
        if picks.is_empty() {
            return Err(CommandError::BadRequest("no hay columnas para importar: revisá la correspondencia".into()));
        }
        let columns: Vec<String> = picks.iter().map(|(_, t)| t.clone()).collect();

        // Formats parsed whole into memory already hold every row: take them
        // out of the reader to know how many there are.
        let mut source: Box<dyn Iterator<Item = std::io::Result<Vec<Value>>> + Send> = match reader.format {
            ImportFormat::Json | ImportFormat::JsonLines | ImportFormat::Xlsx | ImportFormat::Xml => {
                let all = reader.collect::<std::io::Result<Vec<_>>>().map_err(|e| CommandError::BadRequest(format!("error leyendo el archivo: {e}")))?;
                total.store(all.len() as u64, Ordering::Relaxed);
                Box::new(all.into_iter().map(Ok))
            }
            format => {
                let (total, stop) = (total.clone(), stop_count.clone());
                let delim = csv_delimiter(&path, format, &args.options);
                let (path, header) = (path.clone(), args.options.header);
                let (app, id, done) = (app.clone(), args.import_id.clone(), done.clone());
                tokio::task::spawn_blocking(move || {
                    let Ok(Some(records)) = std::fs::File::open(&path).and_then(|f| count_records(f, delim, &stop)) else { return };
                    // The import's own events carry it from here; this one
                    // only matters when batches are slow.
                    let n = records.saturating_sub(header as u64);
                    total.store(n, Ordering::Relaxed);
                    if !stop.load(Ordering::Relaxed) {
                        let rows = done.load(Ordering::Relaxed);
                        let _ = app.emit("import-progress", ImportProgress { id, rows, total: Some(n), phase: "inserting" });
                    }
                });
                Box::new(reader)
            }
        };
        let known = || Some(total.load(Ordering::Relaxed)).filter(|t| *t != u64::MAX);
        emit(0, known(), "inserting");

        let exec = |sql: String| {
            let entry = entry.clone();
            async move {
                let mut out = QueryOutcome::default();
                let mut s = entry.session.lock().await;
                tokio::select! {
                    r = s.execute(&sql, 1, &mut out) => r.map_err(CommandError::from),
                    _ = entry.cancel.notified() => Err(CommandError::Cancelled),
                }
            }
        };

        if let Some(t) = &args.create_table {
            let ddl = driver.table_ddl(t, DdlParts { create: true, indexes: true, ..Default::default() })?;
            exec(ddl).await.map_err(|e| CommandError::Sql(format!("no se pudo crear {}: {e}", t.name)))?;
        }

        let mut rows = 0u64;
        let mut last_emit = Instant::now();
        let batch = args.batch.clamp(1, 10_000);
        loop {
            if entry.cancelled.load(Ordering::SeqCst) {
                return Err(CommandError::Cancelled);
            }
            let chunk: Vec<Vec<Value>> = tokio::task::block_in_place(|| {
                source
                    .by_ref()
                    .take(batch)
                    .map(|r| r.map(|row| picks.iter().map(|(i, _)| row.get(*i).cloned().unwrap_or(Value::Null)).collect()))
                    .collect::<std::io::Result<_>>()
            })
            .map_err(|e| CommandError::BadRequest(format!("error leyendo el archivo (fila {}): {e}", rows + 1)))?;
            if chunk.is_empty() {
                break;
            }
            let n = chunk.len() as u64;
            let script = driver.insert_script(&args.target, &columns, &chunk)?;
            exec(script).await.map_err(|e| CommandError::Sql(format!("filas {}–{}: {e}", rows + 1, rows + n)))?;
            rows += n;
            done.store(rows, Ordering::Relaxed);
            if last_emit.elapsed() >= PROGRESS_EVERY {
                last_emit = Instant::now();
                emit(rows, known(), "inserting");
            }
        }
        emit(rows, known(), "inserting");
        Ok(rows)
    };
    let res = run.await;
    stop_count.store(true, Ordering::Relaxed);
    state.sessions.remove(&key);
    Ok(ImportResult { rows: res?, elapsed_ms: started.elapsed().as_millis() as u64 })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(text: &str, delim: u8) -> u64 {
        count_records(text.as_bytes(), delim, &AtomicBool::new(false)).unwrap().unwrap()
    }

    #[test]
    fn counts_records_like_the_csv_reader() {
        assert_eq!(count("", b','), 0);
        assert_eq!(count("a,b\n1,2\n3,4\n", b','), 3);
        assert_eq!(count("a,b\r\n1,2\r\n3,4", b','), 3);
        // Empty lines don't count; a line with only a delimiter does.
        assert_eq!(count("a\n\n\r\nb\n,\n", b','), 3);
        // Newlines and doubled quotes inside quotes.
        assert_eq!(count("a,b\n\"x\ny\",2\n\"he said \"\"hi\n\"\"\",3\n", b','), 3);
        // A quote in the middle of a field doesn't open quoting.
        assert_eq!(count("a\"b,c\nd,e\n", b','), 2);
        // Quotes open only after this file's delimiter.
        assert_eq!(count("a;\"x\ny\"\n", b';'), 1);
        assert_eq!(count("\u{feff}\"a\nb\",c\n", b','), 1);
    }

    #[test]
    fn stops_when_asked() {
        assert_eq!(count_records("a\nb\n".as_bytes(), b',', &AtomicBool::new(true)).unwrap(), None);
    }
}
