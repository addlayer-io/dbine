//! Vectors (embeddings) against real servers, one test per engine: a
//! table with a vector column and a vector index made from a `TableSchema`
//! through the driver's own DDL (as the designer would), rows inserted with
//! `insert_script`, read back (query and table browse) with the column
//! marked as vectors and the cells whole, the typed scripts (a JSON
//! column next to the vector one, with the types a describe gives), the
//! describe and the index read back (also in the index usage view), a
//! similarity query, an `update_script`, and a transfer (same engine, and
//! pgvector-shaped text cells as another family's source).
//!
//! Each test reads `DBINE_TEST_<ENGINE>_URL` (`mysql://user:pass@host:port`)
//! and is skipped without it; what it creates is dropped first and at the
//! end, also when it fails half-way.
//!
//! ```sh
//! docker start dbine-test-mysql9 dbine-test-mariadb dbine-test-tidb8 \
//!   dbine-test-greptimedb dbine-test-manticore
//! DBINE_TEST_MYSQL9_URL=mysql://root:pw@127.0.0.1:27201 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@127.0.0.1:25012 \
//! DBINE_TEST_TIDB8_URL=mysql://root@127.0.0.1:25044 \
//! DBINE_TEST_GREPTIMEDB_URL=mysql://127.0.0.1:25017 \
//! DBINE_TEST_MANTICORE_URL=mysql://127.0.0.1:25016 \
//!   cargo test -p dbine-driver-mysql --test vectors -- --ignored --nocapture
//! ```
//!
//! The heavy servers, one at a time (`docker start dbine-test-oceanbase`,
//! then `dbine-test-doris`, then `dbine-test-starrocks`; their ports with
//! `docker port`): `DBINE_TEST_OCEANBASE_URL=mysql://root@127.0.0.1:25035`,
//! `DBINE_TEST_DORIS_URL=mysql://root@127.0.0.1:<9030's port>`,
//! `DBINE_TEST_STARROCKS_URL=mysql://root@127.0.0.1:25030` (its FE needs
//! `ADMIN SET FRONTEND CONFIG ("enable_experimental_vector" = "true")`,
//! which the test sets), `DBINE_TEST_DATABEND_URL=mysql://root@127.0.0.1:<3307's port>`.
//! TiDB's vector index needs TiFlash: `DBINE_TEST_TIDB_TIFLASH_URL` points at
//! a cluster with one (`tiup playground`), the test adds the replica.

use dbine_driver::transfer::{
    BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    kinds, ColumnDef, ConnectionConfig, DdlParts, Driver, IndexDef, KeyDef, ObjectRef,
    QueryOutcome, ResultColumn, RowChange, Session, TableSchema, VectorIndex,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};

fn parse_url(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport
        .rsplit_once(':')
        .map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port,
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_mysql::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<QueryOutcome, String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10_000, &mut out)
        .await
        .map_err(|e| e.to_string())?;
    match out.error.take() {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

async fn must(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    run(s, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"))
}

/// A script's statements, without the notes (`-- …` lines) the DDL says
/// before or between them.
fn statements(script: &str) -> Vec<String> {
    script
        .split(";\n")
        .map(|st| {
            st.lines()
                .filter(|l| !l.trim_start().starts_with("--"))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .trim_end_matches(';')
                .to_string()
        })
        .filter(|st| !st.is_empty())
        .collect()
}

/// The notes of a script that say something was left out.
fn refusals(script: &str) -> Vec<&str> {
    script
        .lines()
        .filter(|l| {
            l.trim_start().starts_with("--")
                && ["no tiene", "pide", "necesita la dimensión", "no se puede"]
                    .iter()
                    .any(|w| l.contains(w))
        })
        .collect()
}

fn table(name: &str) -> ObjectRef {
    ObjectRef {
        kind: kinds::TABLE.into(),
        schema: None,
        name: name.into(),
    }
}

#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    batches: Vec<RowBatch>,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> io::Result<()> {
        self.batches.push(b);
        Ok(())
    }
}

/// An export's rows.
#[derive(Default)]
struct Rows {
    columns: Vec<dbine_driver::ResultColumn>,
    rows: Vec<Vec<Value>>,
}

impl dbine_driver::RowSink for Rows {
    fn begin(&mut self, _index: usize, columns: &[dbine_driver::ResultColumn]) -> io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn row(&mut self, _index: usize, row: &[Value]) -> io::Result<()> {
        self.rows.push(row.to_vec());
        Ok(())
    }
}

struct Source(VecDeque<RowBatch>);

#[dbine_driver::async_trait]
impl BatchSource for Source {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.pop_front()
    }
}

async fn read_cells(s: &mut Box<dyn Session>, name: &str) -> (Vec<TransferColumn>, Vec<Vec<Cell>>) {
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(
        &ReadSpec {
            table: table(name),
            columns: None,
            filter: None,
        },
        sink.clone(),
    )
    .await
    .expect("read_batches");
    let c = std::mem::take(&mut *sink.lock().unwrap());
    let mut rows: Vec<Vec<Cell>> = c.batches.into_iter().flat_map(|b| b.rows).collect();
    rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        Cell::UInt(u) => u as i64,
        _ => 0,
    });
    (c.columns, rows)
}

