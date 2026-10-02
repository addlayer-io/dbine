//! A table's indexes and their usage (`Session::index_usage`) against a real
//! server, and dropping an index through schema sync (what "Eliminar
//! índice…" runs).
//!
//! Oracle flushes DBA_INDEX_USAGE every 15 minutes, so the counters test
//! waits for the next flush (up to `DBINE_TEST_ORACLE_IUT_WAIT` seconds,
//! 1000 by default). Needs a user that can create schemas and users (SYSTEM):
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test index_usage -- --ignored --nocapture --test-threads=1
//! ```

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange};
use serde_json::Value;
use std::time::{Duration, Instant};

fn config_from(url: &str) -> ConnectionConfig {
    let rest = url.strip_prefix("oracle://").expect("oracle://user:pass@host:port/service");
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, service) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    cfg
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    if let Some(e) = out.error.take() {
        panic!("{sql}: {e}");
    }
}

async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> Value {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    out.results.first().and_then(|r| r.rows.first()).and_then(|r| r.first()).cloned().unwrap_or(Value::Null)
}

const TABLES: &str = "
CREATE TABLE parent (id NUMBER CONSTRAINT pk_parent PRIMARY KEY);
CREATE TABLE t (
    id NUMBER CONSTRAINT pk_t PRIMARY KEY,
    a NUMBER, b NUMBER, c VARCHAR2(20),
    p NUMBER CONSTRAINT fk_t_parent REFERENCES parent (id)
);
CREATE INDEX ix_seeked ON t (a, c);
CREATE INDEX ix_untouched ON t (b DESC, UPPER(c));
INSERT INTO parent VALUES (1);
INSERT INTO t SELECT LEVEL, LEVEL, MOD(LEVEL, 7), 'x' || LEVEL, 1 FROM dual CONNECT BY LEVEL <= 500;
COMMIT;
";

fn table() -> ObjectRef {
    ObjectRef { kind: "table".into(), schema: None, name: "T".into() }
}

