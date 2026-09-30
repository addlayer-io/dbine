//! "Comparar esquemas" against a real server: two keyspaces whose table
//! differs in its indexes (secondary, on a set's values, SAI with options)
//! and whose user types and materialized view only one side has. The sync
//! script and the objects' definitions are run on the target keyspace and
//! both must then read the same.
//!
//! ```sh
//! DBINE_TEST_CASSANDRA_URL=localhost:25402 \
//!   cargo test -p dbine-driver-cassandra --test compare -- --ignored --nocapture
//! DBINE_TEST_SCYLLADB_URL=localhost:25403 …   # the same on ScyllaDB (no SAI)
//! ```

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange, TableSchema};

fn cfg(driver: &str, url: &str) -> ConnectionConfig {
    let (host, port) = url.rsplit_once(':').unwrap();
    ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().unwrap(), ..Default::default() }
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(text, 10, &mut out).await.map_err(|e| format!("{text}: {e}"))?;
    out.error.map_or(Ok(()), |e| Err(format!("{text}: {e}")))
}

fn docs(schema: &[TableSchema]) -> TableSchema {
    let mut t = schema.iter().find(|t| t.name == "docs").expect("docs").clone();
    t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
    t
}

/// Code objects (types, views) with their definitions.
async fn code(s: &mut Box<dyn Session>) -> Vec<(String, String, String)> {
    let mut v = Vec::new();
    for o in s.list_objects().await.unwrap() {
        if o.kind == kinds::TYPE || o.kind == kinds::MATERIALIZED_VIEW {
            let r = ObjectRef { kind: o.kind.clone(), schema: None, name: o.name.clone() };
            v.push((o.kind, o.name, s.definition(&r).await.unwrap().unwrap()));
        }
    }
    v.sort();
    v
}

async fn compare_and_sync(driver: &str, url: &str) {
    let d = dbine_driver_cassandra::drivers().into_iter().find(|d| d.info().id == driver).unwrap();
    let mut admin = d.connect(&cfg(driver, url), None).await.unwrap();
    for ks in ["dbine_cmp_src", "dbine_cmp_dst"] {
        run(&mut admin, &format!("DROP KEYSPACE IF EXISTS {ks}")).await.unwrap();
        run(&mut admin, &format!("CREATE KEYSPACE {ks} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}")).await.unwrap();
    }
    let mut a = d.connect(&cfg(driver, url), Some("dbine_cmp_src")).await.unwrap();
    let mut b = d.connect(&cfg(driver, url), Some("dbine_cmp_dst")).await.unwrap();
    let table = "CREATE TYPE addr (street text, zip int);
        CREATE TABLE docs (id int PRIMARY KEY, name text, email text, tags set<text>, home frozen<addr>);";
    run(&mut a, table).await.unwrap();
    run(&mut b, table).await.unwrap();
    run(
        &mut a,
        "CREATE TYPE person (name text, home frozen<addr>);
         CREATE INDEX docs_email ON docs (email);
         CREATE INDEX docs_tags ON docs (values(tags));",
    )
    .await
    .unwrap();
    run(&mut b, "CREATE INDEX docs_email ON docs (name);").await.unwrap();
    let sai = driver == "cassandra";
    if sai {
        run(&mut a, "CREATE INDEX docs_name ON docs (name) USING 'sai' WITH OPTIONS = {'case_sensitive': 'false', 'normalize': 'true'};").await.unwrap();
        run(&mut b, "CREATE INDEX docs_name ON docs (name) USING 'sai';").await.unwrap();
    }
    // Materialized views may be switched off (Cassandra 5's default).
    let mv = run(
        &mut a,
        "CREATE MATERIALIZED VIEW docs_by_email AS SELECT id, email FROM docs WHERE email IS NOT NULL AND id IS NOT NULL PRIMARY KEY (email, id)",
    )
    .await
    .map_err(|e| println!("sin vistas materializadas: {e}"))
    .is_ok();

    let src = docs(&a.database_schema().await.unwrap());
    let dst = docs(&b.database_schema().await.unwrap());
    println!("{:#?}", src.indexes);
    if sai {
        let n = src.indexes.iter().find(|i| i.name == "docs_name").unwrap();
        assert_eq!(n.kind.as_deref(), Some("sai"));
        assert_eq!(n.options.get("case_sensitive").map(String::as_str), Some("false"));
    }
    assert_ne!(src.indexes, dst.indexes);
    let src_code = code(&mut a).await;
    assert!(src_code.iter().all(|(_, _, def)| !def.contains("dbine_cmp_src")), "{src_code:#?}");
    assert_eq!(src_code.len(), if mv { 3 } else { 2 }, "{src_code:#?}");

    // The UI carries the source's items into the target's table (its keyspace).
    let src = TableSchema { schema: dst.schema.clone(), ..src };
    let script = d.sync_script(&[TableChange::Alter { old: dst, new: src.clone() }]).unwrap();
    println!("{script:#?}");
    for st in &script.statements {
        run(&mut b, st).await.unwrap();
    }
    // What the compare creates: the objects the target lacks.
    let dst_code = code(&mut b).await;
    for (kind, name, def) in &src_code {
        if !dst_code.iter().any(|(k, n, _)| k == kind && n == name) {
            run(&mut b, def).await.unwrap();
        }
    }

    let after = docs(&b.database_schema().await.unwrap());
    assert_eq!(after.indexes, src.indexes);
    assert_eq!(code(&mut b).await, src_code);
    let again = d.sync_script(&[TableChange::Alter { old: after, new: src }]).unwrap();
    assert!(again.statements.is_empty(), "{again:#?}");

    for ks in ["dbine_cmp_src", "dbine_cmp_dst"] {
        run(&mut admin, &format!("DROP KEYSPACE {ks}")).await.unwrap();
    }
}

#[tokio::test]
#[ignore]
async fn cassandra_compare() {
    let Ok(url) = std::env::var("DBINE_TEST_CASSANDRA_URL") else { return };
    compare_and_sync("cassandra", &url).await;
}

#[tokio::test]
#[ignore]
async fn scylladb_compare() {
    let Ok(url) = std::env::var("DBINE_TEST_SCYLLADB_URL") else { return };
    compare_and_sync("scylladb", &url).await;
}
