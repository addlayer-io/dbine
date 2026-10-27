//! The Ollama and LM Studio clients against a small in-process imitation of
//! each server's HTTP API: model lists, streamed chats (NDJSON and
//! server-sent events), model pulls, error answers and servers that are down.

use dbine_ai::{
    chat, detect, ollama, openai, AiError, Cancel, ChatMessage, ChatRequest, Delta, Endpoints,
    ProviderKind,
};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A request as the fake server saw it.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    body: serde_json::Value,
}

/// `(status, body)` for a request; the body is sent in `chunks` pieces so
/// lines split across reads are exercised too.
type Route = Arc<dyn Fn(&str, &str) -> (u16, String) + Send + Sync>;

struct Server {
    base: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

async fn serve(route: Route) -> Server {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else {
                return;
            };
            let route = route.clone();
            let log = log.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let head_end = loop {
                    let n = s.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut first = head.lines().next().unwrap_or("").split(' ');
                let method = first.next().unwrap_or("").to_string();
                let path = first.next().unwrap_or("").to_string();
                let len: usize = head
                    .lines()
                    .filter_map(|l| l.split_once(": "))
                    .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, v)| v.trim().parse().ok())
                    .unwrap_or(0);
                let mut body = buf[head_end..].to_vec();
                while body.len() < len {
                    let n = s.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    body.extend_from_slice(&tmp[..n]);
                }
                log.lock().unwrap().push(Seen {
                    method: method.clone(),
                    path: path.clone(),
                    body: serde_json::from_slice(&body).unwrap_or_default(),
                });
                let (status, out) = route(&method, &path);
                let head = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    out.len()
                );
                let _ = s.write_all(head.as_bytes()).await;
                // In small pieces: a line may arrive split in two.
                for piece in out.as_bytes().chunks(7) {
                    let _ = s.write_all(piece).await;
                    let _ = s.flush().await;
                }
                let _ = s.shutdown().await;
            });
        }
    });
    Server { base, seen }
}

/// A port nothing listens on.
async fn down() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    drop(l);
    base
}

fn request(kind: ProviderKind, model: Option<&str>) -> ChatRequest {
    ChatRequest {
        kind,
        model: model.map(str::to_string),
        system: "Sos un asistente de SQL.".into(),
        messages: vec![
            ChatMessage {
                role: "user".into(),
                content: "hola".into(),
            },
            ChatMessage {
                role: "assistant".into(),
                content: "¿qué necesitás?".into(),
            },
            ChatMessage {
                role: "user".into(),
                content: "contá los clientes".into(),
            },
        ],
        lang: "es",
    }
}

/// Collects the deltas a chat streams.
#[derive(Default, Clone)]
struct Got(Arc<Mutex<Vec<Delta>>>);

impl Got {
    fn all(&self) -> Vec<Delta> {
        self.0.lock().unwrap().clone()
    }
}

fn not_available(r: &Result<impl std::fmt::Debug, AiError>) -> bool {
    matches!(r, Err(AiError::NotAvailable(_)))
}

fn provider_msg<T>(r: Result<T, AiError>) -> String {
    match r {
        Err(AiError::Provider(m)) => m,
        Err(e) => panic!("expected a provider error, got {e:?}"),
        Ok(_) => panic!("expected a provider error"),
    }
}

// ---- Ollama -------------------------------------------------------------------

#[tokio::test]
async fn ollama_models_put_code_models_first() {
    let srv = serve(Arc::new(|_, path| {
        assert_eq!(path, "/api/tags");
        (
            200,
            r#"{"models":[
                {"name":"llama3:8b","size":4700000000,"details":{"parameter_size":"8B"}},
                {"name":"qwen2.5-coder:7b","size":4200000000,"details":{"parameter_size":"7B"}},
                {"name":"mistral"},
                {"name":"sqlcoder:15b","details":{}}
            ]}"#
            .into(),
        )
    }))
    .await;
    let models = ollama::models(&srv.base).await.unwrap();
    let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        ["qwen2.5-coder:7b", "sqlcoder:15b", "llama3:8b", "mistral"]
    );
    assert_eq!(models[0].detail.as_deref(), Some("7B · 4.2 GB"));
    // No parameter size, no detail.
    assert_eq!(models[1].detail, None);
    assert_eq!(models[3].detail, None);
}

