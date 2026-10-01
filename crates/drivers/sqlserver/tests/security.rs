//! Users, roles and permissions against a real server
//! (`DBINE_TEST_SQLSERVER_URL`, see tests/integration.rs):
//! `cargo test -p dbine-driver-sqlserver --test security -- --ignored`

use dbine_driver::{ConnectionConfig, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session};

fn cfg() -> Option<ConnectionConfig> {
    cfg_from(&std::env::var("DBINE_TEST_SQLSERVER_URL").ok()?)
}

fn cfg_from(url: &str) -> Option<ConnectionConfig> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn users_roles_and_grants() {
    let Some(cfg) = cfg() else { return };
    let d = dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == "sqlserver").unwrap();
    let mut admin = d.connect(&cfg, Some("master")).await.unwrap();
    run(&mut admin, "IF DB_ID('dbine_sec') IS NOT NULL BEGIN ALTER DATABASE dbine_sec SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE dbine_sec; END; IF SUSER_ID('dbine_ana') IS NOT NULL DROP LOGIN dbine_ana; CREATE DATABASE dbine_sec;").await;
    let mut s = d.connect(&cfg, Some("dbine_sec")).await.unwrap();
    run(&mut s, "CREATE TABLE dbo.facturas (id int PRIMARY KEY, total money);").await;

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("Pw_12345!x".into()) })).await;
    run(&mut s, &script(SecurityAction::CreateRole { name: "lectores".into() })).await;
    run(&mut s, &script(SecurityAction::AddMember { role: "lectores".into(), member: "dbine_ana".into() })).await;
    let table = ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "facturas".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["SELECT".into()], object: Some(table.clone()), to: "lectores".into(), grantable: false })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["UPDATE".into()], object: Some(table.clone()), to: "dbine_ana".into(), grantable: true })).await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "dbine_ana").expect("the user");
    assert_eq!(ana.kind, PrincipalKind::User);
    assert_eq!(ana.member_of, vec!["lectores".to_string()]);
    assert!(!ana.system);
    assert!(all.iter().any(|p| p.name == "lectores" && p.kind == PrincipalKind::Role));
    assert!(all.iter().any(|p| p.name == "dbo" && p.system));

    let g = s.grants("dbine_ana").await.unwrap();
    let upd = g.iter().find(|x| x.privilege == "UPDATE").expect("direct UPDATE");
    assert_eq!((upd.object.as_deref(), upd.object_kind.as_deref(), upd.grantable, upd.via.as_deref()), (Some("dbo.facturas"), Some("table"), true, None));
    let sel = g.iter().find(|x| x.privilege == "SELECT").expect("SELECT through the role");
    assert_eq!(sel.via.as_deref(), Some("lectores"));

    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["UPDATE".into()], object: Some(table), from: "dbine_ana".into() })).await;
    assert!(s.grants("dbine_ana").await.unwrap().iter().all(|x| x.privilege != "UPDATE"));
    run(&mut s, &script(SecurityAction::SetPassword { name: "dbine_ana".into(), password: "Otra_Pw_987!".into() })).await;
    run(&mut s, &script(SecurityAction::SetLogin { name: "dbine_ana".into(), enabled: false })).await;
    assert_eq!(s.principals().await.unwrap().iter().find(|p| p.name == "dbine_ana").unwrap().disabled, Some(true));
    run(&mut s, &script(SecurityAction::RemoveMember { role: "lectores".into(), member: "dbine_ana".into() })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_ana".into(), kind: PrincipalKind::User })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "lectores".into(), kind: PrincipalKind::Role })).await;
    assert!(s.principals().await.unwrap().iter().all(|p| p.name != "dbine_ana" && p.name != "lectores"));
    drop(s);
    run(&mut admin, "ALTER DATABASE dbine_sec SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE dbine_sec; DROP LOGIN dbine_ana;").await;
}

