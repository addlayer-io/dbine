//! Against real servers:
//!
//! ```sh
//! docker run -d --name dbine-test-clickhouse -p 25123:8123 -e CLICKHOUSE_USER=dbine \
//!   -e CLICKHOUSE_PASSWORD=dbine clickhouse/clickhouse-server
//! docker run -d --name dbine-test-proton -p 25119:8123 d.timeplus.com/timeplus-io/proton
//! DBINE_TEST_CLICKHOUSE_URL=http://dbine:dbine@localhost:25123 \
//! DBINE_TEST_TIMEPLUS_URL=http://localhost:25119 \
//!   cargo test -p dbine-driver-clickhouse -- --ignored
//! ```

use dbine_driver::{kinds, ConnectionConfig, Driver, Error, ObjectRef, QueryOutcome};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn cfg(env: &str, id: &str, read_only: bool) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(env).ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: id.into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        read_only,
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_clickhouse::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: None, name: name.into() }
}

#[tokio::test]
#[ignore]
async fn clickhouse() {
    let Some(c) = cfg("DBINE_TEST_CLICKHOUSE_URL", "clickhouse", false) else { return };
    let d = driver("clickhouse");

    let mut bad = c.clone();
    bad.password = Some("wrong".into());
    assert!(matches!(d.connect(&bad, None).await, Err(Error::AuthFailed(_))));

    let mut s = d.connect(&c, None).await.unwrap();
    assert!(s.server_version().await.unwrap().starts_with("ClickHouse "));
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP DATABASE IF EXISTS dbine_it; CREATE DATABASE dbine_it;
         CREATE TABLE dbine_it.t (id UInt64, name Nullable(String), d Decimal(10,2) DEFAULT 1.5, big Int64) ENGINE = MergeTree ORDER BY id;
         INSERT INTO dbine_it.t SELECT number, toString(number), number / 4, 9007199254740993 FROM numbers(20);
         CREATE VIEW dbine_it.v AS SELECT id FROM dbine_it.t;
         CREATE MATERIALIZED VIEW dbine_it.mv ENGINE = MergeTree ORDER BY id AS SELECT id FROM dbine_it.t;
         CREATE DICTIONARY dbine_it.dict (id UInt64, name String) PRIMARY KEY id SOURCE(CLICKHOUSE(TABLE 't' DB 'dbine_it')) LAYOUT(FLAT()) LIFETIME(0);
         CREATE FUNCTION IF NOT EXISTS dbine_twice AS (x) -> x * 2;",
        100,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.results[3].rows_affected, Some(20));

    assert!(s.list_databases().await.unwrap().contains(&"dbine_it".to_string()));
    let mut s = d.connect(&c, Some("dbine_it")).await.unwrap();
    let objs = s.list_objects().await.unwrap();
    let has = |k: &str, n: &str| objs.iter().any(|o| o.kind == k && o.name == n);
    assert!(has(kinds::TABLE, "t") && has(kinds::VIEW, "v") && has(kinds::MATERIALIZED_VIEW, "mv"), "{objs:?}");
    assert!(has("dictionary", "dict") && has(kinds::FUNCTION, "dbine_twice"), "{objs:?}");
    assert!(!objs.iter().any(|o| o.name.starts_with(".inner")));

    let cols = s.columns(&obj(kinds::TABLE, "t")).await.unwrap();
    assert_eq!(cols.len(), 4);
    assert!(cols[0].primary_key && !cols[0].nullable && cols[1].nullable);
    assert_eq!(cols[2].default_value.as_deref(), Some("1.5"));
    for (k, n) in [(kinds::TABLE, "t"), (kinds::VIEW, "v"), ("dictionary", "dict"), (kinds::FUNCTION, "dbine_twice")] {
        let def = s.definition(&obj(k, n)).await.unwrap().unwrap();
        assert!(def.starts_with("CREATE"), "{def}");
    }

    let q = s.browse_query(&obj(kinds::TABLE, "t"), 10);
    let mut out = QueryOutcome::default();
    s.execute(&format!("{q}; SELECT dbine_twice(21) AS x, [1,2] AS a; SELECT 1 FORMAT CSV"), 4, &mut out).await.unwrap();
    let r = &out.results[0];
    assert_eq!((r.rows.len(), r.total_rows, r.truncated), (4, 10, true));
    assert_eq!(r.rows[1][0], serde_json::json!(1));
    assert_eq!(r.rows[1][2], serde_json::json!("0.25"));
    assert_eq!(r.rows[1][3], serde_json::json!("9007199254740993"));
    assert_eq!(out.results[1].rows[0], vec![serde_json::json!(42), serde_json::json!("[1,2]")]);
    assert_eq!(out.results[2].rows[0][0], serde_json::json!("1"));

    // Session state survives between runs.
    let mut out = QueryOutcome::default();
    s.execute("CREATE TEMPORARY TABLE tmp (a UInt8); INSERT INTO tmp VALUES (1)", 10, &mut out).await.unwrap();
    s.execute("SELECT count() FROM tmp", 10, &mut out).await.unwrap();
    assert_eq!(out.results.last().unwrap().rows[0][0], serde_json::json!(1));

    // Error mid-script (and mid-stream).
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT 1; SELECT * FROM nope; SELECT 2", 10, &mut out).await.unwrap_err();
    assert!(e.is_query(), "{e:?}");
    assert_eq!(out.results.len(), 1);
    let mut out = QueryOutcome::default();
    let e = s
        .execute("SELECT throwIf(number = 50000) FROM numbers(100000) SETTINGS max_block_size = 10", 5, &mut out)
        .await
        .unwrap_err();
    assert!(e.is_query() && e.to_string().contains("Code: 395"), "{e:?}");

    // Cancel.
    let stop = s.interrupter().unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        stop();
    });
    let t = Instant::now();
    let mut out = QueryOutcome::default();
    let r = s.execute("SELECT count() FROM numbers(100000000000) WHERE sleepEachRow(0) = 0 SETTINGS max_block_size = 1000", 10, &mut out).await;
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(15));

    // Read-only is enforced by the server too (readonly=1).
    let mut ro = d.connect(&cfg("DBINE_TEST_CLICKHOUSE_URL", "clickhouse", true).unwrap(), Some("dbine_it")).await.unwrap();
    let mut out = QueryOutcome::default();
    let e = ro.execute("INSERT INTO t (id) VALUES (1)", 10, &mut out).await.unwrap_err();
    assert!(e.is_query() && e.to_string().contains("readonly"), "{e:?}");
    ro.execute("SELECT count() FROM t", 10, &mut out).await.unwrap();

    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("DROP FUNCTION dbine_twice; DROP DATABASE dbine_it", 10, &mut out).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn timeplus() {
    let Some(c) = cfg("DBINE_TEST_TIMEPLUS_URL", "timeplus", false) else { return };
    let d = driver("timeplus");
    let mut s = d.connect(&c, None).await.unwrap();
    assert!(s.server_version().await.unwrap().starts_with("Timeplus Proton "));
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP STREAM IF EXISTS dbine_s; CREATE STREAM dbine_s (a int64, b string);
         INSERT INTO dbine_s (a, b) VALUES (1, 'x'), (2, 'y'), (3, 'z');",
        10,
        &mut out,
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let objs = s.list_objects().await.unwrap();
    assert!(objs.iter().any(|o| o.kind == kinds::STREAM && o.name == "dbine_s"), "{objs:?}");
    let cols = s.columns(&obj(kinds::STREAM, "dbine_s")).await.unwrap();
    assert!(cols.iter().any(|c| c.name == "a"), "{cols:?}");
    assert!(s.definition(&obj(kinds::STREAM, "dbine_s")).await.unwrap().is_some());
    let q = s.browse_query(&obj(kinds::STREAM, "dbine_s"), 10);
    let mut out = QueryOutcome::default();
    s.execute(&q, 2, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2);
    assert!(out.results[0].truncated);
    s.execute("DROP STREAM dbine_s", 10, &mut out).await.unwrap();
}

