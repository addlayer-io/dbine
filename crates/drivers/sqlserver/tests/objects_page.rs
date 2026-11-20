//! The explorer's pages (`list_objects_page`) against real servers: every
//! page is exactly what `objects::page_of` makes of `list_objects`, for each
//! kind, the schemas, the objects without one, "Otros", searches and
//! lookups, in pages of 7; and a folder of 25 views in pages of 10. Each
//! test works in its own scratch database and drops it at the end, also
//! when a check fails. Skipped without the URLs:
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
//! DBINE_TEST_BABELFISH_URL='mssql://babelfish_user:12345678@localhost:25714' \
//!   cargo test -p dbine-driver-sqlserver --test objects_page -- --ignored --nocapture
//! # The sqlserver_legacy host's driver (OpenSSL TLS), against the same server:
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:Pw_12345!@localhost:25013' \
//!   cargo test -p dbine-driver-sqlserver --features legacy-tls --test objects_page -- --ignored --nocapture
//! ```

use dbine_driver::objects::page_of;
use dbine_driver::{
    kinds, ConnectionConfig, DbObject, Driver, Error, ObjectQuery, QueryOutcome, Session,
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
        ..Default::default()
    }
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_sqlserver::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<QueryOutcome, Error> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out).await.map(|_| out)
}

/// Pages `q` both ways, `size` at a time: the same objects on each page,
/// and the same pages. Returns how many there were.
async fn same_pages(
    s: &mut Box<dyn Session>,
    all: &[DbObject],
    mut q: ObjectQuery,
    size: u32,
) -> usize {
    q.limit = size;
    let mut want_q = q.clone();
    let mut got_q = q.clone();
    let mut pages = 0;
    loop {
        let want = page_of(all, &want_q);
        let got = s
            .list_objects_page(&got_q)
            .await
            .unwrap_or_else(|e| panic!("{got_q:?}: {e}"));
        assert_eq!(got.objects, want.objects, "page {pages} of {q:?}");
        assert_eq!(
            got.next.is_some(),
            want.next.is_some(),
            "page {pages} of {q:?}: whether another follows"
        );
        pages += 1;
        match (got.next, want.next) {
            (Some(g), Some(w)) => {
                got_q.after = Some(g);
                want_q.after = Some(w);
            }
            _ => return pages,
        }
    }
}

fn q(f: impl FnOnce(&mut ObjectQuery)) -> ObjectQuery {
    let mut q = ObjectQuery::default();
    f(&mut q);
    q
}

/// The filter combinations every engine goes through.
fn queries(d: &dyn Driver, all: &[DbObject]) -> Vec<ObjectQuery> {
    let declared: Vec<String> = d
        .info()
        .object_kinds
        .iter()
        .map(|k| k.id.to_string())
        .collect();
    let mut schemas: Vec<String> = all.iter().filter_map(|o| o.schema.clone()).collect();
    schemas.sort();
    schemas.dedup();
    let mut v = vec![
        ObjectQuery::default(),
        q(|q| q.not_kinds = declared.clone()),
        q(|q| q.not_kinds = vec![kinds::TABLE.into(), kinds::VIEW.into()]),
        q(|q| q.no_schema = true),
        q(|q| {
            q.no_schema = true;
            q.schema = Some("dbo".into());
            q.kind = Some("fulltext_stoplist".into());
        }),
        q(|q| q.schema = Some("DBO".into())),
        q(|q| q.schema = Some("nope".into())),
        q(|q| q.kind = Some("collection".into())),
        q(|q| {
            q.schema = Some("dbo".into());
            q.pattern = Some("o".into());
        }),
        q(|q| {
            q.kind = Some(kinds::VIEW.into());
            q.pattern = Some("V_1".into());
        }),
        q(|q| {
            q.schema = Some("Big".into());
            q.kind = Some(kinds::VIEW.into());
            q.name = Some("V_07".into());
        }),
    ];
    for k in &declared {
        v.push(q(|q| q.kind = Some(k.clone())));
        v.push(q(|q| {
            q.schema = Some("dbo".into());
            q.kind = Some(k.clone());
        }));
    }
    for s in schemas {
        v.push(q(|q| q.schema = Some(s)));
    }
    for p in [
        "order", "ORDER", "_", "%", "[", "é", "É", "-", "", " ", "a b", "zzz",
    ] {
        v.push(q(|q| q.pattern = Some(p.into())));
    }
    for n in [
        "orders", "ORDERS", "dup", "ÉCLAIR", "cat_a", "seq_b", "nope", "orders ", "",
    ] {
        v.push(q(|q| q.name = Some(n.into())));
    }
    v
}

/// Every query in pages of 7, and the 25 views of `Big` in pages of 10.
async fn check(d: &dyn Driver, s: &mut Box<dyn Session>) {
    let all = s.list_objects().await.expect("list_objects");
    eprintln!("{} objects", all.len());
    let big = q(|q| {
        q.schema = Some("Big".into());
        q.kind = Some(kinds::VIEW.into());
    });
    assert_eq!(page_of(&all, &big).objects.len(), 25, "the big folder");
    assert_eq!(same_pages(s, &all, big, 10).await, 3, "25 views by 10");
    let queries = queries(d, &all);
    let mut pages = 0;
    for q in &queries {
        pages += same_pages(s, &all, q.clone(), 7).await;
    }
    eprintln!(
        "{} queries, {pages} pages of 7: same as page_of",
        queries.len()
    );
}

