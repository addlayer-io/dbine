//! The git installed on the machine, run the same way by the Library's git
//! (`library_git.rs`) and by projects (`projects.rs`, `projects_git.rs`):
//! the user's own credentials (credential helper, SSH agent), never a prompt
//! (`GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=never`), paths as they are
//! (`core.quotePath=false`), pathspecs taken literally (`GIT_LITERAL_PATHSPECS`),
//! the child killed when the call is dropped, and a time limit.

use crate::error::{CommandError, CommandResult};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

/// Longest a local git command may take.
pub const GIT_LIMIT: Duration = Duration::from_secs(120);
/// Longest a network one may take (clone, fetch, push over a slow link).
pub const NET_LIMIT: Duration = Duration::from_secs(900);

pub fn git_path() -> CommandResult<PathBuf> {
    dbine_ai::env::find("git").ok_or_else(|| {
        CommandError::BadRequest("no se encontró git en esta máquina: instalalo (git-scm.com) y volvé a intentar".into())
    })
}

/// `git --version`, or `None` when git isn't installed.
pub async fn version() -> Option<String> {
    git_path().ok()?;
    run(&std::env::temp_dir(), &["--version"]).await.ok()
}

pub struct RunOpts<'a> {
    pub limit: Duration,
    /// Notified to stop the command (its process is killed).
    pub cancel: Option<Arc<Notify>>,
    /// Set after the defaults, so a caller that really wants glob or magic
    /// pathspecs can opt out with `("GIT_LITERAL_PATHSPECS", "0")`.
    pub env: &'a [(&'a str, &'a str)],
    /// Each line git writes to stderr (split on `\r` and `\n`), for `--progress`.
    pub progress: Option<&'a (dyn Fn(&str) + Send + Sync)>,
    /// Stdout as it is: `git show` output keeps its trailing newlines.
    pub raw: bool,
    /// Written to git's stdin (`--stdin` commands); stdin is closed otherwise.
    pub input: Option<&'a [u8]>,
}

impl Default for RunOpts<'_> {
    fn default() -> Self {
        Self { limit: GIT_LIMIT, cancel: None, env: &[], progress: None, raw: false, input: None }
    }
}

/// What a git command left, whatever its exit code.
pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    /// Trimmed.
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// Run git in `dir` and return what it left, failing only when it couldn't
/// run, timed out or was cancelled.
pub async fn output(dir: &Path, args: &[&str], o: RunOpts<'_>) -> CommandResult<Output> {
    let mut cmd = tokio::process::Command::new(git_path()?);
    cmd.current_dir(cwd(dir))
        // Paths as they are (accents included), not octal-escaped.
        .args(["-c", "core.quotePath=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        // Paths come from the repository (a file may be named `*` or
        // `:(glob)**`): as a pathspec they must match only that file, never
        // expand to every file of the tree (a Discard would wipe them all).
        .env("GIT_LITERAL_PATHSPECS", "1")
        .stdin(if o.input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (k, v) in o.env {
        cmd.env(k, v);
    }
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW: no console window next to the app.
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|e| CommandError::Internal(format!("no se pudo ejecutar git: {e}")))?;
    let stdin = child.stdin.take();
    let mut stdout = child.stdout.take().ok_or_else(|| CommandError::Internal("git sin salida".into()))?;
    let mut stderr = child.stderr.take().ok_or_else(|| CommandError::Internal("git sin salida".into()))?;
    let (input, progress) = (o.input, o.progress);
    let work = async {
        let write = async move {
            if let (Some(mut w), Some(data)) = (stdin, input) {
                let _ = w.write_all(data).await;
                // Dropping `w` closes stdin: git sees the end of its input.
            }
        };
        let read_out = async {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf).await;
            buf
        };
        let read_err = async {
            let (mut all, mut line, mut chunk) = (Vec::new(), Vec::new(), [0u8; 4096]);
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        all.extend_from_slice(&chunk[..n]);
                        if let Some(cb) = progress {
                            for &b in &chunk[..n] {
                                if b == b'\r' || b == b'\n' {
                                    if !line.is_empty() {
                                        cb(&String::from_utf8_lossy(&line));
                                        line.clear();
                                    }
                                } else {
                                    line.push(b);
                                }
                            }
                        }
                    }
                }
            }
            if let (Some(cb), false) = (progress, line.is_empty()) {
                cb(&String::from_utf8_lossy(&line));
            }
            all
        };
        let (_, out, err) = tokio::join!(write, read_out, read_err);
        (child.wait().await, out, err)
    };
    let cancel = o.cancel.clone();
    let stopped = async move {
        match cancel {
            Some(n) => n.notified().await,
            None => std::future::pending().await,
        }
    };
    enum End<T> {
        Done(T),
        Timeout,
        Cancelled,
    }
    let end = tokio::select! {
        // A cancel wins over a command that happens to end at the same time.
        biased;
        _ = stopped => End::Cancelled,
        _ = tokio::time::sleep(o.limit) => End::Timeout,
        r = work => End::Done(r),
    };
    match end {
        End::Done((status, stdout, stderr)) => {
            let status = status.map_err(|e| CommandError::Internal(format!("no se pudo ejecutar git: {e}")))?;
            Ok(Output { code: status.code(), stdout, stderr: String::from_utf8_lossy(&stderr).trim().to_string() })
        }
        stop => {
            // Killed and reaped before returning: whatever it was writing
            // (a clone's target) is no longer touched when the caller cleans up.
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(match stop {
                End::Timeout => CommandError::Connect(format!(
                    "git {} no terminó en {} s",
                    args.first().unwrap_or(&""),
                    o.limit.as_secs()
                )),
                _ => CommandError::BadRequest("operación cancelada".into()),
            })
        }
    }
}

