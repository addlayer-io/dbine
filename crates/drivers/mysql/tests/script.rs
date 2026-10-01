//! Editor scripts statement by statement, as the app runs them, against
//! real servers (see `integration.rs` for the containers):
//!
//! ```sh
//! DBINE_TEST_MYSQL_URL=mysql://root:pw@localhost:25011 \
//! DBINE_TEST_MARIADB_URL=mysql://root:pw@localhost:25012 \
//! DBINE_TEST_TIDB_URL=mysql://root@localhost:25014 \
//! DBINE_TEST_MANTICORE_URL=mysql://localhost:25016 \
//!   cargo test -p dbine-driver-mysql --test script -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, Driver, Error, MessageLevel, QueryOutcome, Session, StatementKind, TxState};
use std::sync::Arc;

fn parse_url(driver: &str, url: &str) -> ConnectionConfig {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostpart.rsplit_once(':').map_or((hostpart, 0), |(h, p)| (h, p.parse().unwrap()));
    ConnectionConfig {
        driver: driver.into(),
        host: host.into(),
        port,
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    }
}

fn driver(id: &str) -> Arc<dyn Driver> {
    dbine_driver_mysql::drivers().into_iter().find(|d| d.info().id == id).unwrap()
}

/// The app's loop: each unit on its own, stopping at the first error.
async fn script(d: &dyn Driver, s: &mut Box<dyn Session>, sql: &str) -> (QueryOutcome, Option<Error>) {
    let mut out = QueryOutcome::default();
    for (i, u) in d.split_script(sql).iter().enumerate() {
        if u.kind == StatementKind::ClientCommand {
            continue;
        }
        out.current_statement = Some(i);
        if let Err(e) = s.execute(&u.text, 100, &mut out).await {
            return (out, Some(e));
        }
    }
    (out, None)
}

async fn session(id: &str, env: &str, db: Option<&str>) -> Option<(Arc<dyn Driver>, Box<dyn Session>)> {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return None;
    };
    let d = driver(id);
    let s = d.connect(&parse_url(id, &url), db).await.expect("connect");
    Some((d, s))
}