/// Proton's streaming port (3218, e.g. `-p 25118:3218`): an unbounded query
/// stops at `max_rows`. `DBINE_TEST_TIMEPLUS_STREAM_URL=http://localhost:25118`.
#[tokio::test]
#[ignore]
async fn timeplus_streaming_stops_at_the_limit() {
    let Some(c) = cfg("DBINE_TEST_TIMEPLUS_STREAM_URL", "timeplus", false) else { return };
    let d = driver("timeplus");
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP STREAM IF EXISTS dbine_r; CREATE RANDOM STREAM dbine_r (a int64 DEFAULT rand()) SETTINGS eps = 100",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let t = Instant::now();
    let mut out = QueryOutcome::default();
    s.execute("SELECT a FROM dbine_r", 5, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 5);
    assert!(out.results[0].truncated);
    assert!(t.elapsed() < Duration::from_secs(10));
    s.execute("DROP STREAM dbine_r", 10, &mut out).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn clickhouse_plans() {
    let Some(c) = cfg("DBINE_TEST_CLICKHOUSE_URL", "clickhouse", false) else { return };
    let mut s = driver("clickhouse").connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP TABLE IF EXISTS dbine_plan_a;
         CREATE TABLE dbine_plan_a (id UInt64, g UInt8) ENGINE = MergeTree ORDER BY id;
         INSERT INTO dbine_plan_a SELECT number, number % 10 FROM numbers(200000);",
        10,
        &mut out,
    )
    .await
    .unwrap();

    // Estimated: nothing runs; only SELECTs get a plan.
    let mut out = QueryOutcome::default();
    s.explain("SELECT g, count() FROM dbine_plan_a WHERE id < 1000 GROUP BY g; TRUNCATE TABLE dbine_plan_a", false, 10, &mut out)
        .await
        .unwrap();
    assert_eq!(out.plans.len(), 1);
    assert!(out.results.is_empty());
    assert_eq!(out.messages.len(), 1);
    fn find<'a>(n: &'a dbine_driver::PlanNode, op: &str) -> Option<&'a dbine_driver::PlanNode> {
        if n.op == op {
            return Some(n);
        }
        n.children.iter().find_map(|c| find(c, op))
    }
    let read = find(&out.plans[0].root, "ReadFromMergeTree").expect("a read step");
    assert!(read.object.as_deref().unwrap_or("").ends_with("dbine_plan_a"));
    assert!(read.props.iter().any(|(k, _)| k == "Índice PrimaryKey"), "{read:#?}");
    let mut check = QueryOutcome::default();
    s.execute("SELECT count() FROM dbine_plan_a", 10, &mut check).await.unwrap();
    assert_eq!(check.results[0].rows[0][0].to_string().trim_matches('"'), "200000");

    // Analyze: runs once, plans stay estimated.
    let mut out = QueryOutcome::default();
    s.explain("SELECT count() FROM dbine_plan_a WHERE g = 3", true, 10, &mut out).await.unwrap();
    assert_eq!(out.results.len(), 1);
    assert_eq!(out.plans.len(), 1);
    assert!(!out.plans[0].actual);
    let full = find(&out.plans[0].root, "ReadFromMergeTree").unwrap();
    assert!(!full.warnings.is_empty(), "{full:#?}");

    let mut out = QueryOutcome::default();
    assert!(s.explain("SELECT 1; SELECT * FROM dbine_plan_missing", true, 10, &mut out).await.is_err());
    assert_eq!(out.results.len(), 1);
    s.execute("DROP TABLE dbine_plan_a", 10, &mut QueryOutcome::default()).await.unwrap();
}