/// Run git in `dir`: its stdout (trimmed unless `raw`), or its message as a
/// `BadRequest`.
pub async fn run_with(dir: &Path, args: &[&str], o: RunOpts<'_>) -> CommandResult<Vec<u8>> {
    let raw = o.raw;
    let out = output(dir, args, o).await?;
    if out.ok() {
        Ok(if raw { out.stdout } else { String::from_utf8_lossy(&out.stdout).trim().as_bytes().to_vec() })
    } else if out.stderr.is_empty() {
        Err(CommandError::BadRequest(String::from_utf8_lossy(&out.stdout).trim().to_string()))
    } else {
        Err(CommandError::BadRequest(out.stderr))
    }
}

/// Run git in `dir`; its output trimmed, or its message as the error.
pub async fn run(dir: &Path, args: &[&str]) -> CommandResult<String> {
    run_with(dir, args, RunOpts::default()).await.map(|b| String::from_utf8_lossy(&b).to_string())
}

/// Commits need a name and an email; DBine's when git has none configured.
/// For the Library's own working copy only: a project commits as the user.
pub async fn identity(dir: &Path) -> Vec<String> {
    let has = |key: &'static str| async move { run(dir, &["config", key]).await.map(|v| !v.is_empty()).unwrap_or(false) };
    let mut args = Vec::new();
    if !has("user.name").await {
        args.extend(["-c".to_string(), "user.name=DBine".to_string()]);
    }
    if !has("user.email").await {
        args.extend(["-c".to_string(), "user.email=dbine@localhost".to_string()]);
    }
    args
}

/// Git knows the user's name and email (repo or global config).
pub async fn has_identity(dir: &Path) -> bool {
    for key in ["user.name", "user.email"] {
        if !run(dir, &["config", key]).await.map(|v| !v.is_empty()).unwrap_or(false) {
            return false;
        }
    }
    true
}

/// A failed network command's message, as an error the user can act on.
pub fn map_error(stderr: &str) -> CommandError {
    let msg = redact_url(stderr.trim());
    let has = |s: &str| msg.contains(s);
    if has("Authentication failed") || has("could not read Username") || has("Permission denied (publickey)") || has("terminal prompts disabled") {
        CommandError::AuthFailed(format!(
            "git no tiene credenciales para este remoto: configurá el credential helper o la clave SSH ({msg})"
        ))
    } else if has("Host key verification failed") {
        CommandError::Connect("el host SSH no es de confianza todavía: conectate una vez desde una terminal".into())
    } else if has("[rejected]") && (has("non-fast-forward") || has("fetch first")) {
        CommandError::BadRequest("el remoto tiene cambios que no tenés: hacé Pull o Sincronizar primero".into())
    } else if has("Could not resolve host") || has("unable to access") {
        CommandError::Connect(format!("no se pudo conectar con el remoto ({msg})"))
    } else if has("Please tell me who you are") || has("unable to auto-detect email address") {
        CommandError::BadRequest("git no tiene tu nombre y email configurados".into())
    } else if has("src refspec") && has("does not match any") {
        CommandError::BadRequest("no hay commits para enviar: confirmá primero".into())
    } else {
        CommandError::BadRequest(msg)
    }
}