#[tokio::test]
async fn ollama_models_errors() {
    assert!(not_available(&ollama::models(&down().await).await));
    let srv = serve(Arc::new(|_, _| (200, "not json".into()))).await;
    let msg = provider_msg(ollama::models(&srv.base).await);
    assert!(msg.starts_with("Ollama"), "{msg}");
    // No `models` field: nothing installed.
    let srv = serve(Arc::new(|_, _| (200, "{}".into()))).await;
    assert!(ollama::models(&srv.base).await.unwrap().is_empty());
}

#[tokio::test]
async fn ollama_chat_streams_until_done() {
    let srv = serve(Arc::new(|_, path| {
        assert_eq!(path, "/api/chat");
        (
            200,
            [
                r#"{"message":{"thinking":"pienso"},"done":false}"#,
                "garbage that isn't json",
                r#"{"message":{"content":"SELECT count(*)"},"done":false}"#,
                r#"{"message":{"content":" FROM clientes"},"done":true}"#,
                r#"{"message":{"content":"never read"},"done":false}"#,
                "",
            ]
            .join("\n"),
        )
    }))
    .await;
    let got = Got::default();
    let g = got.clone();
    let cb = move |d: Delta| g.0.lock().unwrap().push(d);
    let out = ollama::chat(&srv.base, &request(ProviderKind::Ollama, Some("m")), &cb)
        .await
        .unwrap();
    assert_eq!(out, "SELECT count(*) FROM clientes");
    assert_eq!(
        got.all(),
        vec![
            Delta::Thinking("pienso".into()),
            Delta::Text("SELECT count(*)".into()),
            Delta::Text(" FROM clientes".into()),
        ]
    );
    // What was sent: the system prompt first, the whole conversation, a
    // context window that fits it.
    let seen = srv.seen.lock().unwrap()[0].clone();
    assert_eq!(
        (seen.method.as_str(), seen.path.as_str()),
        ("POST", "/api/chat")
    );
    assert_eq!(seen.body["model"], "m");
    assert_eq!(seen.body["stream"], true);
    let msgs = seen.body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[0]["content"], "Sos un asistente de SQL.");
    assert_eq!(msgs[3]["content"], "contá los clientes");
    assert_eq!(seen.body["options"]["num_ctx"], 8192);
}

#[tokio::test]
async fn ollama_chat_last_line_without_newline() {
    let srv = serve(Arc::new(|_, _| {
        (
            200,
            "{\"message\":{\"content\":\"SELECT\"}}\n{\"message\":{\"content\":\" 1\"}}".into(),
        )
    }))
    .await;
    let cb = |_: Delta| {};
    let out = ollama::chat(&srv.base, &request(ProviderKind::Ollama, Some("m")), &cb)
        .await
        .unwrap();
    assert_eq!(out, "SELECT 1");
}

