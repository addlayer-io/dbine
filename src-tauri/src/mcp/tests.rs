//! The server end to end: a real HTTP listener on a free port, a temporary
//! SQLite database as the connection, and an HTTP client speaking MCP.

use super::*;
use dbine_core::StateStore;
use dbine_driver::ConnectionConfig;
use serde_json::{json, Value};

fn conn(level: Option<&str>, tags: &[&str], read_only: bool) -> SavedConnection {
    SavedConnection {
        id: "c".into(),
        name: "c".into(),
        color: None,
        config: ConnectionConfig { driver: "sqlite".into(), read_only, ..Default::default() },
        save_password: false,
        folder_id: None,
        tags: tags.iter().map(|t| t.to_string()).collect(),
        mcp_level: level.map(Into::into),
        updated_at: String::new(),
    }
}

#[test]
fn mcp_effective_level_resolution() {
    use McpLevel::*;
    // The default applies without an override; the override wins.
    assert_eq!(effective_level(&conn(None, &[], false), Schema).level, Schema);
    assert_eq!(effective_level(&conn(None, &[], false), Read).level, Read);
    assert_eq!(effective_level(&conn(Some("disabled"), &[], false), Read).level, Disabled);
    assert_eq!(effective_level(&conn(Some("read"), &[], false), Schema).level, Read);
    assert_eq!(effective_level(&conn(Some("garbage"), &[], false), Schema).level, Schema);
    // prod (any case) and read-only cap at read, and say why.
    let prod = effective_level(&conn(Some("write"), &["PROD"], false), Schema);
    assert_eq!(prod, Effective { level: Read, cap: Some(Cap::Prod) });
    let ro = effective_level(&conn(None, &[], true), Write);
    assert_eq!(ro, Effective { level: Read, cap: Some(Cap::ReadOnly) });
    // Below the cap nothing changes.
    assert_eq!(effective_level(&conn(Some("schema"), &["prod"], true), Read), Effective { level: Schema, cap: None });
    // Write is effective where no cap applies, from the override or the default.
    assert_eq!(effective_level(&conn(Some("write"), &[], false), Schema), Effective { level: Write, cap: None });
    assert_eq!(effective_level(&conn(None, &[], false), Write).level, Write);
    assert_eq!(effective_level(&conn(None, &["prod"], false), Write), Effective { level: Read, cap: Some(Cap::Prod) });
    assert!(check_override(Some("write")).is_ok() && check_level(Write).is_ok());
    assert!(check_override(Some("nope")).is_err());
    assert!(check_override(Some("read")).is_ok() && check_override(None).is_ok());
}

#[test]
fn mcp_token_hashing_and_verification() {
    let t = new_token();
    assert!(t.starts_with("dbine_") && t.len() == 6 + 64);
    assert_ne!(t, new_token());
    let h = hash_token(&t);
    assert_eq!(h.len(), 64);
    assert!(!h.contains(&t[6..]));
    assert!(verify_token(&t, &h));
    assert!(!verify_token(&new_token(), &h));
    assert!(!verify_token(&t, ""));
    // The known SHA-256 of "abc".
    assert_eq!(hash_token("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
}

struct Server {
    rt: McpRuntime,
    url: String,
    http: reqwest::Client,
    dir: std::path::PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn server() -> Server {
    let dir = std::env::temp_dir().join(format!("dbine-mcp-test-{}-{}", std::process::id(), chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)));
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("t.sqlite");
    {
        let c = rusqlite::Connection::open(&db).unwrap();
        c.execute_batch("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL); INSERT INTO people (name) VALUES ('ana'), ('beto');")
            .unwrap();
    }
    let state = AppState::new(StateStore::open_in_memory().unwrap());
    let mut schema = conn(None, &[], false);
    schema.config.host = db.to_string_lossy().into_owned();
    schema.id = "schema-conn".into();
    schema.name = "solo esquema".into();
    let mut read = schema.clone();
    read.id = "read-conn".into();
    read.name = "lectura".into();
    read.mcp_level = Some("read".into());
    let mut hidden = schema.clone();
    hidden.id = "hidden-conn".into();
    hidden.name = "oculta".into();
    hidden.mcp_level = Some("disabled".into());
    for c in [&schema, &read, &hidden] {
        state.store.save_connection(c).unwrap();
    }
    let rt = McpRuntime::with_log(state, activity::ActivityLog::in_memory());
    let running = rt.start(0, false).unwrap();
    let url = format!("http://127.0.0.1:{}/mcp", running.port);
    *rt.inner.server.lock().unwrap() = Some(running);
    Server { rt, url, http: reqwest::Client::new(), dir }
}

impl Server {
    async fn post(&self, token: &str, body: Value) -> reqwest::Response {
        self.http
            .post(&self.url)
            .bearer_auth(token)
            .header("accept", "application/json, text/event-stream")
            .json(&body)
            .send()
            .await
            .unwrap()
    }

    async fn rpc(&self, token: &str, method: &str, params: Value) -> Value {
        let r = self.post(token, json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })).await;
        assert_eq!(r.status(), 200);
        r.json().await.unwrap()
    }

    async fn call(&self, token: &str, tool: &str, args: Value) -> (String, bool) {
        let v = self.rpc(token, "tools/call", json!({ "name": tool, "arguments": args })).await;
        let r = &v["result"];
        (r["content"][0]["text"].as_str().unwrap_or_default().to_string(), r["isError"].as_bool().unwrap())
    }
}

