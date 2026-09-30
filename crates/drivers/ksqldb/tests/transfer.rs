//! Bulk load and typed read against a real ksqlDB (see `integration.rs` for
//! the containers):
//! `DBINE_TEST_KSQLDB_URL=http://localhost:25188 \
//!  cargo test -p dbine-driver-ksqldb --release -- --ignored transfer --nocapture`.

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{kinds, ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const ROWS: usize = 50_000;

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_KSQLDB_URL").ok()?).expect("URL");
    Some(ConnectionConfig { driver: "ksqldb".into(), host: url.host_str()?.into(), port: url.port().unwrap_or(0), ..Default::default() })
}

/// A topic name this run has not used: `DROP … DELETE TOPIC` deletes the
/// old topic in the background, and a `CREATE` that reuses its name right
/// after can find it half gone ("Kafka topic does not exist").
fn topic(base: &str) -> String {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{base}_{}_{nanos}", std::process::id())
}

/// One test at a time: ksqlDB writes each DDL to its command topic in a
/// Kafka transaction, and concurrent ones fail ("Could not write the
/// statement into the command topic", "transactional method … error state").
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(text, 10, &mut out).await {
        panic!("{text}: {e}");
    }
}

struct Batches(std::vec::IntoIter<RowBatch>);

#[dbine_driver::async_trait]
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
    fn batch(&mut self, b: RowBatch) -> std::io::Result<()> {
        self.rows.extend(b.rows);
        Ok(())
    }
}

fn row(i: usize) -> Vec<Cell> {
    let n = i as i64;
    vec![
        Cell::Int(n),
        if i.is_multiple_of(7) { Cell::Null } else { Cell::Int(n * 1_000_003) },
        Cell::Decimal(format!("{}{:04}.{:04}", n + 1, i % 10_000, i % 9999 + 1)),
        Cell::Float(i as f64 / 8.0),
        Cell::Text(format!("fila {i} ñ \"x\", y")),
        Cell::Bytes(vec![(i % 256) as u8; i % 40 + 1]),
        Cell::Date(format!("2024-{:02}-{:02}", i % 12 + 1, i % 28 + 1)),
        Cell::Time(format!("{:02}:{:02}:{:02}", i % 24, i % 59 + 1, i % 59 + 1)),
        Cell::DateTime(format!("2024-01-{:02} 10:00:00.{:03}", i % 28 + 1, i % 999 + 1)),
        Cell::Bool(i.is_multiple_of(2)),
        Cell::Json(format!("[{i},{}]", i + 1)),
    ]
}

async fn cleanup(s: &mut Box<dyn Session>) {
    let mut out = QueryOutcome::default();
    let _ = s.execute("DROP STREAM IF EXISTS DBINE_XFER DELETE TOPIC; DROP TABLE IF EXISTS DBINE_XFER_T DELETE TOPIC;", 10, &mut out).await;
}

