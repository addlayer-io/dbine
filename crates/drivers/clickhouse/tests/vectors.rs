//! Vectors against a real ClickHouse: `Array(Float32)` / `Array(BFloat16)`
//! columns and the `vector_similarity` index, end to end:
//!
//! ```sh
//! docker start dbine-test-clickhouse   # -p 25123:8123, user dbine/dbine
//! cargo test -p dbine-driver-clickhouse --test vectors -- --ignored --nocapture
//! ```
//!
//! `DBINE_TEST_CLICKHOUSE_URL` overrides the server (default
//! `http://dbine:dbine@localhost:25123`). When `docker` reaches
//! `dbine-test-pgvector`, rows also move to and from pgvector through
//! `psql`, in a database of their own (the vector text is the contract).
//! The test's database is dropped first and at the end, even when an
//! assertion fails (the body runs in a task of its own).

use dbine_driver::transfer::{
    BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    kinds, ColumnDef, ConnectionConfig, DdlParts, Driver, IndexDef, KeyDef, ObjectRef,
    QueryOutcome, RowChange, Session, TableChange, TableSchema, VectorElement, VectorIndex,
};
use serde_json::{json, Value};
use std::process::Command;
use std::sync::{Arc, Mutex};

fn cfg() -> ConnectionConfig {
    let url = std::env::var("DBINE_TEST_CLICKHOUSE_URL")
        .unwrap_or_else(|_| "http://dbine:dbine@localhost:25123".into());
    let url = reqwest::Url::parse(&url).expect("URL");
    ConnectionConfig {
        driver: "clickhouse".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    }
}

fn driver() -> Arc<dyn Driver> {
    dbine_driver_clickhouse::drivers()
        .into_iter()
        .find(|d| d.info().id == "clickhouse")
        .unwrap()
}

async fn session(db: Option<&str>) -> Box<dyn Session> {
    driver().connect(&cfg(), db).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}\n{e}"));
    out
}

fn table(db: &str, name: &str) -> ObjectRef {
    ObjectRef {
        kind: kinds::TABLE.into(),
        schema: Some(db.into()),
        name: name.into(),
    }
}

fn col(name: &str, ty: &str) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        ..Default::default()
    }
}

/// The designer's table: a Float32 vector with a cosine index, a BFloat16
/// one, and a Float64 one with an L2 index (a vector because of its index).
fn schema(db: &str, name: &str) -> TableSchema {
    let vix = |n: &str, c: &str, metric: &str, params: &[(&str, &str)]| IndexDef {
        name: n.into(),
        columns: vec![c.into()],
        vector: Some(VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some(metric.into()),
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }),
        ..Default::default()
    };
    TableSchema {
        kind: kinds::TABLE.into(),
        schema: Some(db.into()),
        name: name.into(),
        columns: vec![
            col("id", "UInt64"),
            col("emb", "Array(Float32)"),
            col("half", "Array(BFloat16)"),
            col("wide", "Array(Float64)"),
        ],
        primary_key: Some(KeyDef {
            name: None,
            columns: vec!["id".into()],
        }),
        indexes: vec![
            vix("emb_ix", "emb", "cosine", &[("dimensions", "3")]),
            vix(
                "wide_ix",
                "wide",
                "l2",
                &[("dimensions", "2"), ("quantization", "f32"), ("m", "16")],
            ),
        ],
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

async fn read_all(s: &mut Box<dyn Session>, t: ObjectRef) -> (Vec<TransferColumn>, Vec<Vec<Cell>>) {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec {
        table: t,
        columns: None,
        filter: None,
    };
    s.read_batches(&spec, sink.clone()).await.unwrap();
    let mut c = std::mem::take(&mut *sink.lock().unwrap());
    c.rows.sort_by_key(|r| match r.first() {
        Some(Cell::Int(i)) => *i,
        Some(Cell::UInt(u)) => *u as i64,
        _ => 0,
    });
    (c.cols, c.rows)
}

struct Rows(Option<Vec<Vec<Cell>>>);

#[dbine_driver::async_trait]
impl BatchSource for Rows {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.take().map(|rows| RowBatch { rows, bytes: 0 })
    }
}