/// Babelfish through its TDS port (`DBINE_TEST_BABELFISH_URL`, see
/// tests/integration.rs): its GRANTs land in PostgreSQL's ACLs.
/// `cargo test -p dbine-driver-sqlserver --test security babelfish -- --ignored`
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn babelfish_users_roles_and_grants() {
    let Some(mut cfg) = std::env::var("DBINE_TEST_BABELFISH_URL").ok().and_then(|u| cfg_from(&u)) else {
        return;
    };
    cfg.driver = "babelfish".into();
    let d = dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == "babelfish").unwrap();
    let spec = d.security().expect("Babelfish manages users");
    assert!(!spec.object_kinds.contains(&""));
    let mut admin = d.connect(&cfg, Some("master")).await.unwrap();
    run(&mut admin, "IF DB_ID('dbine_sec') IS NOT NULL DROP DATABASE dbine_sec;").await;
    run(&mut admin, "IF SUSER_ID('dbine_ana') IS NOT NULL DROP LOGIN dbine_ana;").await;
    run(&mut admin, "CREATE DATABASE dbine_sec;").await;
    let mut s = d.connect(&cfg, Some("dbine_sec")).await.unwrap();
    run(
        &mut s,
        "CREATE TABLE dbo.facturas (id int PRIMARY KEY, total money);
         GO
         CREATE PROCEDURE dbo.cerrar AS SELECT 1
         GO
         CREATE SCHEMA ventas
         GO
         CREATE TABLE ventas.pedidos (id int)",
    )
    .await;

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("Pw_12345!x".into()) })).await;
    run(&mut s, &script(SecurityAction::CreateRole { name: "lectores".into() })).await;
    run(&mut s, &script(SecurityAction::AddMember { role: "lectores".into(), member: "dbine_ana".into() })).await;
    let table = ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "facturas".into() };
    let proc = ObjectRef { kind: "procedure".into(), schema: Some("dbo".into()), name: "cerrar".into() };
    let ventas = ObjectRef { kind: "schema".into(), schema: None, name: "ventas".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["SELECT".into()], object: Some(table.clone()), to: "lectores".into(), grantable: false })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["EXECUTE".into()], object: Some(proc), to: "lectores".into(), grantable: false })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["UPDATE".into()], object: Some(table.clone()), to: "dbine_ana".into(), grantable: true })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["INSERT".into()], object: Some(ventas.clone()), to: "dbine_ana".into(), grantable: false })).await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "dbine_ana").expect("the user");
    assert_eq!((ana.kind, ana.disabled, ana.system), (PrincipalKind::User, Some(false), false));
    assert_eq!(ana.member_of, vec!["lectores".to_string()]);
    assert!(ana.details.iter().any(|(k, v)| k == "Login" && v == "dbine_ana"));
    assert!(all.iter().any(|p| p.name == "lectores" && p.kind == PrincipalKind::Role && !p.system));
    assert!(all.iter().any(|p| p.name == "dbo" && p.superuser == Some(true)));
    assert!(all.iter().any(|p| p.name == "db_datareader" && p.system));

    let g = s.grants("dbine_ana").await.unwrap();
    let find = |p: &str, o: &str| g.iter().find(|x| x.privilege == p && x.object.as_deref() == Some(o)).unwrap_or_else(|| panic!("{p} on {o} in {g:?}"));
    let upd = find("UPDATE", "dbo.facturas");
    assert_eq!((upd.object_kind.as_deref(), upd.grantable, upd.via.as_deref()), (Some("table"), true, None));
    assert_eq!(find("SELECT", "dbo.facturas").via.as_deref(), Some("lectores"));
    let exec = find("EXECUTE", "dbo.cerrar");
    assert_eq!((exec.object_kind.as_deref(), exec.via.as_deref()), (Some("procedure"), Some("lectores")));
    // The schema's GRANT reaches its tables.
    assert_eq!(find("INSERT", "ventas.pedidos").via, None);

    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["UPDATE".into()], object: Some(table), from: "dbine_ana".into() })).await;
    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["INSERT".into()], object: Some(ventas), from: "dbine_ana".into() })).await;
    let g = s.grants("dbine_ana").await.unwrap();
    assert!(g.iter().all(|x| x.via.is_some()), "{g:?}");
    run(&mut s, &script(SecurityAction::SetPassword { name: "dbine_ana".into(), password: "Otra_Pw_987!".into() })).await;
    run(&mut s, &script(SecurityAction::SetLogin { name: "dbine_ana".into(), enabled: false })).await;
    assert_eq!(s.principals().await.unwrap().iter().find(|p| p.name == "dbine_ana").unwrap().disabled, Some(true));
    run(&mut s, &script(SecurityAction::SetLogin { name: "dbine_ana".into(), enabled: true })).await;
    run(&mut s, &script(SecurityAction::RemoveMember { role: "lectores".into(), member: "dbine_ana".into() })).await;
    assert!(s.grants("dbine_ana").await.unwrap().is_empty());
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_ana".into(), kind: PrincipalKind::User })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "lectores".into(), kind: PrincipalKind::Role })).await;
    assert!(s.principals().await.unwrap().iter().all(|p| p.name != "dbine_ana" && p.name != "lectores"));
    drop(s);
    run(&mut admin, "DROP DATABASE dbine_sec;").await;
    run(&mut admin, "DROP LOGIN dbine_ana;").await;
}