#[tokio::test]
#[ignore]
async fn ksqldb_transfer() {
    let Some(c) = cfg() else { return };
    let _serial = SERIAL.lock().await;
    let d = dbine_driver_ksqldb::drivers().remove(0);
    assert!(d.supports_bulk_load());
    let mut s = d.connect(&c, None).await.unwrap();
    cleanup(&mut s).await;
    run(
        &mut s,
        &format!(
            "CREATE STREAM DBINE_XFER (ID BIGINT KEY, N BIGINT, D DECIMAL(24, 4), F DOUBLE, S STRING, B BYTES, DT DATE, TM TIME,
                                  TS TIMESTAMP, OK BOOLEAN, L ARRAY<INTEGER>)
           WITH (KAFKA_TOPIC='{}', PARTITIONS=1, VALUE_FORMAT='JSON');",
            topic("dbine_xfer")
        ),
    )
    .await;
    let names = ["ID", "N", "D", "F", "S", "B", "DT", "TM", "TS", "OK", "L"];
    let obj = ObjectRef { kind: kinds::STREAM.into(), schema: None, name: "DBINE_XFER".into() };
    let batches: Vec<RowBatch> =
        (0..ROWS).collect::<Vec<_>>().chunks(1000).map(|c| RowBatch { rows: c.iter().map(|i| row(*i)).collect(), bytes: 0 }).collect();
    let spec = LoadSpec {
        table: obj.clone(),
        columns: names.iter().map(|n| n.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: 10_000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let reports = Mutex::new(Vec::new());
    let t = Instant::now();
    let loaded = s.bulk_load(&spec, &[], &mut Batches(batches.into_iter()), &|n| reports.lock().unwrap().push(n)).await.unwrap();
    let secs = t.elapsed().as_secs_f64();
    println!("ksqldb: loaded {loaded} rows in {secs:.2}s = {:.0} rows/s", loaded as f64 / secs);
    assert_eq!(loaded, ROWS as u64);
    // Every 10,000 rows or more (requests don't end on round numbers), and the total.
    let reports = reports.into_inner().unwrap();
    assert_eq!(reports.last(), Some(&(ROWS as u64)), "{reports:?}");
    assert!(reports.windows(2).all(|w| w[1] - w[0] >= 10_000 || w[1] == ROWS as u64), "{reports:?}");

    let sink = Arc::new(Mutex::new(Collect::default()));
    let t = Instant::now();
    let read = s.read_batches(&ReadSpec { table: obj.clone(), columns: None, filter: None }, sink.clone()).await.unwrap();
    let secs = t.elapsed().as_secs_f64();
    println!("ksqldb: read {read} rows in {secs:.2}s = {:.0} rows/s", read as f64 / secs);
    assert_eq!(read, ROWS as u64);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(got.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), names);
    // One partition: the rows come back in the order they went in.
    for (i, r) in got.rows.iter().enumerate() {
        assert_eq!(r, &row(i), "row {i}");
    }

    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec_r = ReadSpec { table: obj.clone(), columns: Some(vec!["ID".into(), "S".into()]), filter: Some("ID IN (1, 2, 3)".into()) };
    assert_eq!(s.read_batches(&spec_r, sink).await.unwrap(), 3);

    // A value the column doesn't take is an error.
    let mut bad = row(0);
    bad[1] = Cell::Text("no es número".into());
    assert!(s.bulk_load(&spec, &[], &mut Batches(vec![RowBatch { rows: vec![bad], bytes: 0 }].into_iter()), &|_| {}).await.is_err());

    // Tables load too; a plain CREATE TABLE only answers push queries.
    run(&mut s, &format!("CREATE TABLE DBINE_XFER_T (ID BIGINT PRIMARY KEY, V STRING) WITH (KAFKA_TOPIC='{}', PARTITIONS=1, VALUE_FORMAT='JSON');", topic("dbine_xfer_t"))).await;
    let t = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "DBINE_XFER_T".into() };
    let spec_t = LoadSpec { table: t.clone(), columns: vec!["ID".into(), "V".into()], ..spec.clone() };
    let rows = RowBatch { rows: (0..100).map(|i| vec![Cell::Int(i), Cell::Text(format!("v{i}"))]).collect(), bytes: 0 };
    assert_eq!(s.bulk_load(&spec_t, &[], &mut Batches(vec![rows].into_iter()), &|_| {}).await.unwrap(), 100);
    let sink = Arc::new(Mutex::new(Collect::default()));
    match s.read_batches(&ReadSpec { table: t, columns: None, filter: None }, sink).await {
        Err(Error::Unsupported(m)) => assert!(m.contains("consultables"), "{m}"),
        r => panic!("{r:?}"),
    }
    cleanup(&mut s).await;
}

async fn drop_stream(s: &mut Box<dyn Session>, name: &str) {
    let mut out = QueryOutcome::default();
    let _ = s.execute(&format!("DROP STREAM IF EXISTS {name} DELETE TOPIC;"), 10, &mut out).await;
}

/// Rows in a stream (read to its end).
async fn count(s: &mut Box<dyn Session>, obj: &ObjectRef) -> u64 {
    s.read_batches(&ReadSpec { table: obj.clone(), columns: None, filter: None }, Arc::new(Mutex::new(Collect::default()))).await.unwrap()
}

