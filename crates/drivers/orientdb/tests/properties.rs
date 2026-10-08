//! "Propiedades" of a database against a real server
//! (`DBINE_TEST_ORIENTDB_URL`, `user:pass@host:port`), skipped without it:
//!
//! ```sh
//! DBINE_TEST_ORIENTDB_URL=root:dbine-test-pass@localhost:22480 \
//!   cargo test -p dbine-driver-orientdb --test properties -- --ignored --nocapture
//! ```

use dbine_driver::ConnectionConfig;
use std::collections::BTreeMap;

fn changes(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn orientdb_properties() {
    let Ok(url) = std::env::var("DBINE_TEST_ORIENTDB_URL") else {
        eprintln!("DBINE_TEST_ORIENTDB_URL not set; skipping");
        return;
    };
    let (auth, hp) = url.rsplit_once('@').unwrap();
    let (user, pass) = auth.split_once(':').unwrap();
    let (host, port) = hp.rsplit_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: "orientdb".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    let d = dbine_driver_orientdb::drivers().into_iter().find(|d| d.info().id == "orientdb").unwrap();
    assert!(d.capabilities().database_properties);
    let mut s = d.connect(&cfg, None).await.unwrap();
    let _ = s.drop_database("dbine_props").await;
    s.create_database("dbine_props").await.unwrap();

    let p = s.database_properties("dbine_props").await.unwrap();
    eprintln!("{:#?}\n{:?}", p.info, p.values);
    assert!(p.info.iter().any(|i| i.label == "Almacenamiento" && i.value == "plocal"));
    assert!(p.info.iter().any(|i| i.label == "Tamaño"));
    assert_eq!(p.values.get("validation").map(String::as_str), Some("true"));
    assert_eq!(p.values.get("strict_sql").map(String::as_str), Some("true"));
    assert!(p.values.contains_key("timezone") && p.values.contains_key("charset"));
    assert!(p.warnings.contains_key("validation"));

    let ch = changes(&[
        ("timezone", "America/Argentina/Buenos_Aires"),
        ("locale_language", "es"),
        ("locale_country", "AR"),
        ("date_format", "dd/MM/yyyy"),
        ("datetime_format", "dd/MM/yyyy HH:mm:ss"),
        ("cluster_selection", "balanced"),
        ("minimum_clusters", "2"),
        ("conflict_strategy", "automerge"),
        ("validation", ""),
        ("strict_sql", ""),
        ("new_custom_name", "dbineNota"),
        ("new_custom_value", "it's ok"),
    ]);
    eprintln!("{}", d.alter_database_script("dbine_props", &ch).unwrap());
    s.alter_database("dbine_props", &ch).await.unwrap();

    let p = s.database_properties("dbine_props").await.unwrap();
    for (k, v) in [
        ("timezone", "America/Argentina/Buenos_Aires"),
        ("locale_language", "es"),
        ("locale_country", "AR"),
        ("date_format", "dd/MM/yyyy"),
        ("datetime_format", "dd/MM/yyyy HH:mm:ss"),
        ("cluster_selection", "balanced"),
        ("minimum_clusters", "2"),
        ("conflict_strategy", "automerge"),
        ("validation", ""),
        ("strict_sql", ""),
        ("custom:dbineNota", "it's ok"),
    ] {
        assert_eq!(p.values.get(k).map(String::as_str), Some(v), "{k}");
    }
    assert!(p.fields.iter().any(|f| f.key == "custom:dbineNota"));

    // An existing custom attribute changes through its own field.
    s.alter_database("dbine_props", &changes(&[("custom:dbineNota", "otra"), ("validation", "true")])).await.unwrap();
    let p = s.database_properties("dbine_props").await.unwrap();
    assert_eq!(p.values.get("custom:dbineNota").map(String::as_str), Some("otra"));
    assert_eq!(p.values.get("validation").map(String::as_str), Some("true"));

    assert!(s.alter_database("dbine_props", &changes(&[("charset", "UTF 8")])).await.is_err());
    s.drop_database("dbine_props").await.unwrap();
}
