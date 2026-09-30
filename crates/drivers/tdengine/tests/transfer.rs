//! Bulk transfer against a real TDengine (ignored by default; container as
//! in `integration.rs`):
//!
//! ```sh
//! DBINE_TEST_TDENGINE_URL=http://localhost:25641 \
//!   cargo test -p dbine-driver-tdengine -- --ignored transfer --nocapture
//! ```

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{async_trait, ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const ROWS: usize = 100_000;
const DB: &str = "dbine_transfer";
/// 2024-01-01 00:00:00 UTC, in µs.
const T0: i64 = 1_704_067_200_000_000;

struct Batches(std::vec::IntoIter<RowBatch>);

#[async_trait]
impl BatchSource for Batches {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.next()
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
    fn batch(&mut self, batch: RowBatch) -> std::io::Result<()> {
        self.rows.extend(batch.rows);
        Ok(())
    }
}

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_TDENGINE_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "tdengine".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("root".into()),
        password: Some("taosdata".into()),
        // A display zone must not shift what's transferred.
        options: [("timezone".to_string(), "America/Argentina/Buenos_Aires".to_string())].into(),
        ..Default::default()
    })
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    obj_in(DB, kind, name)
}

fn obj_in(db: &str, kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: Some(db.into()), name: name.into() }
}