#[tokio::test]
async fn mcp_server_end_to_end() {
    let s = server().await;
    let (_, token) = s.rt.inner.create_client("Claude Code").unwrap();

    // initialize → tools/list
    let init = s.rpc(&token, "initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "t", "version": "1" } })).await;
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert!(init["result"]["capabilities"]["tools"].is_object());
    let note = s.post(&token, json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await;
    assert_eq!(note.status(), 202);
    let tools = s.rpc(&token, "tools/list", json!({})).await;
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"run_query") && names.contains(&"describe_object"));

    // Only the visible connections, without paths.
    let (list, err) = s.call(&token, "list_connections", json!({})).await;
    assert!(!err, "{list}");
    assert!(list.contains("solo esquema") && list.contains("lectura") && !list.contains("oculta"), "{list}");
    assert!(!list.contains("t.sqlite"), "{list}");

    // Structure works at the schema level.
    let (objects, err) = s.call(&token, "list_objects", json!({ "connection": "solo esquema", "database": "" })).await;
    assert!(!err && objects.contains("people"), "{objects}");
    let (desc, err) = s.call(&token, "describe_object", json!({ "connection": "solo esquema", "database": "", "object": "people" })).await;
    assert!(!err && desc.contains("name") && desc.contains("NOT NULL"), "{desc}");

    // The schema level can't run queries.
    let (msg, err) = s.call(&token, "run_query", json!({ "connection": "solo esquema", "database": "", "query": "select * from people" })).await;
    assert!(err && msg.contains("lectura"), "{msg}");
    // A disabled connection isn't there at all.
    let (_, err) = s.call(&token, "list_objects", json!({ "connection": "oculta", "database": "" })).await;
    assert!(err);

    // The read level can, and writes are refused.
    let (rows, err) = s.call(&token, "run_query", json!({ "connection": "lectura", "database": "", "query": "select name from people order by id" })).await;
    assert!(!err && rows.contains("ana") && rows.contains("beto"), "{rows}");
    let (rows, err) = s.call(&token, "run_query", json!({ "connection": "lectura", "database": "", "query": "select name from people order by id", "max_rows": 1 })).await;
    assert!(!err && rows.contains("ana") && !rows.contains("beto"), "{rows}");
    let (msg, err) = s.call(&token, "run_query", json!({ "connection": "lectura", "database": "", "query": "delete from people" })).await;
    assert!(err && msg.contains("DELETE"), "{msg}");
    let (sample, err) = s.call(&token, "sample_rows", json!({ "connection": "lectura", "database": "", "object": "people", "limit": 5 })).await;
    assert!(!err && sample.contains("beto"), "{sample}");
    let (plan, err) = s.call(&token, "explain", json!({ "connection": "lectura", "database": "", "query": "select * from people where id = 1" })).await;
    assert!(!err && !plan.is_empty(), "{plan}");

    // Every call is in the log, with the client's name.
    let log = s.rt.inner.activity.list(Some("Claude Code"), Some("lectura"), 100).unwrap();
    assert!(log.iter().any(|e| e.tool == "run_query" && !e.ok));
    assert!(log.iter().any(|e| e.tool == "run_query" && e.ok && e.rows == Some(2)));

    // Wrong token → 401; revoked token → 401.
    let r = s.post("dbine_wrong", json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })).await;
    assert_eq!(r.status(), 401);
    let (c2, t2) = s.rt.inner.create_client("Codex").unwrap();
    assert_eq!(s.post(&t2, json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })).await.status(), 200);
    s.rt.inner.revoke_client(&c2.id).unwrap();
    assert_eq!(s.post(&t2, json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })).await.status(), 401);

    // A browser origin → 403, even with a good token.
    let r = s
        .http
        .post(&s.url)
        .bearer_auth(&token)
        .header("origin", "https://example.com")
        .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
}

