//! What an object uses (`Session::references`) against a real server, from
//! `RDB$DEPENDENCIES` and the foreign keys. Reads `DBINE_TEST_FIREBIRD_URL`
//! like `integration.rs`:
//!
//! ```sh
//! DBINE_TEST_FIREBIRD_URL=firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb \
//!   cargo test -p dbine-driver-firebird --test references -- --ignored --nocapture
//! ```
//!
//! A Firebird database is one namespace: the test's objects carry a prefix
//! of their own (`DBR<n>_`) and are dropped at the end, also when a check
//! failed.

use dbine_driver::{
    kinds, Confidence, ConnectionConfig, DependencyReport, DependencyScan, DependencyTarget,
    Driver, ObjectRef, QueryOutcome, Relation, Session,
};
use std::sync::Arc;

fn config(url: &str) -> ConnectionConfig {
    let rest = url
        .strip_prefix("firebird://")
        .expect("firebird://user:pass@host:port/path");
    let (cred, addr) = rest.split_once('@').unwrap();
    let (user, pass) = cred.split_once(':').unwrap();
    let (hostport, path) = addr.split_once('/').unwrap();
    let (host, port) = hostport.split_once(':').unwrap();
    ConnectionConfig {
        driver: "firebird".into(),
        host: host.into(),
        port: port.parse().unwrap(),
        database: path.into(),
        username: Some(user.into()),
        password: Some(pass.into()),
        ..Default::default()
    }
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out)
        .await
        .map_err(|e| format!("{e}\n---\n{sql}"))?;
    match out.error.take() {
        Some(e) => Err(format!("{e}\n---\n{sql}")),
        None => Ok(()),
    }
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

fn check(ok: bool, what: impl Into<String>) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(what.into())
    }
}

/// The item for `name` of `kind`, confirmed and with the relation given.
fn confirmed(
    r: &DependencyReport,
    kind: &str,
    name: &str,
    relation: Relation,
) -> Result<(), String> {
    let found = r
        .items
        .iter()
        .find(|d| d.kind == kind && d.name == name)
        .ok_or_else(|| format!("{kind} {name} missing: {r:#?}"))?;
    check(
        found.confidence == Confidence::Confirmed && found.relation == relation,
        format!("{kind} {name} not confirmed as {relation:?}: {found:#?}"),
    )
}

fn mentioned(r: &DependencyReport, name: &str) -> Result<(), String> {
    check(
        r.items
            .iter()
            .any(|d| d.name == name && !d.mentions.is_empty()),
        format!("{name} without mentions: {r:#?}"),
    )
}

async fn refs(
    s: &mut Box<dyn Session>,
    scan: &DependencyScan,
    kind: &str,
    name: String,
) -> Result<DependencyReport, String> {
    s.references(&target(kind, &name), scan)
        .await
        .map_err(|e| format!("references {name}: {e}"))
}

