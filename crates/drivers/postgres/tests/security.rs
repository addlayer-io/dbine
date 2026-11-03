//! Users, roles and permissions against real servers. Each test reads
//! `DBINE_TEST_<ENGINE>_URL` (`postgres://user:pass@host:port/db`) and is
//! skipped without it:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//! DBINE_TEST_TIMESCALEDB_URL=postgres://postgres:pw@localhost:25015/postgres \
//! DBINE_TEST_YUGABYTEDB_URL=postgres://yugabyte@localhost:25016/yugabyte \
//! DBINE_TEST_OPENGAUSS_URL='postgres://gaussdb:Dbine@1234@localhost:25020/postgres' \
//! DBINE_TEST_CLOUDBERRY_URL=postgres://gpadmin:pw@localhost:25017/postgres \
//! DBINE_TEST_GREENGAGE_URL=postgres://gpadmin:pw@localhost:25018/postgres \
//! DBINE_TEST_MATERIALIZE_URL=postgres://materialize@localhost:25024/materialize \
//! DBINE_TEST_H2_URL=postgres://sa:sa@localhost:25025/test \
//! DBINE_TEST_RISINGWAVE_URL=postgres://root@localhost:4566/dev \
//! DBINE_TEST_CRATEDB_URL=postgres://crate@localhost:25021/doc \
//!   cargo test -p dbine-driver-postgres --test security -- --ignored
//! ```

use dbine_driver::{
    ConnectionConfig, Driver, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session,
};
use std::sync::Arc;

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
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

