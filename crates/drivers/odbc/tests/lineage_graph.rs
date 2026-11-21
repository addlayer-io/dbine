//! The lineage of a whole schema (`Session::lineage_graph`) on Db2 LUW, against a real
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
//!   cargo test -p dbine-driver-odbc --test lineage_graph -- --ignored --nocapture
//! ```

use dbine_driver::dependencies::LineageScope;
use dbine_driver::{
    Confidence, ConnectionConfig, DependencyScan, Driver, QueryOutcome, Relation, Session,
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

async fn checks(
    s: &mut Box<dyn Session>,
    scan: &DependencyScan,
    sc: &str,
    remote: Option<(&str, &str)>,
) -> Result<(), String> {
    let scope = LineageScope {
        schemas: vec![sc.to_string()],
        ..Default::default()
    };
    let g = s
        .lineage_graph("", &scope, scan)
        .await
        .map_err(|e| format!("lineage_graph: {e}"))?;
    check(
        g.notes.is_empty(),
        format!("answered from the catalog: {:?}", g.notes),
    )?;
    let edge = |from: &str, to: &str, rel: Relation| {
        g.edges.iter().any(|e| {
            e.from.name == from
                && e.to.name == to
                && e.from.schema.as_deref() == Some(sc)
                && e.to.schema.as_deref() == Some(sc)
                && e.relation == rel
                && e.confidence == Confidence::Confirmed
        })
    };
    // The driver's object list has no functions or triggers (SQLProcedures
    // and SQLTables don't return them): they are not nodes, so no edge goes through them.
    for (from, to, rel) in [
        ("ORDERS", "LINES", Relation::ForeignKey),
        ("ORDERS", "V_TOTALS", Relation::Code),
        ("LINES", "V_TOTALS", Relation::Code),
        ("ORDERS", "P_LOAD", Relation::Code),
        ("LINES", "P_LOAD", Relation::Code),
        ("SEQ", "P_LOAD", Relation::Code),
        ("P_LOAD", "P_ALL", Relation::Code),
        ("ORDERS", "A_ORDERS", Relation::Code),
    ] {
        check(
            edge(from, to, rel),
            format!("edge {from} -> {to} {rel:?}: {:#?}", g.edges),
        )?;
    }
    check(
        g.objects.iter().all(|o| o.schema.as_deref() == Some(sc)),
        "only the schema in scope",
    )?;
    if let Some((server, database)) = remote {
        let x = g
            .external
            .iter()
            .find(|x| x.to.name == "V_REMOTE")
            .ok_or_else(|| format!("no external: {:#?}", g.external))?;
        check(
            x.source.server.as_deref() == Some(server)
                && x.source.database.as_deref() == Some(database.to_uppercase().as_str())
                && x.source.name.as_deref() == Some("ORDERS")
                && x.source.detail.as_deref() == Some(format!("{sc}.NK").as_str()),
            format!("external: {x:#?}"),
        )?;
    }
    // The scope's kinds and limit.
    let views = s
        .lineage_graph(
            "",
            &LineageScope {
                schemas: vec![sc.to_string()],
                kinds: vec!["view".into()],
                ..Default::default()
            },
            scan,
        )
        .await
        .map_err(|e| e.to_string())?;
    check(
        views.objects.iter().all(|o| o.kind == "view") && views.edges.is_empty(),
        format!("views only: {views:#?}"),
    )?;
    let capped = s
        .lineage_graph(
            "",
            &LineageScope {
                schemas: vec![sc.to_string()],
                limit: 3,
                ..Default::default()
            },
            scan,
        )
        .await
        .map_err(|e| e.to_string())?;
    check(
        capped.truncated && capped.objects.len() == 3,
        "the limit truncates",
    )?;
    Ok(())
}

#[tokio::test]
#[ignore]
async fn lineage_graph_from_the_catalog() {
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
