//! What depends on an object (`Session::dependents`) against real MySQL and
//! MariaDB: foreign keys (also from another database), views (the catalog on
//! MySQL, the text on MariaDB), a function, a procedure and a trigger, and a
//! column's index and check. The native answer is compared with the
//! contract's scan. Each test creates two databases and drops them at the
//! end, also on failure.
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//!   cargo test -p dbine-driver-mysql --test dependents -- --ignored --nocapture
//! ```

use dbine_driver::{
    kinds, Confidence, ConnectionConfig, DependencyReport, DependencyScan, DependencyTarget,
    Driver, ObjectRef, QueryOutcome, Relation, Session,
};
use std::sync::Arc;

fn parse_url(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport
        .trim_end_matches('/')
        .rsplit_once(':')
        .map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port,
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_mysql::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map(|_| out)
}

fn target(kind: &str, name: &str, column: Option<&str>) -> DependencyTarget {
    DependencyTarget {
        object: ObjectRef {
            kind: kind.into(),
            schema: None,
            name: name.into(),
        },
        column: column.map(Into::into),
    }
}

/// What a report says, comparable: `(kind, name, relation, parent)`.
fn names(r: &DependencyReport) -> Vec<(String, String, Relation, Option<String>)> {
    let mut v: Vec<_> = r
        .items
        .iter()
        .map(|d| (d.kind.clone(), d.name.clone(), d.relation, d.parent.clone()))
        .collect();
    v.sort();
    v
}

fn conf(r: &DependencyReport, name: &str) -> Option<Confidence> {
    r.items
        .iter()
        .find(|d| d.name == name)
        .map(|d| d.confidence)
}

