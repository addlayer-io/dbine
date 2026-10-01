//! "Índices" on a real SQLite file: a table with a primary key, a foreign
//! key and two indexes; one of them is read a few times. SQLite keeps no
//! usage counters, so the report lists everything with zeros and a note,
//! and the drop script that "Eliminar índice" generates removes the index.

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange};

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n---\n{sql}"));
}

#[tokio::test]
async fn index_usage_live() {
    let path = std::env::temp_dir().join(format!("dbine-ixu-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let d = dbine_driver_sqlite::drivers().remove(0);
    assert!(d.supports_index_usage());
    let cfg = ConnectionConfig { driver: "sqlite".into(), host: path.to_string_lossy().into(), ..Default::default() };
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(
        &mut s,
        "CREATE TABLE ixu_p (id INTEGER PRIMARY KEY);
         CREATE TABLE ixu_t (id INTEGER PRIMARY KEY, p_id INTEGER REFERENCES ixu_p(id), a INTEGER, b INTEGER);
         CREATE INDEX ixu_a ON ixu_t (a);
         CREATE INDEX ixu_b ON ixu_t (b);
         INSERT INTO ixu_p VALUES (1);
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 200)
         INSERT INTO ixu_t SELECT i, 1, i, i * 2 FROM n;",
    )
    .await;
    for i in 0..5 {
        run(&mut s, &format!("SELECT * FROM ixu_t WHERE a = {i}")).await;
    }
    let t = ObjectRef { kind: "table".into(), schema: None, name: "ixu_t".into() };
    let r = s.index_usage(&t).await.unwrap().expect("report");
    assert!(!r.stats_available && r.note.is_some());
    let names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["PRIMARY KEY", "ixu_a", "ixu_b"]);
    assert!(r.indexes[0].primary_key);
    assert!(r.indexes.iter().all(|i| i.reads == 0 && !i.unused && i.size_kb.is_some()));
    assert_eq!(r.foreign_keys.len(), 1);
    assert_eq!((r.foreign_keys[0].ref_table.as_str(), r.foreign_keys[0].ref_columns.clone()), ("ixu_p", vec!["id".to_string()]));

    // "Eliminar índice": the table against itself without the index.
    let old = s.database_schema().await.unwrap().into_iter().find(|x| x.name == "ixu_t").unwrap();
    let mut new = old.clone();
    new.indexes.retain(|i| i.name != "ixu_b");
    let script = d.sync_script(&[TableChange::Alter { old, new }]).unwrap();
    for st in &script.statements {
        run(&mut s, st).await;
    }
    let r = s.index_usage(&t).await.unwrap().unwrap();
    assert_eq!(r.indexes.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["PRIMARY KEY", "ixu_a"]);
    drop(s);
    let _ = std::fs::remove_file(&path);
}
