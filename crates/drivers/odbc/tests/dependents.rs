//! What depends on an object (`Session::dependents`) on Db2 LUW, against a
//! real server through IBM's CLI / ODBC driver and the "db2" preset: the
//! views, routines, trigger and alias the dependency catalog records for a
//! table, the foreign key that points at it, a routine that reaches it only
//! through dynamic SQL, a function's callers, a column's index and check;
//! the native answer is compared with the contract's scan.
//!
//! The test makes its own schema (`DBD<n>`) and drops it at the end, also
//! when a check failed.
//!
//! ```sh
//! DBINE_TEST_ODBC_DB2='host=127.0.0.1;port=25037;database=testdb;user=db2inst1;password=pw;odbc_driver=IBM DB2 ODBC DRIVER' \
//!   cargo test -p dbine-driver-odbc --test dependents -- --ignored --nocapture
//! ```

use dbine_driver::{
    kinds, Confidence, ConnectionConfig, DependencyReport, DependencyScan, DependencyTarget,
    Driver, ObjectRef, QueryOutcome, Relation, Session,
};
use std::sync::Arc;

fn cfg() -> Option<ConnectionConfig> {
    let raw = std::env::var("DBINE_TEST_ODBC_DB2").ok()?;
    let mut c = ConnectionConfig {
        driver: "db2".into(),
        ..Default::default()
    };
    for part in raw.split(';') {
        let Some((k, v)) = part.split_once('=') else {
            continue;
        };
        match k.trim() {
            "host" => c.host = v.into(),
            "port" => c.port = v.parse().unwrap_or(0),
            "database" => c.database = v.into(),
            "user" => c.username = Some(v.into()),
            "password" => c.password = Some(v.into()),
            key => {
                c.options.insert(key.into(), v.into());
            }
        }
    }
    Some(c)
}

async fn exec(s: &mut Box<dyn Session>, sql: &str) -> Result<QueryOutcome, String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out)
        .await
        .map_err(|e| format!("{e}\n---\n{sql}"))?;
    match out.error.take() {
        Some(e) => Err(format!("{e}\n---\n{sql}")),
        None => Ok(out),
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
    sc: &str,
    name: &str,
    column: Option<&str>,
) -> Result<DependencyReport, String> {
    let t = DependencyTarget {
        object: ObjectRef {
            kind: kind.into(),
            schema: Some(sc.into()),
            name: name.into(),
        },
        column: column.map(Into::into),
    };
    let r = s
        .dependents(&t, scan)
        .await
        .map_err(|e| format!("dependents {name}: {e}"))?;
    println!("{name} {column:?}: {r:#?}");
    Ok(r)
}

fn item<'a>(r: &'a DependencyReport, name: &str) -> Option<&'a dbine_driver::Dependent> {
    r.items.iter().find(|d| d.name == name)
}

fn is(r: &DependencyReport, name: &str, relation: Relation, c: Confidence) -> Result<(), String> {
    let d = item(r, name).ok_or_else(|| format!("{name} missing: {r:#?}"))?;
    check(
        d.relation == relation && d.confidence == c,
        format!("{name}: {d:#?}"),
    )
}

