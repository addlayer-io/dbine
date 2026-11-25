//! DBine's AI assistant backends (docs/ai-assistant.md). Nothing here talks
//! to an AI service of its own: it uses what the user already has.
//!
//! - **Ollama** and **LM Studio**: local model servers (HTTP on localhost);
//!   nothing leaves the machine.
//! - **Claude Code** and **Codex** CLIs: the user's own subscription or API
//!   key, run without tools (no file, shell or MCP access) in an empty
//!   folder: they only answer.
//! - **API keys** ([`api`]): OpenAI, Anthropic, Azure OpenAI, Google Gemini,
//!   xAI Grok or any OpenAI-compatible server, with the user's own key; the
//!   provider bills the user's account.

pub mod anthropic;
pub mod api;
pub mod cli;
pub mod detect;
pub mod embedded;
pub mod env;
#[cfg(test)]
mod fake_server;
pub mod ollama;
pub mod openai;
pub mod prompts;

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
    /// The user's own API key (see [`api`]).
    #[serde(rename = "openai")]
    OpenAi,
    Anthropic,
    #[serde(rename = "azure_openai")]
    AzureOpenAi,
    Gemini,
    /// xAI's Grok.
    Xai,
    /// Any server that speaks OpenAI's chat completions, at the user's URL.
    #[serde(rename = "openai_compatible")]
    OpenAiCompatible,
}

