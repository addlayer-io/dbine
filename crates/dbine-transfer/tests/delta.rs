//! Sync by rows against the in-memory driver.

mod fake;

use dbine_driver::{Buckets, Cell, DeltaResult, Error};
use dbine_transfer::{CopyPath, CopyStats, Engine, Event, Phase, RunOptions, RunReport, Store, TableStatus};
use fake::{delta_job, state_path, Events, Fake, FakeEndpoints, Fault};
use std::sync::Arc;

fn opts() -> RunOptions {
    RunOptions { parallel: 2, backoff_ms: 10, ..Default::default() }
}

fn store(tag: &str) -> (Arc<Store>, std::path::PathBuf) {
    let p = state_path(tag);
    (Arc::new(Store::open(&p).unwrap()), p)
}

fn done_stats(events: &Events, table: &str) -> CopyStats {
    events
        .all()
        .into_iter()
        .find_map(|e| match e {
            Event::TableDone { table: t, stats, .. } if t == table => Some(stats),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{table} not done"))
}

fn error_of(report: &RunReport, table: &str) -> String {
    report.tables.iter().find(|t| t.name == table).and_then(|t| t.error.clone()).unwrap_or_default()
}

/// Set the name of the row with this id, on one side.
fn rename(side: &std::sync::Mutex<std::collections::HashMap<String, fake::Table>>, table: &str, id: i64, name: &str) {
    let mut s = side.lock().unwrap();
    let row = s.get_mut(table).unwrap().iter_mut().find(|r| r[0] == Cell::Int(id)).unwrap();
    row[1] = Cell::Text(name.into());
}

fn remove(side: &std::sync::Mutex<std::collections::HashMap<String, fake::Table>>, table: &str, id: i64) {
    side.lock().unwrap().get_mut(table).unwrap().retain(|r| r[0] != Cell::Int(id));
}

fn add(side: &std::sync::Mutex<std::collections::HashMap<String, fake::Table>>, table: &str, id: i64, name: &str) {
    side.lock().unwrap().get_mut(table).unwrap().push(vec![Cell::Int(id), Cell::Text(name.into())]);
}

fn result(inserted: u64, updated: u64, deleted: u64) -> Option<DeltaResult> {
    Some(DeltaResult { inserted, updated, deleted, ..Default::default() })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_changed_nothing_applied() {
    let fake = Arc::new(Fake::default());
    fake.table("t", 10_000);
    fake.same_on_target("t");
    let (st, _) = store("delta-same");
    let ev = Events::default();
    let report = Engine::new(st.clone(), "r").run(vec![delta_job("t", "id", Some(10_000))], opts(), FakeEndpoints::new(fake.clone()), ev.sink()).await.unwrap();
    assert_eq!(report.summary.done, 1);
    assert!(fake.delta_applies.lock().unwrap().is_empty());
    assert!(fake.filters.lock().unwrap().is_empty(), "the source was read");
    let stats = done_stats(&ev, "t");
    assert_eq!((stats.path, stats.rows, stats.delta), (Some(CopyPath::Delta), 0, result(0, 0, 0)));
    // Both sides summarized, with range buckets on the integer key.
    let sums = fake.summaries.lock().unwrap();
    assert_eq!(sums.len(), 2);
    assert!(matches!(sums[0], Buckets::Range { lo: 0, hi: 9_999, width: 1_000, n: 10, .. }), "{:?}", sums[0]);
    let phases: Vec<Phase> = ev.all().into_iter().filter_map(|e| if let Event::TablePhase { phase, .. } = e { Some(phase) } else { None }).collect();
    assert_eq!(phases, [Phase::Summary, Phase::Compare]);
    let t = st.table("r", "t").unwrap().unwrap();
    assert!(t.delta && t.status == TableStatus::Done);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changes_on_either_side_are_fixed_by_range() {
    let fake = Arc::new(Fake::default());
    fake.table("t", 10_000);
    fake.same_on_target("t");
    // Source: an update, an insert past the end, a delete.
    rename(&fake.source, "t", 100, "changed at the source");
    add(&fake.source, "t", 10_000, "new");
    remove(&fake.source, "t", 200);
    // Target: a missing row, a changed one, rows only there (both edges).
    remove(&fake.target, "t", 5);
    rename(&fake.target, "t", 7, "changed at the target");
    add(&fake.target, "t", 50_000, "only on the target");
    add(&fake.target, "t", -3, "only on the target");

    let (st, _) = store("delta-range");
    let ev = Events::default();
    // "Vaciar y copiar" means nothing to a sync: it's never emptied.
    let mut j = delta_job("t", "id", Some(10_000));
    j.empty_first = true;
    let report = Engine::new(st.clone(), "r").run(vec![j], opts(), FakeEndpoints::new(fake.clone()), ev.sink()).await.unwrap();
    assert_eq!(report.summary.done, 1, "{:?}", report.tables);
    assert!(fake.in_sync("t"));
    assert_eq!(fake.count("TRUNCATE t"), 0);
    // Width 1001 over 0..=10 000: buckets 0 and 9 plus both edges.
    assert_eq!(*fake.delta_applies.lock().unwrap(), [vec![-1, 0, 9, 10]]);
    assert!(fake.filters.lock().unwrap()[0].is_some());
    let stats = done_stats(&ev, "t");
    assert_eq!(stats.delta, result(2, 2, 3));
    // Rows reviewed: the source rows of the changed buckets.
    assert_eq!(stats.rows, 1_000 + 992);
    let t = st.table("r", "t").unwrap().unwrap();
    assert_eq!(t.stats.and_then(|s| s.delta), result(2, 2, 3), "persisted");
    let progress = ev.all().into_iter().rev().find_map(|e| if let Event::TableProgress { rows_done, rows_total, .. } = e { Some((rows_done, rows_total)) } else { None });
    assert_eq!(progress, Some((1_992, Some(1_992))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn text_key_uses_prime_hash_buckets() {
    let fake = Arc::new(Fake::default());
    fake.table("t", 10_000);
    fake.same_on_target("t");
    remove(&fake.target, "t", 5);
    // Same key ("row 7"), another id: an update.
    fake.target.lock().unwrap().get_mut("t").unwrap().iter_mut().find(|r| r[0] == Cell::Int(7)).unwrap()[0] = Cell::Int(99_999);
    add(&fake.target, "t", 123_456, "extra");

    let (st, _) = store("delta-hash");
    let ev = Events::default();
    let report = Engine::new(st, "r").run(vec![delta_job("t", "name", Some(10_000))], opts(), FakeEndpoints::new(fake.clone()), ev.sink()).await.unwrap();
    assert_eq!(report.summary.done, 1, "{:?}", report.tables);
    assert!(fake.in_sync("t"));
    assert!(matches!(fake.summaries.lock().unwrap()[0], Buckets::Hash { n: 17 }));
    let applies = fake.delta_applies.lock().unwrap();
    assert!(!applies[0].is_empty() && applies[0].len() <= 3, "{applies:?}");
    assert_eq!(done_stats(&ev, "t").delta, result(1, 1, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_changed_buckets_apply_the_whole_table() {
    let fake = Arc::new(Fake::default());
    fake.table("t", 10_000);
    fake.same_on_target("t");
    // 7 of 12 buckets (10 plus the edges) differ: more than half.
    for i in 0..7 {
        rename(&fake.target, "t", i * 1_000, "stale");
    }
    let (st, _) = store("delta-whole");
    let ev = Events::default();
    Engine::new(st, "r").run(vec![delta_job("t", "id", Some(10_000))], opts(), FakeEndpoints::new(fake.clone()), ev.sink()).await.unwrap();
    assert!(fake.in_sync("t"));
    // Empty bucket list: the whole table; the source read unfiltered.
    assert_eq!(*fake.delta_applies.lock().unwrap(), [Vec::<i64>::new()]);
    assert_eq!(*fake.filters.lock().unwrap(), [None]);
    let stats = done_stats(&ev, "t");
    assert_eq!((stats.rows, stats.delta), (10_000, result(0, 7, 0)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_sync_runs_again_and_is_never_emptied() {
    let fake = Arc::new(Fake::default());
    fake.table("t", 3_000);
    fake.same_on_target("t");
    remove(&fake.target, "t", 10);
    fake.faults.lock().unwrap().push(Fault { table: "t".into(), after_rows: 0, error: || Error::Query("constraint violated".into()), times: 1 });
    let (st, path) = store("delta-resume");
    let report = Engine::new(st.clone(), "r").run(vec![delta_job("t", "id", Some(3_000))], opts(), FakeEndpoints::new(fake.clone()), |_| {}).await.unwrap();
    assert_eq!(report.summary.failed, 1);
    let t = st.table("r", "t").unwrap().unwrap();
    assert!(t.delta && !t.copied && t.attempts == 1);
    // Rolled back: the target is as it was.
    assert_eq!(fake.target_rows("t").len(), 2_999);

    // A new process resumes it: synced again, never emptied.
    drop(st);
    let st = Arc::new(Store::open(&path).unwrap());
    let report = Engine::new(st.clone(), "r").resume(None, FakeEndpoints::new(fake.clone()), |_| {}).await.unwrap();
    assert_eq!(report.summary.done, 1, "{:?}", report.tables);
    assert!(fake.in_sync("t"));
    assert_eq!(fake.count("TRUNCATE t"), 0);
    let t = st.table("r", "t").unwrap().unwrap();
    assert_eq!((t.attempts, t.status), (2, TableStatus::Done));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_transient_failure_is_retried_without_emptying() {
    let fake = Arc::new(Fake::default());
    fake.table("t", 3_000);
    fake.same_on_target("t");
    rename(&fake.target, "t", 1, "stale");
    fake.faults.lock().unwrap().push(Fault { table: "t".into(), after_rows: 0, error: || Error::Connect("connection reset".into()), times: 1 });
    let (st, _) = store("delta-retry");
    let ev = Events::default();
    let report = Engine::new(st.clone(), "r").run(vec![delta_job("t", "id", Some(3_000))], opts(), FakeEndpoints::new(fake.clone()), ev.sink()).await.unwrap();
    assert_eq!(report.summary.done, 1);
    assert!(fake.in_sync("t"));
    assert_eq!(fake.count("TRUNCATE t"), 0);
    assert_eq!(fake.delta_applies.lock().unwrap().len(), 2);
    assert_eq!(done_stats(&ev, "t").delta, result(0, 1, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsupported_or_different_engines_fail_clearly() {
    let fake = Arc::new(Fake { delta: false, ..Default::default() });
    fake.table("t", 100);
    let (st, _) = store("delta-unsupported");
    let report = Engine::new(st, "r").run(vec![delta_job("t", "id", None)], opts(), FakeEndpoints::new(fake.clone()), |_| {}).await.unwrap();
    assert_eq!(report.summary.failed, 1);
    assert!(error_of(&report, "t").contains("no sincroniza por filas"), "{}", error_of(&report, "t"));
    assert!(fake.summaries.lock().unwrap().is_empty() && fake.target_rows("t").is_empty());

    let fake = Arc::new(Fake { target_id: "other", ..Default::default() });
    fake.table("t", 100);
    let (st, _) = store("delta-other");
    let report = Engine::new(st, "r").run(vec![delta_job("t", "id", None)], opts(), FakeEndpoints::new(fake.clone()), |_| {}).await.unwrap();
    assert_eq!(report.summary.failed, 1);
    assert!(error_of(&report, "t").contains("mismo motor"), "{}", error_of(&report, "t"));
    assert!(fake.summaries.lock().unwrap().is_empty());
}