#[tokio::test]
async fn ollama_chat_errors() {
    let cb = |_: Delta| {};
    // No model chosen: nothing is sent.
    let srv = serve(Arc::new(|_, _| (200, String::new()))).await;
    assert!(not_available(
        &ollama::chat(&srv.base, &request(ProviderKind::Ollama, Some("")), &cb).await
    ));
    assert!(not_available(
        &ollama::chat(&srv.base, &request(ProviderKind::Ollama, None), &cb).await
    ));
    assert!(srv.seen.lock().unwrap().is_empty());
    // Down.
    assert!(not_available(
        &ollama::chat(
            &down().await,
            &request(ProviderKind::Ollama, Some("m")),
            &cb
        )
        .await
    ));
    // An error status with Ollama's message, and one without a body.
    let srv = serve(Arc::new(|_, _| {
        (404, r#"{"error":"model 'm' not found"}"#.into())
    }))
    .await;
    let msg =
        provider_msg(ollama::chat(&srv.base, &request(ProviderKind::Ollama, Some("m")), &cb).await);
    assert_eq!(msg, "Ollama: model 'm' not found");
    let srv = serve(Arc::new(|_, _| (500, String::new()))).await;
    let msg =
        provider_msg(ollama::chat(&srv.base, &request(ProviderKind::Ollama, Some("m")), &cb).await);
    assert_eq!(msg, "Ollama: error desconocido");
    // An error in the middle of the stream.
    let srv = serve(Arc::new(|_, _| {
        (
            200,
            "{\"message\":{\"content\":\"SEL\"}}\n{\"error\":\"out of memory\"}\n".into(),
        )
    }))
    .await;
    let msg =
        provider_msg(ollama::chat(&srv.base, &request(ProviderKind::Ollama, Some("m")), &cb).await);
    assert_eq!(msg, "Ollama: out of memory");
}

#[tokio::test]
async fn ollama_pull_reports_progress() {
    let srv = serve(Arc::new(|_, path| {
        assert_eq!(path, "/api/pull");
        (
            200,
            [
                r#"{"status":"pulling manifest"}"#,
                "not json",
                r#"{"status":"downloading","completed":50,"total":100}"#,
                r#"{"status":"success"}"#,
                r#"{"status":"after success"}"#,
                "",
            ]
            .join("\n"),
        )
    }))
    .await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let s = seen.clone();
    let progress = move |st: &str, done: u64, total: u64| {
        s.lock().unwrap().push((st.to_string(), done, total));
    };
    ollama::pull(&srv.base, "qwen2.5-coder:7b", &progress)
        .await
        .unwrap();
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            ("pulling manifest".to_string(), 0, 0),
            ("downloading".to_string(), 50, 100),
            ("success".to_string(), 0, 0),
        ]
    );
    let body = srv.seen.lock().unwrap()[0].body.clone();
    assert_eq!(body["model"], "qwen2.5-coder:7b");
}

#[tokio::test]
async fn ollama_pull_errors() {
    let progress = |_: &str, _: u64, _: u64| {};
    assert!(not_available(
        &ollama::pull(&down().await, "m", &progress).await
    ));
    let srv = serve(Arc::new(|_, _| {
        (
            200,
            "{\"error\":\"pull model manifest: file does not exist\"}\n".into(),
        )
    }))
    .await;
    let msg = provider_msg(ollama::pull(&srv.base, "nope", &progress).await);
    assert!(msg.contains("file does not exist"), "{msg}");
    // A stream that ends without `success` isn't an error.
    let srv = serve(Arc::new(|_, _| (200, "{\"status\":\"x\"}\n".into()))).await;
    ollama::pull(&srv.base, "m", &progress).await.unwrap();
}

