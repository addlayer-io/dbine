//! Debug builds only: a local test harness to drive the real UI from a
//! script, without screen-recording or accessibility permissions.
//!
//! `POST http://127.0.0.1:17999/eval` (port: `DBINE_DEV_PORT`) with a
//! JavaScript body runs it in the main window (`/eval?label=win-2`: another
//! window; with no `main`, the target window) as the body of an async
//! function; its return value (JSON) is the response. `window.__dbineSnap()`
//! (installed by the UI in dev) returns a PNG data URL of the page. Never
//! compiled into release builds.
//!
//! The harness runs arbitrary code with full IPC, so it is authenticated:
//!
//! - Each start writes a fresh random token (64 hex chars) to
//!   `<temp dir>/dbine-devtools-<port>.token` — `$TMPDIR` on macOS,
//!   `%TEMP%` on Windows, `$TMPDIR` or `/tmp` on Linux (Rust's
//!   `std::env::temp_dir()`). The previous file is removed first and the new
//!   one is created exclusively with mode 0600, so only the current user can
//!   read it. If it can't be written, the harness doesn't start.
//! - Every request must send it in `X-DBine-Devtools-Token`.
//! - Only `POST /eval` is accepted; `Host` must be `127.0.0.1:<port>` or
//!   `localhost:<port>`; any `Origin` header (a browser page) is refused;
//!   the body needs a `Content-Length` of at most 1 MiB and is read with a
//!   timeout.
//!
//! ```sh
//! curl -s http://127.0.0.1:17999/eval \
//!   -H "X-DBine-Devtools-Token: $(cat "${TMPDIR:-/tmp}/dbine-devtools-17999.token")" \
//!   --data-binary 'return document.title'
//! ```

use dashmap::DashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Manager};

/// The header that carries the per-run token.
pub const TOKEN_HEADER: &str = "x-dbine-devtools-token";
/// Largest accepted body.
const MAX_BODY: usize = 1024 * 1024;
/// Largest accepted request line + headers.
const MAX_HEAD: u64 = 16 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(10);

/// `DBINE_DEV_PORT` picks another port (a second dev instance).
fn port() -> u16 {
    std::env::var("DBINE_DEV_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(17999)
}

/// Where the token for `port` is written.
pub fn token_path(port: u16) -> PathBuf {
    std::env::temp_dir().join(format!("dbine-devtools-{port}.token"))
}

fn new_token() -> std::io::Result<String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Replaces the token file: the old one is removed and the new one created
/// exclusively (no following a planted symlink), readable by the owner only.
fn write_token(path: &std::path::Path, token: &str) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(token.as_bytes())
}

/// Compares without an early exit, so timing doesn't reveal the prefix.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A request that passed every check.
#[derive(Debug, PartialEq)]
struct Request {
    label: Option<String>,
    body: Vec<u8>,
}

/// Why a request was refused: an HTTP status and a short reason.
#[derive(Debug, PartialEq)]
struct Reject(u16, &'static str);

/// Reads and validates one request. Nothing in the body is read until the
/// request line and every header have been checked.
fn read_request<R: BufRead>(reader: &mut R, port: u16, token: &str) -> Result<Request, Reject> {
    let mut head = (&mut *reader).take(MAX_HEAD);
    let mut line = String::new();
    let mut read_line = |line: &mut String| -> Result<(), Reject> {
        line.clear();
        match head.read_line(line) {
            Ok(0) => Err(Reject(400, "incomplete request")),
            Ok(_) if !line.ends_with('\n') => Err(Reject(431, "request head too large")),
            Ok(_) => Ok(()),
            Err(_) => Err(Reject(400, "unreadable request")),
        }
    };

    read_line(&mut line)?;
    let mut parts = line.trim_end().split(' ');
    let (method, target, version) = (parts.next(), parts.next(), parts.next());
    if parts.next().is_some() || !version.is_some_and(|v| v.starts_with("HTTP/1.")) {
        return Err(Reject(400, "bad request line"));
    }
    let target = target.unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != "/eval" {
        return Err(Reject(404, "not found"));
    }
    if method != Some("POST") {
        return Err(Reject(405, "method not allowed"));
    }
    let label = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("label="))
        .map(str::to_string);

    let mut len: Option<usize> = None;
    let mut host: Option<String> = None;
    let mut authed = false;
    loop {
        read_line(&mut line)?;
        if line == "\r\n" || line == "\n" {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(Reject(400, "bad header"));
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "origin" => return Err(Reject(403, "browser requests are not accepted")),
            "transfer-encoding" => return Err(Reject(411, "content-length required")),
            "content-length" => {
                if len.is_some() {
                    return Err(Reject(400, "duplicate content-length"));
                }
                len = Some(value.parse().map_err(|_| Reject(400, "bad content-length"))?);
            }
            "host" => {
                if host.is_some() {
                    return Err(Reject(400, "duplicate host"));
                }
                host = Some(value.to_ascii_lowercase());
            }
            TOKEN_HEADER => authed = ct_eq(value.as_bytes(), token.as_bytes()),
            _ => {}
        }
    }

    let host_ok = host.is_some_and(|h| h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}"));
    if !host_ok {
        return Err(Reject(403, "bad host"));
    }
    if !authed {
        return Err(Reject(403, "missing or wrong token"));
    }
    let len = len.ok_or(Reject(411, "content-length required"))?;
    if len > MAX_BODY {
        return Err(Reject(413, "body too large"));
    }
    let mut body = vec![0; len];
    reader.read_exact(&mut body).map_err(|_| Reject(400, "incomplete body"))?;
    Ok(Request { label, body })
}

