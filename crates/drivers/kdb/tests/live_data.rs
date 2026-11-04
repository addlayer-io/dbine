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
    DeltaSpec, LoadSpec, ReadSpec, RowBatch, TransferColumn,
};
use dbine_driver::{
    kinds, ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session, WriteMode, WriteSpec,
};
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

/// The rows of `q`'s results.
async fn rows_of(s: &mut Box<dyn Session>, q: &str) -> Vec<Vec<Vec<Value>>> {
    run(s, q)
        .await
        .results
        .into_iter()
        .map(|r| r.rows)
        .collect()
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

fn write_spec(name: &str, columns: &[&str], key: &[&str], mode: WriteMode) -> WriteSpec {
    WriteSpec {
        table: table(name),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        key: key.iter().map(|k| k.to_string()).collect(),
        mode,
    }
}

fn rows(r: Vec<Vec<Cell>>) -> Batches {
    Batches(vec![RowBatch { rows: r, bytes: 0 }])
}

async fn write(
    s: &mut Box<dyn Session>,
    spec: &WriteSpec,
    r: Vec<Vec<Cell>>,
) -> dbine_driver::Result<dbine_driver::WriteResult> {
    s.write_rows(spec, &[], &mut rows(r), &|_| {}).await
}

#[tokio::test]
#[ignore]
async fn writes_in_every_mode() {
    let _one = STAGING.lock().await;
    let d = kdb();
    assert_eq!(d.write_modes(), WriteMode::ALL.to_vec());
    let mut s = d.connect(&cfg(false), None).await.unwrap();
    run(&mut s, "delete dbine_kdbdata_w, dbine_kdbdata_wu from `.").await;
    let kt = "dbine_kdbdata_w";
    run(
        &mut s,
        "dbine_kdbdata_w:([id:1 2 3] v:1 2 3f; s:`a`b`c; note:(enlist \"x\";enlist \"y\";enlist \"z\"))",
    )
    .await;
    let r2_4 = || {
        vec![
            vec![Cell::Int(2), Cell::Float(20.0)],
            vec![Cell::Int(4), Cell::Float(40.0)],
        ]
    };
    let cols = ["id", "v"];
    // Update by key: only the key that's there.
    let r = write(
        &mut s,
        &write_spec(kt, &cols, &["id"], WriteMode::Update),
        r2_4(),
    )
    .await
    .unwrap();
    assert_eq!(
        (r.inserted, r.updated, r.deleted, r.atomic),
        (0, 1, 0, true)
    );
    assert!(r.notes[0].contains("no tiene transacciones"), "{r:?}");
    assert_eq!(value(&mut s, "dbine_kdbdata_w[2]`v").await, json!(20.0));
    assert_eq!(value(&mut s, "dbine_kdbdata_w[2]`s").await, json!("b"));
    // Upsert: that update and the new key.
    let r = write(
        &mut s,
        &write_spec(kt, &cols, &["id"], WriteMode::Upsert),
        r2_4(),
    )
    .await
    .unwrap();
    assert_eq!((r.inserted, r.updated, r.deleted), (1, 1, 0));
    assert_eq!(value(&mut s, "count dbine_kdbdata_w").await, json!(4));
    // Insert: new keys go in; a key already there, or one repeated, is
    // refused and the table stays as it was.
    let r = write(
        &mut s,
        &write_spec(kt, &cols, &[], WriteMode::Insert),
        vec![
            vec![Cell::Int(5), Cell::Float(5.0)],
            vec![Cell::Int(6), Cell::Null],
        ],
    )
    .await
    .unwrap();
    assert_eq!((r.inserted, r.updated, r.deleted), (2, 0, 0));
    for (bad, why) in [
        (
            vec![vec![Cell::Int(1), Cell::Float(9.0)]],
            "ya tiene una fila",
        ),
        (
            vec![
                vec![Cell::Int(8), Cell::Float(9.0)],
                vec![Cell::Int(8), Cell::Float(9.0)],
            ],
            "repiten",
        ),
    ] {
        let e = write(&mut s, &write_spec(kt, &cols, &[], WriteMode::Insert), bad)
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains(why), "{e}");
        assert_eq!(value(&mut s, "count dbine_kdbdata_w").await, json!(6));
    }
    // Values travel as q objects: a string with q code in it is a string.
    let evil = "x\";system \"echo pwned\";\"";
    let r = write(
        &mut s,
        &write_spec(kt, &["id", "note"], &["id"], WriteMode::Update),
        vec![vec![Cell::Int(3), Cell::Text(evil.into())]],
    )
    .await
    .unwrap();
    assert_eq!(r.updated, 1);
    assert_eq!(value(&mut s, "dbine_kdbdata_w[3]`note").await, json!(evil));
    // Replace: the rows become the table, its key and types kept.
    let r = write(
        &mut s,
        &write_spec(kt, &cols, &[], WriteMode::Replace),
        vec![vec![Cell::Int(9), Cell::Float(0.5)]],
    )
    .await
    .unwrap();
    assert_eq!((r.inserted, r.updated, r.deleted), (1, 0, 6));
    // The columns not written get q's null of their type (a general list,
    // an empty one), as an insert of some columns does.
    assert_eq!(
        run(&mut s, "select id, v, s from dbine_kdbdata_w")
            .await
            .results[0]
            .rows,
        [vec![json!(9), json!(0.5), Value::Null]]
    );
    assert_eq!(value(&mut s, "keys dbine_kdbdata_w").await, json!("id"));
    assert_eq!(
        value(&mut s, "raze exec t from meta dbine_kdbdata_w").await,
        json!("jfs ")
    );

    // An unkeyed table, by a key the cell picks: every row with it.
    let ut = "dbine_kdbdata_wu";
    run(
        &mut s,
        "dbine_kdbdata_wu:([] id:1 2 3 2; v:1 2 3 4f; s:`a`b`c`d)",
    )
    .await;
    let r = write(
        &mut s,
        &write_spec(ut, &cols, &["id"], WriteMode::Update),
        r2_4(),
    )
    .await
    .unwrap();
    assert_eq!((r.inserted, r.updated, r.deleted), (0, 2, 0));
    assert_eq!(
        value(&mut s, "exec sum v from dbine_kdbdata_wu where id=2").await,
        json!(40.0)
    );
    let r = write(
        &mut s,
        &write_spec(ut, &cols, &["id"], WriteMode::Upsert),
        r2_4(),
    )
    .await
    .unwrap();
    assert_eq!((r.inserted, r.updated, r.deleted), (1, 2, 0));
    let r = write(
        &mut s,
        &write_spec(ut, &cols, &[], WriteMode::Insert),
        r2_4(),
    )
    .await
    .unwrap();
    assert_eq!((r.inserted, r.updated, r.deleted), (2, 0, 0));
    assert_eq!(value(&mut s, "count dbine_kdbdata_wu").await, json!(7));
    let r = write(
        &mut s,
        &write_spec(ut, &cols, &[], WriteMode::Replace),
        r2_4(),
    )
    .await
    .unwrap();
    assert_eq!((r.inserted, r.updated, r.deleted), (2, 0, 7));
    assert_eq!(
        value(&mut s, "raze exec t from meta dbine_kdbdata_wu").await,
        json!("jfs")
    );

    // Refused: read-only, on disk, a mode without its key, a bad name.
    let mut ro = d.connect(&cfg(true), None).await.unwrap();
    let e = write(
        &mut ro,
        &write_spec(ut, &cols, &[], WriteMode::Insert),
        r2_4(),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("solo lectura"), "{e}");
    for kind in [dbine_driver_kdb::PARTITIONED, dbine_driver_kdb::SPLAYED] {
        let mut spec = write_spec("dbine_q", &["px"], &[], WriteMode::Insert);
        spec.table.kind = kind.into();
        let e = write(&mut s, &spec, vec![]).await.unwrap_err().to_string();
        assert!(e.contains("particionada o splayed"), "{e}");
    }
    let e = write(
        &mut s,
        &write_spec("dbine_q", &["px"], &[], WriteMode::Insert),
        vec![],
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("particionada o splayed"), "{e}");
    assert!(write(
        &mut s,
        &write_spec(ut, &cols, &[], WriteMode::Update),
        r2_4()
    )
    .await
    .is_err());
    assert!(write(
        &mut s,
        &write_spec(ut, &["id", "v;x"], &[], WriteMode::Insert),
        r2_4()
    )
    .await
    .is_err());
    run(&mut s, "delete dbine_kdbdata_w, dbine_kdbdata_wu from `.").await;
    assert_eq!(value(&mut s, "count key `.dbine_stage").await, json!(1));
}

