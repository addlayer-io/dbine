//! Vectors (embeddings) against Neo4j 5 and Memgraph: a vector index made
//! by the driver's own DDL from the designer's `TableSchema` (and another
//! from a label's `IndexDef::vector`, as a schema conversion gives it),
//! nodes inserted and edited with the driver's scripts, read back as
//! vectors, the property described as `LIST<FLOAT>` and the index listed
//! with its settings, a similarity query, and a transfer (to the same
//! engine, and from a pgvector-shaped source: text cells with a JSON
//! array). Everything is under the label `DbineVecDoc` (and its copy),
//! removed first and at the end.
//!
//! ```sh
//! docker start dbine-test-neo4j dbine-test-memgraph
//! DBINE_TEST_NEO4J_URL=neo4j:dbine-test-pass@localhost:17687 DBINE_TEST_MEMGRAPH_URL=localhost:27687 \
//!   cargo test -p dbine-driver-neo4j --test vectors -- --ignored --nocapture
//! ```

use dbine_driver::transfer::{
    BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    kinds, ColumnDef, ConnectionConfig, DdlParts, Driver, IndexDef, ObjectRef, QueryOutcome,
    RowChange, Session, TableSchema, VectorElement, VectorIndex,
};
use serde_json::json;
use std::sync::{Arc, Mutex};

const LABEL: &str = "DbineVecDoc";
const COPY: &str = "DbineVecCopy";

fn cfg(driver: &str, url: &str) -> ConnectionConfig {
    let (auth, hp) = url
        .rsplit_once('@')
        .map_or((None, url), |(a, h)| (Some(a), h));
    let (host, port) = hp.rsplit_once(':').unwrap();
    let (user, pass) = auth
        .and_then(|a| a.split_once(':'))
        .map_or((None, None), |(u, p)| {
            (Some(u.to_string()), Some(p.to_string()))
        });
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: user,
        password: pass,
        ..Default::default()
    }
}

async fn open(id: &str, url: &str) -> (Arc<dyn Driver>, Box<dyn Session>) {
    let d = dbine_driver_neo4j::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap();
    let s = d.connect(&cfg(id, url), None).await.unwrap();
    (d, s)
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(text, 1000, &mut out).await {
        panic!("{text}: {e}");
    }
    if let Some(e) = &out.error {
        panic!("{text}: {e}");
    }
    out
}

/// Drops what the test makes, whatever is there.
async fn cleanup(id: &str, s: &mut Box<dyn Session>) {
    let stmts: Vec<String> = if id == "neo4j" {
        vec![
            "DROP INDEX dbine_vec_doc IF EXISTS".into(),
            "DROP INDEX dbine_vec_doc2 IF EXISTS".into(),
        ]
    } else {
        vec![
            "DROP VECTOR INDEX dbine_vec_doc".into(),
            "DROP VECTOR INDEX dbine_vec_doc2".into(),
        ]
    };
    for q in stmts.into_iter().chain([
        format!("MATCH (n:{LABEL}) DETACH DELETE n"),
        format!("MATCH (n:{COPY}) DETACH DELETE n"),
    ]) {
        let mut out = QueryOutcome::default();
        let _ = s.execute(&q, 10, &mut out).await;
    }
}

#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    rows: Vec<Vec<Cell>>,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> std::io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> std::io::Result<()> {
        self.rows.extend(b.rows);
        Ok(())
    }
}

struct Once(Option<Vec<Vec<Cell>>>);

#[dbine_driver::async_trait]
impl BatchSource for Once {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.take().map(|rows| RowBatch { rows, bytes: 0 })
    }
}

fn label(name: &str) -> ObjectRef {
    ObjectRef {
        kind: "label".into(),
        schema: None,
        name: name.into(),
    }
}

#[tokio::test]
#[ignore]
async fn vectors_neo4j() {
    let url = std::env::var("DBINE_TEST_NEO4J_URL")
        .unwrap_or_else(|_| "neo4j:dbine-test-pass@localhost:17687".into());
    both_ways("neo4j", url).await;
}

