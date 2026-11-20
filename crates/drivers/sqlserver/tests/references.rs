//! What an object uses (`Session::references`), against a real server: a
//! view, a procedure, a table with a foreign key and a computed column, a
//! trigger and synonyms, with references to another database and through
//! a linked server; and the generic version, with a note, for a user who
//! can't read `sys.sql_expression_dependencies`, in both directions
//! (`references` and `dependents`). Everything it creates
//! (two databases, a linked server and a login) is dropped at the end, also
//! when a check fails.
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
//!   cargo test -p dbine-driver-sqlserver --test references -- --ignored --nocapture
//! DBINE_TEST_BABELFISH_URL='mssql://babelfish_user:12345678@localhost:25714' \
//!   cargo test -p dbine-driver-sqlserver --test references babelfish -- --ignored --nocapture
//! ```

use dbine_driver::{
    kinds, Confidence, ConnectionConfig, DependencyReport, DependencyScan, DependencyTarget,
    Dependent, Driver, ObjectRef, QueryOutcome, Relation, Session,
};
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

fn parse_url(url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport
        .rsplit_once(':')
        .map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port,
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        trust_server_certificate: true,
        ..Default::default()
    }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.unwrap();
}

fn target(kind: &str, schema: &str, name: &str) -> DependencyTarget {
    DependencyTarget {
        object: ObjectRef {
            kind: kind.into(),
            schema: Some(schema.into()),
            name: name.into(),
        },
        column: None,
    }
}

fn find<'a>(
    r: &'a DependencyReport,
    kind: &str,
    schema: &str,
    name: &str,
) -> Option<&'a Dependent> {
    r.items.iter().find(|d| {
        d.kind == kind && d.schema.as_deref() == Some(schema) && d.name.eq_ignore_ascii_case(name)
    })
}

#[track_caller]
fn confirmed<'a>(r: &'a DependencyReport, kind: &str, schema: &str, name: &str) -> &'a Dependent {
    let d = find(r, kind, schema, name).unwrap_or_else(|| panic!("{kind} {schema}.{name}: {r:#?}"));
    assert_eq!(d.confidence, Confidence::Confirmed, "{d:?}");
    d
}

/// A name unique to this run, so runs never step on each other.
fn suffix() -> String {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        % 100_000_000;
    format!("{}_{ms}", std::process::id())
}

fn drop_database(name: &str) -> String {
    format!(
        "IF DB_ID('{name}') IS NOT NULL BEGIN ALTER DATABASE [{name}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{name}]; END"
    )
}

