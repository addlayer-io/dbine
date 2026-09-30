//! Bulk transfer against a real Dremio OSS (ignored by default; container
//! and first user as in `integration.rs`), into Iceberg tables in `$scratch`:
//!
//! ```sh
//! DBINE_TEST_DREMIO_URL=http://localhost:25947 \
//!   cargo test -p dbine-driver-dremio -- --ignored transfer --nocapture
//! ```

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{async_trait, kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session};
use serde_json::json;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ROWS: usize = 100_000;
const USER: &str = "dbine";
const PASS: &str = "secreto123";

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
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_DREMIO_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "dremio".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(USER.into()),
        password: Some(PASS.into()),
        ..Default::default()
    })
}

/// A fresh Dremio has no users: create the first one (ignored if it exists).
async fn bootstrap(c: &ConnectionConfig) {
    let http = reqwest::Client::new();
    let base = format!("http://{}:{}", c.host, c.port);
    for _ in 0..60 {
        if http.get(format!("{base}/apiv2/server_status")).send().await.map(|r| r.status().is_success()).unwrap_or(false) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let _ = http
        .put(format!("{base}/apiv2/bootstrap/firstuser"))
        .header("Authorization", "_dremionull")
        .json(&json!({"userName": USER, "firstName": "DB", "lastName": "Ine", "email": "dbine@example.com", "createdAt": 1700000000000u64, "password": PASS}))
        .send()
        .await;
}

fn batches(rows: Vec<Vec<Cell>>) -> Batches {
    let v: Vec<RowBatch> = rows.chunks(1_000).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect();
    Batches(v.into_iter())
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::TABLE.into(), schema: Some("$scratch".into()), name: name.into() }
}

async fn read(s: &mut dyn Session, spec: ReadSpec) -> Collect {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let n = s.read_batches(&spec, sink.clone()).await.expect("read");
    let c = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(n as usize, c.rows.len());
    c
}

fn by_id(rows: &mut [Vec<Cell>]) {
    rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => i64::MIN,
    });
}

const COLS: [&str; 11] = ["id", "v", "d", "s", "b", "dt", "ts", "t", "ok", "f", "i"];

fn row(i: usize) -> Vec<Cell> {
    let n = i as i64;
    vec![
        Cell::Int(9_007_199_254_740_993 + n),
        Cell::Float(n as f64 / 3.0),
        Cell::Decimal(format!("{}{}.{:04}", if i % 2 == 1 { "-" } else { "" }, 12_345_678_901_234i64 + n, i % 10_000)),
        if i.is_multiple_of(7) { Cell::Null } else { Cell::Text(format!("fila {i} ñ'\"\\")) },
        Cell::Bytes(vec![(i % 256) as u8, 0, 0xFF]),
        Cell::Date(format!("2024-{:02}-{:02}", i % 12 + 1, i % 28 + 1)),
        Cell::DateTime(format!("2024-01-{:02} {:02}:{:02}:{:02}.{:03}", i % 28 + 1, i % 24, i % 60, (i / 60) % 60, i % 1000)),
        Cell::Time(format!("{:02}:{:02}:{:02}.{:03}", i % 24, i % 60, (i / 7) % 60, i % 1000)),
        Cell::Bool(i.is_multiple_of(2)),
        Cell::Float(n as f64 * 0.5),
        Cell::Int(-n),
    ]
}

