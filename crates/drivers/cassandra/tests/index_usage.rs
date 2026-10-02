//! Index usage against a real server: a table with a compound primary key
//! and two secondary indexes, lookups through one of them. CQL counts no
//! usage per index: the report lists the primary key and the indexes
//! without counters. Then "Eliminar índice…": the schema sync script
//! without one of them, run.
//!
//! ```sh
//! DBINE_TEST_SCYLLADB_URL=localhost:25413 \
//!   cargo test -p dbine-driver-cassandra --test index_usage -- --ignored --nocapture
//! DBINE_TEST_CASSANDRA_URL=localhost:25402 …   # the same on Cassandra
//! ```

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange};

fn cfg(driver: &str, url: &str) -> ConnectionConfig {
    let (host, port) = url.rsplit_once(':').unwrap();
    ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().unwrap(), ..Default::default() }
}

async fn run(s: &mut Box<dyn Session>, text: &str) {
    let mut out = QueryOutcome::default();
    s.execute(text, 10, &mut out).await.unwrap_or_else(|e| panic!("{text}: {e}"));
    assert!(out.error.is_none(), "{text}: {:?}", out.error);
}

async fn check(driver: &str, url: &str) {
    let d = dbine_driver_cassandra::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    assert!(d.supports_index_usage());
    let mut admin = d.connect(&cfg(driver, url), None).await.unwrap();
    run(&mut admin, "DROP KEYSPACE IF EXISTS dbine_ixu").await;
    run(&mut admin, "CREATE KEYSPACE dbine_ixu WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}").await;
    let mut s = d.connect(&cfg(driver, url), Some("dbine_ixu")).await.unwrap();
    run(&mut s, "CREATE TABLE pedidos (id int, linea int, cliente text, fecha int, PRIMARY KEY (id, linea)) WITH CLUSTERING ORDER BY (linea DESC)").await;
    run(&mut s, "CREATE INDEX ix_cliente ON pedidos (cliente)").await;
    run(&mut s, "CREATE INDEX ix_fecha ON pedidos (fecha)").await;
    for i in 1..=10 {
        run(&mut s, &format!("INSERT INTO pedidos (id, linea, cliente, fecha) VALUES ({i}, 1, 'c{}', {i})", i % 3)).await;
    }
    for c in ["c0", "c1", "c2", "c0", "c1"] {
        run(&mut s, &format!("SELECT id FROM pedidos WHERE cliente = '{c}'")).await;
    }
    let obj = ObjectRef { kind: kinds::TABLE.into(), schema: Some("dbine_ixu".into()), name: "pedidos".into() };
    let r = s.index_usage(&obj).await.unwrap().expect("report").derived();
    println!("{r:#?}");
    assert!(!r.stats_available && r.note.is_some());
    assert!(r.foreign_keys.is_empty());
    let names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["PRIMARY KEY", "ix_cliente", "ix_fecha"]);
    assert!(r.indexes[0].primary_key);
    assert_eq!(r.indexes[0].key_columns, ["id", "linea DESC"]);
    assert_eq!(r.indexes[1].key_columns, ["cliente"]);
    assert!(r.indexes.iter().all(|i| i.seeks == 0 && !i.unused));

    let table = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "pedidos").unwrap();
    let mut without = table.clone();
    without.indexes.retain(|i| i.name != "ix_fecha");
    let script = d.sync_script(&[TableChange::Alter { old: table, new: without }]).unwrap();
    println!("{script:#?}");
    assert_eq!(script.statements.len(), 1);
    run(&mut s, &script.statements[0]).await;
    let r = s.index_usage(&obj).await.unwrap().unwrap();
    assert!(r.indexes.iter().all(|i| i.name != "ix_fecha"));
    run(&mut admin, "DROP KEYSPACE IF EXISTS dbine_ixu").await;
}

#[tokio::test]
#[ignore]
async fn index_usage_scylladb_live() {
    let Ok(url) = std::env::var("DBINE_TEST_SCYLLADB_URL") else { return };
    check("scylladb", &url).await;
}

#[tokio::test]
#[ignore]
async fn index_usage_cassandra_live() {
    let Ok(url) = std::env::var("DBINE_TEST_CASSANDRA_URL") else { return };
    check("cassandra", &url).await;
}
