//! Bulk transfer against a real SAP HANA. The only container is SAP's
//! HANA express image (`saplabs/hanaexpress`): it needs accepting SAP's
//! license and 8 GB or more of memory, so it isn't one of the
//! `dbine-test-*` containers; point the tests at any HANA:
//!
//! ```sh
//! DBINE_TEST_HANA_URL=hana://USER:PASSWORD@host:39041 \
//!   cargo test --release -p dbine-driver-hana -- --ignored transfer --nocapture
//! ```

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::Instant;

fn config() -> ConnectionConfig {
    let url = std::env::var("DBINE_TEST_HANA_URL").expect("DBINE_TEST_HANA_URL");
    let rest = url.strip_prefix("hana://").expect("hana://user:pass@host:port");
    let (rest, tls) = match rest.split_once('?') {
        Some((r, q)) => (r, q.contains("tls")),
        None => (rest, false),
    };
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (host, port) = addr.split_once(':').unwrap();
    ConnectionConfig {
        driver: "hana".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        encrypt: tls,
        ..Default::default()
    }
}

async fn session() -> Box<dyn Session> {
    dbine_driver_hana::drivers().remove(0).connect(&config(), None).await.expect("connect")
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: "table".into(), schema: None, name: name.into() }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    s.execute(sql, 10, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

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
    fn batch(&mut self, b: RowBatch) -> io::Result<()> {
        self.rows.extend(b.rows);
        Ok(())
    }
}

struct Batches(std::vec::IntoIter<RowBatch>);

#[dbine_driver::async_trait]
impl BatchSource for Batches {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.next()
    }
}

fn batches(rows: Vec<Vec<Cell>>) -> Batches {
    let chunks: Vec<RowBatch> = rows.chunks(1000).map(|c| RowBatch { rows: c.to_vec(), bytes: 0 }).collect();
    Batches(chunks.into_iter())
}

async fn read(s: &mut Box<dyn Session>, name: &str) -> Collect {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table: table(name), columns: None, filter: None };
    s.read_batches(&spec, sink.clone()).await.expect("read");
    Arc::try_unwrap(sink).ok().unwrap().into_inner().unwrap()
}

fn load_spec(name: &str, columns: &[&str], commit_rows: u64) -> LoadSpec {
    LoadSpec {
        table: table(name),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: true,
        commit_rows,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

const TYPES: &str = "ID INTEGER PRIMARY KEY, TI TINYINT, SI SMALLINT, BI BIGINT, D DECIMAL(38,10), SD SMALLDECIMAL,
    R REAL, DB DOUBLE, B BOOLEAN, VC VARCHAR(50), NV NVARCHAR(50), NC NCLOB, BL BLOB, VB VARBINARY(100),
    DT DATE, TM TIME, SE SECONDDATE, TS TIMESTAMP, G ST_GEOMETRY(0)";

#[tokio::test]
#[ignore]
async fn transfer_all_types_round_trip() {
    let mut s = session().await;
    for t in ["DBINE_XFER_SRC", "DBINE_XFER_DST"] {
        let _ = s.execute(&format!("DROP TABLE {t}"), 10, &mut QueryOutcome::default()).await;
        run(&mut s, &format!("CREATE COLUMN TABLE {t} ({TYPES})")).await;
    }
    run(
        &mut s,
        "INSERT INTO DBINE_XFER_SRC VALUES (1, 255, -32768, -9223372036854775808, 1234567890123456789012345678.0123456789,
            1.5, 1.25, -2.5e300, TRUE, 'ascii', 'ñandú ☃', 'texto largo ☃', TO_BLOB(HEXTOBIN('DEADBEEF')), HEXTOBIN('00FF'),
            '2024-02-29', '23:59:59', '2024-01-31 13:45:59', '2024-01-31 13:45:00.1234567', ST_GeomFromText('POINT (1 2)', 0))",
    )
    .await;
    run(&mut s, "INSERT INTO DBINE_XFER_SRC (ID) VALUES (2)").await;

    let src = read(&mut s, "DBINE_XFER_SRC").await;
    let names: Vec<&str> = src.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(src.columns[4].type_name, "DECIMAL(38,10)");
    let one = src.rows.iter().find(|r| r[0] == Cell::Int(1)).unwrap();
    assert_eq!(one[1], Cell::Int(255));
    assert_eq!(one[3], Cell::Int(i64::MIN));
    assert_eq!(one[4], Cell::Decimal("1234567890123456789012345678.0123456789".into()));
    assert_eq!(one[8], Cell::Bool(true));
    assert_eq!(one[10], Cell::Text("ñandú ☃".into()));
    assert_eq!(one[12], Cell::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]));
    assert_eq!(one[14], Cell::Date("2024-02-29".into()));
    assert_eq!(one[15], Cell::Time("23:59:59".into()));
    assert_eq!(one[16], Cell::DateTime("2024-01-31 13:45:59".into()));
    assert_eq!(one[17], Cell::DateTime("2024-01-31 13:45:00.1234567".into()));
    assert!(matches!(&one[18], Cell::Bytes(b) if b.len() == 21), "WKB point: {:?}", one[18]);
    let two = src.rows.iter().find(|r| r[0] == Cell::Int(2)).unwrap();
    assert!(two[1..].iter().all(|c| *c == Cell::Null));

    // Plus a large binary and a large text (streamed LOBs).
    let mut rows = src.rows.clone();
    let mut big = vec![Cell::Null; names.len()];
    big[0] = Cell::Int(3);
    big[11] = Cell::Text("ñ".repeat(1_500_000));
    big[12] = Cell::Bytes((0..3_000_000u32).map(|i| (i % 251) as u8).collect());
    rows.push(big);

    let loaded = s
        .bulk_load(&load_spec("DBINE_XFER_DST", &names, 2), &src.columns, &mut batches(rows.clone()), &|_| {})
        .await
        .expect("load");
    assert_eq!(loaded, 3);
    let mut dst = read(&mut s, "DBINE_XFER_DST").await;
    dst.rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => 0,
    });
    rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => 0,
    });
    assert_eq!(dst.rows, rows);
}

