//! The driver against an in-process server that speaks kdb+'s IPC: the
//! handshake, sync queries and their answers, errors, and a query that
//! takes long enough to be cancelled, a sync request from the server, a
//! connection the server drops while idle and a result too big for the
//! grid. What a real kdb+ would answer is written out per query; the grid's
//! guard (`-22!`) is played by the size of the serialized answer.

use dbine_driver::{kinds, ConnectionConfig, Error, ObjectRef, QueryOutcome};
use dbine_driver_kdb::ipc::{self, Vector, K};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn syms(v: &[&str]) -> K {
    K::Vector(0, Vector::Symbol(v.iter().map(|s| s.to_string()).collect()))
}

fn trade() -> K {
    K::Table(
        vec!["sym".into(), "px".into(), "time".into()],
        vec![
            syms(&["AAPL", "MSFT"]),
            K::Vector(0, Vector::Float(vec![1.5, f64::NAN])),
            K::Vector(0, Vector::Timestamp(vec![0, i64::MIN])),
        ],
    )
}

/// The explorer's table: the root (an HDB's partitioned and splayed
/// tables among its own), a one-letter user namespace (`.u` of kdb+tick)
/// and `.myns`.
fn explorer() -> K {
    let rows = [
        (".", "table", "trade"),
        (".", "partitioned", "quotes"),
        (".", "splayed", "daily"),
        (".", "function", "f1"),
        (".", "variable", "x"),
        (".", "view", "v1"),
        (".u", "function", "upd"),
        (".u", "variable", "w"),
        (".myns", "table", "t2"),
    ];
    K::Table(
        vec!["ns".into(), "k".into(), "n".into()],
        vec![
            syms(&rows.map(|r| r.0)),
            syms(&rows.map(|r| r.1)),
            syms(&rows.map(|r| r.2)),
        ],
    )
}

/// What the fake kdb+ answers to `q`.
fn answer(q: &str) -> K {
    match q {
        "@[{p:parse x;$[(type p)in 0 -11h;reval p;p];1b};\"1+1\";0b]" => K::Bool(true),
        "(.z.K;.z.k)" => K::List(vec![K::Float(4.1), K::Date(9000)]),
        q if q == dbine_driver_kdb::EXPLORER => explorer(),
        "select from .Q.ind[quotes; `long$til 100&count quotes]" => trade(),
        "big[]" | "ask[]" | "bye[]" => K::Vector(0, Vector::Long((0..1_000_000).collect())),
        "0!meta trade" => K::Table(
            vec!["c".into(), "t".into(), "f".into(), "a".into()],
            vec![
                syms(&["sym", "px", "time"]),
                K::Vector(0, Vector::Char(b"sfp".to_vec())),
                syms(&["", "", ""]),
                syms(&["p", "", ""]),
            ],
        ),
        "@[keys;trade;`$()]" => syms(&["sym"]),
        "get `f1" => K::Lambda(String::new(), "{[x] x+1}".into()),
        "select[100] from trade"
        | "{p:parse x;$[(type p)in 0 -11h;reval p;p]} \"select from trade\"" => trade(),
        "1+`a" => K::Error("type".into()),
        "a:1" => K::Nil,
        "til 3" => K::Vector(0, Vector::Long(vec![0, 1, 2])),
        _ => K::Error(format!("unknown query: {q}")),
    }
}

/// A q string literal's text.
fn unquote(s: &str) -> Option<String> {
    let s = s.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::new();
    let mut cs = s.chars();
    while let Some(c) = cs.next() {
        out.push(match c {
            '\\' => match cs.next()? {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                c => c,
            },
            c => c,
        });
    }
    Some(out)
}

