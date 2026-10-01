//! Against a real server:
//! `docker run -d --name dbine-test-cassandra -p 25402:9042 cassandra:5` (wait for
//! "Starting listening for CQL clients"), then
//! `DBINE_TEST_CASSANDRA_URL=localhost:25402 cargo test -p dbine-driver-cassandra -- --ignored`.
//! `DBINE_TEST_SCYLLADB_URL` runs the same test against ScyllaDB (which also
//! gets a materialized view).

use dbine_driver::{kinds, ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};
use serde_json::json;

fn cfg(driver: &str, url: &str, read_only: bool) -> ConnectionConfig {
    let (host, port) = url.rsplit_once(':').unwrap();
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().unwrap(),
        read_only,
        ..Default::default()
    }
}

async fn open(driver: &str, url: &str, ks: Option<&str>, read_only: bool) -> Box<dyn Session> {
    let d = dbine_driver_cassandra::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    d.connect(&cfg(driver, url, read_only), ks).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, text: &str, max_rows: usize) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(text, max_rows, &mut out).await {
        panic!("{text}: {e}");
    }
    out
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: None, name: name.into() }
}

async fn round_trip(driver: &str, url: &str) {
    let mut s = open(driver, url, None, false).await;
    println!("{driver}: {}", s.server_version().await.unwrap());
    // Scylla keeps materialized views off tablet keyspaces.
    let tablets = if driver == "scylladb" { " AND tablets = {'enabled': false}" } else { "" };
    run(
        &mut s,
        &format!(
            "DROP KEYSPACE IF EXISTS dbine_it;
             CREATE KEYSPACE dbine_it WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}{tablets};"
        ),
        10,
    )
    .await;
    let dbs = s.list_databases().await.unwrap();
    assert!(dbs.contains(&"dbine_it".to_string()), "{dbs:?}");
    assert!(!dbs.iter().any(|d| d.starts_with("system")));
    assert!(s.list_objects().await.unwrap().is_empty());

    let mut s = open(driver, url, Some("dbine_it"), false).await;
    run(
        &mut s,
        "CREATE TYPE address (city text, zip int);
         CREATE TABLE events (
             tenant text, day date, at timestamp, id uuid, n bigint, price decimal, ratio float,
             tags set<text>, attrs map<text, int>, addr frozen<address>, raw blob, big varint,
             PRIMARY KEY ((tenant, day), at, id)
         ) WITH CLUSTERING ORDER BY (at DESC, id ASC);
         // comment; with a semicolon
         BEGIN BATCH
           INSERT INTO events (tenant, day, at, id, n, price, ratio, tags, attrs, addr, raw, big)
             VALUES ('acme', '2024-01-31', '2024-01-31 13:45:00+0000', 5a1c395e-b5d1-4ec5-9e1a-6f4a2f5f3e10,
                     9007199254740993, 12.34, 1.5, {'a', 'b'}, {'x': 1}, {city: 'Rosario', zip: 2000}, 0xcafe,
                     123456789012345678901234567890);
           INSERT INTO events (tenant, day, at, id, n) VALUES ('acme', '2024-01-31', '2024-01-31 13:46:00+0000', uuid(), 2);
         APPLY BATCH;
         INSERT INTO events (tenant, day, at, id, n) VALUES ('acme', '2024-01-31', '2024-01-31 13:47:00+0000', uuid(), 3);",
        10,
    )
    .await;
    if driver == "scylladb" {
        run(
            &mut s,
            "CREATE MATERIALIZED VIEW events_by_n AS SELECT * FROM events
             WHERE n IS NOT NULL AND tenant IS NOT NULL AND day IS NOT NULL AND at IS NOT NULL AND id IS NOT NULL
             PRIMARY KEY (n, tenant, day, at, id)",
            10,
        )
        .await;
    }

    let objs = s.list_objects().await.unwrap();
    let kind_of = |n: &str| objs.iter().find(|o| o.name == n).map(|o| o.kind.clone());
    assert_eq!(kind_of("events").as_deref(), Some(kinds::TABLE));
    assert_eq!(kind_of("address").as_deref(), Some("type"));
    if driver == "scylladb" {
        assert_eq!(kind_of("events_by_n").as_deref(), Some(kinds::MATERIALIZED_VIEW));
    }

    let t = obj(kinds::TABLE, "events");
    let cols = s.columns(&t).await.unwrap();
    let names: Vec<_> = cols.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(&names[..4], ["tenant", "day", "at", "id"]);
    assert!(cols[..4].iter().all(|c| c.primary_key) && !cols[4].primary_key);
    let def = s.definition(&t).await.unwrap().unwrap();
    println!("{def}");
    assert!(def.contains("CREATE TABLE dbine_it.events") && def.contains("PRIMARY KEY ((tenant, day), at, id)"));
    let ty = s.columns(&obj("type", "address")).await.unwrap();
    assert_eq!(ty.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["city", "zip"]);
    println!("{}", s.definition(&obj("type", "address")).await.unwrap().unwrap());
    if driver == "scylladb" {
        let def = s.definition(&obj(kinds::MATERIALIZED_VIEW, "events_by_n")).await.unwrap().unwrap();
        println!("{def}");
        assert!(def.contains("MATERIALIZED VIEW"));
    }

    // Browse and value mapping.
    let q = s.browse_query(&t, 2);
    assert_eq!(q, "SELECT * FROM dbine_it.events LIMIT 2;");
    let out = run(&mut s, "SELECT tenant, day, at, id, n, price, ratio, tags, attrs, addr, raw, big FROM events WHERE tenant = 'acme' AND day = '2024-01-31' AND at = '2024-01-31 13:45:00+0000'", 100).await;
    let r = &out.results[0];
    assert_eq!(r.columns[7].type_name, "set<text>");
    let row = &r.rows[0];
    println!("{row:?}");
    assert_eq!(row[1], json!("2024-01-31"));
    assert_eq!(row[2], json!("2024-01-31 13:45:00"));
    assert_eq!(row[3], json!("5a1c395e-b5d1-4ec5-9e1a-6f4a2f5f3e10"));
    assert_eq!(row[4], json!("9007199254740993"));
    assert_eq!(row[5], json!("12.34"));
    assert_eq!(row[6], json!(1.5));
    assert_eq!(row[7], json!(r#"["a","b"]"#));
    assert_eq!(row[8], json!(r#"{"x":1}"#));
    assert_eq!(row[9], json!(r#"{"city":"Rosario","zip":2000}"#));
    assert_eq!(row[10], json!("0xCAFE"));
    assert_eq!(row[11], json!("123456789012345678901234567890"));

    // Paging stops at max_rows.
    let out = run(&mut s, &q, 100).await;
    assert_eq!(out.results[0].rows.len(), 2);
    let mut many = String::new();
    for i in 0..250 {
        many.push_str(&format!("INSERT INTO events (tenant, day, at, id, n) VALUES ('bulk', '2024-02-01', {i}, uuid(), {i});\n"));
    }
    run(&mut s, &many, 10).await;
    let out = run(&mut s, "SELECT * FROM events", 120).await;
    assert_eq!(out.results[0].rows.len(), 120);
    assert!(out.results[0].truncated);
    let out = run(&mut s, "SELECT count(*) FROM events", 10).await;
    assert_eq!(out.results[0].rows[0][0], json!(253));

    // Errors keep what ran before.
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT n FROM events LIMIT 1; SELECT nope FROM events", 10, &mut out).await.unwrap_err();
    assert!(e.is_query(), "{e:?}");
    assert_eq!(out.results.len(), 1);

    // USE switches the session's keyspace.
    let mut s0 = open(driver, url, None, false).await;
    run(&mut s0, "USE dbine_it", 10).await;
    assert!(!s0.list_objects().await.unwrap().is_empty());

    // Read-only.
    let mut ro = open(driver, url, Some("dbine_it"), true).await;
    run(&mut ro, "SELECT * FROM events LIMIT 1; USE dbine_it", 10).await;
    let mut out = QueryOutcome::default();
    let e = ro.execute("SELECT * FROM events LIMIT 1; TRUNCATE events", 10, &mut out).await.unwrap_err();
    assert!(matches!(e, Error::Query(ref m) if m.contains("TRUNCATE")));
    assert!(out.results.is_empty());

    // Nobody listening.
    let d = dbine_driver_cassandra::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(matches!(d.connect(&cfg(driver, "localhost:1", false), None).await, Err(Error::Connect(_))));

    run(&mut s, "DROP KEYSPACE dbine_it", 10).await;
}

#[tokio::test]
#[ignore]
async fn cassandra_round_trip() {
    let url = std::env::var("DBINE_TEST_CASSANDRA_URL").expect("DBINE_TEST_CASSANDRA_URL");
    round_trip("cassandra", &url).await;
}

#[tokio::test]
#[ignore]
async fn scylladb_round_trip() {
    let url = std::env::var("DBINE_TEST_SCYLLADB_URL").expect("DBINE_TEST_SCYLLADB_URL");
    round_trip("scylladb", &url).await;
}

async fn plans(driver: &str, url: &str) {
    let mut s = open(driver, url, None, false).await;
    run(
        &mut s,
        "CREATE KEYSPACE IF NOT EXISTS dbine_plan WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1};
         USE dbine_plan;
         DROP TABLE IF EXISTS ev;
         CREATE TABLE ev (tenant text, day int, ts int, v int, PRIMARY KEY ((tenant, day), ts));
         INSERT INTO ev (tenant, day, ts, v) VALUES ('a', 1, 1, 10);
         INSERT INTO ev (tenant, day, ts, v) VALUES ('a', 1, 2, 20)",
        10,
    )
    .await;

    let mut out = QueryOutcome::default();
    s.explain(
        "SELECT * FROM ev WHERE tenant = 'a' AND day = 1 AND ts > 0; SELECT * FROM ev WHERE v = 1 ALLOW FILTERING; \
         DELETE FROM ev WHERE tenant = 'a' AND day = 1",
        false,
        10,
        &mut out,
    )
    .await
    .unwrap();
    assert!(out.results.is_empty());
    assert_eq!(out.plans[0].root.op, "Lectura por clave de partición");
    assert_eq!(out.plans[0].root.object.as_deref(), Some("dbine_plan.ev"));
    assert_eq!(out.plans[1].root.warnings, ["ALLOW FILTERING: recorre todas las particiones"]);
    assert_eq!(out.plans[2].root.op, "Borrado por clave de partición");
    let o = run(&mut s, "SELECT count(*) FROM ev", 10).await;
    assert_eq!(o.results[0].rows[0][0], json!(2), "the DELETE didn't run");

    let mut out = QueryOutcome::default();
    s.explain("SELECT * FROM ev WHERE tenant = 'a' AND day = 1", true, 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2);
    let p = &out.plans[0];
    println!("{}", p.raw);
    assert!(p.actual);
    assert!(p.root.actual_ms.is_some());
    assert!(!p.root.children.is_empty() && !p.root.children[0].children.is_empty(), "{:#?}", p.root);

    run(&mut s, "DROP KEYSPACE dbine_plan", 10).await;
}

#[tokio::test]
#[ignore]
async fn scylladb_plans() {
    let Ok(url) = std::env::var("DBINE_TEST_SCYLLADB_URL") else { return };
    plans("scylladb", &url).await;
}

#[tokio::test]
#[ignore]
async fn cassandra_plans() {
    let Ok(url) = std::env::var("DBINE_TEST_CASSANDRA_URL") else { return };
    plans("cassandra", &url).await;
}

/// Keyspace create/drop, a table from the designer, read back through
/// `database_schema`, regenerated from it, templates and insert scripts,
/// all through `execute`.
async fn designer(driver: &str, url: &str) {
    use dbine_driver::{ColumnDef, DdlParts, IndexDef, TableSchema};
    let d = dbine_driver_cassandra::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    let caps = d.capabilities();
    assert!(caps.create_database && caps.drop_database && !caps.foreign_keys);

    let mut s = open(driver, url, None, false).await;
    let _ = s.drop_database("dbine_ddl").await;
    s.create_database("dbine_ddl").await.unwrap();
    assert!(s.create_database("bad-name").await.is_err());
    assert!(s.drop_database("system_schema").await.is_err());
    assert!(s.list_databases().await.unwrap().contains(&"dbine_ddl".to_string()));

    let mut s = open(driver, url, Some("dbine_ddl"), false).await;
    let c = |name: &str, ty: &str, opts: &[(&str, &str)]| ColumnDef {
        name: name.into(),
        data_type: ty.into(),
        options: opts.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        ..Default::default()
    };
    let sai = if driver == "cassandra" { Some("sai".to_string()) } else { None };
    let compaction = if driver == "cassandra" { "UnifiedCompactionStrategy" } else { "IncrementalCompactionStrategy" };
    let mut t = TableSchema {
        name: "Events".into(),
        columns: vec![
            c("tenant", "text", &[("partition_key", "true")]),
            c("day", "date", &[("partition_key", "true")]),
            c("ts", "timestamp", &[("clustering_key", "true"), ("clustering_order", "DESC")]),
            c("id", "uuid", &[("clustering_key", "true")]),
            c("region", "text", &[("static", "true")]),
            c("Name", "text", &[]),
            c("amount", "decimal", &[]),
            c("tags", "set<text>", &[]),
            c("attrs", "map<text, int>", &[]),
            c("raw", "blob", &[]),
        ],
        indexes: vec![
            IndexDef { name: "events_name".into(), columns: vec!["Name".into()], kind: sai.clone(), ..Default::default() },
            IndexDef { name: "events_tags".into(), columns: vec!["values(tags)".into()], ..Default::default() },
        ],
        ..Default::default()
    };
    t.options.insert("default_time_to_live".into(), "86400".into());
    t.options.insert("gc_grace_seconds".into(), "3600".into());
    t.options.insert("compaction".into(), compaction.into());
    t.options.insert("comment".into(), "it's a log".into());
    let all = DdlParts { drop: true, if_exists: true, create: true, indexes: true, foreign_keys: true };
    let ddl = d.table_ddl(&t, all).unwrap();
    println!("{ddl}");
    run(&mut s, &ddl, 10).await;

    let schema = s.database_schema().await.unwrap();
    let got = schema.iter().find(|x| x.name == "Events").expect("Events");
    let names: Vec<_> = got.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(&names[..4], ["tenant", "day", "ts", "id"]);
    assert_eq!(got.primary_key.as_ref().unwrap().columns, ["tenant", "day", "ts", "id"]);
    let opt = |col: &str, k: &str| got.columns.iter().find(|c| c.name == col).unwrap().options.get(k).cloned();
    assert_eq!(opt("day", "partition_key").as_deref(), Some("true"));
    assert_eq!(opt("ts", "clustering_order").as_deref(), Some("DESC"));
    assert_eq!(opt("id", "clustering_order").as_deref(), Some("ASC"));
    assert_eq!(opt("region", "static").as_deref(), Some("true"));
    assert_eq!(got.columns.iter().find(|c| c.name == "tags").unwrap().data_type, "set<text>");
    assert_eq!(got.options.get("default_time_to_live").map(String::as_str), Some("86400"));
    assert_eq!(got.options.get("gc_grace_seconds").map(String::as_str), Some("3600"));
    assert_eq!(got.options.get("compaction").map(String::as_str), Some(compaction));
    assert_eq!(got.options.get("comment").map(String::as_str), Some("it's a log"));
    let mut ix = got.indexes.clone();
    ix.sort_by(|a, b| a.name.cmp(&b.name));
    assert_eq!(ix.len(), 2, "{ix:?}");
    assert_eq!(ix[0].kind, sai);
    assert_eq!(ix[1].columns, ["values(tags)"]);

    // Regenerated from what the server reports, it runs again.
    let again = d.table_ddl(got, all).unwrap();
    println!("{again}");
    run(&mut s, &again, 10).await;

    // Rows through INSERT … JSON, with the driver's own cell shapes.
    let cols: Vec<String> =
        ["tenant", "day", "ts", "id", "region", "Name", "amount", "tags", "attrs", "raw"].iter().map(|c| c.to_string()).collect();
    let rows = vec![
        vec![
            json!("acme"),
            json!("2024-01-02"),
            json!("2024-01-02 03:04:05.123"),
            json!("8b2b8a52-0a55-4b3f-9c0e-3c5f4f9b1d11"),
            json!("sur"),
            json!("O'Brien; -- x"),
            json!("12.50"),
            json!("[\"a\",\"b\"]"),
            json!("{\"x\":1}"),
            json!("0x0102"),
        ],
        vec![
            json!("acme"),
            json!("2024-01-02"),
            json!("2024-01-02 03:04:06"),
            json!("1c7e3a52-0a55-4b3f-9c0e-3c5f4f9b1d11"),
            serde_json::Value::Null,
            json!("Ana"),
            json!(3),
            serde_json::Value::Null,
            serde_json::Value::Null,
            serde_json::Value::Null,
        ],
    ];
    let target = obj(kinds::TABLE, "Events");
    let script = d.insert_script(&target, &cols, &rows).unwrap();
    run(&mut s, &script, 10).await;
    let out = run(&mut s, "SELECT \"Name\", amount, tags, attrs, raw, ts FROM \"Events\" WHERE tenant = 'acme' AND day = '2024-01-02'", 10).await;
    let r = &out.results[0].rows;
    assert_eq!(r.len(), 2);
    // DESC clustering: the later timestamp first.
    assert_eq!(r[0][0], json!("Ana"));
    assert_eq!(r[1][0], json!("O'Brien; -- x"));
    assert_eq!(r[1][1], json!("12.50"));
    assert_eq!(r[1][2], json!("[\"a\",\"b\"]"));
    assert_eq!(r[1][3], json!("{\"x\":1}"));
    assert_eq!(r[1][4], json!("0x0102"));
    assert_eq!(r[1][5], json!("2024-01-02 03:04:05.123"));
    // The rows as the browse query reads them, scripted and loaded again.
    let browse = s.browse_query(&target, 100);
    let out = run(&mut s, &browse, 100).await;
    let cols: Vec<String> = out.results[0].columns.iter().map(|c| c.name.clone()).collect();
    run(&mut s, "TRUNCATE \"Events\"", 10).await;
    run(&mut s, &d.insert_script(&target, &cols, &out.results[0].rows).unwrap(), 10).await;
    let back = run(&mut s, &browse, 100).await;
    assert_eq!(back.results[0].rows, out.results[0].rows);

    // Templates, against a table with the names they use. Stock Cassandra
    // has UDFs and materialized views off in cassandra.yaml.
    run(&mut s, "CREATE TABLE tabla (id int PRIMARY KEY, email text, columna text)", 10).await;
    for t in d.create_templates() {
        let text = t.template.replace("{name}", "tpl");
        let mut out = QueryOutcome::default();
        let res = s.execute(&text, 10, &mut out).await;
        println!("{}: {res:?}", t.label);
        if driver == "cassandra" && (t.kind == kinds::FUNCTION || t.kind == kinds::MATERIALIZED_VIEW) {
            assert!(matches!(res, Err(ref e) if e.is_query() && e.to_string().contains("cassandra.yaml")), "{res:?}");
            continue;
        }
        res.unwrap_or_else(|e| panic!("{}: {e}\n{text}", t.label));
        match t.kind {
            kinds::INDEX => run(&mut s, "DROP INDEX tpl", 10).await,
            kinds::MATERIALIZED_VIEW => run(&mut s, "DROP MATERIALIZED VIEW tpl", 10).await,
            _ => QueryOutcome::default(),
        };
    }

    // Only DROP.
    run(&mut s, &d.table_ddl(&t, DdlParts { drop: true, if_exists: true, ..Default::default() }).unwrap(), 10).await;
    assert!(!s.database_schema().await.unwrap().iter().any(|x| x.name == "Events"));
    let mut s = open(driver, url, None, false).await;
    s.drop_database("dbine_ddl").await.unwrap();
    assert!(!s.list_databases().await.unwrap().contains(&"dbine_ddl".to_string()));
}

#[tokio::test]
#[ignore]
async fn cassandra_designer() {
    let url = std::env::var("DBINE_TEST_CASSANDRA_URL").expect("DBINE_TEST_CASSANDRA_URL");
    designer("cassandra", &url).await;
}

#[tokio::test]
#[ignore]
async fn scylladb_designer() {
    let url = std::env::var("DBINE_TEST_SCYLLADB_URL").expect("DBINE_TEST_SCYLLADB_URL");
    designer("scylladb", &url).await;
}

async fn monitor(driver: &str, url: &str) {
    let d = dbine_driver_cassandra::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(d.capabilities().monitor);
    let mut s = open(driver, url, None, false).await;
    let snap = s.monitor().await.expect("monitor");
    for m in &snap.metrics {
        eprintln!("{:<22} {:?} max={:?} counter={}", m.key, m.value, m.max, m.counter);
    }
    for t in &snap.tables {
        eprintln!("table {} rows={}", t.key, t.rows.len());
    }
    eprintln!("info {:?}\nnotes {:?}", snap.info, snap.notes);
    let has = |k: &str| snap.metrics.iter().any(|m| m.key == k && m.value.is_some());
    for k in ["connections", "cache_hit", "storage_used", "uptime"] {
        assert!(has(k), "{driver}: {k}");
    }
    let table = |k: &str| snap.tables.iter().find(|t| t.key == k);
    assert!(table("sessions").is_some_and(|t| !t.rows.is_empty()), "{driver}: sessions");
    assert!(table("nodes").is_some_and(|t| !t.rows.is_empty()), "{driver}: nodes");
    if driver == "scylladb" {
        assert!(has("mem_used"));
    } else {
        assert!(has("queries") && has("rows_read"));
        assert!(table("databases").is_some_and(|t| !t.rows.is_empty()));
    }
}

#[tokio::test]
#[ignore]
async fn cassandra_monitor() {
    let Ok(url) = std::env::var("DBINE_TEST_CASSANDRA_URL") else { return };
    monitor("cassandra", &url).await;
}

#[tokio::test]
#[ignore]
async fn scylladb_monitor() {
    let Ok(url) = std::env::var("DBINE_TEST_SCYLLADB_URL") else { return };
    monitor("scylladb", &url).await;
}

/// One session profiles `dbine_prof`, the other runs a slow statement and a
/// fast one carrying a unique marker. Cassandra samples what runs (the slow
/// one, a full scan, must last); Scylla reads its audit log (both, and its
/// settings are back as they were at stop).
async fn profile(driver: &str, url: &str) {
    use std::time::{Duration, Instant};
    let d = dbine_driver_cassandra::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(d.supports_profiler());
    let mut w = open(driver, url, None, false).await;
    run(
        &mut w,
        "CREATE KEYSPACE IF NOT EXISTS dbine_prof WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1};
         CREATE TABLE IF NOT EXISTS dbine_prof.big (id int PRIMARY KEY, v text);",
        10,
    )
    .await;
    if driver == "cassandra" {
        // Enough rows for a filtered count to take a while.
        for b in 0..1000 {
            let rows: String =
                (0..200).map(|i| format!("INSERT INTO dbine_prof.big (id, v) VALUES ({}, 'value {i}');\n", b * 200 + i)).collect();
            run(&mut w, &format!("BEGIN UNLOGGED BATCH\n{rows}APPLY BATCH;"), 10).await;
        }
    }
    let config = "SELECT name, value FROM system.config WHERE name IN ('audit_categories', 'audit_keyspaces')";
    let before = if driver == "scylladb" { Some(run(&mut w, config, 10).await.results) } else { None };
    let mut p = open(driver, url, None, false).await;
    let opts = dbine_driver::ProfilerOptions { database: "dbine_prof".into(), change_server: true };
    let started = p.profiler_start(&opts).await.expect("profiler_start");
    eprintln!("{driver}: {started:?}");
    let complete = started.mode == dbine_driver::ProfilerMode::Complete;
    let marker = format!("dbine_prof_{}", std::process::id());
    let slow = format!("SELECT count(*) FROM dbine_prof.big WHERE v = '{marker}_slow' ALLOW FILTERING");
    let fast = format!("SELECT * FROM dbine_prof.big WHERE id = 1 AND v = '{marker}_fast' ALLOW FILTERING");
    let work = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let t = Instant::now();
        run(&mut w, &slow, 10).await;
        eprintln!("{driver}: slow took {:?}", t.elapsed());
        tokio::time::sleep(Duration::from_millis(400)).await;
        run(&mut w, &fast, 10).await;
        tokio::time::sleep(Duration::from_millis(600)).await;
    };
    let watch = async {
        let mut got = Vec::new();
        let until = Instant::now() + Duration::from_secs(if complete { 5 } else { 4 });
        while Instant::now() < until {
            got.extend(p.profiler_poll().await.expect("profiler_poll"));
            if complete {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        got
    };
    let ((), got) = tokio::join!(work, watch);
    p.profiler_stop().await.expect("profiler_stop");
    let mine: Vec<_> = got.iter().filter(|s| s.text.contains(&marker)).collect();
    eprintln!("{driver}: {mine:#?}");
    let slow_seen: Vec<_> = mine.iter().filter(|s| s.text.contains("_slow")).collect();
    assert_eq!(slow_seen.len(), 1, "{driver}: the slow statement once");
    assert!(slow_seen[0].text.starts_with("SELECT count(*)"), "{}", slow_seen[0].text);
    if complete {
        assert_eq!(mine.iter().filter(|s| s.text.contains("_fast")).count(), 1, "{driver}: the fast statement once");
        assert_eq!(slow_seen[0].database.as_deref(), Some("dbine_prof"));
    } else {
        assert!(slow_seen[0].duration_ms.unwrap_or(0.0) >= 50.0, "{driver}: duration {:?}", slow_seen[0].duration_ms);
    }
    assert!(got.iter().all(|s| !s.text.contains("dbine profiler")), "{driver}: its own statements are left out");
    if let Some(before) = before {
        let rows = |r: &[dbine_driver::StatementResult]| r.iter().map(|r| r.rows.clone()).collect::<Vec<_>>();
        assert_eq!(rows(&before), rows(&run(&mut w, config, 10).await.results), "settings restored");
    }
    run(&mut w, "DROP KEYSPACE dbine_prof", 10).await;
}

#[tokio::test]
#[ignore]
async fn cassandra_profiler() {
    let Ok(url) = std::env::var("DBINE_TEST_CASSANDRA_URL") else { return };
    profile("cassandra", &url).await;
}

#[tokio::test]
#[ignore]
async fn scylladb_profiler() {
    let Ok(url) = std::env::var("DBINE_TEST_SCYLLADB_URL") else { return };
    profile("scylladb", &url).await;
}

/// Schema sync: a table read back from `database_schema` is changed (column
/// added, dropped, retyped; index swapped; TTL) and the generated script runs.
async fn schema_sync(driver: &str, url: &str) {
    use dbine_driver::{IndexDef, TableChange};
    let d = dbine_driver_cassandra::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(d.supports_schema_sync());
    let mut s = open(driver, url, None, false).await;
    let _ = s.drop_database("dbine_sync").await;
    s.create_database("dbine_sync").await.unwrap();
    let mut s = open(driver, url, Some("dbine_sync"), false).await;
    run(
        &mut s,
        "CREATE TABLE users (id uuid PRIMARY KEY, name text, age int, legacy text);
         CREATE INDEX users_legacy ON users (legacy);
         CREATE TABLE gone (id int PRIMARY KEY);",
        10,
    )
    .await;
    let schema = s.database_schema().await.unwrap();
    let old = schema.iter().find(|t| t.name == "users").unwrap().clone();
    let gone = schema.iter().find(|t| t.name == "gone").unwrap().clone();
    let mut new = old.clone();
    new.columns.retain(|c| c.name != "legacy");
    new.columns.iter_mut().find(|c| c.name == "age").unwrap().data_type = "bigint".into();
    new.columns.push(dbine_driver::ColumnDef { name: "Email".into(), data_type: "text".into(), ..Default::default() });
    new.indexes = vec![IndexDef { name: "users_email".into(), columns: vec!["Email".into()], ..Default::default() }];
    new.options.insert("default_time_to_live".into(), "3600".into());
    let mut created = old.clone();
    created.name = "fresh".into();
    created.indexes.clear();

    let script = d
        .sync_script(&[TableChange::Alter { old, new }, TableChange::Drop { table: gone }, TableChange::Create { table: created }])
        .unwrap();
    println!("{driver}: {script:#?}");
    assert!(script.warnings.iter().any(|w| w.contains("age: int → bigint")), "{:?}", script.warnings);
    for st in &script.statements {
        run(&mut s, st, 10).await;
    }
    let after = s.database_schema().await.unwrap();
    assert!(!after.iter().any(|t| t.name == "gone"));
    assert!(after.iter().any(|t| t.name == "fresh"));
    let users = after.iter().find(|t| t.name == "users").unwrap();
    let cols: Vec<&str> = users.columns.iter().map(|c| c.name.as_str()).collect();
    assert!(cols.contains(&"Email") && !cols.contains(&"legacy"), "{cols:?}");
    assert_eq!(users.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), vec!["users_email"]);
    assert_eq!(users.options.get("default_time_to_live").map(String::as_str), Some("3600"));
    s.drop_database("dbine_sync").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn cassandra_schema_sync() {
    let url = std::env::var("DBINE_TEST_CASSANDRA_URL").expect("DBINE_TEST_CASSANDRA_URL");
    schema_sync("cassandra", &url).await;
}

#[tokio::test]
#[ignore]
async fn scylladb_schema_sync() {
    let url = std::env::var("DBINE_TEST_SCYLLADB_URL").expect("DBINE_TEST_SCYLLADB_URL");
    schema_sync("scylladb", &url).await;
}
