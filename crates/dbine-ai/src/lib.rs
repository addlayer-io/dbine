//! DBine's AI assistant backends (docs/asistente-ia.md). Nothing here talks
//! to an AI service of its own: it finds what the user already has on the
//! machine and uses it.
//!
//! - **Ollama** and **LM Studio**: local model servers (HTTP on localhost);
//!   nothing leaves the machine.
//! - **Claude Code** and **Codex** CLIs: the user's own subscription or API
//!   key, run without tools (no file, shell or MCP access) in an empty
//!   folder: they only answer.

pub mod cli;
pub mod detect;
pub mod embedded;
pub mod env;
pub mod ollama;
pub mod openai;

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Notify;

pub use detect::{detect, Endpoints, ModelInfo, ProviderInfo};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// The model built into DBine (llama.cpp).
    Embedded,
    Ollama,
    LmStudio,
    ClaudeCode,
    Codex,
}

impl ProviderKind {
    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::Embedded => "Integrado en DBine",
            ProviderKind::Ollama => "Ollama",
            ProviderKind::LmStudio => "LM Studio",
            ProviderKind::ClaudeCode => "Claude Code",
            ProviderKind::Codex => "Codex",
        }
    }

    /// Runs on this machine only (nothing is sent anywhere).
    pub fn local(self) -> bool {
        matches!(self, ProviderKind::Embedded | ProviderKind::Ollama | ProviderKind::LmStudio)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    /// `user` or `assistant`.
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub kind: ProviderKind,
    pub model: Option<String>,
    pub system: String,
    pub messages: Vec<ChatMessage>,
}

/// A piece of the answer as it arrives.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", content = "text", rename_all = "snake_case")]
pub enum Delta {
    Text(String),
    /// The model's reasoning, for models that show it.
    Thinking(String),
}

pub type OnDelta<'a> = &'a (dyn Fn(Delta) + Send + Sync);

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("{0}")]
    NotAvailable(String),
    #[error("{0}")]
    Provider(String),
    #[error("se canceló")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, AiError>;

/// Stops a running chat.
#[derive(Clone, Default)]
pub struct Cancel(Arc<Notify>, Arc<std::sync::atomic::AtomicBool>);

impl Cancel {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.1.store(true, std::sync::atomic::Ordering::SeqCst);
        self.0.notify_waiters();
    }
    pub fn is_cancelled(&self) -> bool {
        self.1.load(std::sync::atomic::Ordering::SeqCst)
    }
    pub async fn cancelled(&self) {
        // Registered before the flag is read: a cancel in between isn't lost.
        let notified = self.0.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_cancelled() {
            return;
        }
        notified.await
    }
}

/// Run a chat with the chosen provider, streaming the answer to `on_delta`;
/// returns the whole answer text.
pub async fn chat(req: &ChatRequest, endpoints: &Endpoints, on_delta: OnDelta<'_>, cancel: &Cancel) -> Result<String> {
    let run = async {
        match req.kind {
            ProviderKind::Embedded => embedded::chat(&endpoints.models_dir, req, on_delta, cancel).await,
            ProviderKind::Ollama => ollama::chat(&endpoints.ollama, req, on_delta).await,
            ProviderKind::LmStudio => openai::chat(&endpoints.lmstudio, req, on_delta).await,
            ProviderKind::ClaudeCode => cli::claude(req, on_delta).await,
            ProviderKind::Codex => cli::codex(req, on_delta).await,
        }
    };
    tokio::select! {
        r = run => r,
        _ = cancel.cancelled() => Err(AiError::Cancelled),
    }
}

/// A chat's messages as one prompt, for the CLIs (they take a single
/// prompt; the conversation so far goes in front of the new message).
pub fn transcript(messages: &[ChatMessage]) -> String {
    let Some((last, before)) = messages.split_last() else { return String::new() };
    if before.is_empty() {
        return last.content.clone();
    }
    let mut out = String::from("<conversacion_previa>\n");
    for m in before {
        let who = if m.role == "assistant" { "Asistente" } else { "Usuario" };
        out.push_str(&format!("[{who}]\n{}\n\n", m.content.trim()));
    }
    out.push_str("</conversacion_previa>\n\n");
    out.push_str(&last.content);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_puts_history_before_the_question() {
        let m = |r: &str, c: &str| ChatMessage { role: r.into(), content: c.into() };
        assert_eq!(transcript(&[m("user", "hola")]), "hola");
        let t = transcript(&[m("user", "a"), m("assistant", "b"), m("user", "c")]);
        assert!(t.starts_with("<conversacion_previa>\n[Usuario]\na\n\n[Asistente]\nb\n\n</conversacion_previa>"));
        assert!(t.ends_with("\n\nc"));
    }

    #[tokio::test]
    async fn cancel_before_waiting_still_cancels() {
        let c = Cancel::new();
        c.cancel();
        tokio::time::timeout(std::time::Duration::from_millis(100), c.cancelled()).await.unwrap();
    }
}