/// The answer to what the client sent: a statement as the grid sends it
/// (`{guard}[rows;bytes] value "…"`) is unwrapped, run, and cut as the
/// guard would cut it; `value "…"` runs what it quotes.
fn reply(q: &str) -> K {
    let Some(rest) = q.strip_prefix("{[n;b;r] ") else {
        return match q.strip_prefix("value ").and_then(unquote) {
            Some(stmt) => answer(&stmt),
            None => answer(q),
        };
    };
    let (_, rest) = rest.split_once("}[").unwrap();
    let (args, expr) = rest.split_once("] ").unwrap();
    let (n, b) = args.split_once(';').unwrap();
    let (n, b): (usize, usize) = (n.parse().unwrap(), b.parse().unwrap());
    let r = reply(expr);
    match &r {
        K::Vector(a, Vector::Long(v))
            if v.len() > n && ipc::message(ipc::RESPONSE, &r).len() > b =>
        {
            K::List(vec![
                K::Symbol("dbine.partial".into()),
                K::Long(v.len() as i64),
                K::Vector(*a, Vector::Long(v[..n].to_vec())),
            ])
        }
        _ => r,
    }
}

/// What the client answered to the server's sync requests.
type Replies = Arc<std::sync::Mutex<Vec<K>>>;

async fn read_message(s: &mut TcpStream) -> Option<Vec<u8>> {
    let mut head = [0u8; 8];
    s.read_exact(&mut head).await.ok()?;
    let h = ipc::header(&head).unwrap();
    let mut msg = vec![0u8; h.len];
    msg[..8].copy_from_slice(&head);
    s.read_exact(&mut msg[8..]).await.ok()?;
    Some(msg)
}

async fn session(mut s: TcpStream, slow: Arc<AtomicUsize>, replies: Replies) {
    // The handshake: credentials up to a 0 byte.
    let mut hello = Vec::new();
    loop {
        let mut b = [0u8; 1];
        if s.read_exact(&mut b).await.is_err() {
            return;
        }
        if b[0] == 0 {
            break;
        }
        hello.push(b[0]);
    }
    let creds = String::from_utf8_lossy(&hello[..hello.len().saturating_sub(1)]).into_owned();
    if creds != "u:p" {
        return; // kdb+ closes the connection on a bad login
    }
    s.write_all(&[ipc::CAPABILITY]).await.unwrap();
    loop {
        let Some(msg) = read_message(&mut s).await else {
            return;
        };
        let q = ipc::decode(&msg).unwrap().text().unwrap();
        if q.ends_with("value \"slow[]\"") {
            slow.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        if q.ends_with("value \"ask[]\"") {
            // A callback that asks the client something and waits.
            s.write_all(&ipc::message(ipc::SYNC, &K::Symbol("ping".into())))
                .await
                .unwrap();
            let answer = read_message(&mut s).await.unwrap();
            assert_eq!(
                ipc::header(answer[..8].try_into().unwrap()).unwrap().kind,
                ipc::RESPONSE
            );
            replies.lock().unwrap().push(ipc::decode(&answer).unwrap());
        }
        // An async message first, as a server's callback would send:
        // the client skips it.
        s.write_all(&ipc::message(ipc::ASYNC, &K::Long(0)))
            .await
            .unwrap();
        s.write_all(&ipc::message(ipc::RESPONSE, &reply(&q)))
            .await
            .unwrap();
        if q.ends_with("value \"bye[]\"") {
            // The connection dropped while idle (a firewall, a restart).
            return;
        }
    }
}

async fn server() -> (u16, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let (port, accepted, slow, _) = server_with_replies().await;
    (port, accepted, slow)
}

async fn server_with_replies() -> (u16, Arc<AtomicUsize>, Arc<AtomicUsize>, Replies) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let slow = Arc::new(AtomicUsize::new(0));
    let replies: Replies = Arc::default();
    let (a, sl, r) = (accepted.clone(), slow.clone(), replies.clone());
    tokio::spawn(async move {
        loop {
            let (s, _) = l.accept().await.unwrap();
            a.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(session(s, sl.clone(), r.clone()));
        }
    });
    (port, accepted, slow, replies)
}

fn cfg(port: u16, user: &str, read_only: bool) -> ConnectionConfig {
    ConnectionConfig {
        host: "127.0.0.1".into(),
        port,
        username: Some(user.into()),
        password: Some("p".into()),
        read_only,
        options: [("tcps".to_string(), "false".to_string())].into(),
        ..Default::default()
    }
}

