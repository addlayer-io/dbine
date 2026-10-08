//! "Documentar la base…" (docs/documentar-la-base.md): the data dictionary
//! of a database written to one file (`crate::dbdocs` reads and renders
//! it). On a read-only session of its own, cancellable with `cancel_query`
//! on `docs:<id>`, with progress as `dbdocs-progress`. "Abrir" and "Mostrar
//! en la carpeta" only reach files this run of DBine wrote.

use crate::commands::schema::driver_of;
use crate::dbdocs::{self, Context, DocOptions, Labels};
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};
use tauri::{AppHandle, Emitter, State};

#[derive(Deserialize)]
pub struct OutlineArgs {
    pub connection_id: String,
    pub database: String,
}

#[derive(Serialize)]
pub struct DocsOutline {
    /// The database's schemas (empty on engines without them).
    pub schemas: Vec<String>,
    /// Objects per kind, to offer only what the database has.
    pub kinds: BTreeMap<String, usize>,
    /// The catalog reports foreign keys (links and the diagram's lines).
    pub foreign_keys: bool,
    /// "Usada por" can be worked out.
    pub dependencies: bool,
}

/// What the dialog offers for a database.
#[tauri::command(rename_all = "camelCase")]
pub async fn dbdocs_outline(state: State<'_, AppState>, args: OutlineArgs) -> CommandResult<DocsOutline> {
    let driver = driver_of(&state, &args.connection_id)?;
    let (objects, schemas) = state
        .meta_read(&args.connection_id, &args.database, crate::commands::explorer::META_LIMIT, |s| {
            Box::pin(async move {
                let objects = s.list_objects().await?;
                let schemas = s.list_schemas().await.ok().flatten().unwrap_or_default();
                Ok((objects, schemas))
            })
        })
        .await?;
    let mut names: BTreeSet<String> = objects.iter().filter_map(|o| o.schema.clone()).filter(|s| !s.is_empty()).collect();
    names.extend(schemas.into_iter().filter(|s| !s.system).map(|s| s.name));
    let mut kinds = BTreeMap::new();
    for o in &objects {
        *kinds.entry(o.kind.clone()).or_insert(0) += 1;
    }
    Ok(DocsOutline { schemas: names.into_iter().collect(), kinds, foreign_keys: driver.capabilities().foreign_keys, dependencies: driver.supports_dependencies() })
}

#[derive(Deserialize)]
pub struct GenerateArgs {
    pub connection_id: String,
    pub database: String,
    /// The UI's id: its events and `cancel_query` (`docs:<id>`).
    pub run_id: String,
    pub path: String,
    #[serde(default)]
    pub options: DocOptions,
}

#[derive(Serialize)]
pub struct Generated {
    pub path: String,
    pub tables: usize,
    pub objects: usize,
    pub bytes: u64,
    /// What couldn't be read (also written in the document).
    pub notes: Vec<String>,
}

#[derive(Serialize, Clone)]
struct Progress<'a> {
    run_id: &'a str,
    done: usize,
    total: usize,
    /// A `dbDocs:phase.*` key.
    phase: &'a str,
}

/// The document, ready to write.
pub(crate) struct Built {
    pub text: String,
    pub tables: usize,
    pub objects: usize,
    pub notes: Vec<String>,
}

