//! "Índices" through a real ODBC driver with the generic "odbc" preset: a
//! table with a primary key, a foreign key and two indexes, one of them
//! read a few times. The generic preset has no counters: the report lists
//! the key, the indexes (`SQLStatistics`) and the foreign keys with a note,
//! and the drop script "Eliminar índice" generates removes the index.
//!
//! ```sh
//! DBINE_TEST_ODBC_CONN='DRIVER={ODBC Driver 18 for SQL Server};SERVER=127.0.0.1,25741;UID=sa;PWD={Dbine_Odbc#2026};TrustServerCertificate=yes' \
//!   cargo test -p dbine-driver-odbc --test index_usage -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange};

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n---\n{sql}"));
}

#[tokio::test]
#[ignore]
async fn index_usage_generic_preset() {
    let Ok(conn) = std::env::var("DBINE_TEST_ODBC_CONN") else {
        eprintln!("DBINE_TEST_ODBC_CONN not set");
        return;
    };
    let cfg = ConnectionConfig { driver: "odbc".into(), options: [("connection_string".to_string(), conn)].into(), ..Default::default() };
    let d = dbine_driver_odbc::drivers().into_iter().find(|d| d.info().id == "odbc").unwrap();
    assert!(d.supports_index_usage());
    let mut s = d.connect(&cfg, None).await.expect("connect");
    run(&mut s, "IF OBJECT_ID('dbo.ixu_t') IS NOT NULL DROP TABLE dbo.ixu_t").await;
    run(&mut s, "IF OBJECT_ID('dbo.ixu_p') IS NOT NULL DROP TABLE dbo.ixu_p").await;
    run(&mut s, "CREATE TABLE dbo.ixu_p (id INT NOT NULL CONSTRAINT pk_ixu_p PRIMARY KEY)").await;
    run(
        &mut s,
        "CREATE TABLE dbo.ixu_t (id INT NOT NULL CONSTRAINT pk_ixu_t PRIMARY KEY, p_id INT CONSTRAINT fk_ixu_p REFERENCES dbo.ixu_p(id), a INT, b INT)",
    )
    .await;
    run(&mut s, "CREATE INDEX ixu_a ON dbo.ixu_t (a)").await;
    run(&mut s, "CREATE INDEX ixu_b ON dbo.ixu_t (b)").await;
    run(&mut s, "INSERT INTO dbo.ixu_p VALUES (1); INSERT INTO dbo.ixu_t VALUES (1, 1, 1, 2), (2, 1, 2, 4)").await;
    for i in 0..5 {
        run(&mut s, &format!("SELECT * FROM dbo.ixu_t WHERE a = {i}")).await;
    }
    let t = ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "ixu_t".into() };
    let r = s.index_usage(&t).await.unwrap().expect("report");
    eprintln!("{r:#?}");
    assert!(!r.stats_available && r.note.is_some());
    let names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["pk_ixu_t", "ixu_a", "ixu_b"]);
    assert!(r.indexes[0].primary_key);
    assert_eq!(r.foreign_keys.len(), 1);
    assert_eq!(r.foreign_keys[0].ref_table, "ixu_p");

    let old = s.database_schema().await.unwrap().into_iter().find(|x| x.name == "ixu_t").unwrap();
    let mut new = old.clone();
    new.indexes.retain(|i| i.name != "ixu_b");
    for st in d.sync_script(&[TableChange::Alter { old, new }]).unwrap().statements {
        run(&mut s, &st).await;
    }
    let r = s.index_usage(&t).await.unwrap().unwrap();
    assert_eq!(r.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["pk_ixu_t", "ixu_a"]);
    run(&mut s, "DROP TABLE dbo.ixu_t; DROP TABLE dbo.ixu_p").await;
}
