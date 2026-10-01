//! Editor scripts against a real server: SQL*Plus commands, DBMS_OUTPUT,
//! errors with their place, compile errors, tags and manual transactions.
//!
//! ```sh
//! DBINE_TEST_ORACLE_URL=oracle://dbine:Dbine123@localhost:25601/FREEPDB1 \
//!   cargo test -p dbine-driver-oracle --test script -- --ignored
//! ```
//!
//! The app runs an editor script statement by statement; these tests send
//! the units its oracle lexer cuts (one `execute` each), plus whole scripts
//! as the other screens send them.

use dbine_driver::{ConnectionConfig, Error, Message, MessageLevel, MessageSinkRef, QueryOutcome, Session, TxState};
use serde_json::json;
use std::sync::{Arc, Mutex};

fn config() -> ConnectionConfig {
    let url = std::env::var("DBINE_TEST_ORACLE_URL").expect("DBINE_TEST_ORACLE_URL");
    let rest = url.strip_prefix("oracle://").expect("oracle://user:pass@host:port/service");
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

async fn session() -> Box<dyn Session> {
    let driver = dbine_driver_oracle::drivers().remove(0);
    driver.connect(&config(), None).await.expect("connect")
}

async fn run(s: &mut Box<dyn Session>, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    if let Err(e) = s.execute(sql, 1000, &mut out).await {
        panic!("{sql}: {e}");
    }
    out
}

async fn fail(s: &mut Box<dyn Session>, sql: &str) -> dbine_driver::ScriptError {
    let mut out = QueryOutcome::default();
    match s.execute(sql, 1000, &mut out).await {
        Err(Error::Statement(e)) => *e,
        other => panic!("{sql}: {other:?}"),
    }
}

async fn quiet(s: &mut Box<dyn Session>, sql: &str) {
    let _ = s.execute(sql, 10, &mut QueryOutcome::default()).await;
}

fn texts(out: &QueryOutcome, level: MessageLevel) -> Vec<String> {
    out.log.iter().filter(|m| m.level == level).map(|m| m.text.clone()).collect()
}

#[tokio::test]
#[ignore]
async fn errors_carry_code_and_place() {
    let mut s = session().await;
    // Parse offset (characters) → byte offset in the text.
    let sql = "select 'é', nope from dual";
    let e = fail(&mut s, sql).await;
    assert_eq!(e.code.as_deref(), Some("ORA-00904"), "{e:?}");
    assert!(e.message.starts_with("ORA-00904"), "{e:?}");
    assert_eq!(e.offset, Some(sql.find("nope").unwrap()), "{e:?}");
    assert_eq!(e.line, Some(1));
    // A missing table: ORA-00942 at the name.
    let sql = "select *\n  from dbine_no_such_table";
    let e = fail(&mut s, sql).await;
    assert_eq!(e.code.as_deref(), Some("ORA-00942"));
    assert_eq!((e.offset, e.line), (Some(sql.find("dbine_no").unwrap()), Some(2)), "{e:?}");
    // PL/SQL compile error in an anonymous block: ORA-06550 line/column.
    let sql = "begin\n  dbms_output.put_line('x');\n  no_such_proc;\nend;";
    let e = fail(&mut s, sql).await;
    assert_eq!(e.code.as_deref(), Some("ORA-06550"));
    assert_eq!((e.offset, e.line), (Some(sql.find("no_such_proc").unwrap()), Some(3)), "{e:?}");
    // Runtime error raised by the block: ORA-06512 at line N.
    let sql = "declare\n  n number;\nbegin\n  n := 1 / 0;\nend;";
    let e = fail(&mut s, sql).await;
    assert_eq!(e.code.as_deref(), Some("ORA-01476"));
    assert_eq!(e.line, Some(4), "{e:?}");
    // In a whole script, relative to the whole text.
    let sql = "select 1 from dual;\nselect 2 from dual;\nselect nope from dual;";
    let e = fail(&mut s, sql).await;
    assert_eq!((e.offset, e.line), (Some(sql.find("nope").unwrap()), Some(3)), "{e:?}");
    // The session goes on.
    assert_eq!(run(&mut s, "select 1 from dual").await.results[0].rows[0][0], json!(1));
}

#[tokio::test]
#[ignore]
async fn dbms_output_and_sqlplus_commands() {
    let mut s = session().await;
    // Live: through the message sink while the script runs.
    let seen = Arc::new(Mutex::new(Vec::<Message>::new()));
    let sink = seen.clone();
    let mut out = QueryOutcome {
        message_sink: Some(MessageSinkRef(Arc::new(move |m: &Message| sink.lock().unwrap().push(m.clone())))),
        ..Default::default()
    };
    let units = [
        "PROMPT Inicio del script",
        "begin\n  dbms_output.put_line('uno');\n  dbms_output.put_line('dos');\nend;",
        "select 1 from dual",
        "exec dbms_output.put_line('tres')",
    ];
    for u in units {
        s.execute(u, 100, &mut out).await.unwrap_or_else(|e| panic!("{u}: {e}"));
    }
    assert_eq!(texts(&out, MessageLevel::Info), vec!["Inicio del script", "uno", "dos", "tres"]);
    assert_eq!(seen.lock().unwrap().iter().map(|m| m.text.as_str()).collect::<Vec<_>>(), vec![
        "Inicio del script",
        "uno",
        "dos",
        "tres"
    ]);
    // Tags: the block and the query.
    let tags: Vec<_> = out.results.iter().map(|r| r.tag.clone().unwrap_or_default()).collect();
    assert_eq!(tags, vec!["PL/SQL", "SELECT", "PL/SQL"]);

    // SET SERVEROUTPUT OFF: nothing; ON again: shown. Abbreviated, with `;`.
    let out = run(&mut s, "SET SERVEROUT OFF;").await;
    assert!(out.log.is_empty());
    let out = run(&mut s, "begin dbms_output.put_line('oculto'); end;").await;
    assert!(texts(&out, MessageLevel::Info).is_empty(), "{:?}", out.log);
    let out = run(&mut s, "SET SERVEROUTPUT ON SIZE UNLIMITED").await;
    assert!(out.log.is_empty());
    let out = run(&mut s, "begin dbms_output.put_line('visible'); end;").await;
    assert_eq!(texts(&out, MessageLevel::Info), vec!["visible"]);

    // What a block printed before failing still shows, then the error.
    let mut out = QueryOutcome::default();
    let e = s
        .execute("begin dbms_output.put_line('antes'); raise_application_error(-20001, 'falló'); end;", 10, &mut out)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("ORA-20001"), "{e}");
    assert_eq!(texts(&out, MessageLevel::Info), vec!["antes"]);

    // Display settings are skipped quietly; others are reported.
    let out = run(&mut s, "SET LINESIZE 200\nSET DEFINE OFF\nREM nada\nselect 'x' from dual;").await;
    assert!(out.log.is_empty(), "{:?}", out.log);
    assert_eq!(out.results.len(), 1);
    let out = run(&mut s, "SPOOL salida.log").await;
    assert_eq!(texts(&out, MessageLevel::Warning).len(), 1, "{:?}", out.log);
    let out = run(&mut s, "WHENEVER SQLERROR EXIT FAILURE").await;
    assert!(texts(&out, MessageLevel::Warning)[0].contains("Seguir si hay un error"));

    // A whole script as other screens send it: PROMPT, a block after a SET,
    // the `/` lines.
    let out = run(
        &mut s,
        "SET SERVEROUTPUT ON\nPROMPT paso 1\nBEGIN\n  dbms_output.put_line('a;b');\nEND;\n/\nselect 2 from dual\n/\n",
    )
    .await;
    assert_eq!(texts(&out, MessageLevel::Info), vec!["paso 1", "a;b"]);
    assert_eq!(out.results.len(), 2);
}