async fn fails(s: &mut Box<dyn Session>, sql: &str) -> String {
    let mut out = QueryOutcome::default();
    match s.execute(sql, 100, &mut out).await {
        Err(e) => e.to_string(),
        Ok(()) => out.error.unwrap_or_else(|| panic!("{sql} should fail")),
    }
}

/// A schema grant: privileges, grantee, with grant option.
type SchemaGrant<'a> = (&'a [&'a str], &'a str, bool);

/// The script the Tauri command writes (`build_create`): create, the
/// grants, then the owner change when `schema_owner_script` gives one,
/// each closed by the driver's `GO`.
fn create_script(d: &dyn dbine_driver::Driver, name: &str, owner: Option<&str>, grants: &[SchemaGrant]) -> String {
    let change = owner.and_then(|o| d.schema_owner_script(None, name, o).unwrap());
    let mut parts = vec![d.create_schema_script(None, name, if change.is_some() { None } else { owner }).unwrap()];
    for (privileges, to, grantable) in grants {
        let privileges: Vec<String> = privileges.iter().map(|p| p.to_string()).collect();
        parts.push(d.schema_grant_script(None, name, &privileges, to, *grantable).unwrap());
    }
    parts.extend(change);
    parts.iter().map(|p| format!("{p}\nGO\n")).collect()
}

/// `list_schemas` holds `name`, marked `system` or not.
async fn listed(s: &mut Box<dyn Session>, name: &str, system: bool) {
    let all = s.list_schemas().await.unwrap().expect("SQL Server lists its schemas");
    assert!(all.iter().any(|x| x.name == name && x.system == system), "{name} (system: {system}) in {all:?}");
}

