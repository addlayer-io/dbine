//! What an object uses (`Session::references`) against real servers: each
//! test makes its own schema (and, where the engine has them, another
//! database and a foreign table), checks what the catalog answers for a
//! view, a materialized view, routines, a trigger and tables, and drops
//! everything, also when a check fails. Each one is skipped without its
//! URL:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//! DBINE_TEST_YUGABYTE_URL=postgres://yugabyte@localhost:25016/yugabyte \
//! DBINE_TEST_COCKROACH_URL=postgres://root@localhost:26014/defaultdb \
//! DBINE_TEST_MATERIALIZE_URL=postgres://materialize@localhost:25024/materialize \
//! DBINE_TEST_RISINGWAVE_URL=postgres://root@localhost:25023/dev \
//! DBINE_TEST_OPENGAUSS_URL='postgres://gaussdb:Dbine@1234@localhost:25020/postgres' \
//!   cargo test -p dbine-driver-postgres --test references -- --ignored --nocapture
//! ```

use dbine_driver::{
    Confidence, ConnectionConfig, DependencyReport, DependencyScan, DependencyTarget, Dependent,
    Driver, ObjectRef, QueryOutcome, Relation, Session,
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

/// Runs `sql` and ignores a failure (cleanup).
async fn quiet(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(sql, 100, &mut out).await {
        eprintln!("cleanup: {sql}: {e}");
    }
}

/// A name nobody else uses at the same time.
fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("{prefix}_{}_{nanos}", std::process::id())
}

async fn refs(
    d: &Arc<dyn Driver>,
    s: &mut Box<dyn Session>,
    kind: &str,
    schema: &str,
    name: &str,
) -> DependencyReport {
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
            "  {:?} {:?} {} {:?}.{} {:?} ({} lines)",
            i.confidence,
            i.relation,
            i.kind,
            i.schema,
            i.name,
            i.detail,
            i.mentions.len()
        );
    }
    for x in &r.external {
        eprintln!("  external {x:?}");
    }
    if let Some(n) = &r.note {
        eprintln!("  note: {n}");
    }
    r
}

/// The items for `kind` `name`.
fn items<'a>(r: &'a DependencyReport, kind: &str, name: &str) -> Vec<&'a Dependent> {
    r.items
        .iter()
        .filter(|i| i.kind == kind && i.name == name)
        .collect()
}

/// The one item for `kind` `name`, with that relation and confidence.
fn one<'a>(
    r: &'a DependencyReport,
    kind: &str,
    name: &str,
    relation: Relation,
    confidence: Confidence,
) -> &'a Dependent {
    let found: Vec<_> = items(r, kind, name)
        .into_iter()
        .filter(|i| i.relation == relation)
        .collect();
    assert_eq!(found.len(), 1, "{kind} {name} {relation:?}: {:?}", r.items);
    assert_eq!(found[0].confidence, confidence, "{kind} {name}");
    found[0]
}

// -- PostgreSQL and the variants that keep its catalog --------------------------------------

/// The PostgreSQL catalog path. `fdw`: postgres_fdw is installed or can
/// be, and the foreign table's remote database is created.
async fn postgres_family(env: &str, id: &str, fdw: bool) {
    let Some(c) = cfg(env, id) else {
        return;
    };
    let d = driver(id);
    let mut s = d.connect(&c, None).await.unwrap();
    let sch = unique("dbine_refs");
    let remote = format!("{sch}_remota");
    let server = format!("{sch}_srv");
    // Only the extension this test installs is dropped at the end.
    let mut own_fdw = false;
    if fdw {
        let mut out = QueryOutcome::default();
        s.execute(
            "SELECT count(*) FROM pg_extension WHERE extname = 'postgres_fdw'",
            10,
            &mut out,
        )
        .await
        .unwrap();
        let had = out.results[0].rows[0][0].to_string().trim_matches('"') != "0";
        own_fdw = !had;
    }
    let r = AssertUnwindSafe(postgres_checks(
        &d, &mut s, &c, &sch, &remote, &server, fdw, own_fdw,
    ))
    .catch_unwind()
    .await;
    quiet(&mut s, &format!("DROP SCHEMA IF EXISTS {sch} CASCADE")).await;
    if fdw {
        quiet(&mut s, &format!("DROP SERVER IF EXISTS {server} CASCADE")).await;
        if own_fdw {
            quiet(&mut s, "DROP EXTENSION IF EXISTS postgres_fdw").await;
        }
        // The remote's connection may still be closing.
        quiet(
            &mut s,
            &format!("DROP DATABASE IF EXISTS {remote} WITH (FORCE)"),
        )
        .await;
        quiet(&mut s, &format!("DROP DATABASE IF EXISTS {remote}")).await;
    }
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}