async fn dependents(id: &'static str, env: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let cfg = parse_url(id, &url);
    let d = driver(id);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        % 1_000_000_000;
    let (db, other) = (format!("dbine_dep_{stamp}"), format!("dbine_dep_{stamp}_x"));
    for n in [&db, &other] {
        run(&mut admin, &format!("CREATE DATABASE {n}"))
            .await
            .expect("create database");
    }
    let result = tokio::spawn(checks(
        id,
        d.clone(),
        cfg.clone(),
        db.clone(),
        other.clone(),
    ))
    .await;
    // The other database holds a key into this one: it goes first.
    for n in [&other, &db] {
        if let Err(e) = run(&mut admin, &format!("DROP DATABASE IF EXISTS {n}")).await {
            eprintln!("{id}: DROP DATABASE {n}: {e}");
        }
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn checks(
    id: &'static str,
    d: Arc<dyn Driver>,
    cfg: ConnectionConfig,
    db: String,
    other: String,
) {
    let mut s = d
        .connect(&cfg, Some(&db))
        .await
        .expect("connect to the database");
    let setup = [
        "CREATE TABLE clientes (id INT PRIMARY KEY, nombre VARCHAR(20), \
           CONSTRAINT ck_nombre CHECK (nombre <> ''))"
            .to_string(),
        "CREATE INDEX ix_nombre ON clientes (nombre)".to_string(),
        "CREATE TABLE pedidos (id INT PRIMARY KEY, cliente_id INT, \
           CONSTRAINT fk_pedidos_clientes FOREIGN KEY (cliente_id) REFERENCES clientes (id))"
            .to_string(),
        "CREATE TABLE bitacora (id INT)".to_string(),
        format!(
            "CREATE TABLE {other}.lineas (id INT PRIMARY KEY, cliente_id INT, \
               CONSTRAINT fk_lineas_clientes FOREIGN KEY (cliente_id) REFERENCES {db}.clientes (id))"
        ),
        "CREATE FUNCTION f_doble(x INT) RETURNS INT DETERMINISTIC RETURN x * 2".to_string(),
        "CREATE VIEW v_pedidos AS SELECT p.id, c.nombre, f_doble(p.id) AS doble \
           FROM pedidos p JOIN clientes c ON c.id = p.cliente_id"
            .to_string(),
        "CREATE VIEW v_ids AS SELECT id FROM clientes".to_string(),
        format!("CREATE VIEW {other}.v_lejos AS SELECT id FROM {db}.clientes"),
        "CREATE VIEW v_sobre_vista AS SELECT id FROM v_ids".to_string(),
        "CREATE VIEW v_clientes_viejos AS SELECT 1 AS x".to_string(),
        "CREATE PROCEDURE p_alta() BEGIN INSERT INTO clientes (id) VALUES (1); \
           SELECT f_doble(id) FROM clientes; END"
            .to_string(),
        "CREATE TRIGGER tg_pedidos AFTER INSERT ON pedidos FOR EACH ROW \
           INSERT INTO bitacora SELECT id FROM clientes"
            .to_string(),
    ];
    for sql in &setup {
        run(&mut s, sql)
            .await
            .unwrap_or_else(|e| panic!("{id}: {sql}: {e}"));
    }
    let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
    let catalog = id == "mysql";

    // The table: keys (also from another database), views, routines, triggers.
    let t = target(kinds::TABLE, "clientes", None);
    let r = s.dependents(&t, &scan).await.unwrap();
    eprintln!("{id}: clientes <- {:#?}", r.items);
    let view_conf = if catalog {
        Confidence::Confirmed
    } else {
        Confidence::Probable
    };
    assert_eq!(conf(&r, "pedidos"), Some(Confidence::Confirmed), "{id}");
    assert_eq!(conf(&r, "lineas"), Some(Confidence::Confirmed), "{id}");
    assert_eq!(
        r.items
            .iter()
            .find(|d| d.name == "lineas")
            .unwrap()
            .schema
            .as_deref(),
        Some(other.as_str()),
        "{id}"
    );
    for v in ["v_pedidos", "v_ids"] {
        assert_eq!(conf(&r, v), Some(view_conf), "{id}: {v}");
        assert!(
            !r.items
                .iter()
                .find(|d| d.name == v)
                .unwrap()
                .mentions
                .is_empty(),
            "{id}: {v}"
        );
    }
    assert_eq!(
        conf(&r, "v_lejos"),
        Some(view_conf),
        "{id}: a view of another database"
    );
    assert_eq!(conf(&r, "p_alta"), Some(Confidence::Probable), "{id}");
    assert_eq!(conf(&r, "tg_pedidos"), Some(Confidence::Probable), "{id}");
    assert_eq!(
        r.items
            .iter()
            .find(|d| d.name == "tg_pedidos")
            .unwrap()
            .parent
            .as_deref(),
        Some("pedidos"),
        "{id}"
    );
    for not in ["v_sobre_vista", "v_clientes_viejos", "f_doble", "clientes"] {
        assert!(
            conf(&r, not).is_none(),
            "{id}: {not} doesn't use clientes: {:?}",
            r.items
        );
    }
    assert!(r.note.is_none(), "{id}: {:?}", r.note);

    // The native answer names what the contract's scan names (the scan adds
    // nothing but the other database's objects, which it doesn't read).
    let generic = dbine_driver::dependencies::scan(&mut *s, &t, &scan)
        .await
        .unwrap();
    let mut native = names(&r);
    native.retain(|n| n.1 != "lineas" && n.1 != "v_lejos");
    assert_eq!(native, names(&generic), "{id}: native vs generic");

    // A function: the views and routines that call it.
    let r = s
        .dependents(&target(kinds::FUNCTION, "f_doble", None), &scan)
        .await
        .unwrap();
    eprintln!("{id}: f_doble <- {:#?}", r.items);
    assert_eq!(conf(&r, "v_pedidos"), Some(view_conf), "{id}");
    assert_eq!(conf(&r, "p_alta"), Some(Confidence::Probable), "{id}");
    assert_eq!(r.items.len(), 2, "{id}: {:?}", r.items);
    // Named with the session's database, as the app may send it: the same
    // answer, and never the function itself.
    let mut named = target(kinds::FUNCTION, "f_doble", None);
    named.object.schema = Some(db.clone());
    let again = s.dependents(&named, &scan).await.unwrap();
    assert_eq!(names(&again), names(&r), "{id}: with its database");

    // A view: the view over it (the catalog, not the text, on MySQL).
    let r = s
        .dependents(&target(kinds::VIEW, "v_ids", None), &scan)
        .await
        .unwrap();
    assert_eq!(names(&r).len(), 1, "{id}: {:?}", r.items);
    assert_eq!(conf(&r, "v_sobre_vista"), Some(view_conf), "{id}");

    // A column: the key that points at it, the keys, indexes and checks of
    // the ones the table holds, and the views and routines that name it.
    let r = s
        .dependents(&target(kinds::TABLE, "clientes", Some("nombre")), &scan)
        .await
        .unwrap();
    eprintln!("{id}: clientes.nombre <- {:#?}", r.items);
    let rel = |name: &str, relation| {
        r.items
            .iter()
            .any(|d| d.name == name && d.relation == relation)
    };
    assert!(rel("clientes", Relation::Index), "{id}: ix_nombre");
    assert!(rel("clientes", Relation::Check), "{id}: ck_nombre");
    assert!(rel("v_pedidos", Relation::Code), "{id}");
    assert!(
        !rel("v_ids", Relation::Code),
        "{id}: v_ids doesn't name nombre"
    );
    assert!(!rel("pedidos", Relation::ForeignKey), "{id}");
    let r = s
        .dependents(&target(kinds::TABLE, "clientes", Some("id")), &scan)
        .await
        .unwrap();
    assert!(
        r.items
            .iter()
            .any(|d| d.name == "pedidos" && d.relation == Relation::ForeignKey),
        "{id}"
    );
    assert!(
        r.items
            .iter()
            .any(|d| d.name == "lineas" && d.relation == Relation::ForeignKey),
        "{id}"
    );
    let r = s
        .dependents(&target(kinds::TABLE, "pedidos", Some("cliente_id")), &scan)
        .await
        .unwrap();
    assert!(
        r.items
            .iter()
            .any(|d| d.name == "pedidos" && d.relation == Relation::ForeignKey),
        "{id}: the key the column holds: {:?}",
        r.items
    );
}

#[tokio::test]
#[ignore = "needs DBINE_TEST_MYSQL_URL"]
async fn mysql() {
    dependents("mysql", "DBINE_TEST_MYSQL_URL").await;
}

#[tokio::test]
#[ignore = "needs DBINE_TEST_MARIADB_URL"]
async fn mariadb() {
    dependents("mariadb", "DBINE_TEST_MARIADB_URL").await;
}
