//! Vectors (embeddings) against real servers, ignored by default. Each test
//! reads its URL (`postgres://user:pass@host:port/db`) and is skipped
//! without it:
//!
//! ```sh
//! DBINE_TEST_PGVECTOR_URL=postgres://postgres:pw@localhost:27200/postgres \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//! DBINE_TEST_CRATEDB_URL=postgres://crate@localhost:25021/doc \
//!   cargo test -p dbine-driver-postgres --test vectors -- --ignored --nocapture --test-threads 1
//! ```
//!
//! Others: `DBINE_TEST_TIMESCALE_HA_URL` (pgvector + pgvectorscale),
//! `DBINE_TEST_YUGABYTE_URL`, `DBINE_TEST_RISINGWAVE_URL`,
//! `DBINE_TEST_CLOUDBERRY_URL`, `DBINE_TEST_OPENGAUSS_URL`.
//!
//! Each one makes a table with vector columns and a vector index from a
//! `TableSchema` (as the designer does), inserts rows with
//! `insert_script`, reads them back (vector columns marked, cells exact),
//! checks the describe and the index as read, runs a similarity query and
//! an `update_script`, and moves the rows with the transfer. Everything it
//! creates (a database, or a schema on CrateDB) is dropped first and at
//! the end, also when it fails.

use dbine_driver::transfer::{
    BatchSink, BatchSource, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::vector::metric;
use dbine_driver::{
    ColumnDef, ConnectionConfig, DdlParts, Driver, IndexDef, KeyDef, ObjectRef, QueryOutcome,
    RowChange, Session, TableChange, TableSchema, VectorElement, VectorIndex, VectorMeta,
};
use futures::FutureExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// The database (a schema on CrateDB) every test makes and drops.
const DB: &str = "dbine_vec_t";

fn parse_url(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport
        .rsplit_once(':')
        .map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

async fn try_run(s: &mut Box<dyn Session>, sql: &str) -> Result<QueryOutcome, String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10_000, &mut out)
        .await
        .map_err(|e| format!("{sql}: {e}"))?;
    match out.error.take() {
        Some(e) => Err(format!("{sql}: {e}")),
        None => Ok(out),
    }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    try_run(s, sql).await.unwrap_or_else(|e| panic!("{e}"))
}

