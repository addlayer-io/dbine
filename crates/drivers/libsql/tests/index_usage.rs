//! "Índices" against a real libSQL server: a table with a primary key, a
//! foreign key and two indexes, one of them read a few times. libSQL (like
//! SQLite) keeps no usage counters: the report lists everything with zeros
//! and a note, and the drop script "Eliminar índice" generates removes the
//! index on the server.
//!
//! ```sh
//! DBINE_TEST_LIBSQL_URL=http://localhost:25880 cargo test -p dbine-driver-libsql --test index_usage -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange};

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n---\n{sql}"));
}

#[tokio::test]
#[ignore]
async fn index_usage_live() {
    let Ok(url) = std::env::var("DBINE_TEST_LIBSQL_URL") else {
        eprintln!("DBINE_TEST_LIBSQL_URL not set");
        return;
    };
    let d = dbine_driver_libsql::drivers().remove(0);
    assert!(d.supports_index_usage());
    let cfg = ConnectionConfig { driver: "libsql".into(), host: url, ..Default::default() };
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(&mut s, "DROP TABLE IF EXISTS ixu_t; DROP TABLE IF EXISTS ixu_p;").await;
    run(
        &mut s,
        "CREATE TABLE ixu_p (id INTEGER PRIMARY KEY);
         CREATE TABLE ixu_t (id INTEGER PRIMARY KEY, p_id INTEGER REFERENCES ixu_p(id), a INTEGER, b INTEGER);
         CREATE INDEX ixu_a ON ixu_t (a);
         CREATE INDEX ixu_b ON ixu_t (b);
         INSERT INTO ixu_p VALUES (1);
         INSERT INTO ixu_t VALUES (1, 1, 1, 2), (2, 1, 2, 4), (3, 1, 3, 6);",
    )
    .await;
    for i in 0..5 {
        run(&mut s, &format!("SELECT * FROM ixu_t WHERE a = {i}")).await;
    }
    let t = ObjectRef { kind: "table".into(), schema: None, name: "ixu_t".into() };
    let r = s.index_usage(&t).await.unwrap().expect("report");
    eprintln!("{r:#?}");
    assert!(!r.stats_available && r.note.is_some());
    let names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["PRIMARY KEY", "ixu_a", "ixu_b"]);
    assert!(r.indexes[0].primary_key);
    assert!(r.indexes.iter().all(|i| i.reads == 0 && !i.unused));
    assert_eq!(r.foreign_keys.len(), 1);
    assert_eq!(r.foreign_keys[0].ref_table, "ixu_p");

    let old = s.database_schema().await.unwrap().into_iter().find(|x| x.name == "ixu_t").unwrap();
    let mut new = old.clone();
    new.indexes.retain(|i| i.name != "ixu_b");
    for st in d.sync_script(&[TableChange::Alter { old, new }]).unwrap().statements {
        run(&mut s, &st).await;
    }
    let r = s.index_usage(&t).await.unwrap().unwrap();
    assert_eq!(r.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["PRIMARY KEY", "ixu_a"]);
    run(&mut s, "DROP TABLE ixu_t; DROP TABLE ixu_p;").await;
}
