//! DBine's phone app (docs/mobile-companion.md). It keeps one thing: the
//! pairing with a computer (this phone's key, DBine's key, where to reach
//! it). Every request goes to that DBine over `dbine-link`'s encrypted
//! channel; no connection, host or password ever lives on the phone.

use dbine_link::proto::{Call, EventRow, Events, PairResult, Request, Response};
use dbine_link::{Channel, Invite, Keys};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

/// Connecting to one address waits at most this long.
const CONNECT_LIMIT: Duration = Duration::from_secs(5);
/// The user on the computer has this long to accept the phone (DBine waits
/// 2 minutes; a bit more here so its answer always arrives).
const PAIR_ANSWER: Duration = Duration::from_secs(135);
/// A request waits at most this long (DBine's own limit is 60 s).
const CALL_LIMIT: Duration = Duration::from_secs(70);
/// How often the app asks DBine for news (long queries that ended, task
/// runs) while it's running, in front or behind.
const NEWS_EVERY: Duration = Duration::from_secs(20);
/// Notices kept for the "Avisos" screen.
const NEWS_KEPT: usize = 50;

/// What the phone keeps (in its private app folder).
#[derive(Clone, Serialize, Deserialize)]
struct Pairing {
    phone: Keys,
    desktop_key: Vec<u8>,
    desktop: String,
    addrs: Vec<String>,
}

#[derive(Default)]
struct App {
    pairing: Option<Pairing>,
    channel: Option<Channel<TcpStream>>,
    next_id: u64,
    /// Where the phone is in DBine's list of notices.
    news_after: Option<u64>,
    news_boot: Option<String>,
    /// The latest notices, newest first.
    news: Vec<EventRow>,
}

type Shared = Mutex<App>;

fn file(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("pairing.json"))
}

fn load(app: &AppHandle) -> Option<Pairing> {
    let raw = std::fs::read(file(app).ok()?).ok()?;
    serde_json::from_slice(&raw).ok()
}

