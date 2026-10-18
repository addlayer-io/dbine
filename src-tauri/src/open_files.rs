//! Data files opened from the OS: a double click in Finder / Explorer ("Open
//! with DBine"), a drop on a window, File › Open…
//!
//! Each file becomes a *temporary* connection (dbine-core's
//! `add_temporary_connection`): in the explorer with an "Archivo" badge, gone
//! at the next start unless the user saves it. Opening the same file again
//! focuses the connection it already has (temporary or saved).
//!
//! - SQLite (`.sqlite`, `.sqlite3`, `.db3`, a `.db` that is one): the SQLite
//!   driver on that file.
//! - DuckDB (`.duckdb`, `.ddb`, a `.db` that is one): the DuckDB driver; its
//!   driver and library download on first use, as for any DuckDB connection.
//! - CSV / TSV / Parquet / JSON / Excel: the "Archivos" driver with "Sólo este
//!   archivo", an in-memory DuckDB with the file as a view named after it;
//!   the UI runs `SELECT * FROM "<name>" LIMIT 1000` in a new query.
//!
//! Where the paths come from:
//! - macOS: `RunEvent::Opened` (lib.rs), at launch and while running.
//! - Windows / Linux: the command line, at launch (setup) and from a second
//!   instance (the single-instance plugin hands its argv over).
//! - A drop: the webview's own drag and drop stays on (the explorer uses
//!   it), so the paths are read natively next to it ([`drop`]).
//!
//! Paths wait in a queue until a window takes them (`open_files_take`): at
//! launch the UI isn't listening yet. The `open-files` event only tells a
//! window to look.
//!
//! The file is never run or loaded as code: its path only goes into a
//! connection's settings, and into SQL quoted as a literal by the driver
//! (`read_csv_auto('…')`); the view's name is quoted as an identifier.

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::state::SavedConnection;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::ConnectionConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, State};

/// Rows the first query of a data file shows.
const FIRST_ROWS: u32 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Sqlite,
    Duckdb,
    /// CSV, TSV, Parquet, JSON or an Excel workbook.
    Data,
}

impl Kind {
    fn driver(self) -> &'static str {
        match self {
            Kind::Sqlite => "sqlite",
            Kind::Duckdb => "duckdb",
            Kind::Data => "duckdb_files",
        }
    }
}

/// Data files by extension (`.csv.gz` counts as CSV), as the "Archivos"
/// driver reads them (crates/drivers/duckdb/src/files.rs, which isn't in
/// the app's build when drivers are downloaded).
const DATA_EXTS: &[&str] = &[".csv", ".tsv", ".parquet", ".json", ".jsonl", ".ndjson", ".xlsx"];
/// Compressions DuckDB reads for the text formats.
const COMPRESSED: &[&str] = &[".gz", ".zst"];

/// The data file's name without its extension: the view's name, as the
/// driver names it. `None`: not a data file.
pub fn data_stem(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let lower = name.to_ascii_lowercase();
    let base = COMPRESSED.iter().find_map(|c| lower.strip_suffix(c)).unwrap_or(&lower);
    let ext = DATA_EXTS.iter().find(|e| base.ends_with(*e))?;
    // A compressed workbook or Parquet isn't something DuckDB reads.
    if base.len() != lower.len() && matches!(*ext, ".parquet" | ".xlsx") {
        return None;
    }
    let stem = base.len() - ext.len();
    (stem > 0).then(|| name[..stem].to_string())
}

