//! Against a real server:
//! `docker run -d --name dbine-test-iotdb -p 25405:18080 -e enable_rest_service=true apache/iotdb:1.3.2-standalone`,
//! then `DBINE_TEST_IOTDB_URL=http://localhost:25405 cargo test -p dbine-driver-iotdb -- --ignored`
//! (user root / root). The monitor test also reads the DataNode's Prometheus
//! endpoint when `DBINE_TEST_IOTDB_METRICS_URL` is set (start the container
//! with `-p 25406:9092 -e dn_metric_reporter_list=PROMETHEUS`).
//! Tests that also cover IoTDB 2 read `DBINE_TEST_IOTDB2_URL`:
//! `docker run -d --name dbine-test-iotdb2 -p 27150:18080 -p 27151:9092 -e enable_rest_service=true -e dn_metric_reporter_list=PROMETHEUS apache/iotdb:2.0.5-standalone`,
//! then `DBINE_TEST_IOTDB2_URL=http://localhost:27150`.

use dbine_driver::{ConnectionConfig, Error, ObjectRef, QueryOutcome, Session};
use serde_json::json;

fn cfg(url: &str, read_only: bool) -> ConnectionConfig {
    ConnectionConfig {
        driver: "iotdb".into(),
        host: url.into(),
        username: Some("root".into()),
        password: Some("root".into()),
        read_only,
        ..Default::default()
    }
}

async fn open(url: &str, db: Option<&str>, read_only: bool) -> Box<dyn Session> {
    let d = dbine_driver_iotdb::drivers().into_iter().find(|d| d.info().id == "iotdb").unwrap();
    d.connect(&cfg(url, read_only), db).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, text: &str, max_rows: usize) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(text, max_rows, &mut out).await {
        panic!("{text}: {e}");
    }
    out
}

#[tokio::test]
#[ignore]
async fn round_trip() {
    let url = std::env::var("DBINE_TEST_IOTDB_URL").expect("DBINE_TEST_IOTDB_URL");
    let mut s = open(&url, None, false).await;
    println!("{}", s.server_version().await.unwrap());
    let mut out = QueryOutcome::default();
    let _ = s.execute("DELETE DATABASE root.dbine_it", 10, &mut out).await;
    let mut inserts = String::from("CREATE DATABASE root.dbine_it;\n");
    for i in 0..30 {
        inserts.push_str(&format!(
            "INSERT INTO root.dbine_it.plant.d1(timestamp, temp, status) VALUES ({}, {}.5, {});\n",
            1_706_708_700_000i64 + i * 1000,
            20 + i,
            i % 2 == 0
        ));
    }
    inserts.push_str("INSERT INTO root.dbine_it.d2(timestamp, hum) VALUES (1706708700000, 40);");
    run(&mut s, &inserts, 10).await;

    let dbs = s.list_databases().await.unwrap();
    assert!(dbs.contains(&"root.dbine_it".into()), "{dbs:?}");

    let mut s = open(&url, Some("root.dbine_it"), false).await;
    let objs: Vec<String> = s.list_objects().await.unwrap().into_iter().map(|o| o.name).collect();
    assert_eq!(objs, ["d2", "plant.d1"]);
    let d1 = ObjectRef { kind: "device".into(), schema: None, name: "plant.d1".into() };
    let cols: Vec<(String, String)> = s.columns(&d1).await.unwrap().into_iter().map(|c| (c.name, c.data_type)).collect();
    assert!(cols.contains(&("temp".into(), "DOUBLE".into())) && cols.contains(&("status".into(), "BOOLEAN".into())), "{cols:?}");
    assert_eq!(cols[0].0, "Time");

    let q = s.browse_query(&d1, 5);
    let out = run(&mut s, &q, 100).await;
    assert_eq!(out.results[0].rows.len(), 5);
    assert_eq!(out.results[0].columns[0].name, "Time");
    assert_eq!(out.results[0].rows[0][0], json!("2024-01-31 13:45:29"));

    // More rows than max_rows: the server refuses, the driver caps the SELECT.
    let out = run(&mut s, "SELECT temp FROM root.dbine_it.plant.d1; SHOW DEVICES root.dbine_it.**", 10).await;
    assert_eq!(out.results[0].rows.len(), 10);
    assert!(out.results[0].truncated);
    assert_eq!(out.results[1].rows.len(), 2);
    let out = run(&mut s, "SELECT count(temp) FROM root.dbine_it.plant.d1", 10).await;
    assert_eq!(out.results[0].rows[0][0], json!(30));

    // Errors keep what ran before.
    let mut out = QueryOutcome::default();
    let e = s.execute("SHOW DATABASES; SELECT nope FROM", 10, &mut out).await.unwrap_err();
    assert!(matches!(e, Error::Query(_)), "{e:?}");
    assert_eq!(out.results.len(), 1);

    // Read-only.
    let mut ro = open(&url, Some("root.dbine_it"), true).await;
    run(&mut ro, "SHOW DEVICES; SELECT * FROM root.dbine_it.d2", 10).await;
    let e = ro.execute("SELECT hum INTO root.dbine_it.d3(hum) FROM root.dbine_it.d2", 10, &mut out).await.unwrap_err();
    assert!(matches!(e, Error::Query(ref m) if m.contains("solo lectura")));

    // Wrong password.
    let d = dbine_driver_iotdb::drivers().into_iter().find(|d| d.info().id == "iotdb").unwrap();
    let mut bad = cfg(&url, false);
    bad.password = Some("nope".into());
    assert!(matches!(d.connect(&bad, None).await, Err(Error::AuthFailed(_))));

    run(&mut s, "DELETE DATABASE root.dbine_it", 10).await;
}

