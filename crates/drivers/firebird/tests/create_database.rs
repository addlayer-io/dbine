//! "Nueva base de datos" with options, against a real server
//! (`DBINE_TEST_FIREBIRD_URL`), skipped without it:
//!
//! ```sh
//! DBINE_TEST_FIREBIRD_URL=firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb \
//!   cargo test -p dbine-driver-firebird --test create_database -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome};
use serde_json::Value;
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

#[tokio::test]
#[ignore]
async fn file_page_size_and_charset() {
    let Some(base) = config() else {
        eprintln!("DBINE_TEST_FIREBIRD_URL not set; skipping");
        return;
    };
    let d = dbine_driver_firebird::drivers().remove(0);
    let mut s = d.connect(&base, None).await.unwrap();
    let choices = s.create_database_choices().await.unwrap();
    let get = |k: &str| choices.iter().find(|c| c.key == k).unwrap();
    assert!(get("charset").values.iter().any(|c| c == "WIN1252"));
    let dir = get("folder").values.first().cloned().expect("the current database's folder");
    assert_eq!(dir, base.database.rsplit_once('/').unwrap().0);

    let path = format!("{dir}/dbine_create_opts.fdb");
    let _ = s.drop_database(&path).await;
    let options: BTreeMap<String, String> =
        [("folder", dir.as_str()), ("page_size", "16384"), ("charset", "win1252")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    eprintln!("{}", d.create_database_script("dbine_create_opts", &options).unwrap());
    s.create_database_with("dbine_create_opts", &options).await.unwrap();

    let mut n = d.connect(&ConnectionConfig { database: path.clone(), ..base.clone() }, None).await.unwrap();
    let mut out = QueryOutcome::default();
    n.execute("SELECT MON$PAGE_SIZE, TRIM(RDB$CHARACTER_SET_NAME) FROM MON$DATABASE CROSS JOIN RDB$DATABASE", 10, &mut out).await.unwrap();
    let row = &out.results.last().unwrap().rows[0];
    eprintln!("{row:?}");
    assert_eq!(row[0].to_string(), "16384");
    assert_eq!(row[1], Value::String("WIN1252".into()));
    drop(n);
    s.drop_database(&path).await.unwrap();

    // Without options: the plain create, by path.
    let plain = format!("{dir}/dbine_create_plain.fdb");
    let _ = s.drop_database(&plain).await;
    s.create_database_with(&plain, &BTreeMap::new()).await.unwrap();
    s.drop_database(&plain).await.unwrap();
}
