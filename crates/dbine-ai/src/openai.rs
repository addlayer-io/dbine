//! OpenAI-compatible local servers (LM Studio: `/v1/models`,
//! `/v1/chat/completions` with server-sent events).

use crate::detect::ModelInfo;
use crate::{AiError, ChatRequest, Delta, OnDelta, Result};
use futures::StreamExt;
use std::time::Duration;

fn client() -> reqwest::Client {
    reqwest::Client::builder().connect_timeout(Duration::from_secs(3)).build().unwrap_or_default()
}

pub async fn models(base: &str) -> Result<Vec<ModelInfo>> {
    let resp = client()
        .get(format!("{base}/v1/models"))
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .map_err(|e| AiError::NotAvailable(format!("LM Studio no responde en {base} ({e})")))?;
    let v: serde_json::Value = resp.json().await.map_err(|e| AiError::Provider(format!("LM Studio: {e}")))?;
    Ok(v.get("data")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("id").and_then(|i| i.as_str()))
                // Embedding models can't chat.
                .filter(|id| !id.contains("embed"))
                .map(|id| ModelInfo { id: id.into(), detail: None })
                .collect()
        })
        .unwrap_or_default())
}

pub async fn chat(base: &str, req: &ChatRequest, on_delta: OnDelta<'_>) -> Result<String> {
    let model = req
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .ok_or_else(|| AiError::NotAvailable("elegí un modelo de LM Studio".into()))?;
    let request = client()
        .post(format!("{base}/v1/chat/completions"))
        .json(&serde_json::json!({ "model": model, "messages": messages(req), "stream": true }));
    stream(request, "LM Studio", format!("LM Studio no responde en {base}"), on_delta).await
}

/// The system prompt and the conversation, as chat-completion messages.
pub(crate) fn messages(req: &ChatRequest) -> Vec<serde_json::Value> {
    let mut messages = vec![serde_json::json!({ "role": "system", "content": req.system })];
    messages.extend(req.messages.iter().map(|m| serde_json::json!({ "role": m.role, "content": m.content })));
    messages
}

/// Send a streaming chat-completion request and relay the answer; `who`
/// names the server in errors, `down` is the error when it can't be reached.
pub(crate) async fn stream(request: reqwest::RequestBuilder, who: &str, down: String, on_delta: OnDelta<'_>) -> Result<String> {
    let resp = request.send().await.map_err(|e| AiError::NotAvailable(format!("{down} ({e})")))?;
    if !resp.status().is_success() {
        let v: serde_json::Value = resp.json().await.unwrap_or_default();
        let msg = v.pointer("/error/message").or_else(|| v.get("error")).map(|e| e.to_string()).unwrap_or_default();
        return Err(AiError::Provider(format!("{who}: {msg}")));
    }
    let mut out = String::new();
    let mut buf = Vec::<u8>::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk.map_err(|e| AiError::Provider(format!("{who}: se cortó la respuesta ({e})")))?);
        while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            if sse_line(&String::from_utf8_lossy(&line), &mut out, on_delta) {
                return Ok(out);
            }
        }
    }
    Ok(out)
}

/// One SSE line; true at `[DONE]`.
fn sse_line(line: &str, out: &mut String, on_delta: OnDelta<'_>) -> bool {
    let Some(data) = line.trim().strip_prefix("data:") else { return false };
    let data = data.trim();
    if data == "[DONE]" {
        return true;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(data) else { return false };
    let delta = v.pointer("/choices/0/delta");
    if let Some(t) = delta.and_then(|d| d.get("reasoning_content")).and_then(|t| t.as_str()).filter(|t| !t.is_empty()) {
        on_delta(Delta::Thinking(t.into()));
    }
    if let Some(t) = delta.and_then(|d| d.get("content")).and_then(|t| t.as_str()).filter(|t| !t.is_empty()) {
        out.push_str(t);
        on_delta(Delta::Text(t.into()));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_sent_events() {
        let cb = |_: Delta| {};
        let mut out = String::new();
        assert!(!sse_line(r#"data: {"choices":[{"delta":{"content":"SEL"}}]}"#, &mut out, &cb));
        assert!(!sse_line(": keep-alive", &mut out, &cb));
        assert!(!sse_line(r#"data: {"choices":[{"delta":{"content":"ECT"}}]}"#, &mut out, &cb));
        assert!(sse_line("data: [DONE]", &mut out, &cb));
        assert_eq!(out, "SELECT");
    }
}
