//! Against a real etcd:
//!
//! ```sh
//! docker run -d --name dbine-test-etcd -p 25379:2379 quay.io/coreos/etcd:v3.5.17 \
//!   etcd --advertise-client-urls http://0.0.0.0:2379 --listen-client-urls http://0.0.0.0:2379
//! DBINE_TEST_ETCD_URL=http://localhost:25379 cargo test -p dbine-driver-etcd -- --ignored
//! ```

use dbine_driver::{kinds, ConnectionConfig, DdlParts, Error, ObjectRef, QueryOutcome, TableSchema};
use serde_json::json;
use std::time::Duration;

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_ETCD_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "etcd".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        ..Default::default()
    })
}

fn key(n: &str) -> ObjectRef {
    ObjectRef { kind: kinds::KEY.into(), schema: None, name: n.into() }
}

#[tokio::test]
#[ignore]
async fn etcd() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_etcd::drivers().remove(0);

    let mut bad = c.clone();
    bad.port = 1;
    assert!(matches!(d.connect(&bad, None).await, Err(Error::Connect(_))));

    let mut s = d.connect(&c, None).await.unwrap();
    assert!(s.server_version().await.unwrap().starts_with("etcd 3."));
    assert_eq!(s.list_databases().await.unwrap(), ["default"]);

    let mut out = QueryOutcome::default();
    s.execute("del /dbine_it/ --prefix", 10, &mut out).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "put /dbine_it/a 1\nput /dbine_it/b \"dos palabras\"\nput /dbine_it/c '{\"x\": 1}' --ttl=120\n# comentario\nput /dbine_it/a 2 --prev-kv",
        100,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.results[0].rows_affected, Some(1));
    assert_eq!(out.results[3].rows[0][1], json!("1"), "prev-kv returns the old value");
    assert!(out.messages.iter().any(|m| m.starts_with("lease ")), "{:?}", out.messages);

    let mut out = QueryOutcome::default();
    s.execute("get /dbine_it/ --prefix\nget /dbine_it/ --prefix --count-only\nget /dbine_it/ --prefix --keys-only --limit=2", 100, &mut out)
        .await
        .unwrap();
    let r = &out.results[0];
    assert_eq!(r.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["key", "value", "create_revision", "mod_revision", "version", "lease"]);
    assert_eq!(r.rows.len(), 3);
    assert_eq!(r.rows[0][1], json!("2"));
    assert_eq!(r.rows[0][4], json!(2), "version");
    assert_eq!(r.rows[1][1], json!("dos palabras"));
    assert!(r.rows[2][5].is_string(), "the TTL key has a lease");
    assert_eq!(out.results[1].rows[0][0], json!(3));
    assert_eq!(out.results[2].rows.len(), 2);

    // Leases.
    let lease = out.results[0].rows[2][5].as_str().unwrap().to_string();
    let mut out = QueryOutcome::default();
    s.execute(&format!("lease timetolive {lease} --keys\nlease list"), 10, &mut out).await.unwrap();
    assert!(out.results[0].rows[0][3].as_str().unwrap().contains("/dbine_it/c"));
    assert!(out.results[1].rows.iter().any(|r| r[0] == json!(lease)));

    // Explorer.
    let objs = s.list_objects().await.unwrap();
    assert!(objs.iter().any(|o| o.name == "/dbine_it/b" && o.kind == kinds::KEY));
    assert_eq!(s.columns(&key("/dbine_it/a")).await.unwrap()[0].name, "key");
    let def = s.definition(&key("/dbine_it/c")).await.unwrap().unwrap();
    assert!(def.contains("LEASE") && def.contains("TTL"), "{def}");
    assert!(s.definition(&key("/dbine_it/none")).await.unwrap().is_none());
    let q = s.browse_query(&key("/dbine_it/b"), 10);
    let mut out = QueryOutcome::default();
    s.execute(&q, 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][1], json!("dos palabras"));

    // Cluster commands.
    let mut out = QueryOutcome::default();
    s.execute("member list\nendpoint status\nalarm list", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 1);
    assert!(out.results[1].rows[0][1].as_str().unwrap().starts_with("3."));

    // Errors stop the script; what ran stays.
    let mut out = QueryOutcome::default();
    let e = s.execute("get /dbine_it/a\nfrobnicate\nget /dbine_it/b", 10, &mut out).await.unwrap_err();
    assert!(matches!(e, Error::Query(_)));
    assert_eq!(out.results.len(), 1);
    assert!(s.execute("watch /x", 10, &mut QueryOutcome::default()).await.is_err());

    // Designer and insert scripts.
    let t = TableSchema {
        kind: kinds::KEY.into(),
        name: "/dbine_it/nueva".into(),
        options: [("value".to_string(), "hola mundo".to_string()), ("ttl".to_string(), "60".to_string())].into(),
        ..Default::default()
    };
    let ddl = d.table_ddl(&t, DdlParts { drop: true, create: true, ..Default::default() }).unwrap();
    s.execute(&ddl, 10, &mut QueryOutcome::default()).await.unwrap();
    let ins = d.insert_script(&key("/dbine_it/imp"), &["id".into(), "nombre".into()], &[vec![json!(1), json!("Ana")], vec![json!(2), json!("O'Brien")]]).unwrap();
    s.execute(&ins, 10, &mut QueryOutcome::default()).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("get /dbine_it/nueva\nget /dbine_it/imp/2", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][1], json!("hola mundo"));
    assert_eq!(out.results[1].rows[0][1], json!("{\"id\":2,\"nombre\":\"O'Brien\"}"));
    for t in d.create_templates() {
        let script = t.template.replace("{name}", "/dbine_it/tpl");
        s.execute(&script, 10, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{}: {e}", t.label));
    }

    // Read-only.
    let mut ro = c.clone();
    ro.read_only = true;
    let mut r = d.connect(&ro, None).await.unwrap();
    assert!(r.execute("put /dbine_it/x 1", 10, &mut QueryOutcome::default()).await.is_err());
    r.execute("get /dbine_it/a\nlease list", 10, &mut QueryOutcome::default()).await.unwrap();

    // Prefix option narrows the explorer.
    let mut pc = c.clone();
    pc.options.insert("prefix".into(), "/dbine_it/imp/".into());
    let mut p = d.connect(&pc, None).await.unwrap();
    assert_eq!(p.list_objects().await.unwrap().len(), 2);

    // Cancel: the interrupter before a run makes it stop (etcd calls are
    // too quick to catch mid-flight).
    let stop = s.interrupter().unwrap();
    stop();
    tokio::time::sleep(Duration::from_millis(10)).await;
    s.execute("get /dbine_it/a", 10, &mut QueryOutcome::default()).await.unwrap();

    s.execute("del /dbine_it/ --prefix", 10, &mut QueryOutcome::default()).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn monitor() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_etcd::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    s.execute("put /dbine_mon x", 10, &mut QueryOutcome::default()).await.unwrap();
    let snap = s.monitor().await.unwrap();
    let v = |k: &str| snap.metrics.iter().find(|m| m.key == k).and_then(|m| m.value);
    for k in ["cpu_time", "mem_used", "storage_used", "keys", "queries", "rows_written", "uptime", "net_out"] {
        assert!(v(k).is_some(), "{k} missing: {:#?}", snap.metrics);
    }
    assert!(v("mem_used").unwrap() > 1e6);
    assert!(snap.metrics.iter().find(|m| m.key == "storage_used").unwrap().max.is_some());
    let nodes = snap.tables.iter().find(|t| t.key == "nodes").unwrap();
    assert_eq!(nodes.rows.len(), 1);
    assert_eq!(nodes.rows[0][2], json!("líder"));
    assert!(snap.info.iter().any(|(k, v)| k == "Versión" && v.starts_with("3.")));
    s.execute("del /dbine_mon", 10, &mut QueryOutcome::default()).await.unwrap();
}