#[tokio::test]
#[ignore]
async fn vectors_memgraph() {
    let url = std::env::var("DBINE_TEST_MEMGRAPH_URL").unwrap_or_else(|_| "localhost:27687".into());
    both_ways("memgraph", url).await;
}

/// The protocol: Neo4j 5.26 answers the version manifest and the driver
/// takes 5.8 (Neo4j 2025.10+ would give 6.0, which carries VECTOR), whose
/// GQL statuses and `neo4j_code` failures read as notifications and codes;
/// Memgraph (no manifest) stays on the classic proposals. Read-only.
#[tokio::test]
#[ignore]
async fn bolt_protocol_versions() {
    for (id, var, default) in [
        (
            "neo4j",
            "DBINE_TEST_NEO4J_URL",
            "neo4j:dbine-test-pass@localhost:17687",
        ),
        ("memgraph", "DBINE_TEST_MEMGRAPH_URL", "localhost:27687"),
    ] {
        let url = std::env::var(var).unwrap_or_else(|_| default.into());
        let (_, mut s) = open(id, &url).await;
        let snap = s.monitor().await.unwrap();
        let protocol = snap
            .info
            .iter()
            .find(|(k, _)| k == "Protocolo")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        eprintln!("{id}: {protocol}");
        if id == "neo4j" {
            assert!(
                protocol.contains("Bolt 5.8") || protocol.contains("Bolt 6."),
                "{protocol}"
            );
            // A notification (a cartesian product) from the GQL statuses.
            let out = run(&mut s, "EXPLAIN MATCH (a), (b) RETURN count(*)").await;
            assert!(
                out.log.iter().any(|m| m
                    .code
                    .as_deref()
                    .is_some_and(|c| c.contains("CartesianProduct"))),
                "{:?}",
                out.log
            );
        } else {
            assert!(protocol.contains("Bolt "), "{protocol}");
        }
        // A failure keeps its code.
        let mut out = QueryOutcome::default();
        let e = match s.execute("RETURN 1 +", 10, &mut out).await {
            Err(e) => format!("{e:?}"),
            Ok(()) => format!("{:?} {:?}", out.error, out.errors),
        };
        if id == "neo4j" {
            assert!(e.contains("Neo.ClientError.Statement.SyntaxError"), "{e}");
        } else {
            assert!(!e.is_empty());
        }
    }
}

