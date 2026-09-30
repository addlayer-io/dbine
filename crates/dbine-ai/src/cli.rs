//! The Claude Code and Codex command-line tools, used as plain chat models:
//! one process per answer, in an empty temporary folder, with every tool
//! turned off, so they can't read files, run commands or reach MCP servers.
//! They use the user's own login (subscription or API key).

use crate::{transcript, AiError, ChatRequest, Delta, OnDelta, Result};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// An empty folder of its own for each run (removed when dropped).
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new() -> Result<Self> {
        let dir = std::env::temp_dir().join(format!("dbine-ai-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&dir).map_err(|e| AiError::Provider(format!("carpeta temporal: {e}")))?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn rand_suffix() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

/// What a line of a CLI's JSON stream means.
#[derive(Debug, PartialEq)]
pub enum Event {
    Delta(Delta),
    /// The whole answer (Claude's `result`, Codex's finished message).
    Final { text: String, is_error: bool },
    /// Codex's answer, whole, as soon as it's done (no deltas).
    Message(String),
    Error(String),
    Other,
}

/// Claude Code's `--output-format stream-json --include-partial-messages`.
pub fn parse_claude(line: &str) -> Event {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return Event::Other };
    match v.get("type").and_then(|t| t.as_str()) {
        Some("stream_event") => {
            let d = v.pointer("/event/delta");
            match (v.pointer("/event/type").and_then(|t| t.as_str()), d.and_then(|d| d.get("type")).and_then(|t| t.as_str())) {
                (Some("content_block_delta"), Some("text_delta")) => {
                    Event::Delta(Delta::Text(d.and_then(|d| d.get("text")).and_then(|t| t.as_str()).unwrap_or("").into()))
                }
                (Some("content_block_delta"), Some("thinking_delta")) => {
                    match d.and_then(|d| d.get("thinking")).and_then(|t| t.as_str()).filter(|t| !t.is_empty()) {
                        Some(t) => Event::Delta(Delta::Thinking(t.into())),
                        None => Event::Other,
                    }
                }
                _ => Event::Other,
            }
        }
        Some("result") => Event::Final {
            text: v.get("result").and_then(|r| r.as_str()).unwrap_or("").into(),
            is_error: v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false),
        },
        _ => Event::Other,
    }
}

/// Codex's `exec --json` (JSONL; both the current `item.*` events and the
/// older `msg` ones).
pub fn parse_codex(line: &str) -> Event {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return Event::Other };
    let s = |p: &str| v.pointer(p).and_then(|x| x.as_str()).map(str::to_string);
    match v.get("type").and_then(|t| t.as_str()) {
        Some("item.completed") => match s("/item/type").as_deref() {
            Some("agent_message") => Event::Message(s("/item/text").unwrap_or_default()),
            Some("reasoning") => Event::Delta(Delta::Thinking(s("/item/text").unwrap_or_default())),
            _ => Event::Other,
        },
        Some("turn.failed") => Event::Error(s("/error/message").unwrap_or_else(|| "Codex no pudo responder".into())),
        Some("error") => Event::Error(s("/message").unwrap_or_else(|| "error de Codex".into())),
        _ => match s("/msg/type").as_deref() {
            Some("agent_message_delta") => Event::Delta(Delta::Text(s("/msg/delta").unwrap_or_default())),
            Some("agent_message") => Event::Message(s("/msg/message").unwrap_or_default()),
            Some("error") => Event::Error(s("/msg/message").unwrap_or_default()),
            _ => Event::Other,
        },
    }
}

async fn run(
    program: &std::path::Path,
    args: &[String],
    cwd: &std::path::Path,
    stdin_text: &str,
    parse: fn(&str) -> Event,
    on_delta: OnDelta<'_>,
    name: &str,
) -> Result<String> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env("PATH", crate::env::search_path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| AiError::NotAvailable(format!("no se pudo ejecutar {name}: {e}")))?;
    let mut stdin = child.stdin.take().expect("piped");
    let text = stdin_text.to_string();
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(text.as_bytes()).await;
        let _ = stdin.shutdown().await;
    });
    let mut stderr = child.stderr.take().expect("piped");
    let err_task = tokio::spawn(async move {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s).await;
        s
    });
    let mut lines = BufReader::new(child.stdout.take().expect("piped")).lines();
    let mut streamed = String::new();
    let mut last_message: Option<String> = None;
    let mut final_text: Option<(String, bool)> = None;
    let mut error: Option<String> = None;
    while let Some(line) = lines.next_line().await.map_err(|e| AiError::Provider(format!("{name}: {e}")))? {
        match parse(&line) {
            Event::Delta(Delta::Text(t)) => {
                streamed.push_str(&t);
                on_delta(Delta::Text(t));
            }
            Event::Delta(d) => on_delta(d),
            Event::Message(m) => {
                // Codex sends each message whole: show it as it comes.
                let chunk = if streamed.is_empty() { m.clone() } else { format!("\n\n{m}") };
                streamed.push_str(&chunk);
                on_delta(Delta::Text(chunk));
                last_message = Some(m);
            }
            Event::Final { text, is_error } => final_text = Some((text, is_error)),
            Event::Error(e) => error = Some(e),
            Event::Other => {}
        }
    }
    let _ = writer.await;
    let status = child.wait().await.map_err(|e| AiError::Provider(format!("{name}: {e}")))?;
    let stderr = err_task.await.unwrap_or_default();
    if let Some((text, true)) = &final_text {
        return Err(AiError::Provider(format!("{name}: {}", text.trim())));
    }
    if let Some(e) = error {
        return Err(AiError::Provider(format!("{name}: {e}")));
    }
    if !status.success() && streamed.is_empty() {
        let tail: String = stderr.lines().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        return Err(AiError::Provider(format!("{name} terminó con error ({status}): {}", tail.trim())));
    }
    // Nothing streamed (an older version): send the final answer whole.
    if streamed.is_empty() {
        if let Some((text, _)) = final_text.or(last_message.map(|m| (m, false))) {
            on_delta(Delta::Text(text.clone()));
            return Ok(text);
        }
    }
    Ok(streamed)
}

