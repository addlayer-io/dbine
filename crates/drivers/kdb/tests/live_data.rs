//! Moving data against a real kdb+ (`dbine-test-kdb`, see `tests/live.rs`):
//! sync by rows, the notebook's write modes and a clone.
//!
//! The clone, and the check that two kdb+ processes hash a table alike,
//! need a second one: the TLS server of `tests/live.rs`
//! (`DBINE_TEST_KDB_TLS_URL`, `DBINE_TEST_KDB_CA`). Without it they say so
//! and stop.
//!
//! ```sh
//! DBINE_TEST_KDB_URL=127.0.0.1:15000:dbine:dbinepass \
//! DBINE_TEST_KDB_TLS_URL=127.0.0.1:15001 DBINE_TEST_KDB_CA="$PWD/tls/ca.pem" \
//!   cargo test -p dbine-driver-kdb --test live_data -- --ignored
//! ```

use dbine_driver::transfer::{
    changed_buckets, BatchSink, BatchSource, BucketSum, Buckets, Cell, DeltaDepth, DeltaResult,
    DeltaSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{kinds, ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

/// `host:port[:user:password]` from `DBINE_TEST_KDB_URL`.
fn cfg(read_only: bool) -> ConnectionConfig {
    let url =
        std::env::var("DBINE_TEST_KDB_URL").expect("DBINE_TEST_KDB_URL=host:port[:user:password]");
    let mut parts = url.splitn(4, ':');
    let host = parts.next().unwrap_or("127.0.0.1").to_string();
    let port = parts.next().and_then(|p| p.parse().ok()).unwrap_or(5000);
    ConnectionConfig {
        host,
        port,
        username: parts.next().map(str::to_string),
        password: parts.next().map(str::to_string),
        read_only,
        options: [("tcps".to_string(), "false".to_string())].into(),
        ..Default::default()
    }
}

/// The second kdb+ (TLS, verified by its CA), when there is one.
fn second() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_KDB_TLS_URL").ok()?;
    let ca = std::env::var("DBINE_TEST_KDB_CA").ok()?;
    let mut c = cfg(false);
    let (host, port) = url.split_once(':')?;
    c.host = host.to_string();
    c.port = port.parse().ok()?;
    c.options = [
        ("tcps".to_string(), "true".to_string()),
        ("SSL_CA_CERT_FILE".to_string(), ca),
    ]
    .into();
    Some(c)
}

/// The tests that stage rows check at the end that no staging table is
/// left: one at a time.
static STAGING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn kdb() -> Arc<dyn Driver> {
    dbine_driver_kdb::drivers().remove(0)
}

async fn run(s: &mut Box<dyn Session>, q: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(q, 1000, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    if let Some(e) = out.error.clone() {
        panic!("{q}: {e}");
    }
    out
}

/// The first cell of `q`'s result.
async fn value(s: &mut Box<dyn Session>, q: &str) -> Value {
    run(s, q).await.results[0].rows[0][0].clone()
}

fn table(name: &str) -> ObjectRef {
    ObjectRef {
        kind: kinds::TABLE.into(),
        schema: None,
        name: name.into(),
    }
}

#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    batches: Vec<RowBatch>,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> std::io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, batch: RowBatch) -> std::io::Result<()> {
        self.batches.push(batch);
        Ok(())
    }
}

/// Batches handed to a load.
struct Batches(Vec<RowBatch>);

#[dbine_driver::async_trait]
impl BatchSource for Batches {
    async fn next(&mut self) -> Option<RowBatch> {
        (!self.0.is_empty()).then(|| self.0.remove(0))
    }
}

/// Every batch of a read.
async fn read_all(s: &mut Box<dyn Session>, spec: &ReadSpec) -> (Vec<TransferColumn>, Batches) {
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(spec, sink.clone())
        .await
        .unwrap_or_else(|e| panic!("{spec:?}: {e}"));
    let c = std::mem::take(&mut *sink.lock().unwrap());
    (c.columns, Batches(c.batches))
}

fn delta(name: &str, key: &[&str], columns: &[&str], buckets: Buckets) -> DeltaSpec {
    DeltaSpec {
        table: table(name),
        key: key.iter().map(|k| k.to_string()).collect(),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        buckets,
        depth: DeltaDepth::Full,
        max_cores: 0,
    }
}

/// What the app does for a sync: both summaries, the changed buckets, the
/// source's rows of them (all of them when `whole`) and the apply.
async fn sync(
    d: &dyn Driver,
    src: &mut Box<dyn Session>,
    tgt: &mut Box<dyn Session>,
    source: &DeltaSpec,
    target: &DeltaSpec,
    whole: bool,
) -> (Vec<i64>, DeltaResult) {
    let a = src.delta_summary(source).await.unwrap();
    let b = tgt.delta_summary(target).await.unwrap();
    let changed = changed_buckets(&a, &b);
    if changed.is_empty() {
        // The app stops here: an empty list would mean every bucket.
        return (changed, DeltaResult::default());
    }
    let (buckets, filter) = if whole {
        (Vec::new(), None)
    } else {
        (
            changed.clone(),
            Some(d.delta_filter(source, &changed).unwrap()),
        )
    };
    let (cols, mut rows) = read_all(
        src,
        &ReadSpec {
            table: source.table.clone(),
            columns: Some(source.columns.clone()),
            filter,
        },
    )
    .await;
    let r = tgt
        .delta_apply(target, &buckets, &cols, &mut rows, &|_| {})
        .await
        .unwrap();
    (changed, r)
}

