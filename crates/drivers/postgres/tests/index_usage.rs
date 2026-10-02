//! Index usage against real servers: a table with a primary key, a foreign
//! key and two secondary indexes; several lookups through one of them and
//! none through the other. Then the unread one is dropped with the schema
//! sync script. Each test reads `DBINE_TEST_<ENGINE>_URL`
//! (`postgres://user:pass@host:port/db`) and is skipped without it:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_TIMESCALE_URL=postgres://postgres:pw@localhost:25015/postgres \
//! DBINE_TEST_YUGABYTE_URL=postgres://yugabyte@localhost:25016/yugabyte \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//!   cargo test -p dbine-driver-postgres --test index_usage -- --ignored --test-threads=1
//! ```

use dbine_driver::{ConnectionConfig, Driver, IndexUsageReport, ObjectRef, QueryOutcome, Session, TableChange};
use std::sync::Arc;
use std::time::Duration;

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

fn table(name: &str) -> ObjectRef {
    ObjectRef { kind: "table".into(), schema: Some("dbine_iu".into()), name: name.into() }
}

async fn report(s: &mut Box<dyn Session>, name: &str) -> IndexUsageReport {
    s.index_usage(&table(name)).await.unwrap().expect("a report").derived()
}

/// The counters arrive a moment later (stats flush, async collection):
/// read until `ok` holds or ~20 s pass.
async fn settled(s: &mut Box<dyn Session>, name: &str, ok: impl Fn(&IndexUsageReport) -> bool) -> IndexUsageReport {
    for _ in 0..40 {
        let r = report(s, name).await;
        if ok(&r) {
            return r;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    report(s, name).await
}

async fn scenario(id: &str, env: &str) {
    let Some(cfg) = cfg(id, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver(id);
    assert!(d.supports_index_usage());
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let crdb = id == "cockroachdb";
    run(&mut s, "DROP SCHEMA IF EXISTS dbine_iu CASCADE").await;
    run(&mut s, "CREATE SCHEMA dbine_iu").await;
    run(&mut s, "CREATE TABLE dbine_iu.parent (id int PRIMARY KEY)").await;
    run(&mut s, "INSERT INTO dbine_iu.parent VALUES (1), (2)").await;
    run(
        &mut s,
        "CREATE TABLE dbine_iu.t (id int CONSTRAINT t_pk PRIMARY KEY, a int, b int, c int,
                                  p int CONSTRAINT fk_t_parent REFERENCES dbine_iu.parent (id))",
    )
    .await;
    run(&mut s, "CREATE INDEX ix_seeked ON dbine_iu.t (a) INCLUDE (c)").await;
    run(&mut s, "CREATE INDEX ix_untouched ON dbine_iu.t (b DESC) WHERE b > 0").await;
    run(&mut s, "INSERT INTO dbine_iu.t (id, a, b, c, p) SELECT g, g % 50, g, g, 1 + g % 2 FROM generate_series(1, 500) AS g").await;
    if !crdb {
        run(&mut s, "SET enable_seqscan = off").await;
        run(&mut s, "SET enable_bitmapscan = off").await;
    }
    for i in 0..6 {
        let sql = if crdb {
            format!("SELECT a, c FROM dbine_iu.t@ix_seeked WHERE a = {i}")
        } else {
            format!("SELECT a, c FROM dbine_iu.t WHERE a = {i}")
        };
        run(&mut s, &sql).await;
    }
    if !crdb {
        run(&mut s, "RESET enable_seqscan").await;
        run(&mut s, "RESET enable_bitmapscan").await;
    }

    let r = settled(&mut s, "t", |r| r.indexes.iter().any(|i| i.name == "ix_seeked" && i.seeks >= 6)).await;
    eprintln!("{id}: {}", serde_json::to_string_pretty(&r).unwrap());
    let get = |n: &str| r.indexes.iter().find(|i| i.name == n).unwrap_or_else(|| panic!("{n} in {:?}", r.indexes));
    assert!(r.stats_available, "{:?}", r.note);
    assert!(!r.seek_scan_split);
    assert_eq!(r.indexes.len(), 3);
    let pk = get("t_pk");
    assert!(pk.primary_key && pk.unique && pk.key_columns == ["id"]);
    let seeked = get("ix_seeked");
    assert!(seeked.seeks >= 6, "{seeked:?}");
    assert_eq!(seeked.key_columns, ["a"]);
    assert_eq!(seeked.included_columns, ["c"]);
    let untouched = get("ix_untouched");
    assert_eq!(untouched.reads, 0);
    assert_eq!(untouched.key_columns, ["b DESC"]);
    assert!(untouched.filter.as_deref().is_some_and(|f| f.contains("b > 0")), "{untouched:?}");
    assert!(seeked.read_share.is_some_and(|x| x > 0.0) && untouched.read_share == Some(0.0));
    if crdb || id == "yugabytedb" {
        // No writes per index: never "sin uso", and no size.
        assert!(!r.writes_counted && untouched.writes_per_read.is_none());
        assert!(!untouched.unused && untouched.updates == 0 && untouched.size_kb.is_none());
        assert!(seeked.last_read.is_some() || !crdb);
    } else {
        assert!(r.writes_counted && untouched.updates >= 500 && untouched.unused, "{untouched:?}");
        assert!(seeked.size_kb.is_some_and(|k| k > 0));
    }
    assert_eq!(r.foreign_keys.len(), 1);
    let fk = &r.foreign_keys[0];
    assert_eq!((fk.name.as_deref(), fk.columns.clone(), fk.ref_table.as_str(), fk.ref_columns.clone()), (Some("fk_t_parent"), vec!["p".to_string()], "parent", vec!["id".to_string()]));
    assert_eq!(fk.ref_schema.as_deref(), Some("dbine_iu"));

    // Drop the unread index through the schema sync script.
    let tables = s.database_schema().await.unwrap();
    let old = tables.iter().find(|t| t.name == "t" && t.schema.as_deref() == Some("dbine_iu")).unwrap().clone();
    let mut new = old.clone();
    new.indexes.retain(|i| i.name != "ix_untouched");
    assert_eq!(new.indexes.len() + 1, old.indexes.len(), "{:?}", old.indexes);
    let script = d.sync_script(&[TableChange::Alter { old: old.clone(), new }]).unwrap();
    eprintln!("{id}: {:?}", script.statements);
    assert!(script.statements.iter().any(|st| st.contains("DROP INDEX") && st.contains("ix_untouched")), "{:?}", script.statements);
    for st in &script.statements {
        run(&mut s, st).await;
    }
    let after = report(&mut s, "t").await;
    assert!(after.indexes.iter().all(|i| i.name != "ix_untouched"));
    assert_eq!(after.indexes.len(), 2);

    run(&mut s, "DROP SCHEMA dbine_iu CASCADE").await;
}

#[tokio::test]
#[ignore]
async fn postgres() {
    scenario("postgres", "DBINE_TEST_POSTGRES_URL").await;
}

#[tokio::test]
#[ignore]
async fn cockroach() {
    scenario("cockroachdb", "DBINE_TEST_COCKROACH_URL").await;
}

#[tokio::test]
#[ignore]
async fn yugabyte() {
    scenario("yugabytedb", "DBINE_TEST_YUGABYTE_URL").await;
}

#[tokio::test]
#[ignore]
async fn timescale() {
    scenario("timescaledb", "DBINE_TEST_TIMESCALE_URL").await;
}

/// A hypertable's index adds up its chunks' scans and sizes.
#[tokio::test]
#[ignore]
async fn timescale_hypertable() {
    let env = "DBINE_TEST_TIMESCALE_URL";
    let Some(cfg) = cfg("timescaledb", env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver("timescaledb");
    let mut s = d.connect(&cfg, None).await.expect("connect");
    run(&mut s, "CREATE EXTENSION IF NOT EXISTS timescaledb").await;
    run(&mut s, "DROP SCHEMA IF EXISTS dbine_iu CASCADE").await;
    run(&mut s, "CREATE SCHEMA dbine_iu").await;
    run(&mut s, "CREATE TABLE dbine_iu.m (ts timestamptz NOT NULL, dev int, v float8)").await;
    run(&mut s, "SELECT create_hypertable('dbine_iu.m', 'ts', chunk_time_interval => interval '1 day')").await;
    run(&mut s, "CREATE INDEX m_dev ON dbine_iu.m (dev)").await;
    run(&mut s, "CREATE INDEX m_v ON dbine_iu.m (v)").await;
    run(&mut s, "INSERT INTO dbine_iu.m SELECT '2026-01-01'::timestamptz + g * interval '1 hour', g % 10, g FROM generate_series(1, 200) AS g").await;
    run(&mut s, "SET enable_seqscan = off").await;
    run(&mut s, "SET enable_bitmapscan = off").await;
    for i in 0..3 {
        run(&mut s, &format!("SELECT count(*) FROM dbine_iu.m WHERE dev = {i}")).await;
    }
    let r = settled(&mut s, "m", |r| r.indexes.iter().any(|i| i.name == "m_dev" && i.seeks >= 3)).await;
    eprintln!("hypertable: {}", serde_json::to_string_pretty(&r).unwrap());
    let get = |n: &str| r.indexes.iter().find(|i| i.name == n).unwrap();
    assert!(get("m_dev").seeks >= 3, "chunk scans add up: {:?}", get("m_dev"));
    assert!(get("m_dev").size_kb.is_some_and(|k| k > 0));
    assert!(get("m_v").unused && get("m_v").updates >= 200, "{:?}", get("m_v"));
    run(&mut s, "DROP SCHEMA dbine_iu CASCADE").await;
}

/// Engines that list their indexes and keys, with or without counters:
/// the setup, the table, the indexes expected (primary key first) and
/// whether the counters are there.
#[allow(clippy::too_many_arguments)]
async fn listing(id: &str, env: &str, setup: &[&str], schema: Option<&str>, name: &str, expected: &[&str], fks: usize, stats: bool, cleanup: &[&str]) {
    let Some(cfg) = cfg(id, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver(id);
    assert!(d.supports_index_usage());
    let mut s = d.connect(&cfg, None).await.expect("connect");
    for st in cleanup {
        let mut out = QueryOutcome::default();
        let _ = s.execute(st, 10, &mut out).await;
    }
    for st in setup {
        run(&mut s, st).await;
    }
    let obj = ObjectRef { kind: "table".into(), schema: schema.map(Into::into), name: name.into() };
    let r = s.index_usage(&obj).await.unwrap().expect("a report").derived();
    eprintln!("{id}: {}", serde_json::to_string_pretty(&r).unwrap());
    if r.indexes.is_empty() {
        let tables = s.database_schema().await.unwrap();
        eprintln!("{id} tables: {:?}", tables.iter().map(|t| (t.schema.clone(), t.name.clone(), t.indexes.len())).collect::<Vec<_>>());
    }
    let mut names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
    names.sort();
    let mut want = expected.to_vec();
    want.sort();
    assert_eq!(names, want);
    assert_eq!(r.foreign_keys.len(), fks);
    assert_eq!(r.stats_available, stats, "{:?}", r.note);
    assert!(stats || r.note.is_some());
    for st in cleanup {
        run(&mut s, st).await;
    }
}

#[tokio::test]
#[ignore]
async fn opengauss() {
    listing(
        "opengauss",
        "DBINE_TEST_OPENGAUSS_URL",
        &[
            "CREATE SCHEMA dbine_iu",
            "CREATE TABLE dbine_iu.parent (id int PRIMARY KEY)",
            "CREATE TABLE dbine_iu.t (id int CONSTRAINT t_pk PRIMARY KEY, a int, b int, p int CONSTRAINT fk_t_parent REFERENCES dbine_iu.parent (id))",
            "CREATE INDEX ix_a ON dbine_iu.t (a)",
            "CREATE INDEX ix_b ON dbine_iu.t (b DESC) WHERE b > 0",
            "INSERT INTO dbine_iu.t (id, a, b) SELECT g, g, g FROM generate_series(1, 100) AS g",
        ],
        Some("dbine_iu"),
        "t",
        &["t_pk", "ix_a", "ix_b"],
        1,
        true,
        &["DROP SCHEMA dbine_iu CASCADE"],
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn cloudberry() {
    listing(
        "cloudberry",
        "DBINE_TEST_CLOUDBERRY_URL",
        &[
            "CREATE SCHEMA dbine_iu",
            "CREATE TABLE dbine_iu.t (id int CONSTRAINT t_pk PRIMARY KEY, a int, b int) DISTRIBUTED BY (id)",
            "CREATE INDEX ix_a ON dbine_iu.t (a)",
            "INSERT INTO dbine_iu.t SELECT g, g, g FROM generate_series(1, 100) AS g",
        ],
        Some("dbine_iu"),
        "t",
        &["t_pk", "ix_a"],
        0,
        true,
        &["DROP SCHEMA dbine_iu CASCADE"],
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn materialize() {
    listing(
        "materialize",
        "DBINE_TEST_MATERIALIZE_URL",
        &["CREATE SCHEMA dbine_iu", "CREATE TABLE dbine_iu.t (id int, a int)", "CREATE INDEX ix_a ON dbine_iu.t (a)"],
        Some("dbine_iu"),
        "t",
        &["ix_a"],
        0,
        false,
        &["DROP SCHEMA dbine_iu CASCADE"],
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn risingwave() {
    listing(
        "risingwave",
        "DBINE_TEST_RISINGWAVE_URL",
        &["CREATE SCHEMA dbine_iu", "CREATE TABLE dbine_iu.t (id int PRIMARY KEY, a int)", "CREATE INDEX ix_a ON dbine_iu.t (a)"],
        Some("dbine_iu"),
        "t",
        &["PRIMARY KEY", "ix_a"],
        0,
        false,
        &["DROP INDEX IF EXISTS dbine_iu.ix_a", "DROP TABLE IF EXISTS dbine_iu.t", "DROP SCHEMA IF EXISTS dbine_iu"],
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn cratedb() {
    listing(
        "cratedb",
        "DBINE_TEST_CRATEDB_URL",
        &["CREATE TABLE dbine_iu.t (id int PRIMARY KEY, a text, INDEX ft_a USING FULLTEXT (a))"],
        Some("dbine_iu"),
        "t",
        &["PRIMARY KEY", "ft_a"],
        0,
        false,
        &["DROP TABLE IF EXISTS dbine_iu.t"],
    )
    .await;
}

#[tokio::test]
#[ignore]
async fn h2() {
    listing(
        "h2",
        "DBINE_TEST_H2_URL",
        &[
            "CREATE SCHEMA dbine_iu",
            "CREATE TABLE dbine_iu.parent (id int PRIMARY KEY)",
            "CREATE TABLE dbine_iu.t (id int CONSTRAINT t_pk PRIMARY KEY, a int, p int CONSTRAINT fk_t_parent REFERENCES dbine_iu.parent (id))",
            "CREATE INDEX ix_a ON dbine_iu.t (a)",
        ],
        Some("dbine_iu"),
        "t",
        &["t_pk", "ix_a"],
        1,
        false,
        &["DROP SCHEMA IF EXISTS dbine_iu CASCADE"],
    )
    .await;
}