/// The clone test's objects, removed before and after it.
// The namespace may not exist yet (a fresh process): its delete is protected.
const CLONE_CLEANUP: &str = "delete dbine_kdbdata_ct, dbine_kdbdata_ck, dbine_kdbdata_f, dbine_kdbdata_v from `.; @[{delete t2, g from `.dbine_kdbdata};::;::]";

/// Run the clone `script` on `tgt` as the app does, copying the data from
/// `src`; only the statements about this test's objects (the source
/// process has other sessions' functions and views too).
async fn run_clone(
    src: &mut Box<dyn Session>,
    tgt: &mut Box<dyn Session>,
    script: &dbine_driver::transfer::CloneScript,
) {
    let ours = |q: &&String| q.contains("dbine_kdbdata");
    for q in script.before.iter().filter(ours) {
        run(tgt, q).await;
    }
    for t in &script.tables {
        run(tgt, &t.create).await;
        for q in &t.before_data {
            run(tgt, q).await;
        }
        let (cols, mut rows) = read_all(
            src,
            &ReadSpec {
                table: t.table.clone(),
                columns: None,
                filter: None,
            },
        )
        .await;
        // Only into an empty table (a run again finds the rows there).
        let empty = value(tgt, &format!("0=count {}", t.table.name)).await == json!(true);
        if empty {
            let load = LoadSpec {
                table: t.table.clone(),
                columns: cols.iter().map(|c| c.name.clone()).collect(),
                table_lock: true,
                keep_identity: true,
                commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
                commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
            };
            tgt.bulk_load(&load, &cols, &mut rows, &|_| {})
                .await
                .unwrap();
        }
        for q in &t.after_data {
            run(tgt, q).await;
        }
    }
    for q in script.after.iter().filter(ours) {
        run(tgt, q).await;
    }
}

