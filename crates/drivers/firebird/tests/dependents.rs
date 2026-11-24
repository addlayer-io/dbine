//! What depends on an object (`Session::dependents`) against a real
//! server, from `RDB$DEPENDENCIES` and the foreign keys: a table's views,
//! routines, package, trigger, foreign key, CHECK, expression index and
//! computed column, a column's exact dependents, a function's, a
//! sequence's, and a procedure that reaches the table only through
//! `EXECUTE STATEMENT`. Reads `DBINE_TEST_FIREBIRD_URL`:
//!
//! ```sh
//! DBINE_TEST_FIREBIRD_URL=firebird://dbine:dbine@localhost:25602//var/lib/firebird/data/test.fdb \
//!   cargo test -p dbine-driver-firebird --test dependents -- --ignored --nocapture
//! ```
//!
//! The test's objects carry a prefix of their own (`DBD<n>_`) and are
//! dropped at the end, also when a check failed.

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

fn check(ok: bool, what: impl Into<String>) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(what.into())
    }
}

async fn deps(
    s: &mut Box<dyn Session>,
    scan: &DependencyScan,
    kind: &str,
    name: String,
    column: Option<&str>,
) -> Result<DependencyReport, String> {
    let r = s
        .dependents(&target(kind, &name, column), scan)
        .await
        .map_err(|e| format!("dependents {name}: {e}"))?;
    println!("{name} {column:?}: {r:#?}");
    Ok(r)
}

fn is(
    r: &DependencyReport,
    kind: &str,
    name: &str,
    relation: Relation,
    c: Confidence,
) -> Result<(), String> {
    let d = r
        .items
        .iter()
        .find(|d| d.kind == kind && d.name == name && d.relation == relation)
        .ok_or_else(|| format!("{kind} {name} {relation:?} missing: {r:#?}"))?;
    check(d.confidence == c, format!("{name}: {d:#?}"))
}

fn lacks(r: &DependencyReport, name: &str) -> Result<(), String> {
    check(
        !r.items.iter().any(|d| d.name == name),
        format!("{name} doesn't use it: {r:#?}"),
    )
}

async fn checks(s: &mut Box<dyn Session>, scan: &DependencyScan, p: &str) -> Result<(), String> {
    use Confidence::{Confirmed, Probable, Review};
    let n = |x: &str| format!("{p}{x}");

    // The table: keys, views, routines, the package, the trigger, its own
    // CHECK, expression index and computed column.
    let r = deps(s, scan, kinds::TABLE, n("ORDERS"), None).await?;
    is(
        &r,
        kinds::TABLE,
        &n("LINES"),
        Relation::ForeignKey,
        Confirmed,
    )?;
    is(&r, kinds::VIEW, &n("V_TOTALS"), Relation::Code, Confirmed)?;
    is(
        &r,
        kinds::PROCEDURE,
        &n("P_LOAD"),
        Relation::Code,
        Confirmed,
    )?;
    is(&r, "package", &n("PKG"), Relation::Code, Confirmed)?;
    is(&r, kinds::TRIGGER, &n("TRG"), Relation::Code, Confirmed)?;
    is(&r, kinds::TABLE, &n("ORDERS"), Relation::Check, Confirmed)?;
    is(&r, kinds::TABLE, &n("ORDERS"), Relation::Index, Confirmed)?;
    // Only a string names it in the dynamic one: the text finds it.
    is(&r, kinds::PROCEDURE, &n("P_DYN"), Relation::Code, Review)?;
    for name in [n("V_TOTALS"), n("P_LOAD"), n("PKG"), n("P_DYN")] {
        check(
            r.items
                .iter()
                .any(|d| d.name == name && !d.mentions.is_empty()),
            format!("{name} without lines"),
        )?;
    }
    for name in [n("V_OLD"), n("ORDERS_OLD"), n("F_COUNT"), n("P_ALL")] {
        lacks(&r, &name)?;
    }
    check(r.note.is_none(), format!("note: {:?}", r.note))?;

    // The contract's scan finds nothing the catalog lacks.
    let generic =
        dbine_driver::dependencies::scan(&mut **s, &target(kinds::TABLE, &n("ORDERS"), None), scan)
            .await
            .map_err(|e| format!("generic: {e}"))?;
    for g in &generic.items {
        check(
            r.items.iter().any(|d| d.name == g.name && d.kind == g.kind),
            format!("the scan finds {} and the catalog doesn't", g.name),
        )?;
    }

    // A column: exact in the catalog, so the view over TOTAL only.
    let r = deps(s, scan, kinds::TABLE, n("ORDERS"), Some("TOTAL")).await?;
    is(&r, kinds::VIEW, &n("V_TOTALS"), Relation::Code, Confirmed)?;
    is(&r, kinds::TABLE, &n("ORDERS"), Relation::Index, Confirmed)?;
    is(&r, kinds::TABLE, &n("ORDERS"), Relation::Check, Confirmed)?;
    lacks(&r, &n("V_IDS"))?;
    let r = deps(s, scan, kinds::TABLE, n("ORDERS"), Some("ID")).await?;
    is(
        &r,
        kinds::TABLE,
        &n("LINES"),
        Relation::ForeignKey,
        Confirmed,
    )?;

    // A function, a view, a sequence, a domain.
    let r = deps(s, scan, kinds::FUNCTION, n("F_COUNT"), None).await?;
    is(
        &r,
        kinds::PROCEDURE,
        &n("P_LOAD"),
        Relation::Code,
        Confirmed,
    )?;
    let r = deps(s, scan, kinds::VIEW, n("V_TOTALS"), None).await?;
    is(
        &r,
        kinds::FUNCTION,
        &n("F_COUNT"),
        Relation::Code,
        Confirmed,
    )?;
    let r = deps(s, scan, kinds::SEQUENCE, n("SEQ"), None).await?;
    is(
        &r,
        kinds::PROCEDURE,
        &n("P_LOAD"),
        Relation::Code,
        Confirmed,
    )?;
    let _ = Probable;
    Ok(())
}

