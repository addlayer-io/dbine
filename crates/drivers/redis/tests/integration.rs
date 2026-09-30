//! Against a real server:
//! `docker run -d --name dbine-test-redis -p 25400:6379 redis:7`, then
//! `DBINE_TEST_REDIS_URL=redis://localhost:25400 cargo test -p dbine-driver-redis -- --ignored`.
//! `DBINE_TEST_VALKEY_URL` / `DBINE_TEST_DRAGONFLY_URL` run the same test
//! against those servers.

use dbine_driver::{kinds, ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};

fn cfg(driver: &str, url: &str, read_only: bool) -> ConnectionConfig {
    let rest = url.trim_start_matches("redis://");
    let (host, port) = rest.split_once(':').unwrap_or((rest, "6379"));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().unwrap(),
        read_only,
        ..Default::default()
    }
}

async fn open(driver: &str, url: &str, db: &str, read_only: bool) -> Box<dyn Session> {
    let d = dbine_driver_redis::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    d.connect(&cfg(driver, url, read_only), Some(db)).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, text: &str, max_rows: usize) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(text, max_rows, &mut out).await.unwrap();
    out
}

fn key(name: &str) -> ObjectRef {
    ObjectRef { kind: kinds::KEY.into(), schema: None, name: name.into() }
}