async fn run(s: &mut Box<dyn dbine_driver::Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{e}\n{sql}"));
    out
}

/// database_schema, DDL round trip into a new database, inserts, and
/// create / drop database.
#[tokio::test]
#[ignore]
async fn clickhouse_schema_round_trip() {
    use dbine_driver::DdlParts;
    let Some(c) = cfg("DBINE_TEST_CLICKHOUSE_URL", "clickhouse", false) else { return };
    let d = driver("clickhouse");
    let mut s = d.connect(&c, None).await.unwrap();
    let _ = s.drop_database("dbine_ddl").await;
    let _ = s.drop_database("dbine_ddl_copy").await;
    s.create_database("dbine_ddl").await.unwrap();
    assert!(s.list_databases().await.unwrap().contains(&"dbine_ddl".to_string()));
    let mut s = d.connect(&c, Some("dbine_ddl")).await.unwrap();
    run(
        &mut s,
        "CREATE TABLE ev (
            id UInt64 COMMENT 'clave',
            ver UInt32,
            d DateTime DEFAULT now(),
            day Date MATERIALIZED toDate(d),
            txt Nullable(String) CODEC(ZSTD(1)),
            lc LowCardinality(Nullable(String)),
            tags Array(String),
            INDEX ix_txt txt TYPE bloom_filter(0.01) GRANULARITY 4
         ) ENGINE = ReplacingMergeTree(ver) PARTITION BY toYYYYMM(d) PRIMARY KEY id ORDER BY (id, d)
           TTL d + INTERVAL 1 YEAR SETTINGS index_granularity = 4096 COMMENT 'Eventos';
         CREATE TABLE plain (a String, b Int32) ENGINE = Memory;
         CREATE VIEW v AS SELECT 1;",
    )
    .await;
    let schema = s.database_schema().await.unwrap();
    assert_eq!(schema.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), vec!["ev", "plain"]);
    let ev = &schema[0];
    assert_eq!(ev.options.get("engine").map(String::as_str), Some("ReplacingMergeTree"), "{ev:#?}");
    assert_eq!(ev.options.get("engine_args").map(String::as_str), Some("ver"));
    assert_eq!(ev.options.get("order_by").map(String::as_str), Some("(id, d)"));
    assert_eq!(ev.options.get("partition_by").map(String::as_str), Some("toYYYYMM(d)"));
    assert_eq!(ev.primary_key.as_ref().unwrap().columns, vec!["id"]);
    assert_eq!(ev.comment.as_deref(), Some("Eventos"));
    assert_eq!(ev.columns[0].comment.as_deref(), Some("clave"));
    assert!(ev.columns[4].nullable && ev.columns[4].data_type == "String", "{:?}", ev.columns[4]);
    assert_eq!(ev.columns[3].options.get("default_kind").map(String::as_str), Some("MATERIALIZED"));
    assert_eq!(ev.indexes[0].kind.as_deref(), Some("bloom_filter(0.01) GRANULARITY 4"));

    // Round trip into another database.
    s.create_database("dbine_ddl_copy").await.unwrap();
    let all = DdlParts { drop: true, if_exists: true, create: true, indexes: true, foreign_keys: true };
    let mut copy = d.connect(&c, Some("dbine_ddl_copy")).await.unwrap();
    for t in &schema {
        let mut t = t.clone();
        t.schema = None;
        let ddl = d.table_ddl(&t, all).unwrap();
        run(&mut copy, &ddl).await;
    }
    let mut back = copy.database_schema().await.unwrap();
    for t in &mut back {
        t.schema = Some("dbine_ddl".into());
    }
    assert_eq!(back, schema);

    // Separate index statements, as the script generator writes them.
    let mut t = schema[0].clone();
    t.name = "ev2".into();
    t.schema = None;
    run(&mut copy, &d.table_ddl(&t, DdlParts { create: true, ..Default::default() }).unwrap()).await;
    run(&mut copy, &d.table_ddl(&t, DdlParts { indexes: true, if_exists: true, ..Default::default() }).unwrap()).await;

    // Inserts with awkward strings.
    let target = ObjectRef { kind: kinds::TABLE.into(), schema: None, name: "ev".into() };
    let ins = d
        .insert_script(
            &target,
            &["id".into(), "ver".into(), "txt".into(), "tags".into()],
            &[
                vec![1.into(), 1.into(), "C:\\dir 'x'".into(), serde_json::json!(["a"]).to_string().replace('"', "'").into()],
                vec![2.into(), 1.into(), serde_json::Value::Null, "[]".into()],
            ],
        )
        .unwrap();
    // Arrays come as text; ClickHouse parses them from a string only with a cast, so insert scalars.
    let ins_scalar = d
        .insert_script(&target, &["id".into(), "ver".into(), "txt".into()], &[vec![1.into(), 1.into(), "C:\\dir 'x'".into()], vec![2.into(), 1.into(), serde_json::Value::Null]])
        .unwrap();
    let _ = ins;
    run(&mut copy, &ins_scalar).await;
    let out = run(&mut copy, "SELECT txt FROM ev WHERE id = 1").await;
    assert_eq!(out.results[0].rows[0][0], serde_json::json!("C:\\dir 'x'"));

    // A designer-built table: nullable, codec, default kinds, plain engine.
    let designed: dbine_driver::TableSchema = serde_json::from_value(serde_json::json!({
        "name": "nueva", "comment": "diseñador",
        "columns": [
            {"name": "id", "data_type": "UInt32", "nullable": false},
            {"name": "n", "data_type": "String", "nullable": true, "default_value": "'x'", "options": {"codec": "LZ4"}}
        ],
        "primary_key": {"columns": ["id"]},
        "indexes": [{"name": "ix_n", "columns": ["n"]}],
        "options": {"engine": "MergeTree", "partition_by": "id % 4"}
    }))
    .unwrap();
    run(&mut copy, &d.table_ddl(&designed, all).unwrap()).await;

    drop(copy);
    assert!(s.drop_database("dbine_ddl").await.is_err(), "own database");
    let mut s0 = d.connect(&c, None).await.unwrap();
    s0.drop_database("dbine_ddl_copy").await.unwrap();
    s0.drop_database("dbine_ddl").await.unwrap();
    assert!(!s0.list_databases().await.unwrap().iter().any(|x| x.starts_with("dbine_ddl")));
}

