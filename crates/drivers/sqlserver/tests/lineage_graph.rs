//! The lineage of a whole database (`Session::lineage_graph`) against a SQL
//! Server with the Agent running (`DBINE_TEST_SQLSERVER_AGENT_URL`, a
//! container started with `MSSQL_AGENT_ENABLED=true`). Skipped without it:
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_AGENT_URL='mssql://sa:Pw_12345!@localhost:26131' \
//!   cargo test -p dbine-driver-sqlserver --test lineage_graph -- --ignored --test-threads=1
//! ```
//!
//! The databases `dbine_test_lg` and `dbine_test_lg_b` and the job `dbine_test_lg_job` are made by
//! the test and dropped at the end, whether it passed or not.

use dbine_driver::dependencies::{LineageGraph, LineageScope};
use dbine_driver::{
    Confidence, ConnectionConfig, DependencyScan, Driver, QueryOutcome, Relation, Session,
};
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

const DB: &str = "dbine_test_lg";

fn cfg() -> Option<ConnectionConfig> {
    let Ok(url) = std::env::var("DBINE_TEST_SQLSERVER_AGENT_URL") else {
        eprintln!("DBINE_TEST_SQLSERVER_AGENT_URL not set; skipping");
        return None;
    };
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@')?;
    let (user, pass) = auth.split_once(':')?;
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "sqlserver".into(),
        host: host.into(),
        port: port.trim_end_matches('/').parse().ok()?,
        username: Some(user.into()),
        password: Some(pass.into()),
        trust_server_certificate: true,
        ..Default::default()
    })
}

fn driver() -> Arc<dyn Driver> {
    dbine_driver_sqlserver::drivers()
        .into_iter()
        .find(|d| d.info().id == "sqlserver")
        .unwrap()
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    let r = s.execute(sql, 100, &mut out).await;
    assert!(
        r.is_ok() && out.errors.is_empty(),
        "{sql}: {r:?} {:?}",
        out.errors
    );
}

async fn cleanup(master: &mut Box<dyn Session>) {
    let mut out = QueryOutcome::default();
    let _ = master
        .execute(
            "IF EXISTS (SELECT 1 FROM msdb.dbo.sysjobs WHERE name = N'dbine_test_lg_job') EXEC msdb.dbo.sp_delete_job @job_name = N'dbine_test_lg_job';
             IF DB_ID(N'dbine_test_lg') IS NOT NULL BEGIN
               ALTER DATABASE dbine_test_lg SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE dbine_test_lg;
             END;
             IF DB_ID(N'dbine_test_lg_b') IS NOT NULL BEGIN
               ALTER DATABASE dbine_test_lg_b SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE dbine_test_lg_b;
             END",
            10,
            &mut out,
        )
        .await;
}

fn pair(g: &LineageGraph, from: &str, to: &str) -> Option<(Relation, Confidence)> {
    g.edges
        .iter()
        .find(|e| e.from.name == from && e.to.name == to)
        .map(|e| (e.relation, e.confidence))
}