/// The cells of the first result as text (`None`: NULL).
async fn cells(s: &mut Box<dyn Session>, sql: &str) -> Vec<Vec<Option<String>>> {
    let out = run(s, sql).await;
    out.results
        .iter()
        .find(|r| !r.columns.is_empty())
        .map(|r| {
            r.rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|c| match c {
                            Value::Null => None,
                            Value::String(s) => Some(s.clone()),
                            o => Some(o.to_string()),
                        })
                        .collect()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn col(name: &str, ty: &str, nullable: bool) -> ColumnDef {
    ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        nullable,
        ..Default::default()
    }
}

fn vindex(
    name: &str,
    cols: &[&str],
    alg: Option<&str>,
    m: &str,
    params: &[(&str, &str)],
) -> IndexDef {
    IndexDef {
        name: name.into(),
        columns: cols.iter().map(|c| c.to_string()).collect(),
        vector: Some(VectorIndex {
            algorithm: alg.map(Into::into),
            metric: Some(m.into()),
            params: params
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }),
        ..Default::default()
    }
}

fn table(schema: &str, name: &str, columns: Vec<ColumnDef>, indexes: Vec<IndexDef>) -> TableSchema {
    TableSchema {
        kind: "table".into(),
        schema: Some(schema.into()),
        name: name.into(),
        primary_key: Some(KeyDef {
            name: None,
            columns: vec!["id".into()],
        }),
        columns,
        indexes,
        ..Default::default()
    }
}

fn obj(schema: &str, name: &str) -> ObjectRef {
    ObjectRef {
        kind: "table".into(),
        schema: Some(schema.into()),
        name: name.into(),
    }
}

const ALL: DdlParts = DdlParts {
    drop: false,
    if_exists: false,
    create: true,
    indexes: true,
    foreign_keys: true,
};

// ------------------------------------------------------------- transfer

struct Collect(Arc<Mutex<(Vec<TransferColumn>, Vec<RowBatch>)>>);
impl BatchSink for Collect {
    fn begin(&mut self, cols: &[TransferColumn]) -> std::io::Result<()> {
        self.0.lock().unwrap().0 = cols.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> std::io::Result<()> {
        self.0.lock().unwrap().1.push(b);
        Ok(())
    }
}

struct Replay(Vec<RowBatch>);
#[dbine_driver::async_trait]
impl BatchSource for Replay {
    async fn next(&mut self) -> Option<RowBatch> {
        (!self.0.is_empty()).then(|| self.0.remove(0))
    }
}

/// What the transfer engine does for one table: the source's
/// `read_batches`, then the target's `bulk_load`, or its `insert_script`
/// with each cell as JSON where it has no bulk load (dbine-transfer's
/// `copy.rs`). The columns the source announced.
async fn transfer(
    src: &mut Box<dyn Session>,
    from: &ObjectRef,
    dst_driver: &dyn Driver,
    dst: &mut Box<dyn Session>,
    to: &ObjectRef,
    columns: &[&str],
) -> Vec<TransferColumn> {
    let columns: Vec<String> = columns.iter().map(|c| c.to_string()).collect();
    let got = Arc::new(Mutex::new((Vec::new(), Vec::new())));
    let sink: dbine_driver::BatchSinkRef = Arc::new(Mutex::new(Collect(got.clone())));
    src.read_batches(
        &ReadSpec {
            table: from.clone(),
            columns: Some(columns.clone()),
            filter: None,
        },
        sink,
    )
    .await
    .expect("read_batches");
    let (announced, batches) = std::mem::take(&mut *got.lock().unwrap());
    if dst_driver.supports_bulk_load() {
        let spec = LoadSpec {
            table: to.clone(),
            columns,
            table_lock: false,
            keep_identity: true,
            commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
            commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
        };
        dst.bulk_load(&spec, &announced, &mut Replay(batches), &|_| {})
            .await
            .expect("bulk_load");
    } else {
        for b in batches {
            let rows: Vec<Vec<Value>> = b
                .rows
                .iter()
                .map(|r| r.iter().map(|c| c.to_json()).collect())
                .collect();
            let sql = dst_driver.insert_script(to, &columns, &rows).unwrap();
            run(dst, &sql).await;
        }
    }
    announced
}

// --------------------------------------------------------------- harness

/// Runs `body`, then `cleanup`, also when `body` panics.
async fn guarded<F, C>(body: F, cleanup: C)
where
    F: std::future::Future<Output = ()>,
    C: std::future::Future<Output = ()>,
{
    let res = std::panic::AssertUnwindSafe(body).catch_unwind().await;
    cleanup.await;
    if let Err(p) = res {
        std::panic::resume_unwind(p);
    }
}

/// A fresh database `DB` on the server of `cfg` (dropped first), and a
/// session on it.
async fn fresh_database(
    d: &dyn Driver,
    cfg: &ConnectionConfig,
    cockroach: bool,
) -> Box<dyn Session> {
    drop_database(d, cfg, cockroach).await;
    let mut admin = d.connect(cfg, None).await.expect("connect");
    run(&mut admin, &format!("CREATE DATABASE {DB}")).await;
    drop(admin);
    d.connect(cfg, Some(DB))
        .await
        .expect("connect to the test database")
}

async fn drop_database(d: &dyn Driver, cfg: &ConnectionConfig, cockroach: bool) {
    let Ok(mut admin) = d.connect(cfg, None).await else {
        return;
    };
    let sql = if cockroach {
        format!("DROP DATABASE IF EXISTS {DB} CASCADE")
    } else {
        format!("DROP DATABASE IF EXISTS {DB} WITH (FORCE)")
    };
    if let Err(e) = try_run(&mut admin, &sql).await {
        // Servers before PostgreSQL 13 (or forks) without WITH (FORCE).
        let plain = format!("DROP DATABASE IF EXISTS {DB}");
        if let Err(e2) = try_run(&mut admin, &plain).await {
            eprintln!("cleanup: {e} / {e2}");
        }
    }
}

fn index<'a>(t: &'a TableSchema, name: &str) -> &'a IndexDef {
    t.indexes
        .iter()
        .find(|i| i.name == name)
        .unwrap_or_else(|| panic!("index {name} not listed: {:?}", t.indexes))
}

fn column<'a>(t: &'a TableSchema, name: &str) -> &'a ColumnDef {
    t.columns.iter().find(|c| c.name == name).unwrap()
}