/// A git error, remapped by `map_error` when it carries git's message.
pub fn remap(e: CommandError) -> CommandError {
    match e {
        CommandError::BadRequest(m) if m != "operación cancelada" => map_error(&m),
        other => other,
    }
}

/// `https://user:pass@host/x` → `https://host/x`, anywhere in the text: no
/// credential in a URL reaches a log or an error.
pub fn redact_url(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("://") {
        out.push_str(&rest[..i + 3]);
        rest = &rest[i + 3..];
        let end = rest.find(|c: char| c.is_whitespace() || c == '/' || c == '\'' || c == '"').unwrap_or(rest.len());
        if let Some(at) = rest[..end].rfind('@') {
            rest = &rest[at + 1..];
        }
    }
    out.push_str(rest);
    out
}

/// A directory git can run in: Windows' verbatim prefix (`\\?\`) stripped.
pub fn cwd(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(unc) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{unc}"))
    } else if let Some(plain) = s.strip_prefix(r"\\?\") {
        PathBuf::from(plain)
    } else {
        p.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_lose_their_credentials() {
        assert_eq!(redact_url("fatal: unable to access 'https://ana:s3cr3t@git.example.com/r.git/': x"), "fatal: unable to access 'https://git.example.com/r.git/': x");
        assert_eq!(redact_url("https://token@github.com/o/r"), "https://github.com/o/r");
        assert_eq!(redact_url("git@github.com:o/r.git"), "git@github.com:o/r.git");
        assert_eq!(redact_url("no url here"), "no url here");
    }

    #[test]
    fn errors_carry_a_hint() {
        assert!(matches!(map_error("fatal: Authentication failed for 'https://u:p@h/r'"), CommandError::AuthFailed(m) if !m.contains("u:p")));
        assert!(matches!(map_error("Host key verification failed."), CommandError::Connect(_)));
        assert!(matches!(map_error(" ! [rejected]        main -> main (fetch first)"), CommandError::BadRequest(m) if m.contains("Pull")));
        assert!(matches!(map_error("fatal: Could not resolve host: nope"), CommandError::Connect(_)));
        assert!(matches!(map_error("something else"), CommandError::BadRequest(m) if m == "something else"));
        assert!(matches!(remap(CommandError::BadRequest("operación cancelada".into())), CommandError::BadRequest(m) if m == "operación cancelada"));
    }

    #[test]
    fn verbatim_prefix_is_stripped() {
        assert_eq!(cwd(Path::new(r"\\?\C:\repos\x")), PathBuf::from(r"C:\repos\x"));
        assert_eq!(cwd(Path::new(r"\\?\UNC\srv\share")), PathBuf::from(r"\\srv\share"));
        assert_eq!(cwd(Path::new("/tmp/x")), PathBuf::from("/tmp/x"));
    }

    #[tokio::test]
    async fn a_cancelled_command_stops() {
        if git_path().is_err() {
            return;
        }
        let n = Arc::new(Notify::new());
        // Cancelled before it starts (the UI's cancel can arrive that early).
        n.notify_one();
        let err = output(&std::env::temp_dir(), &["--version"], RunOpts { cancel: Some(n), ..Default::default() }).await.err().unwrap();
        assert_eq!(err.to_string(), "operación cancelada");
        let lines = std::sync::Mutex::new(Vec::new());
        let cb = |l: &str| lines.lock().unwrap().push(l.to_string());
        // stderr lines reach the callback; stdout stays untrimmed with `raw`.
        let out = run_with(&std::env::temp_dir(), &["--version"], RunOpts { raw: true, progress: Some(&cb), ..Default::default() }).await.unwrap();
        assert!(out.ends_with(b"\n"));
    }
}
