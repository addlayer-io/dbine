//! The whole-table compare on local DuckDB tables: the in-memory path and
//! the scratch-DuckDB path (forced with a low threshold) give the same
//! result on the same data.

use super::*;
use serde_json::json;

/// A test's own folder, deleted when the test ends (passed or not).
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(name: &str) -> TempDir {
    let d = std::env::temp_dir().join(format!("dbine-dcmp-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    TempDir(d)
}

/// Folders a compare left in `root` (none once it ends).
fn leftovers(root: &Path) -> Vec<String> {
    std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().display().to_string())
        .collect()
}

fn table(name: &str) -> ObjectRef {
    ObjectRef {
        kind: "table".into(),
        schema: Some("main".into()),
        name: name.into(),
    }
}

/// A DuckDB of its own, in memory, with `sql` run on it.
async fn duck(sql: &str) -> Box<dyn Session> {
    let mut s = local_duckdb::open_session().await.expect("DuckDB opens");
    local_duckdb::exec(s.as_mut(), sql).await.expect("setup");
    s
}

const LEFT: &str = "CREATE TABLE t (id INTEGER, k2 VARCHAR, amount DECIMAL(12,2), ts TIMESTAMP, flag BOOLEAN, note VARCHAR, only_l INTEGER);
INSERT INTO t SELECT i, CASE WHEN i % 7 = 0 THEN NULL ELSE 'k' || (i % 3) END, i * 1.5,
  TIMESTAMP '2026-01-01 00:00:00' + to_seconds(i), i % 2 = 0,
  CASE WHEN i % 97 = 0 THEN NULL ELSE 'n' || i END, 1
  FROM range(1, 3001) r(i);
INSERT INTO t VALUES (7, NULL, 10.5, TIMESTAMP '2026-01-01 00:00:07', false, 'repeated', 1);";

/// The same rows in other types and spellings: numbers as text (`10.00`),
/// dates with `T` and `Z`, booleans as 1/0, upper-case column names. Then
/// a gap (1001–1100 missing), 50 rows only here, notes changed every 50th
/// row, '' where the left has NULL, and key 5 twice.
const RIGHT: &str = "CREATE TABLE \"T\" (\"ID\" VARCHAR, \"K2\" VARCHAR, \"Amount\" VARCHAR, \"TS\" VARCHAR, \"Flag\" INTEGER, \"Note\" VARCHAR, only_r INTEGER);
INSERT INTO \"T\" SELECT CAST(i AS VARCHAR) || CASE WHEN i % 5 = 0 THEN '.00' ELSE '' END,
  CASE WHEN i % 7 = 0 THEN NULL ELSE 'k' || (i % 3) END, printf('%.2f', i * 1.5),
  strftime(TIMESTAMP '2026-01-01 00:00:00' + to_seconds(i), '%Y-%m-%dT%H:%M:%S') || '.000Z',
  CASE WHEN i % 2 = 0 THEN 1 ELSE 0 END,
  CASE WHEN i % 97 = 0 THEN '' WHEN i % 50 = 0 THEN 'changed ' || i ELSE 'n' || i END, 2
  FROM range(1, 3051) r(i) WHERE i NOT BETWEEN 1001 AND 1100;
INSERT INTO \"T\" VALUES ('5', 'k2', '7.50', '2026-01-01T00:00:05.000Z', 0, 'second five', 2);";

async fn run(
    left_sql: &str,
    right_sql: &str,
    key: &[&str],
    opts: &Options,
    ctl: &Arc<Ctl>,
    progress: &(dyn Fn(&Progress) + Send + Sync),
) -> CommandResult<Compared> {
    let mut l = duck(left_sql).await;
    let mut r = duck(right_sql).await;
    let (lo, ro) = (table("t"), table("T"));
    let cols = [l.columns(&lo).await.unwrap(), r.columns(&ro).await.unwrap()];
    for s in [&l, &r] {
        if let Some(i) = s.interrupter() {
            ctl.add_stopper(i);
        }
    }
    let input = Input {
        key: key.iter().map(|k| k.to_string()).collect(),
        wanted: Vec::new(),
        cols,
        ..Default::default()
    };
    compare(
        Side {
            session: l.as_mut(),
            object: &lo,
        },
        Side {
            session: r.as_mut(),
            object: &ro,
        },
        input,
        opts,
        ctl,
        progress,
    )
    .await
}

fn opts(in_memory_rows: usize, max_diff_rows: usize, root: &Path) -> Options {
    Options {
        in_memory_rows,
        in_memory_bytes: usize::MAX,
        max_diff_rows,
        max_equal_rows: 0,
        scratch: Some(root.to_path_buf()),
        duckdb: None,
    }
}

/// Everything a compare gives, comparable.
fn summary(c: &Compared) -> Value {
    json!({
        "columns": c.projection.columns.iter().map(|x| [x.0.clone(), x.1.clone()]).collect::<Vec<_>>(),
        "only_left_columns": c.projection.only_left_columns,
        "only_right_columns": c.projection.only_right_columns,
        "counts": serde_json::to_value(&c.counts).unwrap(),
        "only_left": c.only_left,
        "only_right": c.only_right,
        "changed": serde_json::to_value(&c.changed).unwrap(),
        "duplicate_keys": c.duplicate_keys,
        "diff_truncated": c.diff_truncated,
        "equal": c.equal,
        "last_keys": c.last_keys.as_ref().map(|(l, r)| json!([l, r])),
    })
}

fn quiet(_: &Progress) {}

#[tokio::test(flavor = "multi_thread")]
async fn memory_and_duckdb_give_the_same_result() {
    let root = temp_dir("same");
    let ctl = Arc::new(Ctl::default());
    let mem = run(
        LEFT,
        RIGHT,
        &["id", "k2"],
        &opts(1_000_000, 1_000_000, &root.0),
        &ctl,
        &quiet,
    )
    .await
    .unwrap();
    let ctl = Arc::new(Ctl::default());
    let disk = run(
        LEFT,
        RIGHT,
        &["id", "k2"],
        &opts(100, 1_000_000, &root.0),
        &ctl,
        &quiet,
    )
    .await
    .unwrap();
    assert!(!mem.spilled && disk.spilled);
    assert_eq!(summary(&mem), summary(&disk));
    assert_eq!(leftovers(&root.0), Vec::<String>::new());

    // The counts add up over the whole tables.
    let c = &mem.counts;
    assert_eq!((c.left, c.right), (3001, 2951));
    assert_eq!(mem.duplicate_keys, 2);
    assert_eq!((c.only_left, c.only_right), (100, 50));
    assert_eq!(c.same + c.changed + c.only_left + 1, c.left);
    assert_eq!(c.same + c.changed + c.only_right + 1, c.right);
    let changed = (1..=3000)
        .filter(|i| !(1001..=1100).contains(i) && (i % 50 == 0 || i % 97 == 0))
        .count() as u64;
    assert_eq!(c.changed, changed);
    // Columns by name without case, the key first; booleans, numbers as
    // text and dates with T compare equal.
    assert_eq!(
        mem.projection
            .columns
            .iter()
            .map(|x| x.0.as_str())
            .collect::<Vec<_>>(),
        ["id", "k2", "amount", "ts", "flag", "note"]
    );
    assert_eq!(mem.projection.only_left_columns, ["only_l"]);
    assert_eq!(mem.projection.only_right_columns, ["only_r"]);
    let note = 5;
    assert!(
        mem.changed.iter().all(|r| r.diff == [note]),
        "only the note differs"
    );
    // The first row of a repeated key is the one compared: key 5's second
    // row on the right ("second five") isn't a difference.
    assert!(!mem
        .changed
        .iter()
        .any(|r| r.right[note] == json!("second five")));
    // NULL against '' is a difference.
    assert!(mem
        .changed
        .iter()
        .any(|r| r.left[0] == json!(97) && r.left[note].is_null() && r.right[note] == json!("")));
    // The lists come in key order: by number, not as text.
    let ids: Vec<f64> = mem
        .only_left
        .iter()
        .map(|r| r[0].as_f64().unwrap())
        .collect();
    assert!(ids.windows(2).all(|w| w[0] <= w[1]), "{ids:?}");
    assert_eq!(ids.first(), Some(&1001.0));
    // A composite key has no "last key".
    assert!(mem.last_keys.is_none());
}

/// Issue #115: asked for, the equal rows come back on both paths, the same
/// ones in key order, each the left side's values; not asked for, none.
#[tokio::test(flavor = "multi_thread")]
async fn the_equal_rows_come_back_on_both_paths_in_key_order() {
    let root = temp_dir("equal");
    let mut got = Vec::new();
    for in_memory_rows in [1_000_000, 100] {
        let mut o = opts(in_memory_rows, 1_000_000, &root.0);
        o.max_equal_rows = 1_000_000;
        let ctl = Arc::new(Ctl::default());
        let c = run(LEFT, RIGHT, &["id", "k2"], &o, &ctl, &quiet)
            .await
            .unwrap();
        assert_eq!(c.spilled, in_memory_rows == 100);
        assert_eq!(c.equal.len() as u64, c.counts.same);
        got.push(summary(&c));
    }
    assert_eq!(got[0], got[1]);
    let ids: Vec<f64> = got[0]["equal"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r[0].as_f64().unwrap())
        .collect();
    // Every key that is on both sides and doesn't differ, in key order.
    let expected: Vec<f64> = (1..=3000)
        .filter(|i| !(1001..=1100).contains(i) && i % 50 != 0 && i % 97 != 0)
        .map(f64::from)
        .collect();
    assert_eq!(ids, expected);
    // The left's values (its id is a number; the right's is text).
    assert_eq!(
        got[0]["equal"][0],
        json!([1, "k1", "1.50", "2026-01-01 00:00:01", false, "n1"])
    );
    // Not asked for: counted, not listed.
    for in_memory_rows in [1_000_000, 100] {
        let ctl = Arc::new(Ctl::default());
        let c = run(
            LEFT,
            RIGHT,
            &["id", "k2"],
            &opts(in_memory_rows, 1_000_000, &root.0),
            &ctl,
            &quiet,
        )
        .await
        .unwrap();
        assert!(c.equal.is_empty() && c.counts.same > 0);
    }
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

/// Issue #115: past their limit, both paths keep the same first equal rows
/// in key order, also with long keys whose order keys tie (the scratch
/// DuckDB's cut), and the differences stay as they were.
#[tokio::test(flavor = "multi_thread")]
async fn past_their_limit_the_equal_rows_are_the_first_in_key_order() {
    let rows = |table: &str, changed: &str, gap: &str| {
        format!(
            "CREATE TABLE {table} (name VARCHAR, v INTEGER);
INSERT INTO {table} SELECT repeat('x', 150) || lpad(i::VARCHAR, 5, '0'), i{changed} FROM range(1, 2001) r(i){gap};
INSERT INTO {table} SELECT repeat('X', 150) || lpad(i::VARCHAR, 5, '0'), i{changed} FROM range(1, 61) r(i){gap};"
        )
    };
    let left = rows("t", "", "");
    let right = rows(
        "\"T\"",
        " + CASE WHEN i % 3 = 0 THEN 1 ELSE 0 END",
        " WHERE i % 7 <> 0",
    );
    let root = temp_dir("equalcut");
    for (l, r, key) in [
        (LEFT, RIGHT, &["id", "k2"][..]),
        (left.as_str(), right.as_str(), &["name"][..]),
    ] {
        // Also with the differences cut: both cuts on one scratch DuckDB.
        for max_diff_rows in [1_000_000, 30] {
            let mut got = Vec::new();
            for in_memory_rows in [1_000_000, 50] {
                let mut o = opts(in_memory_rows, max_diff_rows, &root.0);
                o.max_equal_rows = 40;
                let ctl = Arc::new(Ctl::default());
                let c = run(l, r, key, &o, &ctl, &quiet).await.unwrap();
                assert_eq!(c.equal.len(), 40);
                assert!(c.counts.same > 40, "the count stays exact");
                assert_eq!(c.diff_truncated, max_diff_rows == 30);
                got.push(summary(&c));
            }
            assert_eq!(got[0], got[1], "{key:?}, {max_diff_rows}");
        }
    }
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn past_the_limit_both_keep_the_first_differences_in_key_order() {
    let root = temp_dir("trunc");
    let mut got = Vec::new();
    for in_memory_rows in [1_000_000, 50] {
        let ctl = Arc::new(Ctl::default());
        let c = run(
            LEFT,
            RIGHT,
            &["id", "k2"],
            &opts(in_memory_rows, 30, &root.0),
            &ctl,
            &quiet,
        )
        .await
        .unwrap();
        assert!(c.diff_truncated);
        assert_eq!(c.only_left.len() + c.only_right.len() + c.changed.len(), 30);
        // The counts stay exact.
        assert_eq!((c.counts.only_left, c.counts.only_right), (100, 50));
        got.push(summary(&c));
    }
    assert_eq!(got[0], got[1]);
    // The first 30 differences by key: changed rows 50, 97, 100…, then
    // the gap from 1001 on.
    let all: Vec<f64> = got[0]["changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["left"][0].as_f64().unwrap())
        .chain(
            got[0]["only_left"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| r[0].as_f64().unwrap()),
        )
        .collect();
    let mut expected: Vec<f64> = (1..=3000)
        .filter(|i| (1001..=1100).contains(i) || i % 50 == 0 || i % 97 == 0)
        .map(f64::from)
        .collect();
    expected.truncate(30);
    let mut sorted = all.clone();
    sorted.sort_by(f64::total_cmp);
    assert_eq!(sorted, expected);
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

/// Keys longer than the cut's window, sharing a long prefix, and pairs
/// whose order keys tie whole (they differ only in case): past the limit,
/// DuckDB's cut window by window keeps the same rows as the memory path.
#[tokio::test(flavor = "multi_thread")]
async fn long_keys_past_the_limit_cut_as_in_memory() {
    let rows = |table: &str, changed: &str, gap: &str| {
        format!(
            "CREATE TABLE {table} (name VARCHAR, v INTEGER);
INSERT INTO {table} SELECT repeat('x', 150) || lpad(i::VARCHAR, 5, '0'), i{changed} FROM range(1, 2001) r(i){gap};
INSERT INTO {table} SELECT repeat('X', 150) || lpad(i::VARCHAR, 5, '0'), i{changed} FROM range(1, 61) r(i){gap};"
        )
    };
    let left = rows("t", "", "");
    let right = rows(
        "\"T\"",
        " + CASE WHEN i % 3 = 0 THEN 1 ELSE 0 END",
        " WHERE i % 7 <> 0",
    );
    let root = temp_dir("longcut");
    let mut got = Vec::new();
    for in_memory_rows in [1_000_000, 50] {
        let ctl = Arc::new(Ctl::default());
        let c = run(
            &left,
            &right,
            &["name"],
            &opts(in_memory_rows, 100, &root.0),
            &ctl,
            &quiet,
        )
        .await
        .unwrap();
        assert!(c.diff_truncated);
        assert_eq!(
            c.only_left.len() + c.only_right.len() + c.changed.len(),
            100
        );
        got.push(summary(&c));
    }
    assert_eq!(got[0], got[1]);
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_one_column_key_keeps_each_sides_last_key_by_value() {
    let root = temp_dir("last");
    let left = "CREATE TABLE t (id INTEGER, v VARCHAR); INSERT INTO t SELECT i, 'x' FROM range(1, 1201) r(i);";
    let right = "CREATE TABLE \"T\" (id VARCHAR, v VARCHAR); INSERT INTO \"T\" SELECT CAST(i AS VARCHAR), 'x' FROM range(1, 1001) r(i);";
    let mut got = Vec::new();
    for in_memory_rows in [1_000_000, 10] {
        let ctl = Arc::new(Ctl::default());
        let c = run(
            left,
            right,
            &["id"],
            &opts(in_memory_rows, 1_000_000, &root.0),
            &ctl,
            &quiet,
        )
        .await
        .unwrap();
        // "1000" by value, not "999" as text.
        assert_eq!(c.last_keys, Some((Some(json!(1200)), Some(json!("1000")))));
        assert_eq!(
            (c.counts.same, c.counts.only_left, c.counts.only_right),
            (1000, 200, 0)
        );
        got.push(summary(&c));
    }
    assert_eq!(got[0], got[1]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_one_side_lacks_is_refused_on_both_paths() {
    let root = temp_dir("nokey");
    for in_memory_rows in [1_000_000, 10] {
        let ctl = Arc::new(Ctl::default());
        let e = run(
            LEFT,
            RIGHT,
            &["only_l"],
            &opts(in_memory_rows, 10, &root.0),
            &ctl,
            &quiet,
        )
        .await
        .err()
        .expect("refused");
        assert!(e.to_string().contains("no está en las dos tablas"), "{e}");
    }
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_folder_for_duckdb_a_big_table_says_why() {
    let ctl = Arc::new(Ctl::default());
    let o = Options {
        in_memory_rows: 10,
        in_memory_bytes: usize::MAX,
        max_diff_rows: 10,
        max_equal_rows: 0,
        scratch: None,
        duckdb: None,
    };
    let e = run(LEFT, RIGHT, &["id", "k2"], &o, &ctl, &quiet)
        .await
        .err()
        .expect("refused");
    assert!(matches!(e, CommandError::State(_)), "{e:?}");
    assert!(e.to_string().contains("demasiado grande"), "{e}");
}

#[tokio::test(flavor = "multi_thread")]
async fn wide_rows_go_to_duckdb_by_their_bytes() {
    let root = temp_dir("bytes");
    let ctl = Arc::new(Ctl::default());
    let mem = run(
        LEFT,
        RIGHT,
        &["id", "k2"],
        &opts(1_000_000, 1_000_000, &root.0),
        &ctl,
        &quiet,
    )
    .await
    .unwrap();
    // Far under the row limit, past the byte budget.
    let mut o = opts(1_000_000, 1_000_000, &root.0);
    o.in_memory_bytes = 64 << 10;
    let ctl = Arc::new(Ctl::default());
    let disk = run(LEFT, RIGHT, &["id", "k2"], &o, &ctl, &quiet)
        .await
        .unwrap();
    assert!(!mem.spilled && disk.spilled);
    assert_eq!(summary(&mem), summary(&disk));
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_scratch_opened_early_matters_only_if_it_is_needed() {
    // 3001 rows: past half of 4000, so the scratch DuckDB starts opening;
    // under 4000, so the compare stays in memory. Without a folder it
    // can't open, and that's no error then.
    let ctl = Arc::new(Ctl::default());
    let mut o = opts(4_000, 1_000_000, Path::new("/"));
    o.scratch = None;
    let c = run(LEFT, RIGHT, &["id", "k2"], &o, &ctl, &quiet)
        .await
        .unwrap();
    assert!(!c.spilled);
    assert_eq!((c.counts.left, c.counts.right), (3001, 2951));
    // With a folder, the one that opened is deleted all the same.
    let root = temp_dir("early");
    let ctl = Arc::new(Ctl::default());
    let c = run(
        LEFT,
        RIGHT,
        &["id", "k2"],
        &opts(4_000, 1_000_000, &root.0),
        &ctl,
        &quiet,
    )
    .await
    .unwrap();
    assert!(!c.spilled);
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

/// A session whose batched read starts late (a cloud warehouse running its
/// job, a view whose first row waits for a sort): it gives its first row
/// only once `go` is set (or after a minute), so the test decides when
/// rather than a timer that a slow machine outruns.
struct Slow(Box<dyn Session>, Arc<std::sync::atomic::AtomicBool>);

#[dbine_driver::async_trait]
impl Session for Slow {
    async fn server_version(&mut self) -> dbine_driver::Result<String> {
        self.0.server_version().await
    }
    async fn list_databases(&mut self) -> dbine_driver::Result<Vec<String>> {
        self.0.list_databases().await
    }
    async fn list_objects(&mut self) -> dbine_driver::Result<Vec<dbine_driver::DbObject>> {
        self.0.list_objects().await
    }
    async fn columns(&mut self, obj: &ObjectRef) -> dbine_driver::Result<Vec<ColumnInfo>> {
        self.0.columns(obj).await
    }
    async fn definition(&mut self, obj: &ObjectRef) -> dbine_driver::Result<Option<String>> {
        self.0.definition(obj).await
    }
    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        self.0.browse_query(obj, limit)
    }
    async fn execute(
        &mut self,
        text: &str,
        max_rows: usize,
        out: &mut QueryOutcome,
    ) -> dbine_driver::Result<()> {
        self.0.execute(text, max_rows, out).await
    }
    async fn read_batches(
        &mut self,
        spec: &ReadSpec,
        sink: dbine_driver::BatchSinkRef,
    ) -> dbine_driver::Result<u64> {
        let waited = std::time::Instant::now();
        while !self.1.load(std::sync::atomic::Ordering::SeqCst)
            && waited.elapsed() < Duration::from_secs(60)
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        self.0.read_batches(spec, sink).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_side_goes_on_while_the_other_has_not_begun() {
    // Each side projects its rows as they come (the columns are known
    // before reading), so the fast side is read whole, to DuckDB, while
    // the slow one hasn't given a row: nothing waits for it, nothing piles
    // up raw.
    let root = temp_dir("slow");
    let ctl = Arc::new(Ctl::default());
    let mut l = duck(LEFT).await;
    let mut r = duck(RIGHT).await;
    let (lo, ro) = (table("t"), table("T"));
    let cols = [l.columns(&lo).await.unwrap(), r.columns(&ro).await.unwrap()];
    // The slow side starts once the progress has shown the fast one read
    // whole while it hadn't given a row (progress is sampled every 250 ms,
    // and the scratch DuckDB may take a while to set up on a busy machine).
    let go = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut slow = Slow(r, go.clone());
    let input = Input {
        key: vec!["id".into(), "k2".into()],
        wanted: Vec::new(),
        cols,
        ..Default::default()
    };
    let seen = Arc::new(Mutex::new(Vec::<Progress>::new()));
    let s2 = seen.clone();
    let progress = move |p: &Progress| {
        if p.rows == [3001, 0] {
            go.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        s2.lock().unwrap().push(p.clone());
    };
    let got = compare(
        Side {
            session: l.as_mut(),
            object: &lo,
        },
        Side {
            session: &mut slow,
            object: &ro,
        },
        input,
        &opts(100, 1_000_000, &root.0),
        &ctl,
        &progress,
    )
    .await
    .unwrap();
    assert!(got.spilled);
    assert_eq!((got.counts.left, got.counts.right), (3001, 2951));
    let seen = seen.lock().unwrap();
    assert!(seen.iter().any(|p| p.rows == [3001, 0]), "{seen:?}");
    drop(seen);
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_stops_the_reads_and_the_duckdb_work_and_deletes_it() {
    let root = temp_dir("cancel");
    let big = "CREATE TABLE t (id BIGINT, v VARCHAR); INSERT INTO t SELECT i, 'value ' || i FROM range(2000000) r(i);";
    let big_r = "CREATE TABLE \"T\" (id BIGINT, v VARCHAR); INSERT INTO \"T\" SELECT i, 'value ' || i FROM range(2000000) r(i);";
    // Cancelled before it starts.
    let ctl = Arc::new(Ctl::default());
    ctl.cancel();
    let e = run(big, big_r, &["id"], &opts(1_000, 10, &root.0), &ctl, &quiet)
        .await
        .err()
        .expect("cancelled");
    assert!(matches!(e, CommandError::Cancelled), "{e:?}");
    // Cancelled while reading into DuckDB.
    let ctl = Arc::new(Ctl::default());
    let c2 = ctl.clone();
    let seen = Arc::new(Mutex::new(Vec::<Progress>::new()));
    let s2 = seen.clone();
    let progress = move |p: &Progress| {
        s2.lock().unwrap().push(p.clone());
        if p.rows[0] > 5_000 {
            c2.cancel();
        }
    };
    let started = std::time::Instant::now();
    let e = run(
        big,
        big_r,
        &["id"],
        &opts(1_000, 10, &root.0),
        &ctl,
        &progress,
    )
    .await
    .err()
    .expect("cancelled");
    assert!(matches!(e, CommandError::Cancelled), "{e:?}");
    let seen = seen.lock().unwrap();
    // Cancelled while reading (preparing the scratch DuckDB is part of it):
    // it never got to compare.
    assert!(
        seen.iter()
            .all(|p| matches!(p.phase, Phase::Reading | Phase::Preparing)),
        "{seen:?}"
    );
    assert!(seen.last().unwrap().rows[0] < 2_000_000, "{seen:?}");
    eprintln!("cancelled after {:?}", started.elapsed());
    // Nothing left on disk.
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

/// The sources and the scratch database on one DuckDB host process, as a
/// release build has them when a `duckdb` or `duckdb_files` connection is
/// compared: a read waiting for the scratch database must not hold the
/// host's reader thread, which brings the scratch database's replies (the
/// deadlock the review found when rows came through `execute`'s row sink).
///
/// Ignored by default: it needs a host built with the DuckDB driver:
///
/// ```sh
/// cargo build -p dbine-plugin-host --no-default-features --features duckdb
/// DBINE_TEST_DUCKDB_HOST=$PWD/target/debug/dbine-plugin-host \
///   cargo test -p dbine --lib -- --ignored sources_and_scratch_on_one_duckdb_host
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a DuckDB driver host: DBINE_TEST_DUCKDB_HOST"]
async fn sources_and_scratch_on_one_duckdb_host() {
    let exe = std::env::var("DBINE_TEST_DUCKDB_HOST").expect("DBINE_TEST_DUCKDB_HOST");
    let local = dbine_drivers::find("duckdb").expect("the DuckDB driver");
    let launcher = dbine_plugin::Launcher::at(
        "duckdb",
        PathBuf::from(exe),
        Vec::new(),
        Some(dbine_driver::runtime::components_dir()),
    );
    let host: Arc<dyn Driver> = Arc::new(dbine_plugin::RemoteDriver::new(
        dbine_plugin::DriverMeta::of("duckdb", local.as_ref()),
        launcher,
    ));
    let open = |sql: &'static str| {
        let host = host.clone();
        async move {
            let mut s = host
                .connect(&local_duckdb::config(), None)
                .await
                .expect("a session on the host");
            local_duckdb::exec(s.as_mut(), sql).await.expect("setup");
            s
        }
    };
    // 300,000 rows; the right lacks 100–199 and changes every 1,000th.
    let mut l =
        open("CREATE TABLE t AS SELECT i AS id, 'v' || i AS v FROM range(300000) r(i)").await;
    let mut r = open(
        "CREATE TABLE \"T\" AS SELECT i AS id, 'v' || i || CASE WHEN i % 1000 = 0 THEN 'x' ELSE '' END AS v FROM range(300000) r(i) WHERE i NOT BETWEEN 100 AND 199",
    )
    .await;
    let (lo, ro) = (table("t"), table("T"));
    let cols = [l.columns(&lo).await.unwrap(), r.columns(&ro).await.unwrap()];
    let root = temp_dir("host");
    let mut o = opts(1_000, 1_000_000, &root.0);
    o.duckdb = Some(host.clone());
    let ctl = Arc::new(Ctl::default());
    let input = Input {
        key: vec!["id".into()],
        wanted: Vec::new(),
        cols,
        ..Default::default()
    };
    let started = std::time::Instant::now();
    let got = tokio::time::timeout(
        Duration::from_secs(180),
        compare(
            Side {
                session: l.as_mut(),
                object: &lo,
            },
            Side {
                session: r.as_mut(),
                object: &ro,
            },
            input,
            &o,
            &ctl,
            &quiet,
        ),
    )
    .await
    .expect("the compare ends (no deadlock on the host's reader thread)")
    .unwrap();
    eprintln!("one DuckDB host: {:?}", started.elapsed());
    assert!(got.spilled);
    assert_eq!(
        got.counts,
        Counts {
            left: 300_000,
            right: 299_900,
            same: 299_600,
            only_left: 100,
            only_right: 0,
            changed: 300,
        }
    );
    drop((l, r));
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

/// The files under `dir`, and those holding `mark` in plain text.
fn scan(dir: &Path, mark: &str) -> (Vec<PathBuf>, Vec<String>) {
    let (mut files, mut found) = (Vec::new(), Vec::new());
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if std::fs::read(&p).is_ok_and(|b| b.windows(mark.len()).any(|w| w == mark.as_bytes()))
            {
                found.push(p.display().to_string());
            }
            files.push(p);
        }
    }
    (files, found)
}

/// Scans a folder when the first row of a result comes out.
struct ScanOnFirstRow {
    dir: PathBuf,
    mark: &'static str,
    seen: Option<(Vec<PathBuf>, Vec<String>)>,
}

impl RowSink for ScanOnFirstRow {
    fn begin(&mut self, _: usize, _: &[ResultColumn]) -> std::io::Result<()> {
        Ok(())
    }
    fn row(&mut self, _: usize, _: &[Value]) -> std::io::Result<()> {
        if self.seen.is_none() {
            self.seen = Some(scan(&self.dir, self.mark));
        }
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_scratch_database_is_encrypted_and_goes_when_it_closes() {
    const MARK: &str = "PLAINTEXT_MARKER_dc88";
    let root = temp_dir("enc");
    let ctl = Ctl::default();
    // A small memory limit, so the work spills to the temp files.
    let mut s = Scratch::open(Some(&root.0), &ctl, None, "128MB")
        .await
        .expect("opens");
    let dir = s.dir.clone();
    assert!(dir.starts_with(&root.0));
    s.create(&ctl).await.unwrap();
    // About 160 MB of rows with the marker, half on each side, then a few
    // small inserts, which go through the WAL.
    local_duckdb::exec(
        s.session(),
        &format!(
            "INSERT INTO main.rows SELECT i % 2, i, i // 2, 0, 'k' || (i // 2), 'o' || (i // 2), i, i, '{MARK} ' || i || repeat('x', 300) FROM range(500000) r(i)"
        ),
    )
    .await
    .unwrap();
    for i in 0..3 {
        local_duckdb::exec(
            s.session(),
            &format!(
                "INSERT INTO main.rows VALUES (0, -{i}, -{i} - 1, 0, 'w{i}', 'w', 0, 0, '{MARK} wal {i}')"
            ),
        )
        .await
        .unwrap();
    }
    // The database and its WAL, before any checkpoint.
    let (files, found) = scan(&dir, MARK);
    assert_eq!(found, Vec::<String>::new());
    assert!(
        files.iter().any(|f| f.to_string_lossy().ends_with(".wal")),
        "a WAL to check: {files:?}"
    );
    // The compare's join over the whole tables, then a grouping bigger
    // than the memory limit: its partitions are in the temp files when the
    // first row comes out, and that's when they're read.
    let (counts, _) = s.counts(&ctl).await.unwrap();
    assert_eq!((counts.left, counts.right), (250_003, 250_000));
    let spilled = Arc::new(Mutex::new(ScanOnFirstRow {
        dir: dir.join("tmp"),
        mark: MARK,
        seen: None,
    }));
    let mut out = QueryOutcome {
        sink: Some(RowSinkRef(spilled.clone())),
        ..Default::default()
    };
    s.session()
        .execute(
            "SELECT v, count(*) FROM main.rows GROUP BY v",
            usize::MAX,
            &mut out,
        )
        .await
        .unwrap();
    out.sink = None;
    assert_eq!(out.error, None);
    let (files, found) = lock(&spilled).seen.take().expect("rows came out");
    assert!(!files.is_empty(), "nothing spilled to tmp/");
    assert_eq!(found, Vec::<String>::new());
    local_duckdb::exec(s.session(), "CHECKPOINT").await.unwrap();
    let (_, found) = scan(&dir, MARK);
    assert_eq!(found, Vec::<String>::new());
    // Locked: nothing else can be attached or read.
    assert!(
        local_duckdb::exec(s.session(), "SET lock_configuration = false")
            .await
            .is_err()
    );
    let other = root.0.join("other.duckdb");
    assert!(local_duckdb::exec(
        s.session(),
        &format!(
            "ATTACH {} AS other",
            local_duckdb::lit(&other.to_string_lossy())
        )
    )
    .await
    .is_err());
    assert!(!other.exists());
    assert!(dir.join(FILE).exists());
    s.close().await;
    assert!(!dir.exists());
}

/// The compare's SQL over tables far bigger than the scratch DuckDB's
/// memory (10,000,000 rows a side at the app's limit): the join, the
/// counts and the differences work on the encrypted temp files. Prints the
/// time and the process's peak memory.
///
/// Ignored by default (about 2 GB on disk while it runs):
/// `cargo test -p dbine --lib -- --ignored the_scratch_compares_tables_far_bigger --nocapture`
#[tokio::test(flavor = "multi_thread")]
#[ignore = "heavy: 20,000,000 rows on disk"]
async fn the_scratch_compares_tables_far_bigger_than_its_memory() {
    fn rss() -> u64 {
        std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0)
            / 1024
    }
    let peak = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (peak, done) = (peak.clone(), done.clone());
        std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                peak.fetch_max(rss(), Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    };
    let root = temp_dir("big");
    let ctl = Arc::new(Ctl::default());
    let started = std::time::Instant::now();
    let mut s = Scratch::open(Some(&root.0), &ctl, None, MEMORY_LIMIT)
        .await
        .expect("opens");
    s.create(&ctl).await.unwrap();
    const N: i64 = 10_000_000;
    // Rows as the compare writes them: key, order key, hash, the row.
    let side = |side: u8, extra: &str, filter: &str| {
        format!(
            "INSERT INTO main.rows SELECT {side}, i, i, 0, 'k' || i, '0' || lpad(i::VARCHAR, 16, '0'), i, i{extra}, '[' || i || ',\"value ' || i || '\"]' FROM range({N}) r(i){filter}"
        )
    };
    local_duckdb::exec(s.session(), &side(0, "", ""))
        .await
        .unwrap();
    local_duckdb::exec(
        s.session(),
        &side(
            1,
            " + CASE WHEN i % 10000 = 0 THEN 1 ELSE 0 END",
            " WHERE i NOT BETWEEN 1000 AND 1999",
        ),
    )
    .await
    .unwrap();
    let loaded = started.elapsed();
    let (counts, dups) = s.counts(&ctl).await.unwrap();
    let counted = started.elapsed();
    assert_eq!(dups, 0);
    assert_eq!(
        counts,
        Counts {
            left: N as u64,
            right: N as u64 - 1000,
            same: N as u64 - 2000,
            only_left: 1000,
            only_right: 0,
            changed: 1000,
        }
    );
    let projection = Arc::new(Projection {
        columns: vec![("id".into(), "id".into()), ("v".into(), "v".into())],
        nk: 1,
        bools: [vec![false; 2], vec![false; 2]],
        vectors: Default::default(),
        only_left_columns: Vec::new(),
        only_right_columns: Vec::new(),
    });
    let job = Arc::new(Job::new(
        ctl.clone(),
        projection,
        &opts(1, 1_000_000, &root.0),
    ));
    let lists = s.differences(&ctl, &job, 1, 1_000_000).await.unwrap();
    assert_eq!(
        (
            lists.only_left.len(),
            lists.only_right.len(),
            lists.changed.len()
        ),
        (1000, 0, 1000)
    );
    let total = started.elapsed();
    s.close().await;
    done.store(true, Ordering::SeqCst);
    let _ = sampler.join();
    eprintln!(
        "20M rows: loaded in {loaded:?}, counted at {counted:?}, differences at {total:?}; peak RSS {} MiB",
        peak.load(Ordering::SeqCst)
    );
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

#[test]
fn cells_compare_as_the_grid_values_did() {
    // Each engine's batched read gives typed cells; as compared they equal
    // what the grid gave for the same data (and what another engine gives).
    // The assertion names no value: a failure shows the two canon strings
    // only (CodeQL reads a printed cell as possible cleartext logging).
    let same = |c: Cell, grid: Value| {
        assert_eq!(canon(&value_of(c)), canon(&grid));
    };
    same(Cell::Int(7), json!(7));
    same(Cell::Int(7), json!("7"));
    same(Cell::UInt(u64::from(u32::MAX)), json!(4294967295u64));
    same(Cell::Float(2.5), json!("2.50"));
    same(Cell::Decimal("10.50".into()), json!(10.5));
    same(Cell::Decimal("-0.10".into()), json!("-0.1"));
    same(Cell::Bool(true), json!(1));
    same(Cell::Bool(false), json!("0"));
    same(Cell::Text("Ana".into()), json!("Ana"));
    same(Cell::Null, json!(null));
    same(Cell::Date("2026-03-01".into()), json!("2026-03-01"));
    same(Cell::Time("10:00:00".into()), json!("10:00:00"));
    same(
        Cell::DateTime("2026-03-01 10:00:00.1230000".into()),
        json!("2026-03-01T10:00:00.123"),
    );
    same(
        Cell::DateTime("2026-03-01 10:00:00".into()),
        json!("2026-03-01T10:00:00.000Z"),
    );
    same(
        Cell::DateTimeTz("2026-03-01 10:00:00.1230000+02:00".into()),
        json!("2026-03-01 10:00:00.123+02:00"),
    );
    same(
        Cell::Uuid("6f9619ff-8b86-d011-b42d-00c04fc964ff".into()),
        json!("6f9619ff-8b86-d011-b42d-00c04fc964ff"),
    );
    same(Cell::Json("{\"a\":1}".into()), json!({"a": 1}));
    same(Cell::Json("not json".into()), json!("not json"));
    // Binaries whole, as the `0x…` text scripts write.
    assert_eq!(value_of(Cell::Bytes(vec![0, 0xAB])), json!("0x00AB"));
    // Distinct values stay distinct.
    assert_ne!(
        canon(&value_of(Cell::Text("007".into()))),
        canon(&value_of(Cell::Int(7)))
    );
    assert_ne!(
        canon(&value_of(Cell::Null)),
        canon(&value_of(Cell::Text(String::new())))
    );
}

#[test]
fn the_order_key_sorts_as_the_grid_does() {
    let values = [
        json!(-5),
        json!(-0.5),
        json!(0),
        json!("2"),
        json!(10),
        json!("10.5"),
        json!(1e20),
        json!("abc"),
        json!("ABD"),
        json!("b"),
        json!("ba"),
        json!(null),
    ];
    for a in &values {
        for b in &values {
            let by_grid = sort_value(a).cmp_with(&sort_value(b), false);
            let by_key =
                order_key(std::slice::from_ref(a)).cmp(&order_key(std::slice::from_ref(b)));
            assert_eq!(by_grid, by_key, "{a} vs {b}");
        }
    }
    // Column by column: the first decides, then the second.
    let k = |a: Value, b: Value| order_key(&[a, b]);
    assert!(k(json!(1), json!("z")) < k(json!(2), json!("a")));
    assert!(k(json!("a"), json!(9)) < k(json!("a"), json!(10)));
    assert!(k(json!("a"), json!(10)) < k(json!("ab"), json!(1)));
}

#[test]
fn equal_rows_hash_alike_by_their_canonical_values() {
    let row = |v: Vec<Value>| v;
    let h = |p: &[Value]| hash_of(p, 1);
    assert_eq!(
        h(&row(vec![
            json!(1),
            json!(1),
            json!("2026-09-28T10:00:00.000Z"),
            json!(true)
        ])),
        h(&row(vec![
            json!("1"),
            json!("1.00"),
            json!("2026-09-28 10:00:00"),
            json!(1)
        ]))
    );
    assert_ne!(h(&[json!(1), json!(null)]), h(&[json!(1), json!("")]));
    assert_ne!(h(&[json!(1), json!("007")]), h(&[json!(1), json!("7")]));
    // Values don't run into each other.
    assert_ne!(
        h(&[json!(1), json!("ab"), json!("c")]),
        h(&[json!(1), json!("a"), json!("bc")])
    );
    // The key isn't part of it.
    assert_eq!(h(&[json!(1), json!("x")]), h(&[json!(2), json!("x")]));
    assert_eq!(
        key_of(&[json!(1), json!("1.00")], 2),
        key_of(&[json!("1"), json!(1)], 2)
    );
}

/// Keys that tie on their whole order key (here 300 keys with one order key,
/// as numbers past `f64`'s precision or text that differs only in case
/// give): past the limit, the cut breaks the tie by key and DuckDB hands
/// back exactly the first `max`, as the memory path would keep.
#[tokio::test(flavor = "multi_thread")]
async fn order_key_ties_are_cut_by_key() {
    let root = temp_dir("ties");
    let ctl = Arc::new(Ctl::default());
    let mut s = Scratch::open(Some(&root.0), &ctl, None, MEMORY_LIMIT)
        .await
        .expect("opens");
    s.create(&ctl).await.unwrap();
    for side in 0..2 {
        local_duckdb::exec(
            s.session(),
            &format!(
                "INSERT INTO main.rows SELECT {side}, i, i, 0, 'k' || lpad(i::VARCHAR, 3, '0'), 'same', i + {side}, i, \
                 '[' || i || ',\"x\"]' FROM range(300) r(i)"
            ),
        )
        .await
        .unwrap();
    }
    let projection = Arc::new(Projection {
        columns: vec![("id".into(), "id".into()), ("v".into(), "v".into())],
        nk: 1,
        bools: [vec![false; 2], vec![false; 2]],
        vectors: Default::default(),
        only_left_columns: Vec::new(),
        only_right_columns: Vec::new(),
    });
    let job = Arc::new(Job::new(ctl.clone(), projection, &opts(1, 10, &root.0)));
    s.counts(&ctl).await.unwrap();
    let lists = s.differences(&ctl, &job, 1, 10).await.unwrap();
    let mut ids: Vec<i64> = lists
        .changed
        .iter()
        .map(|r| r.left[0].as_i64().unwrap())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, (0..10).collect::<Vec<_>>());
    s.close().await;
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

/// Keys as long as a long text column (2,000 characters, sharing a long
/// prefix) over 1,000,000 rows a side, at the app's memory limit: the
/// counts and the differences join and group by the key's hash, and the
/// cut past the limit sorts short windows of the order key (issue #107:
/// DuckDB ran out of memory on the whole keys). Repeated keys, every key
/// changed, and more differing rows than the list keeps.
///
/// Ignored by default (about 5 GB on disk while it runs):
/// `cargo test -p dbine --lib -- --ignored long_keys_fit --nocapture`
#[tokio::test(flavor = "multi_thread")]
#[ignore = "heavy: 2,000,000 rows of 2 KB keys on disk"]
async fn long_keys_fit_the_scratch_memory() {
    const N: i64 = 1_000_000;
    for (groups, max) in [(N / 4, 1_000_000), (N, 1_000_000), (N, 100_000)] {
        let root = temp_dir("longkeys");
        let ctl = Arc::new(Ctl::default());
        let started = std::time::Instant::now();
        let mut s = Scratch::open(Some(&root.0), &ctl, None, MEMORY_LIMIT)
            .await
            .expect("opens");
        s.create(&ctl).await.unwrap();
        // Each key `groups` apart repeats; every row differs from its pair.
        for side in 0..2 {
            local_duckdb::exec(
                s.session(),
                &format!(
                    "INSERT INTO main.rows SELECT {side}, i, (hash(k) >> 1)::BIGINT, 0, k, '1' || k, i + {side}, i, \
                     '[' || i || ',\"' || repeat('x', 200) || '\"]' \
                     FROM (SELECT i, repeat('k', 2000) || lpad((i % {groups})::VARCHAR, 7, '0') AS k FROM range({N}) r(i))"
                ),
            )
            .await
            .unwrap();
        }
        let (counts, dups) = s.counts(&ctl).await.unwrap();
        assert_eq!(
            (counts.changed, dups),
            (groups as u64, 2 * (N - groups) as u64)
        );
        let projection = Arc::new(Projection {
            columns: vec![("id".into(), "id".into()), ("v".into(), "v".into())],
            nk: 1,
            bools: [vec![false; 2], vec![false; 2]],
            vectors: Default::default(),
            only_left_columns: Vec::new(),
            only_right_columns: Vec::new(),
        });
        let job = Arc::new(Job::new(ctl.clone(), projection, &opts(1, max, &root.0)));
        let lists = s.differences(&ctl, &job, 1, max).await.unwrap();
        // Past the limit, the first `max` by order key: keys 0 to max - 1.
        assert_eq!(lists.changed.len(), max.min(groups as usize));
        if max < groups as usize {
            let mut ids: Vec<i64> = lists
                .changed
                .iter()
                .map(|r| r.left[0].as_i64().unwrap())
                .collect();
            ids.sort_unstable();
            assert_eq!(ids, (0..max as i64).collect::<Vec<_>>());
        }
        s.close().await;
        eprintln!(
            "{N} rows a side, {groups} keys, list of {max}: {:?}",
            started.elapsed()
        );
        assert_eq!(leftovers(&root.0), Vec::<String>::new());
    }
}

/// Issue #107 as reported: a short composite key on which nothing matches
/// (the key adjusted to a column that differs between the sides), so every
/// row is a difference: 8,000,000 of them. Picking the first
/// `MAX_DIFF_ROWS` must fit the scratch memory; 1.6.0 sorted them all and
/// ran out of it from about 2,500,000 rows a side.
#[tokio::test(flavor = "multi_thread")]
async fn millions_of_differences_fit_the_scratch_memory() {
    const N: i64 = 4_000_000;
    let max = super::super::MAX_DIFF_ROWS;
    let root = temp_dir("many");
    let ctl = Arc::new(Ctl::default());
    let mut s = Scratch::open(Some(&root.0), &ctl, None, MEMORY_LIMIT)
        .await
        .expect("opens");
    s.create(&ctl).await.unwrap();
    // Key: (number, branch code); the right side's numbers are all odd.
    let side = |side: u8| {
        format!(
            "INSERT INTO main.rows SELECT {side}, i, \
             (hash(i * 2 + {side}) >> 1)::BIGINT, (hash(i * 2 + {side} + 100000000000) >> 1)::BIGINT, \
             \
             (i * 2 + {side})::VARCHAR || chr(1) || 'SUC-' || lpad((i % 997)::VARCHAR, 4, '0'), \
             '0' || lpad((i * 2 + {side})::VARCHAR, 16, '0') || chr(1) || '1SUC-' || lpad((i % 997)::VARCHAR, 4, '0') || chr(1), \
             i, i, \
             '[' || (i * 2 + {side}) || ',\"SUC-' || lpad((i % 997)::VARCHAR, 4, '0') || '\",' || (i * 1.5)::VARCHAR || ',\"' || md5(i::VARCHAR) || '\"]' \
             FROM range({N}) r(i)"
        )
    };
    local_duckdb::exec(s.session(), &side(0)).await.unwrap();
    local_duckdb::exec(s.session(), &side(1)).await.unwrap();
    let (counts, dups) = s.counts(&ctl).await.unwrap();
    assert_eq!(dups, 0);
    assert_eq!(
        counts,
        Counts {
            left: N as u64,
            right: N as u64,
            same: 0,
            only_left: N as u64,
            only_right: N as u64,
            changed: 0,
        }
    );
    let projection = Arc::new(Projection {
        columns: ["id", "suc", "importe", "hash"]
            .iter()
            .map(|c| (c.to_string(), c.to_string()))
            .collect(),
        nk: 2,
        bools: [vec![false; 4], vec![false; 4]],
        vectors: Default::default(),
        only_left_columns: Vec::new(),
        only_right_columns: Vec::new(),
    });
    let job = Arc::new(Job::new(ctl.clone(), projection, &opts(1, max, &root.0)));
    let lists = s.differences(&ctl, &job, 2, max).await.unwrap();
    // The first `max` in key order: the numbers 0..max, alternating sides.
    assert_eq!(
        (
            lists.only_left.len(),
            lists.only_right.len(),
            lists.changed.len()
        ),
        (max / 2, max / 2, 0)
    );
    s.close().await;
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

/// Vector columns compare by value, in memory and in the scratch DuckDB:
/// `[1e-07, 2]` is `[0.0000001,2.0]`, and digits a float32 can't hold
/// don't count. Without the catalog's mark they'd compare as text.
#[tokio::test(flavor = "multi_thread")]
async fn vector_columns_compare_by_value_on_both_paths() {
    const L: &str = "CREATE TABLE t (id INTEGER, v VARCHAR);
INSERT INTO t VALUES (1, '[1e-07, 2]'), (2, '[0.1,0.2]'), (3, '[1,2,3]'), (4, NULL), (5, '[1,2]');";
    const R: &str = "CREATE TABLE \"T\" (id INTEGER, v VARCHAR);
INSERT INTO \"T\" VALUES (1, '[0.0000001,2.0]'), (2, '[0.100000001, 0.2]'), (3, '[1,2,4]'), (4, NULL), (5, '[1,2,0]');";
    let root = temp_dir("vectors");
    let compare_with_marks = |marked: bool, in_memory_rows: usize| {
        let root = root.0.clone();
        async move {
            let mut l = duck(L).await;
            let mut r = duck(R).await;
            let (lo, ro) = (table("t"), table("T"));
            let cols = [l.columns(&lo).await.unwrap(), r.columns(&ro).await.unwrap()];
            let mark = || {
                if marked {
                    HashMap::from([("v".to_string(), VectorElement::F32)])
                } else {
                    HashMap::new()
                }
            };
            let input = Input {
                key: vec!["id".into()],
                wanted: Vec::new(),
                cols,
                vectors: [mark(), mark()],
            };
            compare(
                Side {
                    session: l.as_mut(),
                    object: &lo,
                },
                Side {
                    session: r.as_mut(),
                    object: &ro,
                },
                input,
                &opts(in_memory_rows, 1_000_000, &root),
                &Arc::new(Ctl::default()),
                &quiet,
            )
            .await
            .unwrap()
        }
    };
    let memory = compare_with_marks(true, 1_000_000).await;
    let disk = compare_with_marks(true, 1).await;
    assert!(!memory.spilled && disk.spilled);
    assert_eq!(summary(&memory), summary(&disk));
    assert_eq!((memory.counts.same, memory.counts.changed), (3, 2));
    let changed: Vec<Value> = memory.changed.iter().map(|c| c.left[0].clone()).collect();
    assert_eq!(changed, [json!(3), json!(5)]);
    // The values kept (shown, and written by a sync script) are the
    // vectors' exact text.
    assert_eq!(memory.changed[0].left[1], json!("[1,2,3]"));
    assert_eq!(memory.changed[1].right[1], json!("[1,2,0]"));
    // Unmarked, the text differs on 1, 2, 3 and 5.
    let text = compare_with_marks(false, 1_000_000).await;
    assert_eq!((text.counts.same, text.counts.changed), (1, 4));
    assert_eq!(leftovers(&root.0), Vec::<String>::new());
}

/// A query's result marks its own vector columns as it begins: their cells
/// come out of the read as compared (`[1e-07, 2]` as `[0.0000001,2]`),
/// other columns as they came.
#[test]
fn a_query_results_vector_columns_compare_by_value() {
    use dbine_driver::VectorMeta;
    let root = temp_dir("relay-vectors");
    let projection = Arc::new(Projection {
        columns: vec![
            ("id".into(), "id".into()),
            ("v".into(), "v".into()),
            ("t".into(), "t".into()),
        ],
        nk: 1,
        bools: [vec![false; 3], vec![false; 3]],
        vectors: Default::default(),
        only_left_columns: Vec::new(),
        only_right_columns: Vec::new(),
    });
    let job = Arc::new(Job::new(
        Arc::new(Ctl::default()),
        projection,
        &opts(1_000, 1_000, &root.0),
    ));
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut relay = Relay {
        side: 0,
        job,
        tx,
        queued: Arc::new(AtomicUsize::new(0)),
        cap: usize::MAX,
        hold: true,
        index: None,
        pick: Vec::new(),
        bools: Vec::new(),
        vectors: Vec::new(),
        batch: Vec::new(),
        batch_bytes: 0,
    };
    let cols = [
        ResultColumn::new("id", "int"),
        ResultColumn::new("v", "vector")
            .with_vector(VectorMeta::dense(Some(2), VectorElement::F32)),
        ResultColumn::new("t", "text"),
    ];
    relay.begin(0, &cols).unwrap();
    relay
        .row(0, &[json!(1), json!("[1e-07, 2]"), json!("[1e-07, 2]")])
        .unwrap();
    relay.send().unwrap();
    let got = rx.try_recv().unwrap();
    assert_eq!(
        got[0].0,
        vec![json!(1), json!("[0.0000001,2]"), json!("[1e-07, 2]")]
    );
}
