//! `vector(n)` against a real SQL Server 2025: a table and a DiskANN index
//! made from the designer's `TableSchema`, rows written with the edit
//! scripts, read back marked as vectors, the declared type kept, the index
//! listed, a similarity search, a sync that changes the index, transfers
//! (another engine's batches in, a typed read out, native copies raw and
//! decoded) and the clone script. It works in its own databases, dropped
//! first and at the end. Reads `DBINE_TEST_SQLSERVER2025_URL`
//! (`mssql://user:pass@host:port`) and is skipped without it:
//!
//! ```sh
//! DBINE_TEST_SQLSERVER2025_URL='mssql://sa:Pw_12345!@localhost:27202' \
//!   cargo test -p dbine-driver-sqlserver --test vectors -- --ignored --test-threads=1
//! ```

use dbine_driver::transfer::{
    BatchSink, BatchSource, Cell, CopySpec, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    async_trait, ColumnDef, ConnectionConfig, DdlParts, Driver, IndexDef, KeyDef, ObjectRef,
    QueryOutcome, RowChange, Session, TableChange, TableSchema, VectorElement, VectorIndex,
    VectorMeta,
};
use futures::FutureExt;
use serde_json::{json, Value};
use std::io;
use std::sync::{Arc, Mutex};

const ENV: &str = "DBINE_TEST_SQLSERVER2025_URL";
const DB: &str = "dbine_vec_test";
const DB2: &str = "dbine_vec_test2";

fn config() -> Option<ConnectionConfig> {
    let Ok(url) = std::env::var(ENV) else {
        eprintln!("{ENV} not set; skipping");
        return None;
    };
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hostport.rsplit_once(':').unwrap();
    Some(ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    })
}

fn driver() -> Arc<dyn Driver> {
    dbine_driver_sqlserver::drivers().remove(0)
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{e}\n{sql}"));
    if let Some(e) = &out.error {
        panic!("{e}\n{sql}");
    }
    out
}

/// Each `GO` batch of a script, as the editor sends them.
async fn run_script(s: &mut Box<dyn Session>, script: &str) {
    for batch in script.split("\nGO\n") {
        if !batch.trim().is_empty() {
            run(s, batch).await;
        }
    }
}

fn table(name: &str) -> ObjectRef {
    ObjectRef {
        kind: "table".into(),
        schema: Some("dbo".into()),
        name: name.into(),
    }
}

/// Drops both test databases (and makes them again when `create`).
async fn databases(cfg: &ConnectionConfig, create: bool) {
    let mut admin = driver()
        .connect(cfg, Some("master"))
        .await
        .expect("connect");
    for db in [DB, DB2] {
        run(
            &mut admin,
            &format!(
                "IF DB_ID('{db}') IS NOT NULL BEGIN ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]; END"
            ),
        )
        .await;
        if create {
            run(&mut admin, &format!("CREATE DATABASE [{db}]")).await;
        }
    }
}

fn docs(name: &str, index: Option<VectorIndex>) -> TableSchema {
    let col = |name: &str, ty: &str, nullable| ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable,
        ..Default::default()
    };
    TableSchema {
        kind: "table".into(),
        schema: Some("dbo".into()),
        name: name.into(),
        columns: vec![
            col("id", "int", false),
            col("emb", "vector(3)", true),
            col("note", "nvarchar(20)", true),
        ],
        primary_key: Some(KeyDef {
            name: Some(format!("pk_{name}")),
            columns: vec!["id".into()],
        }),
        indexes: index
            .map(|v| IndexDef {
                name: format!("ix_{name}_emb"),
                columns: vec!["emb".into()],
                vector: Some(v),
                ..Default::default()
            })
            .into_iter()
            .collect(),
        ..Default::default()
    }
}

fn diskann(metric: &str) -> VectorIndex {
    VectorIndex {
        algorithm: Some("diskann".into()),
        metric: Some(metric.into()),
        ..Default::default()
    }
}

#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    batches: Vec<RowBatch>,
}
impl BatchSink for Collect {
    fn begin(&mut self, c: &[TransferColumn]) -> io::Result<()> {
        self.columns = c.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> io::Result<()> {
        self.batches.push(b);
        Ok(())
    }
}

struct Replay(std::vec::IntoIter<RowBatch>);
#[async_trait]
impl BatchSource for Replay {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.next()
    }
}

