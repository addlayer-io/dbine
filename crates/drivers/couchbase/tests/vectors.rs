//! GSI vector indexes against Couchbase Server Enterprise 8.0:
//!
//! ```sh
//! docker start dbine-test-couchbase-ee   # 25991 / 25993, Administrator / secreto1
//! DBINE_TEST_COUCHBASE_EE_URL=http://localhost:25993 DBINE_TEST_COUCHBASE_EE_MGMT_PORT=25991 \
//!   cargo test -p dbine-driver-couchbase --test vectors -- --ignored --nocapture
//! ```
//!
//! The test's bucket is dropped first and at the end. The container has no
//! Search service: Search (FTS) vector indexes are covered by unit tests.

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
    let url =
        reqwest::Url::parse(&std::env::var("DBINE_TEST_COUCHBASE_EE_URL").ok()?).expect("URL");
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
        std::env::var("DBINE_TEST_COUCHBASE_EE_MGMT_PORT").unwrap_or_else(|_| "8091".into()),
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
    assert_eq!(hyper.columns, ["emb", "`cat`"]);
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

    drop(s);
    admin.drop_database(BUCKET).await.expect("drop bucket");
}
