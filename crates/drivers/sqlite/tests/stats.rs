//! Row estimates and object comments on a real file (no server needed, so
//! it runs by default): none before `ANALYZE`, the counts after it.

use dbine_driver::{ConnectionConfig, QueryOutcome};

#[tokio::test]
async fn sqlite_row_estimates() {
    let path = std::env::temp_dir().join(format!("dbine-sqlite-stats-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let d = dbine_driver_sqlite::drivers().pop().unwrap();
    let cfg = ConnectionConfig { driver: "sqlite".into(), host: path.display().to_string(), ..Default::default() };
    let mut s = d.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE TABLE plain (v TEXT);
         CREATE TABLE indexed (id INTEGER PRIMARY KEY, k TEXT);
         CREATE INDEX indexed_k ON indexed (k);
         CREATE VIEW v AS SELECT * FROM plain;
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 250)
         INSERT INTO plain SELECT 'x' || i FROM n;
         WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 40)
         INSERT INTO indexed (k) SELECT 'k' || (i % 7) FROM n;",
        10,
        &mut out,
    )
    .await
    .unwrap();

    // No sqlite_stat1 yet: nothing, and nothing counted.
    assert!(s.row_estimates().await.unwrap().is_empty());

    s.execute("ANALYZE", 10, &mut out).await.unwrap();
    let mut got: Vec<(String, String, u64)> =
        s.row_estimates().await.unwrap().into_iter().map(|e| (e.object.kind, e.object.name, e.rows)).collect();
    got.sort();
    assert_eq!(got, vec![("table".into(), "indexed".into(), 40), ("table".into(), "plain".into(), 250)]);

    assert!(s.object_comments().await.unwrap().is_empty());
    drop(s);
    let _ = std::fs::remove_file(&path);
}