#[tokio::test]
async fn ollama_start_when_already_running() {
    let srv = serve(Arc::new(|_, _| (200, r#"{"models":[]}"#.into()))).await;
    ollama::start(&srv.base).await.unwrap();
    assert_eq!(srv.seen.lock().unwrap().len(), 1);
}

// ---- LM Studio (OpenAI-compatible) ------------------------------------------------

#[tokio::test]
async fn lmstudio_models_skip_embeddings() {
    let srv = serve(Arc::new(|_, path| {
        assert_eq!(path, "/v1/models");
        (
            200,
            r#"{"data":[{"id":"qwen2.5-7b-instruct"},{"id":"text-embedding-nomic"},{"object":"model"}]}"#
                .into(),
        )
    }))
    .await;
    let models = openai::models(&srv.base).await.unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "qwen2.5-7b-instruct");
    assert_eq!(models[0].detail, None);
    // No `data`: no models.
    let srv = serve(Arc::new(|_, _| (200, "{}".into()))).await;
    assert!(openai::models(&srv.base).await.unwrap().is_empty());
    let srv = serve(Arc::new(|_, _| (200, "<html>".into()))).await;
    assert!(provider_msg(openai::models(&srv.base).await).starts_with("LM Studio"));
    assert!(not_available(&openai::models(&down().await).await));
}

#[tokio::test]
async fn lmstudio_chat_streams_server_sent_events() {
    let srv = serve(Arc::new(|_, path| {
        assert_eq!(path, "/v1/chat/completions");
        (
            200,
            [
                ": keep-alive",
                r#"data: {"choices":[{"delta":{"reasoning_content":"veamos"}}]}"#,
                "data: not json",
                r#"data: {"choices":[{"delta":{"content":"SELECT"}}]}"#,
                r#"data: {"choices":[{"delta":{"content":""}}]}"#,
                r#"data: {"choices":[{"delta":{"content":" 1"}}]}"#,
                "data: [DONE]",
                r#"data: {"choices":[{"delta":{"content":"ignored"}}]}"#,
                "",
            ]
            .join("\n"),
        )
    }))
    .await;
    let got = Got::default();
    let g = got.clone();
    let cb = move |d: Delta| g.0.lock().unwrap().push(d);
    let out = openai::chat(
        &srv.base,
        &request(ProviderKind::LmStudio, Some("qwen")),
        &cb,
    )
    .await
    .unwrap();
    assert_eq!(out, "SELECT 1");
    assert_eq!(
        got.all(),
        vec![
            Delta::Thinking("veamos".into()),
            Delta::Text("SELECT".into()),
            Delta::Text(" 1".into()),
        ]
    );
    let body = srv.seen.lock().unwrap()[0].body.clone();
    assert_eq!(body["model"], "qwen");
    assert_eq!(body["stream"], true);
    assert_eq!(body["messages"].as_array().unwrap().len(), 4);
    assert_eq!(body["messages"][0]["role"], "system");
    assert_eq!(body["messages"][2]["role"], "assistant");
}

#[tokio::test]
async fn lmstudio_chat_errors() {
    let cb = |_: Delta| {};
    assert!(not_available(
        &openai::chat("http://unused", &request(ProviderKind::LmStudio, None), &cb).await
    ));
    assert!(not_available(
        &openai::chat(
            &down().await,
            &request(ProviderKind::LmStudio, Some("m")),
            &cb
        )
        .await
    ));
    // OpenAI's error shape, and a plain `error` string.
    let srv = serve(Arc::new(|_, _| {
        (400, r#"{"error":{"message":"No models loaded"}}"#.into())
    }))
    .await;
    let msg = provider_msg(
        openai::chat(&srv.base, &request(ProviderKind::LmStudio, Some("m")), &cb).await,
    );
    assert_eq!(msg, "LM Studio: \"No models loaded\"");
    let srv = serve(Arc::new(|_, _| (500, r#"{"error":"boom"}"#.into()))).await;
    let msg = provider_msg(
        openai::chat(&srv.base, &request(ProviderKind::LmStudio, Some("m")), &cb).await,
    );
    assert_eq!(msg, "LM Studio: \"boom\"");
    // A stream that ends without [DONE] keeps what arrived.
    let srv = serve(Arc::new(|_, _| {
        (
            200,
            "data: {\"choices\":[{\"delta\":{\"content\":\"SEL\"}}]}\n".into(),
        )
    }))
    .await;
    let out = openai::chat(&srv.base, &request(ProviderKind::LmStudio, Some("m")), &cb)
        .await
        .unwrap();
    assert_eq!(out, "SEL");
}

// ---- Dispatch and detection -----------------------------------------------------

fn endpoints(ollama: &str, lmstudio: &str, models_dir: &std::path::Path) -> Endpoints {
    Endpoints {
        ollama: ollama.into(),
        lmstudio: lmstudio.into(),
        models_dir: models_dir.to_path_buf(),
    }
}

#[tokio::test]
async fn chat_goes_to_the_chosen_local_server() {
    let ollama_srv = serve(Arc::new(|_, _| {
        (
            200,
            "{\"message\":{\"content\":\"de ollama\"},\"done\":true}\n".into(),
        )
    }))
    .await;
    let lms_srv = serve(Arc::new(|_, _| {
        (
            200,
            "data: {\"choices\":[{\"delta\":{\"content\":\"de lm studio\"}}]}\ndata: [DONE]\n"
                .into(),
        )
    }))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let ep = endpoints(&ollama_srv.base, &lms_srv.base, dir.path());
    let cb = |_: Delta| {};
    let cancel = Cancel::new();
    assert_eq!(
        chat(&request(ProviderKind::Ollama, Some("m")), &ep, &cb, &cancel)
            .await
            .unwrap(),
        "de ollama"
    );
    assert_eq!(
        chat(
            &request(ProviderKind::LmStudio, Some("m")),
            &ep,
            &cb,
            &cancel
        )
        .await
        .unwrap(),
        "de lm studio"
    );
}

#[tokio::test]
async fn a_cancelled_chat_stops_waiting() {
    // A server that accepts and never answers.
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = l.accept().await {
            held.push(s);
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let ep = endpoints(&base, &base, dir.path());
    let cancel = Cancel::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        c.cancel();
    });
    let cb = |_: Delta| {};
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        chat(&request(ProviderKind::Ollama, Some("m")), &ep, &cb, &cancel),
    )
    .await
    .expect("the cancel ends the chat");
    assert!(matches!(r, Err(AiError::Cancelled)));
    assert!(cancel.is_cancelled());
}

#[tokio::test]
async fn detect_reports_what_each_local_server_has() {
    let ollama_srv = serve(Arc::new(|_, _| {
        (
            200,
            r#"{"models":[{"name":"llama3"},{"name":"codellama"}]}"#.into(),
        )
    }))
    .await;
    let lms_srv = serve(Arc::new(|_, _| (200, r#"{"data":[{"id":"m1"}]}"#.into()))).await;
    let dir = tempfile::tempdir().unwrap();
    let all = detect(&endpoints(&ollama_srv.base, &lms_srv.base, dir.path())).await;
    let kinds: Vec<ProviderKind> = all.iter().map(|p| p.kind).collect();
    assert_eq!(
        kinds,
        [
            ProviderKind::Embedded,
            ProviderKind::Ollama,
            ProviderKind::ClaudeCode,
            ProviderKind::Codex,
            ProviderKind::LmStudio
        ]
    );
    let ollama = &all[1];
    assert!(ollama.installed && ollama.available && ollama.local);
    assert_eq!(ollama.models[0].id, "codellama");
    assert!(
        ollama.status.starts_with("2 modelo(s)"),
        "{}",
        ollama.status
    );
    let lms = &all[4];
    assert!(lms.installed && lms.available);
    assert_eq!(lms.models.len(), 1);
    // The CLIs send to the user's account: never local.
    assert!(!all[2].local && !all[3].local);
    assert_eq!(all[2].models.len(), 3);
    assert_eq!(all[2].available, all[2].installed);
    // The built-in model: nothing downloaded to this folder.
    let embedded = &all[0];
    assert!(!embedded.available);
    assert!(embedded.models.is_empty());
    if embedded.installed {
        assert_eq!(embedded.action, Some("download_model"));
    }
}

#[tokio::test]
async fn detect_running_servers_without_models() {
    let ollama_srv = serve(Arc::new(|_, _| (200, r#"{"models":[]}"#.into()))).await;
    let lms_srv = serve(Arc::new(|_, _| (200, r#"{"data":[]}"#.into()))).await;
    let dir = tempfile::tempdir().unwrap();
    let all = detect(&endpoints(&ollama_srv.base, &lms_srv.base, dir.path())).await;
    let ollama = &all[1];
    assert!(ollama.installed && !ollama.available);
    assert_eq!(ollama.action, Some("pull_model"));
    let lms = &all[4];
    assert!(lms.installed && !lms.available);
    assert!(lms.status.contains("no hay modelos"), "{}", lms.status);
}

#[tokio::test]
async fn detect_servers_that_are_down() {
    let gone = down().await;
    let dir = tempfile::tempdir().unwrap();
    let all = detect(&endpoints(&gone, &gone, dir.path())).await;
    for p in [&all[1], &all[4]] {
        assert!(!p.available);
        assert!(p.models.is_empty());
        // Whether the app is installed depends on the machine; the status
        // says which case it is.
        if p.installed {
            assert!(p.status.starts_with("instalado"), "{}", p.status);
        } else {
            assert_eq!(p.status, "no instalado");
            assert_eq!(p.action, None);
        }
    }
}
