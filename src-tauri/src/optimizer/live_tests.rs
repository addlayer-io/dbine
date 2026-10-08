//! The optimizer against real engines: the rules' candidates compared with
//! the original on a read-only session (they must be equivalent; a version
//! that isn't must be caught), and index suggestions from real plans.
//!
//! SQLite runs always (a file in the temp folder). The rest are ignored by
//! default and use the `dbine-test-*` containers on their usual ports (each
//! URL can be replaced with `DBINE_TEST_<ENGINE>_URL`):
//!
//! ```sh
//! cargo test -p dbine --lib optimizer::live_tests -- --ignored --test-threads=1 --nocapture
//! ```

use super::compare::{mark_equivalence, measure, Measure, Options, Version};
use crate::commands::optimizer::build;
use dbine_driver::{ConnectionConfig, Driver, QueryOutcome, Session};
use std::sync::Arc;

async fn open(cfg: &ConnectionConfig, db: Option<&str>) -> (Arc<dyn Driver>, Box<dyn Session>) {
    let d = dbine_drivers::find(&cfg.driver).unwrap_or_else(|| panic!("no driver {}", cfg.driver)).clone();
    let s = dbine_drivers::open_session(cfg, db).await.unwrap_or_else(|e| panic!("{}: connect: {e}", cfg.driver));
    (d, s)
}

