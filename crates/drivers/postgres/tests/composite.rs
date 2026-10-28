//! Composite values show their field names, against a real PostgreSQL:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//!   cargo test -p dbine-driver-postgres --test composite -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, QueryOutcome, Session};
use serde_json::{json, Value};

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_POSTGRES_URL").ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "postgres".into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out
}

const SQL: &str = "SELECT
  ARRAY[ROW(42, true, ARRAY[ROW('demo-product', 2)::dbine_comp.item], NULL)::dbine_comp.order_t] AS a,
  ROW('', '{}', '{\"k\": [1]}', 9007199254740993)::dbine_comp.misc AS b,
  ROW(1, 'x') AS anon";

fn check(out: &QueryOutcome) {
    let row = &out.results[0].rows[0];
    assert_eq!(
        row[0],
        json!(
            r#"[{"customer_id":42,"active":true,"items":[{"product_name":"demo-product","quantity":2}],"note":null}]"#
        )
    );
    let b: Value = serde_json::from_str(row[1].as_str().unwrap()).unwrap();
    assert_eq!(
        b,
        json!({"a b": "", "e": [], "j": {"k": [1]}, "big": "9007199254740993"})
    );
    // An anonymous record has no names: PostgreSQL's own text.
    assert_eq!(row[2], json!("(1,x)"));
}

#[tokio::test]
#[ignore]
async fn postgres_composites_have_field_names() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_POSTGRES_URL not set; skipping");
        return;
    };
    let d = dbine_driver_postgres::drivers()
        .into_iter()
        .find(|d| d.info().id == "postgres")
        .unwrap();
    let mut s = d.connect(&c, None).await.unwrap();
    run(&mut s, "DROP SCHEMA IF EXISTS dbine_comp CASCADE").await;
    run(&mut s, "CREATE SCHEMA dbine_comp").await;
    run(
        &mut s,
        "CREATE TYPE dbine_comp.item AS (product_name text, quantity int)",
    )
    .await;
    run(
        &mut s,
        "CREATE TYPE dbine_comp.order_t AS (customer_id bigint, active boolean, \
         items dbine_comp.item[], note text)",
    )
    .await;
    run(
        &mut s,
        "CREATE TYPE dbine_comp.misc AS (\"a b\" text, e int[], j jsonb, big bigint)",
    )
    .await;
    check(&run(&mut s, SQL).await);
    // The read-only path (assistant and MCP reads) too.
    let mut out = QueryOutcome::default();
    s.run_read_only(SQL, 10, &mut out).await.unwrap();
    assert!(out.error.is_none(), "{:?}", out.error);
    check(&out);
    run(&mut s, "DROP SCHEMA dbine_comp CASCADE").await;
}
