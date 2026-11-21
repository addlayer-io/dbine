//! The lineage of a whole database (`Session::lineage_graph`) against real
//! servers. Each test makes its own schema, builds the graph of that scope
//! and drops everything, also when a check fails. Skipped without the URL:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//!   cargo test -p dbine-driver-postgres --test lineage_graph -- --ignored --nocapture
//! ```

use dbine_driver::dependencies::{LineageGraph, LineageScope};
use dbine_driver::{
    Confidence, ConnectionConfig, DependencyScan, Driver, QueryOutcome, Relation, Session,
};
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

fn cfg(env: &str, driver: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        options: [("sslmode".to_string(), "disable".to_string())].into(),
        ..Default::default()
    })
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_postgres::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
}

async fn quiet(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(sql, 100, &mut out).await {
        eprintln!("cleanup: {sql}: {e}");
    }
}

async fn graph(d: &Arc<dyn Driver>, s: &mut Box<dyn Session>, schema: &str) -> LineageGraph {
    let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
    let scope = LineageScope {
        schemas: vec![schema.into()],
        ..Default::default()
    };
    let g = s
        .lineage_graph("", &scope, &scan)
        .await
        .unwrap_or_else(|e| panic!("lineage_graph: {e}"));
    for o in &g.objects {
        eprintln!("node {} {:?}.{}", o.kind, o.schema, o.name);
    }
    for e in &g.edges {
        eprintln!(
            "edge {:?} {:?} {} {} -> {} {} {:?} ({} lines)",
            e.confidence,
            e.relation,
            e.from.kind,
            e.from.name,
            e.to.kind,
            e.to.name,
            e.detail,
            e.mentions.len()
        );
    }
    for x in &g.external {
        eprintln!("external {:?} -> {}", x.source, x.to.name);
    }
    for n in &g.notes {
        eprintln!("note: {n}");
    }
    g
}

/// The edge `from` > `to` of that relation, if the graph has it.
fn edge<'a>(
    g: &'a LineageGraph,
    from: &str,
    to: &str,
    relation: Relation,
) -> Option<&'a dbine_driver::dependencies::LineageEdge> {
    g.edges
        .iter()
        .find(|e| e.from.name == from && e.to.name == to && e.relation == relation)
}

fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("{prefix}_{}_{nanos}", std::process::id())
}

