//! Row estimates and object comments against real servers: the counts
//! come from the statistics `ANALYZE` / `CREATE STATISTICS` leave, and a
//! table nobody analyzed has none. Each test reads
//! `DBINE_TEST_<ENGINE>_URL` and is skipped without it:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//!   cargo test -p dbine-driver-postgres --test stats -- --ignored --test-threads=1
//! ```

use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::{ConnectionConfig, Driver, QueryOutcome, Session};
use std::sync::Arc;

const DB: &str = "dbine_stats";

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

fn rows_of(est: &[RowEstimate], kind: &str, name: &str) -> Option<u64> {
    est.iter().find(|e| e.object.kind == kind && e.object.name == name && e.object.schema.as_deref() == Some("public")).map(|e| e.rows)
}

fn comment_of<'a>(c: &'a [ObjectComment], kind: &str, name: &str) -> Option<&'a str> {
    c.iter()
        .find(|c| c.object.kind == kind && c.object.name == name && c.object.schema.as_deref() == Some("public"))
        .map(|c| c.comment.as_str())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn postgres_reads_planner_stats_and_comments() {
    let Some(cfg) = cfg("postgres", "DBINE_TEST_POSTGRES_URL") else {
        eprintln!("DBINE_TEST_POSTGRES_URL not set; skipping");
        return;
    };
    let d = driver("postgres");
    let mut m = d.connect(&cfg, None).await.unwrap();
    let _ = m.drop_database(DB).await;
    m.create_database(DB).await.unwrap();
    let mut s = d.connect(&cfg, Some(DB)).await.unwrap();
    for sql in [
        "CREATE TABLE clientes (id int PRIMARY KEY, nombre text)",
        "INSERT INTO clientes SELECT g, 'c' || g FROM generate_series(1, 1000) g",
        "CREATE TABLE sin_analizar (id int)",
        "CREATE VIEW v_clientes AS SELECT * FROM clientes",
        "CREATE MATERIALIZED VIEW mv_clientes AS SELECT * FROM clientes WHERE id <= 300",
        "CREATE SEQUENCE seq_pedidos",
        "CREATE TYPE estado AS ENUM ('a', 'b')",
        "CREATE FUNCTION doble(x int) RETURNS int LANGUAGE sql AS 'SELECT x * 2'",
        "CREATE PROCEDURE limpiar() LANGUAGE sql AS 'SELECT 1'",
        "CREATE FUNCTION tg() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RETURN NEW; END'",
        "CREATE TRIGGER tg_clientes BEFORE INSERT ON clientes FOR EACH ROW EXECUTE FUNCTION tg()",
        "COMMENT ON VIEW v_clientes IS 'Vista de clientes'",
        "COMMENT ON MATERIALIZED VIEW mv_clientes IS 'Primeros 300'",
        "COMMENT ON SEQUENCE seq_pedidos IS 'Números de pedido'",
        "COMMENT ON TYPE estado IS 'Estado del pedido'",
        "COMMENT ON FUNCTION doble(int) IS 'El doble'",
        "COMMENT ON PROCEDURE limpiar() IS 'Limpia'",
        "COMMENT ON TRIGGER tg_clientes ON clientes IS 'Antes de insertar'",
        "COMMENT ON TABLE clientes IS 'Tabla: viene con el esquema'",
        "ANALYZE clientes",
        "ANALYZE mv_clientes",
    ] {
        run(&mut s, sql).await;
    }

    let est = s.row_estimates().await.unwrap();
    eprintln!("{est:?}");
    assert_eq!(rows_of(&est, "table", "clientes"), Some(1000));
    assert_eq!(rows_of(&est, "materialized_view", "mv_clientes"), Some(300));
    assert_eq!(rows_of(&est, "table", "sin_analizar"), None, "never analyzed: no estimate");

    let c = s.object_comments().await.unwrap();
    eprintln!("{c:?}");
    assert_eq!(comment_of(&c, "view", "v_clientes"), Some("Vista de clientes"));
    assert_eq!(comment_of(&c, "materialized_view", "mv_clientes"), Some("Primeros 300"));
    assert_eq!(comment_of(&c, "sequence", "seq_pedidos"), Some("Números de pedido"));
    assert_eq!(comment_of(&c, "type", "estado"), Some("Estado del pedido"));
    assert_eq!(comment_of(&c, "function", "doble"), Some("El doble"));
    assert_eq!(comment_of(&c, "procedure", "limpiar"), Some("Limpia"));
    assert_eq!(comment_of(&c, "trigger", "tg_clientes"), Some("Antes de insertar"));
    assert!(c.iter().all(|c| c.object.kind != "table"), "tables come with the schema");

    // Every commented object is one the explorer lists, with the same kind.
    let listed = s.list_objects().await.unwrap();
    for c in &c {
        assert!(
            listed.iter().any(|o| o.kind == c.object.kind && o.schema == c.object.schema && o.name == c.object.name),
            "{:?} not in list_objects",
            c.object
        );
    }
    drop(s);
    m.drop_database(DB).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cockroach_reads_table_statistics_and_comments() {
    let Some(cfg) = cfg("cockroachdb", "DBINE_TEST_COCKROACH_URL") else {
        eprintln!("DBINE_TEST_COCKROACH_URL not set; skipping");
        return;
    };
    let d = driver("cockroachdb");
    let mut m = d.connect(&cfg, None).await.unwrap();
    let _ = m.drop_database(DB).await;
    m.create_database(DB).await.unwrap();
    let mut s = d.connect(&cfg, Some(DB)).await.unwrap();
    for sql in [
        "CREATE TABLE clientes (id int PRIMARY KEY)",
        "INSERT INTO clientes SELECT generate_series(1, 400)",
        "CREATE TABLE sin_analizar (id int PRIMARY KEY) WITH (sql_stats_automatic_collection_enabled = false)",
        "INSERT INTO sin_analizar SELECT generate_series(1, 10)",
        "CREATE VIEW v_clientes AS SELECT * FROM clientes",
        "CREATE FUNCTION doble(x int) RETURNS int LANGUAGE sql AS 'SELECT x * 2'",
        "COMMENT ON VIEW v_clientes IS 'Vista de clientes'",
        "COMMENT ON FUNCTION doble IS 'El doble'",
        "CREATE STATISTICS manual FROM clientes",
    ] {
        run(&mut s, sql).await;
    }

    // The new statistics reach SHOW TABLES' cache a moment later.
    let mut est = Vec::new();
    for _ in 0..40 {
        est = s.row_estimates().await.unwrap();
        if rows_of(&est, "table", "clientes") == Some(400) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    eprintln!("{est:?}");
    assert_eq!(rows_of(&est, "table", "clientes"), Some(400));
    assert_eq!(rows_of(&est, "table", "sin_analizar"), None, "no statistics: no estimate");

    let c = s.object_comments().await.unwrap();
    eprintln!("{c:?}");
    assert_eq!(comment_of(&c, "view", "v_clientes"), Some("Vista de clientes"));
    assert_eq!(comment_of(&c, "function", "doble"), Some("El doble"));
    drop(s);
    m.drop_database(DB).await.unwrap();
}
