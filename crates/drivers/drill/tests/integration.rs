//! Against a real Apache Drill (embedded mode):
//!
//! ```sh
//! docker run -d -i --name dbine-test-drill -p 25847:8047 apache/drill
//! DBINE_TEST_DRILL_URL=http://localhost:25847 cargo test -p dbine-driver-drill -- --ignored
//! ```

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{kinds, ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};
use serde_json::json;
use std::time::{Duration, Instant};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_DRILL_URL").ok()?).expect("URL");
    Some(ConnectionConfig { driver: "drill".into(), host: url.host_str()?.into(), port: url.port().unwrap_or(0), ..Default::default() })
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: Some("dfs.tmp".into()), name: name.into() }
}

#[tokio::test]
#[ignore]
async fn drill() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_drill::drivers().remove(0);

    let mut bad = c.clone();
    bad.database = "nope.nada".into();
    assert!(d.connect(&bad, None).await.is_err());

    let mut s = d.connect(&c, None).await.unwrap();
    assert!(s.server_version().await.unwrap().starts_with("Apache Drill 1."));
    let dbs = s.list_databases().await.unwrap();
    assert!(dbs.contains(&"dfs.tmp".to_string()) && dbs.contains(&"cp.default".to_string()), "{dbs:?}");

    let mut s = d.connect(&c, Some("dfs.tmp")).await.unwrap();
    let mut out = QueryOutcome::default();
    for stmt in ["DROP TABLE IF EXISTS dbine_t", "DROP VIEW IF EXISTS dbine_v"] {
        s.execute(stmt, 10, &mut out).await.unwrap();
    }
    let mut out = QueryOutcome::default();
    s.execute(
        "ALTER SESSION SET `store.format` = 'json';
         CREATE TABLE dbine_t AS SELECT employee_id, full_name, salary, CAST(hire_date AS TIMESTAMP) AS hired, CAST(birth_date AS DATE) AS born
           FROM cp.`employee.json` LIMIT 20;
         CREATE VIEW dbine_v AS SELECT employee_id, full_name FROM dbine_t WHERE salary > 1000",
        100,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.results[1].rows[0][1], json!(20), "CTAS records written");

    // The session option persisted: the table was written as JSON files.
    let objs = s.list_objects().await.unwrap();
    let has = |k: &str, n: &str| objs.iter().any(|o| o.kind == k && o.name == n);
    assert!(has(kinds::VIEW, "dbine_v") && has("file", "dbine_t"), "{objs:?}");
    let cols = s.columns(&obj("file", "dbine_t")).await.unwrap();
    assert!(cols.iter().any(|c| c.name == "full_name"), "{cols:?}");
    let vcols = s.columns(&obj(kinds::VIEW, "dbine_v")).await.unwrap();
    assert_eq!(vcols.len(), 2);
    let def = s.definition(&obj(kinds::VIEW, "dbine_v")).await.unwrap().unwrap();
    assert!(def.starts_with("CREATE OR REPLACE VIEW `dfs.tmp`.`dbine_v` AS"), "{def}");

    let q = s.browse_query(&obj("file", "dbine_t"), 10);
    let mut out = QueryOutcome::default();
    s.execute(&q, 4, &mut out).await.unwrap();
    let r = &out.results[0];
    assert_eq!((r.rows.len(), r.total_rows, r.truncated), (4, 10, true));

    // Types.
    let mut out = QueryOutcome::default();
    s.execute(
        "SELECT CAST('2024-01-31 13:45:00.123' AS TIMESTAMP) AS ts, CAST('2024-01-31' AS DATE) AS d, CAST(1.5 AS DECIMAL(10,2)) AS dec,
                CONVERT_TO('abc', 'UTF8') AS bin, 9007199254740993 AS big FROM (VALUES(1))",
        10,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.results[0].rows[0], vec![json!("2024-01-31 13:45:00.123"), json!("2024-01-31"), json!("1.5"), json!("0x616263"), json!("9007199254740993")]);

    // USE persists between runs.
    let mut out = QueryOutcome::default();
    s.execute("USE cp.`default`; SELECT count(*) FROM `employee.json`", 10, &mut out).await.unwrap();
    s.execute("SELECT count(*) AS n FROM `employee.json`", 10, &mut out).await.unwrap();
    assert_eq!(out.results[2].rows[0][0], json!(1155));
    s.execute("USE dfs.tmp", 10, &mut out).await.unwrap();

    // Errors carry Drill's message and stop the script.
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT 1 FROM (VALUES(1)); SELECT * FROM nope; SELECT 2 FROM (VALUES(1))", 10, &mut out).await.unwrap_err();
    assert!(matches!(&e, Error::Query(m) if m.contains("nope")), "{e:?}");
    assert_eq!(out.results.len(), 1);

    // Plans.
    let q = "SELECT n.n_regionkey, count(*) FROM cp.`tpch/nation.parquet` n JOIN cp.`tpch/region.parquet` r ON n.n_regionkey = r.r_regionkey GROUP BY n.n_regionkey";
    let mut out = QueryOutcome::default();
    s.explain(&format!("{q}; DROP VIEW IF EXISTS nothing_here"), false, 10, &mut out).await.unwrap();
    assert_eq!(out.plans.len(), 1);
    assert!(out.results.is_empty());
    assert_eq!(out.plans[0].root.op, "Screen");
    assert!(out.plans[0].root.total_cost.is_some() && out.plans[0].root.est_rows.is_some());
    let mut out = QueryOutcome::default();
    s.explain(q, true, 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 5);
    let p = &out.plans[0];
    assert!(p.actual);
    assert_eq!(p.root.actual_rows, Some(5.0), "{:#?}", p.root);
    fn scans(n: &dbine_driver::PlanNode, acc: &mut Vec<(Option<String>, Option<f64>)>) {
        if n.op == "Scan" {
            acc.push((n.object.clone(), n.actual_rows));
        }
        n.children.iter().for_each(|c| scans(c, acc));
    }
    let mut acc = Vec::new();
    scans(&p.root, &mut acc);
    assert!(acc.contains(&(Some("cp.tpch/nation.parquet".into()), Some(25.0))), "{acc:?}");

    // Templates.
    for t in d.create_templates() {
        let sql = t.template.replace("{schema}", "dfs.tmp").replace("{name}", "dbine_tpl");
        s.execute(&sql, 10, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{}: {e}", t.label));
        let _ = s.execute("DROP TABLE IF EXISTS dfs.tmp.dbine_tpl; DROP VIEW IF EXISTS dfs.tmp.dbine_tpl", 10, &mut QueryOutcome::default()).await;
    }
    assert!(matches!(d.insert_script(&obj(kinds::TABLE, "x"), &["a".into()], &[vec![json!(1)]]), Err(Error::Unsupported(_))));

    // Cancel: the request stops and Drill cancels the query.
    let stop = s.interrupter().unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        stop();
    });
    let t = Instant::now();
    let slow = "SELECT count(*) FROM (SELECT a.l_orderkey FROM cp.`tpch/lineitem.parquet` a JOIN cp.`tpch/lineitem.parquet` b ON a.l_suppkey = b.l_suppkey JOIN cp.`tpch/lineitem.parquet` c ON b.l_suppkey = c.l_suppkey)";
    let r = s.execute(slow, 10, &mut QueryOutcome::default()).await;
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(10));
    let mut gone = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let mut out = QueryOutcome::default();
        s.execute("SELECT 1 FROM (VALUES(1))", 10, &mut out).await.unwrap();
        let snap = s.monitor().await.unwrap();
        if snap.tables.iter().find(|t| t.key == "queries").unwrap().rows.is_empty() {
            gone = true;
            break;
        }
    }
    assert!(gone, "the query was cancelled on the server");

    // Read-only (the registry wraps SQL sessions).
    let mut ro = ReadOnlySession::new(d.connect(&c, Some("dfs.tmp")).await.unwrap());
    assert!(ro.execute("DROP TABLE dbine_t", 10, &mut QueryOutcome::default()).await.is_err());

    s.execute("DROP VIEW dbine_v; DROP TABLE dbine_t", 10, &mut QueryOutcome::default()).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn monitor() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_drill::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    s.execute("SELECT count(*) FROM cp.`employee.json`", 10, &mut QueryOutcome::default()).await.unwrap();
    let snap = s.monitor().await.unwrap();
    let v = |k: &str| snap.metrics.iter().find(|m| m.key == k).and_then(|m| m.value);
    for k in ["cpu", "mem_used", "direct_used", "connections", "queries", "uptime", "threads"] {
        assert!(v(k).is_some(), "{k}: {:#?}", snap.metrics);
    }
    assert!(v("mem_used").unwrap() > 1e6 && v("queries").unwrap() >= 1.0);
    assert!(snap.metrics.iter().find(|m| m.key == "mem_used").unwrap().max.is_some());
    let nodes = snap.tables.iter().find(|t| t.key == "nodes").unwrap();
    assert_eq!(nodes.rows.len(), 1);
    assert_eq!(nodes.rows[0][2], json!("ONLINE"));
    assert!(!snap.tables.iter().find(|t| t.key == "sessions").unwrap().rows.is_empty());
    assert!(snap.info.iter().any(|(k, v)| k == "Versión" && v.starts_with("1.")));
}