#[tokio::test]
#[ignore]
async fn timeplus_schema_round_trip() {
    use dbine_driver::DdlParts;
    let Some(c) = cfg("DBINE_TEST_TIMEPLUS_URL", "timeplus", false) else { return };
    let d = driver("timeplus");
    let mut s = d.connect(&c, None).await.unwrap();
    // The stock image's `default` user may only use the `default` database.
    let _ = s.drop_database("dbine_tp").await;
    let own_db = match s.create_database("dbine_tp").await {
        Ok(()) => true,
        Err(Error::Query(m)) if m.contains("ACCESS_DENIED") => false,
        Err(e) => panic!("{e}"),
    };
    let mut s = d.connect(&c, Some(if own_db { "dbine_tp" } else { "default" })).await.unwrap();
    run(&mut s, "DROP STREAM IF EXISTS kv; DROP STREAM IF EXISTS ap").await;
    let designed: dbine_driver::TableSchema = serde_json::from_value(serde_json::json!({
        "kind": "stream", "name": "kv", "comment": "estado",
        "columns": [
            {"name": "k", "data_type": "string", "nullable": false},
            {"name": "v", "data_type": "int32", "nullable": true, "comment": "valor"},
            {"name": "d", "data_type": "string", "nullable": false, "default_value": "'x'"}
        ],
        "primary_key": {"columns": ["k"]},
        "options": {"mode": "versioned_kv"}
    }))
    .unwrap();
    let all = DdlParts { drop: true, if_exists: true, create: true, indexes: true, foreign_keys: true };
    run(&mut s, &d.table_ddl(&designed, all).unwrap()).await;
    let mut plain = designed.clone();
    plain.name = "ap".into();
    plain.primary_key = None;
    plain.options.clear();
    run(&mut s, &d.table_ddl(&plain, all).unwrap()).await;
    let schema: Vec<_> = s.database_schema().await.unwrap().into_iter().filter(|t| t.name == "kv" || t.name == "ap").collect();
    eprintln!("{schema:#?}");
    assert_eq!(schema.len(), 2);
    let kv = schema.iter().find(|t| t.name == "kv").unwrap();
    assert_eq!(kv.kind, kinds::STREAM);
    assert_eq!(kv.options.get("mode").map(String::as_str), Some("versioned_kv"));
    assert_eq!(kv.primary_key.as_ref().unwrap().columns, vec!["k"]);
    assert_eq!(kv.columns.len(), 3);
    assert!(kv.columns[1].nullable);
    // Recreate from what was read.
    for t in &schema {
        run(&mut s, &d.table_ddl(t, all).unwrap()).await;
    }
    let again: Vec<_> = s.database_schema().await.unwrap().into_iter().filter(|t| t.name == "kv" || t.name == "ap").collect();
    assert_eq!(again, schema);
    let target = ObjectRef { kind: kinds::STREAM.into(), schema: None, name: "ap".into() };
    let ins = d.insert_script(&target, &["k".into(), "v".into()], &[vec!["a'b".into(), 1.into()], vec!["c".into(), serde_json::Value::Null]]).unwrap();
    run(&mut s, &ins).await;
    run(&mut s, "DROP STREAM kv; DROP STREAM ap").await;
    drop(s);
    if own_db {
        let mut s0 = d.connect(&c, None).await.unwrap();
        s0.drop_database("dbine_tp").await.unwrap();
    }
}

