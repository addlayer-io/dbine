//! Ollama's HTTP API (https://github.com/ollama/ollama/blob/main/docs/api.md):
//! `/api/tags` (installed models), `/api/chat` (streamed NDJSON) and
//! `/api/pull` (download a model, with progress).

use crate::detect::ModelInfo;
use crate::{AiError, ChatRequest, Delta, OnDelta, Result};
use futures::StreamExt;
use serde::Deserialize;
use std::time::Duration;

/// The model suggested when none is installed: good at SQL and code, ~4.7
/// GB, runs on a 16 GB machine.
pub const RECOMMENDED_MODEL: &str = "qwen2.5-coder:7b";

fn client() -> reqwest::Client {
    reqwest::Client::builder().connect_timeout(Duration::from_secs(3)).build().unwrap_or_default()
}

fn unreachable(base: &str, e: reqwest::Error) -> AiError {
    AiError::NotAvailable(format!("Ollama no responde en {base} ({e}). ¿Está abierto?"))
}

/// Installed models; `Err` when Ollama isn't running.
pub async fn models(base: &str) -> Result<Vec<ModelInfo>> {
    #[derive(Deserialize)]
    struct Tags {
        #[serde(default)]
        models: Vec<Tag>,
    }
    #[derive(Deserialize)]
    struct Tag {
        name: String,
        #[serde(default)]
        size: Option<u64>,
        #[serde(default)]
        details: Option<serde_json::Value>,
    }
    let resp = client()
        .get(format!("{base}/api/tags"))
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .map_err(|e| unreachable(base, e))?;
    let tags: Tags = resp.json().await.map_err(|e| AiError::Provider(format!("Ollama: respuesta inesperada ({e})")))?;
    let mut out: Vec<ModelInfo> = tags
        .models
        .into_iter()
        .map(|t| ModelInfo {
            detail: t
                .details
                .as_ref()
                .and_then(|d| d.get("parameter_size").and_then(|v| v.as_str()))
                .map(|p| format!("{p} · {:.1} GB", t.size.unwrap_or(0) as f64 / 1e9)),
            id: t.name,
        })
        .collect();
    // Code-oriented models first: they're the best fit for SQL.
    out.sort_by_key(|m| {
        let n = m.id.to_lowercase();
        (!(n.contains("coder") || n.contains("sql") || n.contains("code")), n)
    });
    Ok(out)
}

/// Context window for a prompt of this size (Ollama's default, 2–4k
/// tokens, would silently cut a schema).
fn num_ctx(chars: usize) -> u32 {
    ((chars / 3) as u32 + 4096).clamp(8192, 32768)
}

pub async fn chat(base: &str, req: &ChatRequest, on_delta: OnDelta<'_>) -> Result<String> {
    let model = req
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .ok_or_else(|| AiError::NotAvailable("elegí un modelo de Ollama".into()))?;
    let mut messages = vec![serde_json::json!({ "role": "system", "content": req.system })];
    messages.extend(req.messages.iter().map(|m| serde_json::json!({ "role": m.role, "content": m.content })));
    let chars = req.system.len() + req.messages.iter().map(|m| m.content.len()).sum::<usize>();
    let body = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "options": { "num_ctx": num_ctx(chars) },
    });
    let resp = client().post(format!("{base}/api/chat")).json(&body).send().await.map_err(|e| unreachable(base, e))?;
    if !resp.status().is_success() {
        let v: serde_json::Value = resp.json().await.unwrap_or_default();
        let msg = v.get("error").and_then(|e| e.as_str()).unwrap_or("error desconocido");
        return Err(AiError::Provider(format!("Ollama: {msg}")));
    }
    let mut out = String::new();
    let mut buf = Vec::<u8>::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| AiError::Provider(format!("Ollama: se cortó la respuesta ({e})")))?;
        buf.extend_from_slice(&chunk);
        while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            if let Some(done) = handle_line(&line, &mut out, on_delta)? {
                if done {
                    return Ok(out);
                }
            }
        }
    }
    if !buf.is_empty() {
        handle_line(&buf, &mut out, on_delta)?;
    }
    Ok(out)
}