#[tokio::test]
#[ignore]
async fn schema_ddl_round_trip() {
    use dbine_driver::DdlParts;
    let url = std::env::var("DBINE_TEST_IOTDB_URL").expect("DBINE_TEST_IOTDB_URL");
    let d = dbine_driver_iotdb::drivers().into_iter().find(|d| d.info().id == "iotdb").unwrap();
    assert!(d.capabilities().create_database && !d.capabilities().foreign_keys);
    let mut s = open(&url, None, false).await;
    for db in ["root.dbine_ddl", "root.dbine_ddl2"] {
        let _ = s.drop_database(db).await;
    }
    let _ = s.execute("DROP DEVICE TEMPLATE dbine_tpl", 10, &mut QueryOutcome::default()).await;
    s.create_database("dbine_ddl").await.unwrap();
    assert!(s.list_databases().await.unwrap().contains(&"root.dbine_ddl".into()));
    run(
        &mut s,
        "CREATE ALIGNED TIMESERIES root.dbine_ddl.plant.d1(temp DOUBLE encoding=GORILLA compressor=SNAPPY, `my-s` INT32 encoding=RLE);\n\
         CREATE TIMESERIES root.dbine_ddl.d2.hum WITH DATATYPE=FLOAT, ENCODING=PLAIN, COMPRESSOR=LZ4;\n\
         CREATE TIMESERIES root.dbine_ddl.d2.label WITH DATATYPE=TEXT, ENCODING=DICTIONARY;\n\
         CREATE TIMESERIES root.dbine_ddl.d2.ok WITH DATATYPE=BOOLEAN",
        10,
    )
    .await;

    let mut s = open(&url, Some("root.dbine_ddl"), false).await;
    let schema = s.database_schema().await.unwrap();
    let names: Vec<&str> = schema.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["d2", "plant.d1"]);
    let d1 = &schema[1];
    assert_eq!(d1.schema.as_deref(), Some("root.dbine_ddl"));
    assert_eq!(d1.options.get("aligned").map(String::as_str), Some("true"));
    assert_eq!(d1.columns[0].name, "Time");
    let my = d1.columns.iter().find(|c| c.name == "my-s").unwrap();
    assert_eq!((my.data_type.as_str(), my.options["encoding"].as_str()), ("INT32", "RLE"));
    let hum = schema[0].columns.iter().find(|c| c.name == "hum").unwrap();
    assert_eq!((hum.options["encoding"].as_str(), hum.options["compression"].as_str()), ("PLAIN", "LZ4"));

    // Round trip into a fresh database.
    s.create_database("root.dbine_ddl2").await.unwrap();
    let mut script = Vec::new();
    for t in &schema {
        let mut t = t.clone();
        t.schema = Some("root.dbine_ddl2".into());
        script.push(d.table_ddl(&t, DdlParts { create: true, indexes: true, foreign_keys: true, ..Default::default() }).unwrap());
    }
    let script = script.join("\n");
    println!("{script}");
    run(&mut s, &script, 10).await;
    let mut s2 = open(&url, Some("root.dbine_ddl2"), false).await;
    let mut copy = s2.database_schema().await.unwrap();
    for t in &mut copy {
        t.schema = Some("root.dbine_ddl".into());
    }
    assert_eq!(copy, schema);

    // Drop + create again from the same DDL.
    let mut t = copy[0].clone();
    t.schema = Some("root.dbine_ddl2".into());
    run(&mut s2, &d.table_ddl(&t, DdlParts { drop: true, create: true, ..Default::default() }).unwrap(), 10).await;

    // Rows from the browse query back as INSERTs.
    run(&mut s, "INSERT INTO root.dbine_ddl.d2(timestamp, hum, label, ok) VALUES (1706708700500, 40.5, 'it''s', true), (1706708701000, 41, null, false)", 10).await;
    let dev = dbine_driver::ObjectRef { kind: "device".into(), schema: Some("root.dbine_ddl2".into()), name: "d2".into() };
    let out = run(&mut s, "SELECT hum, label, ok FROM root.dbine_ddl.d2", 10).await;
    let cols: Vec<String> =
        out.results[0].columns.iter().map(|c| c.name.replace("root.dbine_ddl.d2.", "root.dbine_ddl2.d2.")).collect();
    let ins = d.insert_script(&dev, &cols, &out.results[0].rows).unwrap();
    println!("{ins}");
    run(&mut s2, &ins, 10).await;
    let back = run(&mut s2, "SELECT hum, label, ok FROM root.dbine_ddl2.d2", 10).await;
    assert_eq!(back.results[0].rows, out.results[0].rows);

    // Templates that need no plugin jar run as they are.
    for tpl in d.create_templates().iter().filter(|t| t.kind == "continuous_query" || t.kind == "device_template") {
        let text = tpl.template.replace("{name}", "dbine_tpl").replace("root.db.", "root.dbine_ddl2.");
        run(&mut s2, &text, 10).await;
    }
    run(&mut s2, "DROP CONTINUOUS QUERY dbine_tpl", 10).await;

    let e = s2.drop_database("root.dbine_ddl2").await.unwrap_err();
    assert!(matches!(e, Error::Query(ref m) if m.contains("esta conexión")), "{e:?}");
    let mut admin = open(&url, None, false).await;
    run(&mut admin, "UNSET DEVICE TEMPLATE dbine_tpl FROM root.dbine_ddl2.planta", 10).await;
    admin.drop_database("root.dbine_ddl").await.unwrap();
    admin.drop_database("dbine_ddl2").await.unwrap();
    run(&mut admin, "DROP DEVICE TEMPLATE dbine_tpl", 10).await;
}

