//! What an object uses (`Session::references`) on Db2 LUW, against a real
//! server through IBM's CLI / ODBC driver and the "db2" preset: views,
//! inline and compiled SQL functions, SQL procedures (their package's
//! dependencies), a trigger, an alias, a table's foreign key and, through
//! federation, a nickname's remote table in `external`.
//!
//! The test makes its own schema (`DBR<n>`) and, for the nickname, a
//! federated server on the same database (and the `DRDA` wrapper when it
//! isn't there), and drops them all at the end, also when a check failed.
//! Without federation (`FEDERATED` = NO) the nickname's check is skipped.
//!
//! The connection, as in `tests/db2.rs`:
//!
//! ```sh
//! DBINE_TEST_ODBC_DB2='host=127.0.0.1;port=50000;database=testdb;user=db2inst1;password=…;odbc_driver=IBM DB2 ODBC DRIVER' \
//!   cargo test -p dbine-driver-odbc --test references -- --ignored --nocapture
//! ```

use dbine_driver::dependencies::ExternalRef;
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

/// The first cell of `sql`'s last result, as text.
async fn scalar(s: &mut Box<dyn Session>, sql: &str) -> Result<String, String> {
    let out = exec(s, sql).await?;
    let r = out
        .results
        .into_iter()
        .rev()
        .find(|r| !r.columns.is_empty())
        .ok_or_else(|| format!("no result: {sql}"))?;
    Ok(match r.rows.first().and_then(|row| row.first()) {
        Some(serde_json::Value::String(v)) => v.trim().to_string(),
        Some(v) => v.to_string(),
        None => String::new(),
    })
}

fn check(ok: bool, what: impl Into<String>) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(what.into())
    }
}

async fn refs(
    s: &mut Box<dyn Session>,
    scan: &DependencyScan,
    kind: &str,
    schema: &str,
    name: &str,
) -> Result<DependencyReport, String> {
    let t = DependencyTarget {
        object: ObjectRef {
            kind: kind.into(),
            schema: Some(schema.into()),
            name: name.into(),
        },
        column: None,
    };
    s.references(&t, scan)
        .await
        .map_err(|e| format!("references {name}: {e}"))
}