fn spec(name: &str) -> LoadSpec {
    LoadSpec {
        table: table(name),
        columns: COLS.iter().map(|s| s.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: 20_000,
        commit_bytes: 1 << 29,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_dremio() {
    let Some(c) = cfg() else { return };
    bootstrap(&c).await;
    let d = dbine_driver_dremio::drivers().remove(0);
    assert!(d.supports_bulk_load());
    let mut s = d.connect(&c, Some("$scratch")).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP TABLE IF EXISTS \"$scratch\".dbine_tr; DROP TABLE IF EXISTS \"$scratch\".dbine_tr2;
         CREATE TABLE \"$scratch\".dbine_tr (id BIGINT, v DOUBLE, d DECIMAL(24,4), s VARCHAR, b VARBINARY, dt DATE, ts TIMESTAMP, t TIME, ok BOOLEAN, f FLOAT, i INT);
         CREATE TABLE \"$scratch\".dbine_tr2 (id BIGINT, v DOUBLE, d DECIMAL(24,4), s VARCHAR, b VARBINARY, dt DATE, ts TIMESTAMP, t TIME, ok BOOLEAN, f FLOAT, i INT)",
        10,
        &mut out,
    )
    .await
    .unwrap();

    let rows: Vec<Vec<Cell>> = (0..ROWS).map(row).collect();
    let reports = Mutex::new(Vec::new());
    let t = Instant::now();
    let n = s.bulk_load(&spec("dbine_tr"), &[], &mut batches(rows.clone()), &|n| reports.lock().unwrap().push(n)).await.expect("load");
    let secs = t.elapsed().as_secs_f64();
    println!("dremio load: {n} rows in {secs:.2}s = {:.0} rows/s", n as f64 / secs);
    assert_eq!(n as usize, ROWS);
    let reports = reports.into_inner().unwrap();
    assert!(reports.len() >= 4 && reports.windows(2).all(|w| w[0] < w[1]) && *reports.last().unwrap() == ROWS as u64, "{reports:?}");

    let t = Instant::now();
    let mut got = read(s.as_mut(), ReadSpec { table: table("dbine_tr"), columns: None, filter: None }).await;
    let secs = t.elapsed().as_secs_f64();
    println!("dremio read: {} rows in {secs:.2}s = {:.0} rows/s", got.rows.len(), got.rows.len() as f64 / secs);
    assert_eq!(got.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), COLS);
    assert_eq!(got.columns[2].type_name, "DECIMAL(24,4)");
    by_id(&mut got.rows);
    assert_eq!(got.rows.len(), ROWS);
    for (i, r) in got.rows.iter().enumerate() {
        assert_eq!(r, &rows[i], "row {i}");
    }

    // Filtered, some columns.
    let f = read(s.as_mut(), ReadSpec { table: table("dbine_tr"), columns: Some(vec!["id".into(), "d".into()]), filter: Some("ok".into()) }).await;
    assert_eq!(f.rows.len(), ROWS / 2);

    // Table to table, with the read's columns.
    let t = Instant::now();
    let n = s.bulk_load(&spec("dbine_tr2"), &got.columns, &mut batches(got.rows), &|_| {}).await.expect("copy");
    println!("dremio copy: {n} rows in {:.2}s", t.elapsed().as_secs_f64());
    let mut back = read(s.as_mut(), ReadSpec { table: table("dbine_tr2"), columns: None, filter: None }).await.rows;
    by_id(&mut back);
    assert_eq!(back, rows);

    // Zoned instants land as UTC; text into a binary column goes as its bytes.
    s.execute("DELETE FROM \"$scratch\".dbine_tr2", 10, &mut out).await.unwrap();
    let odd = vec![vec![
        Cell::Int(1),
        Cell::Float(f64::INFINITY),
        Cell::Null,
        Cell::Text(String::new()),
        Cell::Text("ab".into()),
        Cell::Null,
        Cell::DateTimeTz("2024-01-31 22:30:00.5-03:00".into()),
        Cell::Null,
        Cell::Null,
        Cell::Null,
        Cell::Null,
    ]];
    s.bulk_load(&spec("dbine_tr2"), &[], &mut batches(odd), &|_| {}).await.expect("odd");
    let back = read(s.as_mut(), ReadSpec { table: table("dbine_tr2"), columns: None, filter: None }).await.rows;
    assert_eq!(
        back,
        vec![vec![
            Cell::Int(1),
            Cell::Float(f64::INFINITY),
            Cell::Null,
            Cell::Text(String::new()),
            Cell::Bytes(b"ab".to_vec()),
            Cell::Null,
            Cell::DateTime("2024-02-01 01:30:00.500".into()),
            Cell::Null,
            Cell::Null,
            Cell::Null,
            Cell::Null,
        ]]
    );

    s.execute("DROP TABLE \"$scratch\".dbine_tr; DROP TABLE \"$scratch\".dbine_tr2", 10, &mut out).await.unwrap();
}

async fn count(s: &mut dyn Session, name: &str) -> usize {
    read(s, ReadSpec { table: table(name), columns: Some(vec!["id".into()]), filter: None }).await.rows.len()
}

fn spec2(name: &str, cols: &[&str]) -> LoadSpec {
    LoadSpec { columns: cols.iter().map(|s| s.to_string()).collect(), ..spec(name) }
}

/// Values the planner or the text casts used to mangle: text outside
/// Latin-1, decimals Dremio writes with an exponent, FLOAT bits, -0.0.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_dremio_fidelity() {
    let Some(c) = cfg() else { return };
    bootstrap(&c).await;
    let d = dbine_driver_dremio::drivers().remove(0);
    let mut s = d.connect(&c, Some("$scratch")).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP TABLE IF EXISTS \"$scratch\".dbine_trf;
         CREATE TABLE \"$scratch\".dbine_trf (id BIGINT, s VARCHAR, d DECIMAL(38,10), f FLOAT, v DOUBLE)",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let rows = vec![
        vec![Cell::Int(1), Cell::Text("😀 a".into()), Cell::Decimal("0.0000000001".into()), Cell::Float(0.1f32 as f64), Cell::Float(-0.0)],
        vec![Cell::Int(2), Cell::Text("Ω 中".into()), Cell::Decimal("-0.0000001000".into()), Cell::Float(f32::MAX as f64), Cell::Float(0.1)],
        vec![Cell::Int(3), Cell::Text("𝄞 ñ '".into()), Cell::Decimal("0.0000000000".into()), Cell::Float(1.4e-45f32 as f64), Cell::Float(-1e-300)],
        vec![Cell::Int(4), Cell::Null, Cell::Decimal("-12345678901234567890.1234567890".into()), Cell::Float(-0.0), Cell::Null],
    ];
    let n = s.bulk_load(&spec2("dbine_trf", &["id", "s", "d", "f", "v"]), &[], &mut batches(rows.clone()), &|_| {}).await.expect("load");
    assert_eq!(n, 4);
    let mut back = read(s.as_mut(), ReadSpec { table: table("dbine_trf"), columns: None, filter: None }).await.rows;
    by_id(&mut back);
    assert_eq!(back.len(), 4);
    for (got, want) in back.iter().zip(&rows) {
        assert_eq!(got[..4], want[..4], "{got:?}");
        match (&got[4], &want[4]) {
            (Cell::Float(a), Cell::Float(b)) => assert_eq!(a.to_bits(), b.to_bits(), "{a} vs {b}"),
            (a, b) => assert_eq!(a, b),
        }
    }
    // -0.0 into FLOAT keeps its sign too.
    assert!(matches!(back[3][3], Cell::Float(f) if f == 0.0 && f.is_sign_negative()), "{:?}", back[3][3]);
    s.execute("DROP TABLE \"$scratch\".dbine_trf", 10, &mut out).await.unwrap();
}

