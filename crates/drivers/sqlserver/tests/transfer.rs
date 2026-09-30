//! Bulk transfer against a real server: typed reads, `INSERT BULK` loads
//! and native copies (raw TDS rows and decoded), checked with `EXCEPT`.
//! Reads `DBINE_TEST_SQLSERVER_URL` (`mssql://user:pass@host:port`), by
//! default the `dbine-test-sqlserver` container:
//!
//! ```sh
//! cargo test -p dbine-driver-sqlserver --test transfer -- --ignored --test-threads=1
//! # the 1M-row benchmark, in release:
//! cargo test --release -p dbine-driver-sqlserver --test transfer -- --ignored --nocapture --test-threads=1
//! ```

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::transfer::{BatchSink, BatchSource, Cell, CopySpec, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{async_trait, ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session};
use std::io;
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Instant;

const DEFAULT_URL: &str = "mssql://sa:Pw_12345!@localhost:25013";
const SRC_DB: &str = "dbine_xfer_src";
const DST_DB: &str = "dbine_xfer_dst";

fn config() -> ConnectionConfig {
    let url = std::env::var("DBINE_TEST_SQLSERVER_URL").unwrap_or_else(|_| DEFAULT_URL.into());
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hostport.rsplit_once(':').unwrap();
    ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    }
}

fn driver() -> Arc<dyn Driver> {
    dbine_driver_sqlserver::drivers().remove(0)
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{sql}"));
    if let Some(e) = &out.error {
        panic!("{e}\n{sql}");
    }
    out
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> serde_json::Value {
    run(s, sql).await.results[0].rows[0][0].clone()
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: name.into() }
}

async fn fresh_databases() {
    let mut admin = driver().connect(&config(), Some("master")).await.expect("connect");
    for db in [SRC_DB, DST_DB] {
        run(
            &mut admin,
            &format!(
                "IF DB_ID('{db}') IS NOT NULL BEGIN ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]; END
                 GO
                 CREATE DATABASE [{db}]
                 GO
                 ALTER DATABASE [{db}] SET RECOVERY SIMPLE"
            ),
        )
        .await;
    }
}

/// Which copy mode the driver logged (`raw` / `decoded`).
fn modes() -> &'static Mutex<Vec<String>> {
    static MODES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    MODES.get_or_init(|| {
        struct Capture;
        struct Visit(Option<String>);
        impl tracing::field::Visit for Visit {
            fn record_str(&mut self, f: &tracing::field::Field, v: &str) {
                if f.name() == "mode" {
                    self.0 = Some(v.into());
                }
            }
            fn record_debug(&mut self, f: &tracing::field::Field, v: &dyn std::fmt::Debug) {
                if f.name() == "mode" {
                    self.0 = Some(format!("{v:?}").trim_matches('"').into());
                }
            }
        }
        impl tracing::Subscriber for Capture {
            fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
                true
            }
            fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }
            fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
            fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
            fn event(&self, e: &tracing::Event<'_>) {
                let mut v = Visit(None);
                e.record(&mut v);
                if let Some(m) = v.0 {
                    modes().lock().unwrap().push(m);
                }
            }
            fn enter(&self, _: &tracing::span::Id) {}
            fn exit(&self, _: &tracing::span::Id) {}
        }
        let _ = tracing::subscriber::set_global_default(Capture);
        Mutex::new(Vec::new())
    })
}

fn last_mode() -> String {
    modes().lock().unwrap().last().cloned().unwrap_or_default()
}

