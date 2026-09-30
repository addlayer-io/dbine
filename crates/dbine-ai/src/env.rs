//! Finding command-line tools the way the user's terminal does. An app
//! opened from the Finder / Start menu gets a bare PATH (no Homebrew, no
//! npm globals, no mise/asdf/nvm shims), so the login shell's PATH is read
//! once and added to the usual install folders.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

static SEARCH_PATH: OnceLock<String> = OnceLock::new();

#[cfg(windows)]
const SEP: char = ';';
#[cfg(not(windows))]
const SEP: char = ':';

fn home() -> PathBuf {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from).unwrap_or_default()
}

/// Folders where these tools usually end up.
fn usual_dirs() -> Vec<PathBuf> {
    let h = home();
    let mut v: Vec<PathBuf> = if cfg!(windows) {
        let appdata = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_default();
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from).unwrap_or_default();
        vec![appdata.join("npm"), local.join("Programs").join("Ollama"), h.join(".local").join("bin"), h.join(".bun").join("bin")]
    } else {
        vec![
            "/opt/homebrew/bin".into(),
            "/usr/local/bin".into(),
            "/usr/bin".into(),
            h.join(".local/bin"),
            h.join(".claude/local"),
            h.join(".npm-global/bin"),
            h.join(".bun/bin"),
            h.join(".volta/bin"),
            h.join(".local/share/mise/shims"),
            h.join(".asdf/shims"),
            h.join("Library/pnpm"),
            "/Applications/Ollama.app/Contents/Resources".into(),
        ]
    };
    // nvm's current node versions.
    if let Ok(rd) = std::fs::read_dir(h.join(".nvm/versions/node")) {
        v.extend(rd.flatten().map(|e| e.path().join("bin")));
    }
    v
}

/// The login shell's PATH (macOS / Linux), within a few seconds.
fn shell_path() -> Option<String> {
    if cfg!(windows) {
        return None;
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    // -i as well: version managers are often activated in .zshrc / .bashrc.
    for flags in ["-ilc", "-lc"] {
        let mut child = std::process::Command::new(&shell)
            .args([flags, "printf '__DBINE_PATH__%s__END__' \"$PATH\""])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(30)),
                _ => {
                    let _ = child.kill();
                    break;
                }
            }
        }
        let out = child.wait_with_output().ok()?;
        let s = String::from_utf8_lossy(&out.stdout);
        if let Some(p) = s.split("__DBINE_PATH__").nth(1).and_then(|r| r.split("__END__").next()) {
            if !p.is_empty() {
                return Some(p.to_string());
            }
        }
    }
    None
}

/// PATH for finding and running the tools: the app's own, the login
/// shell's and the usual folders (computed once).
pub fn search_path() -> &'static str {
    SEARCH_PATH.get_or_init(|| {
        let mut parts: Vec<String> = Vec::new();
        let mut add = |p: &str| {
            if !p.is_empty() && !parts.iter().any(|x| x == p) {
                parts.push(p.to_string());
            }
        };
        if let Some(sp) = shell_path() {
            sp.split(SEP).for_each(&mut add);
        }
        if let Ok(own) = std::env::var("PATH") {
            own.split(SEP).for_each(&mut add);
        }
        for d in usual_dirs() {
            add(&d.to_string_lossy());
        }
        parts.join(&SEP.to_string())
    })
}

fn is_exec(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata().map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// Where `name` is installed, if it is.
pub fn find(name: &str) -> Option<PathBuf> {
    find_in(name, search_path())
}

pub fn find_in(name: &str, path: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat", ""] } else { &[""] };
    path.split(SEP).filter(|d| !d.is_empty()).find_map(|dir| {
        exts.iter().map(|e| Path::new(dir).join(format!("{name}{e}"))).find(|p| is_exec(p))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn finds_executables_only() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let tool = d.path().join("herramienta");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        let path = format!("/nonexistent:{}", d.path().display());
        assert!(find_in("herramienta", &path).is_none(), "not executable yet");
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_in("herramienta", &path).unwrap(), tool);
        assert!(find_in("otra", &path).is_none());
    }
}
