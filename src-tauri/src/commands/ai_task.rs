//! One-shot AI tasks (docs/ai-assistant.md, "Tasks"): DBine asks for an answer
//! in a format it reads, and an answer it can't read goes back to the model
//! once, with what was wrong. The optimizer, schema compare and data compare
//! use it. Nothing here touches a database: the prompts carry what DBine
//! already computed, and what comes back is only a suggestion for the UI.

use crate::commands::ai::AiRuntime;
use crate::error::{CommandError, CommandResult};
use dashmap::DashMap;
use dbine_ai::prompts::Prompts;
use dbine_ai::{embedded, Cancel, ChatMessage, ChatRequest, ProviderKind};
use serde::Deserialize;
use std::sync::OnceLock;

/// Tasks in flight, by the UI's id.
fn runs() -> &'static DashMap<String, Cancel> {
    static R: OnceLock<DashMap<String, Cancel>> = OnceLock::new();
    R.get_or_init(DashMap::new)
}

/// A task's cancel, registered under its id while it lives.
pub(crate) struct Run {
    id: String,
    pub cancel: Cancel,
}

impl Run {
    pub fn start(id: &str) -> Self {
        let cancel = Cancel::new();
        runs().insert(id.to_string(), cancel.clone());
        Run {
            id: id.to_string(),
            cancel,
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        runs().remove(&self.id);
    }
}

/// Stops a task, if it's still running.
pub(crate) fn cancel(id: &str) {
    if let Some((_, c)) = runs().remove(id) {
        c.cancel();
    }
}

/// Who answers and in which language, as the UI chose.
#[derive(Debug, Clone, Deserialize)]
pub struct Asker {
    pub provider: ProviderKind,
    #[serde(default)]
    pub model: Option<String>,
    /// The UI's language (`en`, `pt`…): the answer's texts come in it.
    #[serde(default)]
    pub ui_language: Option<String>,
    pub run_id: String,
}

impl Asker {
    pub fn prompts(&self) -> &'static Prompts {
        dbine_ai::prompts::get(self.ui_language.as_deref().unwrap_or(""))
    }

    /// How much context fits: local models have small windows.
    pub fn budget(&self) -> usize {
        if self.provider.local() {
            12_000
        } else {
            60_000
        }
    }
}

/// Gets the provider ready: the built-in model's engine downloads on first use.
pub(crate) async fn ready(
    ai: &AiRuntime,
    provider: ProviderKind,
    cancel: &Cancel,
) -> CommandResult<()> {
    if provider == ProviderKind::Embedded {
        embedded::ensure_engine(&ai.endpoints.models_dir, &|_, _| {}, cancel).await?;
    }
    Ok(())
}

/// Asks once and reads the answer with `read`. An answer it can't read goes
/// back with the reason, once; a second one that fails is an error.
pub(crate) async fn ask<T>(
    ai: &AiRuntime,
    asker: &Asker,
    cancel: &Cancel,
    system: String,
    user: String,
    read: impl Fn(&str) -> Result<T, String>,
) -> CommandResult<T> {
    ready(ai, asker.provider, cancel).await?;
    let p = asker.prompts();
    let mut req = ChatRequest {
        kind: asker.provider,
        model: asker.model.clone(),
        system,
        messages: vec![ChatMessage {
            role: "user".into(),
            content: user,
        }],
        lang: p.lang(),
    };
    let text = dbine_ai::chat(&req, &ai.endpoints, &|_| {}, cancel).await?;
    let why = match read(&text) {
        Ok(v) => return Ok(v),
        Err(why) => why,
    };
    tracing::debug!("AI task: unreadable answer ({why}), asking again");
    req.messages.push(ChatMessage {
        role: "assistant".into(),
        content: text,
    });
    req.messages.push(ChatMessage {
        role: "user".into(),
        content: p.fill("task.repair", &[("error", &why)]),
    });
    let text = dbine_ai::chat(&req, &ai.endpoints, &|_| {}, cancel).await?;
    read(&text).map_err(|why| {
        CommandError::Internal(format!(
            "La IA respondió algo que DBine no pudo leer: {why}"
        ))
    })
}

/// The JSON object in an answer: in a fenced block, or the outermost braces.
pub(crate) fn json_in(text: &str) -> Result<serde_json::Value, String> {
    let fenced = text.split("```").enumerate().filter(|(i, _)| i % 2 == 1);
    for (_, block) in fenced {
        let body = block.trim_start_matches(|c: char| c.is_ascii_alphabetic());
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(body.trim()) {
            if v.is_object() {
                return Ok(v);
            }
        }
    }
    let (Some(start), Some(end)) = (text.find('{'), text.rfind('}')) else {
        return Err("no JSON object in the answer".into());
    };
    if end < start {
        return Err("no JSON object in the answer".into());
    }
    match serde_json::from_str::<serde_json::Value>(&text[start..=end]) {
        Ok(v) if v.is_object() => Ok(v),
        Ok(_) => Err("the answer's JSON isn't an object".into()),
        Err(e) => Err(format!("the answer's JSON doesn't parse: {e}")),
    }
}

/// Text cut to `budget` characters on a line boundary, saying how much was
/// left out with `more` (a prompt with `{count}`).
pub(crate) fn fit(text: &str, budget: usize, p: &Prompts, more: &str) -> String {
    if text.chars().count() <= budget {
        return text.to_string();
    }
    let mut out = String::new();
    let mut lines = text.lines();
    for line in lines.by_ref() {
        if out.chars().count() + line.chars().count() + 1 > budget {
            break;
        }
        out.push_str(line);
        out.push('\n');
    }
    let left = text.lines().count() - out.lines().count();
    out.push_str(&p.fill(more, &[("count", &left.to_string())]));
    out
}

#[derive(Deserialize)]
pub struct CancelArgs {
    pub run_id: String,
}

/// Stops a task the UI no longer waits for.
#[tauri::command(rename_all = "camelCase")]
pub async fn ai_task_cancel(args: CancelArgs) -> CommandResult<()> {
    cancel(&args.run_id);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_json_comes_from_a_fence_or_the_outer_braces() {
        let fenced = "Here:\n```json\n{\"a\": [1, 2]}\n```\nDone.";
        assert_eq!(json_in(fenced).unwrap()["a"][1], 2);
        let bare = "Sure. {\"renames\": []} That's all.";
        assert!(json_in(bare).unwrap()["renames"].is_array());
        assert!(json_in("no idea").is_err());
        assert!(json_in("} backwards {").is_err());
        assert!(json_in("{\"a\": ").is_err());
    }

    #[test]
    fn fit_cuts_on_a_line_and_says_how_much_is_left() {
        let p = dbine_ai::prompts::get("en");
        let text = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let short = fit(&text, 40, p, "task.more_lines");
        assert!(short.starts_with("line 0\n"));
        assert!(short.chars().count() < text.chars().count());
        assert!(!short.contains("line 99"));
        assert_eq!(fit("short", 40, p, "task.more_lines"), "short");
    }
}