async fn checks(s: &mut Box<dyn Session>, scan: &DependencyScan, sc: &str) -> Result<(), String> {
    use Confidence::{Confirmed, Probable, Review};
    let r = deps(s, scan, kinds::TABLE, sc, "ORDERS", None).await?;
    is(&r, "LINES", Relation::ForeignKey, Confirmed)?;
    is(&r, "V_TOTALS", Relation::Code, Confirmed)?;
    is(&r, "P_LOAD", Relation::Code, Confirmed)?;
    is(&r, "TRG", Relation::Code, Confirmed)?;
    is(&r, "A_ORDERS", Relation::Code, Confirmed)?;
    // The catalog doesn't record dynamic SQL: the text finds it.
    is(&r, "F_SUM", Relation::Code, Confirmed)?;
    is(&r, "P_DYN", Relation::Code, Review)?;
    check(
        item(&r, "P_DYN").is_some_and(|d| d.mentions.iter().any(|m| m.dynamic)),
        "P_DYN mentions",
    )?;
    for n in ["V_TOTALS", "P_LOAD", "TRG", "P_DYN"] {
        check(
            item(&r, n).is_some_and(|d| !d.mentions.is_empty()),
            format!("{n} without lines"),
        )?;
    }
    for not in ["V_OLD", "ORDERS_OLD", "ORDERS", "F_COUNT", "P_ALL"] {
        check(item(&r, not).is_none(), format!("{not} doesn't use ORDERS"))?;
    }
    check(r.note.is_none(), format!("note: {:?}", r.note))?;

    // The contract's scan finds nothing the native read lacks.
    let t = DependencyTarget {
        object: ObjectRef {
            kind: kinds::TABLE.into(),
            schema: Some(sc.into()),
            name: "ORDERS".into(),
        },
        column: None,
    };
    let generic = dbine_driver::dependencies::scan(&mut **s, &t, scan)
        .await
        .map_err(|e| format!("generic: {e}"))?;
    for g in &generic.items {
        check(
            item(&r, &g.name).is_some(),
            format!("the scan finds {} and the catalog doesn't", g.name),
        )?;
    }

    // A function: its callers, through the routine catalog.
    let r = deps(s, scan, kinds::FUNCTION, sc, "F_SUM", None).await?;
    is(&r, "P_LOAD", Relation::Code, Confirmed)?;
    check(r.items.len() == 1, format!("only P_LOAD: {r:#?}"))?;
    // A procedure and a view.
    let r = deps(s, scan, kinds::PROCEDURE, sc, "P_LOAD", None).await?;
    is(&r, "P_ALL", Relation::Code, Confirmed)?;
    let r = deps(s, scan, kinds::VIEW, sc, "V_TOTALS", None).await?;
    is(&r, "F_COUNT", Relation::Code, Confirmed)?;

    // A column: the views that name it, its index and check, the key.
    let r = deps(s, scan, kinds::TABLE, sc, "ORDERS", Some("TOTAL")).await?;
    is(&r, "V_TOTALS", Relation::Code, Probable)?;
    is(&r, "IX_TOTAL", Relation::Code, Confirmed).or_else(|_| {
        check(
            r.items.iter().any(|d| {
                d.relation == Relation::Index && d.detail.as_deref() == Some("IX_TOTAL (TOTAL)")
            }),
            format!("the index on TOTAL: {r:#?}"),
        )
    })?;
    check(
        r.items.iter().any(|d| d.relation == Relation::Check),
        format!("the check on TOTAL: {r:#?}"),
    )?;
    check(item(&r, "V_IDS").is_none(), "V_IDS doesn't name TOTAL")?;
    let r = deps(s, scan, kinds::TABLE, sc, "ORDERS", Some("ID")).await?;
    check(
        r.items
            .iter()
            .any(|d| d.name == "LINES" && d.relation == Relation::ForeignKey),
        format!("LINES' key: {r:#?}"),
    )
}