fn store(app: &AppHandle, p: &Pairing) -> Result<(), String> {
    let path = file(app)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(p).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// The first address that answers.
async fn reach(addrs: &[String]) -> Result<TcpStream, String> {
    let mut last = "no hay direcciones".to_string();
    for a in addrs {
        match tokio::time::timeout(CONNECT_LIMIT, TcpStream::connect(a)).await {
            Ok(Ok(s)) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Ok(Err(e)) => last = e.to_string(),
            Err(_) => last = "no respondió".into(),
        }
    }
    Err(format!(
        "no se pudo llegar a DBine ({last}). Revisá que DBine esté abierto, con «Permitir celulares» activo, y que el celular esté en la misma red."
    ))
}

#[derive(Serialize)]
struct Status {
    paired: bool,
    desktop: Option<String>,
    /// This phone's key, as DBine shows it.
    fingerprint: Option<String>,
}

#[tauri::command]
async fn status(state: State<'_, Shared>) -> Result<Status, String> {
    let s = state.lock().await;
    Ok(Status {
        paired: s.pairing.is_some(),
        desktop: s.pairing.as_ref().map(|p| p.desktop.clone()),
        fingerprint: s.pairing.as_ref().map(|p| dbine_link::fingerprint(&p.phone.public)),
    })
}

#[derive(Serialize, Clone)]
struct Waiting {
    desktop: String,
    fingerprint: String,
}

/// Pair with the DBine whose QR code was read. Emits `pair-waiting` with
/// this phone's fingerprint while the user confirms on the computer.
#[tauri::command]
async fn pair(app: AppHandle, state: State<'_, Shared>, code: String, name: String) -> Result<String, String> {
    let invite = Invite::decode(&code)?;
    let phone = Keys::generate()?;
    let stream = reach(&invite.addrs).await?;
    let name: String = name.trim().chars().take(60).collect();
    let mut ch = dbine_link::pair(stream, &invite, &phone, name.as_bytes())
        .await
        .map_err(|_| "DBine no aceptó el código: puede haber vencido. Generá uno nuevo en la computadora.".to_string())?;
    ch.send(b"ready").await?;
    let _ = app.emit("pair-waiting", Waiting { desktop: invite.name.clone(), fingerprint: dbine_link::fingerprint(&phone.public) });
    let result: PairResult = tokio::time::timeout(PAIR_ANSWER, ch.recv_json())
        .await
        .map_err(|_| "la computadora no respondió a tiempo".to_string())??;
    if !result.accepted {
        return Err("se rechazó la vinculación en la computadora".into());
    }
    let p = Pairing { phone, desktop_key: invite.key.clone(), desktop: result.desktop.clone(), addrs: invite.addrs.clone() };
    store(&app, &p)?;
    let mut s = state.lock().await;
    s.pairing = Some(p);
    s.channel = None;
    Ok(result.desktop)
}

#[tauri::command]
async fn unpair(app: AppHandle, state: State<'_, Shared>) -> Result<(), String> {
    let mut s = state.lock().await;
    s.pairing = None;
    s.channel = None;
    let _ = std::fs::remove_file(file(&app)?);
    Ok(())
}

async fn ask(s: &mut App, call: &Call) -> Result<Response, String> {
    let pairing = s.pairing.clone().ok_or("este celular no está vinculado")?;
    if s.channel.is_none() {
        let stream = reach(&pairing.addrs).await?;
        let ch = dbine_link::connect(stream, &pairing.desktop_key, &pairing.phone)
            .await
            .map_err(|_| "DBine rechazó la conexión: puede que hayan quitado este celular. Vinculalo de nuevo.".to_string())?;
        s.channel = Some(ch);
    }
    s.next_id += 1;
    let id = s.next_id;
    let ch = s.channel.as_mut().expect("just opened");
    ch.send_json(&Request { id, call: call.clone() }).await?;
    loop {
        let r: Response = ch.recv_json().await?;
        if r.id == id {
            return Ok(r);
        }
    }
}

/// One request to DBine. A dropped channel is reopened once, except for a
/// kill: that one is never sent twice.
#[tauri::command]
async fn call(state: State<'_, Shared>, call: Call) -> Result<serde_json::Value, String> {
    let mut s = state.lock().await;
    let once = matches!(call, Call::Kill { .. });
    let first = tokio::time::timeout(CALL_LIMIT, ask(&mut s, &call)).await.unwrap_or_else(|_| Err("DBine no respondió".into()));
    let r = match first {
        Ok(r) => r,
        Err(e) => {
            s.channel = None;
            if once {
                return Err(format!("{e}. No se reintentó: revisá en DBine si la sesión terminó."));
            }
            tokio::time::timeout(CALL_LIMIT, ask(&mut s, &call)).await.unwrap_or_else(|_| Err("DBine no respondió".into())).inspect_err(|_| s.channel = None)?
        }
    };
    match (r.ok, r.error) {
        (_, Some(e)) => Err(e),
        (Some(v), None) => Ok(v),
        (None, None) => Ok(serde_json::Value::Null),
    }
}

/// The notices received while the app runs, newest first.
#[tauri::command]
async fn news(state: State<'_, Shared>) -> Result<Vec<EventRow>, String> {
    Ok(state.lock().await.news.clone())
}

/// Ask DBine what's new and turn it into notifications. The first look
/// after starting (or after DBine restarted) only learns where the list is:
/// old news isn't news.
async fn check_news(app: &AppHandle) {
    use tauri_plugin_notification::NotificationExt;
    let state = app.state::<Shared>();
    let mut s = state.lock().await;
    if s.pairing.is_none() {
        return;
    }
    let call = Call::Events { after: s.news_after, boot: s.news_boot.clone() };
    let r = match tokio::time::timeout(CALL_LIMIT, ask(&mut s, &call)).await {
        Ok(Ok(r)) => r,
        _ => {
            s.channel = None;
            return;
        }
    };
    let Some(ev) = r.ok.and_then(|v| serde_json::from_value::<Events>(v).ok()) else { return };
    let fresh = s.news_boot.as_deref() == Some(ev.boot.as_str());
    s.news_after = Some(ev.last);
    s.news_boot = Some(ev.boot);
    if !fresh {
        return;
    }
    for e in &ev.events {
        let _ = app.notification().builder().title(&e.title).body(&e.body).show();
    }
    let mut all: Vec<EventRow> = ev.events.into_iter().rev().collect();
    all.append(&mut s.news);
    all.truncate(NEWS_KEPT);
    s.news = all;
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    #[cfg(mobile)]
    let builder = builder.plugin(tauri_plugin_barcode_scanner::init()).plugin(tauri_plugin_biometric::init());
    builder
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let pairing = load(app.handle());
            app.manage::<Shared>(Mutex::new(App { pairing, ..Default::default() }));
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                // Android 13 and later ask the user once.
                {
                    use tauri_plugin_notification::{NotificationExt, PermissionState};
                    if !matches!(handle.notification().permission_state(), Ok(PermissionState::Granted)) {
                        let _ = handle.notification().request_permission();
                    }
                }
                loop {
                    check_news(&handle).await;
                    tokio::time::sleep(NEWS_EVERY).await;
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![status, pair, unpair, call, news])
        .run(tauri::generate_context!())
        .expect("error while running DBine mobile");
}