impl ProviderKind {
    /// Every provider used with the user's own API key, in the order the UI lists them.
    pub const API: [ProviderKind; 6] = [
        ProviderKind::OpenAi,
        ProviderKind::Anthropic,
        ProviderKind::AzureOpenAi,
        ProviderKind::Gemini,
        ProviderKind::Xai,
        ProviderKind::OpenAiCompatible,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ProviderKind::Embedded => "Integrado en DBine",
            ProviderKind::Ollama => "Ollama",
            ProviderKind::LmStudio => "LM Studio",
            ProviderKind::ClaudeCode => "Claude Code",
            ProviderKind::Codex => "Codex",
            ProviderKind::OpenAi => "OpenAI",
            ProviderKind::Anthropic => "Anthropic",
            ProviderKind::AzureOpenAi => "Azure OpenAI",
            ProviderKind::Gemini => "Google Gemini",
            ProviderKind::Xai => "xAI Grok",
            ProviderKind::OpenAiCompatible => "Compatible con OpenAI",
        }
    }

    /// Used with the user's own API key.
    pub fn api_key(self) -> bool {
        ProviderKind::API.contains(&self)
    }

    /// The name used in settings and in the vault (`openai`, `azure_openai`…).
    pub fn id(self) -> &'static str {
        match self {
            ProviderKind::Embedded => "embedded",
            ProviderKind::Ollama => "ollama",
            ProviderKind::LmStudio => "lm_studio",
            ProviderKind::ClaudeCode => "claude_code",
            ProviderKind::Codex => "codex",
            ProviderKind::OpenAi => "openai",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::AzureOpenAi => "azure_openai",
            ProviderKind::Gemini => "gemini",
            ProviderKind::Xai => "xai",
            ProviderKind::OpenAiCompatible => "openai_compatible",
        }
    }

    /// Runs on this machine only (nothing is sent anywhere).
    pub fn local(self) -> bool {
        matches!(
            self,
            ProviderKind::Embedded | ProviderKind::Ollama | ProviderKind::LmStudio
        )
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
    /// The language of what DBine adds around the chat (the CLIs' framing),
    /// as [`prompts::lang`] gives it: the UI language.
    pub lang: &'static str,
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
pub async fn chat(
    req: &ChatRequest,
    endpoints: &Endpoints,
    on_delta: OnDelta<'_>,
    cancel: &Cancel,
) -> Result<String> {
    let run = async {
        match req.kind {
            ProviderKind::Embedded => {
                embedded::chat(&endpoints.models_dir, req, on_delta, cancel).await
            }
            ProviderKind::Ollama => ollama::chat(&endpoints.ollama, req, on_delta).await,
            ProviderKind::LmStudio => openai::chat(&endpoints.lmstudio, req, on_delta).await,
            ProviderKind::ClaudeCode => cli::claude(req, on_delta).await,
            ProviderKind::Codex => cli::codex(req, on_delta).await,
            kind => match endpoints.api.get(kind) {
                Some(access) => api::chat(kind, &access, req, on_delta).await,
                None => Err(api::not_configured(kind)),
            },
        }
    };
    tokio::select! {
        r = run => r,
        _ = cancel.cancelled() => Err(AiError::Cancelled),
    }
}

/// A chat's messages as one prompt, for the CLIs (they take a single
/// prompt; the conversation so far goes in front of the new message).
pub fn transcript(messages: &[ChatMessage], lang: &str) -> String {
    let Some((last, before)) = messages.split_last() else {
        return String::new();
    };
    if before.is_empty() {
        return last.content.clone();
    }
    let p = prompts::get(lang);
    let mut out = String::from("<previous_conversation>\n");
    for m in before {
        let who = p.text(if m.role == "assistant" {
            "transcript.assistant"
        } else {
            "transcript.user"
        });
        out.push_str(&format!("[{who}]\n{}\n\n", m.content.trim()));
    }
    out.push_str("</previous_conversation>\n\n");
    out.push_str(&last.content);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_puts_history_before_the_question() {
        let m = |r: &str, c: &str| ChatMessage {
            role: r.into(),
            content: c.into(),
        };
        assert_eq!(transcript(&[m("user", "hola")], "es"), "hola");
        let t = transcript(&[m("user", "a"), m("assistant", "b"), m("user", "c")], "es");
        assert!(t.starts_with(
            "<previous_conversation>\n[Usuario]\na\n\n[Asistente]\nb\n\n</previous_conversation>"
        ));
        assert!(t.ends_with("\n\nc"));
        let t = transcript(&[m("user", "a"), m("assistant", "b"), m("user", "c")], "en");
        assert!(
            t.starts_with("<previous_conversation>\n[User]\na\n\n[Assistant]\nb"),
            "{t}"
        );
    }

    /// Claude Code and Codex start a new process per call: a follow-up
    /// after a tool carries the question, the tool call and its result.
    #[test]
    fn transcript_carries_tool_steps() {
        let m = |r: &str, c: &str| ChatMessage {
            role: r.into(),
            content: c.into(),
        };
        let call = "<tool>{\"name\": \"list_objects\", \"arguments\": {\"database\": \"inventario\"}}</tool>";
        let t = transcript(
            &[
                m("user", "tablas de inventario"),
                m("assistant", call),
                m("user", "<result>\ntable productos\n</result>"),
            ],
            "en",
        );
        let q = t.find("tablas de inventario").unwrap();
        let c = t.find(call).unwrap();
        let r = t.find("<result>\ntable productos\n</result>").unwrap();
        assert!(q < c && c < r, "{t}");
        assert!(t.ends_with("</result>"), "the result is the last turn: {t}");
    }

    #[test]
    fn provider_ids_match_their_serde_names() {
        for k in ProviderKind::API.into_iter().chain([
            ProviderKind::Embedded,
            ProviderKind::Ollama,
            ProviderKind::LmStudio,
            ProviderKind::ClaudeCode,
            ProviderKind::Codex,
        ]) {
            assert_eq!(serde_json::to_value(k).unwrap(), k.id(), "{k:?}");
            assert_eq!(k.api_key(), ProviderKind::API.contains(&k));
            // Every API-key provider is remote: never sent rows (ai.rs).
            assert!(!k.api_key() || !k.local(), "{k:?}");
        }
    }

    #[tokio::test]
    async fn cancel_before_waiting_still_cancels() {
        let c = Cancel::new();
        c.cancel();
        tokio::time::timeout(std::time::Duration::from_millis(100), c.cancelled())
            .await
            .unwrap();
    }
}
