//! Tests for write cells (`notebook::write`). The pure ones and the SQLite
//! ones always run (DuckDB is loaded as the app does); the live ones
//! against the `dbine-test-*` containers are ignored by default:
//!
//! ```sh
//! cargo test -p dbine --lib -- --ignored notebook_write_live --test-threads=1 --nocapture
//! ```

use super::*;
use crate::notebook::local::LiveRows;
use dbine_core::state::SavedConnection;
use dbine_core::StateStore;
use dbine_driver::{ConnectionConfig, ResultColumn};
use serde_json::json;

fn names(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn pair(s: &str, t: &str) -> ColumnPair {
    ColumnPair {
        source: s.into(),
        target: t.into(),
    }
}

// -- mapping ----------------------------------------------------------------------------------

#[test]
fn columns_pair_by_name_or_as_mapped() {
    let src = names(&["ID", "nombre", "total", "extra"]);
    let dst = names(&["id", "Nombre", "total", "alta"]);
    // By name, ignoring case; the target's spelling is kept; `extra` isn't written.
    assert_eq!(
        pair_columns(&src, &dst, None, false).unwrap(),
        vec![
            pair("ID", "id"),
            pair("nombre", "Nombre"),
            pair("total", "total")
        ]
    );
    // As mapped: `extra` into `alta`, `total` left out.
    let m = vec![pair("id", "ID"), pair("extra", "alta")];
    assert_eq!(
        pair_columns(&src, &dst, Some(&m), false).unwrap(),
        vec![pair("ID", "id"), pair("extra", "alta")]
    );
    // A column that isn't there, either side.
    assert!(pair_columns(&src, &dst, Some(&[pair("nope", "id")]), false)
        .unwrap_err()
        .to_string()
        .contains("«nope»"));
    assert!(pair_columns(&src, &dst, Some(&[pair("id", "nope")]), false)
        .unwrap_err()
        .to_string()
        .contains("«nope»"));
    // Two into one target column.
    assert!(pair_columns(
        &src,
        &dst,
        Some(&[pair("id", "id"), pair("total", "ID")]),
        false
    )
    .unwrap_err()
    .to_string()
    .contains("dos columnas"));
    // Nothing in common.
    assert!(pair_columns(&names(&["a"]), &names(&["b"]), None, false).is_err());
    // Schemaless: every column becomes a field of its name.
    assert_eq!(
        pair_columns(&names(&["a", "b"]), &names(&["b"]), None, true).unwrap(),
        vec![pair("a", "a"), pair("b", "b")]
    );
}

#[test]
fn type_changes_and_warnings() {
    assert!(!converted("BIGINT", "int"));
    assert!(!converted("INTEGER", "decimal(10,2)"));
    assert!(converted("DOUBLE", "int"));
    assert!(converted("VARCHAR", "int"));
    assert!(converted("DATE", "nvarchar(20)"));
    assert!(!converted("TIMESTAMP WITH TIME ZONE", "datetimeoffset"));
    assert!(
        !converted("VARCHAR", ""),
        "a schemaless field has no type to change into"
    );
    let col = |s: &str, t: &str| PlannedColumn {
        source: "a".into(),
        source_type: s.into(),
        target: "b".into(),
        target_type: t.into(),
        converted: converted(s, t),
    };
    assert!(
        type_warnings(&col("VARCHAR", "nvarchar(5)"), Some(9))[0].contains("hasta 9 caracteres")
    );
    assert!(type_warnings(&col("VARCHAR", "nvarchar(max)"), Some(9)).is_empty());
    assert!(type_warnings(&col("VARCHAR", "varchar(20)"), Some(9)).is_empty());
    assert!(
        type_warnings(&col("DECIMAL(18,4)", "numeric(10,2)"), None)[0].contains("se redondean")
    );
    assert!(type_warnings(&col("DOUBLE", "bigint"), None)[0].contains("decimales"));
    assert!(
        type_warnings(&col("TIMESTAMP WITH TIME ZONE", "datetime2"), None)[0]
            .contains("zona horaria")
    );
    assert_eq!(text_length("varchar2(30 char)"), Some(30));
    assert_eq!(decimal_scale("numeric(10)"), Some(0));
}

#[test]
fn engines_without_modes_only_insert_where_they_bulk_load() {
    let redis = dbine_drivers::find("redis").unwrap();
    assert!(redis.write_modes().is_empty());
    assert_eq!(modes_of(redis.as_ref()), vec![WriteMode::Insert]);
    for id in [
        "sqlserver",
        "postgres",
        "mysql",
        "oracle",
        "sqlite",
        "duckdb",
        "mongodb",
    ] {
        if let Some(d) = dbine_drivers::find(id) {
            assert_eq!(modes_of(d.as_ref()), WriteMode::ALL.to_vec(), "{id}");
        }
    }
}

#[test]
fn no_mcp_or_assistant_tool_writes_a_dataframe() {
    // MCP's tools are its fixed list; none is a notebook's (its one write,
    // `execute`, asks in DBine for every call and takes one statement).
    let defs = crate::mcp::tools::definitions().to_string().to_lowercase();
    for word in ["notebook", "dataframe", "write_rows", "write cell"] {
        assert!(!defs.contains(word), "an MCP tool mentions {word}");
    }
    for name in [
        "notebook_write_prepare",
        "notebook_write_run",
        "notebook_write_target",
        "write_rows",
    ] {
        assert!(!crate::mcp::tools::exists(name), "{name}");
        assert!(
            !crate::mcp::tools::ASSISTANT_TOOLS.contains(&name),
            "{name}"
        );
    }
    // The assistant never writes at all.
    assert!(!crate::mcp::tools::ASSISTANT_TOOLS.contains(&"execute"));
}

// -- the token -------------------------------------------------------------------------------

fn args(tab: &str, cell: &str) -> WriteArgs {
    WriteArgs {
        tab_id: tab.into(),
        cell_id: cell.into(),
        source: "c3".into(),
        sources: Vec::new(),
        row_cap: None,
        notebook_path: None,
        approval: None,
        aws_profile: None,
        target: WriteTarget {
            connection_id: "conn".into(),
            database: "ventas".into(),
            schema: Some("dbo".into()),
            table: "resumen".into(),
            create: false,
        },
        mode: WriteMode::Upsert,
        keys: names(&["id"]),
        mapping: None,
    }
}

fn pending_for(a: &WriteArgs, rows: u64, snapshot: &str) -> Pending {
    Pending {
        tab: a.tab_id.clone(),
        cell: a.cell_id.clone(),
        bind: binding(a, rows),
        snapshot: snapshot.into(),
        rows,
        pairs: Vec::new(),
        table: table_ref(&a.target),
        create: None,
        label: String::new(),
        warnings: Vec::new(),
        at: Instant::now(),
    }
}

#[test]
fn the_token_is_bound_to_what_was_confirmed_and_works_once() {
    let a = args("tab-token", "w1");
    let (t, old) = issue(pending_for(&a, 120, "s1")).unwrap();
    assert!(old.is_none());
    // Another row count, mode, target, keys or mapping: refused, and spent.
    let mut b = a.clone();
    b.mode = WriteMode::Replace;
    assert!(take(&t, &a.tab_id, &a.cell_id, &binding(&b, 120)).is_err());
    assert!(
        take(&t, &a.tab_id, &a.cell_id, &binding(&a, 120)).is_err(),
        "spent by the failed try"
    );

    let (t, _) = issue(pending_for(&a, 120, "s2")).unwrap();
    for bad in [
        binding(&a, 121),
        {
            let mut c = a.clone();
            c.target.table = "otra".into();
            binding(&c, 120)
        },
        {
            let mut c = a.clone();
            c.keys = names(&["id", "region"]);
            binding(&c, 120)
        },
        {
            let mut c = a.clone();
            c.mapping = Some(vec![pair("id", "id")]);
            binding(&c, 120)
        },
    ] {
        let (t2, _) = issue(pending_for(&a, 120, "s3")).unwrap();
        assert!(take(&t2, &a.tab_id, &a.cell_id, &bad).is_err());
    }
    // `t` was replaced by the later issues for the same cell.
    assert!(take(&t, &a.tab_id, &a.cell_id, &binding(&a, 120)).is_err());

    // The right one, once; not from another tab or cell.
    let (_, old) = issue(pending_for(&a, 120, "s4")).unwrap();
    assert!(old.is_none(), "the failed tries spent theirs");
    let (t, old) = issue(pending_for(&a, 120, "s4b")).unwrap();
    assert_eq!(
        old.as_deref(),
        Some("s4"),
        "the cell's earlier snapshot is handed back to drop"
    );
    assert!(take(&t, "other-tab", &a.cell_id, &binding(&a, 120)).is_err());
    let (t, _) = issue(pending_for(&a, 120, "s5")).unwrap();
    assert!(take(&t, &a.tab_id, "w2", &binding(&a, 120)).is_err());
    let (t, _) = issue(pending_for(&a, 120, "s6")).unwrap();
    let p = take(&t, &a.tab_id, &a.cell_id, &binding(&a, 120)).unwrap();
    assert_eq!((p.rows, p.snapshot.as_str()), (120, "s6"));
    assert!(
        take(&t, &a.tab_id, &a.cell_id, &binding(&a, 120)).is_err(),
        "a token works once"
    );
}

// -- read-only --------------------------------------------------------------------------------

fn saved(id: &str, name: &str, cfg: ConnectionConfig) -> SavedConnection {
    SavedConnection {
        id: id.into(),
        name: name.into(),
        color: None,
        config: cfg,
        save_password: false,
        folder_id: None,
        tags: Vec::new(),
        mcp_level: None,
        updated_at: String::new(),
    }
}

#[tokio::test]
async fn read_only_connections_are_never_written() {
    let state = AppState::new(StateStore::open_in_memory().unwrap());
    let cfg = ConnectionConfig {
        driver: "sqlite".into(),
        host: ":memory:".into(),
        read_only: true,
        ..Default::default()
    };
    state
        .store
        .save_connection(&saved("ro", "Solo lectura", cfg.clone()))
        .unwrap();
    let mut a = args("tab-ro", "w1");
    a.target.connection_id = "ro".into();
    a.target.schema = None;
    let e = target_info(&state, "tab-ro", &a.target).await.unwrap_err();
    assert!(e.to_string().contains("solo lectura"), "{e}");
    let e = prepare(
        &state,
        &a,
        crate::notebook::access::Policy::new(None, &[], &[], Vec::new()),
        net(),
    )
    .await
    .unwrap_err();
    assert!(e.to_string().contains("solo lectura"), "{e}");
    // The session of a read-only connection refuses it too.
    let mut s = dbine_drivers::open_session(&cfg, None).await.unwrap();
    struct Empty;
    #[dbine_driver::async_trait]
    impl BatchSource for Empty {
        async fn next(&mut self) -> Option<RowBatch> {
            None
        }
    }
    let spec = WriteSpec {
        table: ObjectRef {
            kind: "table".into(),
            schema: None,
            name: "t".into(),
        },
        columns: names(&["a"]),
        key: Vec::new(),
        mode: WriteMode::Insert,
    };
    let e = s
        .write_rows(&spec, &[], &mut Empty, &|_| {})
        .await
        .unwrap_err();
    assert!(e.to_string().contains("solo lectura"), "{e}");
}

// -- end to end on SQLite (no containers) -------------------------------------------------------

async fn rows_of(s: &mut dyn Session, sql: &str) -> Vec<Vec<Value>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10_000, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out.results
        .into_iter()
        .find(|r| !r.columns.is_empty())
        .map(|r| r.rows)
        .unwrap_or_default()
}

