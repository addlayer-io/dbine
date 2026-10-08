//! "Propiedades" of a database against a real server
//! (`DBINE_TEST_FIREBIRD_URL`), skipped without it. It creates a database
//! file next to the connected one, reads and changes it from the base
//! connection, and drops it:
//!
//! ```sh
//! DBINE_TEST_FIREBIRD_URL=firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb \
//!   cargo test -p dbine-driver-firebird --test properties -- --ignored --nocapture
//! ```

use dbine_driver::ConnectionConfig;
use std::collections::BTreeMap;

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_FIREBIRD_URL").ok()?;
    let rest = url.strip_prefix("firebird://")?;
    let (cred, addr) = rest.split_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (hostport, path) = addr.split_once('/')?;
    let (host, port) = hostport.split_once(':')?;
    Some(ConnectionConfig {
        driver: "firebird".into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: path.into(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    })
}

fn changes(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

#[tokio::test]
#[ignore]
async fn database_properties() {
    let Some(base) = config() else {
        eprintln!("DBINE_TEST_FIREBIRD_URL not set; skipping");
        return;
    };
    let d = dbine_driver_firebird::drivers().remove(0);
    assert!(d.capabilities().database_properties);
    let mut s = d.connect(&base, None).await.unwrap();
    let folder = base.database.rsplit_once('/').map(|(dir, _)| dir.to_string()).unwrap();
    let file = format!("{folder}/dbine_props.fdb");
    let _ = s.drop_database(&file).await;
    s.create_database(&file).await.unwrap();

    let p = s.database_properties(&file).await.unwrap();
    eprintln!("{:?}\n{:#?}", p.values, p.info);
    assert_eq!(p.values["charset"], "NONE");
    assert_eq!(p.values["linger"], "0");
    assert_eq!(p.values["sql_security"], "INVOKER");
    assert_eq!((p.values["publication"].as_str(), p.values["publication_all"].as_str()), ("", ""));
    assert!(p.info.iter().any(|i| i.label == "Archivo" && i.value == file));
    assert!(p.info.iter().any(|i| i.label == "Tamaño de página"));
    assert!(p.choices[0].values.contains(&"UTF8".to_string()));
    // The session's own database, with an empty name.
    assert_eq!(s.database_properties("").await.unwrap().info[0].value, base.database);

    let ch = changes(&[
        ("charset", "utf8"),
        ("comment", "ventas: it's"),
        ("linger", "30"),
        ("sql_security", "DEFINER"),
        ("publication", "true"),
        ("publication_all", "true"),
    ]);
    eprintln!("{}", d.alter_database_script(&file, &ch).unwrap());
    s.alter_database(&file, &ch).await.unwrap();
    let p = s.database_properties(&file).await.unwrap();
    for (k, v) in [
        ("charset", "UTF8"),
        ("comment", "ventas: it's"),
        ("linger", "30"),
        ("sql_security", "DEFINER"),
        ("publication", "true"),
        ("publication_all", "true"),
    ] {
        assert_eq!(p.values.get(k).map(String::as_str), Some(v), "{k}");
    }

    s.alter_database(&file, &changes(&[("comment", ""), ("linger", "0"), ("publication", ""), ("publication_all", "")])).await.unwrap();
    let p = s.database_properties(&file).await.unwrap();
    for k in ["comment", "publication", "publication_all"] {
        assert_eq!(p.values[k], "", "{k}");
    }
    assert_eq!(p.values["linger"], "0");

    let e = s.alter_database(&file, &changes(&[("charset", "NO_SUCH_CS"), ("comment", "x")])).await.unwrap_err();
    eprintln!("{e}");
    s.drop_database(&file).await.unwrap();
}
