//! Editor scripts against real servers, as in tests/integration.rs:
//! cypher-shell's client commands (`:use` on its own line, `:param`,
//! `:begin` / `:commit` / `:rollback`), manual transactions, and errors with
//! their code and place.
//!
//! `DBINE_TEST_NEO4J_URL=neo4j:dbine-test-pass@localhost:17687 DBINE_TEST_MEMGRAPH_URL=localhost:27687 \
//!   cargo test -p dbine-driver-neo4j --test script -- --ignored --test-threads=1`

use dbine_driver::{ConnectionConfig, MessageLevel, QueryOutcome, Session, TxState};
use serde_json::json;

fn cfg(driver: &str, url: &str) -> ConnectionConfig {
    let (auth, hp) = url.rsplit_once('@').map_or((None, url), |(a, h)| (Some(a), h));
    let (host, port) = hp.rsplit_once(':').unwrap();
    let (user, pass) = auth.and_then(|a| a.split_once(':')).map_or((None, None), |(u, p)| (Some(u.to_string()), Some(p.to_string())));
    ConnectionConfig { driver: driver.into(), host: host.into(), port: port.parse().unwrap(), username: user, password: pass, ..Default::default() }
}

async fn open(id: &str, url: &str) -> Box<dyn Session> {
    let d = dbine_driver_neo4j::drivers().into_iter().find(|d| d.info().id == id).unwrap();
    assert!(d.supports_manual_transactions());
    d.connect(&cfg(id, url), None).await.unwrap()
}

async fn run(s: &mut Box<dyn Session>, text: &str) -> dbine_driver::Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(text, 100, &mut out).await.map(|_| out)
}

async fn count(s: &mut Box<dyn Session>) -> serde_json::Value {
    run(s, "MATCH (n:DbineScriptTx) RETURN count(n) AS c").await.unwrap().results[0].rows[0][0].clone()
}

async fn check(id: &str, url: &str, multi_db: bool) {
    let mut s = open(id, url).await;
    let mut other = open(id, url).await;
    run(&mut s, "MATCH (n:DbineScriptTx) DETACH DELETE n").await.unwrap();

    // `:use` on its own line doesn't take the next Cypher lines.
    if multi_db {
        let out = run(&mut s, ":use neo4j\nMATCH (n:DbineScriptTx)\nRETURN count(n) AS c").await.unwrap();
        assert_eq!(out.results[0].rows[0][0], json!(0));
        assert_eq!((out.results[0].statement, out.results[0].line), (Some(1), Some(2)));
        assert!(out.log.iter().any(|m| m.text == "Base de datos actual: neo4j"), "{:?}", out.log);
    }

    // Errors carry the server's code (Neo4j) and the script line.
    let mut out = QueryOutcome::default();
    let e = s.execute("RETURN 1 AS a;\n\nRETRN 2", 10, &mut out).await.unwrap_err().to_script_error();
    assert_eq!(out.results.len(), 1, "stops at the error, as cypher-shell");
    assert_eq!(e.line, Some(3), "{e:?}");
    if id == "neo4j" {
        assert_eq!(e.code.as_deref(), Some("Neo.ClientError.Statement.SyntaxError"), "{e:?}");
    }

    // :param, evaluated by the server and sent with the next statements.
    let out = run(&mut s, ":param x => 21 * 2\n:param {nombre: 'Ana', n: $x + 1}\nRETURN $x AS x, $nombre AS nombre, $n AS n").await.unwrap();
    assert_eq!(out.results.last().unwrap().rows[0], vec![json!(42), json!("Ana"), json!(43)]);
    assert!(out.log.iter().any(|m| m.text == "$x = 42"), "{:?}", out.log);
    let out = run(&mut s, ":params").await.unwrap();
    assert_eq!(out.results[0].rows.len(), 3);
    let out = run(&mut s, "RETURN $nombre AS n").await.unwrap();
    assert_eq!(out.results[0].rows[0][0], json!("Ana"), "parameters last across runs");
    run(&mut s, ":params clear").await.unwrap();
    assert!(run(&mut s, "RETURN $nombre AS n").await.is_err());

    // :begin … :commit across runs; others don't see it before.
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    run(&mut s, ":begin\nCREATE (:DbineScriptTx {k: 1})").await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Open));
    assert_eq!(count(&mut s).await, json!(1), "the transaction sees its own write");
    assert_eq!(count(&mut other).await, json!(0));
    if multi_db {
        assert!(run(&mut s, ":use system").await.is_err(), "no database switch inside a transaction");
    }
    assert!(run(&mut s, ":begin").await.is_err());
    run(&mut s, ":commit").await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    assert_eq!(count(&mut other).await, json!(1));
    assert!(run(&mut s, ":commit").await.is_err(), "nothing open");

    // :rollback.
    run(&mut s, ":begin\nCREATE (:DbineScriptTx {k: 2});\n:rollback").await.unwrap();
    assert_eq!(count(&mut other).await, json!(1));

    // A failed statement inside the transaction rolls it back.
    let mut out = QueryOutcome::default();
    let r = s.execute(":begin\nCREATE (:DbineScriptTx {k: 3});\nRETURN 1/0", 10, &mut out).await;
    assert!(r.is_err());
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    assert!(out.log.iter().any(|m| m.level == MessageLevel::Warning && m.text.starts_with("La transacción se deshizo")), "{:?}", out.log);
    assert_eq!(count(&mut other).await, json!(1));

    // Manual mode: the first statement opens a transaction; the buttons end it.
    s.set_autocommit(false).await.unwrap();
    let out = run(&mut s, "CREATE (:DbineScriptTx {k: 4})").await.unwrap();
    assert!(out.log.iter().any(|m| m.text == "Transacción iniciada."));
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Open));
    s.rollback().await.unwrap();
    assert_eq!(count(&mut other).await, json!(1));
    run(&mut s, "CREATE (:DbineScriptTx {k: 5})").await.unwrap();
    s.commit().await.unwrap();
    assert_eq!(count(&mut other).await, json!(2));
    s.set_autocommit(true).await.unwrap();

    // Other cypher-shell commands are refused, placed on their line.
    let e = run(&mut s, "RETURN 1;\n:source x.cypher").await.unwrap_err().to_script_error();
    assert_eq!(e.line, Some(2));
    assert!(e.message.contains(":source"), "{e:?}");
    run(&mut s, "MATCH (n:DbineScriptTx) DETACH DELETE n").await.unwrap();
}

#[tokio::test]
#[ignore]
async fn neo4j_scripts() {
    let Ok(url) = std::env::var("DBINE_TEST_NEO4J_URL") else { return };
    check("neo4j", &url, true).await;
}

#[tokio::test]
#[ignore]
async fn memgraph_scripts() {
    let Ok(url) = std::env::var("DBINE_TEST_MEMGRAPH_URL") else { return };
    check("memgraph", &url, false).await;
}