fn driver() -> Arc<dyn dbine_driver::Driver> {
    dbine_driver_kdb::drivers().remove(0)
}

#[tokio::test]
async fn connect_explore_and_query() {
    let (port, _, _) = server().await;
    let mut s = driver()
        .connect(&cfg(port, "u", false), None)
        .await
        .unwrap();
    assert_eq!(s.server_version().await.unwrap(), "kdb+ 4.1 (2024-08-22)");
    assert_eq!(s.list_databases().await.unwrap(), ["default"]);

    let objs = s.list_objects().await.unwrap();
    let names: Vec<(String, String)> = objs.into_iter().map(|o| (o.kind, o.name)).collect();
    assert_eq!(
        names,
        [
            ("table".to_string(), "trade".to_string()),
            ("partitioned_table".into(), "quotes".into()),
            ("splayed_table".into(), "daily".into()),
            ("view".into(), "v1".into()),
            ("function".into(), "f1".into()),
            ("variable".into(), "x".into()),
            ("function".into(), ".u.upd".into()),
            ("variable".into(), ".u.w".into()),
            ("table".into(), ".myns.t2".into()),
        ]
    );

    // A partitioned table opens by index, not with a scan of every
    // partition; its rows can't be edited.
    let quotes = ObjectRef {
        kind: "partitioned_table".into(),
        schema: None,
        name: "quotes".into(),
    };
    let browse = s.browse_query(&quotes, 100);
    assert_eq!(
        browse,
        "select from .Q.ind[quotes; `long$til 100&count quotes]"
    );
    let mut out = QueryOutcome::default();
    s.execute(&browse, 1000, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 2);
    let e = driver()
        .update_script(&quotes, &[])
        .err()
        .unwrap()
        .to_string();
    assert!(e.contains("particionada"), "{e}");

    let trade = ObjectRef {
        kind: kinds::TABLE.into(),
        schema: None,
        name: "trade".into(),
    };
    let cols = s.columns(&trade).await.unwrap();
    assert_eq!(
        cols.iter()
            .map(|c| (c.name.as_str(), c.data_type.as_str(), c.primary_key))
            .collect::<Vec<_>>(),
        [
            ("sym", "symbol", true),
            ("px", "float", false),
            ("time", "timestamp", false)
        ]
    );
    assert_eq!(
        s.definition(&trade).await.unwrap().as_deref(),
        Some("trade:([sym:`symbol$()] px:`float$(); time:`timestamp$())")
    );
    let f1 = ObjectRef {
        kind: kinds::FUNCTION.into(),
        schema: None,
        name: "f1".into(),
    };
    assert_eq!(
        s.definition(&f1).await.unwrap().as_deref(),
        Some("{[x] x+1}")
    );

    let browse = s.browse_query(&trade, 100);
    let mut out = QueryOutcome::default();
    s.execute(&browse, 1000, &mut out).await.unwrap();
    let r = &out.results[0];
    assert_eq!(r.columns[2].type_name, "timestamp");
    assert_eq!(
        r.rows,
        vec![
            vec![json!("AAPL"), json!(1.5), json!("2000-01-01 00:00:00")],
            vec![
                json!("MSFT"),
                serde_json::Value::Null,
                serde_json::Value::Null
            ],
        ]
    );

    // A script: each statement its own result; a q error stops it.
    let mut out = QueryOutcome::default();
    let e = s
        .execute("a:1\ntil 3\n1+`a\ntil 3", 1000, &mut out)
        .await
        .unwrap_err();
    assert_eq!(e.to_string(), "'type");
    assert_eq!(out.results.len(), 2);
    assert_eq!(out.results[1].rows.len(), 3);
    assert_eq!(out.results[1].statement, Some(1));
    assert_eq!(e.to_script_error().line, Some(3));
}

#[tokio::test]
async fn a_bad_login_is_an_auth_failure() {
    let (port, _, _) = server().await;
    let e = driver()
        .connect(&cfg(port, "nobody", false), None)
        .await
        .err()
        .unwrap();
    assert!(matches!(e, Error::AuthFailed(_)), "{e:?}");
}

