//! What an object uses (`Session::references`) against a real server: a
//! schema of its own with tables, a foreign key, a type, a view, a
//! function, a package, a trigger, materialized views, synonyms (private
//! and public), an external table and a database link to the same database
//! (when the server lets one be made), each one's references checked, then
//! everything dropped, also when a check fails. Needs a user that can create
//! users, directories and public synonyms (SYSTEM):
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test references -- --ignored --nocapture
//! ```

use dbine_driver::dependencies::{
    Confidence, DependencyReport, DependencyScan, DependencyTarget, Relation,
};
use dbine_driver::{kinds, ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session};
use std::sync::Arc;

const SCHEMA: &str = "DBINE_LINEAGE";
const PASSWORD: &str = "Lineage_123";
const DIRECTORY: &str = "DBINE_LINEAGE_DIR";
const PUBLIC_SYNONYM: &str = "DBINE_LINEAGE_PUB";

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
    "CREATE SEQUENCE emp_seq",
    "CREATE OR REPLACE TYPE t_addr AS OBJECT (street VARCHAR2(30))",
    "CREATE TABLE cust (id NUMBER PRIMARY KEY, addr t_addr)",
    "CREATE VIEW v_emp AS\nSELECT e.id, e.name, d.name AS dname\n  FROM emp e\n  JOIN dept d ON d.id = e.dept_id",
    "CREATE OR REPLACE FUNCTION f_count RETURN NUMBER IS\n  n NUMBER;\nBEGIN\n  SELECT COUNT(*) INTO n FROM v_emp;\n  DBMS_OUTPUT.PUT_LINE(n);\n  RETURN n;\nEND f_count;",
    "CREATE OR REPLACE PACKAGE pkg AS\n  PROCEDURE add_emp(p_name VARCHAR2);\nEND pkg;",
    "CREATE OR REPLACE PACKAGE BODY pkg AS\n  PROCEDURE add_emp(p_name VARCHAR2) IS\n    v NUMBER;\n  BEGIN\n    v := f_count;\n    INSERT INTO emp (id, dept_id, name) VALUES (emp_seq.NEXTVAL, 1, p_name);\n  END add_emp;\nEND pkg;",
    "CREATE OR REPLACE TRIGGER trg_emp BEFORE INSERT ON emp FOR EACH ROW\nBEGIN\n  :new.name := UPPER(:new.name);\nEND;",
    "CREATE MATERIALIZED VIEW mv_dept AS SELECT id, name FROM dept",
    "CREATE VIEW v_mv AS SELECT id FROM mv_dept",
    "CREATE SYNONYM s_emp FOR emp",
    "CREATE VIEW v_sys AS SELECT username FROM all_users",
    // Invalid: the missing table stops the compilation.
    "CREATE OR REPLACE PROCEDURE p_bad IS\n  n NUMBER;\nBEGIN\n  SELECT COUNT(*) INTO n FROM dept;\n  SELECT COUNT(*) INTO n FROM no_such_table;\nEND p_bad;",
];

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

/// `(kind, schema, name, relation, confidence)` of each item.
fn items(r: &DependencyReport) -> Vec<(String, Option<String>, String, Relation, Confidence)> {
    r.items
        .iter()
        .map(|d| {
            (
                d.kind.clone(),
                d.schema.clone(),
                d.name.clone(),
                d.relation,
                d.confidence,
            )
        })
        .collect()
}

fn has(r: &DependencyReport, kind: &str, schema: Option<&str>, name: &str) -> bool {
    r.items
        .iter()
        .any(|d| d.kind == kind && d.schema.as_deref() == schema && d.name == name)
}

fn code(kind: &str, name: &str) -> (String, Option<String>, String, Relation, Confidence) {
    (
        kind.into(),
        None,
        name.into(),
        Relation::Code,
        Confidence::Confirmed,
    )
}

