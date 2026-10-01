//! "Índices" against a real Flight SQL server (GizmoSQL, DuckDB behind): a
//! table with a primary key, a foreign key and two indexes, one of them
//! read a few times. No engine behind Flight SQL reports per-index
//! counters: the report lists the key, the indexes (DuckDB's catalog) and
//! the foreign keys with zeros and a note. Flight SQL has no schema sync,
//! so the index is dropped with a plain `DROP INDEX`.
//!
//! ```sh
//! DBINE_TEST_FLIGHTSQL_URL=http://localhost:25337 cargo test -p dbine-driver-flightsql --test index_usage -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session};

fn cfg() -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var("DBINE_TEST_FLIGHTSQL_URL").ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: "flightsql".into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(std::env::var("DBINE_TEST_FLIGHTSQL_USER").unwrap_or_else(|_| "gizmosql_user".into())),
        password: Some(std::env::var("DBINE_TEST_FLIGHTSQL_PASSWORD").unwrap_or_else(|_| "secreto1".into())),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n---\n{sql}"));
}

#[tokio::test]
#[ignore]
async fn index_usage_live() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_FLIGHTSQL_URL not set");
        return;
    };
    let d = dbine_driver_flightsql::drivers().remove(0);
    assert!(d.supports_index_usage());
    assert!(!d.supports_schema_sync());
    let mut s = d.connect(&c, None).await.unwrap();
    run(&mut s, "DROP TABLE IF EXISTS ixu_t").await;
    run(&mut s, "DROP TABLE IF EXISTS ixu_p").await;
    run(&mut s, "CREATE TABLE ixu_p (id INTEGER PRIMARY KEY)").await;
    run(&mut s, "CREATE TABLE ixu_t (id INTEGER PRIMARY KEY, p_id INTEGER REFERENCES ixu_p(id), a INTEGER, b INTEGER)").await;
    run(&mut s, "CREATE INDEX ixu_a ON ixu_t (a)").await;
    run(&mut s, "CREATE INDEX ixu_b ON ixu_t (b)").await;
    run(&mut s, "INSERT INTO ixu_p VALUES (1)").await;
    run(&mut s, "INSERT INTO ixu_t SELECT i, 1, i, i * 2 FROM range(1, 201) r(i)").await;
    for i in 0..5 {
        run(&mut s, &format!("SELECT * FROM ixu_t WHERE a = {i}")).await;
    }
    let t = ObjectRef { kind: "table".into(), schema: Some("main".into()), name: "ixu_t".into() };
    let r = s.index_usage(&t).await.unwrap().expect("report");
    eprintln!("{r:#?}");
    assert!(!r.stats_available && r.note.is_some());
    let names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["PRIMARY KEY", "ixu_a", "ixu_b"]);
    assert!(r.indexes[0].primary_key && r.indexes[0].key_columns == ["id"]);
    assert_eq!(r.indexes[1].key_columns, ["a"]);
    assert!(r.indexes.iter().all(|i| i.reads == 0 && !i.unused));
    assert_eq!(r.foreign_keys.len(), 1);
    assert_eq!((r.foreign_keys[0].ref_table.as_str(), r.foreign_keys[0].ref_columns.clone()), ("ixu_p", vec!["id".to_string()]));

    run(&mut s, "DROP INDEX ixu_b").await;
    let r = s.index_usage(&t).await.unwrap().unwrap();
    assert_eq!(r.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["PRIMARY KEY", "ixu_a"]);
    run(&mut s, "DROP TABLE ixu_t").await;
    run(&mut s, "DROP TABLE ixu_p").await;
}