async fn same(
    src: &mut Box<dyn Session>,
    tgt: &mut Box<dyn Session>,
    a: &DeltaSpec,
    b: &DeltaSpec,
) -> (Vec<BucketSum>, Vec<BucketSum>) {
    (
        src.delta_summary(a).await.unwrap(),
        tgt.delta_summary(b).await.unwrap(),
    )
}

#[tokio::test]
#[ignore]
async fn sync_by_rows_finds_and_fixes_differences() {
    let _one = STAGING.lock().await;
    let d = kdb();
    assert!(d.supports_delta());
    let mut s = d.connect(&cfg(false), None).await.unwrap();
    run(
        &mut s,
        "delete dbine_kdbdata_s, dbine_kdbdata_d, dbine_kdbdata_us, dbine_kdbdata_ud from `.",
    )
    .await;
    let mut ro = d.connect(&cfg(true), None).await.unwrap();
    // A keyed source and a target that differs: two rows missing, two
    // changed, one too many (past the source's largest key).
    run(
        &mut s,
        "dbine_kdbdata_s:([id:1+til 5000] sym:5000#`a`b`c; px:0.5*til 5000; note:string til 5000; ts:2024.01.01D00:00:00.000000001+1000000000*til 5000); dbine_kdbdata_d:dbine_kdbdata_s",
    )
    .await;
    run(
        &mut s,
        "delete from `dbine_kdbdata_d where id in 10 20; update px:-1f from `dbine_kdbdata_d where id=3000; update note:enlist \"changed\" from `dbine_kdbdata_d where id=4999; `dbine_kdbdata_d upsert (6000;`z;1f;\"extra\";.z.p)",
    )
    .await;
    let cols = ["id", "sym", "px", "note", "ts"];
    // The source's key is an integer: range buckets of 1,000 ids.
    assert_eq!(
        ro.key_range(&table("dbine_kdbdata_s"), "id").await.unwrap(),
        Some((1, 5000, 5000))
    );
    assert!(ro
        .key_range(&table("dbine_kdbdata_s"), "sym")
        .await
        .unwrap_err()
        .to_string()
        .contains("no es entera"));
    let range = Buckets::Range {
        column: "id".into(),
        lo: 1,
        hi: 5000,
        width: 1000,
        n: 5,
    };
    let src_spec = delta("dbine_kdbdata_s", &["id"], &cols, range.clone());
    let tgt_spec = delta("dbine_kdbdata_d", &["id"], &cols, range.clone());
    // The source read-only (its summary and filtered read through reval).
    let (changed, r) = sync(d.as_ref(), &mut ro, &mut s, &src_spec, &tgt_spec, false).await;
    assert_eq!(changed, [0, 2, 4, 5]);
    assert_eq!((r.inserted, r.updated, r.deleted), (2, 2, 1), "{r:?}");
    assert!(r.notes[0].contains("un solo paso"), "{r:?}");
    let (a, b) = same(&mut ro, &mut s, &src_spec, &tgt_spec).await;
    assert_eq!(a, b);
    assert_eq!(
        value(
            &mut s,
            "(`id xasc 0!dbine_kdbdata_s)~`id xasc 0!dbine_kdbdata_d"
        )
        .await,
        json!(true)
    );
    // Nothing to do the second time.
    let (changed, _) = sync(d.as_ref(), &mut ro, &mut s, &src_spec, &tgt_spec, false).await;
    assert!(changed.is_empty());

    // Hash buckets on unkeyed tables, by a key the user picks; the target
    // keeps its key as `int` and the symbols as strings: the hashes see
    // the same values. The whole table is merged.
    run(
        &mut s,
        "dbine_kdbdata_us:0!dbine_kdbdata_s; dbine_kdbdata_ud:update id:`int$id, sym:string sym from 0!dbine_kdbdata_s",
    )
    .await;
    let hash = Buckets::Hash { n: 17 };
    let us = delta("dbine_kdbdata_us", &["id"], &cols, hash.clone());
    let ud = delta("dbine_kdbdata_ud", &["id"], &cols, hash.clone());
    let (a, b) = same(&mut ro, &mut s, &us, &ud).await;
    assert_eq!(a, b, "int and long keys, symbols and strings hash alike");
    assert_eq!(a.iter().map(|x| x.rows).sum::<u64>(), 5000);
    run(
        &mut s,
        "delete from `dbine_kdbdata_ud where id within 100 199; update px:px+1 from `dbine_kdbdata_ud where id within 400 409; `dbine_kdbdata_ud insert (7000i;enlist \"q\";2f;enlist \"x\";.z.p)",
    )
    .await;
    let (changed, r) = sync(d.as_ref(), &mut ro, &mut s, &us, &ud, true).await;
    assert!(!changed.is_empty());
    assert_eq!((r.inserted, r.updated, r.deleted), (100, 10, 1), "{r:?}");
    let (a, b) = same(&mut ro, &mut s, &us, &ud).await;
    assert_eq!(a, b);
    // Its types are what they were.
    assert_eq!(
        value(&mut s, "raze exec t from meta dbine_kdbdata_ud").await,
        json!("iCfCp")
    );
    // Filtered by hash buckets: only their rows are read and merged.
    run(&mut s, "update px:0f from `dbine_kdbdata_ud where id=1234").await;
    let (changed, r) = sync(d.as_ref(), &mut ro, &mut s, &us, &ud, false).await;
    assert_eq!(changed.len(), 1);
    assert_eq!((r.inserted, r.updated, r.deleted), (0, 1, 0), "{r:?}");
    let (a, b) = same(&mut ro, &mut s, &us, &ud).await;
    assert_eq!(a, b);

    // Depths: by sizes a string of the same length isn't seen, a longer one
    // is; by keys only rows that come or go.
    run(
        &mut s,
        "update note:enlist \"zzz\" from `dbine_kdbdata_ud where id=1000",
    )
    .await;
    let mut sizes = (us.clone(), ud.clone());
    sizes.0.depth = DeltaDepth::Sizes;
    sizes.1.depth = DeltaDepth::Sizes;
    let (a, b) = same(&mut ro, &mut s, &sizes.0, &sizes.1).await;
    assert_eq!(a, b, "same length");
    run(
        &mut s,
        "update note:enlist \"longer\" from `dbine_kdbdata_ud where id=1000",
    )
    .await;
    let (a, b) = same(&mut ro, &mut s, &sizes.0, &sizes.1).await;
    assert_eq!(changed_buckets(&a, &b).len(), 1);
    let mut keys = (us.clone(), ud.clone());
    keys.0.depth = DeltaDepth::Keys;
    keys.1.depth = DeltaDepth::Keys;
    let (a, b) = same(&mut ro, &mut s, &keys.0, &keys.1).await;
    assert_eq!(a, b, "only a value changed");
    run(&mut s, "delete from `dbine_kdbdata_ud where id=2").await;
    let (a, b) = same(&mut ro, &mut s, &keys.0, &keys.1).await;
    assert_eq!(changed_buckets(&a, &b).len(), 1);

    // A read-only connection reads, and doesn't apply.
    let e = ro
        .delta_apply(&ud, &[], &[], &mut Batches(vec![]), &|_| {})
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("solo lectura"), "{e}");
    // A partitioned table is summarized (a read) but never changed: by its
    // kind, and checked on the server when the kind isn't known.
    let q = delta(
        "dbine_q",
        &["date", "sym"],
        &["date", "sym", "px"],
        Buckets::Hash { n: 17 },
    );
    let sums = ro.delta_summary(&q).await.unwrap();
    assert_eq!(sums.iter().map(|x| x.rows).sum::<u64>(), 6);
    let mut on_disk = q.clone();
    on_disk.table.kind = dbine_driver_kdb::PARTITIONED.into();
    for spec in [&on_disk, &q] {
        let rows = vec![vec![
            Cell::Date("2026-11-01".into()),
            Cell::Text("a".into()),
            Cell::Float(1.0),
        ]];
        let e = s
            .delta_apply(
                spec,
                &[],
                &[],
                &mut Batches(vec![RowBatch { rows, bytes: 0 }]),
                &|_| {},
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("particionada o splayed"), "{e}");
    }
    // Names that aren't q names never reach the server.
    let mut evil = us.clone();
    evil.key = vec!["id;system\"echo x\"".into()];
    assert!(ro.delta_summary(&evil).await.is_err());

    // Two kdb+ processes hash the same table alike.
    if let Some(other) = second() {
        let mut o = d.connect(&other, None).await.unwrap();
        run(&mut o, "delete dbine_kdbdata_s from `.").await;
        run(
            &mut o,
            "dbine_kdbdata_s:([id:1+til 5000] sym:5000#`a`b`c; px:0.5*til 5000; note:string til 5000; ts:2024.01.01D00:00:00.000000001+1000000000*til 5000)",
        )
        .await;
        for b in [range, hash] {
            let spec = delta("dbine_kdbdata_s", &["id"], &cols, b);
            let (a, b) = same(&mut ro, &mut o, &spec, &spec).await;
            assert_eq!(a, b);
        }
        run(&mut o, "delete dbine_kdbdata_s from `.").await;
    } else {
        eprintln!("sin DBINE_TEST_KDB_TLS_URL: no se compara con otro proceso");
    }
    run(
        &mut s,
        "delete dbine_kdbdata_s, dbine_kdbdata_d, dbine_kdbdata_us, dbine_kdbdata_ud from `.",
    )
    .await;
    // The staging tables are gone.
    assert_eq!(value(&mut s, "count key `.dbine_stage").await, json!(1));
}