/// "Nuevo esquema…" with an owner and grants, checked in the catalog, then
/// "Borrar esquema…" (no CASCADE in T-SQL: it fails while the schema holds
/// objects). The creator isn't db_owner: CREATE SCHEMA, IMPERSONATE on the
/// owner user, ALTER on the owner role and db_securityadmin (to grant on a
/// schema it doesn't own). The owner goes in `AUTHORIZATION`: an `ALTER
/// AUTHORIZATION` after the grants would drop them.
/// `cargo test -p dbine-driver-sqlserver --test security schemas -- --ignored`
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn schemas_with_owner_and_grants() {
    let Some(cfg) = cfg() else { return };
    let d = dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == "sqlserver").unwrap();
    let spec = d.schema_spec().expect("SQL Server creates schemas");
    assert!(spec.owner && !spec.cascade && spec.privileges.contains(&"TAKE OWNERSHIP"));
    assert_eq!(spec.owner_kinds, dbine_driver::SchemaOwnerKinds::Both);
    assert_eq!(d.schema_owner_script(None, "ventas", "ana").unwrap(), None);
    let mut admin = d.connect(&cfg, Some("master")).await.unwrap();
    run(&mut admin, "IF DB_ID('dbine_sch') IS NOT NULL BEGIN ALTER DATABASE dbine_sch SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE dbine_sch; END; IF SUSER_ID('dbine_creador') IS NOT NULL DROP LOGIN dbine_creador; CREATE DATABASE dbine_sch; CREATE LOGIN dbine_creador WITH PASSWORD = 'Pw_12345!x';").await;
    assert_eq!(admin.permissions(Some("dbine_sch")).await.unwrap().create_schema, dbine_driver::Access::Allowed);
    let mut sa = d.connect(&cfg, Some("dbine_sch")).await.unwrap();
    run(
        &mut sa,
        "CREATE USER dbine_dueno WITHOUT LOGIN; CREATE ROLE lectores; CREATE USER dbine_creador FOR LOGIN dbine_creador;
         GRANT CREATE SCHEMA TO dbine_creador; GRANT IMPERSONATE ON USER::dbine_dueno TO dbine_creador;
         GRANT ALTER ON ROLE::lectores TO dbine_creador; ALTER ROLE db_securityadmin ADD MEMBER dbine_creador;",
    )
    .await;
    // Every privilege the spec offers is one the server takes on a schema.
    for p in &spec.privileges {
        run(&mut sa, &format!("IF NOT EXISTS (SELECT 1 FROM sys.fn_builtin_permissions('SCHEMA') WHERE permission_name = '{p}') THROW 50000, '{p}', 1;")).await;
    }
    let mut creador = cfg.clone();
    creador.username = Some("dbine_creador".into());
    creador.password = Some("Pw_12345!x".into());
    let mut s = d.connect(&creador, Some("dbine_sch")).await.unwrap();
    run(&mut s, "IF IS_MEMBER('db_owner') = 1 OR IS_SRVROLEMEMBER('sysadmin') = 1 THROW 50000, 'superuser', 1;").await;
    let owners = s.principals().await.unwrap();
    assert!(owners.iter().any(|p| p.name == "dbine_dueno") && owners.iter().any(|p| p.name == "lectores"));

    let grants: [SchemaGrant; 2] = [(&["SELECT", "INSERT"], "lectores", true), (&["EXECUTE"], "public", false)];
    run(&mut s, &create_script(d.as_ref(), "ven tas", Some("dbine_dueno"), &grants)).await;
    run(
        &mut sa,
        "IF NOT EXISTS (SELECT 1 FROM sys.schemas s JOIN sys.database_principals p ON p.principal_id = s.principal_id
                         WHERE s.name = N'ven tas' AND p.name = N'dbine_dueno') THROW 50000, 'owner', 1;
         IF (SELECT COUNT(*) FROM sys.database_permissions x JOIN sys.database_principals p ON p.principal_id = x.grantee_principal_id
              WHERE x.class = 3 AND x.major_id = SCHEMA_ID(N'ven tas') AND p.name = N'lectores' AND x.state = 'W'
                AND x.permission_name IN ('SELECT', 'INSERT')) <> 2 THROW 50000, 'grants', 1;
         IF NOT EXISTS (SELECT 1 FROM sys.database_permissions x WHERE x.class = 3 AND x.major_id = SCHEMA_ID(N'ven tas')
                         AND x.grantee_principal_id = DATABASE_PRINCIPAL_ID('public') AND x.permission_name = 'EXECUTE' AND x.state = 'G')
           THROW 50000, 'public', 1;",
    )
    .await;
    let g = sa.grants("lectores").await.unwrap();
    assert!(g.iter().any(|x| x.privilege == "INSERT" && x.object.as_deref() == Some("ven tas") && x.object_kind.as_deref() == Some("schema") && x.grantable), "{g:?}");

    // A role owns a schema too.
    run(&mut s, &create_script(d.as_ref(), "de rol", Some("lectores"), &[(&["SELECT"], "public", false)])).await;
    run(&mut sa, "IF (SELECT USER_NAME(principal_id) FROM sys.schemas WHERE name = N'de rol') <> N'lectores' THROW 50000, 'role owner', 1;").await;

    // The new schemas are empty and listed; the built-in ones are system.
    for (name, system) in [("ven tas", false), ("de rol", false), ("dbo", false), ("sys", true), ("INFORMATION_SCHEMA", true), ("guest", true), ("db_owner", true), ("db_datareader", true)] {
        listed(&mut s, name, system).await;
    }

    // A schema without owner is its creator's.
    run(&mut sa, &create_script(d.as_ref(), "vacio", None, &[])).await;
    run(&mut sa, "IF SCHEMA_ID(N'vacio') IS NULL OR (SELECT principal_id FROM sys.schemas WHERE name = N'vacio') <> 1 THROW 50000, 'vacio', 1;").await;
    run(&mut sa, &d.drop_schema_script(None, "vacio", false).unwrap()).await;
    run(&mut sa, &d.drop_schema_script(None, "de rol", false).unwrap()).await;

    run(&mut sa, "CREATE TABLE [ven tas].pedidos (id int);").await;
    let e = fails(&mut sa, &d.drop_schema_script(None, "ven tas", false).unwrap()).await;
    assert!(e.contains("pedidos") || e.contains("ven tas"), "{e}");
    assert!(d.drop_schema_script(None, "ven tas", true).is_err());
    run(&mut sa, "DROP TABLE [ven tas].pedidos;").await;
    run(&mut sa, &d.drop_schema_script(None, "ven tas", false).unwrap()).await;
    run(&mut sa, "IF SCHEMA_ID(N'ven tas') IS NOT NULL THROW 50000, 'dropped', 1;").await;
    let all = sa.list_schemas().await.unwrap().unwrap();
    assert!(all.iter().all(|x| x.name != "ven tas" && x.name != "de rol"), "{all:?}");
    drop(s);
    drop(sa);
    run(&mut admin, "ALTER DATABASE dbine_sch SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE dbine_sch; DROP LOGIN dbine_creador;").await;
}

