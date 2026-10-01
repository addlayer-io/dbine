//! Against a real Dremio OSS (the test creates the first user if needed):
//!
//! ```sh
//! docker run -d --name dbine-test-dremio -p 25947:9047 dremio/dremio-oss
//! DBINE_TEST_DREMIO_URL=http://localhost:25947 cargo test -p dbine-driver-dremio -- --ignored --test-threads=1
//! ```

use dbine_driver::read_only::ReadOnlySession;
use dbine_driver::{kinds, ColumnDef, ConnectionConfig, DdlParts, Error, ObjectRef, QueryOutcome, Session, TableSchema};
use serde_json::json;
use std::time::{Duration, Instant};

const USER: &str = "dbine";
const PASS: &str = "secreto123";

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_DREMIO_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "dremio".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(USER.into()),
        password: Some(PASS.into()),
        ..Default::default()
    })
}

/// A fresh Dremio has no users: create the first one (ignored if it exists).
async fn bootstrap(c: &ConnectionConfig) {
    let http = reqwest::Client::new();
    let base = format!("http://{}:{}", c.host, c.port);
    for _ in 0..60 {
        if http.get(format!("{base}/apiv2/server_status")).send().await.map(|r| r.status().is_success()).unwrap_or(false) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let _ = http
        .put(format!("{base}/apiv2/bootstrap/firstuser"))
        .header("Authorization", "_dremionull")
        .json(&json!({"userName": USER, "firstName": "DB", "lastName": "Ine", "email": "dbine@example.com", "createdAt": 1700000000000u64, "password": PASS}))
        .send()
        .await;
}

fn obj(kind: &str, schema: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: Some(schema.into()), name: name.into() }
}