/// 25 views in `Big`, named in mixed case.
fn big_views() -> String {
    (0..25)
        .map(|i| {
            let name = if i % 2 == 0 {
                format!("V_{i:02}")
            } else {
                format!("v_{i:02}")
            };
            format!("CREATE VIEW Big.{name} AS SELECT {i} AS n;\nGO\n")
        })
        .collect()
}

const SQLSERVER_OBJECTS: &str = "
CREATE SCHEMA Sales;
GO
CREATE SCHEMA [Ünïcode];
GO
CREATE SCHEMA [a b];
GO
CREATE SCHEMA Big;
GO
CREATE TABLE dbo.Orders (id int PRIMARY KEY, total int DEFAULT 0);
CREATE TABLE Sales.Orders (id int PRIMARY KEY, order_id int REFERENCES dbo.Orders (id));
CREATE TABLE dbo.[Éclair] (id int);
CREATE TABLE dbo.eclair2 (id int);
CREATE TABLE dbo._t (id int);
CREATE TABLE dbo.[t-1] (id int);
CREATE TABLE dbo.[100%] (id int);
CREATE TABLE dbo.[ñandú] (id int);
CREATE TABLE dbo.[a[b] (id int);
CREATE TABLE [Ünïcode].Zeta (id int);
CREATE TABLE [a b].[x y] (id int);
CREATE TABLE dbo.Dup (id int);
CREATE TYPE dbo.Dup FROM int;
CREATE TYPE dbo.Phone FROM varchar(20) NOT NULL;
CREATE TYPE Sales.OrderRows AS TABLE (id int);
GO
CREATE VIEW dbo.v_orders AS SELECT id FROM dbo.Orders;
GO
CREATE PROCEDURE dbo.p_one AS SELECT 1;
GO
CREATE PROCEDURE Sales.P_Two AS SELECT 2;
GO
CREATE FUNCTION dbo.f_scalar() RETURNS int AS BEGIN RETURN 1 END;
GO
CREATE FUNCTION dbo.f_inline() RETURNS TABLE AS RETURN SELECT 1 AS one;
GO
CREATE FUNCTION Sales.F_Multi() RETURNS @t TABLE (a int) AS BEGIN INSERT @t VALUES (1); RETURN END;
GO
CREATE TRIGGER dbo.tr_orders ON dbo.Orders AFTER INSERT AS SELECT 1;
GO
CREATE SEQUENCE dbo.seq_a;
CREATE SEQUENCE Sales.Seq_B;
CREATE SYNONYM dbo.syn_orders FOR dbo.Orders;
CREATE FULLTEXT CATALOG Cat_A;
CREATE FULLTEXT STOPLIST cat_a;
CREATE FULLTEXT STOPLIST zz_stop;
GO
";

/// What only a case-sensitive database can hold: names that differ only in
/// case.
const CASE_TWINS: &str = "
CREATE SCHEMA sales;
GO
CREATE TABLE dbo.orders (id int);
CREATE TABLE sales.orders (id int);
GO
CREATE VIEW dbo.V_ORDERS AS SELECT 0 AS n;
";

/// Runs `body` on a session to a new database `db` (collation `collation`)
/// holding the SQL Server test objects, and drops it whatever happens.
async fn on_sqlserver_database(
    d: &dyn Driver,
    cfg: &ConnectionConfig,
    db: &str,
    collation: Option<&str>,
) {
    let mut admin = d.connect(cfg, None).await.expect("connect");
    let drop_sql = format!(
        "IF DB_ID('{db}') IS NOT NULL BEGIN ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]; END"
    );
    run(&mut admin, &drop_sql).await.unwrap();
    let collate = collation.map_or(String::new(), |c| format!(" COLLATE {c}"));
    run(&mut admin, &format!("CREATE DATABASE [{db}]{collate}"))
        .await
        .unwrap();
    let outcome = AssertUnwindSafe(async {
        let mut s = d
            .connect(cfg, Some(db))
            .await
            .expect("connect to the scratch database");
        run(&mut s, &format!("{SQLSERVER_OBJECTS}{}", big_views()))
            .await
            .expect("the test objects");
        if collation.is_some_and(|c| c.contains("_CS_")) {
            run(&mut s, CASE_TWINS).await.expect("the case twins");
        }
        let all = s.list_objects().await.unwrap();
        // Not a vacuous check: every source is there, and nothing else.
        for (kind, schema, name) in [
            ("trigger", Some("dbo"), "tr_orders"),
            ("type", Some("dbo"), "Dup"),
            ("table", Some("dbo"), "Dup"),
            ("sequence", Some("Sales"), "Seq_B"),
            ("synonym", Some("dbo"), "syn_orders"),
            ("fulltext_catalog", None, "Cat_A"),
            ("fulltext_stoplist", None, "cat_a"),
        ] {
            assert!(
                all.iter()
                    .any(|o| o.kind == kind && o.schema.as_deref() == schema && o.name == name),
                "{kind} {name} in {all:?}"
            );
        }
        assert!(all
            .iter()
            .any(|o| o.kind == "trigger" && o.parent.as_deref() == Some("Orders")));
        check(d, &mut s).await;
    })
    .catch_unwind()
    .await;
    run(&mut admin, &drop_sql).await.unwrap();
    if let Err(p) = outcome {
        std::panic::resume_unwind(p);
    }
}

/// SQL Server, in a database with the server's default collation and in a
/// case-sensitive one.
#[tokio::test]
#[ignore]
async fn sqlserver_pages_are_the_whole_lists_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let cfg = parse_url(&url);
    let d = driver("sqlserver");
    on_sqlserver_database(&*d, &cfg, "dbine_page_ci", None).await;
    on_sqlserver_database(&*d, &cfg, "dbine_page_cs", Some("Latin1_General_100_CS_AS")).await;
}