async fn references(
    s: &mut Box<dyn Session>,
    scan: &DependencyScan,
    kind: &str,
    name: &str,
) -> DependencyReport {
    let r = s.references(&target(kind, name), scan).await.unwrap();
    eprintln!(
        "{kind} {name}: {:?} external {:?} note {:?}",
        items(&r),
        r.external,
        r.note
    );
    assert_eq!(r.note, None, "{name}: answered from the dictionary");
    r
}

/// The checks, as the schema's owner (`cfg`); `link`: the database link
/// was made.
async fn check(
    d: Arc<dyn Driver>,
    admin_cfg: ConnectionConfig,
    cfg: ConnectionConfig,
    service: String,
    link: bool,
) {
    let mut s = d
        .connect(&cfg, Some(SCHEMA))
        .await
        .expect("connect as owner");
    let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);

    // A table: its foreign key's table, its column's type, no external.
    let r = references(&mut s, &scan, kinds::TABLE, "EMP").await;
    assert_eq!(
        items(&r),
        vec![(
            kinds::TABLE.into(),
            None,
            "DEPT".into(),
            Relation::ForeignKey,
            Confidence::Confirmed
        )]
    );
    assert_eq!(
        r.items[0].detail.as_deref(),
        Some("FK_EMP_DEPT (DEPT_ID) → DEPT (ID)")
    );
    assert!(r.external.is_empty());
    let r = references(&mut s, &scan, kinds::TABLE, "CUST").await;
    assert_eq!(items(&r), vec![code(kinds::TYPE, "T_ADDR")]);

    // A view: its tables, with the lines that name them.
    let r = references(&mut s, &scan, kinds::VIEW, "V_EMP").await;
    assert_eq!(
        items(&r),
        vec![code(kinds::TABLE, "DEPT"), code(kinds::TABLE, "EMP")]
    );
    assert!(r.items.iter().all(|d| !d.mentions.is_empty()), "{r:?}");
    assert_eq!(r.scanned, 1);

    // A function: the view; not SYS's STANDARD or DBMS_OUTPUT.
    let r = references(&mut s, &scan, kinds::FUNCTION, "F_COUNT").await;
    assert_eq!(items(&r), vec![code(kinds::VIEW, "V_EMP")]);
    assert!(r.items[0].mentions.iter().any(|m| m.text.contains("v_emp")));

    // A package: what its body uses, not the package itself.
    let r = references(&mut s, &scan, "package", "PKG").await;
    assert_eq!(
        items(&r),
        vec![
            code(kinds::TABLE, "EMP"),
            code(kinds::SEQUENCE, "EMP_SEQ"),
            code(kinds::FUNCTION, "F_COUNT"),
        ]
    );
    assert!(r.items.iter().all(|d| !d.mentions.is_empty()), "{r:?}");

    // A trigger: its table.
    let r = references(&mut s, &scan, kinds::TRIGGER, "TRG_EMP").await;
    assert_eq!(items(&r), vec![code(kinds::TABLE, "EMP")]);

    // A materialized view: its master, not its own container table; a view
    // on it sees the materialized view.
    let r = references(&mut s, &scan, kinds::MATERIALIZED_VIEW, "MV_DEPT").await;
    assert_eq!(items(&r), vec![code(kinds::TABLE, "DEPT")]);
    let r = references(&mut s, &scan, kinds::VIEW, "V_MV").await;
    assert_eq!(items(&r), vec![code(kinds::MATERIALIZED_VIEW, "MV_DEPT")]);

    // Synonyms: the private one's table; a view through the public one.
    let r = references(&mut s, &scan, kinds::SYNONYM, "S_EMP").await;
    assert_eq!(items(&r), vec![code(kinds::TABLE, "EMP")]);
    let r = references(&mut s, &scan, kinds::VIEW, "V_PUB").await;
    assert!(
        has(&r, kinds::SYNONYM, Some("PUBLIC"), PUBLIC_SYNONYM),
        "{r:?}"
    );
    // Oracle's own (a public synonym of SYS's view): left out.
    let r = references(&mut s, &scan, kinds::VIEW, "V_SYS").await;
    assert!(r.items.is_empty(), "{r:?}");

    // Not VALID: what the dictionary has, plus what the source names.
    let r = references(&mut s, &scan, kinds::PROCEDURE, "P_BAD").await;
    assert!(has(&r, kinds::TABLE, None, "DEPT"), "{r:?}");

    // An external table: its files.
    let r = references(&mut s, &scan, kinds::TABLE, "EXT_T").await;
    assert!(r.items.is_empty());
    let files: Vec<_> = r
        .external
        .iter()
        .map(|x| (x.system.as_deref(), x.name.as_deref(), x.detail.as_deref()))
        .collect();
    let dir = format!("{DIRECTORY} (/tmp/dbine_lineage)");
    assert_eq!(
        files,
        vec![
            (Some("ORACLE_LOADER"), Some("data.csv"), Some(dir.as_str())),
            (Some("ORACLE_LOADER"), Some("more.csv"), Some(dir.as_str())),
        ]
    );

    if link {
        let remote = |r: &DependencyReport, name: &str| {
            let x = r
                .external
                .iter()
                .find(|x| x.name.as_deref() == Some(name))
                .unwrap_or_else(|| panic!("{name} through the link: {r:?}"));
            assert_eq!(x.system.as_deref(), Some("Oracle"));
            assert_eq!(x.server.as_deref(), Some("localhost:1521"));
            assert_eq!(x.database.as_deref(), Some(service.as_str()));
            assert_eq!(x.schema.as_deref(), Some(SCHEMA));
            assert_eq!(x.detail.as_deref(), Some("@LNK"));
            x.clone()
        };
        // A view's link isn't in the dictionary: read from its text.
        let r = references(&mut s, &scan, kinds::VIEW, "V_REMOTE").await;
        assert_eq!(items(&r), vec![code(kinds::TABLE, "DEPT")]);
        assert!(!remote(&r, "EMP").mentions.is_empty());
        assert_eq!(r.external.len(), 1);
        // A procedure's is.
        let r = references(&mut s, &scan, kinds::PROCEDURE, "P_REMOTE").await;
        assert!(r.items.is_empty(), "{r:?}");
        assert!(!remote(&r, "EMP").mentions.is_empty());
        let r = references(&mut s, &scan, kinds::SYNONYM, "S_REMOTE").await;
        assert!(r.items.is_empty());
        remote(&r, "DEPT");
        let r = references(&mut s, &scan, kinds::MATERIALIZED_VIEW, "MV_REMOTE").await;
        assert!(r.items.is_empty(), "{r:?}");
        remote(&r, "DEPT");

        // Someone else's private link: a DBA reads its host in DBA_DB_LINKS.
        let mut a = d.connect(&admin_cfg, Some(SCHEMA)).await.expect("connect");
        let r = a
            .references(&target(kinds::VIEW, "V_REMOTE"), &scan)
            .await
            .unwrap();
        remote(&r, "EMP");
    }
}

