//! libSQL's vector columns in a SQLite file (one libSQL wrote, or one DBine
//! filled): results, describe, scripts and transfers. Embedded: a
//! temporary file, deleted at the end.

use dbine_driver::transfer::{
    BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    kinds, ConnectionConfig, Driver, ObjectRef, QueryOutcome, RowChange, Session, VectorElement,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

fn table(name: &str) -> ObjectRef {
    ObjectRef {
        kind: kinds::TABLE.into(),
        schema: None,
        name: name.into(),
    }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}\n{e}"));
    out
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

struct Rows(Option<Vec<Vec<Cell>>>);

#[dbine_driver::async_trait]
impl BatchSource for Rows {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.take().map(|rows| RowBatch { rows, bytes: 0 })
    }
}

#[tokio::test]
async fn libsql_vector_columns_in_a_file() {
    let path = std::env::temp_dir().join(format!("dbine-sqlite-vec-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let d: Arc<dyn Driver> = dbine_driver_sqlite::drivers().pop().unwrap();
    let cfg = ConnectionConfig {
        driver: "sqlite".into(),
        host: path.to_string_lossy().into(),
        ..Default::default()
    };
    let r = {
        let (d, cfg) = (d.clone(), cfg.clone());
        tokio::spawn(async move {
            let mut s = d.connect(&cfg, None).await.unwrap();
            run(
                &mut s,
                "CREATE TABLE v (id INTEGER PRIMARY KEY, e F32_BLOB(2), w F64_BLOB(2), b F1BIT_BLOB(3), q F8_BLOB(2), r FLOAT64);
                 CREATE TABLE w (id INTEGER PRIMARY KEY, e F32_BLOB(2), w F64_BLOB(2), b F1BIT_BLOB(3), q F8_BLOB(2), r FLOAT64)",
            )
            .await;
            // Insert script: a vector is the F32 blob (SQLite has no vector32()).
            let ins = d
                .insert_script(
                    &table("v"),
                    &["id".into(), "e".into(), "r".into()],
                    &[vec![json!(1), json!([0.5, -2]), json!(1.5)]],
                )
                .unwrap();
            assert!(ins.contains("X'0000003F000000C0'"), "{ins}");
            run(&mut s, &ins).await;
            // What libSQL itself writes for the other formats.
            run(
                &mut s,
                "UPDATE v SET w = X'000000000000F03F000000000000004002', b = X'050D03', q = X'007FFF008180003C0000803F000104' WHERE id = 1",
            )
            .await;
            let out = run(&mut s, "SELECT e, w, b, q, r FROM v").await;
            let res = &out.results[0];
            assert_eq!(
                res.columns
                    .iter()
                    .map(|c| c.vector.as_ref().map(|v| (v.dims, v.element)))
                    .collect::<Vec<_>>(),
                [
                    Some((Some(2), VectorElement::F32)),
                    Some((Some(2), VectorElement::F64)),
                    Some((Some(3), VectorElement::Bit)),
                    Some((Some(2), VectorElement::Other)),
                    None
                ]
            );
            assert_eq!(
                res.rows[0],
                vec![
                    json!("[0.5,-2]"),
                    json!("[1,2]"),
                    json!("[1,-1,1]"),
                    json!("[1,1.9960785,3]"),
                    json!(1.5)
                ]
            );
            // Describe keeps the declared type.
            let cols = s.columns(&table("v")).await.unwrap();
            assert_eq!(cols[1].data_type, "F32_BLOB(2)");
            let schema = s.database_schema().await.unwrap();
            assert_eq!(schema[0].columns[2].data_type, "F64_BLOB(2)");
            // An edit.
            let up = d
                .update_script(
                    &table("v"),
                    &[RowChange {
                        key: vec![("id".into(), json!(1))],
                        set: vec![("e".into(), json!([3, 4.25]))],
                        row: Vec::new(),
                    }],
                )
                .unwrap();
            run(&mut s, &up).await;
            let out = run(&mut s, "SELECT e FROM v").await;
            assert_eq!(out.results[0].rows[0][0], json!("[3,4.25]"));
            // Transfer: vector text out, each column's format in.
            let sink = Arc::new(Mutex::new(Collect::default()));
            let spec = ReadSpec {
                table: table("v"),
                columns: None,
                filter: None,
            };
            s.read_batches(&spec, sink.clone()).await.unwrap();
            let rows = std::mem::take(&mut sink.lock().unwrap().rows);
            assert_eq!(
                rows[0],
                vec![
                    Cell::Int(1),
                    Cell::Text("[3,4.25]".into()),
                    Cell::Text("[1,2]".into()),
                    Cell::Text("[1,-1,1]".into()),
                    Cell::Text("[1,1.9960785,3]".into()),
                    Cell::Float(1.5)
                ]
            );
            let load = LoadSpec {
                table: table("w"),
                columns: ["id", "e", "w", "b", "q", "r"]
                    .iter()
                    .map(|c| c.to_string())
                    .collect(),
                table_lock: false,
                keep_identity: true,
                commit_rows: 0,
                commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
            };
            s.bulk_load(&load, &[], &mut Rows(Some(rows)), &|_| {})
                .await
                .unwrap();
            let out = run(
                &mut s,
                "SELECT hex(e), hex(w), hex(b), length(q), e, w, b, q FROM w",
            )
            .await;
            assert_eq!(
                out.results[0].rows[0],
                vec![
                    json!("0000404000008840"),
                    json!("000000000000F03F000000000000004002"),
                    json!("050D03"),
                    json!(15),
                    json!("[3,4.25]"),
                    json!("[1,2]"),
                    json!("[1,-1,1]"),
                    json!("[1,1.9960785,3]")
                ]
            );
        })
    };
    let r = r.await;
    let _ = std::fs::remove_file(&path);
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
}

/// A table with a libSQL vector index (a pgvector table converted for
/// SQLite, or one a libSQL file has): SQLite's DDL and sync scripts leave
/// the index out, saying why, and run on a real file.
#[tokio::test]
async fn libsql_vector_indexes_left_out_on_sqlite() {
    use dbine_driver::{ColumnDef, DdlParts, IndexDef, TableChange, TableSchema, VectorIndex};
    let path = std::env::temp_dir().join(format!("dbine-sqlite-vecix-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let d: Arc<dyn Driver> = dbine_driver_sqlite::drivers().pop().unwrap();
    let cfg = ConnectionConfig {
        driver: "sqlite".into(),
        host: path.to_string_lossy().into(),
        ..Default::default()
    };
    let r = {
        let (d, cfg) = (d.clone(), cfg.clone());
        tokio::spawn(async move {
            let col = |name: &str, ty: &str| ColumnDef {
                name: name.into(),
                data_type: ty.into(),
                nullable: name != "id",
                ..Default::default()
            };
            let t = TableSchema {
                name: "docs".into(),
                columns: vec![col("id", "INTEGER"), col("emb", "F32_BLOB(3)")],
                indexes: vec![
                    IndexDef {
                        name: "docs_emb".into(),
                        columns: vec!["emb".into()],
                        vector: Some(VectorIndex {
                            metric: Some("cosine".into()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    IndexDef {
                        name: "docs_id".into(),
                        columns: vec!["id".into()],
                        ..Default::default()
                    },
                ],
                ..Default::default()
            };
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
            assert!(!ddl.contains("CREATE INDEX \"docs_emb\""), "{ddl}");
            assert!(
                ddl.contains("-- El índice vectorial «docs_emb» no se crea"),
                "{ddl}"
            );
            let mut s = d.connect(&cfg, None).await.unwrap();
            run(&mut s, &ddl).await;
            let schema = s.database_schema().await.unwrap();
            assert_eq!(
                schema[0]
                    .indexes
                    .iter()
                    .map(|i| i.name.as_str())
                    .collect::<Vec<_>>(),
                ["docs_id"]
            );
            // A sync that adds the index, one that adds a column and one
            // that rebuilds the table (dropping that column).
            let mut wider = t.clone();
            wider.columns.push(col("n", "TEXT"));
            for (new, rebuild) in [(t.clone(), false), (wider, false), (t.clone(), true)] {
                let old = s.database_schema().await.unwrap().remove(0);
                let sync = d.sync_script(&[TableChange::Alter { old, new }]).unwrap();
                assert_eq!(
                    sync.statements.iter().any(|st| st.contains("DROP TABLE")),
                    rebuild,
                    "{:?}",
                    sync.statements
                );
                assert_eq!(
                    sync.warnings
                        .iter()
                        .filter(|w| w.contains("«docs_emb» no se crea"))
                        .count(),
                    1,
                    "{:?}",
                    sync.warnings
                );
                for st in &sync.statements {
                    assert!(!st.contains("libsql_vector_idx"), "{st}");
                    run(&mut s, st).await;
                }
            }
            let schema = s.database_schema().await.unwrap();
            assert_eq!(
                schema[0]
                    .columns
                    .iter()
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>(),
                ["id", "emb"]
            );
            assert_eq!(schema[0].indexes.len(), 1);
            assert!(schema[0].indexes[0].vector.is_none());
        })
    };
    let r = r.await;
    let _ = std::fs::remove_file(&path);
    if let Err(e) = r {
        std::panic::resume_unwind(e.into_panic());
    }
}
