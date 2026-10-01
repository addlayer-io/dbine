//! Against a real TDengine (taosAdapter's REST port):
//!
//! ```sh
//! docker run -d --name dbine-test-tdengine -p 25641:6041 tdengine/tdengine
//! DBINE_TEST_TDENGINE_URL=http://localhost:25641 cargo test -p dbine-driver-tdengine -- --ignored
//! ```

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{kinds, ConnectionConfig, DdlParts, Error, ObjectRef, QueryOutcome, Session};
use serde_json::json;
use std::time::{Duration, Instant};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_TDENGINE_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "tdengine".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("root".into()),
        password: Some("taosdata".into()),
        ..Default::default()
    })
}

fn obj(kind: &str, db: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: Some(db.into()), name: name.into() }
}

#[tokio::test]
#[ignore]
async fn tdengine() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_tdengine::drivers().remove(0);

    let mut bad = c.clone();
    bad.password = Some("nope".into());
    assert!(matches!(d.connect(&bad, None).await, Err(Error::AuthFailed(_))));

    let mut s = d.connect(&c, None).await.unwrap();
    assert!(s.server_version().await.unwrap().starts_with("TDengine 3."));
    cleanup(s.as_mut()).await;
    s.create_database("dbine_it").await.unwrap();
    assert!(s.list_databases().await.unwrap().contains(&"dbine_it".to_string()));

    let mut s = d.connect(&c, Some("dbine_it")).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE STABLE st (ts TIMESTAMP, v DOUBLE, s VARCHAR(20), b VARBINARY(8)) TAGS (loc VARCHAR(20), gid INT);
         CREATE TABLE d1 USING st TAGS ('norte', 1);
         CREATE TABLE d2 USING st TAGS ('sur', 2);
         CREATE TABLE n1 (ts TIMESTAMP, x INT, u BIGINT UNSIGNED) COMMENT 'normal';
         INSERT INTO d1 VALUES ('2024-01-01 00:00:00.000', 1.5, 'a', '\\xCAFE') ('2024-01-01 00:00:01.000', 2.5, 'O''Brien', NULL);
         INSERT INTO d2 VALUES ('2024-01-01 00:00:00.000', 10, 'z', NULL);
         INSERT INTO n1 VALUES ('2024-01-01 00:00:00.000', 1, 18446744073709551615);",
        100,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.results[4].rows_affected, Some(2));

    let objs = s.list_objects().await.unwrap();
    let has = |k: &str, n: &str| objs.iter().any(|o| o.kind == k && o.name == n);
    assert!(has("supertable", "st") && has(kinds::TABLE, "n1") && has("subtable", "d1"), "{objs:?}");
    assert_eq!(objs.iter().find(|o| o.name == "d1").unwrap().parent.as_deref(), Some("st"));
    let cols = s.columns(&obj("supertable", "dbine_it", "st")).await.unwrap();
    assert_eq!(cols.iter().map(|c| c.data_type.as_str()).collect::<Vec<_>>(), ["TIMESTAMP", "DOUBLE", "VARCHAR(20)", "VARBINARY(8)", "VARCHAR(20) TAG", "INT TAG"]);
    assert!(cols[0].primary_key);
    for (k, n) in [("supertable", "st"), (kinds::TABLE, "n1"), ("subtable", "d1")] {
        let def = s.definition(&obj(k, "dbine_it", n)).await.unwrap().unwrap();
        assert!(def.starts_with("CREATE"), "{def}");
    }

    let q = s.browse_query(&obj("supertable", "dbine_it", "st"), 10);
    let mut out = QueryOutcome::default();
    s.execute(&format!("{q}; SELECT * FROM n1; USE information_schema; SELECT count(*) FROM ins_dnodes"), 2, &mut out).await.unwrap();
    let r = &out.results[0];
    assert_eq!((r.rows.len(), r.total_rows, r.truncated), (2, 3, true));
    assert_eq!(r.rows[0][0], json!("2024-01-01 00:00:00.000"));
    assert!(r.rows.iter().any(|row| row[3] == json!("0xCAFE")), "{:?}", r.rows);
    assert_eq!(out.results[1].rows[0][2], json!("18446744073709551615"));
    assert_eq!(out.results[3].rows[0][0], json!(1), "USE changed the database");

    // Timezone option.
    let mut tzc = c.clone();
    tzc.options.insert("timezone".into(), "America/Argentina/Buenos_Aires".into());
    let mut tz = d.connect(&tzc, Some("dbine_it")).await.unwrap();
    let mut out = QueryOutcome::default();
    tz.execute("SELECT ts FROM n1", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], json!("2023-12-31 21:00:00.000 -03:00"));

    // Errors stop the script.
    let mut s = d.connect(&c, Some("dbine_it")).await.unwrap();
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT 1; SELECT * FROM nope; SELECT 2", 10, &mut out).await.unwrap_err();
    assert!(e.is_query(), "{e:?}");
    assert_eq!(out.results.len(), 1);

    // Plans.
    let mut out = QueryOutcome::default();
    s.explain("SELECT loc, avg(v) FROM st GROUP BY loc; INSERT INTO n1 VALUES (now, 2, 0)", false, 10, &mut out).await.unwrap();
    assert_eq!(out.plans.len(), 1);
    assert!(out.results.is_empty() && !out.plans[0].actual);
    let mut out = QueryOutcome::default();
    s.explain("SELECT * FROM st", true, 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 3);
    assert!(out.plans[0].actual && out.plans[0].root.actual_rows.is_some(), "{:#?}", out.plans[0].root);

    // Designer round trip: supertable with tags, normal table with comment and TTL.
    let schema = s.database_schema().await.unwrap();
    assert_eq!(schema.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["st", "n1"]);
    assert_eq!(schema[0].columns.iter().filter(|c| c.options.contains_key("tag")).count(), 2);
    assert_eq!(schema[1].comment.as_deref(), Some("normal"));
    s.create_database("dbine_it2").await.unwrap();
    let moved: Vec<_> = schema.iter().cloned().map(|mut t| {
        t.schema = Some("dbine_it2".into());
        t
    }).collect();
    let script: Vec<String> = moved.iter().map(|t| d.table_ddl(t, DdlParts { create: true, if_exists: true, ..Default::default() }).unwrap()).collect();
    s.execute(&script.join("\n"), 10, &mut QueryOutcome::default()).await.unwrap();
    let mut s2 = d.connect(&c, Some("dbine_it2")).await.unwrap();
    assert_eq!(s2.database_schema().await.unwrap(), moved);
    let again = d.table_ddl(&moved[0], DdlParts { drop: true, if_exists: true, create: true, ..Default::default() }).unwrap();
    s2.execute(&again, 10, &mut QueryOutcome::default()).await.unwrap();

    // INSERT scripts.
    let target = obj(kinds::TABLE, "dbine_it2", "n1");
    let ins = d
        .insert_script(&target, &["ts".into(), "x".into(), "u".into()], &[vec![json!("2024-02-01 00:00:00.000"), json!(7), serde_json::Value::Null], vec![json!("2024-02-01 00:00:01.000"), json!(8), json!(1)]])
        .unwrap();
    let mut out = QueryOutcome::default();
    s2.execute(&ins, 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows_affected, Some(2));
    s2.execute("CREATE TABLE dbine_it2.dd USING dbine_it2.st TAGS ('x', 1)", 10, &mut QueryOutcome::default()).await.unwrap();
    let ins = d.insert_script(&obj(kinds::TABLE, "dbine_it2", "dd"), &["ts".into(), "s".into(), "b".into()], &[vec![json!("2024-02-01 00:00:00.000"), json!("O'Brien"), json!("0xCAFE")]]).unwrap();
    s2.execute(&ins, 10, &mut QueryOutcome::default()).await.unwrap();
    let mut out = QueryOutcome::default();
    s2.execute("SELECT s, b FROM dbine_it2.dd", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0], vec![json!("O'Brien"), json!("0xCAFE")]);
    for t in d.create_templates().iter().filter(|t| t.kind != kinds::VIEW) {
        let sql = t.template.replace("{schema}", "dbine_it2").replace("{name}", &format!("tpl_{}", t.kind)).replace("`supertabla`", "`st`");
        let sql = sql.replace("(ubicacion, grupo)", "(loc, gid)").replace("avg(valor)", "avg(v)").replace("ts, valor", "ts, v").replace("valor DOUBLE", "v DOUBLE");
        s2.execute(&sql, 10, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{}: {e}", t.label));
    }

    // Cancel a long query: the request stops and the server kills it.
    let stop = s.interrupter().unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        stop();
    });
    let t = Instant::now();
    let r = s
        .execute("SELECT _wstart, count(*), cast(_wstart AS VARCHAR(40)) FROM st WHERE ts >= '2023-11-01' AND ts < '2024-01-20' INTERVAL(1s) FILL(VALUE, 0)", 10, &mut QueryOutcome::default())
        .await;
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());

    // Read-only (the registry wraps SQL sessions).
    let mut ro = ReadOnlySession::new(d.connect(&c, Some("dbine_it")).await.unwrap());
    assert!(ro.execute("DROP TABLE n1", 10, &mut QueryOutcome::default()).await.is_err());

    cleanup(s.as_mut()).await;
}

