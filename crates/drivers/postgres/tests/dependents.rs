//! What depends on an object (`Session::dependents`) against a real server:
//! the test makes its own schema with a table, the objects that read it
//! (views, a materialized view, routines of every kind of body, triggers,
//! a policy, an inheriting table, foreign keys, defaults, checks, indexes),
//! checks what the catalog answers for the table, a column, a view, a
//! sequence and a function, compares it with the generic scan's and drops
//! everything, also when a check fails. Skipped without its URL:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//!   cargo test -p dbine-driver-postgres --test dependents -- --ignored --nocapture
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

async fn quiet(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(sql, 100, &mut out).await {
        eprintln!("cleanup: {sql}: {e}");
    }
}

fn unique(prefix: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    format!("{prefix}_{}_{nanos}", std::process::id())
}

fn target(kind: &str, schema: &str, name: &str, column: Option<&str>) -> DependencyTarget {
    DependencyTarget {
        object: ObjectRef {
            kind: kind.into(),
            schema: Some(schema.into()),
            name: name.into(),
        },
        column: column.map(Into::into),
    }
}

/// What the native path answers for `t`.
async fn deps(
    d: &Arc<dyn Driver>,
    s: &mut Box<dyn Session>,
    t: &DependencyTarget,
) -> DependencyReport {
    let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
    let r = s
        .dependents(t, &scan)
        .await
        .unwrap_or_else(|e| panic!("{t:?}: {e}"));
    eprintln!("{} {:?}:", t.object.name, t.column);
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
    assert!(r.note.is_none(), "native path expected: {:?}", r.note);
    r
}

fn items<'a>(r: &'a DependencyReport, kind: &str, name: &str) -> Vec<&'a Dependent> {
    r.items
        .iter()
        .filter(|i| i.kind == kind && i.name == name)
        .collect()
}

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

fn absent(r: &DependencyReport, kind: &str, name: &str) {
    assert!(
        items(r, kind, name).is_empty(),
        "{kind} {name}: {:?}",
        r.items
    );
}