fn live_source(name: &str, cols: &[(&str, &str)], rows: Vec<Vec<Value>>) -> LocalSource {
    LocalSource {
        name: name.into(),
        local: false,
        sql: format!("SELECT * FROM {name}"),
        connection_id: Some("src".into()),
        database: None,
        run: Some(1),
        rows: Some(LiveRows {
            columns: cols
                .iter()
                .map(|(n, t)| ResultColumn {
                    name: (*n).into(),
                    type_name: (*t).into(),
                })
                .collect(),
            rows,
        }),
        files_key: String::new(),
    }
}

fn df_source(name: &str, sql: &str) -> LocalSource {
    LocalSource {
        name: name.into(),
        local: true,
        sql: sql.into(),
        connection_id: None,
        database: None,
        run: None,
        rows: None,
        files_key: String::new(),
    }
}

fn net() -> crate::notebook::remote::Net {
    crate::notebook::remote::Net::current(None)
}

fn policy() -> crate::notebook::access::Policy {
    crate::notebook::access::Policy::new(None, &[], &[], Vec::new())
}

/// Prepare and run one write; the plan and what it did.
async fn write_once(state: &AppState, a: &WriteArgs) -> (WritePlan, WriteOutcome) {
    let r = prepare(state, a, policy(), net())
        .await
        .unwrap_or_else(|e| panic!("prepare {:?}: {e}", a.mode));
    let plan = r.plan.expect("a plan");
    let out = run(state, a, &plan.token, plan.rows)
        .await
        .unwrap_or_else(|e| panic!("run {:?}: {e}", a.mode));
    (plan, out)
}