/// What a file is: by its first bytes when they say (a `.db` may be either
/// engine's), else by its extension.
pub fn classify(path: &Path) -> Option<Kind> {
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    let mut head = [0u8; 16];
    let read = std::fs::File::open(path).and_then(|mut f| {
        use std::io::Read;
        let mut n = 0;
        while n < head.len() {
            match f.read(&mut head[n..])? {
                0 => break,
                k => n += k,
            }
        }
        Ok(n)
    });
    let n = read.unwrap_or(0);
    let db_ext = matches!(ext.as_str(), "sqlite" | "sqlite3" | "db3" | "s3db" | "sl3" | "db" | "duckdb" | "ddb");
    if db_ext && n == 16 && &head == b"SQLite format 3\0" {
        return Some(Kind::Sqlite);
    }
    if db_ext && n >= 12 && &head[8..12] == b"DUCK" {
        return Some(Kind::Duckdb);
    }
    match ext.as_str() {
        // Empty (a new database) or not readable yet: the extension says.
        "sqlite" | "sqlite3" | "db3" | "s3db" | "sl3" if n < 16 => Some(Kind::Sqlite),
        "duckdb" | "ddb" if n < 12 => Some(Kind::Duckdb),
        "db" if n == 0 => Some(Kind::Sqlite),
        // Another engine's or app's file under a database extension
        // (Thumbs.db, a Berkeley DB…).
        "sqlite" | "sqlite3" | "db3" | "s3db" | "sl3" | "db" | "duckdb" | "ddb" => None,
        _ => data_stem(path).map(|_| Kind::Data),
    }
}

/// The first query of a data file: its rows, the view's name quoted.
pub fn first_query(view: &str) -> String {
    format!("SELECT * FROM {} LIMIT {FIRST_ROWS}", quote_ident(Quote::Double, view))
}

/// The connection a file opens as (not saved yet).
pub fn connection_for(path: &Path, kind: Kind) -> SavedConnection {
    let mut config = ConnectionConfig { driver: kind.driver().into(), host: path.to_string_lossy().into(), ..Default::default() };
    if kind == Kind::Data {
        config.options.insert("single".into(), "true".into());
    }
    SavedConnection {
        id: uuid::Uuid::new_v4().to_string(),
        name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string_lossy().into()),
        color: None,
        config,
        save_password: false,
        folder_id: None,
        tags: Vec::new(),
        mcp_level: None,
        updated_at: String::new(),
    }
}

/// Same file, compared resolved (symlinks, `..`, letter case on
/// case-insensitive disks); a path that can't be resolved, as written.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// A connection (temporary or saved) already on this file, as this kind.
fn existing<'a>(conns: &'a [SavedConnection], path: &Path, kind: Kind) -> Option<&'a SavedConnection> {
    conns.iter().find(|c| {
        c.config.driver == kind.driver()
            && (kind != Kind::Data || c.config.option("single") == Some("true"))
            && !c.config.host.trim().is_empty()
            && same_file(Path::new(c.config.host.trim()), path)
    })
}

/// File paths among a command line's arguments (`args` without the
/// program): options (`--new-window`, `--run-task id`…) and anything that
/// isn't an existing file DBine opens are left out. Relative paths are the
/// launching shell's (`cwd`); `file://` URLs (some Linux launchers) count.
pub fn paths_from_args<I: IntoIterator<Item = String>>(args: I, cwd: &Path) -> Vec<PathBuf> {
    args.into_iter()
        .filter(|a| !a.starts_with('-') && !a.trim().is_empty())
        .filter_map(|a| {
            if a.starts_with("file://") {
                return tauri::Url::parse(&a).ok()?.to_file_path().ok();
            }
            let p = PathBuf::from(&a);
            Some(if p.is_absolute() { p } else { cwd.join(p) })
        })
        .filter(|p| p.is_file() && classify(p).is_some())
        .collect()
}

/// Paths no window has taken yet.
static PENDING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn pending() -> std::sync::MutexGuard<'static, Vec<PathBuf>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}

/// Queue paths for the UI (no window told: the one that starts takes them).
pub fn queue(paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    let mut q = pending();
    for p in paths {
        if !q.contains(&p) {
            q.push(p);
        }
    }
}