/// The same on Babelfish (`DBINE_TEST_BABELFISH_URL`): the owner is a
/// database user or role; schema grants have no grant option there. The
/// creator is a member of db_ddladmin and db_securityadmin (Babelfish
/// rejects `GRANT CREATE SCHEMA`, a database-wide GRANT). Babelfish has no
/// `ALTER AUTHORIZATION` on a schema, so the owner goes in `AUTHORIZATION`.
/// `cargo test -p dbine-driver-sqlserver --test security babelfish_schemas -- --ignored`
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn babelfish_schemas_with_owner_and_grants() {
    let Some(mut cfg) = std::env::var("DBINE_TEST_BABELFISH_URL").ok().and_then(|u| cfg_from(&u)) else {
        return;
    };
    cfg.driver = "babelfish".into();
    let d = dbine_driver_sqlserver::drivers().into_iter().find(|d| d.info().id == "babelfish").unwrap();
    let spec = d.schema_spec().expect("Babelfish creates schemas");
    assert!(spec.owner && !spec.cascade);
    assert_eq!(spec.owner_kinds, dbine_driver::SchemaOwnerKinds::Both);
    assert_eq!(d.schema_owner_script(None, "ventas", "ana").unwrap(), None);
    let mut admin = d.connect(&cfg, Some("master")).await.unwrap();
    run(&mut admin, "IF DB_ID('dbine_sch') IS NOT NULL DROP DATABASE dbine_sch;").await;
    for login in ["dbine_dueno", "dbine_creador"] {
        run(&mut admin, &format!("IF SUSER_ID('{login}') IS NOT NULL DROP LOGIN {login};")).await;
        run(&mut admin, &format!("CREATE LOGIN {login} WITH PASSWORD = 'Pw_12345!x';")).await;
    }
    run(&mut admin, "CREATE DATABASE dbine_sch;").await;
    let mut sa = d.connect(&cfg, Some("dbine_sch")).await.unwrap();
    run(&mut sa, "CREATE USER dbine_dueno FOR LOGIN dbine_dueno;").await;
    run(&mut sa, "CREATE USER dbine_creador FOR LOGIN dbine_creador;").await;
    run(&mut sa, "CREATE ROLE lectores;").await;
    run(&mut sa, "ALTER ROLE db_ddladmin ADD MEMBER dbine_creador;").await;
    run(&mut sa, "ALTER ROLE db_securityadmin ADD MEMBER dbine_creador;").await;
    let mut creador = cfg.clone();
    creador.username = Some("dbine_creador".into());
    creador.password = Some("Pw_12345!x".into());
    let mut s = d.connect(&creador, Some("dbine_sch")).await.unwrap();
    run(&mut s, "IF IS_MEMBER('db_owner') = 1 OR IS_SRVROLEMEMBER('sysadmin') = 1 THROW 50000, 'superuser', 1;").await;

    let ventas = Some(ObjectRef { kind: "schema".into(), schema: None, name: "ventas".into() });
    let grants: [SchemaGrant; 1] = [(&["SELECT", "INSERT"], "lectores", false)];
    assert!(d.security_script(&SecurityAction::Grant { privileges: vec!["SELECT".into()], object: ventas, to: "lectores".into(), grantable: true }).is_err());
    run(&mut s, &create_script(d.as_ref(), "ventas", Some("dbine_dueno"), &grants)).await;
    run(&mut s, &create_script(d.as_ref(), "de_rol", Some("lectores"), &[])).await;
    run(
        &mut sa,
        "IF NOT EXISTS (SELECT 1 FROM sys.schemas s JOIN sys.database_principals p ON p.principal_id = s.principal_id
                         WHERE s.name = 'ventas' AND p.name = 'dbine_dueno') THROW 50000, 'owner', 1;
         IF NOT EXISTS (SELECT 1 FROM sys.schemas s JOIN sys.database_principals p ON p.principal_id = s.principal_id
                         WHERE s.name = 'de_rol' AND p.name = 'lectores') THROW 50000, 'role owner', 1;",
    )
    .await;
    // Both new schemas are empty and listed; guest is a system one.
    for (name, system) in [("ventas", false), ("de_rol", false), ("dbo", false), ("guest", true)] {
        listed(&mut s, name, system).await;
    }
    // The schema's GRANT reaches the tables created in it.
    run(&mut sa, "CREATE TABLE ventas.pedidos (id int);").await;
    let g = sa.grants("lectores").await.unwrap();
    assert!(g.iter().any(|x| x.privilege == "INSERT" && x.object.as_deref() == Some("ventas.pedidos")), "{g:?}");

    let e = fails(&mut sa, &d.drop_schema_script(None, "ventas", false).unwrap()).await;
    assert!(!e.is_empty());
    run(&mut sa, "DROP TABLE ventas.pedidos;").await;
    run(&mut sa, &d.drop_schema_script(None, "ventas", false).unwrap()).await;
    run(&mut sa, &d.drop_schema_script(None, "de_rol", false).unwrap()).await;
    run(&mut sa, "IF SCHEMA_ID('ventas') IS NOT NULL THROW 50000, 'dropped', 1;").await;
    let all = sa.list_schemas().await.unwrap().unwrap();
    assert!(all.iter().all(|x| x.name != "ventas" && x.name != "de_rol"), "{all:?}");
    drop(s);
    drop(sa);
    // The two sessions' backends take a moment to leave the database.
    for _ in 0..50 {
        let mut out = QueryOutcome::default();
        if admin.execute("DROP DATABASE dbine_sch;", 100, &mut out).await.is_ok() && out.error.is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    run(&mut admin, "IF DB_ID('dbine_sch') IS NOT NULL THROW 50000, 'database left', 1;").await;
    run(&mut admin, "DROP LOGIN dbine_dueno;").await;
    run(&mut admin, "DROP LOGIN dbine_creador;").await;
}