async fn checks(s: &mut Box<dyn Session>, scan: &DependencyScan, p: &str) -> Result<(), String> {
    let n = |x: &str| format!("{p}{x}");

    // A table: what its foreign key points at.
    let r = refs(s, scan, kinds::TABLE, n("LINES")).await?;
    confirmed(&r, kinds::TABLE, &n("ORDERS"), Relation::ForeignKey)?;
    check(r.items.len() == 1, format!("one parent: {r:#?}"))?;
    check(
        r.items[0].detail.as_deref()
            == Some(format!("{} (ORDER_ID) → {} (ID)", n("FK"), n("ORDERS")).as_str()),
        format!("FK detail: {r:#?}"),
    )?;
    let r = refs(s, scan, kinds::TABLE, n("ORDERS")).await?;
    check(r.items.is_empty(), format!("no parents: {r:#?}"))?;

    // A view: both tables, once each (the column rows collapse).
    let r = refs(s, scan, kinds::VIEW, n("V_TOTALS")).await?;
    confirmed(&r, kinds::TABLE, &n("ORDERS"), Relation::Code)?;
    confirmed(&r, kinds::TABLE, &n("LINES"), Relation::Code)?;
    check(r.items.len() == 2, format!("two tables: {r:#?}"))?;
    mentioned(&r, &n("LINES"))?;
    check(r.scanned == 1, "the view's source read")?;

    // A function on the view.
    let r = refs(s, scan, kinds::FUNCTION, n("F_COUNT")).await?;
    confirmed(&r, kinds::VIEW, &n("V_TOTALS"), Relation::Code)?;
    mentioned(&r, &n("V_TOTALS"))?;

    // A procedure: tables, the function, the generator, the domain.
    let r = refs(s, scan, kinds::PROCEDURE, n("P_LOAD")).await?;
    for (kind, name) in [
        (kinds::TABLE, n("ORDERS")),
        (kinds::TABLE, n("LINES")),
        (kinds::FUNCTION, n("F_COUNT")),
        (kinds::SEQUENCE, n("SEQ")),
        (kinds::TYPE, n("D_QTY")),
    ] {
        confirmed(&r, kind, &name, Relation::Code)?;
    }
    mentioned(&r, &n("SEQ"))?;
    check(
        !r.items.iter().any(|d| d.name == n("P_LOAD")),
        "the procedure doesn't use itself",
    )?;

    // A procedure calling another and a packaged function.
    let r = refs(s, scan, kinds::PROCEDURE, n("P_ALL")).await?;
    confirmed(&r, kinds::PROCEDURE, &n("P_LOAD"), Relation::Code)?;
    confirmed(&r, "package", &n("PKG"), Relation::Code)?;
    let pkg = r.items.iter().find(|d| d.name == n("PKG")).unwrap();
    check(
        pkg.detail.as_deref() == Some("TOTAL"),
        format!("the package's routine: {pkg:#?}"),
    )?;

    // A package body: its table.
    let r = refs(s, scan, "package", n("PKG")).await?;
    confirmed(&r, kinds::TABLE, &n("ORDERS"), Relation::Code)?;
    check(
        !r.items.iter().any(|d| d.name == n("PKG")),
        format!("the package doesn't use itself: {r:#?}"),
    )?;

    // A trigger: its table and the one it writes.
    let r = refs(s, scan, kinds::TRIGGER, n("TRG")).await?;
    confirmed(&r, kinds::TABLE, &n("ORDERS"), Relation::Code)?;
    confirmed(&r, kinds::TABLE, &n("LINES"), Relation::Code)?;

    // Every item is an object of the database, not a system one.
    let r = refs(s, scan, kinds::PROCEDURE, n("P_LOAD")).await?;
    check(
        r.items.iter().all(|d| !d.name.starts_with("RDB$")),
        format!("no system objects: {r:#?}"),
    )?;
    check(
        r.external.is_empty(),
        "Firebird records no external sources",
    )?;
    check(r.note.is_none(), format!("no note: {:?}", r.note))?;
    Ok(())
}

