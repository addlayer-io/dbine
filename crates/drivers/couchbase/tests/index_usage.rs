//! Index usage against a real Couchbase Server (already initialized, as
//! `integration.rs` leaves it): a collection with its primary index and
//! two more, five lookups through one of them and none through the other
//! (freshly built: not "sin uso", Couchbase's write counter includes the
//! build). Then "Eliminar índice…": the schema sync script without the
//! unread index, run.
//!
//! ```sh
//! DBINE_TEST_COUCHBASE_URL=http://localhost:25893 DBINE_TEST_COUCHBASE_MGMT_PORT=25891 \
//!   cargo test -p dbine-driver-couchbase --test index_usage -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange};
use std::time::Duration;

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_COUCHBASE_URL").ok()?).expect("URL");
    let mut c = ConnectionConfig {
        driver: "couchbase".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some("Administrator".into()),
        password: Some("secreto1".into()),
        ..Default::default()
    };
    c.options.insert("mgmt_port".into(), std::env::var("DBINE_TEST_COUCHBASE_MGMT_PORT").unwrap_or_else(|_| "8091".into()));
    Some(c)
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 10, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
}

async fn wait(s: &mut Box<dyn Session>, keyspace: &str) {
    for _ in 0..40 {
        if s.execute(&format!("SELECT RAW 1 FROM {keyspace} LIMIT 1"), 1, &mut QueryOutcome::default()).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("{keyspace} no quedó lista");
}

#[tokio::test]
#[ignore]
async fn index_usage_live() {
    let Some(c) = cfg() else { return };
    let d = dbine_driver_couchbase::drivers().remove(0);
    assert!(d.supports_index_usage());
    let mut s = d.connect(&c, None).await.unwrap();
    if !s.list_databases().await.unwrap().contains(&"dbine_ixu".to_string()) {
        s.create_database("dbine_ixu").await.unwrap();
    }
    let mut s = d.connect(&c, Some("dbine_ixu")).await.unwrap();
    let _ = s.execute("DROP SCOPE dbine_ixu.s1", 1, &mut QueryOutcome::default()).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    run(&mut s, "CREATE SCOPE dbine_ixu.s1; CREATE COLLECTION dbine_ixu.s1.pedidos").await;
    let ks = "dbine_ixu.s1.pedidos";
    wait(&mut s, ks).await;
    for i in 1..=10 {
        run(&mut s, &format!("INSERT INTO {ks} (KEY, VALUE) VALUES ('p{i}', {{'cliente': 'c{}', 'fecha': {i}}})", i % 3)).await;
    }
    run(&mut s, &format!("CREATE PRIMARY INDEX ON {ks}; CREATE INDEX ix_cliente ON {ks}(cliente); CREATE INDEX ix_fecha ON {ks}(fecha DESC) WHERE fecha > 0")).await;
    for c in ["c0", "c1", "c2", "c0", "c1"] {
        run(&mut s, &format!("SELECT META(p).id FROM {ks} AS p USE INDEX (ix_cliente) WHERE p.cliente = '{c}'")).await;
    }
    let obj = ObjectRef { kind: "collection".into(), schema: Some("dbine_ixu.s1".into()), name: "pedidos".into() };
    // The cluster manager samples the index statistics every few seconds.
    let mut r = s.index_usage(&obj).await.unwrap().expect("report").derived();
    for _ in 0..30 {
        if r.indexes.iter().any(|i| i.name == "ix_cliente" && i.seeks >= 5) && r.indexes.iter().any(|i| i.name == "ix_fecha" && i.size_kb.is_some()) {
            break;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        r = s.index_usage(&obj).await.unwrap().expect("report").derived();
    }
    println!("{r:#?}");
    assert!(r.stats_available && !r.seek_scan_split, "{:?}", r.note);
    assert!(r.since.is_some());
    let get = |n: &str| r.indexes.iter().find(|i| i.name == n).unwrap_or_else(|| panic!("{n}"));
    assert!(get("#primary").primary_key);
    assert_eq!(get("ix_cliente").seeks, 5);
    assert!(!get("ix_cliente").unused);
    assert!(get("ix_cliente").size_kb.is_some());
    assert_eq!(get("ix_fecha").seeks, 0);
    assert!(get("ix_fecha").filter.is_some());
    // Its write counter includes the initial build: writes not counted, so
    // a freshly built, never-read index is not "sin uso".
    assert!(!r.writes_counted);
    assert!(r.indexes.iter().all(|i| i.updates == 0 && !i.unused));
    assert!(r.note.as_deref().is_some_and(|n| n.contains("construcción inicial")), "{:?}", r.note);

    let table = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "pedidos" && t.schema.as_deref() == Some("dbine_ixu.s1")).unwrap();
    let mut without = table.clone();
    without.indexes.retain(|i| i.name != "ix_fecha");
    let script = d.sync_script(&[TableChange::Alter { old: table, new: without }]).unwrap();
    println!("{script:#?}");
    assert_eq!(script.statements.len(), 1);
    run(&mut s, &script.statements[0]).await;
    let r = s.index_usage(&obj).await.unwrap().unwrap();
    assert!(r.indexes.iter().all(|i| i.name != "ix_fecha"));
    assert!(r.indexes.iter().any(|i| i.name == "ix_cliente"));
    run(&mut s, "DROP SCOPE dbine_ixu.s1").await;
}