/// With auth enabled (`etcdctl user add root:<pw> && etcdctl auth enable`)
/// and `DBINE_TEST_ETCD_PASSWORD=<pw>`: token login, bad password.
#[tokio::test]
#[ignore]
async fn auth() {
    let (Some(mut c), Ok(pw)) = (cfg(), std::env::var("DBINE_TEST_ETCD_PASSWORD")) else { return };
    let d = dbine_driver_etcd::drivers().remove(0);
    c.username = Some("root".into());
    c.password = Some("wrong".into());
    assert!(matches!(d.connect(&c, None).await, Err(Error::AuthFailed(_))));
    c.password = Some(pw);
    let mut s = d.connect(&c, None).await.unwrap();
    s.execute("put /dbine_auth 1\nget /dbine_auth\ndel /dbine_auth", 10, &mut QueryOutcome::default()).await.unwrap();
    assert!(!s.monitor().await.unwrap().tables[0].rows.is_empty());
}

#[tokio::test]
#[ignore]
async fn key_search() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_etcd::drivers().remove(0);
    assert!(d.key_search().is_some());
    let mut s = d.connect(&c, None).await.unwrap();
    let mut script = vec!["del /dbine_ks/ --prefix".to_string()];
    script.extend((1..=1200).map(|i| format!("put /dbine_ks/users/{i:04} x")));
    script.extend((1..=5).map(|i| format!("put /dbine_ks/sessions/{i} x --ttl=300")));
    s.execute(&script.join("\n"), 10, &mut QueryOutcome::default()).await.unwrap();

    let scan = |pattern: &str, cursor: Option<String>| dbine_driver::KeyScan { pattern: pattern.into(), key_type: None, cursor, count: 500 };
    // Pages of a prefix: the range's total up front, then a cursor until it's over.
    let first = s.scan_keys(&scan("/dbine_ks/users/", None)).await.unwrap();
    assert_eq!((first.keys.len(), first.total), (500, Some(1200)));
    let mut names: Vec<String> = first.keys.into_iter().map(|k| k.name).collect();
    let mut cursor = first.cursor;
    while let Some(c) = cursor {
        let page = s.scan_keys(&scan("/dbine_ks/users/", Some(c))).await.unwrap();
        assert_eq!(page.total, None);
        names.extend(page.keys.into_iter().map(|k| k.name));
        cursor = page.cursor;
    }
    assert_eq!(names.len(), 1200);
    assert_eq!((names[0].as_str(), names[1199].as_str()), ("/dbine_ks/users/0001", "/dbine_ks/users/1200"));

    // Keys under a lease carry its time to live.
    let sessions = s.scan_keys(&scan("/dbine_ks/sessions/", None)).await.unwrap();
    assert_eq!(sessions.keys.len(), 5);
    assert!(sessions.cursor.is_none());
    assert!(sessions.keys.iter().all(|k| k.ttl_ms.is_some_and(|t| t > 0 && t <= 300_000)));
    assert!(s.scan_keys(&scan("/dbine_ks/nothing/", None)).await.unwrap().keys.is_empty());

    s.execute("del /dbine_ks/ --prefix", 10, &mut QueryOutcome::default()).await.unwrap();
}