fn load_spec(obj: &ObjectRef, columns: &[&str], commit_rows: u64) -> LoadSpec {
    LoadSpec {
        table: obj.clone(),
        columns: columns.iter().map(|n| n.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

/// A cancel mid-load: nothing is committed after the load returns, and
/// what was reported is what the stream holds.
#[tokio::test]
#[ignore]
async fn ksqldb_transfer_cancel() {
    let Some(c) = cfg() else { return };
    let _serial = SERIAL.lock().await;
    let d = dbine_driver_ksqldb::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    drop_stream(&mut s, "DBINE_XFER_C").await;
    run(&mut s, &format!("CREATE STREAM DBINE_XFER_C (ID BIGINT KEY, S STRING) WITH (KAFKA_TOPIC='{}', PARTITIONS=1, VALUE_FORMAT='JSON');", topic("dbine_xfer_c"))).await;
    let obj = ObjectRef { kind: kinds::STREAM.into(), schema: None, name: "DBINE_XFER_C".into() };
    let batches: Vec<RowBatch> = (0..40)
        .map(|b| RowBatch { rows: (0..1000).map(|i| vec![Cell::Int(b * 1000 + i), Cell::Text(format!("fila {i} {}", "x".repeat(200)))]).collect(), bytes: 0 })
        .collect();
    let stop = s.interrupter().unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(400)).await;
        stop();
    });
    let reports = Mutex::new(Vec::new());
    let r = s.bulk_load(&load_spec(&obj, &["ID", "S"], 1), &[], &mut Batches(batches.into_iter()), &|n| reports.lock().unwrap().push(n)).await;
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    let reported = reports.into_inner().unwrap().last().copied().unwrap_or(0);
    println!("ksqldb: cancelled with {reported} rows reported");
    assert!(reported > 0 && reported < 40_000, "{reported}");
    assert_eq!(count(&mut s, &obj).await, reported);
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(count(&mut s, &obj).await, reported, "rows committed after the load returned");
    drop_stream(&mut s, "DBINE_XFER_C").await;
}

/// A load dropped mid-request (how the transfer engine cancels a table):
/// the drop waits for the request ksqlDB already has, so no row lands in
/// the stream after the load is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn ksqldb_transfer_dropped() {
    let Some(c) = cfg() else { return };
    let _serial = SERIAL.lock().await;
    let d = dbine_driver_ksqldb::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    drop_stream(&mut s, "DBINE_XFER_D").await;
    run(&mut s, &format!("CREATE STREAM DBINE_XFER_D (ID BIGINT KEY, S STRING) WITH (KAFKA_TOPIC='{}', PARTITIONS=1, VALUE_FORMAT='JSON');", topic("dbine_xfer_d"))).await;
    let obj = ObjectRef { kind: kinds::STREAM.into(), schema: None, name: "DBINE_XFER_D".into() };
    let batches: Vec<RowBatch> = (0..40)
        .map(|b| RowBatch { rows: (0..1000).map(|i| vec![Cell::Int(b * 1000 + i), Cell::Text(format!("fila {i} {}", "x".repeat(200)))]).collect(), bytes: 0 })
        .collect();
    let spec = load_spec(&obj, &["ID", "S"], 1);
    let mut src = Batches(batches.into_iter());
    let r = tokio::time::timeout(Duration::from_millis(250), s.bulk_load(&spec, &[], &mut src, &|_| {})).await;
    assert!(r.is_err(), "the load should have been dropped: {r:?}");
    let at_drop = count(&mut s, &obj).await;
    println!("ksqldb: dropped with {at_drop} rows in the stream");
    assert!(at_drop > 0 && at_drop < 40_000, "{at_drop}");
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(count(&mut s, &obj).await, at_drop, "rows landed after the load was dropped");
    drop_stream(&mut s, "DBINE_XFER_D").await;
}

