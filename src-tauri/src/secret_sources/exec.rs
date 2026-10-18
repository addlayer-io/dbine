//! Running the programs a secret comes from: the user's command (through
//! their login shell) and the 1Password CLI (no shell). No stdin, a time
//! limit that kills the whole process group, the output capped; stdout is
//! the secret and is never logged or put in an error, only stderr is.

use dbine_core::secret_sources::wipe_bytes;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// How long a command may take.
pub const COMMAND_LIMIT: Duration = Duration::from_secs(30);
/// The secret can't be longer than this (a key file is a few KB).
const MAX_STDOUT: u64 = 64 * 1024;
/// stderr kept for the error message.
const MAX_STDERR: u64 = 16 * 1024;
/// stderr shown in the error.
const STDERR_SHOWN: usize = 300;

/// How a run ended when it didn't print a secret.
#[derive(Debug, PartialEq)]
pub enum Failure {
    /// The program couldn't start (not found, not executable).
    Spawn(String),
    /// Killed at the limit.
    Timeout(Duration),
    /// Exit status not zero: the code and stderr (first lines, cut).
    Exit { code: Option<i32>, stderr: String },
    /// More than `MAX_STDOUT` bytes.
    TooLong,
    /// Not UTF-8.
    NotText,
}

/// The user's login shell: `$SHELL` when it's an absolute path, else the
/// system's default.
#[cfg(unix)]
pub fn login_shell() -> PathBuf {
    match std::env::var_os("SHELL").map(PathBuf::from) {
        Some(p) if p.is_absolute() && p.is_file() => p,
        _ if cfg!(target_os = "macos") => PathBuf::from("/bin/zsh"),
        _ => PathBuf::from("/bin/sh"),
    }
}

/// The command line, run by the login shell (`-l`: its profile sets `PATH`,
/// so `op`, `vault`, `pass`… are found as in a terminal).
pub fn shell_command(command: &str) -> tokio::process::Command {
    #[cfg(unix)]
    {
        let mut c = tokio::process::Command::new(login_shell());
        c.arg("-l").arg("-c").arg(command);
        c
    }
    #[cfg(windows)]
    {
        // cmd.exe takes the line as is after `/s /c "…"` (outer quotes
        // stripped); `/d` skips AutoRun.
        let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let mut c = tokio::process::Command::new(root.join("System32").join("cmd.exe"));
        c.raw_arg(format!("/d /s /c \"{command}\""));
        c
    }
}

#[cfg(unix)]
fn kill_group(pid: u32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const SIGKILL: i32 = 9;
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: plain syscall; a negative pid names the process group the
        // child leads (`process_group(0)`), so grandchildren go too.
        unsafe {
            kill(-pid, SIGKILL);
        }
    }
}

/// Run `cmd` and return its stdout (the caller wipes it). The child gets no
/// stdin and its own process group, killed whole at `limit`.
pub async fn run(mut cmd: tokio::process::Command, limit: Duration) -> Result<String, Failure> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let mut child = cmd.spawn().map_err(|e| Failure::Spawn(e.to_string()))?;
    #[cfg(unix)]
    let pid = child.id();
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let work = async {
        let (mut o, mut e) = ((&mut stdout).take(MAX_STDOUT + 1), (&mut stderr).take(MAX_STDERR));
        let (a, b) = tokio::join!(o.read_to_end(&mut out), e.read_to_end(&mut err));
        let status = child.wait().await;
        (a, b, status)
    };
    let finished = tokio::time::timeout(limit, work).await;
    let Ok((read, _, status)) = finished else {
        #[cfg(unix)]
        if let Some(pid) = pid {
            kill_group(pid);
        }
        // Not reaped yet, so the group id is still the child's.
        let too_long = out.len() as u64 > MAX_STDOUT;
        wipe_bytes(&mut out);
        return Err(if too_long { Failure::TooLong } else { Failure::Timeout(limit) });
    };
    let status = status.map_err(|e| Failure::Spawn(e.to_string()))?;
    if read.is_err() {
        wipe_bytes(&mut out);
        return Err(Failure::NotText);
    }
    if !status.success() {
        wipe_bytes(&mut out);
        return Err(Failure::Exit { code: status.code(), stderr: first_lines(&String::from_utf8_lossy(&err), STDERR_SHOWN) });
    }
    if out.len() as u64 > MAX_STDOUT {
        wipe_bytes(&mut out);
        return Err(Failure::TooLong);
    }
    match String::from_utf8(out) {
        Ok(s) => Ok(s),
        Err(e) => {
            let mut v = e.into_bytes();
            wipe_bytes(&mut v);
            Err(Failure::NotText)
        }
    }
}

/// stderr for a message: trimmed, lines joined, at most `max` characters.
pub fn first_lines(s: &str, max: usize) -> String {
    let joined = s.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" · ");
    match joined.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &joined[..i]),
        None => joined,
    }
}

/// The output without the trailing line break(s) a command prints.
pub fn trim_newline(mut s: String) -> String {
    let keep = s.trim_end_matches(['\n', '\r']).len();
    // Zero the cut part before it's released.
    // SAFETY: only ASCII line breaks are overwritten, with zeros.
    for b in unsafe { &mut s.as_bytes_mut()[keep..] } {
        *b = 0;
    }
    s.truncate(keep);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newlines_and_stderr() {
        assert_eq!(trim_newline("s3cr3t\n".into()), "s3cr3t");
        assert_eq!(trim_newline("s3cr3t\r\n\n".into()), "s3cr3t");
        assert_eq!(trim_newline(" with spaces \n".into()), " with spaces ", "only the line break goes");
        assert_eq!(first_lines("\n  error: one\n\n two  \n", 100), "error: one · two");
        assert_eq!(first_lines(&"x".repeat(500), 10), format!("{}…", "x".repeat(10)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_exit_codes_and_timeouts() {
        let ok = run(shell_command("printf 'hunter2\\n'"), COMMAND_LIMIT).await.unwrap();
        assert_eq!(trim_newline(ok), "hunter2");
        // A failure shows stderr, never stdout.
        let e = run(shell_command("echo the-secret; echo 'vault: permission denied' >&2; exit 3"), COMMAND_LIMIT).await.unwrap_err();
        match e {
            Failure::Exit { code, stderr } => {
                assert_eq!(code, Some(3));
                assert_eq!(stderr, "vault: permission denied");
            }
            other => panic!("{other:?}"),
        }
        // No stdin: a command that reads it gets EOF at once.
        let e = run(shell_command("read x && echo got-$x"), COMMAND_LIMIT).await;
        assert!(matches!(e, Err(Failure::Exit { code: Some(1), .. })), "{e:?}");
        // The limit kills it, grandchildren included.
        let marker = std::env::temp_dir().join(format!("dbine-secret-timeout-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let started = std::time::Instant::now();
        let line = format!("(sleep 3; touch '{}') & sleep 20", marker.display());
        let e = run(shell_command(&line), Duration::from_millis(500)).await.unwrap_err();
        assert_eq!(e, Failure::Timeout(Duration::from_millis(500)));
        assert!(started.elapsed() < Duration::from_secs(3), "returned at the limit");
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(!marker.exists(), "the background child was killed with the group");
        // Too much output.
        let e = run(shell_command("head -c 70000 /dev/zero | tr '\\0' a"), COMMAND_LIMIT).await.unwrap_err();
        assert_eq!(e, Failure::TooLong);
        // A program that doesn't exist.
        let e = run(tokio::process::Command::new("/nonexistent/dbine-op"), COMMAND_LIMIT).await.unwrap_err();
        assert!(matches!(e, Failure::Spawn(_)));
    }
}
