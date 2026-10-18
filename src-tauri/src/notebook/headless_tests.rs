//! Tests for the headless notebook run (`notebook::headless`) and the
//! "Ejecutar notebook" step. SQLite and DuckDB run always (DuckDB loads as
//! the app does: downloaded once into the components folder, or
//! `DBINE_DUCKDB_LIB`). The live test against the `dbine-test-*` containers
//! is ignored by default:
//!
//! ```sh
//! cargo test -p dbine --lib -- --ignored notebook_headless_live --test-threads=1 --nocapture
//! ```

use super::*;
use crate::notebook::{serialize, PreviewLimits};
use dbine_core::tasks::{kinds, Notify, RunStatus, ScheduledTask, Step};
use dbine_core::StateStore;
use dbine_driver::ConnectionConfig;
use serde_json::json;

struct World {
    state: AppState,
    dir: PathBuf,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn save_conn(state: &AppState, id: &str, name: &str, config: ConnectionConfig, tags: Vec<String>) {
    state
        .store
        .save_connection(&SavedConnection {
            id: id.into(),
            name: name.into(),
            color: None,
            config,
            save_password: false,
            folder_id: None,
            tags,
            mcp_level: None,
            updated_at: String::new(),
        })
        .unwrap();
}

/// A SQLite file with sales ("Ventas"), an in-memory DuckDB ("Clientes")
/// and a CSV of zones next to where the notebook goes.
fn world(tag: &str, sales_tags: Vec<String>) -> World {
    // Not the system's temp folder: on macOS it is under /var, which the
    // step's policy (the real one) blocks.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/nbheadless-tests").join(format!("{tag}-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    let state = AppState::new(StateStore::open_in_memory().unwrap());
    let db = dir.join("ventas.sqlite");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch("CREATE TABLE ventas (id INTEGER, total REAL); INSERT INTO ventas VALUES (1, 10.5), (2, 3), (1, 4);")
        .unwrap();
    save_conn(&state, "sq", "Ventas", ConnectionConfig { driver: "sqlite".into(), host: db.to_string_lossy().into_owned(), ..Default::default() }, sales_tags);
    save_conn(&state, "dk", "Clientes", ConnectionConfig { driver: "duckdb".into(), host: ":memory:".into(), ..Default::default() }, vec![]);
    std::fs::write(dir.join("zonas.csv"), "id,zona\n1,norte\n2,sur\n").unwrap();
    World { state, dir }
}

fn cell(id: &str, name: &str, target: Option<NotebookTarget>, sql: &str) -> NotebookCell {
    NotebookCell {
        id: id.into(),
        kind: CellKind::Sql,
        source: sql.into(),
        language: Some("sql".into()),
        target,
        name: Some(name.into()),
        keep_result: true,
        chart: None,
        execution_count: None,
        result: None,
        raw: None,
    }
}

fn text_cell(id: &str, text: &str) -> NotebookCell {
    NotebookCell { kind: CellKind::Markdown, name: None, language: None, ..cell(id, "", None, text) }
}

fn on(conn: &str) -> Option<NotebookTarget> {
    Some(NotebookTarget { connection: Some(conn.into()), ..Default::default() })
}

fn dataframes() -> Option<NotebookTarget> {
    Some(NotebookTarget { results: true, ..Default::default() })
}

fn write_nb(path: &Path, cells: Vec<NotebookCell>) {
    let nb = Notebook {
        target: None,
        save_results: true,
        save_results_set: true,
        cells,
        parameters: vec![],
        origin: "dbine".into(),
        extra_metadata: json!({}),
        warnings: vec![],
    };
    std::fs::write(path, serialize(&nb, PreviewLimits::default())).unwrap();
}

/// The report: sales from SQLite, customers from DuckDB, a text cell and a
/// DataFrame SQL join of both with a CSV.
fn report(w: &World) -> PathBuf {
    let path = w.dir.join("reporte.ipynb");
    let mut chart = cell("c3", "c3", dataframes(), "SELECT c.nombre, sum(v.total) AS total, max(z.zona) AS zona\nFROM c1 v JOIN c2 c USING (id) JOIN read_csv('zonas.csv') z USING (id)\nGROUP BY ALL ORDER BY 1");
    chart.chart = Some(json!({"type": "bar", "x": "nombre", "y": ["total"]}));
    write_nb(
        &path,
        vec![
            text_cell("t1", "# Ventas por cliente"),
            cell("c1", "c1", on("Ventas"), "SELECT id, total FROM ventas"),
            cell("c2", "c2", on("Clientes"), "SELECT 1 AS id, 'Ana' AS nombre UNION ALL SELECT 2, 'Beto'"),
            chart,
        ],
    );
    path
}

fn opts(path: &Path) -> Options {
    Options { path: path.to_string_lossy().into_owned(), ..Default::default() }
}

fn grant(w: &World, nb: &Path, file: &str) {
    let key = crate::notebook::access::notebook_key(Some(&nb.to_string_lossy())).unwrap();
    w.state.store.add_file_grants(&key, &[(w.dir.join(file).to_string_lossy().into_owned(), false)]).unwrap();
}

fn rows(c: &NotebookCell) -> Vec<Vec<Value>> {
    c.result.as_ref().and_then(|r| r.results.first()).map(|s| s.rows.clone()).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread")]
async fn notebook_headless_runs_every_cell_and_joins_dataframes() {
    let w = world("all", vec![]);
    let path = report(&w);
    grant(&w, &path, "zonas.csv");
    let ran = run(&w.state, "t:run", &opts(&path), Policy::default()).await.unwrap();
    assert!(ran.failure.is_none(), "{:?}", ran.failure);
    assert_eq!(ran.ran, 3);
    let c = &ran.notebook.cells;
    assert_eq!(c[1].execution_count, Some(1));
    assert_eq!(c[1].result.as_ref().unwrap().engine.as_deref(), Some("sqlite"));
    assert_eq!(c[1].result.as_ref().unwrap().target_label.as_deref(), Some("Ventas"));
    assert_eq!(rows(&c[1]).len(), 3);
    assert_eq!(rows(&c[2]).len(), 2);
    let joined = rows(&c[3]);
    assert_eq!(joined.len(), 2, "{:?}", c[3].result);
    assert_eq!(joined[0][0], json!("Ana"));
    assert_eq!(joined[0][1].as_f64(), Some(14.5));
    assert_eq!(joined[1][2], json!("sur"));
    assert_eq!(c[3].result.as_ref().unwrap().target_label.as_deref(), Some("DataFrame SQL"));
    // Every session of the run is closed.
    assert!(!w.state.sessions.iter().any(|e| e.key().starts_with("t:run")), "sessions left open");
}

#[tokio::test(flavor = "multi_thread")]
async fn notebook_headless_never_reads_a_file_without_permitir_siempre() {
    let w = world("files", vec![]);
    let path = report(&w);
    let pl = super::plan(&w.state, &opts(&path), Policy::default()).unwrap();
    assert_eq!(pl.pending.iter().map(|f| f.path.clone()).collect::<Vec<_>>(), [w.dir.join("zonas.csv").to_string_lossy()]);
    assert!(pl.files.is_empty());

    let ran = run(&w.state, "t:files", &opts(&path), Policy::default()).await.unwrap();
    let (at, why) = ran.failure.expect("the file isn't allowed");
    assert_eq!(at, "c3");
    assert!(why.contains("Permitir siempre") && why.contains("zonas.csv"), "{why}");
    // The cells before it ran; the failing one keeps the error.
    assert_eq!(ran.ran, 3);
    assert!(ran.notebook.cells[3].result.as_ref().unwrap().error.is_some());
    // Nothing remembered: no read record for the file.
    let key = crate::notebook::access::notebook_key(Some(&path.to_string_lossy())).unwrap();
    assert!(w.state.store.file_record(&key, &w.dir.join("zonas.csv").to_string_lossy()).unwrap().is_none());

    // Granted ("Permitir siempre" in the app): the plan says so and it runs.
    grant(&w, &path, "zonas.csv");
    let pl = super::plan(&w.state, &opts(&path), Policy::default()).unwrap();
    assert!(pl.pending.is_empty() && pl.files.len() == 1);
    assert!(run(&w.state, "t:files2", &opts(&path), Policy::default()).await.unwrap().failure.is_none());

    // A blocked folder stays blocked even with a grant.
    let blocked = Policy::new(None, &[w.dir.clone()], &[], vec![]);
    let pl = super::plan(&w.state, &opts(&path), blocked.clone()).unwrap();
    assert_eq!(pl.blocked.len(), 1, "{pl:?}");
    let ran = run(&w.state, "t:files3", &opts(&path), blocked).await.unwrap();
    assert!(ran.failure.unwrap().1.contains("nunca deja"));
}

#[tokio::test(flavor = "multi_thread")]
async fn notebook_headless_production_needs_the_opt_in() {
    let w = world("prod", vec!["prod".into()]);
    let path = report(&w);
    grant(&w, &path, "zonas.csv");
    let pl = super::plan(&w.state, &opts(&path), Policy::default()).unwrap();
    assert_eq!(pl.confirm.iter().map(|r| r.label.clone()).collect::<Vec<_>>(), ["Ventas"]);
    assert!(pl.confirm[0].production);

    let ran = run(&w.state, "t:prod", &opts(&path), Policy::default()).await.unwrap();
    let (at, why) = ran.failure.expect("production without the opt-in");
    assert_eq!(at, "c1");
    assert!(why.contains("Permitir ejecutar en Ventas sin confirmar"), "{why}");
    assert_eq!(ran.ran, 0);
    // Cells it didn't reach have no result (none is passed off as this run's).
    assert!(ran.notebook.cells[2].result.is_none() && ran.notebook.cells[3].result.is_none());

    // A DataFrame SQL cell whose source is production fails the same way.
    let only_df = w.dir.join("solo.ipynb");
    write_nb(&only_df, vec![cell("c1", "c1", on("Ventas"), "SELECT 1"), cell("c2", "c2", dataframes(), "SELECT * FROM c1")]);
    let mut o = opts(&only_df);
    o.unconfirmed = vec![];
    let ran = run(&w.state, "t:prod-df", &o, Policy::default()).await.unwrap();
    assert_eq!(ran.failure.unwrap().0, "c1");

    let mut o = opts(&path);
    o.unconfirmed = vec![Unconfirmed { connection_id: "sq".into(), database: String::new() }];
    let ran = run(&w.state, "t:prod2", &o, Policy::default()).await.unwrap();
    assert!(ran.failure.is_none(), "{:?}", ran.failure);
    // Another database of that connection isn't covered.
    o.unconfirmed = vec![Unconfirmed { connection_id: "sq".into(), database: "otra".into() }];
    assert!(run(&w.state, "t:prod3", &o, Policy::default()).await.unwrap().failure.is_some());
}

#[tokio::test]
async fn notebook_headless_names_resolve_or_ask_in_the_editor() {
    let w = world("names", vec![]);
    save_conn(&w.state, "sq2", "Ventas", ConnectionConfig { driver: "sqlite".into(), host: w.dir.join("otra.sqlite").to_string_lossy().into_owned(), ..Default::default() }, vec![]);
    let path = w.dir.join("n.ipynb");
    write_nb(&path, vec![cell("c1", "c1", on("Ventas"), "SELECT 1"), cell("c2", "c2", on("Nadie"), "SELECT 2"), cell("c3", "c3", None, "SELECT 3")]);
    let p = plan(&w.state, &opts(&path), Policy::default()).unwrap();
    let kinds: Vec<_> = p.cells.iter().map(|c| c.problem.as_ref().map(|p| p.kind)).collect();
    assert_eq!(kinds, [Some("ambiguous"), Some("connection"), Some("none")]);
    assert_eq!(p.cells[0].problem.as_ref().unwrap().candidates.len(), 2);
    // The step's choice settles it.
    let mut o = opts(&path);
    o.targets.insert("Ventas".into(), Binding { connection_id: "sq".into(), database: String::new() });
    let p = plan(&w.state, &o, Policy::default()).unwrap();
    assert_eq!(p.cells[0].target.as_ref().unwrap().connection_id, "sq");
    // Running stops at the first cell with no target here.
    let ran = run(&w.state, "t:names", &o, Policy::default()).await.unwrap();
    let (at, why) = ran.failure.unwrap();
    assert_eq!(at, "c2");
    assert!(why.contains("Nadie"), "{why}");
}

#[test]
fn notebook_headless_parameters_and_write_cells_hooks() {
    let mut nb = Notebook {
        target: None,
        save_results: true,
        save_results_set: true,
        cells: vec![cell("c1", "c1", None, "SELECT 1")],
        parameters: vec![],
        origin: "dbine".into(),
        extra_metadata: json!({}),
        warnings: vec![],
    };
    let sqlite = |_: &NotebookCell| dbine_drivers::find("sqlite").cloned();
    assert_eq!(executable_sources(&nb, &BTreeMap::new(), &sqlite).unwrap()["c1"], "SELECT 1");
    // A value for a parameter the notebook doesn't declare is refused.
    let params = BTreeMap::from([("desde".to_string(), json!("2026-01-01"))]);
    assert!(executable_sources(&nb, &params, &sqlite).unwrap_err().to_string().contains("desde"));
    // Declared: the step's value, as the engine's literal; a required one missing fails.
    nb.parameters = vec![crate::notebook::params::NotebookParameter {
        name: "desde".into(),
        kind: crate::notebook::params::ParamType::Date,
        default: None,
        description: None,
        required: true,
        options: vec![],
    }];
    nb.cells[0].source = "SELECT * FROM t WHERE d >= {{desde}}".into();
    assert_eq!(executable_sources(&nb, &params, &sqlite).unwrap()["c1"], "SELECT * FROM t WHERE d >= '2026-01-01'");
    assert_eq!(declared_parameters(&nb), vec!["desde"]);
    assert!(executable_sources(&nb, &BTreeMap::new(), &sqlite).unwrap_err().to_string().contains("obligatorios"));
    // A write cell runs only as authorized: same cell, target and mode.
    let wc = WriteCell { cell_id: "c9".into(), name: "c9".into(), target: "dbo.resumen".into(), mode: "replace".into() };
    let auth = |t: &str, m: &str| vec![WriteAuth { cell_id: "c9".into(), target: t.into(), mode: m.into() }];
    assert!(write_authorized(&wc, &auth("dbo.resumen", "replace")));
    assert!(!write_authorized(&wc, &auth("dbo.resumen", "append")));
    assert!(!write_authorized(&wc, &auth("dbo.otra", "replace")));
    assert!(!write_authorized(&wc, &[]));
}

#[test]
fn notebook_headless_outcome_keeps_each_statement() {
    let script = "SELECT 1;\nUPDATE t SET x = 1";
    let o = QueryOutcome {
        results: vec![
            dbine_driver::StatementResult {
                columns: vec![dbine_driver::ResultColumn { name: "n".into(), type_name: "int".into() }],
                rows: vec![vec![json!(1)]],
                total_rows: 7,
                offset: Some(0),
                ..Default::default()
            },
            dbine_driver::StatementResult { rows_affected: Some(3), offset: Some(10), ..Default::default() },
            dbine_driver::StatementResult::default(),
        ],
        messages: vec!["  hola ".into(), "".into()],
        elapsed_ms: 12,
        ..Default::default()
    };
    let r = outcome_to_result(&o, script, "2026-10-18T10:00:00Z", Some("postgres".into()), Some("PG › db".into()));
    assert_eq!(r.results.len(), 2);
    assert_eq!(r.results[0].statement.as_deref(), Some("SELECT 1"));
    assert!(r.results[0].truncated && r.results[0].rows_total == 7);
    assert_eq!(r.results[1].statement.as_deref(), Some("UPDATE t SET x = 1"));
    assert_eq!(r.messages, ["hola"]);
    assert!(r.error.is_none());
}

// -- the step -----------------------------------------------------------------------------------

/// A minimal SMTP server: takes one mail and hands its DATA over.
async fn mock_smtp() -> (u16, tokio::task::JoinHandle<String>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let (r, mut wr) = sock.into_split();
        let mut lines = BufReader::new(r).lines();
        wr.write_all(b"220 mock ESMTP\r\n").await.unwrap();
        let mut data = String::new();
        let mut in_data = false;
        while let Ok(Some(line)) = lines.next_line().await {
            if in_data {
                if line == "." {
                    in_data = false;
                    wr.write_all(b"250 queued\r\n").await.unwrap();
                } else {
                    data.push_str(&line);
                    data.push('\n');
                }
                continue;
            }
            let up = line.to_uppercase();
            if up.starts_with("EHLO") || up.starts_with("HELO") {
                wr.write_all(b"250 mock\r\n").await.unwrap();
            } else if up.starts_with("DATA") {
                in_data = true;
                wr.write_all(b"354 go\r\n").await.unwrap();
            } else if up.starts_with("QUIT") {
                wr.write_all(b"221 bye\r\n").await.unwrap();
                break;
            } else {
                wr.write_all(b"250 ok\r\n").await.unwrap();
            }
        }
        data
    });
    (port, handle)
}

