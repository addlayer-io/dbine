//! Cancel against a real server: the interrupter sends a TDS attention on the
//! session's own connection. Reads `DBINE_TEST_SQLSERVER_URL` /
//! `DBINE_TEST_BABELFISH_URL` (`mssql://user:pass@host:port`) and is skipped
//! without them:
//!
//! ```sh
//! DBINE_TEST_SQLSERVER_URL='mssql://sa:…@localhost:25013' \
//! DBINE_TEST_BABELFISH_URL='mssql://babelfish_user:…@localhost:25714' \
//!   cargo test -p dbine-driver-sqlserver --lib cancel_live -- --ignored --test-threads=1
//! ```

use super::*;
use std::time::{Duration, Instant};

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

async fn run(s: &mut SqlServerSession, sql: &str) -> Result<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 100, &mut out).await.map(|_| out)
}

async fn int(s: &mut SqlServerSession, sql: &str) -> i64 {
    let out = run(s, sql).await.unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    let r = out.results.iter().rfind(|r| !r.columns.is_empty()).expect("a result");
    r.rows[0][0].as_i64().unwrap_or_else(|| panic!("{sql}: {:?}", r.rows[0][0]))
}

/// Fire the interrupter after `after`, run `sql`, expect a cancel.
///
/// The bound is measured from the moment the interrupter fires and is far
/// below what the slow batches take uncancelled (WAITFOR runs 30 s), with
/// room for a server just started cold, which acknowledges the attention
/// slower than a warm one.
async fn cancel_during(s: &mut SqlServerSession, sql: &str, after: Duration) {
    let stop = s.interrupter().expect("interrupter");
    let fired = std::sync::Arc::new(std::sync::Mutex::new(None::<Instant>));
    let at = fired.clone();
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        *at.lock().unwrap() = Some(Instant::now());
        stop();
    });
    let e = tokio::time::timeout(Duration::from_secs(25), run(s, sql)).await.expect("the cancel didn't stop it").unwrap_err();
    assert!(matches!(e, Error::Cancelled), "{sql}: {e:?}");
    let fired = fired.lock().unwrap().expect("ended before the interrupter fired");
    assert!(fired.elapsed() < Duration::from_secs(10), "{sql}: the cancel took {:?}", fired.elapsed());
}

/// A batch that runs for a long time without sending anything. Babelfish
/// has no WAITFOR.
fn slow(v: Variant) -> &'static str {
    match v {
        Variant::Babelfish => {
            "SELECT COUNT_BIG(*) FROM sys.all_objects a CROSS JOIN sys.all_objects b CROSS JOIN sys.all_objects c CROSS JOIN sys.all_objects d"
        }
        _ => "WAITFOR DELAY '00:00:30'",
    }
}

async fn keeps_session(env: &str, v: Variant) {
    let Some(cfg) = config(env, v) else { return };
    let d = driver(v);
    let mut s = d.open(&cfg, None).await.expect("connect");
    let spid = int(&mut s, "SELECT CAST(@@SPID AS int)").await;
    let full = v != Variant::Babelfish;
    run(&mut s, "CREATE TABLE #t (n int); INSERT INTO #t VALUES (1), (2)").await.unwrap();
    if full {
        run(&mut s, "SET DATEFIRST 3").await.unwrap();
    }
    run(&mut s, "BEGIN TRANSACTION").await.unwrap();

    // A batch that waits.
    cancel_during(&mut s, slow(v), Duration::from_millis(500)).await;
    assert_eq!(int(&mut s, "SELECT CAST(@@SPID AS int)").await, spid, "same session");
    assert_eq!(int(&mut s, "SELECT COUNT(*) FROM #t").await, 2, "#temp table kept");
    let trancount = int(&mut s, "SELECT CAST(@@TRANCOUNT AS int)").await;
    eprintln!("{env}: @@TRANCOUNT after the cancel = {trancount}");
    if full {
        assert_eq!(int(&mut s, "SELECT CAST(@@DATEFIRST AS int)").await, 3, "SET kept");
        assert_eq!(trancount, 1, "the open transaction is the user's");
    }
    if trancount > 0 {
        run(&mut s, "ROLLBACK").await.unwrap();
    }

    // A batch streaming rows.
    cancel_during(
        &mut s,
        "SELECT a.name, b.name FROM sys.all_objects a CROSS JOIN sys.all_objects b CROSS JOIN sys.all_objects c",
        Duration::from_millis(500),
    )
    .await;
    assert_eq!(int(&mut s, "SELECT CAST(@@SPID AS int)").await, spid);

    // A script cancelled in its first batch doesn't go on with the next.
    cancel_during(&mut s, &format!("{}\nGO\nINSERT INTO #t VALUES (3)", slow(v)), Duration::from_millis(500)).await;
    assert_eq!(int(&mut s, "SELECT COUNT(*) FROM #t").await, 2, "the second batch didn't run");

    // A cancel while nothing runs doesn't touch the next statement.
    s.interrupter().unwrap()();
    assert_eq!(int(&mut s, "SELECT 7").await, 7);

    // The attention goes out after the server finished the batch: its
    // acknowledgement comes alone and must not be read as the next answer.
    for _ in 0..5 {
        let handle = s.client.cancel_handle();
        let mut stream = s
            .client
            .simple_query("SELECT TOP 20000 a.name FROM sys.all_objects a CROSS JOIN sys.all_objects b")
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        handle.cancel();
        let mut rows = 0;
        let end = loop {
            match stream.try_next().await {
                Ok(Some(QueryItem::Row(_))) => rows += 1,
                Ok(Some(_)) => {}
                Ok(None) => break "complete",
                Err(tiberius::error::Error::Cancelled) => break "cancelled",
                Err(e) => panic!("{e:?}"),
            }
        };
        drop(stream);
        eprintln!("{env}: late attention: {end} after {rows} rows");
        assert_eq!(int(&mut s, "SELECT 42").await, 42);
    }
    assert_eq!(int(&mut s, "SELECT CAST(@@SPID AS int)").await, spid);
}