/// Read and render the documentation of a database on a session of its own
/// under `key` (removed at the end). Shared by the dialog and the scheduled
/// step.
pub(crate) async fn document(
    state: &AppState,
    key: &str,
    connection_id: &str,
    database: &str,
    opts: &DocOptions,
    progress: &(dyn Fn(usize, usize, &'static str) + Send + Sync),
) -> CommandResult<Built> {
    let driver = driver_of(state, connection_id)?;
    let connection = state.store.get_connection(connection_id)?.map(|c| c.name).unwrap_or_default();
    let labels = Labels::new(&opts.labels);
    let entry = state.dedicated_session(key, connection_id, database, true).await?;
    let result = {
        let mut s = entry.session.lock().await;
        let cancelled = || entry.cancelled.load(Ordering::Relaxed);
        let cx = Context { database, connection: &connection, progress, cancelled: &cancelled };
        tokio::select! {
            r = dbdocs::collect(&mut **s, driver.as_ref(), opts, &labels, &cx) => r,
            _ = entry.cancel.notified() => Err(CommandError::Cancelled),
        }
    };
    state.sessions.remove(key);
    let doc = result?;
    Ok(Built { text: dbdocs::render(&doc, opts.format, &labels), tables: doc.table_count(), objects: doc.object_count(), notes: doc.notes })
}

/// `path` with the format's extension when it has none of its own.
pub(crate) fn with_extension(path: &Path, format: dbdocs::DocFormat) -> PathBuf {
    let ext = format.extension();
    let has = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case(ext) || (ext == "md" && e.eq_ignore_ascii_case("markdown")) || (ext == "html" && e.eq_ignore_ascii_case("htm")));
    if has {
        path.to_path_buf()
    } else {
        let mut p = path.as_os_str().to_owned();
        p.push(format!(".{ext}"));
        PathBuf::from(p)
    }
}

/// Files written by this run of DBine: the only ones "Abrir" opens.
fn written() -> &'static Mutex<HashSet<PathBuf>> {
    static W: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    W.get_or_init(Default::default)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn dbdocs_generate(app: AppHandle, state: State<'_, AppState>, args: GenerateArgs) -> CommandResult<Generated> {
    if args.path.trim().is_empty() {
        return Err(CommandError::BadRequest("falta el archivo de destino".into()));
    }
    let path = with_extension(Path::new(args.path.trim()), args.options.format);
    let run_id = args.run_id.clone();
    let emit = move |done: usize, total: usize, phase: &'static str| {
        let _ = app.emit("dbdocs-progress", Progress { run_id: &run_id, done, total, phase });
    };
    let built = document(&state, &format!("docs:{}", args.run_id), &args.connection_id, &args.database, &args.options, &emit).await?;
    std::fs::write(&path, &built.text).map_err(|e| CommandError::Internal(format!("no se pudo escribir {}: {e}", path.display())))?;
    if let Ok(mut w) = written().lock() {
        w.insert(path.clone());
    }
    Ok(Generated { path: path.to_string_lossy().into_owned(), tables: built.tables, objects: built.objects, bytes: built.text.len() as u64, notes: built.notes })
}

#[derive(Deserialize)]
pub struct FileArgs {
    pub path: String,
    /// Show it in its folder instead of opening it.
    #[serde(default)]
    pub reveal: bool,
}