fn load(name: &str) -> LoadSpec {
    LoadSpec {
        table: table(name),
        columns: vec!["id".into(), "emb".into(), "note".into()],
        table_lock: false,
        keep_identity: false,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

/// `id → emb` as the server's canonical text, for comparing tables.
async fn vectors_of(s: &mut Box<dyn Session>, name: &str) -> Vec<(i64, Value)> {
    let out = run(s, &format!("SELECT id, emb FROM dbo.{name} ORDER BY id")).await;
    let r = &out.results[0];
    assert_eq!(
        r.columns[1].vector,
        Some(VectorMeta::dense(Some(3), VectorElement::F32)),
        "{name}"
    );
    r.rows
        .iter()
        .map(|row| (row[0].as_i64().unwrap(), row[1].clone()))
        .collect()
}

#[tokio::test]
#[ignore]
async fn vectors_end_to_end() {
    let Some(cfg) = config() else { return };
    databases(&cfg, true).await;
    let res = std::panic::AssertUnwindSafe(body(&cfg))
        .catch_unwind()
        .await;
    databases(&cfg, false).await;
    if let Err(p) = res {
        std::panic::resume_unwind(p);
    }
}

async fn body(cfg: &ConnectionConfig) {
    let d = driver();
    assert!(d.capabilities().vectors);
    let spec = d
        .designer()
        .unwrap()
        .vector_indexes
        .expect("vector indexes");
    assert_eq!(spec.algorithms, ["diskann"]);
    let mut s = d.connect(cfg, Some(DB)).await.expect("connect");
    // CREATE VECTOR INDEX is a preview feature of SQL Server 2025.
    run(
        &mut s,
        "ALTER DATABASE SCOPED CONFIGURATION SET PREVIEW_FEATURES = ON",
    )
    .await;

    // 1. The table, as the designer makes it; the index after the rows (a
    //    table with a vector index takes no more rows).
    let schema = docs("docs", Some(diskann("cosine")));
    let create = d
        .table_ddl(
            &schema,
            DdlParts {
                create: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(create.contains("[emb] vector(3)"), "{create}");
    run(&mut s, &create).await;

    // 2. Rows from the insert script, vectors as JSON arrays.
    let mut rows = vec![
        vec![json!(1), json!([0.1, 0.2, 0.3]), json!("a")],
        vec![json!(2), json!([1.0, 0.0, 0.0]), json!("b")],
        vec![json!(3), json!([-1.5, 2.25, 1e-7]), json!("c")],
    ];
    for i in 4..60 {
        rows.push(vec![
            json!(i),
            json!([i as f64, (i % 7) as f64, 1.0]),
            Value::Null,
        ]);
    }
    let insert = d
        .insert_script(
            &table("docs"),
            &["id".into(), "emb".into(), "note".into()],
            &rows,
        )
        .unwrap();
    assert!(insert.contains("N'[0.1,0.2,0.3]'"), "{insert}");
    run(&mut s, &insert).await;

    // 3. Read back: the column says vector(3), the text is exact.
    let out = run(
        &mut s,
        "SELECT id, emb, note FROM dbo.docs WHERE id <= 3 ORDER BY id",
    )
    .await;
    let r = &out.results[0];
    assert_eq!(r.columns[1].type_name, "vector(3)");
    assert_eq!(
        r.columns[1].vector,
        Some(VectorMeta::dense(Some(3), VectorElement::F32))
    );
    assert_eq!(r.columns[0].vector, None);
    assert_eq!(r.rows[0][1], json!("[0.1,0.2,0.3]"));
    assert_eq!(r.rows[2][1], json!("[-1.5,2.25,0.0000001]"));
    // Table browsing too.
    let browse = s.browse_query(&table("docs"), 5);
    let out = run(&mut s, &browse).await;
    let r = &out.results[0];
    let at = r.columns.iter().position(|c| c.name == "emb").unwrap();
    assert!(r.columns[at].vector.is_some(), "{browse}");
    // An expression is a vector too, with its dimensions.
    let out = run(&mut s, "SELECT CAST('[1,2]' AS vector(2)) AS v, VECTOR_NORMALIZE(emb, 'norm2') AS n FROM dbo.docs WHERE id = 2").await;
    let r = &out.results[0];
    assert_eq!(r.columns[0].type_name, "vector(2)");
    assert_eq!(r.rows[0][0], json!("[1,2]"));
    assert_eq!(r.rows[0][1], json!("[1,0,0]"));

    // 4. The update script, before the index (it makes the table read-only).
    let update = d
        .update_script(
            &table("docs"),
            &[RowChange {
                key: vec![("id".into(), json!(2))],
                set: vec![("emb".into(), json!([0.0, 1.0, 0.5]))],
                ..Default::default()
            }],
        )
        .unwrap();
    run(&mut s, &update).await;
    let out = run(&mut s, "SELECT emb FROM dbo.docs WHERE id = 2").await;
    assert_eq!(out.results[0].rows[0][0], json!("[0,1,0.5]"));

    // 5. Describe keeps the dimension.
    let cols = s.columns(&table("docs")).await.unwrap();
    assert_eq!(cols[1].data_type, "vector(3)");

    // 6. The vector index, from the designer's IndexDef.
    let indexes = d
        .table_ddl(
            &schema,
            DdlParts {
                indexes: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        indexes.contains("CREATE VECTOR INDEX [ix_docs_emb] ON [dbo].[docs] ([emb]) WITH (METRIC = 'cosine', TYPE = 'DiskANN');"),
        "{indexes}"
    );
    run(&mut s, &indexes).await;
    let tables = s.database_schema().await.unwrap();
    let t = tables.iter().find(|t| t.name == "docs").unwrap();
    assert_eq!(t.columns[1].data_type, "vector(3)");
    let ix = t
        .indexes
        .iter()
        .find(|i| i.name == "ix_docs_emb")
        .expect("vector index listed");
    assert_eq!(ix.columns, ["emb"]);
    assert_eq!(ix.kind.as_deref(), Some("VECTOR"));
    assert_eq!(ix.vector, Some(diskann("cosine")));
    // The same index scripted from the catalog.
    let again = d
        .table_ddl(
            t,
            DdlParts {
                indexes: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        again.contains("CREATE VECTOR INDEX [ix_docs_emb]"),
        "{again}"
    );

    // 7. Similarity: exact and through the index.
    let out = run(
        &mut s,
        "SELECT TOP 2 id, VECTOR_DISTANCE('cosine', emb, CAST('[0.1,0.2,0.3]' AS vector(3))) AS d FROM dbo.docs ORDER BY d",
    )
    .await;
    assert_eq!(out.results[0].rows[0][0], json!(1));
    let out = run(
        &mut s,
        "DECLARE @q vector(3) = '[0.1,0.2,0.3]';
         SELECT t.id, s.distance FROM VECTOR_SEARCH(TABLE = dbo.docs AS t, COLUMN = emb, SIMILAR_TO = @q, METRIC = 'cosine', TOP_N = 3) AS s ORDER BY s.distance",
    )
    .await;
    let r = out.results.iter().rfind(|r| !r.columns.is_empty()).unwrap();
    assert_eq!(r.rows[0][0], json!(1));

    // 8. A sync that changes the distance remakes the index.
    let mut new = t.clone();
    new.indexes
        .iter_mut()
        .find(|i| i.name == "ix_docs_emb")
        .unwrap()
        .vector = Some(diskann("l2"));
    let sync = d
        .sync_script(&[TableChange::Alter {
            old: t.clone(),
            new,
        }])
        .unwrap();
    let text = sync.statements.join("\n");
    assert!(text.contains("DROP INDEX [ix_docs_emb]"), "{text}");
    assert!(text.contains("METRIC = 'euclidean'"), "{text}");
    for st in &sync.statements {
        run(&mut s, st).await;
    }
    let tables = s.database_schema().await.unwrap();
    let t = tables.iter().find(|t| t.name == "docs").unwrap();
    let ix = t.indexes.iter().find(|i| i.name == "ix_docs_emb").unwrap();
    assert_eq!(ix.vector, Some(diskann("l2")));

    // 9. Transfers. A typed read gives the canonical text…
    let sink = Arc::new(Mutex::new(Collect::default()));
    let n = s
        .read_batches(
            &ReadSpec {
                table: table("docs"),
                columns: None,
                filter: None,
            },
            sink.clone(),
        )
        .await
        .unwrap();
    assert_eq!(n, 59);
    let read = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(read.columns[1].type_name, "vector(3)");
    let first = &read.batches[0].rows[0];
    assert_eq!(first[1], Cell::Text("[0.1,0.2,0.3]".into()));
    // …which loads into another table, with another engine's batches
    // (pgvector's text) after it.
    let copy = docs("docs_copy", None);
    run(
        &mut s,
        &d.table_ddl(
            &copy,
            DdlParts {
                create: true,
                ..Default::default()
            },
        )
        .unwrap(),
    )
    .await;
    let mut batches = read.batches.clone();
    batches.push(RowBatch {
        rows: vec![vec![
            Cell::Int(100),
            Cell::Text("[1, 2.5, -3]".into()),
            Cell::Text("pgvector".into()),
        ]],
        bytes: 64,
    });
    let loaded = s
        .bulk_load(
            &load("docs_copy"),
            &read.columns,
            &mut Replay(batches.into_iter()),
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(loaded, 60);
    let mut want = vectors_of(&mut s, "docs").await;
    want.push((100, json!("[1,2.5,-3]")));
    assert_eq!(vectors_of(&mut s, "docs_copy").await, want);
    // A vector of the wrong size is refused with the reason.
    let wrong = s
        .bulk_load(
            &load("docs_copy"),
            &read.columns,
            &mut Replay(
                vec![RowBatch {
                    rows: vec![vec![Cell::Int(200), Cell::Text("[1,2]".into()), Cell::Null]],
                    bytes: 32,
                }]
                .into_iter(),
            ),
            &|_| {},
        )
        .await
        .unwrap_err();
    assert!(wrong.to_string().contains("dimensiones"), "{wrong}");

    // Native copies into the second database: raw TDS rows, then decoded.
    for (decoded, name) in [(false, "docs_raw"), (true, "docs_decoded")] {
        let mut dst = d.connect(cfg, Some(DB2)).await.unwrap();
        run(
            &mut dst,
            &d.table_ddl(
                &docs(name, None),
                DdlParts {
                    create: true,
                    ..Default::default()
                },
            )
            .unwrap(),
        )
        .await;
        if decoded {
            std::env::set_var("DBINE_SQLSERVER_NO_RAW", "1");
        } else {
            std::env::remove_var("DBINE_SQLSERVER_NO_RAW");
        }
        let mut src = d.connect(cfg, Some(DB)).await.unwrap();
        let n = d
            .copy_native(
                &mut *src,
                &mut *dst,
                &CopySpec {
                    source: ReadSpec {
                        table: table("docs"),
                        columns: Some(vec!["id".into(), "emb".into(), "note".into()]),
                        filter: None,
                    },
                    target: load(name),
                },
                &|_| {},
            )
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        std::env::remove_var("DBINE_SQLSERVER_NO_RAW");
        assert_eq!(n, 59, "{name}");
        assert_eq!(
            vectors_of(&mut dst, name).await,
            vectors_of(&mut s, "docs").await,
            "{name}"
        );
    }

    // 10. The clone keeps the type and makes the index after the rows.
    let mut src = d.connect(cfg, Some(DB)).await.unwrap();
    let mut dst = d.connect(cfg, Some(DB2)).await.unwrap();
    let clone = d
        .clone_script(&mut *src, &mut *dst, &[table("docs")])
        .await
        .unwrap();
    let ct = &clone.tables[0];
    assert!(ct.create.contains("[emb] vector(3)"), "{}", ct.create);
    let after = ct.after_data.join("\n");
    assert!(
        after.contains("CREATE VECTOR INDEX [ix_docs_emb] ON [dbo].[docs] ([emb]) WITH (METRIC = N'euclidean', TYPE = N'DiskANN')"),
        "{after}"
    );
    let mut dst = d.connect(cfg, Some(DB2)).await.unwrap();
    run(
        &mut dst,
        "ALTER DATABASE SCOPED CONFIGURATION SET PREVIEW_FEATURES = ON",
    )
    .await;
    for st in clone.before.iter().chain(std::iter::once(&ct.create)) {
        run_script(&mut dst, st).await;
    }
    for st in &ct.after_data {
        run_script(&mut dst, st).await;
    }
    let tables = dst.database_schema().await.unwrap();
    let t = tables.iter().find(|t| t.name == "docs").unwrap();
    assert_eq!(t.columns[1].data_type, "vector(3)");
    let ix = t.indexes.iter().find(|i| i.name == "ix_docs_emb").unwrap();
    assert_eq!(ix.vector, Some(diskann("l2")));
}
