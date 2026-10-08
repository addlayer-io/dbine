//! "Buscar en la base" (docs/busqueda.md): object names and the text of
//! views, routines, triggers… On a session of its own (the explorer's
//! stays free), read-only, cancellable with `cancel_query` on
//! `search:<id>`. Names come from `list_objects`; the text from the
//! driver's catalog query when it has one (`Session::search_code`), else
//! from each object's definition, with progress and partial hits.

use crate::commands::schema::driver_of;
use crate::error::CommandResult;
use crate::state::AppState;
use dbine_driver::search::{hits_in, line_matches, CodeHit, CodeSearch};
use dbine_driver::ObjectRef;
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, State};

/// Hits at most when the query sets no cap.
const DEFAULT_CAP: usize = 2000;
/// How often the partial hits go to the UI.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

#[derive(Deserialize)]
pub struct SearchArgs {
    pub connection_id: String,
    pub database: String,
    /// The UI's id for this search: its events and `cancel_query`.
    pub search_id: String,
    pub query: CodeSearch,
    /// Look in object names.
    #[serde(default = "yes")]
    pub names: bool,
    /// Look in the objects' source.
    #[serde(default = "yes")]
    pub code: bool,
}

fn yes() -> bool {
    true
}

/// One hit: in a name (`line` 0) or in a source line.
pub type SearchHit = CodeHit;

#[derive(Serialize, Clone)]
struct Progress<'a> {
    search_id: &'a str,
    done: usize,
    total: usize,
    /// The hits found since the previous event.
    hits: Vec<SearchHit>,
}

#[derive(Serialize, Default)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    /// Objects whose source was read.
    pub scanned: usize,
    pub unreadable: Vec<String>,
    /// The cap cut it short.
    pub truncated: bool,
    pub cancelled: bool,
    /// The driver answered from its catalog (no per-object scan).
    pub from_catalog: bool,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn search_database(app: AppHandle, state: State<'_, AppState>, args: SearchArgs) -> CommandResult<SearchResult> {
    let key = format!("search:{}", args.search_id);
    let with_source: Vec<String> = driver_of(&state, &args.connection_id)?
        .info()
        .object_kinds
        .iter()
        .filter(|k| k.has_definition)
        .map(|k| k.id.to_string())
        .collect();
    let entry = state.dedicated_session(&key, &args.connection_id, &args.database, true).await?;
    let result = run(&app, &entry, &args, &with_source).await;
    state.sessions.remove(&key);
    result
}

async fn run(app: &AppHandle, entry: &crate::state::SessionEntry, args: &SearchArgs, with_source: &[String]) -> CommandResult<SearchResult> {
    let q = &args.query;
    let cap = if q.max_hits == 0 { DEFAULT_CAP } else { q.max_hits };
    let mut out = SearchResult::default();
    let cancelled = || entry.cancelled.load(Ordering::Relaxed);
    let wanted = |kind: &str| q.kinds.is_empty() || q.kinds.iter().any(|k| k == kind);
    let emit = |done: usize, total: usize, hits: Vec<SearchHit>| {
        let _ = app.emit("code-search-progress", Progress { search_id: &args.search_id, done, total, hits });
    };
    let mut s = entry.session.lock().await;
    let objects = s.list_objects().await?;

    if args.names && !q.text.is_empty() {
        let hits: Vec<SearchHit> = objects
            .iter()
            .filter(|o| wanted(&o.kind) && line_matches(&o.name, q))
            .take(cap)
            .map(|o| CodeHit { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone(), parent: o.parent.clone(), line: 0, text: String::new() })
            .collect();
        emit(0, 0, hits.clone());
        out.hits = hits;
    }
    if !args.code || q.text.is_empty() || out.hits.len() >= cap {
        out.truncated = out.hits.len() >= cap;
        return Ok(out);
    }

    // The driver's catalog, in one query.
    let mut catalog_query = q.clone();
    catalog_query.max_hits = cap - out.hits.len();
    if let Some(report) = s.search_code(&catalog_query).await? {
        let hits: Vec<SearchHit> = report.hits.into_iter().filter(|h| wanted(&h.kind)).collect();
        emit(report.scanned, report.scanned, hits.clone());
        out.hits.extend(hits);
        out.scanned = report.scanned;
        out.unreadable = report.unreadable;
        out.truncated = report.truncated;
        out.from_catalog = true;
        return Ok(out);
    }

    // Each object's source.
    let candidates: Vec<_> = objects.iter().filter(|o| with_source.contains(&o.kind) && wanted(&o.kind)).collect();
    let total = candidates.len();
    let mut pending = Vec::new();
    let mut last = Instant::now();
    for (i, o) in candidates.iter().enumerate() {
        if cancelled() {
            out.cancelled = true;
            break;
        }
        let obj = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
        match s.definition(&obj).await {
            Ok(Some(source)) => {
                out.scanned += 1;
                let hits = hits_in(&o.kind, o.schema.as_deref(), &o.name, o.parent.as_deref(), &source, q);
                pending.extend(hits.iter().cloned());
                out.hits.extend(hits);
            }
            Ok(None) => out.scanned += 1,
            Err(_) => out.unreadable.push(match &o.schema {
                Some(sc) => format!("{sc}.{}", o.name),
                None => o.name.clone(),
            }),
        }
        if out.hits.len() >= cap {
            out.hits.truncate(cap);
            out.truncated = true;
            emit(i + 1, total, std::mem::take(&mut pending));
            break;
        }
        if last.elapsed() >= PROGRESS_EVERY || i + 1 == total {
            emit(i + 1, total, std::mem::take(&mut pending));
            last = Instant::now();
        }
    }
    Ok(out)
}
