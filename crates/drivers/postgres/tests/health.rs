//! "Chequeo de salud" of PostgreSQL and CockroachDB against real servers:
//! a database with problems made on purpose shows each one, and running a
//! finding's fix clears it. Each test reads `DBINE_TEST_<ENGINE>_URL` and
//! is skipped without it:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//!   cargo test -p dbine-driver-postgres --test health -- --ignored --test-threads=1
//! ```

use dbine_driver::health::{HealthCheck, Severity};
use dbine_driver::{ConnectionConfig, Driver, QueryOutcome, Session};
use std::sync::Arc;
use std::time::Duration;

const DB: &str = "dbine_health";

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

/// A statement that's meant to fail.
async fn run_failing(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    let failed = s.execute(sql, 10, &mut out).await.is_err() || out.error.is_some();
    assert!(failed, "{sql} should have failed");
}

async fn checks(s: &mut Box<dyn Session>) -> Vec<HealthCheck> {
    let checks = s.health_checks(DB).await.unwrap();
    for c in &checks {
        eprintln!("{:?} [{}] {} {:?}", c.severity, c.id, c.title, c.objects);
    }
    checks
}

fn get<'a>(checks: &'a [HealthCheck], id: &str) -> &'a HealthCheck {
    checks.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("no {id} check"))
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn postgres_finds_what_was_broken() {
    let Some(cfg) = cfg("postgres", "DBINE_TEST_POSTGRES_URL") else {
        eprintln!("DBINE_TEST_POSTGRES_URL not set; skipping");
        return;
    };
    let d = driver("postgres");
    let mut m = d.connect(&cfg, None).await.unwrap();
    let _ = m.drop_database(DB).await;
    m.create_database(DB).await.unwrap();
    let mut s = d.connect(&cfg, Some(DB)).await.unwrap();
    run(&mut s, "CREATE TABLE clientes (id int PRIMARY KEY)").await;
    // No primary key, an unindexed foreign key, autovacuum off, two equal indexes.
    run(&mut s, "CREATE TABLE pedidos (id int, cliente_id int REFERENCES clientes(id)) WITH (autovacuum_enabled = off)").await;
    run(&mut s, "CREATE INDEX ix_a ON pedidos (id)").await;
    run(&mut s, "CREATE INDEX ix_b ON pedidos (id)").await;
    // Dead tuples on a table never analyzed.
    run(&mut s, "INSERT INTO pedidos SELECT g, NULL FROM generate_series(1, 5000) g").await;
    run(&mut s, "DELETE FROM pedidos WHERE id > 1000").await;
    // An index a failed CREATE INDEX CONCURRENTLY left invalid.
    run(&mut s, "CREATE TABLE codigos (v int)").await;
    run(&mut s, "INSERT INTO codigos VALUES (1), (1)").await;
    run_failing(&mut s, "CREATE UNIQUE INDEX CONCURRENTLY ux_codigos ON codigos (v)").await;
    // A sequence at 93 % of its range.
    run(&mut s, "CREATE TABLE tickets (id serial PRIMARY KEY)").await;
    run(&mut s, "SELECT setval('tickets_id_seq', 2000000000)").await;

    // The statistics reach the views when the writing backend flushes them.
    let mut all = Vec::new();
    for _ in 0..20 {
        let _ = s.execute("SELECT pg_stat_force_next_flush()", 1, &mut QueryOutcome::default()).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        all = checks(&mut s).await;
        if all.iter().any(|c| c.id == "dead_tuples" && c.severity == Severity::Warning) {
            break;
        }
    }
    assert_eq!(get(&all, "autovacuum").severity, Severity::Ok);
    assert!(get(&all, "autovacuum_tables").objects.contains(&"public.pedidos".to_string()));
    assert!(get(&all, "autovacuum_tables").fix.as_deref().unwrap().contains("ALTER TABLE \"public\".\"pedidos\" RESET (autovacuum_enabled);"));
    assert!(get(&all, "dead_tuples").objects.iter().any(|o| o.starts_with("public.pedidos (")));
    assert!(get(&all, "dead_tuples").fix.as_deref().unwrap().contains("VACUUM ANALYZE \"public\".\"pedidos\";"));
    assert!(get(&all, "never_analyzed").objects.iter().any(|o| o.starts_with("public.pedidos")));
    assert_eq!(get(&all, "xid_wraparound").severity, Severity::Ok);
    assert!(get(&all, "unused_indexes").objects.iter().any(|o| o.starts_with("public.pedidos · ix_a")));
    assert!(get(&all, "no_primary_key").objects.contains(&"public.pedidos".to_string()));
    assert!(get(&all, "fk_without_index").objects.iter().any(|o| o.starts_with("public.pedidos (cliente_id)")));
    assert!(get(&all, "fk_without_index").fix.as_deref().unwrap().contains("CREATE INDEX ON \"public\".\"pedidos\" (\"cliente_id\");"));
    assert!(get(&all, "duplicate_indexes").objects.contains(&"public.pedidos · ix_a = ix_b".to_string()));
    assert!(get(&all, "duplicate_indexes").fix.as_deref().unwrap().contains("DROP INDEX \"public\".\"ix_b\";"));
    assert!(get(&all, "invalid_indexes").objects.contains(&"public.codigos · ux_codigos".to_string()));
    let seq = get(&all, "sequences_near_limit");
    assert_eq!(seq.severity, Severity::Critical);
    assert!(seq.objects.iter().any(|o| o.starts_with("public.tickets_id_seq (93 % usado, de public.tickets.id")));

    // The fixes clear their findings: the sequence and the column to bigint,
    // the invalid index rebuilt once the duplicate is gone, the FK indexed,
    // the duplicate dropped.
    let seq_fix = seq.fix.clone().unwrap();
    assert!(seq_fix.contains("ALTER TABLE \"public\".\"tickets\" ALTER COLUMN \"id\" TYPE bigint;") && seq_fix.contains("ALTER SEQUENCE \"public\".\"tickets_id_seq\" AS bigint;"));
    for stmt in seq_fix.lines() {
        run(&mut s, stmt).await;
    }
    run(&mut s, "DELETE FROM codigos WHERE ctid = (SELECT max(ctid) FROM codigos)").await;
    run(&mut s, get(&all, "invalid_indexes").fix.clone().unwrap().as_str()).await;
    run(&mut s, get(&all, "fk_without_index").fix.clone().unwrap().as_str()).await;
    for stmt in get(&all, "duplicate_indexes").fix.clone().unwrap().lines().filter(|l| !l.starts_with("--")) {
        run(&mut s, stmt).await;
    }
    let again = checks(&mut s).await;
    for id in ["sequences_near_limit", "invalid_indexes", "fk_without_index", "duplicate_indexes"] {
        assert_eq!(get(&again, id).severity, Severity::Ok, "{id}");
    }

    drop(s);
    m.drop_database(DB).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn cockroach_finds_what_was_broken() {
    let Some(cfg) = cfg("cockroachdb", "DBINE_TEST_COCKROACH_URL") else {
        eprintln!("DBINE_TEST_COCKROACH_URL not set; skipping");
        return;
    };
    let d = driver("cockroachdb");
    let mut m = d.connect(&cfg, None).await.unwrap();
    let _ = m.drop_database(DB).await;
    m.create_database(DB).await.unwrap();
    let mut s = d.connect(&cfg, Some(DB)).await.unwrap();
    run(&mut s, "CREATE TABLE clientes (id INT PRIMARY KEY)").await;
    run(&mut s, "CREATE TABLE pedidos (id INT, cliente_id INT REFERENCES clientes(id))").await;
    run(&mut s, "CREATE INDEX ix_a ON pedidos (id)").await;
    run(&mut s, "CREATE INDEX ix_b ON pedidos (id)").await;

    let all = checks(&mut s).await;
    // Neither VACUUM nor transaction IDs: no PostgreSQL maintenance checks.
    assert!(all.iter().all(|c| !matches!(c.id.as_str(), "autovacuum" | "dead_tuples" | "xid_wraparound" | "invalid_indexes")));
    assert!(get(&all, "no_primary_key").objects.contains(&"public.pedidos".to_string()));
    assert!(!get(&all, "no_primary_key").objects.contains(&"public.clientes".to_string()));
    assert!(get(&all, "fk_without_index").objects.iter().any(|o| o.starts_with("public.pedidos (cliente_id)")));
    assert!(get(&all, "duplicate_indexes").objects.contains(&"public.pedidos · ix_a = ix_b".to_string()));
    assert!(get(&all, "unused_indexes").objects.iter().any(|o| o == "public.pedidos · ix_a"));

    run(&mut s, get(&all, "fk_without_index").fix.clone().unwrap().as_str()).await;
    for stmt in get(&all, "duplicate_indexes").fix.clone().unwrap().lines().filter(|l| !l.starts_with("--")) {
        run(&mut s, stmt).await;
    }
    let again = checks(&mut s).await;
    assert_eq!(get(&again, "fk_without_index").severity, Severity::Ok);
    assert_eq!(get(&again, "duplicate_indexes").severity, Severity::Ok);

    drop(s);
    m.drop_database(DB).await.unwrap();
}
