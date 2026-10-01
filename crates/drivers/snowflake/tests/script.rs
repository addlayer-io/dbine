//! Editor scripts against a fake SQL API: what a run leaves in the session
//! (USE, ALTER SESSION, SET) reaches the next request, and errors come back
//! with their code, SQLSTATE and position.

use dbine_driver::{ConnectionConfig, Error, QueryOutcome};
use serde_json::{json, Value as Json};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Default)]
struct Fake {
    /// Bodies of the POSTed statements.
    posts: Mutex<Vec<Json>>,
    /// The statements of the last multi-statement request, by handle.
    parts: Mutex<Vec<String>>,
}

fn rows(stmt: &str) -> Json {
    if stmt.starts_with("SELECT CURRENT_DATABASE()") {
        return json!({ "resultSetMetaData": { "numRows": 1, "rowType": [{ "name": "a", "type": "text" }, { "name": "b", "type": "text" }, { "name": "c", "type": "text" }, { "name": "d", "type": "text" }] },
                       "data": [["DB2", "S2", "WH", "R"]] });
    }
    if stmt == "SHOW VARIABLES" {
        let cols: Vec<Json> = ["session_id", "created_on", "updated_on", "name", "value", "type", "comment"].iter().map(|n| json!({ "name": n, "type": "text" })).collect();
        return json!({ "resultSetMetaData": { "numRows": 1, "rowType": cols }, "data": [["1", "x", "x", "V", "5", "fixed", null]] });
    }
    if stmt.starts_with("INSERT") {
        return json!({ "resultSetMetaData": { "numRows": 1, "rowType": [{ "name": "number of rows inserted", "type": "fixed", "scale": 0 }] },
                       "data": [["3"]], "stats": { "numRowsInserted": 3 } });
    }
    json!({ "resultSetMetaData": { "numRows": 1, "rowType": [{ "name": "X", "type": "fixed", "scale": 0 }] }, "data": [["1"]] })
}

impl Fake {
    fn answer(&self, method: &str, path: &str, body: &str) -> (u16, Json) {
        if method == "GET" {
            let h = path.trim_start_matches("/api/v2/statements/").split('?').next().unwrap_or("");
            let i: usize = h.trim_start_matches('h').parse().unwrap_or(0);
            let stmt = self.parts.lock().unwrap().get(i).cloned().unwrap_or_default();
            let mut r = rows(&stmt);
            r["statementHandle"] = json!(h);
            return (200, r);
        }
        let b: Json = serde_json::from_str(body).unwrap_or(Json::Null);
        self.posts.lock().unwrap().push(b.clone());
        let stmt = b["statement"].as_str().unwrap_or("").to_string();
        if stmt.contains("fron") {
            return (422, json!({ "code": "001003", "sqlState": "42000", "message": "SQL compilation error:\nsyntax error line 2 at position 0 unexpected 'fron'." }));
        }
        if b.pointer("/parameters/MULTI_STATEMENT_COUNT").is_some() {
            let parts: Vec<String> = stmt.split("\n;\n").map(str::to_string).collect();
            let handles: Vec<String> = (0..parts.len()).map(|i| format!("h{i}")).collect();
            *self.parts.lock().unwrap() = parts;
            return (200, json!({ "statementHandle": "m", "statementHandles": handles, "data": [["Multiple statements executed successfully."]] }));
        }
        (200, rows(&stmt))
    }
}

async fn serve(fake: Arc<Fake>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let fake = fake.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 8192];
                let (head, body) = loop {
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else { continue };
                    let head = String::from_utf8_lossy(&buf[..end]).to_string();
                    let len = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0)))
                        .unwrap_or(0);
                    while buf.len() < end + 4 + len {
                        let n = sock.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    break (head, String::from_utf8_lossy(&buf[end + 4..end + 4 + len]).to_string());
                };
                let mut first = head.lines().next().unwrap_or("").split(' ');
                let (method, path) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
                let (status, json) = fake.answer(method, path, &body);
                let text = json.to_string();
                let resp = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}", text.len());
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn session_state_carries_between_runs() {
    let fake = Arc::new(Fake::default());
    let url = serve(fake.clone()).await;
    let mut cfg = ConnectionConfig { driver: "snowflake".into(), database: "DB1".into(), username: Some("me".into()), ..Default::default() };
    cfg.options.insert("account".into(), url);
    cfg.options.insert("token".into(), "t".into());
    let d = dbine_driver_snowflake::drivers().remove(0);
    assert_eq!(d.script_mode(), dbine_driver::ScriptMode::Whole);
    let mut s = d.connect(&cfg, None).await.unwrap();

    let mut out = QueryOutcome::default();
    s.execute("USE DATABASE DB2; ALTER SESSION SET TIMEZONE = 'UTC';\nSET v = 5; INSERT INTO t VALUES (1), (2), (3); SELECT 1", 100, &mut out)
        .await
        .unwrap();
    // Only the script's statements show; the context and variables queries don't.
    assert_eq!(out.results.len(), 5, "{:?}", out.results);
    assert_eq!(out.results[3].rows_affected, Some(3));
    assert!(out.log.iter().any(|m| m.text == "Contexto: DB2.S2"), "{:?}", out.log);
    // The tab's database follows the USE.
    assert_eq!(out.database.as_deref(), Some("DB2"));
    let last = fake.posts.lock().unwrap().last().cloned().unwrap();
    assert!(last["statement"].as_str().unwrap().ends_with("\n;\nSELECT CURRENT_DATABASE(), CURRENT_SCHEMA(), CURRENT_WAREHOUSE(), CURRENT_ROLE()\n;\nSHOW VARIABLES"));
    assert_eq!(last["database"], "DB1");

    // The next run starts where the last one left the session. A script
    // that can't change the context goes without the context query, and a
    // trailing `--` comment doesn't swallow the `;` after it.
    let mut out = QueryOutcome::default();
    s.execute("SELECT 2 -- note", 100, &mut out).await.unwrap();
    assert_eq!(out.results.len(), 1);
    assert_eq!(out.database, None);
    let last = fake.posts.lock().unwrap().last().cloned().unwrap();
    assert_eq!((&last["database"], &last["schema"], &last["warehouse"], &last["role"]), (&json!("DB2"), &json!("S2"), &json!("WH"), &json!("R")));
    assert_eq!(last["statement"], "ALTER SESSION SET TIMEZONE = 'UTC'\n;\nSET V = 5\n;\nSELECT 2 -- note");

    // Users and permissions / Backups: a fresh session, nothing carried in
    // or out; the script goes alone.
    let mut fresh = d.connect(&cfg, None).await.unwrap();
    let mut out = QueryOutcome::default();
    fresh.execute("CREATE ROLE R1;\nGRANT ROLE R1 TO USER U1; -- done", 100, &mut out).await.unwrap();
    assert_eq!(out.results.len(), 2);
    let last = fake.posts.lock().unwrap().last().cloned().unwrap();
    assert_eq!(last["statement"], "CREATE ROLE R1\n;\nGRANT ROLE R1 TO USER U1");
    assert_eq!(last["database"], "DB1");

    // A refused script: the server's code, SQLSTATE and the line.
    let mut out = QueryOutcome::default();
    let err = s.execute("-- x\nselect 1\nfron t", 100, &mut out).await.unwrap_err();
    let Error::Statement(e) = err else { panic!("{err:?}") };
    assert_eq!((e.code.as_deref(), e.sqlstate.as_deref(), e.line), (Some("001003"), Some("42000"), Some(3)));
    assert_eq!(e.offset, Some("-- x\nselect 1\n".len()));
}