#[tokio::test]
#[ignore = "needs DBINE_TEST_POSTGRES_URL"]
async fn postgres_whole_database_graph() {
    let Some(c) = cfg("DBINE_TEST_POSTGRES_URL", "postgres") else {
        eprintln!("DBINE_TEST_POSTGRES_URL not set; skipping");
        return;
    };
    let d = driver("postgres");
    let mut s = d.connect(&c, None).await.expect("connect");
    let sch = unique("dbine_test_lg");
    let body = async {
        run(&mut s, &format!("CREATE SCHEMA {sch}")).await;
        run(
            &mut s,
            &format!(
                "CREATE TABLE {sch}.clientes (id int PRIMARY KEY, nombre text);
                 CREATE TABLE {sch}.pedidos (id int PRIMARY KEY, cliente_id int REFERENCES {sch}.clientes(id), total numeric);
                 CREATE TABLE {sch}.aislada (id int);
                 CREATE VIEW {sch}.v_ventas AS SELECT p.id, c.nombre FROM {sch}.pedidos p JOIN {sch}.clientes c ON c.id = p.cliente_id;
                 CREATE MATERIALIZED VIEW {sch}.mv_ventas AS SELECT * FROM {sch}.v_ventas;
                 CREATE FUNCTION {sch}.total_cliente(int) RETURNS numeric LANGUAGE plpgsql AS $$
                   BEGIN
                     RETURN (SELECT sum(total) FROM {sch}.pedidos WHERE cliente_id = $1);
                   END $$;
                 CREATE FUNCTION {sch}.cuenta() RETURNS bigint LANGUAGE plpgsql AS $$
                   BEGIN
                     RETURN (SELECT count(*) FROM {sch}.v_ventas);
                   END $$;
                 CREATE FUNCTION {sch}.trg_fn() RETURNS trigger LANGUAGE plpgsql AS $$
                   BEGIN RETURN NEW; END $$;
                 CREATE TRIGGER trg_pedidos BEFORE INSERT ON {sch}.pedidos
                   FOR EACH ROW EXECUTE FUNCTION {sch}.trg_fn();
                 CREATE TABLE {sch}.con_default (id int DEFAULT {sch}.cuenta()::int);
                 CREATE TABLE {sch}.hija () INHERITS ({sch}.clientes);"
            ),
        )
        .await;
        let g = graph(&d, &mut s, &sch).await;

        // Every object is a node, also the one nothing touches.
        for n in [
            "clientes",
            "pedidos",
            "aislada",
            "v_ventas",
            "mv_ventas",
            "trg_pedidos",
        ] {
            assert!(g.objects.iter().any(|o| o.name == n), "node {n}");
        }
        // The catalog's edges are Confirmed.
        let fk = edge(&g, "clientes", "pedidos", Relation::ForeignKey).expect("fk");
        assert_eq!(fk.confidence, Confidence::Confirmed);
        for (from, to) in [
            ("pedidos", "v_ventas"),
            ("clientes", "v_ventas"),
            ("v_ventas", "mv_ventas"),
            ("pedidos", "trg_pedidos"),
            ("trg_fn", "trg_pedidos"),
            ("cuenta", "con_default"),
            ("clientes", "hija"),
        ] {
            let e =
                edge(&g, from, to, Relation::Code).unwrap_or_else(|| panic!("edge {from} > {to}"));
            assert_eq!(e.confidence, Confidence::Confirmed, "{from} > {to}");
        }
        // The line of the view that names the table comes with the edge.
        assert!(!edge(&g, "pedidos", "v_ventas", Relation::Code)
            .unwrap()
            .mentions
            .is_empty());
        // A PL/pgSQL body isn't in the catalog: parsed, probable, with lines.
        for (from, to) in [("pedidos", "total_cliente"), ("v_ventas", "cuenta")] {
            let e =
                edge(&g, from, to, Relation::Code).unwrap_or_else(|| panic!("edge {from} > {to}"));
            assert_eq!(e.confidence, Confidence::Probable, "{from} > {to}");
            assert!(!e.mentions.is_empty());
        }
        // Nothing points at the isolated table.
        assert!(!g
            .edges
            .iter()
            .any(|e| e.from.name == "aislada" || e.to.name == "aislada"));
        // The scope keeps only its schema.
        assert!(g
            .objects
            .iter()
            .all(|o| o.schema.as_deref() == Some(sch.as_str())));
    };
    let r = AssertUnwindSafe(body).catch_unwind().await;
    quiet(&mut s, &format!("DROP SCHEMA IF EXISTS {sch} CASCADE")).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e);
    }
}

#[tokio::test]
#[ignore = "needs DBINE_TEST_POSTGRES_URL"]
async fn postgres_foreign_table_is_external_and_the_scope_limits() {
    let Some(c) = cfg("DBINE_TEST_POSTGRES_URL", "postgres") else {
        return;
    };
    let d = driver("postgres");
    let mut s = d.connect(&c, None).await.expect("connect");
    let sch = unique("dbine_test_lf");
    let srv = unique("dbine_test_srv");
    let body = async {
        run(&mut s, &format!("CREATE SCHEMA {sch}")).await;
        let mut out = QueryOutcome::default();
        if s.execute("CREATE EXTENSION IF NOT EXISTS postgres_fdw", 10, &mut out)
            .await
            .is_err()
            || out.error.is_some()
        {
            eprintln!("postgres_fdw is not available; skipping the foreign table");
            return;
        }
        run(
            &mut s,
            &format!(
                "CREATE SERVER {srv} FOREIGN DATA WRAPPER postgres_fdw OPTIONS (host 'otro.host', port '5432', dbname 'remota');
                 CREATE FOREIGN TABLE {sch}.remota (id int) SERVER {srv} OPTIONS (schema_name 'public', table_name 'datos');
                 CREATE TABLE {sch}.local (id int);
                 CREATE VIEW {sch}.v AS SELECT * FROM {sch}.remota"
            ),
        )
        .await;
        let g = graph(&d, &mut s, &sch).await;
        let x = g
            .external
            .iter()
            .find(|x| x.to.name == "v")
            .expect("the foreign table's source");
        assert_eq!(x.source.system.as_deref(), Some("postgres_fdw"));
        assert_eq!(x.source.database.as_deref(), Some("remota"));
        assert_eq!(x.source.name.as_deref(), Some("datos"));
        // A limit truncates and says so.
        let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
        let few = LineageScope {
            schemas: vec![sch.clone()],
            limit: 1,
            ..Default::default()
        };
        let g = s.lineage_graph("", &few, &scan).await.unwrap();
        assert!(g.truncated && g.objects.len() == 1);
    };
    let r = AssertUnwindSafe(body).catch_unwind().await;
    quiet(&mut s, &format!("DROP SCHEMA IF EXISTS {sch} CASCADE")).await;
    quiet(&mut s, &format!("DROP SERVER IF EXISTS {srv} CASCADE")).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e);
    }
}