#[tokio::test]
#[ignore]
async fn monitor() {
    let url = std::env::var("DBINE_TEST_IOTDB_URL").expect("DBINE_TEST_IOTDB_URL");
    let metrics = std::env::var("DBINE_TEST_IOTDB_METRICS_URL").ok();
    let mut s = open(&url, None, false).await;
    let _ = s.execute("DELETE DATABASE root.dbine_mon", 10, &mut QueryOutcome::default()).await;
    run(&mut s, "CREATE DATABASE root.dbine_mon", 10).await;
    run(&mut s, "INSERT INTO root.dbine_mon.d1(timestamp, t, h) VALUES (1, 1.5, 2)", 10).await;
    for id in ["iotdb", "timechodb"] {
        let d = dbine_driver_iotdb::drivers().into_iter().find(|d| d.info().id == id).unwrap();
        assert!(d.capabilities().monitor);
        let mut c = cfg(&url, true);
        c.driver = id.into();
        if let Some(m) = &metrics {
            c.options.insert("metrics_url".into(), m.clone());
        }
        let mut s = d.connect(&c, None).await.unwrap();
        println!("{}", s.server_version().await.unwrap());
        let snap = s.monitor().await.unwrap();
        for m in &snap.metrics {
            println!("{:<16} {:?} max={:?} counter={}", m.key, m.value, m.max, m.counter);
        }
        for t in &snap.tables {
            println!("[{}] {} rows {:?}", t.key, t.rows.len(), t.rows.first());
        }
        println!("info {:?}\nnotes {:?}", snap.info, snap.notes);
        let val = |k: &str| snap.metrics.iter().find(|m| m.key == k).unwrap().value;
        assert!(val("series").unwrap() >= 2.0);
        assert!(val("nodes_running").unwrap() >= 1.0);
        assert!(snap.tables.iter().any(|t| t.key == "nodes" && !t.rows.is_empty()));
        let dbs = snap.tables.iter().find(|t| t.key == "databases").unwrap();
        assert!(dbs.rows.iter().any(|r| r[0] == json!("root.dbine_mon")), "{:?}", dbs.rows);
        assert!(snap.info.iter().any(|(k, _)| k == "Versión"));
        if metrics.is_some() {
            assert!(val("cpu_time").unwrap() > 0.0);
            assert!(val("mem_used").unwrap() > 0.0);
            assert!(val("uptime").unwrap() > 0.0);
        } else {
            assert!(val("cpu").is_none() && !snap.notes.is_empty());
        }
    }
    run(&mut s, "DELETE DATABASE root.dbine_mon", 10).await;
}