async fn exercise(id: &str, env: &str, routines: bool) {
    let Some((d, mut admin)) = session(id, env, None).await else { return };
    let (_, e) = script(&*d, &mut admin, "DROP DATABASE IF EXISTS dbine_script; CREATE DATABASE dbine_script; DROP DATABASE IF EXISTS dbine_script2; CREATE DATABASE dbine_script2").await;
    assert!(e.is_none(), "{e:?}");
    let (_, mut s) = session(id, env, Some("dbine_script")).await.unwrap();

    // Info string, last insert id, a semicolon in a string and a comment.
    let (out, e) = script(
        &*d,
        &mut s,
        "CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, a VARCHAR(4), b INT);\n\
         INSERT INTO t (a, b) VALUES ('x;y', 1), ('it\\'s', 2); -- two rows; ok\n\
         UPDATE t SET b = b + 1 WHERE id > 0;",
    )
    .await;
    assert!(e.is_none(), "{e:?}");
    let infos: Vec<&str> = out.log.iter().filter(|m| m.level == MessageLevel::Info).map(|m| m.text.as_str()).collect();
    eprintln!("{id} infos: {infos:?}");
    assert!(infos.iter().any(|t| t.starts_with("Records: 2")), "{infos:?}");
    assert!(infos.iter().any(|t| t.starts_with("Rows matched: 2")), "{infos:?}");
    assert!(infos.contains(&"Último id generado: 1"), "{infos:?}");

    // Warnings with their text and code (MySQL / MariaDB; TiDB truncates too).
    let (out, e) = script(&*d, &mut s, "SET SESSION sql_mode = ''; INSERT INTO t (a, b) VALUES ('toolong', 3);").await;
    assert!(e.is_none(), "{e:?}");
    let warns: Vec<_> = out.log.iter().filter(|m| m.level == MessageLevel::Warning).collect();
    eprintln!("{id} warnings: {warns:?}");
    // TiDB reports 1406 (data too long) where MySQL / MariaDB say 1265.
    assert!(warns.iter().any(|m| matches!(m.code.as_deref(), Some("1265" | "1406")) && m.text.contains("'a'")), "{warns:?}");

    // Errors: number, SQLSTATE, line and offset inside the statement.
    let (_, e) = script(&*d, &mut s, "SELECT 1;\nSELECT a,\n  b FRM t;\nSELECT 2").await;
    match e {
        Some(Error::Statement(e)) => {
            eprintln!("{id} syntax: {e:?}");
            assert_eq!((e.code.as_deref(), e.sqlstate.as_deref(), e.line), (Some("1064"), Some("42000"), Some(2)));
            let text = "SELECT a,\n  b FRM t";
            let at = e.offset.expect("offset");
            assert!(text[at..].starts_with("FRM") || text[at..].starts_with("t"), "{at}: {}", &text[at..]);
        }
        other => panic!("{other:?}"),
    }
    match script(&*d, &mut s, "SELECT * FROM missing_table").await.1 {
        Some(Error::Statement(e)) => assert_eq!((e.code.as_deref(), e.sqlstate.as_deref()), (Some("1146"), Some("42S02"))),
        other => panic!("{other:?}"),
    }

    // DELIMITER: a routine body with `;` goes whole, then the terminator is back.
    if routines {
        let (out, e) = script(
            &*d,
            &mut s,
            "DELIMITER //\nCREATE PROCEDURE p(IN n INT)\nBEGIN\n  SELECT n AS v;\n  SELECT n + 1 AS w;\nEND//\nDELIMITER ;\nCALL p(5);\nSELECT 'after';",
        )
        .await;
        assert!(e.is_none(), "{e:?}");
        let firsts: Vec<_> = out.results.iter().filter(|r| !r.columns.is_empty()).map(|r| r.rows[0][0].clone()).collect();
        assert_eq!(firsts, vec![serde_json::json!(5), serde_json::json!(6), serde_json::json!("after")], "{:?}", out.results);
        // Same through `execute` with the whole script (Whole callers, run script file).
        let mut out = QueryOutcome::default();
        s.execute("DROP PROCEDURE p;\nDELIMITER $$\nCREATE PROCEDURE p() BEGIN SELECT 7; END$$\nDELIMITER ;\nCALL p()", 10, &mut out).await.unwrap();
        assert_eq!(out.results.iter().find(|r| !r.columns.is_empty()).unwrap().rows[0][0], serde_json::json!(7));
    }

    // USE: the tab follows the session's database.
    let mut out = QueryOutcome::default();
    s.execute("USE dbine_script2", 10, &mut out).await.unwrap();
    assert_eq!(out.database.as_deref(), Some("dbine_script2"));
    let mut out = QueryOutcome::default();
    s.execute("SELECT 1", 10, &mut out).await.unwrap();
    assert_eq!(out.database, None);
    s.execute("USE dbine_script", 10, &mut QueryOutcome::default()).await.unwrap();

    // Manual transactions.
    if d.supports_manual_transactions() {
        assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
        s.set_autocommit(false).await.unwrap();
        script(&*d, &mut s, "DELETE FROM t WHERE id = 1").await;
        assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Open));
        // An error keeps the transaction open (state read without an OK packet).
        assert!(script(&*d, &mut s, "SELECT * FROM missing_table").await.1.is_some());
        assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Open));
        s.rollback().await.unwrap();
        assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
        let (out, _) = script(&*d, &mut s, "SELECT COUNT(*) FROM t WHERE id = 1").await;
        assert_eq!(out.results[0].rows[0][0], serde_json::json!(1), "rolled back");
        script(&*d, &mut s, "DELETE FROM t WHERE id = 1").await;
        s.commit().await.unwrap();
        assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
        s.set_autocommit(true).await.unwrap();
        let (out, _) = script(&*d, &mut admin, "SELECT COUNT(*) FROM dbine_script.t WHERE id = 1").await;
        assert_eq!(out.results[0].rows[0][0], serde_json::json!(0), "committed");
        // Autocommit: a typed START TRANSACTION shows as open too.
        script(&*d, &mut s, "START TRANSACTION; SELECT 1").await;
        assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Open));
        script(&*d, &mut s, "ROLLBACK").await;
        assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    } else {
        assert_eq!(s.transaction_state().await.unwrap(), None);
    }

    drop(s);
    script(&*d, &mut admin, "DROP DATABASE dbine_script; DROP DATABASE dbine_script2").await;
}

#[tokio::test]
#[ignore]
async fn mysql_scripts() {
    exercise("mysql", "DBINE_TEST_MYSQL_URL", true).await;
}

#[tokio::test]
#[ignore]
async fn mariadb_scripts() {
    exercise("mariadb", "DBINE_TEST_MARIADB_URL", true).await;
}

#[tokio::test]
#[ignore]
async fn tidb_scripts() {
    exercise("tidb", "DBINE_TEST_TIDB_URL", false).await;
}

#[tokio::test]
#[ignore]
async fn manticore_scripts() {
    let Some((d, mut s)) = session("manticore", "DBINE_TEST_MANTICORE_URL", None).await else { return };
    let (_, e) = script(
        &*d,
        &mut s,
        "DROP TABLE IF EXISTS dbine_script_t; /* a; comment */ CREATE TABLE dbine_script_t (title text, n int);\n\
         -- one; two\nINSERT INTO dbine_script_t (id, title, n) VALUES (1, 'it\\'s; ok', 1); # done; really\n",
    )
    .await;
    assert!(e.is_none(), "{e:?}");
    // The same text in one call (Whole callers) splits the same way.
    let mut out = QueryOutcome::default();
    s.execute("SELECT title FROM dbine_script_t; /* x; */ SELECT COUNT(*) FROM dbine_script_t -- y;", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], serde_json::json!("it's; ok"));
    assert_eq!(out.results.len(), 2);
    match script(&*d, &mut s, "SELECT * FROM no_such_table").await.1 {
        Some(Error::Statement(e)) => eprintln!("manticore error: {e:?}"),
        other => eprintln!("manticore error (plain): {other:?}"),
    }
    script(&*d, &mut s, "DROP TABLE dbine_script_t").await;
}
