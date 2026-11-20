//! "Ver dependencias…", what an object uses (`Session::references`),
//! against real servers:
//! `DBINE_TEST_CASSANDRA_URL=localhost:25402` (cassandra:5) and
//! `DBINE_TEST_SCYLLADB_URL=localhost:25413`, then
//! `cargo test -p dbine-driver-cassandra --test references -- --ignored --nocapture`.
//!
//! Fixture, in a keyspace of its own (`dbine_refs_<pid>`): types
//! `direccion` and `persona` (which uses `direccion`); `clientes` with a
//! column of each; `simple`, which uses none; and, where the server allows
//! them, a materialized view on `clientes` and an aggregate with its state
//! and final functions (off by default on Cassandra 5; ScyllaDB has
//! materialized views on keyspaces without tablets, and UDFs only when
//! enabled). The keyspace is dropped at the end, also when an assertion
//! fails.

use dbine_driver::{
    kinds, Confidence, ConnectionConfig, DependencyScan, DependencyTarget, Driver, ObjectRef,
    QueryOutcome, Session,
};
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

async fn open(driver: &str, url: &str) -> (Arc<dyn Driver>, Box<dyn Session>) {
    let (host, port) = url.rsplit_once(':').unwrap();
    let cfg = ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().unwrap(),
        ..Default::default()
    };
    let d = dbine_driver_cassandra::drivers()
        .into_iter()
        .find(|d| d.info().id == driver)
        .unwrap();
    let s = d.connect(&cfg, None).await.unwrap();
    (d, s)
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> dbine_driver::Result<()> {
    let mut out = QueryOutcome::default();
    s.execute(text, 10, &mut out).await?;
    match out.error {
        Some(e) => Err(dbine_driver::Error::Query(format!("{e:?}"))),
        None => Ok(()),
    }
}

fn target(kind: &str, name: &str) -> DependencyTarget {
    DependencyTarget {
        object: ObjectRef {
            kind: kind.into(),
            schema: None,
            name: name.into(),
        },
        column: None,
    }
}

/// `(kind, name, detail)` of what `name` uses, all confirmed.
async fn uses(
    d: &Arc<dyn Driver>,
    s: &mut Box<dyn Session>,
    kind: &str,
    name: &str,
) -> Vec<(String, String, Option<String>)> {
    let scan = DependencyScan::new(d.info(), d.script_dialect(), false);
    let r = s.references(&target(kind, name), &scan).await.unwrap();
    println!("{name}: {:?} note={:?}", r.items, r.note);
    assert!(r.note.is_none(), "{:?}", r.note);
    assert!(
        r.items
            .iter()
            .all(|i| i.confidence == Confidence::Confirmed),
        "{:?}",
        r.items
    );
    r.items
        .into_iter()
        .map(|i| (i.kind, i.name, i.detail))
        .collect()
}

fn item(kind: &str, name: &str, detail: Option<&str>) -> (String, String, Option<String>) {
    (kind.into(), name.into(), detail.map(Into::into))
}