/// Two snapshots with a query running meanwhile: real values and the
/// standard tables.
async fn monitor(env: &str, id: &str) {
    let Some(c) = cfg(env, id, false) else { return };
    let d = driver(id);
    assert!(d.capabilities().monitor);
    let mut s = d.connect(&c, None).await.unwrap();
    let (d2, c2) = (d.clone(), c.clone());
    let sql = if id == "timeplus" { "SELECT sleep(2)" } else { "SELECT sleepEachRow(1) FROM numbers(3)" };
    let bg = tokio::spawn(async move {
        let mut b = d2.connect(&c2, None).await.unwrap();
        let mut out = QueryOutcome::default();
        let _ = b.execute(sql, 10, &mut out).await;
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let first = s.monitor().await.expect("monitor");
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let snap = s.monitor().await.expect("monitor");
    for m in &snap.metrics {
        eprintln!("{id}: {} [{}] = {:?} max {:?}{}", m.key, m.group, m.value, m.max, if m.counter { " (counter)" } else { "" });
    }
    for t in &snap.tables {
        eprintln!("{id}: table {} ({}) {} rows, cols {:?}", t.key, t.title, t.rows.len(), t.columns);
    }
    eprintln!("{id}: info {:?}", snap.info);
    eprintln!("{id}: notes {:#?}", snap.notes);
    for k in ["cpu", "mem_used", "connections", "queries", "uptime", "storage_used"] {
        assert!(snap.metrics.iter().any(|m| m.key == k && m.value.is_some()), "{id}: no {k}");
    }
    for t in &snap.tables {
        assert!(t.rows.len() <= 200 && t.rows.iter().all(|r| r.len() == t.columns.len()), "{}", t.key);
    }
    let running = first.tables.iter().chain(&snap.tables).any(|t| t.key == "queries" && !t.rows.is_empty());
    assert!(running, "{id}: the sleeping query should show");
    bg.await.unwrap();
}

#[tokio::test]
#[ignore]
async fn clickhouse_monitor() {
    monitor("DBINE_TEST_CLICKHOUSE_URL", "clickhouse").await;
}

#[tokio::test]
#[ignore]
async fn timeplus_monitor() {
    monitor("DBINE_TEST_TIMEPLUS_URL", "timeplus").await;
}

/// The profiler: one session profiles while another runs a slow statement
/// and a fast one; each is seen once, and the profiler's own are left out.
async fn profile(env: &str, id: &str, read_only: bool) {
    let Some(mut c) = cfg(env, id, false) else { return };
    let d = driver(id);
    assert!(d.supports_profiler());
    // Proton's default user can't create databases.
    let db = if id == "timeplus" { "default" } else { "dbine_prof" };
    let mut admin = d.connect(&c, Some("system")).await.unwrap();
    run(&mut admin, &format!("CREATE DATABASE IF NOT EXISTS {db}")).await;
    c.database = db.into();
    let mut w = d.connect(&c, None).await.unwrap();
    c.read_only = read_only;
    let mut p = d.connect(&c, None).await.unwrap();
    let opts = dbine_driver::ProfilerOptions { database: db.into(), change_server: !read_only };
    let started = p.profiler_start(&opts).await.expect("profiler_start");
    eprintln!("{id}: {started:?}");
    let marker = format!("dbine_prof_{}_{}", std::process::id(), read_only);
    let work = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(run(&mut w, &format!("SELECT sleep(0.6), 1 AS {marker}_slow")).await.error.is_none());
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(run(&mut w, &format!("SELECT 1 AS {marker}_fast")).await.error.is_none());
        // Another database: left out.
        assert!(run(&mut admin, &format!("SELECT 2 AS {marker}_other")).await.error.is_none());
    };
    let watch = async {
        let mut got = Vec::new();
        let until = Instant::now() + Duration::from_secs(if read_only { 20 } else { 8 });
        while Instant::now() < until {
            got.extend(p.profiler_poll().await.expect("profiler_poll"));
            if got.iter().filter(|s| s.text.contains(&marker)).count() >= 2 && Instant::now() + Duration::from_secs(5) > until {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        got
    };
    let ((), got) = tokio::join!(work, watch);
    p.profiler_stop().await.expect("profiler_stop");
    let mine: Vec<_> = got.iter().filter(|s| s.text.contains(&marker)).collect();
    eprintln!("{id}: {mine:#?}");
    let slow: Vec<_> = mine.iter().filter(|s| s.text.contains("_slow")).collect();
    assert_eq!(slow.len(), 1, "{id}: the slow statement once");
    assert!(slow[0].duration_ms.unwrap_or(0.0) >= 500.0, "{id}: {:?}", slow[0].duration_ms);
    // CPU from ProfileEvents, rows read and written.
    assert_eq!(started.reads_unit.as_deref(), Some("filas"));
    assert!(slow[0].cpu_ms.is_some() && slow[0].reads.is_some() && slow[0].writes.is_some(), "{id}: figures");
    assert_eq!(mine.iter().filter(|s| s.text.contains("_fast")).count(), 1, "{id}: the fast statement once");
    assert!(!mine.iter().any(|s| s.text.contains("_other")), "{id}: other databases are left out");
    assert!(got.iter().all(|s| !s.text.contains("query_log") && !s.text.contains("FLUSH LOGS")), "{id}: its own are left out");
}

#[tokio::test]
#[ignore]
async fn clickhouse_profiler() {
    profile("DBINE_TEST_CLICKHOUSE_URL", "clickhouse", false).await;
    profile("DBINE_TEST_CLICKHOUSE_URL", "clickhouse", true).await;
}

#[tokio::test]
#[ignore]
async fn timeplus_profiler() {
    profile("DBINE_TEST_TIMEPLUS_URL", "timeplus", false).await;
}

/// The editor's script contract: the app's units (heredocs, backslash
/// escapes), errors with code and position, session state between calls.
#[tokio::test]
#[ignore]
async fn clickhouse_script_contract() {
    let Some(c) = cfg("DBINE_TEST_CLICKHOUSE_URL", "clickhouse", false) else { return };
    let d = driver("clickhouse");
    assert_eq!(d.script_mode(), dbine_driver::sql::ScriptMode::PerStatement);
    let script = "SET max_threads = 3;\nSELECT 'it\\'s; ok' AS a, $h$x;y$h$ AS b;\nSELECT getSetting('max_threads');\nSELEC 1;";
    let units = d.split_script(script);
    assert_eq!(units.len(), 4, "{units:?}");
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    for u in &units[..3] {
        s.execute(&u.text, 10, &mut out).await.unwrap_or_else(|e| panic!("{}: {e}", u.text));
    }
    assert_eq!(out.results[1].rows[0], vec![serde_json::json!("it's; ok"), serde_json::json!("x;y")]);
    assert_eq!(out.results[2].rows[0][0].to_string().trim_matches('"'), "3");
    let e = s.execute(&units[3].text, 10, &mut out).await.unwrap_err().to_script_error();
    eprintln!("{e:?}");
    assert_eq!((e.code.as_deref(), e.offset, e.line), (Some("62"), Some(0), Some(1)));
    let e = s.execute("SELECT 1;\nSELECT * FROM nope_nope", 10, &mut out).await.unwrap_err().to_script_error();
    assert_eq!((e.code.as_deref(), e.line), (Some("60"), Some(2)), "{e:?}");

    // USE moves the session (and the tab) as in clickhouse-client.
    let mut go = QueryOutcome::default();
    s.execute("CREATE DATABASE IF NOT EXISTS dbine_use_db2", 10, &mut go).await.unwrap();
    let mut out = QueryOutcome::default();
    for u in d.split_script("USE `dbine_use_db2`;\nSELECT currentDatabase();") {
        s.execute(&u.text, 10, &mut out).await.unwrap();
    }
    assert_eq!(out.database.as_deref(), Some("dbine_use_db2"));
    assert_eq!(out.results.last().unwrap().rows[0][0], serde_json::json!("dbine_use_db2"));
    let mut out = QueryOutcome::default();
    s.execute("USE default", 10, &mut out).await.unwrap();
    s.execute("SELECT currentDatabase()", 10, &mut out).await.unwrap();
    assert_eq!(out.results.last().unwrap().rows[0][0], serde_json::json!("default"));
    s.execute("DROP DATABASE dbine_use_db2", 10, &mut go).await.unwrap();
}
