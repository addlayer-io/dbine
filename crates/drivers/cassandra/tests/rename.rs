//! "Renombrar…" against real servers (`DBINE_TEST_CASSANDRA_URL`,
//! `host:port`; `DBINE_TEST_SCYLLADB_URL` for ScyllaDB), skipped without it:
//!
//! ```sh
//! DBINE_TEST_CASSANDRA_URL=localhost:25402 DBINE_TEST_SCYLLADB_URL=localhost:25413 \
//!   cargo test -p dbine-driver-cassandra --test rename -- --ignored --nocapture
//! ```
//!
//! A key column is renamed and the table still answers by the new name; a
//! regular column and an indexed key column are refused before reaching the
//! server, and the server refuses the indexed one too.

use dbine_driver::rename::{RenameRequest, RenameTarget};
use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session};

const KS: &str = "dbine_rename";

async fn open(driver: &str, env: &str) -> Option<(std::sync::Arc<dyn dbine_driver::Driver>, Box<dyn Session>)> {
    let url = std::env::var(env).ok()?;
    let (host, port) = url.rsplit_once(':')?;
    let cfg = ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().ok()?, ..Default::default() };
    let d = dbine_driver_cassandra::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    let s = d.connect(&cfg, None).await.unwrap();
    Some((d, s))
}

async fn run(s: &mut Box<dyn Session>, cql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(cql, 100, &mut out).await.map(|_| out)
}

fn request(column: &str, new: &str, table: &dbine_driver::TableSchema) -> RenameRequest {
    RenameRequest {
        target: RenameTarget::Column { table: ObjectRef { kind: kinds::TABLE.into(), schema: Some(KS.into()), name: "t".into() }, column: column.into() },
        new_name: new.into(),
        table: Some(table.clone()),
        definition: None,
    }
}

async fn check(driver: &str, env: &str) {
    let Some((d, mut s)) = open(driver, env).await else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let spec = d.rename_spec().expect("renames key columns");
    assert!(spec.columns && spec.kinds.is_empty());
    let _ = run(&mut s, &format!("DROP KEYSPACE IF EXISTS {KS}")).await;
    // ScyllaDB: tablets take neither SimpleStrategy nor (on some versions) secondary indexes.
    let tablets = if driver == "scylladb" { " AND tablets = {'enabled': false}" } else { "" };
    run(&mut s, &format!("CREATE KEYSPACE {KS} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}{tablets}")).await.unwrap();
    run(&mut s, &format!("CREATE TABLE {KS}.t (id int, ck int, tagged int, pepe text, PRIMARY KEY (id, ck, tagged))")).await.unwrap();
    run(&mut s, &format!("CREATE INDEX t_tagged ON {KS}.t (tagged)")).await.unwrap();
    run(&mut s, &format!("INSERT INTO {KS}.t (id, ck, tagged, pepe) VALUES (1, 2, 3, 'x')")).await.unwrap();
    // database_schema reads the session's keyspace.
    let (host, port) = std::env::var(env).unwrap().rsplit_once(':').map(|(h, p)| (h.to_string(), p.parse().unwrap())).unwrap();
    let cfg = ConnectionConfig { driver: driver.into(), host, port, ..Default::default() };
    let mut in_ks = d.connect(&cfg, Some(KS)).await.unwrap();
    let table = in_ks.database_schema().await.unwrap().into_iter().find(|t| t.name == "t").expect("table t");

    // A primary key column (the partition key and a clustering column).
    for (old, new) in [("id", "user_id"), ("ck", "Fecha")] {
        let script = d.rename_script(&request(old, new, &table)).unwrap();
        eprintln!("{driver}: {script:?}");
        for st in &script.statements {
            run(&mut s, st).await.unwrap_or_else(|e| panic!("{st}: {e}"));
        }
    }
    let out = run(&mut s, &format!("SELECT user_id, \"Fecha\", pepe FROM {KS}.t WHERE user_id = 1")).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 1);

    // A regular column: refused with the Spanish message.
    let e = d.rename_script(&request("pepe", "juan", &table)).unwrap_err().to_string();
    assert!(e.contains("solo renombra columnas de la clave primaria") && e.contains("«pepe»"), "{e}");
    // An indexed key column: refused by DBine, and by the server too.
    let e = d.rename_script(&request("tagged", "etiqueta", &table)).unwrap_err().to_string();
    assert!(e.contains("t_tagged"), "{e}");
    let e = run(&mut s, &format!("ALTER TABLE {KS}.t RENAME tagged TO etiqueta")).await.unwrap_err();
    eprintln!("{driver}: server on an indexed column: {e}");
    // The server's own refusal of a regular column, which DBine prevents.
    let e = run(&mut s, &format!("ALTER TABLE {KS}.t RENAME pepe TO juan")).await.unwrap_err();
    eprintln!("{driver}: server on a regular column: {e}");

    run(&mut s, &format!("DROP KEYSPACE {KS}")).await.unwrap();
}

#[tokio::test]
#[ignore]
async fn cassandra() {
    check("cassandra", "DBINE_TEST_CASSANDRA_URL").await;
}

#[tokio::test]
#[ignore]
async fn scylladb() {
    check("scylladb", "DBINE_TEST_SCYLLADB_URL").await;
}
