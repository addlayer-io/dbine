//! Anthropic's Messages API with the user's API key (docs/ai-assistant.md):
//! `POST /v1/messages`, streamed as server-sent events, and `GET /v1/models`.
//! Raw HTTP: Rust has no official Anthropic SDK.
//!
//! - Headers: `x-api-key` and `anthropic-version` ([`VERSION`]).
//! - No `temperature`, `top_p` or `top_k`: on this API version they are a 400.
//! - No `thinking`: the current models think adaptively by default; what they
//!   show of it arrives as thinking deltas.
//! - The models that support it fall back to another model on a safety
//!   refusal (`fallbacks: "default"`, beta [`FALLBACK_BETA`]) instead of
//!   ending the answer; a refusal that stands is an error that says so.
//! - Room for a long answer ([`MAX_TOKENS`], streamed); an older model whose
//!   output cap is lower gets the request again with [`SMALL_MAX_TOKENS`].

use crate::api::{self, ApiAccess};
use crate::detect::ModelInfo;
use crate::{AiError, ChatRequest, Delta, OnDelta, ProviderKind, Result};
use futures::StreamExt;
use serde_json::{json, Value};
use std::time::Duration;

pub const VERSION: &str = "2026-10-21";
/// The model asked when none is chosen.
pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
pub const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
/// The models that take `fallbacks: "default"`.
const FALLBACK_MODELS: [&str; 5] = [
    "claude-fable-5-2",
    "claude-fable-5-1",
    "claude-opus-5-5",
    "claude-opus-5",
    "claude-sonnet-5-5",
];
const MAX_TOKENS: u32 = 64_000;
const SMALL_MAX_TOKENS: u32 = 8_192;

fn falls_back(model: &str) -> bool {
    FALLBACK_MODELS.contains(&model)
}

/// The request's body: the system prompt, the conversation (empty turns
/// left out) and nothing that samples.
pub(crate) fn body(model: &str, req: &ChatRequest, max_tokens: u32) -> Value {
    let messages: Vec<Value> = req
        .messages
        .iter()
        .filter(|m| !m.content.trim().is_empty())
        .map(|m| {
            let role = if m.role == "assistant" {
                "assistant"
            } else {
                "user"
            };
            json!({ "role": role, "content": m.content })
        })
        .collect();
    let mut b = json!({
        "model": model,
        "max_tokens": max_tokens,
        "stream": true,
        "messages": messages,
    });
    if !req.system.trim().is_empty() {
        b["system"] = json!(req.system);
    }
    if falls_back(model) {
        b["fallbacks"] = json!("default");
    }
    b
}

fn request(access: &ApiAccess, path: &str, model: Option<&str>) -> reqwest::RequestBuilder {
    let c = api::client();
    let url = format!("{}{path}", access.base);
    let mut rb = if model.is_some() {
        c.post(url)
    } else {
        c.get(url)
    }
    .header("anthropic-version", VERSION);
    if let Some(k) = access.key.as_deref() {
        rb = rb.header("x-api-key", k);
    }
    if model.is_some_and(falls_back) {
        rb = rb.header("anthropic-beta", FALLBACK_BETA);
    }
    rb
}

pub async fn chat(
    access: &ApiAccess,
    model: &str,
    req: &ChatRequest,
    on_delta: OnDelta<'_>,
) -> Result<String> {
    let key = access.key.as_deref();
    let kind = ProviderKind::Anthropic;
    let mut max = MAX_TOKENS;
    loop {
        let resp = request(access, "/v1/messages", Some(model))
            .json(&body(model, req, max))
            .send()
            .await
            .map_err(|e| api::unreachable(kind, &access.base, &e, key))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let text = resp.text().await.unwrap_or_default();
            if status == 400 && max == MAX_TOKENS && text.contains("max_tokens") {
                max = SMALL_MAX_TOKENS;
                continue;
            }
            return Err(api::failure(kind, status, &text, key));
        }
        return relay(resp, on_delta).await.map_err(|e| match e {
            AiError::Provider(m) => AiError::Provider(api::redact(&m, key)),
            other => other,
        });
    }
}

/// What the stream said besides the text.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Ending {
    pub stop: Option<String>,
    pub why: Option<String>,
    pub error: Option<String>,
}

/// One event's data: text and thinking go to `on_delta`, the stop reason
/// and an error to `end`.
pub(crate) fn event(data: &str, out: &mut String, end: &mut Ending, on_delta: OnDelta<'_>) {
    let Ok(v) = serde_json::from_str::<Value>(data) else {
        return;
    };
    let s = |p: &str| v.pointer(p).and_then(Value::as_str);
    match s("/type") {
        Some("content_block_delta") => match s("/delta/type") {
            Some("text_delta") => {
                if let Some(t) = s("/delta/text").filter(|t| !t.is_empty()) {
                    out.push_str(t);
                    on_delta(Delta::Text(t.into()));
                }
            }
            Some("thinking_delta") => {
                if let Some(t) = s("/delta/thinking").filter(|t| !t.is_empty()) {
                    on_delta(Delta::Thinking(t.into()));
                }
            }
            _ => {}
        },
        Some("message_delta") => {
            if let Some(r) = s("/delta/stop_reason") {
                end.stop = Some(r.into());
            }
            if let Some(w) = s("/delta/stop_details/explanation")
                .or_else(|| s("/stop_details/explanation"))
                .or_else(|| s("/delta/stop_details/category"))
            {
                end.why = Some(w.into());
            }
        }
        Some("error") => end.error = Some(s("/error/message").unwrap_or("error").into()),
        _ => {}
    }
}

