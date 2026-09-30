//! Bulk transfer through a real ODBC driver, with the generic "odbc"
//! preset against SQL Server (the `dbine-test-sqlserver` container and
//! Microsoft's ODBC Driver 18). Run with:
//!
//! ```sh
//! cargo test -p dbine-driver-odbc --test transfer -- --ignored --nocapture
//! ```
//!
//! `DBINE_TEST_ODBC_CONN` overrides the connection string.

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{async_trait, ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};

const DEFAULT_CONN: &str =
    "DRIVER={ODBC Driver 18 for SQL Server};SERVER=127.0.0.1,25013;UID=sa;PWD={Pw_12345!};TrustServerCertificate=yes";
const DB: &str = "dbine_odbc_transfer";

fn cfg() -> ConnectionConfig {
    let conn = std::env::var("DBINE_TEST_ODBC_CONN").unwrap_or_else(|_| DEFAULT_CONN.into());
    ConnectionConfig {
        driver: "odbc".into(),
        options: [("connection_string".to_string(), conn), ("batch_mode".to_string(), "go".to_string())].into(),
        ..Default::default()
    }
}

async fn open(db: Option<&str>) -> Box<dyn Session> {
    let d = dbine_driver_odbc::drivers().into_iter().find(|d| d.info().id == "odbc").unwrap();
    assert!(d.supports_bulk_load());
    d.connect(&cfg(), db).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: name.into() }
}

#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    batches: Vec<RowBatch>,
}

impl BatchSink for Collect {
    fn begin(&mut self, columns: &[TransferColumn]) -> io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, batch: RowBatch) -> io::Result<()> {
        self.batches.push(batch);
        Ok(())
    }
}

struct Source(VecDeque<RowBatch>);

#[async_trait]
impl BatchSource for Source {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.pop_front()
    }
}

async fn read(s: &mut Box<dyn Session>, name: &str, columns: Option<Vec<&str>>, filter: Option<&str>) -> (Vec<TransferColumn>, Vec<RowBatch>) {
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec {
        table: table(name),
        columns: columns.map(|c| c.into_iter().map(String::from).collect()),
        filter: filter.map(String::from),
    };
    let n = s.read_batches(&spec, sink.clone()).await.unwrap_or_else(|e| panic!("read {name}: {e}"));
    let c = std::mem::take(&mut *sink.lock().unwrap());
    assert_eq!(n as usize, c.batches.iter().map(RowBatch::len).sum::<usize>());
    (c.columns, c.batches)
}

/// Rows sorted by their first cell (an integer id).
fn sorted(batches: &[RowBatch]) -> Vec<Vec<Cell>> {
    let mut rows: Vec<Vec<Cell>> = batches.iter().flat_map(|b| b.rows.clone()).collect();
    rows.sort_by_key(|r| match r[0] {
        Cell::Int(i) => i,
        _ => i64::MIN,
    });
    rows
}

async fn load(s: &mut Box<dyn Session>, name: &str, columns: &[TransferColumn], batches: Vec<RowBatch>, keep_identity: bool) -> (u64, Vec<u64>) {
    let spec = LoadSpec {
        table: table(name),
        columns: columns.iter().map(|c| c.name.clone()).collect(),
        table_lock: false,
        keep_identity,
        commit_rows: 1_000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let seen = Mutex::new(Vec::new());
    let progress = |n: u64| seen.lock().unwrap().push(n);
    let n = s
        .bulk_load(&spec, columns, &mut Source(batches.into()), &progress)
        .await
        .unwrap_or_else(|e| panic!("load {name}: {e}"));
    (n, seen.into_inner().unwrap())
}

const NARROW: &str = "(id int NOT NULL PRIMARY KEY, big bigint, d decimal(38,10), f float, b bit, dt date,
    ts datetime2(7), tm time(7), dto datetimeoffset(7), g uniqueidentifier, s nvarchar(50), vb varbinary(16))";
const LONG: &str = "(id int NOT NULL PRIMARY KEY, txt nvarchar(max), blob varbinary(max))";