#[tokio::test]
#[ignore]
async fn sqlserver_cancel_keeps_session() {
    keeps_session("DBINE_TEST_SQLSERVER_URL", Variant::SqlServer).await;
}

/// Babelfish ends the session (verified KILL); only that session.
#[tokio::test]
#[ignore]
async fn babelfish_cancel_ends_only_this_session() {
    let v = Variant::Babelfish;
    let Some(cfg) = config("DBINE_TEST_BABELFISH_URL", v) else { return };
    let d = driver(v);
    let mut s = d.open(&cfg, None).await.expect("connect");
    let mut other = d.open(&cfg, None).await.unwrap();
    cancel_during(&mut s, slow(v), Duration::from_millis(500)).await;
    assert!(run(&mut s, "SELECT 1").await.is_err(), "KILL ended the session");
    assert_eq!(int(&mut other, "SELECT CAST(@@SPID AS int)").await, other.spid as i64);
    // The attention itself works, late: Babelfish acknowledges it once the
    // batch is over, and the acknowledgement must not be read as the next
    // answer.
    let handle = other.client.cancel_handle();
    let mut stream = other.client.simple_query("SELECT TOP 2000 a.name FROM sys.all_objects a CROSS JOIN sys.all_objects b").await.unwrap();
    handle.cancel();
    let mut rows = 0;
    let end = loop {
        match stream.try_next().await {
            Ok(Some(QueryItem::Row(_))) => rows += 1,
            Ok(Some(_)) => {}
            Ok(None) => break "complete",
            Err(tiberius::error::Error::Cancelled) => break "cancelled",
            Err(e) => panic!("{e:?}"),
        }
    };
    drop(stream);
    eprintln!("babelfish: late attention: {end} after {rows} rows");
    assert_eq!(int(&mut other, "SELECT 42").await, 42);
}

/// The old interrupter sent `KILL <spid>` with the spid read at connect: after
/// a reconnect that spid could be another client's. The cancel now goes to
/// the current connection (Babelfish: to a backend verified as this one) and
/// never reaches anyone else.
async fn spares_other_sessions(env: &str, v: Variant) {
    let Some(cfg) = config(env, v) else { return };
    let d = driver(v);
    let mut s = d.open(&cfg, None).await.expect("connect");
    let old = s.spid;
    // Taken before the reconnect, as the app keeps it per tab.
    let stop = s.interrupter().unwrap();
    s.reconnect().await.unwrap();
    let new = s.spid;
    assert_ne!(old, new);
    tokio::time::sleep(Duration::from_millis(500)).await;
    // Other clients, one of them likely on the freed spid.
    let mut others = Vec::new();
    for _ in 0..3 {
        others.push(d.open(&cfg, None).await.unwrap());
    }
    let spids: Vec<i32> = others.iter().map(|o| o.spid).collect();
    eprintln!("{env}: old spid {old}, new spid {new}, other sessions {spids:?}");

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        stop();
    });
    let t = Instant::now();
    let e = tokio::time::timeout(Duration::from_secs(20), run(&mut s, slow(v))).await.expect("not cancelled").unwrap_err();
    assert!(matches!(e, Error::Cancelled), "{e:?}");
    assert!(t.elapsed() < Duration::from_secs(5), "the cancel reached the current connection");
    if v == Variant::Babelfish {
        assert!(run(&mut s, "SELECT 1").await.is_err(), "KILL ended the current session");
    } else {
        assert_eq!(int(&mut s, "SELECT CAST(@@SPID AS int)").await, new as i64);
    }
    for (o, spid) in others.iter_mut().zip(spids) {
        assert_eq!(int(o, "SELECT CAST(@@SPID AS int)").await, spid as i64, "session {spid} survives");
    }

    // A session id that isn't this session's is never killed: aim the
    // Babelfish KILL at another session's id with our login time, then with
    // its own.
    if v == Variant::Babelfish {
        let mut o = others.remove(0);
        let (victim, ours) = (o.spid, s.cancel.lock().unwrap().backend.clone().unwrap().1);
        let theirs = o.cancel.lock().unwrap().backend.clone().unwrap().1;
        let task = tokio::spawn(async move { run(&mut o, slow(v)).await });
        tokio::time::sleep(Duration::from_millis(500)).await;
        babelfish_kill(s.config.clone(), victim, ours).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!task.is_finished(), "another session was killed");
        babelfish_kill(s.config.clone(), victim, theirs).await;
        let r = tokio::time::timeout(Duration::from_secs(5), task).await.expect("the verified KILL stops it").unwrap();
        eprintln!("{env}: the other session, killed with its own login time: {r:?}");
        assert!(r.is_err());
    }
}

#[tokio::test]
#[ignore]
async fn sqlserver_cancel_after_reconnect_spares_other_sessions() {
    spares_other_sessions("DBINE_TEST_SQLSERVER_URL", Variant::SqlServer).await;
}

#[tokio::test]
#[ignore]
async fn babelfish_cancel_after_reconnect_spares_other_sessions() {
    spares_other_sessions("DBINE_TEST_BABELFISH_URL", Variant::Babelfish).await;
}