/// Topics and streams block DROP DATABASE: they go first.
async fn cleanup(s: &mut dyn Session) {
    for stmt in ["DROP TOPIC IF EXISTS tpl_topic", "DROP STREAM IF EXISTS tpl_stream", "DROP DATABASE IF EXISTS dbine_it", "DROP DATABASE IF EXISTS dbine_it2"] {
        let _ = s.execute(stmt, 10, &mut QueryOutcome::default()).await;
    }
}

#[tokio::test]
#[ignore]
async fn monitor() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_tdengine::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let snap = s.monitor().await.unwrap();
    let v = |k: &str| snap.metrics.iter().find(|m| m.key == k).and_then(|m| m.value);
    for k in ["connections", "tables", "vnodes", "dnodes_ready"] {
        assert!(v(k).is_some(), "{k}: {:#?}", snap.metrics);
    }
    // The official image runs taosKeeper, which fills the log database.
    for k in ["cpu", "mem_used", "disk_used", "uptime"] {
        assert!(v(k).is_some(), "{k}: {:#?} {:?}", snap.metrics, snap.notes);
    }
    assert!(v("mem_used").unwrap() > 1e6);
    let t = |k: &str| snap.tables.iter().find(|t| t.key == k).unwrap();
    assert_eq!(t("nodes").rows.len(), 1);
    assert!(!t("sessions").rows.is_empty());
    assert!(snap.info.iter().any(|(k, v)| k == "Versión" && v.starts_with("3.")));
}

