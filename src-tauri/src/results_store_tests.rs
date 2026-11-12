use super::*;
use dbine_driver::{ResultColumn, StatementResult};
use serde_json::json;

const MARK: &str = "PLAINTEXT_MARKER_c41e";

fn key(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

/// A test's own folder, deleted when the test ends (passed or not).
struct TempDir(PathBuf);

impl std::ops::Deref for TempDir {
    type Target = PathBuf;
    fn deref(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn temp_dir(name: &str) -> TempDir {
    let d = std::env::temp_dir().join(format!("dbine-results-test-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    TempDir(d)
}

fn col(name: &str, type_name: &str) -> ResultColumn {
    ResultColumn {
        name: name.into(),
        type_name: type_name.into(),
    }
}

/// Every kind of value the grid gets, with text no one should find on disk.
fn sample(rows: usize) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    out.results.push(StatementResult {
        columns: vec![
            col("id", "int4"),
            col("name", "text"),
            col("amount", "numeric"),
            col("doc", "jsonb"),
            col("flag", "bool"),
            col("big", "int8"),
            col("when", "timestamptz"),
        ],
        rows: (0..rows)
            .map(|i| {
                vec![
                    json!(i),
                    json!(format!("{MARK} ñandú 日本 '\"\\ {i}")),
                    json!("12345678901234567890.0100"),
                    json!({ "k": [1, 2.5, null, "x"], "nested": { "a": true } }),
                    if i % 2 == 0 { json!(true) } else { Value::Null },
                    json!("9223372036854775807"),
                    json!("2026-11-11 19:01:57.594977+00"),
                ]
            })
            .collect(),
        total_rows: rows as u64,
        truncated: false,
        line: Some(3),
        ..Default::default()
    });
    out.results.push(StatementResult {
        rows_affected: Some(7),
        ..Default::default()
    });
    out.messages.push("NOTICE: done".into());
    out.elapsed_ms = 42;
    out
}

/// Rows that don't compress: a different hash in each.
fn noisy(rows: usize) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    out.results.push(StatementResult {
        columns: vec![col("h", "text")],
        rows: (0..rows)
            .map(|i| {
                let h = Sha256::digest(i.to_le_bytes());
                vec![json!(h
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>())]
            })
            .collect(),
        total_rows: rows as u64,
        ..Default::default()
    });
    out
}

fn meta(id: &str, tab: &str, at: i64, pinned: bool) -> RunMeta {
    let mut extra = serde_json::Map::new();
    extra.insert("script".into(), json!("select * from t -- ñ"));
    extra.insert("connection_id".into(), json!("c1"));
    extra.insert("database".into(), json!("db"));
    RunMeta {
        id: id.into(),
        tab_id: tab.into(),
        at,
        pinned,
        extra,
    }
}

fn files_contain(dir: &Path, needle: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if std::fs::read(&p)
                .is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
            {
                found.push(p.display().to_string());
            }
        }
    }
    found
}

#[test]
fn helpers() {
    assert_eq!(lit("o'k"), "'o''k'");
    assert!(valid_key(&key('a')));
    assert!(!valid_key("short"));
    assert!(!valid_key(&key('z')));
    let k = new_key().unwrap();
    assert!(valid_key(&k));
    assert_ne!(k, new_key().unwrap());
    assert_eq!(
        redact(format!("ATTACH x (ENCRYPTION_KEY '{k}')"), &k),
        "ATTACH x (ENCRYPTION_KEY '…')"
    );
    assert!(table("run-1", 0).starts_with(&prefix("run-1")));
    assert_ne!(prefix("run-1"), prefix("run-2"));
    assert!(valid_id("a1-b2"));
    assert!(!valid_id(""));
    assert!(!valid_id("a\nb"));
    assert!(!valid_id(&"x".repeat(MAX_ID + 1)));
    // Every value comes back exactly as the grid had it.
    for v in [
        json!(1),
        json!(1.5),
        json!("x"),
        json!("1"),
        json!(true),
        json!({"a": [null]}),
        json!([]),
    ] {
        assert_eq!(from_stored(Value::String(v.to_string())), v);
    }
    assert_eq!(from_stored(Value::Null), Value::Null);
    let sql = settings_sql(Path::new("/tmp/r'x")).join(";");
    assert!(sql.contains("temp_file_encryption = true"));
    assert!(sql.contains("'/tmp/r''x/tmp'"));
    assert!(LOCK.contains(&"SET lock_configuration = true"));
    assert!(LOCK.contains(&"SET enable_external_access = false"));
    assert_eq!(
        attach_sql(Path::new("/d"), FILE, "store", &key('a')),
        format!(
            "ATTACH '/d/results.duckdb' AS store (ENCRYPTION_KEY '{}')",
            key('a')
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn saves_reads_and_encrypts() {
    let dir = temp_dir("roundtrip");
    let k = key('a');
    let mut s = open_at(&dir, &k).await.expect("store opens");
    let out = sample(2_500);
    let saved = s
        .save(
            meta("run-1", "tab-1", 10, false),
            out.clone(),
            DEFAULT_MAX_BYTES,
        )
        .await
        .unwrap();
    assert!(saved.stored);
    // Back exactly as it went in, rows and everything else.
    let back = s.outcome("run-1").await.unwrap().expect("stored");
    assert_eq!(
        serde_json::to_value(&back).unwrap(),
        serde_json::to_value(&out).unwrap()
    );
    let runs = s.runs("tab-1").await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(
        runs[0].extra.get("script"),
        Some(&json!("select * from t -- ñ"))
    );
    assert!(s.runs("tab-2").await.unwrap().is_empty());
    // Pin, run again in place.
    s.set_pinned("run-1", true).await.unwrap();
    assert!(s.runs("tab-1").await.unwrap()[0].pinned);
    let again = sample(3);
    s.save(
        meta("run-1", "tab-1", 11, true),
        again.clone(),
        DEFAULT_MAX_BYTES,
    )
    .await
    .unwrap();
    let back = s.outcome("run-1").await.unwrap().unwrap();
    assert_eq!(back.results[0].rows.len(), 3);
    assert_eq!(s.tables("r\\_").await.unwrap().len(), 1);
    // Nothing readable on disk: database, WAL, temp files.
    exec(s.session.as_mut(), "CHECKPOINT").await.unwrap();
    s.save(
        meta("run-2", "tab-1", 12, false),
        sample(500),
        DEFAULT_MAX_BYTES,
    )
    .await
    .unwrap();
    assert_eq!(files_contain(&dir, MARK), Vec::<String>::new());
    assert_eq!(files_contain(&dir, "ñandú"), Vec::<String>::new());
    let _ = s_close(s.session).await;
    // Opened again with the same key: still there.
    let mut s = open_at(&dir, &k).await.unwrap();
    assert_eq!(s.runs("tab-1").await.unwrap().len(), 2);
    let _ = s_close(s.session).await;
    // Another key (the file moved to another machine): it doesn't open, and
    // a new, empty store starts.
    let mut s = open_at(&dir, &key('b')).await.unwrap();
    assert!(s.runs("tab-1").await.unwrap().is_empty());
    let _ = s_close(s.session).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn half_written_runs_and_stray_tables_go_on_open() {
    let dir = temp_dir("half");
    let k = key('c');
    let mut s = open_at(&dir, &k).await.unwrap();
    s.save(meta("ok", "t", 1, false), sample(10), DEFAULT_MAX_BYTES)
        .await
        .unwrap();
    // A crash mid-write: the row says incomplete, a table is half there.
    exec(
        s.session.as_mut(),
        "INSERT INTO runs (id, tab_id, ran_at, pinned, meta, outcome, results, complete) VALUES ('cut', 't', 2, false, '{}', '{}', 1, false)",
    )
    .await
    .unwrap();
    exec(
        s.session.as_mut(),
        &format!("CREATE TABLE main.\"{}\" (_row BIGINT)", table("cut", 0)),
    )
    .await
    .unwrap();
    exec(
        s.session.as_mut(),
        "CREATE TABLE main.\"r_ffffffffffffffff_0\" (_row BIGINT)",
    )
    .await
    .unwrap();
    let _ = s_close(s.session).await;
    let mut s = open_at(&dir, &k).await.unwrap();
    let ids: Vec<String> = s
        .runs("t")
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, ["ok"]);
    assert_eq!(s.tables("r\\_").await.unwrap(), [table("ok", 0)]);
    let _ = s_close(s.session).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_space_cap_drops_the_oldest_unpinned_first() {
    let dir = temp_dir("cap");
    let mut s = open_at(&dir, &key('d')).await.unwrap();
    s.save(
        meta("pinned", "t", 1, true),
        sample(3_000),
        DEFAULT_MAX_BYTES,
    )
    .await
    .unwrap();
    s.save(meta("old", "t", 2, false), sample(3_000), DEFAULT_MAX_BYTES)
        .await
        .unwrap();
    s.save(meta("mid", "t", 3, false), sample(3_000), DEFAULT_MAX_BYTES)
        .await
        .unwrap();
    exec(s.session.as_mut(), "CHECKPOINT").await.unwrap();
    let full = s.bytes().await.unwrap();
    // Room for about one run less than what's there: "old" goes first.
    let cap = full - 1;
    let saved = s
        .save(meta("new", "t", 4, false), sample(3_000), cap)
        .await
        .unwrap();
    assert!(saved.stored);
    let ids: Vec<String> = s
        .runs("t")
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert!(ids.contains(&"pinned".to_string()), "{ids:?}");
    assert!(ids.contains(&"new".to_string()), "{ids:?}");
    assert!(!ids.contains(&"old".to_string()), "{ids:?}");
    // A run that can't fit even alone isn't kept; pinned ones never go.
    let saved = s
        .save(meta("huge", "t", 5, false), sample(3_000), 1)
        .await
        .unwrap();
    assert!(!saved.stored);
    let ids: Vec<String> = s
        .runs("t")
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert!(ids.contains(&"pinned".to_string()));
    assert!(!ids.contains(&"huge".to_string()));
    let _ = s_close(s.session).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_public_api_keeps_and_forgets_tabs() {
    let dir = temp_dir("api");
    let _ = DIR.set(dir.to_path_buf());
    let _ = TEST_KEY.set(key('e'));
    save(
        meta("a1", "tab-a", 1, false),
        sample(5),
        DEFAULT_MAX_BYTES,
        generation(),
    )
    .await
    .unwrap()
    .unwrap();
    save(
        meta("b1", "tab-b", 2, false),
        sample(5),
        DEFAULT_MAX_BYTES,
        generation(),
    )
    .await
    .unwrap()
    .unwrap();
    save(
        meta("c1", "tab-c", 3, false),
        sample(5),
        DEFAULT_MAX_BYTES,
        generation(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(status().await.unwrap().available);
    keep_tabs(vec!["tab-a".into(), "tab-b".into()])
        .await
        .unwrap();
    assert!(runs("tab-c".into()).await.unwrap().is_empty());
    delete_tab("tab-b".into()).await.unwrap();
    assert!(runs("tab-b".into()).await.unwrap().is_empty());
    assert_eq!(runs("tab-a".into()).await.unwrap().len(), 1);
    set_pinned("a1".into(), true).await.unwrap();
    assert!(runs("tab-a".into()).await.unwrap()[0].pinned);
    assert!(outcome("a1".into()).await.unwrap().is_some());
    delete(vec!["a1".into()]).await.unwrap();
    // Nothing stored any more: nothing on disk either.
    assert!(!dir.join(FILE).exists());
    assert!(outcome("a1".into()).await.unwrap().is_none());
    save(
        meta("d1", "tab-a", 4, false),
        sample(5),
        DEFAULT_MAX_BYTES,
        generation(),
    )
    .await
    .unwrap();
    clear().await.unwrap();
    assert!(!dir.join(FILE).exists());
    // A new store starts on the next use.
    assert!(runs("tab-a".into()).await.unwrap().is_empty());
    // A run that started before a clear isn't stored when it ends.
    let started = generation();
    clear().await.unwrap();
    let late = save(
        meta("late", "tab-a", 5, false),
        sample(5),
        DEFAULT_MAX_BYTES,
        started,
    )
    .await
    .unwrap();
    assert!(late.is_none());
    assert!(!dir.join(FILE).exists());
    // A managed (DBine Enterprise) connection's results never go to disk.
    let mut managed = meta("ent1", "tab-a", 6, false);
    managed
        .extra
        .insert("connection_id".into(), json!("ent-0001"));
    let kept = save(managed, sample(5), DEFAULT_MAX_BYTES, generation())
        .await
        .unwrap();
    assert!(kept.is_none());
    assert!(!dir.join(FILE).exists());
    clear().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mostly_free_file_is_compacted_when_it_opens() {
    let dir = temp_dir("compact");
    let k = key('f');
    let size = || {
        std::fs::metadata(dir.join(FILE))
            .map(|m| m.len())
            .unwrap_or(0)
    };
    let mut s = open_at(&dir, &k).await.unwrap();
    s.save(meta("big", "t", 1, false), noisy(60_000), DEFAULT_MAX_BYTES)
        .await
        .unwrap();
    let small = sample(3);
    s.save(
        meta("small", "t", 2, true),
        small.clone(),
        DEFAULT_MAX_BYTES,
    )
    .await
    .unwrap();
    exec(s.session.as_mut(), "CHECKPOINT").await.unwrap();
    let _ = s_close(s.session).await;
    let full = size();
    // Opened again with little free space: left as it is.
    let mut s = open_at(&dir, &k).await.unwrap();
    s.remove(&["big".to_string()]).await.unwrap();
    exec(s.session.as_mut(), "CHECKPOINT").await.unwrap();
    let _ = s_close(s.session).await;
    assert!(
        size() >= full / 2,
        "dropping a table doesn't shrink the file"
    );
    // Mostly free now: the next open rewrites it with what's left.
    let mut s = open_at(&dir, &k).await.unwrap();
    assert!(size() < full / 2, "{} not < {}", size(), full / 2);
    assert!(!dir.join(COMPACT_FILE).exists());
    let back = s.outcome("small").await.unwrap().expect("still stored");
    assert_eq!(
        serde_json::to_value(&back).unwrap(),
        serde_json::to_value(&small).unwrap()
    );
    assert!(s.runs("t").await.unwrap()[0].pinned);
    // Still encrypted, still locked down.
    assert_eq!(files_contain(&dir, MARK), Vec::<String>::new());
    let _ = s_close(s.session).await;
    let mut other = open_at(&dir, &key('g')).await.unwrap();
    assert!(other.runs("t").await.unwrap().is_empty());
    let _ = s_close(other.session).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_compaction_cut_short_leaves_the_store_whole() {
    let dir = temp_dir("cut");
    let k = key('h');
    let mut s = open_at(&dir, &k).await.unwrap();
    s.save(meta("kept", "t", 1, false), sample(10), DEFAULT_MAX_BYTES)
        .await
        .unwrap();
    let _ = s_close(s.session).await;
    // A crash mid-compaction: a half-written new file next to the store.
    std::fs::write(dir.join(COMPACT_FILE), b"half written").unwrap();
    let mut s = open_at(&dir, &k).await.unwrap();
    assert!(!dir.join(COMPACT_FILE).exists());
    assert_eq!(s.runs("t").await.unwrap().len(), 1);
    let _ = s_close(s.session).await;
}