#[tokio::test]
async fn read_only_refuses_writes_and_runs_reads_inside_reval() {
    let (port, _, _) = server().await;
    let mut s = driver().connect(&cfg(port, "u", true), None).await.unwrap();
    let mut out = QueryOutcome::default();
    let e = s
        .execute("til 3\n`t upsert (1;2)", 100, &mut out)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("solo lectura"), "{e}");
    // Nothing ran: the whole script is checked first.
    assert!(out.results.is_empty());
    let mut out = QueryOutcome::default();
    s.run_read_only("select from trade", 100, &mut out)
        .await
        .unwrap();
    assert_eq!(out.results[0].rows.len(), 2);
    assert!(s
        .run_read_only("a:1", 100, &mut QueryOutcome::default())
        .await
        .is_err());
}

#[tokio::test]
async fn a_cancel_drops_the_connection_and_the_next_query_reconnects() {
    let (port, accepted, slow) = server().await;
    let mut s = driver()
        .connect(&cfg(port, "u", false), None)
        .await
        .unwrap();
    let stop = s.interrupter().unwrap();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        stop();
    });
    let started = std::time::Instant::now();
    let e = s
        .execute("slow[]", 10, &mut QueryOutcome::default())
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Cancelled), "{e:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert_eq!(slow.load(Ordering::SeqCst), 1);
    let before = accepted.load(Ordering::SeqCst);
    let mut out = QueryOutcome::default();
    s.execute("til 3", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 3);
    assert_eq!(accepted.load(Ordering::SeqCst), before + 1);
}

#[tokio::test]
async fn a_sync_request_from_the_server_gets_an_error_and_the_answer_still_comes() {
    let (port, _, _, replies) = server_with_replies().await;
    let mut s = driver()
        .connect(&cfg(port, "u", false), None)
        .await
        .unwrap();
    let mut out = QueryOutcome::default();
    tokio::time::timeout(Duration::from_secs(5), s.execute("ask[]", 10, &mut out))
        .await
        .expect("no hang")
        .unwrap();
    assert_eq!(out.results[0].rows.len(), 10);
    let replies = replies.lock().unwrap();
    assert!(matches!(replies.as_slice(), [K::Error(_)]), "{replies:?}");
}

#[tokio::test]
async fn a_big_result_comes_cut_for_the_grid_and_whole_without_a_limit() {
    let (port, _, _) = server().await;
    let mut s = driver()
        .connect(&cfg(port, "u", false), None)
        .await
        .unwrap();
    // The grid: the first rows and the whole count, marked partial.
    let mut out = QueryOutcome::default();
    s.execute("big[]", 5000, &mut out).await.unwrap();
    let r = &out.results[0];
    assert_eq!(r.rows.len(), 5000);
    assert_eq!(r.total_rows, 1_000_000);
    assert!(r.truncated);
    assert_eq!(r.rows[4999], vec![json!(4999)]);
    // Export (no limit): every row, nothing cut on the server.
    let mut out = QueryOutcome::default();
    s.execute("big[]", usize::MAX, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 1_000_000);
    assert!(!out.results[0].truncated);
    // A small result isn't touched.
    let mut out = QueryOutcome::default();
    s.execute("til 3", 2, &mut out).await.unwrap();
    assert_eq!(
        (out.results[0].rows.len(), out.results[0].total_rows),
        (2, 3)
    );
}

#[tokio::test]
async fn a_connection_dropped_while_idle_is_opened_again_and_the_query_sent_once_more() {
    let (port, accepted, _) = server().await;
    let mut s = driver()
        .connect(&cfg(port, "u", false), None)
        .await
        .unwrap();
    // The server answers, then drops the connection.
    s.execute("bye[]", 10, &mut QueryOutcome::default())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let before = accepted.load(Ordering::SeqCst);
    let mut out = QueryOutcome::default();
    s.execute("til 3", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 3);
    assert_eq!(accepted.load(Ordering::SeqCst), before + 1);
}
