//! The query compare on fixtures shaped like each engine's results (Power
//! BI's `executeQueries`, SQL Server's decimals as text) and on local DuckDB
//! queries (PostgreSQL-like numerics and dates): the rules by type, the
//! tolerance, NULL as BLANK, repeated keys, refused results, and the
//! in-memory and scratch-DuckDB paths giving the same result.

use super::*;
use crate::commands::data_compare::whole::{
    self, Compared, Ctl, How, Input, Options, Progress, Read, Rules, Source,
};
use crate::local_duckdb;
use dbine_driver::{ConnectionConfig, DbObject, Driver, DriverInfo, Session};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// A test's own folder, deleted when the test ends (passed or not).
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(name: &str) -> TempDir {
    let d = std::env::temp_dir().join(format!("dbine-dcmpq-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    TempDir(d)
}

fn leftovers(root: &Path) -> Vec<String> {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().display().to_string())
        .collect()
}

fn col(name: &str, type_name: &str) -> ResultColumn {
    ResultColumn {
        name: name.into(),
        type_name: type_name.into(),
    }
}

/// `[[…], […]]` as rows.
fn rows(v: Value) -> Vec<Vec<Value>> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|r| r.as_array().unwrap().clone())
        .collect()
}

/// A session that answers any query with fixed results, pushed the way a
/// driver pushes them (`begin_result`, `push_row`).
#[derive(Default)]
struct Fixture {
    results: Vec<(Vec<ResultColumn>, Vec<Vec<Value>>)>,
    /// The engine cut the result (an API's row cap) and says so.
    truncated: bool,
    /// The query never ends (until cancelled).
    hang: bool,
    /// Rows reach the sink as a driver host's do: on a thread of its own
    /// (the host's reader thread), not on the runtime.
    remote: bool,
    /// Rows the sink took, counted as they go.
    emitted: Option<Arc<AtomicU64>>,
    /// The engine cuts the result at the rows asked for and answers the
    /// cut as an error right after them, in the same poll (in process)
    /// or right after the last row (a host).
    cut_as_error: bool,
}

