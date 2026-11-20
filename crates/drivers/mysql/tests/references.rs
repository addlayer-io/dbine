//! What an object uses (`Session::references`) against real servers: a
//! view over two tables and a function, a procedure, a trigger, foreign
//! keys (one to another database), a view reading another database and,
//! where the server has the engine, a FEDERATED table. Each test creates
//! two databases of its own and drops them at the end, also on failure.
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//! DBINE_TEST_TIDB_URL=mysql://root@localhost:25014 \
//! DBINE_TEST_OCEANBASE_URL=mysql://root@test:pw@localhost:25035 \
//!   cargo test -p dbine-driver-mysql --test references -- --ignored --nocapture
//! ```

use dbine_driver::{
    kinds, Confidence, ConnectionConfig, DependencyReport, DependencyScan, DependencyTarget,
    Driver, ObjectRef, QueryOutcome, Relation, Session,
};
use std::sync::Arc;

/// `scheme://user:pass@host:port` into a config (no URL escapes).
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

fn target(kind: &str, name: &str) -> DependencyTarget {
    DependencyTarget {
        object: ObjectRef {
            kind: kind.into(),
            schema: None,
            name: name.into(),
        },
        column: None,
    }
}

/// The item for `name` in `r`, as (kind, relation, confidence).
fn item(r: &DependencyReport, name: &str) -> Option<(String, Relation, Confidence)> {
    r.items
        .iter()
        .find(|d| d.name.eq_ignore_ascii_case(name))
        .map(|d| (d.kind.clone(), d.relation, d.confidence))
}

