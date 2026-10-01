//! Against real servers. Each test reads `DBINE_TEST_<ENGINE>_URL`
//! (`postgres://user:pass@host:port/db`) and is skipped without it:
//!
//! ```sh
//! docker run -d --name dbine-test-pg -e POSTGRES_PASSWORD=pw -p 25010:5432 postgres:16
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//!   cargo test -p dbine-driver-postgres --test integration -- --ignored
//! ```

use dbine_driver::{kinds, ConnectionConfig, Driver, Error, ObjectRef, QueryOutcome, Session};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// `scheme://user:pass@host:port/db` into a config (no URL escapes).
fn parse_url(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> Result<QueryOutcome, Error> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1000, &mut out).await.map(|_| out)
}

fn obj(kind: &str, name: &str) -> ObjectRef {
    ObjectRef { kind: kind.into(), schema: Some("dbine_t".into()), name: name.into() }
}

async fn exercise(id: &str, env: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let cfg = parse_url(id, &url);
    let d = driver(id);
    let mut s = d.connect(&cfg, None).await.expect("connect");

    let version = s.server_version().await.unwrap();
    eprintln!("{id}: {version}");
    assert!(!version.is_empty());
    let dbs = s.list_databases().await.unwrap();
    eprintln!("{id}: databases {dbs:?}");
    assert!(!dbs.is_empty());

    run(&mut s, "DROP SCHEMA IF EXISTS dbine_t CASCADE").await.unwrap();
    run(
        &mut s,
        "CREATE SCHEMA dbine_t;
         CREATE TABLE dbine_t.items (id serial PRIMARY KEY, name varchar(40) NOT NULL DEFAULT 'x', price numeric(10,2));
         INSERT INTO dbine_t.items (name, price) VALUES ('a', 1.5), ('b', 2), ('c', NULL);
         CREATE VIEW dbine_t.v_items AS SELECT id, name FROM dbine_t.items;",
    )
    .await
    .unwrap();
    // Optional features: each variant supports a different subset.
    let mut extras = Vec::new();
    for (kind, name, sql) in [
        (kinds::MATERIALIZED_VIEW, "mv_items", "CREATE MATERIALIZED VIEW dbine_t.mv_items AS SELECT count(*) AS n FROM dbine_t.items"),
        (kinds::FUNCTION, "add_one", "CREATE FUNCTION dbine_t.add_one(x int) RETURNS int LANGUAGE sql AS 'SELECT x + 1'"),
        (kinds::PROCEDURE, "noop", "CREATE PROCEDURE dbine_t.noop() LANGUAGE sql AS 'SELECT 1'"),
        (
            kinds::TRIGGER,
            "items_trg",
            "CREATE FUNCTION dbine_t.trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
             CREATE TRIGGER items_trg BEFORE INSERT ON dbine_t.items FOR EACH ROW EXECUTE FUNCTION dbine_t.trg();",
        ),
    ] {
        match run(&mut s, sql).await {
            Ok(_) => extras.push((kind, name)),
            Err(e) => eprintln!("{id}: no {kind}: {e}"),
        }
    }

    if id == "timescaledb" {
        // Hypertable chunks and the extension's schemas stay hidden.
        run(
            &mut s,
            "CREATE EXTENSION IF NOT EXISTS timescaledb;
             CREATE TABLE dbine_t.metrics (ts timestamptz NOT NULL, v double precision);
             SELECT create_hypertable('dbine_t.metrics', 'ts');
             INSERT INTO dbine_t.metrics VALUES (now(), 1), (now() - interval '30 days', 2);",
        )
        .await
        .unwrap();
    }

    let objs = s.list_objects().await.unwrap();
    assert!(
        !objs.iter().any(|o| o.schema.as_deref().is_some_and(|s| s.starts_with("_timescaledb") || s == "crdb_internal")),
        "{id}: system schemas leaked"
    );
    let find = |name: &str| objs.iter().find(|o| o.schema.as_deref() == Some("dbine_t") && o.name == name);
    assert_eq!(find("items").map(|o| o.kind.as_str()), Some(kinds::TABLE));
    assert_eq!(find("v_items").map(|o| o.kind.as_str()), Some(kinds::VIEW));
    for (kind, name) in &extras {
        assert_eq!(find(name).map(|o| o.kind.as_str()), Some(*kind), "{id}: {name} in {objs:?}");
    }
    let declared: Vec<_> = d.info().object_kinds.iter().map(|k| k.id).collect();
    assert!(objs.iter().all(|o| declared.contains(&o.kind.as_str())), "{id}: undeclared kind");
    assert!(!objs.iter().any(|o| o.schema.as_deref() == Some("pg_catalog")));

    let cols = s.columns(&obj(kinds::TABLE, "items")).await.unwrap();
    eprintln!("{id}: columns {cols:?}");
    assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "name", "price"]);
    assert!(cols[0].primary_key && cols[0].auto_increment);
    assert!(!cols[1].nullable && cols[2].nullable);

    let view = s.definition(&obj(kinds::VIEW, "v_items")).await.unwrap().expect("view definition");
    assert!(view.to_lowercase().contains("items"), "{view}");
    let table = s.definition(&obj(kinds::TABLE, "items")).await.unwrap();
    eprintln!("{id}: table definition {table:?}");
    for (kind, name) in &extras {
        let def = s.definition(&obj(kind, name)).await.unwrap();
        assert!(def.is_some(), "{id}: no definition for {kind} {name}");
    }

    let q = s.browse_query(&obj(kinds::TABLE, "items"), 2);
    let out = run(&mut s, &q).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2);
    assert_eq!(out.results[0].columns.len(), 3);

    // Several statements; one fails in the middle.
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT 1 AS a; SELECT * FROM dbine_t.nope; SELECT 2", 10, &mut out).await.unwrap_err();
    assert!(e.is_query(), "{e:?}");
    assert_eq!(out.results.len(), 1, "{id}: {out:?}");

    // Truncation past max_rows.
    let mut out = QueryOutcome::default();
    s.execute("SELECT * FROM generate_series(1, 10)", 3, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 3);
    assert_eq!(out.results[0].total_rows, 10);
    assert!(out.results[0].truncated);

    // Affected rows.
    let out = run(&mut s, "UPDATE dbine_t.items SET price = 3 WHERE name <> 'a'").await.unwrap();
    assert_eq!(out.results[0].rows_affected, Some(2));

    // Notices (plpgsql DO blocks).
    match run(&mut s, "DO $$ BEGIN RAISE NOTICE 'hola'; END $$").await {
        Ok(out) => assert!(out.messages.iter().any(|m| m.contains("hola")), "{id}: {:?}", out.messages),
        Err(e) => eprintln!("{id}: no DO blocks: {e}"),
    }

    // Cancel a long statement from another task.
    let stop = s.interrupter().expect("interrupter");
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        stop();
    });
    let t = Instant::now();
    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT pg_sleep(30)", 10, &mut out).await.unwrap_err();
    assert!(matches!(e, Error::Cancelled), "{id}: {e:?}");
    assert!(t.elapsed() < Duration::from_secs(10));
    // The session is still usable afterwards.
    run(&mut s, "SELECT 1").await.unwrap();

    // Read-only session: the server refuses writes on its own.
    let mut ro_cfg = cfg.clone();
    ro_cfg.read_only = true;
    let mut ro = d.connect(&ro_cfg, None).await.unwrap();
    let e = run(&mut ro, "INSERT INTO dbine_t.items (name) VALUES ('z')").await.unwrap_err();
    eprintln!("{id}: read-only refusal: {e}");
    assert!(e.is_query(), "{e:?}");

    // A second database through `connect(.., Some(db))`.
    let other = dbs.iter().find(|d| d.as_str() != cfg.database).cloned();
    if let Some(db) = other {
        let mut s2 = d.connect(&cfg, Some(&db)).await.unwrap();
        s2.list_objects().await.unwrap();
    }

    run(&mut s, "DROP SCHEMA dbine_t CASCADE").await.unwrap();

    // Wrong password.
    let mut bad = cfg.clone();
    bad.password = Some("definitely-wrong".into());
    if cfg.password.is_some() {
        match d.connect(&bad, None).await {
            Err(Error::AuthFailed(m)) => eprintln!("{id}: auth refused: {m}"),
            Err(e) => panic!("{id}: expected AuthFailed, got {e:?}"),
            Ok(_) => panic!("{id}: wrong password accepted"),
        }
    }
}