/// Paths from the OS while running: queued, and a window (`label`'s, else
/// the target one) is told and brought forward.
pub fn received(app: &AppHandle, paths: Vec<PathBuf>, label: Option<&str>) {
    let paths: Vec<PathBuf> = paths.into_iter().filter(|p| p.is_file()).collect();
    if paths.is_empty() {
        return;
    }
    tracing::info!(count = paths.len(), "files to open");
    queue(paths);
    let win = label.and_then(|l| app.get_webview_window(l)).or_else(|| crate::windows::target_window(app));
    match win {
        Some(w) => {
            let _ = w.emit("open-files", ());
            crate::windows::bring_forward(&w);
        }
        // No window (macOS, all closed): a new one takes them when it starts.
        None => crate::windows::spawn_new(app),
    }
}

/// A second launch's command line (Windows, Linux): true when it carried
/// files, which then open in this process instead of a new window.
#[cfg_attr(any(target_os = "macos", debug_assertions), allow(dead_code))]
pub fn second_launch(app: &AppHandle, argv: &[String], cwd: &str) -> bool {
    let paths = paths_from_args(argv.iter().skip(1).cloned(), Path::new(cwd));
    if paths.is_empty() {
        return false;
    }
    received(app, paths, None);
    true
}

/// What the UI gets for a file: the connection to show and, for a data
/// file, the query to run.
#[derive(Debug, Serialize)]
pub struct OpenedFile {
    pub connection_id: String,
    pub kind: Kind,
    /// The file's name.
    pub name: String,
    /// It already had a connection: just focus it.
    pub existing: bool,
    /// Data files: `SELECT * FROM "<view>" LIMIT 1000`.
    pub sql: Option<String>,
}

/// The connection for `path`: the one it has, or a new temporary one.
pub fn open(store: &dbine_core::StateStore, path: &Path) -> CommandResult<OpenedFile> {
    if !path.is_file() {
        return Err(CommandError::NotFound(format!("no existe el archivo «{}»", path.display())));
    }
    let kind = classify(path).ok_or_else(|| CommandError::BadRequest(format!("DBine no abre este tipo de archivo: «{}»", path.display())))?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let sql = (kind == Kind::Data).then(|| data_stem(path).map(|v| first_query(&v))).flatten();
    let mut known = store.list_temporary_connections()?;
    known.extend(store.list_connections()?);
    if let Some(c) = existing(&known, path, kind) {
        return Ok(OpenedFile { connection_id: c.id.clone(), kind, name, existing: true, sql });
    }
    // Absolute, as given (not canonicalized: on Windows that is a `\\?\`
    // path some readers don't take).
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let conn = store.add_temporary_connection(&connection_for(&absolute, kind))?;
    Ok(OpenedFile { connection_id: conn.id, kind, name, existing: false, sql })
}

#[tauri::command]
pub fn open_files_take() -> Vec<String> {
    take_pending()
}

fn take_pending() -> Vec<String> {
    std::mem::take(&mut *pending()).into_iter().map(|p| p.to_string_lossy().into_owned()).collect()
}

#[derive(Deserialize)]
pub struct OpenFileArgs {
    pub path: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn open_file(state: State<'_, AppState>, args: OpenFileArgs) -> CommandResult<OpenedFile> {
    open(&state.store, Path::new(&args.path))
}

/// Attach the native drop hooks to a window (each window, when created).
pub fn attach(win: &tauri::WebviewWindow) {
    drop::attach(win);
}

/// Files dropped on a window. The webview keeps its own drag and drop (the
/// explorer's), which never gives the page a file's path; so the path is
/// read natively, next to it, and the page only shows the "Soltá para
/// abrir" overlay. Only drags from other apps with files count.
mod drop {
    #[cfg(target_os = "macos")]
    pub fn attach(win: &tauri::WebviewWindow) {
        use tauri::Manager;
        let app = win.app_handle().clone();
        // Runs once the webview exists: its class is the one to wrap.
        let _ = win.with_webview(move |pw| mac::install(&app, pw.inner().cast()));
    }

    #[cfg(windows)]
    pub fn attach(win: &tauri::WebviewWindow) {
        windows_drop::attach(win);
    }