#[tokio::test]
#[ignore]
async fn compile_errors_and_show_errors() {
    let mut s = session().await;
    quiet(&mut s, "DROP TABLE dbine_scr_t PURGE").await;
    run(&mut s, "CREATE TABLE dbine_scr_t (a NUMBER)").await;

    // A procedure with an error on its 3rd line (the CREATE on two lines).
    let sql = "create or replace\nprocedure dbine_scr_p as\nbegin\n  no_such_thing;\nend;";
    let out = run(&mut s, sql).await;
    assert_eq!(out.results[0].tag.as_deref(), Some("CREATE PROCEDURE"));
    let warn: Vec<_> = out.log.iter().filter(|m| m.level == MessageLevel::Warning).collect();
    assert_eq!(warn[0].code.as_deref(), Some("ORA-24344"), "{:?}", out.log);
    assert!(warn[0].text.contains("DBINE_SCR_P"), "{:?}", warn[0]);
    let errs: Vec<_> = out.errors.iter().filter(|e| e.code.as_deref() == Some("PLS-00201")).collect();
    assert_eq!(errs.len(), 1, "{:?}", out.errors);
    assert_eq!((errs[0].line, errs[0].offset), (Some(4), Some(sql.find("no_such_thing").unwrap())), "{:?}", errs[0]);

    // SHOW ERRORS: the last one compiled.
    let out = run(&mut s, "SHOW ERRORS").await;
    let lines = texts(&out, MessageLevel::Warning);
    assert!(lines.iter().any(|l| l.starts_with("Línea 3, columna 3: PLS-00201")), "{lines:?}");
    // …or the one named.
    let out = run(&mut s, "sho err procedure dbine_scr_p").await;
    assert!(!texts(&out, MessageLevel::Warning).is_empty());

    // A package body: lines count from PACKAGE.
    run(&mut s, "create or replace package dbine_scr_pk as\n  procedure p;\nend;").await;
    let sql = "create or replace package body dbine_scr_pk as\n  procedure p is\n  begin\n    nope;\n  end;\nend;";
    let out = run(&mut s, sql).await;
    let e = out.errors.iter().find(|e| e.code.as_deref() == Some("PLS-00201")).expect("PLS-00201");
    assert_eq!((e.line, e.offset), (Some(4), Some(sql.find("nope").unwrap())), "{e:?}");

    // A trigger: its error lines count from its PL/SQL block.
    let sql = "create or replace trigger dbine_scr_trg\nbefore insert on dbine_scr_t\nfor each row\ndeclare\n  x number;\nbegin\n  :new.a := nope;\nend;";
    let out = run(&mut s, sql).await;
    let e = out.errors.iter().find(|e| e.code.as_deref() == Some("PLS-00201")).expect("PLS-00201");
    assert_eq!((e.line, e.offset), (Some(7), Some(sql.find("nope").unwrap())), "{e:?}");
    let out = run(&mut s, "SHOW ERRORS").await;
    assert!(texts(&out, MessageLevel::Warning).iter().any(|l| l.contains("PLS-00201")), "{:?}", out.log);

    // Fixed: no errors, SHOW ERRORS says so.
    let out = run(&mut s, "create or replace procedure dbine_scr_p as\nbegin\n  null;\nend;").await;
    assert!(out.errors.is_empty() && out.log.is_empty(), "{:?}", out.log);
    let out = run(&mut s, "SHOW ERRORS").await;
    assert_eq!(texts(&out, MessageLevel::Info), vec!["PROCEDURE DBINE_SCR_P: sin errores."]);

    for d in ["DROP TRIGGER dbine_scr_trg", "DROP PACKAGE dbine_scr_pk", "DROP PROCEDURE dbine_scr_p", "DROP TABLE dbine_scr_t PURGE"] {
        quiet(&mut s, d).await;
    }
}