#[tokio::test]
#[ignore]
async fn postgres() {
    exercise("postgres", "DBINE_TEST_POSTGRES_URL").await;
}

#[tokio::test]
#[ignore]
async fn cockroachdb() {
    exercise("cockroachdb", "DBINE_TEST_COCKROACHDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn timescaledb() {
    exercise("timescaledb", "DBINE_TEST_TIMESCALEDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn yugabytedb() {
    exercise("yugabytedb", "DBINE_TEST_YUGABYTEDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn greenplum() {
    exercise("greenplum", "DBINE_TEST_GREENPLUM_URL").await;
}

#[tokio::test]
#[ignore]
async fn cloudberry() {
    exercise("cloudberry", "DBINE_TEST_CLOUDBERRY_URL").await;
}

#[tokio::test]
#[ignore]
async fn greengage() {
    exercise("greengage", "DBINE_TEST_GREENGAGE_URL").await;
}

#[tokio::test]
#[ignore]
async fn redshift() {
    exercise("redshift", "DBINE_TEST_REDSHIFT_URL").await;
}

/// One line per operator, indented, with its figures.
fn outline(n: &dbine_driver::PlanNode, depth: usize) -> String {
    let mut s = format!(
        "{}{} [{}] {:?} cost={:?} est={:?} act={:?} x{:?} ms={:?} {:?}\n",
        "  ".repeat(depth), n.op, n.detail, n.object, n.total_cost, n.est_rows, n.actual_rows, n.executions, n.actual_ms, n.warnings
    );
    for c in &n.children {
        s.push_str(&outline(c, depth + 1));
    }
    s
}

/// Estimated plans run nothing (a DELETE leaves its rows); actual plans
/// run the script and carry measured figures.
async fn plans(id: &str, env: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let mut s = driver(id).connect(&parse_url(id, &url), None).await.expect("connect");
    run(&mut s, "DROP TABLE IF EXISTS dbine_plan_b; DROP TABLE IF EXISTS dbine_plan_a").await.unwrap();
    run(
        &mut s,
        "CREATE TABLE dbine_plan_a (id int PRIMARY KEY, g int);
         CREATE TABLE dbine_plan_b (id int PRIMARY KEY, a_id int);
         INSERT INTO dbine_plan_a SELECT i, i % 10 FROM generate_series(1, 2000) i;
         INSERT INTO dbine_plan_b SELECT i, i % 100 FROM generate_series(1, 500) i;",
    )
    .await
    .unwrap();
    let count = |out: &QueryOutcome| out.results.last().unwrap().rows[0][0].clone();

    let mut out = QueryOutcome::default();
    s.explain(
        "SELECT a.g, count(*) FROM dbine_plan_a a JOIN dbine_plan_b b ON b.a_id = a.id GROUP BY a.g;
         DELETE FROM dbine_plan_b WHERE id < 100",
        false,
        100,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.plans.len(), 2, "{out:?}");
    assert!(out.results.is_empty());
    assert!(out.plans.iter().all(|p| !p.actual && !p.root.op.is_empty()));
    eprintln!("{id}: estimated\n{}", outline(&out.plans[0].root, 0));
    assert!(run(&mut s, "SELECT count(*) FROM dbine_plan_b").await.map(|o| count(&o)).unwrap() == "500");

    let mut out = QueryOutcome::default();
    s.explain(
        "SELECT count(*) FROM dbine_plan_a WHERE g = 3; DELETE FROM dbine_plan_b WHERE id <= 100; SELECT count(*) FROM dbine_plan_b",
        true,
        100,
        &mut out,
    )
    .await
    .unwrap();
    assert_eq!(out.plans.len(), 3);
    assert_eq!(out.results.len(), 3, "{out:?}");
    assert_eq!(out.results[0].rows[0][0], "200");
    // The DELETE ran exactly once.
    assert_eq!(count(&out), "400");
    assert!(out.plans[0].actual && !out.plans[1].actual && out.plans[2].actual);
    let has_actual = |n: &dbine_driver::PlanNode| n.actual_rows.is_some();
    assert!(has_actual(&out.plans[0].root), "{:#?}", out.plans[0].root);
    eprintln!("{id}: actual\n{}", outline(&out.plans[0].root, 0));

    // A failing statement stops the script; what ran stays.
    let mut out = QueryOutcome::default();
    assert!(s.explain("SELECT 1; SELECT * FROM dbine_plan_missing", true, 10, &mut out).await.is_err());
    assert_eq!(out.results.len(), 1);
    run(&mut s, "DROP TABLE dbine_plan_b; DROP TABLE dbine_plan_a").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn postgres_plans() {
    plans("postgres", "DBINE_TEST_POSTGRES_URL").await;
}

#[tokio::test]
#[ignore]
async fn cloudberry_plans() {
    plans("cloudberry", "DBINE_TEST_CLOUDBERRY_URL").await;
}

#[tokio::test]
#[ignore]
async fn greengage_plans() {
    plans("greengage", "DBINE_TEST_GREENGAGE_URL").await;
}

#[tokio::test]
#[ignore]
async fn opengauss_plans() {
    plans("opengauss", "DBINE_TEST_OPENGAUSS_URL").await;
}

#[tokio::test]
#[ignore]
async fn cockroachdb_plans() {
    plans("cockroachdb", "DBINE_TEST_COCKROACHDB_URL").await;
}

/// The engines that took the protocol but not the rest of PostgreSQL
/// (no serial, no PL/pgSQL, their own EXPLAIN): the explorer, scripts and
/// estimated plans.
///
/// ```sh
/// docker run -d --name dbine-test-cratedb -p 25021:5432 crate:latest -Cdiscovery.type=single-node
/// docker run -d --name dbine-test-risingwave -p 25023:4566 risingwavelabs/risingwave:latest single_node
/// docker run -d --name dbine-test-materialize -p 25024:6875 materialize/materialized:latest
/// DBINE_TEST_CRATEDB_URL=postgres://crate@localhost:25021/doc \
/// DBINE_TEST_RISINGWAVE_URL=postgres://root@localhost:25023/dev \
/// DBINE_TEST_MATERIALIZE_URL=postgres://materialize@localhost:25024/materialize \
///   cargo test -p dbine-driver-postgres --test integration -- --ignored
/// ```
async fn exercise_light(id: &str, env: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let cfg = parse_url(id, &url);
    let d = driver(id);
    let mut s = d.connect(&cfg, None).await.expect("connect");
    let version = s.server_version().await.unwrap();
    eprintln!("{id}: {version}");
    let dbs = s.list_databases().await.unwrap();
    eprintln!("{id}: databases {dbs:?}");
    assert!(!dbs.is_empty());

    let streaming = matches!(id, "risingwave" | "materialize");
    let cleanup = [
        "DROP MATERIALIZED VIEW IF EXISTS dbine_t.mv_items",
        "DROP VIEW IF EXISTS dbine_t.v_items",
        "DROP TABLE IF EXISTS dbine_t.items",
    ];
    for sql in cleanup {
        if streaming || !sql.contains("MATERIALIZED") {
            let _ = run(&mut s, sql).await;
        }
    }
    let mut setup = match id {
        "cratedb" => vec![
            "CREATE TABLE dbine_t.items (id integer PRIMARY KEY, name varchar(40), price double precision)",
            "INSERT INTO dbine_t.items (id, name, price) VALUES (1, 'a', 1.5), (2, 'b', 2), (3, 'c', NULL)",
            "REFRESH TABLE dbine_t.items",
        ],
        "risingwave" => vec![
            "CREATE SCHEMA IF NOT EXISTS dbine_t",
            "CREATE TABLE dbine_t.items (id integer PRIMARY KEY, name varchar, price double precision)",
            "INSERT INTO dbine_t.items (id, name, price) VALUES (1, 'a', 1.5), (2, 'b', 2), (3, 'c', NULL)",
            "FLUSH",
        ],
        _ => vec![
            "CREATE SCHEMA IF NOT EXISTS dbine_t",
            "CREATE TABLE dbine_t.items (id integer NOT NULL, name varchar(40), price double precision)",
            "INSERT INTO dbine_t.items (id, name, price) VALUES (1, 'a', 1.5), (2, 'b', 2), (3, 'c', NULL)",
        ],
    };
    setup.push("CREATE VIEW dbine_t.v_items AS SELECT id, name FROM dbine_t.items");
    if streaming {
        setup.push("CREATE MATERIALIZED VIEW dbine_t.mv_items AS SELECT count(*) AS n FROM dbine_t.items");
    }
    for sql in &setup {
        run(&mut s, sql).await.unwrap_or_else(|e| panic!("{id}: {sql}: {e}"));
    }

    let objs = s.list_objects().await.unwrap();
    let find = |name: &str| objs.iter().find(|o| o.schema.as_deref() == Some("dbine_t") && o.name == name);
    assert_eq!(find("items").map(|o| o.kind.as_str()), Some(kinds::TABLE), "{id}: {objs:?}");
    assert_eq!(find("v_items").map(|o| o.kind.as_str()), Some(kinds::VIEW), "{id}: {objs:?}");
    if streaming {
        assert_eq!(find("mv_items").map(|o| o.kind.as_str()), Some(kinds::MATERIALIZED_VIEW), "{id}: {objs:?}");
    }
    let declared: Vec<_> = d.info().object_kinds.iter().map(|k| k.id).collect();
    assert!(objs.iter().all(|o| declared.contains(&o.kind.as_str())), "{id}: undeclared kind in {objs:?}");
    assert!(!objs.iter().any(|o| matches!(o.schema.as_deref(), Some("pg_catalog" | "sys" | "mz_catalog" | "rw_catalog"))));

    let cols = s.columns(&obj(kinds::TABLE, "items")).await.unwrap();
    eprintln!("{id}: columns {cols:?}");
    assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "name", "price"]);
    for (kind, name) in [(kinds::TABLE, "items"), (kinds::VIEW, "v_items"), (kinds::MATERIALIZED_VIEW, "mv_items")] {
        if kind == kinds::MATERIALIZED_VIEW && !streaming {
            continue;
        }
        let def = s.definition(&obj(kind, name)).await.unwrap();
        eprintln!("{id}: {kind} definition {def:?}");
        assert!(def.is_some_and(|d| d.to_lowercase().contains("items")), "{id}: {kind}");
    }

    let q = s.browse_query(&obj(kinds::TABLE, "items"), 2);
    let out = run(&mut s, &q).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2);

    let mut out = QueryOutcome::default();
    let e = s.execute("SELECT 1 AS a; SELECT * FROM dbine_t.nope; SELECT 2", 10, &mut out).await.unwrap_err();
    assert!(e.is_query(), "{e:?}");
    assert_eq!(out.results.len(), 1, "{id}: {out:?}");

    let mut out = QueryOutcome::default();
    s.execute("SELECT * FROM generate_series(1, 10)", 3, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 3);
    assert_eq!(out.results[0].total_rows, 10);

    // Estimated plans; "actual" runs the statements and says why there are
    // no figures.
    let mut out = QueryOutcome::default();
    s.explain("SELECT name, count(*) FROM dbine_t.items WHERE id > 1 GROUP BY name", false, 100, &mut out).await.unwrap();
    assert_eq!(out.plans.len(), 1, "{out:?}");
    eprintln!("{id}: estimated\n{}", outline(&out.plans[0].root, 0));
    assert!(!out.plans[0].root.op.is_empty());
    let mut out = QueryOutcome::default();
    s.explain("SELECT count(*) FROM dbine_t.items", true, 100, &mut out).await.unwrap();
    assert_eq!(out.results.len(), 1, "{out:?}");
    assert!(!out.messages.is_empty());

    let schema = s.database_schema().await.unwrap();
    let items = schema.iter().find(|t| t.name == "items").expect("items in database_schema");
    assert_eq!(items.columns.len(), 3);

    for sql in cleanup {
        if streaming || !sql.contains("MATERIALIZED") {
            run(&mut s, sql).await.unwrap_or_else(|e| panic!("{id}: {sql}: {e}"));
        }
    }
}

#[tokio::test]
#[ignore]
async fn cratedb() {
    exercise_light("cratedb", "DBINE_TEST_CRATEDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn risingwave() {
    exercise_light("risingwave", "DBINE_TEST_RISINGWAVE_URL").await;
}

#[tokio::test]
#[ignore]
async fn materialize() {
    exercise_light("materialize", "DBINE_TEST_MATERIALIZE_URL").await;
}

/// `docker run -d --name dbine-test-h2 --platform linux/amd64 -p 25025:5435 --entrypoint sh oscarfonts/h2 \
///   -c 'java -cp /opt/h2/bin/h2-2.1.214.jar org.h2.tools.Server -pg -pgAllowOthers -pgPort 5435 -ifNotExists -baseDir /tmp'`
/// and `DBINE_TEST_H2_URL=postgres://sa:sa@localhost:25025/test`.
#[tokio::test]
#[ignore]
async fn h2() {
    exercise_light("h2", "DBINE_TEST_H2_URL").await;
}

#[tokio::test]
#[ignore]
async fn h2_monitor() {
    monitor("h2", "DBINE_TEST_H2_URL").await;
}

#[tokio::test]
#[ignore]
async fn opengauss() {
    exercise("opengauss", "DBINE_TEST_OPENGAUSS_URL").await;
}

/// Two snapshots of the monitor: real figures, the standard tables, and
/// running totals that don't go backwards.
async fn monitor(id: &str, env: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let d = driver(id);
    assert!(d.capabilities().monitor, "{id}: monitor capability");
    let mut s = d.connect(&parse_url(id, &url), None).await.expect("connect");
    let first = s.monitor().await.expect("monitor");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let snap = s.monitor().await.expect("monitor");
    for m in &snap.metrics {
        eprintln!(
            "{id}: [{}] {} = {:?}{}{}",
            m.group,
            m.key,
            m.value,
            m.max.map(|x| format!(" / {x}")).unwrap_or_default(),
            if m.counter { " (counter)" } else { "" }
        );
    }
    for (k, v) in &snap.info {
        eprintln!("{id}: info {k}: {v}");
    }
    for t in &snap.tables {
        eprintln!("{id}: table {} ({} cols, {} rows) {:?}", t.key, t.columns.len(), t.rows.len(), t.rows.first());
        assert!(t.rows.len() <= 200);
        assert!(t.rows.iter().all(|r| r.len() == t.columns.len()), "{id}: {} row width", t.key);
    }
    for n in &snap.notes {
        eprintln!("{id}: note {n}");
    }
    let with_value = snap.metrics.iter().filter(|m| m.value.is_some()).count();
    assert!(with_value >= 3, "{id}: only {with_value} metrics with a value");
    assert!(!snap.info.is_empty() && !snap.tables.is_empty());
    let mut keys: Vec<_> = snap.metrics.iter().map(|m| m.key.as_str()).collect();
    keys.sort();
    let n = keys.len();
    keys.dedup();
    assert_eq!(keys.len(), n, "{id}: duplicated metric keys");
    for m in snap.metrics.iter().filter(|m| m.counter) {
        let before = first.metrics.iter().find(|x| x.key == m.key).and_then(|x| x.value);
        if let (Some(a), Some(b)) = (before, m.value) {
            assert!(b >= a, "{id}: counter {} went back ({a} -> {b})", m.key);
        }
    }
}

#[tokio::test]
#[ignore]
async fn postgres_monitor() {
    monitor("postgres", "DBINE_TEST_POSTGRES_URL").await;
}

#[tokio::test]
#[ignore]
async fn cockroachdb_monitor() {
    monitor("cockroachdb", "DBINE_TEST_COCKROACHDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn timescaledb_monitor() {
    monitor("timescaledb", "DBINE_TEST_TIMESCALEDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn yugabytedb_monitor() {
    monitor("yugabytedb", "DBINE_TEST_YUGABYTEDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn greenplum_monitor() {
    monitor("greenplum", "DBINE_TEST_GREENPLUM_URL").await;
}

#[tokio::test]
#[ignore]
async fn cloudberry_monitor() {
    monitor("cloudberry", "DBINE_TEST_CLOUDBERRY_URL").await;
}

#[tokio::test]
#[ignore]
async fn greengage_monitor() {
    monitor("greengage", "DBINE_TEST_GREENGAGE_URL").await;
}

#[tokio::test]
#[ignore]
async fn redshift_monitor() {
    monitor("redshift", "DBINE_TEST_REDSHIFT_URL").await;
}

#[tokio::test]
#[ignore]
async fn opengauss_monitor() {
    monitor("opengauss", "DBINE_TEST_OPENGAUSS_URL").await;
}

#[tokio::test]
#[ignore]
async fn cratedb_monitor() {
    monitor("cratedb", "DBINE_TEST_CRATEDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn risingwave_monitor() {
    monitor("risingwave", "DBINE_TEST_RISINGWAVE_URL").await;
}

#[tokio::test]
#[ignore]
async fn materialize_monitor() {
    monitor("materialize", "DBINE_TEST_MATERIALIZE_URL").await;
}

/// The managed services and PostgreSQL distributions speak plain
/// PostgreSQL; pointed at one, their variant must work end to end
/// (`DBINE_TEST_PGFAMILY_URL`, e.g. the dbine-test-postgres container).
#[tokio::test]
#[ignore]
async fn postgres_distributions() {
    let Ok(url) = std::env::var("DBINE_TEST_PGFAMILY_URL") else {
        eprintln!("DBINE_TEST_PGFAMILY_URL not set; skipping");
        return;
    };
    for id in ["alloydb", "cloudsql_postgres", "aurora_postgres", "edb", "fujitsu"] {
        std::env::set_var("DBINE_TEST_PGFAMILY_ONE", &url);
        exercise(id, "DBINE_TEST_PGFAMILY_ONE").await;
        monitor(id, "DBINE_TEST_PGFAMILY_ONE").await;
    }
}

// -- profiler -----------------------------------------------------------------------------

/// The profiler sees another session's statements: a slow one (with its
/// duration) and, where the engine keeps the last statement or a history,
/// a fast one. Each is reported once.
async fn profile(id: &str, env: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let cfg = parse_url(id, &url);
    let d = driver(id);
    assert!(d.supports_profiler(), "{id}");
    let mut p = d.connect(&cfg, None).await.expect("connect");
    let mut w = d.connect(&cfg, None).await.expect("connect");
    let opts = dbine_driver::ProfilerOptions { database: cfg.database.clone(), change_server: true };
    let started = match p.profiler_start(&opts).await {
        Err(Error::Query(e)) if id == "materialize" && e.contains("mz_system") => {
            eprintln!("{id}: {e}");
            return;
        }
        r => r.expect("profiler_start"),
    };
    eprintln!("{id}: {started:?}");
    let complete = started.mode == dbine_driver::ProfilerMode::Complete;
    let marker = format!("dbine_prof_{}", std::process::id());
    // Sampling engines only see a statement while it runs (or, in
    // pg_stat_activity, as a connection's last one): the slow one must last.
    let slow = match id {
        "h2" => format!("SELECT DBINE_SLEEP(600), 1 AS {marker}_slow"),
        "cratedb" | "materialize" => format!("SELECT 2 AS {marker}_slow"),
        _ => format!("SELECT pg_sleep(0.6), 1 AS {marker}_slow"),
    };
    let fast = format!("SELECT 1 AS {marker}_fast");
    let fast_seen = id != "cockroachdb" && id != "h2";

    if id == "h2" {
        run(&mut w, "CREATE ALIAS IF NOT EXISTS DBINE_SLEEP FOR 'java.lang.Thread.sleep(long)'").await.expect("alias");
    }
    let work = async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        run(&mut w, &slow).await.expect("slow");
        tokio::time::sleep(Duration::from_millis(400)).await;
        run(&mut w, &fast).await.expect("fast");
        tokio::time::sleep(Duration::from_millis(600)).await;
    };
    let watch = async {
        let mut got = Vec::new();
        let until = Instant::now() + Duration::from_secs(if complete { 12 } else { 4 });
        while Instant::now() < until {
            got.extend(p.profiler_poll().await.expect("profiler_poll"));
            let n = got.iter().filter(|s| s.text.contains(&marker)).count();
            if n >= if fast_seen { 2 } else { 1 } && (!complete || Instant::now() + Duration::from_secs(9) > until) {
                break;
            }
            if complete {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
        got
    };
    let ((), got) = tokio::join!(work, watch);
    p.profiler_stop().await.expect("profiler_stop");
    let mine: Vec<_> = got.iter().filter(|s| s.text.contains(&marker)).collect();
    eprintln!("{id}: {mine:#?}");
    let slow_seen: Vec<_> = mine.iter().filter(|s| s.text.contains("_slow")).collect();
    assert_eq!(slow_seen.len(), 1, "{id}: the slow statement once");
    if !complete {
        assert!(slow_seen[0].duration_ms.unwrap_or(0.0) >= 300.0, "{id}: duration {:?}", slow_seen[0].duration_ms);
    }
    if fast_seen {
        assert_eq!(mine.iter().filter(|s| s.text.contains("_fast")).count(), 1, "{id}: the fast statement once");
    }
    assert!(got.iter().all(|s| !s.text.contains("pg_stat_activity")), "{id}: its own statements are left out");
}

#[tokio::test]
#[ignore]
async fn postgres_profiler() {
    profile("postgres", "DBINE_TEST_POSTGRES_URL").await;
}

#[tokio::test]
#[ignore]
async fn cockroachdb_profiler() {
    profile("cockroachdb", "DBINE_TEST_COCKROACHDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn yugabytedb_profiler() {
    profile("yugabytedb", "DBINE_TEST_YUGABYTEDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn cratedb_profiler() {
    profile("cratedb", "DBINE_TEST_CRATEDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn materialize_profiler() {
    profile("materialize", "DBINE_TEST_MATERIALIZE_URL").await;
}

#[tokio::test]
#[ignore]
async fn h2_profiler() {
    profile("h2", "DBINE_TEST_H2_URL").await;
}
