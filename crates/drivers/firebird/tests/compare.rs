//! "Comparar esquemas" against a real server: two databases that differ in
//! each thing the compare reads (named and unnamed CHECKs, descending,
//! computed, partial and inactive indexes, sequences, domains). The sync
//! script and the object definitions run on one side and both must then
//! read the same.
//!
//! ```sh
//! DBINE_TEST_FIREBIRD_URL=firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb \
//!   cargo test -p dbine-driver-firebird --test compare -- --ignored --nocapture
//! ```

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session, TableChange, TableSchema};
use std::collections::BTreeMap;

fn config() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_FIREBIRD_URL").ok()?;
    let rest = url.strip_prefix("firebird://").expect("firebird://user:pass@host:port/path");
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, path) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    Some(ConnectionConfig {
        driver: "firebird".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        database: path.into(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    })
}

const CODE_KINDS: &[&str] = &["view", "sequence", "type"];
const PREREQS: &[&str] = &["type", "sequence"];

type Objects = BTreeMap<(String, String), String>;

async fn run(s: &mut Box<dyn Session>, sql: &str) {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10, &mut out).await.unwrap_or_else(|e| panic!("{e}\n---\n{sql}"));
}

async fn read(s: &mut Box<dyn Session>) -> (BTreeMap<String, TableSchema>, Objects) {
    let tables = s
        .database_schema()
        .await
        .unwrap()
        .into_iter()
        .map(|mut t| {
            t.indexes.sort_by(|a, b| a.name.cmp(&b.name));
            (t.name.clone(), t)
        })
        .collect();
    let mut objects = Objects::new();
    for o in s.list_objects().await.unwrap() {
        if CODE_KINDS.contains(&o.kind.as_str()) {
            let r = ObjectRef { kind: o.kind.clone(), schema: None, name: o.name.clone() };
            objects.insert((o.kind, o.name), s.definition(&r).await.unwrap().expect("definition"));
        }
    }
    (tables, objects)
}

/// What src-tauri's `drop_other` has to write for Firebird: its DROP has
/// no `IF EXISTS` (before Firebird 6) and a domain is a DOMAIN.
fn drop_other(kind: &str, name: &str) -> String {
    let what = if kind == "type" { "DOMAIN".to_string() } else { kind.to_uppercase() };
    format!("DROP {what} \"{name}\";")
}

const A: &str = "
CREATE DOMAIN D_EMAIL AS VARCHAR(120) CHARACTER SET UTF8 DEFAULT 'x@y' NOT NULL CHECK (VALUE LIKE '%@%') COLLATE UNICODE_CI;
CREATE DOMAIN D_MONTO AS NUMERIC(18,2) CHECK (VALUE >= 0);
CREATE SEQUENCE SQ_FOLIO START WITH 100 INCREMENT BY 5;
CREATE TABLE DOCS (
    ID INTEGER NOT NULL PRIMARY KEY,
    N INTEGER CHECK (N > 0),
    M NUMERIC(18,2),
    T VARCHAR(50),
    CONSTRAINT CK_DOCS CHECK (N < 1000 OR M > 0)
);
CREATE DESCENDING INDEX IX_N ON DOCS (N);
CREATE INDEX IX_UP ON DOCS COMPUTED BY (UPPER(T));
CREATE INDEX IX_M ON DOCS (M) WHERE M > 0;
CREATE INDEX IX_T ON DOCS (T);
ALTER INDEX IX_T INACTIVE;
CREATE VIEW V_DOCS AS SELECT ID, T FROM DOCS;
";

const B: &str = "
CREATE DOMAIN D_EMAIL AS VARCHAR(100);
CREATE DOMAIN D_SOBRA AS INTEGER;
CREATE SEQUENCE SQ_FOLIO START WITH 1 INCREMENT BY 1;
CREATE SEQUENCE SQ_EXTRA;
CREATE TABLE DOCS (
    ID INTEGER NOT NULL PRIMARY KEY,
    N INTEGER CHECK (N > 5),
    M NUMERIC(18,2),
    T VARCHAR(50),
    CONSTRAINT CK_DOCS CHECK (N < 10)
);
INSERT INTO DOCS VALUES (1, 8, 5, 'a');
CREATE INDEX IX_N ON DOCS (N);
CREATE INDEX IX_UP ON DOCS COMPUTED BY (LOWER(T));
CREATE INDEX IX_T ON DOCS (T);
";