/// Answer the next write request with `decision` once it's pending.
async fn answer_next(rt: &McpRuntime, decision: approvals::Decision) {
    for _ in 0..400 {
        if let Some(r) = rt.inner.approvals.pending().first() {
            rt.inner.approvals.answer(&r.id, decision).unwrap();
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no write request came");
}

#[tokio::test]
async fn mcp_write_needs_the_level_and_the_users_approval() {
    use approvals::Decision;
    let s = server().await;
    let (claude, token) = s.rt.inner.create_client("Claude Code").unwrap();
    let mut w = load_connection(&s, "read-conn");
    w.id = "write-conn".into();
    w.name = "escritura".into();
    w.mcp_level = Some("write".into());
    s.rt.inner.state.store.save_connection(&w).unwrap();
    let mut prod = w.clone();
    prod.id = "prod-conn".into();
    prod.name = "produccion".into();
    prod.tags = vec!["prod".into()];
    s.rt.inner.state.store.save_connection(&prod).unwrap();
    let args = |conn: &str, code: &str| json!({ "connection": conn, "database": "", "code": code });

    // Below write (and on prod, capped at read) it's refused without asking.
    let (msg, err) = s.call(&token, "execute", args("lectura", "delete from people")).await;
    assert!(err && msg.contains("escritura"), "{msg}");
    let (msg, err) = s.call(&token, "execute", args("produccion", "delete from people")).await;
    assert!(err && msg.contains("prod"), "{msg}");
    assert!(s.rt.inner.approvals.pending().is_empty());
    // The read tools keep refusing writes at the write level.
    let (msg, err) = s.call(&token, "run_query", json!({ "connection": "escritura", "database": "", "query": "delete from people" })).await;
    assert!(err && msg.contains("DELETE"), "{msg}");

    // Rejected: nothing runs.
    let (r, _) = tokio::join!(s.call(&token, "execute", args("escritura", "delete from people")), answer_next(&s.rt, Decision::Reject));
    assert!(r.1 && r.0.contains("Rechazado"), "{}", r.0);
    let (rows, _) = s.call(&token, "run_query", json!({ "connection": "escritura", "database": "", "query": "select count(*) as n from people" })).await;
    assert!(rows.contains("\n2\n"), "{rows}");

    // Approved: it runs and says how many rows changed.
    let (r, _) = tokio::join!(
        s.call(&token, "execute", args("escritura", "insert into people (name) values ('carla')")),
        answer_next(&s.rt, Decision::Approve)
    );
    assert!(!r.1 && r.0.contains("Aprobado") && r.0.contains("1 rows affected"), "{}", r.0);
    let req_seen = s.rt.inner.activity.list(Some("Claude Code"), Some("escritura"), 100).unwrap();
    assert!(req_seen.iter().any(|e| e.tool == "execute:request"));
    assert!(req_seen.iter().any(|e| e.tool == "execute:rejected" && !e.ok));
    assert!(req_seen.iter().any(|e| e.tool == "execute:approved" && e.ok && e.rows == Some(1)));

    // Approve all: the next one runs without asking, until it's removed.
    let (r, _) = tokio::join!(
        s.call(&token, "execute", args("escritura", "update people set name = upper(name)")),
        answer_next(&s.rt, Decision::ApproveAll)
    );
    assert!(!r.1, "{}", r.0);
    assert!(s.rt.inner.approvals.approves_all(&claude.id));
    let (r, err) = s.call(&token, "execute", args("escritura", "delete from people where name = 'CARLA'")).await;
    assert!(!err && r.contains("1 rows affected"), "{r}");
    assert!(s.rt.inner.activity.list(Some("Claude Code"), None, 100).unwrap().iter().any(|e| e.tool == "execute:auto_approved"));
    // Revoking the client ends approve-all.
    s.rt.inner.revoke_client(&claude.id).unwrap();
    assert!(!s.rt.inner.approvals.approves_all(&claude.id));
}

fn load_connection(s: &Server, id: &str) -> SavedConnection {
    s.rt.inner.state.store.list_connections().unwrap().into_iter().find(|c| c.id == id).unwrap()
}