    #[cfg(target_os = "linux")]
    pub fn attach(win: &tauri::WebviewWindow) {
        linux::attach(win);
    }

    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    pub fn attach(_win: &tauri::WebviewWindow) {}

    /// macOS: wry's webview class (`WryWebView`) hands
    /// `performDragOperation:` to WebKit when Tauri's own handler is off; it
    /// is wrapped (once, for the class, taken from a live webview) to read the
    /// dragged files' paths first, then call the original, so the page still
    /// gets its `drop` event.
    #[cfg(target_os = "macos")]
    mod mac {
        use objc2::runtime::{AnyClass, AnyObject, Bool, Imp, Sel};
        use objc2::{msg_send, sel};
        use objc2_foundation::NSString;
        use std::path::PathBuf;
        use std::sync::OnceLock;
        use tauri::{AppHandle, Manager};

        static APP: OnceLock<AppHandle> = OnceLock::new();
        static ORIGINAL: OnceLock<Imp> = OnceLock::new();

        type Perform = unsafe extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject) -> Bool;

        /// Once per process (every window's webview is of the same class).
        /// Runs on the main thread (`with_webview`).
        pub fn install(app: &AppHandle, webview: *mut AnyObject) {
            if ORIGINAL.get().is_some() || webview.is_null() {
                return;
            }
            let _ = APP.set(app.clone());
            // SAFETY: a live WKWebView (wry's subclass) handed by Tauri.
            let class: &AnyClass = unsafe { &*webview }.class();
            // Only wry's own subclass overrides it; never WebKit's class.
            if class.name().to_bytes() == b"WKWebView" {
                tracing::warn!("open files: unexpected webview class; dropped files won't open");
                return;
            }
            let Some(method) = class.instance_method(sel!(performDragOperation:)) else {
                tracing::warn!("open files: webview lacks performDragOperation:");
                return;
            };
            let ours: Perform = perform_drag_operation;
            // SAFETY: same signature as the method replaced (an NSView's
            // `-(BOOL)performDragOperation:(id<NSDraggingInfo>)`); the
            // original is kept and always called.
            let original = unsafe { method.set_implementation(std::mem::transmute::<Perform, Imp>(ours)) };
            let _ = ORIGINAL.set(original);
            tracing::debug!("open files: drop hook installed");
        }

        unsafe extern "C-unwind" fn perform_drag_operation(this: *mut AnyObject, sel: Sel, info: *mut AnyObject) -> Bool {
            let paths = unsafe { external_paths(info) };
            if !paths.is_empty() {
                if let Some(app) = APP.get() {
                    let ns_window: *mut AnyObject = unsafe { msg_send![this, window] };
                    let label = app
                        .webview_windows()
                        .into_iter()
                        .find(|(_, w)| w.ns_window().map(|p| p as *mut AnyObject == ns_window).unwrap_or(false))
                        .map(|(l, _)| l);
                    super::super::received(app, paths, label.as_deref());
                }
            }
            match ORIGINAL.get() {
                // SAFETY: the implementation `install` replaced, same signature.
                Some(imp) => unsafe { std::mem::transmute::<Imp, Perform>(*imp)(this, sel, info) },
                None => Bool::NO,
            }
        }

        /// The files of a drag from another app (Finder…); none for the
        /// page's own drags (they have a source in this app).
        unsafe fn external_paths(info: *mut AnyObject) -> Vec<PathBuf> {
            if info.is_null() {
                return Vec::new();
            }
            let source: *mut AnyObject = unsafe { msg_send![info, draggingSource] };
            if !source.is_null() {
                return Vec::new();
            }
            let pb: *mut AnyObject = unsafe { msg_send![info, draggingPasteboard] };
            if pb.is_null() {
                return Vec::new();
            }
            let kind = NSString::from_str("NSFilenamesPboardType");
            let list: *mut AnyObject = unsafe { msg_send![pb, propertyListForType: &*kind] };
            if list.is_null() {
                return Vec::new();
            }
            let count: usize = unsafe { msg_send![list, count] };
            (0..count)
                .filter_map(|i| {
                    let s: *mut AnyObject = unsafe { msg_send![list, objectAtIndex: i] };
                    if s.is_null() {
                        return None;
                    }
                    let utf8: *const std::ffi::c_char = unsafe { msg_send![s, UTF8String] };
                    (!utf8.is_null()).then(|| PathBuf::from(unsafe { std::ffi::CStr::from_ptr(utf8) }.to_string_lossy().into_owned()))
                })
                .collect()
        }
    }