#[tokio::test]
#[ignore]
async fn dremio() {
    let Some(c) = cfg() else { return };
    bootstrap(&c).await;
    let d = dbine_driver_dremio::drivers().remove(0);

    let mut bad = c.clone();
    bad.password = Some("nope".into());
    assert!(matches!(d.connect(&bad, None).await, Err(Error::AuthFailed(_))));

    let mut s = d.connect(&c, None).await.unwrap();
    assert!(s.server_version().await.unwrap().starts_with("Dremio "));
    if s.list_databases().await.unwrap().contains(&"dbine_it".to_string()) {
        s.drop_database("dbine_it").await.unwrap();
    }
    s.create_database("dbine_it").await.unwrap();
    let dbs = s.list_databases().await.unwrap();
    assert!(dbs.contains(&"dbine_it".to_string()) && dbs.contains(&"$scratch".to_string()), "{dbs:?}");

    let mut s = d.connect(&c, Some("$scratch")).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "DROP TABLE IF EXISTS \"$scratch\".dbine_t; DROP TABLE IF EXISTS \"$scratch\".dbine_copia;
         CREATE TABLE dbine_t (id BIGINT, nombre VARCHAR, total DECIMAL(10,2), alta DATE, ts TIMESTAMP, b VARBINARY, ok BOOLEAN);
         INSERT INTO dbine_t VALUES (1, 'Ana', 10.5, DATE '2024-01-31', TIMESTAMP '2024-01-31 13:45:00.123', CAST('abc' AS VARBINARY), TRUE),
                                    (2, 'O''Brien', 3, NULL, NULL, NULL, FALSE), (3, 'Luis', 1, NULL, NULL, NULL, NULL);
         CREATE VIEW dbine_it.v AS SELECT id, nombre FROM \"$scratch\".dbine_t WHERE ok",
        100,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.results[3].rows[0][0], json!(3), "rows inserted");

    let objs = s.list_objects().await.unwrap();
    assert!(objs.iter().any(|o| o.kind == kinds::TABLE && o.name == "dbine_t" && o.schema.as_deref() == Some("$scratch")), "{objs:?}");
    let cols = s.columns(&obj(kinds::TABLE, "$scratch", "dbine_t")).await.unwrap();
    assert_eq!(cols.iter().map(|c| c.data_type.as_str()).collect::<Vec<_>>(), ["BIGINT", "CHARACTER VARYING", "DECIMAL(10,2)", "DATE", "TIMESTAMP", "BINARY VARYING", "BOOLEAN"]);
    let def = s.definition(&obj(kinds::TABLE, "$scratch", "dbine_t")).await.unwrap().unwrap();
    assert!(def.starts_with("CREATE TABLE"), "{def}");
    let mut sp = d.connect(&c, Some("dbine_it")).await.unwrap();
    let objs = sp.list_objects().await.unwrap();
    assert!(objs.iter().any(|o| o.kind == kinds::VIEW && o.name == "v"), "{objs:?}");
    let vdef = sp.definition(&obj(kinds::VIEW, "dbine_it", "v")).await.unwrap().unwrap();
    assert!(vdef.starts_with("CREATE OR REPLACE VIEW \"dbine_it\".\"v\" AS"), "{vdef}");

    let q = s.browse_query(&obj(kinds::TABLE, "$scratch", "dbine_t"), 10);
    let mut out = QueryOutcome::default();
    s.execute(&format!("{q}; SELECT * FROM dbine_t WHERE id = 1"), 2, &mut out).await.unwrap();
    let r = &out.results[0];
    assert_eq!((r.rows.len(), r.total_rows, r.truncated), (2, 3, true));
    assert_eq!(
        out.results[1].rows[0],
        vec![json!(1), json!("Ana"), json!("10.5"), json!("2024-01-31"), json!("2024-01-31 13:45:00.123"), json!("0x616263"), json!(true)]
    );

    // USE changes the context of later jobs.
    let mut out = QueryOutcome::default();
    s.execute("USE dbine_it; SELECT count(*) FROM v", 10, &mut out).await.unwrap();
    assert_eq!(out.results[1].rows[0][0], json!(1));
    s.execute("USE \"$scratch\"", 10, &mut QueryOutcome::default()).await.unwrap();

    // Errors stop the script.
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT 1; SELECT * FROM nope; SELECT 2", 10, &mut out).await.unwrap_err();
    assert!(e.is_query() && e.to_string().contains("nope"), "{e:?}");
    assert_eq!(out.results.len(), 1);

    // Plans.
    let q = "SELECT a.nombre, count(*) FROM dbine_t a JOIN dbine_t b ON a.id = b.id GROUP BY a.nombre";
    let mut out = QueryOutcome::default();
    s.explain(&format!("{q}; DROP TABLE IF EXISTS nada"), false, 10, &mut out).await.unwrap();
    assert_eq!(out.plans.len(), 1);
    assert!(out.results.is_empty());
    assert_eq!(out.plans[0].root.op, "Screen");
    assert!(out.plans[0].root.total_cost.is_some());
    let mut out = QueryOutcome::default();
    s.explain(q, true, 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 3);
    let p = &out.plans[0];
    assert!(p.actual);
    fn scans(n: &dbine_driver::PlanNode, acc: &mut Vec<Option<f64>>) {
        if n.detail.contains("DATA_FILE_SCAN") {
            acc.push(n.actual_rows);
        }
        n.children.iter().for_each(|c| scans(c, acc));
    }
    let mut acc = Vec::new();
    scans(&p.root, &mut acc);
    assert!(!acc.is_empty() && acc.iter().all(|r| *r == Some(3.0)), "{acc:?} {:#?}", p.root);

    // Designer round trip and INSERT scripts.
    let t = TableSchema {
        kind: kinds::TABLE.into(),
        schema: Some("$scratch".into()),
        name: "dbine_copia".into(),
        columns: cols.iter().map(|c| ColumnDef { name: c.name.clone(), data_type: c.data_type.replace("CHARACTER VARYING", "VARCHAR").replace("BINARY VARYING", "VARBINARY"), ..Default::default() }).collect(),
        options: [("partition_by".to_string(), "ok".to_string())].into(),
        ..Default::default()
    };
    s.execute(&d.table_ddl(&t, DdlParts { create: true, if_exists: true, ..Default::default() }).unwrap(), 10, &mut QueryOutcome::default()).await.unwrap();
    let mut browse = QueryOutcome::default();
    s.execute("SELECT id, nombre, total, alta, ts, ok FROM dbine_t", 100, &mut browse).await.unwrap();
    let names: Vec<String> = browse.results[0].columns.iter().map(|c| c.name.clone()).collect();
    let ins = d.insert_script(&obj(kinds::TABLE, "$scratch", "dbine_copia"), &names, &browse.results[0].rows).unwrap();
    s.execute(&ins, 10, &mut QueryOutcome::default()).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("SELECT count(*), max(ts), sum(total) FROM dbine_copia", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0], vec![json!(3), json!("2024-01-31 13:45:00.123"), json!("14.5")]);
    let schema = s.database_schema().await.unwrap();
    assert!(schema.iter().any(|t| t.name == "dbine_copia" && t.columns.len() == 7));

    // Templates (in a space; CTAS needs an Iceberg-capable source).
    for tpl in d.create_templates().iter().filter(|t| t.kind == kinds::VIEW) {
        let sql = tpl.template.replace("{schema}", "dbine_it").replace("{name}", "v_tpl").replace("dbine_it.\"tabla\" t\nWHERE t.activo = true", "\"$scratch\".dbine_t t").replace("t.id,\n    t.nombre", "t.id, t.nombre");
        sp.execute(&sql, 10, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{}: {e}", tpl.label));
    }

    // Cancel: the job is cancelled on the server.
    let stop = s.interrupter().unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        stop();
    });
    let t = Instant::now();
    let slow = "SELECT count(*) FROM sys.\"options\" a JOIN sys.\"options\" b ON a.kind = b.kind JOIN sys.\"options\" c ON b.kind = c.kind JOIN sys.\"options\" e ON c.kind = e.kind";
    let r = s.execute(slow, 10, &mut QueryOutcome::default()).await;
    assert!(matches!(r, Err(Error::Cancelled)), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(10));
    let mut stopped = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let snap = s.monitor().await.unwrap();
        if !snap.tables.iter().find(|t| t.key == "queries").unwrap().rows.iter().any(|r| r[8].as_str().is_some_and(|q| q.contains("JOIN sys.\"options\" e"))) {
            stopped = true;
            break;
        }
    }
    assert!(stopped, "the job was cancelled on the server");

    // Read-only (the registry wraps SQL sessions).
    let mut ro = ReadOnlySession::new(d.connect(&c, Some("$scratch")).await.unwrap());
    assert!(ro.execute("DROP TABLE dbine_t", 10, &mut QueryOutcome::default()).await.is_err());

    s.execute("DROP TABLE dbine_t; DROP TABLE dbine_copia", 10, &mut QueryOutcome::default()).await.unwrap();
    s.drop_database("dbine_it").await.unwrap();
    assert!(!s.list_databases().await.unwrap().contains(&"dbine_it".to_string()));
}

