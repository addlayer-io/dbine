//! Users, roles and permissions against real servers
//! (`DBINE_TEST_MYSQL_URL`, `DBINE_TEST_MARIADB_URL`, `DBINE_TEST_TIDB_URL`,
//! `DBINE_TEST_OCEANBASE_URL`, `DBINE_TEST_STARROCKS_URL`, `DBINE_TEST_DORIS_URL`
//! (also run as VeloDB), `DBINE_TEST_DATABEND_URL`, see tests/integration.rs):
//! `cargo test -p dbine-driver-mysql --test security -- --ignored`

use dbine_driver::{ConnectionConfig, ObjectRef, PrincipalKind, QueryOutcome, SecurityAction, Session};

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.trim_end_matches('/').rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

async fn exercise(id: &str, env: &str) {
    let Some(cfg) = cfg(id, env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = dbine_driver_mysql::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    assert!(d.security().is_some());
    let mut admin = d.connect(&cfg, None).await.unwrap();
    run(&mut admin, "DROP USER IF EXISTS 'dbine_ana'@'%'; DROP ROLE IF EXISTS 'dbine_lect'; DROP DATABASE IF EXISTS dbine_sec; CREATE DATABASE dbine_sec").await;
    let mut s = d.connect(&cfg, Some("dbine_sec")).await.unwrap();
    run(&mut s, "CREATE TABLE facturas (id int PRIMARY KEY, total decimal(10,2))").await;

    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    run(&mut s, &script(SecurityAction::CreateUser { name: "dbine_ana".into(), password: Some("Pw_12345!x".into()) })).await;
    run(&mut s, &script(SecurityAction::CreateRole { name: "dbine_lect".into() })).await;
    run(&mut s, &script(SecurityAction::AddMember { role: "dbine_lect".into(), member: "dbine_ana@%".into() })).await;
    let table = ObjectRef { kind: "table".into(), schema: Some("dbine_sec".into()), name: "facturas".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["SELECT".into()], object: Some(table.clone()), to: "dbine_lect".into(), grantable: false })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["UPDATE".into()], object: Some(table.clone()), to: "dbine_ana@%".into(), grantable: true })).await;
    let db = ObjectRef { kind: "schema".into(), schema: None, name: "dbine_sec".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec!["INSERT".into()], object: Some(db.clone()), to: "dbine_ana@%".into(), grantable: false })).await;

    let all = s.principals().await.unwrap();
    let ana = all.iter().find(|p| p.name == "dbine_ana@%").unwrap_or_else(|| panic!("the user: {all:?}"));
    assert_eq!(ana.kind, PrincipalKind::User);
    assert_eq!(ana.member_of, vec!["dbine_lect".to_string()]);
    assert_eq!(ana.disabled, Some(false));
    assert!(!ana.system);
    assert!(all.iter().any(|p| p.name == "dbine_lect" && p.kind == PrincipalKind::Role), "{all:?}");
    assert!(all.iter().any(|p| p.name.starts_with("root@") && p.system && p.superuser == Some(true)));

    let g = s.grants("dbine_ana@%").await.unwrap();
    let upd = g.iter().find(|x| x.privilege == "UPDATE").unwrap_or_else(|| panic!("direct UPDATE: {g:?}"));
    assert_eq!((upd.object.as_deref(), upd.object_kind.as_deref(), upd.grantable, upd.via.as_deref()), (Some("dbine_sec.facturas"), Some("table"), true, None));
    let ins = g.iter().find(|x| x.privilege == "INSERT").expect("INSERT on the database");
    assert_eq!((ins.object.as_deref(), ins.object_kind.as_deref()), (Some("dbine_sec"), Some("schema")));
    let sel = g.iter().find(|x| x.privilege == "SELECT").expect("SELECT through the role");
    assert_eq!(sel.via.as_deref(), Some("dbine_lect"));

    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["UPDATE".into()], object: Some(table), from: "dbine_ana@%".into() })).await;
    run(&mut s, &script(SecurityAction::Revoke { privileges: vec!["INSERT".into()], object: Some(db), from: "dbine_ana@%".into() })).await;
    let g = s.grants("dbine_ana@%").await.unwrap();
    assert!(g.iter().all(|x| x.privilege != "UPDATE" && x.privilege != "INSERT"), "{g:?}");
    run(&mut s, &script(SecurityAction::SetPassword { name: "dbine_ana@%".into(), password: "Otra_Pw_987!".into() })).await;
    run(&mut s, &script(SecurityAction::SetLogin { name: "dbine_ana@%".into(), enabled: false })).await;
    assert_eq!(s.principals().await.unwrap().iter().find(|p| p.name == "dbine_ana@%").unwrap().disabled, Some(true));
    run(&mut s, &script(SecurityAction::SetLogin { name: "dbine_ana@%".into(), enabled: true })).await;
    run(&mut s, &script(SecurityAction::RemoveMember { role: "dbine_lect".into(), member: "dbine_ana@%".into() })).await;
    assert!(s.principals().await.unwrap().iter().find(|p| p.name == "dbine_ana@%").unwrap().member_of.is_empty());
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_ana@%".into(), kind: PrincipalKind::User })).await;
    run(&mut s, &script(SecurityAction::Drop { name: "dbine_lect".into(), kind: PrincipalKind::Role })).await;
    assert!(s.principals().await.unwrap().iter().all(|p| p.name != "dbine_ana@%" && p.name != "dbine_lect"));
    drop(s);
    run(&mut admin, "DROP DATABASE dbine_sec").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mysql_users_roles_and_grants() {
    exercise("mysql", "DBINE_TEST_MYSQL_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn mariadb_users_roles_and_grants() {
    exercise("mariadb", "DBINE_TEST_MARIADB_URL").await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn tidb_users_roles_and_grants() {
    exercise("tidb", "DBINE_TEST_TIDB_URL").await;
}

/// A grant's object as the UI turns it back into an ObjectRef to revoke it
/// (web/src/views/SecurityView.vue, objectOf).
fn object_of(g: &dbine_driver::Grant) -> Option<ObjectRef> {
    let o = g.object.as_deref()?;
    let kind = g.object_kind.clone().unwrap_or_else(|| "table".into());
    if kind == "database" || kind == "schema" {
        return Some(ObjectRef { kind, schema: None, name: o.into() });
    }
    Some(match o.split_once('.') {
        Some((s, n)) if !s.is_empty() => ObjectRef { kind, schema: Some(s.into()), name: n.into() },
        _ => ObjectRef { kind, schema: None, name: o.into() },
    })
}

/// The engines whose roles are named `role:<name>`: a user and a role,
/// membership, grants on a table and on the database, read back and
/// revoked the way the UI does it.
struct Olap {
    id: &'static str,
    env: &'static str,
    /// The admin connection's name in the principal list.
    user: &'static str,
    create_table: &'static str,
    /// Accounts can be disabled.
    set_login: bool,
    /// WITH GRANT OPTION exists.
    grant_option: bool,
    /// The engine's names for SELECT, UPDATE and INSERT.
    privs: [&'static str; 3],
}

async fn exercise_prefixed(e: Olap) {
    let Some(cfg) = cfg(e.id, e.env) else {
        eprintln!("{} not set; skipping", e.env);
        return;
    };
    let d = dbine_driver_mysql::drivers().into_iter().find(|d| d.info().id == e.id).unwrap();
    assert!(d.security().is_some());
    let mut admin = d.connect(&cfg, None).await.unwrap();
    let script = |a: SecurityAction| d.security_script(&a).unwrap();
    let ana = e.user;
    let lect = "role:dbine_lect";
    let [select, update, insert] = e.privs;
    // Leftovers from an interrupted run.
    for a in [SecurityAction::Drop { name: ana.into(), kind: PrincipalKind::User }, SecurityAction::Drop { name: lect.into(), kind: PrincipalKind::Role }] {
        let mut out = QueryOutcome::default();
        let _ = admin.execute(&script(a), 10, &mut out).await;
    }
    run(&mut admin, "DROP DATABASE IF EXISTS dbine_sec").await;
    run(&mut admin, "CREATE DATABASE dbine_sec").await;
    let mut s = d.connect(&cfg, Some("dbine_sec")).await.unwrap();
    run(&mut s, e.create_table).await;

    run(&mut s, &script(SecurityAction::CreateUser { name: ana.split('@').next().unwrap().into(), password: Some("Pw_12345!x".into()) })).await;
    run(&mut s, &script(SecurityAction::CreateRole { name: "dbine_lect".into() })).await;
    run(&mut s, &script(SecurityAction::AddMember { role: lect.into(), member: ana.into() })).await;
    let table = ObjectRef { kind: "table".into(), schema: Some("dbine_sec".into()), name: "facturas".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec![select.into()], object: Some(table.clone()), to: lect.into(), grantable: false })).await;
    run(&mut s, &script(SecurityAction::Grant { privileges: vec![update.into()], object: Some(table.clone()), to: ana.into(), grantable: e.grant_option })).await;
    let db = ObjectRef { kind: "schema".into(), schema: None, name: "dbine_sec".into() };
    run(&mut s, &script(SecurityAction::Grant { privileges: vec![insert.into()], object: Some(db), to: ana.into(), grantable: false })).await;

    let all = s.principals().await.unwrap();
    let p = all.iter().find(|p| p.name == ana).unwrap_or_else(|| panic!("the user: {all:?}"));
    assert_eq!(p.kind, PrincipalKind::User);
    assert_eq!(p.member_of, vec![lect.to_string()]);
    assert!(!p.system);
    assert!(all.iter().any(|p| p.name == lect && p.kind == PrincipalKind::Role && !p.system), "{all:?}");
    assert!(all.iter().any(|p| p.kind == PrincipalKind::User && p.system && p.superuser == Some(true)), "{all:?}");

    let g = s.grants(ana).await.unwrap();
    let upd = g.iter().find(|x| x.privilege == update).unwrap_or_else(|| panic!("direct UPDATE: {g:?}"));
    assert_eq!((upd.object.as_deref(), upd.object_kind.as_deref(), upd.grantable, upd.via.as_deref()), (Some("dbine_sec.facturas"), Some("table"), e.grant_option, None));
    let ins = g.iter().find(|x| x.privilege == insert && x.via.is_none()).unwrap_or_else(|| panic!("INSERT on the database: {g:?}"));
    assert!(ins.object.as_deref().is_some_and(|o| o.starts_with("dbine_sec")), "{ins:?}");
    let sel = g.iter().find(|x| x.privilege == select).unwrap_or_else(|| panic!("SELECT through the role: {g:?}"));
    assert_eq!((sel.via.as_deref(), sel.object.as_deref()), (Some(lect), Some("dbine_sec.facturas")));
    let rg = s.grants(lect).await.unwrap();
    assert!(rg.iter().any(|x| x.privilege == select && x.via.is_none()), "{rg:?}");

    // Revoke what was read, as the UI does.
    for x in [upd.clone(), ins.clone()] {
        run(&mut s, &script(SecurityAction::Revoke { privileges: vec![x.privilege.clone()], object: object_of(&x), from: ana.into() })).await;
    }
    let g = s.grants(ana).await.unwrap();
    assert!(g.iter().all(|x| x.privilege != update && x.privilege != insert), "{g:?}");
    run(&mut s, &script(SecurityAction::SetPassword { name: ana.into(), password: "Otra_Pw_987!".into() })).await;
    if e.set_login {
        run(&mut s, &script(SecurityAction::SetLogin { name: ana.into(), enabled: false })).await;
        assert_eq!(s.principals().await.unwrap().iter().find(|p| p.name == ana).unwrap().disabled, Some(true));
        run(&mut s, &script(SecurityAction::SetLogin { name: ana.into(), enabled: true })).await;
    }
    run(&mut s, &script(SecurityAction::RemoveMember { role: lect.into(), member: ana.into() })).await;
    assert!(s.principals().await.unwrap().iter().find(|p| p.name == ana).unwrap().member_of.is_empty());
    run(&mut s, &script(SecurityAction::Drop { name: ana.into(), kind: PrincipalKind::User })).await;
    run(&mut s, &script(SecurityAction::Drop { name: lect.into(), kind: PrincipalKind::Role })).await;
    assert!(s.principals().await.unwrap().iter().all(|p| p.name != ana && p.name != lect));
    drop(s);
    run(&mut admin, "DROP DATABASE dbine_sec").await;
}

const SQL_PRIVS: [&str; 3] = ["SELECT", "UPDATE", "INSERT"];
const OLAP_TABLE: &str = "CREATE TABLE facturas (id int, total decimal(10,2)) DUPLICATE KEY(id) DISTRIBUTED BY HASH(id) BUCKETS 1 PROPERTIES ('replication_num' = '1')";

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn starrocks_users_roles_and_grants() {
    exercise_prefixed(Olap { id: "starrocks", env: "DBINE_TEST_STARROCKS_URL", user: "dbine_ana@%", create_table: OLAP_TABLE, set_login: false, grant_option: true, privs: SQL_PRIVS }).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn doris_users_roles_and_grants() {
    exercise_prefixed(Olap { id: "doris", env: "DBINE_TEST_DORIS_URL", user: "dbine_ana@%", create_table: OLAP_TABLE, set_login: false, grant_option: false, privs: ["SELECT_PRIV", "ALTER_PRIV", "LOAD_PRIV"] }).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn databend_users_roles_and_grants() {
    exercise_prefixed(Olap {
        id: "databend",
        env: "DBINE_TEST_DATABEND_URL",
        user: "dbine_ana",
        create_table: "CREATE TABLE facturas (id int, total decimal(10,2))",
        set_login: true,
        grant_option: false,
        privs: SQL_PRIVS,
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn oceanbase_users_roles_and_grants() {
    exercise("oceanbase", "DBINE_TEST_OCEANBASE_URL").await;
}

/// VeloDB is managed Doris: the same catalog, against a Doris server.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn velodb_users_roles_and_grants() {
    exercise_prefixed(Olap { id: "velodb", env: "DBINE_TEST_DORIS_URL", user: "dbine_ana@%", create_table: OLAP_TABLE, set_login: false, grant_option: false, privs: ["SELECT_PRIV", "ALTER_PRIV", "LOAD_PRIV"] }).await;
}