/// The answer, or why there isn't one: an error event, or a refusal that
/// no fallback rescued.
pub(crate) fn finish(out: String, end: Ending) -> Result<String> {
    if let Some(e) = end.error {
        return Err(AiError::Provider(format!("Anthropic: {e}")));
    }
    if end.stop.as_deref() == Some("refusal") {
        return Err(AiError::Provider(match end.why {
            Some(w) => format!("Anthropic: el modelo declinó responder esta solicitud ({w})"),
            None => "Anthropic: el modelo declinó responder esta solicitud".into(),
        }));
    }
    Ok(out)
}

async fn relay(resp: reqwest::Response, on_delta: OnDelta<'_>) -> Result<String> {
    let mut out = String::new();
    let mut end = Ending::default();
    let mut buf = Vec::<u8>::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(
            &chunk.map_err(|e| {
                AiError::Provider(format!("Anthropic: se cortó la respuesta ({e})"))
            })?,
        );
        while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let line = String::from_utf8_lossy(&line);
            if let Some(data) = line.trim().strip_prefix("data:") {
                event(data.trim(), &mut out, &mut end, on_delta);
            }
        }
    }
    finish(out, end)
}

/// The models the key may use, newest first, with [`DEFAULT_MODEL`] at the top.
pub async fn models(access: &ApiAccess) -> Result<Vec<ModelInfo>> {
    let key = access.key.as_deref();
    let kind = ProviderKind::Anthropic;
    let resp = request(access, "/v1/models?limit=100", None)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| api::unreachable(kind, &access.base, &e, key))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(api::failure(kind, status, &text, key));
    }
    Ok(parse_models(&text))
}