/// `YYYY-MM-DD HH:MM:SS.ffffff` of `T0 + us`.
fn stamp(us: i64) -> String {
    let t = T0 + us;
    let (secs, frac) = (t.div_euclid(1_000_000), t.rem_euclid(1_000_000));
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Days since 2024-01-01 stay within January–March for these tests.
    let doy = days - 19_723;
    let (m, d) = if doy < 31 { (1, doy + 1) } else if doy < 60 { (2, doy - 30) } else { (3, doy - 59) };
    format!("2024-{m:02}-{d:02} {:02}:{:02}:{:02}.{frac:06}", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn plain_row(i: usize) -> Vec<Cell> {
    vec![
        Cell::DateTime(stamp(i as i64 * 1_001)),
        Cell::Float(if i == 1 { 1e300 } else { i as f64 / 3.0 }),
        if i.is_multiple_of(7) { Cell::Null } else { Cell::Text(format!("fila {i} ñ'")) },
        Cell::Bytes(vec![(i % 256) as u8, 0xFE]),
        Cell::UInt(u64::MAX - i as u64),
        Cell::Bool(i.is_multiple_of(2)),
    ]
}

fn batches(rows: Vec<Vec<Cell>>) -> Batches {
    let v: Vec<RowBatch> = rows.chunks(1_000).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect();
    Batches(v.into_iter())
}

async fn read(s: &mut dyn Session, spec: ReadSpec) -> Collect {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let n = s.read_batches(&spec, sink.clone()).await.expect("read");
    let c = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(n as usize, c.rows.len());
    c
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_tdengine() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_tdengine::drivers().remove(0);
    assert!(d.supports_bulk_load());
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&format!("DROP DATABASE IF EXISTS {DB}; CREATE DATABASE {DB} PRECISION 'us'"), 10, &mut out).await.unwrap();
    let mut s = d.connect(&c, Some(DB)).await.unwrap();
    s.execute(
        "CREATE TABLE plain (ts TIMESTAMP, v DOUBLE, s NCHAR(30), b VARBINARY(8), u BIGINT UNSIGNED, f BOOL);
         CREATE STABLE st (ts TIMESTAMP, v INT, s VARCHAR(20)) TAGS (loc VARCHAR(20), gid INT);
         CREATE STABLE st2 (ts TIMESTAMP, v INT, s VARCHAR(20)) TAGS (loc VARCHAR(20), gid INT);
         CREATE STABLE sj (ts TIMESTAMP, v INT) TAGS (j JSON)",
        10,
        &mut out,
    )
    .await
    .unwrap();

    // A normal table: load, read back, compare.
    let rows: Vec<Vec<Cell>> = (0..ROWS).map(plain_row).collect();
    let names: Vec<String> = ["ts", "v", "s", "b", "u", "f"].iter().map(|s| s.to_string()).collect();
    let spec = LoadSpec { table: obj("table", "plain"), columns: names.clone(), table_lock: false, keep_identity: false, commit_rows: 10_000, commit_bytes: 1 << 29 };
    let reports = Mutex::new(Vec::new());
    let t = Instant::now();
    let n = s.bulk_load(&spec, &[], &mut batches(rows.clone()), &|n| reports.lock().unwrap().push(n)).await.expect("load");
    let secs = t.elapsed().as_secs_f64();
    println!("tdengine load (table): {n} rows in {secs:.2}s = {:.0} rows/s", n as f64 / secs);
    assert_eq!(n as usize, ROWS);
    let reports = reports.into_inner().unwrap();
    assert!(reports.len() >= 9 && reports.windows(2).all(|w| w[0] < w[1]) && *reports.last().unwrap() == ROWS as u64, "{reports:?}");

    let t = Instant::now();
    let got = read(s.as_mut(), ReadSpec { table: obj("table", "plain"), columns: None, filter: None }).await;
    let secs = t.elapsed().as_secs_f64();
    println!("tdengine read (table): {} rows in {secs:.2}s = {:.0} rows/s", got.rows.len(), got.rows.len() as f64 / secs);
    assert_eq!(got.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), names);
    assert_eq!(got.columns[0].type_name, "TIMESTAMP");
    assert_eq!(got.rows.len(), ROWS);
    for (i, r) in got.rows.iter().enumerate() {
        assert_eq!(r, &rows[i], "row {i}");
    }

    // Filtered.
    let got = read(s.as_mut(), ReadSpec { table: obj("table", "plain"), columns: Some(vec!["v".into()]), filter: Some("f = true".into()) }).await;
    assert_eq!(got.rows.len(), ROWS / 2);

    // A supertable: 10 subtables sharing their instants (pages cut through them).
    let subs = 10;
    let names: Vec<String> = ["tbname", "ts", "v", "s", "loc", "gid"].iter().map(|s| s.to_string()).collect();
    let rows: Vec<Vec<Cell>> = (0..ROWS)
        .map(|i| {
            let (sub, k) = (i % subs, i / subs);
            vec![
                Cell::Text(format!("d{sub}")),
                Cell::DateTime(stamp(k as i64 * 1_000_000)),
                Cell::Int(i as i64),
                Cell::Text(format!("s{i}")),
                Cell::Text(format!("loc{sub}")),
                Cell::Int(sub as i64),
            ]
        })
        .collect();
    let spec = LoadSpec { table: obj("supertable", "st"), columns: names.clone(), ..spec };
    let t = Instant::now();
    let n = s.bulk_load(&spec, &[], &mut batches(rows.clone()), &|_| {}).await.expect("load st");
    let secs = t.elapsed().as_secs_f64();
    println!("tdengine load (supertable): {n} rows in {secs:.2}s = {:.0} rows/s", n as f64 / secs);
    let t = Instant::now();
    let got = read(s.as_mut(), ReadSpec { table: obj("supertable", "st"), columns: None, filter: None }).await;
    let secs = t.elapsed().as_secs_f64();
    println!("tdengine read (supertable): {} rows in {secs:.2}s = {:.0} rows/s", got.rows.len(), got.rows.len() as f64 / secs);
    assert_eq!(got.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), names);
    let mut sorted = got.rows.clone();
    sorted.sort_by_key(|r| match r[2] {
        Cell::Int(i) => i,
        _ => -1,
    });
    assert_eq!(sorted, rows);

    // Supertable to supertable (the migration's path), subtables recreated
    // (renamed: subtable names are unique in a database).
    let spec = LoadSpec { table: obj("supertable", "st2"), ..spec };
    let renamed = |mut r: Vec<Cell>| {
        if let Cell::Text(t) = &r[0] {
            r[0] = Cell::Text(format!("c{t}"));
        }
        r
    };
    let copied: Vec<Vec<Cell>> = got.rows.into_iter().map(renamed).collect();
    let n = s.bulk_load(&spec, &got.columns, &mut batches(copied), &|_| {}).await.expect("copy");
    assert_eq!(n as usize, ROWS);
    let back = read(s.as_mut(), ReadSpec { table: obj("supertable", "st2"), columns: None, filter: None }).await;
    let mut b = back.rows;
    let rows: Vec<Vec<Cell>> = rows.into_iter().map(renamed).collect();
    b.sort_by_key(|r| match r[2] {
        Cell::Int(i) => i,
        _ => -1,
    });
    assert_eq!(b, rows);

    // A JSON tag.
    let rows: Vec<Vec<Cell>> = (0..100)
        .map(|i| vec![Cell::Text(format!("j{}", i % 3)), Cell::DateTime(stamp(i)), Cell::Int(i), Cell::Json(format!("{{\"k\":{}}}", i % 3))])
        .collect();
    let jspec = LoadSpec { table: obj("supertable", "sj"), columns: vec!["tbname".into(), "ts".into(), "v".into(), "j".into()], ..spec.clone() };
    s.bulk_load(&jspec, &[], &mut batches(rows.clone()), &|_| {}).await.expect("json tags");
    let mut got = read(s.as_mut(), ReadSpec { table: obj("supertable", "sj"), columns: None, filter: None }).await.rows;
    got.sort_by_key(|r| match r[2] {
        Cell::Int(i) => i,
        _ => -1,
    });
    assert_eq!(got, rows);

    // Without tbname a supertable can't be loaded.
    let spec = LoadSpec { columns: vec!["ts".into(), "v".into()], ..spec };
    assert!(s.bulk_load(&spec, &[], &mut batches(vec![]), &|_| {}).await.is_err());

    s.execute(&format!("DROP DATABASE {DB}"), 10, &mut out).await.unwrap();
}