/// A statement whose text (or binary, decimal, date...) column is NULL in
/// every row: Calcite types that VALUES column NULL, and the text decode
/// must still be accepted (one row, a whole statement, a short last one).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_dremio_all_null_columns() {
    let Some(c) = cfg() else { return };
    bootstrap(&c).await;
    let d = dbine_driver_dremio::drivers().remove(0);
    let mut s = d.connect(&c, Some("$scratch")).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP TABLE IF EXISTS \"$scratch\".dbine_trn;
         CREATE TABLE \"$scratch\".dbine_trn (id BIGINT, s VARCHAR, b VARBINARY, d DECIMAL(10,2), f DOUBLE, t TIMESTAMP, k BOOLEAN)",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let cols = ["id", "s", "b", "d", "f", "t", "k"];
    let nulls = |id: i64| {
        let mut r = vec![Cell::Int(id)];
        r.extend(std::iter::repeat_n(Cell::Null, 6));
        r
    };
    // One row, then a full statement of NULLs.
    let n = s.bulk_load(&spec2("dbine_trn", &cols), &[], &mut batches(vec![nulls(1)]), &|_| {}).await.expect("one row");
    assert_eq!(n, 1);
    let many: Vec<Vec<Cell>> = (2..=3_000).map(nulls).collect();
    let n = s.bulk_load(&spec2("dbine_trn", &cols), &[], &mut batches(many), &|_| {}).await.expect("all NULL");
    assert_eq!(n, 2_999);
    // Tagged text next to a NULL-only binary column.
    let mixed = vec![
        vec![Cell::Int(3_001), Cell::Text("Ω".into()), Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null],
        vec![Cell::Int(3_002), Cell::Text("ñ".into()), Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null],
    ];
    let n = s.bulk_load(&spec2("dbine_trn", &cols), &[], &mut batches(mixed.clone()), &|_| {}).await.expect("mixed");
    assert_eq!(n, 2);
    let mut back = read(s.as_mut(), ReadSpec { table: table("dbine_trn"), columns: None, filter: None }).await.rows;
    by_id(&mut back);
    assert_eq!(back.len(), 3_002);
    assert_eq!(back[0], nulls(1));
    assert_eq!(back[2_998], nulls(2_999));
    assert_eq!(back[3_000..], mixed[..]);
    s.execute("DROP TABLE \"$scratch\".dbine_trn", 10, &mut out).await.unwrap();

    // STRUCT/LIST/MAP targets: refused before anything is written.
    s.execute(
        "DROP TABLE IF EXISTS \"$scratch\".dbine_trc; CREATE TABLE \"$scratch\".dbine_trc (id BIGINT, l LIST<VARCHAR>)",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let r = s.bulk_load(&spec2("dbine_trc", &["id", "l"]), &[], &mut batches(vec![vec![Cell::Int(1), Cell::Json("[\"a\"]".into())]]), &|_| {}).await;
    assert!(matches!(r, Err(dbine_driver::Error::Unsupported(_))), "{r:?}");
    assert_eq!(count(s.as_mut(), "dbine_trc").await, 0);
    s.execute("DROP TABLE \"$scratch\".dbine_trc", 10, &mut out).await.unwrap();
}

