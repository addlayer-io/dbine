//! What an object uses (`Session::references`) through the password test
//! hook against a plain PostgreSQL 16 (DSQL has no emulator), with DSQL's
//! subset: no foreign keys, triggers or other databases. A scratch schema
//! with a unique name is dropped at the end, also when a check fails.
//!   docker run -d --name dbine-test-dsqlpg -e POSTGRES_PASSWORD=dbine -p 25301:5432 postgres:16
//!   DBINE_TEST_DSQL_URL=localhost:25301 cargo test -p dbine-driver-dsql --test references -- --ignored --nocapture
//! (user postgres, password dbine).

use dbine_driver::{
    Confidence, ConnectionConfig, DependencyReport, DependencyScan, DependencyTarget, Dependent,
    ObjectRef, QueryOutcome, Relation, Session,
};
use futures::FutureExt;
use std::panic::AssertUnwindSafe;

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

async fn refs(s: &mut Box<dyn Session>, kind: &str, schema: &str, name: &str) -> DependencyReport {
    let d = dbine_driver_dsql::drivers().pop().unwrap();
    let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
    let target = DependencyTarget {
        object: ObjectRef {
            kind: kind.into(),
            schema: Some(schema.into()),
            name: name.into(),
        },
        column: None,
    };
    let r = s
        .references(&target, &scan)
        .await
        .unwrap_or_else(|e| panic!("{kind} {name}: {e}"));
    eprintln!("{kind} {schema}.{name}:");
    for i in &r.items {
        eprintln!(
            "  {:?} {:?} {} {}.{} {:?} ({} lines)",
            i.confidence,
            i.relation,
            i.kind,
            i.schema.as_deref().unwrap_or(""),
            i.name,
            i.detail,
            i.mentions.len()
        );
    }
    assert!(r.note.is_none(), "{:?}", r.note);
    r
}

fn one<'a>(
    r: &'a DependencyReport,
    kind: &str,
    name: &str,
    relation: Relation,
    confidence: Confidence,
) -> &'a Dependent {
    let found: Vec<_> = r
        .items
        .iter()
        .filter(|i| i.kind == kind && i.name == name && i.relation == relation)
        .collect();
    assert_eq!(found.len(), 1, "{kind} {name} {relation:?}: {:?}", r.items);
    assert_eq!(found[0].confidence, confidence, "{kind} {name}");
    found[0]
}

async fn checks(s: &mut Box<dyn Session>, sch: &str) {
    run(s, &format!("CREATE SCHEMA {sch}")).await;
    for stmt in [
        "CREATE TABLE {s}.clientes (id int PRIMARY KEY, nombre text)",
        "CREATE SEQUENCE {s}.seq_ped",
        "CREATE FUNCTION {s}.positivo(x int) RETURNS boolean LANGUAGE sql IMMUTABLE RETURN x > 0",
        "CREATE TABLE {s}.pedidos (id int PRIMARY KEY DEFAULT nextval('{s}.seq_ped'),
            cliente_id int, total numeric CHECK ({s}.positivo(total::int)))",
        "CREATE VIEW {s}.v_pedidos AS
           SELECT p.id, c.nombre, {s}.positivo(p.id) AS ok
           FROM {s}.pedidos p JOIN {s}.clientes c ON c.id = p.cliente_id",
        "CREATE FUNCTION {s}.contar() RETURNS bigint LANGUAGE sql BEGIN ATOMIC SELECT count(*) FROM {s}.v_pedidos; END",
        "CREATE FUNCTION {s}.contar_texto() RETURNS bigint LANGUAGE sql AS 'SELECT count(*) FROM {s}.clientes'",
    ] {
        run(s, &stmt.replace("{s}", sch)).await;
    }

    let r = refs(s, "view", sch, "v_pedidos").await;
    assert_eq!(r.items.len(), 3, "{:?}", r.items);
    let p = one(
        &r,
        "table",
        "pedidos",
        Relation::Code,
        Confidence::Confirmed,
    );
    assert!(!p.mentions.is_empty());
    one(
        &r,
        "table",
        "clientes",
        Relation::Code,
        Confidence::Confirmed,
    );
    one(
        &r,
        "function",
        "positivo",
        Relation::Code,
        Confidence::Confirmed,
    );

    let r = refs(s, "function", sch, "contar").await;
    assert_eq!(r.items.len(), 1, "{:?}", r.items);
    one(
        &r,
        "view",
        "v_pedidos",
        Relation::Code,
        Confidence::Confirmed,
    );

    // A string body: only its text says what it reads.
    let r = refs(s, "function", sch, "contar_texto").await;
    one(
        &r,
        "table",
        "clientes",
        Relation::Code,
        Confidence::Probable,
    );

    let r = refs(s, "table", sch, "pedidos").await;
    let seq = one(
        &r,
        "sequence",
        "seq_ped",
        Relation::Code,
        Confidence::Confirmed,
    );
    assert!(seq
        .detail
        .as_deref()
        .is_some_and(|d| d.starts_with("id DEFAULT nextval(")));
    one(
        &r,
        "function",
        "positivo",
        Relation::Check,
        Confidence::Confirmed,
    );
}

#[tokio::test]
#[ignore]
async fn dsql_references() {
    let Ok(url) = std::env::var("DBINE_TEST_DSQL_URL") else {
        return;
    };
    let (host, port) = url.split_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: "dsql".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some("postgres".into()),
        ..Default::default()
    };
    let mut s = dbine_driver_dsql::connect_with_password(&cfg, "dbine")
        .await
        .unwrap();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let sch = format!("dbine_refs_{}_{nanos}", std::process::id());
    let r = AssertUnwindSafe(checks(&mut s, &sch)).catch_unwind().await;
    let mut out = QueryOutcome::default();
    if let Err(e) = s
        .execute(
            &format!("DROP SCHEMA IF EXISTS {sch} CASCADE"),
            10,
            &mut out,
        )
        .await
    {
        eprintln!("cleanup: {e}");
    }
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}