    /// Windows: WebView2 gives the page dropped `File`s without a path, but
    /// hands their paths to the app when the page posts them with
    /// `chrome.webview.postMessageWithAdditionalObjects` (web/src/composables/
    /// openFiles.ts). Only `File` objects count, which only a drop (or a file
    /// picker) gives a page.
    #[cfg(windows)]
    mod windows_drop {
        use std::path::PathBuf;
        use tauri::Manager;
        use webview2_com::Microsoft::Web::WebView2::Win32::{
            ICoreWebView2File, ICoreWebView2WebMessageReceivedEventArgs2,
        };
        use webview2_com::WebMessageReceivedEventHandler;
        use windows::core::{Interface, PWSTR};

        /// The message the page posts with the files.
        const MESSAGE: &str = "dbine-open-files";

        pub fn attach(win: &tauri::WebviewWindow) {
            let app = win.app_handle().clone();
            let label = win.label().to_string();
            let _ = win.with_webview(move |pw| unsafe {
                let Ok(core) = pw.controller().CoreWebView2() else { return };
                let handler = WebMessageReceivedEventHandler::create(Box::new(move |_, args| {
                    let Some(args) = args else { return Ok(()) };
                    let mut text = PWSTR::null();
                    if args.TryGetWebMessageAsString(&mut text).is_err() {
                        return Ok(());
                    }
                    if webview2_com::take_pwstr(text) != MESSAGE {
                        return Ok(());
                    }
                    let Ok(args) = args.cast::<ICoreWebView2WebMessageReceivedEventArgs2>() else { return Ok(()) };
                    let Ok(objects) = args.AdditionalObjects() else { return Ok(()) };
                    let mut count = 0u32;
                    let _ = objects.Count(&mut count);
                    let mut paths = Vec::new();
                    for i in 0..count {
                        let Ok(file) = objects.GetValueAtIndex(i).and_then(|o| o.cast::<ICoreWebView2File>()) else { continue };
                        let mut path = PWSTR::null();
                        if file.Path(&mut path).is_ok() {
                            paths.push(PathBuf::from(webview2_com::take_pwstr(path)));
                        }
                    }
                    super::super::received(&app, paths, Some(&label));
                    Ok(())
                }));
                let mut token = 0i64;
                if let Err(e) = core.add_WebMessageReceived(&handler, &mut token) {
                    tracing::warn!("open files: drop handler failed: {e}");
                }
            });
        }
    }

    /// Linux: WebKitGTK reads a drop's data from GTK; the file URIs are
    /// read from the same data. WebKit asks for it while the drag moves (or
    /// at the drop), so it is kept until the drop, and dropped if the drag
    /// leaves without one.
    #[cfg(target_os = "linux")]
    mod linux {
        use gtk::glib;
        use gtk::prelude::*;
        use std::cell::{Cell, RefCell};
        use std::path::PathBuf;
        use std::rc::Rc;
        use tauri::Manager;

        #[derive(Default)]
        struct Drag {
            paths: RefCell<Vec<PathBuf>>,
            /// The drop came before its data.
            dropping: Cell<bool>,
            /// Bumped on every enter/leave: a delayed clear only clears its own drag.
            seq: Cell<u64>,
        }