/// One INSERT statement's rows (the driver's 5,000), then either the end
/// or a source that never answers. `stage` is 1 once the last row was
/// handed out (the INSERT is being run) and 2 once the load asked for
/// more (the INSERT returned), so a test knows it stopped the load while
/// that INSERT was in flight.
struct Gated {
    rows: std::vec::IntoIter<RowBatch>,
    hold: bool,
    stage: Arc<std::sync::atomic::AtomicU8>,
}

#[async_trait]
impl BatchSource for Gated {
    async fn next(&mut self) -> Option<RowBatch> {
        use std::sync::atomic::Ordering::SeqCst;
        match self.rows.next() {
            Some(b) => {
                if self.rows.len() == 0 {
                    self.stage.store(1, SeqCst);
                }
                Some(b)
            }
            None => {
                self.stage.store(2, SeqCst);
                if self.hold {
                    std::future::pending::<()>().await;
                }
                None
            }
        }
    }
}

async fn wait_stage(stage: &std::sync::atomic::AtomicU8, at: u8) {
    while stage.load(std::sync::atomic::Ordering::SeqCst) < at {
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// A load dropped mid-INSERT (the run's cancel, or a failed read) or
/// interrupted must not leave rows that get committed after it returned.
/// Each case stops the load while its INSERT job is in flight (checked).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_dremio_no_late_commit() {
    use std::sync::atomic::{AtomicU8, Ordering::SeqCst};
    let Some(c) = cfg() else { return };
    bootstrap(&c).await;
    let d = dbine_driver_dremio::drivers().remove(0);
    let mut s = d.connect(&c, Some("$scratch")).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP TABLE IF EXISTS \"$scratch\".dbine_trl; CREATE TABLE \"$scratch\".dbine_trl (id BIGINT, s VARCHAR)",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let rows: Vec<Vec<Cell>> = (0..5_000).map(|i| vec![Cell::Int(i), Cell::Text(format!("fila {i}"))]).collect();
    let spec = spec2("dbine_trl", &["id", "s"]);
    let gated = |hold: bool, stage: &Arc<AtomicU8>| Gated {
        rows: rows.chunks(1_000).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect::<Vec<_>>().into_iter(),
        hold,
        stage: stage.clone(),
    };

    // Dropped: right after the INSERT was started (before or while its job
    // is submitted) and while the job runs.
    let mut in_flight = 0;
    for ms in [0u64, 150, 400] {
        let stage = Arc::new(AtomicU8::new(0));
        let mut src = gated(true, &stage);
        {
            let load = s.bulk_load(&spec, &[], &mut src, &|_| {});
            tokio::select! {
                _ = load => panic!("the load can't end: its source never does"),
                _ = async { wait_stage(&stage, 1).await; tokio::time::sleep(Duration::from_millis(ms)).await } => {}
            }
        }
        let flying = stage.load(SeqCst) == 1;
        in_flight += usize::from(flying);
        let now = count(s.as_mut(), "dbine_trl").await;
        tokio::time::sleep(Duration::from_secs(15)).await;
        let later = count(s.as_mut(), "dbine_trl").await;
        println!("dropped {ms} ms into the INSERT (in flight: {flying}): {now} rows, {later} 15 s later");
        assert_eq!(now, later, "rows committed after the dropped load returned");
        s.execute("DELETE FROM \"$scratch\".dbine_trl", 10, &mut out).await.unwrap();
    }
    assert!(in_flight >= 2, "only {in_flight} drops hit a running INSERT");

    // Interrupted while the INSERT runs.
    let mut interrupted_in_flight = false;
    for ms in [50u64, 250] {
        let stage = Arc::new(AtomicU8::new(0));
        let mut src = gated(false, &stage);
        let stop = s.interrupter().expect("interrupter");
        let st = stage.clone();
        let t = tokio::spawn(async move {
            wait_stage(&st, 1).await;
            tokio::time::sleep(Duration::from_millis(ms)).await;
            let flying = st.load(SeqCst) == 1;
            stop();
            flying
        });
        let reported = std::sync::atomic::AtomicU64::new(0);
        let r = s.bulk_load(&spec, &[], &mut src, &|n| reported.store(n, SeqCst)).await;
        let reported = reported.load(SeqCst) as usize;
        let flying = t.await.unwrap();
        interrupted_in_flight |= flying;
        let now = count(s.as_mut(), "dbine_trl").await;
        tokio::time::sleep(Duration::from_secs(15)).await;
        let later = count(s.as_mut(), "dbine_trl").await;
        println!("interrupted {ms} ms into the INSERT (in flight: {flying}): {r:?}, {now} rows ({reported} reported), {later} 15 s later");
        assert_eq!(now, later, "rows committed after the interrupted load returned");
        // Cancelled (the job's rows, if it won the race, reported), or
        // ended with exactly the rows it returned.
        assert_eq!(now, reported, "{r:?}");
        assert!(matches!(r, Err(dbine_driver::Error::Cancelled)) || r.as_ref().is_ok_and(|n| *n as usize == now), "{r:?}");
        s.execute("DELETE FROM \"$scratch\".dbine_trl", 10, &mut out).await.unwrap();
    }
    assert!(interrupted_in_flight, "no interrupt hit a running INSERT");

    s.execute("DROP TABLE \"$scratch\".dbine_trl", 10, &mut out).await.unwrap();
}