#[tokio::test]
#[ignore]
async fn manual_and_automatic_transactions() {
    let mut s = session().await;
    let mut other = session().await;
    quiet(&mut s, "DROP TABLE dbine_scr_tx PURGE").await;
    run(&mut s, "CREATE TABLE dbine_scr_tx (a NUMBER)").await;
    let count = |o: QueryOutcome| o.results[0].rows[0][0].clone();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));

    // Manual: the insert stays pending until commit / rollback.
    s.set_autocommit(false).await.unwrap();
    let out = run(&mut s, "insert into dbine_scr_tx values (1)").await;
    assert_eq!((out.results[0].rows_affected, out.results[0].tag.as_deref()), (Some(1), Some("INSERT")));
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Open));
    assert_eq!(count(run(&mut other, "select count(*) from dbine_scr_tx").await), json!(0));
    s.rollback().await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    assert_eq!(count(run(&mut s, "select count(*) from dbine_scr_tx").await), json!(0));

    // A block too; then commit.
    run(&mut s, "begin insert into dbine_scr_tx values (2); end;").await;
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Open));
    s.commit().await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    assert_eq!(count(run(&mut other, "select count(*) from dbine_scr_tx").await), json!(1));

    // A failed statement keeps the transaction (Oracle rolls back only it).
    run(&mut s, "insert into dbine_scr_tx values (3)").await;
    fail(&mut s, "insert into dbine_scr_tx values ('x')").await;
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Open));
    // Back to Auto commits what's pending.
    s.set_autocommit(true).await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    assert_eq!(count(run(&mut other, "select count(*) from dbine_scr_tx").await), json!(2));

    // Auto: a block's changes are committed at once.
    run(&mut s, "begin insert into dbine_scr_tx values (4); end;").await;
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    assert_eq!(count(run(&mut other, "select count(*) from dbine_scr_tx").await), json!(3));

    quiet(&mut s, "DROP TABLE dbine_scr_tx PURGE").await;
}