/// The data-compare delete script removes exactly the keyed keys: an
/// exact `del`, not a prefix (`/dbine_del/a` stays when `/dbine_del/` goes).
#[tokio::test]
#[ignore]
async fn delete_script_runs() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_etcd::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("del /dbine_del/ --prefix\nput /dbine_del/ root\nput /dbine_del/a 1\nput \"/dbine_del/O'Brien \\\"Bob\\\"\" 2", 10, &mut out)
        .await
        .unwrap();
    let keys = vec![
        vec![("key".to_string(), json!("/dbine_del/")), ("value".to_string(), json!("root"))],
        vec![("key".to_string(), json!("/dbine_del/O'Brien \"Bob\"")), ("value".to_string(), json!("2"))],
    ];
    let script = d.delete_script(&key("/dbine_del/"), &keys).unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&script, 10, &mut out).await.expect(&script);
    let mut out = QueryOutcome::default();
    s.execute("get /dbine_del/ --prefix --keys-only", 10, &mut out).await.unwrap();
    let rows = serde_json::to_string(&out.results[0].rows).unwrap();
    assert!(rows.contains("/dbine_del/a") && !rows.contains("Brien") && !rows.contains("\"/dbine_del/\""), "{rows}");
    let mut out = QueryOutcome::default();
    s.execute("del /dbine_del/ --prefix", 10, &mut out).await.unwrap();
}
