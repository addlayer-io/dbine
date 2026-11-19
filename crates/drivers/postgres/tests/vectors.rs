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
//! `transfers_with_pgvector_siblings` moves rows between pgvector and each
//! of TimescaleDB, YugabyteDB and RisingWave whose URL is set.
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
    ResultColumn, RowChange, RowSink, RowSinkRef, Session, StatementResult, TableChange,
    TableSchema, VectorElement, VectorIndex, VectorMeta,
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

/// A run whose rows go to a sink (exports, the results store, compares):
/// the columns the sink began the first result with, and its rows.
type Rows = (Vec<ResultColumn>, Vec<Vec<Value>>);
struct Streamed(Arc<Mutex<Rows>>);
impl RowSink for Streamed {
    fn begin(&mut self, index: usize, columns: &[ResultColumn]) -> std::io::Result<()> {
        if index == 0 {
            self.0.lock().unwrap().0 = columns.to_vec();
        }
        Ok(())
    }
    fn row(&mut self, index: usize, row: &[Value]) -> std::io::Result<()> {
        if index == 0 {
            self.0.lock().unwrap().1.push(row.to_vec());
        }
        Ok(())
    }
}

async fn streamed(s: &mut Box<dyn Session>, sql: &str) -> Rows {
    let got = Arc::new(Mutex::new((Vec::new(), Vec::new())));
    let mut out = QueryOutcome {
        sink: Some(RowSinkRef(Arc::new(Mutex::new(Streamed(got.clone()))))),
        ..Default::default()
    };
    s.execute(sql, 10_000, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    assert!(out.sink_error.is_none(), "{sql}: {:?}", out.sink_error);
    let got = std::mem::take(&mut *got.lock().unwrap());
    got
}

/// The same query streamed to a sink gives the sink what the grid got:
/// the columns' types and vectors, and the cells as JSON arrays.
async fn same_when_streamed(s: &mut Box<dyn Session>, sql: &str, grid: &StatementResult) {
    let (cols, rows) = streamed(s, sql).await;
    let shape = |c: &[ResultColumn]| {
        c.iter()
            .map(|c| (c.name.clone(), c.type_name.clone(), c.vector.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(shape(&cols), shape(&grid.columns), "{sql}");
    assert_eq!(rows, grid.rows, "{sql}");
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
    fresh_named(d, cfg, cockroach, DB).await
}

async fn fresh_named(
    d: &dyn Driver,
    cfg: &ConnectionConfig,
    cockroach: bool,
    db: &str,
) -> Box<dyn Session> {
    drop_named(d, cfg, cockroach, db).await;
    let mut admin = d.connect(cfg, None).await.expect("connect");
    run(&mut admin, &format!("CREATE DATABASE {db}")).await;
    drop(admin);
    d.connect(cfg, Some(db))
        .await
        .expect("connect to the test database")
}

async fn drop_database(d: &dyn Driver, cfg: &ConnectionConfig, cockroach: bool) {
    drop_named(d, cfg, cockroach, DB).await
}

async fn drop_named(d: &dyn Driver, cfg: &ConnectionConfig, cockroach: bool, db: &str) {
    let Ok(mut admin) = d.connect(cfg, None).await else {
        return;
    };
    let sql = if cockroach {
        format!("DROP DATABASE IF EXISTS {db} CASCADE")
    } else {
        format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)")
    };
    if let Err(e) = try_run(&mut admin, &sql).await {
        // Servers before PostgreSQL 13 (or forks) without WITH (FORCE).
        let plain = format!("DROP DATABASE IF EXISTS {db}");
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

/// `insert_script` of a `jsonb` array of big integers and `1.0` writes it
/// exactly (it isn't rounded through floats as a vector would be).
async fn json_numbers_stay(d: &dyn Driver, s: &mut Box<dyn Session>, schema: &str) {
    run(
        s,
        &format!("CREATE TABLE {schema}.json_numbers (id integer PRIMARY KEY, j jsonb)"),
    )
    .await;
    let ins = d
        .insert_script(
            &obj(schema, "json_numbers"),
            &["id".to_string(), "j".to_string()],
            &[vec![json!(1), json!([9007199254740993u64, 1.0])]],
        )
        .unwrap();
    run(s, &ins).await;
    let got = cells(
        s,
        &format!("SELECT j::text FROM {schema}.json_numbers WHERE id = 1"),
    )
    .await;
    assert_eq!(got, [[Some("[9007199254740993, 1.0]".to_string())]]);
}

// ------------------------------------------------------------- pgvector

/// pgvector's whole round trip on a server where `CREATE EXTENSION
/// vector` works; `extra` adds the variant's own index (DiskANN…).
async fn pgvector_round_trip(id: &str, env: &str, extra: Option<(IndexDef, &str, &str)>) {
    round_trip(id, env, extra, true).await
}

/// `full`: the server has pgvector's HNSW and IVFFlat for every type;
/// otherwise (YugabyteDB's ybhnsw) only `vector` columns are indexed.
async fn round_trip(id: &str, env: &str, extra: Option<(IndexDef, &str, &str)>, full: bool) {
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
        // openGauss' DataVec is built in: no extension to create.
        if id != "opengauss" {
            run(&mut s, "CREATE EXTENSION IF NOT EXISTS vector").await;
        }
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
        let mut indexes = vec![vindex(
            "ix_v",
            &["v"],
            Some("hnsw"),
            metric::COSINE,
            &[("m", "16"), ("ef_construction", "64")],
        )];
        if full {
            indexes.extend([
                vindex(
                    "ix_h",
                    &["h"],
                    Some("ivfflat"),
                    metric::L2,
                    &[("lists", "1")],
                ),
                vindex("ix_s", &["s"], None, metric::IP, &[]),
                vindex("ix_b", &["b"], Some("hnsw"), metric::HAMMING, &[]),
            ]);
        } else {
            indexes.push(vindex("ix_v2", &["v"], None, metric::L2, &[]));
        }
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
        let grid = r.clone();
        same_when_streamed(
            &mut s,
            "SELECT id, v, h, s, b FROM public.items ORDER BY id",
            &grid,
        )
        .await;
        // A JSON array of numbers that isn't a vector keeps its exact text.
        json_numbers_stay(&*d, &mut s, "public").await;
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
        let default_alg = spec.vector_indexes.as_ref().unwrap().algorithms[0];
        let hnsw = if full { "hnsw" } else { default_alg };
        assert_vector_index(
            index(&read, "ix_v"),
            hnsw,
            metric::COSINE,
            &[("m", "16"), ("ef_construction", "64")],
        );
        if full {
            assert_vector_index(
                index(&read, "ix_h"),
                "ivfflat",
                metric::L2,
                &[("lists", "1")],
            );
            assert_vector_index(index(&read, "ix_s"), default_alg, metric::IP, &[]);
            assert_vector_index(index(&read, "ix_b"), "hnsw", metric::HAMMING, &[]);
        } else {
            assert_vector_index(index(&read, "ix_v2"), default_alg, metric::L2, &[]);
        }
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
            again.contains(&format!(
                "USING {hnsw} (\"v\" vector_cosine_ops) WITH (ef_construction=64, m=16)"
            )),
            "{again}"
        );
        if full {
            assert!(
                again.contains("USING ivfflat (\"h\" halfvec_l2_ops) WITH (lists=1)"),
                "{again}"
            );
            assert!(again.contains("(\"s\" sparsevec_ip_ops)"), "{again}");
            assert!(
                again.contains("USING hnsw (\"b\" bit_hamming_ops)"),
                "{again}"
            );
        }
        // Sync: unchanged, then a metric changed.
        sync_round_trip(&*d, &mut s, &read, "ix_v", metric::IP, "vector_ip_ops").await;
        let read = read_table(&mut s, "public", "items").await;
        assert_vector_index(
            index(&read, "ix_v"),
            hnsw,
            metric::IP,
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

/// YugabyteDB: pgvector's types, its own `ybhnsw` index (also for `USING
/// hnsw`), vector columns only.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn yugabyte_vectors() {
    round_trip("yugabytedb", "DBINE_TEST_YUGABYTE_URL", None, false).await;
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

// ------------------------------------------------------------ CockroachDB

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cockroach_vectors() {
    let Ok(url) = std::env::var("DBINE_TEST_COCKROACH_URL") else {
        eprintln!("DBINE_TEST_COCKROACH_URL not set; skipping");
        return;
    };
    let cfg = parse_url("cockroachdb", &url);
    let d = driver("cockroachdb");
    assert!(d.capabilities().vectors);
    let spec = d.designer().unwrap();
    assert_eq!(spec.vector_indexes.as_ref().unwrap().algorithms, ["cspann"]);
    assert!(spec.data_types.contains(&"VECTOR(1536)"));
    let mut s = fresh_database(&*d, &cfg, true).await;
    let body = async {
        // 1. A table with two vector indexes, one with a prefix column; a
        // pgvector-spelled type (halfvec) comes out as VECTOR.
        let t = table(
            "public",
            "items",
            vec![
                col("id", "INT8", false),
                col("uid", "INT8", true),
                col("v", "VECTOR(3)", true),
                col("h", "halfvec(3)", true),
            ],
            vec![
                vindex("ix_v", &["v"], None, metric::COSINE, &[]),
                vindex(
                    "ix_vu",
                    &["v", "uid"],
                    Some("cspann"),
                    metric::L2,
                    &[("min_partition_size", "16"), ("max_partition_size", "128")],
                ),
                // pgvector's HNSW from a PostgreSQL schema: C-SPANN, without
                // HNSW's parameters.
                vindex("ix_h", &["h"], Some("hnsw"), metric::IP, &[("m", "16")]),
            ],
        );
        let ddl = d.table_ddl(&t, ALL).unwrap();
        eprintln!("{ddl}");
        assert!(ddl.contains("\"h\" VECTOR(3)"), "{ddl}");
        assert!(ddl.contains("CREATE VECTOR INDEX \"ix_vu\" ON \"public\".\"items\" (\"uid\", \"v\" vector_l2_ops) WITH (max_partition_size=128, min_partition_size=16);"), "{ddl}");
        for st in ddl.split(";\n") {
            run(&mut s, st).await;
        }
        // 2. Rows.
        let target = obj("public", "items");
        let cols: Vec<String> = ["id", "uid", "v", "h"].map(String::from).to_vec();
        let rows = vec![
            vec![json!(1), json!(7), json!([1, 2.5, -3]), json!([1, 0, 0])],
            vec![json!(2), json!(7), json!([0.5, 0, 1]), json!([0, 1, 0])],
            vec![json!(3), json!(8), json!([-1, -1, -1]), json!([0, 0, 1])],
        ];
        run(&mut s, &d.insert_script(&target, &cols, &rows).unwrap()).await;
        // 3. Read back.
        let out = run(&mut s, "SELECT id, v FROM public.items ORDER BY id").await;
        let r = &out.results[0];
        assert_eq!(r.columns[1].type_name, "vector(3)");
        assert_eq!(
            r.columns[1].vector,
            Some(VectorMeta::dense(Some(3), VectorElement::F32))
        );
        assert_eq!(r.rows[0][1], json!("[1,2.5,-3]"));
        let grid = r.clone();
        same_when_streamed(&mut s, "SELECT id, v FROM public.items ORDER BY id", &grid).await;
        json_numbers_stay(&*d, &mut s, "public").await;
        let browse = s.browse_query(&target, 10);
        let out = run(&mut s, &browse).await;
        assert!(out.results[0]
            .columns
            .iter()
            .any(|c| c.name == "v" && c.vector.is_some()));
        // 4. Describe.
        let read = read_table(&mut s, "public", "items").await;
        assert_eq!(column(&read, "v").data_type, "vector(3)");
        assert_eq!(column(&read, "h").data_type, "vector(3)");
        // 5. The vector indexes (pg_am says `prefix`; they are C-SPANN).
        assert_vector_index(index(&read, "ix_v"), "cspann", metric::COSINE, &[]);
        let vu = index(&read, "ix_vu");
        assert_vector_index(
            vu,
            "cspann",
            metric::L2,
            &[("min_partition_size", "16"), ("max_partition_size", "128")],
        );
        assert_eq!(vu.columns, ["v", "uid"]);
        assert_vector_index(index(&read, "ix_h"), "cspann", metric::IP, &[]);
        let again = d
            .table_ddl(
                &read,
                DdlParts {
                    indexes: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(again.contains("(\"uid\", \"v\" vector_l2_ops) WITH (max_partition_size=128, min_partition_size=16)"), "{again}");
        sync_round_trip(&*d, &mut s, &read, "ix_v", metric::L2, "vector_l2_ops").await;
        let read = read_table(&mut s, "public", "items").await;
        assert_vector_index(index(&read, "ix_v"), "cspann", metric::L2, &[]);
        // 6. Similarity, also with the prefix column.
        let near = cells(
            &mut s,
            "SELECT id FROM public.items ORDER BY v <-> '[1,2.5,-3]' LIMIT 1",
        )
        .await;
        assert_eq!(near, [[Some("1".to_string())]]);
        let near = cells(
            &mut s,
            "SELECT id FROM public.items WHERE uid = 8 ORDER BY v <-> '[1,2.5,-3]' LIMIT 1",
        )
        .await;
        assert_eq!(near, [[Some("3".to_string())]]);
        // 7. An edit.
        let change = RowChange {
            key: vec![("id".into(), json!(2))],
            set: vec![("v".into(), json!([9, 8, 7.5]))],
            row: Vec::new(),
        };
        run(&mut s, &d.update_script(&target, &[change]).unwrap()).await;
        let got = cells(&mut s, "SELECT v FROM public.items WHERE id = 2").await;
        assert_eq!(got, [[Some("[9,8,7.5]".into())]]);
        // 8. A transfer to a copy (CockroachDB has no bulk load: INSERTs).
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
            &["id", "uid", "v", "h"],
        )
        .await;
        assert_eq!(announced[2].type_name, "vector(3)");
        let diff = cells(&mut s, "SELECT count(*) FROM (SELECT id, v::STRING, h::STRING FROM public.items EXCEPT SELECT id, v::STRING, h::STRING FROM public.items_copy)").await;
        assert_eq!(diff, [[Some("0".to_string())]]);
        drop(s2);
    };
    let cleanup = async {
        drop_database(&*d, &cfg, true).await;
    };
    guarded(body, cleanup).await;
}

// --------------------------------------------------------------- CrateDB

/// The tables the CrateDB test makes (its schema goes with them).
async fn drop_crate_tables(s: &mut Box<dyn Session>) {
    for t in ["items", "items_copy", "items_synced", "json_numbers"] {
        if let Err(e) = try_run(s, &format!("DROP TABLE IF EXISTS {DB}.{t}")).await {
            eprintln!("cleanup: {e}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cratedb_vectors() {
    let Ok(url) = std::env::var("DBINE_TEST_CRATEDB_URL") else {
        eprintln!("DBINE_TEST_CRATEDB_URL not set; skipping");
        return;
    };
    let cfg = parse_url("cratedb", &url);
    let d = driver("cratedb");
    assert!(d.capabilities().vectors);
    // FLOAT_VECTOR is indexed (HNSW) by being declared: no index to make.
    let spec = d.designer().unwrap();
    assert!(spec.vector_indexes.is_none());
    assert!(spec.data_types.contains(&"FLOAT_VECTOR(1536)"));
    let mut s = d.connect(&cfg, None).await.expect("connect");
    drop_crate_tables(&mut s).await;
    let mut s2 = d.connect(&cfg, None).await.expect("connect");
    let body = async {
        // 1. The table; a vector index (from a PostgreSQL schema) adds
        // nothing, and a pgvector type comes out as FLOAT_VECTOR.
        let t = table(
            DB,
            "items",
            vec![col("id", "INTEGER", false), col("v", "vector(3)", true)],
            vec![vindex("ix_v", &["v"], Some("hnsw"), metric::COSINE, &[])],
        );
        let ddl = d.table_ddl(&t, ALL).unwrap();
        eprintln!("{ddl}");
        assert!(ddl.contains("\"v\" FLOAT_VECTOR(3)"), "{ddl}");
        assert!(!ddl.contains("INDEX"), "{ddl}");
        run(&mut s, &ddl).await;
        // 2. Rows: vectors as array literals.
        let target = obj(DB, "items");
        let cols: Vec<String> = ["id", "v"].map(String::from).to_vec();
        let rows = vec![
            vec![json!(1), json!([1, 2.5, -3])],
            vec![json!(2), json!([0.5, 0, 1])],
            vec![json!(3), json!([-1, -1, -1])],
        ];
        let ins = d.insert_script(&target, &cols, &rows).unwrap();
        assert!(ins.contains("[1,2.5,-3]") && !ins.contains("'[1"), "{ins}");
        run(&mut s, &ins).await;
        run(&mut s, &format!("REFRESH TABLE {DB}.items")).await;
        // 3. Read back: real[] on the wire, a vector to DBine.
        let out = run(&mut s, &format!("SELECT id, v FROM {DB}.items ORDER BY id")).await;
        let r = &out.results[0];
        assert_eq!(r.columns[1].type_name, "float_vector(3)");
        assert_eq!(
            r.columns[1].vector,
            Some(VectorMeta::dense(Some(3), VectorElement::F32))
        );
        assert_eq!(r.rows[0][1], json!("[1,2.5,-3]"));
        assert_eq!(r.rows[1][1], json!("[0.5,0,1]"));
        // Streamed to a sink: the columns are vectors and the cells JSON
        // arrays as they arrive, so a data sync from them writes vectors.
        let grid = r.clone();
        let sql = format!("SELECT id, v FROM {DB}.items ORDER BY id");
        same_when_streamed(&mut s, &sql, &grid).await;
        let (scols, srows) = streamed(&mut s, &sql).await;
        run(
            &mut s,
            &format!("CREATE TABLE {DB}.items_synced (id INTEGER PRIMARY KEY, v FLOAT_VECTOR(3))"),
        )
        .await;
        let ins = d
            .insert_script(
                &obj(DB, "items_synced"),
                &cols,
                &dbine_driver::vector::rows_for_scripts(&scols, &srows),
            )
            .unwrap();
        run(&mut s, &ins).await;
        run(&mut s, &format!("REFRESH TABLE {DB}.items_synced")).await;
        let synced = run(
            &mut s,
            &format!("SELECT id, v FROM {DB}.items_synced ORDER BY id"),
        )
        .await;
        assert_eq!(synced.results[0].rows, grid.rows);
        // An ARRAY(BIGINT) value keeps its big integers.
        run(
            &mut s,
            &format!("CREATE TABLE {DB}.json_numbers (id INTEGER PRIMARY KEY, a ARRAY(BIGINT))"),
        )
        .await;
        let ins = d
            .insert_script(
                &obj(DB, "json_numbers"),
                &["id".to_string(), "a".to_string()],
                &[vec![json!(1), json!([9007199254740993u64, 1])]],
            )
            .unwrap();
        assert!(ins.contains("[9007199254740993,1]"), "{ins}");
        run(&mut s, &ins).await;
        run(&mut s, &format!("REFRESH TABLE {DB}.json_numbers")).await;
        let got = cells(&mut s, &format!("SELECT a[1] FROM {DB}.json_numbers")).await;
        assert_eq!(got, [[Some("9007199254740993".to_string())]]);
        let browse = s.browse_query(&target, 10);
        let out = run(&mut s, &browse).await;
        assert!(out.results[0]
            .columns
            .iter()
            .any(|c| c.name == "v" && c.vector.is_some()));
        // 4. Describe.
        let read = read_table(&mut s, DB, "items").await;
        assert_eq!(column(&read, "v").data_type, "FLOAT_VECTOR(3)");
        // 5. No index object: the column's own HNSW.
        assert!(read.indexes.iter().all(|i| i.vector.is_none()));
        // 6. Similarity.
        let near = cells(&mut s, &format!("SELECT id FROM {DB}.items WHERE knn_match(v, [1, 2.5, -3], 1) ORDER BY _score DESC LIMIT 1")).await;
        assert_eq!(near, [[Some("1".to_string())]]);
        // 7. An edit.
        let change = RowChange {
            key: vec![("id".into(), json!(2))],
            set: vec![("v".into(), json!([9, 8, 7.5]))],
            row: Vec::new(),
        };
        run(&mut s, &d.update_script(&target, &[change]).unwrap()).await;
        run(&mut s, &format!("REFRESH TABLE {DB}.items")).await;
        let out = run(&mut s, &format!("SELECT v FROM {DB}.items WHERE id = 2")).await;
        assert_eq!(out.results[0].rows[0][0], json!("[9,8,7.5]"));
        // 8. A transfer to a copy (no bulk load: INSERTs with array literals).
        let mut copy = t.clone();
        copy.name = "items_copy".into();
        run(&mut s, &d.table_ddl(&copy, ALL).unwrap()).await;
        let announced = transfer(
            &mut s,
            &target,
            &*d,
            &mut s2,
            &obj(DB, "items_copy"),
            &["id", "v"],
        )
        .await;
        assert_eq!(announced[1].type_name, "FLOAT_VECTOR(3)");
        run(&mut s, &format!("REFRESH TABLE {DB}.items_copy")).await;
        let a = run(&mut s, &format!("SELECT id, v FROM {DB}.items ORDER BY id")).await;
        let b = run(
            &mut s,
            &format!("SELECT id, v FROM {DB}.items_copy ORDER BY id"),
        )
        .await;
        assert_eq!(a.results[0].rows, b.results[0].rows);
    };
    let cleanup = async {
        let mut c = d.connect(&cfg, None).await.expect("connect");
        drop_crate_tables(&mut c).await;
    };
    guarded(body, cleanup).await;
}

// ------------------------------------------------- between the engines

/// pgvector → CockroachDB → pgvector and pgvector → CrateDB → pgvector:
/// the vectors arrive whole each way (bulk load into PostgreSQL, INSERTs
/// with each target's literal into the others).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfers_between_engines() {
    let (Ok(pg_url), Ok(crdb_url), Ok(crate_url)) = (
        std::env::var("DBINE_TEST_PGVECTOR_URL"),
        std::env::var("DBINE_TEST_COCKROACH_URL"),
        std::env::var("DBINE_TEST_CRATEDB_URL"),
    ) else {
        eprintln!("DBINE_TEST_PGVECTOR_URL, DBINE_TEST_COCKROACH_URL or DBINE_TEST_CRATEDB_URL not set; skipping");
        return;
    };
    const X: &str = "dbine_vec_x";
    let (pgd, crdbd, crated) = (driver("postgres"), driver("cockroachdb"), driver("cratedb"));
    let (pgc, crdbc, cratec) = (
        parse_url("postgres", &pg_url),
        parse_url("cockroachdb", &crdb_url),
        parse_url("cratedb", &crate_url),
    );
    let mut pg = fresh_named(&*pgd, &pgc, false, X).await;
    let mut crdb = fresh_named(&*crdbd, &crdbc, true, X).await;
    let mut cr = crated.connect(&cratec, None).await.unwrap();
    let _ = try_run(&mut cr, &format!("DROP TABLE IF EXISTS {X}.dst")).await;
    let body = async {
        run(&mut pg, "CREATE EXTENSION IF NOT EXISTS vector").await;
        let vec_cols = |v: &str, h: &str| {
            vec![
                col("id", "integer", false),
                col("v", v, true),
                col("h", h, true),
            ]
        };
        for (name, cols) in [
            ("src", vec_cols("vector(3)", "halfvec(3)")),
            ("from_crdb", vec_cols("vector(3)", "halfvec(3)")),
            ("from_crate", vec_cols("vector(3)", "halfvec(3)")),
        ] {
            run(
                &mut pg,
                &pgd.table_ddl(&table("public", name, cols, vec![]), ALL)
                    .unwrap(),
            )
            .await;
        }
        // The PostgreSQL table as it is, created on the others (a schema
        // conversion within the family keeps the types as they are).
        let src_schema = read_table(&mut pg, "public", "src").await;
        let mut on_crdb = src_schema.clone();
        on_crdb.name = "dst".into();
        run(&mut crdb, &crdbd.table_ddl(&on_crdb, ALL).unwrap()).await;
        let mut on_crate = src_schema.clone();
        on_crate.schema = Some(X.into());
        on_crate.name = "dst".into();
        run(&mut cr, &crated.table_ddl(&on_crate, ALL).unwrap()).await;
        run(&mut pg, "INSERT INTO public.src VALUES (1, '[1,2.5,-3]', '[1,2,3]'), (2, '[0.125,-0,1e-3]', '[0.5,0,1]'), (3, NULL, NULL)").await;
        let cols = ["id", "v", "h"];
        let mut pg2 = pgd.connect(&pgc, Some(X)).await.unwrap();
        // pgvector → CockroachDB → pgvector.
        transfer(
            &mut pg,
            &obj("public", "src"),
            &*crdbd,
            &mut crdb,
            &obj("public", "dst"),
            &cols,
        )
        .await;
        let mut crdb2 = crdbd.connect(&crdbc, Some(X)).await.unwrap();
        transfer(
            &mut crdb2,
            &obj("public", "dst"),
            &*pgd,
            &mut pg2,
            &obj("public", "from_crdb"),
            &cols,
        )
        .await;
        // pgvector → CrateDB → pgvector.
        let mut cr2 = crated.connect(&cratec, None).await.unwrap();
        transfer(
            &mut pg,
            &obj("public", "src"),
            &*crated,
            &mut cr,
            &obj(X, "dst"),
            &cols,
        )
        .await;
        run(&mut cr, &format!("REFRESH TABLE {X}.dst")).await;
        let out = run(&mut cr, &format!("SELECT v, h FROM {X}.dst WHERE id = 1")).await;
        assert_eq!(
            out.results[0].rows[0],
            vec![json!("[1,2.5,-3]"), json!("[1,2,3]")]
        );
        transfer(
            &mut cr2,
            &obj(X, "dst"),
            &*pgd,
            &mut pg2,
            &obj("public", "from_crate"),
            &cols,
        )
        .await;
        for back in ["from_crdb", "from_crate"] {
            // By value: -0 and 0 are the same number (JSON says `-0` and
            // reads it as 0).
            let diff = cells(&mut pg, &format!("SELECT count(*) FROM (SELECT id, v, h FROM public.src EXCEPT SELECT id, v, h FROM public.{back}) x")).await;
            assert_eq!(diff, [[Some("0".to_string())]], "{back}");
            let n = cells(&mut pg, &format!("SELECT count(*) FROM public.{back}")).await;
            assert_eq!(n, [[Some("3".to_string())]], "{back}");
        }
    };
    let cleanup = async {
        drop_named(&*pgd, &pgc, false, X).await;
        drop_named(&*crdbd, &crdbc, true, X).await;
        if let Ok(mut c) = crated.connect(&cratec, None).await {
            let _ = try_run(&mut c, &format!("DROP TABLE IF EXISTS {X}.dst")).await;
        }
    };
    guarded(body, cleanup).await;
}

/// Where a sibling keeps the test's tables: a database of its own, or a
/// schema (RisingWave).
#[derive(Clone, Copy, PartialEq)]
enum Home {
    Database,
    Schema,
}

/// pgvector → `id` → pgvector for TimescaleDB, YugabyteDB (pgvector there
/// too: COPY both ways) and RisingWave (`vector(n)`, INSERTs into it):
/// every vector arrives whole each way. Each sibling is skipped without its
/// URL; what each makes is dropped first and at the end.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfers_with_pgvector_siblings() {
    let Ok(pg_url) = std::env::var("DBINE_TEST_PGVECTOR_URL") else {
        eprintln!("DBINE_TEST_PGVECTOR_URL not set; skipping");
        return;
    };
    for (id, env, home, halfvec) in [
        (
            "timescaledb",
            "DBINE_TEST_TIMESCALE_HA_URL",
            Home::Database,
            true,
        ),
        (
            "yugabytedb",
            "DBINE_TEST_YUGABYTE_URL",
            Home::Database,
            true,
        ),
        (
            "risingwave",
            "DBINE_TEST_RISINGWAVE_URL",
            Home::Schema,
            false,
        ),
    ] {
        let Ok(url) = std::env::var(env) else {
            eprintln!("{env} not set; skipping {id}");
            continue;
        };
        sibling_round_trip(&pg_url, id, &url, home, halfvec).await;
    }
}

async fn sibling_round_trip(pg_url: &str, id: &str, url: &str, home: Home, halfvec: bool) {
    const X: &str = "dbine_vec_s";
    let (pgd, od) = (driver("postgres"), driver(id));
    let (pgc, oc) = (parse_url("postgres", pg_url), parse_url(id, url));
    let mut pg = fresh_named(&*pgd, &pgc, false, X).await;
    let schema = match home {
        Home::Database => "public",
        Home::Schema => X,
    };
    let drop_schema = format!("DROP SCHEMA IF EXISTS {X} CASCADE");
    let mut other = match home {
        Home::Database => fresh_named(&*od, &oc, false, X).await,
        Home::Schema => {
            let mut s = od.connect(&oc, None).await.expect("connect");
            run(&mut s, &drop_schema).await;
            run(&mut s, &format!("CREATE SCHEMA {X}")).await;
            s
        }
    };
    let body = async {
        run(&mut pg, "CREATE EXTENSION IF NOT EXISTS vector").await;
        if home == Home::Database {
            run(&mut other, "CREATE EXTENSION IF NOT EXISTS vector").await;
        }
        let mut columns = vec![col("id", "integer", false), col("v", "vector(3)", true)];
        let mut cols = vec!["id", "v"];
        if halfvec {
            columns.push(col("h", "halfvec(3)", true));
            cols.push("h");
        }
        for name in ["src", "back"] {
            let t = table("public", name, columns.clone(), vec![]);
            run(&mut pg, &pgd.table_ddl(&t, ALL).unwrap()).await;
        }
        let dst = table(schema, "dst", columns.clone(), vec![]);
        run(&mut other, &od.table_ddl(&dst, ALL).unwrap()).await;
        let values = if halfvec {
            "(1, '[1,2.5,-3]', '[1,2,3]'), (2, '[0.125,-0,1e-3]', '[0.5,0,1]'), (3, NULL, NULL)"
        } else {
            "(1, '[1,2.5,-3]'), (2, '[0.125,-0,1e-3]'), (3, NULL)"
        };
        run(&mut pg, &format!("INSERT INTO public.src VALUES {values}")).await;
        let mut pg2 = pgd.connect(&pgc, Some(X)).await.unwrap();
        let mut other2 = match home {
            Home::Database => od.connect(&oc, Some(X)).await.unwrap(),
            Home::Schema => od.connect(&oc, None).await.unwrap(),
        };
        let announced = transfer(
            &mut pg,
            &obj("public", "src"),
            &*od,
            &mut other,
            &obj(schema, "dst"),
            &cols,
        )
        .await;
        assert_eq!(announced[1].type_name, "vector(3)", "{id}");
        if home == Home::Schema {
            run(&mut other, "FLUSH").await;
        }
        // On the sibling: the vectors as they were sent, read as vectors.
        let out = run(
            &mut other,
            &format!("SELECT id, v FROM {schema}.dst ORDER BY id"),
        )
        .await;
        let r = &out.results[0];
        assert_eq!(
            r.columns[1].vector,
            Some(VectorMeta::dense(Some(3), VectorElement::F32)),
            "{id}"
        );
        assert_eq!(r.rows[0][1], json!("[1,2.5,-3]"), "{id}");
        let announced = transfer(
            &mut other2,
            &obj(schema, "dst"),
            &*pgd,
            &mut pg2,
            &obj("public", "back"),
            &cols,
        )
        .await;
        assert_eq!(announced[1].type_name, "vector(3)", "{id}");
        let list = cols.join(", ");
        let diff = cells(&mut pg, &format!("SELECT count(*) FROM (SELECT {list} FROM public.src EXCEPT SELECT {list} FROM public.back) x")).await;
        assert_eq!(diff, [[Some("0".to_string())]], "{id}");
        let n = cells(&mut pg, "SELECT count(*) FROM public.back").await;
        assert_eq!(n, [[Some("3".to_string())]], "{id}");
    };
    let cleanup = async {
        drop_named(&*pgd, &pgc, false, X).await;
        match home {
            Home::Database => drop_named(&*od, &oc, false, X).await,
            Home::Schema => {
                if let Ok(mut c) = od.connect(&oc, None).await {
                    if let Err(e) = try_run(&mut c, &drop_schema).await {
                        eprintln!("cleanup: {e}");
                    }
                }
            }
        }
    };
    guarded(body, cleanup).await;
}

// ------------------------------------------------------------ RisingWave

/// RisingWave: `vector(n)`, HNSW and FLAT indexes on append-only tables.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn risingwave_vectors() {
    let Ok(url) = std::env::var("DBINE_TEST_RISINGWAVE_URL") else {
        eprintln!("DBINE_TEST_RISINGWAVE_URL not set; skipping");
        return;
    };
    let cfg = parse_url("risingwave", &url);
    let d = driver("risingwave");
    assert!(d.capabilities().vectors);
    let spec = d.designer().unwrap();
    assert_eq!(
        spec.vector_indexes.as_ref().unwrap().algorithms,
        ["hnsw", "flat"]
    );
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let drop = format!("DROP SCHEMA IF EXISTS {DB} CASCADE");
    run(&mut s, &drop).await;
    run(&mut s, &format!("CREATE SCHEMA {DB}")).await;
    let body = async {
        // 1. An append-only table (vector indexes need one) with two indexes.
        let mut t = table(
            DB,
            "items",
            vec![col("id", "integer", false), col("v", "vector(3)", true)],
            vec![
                vindex("ix_v", &["v"], Some("hnsw"), metric::COSINE, &[("m", "16")]),
                vindex("ix_f", &["v"], Some("flat"), metric::IP, &[]),
            ],
        );
        t.primary_key = None;
        t.options.insert("append_only".into(), "true".into());
        let ddl = d.table_ddl(&t, ALL).unwrap();
        eprintln!("{ddl}");
        for st in ddl.split(";\n") {
            run(&mut s, st).await;
        }
        // 2. Rows.
        let target = obj(DB, "items");
        let cols: Vec<String> = ["id", "v"].map(String::from).to_vec();
        let rows = vec![
            vec![json!(1), json!([1, 2.5, -3])],
            vec![json!(2), json!([0.5, 0, 1])],
            vec![json!(3), json!([-1, -1, -1])],
        ];
        run(&mut s, &d.insert_script(&target, &cols, &rows).unwrap()).await;
        run(&mut s, "FLUSH").await;
        // 3. Read back (by the table's types: RisingWave can't describe it).
        let out = run(&mut s, &format!("SELECT id, v FROM {DB}.items ORDER BY id")).await;
        let r = &out.results[0];
        assert_eq!(r.columns[1].type_name, "vector(3)");
        assert_eq!(
            r.columns[1].vector,
            Some(VectorMeta::dense(Some(3), VectorElement::F32))
        );
        assert_eq!(r.rows[0][1], json!("[1,2.5,-3]"));
        let browse = s.browse_query(&target, 10);
        let out = run(&mut s, &browse).await;
        assert!(out.results[0]
            .columns
            .iter()
            .any(|c| c.name == "v" && c.vector.is_some()));
        // 4. Describe.
        let read = read_table(&mut s, DB, "items").await;
        assert_eq!(column(&read, "v").data_type, "vector(3)");
        // 5. The indexes.
        assert_vector_index(index(&read, "ix_v"), "hnsw", metric::COSINE, &[("m", "16")]);
        assert_vector_index(index(&read, "ix_f"), "flat", metric::IP, &[]);
        sync_round_trip(
            &*d,
            &mut s,
            &read,
            "ix_v",
            metric::L2,
            "distance_type = 'l2'",
        )
        .await;
        let read = read_table(&mut s, DB, "items").await;
        assert_vector_index(index(&read, "ix_v"), "hnsw", metric::L2, &[("m", "16")]);
        // 6. Similarity.
        let near = cells(
            &mut s,
            &format!("SELECT id FROM {DB}.items ORDER BY v <-> '[1,2.5,-3]' LIMIT 1"),
        )
        .await;
        assert_eq!(near, [[Some("1".to_string())]]);
        // 8. A transfer into a table with a key (INSERTs: no bulk load),
        // where 7. an edit can run (an append-only table takes none).
        let mut copy = table(DB, "items_copy", t.columns.clone(), vec![]);
        copy.options.clear();
        run(&mut s, &d.table_ddl(&copy, ALL).unwrap()).await;
        let mut s2 = d.connect(&cfg, None).await.unwrap();
        let announced = transfer(
            &mut s,
            &target,
            &*d,
            &mut s2,
            &obj(DB, "items_copy"),
            &["id", "v"],
        )
        .await;
        assert_eq!(announced[1].type_name, "vector(3)");
        run(&mut s, "FLUSH").await;
        let a = run(&mut s, &format!("SELECT id, v FROM {DB}.items ORDER BY id")).await;
        let b = run(
            &mut s,
            &format!("SELECT id, v FROM {DB}.items_copy ORDER BY id"),
        )
        .await;
        assert_eq!(a.results[0].rows, b.results[0].rows);
        let change = RowChange {
            key: vec![("id".into(), json!(2))],
            set: vec![("v".into(), json!([9, 8, 7.5]))],
            row: Vec::new(),
        };
        run(
            &mut s,
            &d.update_script(&obj(DB, "items_copy"), &[change]).unwrap(),
        )
        .await;
        run(&mut s, "FLUSH").await;
        let got = cells(
            &mut s,
            &format!("SELECT v FROM {DB}.items_copy WHERE id = 2"),
        )
        .await;
        assert_eq!(got, [[Some("[9,8,7.5]".into())]]);
    };
    let cleanup = async {
        if let Ok(mut c) = d.connect(&cfg, None).await {
            if let Err(e) = try_run(&mut c, &drop).await {
                eprintln!("cleanup: {e}");
            }
        }
    };
    guarded(body, cleanup).await;
}
