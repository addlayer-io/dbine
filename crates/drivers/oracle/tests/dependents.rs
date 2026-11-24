//! What depends on an object (`Session::dependents`) against a real server:
//! a schema of its own with tables, a foreign key, views, a materialized
//! view, routines (one with dynamic SQL), a package, a trigger, synonyms
//! (private and public), a sequence and a type, each one's dependents
//! checked and compared with the generic scan's, then everything dropped,
//! also when a check fails. Needs a user that can create users and public
//! synonyms (SYSTEM):
//!
//! ```sh
//! DBINE_TEST_ORACLE_ADMIN_URL=oracle://system:Secret123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test dependents -- --ignored --nocapture
//! ```

use dbine_driver::dependencies::{
    Confidence, DependencyReport, DependencyScan, DependencyTarget, Dependent, Relation,
};
use dbine_driver::{ConnectionConfig, Driver, ObjectRef, QueryOutcome, Session};
use std::sync::Arc;

const SCHEMA: &str = "DBINE_DEPS";
const PASSWORD: &str = "Deps_123";
const PUBLIC_SYNONYM: &str = "DBINE_DEPS_PUB";

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
    "CREATE TABLE emp (id NUMBER PRIMARY KEY, dept_id NUMBER CONSTRAINT fk_emp_dept REFERENCES dept (id), name VARCHAR2(30) NOT NULL, salary NUMBER CONSTRAINT ck_salary CHECK (salary > 0))",
    "CREATE INDEX ix_emp_name ON emp (name)",
    "CREATE SEQUENCE emp_seq",
    "CREATE OR REPLACE TYPE t_addr AS OBJECT (street VARCHAR2(30))",
    "CREATE TABLE cust (id NUMBER PRIMARY KEY, addr t_addr)",
    "CREATE VIEW v_emp AS\nSELECT e.id, e.name, d.name AS dname\n  FROM emp e\n  JOIN dept d ON d.id = e.dept_id",
    "CREATE VIEW v_of_view AS SELECT id FROM v_emp",
    "CREATE VIEW v_salary AS SELECT id, salary FROM emp",
    "CREATE OR REPLACE FUNCTION f_count RETURN NUMBER IS\n  n NUMBER;\nBEGIN\n  SELECT COUNT(*) INTO n FROM v_emp;\n  RETURN n;\nEND f_count;",
    "CREATE OR REPLACE PROCEDURE p_static IS\n  n NUMBER;\nBEGIN\n  SELECT COUNT(*) INTO n FROM emp;\nEND p_static;",
    "CREATE OR REPLACE PROCEDURE p_dynamic IS\nBEGIN\n  EXECUTE IMMEDIATE 'SELECT COUNT(*) FROM emp';\nEND p_dynamic;",
    "CREATE OR REPLACE PROCEDURE p_other IS\nBEGIN\n  NULL;\nEND p_other;",
    "CREATE OR REPLACE PACKAGE pkg AS\n  PROCEDURE add_emp(p_name VARCHAR2);\nEND pkg;",
    "CREATE OR REPLACE PACKAGE BODY pkg AS\n  PROCEDURE add_emp(p_name VARCHAR2) IS\n  BEGIN\n    INSERT INTO emp (id, dept_id, name) VALUES (emp_seq.NEXTVAL, 1, p_name);\n  END add_emp;\nEND pkg;",
    "CREATE OR REPLACE TRIGGER trg_emp BEFORE INSERT ON emp FOR EACH ROW\nBEGIN\n  :new.name := UPPER(:new.name);\nEND;",
    "CREATE MATERIALIZED VIEW mv_dept AS SELECT id, name FROM dept",
    "CREATE VIEW v_mv AS SELECT id FROM mv_dept",
    "CREATE SYNONYM s_emp FOR emp",
];

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

async fn deps(
    s: &mut Box<dyn Session>,
    scan: &DependencyScan,
    kind: &str,
    name: &str,
    column: Option<&str>,
) -> DependencyReport {
    let r = s
        .dependents(&target(kind, name, column), scan)
        .await
        .unwrap();
    eprintln!("{kind} {name} {column:?}: note {:?}", r.note);
    for i in &r.items {
        eprintln!(
            "  {:?} {:?} {} {:?}.{} {:?} parent {:?} ({} lines)",
            i.confidence,
            i.relation,
            i.kind,
            i.schema,
            i.name,
            i.detail,
            i.parent,
            i.mentions.len()
        );
    }
    assert_eq!(r.note, None, "{name}: answered from the dictionary");
    assert!(r.unreadable.is_empty(), "{:?}", r.unreadable);
    r
}

