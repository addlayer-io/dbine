//! GSI vector indexes against Couchbase Server Enterprise 8.0:
//!
//! ```sh
//! docker start dbine-test-couchbase-ee   # 25991 / 25993, Administrator / secreto1
//! DBINE_TEST_COUCHBASE_EE_URL=http://localhost:25993 DBINE_TEST_COUCHBASE_EE_MGMT_PORT=25991 \
//!   cargo test -p dbine-driver-couchbase --test vectors -- --ignored --nocapture
//! ```
//!
//! The test's bucket is dropped first and at the end. That container has no
//! Search service; Search (FTS) vector indexes run against a node that has
//! one (data, index, query and fts services):
//!
//! ```sh
//! DBINE_TEST_COUCHBASE_FTS_URL=http://localhost:25893 DBINE_TEST_COUCHBASE_FTS_MGMT_PORT=25891 \
//!   cargo test -p dbine-driver-couchbase --test vectors couchbase_search -- --ignored --nocapture
//! ```

use dbine_driver::async_trait;
use dbine_driver::transfer::{
    BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    vector, ColumnDef, ConnectionConfig, DdlParts, IndexDef, ObjectRef, QueryOutcome, ResultColumn,
    RowChange, Session, TableChange, TableSchema, VectorElement, VectorIndex, VectorMeta,
};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BUCKET: &str = "dbine_vec_it";

fn cfg(database: &str) -> Option<ConnectionConfig> {
    cfg_of("DBINE_TEST_COUCHBASE_EE", database)
}

fn cfg_of(prefix: &str, database: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(format!("{prefix}_URL")).ok()?).expect("URL");
    let mut c = ConnectionConfig {
        driver: "couchbase".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("Administrator".into()),
        password: Some("secreto1".into()),
        database: database.into(),
        ..Default::default()
    };
    c.options.insert(
        "mgmt_port".into(),
        std::env::var(format!("{prefix}_MGMT_PORT")).unwrap_or_else(|_| "8091".into()),
    );
    Some(c)
}