#[tokio::test]
#[ignore]
async fn dependents_from_the_catalog() {
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
    let p = format!("DBD{stamp}_");
    let n = |x: &str| format!("{p}{x}");

    let create = [
        format!("CREATE SEQUENCE {}", n("SEQ")),
        format!(
            "CREATE TABLE {} (ID INTEGER NOT NULL PRIMARY KEY, TOTAL INTEGER, DOBLE COMPUTED BY (TOTAL * 2), \
             CONSTRAINT {} CHECK (TOTAL >= 0))",
            n("ORDERS"),
            n("CK_TOTAL")
        ),
        format!("CREATE INDEX {} ON {} COMPUTED BY (TOTAL + 1)", n("IX_EXPR"), n("ORDERS")),
        format!("CREATE TABLE {} (ID INTEGER)", n("ORDERS_OLD")),
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
        format!("CREATE VIEW {} AS SELECT ID FROM {}", n("V_IDS"), n("ORDERS")),
        format!("CREATE VIEW {} AS SELECT ID FROM {}", n("V_OLD"), n("ORDERS_OLD")),
        format!(
            "CREATE FUNCTION {} (X INTEGER) RETURNS INTEGER AS\nBEGIN\n  RETURN (SELECT COUNT(*) FROM {} WHERE ID = :X);\nEND",
            n("F_COUNT"),
            n("V_TOTALS")
        ),
        format!(
            "CREATE PROCEDURE {} (X INTEGER) AS\nDECLARE N INTEGER;\nBEGIN\n  N = NEXT VALUE FOR {};\n  INSERT INTO {} (ID, TOTAL) VALUES (:N, {}(:X));\nEND",
            n("P_LOAD"),
            n("SEQ"),
            n("ORDERS"),
            n("F_COUNT")
        ),
        format!(
            "CREATE PROCEDURE {} AS\nBEGIN\n  EXECUTE STATEMENT 'DELETE FROM {}';\nEND",
            n("P_DYN"),
            n("ORDERS")
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
            "CREATE PROCEDURE {} RETURNS (R INTEGER) AS\nBEGIN\n  EXECUTE PROCEDURE {}(1);\n  SUSPEND;\nEND",
            n("P_ALL"),
            n("P_LOAD")
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
        format!("DROP PROCEDURE {}", n("P_DYN")),
        format!("DROP PROCEDURE {}", n("P_LOAD")),
        format!("DROP FUNCTION {}", n("F_COUNT")),
        format!("DROP VIEW {}", n("V_OLD")),
        format!("DROP VIEW {}", n("V_IDS")),
        format!("DROP VIEW {}", n("V_TOTALS")),
        format!("DROP TABLE {}", n("LINES")),
        format!("DROP INDEX {}", n("IX_EXPR")),
        format!("DROP TABLE {}", n("ORDERS_OLD")),
        format!("DROP TABLE {}", n("ORDERS")),
        format!("DROP SEQUENCE {}", n("SEQ")),
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
    .unwrap();
    let left = left
        .results
        .last()
        .and_then(|r| r.rows.first())
        .and_then(|r| r.first())
        .map(|v| v.to_string())
        .unwrap_or_default();
    assert_eq!(left.trim_matches('"'), "0", "leftovers");
    result.unwrap();
}