#[allow(clippy::too_many_arguments)]
async fn postgres_checks(
    d: &Arc<dyn Driver>,
    s: &mut Box<dyn Session>,
    c: &ConnectionConfig,
    sch: &str,
    remote: &str,
    server: &str,
    fdw: bool,
    own_fdw: bool,
) {
    run(s, &format!("CREATE SCHEMA {sch}")).await;
    for stmt in [
        "CREATE TABLE {s}.clientes (id serial PRIMARY KEY, nombre text)",
        "CREATE SEQUENCE {s}.seq_ped",
        "CREATE FUNCTION {s}.positivo(x int) RETURNS boolean LANGUAGE sql IMMUTABLE RETURN x > 0",
        "CREATE TABLE {s}.pedidos (id int PRIMARY KEY DEFAULT nextval('{s}.seq_ped'),
            cliente_id int REFERENCES {s}.clientes (id), total numeric CHECK ({s}.positivo(total::int)))",
        "CREATE INDEX ix_pedidos_positivo ON {s}.pedidos (({s}.positivo(cliente_id)))",
        "CREATE VIEW {s}.v_pedidos AS
           SELECT p.id, c.nombre, {s}.positivo(p.id) AS ok
           FROM {s}.pedidos p JOIN {s}.clientes c ON c.id = p.cliente_id",
        "CREATE MATERIALIZED VIEW {s}.mv_pedidos AS SELECT * FROM {s}.v_pedidos",
        "CREATE FUNCTION {s}.contar() RETURNS bigint LANGUAGE sql BEGIN ATOMIC SELECT count(*) FROM {s}.v_pedidos; END",
        "CREATE FUNCTION {s}.listar() RETURNS SETOF {s}.pedidos LANGUAGE plpgsql AS $$
         BEGIN
           PERFORM 1 FROM {s}.clientes;
           RETURN QUERY SELECT * FROM {s}.pedidos;
         END $$",
        "CREATE FUNCTION {s}.auditar() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN INSERT INTO {s}.clientes (nombre) VALUES ('x'); RETURN NEW; END $$",
        "CREATE TRIGGER trg_pedidos AFTER INSERT ON {s}.pedidos FOR EACH ROW
           WHEN ({s}.positivo(NEW.id)) EXECUTE FUNCTION {s}.auditar()",
        "CREATE TABLE {s}.ventas (id int, dia date) PARTITION BY RANGE (dia)",
        "CREATE TABLE {s}.ventas_2026 PARTITION OF {s}.ventas FOR VALUES FROM ('2026-01-01') TO ('2027-01-01')",
    ] {
        run(s, &stmt.replace("{s}", sch)).await;
    }
    if fdw {
        run(s, &format!("CREATE DATABASE {remote}")).await;
        let mut rc = c.clone();
        rc.database = remote.into();
        let mut rs = d.connect(&rc, None).await.unwrap();
        run(&mut rs, "CREATE TABLE public.remota (id int, valor text)").await;
        drop(rs);
        if own_fdw {
            run(s, &format!("CREATE EXTENSION postgres_fdw SCHEMA {sch}")).await;
        }
        run(
            s,
            &format!(
                "CREATE SERVER {server} FOREIGN DATA WRAPPER postgres_fdw
                 OPTIONS (host 'localhost', port '5432', dbname '{remote}')"
            ),
        )
        .await;
        run(
            s,
            &format!(
                "CREATE FOREIGN TABLE {sch}.ft_remota (id int, valor text) SERVER {server}
                 OPTIONS (schema_name 'public', table_name 'remota')"
            ),
        )
        .await;
        run(
            s,
            &format!("CREATE VIEW {sch}.v_remota AS SELECT id FROM {sch}.ft_remota"),
        )
        .await;
    }

    // A view: its tables and the function it calls, from pg_rewrite.
    let r = refs(d, s, "view", sch, "v_pedidos").await;
    assert_eq!(r.items.len(), 3, "{:?}", r.items);
    let p = one(
        &r,
        "table",
        "pedidos",
        Relation::Code,
        Confidence::Confirmed,
    );
    assert!(
        p.mentions.iter().any(|m| m.text.contains("pedidos p")),
        "{:?}",
        p.mentions
    );
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
    assert!(r.note.is_none() && r.external.is_empty());

    let r = refs(d, s, "materialized_view", sch, "mv_pedidos").await;
    assert_eq!(r.items.len(), 1, "{:?}", r.items);
    one(
        &r,
        "view",
        "v_pedidos",
        Relation::Code,
        Confidence::Confirmed,
    );

    // A SQL-standard body: recorded, nothing else.
    let r = refs(d, s, "function", sch, "contar").await;
    assert_eq!(r.items.len(), 1, "{:?}", r.items);
    one(
        &r,
        "view",
        "v_pedidos",
        Relation::Code,
        Confidence::Confirmed,
    );

    // PL/pgSQL: its row type from the catalog, the body's names by text.
    let r = refs(d, s, "function", sch, "listar").await;
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
        Confidence::Probable,
    );

    // A trigger: its table, its function and what its WHEN calls.
    let r = refs(d, s, "trigger", sch, "trg_pedidos").await;
    assert_eq!(r.items.len(), 3, "{:?}", r.items);
    one(
        &r,
        "table",
        "pedidos",
        Relation::Code,
        Confidence::Confirmed,
    );
    one(
        &r,
        "function",
        "auditar",
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

    // A table: its foreign key's table, the sequence of its default, the
    // function of its check and of its index.
    let r = refs(d, s, "table", sch, "pedidos").await;
    let fk = one(
        &r,
        "table",
        "clientes",
        Relation::ForeignKey,
        Confidence::Confirmed,
    );
    assert!(
        fk.detail
            .as_deref()
            .is_some_and(|x| x.contains("(cliente_id) →")),
        "{:?}",
        fk.detail
    );
    let seq = one(
        &r,
        "sequence",
        "seq_ped",
        Relation::Code,
        Confidence::Confirmed,
    );
    assert!(
        seq.detail
            .as_deref()
            .is_some_and(|x| x.starts_with("id DEFAULT nextval(")),
        "{:?}",
        seq.detail
    );
    one(
        &r,
        "function",
        "positivo",
        Relation::Check,
        Confidence::Confirmed,
    );
    let ix = one(
        &r,
        "function",
        "positivo",
        Relation::Index,
        Confidence::Confirmed,
    );
    assert_eq!(ix.detail.as_deref(), Some("ix_pedidos_positivo"));
    assert_eq!(r.items.len(), 4, "{:?}", r.items);

    // serial: the sequence once (its default), not again as OWNED BY.
    let r = refs(d, s, "table", sch, "clientes").await;
    assert_eq!(r.items.len(), 1, "{:?}", r.items);
    one(
        &r,
        "sequence",
        "clientes_id_seq",
        Relation::Code,
        Confidence::Confirmed,
    );

    let r = refs(d, s, "table", sch, "ventas_2026").await;
    let parent = one(&r, "table", "ventas", Relation::Code, Confidence::Confirmed);
    assert_eq!(parent.detail.as_deref(), Some("PARTITION OF"));

    if fdw {
        // The foreign table's remote source, and the view that reads it.
        let r = refs(d, s, "table", sch, "ft_remota").await;
        assert_eq!(r.external.len(), 1, "{:?}", r.external);
        let x = &r.external[0];
        assert_eq!(x.system.as_deref(), Some("postgres_fdw"));
        assert_eq!(x.server.as_deref(), Some("localhost:5432"));
        assert_eq!(x.database.as_deref(), Some(remote));
        assert_eq!(x.schema.as_deref(), Some("public"));
        assert_eq!(x.name.as_deref(), Some("remota"));
        assert_eq!(x.detail.as_deref(), Some(server));

        let r = refs(d, s, "view", sch, "v_remota").await;
        let ft = one(
            &r,
            "table",
            "ft_remota",
            Relation::Code,
            Confidence::Confirmed,
        );
        assert_eq!(
            ft.detail.as_deref(),
            Some(format!("postgres_fdw · {server}").as_str())
        );
    }
}