fn external<'a>(
    r: &'a DependencyReport,
    db: &str,
    name: &str,
) -> Option<&'a dbine_driver::dependencies::ExternalRef> {
    r.external.iter().find(|x| {
        x.database
            .as_deref()
            .is_some_and(|d| d.eq_ignore_ascii_case(db))
            && x.name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

struct Names {
    db: String,
    other: String,
}

async fn references(id: &'static str, env: &str) {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return;
    };
    let cfg = parse_url(id, &url);
    let d = driver(id);
    let mut admin = d.connect(&cfg, None).await.expect("connect");
    eprintln!("{id}: {}", admin.server_version().await.unwrap());
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        % 1_000_000_000;
    let names = Arc::new(Names {
        db: format!("dbine_ref_{stamp}"),
        other: format!("dbine_ref_{stamp}_x"),
    });
    for db in [&names.db, &names.other] {
        run(&mut admin, &format!("CREATE DATABASE {db}"))
            .await
            .expect("create database");
    }
    let checks = tokio::spawn(checks(id, d.clone(), cfg.clone(), names.clone()));
    let result = checks.await;
    for db in [&names.db, &names.other] {
        if let Err(e) = run(&mut admin, &format!("DROP DATABASE IF EXISTS {db}")).await {
            eprintln!("{id}: DROP DATABASE {db}: {e}");
        }
    }
    if let Err(e) = result {
        std::panic::resume_unwind(e.into_panic());
    }
}

async fn checks(id: &'static str, d: Arc<dyn Driver>, cfg: ConnectionConfig, n: Arc<Names>) {
    let (db, other) = (n.db.as_str(), n.other.as_str());
    let mut s = d
        .connect(&cfg, Some(db))
        .await
        .expect("connect to the database");
    let routines = matches!(id, "mysql" | "mariadb" | "oceanbase");
    let mut setup = vec![
        format!("CREATE TABLE {other}.items (id INT PRIMARY KEY, nombre VARCHAR(20))"),
        "CREATE TABLE clientes (id INT PRIMARY KEY, nombre VARCHAR(20))".to_string(),
        "CREATE TABLE pedidos (id INT PRIMARY KEY, cliente_id INT, \
           CONSTRAINT fk_pedidos_clientes FOREIGN KEY (cliente_id) REFERENCES clientes (id))"
            .to_string(),
        "CREATE TABLE bitacora (id INT)".to_string(),
        format!("CREATE VIEW v_otra AS SELECT i.id, c.nombre FROM {other}.items i JOIN clientes c ON c.id = i.id"),
    ];
    if routines {
        setup.extend([
            "CREATE FUNCTION f_doble(x INT) RETURNS INT DETERMINISTIC RETURN x * 2".to_string(),
            "CREATE VIEW v_pedidos AS SELECT p.id, c.nombre, f_doble(p.id) AS doble \
               FROM pedidos p JOIN clientes c ON c.id = p.cliente_id"
                .to_string(),
            format!(
                "CREATE PROCEDURE p_alta() BEGIN \
                   INSERT INTO pedidos (id, cliente_id) SELECT id, id FROM clientes; \
                   SELECT f_doble(id) FROM {other}.items; END"
            ),
            "CREATE TRIGGER tg_pedidos AFTER INSERT ON pedidos FOR EACH ROW \
               INSERT INTO bitacora VALUES (NEW.id)"
                .to_string(),
        ]);
    } else {
        setup.push(
            "CREATE VIEW v_pedidos AS SELECT p.id, c.nombre \
               FROM pedidos p JOIN clientes c ON c.id = p.cliente_id"
                .to_string(),
        );
    }
    for sql in &setup {
        run(&mut s, sql)
            .await
            .unwrap_or_else(|e| panic!("{id}: {sql}: {e}"));
    }
    // A foreign key to another database: not every engine takes one.
    let cross_fk = run(
        &mut s,
        &format!(
            "CREATE TABLE lineas (id INT PRIMARY KEY, item_id INT, \
               CONSTRAINT fk_lineas_items FOREIGN KEY (item_id) REFERENCES {other}.items (id))"
        ),
    )
    .await;
    if let Err(e) = &cross_fk {
        eprintln!("{id}: no foreign key to another database: {e}");
    }

    let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
    let catalog = id == "mysql";

    // The view over two tables (and a function).
    let r = s
        .references(&target(kinds::VIEW, "v_pedidos"), &scan)
        .await
        .unwrap();
    eprintln!("{id}: v_pedidos uses {:#?}", r.items);
    let view_conf = if catalog {
        Confidence::Confirmed
    } else {
        Confidence::Probable
    };
    for t in ["pedidos", "clientes"] {
        assert_eq!(
            item(&r, t),
            Some((kinds::TABLE.to_string(), Relation::Code, view_conf)),
            "{id}: v_pedidos → {t}"
        );
    }
    if routines {
        assert_eq!(
            item(&r, "f_doble"),
            Some((kinds::FUNCTION.to_string(), Relation::Code, view_conf)),
            "{id}: v_pedidos → f_doble"
        );
    }
    assert!(
        r.items.iter().all(|d| !d.mentions.is_empty()),
        "{id}: mentions {r:#?}"
    );
    assert!(r.external.is_empty(), "{id}: {:?}", r.external);

    // The view reading another database.
    let r = s
        .references(&target(kinds::VIEW, "v_otra"), &scan)
        .await
        .unwrap();
    eprintln!("{id}: v_otra uses {:#?} / {:#?}", r.items, r.external);
    assert_eq!(
        item(&r, "clientes").map(|i| i.0),
        Some(kinds::TABLE.to_string()),
        "{id}"
    );
    assert!(
        item(&r, "items").is_none(),
        "{id}: items is the other database's"
    );
    let x = external(&r, other, "items").unwrap_or_else(|| panic!("{id}: {:?}", r.external));
    assert_eq!(x.server, None, "{id}");
    assert!(!x.mentions.is_empty(), "{id}: {x:?}");

    if routines {
        // The procedure: its tables and function, and another database's.
        let r = s
            .references(&target(kinds::PROCEDURE, "p_alta"), &scan)
            .await
            .unwrap();
        eprintln!("{id}: p_alta uses {:#?} / {:#?}", r.items, r.external);
        for (name, kind) in [
            ("pedidos", kinds::TABLE),
            ("clientes", kinds::TABLE),
            ("f_doble", kinds::FUNCTION),
        ] {
            assert_eq!(
                item(&r, name),
                Some((kind.to_string(), Relation::Code, Confidence::Probable)),
                "{id}: p_alta → {name}"
            );
        }
        assert!(
            external(&r, other, "items").is_some(),
            "{id}: {:?}",
            r.external
        );

        // The trigger: its table (catalog) and what its body writes.
        let r = s
            .references(&target(kinds::TRIGGER, "tg_pedidos"), &scan)
            .await
            .unwrap();
        eprintln!("{id}: tg_pedidos uses {:#?}", r.items);
        assert_eq!(
            item(&r, "pedidos"),
            Some((
                kinds::TABLE.to_string(),
                Relation::Code,
                Confidence::Confirmed
            )),
            "{id}: tg_pedidos → pedidos"
        );
        let on = r.items.iter().find(|d| d.name == "pedidos").unwrap();
        assert_eq!(on.detail.as_deref(), Some("AFTER INSERT"), "{id}");
        assert_eq!(
            item(&r, "bitacora"),
            Some((
                kinds::TABLE.to_string(),
                Relation::Code,
                Confidence::Probable
            )),
            "{id}: tg_pedidos → bitacora"
        );
    }

    // The table's foreign key.
    let r = s
        .references(&target(kinds::TABLE, "pedidos"), &scan)
        .await
        .unwrap();
    eprintln!("{id}: pedidos uses {:#?}", r.items);
    assert_eq!(
        item(&r, "clientes"),
        Some((
            kinds::TABLE.to_string(),
            Relation::ForeignKey,
            Confidence::Confirmed
        )),
        "{id}: pedidos → clientes"
    );
    if cross_fk.is_ok() {
        let r = s
            .references(&target(kinds::TABLE, "lineas"), &scan)
            .await
            .unwrap();
        eprintln!("{id}: lineas uses {:#?} / {:#?}", r.items, r.external);
        let x = external(&r, other, "items").unwrap_or_else(|| panic!("{id}: {r:?}"));
        assert!(
            x.detail
                .as_deref()
                .is_some_and(|d| d.contains("fk_lineas_items")),
            "{id}: {x:?}"
        );
        assert!(r.items.is_empty(), "{id}: {:?}", r.items);
    }

    federated(id, &mut s, &cfg, other, &scan).await;
}

/// A FEDERATED table over the other database's `items`, where the server
/// has the engine (MySQL starts without it; MariaDB loads FederatedX on
/// demand, and the test unloads it again if it loaded it).
async fn federated(
    id: &str,
    s: &mut Box<dyn Session>,
    cfg: &ConnectionConfig,
    other: &str,
    scan: &DependencyScan,
) {
    if !matches!(id, "mysql" | "mariadb") {
        return;
    }
    let engines = run(s, "SHOW ENGINES").await.unwrap();
    let has = engines.results.last().is_some_and(|r| {
        r.rows.iter().any(|row| {
            row[0]
                .as_str()
                .is_some_and(|e| e.eq_ignore_ascii_case("FEDERATED"))
                && row[1].as_str().is_some_and(|v| {
                    v.eq_ignore_ascii_case("YES") || v.eq_ignore_ascii_case("DEFAULT")
                })
        })
    });
    let installed =
        !has && id == "mariadb" && run(s, "INSTALL SONAME 'ha_federatedx'").await.is_ok();
    if !has && !installed {
        eprintln!("{id}: no FEDERATED engine; skipped");
        return;
    }
    let pass = cfg.password.clone().unwrap_or_default();
    let user = cfg.username.clone().unwrap_or_default();
    let created = run(
        s,
        &format!(
            "CREATE TABLE remota (id INT PRIMARY KEY, nombre VARCHAR(20)) ENGINE=FEDERATED \
               CONNECTION='mysql://{user}:{pass}@127.0.0.1:3306/{other}/items'"
        ),
    )
    .await;
    let checked = match created {
        Ok(_) => {
            let r = s
                .references(&target(kinds::TABLE, "remota"), scan)
                .await
                .unwrap();
            eprintln!("{id}: remota uses {:#?}", r.external);
            let _ = run(s, "DROP TABLE remota").await;
            Some(r)
        }
        Err(e) => {
            eprintln!("{id}: FEDERATED table not created: {e}");
            None
        }
    };
    if installed {
        let _ = run(s, "UNINSTALL SONAME 'ha_federatedx'").await;
    }
    if let Some(r) = checked {
        let x = external(&r, other, "items").unwrap_or_else(|| panic!("{id}: {:?}", r.external));
        assert_eq!(x.system.as_deref(), Some("FEDERATED"), "{id}");
        assert_eq!(x.server.as_deref(), Some("127.0.0.1:3306"), "{id}");
        if !pass.is_empty() {
            assert!(
                !format!("{x:?}").contains(&format!(":{pass}@")),
                "{id}: password"
            );
        }
    }
}

#[tokio::test]
#[ignore]
async fn mysql() {
    references("mysql", "DBINE_TEST_MYSQL_URL").await;
}

#[tokio::test]
#[ignore]
async fn mariadb() {
    references("mariadb", "DBINE_TEST_MARIADB_URL").await;
}

#[tokio::test]
#[ignore]
async fn tidb() {
    references("tidb", "DBINE_TEST_TIDB_URL").await;
}

#[tokio::test]
#[ignore]
async fn oceanbase() {
    references("oceanbase", "DBINE_TEST_OCEANBASE_URL").await;
}