fn find<'a>(
    r: &'a DependencyReport,
    kind: &str,
    name: &str,
    relation: Relation,
    confidence: Confidence,
) -> &'a Dependent {
    let found: Vec<_> = r
        .items
        .iter()
        .filter(|i| i.kind == kind && i.name == name && i.relation == relation)
        .collect();
    assert_eq!(found.len(), 1, "{kind} {name} {relation:?}: {:?}", r.items);
    assert_eq!(found[0].confidence, confidence, "{kind} {name}");
    found[0]
}

fn absent(r: &DependencyReport, kind: &str, name: &str) {
    assert!(
        !r.items.iter().any(|i| i.kind == kind && i.name == name),
        "{kind} {name}: {:?}",
        r.items
    );
}

async fn check(d: Arc<dyn Driver>, admin_cfg: ConnectionConfig, cfg: ConnectionConfig) {
    let mut s = d
        .connect(&cfg, Some(SCHEMA))
        .await
        .expect("connect as owner");
    let scan = DependencyScan::new(d.info(), d.script_dialect(), d.capabilities().foreign_keys);
    for st in FIXTURE {
        run(&mut s, st).await;
    }
    let c = Confidence::Confirmed;

    // The table: what the dictionary records, and what only the source says.
    let r = deps(&mut s, &scan, "table", "EMP", None).await;
    find(&r, "view", "V_EMP", Relation::Code, c);
    find(&r, "view", "V_SALARY", Relation::Code, c);
    find(&r, "procedure", "P_STATIC", Relation::Code, c);
    find(&r, "package", "PKG", Relation::Code, c);
    let trg = find(&r, "trigger", "TRG_EMP", Relation::Code, c);
    assert_eq!(trg.parent.as_deref(), Some("EMP"));
    find(&r, "synonym", "S_EMP", Relation::Code, c);
    let dynamic = find(
        &r,
        "procedure",
        "P_DYNAMIC",
        Relation::Code,
        Confidence::Review,
    );
    assert!(!dynamic.mentions.is_empty());
    assert!(
        r.items.iter().all(|i| i.confidence != Confidence::Probable),
        "{:?}",
        r.items
    );
    absent(&r, "procedure", "P_OTHER");
    absent(&r, "view", "V_OF_VIEW");
    absent(&r, "table", "EMP");
    // The dictionary's rows are not read; only the source hits it didn't
    // return are (P_DYNAMIC, and F_COUNT because it names V_EMP).
    absent(&r, "function", "F_COUNT");
    assert_eq!(r.scanned, 2, "only the source hits are read");

    // The table pointed at by a key; the foreign key holder.
    let r = deps(&mut s, &scan, "table", "DEPT", None).await;
    let fk = find(&r, "table", "EMP", Relation::ForeignKey, c);
    assert_eq!(
        fk.detail.as_deref(),
        Some("FK_EMP_DEPT (DEPT_ID) → DEPT (ID)")
    );
    find(&r, "view", "V_EMP", Relation::Code, c);
    find(&r, "materialized_view", "MV_DEPT", Relation::Code, c);

    // A column: the table's dependents whose source names it, its own keys.
    let r = deps(&mut s, &scan, "table", "EMP", Some("SALARY")).await;
    find(&r, "view", "V_SALARY", Relation::Code, Confidence::Probable);
    absent(&r, "view", "V_EMP");
    find(&r, "table", "EMP", Relation::Check, c);
    let r = deps(&mut s, &scan, "table", "EMP", Some("NAME")).await;
    find(&r, "view", "V_EMP", Relation::Code, Confidence::Probable);
    let ix = r
        .items
        .iter()
        .filter(|i| i.relation == Relation::Index)
        .map(|i| i.detail.clone().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ix, ["IX_EMP_NAME (NAME)"], "{:?}", r.items);
    assert!(
        !r.items.iter().any(|i| i.relation == Relation::Check),
        "NOT NULL is no check"
    );
    let r = deps(&mut s, &scan, "table", "DEPT", Some("ID")).await;
    find(&r, "table", "EMP", Relation::ForeignKey, c);

    // Views, a materialized view, a sequence, a function, a type.
    let r = deps(&mut s, &scan, "view", "V_EMP", None).await;
    find(&r, "view", "V_OF_VIEW", Relation::Code, c);
    find(&r, "function", "F_COUNT", Relation::Code, c);
    assert_eq!(r.items.len(), 2, "{:?}", r.items);
    let r = deps(&mut s, &scan, "materialized_view", "MV_DEPT", None).await;
    find(&r, "view", "V_MV", Relation::Code, c);
    let r = deps(&mut s, &scan, "sequence", "EMP_SEQ", None).await;
    find(&r, "package", "PKG", Relation::Code, c);
    let r = deps(&mut s, &scan, "type", "T_ADDR", None).await;
    find(&r, "table", "CUST", Relation::Code, c);
    let r = deps(&mut s, &scan, "function", "F_COUNT", None).await;
    assert!(r.items.is_empty(), "{:?}", r.items);
    let r = deps(&mut s, &scan, "package", "PKG", None).await;
    assert!(r.items.is_empty(), "{:?}", r.items);

    // A public synonym, as PUBLIC's; and from another session of the same
    // database (the admin's), the owner's objects with their schema.
    let r = deps(&mut s, &scan, "table", "DEPT", None).await;
    assert!(
        !r.items.iter().any(|i| i.name == PUBLIC_SYNONYM),
        "created below"
    );
    let mut admin = d.connect(&admin_cfg, None).await.expect("connect");
    run(
        &mut admin,
        &format!("CREATE PUBLIC SYNONYM {PUBLIC_SYNONYM} FOR {SCHEMA}.DEPT"),
    )
    .await;
    let r = deps(&mut s, &scan, "table", "DEPT", None).await;
    let p = find(&r, "synonym", PUBLIC_SYNONYM, Relation::Code, c);
    assert_eq!(p.schema.as_deref(), Some("PUBLIC"));
    let o = ObjectRef {
        kind: "table".into(),
        schema: Some(SCHEMA.into()),
        name: "EMP".into(),
    };
    let t = DependencyTarget {
        object: o,
        column: None,
    };
    let r = admin.dependents(&t, &scan).await.unwrap();
    assert_eq!(r.note, None);
    let v = r.items.iter().find(|i| i.name == "V_EMP").expect("V_EMP");
    assert_eq!(v.schema.as_deref(), Some(SCHEMA));

    // What the generic scan finds, the dictionary finds too.
    let t = target("table", "EMP", None);
    let started = std::time::Instant::now();
    let native = s.dependents(&t, &scan).await.unwrap();
    eprintln!(
        "native: {} definitions read in {:?}",
        native.scanned,
        started.elapsed()
    );
    let started = std::time::Instant::now();
    let generic = dbine_driver::dependencies::scan(&mut *s, &t, &scan)
        .await
        .unwrap();
    eprintln!(
        "generic scan: {} definitions read in {:?}",
        generic.scanned,
        started.elapsed()
    );
    for g in &generic.items {
        assert!(
            native
                .items
                .iter()
                .any(|n| n.kind == g.kind && n.name == g.name && n.relation == g.relation),
            "the dictionary misses {g:?}"
        );
    }
}