#[tokio::test]
#[ignore]
async fn monitor() {
    let Some(c) = cfg() else { return };
    bootstrap(&c).await;
    let d = dbine_driver_dremio::drivers().remove(0);
    let mut s = d.connect(&c, None).await.unwrap();
    s.execute("SELECT count(*) FROM sys.\"options\"", 10, &mut QueryOutcome::default()).await.unwrap();
    let snap = s.monitor().await.unwrap();
    let v = |k: &str| snap.metrics.iter().find(|m| m.key == k).and_then(|m| m.value);
    for k in ["cpu", "mem_used", "direct_used", "active_sessions", "queries", "nodes_green", "uptime"] {
        assert!(v(k).is_some(), "{k}: {:#?}", snap.metrics);
    }
    assert!(v("mem_used").unwrap() > 1e6 && v("queries").unwrap() >= 1.0);
    assert_eq!(snap.tables.iter().find(|t| t.key == "nodes").unwrap().rows.len(), 1);
    assert!(snap.info.iter().any(|(k, _)| k == "Versión"));
}

/// The profiler reports another session's jobs in the container once each
/// (a fast one and a failed one, with its error), and leaves out its own
/// reads and other containers' jobs.
#[tokio::test]
#[ignore]
async fn profiler() {
    let Some(c) = cfg() else { return };
    bootstrap(&c).await;
    let d = dbine_driver_dremio::drivers().remove(0);
    assert!(d.supports_profiler());
    let mut p = d.connect(&c, None).await.unwrap();
    let mut w = d.connect(&c, Some("sys")).await.unwrap();
    let opts = dbine_driver::ProfilerOptions { database: "sys".into(), change_server: false };
    let started = p.profiler_start(&opts).await.expect("profiler_start");
    eprintln!("{started:?}");
    let marker = format!("dbine_prof_{}", std::process::id());
    let mut out = QueryOutcome::default();
    w.execute(&format!("SELECT version AS {marker}_ok FROM version"), 10, &mut out).await.unwrap();
    assert!(w.execute(&format!("SELECT {marker}_bad FROM version"), 10, &mut out).await.is_err());
    let mut other = d.connect(&c, None).await.unwrap();
    other.execute(&format!("SELECT 1 AS {marker}_other"), 10, &mut out).await.unwrap();
    let mut got = Vec::new();
    let until = Instant::now() + Duration::from_secs(30);
    while Instant::now() < until && got.iter().filter(|s: &&dbine_driver::ProfiledStatement| s.text.contains(&marker)).count() < 2 {
        got.extend(p.profiler_poll().await.expect("profiler_poll"));
    }
    got.extend(p.profiler_poll().await.expect("profiler_poll"));
    p.profiler_stop().await.unwrap();
    let mine: Vec<_> = got.iter().filter(|s| s.text.contains(&marker)).collect();
    eprintln!("{mine:#?}");
    let ok: Vec<_> = mine.iter().filter(|s| s.text.contains("_ok")).collect();
    assert_eq!(ok.len(), 1, "the job once");
    assert_eq!((ok[0].rows, ok[0].database.as_deref(), ok[0].user.as_deref()), (Some(1), Some("sys"), Some(USER)));
    let bad: Vec<_> = mine.iter().filter(|s| s.text.contains("_bad")).collect();
    assert_eq!(bad.len(), 1, "the failed job once");
    assert!(bad[0].error.is_some());
    assert!(!mine.iter().any(|s| s.text.contains("_other")), "other containers' jobs are left out");
    assert!(got.iter().all(|s| !s.text.contains("dbine profiler")), "its own reads are left out");
}
