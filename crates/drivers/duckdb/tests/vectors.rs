//! Vectors in DuckDB: `FLOAT[n]` / `DOUBLE[n]` columns and vss's HNSW
//! index, end to end on a temporary database file:
//! `cargo test -p dbine-driver-duckdb --test vectors -- --ignored`.
//! It needs the network once (DuckDB downloads the vss extension); vss is
//! removed again if it wasn't installed before. When `docker` reaches
//! `dbine-test-pgvector`, rows also move to and from pgvector through
//! `psql`, in a database of their own (the vector text is the contract).
//!
//! Everything it makes is removed first and at the end, even when an
//! assertion fails (the body runs in a task of its own).

use dbine_driver::transfer::{
    BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    kinds, ColumnDef, ConnectionConfig, DdlParts, Driver, IndexDef, KeyDef, ObjectRef,
    QueryOutcome, RowChange, Session, TableChange, TableSchema, VectorElement, VectorIndex,
};
use serde_json::json;
use std::process::Command;
use std::sync::{Arc, Mutex};

fn driver() -> Arc<dyn Driver> {
    dbine_driver_duckdb::drivers().remove(0)
}

async fn open(path: &str) -> Box<dyn Session> {
    let cfg = ConnectionConfig {
        driver: "duckdb".into(),
        host: path.into(),
        ..Default::default()
    };
    driver().connect(&cfg, None).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}\n{e}"));
    out
}

fn table(name: &str) -> ObjectRef {
    ObjectRef {
        kind: kinds::TABLE.into(),
        schema: Some("main".into()),
        name: name.into(),
    }
}

fn col(name: &str, ty: &str, nullable: bool) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable,
        ..Default::default()
    }
}

