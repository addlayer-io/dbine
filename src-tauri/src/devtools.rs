//! Debug builds only: a local test harness to drive the real UI from a
//! script, without screen-recording or accessibility permissions.
//!
//! `POST http://127.0.0.1:17999/eval` (port: `DBINE_DEV_PORT`) with a JavaScript body runs it in the
//! main window as the body of an async function; its return value (JSON)
//! is the response. `window.__dbineSnap()` (installed by the UI in dev)
//! returns a PNG data URL of the page. Never compiled into release builds.

use dashmap::DashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, OnceLock};
use std::time::Duration;
use tauri::{AppHandle, Manager};

/// `DBINE_DEV_PORT` picks another port (a second dev instance).
fn addr() -> String {
    format!("127.0.0.1:{}", std::env::var("DBINE_DEV_PORT").unwrap_or_else(|_| "17999".into()))
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

pub fn start(app: AppHandle) {
    let addr = addr();
    let listener = match TcpListener::bind(&addr) {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!(%e, "dev harness not started");
            return;
        }
    };
    tracing::info!("dev harness on http://{addr}/eval");
    let seq = AtomicU64::new(1);
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(s) => s,
                Err(_) => continue,
            });
            let mut len = 0usize;
            let mut line = String::new();
            // Request line + headers.
            while reader.read_line(&mut line).map(|n| n > 0).unwrap_or(false) {
                if line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
                line.clear();
            }
            let mut body = vec![0; len];
            if reader.read_exact(&mut body).is_err() {
                continue;
            }
            let js = String::from_utf8_lossy(&body).to_string();
            let id = seq.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = mpsc::channel();
            pending().insert(id, tx);
            let wrapped = format!(
                "(async () => {{ let r; try {{ r = await (async () => {{ {js} }})(); r = JSON.stringify({{ ok: true, value: r ?? null }}); }} \
                 catch (e) {{ r = JSON.stringify({{ ok: false, error: String(e && e.stack || e) }}); }} \
                 window.__TAURI_INTERNALS__.invoke('dev_report', {{ id: {id}, result: r }}); }})()"
            );
            let result = match app.get_webview_window("main").map(|w| w.eval(&wrapped)) {
                Some(Ok(())) => rx.recv_timeout(Duration::from_secs(60)).unwrap_or_else(|_| r#"{"ok":false,"error":"timeout"}"#.into()),
                _ => r#"{"ok":false,"error":"no window"}"#.into(),
            };
            pending().remove(&id);
            let mut out = stream;
            let _ = write!(
                out,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                result.len(),
                result
            );
        }
    });
}
