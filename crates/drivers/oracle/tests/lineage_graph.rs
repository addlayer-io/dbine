//! The lineage of a whole schema (`Session::lineage_graph`) against a real
//! Oracle: a schema of its own with tables, a foreign key, views, a package,
//! a trigger, a synonym, a database link (when the server lets one be
//! made) and a scheduler job, then the graph checked and everything dropped,
//! also when a check fails. Needs a user that can create users (SYSTEM):
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test lineage_graph -- --ignored --nocapture
//! ```

use dbine_driver::dependencies::{
    Confidence, DependencyScan, LineageGraph, LineageScope, Relation,
};
use dbine_driver::{ConnectionConfig, Driver, QueryOutcome, Session};
use std::sync::Arc;

const SCHEMA: &str = "DBINE_LINEAGE_G";
const PASSWORD: &str = "Lineage_123";

fn config_from(url: &str) -> ConnectionConfig {
    let rest = url
        .strip_prefix("oracle://")
        .expect("oracle://user:pass@host:port/service");
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, service) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    let mut cfg = ConnectionConfig {
        driver: "oracle".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    };
    cfg.options.insert("service".into(), service.into());
    cfg
}

async fn try_run(s: &mut Box<dyn Session>, sql: &str) -> Option<String> {
    let mut out = QueryOutcome::default();
    match s.execute(sql, 100, &mut out).await {
        Err(e) => Some(e.to_string()),
        Ok(()) => out.error.take().map(|e| e.to_string()),
    }
}

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    if let Some(e) = try_run(s, sql).await {
        panic!("{sql}: {e}");
    }
}

const FIXTURE: &[&str] = &[
    "CREATE TABLE dept (id NUMBER PRIMARY KEY, name VARCHAR2(30))",
    "CREATE TABLE emp (id NUMBER PRIMARY KEY, dept_id NUMBER CONSTRAINT fk_emp_dept REFERENCES dept (id), name VARCHAR2(30))",
    "CREATE TABLE lonely (id NUMBER)",
    "CREATE SEQUENCE emp_seq",
    "CREATE VIEW v_emp AS\nSELECT e.id, e.name, d.name AS dname\n  FROM emp e\n  JOIN dept d ON d.id = e.dept_id",
    "CREATE OR REPLACE FUNCTION f_count RETURN NUMBER IS\n  n NUMBER;\nBEGIN\n  SELECT COUNT(*) INTO n FROM v_emp;\n  RETURN n;\nEND f_count;",
    "CREATE OR REPLACE PACKAGE pkg AS\n  PROCEDURE add_emp(p_name VARCHAR2);\nEND pkg;",
    "CREATE OR REPLACE PACKAGE BODY pkg AS\n  PROCEDURE add_emp(p_name VARCHAR2) IS\n    v NUMBER;\n  BEGIN\n    v := f_count;\n    INSERT INTO emp (id, dept_id, name) VALUES (emp_seq.NEXTVAL, 1, p_name);\n  END add_emp;\nEND pkg;",
    "CREATE OR REPLACE TRIGGER trg_emp BEFORE INSERT ON emp FOR EACH ROW\nBEGIN\n  :new.name := UPPER(:new.name);\nEND;",
    "CREATE SYNONYM s_emp FOR emp",
    "CREATE OR REPLACE PROCEDURE p_job IS\nBEGIN\n  pkg.add_emp('x');\nEND p_job;",
];

fn has(g: &LineageGraph, from: &str, to: &str, rel: Relation) -> bool {
    g.edges.iter().any(|e| {
        e.from.name == from
            && e.to.name == to
            && e.relation == rel
            && e.confidence == Confidence::Confirmed
    })
}