async fn round_trip(driver: &str, url: &str) {
    let mut s = open(driver, url, "db5", false).await;
    let v = s.server_version().await.unwrap();
    println!("{driver}: {v}");
    let dbs = s.list_databases().await.unwrap();
    assert!(dbs.len() >= 16 && dbs[5] == "db5");

    run(
        &mut s,
        "FLUSHDB\n\
         # fixtures\n\
         SET str \"hello world\"\n\
         HSET user:1 name Ana age 30\n\
         RPUSH list a b c d e\n\
         SADD tags x y z\n\
         ZADD board 1.5 alice 3 bob\n\
         XADD events 1-0 kind login\n\
         XADD events 2-0 kind logout user ana\n\
         SET \"key with space\" 1",
        100,
    )
    .await;

    let objs = s.list_objects().await.unwrap();
    let names: Vec<_> = objs.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, ["board", "events", "key with space", "list", "str", "tags", "user:1"]);
    assert!(objs.iter().all(|o| o.kind == kinds::KEY));

    // Columns and definition per type.
    let cols = |c: Vec<dbine_driver::ColumnInfo>| c.into_iter().map(|c| c.name).collect::<Vec<_>>();
    assert_eq!(cols(s.columns(&key("user:1")).await.unwrap()), ["field", "value"]);
    assert_eq!(cols(s.columns(&key("board")).await.unwrap()), ["member", "score"]);
    assert_eq!(cols(s.columns(&key("events")).await.unwrap()), ["id", "kind", "user"]);
    let def = s.definition(&key("list")).await.unwrap().unwrap();
    println!("{def}");
    assert!(def.contains("TYPE      list") && def.contains("LENGTH    5"));
    assert!(s.definition(&key("missing")).await.unwrap().is_none());

    // Browse queries run and have the documented shapes.
    let browse = |s: &Box<dyn Session>, k: &str| s.browse_query(&key(k), 3);
    assert_eq!(browse(&s, "str"), "GET str");
    assert_eq!(browse(&s, "key with space"), "GET \"key with space\"");
    let q = browse(&s, "list");
    assert_eq!(q, "LRANGE list 0 2");
    let out = run(&mut s, &q, 100).await;
    assert_eq!(out.results[0].rows.len(), 3);
    let q = browse(&s, "user:1");
    let out = run(&mut s, &q, 100).await;
    assert_eq!(out.results[0].columns[0].name, "field");
    assert_eq!(out.results[0].rows.len(), 2);
    let q = browse(&s, "board");
    let out = run(&mut s, &q, 100).await;
    assert_eq!(out.results[0].rows[0], vec![serde_json::json!("alice"), serde_json::json!(1.5)]);
    let q = browse(&s, "events");
    let out = run(&mut s, &q, 100).await;
    assert_eq!(out.results[0].columns.len(), 3);
    assert_eq!(out.results[0].rows[1][2], serde_json::json!("ana"));
    let q = browse(&s, "tags");
    let out = run(&mut s, &q, 100).await;
    assert_eq!(out.results[0].rows.len(), 3);
    assert!(out.messages[0].contains("cursor"));

    // Scripts: several results, nil, INFO, max_rows.
    let out = run(&mut s, "GET nope\nDBSIZE\nINFO server", 100).await;
    assert_eq!(out.results.len(), 3);
    assert_eq!(out.results[0].rows[0][0], serde_json::Value::Null);
    assert_eq!(out.results[1].columns[0].name, "result");
    assert_eq!(out.results[2].columns.len(), 3);
    let out = run(&mut s, "LRANGE list 0 -1", 2).await;
    assert_eq!(out.results[0].rows.len(), 2);
    assert_eq!(out.results[0].total_rows, 5);
    assert!(out.results[0].truncated);

    // A failing command stops the script; what ran before stays.
    let mut out = QueryOutcome::default();
    let e = s.execute("GET str\nHGETALL str\nGET str", 100, &mut out).await.unwrap_err();
    assert!(matches!(e, Error::Query(ref m) if m.contains("WRONGTYPE")), "{e:?}");
    assert_eq!(out.results.len(), 1);
    let mut out = QueryOutcome::default();
    assert!(matches!(s.execute("GET \"open", 100, &mut out).await, Err(Error::Query(_))));
    assert!(matches!(s.execute("SUBSCRIBE ch", 100, &mut out).await, Err(Error::Unsupported(_))));

    // Read-only: reads pass, writes are refused before reaching the server.
    let mut ro = open(driver, url, "5", true).await;
    let out = run(&mut ro, "GET str\nHGETALL user:1\nCONFIG GET databases", 100).await;
    assert_eq!(out.results[0].rows[0][0], serde_json::json!("hello world"));
    let mut out = QueryOutcome::default();
    let e = ro.execute("GET str\nDEL str", 100, &mut out).await.unwrap_err();
    assert!(matches!(e, Error::Query(ref m) if m.contains("DEL")));
    assert!(out.results.is_empty());
    assert_eq!(run(&mut s, "EXISTS str", 10).await.results[0].rows[0][0], serde_json::json!(1));

    // Wrong password.
    let d = dbine_driver_redis::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    let mut bad = cfg(driver, url, false);
    bad.password = Some("nope".into());
    bad.username = Some("nobody".into());
    match d.connect(&bad, None).await {
        Err(Error::AuthFailed(_)) => {}
        Err(e) => panic!("expected AuthFailed, got {e:?}"),
        Ok(_) => panic!("expected AuthFailed, connected"),
    }
    // Nobody listening.
    let mut off = cfg(driver, "localhost:1", false);
    off.port = 1;
    assert!(matches!(d.connect(&off, None).await, Err(Error::Connect(_))));

    designer_and_scripts(driver, &mut s).await;
    run(&mut s, "FLUSHDB", 10).await;
}