        pub fn attach(win: &tauri::WebviewWindow) {
            let app = win.app_handle().clone();
            let label = win.label().to_string();
            let _ = win.with_webview(move |pw| {
                let view = pw.inner();
                let drag = Rc::new(Drag::default());
                let deliver = {
                    let app = app.clone();
                    let label = label.clone();
                    move |paths: Vec<PathBuf>| {
                        if !paths.is_empty() {
                            super::super::received(&app, paths, Some(&label));
                        }
                    }
                };
                {
                    let drag = drag.clone();
                    let deliver = deliver.clone();
                    view.connect_drag_data_received(move |_, _, _, _, data, _, _| {
                        // Only files: the page's own drags carry none.
                        let paths: Vec<PathBuf> = data
                            .uris()
                            .iter()
                            .filter_map(|u| glib::filename_from_uri(u).ok().map(|(p, _)| p))
                            .collect();
                        if drag.dropping.replace(false) {
                            deliver(paths);
                        } else {
                            *drag.paths.borrow_mut() = paths;
                        }
                    });
                }
                {
                    let drag = drag.clone();
                    view.connect_drag_motion(move |_, _, _, _, _| {
                        drag.seq.set(drag.seq.get() + 1);
                        false
                    });
                }
                {
                    let drag = drag.clone();
                    view.connect_drag_leave(move |_, _, _| {
                        // GTK sends a leave right before the drop too: clear
                        // a little later, unless the drag came back.
                        let seq = drag.seq.get();
                        let drag = drag.clone();
                        glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
                            if drag.seq.get() == seq {
                                drag.paths.borrow_mut().clear();
                                drag.dropping.set(false);
                            }
                        });
                    });
                }
                {
                    let drag = drag.clone();
                    view.connect_drag_drop(move |_, _, _, _, _| {
                        let paths = std::mem::take(&mut *drag.paths.borrow_mut());
                        if paths.is_empty() {
                            drag.dropping.set(true);
                        } else {
                            deliver(paths);
                        }
                        // WebKit still handles it: the page gets its `drop`.
                        false
                    });
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dbine-open-files-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn data_files_and_their_view_names() {
        assert_eq!(data_stem(Path::new("/d/ventas.csv")).as_deref(), Some("ventas"));
        assert_eq!(data_stem(Path::new("/d/Ventas 2024.CSV.gz")).as_deref(), Some("Ventas 2024"));
        assert_eq!(data_stem(Path::new("/d/log.ndjson")).as_deref(), Some("log"));
        assert_eq!(data_stem(Path::new("/d/libro.xlsx")).as_deref(), Some("libro"));
        assert_eq!(data_stem(Path::new("/d/x.parquet.gz")), None);
        assert_eq!(data_stem(Path::new("/d/.csv")), None);
        assert_eq!(data_stem(Path::new("/d/notas.txt")), None);
    }

    #[test]
    fn the_first_query_quotes_the_view() {
        assert_eq!(first_query("ventas"), r#"SELECT * FROM "ventas" LIMIT 1000"#);
        assert_eq!(first_query(r#"a"; DROP TABLE x; --"#), r#"SELECT * FROM "a""; DROP TABLE x; --" LIMIT 1000"#);
    }

    #[test]
    fn files_are_told_apart_by_content_then_extension() {
        let d = temp_dir("kinds");
        let w = |name: &str, bytes: &[u8]| {
            let p = d.join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        };
        let mut sqlite = b"SQLite format 3\0".to_vec();
        sqlite.extend([0u8; 84]);
        let mut duck = vec![0u8; 8];
        duck.extend(b"DUCK");
        duck.extend([0u8; 100]);
        assert_eq!(classify(&w("a.sqlite", &sqlite)), Some(Kind::Sqlite));
        assert_eq!(classify(&w("b.db", &sqlite)), Some(Kind::Sqlite));
        assert_eq!(classify(&w("c.db", &duck)), Some(Kind::Duckdb));
        assert_eq!(classify(&w("d.duckdb", &duck)), Some(Kind::Duckdb));
        // A new, empty database: the extension says.
        assert_eq!(classify(&w("e.sqlite3", b"")), Some(Kind::Sqlite));
        assert_eq!(classify(&w("f.duckdb", b"")), Some(Kind::Duckdb));
        // Someone else's .db (Thumbs.db) or a mislabeled file isn't opened.
        assert_eq!(classify(&w("Thumbs.db", b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1 not sqlite")), None);
        assert_eq!(classify(&w("g.sqlite", b"hello, this is plain text")), None);
        assert_eq!(classify(&w("h.csv", b"a,b\n1,2\n")), Some(Kind::Data));
        assert_eq!(classify(&w("i.parquet", b"PAR1")), Some(Kind::Data));
        assert_eq!(classify(&w("j.txt", b"x")), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn command_lines_keep_only_files_dbine_opens() {
        let d = temp_dir("args");
        std::fs::write(d.join("ventas.csv"), "a\n1\n").unwrap();
        std::fs::write(d.join("leeme.txt"), "x").unwrap();
        std::fs::create_dir_all(d.join("carpeta.csv")).unwrap();
        let abs = d.join("ventas.csv").to_string_lossy().into_owned();
        let url = tauri::Url::from_file_path(d.join("ventas.csv")).unwrap().to_string();
        let args = ["--new-window", "--run-task", "abc", "ventas.csv", "leeme.txt", "carpeta.csv", "no-existe.csv", &abs, &url]
            .map(String::from);
        let got = paths_from_args(args, &d);
        assert_eq!(got, [d.join("ventas.csv"), d.join("ventas.csv"), d.join("ventas.csv")]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_queue_keeps_each_path_once_until_taken() {
        let _ = take_pending();
        queue(vec![PathBuf::from("/a.csv"), PathBuf::from("/b.csv")]);
        queue(vec![PathBuf::from("/a.csv")]);
        assert_eq!(take_pending(), ["/a.csv", "/b.csv"]);
        assert!(take_pending().is_empty());
    }

    #[test]
    fn opening_a_file_twice_focuses_its_connection() {
        let d = temp_dir("open");
        let store = dbine_core::StateStore::open_in_memory().unwrap();
        let csv = d.join("it's \"ventas\".csv");
        std::fs::write(&csv, "a\n1\n").unwrap();
        let first = open(&store, &csv).unwrap();
        assert!(!first.existing);
        assert_eq!(first.kind, Kind::Data);
        assert_eq!(first.sql.as_deref(), Some(r#"SELECT * FROM "it's ""ventas""" LIMIT 1000"#));
        let conn = store.get_connection(&first.connection_id).unwrap().unwrap();
        assert_eq!(conn.name, "it's \"ventas\".csv");
        assert_eq!(conn.config.driver, "duckdb_files");
        assert_eq!(conn.config.option("single"), Some("true"));
        // Not saved: only a temporary connection.
        assert!(store.list_connections().unwrap().is_empty());
        // The same file, reached another way.
        let again = open(&store, &d.join(".").join("it's \"ventas\".csv")).unwrap();
        assert!(again.existing);
        assert_eq!(again.connection_id, first.connection_id);
        // A saved connection to the file counts too.
        let db = d.join("datos.sqlite");
        std::fs::write(&db, b"").unwrap();
        let mut saved = connection_for(&db, Kind::Sqlite);
        saved.name = "Mis datos".into();
        store.save_connection(&saved).unwrap();
        let opened = open(&store, &db).unwrap();
        assert!(opened.existing && opened.sql.is_none());
        assert_eq!(opened.connection_id, saved.id);
        assert!(matches!(open(&store, &d.join("nada.csv")), Err(CommandError::NotFound(_))));
        std::fs::write(d.join("leeme.txt"), "x").unwrap();
        assert!(matches!(open(&store, &d.join("leeme.txt")), Err(CommandError::BadRequest(_))));
        let _ = std::fs::remove_dir_all(&d);
    }
}