fn task(steps: Vec<Step>) -> ScheduledTask {
    ScheduledTask { id: "t".into(), name: "Reporte".into(), enabled: true, steps, notify: Notify::Never, ..Default::default() }
}

#[tokio::test(flavor = "multi_thread")]
async fn notebook_step_saves_exports_and_mails_the_report() {
    let w = world("step", vec![]);
    let path = report(&w);
    grant(&w, &path, "zonas.csv");
    let out = w.dir.join("salida");
    let (port, smtp) = mock_smtp().await;
    w.state
        .store
        .set_setting(crate::tasks::mail::SETTING, Some(&json!({"host": "127.0.0.1", "port": port, "security": "none", "from_address": "dbine@example.com"})))
        .unwrap();
    let t = task(vec![
        Step {
            id: "nb".into(),
            kind: kinds::RUN_NOTEBOOK.into(),
            config: json!({
                "path": path.to_string_lossy(),
                "save": {"mode": "folder", "folder": out.to_string_lossy(), "file_name": "{notebook}-{date}"},
                "export": {"format": "html"},
            }),
            ..Default::default()
        },
        Step {
            id: "m".into(),
            kind: kinds::SEND_MAIL.into(),
            config: json!({"to": "ana@example.com", "subject": "{task}", "body": "Va el reporte.", "attachments": ["{steps.1.notebook}"], "html_body": "{steps.1.html}"}),
            ..Default::default()
        },
    ]);
    let tr = crate::tasks::run_task(&w.state, None, &t, "manual").await;
    assert_eq!(tr.status, RunStatus::Ok, "{tr:#?}");
    let s = &tr.steps[0];
    assert_eq!(s.outputs["cells"], "3");
    let copy = PathBuf::from(&s.outputs["notebook"]);
    assert!(copy.starts_with(&out) && copy.extension().unwrap() == "ipynb", "{copy:?}");
    let html_path = PathBuf::from(&s.outputs["html"]);
    // The export goes where the copy went, and `file` is the export.
    assert_eq!(html_path.parent(), Some(out.as_path()));
    assert_eq!(s.outputs["file"], s.outputs["html"]);

    // The copy has this run's results; the original is untouched.
    let saved = crate::commands::notebook::read(&copy.to_string_lossy()).unwrap().notebook;
    assert_eq!(saved.cells[3].result.as_ref().unwrap().results[0].rows_total, 2);
    let original = crate::commands::notebook::read(&path.to_string_lossy()).unwrap().notebook;
    assert!(original.cells[3].result.is_none());

    // Tables, and the note where the chart would be.
    let html = std::fs::read_to_string(&html_path).unwrap();
    assert!(html.contains("Ana") && html.contains("<table"), "{html}");
    assert!(html.contains(crate::tasks::CHART_NOTE), "the chart note");

    // The mail: the HTML body and the notebook attached.
    let data = smtp.await.unwrap();
    assert!(data.contains("multipart/alternative") && data.contains("text/html"), "{data}");
    assert!(data.contains("Content-Disposition: attachment") && data.contains(copy.file_name().unwrap().to_str().unwrap()), "{data}");
    assert!(tr.steps[1].summary.contains("ana@example.com"), "{}", tr.steps[1].summary);

    // The copy can't take the original's place.
    let t = task(vec![Step {
        id: "nb".into(),
        kind: kinds::RUN_NOTEBOOK.into(),
        config: json!({"path": path.to_string_lossy(), "save": {"mode": "beside", "file_name": "reporte"}}),
        ..Default::default()
    }]);
    let tr = crate::tasks::run_task(&w.state, None, &t, "manual").await;
    assert_eq!(tr.status, RunStatus::Failed);
    assert!(tr.steps[0].summary.contains("no se sobrescribe"), "{}", tr.steps[0].summary);
}