#[tokio::test]
#[ignore]
async fn sqlserver() {
    let Ok(url) = std::env::var("DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let cfg = parse_url(&url);
    let d = dbine_driver_sqlserver::drivers().remove(0);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    let id = suffix();
    let (db, other, lnk, low) = (
        format!("dbine_refs_{id}"),
        format!("dbine_refs_other_{id}"),
        format!("DBINE_REFS_LNK_{id}"),
        format!("dbine_refs_low_{id}"),
    );
    let checks = AssertUnwindSafe(checks(&d, &cfg, &mut admin, &db, &other, &lnk, &low))
        .catch_unwind()
        .await;
    run(
        &mut admin,
        &format!(
            "{}
             GO
             {}
             GO
             IF EXISTS (SELECT 1 FROM sys.servers WHERE name = N'{lnk}') EXEC sp_dropserver N'{lnk}', 'droplogins'
             GO
             IF SUSER_ID(N'{low}') IS NOT NULL DROP LOGIN [{low}]",
            drop_database(&db),
            drop_database(&other)
        ),
    )
    .await;
    if let Err(p) = checks {
        std::panic::resume_unwind(p);
    }
}

async fn checks(
    d: &Arc<dyn Driver>,
    cfg: &ConnectionConfig,
    admin: &mut Box<dyn Session>,
    db: &str,
    other: &str,
    lnk: &str,
    low: &str,
) {
    run(
        admin,
        &format!(
            "CREATE DATABASE [{db}]
             GO
             CREATE DATABASE [{other}]
             GO
             CREATE TABLE [{other}].dbo.Remota (id int)
             GO
             EXEC sp_addlinkedserver @server = N'{lnk}', @srvproduct = N'', @provider = N'MSOLEDBSQL', @datasrc = N'localhost'
             GO
             CREATE LOGIN [{low}] WITH PASSWORD = N'Pw_12345!low', CHECK_POLICY = OFF"
        ),
    )
    .await;
    let mut s = d.connect(cfg, Some(db)).await.unwrap();
    run(
        &mut s,
        &format!(
            "CREATE SCHEMA ventas
             GO
             CREATE TABLE dbo.Clientes (id int PRIMARY KEY, nombre nvarchar(50))
             GO
             CREATE TABLE ventas.Clientes (id int PRIMARY KEY)
             GO
             CREATE SEQUENCE dbo.seq
             GO
             CREATE TYPE dbo.Ids AS TABLE (id int)
             GO
             CREATE FUNCTION dbo.f_ok(@x int) RETURNS bit AS BEGIN RETURN CASE WHEN @x > 0 THEN 1 ELSE 0 END END
             GO
             CREATE TABLE dbo.Pedidos (
               id int PRIMARY KEY,
               cliente_id int CONSTRAINT FK_Pedidos_Clientes REFERENCES dbo.Clientes (id),
               q int CONSTRAINT CK_q CHECK (dbo.f_ok(q) = 1),
               total AS dbo.f_ok(q))
             GO
             CREATE VIEW dbo.vPedidos AS
               SELECT c.nombre, p.q
                 FROM dbo.Clientes c
                 JOIN dbo.Pedidos p ON p.cliente_id = c.id
             GO
             CREATE PROCEDURE dbo.pUsa @ids dbo.Ids READONLY AS
               SELECT * FROM dbo.vPedidos;
               SELECT * FROM dbo.Clientes WHERE id IN (SELECT id FROM @ids);
               SELECT NEXT VALUE FOR dbo.seq;
               SELECT * FROM [{other}].dbo.Remota;
               SELECT * FROM [{lnk}].[{other}].dbo.Remota;
               SELECT dbo.f_ok(1);
               SELECT * FROM dbo.Tarde;
               SELECT * FROM dbo.NoExiste;
               SELECT * FROM #tmp;
             GO
             CREATE TABLE dbo.Tarde (id int)
             GO
             CREATE TRIGGER dbo.trPedidos ON dbo.Pedidos AFTER INSERT AS SELECT COUNT(*) FROM ventas.Clientes
             GO
             CREATE SYNONYM dbo.sRemota FOR [{other}].dbo.Remota
             GO
             CREATE SYNONYM dbo.sClientes FOR dbo.Clientes
             GO
             CREATE USER [{low}] FOR LOGIN [{low}]
             GO
             GRANT VIEW DEFINITION ON SCHEMA::dbo TO [{low}]"
        ),
    )
    .await;
    let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);

    // A view: its two tables, with the lines that name them.
    let view = s
        .references(&target(kinds::VIEW, "dbo", "vPedidos"), &scan)
        .await
        .unwrap();
    eprintln!("view: {view:#?}");
    assert_eq!(view.note, None);
    assert_eq!(view.scanned, 1);
    confirmed(&view, kinds::TABLE, "dbo", "Clientes");
    let pedidos = confirmed(&view, kinds::TABLE, "dbo", "Pedidos");
    assert_eq!(
        pedidos.mentions[0].text,
        "JOIN dbo.Pedidos p ON p.cliente_id = c.id"
    );
    assert!(view.external.is_empty());
    assert_eq!(view.items.len(), 2);

    // A procedure: a view, a table, a sequence, a type and a function;
    // another database and a linked server outside.
    let proc = s
        .references(&target(kinds::PROCEDURE, "dbo", "pUsa"), &scan)
        .await
        .unwrap();
    eprintln!("procedure: {proc:#?}");
    confirmed(&proc, kinds::VIEW, "dbo", "vPedidos");
    confirmed(&proc, kinds::TABLE, "dbo", "Clientes");
    confirmed(&proc, kinds::SEQUENCE, "dbo", "seq");
    confirmed(&proc, kinds::TYPE, "dbo", "Ids");
    let f = confirmed(&proc, kinds::FUNCTION, "dbo", "f_ok");
    assert_eq!(f.mentions[0].text, "SELECT dbo.f_ok(1);");
    // Created after the procedure: the catalog resolves it when read.
    assert!(find(&proc, kinds::TABLE, "dbo", "Tarde").is_some());
    assert!(find(&proc, kinds::TABLE, "dbo", "NoExiste").is_none());
    assert!(proc.items.iter().all(|d| !d.name.starts_with('#')));
    let cross = proc
        .external
        .iter()
        .find(|x| x.server.is_none())
        .expect("another database");
    assert_eq!(cross.system.as_deref(), Some("SQL Server"));
    assert_eq!(cross.database.as_deref(), Some(other));
    assert_eq!(
        (cross.schema.as_deref(), cross.name.as_deref()),
        (Some("dbo"), Some("Remota"))
    );
    assert_eq!(cross.mentions.len(), 1, "{cross:?}");
    assert!(cross.mentions[0]
        .text
        .starts_with(&format!("SELECT * FROM [{other}]")));
    let linked = proc
        .external
        .iter()
        .find(|x| x.server.as_deref() == Some(lnk))
        .expect("linked server");
    assert_eq!(linked.system.as_deref(), Some("MSOLEDBSQL"));
    assert_eq!(linked.database.as_deref(), Some(other));
    assert_eq!(linked.mentions.len(), 1, "{linked:?}");
    assert!(linked.mentions[0]
        .text
        .starts_with(&format!("SELECT * FROM [{lnk}]")));
    assert_eq!(proc.external.len(), 2);

    // Never less than the generic version.
    for t in [
        target(kinds::VIEW, "dbo", "vPedidos"),
        target(kinds::PROCEDURE, "dbo", "pUsa"),
        target(kinds::TABLE, "dbo", "Pedidos"),
        target(kinds::TRIGGER, "dbo", "trPedidos"),
    ] {
        let ours = s.references(&t, &scan).await.unwrap();
        let generic = dbine_driver::dependencies::references_scan(s.as_mut(), &t, &scan)
            .await
            .unwrap();
        for g in &generic.items {
            assert!(
                find(
                    &ours,
                    &g.kind,
                    g.schema.as_deref().unwrap_or_default(),
                    &g.name
                )
                .is_some(),
                "{}: the generic version finds {g:?}",
                t.object.name
            );
        }
    }

    // A table: the foreign key's table, and the function its CHECK and its
    // computed column use.
    let table = s
        .references(&target(kinds::TABLE, "dbo", "Pedidos"), &scan)
        .await
        .unwrap();
    eprintln!("table: {table:#?}");
    let fk = table
        .items
        .iter()
        .find(|x| x.relation == Relation::ForeignKey)
        .unwrap();
    assert_eq!(
        (fk.schema.as_deref(), fk.name.as_str()),
        (Some("dbo"), "Clientes")
    );
    assert_eq!(fk.confidence, Confidence::Confirmed);
    assert_eq!(
        fk.detail.as_deref(),
        Some("FK_Pedidos_Clientes (cliente_id) → dbo.Clientes (id)")
    );
    let check = table
        .items
        .iter()
        .find(|x| x.relation == Relation::Check)
        .unwrap();
    assert_eq!(
        (check.name.as_str(), check.detail.as_deref()),
        ("f_ok", Some("CK_q"))
    );
    let computed = table
        .items
        .iter()
        .find(|x| x.relation == Relation::Code)
        .unwrap();
    assert_eq!(
        (computed.name.as_str(), computed.detail.as_deref()),
        ("f_ok", Some("total"))
    );
    assert_eq!(table.items.len(), 3);

    // A trigger: its table and what its body reads.
    let trigger = s
        .references(&target(kinds::TRIGGER, "dbo", "trPedidos"), &scan)
        .await
        .unwrap();
    eprintln!("trigger: {trigger:#?}");
    let parent = confirmed(&trigger, kinds::TABLE, "dbo", "Pedidos");
    assert!(parent.mentions[0].text.contains("ON dbo.Pedidos"));
    confirmed(&trigger, kinds::TABLE, "ventas", "Clientes");
    assert!(find(&trigger, kinds::TABLE, "dbo", "Clientes").is_none());

    // Synonyms: one to another database, one here.
    let remote = s
        .references(&target(kinds::SYNONYM, "dbo", "sRemota"), &scan)
        .await
        .unwrap();
    eprintln!("synonym: {remote:#?}");
    assert!(remote.items.is_empty());
    assert_eq!(remote.external.len(), 1);
    assert_eq!(remote.external[0].database.as_deref(), Some(other));
    assert_eq!(remote.external[0].name.as_deref(), Some("Remota"));
    let local = s
        .references(&target(kinds::SYNONYM, "dbo", "sClientes"), &scan)
        .await
        .unwrap();
    confirmed(&local, kinds::TABLE, "dbo", "Clientes");
    assert!(local.external.is_empty());
    drop(s);

    // Without SELECT on the dependency view: the generic version, and a note.
    let mut low_cfg = cfg.clone();
    low_cfg.username = Some(low.into());
    low_cfg.password = Some("Pw_12345!low".into());
    let mut weak = d.connect(&low_cfg, Some(db)).await.unwrap();
    let fallback = weak
        .references(&target(kinds::VIEW, "dbo", "vPedidos"), &scan)
        .await
        .unwrap();
    eprintln!("without permission: {fallback:#?}");
    assert!(fallback
        .note
        .as_deref()
        .is_some_and(|n| n.contains("sys.sql_expression_dependencies")));
    for name in ["Clientes", "Pedidos"] {
        assert_eq!(
            find(&fallback, kinds::TABLE, "dbo", name)
                .unwrap_or_else(|| panic!("{name}"))
                .confidence,
            Confidence::Probable
        );
    }

    // The other direction too (`dependents`): the generic scan, and a note,
    // instead of the server's refusal.
    let users = weak
        .dependents(&target(kinds::TABLE, "dbo", "Clientes"), &scan)
        .await
        .expect("dependents without SELECT on the dependency view");
    eprintln!("dependents without permission: {users:#?}");
    assert!(users
        .note
        .as_deref()
        .is_some_and(|n| n.contains("sys.sql_expression_dependencies")));
    let fk = users
        .items
        .iter()
        .find(|x| x.relation == Relation::ForeignKey)
        .expect("foreign key");
    assert_eq!(
        (fk.name.as_str(), fk.confidence),
        ("Pedidos", Confidence::Confirmed)
    );
    for (kind, name) in [(kinds::VIEW, "vPedidos"), (kinds::PROCEDURE, "pUsa")] {
        assert_eq!(
            find(&users, kind, "dbo", name)
                .unwrap_or_else(|| panic!("{name}"))
                .confidence,
            Confidence::Probable
        );
    }
}