#[tokio::test]
#[ignore]
async fn dependents_live() {
    let Ok(url) = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL") else {
        eprintln!("DBINE_TEST_ORACLE_ADMIN_URL not set; skipping");
        return;
    };
    let admin_cfg = config_from(&url);
    let d: Arc<dyn Driver> = dbine_driver_oracle::drivers().remove(0);
    let mut admin = d.connect(&admin_cfg, None).await.expect("connect");
    let drop = [
        format!("DROP PUBLIC SYNONYM {PUBLIC_SYNONYM}"),
        format!("DROP USER {SCHEMA} CASCADE"),
    ];
    for st in &drop {
        let _ = try_run(&mut admin, st).await;
    }
    run(
        &mut admin,
        &format!("CREATE USER {SCHEMA} IDENTIFIED BY \"{PASSWORD}\" QUOTA UNLIMITED ON users"),
    )
    .await;
    run(&mut admin, &format!("GRANT CREATE SESSION, CREATE TABLE, CREATE VIEW, CREATE PROCEDURE, CREATE TRIGGER, CREATE SEQUENCE, CREATE SYNONYM, CREATE TYPE, CREATE MATERIALIZED VIEW TO {SCHEMA}")).await;
    let mut cfg = admin_cfg.clone();
    cfg.username = Some(SCHEMA.into());
    cfg.password = Some(PASSWORD.into());
    // In a task: a failed check still drops what it made.
    let outcome = tokio::spawn(check(d.clone(), admin_cfg, cfg)).await;
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