async fn scenario(d: Arc<dyn Driver>, admin_cfg: ConnectionConfig, service: String) {
    let mut admin = d.connect(&admin_cfg, None).await.expect("connect");
    run(
        &mut admin,
        &format!("CREATE USER {SCHEMA} IDENTIFIED BY \"{PASSWORD}\" QUOTA UNLIMITED ON users"),
    )
    .await;
    run(&mut admin, &format!("GRANT CREATE SESSION, CREATE TABLE, CREATE VIEW, CREATE PROCEDURE, CREATE TRIGGER, CREATE SEQUENCE, CREATE SYNONYM, CREATE DATABASE LINK, CREATE JOB TO {SCHEMA}")).await;
    let mut cfg = admin_cfg.clone();
    cfg.username = Some(SCHEMA.into());
    cfg.password = Some(PASSWORD.into());
    let mut s = d
        .connect(&cfg, Some(SCHEMA))
        .await
        .expect("connect as owner");
    for st in FIXTURE {
        run(&mut s, st).await;
    }
    let link = try_run(
        &mut s,
        &format!("CREATE DATABASE LINK lnk CONNECT TO {SCHEMA} IDENTIFIED BY \"{PASSWORD}\" USING 'localhost:1521/{service}'"),
    )
    .await;
    let link = match link {
        None => try_run(&mut s, "SELECT COUNT(*) FROM dept@lnk").await,
        e => e,
    };
    let link = match link {
        None => {
            run(&mut s, "CREATE OR REPLACE PROCEDURE p_remote IS\n  n NUMBER;\nBEGIN\n  SELECT COUNT(*) INTO n FROM emp@lnk;\nEND p_remote;").await;
            run(&mut s, "CREATE SYNONYM s_remote FOR dept@lnk").await;
            true
        }
        Some(e) => {
            eprintln!("no database link to the same database ({e}): its checks are skipped");
            false
        }
    };
    run(&mut s, "BEGIN DBMS_SCHEDULER.CREATE_JOB('J_NIGHTLY', job_type => 'STORED_PROCEDURE', job_action => 'P_JOB', repeat_interval => 'FREQ=DAILY', enabled => FALSE); END;").await;

    let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
    let g = s
        .lineage_graph(
            SCHEMA,
            &LineageScope {
                jobs: true,
                ..Default::default()
            },
            &scan,
        )
        .await
        .expect("lineage");
    eprintln!(
        "edges {:?}\nexternal {:?}\njobs {:?}\nnotes {:?}",
        g.edges
            .iter()
            .map(|e| (&e.from.name, &e.to.name, e.relation))
            .collect::<Vec<_>>(),
        g.external,
        g.jobs
            .iter()
            .map(|j| (&j.name, j.uses.len()))
            .collect::<Vec<_>>(),
        g.notes
    );
    assert!(
        g.notes.iter().all(|n| !n.contains("ALL_DEPENDENCIES")),
        "{:?}",
        g.notes
    );
    assert!(
        g.objects.iter().any(|o| o.name == "LONELY"),
        "isolated tables are nodes"
    );
    assert!(has(&g, "EMP_SEQ", "PKG", Relation::Code));
    assert!(has(&g, "DEPT", "EMP", Relation::ForeignKey));
    assert!(has(&g, "EMP", "V_EMP", Relation::Code));
    assert!(has(&g, "DEPT", "V_EMP", Relation::Code));
    assert!(has(&g, "V_EMP", "F_COUNT", Relation::Code));
    assert!(has(&g, "F_COUNT", "PKG", Relation::Code));
    assert!(has(&g, "EMP", "PKG", Relation::Code));
    assert!(has(&g, "EMP", "TRG_EMP", Relation::Code));
    assert!(has(&g, "EMP", "S_EMP", Relation::Code));
    assert!(has(&g, "PKG", "P_JOB", Relation::Code));
    // The package body is not a node and is not an edge on its spec.
    assert!(!g
        .edges
        .iter()
        .any(|e| e.from.name == "PKG" && e.to.name == "PKG"));
    let job = g
        .jobs
        .iter()
        .find(|j| j.name == "J_NIGHTLY")
        .expect("the job");
    assert!(
        job.uses
            .iter()
            .any(|u| u.name == "P_JOB" && u.confidence == Confidence::Confirmed),
        "{:?}",
        job.uses
    );
    if link {
        let ext = |to: &str| {
            g.external.iter().any(|x| {
                x.to.name == to
                    && x.source.system.as_deref() == Some("Oracle")
                    && x.source.detail.as_deref() == Some("@LNK")
            })
        };
        assert!(ext("P_REMOTE"), "{:?}", g.external);
        assert!(ext("S_REMOTE"), "{:?}", g.external);
    }

    // The scope: kinds and the limit.
    let views = s
        .lineage_graph(
            SCHEMA,
            &LineageScope {
                kinds: vec!["view".into()],
                ..Default::default()
            },
            &scan,
        )
        .await
        .unwrap();
    assert_eq!(views.objects.len(), 1);
    assert!(views.edges.is_empty() && views.jobs.is_empty());
    let capped = s
        .lineage_graph(
            SCHEMA,
            &LineageScope {
                limit: 3,
                ..Default::default()
            },
            &scan,
        )
        .await
        .unwrap();
    assert!(capped.truncated && capped.objects.len() == 3);
}

#[tokio::test]
#[ignore]
async fn lineage_graph_live() {
    let Ok(url) = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL") else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    let admin_cfg = config_from(&url);
    let service = admin_cfg.option("service").unwrap_or_default().to_string();
    let d: Arc<dyn Driver> = dbine_driver_oracle::drivers().remove(0);
    let mut admin = d.connect(&admin_cfg, None).await.expect("connect");
    let drop = format!("DROP USER {SCHEMA} CASCADE");
    let _ = try_run(&mut admin, &drop).await;
    // In a task: a failed check still drops what it made.
    let outcome = tokio::spawn(scenario(d.clone(), admin_cfg, service)).await;
    let left = try_run(&mut admin, &drop).await;
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
    assert!(left.is_none(), "not dropped: {left:?}");
}
