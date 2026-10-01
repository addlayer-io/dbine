//! Scripts against a real server: messages, counts, errors with their line,
//! `GO N`, `USE`, severity 20, FOR XML / JSON, manual transactions. Reads
//! `DBINE_TEST_SQLSERVER_URL` / `DBINE_TEST_BABELFISH_URL` like
//! `cancel_live` and is skipped without them:
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:…@localhost:25013' \
//! DBINE_TEST_BABELFISH_URL='mssql://babelfish_user:…@localhost:25714' \
//!   cargo test -p dbine-driver-sqlserver --lib script_live -- --ignored --test-threads=1
//! ```

use super::*;
use dbine_driver::{MessageLevel, TxState};

fn config(env: &str, v: Variant) -> Option<ConnectionConfig> {
    let Ok(url) = std::env::var(env) else {
        eprintln!("{env} not set; skipping");
        return None;
    };
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostport) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (host, port) = hostport.rsplit_once(':').map_or((hostport, 0), |(h, p)| (h, p.parse().unwrap()));
    Some(ConnectionConfig {
        driver: variant::info(v).id.to_string(),
        host: host.into(),
        port,
        username: Some(user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

fn driver(v: Variant) -> SqlServerDriver {
    SqlServerDriver { info: variant::info(v), variant: v }
}

async fn open(env: &str, v: Variant) -> Option<SqlServerSession> {
    let cfg = config(env, v)?;
    Some(driver(v).open(&cfg, None).await.expect("connect"))
}

async fn run(s: &mut SqlServerSession, sql: &str) -> (QueryOutcome, Result<()>) {
    let mut out = QueryOutcome::default();
    let r = s.execute(sql, 1000, &mut out).await;
    (out, r)
}

fn log(out: &QueryOutcome) -> Vec<(MessageLevel, String, Option<u32>)> {
    out.log.iter().map(|m| (m.level, m.text.clone(), m.line)).collect()
}

fn texts(out: &QueryOutcome) -> Vec<String> {
    out.log.iter().map(|m| m.text.clone()).collect()
}

async fn scalar(s: &mut SqlServerSession, sql: &str) -> serde_json::Value {
    let (out, r) = run(s, sql).await;
    r.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    out.results.iter().rfind(|r| !r.columns.is_empty()).expect("a result").rows[0][0].clone()
}

/// What the editor does on this driver: one `execute` per GO batch, going
/// on after errors unless one is fatal. `GO N` repeats a batch with errors
/// too, as sqlcmd and SSMS do, counting every iteration that ran.
async fn run_editor(d: &SqlServerDriver, s: &mut SqlServerSession, sql: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    for (i, u) in d.split_script(sql).iter().enumerate() {
        out.current_statement = Some(i);
        let repeat = u.repeat.max(1);
        if repeat > 1 {
            out.info("Inicio del ciclo de ejecución");
        }
        let mut done = 0u32;
        for _ in 0..repeat {
            let l0 = out.log.len();
            let e0 = out.errors.len();
            let r = s.execute(&u.text, 1000, &mut out).await;
            // The app moves lines to the script.
            for m in &mut out.log[l0..] {
                if let Some(l) = m.line.as_mut() {
                    *l = u.line + *l - 1;
                }
            }
            for e in &mut out.errors[e0..] {
                if let Some(l) = e.line.as_mut() {
                    *l = u.line + *l - 1;
                }
            }
            done += 1;
            if let Err(e) = r {
                if e.ends_script() {
                    return out;
                }
            }
        }
        if repeat > 1 {
            out.info(format!("Lote ejecutado {done} veces."));
        }
    }
    out
}

#[tokio::test]
#[ignore]
async fn sqlserver_messages_counts_and_errors_in_order() {
    let Some(mut s) = open("DBINE_TEST_SQLSERVER_URL", Variant::SqlServer).await else { return };
    let (out, r) = run(
        &mut s,
        "CREATE TABLE #t (id int PRIMARY KEY, v int);\n\
         PRINT 'hola';\n\
         INSERT INTO #t VALUES (1, 1), (2, 2), (3, NULL);\n\
         RAISERROR('aviso %d', 10, 1, 7);\n\
         SELECT id FROM #t;\n\
         UPDATE #t SET v = 5 WHERE id = 1;\n\
         SELECT SUM(v) FROM #t;\n\
         SET NOCOUNT ON;\n\
         INSERT INTO #t VALUES (4, 4);\n\
         SET NOCOUNT OFF;\n\
         SET STATISTICS IO ON;\n\
         SELECT COUNT(*) FROM #t;\n\
         SET STATISTICS IO OFF;\n",
    )
    .await;
    r.unwrap();
    let t = texts(&out);
    eprintln!("{t:#?}");
    let pos = |needle: &str| t.iter().position(|x| x.contains(needle)).unwrap_or_else(|| panic!("no {needle:?} in {t:#?}"));
    assert!(pos("hola") < pos("(3 filas afectadas)"));
    assert!(pos("(3 filas afectadas)") < pos("aviso 7"));
    assert!(pos("aviso 7") < pos("(1 fila afectada)"), "the UPDATE's count after the RAISERROR");
    // ANSI warning as an INFO token.
    assert!(out.log.iter().any(|m| m.level == MessageLevel::Warning && m.text.contains("Null value is eliminated")), "{t:#?}");
    // NOCOUNT: the second INSERT prints nothing; SELECTs show their rows in the grid.
    assert_eq!(t.iter().filter(|x| x.contains("afectada")).count(), 2, "{t:#?}");
    // STATISTICS IO.
    assert!(t.iter().any(|x| x.contains("Scan count")), "{t:#?}");
    assert_eq!(out.results.iter().filter(|r| !r.columns.is_empty()).count(), 3);
}

#[tokio::test]
#[ignore]
async fn sqlserver_every_error_with_its_script_line() {
    let Some(mut s) = open("DBINE_TEST_SQLSERVER_URL", Variant::SqlServer).await else { return };
    // Whole script (Users & permissions, Backups): the first failing batch
    // ends it, with all its errors.
    let (out, r) = run(
        &mut s,
        "SELECT 1;\nGO\n\nCREATE TABLE #u (id int PRIMARY KEY);\nINSERT INTO #u VALUES (1);\nINSERT INTO #u VALUES (1);\nSELECT * FROM no_existe;\nGO\nSELECT 2;\n",
    )
    .await;
    assert!(r.is_err());
    eprintln!("{:#?}", log(&out));
    let errs: Vec<_> = out.errors.iter().map(|e| (e.code.clone().unwrap_or_default(), e.line)).collect();
    // 2627 at line 6; 208 at line 7 (the batch starts at line 4).
    assert_eq!(errs, vec![("2627".to_string(), Some(6)), ("208".to_string(), Some(7))]);
    assert!(out.log.iter().any(|m| m.text.starts_with("Msg 2627, Nivel 14, Estado 1\n")));
    assert!(out.log.iter().any(|m| m.text.contains("The statement has been terminated.")));
    // The plain message is what other screens show.
    assert!(out.error.as_deref().is_some_and(|e| e.starts_with("Violation of PRIMARY KEY")));
    // The third batch didn't run.
    assert_eq!(out.results.iter().filter(|r| !r.columns.is_empty()).count(), 1);

    // The editor: every batch runs, errors at their script lines.
    let d = driver(Variant::SqlServer);
    let out = run_editor(&d, &mut s, "SELECT 1/0;\nGO\nPRINT 'sigue';\nGO\n\n\nSELECT * FROM no_existe;\nGO\nPRINT 'fin';").await;
    eprintln!("{:#?}", log(&out));
    assert_eq!(out.errors.iter().map(|e| (e.code.clone().unwrap(), e.line)).collect::<Vec<_>>(), vec![
        ("8134".to_string(), Some(1)),
        ("208".to_string(), Some(7))
    ]);
    let t = texts(&out);
    assert!(t.contains(&"sigue".to_string()) && t.contains(&"fin".to_string()), "{t:#?}");
}

#[tokio::test]
#[ignore]
async fn sqlserver_go_n_and_go_in_comments_and_strings() {
    let Some(mut s) = open("DBINE_TEST_SQLSERVER_URL", Variant::SqlServer).await else { return };
    let (out, r) = run(
        &mut s,
        "CREATE TABLE #g (n int);\nGO\nINSERT INTO #g VALUES (1);\nGO 3 -- three times\n/*\nGO\n*/\nSELECT 'a\nGO\nb' AS s, (SELECT COUNT(*) FROM #g) AS n;\n",
    )
    .await;
    r.unwrap();
    let t = texts(&out);
    assert!(t.contains(&"Inicio del ciclo de ejecución".to_string()) && t.contains(&"Lote ejecutado 3 veces.".to_string()), "{t:#?}");
    let last = out.results.iter().rfind(|r| !r.columns.is_empty()).unwrap();
    assert_eq!(last.rows[0], vec![serde_json::json!("a\nGO\nb"), serde_json::json!(3)]);
    // An iteration with errors doesn't end the repeats (sqlcmd and SSMS):
    // three times each, then the next batch.
    let sql = "PRINT 'it'; RAISERROR('e16', 16, 1);\nGO 3\nPRINT 'next';\n";
    let out = run_editor(&driver(Variant::SqlServer), &mut s, sql).await;
    let t = texts(&out);
    eprintln!("{t:#?}");
    assert_eq!(t.iter().filter(|x| *x == "it").count(), 3, "{t:#?}");
    assert_eq!(out.errors.iter().filter(|e| e.code.as_deref() == Some("50000")).count(), 3, "{:#?}", out.errors);
    assert!(t.contains(&"Lote ejecutado 3 veces.".to_string()) && t.contains(&"next".to_string()), "{t:#?}");
    // The whole script: the repeats finish, then the batch with errors ends it.
    let (out, r) = run(&mut s, sql).await;
    assert!(r.is_err());
    let t = texts(&out);
    assert_eq!(t.iter().filter(|x| *x == "it").count(), 3, "{t:#?}");
    assert!(t.contains(&"Lote ejecutado 3 veces.".to_string()) && !t.contains(&"next".to_string()), "{t:#?}");
    // An invalid count: nothing runs.
    let (out, r) = run(&mut s, "PRINT 'no';\nGO 0\n").await;
    assert!(r.is_err());
    assert!(out.log.iter().all(|m| m.text != "no"));
}

#[tokio::test]
#[ignore]
async fn sqlserver_use_is_followed_and_hidden_like_ssms() {
    let Some(mut s) = open("DBINE_TEST_SQLSERVER_URL", Variant::SqlServer).await else { return };
    let (out, r) = run(&mut s, "USE tempdb;").await;
    r.unwrap();
    assert_eq!(out.database.as_deref(), Some("tempdb"));
    assert!(out.log.iter().all(|m| !m.text.contains("Changed database context")), "{:#?}", log(&out));
    assert_eq!(scalar(&mut s, "SELECT DB_NAME()").await, "tempdb");
    // A reconnect keeps it (and the tab with it).
    s.reconnect().await.unwrap();
    assert_eq!(scalar(&mut s, "SELECT DB_NAME()").await, "tempdb");
    let (out, _) = run(&mut s, "SELECT 1").await;
    assert_eq!(out.database, None, "unchanged");
}

#[tokio::test]
#[ignore]
async fn sqlserver_severity_20_ends_the_script_and_a_new_connection_follows() {
    let Some(mut s) = open("DBINE_TEST_SQLSERVER_URL", Variant::SqlServer).await else { return };
    let d = driver(Variant::SqlServer);
    let out = run_editor(&d, &mut s, "PRINT 'antes';\nGO\nRAISERROR('grave', 20, 1) WITH LOG;\nGO\nPRINT 'nunca';").await;
    eprintln!("{:#?} {:#?}", log(&out), out.errors);
    assert!(out.errors.iter().any(|e| e.fatal && e.message.contains("grave")), "{:#?}", out.errors);
    assert!(texts(&out).iter().all(|t| t != "nunca"));
    // The session works again.
    assert_eq!(scalar(&mut s, "SELECT 42").await, 42);
}

#[tokio::test]
#[ignore]
async fn sqlserver_for_xml_and_json_come_as_one_value() {
    let Some(mut s) = open("DBINE_TEST_SQLSERVER_URL", Variant::SqlServer).await else { return };
    for (sql, start) in [
        ("SELECT TOP 300 name, object_id FROM sys.all_objects FOR JSON PATH", "[{"),
        ("SELECT TOP 300 name, object_id FROM sys.all_objects FOR XML PATH('o'), ROOT('r')", "<r>"),
    ] {
        let (out, r) = run(&mut s, sql).await;
        r.unwrap();
        let res = &out.results[0];
        assert_eq!(res.rows.len(), 1, "{sql}");
        let v = res.rows[0][0].as_str().unwrap();
        assert!(v.len() > 4000 && v.starts_with(start), "{sql}: {}", &v[..40]);
        if start == "[{" {
            assert!(serde_json::from_str::<serde_json::Value>(v).is_ok(), "valid JSON");
        }
    }
}

#[tokio::test]
#[ignore]
async fn sqlserver_manual_transactions() {
    let Some(mut s) = open("DBINE_TEST_SQLSERVER_URL", Variant::SqlServer).await else { return };
    manual_transactions(&mut s).await;
}

async fn manual_transactions(s: &mut SqlServerSession) {
    run(s, "IF OBJECT_ID('dbine_tx') IS NOT NULL DROP TABLE dbine_tx; CREATE TABLE dbine_tx (id int);").await.1.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    s.set_autocommit(false).await.unwrap();
    run(s, "INSERT INTO dbine_tx VALUES (1);").await.1.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Open));
    s.rollback().await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    assert_eq!(scalar(s, "SELECT COUNT(*) FROM dbine_tx").await, 0);
    s.rollback().await.unwrap();
    run(s, "INSERT INTO dbine_tx VALUES (2);").await.1.unwrap();
    s.commit().await.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    // Manual mode survives a reconnect.
    s.reconnect().await.unwrap();
    run(s, "INSERT INTO dbine_tx VALUES (3);").await.1.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Open));
    s.rollback().await.unwrap();
    s.set_autocommit(true).await.unwrap();
    run(s, "INSERT INTO dbine_tx VALUES (4);").await.1.unwrap();
    assert_eq!(s.transaction_state().await.unwrap(), Some(TxState::Idle));
    assert_eq!(scalar(s, "SELECT COUNT(*) FROM dbine_tx").await, 2);
    run(s, "DROP TABLE dbine_tx").await.1.unwrap();
}