#[tokio::test]
#[ignore]
async fn index_usage_live() {
    let Ok(url) = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL") else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    let cfg = config_from(&url);
    let d = dbine_driver_oracle::drivers().remove(0);
    assert!(d.supports_index_usage());
    let db = "DBINE_IXU";
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    let _ = admin.drop_database(db).await;
    admin.create_database(db).await.expect("create_database");
    let mut s = d.connect(&cfg, Some(db)).await.unwrap();
    run(&mut s, TABLES).await;
    // Every access counted (not sampled) in this session.
    run(&mut s, "ALTER SESSION SET \"_iut_stat_collection_type\" = ALL").await;
    for i in 1..=5 {
        run(&mut s, &format!("SELECT /*+ INDEX(t ix_seeked) */ c FROM t WHERE a = {i}")).await;
    }

    // The dictionary, right away.
    let r = s.index_usage(&table()).await.unwrap().unwrap().derived();
    eprintln!("{r:#?}");
    assert!(r.stats_available, "{r:?}");
    assert!(!r.seek_scan_split);
    assert!(r.note.as_deref().is_some_and(|n| n.contains("15 minutos")), "{:?}", r.note);
    assert_eq!(r.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["PK_T", "IX_SEEKED", "IX_UNTOUCHED"]);
    let get = |r: &dbine_driver::IndexUsageReport, n: &str| r.indexes.iter().find(|i| i.name == n).unwrap_or_else(|| panic!("{n} in {r:?}")).clone();
    let pk = get(&r, "PK_T");
    assert!(pk.primary_key && pk.unique && pk.kind == "NORMAL", "{pk:?}");
    assert_eq!(pk.key_columns, ["ID"]);
    assert_eq!(get(&r, "IX_SEEKED").key_columns, ["A", "C"]);
    assert_eq!(get(&r, "IX_UNTOUCHED").key_columns, ["B DESC", "UPPER(\"C\")"]);
    assert_eq!(get(&r, "IX_UNTOUCHED").kind, "FUNCTION-BASED NORMAL");
    assert!(r.indexes.iter().all(|i| i.size_kb.is_some_and(|k| k > 0)), "{r:?}");
    assert!(get(&r, "IX_UNTOUCHED").updates > 0, "the insert changed its blocks: {r:?}");
    assert_eq!(r.foreign_keys.len(), 1);
    let fk = &r.foreign_keys[0];
    assert_eq!((fk.name.as_deref(), fk.columns.clone(), fk.ref_table.as_str(), fk.ref_columns.clone(), fk.ref_schema.clone()), (Some("FK_T_PARENT"), vec!["P".to_string()], "PARENT", vec!["ID".to_string()], None));

    // The counters, after Oracle's next flush.
    let wait: u64 = std::env::var("DBINE_TEST_ORACLE_IUT_WAIT").ok().and_then(|v| v.parse().ok()).unwrap_or(1000);
    let start = Instant::now();
    let r = loop {
        let r = s.index_usage(&table()).await.unwrap().unwrap().derived();
        if get(&r, "IX_SEEKED").seeks > 0 || start.elapsed() > Duration::from_secs(wait) {
            break r;
        }
        eprintln!("{}s: waiting for the flush ({:?})", start.elapsed().as_secs(), r.note);
        tokio::time::sleep(Duration::from_secs(30)).await;
    };
    eprintln!("{r:#?}");
    let seeked = get(&r, "IX_SEEKED");
    assert_eq!(seeked.seeks, 5, "{seeked:?}");
    assert_eq!(seeked.scans, 0);
    assert!(seeked.last_read.as_deref().is_some_and(|t| t.len() == 19), "{seeked:?}");
    assert!(seeked.read_share.is_some_and(|v| v > 0.0) && seeked.seek_health.is_none(), "{seeked:?}");
    let untouched = get(&r, "IX_UNTOUCHED");
    assert_eq!(untouched.seeks, 0, "{untouched:?}");
    assert!(untouched.unused, "written by the insert, never read: {untouched:?}");
    assert!(r.note.as_deref().is_some_and(|n| n.contains("último volcado")), "{:?}", r.note);

    // "Eliminar índice…": the table against itself without the index.
    let tables = s.database_schema().await.unwrap();
    let old = tables.iter().find(|t| t.name == "T").unwrap().clone();
    let mut new = old.clone();
    new.indexes.retain(|i| i.name != "IX_UNTOUCHED");
    assert_eq!(new.indexes.len() + 1, old.indexes.len(), "{:?}", old.indexes);
    let script = d.sync_script(&[TableChange::Alter { old, new }]).unwrap();
    eprintln!("{:?}", script.statements);
    assert_eq!(script.statements, ["DROP INDEX \"IX_UNTOUCHED\";"]);
    for st in &script.statements {
        run(&mut s, st).await;
    }
    let r = s.index_usage(&table()).await.unwrap().unwrap();
    assert_eq!(r.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["PK_T", "IX_SEEKED"]);

    drop(s);
    admin.drop_database(db).await.unwrap();
}

