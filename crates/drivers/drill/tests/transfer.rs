//! Typed read against a real Apache Drill (see `integration.rs` for the
//! container); Drill has no bulk load:
//! `DBINE_TEST_DRILL_URL=http://localhost:25847 \
//!  cargo test -p dbine-driver-drill --release -- --ignored transfer --nocapture`.

use dbine_driver::transfer::{BatchSink, BatchSource, Cell, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const ROWS: usize = 200_000;

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_DRILL_URL").ok()?).expect("URL");
    Some(ConnectionConfig { driver: "drill".into(), host: url.host_str()?.into(), port: url.port().unwrap_or(0), ..Default::default() })
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(text, 10, &mut out).await {
        panic!("{text}: {e}");
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

struct Nothing;

#[dbine_driver::async_trait]
impl BatchSource for Nothing {
    async fn next(&mut self) -> Option<RowBatch> {
        None
    }
}

#[tokio::test]
#[ignore]
async fn drill_transfer() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_drill::drivers().remove(0);
    assert!(!d.supports_bulk_load());
    let mut s = d.connect(&c, None).await.unwrap();
    run(&mut s, "DROP TABLE IF EXISTS dfs.tmp.`dbine_xfer`").await;
    run(
        &mut s,
        &format!(
            "CREATE TABLE dfs.tmp.`dbine_xfer` AS
             SELECT CAST(a.employee_id * 10000 + b.employee_id AS BIGINT) AS id,
                    CAST(a.salary AS DECIMAL(38, 4)) + CAST(b.employee_id AS DECIMAL(38, 4)) / 10000 + 12345678901234567 AS d,
                    a.salary / 7 AS f, CONCAT(a.full_name, ' ñ ', b.full_name) AS s,
                    CAST(a.hire_date AS TIMESTAMP) AS ts, CAST(a.birth_date AS DATE) AS dt, CAST(a.full_name AS VARBINARY) AS b,
                    a.employee_id > 500 AS ok
             FROM cp.`employee.json` a JOIN cp.`employee.json` b ON MOD(a.employee_id, 2) = MOD(b.employee_id, 2) LIMIT {ROWS}"
        ),
    )
    .await;
    let obj = ObjectRef { kind: "table".into(), schema: Some("dfs.tmp".into()), name: "dbine_xfer".into() };

    let sink = Arc::new(Mutex::new(Collect::default()));
    let t = Instant::now();
    let read = s.read_batches(&ReadSpec { table: obj.clone(), columns: None, filter: None }, sink.clone()).await.unwrap();
    let secs = t.elapsed().as_secs_f64();
    println!("drill: read {read} rows in {secs:.2}s = {:.0} rows/s", read as f64 / secs);
    assert_eq!(read, ROWS as u64);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    let names: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["id", "d", "f", "s", "ts", "dt", "b", "ok"]);
    assert!(got.columns[1].type_name.starts_with("VARDECIMAL"), "{:?}", got.columns[1]);

    // Against the grid's values for the same rows: same data, typed.
    let mut out = QueryOutcome::default();
    s.execute("SELECT id, CAST(d AS VARCHAR) d, s, b FROM dfs.tmp.`dbine_xfer` WHERE id = 10001", 10, &mut out).await.unwrap();
    let grid = &out.results[0].rows[0];
    let row = got.rows.iter().find(|r| r[0] == Cell::Int(10001)).expect("row 10001");
    assert_eq!(row[1], Cell::Decimal(grid[1].as_str().unwrap().to_string()), "exact decimal");
    assert_eq!(row[3], Cell::Text(grid[2].as_str().unwrap().to_string()));
    assert!(matches!(&row[6], Cell::Bytes(b) if b == b"Sheri Nowmer"), "{:?}", row[6]);
    assert!(matches!(&row[4], Cell::DateTime(t) if t.starts_with("1994-12-01")), "{:?}", row[4]);
    assert!(matches!(&row[5], Cell::Date(d) if d.len() == 10), "{:?}", row[5]);
    assert!(matches!(row[2], Cell::Float(_)) && matches!(row[7], Cell::Bool(_)));

    // Some columns and a filter.
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table: obj.clone(), columns: Some(vec!["id".into(), "s".into()]), filter: Some("id IN (10001, 10003, 10005)".into()) };
    let mut out = QueryOutcome::default();
    s.execute("SELECT COUNT(*) FROM dfs.tmp.`dbine_xfer` WHERE id IN (10001, 10003, 10005)", 10, &mut out).await.unwrap();
    let want = out.results[0].rows[0][0].as_u64().unwrap();
    assert!(want > 0);
    assert_eq!(s.read_batches(&spec, sink.clone()).await.unwrap(), want);
    assert_eq!(sink.lock().unwrap().columns.len(), 2);

    // An unknown column is an error (Drill itself answers it with NULLs).
    let sink = Arc::new(Mutex::new(Collect::default()));
    let spec = ReadSpec { table: obj.clone(), columns: Some(vec!["nope".into(), "id".into()]), filter: None };
    assert!(matches!(s.read_batches(&spec, sink).await, Err(e) if e.is_query() && e.to_string().contains("nope")));

    // Lists (Drill types them by their element) and NaN/±Infinity (quoted).
    run(&mut s, "DROP TABLE IF EXISTS dfs.tmp.`dbine_xfer_l`").await;
    run(
        &mut s,
        "CREATE TABLE dfs.tmp.`dbine_xfer_l` AS
         SELECT CONVERT_FROM('[true,false]', 'JSON') AS ok, CONVERT_FROM('[1,3]', 'JSON') AS n, CONVERT_FROM('[1.5,2.25]', 'JSON') AS f,
                CAST('NaN' AS DOUBLE) AS nan, CAST('Infinity' AS DOUBLE) AS i, CAST('-Infinity' AS FLOAT) AS i4
         FROM (VALUES(1))",
    )
    .await;
    let sink = Arc::new(Mutex::new(Collect::default()));
    let lists = ObjectRef { kind: "table".into(), schema: Some("dfs.tmp".into()), name: "dbine_xfer_l".into() };
    assert_eq!(s.read_batches(&ReadSpec { table: lists, columns: None, filter: None }, sink.clone()).await.unwrap(), 1);
    let got = std::mem::take(&mut *sink.lock().unwrap());
    println!("lists: {:?} {:?}", got.columns, got.rows[0]);
    assert_eq!(got.rows[0][0], Cell::Json("[true,false]".into()));
    assert_eq!(got.rows[0][1], Cell::Json("[1,3]".into()));
    assert_eq!(got.rows[0][2], Cell::Json("[1.5,2.25]".into()));
    assert!(matches!(got.rows[0][3], Cell::Float(f) if f.is_nan()), "{:?}", got.rows[0][3]);
    assert_eq!(got.rows[0][4], Cell::Float(f64::INFINITY));
    assert_eq!(got.rows[0][5], Cell::Float(f64::NEG_INFINITY));
    run(&mut s, "DROP TABLE dfs.tmp.`dbine_xfer_l`").await;

    // A failing query is an error.
    let sink = Arc::new(Mutex::new(Collect::default()));
    let bad = ObjectRef { kind: "table".into(), schema: Some("dfs.tmp".into()), name: "no_existe".into() };
    assert!(matches!(s.read_batches(&ReadSpec { table: bad, columns: None, filter: None }, sink).await, Err(e) if e.is_query()));

    // No bulk load, with the reason.
    let load = LoadSpec {
        table: obj,
        columns: vec!["id".into()],
        table_lock: false,
        keep_identity: false,
        commit_rows: 1000,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    assert!(matches!(s.bulk_load(&load, &[], &mut Nothing, &|_| {}).await, Err(Error::Unsupported(m)) if m.contains("INSERT")));
    run(&mut s, "DROP TABLE dfs.tmp.`dbine_xfer`").await;
}