#[tokio::test]
#[ignore = "needs DBINE_TEST_SQLSERVER_AGENT_URL"]
async fn the_whole_database_as_a_graph() {
    let Some(c) = cfg() else { return };
    let d = driver();
    let mut master = d.connect(&c, None).await.expect("connect");
    cleanup(&mut master).await;
    run(&mut master, "CREATE DATABASE dbine_test_lg").await;
    run(&mut master, "CREATE DATABASE dbine_test_lg_b").await;
    run(
        &mut master,
        "CREATE TABLE dbine_test_lg_b.dbo.archivo (id int)",
    )
    .await;
    let body = async {
        let mut s = d
            .connect(
                &ConnectionConfig {
                    database: DB.into(),
                    ..c.clone()
                },
                None,
            )
            .await
            .expect("connect to the test database");
        for sql in [
            "CREATE TABLE dbo.clientes (id int PRIMARY KEY)",
            "CREATE TABLE dbo.pedidos (id int PRIMARY KEY, cliente_id int CONSTRAINT FK_lg_ped_cli REFERENCES dbo.clientes(id))",
            "CREATE TABLE dbo.aislada (id int)",
            "CREATE FUNCTION dbo.fn_lg_tope() RETURNS int AS BEGIN RETURN 100 END",
            "CREATE TABLE dbo.limites (id int, tope int CONSTRAINT CK_lg_tope CHECK (tope < dbo.fn_lg_tope()))",
            "CREATE VIEW dbo.v_lg_ventas AS SELECT p.id, c.id AS cid FROM dbo.pedidos p JOIN dbo.clientes c ON c.id = p.cliente_id",
            "CREATE PROCEDURE dbo.sp_lg_reporte AS SELECT * FROM dbo.v_lg_ventas",
            "CREATE PROCEDURE dbo.sp_lg_otra_base AS SELECT * FROM dbine_test_lg_b.dbo.archivo",
            "CREATE TRIGGER dbo.tr_lg_pedidos ON dbo.pedidos AFTER INSERT AS SELECT 1",
            // Deferred name resolution: the procedure exists before its table.
            "CREATE PROCEDURE dbo.sp_lg_tardia AS SELECT * FROM dbo.tabla_tardia",
            "CREATE TABLE dbo.tabla_tardia (id int)",
        ] {
            run(&mut s, sql).await;
        }
        run(
            &mut master,
            "EXEC msdb.dbo.sp_add_job @job_name = N'dbine_test_lg_job';
             EXEC msdb.dbo.sp_add_jobstep @job_name = N'dbine_test_lg_job', @step_name = N'Reporte', @subsystem = N'TSQL',
                  @database_name = N'dbine_test_lg', @command = N'EXEC dbo.sp_lg_reporte;';
             EXEC msdb.dbo.sp_add_jobserver @job_name = N'dbine_test_lg_job';",
        )
        .await;
        let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
        let scope = LineageScope {
            jobs: true,
            ..Default::default()
        };
        let g = s.lineage_graph(DB, &scope, &scan).await.expect("graph");

        // Nodes: also the table nothing touches.
        for n in [
            "clientes",
            "pedidos",
            "aislada",
            "limites",
            "v_lg_ventas",
            "sp_lg_reporte",
            "tr_lg_pedidos",
        ] {
            assert!(
                g.objects.iter().any(|o| o.name == n),
                "{n}: {:?}",
                g.objects
            );
        }
        // The catalog's edges are confirmed.
        assert_eq!(
            pair(&g, "clientes", "pedidos"),
            Some((Relation::ForeignKey, Confidence::Confirmed))
        );
        assert_eq!(
            pair(&g, "pedidos", "v_lg_ventas"),
            Some((Relation::Code, Confidence::Confirmed))
        );
        assert_eq!(
            pair(&g, "clientes", "v_lg_ventas"),
            Some((Relation::Code, Confidence::Confirmed))
        );
        assert_eq!(
            pair(&g, "v_lg_ventas", "sp_lg_reporte"),
            Some((Relation::Code, Confidence::Confirmed))
        );
        assert_eq!(
            pair(&g, "pedidos", "tr_lg_pedidos"),
            Some((Relation::Code, Confidence::Confirmed))
        );
        // A CHECK that calls a function: the function, drawn to its table.
        assert_eq!(
            pair(&g, "fn_lg_tope", "limites"),
            Some((Relation::Check, Confidence::Confirmed))
        );
        // The line that names the table comes with the edge.
        let e = g
            .edges
            .iter()
            .find(|e| e.from.name == "v_lg_ventas" && e.to.name == "sp_lg_reporte")
            .unwrap();
        assert_eq!(e.mentions[0].line, 1, "{:?}", e.mentions);
        // The isolated one has no edge.
        assert!(!g
            .edges
            .iter()
            .any(|e| e.from.name == "aislada" || e.to.name == "aislada"));
        // Another database: outside the graph.
        assert!(
            g.external.iter().any(|x| x.to.name == "sp_lg_otra_base"
                && x.source.database.as_deref() == Some("dbine_test_lg_b")),
            "{:?}",
            g.external
        );
        // The table the catalog doesn't tie (the name was unknown when compiled)
        // or does, once it exists: either way the procedure is linked to it.
        assert!(
            pair(&g, "tabla_tardia", "sp_lg_tardia").is_some(),
            "{:?} {:?} {:?}",
            g.edges
                .iter()
                .map(|e| (&e.from.name, &e.to.name))
                .collect::<Vec<_>>(),
            g.unreadable,
            g.notes
        );
        assert!(g.scanned >= 5);
        // The job and what its step runs.
        let job = g
            .jobs
            .iter()
            .find(|j| j.name == "dbine_test_lg_job")
            .expect("the job");
        assert!(
            job.uses.iter().any(|u| u.name == "sp_lg_reporte"),
            "{:?}",
            job.uses
        );

        // A scope: one kind, no jobs.
        let tables = s
            .lineage_graph(
                DB,
                &LineageScope {
                    kinds: vec!["table".into()],
                    ..Default::default()
                },
                &scan,
            )
            .await
            .expect("tables");
        assert!(tables.objects.iter().all(|o| o.kind == "table"));
        assert!(tables.jobs.is_empty());
        assert!(pair(&tables, "clientes", "pedidos").is_some());
        assert!(pair(&tables, "pedidos", "v_lg_ventas").is_none());
        // The limit truncates.
        let few = s
            .lineage_graph(
                DB,
                &LineageScope {
                    limit: 2,
                    ..Default::default()
                },
                &scan,
            )
            .await
            .expect("limited");
        assert!(few.truncated && few.objects.len() == 2);
        drop(s);
    };
    let result = AssertUnwindSafe(body).catch_unwind().await;
    cleanup(&mut master).await;
    if let Err(e) = result {
        std::panic::resume_unwind(e);
    }
}