#[tokio::test]
#[ignore]
async fn transfer_benchmark_100k_rows() {
    let mut s = session().await;
    let _ = s.execute("DROP TABLE DBINE_XFER_BENCH", 10, &mut QueryOutcome::default()).await;
    run(
        &mut s,
        "CREATE COLUMN TABLE DBINE_XFER_BENCH (ID BIGINT PRIMARY KEY, NAME NVARCHAR(100), AMOUNT DECIMAL(18,4), TS TIMESTAMP, FLAG BOOLEAN)",
    )
    .await;
    const N: i64 = 100_000;
    let rows: Vec<Vec<Cell>> = (0..N)
        .map(|i| {
            vec![
                Cell::Int(i),
                Cell::Text(format!("fila {i}")),
                Cell::Decimal(format!("{}.{:04}", i * 3, i % 10_000)),
                Cell::DateTime(format!("2024-01-01 00:00:{:02}.{}", i % 60, format!("{:07}", 1 + i % 9_999_999).trim_end_matches('0'))),
                if i % 7 == 0 { Cell::Null } else { Cell::Bool(i % 2 == 0) },
            ]
        })
        .collect();
    let spec = load_spec("DBINE_XFER_BENCH", &["ID", "NAME", "AMOUNT", "TS", "FLAG"], LoadSpec::DEFAULT_COMMIT_ROWS);
    let start = Instant::now();
    let loaded = s.bulk_load(&spec, &[], &mut batches(rows.clone()), &|_| {}).await.expect("load");
    let secs = start.elapsed().as_secs_f64();
    assert_eq!(loaded, N as u64);
    println!("hana bulk_load: {N} filas en {secs:.2} s ({:.0} filas/s)", N as f64 / secs);

    let start = Instant::now();
    let mut back = read(&mut s, "DBINE_XFER_BENCH").await;
    let secs = start.elapsed().as_secs_f64();
    println!("hana read_batches: {N} filas en {secs:.2} s ({:.0} filas/s)", N as f64 / secs);
    back.rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => 0,
    });
    assert_eq!(back.rows, rows);
    let _ = s.execute("DROP TABLE DBINE_XFER_BENCH", 10, &mut QueryOutcome::default()).await;
}