/// Babelfish: its catalog answers what it records; the generic version
/// fills in the rest, and nothing the generic version finds is lost.
#[tokio::test]
#[ignore]
async fn babelfish() {
    let Ok(url) = std::env::var("DBINE_TEST_BABELFISH_URL") else {
        eprintln!("DBINE_TEST_BABELFISH_URL not set; skipping");
        return;
    };
    let mut cfg = parse_url(&url);
    cfg.driver = "babelfish".into();
    let d = dbine_driver_sqlserver::drivers()
        .into_iter()
        .find(|d| d.info().id == "babelfish")
        .unwrap();
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    let db = format!("dbine_bbf_refs_{}", suffix());
    admin.create_database(&db).await.expect("create database");
    let checks = AssertUnwindSafe(async {
        let mut s = d.connect(&cfg, Some(&db)).await.unwrap();
        run(
            &mut s,
            "CREATE TABLE dbo.Clientes (id int PRIMARY KEY, nombre nvarchar(50))
             GO
             CREATE TABLE dbo.Pedidos (id int PRIMARY KEY, cliente_id int CONSTRAINT FK_Pedidos_Clientes REFERENCES dbo.Clientes (id))
             GO
             CREATE VIEW dbo.vPedidos AS SELECT c.nombre FROM dbo.Clientes c JOIN dbo.Pedidos p ON p.cliente_id = c.id
             GO
             CREATE PROCEDURE dbo.pUsa AS SELECT * FROM dbo.vPedidos",
        )
        .await;
        let scan =
            DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
        let view = s
            .references(&target(kinds::VIEW, "dbo", "vPedidos"), &scan)
            .await
            .unwrap();
        eprintln!("view: {view:#?}");
        for name in ["Clientes", "Pedidos"] {
            assert!(find(&view, kinds::TABLE, "dbo", name).is_some(), "{name}");
        }
        let proc = s
            .references(&target(kinds::PROCEDURE, "dbo", "pUsa"), &scan)
            .await
            .unwrap();
        eprintln!("procedure: {proc:#?}");
        assert!(find(&proc, kinds::VIEW, "dbo", "vPedidos").is_some());
        let table = s
            .references(&target(kinds::TABLE, "dbo", "Pedidos"), &scan)
            .await
            .unwrap();
        eprintln!("table: {table:#?}");
        let fk = table
            .items
            .iter()
            .find(|x| x.relation == Relation::ForeignKey)
            .expect("foreign key");
        assert_eq!(fk.name.to_lowercase(), "clientes");
        assert_eq!(fk.confidence, Confidence::Confirmed);
    })
    .catch_unwind()
    .await;
    admin.drop_database(&db).await.unwrap();
    if let Err(p) = checks {
        std::panic::resume_unwind(p);
    }
}
