//! Protected reads (`Session::run_read_only`) on MySQL and MariaDB.
//!
//! The statement goes as a server-side prepared statement (COM_STMT_PREPARE
//! takes exactly one statement: `SELECT 1; DELETE …` is a syntax error)
//! inside `START TRANSACTION READ ONLY`, with the session's transaction
//! access mode also set to READ ONLY for the call, and is always rolled
//! back. Verified on MySQL 8.4 and MariaDB 11.8:
//!
//! - INSERT / UPDATE / DELETE, also from a stored function and on MyISAM
//!   or temporary tables, fail with ER_CANT_EXECUTE_IN_READ_ONLY_TRANSACTION.
//! - DDL commits the open transaction implicitly; inside a READ ONLY
//!   *transaction* alone `CREATE` / `DROP` / `TRUNCATE` still run. With the
//!   *session* access mode READ ONLY they are refused too, and so is a write
//!   after a `COMMIT`: the implicit or explicit commit only opens another
//!   read-only transaction.
//! - `SELECT … FOR UPDATE` is refused by both servers in a read-only
//!   transaction.
//! - What the server lets through is refused here, before anything is
//!   sent ([`refusal`]): `SELECT … INTO OUTFILE / DUMPFILE` (both servers
//!   write the file inside a read-only transaction, wherever
//!   `secure_file_priv` allows, and a file isn't rolled back), user locks
//!   (`GET_LOCK`…, held by the session, not the transaction), `COMMIT`,
//!   `SET` (they run; the next call is protected again, but nothing is
//!   left to chance) and any statement that isn't a read by its first word
//!   (`ANALYZE TABLE`, `CALL`, `DO`, `HANDLER`, `LOAD`, `PREPARE`, `FLUSH`,
//!   `KILL`…). Locking reads are refused here too. The generic read-only
//!   guard runs as well.
//! - The session's access mode is set back after every call, and a call
//!   dropped half-way (a timeout) is settled by the next one, or by the
//!   next `execute`. A transaction the user left open is never committed:
//!   the read is refused instead.
//!
//! Left to the server, not refusable from the text: a stored function
//! called from a SELECT can still set session variables or take a user
//! lock (`GET_LOCK` inside its body); it can't write data, commit or run
//! DDL.

use crate::Variant;
use dbine_driver::sql::{expose_versioned, name_tokens, TokenKind};

/// First words of the statements a protected read runs.
const READS: &[&str] = &["select", "with", "show", "explain", "describe", "desc", "values", "table"];

/// Words that write outside the transaction (files) wherever they appear.
const FILE_WORDS: &[&str] = &["outfile", "dumpfile"];

/// User-level locks: held by the session, not the transaction.
const LOCK_FUNCTIONS: &[&str] = &["get_lock", "release_lock", "release_all_locks"];

/// Whether `variant` at server version `version` (`SELECT VERSION()`, and
/// the handshake's numbers) runs protected reads: MySQL 5.6.5+ (START
/// TRANSACTION READ ONLY) and MariaDB 10.0+, not the engines that speak
/// the protocol without those semantics (their version text gives them
/// away when the connection was opened as plain MySQL).
pub(crate) fn supported(variant: Variant, numbers: (u16, u16, u16), version: &str) -> bool {
    let v = version.to_ascii_lowercase();
    let maria = v.contains("mariadb");
    let other = ["tidb", "oceanbase", "singlestore", "memsql", "starrocks", "doris", "databend", "manticore", "greptime", "vitess"]
        .iter()
        .any(|o| v.contains(o));
    match variant {
        _ if other => false,
        Variant::MySql if maria => numbers >= (10, 0, 0),
        Variant::MySql => numbers >= (5, 6, 5),
        Variant::MariaDb => numbers >= (10, 0, 0),
        _ => false,
    }
}