#[tokio::test]
#[ignore]
async fn sql_server_bulk_transfer_through_odbc() {
    let mut s = open(None).await;
    run(&mut s, &format!("IF DB_ID('{DB}') IS NULL CREATE DATABASE {DB}")).await;
    drop(s);
    let mut s = open(Some(DB)).await;
    for t in ["src_narrow", "dst_narrow", "src_long", "dst_long", "dst_ident"] {
        run(&mut s, &format!("IF OBJECT_ID('dbo.{t}') IS NOT NULL DROP TABLE dbo.{t}")).await;
    }
    run(&mut s, &format!("CREATE TABLE dbo.src_narrow {NARROW}\nGO\nCREATE TABLE dbo.dst_narrow {NARROW}")).await;
    run(
        &mut s,
        "WITH n AS (SELECT TOP (2500) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS i FROM sys.all_objects a CROSS JOIN sys.all_objects b)
         INSERT INTO dbo.src_narrow
         SELECT i, CAST(i AS bigint) * 3037000499, CAST(i AS decimal(38,10)) / 7 - 1000, 1.0 / i, i % 2,
                DATEADD(day, i, '1999-12-31'), DATEADD(ns, i * 100, CAST('2024-02-29 23:59:58.1234567' AS datetime2(7))),
                CAST('13:45:00.1234567' AS time(7)), CAST('2024-01-02 03:04:05.1234567 -03:00' AS datetimeoffset(7)),
                NEWID(), CONCAT(N'fila ñ ', i), CAST(i AS varbinary(16))
           FROM n
         INSERT INTO dbo.src_narrow (id, d, s) VALUES (0, -0.0000000001, N'')
         INSERT INTO dbo.src_narrow (id) VALUES (-1)",
    )
    .await;
    run(&mut s, &format!("CREATE TABLE dbo.src_long {LONG}\nGO\nCREATE TABLE dbo.dst_long {LONG}")).await;
    run(
        &mut s,
        "INSERT INTO dbo.src_long VALUES
           (1, REPLICATE(CAST(N'áé€𝄞' AS nvarchar(max)), 30000), CAST(REPLICATE(CAST(CHAR(171) + CHAR(0) AS varchar(max)), 1500000) AS varbinary(max))),
           (2, N'', 0x),
           (3, NULL, NULL)",
    )
    .await;

    // Requested columns, in the requested order, filtered: bound in blocks.
    let (cols, batches) = read(&mut s, "src_narrow", Some(vec!["s", "id", "d"]), Some("id BETWEEN 1 AND 10")).await;
    assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["s", "id", "d"]);
    assert_eq!(cols[0].type_name, "nvarchar(50)");
    assert_eq!(cols[2].type_name, "decimal(38,10)");
    let mut rows: Vec<Vec<Cell>> = batches.iter().flat_map(|b| b.rows.clone()).collect();
    rows.sort_by_key(|r| match r[1] {
        Cell::Int(i) => i,
        _ => 0,
    });
    assert_eq!(rows.len(), 10);
    assert_eq!(rows[6], vec![Cell::Text("fila ñ 7".into()), Cell::Int(7), Cell::Decimal("-999.0000000000".into())]);

    // Every column, typed.
    let (cols, batches) = read(&mut s, "src_narrow", None, None).await;
    assert!(batches.iter().all(|b| b.len() <= 1_000));
    let src = sorted(&batches);
    assert_eq!(src.len(), 2502);
    assert!(src[0][1..].iter().all(|c| *c == Cell::Null), "{:?}", src[0]);
    assert_eq!(src[1][2], Cell::Decimal("-0.0000000001".into()));
    assert_eq!(src[1][10], Cell::Text(String::new()));
    let r = &src[2 + 2499]; // id 2500
    assert_eq!(r[1], Cell::Int(2500 * 3_037_000_499));
    assert_eq!(r[2], Cell::Decimal("-642.8571428572".into()));
    assert_eq!(r[3], Cell::Float(1.0 / 2500.0));
    assert_eq!(r[4], Cell::Bool(false));
    assert_eq!(r[5], Cell::Date("2006-11-04".into()));
    assert_eq!(r[6], Cell::DateTime("2024-02-29 23:59:58.1237067".into()));
    assert_eq!(r[7], Cell::Time("13:45:00.1234567".into()));
    assert_eq!(r[8], Cell::DateTimeTz("2024-01-02 03:04:05.1234567-03:00".into()));
    assert!(matches!(&r[9], Cell::Uuid(u) if u.len() == 36 && u == &u.to_lowercase()), "{:?}", r[9]);
    assert_eq!(r[11], Cell::Bytes(vec![0, 0, 0, 0, 0, 0, 0x09, 0xC4])); // ROW_NUMBER is a bigint

    // Load them back with parameter arrays, a commit every 1 000 rows.
    let (n, progress) = load(&mut s, "dst_narrow", &cols, batches, false).await;
    assert_eq!(n, 2502);
    println!("narrow progress: {progress:?}");
    assert_eq!(progress.last(), Some(&2502));
    assert!(progress.len() >= 3 && progress.windows(2).all(|w| w[0] < w[1]));
    let (_, back) = read(&mut s, "dst_narrow", None, None).await;
    assert_eq!(sorted(&back), src);

    // Long columns: row by row, whole.
    let (lcols, lbatches) = read(&mut s, "src_long", None, None).await;
    assert_eq!(lcols[2].type_name, "varbinary");
    let lsrc = sorted(&lbatches);
    match (&lsrc[0][1], &lsrc[0][2]) {
        (Cell::Text(t), Cell::Bytes(b)) => {
            assert_eq!(t.chars().count(), 4 * 30_000);
            assert!(t.starts_with("áé€𝄞"));
            assert_eq!(b.len(), 3_000_000);
            assert!(b.chunks(2).all(|p| p == [0xAB, 0x00]));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(lsrc[1][1..], [Cell::Text(String::new()), Cell::Bytes(Vec::new())]);
    assert_eq!(lsrc[2][1..], [Cell::Null, Cell::Null]);
    let (n, _) = load(&mut s, "dst_long", &lcols, lbatches, false).await;
    assert_eq!(n, 3);
    let (_, lback) = read(&mut s, "dst_long", None, None).await;
    assert_eq!(sorted(&lback), lsrc);

    // Identity values kept (IDENTITY_INSERT around the load).
    run(&mut s, "CREATE TABLE dbo.dst_ident (id int IDENTITY(1,1) PRIMARY KEY, s nvarchar(10))").await;
    let icols = vec![
        TransferColumn { name: "id".into(), type_name: "int".into(), nullable: false },
        TransferColumn { name: "s".into(), type_name: "nvarchar(10)".into(), nullable: true },
    ];
    let rows = vec![vec![Cell::Int(40), Cell::Text("a".into())], vec![Cell::Int(7), Cell::Text("b".into())]];
    let (n, _) = load(&mut s, "dst_ident", &icols, vec![RowBatch { rows: rows.clone(), bytes: 0 }], true).await;
    assert_eq!(n, 2);
    let (_, iback) = read(&mut s, "dst_ident", None, None).await;
    let mut expect = rows;
    expect.reverse();
    assert_eq!(sorted(&iback), expect);

    // A bad value fails the load and rolls back its open window: nothing
    // of that window stays.
    let bad = vec![vec![Cell::Int(1), Cell::Text("ok".into())], vec![Cell::Text("x".into()), Cell::Text("no".into())]];
    let spec = LoadSpec {
        table: table("dst_ident"),
        columns: vec!["id".into(), "s".into()],
        table_lock: false,
        keep_identity: true,
        commit_rows: 1_000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let r = s.bulk_load(&spec, &icols, &mut Source(vec![RowBatch { rows: bad, bytes: 0 }].into()), &|_| {}).await;
    assert!(r.is_err());
    let (_, after) = read(&mut s, "dst_ident", None, None).await;
    assert_eq!(after.iter().map(RowBatch::len).sum::<usize>(), 2);

    for t in ["src_narrow", "dst_narrow", "src_long", "dst_long", "dst_ident"] {
        run(&mut s, &format!("DROP TABLE dbo.{t}")).await;
    }
}

/// First number of the first result of `sql`.
async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> u64 {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    let v = out.results.iter().find(|r| !r.rows.is_empty()).map(|r| r.rows[0][0].clone()).unwrap_or_else(|| panic!("{sql}"));
    v.as_u64().or_else(|| v.as_str()?.trim().parse().ok()).unwrap_or_else(|| panic!("{sql}: {v}"))
}

fn spec(name: &str, columns: &[&str], commit_rows: u64) -> LoadSpec {
    LoadSpec {
        table: table(name),
        columns: columns.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

fn int_batches(batches: i64, rows: i64) -> Vec<RowBatch> {
    (0..batches)
        .map(|b| RowBatch { rows: (0..rows).map(|r| vec![Cell::Int(b * rows + r + 1)]).collect(), bytes: 0 })
        .collect()
}

/// Hands out its batches; before each one after the first, counts (through
/// a second connection) the rows the load has already executed; after the
/// last, either ends or pends forever (a read that stalls).
struct Probe {
    batches: VecDeque<RowBatch>,
    peek: Box<dyn Session>,
    count_sql: String,
    handed: usize,
    seen: Vec<u64>,
    hang: Option<tokio::sync::oneshot::Sender<()>>,
}

#[async_trait]
impl BatchSource for Probe {
    async fn next(&mut self) -> Option<RowBatch> {
        if self.handed > 0 && !self.count_sql.is_empty() {
            let n = scalar(&mut self.peek, &self.count_sql.clone()).await;
            self.seen.push(n);
        }
        match self.batches.pop_front() {
            Some(b) => {
                self.handed += 1;
                Some(b)
            }
            None => match self.hang.take() {
                Some(tx) => {
                    let _ = tx.send(());
                    std::future::pending().await
                }
                None => None,
            },
        }
    }
}

#[tokio::test]
#[ignore]
async fn sql_server_bulk_load_edge_cases() {
    let mut s = open(None).await;
    run(&mut s, &format!("IF DB_ID('{DB}') IS NULL CREATE DATABASE {DB}")).await;
    drop(s);
    let mut s = open(Some(DB)).await;
    let tables = ["fix_late", "fix_mem", "fix_utf8", "fix_ts", "fix_vsrc", "fix_vdst", "fix_usrc", "fix_udst"];
    for t in tables {
        run(&mut s, &format!("IF OBJECT_ID('dbo.{t}') IS NOT NULL DROP TABLE dbo.{t}")).await;
    }
    let id = TransferColumn { name: "id".into(), type_name: "int".into(), nullable: false };

    // 1. A load dropped while its source stalls commits nothing afterwards:
    // what's in the table right after the drop is what stays.
    run(&mut s, "CREATE TABLE dbo.fix_late (id int NOT NULL PRIMARY KEY)").await;
    let (hung_tx, hung_rx) = tokio::sync::oneshot::channel();
    let mut src = Probe {
        batches: int_batches(8, 1000).into(),
        peek: open(Some(DB)).await,
        count_sql: String::new(),
        handed: 0,
        seen: Vec::new(),
        hang: Some(hung_tx),
    };
    let seen = Mutex::new(Vec::new());
    let progress = |n: u64| seen.lock().unwrap().push(n);
    {
        let cols = [id.clone()];
        let sp = spec("fix_late", &["id"], 1000);
        let fut = s.bulk_load(&sp, &cols, &mut src, &progress);
        tokio::select! {
            r = fut => panic!("the load ended with a stalled source: {r:?}"),
            _ = hung_rx => {}
        }
    }
    let at_drop = scalar(&mut src.peek, "SELECT COUNT(*) FROM dbo.fix_late WITH (READPAST)").await;
    // The session's next statement waits for the loading thread to finish.
    let after = scalar(&mut s, "SELECT COUNT(*) FROM dbo.fix_late").await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let later = scalar(&mut src.peek, "SELECT COUNT(*) FROM dbo.fix_late").await;
    let reported = seen.lock().unwrap().last().copied().unwrap_or(0);
    println!("late commits: progress {:?}, at drop {at_drop}, after {after}, later {later}", seen.lock().unwrap());
    assert_eq!((after, later), (at_drop, at_drop));
    assert!(at_drop % 1000 == 0 && at_drop >= reported, "{at_drop} vs {reported}");

    // 6. The next batch is only read once the last one was executed (and
    // freed): nothing waits outside the orchestrator's window.
    run(&mut s, "CREATE TABLE dbo.fix_mem (id int NOT NULL PRIMARY KEY)").await;
    let mut src = Probe {
        batches: int_batches(6, 500).into(),
        peek: src.peek,
        count_sql: "SELECT COUNT(*) FROM dbo.fix_mem WITH (NOLOCK)".into(),
        handed: 0,
        seen: Vec::new(),
        hang: None,
    };
    let n = s.bulk_load(&spec("fix_mem", &["id"], 100_000), std::slice::from_ref(&id), &mut src, &|_| {}).await.expect("load");
    assert_eq!(n, 3000);
    assert_eq!(src.seen, [500, 1000, 1500, 2000, 2500, 3000]);

    // 2. Non-ASCII text into a narrow UTF-8 column arrives whole.
    run(&mut s, "CREATE TABLE dbo.fix_utf8 (id int NOT NULL PRIMARY KEY, v varchar(10) COLLATE Latin1_General_100_CI_AS_SC_UTF8)").await;
    let cols = vec![id.clone(), TransferColumn { name: "v".into(), type_name: "varchar(10)".into(), nullable: true }];
    let rows = vec![vec![Cell::Int(1), Cell::Text("ñ€𝄞".into())], vec![Cell::Int(2), Cell::Text("abc".into())]];
    load(&mut s, "fix_utf8", &cols, vec![RowBatch { rows: rows.clone(), bytes: 0 }], false).await;
    let (_, back) = read(&mut s, "fix_utf8", None, None).await;
    assert_eq!(sorted(&back), rows);

    // 3. Nine fraction digits into datetime2(7): cut to the column's seven.
    run(&mut s, "CREATE TABLE dbo.fix_ts (id int NOT NULL PRIMARY KEY, ts datetime2(7))").await;
    let cols = vec![id.clone(), TransferColumn { name: "ts".into(), type_name: "datetime2(7)".into(), nullable: true }];
    let rows = vec![vec![Cell::Int(1), Cell::DateTime("2024-01-01 00:00:00.123456789".into())]];
    load(&mut s, "fix_ts", &cols, vec![RowBatch { rows, bytes: 0 }], false).await;
    let (_, back) = read(&mut s, "fix_ts", None, None).await;
    assert_eq!(sorted(&back), [vec![Cell::Int(1), Cell::DateTime("2024-01-01 00:00:00.1234567".into())]]);

    // 4. Vendor types: xml past 4 000 characters and sql_variant.
    for t in ["fix_vsrc", "fix_vdst"] {
        run(&mut s, &format!("CREATE TABLE dbo.{t} (id int NOT NULL PRIMARY KEY, x xml, v sql_variant)")).await;
    }
    run(
        &mut s,
        "INSERT INTO dbo.fix_vsrc VALUES (1, CAST(N'<a>' + REPLICATE(CAST(N'b' AS nvarchar(max)), 5000) + N'</a>' AS xml), CAST(CAST(1.50 AS decimal(3,2)) AS sql_variant))
         INSERT INTO dbo.fix_vsrc VALUES (2, N'<c/>', CAST(N'a' AS sql_variant))
         INSERT INTO dbo.fix_vsrc VALUES (3, NULL, NULL)",
    )
    .await;
    let (vcols, vbatches) = read(&mut s, "fix_vsrc", None, None).await;
    let vsrc = sorted(&vbatches);
    let (n, _) = load(&mut s, "fix_vdst", &vcols, vbatches, false).await;
    assert_eq!(n, 3);
    let (_, vback) = read(&mut s, "fix_vdst", None, None).await;
    assert_eq!(sorted(&vback), vsrc);
    let rows = vec![vec![Cell::Int(4), Cell::Null, Cell::Text("a".into())]];
    load(&mut s, "fix_vdst", &vcols, vec![RowBatch { rows, bytes: 0 }], false).await;

    // 5. CLR types travel as their binary form and load back.
    for t in ["fix_usrc", "fix_udst"] {
        run(&mut s, &format!("CREATE TABLE dbo.{t} (id int NOT NULL PRIMARY KEY, h hierarchyid, g geography)")).await;
    }
    run(
        &mut s,
        "INSERT INTO dbo.fix_usrc VALUES (1, hierarchyid::Parse('/1/2/'), geography::Point(1, 2, 4326)), (2, NULL, NULL)",
    )
    .await;
    let (ucols, ubatches) = read(&mut s, "fix_usrc", None, None).await;
    let usrc = sorted(&ubatches);
    assert!(matches!(&usrc[0][1], Cell::Bytes(b) if !b.is_empty()), "{:?}", usrc[0]);
    assert!(matches!(&usrc[0][2], Cell::Bytes(b) if !b.is_empty()), "{:?}", usrc[0]);
    load(&mut s, "fix_udst", &ucols, ubatches, false).await;
    let (_, uback) = read(&mut s, "fix_udst", None, None).await;
    assert_eq!(sorted(&uback), usrc);
    assert_eq!(
        scalar(&mut s, "SELECT COUNT(*) FROM dbo.fix_udst WHERE h.ToString() = '/1/2/' AND g.Lat = 1 AND g.Long = 2").await,
        1
    );

    for t in tables {
        run(&mut s, &format!("DROP TABLE dbo.{t}")).await;
    }
}