#[tokio::test]
#[ignore]
async fn compare_and_sync_everything() {
    let Some(base) = config() else {
        eprintln!("DBINE_TEST_FIREBIRD_URL not set; skipping");
        return;
    };
    let d = dbine_driver_firebird::drivers().remove(0);
    assert!(d.info().object_kinds.iter().any(|k| k.id == "type"));
    let mut admin = d.connect(&base, None).await.expect("connect");
    let dir = base.database.rsplit_once('/').map_or("", |(dir, _)| dir).to_string();
    let (pa, pb) = (format!("{dir}/dbine_cmp_a.fdb"), format!("{dir}/dbine_cmp_b.fdb"));
    for p in [&pa, &pb] {
        let _ = admin.drop_database(p).await;
        admin.create_database(p).await.expect("create database");
    }
    let at = |p: &str| ConnectionConfig { database: p.into(), ..base.clone() };
    let mut a = d.connect(&at(&pa), None).await.unwrap();
    let mut b = d.connect(&at(&pb), None).await.unwrap();
    run(&mut a, A).await;
    run(&mut b, B).await;

    let (ta, oa) = read(&mut a).await;
    let (tb, ob) = read(&mut b).await;
    for ((k, n), def) in &oa {
        eprintln!("-- {k} {n}\n{def}");
    }
    let docs = &ta["DOCS"];
    let checks: Vec<(Option<&str>, &str)> = docs.checks.iter().map(|c| (c.name.as_deref(), c.expression.as_str())).collect();
    assert_eq!(checks, [(Some("CK_DOCS"), "(N < 1000 OR M > 0)"), (None, "(N > 0)")]);
    let ix = |n: &str| docs.indexes.iter().find(|i| i.name == n).cloned().unwrap();
    assert_eq!(ix("IX_N").kind.as_deref(), Some("DESC"));
    assert_eq!(ix("IX_UP").columns, ["(UPPER(T))"]);
    assert_eq!(ix("IX_M").filter.as_deref(), Some("M > 0"));
    assert_eq!(ix("IX_T").options.get("inactive").map(String::as_str), Some("true"));
    assert_eq!(
        oa[&("type".into(), "D_EMAIL".into())],
        "CREATE DOMAIN \"D_EMAIL\" AS VARCHAR(120) CHARACTER SET UTF8 DEFAULT 'x@y' NOT NULL CHECK (VALUE LIKE '%@%') COLLATE UNICODE_CI;"
    );
    assert_eq!(oa[&("sequence".into(), "SQ_FOLIO".into())], "CREATE SEQUENCE \"SQ_FOLIO\" START WITH 100 INCREMENT BY 5;");

    let mut tables = Vec::new();
    for (name, t) in &ta {
        match tb.get(name) {
            None => tables.push(TableChange::Create { table: t.clone() }),
            Some(o) if o != t => tables.push(TableChange::Alter { old: o.clone(), new: t.clone() }),
            Some(_) => {}
        }
    }
    assert_eq!(tables.len(), 1);
    let (mut before, mut early, mut after, mut late) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for ((k, n), def) in &oa {
        match ob.get(&(k.clone(), n.clone())) {
            Some(o) if o == def => continue,
            Some(_) => before.push(drop_other(k, n)),
            None => {}
        }
        if PREREQS.contains(&k.as_str()) { early.push(def.clone()) } else { after.push(def.clone()) }
    }
    for (k, n) in ob.keys() {
        if !oa.contains_key(&(k.clone(), n.clone())) {
            let drop = drop_other(k, n);
            if PREREQS.contains(&k.as_str()) { late.push(drop) } else { before.push(drop) }
        }
    }
    let script = d.sync_script(&tables).unwrap();
    for w in &script.warnings {
        eprintln!("aviso: {w}");
    }
    let all: Vec<String> = before.into_iter().chain(early).chain(script.statements).chain(after).chain(late).collect();
    for s in &all {
        run(&mut b, s).await;
    }

    let (tb2, ob2) = read(&mut b).await;
    assert_eq!(tb2, ta);
    assert_eq!(ob2, oa);
    let mut out = QueryOutcome::default();
    b.execute("SELECT T FROM DOCS", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows, [[serde_json::json!("a")]]);

    drop((a, b));
    for p in [&pa, &pb] {
        admin.drop_database(p).await.expect("drop database");
    }
}