fn coll(name: &str) -> ObjectRef {
    ObjectRef {
        kind: "collection".into(),
        schema: Some(format!("{BUCKET}._default")),
        name: name.into(),
    }
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(text, 1000, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{text}: {e}"));
    out
}

/// Retries `text` while the cluster settles (a new bucket or collection
/// takes a few seconds to be queryable).
async fn run_settled(s: &mut Box<dyn Session>, text: &str) -> QueryOutcome {
    let start = Instant::now();
    loop {
        let mut out = QueryOutcome::default();
        match s.execute(text, 1000, &mut out).await {
            Ok(()) => return out,
            Err(e) if start.elapsed() < Duration::from_secs(90) => {
                eprintln!("retrying: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => panic!("{text}: {e}"),
        }
    }
}

/// Waits until the indexes are online.
async fn wait_online(s: &mut Box<dyn Session>, names: &[&str]) {
    let list = serde_json::to_string(names).unwrap();
    for _ in 0..120 {
        let out = run(
            s,
            &format!("SELECT RAW i.state FROM system:indexes AS i WHERE i.bucket_id = \"{BUCKET}\" AND i.name IN {list}"),
        )
        .await;
        let states = &out.results[0].rows;
        if states.len() == names.len() && states.iter().all(|r| r[0] == json!("online")) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("the indexes {names:?} never got online");
}

fn col(name: &str, t: &str) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        data_type: t.into(),
        nullable: true,
        ..Default::default()
    }
}

fn designed(name: &str, indexes: Vec<IndexDef>) -> TableSchema {
    TableSchema {
        kind: "collection".into(),
        schema: Some(format!("{BUCKET}._default")),
        name: name.into(),
        columns: vec![col("cat", "string"), col("emb", "vector(3)")],
        indexes,
        options: [("primary_index".to_string(), "true".to_string())].into(),
        ..Default::default()
    }
}

fn vix(name: &str, columns: &[&str], kind: Option<&str>, metric: &str) -> IndexDef {
    IndexDef {
        name: name.into(),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        kind: kind.map(str::to_string),
        vector: Some(VectorIndex {
            metric: Some(metric.into()),
            params: [("lists".to_string(), "1".to_string())].into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// With IF NOT EXISTS: safe to retry while the new bucket settles.
const ALL: DdlParts = DdlParts {
    drop: false,
    if_exists: true,
    create: true,
    indexes: true,
    foreign_keys: false,
};

#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    rows: Vec<Vec<Cell>>,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, batch: RowBatch) -> io::Result<()> {
        self.rows.extend(batch.rows);
        Ok(())
    }
}

struct Batches(VecDeque<RowBatch>);

#[async_trait]
impl BatchSource for Batches {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.pop_front()
    }
}

#[tokio::test]
#[ignore]
async fn couchbase_vector_indexes() {
    let Some(c) = cfg("") else {
        return;
    };
    let d = dbine_driver_couchbase::drivers().remove(0);
    assert!(d.capabilities().vectors);
    assert!(d.designer().unwrap().vector_indexes.unwrap().filter_columns);
    let mut admin = d.connect(&c, None).await.expect("connect");
    let _ = admin.drop_database(BUCKET).await;
    admin.create_database(BUCKET).await.expect("bucket");
    let mut s = d
        .connect(&cfg(BUCKET).unwrap(), Some(BUCKET))
        .await
        .unwrap();
    let ks = format!("`{BUCKET}`.`_default`.`docs`");

    // 1. The collection, a hyperscale index (with an INCLUDE field) and a
    // composite one, by the driver's DDL.
    let t = designed(
        "docs",
        vec![
            vix("emb_hyper", &["emb", "cat"], None, "l2_squared"),
            vix("emb_comp", &["emb", "cat"], Some("COMPOSITE"), "cosine"),
        ],
    );
    let ddl = d.table_ddl(&t, ALL).unwrap();
    assert!(
        ddl.contains("CREATE VECTOR INDEX `emb_hyper`") && ddl.contains("INCLUDE (`cat`)"),
        "{ddl}"
    );
    for st in d.split_script(&ddl) {
        run_settled(&mut s, &st.text).await;
    }

    // 2. insert_script with vectors as arrays.
    let cols = vec!["_id".to_string(), "cat".into(), "emb".into()];
    let result_cols = vec![
        ResultColumn::new("_id", ""),
        ResultColumn::new("cat", ""),
        ResultColumn::new("emb", "vector(3)")
            .with_vector(VectorMeta::dense(Some(3), VectorElement::F32)),
    ];
    let rows = vector::rows_for_scripts(
        &result_cols,
        &[
            vec![json!("1"), json!("a"), json!("[1,0,0]")],
            vec![json!("2"), json!("b"), json!("[0,1,0]")],
            vec![json!("3"), json!("a"), json!("[0.9,0.1,0]")],
        ],
    );
    run(
        &mut s,
        &d.insert_script(&coll("docs"), &cols, &rows).unwrap(),
    )
    .await;
    // Made deferred on the new collection: built now that it has documents.
    assert!(ddl.contains("\"defer_build\": true"), "{ddl}");
    run_settled(
        &mut s,
        &format!("BUILD INDEX ON {ks}(`emb_hyper`, `emb_comp`)"),
    )
    .await;
    wait_online(&mut s, &["emb_hyper", "emb_comp"]).await;

    // 3. Read back: marked, exact.
    let out = run(
        &mut s,
        &format!("SELECT META(d).id AS k, d.emb FROM {ks} AS d ORDER BY META(d).id"),
    )
    .await;
    let r = &out.results[0];
    let at = r.columns.iter().position(|c| c.name == "emb").unwrap();
    assert_eq!(
        r.columns[at].vector,
        Some(VectorMeta::dense(Some(3), VectorElement::F32))
    );
    let cells: Vec<&Value> = r.rows.iter().map(|row| &row[at]).collect();
    assert_eq!(
        cells,
        [&json!("[1,0,0]"), &json!("[0,1,0]"), &json!("[0.9,0.1,0]")]
    );

    // 4. Describe keeps the dimension; 5. the indexes have their vector.
    let schema = s.database_schema().await.unwrap();
    let tb = schema.iter().find(|x| x.name == "docs").expect("docs");
    assert_eq!(
        tb.columns
            .iter()
            .find(|c| c.name == "emb")
            .unwrap()
            .data_type,
        "vector(3)"
    );
    let hyper = tb.indexes.iter().find(|i| i.name == "emb_hyper").unwrap();
    assert_eq!(hyper.kind.as_deref(), Some("HYPERSCALE"));
    // Every key by its name, as the vector field.
    assert_eq!(hyper.columns, ["emb", "cat"]);
    assert_eq!(
        hyper.vector.as_ref().unwrap().metric.as_deref(),
        Some("l2_squared")
    );
    let comp = tb.indexes.iter().find(|i| i.name == "emb_comp").unwrap();
    assert_eq!(comp.kind.as_deref(), Some("COMPOSITE"));
    assert_eq!(
        comp.vector.as_ref().unwrap().metric.as_deref(),
        Some("cosine")
    );
    let usage = s.index_usage(&coll("docs")).await.unwrap().unwrap();
    let u = usage
        .indexes
        .iter()
        .find(|i| i.name == "emb_hyper")
        .unwrap();
    assert!(u.kind.starts_with("GSI VECTOR HYPERSCALE"), "{}", u.kind);
    // The explorer's vector indexes carry the schema's settings.
    for name in ["emb_hyper", "emb_comp"] {
        let u = usage.indexes.iter().find(|i| i.name == name).unwrap();
        let ix = tb.indexes.iter().find(|i| i.name == name).unwrap();
        assert!(u.vector.is_some(), "{name}");
        assert_eq!(u.vector, ix.vector, "{name}");
    }
    // Compared with itself, and with the designer's vector indexes:
    // nothing to do.
    let mut designer = tb.clone();
    designer.indexes.retain(|i| i.vector.is_none());
    designer.indexes.extend(t.indexes.iter().cloned());
    for new in [designer, tb.clone()] {
        let same = d
            .sync_script(&[TableChange::Alter {
                old: tb.clone(),
                new,
            }])
            .unwrap();
        assert!(same.statements.is_empty(), "{:?}", same.statements);
    }

    // 6. A similarity query (the hyperscale index's distance).
    let out = run(
        &mut s,
        &format!("SELECT RAW META(d).id FROM {ks} AS d ORDER BY APPROX_VECTOR_DISTANCE(d.emb, [1, 0, 0], \"L2_SQUARED\") LIMIT 2"),
    )
    .await;
    assert_eq!(out.results[0].rows, [[json!("1")], [json!("3")]]);

    // 7. update_script.
    let upd = d
        .update_script(
            &coll("docs"),
            &[RowChange {
                key: vec![("_id".into(), json!("1"))],
                set: vec![("emb".into(), json!([0.0, 0.25, 1.0]))],
                ..Default::default()
            }],
        )
        .unwrap();
    run(&mut s, &upd).await;
    let out = run(
        &mut s,
        &format!("SELECT d.emb FROM {ks} AS d USE KEYS \"1\""),
    )
    .await;
    assert_eq!(out.results[0].rows[0][0], json!("[0,0.25,1]"));

    // An index that changes: dropped and made again; the composite one
    // dropped (what "Eliminar índice" runs).
    let mut changed = tb.clone();
    changed.indexes.retain(|i| i.name != "emb_comp");
    changed
        .indexes
        .iter_mut()
        .find(|i| i.name == "emb_hyper")
        .unwrap()
        .vector
        .as_mut()
        .unwrap()
        .metric = Some("ip".into());
    let sync = d
        .sync_script(&[TableChange::Alter {
            old: tb.clone(),
            new: changed,
        }])
        .unwrap();
    assert_eq!(sync.statements.len(), 3, "{:?}", sync.statements);
    for st in &sync.statements {
        run_settled(&mut s, st).await;
    }
    let schema = s.database_schema().await.unwrap();
    let tb = schema.iter().find(|x| x.name == "docs").unwrap();
    assert!(tb.indexes.iter().all(|i| i.name != "emb_comp"));
    let hyper = tb.indexes.iter().find(|i| i.name == "emb_hyper").unwrap();
    assert_eq!(hyper.vector.as_ref().unwrap().metric.as_deref(), Some("ip"));

    // 8. Transfers: the read gives the vectors as JSON arrays; a
    // pgvector-like source (`vector(3)`, text cells) loads as arrays.
    let sink = Arc::new(Mutex::new(Collect::default()));
    let read = ReadSpec {
        table: coll("docs"),
        columns: None,
        filter: None,
    };
    s.read_batches(&read, sink.clone()).await.unwrap();
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let at = got.columns.iter().position(|c| c.name == "emb").unwrap();
    let texts: Vec<Option<Vec<f64>>> = got
        .rows
        .iter()
        .map(|r| match &r[at] {
            Cell::Json(j) | Cell::Text(j) => vector::parse(j),
            _ => None,
        })
        .collect();
    assert!(texts.contains(&Some(vec![0.9, 0.1, 0.0])), "{texts:?}");
    let target = designed("from_pg", vec![vix("pg_vix", &["emb"], None, "l2")]);
    for st in d.split_script(&d.table_ddl(&target, ALL).unwrap()) {
        run_settled(&mut s, &st.text).await;
    }
    let pg_cols = vec![
        TransferColumn {
            name: "id".into(),
            type_name: "text".into(),
            nullable: false,
        },
        TransferColumn {
            name: "emb".into(),
            type_name: "vector(3)".into(),
            nullable: true,
        },
    ];
    let load = LoadSpec {
        table: coll("from_pg"),
        columns: vec!["id".into(), "emb".into()],
        table_lock: false,
        keep_identity: true,
        commit_rows: 1000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let batch = RowBatch {
        rows: vec![vec![
            Cell::Text("p1".into()),
            Cell::Text("[0.1,0.2,0.3]".into()),
        ]],
        bytes: 0,
    };
    let n = s
        .bulk_load(&load, &pg_cols, &mut Batches(vec![batch].into()), &|_| {})
        .await
        .unwrap();
    assert_eq!(n, 1);
    let out = run_settled(
        &mut s,
        &format!("SELECT RAW ISARRAY(d.emb) FROM `{BUCKET}`.`_default`.`from_pg` AS d"),
    )
    .await;
    assert_eq!(out.results[0].rows, [[json!(true)]]);
    run_settled(
        &mut s,
        &format!("BUILD INDEX ON `{BUCKET}`.`_default`.`from_pg`(`pg_vix`)"),
    )
    .await;
    wait_online(&mut s, &["pg_vix"]).await;
    // From MongoDB (a BinData vector field, marked `bson:`) and from
    // Elasticsearch (`dense_vector`): arrays too, not strings.
    for (id, ty) in [
        ("m1", "bson:vector(3, float32)|bson:null"),
        ("e1", "dense_vector(3)"),
    ] {
        let cols = vec![
            TransferColumn {
                name: "id".into(),
                type_name: "bson:string".into(),
                nullable: false,
            },
            TransferColumn {
                name: "emb".into(),
                type_name: ty.into(),
                nullable: true,
            },
        ];
        let batch = RowBatch {
            rows: vec![vec![
                Cell::Text(id.into()),
                Cell::Text("[0.4,0.5,0.6]".into()),
            ]],
            bytes: 0,
        };
        s.bulk_load(&load, &cols, &mut Batches(vec![batch].into()), &|_| {})
            .await
            .unwrap();
        let out = run_settled(
            &mut s,
            &format!("SELECT RAW ISARRAY(d.emb) FROM `{BUCKET}`.`_default`.`from_pg` AS d USE KEYS \"{id}\""),
        )
        .await;
        assert_eq!(out.results[0].rows, [[json!(true)]], "{ty}");
    }

    // A vector index on a collection without documents (a clone without
    // its data): never a failure; built at once if the server can, else
    // deferred with a warning.
    let empty = designed(
        "empty",
        vec![vix("empty_vix", &["emb", "cat"], None, "cosine")],
    );
    for st in d.split_script(
        &d.table_ddl(
            &empty,
            DdlParts {
                create: true,
                if_exists: true,
                ..Default::default()
            },
        )
        .unwrap(),
    ) {
        run_settled(&mut s, &st.text).await;
    }
    let only_indexes = d
        .table_ddl(
            &empty,
            DdlParts {
                indexes: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!only_indexes.contains("defer_build"), "{only_indexes}");
    let out = run_settled(&mut s, &only_indexes).await;
    let state = run(
        &mut s,
        &format!("SELECT RAW i.state FROM system:indexes AS i WHERE i.bucket_id = \"{BUCKET}\" AND i.name = \"empty_vix\""),
    )
    .await;
    eprintln!(
        "vector index on an empty collection: state {:?}, messages {:?}",
        state.results[0].rows,
        out.log.iter().map(|m| m.text.clone()).collect::<Vec<_>>()
    );
    assert_eq!(state.results[0].rows.len(), 1, "the index exists");

    drop(s);
    admin.drop_database(BUCKET).await.expect("drop bucket");
}

/// Base64 vectors (`vector(N, base64)`: little-endian float32s as base64
/// text) with a hyperscale index on `DECODE_VECTOR(field, false)`: made by
/// the driver's DDL, written by typed scripts and loads as base64, read
/// as numbers, described, searched and copied.
#[tokio::test]
#[ignore]
async fn couchbase_base64_vectors() {
    const B: &str = "dbine_vec_b64";
    let Some(c) = cfg("") else {
        return;
    };
    let d = dbine_driver_couchbase::drivers().remove(0);
    assert!(d
        .designer()
        .unwrap()
        .data_types
        .contains(&"vector(1536, base64)"));
    let mut admin = d.connect(&c, None).await.expect("connect");
    let _ = admin.drop_database(B).await;
    admin.create_database(B).await.expect("bucket");
    let mut s = d
        .connect(&cfg_of("DBINE_TEST_COUCHBASE_EE", B).unwrap(), Some(B))
        .await
        .unwrap();
    let coll = |name: &str| ObjectRef {
        kind: "collection".into(),
        schema: Some(format!("{B}._default")),
        name: name.into(),
    };
    let ks = |name: &str| format!("`{B}`.`_default`.`{name}`");
    let raw =
        |name: &str, key: &str| format!("SELECT RAW d.e FROM {} AS d USE KEYS \"{key}\"", ks(name));

    // 1. The collection (a base64 field) and its hyperscale index, by the
    // driver's DDL: the key decodes the field.
    let t = TableSchema {
        kind: "collection".into(),
        schema: Some(format!("{B}._default")),
        name: "docs".into(),
        columns: vec![col("cat", "string"), col("e", "vector(3, base64)")],
        indexes: vec![vix("e_vix", &["e"], None, "l2")],
        options: [("primary_index".to_string(), "true".to_string())].into(),
        ..Default::default()
    };
    let ddl = d.table_ddl(&t, ALL).unwrap();
    assert!(
        ddl.contains("ON `dbine_vec_b64`.`_default`.`docs`(DECODE_VECTOR(`e`, false) VECTOR)"),
        "{ddl}"
    );
    for st in d.split_script(&ddl) {
        run_settled(&mut s, &st.text).await;
    }

    // 2. Typed inserts (the describe's type): base64 in the documents.
    let types = vec![
        ResultColumn::new("_id", ""),
        ResultColumn::new("cat", "string"),
        ResultColumn::new("e", "vector(3, base64)"),
    ];
    let ins = d
        .insert_script_typed(
            &coll("docs"),
            &types,
            &[
                vec![json!("1"), json!("a"), json!([1.0, 2.0, 3.0])],
                vec![json!("2"), json!("b"), json!([3.0, 2.0, 1.0])],
                vec![json!("3"), json!("a"), json!([1.0, 2.0, 2.5])],
            ],
        )
        .unwrap();
    run_settled(&mut s, &ins).await;
    let out = run(&mut s, &raw("docs", "1")).await;
    assert_eq!(out.results[0].rows, [[json!("AACAPwAAAEAAAEBA")]]);
    // What the server's own encoder writes.
    let out = run(&mut s, "SELECT RAW ENCODE_VECTOR([1, 2, 3], false)").await;
    assert_eq!(out.results[0].rows, [[json!("AACAPwAAAEAAAEBA")]]);
    run_settled(&mut s, &format!("BUILD INDEX ON {}(`e_vix`)", ks("docs"))).await;
    for _ in 0..120 {
        let out = run(
            &mut s,
            &format!("SELECT RAW i.state FROM system:indexes AS i WHERE i.bucket_id = \"{B}\" AND i.name = \"e_vix\""),
        )
        .await;
        if out.results[0].rows == [[json!("online")]] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // 3. Read back: the numbers, marked with the field's form.
    let out = run(
        &mut s,
        &format!(
            "SELECT META(d).id AS _id, d.e FROM {} AS d ORDER BY META(d).id",
            ks("docs")
        ),
    )
    .await;
    let r = &out.results[0];
    let at = r.columns.iter().position(|c| c.name == "e").unwrap();
    assert_eq!(r.columns[at].type_name, "vector(3, base64)");
    assert_eq!(
        r.columns[at].vector,
        Some(VectorMeta::dense(Some(3), VectorElement::F32))
    );
    assert_eq!(r.rows[0][at], json!("[1,2,3]"));
    assert_eq!(r.rows[2][at], json!("[1,2,2.5]"));
    let result_types = r.columns.clone();

    // 4. Describe: `vector(3, base64)`; 5. the index by its field, with
    // its vector; unchanged against itself and the designer's.
    let schema = s.database_schema().await.unwrap();
    let tb = schema.iter().find(|x| x.name == "docs").unwrap().clone();
    assert_eq!(
        tb.columns.iter().find(|c| c.name == "e").unwrap().data_type,
        "vector(3, base64)"
    );
    assert!(tb.columns.iter().all(|c| !c.name.contains('(')));
    let ix = tb.indexes.iter().find(|i| i.name == "e_vix").unwrap();
    assert_eq!(ix.columns, ["e"]);
    assert_eq!(ix.vector.as_ref().unwrap().metric.as_deref(), Some("l2"));
    let mut designer = tb.clone();
    designer.indexes.retain(|i| i.vector.is_none());
    designer.indexes.extend(t.indexes.iter().cloned());
    for new in [designer, tb.clone()] {
        let same = d
            .sync_script(&[TableChange::Alter {
                old: tb.clone(),
                new,
            }])
            .unwrap();
        assert!(same.statements.is_empty(), "{:?}", same.statements);
    }
    let usage = s.index_usage(&coll("docs")).await.unwrap().unwrap();
    let u = usage.indexes.iter().find(|i| i.name == "e_vix").unwrap();
    assert_eq!(u.vector, ix.vector);

    // 6. A similarity query through the index.
    let out = run(
        &mut s,
        &format!("SELECT RAW META(d).id FROM {} AS d ORDER BY APPROX_VECTOR_DISTANCE(DECODE_VECTOR(d.e, false), [1, 2, 3], \"L2\") LIMIT 2", ks("docs")),
    )
    .await;
    assert_eq!(out.results[0].rows, [[json!("1")], [json!("3")]]);

    // 7. A typed update with the result's types: base64 again.
    let upd = d
        .update_script_typed(
            &coll("docs"),
            &[RowChange {
                key: vec![("_id".into(), json!("2"))],
                set: vec![("e".into(), json!([0.5, 0.25, 1.0]))],
                ..Default::default()
            }],
            &result_types,
        )
        .unwrap();
    run(&mut s, &upd).await;
    let out = run(
        &mut s,
        &format!(
            "SELECT RAW DECODE_VECTOR(d.e, false) FROM {} AS d USE KEYS \"2\"",
            ks("docs")
        ),
    )
    .await;
    assert_eq!(out.results[0].rows, [[json!("[0.5,0.25,1]")]]);

    // 8. Transfers: the read gives the numbers and says the form; a load
    // into another collection writes base64 (by the source's type, its
    // index not made yet), and one into a collection whose index decodes
    // base64 writes it from a pgvector-like source.
    let sink = Arc::new(Mutex::new(Collect::default()));
    let read = ReadSpec {
        table: coll("docs"),
        columns: None,
        filter: None,
    };
    s.read_batches(&read, sink.clone()).await.unwrap();
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let at = got.columns.iter().position(|c| c.name == "e").unwrap();
    assert_eq!(got.columns[at].type_name, "vector(3, base64)");
    let mut cells: Vec<String> = got
        .rows
        .iter()
        .map(|r| match &r[at] {
            Cell::Json(j) => j.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    cells.sort();
    assert_eq!(cells, ["[0.5,0.25,1]", "[1,2,2.5]", "[1,2,3]"]);
    let copy = TableSchema {
        name: "copy".into(),
        indexes: Vec::new(),
        ..t.clone()
    };
    for st in d.split_script(&d.table_ddl(&copy, ALL).unwrap()) {
        run_settled(&mut s, &st.text).await;
    }
    let load = LoadSpec {
        table: coll("copy"),
        columns: got.columns.iter().map(|c| c.name.clone()).collect(),
        table_lock: false,
        keep_identity: true,
        commit_rows: 1000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let n = s
        .bulk_load(
            &load,
            &got.columns,
            &mut Batches(
                vec![RowBatch {
                    rows: got.rows.clone(),
                    bytes: 0,
                }]
                .into(),
            ),
            &|_| {},
        )
        .await
        .unwrap();
    assert_eq!(n, 3);
    let out = run_settled(&mut s, &raw("copy", "1")).await;
    assert_eq!(out.results[0].rows, [[json!("AACAPwAAAEAAAEBA")]]);
    // From pgvector (`vector(3)`, text cells) into the indexed collection.
    let pg_cols = vec![
        TransferColumn {
            name: "id".into(),
            type_name: "text".into(),
            nullable: false,
        },
        TransferColumn {
            name: "e".into(),
            type_name: "vector(3)".into(),
            nullable: true,
        },
    ];
    let load = LoadSpec {
        table: coll("docs"),
        columns: vec!["id".into(), "e".into()],
        ..load
    };
    s.bulk_load(
        &load,
        &pg_cols,
        &mut Batches(
            vec![RowBatch {
                rows: vec![vec![Cell::Text("p1".into()), Cell::Text("[1,2,3]".into())]],
                bytes: 0,
            }]
            .into(),
        ),
        &|_| {},
    )
    .await
    .unwrap();
    let out = run_settled(&mut s, &raw("docs", "p1")).await;
    assert_eq!(out.results[0].rows, [[json!("AACAPwAAAEAAAEBA")]]);

    drop(s);
    admin.drop_database(B).await.expect("drop bucket");
}

/// Search (FTS) vector indexes: made by the driver's DDL through the Search
/// service, read into the schema with their vector, compared, changed,
/// copied to another collection, and their fields marked in results.
#[tokio::test]
#[ignore]
async fn couchbase_search_vector_indexes() {
    const B: &str = "dbine_vec_fts";
    let Some(c) = cfg_of("DBINE_TEST_COUCHBASE_FTS", "") else {
        return;
    };
    let d = dbine_driver_couchbase::drivers().remove(0);
    let mut admin = d.connect(&c, None).await.expect("connect");
    let _ = admin.drop_database(B).await;
    admin.create_database(B).await.expect("bucket");
    let mut s = d
        .connect(&cfg_of("DBINE_TEST_COUCHBASE_FTS", B).unwrap(), Some(B))
        .await
        .unwrap();
    let coll = |name: &str| ObjectRef {
        kind: "collection".into(),
        schema: Some(format!("{B}._default")),
        name: name.into(),
    };
    let ks = format!("`{B}`.`_default`.`docs`");

    // 1. The collection and its Search vector index, by the driver's DDL.
    let search = IndexDef {
        name: "vs".into(),
        columns: vec!["emb".into()],
        kind: Some("SEARCH".into()),
        vector: Some(VectorIndex {
            metric: Some("ip".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let t = TableSchema {
        kind: "collection".into(),
        schema: Some(format!("{B}._default")),
        name: "docs".into(),
        columns: vec![col("cat", "string"), col("emb", "vector(3)")],
        indexes: vec![search],
        options: [("primary_index".to_string(), "true".to_string())].into(),
        ..Default::default()
    };
    let ddl = d.table_ddl(&t, ALL).unwrap();
    assert!(ddl.contains("CREATE SEARCH INDEX `vs`"), "{ddl}");
    for st in d.split_script(&ddl) {
        run_settled(&mut s, &st.text).await;
    }

    // 2. Rows.
    for (k, v) in [("1", "[1,0,0]"), ("2", "[0,1,0]"), ("3", "[0.9,0.1,0]")] {
        run_settled(
            &mut s,
            &format!(
                "UPSERT INTO {ks} (KEY, VALUE) VALUES (\"{k}\", {{\"cat\": \"a\", \"emb\": {v}}})"
            ),
        )
        .await;
    }

    // 3. Results mark the field the Search index declares.
    let out = run(
        &mut s,
        &format!("SELECT d.emb FROM {ks} AS d ORDER BY META(d).id"),
    )
    .await;
    let r = &out.results[0];
    assert_eq!(
        r.columns[0].vector,
        Some(VectorMeta::dense(Some(3), VectorElement::F32))
    );
    assert_eq!(r.rows[2][0], json!("[0.9,0.1,0]"));

    // 4 / 5. Describe: the index with its vector, the field's dimension.
    let schema = s.database_schema().await.unwrap();
    let tb = schema.iter().find(|x| x.name == "docs").unwrap().clone();
    let ix = tb
        .indexes
        .iter()
        .find(|i| i.name == "vs")
        .expect("Search index");
    assert_eq!(ix.kind.as_deref(), Some("SEARCH"));
    assert_eq!(ix.columns, ["emb"]);
    assert_eq!(ix.vector.as_ref().unwrap().metric.as_deref(), Some("ip"));
    assert_eq!(
        tb.columns
            .iter()
            .find(|c| c.name == "emb")
            .unwrap()
            .data_type,
        "vector(3)"
    );
    let usage = s.index_usage(&coll("docs")).await.unwrap().unwrap();
    let u = usage
        .indexes
        .iter()
        .find(|u| u.name == "vs" && u.kind == "SEARCH VECTOR")
        .expect("listed");
    assert_eq!(u.vector, ix.vector);
    let same = d
        .sync_script(&[TableChange::Alter {
            old: tb.clone(),
            new: tb.clone(),
        }])
        .unwrap();
    assert!(same.statements.is_empty(), "{:?}", same.statements);

    // 6. A similarity query through the Search service, once it has the
    // documents (a scoped Search index goes by its full name there).
    let knn = format!("SELECT RAW META(d).id FROM {ks} AS d WHERE SEARCH(d, {{\"query\": {{\"match_none\": {{}}}}, \"knn\": [{{\"field\": \"emb\", \"vector\": [1, 0, 0], \"k\": 2}}]}}, {{\"index\": \"{B}._default.vs\"}})");
    let mut ids = Vec::new();
    for _ in 0..120 {
        let mut out = QueryOutcome::default();
        if s.execute(&knn, 10, &mut out).await.is_ok() {
            ids = out.results[0].rows.iter().map(|r| r[0].clone()).collect();
            if ids.len() == 2 {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    ids.sort_by_key(|v| v.to_string());
    assert_eq!(ids, [json!("1"), json!("3")]);

    // 7. A change: dropped and made again with the new distance.
    let mut changed = tb.clone();
    changed
        .indexes
        .iter_mut()
        .find(|i| i.name == "vs")
        .unwrap()
        .vector
        .as_mut()
        .unwrap()
        .metric = Some("l2".into());
    let sync = d
        .sync_script(&[TableChange::Alter {
            old: tb.clone(),
            new: changed,
        }])
        .unwrap();
    assert_eq!(
        sync.statements[0],
        format!("DROP SEARCH INDEX `vs` ON {ks};")
    );
    for st in &sync.statements {
        run_settled(&mut s, st).await;
    }
    let schema = s.database_schema().await.unwrap();
    let tb = schema.iter().find(|x| x.name == "docs").unwrap().clone();
    let ix = tb.indexes.iter().find(|i| i.name == "vs").unwrap();
    assert_eq!(ix.vector.as_ref().unwrap().metric.as_deref(), Some("l2"));

    // 8. Copied to another collection (as a clone does, renamed).
    let mut copy = tb.clone();
    copy.name = "copy".into();
    copy.indexes.retain(|i| i.name == "vs");
    copy.indexes[0].name = "vs_copy".into();
    for st in d.split_script(&d.table_ddl(&copy, ALL).unwrap()) {
        run_settled(&mut s, &st.text).await;
    }
    let schema = s.database_schema().await.unwrap();
    let cp = schema.iter().find(|x| x.name == "copy").unwrap();
    let ix = cp
        .indexes
        .iter()
        .find(|i| i.name == "vs_copy")
        .expect("copied");
    assert_eq!(ix.vector.as_ref().unwrap().metric.as_deref(), Some("l2"));

    // The explorer's "Eliminar índice": DROP SEARCH INDEX.
    run(
        &mut s,
        &format!("DROP SEARCH INDEX `vs_copy` ON `{B}`.`_default`.`copy`"),
    )
    .await;
    let schema = s.database_schema().await.unwrap();
    let cp = schema.iter().find(|x| x.name == "copy").unwrap();
    assert!(cp.indexes.iter().all(|i| i.name != "vs_copy"));

    drop(s);
    run(
        &mut admin,
        &format!("DROP SEARCH INDEX IF EXISTS `vs` ON `{B}`.`_default`.`docs`"),
    )
    .await;
    admin.drop_database(B).await.expect("drop bucket");
}