/// The profiler: a statement that outlives a client heartbeat is seen once,
/// and the profiler's own are left out.
#[tokio::test]
#[ignore]
async fn tdengine_profiler() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_tdengine::drivers().remove(0);
    assert!(d.supports_profiler());
    const ROWS: u64 = 2_000_000;
    let mut admin = d.connect(&c, None).await.unwrap();
    async fn exec(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
        let mut out = QueryOutcome::default();
        s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{sql}"));
        assert!(out.error.is_none(), "{:?}\n{sql}", out.error);
        out
    }
    exec(&mut admin, "CREATE DATABASE IF NOT EXISTS dbine_prof").await;
    let have = exec(&mut admin, "SELECT COUNT(*) FROM dbine_prof.big").await;
    let have = have.results.first().and_then(|r| r.rows.first()).and_then(|r| r[0].as_u64());
    if have != Some(ROWS) {
        exec(&mut admin, "DROP TABLE IF EXISTS dbine_prof.big").await;
        exec(&mut admin, "CREATE TABLE dbine_prof.big (ts TIMESTAMP, v INT)").await;
        for b in 0..ROWS / 20_000 {
            let vals: Vec<String> =
                (0..20_000u64).map(|i| format!("({}, {})", 1_700_000_000_000 + b * 20_000 + i, (b * 20_000 + i) * 7919 % 1_000_003)).collect();
            exec(&mut admin, &format!("INSERT INTO dbine_prof.big VALUES {}", vals.join(" "))).await;
        }
    }
    let mut p = d.connect(&c, Some("dbine_prof")).await.unwrap();
    let mut w = d.connect(&c, Some("dbine_prof")).await.unwrap();
    let opts = dbine_driver::ProfilerOptions { database: "dbine_prof".into(), change_server: true };
    let started = p.profiler_start(&opts).await.expect("profiler_start");
    eprintln!("{started:?}");
    let marker = format!("dbine_prof_{}", std::process::id());
    let work = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        exec(&mut w, &format!("SELECT v AS g, COUNT(*) AS {marker}_slow FROM big GROUP BY v ORDER BY {marker}_slow DESC LIMIT 1")).await;
        tokio::time::sleep(Duration::from_millis(2500)).await;
    };
    let watch = async {
        let mut got = Vec::new();
        let until = Instant::now() + Duration::from_secs(8);
        while Instant::now() < until && !got.iter().any(|s: &dbine_driver::ProfiledStatement| s.text.contains(&marker)) {
            got.extend(p.profiler_poll().await.expect("profiler_poll"));
        }
        got
    };
    let ((), got) = tokio::join!(work, watch);
    p.profiler_stop().await.expect("profiler_stop");
    let mine: Vec<_> = got.iter().filter(|s| s.text.contains(&marker)).collect();
    eprintln!("{mine:#?}");
    assert_eq!(mine.len(), 1, "the slow statement once");
    assert!(mine[0].duration_ms.unwrap_or(0.0) > 0.0, "{:?}", mine[0].duration_ms);
    assert!(got.iter().all(|s| !s.text.contains("perf_queries")), "its own are left out");
}