#[tokio::test]
#[ignore]
async fn babelfish_messages_counts_errors_use_and_transactions() {
    let Some(mut s) = open("DBINE_TEST_BABELFISH_URL", Variant::Babelfish).await else { return };
    let (out, r) = run(
        &mut s,
        "CREATE TABLE #t (id int PRIMARY KEY);\nPRINT 'hola';\nINSERT INTO #t VALUES (1), (2);\nSELECT id FROM #t;\n",
    )
    .await;
    eprintln!("{:#?}", log(&out));
    r.unwrap();
    let t = texts(&out);
    assert!(t.contains(&"hola".to_string()), "{t:#?}");
    assert!(t.contains(&"(2 filas afectadas)".to_string()), "{t:#?}");
    let d = driver(Variant::Babelfish);
    let out = run_editor(&d, &mut s, "SELECT * FROM no_existe;\nGO\nPRINT 'sigue';").await;
    eprintln!("{:#?} {:#?}", log(&out), out.errors);
    assert_eq!(out.errors.len(), 1);
    assert_eq!(out.errors[0].line, Some(1));
    assert!(texts(&out).contains(&"sigue".to_string()));
    let (out, r) = run(&mut s, "USE master;").await;
    r.unwrap();
    assert_eq!(out.database.as_deref(), Some("master"));
    assert!(out.log.is_empty(), "{:#?}", log(&out));
    manual_transactions(&mut s).await;
}
