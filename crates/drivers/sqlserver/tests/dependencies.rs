//! What depends on a table or a column (`Session::dependents`), against a
//! real server. Reads `DBINE_TEST_SQLSERVER_URL` like `integration.rs`.

use dbine_driver::{kinds, Confidence, ConnectionConfig, DependencyReport, DependencyScan, DependencyTarget, ObjectRef, QueryOutcome, Relation, Session};

fn parse_url(url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port,
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap();
}

fn target(name: &str, column: Option<&str>) -> DependencyTarget {
    DependencyTarget { object: ObjectRef { kind: kinds::TABLE.into(), schema: Some("dbo".into()), name: name.into() }, column: column.map(Into::into) }
}

fn find<'a>(r: &'a DependencyReport, name: &str, relation: Relation) -> Option<&'a dbine_driver::Dependent> {
    r.items.iter().find(|d| d.name == name && d.relation == relation)
}

#[tokio::test]
#[ignore]
async fn dependents() {
    let Ok(url) = std::env::var("DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let cfg = parse_url(&url);
    let d = dbine_driver_sqlserver::drivers().remove(0);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    run(
        &mut admin,
        "IF DB_ID('dbine_deps') IS NOT NULL BEGIN ALTER DATABASE dbine_deps SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE dbine_deps; END
         GO
         CREATE DATABASE dbine_deps",
    )
    .await;
    let mut s = d.connect(&cfg, Some("dbine_deps")).await.unwrap();
    run(
        &mut s,
        "CREATE TABLE dbo.Clientes (id int PRIMARY KEY, Pepe int CONSTRAINT CK_Pepe CHECK (Pepe > 0), nombre nvarchar(50))
         GO
         CREATE INDEX IX_Pepe ON dbo.Clientes (Pepe)
         GO
         CREATE TABLE dbo.Pedidos (id int PRIMARY KEY, cliente_id int CONSTRAINT FK_Pedidos_Clientes REFERENCES dbo.Clientes (id), Pepe int)
         GO
         CREATE VIEW dbo.vPepe WITH SCHEMABINDING AS SELECT c.id, c.Pepe FROM dbo.Clientes c
         GO
         CREATE VIEW dbo.vNombres AS SELECT nombre FROM dbo.Clientes
         GO
         CREATE PROCEDURE dbo.pPepe AS
           -- only a comment names dbo.Clientes here? no: the query does
           SELECT Pepe
             FROM dbo.Clientes
         GO
         CREATE PROCEDURE dbo.pDinamico AS EXEC sp_executesql N'SELECT Pepe FROM dbo.Clientes'
         GO
         CREATE PROCEDURE dbo.pOtraTabla AS SELECT Pepe FROM dbo.Pedidos",
    )
    .await;
    let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);

    let table = s.dependents(&target("Clientes", None), &scan).await.unwrap();
    eprintln!("{table:#?}");
    assert_eq!(find(&table, "Pedidos", Relation::ForeignKey).unwrap().confidence, Confidence::Confirmed);
    for name in ["vPepe", "vNombres", "pPepe"] {
        assert_eq!(find(&table, name, Relation::Code).unwrap_or_else(|| panic!("{name}")).confidence, Confidence::Confirmed, "{name}");
    }
    assert_eq!(find(&table, "pDinamico", Relation::Code).unwrap().confidence, Confidence::Review);
    assert!(find(&table, "pOtraTabla", Relation::Code).is_none());

    let pepe = s.dependents(&target("Clientes", Some("Pepe")), &scan).await.unwrap();
    eprintln!("{pepe:#?}");
    assert_eq!(find(&pepe, "vPepe", Relation::Code).unwrap().confidence, Confidence::Confirmed);
    let proc = find(&pepe, "pPepe", Relation::Code).unwrap();
    assert_eq!(proc.confidence, Confidence::Probable);
    assert_eq!(proc.mentions.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), ["SELECT Pepe"]);
    assert_eq!(find(&pepe, "pDinamico", Relation::Code).unwrap().confidence, Confidence::Review);
    assert!(find(&pepe, "vNombres", Relation::Code).is_none());
    assert!(find(&pepe, "pOtraTabla", Relation::Code).is_none());
    assert!(find(&pepe, "Clientes", Relation::Index).is_some());
    assert!(find(&pepe, "Clientes", Relation::Check).is_some());

    // The generic scan (what every other engine gets) finds the same code.
    let generic = dbine_driver::dependencies::scan(s.as_mut(), &target("Clientes", Some("Pepe")), &scan).await.unwrap();
    for name in ["vPepe", "pPepe", "pDinamico"] {
        assert!(find(&generic, name, Relation::Code).is_some(), "generic: {name}");
    }
    assert!(find(&generic, "pOtraTabla", Relation::Code).is_none());

    drop(s);
    run(&mut admin, "ALTER DATABASE dbine_deps SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE dbine_deps").await;
}