pub(crate) fn parse_models(body: &str) -> Vec<ModelInfo> {
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    let mut list: Vec<ModelInfo> = v
        .get("data")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    Some(ModelInfo {
                        id: m.get("id")?.as_str()?.to_string(),
                        detail: m
                            .get("display_name")
                            .and_then(Value::as_str)
                            .map(str::to_string),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    if let Some(i) = list.iter().position(|m| m.id == DEFAULT_MODEL) {
        let m = list.remove(i);
        list.insert(0, m);
    }
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_server::serve;
    use crate::ChatMessage;

    const KEY: &str = "sk-ant-test-0123456789";

    fn req() -> ChatRequest {
        ChatRequest {
            kind: ProviderKind::Anthropic,
            model: None,
            system: "You write SQL.".into(),
            messages: vec![
                ChatMessage {
                    role: "user".into(),
                    content: "count the customers".into(),
                },
                ChatMessage {
                    role: "assistant".into(),
                    content: "  ".into(),
                },
            ],
            lang: "en",
        }
    }

    fn sse(events: &[&str]) -> String {
        events
            .iter()
            .map(|e| {
                let v: Value = serde_json::from_str(e).unwrap();
                format!("event: {}\ndata: {e}\n\n", v["type"].as_str().unwrap())
            })
            .collect()
    }

    fn answer() -> String {
        sse(&[
            r#"{"type":"message_start","message":{"id":"msg_1","type":"message"}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":""}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"SELECT "}}"#,
            r#"{"type":"ping"}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"count(*)"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":4}}"#,
            r#"{"type":"message_stop"}"#,
        ])
    }

    fn access(base: String) -> ApiAccess {
        ApiAccess {
            base,
            key: Some(KEY.into()),
            model: None,
        }
    }

    #[tokio::test]
    async fn messages_go_with_its_headers_and_without_sampling() {
        let (base, seen) = serve(vec![(200, "text/event-stream", answer())]).await;
        let got = std::sync::Mutex::new(Vec::new());
        let on = |d: Delta| got.lock().unwrap().push(d);
        let out = chat(&access(base), DEFAULT_MODEL, &req(), &on)
            .await
            .unwrap();
        assert_eq!(out, "SELECT count(*)");
        assert_eq!(
            *got.lock().unwrap(),
            [
                Delta::Text("SELECT ".into()),
                Delta::Text("count(*)".into())
            ],
            "an empty thinking delta shows nothing"
        );
        let r = &seen.lock().unwrap()[0];
        assert_eq!(
            (r.method.as_str(), r.path.as_str()),
            ("POST", "/v1/messages")
        );
        assert_eq!(r.header("x-api-key"), Some(KEY));
        assert_eq!(r.header("anthropic-version"), Some(VERSION));
        assert_eq!(r.header("authorization"), None);
        let b: Value = serde_json::from_str(&r.body).unwrap();
        for f in ["temperature", "top_p", "top_k", "thinking"] {
            assert!(b.get(f).is_none(), "{f} must not be sent: {b}");
        }
        assert_eq!(b["stream"], true);
        assert_eq!(b["max_tokens"], MAX_TOKENS);
        assert_eq!(b["system"], "You write SQL.");
        assert_eq!(
            b["messages"].as_array().unwrap().len(),
            1,
            "the empty turn is left out"
        );
    }

    #[tokio::test]
    async fn fallbacks_only_on_the_models_that_take_them() {
        for (model, falls) in [
            ("claude-opus-5-5", true),
            ("claude-fable-5-2", true),
            ("claude-fable-5-1", true),
            ("claude-opus-5", true),
            ("claude-sonnet-5-5", true),
            ("claude-haiku-4-5", false),
            ("claude-opus-4-8", false),
        ] {
            let (base, seen) = serve(vec![(200, "text/event-stream", answer())]).await;
            chat(&access(base), model, &req(), &|_| {}).await.unwrap();
            let r = &seen.lock().unwrap()[0];
            let b: Value = serde_json::from_str(&r.body).unwrap();
            assert_eq!(b.get("fallbacks").is_some(), falls, "{model}");
            assert_eq!(r.header("anthropic-beta").is_some(), falls, "{model}");
            if falls {
                assert_eq!(b["fallbacks"], "default");
                assert_eq!(r.header("anthropic-beta"), Some(FALLBACK_BETA));
            }
        }
    }

    #[tokio::test]
    async fn an_error_event_and_a_refusal_are_errors() {
        let failed = sse(&[
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"SEL"}}"#,
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
        ]);
        let refused = sse(&[
            r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","explanation":"policy"}}}"#,
            r#"{"type":"message_stop"}"#,
        ]);
        let (base, _) = serve(vec![
            (200, "text/event-stream", failed),
            (200, "text/event-stream", refused),
        ])
        .await;
        let a = access(base);
        let e = chat(&a, DEFAULT_MODEL, &req(), &|_| {})
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(e, "Anthropic: Overloaded");
        let e = chat(&a, DEFAULT_MODEL, &req(), &|_| {})
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("declinó responder") && e.contains("policy"),
            "{e}"
        );
    }

    #[tokio::test]
    async fn a_lower_output_cap_gets_the_request_again() {
        let (base, seen) = serve(vec![
            (
                400,
                "application/json",
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: 64000 > 8192, which is the maximum allowed"}}"#.into(),
            ),
            (200, "text/event-stream", answer()),
        ])
        .await;
        let out = chat(&access(base), "claude-3-5-haiku-latest", &req(), &|_| {})
            .await
            .unwrap();
        assert_eq!(out, "SELECT count(*)");
        let s = seen.lock().unwrap();
        let b: Value = serde_json::from_str(&s[1].body).unwrap();
        assert_eq!(b["max_tokens"], SMALL_MAX_TOKENS);
    }

    #[tokio::test]
    async fn a_refused_key_never_shows() {
        let body = format!(
            r#"{{"type":"error","error":{{"type":"authentication_error","message":"invalid x-api-key {KEY}"}}}}"#
        );
        let (base, _) = serve(vec![
            (401, "application/json", body.clone()),
            (401, "application/json", body),
        ])
        .await;
        let a = access(base);
        for e in [
            chat(&a, DEFAULT_MODEL, &req(), &|_| {})
                .await
                .unwrap_err()
                .to_string(),
            models(&a).await.unwrap_err().to_string(),
        ] {
            assert!(e.contains("no es válida") && !e.contains(KEY), "{e}");
        }
    }

    #[tokio::test]
    async fn models_put_the_default_first() {
        let (base, seen) = serve(vec![(
            200,
            "application/json",
            r#"{"data":[{"id":"claude-fable-5-2","display_name":"Claude Fable 5.2"},{"id":"claude-opus-5-5","display_name":"Claude Opus 5.5"},{"id":"claude-haiku-5-5","display_name":"Claude Haiku 5.5"}]}"#.into(),
        )])
        .await;
        let m = models(&access(base)).await.unwrap();
        let ids: Vec<&str> = m.iter().map(|x| x.id.as_str()).collect();
        assert_eq!(
            ids,
            ["claude-opus-5-5", "claude-fable-5-2", "claude-haiku-5-5"]
        );
        assert_eq!(m[0].detail.as_deref(), Some("Claude Opus 5.5"));
        let r = &seen.lock().unwrap()[0];
        assert_eq!(
            (r.method.as_str(), r.path.as_str()),
            ("GET", "/v1/models?limit=100")
        );
        assert_eq!(r.header("x-api-key"), Some(KEY));
        assert_eq!(r.header("anthropic-version"), Some(VERSION));
    }
}
