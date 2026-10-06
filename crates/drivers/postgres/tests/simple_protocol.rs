//! The simple-protocol mode ("Protocolo de consultas"), against a real
//! server: forced on the connection, and found on connect through a
//! gateway that refuses the extended protocol the way the reported one did
//! (`0A000`, "Extended query protocol is not supported by this gateway").
//! Reads `DBINE_TEST_POSTGRES_URL` and is skipped without it:
//!
//! ```sh
//! DBINE_TEST_POSTGRES_URL=postgres://postgres:pw@localhost:25010/postgres \
//!   cargo test -p dbine-driver-postgres --test simple_protocol -- --ignored
//! ```

use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, Session};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

fn cfg() -> Option<ConnectionConfig> {
    let url = std::env::var("DBINE_TEST_POSTGRES_URL").ok()?;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let (auth, hostpart) = rest.rsplit_once('@').unwrap_or(("", rest));
    let (user, pass) = auth.split_once(':').map_or((auth, None), |(u, p)| (u, Some(p)));
    let (hostport, db) = hostpart.split_once('/').unwrap_or((hostpart, ""));
    let (host, port) = hostport.rsplit_once(':')?;
    Some(ConnectionConfig {
        driver: "postgres".into(),
        host: host.into(),
        port: port.parse().ok()?,
        database: db.into(),
        username: (!user.is_empty()).then(|| user.into()),
        password: pass.map(Into::into),
        ..Default::default()
    })
}

/// Explorer and editor work on a simple-only session.
async fn exercise(s: &mut Box<dyn Session>, t: &str) {
    let mut out = QueryOutcome::default();
    s.execute(&format!("DROP TABLE IF EXISTS {t}; CREATE TABLE {t} (id int PRIMARY KEY, name text DEFAULT 'x''y');"), 10, &mut out)
        .await
        .unwrap();
    let mut out = QueryOutcome::default();
    s.execute(&format!("INSERT INTO {t} VALUES (1, 'uno'); SELECT id, name FROM {t}"), 10, &mut out).await.unwrap();
    let r = out.results.last().unwrap();
    assert_eq!(r.rows.len(), 1, "{r:?}");
    assert_eq!(r.columns.len(), 2);

    let objects = s.list_objects().await.unwrap();
    assert!(objects.iter().any(|o| o.name == t && o.kind == "table"), "the table is listed");
    let table = ObjectRef { kind: "table".into(), schema: Some("public".into()), name: t.into() };
    let cols = s.columns(&table).await.unwrap();
    assert_eq!(cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["id", "name"]);
    assert!(cols[0].primary_key && !cols[0].nullable && cols[1].nullable);
    assert_eq!(cols[1].default_value.as_deref(), Some("'x''y'::text"));

    let mut out = QueryOutcome::default();
    s.execute(&format!("CREATE OR REPLACE VIEW {t}_v AS SELECT id FROM {t}"), 10, &mut out).await.unwrap();
    let v = ObjectRef { kind: "view".into(), schema: Some("public".into()), name: format!("{t}_v") };
    assert!(s.definition(&v).await.unwrap().unwrap_or_default().contains(t), "view definition");

    let mut out = QueryOutcome::default();
    s.execute(&format!("DROP VIEW {t}_v; DROP TABLE {t}"), 10, &mut out).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn forced_simple() {
    let Some(mut c) = cfg() else {
        eprintln!("DBINE_TEST_POSTGRES_URL not set; skipping");
        return;
    };
    c.options.insert("query_protocol".into(), "simple".into());
    let d = dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == "postgres").unwrap();
    assert!(d.info().fields.iter().any(|f| f.key == "query_protocol"));
    let mut s = d.connect(&c, None).await.unwrap();
    exercise(&mut s, "dbine_simple_forced").await;
}

/// A gateway in front of the server that answers every `Parse` the way the
/// reported one did and passes everything else through. Counts the
/// refused `Parse` messages.
async fn gateway(upstream: String) -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let refused = Arc::new(AtomicUsize::new(0));
    let count = refused.clone();
    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = listener.accept().await else { return };
            let upstream = upstream.clone();
            let count = count.clone();
            tokio::spawn(async move {
                let _ = relay(client, &upstream, count).await;
            });
        }
    });
    (port, refused)
}

async fn read_msg(r: &mut (impl AsyncReadExt + Unpin)) -> std::io::Result<(u8, Vec<u8>)> {
    let tag = r.read_u8().await?;
    let len = r.read_u32().await? as usize;
    let mut body = vec![0; len - 4];
    r.read_exact(&mut body).await?;
    Ok((tag, body))
}

fn msg(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut m = vec![tag];
    m.extend_from_slice(&(body.len() as u32 + 4).to_be_bytes());
    m.extend_from_slice(body);
    m
}

async fn relay(mut client: TcpStream, upstream: &str, refused: Arc<AtomicUsize>) -> std::io::Result<()> {
    // Startup: refuse TLS, then pass the startup packet on.
    let mut server = TcpStream::connect(upstream).await?;
    loop {
        let len = client.read_u32().await? as usize;
        let mut body = vec![0; len - 4];
        client.read_exact(&mut body).await?;
        if body.starts_with(&80877103u32.to_be_bytes()) {
            client.write_all(b"N").await?;
            continue;
        }
        server.write_all(&(len as u32).to_be_bytes()).await?;
        server.write_all(&body).await?;
        break;
    }
    let (mut cr, cw) = client.into_split();
    let (mut sr, mut sw) = server.into_split();
    let cw = Arc::new(Mutex::new(cw));
    let to_client = cw.clone();
    tokio::spawn(async move {
        while let Ok((tag, body)) = read_msg(&mut sr).await {
            if to_client.lock().await.write_all(&msg(tag, &body)).await.is_err() {
                break;
            }
        }
    });
    let mut skipping = false;
    loop {
        let (tag, body) = read_msg(&mut cr).await?;
        match tag {
            b'P' => {
                refused.fetch_add(1, Ordering::SeqCst);
                let mut e = Vec::new();
                for (k, v) in [(b'S', "ERROR"), (b'C', "0A000"), (b'M', "Extended query protocol is not supported by this gateway. Use simple Query (sql.text without bound parameters).")] {
                    e.push(k);
                    e.extend_from_slice(v.as_bytes());
                    e.push(0);
                }
                e.push(0);
                cw.lock().await.write_all(&msg(b'E', &e)).await?;
                skipping = true;
            }
            b'S' if skipping => {
                skipping = false;
                cw.lock().await.write_all(&msg(b'Z', b"I")).await?;
            }
            _ if skipping => {}
            _ => sw.write_all(&msg(tag, &body)).await?,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn found_behind_a_gateway() {
    let Some(mut c) = cfg() else {
        eprintln!("DBINE_TEST_POSTGRES_URL not set; skipping");
        return;
    };
    let (port, refused) = gateway(format!("{}:{}", c.host, c.port)).await;
    c.host = "127.0.0.1".into();
    c.port = port;
    let d = dbine_driver_postgres::drivers().into_iter().find(|d| d.info().id == "postgres").unwrap();
    let mut s = d.connect(&c, None).await.unwrap();
    assert_eq!(refused.load(Ordering::SeqCst), 1, "one probe on connect");
    exercise(&mut s, "dbine_simple_gateway").await;
    assert_eq!(refused.load(Ordering::SeqCst), 1, "nothing else tried the extended protocol");
    let err = s
        .key_range(&ObjectRef { kind: "table".into(), schema: Some("public".into()), name: "x".into() }, "id")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("protocolo simple"), "{err}");
}