async fn drop_create(s: &mut Box<dyn Session>, name: &str, columns: &str) {
    let _ = s.execute(&format!("DROP TABLE {name}"), 10, &mut QueryOutcome::default()).await;
    run(s, &format!("CREATE COLUMN TABLE {name} ({columns})")).await;
}

async fn count(s: &mut Box<dyn Session>, name: &str) -> usize {
    read(s, name).await.rows.len()
}

/// A decimal with more digits than the column keeps is refused (the client
/// would cut `1.239` to `1.23`), and the failed window leaves no rows.
#[tokio::test]
#[ignore]
async fn transfer_refuses_decimals_the_column_would_cut() {
    let mut s = session().await;
    drop_create(&mut s, "DBINE_XFER_DEC", "ID INTEGER PRIMARY KEY, D DECIMAL(10,2), W DECIMAL(28,0)").await;
    let spec = load_spec("DBINE_XFER_DEC", &["ID", "D", "W"], 1_000);
    let ok = vec![vec![Cell::Int(1), Cell::Decimal("1.2300".into()), Cell::Decimal("9".repeat(28))]];
    assert_eq!(s.bulk_load(&spec, &[], &mut batches(ok), &|_| {}).await.expect("load"), 1);
    for bad in [
        vec![Cell::Int(2), Cell::Decimal("1.239".into()), Cell::Null],
        vec![Cell::Int(3), Cell::Null, Cell::Decimal("39614081257132168796771975168".into())],
    ] {
        let rows = vec![vec![Cell::Int(10), Cell::Decimal("5".into()), Cell::Null], bad];
        let e = s.bulk_load(&spec, &[], &mut batches(rows), &|_| {}).await.unwrap_err();
        println!("{e}");
    }
    let back = read(&mut s, "DBINE_XFER_DEC").await;
    assert_eq!(back.rows, vec![vec![Cell::Int(1), Cell::Decimal("1.23".into()), Cell::Decimal("9".repeat(28))]]);
    let _ = s.execute("DROP TABLE DBINE_XFER_DEC", 10, &mut QueryOutcome::default()).await;
}

/// With `keep_identity`, the identity goes on past the copied keys.
#[tokio::test]
#[ignore]
async fn transfer_moves_the_identity_past_the_copied_keys() {
    let mut s = session().await;
    drop_create(&mut s, "DBINE_XFER_IDENT", "ID BIGINT GENERATED BY DEFAULT AS IDENTITY NOT NULL PRIMARY KEY, NAME NVARCHAR(20)").await;
    // Redeclaring the identity must not lose the column's comment.
    run(&mut s, "COMMENT ON COLUMN DBINE_XFER_IDENT.ID IS 'la clave'").await;
    let rows: Vec<Vec<Cell>> = [5i64, 42, 7].iter().map(|i| vec![Cell::Int(*i), Cell::Text(format!("fila {i}"))]).collect();
    // The identity is moved after the last window is committed (HANA
    // commits DDL on its own): progress has counted every row by then.
    let reported = std::sync::atomic::AtomicU64::new(0);
    let loaded = s
        .bulk_load(&load_spec("DBINE_XFER_IDENT", &["ID", "NAME"], 2), &[], &mut batches(rows), &|n| {
            reported.store(n, std::sync::atomic::Ordering::SeqCst)
        })
        .await
        .expect("load");
    assert_eq!(loaded, 3);
    assert_eq!(reported.load(std::sync::atomic::Ordering::SeqCst), 3);
    // The next generated key doesn't collide with the copied ones.
    run(&mut s, "INSERT INTO DBINE_XFER_IDENT (NAME) VALUES ('nueva')").await;
    let back = read(&mut s, "DBINE_XFER_IDENT").await;
    let new = back.rows.iter().find(|r| r[1] == Cell::Text("nueva".into())).unwrap();
    assert!(matches!(new[0], Cell::Int(n) if n > 42), "{:?}", new[0]);
    let mut out = QueryOutcome::default();
    s.execute(
        "SELECT COMMENTS, GENERATION_TYPE, IS_NULLABLE FROM SYS.TABLE_COLUMNS WHERE TABLE_NAME = 'DBINE_XFER_IDENT' AND COLUMN_NAME = 'ID'",
        10,
        &mut out,
    )
    .await
    .expect("catalog");
    assert!(out.error.is_none(), "{out:?}");
    // Redeclaring the identity kept the comment, the generation and the
    // NOT NULL (a lost comment is put back; anything else fails the load).
    let def: Vec<Option<&str>> = out.results[0].rows[0].iter().map(|v| v.as_str()).collect();
    assert_eq!(def, [Some("la clave"), Some("BY DEFAULT AS IDENTITY"), Some("FALSE")], "{out:?}");
    let _ = s.execute("DROP TABLE DBINE_XFER_IDENT", 10, &mut QueryOutcome::default()).await;
}