/// Keys from the designer, the templates and insert scripts, all run
/// through `execute`.
async fn designer_and_scripts(driver: &str, s: &mut Box<dyn Session>) {
    use dbine_driver::{ColumnDef, DdlParts, TableSchema};
    let d = dbine_driver_redis::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    let caps = d.capabilities();
    assert!(!caps.create_database && !caps.drop_database && !caps.foreign_keys);
    assert_eq!(d.designer().unwrap().kind, kinds::KEY);
    run(s, "FLUSHDB", 10).await;

    let entry = |n: &str, v: Option<&str>| ColumnDef { name: n.into(), default_value: v.map(str::to_string), ..Default::default() };
    let make = |name: &str, ty: &str, cols: Vec<ColumnDef>, opts: &[(&str, &str)]| {
        let mut t = TableSchema { kind: kinds::KEY.into(), name: name.into(), columns: cols, ..Default::default() };
        t.options.insert("type".into(), ty.into());
        for (k, v) in opts {
            t.options.insert(k.to_string(), v.to_string());
        }
        t
    };
    let mut keys = vec![
        (make("d:string", "string", vec![], &[("value", "hola \"mundo\"\nfin"), ("ttl", "300")]), "string"),
        (make("d:hash one", "hash", vec![entry("name", Some("Ana")), entry("age", Some("30"))], &[]), "hash"),
        (make("d:list", "list", vec![entry("a", None), entry("x", Some("b c"))], &[]), "list"),
        (make("d:set", "set", vec![entry("x", None), entry("y", None)], &[]), "set"),
        (make("d:zset", "zset", vec![entry("ana", Some("1.5")), entry("luis", Some("3"))], &[]), "zset"),
        (make("d:stream", "stream", vec![entry("evento", Some("alta"))], &[]), "stream"),
    ];
    // JSON is built into Dragonfly; plain redis:7 and valkey images lack the module.
    if driver == "dragonfly" {
        keys.push((make("d:json", "json", vec![], &[("value", "{\"a\": [1, 2], \"b\": \"it's\"}")]), "ReJSON-RL"));
    }
    let both = DdlParts { drop: true, create: true, ..Default::default() };
    for (t, ty) in &keys {
        let ddl = d.table_ddl(t, both).unwrap();
        run(s, &ddl, 10).await;
        // Twice: DROP first makes it repeatable.
        run(s, &ddl, 10).await;
        let out = run(s, &format!("TYPE {}", command_arg(&t.name)), 10).await;
        assert_eq!(out.results[0].rows[0][0], serde_json::json!(ty), "{ddl}");
    }
    let out = run(s, "GET d:string\nTTL d:string\nHGET \"d:hash one\" name\nLRANGE d:list 0 -1\nZSCORE d:zset ana\nXLEN d:stream", 10).await;
    assert_eq!(out.results[0].rows[0][0], serde_json::json!("hola \"mundo\"\nfin"));
    assert!(out.results[1].rows[0][0].as_i64().unwrap() > 0);
    assert_eq!(out.results[2].rows[0][0], serde_json::json!("Ana"));
    assert_eq!(out.results[3].rows.len(), 2);
    assert_eq!(out.results[3].rows[1][0], serde_json::json!("b c"));
    assert_eq!(out.results[5].rows[0][0], serde_json::json!(1));
    if driver == "dragonfly" {
        let out = run(s, "JSON.GET d:json $.b", 10).await;
        println!("json: {:?}", out.results[0].rows);
    }
    let drop = d.table_ddl(&keys[0].0, DdlParts { drop: true, ..Default::default() }).unwrap();
    run(s, &drop, 10).await;
    assert_eq!(run(s, "EXISTS d:string", 10).await.results[0].rows[0][0], serde_json::json!(0));

    // No tables to draw.
    assert!(s.database_schema().await.unwrap().is_empty());

    // Templates run as they are.
    for (i, t) in d.create_templates().iter().enumerate() {
        let text = t.template.replace("{name}", &format!("tpl:{i}"));
        let mut out = QueryOutcome::default();
        if let Err(e) = s.execute(&text, 100, &mut out).await {
            panic!("{}: {e}\n{text}", t.label);
        }
    }

    // Rows as hashes.
    let cols = vec!["id".to_string(), "name".into(), "score".into()];
    let rows = vec![
        vec![serde_json::json!(1), serde_json::json!("Ana \"A\""), serde_json::json!(9.5)],
        vec![serde_json::json!("x y"), serde_json::Value::Null, serde_json::json!(true)],
    ];
    let script = d.insert_script(&key("person"), &cols, &rows).unwrap();
    run(s, &script, 10).await;
    let out = run(s, "HGETALL person:1\nHGETALL \"person:x y\"", 10).await;
    assert_eq!(out.results[0].rows.len(), 3);
    assert!(out.results[0].rows.contains(&vec![serde_json::json!("name"), serde_json::json!("Ana \"A\"")]));
    assert_eq!(out.results[1].rows.len(), 2);
}

fn command_arg(s: &str) -> String {
    if s.contains(' ') { format!("\"{s}\"") } else { s.to_string() }
}