impl Fixture {
    /// As `RemoteSession` runs it: the sink gets the rows on another
    /// thread while the run waits for the reply, which brings the results
    /// without their rows.
    async fn execute_remote(
        &mut self,
        max_rows: usize,
        out: &mut QueryOutcome,
    ) -> dbine_driver::Result<()> {
        let sink = out.sink.clone().expect("a sink");
        let results = self.results.clone();
        let emitted = self.emitted.clone();
        let take = if self.cut_as_error {
            max_rows
        } else {
            usize::MAX
        };
        let reader = std::thread::spawn(move || {
            for (index, (columns, rows)) in results.iter().enumerate() {
                let mut s = sink.0.lock().unwrap();
                if s.begin(index, columns).is_err() {
                    return;
                }
                drop(s);
                for r in rows.iter().take(take) {
                    if sink.0.lock().unwrap().row(index, r).is_err() {
                        return;
                    }
                    if let Some(n) = &emitted {
                        n.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }
        });
        while !reader.is_finished() {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        reader.join().expect("the reader thread");
        if self.cut_as_error {
            return Err(dbine_driver::Error::Query("el resultado se cortó".into()));
        }
        for (columns, rows) in &self.results {
            out.results.push(dbine_driver::StatementResult {
                columns: columns.clone(),
                total_rows: rows.len() as u64,
                ..Default::default()
            });
        }
        Ok(())
    }

    fn one(columns: Vec<ResultColumn>, rows: Vec<Vec<Value>>) -> Fixture {
        Fixture {
            results: vec![(columns, rows)],
            ..Default::default()
        }
    }
}

#[dbine_driver::async_trait]
impl Session for Fixture {
    async fn server_version(&mut self) -> dbine_driver::Result<String> {
        Ok("fixture".into())
    }
    async fn list_databases(&mut self) -> dbine_driver::Result<Vec<String>> {
        Ok(Vec::new())
    }
    async fn list_objects(&mut self) -> dbine_driver::Result<Vec<DbObject>> {
        Ok(Vec::new())
    }
    async fn columns(&mut self, _: &ObjectRef) -> dbine_driver::Result<Vec<ColumnInfo>> {
        Ok(Vec::new())
    }
    async fn definition(&mut self, _: &ObjectRef) -> dbine_driver::Result<Option<String>> {
        Ok(None)
    }
    fn browse_query(&self, _: &ObjectRef, _: u32) -> String {
        String::new()
    }
    async fn execute(
        &mut self,
        _: &str,
        max_rows: usize,
        out: &mut QueryOutcome,
    ) -> dbine_driver::Result<()> {
        if self.hang {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        if self.remote {
            return self.execute_remote(max_rows, out).await;
        }
        if self.cut_as_error {
            // No await between the rows and the error: one poll.
            for (columns, rows) in &self.results {
                out.begin_result(columns.clone());
                for r in rows.iter().take(max_rows) {
                    out.push_row(r.clone(), max_rows);
                }
            }
            return Err(dbine_driver::Error::Query("el resultado se cortó".into()));
        }
        for (columns, rows) in &self.results {
            out.begin_result(columns.clone());
            for (i, r) in rows.iter().enumerate() {
                out.push_row(r.clone(), max_rows);
                if i % 100 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        }
        if self.truncated {
            if let Some(r) = out.results.last_mut() {
                r.truncated = true;
            }
        }
        Ok(())
    }
}

/// `EVALUATE SUMMARIZECOLUMNS('Date'[Year], 'Geo'[Region], "Ventas",
/// [Total Sales], …)` as Power BI's `executeQueries` answers it: columns as
/// `Table[Column]` and `[Measure]`, a measure's float noise, BLANK as null,
/// dates as DateTime at midnight. The last row repeats a key.
fn power_bi() -> Fixture {
    Fixture::one(
        vec![
            col("Date[Year]", "Int64"),
            col("Geo[Region]", "String"),
            col("[Ventas]", "Double"),
            col("[Clientes]", "Int64"),
            col("[Desde]", "DateTime"),
        ],
        rows(json!([
            [2023, "Norte", 1234.5600000000002, 10, "2023-01-01T00:00:00"],
            [2023, "Sur", 999.99, null, "2023-01-01T00:00:00"],
            [2024, "Norte", 2000.0, 12, "2024-01-01T00:00:00"],
            [2024, "Sur", 1500.25, 7, "2024-01-01T00:00:00"],
            [2024, "Este", 300.0, 3, "2024-01-01T00:00:00"],
            [2024, "Sur", 1.0, 1, "2024-01-01T00:00:00"],
        ])),
    )
}

/// The source's `SELECT year, region, SUM(amount) …` as SQL Server's driver
/// answers it: decimals as text with the column's scale, a count of no
/// rows as 0, dates as dates.
fn sql_server() -> Fixture {
    Fixture::one(
        vec![
            col("year", "int"),
            col("region", "nvarchar"),
            col("ventas", "decimal"),
            col("clientes", "int"),
            col("desde", "date"),
        ],
        rows(json!([
            [2023, "Norte", "1234.5600", 10, "2023-01-01"],
            [2023, "Sur", "999.9900", 0, "2023-01-01"],
            [2024, "Norte", "2000.0000", 12, "2024-01-01"],
            [2024, "Sur", "1500.3000", 7, "2024-01-01"],
            [2025, "Norte", "10.0000", 1, "2025-01-01"],
        ])),
    )
}

/// The same source on PostgreSQL-like types, run for real on a local DuckDB.
const POSTGRES: &str = "SELECT * FROM (VALUES
  (2023, 'Norte', CAST(1234.56 AS DECIMAL(12,2)), 10, DATE '2023-01-01'),
  (2023, 'Sur', CAST(999.99 AS DECIMAL(12,2)), 0, DATE '2023-01-01'),
  (2024, 'Norte', CAST(2000 AS DECIMAL(12,2)), 12, DATE '2024-01-01'),
  (2024, 'Sur', CAST(1500.30 AS DECIMAL(12,2)), 7, DATE '2024-01-01'),
  (2025, 'Norte', CAST(10 AS DECIMAL(12,2)), 1, DATE '2025-01-01')
) t(year, region, ventas, clientes, desde) ORDER BY year DESC";

fn pair(left: &str, right: &str, key: bool) -> ColumnPair {
    ColumnPair {
        left: left.into(),
        right: right.into(),
        key,
    }
}

/// The model's columns against the source's.
fn model_pairs() -> Vec<ColumnPair> {
    vec![
        pair("Date[Year]", "year", true),
        pair("Geo[Region]", "region", true),
        pair("[Ventas]", "ventas", false),
        pair("[Clientes]", "clientes", false),
        pair("[Desde]", "desde", false),
    ]
}

fn opts(in_memory_rows: usize, root: &Path) -> Options {
    Options {
        in_memory_rows,
        in_memory_bytes: usize::MAX,
        max_diff_rows: 1_000_000,
        scratch: Some(root.to_path_buf()),
        duckdb: None,
    }
}

fn quiet(_: &Progress) {}

#[allow(clippy::too_many_arguments)]
async fn compare(
    l: &mut dyn Session,
    lq: &str,
    r: &mut dyn Session,
    rq: &str,
    columns: &[ColumnPair],
    options: QueryOptions,
    opts: &Options,
    ctl: &Arc<Ctl>,
) -> CommandResult<Compared> {
    compare_held(
        (l, lq, true),
        (r, rq, true),
        columns,
        options,
        opts,
        ctl,
        &quiet,
    )
    .await
}

/// [`compare`] with each side's [`Source::hold`] and a progress callback.
async fn compare_held(
    (l, lq, lhold): (&mut dyn Session, &str, bool),
    (r, rq, rhold): (&mut dyn Session, &str, bool),
    columns: &[ColumnPair],
    options: QueryOptions,
    opts: &Options,
    ctl: &Arc<Ctl>,
    progress: &(dyn Fn(&Progress) + Send + Sync),
) -> CommandResult<Compared> {
    let pairs = pairs_of(columns)?;
    let input = Input {
        key: Vec::new(),
        wanted: Vec::new(),
        cols: [named(&pairs, 0), named(&pairs, 1)],
    };
    let how = How {
        pairs: Some(pairs),
        rules: Arc::new(QueryRules::new(options)?),
    };
    whole::compare_with(
        Source {
            session: l,
            read: Read::Query(lq),
            hold: lhold,
        },
        Source {
            session: r,
            read: Read::Query(rq),
            hold: rhold,
        },
        input,
        how,
        opts,
        ctl,
        progress,
    )
    .await
}

/// Everything a compare gives, comparable.
fn summary(c: &Compared) -> Value {
    json!({
        "columns": c.projection.columns.iter().map(|x| [x.0.clone(), x.1.clone()]).collect::<Vec<_>>(),
        "counts": serde_json::to_value(&c.counts).unwrap(),
        "only_left": c.only_left,
        "only_right": c.only_right,
        "changed": serde_json::to_value(&c.changed).unwrap(),
        "duplicate_keys": c.duplicate_keys,
        "diff_truncated": c.diff_truncated,
    })
}

fn rounding() -> QueryOptions {
    QueryOptions {
        absolute: 0.005,
        null_as_blank: true,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// The rules

#[test]
fn values_compare_by_type() {
    let strict = QueryRules::new(QueryOptions::default()).unwrap();
    // Numbers by value, however each engine writes them.
    assert!(strict.same(&json!(1), &json!("1.00")));
    assert!(strict.same(&json!(2000.0), &json!("2000.0000")));
    assert!(!strict.same(&json!(1234.5600000000002), &json!("1234.56")));
    // A code with leading zeros is text.
    assert!(!strict.same(&json!("007"), &json!(7)));
    // A date and a timestamp at its midnight are the same day; another
    // hour isn't.
    assert!(strict.same(&json!("2024-03-01"), &json!("2024-03-01T00:00:00")));
    assert!(strict.same(&json!("2024-03-01"), &json!("2024-03-01 00:00:00.000")));
    assert!(strict.same(&json!("2024-03-01"), &json!("2024-03-01T00:00:00Z")));
    assert!(!strict.same(&json!("2024-03-01"), &json!("2024-03-01T00:00:01")));
    // PostgreSQL's booleans as its driver streams them (`t`, `f`, before
    // it says the type) against another engine's.
    assert!(strict.same(&json!("t"), &json!(true)));
    assert!(strict.same(&json!("f"), &json!(false)));
    assert!(strict.same(&json!("t"), &json!(1)));
    assert!(strict.same(&json!("true"), &json!("t")));
    assert!(!strict.same(&json!("t"), &json!(false)));
    assert_eq!(strict.key(&json!("t")), strict.key(&json!(true)));
    // NULL is only NULL, unless NULL is BLANK.
    assert!(strict.same(&Value::Null, &Value::Null));
    assert!(!strict.same(&Value::Null, &json!(0)));
    assert!(!strict.same(&Value::Null, &json!("")));
    let blank = QueryRules::new(QueryOptions {
        null_as_blank: true,
        ..Default::default()
    })
    .unwrap();
    for b in [json!(0), json!("0.00"), json!(""), json!(false), json!("f")] {
        assert!(blank.same(&Value::Null, &b), "{b}");
        assert!(blank.same(&b, &Value::Null), "{b}");
    }
    assert!(!blank.same(&Value::Null, &json!(5)));
    // Blank only stands in for NULL: "" isn't 0.
    assert!(!blank.same(&json!(""), &json!(0)));
    // Text exactly, unless told otherwise.
    assert!(!strict.same(&json!("Norte"), &json!("norte ")));
    let loose_text = QueryRules::new(QueryOptions {
        ignore_case: true,
        trim: true,
        ..Default::default()
    })
    .unwrap();
    assert!(loose_text.same(&json!("Norte"), &json!(" norte ")));
    assert_eq!(
        loose_text.key(&json!(" NORTE")),
        loose_text.key(&json!("norte"))
    );
    assert_ne!(strict.key(&json!("NORTE")), strict.key(&json!("norte")));
    // Keys: a DAX year against a SQL one, a date against a midnight.
    assert_eq!(strict.key(&json!(2024)), strict.key(&json!("2024")));
    assert_eq!(
        strict.key(&json!("2024-01-01T00:00:00")),
        strict.key(&json!("2024-01-01"))
    );
    assert_ne!(strict.key(&Value::Null), strict.key(&json!("")));
}

#[test]
fn numbers_compare_within_the_tolerance() {
    let abs = QueryRules::new(QueryOptions {
        absolute: 0.01,
        ..Default::default()
    })
    .unwrap();
    assert!(abs.same(&json!(1234.5600000000002), &json!("1234.56")));
    assert!(abs.same(&json!(100.004), &json!(100)));
    assert!(abs.same(&json!("-5.005"), &json!(-5)));
    assert!(!abs.same(&json!(100.02), &json!("100")));
    // Only numbers: text and dates don't get a tolerance.
    assert!(!abs.same(&json!("2024-03-01"), &json!("2024-03-02")));
    let rel = QueryRules::new(QueryOptions {
        relative: 0.001,
        ..Default::default()
    })
    .unwrap();
    assert!(rel.same(&json!(1000), &json!(1000.9)));
    assert!(!rel.same(&json!(1000), &json!(1001.5)));
    assert!(rel.same(&json!(1_000_000), &json!(1_000_900)));
    assert!(!rel.same(&json!(0.1), &json!(0.2)));
    // Either one is enough.
    let both = QueryRules::new(QueryOptions {
        absolute: 0.5,
        relative: 0.001,
        ..Default::default()
    })
    .unwrap();
    assert!(both.same(&json!(0.1), &json!(0.5)));
    assert!(both.same(&json!(1_000_000), &json!(1_000_900)));
    // A negative or endless tolerance is refused.
    for bad in [-1.0, f64::NAN, f64::INFINITY] {
        assert!(QueryRules::new(QueryOptions {
            absolute: bad,
            ..Default::default()
        })
        .is_err());
        assert!(QueryRules::new(QueryOptions {
            relative: bad,
            ..Default::default()
        })
        .is_err());
    }
}

#[test]
fn the_key_comes_first_and_is_required() {
    let p = pairs_of(&[
        pair("[Ventas]", "ventas", false),
        pair("Date[Year]", "year", true),
        pair("Geo[Region]", "region", true),
    ])
    .unwrap();
    assert_eq!(p.nk, 2);
    assert_eq!(p.columns[0], ("Date[Year]".into(), "year".into()));
    assert_eq!(p.columns[2], ("[Ventas]".into(), "ventas".into()));
    let e = pairs_of(&[pair("a", "b", false)]).err().unwrap();
    assert!(e.to_string().contains("clave"), "{e}");
}

// ---------------------------------------------------------------------------
// Whole compares

#[tokio::test(flavor = "multi_thread")]
async fn power_bi_against_sql_server_and_postgres() {
    let root = temp_dir("pbi");
    let pairs = model_pairs();
    let mut got = Vec::new();
    for source in ["sqlserver", "postgres"] {
        for options in [QueryOptions::default(), rounding()] {
            let ctl = Arc::new(Ctl::default());
            let mut model = power_bi();
            let mut sql: Box<dyn Session> = if source == "sqlserver" {
                Box::new(sql_server())
            } else {
                local_duckdb::open_session().await.expect("DuckDB opens")
            };
            let c = compare(
                &mut model,
                "EVALUATE SUMMARIZECOLUMNS(…)",
                sql.as_mut(),
                POSTGRES,
                &pairs,
                options,
                &opts(1_000_000, &root.0),
                &ctl,
            )
            .await
            .unwrap();
            let k = &c.counts;
            // Each side's rows, whole; the model's repeated key reported.
            assert_eq!((k.left, k.right), (6, 5), "{source}");
            assert_eq!(c.duplicate_keys, 1, "{source}");
            assert_eq!((k.only_left, k.only_right), (1, 1), "{source}");
            assert_eq!(c.only_left[0][1], json!("Este"));
            assert_eq!(c.only_right[0][0], json!(2025));
            let ventas = 2;
            let clientes = 3;
            if options == rounding() {
                // The measure's float noise is within the tolerance and BLANK
                // is 0: only 2024 Sur differs, in its sales.
                assert_eq!((k.same, k.changed), (3, 1), "{source}");
                assert_eq!(c.changed[0].left[1], json!("Sur"));
                assert_eq!(c.changed[0].diff, [ventas]);
            } else {
                assert_eq!((k.same, k.changed), (1, 3), "{source}");
                let diffs: Vec<Vec<usize>> = c.changed.iter().map(|r| r.diff.clone()).collect();
                // In key order: 2023 Norte (noise), 2023 Sur (BLANK against
                // 0), 2024 Sur. The dates at midnight never differ.
                assert_eq!(diffs, [vec![ventas], vec![clientes], vec![ventas]]);
            }
            // The first row of a repeated key is the one compared.
            assert!(!c.changed.iter().any(|r| r.left[ventas] == json!(1.0)));
            // What each query returned, for the result's types.
            assert_eq!(c.query_columns[0][2].type_name, "Double");
            assert_eq!(c.query_columns[1][0].name, "year");
            got.push(json!({
                "counts": serde_json::to_value(&c.counts).unwrap(),
                "diff": c.changed.iter().map(|r| r.diff.clone()).collect::<Vec<_>>(),
            }));
        }
    }
    // SQL Server's and PostgreSQL's answers compare alike.
    assert_eq!(got[0], got[2]);
    assert_eq!(got[1], got[3]);
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

/// Value pairs at the edges of the rules, one row each, compared on both
/// paths under several options: the scratch DuckDB's SQL says what
/// [`QueryRules::same`] says.
#[tokio::test(flavor = "multi_thread")]
async fn memory_and_duckdb_agree_on_every_rule() {
    let root = temp_dir("rules");
    let pairs_of_values = json!([
        [1, "1.00"],
        [1.0, 1],
        [1234.5600000000002, "1234.56"],
        [100.004, 100],
        [100.02, "100"],
        [-5.004, "-5"],
        [null, null],
        [null, 0],
        [null, "0.00"],
        [null, ""],
        [null, false],
        [null, 5],
        ["", 0],
        ["abc", "ABC"],
        [" abc ", "abc"],
        ["2024-03-01", "2024-03-01T00:00:00"],
        ["2024-03-01", "2024-03-01T00:00:01"],
        ["007", 7],
        [true, 1],
        [1000, 1000.9],
        [1000, 1001.5],
        [1000000, 1000900],
        [0.1, 0.2],
        ["x", null],
        ["t", true],
        ["f", 0],
        [null, "f"],
        ["t", "f"],
        ["9007199254740993", 9007199254740992i64],
    ]);
    let values = rows(pairs_of_values);
    let left: Vec<Vec<Value>> = values
        .iter()
        .enumerate()
        .map(|(i, v)| vec![json!(i), v[0].clone()])
        .collect();
    let right: Vec<Vec<Value>> = values
        .iter()
        .enumerate()
        .map(|(i, v)| vec![json!(i), v[1].clone()])
        .collect();
    let columns = vec![pair("id", "id", true), pair("v", "v", false)];
    let options = [
        QueryOptions::default(),
        QueryOptions {
            absolute: 0.01,
            ..Default::default()
        },
        QueryOptions {
            relative: 0.001,
            null_as_blank: true,
            ignore_case: true,
            trim: true,
            ..Default::default()
        },
        QueryOptions {
            absolute: 0.005,
            relative: 0.0005,
            null_as_blank: true,
            ..Default::default()
        },
    ];
    for o in options {
        let rules = QueryRules::new(o).unwrap();
        let mut got = Vec::new();
        for in_memory_rows in [1_000_000, 1] {
            let ctl = Arc::new(Ctl::default());
            let cols = vec![col("id", "INTEGER"), col("v", "VARCHAR")];
            let mut l = Fixture::one(cols.clone(), left.clone());
            let mut r = Fixture::one(cols, right.clone());
            let c = compare(
                &mut l,
                "q",
                &mut r,
                "q",
                &columns,
                o,
                &opts(in_memory_rows, &root.0),
                &ctl,
            )
            .await
            .unwrap();
            assert_eq!(c.spilled, in_memory_rows == 1, "{o:?}");
            let changed: Vec<i64> = c
                .changed
                .iter()
                .map(|r| r.left[0].as_i64().unwrap())
                .collect();
            let expected: Vec<i64> = values
                .iter()
                .enumerate()
                .filter(|(_, v)| !rules.same(&v[0], &v[1]))
                .map(|(i, _)| i as i64)
                .collect();
            assert_eq!(changed, expected, "{o:?}, in memory: {}", !c.spilled);
            assert_eq!(c.counts.same as usize, values.len() - expected.len());
            got.push(summary(&c));
        }
        assert_eq!(got[0], got[1], "{o:?}");
    }
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

/// Two results far past what stays in memory, from real queries on local
/// DuckDB sessions: the scratch DuckDB gives what memory gives, with the
/// tolerance, the dates as timestamps, a gap, extra rows and a repeated key.
#[tokio::test(flavor = "multi_thread")]
async fn large_results_go_to_duckdb_and_agree_with_memory() {
    const N: i64 = 200_000;
    let root = temp_dir("large");
    let left = format!(
        "SELECT i AS id, 'k' || (i % 7) AS grp, CAST(i AS DOUBLE) / 10 AS amount, \
         DATE '2024-01-01' + CAST(i % 365 AS INTEGER) AS day \
         FROM range(0, {N}) r(i)"
    );
    let right = format!(
        "SELECT i AS \"ID\", 'K' || (i % 7) AS grp, \
         CAST(CAST(i AS DOUBLE) / 10 + CASE WHEN i % 1000 = 0 THEN 0.5 ELSE 0.0004 END AS VARCHAR) AS amount, \
         TIMESTAMP '2024-01-01 00:00:00' + to_days(CAST(i % 365 AS INTEGER)) AS day \
         FROM range(0, {M}) r(i) WHERE i NOT BETWEEN 50000 AND 50099 \
         UNION ALL SELECT 5, 'K5', '0.5', TIMESTAMP '2024-01-06 00:00:00'",
        M = N + 50
    );
    let columns = vec![
        pair("id", "ID", true),
        pair("grp", "grp", false),
        pair("amount", "amount", false),
        pair("day", "day", false),
    ];
    let options = QueryOptions {
        absolute: 0.001,
        ignore_case: true,
        ..Default::default()
    };
    let mut got = Vec::new();
    for in_memory_rows in [1_000_000, 10_000] {
        let ctl = Arc::new(Ctl::default());
        let mut l = local_duckdb::open_session().await.expect("DuckDB opens");
        let mut r = local_duckdb::open_session().await.expect("DuckDB opens");
        let c = compare(
            l.as_mut(),
            &left,
            r.as_mut(),
            &right,
            &columns,
            options,
            &opts(in_memory_rows, &root.0),
            &ctl,
        )
        .await
        .unwrap();
        assert_eq!(c.spilled, in_memory_rows == 10_000);
        let k = &c.counts;
        assert_eq!((k.left, k.right), (N as u64, N as u64 - 100 + 50 + 1));
        assert_eq!(c.duplicate_keys, 1);
        assert_eq!((k.only_left, k.only_right), (100, 50));
        // Every 1,000th amount is 0.5 off; the rest are within 0.001.
        let off = (0..N)
            .filter(|i| i % 1000 == 0 && !(50000..=50099).contains(i))
            .count() as u64;
        assert_eq!(k.changed, off);
        assert_eq!(k.same + k.changed + k.only_left, k.left);
        assert!(c.changed.iter().all(|r| r.diff == [2]));
        got.push(summary(&c));
    }
    assert_eq!(got[0], got[1]);
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cut_or_ambiguous_result_is_refused() {
    let root = temp_dir("refused");
    let cols = || vec![col("id", "int"), col("v", "int")];
    let data = || rows(json!([[1, 1], [2, 2]]));
    let columns = vec![pair("id", "id", true), pair("v", "v", false)];
    let run = |l: Fixture, r: Fixture| {
        let root = root.0.clone();
        let columns = columns.clone();
        async move {
            let (mut l, mut r) = (l, r);
            let ctl = Arc::new(Ctl::default());
            compare(
                &mut l,
                "q",
                &mut r,
                "q",
                &columns,
                QueryOptions::default(),
                &opts(1_000_000, &root),
                &ctl,
            )
            .await
            .err()
            .expect("refused")
            .to_string()
        }
    };
    // An API's row cap: a compare is whole or it isn't.
    let mut cut = Fixture::one(cols(), data());
    cut.truncated = true;
    let e = run(Fixture::one(cols(), data()), cut).await;
    assert!(e.contains("cortó") && e.contains("derecha"), "{e}");
    // Two results: which one would be compared?
    let two = Fixture {
        results: vec![(cols(), data()), (cols(), data())],
        ..Default::default()
    };
    let e = run(two, Fixture::one(cols(), data())).await;
    assert!(
        e.contains("más de un resultado") && e.contains("izquierda"),
        "{e}"
    );
    // Nothing with columns.
    let e = run(Fixture::default(), Fixture::one(cols(), data())).await;
    assert!(e.contains("no devolvió un resultado"), "{e}");
    // A paired column the result doesn't have.
    let e = run(
        Fixture::one(vec![col("id", "int"), col("w", "int")], data()),
        Fixture::one(cols(), data()),
    )
    .await;
    assert!(e.contains("«v»"), "{e}");
    // Two columns of one name (`SELECT *` over a join): which is compared?
    let e = run(
        Fixture::one(
            vec![col("id", "int"), col("v", "int"), col("ID", "int")],
            rows(json!([[1, 1, 1], [2, 2, 2]])),
        ),
        Fixture::one(cols(), data()),
    )
    .await;
    assert!(
        e.contains("dos columnas «ID»") && e.contains("izquierda") && e.contains("alias"),
        "{e}"
    );
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_a_query_that_never_ends() {
    let root = temp_dir("cancel");
    let ctl = Arc::new(Ctl::default());
    let c2 = ctl.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        c2.cancel();
    });
    let mut l = Fixture {
        hang: true,
        ..Default::default()
    };
    let mut r = Fixture::one(vec![col("id", "int")], rows(json!([[1]])));
    let started = std::time::Instant::now();
    let e = compare(
        &mut l,
        "q",
        &mut r,
        "q",
        &[pair("id", "id", true)],
        QueryOptions::default(),
        &opts(1_000_000, &root.0),
        &ctl,
    )
    .await
    .err()
    .expect("cancelled");
    assert!(matches!(e, CommandError::Cancelled), "{e:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

/// A driver host hands its rows over on its reader thread, which must
/// never wait for the scratch DuckDB when it also brings that DuckDB's
/// replies (both on the DuckDB host: `hold` false): the rows queue for the
/// task that reads, and the result is the in-process one, in memory or on
/// disk.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_reader_thread_never_waits() {
    let root = temp_dir("remote");
    let cols = vec![col("id", "BIGINT"), col("v", "VARCHAR")];
    let data = |shift: i64| -> Vec<Vec<Value>> {
        (0..20_000i64)
            .map(|i| vec![json!(i + shift), json!(format!("v{}", (i + shift) % 997))])
            .collect()
    };
    let columns = vec![pair("id", "id", true), pair("v", "v", false)];
    let mut got = Vec::new();
    for (remote, in_memory_rows) in [(false, 1_000_000), (true, 1_000_000), (true, 500)] {
        let ctl = Arc::new(Ctl::default());
        let mut l = Fixture::one(cols.clone(), data(0));
        let mut r = Fixture::one(cols.clone(), data(100));
        l.remote = remote;
        r.remote = remote;
        let started = std::time::Instant::now();
        let c = compare_held(
            (&mut l, "q", false),
            (&mut r, "q", false),
            &columns,
            QueryOptions::default(),
            &opts(in_memory_rows, &root.0),
            &ctl,
            &quiet,
        )
        .await
        .unwrap();
        assert_eq!(c.spilled, in_memory_rows == 500);
        assert_eq!(
            (c.counts.left, c.counts.right, c.counts.same),
            (20_000, 20_000, 19_900)
        );
        assert_eq!((c.counts.only_left, c.counts.only_right), (100, 100));
        assert!(started.elapsed() < Duration::from_secs(120));
        got.push(summary(&c));
    }
    assert_eq!(got[0], got[1]);
    assert_eq!(got[1], got[2]);
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_probe_gives_the_columns_and_the_first_row() {
    let ctl = Ctl::default();
    let mut s = power_bi();
    let stopped = std::sync::atomic::AtomicBool::new(false);
    let got = probe(&mut s, "q", &ctl, || {
        stopped.store(true, std::sync::atomic::Ordering::SeqCst)
    })
    .await
    .unwrap();
    assert_eq!(got.columns.len(), 5);
    assert_eq!(got.columns[0].name, "Date[Year]");
    assert_eq!(
        got.sample,
        Some(
            rows(json!([[
                2023,
                "Norte",
                1234.5600000000002,
                10,
                "2023-01-01T00:00:00"
            ]]))[0]
                .clone()
        )
    );
    // An empty result still has its columns.
    let mut empty = Fixture::one(vec![col("a", "int")], Vec::new());
    let got = probe(&mut empty, "q", &ctl, || {}).await.unwrap();
    assert_eq!((got.columns.len(), got.sample), (1, None));
    // Nothing with columns, or two results.
    assert!(probe(&mut Fixture::default(), "q", &ctl, || {})
        .await
        .is_err());
    let mut two = Fixture {
        results: vec![
            (vec![col("a", "int")], Vec::new()),
            (vec![col("b", "int")], Vec::new()),
        ],
        ..Default::default()
    };
    let e = probe(&mut two, "q", &ctl, || {}).await.err().unwrap();
    assert!(e.to_string().contains("más de un resultado"), "{e}");
    // Two columns of one name, or of no name.
    let mut twice = Fixture::one(vec![col("id", "int"), col("Id", "int")], Vec::new());
    let e = probe(&mut twice, "q", &ctl, || {}).await.err().unwrap();
    assert!(e.to_string().contains("dos columnas «Id»"), "{e}");
    let mut unnamed = Fixture::one(vec![col("", "int"), col("", "int")], Vec::new());
    let e = probe(&mut unnamed, "q", &ctl, || {}).await.err().unwrap();
    assert!(e.to_string().contains("sin nombre"), "{e}");
}

/// An engine that cuts a result at the rows asked for (Power BI's
/// `resultSetRowCountLimit`) may answer the cut as an error together with
/// the first row: in the same poll in process, right after it from a host.
/// The probe has what it came for, whichever the run's `select!` sees
/// first; an error before any row still fails it.
#[tokio::test(flavor = "multi_thread")]
async fn the_probe_keeps_its_row_when_the_run_then_fails() {
    let ctl = Ctl::default();
    for remote in [false, true] {
        for _ in 0..40 {
            let mut s = power_bi();
            s.cut_as_error = true;
            s.remote = remote;
            let got = probe(&mut s, "q", &ctl, || {}).await.unwrap();
            assert_eq!(got.columns.len(), 5, "remote: {remote}");
            assert_eq!(
                got.sample.as_ref().map(|r| r[1].clone()),
                Some(json!("Norte"))
            );
        }
        let mut nothing = Fixture::one(vec![col("a", "int")], Vec::new());
        nothing.cut_as_error = true;
        nothing.remote = remote;
        let e = probe(&mut nothing, "q", &ctl, || {}).await.err().unwrap();
        assert!(e.to_string().contains("se cortó"), "{e}");
    }
}

/// The result as the UI gets it, and the difference kept like a table
/// compare's: sorted and filtered over all its rows by `data_compare_rows`.
#[tokio::test(flavor = "multi_thread")]
async fn the_result_sorts_and_filters_like_a_table_compare() {
    let root = temp_dir("keep");
    let ctl = Arc::new(Ctl::default());
    let mut model = power_bi();
    let mut source = sql_server();
    // [Desde] left out: listed as not compared, on each side.
    let pairs: Vec<ColumnPair> = model_pairs().into_iter().take(4).collect();
    let c = compare(
        &mut model,
        "q",
        &mut source,
        "q",
        &pairs,
        QueryOptions::default(),
        &opts(1_000_000, &root.0),
        &ctl,
    )
    .await
    .unwrap();
    let side = |c: &str| QuerySide {
        connection_id: c.into(),
        database: String::new(),
        query: "q".into(),
    };
    let out = keep(side("pbi"), side("mssql"), c, 1_000_000);
    let r = &out.result;
    assert_eq!(r.key, ["Date[Year]", "Geo[Region]"]);
    assert_eq!(out.right_columns, ["year", "region", "ventas", "clientes"]);
    assert_eq!(r.types, ["Int64", "String", "Double", "Int64"]);
    assert_eq!(out.right_types[2], "decimal");
    assert_eq!(r.only_left_columns, ["[Desde]"]);
    assert_eq!(r.only_right_columns, ["desde"]);
    assert_eq!(r.listed.changed, 3);
    let rows = data_compare::data_compare_rows(data_compare::DataRowsArgs {
        id: r.id.clone(),
        kind: data_compare::RowKind::Changed,
        sort: vec![dbine_driver::SortKey {
            column: "[Ventas]".into(),
            descending: true,
        }],
        filters: Vec::new(),
        changed_in: vec!["[Ventas]".into()],
    })
    .await
    .unwrap();
    // Only the rows whose sales differ, the largest first.
    assert_eq!(rows.total, 2);
    let sales: Vec<Value> = rows.rows.iter().map(|x| x.left[2].clone()).collect();
    assert_eq!(sales, [json!(1500.25), json!(1234.5600000000002)]);
    assert!(data_compare::diffs()
        .lock()
        .unwrap()
        .get(&r.id)
        .is_some_and(|d| data_compare::is_query(&d.left) && data_compare::is_query(&d.right)));
}

#[test]
fn a_query_compare_keeps_no_table_to_write() {
    let side = kept_side(&QuerySide {
        connection_id: "c".into(),
        database: "d".into(),
        query: "SELECT 1".into(),
    });
    assert!(data_compare::is_query(&side));
    assert_eq!(side.object.name, "");
}

#[test]
fn only_the_duckdb_hosts_reader_isnt_held() {
    assert!(holds_its_reader(Some("postgres")));
    assert!(holds_its_reader(Some("powerbi")));
    assert!(!holds_its_reader(Some("duckdb")));
    assert!(!holds_its_reader(Some("duckdb_files")));
    assert!(!holds_its_reader(None));
}

// ---------------------------------------------------------------------------
// Integers past 2^53

#[test]
fn integers_past_2_pow_53_stay_distinct() {
    let strict = QueryRules::new(QueryOptions::default()).unwrap();
    let (a, b) = (json!("9007199254740993"), json!("9007199254740992"));
    assert_ne!(strict.key(&a), strict.key(&b));
    assert!(!strict.same(&a, &b));
    assert!(!strict.same(&json!(9007199254740993i64), &json!(9007199254740992i64)));
    assert!(strict.same(&json!(9007199254740993i64), &a));
    assert!(strict.same(&json!("9007199254740993.000"), &a));
    // A decimal with more digits than a double holds.
    assert!(!strict.same(
        &json!("0.12345678901234567890"),
        &json!("0.1234567890123456789001")
    ));
}

/// BigQuery's or Snowflake's INT64 ids as `json_i64` sends them (text past
/// 2^53) against an engine that sends JSON numbers: each key pairs with its
/// own, and values one apart differ, in memory and in the scratch DuckDB.
#[tokio::test(flavor = "multi_thread")]
async fn int64_ids_past_2_pow_53_pair_and_compare_exactly() {
    let root = temp_dir("int64");
    let left = rows(json!([
        ["9007199254740993", "9007199254740993"],
        ["9007199254740992", "1"],
        ["18446744073709551615", "7"],
    ]));
    let right = rows(json!([
        [9007199254740992i64, 1],
        [9007199254740993i64, 9007199254740992i64],
        [18446744073709551615u64, "7.000"],
    ]));
    let columns = vec![pair("id", "id", true), pair("v", "v", false)];
    let mut got = Vec::new();
    for in_memory_rows in [1_000_000, 1] {
        let ctl = Arc::new(Ctl::default());
        let mut l = Fixture::one(vec![col("id", "INT64"), col("v", "INT64")], left.clone());
        let mut r = Fixture::one(vec![col("id", "bigint"), col("v", "bigint")], right.clone());
        let c = compare(
            &mut l,
            "q",
            &mut r,
            "q",
            &columns,
            QueryOptions::default(),
            &opts(in_memory_rows, &root.0),
            &ctl,
        )
        .await
        .unwrap();
        assert_eq!(c.spilled, in_memory_rows == 1);
        assert_eq!(c.duplicate_keys, 0, "spilled: {}", c.spilled);
        assert_eq!((c.counts.only_left, c.counts.only_right), (0, 0));
        assert_eq!((c.counts.same, c.counts.changed), (2, 1));
        assert_eq!(c.changed[0].left[0], json!("9007199254740993"));
        got.push(summary(&c));
    }
    assert_eq!(got[0], got[1]);
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

// ---------------------------------------------------------------------------
// Rows one side leaves out as BLANK

/// DAX's SUMMARIZECOLUMNS leaves out the rows whose measures are all BLANK;
/// SQL over a date dimension gives them with NULL or 0. Under NULL as
/// BLANK such a row, on either side, is the same as the one left out; a
/// row with any value isn't, nor one compared by its key alone. Memory and
/// the scratch DuckDB agree.
#[tokio::test(flavor = "multi_thread")]
async fn rows_left_out_as_blank_are_the_same_under_null_as_blank() {
    let root = temp_dir("blank");
    let dax = rows(json!([
        ["2024-01", 10.0, 1],
        ["2024-02", 20.0, 2],
        ["2024-05", null, 3],
        ["2024-07", 0, null],
    ]));
    let sql = rows(json!([
        ["2024-01", "10.00", 1],
        ["2024-02", "20.00", 2],
        ["2024-03", null, 0],
        ["2024-04", "0.00", null],
        ["2024-05", null, 3],
        ["2024-06", "5.00", null],
    ]));
    let dcols = vec![
        col("Date[Month]", "String"),
        col("[Ventas]", "Double"),
        col("[Pedidos]", "Int64"),
    ];
    let scols = vec![
        col("month", "varchar"),
        col("ventas", "decimal"),
        col("pedidos", "int"),
    ];
    let all = vec![
        pair("Date[Month]", "month", true),
        pair("[Ventas]", "ventas", false),
        pair("[Pedidos]", "pedidos", false),
    ];
    let key_only = vec![pair("Date[Month]", "month", true)];
    let blank = QueryOptions {
        null_as_blank: true,
        ..Default::default()
    };
    // (same, changed, only on the left, only on the right)
    let cases = [
        // 2024-07 on the left, 2024-03 and 2024-04 on the right: all blank.
        (&all, blank, (6, 0, 0, 1)),
        (&all, QueryOptions::default(), (3, 0, 1, 3)),
        (&key_only, blank, (3, 0, 1, 3)),
    ];
    for (columns, options, want) in cases {
        let mut got = Vec::new();
        for in_memory_rows in [1_000_000, 1] {
            let ctl = Arc::new(Ctl::default());
            let mut l = Fixture::one(dcols.clone(), dax.clone());
            let mut r = Fixture::one(scols.clone(), sql.clone());
            let c = compare(
                &mut l,
                "q",
                &mut r,
                "q",
                columns,
                options,
                &opts(in_memory_rows, &root.0),
                &ctl,
            )
            .await
            .unwrap();
            let k = &c.counts;
            let label = format!(
                "{options:?}, {} columns, spilled: {}",
                columns.len(),
                c.spilled
            );
            assert_eq!((k.left, k.right), (4, 6), "{label}");
            assert_eq!(
                (k.same, k.changed, k.only_left, k.only_right),
                want,
                "{label}"
            );
            assert_eq!(c.only_left.len() as u64, k.only_left, "{label}");
            assert_eq!(c.only_right.len() as u64, k.only_right, "{label}");
            got.push(summary(&c));
        }
        assert_eq!(got[0], got[1], "{options:?}");
    }
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

// ---------------------------------------------------------------------------
// A host that runs ahead

/// A DuckDB whose sessions open only once `open` is set: the scratch
/// database opening (on its first use, downloading) for as long as a test
/// wants.
struct Gated {
    inner: Arc<dyn Driver>,
    open: Arc<AtomicBool>,
}

#[dbine_driver::async_trait]
impl Driver for Gated {
    fn info(&self) -> &DriverInfo {
        self.inner.info()
    }

    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        database: Option<&str>,
    ) -> dbine_driver::Result<Box<dyn Session>> {
        while !self.open.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        self.inner.connect(cfg, database).await
    }
}

/// The estimated bytes of a row of [`ahead`] (`value_size`): a number and
/// a text of 9 characters.
const AHEAD_ROW: u64 = 16 + 16 + 9;
/// The rows the relay hands over at once.
const AHEAD_BATCH: u64 = 1_000;

/// `n` rows with ids from `from`, each with a value made of its id.
fn ahead(from: u64, n: u64) -> Vec<Vec<Value>> {
    (from..from + n)
        .map(|i| vec![json!(i), json!(format!("v{:08}", i % 1000))])
        .collect()
}

/// Two sides streamed by "hosts" whose scratch DuckDB can't open until
/// `open`; the compare runs on a task of its own, giving its counts.
struct AheadRun {
    open: Arc<AtomicBool>,
    emitted: [Arc<AtomicU64>; 2],
    /// Estimated bytes a side may hold in memory, and queue.
    in_memory_bytes: u64,
    cap: u64,
    task: tokio::task::JoinHandle<CommandResult<(bool, data_compare::Counts)>>,
}

fn run_ahead(root: &Path, n: u64, hold: [bool; 2]) -> AheadRun {
    let open = Arc::new(AtomicBool::new(false));
    let duckdb = dbine_drivers::find("duckdb")
        .expect("the DuckDB driver")
        .clone();
    let mut o = opts(1_000_000, root);
    // About 1,000 rows a side in memory, and twice that queued.
    o.in_memory_bytes = 40_000;
    o.duckdb = Some(Arc::new(Gated {
        inner: duckdb,
        open: open.clone(),
    }));
    let emitted = [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))];
    let cols = vec![col("id", "BIGINT"), col("v", "VARCHAR")];
    let side = |from: u64, counter: &Arc<AtomicU64>| Fixture {
        remote: true,
        emitted: Some(counter.clone()),
        ..Fixture::one(cols.clone(), ahead(from, n))
    };
    let (mut l, mut r) = (side(0, &emitted[0]), side(100, &emitted[1]));
    let in_memory_bytes = o.in_memory_bytes as u64;
    let task = tokio::spawn(async move {
        let ctl = Arc::new(Ctl::default());
        let columns = vec![pair("id", "id", true), pair("v", "v", false)];
        compare_held(
            (&mut l, "q", hold[0]),
            (&mut r, "q", hold[1]),
            &columns,
            QueryOptions::default(),
            &o,
            &ctl,
            &quiet,
        )
        .await
        .map(|c| (c.spilled, c.counts))
    });
    AheadRun {
        open,
        emitted,
        in_memory_bytes,
        cap: 2 * in_memory_bytes,
        task,
    }
}

impl AheadRun {
    /// The rows each side read, once neither reads any more.
    async fn settled(&self) -> [u64; 2] {
        let mut last = [u64::MAX; 2];
        let mut still = 0;
        loop {
            tokio::time::sleep(Duration::from_millis(200)).await;
            let now = [
                self.emitted[0].load(Ordering::SeqCst),
                self.emitted[1].load(Ordering::SeqCst),
            ];
            still = if now == last { still + 1 } else { 0 };
            last = now;
            if still == 4 {
                return now;
            }
        }
    }

    /// The most a side reads while the compare can't take its rows: what
    /// stays in memory (the limit, and the batch that passed it) and what
    /// queues (the cap, and the batch that passed it).
    fn bound(&self) -> u64 {
        (self.in_memory_bytes + self.cap) / AHEAD_ROW + 2 * AHEAD_BATCH
    }
}

/// A driver host streams a query's rows on its reader thread, and its
/// protocol has no window for them: while the compare can't take them (its
/// scratch DuckDB still opening), what queues stays within the relay's cap
/// and the reader thread is held, rather than the whole result piling up
/// in memory. Once the scratch database opens, the compare ends as it would
/// have.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_that_runs_ahead_is_held_within_the_cap() {
    const N: u64 = 200_000;
    let root = temp_dir("held");
    let run = run_ahead(&root.0, N, [true, true]);
    let read = run.settled().await;
    eprintln!("read while held: {read:?} (at most {} a side)", run.bound());
    assert!(!run.task.is_finished(), "the compare can't end yet");
    for (side, n) in read.into_iter().enumerate() {
        assert!(
            n > 0 && n <= run.bound(),
            "side {side}: {n} rows read while the compare couldn't take them (at most {})",
            run.bound()
        );
    }
    run.open.store(true, Ordering::SeqCst);
    let (spilled, k) = tokio::time::timeout(Duration::from_secs(120), run.task)
        .await
        .expect("the compare ends")
        .expect("its task")
        .unwrap();
    assert!(spilled);
    assert_eq!((k.left, k.right, k.same), (N, N, N - 100));
    assert_eq!((k.only_left, k.only_right), (100, 100));
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

/// The same on the DuckDB host, whose reader thread can't be held (it also
/// brings the scratch database's replies): past the cap, the compare stops
/// with a message rather than fill the memory.
#[tokio::test(flavor = "multi_thread")]
async fn a_duckdb_host_too_far_ahead_stops_the_compare() {
    let root = temp_dir("ahead");
    let run = run_ahead(&root.0, 200_000, [false, true]);
    let (bound, emitted) = (run.bound(), run.emitted[0].clone());
    let e = tokio::time::timeout(Duration::from_secs(60), run.task)
        .await
        .expect("the compare stops")
        .expect("its task")
        .expect_err("refused")
        .to_string();
    assert!(e.contains("más rápido") && e.contains("izquierda"), "{e}");
    let read = emitted.load(Ordering::SeqCst);
    assert!(read <= bound, "{read} rows read (at most {bound})");
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}