/// Spatial values: EWKB (PostGIS), hex, WKT and EWKT into ST_GEOMETRY and
/// ST_POINT; another SRID and GeoJSON are refused.
#[tokio::test]
#[ignore]
async fn transfer_spatial_values() {
    let mut s = session().await;
    drop_create(&mut s, "DBINE_XFER_GEO", "ID INTEGER PRIMARY KEY, G ST_GEOMETRY(4326), P ST_POINT(4326)").await;
    let mut ewkb = vec![1u8];
    ewkb.extend_from_slice(&(1u32 | 0x2000_0000).to_le_bytes());
    ewkb.extend_from_slice(&4326u32.to_le_bytes());
    ewkb.extend_from_slice(&1.0f64.to_le_bytes());
    ewkb.extend_from_slice(&2.0f64.to_le_bytes());
    let hex: String = ewkb.iter().map(|b| format!("{b:02X}")).collect();
    let rows = vec![
        vec![Cell::Int(1), Cell::Bytes(ewkb.clone()), Cell::Bytes(ewkb.clone())],
        vec![Cell::Int(2), Cell::Text(hex), Cell::Text("POINT (1 2)".into())],
        vec![Cell::Int(3), Cell::Text("SRID=4326;LINESTRING (0 0, 1 1)".into()), Cell::Null],
    ];
    let spec = load_spec("DBINE_XFER_GEO", &["ID", "G", "P"], 1_000);
    assert_eq!(s.bulk_load(&spec, &[], &mut batches(rows), &|_| {}).await.expect("load"), 3);
    let back = read(&mut s, "DBINE_XFER_GEO").await;
    assert_eq!(back.columns[1].type_name, "ST_GEOMETRY(4326)");
    assert_eq!(back.columns[2].type_name, "ST_POINT(4326)");
    for bad in [Cell::Json("{\"type\":\"Point\",\"coordinates\":[1,2]}".into()), Cell::Text("SRID=3857;POINT (1 2)".into())] {
        let rows = vec![vec![Cell::Int(9), bad, Cell::Null]];
        assert!(s.bulk_load(&spec, &[], &mut batches(rows), &|_| {}).await.is_err());
    }
    assert_eq!(count(&mut s, "DBINE_XFER_GEO").await, 3);
    let _ = s.execute("DROP TABLE DBINE_XFER_GEO", 10, &mut QueryOutcome::default()).await;
}

