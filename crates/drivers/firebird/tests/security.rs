//! `Session::privileges` against a real server, as the database's owner:
//! each privilege listed is granted on an object of each of its kinds.
//!
//! ```sh
//! DBINE_TEST_FIREBIRD_URL=firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb \
//!   cargo test -p dbine-driver-firebird --test security -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, SecurityAction, Session};

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_FIREBIRD_URL").ok()?;
    let rest = url
        .strip_prefix("firebird://")
        .expect("firebird://user:pass@host:port/path");
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, path) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    Some(ConnectionConfig {
        driver: "firebird".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        database: path.into(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    })
}

/// The error of a statement, if it failed.
async fn exec(s: &mut Box<dyn Session>, sql: &str) -> Option<String> {
    let mut out = QueryOutcome::default();
    match s.execute(sql, 100, &mut out).await {
        Err(e) => Some(e.to_string()),
        Ok(_) => out.error.map(|e| format!("{e:?}")),
    }
}

#[tokio::test]
#[ignore]
async fn lists_what_it_can_grant() {
    let Some(cfg) = config() else {
        eprintln!("DBINE_TEST_FIREBIRD_URL not set; skipping");
        return;
    };
    let d = dbine_driver_firebird::drivers().remove(0);
    let kinds = d.security().unwrap().object_kinds;
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let list = s.privileges().await.unwrap();
    let on = |n: &str| {
        list.iter()
            .find(|p| p.name == n)
            .map(|p| p.on.iter().map(String::as_str).collect::<Vec<_>>())
    };
    assert_eq!(on("SELECT"), Some(vec!["table", "view"]));
    assert_eq!(
        on("EXECUTE"),
        Some(vec!["procedure", "function", "package"])
    );
    assert_eq!(on("USAGE"), Some(vec!["sequence"]));
    assert_eq!(on("CREATE TABLE"), Some(vec![""]));
    for p in &list {
        assert!(!p.on.is_empty(), "{}", p.name);
        assert!(
            p.on.iter().all(|k| kinds.contains(&k.as_str())),
            "{}: {:?}",
            p.name,
            p.on
        );
    }

    let drop = [
        "DROP ROLE DBINE_PRIVR",
        "DROP PACKAGE DBINE_PK",
        "DROP FUNCTION DBINE_PF",
        "DROP PROCEDURE DBINE_PP",
        "DROP VIEW DBINE_PV",
        "DROP TABLE DBINE_PT",
        "DROP SEQUENCE DBINE_PS",
    ];
    for sql in drop {
        exec(&mut s, sql).await;
    }
    for sql in [
        "CREATE ROLE DBINE_PRIVR",
        "CREATE TABLE DBINE_PT (ID INTEGER NOT NULL PRIMARY KEY)",
        "CREATE VIEW DBINE_PV AS SELECT ID FROM DBINE_PT",
        "CREATE PROCEDURE DBINE_PP AS BEGIN END",
        "CREATE FUNCTION DBINE_PF RETURNS INTEGER AS BEGIN RETURN 1; END",
        "CREATE PACKAGE DBINE_PK AS BEGIN END",
        "CREATE SEQUENCE DBINE_PS",
    ] {
        assert_eq!(exec(&mut s, sql).await, None, "{sql}");
    }
    let object = |kind: &str| ObjectRef {
        kind: kind.into(),
        schema: None,
        name: match kind {
            "table" => "DBINE_PT",
            "view" => "DBINE_PV",
            "procedure" => "DBINE_PP",
            "function" => "DBINE_PF",
            "package" => "DBINE_PK",
            _ => "DBINE_PS",
        }
        .into(),
    };
    let mut failed = Vec::new();
    for p in &list {
        // Kept in the security database, for its users.
        if p.name == "CREATE DATABASE" {
            continue;
        }
        for k in &p.on {
            let sql = d
                .security_script(&SecurityAction::Grant {
                    privileges: vec![p.name.clone()],
                    object: (!k.is_empty()).then(|| object(k)),
                    to: "DBINE_PRIVR".into(),
                    grantable: false,
                })
                .unwrap();
            if let Some(e) = exec(&mut s, &sql).await {
                failed.push(format!("{sql}: {e}"));
            }
        }
    }
    for sql in drop {
        exec(&mut s, sql).await;
    }
    assert!(failed.is_empty(), "{failed:#?}");
}