pub async fn claude(req: &ChatRequest, on_delta: OnDelta<'_>) -> Result<String> {
    let bin = crate::env::find("claude").ok_or_else(|| AiError::NotAvailable("Claude Code no está instalado".into()))?;
    let scratch = Scratch::new()?;
    let system_file = scratch.0.join("system.md");
    std::fs::write(&system_file, &req.system).map_err(|e| AiError::Provider(format!("carpeta temporal: {e}")))?;
    let mut args: Vec<String> = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        // No tools, no MCP servers, no user/project settings (hooks,
        // permissions): it only answers.
        "--tools",
        "",
        "--strict-mcp-config",
        "--setting-sources",
        "",
        "--no-session-persistence",
        "--system-prompt-file",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(system_file.display().to_string());
    if let Some(m) = req.model.as_ref().filter(|m| !m.is_empty()) {
        args.push("--model".into());
        args.push(m.clone());
    }
    run(&bin, &args, &scratch.0, &transcript(&req.messages), parse_claude, on_delta, "Claude Code").await
}

pub async fn codex(req: &ChatRequest, on_delta: OnDelta<'_>) -> Result<String> {
    let bin = crate::env::find("codex").ok_or_else(|| AiError::NotAvailable("Codex no está instalado".into()))?;
    let scratch = Scratch::new()?;
    let mut args: Vec<String> = ["exec", "--json", "--skip-git-repo-check", "--sandbox", "read-only", "-C"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.push(scratch.0.display().to_string());
    if let Some(m) = req.model.as_ref().filter(|m| !m.is_empty()) {
        args.push("-m".into());
        args.push(m.clone());
    }
    // The prompt from stdin: Codex has no system-prompt flag, so the
    // instructions go first.
    args.push("-".into());
    let prompt = format!(
        "<instrucciones>\n{}\n</instrucciones>\n\nNo ejecutes comandos ni leas archivos: respondé solo con texto.\n\n{}",
        req.system,
        transcript(&req.messages)
    );
    run(&bin, &args, &scratch.0, &prompt, parse_codex, on_delta, "Codex").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_stream_json() {
        let d = r#"{"type":"stream_event","event":{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"SELECT 1"}}}"#;
        assert_eq!(parse_claude(d), Event::Delta(Delta::Text("SELECT 1".into())));
        let empty_thinking = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":""}}}"#;
        assert_eq!(parse_claude(empty_thinking), Event::Other);
        let r = r#"{"type":"result","subtype":"success","is_error":false,"result":"hola"}"#;
        assert_eq!(parse_claude(r), Event::Final { text: "hola".into(), is_error: false });
        let e = r#"{"type":"result","subtype":"success","is_error":true,"result":"Not logged in · Please run /login"}"#;
        assert!(matches!(parse_claude(e), Event::Final { is_error: true, .. }));
        assert_eq!(parse_claude("not json"), Event::Other);
    }

    #[test]
    fn codex_jsonl_both_formats() {
        let m = r#"{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"SELECT 1"}}"#;
        assert_eq!(parse_codex(m), Event::Message("SELECT 1".into()));
        assert!(matches!(parse_codex(r#"{"type":"turn.failed","error":{"message":"quota"}}"#), Event::Error(e) if e == "quota"));
        assert_eq!(parse_codex(r#"{"type":"turn.completed","usage":{}}"#), Event::Other);
        let old = r#"{"id":"0","msg":{"type":"agent_message_delta","delta":"SEL"}}"#;
        assert_eq!(parse_codex(old), Event::Delta(Delta::Text("SEL".into())));
    }

    /// A fake CLI (a shell script) streaming JSON lines: the runner passes
    /// stdin, streams deltas and returns the text; a failing one reports
    /// its stderr.
    #[cfg(unix)]
    #[tokio::test]
    async fn runs_a_cli_and_streams() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let script = d.path().join("fake");
        std::fs::write(
            &script,
            "#!/bin/sh\nread q\necho '{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"eco: \"}}}'\n\
             printf '{\"type\":\"stream_event\",\"event\":{\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"%s\"}}}\\n' \"$q\"\n\
             echo '{\"type\":\"result\",\"is_error\":false,\"result\":\"x\"}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let got = std::sync::Mutex::new(String::new());
        let cb = |dl: Delta| {
            if let Delta::Text(t) = dl {
                got.lock().unwrap().push_str(&t)
            }
        };
        let out = run(&script, &[], d.path(), "hola\n", parse_claude, &cb, "fake").await.unwrap();
        assert_eq!(out, "eco: hola");
        assert_eq!(*got.lock().unwrap(), "eco: hola");

        std::fs::write(&script, "#!/bin/sh\necho 'Not logged in' >&2\nexit 3\n").unwrap();
        let err = run(&script, &[], d.path(), "", parse_claude, &cb, "fake").await.unwrap_err();
        assert!(err.to_string().contains("Not logged in"), "{err}");
    }
}