/// Batches read into memory.
#[derive(Default)]
struct Collect {
    columns: Vec<TransferColumn>,
    batches: Vec<RowBatch>,
}
impl BatchSink for Collect {
    fn begin(&mut self, c: &[TransferColumn]) -> io::Result<()> {
        self.columns = c.to_vec();
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> io::Result<()> {
        self.batches.push(b);
        Ok(())
    }
}

struct Replay(std::vec::IntoIter<RowBatch>);
#[async_trait]
impl BatchSource for Replay {
    async fn next(&mut self) -> Option<RowBatch> {
        self.0.next()
    }
}

/// Batches handed from a reader thread (its own runtime) through a bounded channel (the
/// reader blocks while the loader is behind).
struct Channel(mpsc::SyncSender<RowBatch>);
impl BatchSink for Channel {
    fn begin(&mut self, _: &[TransferColumn]) -> io::Result<()> {
        Ok(())
    }
    fn batch(&mut self, b: RowBatch) -> io::Result<()> {
        self.0.send(b).map_err(|_| io::Error::other("closed"))
    }
}
struct Receive(Arc<Mutex<mpsc::Receiver<RowBatch>>>);
#[async_trait]
impl BatchSource for Receive {
    async fn next(&mut self) -> Option<RowBatch> {
        let rx = self.0.clone();
        tokio::task::spawn_blocking(move || rx.lock().unwrap().recv().ok()).await.unwrap()
    }
}

async fn copy_native(spec: &CopySpec, decoded: bool) -> u64 {
    if decoded {
        std::env::set_var("DBINE_SQLSERVER_NO_RAW", "1");
    } else {
        std::env::remove_var("DBINE_SQLSERVER_NO_RAW");
    }
    let d = driver();
    assert!(d.supports_bulk_load() && d.supports_native_copy("sqlserver") && d.supports_native_copy("azuresql"));
    // The source through the app's read-only wrapper.
    let mut src: Box<dyn Session> = Box::new(ReadOnlySession::new(d.connect(&config(), Some(SRC_DB)).await.unwrap()));
    let mut dst = d.connect(&config(), Some(DST_DB)).await.unwrap();
    let last = Mutex::new(0u64);
    let n = d
        .copy_native(&mut *src, &mut *dst, spec, &|c| {
            let mut l = last.lock().unwrap();
            assert!(c > *l);
            *l = c;
        })
        .await
        .unwrap();
    std::env::remove_var("DBINE_SQLSERVER_NO_RAW");
    assert_eq!(*last.lock().unwrap(), n);
    n
}

// ------------------------------------------------------------ every type

/// (column, type, how EXCEPT compares it: exact bytes where equality is
/// looser than identity: collations, offsets, LOBs.)
const COLUMNS: &[(&str, &str)] = &[
    ("id", "int IDENTITY(1,1) PRIMARY KEY"),
    ("c_bit", "bit NULL"),
    ("c_tiny", "tinyint NULL"),
    ("c_small", "smallint NULL"),
    ("c_int", "int NOT NULL"),
    ("c_big", "bigint NULL"),
    ("c_dec", "decimal(38,10) NULL"),
    ("c_num", "numeric(9,2) NOT NULL"),
    ("c_money", "money NULL"),
    ("c_money_nn", "money NOT NULL"),
    ("c_smoney", "smallmoney NULL"),
    ("c_float", "float NULL"),
    ("c_real", "real NULL"),
    ("c_char", "char(10) NULL"),
    ("c_vchar", "varchar(50) NULL"),
    ("c_vcmax", "varchar(max) NULL"),
    ("c_nchar", "nchar(10) NULL"),
    ("c_nvchar", "nvarchar(4000) NOT NULL"),
    ("c_nvmax", "nvarchar(max) NULL"),
    ("c_text", "text NULL"),
    ("c_ntext", "ntext NULL"),
    ("c_bin", "binary(8) NULL"),
    ("c_vbin", "varbinary(100) NULL"),
    ("c_vbmax", "varbinary(max) NULL"),
    ("c_image", "image NULL"),
    ("c_guid", "uniqueidentifier NULL"),
    ("c_date", "date NULL"),
    ("c_time", "time(7) NULL"),
    ("c_time3", "time(3) NULL"),
    ("c_dt", "datetime NULL"),
    ("c_dt_nn", "datetime NOT NULL"),
    ("c_sdt", "smalldatetime NULL"),
    ("c_sdt_nn", "smalldatetime NOT NULL"),
    ("c_dt2", "datetime2(7) NULL"),
    ("c_dt2_0", "datetime2(0) NULL"),
    ("c_dto", "datetimeoffset(7) NULL"),
    ("c_dto3", "datetimeoffset(3) NULL"),
    ("c_xml", "xml NULL"),
    ("c_xml_nn", "xml NOT NULL"),
    ("c_geo", "geography NULL"),
    ("c_geom", "geometry NULL"),
    ("c_hid", "hierarchyid NULL"),
];

fn create_all_types(name: &str) -> String {
    let cols: Vec<String> = COLUMNS.iter().map(|(n, t)| format!("[{n}] {t}")).collect();
    format!("CREATE TABLE dbo.[{name}] ({}, c_calc AS (CAST(c_int AS bigint) * 2), c_rv rowversion)", cols.join(", "))
}

fn compare_list() -> String {
    let mut out: Vec<String> = COLUMNS
        .iter()
        .map(|(n, t)| {
            let ty = t.split([' ', '(']).next().unwrap();
            match ty {
                "char" | "varchar" | "nchar" | "nvarchar" | "image" | "geography" | "geometry" | "hierarchyid" => {
                    format!("CAST([{n}] AS varbinary(max)) AS [{n}]")
                }
                "text" => format!("CAST(CAST([{n}] AS varchar(max)) AS varbinary(max)) AS [{n}]"),
                "ntext" | "xml" => format!("CAST(CAST([{n}] AS nvarchar(max)) AS varbinary(max)) AS [{n}]"),
                "datetimeoffset" => format!("CAST([{n}] AS nvarchar(40)) AS [{n}]"),
                _ => format!("[{n}]"),
            }
        })
        .collect();
    out.push("c_calc".into());
    out.join(", ")
}

async fn except_both_ways(s: &mut Box<dyn Session>, src: &str, dst: &str) -> (i64, i64) {
    let list = compare_list();
    let a = format!("SELECT {list} FROM [{SRC_DB}].dbo.[{src}]");
    let b = format!("SELECT {list} FROM [{DST_DB}].dbo.[{dst}]");
    let n = |v: serde_json::Value| v.as_i64().unwrap();
    let ab = n(scalar(s, &format!("SELECT COUNT_BIG(*) FROM ({a} EXCEPT {b}) x")).await);
    let ba = n(scalar(s, &format!("SELECT COUNT_BIG(*) FROM ({b} EXCEPT {a}) x")).await);
    (ab, ba)
}

const ROWS_SQL: &str = "
SET IDENTITY_INSERT dbo.alltypes ON;
INSERT INTO dbo.alltypes (id, c_bit, c_tiny, c_small, c_int, c_big, c_dec, c_num, c_money, c_money_nn, c_smoney, c_float, c_real,
    c_char, c_vchar, c_vcmax, c_nchar, c_nvchar, c_nvmax, c_text, c_ntext, c_bin, c_vbin, c_vbmax, c_image, c_guid,
    c_date, c_time, c_time3, c_dt, c_dt_nn, c_sdt, c_sdt_nn, c_dt2, c_dt2_0, c_dto, c_dto3, c_xml, c_xml_nn, c_geo, c_geom, c_hid)
VALUES
 (10, 1, 255, -32768, -2147483648, -9223372036854775808, -1234567890123456789012345678.0123456789, -9999999.99,
  922337203685477.5807, -922337203685477.5808, 214748.3647, 1.7976931348623157E+308, -3.4E+38,
  'abc', 'Árbol ñ', REPLICATE(CAST('x' AS varchar(max)), 100000), N'ñandú', N'日本語 ☃ 🎉', REPLICATE(CAST(N'ü' AS nvarchar(max)), 70000),
  'texto viejo', N'ntexto ☃', 0x0102030405060708, 0x00FF, CAST(REPLICATE(CAST('ab' AS varchar(max)), 2621440) AS varbinary(max)), 0xDEADBEEF,
  '6F9619FF-8B86-D011-B42D-00C04FC964FF',
  '0001-01-01', '23:59:59.9999999', '00:00:00.001', '1753-01-01 00:00:00.003', '9999-12-31 23:59:59.997', '1900-01-01 00:00', '2079-06-06 23:59',
  '9999-12-31 23:59:59.9999999', '2024-02-29 12:34:56', '2024-02-29 23:30:00.1234567 -03:00', '0001-01-01 14:00:00.000 +14:00',
  N'<a x=\"1\"><b>ñ</b></a>', N'<r/>', geography::Point(-34.6, -58.4, 4326), geometry::STGeomFromText('LINESTRING (0 0, 1 1)', 0),
  hierarchyid::Parse('/1/2/')),
 (20, NULL, NULL, NULL, 0, NULL, NULL, 0, NULL, 0, NULL, NULL, NULL,
  NULL, NULL, NULL, NULL, N'', NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
  NULL, NULL, NULL, NULL, '1900-01-01', NULL, '1900-01-01', NULL, NULL, NULL, NULL, NULL, N'<empty/>', NULL, NULL, NULL),
 (30, 0, 0, 32767, 2147483647, 9223372036854775807, 0.0000000001, 9999999.99,
  -922337203685477.5808, 922337203685477.5807, -214748.3648, -2.2250738585072014E-308, 1.17549435E-38,
  '', '', '', N'', N'  trailing  ', N'', '', N'', 0x, 0x, 0x, 0x,
  '00000000-0000-0000-0000-000000000000',
  '9999-12-31', '00:00:00', '12:00:00.5', '2000-02-29 12:00:00.007', '2000-01-01', '2079-06-06 23:59', '1900-01-01',
  '0001-01-01', '0001-01-01', '0001-01-01 00:00:00 -14:00', '9999-12-31 23:59:59.999 +00:00',
  N'<a/>', N'<b>x</b>', NULL, NULL, hierarchyid::GetRoot());
SET IDENTITY_INSERT dbo.alltypes OFF;
INSERT INTO dbo.alltypes (c_int, c_num, c_money_nn, c_nvchar, c_dt_nn, c_sdt_nn, c_xml_nn, c_money, c_dto3)
VALUES (7, 1.5, 0.0001, N'after gap', '2024-01-01', '2024-01-01', N'<z/>', 0.0001, '2024-06-01 10:00:00.123 +05:30');
";

fn insertable() -> Vec<String> {
    COLUMNS.iter().map(|(n, _)| n.to_string()).collect()
}

fn load_spec(name: &str, commit_rows: u64) -> LoadSpec {
    LoadSpec {
        table: table(name),
        columns: insertable(),
        table_lock: true,
        keep_identity: true,
        commit_rows,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_every_type() {
    modes();
    fresh_databases().await;
    let mut src = driver().connect(&config(), Some(SRC_DB)).await.unwrap();
    let mut dst = driver().connect(&config(), Some(DST_DB)).await.unwrap();
    // Sessions ask for 32767-byte packets (the server grants 32576).
    let packet = scalar(&mut dst, "SELECT net_packet_size FROM sys.dm_exec_connections WHERE session_id = @@SPID").await;
    assert!(packet.as_i64().unwrap() > 32_000, "packet size {packet}");
    run(&mut src, &create_all_types("alltypes")).await;
    run(&mut src, ROWS_SQL).await;
    for t in ["t_bulk", "t_raw", "t_dec"] {
        run(&mut dst, &create_all_types(t)).await;
    }

    // 1. read_batches → bulk_load, two rows per commit window.
    let mut ro = ReadOnlySession::new(driver().connect(&config(), Some(SRC_DB)).await.unwrap());
    let sink = Arc::new(Mutex::new(Collect::default()));
    let read = ro.read_batches(&ReadSpec { table: table("alltypes"), columns: None, filter: None }, sink.clone()).await.unwrap();
    assert_eq!(read, 4);
    let collected = std::mem::take(&mut *sink.lock().unwrap());
    let names: Vec<&str> = collected.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, insertable(), "computed and rowversion columns are skipped");
    let rows: Vec<&Vec<Cell>> = collected.batches.iter().flat_map(|b| &b.rows).collect();
    let at = |row: usize, name: &str| rows[row][names.iter().position(|n| *n == name).unwrap()].clone();
    assert_eq!(at(0, "c_money"), Cell::Decimal("922337203685477.5807".into()));
    assert_eq!(at(0, "c_money_nn"), Cell::Decimal("-922337203685477.5808".into()));
    assert_eq!(at(0, "c_dec"), Cell::Decimal("-1234567890123456789012345678.0123456789".into()));
    assert_eq!(at(0, "c_dto"), Cell::DateTimeTz("2024-02-29 23:30:00.1234567-03:00".into()));
    assert_eq!(at(0, "c_dto3"), Cell::DateTimeTz("0001-01-01 14:00:00.000+14:00".into()));
    assert_eq!(at(0, "c_time"), Cell::Time("23:59:59.9999999".into()));
    assert_eq!(at(0, "c_dt"), Cell::DateTime("1753-01-01 00:00:00.0033333".into()));
    assert_eq!(at(0, "c_date"), Cell::Date("0001-01-01".into()));
    assert_eq!(at(0, "c_guid"), Cell::Uuid("6F9619FF-8B86-D011-B42D-00C04FC964FF".into()));
    assert_eq!(at(0, "c_bit"), Cell::Bool(true));
    assert!(matches!(at(0, "c_vbmax"), Cell::Bytes(b) if b.len() == 5 * 1024 * 1024));
    assert!(matches!(at(0, "c_xml"), Cell::Text(t) if t.contains("<b>ñ</b>")));
    assert_eq!(at(1, "c_int"), Cell::Int(0));
    assert_eq!(at(1, "c_money"), Cell::Null);
    let progress = Mutex::new(Vec::new());
    let loaded = dst
        .bulk_load(&load_spec("t_bulk", 2), &collected.columns, &mut Replay(collected.batches.into_iter()), &|c| progress.lock().unwrap().push(c))
        .await
        .unwrap();
    assert_eq!(loaded, 4);
    assert_eq!(*progress.lock().unwrap().last().unwrap(), 4);

    // 2. Native copy, raw TDS rows.
    let spec = |t: &str| CopySpec { source: ReadSpec { table: table("alltypes"), columns: None, filter: None }, target: load_spec(t, 3) };
    assert_eq!(copy_native(&spec("t_raw"), false).await, 4);
    assert_eq!(last_mode(), "raw");
    // 3. Native copy, decoded.
    assert_eq!(copy_native(&spec("t_dec"), true).await, 4);
    assert_eq!(last_mode(), "decoded");

    for t in ["t_bulk", "t_raw", "t_dec"] {
        let diff = except_both_ways(&mut dst, "alltypes", t).await;
        eprintln!("EXCEPT alltypes vs {t}: {diff:?}");
        assert_eq!(diff, (0, 0), "{t} differs from the source");
        let n = scalar(&mut dst, &format!("SELECT COUNT(*) FROM dbo.[{t}]")).await;
        assert_eq!(n, serde_json::json!(4));
    }

    // A filtered read, and the load refusing what it can't write.
    let spec = CopySpec {
        source: ReadSpec { table: table("alltypes"), columns: Some(vec!["id".into(), "c_int".into()]), filter: Some("id > 10".into()) },
        target: LoadSpec { columns: vec!["id".into(), "c_int".into()], ..load_spec("t_bulk", 100) },
    };
    run(&mut dst, "CREATE TABLE dbo.small (id int IDENTITY PRIMARY KEY, c_int int NOT NULL)").await;
    let spec = CopySpec { target: LoadSpec { table: table("small"), ..spec.target }, ..spec };
    assert_eq!(copy_native(&spec, false).await, 3);
    let bad = LoadSpec { keep_identity: false, ..spec.target.clone() };
    let e = dst.bulk_load(&bad, &[], &mut Replay(Vec::new().into_iter()), &|_| {}).await.unwrap_err();
    assert!(e.to_string().contains("identidad"), "{e}");
    let calc = LoadSpec { columns: vec!["c_calc".into()], ..load_spec("t_bulk", 100) };
    assert!(dst.bulk_load(&calc, &[], &mut Replay(Vec::new().into_iter()), &|_| {}).await.is_err());
    // The session is usable after a refused load.
    assert_eq!(scalar(&mut dst, "SELECT COUNT(*) FROM dbo.small").await, serde_json::json!(3));
}

// ------------------------------------------------------------ benchmark

const MIXED: &str = "(id int IDENTITY PRIMARY KEY, a int NOT NULL, b bigint NULL, c nvarchar(50) NOT NULL, d varchar(100) NULL,
    e decimal(18,4) NULL, f datetime2(3) NOT NULL, g float NULL, h bit NOT NULL, i uniqueidentifier NULL, j date NULL, k money NULL)";

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn transfer_benchmark_1m() {
    modes();
    let rows: u64 = std::env::var("DBINE_BENCH_ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    let mut admin = driver().connect(&config(), Some("master")).await.unwrap();
    for db in ["dbine_bench_src", "dbine_bench_dst"] {
        run(
            &mut admin,
            &format!(
                "IF DB_ID('{db}') IS NOT NULL BEGIN ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]; END
                 GO
                 CREATE DATABASE [{db}]
                 GO
                 ALTER DATABASE [{db}] SET RECOVERY SIMPLE"
            ),
        )
        .await;
    }
    let mut src = driver().connect(&config(), Some("dbine_bench_src")).await.unwrap();
    run(&mut src, &format!("CREATE TABLE dbo.mixed {MIXED}")).await;
    let t = Instant::now();
    run(
        &mut src,
        &format!(
            "WITH n AS (SELECT TOP ({rows}) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS r FROM sys.all_columns a CROSS JOIN sys.all_columns b CROSS JOIN sys.all_columns c)
             INSERT INTO dbo.mixed WITH (TABLOCK) (a, b, c, d, e, f, g, h, i, j, k)
             SELECT CAST(r AS int), CASE WHEN r % 7 = 0 THEN NULL ELSE r * 1000003 END, CONCAT(N'nombre ', r), CASE WHEN r % 5 = 0 THEN NULL ELSE CONCAT('dato-', r, '-', r * 3) END,
                    CAST(r AS decimal(18,4)) / 7, DATEADD(SECOND, CAST(r AS int), '2020-01-01'), r / 3.0, CAST(r % 2 AS bit), NEWID(),
                    DATEADD(DAY, CAST(r % 3000 AS int), '2000-01-01'), CAST(r AS money) / 100
               FROM n"
        ),
    )
    .await;
    eprintln!("generated {rows} rows in {:.1?}", t.elapsed());
    let mut dst = driver().connect(&config(), Some("dbine_bench_dst")).await.unwrap();
    let cols: Vec<String> = ["id", "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k"].iter().map(|s| s.to_string()).collect();
    let load = |t: &str| LoadSpec {
        table: table(t),
        columns: cols.clone(),
        table_lock: true,
        keep_identity: true,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let copy = |t: &str| CopySpec { source: ReadSpec { table: table("mixed"), columns: None, filter: None }, target: load(t) };
    let mut report = Vec::new();
    for (path, t) in [("copy_native raw", "m_raw"), ("copy_native decoded", "m_dec"), ("read_batches → bulk_load", "m_bulk")] {
        run(&mut dst, &format!("CREATE TABLE dbo.{t} {MIXED}")).await;
        let start = Instant::now();
        let n = match path {
            "copy_native raw" => {
                let n = copy_native_bench(&copy(t), false).await;
                assert_eq!(last_mode(), "raw");
                n
            }
            "copy_native decoded" => {
                let n = copy_native_bench(&copy(t), true).await;
                assert_eq!(last_mode(), "decoded");
                n
            }
            _ => bench_read_then_load(&load(t)).await,
        };
        let secs = start.elapsed().as_secs_f64();
        assert_eq!(n, rows);
        let line = format!("{path:<26} {n} rows in {secs:.2} s = {:.0} rows/s", n as f64 / secs);
        eprintln!("{line}");
        report.push(line);
        let list = cols.join(", ");
        let diff = scalar(
            &mut dst,
            &format!(
                "SELECT (SELECT COUNT_BIG(*) FROM (SELECT {list} FROM dbine_bench_src.dbo.mixed EXCEPT SELECT {list} FROM dbo.{t}) x)
                      + (SELECT COUNT_BIG(*) FROM (SELECT {list} FROM dbo.{t} EXCEPT SELECT {list} FROM dbine_bench_src.dbo.mixed) y)"
            ),
        )
        .await;
        assert_eq!(diff, serde_json::json!(0), "{t} differs");
    }
    // Where the ceiling is: the server copying on its own, the read alone
    // and the load alone (from batches already in memory).
    run(&mut dst, &format!("CREATE TABLE dbo.m_server {MIXED}")).await;
    let start = Instant::now();
    run(
        &mut dst,
        &format!(
            "SET IDENTITY_INSERT dbo.m_server ON;
             INSERT INTO dbo.m_server WITH (TABLOCK) ({0}) SELECT {0} FROM dbine_bench_src.dbo.mixed;
             SET IDENTITY_INSERT dbo.m_server OFF",
            cols.join(", ")
        ),
    )
    .await;
    report.push(format!("{:<26} {:.0} rows/s", "server INSERT…SELECT", rows as f64 / start.elapsed().as_secs_f64()));
    let sink = Arc::new(Mutex::new(Collect::default()));
    let mut ro = ReadOnlySession::new(driver().connect(&config(), Some("dbine_bench_src")).await.unwrap());
    let start = Instant::now();
    ro.read_batches(&ReadSpec { table: table("mixed"), columns: None, filter: None }, sink.clone()).await.unwrap();
    report.push(format!("{:<26} {:.0} rows/s", "read_batches only", rows as f64 / start.elapsed().as_secs_f64()));
    let batches = std::mem::take(&mut sink.lock().unwrap().batches);
    run(&mut dst, &format!("CREATE TABLE dbo.m_mem {MIXED}")).await;
    let start = Instant::now();
    dst.bulk_load(&load("m_mem"), &[], &mut Replay(batches.into_iter()), &|_| {}).await.unwrap();
    report.push(format!("{:<26} {:.0} rows/s", "bulk_load only (memory)", rows as f64 / start.elapsed().as_secs_f64()));
    eprintln!("\n{}", report.join("\n"));
    // Free the server's memory (the test container shares a VM with others).
    drop((src, dst, ro));
    for db in ["dbine_bench_src", "dbine_bench_dst"] {
        run(&mut admin, &format!("ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]")).await;
    }
}

async fn copy_native_bench(spec: &CopySpec, decoded: bool) -> u64 {
    if decoded {
        std::env::set_var("DBINE_SQLSERVER_NO_RAW", "1");
    } else {
        std::env::remove_var("DBINE_SQLSERVER_NO_RAW");
    }
    let d = driver();
    let mut src: Box<dyn Session> = Box::new(ReadOnlySession::new(d.connect(&config(), Some("dbine_bench_src")).await.unwrap()));
    let mut dst = d.connect(&config(), Some("dbine_bench_dst")).await.unwrap();
    let n = d.copy_native(&mut *src, &mut *dst, spec, &|_| {}).await.unwrap();
    std::env::remove_var("DBINE_SQLSERVER_NO_RAW");
    n
}

/// `read_batches` on its own thread, piped into `bulk_load`.
async fn bench_read_then_load(load: &LoadSpec) -> u64 {
    let (tx, rx) = mpsc::sync_channel(16);
    let reader = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let src = driver().connect(&config(), Some("dbine_bench_src")).await.unwrap();
            let mut src = ReadOnlySession::new(src);
            let spec = ReadSpec { table: table("mixed"), columns: None, filter: None };
            src.read_batches(&spec, Arc::new(Mutex::new(Channel(tx)))).await.unwrap()
        })
    });
    let mut dst = driver().connect(&config(), Some("dbine_bench_dst")).await.unwrap();
    let loaded = dst.bulk_load(load, &[], &mut Receive(Arc::new(Mutex::new(rx))), &|_| {}).await.unwrap();
    assert_eq!(reader.join().unwrap(), loaded);
    loaded
}