/// A row that can't go stops the load before its request is sent, so
/// nothing past the rows already reported reaches the stream.
#[tokio::test]
#[ignore]
async fn ksqldb_transfer_rejected_rows() {
    let Some(c) = cfg() else { return };
    let _serial = SERIAL.lock().await;
    let d = dbine_driver_ksqldb::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    drop_stream(&mut s, "DBINE_XFER_E").await;
    run(&mut s, &format!("CREATE STREAM DBINE_XFER_E (ID BIGINT KEY, TS TIMESTAMP, DT DATE) WITH (KAFKA_TOPIC='{}', PARTITIONS=1, VALUE_FORMAT='JSON');", topic("dbine_xfer_e"))).await;
    let obj = ObjectRef { kind: kinds::STREAM.into(), schema: None, name: "DBINE_XFER_E".into() };
    let spec = load_spec(&obj, &["ID", "TS", "DT"], 1);
    let rows = |bad: usize, ts: &str, dt: &str| {
        let rows = (0..10)
            .map(|i| {
                let (t, d) = if i == bad { (ts, dt) } else { ("2024-01-01 00:00:00", "2024-01-01") };
                vec![Cell::Int(i as i64), Cell::DateTime(t.into()), Cell::Date(d.into())]
            })
            .collect();
        Batches(vec![RowBatch { rows, bytes: 0 }].into_iter())
    };

    // Caught before sending: nothing goes in.
    let reports = Mutex::new(Vec::new());
    let r = s.bulk_load(&spec, &[], &mut rows(5, "garbage", "2024-01-01"), &|n| reports.lock().unwrap().push(n)).await;
    match r {
        Err(Error::Query(m)) => assert!(m.contains("fila 6") && m.contains("TS"), "{m}"),
        r => panic!("{r:?}"),
    }
    assert!(reports.into_inner().unwrap().is_empty());
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(count(&mut s, &obj).await, 0);

    // Values ksqlDB would reject itself, after taking the rows before them
    // (and, depending on timing, the ones after): caught before sending too.
    for (ts, dt) in [("2024-01-01 00:00:00", "2024-02-31"), ("0000-01-01 00:00:00", "2024-01-01")] {
        let r = s.bulk_load(&spec, &[], &mut rows(5, ts, dt), &|n| panic!("{n} rows reported")).await;
        assert!(matches!(&r, Err(Error::Query(m)) if m.contains("fila 6")), "{r:?}");
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(count(&mut s, &obj).await, 0);
    drop_stream(&mut s, "DBINE_XFER_E").await;
}

/// Quoted (case-sensitive) names, nested decimals, milliseconds and
/// infinities go in and come back whole, also copied ksqlDB to ksqlDB.
#[tokio::test]
#[ignore]
async fn ksqldb_transfer_exact_values() {
    let Some(c) = cfg() else { return };
    let _serial = SERIAL.lock().await;
    let d = dbine_driver_ksqldb::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    drop_stream(&mut s, "`dbine_xfer_q`").await;
    run(
        &mut s,
        &format!(
            "CREATE STREAM `dbine_xfer_q` (`id` BIGINT KEY, `Lo` INTEGER, `s` STRUCT<`lo` INTEGER, HI DECIMAL(30, 10), `t` TIME>,
                                       `a` ARRAY<DECIMAL(30, 10)>, `tm` TIME, `d` DECIMAL(38, 10), `f` DOUBLE, `af` ARRAY<DOUBLE>)
           WITH (KAFKA_TOPIC='{}', PARTITIONS=1, VALUE_FORMAT='JSON');",
            topic("dbine_xfer_q")
        ),
    )
    .await;
    let obj = ObjectRef { kind: kinds::STREAM.into(), schema: None, name: "dbine_xfer_q".into() };
    let names = ["id", "Lo", "s", "a", "tm", "d", "f", "af"];
    let spec = load_spec(&obj, &names, 1);
    let rows = vec![
        vec![
            Cell::Int(1),
            Cell::Int(7),
            Cell::Json(r#"{"lo":1,"HI":12345678901234567890.1234567890,"t":"01:02:03.456"}"#.into()),
            Cell::Json("[12345678901234567890.1234567890,null]".into()),
            Cell::Time("23:59:59.999".into()),
            Cell::Decimal("9999999999999999999999999999.9999999999".into()),
            Cell::Float(f64::INFINITY),
            Cell::Json(r#"["-Infinity",0.1]"#.into()),
        ],
        vec![Cell::Int(2), Cell::Null, Cell::Null, Cell::Null, Cell::Time("12:00:00.5".into()), Cell::Decimal("-0.0000000001".into()), Cell::Float(f64::NEG_INFINITY), Cell::Null],
    ];
    let n = s.bulk_load(&spec, &[], &mut Batches(vec![RowBatch { rows: rows.clone(), bytes: 0 }].into_iter()), &|_| {}).await.unwrap();
    assert_eq!(n, 2);
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(&ReadSpec { table: obj.clone(), columns: None, filter: None }, sink.clone()).await.unwrap();
    let got = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(got.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), names);
    assert_eq!(
        got.rows[0],
        vec![
            Cell::Int(1),
            Cell::Int(7),
            // TO_JSON_STRING sorts the fields.
            Cell::Json(r#"{"HI":12345678901234567890.1234567890,"lo":1,"t":"01:02:03.456"}"#.into()),
            Cell::Json("[12345678901234567890.1234567890,null]".into()),
            Cell::Time("23:59:59.999".into()),
            Cell::Decimal("9999999999999999999999999999.9999999999".into()),
            Cell::Float(f64::INFINITY),
            Cell::Json(r#"["-Infinity",0.1]"#.into()),
        ]
    );
    assert_eq!(
        got.rows[1],
        vec![Cell::Int(2), Cell::Null, Cell::Null, Cell::Null, Cell::Time("12:00:00.500".into()), Cell::Decimal("-0.0000000001".into()), Cell::Float(f64::NEG_INFINITY), Cell::Null]
    );
    // A subset, in another order; an unknown column is an error.
    let sink = Arc::new(Mutex::new(Collect::default()));
    let sub = ReadSpec { table: obj.clone(), columns: Some(vec!["tm".into(), "id".into()]), filter: Some("`id` = 2".into()) };
    assert_eq!(s.read_batches(&sub, sink.clone()).await.unwrap(), 1);
    assert_eq!(sink.lock().unwrap().rows, vec![vec![Cell::Time("12:00:00.500".into()), Cell::Int(2)]]);
    let unknown = ReadSpec { table: obj.clone(), columns: Some(vec!["nada".into()]), filter: None };
    assert!(s.read_batches(&unknown, Arc::new(Mutex::new(Collect::default()))).await.is_err());

    // ksqlDB to ksqlDB: what was read loads again, and reads the same.
    let again = s.bulk_load(&spec, &[], &mut Batches(vec![RowBatch { rows: got.rows.clone(), bytes: 0 }].into_iter()), &|_| {}).await.unwrap();
    assert_eq!(again, 2);
    let sink = Arc::new(Mutex::new(Collect::default()));
    s.read_batches(&ReadSpec { table: obj.clone(), columns: None, filter: None }, sink.clone()).await.unwrap();
    let all = std::mem::take(&mut sink.lock().unwrap().rows);
    assert_eq!(all[2..], got.rows[..]);

    // Values ksqlDB would take and lose, or can't take: errors, nothing in.
    for (col, bad) in [
        (5, Cell::Decimal("99999999999999999999999999999".into())),
        (5, Cell::Decimal("1.00000000001".into())),
        (4, Cell::Time("01:02:03.4567".into())),
        (2, Cell::Json(r#"{"otro":1}"#.into())),
        (6, Cell::Float(f64::NAN)),
    ] {
        let mut row = rows[1].clone();
        row[col] = bad.clone();
        let r = s.bulk_load(&spec, &[], &mut Batches(vec![RowBatch { rows: vec![row], bytes: 0 }].into_iter()), &|_| {}).await;
        assert!(r.is_err(), "{bad:?}");
        if matches!(bad, Cell::Float(_)) {
            assert!(matches!(r, Err(Error::Unsupported(_))), "{r:?}");
        }
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(count(&mut s, &obj).await, 4);
    drop_stream(&mut s, "`dbine_xfer_q`").await;
}
