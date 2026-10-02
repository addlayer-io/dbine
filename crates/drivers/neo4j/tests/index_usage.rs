//! Index usage against real servers (same containers as `integration.rs`):
//! a label with a KEY constraint (its primary key), two indexes, five
//! lookups through one of them and none through the other. Then "Eliminar
//! índice…": the schema sync script without the unread index, run.
//!
//! ```sh
//! DBINE_TEST_NEO4J_URL=neo4j:dbine-test-pass@localhost:17687 DBINE_TEST_MEMGRAPH_URL=localhost:27687 \
//!   cargo test -p dbine-driver-neo4j --test index_usage -- --ignored --nocapture
//! ```
//!
//! `DBINE_TEST_NEO4J_EE_URL` (Enterprise, admin login): a user without the
//! SHOW INDEX privilege gets the constraints' indexes and a note.

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange};

fn cfg(driver: &str, url: &str) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').map_or((None, url), |(a, h)| (Some(a), h));
    let (host, port) = hp.rsplit_once(':').unwrap();
    let (user, pass) = auth.and_then(|a| a.split_once(':')).map_or((None, None), |(u, p)| (Some(u.to_string()), Some(p.to_string())));
    ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().unwrap(), username: user, password: pass, ..Default::default() }
}

async fn run(s: &mut Box<dyn Session>, q: &str) {
    s.execute(q, 100, &mut QueryOutcome::default()).await.unwrap_or_else(|e| panic!("{q}: {e}"));
}

async fn quiet(s: &mut Box<dyn Session>, q: &str) {
    let _ = s.execute(q, 100, &mut QueryOutcome::default()).await;
}

fn label() -> ObjectRef {
    ObjectRef { kind: "label".into(), schema: None, name: "IxPedido".into() }
}

#[tokio::test]
#[ignore]
async fn index_usage_neo4j_live() {
    let Ok(url) = std::env::var("DBINE_TEST_NEO4J_URL") else { return };
    let d = dbine_driver_neo4j::drivers().into_iter().find(|d| d.info().id == "neo4j").unwrap();
    assert!(d.supports_index_usage());
    let mut s = d.connect(&cfg("neo4j", &url), None).await.unwrap();
    for q in ["DROP CONSTRAINT ix_pk IF EXISTS", "DROP INDEX ix_cliente IF EXISTS", "DROP INDEX ix_fecha IF EXISTS", "MATCH (n:IxPedido) DETACH DELETE n"] {
        quiet(&mut s, q).await;
    }
    // A KEY constraint needs Enterprise; Community gets a UNIQUE one.
    let key = s.execute("CREATE CONSTRAINT ix_pk FOR (p:IxPedido) REQUIRE p.id IS NODE KEY", 10, &mut QueryOutcome::default()).await.is_ok();
    if !key {
        run(&mut s, "CREATE CONSTRAINT ix_pk FOR (p:IxPedido) REQUIRE p.id IS UNIQUE").await;
    }
    run(&mut s, "CREATE INDEX ix_cliente FOR (p:IxPedido) ON (p.cliente)").await;
    run(&mut s, "CREATE INDEX ix_fecha FOR (p:IxPedido) ON (p.fecha)").await;
    run(&mut s, "CALL db.awaitIndexes(60)").await;
    run(&mut s, "UNWIND range(1, 50) AS i CREATE (:IxPedido {id: i, cliente: 'c' + toString(i % 5), fecha: i})").await;
    for c in ["c1", "c2", "c3", "c1", "c2"] {
        run(&mut s, &format!("MATCH (p:IxPedido) USING INDEX p:IxPedido(cliente) WHERE p.cliente = '{c}' RETURN count(p)")).await;
    }
    // Neo4j flushes the counters every few seconds.
    let mut r = s.index_usage(&label()).await.unwrap().expect("report").derived();
    for _ in 0..30 {
        if r.indexes.iter().any(|i| i.name == "ix_cliente" && i.seeks >= 5) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        r = s.index_usage(&label()).await.unwrap().expect("report").derived();
    }
    println!("{r:#?}");
    assert!(r.stats_available && !r.seek_scan_split);
    assert!(r.since.is_some());
    let get = |n: &str| r.indexes.iter().find(|i| i.name == n).unwrap_or_else(|| panic!("{n}"));
    assert_eq!(get("ix_pk").primary_key, key);
    assert!(get("ix_pk").unique);
    assert!(get("ix_cliente").seeks >= 5, "{}", get("ix_cliente").seeks);
    assert!(get("ix_cliente").last_read.is_some());
    assert_eq!(get("ix_fecha").seeks, 0);
    assert_eq!(get("ix_fecha").read_share, Some(0.0));
    assert_eq!(get("ix_fecha").key_columns, ["fecha"]);
    // The schema compare asks for a "table".
    let t = ObjectRef { kind: "table".into(), ..label() };
    assert_eq!(s.index_usage(&t).await.unwrap().unwrap().indexes.len(), 3);

    let table = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "IxPedido").unwrap();
    let mut without = table.clone();
    without.indexes.retain(|i| i.name != "ix_fecha");
    let script = d.sync_script(&[TableChange::Alter { old: table, new: without }]).unwrap();
    println!("{script:#?}");
    assert_eq!(script.statements.len(), 1);
    run(&mut s, &script.statements[0]).await;
    let r = s.index_usage(&label()).await.unwrap().unwrap();
    assert!(r.indexes.iter().all(|i| i.name != "ix_fecha"));
    assert!(r.indexes.iter().any(|i| i.name == "ix_cliente"));
    for q in ["DROP CONSTRAINT ix_pk IF EXISTS", "DROP INDEX ix_cliente IF EXISTS", "MATCH (n:IxPedido) DETACH DELETE n"] {
        quiet(&mut s, q).await;
    }
}