async fn load(
    s: &mut Box<dyn Session>,
    name: &str,
    columns: &[TransferColumn],
    rows: Vec<Vec<Cell>>,
) {
    let spec = LoadSpec {
        table: table(name),
        columns: columns.iter().map(|c| c.name.clone()).collect(),
        table_lock: false,
        keep_identity: true,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let bytes = rows.iter().flatten().map(Cell::size).sum();
    let mut source = Source([RowBatch { rows, bytes }].into_iter().collect());
    s.bulk_load(&spec, columns, &mut source, &|_| {})
        .await
        .unwrap_or_else(|e| panic!("bulk_load {name}: {e}"));
}

/// One engine's case.
struct Case {
    id: &'static str,
    env: &'static str,
    /// The database to work in (created and dropped), `None`: the server's
    /// only namespace (Manticore).
    db: Option<&'static str>,
    /// The vector column's type, as the designer writes it and the
    /// describe reads it back.
    column_type: &'static str,
    described: &'static str,
    /// The vector index the designer would make (`None`: the engine has
    /// none, or this server can't build it).
    index: Option<IndexDef>,
    /// What the catalog reads back of it.
    index_back: Option<VectorIndex>,
    /// What the DDL made from the read-back table must have (the engine's
    /// vector index syntax).
    marker: &'static str,
    /// Ordered by similarity to [1,0,0], ids in order.
    similarity: &'static str,
    /// The result header's type of the vector column.
    header: &'static str,
    /// Table options the engine needs (key model, time index…).
    options: Vec<(&'static str, &'static str)>,
    /// `update_script` works (GreptimeDB has no UPDATE).
    update: bool,
    /// Bulk load works (Manticore has none).
    transfer: bool,
    /// A schema sync that changes the vector index to this metric (the
    /// engines that rebuild an index with ALTER / CREATE INDEX).
    sync_metric: Option<&'static str>,
    /// Run before the vector index is made (TiDB's TiFlash replica), with
    /// a query that returns 1 once it's ready.
    before_index: Option<(&'static str, &'static str)>,
    /// Run on the server first (a setting the engine's vector indexes need).
    setup: Option<&'static str>,
    /// The engine's JSON type, for the typed scripts' check (a JSON
    /// column's array is its JSON text, never a vector literal).
    json_type: Option<&'static str>,
}

fn vindex(
    name: &str,
    algorithm: Option<&str>,
    metric: Option<&str>,
    params: &[(&str, &str)],
) -> IndexDef {
    IndexDef {
        name: name.into(),
        columns: vec!["v".into()],
        vector: Some(VectorIndex {
            algorithm: algorithm.map(Into::into),
            metric: metric.map(Into::into),
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }),
        ..Default::default()
    }
}

fn vi(algorithm: Option<&str>, metric: Option<&str>, params: &[(&str, &str)]) -> VectorIndex {
    vindex("", algorithm, metric, params).vector.unwrap()
}

const ROWS: [(i64, [f64; 3]); 3] = [
    (1, [1.0, 0.0, 0.0]),
    (2, [0.5, 0.5, 0.0]),
    (3, [-1.0, 2.5, 0.125]),
];

fn schema(case: &Case, name: &str, with_index: bool) -> TableSchema {
    let mut cols = vec![
        ColumnDef {
            name: "id".into(),
            data_type: if case.id == "manticore" {
                "bigint"
            } else {
                "INT"
            }
            .into(),
            nullable: false,
            ..Default::default()
        },
        ColumnDef {
            name: "v".into(),
            data_type: case.column_type.into(),
            nullable: false,
            ..Default::default()
        },
    ];
    if case.id == "greptimedb" {
        cols.push(ColumnDef {
            name: "ts".into(),
            data_type: "TIMESTAMP(3)".into(),
            nullable: false,
            default_value: Some("'2024-01-01 00:00:00'".into()),
            ..Default::default()
        });
    }
    if case.id == "manticore" {
        // `id` is implicit.
        cols.remove(0);
    }
    TableSchema {
        kind: kinds::TABLE.into(),
        name: name.into(),
        columns: cols,
        primary_key: (!matches!(case.id, "manticore" | "greptimedb")).then(|| KeyDef {
            name: None,
            columns: vec!["id".into()],
        }),
        indexes: case
            .index
            .iter()
            .filter(|_| with_index)
            .cloned()
            .map(|mut ix| {
                if case.id == "manticore" {
                    ix.name = "v".into();
                }
                ix
            })
            .collect(),
        options: case
            .options
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    }
}

async fn cleanup(admin: &mut Box<dyn Session>, s: Option<&mut Box<dyn Session>>, case: &Case) {
    match case.db {
        Some(db) => {
            let _ = run(admin, &format!("DROP DATABASE IF EXISTS {db}")).await;
        }
        None => {
            if let Some(s) = s {
                for t in ["vec_items", "vec_copy", "vec_pg", "vec_typed"] {
                    let _ = run(s, &format!("DROP TABLE IF EXISTS {t}")).await;
                }
            }
        }
    }
}

async fn exercise(case: Case) {
    let Ok(url) = std::env::var(case.env) else {
        eprintln!("{} not set; skipping", case.env);
        return;
    };
    let d = driver(case.id);
    let cfg = parse_url(case.id, &url);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    eprintln!(
        "{}: {}",
        case.id,
        admin.server_version().await.unwrap_or_default()
    );
    if let Some(setup) = case.setup {
        must(&mut admin, setup).await;
    }
    cleanup(&mut admin, None, &case).await;
    if let Some(db) = case.db {
        must(&mut admin, &format!("CREATE DATABASE {db}")).await;
    }
    let mut s = d
        .connect(&cfg, case.db)
        .await
        .expect("connect to the test database");
    if case.db.is_none() {
        cleanup(&mut admin, Some(&mut s), &case).await;
    }
    // Whatever fails below, the database goes.
    let outcome = tokio::spawn(steps(case.id, url.clone(), case.db, CaseData::from(&case))).await;
    cleanup(&mut admin, Some(&mut s), &case).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

/// What `steps` needs of a case (it runs in its own task, so that a
/// failure still leaves the cleanup to the caller).
struct CaseData {
    schema: TableSchema,
    copy: TableSchema,
    described: String,
    index_back: Option<VectorIndex>,
    marker: String,
    similarity: String,
    header: String,
    update: bool,
    transfer: bool,
    sync_metric: Option<String>,
    before_index: Option<(String, String)>,
    /// The table of the typed scripts' check: the case's plus a JSON `j`.
    typed: Option<TableSchema>,
}

impl From<&Case> for CaseData {
    fn from(c: &Case) -> Self {
        CaseData {
            schema: schema(c, "vec_items", true),
            copy: schema(c, "vec_copy", false),
            described: c.described.into(),
            index_back: c.index_back.clone(),
            marker: c.marker.into(),
            similarity: c.similarity.into(),
            header: c.header.into(),
            update: c.update,
            transfer: c.transfer,
            sync_metric: c.sync_metric.map(Into::into),
            before_index: c.before_index.map(|(a, b)| (a.into(), b.into())),
            typed: c.json_type.map(|j| {
                let mut t = schema(c, "vec_typed", false);
                t.columns.push(ColumnDef {
                    name: "j".into(),
                    data_type: j.into(),
                    nullable: true,
                    ..Default::default()
                });
                t
            }),
        }
    }
}

/// The typed check's rows (`id`, `v`, `j`), ordered by id: the vector's
/// cell as it came, the JSON's parsed.
async fn typed_rows(s: &mut Box<dyn Session>, id: &str) -> Vec<(i64, Value, Value)> {
    let sql = if id == "manticore" {
        "SELECT id, v, j FROM vec_typed ORDER BY id ASC"
    } else {
        "SELECT `id`, `v`, `j` FROM `vec_typed` ORDER BY `id`"
    };
    let out = must(s, sql).await;
    out.results[0]
        .rows
        .iter()
        .map(|r| {
            let n = r[0]
                .as_i64()
                .or_else(|| r[0].as_str().and_then(|x| x.parse().ok()))
                .unwrap();
            let j = match &r[2] {
                Value::String(t) => serde_json::from_str(t).unwrap_or_else(|_| r[2].clone()),
                other => other.clone(),
            };
            (n, r[1].clone(), j)
        })
        .collect()
}

async fn steps(id: &'static str, url: String, db: Option<&'static str>, c: CaseData) {
    let d = driver(id);
    let cfg = parse_url(id, &url);
    let mut s = d.connect(&cfg, db).await.expect("connect");
    let all = DdlParts {
        create: true,
        indexes: true,
        ..Default::default()
    };

    // 1. The table and its vector index, from the designer's schema.
    let ddl = d.table_ddl(&c.schema, all).expect("table_ddl");
    eprintln!("{id}: {ddl}");
    assert!(
        !ddl.contains("CREATE INDEX `iv` ON `vec_items` (`v`);"),
        "{ddl}"
    );
    assert!(refusals(&ddl).is_empty(), "{ddl}");
    let mut prepared = c.before_index.is_none();
    for stmt in statements(&ddl) {
        if !prepared && stmt.contains("VECTOR INDEX") {
            let (sql, ready) = c.before_index.as_ref().unwrap();
            must(&mut s, sql).await;
            for _ in 0..120 {
                let out = must(&mut s, ready).await;
                if out.results[0]
                    .rows
                    .first()
                    .map(|r| r[0].to_string().trim_matches('"').to_string())
                    == Some("1".into())
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            prepared = true;
        }
        must(&mut s, &stmt).await;
    }

    // 2. Rows from insert_script, vectors as JSON arrays.
    let mut columns: Vec<String> = vec!["id".into(), "v".into()];
    let rows: Vec<Vec<Value>> = ROWS.iter().map(|(i, v)| vec![json!(i), json!(v)]).collect();
    if id == "greptimedb" {
        columns.push("ts".into());
    }
    let rows: Vec<Vec<Value>> = rows
        .into_iter()
        .enumerate()
        .map(|(n, mut r)| {
            if id == "greptimedb" {
                r.push(json!(format!("2024-01-01 00:00:0{n}")));
            }
            r
        })
        .collect();
    let ins = d
        .insert_script(&table("vec_items"), &columns, &rows)
        .expect("insert_script");
    eprintln!("{id}: {ins}");
    for stmt in statements(&ins) {
        must(&mut s, &stmt).await;
    }
    if matches!(id, "doris" | "starrocks") {
        // Inserts land asynchronously on the OLAP engines' visibility.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
    let expect = ["[1,0,0]", "[0.5,0.5,0]", "[-1,2.5,0.125]"];

    // 3. Read back: a query and the table browse.
    for sql in [
        // Manticore doesn't sort by a quoted name; GreptimeDB needs `id` quoted.
        if id == "manticore" {
            "SELECT id, v FROM vec_items ORDER BY id ASC".to_string()
        } else {
            "SELECT `id`, `v` FROM `vec_items` ORDER BY `id`".to_string()
        },
        s.browse_query(&table("vec_items"), 100),
    ] {
        let out = must(&mut s, &sql).await;
        let r = &out.results[0];
        let vi = r
            .columns
            .iter()
            .position(|c| c.name == "v")
            .expect("v column");
        let ii = r
            .columns
            .iter()
            .position(|c| c.name == "id")
            .expect("id column");
        let col = &r.columns[vi];
        let meta = col
            .vector
            .as_ref()
            .unwrap_or_else(|| panic!("{sql}: {col:?}"));
        assert!(!meta.sparse);
        assert_eq!(
            col.type_name.to_ascii_lowercase(),
            c.header.to_ascii_lowercase(),
            "{sql}"
        );
        let mut cells: Vec<(String, Value)> = r
            .rows
            .iter()
            .map(|row| (row[ii].to_string(), row[vi].clone()))
            .collect();
        cells.sort_by(|a, b| a.0.cmp(&b.0));
        let cells: Vec<Value> = cells.into_iter().map(|(_, v)| v).collect();
        assert_eq!(cells, expect.map(|e| json!(e)), "{sql}");
    }

    // 3a. The lookup that marks the vector columns runs on a connection of
    // its own: a fresh session's first read leaves FOUND_ROWS() to the
    // user's statements. MariaDB also marks an expression's vector.
    // (TiDB has no SQL_CALC_FOUND_ROWS; MySQL 9's vectors need no lookup.)
    if matches!(id, "mariadb" | "mysql") {
        let mut fresh = d.connect(&cfg, db).await.expect("connect");
        let out = must(
            &mut fresh,
            "SELECT SQL_CALC_FOUND_ROWS `id`, `v` FROM `vec_items` ORDER BY `id` LIMIT 1",
        )
        .await;
        assert!(
            out.results[0].columns[1].vector.is_some(),
            "{:?}",
            out.results[0].columns
        );
        let out = must(&mut fresh, "SELECT FOUND_ROWS()").await;
        assert_eq!(
            out.results[0].rows[0][0].to_string().trim_matches('"'),
            "3",
            "FOUND_ROWS() after the lookup"
        );
        if id == "mariadb" {
            let out = must(&mut fresh, "SELECT VEC_FromText('[1,2.5,-3]')").await;
            let r = &out.results[0];
            assert!(r.columns[0].vector.is_some(), "{:?}", r.columns[0]);
            assert_eq!(r.rows[0][0], json!("[1,2.5,-3]"));
        }
    }

    // 3b. An export (rows to a sink, none kept): the header and the cells
    // the same, on a fresh session that never looked the table up.
    {
        let mut fresh = d.connect(&cfg, db).await.expect("connect");
        let sink = Arc::new(Mutex::new(Rows::default()));
        let mut out = QueryOutcome {
            sink: Some(dbine_driver::RowSinkRef(sink.clone())),
            ..Default::default()
        };
        let sql = fresh.browse_query(&table("vec_items"), 100);
        fresh.execute(&sql, 100, &mut out).await.expect("export");
        let got = std::mem::take(&mut *sink.lock().unwrap());
        let vi = got.columns.iter().position(|c| c.name == "v").unwrap();
        assert!(got.columns[vi].vector.is_some(), "{:?}", got.columns[vi]);
        let mut cells: Vec<String> = got
            .rows
            .iter()
            .map(|r| r[vi].as_str().unwrap_or("").to_string())
            .collect();
        cells.sort();
        let mut want: Vec<String> = expect.iter().map(|e| e.to_string()).collect();
        want.sort();
        assert_eq!(cells, want);
    }

    // 3c. A protected read (MCP, the assistant): MySQL and MariaDB run it
    // in a read-only transaction; the vectors come the same.
    {
        let mut out = QueryOutcome::default();
        match s
            .run_read_only(
                "SELECT `id`, `v` FROM `vec_items` ORDER BY `id`",
                100,
                &mut out,
            )
            .await
        {
            Ok(()) => {
                let r = &out.results[0];
                assert!(r.columns[1].vector.is_some(), "{:?}", r.columns[1]);
                let cells: Vec<Value> = r.rows.iter().map(|row| row[1].clone()).collect();
                assert_eq!(cells, expect.map(|e| json!(e)));
                eprintln!("{id}: protected read ok");
            }
            Err(dbine_driver::Error::Unsupported(_)) => {}
            Err(e) => panic!("run_read_only: {e}"),
        }
    }

    // 3d. Typed scripts, with the types a describe gives: a JSON column's
    // array is its JSON text and a vector column's text value the
    // engine's vector literal (untyped, both would be vector literals and
    // the text a plain string).
    if let Some(typed) = &c.typed {
        let ddl = d
            .table_ddl(
                typed,
                DdlParts {
                    create: true,
                    ..Default::default()
                },
            )
            .expect("typed table_ddl");
        for stmt in statements(&ddl) {
            must(&mut s, &stmt).await;
        }
        let described = s.database_schema().await.expect("database_schema");
        let types: Vec<ResultColumn> = described
            .iter()
            .find(|t| t.name == "vec_typed")
            .expect("vec_typed described")
            .columns
            .iter()
            .map(|col| ResultColumn::new(col.name.clone(), col.data_type.clone()))
            .collect();
        eprintln!("{id}: typed columns {types:?}");
        let row = |n: i64, v: Value, j: Value| -> Vec<Value> {
            types
                .iter()
                .map(|col| match col.name.as_str() {
                    "id" => json!(n),
                    "v" => v.clone(),
                    "j" => j.clone(),
                    _ => json!(format!("2024-01-01 00:00:0{n}")),
                })
                .collect()
        };
        let rows = vec![
            row(1, json!([1, 0, 0]), json!([1, 2, 3])),
            row(2, json!("[0.5, 0.5, 0]"), json!([4, 5])),
        ];
        let ins = d
            .insert_script_typed(&table("vec_typed"), &types, &rows)
            .expect("insert_script_typed");
        eprintln!("{id}: typed {ins}");
        for stmt in statements(&ins) {
            must(&mut s, &stmt).await;
        }
        if matches!(id, "doris" | "starrocks") {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        assert_eq!(
            typed_rows(&mut s, id).await,
            [
                (1, json!("[1,0,0]"), json!([1, 2, 3])),
                (2, json!("[0.5,0.5,0]"), json!([4, 5]))
            ]
        );
        if c.update {
            let up = d
                .update_script_typed(
                    &table("vec_typed"),
                    &[RowChange {
                        key: vec![("id".into(), json!(2))],
                        set: vec![
                            ("v".into(), json!("[0.25, -4, 8]")),
                            ("j".into(), json!([7])),
                        ],
                        row: Vec::new(),
                    }],
                    &types,
                )
                .expect("update_script_typed");
            eprintln!("{id}: typed {up}");
            for stmt in statements(&up) {
                must(&mut s, &stmt).await;
            }
        }
        let del = d
            .delete_script_typed(
                &table("vec_typed"),
                &[vec![("id".to_string(), json!(1))]],
                &types,
            )
            .expect("delete_script_typed");
        eprintln!("{id}: typed {del}");
        for stmt in statements(&del) {
            must(&mut s, &stmt).await;
        }
        if matches!(id, "doris" | "starrocks") {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        let left = typed_rows(&mut s, id).await;
        let want = if c.update {
            (2, json!("[0.25,-4,8]"), json!([7]))
        } else {
            (2, json!("[0.5,0.5,0]"), json!([4, 5]))
        };
        assert_eq!(left, [want]);
        eprintln!("{id}: typed scripts ok");
    }

    // 4. The describe keeps the type with its dimension; 5. the index, its
    // vector settings.
    let schema = s.database_schema().await.expect("database_schema");
    let t = schema
        .iter()
        .find(|t| t.name == "vec_items")
        .expect("table described");
    let v = t.columns.iter().find(|c| c.name == "v").unwrap();
    assert_eq!(
        v.data_type.to_ascii_lowercase(),
        c.described.to_ascii_lowercase()
    );
    if let Some(back) = &c.index_back {
        let ix = t
            .indexes
            .iter()
            .find(|i| i.vector.is_some())
            .unwrap_or_else(|| panic!("no vector index read back: {:?}", t.indexes));
        assert_eq!(ix.columns, ["v"]);
        // Its settings: the ones given, and the engine's defaults it may
        // write out.
        let got = ix.vector.as_ref().unwrap();
        assert_eq!(
            (got.algorithm.as_deref(), got.metric.as_deref()),
            (back.algorithm.as_deref(), back.metric.as_deref()),
            "{ix:?}"
        );
        for (k, val) in &back.params {
            assert_eq!(got.params.get(k), Some(val), "{k}: {ix:?}");
        }
        // The DDL made from what was read is the same index again.
        let again = d.table_ddl(t, all).unwrap();
        assert!(again.contains(&c.marker), "{again}");
        assert!(refusals(&again).is_empty(), "{again}");
        // 5b. The index view has the same vector settings (Manticore has
        // no index list: its tables are the index).
        match s.index_usage(&table("vec_items")).await {
            Ok(Some(rep)) => {
                let u = rep
                    .indexes
                    .iter()
                    .find(|u| u.name == ix.name)
                    .unwrap_or_else(|| panic!("{} not in {:?}", ix.name, rep.indexes));
                assert_eq!(u.vector, ix.vector, "{u:?}");
                eprintln!("{id}: index usage {u:?}");
            }
            Ok(None) => assert_eq!(id, "manticore"),
            Err(e) => panic!("index_usage: {e}"),
        }
    }

    // 4b. A schema sync that changes the index's distance: the old one
    // dropped, the new one created with the engine's syntax.
    if let Some(m) = &c.sync_metric {
        let mut new = t.clone();
        let ix = new.indexes.iter_mut().find(|i| i.vector.is_some()).unwrap();
        // As the designer does: the vector settings change, the native
        // options the index was read with stay.
        ix.vector.as_mut().unwrap().metric = Some(m.clone());
        let script = d
            .sync_script(&[dbine_driver::TableChange::Alter {
                old: t.clone(),
                new,
            }])
            .expect("sync_script");
        eprintln!("{id}: sync {:?}", script.statements);
        for stmt in &script.statements {
            for one in statements(stmt) {
                must(&mut s, &one).await;
            }
        }
        if matches!(id, "doris" | "starrocks") {
            // The index change is an asynchronous job.
            for _ in 0..60 {
                let t = s.database_schema().await.unwrap();
                let t = t.iter().find(|t| t.name == "vec_items").unwrap();
                if t.indexes.iter().any(|i| {
                    i.vector.as_ref().and_then(|v| v.metric.as_deref()) == Some(m.as_str())
                }) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
        let after = s.database_schema().await.unwrap();
        let t = after.iter().find(|t| t.name == "vec_items").unwrap();
        let ix = t
            .indexes
            .iter()
            .find(|i| i.vector.is_some())
            .expect("index after sync");
        assert_eq!(
            ix.vector.as_ref().unwrap().metric.as_deref(),
            Some(m.as_str())
        );
    }

    // 6. Similarity.
    let out = must(&mut s, &c.similarity).await;
    let ids: Vec<i64> = out.results[0]
        .rows
        .iter()
        .map(|r| {
            r[0].as_i64()
                .or_else(|| r[0].as_str().and_then(|x| x.parse().ok()))
                .unwrap()
        })
        .collect();
    assert_eq!(ids.first(), Some(&1), "{ids:?}");

    // 7. update_script.
    if c.update {
        let up = d
            .update_script(
                &table("vec_items"),
                &[RowChange {
                    key: vec![("id".into(), json!(2))],
                    set: vec![("v".into(), json!([0.25, -4, 8]))],
                    row: Vec::new(),
                }],
            )
            .expect("update_script");
        eprintln!("{id}: {up}");
        must(&mut s, up.trim_end_matches(';')).await;
        if matches!(id, "doris" | "starrocks") {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        let out = must(&mut s, "SELECT `v` FROM `vec_items` WHERE `id` = 2").await;
        assert_eq!(out.results[0].rows[0][0], json!("[0.25,-4,8]"));
    }

    // 8. Transfer: read, load into a copy, read the copy; then text cells
    // as pgvector's reader sends them.
    // Manticore has no bulk load (its moves write insert_script): its read
    // is checked.
    if !c.transfer {
        let (cols, rows) = read_cells(&mut s, "vec_items").await;
        let vpos = cols.iter().position(|c| c.name == "v").unwrap();
        assert!(
            matches!(&rows[0][vpos], Cell::Text(t) if t == "[1,0,0]"),
            "{:?}",
            rows[0][vpos]
        );
    }
    if c.transfer {
        let (cols, rows) = read_cells(&mut s, "vec_items").await;
        let vpos = cols.iter().position(|c| c.name == "v").unwrap();
        assert!(
            matches!(&rows[0][vpos], Cell::Text(t) if t == "[1,0,0]"),
            "{:?}",
            rows[0][vpos]
        );
        let ddl = d
            .table_ddl(
                &c.copy,
                DdlParts {
                    create: true,
                    ..Default::default()
                },
            )
            .unwrap();
        for stmt in statements(&ddl) {
            must(&mut s, &stmt).await;
        }
        load(&mut s, "vec_copy", &cols, rows.clone()).await;
        if matches!(id, "doris" | "starrocks") {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        let (_, back) = read_cells(&mut s, "vec_copy").await;
        assert_eq!(back, rows);
        // Another family's source: pgvector's text, with its spacing.
        // (A whole-table delete: Doris' DUPLICATE KEY tables take simple
        // predicates only.)
        must(&mut s, "DELETE FROM vec_copy WHERE id > 0").await;
        if matches!(id, "doris" | "starrocks") {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        let pg: Vec<Vec<Cell>> = rows
            .iter()
            .enumerate()
            .map(|(n, r)| {
                let mut r = r.clone();
                r[vpos] = Cell::Text(["[1,0,0]", "[0.5, 0.5, 0]", "[-1,2.5,0.125]"][n].into());
                r
            })
            .collect();
        load(&mut s, "vec_copy", &cols, pg).await;
        if matches!(id, "doris" | "starrocks") {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        let (_, back) = read_cells(&mut s, "vec_copy").await;
        let got: Vec<&Cell> = back.iter().map(|r| &r[vpos]).collect();
        let want: Vec<Cell> = ["[1,0,0]", "[0.5,0.5,0]", "[-1,2.5,0.125]"]
            .iter()
            .map(|t| Cell::Text(t.to_string()))
            .collect();
        assert_eq!(got, want.iter().collect::<Vec<_>>());

        // A join's vector column is marked too (GreptimeDB's and
        // Manticore's columns don't name their table: the query's tables
        // are looked up). Manticore joins by full table names.
        {
            let mut fresh = d.connect(&cfg, db).await.expect("connect");
            let sql = if id == "manticore" {
                "SELECT vec_items.id, vec_items.v FROM vec_items INNER JOIN vec_copy ON vec_items.id = vec_copy.id ORDER BY vec_items.id ASC"
            } else {
                "SELECT a.id, a.v FROM vec_items a JOIN vec_copy b ON a.id = b.id ORDER BY a.id"
            };
            let out = must(&mut fresh, sql).await;
            let r = &out.results[0];
            assert!(r.columns[1].vector.is_some(), "join: {:?}", r.columns[1]);
            assert_eq!(r.rows.len(), 3);
            assert_eq!(r.rows[0][1], json!("[1,0,0]"));
        }
    }
}

#[tokio::test]
#[ignore]
async fn mysql9_vectors() {
    exercise(Case {
        id: "mysql",
        env: "DBINE_TEST_MYSQL9_URL",
        db: Some("dbine_vec"),
        column_type: "vector(3)",
        described: "vector(3)",
        // MySQL Community has no vector index.
        index: None,
        index_back: None,
        marker: "`v` vector(3) NOT NULL",
        // Community has no distance function: a dot product over the
        // vector's text.
        similarity: "SELECT id FROM vec_items, JSON_TABLE(VECTOR_TO_STRING(v), '$[*]' \
                     COLUMNS(n FOR ORDINALITY, x DOUBLE PATH '$')) e GROUP BY id \
                     ORDER BY SUM(x * JSON_EXTRACT('[1,0,0]', CONCAT('$[', n - 1, ']'))) DESC",
        header: "vector(3)",
        options: vec![],
        update: true,
        transfer: true,
        sync_metric: None,
        before_index: None,
        setup: None,
        json_type: Some("json"),
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn mariadb_vectors() {
    exercise(Case {
        id: "mariadb",
        env: "DBINE_TEST_MARIADB_URL",
        db: Some("dbine_vec"),
        column_type: "vector(3)",
        described: "vector(3)",
        index: Some(vindex("iv", None, Some("cosine"), &[("m", "8")])),
        index_back: Some(vi(None, Some("cosine"), &[("m", "8")])),
        marker: "CREATE VECTOR INDEX `iv` ON `vec_items` (`v`) M=8 DISTANCE=cosine;",
        similarity: "SELECT id FROM vec_items ORDER BY VEC_DISTANCE_COSINE(v, VEC_FromText('[1,0,0]')) LIMIT 3",
        header: "vector(3)",
        options: vec![],
        update: true,
        transfer: true,
        sync_metric: Some("l2"),
        before_index: None,
        setup: None,
        json_type: Some("JSON"),
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn tidb_vectors() {
    exercise(Case {
        id: "tidb",
        env: "DBINE_TEST_TIDB8_URL",
        db: Some("dbine_vec"),
        column_type: "vector(3)",
        described: "vector(3)",
        // The container has no TiFlash, which vector indexes need: their
        // DDL is covered by the unit tests.
        index: None,
        index_back: None,
        marker: "`v` vector(3) NOT NULL",
        similarity: "SELECT id FROM vec_items ORDER BY VEC_COSINE_DISTANCE(v, '[1,0,0]') LIMIT 3",
        header: "vector(3)",
        options: vec![],
        update: true,
        transfer: true,
        sync_metric: None,
        before_index: None,
        setup: None,
        json_type: Some("json"),
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn greptimedb_vectors() {
    exercise(Case {
        id: "greptimedb",
        env: "DBINE_TEST_GREPTIMEDB_URL",
        db: Some("dbine_vec"),
        column_type: "VECTOR(3)",
        described: "vector(3)",
        index: Some(vindex("iv", None, Some("cosine"), &[("m", "16")])),
        // Its defaults come back written out.
        index_back: Some(vi(
            None,
            Some("cosine"),
            &[("m", "16"), ("ef_construction", "128"), ("ef_search", "64")],
        )),
        marker: "VECTOR INDEX WITH(metric = 'cosine', expansion_add = '128', expansion_search = '64', connectivity = '16')",
        similarity: "SELECT `id` FROM vec_items ORDER BY vec_cos_distance(v, '[1,0,0]') LIMIT 3",
        header: "vector(3)",
        options: vec![],
        // No UPDATE: a row is replaced by inserting it again.
        update: false,
        transfer: true,
        sync_metric: None,
        before_index: None,
        setup: None,
        json_type: Some("JSON"),
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn manticore_vectors() {
    exercise(Case {
        id: "manticore",
        env: "DBINE_TEST_MANTICORE_URL",
        db: None,
        column_type: "float_vector",
        described: "float_vector",
        index: Some(vindex("v", Some("hnsw"), Some("l2"), &[("knn_dims", "3")])),
        index_back: Some(vi(Some("hnsw"), Some("l2"), &[("knn_dims", "3")])),
        marker: "`v` float_vector knn_type='hnsw' knn_dims='3' hnsw_similarity='L2'",
        similarity: "SELECT id FROM vec_items WHERE knn(v, 3, (1,0,0))",
        header: "float_vector",
        options: vec![],
        update: true,
        // Its load writes INSERTs with its vectors as (…).
        transfer: true,
        sync_metric: None,
        before_index: None,
        setup: None,
        json_type: Some("json"),
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn tidb_tiflash_vector_index() {
    // A cluster with TiFlash (the vector index lives there).
    exercise(Case {
        id: "tidb",
        env: "DBINE_TEST_TIDB_TIFLASH_URL",
        db: Some("dbine_vec"),
        column_type: "vector(3)",
        described: "vector(3)",
        index: Some(vindex("iv", Some("hnsw"), Some("cosine"), &[])),
        index_back: Some(vi(Some("hnsw"), Some("cosine"), &[])),
        marker: "CREATE VECTOR INDEX `iv` ON `vec_items` ((VEC_COSINE_DISTANCE(`v`))) USING HNSW;",
        similarity: "SELECT id FROM vec_items ORDER BY VEC_COSINE_DISTANCE(v, '[1,0,0]') LIMIT 3",
        header: "vector(3)",
        options: vec![],
        update: true,
        transfer: true,
        sync_metric: Some("l2"),
        before_index: Some((
            "ALTER TABLE vec_items SET TIFLASH REPLICA 1",
            "SELECT AVAILABLE FROM information_schema.TIFLASH_REPLICA WHERE TABLE_SCHEMA = 'dbine_vec' AND TABLE_NAME = 'vec_items'",
        )),
        setup: None,
        json_type: Some("json"),
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn oceanbase_vectors() {
    exercise(Case {
        id: "oceanbase",
        env: "DBINE_TEST_OCEANBASE_URL",
        db: Some("dbine_vec"),
        column_type: "vector(3)",
        described: "vector(3)",
        index: Some(vindex("iv", Some("hnsw"), Some("l2"), &[("m", "16")])),
        index_back: Some(vi(Some("hnsw"), Some("l2"), &[("m", "16")])),
        marker:
            "CREATE VECTOR INDEX `iv` ON `vec_items` (`v`) WITH (distance=L2, type=hnsw, lib=vsag",
        similarity: "SELECT id FROM vec_items ORDER BY l2_distance(v, '[1,0,0]') LIMIT 3",
        header: "vector(3)",
        options: vec![],
        update: true,
        transfer: true,
        sync_metric: Some("cosine"),
        before_index: None,
        setup: None,
        json_type: Some("json"),
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn doris_vectors() {
    exercise(Case {
        id: "doris",
        env: "DBINE_TEST_DORIS_URL",
        db: Some("dbine_vec"),
        column_type: "ARRAY<FLOAT>",
        described: "array<float>",
        index: Some(vindex(
            "iv",
            Some("hnsw"),
            Some("l2"),
            &[("dim", "3"), ("m", "32")],
        )),
        index_back: Some(vi(Some("hnsw"), Some("l2"), &[("dim", "3"), ("m", "32")])),
        marker: "USING ANN PROPERTIES(\"index_type\" = \"hnsw\", \"metric_type\" = \"l2_distance\", \"dim\" = \"3\"",
        similarity: "SELECT id FROM vec_items ORDER BY l2_distance(v, [1,0,0]) LIMIT 3",
        header: "array<float>",
        // ANN indexes are on DUPLICATE KEY tables, which take no UPDATE.
        options: vec![("key_model", "duplicate"), ("replication_num", "1"), ("buckets", "1")],
        update: false,
        transfer: true,
        sync_metric: Some("ip"),
        before_index: None,
        setup: None,
        json_type: Some("JSON"),
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn starrocks_vectors() {
    exercise(Case {
        id: "starrocks",
        env: "DBINE_TEST_STARROCKS_URL",
        db: Some("dbine_vec"),
        column_type: "ARRAY<FLOAT>",
        described: "array<float>",
        index: Some(vindex(
            "iv",
            Some("hnsw"),
            Some("l2"),
            &[("dim", "3"), ("m", "16")],
        )),
        index_back: Some(vi(Some("hnsw"), Some("l2"), &[("dim", "3"), ("m", "16")])),
        marker: "USING VECTOR (\"index_type\" = \"hnsw\", \"metric_type\" = \"l2_distance\", \"dim\" = \"3\"",
        similarity: "SELECT id FROM vec_items ORDER BY approx_l2_distance([1,0,0], v) LIMIT 3",
        header: "array<float>",
        options: vec![("key_model", "duplicate"), ("replication_num", "1"), ("buckets", "1")],
        update: false,
        transfer: true,
        sync_metric: Some("cosine"),
        before_index: None,
        // Its vector indexes are experimental: the FE has to allow them.
        setup: Some("ADMIN SET FRONTEND CONFIG (\"enable_experimental_vector\" = \"true\")"),
        json_type: Some("JSON"),
    })
    .await;
}

#[tokio::test]
#[ignore]
async fn databend_vectors() {
    exercise(Case {
        id: "databend",
        env: "DBINE_TEST_DATABEND_URL",
        db: Some("dbine_vec"),
        column_type: "VECTOR(3)",
        described: "vector(3)",
        index: Some(vindex("iv", Some("hnsw"), Some("l2"), &[("m", "16")])),
        index_back: Some(vi(Some("hnsw"), Some("l2"), &[("m", "16")])),
        marker: "CREATE VECTOR INDEX `iv` ON `vec_items`(`v`)",
        similarity: "SELECT id FROM vec_items ORDER BY l2_distance(v, [1,0,0]::VECTOR(3)) LIMIT 3",
        header: "vector(3)",
        options: vec![],
        update: true,
        transfer: true,
        sync_metric: Some("cosine"),
        before_index: None,
        setup: None,
        json_type: Some("VARIANT"),
    })
    .await;
}