#[tokio::test]
#[ignore]
async fn redis_round_trip() {
    let url = std::env::var("DBINE_TEST_REDIS_URL").expect("DBINE_TEST_REDIS_URL");
    round_trip("redis", &url).await;
}

#[tokio::test]
#[ignore]
async fn valkey_round_trip() {
    let url = std::env::var("DBINE_TEST_VALKEY_URL").expect("DBINE_TEST_VALKEY_URL");
    round_trip("valkey", &url).await;
}

#[tokio::test]
#[ignore]
async fn dragonfly_round_trip() {
    let url = std::env::var("DBINE_TEST_DRAGONFLY_URL").expect("DBINE_TEST_DRAGONFLY_URL");
    round_trip("dragonfly", &url).await;
}

async fn monitor(driver: &str, url: &str) {
    let mut s = open(driver, url, "db0", false).await;
    run(&mut s, "SET monitor_probe 1", 10).await;
    run(&mut s, "GET monitor_probe", 10).await;
    let snap = s.monitor().await.expect("monitor");
    for m in &snap.metrics {
        eprintln!("{:<22} {:?} max={:?} counter={}", m.key, m.value, m.max, m.counter);
    }
    for t in &snap.tables {
        eprintln!("table {} rows={}", t.key, t.rows.len());
    }
    eprintln!("info {:?}\nnotes {:?}", snap.info, snap.notes);
    let has = |k: &str| snap.metrics.iter().any(|m| m.key == k && m.value.is_some());
    for k in ["cpu_time", "mem_used", "connections", "queries", "net_in", "keys", "uptime"] {
        assert!(has(k), "{driver}: {k}");
    }
    let table = |k: &str| snap.tables.iter().find(|t| t.key == k);
    assert!(table("sessions").is_some_and(|t| !t.rows.is_empty()), "{driver}: sessions");
    assert!(table("databases").is_some_and(|t| !t.rows.is_empty()), "{driver}: databases");
    run(&mut s, "DEL monitor_probe", 10).await;
}

#[tokio::test]
#[ignore]
async fn redis_monitor() {
    let Ok(url) = std::env::var("DBINE_TEST_REDIS_URL") else { return };
    monitor("redis", &url).await;
}

#[tokio::test]
#[ignore]
async fn valkey_monitor() {
    let Ok(url) = std::env::var("DBINE_TEST_VALKEY_URL") else { return };
    monitor("valkey", &url).await;
}

#[tokio::test]
#[ignore]
async fn dragonfly_monitor() {
    let Ok(url) = std::env::var("DBINE_TEST_DRAGONFLY_URL") else { return };
    monitor("dragonfly", &url).await;
}