/// Runs the same steps as SQL Server's test: create a user and a role,
/// membership, grants on a table (direct and through the role), a
/// database-wide grant, revoke, password, NOLOGIN, drop. `passwords`:
/// false where the server refuses them (CockroachDB in insecure mode).
async fn users_roles_and_grants(id: &str, env: &str, passwords: bool) {
    let Some(cfg) = cfg(id, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d: Arc<dyn Driver> = dbine_driver_postgres::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap();
    assert!(d.security().is_some());
    let mut s = d.connect(&cfg, None).await.unwrap();
    // Leftovers of an interrupted run.
    for stmt in [
        "DROP TABLE IF EXISTS dbine_sec_facturas",
        "DROP OWNED BY dbine_ana",
        "DROP OWNED BY dbine_lectores",
        "DROP ROLE IF EXISTS dbine_ana",
        "DROP ROLE IF EXISTS dbine_lectores",
    ] {
        let mut out = QueryOutcome::default();
        let _ = s.execute(stmt, 10, &mut out).await;
    }
    run(
        &mut s,
        "CREATE TABLE dbine_sec_facturas (id int PRIMARY KEY, total numeric)",
    )
    .await;
    let schema: String = {
        let mut out = QueryOutcome::default();
        s.execute("SELECT current_schema()", 1, &mut out)
            .await
            .unwrap();
        out.results[0].rows[0][0].as_str().unwrap().to_string()
    };

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(
        &mut s,
        &script(SecurityAction::CreateUser {
            name: "dbine_ana".into(),
            password: passwords.then(|| "Pw_12345!x".into()),
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::CreateRole {
            name: "dbine_lectores".into(),
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::AddMember {
            role: "dbine_lectores".into(),
            member: "dbine_ana".into(),
        }),
    )
    .await;
    let table = ObjectRef {
        kind: "table".into(),
        schema: Some(schema.clone()),
        name: "dbine_sec_facturas".into(),
    };
    run(
        &mut s,
        &script(SecurityAction::Grant {
            privileges: vec!["SELECT".into()],
            object: Some(table.clone()),
            to: "dbine_lectores".into(),
            grantable: false,
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::Grant {
            privileges: vec!["UPDATE".into()],
            object: Some(table.clone()),
            to: "dbine_ana".into(),
            grantable: true,
        }),
    )
    .await;
    // Database-wide grants, where the engine's scripts can name the database.
    let database_grants = d.security().unwrap().object_kinds.contains(&"");
    if database_grants {
        run(
            &mut s,
            &script(SecurityAction::Grant {
                privileges: vec!["CONNECT".into()],
                object: None,
                to: "dbine_ana".into(),
                grantable: false,
            }),
        )
        .await;
    }

    let all = s.principals().await.unwrap();
    let ana = all
        .iter()
        .find(|p| p.name == "dbine_ana")
        .expect("the user");
    assert_eq!(ana.kind, PrincipalKind::User);
    assert_eq!(ana.member_of, vec!["dbine_lectores".to_string()]);
    assert_eq!(
        (ana.can_login, ana.disabled, ana.system),
        (Some(true), Some(false), false)
    );
    assert!(all
        .iter()
        .any(|p| p.name == "dbine_lectores" && p.kind == PrincipalKind::Role));
    assert!(
        all.iter().any(|p| p.system && p.superuser == Some(true)),
        "the bootstrap superuser: {all:?}"
    );

    let g = s.grants("dbine_ana").await.unwrap();
    let object = format!("{schema}.dbine_sec_facturas");
    let upd = g
        .iter()
        .find(|x| x.privilege == "UPDATE")
        .expect("direct UPDATE");
    assert_eq!(
        (
            upd.object.as_deref(),
            upd.object_kind.as_deref(),
            upd.grantable,
            upd.via.as_deref()
        ),
        (Some(object.as_str()), Some("table"), true, None)
    );
    let sel = g
        .iter()
        .find(|x| x.privilege == "SELECT")
        .expect("SELECT through the role");
    assert_eq!(
        (sel.object.as_deref(), sel.via.as_deref()),
        (Some(object.as_str()), Some("dbine_lectores"))
    );
    if database_grants {
        let conn = g
            .iter()
            .find(|x| x.privilege == "CONNECT")
            .expect("CONNECT on the database");
        assert_eq!(
            (conn.object_kind.as_deref(), conn.via.as_deref()),
            (Some("database"), None)
        );
    }
    assert_eq!(
        s.grants("dbine_lectores").await.unwrap().len(),
        1,
        "the role only has SELECT"
    );

    run(
        &mut s,
        &script(SecurityAction::Revoke {
            privileges: vec!["UPDATE".into()],
            object: Some(table.clone()),
            from: "dbine_ana".into(),
        }),
    )
    .await;
    if database_grants {
        run(
            &mut s,
            &script(SecurityAction::Revoke {
                privileges: vec!["CONNECT".into()],
                object: None,
                from: "dbine_ana".into(),
            }),
        )
        .await;
    }
    let g = s.grants("dbine_ana").await.unwrap();
    assert!(
        g.iter()
            .all(|x| x.privilege != "UPDATE" && x.privilege != "CONNECT"),
        "{g:?}"
    );
    if passwords {
        // A quote checks the escaping; openGauss doesn't take it in passwords.
        let new_password = if id == "opengauss" {
            "Otra_Pw_987!"
        } else {
            "Otra_Pw_9'87!"
        };
        run(
            &mut s,
            &script(SecurityAction::SetPassword {
                name: "dbine_ana".into(),
                password: new_password.into(),
            }),
        )
        .await;
    }
    run(
        &mut s,
        &script(SecurityAction::SetLogin {
            name: "dbine_ana".into(),
            enabled: false,
        }),
    )
    .await;
    let ana = s
        .principals()
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.name == "dbine_ana")
        .unwrap();
    assert_eq!(ana.can_login, Some(false));
    run(
        &mut s,
        &script(SecurityAction::SetLogin {
            name: "dbine_ana".into(),
            enabled: true,
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::RemoveMember {
            role: "dbine_lectores".into(),
            member: "dbine_ana".into(),
        }),
    )
    .await;
    assert!(s
        .principals()
        .await
        .unwrap()
        .iter()
        .find(|p| p.name == "dbine_ana")
        .unwrap()
        .member_of
        .is_empty());
    assert!(s.grants("dbine_ana").await.unwrap().is_empty());
    run(
        &mut s,
        &script(SecurityAction::Revoke {
            privileges: vec!["SELECT".into()],
            object: Some(table),
            from: "dbine_lectores".into(),
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
            name: "dbine_lectores".into(),
            kind: PrincipalKind::Role,
        }),
    )
    .await;
    assert!(s
        .principals()
        .await
        .unwrap()
        .iter()
        .all(|p| p.name != "dbine_ana" && p.name != "dbine_lectores"));
    run(&mut s, "DROP TABLE dbine_sec_facturas").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn postgres() {
    users_roles_and_grants("postgres", "DBINE_TEST_POSTGRES_URL", true).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cockroachdb() {
    users_roles_and_grants("cockroachdb", "DBINE_TEST_COCKROACH_URL", false).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn timescaledb() {
    users_roles_and_grants("timescaledb", "DBINE_TEST_TIMESCALEDB_URL", true).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn yugabytedb() {
    users_roles_and_grants("yugabytedb", "DBINE_TEST_YUGABYTEDB_URL", true).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn opengauss() {
    users_roles_and_grants("opengauss", "DBINE_TEST_OPENGAUSS_URL", true).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn greenplum_family() {
    users_roles_and_grants("cloudberry", "DBINE_TEST_CLOUDBERRY_URL", true).await;
    users_roles_and_grants("greengage", "DBINE_TEST_GREENGAGE_URL", true).await;
}

/// Materialize: no WITH GRANT OPTION, server-wide grants are SYSTEM
/// privileges, and the emulator takes no passwords.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn materialize() {
    let Some(cfg) = cfg("materialize", "DBINE_TEST_MATERIALIZE_URL") else {
        eprintln!("DBINE_TEST_MATERIALIZE_URL not set; skipping");
        return;
    };
    let d: Arc<dyn Driver> = dbine_driver_postgres::drivers()
        .into_iter()
        .find(|d| d.info().id == "materialize")
        .unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    for stmt in [
        "DROP TABLE IF EXISTS dbine_sec_facturas",
        "DROP OWNED BY dbine_ana",
        "DROP OWNED BY dbine_lectores",
        "DROP ROLE IF EXISTS dbine_ana",
        "DROP ROLE IF EXISTS dbine_lectores",
    ] {
        let mut out = QueryOutcome::default();
        let _ = s.execute(stmt, 10, &mut out).await;
    }
    run(
        &mut s,
        "CREATE TABLE dbine_sec_facturas (id int, total numeric)",
    )
    .await;
    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(
        &mut s,
        &script(SecurityAction::CreateUser {
            name: "dbine_ana".into(),
            password: None,
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::CreateRole {
            name: "dbine_lectores".into(),
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::AddMember {
            role: "dbine_lectores".into(),
            member: "dbine_ana".into(),
        }),
    )
    .await;
    let table = ObjectRef {
        kind: "table".into(),
        schema: Some("public".into()),
        name: "dbine_sec_facturas".into(),
    };
    run(
        &mut s,
        &script(SecurityAction::Grant {
            privileges: vec!["SELECT".into()],
            object: Some(table.clone()),
            to: "dbine_lectores".into(),
            grantable: false,
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::Grant {
            privileges: vec!["UPDATE".into()],
            object: Some(table.clone()),
            to: "dbine_ana".into(),
            grantable: false,
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::Grant {
            privileges: vec!["CREATEROLE".into()],
            object: None,
            to: "dbine_ana".into(),
            grantable: false,
        }),
    )
    .await;

    let all = s.principals().await.unwrap();
    let ana = all
        .iter()
        .find(|p| p.name == "dbine_ana")
        .expect("the user");
    assert_eq!(
        (ana.kind, ana.member_of.clone()),
        (PrincipalKind::User, vec!["dbine_lectores".to_string()])
    );
    assert!(all
        .iter()
        .any(|p| p.name == "dbine_lectores" && p.kind == PrincipalKind::Role));
    assert!(all.iter().any(|p| p.name == "mz_system" && p.system));

    let g = s.grants("dbine_ana").await.unwrap();
    let upd = g
        .iter()
        .find(|x| x.privilege == "UPDATE")
        .expect("direct UPDATE");
    assert_eq!(
        (
            upd.object.as_deref(),
            upd.object_kind.as_deref(),
            upd.via.as_deref()
        ),
        (Some("public.dbine_sec_facturas"), Some("table"), None)
    );
    let sel = g
        .iter()
        .find(|x| x.privilege == "SELECT")
        .expect("SELECT through the role");
    assert_eq!(sel.via.as_deref(), Some("dbine_lectores"));
    let sys = g
        .iter()
        .find(|x| x.privilege == "CREATEROLE")
        .expect("the SYSTEM privilege");
    assert_eq!((sys.object.as_deref(), sys.via.as_deref()), (None, None));

    run(
        &mut s,
        &script(SecurityAction::Revoke {
            privileges: vec!["UPDATE".into()],
            object: Some(table.clone()),
            from: "dbine_ana".into(),
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::Revoke {
            privileges: vec!["CREATEROLE".into()],
            object: None,
            from: "dbine_ana".into(),
        }),
    )
    .await;
    assert!(s
        .grants("dbine_ana")
        .await
        .unwrap()
        .iter()
        .all(|x| x.privilege == "SELECT" || x.privilege == "USAGE"));
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
            .can_login,
        Some(false)
    );
    run(
        &mut s,
        &script(SecurityAction::RemoveMember {
            role: "dbine_lectores".into(),
            member: "dbine_ana".into(),
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::Revoke {
            privileges: vec!["SELECT".into()],
            object: Some(table),
            from: "dbine_lectores".into(),
        }),
    )
    .await;
    run(&mut s, "DROP OWNED BY dbine_ana").await;
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
            name: "dbine_lectores".into(),
            kind: PrincipalKind::Role,
        }),
    )
    .await;
    assert!(s
        .principals()
        .await
        .unwrap()
        .iter()
        .all(|p| p.name != "dbine_ana" && p.name != "dbine_lectores"));
    run(&mut s, "DROP TABLE dbine_sec_facturas").await;
}

async fn cleanup(s: &mut Box<dyn Session>, stmts: &[&str]) {
    for stmt in stmts {
        let mut out = QueryOutcome::default();
        let _ = s.execute(stmt, 10, &mut out).await;
    }
}

/// H2 in `-pg` mode (`DBINE_TEST_H2_URL=postgres://sa:sa@localhost:25025/test`):
/// its own INFORMATION_SCHEMA, ADMIN and ALTER ANY SCHEMA as database-wide
/// rights, no WITH GRANT OPTION and no disabled users.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn h2() {
    let Some(cfg) = cfg("h2", "DBINE_TEST_H2_URL") else {
        eprintln!("DBINE_TEST_H2_URL not set; skipping");
        return;
    };
    let d: Arc<dyn Driver> = dbine_driver_postgres::drivers()
        .into_iter()
        .find(|d| d.info().id == "h2")
        .unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    cleanup(
        &mut s,
        &[
            "DROP VIEW IF EXISTS dbine_sec_v",
            "DROP TABLE IF EXISTS dbine_sec_facturas",
            "DROP USER IF EXISTS dbine_ana",
            "DROP ROLE IF EXISTS dbine_lectores",
        ],
    )
    .await;
    run(
        &mut s,
        "CREATE TABLE dbine_sec_facturas (id int PRIMARY KEY, total numeric)",
    )
    .await;
    run(
        &mut s,
        "CREATE VIEW dbine_sec_v AS SELECT * FROM dbine_sec_facturas",
    )
    .await;
    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    assert!(d
        .security_script(&SecurityAction::CreateUser {
            name: "dbine_ana".into(),
            password: None
        })
        .is_err());
    run(
        &mut s,
        &script(SecurityAction::CreateUser {
            name: "dbine_ana".into(),
            password: Some("Pw_1'x".into()),
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::CreateRole {
            name: "dbine_lectores".into(),
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::AddMember {
            role: "dbine_lectores".into(),
            member: "dbine_ana".into(),
        }),
    )
    .await;
    let table = ObjectRef {
        kind: "table".into(),
        schema: Some("public".into()),
        name: "dbine_sec_facturas".into(),
    };
    let view = ObjectRef {
        kind: "view".into(),
        schema: Some("public".into()),
        name: "dbine_sec_v".into(),
    };
    let schema = ObjectRef {
        kind: "schema".into(),
        schema: None,
        name: "public".into(),
    };
    let grant = |p: &str, o: Option<&ObjectRef>, to: &str| SecurityAction::Grant {
        privileges: vec![p.into()],
        object: o.cloned(),
        to: to.into(),
        grantable: false,
    };
    run(
        &mut s,
        &script(grant("SELECT", Some(&table), "dbine_lectores")),
    )
    .await;
    run(&mut s, &script(grant("UPDATE", Some(&table), "dbine_ana"))).await;
    run(&mut s, &script(grant("SELECT", Some(&view), "dbine_ana"))).await;
    run(&mut s, &script(grant("INSERT", Some(&schema), "dbine_ana"))).await;
    let database = SecurityAction::Grant {
        privileges: vec!["ADMIN".into(), "ALTER ANY SCHEMA".into()],
        object: None,
        to: "dbine_ana".into(),
        grantable: false,
    };
    // Two statements in one script, as the tab runs it.
    run(&mut s, &script(database)).await;

    let all = s.principals().await.unwrap();
    let ana = all
        .iter()
        .find(|p| p.name == "dbine_ana")
        .expect("the user");
    assert_eq!(
        (ana.kind, ana.superuser, ana.system),
        (PrincipalKind::User, Some(true), false)
    );
    assert_eq!(ana.member_of, vec!["dbine_lectores".to_string()]);
    assert!(all
        .iter()
        .any(|p| p.name == "dbine_lectores" && p.kind == PrincipalKind::Role));
    assert!(all.iter().any(|p| p.name == "public" && p.system));

    let g = s.grants("dbine_ana").await.unwrap();
    let find = |p: &str, kind: Option<&str>| {
        g.iter()
            .find(|x| x.privilege == p && x.object_kind.as_deref() == kind)
            .unwrap_or_else(|| panic!("{p} {kind:?}: {g:?}"))
    };
    let upd = find("UPDATE", Some("table"));
    assert_eq!(
        (upd.object.as_deref(), upd.via.as_deref()),
        (Some("public.dbine_sec_facturas"), None)
    );
    assert_eq!(
        find("SELECT", Some("view")).object.as_deref(),
        Some("public.dbine_sec_v")
    );
    assert_eq!(
        find("INSERT", Some("schema")).object.as_deref(),
        Some("public")
    );
    assert_eq!(find("ALTER ANY SCHEMA", None).object, None);
    assert_eq!(find("ADMIN", None).via, None);
    let sel = find("SELECT", Some("table"));
    assert_eq!(
        (sel.object.as_deref(), sel.via.as_deref()),
        (Some("public.dbine_sec_facturas"), Some("dbine_lectores"))
    );
    assert_eq!(
        s.grants("dbine_lectores").await.unwrap().len(),
        1,
        "the role only has SELECT"
    );

    let revoke = |p: &str, o: Option<&ObjectRef>| SecurityAction::Revoke {
        privileges: vec![p.into()],
        object: o.cloned(),
        from: "dbine_ana".into(),
    };
    run(&mut s, &script(revoke("UPDATE", Some(&table)))).await;
    run(&mut s, &script(revoke("SELECT", Some(&view)))).await;
    run(&mut s, &script(revoke("INSERT", Some(&schema)))).await;
    run(&mut s, &script(revoke("ADMIN", None))).await;
    run(&mut s, &script(revoke("ALTER ANY SCHEMA", None))).await;
    let g = s.grants("dbine_ana").await.unwrap();
    assert!(g.iter().all(|x| x.via.is_some()), "{g:?}");
    run(
        &mut s,
        &script(SecurityAction::SetPassword {
            name: "dbine_ana".into(),
            password: "Otra_9'87".into(),
        }),
    )
    .await;
    assert!(d
        .security_script(&SecurityAction::SetLogin {
            name: "dbine_ana".into(),
            enabled: false
        })
        .is_err());
    run(
        &mut s,
        &script(SecurityAction::RemoveMember {
            role: "dbine_lectores".into(),
            member: "dbine_ana".into(),
        }),
    )
    .await;
    let all = s.principals().await.unwrap();
    assert!(all
        .iter()
        .find(|p| p.name == "dbine_ana")
        .unwrap()
        .member_of
        .is_empty());
    assert_eq!(
        all.iter()
            .find(|p| p.name == "dbine_ana")
            .unwrap()
            .superuser,
        Some(false)
    );
    assert!(s.grants("dbine_ana").await.unwrap().is_empty());
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
            name: "dbine_lectores".into(),
            kind: PrincipalKind::Role,
        }),
    )
    .await;
    assert!(s
        .principals()
        .await
        .unwrap()
        .iter()
        .all(|p| p.name != "dbine_ana" && p.name != "dbine_lectores"));
    run(&mut s, "DROP VIEW dbine_sec_v").await;
    run(&mut s, "DROP TABLE dbine_sec_facturas").await;
}

/// RisingWave (`DBINE_TEST_RISINGWAVE_URL=postgres://root@localhost:4566/dev`):
/// users only, grants on tables, materialized views, sources, sinks,
/// schemas and a named database read from the `rw_catalog` ACLs.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn risingwave() {
    let Some(cfg) = cfg("risingwave", "DBINE_TEST_RISINGWAVE_URL") else {
        eprintln!("DBINE_TEST_RISINGWAVE_URL not set; skipping");
        return;
    };
    let d: Arc<dyn Driver> = dbine_driver_postgres::drivers()
        .into_iter()
        .find(|d| d.info().id == "risingwave")
        .unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    cleanup(
        &mut s,
        &[
            "DROP SINK IF EXISTS dbine_sec_sink",
            "DROP SOURCE IF EXISTS dbine_sec_src",
            "DROP VIEW IF EXISTS dbine_sec_v",
            "DROP MATERIALIZED VIEW IF EXISTS dbine_sec_mv",
            "DROP TABLE IF EXISTS dbine_sec_facturas",
            "DROP USER IF EXISTS dbine_ana",
        ],
    )
    .await;
    run(
        &mut s,
        "CREATE TABLE dbine_sec_facturas (id int PRIMARY KEY, total numeric)",
    )
    .await;
    run(
        &mut s,
        "CREATE MATERIALIZED VIEW dbine_sec_mv AS SELECT count(*) AS n FROM dbine_sec_facturas",
    )
    .await;
    run(
        &mut s,
        "CREATE VIEW dbine_sec_v AS SELECT id FROM dbine_sec_facturas",
    )
    .await;
    run(
        &mut s,
        "CREATE SINK dbine_sec_sink FROM dbine_sec_facturas WITH (connector = 'blackhole')",
    )
    .await;
    run(
        &mut s,
        "CREATE SOURCE dbine_sec_src (id int) WITH (connector = 'datagen', datagen.rows.per.second = '1') FORMAT PLAIN ENCODE JSON",
    )
    .await;
    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    assert!(d
        .security_script(&SecurityAction::CreateRole {
            name: "dbine_lectores".into()
        })
        .is_err());
    run(
        &mut s,
        &script(SecurityAction::CreateUser {
            name: "dbine_ana".into(),
            password: Some("Pw_1'x".into()),
        }),
    )
    .await;
    let obj = |kind: &str, name: &str| ObjectRef {
        kind: kind.into(),
        schema: Some("public".into()),
        name: name.into(),
    };
    let objects = [
        (obj("table", "dbine_sec_facturas"), true),
        (obj("materialized_view", "dbine_sec_mv"), false),
        (obj("source", "dbine_sec_src"), false),
        (obj("sink", "dbine_sec_sink"), false),
        (obj("view", "dbine_sec_v"), false),
    ];
    for (o, grantable) in &objects {
        run(
            &mut s,
            &script(SecurityAction::Grant {
                privileges: vec!["SELECT".into()],
                object: Some(o.clone()),
                to: "dbine_ana".into(),
                grantable: *grantable,
            }),
        )
        .await;
    }
    let schema = ObjectRef {
        kind: "schema".into(),
        schema: None,
        name: "public".into(),
    };
    run(
        &mut s,
        &script(SecurityAction::Grant {
            privileges: vec!["USAGE".into()],
            object: Some(schema.clone()),
            to: "dbine_ana".into(),
            grantable: false,
        }),
    )
    .await;

    let all = s.principals().await.unwrap();
    let ana = all
        .iter()
        .find(|p| p.name == "dbine_ana")
        .expect("the user");
    assert_eq!(
        (
            ana.kind,
            ana.can_login,
            ana.disabled,
            ana.superuser,
            ana.system
        ),
        (
            PrincipalKind::User,
            Some(true),
            Some(false),
            Some(false),
            false
        )
    );
    assert!(all
        .iter()
        .any(|p| p.name == "root" && p.system && p.superuser == Some(true)));

    let g = s.grants("dbine_ana").await.unwrap();
    for (o, grantable) in &objects {
        let x = g
            .iter()
            .find(|x| x.object_kind.as_deref() == Some(o.kind.as_str()))
            .unwrap_or_else(|| panic!("{}: {g:?}", o.kind));
        assert_eq!(
            (
                x.privilege.as_str(),
                x.object.clone(),
                x.grantable,
                x.via.as_deref()
            ),
            (
                "SELECT",
                Some(format!("public.{}", o.name)),
                *grantable,
                None
            )
        );
    }
    let usage = g
        .iter()
        .find(|x| x.object_kind.as_deref() == Some("schema"))
        .expect("USAGE on the schema");
    assert_eq!(
        (usage.privilege.as_str(), usage.object.as_deref()),
        ("USAGE", Some("public"))
    );

    for (o, _) in &objects {
        run(
            &mut s,
            &script(SecurityAction::Revoke {
                privileges: vec!["SELECT".into()],
                object: Some(o.clone()),
                from: "dbine_ana".into(),
            }),
        )
        .await;
    }
    run(
        &mut s,
        &script(SecurityAction::Revoke {
            privileges: vec!["USAGE".into()],
            object: Some(schema),
            from: "dbine_ana".into(),
        }),
    )
    .await;
    // RisingWave gives every new user CONNECT (with grant option) on the
    // database it's created in.
    let g = s.grants("dbine_ana").await.unwrap();
    assert!(
        g.iter()
            .all(|x| x.privilege == "CONNECT" && x.object_kind.as_deref() == Some("database")),
        "{g:?}"
    );
    // Revoking that listed grant (the tab sends its named database), then a
    // grant on the database picked by name.
    let db = ObjectRef {
        kind: "database".into(),
        schema: None,
        name: g[0].object.clone().expect("the database"),
    };
    run(
        &mut s,
        &script(SecurityAction::Revoke {
            privileges: vec!["CONNECT".into()],
            object: Some(db.clone()),
            from: "dbine_ana".into(),
        }),
    )
    .await;
    assert!(s.grants("dbine_ana").await.unwrap().is_empty());
    run(
        &mut s,
        &script(SecurityAction::Grant {
            privileges: vec!["CREATE".into()],
            object: Some(db.clone()),
            to: "dbine_ana".into(),
            grantable: true,
        }),
    )
    .await;
    let g = s.grants("dbine_ana").await.unwrap();
    assert_eq!(g.len(), 1, "{g:?}");
    assert_eq!(
        (
            g[0].privilege.as_str(),
            g[0].object.as_deref(),
            g[0].grantable
        ),
        ("CREATE", Some(db.name.as_str()), true)
    );
    run(
        &mut s,
        &script(SecurityAction::Revoke {
            privileges: vec!["CREATE".into()],
            object: Some(db),
            from: "dbine_ana".into(),
        }),
    )
    .await;
    run(
        &mut s,
        &script(SecurityAction::SetPassword {
            name: "dbine_ana".into(),
            password: "Otra_9'87".into(),
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
    let ana = s
        .principals()
        .await
        .unwrap()
        .into_iter()
        .find(|p| p.name == "dbine_ana")
        .unwrap();
    assert_eq!((ana.can_login, ana.disabled), (Some(false), Some(true)));
    run(
        &mut s,
        &script(SecurityAction::SetLogin {
            name: "dbine_ana".into(),
            enabled: true,
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
    assert!(s
        .principals()
        .await
        .unwrap()
        .iter()
        .all(|p| p.name != "dbine_ana"));
    run(&mut s, "DROP SINK dbine_sec_sink").await;
    run(&mut s, "DROP SOURCE dbine_sec_src").await;
    run(&mut s, "DROP VIEW dbine_sec_v").await;
    run(&mut s, "DROP MATERIALIZED VIEW dbine_sec_mv").await;
    run(&mut s, "DROP TABLE dbine_sec_facturas").await;
}

// -- what can be granted ------------------------------------------------------

fn obj(kind: &str, schema: Option<&str>, name: &str) -> (String, Option<ObjectRef>) {
    (
        kind.to_string(),
        Some(ObjectRef {
            kind: kind.into(),
            schema: schema.map(Into::into),
            name: name.into(),
        }),
    )
}

/// The current database ("" in the tab).
fn here() -> (String, Option<ObjectRef>) {
    (String::new(), None)
}

/// `Session::privileges` on a real server: a list within the spec's kinds
/// that has the `expected` (privilege, kind) pairs; then the server takes
/// each listed privilege on each of its kinds that `targets` has an object
/// of (granted to a scratch principal made by `grantee`, revoked at once).
/// `setup` runs after `cleanup`, which runs again at the end.
async fn grantable(
    id: &str,
    env: &str,
    grantee: SecurityAction,
    cleanup_sql: &[&str],
    setup: &[&str],
    targets: &[(String, Option<ObjectRef>)],
    expected: &[(&str, &str)],
) -> Vec<dbine_driver::Privilege> {
    let Some(cfg) = cfg(id, env) else {
        eprintln!("{env} not set; skipping");
        return Vec::new();
    };
    let d: Arc<dyn Driver> = dbine_driver_postgres::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap();
    let spec = d.security().unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    let list = s.privileges().await.unwrap();
    eprintln!("{id}: {} privileges: {list:?}", list.len());
    assert!(!list.is_empty(), "{id}");
    for p in &list {
        assert!(!p.on.is_empty(), "{id}: {} on nothing", p.name);
        for k in &p.on {
            assert!(
                spec.object_kinds.contains(&k.as_str()),
                "{id}: {} on {k:?}",
                p.name
            );
        }
    }
    for (name, kind) in expected {
        assert!(
            list.iter()
                .any(|p| p.name == *name && p.on.iter().any(|k| k == kind)),
            "{id}: {name} on {kind:?} missing from {list:?}"
        );
    }

    let to = match &grantee {
        SecurityAction::CreateUser { name, .. } | SecurityAction::CreateRole { name } => {
            name.clone()
        }
        _ => unreachable!(),
    };
    let kind = match grantee {
        SecurityAction::CreateRole { .. } => PrincipalKind::Role,
        _ => PrincipalKind::User,
    };
    cleanup(&mut s, cleanup_sql).await;
    for stmt in setup {
        run(&mut s, stmt).await;
    }
    run(&mut s, &d.security_script(&grantee).unwrap()).await;
    let mut tried: Vec<&str> = Vec::new();
    for p in &list {
        for k in &p.on {
            let Some((_, object)) = targets.iter().find(|(t, _)| t == k) else {
                continue;
            };
            let grant = SecurityAction::Grant {
                privileges: vec![p.name.clone()],
                object: object.clone(),
                to: to.clone(),
                grantable: false,
            };
            run(&mut s, &d.security_script(&grant).unwrap()).await;
            let revoke = SecurityAction::Revoke {
                privileges: vec![p.name.clone()],
                object: object.clone(),
                from: to.clone(),
            };
            run(&mut s, &d.security_script(&revoke).unwrap()).await;
            tried.push(k);
        }
    }
    for (k, _) in targets {
        assert!(
            tried.contains(&k.as_str()),
            "{id}: nothing granted on {k:?}"
        );
    }
    run(
        &mut s,
        &d.security_script(&SecurityAction::Drop { name: to, kind })
            .unwrap(),
    )
    .await;
    cleanup(&mut s, cleanup_sql).await;
    list
}

const PG_CLEANUP: &[&str] = &[
    "DROP SCHEMA IF EXISTS dbine_priv CASCADE",
    "DROP ROLE IF EXISTS dbine_priv_r",
];

/// Every object kind PostgreSQL's spec offers, in schema `dbine_priv`.
fn pg_targets() -> Vec<(String, Option<ObjectRef>)> {
    vec![
        here(),
        obj("schema", None, "dbine_priv"),
        obj("table", Some("dbine_priv"), "t"),
        obj("view", Some("dbine_priv"), "v"),
        obj("materialized_view", Some("dbine_priv"), "mv"),
        obj("function", Some("dbine_priv"), "f()"),
        obj("procedure", Some("dbine_priv"), "p()"),
    ]
}

const PG_SETUP: &[&str] = &[
    "CREATE SCHEMA dbine_priv",
    "CREATE TABLE dbine_priv.t (id int)",
    "CREATE VIEW dbine_priv.v AS SELECT id FROM dbine_priv.t",
    "CREATE MATERIALIZED VIEW dbine_priv.mv AS SELECT id FROM dbine_priv.t",
    "CREATE FUNCTION dbine_priv.f() RETURNS int LANGUAGE sql AS 'SELECT 1'",
    "CREATE PROCEDURE dbine_priv.p() LANGUAGE sql AS 'SELECT 1'",
];

/// PostgreSQL and its distributions: `acldefault`'s list.
async fn pg_grantable(id: &str, env: &str, setup: &[&str], expected: &[(&str, &str)]) {
    let list = grantable(
        id,
        env,
        SecurityAction::CreateRole {
            name: "dbine_priv_r".into(),
        },
        PG_CLEANUP,
        setup,
        &pg_targets(),
        expected,
    )
    .await;
    if list.is_empty() {
        return;
    }
    // MAINTAIN only where the server has it (PostgreSQL 17 on).
    let cfg = cfg(id, env).unwrap();
    let d = dbine_driver_postgres::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap();
    let mut s = d.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.execute(
        "SELECT current_setting('server_version_num')::int >= 170000 AS m",
        1,
        &mut out,
    )
    .await
    .unwrap();
    let has = out.results[0].rows[0][0].as_bool() == Some(true)
        || out.results[0].rows[0][0].as_str() == Some("t");
    assert_eq!(
        list.iter().any(|p| p.name == "MAINTAIN"),
        has,
        "{id}: {list:?}"
    );
}

const PG_EXPECTED: &[(&str, &str)] = &[
    ("CONNECT", ""),
    ("TEMPORARY", ""),
    ("USAGE", "schema"),
    ("SELECT", "view"),
    ("TRUNCATE", "materialized_view"),
    ("TRIGGER", "table"),
    ("EXECUTE", "procedure"),
    ("ALL PRIVILEGES", "function"),
];

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn postgres_privileges() {
    pg_grantable("postgres", "DBINE_TEST_POSTGRES_URL", PG_SETUP, PG_EXPECTED).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn timescaledb_privileges() {
    pg_grantable(
        "timescaledb",
        "DBINE_TEST_TIMESCALEDB_URL",
        PG_SETUP,
        PG_EXPECTED,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn yugabytedb_privileges() {
    pg_grantable(
        "yugabytedb",
        "DBINE_TEST_YUGABYTEDB_URL",
        PG_SETUP,
        PG_EXPECTED,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn greenplum_family_privileges() {
    pg_grantable(
        "cloudberry",
        "DBINE_TEST_CLOUDBERRY_URL",
        PG_SETUP,
        PG_EXPECTED,
    )
    .await;
    pg_grantable(
        "greengage",
        "DBINE_TEST_GREENGAGE_URL",
        PG_SETUP,
        PG_EXPECTED,
    )
    .await;
}

/// openGauss: its DDL privileges (ALTER, DROP, COMMENT, INDEX, VACUUM) on
/// top of `acldefault`'s.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn opengauss_privileges() {
    let mut setup = PG_SETUP[..5].to_vec();
    setup.push("CREATE PROCEDURE dbine_priv.p() AS BEGIN NULL; END");
    let mut expected = PG_EXPECTED.to_vec();
    expected.extend([
        ("VACUUM", "table"),
        ("INDEX", "materialized_view"),
        ("COMMENT", ""),
        ("ALTER", "schema"),
        ("DROP", "procedure"),
    ]);
    pg_grantable("opengauss", "DBINE_TEST_OPENGAUSS_URL", &setup, &expected).await;
}

/// CockroachDB: `acldefault`'s plus its own; no database-wide grants.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cockroachdb_privileges() {
    let mut targets = pg_targets();
    targets.remove(0);
    let list = grantable(
        "cockroachdb",
        "DBINE_TEST_COCKROACH_URL",
        SecurityAction::CreateRole {
            name: "dbine_priv_r".into(),
        },
        PG_CLEANUP,
        PG_SETUP,
        &targets,
        &[
            ("SELECT", "materialized_view"),
            ("CREATE", "table"),
            ("USAGE", "schema"),
            ("EXECUTE", "function"),
            ("BACKUP", "table"),
            ("CHANGEFEED", "view"),
            ("DROP", "table"),
            ("ZONECONFIG", "table"),
            ("ALL", "procedure"),
        ],
    )
    .await;
    assert!(list.iter().all(|p| p.name != "CONNECT"), "{list:?}");
}

/// Materialize: the documented list; "" is SYSTEM.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn materialize_privileges() {
    grantable(
        "materialize",
        "DBINE_TEST_MATERIALIZE_URL",
        SecurityAction::CreateRole {
            name: "dbine_priv_r".into(),
        },
        PG_CLEANUP,
        &PG_SETUP[..4],
        &pg_targets()[..5],
        &[
            ("CREATECLUSTER", ""),
            ("CREATENETWORKPOLICY", ""),
            ("CREATE", "schema"),
            ("INSERT", "table"),
            ("SELECT", "source"),
            ("ALL PRIVILEGES", ""),
        ],
    )
    .await;
}

/// RisingWave: the documented list; databases by name.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn risingwave_privileges() {
    let db = cfg("risingwave", "DBINE_TEST_RISINGWAVE_URL").map(|c| c.database);
    let Some(db) = db else {
        return;
    };
    grantable(
        "risingwave",
        "DBINE_TEST_RISINGWAVE_URL",
        SecurityAction::CreateUser {
            name: "dbine_priv_r".into(),
            password: None,
        },
        &[
            "DROP SCHEMA IF EXISTS dbine_priv CASCADE",
            "DROP USER IF EXISTS dbine_priv_r",
        ],
        &[
            "CREATE SCHEMA dbine_priv",
            "CREATE TABLE dbine_priv.t (id int)",
            "CREATE VIEW dbine_priv.v AS SELECT id FROM dbine_priv.t",
            "CREATE MATERIALIZED VIEW dbine_priv.mv AS SELECT id FROM dbine_priv.t",
            "CREATE SOURCE dbine_priv.src (id int) WITH (connector = 'datagen', datagen.rows.per.second = '1') FORMAT PLAIN ENCODE JSON",
            "CREATE SINK dbine_priv.k FROM dbine_priv.t WITH (connector = 'blackhole')",
        ],
        &[
            obj("database", None, &db),
            obj("schema", None, "dbine_priv"),
            obj("table", Some("dbine_priv"), "t"),
            obj("view", Some("dbine_priv"), "v"),
            obj("materialized_view", Some("dbine_priv"), "mv"),
            obj("source", Some("dbine_priv"), "src"),
            obj("sink", Some("dbine_priv"), "k"),
        ],
        &[
            ("CONNECT", "database"),
            ("USAGE", "schema"),
            ("DELETE", "view"),
            ("SELECT", "sink"),
            ("ALL PRIVILEGES", "source"),
        ],
    )
    .await;
}

/// CrateDB (`DBINE_TEST_CRATEDB_URL=postgres://crate@localhost:25021/doc`):
/// "" is the cluster, where AL goes.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cratedb_privileges() {
    grantable(
        "cratedb",
        "DBINE_TEST_CRATEDB_URL",
        SecurityAction::CreateRole {
            name: "dbine_priv_r".into(),
        },
        &[
            "DROP VIEW IF EXISTS dbine_priv.v",
            "DROP TABLE IF EXISTS dbine_priv.t",
            "DROP ROLE IF EXISTS dbine_priv_r",
        ],
        &[
            "CREATE TABLE dbine_priv.t (id int)",
            "CREATE VIEW dbine_priv.v AS SELECT id FROM dbine_priv.t",
        ],
        &[
            here(),
            obj("schema", None, "dbine_priv"),
            obj("table", Some("dbine_priv"), "t"),
            obj("view", Some("dbine_priv"), "v"),
        ],
        &[
            ("AL", ""),
            ("DQL", "view"),
            ("DDL", "schema"),
            ("ALL PRIVILEGES", "table"),
        ],
    )
    .await;
}

/// H2: the documented rights; ADMIN and ALTER ANY SCHEMA on the database.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn h2_privileges() {
    grantable(
        "h2",
        "DBINE_TEST_H2_URL",
        SecurityAction::CreateUser {
            name: "dbine_priv_r".into(),
            password: Some("Pw_12345".into()),
        },
        &[
            "DROP SCHEMA IF EXISTS dbine_priv CASCADE",
            "DROP USER IF EXISTS dbine_priv_r",
        ],
        &[
            "CREATE SCHEMA dbine_priv",
            "CREATE TABLE dbine_priv.t (id int)",
            "CREATE VIEW dbine_priv.v AS SELECT id FROM dbine_priv.t",
        ],
        &[
            here(),
            obj("schema", None, "dbine_priv"),
            obj("table", Some("dbine_priv"), "t"),
            obj("view", Some("dbine_priv"), "v"),
        ],
        &[
            ("ADMIN", ""),
            ("ALTER ANY SCHEMA", ""),
            ("SELECT", "view"),
            ("ALL PRIVILEGES", "schema"),
        ],
    )
    .await;
}