/// Makes the schema's objects and checks them.
async fn scenario(d: Arc<dyn Driver>, admin_cfg: ConnectionConfig, service: String) {
    let mut admin = d.connect(&admin_cfg, None).await.expect("connect");
    run(
        &mut admin,
        &format!("CREATE USER {SCHEMA} IDENTIFIED BY \"{PASSWORD}\" QUOTA UNLIMITED ON users"),
    )
    .await;
    run(&mut admin, &format!("GRANT CREATE SESSION, CREATE TABLE, CREATE VIEW, CREATE PROCEDURE, CREATE TRIGGER, CREATE SEQUENCE, CREATE SYNONYM, CREATE TYPE, CREATE MATERIALIZED VIEW, CREATE DATABASE LINK TO {SCHEMA}")).await;
    run(
        &mut admin,
        &format!("CREATE OR REPLACE DIRECTORY {DIRECTORY} AS '/tmp/dbine_lineage'"),
    )
    .await;
    run(
        &mut admin,
        &format!("GRANT READ ON DIRECTORY {DIRECTORY} TO {SCHEMA}"),
    )
    .await;

    let mut cfg = admin_cfg.clone();
    cfg.username = Some(SCHEMA.into());
    cfg.password = Some(PASSWORD.into());
    let link = {
        let mut s = d
            .connect(&cfg, Some(SCHEMA))
            .await
            .expect("connect as owner");
        for st in FIXTURE {
            // p_bad compiles with errors, as meant.
            if let Some(e) = try_run(&mut s, st).await {
                assert!(st.contains("p_bad"), "{st}: {e}");
            }
        }
        run(
            &mut admin,
            &format!("CREATE PUBLIC SYNONYM {PUBLIC_SYNONYM} FOR {SCHEMA}.DEPT"),
        )
        .await;
        run(
            &mut s,
            &format!("CREATE VIEW v_pub AS SELECT id FROM {PUBLIC_SYNONYM}"),
        )
        .await;
        run(&mut s, &format!("CREATE TABLE ext_t (id NUMBER, name VARCHAR2(30)) ORGANIZATION EXTERNAL (TYPE ORACLE_LOADER DEFAULT DIRECTORY {DIRECTORY} ACCESS PARAMETERS (FIELDS TERMINATED BY ',') LOCATION ('data.csv', {DIRECTORY}:'more.csv'))")).await;
        // The listener inside the server's own host.
        let link = try_run(
            &mut s,
            &format!("CREATE DATABASE LINK lnk CONNECT TO {SCHEMA} IDENTIFIED BY \"{PASSWORD}\" USING 'localhost:1521/{service}'"),
        )
        .await;
        let link = match link {
            None => try_run(&mut s, "SELECT COUNT(*) FROM dept@lnk").await,
            e => e,
        };
        match link {
            None => {
                for st in [
                    "CREATE VIEW v_remote AS\nSELECT d.id\n  FROM dept d\n  JOIN emp@lnk e ON e.dept_id = d.id",
                    "CREATE OR REPLACE PROCEDURE p_remote IS\n  n NUMBER;\nBEGIN\n  SELECT COUNT(*) INTO n FROM emp@lnk;\nEND p_remote;",
                    "CREATE SYNONYM s_remote FOR dept@lnk",
                    "CREATE MATERIALIZED VIEW mv_remote AS SELECT id, name FROM dept@lnk",
                ] {
                    run(&mut s, st).await;
                }
                true
            }
            Some(e) => {
                eprintln!("no database link to the same database ({e}): its checks are skipped");
                false
            }
        }
    };
    check(d, admin_cfg, cfg, service, link).await;
}

#[tokio::test]
#[ignore]
async fn references_live() {
    let Ok(url) = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL") else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    let admin_cfg = config_from(&url);
    let service = admin_cfg.option("service").unwrap_or_default().to_string();
    let d: Arc<dyn Driver> = dbine_driver_oracle::drivers().remove(0);
    let mut admin = d.connect(&admin_cfg, None).await.expect("connect");
    let drop = [
        format!("DROP PUBLIC SYNONYM {PUBLIC_SYNONYM}"),
        format!("DROP USER {SCHEMA} CASCADE"),
        format!("DROP DIRECTORY {DIRECTORY}"),
    ];
    for st in &drop {
        let _ = try_run(&mut admin, st).await;
    }
    // In a task: a failed check still drops what it made.
    let outcome = tokio::spawn(scenario(d.clone(), admin_cfg, service)).await;
    let mut left = Vec::new();
    for st in &drop {
        if let Some(e) = try_run(&mut admin, st).await {
            left.push(format!("{st}: {e}"));
        }
    }
    if let Err(e) = outcome {
        std::panic::resume_unwind(e.into_panic());
    }
    assert!(left.is_empty(), "not dropped: {left:?}");
}