#[tokio::test(flavor = "multi_thread")]
async fn notebook_step_cells_that_write_need_the_tasks_approval() {
    let w = world("writes", vec![]);
    let path = w.dir.join("limpieza.ipynb");
    write_nb(&path, vec![cell("c1", "c1", on("Ventas"), "SELECT count(*) FROM ventas"), cell("c2", "borrar", on("Ventas"), "DELETE FROM ventas WHERE id = 2")]);
    let mut t = task(vec![Step { id: "nb".into(), kind: kinds::RUN_NOTEBOOK.into(), config: json!({"path": path.to_string_lossy()}), ..Default::default() }]);
    let scope = crate::tasks::write_scope(&w.state, &t);
    assert_eq!(scope.len(), 1);
    assert_eq!((scope[0].what.as_str(), scope[0].cell.as_deref(), scope[0].connection.as_str()), ("DELETE", Some("borrar"), "Ventas"));

    // Not approved: nothing runs.
    let tr = crate::tasks::run_task(&w.state, None, &t, "manual").await;
    assert_eq!(tr.status, RunStatus::Failed);
    assert!(tr.steps.is_empty());
    let count = || rusqlite::Connection::open(w.dir.join("ventas.sqlite")).unwrap().query_row("SELECT COUNT(*) FROM ventas", [], |r| r.get::<_, i64>(0)).unwrap();
    assert_eq!(count(), 3);

    // Approved: it runs.
    t.approved_writes = crate::tasks::write_fingerprint(&w.state, &t);
    let tr = crate::tasks::run_task(&w.state, None, &t, "manual").await;
    assert_eq!(tr.status, RunStatus::Ok, "{tr:#?}");
    assert_eq!(count(), 2);

    // The notebook's writing cell changes on disk: the approval no longer holds.
    write_nb(&path, vec![cell("c2", "borrar", on("Ventas"), "DELETE FROM ventas")]);
    assert_ne!(crate::tasks::write_fingerprint(&w.state, &t), t.approved_writes);
    let tr = crate::tasks::run_task(&w.state, None, &t, "manual").await;
    assert_eq!(tr.status, RunStatus::Failed);
    assert_eq!(count(), 2);

    // A read-only connection: nothing to approve (and it can't write).
    let mut ro = w.state.store.get_connection("sq").unwrap().unwrap();
    ro.config.read_only = true;
    w.state.store.save_connection(&ro).unwrap();
    assert!(crate::tasks::write_scope(&w.state, &t).is_empty());
    let ran = run(&w.state, "t:ro", &opts(&path), Policy::default()).await.unwrap();
    assert!(ran.failure.is_some(), "a read-only connection refuses the DELETE");
    assert_eq!(count(), 2);
}