/// Rows in a table, straight from the REST API.
async fn count(table: &str) -> u64 {
    let url = format!("{}/rest/sql", std::env::var("DBINE_TEST_TDENGINE_URL").unwrap());
    let v: serde_json::Value = reqwest::Client::new()
        .post(url)
        .basic_auth("root", Some("taosdata"))
        .body(format!("SELECT COUNT(*) FROM {table}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    v["data"][0][0].as_u64().unwrap_or(0)
}

/// Endless batches of `(ts, v, s)` rows, 1 ms apart.
struct Endless(i64);

#[async_trait]
impl BatchSource for Endless {
    async fn next(&mut self) -> Option<RowBatch> {
        let rows = (0..1_000)
            .map(|_| {
                self.0 += 1;
                vec![Cell::DateTime(stamp(self.0 * 1_000)), Cell::Int(self.0), Cell::Text(format!("fila {} {}", self.0, "x".repeat(40)))]
            })
            .collect();
        tokio::task::yield_now().await;
        Some(RowBatch { rows, bytes: 0 })
    }
}

/// The database of `transfer_tdengine_faithful` (its own: the tests run in
/// parallel, and one dropping the other's database breaks it).
const FAITH: &str = "dbine_transfer_f";

fn load_spec(kind: &str, table: &str, columns: &[&str]) -> LoadSpec {
    LoadSpec {
        table: obj_in(FAITH, kind, table),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: 1,
        commit_bytes: 1 << 29,
    }
}

/// What a load leaves behind on errors and cancels, and values that can't
/// travel faithfully.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_tdengine_faithful() {
    const FDB: &str = "dbine_transfer_faithful";
    let Some(c) = cfg() else { return };
    let d = dbine_driver_tdengine::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        &format!("DROP DATABASE IF EXISTS {FAITH}; CREATE DATABASE {FAITH} PRECISION 'us'; DROP DATABASE IF EXISTS {FDB}; CREATE DATABASE {FDB} PRECISION 'ms'"),
        10,
        &mut out,
    )
    .await
    .unwrap();
    let mut s = d.connect(&c, Some(FAITH)).await.unwrap();
    s.execute(
        &format!(
            "CREATE TABLE e (ts TIMESTAMP, v INT, s VARCHAR(60));
             CREATE TABLE k (ts TIMESTAMP, v INT, s VARCHAR(60));
             CREATE TABLE fl (ts TIMESTAMP, f FLOAT, d DOUBLE);
             CREATE TABLE fl2 (ts TIMESTAMP, f FLOAT, d DOUBLE);
             CREATE STABLE sw (ts TIMESTAMP, v INT, s VARCHAR(60000)) TAGS (g INT);
             CREATE STABLE sp (ts TIMESTAMP, v INT) TAGS (g INT);
             CREATE TABLE {FDB}.ms (ts TIMESTAMP, v INT)"
        ),
        10,
        &mut out,
    )
    .await
    .unwrap();
    let last = Arc::new(Mutex::new(0u64));
    let report = |l: Arc<Mutex<u64>>| move |n: u64| *l.lock().unwrap() = n;

    // 1a. A load that fails half-way: what it reports is what's in the
    // table, and nothing lands after it returned.
    let mut rows: Vec<Vec<Cell>> =
        (0..30_000i64).map(|i| vec![Cell::DateTime(stamp(i * 1_000)), Cell::Int(i), Cell::Text(format!("fila {i} {}", "x".repeat(40)))]).collect();
    rows.push(vec![Cell::DateTime(stamp(40_000_000)), Cell::Int(-1)]);
    let r = s.bulk_load(&load_spec("table", "e", &["ts", "v", "s"]), &[], &mut batches(rows), &report(last.clone())).await;
    assert!(r.is_err());
    let now = count(&format!("{FAITH}.e")).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(count(&format!("{FAITH}.e")).await, now, "rows committed after the load returned");
    assert_eq!(*last.lock().unwrap(), now, "progress must be the committed rows");
    println!("failed load: {now} rows committed and reported");

    // 1b. A load dropped half-way (the orchestrator's cancel): the same.
    for ms in [400, 700] {
        s.execute(&format!("DELETE FROM {FAITH}.k"), 10, &mut out).await.unwrap();
        *last.lock().unwrap() = 0;
        let spec = load_spec("table", "k", &["ts", "v", "s"]);
        let cb = report(last.clone());
        let mut src = Endless(0);
        let load = s.bulk_load(&spec, &[], &mut src, &cb);
        assert!(tokio::time::timeout(std::time::Duration::from_millis(ms), load).await.is_err());
        let now = count(&format!("{FAITH}.k")).await;
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(count(&format!("{FAITH}.k")).await, now, "rows committed after the load was dropped ({ms} ms)");
        assert_eq!(*last.lock().unwrap(), now, "progress must be the committed rows ({ms} ms)");
        println!("dropped load ({ms} ms): {now} rows committed and reported");
    }

    // 1c. Cancelled through the interrupter: stops, waits, reports.
    s.execute(&format!("DELETE FROM {FAITH}.k"), 10, &mut out).await.unwrap();
    let stop = s.interrupter().expect("interrupter");
    let t = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        stop();
    });
    let r = s.bulk_load(&load_spec("table", "k", &["ts", "v", "s"]), &[], &mut Endless(0), &report(last.clone())).await;
    t.await.unwrap();
    assert!(matches!(r, Err(dbine_driver::Error::Cancelled)), "{r:?}");
    let now = count(&format!("{FAITH}.k")).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(count(&format!("{FAITH}.k")).await, now);
    assert_eq!(*last.lock().unwrap(), now);

    // 2. A supertable whose subtables share their instants, with a wide
    // column: pages are small and one instant spans several of them.
    let subs = 100;
    let rows: Vec<Vec<Cell>> = (0..3 * subs)
        .map(|i| vec![Cell::Text(format!("w{}", i % subs)), Cell::DateTime(stamp((i / subs) as i64 * 1_000_000)), Cell::Int(i as i64), Cell::Text(format!("s{i}")), Cell::Int((i % subs) as i64)])
        .collect();
    s.bulk_load(&load_spec("supertable", "sw", &["tbname", "ts", "v", "s", "g"]), &[], &mut batches(rows.clone()), &|_| {}).await.expect("load sw");
    let mut got = read(s.as_mut(), ReadSpec { table: obj_in(FAITH, "supertable", "sw"), columns: None, filter: None }).await.rows;
    got.sort_by_key(|r| match r[2] {
        Cell::Int(i) => i,
        _ => -1,
    });
    assert_eq!(got, rows);
    // An unknown column is an error, not an empty one.
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table: obj_in(FAITH, "supertable", "sw"), columns: Some(vec!["v".into(), "nope".into()]), filter: None };
    assert!(s.read_batches(&spec, sink).await.is_err());

    // 2b. A failed load into a supertable whose rows span many subtables
    // (vgroups): a statement that fails commits none of its rows, so what's
    // reported is what's in the table. One row, for a new subtable, is
    // dated outside the database's KEEP.
    let subs = 50;
    let mut rows: Vec<Vec<Cell>> =
        (0..1_000i64).map(|i| vec![Cell::Text(format!("p{}", i % subs)), Cell::DateTime(stamp(i * 1_000)), Cell::Int(i), Cell::Int(i % subs)]).collect();
    rows.insert(500, vec![Cell::Text("pold".into()), Cell::DateTime("1971-01-01 00:00:00".into()), Cell::Int(-1), Cell::Int(-1)]);
    *last.lock().unwrap() = 0;
    let r = s.bulk_load(&load_spec("supertable", "sp", &["tbname", "ts", "v", "g"]), &[], &mut batches(rows), &report(last.clone())).await;
    assert!(r.is_err(), "{r:?}");
    let now = count(&format!("{FAITH}.sp")).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(count(&format!("{FAITH}.sp")).await, now, "rows committed after the load returned");
    assert_eq!(*last.lock().unwrap(), now, "progress must be the committed rows");
    // Whole subtables (20 rows each) or none of them.
    assert_eq!(now % 20, 0, "{now}");
    println!("failed supertable load: {now} rows committed and reported");

    // 3. FLOAT extremes: read exactly, and loadable again (TDengine to
    // TDengine).
    let floats = [f32::MAX, f32::MIN, f32::from_bits(1), 0.1, -1.5e-38, 16_777_217.0];
    let rows: Vec<Vec<Cell>> =
        floats.iter().enumerate().map(|(i, f)| vec![Cell::DateTime(stamp(i as i64)), Cell::Float(f64::from(*f)), Cell::Float(f64::from(*f) / 3.0)]).collect();
    s.bulk_load(&load_spec("table", "fl", &["ts", "f", "d"]), &[], &mut batches(rows.clone()), &|_| {}).await.expect("load fl");
    let got = read(s.as_mut(), ReadSpec { table: obj_in(FAITH, "table", "fl"), columns: None, filter: None }).await;
    assert_eq!(got.rows, rows);
    s.bulk_load(&load_spec("table", "fl2", &["ts", "f", "d"]), &got.columns, &mut batches(got.rows.clone()), &|_| {}).await.expect("copy fl");
    assert_eq!(read(s.as_mut(), ReadSpec { table: obj_in(FAITH, "table", "fl2"), columns: None, filter: None }).await.rows, rows);

    // 4. Instants finer than the target's precision: refused, nothing
    // collapses.
    let rows: Vec<Vec<Cell>> = (0..1_000).map(|i| vec![Cell::DateTime(stamp(i)), Cell::Int(i)]).collect();
    let spec = LoadSpec { table: ObjectRef { kind: "table".into(), schema: Some(FDB.into()), name: "ms".into() }, ..load_spec("table", "ms", &["ts", "v"]) };
    let r = s.bulk_load(&spec, &[], &mut batches(rows), &|_| {}).await;
    assert!(matches!(r, Err(dbine_driver::Error::Unsupported(_))), "{r:?}");
    assert_eq!(count(&format!("{FDB}.ms")).await, 0);

    // 5. NaN and infinities: refused, not stored as NULL.
    for bad in [f64::NAN, f64::INFINITY] {
        let rows = vec![vec![Cell::DateTime(stamp(99_000_000)), Cell::Float(1.0), Cell::Float(bad)]];
        let r = s.bulk_load(&load_spec("table", "fl2", &["ts", "f", "d"]), &[], &mut batches(rows), &|_| {}).await;
        assert!(matches!(r, Err(dbine_driver::Error::Unsupported(_))), "{r:?}");
    }
    assert_eq!(count(&format!("{FAITH}.fl2")).await, floats.len() as u64);

    s.execute(&format!("DROP DATABASE {FAITH}; DROP DATABASE {FDB}"), 10, &mut out).await.unwrap();
}

