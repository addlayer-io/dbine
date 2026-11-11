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
        tracing::debug!("AI task: unreadable answer again ({why})");
        CommandError::Internal(
            "La IA respondió algo que DBine no pudo leer. Probá de nuevo o con otro modelo.".into(),
        )
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
        return Err("JSON object missing from the answer".into());
    };
    if end < start {
        return Err("JSON object missing from the answer".into());
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

/// The tasks' prompts against a real model, which only a person can judge:
/// `DBINE_AI_LIVE_MODEL=<an Ollama model> cargo test -p dbine ai_task_live -- --ignored --nocapture`.
/// Ollama runs on this machine: nothing leaves it.
#[cfg(test)]
mod live {
    use super::*;
    use crate::commands::data_compare_ai::{read_filter, AiFilter};
    use dbine_driver::{ColumnDef, TableSchema};
    use dbine_schema::compare::{compare, CompareOptions, DbModel};
    use dbine_schema::renames;

    fn asker() -> Option<Asker> {
        let model = std::env::var("DBINE_AI_LIVE_MODEL").ok()?;
        Some(Asker {
            provider: ProviderKind::Ollama,
            model: Some(model),
            ui_language: Some("es".into()),
            run_id: "live".into(),
        })
    }

    fn table(name: &str, cols: &[(&str, &str)]) -> TableSchema {
        TableSchema {
            kind: "table".into(),
            schema: Some("dbo".into()),
            name: name.into(),
            columns: cols
                .iter()
                .map(|(n, t)| ColumnDef {
                    name: n.to_string(),
                    data_type: t.to_string(),
                    nullable: true,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    #[tokio::test]
    #[ignore]
    async fn ai_task_live_renames_and_filters() {
        let Some(a) = asker() else {
            eprintln!("skipped: set DBINE_AI_LIVE_MODEL");
            return;
        };
        let ai = AiRuntime::new(std::env::temp_dir().join("dbine-ai-live"));
        let cancel = Cancel::new();
        let p = a.prompts();

        let left = DbModel {
            driver: "sqlserver".into(),
            tables: vec![table(
                "clientes",
                &[
                    ("id", "int"),
                    ("nombre", "nvarchar(80)"),
                    ("apellido", "nvarchar(80)"),
                    ("fecha_alta", "date"),
                ],
            )],
            objects: Vec::new(),
        };
        let right = DbModel {
            driver: "sqlserver".into(),
            tables: vec![table(
                "clientes",
                &[
                    ("id", "int"),
                    ("first_name", "nvarchar(80)"),
                    ("last_name", "nvarchar(80)"),
                    ("created_on", "date"),
                ],
            )],
            objects: Vec::new(),
        };
        let result = compare(&left, &right, &CompareOptions::default());
        let q =
            renames::question(&left, &right, &result, &result.renames).expect("something to ask");
        eprintln!("asked:\n{}", q.text);
        let pairs = ask(
            &ai,
            &a,
            &cancel,
            p.text("compare.renames_system").into(),
            q.text.clone(),
            |t| renames::read(json_in(t)?, &q),
        )
        .await
        .unwrap();
        eprintln!("renames: {pairs:?}");
        assert!(
            pairs
                .iter()
                .any(|h| h.left == "nombre" && h.right == "first_name"),
            "{pairs:?}"
        );

        let columns: Vec<String> = ["id", "precio", "nombre", "actualizado"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let text = "list: rows whose values differ (a filter passes if the left or the right value passes)\ncolumns:\n  id int\n  precio numeric(10,2)\n  nombre text\n  actualizado timestamp\noperators: eq, ne, gt, ge, lt, le, contains, not_contains, starts_with, ends_with, is_null, not_null, is_empty, not_empty, in, not_in, is_true, is_false\nrequest: las que difieren solo en el precio, con precio mayor a 100, las más caras primero\n";
        let f: AiFilter = ask(
            &ai,
            &a,
            &cancel,
            p.text("datacompare.filter_system").into(),
            text.into(),
            |t| read_filter(json_in(t)?, &columns, true),
        )
        .await
        .unwrap();
        eprintln!("filter: {f:?}");
        assert!(f.changed_in.contains(&"precio".to_string()), "{f:?}");
        assert!(f.filters.iter().any(|x| x.column == "precio"), "{f:?}");
        assert!(
            f.sort
                .first()
                .is_some_and(|s| s.column == "precio" && s.descending),
            "{f:?}"
        );
    }
}