/// The item for `schema.name` of `kind`, confirmed and with the relation
/// given.
fn confirmed(
    r: &DependencyReport,
    kind: &str,
    schema: &str,
    name: &str,
    relation: Relation,
) -> Result<(), String> {
    let found = r
        .items
        .iter()
        .find(|d| d.kind == kind && d.name == name && d.schema.as_deref() == Some(schema))
        .ok_or_else(|| format!("{kind} {schema}.{name} missing: {r:#?}"))?;
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

async fn checks(
    s: &mut Box<dyn Session>,
    scan: &DependencyScan,
    sc: &str,
    remote: Option<(&str, &str)>,
) -> Result<(), String> {
    // A table: what its foreign key points at.
    let r = refs(s, scan, kinds::TABLE, sc, "LINES").await?;
    confirmed(&r, kinds::TABLE, sc, "ORDERS", Relation::ForeignKey)?;
    check(r.items.len() == 1, format!("one parent: {r:#?}"))?;
    check(
        r.items[0].detail.as_deref()
            == Some(format!("FK_LINES (ORDER_ID) → {sc}.ORDERS (ID)").as_str()),
        format!("FK detail: {r:#?}"),
    )?;
    let r = refs(s, scan, kinds::TABLE, sc, "ORDERS").await?;
    check(r.items.is_empty(), format!("no parents: {r:#?}"))?;

    // A view: both tables.
    let r = refs(s, scan, kinds::VIEW, sc, "V_TOTALS").await?;
    confirmed(&r, kinds::TABLE, sc, "ORDERS", Relation::Code)?;
    confirmed(&r, kinds::TABLE, sc, "LINES", Relation::Code)?;
    check(r.items.len() == 2, format!("two tables: {r:#?}"))?;
    mentioned(&r, "LINES")?;
    check(r.scanned == 1, "the view's source read")?;

    // An inline SQL function on the view.
    let r = refs(s, scan, kinds::FUNCTION, sc, "F_COUNT").await?;
    confirmed(&r, kinds::VIEW, sc, "V_TOTALS", Relation::Code)?;
    mentioned(&r, "V_TOTALS")?;

    // A compiled SQL function: its package's table.
    let r = refs(s, scan, kinds::FUNCTION, sc, "F_SUM").await?;
    confirmed(&r, kinds::TABLE, sc, "ORDERS", Relation::Code)?;

    // A procedure: tables, both functions (by name, not specific name),
    // the sequence; no indexes or packages.
    let r = refs(s, scan, kinds::PROCEDURE, sc, "P_LOAD").await?;
    for (kind, name) in [
        (kinds::TABLE, "ORDERS"),
        (kinds::TABLE, "LINES"),
        (kinds::FUNCTION, "F_COUNT"),
        (kinds::FUNCTION, "F_SUM"),
        (kinds::SEQUENCE, "SEQ"),
    ] {
        confirmed(&r, kind, sc, name, Relation::Code)?;
    }
    mentioned(&r, "SEQ")?;
    // Nothing else: no specific names, packages, indexes or the procedure itself.
    check(r.items.len() == 5, format!("five objects: {r:#?}"))?;

    // A procedure calling another.
    let r = refs(s, scan, kinds::PROCEDURE, sc, "P_ALL").await?;
    confirmed(&r, kinds::PROCEDURE, sc, "P_LOAD", Relation::Code)?;

    // A trigger: its table and the one it writes.
    let r = refs(s, scan, kinds::TRIGGER, sc, "TRG").await?;
    confirmed(&r, kinds::TABLE, sc, "ORDERS", Relation::Code)?;
    confirmed(&r, kinds::TABLE, sc, "LINES", Relation::Code)?;

    // An alias: its table.
    let r = refs(s, scan, kinds::SYNONYM, sc, "A_ORDERS").await?;
    confirmed(&r, kinds::TABLE, sc, "ORDERS", Relation::Code)?;

    // A view on a nickname: the remote table, outside.
    if let Some((server, database)) = remote {
        let r = refs(s, scan, kinds::VIEW, sc, "V_REMOTE").await?;
        check(
            r.items.is_empty(),
            format!("the nickname isn't local: {r:#?}"),
        )?;
        let x = r
            .external
            .first()
            .ok_or_else(|| format!("no external: {r:#?}"))?;
        check(
            *x == ExternalRef {
                mentions: x.mentions.clone(),
                ..ExternalRef {
                    system: Some("DB2/UDB".into()),
                    server: Some(server.into()),
                    database: Some(database.to_uppercase()),
                    schema: Some(sc.into()),
                    name: Some("ORDERS".into()),
                    detail: Some(format!("{sc}.NK")),
                    mentions: Vec::new(),
                }
            },
            format!("external: {x:#?}"),
        )?;
        check(
            !x.mentions.is_empty(),
            format!("the nickname's line: {x:#?}"),
        )?;
    }
    Ok(())
}

#[tokio::test]
#[ignore]
async fn references_from_the_catalog() {
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
    let sc = format!("DBR{stamp}");
    let server = format!("DBR{stamp}_SRV");

    let create = [
        format!("CREATE SCHEMA {sc}"),
        format!("CREATE SEQUENCE {sc}.SEQ"),
        format!("CREATE TABLE {sc}.ORDERS (ID INT NOT NULL PRIMARY KEY, TOTAL INT)"),
        format!(
            "CREATE TABLE {sc}.LINES (ID INT NOT NULL PRIMARY KEY, ORDER_ID INT, QTY INT, \
             CONSTRAINT FK_LINES FOREIGN KEY (ORDER_ID) REFERENCES {sc}.ORDERS (ID))"
        ),
        format!("CREATE ALIAS {sc}.A_ORDERS FOR {sc}.ORDERS"),
        format!(
            "CREATE VIEW {sc}.V_TOTALS AS SELECT o.ID, o.TOTAL, l.QTY FROM {sc}.ORDERS o JOIN {sc}.LINES l ON l.ORDER_ID = o.ID"
        ),
        format!(
            "CREATE FUNCTION {sc}.F_COUNT (X INT) RETURNS INT LANGUAGE SQL\nRETURN (SELECT COUNT(*) FROM {sc}.V_TOTALS WHERE ID = X)"
        ),
        format!(
            "CREATE FUNCTION {sc}.F_SUM (X INT) RETURNS INT LANGUAGE SQL\nBEGIN\n  DECLARE N INT;\n  SET N = (SELECT SUM(TOTAL) FROM {sc}.ORDERS WHERE ID > X);\n  RETURN N;\nEND"
        ),
        format!(
            "CREATE PROCEDURE {sc}.P_LOAD (IN X INT) LANGUAGE SQL\nBEGIN\n  DECLARE N INT;\n  SET N = NEXT VALUE FOR {sc}.SEQ;\n  INSERT INTO {sc}.ORDERS (ID, TOTAL) VALUES (N, {sc}.F_COUNT(X) + {sc}.F_SUM(X));\n  UPDATE {sc}.LINES SET QTY = X WHERE ORDER_ID = N;\nEND"
        ),
        format!("CREATE PROCEDURE {sc}.P_ALL () LANGUAGE SQL\nBEGIN\n  CALL {sc}.P_LOAD(1);\nEND"),
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

    // Federation on the same database: a server, a nickname and a view on it.
    let user = c.username.clone().unwrap_or_default();
    let password = c.password.clone().unwrap_or_default();
    let mut made_wrapper = false;
    let mut remote = None;
    if result.is_ok() {
        let had = scalar(
            &mut s,
            "SELECT COUNT(*) FROM SYSCAT.WRAPPERS WHERE WRAPNAME = 'DRDA'",
        )
        .await;
        let mut fed = Ok(());
        if had.as_deref() == Ok("0") {
            fed = exec(&mut s, "CREATE WRAPPER DRDA").await.map(|_| ());
            made_wrapper = fed.is_ok();
        }
        for sql in [
            format!(
                "CREATE SERVER {server} TYPE DB2/UDB VERSION '12.1' WRAPPER DRDA \
                 AUTHORIZATION \"{user}\" PASSWORD \"{password}\" OPTIONS (DBNAME '{}')",
                c.database.to_uppercase()
            ),
            format!(
                "CREATE USER MAPPING FOR USER SERVER {server} \
                 OPTIONS (REMOTE_AUTHID '{user}', REMOTE_PASSWORD '{password}')"
            ),
            format!("CREATE NICKNAME {sc}.NK FOR {server}.{sc}.ORDERS"),
            format!("CREATE VIEW {sc}.V_REMOTE AS SELECT ID, TOTAL FROM {sc}.NK"),
        ] {
            if fed.is_err() {
                break;
            }
            fed = exec(&mut s, &sql).await.map(|_| ());
        }
        match fed {
            Ok(()) => remote = Some((server.as_str(), c.database.as_str())),
            Err(e) => eprintln!("federation not available, the nickname isn't checked: {e}"),
        }
    }
    if result.is_ok() {
        result = checks(&mut s, &scan, &sc, remote).await;
    }

    // Everything the test made, also after a failure.
    let mut drops = vec![
        format!("DROP VIEW {sc}.V_REMOTE"),
        format!("DROP NICKNAME {sc}.NK"),
        format!("DROP USER MAPPING FOR USER SERVER {server}"),
        format!("DROP SERVER {server}"),
    ];
    if made_wrapper {
        drops.push("DROP WRAPPER DRDA".into());
    }
    drops.extend([
        format!("DROP TRIGGER {sc}.TRG"),
        format!("DROP PROCEDURE {sc}.P_ALL"),
        format!("DROP PROCEDURE {sc}.P_LOAD"),
        format!("DROP FUNCTION {sc}.F_SUM"),
        format!("DROP FUNCTION {sc}.F_COUNT"),
        format!("DROP VIEW {sc}.V_TOTALS"),
        format!("DROP ALIAS {sc}.A_ORDERS"),
        format!("DROP TABLE {sc}.LINES"),
        format!("DROP TABLE {sc}.ORDERS"),
        format!("DROP SEQUENCE {sc}.SEQ"),
        format!("DROP SCHEMA {sc} RESTRICT"),
    ]);
    for d in &drops {
        if let Err(e) = exec(&mut s, d).await {
            eprintln!("cleanup: {e}");
        }
    }
    let left = scalar(
        &mut s,
        &format!(
            "SELECT (SELECT COUNT(*) FROM SYSCAT.SCHEMATA WHERE SCHEMANAME = '{sc}') \
                  + (SELECT COUNT(*) FROM SYSCAT.SERVERS WHERE SERVERNAME = '{server}') \
             FROM SYSIBM.SYSDUMMY1"
        ),
    )
    .await;
    assert_eq!(left.as_deref(), Ok("0"), "leftovers");
    result.unwrap();
}