/// The profiler reports another session's queries once each (a slow one,
/// a fast one and a failed one, with its error) from the profile store.
#[tokio::test]
#[ignore]
async fn profiler() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_drill::drivers().remove(0);
    assert!(d.supports_profiler());
    let mut p = d.connect(&c, None).await.unwrap();
    let mut w = d.connect(&c, Some("dfs.tmp")).await.unwrap();
    let opts = dbine_driver::ProfilerOptions { database: "dfs.tmp".into(), change_server: false };
    let started = p.profiler_start(&opts).await.expect("profiler_start");
    eprintln!("{started:?}");
    let marker = format!("dbine_prof_{}", std::process::id());
    let mut out = QueryOutcome::default();
    w.execute(&format!("SELECT count(*) AS {marker}_slow FROM cp.`employee.json` a JOIN cp.`employee.json` b ON a.department_id = b.department_id"), 10, &mut out).await.unwrap();
    w.execute(&format!("SELECT 1 AS {marker}_fast FROM (VALUES(1))"), 10, &mut out).await.unwrap();
    let e = w.execute(&format!("SELECT {marker}_bad FROM nope.nada"), 10, &mut out).await;
    assert!(e.is_err());
    let mut got = Vec::new();
    let until = Instant::now() + Duration::from_secs(15);
    while Instant::now() < until && got.iter().filter(|s: &&dbine_driver::ProfiledStatement| s.text.contains(&marker)).count() < 3 {
        got.extend(p.profiler_poll().await.expect("profiler_poll"));
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // One more poll: nothing is reported twice.
    got.extend(p.profiler_poll().await.expect("profiler_poll"));
    p.profiler_stop().await.unwrap();
    let mine: Vec<_> = got.iter().filter(|s| s.text.contains(&marker)).collect();
    eprintln!("{mine:#?}");
    for kind in ["_slow", "_fast", "_bad"] {
        assert_eq!(mine.iter().filter(|s| s.text.contains(kind)).count(), 1, "{kind} once");
    }
    let bad = mine.iter().find(|s| s.text.contains("_bad")).unwrap();
    assert!(bad.error.as_deref().is_some_and(|e| e.contains("nope")), "{:?}", bad.error);
    assert!(mine.iter().all(|s| s.duration_ms.is_some() && s.user.is_some()));
}
