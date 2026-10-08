//! "Propiedades" of a database on a real file (no server needed, so it runs
//! by default): read, change journal, page size, auto vacuum and the
//! application's numbers, read again.

use dbine_driver::{ConnectionConfig, QueryOutcome};
use std::collections::BTreeMap;

fn changes(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test]
async fn sqlite_properties() {
    let path = std::env::temp_dir().join(format!("dbine-sqlite-props-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let d = dbine_driver_sqlite::drivers().pop().unwrap();
    assert!(d.capabilities().database_properties);
    let cfg = ConnectionConfig { driver: "sqlite".into(), host: path.display().to_string(), ..Default::default() };
    let mut s = d.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); INSERT INTO t (v) VALUES ('a'), ('b');", 10, &mut out).await.unwrap();

    let p = s.database_properties("main").await.unwrap();
    assert_eq!(p.values.get("journal_mode").map(String::as_str), Some("DELETE"));
    assert_eq!(p.values.get("page_size").map(String::as_str), Some("4096"));
    assert_eq!(p.values.get("auto_vacuum").map(String::as_str), Some("NONE"));
    assert_eq!(p.values.get("user_version").map(String::as_str), Some("0"));
    assert!(p.info.iter().any(|i| i.label == "Archivo" && i.value.ends_with(".db")), "{:?}", p.info);
    assert!(p.info.iter().any(|i| i.label == "Tablas" && i.value == "1"), "{:?}", p.info);
    assert!(p.warnings.contains_key("page_size"));

    let c = changes(&[("page_size", "8192"), ("auto_vacuum", "INCREMENTAL"), ("user_version", "42"), ("application_id", "1234"), ("journal_mode", "WAL")]);
    eprintln!("{}", d.alter_database_script("main", &c).unwrap());
    s.alter_database("main", &c).await.unwrap();
    let p = s.database_properties("main").await.unwrap();
    for (k, v) in [("page_size", "8192"), ("auto_vacuum", "INCREMENTAL"), ("user_version", "42"), ("application_id", "1234"), ("journal_mode", "WAL")] {
        assert_eq!(p.values.get(k).map(String::as_str), Some(v), "{k}");
    }

    // In WAL the page size doesn't change: SQLite accepts it silently, the
    // driver says so.
    let e = s.alter_database("main", &changes(&[("page_size", "4096")])).await.unwrap_err();
    assert!(e.to_string().contains("page_size quedó en «8192»"), "{e}");
    // Out of WAL in the same change, it does.
    s.alter_database("main", &changes(&[("page_size", "4096"), ("journal_mode", "DELETE")])).await.unwrap();
    let p = s.database_properties("main").await.unwrap();
    assert_eq!(p.values.get("page_size").map(String::as_str), Some("4096"));
    assert_eq!(p.values.get("journal_mode").map(String::as_str), Some("DELETE"));

    assert!(s.alter_database("main", &changes(&[("user_version", "1; DROP TABLE t")])).await.is_err());
    drop(s);
    let _ = std::fs::remove_file(&path);
}