#[tokio::test]
#[ignore]
async fn postgres_references() {
    postgres_family("DBINE_TEST_POSTGRES_URL", "postgres", true).await;
}

#[tokio::test]
#[ignore]
async fn yugabyte_references() {
    postgres_family("DBINE_TEST_YUGABYTE_URL", "yugabytedb", true).await;
}

/// openGauss reports 9.2: the catalog query without what came later
/// (`prokind`, policies, generated columns, `relispartition`).
#[tokio::test]
#[ignore]
async fn opengauss_references() {
    let Some(c) = cfg("DBINE_TEST_OPENGAUSS_URL", "opengauss") else {
        return;
    };
    let d = driver("opengauss");
    let mut s = d.connect(&c, None).await.unwrap();
    let sch = unique("dbine_refs");
    let r = AssertUnwindSafe(async {
        run(&mut s, &format!("CREATE SCHEMA {sch}")).await;
        for stmt in [
            "CREATE TABLE {s}.clientes (id int PRIMARY KEY, nombre text)",
            "CREATE SEQUENCE {s}.seq_ped",
            "CREATE TABLE {s}.pedidos (id int PRIMARY KEY DEFAULT nextval('{s}.seq_ped'),
                cliente_id int REFERENCES {s}.clientes (id), total numeric)",
            "CREATE VIEW {s}.v_pedidos AS
               SELECT p.id, c.nombre FROM {s}.pedidos p JOIN {s}.clientes c ON c.id = p.cliente_id",
            "CREATE FUNCTION {s}.contar() RETURNS bigint LANGUAGE sql AS 'SELECT count(*) FROM {s}.v_pedidos'",
            "CREATE FUNCTION {s}.auditar() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
            "CREATE TRIGGER trg_pedidos AFTER INSERT ON {s}.pedidos FOR EACH ROW EXECUTE PROCEDURE {s}.auditar()",
        ] {
            run(&mut s, &stmt.replace("{s}", &sch)).await;
        }

        let r = refs(&d, &mut s, "view", &sch, "v_pedidos").await;
        assert_eq!(r.items.len(), 2, "{:?}", r.items);
        one(&r, "table", "pedidos", Relation::Code, Confidence::Confirmed);
        one(&r, "table", "clientes", Relation::Code, Confidence::Confirmed);
        assert!(r.note.is_none(), "{:?}", r.note);

        // A string body: by its text.
        let r = refs(&d, &mut s, "function", &sch, "contar").await;
        one(&r, "view", "v_pedidos", Relation::Code, Confidence::Probable);

        let r = refs(&d, &mut s, "trigger", &sch, "trg_pedidos").await;
        one(&r, "table", "pedidos", Relation::Code, Confidence::Confirmed);
        one(&r, "function", "auditar", Relation::Code, Confidence::Confirmed);

        let r = refs(&d, &mut s, "table", &sch, "pedidos").await;
        one(&r, "table", "clientes", Relation::ForeignKey, Confidence::Confirmed);
        one(&r, "sequence", "seq_ped", Relation::Code, Confidence::Confirmed);
        assert!(r.note.is_none(), "{:?}", r.note);
    })
    .catch_unwind()
    .await;
    quiet(&mut s, &format!("DROP SCHEMA IF EXISTS {sch} CASCADE")).await;
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}