// ------------------------------------------------ failures and edge values

/// A load that fails after it started (a key the target already has) ends
/// the copy with that error, even with far more of the source left than the
/// read-ahead holds.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn copy_native_fails_fast_when_the_load_fails() {
    modes();
    fresh_databases().await;
    let mut src = driver().connect(&config(), Some(SRC_DB)).await.unwrap();
    let mut dst = driver().connect(&config(), Some(DST_DB)).await.unwrap();
    // 40 chunks of 1000 rows: well over the 16 the reader may be ahead.
    run(
        &mut src,
        "CREATE TABLE dbo.dup (id int NOT NULL PRIMARY KEY, v varchar(20) NOT NULL);
         WITH n AS (SELECT TOP (40000) ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS i FROM sys.all_objects a CROSS JOIN sys.all_objects b)
         INSERT INTO dbo.dup (id, v) SELECT CAST(i AS int), CONCAT('fila ', i) FROM n",
    )
    .await;
    run(&mut dst, "CREATE TABLE dbo.dup (id int NOT NULL PRIMARY KEY, v varchar(20) NOT NULL); INSERT INTO dbo.dup VALUES (5, 'ya estaba')").await;
    let spec = CopySpec {
        source: ReadSpec { table: table("dup"), columns: None, filter: None },
        target: LoadSpec { table: table("dup"), columns: vec!["id".into(), "v".into()], table_lock: false, keep_identity: false, commit_rows: 1000, commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES },
    };
    let d = driver();
    for decoded in [false, true] {
        if decoded {
            std::env::set_var("DBINE_SQLSERVER_NO_RAW", "1");
        } else {
            std::env::remove_var("DBINE_SQLSERVER_NO_RAW");
        }
        let mut s = d.connect(&config(), Some(SRC_DB)).await.unwrap();
        let mut t = d.connect(&config(), Some(DST_DB)).await.unwrap();
        let copy = d.copy_native(&mut *s, &mut *t, &spec, &|_| {});
        let res = tokio::time::timeout(std::time::Duration::from_secs(60), copy).await;
        std::env::remove_var("DBINE_SQLSERVER_NO_RAW");
        let e = res.unwrap_or_else(|_| panic!("the copy hung after the load failed (decoded: {decoded})")).unwrap_err();
        assert!(e.to_string().contains("PRIMARY KEY") || e.to_string().contains("duplicate"), "{e}");
        assert_eq!(last_mode(), if decoded { "decoded" } else { "raw" });
        // Both sessions still work after the failed copy.
        assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM dbo.dup").await.as_i64(), Some(40000));
        assert_eq!(scalar(&mut t, "SELECT COUNT(*) FROM dbo.dup").await.as_i64(), Some(1));
    }
}