async fn read_table(s: &mut Box<dyn Session>, schema: &str, name: &str) -> TableSchema {
    s.database_schema()
        .await
        .expect("database_schema")
        .into_iter()
        .find(|t| t.schema.as_deref() == Some(schema) && t.name == name)
        .unwrap_or_else(|| panic!("{schema}.{name} not described"))
}

fn assert_vector_index(ix: &IndexDef, alg: &str, m: &str, params: &[(&str, &str)]) {
    let v = ix
        .vector
        .as_ref()
        .unwrap_or_else(|| panic!("{} isn't a vector index: {ix:?}", ix.name));
    assert_eq!(v.algorithm.as_deref(), Some(alg), "{ix:?}");
    assert_eq!(v.metric.as_deref(), Some(m), "{ix:?}");
    let want: BTreeMap<String, String> = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    assert_eq!(v.params, want, "{ix:?}");
    assert_eq!(ix.kind.as_deref(), Some(alg), "{ix:?}");
}

/// The sync of a table with itself changes nothing; with one vector
/// index's metric changed, it makes that index again with the new one.
async fn sync_round_trip(
    d: &dyn Driver,
    s: &mut Box<dyn Session>,
    t: &TableSchema,
    ix: &str,
    to: &str,
    opclass: &str,
) {
    let same = d
        .sync_script(&[TableChange::Alter {
            old: t.clone(),
            new: t.clone(),
        }])
        .unwrap();
    assert!(same.statements.is_empty(), "{:?}", same.statements);
    let mut changed = t.clone();
    changed
        .indexes
        .iter_mut()
        .find(|i| i.name == ix)
        .unwrap()
        .vector
        .as_mut()
        .unwrap()
        .metric = Some(to.into());
    let script = d
        .sync_script(&[TableChange::Alter {
            old: t.clone(),
            new: changed,
        }])
        .unwrap();
    let all = script.statements.join("\n");
    assert!(all.contains("DROP INDEX"), "{all}");
    assert!(all.contains(opclass), "{all}");
    for st in &script.statements {
        run(s, st).await;
    }
}

// ------------------------------------------------------------- pgvector