/// The driver the `sqlserver_legacy` host serves, with legacy TLS on
/// (OpenSSL).
#[cfg(feature = "legacy-tls")]
#[tokio::test]
#[ignore]
async fn legacy_tls_pages_are_the_whole_lists_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_SQLSERVER_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_URL not set; skipping");
        return;
    };
    let mut cfg = parse_url(&url);
    for (k, v) in [
        ("Encrypt", "Mandatory"),
        ("LegacyTls", "true"),
        ("TrustServerCertificate", "true"),
    ] {
        cfg.options.insert(k.into(), v.into());
    }
    let d = dbine_driver_sqlserver::legacy_tls_drivers().remove(0);
    on_sqlserver_database(&*d, &cfg, "dbine_page_legacy", None).await;
}

/// Without the table and type of the same name: on Babelfish a table is a
/// PostgreSQL type too.
const BABELFISH_OBJECTS: &str = "
CREATE SCHEMA Sales;
GO
CREATE SCHEMA Big;
GO
CREATE TABLE dbo.Orders (id int PRIMARY KEY, total int DEFAULT 0);
CREATE TABLE Sales.Orders (id int PRIMARY KEY);
CREATE TABLE dbo.[Éclair] (id int);
CREATE TABLE dbo.eclair2 (id int);
CREATE TABLE dbo._t (id int);
CREATE TABLE dbo.[t-1] (id int);
CREATE TABLE dbo.[ñandú] (id int);
CREATE TYPE dbo.Phone FROM varchar(20) NOT NULL;
CREATE TYPE Sales.OrderRows AS TABLE (id int);
GO
CREATE VIEW dbo.v_orders AS SELECT id FROM dbo.Orders;
GO
CREATE PROCEDURE dbo.p_one AS SELECT 1;
GO
CREATE PROCEDURE Sales.P_Two AS SELECT 2;
GO
CREATE FUNCTION dbo.f_scalar() RETURNS int AS BEGIN RETURN 1 END;
GO
CREATE FUNCTION dbo.f_inline() RETURNS TABLE AS RETURN SELECT 1 AS one;
GO
CREATE TRIGGER dbo.tr_orders ON dbo.Orders AFTER INSERT AS SELECT 1;
GO
";

/// Babelfish, in a database of its own, and in `master`, whose overloaded
/// `xp_instance_regread` procedures tie on schema and name.
#[tokio::test]
#[ignore]
async fn babelfish_pages_are_the_whole_lists_pages() {
    let Ok(url) = std::env::var("DBINE_TEST_BABELFISH_URL") else {
        eprintln!("DBINE_TEST_BABELFISH_URL not set; skipping");
        return;
    };
    let mut cfg = parse_url(&url);
    cfg.driver = "babelfish".into();
    let d = driver("babelfish");
    let db = "dbine_page_bbf";
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    if admin
        .list_databases()
        .await
        .unwrap()
        .iter()
        .any(|n| n == db)
    {
        admin.drop_database(db).await.unwrap();
    }
    admin.create_database(db).await.expect("create database");
    let outcome = AssertUnwindSafe(async {
        let mut s = d.connect(&cfg, Some(db)).await.unwrap();
        run(&mut s, &format!("{BABELFISH_OBJECTS}{}", big_views()))
            .await
            .expect("the test objects");
        check(&*d, &mut s).await;
        drop(s);
        // master: pages of 1 so the twins straddle a page.
        let all = admin.list_objects().await.unwrap();
        let procedures = q(|q| q.kind = Some(kinds::PROCEDURE.into()));
        let pages = same_pages(&mut admin, &all, procedures, 1).await;
        eprintln!("master: {pages} procedures, one per page");
        for q in queries(&*d, &all) {
            same_pages(&mut admin, &all, q, 2).await;
        }
    })
    .catch_unwind()
    .await;
    admin.drop_database(db).await.unwrap();
    if let Err(p) = outcome {
        std::panic::resume_unwind(p);
    }
}