/// `n` rows of `(ts, s)`, `s` 60,000 control characters (6 bytes each in a
/// REST answer), made as they're asked for.
struct Escaped(usize, usize);

#[async_trait]
impl BatchSource for Escaped {
    async fn next(&mut self) -> Option<RowBatch> {
        if self.0 >= self.1 {
            return None;
        }
        let rows = (self.0..(self.0 + 10).min(self.1)).map(|i| vec![Cell::DateTime(stamp(i as i64 * 1_000)), Cell::Text("\u{1}".repeat(60_000))]).collect();
        self.0 = (self.0 + 10).min(self.1);
        Some(RowBatch { rows, bytes: 0 })
    }
}

/// Checks each row and keeps none of them.
#[derive(Default)]
struct Check(usize);

impl BatchSink for Check {
    fn begin(&mut self, _: &[TransferColumn]) -> std::io::Result<()> {
        Ok(())
    }
    fn batch(&mut self, batch: RowBatch) -> std::io::Result<()> {
        for r in batch.rows {
            assert_eq!(r[1], Cell::Text("\u{1}".repeat(60_000)));
            self.0 += 1;
        }
        Ok(())
    }
}

/// Wide text of control characters, escaped in the REST answer: read in
/// pages small enough for it (peak memory: run it under `/usr/bin/time -l`).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_tdengine_escaped() {
    const EDB: &str = "dbine_transfer_esc";
    let Some(c) = cfg() else { return };
    let d = dbine_driver_tdengine::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&format!("DROP DATABASE IF EXISTS {EDB}; CREATE DATABASE {EDB} PRECISION 'us'"), 10, &mut out).await.unwrap();
    let mut s = d.connect(&c, Some(EDB)).await.unwrap();
    s.execute("CREATE TABLE w (ts TIMESTAMP, s VARCHAR(60000))", 10, &mut out).await.unwrap();
    let spec = LoadSpec { table: obj_in(EDB, "table", "w"), ..load_spec("table", "w", &["ts", "s"]) };
    let n = s.bulk_load(&spec, &[], &mut Escaped(0, 400), &|_| {}).await.expect("load");
    assert_eq!(n, 400);
    let sink = Arc::new(Mutex::new(Check::default()));
    let n = s.read_batches(&ReadSpec { table: obj_in(EDB, "table", "w"), columns: None, filter: None }, sink.clone()).await.expect("read");
    assert_eq!((n, sink.lock().unwrap().0), (400, 400));
    s.execute(&format!("DROP DATABASE {EDB}"), 10, &mut out).await.unwrap();
}
