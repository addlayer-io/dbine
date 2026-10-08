//! Row estimates and object comments on a temporary file (DuckDB is
//! embedded, so no server or env var is needed).

use dbine_driver::{ConnectionConfig, QueryOutcome};

#[tokio::test]
async fn duckdb_row_estimates_and_comments() {
    let path = std::env::temp_dir().join(format!("dbine-duck-stats-{}.duckdb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let d = dbine_driver_duckdb::drivers().remove(0);
    let cfg = ConnectionConfig { driver: "duckdb".into(), host: path.to_string_lossy().into(), ..Default::default() };
    let mut s = d.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "CREATE TABLE t AS SELECT range AS i FROM range(1000);
         CREATE TABLE empty_t (a INT);
         CREATE VIEW v AS SELECT * FROM t;
         CREATE MACRO twice(x) AS x * 2;
         CREATE SEQUENCE seq;
         CREATE TYPE mood AS ENUM ('ok', 'bad');
         COMMENT ON VIEW v IS 'Vista de t';
         COMMENT ON MACRO twice IS 'El doble';
         COMMENT ON SEQUENCE seq IS 'Ids';
         COMMENT ON TYPE mood IS 'Ánimo';
         CHECKPOINT;",
        10,
        &mut out,
    )
    .await
    .unwrap();

    let objects = s.list_objects().await.unwrap();
    let mut rows: Vec<(String, u64)> = s.row_estimates().await.unwrap().into_iter().map(|e| (e.object.name, e.rows)).collect();
    rows.sort();
    assert_eq!(rows, vec![("empty_t".into(), 0), ("t".into(), 1000)]);

    let mut comments: Vec<(String, String, String)> =
        s.object_comments().await.unwrap().into_iter().map(|c| (c.object.kind, c.object.name, c.comment)).collect();
    comments.sort();
    assert_eq!(
        comments,
        vec![
            ("function".into(), "twice".into(), "El doble".into()),
            ("sequence".into(), "seq".into(), "Ids".into()),
            ("type".into(), "mood".into(), "Ánimo".into()),
            ("view".into(), "v".into(), "Vista de t".into()),
        ]
    );
    // The kinds match list_objects, so the app can pair them.
    for (kind, name, _) in &comments {
        assert!(objects.iter().any(|o| &o.kind == kind && &o.name == name), "{kind} {name} not in list_objects");
    }
    drop(s);
    let _ = std::fs::remove_file(&path);
}