/// decimal(38,38) through the decoded copy (38 fractional digits).
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn copy_native_decimal_38_38() {
    modes();
    fresh_databases().await;
    let mut src = driver().connect(&config(), Some(SRC_DB)).await.unwrap();
    let mut dst = driver().connect(&config(), Some(DST_DB)).await.unwrap();
    let create = "CREATE TABLE dbo.dec38 (id int NOT NULL PRIMARY KEY, d decimal(38,38) NULL)";
    run(&mut src, create).await;
    run(
        &mut src,
        "INSERT INTO dbo.dec38 SELECT id, CAST(v AS decimal(38,38)) FROM (VALUES (1, '0.12345678901234567890123456789012345678'),
         (2, '-0.99999999999999999999999999999999999999'), (3, '0.00000000000000000000000000000000000001'), (4, '0'), (5, NULL)) x(id, v)",
    )
    .await;
    run(&mut dst, create).await;
    let spec = CopySpec {
        source: ReadSpec { table: table("dec38"), columns: None, filter: None },
        target: LoadSpec { table: table("dec38"), columns: vec!["id".into(), "d".into()], table_lock: true, keep_identity: false, commit_rows: 100, commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES },
    };
    assert_eq!(copy_native(&spec, true).await, 5);
    assert_eq!(last_mode(), "decoded");
    let diff = |a: &str, b: &str| format!("SELECT COUNT_BIG(*) FROM (SELECT id, d FROM [{a}].dbo.dec38 EXCEPT SELECT id, d FROM [{b}].dbo.dec38) x");
    assert_eq!(scalar(&mut dst, &diff(SRC_DB, DST_DB)).await.as_i64(), Some(0));
    assert_eq!(scalar(&mut dst, &diff(DST_DB, SRC_DB)).await.as_i64(), Some(0));
}
