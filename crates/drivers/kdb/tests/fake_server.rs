//! The driver against an in-process server that speaks kdb+'s IPC: the
//! handshake, sync queries and their answers, errors, and a query that
//! takes long enough to be cancelled. What a real kdb+ would answer is
//! written out per query.

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

/// What the fake kdb+ answers to `q`.
fn answer(q: &str) -> K {
    match q {
        "@[{reval parse x;1b};\"1\";0b]" => K::Bool(true),
        "(.z.K;.z.k)" => K::List(vec![K::Float(4.1), K::Date(9000)]),
        "{x where 1<count each string x} key `" => syms(&["myns"]),
        "system \"b\"" => syms(&["v1"]),
        "tables `." => syms(&["trade"]),
        "tables `.myns" => syms(&["t2"]),
        "system \"f\"" => syms(&["f1"]),
        "system \"f .myns\"" => syms(&[]),
        "system \"v\"" => syms(&["trade", "v1", "x"]),
        "system \"v .myns\"" => syms(&["t2"]),
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
        "select[100] from trade" | "reval parse \"select from trade\"" => trade(),
        "1+`a" => K::Error("type".into()),
        "a:1" => K::Nil,
        "til 3" => K::Vector(0, Vector::Long(vec![0, 1, 2])),
        _ => K::Error(format!("unknown query: {q}")),
    }
}

async fn session(mut s: TcpStream, slow: Arc<AtomicUsize>) {
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
        let mut head = [0u8; 8];
        if s.read_exact(&mut head).await.is_err() {
            return;
        }
        let h = ipc::header(&head).unwrap();
        let mut msg = vec![0u8; h.len];
        msg[..8].copy_from_slice(&head);
        s.read_exact(&mut msg[8..]).await.unwrap();
        let q = ipc::decode(&msg).unwrap().text().unwrap();
        if q == "slow[]" {
            slow.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
        // An async message first, as a server's callback would send:
        // the client skips it.
        s.write_all(&ipc::message(ipc::ASYNC, &K::Long(0)))
            .await
            .unwrap();
        s.write_all(&ipc::message(ipc::RESPONSE, &answer(&q)))
            .await
            .unwrap();
    }
}

async fn server() -> (u16, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let slow = Arc::new(AtomicUsize::new(0));
    let (a, sl) = (accepted.clone(), slow.clone());
    tokio::spawn(async move {
        loop {
            let (s, _) = l.accept().await.unwrap();
            a.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(session(s, sl.clone()));
        }
    });
    (port, accepted, slow)
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
            ("view".into(), "v1".into()),
            ("function".into(), "f1".into()),
            ("variable".into(), "x".into()),
            ("table".into(), ".myns.t2".into()),
        ]
    );

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
