//! "Propiedades" of a database on a temporary file (DuckDB is embedded, so
//! no server or env var is needed): facts only, nothing to change.

use dbine_driver::{ConnectionConfig, QueryOutcome};

#[tokio::test]
async fn duckdb_properties() {
    let path = std::env::temp_dir().join(format!("dbine-duck-props-{}.duckdb", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let d = dbine_driver_duckdb::drivers().remove(0);
    assert!(d.capabilities().database_properties);
    let cfg = ConnectionConfig { driver: "duckdb".into(), host: path.to_string_lossy().into(), ..Default::default() };
    let mut s = d.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("CREATE TABLE t AS SELECT range AS i FROM range(1000); CHECKPOINT;", 10, &mut out).await.unwrap();
    let db = s.list_databases().await.unwrap().remove(0);

    let p = s.database_properties(&db).await.unwrap();
    assert!(p.fields.is_empty());
    let get = |l: &str| p.info.iter().find(|i| i.label == l).map(|i| i.value.clone());
    assert!(get("Archivo").is_some_and(|f| f.ends_with(".duckdb")), "{:?}", p.info);
    assert_eq!(get("Tablas").as_deref(), Some("1"));
    assert_eq!(get("Filas (estimadas)").as_deref(), Some("1000"));
    assert!(get("Tamaño").is_some() && get("Bloques usados").is_some(), "{:?}", p.info);
    assert!(s.alter_database(&db, &[("x".to_string(), "1".to_string())].into()).await.is_err());
    drop(s);
    let _ = std::fs::remove_file(&path);
}
