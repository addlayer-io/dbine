//! A login without VIEW DEFINITION reads the structure (as a migration does):
//! the catalog gives CHECK, DEFAULT and computed definitions as NULL. They
//! must come back as the table's warnings, never as `CHECK ()`; with the
//! permission they come through. `DBINE_TEST_SQLSERVER_URL` (see
//! tests/integration.rs):
//! `cargo test -p dbine-driver-sqlserver --test view_definition -- --ignored`

use dbine_driver::{ConnectionConfig, DdlParts, ObjectRef, QueryOutcome, Session, TableSchema};

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_SQLSERVER_URL").ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
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
    s.execute(sql, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

const DB: &str = "dbine_viewdef";
const LOGIN: &str = "dbine_viewdef_user";

async fn table(s: &mut Box<dyn Session>) -> TableSchema {
    s.database_schema()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.name == "pedido")
        .expect("the table")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn hidden_definitions_are_warnings_not_empty_ddl() {
    let Some(cfg) = cfg() else { return };
    let d = dbine_driver_sqlserver::drivers()
        .into_iter()
        .find(|d| d.info().id == "sqlserver")
        .unwrap();
    let mut admin = d.connect(&cfg, Some("master")).await.unwrap();
    let drop_all = format!(
        "IF DB_ID('{DB}') IS NOT NULL BEGIN ALTER DATABASE {DB} SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE {DB}; END; \
         IF DB_ID('{DB}_c') IS NOT NULL BEGIN ALTER DATABASE {DB}_c SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE {DB}_c; END; \
         IF SUSER_ID('{LOGIN}') IS NOT NULL DROP LOGIN {LOGIN};"
    );
    run(&mut admin, &drop_all).await;
    run(
        &mut admin,
        &format!(
            "CREATE DATABASE {DB}; CREATE DATABASE {DB}_c; \
             CREATE LOGIN {LOGIN} WITH PASSWORD = 'Pw_viewdef_123!', CHECK_POLICY = OFF;"
        ),
    )
    .await;
    run(
        &mut admin,
        &format!(
            "USE {DB}; \
             CREATE TABLE dbo.pedido (id int NOT NULL PRIMARY KEY, cantidad int NOT NULL CONSTRAINT DF_pedido_cantidad DEFAULT (1), \
               doble AS (cantidad * 2), CONSTRAINT CK_pedido_cantidad CHECK (cantidad > 0)); \
             CREATE USER {LOGIN} FOR LOGIN {LOGIN}; GRANT SELECT ON SCHEMA::dbo TO {LOGIN};"
        ),
    )
    .await;

    let mut user_cfg = cfg.clone();
    user_cfg.username = Some(LOGIN.into());
    user_cfg.password = Some("Pw_viewdef_123!".into());
    let ddl = |d: &dyn dbine_driver::Driver, t: &TableSchema| {
        d.table_ddl(
            t,
            DdlParts {
                create: true,
                ..Default::default()
            },
        )
        .unwrap()
    };

    // Without VIEW DEFINITION: warnings, and nothing written empty.
    let mut user = d.connect(&user_cfg, Some(DB)).await.unwrap();
    let t = table(&mut user).await;
    println!("{:#?}", t.warnings);
    assert!(t.checks.is_empty(), "{:?}", t.checks);
    assert_eq!(t.warnings.len(), 3, "{:?}", t.warnings);
    for (what, name) in [
        ("CHECK", "CK_pedido_cantidad"),
        ("por defecto", "cantidad"),
        ("calculada", "doble"),
    ] {
        assert!(
            t.warnings.iter().any(|w| w.contains(what)
                && w.contains(name)
                && w.contains("GRANT VIEW DEFINITION TO [dbine_viewdef_user]")),
            "{what} {name}: {:?}",
            t.warnings
        );
    }
    let sql = ddl(d.as_ref(), &t);
    println!("{sql}");
    assert!(!sql.contains("CHECK") && !sql.contains("DEFAULT"), "{sql}");
    assert!(sql.contains("[doble] int"), "{sql}");

    // Cloning needs the database's VIEW DEFINITION: refused, saying so.
    let mut target = d.connect(&cfg, Some(&format!("{DB}_c"))).await.unwrap();
    let pedido = [ObjectRef {
        kind: "table".into(),
        schema: Some("dbo".into()),
        name: "pedido".into(),
    }];
    let err = d
        .clone_script(user.as_mut(), target.as_mut(), &pedido)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("VIEW DEFINITION"), "{err}");
    drop(user);

    // With VIEW DEFINITION: the CHECK, the DEFAULT and the expression, and
    // the clone has them too.
    run(
        &mut admin,
        &format!(
            "USE {DB}; GRANT VIEW DEFINITION TO {LOGIN}; GRANT SELECT ON sys.sql_expression_dependencies TO {LOGIN};"
        ),
    )
    .await;
    let mut user = d.connect(&user_cfg, Some(DB)).await.unwrap();
    let script = d
        .clone_script(user.as_mut(), target.as_mut(), &pedido)
        .await
        .unwrap();
    assert!(
        script
            .after
            .iter()
            .any(|s| s.contains("CK_pedido_cantidad")),
        "{:?}",
        script.after
    );
    assert!(
        script
            .notes
            .iter()
            .all(|n| !n.contains(": no se pudo leer ")),
        "{:?}",
        script.notes
    );
    let t = table(&mut user).await;
    assert!(t.warnings.is_empty(), "{:?}", t.warnings);
    assert_eq!(t.checks.len(), 1);
    let sql = ddl(d.as_ref(), &t);
    assert!(
        sql.contains("CONSTRAINT [CK_pedido_cantidad] CHECK"),
        "{sql}"
    );
    assert!(sql.contains("DEFAULT"), "{sql}");
    assert!(sql.contains("[doble] AS"), "{sql}");
    drop(user);
    drop(target);

    run(&mut admin, "USE master;").await;
    run(&mut admin, &drop_all).await;
}
