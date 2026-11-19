//! Vectors against a real sqld (`dbine-test-libsql`, port 25880):
//! `cargo test -p dbine-driver-libsql --test vectors -- --ignored`
//! (`DBINE_LIBSQL_URL` points elsewhere). It also moves rows to and from a
//! local SQLite file and, when `docker` reaches `dbine-test-pgvector`, to
//! and from pgvector (through `psql`: the vector text is the contract).
//!
//! Every table it makes is dropped first and at the end, even when an
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

/// One test at a time: sqld 0.24.33 crashed (a segfault) with the two
/// running at once, each making and dropping vector-indexed tables, and the
/// crash left half-made indexes behind.
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn url() -> String {
    std::env::var("DBINE_LIBSQL_URL").unwrap_or_else(|_| "http://localhost:25880".into())
}

fn driver() -> Arc<dyn Driver> {
    dbine_driver_libsql::drivers().pop().unwrap()
}

async fn open() -> Box<dyn Session> {
    let cfg = ConnectionConfig {
        driver: "libsql".into(),
        host: url(),
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
        schema: None,
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

/// The designer's table: an F32 vector with a cosine index, an F64 and an
/// F16 vector without one.
fn schema(name: &str) -> TableSchema {
    TableSchema {
        kind: kinds::TABLE.into(),
        name: name.into(),
        columns: vec![
            col("id", "INTEGER", false),
            col("emb", "F32_BLOB(3)", true),
            col("emb64", "F64_BLOB(2)", true),
            col("half", "F16_BLOB(2)", true),
        ],
        primary_key: Some(KeyDef {
            name: None,
            columns: vec!["id".into()],
        }),
        indexes: vec![IndexDef {
            name: format!("{name}_emb"),
            columns: vec!["emb".into()],
            vector: Some(VectorIndex {
                algorithm: None,
                metric: Some("cosine".into()),
                params: [("m".to_string(), "16".to_string())].into(),
            }),
            ..Default::default()
        }],
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

/// psql on the pgvector container, in database `db`; `None` when docker
/// or it isn't there.
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

async fn drop_all(names: &[String], meta_was_there: bool) {
    let mut s = open().await;
    for n in names {
        run(&mut s, &format!("DROP TABLE IF EXISTS \"{n}\"")).await;
    }
    // The vector index metadata table appears with the first vector index:
    // gone with ours when it wasn't there before.
    if !meta_was_there {
        let r = run(
            &mut s,
            "SELECT count(*) FROM sqlite_master WHERE name = 'libsql_vector_meta_shadow'",
        )
        .await;
        if r.results[0].rows[0][0] == json!(1) {
            let n = run(&mut s, "SELECT count(*) FROM libsql_vector_meta_shadow").await;
            if n.results[0].rows[0][0] == json!(0) {
                run(&mut s, "DROP TABLE libsql_vector_meta_shadow").await;
            }
        }
    }
}

#[tokio::test]
#[ignore]
async fn vectors_end_to_end() {
    let _one = LOCK.lock().await;
    let pid = std::process::id();
    let name = format!("vec_t_{pid}");
    let copy = format!("vec_c_{pid}");
    let back = format!("vec_b_{pid}");
    let from_pg = format!("vec_p_{pid}");
    let other = format!("vec_l_{pid}");
    let names = vec![
        name.clone(),
        copy.clone(),
        back.clone(),
        from_pg.clone(),
        other.clone(),
    ];
    let pg_table = format!("dbine_vec_libsql_{pid}");
    let file = std::env::temp_dir().join(format!("dbine-libsql-vec-{pid}.db"));
    let meta_was_there = {
        let mut s = open().await;
        let r = run(
            &mut s,
            "SELECT count(*) FROM sqlite_master WHERE name = 'libsql_vector_meta_shadow'",
        )
        .await;
        r.results[0].rows[0][0] == json!(1)
    };
    drop_all(&names, true).await;
    let _ = std::fs::remove_file(&file);
    // A database of its own, with the extension, so nothing else is touched.
    let pg = psql_in("postgres", &format!("DROP DATABASE IF EXISTS {pg_table}")).is_some()
        && psql_in("postgres", &format!("CREATE DATABASE {pg_table}")).is_some()
        && psql_in(&pg_table, "CREATE EXTENSION vector").is_some();

    let body = {
        let (name, copy, back, from_pg, pg_table, file, other) = (
            name.clone(),
            copy.clone(),
            back.clone(),
            from_pg.clone(),
            pg_table.clone(),
            file.clone(),
            other.clone(),
        );
        tokio::spawn(async move {
            let psql = |sql: &str| psql_in(&pg_table, sql);
            let d = driver();
            let mut s = open().await;
            // 1. The designer's DDL: types and the vector index.
            let ddl = d
                .table_ddl(
                    &schema(&name),
                    DdlParts {
                        create: true,
                        indexes: true,
                        ..Default::default()
                    },
                )
                .unwrap();
            assert!(
                ddl.contains(&format!(
                    "CREATE INDEX \"{name}_emb\" ON \"{name}\" (libsql_vector_idx(\"emb\", 'metric=cosine', 'max_neighbors=16'));"
                )),
                "{ddl}"
            );
            run(&mut s, &ddl).await;

            // 2. Rows through insert_script, vectors as JSON arrays.
            let cols: Vec<String> = ["id", "emb", "emb64", "half"]
                .iter()
                .map(|c| c.to_string())
                .collect();
            let ins = d
                .insert_script(
                    &table(&name),
                    &cols,
                    &[
                        vec![
                            json!(1),
                            json!([1, 0, 0]),
                            json!([0.5, -2]),
                            json!([1.5, 2]),
                        ],
                        vec![json!(2), json!([0, 1, 0.25]), json!(null), json!(null)],
                        vec![
                            json!(3),
                            json!([0.9, 0.1, 0]),
                            json!([1e-3, 4]),
                            json!([-1, 0]),
                        ],
                    ],
                )
                .unwrap();
            assert!(ins.contains("FROM (SELECT '[1,0,0]' AS v)"), "{ins}");
            // An F64 column with a vector index of its own takes only F64
            // vectors: the script writes each column's format.
            run(
                &mut s,
                &format!("CREATE INDEX \"{name}_emb64\" ON \"{name}\" (libsql_vector_idx(emb64))"),
            )
            .await;
            run(&mut s, &ins).await;
            let out = run(
                &mut s,
                &format!(
                    "SELECT typeof(emb64), length(emb64), length(half), length(emb) FROM \"{name}\" WHERE id = 1"
                ),
            )
            .await;
            // 2 · 8 bytes and the F64 type byte, 2 · 2 and the F16 one, 3 · 4.
            assert_eq!(
                out.results[0].rows[0],
                vec![json!("blob"), json!(17), json!(5), json!(12)]
            );
            run(&mut s, &format!("DROP INDEX \"{name}_emb64\"")).await;

            // 3. Read back: marked, whole, exact.
            let out = run(
                &mut s,
                &format!("SELECT id, emb, emb64, half FROM \"{name}\" ORDER BY id"),
            )
            .await;
            let r = &out.results[0];
            let v = r.columns[1].vector.as_ref().expect("emb is a vector");
            assert_eq!((v.dims, v.element), (Some(3), VectorElement::F32));
            assert_eq!(r.columns[1].type_name, "F32_BLOB(3)");
            assert_eq!(
                r.columns[2].vector.as_ref().unwrap().element,
                VectorElement::F64
            );
            assert!(r.columns[0].vector.is_none());
            assert_eq!(
                r.rows,
                vec![
                    vec![
                        json!(1),
                        json!("[1,0,0]"),
                        json!("[0.5,-2]"),
                        json!("[1.5,2]")
                    ],
                    vec![json!(2), json!("[0,1,0.25]"), json!(null), json!(null)],
                    vec![
                        json!(3),
                        json!("[0.9,0.1,0]"),
                        json!("[0.001,4]"),
                        json!("[-1,0]")
                    ],
                ]
            );
            // The engine's own reading of what the script wrote.
            let out = run(
                &mut s,
                &format!("SELECT vector_extract(emb) FROM \"{name}\" WHERE id = 3"),
            )
            .await;
            assert_eq!(out.results[0].rows[0][0], json!("[0.9,0.1,0]"));

            // 4. Describe keeps the type and its dimension; libSQL's shadow
            // tables aren't the user's.
            let cs = s.columns(&table(&name)).await.unwrap();
            assert_eq!(cs[1].data_type, "F32_BLOB(3)");
            let objs = s.list_objects().await.unwrap();
            assert!(
                !objs
                    .iter()
                    .any(|o| o.name.contains("_shadow") || o.name.starts_with("libsql_")),
                "{objs:?}"
            );
            let all = s.database_schema().await.unwrap();
            assert!(
                !all.iter()
                    .any(|t| t.name.contains("_shadow") || t.name.starts_with("libsql_")),
                "{:?}",
                all.iter().map(|t| &t.name).collect::<Vec<_>>()
            );
            let t = all.iter().find(|t| t.name == name).unwrap().clone();
            assert_eq!(t.columns[1].data_type, "F32_BLOB(3)");

            // 5. The index, with its vector settings.
            let ix = &t.indexes[0];
            assert_eq!(ix.columns, ["emb"]);
            assert_eq!(ix.vector, schema(&name).indexes[0].vector);
            // Its key in the options too, for the schema compare: an index
            // that differs only in its metric isn't the same index.
            assert_eq!(
                ix.options.get("vector").map(String::as_str),
                Some("libsql_vector_idx(\"emb\", 'metric=cosine', 'max_neighbors=16')")
            );
            let mut l2 = schema(&other);
            l2.indexes[0].vector.as_mut().unwrap().metric = Some("l2".into());
            run(
                &mut s,
                &d.table_ddl(
                    &l2,
                    DdlParts {
                        create: true,
                        indexes: true,
                        ..Default::default()
                    },
                )
                .unwrap(),
            )
            .await;
            let all = s.database_schema().await.unwrap();
            let o = &all.iter().find(|t| t.name == other).unwrap().indexes[0];
            assert_eq!(
                (&o.columns, &o.kind, o.unique, &o.filter),
                (&ix.columns, &ix.kind, ix.unique, &ix.filter)
            );
            assert_ne!(o.options, ix.options);
            assert!(
                o.options["vector"].contains("'metric=l2'"),
                "{:?}",
                o.options
            );
            // The DDL from the catalog makes the same index (compare, clone).
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
                again.contains("libsql_vector_idx(\"emb\", 'metric=cosine', 'max_neighbors=16')"),
                "{again}"
            );

            // 6. A similarity search, through the index and by distance.
            let out = run(
                &mut s,
                &format!(
                    "SELECT t.id FROM vector_top_k('{name}_emb', vector32('[1,0,0]'), 2) k JOIN \"{name}\" t ON t.rowid = k.id"
                ),
            )
            .await;
            let mut ids: Vec<_> = out.results[0].rows.iter().map(|r| r[0].clone()).collect();
            ids.sort_by_key(|v| v.as_i64());
            assert_eq!(ids, [json!(1), json!(3)]);
            let out = run(
                &mut s,
                &format!(
                    "SELECT id FROM \"{name}\" ORDER BY vector_distance_cos(emb, vector32('[0,1,0]')) LIMIT 1"
                ),
            )
            .await;
            assert_eq!(out.results[0].rows[0][0], json!(2));

            // 7. An edit.
            let up = d
                .update_script(
                    &table(&name),
                    &[RowChange {
                        key: vec![("id".into(), json!(2))],
                        set: vec![("emb".into(), json!([0.5, 0.5, 0]))],
                        row: Vec::new(),
                    }],
                )
                .unwrap();
            run(&mut s, &up).await;
            let out = run(&mut s, &format!("SELECT emb FROM \"{name}\" WHERE id = 2")).await;
            assert_eq!(out.results[0].rows[0][0], json!("[0.5,0.5,0]"));
            // And one into an indexed F64 column (an F32 vector is refused
            // there: "vector type differs from column type").
            run(
                &mut s,
                &format!("CREATE INDEX \"{name}_emb64\" ON \"{name}\" (libsql_vector_idx(emb64))"),
            )
            .await;
            let up = d
                .update_script(
                    &table(&name),
                    &[RowChange {
                        key: vec![("id".into(), json!(2))],
                        set: vec![("emb64".into(), json!([0.25, 8]))],
                        row: Vec::new(),
                    }],
                )
                .unwrap();
            run(&mut s, &up).await;
            let out = run(
                &mut s,
                &format!(
                    "SELECT emb64, (SELECT count(*) FROM vector_top_k('{name}_emb64', vector64('[0.25,8]'), 1)) FROM \"{name}\" WHERE id = 2"
                ),
            )
            .await;
            assert_eq!(out.results[0].rows[0], vec![json!("[0.25,8]"), json!(1)]);
            run(&mut s, &format!("DROP INDEX \"{name}_emb64\"")).await;

            // A rebuild (a column becomes NOT NULL) keeps the vector index.
            let mut new = t.clone();
            new.columns[1].nullable = false;
            let sync = d
                .sync_script(&[TableChange::Alter {
                    old: t.clone(),
                    new: new.clone(),
                }])
                .unwrap();
            for st in &sync.statements {
                run(&mut s, st).await;
            }
            let all = s.database_schema().await.unwrap();
            let t2 = all.iter().find(|t| t.name == name).unwrap();
            assert_eq!(t2.indexes.len(), 1);
            assert_eq!(t2.indexes[0].vector, schema(&name).indexes[0].vector);
            assert!(!t2.columns[1].nullable);

            // 8. Transfers. libSQL → libSQL: the read emits vector text, the
            // load writes it as each column's format.
            let (tcols, rows) = read_all(&mut s, &name).await;
            assert_eq!(tcols[1].type_name, "F32_BLOB(3)");
            assert_eq!(rows[0][1], text("[1,0,0]"));
            assert_eq!(rows[0][2], text("[0.5,-2]"));
            run(
                &mut s,
                &d.table_ddl(
                    &schema(&copy),
                    DdlParts {
                        create: true,
                        indexes: true,
                        ..Default::default()
                    },
                )
                .unwrap(),
            )
            .await;
            load(&mut s, &copy, &["id", "emb", "emb64", "half"], rows.clone()).await;
            let out = run(
                &mut s,
                &format!(
                    "SELECT id, typeof(emb64), vector_extract(emb64), vector_extract(half) FROM \"{copy}\" WHERE id = 1"
                ),
            )
            .await;
            assert_eq!(
                out.results[0].rows[0],
                vec![json!(1), json!("blob"), json!("[0.5,-2]"), json!("[1.5,2]")]
            );
            // The F64 column got F64 blobs (8 bytes a number and the type byte).
            let out = run(
                &mut s,
                &format!("SELECT length(emb64) FROM \"{copy}\" WHERE id = 1"),
            )
            .await;
            assert_eq!(out.results[0].rows[0][0], json!(17));
            let (_, copied) = read_all(&mut s, &copy).await;
            assert_eq!(copied, rows);

            // libSQL → a SQLite file → libSQL.
            let sqlite = dbine_driver_sqlite::drivers().pop().unwrap();
            let fcfg = ConnectionConfig {
                driver: "sqlite".into(),
                host: file.to_string_lossy().into(),
                ..Default::default()
            };
            let mut f = sqlite.connect(&fcfg, None).await.unwrap();
            // SQLite's DDL leaves the vector index out (it has no
            // libsql_vector_idx), saying why, and runs.
            let file_ddl = sqlite
                .table_ddl(
                    &schema(&name),
                    DdlParts {
                        create: true,
                        indexes: true,
                        ..Default::default()
                    },
                )
                .unwrap();
            assert!(
                !file_ddl.contains("libsql_vector_idx(")
                    && file_ddl.contains("-- El índice vectorial"),
                "{file_ddl}"
            );
            run(&mut f, &file_ddl).await;
            load(&mut f, &name, &["id", "emb", "emb64", "half"], rows.clone()).await;
            let out = run(
                &mut f,
                &format!("SELECT emb, emb64 FROM \"{name}\" ORDER BY id"),
            )
            .await;
            assert!(out.results[0].columns[0].vector.is_some());
            assert_eq!(
                out.results[0].rows[0],
                vec![json!("[1,0,0]"), json!("[0.5,-2]")]
            );
            let (_, from_file) = read_all(&mut f, &name).await;
            assert_eq!(from_file, rows);
            drop(f);
            run(
                &mut s,
                &d.table_ddl(
                    &schema(&back),
                    DdlParts {
                        create: true,
                        indexes: true,
                        ..Default::default()
                    },
                )
                .unwrap(),
            )
            .await;
            load(&mut s, &back, &["id", "emb", "emb64", "half"], from_file).await;
            let (_, round) = read_all(&mut s, &back).await;
            assert_eq!(round, rows);

            // pgvector → libSQL → pgvector, with pgvector's own vector text.
            if pg {
                psql(&format!(
                    "CREATE TABLE {pg_table} (id int PRIMARY KEY, emb vector(3)); \
                     INSERT INTO {pg_table} VALUES (1, '[0.25,-1,3]'), (2, '[1e-7,0,1]')"
                ))
                .expect("pgvector table");
                let pg_rows: Vec<Vec<Cell>> =
                    psql(&format!("SELECT id, emb::text FROM {pg_table} ORDER BY id"))
                        .unwrap()
                        .lines()
                        .map(|l| {
                            let (id, v) = l.split_once('|').unwrap();
                            vec![Cell::Int(id.parse().unwrap()), text(v)]
                        })
                        .collect();
                run(
                    &mut s,
                    &format!(
                        "CREATE TABLE \"{from_pg}\" (id INTEGER PRIMARY KEY, emb F32_BLOB(3))"
                    ),
                )
                .await;
                load(&mut s, &from_pg, &["id", "emb"], pg_rows).await;
                let out = run(
                    &mut s,
                    &format!("SELECT emb FROM \"{from_pg}\" ORDER BY id"),
                )
                .await;
                assert_eq!(
                    out.results[0].rows,
                    vec![vec![json!("[0.25,-1,3]")], vec![json!("[0.0000001,0,1]")]]
                );
                let (_, mine) = read_all(&mut s, &from_pg).await;
                for r in &mine {
                    let (Cell::Int(id), Cell::Text(v)) = (&r[0], &r[1]) else {
                        panic!("{r:?}");
                    };
                    psql(&format!(
                        "UPDATE {pg_table} SET emb = '{v}' WHERE id = {id}"
                    ))
                    .expect("pgvector update");
                }
                let back = psql(&format!("SELECT emb::text FROM {pg_table} ORDER BY id")).unwrap();
                assert_eq!(back, "[0.25,-1,3]\n[1e-07,0,1]");
            } else {
                eprintln!("dbine-test-pgvector not reachable: the pgvector move was not run");
            }
        })
    };
    let r = body.await;
    drop_all(&names, meta_was_there).await;
    let _ = std::fs::remove_file(&file);
    psql_in("postgres", &format!("DROP DATABASE IF EXISTS {pg_table}"));
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
}

/// Typed scripts against sqld, in every vector format, each column with a
/// vector index of its own (which refuses a vector of another format): the
/// script writes each format's function with no catalog lookup, the rows
/// read back, an edit lands and is found through the index, and the
/// Índices view lists the index as DiskANN with its settings.
#[tokio::test]
#[ignore]
async fn typed_scripts_in_every_format() {
    let _one = LOCK.lock().await;
    let pid = std::process::id();
    let formats = [
        ("F32_BLOB", "vector32"),
        ("F64_BLOB", "vector64"),
        ("F16_BLOB", "vector16"),
        ("FB16_BLOB", "vectorb16"),
        ("F8_BLOB", "vector8"),
        ("F1BIT_BLOB", "vector1bit"),
    ];
    let names: Vec<String> = formats
        .iter()
        .map(|(f, _)| format!("vec_ty_{pid}_{}", f.to_ascii_lowercase()))
        .collect();
    let meta_was_there = {
        let mut s = open().await;
        let r = run(
            &mut s,
            "SELECT count(*) FROM sqlite_master WHERE name = 'libsql_vector_meta_shadow'",
        )
        .await;
        r.results[0].rows[0][0] == json!(1)
    };
    drop_all(&names, true).await;
    let body = {
        let names = names.clone();
        tokio::spawn(async move {
            let d = driver();
            let mut s = open().await;
            for ((format, function), name) in formats.iter().zip(&names) {
                let mut t = schema(name);
                t.columns = vec![
                    col("id", "INTEGER", false),
                    col("emb", &format!("{format}(4)"), true),
                ];
                // libSQL has no L2 distance for 1-bit vectors: the DDL
                // refuses such an index (libSQL would take it, and then
                // rank wrong); cosine for them.
                t.indexes[0].vector.as_mut().unwrap().metric = Some("l2".into());
                if *format == "F1BIT_BLOB" {
                    let e = d
                        .table_ddl(
                            &t,
                            DdlParts {
                                create: true,
                                indexes: true,
                                ..Default::default()
                            },
                        )
                        .unwrap_err();
                    assert!(e.to_string().contains("1 bit"), "{e}");
                    t.indexes[0].vector.as_mut().unwrap().metric = Some("cosine".into());
                }
                let ddl = d
                    .table_ddl(
                        &t,
                        DdlParts {
                            create: true,
                            indexes: true,
                            ..Default::default()
                        },
                    )
                    .unwrap();
                run(&mut s, &ddl).await;
                // The types as the app gets them from describe.
                let types: Vec<dbine_driver::ResultColumn> = s
                    .columns(&table(name))
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|c| dbine_driver::ResultColumn::new(c.name, c.data_type))
                    .collect();
                assert_eq!(types[1].type_name, format!("{format}(4)"));
                let ins = d
                    .insert_script_typed(
                        &table(name),
                        &types,
                        &[
                            vec![json!(1), json!([1, 2, 3, 4])],
                            vec![json!(2), json!([-1, 0.5, -2, 3])],
                        ],
                    )
                    .unwrap();
                assert!(
                    ins.contains(&format!("{function}('[1,2,3,4]')"))
                        && !ins.contains("pragma_table_info"),
                    "{ins}"
                );
                run(&mut s, &ins).await;
                let out = run(
                    &mut s,
                    &format!("SELECT id, emb FROM \"{name}\" ORDER BY id"),
                )
                .await;
                let r = &out.results[0];
                assert!(r.columns[1].vector.is_some(), "{format}");
                let first = r.rows[0][1].as_str().unwrap().to_string();
                let want = match *format {
                    "F1BIT_BLOB" => "[1,1,1,1]",
                    _ => "[1,2,3,4]",
                };
                if *format == "F8_BLOB" {
                    // Scaled bytes: close, not exact.
                    let n = dbine_driver::vector::parse(&first).unwrap();
                    assert!(
                        n.iter()
                            .zip([1.0, 2.0, 3.0, 4.0])
                            .all(|(a, b)| (a - b).abs() < 0.05),
                        "{first}"
                    );
                } else {
                    assert_eq!(first, want, "{format}");
                }
                // An edit with the types, found through the index.
                let up = d
                    .update_script_typed(
                        &table(name),
                        &[RowChange {
                            key: vec![("id".into(), json!(2))],
                            set: vec![("emb".into(), json!([4, -3, 2, -1]))],
                            row: Vec::new(),
                        }],
                        &types,
                    )
                    .unwrap();
                assert!(up.contains(&format!("{function}('[4,-3,2,-1]')")), "{up}");
                run(&mut s, &up).await;
                let out = run(
                    &mut s,
                    &format!(
                        "SELECT t.id FROM vector_top_k('{name}_emb', {function}('[4,-3,2,-1]'), 1) k JOIN \"{name}\" t ON t.rowid = k.id"
                    ),
                )
                .await;
                assert_eq!(out.results[0].rows[0][0], json!(2), "{format}");
                // The Índices view: DiskANN, the index's settings, a size.
                let usage = s.index_usage(&table(name)).await.unwrap().unwrap();
                let ix = usage
                    .indexes
                    .iter()
                    .find(|i| i.name == format!("{name}_emb"))
                    .unwrap_or_else(|| panic!("{usage:?}"));
                assert_eq!(ix.kind, "DISKANN");
                assert_eq!(ix.key_columns, ["emb"]);
                assert_eq!(
                    ix.vector.as_ref().and_then(|v| v.metric.as_deref()),
                    Some(if *format == "F1BIT_BLOB" {
                        "cosine"
                    } else {
                        "l2"
                    })
                );
                assert_eq!(ix.vector.as_ref().unwrap().params["m"], "16");
                assert!(ix.size_kb.is_some_and(|k| k > 0), "{ix:?}");
            }
        })
    };
    let r = body.await;
    drop_all(&names, meta_was_there).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
}

/// An L2 index on 1-bit vectors made by hand, which sqld takes (and then
/// ranks wrong): the table it's on still has a script. Its DDL, as a
/// clone, a script export or a same-engine migration asks for it, and a
/// compare's copy of the table, run on the server and give the same index,
/// with a warning. The designer still can't make one. The other refusals
/// (no dimension, 1-bit compression with L2) are sqld's own: no table can
/// hold those.
#[tokio::test]
#[ignore]
async fn an_index_the_server_holds_is_scripted() {
    let _one = LOCK.lock().await;
    let pid = std::process::id();
    let name = format!("vec_h_{pid}");
    let copy = format!("vec_hc_{pid}");
    let synced = format!("vec_hs_{pid}");
    let refused = format!("vec_hr_{pid}");
    let names = vec![name.clone(), copy.clone(), synced.clone(), refused.clone()];
    let meta_was_there = {
        let mut s = open().await;
        let r = run(
            &mut s,
            "SELECT count(*) FROM sqlite_master WHERE name = 'libsql_vector_meta_shadow'",
        )
        .await;
        r.results[0].rows[0][0] == json!(1)
    };
    drop_all(&names, true).await;
    let body = {
        let (name, copy, synced, refused) =
            (name.clone(), copy.clone(), synced.clone(), refused.clone());
        tokio::spawn(async move {
            let d = driver();
            let mut s = open().await;
            run(
                &mut s,
                &format!(
                    "CREATE TABLE \"{name}\" (id INTEGER PRIMARY KEY, bits F1BIT_BLOB(8));\n\
                     CREATE INDEX \"{name}_bits\" ON \"{name}\" (libsql_vector_idx(bits, 'metric=l2'));\n\
                     INSERT INTO \"{name}\" VALUES (1, vector1bit('[1,0,1,0,1,0,1,0]')), (2, vector1bit('[0,1,0,1,0,1,0,1]'));"
                ),
            )
            .await;
            // sqld's own refusals: no table holds such an index.
            run(
                &mut s,
                &format!("CREATE TABLE \"{refused}\" (a F32_BLOB(8), b F32_BLOB)"),
            )
            .await;
            for key in ["a, 'metric=l2', 'compress_neighbors=float1bit'", "b"] {
                let mut out = QueryOutcome::default();
                let r = s
                    .execute(
                        &format!(
                            "CREATE INDEX \"{refused}_x\" ON \"{refused}\" (libsql_vector_idx({key}))"
                        ),
                        10,
                        &mut out,
                    )
                    .await;
                assert!(
                    r.is_err() || out.error.is_some(),
                    "sqld took libsql_vector_idx({key})"
                );
            }

            // The table as the catalog read gives it.
            let read = |all: Vec<TableSchema>, n: &str| {
                all.into_iter()
                    .find(|t| t.name == n)
                    .unwrap_or_else(|| panic!("{n} not read"))
            };
            let t = read(s.database_schema().await.unwrap(), &name);
            let ix = &t.indexes[0];
            assert_eq!(
                ix.vector.as_ref().and_then(|v| v.metric.as_deref()),
                Some("l2")
            );
            assert!(dbine_driver_sqlite::schema::held_vector_index(ix));
            let all = DdlParts {
                create: true,
                indexes: true,
                ..Default::default()
            };
            // Its DDL, as a clone or an export asks for it, runs.
            let mut c = t.clone();
            c.name = copy.clone();
            c.indexes[0].name = format!("{copy}_bits");
            let ddl = d.table_ddl(&c, all).unwrap();
            assert!(
                ddl.contains(&format!(
                    "-- El índice vectorial «{copy}_bits» mide con distancia L2"
                )),
                "{ddl}"
            );
            run(&mut s, &ddl).await;
            // A compare's copy: the warning apart, the same index.
            let mut c = t.clone();
            c.name = synced.clone();
            c.indexes[0].name = format!("{synced}_bits");
            let script = d.sync_script(&[TableChange::Create { table: c }]).unwrap();
            assert_eq!(script.warnings.len(), 1, "{:?}", script.warnings);
            for st in &script.statements {
                run(&mut s, st).await;
            }
            let after = s.database_schema().await.unwrap();
            for n in [&copy, &synced] {
                let x = read(after.clone(), n);
                assert_eq!(x.indexes.len(), 1, "{n}");
                assert!(dbine_driver::vector::same_index(
                    &x.indexes[0].vector,
                    &t.indexes[0].vector
                ));
                assert_eq!(x.columns[1].data_type, "F1BIT_BLOB(8)");
            }
            run(
                &mut s,
                &format!("INSERT INTO \"{copy}\" SELECT * FROM \"{name}\""),
            )
            .await;
            let out = run(
                &mut s,
                &format!(
                    "SELECT count(*) FROM vector_top_k('{copy}_bits', vector1bit('[1,0,1,0,1,0,1,0]'), 2)"
                ),
            )
            .await;
            assert_eq!(out.results[0].rows[0][0], json!(2));
            // The designer's own L2 index on 1-bit vectors stays refused.
            let mut made = t.clone();
            made.name = refused.clone();
            made.indexes[0].options.clear();
            let e = d.table_ddl(&made, all).unwrap_err().to_string();
            assert!(e.contains("1 bit"), "{e}");
        })
    };
    let r = body.await;
    drop_all(&names, meta_was_there).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
}