/// pgvector's whole round trip on a server where `CREATE EXTENSION
/// vector` works; `extra` adds the variant's own index (DiskANN…).
async fn pgvector_round_trip(id: &str, env: &str, extra: Option<(IndexDef, &str, &str)>) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let cfg = parse_url(id, &url);
    let d = driver(id);
    assert!(d.capabilities().vectors);
    let spec = d.designer().unwrap();
    assert!(spec.vector_indexes.is_some());
    assert!(spec.data_types.contains(&"vector(1536)"));
    let mut s = fresh_database(&*d, &cfg, false).await;
    let body = async {
        run(&mut s, "CREATE EXTENSION IF NOT EXISTS vector").await;
        let extra_ext = extra
            .as_ref()
            .map(|(_, ext, _)| *ext)
            .filter(|e| !e.is_empty());
        if let Some(ext) = extra_ext {
            run(
                &mut s,
                &format!("CREATE EXTENSION IF NOT EXISTS {ext} CASCADE"),
            )
            .await;
        }
        // 1. The table, as the designer makes it.
        let mut indexes = vec![
            vindex(
                "ix_v",
                &["v"],
                Some("hnsw"),
                metric::COSINE,
                &[("m", "16"), ("ef_construction", "64")],
            ),
            vindex(
                "ix_h",
                &["h"],
                Some("ivfflat"),
                metric::L2,
                &[("lists", "1")],
            ),
            vindex("ix_s", &["s"], None, metric::IP, &[]),
            vindex("ix_b", &["b"], Some("hnsw"), metric::HAMMING, &[]),
        ];
        if let Some((ix, _, _)) = &extra {
            indexes.push(ix.clone());
        }
        let t = table(
            "public",
            "items",
            vec![
                col("id", "integer", false),
                col("v", "vector(3)", true),
                col("h", "halfvec(3)", true),
                col("s", "sparsevec(5)", true),
                col("b", "bit(4)", true),
            ],
            indexes,
        );
        let ddl = d.table_ddl(&t, ALL).unwrap();
        eprintln!("{id}: {ddl}");
        for st in ddl.split(";\n") {
            run(&mut s, st).await;
        }
        // 2. Rows with vectors as arrays.
        let target = obj("public", "items");
        let cols: Vec<String> = ["id", "v", "h", "s", "b"].map(String::from).to_vec();
        let rows = vec![
            vec![
                json!(1),
                json!([1, 2.5, -3]),
                json!([1, 2, 3]),
                json!("{1:1.5,3:2}/5"),
                json!("1010"),
            ],
            vec![
                json!(2),
                json!([0.5, 0, 1]),
                json!([0.5, 0, 1]),
                json!("{2:1}/5"),
                json!("0110"),
            ],
            vec![
                json!(3),
                json!([-1, -1, -1]),
                json!([0, 0, 1]),
                json!("{5:3}/5"),
                json!("1111"),
            ],
        ];
        run(&mut s, &d.insert_script(&target, &cols, &rows).unwrap()).await;
        // 3. Read back: the columns say they hold vectors, the cells are whole.
        let out = run(
            &mut s,
            "SELECT id, v, h, s, b FROM public.items ORDER BY id",
        )
        .await;
        let r = &out.results[0];
        assert_eq!(r.columns[1].type_name, "vector(3)");
        assert_eq!(
            r.columns[1].vector,
            Some(VectorMeta::dense(Some(3), VectorElement::F32))
        );
        assert_eq!(r.columns[2].type_name, "halfvec(3)");
        assert_eq!(
            r.columns[2].vector,
            Some(VectorMeta::dense(Some(3), VectorElement::F16))
        );
        assert_eq!(
            r.columns[3].vector,
            Some(VectorMeta::sparse(Some(5), VectorElement::F32))
        );
        assert_eq!(r.columns[4].vector, None, "bit(n) is a bit string");
        assert_eq!(r.rows[0][1], json!("[1,2.5,-3]"));
        assert_eq!(r.rows[0][2], json!("[1,2,3]"));
        assert_eq!(r.rows[0][3], json!("{1:1.5,3:2}/5"));
        assert_eq!(r.rows[1][1], json!("[0.5,0,1]"));
        // Table browsing too.
        let browse = s.browse_query(&target, 100);
        let out = run(&mut s, &browse).await;
        assert!(out.results[0]
            .columns
            .iter()
            .any(|c| c.name == "v" && c.vector.is_some()));
        // 4. The describe keeps the dimensions.
        let read = read_table(&mut s, "public", "items").await;
        assert_eq!(column(&read, "v").data_type, "vector(3)");
        assert_eq!(column(&read, "h").data_type, "halfvec(3)");
        assert_eq!(column(&read, "s").data_type, "sparsevec(5)");
        assert_eq!(column(&read, "b").data_type, "bit(4)");
        // 5. The indexes, as vector indexes.
        assert_vector_index(
            index(&read, "ix_v"),
            "hnsw",
            metric::COSINE,
            &[("m", "16"), ("ef_construction", "64")],
        );
        assert_vector_index(
            index(&read, "ix_h"),
            "ivfflat",
            metric::L2,
            &[("lists", "1")],
        );
        let default_alg = spec.vector_indexes.as_ref().unwrap().algorithms[0];
        assert_vector_index(index(&read, "ix_s"), default_alg, metric::IP, &[]);
        assert_vector_index(index(&read, "ix_b"), "hnsw", metric::HAMMING, &[]);
        if let Some((ix, _, _)) = &extra {
            let got = index(&read, &ix.name);
            let want = ix.vector.as_ref().unwrap();
            assert_eq!(got.vector.as_ref().unwrap().algorithm, want.algorithm);
            assert_eq!(got.vector.as_ref().unwrap().metric, want.metric);
            assert_eq!(got.vector.as_ref().unwrap().params, want.params);
        }
        // The read table's DDL makes the same indexes.
        let again = d
            .table_ddl(
                &read,
                DdlParts {
                    indexes: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            again.contains("USING hnsw (\"v\" vector_cosine_ops) WITH (ef_construction=64, m=16)"),
            "{again}"
        );
        assert!(
            again.contains("USING ivfflat (\"h\" halfvec_l2_ops) WITH (lists=1)"),
            "{again}"
        );
        assert!(again.contains("(\"s\" sparsevec_ip_ops)"), "{again}");
        assert!(
            again.contains("USING hnsw (\"b\" bit_hamming_ops)"),
            "{again}"
        );
        // Sync: unchanged, then a metric changed.
        sync_round_trip(&*d, &mut s, &read, "ix_v", metric::L2, "vector_l2_ops").await;
        let read = read_table(&mut s, "public", "items").await;
        assert_vector_index(
            index(&read, "ix_v"),
            "hnsw",
            metric::L2,
            &[("m", "16"), ("ef_construction", "64")],
        );
        // 6. A similarity query.
        let near = cells(
            &mut s,
            "SELECT id FROM public.items ORDER BY v <-> '[1,2.5,-3]' LIMIT 1",
        )
        .await;
        assert_eq!(near, [[Some("1".to_string())]]);
        let near = cells(
            &mut s,
            "SELECT id FROM public.items ORDER BY s <#> '{1:1}/5' LIMIT 1",
        )
        .await;
        assert_eq!(near, [[Some("1".to_string())]]);
        // 7. An edit.
        let change = RowChange {
            key: vec![("id".into(), json!(2))],
            set: vec![
                ("v".into(), json!([9, 8, 7.5])),
                ("h".into(), json!([1, 1, 1])),
            ],
            row: Vec::new(),
        };
        run(&mut s, &d.update_script(&target, &[change]).unwrap()).await;
        let got = cells(&mut s, "SELECT v, h FROM public.items WHERE id = 2").await;
        assert_eq!(got, [[Some("[9,8,7.5]".into()), Some("[1,1,1]".into())]]);
        run(
            &mut s,
            &d.delete_script(&target, &[vec![("id".into(), json!(3))]])
                .unwrap(),
        )
        .await;
        // 8. A transfer to a copy of the table.
        let mut copy = t.clone();
        copy.name = "items_copy".into();
        copy.indexes.clear();
        run(&mut s, &d.table_ddl(&copy, ALL).unwrap()).await;
        let mut s2 = d.connect(&cfg, Some(DB)).await.unwrap();
        let announced = transfer(
            &mut s,
            &target,
            &*d,
            &mut s2,
            &obj("public", "items_copy"),
            &["id", "v", "h", "s", "b"],
        )
        .await;
        assert_eq!(announced[1].type_name, "vector(3)");
        let diff = cells(&mut s, "SELECT count(*) FROM (SELECT id, v::text, h::text, s::text, b::text FROM public.items EXCEPT SELECT id, v::text, h::text, s::text, b::text FROM public.items_copy) x").await;
        assert_eq!(diff, [[Some("0".to_string())]]);
        let n = cells(&mut s, "SELECT count(*) FROM public.items_copy").await;
        assert_eq!(n, [[Some("2".to_string())]]);
        // A dense vector into a sparsevec column: its notation.
        run(
            &mut s,
            "CREATE TABLE public.dense_to_sparse (id integer PRIMARY KEY, s sparsevec(3))",
        )
        .await;
        run(
            &mut s,
            "CREATE TABLE public.dense (id integer PRIMARY KEY, s vector(3))",
        )
        .await;
        run(&mut s, "INSERT INTO public.dense VALUES (1, '[0,2.5,0]')").await;
        transfer(
            &mut s,
            &obj("public", "dense"),
            &*d,
            &mut s2,
            &obj("public", "dense_to_sparse"),
            &["id", "s"],
        )
        .await;
        let got = cells(&mut s, "SELECT s::text FROM public.dense_to_sparse").await;
        assert_eq!(got, [[Some("{2:2.5}/3".to_string())]]);
        drop(s2);
    };
    let cleanup = async {
        drop_database(&*d, &cfg, false).await;
    };
    guarded(body, cleanup).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn pgvector_vectors() {
    pgvector_round_trip("postgres", "DBINE_TEST_PGVECTOR_URL", None).await;
}

/// TimescaleDB with pgvectorscale: DiskANN too.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn timescale_vectors_with_diskann() {
    let disk = vindex(
        "ix_disk",
        &["v"],
        Some("diskann"),
        metric::L2,
        &[("num_neighbors", "32"), ("search_list_size", "50")],
    );
    pgvector_round_trip(
        "timescaledb",
        "DBINE_TEST_TIMESCALE_HA_URL",
        Some((disk, "vectorscale", "")),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cloudberry_vectors() {
    pgvector_round_trip("cloudberry", "DBINE_TEST_CLOUDBERRY_URL", None).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn opengauss_vectors() {
    pgvector_round_trip("opengauss", "DBINE_TEST_OPENGAUSS_URL", None).await;
}
