//! "Linaje" of a whole database (`Session::lineage_graph`) against a real
//! MySQL and MariaDB: foreign keys, views (the catalog where it records them,
//! the code where it doesn't), a trigger, a function and a procedure read from
//! code, a view on another database (external) and an event as a job.
//! Everything is dropped at the end, also when a check fails.
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//!   cargo test -p dbine-driver-mysql --test lineage -- --ignored --nocapture
//! ```

use dbine_driver::dependencies::{Confidence, LineageScope};
use dbine_driver::{ConnectionConfig, DependencyScan, QueryOutcome};

fn cfg(driver: &str, env: &str) -> Option<ConnectionConfig> {
    let url = std::env::var(env).ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth
        .split_once(':')
        .map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.trim_end_matches('/').rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port: port.parse().ok()?,
        username: Some(user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

const DROP: &str = "DROP DATABASE IF EXISTS dbine_lin_a; DROP DATABASE IF EXISTS dbine_lin_b;";

async fn graph_of(id: &str, env: &str) {
    let Some(c) = cfg(id, env) else { return };
    let d = dbine_driver_mysql::drivers()
        .into_iter()
        .find(|d| d.info().id == id)
        .unwrap();
    let mut root = d.connect(&c, None).await.unwrap();
    let run = |sql: &'static str| async {
        let mut c2 = cfg(id, env).unwrap();
        c2.database = String::new();
        let mut s = dbine_driver_mysql::drivers()
            .into_iter()
            .find(|d| d.info().id == id)
            .unwrap()
            .connect(&c2, None)
            .await
            .unwrap();
        let mut out = QueryOutcome::default();
        s.execute(sql, 100, &mut out).await
    };
    let _ = root.execute(DROP, 10, &mut QueryOutcome::default()).await;
    let result = async {
        run(
            "CREATE DATABASE dbine_lin_a; CREATE DATABASE dbine_lin_b;
         CREATE TABLE dbine_lin_b.items (id INT PRIMARY KEY);
         CREATE TABLE dbine_lin_a.clientes (id INT PRIMARY KEY);
         CREATE TABLE dbine_lin_a.pedidos (id INT PRIMARY KEY, cliente_id INT,
           CONSTRAINT fk_cli FOREIGN KEY (cliente_id) REFERENCES clientes (id));
         CREATE TABLE dbine_lin_a.aislada (id INT);
         CREATE VIEW dbine_lin_a.v_base AS SELECT id FROM dbine_lin_a.clientes;
         CREATE VIEW dbine_lin_a.v_pedidos AS
           SELECT p.id FROM dbine_lin_a.pedidos p JOIN dbine_lin_a.v_base b ON b.id = p.cliente_id
           JOIN dbine_lin_b.items i ON i.id = p.id;",
        )
        .await?;
        run("CREATE TRIGGER dbine_lin_a.tg_pedidos BEFORE INSERT ON dbine_lin_a.pedidos FOR EACH ROW SET NEW.cliente_id = NEW.cliente_id").await?;
        run("CREATE PROCEDURE dbine_lin_a.p_reporte() SELECT COUNT(*) FROM dbine_lin_a.v_pedidos").await?;
        run("CREATE EVENT dbine_lin_a.e_noche ON SCHEDULE EVERY 1 DAY DISABLE DO CALL dbine_lin_a.p_reporte()").await?;
        let mut s = d.connect(&c, Some("dbine_lin_a")).await.unwrap();
        let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
        let scope = LineageScope {
            jobs: true,
            ..Default::default()
        };
        let g = s.lineage_graph("dbine_lin_a", &scope, &scan).await.unwrap();
        let edge = |from: &str, to: &str| {
            g.edges
                .iter()
                .find(|e| e.from.name == from && e.to.name == to)
                .map(|e| e.confidence)
        };
        println!("{id}: {:#?}", g.edges.iter().map(|e| format!("{}>{} {:?}", e.from.name, e.to.name, e.confidence)).collect::<Vec<_>>());
        let names: Vec<_> = g.objects.iter().map(|o| o.name.as_str()).collect();
        assert!(names.contains(&"aislada"), "{names:?}");
        assert_eq!(edge("clientes", "pedidos"), Some(Confidence::Confirmed));
        // MySQL 8 records what a view uses; MariaDB's views are read from code.
        let view = if id == "mysql" {
            Confidence::Confirmed
        } else {
            Confidence::Probable
        };
        assert_eq!(edge("clientes", "v_base"), Some(view));
        assert_eq!(edge("v_base", "v_pedidos"), Some(view));
        assert_eq!(edge("pedidos", "v_pedidos"), Some(view));
        assert_eq!(edge("pedidos", "tg_pedidos"), Some(Confidence::Confirmed));
        assert_eq!(edge("v_pedidos", "p_reporte"), Some(Confidence::Probable));
        assert!(
            g.external
                .iter()
                .any(|x| x.to.name == "v_pedidos" && x.source.name.as_deref() == Some("items")),
            "{:?}",
            g.external
        );
        let job = g.jobs.iter().find(|j| j.name.contains("e_noche"));
        assert!(
            job.is_some_and(|j| j.uses.iter().any(|u| u.name == "p_reporte")),
            "{:?} {:?}",
            g.jobs,
            g.job_notes
        );
        // A scope keeps only its kinds.
        let only = LineageScope {
            kinds: vec!["table".into()],
            ..Default::default()
        };
        let t = s.lineage_graph("dbine_lin_a", &only, &scan).await.unwrap();
        assert_eq!(t.objects.len(), 3);
        assert_eq!(t.edges.len(), 1);
        Ok::<_, dbine_driver::Error>(())
    }
    .await;
    let _ = root.execute(DROP, 10, &mut QueryOutcome::default()).await;
    result.unwrap();
}

#[tokio::test]
#[ignore]
async fn mysql() {
    graph_of("mysql", "DBINE_TEST_MYSQL_URL").await;
}

#[tokio::test]
#[ignore]
async fn mariadb() {
    graph_of("mariadb", "DBINE_TEST_MARIADB_URL").await;
}