#[tokio::test]
#[ignore]
async fn index_usage_memgraph_live() {
    let Ok(url) = std::env::var("DBINE_TEST_MEMGRAPH_URL") else { return };
    let d = dbine_driver_neo4j::drivers().into_iter().find(|d| d.info().id == "memgraph").unwrap();
    let mut s = d.connect(&cfg("memgraph", &url), None).await.unwrap();
    for q in ["DROP CONSTRAINT ON (p:IxPedido) ASSERT p.id IS UNIQUE", "DROP INDEX ON :IxPedido(cliente)", "DROP INDEX ON :IxPedido(fecha)", "MATCH (n:IxPedido) DETACH DELETE n"] {
        quiet(&mut s, q).await;
    }
    run(&mut s, "CREATE CONSTRAINT ON (p:IxPedido) ASSERT p.id IS UNIQUE").await;
    run(&mut s, "CREATE INDEX ON :IxPedido(cliente)").await;
    run(&mut s, "CREATE INDEX ON :IxPedido(fecha)").await;
    run(&mut s, "CREATE (:IxPedido {id: 1, cliente: 'a', fecha: 1})").await;
    let r = s.index_usage(&label()).await.unwrap().expect("report").derived();
    println!("{r:#?}");
    assert!(!r.stats_available && r.note.is_some());
    assert_eq!(r.indexes.len(), 3);
    assert!(r.indexes.iter().any(|i| i.unique && i.key_columns == ["id"]));

    let table = s.database_schema().await.unwrap().into_iter().find(|t| t.name == "IxPedido").unwrap();
    let mut without = table.clone();
    without.indexes.retain(|i| i.columns != ["fecha"]);
    let script = d.sync_script(&[TableChange::Alter { old: table, new: without }]).unwrap();
    println!("{script:#?}");
    for q in &script.statements {
        run(&mut s, q).await;
    }
    let r = s.index_usage(&label()).await.unwrap().unwrap();
    assert_eq!(r.indexes.len(), 2);
    for q in ["DROP CONSTRAINT ON (p:IxPedido) ASSERT p.id IS UNIQUE", "DROP INDEX ON :IxPedido(cliente)", "MATCH (n:IxPedido) DETACH DELETE n"] {
        quiet(&mut s, q).await;
    }
}

/// Enterprise RBAC: `SHOW INDEXES` refused, the constraints' indexes listed
/// without counters and a note naming the privilege.
#[tokio::test]
#[ignore]
async fn index_usage_without_show_index_privilege() {
    let Ok(url) = std::env::var("DBINE_TEST_NEO4J_EE_URL") else { return };
    let d = dbine_driver_neo4j::drivers().into_iter().find(|d| d.info().id == "neo4j").unwrap();
    let mut admin = d.connect(&cfg("neo4j", &url), None).await.unwrap();
    for q in ["DROP CONSTRAINT ixr_pk IF EXISTS", "DROP INDEX ixr_nombre IF EXISTS"] {
        quiet(&mut admin, q).await;
    }
    run(&mut admin, "CREATE CONSTRAINT ixr_pk FOR (p:IxRbac) REQUIRE p.id IS NODE KEY").await;
    run(&mut admin, "CREATE INDEX ixr_nombre FOR (p:IxRbac) ON (p.nombre)").await;
    for q in ["DROP USER ixr_user IF EXISTS", "DROP ROLE ixr_role IF EXISTS"] {
        quiet(&mut admin, q).await;
    }
    for q in [
        "CREATE USER ixr_user SET PASSWORD 'ixr-pass-123' CHANGE NOT REQUIRED",
        "CREATE ROLE ixr_role",
        "GRANT ACCESS ON DATABASE neo4j TO ixr_role",
        "GRANT SHOW CONSTRAINT ON DATABASE neo4j TO ixr_role",
        "GRANT ROLE ixr_role TO ixr_user",
    ] {
        run(&mut admin, q).await;
    }
    let (_, hp) = url.rsplit_once('@').unwrap();
    let mut s = d.connect(&cfg("neo4j", &format!("ixr_user:ixr-pass-123@{hp}")), None).await.unwrap();
    let r = s.index_usage(&ObjectRef { kind: "label".into(), schema: None, name: "IxRbac".into() }).await.unwrap().expect("report");
    println!("{r:#?}");
    assert!(!r.stats_available);
    assert!(r.note.as_deref().is_some_and(|n| n.contains("SHOW INDEX")), "{:?}", r.note);
    assert_eq!(r.indexes.len(), 1);
    assert!(r.indexes[0].primary_key && r.indexes[0].name == "ixr_pk");
    for q in ["DROP USER ixr_user IF EXISTS", "DROP ROLE ixr_role IF EXISTS"] {
        quiet(&mut admin, q).await;
    }
    for q in ["DROP CONSTRAINT ixr_pk IF EXISTS", "DROP INDEX ixr_nombre IF EXISTS"] {
        quiet(&mut admin, q).await;
    }
}