/// One NDJSON line: `Some(done)` when it was a chat event.
fn handle_line(line: &[u8], out: &mut String, on_delta: OnDelta<'_>) -> Result<Option<bool>> {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(line) else { return Ok(None) };
    if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
        return Err(AiError::Provider(format!("Ollama: {e}")));
    }
    if let Some(t) = v.pointer("/message/thinking").and_then(|t| t.as_str()).filter(|t| !t.is_empty()) {
        on_delta(Delta::Thinking(t.into()));
    }
    if let Some(t) = v.pointer("/message/content").and_then(|t| t.as_str()).filter(|t| !t.is_empty()) {
        out.push_str(t);
        on_delta(Delta::Text(t.into()));
    }
    Ok(Some(v.get("done").and_then(|d| d.as_bool()).unwrap_or(false)))
}

/// Download a model, reporting `(status, completed, total)`.
pub async fn pull(base: &str, model: &str, progress: &(dyn Fn(&str, u64, u64) + Send + Sync)) -> Result<()> {
    let resp = client()
        .post(format!("{base}/api/pull"))
        .json(&serde_json::json!({ "model": model, "stream": true }))
        .send()
        .await
        .map_err(|e| unreachable(base, e))?;
    let mut buf = Vec::<u8>::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        buf.extend_from_slice(&chunk.map_err(|e| AiError::Provider(format!("Ollama: se cortó la descarga ({e})")))?);
        while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&line) else { continue };
            if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
                return Err(AiError::Provider(format!("Ollama: {e}")));
            }
            let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
            progress(
                status,
                v.get("completed").and_then(|x| x.as_u64()).unwrap_or(0),
                v.get("total").and_then(|x| x.as_u64()).unwrap_or(0),
            );
            if status == "success" {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Start Ollama (its app on macOS, else `ollama serve` in the background)
/// and wait until it answers.
pub async fn start(base: &str) -> Result<()> {
    if models(base).await.is_ok() {
        return Ok(());
    }
    let app = std::path::Path::new("/Applications/Ollama.app");
    let started = if cfg!(target_os = "macos") && app.exists() {
        std::process::Command::new("open").args(["-g", "-a", "Ollama"]).status().map(|s| s.success()).unwrap_or(false)
    } else if let Some(bin) = crate::env::find("ollama") {
        std::process::Command::new(bin)
            .arg("serve")
            .env("PATH", crate::env::search_path())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
    } else {
        false
    };
    if !started {
        return Err(AiError::NotAvailable("Ollama no está instalado".into()));
    }
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if models(base).await.is_ok() {
            return Ok(());
        }
    }
    Err(AiError::NotAvailable("Ollama no arrancó a tiempo: abrilo a mano y volvé a detectar".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_grows_with_the_prompt() {
        assert_eq!(num_ctx(100), 8192);
        assert!(num_ctx(60_000) > 20_000);
        assert_eq!(num_ctx(10_000_000), 32768);
    }

    #[test]
    fn lines_stream_text_and_thinking() {
        let got = std::sync::Mutex::new(Vec::new());
        let cb = |d: Delta| got.lock().unwrap().push(d);
        let mut out = String::new();
        assert_eq!(handle_line(br#"{"message":{"thinking":"hmm"},"done":false}"#, &mut out, &cb).unwrap(), Some(false));
        assert_eq!(handle_line(br#"{"message":{"content":"SELECT"},"done":false}"#, &mut out, &cb).unwrap(), Some(false));
        assert_eq!(handle_line(br#"{"message":{"content":" 1"},"done":true}"#, &mut out, &cb).unwrap(), Some(true));
        assert_eq!(out, "SELECT 1");
        assert_eq!(got.lock().unwrap()[0], Delta::Thinking("hmm".into()));
        assert!(handle_line(br#"{"error":"model not found"}"#, &mut out, &cb).is_err());
    }
}