// -- live -----------------------------------------------------------------------------------------

fn server(driver: &str, host: &str, port: u16, user: &str) -> ConnectionConfig {
    ConnectionConfig { driver: driver.into(), host: host.into(), port, username: Some(user.into()), ..Default::default() }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn notebook_headless_live_postgres_sqlserver() {
    let w = world("live", vec![]);
    let mut pg = server("postgres", "localhost", 25010, "postgres");
    pg.database = "postgres".into();
    save_conn(&w.state, "pg", "PG prueba", pg.clone(), vec!["prod".into()]);
    w.state.typed_secrets.insert("pg".into(), [("password".to_string(), "pw".to_string())].into_iter().collect());
    let ms = server("sqlserver", "localhost", 25013, "sa");
    save_conn(&w.state, "ms", "MSSQL prueba", ms.clone(), vec![]);
    w.state.typed_secrets.insert("ms".into(), [("password".to_string(), "Pw_12345!".to_string())].into_iter().collect());

    let path = w.dir.join("live.ipynb");
    write_nb(
        &path,
        vec![
            cell("c1", "ventas_pg", on("PG prueba"), "SELECT * FROM (VALUES (10, 12.50), (20, 7.25), (10, 1.00)) AS v(cliente, total)"),
            cell("c2", "clientes_ms", on("MSSQL prueba"), "SELECT 10 AS id, N'Ana' AS nombre UNION ALL SELECT 20, N'Luis'"),
            cell("c3", "c3", dataframes(), "SELECT c.nombre, sum(v.total) AS total FROM ventas_pg v JOIN clientes_ms c ON c.id = v.cliente GROUP BY ALL ORDER BY 1"),
        ],
    );
    // PostgreSQL is tagged prod: without the opt-in it stops there.
    let ran = run(&w.state, "t:live", &opts(&path), Policy::default()).await.unwrap();
    assert_eq!(ran.failure.as_ref().map(|f| f.0.as_str()), Some("ventas_pg"));
    let mut o = opts(&path);
    o.unconfirmed = vec![Unconfirmed { connection_id: "pg".into(), database: "postgres".into() }];
    let ran = run(&w.state, "t:live2", &o, Policy::default()).await.unwrap();
    assert!(ran.failure.is_none(), "{:?}", ran.failure);
    let joined = rows(&ran.notebook.cells[2]);
    assert_eq!(joined.len(), 2, "{:?}", ran.notebook.cells[2].result);
    assert_eq!(joined[0][0], json!("Ana"));
    assert_eq!(joined[0][1].as_f64().or_else(|| joined[0][1].as_str().and_then(|s| s.parse().ok())), Some(13.5));
    assert_eq!(ran.notebook.cells[1].result.as_ref().unwrap().engine.as_deref(), Some("sqlserver"));
}

/// The "Ejecutar notebook" step with parameter values: each cell gets them
/// (bound on SQLite, a value made to escape its literal stays data), a
/// DataFrame SQL cell too, and each result records the values it used.
#[tokio::test(flavor = "multi_thread")]
async fn notebook_headless_runs_with_the_steps_parameter_values() {
    use crate::notebook::params::{NotebookParameter, ParamType};
    let w = world("params", vec![]);
    let path = w.dir.join("param.ipynb");
    let nb = Notebook {
        target: None,
        save_results: true,
        save_results_set: true,
        cells: vec![
            cell("c1", "c1", on("Ventas"), "SELECT id, total, {{nota}} AS nota FROM ventas WHERE total > {{minimo}} ORDER BY id, total"),
            cell("c2", "c2", dataframes(), "SELECT count(*) AS n FROM c1 WHERE total > {{minimo}}"),
        ],
        parameters: vec![
            NotebookParameter { name: "minimo".into(), kind: ParamType::Number, default: Some(json!(1)), description: None, required: false, options: vec![] },
            NotebookParameter { name: "nota".into(), kind: ParamType::Text, default: None, description: None, required: true, options: vec![] },
        ],
        origin: "dbine".into(),
        extra_metadata: json!({}),
        warnings: vec![],
    };
    std::fs::write(&path, serialize(&nb, PreviewLimits::default())).unwrap();
    let mut o = opts(&path);
    o.params = BTreeMap::from([("nota".to_string(), json!("'; DROP TABLE ventas; --")), ("minimo".to_string(), json!(3.5))]);
    let ran = run(&w.state, "t:params", &o, Policy::default()).await.unwrap();
    assert!(ran.failure.is_none(), "{:?}", ran.failure);
    let c = &ran.notebook.cells;
    assert_eq!(rows(&c[0]), vec![vec![json!(1), json!(4.0), json!("'; DROP TABLE ventas; --")], vec![json!(1), json!(10.5), json!("'; DROP TABLE ventas; --")]]);
    assert_eq!(c[0].result.as_ref().unwrap().params.as_ref().unwrap()["minimo"], json!(3.5));
    assert_eq!(rows(&c[1])[0][0].as_i64(), Some(2));
    // A required value missing: the run fails before any cell.
    o.params.remove("nota");
    assert!(run(&w.state, "t:params2", &o, Policy::default()).await.unwrap_err().to_string().contains("nota"));
}