/// Two sources (as two engines' runs would give them) joined by a
/// DataFrame SQL cell: the DataFrame written.
fn two_engine_sources() -> Vec<LocalSource> {
    vec![
        live_source(
            "ventas",
            &[("cliente", "int4"), ("total", "numeric(10,2)"), ("dia", "date")],
            vec![
                vec![json!(10), json!("12.50"), json!("2026-10-01")],
                vec![json!(20), json!("7.25"), json!("2026-10-02")],
                vec![json!(10), json!("1.00"), json!("2026-10-03")],
                vec![json!(30), json!("5.00"), json!("2026-10-03")],
            ],
        ),
        live_source("clientes", &[("id", "int"), ("nombre", "nvarchar(50)")], vec![vec![json!(10), json!("Ana")], vec![json!(20), json!("Luis")], vec![json!(30), json!("Eva")]]),
        df_source("resumen", "SELECT c.id, c.nombre, sum(v.total) AS total, max(v.dia) AS ultimo FROM ventas v JOIN clientes c ON c.id = v.cliente GROUP BY ALL"),
    ]
}

const READ_RESUMEN: &str = "SELECT id, nombre, CAST(total AS TEXT) FROM resumen ORDER BY id";

#[tokio::test]
async fn notebook_write_every_mode_into_sqlite() {
    let dir = std::env::temp_dir().join(format!("dbine-nbw-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.sqlite");
    let cfg = ConnectionConfig {
        driver: "sqlite".into(),
        host: path.to_string_lossy().into(),
        ..Default::default()
    };
    let state = AppState::new(StateStore::open_in_memory().unwrap());
    state
        .store
        .save_connection(&saved("lite", "Local", cfg.clone()))
        .unwrap();
    let d = dbine_drivers::find("sqlite").unwrap();
    let mut check = d.connect(&cfg, None).await.unwrap();
    let reset = "DROP TABLE IF EXISTS resumen; CREATE TABLE resumen (id INTEGER PRIMARY KEY, nombre TEXT, total NUMERIC, ultimo TEXT); \
                 INSERT INTO resumen VALUES (10, 'viejo', 0, NULL), (99, 'queda', 1, NULL);";
    let mut a = args("tab-lite", "w1");
    a.sources = two_engine_sources();
    a.source = "resumen".into();
    a.target = WriteTarget {
        connection_id: "lite".into(),
        database: String::new(),
        schema: None,
        table: "resumen".into(),
        create: false,
    };

    // Update by key: 10 updated; 20 and 30 not added; 99 stays.
    rows_of(check.as_mut(), reset).await;
    a.mode = WriteMode::Update;
    let (plan, out) = write_once(&state, &a).await;
    assert_eq!(plan.rows, 3);
    assert_eq!(
        plan.columns
            .iter()
            .map(|c| c.target.as_str())
            .collect::<Vec<_>>(),
        ["id", "nombre", "total", "ultimo"]
    );
    assert_eq!(
        (out.inserted, out.updated, out.deleted, out.atomic),
        (0, 1, 0, true)
    );
    assert_eq!(
        rows_of(check.as_mut(), READ_RESUMEN).await,
        vec![
            vec![json!(10), json!("Ana"), json!("13.5")],
            vec![json!(99), json!("queda"), json!("1")]
        ]
    );

    // Upsert: 10 updated, 20 and 30 inserted, 99 stays.
    rows_of(check.as_mut(), reset).await;
    a.mode = WriteMode::Upsert;
    let (_, out) = write_once(&state, &a).await;
    assert_eq!((out.inserted, out.updated, out.deleted), (2, 1, 0));
    assert_eq!(rows_of(check.as_mut(), READ_RESUMEN).await.len(), 4);

    // Replace: both rows deleted, three inserted.
    rows_of(check.as_mut(), reset).await;
    a.mode = WriteMode::Replace;
    a.keys.clear();
    let (_, out) = write_once(&state, &a).await;
    assert_eq!((out.inserted, out.updated, out.deleted), (3, 0, 2));
    assert_eq!(
        rows_of(check.as_mut(), READ_RESUMEN)
            .await
            .iter()
            .map(|r| r[0].clone())
            .collect::<Vec<_>>(),
        vec![json!(10), json!(20), json!(30)]
    );

    // Insert: a key already there fails, and nothing is left half done.
    rows_of(check.as_mut(), reset).await;
    a.mode = WriteMode::Insert;
    let r = prepare(&state, &a, policy(), net())
        .await
        .unwrap()
        .plan
        .unwrap();
    let e = run(&state, &a, &r.token, r.rows).await.unwrap_err();
    assert!(e.to_string().contains("No se escribió nada"), "{e}");
    assert_eq!(rows_of(check.as_mut(), READ_RESUMEN).await.len(), 2);

    // Mapped, into a new table with the key as its primary key, then again
    // (the monthly run): upserted the second time.
    let mut n = a.clone();
    n.target.table = "nueva".into();
    n.target.create = true;
    n.mode = WriteMode::Upsert;
    n.keys = names(&["id"]);
    n.mapping = Some(vec![pair("id", "id"), pair("total", "total")]);
    let (plan, out) = write_once(&state, &n).await;
    assert!(
        out.created
            && plan
                .create
                .as_deref()
                .is_some_and(|c| c.contains("CREATE TABLE")),
        "{plan:?}"
    );
    assert_eq!(plan.unmatched_source, names(&["nombre", "ultimo"]));
    assert_eq!(out.inserted, 3);
    n.target.create = false;
    let (_, out) = write_once(&state, &n).await;
    assert_eq!((out.inserted, out.updated), (0, 3));
    assert_eq!(
        rows_of(check.as_mut(), "SELECT count(*) FROM nueva").await,
        vec![vec![json!(3)]]
    );

    // A repeated key in the DataFrame is refused before anything is written.
    let mut dup = a.clone();
    dup.sources.push(df_source(
        "dos",
        "SELECT * FROM resumen UNION ALL SELECT * FROM resumen",
    ));
    dup.source = "dos".into();
    dup.mode = WriteMode::Upsert;
    dup.keys = names(&["id"]);
    let e = prepare(&state, &dup, policy(), net()).await.unwrap_err();
    assert!(e.to_string().contains("repite 3 claves"), "{e}");

    // A DataFrame cut at the row cap is never written.
    let mut big = a.clone();
    big.sources
        .push(df_source("muchas", "SELECT range AS id FROM range(1500)"));
    big.source = "muchas".into();
    big.row_cap = Some(1_000);
    big.mapping = Some(vec![pair("id", "id")]);
    let e = prepare(&state, &big, policy(), net()).await.unwrap_err();
    assert!(e.to_string().contains("cortada"), "{e}");
    // Under the cap: the whole DataFrame is planned, never the rows a cell shows.
    big.row_cap = None;
    let plan = prepare(&state, &big, policy(), net())
        .await
        .unwrap()
        .plan
        .unwrap();
    assert_eq!(plan.rows, 1500);

    // A table that doesn't exist (without "crear tabla nueva").
    let mut none = a.clone();
    none.target.table = "no_existe".into();
    let e = prepare(&state, &none, policy(), net()).await.unwrap_err();
    assert!(e.to_string().contains("no existe la tabla"), "{e}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn notebook_write_into_a_duckdb_file() {
    let dir =
        std::env::temp_dir().join(format!("dbine-nbw-duck-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("t.duckdb");
    let cfg = ConnectionConfig {
        driver: "duckdb".into(),
        host: path.to_string_lossy().into(),
        ..Default::default()
    };
    let state = AppState::new(StateStore::open_in_memory().unwrap());
    state
        .store
        .save_connection(&saved("duck", "Archivo DuckDB", cfg.clone()))
        .unwrap();
    let mut check = dbine_drivers::open_session(&cfg, None).await.unwrap();
    rows_of(check.as_mut(), "CREATE TABLE main.resumen (id INTEGER PRIMARY KEY, nombre VARCHAR, total DECIMAL(12,2), ultimo DATE)").await;
    rows_of(
        check.as_mut(),
        "INSERT INTO main.resumen VALUES (10, 'viejo', 0, NULL), (99, 'queda', 1, NULL)",
    )
    .await;
    drop(check);
    let mut a = args("tab-duck", "w1");
    a.sources = two_engine_sources();
    a.source = "resumen".into();
    a.target = WriteTarget {
        connection_id: "duck".into(),
        database: String::new(),
        schema: Some("main".into()),
        table: "resumen".into(),
        create: false,
    };
    let (_, out) = write_once(&state, &a).await;
    assert_eq!((out.inserted, out.updated, out.atomic), (2, 1, true));
    a.mode = WriteMode::Replace;
    a.keys.clear();
    let (_, out) = write_once(&state, &a).await;
    assert_eq!((out.inserted, out.deleted), (3, 4));
    let mut check = dbine_drivers::open_session(&cfg, None).await.unwrap();
    let rows = texts(
        rows_of(
            check.as_mut(),
            "SELECT id, nombre, CAST(total AS VARCHAR) FROM main.resumen ORDER BY id",
        )
        .await,
    );
    assert_eq!(
        rows,
        vec![
            vec!["10", "Ana", "13.50"],
            vec!["20", "Luis", "7.25"],
            vec!["30", "Eva", "5.00"]
        ]
    );
    // The staging table went with the write.
    assert_eq!(
        texts(
            rows_of(
                check.as_mut(),
                "SELECT count(*) FROM duckdb_tables() WHERE table_name LIKE 'dbine_w_%'"
            )
            .await
        ),
        vec![vec!["0"]]
    );
    drop(check);
    let _ = std::fs::remove_dir_all(&dir);
}

// -- the .ipynb ---------------------------------------------------------------------------------

#[test]
fn the_file_keeps_the_definition_by_name_and_only_a_short_summary() {
    use crate::notebook::{parse, serialize, CellKind, PreviewLimits, WriteCell, WriteSummary};
    let mut nb =
        parse(r#"{"nbformat":4,"nbformat_minor":5,"metadata":{"dbine":{"version":1}},"cells":[]}"#)
            .unwrap();
    let mut cell = parse(r#"{"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[{"cell_type":"markdown","id":"x","metadata":{},"source":""}]}"#).unwrap().cells.remove(0);
    cell.id = "w1".into();
    cell.kind = CellKind::Write;
    cell.write = Some(WriteCell {
        source: Some("c3".into()),
        target: Some(crate::notebook::NotebookTarget {
            connection: Some("Ventas prod".into()),
            database: Some("ventas".into()),
            environment: None,
            results: false,
        }),
        schema: Some("dbo".into()),
        table: Some("resumen".into()),
        create: false,
        mode: WriteMode::Upsert,
        keys: names(&["id"]),
        mapping: Some(vec![pair("id", "id"), pair("total", "importe")]),
    });
    cell.write_result = Some(WriteSummary {
        inserted: 2,
        updated: 1,
        deleted: 0,
        ran_at: "2026-10-18T14:32:05Z".into(),
        duration_ms: 1200,
        target_label: Some("Ventas prod › ventas › dbo.resumen".into()),
        error: None,
        warnings: names(&["only on screen"]),
    });
    nb.cells.push(cell);
    let text = serialize(&nb, PreviewLimits::default());
    let v: Value = serde_json::from_str(&text).unwrap();
    let c = &v["cells"][0];
    assert_eq!(c["metadata"]["dbine"]["kind"], "write");
    let w = &c["metadata"]["dbine"]["write"];
    assert_eq!(
        w["target"],
        json!({ "connection": "Ventas prod", "database": "ventas" })
    );
    assert_eq!(
        (w["mode"].as_str(), w["keys"].clone()),
        (Some("upsert"), json!(["id"]))
    );
    assert_eq!(c["outputs"][0]["output_type"], "stream");
    assert!(
        c["outputs"][0]["text"][0]
            .as_str()
            .unwrap()
            .starts_with("2 inserted, 1 updated"),
        "{}",
        c["outputs"]
    );
    assert!(!text.contains("only on screen"), "warnings stay on screen");
    assert!(
        !text.contains("connection_id") && !text.contains("\"conn\""),
        "never ids"
    );
    assert!(c["source"]
        .as_array()
        .unwrap()
        .iter()
        .any(|l| l.as_str().unwrap().contains("insert or update by id")));
    // And back.
    let back = parse(&text).unwrap();
    let b = &back.cells[0];
    assert_eq!(b.kind, CellKind::Write);
    assert_eq!(b.write, nb.cells[0].write);
    assert_eq!(
        b.write_result
            .as_ref()
            .map(|r| (r.inserted, r.updated, r.duration_ms)),
        Some((2, 1, 1200))
    );

    // A foreign file can't sneak credentials or a DataFrame SQL target in.
    let evil = text.replace(
        "\"connection\": \"Ventas prod\"",
        "\"connection\": \"postgres://u:p@h/db\", \"password\": \"x\"",
    );
    let back = parse(&evil).unwrap();
    assert!(back.cells[0]
        .write
        .as_ref()
        .unwrap()
        .target
        .as_ref()
        .is_none_or(|t| t.connection.is_none()));
    assert!(!back.warnings.is_empty());
    let unknown = text.replace("\"upsert\"", "\"truncate_everything\"");
    assert_eq!(
        parse(&unknown).unwrap().cells[0]
            .write
            .as_ref()
            .unwrap()
            .mode,
        WriteMode::Insert,
        "an unknown mode reads as insert"
    );
}

// -- live: a DataFrame from two engines into SQL Server, PostgreSQL and MongoDB ------------------

fn url(env: &str, default: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| default.to_string())
}

fn server_cfg(driver: &str, u: &str) -> ConnectionConfig {
    let rest = u.split_once("://").map_or(u, |(_, r)| r);
    let rest = rest.split(['/', '?']).next().unwrap_or(rest);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').unwrap_or((auth, ""));
    let (host, port) = hostport.rsplit_once(':').unwrap_or((hostport, "0"));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().unwrap_or(0),
        username: (!user.is_empty()).then(|| user.to_string()),
        password: (!pass.is_empty()).then(|| pass.to_string()),
        ..Default::default()
    }
}

/// Saved in `state` (the password as typed this run, as the app keeps it).
fn save(state: &AppState, id: &str, cfg: &ConnectionConfig) {
    state
        .store
        .save_connection(&saved(id, &format!("{id} test"), cfg.clone()))
        .unwrap();
    if let Some(p) = &cfg.password {
        state.typed_secrets.insert(
            id.into(),
            [("password".to_string(), p.clone())].into_iter().collect(),
        );
    }
}

/// Values as text (engines give integers as numbers or as text).
fn texts(rows: Vec<Vec<Value>>) -> Vec<Vec<String>> {
    rows.into_iter()
        .map(|r| {
            r.into_iter()
                .map(|v| {
                    v.as_str()
                        .map(String::from)
                        .unwrap_or_else(|| v.to_string())
                })
                .collect()
        })
        .collect()
}

fn source_on(name: &str, conn: &str, database: &str, sql: &str) -> LocalSource {
    LocalSource {
        name: name.into(),
        local: false,
        sql: sql.into(),
        connection_id: Some(conn.into()),
        database: Some(database.into()),
        run: None,
        rows: None,
        files_key: String::new(),
    }
}

#[tokio::test]
#[ignore]
async fn notebook_write_live_sqlserver_postgres_mongodb() {
    let state = AppState::new(StateStore::open_in_memory().unwrap());
    let mut pg = server_cfg(
        "postgres",
        &url(
            "DBINE_TEST_POSTGRES_URL",
            "postgres://postgres:pw@localhost:25010",
        ),
    );
    pg.database = "postgres".into();
    let mut ms = server_cfg(
        "sqlserver",
        &url(
            "DBINE_TEST_SQLSERVER_URL",
            "mssql://sa:Pw_12345!@localhost:25013",
        ),
    );
    ms.database = "master".into();
    ms.trust_server_certificate = true;
    let mut mg = ConnectionConfig {
        driver: "mongodb".into(),
        database: "dbine_nbw_db".into(),
        ..Default::default()
    };
    mg.options.insert(
        "connection_string".into(),
        url(
            "DBINE_TEST_MONGODB_URL",
            "mongodb://root:secret@localhost:25201/?authSource=admin",
        ),
    );
    save(&state, "pg", &pg);
    save(&state, "ms", &ms);
    save(&state, "mg", &mg);

    // The sources: sales on PostgreSQL, customers on SQL Server.
    let mut p = dbine_drivers::open_session(&pg, Some("postgres"))
        .await
        .unwrap();
    rows_of(p.as_mut(), "DROP TABLE IF EXISTS dbine_nbw_ventas").await;
    rows_of(
        p.as_mut(),
        "CREATE TABLE dbine_nbw_ventas (cliente int, total numeric(10,2), dia date)",
    )
    .await;
    rows_of(p.as_mut(), "INSERT INTO dbine_nbw_ventas VALUES (10, 12.50, '2026-10-01'), (20, 7.25, '2026-10-02'), (10, 1.00, '2026-10-03'), (30, 5.00, '2026-10-03')").await;
    let mut m = dbine_drivers::open_session(&ms, Some("master"))
        .await
        .unwrap();
    rows_of(
        m.as_mut(),
        "IF OBJECT_ID('dbo.dbine_nbw_clientes') IS NOT NULL DROP TABLE dbo.dbine_nbw_clientes",
    )
    .await;
    rows_of(
        m.as_mut(),
        "CREATE TABLE dbo.dbine_nbw_clientes (id int, nombre nvarchar(50))",
    )
    .await;
    rows_of(
        m.as_mut(),
        "INSERT INTO dbo.dbine_nbw_clientes VALUES (10, N'Ana'), (20, N'Luis'), (30, N'Eva')",
    )
    .await;
    let sources = vec![
        source_on("ventas_pg", "pg", "postgres", "SELECT cliente, total, dia FROM dbine_nbw_ventas"),
        source_on("clientes_ms", "ms", "master", "SELECT id, nombre FROM dbo.dbine_nbw_clientes"),
        df_source("resumen", "SELECT c.id, c.nombre, sum(v.total) AS total, max(v.dia) AS ultimo FROM ventas_pg v JOIN clientes_ms c ON c.id = v.cliente GROUP BY ALL"),
    ];
    let mut a = args("tab-live", "w1");
    a.sources = sources;
    a.source = "resumen".into();

    // SQL Server and PostgreSQL, each mode, from the same starting rows.
    let targets: [(&str, &str, Option<&str>, &str, &str); 3] = [
        ("ms", "master", Some("dbo"), "IF OBJECT_ID('dbo.dbine_nbw_resumen') IS NOT NULL DROP TABLE dbo.dbine_nbw_resumen; CREATE TABLE dbo.dbine_nbw_resumen (id int PRIMARY KEY, nombre nvarchar(50), total decimal(12,2), ultimo date); INSERT INTO dbo.dbine_nbw_resumen VALUES (10, N'viejo', 0, NULL), (99, N'queda', 1, NULL)", "SELECT id, nombre, CAST(total AS varchar(20)) FROM dbo.dbine_nbw_resumen ORDER BY id"),
        // An identity key: its inserts go through IDENTITY_INSERT.
        ("ms", "master", Some("dbo"), "IF OBJECT_ID('dbo.dbine_nbw_resumen') IS NOT NULL DROP TABLE dbo.dbine_nbw_resumen; CREATE TABLE dbo.dbine_nbw_resumen (id int IDENTITY PRIMARY KEY, nombre nvarchar(50), total decimal(12,2), ultimo date); SET IDENTITY_INSERT dbo.dbine_nbw_resumen ON; INSERT INTO dbo.dbine_nbw_resumen (id, nombre, total) VALUES (10, N'viejo', 0), (99, N'queda', 1); SET IDENTITY_INSERT dbo.dbine_nbw_resumen OFF", "SELECT id, nombre, CAST(total AS varchar(20)) FROM dbo.dbine_nbw_resumen ORDER BY id"),
        ("pg", "postgres", Some("public"), "DROP TABLE IF EXISTS public.dbine_nbw_resumen; CREATE TABLE public.dbine_nbw_resumen (id int PRIMARY KEY, nombre varchar(50), total numeric(12,2), ultimo date); INSERT INTO public.dbine_nbw_resumen VALUES (10, 'viejo', 0, NULL), (99, 'queda', 1, NULL)", "SELECT id, nombre, CAST(total AS text) FROM public.dbine_nbw_resumen ORDER BY id"),
    ];
    for (conn, db, schema, reset, read) in targets {
        let s: &mut dyn Session = if conn == "ms" { m.as_mut() } else { p.as_mut() };
        a.target = WriteTarget {
            connection_id: conn.into(),
            database: db.into(),
            schema: schema.map(String::from),
            table: "dbine_nbw_resumen".into(),
            create: false,
        };
        let cases: [(WriteMode, (u64, u64, u64), Vec<Vec<Value>>); 4] = [
            (WriteMode::Insert, (0, 0, 0), vec![]),
            (
                WriteMode::Update,
                (0, 1, 0),
                vec![
                    vec![json!(10), json!("Ana"), json!("13.50")],
                    vec![json!(99), json!("queda"), json!("1.00")],
                ],
            ),
            (
                WriteMode::Upsert,
                (2, 1, 0),
                vec![
                    vec![json!(10), json!("Ana"), json!("13.50")],
                    vec![json!(20), json!("Luis"), json!("7.25")],
                    vec![json!(30), json!("Eva"), json!("5.00")],
                    vec![json!(99), json!("queda"), json!("1.00")],
                ],
            ),
            (
                WriteMode::Replace,
                (3, 0, 2),
                vec![
                    vec![json!(10), json!("Ana"), json!("13.50")],
                    vec![json!(20), json!("Luis"), json!("7.25")],
                    vec![json!(30), json!("Eva"), json!("5.00")],
                ],
            ),
        ];
        for (mode, counts, expected) in cases {
            // The DROP in a batch of its own: SQL Server compiles a batch's
            // INSERT against the table that exists before the DROP runs.
            let (drop, rest) = reset.split_once("; ").unwrap();
            rows_of(s, drop).await;
            rows_of(s, rest).await;
            a.mode = mode;
            a.keys = if mode.needs_key() {
                names(&["id"])
            } else {
                Vec::new()
            };
            if mode == WriteMode::Insert {
                // Rows 10 and 99 are there: only the new keys go in.
                rows_of(
                    s,
                    &format!(
                        "DELETE FROM {}dbine_nbw_resumen WHERE id = 10",
                        schema.map(|x| format!("{x}.")).unwrap_or_default()
                    ),
                )
                .await;
                let (_, out) = write_once(&state, &a).await;
                eprintln!("{conn} insert: {out:?}");
                assert_eq!(
                    (out.inserted, out.updated, out.deleted, out.atomic),
                    (3, 0, 0, true)
                );
                assert_eq!(rows_of(s, read).await.len(), 4);
                // Again: the key is there, the transaction rolls back whole.
                let r = prepare(&state, &a, policy(), net())
                    .await
                    .unwrap()
                    .plan
                    .unwrap();
                let e = run(&state, &a, &r.token, r.rows).await.unwrap_err();
                assert!(e.to_string().contains("No se escribió nada"), "{e}");
                assert_eq!(rows_of(s, read).await.len(), 4);
                continue;
            }
            let (plan, out) = write_once(&state, &a).await;
            eprintln!("{conn} {mode:?}: {out:?} warnings {:?}", plan.warnings);
            assert_eq!(
                (out.inserted, out.updated, out.deleted),
                counts,
                "{conn} {mode:?}"
            );
            assert!(out.atomic);
            assert_eq!(
                texts(rows_of(s, read).await),
                texts(expected),
                "{conn} {mode:?}"
            );
        }
    }

    // A new table on each, by key, then the monthly run again.
    for (conn, db, schema, drop) in [
        (
            "ms",
            "master",
            Some("dbo"),
            "IF OBJECT_ID('dbo.dbine_nbw_nueva') IS NOT NULL DROP TABLE dbo.dbine_nbw_nueva",
        ),
        (
            "pg",
            "postgres",
            Some("public"),
            "DROP TABLE IF EXISTS public.dbine_nbw_nueva",
        ),
    ] {
        let s: &mut dyn Session = if conn == "ms" { m.as_mut() } else { p.as_mut() };
        rows_of(s, drop).await;
        a.target = WriteTarget {
            connection_id: conn.into(),
            database: db.into(),
            schema: schema.map(String::from),
            table: "dbine_nbw_nueva".into(),
            create: true,
        };
        a.mode = WriteMode::Upsert;
        a.keys = names(&["id"]);
        let (plan, out) = write_once(&state, &a).await;
        eprintln!("{conn} create: {:?}\n{out:?}", plan.create);
        assert!(out.created);
        assert_eq!(out.inserted, 3);
        a.target.create = false;
        let (_, out) = write_once(&state, &a).await;
        assert_eq!((out.inserted, out.updated), (0, 3));
    }

    // MongoDB: insert, then upsert by id.
    let mut g = dbine_drivers::open_session(&mg, Some("dbine_nbw_db"))
        .await
        .unwrap();
    rows_of(g.as_mut(), "db.resumen.drop()").await;
    rows_of(
        g.as_mut(),
        r#"db.resumen.insertMany([{id: 10, nombre: "viejo"}, {id: 99, nombre: "queda"}])"#,
    )
    .await;
    a.target = WriteTarget {
        connection_id: "mg".into(),
        database: "dbine_nbw_db".into(),
        schema: None,
        table: "resumen".into(),
        create: false,
    };
    a.mode = WriteMode::Upsert;
    a.keys = names(&["id"]);
    let (plan, out) = write_once(&state, &a).await;
    eprintln!("mongo upsert: {out:?} {:?}", plan.columns);
    assert_eq!((out.inserted, out.updated, out.atomic), (2, 1, false));
    let docs = rows_of(
        g.as_mut(),
        "db.resumen.find({}, {_id: 0, id: 1, nombre: 1}).sort({id: 1})",
    )
    .await;
    assert_eq!(docs.len(), 4, "{docs:?}");
    assert!(
        format!("{docs:?}").contains("Ana") && format!("{docs:?}").contains("queda"),
        "{docs:?}"
    );
    a.mode = WriteMode::Update;
    let (_, out) = write_once(&state, &a).await;
    assert_eq!((out.inserted, out.updated), (0, 3));
    a.mode = WriteMode::Replace;
    a.keys.clear();
    let (_, out) = write_once(&state, &a).await;
    assert_eq!((out.inserted, out.deleted), (3, 4));
    a.mode = WriteMode::Insert;
    let (_, out) = write_once(&state, &a).await;
    assert_eq!(out.inserted, 3);
    assert_eq!(
        rows_of(g.as_mut(), "db.resumen.countDocuments({})").await,
        vec![vec![json!(6)]]
    );
}

/// The DataFrame of [`two_engine_sources`] written in each mode into
/// `table` (reset by `reset`, read back ordered by key with `read`).
async fn every_mode(
    state: &AppState,
    s: &mut dyn Session,
    conn: &str,
    db: &str,
    schema: Option<&str>,
    table: &str,
    reset: &[&str],
    read: &str,
) {
    let mut a = args(&format!("tab-{conn}"), "w1");
    a.sources = two_engine_sources();
    a.source = "resumen".into();
    a.target = WriteTarget {
        connection_id: conn.into(),
        database: db.into(),
        schema: schema.map(String::from),
        table: table.into(),
        create: false,
    };
    let cases: [(WriteMode, (u64, u64, u64), usize); 4] = [
        (WriteMode::Update, (0, 1, 0), 2),
        (WriteMode::Upsert, (2, 1, 0), 4),
        (WriteMode::Replace, (3, 0, 2), 3),
        (WriteMode::Insert, (3, 0, 0), 0),
    ];
    for (mode, counts, rows) in cases {
        for r in reset {
            rows_of(s, r).await;
        }
        a.mode = mode;
        a.keys = if mode.needs_key() {
            names(&["id"])
        } else {
            Vec::new()
        };
        if mode == WriteMode::Insert {
            // 10 is there: the whole insert rolls back.
            let r = prepare(state, &a, policy(), net())
                .await
                .unwrap()
                .plan
                .unwrap();
            let e = run(state, &a, &r.token, r.rows).await.unwrap_err();
            eprintln!("{conn} insert over a key: {e}");
            assert!(e.to_string().contains("No se escribió nada"), "{e}");
            assert_eq!(rows_of(s, read).await.len(), 2);
            continue;
        }
        let (plan, out) = write_once(state, &a).await;
        eprintln!("{conn} {mode:?}: {out:?} {:?}", plan.warnings);
        assert_eq!(
            (out.inserted, out.updated, out.deleted, out.atomic),
            (counts.0, counts.1, counts.2, true),
            "{conn} {mode:?}"
        );
        let got = texts(rows_of(s, read).await);
        assert_eq!(got.len(), rows, "{conn} {mode:?}: {got:?}");
        assert!(
            got.iter().any(|r| r[1] == "Ana"),
            "{conn} {mode:?}: {got:?}"
        );
    }
}

#[tokio::test]
#[ignore]
async fn notebook_write_live_mysql_oracle() {
    let state = AppState::new(StateStore::open_in_memory().unwrap());
    let my = server_cfg(
        "mysql",
        &url("DBINE_TEST_MYSQL_URL", "mysql://root:pw@localhost:25011"),
    );
    save(&state, "my", &my);
    let mut s = dbine_drivers::open_session(&my, None).await.unwrap();
    rows_of(s.as_mut(), "CREATE DATABASE IF NOT EXISTS dbine_nbw").await;
    let mut s = dbine_drivers::open_session(&my, Some("dbine_nbw"))
        .await
        .unwrap();
    every_mode(
        &state,
        s.as_mut(),
        "my",
        "dbine_nbw",
        None,
        "resumen",
        &["DROP TABLE IF EXISTS resumen", "CREATE TABLE resumen (id int PRIMARY KEY, nombre varchar(50), total decimal(12,2), ultimo date)", "INSERT INTO resumen VALUES (10, 'viejo', 0, NULL), (99, 'queda', 1, NULL)"],
        "SELECT id, nombre, total FROM resumen ORDER BY id",
    )
    .await;

    let u = url(
        "DBINE_TEST_ORACLE_ADMIN_URL",
        "oracle://system:Secret123@localhost:25601/FREEPDB1",
    );
    let (base, service) = u.rsplit_once('/').unwrap();
    let mut ora = server_cfg("oracle", base);
    ora.options.insert("service".into(), service.into());
    save(&state, "ora", &ora);
    let mut o = dbine_drivers::open_session(&ora, None).await.unwrap();
    every_mode(
        &state,
        o.as_mut(),
        "ora",
        "",
        Some("SYSTEM"),
        "DBINE_NBW_RESUMEN",
        &[
            "BEGIN EXECUTE IMMEDIATE 'DROP TABLE DBINE_NBW_RESUMEN PURGE'; EXCEPTION WHEN OTHERS THEN NULL; END;",
            "CREATE TABLE DBINE_NBW_RESUMEN (ID NUMBER(10) PRIMARY KEY, NOMBRE VARCHAR2(50), TOTAL NUMBER(12,2), ULTIMO DATE)",
            "INSERT INTO DBINE_NBW_RESUMEN VALUES (10, 'viejo', 0, NULL)",
            "INSERT INTO DBINE_NBW_RESUMEN VALUES (99, 'queda', 1, NULL)",
        ],
        "SELECT ID, NOMBRE, TOTAL FROM DBINE_NBW_RESUMEN ORDER BY ID",
    )
    .await;
    // No staging table is left behind.
    let left = rows_of(
        o.as_mut(),
        r"SELECT count(*) FROM user_tables WHERE table_name LIKE 'DBINE\_W\_%' ESCAPE '\'",
    )
    .await;
    assert_eq!(texts(left), vec![vec!["0".to_string()]]);
}