/// A read-only session profiles db1 (MONITOR changes nothing), another runs
/// commands with a unique marker there and in db2: each db1 command is
/// seen once, db2's never, the profiler's own never, and the monitoring
/// connection is gone after stop.
async fn profile(driver: &str, url: &str) {
    use std::time::{Duration, Instant};
    let d = dbine_driver_redis::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(d.supports_profiler());
    let mut p = open(driver, url, "db1", true).await;
    let mut w = open(driver, url, "db1", false).await;
    let mut other = open(driver, url, "db2", false).await;
    let opts = dbine_driver::ProfilerOptions { database: "db1".into(), change_server: false };
    let started = p.profiler_start(&opts).await.expect("profiler_start");
    eprintln!("{driver}: {started:?}");
    let marker = format!("dbine_prof_{}", std::process::id());
    run(&mut w, &format!("SET {marker}_a \"x y\""), 10).await;
    run(&mut w, &format!("GET {marker}_a"), 10).await;
    run(&mut other, &format!("SET {marker}_other 1"), 10).await;
    let mut got = Vec::new();
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until {
        got.extend(p.profiler_poll().await.expect("profiler_poll"));
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    p.profiler_stop().await.expect("profiler_stop");
    let mine: Vec<_> = got.iter().filter(|s| s.text.contains(&marker)).collect();
    eprintln!("{driver}: {mine:#?}");
    assert_eq!(mine.len(), 2, "{driver}: db1's commands once each");
    assert!(mine[0].text.eq_ignore_ascii_case(&format!("SET {marker}_a \"x y\"")), "{driver}: {}", mine[0].text);
    assert_eq!(mine[1].database.as_deref(), Some("db1"));
    assert!(mine[0].time.len() == 23 && mine[0].time <= mine[1].time, "{driver}: {}", mine[0].time);
    assert!(got.iter().all(|s| !s.text.to_ascii_uppercase().starts_with("CLIENT")), "{driver}: own commands left out");
    run(&mut w, &format!("DEL {marker}_a"), 10).await;
    run(&mut other, &format!("DEL {marker}_other"), 10).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let list = run(&mut w, "CLIENT LIST", 1000).await;
    let text = format!("{:?}", list.results);
    assert!(!text.contains("cmd=monitor") && !text.contains("flags=O "), "{driver}: MONITOR still open: {text}");
}

#[tokio::test]
#[ignore]
async fn redis_profiler() {
    let Ok(url) = std::env::var("DBINE_TEST_REDIS_URL") else { return };
    profile("redis", &url).await;
}

#[tokio::test]
#[ignore]
async fn valkey_profiler() {
    let Ok(url) = std::env::var("DBINE_TEST_VALKEY_URL") else { return };
    profile("valkey", &url).await;
}

#[tokio::test]
#[ignore]
async fn dragonfly_profiler() {
    let Ok(url) = std::env::var("DBINE_TEST_DRAGONFLY_URL") else { return };
    profile("dragonfly", &url).await;
}

/// Every page of a key search, until the server says it's over.
async fn scan_all(s: &mut Box<dyn Session>, pattern: &str, key_type: Option<&str>) -> Vec<dbine_driver::KeyEntry> {
    let mut out = Vec::new();
    let mut cursor = None;
    loop {
        let scan = dbine_driver::KeyScan { pattern: pattern.into(), key_type: key_type.map(Into::into), cursor, count: 500 };
        let page = s.scan_keys(&scan).await.unwrap();
        out.extend(page.keys);
        match page.cursor {
            Some(c) => cursor = Some(c),
            None => break,
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

async fn key_search(driver: &str, url: &str) {
    let mut s = open(driver, url, "db6", false).await;
    // Plain commands, no Lua: Dragonfly refuses scripts that touch undeclared keys.
    let mut fixtures = vec!["FLUSHDB".to_string()];
    for chunk in (1..=3000).collect::<Vec<_>>().chunks(500) {
        fixtures.push(format!("MSET {}", chunk.iter().map(|i| format!("user:{i}:name x")).collect::<Vec<_>>().join(" ")));
    }
    for i in 1..=20 {
        fixtures.push(format!("HSET session:{i} u x"));
        fixtures.push(format!("PEXPIRE session:{i} 600000"));
    }
    fixtures.push("SET exact 1".into());
    run(&mut s, &fixtures.join("\n"), 10).await;

    // First page: a slice of everything, the total, and a cursor to go on.
    let first = s.scan_keys(&dbine_driver::KeyScan { pattern: String::new(), key_type: None, cursor: None, count: 500 }).await.unwrap();
    assert!(first.keys.len() >= 500, "{driver}: {} keys", first.keys.len());
    assert_eq!(first.total, Some(3021), "{driver}");
    assert!(first.cursor.is_some(), "{driver}");

    let sessions = scan_all(&mut s, "session:*", None).await;
    assert_eq!(sessions.len(), 20, "{driver}");
    assert!(sessions.iter().all(|k| k.key_type.as_deref() == Some("hash") && k.ttl_ms.is_some_and(|t| t > 0)), "{driver}");
    assert!(scan_all(&mut s, "user:1*", None).await.iter().all(|k| k.ttl_ms.is_none() && k.key_type.as_deref() == Some("string")));

    // By type, on the server or (old servers) in the driver.
    assert_eq!(scan_all(&mut s, "", Some("hash")).await.len(), 20, "{driver}");

    // Text without wildcards: the exact key first, then those containing it.
    let exact = s.scan_keys(&dbine_driver::KeyScan { pattern: "exact".into(), key_type: None, cursor: None, count: 50 }).await.unwrap();
    assert_eq!(exact.keys.first().map(|k| k.name.as_str()), Some("exact"), "{driver}");
    let contains: Vec<_> = scan_all(&mut s, "ssion:1", None).await.into_iter().map(|k| k.name).collect();
    assert_eq!(contains.len(), 11, "{driver}: {contains:?}"); // session:1 and session:10…19
    run(&mut s, "FLUSHDB", 10).await;
}

#[tokio::test]
#[ignore]
async fn redis_key_search() {
    key_search("redis", &std::env::var("DBINE_TEST_REDIS_URL").expect("DBINE_TEST_REDIS_URL")).await;
}

#[tokio::test]
#[ignore]
async fn valkey_key_search() {
    key_search("valkey", &std::env::var("DBINE_TEST_VALKEY_URL").expect("DBINE_TEST_VALKEY_URL")).await;
}

#[tokio::test]
#[ignore]
async fn dragonfly_key_search() {
    key_search("dragonfly", &std::env::var("DBINE_TEST_DRAGONFLY_URL").expect("DBINE_TEST_DRAGONFLY_URL")).await;
}

/// The data-compare delete script removes exactly the keyed rows of each
/// key type (hash field, zset member, stream entry, set / list element,
/// string key).
#[tokio::test]
#[ignore]
async fn delete_script_runs() {
    let url = std::env::var("DBINE_TEST_REDIS_URL").expect("DBINE_TEST_REDIS_URL");
    let d = dbine_driver_redis::drivers().into_iter().find(|d| d.info().id == "redis").unwrap();
    let mut s = open("redis", &url, "db6", false).await;
    run(&mut s, "DEL dh dz dx ds dl dstr", 10).await;
    run(
        &mut s,
        "HSET dh \"O'Brien \\\"Bob\\\"\" 1 keep 2\nZADD dz 1 \"m 1\" 2 keep\nXADD dx 1-0 a 1\nXADD dx 2-0 a 2\n\
         SADD ds \"a b\" keep\nRPUSH dl x \"a b\" x\nSET dstr v",
        10,
    )
    .await;
    let del = |k: &str, col: &str, v: &str| (key(k), vec![vec![(col.to_string(), serde_json::json!(v))]]);
    for (obj, keys) in [
        del("dh", "field", "O'Brien \"Bob\""),
        del("dz", "member", "m 1"),
        del("dx", "id", "1-0"),
        del("ds", "value", "a b"),
        del("dl", "value", "x"),
        del("dstr", "value", "v"),
    ] {
        let script = d.delete_script(&obj, &keys).unwrap();
        run(&mut s, &script, 10).await;
    }
    let dump = |o: QueryOutcome| serde_json::to_string(&o.results.iter().map(|r| &r.rows).collect::<Vec<_>>()).unwrap();
    assert_eq!(dump(run(&mut s, "HKEYS dh", 10).await).matches("keep").count(), 1);
    assert!(!dump(run(&mut s, "HKEYS dh", 10).await).contains("Brien"));
    assert!(!dump(run(&mut s, "ZRANGE dz 0 -1", 10).await).contains("m 1"));
    assert!(dump(run(&mut s, "ZRANGE dz 0 -1", 10).await).contains("keep"));
    let x = dump(run(&mut s, "XRANGE dx - +", 10).await);
    assert!(!x.contains("1-0") && x.contains("2-0"), "{x}");
    let set = dump(run(&mut s, "SMEMBERS ds", 10).await);
    assert!(!set.contains("a b") && set.contains("keep"), "{set}");
    let list = dump(run(&mut s, "LRANGE dl 0 -1", 10).await);
    assert_eq!(list.matches("\"x\"").count(), 1, "{list}");
    assert!(list.contains("a b"), "{list}");
    assert!(dump(run(&mut s, "EXISTS dstr", 10).await).contains('0'));
    run(&mut s, "DEL dh dz dx ds dl dstr", 10).await;
}