type Pending = DashMap<u64, mpsc::Sender<String>>;

fn pending() -> &'static Arc<Pending> {
    static P: OnceLock<Arc<Pending>> = OnceLock::new();
    P.get_or_init(|| Arc::new(DashMap::new()))
}

/// The page reports an eval's result.
#[tauri::command]
pub async fn dev_report(id: u64, result: String) {
    if let Some((_, tx)) = pending().remove(&id) {
        let _ = tx.send(result);
    }
}

fn respond(mut out: &TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    };
    let _ = write!(
        out,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
}

pub fn start(app: AppHandle) {
    let port = port();
    let path = token_path(port);
    let token = match new_token().and_then(|t| write_token(&path, &t).map(|()| t)) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(%e, path = %path.display(), "dev harness not started: no token file");
            return;
        }
    };
    let addr = format!("127.0.0.1:{port}");
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(%e, "dev harness not started");
            return;
        }
    };
    tracing::info!("dev harness on http://{addr}/eval (token in {})", path.display());
    let seq = AtomicU64::new(1);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
            let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(s) => s,
                Err(_) => continue,
            });
            let Request { label, body } = match read_request(&mut reader, port, &token) {
                Ok(r) => r,
                Err(Reject(status, why)) => {
                    let msg = serde_json::json!({ "ok": false, "error": why }).to_string();
                    respond(&stream, status, &msg);
                    continue;
                }
            };
            let js = String::from_utf8_lossy(&body).to_string();
            let id = seq.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = mpsc::channel();
            pending().insert(id, tx);
            let wrapped = format!(
                "(async () => {{ let r; try {{ r = await (async () => {{ {js} }})(); r = JSON.stringify({{ ok: true, value: r ?? null }}); }} \
                 catch (e) {{ r = JSON.stringify({{ ok: false, error: String(e && e.stack || e) }}); }} \
                 window.__TAURI_INTERNALS__.invoke('dev_report', {{ id: {id}, result: r }}); }})()"
            );
            let window = match label {
                Some(l) => app.get_webview_window(&l),
                None => app.get_webview_window(crate::windows::MAIN).or_else(|| crate::windows::target_window(&app)),
            };
            let result = match window.map(|w| w.eval(&wrapped)) {
                Some(Ok(())) => rx.recv_timeout(Duration::from_secs(60)).unwrap_or_else(|_| r#"{"ok":false,"error":"timeout"}"#.into()),
                _ => r#"{"ok":false,"error":"no window"}"#.into(),
            };
            pending().remove(&id);
            respond(&stream, 200, &result);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "abc123";
    const PORT: u16 = 17999;

    fn req(raw: &str) -> Result<Request, Reject> {
        read_request(&mut std::io::Cursor::new(raw.as_bytes().to_vec()), PORT, TOKEN)
    }

    fn ok_head(extra: &str, body: &str) -> String {
        format!(
            "POST /eval HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\nX-DBine-Devtools-Token: {TOKEN}\r\n{extra}Content-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn accepts_a_valid_request() {
        let r = req(&ok_head("", "return 1")).unwrap();
        assert_eq!(r, Request { label: None, body: b"return 1".to_vec() });
    }

    #[test]
    fn reads_the_window_label() {
        let raw = ok_head("", "1").replacen("/eval", "/eval?x=1&label=win-2", 1);
        assert_eq!(req(&raw).unwrap().label.as_deref(), Some("win-2"));
    }

    #[test]
    fn accepts_localhost_host() {
        let raw = ok_head("", "1").replace("127.0.0.1:", "localhost:");
        assert!(req(&raw).is_ok());
    }

    #[test]
    fn refuses_without_or_with_a_wrong_token() {
        let missing = ok_head("", "1").replace(&format!("X-DBine-Devtools-Token: {TOKEN}\r\n"), "");
        assert_eq!(req(&missing), Err(Reject(403, "missing or wrong token")));
        let wrong = ok_head("", "1").replace(TOKEN, "abc124");
        assert_eq!(req(&wrong), Err(Reject(403, "missing or wrong token")));
        let prefix = ok_head("", "1").replace(TOKEN, "abc");
        assert_eq!(req(&prefix), Err(Reject(403, "missing or wrong token")));
    }

    #[test]
    fn refuses_browser_origins() {
        assert_eq!(req(&ok_head("Origin: null\r\n", "1")).unwrap_err().0, 403);
        assert_eq!(req(&ok_head("Origin: https://evil.example\r\n", "1")).unwrap_err().0, 403);
    }

    #[test]
    fn refuses_other_hosts() {
        let rebound = ok_head("", "1").replace(&format!("127.0.0.1:{PORT}"), &format!("evil.example:{PORT}"));
        assert_eq!(req(&rebound), Err(Reject(403, "bad host")));
        let other_port = ok_head("", "1").replace(&format!("127.0.0.1:{PORT}"), "127.0.0.1:80");
        assert_eq!(req(&other_port), Err(Reject(403, "bad host")));
        let none = ok_head("", "1").replace(&format!("Host: 127.0.0.1:{PORT}\r\n"), "");
        assert_eq!(req(&none), Err(Reject(403, "bad host")));
    }

    #[test]
    fn refuses_other_methods_and_paths() {
        assert_eq!(req(&ok_head("", "1").replacen("POST", "GET", 1)).unwrap_err().0, 405);
        assert_eq!(req(&ok_head("", "1").replacen("/eval", "/", 1)).unwrap_err().0, 404);
        assert_eq!(req(&ok_head("", "1").replacen("/eval", "/evalx", 1)).unwrap_err().0, 404);
    }

    #[test]
    fn caps_the_body() {
        let raw = format!(
            "POST /eval HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\nX-DBine-Devtools-Token: {TOKEN}\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        );
        assert_eq!(req(&raw), Err(Reject(413, "body too large")));
        let huge = raw.replace(&(MAX_BODY + 1).to_string(), "99999999999999999999999");
        assert_eq!(req(&huge).unwrap_err().0, 400);
    }

    #[test]
    fn requires_a_single_content_length() {
        let none = ok_head("", "").replace("Content-Length: 0\r\n", "");
        assert_eq!(req(&none).unwrap_err().0, 411);
        assert_eq!(req(&ok_head("Content-Length: 1\r\n", "1")).unwrap_err().0, 400);
        assert_eq!(req(&ok_head("Transfer-Encoding: chunked\r\n", "1")).unwrap_err().0, 411);
    }

    #[test]
    fn refuses_a_short_body_and_an_oversized_head() {
        let short = ok_head("", "12345").replace("12345", "12");
        assert_eq!(req(&short).unwrap_err().0, 400);
        let big = ok_head(&format!("X-Pad: {}\r\n", "a".repeat(MAX_HEAD as usize)), "1");
        assert_eq!(req(&big).unwrap_err().0, 431);
    }

    #[test]
    fn token_file_is_fresh_and_private() {
        let path = std::env::temp_dir().join(format!("dbine-devtools-test-{}.token", std::process::id()));
        write_token(&path, "first").unwrap();
        write_token(&path, "second").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_file(&path).unwrap();
        let t = new_token().unwrap();
        assert_eq!(t.len(), 64);
        assert_ne!(t, new_token().unwrap());
    }

    #[test]
    fn constant_time_eq() {
        assert!(ct_eq(b"abc", b"abc"));
        assert!(!ct_eq(b"abc", b"abd"));
        assert!(!ct_eq(b"abc", b"ab"));
    }
}
