//! Users, roles and permissions against a real server, as a DBA
//! (`DBINE_TEST_ORACLE_ADMIN_URL`) and, for the fallback without the DBA_
//! views, as a plain user (`DBINE_TEST_ORACLE_URL`, see tests/integration.rs):
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//! DBINE_TEST_ORACLE_URL=oracle://dbine:Dbine123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test security -- --ignored
//! ```

use dbine_driver::{
    ConnectionConfig, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session,
};

fn config(env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.strip_prefix("oracle://")?;
    let (cred, addr) = rest.split_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (hostport, service) = addr.split_once('/')?;
    let (host, port) = hostport.split_once(':')?;
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    Some(cfg)
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

async fn try_run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    let _ = s.execute(sql, 100, &mut out).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn users_roles_and_grants() {
    let Some(cfg) = config("DBINE_TEST_ORACLE_ADMIN_URL") else {
        return;
    };
    let d = dbine_driver_oracle::drivers().remove(0);
    let mut s = d.connect(&cfg, None).await.unwrap();
    try_run(&mut s, "DROP USER DBINE_ANA CASCADE").await;
    try_run(&mut s, "DROP ROLE DBINE_LECT").await;
    try_run(&mut s, "DROP USER DBINE_SEC CASCADE").await;
    run(
        &mut s,
        "CREATE USER DBINE_SEC NO AUTHENTICATION QUOTA UNLIMITED ON USERS",
    )
    .await;
    run(
        &mut s,
        "CREATE TABLE DBINE_SEC.FACTURAS (ID NUMBER PRIMARY KEY, TOTAL NUMBER(10,2))",
    )
    .await;

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(
        &mut s,
        &script(SecurityAction::CreateUser {
            name: "dbine_ana".into(),
            password: Some("Pw_12345x".into()),
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
            role: "DBINE_LECT".into(),
            member: "DBINE_ANA".into(),
        }),
    )
    .await;
    let table = ObjectRef {
        kind: "table".into(),
        schema: Some("DBINE_SEC".into()),
        name: "FACTURAS".into(),
    };
    run(
        &mut s,
        &script(SecurityAction::Grant {
            privileges: vec!["SELECT".into()],
            object: Some(table.clone()),
            to: "DBINE_LECT".into(),
            grantable: false,
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::Grant {
            privileges: vec!["UPDATE".into()],
            object: Some(table.clone()),
            to: "DBINE_ANA".into(),
            grantable: true,
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::Grant {
            privileges: vec!["CREATE VIEW".into()],
            object: None,
            to: "DBINE_ANA".into(),
            grantable: false,
        }),
    )
    .await;

    let all = s.principals().await.unwrap();
    let ana = all
        .iter()
        .find(|p| p.name == "DBINE_ANA")
        .expect("the user");
    assert_eq!(ana.kind, PrincipalKind::User);
    assert_eq!(ana.member_of, vec!["DBINE_LECT".to_string()]);
    assert_eq!(
        (ana.disabled, ana.can_login, ana.system),
        (Some(false), Some(true), false)
    );
    assert!(all
        .iter()
        .any(|p| p.name == "DBINE_LECT" && p.kind == PrincipalKind::Role));
    assert!(all
        .iter()
        .any(|p| p.name == "SYS" && p.system && p.superuser == Some(true)));
    assert_eq!(
        all.iter()
            .find(|p| p.name == "DBINE_SEC")
            .unwrap()
            .can_login,
        Some(false)
    );

    let g = s.grants("DBINE_ANA").await.unwrap();
    let upd = g
        .iter()
        .find(|x| x.privilege == "UPDATE")
        .unwrap_or_else(|| panic!("direct UPDATE: {g:?}"));
    assert_eq!(
        (
            upd.object.as_deref(),
            upd.object_kind.as_deref(),
            upd.grantable,
            upd.via.as_deref()
        ),
        (Some("DBINE_SEC.FACTURAS"), Some("table"), true, None)
    );
    assert!(g
        .iter()
        .any(|x| x.privilege == "CREATE VIEW" && x.object.is_none() && x.via.is_none()));
    assert!(g
        .iter()
        .any(|x| x.privilege == "CREATE SESSION" && x.via.is_none()));
    let sel = g
        .iter()
        .find(|x| x.privilege == "SELECT")
        .expect("SELECT through the role");
    assert_eq!(sel.via.as_deref(), Some("DBINE_LECT"));

    run(
        &mut s,
        &script(SecurityAction::Revoke {
            privileges: vec!["UPDATE".into()],
            object: Some(table),
            from: "DBINE_ANA".into(),
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::Revoke {
            privileges: vec!["CREATE VIEW".into()],
            object: None,
            from: "DBINE_ANA".into(),
        }),
    )
    .await;
    let g = s.grants("DBINE_ANA").await.unwrap();
    assert!(
        g.iter()
            .all(|x| x.privilege != "UPDATE" && x.privilege != "CREATE VIEW"),
        "{g:?}"
    );
    run(
        &mut s,
        &script(SecurityAction::SetPassword {
            name: "DBINE_ANA".into(),
            password: "Otra_Pw_987".into(),
        }),
    )
    .await;

    // The fallback without DBA_ views, as the new user itself.
    let mut own = cfg.clone();
    own.username = Some("DBINE_ANA".into());
    own.password = Some("Otra_Pw_987".into());
    let mut a = d.connect(&own, None).await.unwrap();
    let mine = a.principals().await.unwrap();
    let me = mine
        .iter()
        .find(|p| p.name == "DBINE_ANA")
        .expect("own user");
    assert_eq!(me.member_of, vec!["DBINE_LECT".to_string()]);
    assert!(me.details.iter().any(|(k, _)| k == "Nota"));
    let g = a.grants("DBINE_ANA").await.unwrap();
    assert!(
        g.iter()
            .any(|x| x.privilege == "SELECT" && x.via.as_deref() == Some("DBINE_LECT")),
        "{g:?}"
    );
    drop(a);

    run(
        &mut s,
        &script(SecurityAction::SetLogin {
            name: "DBINE_ANA".into(),
            enabled: false,
        }),
    )
    .await;
    assert_eq!(
        s.principals()
            .await
            .unwrap()
            .iter()
            .find(|p| p.name == "DBINE_ANA")
            .unwrap()
            .disabled,
        Some(true)
    );
    run(
        &mut s,
        &script(SecurityAction::SetLogin {
            name: "DBINE_ANA".into(),
            enabled: true,
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::RemoveMember {
            role: "DBINE_LECT".into(),
            member: "DBINE_ANA".into(),
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::Drop {
            name: "DBINE_ANA".into(),
            kind: PrincipalKind::User,
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::Drop {
            name: "DBINE_LECT".into(),
            kind: PrincipalKind::Role,
        }),
    )
    .await;
    assert!(s
        .principals()
        .await
        .unwrap()
        .iter()
        .all(|p| p.name != "DBINE_ANA" && p.name != "DBINE_LECT"));
    run(&mut s, "DROP USER DBINE_SEC CASCADE").await;
}

/// What the server can grant, read as a plain user (the maps are PUBLIC's)
/// and as a DBA; as the DBA, each object privilege is granted on an object
/// of each of its kinds.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn lists_what_it_can_grant() {
    let d = dbine_driver_oracle::drivers().remove(0);
    let kinds = d.security().unwrap().object_kinds;
    for env in ["DBINE_TEST_ORACLE_URL", "DBINE_TEST_ORACLE_ADMIN_URL"] {
        let Some(cfg) = config(env) else {
            continue;
        };
        let mut s = d.connect(&cfg, None).await.unwrap();
        let list = s.privileges().await.unwrap();
        assert!(list.len() > 100, "{env}: {}", list.len());
        let on = |n: &str| list.iter().find(|p| p.name == n).map(|p| p.on.clone());
        assert_eq!(on("CREATE SESSION"), Some(vec![String::new()]));
        assert_eq!(on("SELECT ANY TABLE"), Some(vec![String::new()]));
        assert!(on("SELECT").unwrap().contains(&"sequence".to_string()));
        assert_eq!(
            on("EXECUTE"),
            Some(vec![
                "procedure".into(),
                "function".into(),
                "package".into()
            ])
        );
        assert_eq!(on("SYSRAC"), None);
        for p in &list {
            assert!(!p.on.is_empty(), "{}", p.name);
            assert!(
                p.on.iter().all(|k| kinds.contains(&k.as_str())),
                "{}: {:?}",
                p.name,
                p.on
            );
        }
    }

    let Some(cfg) = config("DBINE_TEST_ORACLE_ADMIN_URL") else {
        return;
    };
    let mut s = d.connect(&cfg, None).await.unwrap();
    let list = s.privileges().await.unwrap();
    try_run(&mut s, "DROP USER DBINE_PRIVT CASCADE").await;
    try_run(&mut s, "DROP USER DBINE_PRIVU CASCADE").await;
    run(
        &mut s,
        "CREATE USER DBINE_PRIVT NO AUTHENTICATION QUOTA UNLIMITED ON USERS",
    )
    .await;
    run(
        &mut s,
        "GRANT CREATE TABLE, CREATE MATERIALIZED VIEW TO DBINE_PRIVT",
    )
    .await;
    run(&mut s, "CREATE USER DBINE_PRIVU NO AUTHENTICATION").await;
    for sql in [
        "CREATE TABLE DBINE_PRIVT.T (ID NUMBER PRIMARY KEY)",
        "CREATE VIEW DBINE_PRIVT.V AS SELECT ID FROM DBINE_PRIVT.T",
        "CREATE MATERIALIZED VIEW DBINE_PRIVT.MV AS SELECT ID FROM DBINE_PRIVT.T",
        "CREATE PROCEDURE DBINE_PRIVT.P AS BEGIN NULL; END;",
        "CREATE FUNCTION DBINE_PRIVT.F RETURN NUMBER AS BEGIN RETURN 1; END;",
        "CREATE PACKAGE DBINE_PRIVT.PK AS X NUMBER; END;",
        "CREATE SEQUENCE DBINE_PRIVT.S",
    ] {
        run(&mut s, sql).await;
    }
    let object = |kind: &str| {
        let name = match kind {
            "table" => "T",
            "view" => "V",
            "materialized_view" => "MV",
            "procedure" => "P",
            "function" => "F",
            "package" => "PK",
            _ => "S",
        };
        ObjectRef {
            kind: kind.into(),
            schema: Some("DBINE_PRIVT".into()),
            name: name.into(),
        }
    };
    let mut failed = Vec::new();
    for p in &list {
        // SIGN takes blockchain and immutable tables only; UNDER, object
        // views.
        if p.name == "SIGN" || p.name == "UNDER" {
            continue;
        }
        for k in p.on.iter().filter(|k| !k.is_empty()) {
            let sql = d
                .security_script(&SecurityAction::Grant {
                    privileges: vec![p.name.clone()],
                    object: Some(object(k)),
                    to: "DBINE_PRIVU".into(),
                    grantable: false,
                })
                .unwrap();
            let mut out = QueryOutcome::default();
            let r = s.execute(&sql, 100, &mut out).await;
            if r.is_err() || out.error.is_some() {
                failed.push(format!("{sql}: {:?} {:?}", r.err(), out.error));
            }
        }
    }
    run(&mut s, "DROP USER DBINE_PRIVU CASCADE").await;
    run(&mut s, "DROP USER DBINE_PRIVT CASCADE").await;
    assert!(failed.is_empty(), "{failed:#?}");
}