#[tokio::test]
#[ignore = "needs DBINE_TEST_COCKROACH_URL"]
async fn cockroach_whole_database_graph() {
    let Some(c) = cfg("DBINE_TEST_COCKROACH_URL", "cockroachdb") else {
        eprintln!("DBINE_TEST_COCKROACH_URL not set; skipping");
        return;
    };
    let d = driver("cockroachdb");
    let mut s = d.connect(&c, None).await.expect("connect");
    let sch = unique("dbine_test_lg");
    let body = async {
        run(&mut s, &format!("CREATE SCHEMA {sch}")).await;
        run(
            &mut s,
            &format!(
                "CREATE TABLE {sch}.clientes (id INT PRIMARY KEY, nombre STRING);
                 CREATE TABLE {sch}.pedidos (id INT PRIMARY KEY, cliente_id INT REFERENCES {sch}.clientes(id));
                 CREATE VIEW {sch}.v_ventas AS SELECT p.id, c.nombre FROM {sch}.pedidos p JOIN {sch}.clientes c ON c.id = p.cliente_id;
                 CREATE VIEW {sch}.v_doble AS SELECT * FROM {sch}.v_ventas"
            ),
        )
        .await;
        let g = graph(&d, &mut s, &sch).await;
        assert_eq!(
            edge(&g, "clientes", "pedidos", Relation::ForeignKey)
                .expect("fk")
                .confidence,
            Confidence::Confirmed
        );
        for (from, to) in [
            ("pedidos", "v_ventas"),
            ("clientes", "v_ventas"),
            ("v_ventas", "v_doble"),
        ] {
            assert_eq!(
                edge(&g, from, to, Relation::Code)
                    .unwrap_or_else(|| panic!("{from} > {to}"))
                    .confidence,
                Confidence::Confirmed
            );
        }
    };
    let r = AssertUnwindSafe(body).catch_unwind().await;
    quiet(&mut s, &format!("DROP SCHEMA IF EXISTS {sch} CASCADE")).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e);
    }
}

/// With `scope.jobs`, the pg_cron jobs and the objects their commands name.
/// Needs the pg_cron server of tests/jobs.rs (`DBINE_TEST_PGCRON_URL`).
#[tokio::test]
#[ignore = "needs DBINE_TEST_PGCRON_URL"]
async fn pg_cron_jobs_are_in_the_graph_when_the_scope_asks() {
    let Some(c) = cfg("DBINE_TEST_PGCRON_URL", "postgres") else {
        eprintln!("DBINE_TEST_PGCRON_URL not set; skipping");
        return;
    };
    let d = driver("postgres");
    let mut s = d.connect(&c, None).await.expect("connect");
    let sch = unique("dbine_test_lj");
    let job = format!("{sch}_job");
    let body = async {
        run(&mut s, &format!("CREATE SCHEMA {sch}")).await;
        run(
            &mut s,
            &format!(
                "CREATE TABLE {sch}.t (id int);
                 CREATE PROCEDURE {sch}.carga() LANGUAGE sql AS $$ SELECT 1 $$;
                 SELECT cron.schedule('{job}', '0 3 * * *', 'CALL {sch}.carga()')"
            ),
        )
        .await;
        let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
        let mut scope = LineageScope {
            schemas: vec![sch.clone()],
            ..Default::default()
        };
        // Without the flag the jobs are not read.
        let g = s.lineage_graph(&c.database, &scope, &scan).await.unwrap();
        assert!(g.jobs.is_empty());
        scope.jobs = true;
        let g = s.lineage_graph(&c.database, &scope, &scan).await.unwrap();
        let j = g
            .jobs
            .iter()
            .find(|j| j.name == job)
            .unwrap_or_else(|| panic!("job {job}: {:?} {:?}", g.jobs, g.job_notes));
        assert_eq!(j.kind, "pg_cron");
        assert!(j.uses.iter().any(|u| u.name == "carga"), "{:?}", j.uses);
    };
    let r = AssertUnwindSafe(body).catch_unwind().await;
    quiet(
        &mut s,
        &format!("SELECT cron.unschedule(jobid) FROM cron.job WHERE jobname = '{job}'"),
    )
    .await;
    quiet(&mut s, &format!("DROP SCHEMA IF EXISTS {sch} CASCADE")).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e);
    }
}