#[tokio::test]
#[ignore]
async fn dependents_from_the_catalog() {
    let Some(c) = cfg() else {
        eprintln!("DBINE_TEST_ODBC_DB2 not set; skipping");
        return;
    };
    let driver: Arc<dyn Driver> = dbine_driver_odbc::drivers()
        .into_iter()
        .find(|d| d.info().id == "db2")
        .expect("db2 preset");
    let mut s = driver.connect(&c, None).await.expect("connect");
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
    let sc = format!("DBD{stamp}");
    let create = [
        format!("CREATE SCHEMA {sc}"),
        format!("CREATE TABLE {sc}.ORDERS (ID INT NOT NULL PRIMARY KEY, TOTAL INT, CONSTRAINT CK_TOTAL CHECK (TOTAL >= 0))"),
        format!("CREATE INDEX {sc}.IX_TOTAL ON {sc}.ORDERS (TOTAL)"),
        format!("CREATE TABLE {sc}.ORDERS_OLD (ID INT)"),
        format!(
            "CREATE TABLE {sc}.LINES (ID INT NOT NULL PRIMARY KEY, ORDER_ID INT, QTY INT, \
             CONSTRAINT FK_LINES FOREIGN KEY (ORDER_ID) REFERENCES {sc}.ORDERS (ID))"
        ),
        format!("CREATE ALIAS {sc}.A_ORDERS FOR {sc}.ORDERS"),
        format!("CREATE VIEW {sc}.V_TOTALS AS SELECT o.ID, o.TOTAL, l.QTY FROM {sc}.ORDERS o JOIN {sc}.LINES l ON l.ORDER_ID = o.ID"),
        format!("CREATE VIEW {sc}.V_IDS AS SELECT ID FROM {sc}.ORDERS"),
        format!("CREATE VIEW {sc}.V_OLD AS SELECT ID FROM {sc}.ORDERS_OLD"),
        format!(
            "CREATE FUNCTION {sc}.F_COUNT (X INT) RETURNS INT LANGUAGE SQL\nRETURN (SELECT COUNT(*) FROM {sc}.V_TOTALS WHERE ID = X)"
        ),
        format!(
            "CREATE FUNCTION {sc}.F_SUM (X INT) RETURNS INT LANGUAGE SQL\nBEGIN\n  DECLARE N INT;\n  SET N = (SELECT SUM(TOTAL) FROM {sc}.ORDERS WHERE ID > X);\n  RETURN N;\nEND"
        ),
        format!(
            "CREATE PROCEDURE {sc}.P_LOAD (IN X INT) LANGUAGE SQL\nBEGIN\n  INSERT INTO {sc}.ORDERS (ID, TOTAL) VALUES (X, {sc}.F_SUM(X));\nEND"
        ),
        format!("CREATE PROCEDURE {sc}.P_ALL () LANGUAGE SQL\nBEGIN\n  CALL {sc}.P_LOAD(1);\nEND"),
        format!(
            "CREATE PROCEDURE {sc}.P_DYN () LANGUAGE SQL\nBEGIN\n  EXECUTE IMMEDIATE 'DELETE FROM {sc}.ORDERS';\nEND"
        ),
        format!(
            "CREATE TRIGGER {sc}.TRG AFTER INSERT ON {sc}.ORDERS REFERENCING NEW AS N FOR EACH ROW\nBEGIN ATOMIC\n  INSERT INTO {sc}.LINES (ID, ORDER_ID, QTY) VALUES (N.ID, N.ID, 1);\nEND"
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
        result = checks(&mut s, &scan, &sc).await;
    }

    // Everything the test made, also after a failure.
    for d in [
        format!("DROP TRIGGER {sc}.TRG"),
        format!("DROP PROCEDURE {sc}.P_DYN"),
        format!("DROP PROCEDURE {sc}.P_ALL"),
        format!("DROP PROCEDURE {sc}.P_LOAD"),
        format!("DROP FUNCTION {sc}.F_SUM"),
        format!("DROP FUNCTION {sc}.F_COUNT"),
        format!("DROP VIEW {sc}.V_OLD"),
        format!("DROP VIEW {sc}.V_IDS"),
        format!("DROP VIEW {sc}.V_TOTALS"),
        format!("DROP ALIAS {sc}.A_ORDERS"),
        format!("DROP TABLE {sc}.LINES"),
        format!("DROP TABLE {sc}.ORDERS_OLD"),
        format!("DROP TABLE {sc}.ORDERS"),
        format!("DROP SCHEMA {sc} RESTRICT"),
    ] {
        if let Err(e) = exec(&mut s, &d).await {
            eprintln!("cleanup: {e}");
        }
    }
    let left = exec(
        &mut s,
        &format!("SELECT COUNT(*) FROM SYSCAT.SCHEMATA WHERE SCHEMANAME = '{sc}'"),
    )
    .await
    .ok()
    .and_then(|o| {
        o.results
            .into_iter()
            .rev()
            .find(|r| !r.columns.is_empty())
            .and_then(|r| r.rows.first().and_then(|row| row.first().cloned()))
    })
    .map(|v| match v {
        serde_json::Value::String(v) => v.trim().to_string(),
        v => v.to_string(),
    });
    assert_eq!(left.as_deref(), Some("0"), "leftovers");
    result.unwrap();
}