async fn load(s: &mut Box<dyn Session>, t: ObjectRef, cols: &[&str], rows: Vec<Vec<Cell>>) -> u64 {
    let spec = LoadSpec {
        table: t,
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

fn ids(out: &QueryOutcome) -> Vec<Value> {
    out.results[0].rows.iter().map(|r| r[0].clone()).collect()
}

#[tokio::test]
#[ignore]
async fn vectors_end_to_end() {
    let pid = std::process::id();
    let db = format!("dbine_vec_{pid}");
    let pg_db = format!("dbine_vec_clickhouse_{pid}");
    let drop_db = |db: String| async move {
        let mut s = session(None).await;
        run(&mut s, &format!("DROP DATABASE IF EXISTS `{db}` SYNC")).await;
    };
    drop_db(db.clone()).await;
    let pg = psql_in("postgres", &format!("DROP DATABASE IF EXISTS {pg_db}")).is_some()
        && psql_in("postgres", &format!("CREATE DATABASE {pg_db}")).is_some()
        && psql_in(&pg_db, "CREATE EXTENSION vector").is_some();

    let body = {
        let (db, pg_db) = (db.clone(), pg_db.clone());
        tokio::spawn(async move {
            let psql = |sql: &str| psql_in(&pg_db, sql);
            let d = driver();
            let mut s = session(None).await;
            run(&mut s, &format!("CREATE DATABASE `{db}`")).await;
            // 1. The designer's DDL: the indexes inline, with ClickHouse's
            // own granularity for vector indexes.
            let ddl = d
                .table_ddl(
                    &schema(&db, "t"),
                    DdlParts {
                        create: true,
                        indexes: true,
                        ..Default::default()
                    },
                )
                .unwrap();
            assert!(
                ddl.contains("INDEX `emb_ix` emb TYPE vector_similarity('hnsw', 'cosineDistance', 3) GRANULARITY 100000000,")
                    && ddl.contains("INDEX `wide_ix` wide TYPE vector_similarity('hnsw', 'L2Distance', 2, 'f32', 16, 0)"),
                "{ddl}"
            );
            run(&mut s, &ddl).await;

            // 2. Rows through insert_script: vectors as array literals.
            let cols: Vec<String> = ["id", "emb", "half", "wide"]
                .iter()
                .map(|c| c.to_string())
                .collect();
            let ins = d
                .insert_script(
                    &table(&db, "t"),
                    &cols,
                    &[
                        vec![
                            json!(1),
                            json!([1, 0, 0]),
                            json!([1.5, 2]),
                            json!([0.1, -2]),
                        ],
                        vec![
                            json!(2),
                            json!([0, 1, 0.25]),
                            json!([0, 1]),
                            json!([1e-7, 4]),
                        ],
                        vec![
                            json!(3),
                            json!([0.9, 0.1, 0]),
                            json!([-1, 0]),
                            json!([3, 3]),
                        ],
                    ],
                )
                .unwrap();
            assert!(ins.contains("(1, [1,0,0], [1.5,2], [0.1,-2])"), "{ins}");
            run(&mut s, &ins).await;

            // 3. Read back: Float32 and BFloat16 arrays are vectors, exact.
            let out = run(
                &mut s,
                &format!("SELECT id, emb, half, wide FROM `{db}`.t ORDER BY id"),
            )
            .await;
            let r = &out.results[0];
            let v = r.columns[1].vector.as_ref().expect("emb is a vector");
            assert_eq!((v.dims, v.element), (None, VectorElement::F32));
            assert_eq!(r.columns[1].type_name, "Array(Float32)");
            assert_eq!(
                r.columns[2].vector.as_ref().unwrap().element,
                VectorElement::Bf16
            );
            assert!(
                r.columns[3].vector.is_none(),
                "Float64 arrays aren't vectors in results"
            );
            assert_eq!(
                r.rows,
                vec![
                    vec![
                        json!(1),
                        json!("[1,0,0]"),
                        json!("[1.5,2]"),
                        json!("[0.1,-2]")
                    ],
                    vec![
                        json!(2),
                        json!("[0,1,0.25]"),
                        json!("[0,1]"),
                        json!("[1e-7,4]")
                    ],
                    vec![
                        json!(3),
                        json!("[0.9,0.1,0]"),
                        json!("[-1,0]"),
                        json!("[3,3]")
                    ],
                ]
            );

            // 4. Describe keeps the types.
            let mut sd = session(Some(&db)).await;
            let all = sd.database_schema().await.unwrap();
            let t = all.iter().find(|t| t.name == "t").unwrap().clone();
            assert_eq!(t.columns[1].data_type, "Array(Float32)");
            assert_eq!(t.columns[2].data_type, "Array(BFloat16)");

            // 5. The vector indexes with their settings.
            let ix = |n: &str| t.indexes.iter().find(|i| i.name == n).unwrap().clone();
            let e = ix("emb_ix");
            assert_eq!(
                (e.kind.as_deref(), e.columns.as_slice()),
                (Some("vector_similarity"), &["emb".to_string()][..])
            );
            assert_eq!(e.vector, schema(&db, "t").indexes[0].vector);
            assert_eq!(ix("wide_ix").vector, schema(&db, "t").indexes[1].vector);
            // The Índices view: the same settings, next to the skip
            // index's size.
            let usage = sd.index_usage(&table(&db, "t")).await.unwrap().unwrap();
            let u = usage.indexes.iter().find(|u| u.name == "emb_ix").unwrap();
            assert_eq!(
                (u.kind.as_str(), &u.vector),
                ("VECTOR_SIMILARITY", &schema(&db, "t").indexes[0].vector)
            );
            // The whole type in the options too, for the schema compare.
            assert_eq!(
                e.options.get("type").map(String::as_str),
                Some("vector_similarity('hnsw', 'cosineDistance', 3)")
            );
            // The DDL from the catalog makes the same indexes (clone, compare).
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
                again.contains(
                    "ADD INDEX `emb_ix` emb TYPE vector_similarity('hnsw', 'cosineDistance', 3) GRANULARITY 100000000;"
                ),
                "{again}"
            );

            // 6. Similarity: the index serves an ORDER BY distance LIMIT.
            let q =
                format!("SELECT id FROM `{db}`.t ORDER BY cosineDistance(emb, [1, 0, 0]) LIMIT 2");
            assert_eq!(ids(&run(&mut s, &q).await), [json!(1), json!(3)]);
            let plan = run(&mut s, &format!("EXPLAIN indexes = 1 {q}")).await;
            let plan: Vec<String> = plan.results[0]
                .rows
                .iter()
                .map(|r| r[0].as_str().unwrap_or("").to_string())
                .collect();
            assert!(plan.iter().any(|l| l.contains("emb_ix")), "{plan:#?}");
            let out = run(
                &mut s,
                &format!("SELECT id FROM `{db}`.t ORDER BY L2Distance(wide, [3., 3.]) LIMIT 1"),
            )
            .await;
            assert_eq!(ids(&out), [json!(3)]);

            // The inner product (dotProduct, ClickHouse 26.4 and later) is
            // a distance the index takes.
            let ip = TableSchema {
                indexes: vec![IndexDef {
                    name: "ip_ix".into(),
                    columns: vec!["emb".into()],
                    vector: Some(VectorIndex {
                        algorithm: Some("hnsw".into()),
                        metric: Some("ip".into()),
                        params: [("dimensions".to_string(), "3".to_string())].into(),
                    }),
                    ..Default::default()
                }],
                ..t.clone()
            };
            let add = d
                .table_ddl(
                    &ip,
                    DdlParts {
                        indexes: true,
                        ..Default::default()
                    },
                )
                .unwrap();
            assert!(
                add.contains("TYPE vector_similarity('hnsw', 'dotProduct', 3)"),
                "{add}"
            );
            run(&mut s, &add).await;
            let all = sd.database_schema().await.unwrap();
            let t_ip = all.iter().find(|t| t.name == "t").unwrap();
            let i = t_ip.indexes.iter().find(|i| i.name == "ip_ix").unwrap();
            assert_eq!(i.vector.as_ref().unwrap().metric.as_deref(), Some("ip"));
            run(&mut s, &format!("ALTER TABLE `{db}`.t DROP INDEX ip_ix")).await;

            // 7. An edit (a mutation; waited for).
            let up = d
                .update_script(
                    &table(&db, "t"),
                    &[RowChange {
                        key: vec![("id".into(), json!(2))],
                        set: vec![("emb".into(), json!([0.5, 0.5, 0]))],
                        row: Vec::new(),
                    }],
                )
                .unwrap();
            run(
                &mut s,
                &format!("{} SETTINGS mutations_sync = 1", up.trim_end_matches(';')),
            )
            .await;
            let out = run(&mut s, &format!("SELECT emb FROM `{db}`.t WHERE id = 2")).await;
            assert_eq!(out.results[0].rows[0][0], json!("[0.5,0.5,0]"));

            // A sync that changes a vector index's metric drops and adds it.
            let mut new = t.clone();
            for i in &mut new.indexes {
                if i.name == "emb_ix" {
                    i.vector.as_mut().unwrap().metric = Some("l2".into());
                }
            }
            let sync = d
                .sync_script(&[TableChange::Alter {
                    old: t.clone(),
                    new,
                }])
                .unwrap();
            assert!(
                sync.statements.iter().any(|st| st.contains(
                    "ADD INDEX `emb_ix` emb TYPE vector_similarity('hnsw', 'L2Distance', 3)"
                )),
                "{:?}",
                sync.statements
            );
            for st in &sync.statements {
                run(&mut s, st).await;
            }
            let all = sd.database_schema().await.unwrap();
            let t2 = all.iter().find(|t| t.name == "t").unwrap();
            let e2 = t2.indexes.iter().find(|i| i.name == "emb_ix").unwrap();
            assert_eq!(e2.vector.as_ref().unwrap().metric.as_deref(), Some("l2"));
            // Nothing to do when nothing changed.
            let none = d
                .sync_script(&[TableChange::Alter {
                    old: t2.clone(),
                    new: t2.clone(),
                }])
                .unwrap();
            assert!(none.statements.is_empty(), "{:?}", none.statements);

            // 8. Transfers: the vector text out (Float32 the shortest way),
            // and in.
            let (tcols, rows) = read_all(&mut s, table(&db, "t")).await;
            assert_eq!(tcols[1].type_name, "Array(Float32)");
            assert_eq!(rows[0][1], Cell::Json("[1,0,0]".into()));
            assert_eq!(rows[2][1], Cell::Json("[0.9,0.1,0]".into()));
            assert_eq!(rows[0][2], Cell::Json("[1.5,2]".into()));
            run(
                &mut s,
                &d.table_ddl(
                    &schema(&db, "c"),
                    DdlParts {
                        create: true,
                        indexes: true,
                        ..Default::default()
                    },
                )
                .unwrap(),
            )
            .await;
            load(
                &mut s,
                table(&db, "c"),
                &["id", "emb", "half", "wide"],
                rows.clone(),
            )
            .await;
            let (_, copied) = read_all(&mut s, table(&db, "c")).await;
            assert_eq!(copied, rows);
            // Vector text from elsewhere (as a Text cell) loads too.
            load(
                &mut s,
                table(&db, "c"),
                &["id", "emb", "half", "wide"],
                vec![vec![
                    Cell::UInt(9),
                    Cell::Text("[0.25,-1,3]".into()),
                    Cell::Text("[1,1]".into()),
                    Cell::Text("[1e-07,0]".into()),
                ]],
            )
            .await;
            let out = run(
                &mut s,
                &format!("SELECT emb, wide FROM `{db}`.c WHERE id = 9"),
            )
            .await;
            assert_eq!(
                out.results[0].rows[0],
                vec![json!("[0.25,-1,3]"), json!("[1e-7,0]")]
            );

            // pgvector → ClickHouse → pgvector.
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
                        vec![Cell::UInt(id.parse().unwrap()), Cell::Text(v.into())]
                    })
                    .collect();
                run(
                    &mut s,
                    &format!(
                        "CREATE TABLE `{db}`.p (id UInt64, emb Array(Float32), INDEX pe emb TYPE vector_similarity('hnsw', 'L2Distance', 3)) ENGINE = MergeTree ORDER BY id"
                    ),
                )
                .await;
                load(&mut s, table(&db, "p"), &["id", "emb"], pg_rows).await;
                let out = run(&mut s, &format!("SELECT emb FROM `{db}`.p ORDER BY id")).await;
                assert_eq!(
                    out.results[0].rows,
                    vec![vec![json!("[0.25,-1,3]")], vec![json!("[0.0000001,0,1]")]]
                );
                let (_, mine) = read_all(&mut s, table(&db, "p")).await;
                for r in &mine {
                    let (Cell::UInt(id), Cell::Json(v)) = (&r[0], &r[1]) else {
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
        })
    };
    let r = body.await;
    drop_db(db.clone()).await;
    psql_in("postgres", &format!("DROP DATABASE IF EXISTS {pg_db}"));
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
}

/// Timeplus Proton: `array(float32)` is its embedding type (it has the
/// distances, no vector index). A stream described and a vector read;
/// `DBINE_TEST_TIMEPLUS_URL` (default `http://localhost:25119`).
#[tokio::test]
#[ignore]
async fn timeplus_vectors() {
    let url = std::env::var("DBINE_TEST_TIMEPLUS_URL")
        .unwrap_or_else(|_| "http://localhost:25119".into());
    let url = reqwest::Url::parse(&url).expect("URL");
    let c = ConnectionConfig {
        driver: "timeplus".into(),
        host: url.host_str().unwrap().into(),
        port: url.port().unwrap_or(0),
        ..Default::default()
    };
    let d = dbine_driver_clickhouse::drivers()
        .into_iter()
        .find(|d| d.info().id == "timeplus")
        .unwrap();
    let name = format!("dbine_vec_tp_{}", std::process::id());
    let mut s = d.connect(&c, None).await.expect("connect");
    run(&mut s, &format!("DROP STREAM IF EXISTS {name}")).await;
    let r = {
        let (d, c, name) = (d.clone(), c.clone(), name.clone());
        tokio::spawn(async move {
            let mut s = d.connect(&c, None).await.expect("connect");
            assert!(d.capabilities().vectors);
            assert!(d.designer().unwrap().data_types.contains(&"array(float32)"));
            run(
                &mut s,
                &format!("CREATE STREAM {name} (id uint64, v array(float32))"),
            )
            .await;
            let cols = s
                .columns(&ObjectRef {
                    kind: kinds::STREAM.into(),
                    schema: None,
                    name: name.clone(),
                })
                .await
                .unwrap();
            assert!(
                cols.iter()
                    .any(|c| c.name == "v" && c.data_type == "array(float32)"),
                "{cols:?}"
            );
            let out = run(
                &mut s,
                "SELECT cast([0.1, 2, 1e-7], 'array(float32)') AS v, cosine_distance(v, [0.1, 2, 0]) AS d",
            )
            .await;
            let r = &out.results[0];
            assert_eq!(
                r.columns[0].vector.as_ref().unwrap().element,
                VectorElement::F32
            );
            assert_eq!(r.rows[0][0], json!("[0.1,2,0.0000001]"));
            assert!(r.columns[1].vector.is_none());
        })
    };
    let r = r.await;
    run(&mut s, &format!("DROP STREAM IF EXISTS {name}")).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
}