/// The designer's table: a FLOAT vector with a cosine HNSW index and a
/// DOUBLE vector.
fn schema(name: &str) -> TableSchema {
    TableSchema {
        kind: kinds::TABLE.into(),
        schema: Some("main".into()),
        name: name.into(),
        columns: vec![
            col("id", "INTEGER", false),
            col("emb", "FLOAT[3]", true),
            col("emb64", "DOUBLE[2]", true),
        ],
        primary_key: Some(KeyDef {
            name: None,
            columns: vec!["id".into()],
        }),
        indexes: vec![IndexDef {
            name: format!("{name}_emb"),
            columns: vec!["emb".into()],
            vector: Some(VectorIndex {
                algorithm: Some("hnsw".into()),
                metric: Some("cosine".into()),
                params: [("m".to_string(), "16".to_string())].into(),
            }),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn all_parts() -> DdlParts {
    DdlParts {
        create: true,
        indexes: true,
        ..Default::default()
    }
}

#[derive(Default)]
struct Collect {
    cols: Vec<TransferColumn>,
    rows: Vec<Vec<Cell>>,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> std::io::Result<()> {
        self.cols = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> std::io::Result<()> {
        self.rows.extend(b.rows);
        Ok(())
    }
}

async fn read_all(s: &mut Box<dyn Session>, name: &str) -> (Vec<TransferColumn>, Vec<Vec<Cell>>) {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec {
        table: table(name),
        columns: None,
        filter: None,
    };
    s.read_batches(&spec, sink.clone()).await.unwrap();
    let c = std::mem::take(&mut *sink.lock().unwrap());
    (c.cols, c.rows)
}

struct Rows(Option<Vec<Vec<Cell>>>);

#[dbine_driver::async_trait]
impl BatchSource for Rows {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.take().map(|rows| RowBatch { rows, bytes: 0 })
    }
}

async fn load(s: &mut Box<dyn Session>, name: &str, cols: &[&str], rows: Vec<Vec<Cell>>) -> u64 {
    let spec = LoadSpec {
        table: table(name),
        columns: cols.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: true,
        commit_rows: 0,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    s.bulk_load(&spec, &[], &mut Rows(Some(rows)), &|_| {})
        .await
        .unwrap()
}

fn text(s: &str) -> Cell {
    Cell::Text(s.into())
}

/// A vector cell as DuckDB's read sends it (JSON, like its other arrays).
fn vec_cell(s: &str) -> Cell {
    Cell::Json(s.into())
}

/// psql on the pgvector container, in database `db`.
fn psql_in(db: &str, sql: &str) -> Option<String> {
    let out = Command::new("docker")
        .args([
            "exec",
            "-i",
            "dbine-test-pgvector",
            "psql",
            "-U",
            "postgres",
            "-d",
            db,
            "-At",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            sql,
        ])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Where vss is installed, if it is (asked of an in-memory database).
async fn vss_installed() -> Option<String> {
    let mut s = open(":memory:").await;
    let out = run(
        &mut s,
        "SELECT install_path FROM duckdb_extensions() WHERE extension_name = 'vss' AND installed",
    )
    .await;
    out.results[0]
        .rows
        .first()
        .and_then(|r| r[0].as_str())
        .map(str::to_string)
}

fn remove_db(path: &str) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{path}.wal"));
}

#[tokio::test]
#[ignore]
async fn vectors_end_to_end() {
    let pid = std::process::id();
    let path = std::env::temp_dir()
        .join(format!("dbine-duck-vec-{pid}.duckdb"))
        .to_string_lossy()
        .to_string();
    let pg_db = format!("dbine_vec_duckdb_{pid}");
    remove_db(&path);
    let vss_before = vss_installed().await;
    let pg = psql_in("postgres", &format!("DROP DATABASE IF EXISTS {pg_db}")).is_some()
        && psql_in("postgres", &format!("CREATE DATABASE {pg_db}")).is_some()
        && psql_in(&pg_db, "CREATE EXTENSION vector").is_some();

    let body = {
        let (path, pg_db, vss_before) = (path.clone(), pg_db.clone(), vss_before.clone());
        tokio::spawn(async move {
            let psql = |sql: &str| psql_in(&pg_db, sql);
            let d = driver();
            {
                let mut s = open(&path).await;
                // 1. The designer's DDL, vss's HNSW index included.
                let ddl = d.table_ddl(&schema("t"), all_parts()).unwrap();
                assert!(
                    ddl.contains("CREATE INDEX \"t_emb\" ON \"main\".\"t\" USING HNSW (\"emb\") WITH (metric = 'cosine', M = 16);"),
                    "{ddl}"
                );
                run(&mut s, &ddl).await;

                // 2. Rows through insert_script.
                let cols: Vec<String> = ["id", "emb", "emb64"]
                    .iter()
                    .map(|c| c.to_string())
                    .collect();
                let ins = d
                    .insert_script(
                        &table("t"),
                        &cols,
                        &[
                            vec![json!(1), json!([1, 0, 0]), json!([0.1, -2])],
                            vec![json!(2), json!([0, 1, 0.25]), json!(null)],
                            vec![json!(3), json!([0.9, 0.1, 0]), json!([1e-7, 4])],
                        ],
                    )
                    .unwrap();
                assert!(ins.contains("'[1,0,0]'"), "{ins}");
                run(&mut s, &ins).await;

                // 3. Read back: marked, whole, exact (0.1 as a FLOAT, not
                // widened to 0.10000000149011612).
                let out = run(
                    &mut s,
                    "SELECT id, emb, emb64, [1, 2] AS l FROM t ORDER BY id",
                )
                .await;
                let r = &out.results[0];
                assert_eq!(r.columns[1].type_name, "FLOAT[3]");
                let v = r.columns[1].vector.as_ref().expect("emb is a vector");
                assert_eq!((v.dims, v.element), (Some(3), VectorElement::F32));
                assert_eq!(r.columns[2].type_name, "DOUBLE[2]");
                assert_eq!(
                    r.columns[2].vector.as_ref().unwrap().element,
                    VectorElement::F64
                );
                assert!(r.columns[3].vector.is_none(), "a list isn't a vector");
                assert_eq!(
                    r.rows,
                    vec![
                        vec![
                            json!(1),
                            json!("[1,0,0]"),
                            json!("[0.1,-2]"),
                            json!("[1,2]")
                        ],
                        vec![json!(2), json!("[0,1,0.25]"), json!(null), json!("[1,2]")],
                        vec![
                            json!(3),
                            json!("[0.9,0.1,0]"),
                            json!("[0.0000001,4]"),
                            json!("[1,2]")
                        ],
                    ]
                );

                // 4. Describe keeps the type and its size.
                let cs = s.columns(&table("t")).await.unwrap();
                assert_eq!(cs[1].data_type, "FLOAT[3]");
                let all = s.database_schema().await.unwrap();
                let t = all.iter().find(|t| t.name == "t").unwrap().clone();
                assert_eq!(t.columns[2].data_type, "DOUBLE[2]");

                // 5. The HNSW index, with its settings (no longer "ART").
                let ix = &t.indexes[0];
                assert_eq!(
                    (ix.kind.as_deref(), ix.columns.as_slice()),
                    (Some("HNSW"), &["emb".to_string()][..])
                );
                assert_eq!(ix.vector, schema("t").indexes[0].vector);
                // The Índices view: HNSW, with the same settings.
                let usage = s.index_usage(&table("t")).await.unwrap().unwrap();
                let u = usage.indexes.iter().find(|u| u.name == ix.name).unwrap();
                assert_eq!(
                    (u.kind.as_str(), &u.vector),
                    ("HNSW", &schema("t").indexes[0].vector)
                );
                let again = d
                    .table_ddl(
                        &t,
                        DdlParts {
                            indexes: true,
                            ..Default::default()
                        },
                    )
                    .unwrap();
                assert!(
                    again.contains("USING HNSW (\"emb\") WITH (metric = 'cosine', M = 16)"),
                    "{again}"
                );

                // 6. Similarity, through the index (DuckDB plans it as an
                // HNSW scan) and by distance.
                let q = "SELECT id FROM t ORDER BY array_cosine_distance(emb, [1, 0, 0]::FLOAT[3]) LIMIT 2";
                let out = run(&mut s, q).await;
                assert_eq!(out.results[0].rows, vec![vec![json!(1)], vec![json!(3)]]);
                let out = run(&mut s, &format!("EXPLAIN {q}")).await;
                assert!(
                    out.results[0].rows[0][1]
                        .as_str()
                        .unwrap()
                        .to_ascii_lowercase()
                        .contains("hnsw index scan"),
                    "{:?}",
                    out.results[0].rows
                );
                let out = run(
                    &mut s,
                    "SELECT array_distance(emb64, [0.1, -2]::DOUBLE[2]) FROM t WHERE id = 1",
                )
                .await;
                assert_eq!(out.results[0].rows[0][0], json!(0.0));

                // 7. An edit.
                let up = d
                    .update_script(
                        &table("t"),
                        &[RowChange {
                            key: vec![("id".into(), json!(2))],
                            set: vec![("emb".into(), json!([0.5, 0.5, 0]))],
                            row: Vec::new(),
                        }],
                    )
                    .unwrap();
                run(&mut s, &up).await;
                let out = run(&mut s, "SELECT emb FROM t WHERE id = 2").await;
                assert_eq!(out.results[0].rows[0][0], json!("[0.5,0.5,0]"));

                // A sync that changes a column rebuilds the HNSW index, never
                // turns it into an ART.
                let mut new = t.clone();
                new.columns[2].nullable = false;
                run(&mut s, "UPDATE t SET emb64 = [0, 0] WHERE emb64 IS NULL").await;
                let sync = d
                    .sync_script(&[TableChange::Alter {
                        old: t.clone(),
                        new,
                    }])
                    .unwrap();
                for st in &sync.statements {
                    run(&mut s, st).await;
                }
                let all = s.database_schema().await.unwrap();
                let t2 = all.iter().find(|t| t.name == "t").unwrap();
                assert_eq!(t2.indexes.len(), 1, "{:?}", t2.indexes);
                assert_eq!(t2.indexes[0].vector, schema("t").indexes[0].vector);
                assert!(!t2.columns[2].nullable);

                // 8. Transfers: vector text out, the column's type in.
                let (tcols, rows) = read_all(&mut s, "t").await;
                assert_eq!(tcols[1].type_name, "FLOAT[3]");
                assert_eq!(rows[0][1], vec_cell("[1,0,0]"));
                assert_eq!(rows[0][2], vec_cell("[0.1,-2]"));
                run(&mut s, &d.table_ddl(&schema("c"), all_parts()).unwrap()).await;
                load(&mut s, "c", &["id", "emb", "emb64"], rows.clone()).await;
                let (_, copied) = read_all(&mut s, "c").await;
                assert_eq!(copied, rows);
                let out = run(&mut s, "SELECT emb, emb64 FROM c WHERE id = 3").await;
                assert_eq!(
                    out.results[0].rows[0],
                    vec![json!("[0.9,0.1,0]"), json!("[0.0000001,4]")]
                );

                // pgvector → DuckDB → pgvector.
                if pg {
                    psql(
                        "CREATE TABLE v (id int PRIMARY KEY, emb vector(3)); \
                         INSERT INTO v VALUES (1, '[0.25,-1,3]'), (2, '[1e-7,0,1]')",
                    )
                    .expect("pgvector table");
                    let pg_rows: Vec<Vec<Cell>> = psql("SELECT id, emb::text FROM v ORDER BY id")
                        .unwrap()
                        .lines()
                        .map(|l| {
                            let (id, v) = l.split_once('|').unwrap();
                            vec![Cell::Int(id.parse().unwrap()), text(v)]
                        })
                        .collect();
                    run(
                        &mut s,
                        "CREATE TABLE p (id INTEGER PRIMARY KEY, emb FLOAT[3])",
                    )
                    .await;
                    load(&mut s, "p", &["id", "emb"], pg_rows).await;
                    let out = run(&mut s, "SELECT emb FROM p ORDER BY id").await;
                    assert_eq!(
                        out.results[0].rows,
                        vec![vec![json!("[0.25,-1,3]")], vec![json!("[0.0000001,0,1]")]]
                    );
                    let (_, mine) = read_all(&mut s, "p").await;
                    for r in &mine {
                        let (Cell::Int(id), Cell::Json(v)) = (&r[0], &r[1]) else {
                            panic!("{r:?}");
                        };
                        psql(&format!("UPDATE v SET emb = '{v}' WHERE id = {id}"))
                            .expect("pgvector update");
                    }
                    assert_eq!(
                        psql("SELECT emb::text FROM v ORDER BY id").unwrap(),
                        "[0.25,-1,3]\n[1e-07,0,1]"
                    );
                } else {
                    eprintln!("dbine-test-pgvector not reachable: the pgvector move was not run");
                }
            }
            // The file again, in a new instance: vss loads by itself, so
            // the table with the HNSW index still takes writes.
            {
                let mut s = open(&path).await;
                run(&mut s, "INSERT INTO t VALUES (4, [0, 0, 1], [1, 1])").await;
                let out = run(
                    &mut s,
                    "SELECT id FROM t ORDER BY array_cosine_distance(emb, [0, 0, 1]::FLOAT[3]) LIMIT 1",
                )
                .await;
                assert_eq!(out.results[0].rows[0][0], json!(4));
            }
            // Read-only, it loads nothing, and reads work.
            {
                let cfg = ConnectionConfig {
                    driver: "duckdb".into(),
                    host: path.clone(),
                    read_only: true,
                    ..Default::default()
                };
                let mut s = driver().connect(&cfg, None).await.unwrap();
                let out = run(
                    &mut s,
                    "SELECT count(*) FROM duckdb_extensions() WHERE extension_name = 'vss' AND loaded",
                )
                .await;
                assert_eq!(out.results[0].rows[0][0], json!(0));
                let out = run(&mut s, "SELECT count(*) FROM t").await;
                assert_eq!(out.results[0].rows[0][0], json!(4));
            }
            // Without vss on the machine (it wasn't before this test), the
            // file opens and reads, nothing is downloaded, and a write says
            // how to get vss.
            if vss_before.is_none() {
                if let Some(p) = vss_installed().await {
                    let _ = std::fs::remove_file(&p);
                    let _ = std::fs::remove_file(format!("{p}.info"));
                }
                let mut s = open(&path).await;
                let out = run(&mut s, "SELECT count(*) FROM t").await;
                assert_eq!(out.results[0].rows[0][0], json!(4));
                let mut out = QueryOutcome::default();
                let e = s
                    .execute("INSERT INTO t VALUES (5, [1, 1, 1], [1, 1])", 10, &mut out)
                    .await
                    .expect_err("vss isn't there")
                    .to_string();
                assert!(e.contains("INSTALL vss; LOAD vss;"), "{e}");
                assert!(vss_installed().await.is_none(), "nothing was downloaded");
            }
        })
    };
    let r = body.await;
    remove_db(&path);
    psql_in("postgres", &format!("DROP DATABASE IF EXISTS {pg_db}"));
    if vss_before.is_none() {
        if let Some(p) = vss_installed().await {
            let _ = std::fs::remove_file(&p);
            let _ = std::fs::remove_file(format!("{p}.info"));
        }
    }
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
}
