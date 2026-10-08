//! Row estimates and object comments of a Firebird database against a
//! real server (`DBINE_TEST_FIREBIRD_URL`): after `SET STATISTICS`, a
//! table's rows come from its primary key's selectivity; a table without a
//! unique index has none; views, procedures, sequences and domains carry
//! their `COMMENT ON`.
//!
//! ```sh
//! DBINE_TEST_FIREBIRD_URL=firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb \
//!   cargo test -p dbine-driver-firebird --test stats -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_FIREBIRD_URL").ok()?;
    let rest = url.strip_prefix("firebird://")?;
    let (cred, addr) = rest.split_once('@')?;
    let (user, pass) = cred.split_once(':')?;
    let (hostport, path) = addr.split_once('/')?;
    let (host, port) = hostport.split_once(':')?;
    Some(ConnectionConfig {
        driver: "firebird".into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: path.into(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn estimates_and_comments() {
    let Some(base) = config() else {
        eprintln!("DBINE_TEST_FIREBIRD_URL not set; skipping");
        return;
    };
    let d = dbine_driver_firebird::drivers().remove(0);
    let mut m = d.connect(&base, None).await.unwrap();
    let folder = base.database.rsplit_once('/').map(|(dir, _)| dir.to_string()).unwrap();
    let file = format!("{folder}/dbine_stats.fdb");
    let _ = m.drop_database(&file).await;
    m.create_database(&file).await.unwrap();

    let mut cfg = base.clone();
    cfg.database = file.clone();
    let mut s = d.connect(&cfg, None).await.unwrap();
    run(&mut s, "CREATE TABLE clientes (id INTEGER NOT NULL CONSTRAINT pk_clientes PRIMARY KEY, nombre VARCHAR(40))").await;
    run(&mut s, "CREATE TABLE eventos (id INTEGER)").await;
    run(&mut s, "COMMIT").await;
    run(&mut s, "EXECUTE BLOCK AS DECLARE i INTEGER = 1; BEGIN WHILE (i <= 250) DO BEGIN INSERT INTO clientes VALUES (:i, 'c' || :i); INSERT INTO eventos VALUES (:i); i = i + 1; END END").await;
    run(&mut s, "COMMIT").await;
    run(&mut s, "SET STATISTICS INDEX pk_clientes").await;
    run(&mut s, "CREATE VIEW v_clientes AS SELECT id, nombre FROM clientes").await;
    run(&mut s, "CREATE PROCEDURE p_hola RETURNS (x INTEGER) AS BEGIN x = 1; SUSPEND; END").await;
    run(&mut s, "CREATE SEQUENCE sq_pedidos").await;
    run(&mut s, "CREATE DOMAIN d_importe AS NUMERIC(12, 2)").await;
    run(&mut s, "COMMIT").await;
    run(&mut s, "COMMENT ON VIEW v_clientes IS 'Clientes visibles'").await;
    run(&mut s, "COMMENT ON PROCEDURE p_hola IS 'Saluda'").await;
    run(&mut s, "COMMENT ON SEQUENCE sq_pedidos IS 'Numera pedidos'").await;
    run(&mut s, "COMMENT ON DOMAIN d_importe IS 'Importe en pesos'").await;
    run(&mut s, "COMMIT").await;

    let est = s.row_estimates().await.unwrap();
    eprintln!("{est:?}");
    let rows = |name: &str| est.iter().find(|e| e.object.kind == "table" && e.object.name == name).map(|e| e.rows);
    assert_eq!(rows("CLIENTES"), Some(250));
    assert_eq!(rows("EVENTOS"), None, "no unique index: no estimate");

    let comments = s.object_comments().await.unwrap();
    eprintln!("{comments:?}");
    let comment = |kind: &str, name: &str| {
        comments.iter().find(|c| c.object.kind == kind && c.object.name == name).map(|c| c.comment.clone())
    };
    assert_eq!(comment("view", "V_CLIENTES").as_deref(), Some("Clientes visibles"));
    assert_eq!(comment("procedure", "P_HOLA").as_deref(), Some("Saluda"));
    assert_eq!(comment("sequence", "SQ_PEDIDOS").as_deref(), Some("Numera pedidos"));
    assert_eq!(comment("type", "D_IMPORTE").as_deref(), Some("Importe en pesos"));

    // Every commented object is one list_objects returns, with the same kind.
    let objects = s.list_objects().await.unwrap();
    for c in &comments {
        assert!(objects.iter().any(|o| o.kind == c.object.kind && o.name == c.object.name), "{c:?}");
    }

    drop(s);
    m.drop_database(&file).await.unwrap();
}