/// Values the target can't keep are refused: a TIME with a fraction, a
/// REAL out of range, bytes that aren't UTF-8 into text, NaN / infinity,
/// a time of day in text into a DATE, a date in text into a TIME, and a
/// decimal exponent past any DECIMAL (refused before it's expanded).
#[tokio::test]
#[ignore]
async fn transfer_refuses_silent_changes() {
    let mut s = session().await;
    drop_create(
        &mut s,
        "DBINE_XFER_LOSS",
        "ID INTEGER PRIMARY KEY, T TIME, R REAL, N NVARCHAR(20), F DOUBLE, D DATE, X DECIMAL(38,2)",
    )
    .await;
    let spec = load_spec("DBINE_XFER_LOSS", &["ID", "T", "R", "N", "F", "D", "X"], 1_000);
    let row = |i: usize, c: Cell| {
        let mut r = vec![Cell::Int(1), Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null];
        r[i] = c;
        r
    };
    for bad in [
        row(1, Cell::Time("10:00:00.5".into())),
        row(2, Cell::Float(1e300)),
        row(3, Cell::Bytes(vec![0xFF, 0xFE])),
        row(2, Cell::Float(f64::NAN)),
        row(4, Cell::Float(f64::NAN)),
        row(4, Cell::Float(f64::INFINITY)),
        row(4, Cell::Text("-Infinity".into())),
        row(5, Cell::Text("2024-01-01 10:30:00".into())),
        row(1, Cell::Text("2024-01-01 10:30:00".into())),
        row(6, Cell::Text("1E+100000000".into())),
        // A DOUBLE can't hold these exactly.
        row(4, Cell::Int(9_007_199_254_740_993)),
        row(4, Cell::Decimal("1.00000000000000001".into())),
        // A time of day in unpadded date text.
        row(5, Cell::Text("2024-1-1 10:30:00".into())),
        row(5, Cell::Text("20240101 10:00:00".into())),
        // Invalid dates and times with a zone: refused, not rolled over
        // into another valid instant by the UTC conversion.
        row(5, Cell::Text("2023-02-29T00:00:00Z".into())),
        row(5, Cell::DateTimeTz("2024-02-30 00:00:00+00:00".into())),
        // A clock HANA's implicit conversion might read differently.
        row(5, Cell::Text("2024-01-01 10:00:00 PM".into())),
        row(1, Cell::Text("10:00:00 PM".into())),
    ] {
        let e = s.bulk_load(&spec, &[], &mut batches(vec![bad.clone()]), &|_| {}).await;
        assert!(e.is_err(), "{bad:?}");
        println!("{}", e.unwrap_err());
    }
    assert_eq!(count(&mut s, "DBINE_XFER_LOSS").await, 0);
    // The largest REAL, as text, is a REAL: it loads and reads back as one.
    let ok = row(2, Cell::Text("3.4028235e38".into()));
    assert_eq!(s.bulk_load(&spec, &[], &mut batches(vec![ok]), &|_| {}).await.expect("REAL max"), 1);
    assert_eq!(read(&mut s, "DBINE_XFER_LOSS").await.rows[0][2], Cell::Float(f64::from(f32::MAX)));
    let _ = s.execute("DROP TABLE DBINE_XFER_LOSS", 10, &mut QueryOutcome::default()).await;
}

/// Zoned text into SECONDDATE / TIMESTAMP is stored in UTC, and a comma
/// fraction is read as a point.
#[tokio::test]
#[ignore]
async fn transfer_zoned_text_goes_in_utc() {
    let mut s = session().await;
    drop_create(&mut s, "DBINE_XFER_TZ", "ID INTEGER PRIMARY KEY, S SECONDDATE, L TIMESTAMP").await;
    let rows = vec![
        vec![Cell::Int(1), Cell::Text("2024-01-01 10:00:00+02:00".into()), Cell::Text("2024-01-01T10:00:00Z".into())],
        vec![Cell::Int(2), Cell::Text("2024-01-01 10:00:00".into()), Cell::Text("2024-01-01 10:00:00,125".into())],
    ];
    let spec = load_spec("DBINE_XFER_TZ", &["ID", "S", "L"], 1_000);
    assert_eq!(s.bulk_load(&spec, &[], &mut batches(rows), &|_| {}).await.expect("load"), 2);
    let back = read(&mut s, "DBINE_XFER_TZ").await;
    println!("{:?}", back.rows);
    assert_eq!(back.rows[0][1], Cell::DateTime("2024-01-01 08:00:00".into()));
    // Invalid zoned values (and a clock with a suffix) are refused, and the
    // failed load leaves nothing.
    for bad in [
        "2024-02-30 10:00:00Z",
        "2024-13-01 10:00:00+00:00",
        "2024-01-01 25:61:00Z",
        "2024-01-01 10:00:00 UTC",
        "2024-01-01 10:00:00 PM",
    ] {
        let rows = vec![vec![Cell::Int(9), Cell::Text(bad.into()), Cell::Text(bad.into())]];
        let e = s.bulk_load(&spec, &[], &mut batches(rows), &|_| {}).await;
        assert!(e.is_err(), "{bad}");
    }
    assert_eq!(count(&mut s, "DBINE_XFER_TZ").await, 2);
    let _ = s.execute("DROP TABLE DBINE_XFER_TZ", 10, &mut QueryOutcome::default()).await;
}
