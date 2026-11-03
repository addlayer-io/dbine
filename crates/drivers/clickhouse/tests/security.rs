//! Users, roles and permissions against a real server
//! (`DBINE_TEST_CLICKHOUSE_URL`, see tests/integration.rs; the container
//! needs `CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=1`):
//! `cargo test -p dbine-driver-clickhouse --test security -- --ignored`
//!
//! What a server can grant is also read from 22.8
//! (`DBINE_TEST_CLICKHOUSE_22_URL`, tests/old_server.rs) and Timeplus
//! Proton (`DBINE_TEST_TIMEPLUS_URL`).

use dbine_driver::{
    ConnectionConfig, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session,
};

fn cfg() -> Option<ConnectionConfig> {
    cfg_from("DBINE_TEST_CLICKHOUSE_URL", "clickhouse")
}

fn cfg_from(env: &str, driver: &str) -> Option<ConnectionConfig> {
    let url = reqwest::Url::parse(&std::env::var(env).ok()?).expect("URL");
    Some(ConnectionConfig {
        driver: driver.into(),
        host: url.host_str()?.into(),
        port: url.port().unwrap_or(0),
        username: Some(url.username().to_string()).filter(|u| !u.is_empty()),
        password: url.password().map(str::to_string),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn users_roles_and_grants() {
    let Some(cfg) = cfg() else { return };
    let d = dbine_driver_clickhouse::drivers()
        .into_iter()
        .find(|d| d.info().id == "clickhouse")
        .unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(&mut s, "DROP USER IF EXISTS dbine_ana; DROP ROLE IF EXISTS dbine_lect; DROP DATABASE IF EXISTS dbine_sec; CREATE DATABASE dbine_sec").await;
    run(&mut s, "CREATE TABLE dbine_sec.facturas (id Int32, total Decimal(10, 2)) ENGINE = MergeTree ORDER BY id").await;

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(
        &mut s,
        &script(SecurityAction::CreateUser {
            name: "dbine_ana".into(),
            password: Some("Pw_12345!x".into()),
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::CreateRole {
            name: "dbine_lect".into(),
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::AddMember {
            role: "dbine_lect".into(),
            member: "dbine_ana".into(),
        }),
    )
    .await;
    let table = ObjectRef {
        kind: "table".into(),
        schema: Some("dbine_sec".into()),
        name: "facturas".into(),
    };
    run(
        &mut s,
        &script(SecurityAction::Grant {
            privileges: vec!["SELECT".into()],
            object: Some(table.clone()),
            to: "dbine_lect".into(),
            grantable: false,
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::Grant {
            privileges: vec!["INSERT".into()],
            object: Some(table.clone()),
            to: "dbine_ana".into(),
            grantable: true,
        }),
    )
    .await;

    let all = s.principals().await.unwrap();
    let ana = all
        .iter()
        .find(|p| p.name == "dbine_ana")
        .unwrap_or_else(|| panic!("the user: {all:?}"));
    assert_eq!(ana.kind, PrincipalKind::User);
    assert_eq!(ana.member_of, vec!["dbine_lect".to_string()]);
    assert_eq!(
        (ana.disabled, ana.system, ana.superuser),
        (Some(false), false, Some(false))
    );
    assert!(all
        .iter()
        .any(|p| p.name == "dbine_lect" && p.kind == PrincipalKind::Role));
    // The container's admin is defined in users.xml.
    assert!(
        all.iter()
            .any(|p| p.name == "dbine" && p.system && p.superuser == Some(true)),
        "{all:?}"
    );

    let g = s.grants("dbine_ana").await.unwrap();
    let ins = g
        .iter()
        .find(|x| x.privilege == "INSERT")
        .unwrap_or_else(|| panic!("direct INSERT: {g:?}"));
    assert_eq!(
        (
            ins.object.as_deref(),
            ins.object_kind.as_deref(),
            ins.grantable,
            ins.via.as_deref()
        ),
        (Some("dbine_sec.facturas"), Some("table"), true, None)
    );
    let sel = g
        .iter()
        .find(|x| x.privilege == "SELECT")
        .expect("SELECT through the role");
    assert_eq!(sel.via.as_deref(), Some("dbine_lect"));

    run(
        &mut s,
        &script(SecurityAction::Revoke {
            privileges: vec!["INSERT".into()],
            object: Some(table),
            from: "dbine_ana".into(),
        }),
    )
    .await;
    assert!(s
        .grants("dbine_ana")
        .await
        .unwrap()
        .iter()
        .all(|x| x.privilege != "INSERT"));
    run(
        &mut s,
        &script(SecurityAction::SetPassword {
            name: "dbine_ana".into(),
            password: "Otra_Pw_987!".into(),
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::SetLogin {
            name: "dbine_ana".into(),
            enabled: false,
        }),
    )
    .await;
    assert_eq!(
        s.principals()
            .await
            .unwrap()
            .iter()
            .find(|p| p.name == "dbine_ana")
            .unwrap()
            .disabled,
        Some(true)
    );
    run(
        &mut s,
        &script(SecurityAction::SetLogin {
            name: "dbine_ana".into(),
            enabled: true,
        }),
    )
    .await;
    assert_eq!(
        s.principals()
            .await
            .unwrap()
            .iter()
            .find(|p| p.name == "dbine_ana")
            .unwrap()
            .disabled,
        Some(false)
    );
    run(
        &mut s,
        &script(SecurityAction::RemoveMember {
            role: "dbine_lect".into(),
            member: "dbine_ana".into(),
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::Drop {
            name: "dbine_ana".into(),
            kind: PrincipalKind::User,
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::Drop {
            name: "dbine_lect".into(),
            kind: PrincipalKind::Role,
        }),
    )
    .await;
    assert!(s
        .principals()
        .await
        .unwrap()
        .iter()
        .all(|p| p.name != "dbine_ana" && p.name != "dbine_lect"));
    run(&mut s, "DROP DATABASE dbine_sec").await;
}

/// The server's list of what it can grant: not empty, with `expect` in it
/// (names only that version has show it came from the catalog), each on
/// kinds the tab offers. With `grant`, every one is granted on each of its
/// kinds to a scratch role; the only refusals allowed are the test login's
/// own missing rights (`ALL … WITH GRANT OPTION`, named collections).
async fn lists_privileges(env: &str, driver: &str, expect: &[&str], grant: bool) {
    let Some(cfg) = cfg_from(env, driver) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = dbine_driver_clickhouse::drivers()
        .into_iter()
        .find(|d| d.info().id == driver)
        .unwrap();
    let kinds = d.security().unwrap().object_kinds;
    let mut s = d.connect(&cfg, None).await.unwrap();
    let privileges = s.privileges().await.unwrap();
    assert!(privileges.len() > 100, "{}", privileges.len());
    for name in expect {
        assert!(privileges.iter().any(|p| p.name == *name), "{name}");
    }
    for p in &privileges {
        assert!(!p.on.is_empty(), "{}", p.name);
        for k in &p.on {
            assert!(kinds.contains(&k.as_str()), "{} on {k}", p.name);
        }
    }
    let on = |n: &str| privileges.iter().find(|p| p.name == n).unwrap().on.clone();
    let table = if driver == "timeplus" {
        "stream"
    } else {
        "table"
    };
    assert!(on("SELECT").iter().any(|k| k == table));
    assert!(on("dictGet").iter().any(|k| k == "dictionary"));
    assert_eq!(on("KILL QUERY"), [""]);
    assert_eq!(on("CREATE DATABASE"), ["", "schema"]);
    if !grant {
        return;
    }
    run(
        &mut s,
        "DROP ROLE IF EXISTS dbine_privs; CREATE ROLE dbine_privs",
    )
    .await;
    let (mut granted, mut denied) = (0, 0);
    for p in &privileges {
        for k in &p.on {
            let object = match k.as_str() {
                "" => None,
                "schema" => Some(ObjectRef {
                    kind: k.clone(),
                    schema: None,
                    name: "dbine_sec".into(),
                }),
                _ => Some(ObjectRef {
                    kind: k.clone(),
                    schema: Some("dbine_sec".into()),
                    name: "x".into(),
                }),
            };
            let sql = d
                .security_script(&SecurityAction::Grant {
                    privileges: vec![p.name.clone()],
                    object,
                    to: "dbine_privs".into(),
                    grantable: false,
                })
                .unwrap();
            let mut out = QueryOutcome::default();
            let error = match s.execute(&sql, 100, &mut out).await {
                Err(e) => Some(e.to_string()),
                Ok(()) => out.error.clone(),
            };
            match error {
                None => granted += 1,
                Some(e) if e.contains("Not enough privileges") => denied += 1,
                Some(e) => panic!("{sql}: {e}"),
            }
        }
    }
    run(&mut s, "DROP ROLE dbine_privs").await;
    assert!(
        granted > 300 && denied < 40,
        "{granted} granted, {denied} denied"
    );
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn lists_what_it_can_grant() {
    lists_privileges(
        "DBINE_TEST_CLICKHOUSE_URL",
        "clickhouse",
        &["SELECT", "S3", "ALTER VIEW MODIFY QUERY", "TABLE ENGINE"],
        true,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn lists_what_22_8_can_grant() {
    // SHOW CACHES became SHOW FILESYSTEM CACHES: only 22.8's catalog has it.
    lists_privileges(
        "DBINE_TEST_CLICKHOUSE_22_URL",
        "clickhouse",
        &["SELECT", "S3", "SHOW CACHES"],
        false,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn lists_what_timeplus_can_grant() {
    lists_privileges(
        "DBINE_TEST_TIMEPLUS_URL",
        "timeplus",
        &["SELECT", "SHOW ALERTS"],
        false,
    )
    .await;
}