/// Why `stmt` can't run as a protected read (the word that stops it), if
/// anything stops it.
pub(crate) fn refusal(stmt: &str, variant: Variant) -> Option<String> {
    let dialect = crate::script_dialect(variant);
    if let Some(word) = dbine_driver::read_only::first_write_in(stmt, &dialect) {
        return Some(word);
    }
    // Versioned comments (`/*!50000 … */`) run as code: read them as such.
    let exposed = expose_versioned(stmt, &dialect);
    let toks = name_tokens(&exposed, &dialect);
    let bytes = exposed.as_bytes();
    let bare = |i: usize| toks[i].kind == TokenKind::Name && !matches!(bytes[toks[i].start], b'"' | b'`');
    let lower = |i: usize| toks[i].text.to_ascii_lowercase();
    // The first word, past opening parentheses (`(SELECT …) UNION …`).
    let first = toks.iter().position(|t| !(t.kind == TokenKind::Punct && t.text == "("))?;
    if !bare(first) || !READS.contains(&lower(first).as_str()) {
        return Some(toks[first].text.to_uppercase());
    }
    let next_is = |i: usize, words: &[&str]| toks.get(i + 1).is_some_and(|t| t.kind == TokenKind::Name && words.contains(&t.text.to_ascii_lowercase().as_str()));
    for i in 0..toks.len() {
        if !bare(i) {
            continue;
        }
        let w = lower(i);
        if FILE_WORDS.contains(&w.as_str()) {
            return Some(format!("INTO {}", w.to_uppercase()));
        }
        if w == "for" && next_is(i, &["update", "share"]) {
            return Some(format!("FOR {}", toks[i + 1].text.to_uppercase()));
        }
        if w == "lock" && next_is(i, &["in"]) {
            return Some("LOCK IN SHARE MODE".into());
        }
        if LOCK_FUNCTIONS.contains(&w.as_str()) && toks.get(i + 1).is_some_and(|t| t.text == "(") {
            return Some(w.to_uppercase());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(sql: &str) -> Option<String> {
        refusal(sql, Variant::MySql)
    }

    #[test]
    fn reads_pass() {
        for sql in [
            "SELECT 1",
            "select * from t where a = 'for update'",
            "/* note */ SELECT `for`, `update` FROM t",
            "(SELECT 1) UNION (SELECT 2)",
            "WITH c AS (SELECT 1 AS a) SELECT a FROM c",
            "SHOW CREATE TABLE t",
            "SHOW TABLES",
            "EXPLAIN SELECT * FROM t",
            "DESCRIBE t",
            "VALUES ROW(1, 2)",
            "TABLE t",
            "SELECT a INTO @x FROM t",
            "SELECT replace(a, 'x', 'y') FROM t",
        ] {
            assert_eq!(refused(sql), None, "{sql}");
        }
    }

    #[test]
    fn non_reads_are_refused_by_their_first_word() {
        for sql in [
            "ANALYZE TABLE t",
            "DO 1",
            "HANDLER t OPEN",
            "FLUSH TABLES",
            "CHECKSUM TABLE t",
            "XA START 'x'",
            "BEGIN",
            "START TRANSACTION",
            "UNLOCK TABLES",
            "DEALLOCATE PREPARE p",
            "CACHE INDEX t IN k",
            "`select` 1",
        ] {
            assert!(refused(sql).is_some(), "{sql}");
        }
    }

    #[test]
    fn transaction_control_and_session_changes_are_refused() {
        for sql in [
            "COMMIT",
            "ROLLBACK",
            "SET SESSION TRANSACTION READ WRITE",
            "SET TRANSACTION READ WRITE",
            "SET autocommit = 1",
            "LOCK TABLES t WRITE",
            "LOAD DATA INFILE '/tmp/x' INTO TABLE t",
            "CALL p()",
            "PREPARE s FROM 'DELETE FROM t'",
            "EXECUTE s",
            "/*!50000 SET SESSION TRANSACTION READ WRITE */",
            "/*!COMMIT*/",
        ] {
            assert!(refused(sql).is_some(), "{sql}");
        }
    }

    #[test]
    fn ddl_and_writes_are_refused() {
        for sql in [
            "CREATE TABLE x (a INT)",
            "DROP TABLE t",
            "TRUNCATE TABLE t",
            "ALTER TABLE t ADD b INT",
            "RENAME TABLE t TO u",
            "GRANT SELECT ON *.* TO u",
            "DELETE FROM t",
            "UPDATE t SET a = 1",
            "INSERT INTO t VALUES (1)",
            "REPLACE INTO t VALUES (1)",
            "WITH c AS (SELECT 1) DELETE FROM t",
            // A versioned comment hides the real first word from a reader
            // that skips comments: the server runs CREATE TABLE.
            "/*!CREATE*/ TABLE x (a INT)",
            "/*M!100000 DROP */ TABLE t",
            "SELECT 1; DELETE FROM t",
        ] {
            assert!(refused(sql).is_some(), "{sql}");
        }
    }

    #[test]
    fn files_and_locks_the_transaction_does_not_cover_are_refused() {
        assert_eq!(refused("SELECT * FROM t INTO OUTFILE '/tmp/x'").as_deref().map(|s| s.contains("INTO")), Some(true));
        assert!(refused("SELECT a FROM t INTO DUMPFILE '/tmp/x'").is_some());
        assert!(refused("SELECT a FROM t /*!INTO*/ /*!OUTFILE '/tmp/x' */").is_some());
        assert!(refused("TABLE t INTO OUTFILE '/tmp/x'").is_some());
        assert!(refused("SELECT * FROM t FOR UPDATE").is_some());
        assert!(refused("SELECT * FROM t FOR SHARE").is_some());
        assert!(refused("SELECT * FROM t LOCK IN SHARE MODE").is_some());
        assert!(refused("SELECT GET_LOCK('x', 1)").is_some());
        assert!(refused("SELECT RELEASE_ALL_LOCKS()").is_some());
        // Lone CR and ambiguous spaces: where a comment ends is unclear.
        assert!(refused("SELECT 1 -- x\rDELETE FROM t").is_some());
    }

    #[test]
    fn engines_and_versions() {
        assert!(supported(Variant::MySql, (8, 4, 11), "8.4.11"));
        assert!(supported(Variant::MySql, (5, 6, 5), "5.6.5-log"));
        assert!(!supported(Variant::MySql, (5, 6, 4), "5.6.4"));
        assert!(supported(Variant::MariaDb, (11, 8, 9), "11.8.9-MariaDB-ubu2404"));
        assert!(supported(Variant::MySql, (10, 6, 0), "10.6.0-MariaDB"));
        assert!(!supported(Variant::MariaDb, (5, 5, 68), "5.5.68-MariaDB"));
        assert!(!supported(Variant::MySql, (8, 0, 11), "8.0.11-TiDB-v8.5.3"));
        assert!(!supported(Variant::MySql, (5, 7, 25), "5.7.25-OceanBase-v4.2.1"));
        for v in [Variant::TiDb, Variant::OceanBase, Variant::SingleStore, Variant::StarRocks, Variant::Doris, Variant::Databend, Variant::Manticore, Variant::GreptimeDb] {
            assert!(!supported(v, (8, 0, 0), "8.0.0"), "{v:?}");
        }
    }

    /// What the server alone stops (no text checks), against
    /// `DBINE_TEST_MYSQL_URL` / `DBINE_TEST_MARIADB_URL`: why DDL, COMMIT
    /// and file writes are refused before they're sent too.
    #[tokio::test]
    #[ignore]
    async fn server_side_protection() {
        use crate::session::MySqlSession;
        use dbine_driver::{ConnectionConfig, QueryOutcome};
        for (id, env) in [("mysql", "DBINE_TEST_MYSQL_URL"), ("mariadb", "DBINE_TEST_MARIADB_URL")] {
            let Ok(url) = std::env::var(env) else {
                eprintln!("{env} not set; skipping");
                continue;
            };
            let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
            let (auth, hostport) = rest.rsplit_once('@').unwrap();
            let (user, pass) = auth.split_once(':').unwrap();
            let (host, port) = hostport.rsplit_once(':').unwrap();
            let cfg = ConnectionConfig {
                driver: id.into(),
                host: host.into(),
                port: port.parse().unwrap(),
                username: Some(user.into()),
                password: Some(pass.into()),
                ..Default::default()
            };
            let d = crate::drivers().into_iter().find(|d| d.info().id == id).unwrap();
            let mut admin = d.connect(&cfg, None).await.unwrap();
            let mut out = QueryOutcome::default();
            admin
                .execute("DROP DATABASE IF EXISTS dbine_ro_srv; CREATE DATABASE dbine_ro_srv; CREATE TABLE dbine_ro_srv.t (id INT) ENGINE=InnoDB; CREATE TABLE dbine_ro_srv.m (id INT) ENGINE=MyISAM; INSERT INTO dbine_ro_srv.t VALUES (1); INSERT INTO dbine_ro_srv.m VALUES (1)", 10, &mut out)
                .await
                .unwrap();
            let mut boxed = d.connect(&cfg, Some("dbine_ro_srv")).await.unwrap();
            let s = boxed.as_any().unwrap().downcast_mut::<MySqlSession>().unwrap();
            let mut read = async |sql: &str| {
                let mut out = QueryOutcome::default();
                s.server_read_only(sql, 10, &mut out).await.map(|_| out)
            };
            for sql in ["DELETE FROM t", "UPDATE m SET id = 2", "CREATE TABLE x (a INT)", "DROP TABLE t", "TRUNCATE TABLE m", "RENAME TABLE t TO u", "SELECT 1; SELECT 2"] {
                let e = read(sql).await.expect_err(sql);
                eprintln!("{id}: server: {sql} -> {e}");
            }
            // COMMIT and SET end or change the transaction; they run (one
            // statement: nothing follows them in the call) and the next
            // call is protected again.
            for sql in ["COMMIT", "SET SESSION TRANSACTION READ WRITE", "SET autocommit = 1"] {
                eprintln!("{id}: server: {sql} -> {:?}", read(sql).await.map(|_| "ran").map_err(|e| e.to_string()));
                assert!(read("DELETE FROM t").await.is_err(), "{id}: after {sql}");
            }
            // Locking reads: MySQL refuses FOR UPDATE in a read-only
            // transaction, MariaDB takes the locks until the rollback.
            let locking = read("SELECT * FROM t FOR UPDATE").await;
            eprintln!("{id}: server: FOR UPDATE -> {:?}", locking.map(|_| "ran").map_err(|e| e.to_string()));
            // User locks outlive the transaction (released here).
            assert_eq!(read("SELECT GET_LOCK('dbine_ro_srv', 0)").await.unwrap().results[0].rows[0][0], serde_json::json!(1), "{id}");
            read("SELECT RELEASE_LOCK('dbine_ro_srv')").await.unwrap();
            let file = format!("dbine_ro_{}.txt", std::process::id());
            let outfile = read(&format!("SELECT * FROM t INTO OUTFILE '/var/lib/mysql-files/{file}'")).await;
            eprintln!("{id}: server: INTO OUTFILE -> {:?}", outfile.as_ref().map(|_| "written").map_err(|e| e.to_string()));
            let mut out = QueryOutcome::default();
            admin.execute("SELECT (SELECT COUNT(*) FROM dbine_ro_srv.t), (SELECT COUNT(*) FROM dbine_ro_srv.m), (SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = 'dbine_ro_srv')", 10, &mut out).await.unwrap();
            assert_eq!(out.results[0].rows[0], vec![serde_json::json!(1), serde_json::json!(1), serde_json::json!(2)], "{id}");
            assert_eq!(boxed.transaction_state().await.unwrap(), Some(dbine_driver::TxState::Idle), "{id}");
            let mut out = QueryOutcome::default();
            boxed.execute("SELECT @@session.transaction_read_only", 10, &mut out).await.unwrap();
            assert_eq!(out.results[0].rows[0][0], serde_json::json!(0), "{id}");
            let mut out = QueryOutcome::default();
            admin.execute("DROP DATABASE dbine_ro_srv", 10, &mut out).await.unwrap();
        }
    }
}