// -- CockroachDB ----------------------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn cockroach_references() {
    let Some(c) = cfg("DBINE_TEST_COCKROACH_URL", "cockroachdb") else {
        return;
    };
    let d = driver("cockroachdb");
    let mut s = d.connect(&c, None).await.unwrap();
    let sch = unique("dbine_refs");
    let other = format!("{sch}_otra");
    // Cross-database views are off by default: on for the test only.
    let mut out = QueryOutcome::default();
    s.execute(
        "SHOW CLUSTER SETTING sql.cross_db_views.enabled",
        10,
        &mut out,
    )
    .await
    .unwrap();
    let was = out.results[0].rows[0][0].clone();
    let r = AssertUnwindSafe(cockroach_checks(&d, &mut s, &sch, &other))
        .catch_unwind()
        .await;
    quiet(&mut s, &format!("DROP SCHEMA IF EXISTS {sch} CASCADE")).await;
    quiet(&mut s, &format!("DROP DATABASE IF EXISTS {other} CASCADE")).await;
    let was =
        matches!(was, serde_json::Value::Bool(true)) || was.as_str().is_some_and(|v| v == "true");
    quiet(
        &mut s,
        &format!("SET CLUSTER SETTING sql.cross_db_views.enabled = {was}"),
    )
    .await;
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}