async fn exec(s: &mut dyn Session, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

async fn plans(s: &mut dyn Session, sql: &str) -> Vec<dbine_driver::Plan> {
    let mut out = QueryOutcome::default();
    s.explain(sql, false, 100, &mut out).await.unwrap_or_else(|e| panic!("explain {sql}: {e}"));
    out.plans
}

/// The original and the versions, measured and checked against it.
async fn compare(s: &mut dyn Session, original: &str, others: &[(&str, String)], ordered: bool) -> Vec<Measure> {
    let mut versions = vec![Version { id: "original".into(), sql: original.into() }];
    versions.extend(others.iter().map(|(id, sql)| Version { id: id.to_string(), sql: sql.clone() }));
    let o = Options { runs: 2, max_rows: 100_000, ordered, execute: true, explain: true };
    let mut out = Vec::new();
    for v in &versions {
        out.push(measure(s, v, &o, &|| false).await);
    }
    mark_equivalence(&mut out);
    out
}

fn sqlite_cfg(path: &std::path::Path, read_only: bool) -> ConnectionConfig {
    ConnectionConfig { driver: "sqlite".into(), host: path.to_string_lossy().into_owned(), read_only, ..Default::default() }
}

#[tokio::test]
async fn sqlite_rules_compare_and_index_hints() {
    let path = std::env::temp_dir().join(format!("dbine-optimizer-{}.db", uuid::Uuid::new_v4()));
    {
        let (_, mut w) = open(&sqlite_cfg(&path, false), None).await;
        for sql in [
            "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT NOT NULL, region INTEGER)",
            "CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL, total REAL, status TEXT)",
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 2000) INSERT INTO customers SELECT i, 'c' || i, CASE WHEN i % 50 = 0 THEN NULL ELSE i % 7 END FROM n",
            "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 20000) INSERT INTO orders SELECT i, (i % 1500) + 1, i * 1.5, CASE WHEN i % 3 = 0 THEN 'A' ELSE 'B' END FROM n",
        ] {
            exec(w.as_mut(), sql).await;
        }
    }
    let (d, mut s) = open(&sqlite_cfg(&path, true), None).await;
    let tables = s.database_schema().await.unwrap();

    // Correlated counts and an OR across columns: the rules rewrite both, and every rewrite returns the same rows.
    let q = "SELECT c.id, c.name, (SELECT COUNT(*) FROM orders o WHERE o.customer_id = c.id) AS n FROM customers c WHERE c.region = 3 OR c.name = 'c10'";
    let a = build(d.as_ref(), q, Some(&tables), plans(s.as_mut(), q).await, Vec::new());
    let rules: Vec<&str> = a.candidates.iter().filter_map(|c| c.rule.as_deref()).collect();
    assert!(rules.contains(&"scalar_to_join") && rules.contains(&"or_to_union"), "{rules:?}");
    let mut others: Vec<(&str, String)> = a.candidates.iter().map(|c| (c.rule.as_deref().unwrap(), c.sql.clone())).collect();
    // A version that isn't equivalent must be caught.
    others.push(("wrong", q.replace("c.region = 3", "c.region = 4")));
    let m = compare(s.as_mut(), q, &others, false).await;
    for x in &m {
        assert!(x.error.is_none(), "{}: {:?}", x.id, x.error);
        assert!(x.min_ms.is_some() && x.avg_ms.is_some() && x.runs_ms.len() == 2, "{}", x.id);
    }
    let eq: Vec<(&str, Option<bool>)> = m.iter().map(|x| (x.id.as_str(), x.equivalent)).collect();
    assert_eq!(eq.last().unwrap(), &("wrong", Some(false)));
    assert!(eq[..eq.len() - 1].iter().all(|(_, e)| *e == Some(true)), "{eq:?}");
    assert!(m[0].rows.unwrap() > 200);

    // IN → EXISTS, NOT IN → NOT EXISTS (both columns NOT NULL), COUNT(*) > 0 → EXISTS, DISTINCT over the key.
    for q in [
        "SELECT * FROM customers c WHERE c.id IN (SELECT o.customer_id FROM orders o WHERE o.status = 'A')",
        "SELECT c.id FROM customers c WHERE c.id NOT IN (SELECT o.customer_id FROM orders o WHERE o.total > 100)",
        "SELECT c.id FROM customers c WHERE (SELECT COUNT(*) FROM orders o WHERE o.customer_id = c.id AND o.status = 'A') > 0",
        "SELECT DISTINCT c.id, c.name FROM customers c WHERE c.region = 2",
        "SELECT * FROM customers c WHERE EXISTS (SELECT 1 FROM orders o WHERE o.customer_id = c.id AND o.total > 29000)",
    ] {
        let a = build(d.as_ref(), q, Some(&tables), Vec::new(), Vec::new());
        assert!(!a.candidates.is_empty(), "no candidate for {q}");
        let others: Vec<(&str, String)> = a.candidates.iter().map(|c| (c.rule.as_deref().unwrap(), c.sql.clone())).collect();
        let m = compare(s.as_mut(), q, &others, false).await;
        assert!(m.iter().all(|x| x.equivalent == Some(true)), "{q}: {:?}", m.iter().map(|x| (&x.id, x.equivalent, &x.error)).collect::<Vec<_>>());
    }

    // Order counts when the original sorts.
    let q = "SELECT id FROM customers WHERE region = 1 ORDER BY id";
    let m = compare(s.as_mut(), q, &[("desc", q.replace("ORDER BY id", "ORDER BY id DESC"))], true).await;
    assert_eq!(m[1].equivalent, Some(false));
    let m = compare(s.as_mut(), q, &[("desc", q.replace("ORDER BY id", "ORDER BY id DESC"))], false).await;
    assert_eq!(m[1].equivalent, Some(true));

    // A scan filtered by columns without an index: a CREATE INDEX for SQLite.
    let q = "SELECT * FROM orders WHERE status = 'A' AND total > 100";
    let a = build(d.as_ref(), q, Some(&tables), plans(s.as_mut(), q).await, Vec::new());
    let h = a.hints.iter().find(|h| h.table == "orders").unwrap_or_else(|| panic!("{:?} / {:?}", a.hints, a.plans.iter().map(|p| &p.root).collect::<Vec<_>>()));
    assert_eq!(h.columns, vec!["status", "total"]);
    assert!(h.script.as_deref().is_some_and(|s| s.contains("CREATE INDEX") && s.contains("orders")), "{:?}", h.script);

    // What writes is never run: it's marked and compared by its plan only.
    let a = build(d.as_ref(), "DELETE FROM orders WHERE id = 1", Some(&tables), Vec::new(), Vec::new());
    assert!(a.writes);
    // The read-only session refuses it anyway.
    let o = Options { runs: 1, max_rows: 10, ordered: false, execute: true, explain: false };
    let m = measure(s.as_mut(), &Version { id: "w".into(), sql: "DELETE FROM orders WHERE id = 1".into() }, &o, &|| false).await;
    assert!(m.error.is_some());
    drop(s);
    let _ = std::fs::remove_file(&path);
}

fn url(env: &str, default: &str) -> String {
    std::env::var(env).unwrap_or_else(|_| default.to_string())
}

fn server_cfg(driver: &str, env: &str, default: &str) -> ConnectionConfig {
    let u = url(env, default);
    let rest = u.split_once("://").map_or(u.as_str(), |(_, r)| r);
    let rest = rest.split(['/', '?']).next().unwrap_or(rest);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').unwrap_or((auth, ""));
    let (host, port) = hostport.rsplit_once(':').unwrap_or((hostport, "0"));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().unwrap_or(0),
        username: (!user.is_empty()).then(|| user.to_string()),
        password: (!pass.is_empty()).then(|| pass.to_string()),
        ..Default::default()
    }
}