async fn both_ways(id: &'static str, url: String) {
    let (d, mut s) = open(id, &url).await;
    cleanup(id, &mut s).await;
    let outcome = tokio::spawn(async move {
        scenario(id, d.as_ref(), &mut s).await;
    })
    .await;
    let (_, mut s) = open(id, &url).await;
    cleanup(id, &mut s).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn scenario(id: &str, d: &dyn Driver, s: &mut Box<dyn Session>) {
    let neo = id == "neo4j";
    // 1. The index from the designer: kind index, VECTOR, its settings.
    let mut opts: Vec<(&str, &str)> = vec![
        ("target", LABEL),
        ("entity", "node"),
        ("index_type", "VECTOR"),
    ];
    if neo {
        opts.extend([
            ("vector.dimensions", "3"),
            ("vector.similarity_function", "COSINE"),
            ("vector.hnsw.m", "24"),
            ("vector.quantization.enabled", "false"),
        ]);
    } else {
        opts.extend([
            ("dimension", "3"),
            ("metric", "cos"),
            ("capacity", "100"),
            ("resize_coefficient", "3"),
        ]);
    }
    let designed = TableSchema {
        kind: kinds::INDEX.into(),
        name: "dbine_vec_doc".into(),
        columns: vec![ColumnDef {
            name: "emb".into(),
            ..Default::default()
        }],
        options: opts
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    };
    let ddl = d
        .table_ddl(
            &designed,
            DdlParts {
                create: true,
                ..Default::default()
            },
        )
        .unwrap();
    // The designer's quantization (Neo4j) and growth (Memgraph) go in, and
    // the server takes them.
    assert!(
        ddl.contains(if neo {
            "`vector.quantization.enabled`: false"
        } else {
            "\"resize_coefficient\": 3"
        }),
        "{ddl}"
    );
    run(s, &ddl).await;
    // Settings the server would refuse are refused before anything runs.
    let mut bad = designed.clone();
    bad.options.insert(
        if neo { "vector.hnsw.m" } else { "dimension" }.into(),
        "0".into(),
    );
    let e = d
        .table_ddl(
            &bad,
            DdlParts {
                create: true,
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(e.to_string().contains("no sirve"), "{e}");
    // And one from a label's IndexDef with only vector settings (what a
    // conversion from another engine gives), on another property.
    let mut vi = VectorIndex {
        metric: Some("l2".into()),
        ..Default::default()
    };
    vi.params.insert("dimensions".into(), "2".into());
    let label_schema = TableSchema {
        kind: "label".into(),
        name: LABEL.into(),
        indexes: vec![IndexDef {
            name: "dbine_vec_doc2".into(),
            columns: vec!["small".into()],
            vector: Some(vi),
            ..Default::default()
        }],
        ..Default::default()
    };
    let ddl2 = d
        .table_ddl(
            &label_schema,
            DdlParts {
                indexes: true,
                ..Default::default()
            },
        )
        .unwrap();
    run(s, &ddl2).await;
    if neo {
        run(s, "CALL db.awaitIndexes(60)").await;
    }

    // 2. Nodes from the insert script, vectors as JSON arrays (as the grid
    // gives them: `rows_for_scripts`).
    let cols = vec![
        dbine_driver::ResultColumn::new("id", ""),
        dbine_driver::ResultColumn::new("emb", "LIST<FLOAT>")
            .with_vector(dbine_driver::VectorMeta::dense(Some(3), VectorElement::F64)),
        dbine_driver::ResultColumn::new("small", "LIST<FLOAT>")
            .with_vector(dbine_driver::VectorMeta::dense(Some(2), VectorElement::F64)),
    ];
    let rows = dbine_driver::vector::rows_for_scripts(
        &cols,
        &[
            vec![json!(1), json!("[1,0,0]"), json!("[1,0]")],
            vec![json!(2), json!("[0.1,0.2,0.3]"), json!("[0,1]")],
            vec![json!(3), json!("[-1.5,2.25,1e-7]"), json!("[0.5,0.5]")],
        ],
    );
    let ins = d
        .insert_script(
            &label(LABEL),
            &["id".into(), "emb".into(), "small".into()],
            &rows,
        )
        .unwrap();
    run(s, &ins).await;

    // 3. Read back: the column is a vector, the text exact.
    let out = run(
        s,
        &format!("MATCH (n:{LABEL}) RETURN n.id AS id, n.emb AS emb ORDER BY id"),
    )
    .await;
    let r = out.results[0].clone();
    let v = r.columns[1].vector.as_ref().expect("vector column");
    assert_eq!(v.dims, Some(3));
    assert_eq!(r.columns[0].vector, None);
    let cells: Vec<_> = r.rows.iter().map(|row| row[1].clone()).collect();
    assert_eq!(
        cells,
        [
            json!("[1,0,0]"),
            json!("[0.1,0.2,0.3]"),
            json!("[-1.5,2.25,1e-7]")
        ]
        .map(|c| json!(dbine_driver::vector::text(
            dbine_driver::vector::parse(c.as_str().unwrap()).unwrap()
        )))
    );

    // 4. Described: the property is a list of floats; the dimension is the
    // index's (5).
    let cols = s.columns(&label(LABEL)).await.unwrap();
    assert_eq!(
        cols.iter().find(|c| c.name == "emb").unwrap().data_type,
        "LIST<FLOAT>"
    );

    // 5. The indexes, with their vector settings.
    let schema = s.database_schema().await.unwrap();
    let t = schema
        .iter()
        .find(|t| t.kind == "label" && t.name == LABEL)
        .unwrap();
    let ix = t
        .indexes
        .iter()
        .find(|i| i.name == "dbine_vec_doc")
        .unwrap();
    let got = ix.vector.as_ref().expect("vector index");
    assert_eq!(got.metric.as_deref(), Some("cosine"), "{ix:?}");
    if neo {
        assert_eq!(
            got.params.get("m").map(String::as_str),
            Some("24"),
            "{ix:?}"
        );
        assert_eq!(
            got.params.get("quantization").map(String::as_str),
            Some("none"),
            "{ix:?}"
        );
    }
    assert_eq!(
        got.params.get("dimensions").map(String::as_str),
        Some("3"),
        "{ix:?}"
    );
    let ix2 = t
        .indexes
        .iter()
        .find(|i| i.name == "dbine_vec_doc2")
        .unwrap();
    let got2 = ix2.vector.as_ref().expect("vector index");
    assert_eq!(got2.metric.as_deref(), Some("l2"), "{ix2:?}");
    assert_eq!(
        got2.params.get("dimensions").map(String::as_str),
        Some("2"),
        "{ix2:?}"
    );
    // Nothing to sync against itself.
    let same = d
        .sync_script(&[dbine_driver::TableChange::Alter {
            old: t.clone(),
            new: t.clone(),
        }])
        .unwrap();
    assert!(same.statements.is_empty(), "{:?}", same.statements);
    // A changed distance remakes the index.
    let mut changed = t.clone();
    for i in changed
        .indexes
        .iter_mut()
        .filter(|i| i.name == "dbine_vec_doc")
    {
        i.vector.as_mut().unwrap().metric = Some("l2".into());
    }
    let sync = d
        .sync_script(&[dbine_driver::TableChange::Alter {
            old: t.clone(),
            new: changed,
        }])
        .unwrap();
    assert_eq!(sync.statements.len(), 2, "{:?}", sync.statements);
    // The index usage report lists it with the same settings.
    let usage = s.index_usage(&label(LABEL)).await.unwrap().unwrap();
    let u = usage
        .indexes
        .iter()
        .find(|i| i.name == "dbine_vec_doc")
        .unwrap_or_else(|| panic!("{:?}", usage.indexes));
    assert_eq!(u.vector.as_ref(), Some(got), "{u:?}");

    // 6. A similarity query: the index's and the function's.
    let q = if neo {
        "CALL db.index.vector.queryNodes('dbine_vec_doc', 1, [1.0, 0.0, 0.0]) YIELD node, score RETURN node.id AS id, score".to_string()
    } else {
        "CALL vector_search.search('dbine_vec_doc', 1, [1.0, 0.0, 0.0]) YIELD node, similarity RETURN node.id AS id, similarity".to_string()
    };
    let out = run(s, &q).await;
    assert_eq!(out.results[0].rows[0][0], json!(1), "{q}");
    if neo {
        let out = run(
            s,
            &format!("MATCH (n:{LABEL}) RETURN n.id AS id, vector.similarity.cosine(n.emb, [1.0, 0.0, 0.0]) AS sim ORDER BY sim DESC LIMIT 1"),
        )
        .await;
        assert_eq!(out.results[0].rows[0][0], json!(1));
    }

    // 7. An edit from the grid: the vector cell as a JSON array.
    let r_cols = r.columns.clone();
    let edit =
        dbine_driver::vector::rows_for_scripts(&r_cols, &[vec![json!(2), json!("[0.5,0.25,-1]")]]);
    let up = d
        .update_script(
            &label(LABEL),
            &[RowChange {
                key: vec![("id".into(), json!(2))],
                set: vec![("emb".into(), edit[0][1].clone())],
                row: Vec::new(),
            }],
        )
        .unwrap();
    run(s, &up).await;
    let out = run(
        s,
        &format!("MATCH (n:{LABEL} {{id: 2}}) RETURN n.emb AS emb"),
    )
    .await;
    assert_eq!(out.results[0].rows[0][0], json!("[0.5,0.25,-1]"));
    assert!(out.results[0].columns[0].vector.is_some());
    // The same edit knowing the result's types: a float list stays one.
    let typed = d
        .update_script_typed(
            &label(LABEL),
            &[RowChange {
                key: vec![("id".into(), json!(2))],
                set: vec![("emb".into(), json!([0.5, 0.25, -1.5]))],
                row: Vec::new(),
            }],
            &out.results[0].columns,
        )
        .unwrap();
    assert!(typed.contains("n.emb = [0.5, 0.25, -1.5]"), "{typed}");
    run(s, &typed).await;
    let out = run(
        s,
        &format!("MATCH (n:{LABEL} {{id: 2}}) RETURN n.emb AS emb"),
    )
    .await;
    assert_eq!(out.results[0].rows[0][0], json!("[0.5,0.25,-1.5]"));
    run(s, &typed.replace("-1.5", "-1")).await;

    // 8. Transfer: read as vector text, load into a copy (same engine);
    // then a pgvector-shaped source (text cells with a JSON array).
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(
        &ReadSpec {
            table: label(LABEL),
            columns: Some(vec!["id".into(), "emb".into()]),
            filter: None,
        },
        sink.clone(),
    )
    .await
    .unwrap();
    let mut got = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(got.columns[1].type_name, "LIST<FLOAT>");
    got.rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => 0,
    });
    assert_eq!(got.rows[0][1], Cell::Json("[1,0,0]".into()));
    assert_eq!(got.rows[1][1], Cell::Json("[0.5,0.25,-1]".into()));
    let spec = LoadSpec {
        table: label(COPY),
        columns: vec!["id".into(), "emb".into()],
        table_lock: false,
        keep_identity: false,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let n = s
        .bulk_load(
            &spec,
            &got.columns,
            &mut Once(Some(got.rows.clone())),
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(n, 3);
    let pg_cols = vec![
        TransferColumn {
            name: "id".into(),
            type_name: "integer".into(),
            nullable: false,
        },
        TransferColumn {
            name: "emb".into(),
            type_name: "vector(3)".into(),
            nullable: true,
        },
    ];
    let pg_like = vec![
        vec![Cell::Int(10), Cell::Text("[0.25,-0.5,4]".into())],
        vec![Cell::Int(11), Cell::Text("[1,2,3]".into())],
    ];
    let n = s
        .bulk_load(&spec, &pg_cols, &mut Once(Some(pg_like)), &|_| {})
        .await
        .unwrap();
    assert_eq!(n, 2);
    let out = run(
        s,
        &format!("MATCH (n:{COPY}) RETURN n.id AS id, n.emb AS emb ORDER BY id"),
    )
    .await;
    let r = &out.results[0];
    assert!(r.columns[1].vector.is_some(), "{:?}", r.columns[1]);
    let texts: Vec<_> = r.rows.iter().map(|r| r[1].clone()).collect();
    assert_eq!(
        texts,
        [
            json!("[1,0,0]"),
            json!("[0.5,0.25,-1]"),
            json!("[-1.5,2.25,1e-7]")
                .as_str()
                .map(|t| json!(dbine_driver::vector::text(
                    dbine_driver::vector::parse(t).unwrap()
                )))
                .unwrap(),
            json!("[0.25,-0.5,4]"),
            json!("[1,2,3]")
        ]
    );
    // The loaded lists are floats: the vector function takes them.
    if neo {
        let out = run(
            s,
            &format!("MATCH (n:{COPY} {{id: 11}}) RETURN vector.similarity.euclidean(n.emb, [1.0, 2.0, 3.0]) AS sim"),
        )
        .await;
        assert_eq!(out.results[0].rows[0][0], json!(1.0));
    }
    eprintln!("{id} vectors: indexes, scripts, queries and transfer checked");
}