/// The cancel kills the server session (the thin client has no break):
/// needs ALTER SYSTEM, so it runs only with DBINE_TEST_ORACLE_ADMIN_URL
/// (e.g. oracle://system:Secret123@localhost:25601/FREEPDB1). The session
/// reconnects for the next call, with SET SERVEROUTPUT as it was.
#[tokio::test]
#[ignore]
async fn cancel_ends_the_server_session() {
    let Ok(url) = std::env::var("DBINE_TEST_ORACLE_ADMIN_URL") else { return };
    std::env::set_var("DBINE_TEST_ORACLE_URL", url);
    let mut s = session().await;
    run(&mut s, "SET SERVEROUTPUT OFF").await;
    let sid = run(&mut s, "select sys_context('USERENV', 'SESSIONID') || '/' || sys_context('USERENV', 'SID') from dual").await.results[0].rows[0][0].clone();
    let stop = s.interrupter().expect("an interrupter");
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        stop();
    });
    let started = std::time::Instant::now();
    let e = s.execute("BEGIN DBMS_SESSION.SLEEP(30); END;", 10, &mut QueryOutcome::default()).await.unwrap_err();
    assert!(matches!(e, Error::Cancelled), "{e:?}");
    assert!(started.elapsed().as_secs() < 15);
    let now = run(&mut s, "select sys_context('USERENV', 'SESSIONID') || '/' || sys_context('USERENV', 'SID') from dual").await.results[0].rows[0][0].clone();
    assert_ne!(sid, now, "a new server session");
    let out = run(&mut s, "begin dbms_output.put_line('no'); end;").await;
    assert!(out.log.is_empty(), "SERVEROUTPUT stays off: {:?}", out.log);
}

/// The editor's run of a whole script (`Whole`): each unit reported live,
/// and with «Seguir si hay un error» the failures are recorded and the
/// script goes on, as SQL*Plus does; without it, it stops at the first.
#[tokio::test]
#[ignore]
async fn whole_script_goes_on_after_errors() {
    use dbine_driver::{ProgressSinkRef, StatementEnd};
    let mut s = session().await;
    quiet(&mut s, "DROP TABLE dbine_whole_t PURGE").await;
    let sql = "SET SERVEROUTPUT ON\n\
               PROMPT Creando tabla\n\
               CREATE TABLE dbine_whole_t (id NUMBER PRIMARY KEY);\n\
               INSERT INTO dbine_whole_t VALUES (1);\n\
               INSERT INTO dbine_whole_t VALUES (1);\n\
               BEGIN\n  DBMS_OUTPUT.PUT_LINE('bloque');\nEND;\n/\n\
               DECLARE\n  n NUMBER;\nBEGIN\n  n := 1 / 0;\nEND;\n/\n\
               SELECT COUNT(*) FROM dbine_whole_t;\n\
               DROP TABLE dbine_whole_t PURGE;\n";
    let ends: Arc<Mutex<Vec<StatementEnd>>> = Arc::default();
    let sink = ends.clone();
    let mut out = QueryOutcome {
        continue_on_error: Some(true),
        progress_sink: Some(ProgressSinkRef(Arc::new(move |e: &StatementEnd| sink.lock().unwrap().push(e.clone())))),
        ..Default::default()
    };
    s.execute(sql, 100, &mut out).await.expect("goes on after errors");
    let codes: Vec<_> = out.errors.iter().map(|e| e.code.clone().unwrap_or_default()).collect();
    assert_eq!(codes, ["ORA-00001", "ORA-01476"], "{:?}", out.errors);
    assert_eq!(out.errors[0].line, Some(5), "{:?}", out.errors[0]);
    assert_eq!(out.errors[0].statement, Some(4));
    let select = out.results.iter().find(|r| r.statement == Some(7)).expect("the SELECT after the failures ran");
    assert_eq!(select.rows[0][0], json!(1));
    assert!(texts(&out, MessageLevel::Info).contains(&"bloque".to_string()), "{:?}", out.log);
    let ends = std::mem::take(&mut *ends.lock().unwrap());
    assert_eq!(ends.len(), 9, "every unit reported");
    assert_eq!(ends.iter().map(|e| e.statement).collect::<Vec<_>>(), (0..9).collect::<Vec<_>>());
    assert_eq!((ends[4].line, ends[4].errors.len()), (5, 1));
    assert_eq!(ends[7].results.len(), 1);

    // Without it: stops at the first failure, which is recorded once.
    quiet(&mut s, "CREATE TABLE dbine_whole_t (id NUMBER PRIMARY KEY)").await;
    let mut out = QueryOutcome { continue_on_error: Some(false), ..Default::default() };
    let sql = "INSERT INTO dbine_whole_t VALUES (1);\nINSERT INTO dbine_whole_t VALUES (1);\nSELECT 1 FROM dual;";
    let e = s.execute(sql, 100, &mut out).await.unwrap_err();
    assert!(e.to_string().starts_with("ORA-00001"), "{e:?}");
    assert_eq!(out.errors.len(), 1);
    assert!(out.results.iter().all(|r| r.rows.is_empty()), "the SELECT didn't run: {:?}", out.results);
    quiet(&mut s, "DROP TABLE dbine_whole_t PURGE").await;
}
