//! "Nueva base de datos" (a schema-only account) with options, against a
//! real server, as a user with CREATE USER (`DBINE_TEST_ORACLE_ADMIN_URL`),
//! skipped without it:
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test create_database -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::Value;
use std::collections::BTreeMap;

fn config(url: &str) -> ConnectionConfig {
    let rest = url.strip_prefix("oracle://").expect("oracle://user:pass@host:port/service");
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, service) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    cfg
}

async fn text(s: &mut Box<dyn Session>, sql: &str) -> String {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    match &out.results.last().unwrap().rows[0][0] {
        Value::String(v) => v.clone(),
        v => v.to_string(),
    }
}

const ACCOUNT: &str = "SELECT u.default_tablespace || '|' || u.temporary_tablespace || '|' || NVL(TO_CHAR(q.max_bytes), 'none')
    FROM dba_users u LEFT JOIN dba_ts_quotas q ON q.username = u.username AND q.tablespace_name = u.default_tablespace
    WHERE u.username = ";

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn schema_options() {
    let Ok(url) = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL") else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    let d = dbine_driver_oracle::drivers().remove(0);
    let mut s = d.connect(&config(&url), None).await.unwrap();
    for n in ["DBINE_CREATE_OPTS", "DBINE_CREATE_PLAIN", "DBINE_CREATE_TEMP"] {
        let _ = s.drop_database(n).await;
    }

    let choices = s.create_database_choices().await.unwrap();
    eprintln!("{choices:?}");
    let get = |k: &str| choices.iter().find(|c| c.key == k).unwrap();
    let default_ts = get("default_tablespace").default.clone().expect("default tablespace");
    assert!(get("default_tablespace").values.contains(&"SYSAUX".to_string()));
    let temp = get("temporary_tablespace").default.clone().expect("default temp");
    assert!(get("temporary_tablespace").values.contains(&temp));

    // Named tablespace and a quota: one CREATE USER.
    let options: BTreeMap<String, String> =
        [("default_tablespace", "sysaux"), ("temporary_tablespace", temp.as_str()), ("quota", "10M")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    eprintln!("{}", d.create_database_script("dbine_create_opts", &options).unwrap());
    s.create_database_with("dbine_create_opts", &options).await.unwrap();
    assert_eq!(text(&mut s, &format!("{ACCOUNT}'DBINE_CREATE_OPTS'")).await, format!("SYSAUX|{temp}|10485760"));

    // Only a quota: the block finds the default tablespace.
    let options: BTreeMap<String, String> = [("quota", "5M")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    eprintln!("{}", d.create_database_script("dbine_create_temp", &options).unwrap());
    s.create_database_with("dbine_create_temp", &options).await.unwrap();
    assert_eq!(text(&mut s, &format!("{ACCOUNT}'DBINE_CREATE_TEMP'")).await, format!("{default_ts}|{temp}|5242880"));

    // No options: the plain create (unlimited quota, -1).
    s.create_database_with("dbine_create_plain", &BTreeMap::new()).await.unwrap();
    assert_eq!(text(&mut s, &format!("{ACCOUNT}'DBINE_CREATE_PLAIN'")).await, format!("{default_ts}|{temp}|-1"));

    for n in ["DBINE_CREATE_OPTS", "DBINE_CREATE_PLAIN", "DBINE_CREATE_TEMP"] {
        s.drop_database(n).await.unwrap();
    }
}
