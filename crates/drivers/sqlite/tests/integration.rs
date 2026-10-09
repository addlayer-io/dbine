//! End-to-end through the public API on a real database file (no server
//! needed, so these run by default).

use dbine_driver::{kinds, ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};
use std::time::Duration;

fn cfg(path: &std::path::Path, read_only: bool) -> ConnectionConfig {
    ConnectionConfig { driver: "sqlite".into(), host: path.display().to_string(), read_only, ..Default::default() }
}

async fn open(path: &std::path::Path, read_only: bool) -> Box<dyn Session> {
    let d = dbine_driver_sqlite::drivers().pop().unwrap();
    d.connect(&cfg(path, read_only), None).await.unwrap()
}

fn temp_db(tag: &str) -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("dbine-sqlite-{tag}-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&p);
    p
}

#[tokio::test]
async fn full_round_trip() {
    let path = temp_db("rt");
    let mut s = open(&path, false).await;
    assert!(s.server_version().await.unwrap().starts_with("SQLite"));
    assert_eq!(s.list_databases().await.unwrap(), vec!["main"]);

    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL DEFAULT 'x');
         CREATE VIEW v AS SELECT * FROM t;
         CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT 1; END;
         INSERT INTO t (name) VALUES ('a'), ('b'), ('c');",
        100,
        &mut out,
    )
    .await
    .unwrap();

    let objs = s.list_objects().await.unwrap();
    let kinds_of = |n: &str| objs.iter().find(|o| o.name == n).map(|o| o.kind.clone());
    assert_eq!(kinds_of("t").as_deref(), Some(kinds::TABLE));
    assert_eq!(kinds_of("v").as_deref(), Some(kinds::VIEW));
    assert_eq!(kinds_of("tr").as_deref(), Some(kinds::TRIGGER));

    let t = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "t".into() };
    let cols = s.columns(&t).await.unwrap();
    assert_eq!(cols.len(), 2);
    assert!(cols[0].primary_key && cols[0].auto_increment);
    assert!(!cols[1].nullable);
    assert!(s.definition(&t).await.unwrap().unwrap().starts_with("CREATE TABLE"));

    let q = s.browse_query(&t, 2);
    let mut out = QueryOutcome::default();
    s.execute(&q, 100, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2);

    // max_rows truncation.
    let mut out = QueryOutcome::default();
    s.execute("SELECT * FROM t", 1, &mut out).await.unwrap();
    assert!(out.results[0].truncated);
    assert_eq!(out.results[0].total_rows, 3);

    // An error in the middle keeps what ran before.
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT 1; SELECT * FROM nope; SELECT 2", 10, &mut out).await.unwrap_err();
    assert!(e.is_query(), "{e:?}");
    assert_eq!(out.results.len(), 1);

    // Cancel a long query from another thread.
    let stop = s.interrupter().unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        stop();
    });
    let mut out = QueryOutcome::default();
    let e = s
        .execute(
            "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c",
            10,
            &mut out,
        )
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Cancelled), "{e:?}");
    // No empty grid next to the cancel.
    assert!(out.results.is_empty(), "{:?}", out.results);

    // Manual mode: VACUUM runs (no transaction opened for it); going back
    // to Auto commits what's open, so a later BEGIN works.
    use dbine_driver::TxState;
    s.set_autocommit(false).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("VACUUM", 10, &mut out).await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    s.execute("INSERT INTO t (name) VALUES ('manual')", 10, &mut out).await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Open));
    s.set_autocommit(true).await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    s.execute("BEGIN; DELETE FROM t WHERE name = 'manual'; COMMIT", 10, &mut out).await.unwrap();
    // A write that fails in Manual mode leaves no empty transaction open.
    s.set_autocommit(false).await.unwrap();
    assert!(s.execute("INSERT INTO nope VALUES (1)", 10, &mut out).await.is_err());
    let mut one = QueryOutcome::default();
    s.execute("SELECT min(id) FROM t", 10, &mut one).await.unwrap();
    let id = one.results[0].rows[0][0].clone();
    let e = s.execute(&format!("INSERT INTO t (id, name) VALUES ({id}, 'dup')"), 10, &mut out).await.unwrap_err();
    assert!(e.to_string().contains("UNIQUE"), "{e}");
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    s.set_autocommit(true).await.unwrap();
    drop(s);

    // Read-only: the file is opened read-only, so writes fail at the engine.
    let mut ro = open(&path, true).await;
    let mut out = QueryOutcome::default();
    assert!(ro.execute("INSERT INTO t (name) VALUES ('z')", 10, &mut out).await.is_err());
    drop(ro);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn plans() {
    let path = temp_db("plan");
    let mut s = open(&path, false).await;
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE TABLE a (id INTEGER PRIMARY KEY, g INT); CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INT);
         INSERT INTO a VALUES (1, 1), (2, 2); INSERT INTO b VALUES (1, 1), (2, 2), (3, 2);",
        10,
        &mut out,
    )
    .await
    .unwrap();

    // Estimated: nothing runs.
    let mut out = QueryOutcome::default();
    s.explain("SELECT * FROM a JOIN b ON b.a_id = a.id ORDER BY a.g; DELETE FROM b; CREATE TABLE c (x)", false, 10, &mut out)
        .await
        .unwrap();
    assert_eq!(out.plans.len(), 2);
    assert!(out.results.is_empty());
    assert_eq!(out.messages.len(), 1, "{:?}", out.messages);
    assert_eq!(out.plans[0].root.op, "QUERY PLAN");
    assert!(!out.plans[0].root.children.is_empty());
    let mut check = QueryOutcome::default();
    s.execute("SELECT count(*) FROM b", 10, &mut check).await.unwrap();
    assert_eq!(check.results[0].rows[0][0], serde_json::json!(3));

    // With analyze the script runs once; plans stay estimated.
    let mut out = QueryOutcome::default();
    s.explain("DELETE FROM b WHERE id = 1; SELECT count(*) FROM b", true, 10, &mut out).await.unwrap();
    assert_eq!(out.plans.len(), 2);
    assert!(out.plans.iter().all(|p| !p.actual));
    assert_eq!(out.results[1].rows[0][0], serde_json::json!(2));

    let mut out = QueryOutcome::default();
    assert!(s.explain("SELECT 1; SELECT * FROM missing", true, 10, &mut out).await.is_err());
    assert_eq!(out.results.len(), 1);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn schema_and_insert_script() {
    let path = temp_db("schema");
    let d = dbine_driver_sqlite::drivers().pop().unwrap();
    let mut s = d.connect(&cfg(&path, false), None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE TABLE a (id INTEGER PRIMARY KEY AUTOINCREMENT, n TEXT NOT NULL, ok BOOLEAN);
         CREATE TABLE b (id INTEGER PRIMARY KEY, a_id INTEGER REFERENCES a (id) ON DELETE CASCADE);
         CREATE INDEX b_a ON b (a_id);",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let schema = s.database_schema().await.unwrap();
    assert_eq!(schema.len(), 2);
    assert_eq!(schema[1].foreign_keys[0].on_delete.as_deref(), Some("CASCADE"));
    assert_eq!(schema[1].indexes[0].name, "b_a");
    assert!(!d.capabilities().create_database && d.capabilities().foreign_keys);

    let target = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "a".into() };
    let ins = d
        .insert_script(&target, &["n".into(), "ok".into()], &[vec!["O'Brien".into(), true.into()], vec!["x".into(), serde_json::Value::Null]])
        .unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&ins, 10, &mut out).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("SELECT n, ok FROM a ORDER BY id", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0], vec![serde_json::json!("O'Brien"), serde_json::json!(1)]);
    drop(s);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn monitor() {
    let path = temp_db("mon");
    let d = dbine_driver_sqlite::drivers().pop().unwrap();
    assert!(d.capabilities().monitor);
    let mut s = open(&path, false).await;
    let mut out = QueryOutcome::default();
    s.execute(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000)
         INSERT INTO t (v) SELECT hex(randomblob(50)) FROM n;
         SELECT COUNT(*) FROM t;",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let snap = s.monitor().await.unwrap();
    for m in &snap.metrics {
        eprintln!("{:<14} {:<44} {:?} max={:?}", m.key, m.label, m.value, m.max);
    }
    eprintln!("{:?}\n{:?}\n{:?}", snap.info, snap.tables, snap.notes);
    let v = |k: &str| snap.metrics.iter().find(|m| m.key == k).and_then(|m| m.value);
    assert!(v("file_size").is_some() && v("storage_used").unwrap() > 100_000.0);
    assert!(v("wal_size").unwrap() > 0.0, "WAL mode leaves a -wal file");
    assert!(v("mem_used").unwrap() > 0.0 && v("cache_size").is_some());
    assert!(snap.info.iter().any(|(k, v)| k.starts_with("Modo del diario") && v == "wal"));
    let top = snap.tables.iter().find(|t| t.key == "top_objects").expect("dbstat");
    assert_eq!(top.rows[0][0], serde_json::json!("t"));
    assert!(snap.tables.iter().any(|t| t.key == "databases" && t.rows.len() == 1));
    drop(s);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn run_read_only_reads_and_refuses_the_rest() {
    let path = temp_db("ro");
    let mut s = open(&path, false).await;
    let mut out = QueryOutcome::default();
    s.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, n TEXT); INSERT INTO t (n) VALUES ('a'), ('b'), ('c');", 100, &mut out).await.unwrap();

    let mut out = QueryOutcome::default();
    s.run_read_only("SELECT id, n FROM t ORDER BY id", 2, &mut out).await.unwrap();
    let r = &out.results[0];
    assert_eq!(r.rows, vec![vec![serde_json::json!(1), "a".into()], vec![serde_json::json!(2), "b".into()]]);
    assert_eq!((r.total_rows, r.truncated), (3, true));

    let other = temp_db("ro-attached");
    for bad in [
        "INSERT INTO t (n) VALUES ('x')".to_string(),
        "UPDATE t SET n = 'x'".into(),
        "DELETE FROM t".into(),
        "CREATE TABLE u (a)".into(),
        "DROP TABLE t".into(),
        format!("ATTACH DATABASE '{}' AS x", other.display()),
        "PRAGMA writable_schema = 1".into(),
        "PRAGMA query_only = OFF".into(),
        "SELECT load_extension('nothing')".into(),
        format!("VACUUM INTO '{}'", other.display()),
        "SELECT 1; DELETE FROM t".into(),
        "WITH x AS (SELECT 1) DELETE FROM t".into(),
    ] {
        let mut out = QueryOutcome::default();
        let e = s.run_read_only(&bad, 100, &mut out).await;
        assert!(e.is_err(), "{bad} should be refused");
        assert!(out.results.is_empty(), "{bad} produced results");
    }
    assert!(!other.exists(), "nothing was written next to the database");

    // The session goes on as before: its own reads and writes work, and
    // nothing above changed the table.
    let mut out = QueryOutcome::default();
    s.execute("INSERT INTO t (n) VALUES ('d'); SELECT count(*) FROM t", 100, &mut out).await.unwrap();
    assert_eq!(out.results[1].rows[0][0], serde_json::json!(4));
    let mut out = QueryOutcome::default();
    s.run_read_only("SELECT count(*) FROM t", 100, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(4));
    drop(s);

    // A read-only connection reads too.
    let mut s = open(&path, true).await;
    let mut out = QueryOutcome::default();
    s.run_read_only("SELECT max(id) FROM t", 100, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(4));
    drop(s);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn run_read_only_in_memory_keeps_the_session_writable() {
    let d = dbine_driver_sqlite::drivers().pop().unwrap();
    let cfg = ConnectionConfig { driver: "sqlite".into(), host: ":memory:".into(), ..Default::default() };
    let mut s = d.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("CREATE TABLE t (a); INSERT INTO t VALUES (1)", 100, &mut out).await.unwrap();
    for bad in ["DELETE FROM t", "PRAGMA query_only = OFF", "SELECT 1; DELETE FROM t"] {
        assert!(s.run_read_only(bad, 100, &mut QueryOutcome::default()).await.is_err(), "{bad}");
    }
    let mut out = QueryOutcome::default();
    s.run_read_only("SELECT count(*) FROM t", 100, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(1));
    // query_only was put back: the session still writes.
    s.execute("INSERT INTO t VALUES (2)", 100, &mut QueryOutcome::default()).await.unwrap();
}