#[tokio::test]
#[ignore]
async fn clone_between_two_processes() {
    let Some(other) = second() else {
        eprintln!("sin DBINE_TEST_KDB_TLS_URL: el clon necesita un segundo kdb+");
        return;
    };
    let d = kdb();
    assert!(d.supports_clone());
    let mut w = d.connect(&cfg(false), None).await.unwrap();
    run(&mut w, CLONE_CLEANUP).await;
    // A keyed table, a table linked to it with attributes and every common
    // type, a namespaced table and function (its globals resolve there),
    // a root function and a view.
    run(
        &mut w,
        "dbine_kdbdata_ck:([k:1 2 3] name:`a`b`c); dbine_kdbdata_ct:([] id:`s#1 2 3; r:`dbine_kdbdata_ck$1 3 1; sym:`g#`x`y`x; note:(enlist \"a\";\"bb\";\"\"); ts:3#2024.01.01D10:00:00.123456789; f:1.5 0n 3; d:2024.01.31 0N 2024.02.01; g:3?0Ng; b:(0x0102;`byte$();enlist 0xff); n:(1 2;`long$();enlist 3); z:3#2024.01.01T10:00:00.123; sp:0D01:02:03.000000004 -0D00:00:01 2D00:00:00; i:1 0N 0Wi); .dbine_kdbdata.t2:([] a:1 2); dbine_kdbdata_f:{x+count dbine_kdbdata_ct}",
    )
    .await;
    // A function of the namespace's own (parsed there) and a view.
    run(
        &mut w,
        "{system \"d .dbine_kdbdata\"; .dbine_kdbdata.g:parse \"{count t2}\"; system \"d .\"}[]",
    )
    .await;
    run(
        &mut w,
        "dbine_kdbdata_v::select from dbine_kdbdata_ct where f>1",
    )
    .await;
    assert_eq!(value(&mut w, ".dbine_kdbdata.g[]").await, json!(2));
    // The source is read on a read-only connection, as the app does.
    let mut src = d.connect(&cfg(true), None).await.unwrap();
    let mut tgt = d.connect(&other, None).await.unwrap();
    run(&mut tgt, CLONE_CLEANUP).await;
    let chosen = [
        table("dbine_kdbdata_ck"),
        table("dbine_kdbdata_ct"),
        table(".dbine_kdbdata.t2"),
        table("dbine_q"),
    ];
    let script = d.clone_script(&mut *src, &mut *tgt, &chosen).await.unwrap();
    let names: Vec<&str> = script
        .tables
        .iter()
        .map(|t| t.table.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["dbine_kdbdata_ck", "dbine_kdbdata_ct", ".dbine_kdbdata.t2"]
    );
    let notes = script.notes.join("\n");
    assert!(notes.contains("«dbine_q» es una tabla en disco"), "{notes}");
    assert!(notes.contains("memoria del proceso"), "{notes}");
    assert!(script
        .after
        .iter()
        .any(|a| a.contains("[`dbine_kdbdata_ct;`r;`dbine_kdbdata_ck]")));
    assert!(script
        .after
        .iter()
        .any(|a| a.contains("[`dbine_kdbdata_v;")));
    run_clone(&mut src, &mut tgt, &script).await;

    // The same tables: types, links and attributes, key, and values (their
    // row summaries match).
    for t in ["dbine_kdbdata_ck", "dbine_kdbdata_ct", ".dbine_kdbdata.t2"] {
        for q in [format!("keys {t}"), format!("0!meta {t}")] {
            assert_eq!(
                rows_of(&mut w, &q).await,
                rows_of(&mut tgt, &q).await,
                "{q}"
            );
        }
    }
    let cols = [
        "id", "r", "sym", "note", "ts", "f", "d", "g", "b", "n", "z", "sp", "i",
    ];
    let spec = delta("dbine_kdbdata_ct", &["id"], &cols, Buckets::Hash { n: 17 });
    let (a, b) = same(&mut src, &mut tgt, &spec, &spec).await;
    assert_eq!(a, b);
    let q = "select from dbine_kdbdata_ct";
    assert_eq!(rows_of(&mut w, q).await, rows_of(&mut tgt, q).await);
    // The code works there as here.
    assert_eq!(value(&mut tgt, "dbine_kdbdata_f 1").await, json!(4));
    assert_eq!(value(&mut tgt, ".dbine_kdbdata.g[]").await, json!(2));
    assert_eq!(value(&mut tgt, "count dbine_kdbdata_v").await, json!(2));
    assert_eq!(
        value(&mut tgt, "`dbine_kdbdata_v in system \"b\"").await,
        json!(true)
    );
    // Run again (a resume): nothing fails, nothing doubles.
    run_clone(&mut src, &mut tgt, &script).await;
    assert_eq!(value(&mut tgt, "count dbine_kdbdata_ct").await, json!(3));

    // A function's text that isn't one lambda never runs on the target.
    let f = script
        .after
        .iter()
        .find(|a| a.contains("[`dbine_kdbdata_f;"))
        .unwrap();
    let define = &f[..f.rfind("}[").unwrap() + 1];
    let mut out = QueryOutcome::default();
    let r = tgt
        .execute(
            &format!("{define}[`dbine_kdbdata_bad;\".\";\"{{x}} dbine_kdbdata_pwned:1; {{y}}\"]"),
            10,
            &mut out,
        )
        .await;
    assert!(r.is_err() || out.error.is_some());
    assert_eq!(
        value(&mut tgt, "`dbine_kdbdata_pwned`dbine_kdbdata_bad in key `.").await,
        json!(false)
    );

    for s in [&mut w, &mut tgt] {
        run(s, CLONE_CLEANUP).await;
    }
}