async fn cockroach_checks(d: &Arc<dyn Driver>, s: &mut Box<dyn Session>, sch: &str, other: &str) {
    run(s, &format!("CREATE SCHEMA {sch}")).await;
    for stmt in [
        "CREATE TABLE {s}.clientes (id INT PRIMARY KEY, nombre STRING)",
        "CREATE SEQUENCE {s}.seq_ped",
        "CREATE TABLE {s}.pedidos (id INT PRIMARY KEY DEFAULT nextval('{s}.seq_ped'),
            cliente_id INT REFERENCES {s}.clientes (id), total DECIMAL)",
        "CREATE FUNCTION {s}.positivo(x INT) RETURNS BOOL LANGUAGE SQL IMMUTABLE AS $$ SELECT x > 0 $$",
        "CREATE VIEW {s}.v_pedidos AS
           SELECT p.id, c.nombre, {s}.positivo(p.id) AS ok
           FROM {s}.pedidos p JOIN {s}.clientes c ON c.id = p.cliente_id",
        "CREATE MATERIALIZED VIEW {s}.mv_pedidos AS SELECT * FROM {s}.v_pedidos",
        "CREATE FUNCTION {s}.contar() RETURNS INT LANGUAGE SQL AS $$ SELECT count(*)::INT FROM {s}.v_pedidos $$",
    ] {
        run(s, &stmt.replace("{s}", sch)).await;
    }
    run(s, &format!("CREATE DATABASE {other}")).await;
    run(
        s,
        &format!("CREATE TABLE {other}.public.remota (id INT PRIMARY KEY)"),
    )
    .await;
    run(s, "SET CLUSTER SETTING sql.cross_db_views.enabled = true").await;
    // The setting reaches the SQL layer a moment later.
    let cross = format!("CREATE VIEW {sch}.v_remota AS SELECT id FROM {other}.public.remota");
    let mut made = false;
    for _ in 0..50 {
        let mut out = QueryOutcome::default();
        if s.execute(&cross, 10, &mut out).await.is_ok() && out.error.is_none() {
            made = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(made, "{cross}");

    let r = refs(d, s, "view", sch, "v_pedidos").await;
    one(
        &r,
        "table",
        "pedidos",
        Relation::Code,
        Confidence::Confirmed,
    );
    one(
        &r,
        "table",
        "clientes",
        Relation::Code,
        Confidence::Confirmed,
    );
    // Not in backward_dependencies: the text finds it.
    one(
        &r,
        "function",
        "positivo",
        Relation::Code,
        Confidence::Probable,
    );
    assert!(r.note.is_none(), "{:?}", r.note);

    let r = refs(d, s, "materialized_view", sch, "mv_pedidos").await;
    one(
        &r,
        "view",
        "v_pedidos",
        Relation::Code,
        Confidence::Confirmed,
    );

    let r = refs(d, s, "table", sch, "pedidos").await;
    let fk = one(
        &r,
        "table",
        "clientes",
        Relation::ForeignKey,
        Confidence::Confirmed,
    );
    assert!(fk
        .detail
        .as_deref()
        .is_some_and(|x| x.contains("(cliente_id) →")));
    one(
        &r,
        "sequence",
        "seq_ped",
        Relation::Code,
        Confidence::Confirmed,
    );

    let r = refs(d, s, "function", sch, "contar").await;
    one(
        &r,
        "view",
        "v_pedidos",
        Relation::Code,
        Confidence::Probable,
    );

    let r = refs(d, s, "view", sch, "v_remota").await;
    assert!(r.items.is_empty(), "{:?}", r.items);
    assert_eq!(r.external.len(), 1, "{:?}", r.external);
    let x = &r.external[0];
    assert_eq!(
        (
            x.system.as_deref(),
            x.database.as_deref(),
            x.schema.as_deref(),
            x.name.as_deref()
        ),
        (
            Some("CockroachDB"),
            Some(other),
            Some("public"),
            Some("remota")
        )
    );
}

// -- Materialize and RisingWave -------------------------------------------------------------

#[tokio::test]
#[ignore]
async fn materialize_references() {
    let Some(c) = cfg("DBINE_TEST_MATERIALIZE_URL", "materialize") else {
        return;
    };
    let d = driver("materialize");
    let mut s = d.connect(&c, None).await.unwrap();
    let sch = unique("dbine_refs");
    let other = format!("{sch}_otra");
    let r = AssertUnwindSafe(async {
        run(&mut s, &format!("CREATE SCHEMA {sch}")).await;
        for stmt in [
            "CREATE TABLE {s}.clientes (id int, nombre text)",
            "CREATE TABLE {s}.pedidos (id int, cliente_id int)",
            "CREATE VIEW {s}.v_pedidos AS SELECT p.id, c.nombre FROM {s}.pedidos p JOIN {s}.clientes c ON c.id = p.cliente_id",
            "CREATE MATERIALIZED VIEW {s}.mv_pedidos AS SELECT count(*) AS n FROM {s}.v_pedidos",
        ] {
            run(&mut s, &stmt.replace("{s}", &sch)).await;
        }
        run(&mut s, &format!("CREATE DATABASE {other}")).await;
        run(&mut s, &format!("CREATE TABLE {other}.public.remota (id int)")).await;
        run(
            &mut s,
            &format!("CREATE VIEW {sch}.v_remota AS SELECT id FROM {other}.public.remota"),
        )
        .await;

        let r = refs(&d, &mut s, "view", &sch, "v_pedidos").await;
        assert_eq!(r.items.len(), 2, "{:?}", r.items);
        let p = one(&r, "table", "pedidos", Relation::Code, Confidence::Confirmed);
        assert!(!p.mentions.is_empty());
        one(&r, "table", "clientes", Relation::Code, Confidence::Confirmed);

        // count() is the system's: left out.
        let r = refs(&d, &mut s, "materialized_view", &sch, "mv_pedidos").await;
        assert_eq!(r.items.len(), 1, "{:?}", r.items);
        one(&r, "view", "v_pedidos", Relation::Code, Confidence::Confirmed);

        let r = refs(&d, &mut s, "view", &sch, "v_remota").await;
        assert!(r.items.is_empty(), "{:?}", r.items);
        assert_eq!(r.external.len(), 1);
        let x = &r.external[0];
        assert_eq!(
            (
                x.system.as_deref(),
                x.database.as_deref(),
                x.schema.as_deref(),
                x.name.as_deref()
            ),
            (Some("Materialize"), Some(other.as_str()), Some("public"), Some("remota"))
        );
    })
    .catch_unwind()
    .await;
    quiet(&mut s, &format!("DROP SCHEMA IF EXISTS {sch} CASCADE")).await;
    quiet(&mut s, &format!("DROP DATABASE IF EXISTS {other} CASCADE")).await;
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}

#[tokio::test]
#[ignore]
async fn risingwave_references() {
    let Some(c) = cfg("DBINE_TEST_RISINGWAVE_URL", "risingwave") else {
        return;
    };
    let d = driver("risingwave");
    let mut s = d.connect(&c, None).await.unwrap();
    let sch = unique("dbine_refs");
    let r = AssertUnwindSafe(async {
        run(&mut s, &format!("CREATE SCHEMA {sch}")).await;
        for stmt in [
            "CREATE TABLE {s}.clientes (id int, nombre varchar)",
            "CREATE TABLE {s}.pedidos (id int, cliente_id int)",
            "CREATE VIEW {s}.v_pedidos AS SELECT p.id, c.nombre FROM {s}.pedidos p JOIN {s}.clientes c ON c.id = p.cliente_id",
            "CREATE MATERIALIZED VIEW {s}.mv_pedidos AS SELECT count(*) AS n FROM {s}.v_pedidos",
            "CREATE SINK {s}.snk FROM {s}.mv_pedidos WITH (connector = 'blackhole')",
            "CREATE SOURCE {s}.src_gen (id int) WITH (connector = 'datagen', fields.id.kind = 'sequence',
               fields.id.start = '1', fields.id.end = '10') FORMAT PLAIN ENCODE JSON",
            "CREATE MATERIALIZED VIEW {s}.mv_gen AS SELECT id FROM {s}.src_gen",
        ] {
            run(&mut s, &stmt.replace("{s}", &sch)).await;
        }

        let r = refs(&d, &mut s, "view", &sch, "v_pedidos").await;
        assert_eq!(r.items.len(), 2, "{:?}", r.items);
        one(&r, "table", "pedidos", Relation::Code, Confidence::Confirmed);
        one(&r, "table", "clientes", Relation::Code, Confidence::Confirmed);

        let r = refs(&d, &mut s, "materialized_view", &sch, "mv_pedidos").await;
        one(&r, "view", "v_pedidos", Relation::Code, Confidence::Confirmed);

        let r = refs(&d, &mut s, "sink", &sch, "snk").await;
        one(
            &r,
            "materialized_view",
            "mv_pedidos",
            Relation::Code,
            Confidence::Confirmed,
        );

        let r = refs(&d, &mut s, "materialized_view", &sch, "mv_gen").await;
        one(&r, "source", "src_gen", Relation::Code, Confidence::Confirmed);

        let r = refs(&d, &mut s, "source", &sch, "src_gen").await;
        assert_eq!(r.external.len(), 1, "{:?}", r.external);
        assert_eq!(r.external[0].system.as_deref(), Some("datagen"));
    })
    .catch_unwind()
    .await;
    quiet(&mut s, &format!("DROP SCHEMA IF EXISTS {sch} CASCADE")).await;
    if let Err(p) = r {
        std::panic::resume_unwind(p);
    }
}