/// The profiler sees another session's slow query on the database once,
/// with its duration, and leaves out its own looks. Also against IoTDB 2
/// when `DBINE_TEST_IOTDB2_URL` is set.
#[tokio::test]
#[ignore]
async fn profiler() {
    let url = std::env::var("DBINE_TEST_IOTDB_URL").expect("DBINE_TEST_IOTDB_URL");
    for url in [Some(url), std::env::var("DBINE_TEST_IOTDB2_URL").ok()].into_iter().flatten() {
        profile(&url).await;
    }
}

async fn profile(url: &str) {
    let d = dbine_driver_iotdb::drivers().into_iter().find(|d| d.info().id == "iotdb").unwrap();
    assert!(d.supports_profiler());
    let mut w = open(url, None, false).await;
    println!("{}", w.server_version().await.unwrap());
    let _ = w.execute("DELETE DATABASE root.dbine_prof", 10, &mut QueryOutcome::default()).await;
    run(&mut w, "CREATE DATABASE root.dbine_prof", 10).await;
    run(&mut w, "INSERT INTO root.dbine_prof.d1(timestamp, v) VALUES (1, 1.5)", 10).await;
    let mut p = open(url, None, true).await;
    let opts = dbine_driver::ProfilerOptions { database: "root.dbine_prof".into(), change_server: false };
    let started = p.profiler_start(&opts).await.expect("profiler_start");
    eprintln!("{started:?}");
    let marker = format!("dbine_prof_{}", std::process::id());
    // Millions of empty windows: about a second.
    let slow = format!("SELECT avg(v) AS {marker}_slow FROM root.dbine_prof.d1 GROUP BY ([0, 5000000), 1ms) HAVING avg(v) < 0");
    let work = async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        // Another database's query: left out.
        run(&mut w, &format!("SELECT avg(v) AS {marker}_other FROM root.dbine_prof_x.d1 GROUP BY ([0, 5000000), 1ms)"), 10).await;
        let t = std::time::Instant::now();
        run(&mut w, &slow, 10).await;
        eprintln!("slow query took {:?}", t.elapsed());
    };
    let watch = async {
        let mut got = Vec::new();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < until {
            got.extend(p.profiler_poll().await.expect("profiler_poll"));
            if got.iter().any(|s| s.text.contains(&marker)) {
                break;
            }
        }
        got
    };
    let ((), got) = tokio::join!(work, watch);
    p.profiler_stop().await.expect("profiler_stop");
    let mine: Vec<_> = got.iter().filter(|s| s.text.contains(&marker)).collect();
    eprintln!("{mine:#?}");
    assert_eq!(mine.len(), 1, "the slow query once");
    assert!(mine[0].text.contains("_slow"));
    assert!(mine[0].duration_ms.unwrap_or(0.0) >= 200.0, "duration {:?}", mine[0].duration_ms);
    assert!(got.iter().all(|s| !s.text.to_ascii_uppercase().starts_with("SHOW QUERIES")), "its own looks are left out");
    run(&mut w, "DELETE DATABASE root.dbine_prof", 10).await;
}