async fn checks(d: &Arc<dyn Driver>, s: &mut Box<dyn Session>, sch: &str) {
    for stmt in [
        "CREATE TABLE {s}.clientes (id int PRIMARY KEY, nombre text, email text UNIQUE)",
        "CREATE SEQUENCE {s}.seq_ped",
        "CREATE FUNCTION {s}.positivo(n numeric) RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT n >= 0'",
        "CREATE TABLE {s}.pedidos (id int PRIMARY KEY DEFAULT nextval('{s}.seq_ped'),
            cliente_id int REFERENCES {s}.clientes (id), total numeric CHECK ({s}.positivo(total)))",
        "CREATE INDEX ix_clientes_nombre ON {s}.clientes (nombre)",
        "CREATE INDEX ix_pedidos_positivo ON {s}.pedidos (({s}.positivo(total)))",
        "CREATE VIEW {s}.v_clientes AS SELECT id, nombre FROM {s}.clientes",
        "CREATE VIEW {s}.v_de_vista AS SELECT nombre FROM {s}.v_clientes",
        "CREATE MATERIALIZED VIEW {s}.mv_ids AS SELECT id FROM {s}.clientes",
        "CREATE FUNCTION {s}.f_atomic() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT count(*)::int FROM {s}.clientes; END",
        "CREATE FUNCTION {s}.f_plpgsql() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN (SELECT count(*) FROM {s}.clientes); END $$",
        "CREATE FUNCTION {s}.f_dinamica() RETURNS int LANGUAGE plpgsql AS $$ BEGIN EXECUTE 'SELECT count(*) FROM {s}.clientes'; RETURN 1; END $$",
        "CREATE FUNCTION {s}.f_fila(c {s}.clientes) RETURNS int LANGUAGE sql AS 'SELECT 1'",
        "CREATE FUNCTION {s}.f_ajena() RETURNS int LANGUAGE plpgsql AS $$ BEGIN RETURN 1; END $$",
        "CREATE FUNCTION {s}.trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
        "CREATE TRIGGER t_clientes BEFORE INSERT ON {s}.clientes FOR EACH ROW EXECUTE FUNCTION {s}.trg()",
        "CREATE TABLE {s}.hijo (extra int) INHERITS ({s}.clientes)",
        "ALTER TABLE {s}.clientes ENABLE ROW LEVEL SECURITY",
        "CREATE POLICY pol_nombre ON {s}.clientes USING (nombre IS NOT NULL)",
        "ALTER TABLE {s}.pedidos ENABLE ROW LEVEL SECURITY",
        "CREATE POLICY pol_ped ON {s}.pedidos USING (EXISTS (SELECT 1 FROM {s}.clientes c WHERE c.id = cliente_id))",
        "CREATE TABLE {s}.otra (id int, calc int GENERATED ALWAYS AS ({s}.positivo(id)::int) STORED)",
    ] {
        run(s, &stmt.replace("{s}", sch)).await;
    }

    // The table: what points at it, and nothing of its own.
    let r = deps(d, s, &target("table", sch, "clientes", None)).await;
    one(
        &r,
        "view",
        "v_clientes",
        Relation::Code,
        Confidence::Confirmed,
    );
    one(
        &r,
        "materialized_view",
        "mv_ids",
        Relation::Code,
        Confidence::Confirmed,
    );
    one(
        &r,
        "function",
        "f_atomic",
        Relation::Code,
        Confidence::Confirmed,
    );
    one(
        &r,
        "function",
        "f_fila",
        Relation::Code,
        Confidence::Confirmed,
    );
    let p = one(
        &r,
        "function",
        "f_plpgsql",
        Relation::Code,
        Confidence::Probable,
    );
    assert!(!p.mentions.is_empty());
    one(
        &r,
        "function",
        "f_dinamica",
        Relation::Code,
        Confidence::Review,
    );
    let fk = one(
        &r,
        "table",
        "pedidos",
        Relation::ForeignKey,
        Confidence::Confirmed,
    );
    assert!(
        fk.detail.as_deref().unwrap().contains("(cliente_id)"),
        "{fk:?}"
    );
    assert!(fk
        .detail
        .as_deref()
        .unwrap()
        .contains(&format!("{sch}.clientes (id)")));
    let inh = one(&r, "table", "hijo", Relation::Code, Confidence::Confirmed);
    assert_eq!(inh.detail.as_deref(), Some("INHERITS"));
    let pol = items(&r, "table", "pedidos")
        .into_iter()
        .find(|i| i.relation == Relation::Code)
        .expect("the policy of pedidos");
    assert_eq!(pol.detail.as_deref(), Some("POLICY pol_ped"));
    // Its own policy is not its dependent.
    absent(&r, "table", "clientes");
    let trig = one(
        &r,
        "trigger",
        "t_clientes",
        Relation::Code,
        Confidence::Confirmed,
    );
    assert_eq!(trig.parent.as_deref(), Some("clientes"));
    // Not about the table, or its own indexes and keys.
    absent(&r, "view", "v_de_vista");
    absent(&r, "function", "f_ajena");
    absent(&r, "function", "trg");
    assert!(
        r.items.iter().all(|i| i.relation != Relation::Index),
        "{:?}",
        r.items
    );

    // A column: the views and the index that name it, not the one that doesn't.
    let r = deps(d, s, &target("table", sch, "clientes", Some("nombre"))).await;
    one(
        &r,
        "view",
        "v_clientes",
        Relation::Code,
        Confidence::Confirmed,
    );
    let ix = one(
        &r,
        "table",
        "clientes",
        Relation::Index,
        Confidence::Confirmed,
    );
    assert!(ix
        .detail
        .as_deref()
        .unwrap()
        .starts_with("ix_clientes_nombre"));
    absent(&r, "materialized_view", "mv_ids");
    let r = deps(d, s, &target("table", sch, "clientes", Some("id"))).await;
    one(
        &r,
        "materialized_view",
        "mv_ids",
        Relation::Code,
        Confidence::Confirmed,
    );
    one(
        &r,
        "table",
        "pedidos",
        Relation::ForeignKey,
        Confidence::Confirmed,
    );
    let pk = items(&r, "table", "clientes")
        .into_iter()
        .find(|i| i.relation == Relation::Index)
        .expect("the primary key");
    assert!(pk.detail.as_deref().unwrap().contains("(id)"));

    // A view, a sequence and a function.
    let r = deps(d, s, &target("view", sch, "v_clientes", None)).await;
    one(
        &r,
        "view",
        "v_de_vista",
        Relation::Code,
        Confidence::Confirmed,
    );
    assert_eq!(r.items.len(), 1, "{:?}", r.items);
    let r = deps(d, s, &target("sequence", sch, "seq_ped", None)).await;
    let df = one(
        &r,
        "table",
        "pedidos",
        Relation::Code,
        Confidence::Confirmed,
    );
    assert!(df
        .detail
        .as_deref()
        .unwrap()
        .starts_with("id DEFAULT nextval("));
    let r = deps(d, s, &target("function", sch, "positivo", None)).await;
    one(
        &r,
        "table",
        "pedidos",
        Relation::Check,
        Confidence::Confirmed,
    );
    one(
        &r,
        "table",
        "pedidos",
        Relation::Index,
        Confidence::Confirmed,
    );
    let g = one(&r, "table", "otra", Relation::Code, Confidence::Confirmed);
    assert!(g.detail.as_deref().unwrap().contains("GENERATED ALWAYS"));
    assert_eq!(r.items.len(), 3, "{:?}", r.items);
    let r = deps(d, s, &target("function", sch, "trg", None)).await;
    one(
        &r,
        "trigger",
        "t_clientes",
        Relation::Code,
        Confidence::Confirmed,
    );

    // What the generic scan finds, the catalog finds too.
    let t = target("table", sch, "clientes", None);
    let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
    let native = deps(d, s, &t).await;
    let started = std::time::Instant::now();
    let generic = dbine_driver::dependencies::scan(&mut **s, &t, &scan)
        .await
        .unwrap();
    eprintln!(
        "generic scan: {} definitions read in {:?}",
        generic.scanned,
        started.elapsed()
    );
    for g in &generic.items {
        let hit = native
            .items
            .iter()
            .any(|n| n.kind == g.kind && n.name == g.name && n.relation == g.relation);
        assert!(hit, "the catalog misses {g:?}");
    }
}

#[tokio::test]
#[ignore]
async fn postgres_dependents() {
    let Some(c) = cfg("DBINE_TEST_POSTGRES_URL", "postgres") else {
        return;
    };
    let d = driver("postgres");
    let mut s = d.connect(&c, None).await.unwrap();
    let sch = unique("dbine_deps");
    run(&mut s, &format!("CREATE SCHEMA {sch}")).await;
    let r = AssertUnwindSafe(checks(&d, &mut s, &sch))
        .catch_unwind()
        .await;
    quiet(&mut s, &format!("DROP SCHEMA {sch} CASCADE")).await;
    if let Err(e) = r {
        std::panic::resume_unwind(e);
    }
}