#[tokio::test]
#[ignore]
async fn references_from_the_catalog() {
    let Ok(url) = std::env::var("DBINE_TEST_FIREBIRD_URL") else {
        eprintln!("DBINE_TEST_FIREBIRD_URL not set; skipping");
        return;
    };
    let driver: Arc<dyn Driver> = dbine_driver_firebird::drivers().remove(0);
    let mut s = driver.connect(&config(&url), None).await.expect("connect");
    let scan = DependencyScan::new(
        driver.info(),
        driver.script_dialect(),
        driver.capabilities().foreign_keys,
    );
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        % 100_000_000;
    let p = format!("DBR{stamp}_");
    let n = |x: &str| format!("{p}{x}");

    let create = [
        format!("CREATE DOMAIN {} AS INTEGER CHECK (VALUE > 0)", n("D_QTY")),
        format!("CREATE SEQUENCE {}", n("SEQ")),
        format!(
            "CREATE TABLE {} (ID INTEGER NOT NULL PRIMARY KEY, TOTAL INTEGER)",
            n("ORDERS")
        ),
        format!(
            "CREATE TABLE {} (ID INTEGER NOT NULL PRIMARY KEY, ORDER_ID INTEGER, QTY INTEGER, \
             CONSTRAINT {} FOREIGN KEY (ORDER_ID) REFERENCES {} (ID))",
            n("LINES"),
            n("FK"),
            n("ORDERS")
        ),
        format!(
            "CREATE VIEW {} AS SELECT o.ID, o.TOTAL, l.QTY FROM {} o JOIN {} l ON l.ORDER_ID = o.ID",
            n("V_TOTALS"),
            n("ORDERS"),
            n("LINES")
        ),
        format!(
            "CREATE FUNCTION {} (X INTEGER) RETURNS INTEGER AS\nBEGIN\n  RETURN (SELECT COUNT(*) FROM {} WHERE ID = :X);\nEND",
            n("F_COUNT"),
            n("V_TOTALS")
        ),
        format!(
            "CREATE PROCEDURE {} (X {}) AS\nDECLARE N INTEGER;\nBEGIN\n  N = NEXT VALUE FOR {};\n  INSERT INTO {} (ID, TOTAL) VALUES (:N, {}(:X));\n  INSERT INTO {} (ID, ORDER_ID, QTY) VALUES (:N, :N, :X);\nEND",
            n("P_LOAD"),
            n("D_QTY"),
            n("SEQ"),
            n("ORDERS"),
            n("F_COUNT"),
            n("LINES")
        ),
        format!(
            "CREATE PACKAGE {} AS\nBEGIN\n  FUNCTION TOTAL RETURNS INTEGER;\nEND",
            n("PKG")
        ),
        format!(
            "CREATE PACKAGE BODY {} AS\nBEGIN\n  FUNCTION TOTAL RETURNS INTEGER AS\n  BEGIN\n    RETURN (SELECT COUNT(*) FROM {});\n  END\nEND",
            n("PKG"),
            n("ORDERS")
        ),
        format!(
            "CREATE PROCEDURE {} RETURNS (R INTEGER) AS\nBEGIN\n  EXECUTE PROCEDURE {}(1);\n  R = {}.TOTAL();\n  SUSPEND;\nEND",
            n("P_ALL"),
            n("P_LOAD"),
            n("PKG")
        ),
        format!(
            "CREATE TRIGGER {} FOR {} AFTER INSERT AS\nBEGIN\n  INSERT INTO {} (ID, ORDER_ID, QTY) VALUES (NEW.ID, NEW.ID, 1);\nEND",
            n("TRG"),
            n("ORDERS"),
            n("LINES")
        ),
    ];
    let mut result = Ok(());
    for sql in &create {
        if let Err(e) = exec(&mut s, sql).await {
            result = Err(e);
            break;
        }
    }
    if result.is_ok() {
        result = checks(&mut s, &scan, &p).await;
    }

    // Everything the test made, also after a failure.
    for drop in [
        format!("DROP TRIGGER {}", n("TRG")),
        format!("DROP PROCEDURE {}", n("P_ALL")),
        format!("DROP PACKAGE BODY {}", n("PKG")),
        format!("DROP PACKAGE {}", n("PKG")),
        format!("DROP PROCEDURE {}", n("P_LOAD")),
        format!("DROP FUNCTION {}", n("F_COUNT")),
        format!("DROP VIEW {}", n("V_TOTALS")),
        format!("DROP TABLE {}", n("LINES")),
        format!("DROP TABLE {}", n("ORDERS")),
        format!("DROP SEQUENCE {}", n("SEQ")),
        format!("DROP DOMAIN {}", n("D_QTY")),
    ] {
        if let Err(e) = exec(&mut s, &drop).await {
            eprintln!("cleanup: {e}");
        }
    }
    let mut left = QueryOutcome::default();
    s.execute(
        &format!("SELECT COUNT(*) FROM RDB$RELATIONS WHERE RDB$RELATION_NAME STARTING WITH '{p}'"),
        10,
        &mut left,
    )
    .await
    .expect("count leftovers");
    assert_eq!(
        left.results[0].rows[0][0],
        serde_json::json!(0),
        "leftovers"
    );
    result.unwrap();
}