async fn checks(d: &Arc<dyn Driver>, s: &mut Box<dyn Session>, lang: &str) {
    run(s, "CREATE TYPE direccion (calle text, ciudad text)")
        .await
        .unwrap();
    run(
        s,
        "CREATE TYPE persona (nombre text, casa frozen<direccion>)",
    )
    .await
    .unwrap();
    run(
        s,
        "CREATE TABLE clientes (id int PRIMARY KEY, nombre text, casa frozen<direccion>, familia list<frozen<persona>>)",
    )
    .await
    .unwrap();
    run(s, "CREATE TABLE simple (id int PRIMARY KEY, v int)")
        .await
        .unwrap();

    assert_eq!(
        uses(d, s, kinds::TABLE, "clientes").await,
        [
            item("type", "direccion", Some("casa")),
            item("type", "persona", Some("familia")),
        ]
    );
    assert_eq!(
        uses(d, s, "type", "persona").await,
        [item("type", "direccion", Some("casa"))]
    );
    assert!(uses(d, s, kinds::TABLE, "simple").await.is_empty());
    // The function and aggregate queries run against this server's
    // system_schema (nothing to find: no note, no error).
    assert!(uses(d, s, kinds::FUNCTION, "no_existe").await.is_empty());

    let mv = run(
        s,
        "CREATE MATERIALIZED VIEW clientes_por_nombre AS SELECT id, nombre FROM clientes \
         WHERE nombre IS NOT NULL AND id IS NOT NULL PRIMARY KEY (nombre, id)",
    )
    .await;
    match mv {
        Ok(()) => assert_eq!(
            uses(d, s, kinds::MATERIALIZED_VIEW, "clientes_por_nombre").await,
            [item(kinds::TABLE, "clientes", None)]
        ),
        Err(e) => println!("materialized view not checked: {e}"),
    }

    let (sfunc, ffunc) = if lang == "java" {
        (
            "CREATE FUNCTION acumular(estado tuple<int, bigint>, v int) CALLED ON NULL INPUT \
             RETURNS tuple<int, bigint> LANGUAGE java AS \
             'if (v != null) { estado.setInt(0, estado.getInt(0) + 1); estado.setLong(1, estado.getLong(1) + v); } return estado;'",
            "CREATE FUNCTION promedio(estado tuple<int, bigint>) CALLED ON NULL INPUT \
             RETURNS double LANGUAGE java AS \
             'return estado.getInt(0) == 0 ? null : Double.valueOf(estado.getLong(1)) / estado.getInt(0);'",
        )
    } else {
        (
            "CREATE FUNCTION acumular(estado tuple<int, bigint>, v int) CALLED ON NULL INPUT \
             RETURNS tuple<int, bigint> LANGUAGE lua AS \
             'if v == nil then return estado end return {estado[1] + 1, estado[2] + v}'",
            "CREATE FUNCTION promedio(estado tuple<int, bigint>) CALLED ON NULL INPUT \
             RETURNS double LANGUAGE lua AS 'if estado[1] == 0 then return nil end return estado[2] / estado[1]'",
        )
    };
    let udf = match run(s, sfunc).await {
        Ok(()) => run(s, ffunc).await,
        Err(e) => Err(e),
    };
    let uda = match udf {
        Ok(()) => {
            run(
                s,
                "CREATE AGGREGATE prom(int) SFUNC acumular STYPE tuple<int, bigint> FINALFUNC promedio INITCOND (0, 0)",
            )
            .await
        }
        Err(e) => Err(e),
    };
    match uda {
        Ok(()) => assert_eq!(
            uses(d, s, "aggregate", "prom").await,
            [
                item(kinds::FUNCTION, "acumular", Some("SFUNC")),
                item(kinds::FUNCTION, "promedio", Some("FINALFUNC")),
            ]
        ),
        Err(e) => println!("aggregate not checked: {e}"),
    }
}

async fn live(driver: &str, env: &str, keyspace_options: &str, lang: &str) {
    let Ok(url) = std::env::var(env) else {
        return;
    };
    let (d, mut s) = open(driver, &url).await;
    let ks = format!("dbine_refs_{}", std::process::id());
    run(
        &mut s,
        &format!(
            "CREATE KEYSPACE {ks} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}{keyspace_options}"
        ),
    )
    .await
    .unwrap();
    run(&mut s, &format!("USE {ks}")).await.unwrap();
    let outcome = AssertUnwindSafe(checks(&d, &mut s, lang))
        .catch_unwind()
        .await;
    run(&mut s, &format!("DROP KEYSPACE {ks}")).await.unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
#[ignore]
async fn references_cassandra_live() {
    live("cassandra", "DBINE_TEST_CASSANDRA_URL", "", "java").await;
}

#[tokio::test]
#[ignore]
async fn references_scylladb_live() {
    live(
        "scylladb",
        "DBINE_TEST_SCYLLADB_URL",
        " AND tablets = {'enabled': false}",
        "lua",
    )
    .await;
}