/// "Abrir" (the system's app for the file) or "Mostrar en la carpeta".
#[tauri::command(rename_all = "camelCase")]
pub async fn dbdocs_open(app: AppHandle, args: FileArgs) -> CommandResult<()> {
    use tauri_plugin_opener::OpenerExt;
    let path = PathBuf::from(&args.path);
    if !written().lock().map(|w| w.contains(&path)).unwrap_or(false) {
        return Err(CommandError::BadRequest("ese archivo no lo generó DBine".into()));
    }
    let r = if args.reveal { app.opener().reveal_item_in_dir(&path) } else { app.opener().open_path(path.to_string_lossy(), None::<&str>) };
    r.map_err(|e| CommandError::Internal(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dbdocs::DocFormat;
    use dbine_core::{SavedConnection, StateStore};
    use dbine_driver::ConnectionConfig;

    #[test]
    fn extensions() {
        assert_eq!(with_extension(Path::new("/tmp/x"), DocFormat::Html), PathBuf::from("/tmp/x.html"));
        assert_eq!(with_extension(Path::new("/tmp/x.HTM"), DocFormat::Html), PathBuf::from("/tmp/x.HTM"));
        assert_eq!(with_extension(Path::new("/tmp/x.v1"), DocFormat::Markdown), PathBuf::from("/tmp/x.v1.md"));
    }

    /// A SQLite file with keys, an index, a check, a view and a trigger,
    /// documented in both formats.
    #[tokio::test]
    async fn documents_a_sqlite_file() {
        let dir = std::env::temp_dir().join(format!("dbine-dbdocs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("shop.sqlite");
        rusqlite::Connection::open(&file)
            .unwrap()
            .execute_batch(
                "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT UNIQUE, CHECK (length(name) > 0));
                 CREATE TABLE \"orders<x>\" (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL REFERENCES customers(id) ON DELETE CASCADE, total REAL DEFAULT 0);
                 CREATE INDEX ix_orders_customer ON \"orders<x>\" (customer_id);
                 CREATE VIEW big_customers AS SELECT c.name FROM customers c JOIN \"orders<x>\" o ON o.customer_id = c.id WHERE o.total > 100;
                 CREATE TRIGGER trg_customers AFTER INSERT ON customers BEGIN SELECT 1; END;",
            )
            .unwrap();
        let state = AppState::new(StateStore::open_in_memory().unwrap());
        state
            .store
            .save_connection(&SavedConnection {
                id: "s".into(),
                name: "tienda".into(),
                color: None,
                config: ConnectionConfig { driver: "sqlite".into(), host: file.to_string_lossy().into_owned(), ..Default::default() },
                save_password: false,
                folder_id: None,
                tags: vec![],
                mcp_level: None,
                updated_at: String::new(),
            })
            .unwrap();

        let steps = Mutex::new(Vec::new());
        let progress = |d: usize, t: usize, p: &'static str| steps.lock().unwrap().push((d, t, p));
        let built = document(&state, "docs:test", "s", "main", &DocOptions::default(), &progress).await.unwrap();
        assert!(state.sessions.get("docs:test").is_none());
        assert_eq!(built.tables, 2, "{}", built.text);
        assert!(built.objects >= 2, "view and trigger: {}", built.objects);
        let html = &built.text;
        // To look at it: DBINE_DOCS_DUMP=/tmp/doc.html
        if let Ok(p) = std::env::var("DBINE_DOCS_DUMP") {
            let _ = std::fs::write(p, html);
        }
        assert!(html.contains("tienda") && html.contains("SQLite"));
        assert!(html.contains("orders&lt;x&gt;") && !html.contains("orders<x>"));
        assert!(html.contains("ix_orders_customer"));
        assert!(html.contains("CASCADE"));
        assert!(html.contains("<svg class=\"erd-svg\""));
        assert!(html.contains("big_customers") && html.contains("trg_customers"));
        // "Usada por": the orders' FK and the view's code.
        let used = &html[html.find("Usada por").expect("used by")..];
        assert!(used.contains("big_customers"), "{used}");
        assert!(steps.lock().unwrap().iter().any(|s| s.2 == "source"));

        let md_opts = DocOptions { format: DocFormat::Markdown, source: false, triggers: false, ..Default::default() };
        let built = document(&state, "docs:test2", "s", "main", &md_opts, &progress).await.unwrap();
        let md = &built.text;
        assert!(md.starts_with("# Diccionario de datos"));
        assert!(md.contains("orders&lt;x&gt;") && !md.contains("<svg"));
        assert!(!md.contains("```") && !md.contains("trg_customers"), "{md}");

        // As a scheduled step: the file lands in the folder, named by variables.
        let task = dbine_core::tasks::ScheduledTask {
            id: "t".into(),
            name: "docs".into(),
            enabled: true,
            steps: vec![dbine_core::tasks::Step {
                id: "d".into(),
                kind: dbine_core::tasks::kinds::DOCUMENT.into(),
                config: serde_json::json!({"connection_id": "s", "database": "main", "folder": dir.join("out").to_string_lossy(), "file_name": "{task}-dic", "options": {"format": "markdown"}}),
                ..Default::default()
            }],
            notify: dbine_core::tasks::Notify::Never,
            ..Default::default()
        };
        let run = crate::tasks::run_task(&state, None, &task, "manual").await;
        assert_eq!(run.status, dbine_core::tasks::RunStatus::Ok, "{run:#?}");
        assert_eq!(run.steps[0].outputs["tables"], "2");
        assert!(std::fs::read_to_string(dir.join("out").join("docs-dic.md")).unwrap().contains("customers"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Against the dbine-test-postgres container (`DBINE_TEST_POSTGRES_URL`
    /// replaces `postgres://postgres:pw@localhost:25010`):
    /// `cargo test -p dbine --lib -- --ignored dbdocs_postgres`.
    #[tokio::test]
    #[ignore]
    async fn dbdocs_postgres() {
        let url = std::env::var("DBINE_TEST_POSTGRES_URL").unwrap_or_else(|_| "postgres://postgres:pw@localhost:25010".into());
        let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
        let (auth, hostport) = rest.split('/').next().unwrap().rsplit_once('@').unwrap();
        let (user, pass) = auth.split_once(':').unwrap();
        let (host, port) = hostport.rsplit_once(':').unwrap();
        let cfg = ConnectionConfig {
            driver: "postgres".into(),
            host: host.into(),
            port: port.parse().unwrap(),
            username: Some(user.into()),
            password: Some(pass.into()),
            database: "postgres".into(),
            ..Default::default()
        };
        let driver = dbine_drivers::find("postgres").unwrap().clone();
        let mut s = driver.connect(&cfg, Some("postgres")).await.unwrap();
        let setup = "DROP SCHEMA IF EXISTS dbine_e2e_docs CASCADE;
            CREATE SCHEMA dbine_e2e_docs;
            CREATE TABLE dbine_e2e_docs.customers (id int PRIMARY KEY, name text NOT NULL CHECK (name <> ''));
            COMMENT ON TABLE dbine_e2e_docs.customers IS 'Clientes <b>activos</b>';
            COMMENT ON COLUMN dbine_e2e_docs.customers.name IS 'Nombre & apellido';
            CREATE TABLE dbine_e2e_docs.orders (id int PRIMARY KEY, customer_id int REFERENCES dbine_e2e_docs.customers(id) ON DELETE CASCADE, total numeric(10,2) DEFAULT 0);
            CREATE INDEX ix_orders_customer ON dbine_e2e_docs.orders (customer_id);
            CREATE VIEW dbine_e2e_docs.v_big AS SELECT c.name FROM dbine_e2e_docs.customers c JOIN dbine_e2e_docs.orders o ON o.customer_id = c.id WHERE o.total > 100;
            CREATE FUNCTION dbine_e2e_docs.order_count() RETURNS bigint LANGUAGE sql AS 'SELECT count(*) FROM dbine_e2e_docs.orders';
            CREATE SEQUENCE dbine_e2e_docs.seq_x;";
        let mut out = dbine_driver::QueryOutcome::default();
        s.execute(setup, 10, &mut out).await.unwrap();
        assert!(out.error.is_none() && out.errors.is_empty(), "{:?} {:?}", out.error, out.errors.first().map(|e| e.message.clone()));

        let opts = DocOptions { schemas: vec!["dbine_e2e_docs".into()], ..Default::default() };
        let labels = Labels::new(&opts.labels);
        let cx = Context { database: "postgres", connection: "pg", progress: &|_, _, _| {}, cancelled: &|| false };
        let doc = dbdocs::collect(&mut *s, driver.as_ref(), &opts, &labels, &cx).await;
        let mut out = dbine_driver::QueryOutcome::default();
        let _ = s.execute("DROP SCHEMA IF EXISTS dbine_e2e_docs CASCADE", 10, &mut out).await;
        let doc = doc.unwrap();
        assert_eq!(doc.schemas.len(), 1, "only the schema asked for");
        assert_eq!(doc.table_count(), 2);
        let html = dbdocs::render(&doc, dbdocs::DocFormat::Html, &labels);
        assert!(html.contains("Clientes &lt;b&gt;activos&lt;/b&gt;") && html.contains("Nombre &amp; apellido"), "comments");
        assert!(html.contains("ix_orders_customer") && html.contains("CASCADE") && html.contains("<svg class=\"erd-svg\""));
        assert!(html.contains("v_big") && html.contains("order_count") && html.contains("seq_x"));
        let used = &html[html.find("Usada por").expect("used by")..];
        assert!(used.contains("orders") && used.contains("v_big"), "{used}");
        assert!(doc.version.is_some());
    }
}
