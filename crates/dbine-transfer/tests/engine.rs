//! The engine against an in-memory driver.

mod fake;

use dbine_driver::{Cell, Error, TransferColumn};
use dbine_transfer::{Bottleneck, CopyPath, Engine, Event, RunOptions, RunStatus, Store, TableStatus, CHANNEL_BATCHES};
use fake::{col, job, state_path, until, Events, Fake, FakeEndpoints, Fault, Native};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

fn opts(parallel: usize) -> RunOptions {
    RunOptions { parallel, backoff_ms: 10, ..Default::default() }
}

fn store(tag: &str) -> (Arc<Store>, std::path::PathBuf) {
    let p = state_path(tag);
    (Arc::new(Store::open(&p).unwrap()), p)
}

fn ids(rows: &[Vec<Cell>]) -> Vec<i64> {
    let mut v: Vec<i64> = rows.iter().map(|r| if let Cell::Int(i) = r[0] { i } else { -1 }).collect();
    v.sort();
    v
}

fn done_stats(events: &Events, table: &str) -> dbine_transfer::CopyStats {
    events
        .all()
        .into_iter()
        .find_map(|e| match e {
            Event::TableDone { table: t, stats, .. } if t == table => Some(stats),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{table} not done"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copies_every_table_with_bulk_load() {
    let fake = Arc::new(Fake::default());
    fake.table("a", 2_500);
    fake.table("b", 0);
    let (st, _) = store("basic");
    let ev = Events::default();
    let engine = Engine::new(st.clone(), "r1");
    let report = engine.run(vec![job("a", Some(2_500)), job("b", Some(0))], opts(8), FakeEndpoints::new(fake.clone()), ev.sink()).await.unwrap();
    assert_eq!(report.summary.status, RunStatus::Finished);
    assert_eq!(report.summary.done, 2);
    assert_eq!(fake.target_rows("a"), fake.source.lock().unwrap()["a"]);
    assert!(fake.target_rows("b").is_empty());
    assert_eq!(done_stats(&ev, "a").path, Some(CopyPath::BulkLoad));
    // Fast tables: only the final progress event each.
    let progress = ev.all().iter().filter(|e| matches!(e, Event::TableProgress { table, .. } if table == "a")).count();
    assert_eq!(progress, 1);
    let a = st.table("r1", "a").unwrap().unwrap();
    assert!(a.copied && a.status == TableStatus::Done && a.rows_done == 2_500 && a.attempts == 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_limit_changes_live() {
    let gate = Arc::new(Semaphore::new(0));
    let fake = Arc::new(Fake { gate: Some(gate.clone()), ..Default::default() });
    let names = ["t1", "t2", "t3", "t4", "t5", "t6"];
    for n in names {
        fake.table(n, 10);
    }
    let (st, _) = store("parallel");
    let engine = Engine::new(st.clone(), "r");
    let control = engine.control();
    let jobs = names.iter().map(|n| job(n, Some(10))).collect();
    let ep = FakeEndpoints::new(fake.clone());
    let run = tokio::spawn(async move { engine.run(jobs, opts(2), ep, |_| {}).await });

    let reading = || fake.reading.load(Ordering::SeqCst);
    until("2 running", || reading() == 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(reading(), 2);

    control.set_parallel(3);
    until("3 running", || reading() == 3).await;
    // Lowering doesn't stop running tables; raising again doesn't start a 4th.
    control.set_parallel(2);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(reading(), 3);
    control.set_parallel(3);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(reading(), 3, "a 4th table started");
    assert_eq!(fake.started.lock().unwrap().len(), 3);

    // One ends: the limit is 3, so one more starts.
    gate.add_permits(1);
    until("4th started", || fake.started.lock().unwrap().len() == 4).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(reading(), 3);

    gate.add_permits(100);
    let report = run.await.unwrap().unwrap();
    assert_eq!(report.summary.done, 6);
    assert_eq!(fake.max_reading.load(Ordering::SeqCst), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn largest_tables_first() {
    let fake = Arc::new(Fake::default());
    for n in ["a", "b", "c", "d"] {
        fake.table(n, 5);
    }
    let (st, _) = store("order");
    let jobs = vec![job("a", Some(10)), job("b", Some(300)), job("c", Some(50)), job("d", None)];
    Engine::new(st, "r").run(jobs, opts(1), FakeEndpoints::new(fake.clone()), |_| {}).await.unwrap();
    assert_eq!(*fake.started.lock().unwrap(), ["b", "c", "a", "d"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn memory_is_bounded_by_the_channel() {
    let fake = Arc::new(Fake { write_delay: Duration::from_millis(3), ..Default::default() });
    fake.table("wide", 80_000);
    let (st, _) = store("memory");
    let ev = Events::default();
    Engine::new(st, "r").run(vec![job("wide", None)], opts(1), FakeEndpoints::new(fake.clone()), ev.sink()).await.unwrap();
    let max = fake.max_window.load(Ordering::SeqCst);
    assert!(max <= CHANNEL_BATCHES, "{max} batches in flight");
    assert!(max >= CHANNEL_BATCHES - 2, "the window was never used: {max}");
    assert_eq!(fake.target_rows("wide").len(), 80_000);
    // The reader waited on the slow target.
    assert_eq!(done_stats(&ev, "wide").bottleneck, Some(Bottleneck::Destination));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copied_table_is_never_emptied_on_resume() {
    let fake = Arc::new(Fake::default());
    fake.table("t", 5_000);
    fake.sleep_post.store(true, Ordering::SeqCst);
    let (st, path) = store("copied");
    let mut j = job("t", Some(5_000));
    j.post = vec!["SLEEP t".into()];

    let engine = Engine::new(st.clone(), "r");
    let ep = FakeEndpoints::new(fake.clone());
    let run = tokio::spawn(async move { engine.run(vec![j], opts(1), ep, |_| {}).await });
    until("copied", || st.table("r", "t").unwrap().is_some_and(|t| t.copied)).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // The process dies while the indexes are built.
    run.abort();
    let _ = run.await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(st);

    let st = Arc::new(Store::open(&path).unwrap());
    assert_eq!(st.mark_running_as_interrupted().unwrap(), 1);
    assert_eq!(st.run("r").unwrap().unwrap().status, RunStatus::Interrupted);
    assert_eq!(st.table("r", "t").unwrap().unwrap().status, TableStatus::Copied);

    fake.sleep_post.store(false, Ordering::SeqCst);
    let report = Engine::new(st.clone(), "r").resume(None, FakeEndpoints::new(fake.clone()), |_| {}).await.unwrap();
    assert_eq!(report.summary.status, RunStatus::Finished);
    assert_eq!(fake.count("TRUNCATE t"), 0);
    assert_eq!(fake.count("SLEEP t"), 1);
    assert_eq!(fake.target_rows("t").len(), 5_000);
    assert_eq!(st.table("r", "t").unwrap().unwrap().status, TableStatus::Done);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn half_copied_table_is_emptied_and_copied_again_on_resume() {
    let fake = Arc::new(Fake { write_delay: Duration::from_millis(2), ..Default::default() });
    fake.table("t", 60_000);
    let (st, path) = store("half");
    let options = RunOptions { commit_rows: 1_000, ..opts(1) };

    let engine = Engine::new(st.clone(), "r");
    let ep = FakeEndpoints::new(fake.clone());
    let o = options.clone();
    let run = tokio::spawn(async move { engine.run(vec![job("t", Some(60_000))], o, ep, |_| {}).await });
    until("some rows committed", || fake.target_rows("t").len() >= 5_000).await;
    run.abort();
    let _ = run.await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let partial = fake.target_rows("t").len();
    assert!(partial > 0 && partial < 60_000);
    drop(st);

    // A new process.
    let st = Arc::new(Store::open(&path).unwrap());
    st.mark_running_as_interrupted().unwrap();
    let t = st.table("r", "t").unwrap().unwrap();
    assert!(t.status == TableStatus::Pending && !t.copied && t.attempts == 1);

    let fast = Arc::new(Fake::default());
    std::mem::swap(&mut *fast.source.lock().unwrap(), &mut *fake.source.lock().unwrap());
    std::mem::swap(&mut *fast.target.lock().unwrap(), &mut *fake.target.lock().unwrap());
    let report = Engine::new(st.clone(), "r").resume(None, FakeEndpoints::new(fast.clone()), |_| {}).await.unwrap();
    assert_eq!(report.summary.done, 1);
    assert_eq!(fast.count("TRUNCATE t"), 1);
    assert_eq!(ids(&fast.target_rows("t")), (0..60_000).collect::<Vec<_>>());
    let t = st.table("r", "t").unwrap().unwrap();
    assert_eq!((t.attempts, t.rows_done), (2, 60_000));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transient_error_is_retried_after_emptying() {
    let fake = Arc::new(Fake::default());
    fake.table("t", 5_000);
    fake.faults.lock().unwrap().push(Fault { table: "t".into(), after_rows: 1_500, error: || Error::Connect("connection reset by peer".into()), times: 1 });
    let (st, _) = store("transient");
    let ev = Events::default();
    let options = RunOptions { commit_rows: 1_000, ..opts(1) };
    let report = Engine::new(st.clone(), "r").run(vec![job("t", None)], options, FakeEndpoints::new(fake.clone()), ev.sink()).await.unwrap();
    assert_eq!(report.summary.done, 1);
    assert_eq!(fake.count("TRUNCATE t"), 1);
    assert_eq!(ids(&fake.target_rows("t")), (0..5_000).collect::<Vec<_>>());
    assert_eq!(st.table("r", "t").unwrap().unwrap().attempts, 2);
    assert!(ev.all().iter().any(|e| matches!(e, Event::Log { text, .. } if text.contains("Reintento 1 de 3"))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn other_errors_are_not_retried_and_retry_failed_recovers() {
    let fake = Arc::new(Fake::default());
    fake.table("t", 3_000);
    fake.faults.lock().unwrap().push(Fault {
        table: "t".into(),
        after_rows: 1_000,
        error: || Error::Query("Violation of PRIMARY KEY constraint".into()),
        times: 1,
    });
    let (st, _) = store("fatal");
    let options = RunOptions { commit_rows: 1_000, ..opts(1) };
    let engine = Engine::new(st.clone(), "r");
    let report = engine.run(vec![job("t", None)], options, FakeEndpoints::new(fake.clone()), |_| {}).await.unwrap();
    assert_eq!(report.summary.failed, 1);
    let t = st.table("r", "t").unwrap().unwrap();
    assert_eq!((t.status, t.attempts), (TableStatus::Failed, 1));
    assert!(t.error.unwrap().contains("PRIMARY KEY"));
    assert_eq!(fake.count("TRUNCATE t"), 0);
    assert_eq!(fake.target_rows("t").len(), 1_000);

    // "Reintentar las que fallaron": emptied, then copied whole.
    let report = engine.retry_failed(None, FakeEndpoints::new(fake.clone()), |_| {}).await.unwrap();
    assert_eq!(report.summary.done, 1);
    assert_eq!(fake.count("TRUNCATE t"), 1);
    assert_eq!(ids(&fake.target_rows("t")), (0..3_000).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn column_mismatch_is_reported_and_the_table_skipped() {
    let fake = Arc::new(Fake::default());
    fake.table("good", 10);
    fake.table("bad", 10);
    let mut extra = col("extra", "int");
    extra.nullable = false;
    fake.target_columns.lock().unwrap().insert("bad".into(), vec![col("ID", "int"), col("nombre", "text"), extra]);
    let expected = vec![
        TransferColumn { name: "id".into(), type_name: "int".into(), nullable: false },
        TransferColumn { name: "name".into(), type_name: "text".into(), nullable: true },
    ];
    let mut good = job("good", Some(2));
    good.expected_columns = expected.clone();
    let mut bad = job("bad", Some(1));
    bad.expected_columns = expected;
    let (st, _) = store("mismatch");
    let report = Engine::new(st.clone(), "r").run(vec![good, bad], opts(2), FakeEndpoints::new(fake.clone()), |_| {}).await.unwrap();
    assert_eq!((report.summary.done, report.summary.failed), (1, 1));
    let err = st.table("r", "bad").unwrap().unwrap().error.unwrap();
    assert!(err.contains("«name»") && err.contains("«extra»"), "{err}");
    assert!(!err.contains("«id»"), "names ignore case: {err}");
    assert_eq!(*fake.started.lock().unwrap(), ["good"]);
    assert_eq!(fake.bulk_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_one_table_leaves_the_others() {
    let gate = Arc::new(Semaphore::new(0));
    let fake = Arc::new(Fake { gate: Some(gate.clone()), ..Default::default() });
    fake.table("a", 10);
    fake.table("b", 10);
    let (st, _) = store("cancel");
    let ev = Events::default();
    let engine = Engine::new(st.clone(), "r");
    let control = engine.control();
    let ep = FakeEndpoints::new(fake.clone());
    let sink = ev.sink();
    let run = tokio::spawn(async move { engine.run(vec![job("a", Some(2)), job("b", Some(1))], opts(2), ep, sink).await });
    until("both running", || fake.reading.load(Ordering::SeqCst) == 2).await;
    assert!(control.cancel_table("a"));
    until("a cancelled", || st.table("r", "a").unwrap().unwrap().status == TableStatus::Cancelled).await;
    gate.add_permits(100);
    let report = run.await.unwrap().unwrap();
    assert_eq!((report.summary.done, report.summary.cancelled), (1, 1));
    // A copy had started: its rows (if any) are removed.
    assert_eq!(fake.count("TRUNCATE a"), 1);
    assert_eq!(fake.target_rows("b").len(), 10);
    assert!(ev.all().iter().any(|e| matches!(e, Event::TableCancelled { table } if table == "a")));
    assert!(!control.cancel_table("zzz"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_panic_fails_only_its_table() {
    let fake = Arc::new(Fake { panic_on: Some("boom".into()), ..Default::default() });
    fake.table("boom", 10);
    fake.table("ok", 10);
    let (st, _) = store("panic");
    let report = Engine::new(st.clone(), "r").run(vec![job("boom", None), job("ok", None)], opts(2), FakeEndpoints::new(fake.clone()), |_| {}).await.unwrap();
    assert_eq!((report.summary.done, report.summary.failed), (1, 1));
    let err = st.table("r", "boom").unwrap().unwrap().error.unwrap();
    assert!(err.contains("error inesperado") && err.contains("boom en boom"), "{err}");
    assert_eq!(fake.target_rows("ok").len(), 10);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn insert_script_when_there_is_no_bulk_load() {
    let fake = Arc::new(Fake { bulk: false, ..Default::default() });
    fake.table("t", 2_500);
    let mut j = job("t", None);
    j.before = "BEFORE t".into();
    j.after = "AFTER t".into();
    let (st, _) = store("insert");
    let ev = Events::default();
    Engine::new(st, "r").run(vec![j], opts(1), FakeEndpoints::new(fake.clone()), ev.sink()).await.unwrap();
    assert_eq!(fake.bulk_calls.load(Ordering::SeqCst), 0);
    assert_eq!(fake.target_rows("t"), fake.source.lock().unwrap()["t"]);
    let log: Vec<String> = fake.log.lock().unwrap().iter().filter(|s| !s.starts_with("PROBE")).cloned().collect();
    assert_eq!(log, ["BEFORE t", "AFTER t"]);
    assert_eq!(done_stats(&ev, "t").path, Some(CopyPath::InsertScript));
}

/// The fix-ups after the load (sequence resync, identity restart) run on
/// the bulk-load and native-copy paths too; the `before` only belongs to
/// the `insert_script` path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn after_statements_run_on_every_path() {
    for (native, allowed) in [(Native::Off, false), (Native::On, true)] {
        let fake = Arc::new(Fake { native, native_allowed: allowed, ..Default::default() });
        fake.table("t", 1_200);
        let mut j = job("t", None);
        j.before = "BEFORE t".into();
        j.after = "AFTER t".into();
        let (st, _) = store(if allowed { "after-native" } else { "after-bulk" });
        Engine::new(st, "r").run(vec![j], opts(1), FakeEndpoints::new(fake.clone()), |_| {}).await.unwrap();
        let log: Vec<String> = fake.log.lock().unwrap().iter().filter(|s| !s.starts_with("PROBE")).cloned().collect();
        assert_eq!(log, ["AFTER t"], "native={allowed}");
        assert_eq!(fake.target_rows("t").len(), 1_200);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_copy_when_allowed() {
    let fake = Arc::new(Fake { native: Native::On, native_allowed: true, ..Default::default() });
    fake.table("t", 1_234);
    let (st, _) = store("native");
    let ev = Events::default();
    Engine::new(st, "r").run(vec![job("t", None)], opts(1), FakeEndpoints::new(fake.clone()), ev.sink()).await.unwrap();
    assert_eq!(fake.native_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fake.bulk_calls.load(Ordering::SeqCst), 0);
    assert_eq!(fake.target_rows("t").len(), 1_234);
    assert_eq!(done_stats(&ev, "t").path, Some(CopyPath::Native));

    // Not allowed by the app: batches.
    let fake = Arc::new(Fake { native: Native::On, native_allowed: false, ..Default::default() });
    fake.table("t", 10);
    let (st, _) = store("native-off");
    Engine::new(st, "r").run(vec![job("t", None)], opts(1), FakeEndpoints::new(fake.clone()), |_| {}).await.unwrap();
    assert_eq!(fake.native_calls.load(Ordering::SeqCst), 0);
    assert_eq!(fake.bulk_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_copy_falls_back_on_unsupported() {
    let fake = Arc::new(Fake { native: Native::Unsupported, native_allowed: true, ..Default::default() });
    fake.table("t", 1_500);
    let (st, _) = store("native-fallback");
    let ev = Events::default();
    Engine::new(st, "r").run(vec![job("t", None)], opts(1), FakeEndpoints::new(fake.clone()), ev.sink()).await.unwrap();
    assert_eq!(fake.bulk_calls.load(Ordering::SeqCst), 1);
    assert_eq!(fake.target_rows("t").len(), 1_500);
    assert_eq!(done_stats(&ev, "t").path, Some(CopyPath::BulkLoad));
    assert!(ev.all().iter().any(|e| matches!(e, Event::Log { text, .. } if text.contains("sin copia directa"))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preexisting_rows_are_never_overwritten_by_default() {
    let fake = Arc::new(Fake::default());
    fake.table("keep", 10);
    fake.table("empty", 10);
    for t in ["keep", "empty"] {
        fake.target.lock().unwrap().insert(t.into(), vec![vec![Cell::Int(-1), Cell::Null]]);
    }
    let mut keep = job("keep", None);
    keep.preexisting = true;
    let mut empty = job("empty", None);
    empty.preexisting = true;
    empty.empty_first = true;
    let (st, _) = store("preexisting");
    let report = Engine::new(st.clone(), "r").run(vec![keep, empty], opts(2), FakeEndpoints::new(fake.clone()), |_| {}).await.unwrap();
    assert_eq!((report.summary.done, report.summary.failed), (1, 1));
    assert!(st.table("r", "keep").unwrap().unwrap().error.unwrap().contains("ya tiene filas"));
    assert_eq!(fake.target_rows("keep").len(), 1);
    assert_eq!(ids(&fake.target_rows("empty")), (0..10).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn live_queue_and_run_now() {
    let gate = Arc::new(Semaphore::new(0));
    let fake = Arc::new(Fake { gate: Some(gate.clone()), ..Default::default() });
    for n in ["a", "b", "c"] {
        fake.table(n, 10);
    }
    let (st, _) = store("queue");
    let engine = Engine::new(st.clone(), "r");
    let control = engine.control();
    let ep = FakeEndpoints::new(fake.clone());
    let run = tokio::spawn(async move { engine.run(vec![job("a", None)], opts(1), ep, |_| {}).await });
    until("a running", || fake.reading.load(Ordering::SeqCst) == 1).await;
    assert!(control.enqueue(job("b", None)).unwrap());
    assert!(control.enqueue(job("c", None)).unwrap());
    assert_eq!(st.table("r", "c").unwrap().unwrap().status, TableStatus::Pending);
    // "c" jumps the limit of 1.
    assert!(control.run_now("c"));
    until("c running", || fake.reading.load(Ordering::SeqCst) == 2).await;
    assert_eq!(*fake.started.lock().unwrap(), ["a", "c"]);
    gate.add_permits(100);
    let report = run.await.unwrap().unwrap();
    assert_eq!(report.summary.done, 3);
    // The queued tables are part of the run (a resume knows them).
    assert_eq!(st.run_spec("r").unwrap().unwrap().jobs.len(), 3);
    assert!(!control.enqueue(job("late", None)).unwrap(), "the run ended");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_all_keeps_queued_tables_pending_for_resume() {
    let gate = Arc::new(Semaphore::new(0));
    let fake = Arc::new(Fake { gate: Some(gate.clone()), ..Default::default() });
    for n in ["a", "b"] {
        fake.table(n, 10);
    }
    let (st, _) = store("cancel-all");
    let engine = Engine::new(st.clone(), "r");
    let control = engine.control();
    let ep = FakeEndpoints::new(fake.clone());
    let run = tokio::spawn(async move { engine.run(vec![job("a", Some(2)), job("b", Some(1))], opts(1), ep, |_| {}).await });
    until("a running", || fake.reading.load(Ordering::SeqCst) == 1).await;
    control.cancel_all();
    let report = run.await.unwrap().unwrap();
    assert_eq!(report.summary.status, RunStatus::Cancelled);
    assert_eq!(st.table("r", "b").unwrap().unwrap().status, TableStatus::Pending);

    gate.add_permits(100);
    let report = Engine::new(st.clone(), "r").resume(None, FakeEndpoints::new(fake.clone()), |_| {}).await.unwrap();
    assert_eq!(report.summary.done, 2);
    assert_eq!(ids(&fake.target_rows("a")), (0..10).collect::<Vec<_>>());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_table_waiting_for_a_slot_can_be_cancelled_or_run_now() {
    let gate = Arc::new(Semaphore::new(0));
    let fake = Arc::new(Fake { gate: Some(gate.clone()), ..Default::default() });
    for n in ["a", "b", "c"] {
        fake.table(n, 10);
    }
    let (st, _) = store("head-of-queue");
    let engine = Engine::new(st.clone(), "r");
    let control = engine.control();
    let ep = FakeEndpoints::new(fake.clone());
    let jobs = vec![job("a", Some(3)), job("b", Some(2)), job("c", Some(1))];
    let run = tokio::spawn(async move { engine.run(jobs, opts(1), ep, |_| {}).await });
    until("a running", || fake.reading.load(Ordering::SeqCst) == 1).await;
    // Let the dispatcher reach its wait for a slot, with "b" at the head.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(control.run_now("b"), "the head of the queue is not found");
    until("b running", || fake.reading.load(Ordering::SeqCst) == 2).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(control.cancel_table("c"), "the head of the queue is not found");
    assert_eq!(st.table("r", "c").unwrap().unwrap().status, TableStatus::Cancelled);
    gate.add_permits(100);
    let report = run.await.unwrap().unwrap();
    assert_eq!((report.summary.done, report.summary.cancelled), (2, 1));
    assert_eq!(*fake.started.lock().unwrap(), ["a", "b"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reused_engine_does_not_queue_leftovers_twice() {
    let gate = Arc::new(Semaphore::new(0));
    let fake = Arc::new(Fake { gate: Some(gate.clone()), ..Default::default() });
    for n in ["a", "b", "c"] {
        fake.table(n, 10);
    }
    fake.faults.lock().unwrap().push(Fault { table: "b".into(), after_rows: 0, error: || Error::Query("Violation of PRIMARY KEY constraint".into()), times: 1 });
    let (st, _) = store("reuse");
    let engine = Arc::new(Engine::new(st.clone(), "r"));
    let control = engine.control();
    let (e, ep) = (engine.clone(), FakeEndpoints::new(fake.clone()));
    let jobs = vec![job("a", Some(3)), job("b", Some(2)), job("c", Some(1))];
    let run = tokio::spawn(async move { e.run(jobs, opts(1), ep, |_| {}).await });
    until("a running", || fake.reading.load(Ordering::SeqCst) == 1).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    control.cancel_all();
    assert_eq!(run.await.unwrap().unwrap().summary.status, RunStatus::Cancelled);

    // The same engine: "b" fails once and must stay failed (no duplicate
    // queued behind it copies it again).
    gate.add_permits(100);
    let report = engine.resume(None, FakeEndpoints::new(fake.clone()), |_| {}).await.unwrap();
    assert_eq!((report.summary.done, report.summary.failed), (2, 1));
    assert_eq!(st.table("r", "b").unwrap().unwrap().status, TableStatus::Failed);
    let started = fake.started.lock().unwrap().clone();
    assert_eq!(started.iter().filter(|n| *n == "b").count(), 1, "{started:?}");
    assert_eq!(started.iter().filter(|n| *n == "c").count(), 1, "{started:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rows_in_a_created_table_are_emptied_even_if_the_state_lost_them() {
    let fake = Arc::new(Fake::default());
    fake.table("t", 10);
    // A copy committed rows, then a power cut dropped `attempts` from the
    // state: it says nothing was copied.
    fake.target.lock().unwrap().insert("t".into(), vec![vec![Cell::Int(3), Cell::Null], vec![Cell::Int(4), Cell::Null]]);
    let (st, _) = store("lost-state");
    let report = Engine::new(st.clone(), "r").run(vec![job("t", None)], opts(1), FakeEndpoints::new(fake.clone()), |_| {}).await.unwrap();
    assert_eq!(report.summary.done, 1);
    assert_eq!(fake.count("TRUNCATE t"), 1);
    assert_eq!(ids(&fake.target_rows("t")), (0..10).collect::<Vec<_>>());
}