#[tokio::test]
#[ignore]
async fn postgres_index_hints_and_compare() {
    let mut cfg = server_cfg("postgres", "DBINE_TEST_POSTGRES_URL", "postgres://postgres:pw@localhost:25010");
    cfg.database = "postgres".into();
    let (d, mut w) = open(&cfg, None).await;
    exec(w.as_mut(), "DROP TABLE IF EXISTS dbine_opt_orders").await;
    exec(w.as_mut(), "CREATE TABLE dbine_opt_orders (id int PRIMARY KEY, customer_id int NOT NULL, status text, created timestamp NOT NULL)").await;
    exec(w.as_mut(), "CREATE INDEX dbine_opt_created ON dbine_opt_orders (created)").await;
    exec(w.as_mut(), "INSERT INTO dbine_opt_orders SELECT g, g % 900, CASE WHEN g % 3 = 0 THEN 'A' ELSE 'B' END, timestamp '2023-06-01' + g * interval '17 minutes' FROM generate_series(1, 60000) g").await;
    exec(w.as_mut(), "ANALYZE dbine_opt_orders").await;
    let result = async {
        let mut ro = cfg.clone();
        ro.read_only = true;
        let (_, mut s) = open(&ro, None).await;
        let tables = s.database_schema().await.unwrap();
        let q = "SELECT id FROM dbine_opt_orders WHERE status = 'A' AND customer_id = 5";
        let a = build(d.as_ref(), q, Some(&tables), plans(s.as_mut(), q).await, Vec::new());
        let h = a.hints.iter().find(|h| h.table == "dbine_opt_orders").unwrap_or_else(|| panic!("{:?}", a.hints));
        assert_eq!(h.reason, "full_scan");
        assert_eq!(h.columns, vec!["status", "customer_id"]);
        assert!(h.script.as_deref().unwrap().starts_with("CREATE INDEX"), "{:?}", h.script);
        // EXTRACT(YEAR …) on the indexed timestamp → a range, with the same rows.
        let q = "SELECT id FROM dbine_opt_orders WHERE EXTRACT(YEAR FROM created) = 2024";
        let a = build(d.as_ref(), q, Some(&tables), plans(s.as_mut(), q).await, Vec::new());
        let c = a.candidates.iter().find(|c| c.rule.as_deref() == Some("function_to_range")).expect("function_to_range");
        let m = compare(s.as_mut(), q, &[("range", c.sql.clone())], false).await;
        assert_eq!(m[1].equivalent, Some(true), "{:?}", m);
        assert!(m.iter().all(|x| x.cost.is_some()));
    }
    .await;
    exec(w.as_mut(), "DROP TABLE IF EXISTS dbine_opt_orders").await;
    result
}