/// A user without SELECT_CATALOG_ROLE: the indexes are listed without
/// counters and the note names the privilege; with MONITORING USAGE on every
/// index of its own table, whether each one was used.
#[tokio::test]
#[ignore]
async fn index_usage_without_privileges_live() {
    let Ok(url) = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL") else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    let cfg = config_from(&url);
    let d = dbine_driver_oracle::drivers().remove(0);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    let _ = admin.execute("DROP USER dbine_ixu_plain CASCADE", 1, &mut QueryOutcome::default()).await;
    run(&mut admin, "CREATE USER dbine_ixu_plain IDENTIFIED BY \"Plain_123\" QUOTA UNLIMITED ON users").await;
    run(&mut admin, "GRANT CREATE SESSION, CREATE TABLE TO dbine_ixu_plain").await;
    let mut plain_cfg = cfg.clone();
    plain_cfg.username = Some("dbine_ixu_plain".into());
    plain_cfg.password = Some("Plain_123".into());
    let mut s = d.connect(&plain_cfg, None).await.expect("connect as the plain user");
    assert_eq!(scalar(&mut s, "SELECT COUNT(*) FROM session_roles WHERE role = 'SELECT_CATALOG_ROLE'").await.to_string(), "0");
    run(&mut s, TABLES).await;

    let r = s.index_usage(&table()).await.unwrap().unwrap().derived();
    eprintln!("{r:#?}");
    assert!(!r.stats_available, "{r:?}");
    let note = r.note.clone().unwrap_or_default();
    assert!(note.contains("DBA_INDEX_USAGE") && note.contains("SELECT_CATALOG_ROLE") && note.contains("MONITORING USAGE"), "{note}");
    assert_eq!(r.indexes.len(), 3);
    assert!(r.indexes.iter().all(|i| i.seeks == 0 && !i.unused && i.read_share.is_none()));
    // USER_SEGMENTS: the size of one's own indexes.
    assert!(r.indexes.iter().all(|i| i.size_kb.is_some_and(|k| k > 0)), "{r:?}");
    assert_eq!(r.foreign_keys.len(), 1);

    for ix in ["PK_T", "IX_SEEKED", "IX_UNTOUCHED"] {
        run(&mut s, &format!("ALTER INDEX {ix} MONITORING USAGE")).await;
    }
    run(&mut s, "SELECT /*+ INDEX(t ix_seeked) */ c FROM t WHERE a = 3").await;
    let r = s.index_usage(&table()).await.unwrap().unwrap().derived();
    eprintln!("{r:#?}");
    assert!(r.stats_available, "{r:?}");
    assert!(r.note.as_deref().is_some_and(|n| n.contains("no cuántas veces") && n.contains("V$SEGSTAT")), "{:?}", r.note);
    assert!(r.since.as_deref().is_some_and(|s| s.len() == 19), "{:?}", r.since);
    let get = |n: &str| r.indexes.iter().find(|i| i.name == n).unwrap().clone();
    assert_eq!((get("IX_SEEKED").seeks, get("IX_UNTOUCHED").seeks), (1, 0));
    assert!(get("IX_SEEKED").read_share.is_some_and(|v| v > 0.0));

    // A reader of that table from another schema, with neither DBA_SEGMENTS
    // nor the table's segments in its USER_SEGMENTS: the size is unknown
    // (None), never 0 KB.
    let _ = admin.execute("DROP USER dbine_ixu_reader CASCADE", 1, &mut QueryOutcome::default()).await;
    run(&mut admin, "CREATE USER dbine_ixu_reader IDENTIFIED BY \"Reader_123\"").await;
    run(&mut admin, "GRANT CREATE SESSION TO dbine_ixu_reader").await;
    run(&mut s, "GRANT SELECT ON t TO dbine_ixu_reader").await;
    let mut reader_cfg = cfg.clone();
    reader_cfg.username = Some("dbine_ixu_reader".into());
    reader_cfg.password = Some("Reader_123".into());
    let mut reader = d.connect(&reader_cfg, None).await.expect("connect as the reader");
    let other = ObjectRef { kind: "table".into(), schema: Some("DBINE_IXU_PLAIN".into()), name: "T".into() };
    let r = reader.index_usage(&other).await.unwrap().unwrap().derived();
    eprintln!("{r:#?}");
    assert_eq!(r.indexes.len(), 3, "{r:?}");
    assert!(r.indexes.iter().all(|i| i.size_kb.is_none()), "{r:?}");
    assert_eq!(r.foreign_keys.len(), 1);
    drop(reader);

    drop(s);
    run(&mut admin, "DROP USER dbine_ixu_reader CASCADE").await;
    run(&mut admin, "DROP USER dbine_ixu_plain CASCADE").await;
}