#[tokio::test]
#[ignore]
async fn sqlserver_missing_index_hint() {
    let mut cfg = server_cfg("sqlserver", "DBINE_TEST_SQLSERVER_URL", "mssql://sa:Pw_12345!@localhost:25013");
    cfg.trust_server_certificate = true;
    cfg.database = "tempdb".into();
    let (d, mut w) = open(&cfg, Some("tempdb")).await;
    exec(w.as_mut(), "IF OBJECT_ID('dbo.dbine_opt_orders') IS NOT NULL DROP TABLE dbo.dbine_opt_orders").await;
    exec(w.as_mut(), "CREATE TABLE dbo.dbine_opt_orders (id int PRIMARY KEY, customer_id int NOT NULL, status varchar(5), total decimal(10,2))").await;
    exec(
        w.as_mut(),
        "INSERT INTO dbo.dbine_opt_orders SELECT TOP 50000 n, n % 900, CASE WHEN n % 3 = 0 THEN 'A' ELSE 'B' END, n * 1.5 FROM (SELECT ROW_NUMBER() OVER (ORDER BY (SELECT NULL)) AS n FROM sys.all_objects a CROSS JOIN sys.all_objects b) x",
    )
    .await;
    let result = async {
        let mut ro = cfg.clone();
        ro.read_only = true;
        let (_, mut s) = open(&ro, Some("tempdb")).await;
        // A trivial plan: SQL Server gives no missing-index hint, the scan heuristic does.
        let q = "SELECT id, total FROM dbo.dbine_opt_orders WHERE customer_id = 5 AND status = 'A'";
        let a = build(d.as_ref(), q, None, plans(s.as_mut(), q).await, Vec::new());
        let h = a.hints.iter().find(|h| h.table == "dbine_opt_orders").unwrap_or_else(|| panic!("{:?}", a.hints));
        assert_eq!((h.reason.as_str(), h.columns.clone()), ("full_scan", vec!["customer_id".to_string(), "status".to_string()]));
        assert!(h.script.as_deref().unwrap().contains("CREATE"), "{:?}", h.script);
        // Fully optimized (a join): the engine's own missing-index hint.
        let q = "SELECT o.id, o.total FROM dbo.dbine_opt_orders o JOIN dbo.dbine_opt_orders p ON p.id = o.id + 1 WHERE o.customer_id = 5 AND o.status = 'A'";
        let a = build(d.as_ref(), q, None, plans(s.as_mut(), q).await, Vec::new());
        let h = a.hints.iter().find(|h| h.reason == "missing_index").unwrap_or_else(|| panic!("{:?}", a.hints));
        assert_eq!(h.table, "dbine_opt_orders");
        assert!(h.columns.contains(&"customer_id".to_string()), "{:?}", h.columns);
        assert!(h.script.as_deref().unwrap().starts_with("CREATE NONCLUSTERED INDEX [ix_dbine_opt_orders_"), "{:?}", h.script);
        assert!(h.impact.is_some());
        // The OR → UNION ALL rewrite returns the same rows on SQL Server too.
        let q = "SELECT id FROM dbo.dbine_opt_orders WHERE customer_id = 7 OR status = 'X'";
        let a = build(d.as_ref(), q, None, Vec::new(), Vec::new());
        let c = a.candidates.iter().find(|c| c.rule.as_deref() == Some("or_to_union")).expect("or_to_union");
        let m = compare(s.as_mut(), q, &[("union", c.sql.clone())], false).await;
        assert_eq!(m[1].equivalent, Some(true), "{:?}", m);
    }
    .await;
    exec(w.as_mut(), "DROP TABLE dbo.dbine_opt_orders").await;
    result
}

#[tokio::test]
#[ignore]
async fn mongodb_collscan_hint_and_where() {
    let mut cfg = ConnectionConfig { driver: "mongodb".into(), database: "dbine_opt_db".into(), ..Default::default() };
    cfg.options.insert("connection_string".into(), url("DBINE_TEST_MONGODB_URL", "mongodb://root:secret@localhost:25201/?authSource=admin"));
    let (d, mut w) = open(&cfg, Some("dbine_opt_db")).await;
    let _ = {
        let mut out = QueryOutcome::default();
        w.execute("db.getCollection(\"people\").drop()", 10, &mut out).await
    };
    let docs: Vec<String> = (0..3000).map(|i| format!("{{ _id: {i}, age: {}, city: \"{}\" }}", i % 90, if i % 4 == 0 { "X" } else { "Y" })).collect();
    exec(w.as_mut(), &format!("db.getCollection(\"people\").insertMany([{}])", docs.join(", "))).await;
    let result = async {
        let mut ro = cfg.clone();
        ro.read_only = true;
        let (_, mut s) = open(&ro, Some("dbine_opt_db")).await;
        let q = "db.people.find({ city: \"X\", age: { $gt: 30 } })";
        let a = build(d.as_ref(), q, None, plans(s.as_mut(), q).await, Vec::new());
        let h = a.hints.iter().find(|h| h.table == "people").unwrap_or_else(|| panic!("{:?}", a.hints));
        assert_eq!(h.columns, vec!["city", "age"]);
        assert!(h.script.as_deref().unwrap().contains("createIndex"), "{:?}", h.script);
        let q = "db.people.find({ $where: \"this.age > 30 && this.city == 'X'\" })";
        let a = build(d.as_ref(), q, None, Vec::new(), Vec::new());
        let c = a.candidates.iter().find(|c| c.rule.as_deref() == Some("mongo_where")).expect("mongo_where");
        let m = compare(s.as_mut(), q, &[("ops", c.sql.clone())], false).await;
        assert!(m[0].rows.unwrap() > 0);
        assert_eq!(m[1].equivalent, Some(true), "{:?}", m);
    }
    .await;
    let mut out = QueryOutcome::default();
    let _ = w.execute("db.dropDatabase()", 10, &mut out).await;
    result
}
