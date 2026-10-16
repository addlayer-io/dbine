//! DBine's phone app (docs/mobile-companion.md). It keeps one thing: the
//! pairing with a computer (this phone's key, DBine's key, where to reach
//! it). Every request goes to that DBine over `dbine-link`'s encrypted
//! channel; no connection, host or password ever lives on the phone.

use dbine_link::net::Relays;
use dbine_link::proto::{Call, EventRow, Events, Hello, PairResult, Request, Response};
use dbine_link::{BoxIo, Channel, Invite, Keys, Remote};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::net::TcpStream;
use tokio::sync::Mutex;

/// Connecting to one local address waits at most this long (then the app
/// tries through the internet).
const CONNECT_LIMIT: Duration = Duration::from_secs(2);
/// The user on the computer has this long to accept the phone (DBine waits
/// 2 minutes; a bit more here so its answer always arrives).
const PAIR_ANSWER: Duration = Duration::from_secs(135);
/// A request waits at most this long (DBine's own limit is 60 s).
const CALL_LIMIT: Duration = Duration::from_secs(70);
/// How often the app asks DBine for news (long queries that ended, task
/// runs) while it's running, in front or behind.
const NEWS_EVERY: Duration = Duration::from_secs(20);


/// What the phone keeps (in its private app folder).
#[derive(Clone, Serialize, Deserialize)]
struct Pairing {
    phone: Keys,
    desktop_key: Vec<u8>,
    desktop: String,
    addrs: Vec<String>,
    /// How to reach DBine from outside the local network (absent: only the
    /// local network). DBine keeps it up to date on every connection.
    #[serde(default)]
    remote: Option<Remote>,
}

/// A pairing link that opened the app (https://dbine.com/pair#…), until the
/// UI takes it.
static LINK: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[cfg_attr(not(mobile), allow(dead_code))]
fn take_link(url: &str, app: &AppHandle) {
    if url.starts_with(dbine_link::INVITE_LINK) {
        *LINK.lock().unwrap() = Some(url.to_string());
        let _ = app.emit("pair-link", ());
    }
}

/// The pairing link that opened the app, once.
#[tauri::command]
fn pending_link() -> Option<String> {
    LINK.lock().unwrap().take()
}

/// The computer a pairing link points to, to show before pairing.
#[tauri::command]
fn invite_name(code: String) -> Result<String, String> {
    Ok(Invite::decode(&code)?.name)
}

/// Open a newer version's page in the browser: DBine's GitHub releases only.
#[tauri::command]
fn open_release(app: AppHandle, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    if !url.starts_with("https://github.com/addlayer-io/dbine/releases/") {
        return Err("solo se abren las versiones de DBine".into());
    }
    app.opener().open_url(url, None::<&str>).map_err(|e| e.to_string())
}

#[derive(Default)]
struct App {
    pairing: Option<Pairing>,
    channel: Option<Channel<BoxIo>>,
    next_id: u64,
    /// How the open channel got there: "local" or "internet".
    via: Option<&'static str>,
}

/// The phone's iroh endpoint, made on first use and kept for the relays it
/// was made for.
static IROH: tokio::sync::Mutex<Option<(Relays, dbine_link::net::Endpoint)>> = tokio::sync::Mutex::const_new(None);

async fn iroh_endpoint(relays: &Relays) -> Result<dbine_link::net::Endpoint, String> {
    let mut slot = IROH.lock().await;
    if let Some((r, ep)) = slot.as_ref() {
        if r == relays {
            return Ok(ep.clone());
        }
    }
    let ep = dbine_link::net::endpoint(dbine_link::net::new_secret(), relays, false).await?;
    *slot = Some((relays.clone(), ep.clone()));
    Ok(ep)
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

/// DBine on the local network (fast), else through the internet (iroh).
async fn reach(addrs: &[String], remote: Option<&Remote>) -> Result<(BoxIo, &'static str), String> {
    let mut last = "no hay direcciones".to_string();
    for a in addrs {
        match tokio::time::timeout(CONNECT_LIMIT, TcpStream::connect(a)).await {
            Ok(Ok(s)) => {
                let _ = s.set_nodelay(true);
                return Ok((Box::new(s), "local"));
            }
            Ok(Err(e)) => last = e.to_string(),
            Err(_) => last = "no respondió".into(),
        }
    }
    if let Some(r) = remote {
        let ep = iroh_endpoint(&Relays::from_urls(&r.relays)).await?;
        return match dbine_link::net::dial(&ep, r).await {
            Ok(s) => Ok((Box::new(s), "internet")),
            Err(e) => Err(format!("no se pudo llegar a DBine ni en la red local ni por internet ({}). Revisá que DBine esté abierto, con «Permitir celulares» activo.", e)),
        };
    }
    Err(format!(
        "no se pudo llegar a DBine ({}). Revisá que DBine esté abierto, con «Permitir celulares» activo, y que el celular esté en la misma red.",
        last
    ))
}

#[derive(Serialize)]
struct Status {
    paired: bool,
    desktop: Option<String>,
    /// This phone's key, as DBine shows it.
    fingerprint: Option<String>,
    /// How the app reached DBine last: "local", "internet" or none yet.
    via: Option<&'static str>,
}

#[tauri::command]
async fn status(state: State<'_, Shared>) -> Result<Status, String> {
    let s = state.lock().await;
    Ok(Status {
        paired: s.pairing.is_some(),
        desktop: s.pairing.as_ref().map(|p| p.desktop.clone()),
        fingerprint: s.pairing.as_ref().map(|p| dbine_link::fingerprint(&p.phone.public)),
        via: s.via,
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
    let (stream, _) = reach(&invite.addrs, invite.remote.as_ref()).await?;
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
    let p = Pairing { phone, desktop_key: invite.key.clone(), desktop: result.desktop.clone(), addrs: invite.addrs.clone(), remote: invite.remote.clone() };
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
    let f = file(&app)?;
    let _ = std::fs::remove_file(f.with_file_name("news.json"));
    let _ = std::fs::remove_file(f);
    Ok(())
}

async fn ask(s: &mut App, call: &Call) -> Result<Response, String> {
    let pairing = s.pairing.clone().ok_or("este celular no está vinculado")?;
    if s.channel.is_none() {
        let (stream, via) = reach(&pairing.addrs, pairing.remote.as_ref()).await?;
        let ch = dbine_link::connect(stream, &pairing.desktop_key, &pairing.phone)
            .await
            .map_err(|_| "DBine rechazó la conexión: puede que hayan quitado este celular. Vinculalo de nuevo.".to_string())?;
        s.channel = Some(ch);
        s.via = Some(via);
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
async fn call(app: AppHandle, state: State<'_, Shared>, call: Call) -> Result<serde_json::Value, String> {
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
    // DBine says how to reach it from outside now (its relays may have
    // changed since pairing): keep it.
    if let (Call::Hello, Some(v)) = (&call, &r.ok) {
        if let Ok(h) = serde_json::from_value::<Hello>(v.clone()) {
            if let Some(p) = s.pairing.as_mut() {
                if h.remote.is_some() && p.remote != h.remote {
                    p.remote = h.remote;
                    let p = p.clone();
                    let _ = store(&app, &p);
                }
            }
        }
    }
    match (r.ok, r.error) {
        (_, Some(e)) => Err(e),
        (Some(v), None) => Ok(v),
        (None, None) => Ok(serde_json::Value::Null),
    }
}

/// Where the phone is in DBine's list of notices, kept on disk: the app's
/// loop and Android's periodic check (with the app closed) share it, so a
/// notice arrives once and none is lost when the app restarts.
#[derive(Default, Serialize, Deserialize)]
struct Position {
    after: Option<u64>,
    boot: Option<String>,
}

/// One look at DBine's notices, on a connection of its own: what's new
/// since the saved position (none the very first time: old news isn't
/// news). Used by the app and by `NewsWorker` (Android, app closed).
async fn news_from(dir: &std::path::Path) -> Result<Vec<EventRow>, String> {
    let pairing: Pairing = serde_json::from_slice(&std::fs::read(dir.join("pairing.json")).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let pos_file = dir.join("news.json");
    let pos: Position = std::fs::read(&pos_file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let (stream, _) = reach(&pairing.addrs, pairing.remote.as_ref()).await?;
    let mut ch = dbine_link::connect(stream, &pairing.desktop_key, &pairing.phone).await?;
    ch.send_json(&Request { id: 1, call: Call::Events { after: pos.after, boot: pos.boot.clone() } }).await?;
    let r: Response = tokio::time::timeout(CALL_LIMIT, ch.recv_json()).await.map_err(|_| "DBine no respondió".to_string())??;
    let ev: Events = serde_json::from_value(r.ok.ok_or_else(|| r.error.unwrap_or_default())?).map_err(|e| e.to_string())?;
    let next = Position { after: Some(ev.last), boot: Some(ev.boot) };
    let tmp = pos_file.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(&next).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &pos_file).map_err(|e| e.to_string())?;
    Ok(if pos.after.is_some() { ev.events } else { Vec::new() })
}

async fn check_news(app: &AppHandle) {
    use tauri_plugin_notification::NotificationExt;
    let Ok(file) = file(app) else { return };
    let Some(dir) = file.parent() else { return };
    if !file.exists() {
        return;
    }
    let Ok(events) = news_from(dir).await else { return };
    for e in &events {
        // DBine's glass as the small icon (res/drawable-*/ic_notification.png:
        // white on transparent, as Android wants), in DBine's blue.
        let _ = app.notification().builder().title(&e.title).body(&e.body).icon("ic_notification").icon_color("#3794FF").show();
    }
}

/// Android's periodic check (`NewsWorker.kt`, every 15 minutes, also with
/// the app closed): the new notices as JSON (`[]` when none or DBine isn't
/// reachable). The Kotlin side shows them.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_addlayer_dbine_mobile_NewsWorker_check<'a>(
    mut env: jni::JNIEnv<'a>,
    _class: jni::objects::JClass<'a>,
    data_dir: jni::objects::JString<'a>,
) -> jni::sys::jstring {
    let dir: String = env.get_string(&data_dir).map(Into::into).unwrap_or_default();
    let json = std::panic::catch_unwind(|| {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
        let dir = std::path::PathBuf::from(dir);
        // The app keeps its files in Tauri's app data folder, inside the
        // app's own: look there and one or two levels down.
        let mut look = vec![dir.clone()];
        for d in std::fs::read_dir(&dir).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()) {
            look.extend(std::fs::read_dir(&d).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_dir()));
            look.push(d);
        }
        let found = look.into_iter().find(|d| d.join("pairing.json").exists())?;
        let events = rt.block_on(news_from(&found)).ok()?;
        serde_json::to_string(&events).ok()
    })
    .ok()
    .flatten()
    .unwrap_or_else(|| "[]".into());
    env.new_string(json).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();
    #[cfg(mobile)]
    let builder = builder.plugin(tauri_plugin_barcode_scanner::init()).plugin(tauri_plugin_biometric::init()).plugin(tauri_plugin_deep_link::init());
    builder
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // A pairing link: the one that started the app, and later ones.
            #[cfg(mobile)]
            {
                use tauri_plugin_deep_link::DeepLinkExt;
                let handle = app.handle().clone();
                if let Ok(Some(urls)) = app.deep_link().get_current() {
                    urls.iter().for_each(|u| take_link(u.as_str(), &handle));
                }
                let h2 = handle.clone();
                app.deep_link().on_open_url(move |e| e.urls().iter().for_each(|u| take_link(u.as_str(), &h2)));
            }
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
        .invoke_handler(tauri::generate_handler![status, pair, unpair, call, pending_link, invite_name, open_release])
        .run(tauri::generate_context!())
        .expect("error while running DBine mobile");
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_link::Accepted;
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::net::TcpListener;

    /// A DBine that answers `events` from a list it is given, like the
    /// desktop's (`after` within the same `boot`, everything from another).
    async fn fake_dbine(keys: Keys, phone: Vec<u8>, state: Arc<StdMutex<(String, Vec<EventRow>)>>) -> String {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            loop {
                let (s, _) = l.accept().await.unwrap();
                let (keys, phone, state) = (keys.clone(), phone.clone(), state.clone());
                tokio::spawn(async move {
                    let Ok(Accepted::Connected(mut ch)) = dbine_link::accept(s, &keys, None, |k| k == phone.as_slice()).await else { return };
                    while let Ok(req) = ch.recv_json::<Request>().await {
                        let Call::Events { after, boot } = req.call else { continue };
                        let (b, list) = state.lock().unwrap().clone();
                        let after = match (after, boot) {
                            (Some(a), Some(x)) if x == b => Some(a),
                            (Some(_), _) => Some(0),
                            (None, _) => None,
                        };
                        let events = after.map(|a| list.iter().filter(|e| e.seq > a).cloned().collect()).unwrap_or_default();
                        let last = list.last().map(|e| e.seq).unwrap_or(0);
                        let _ = ch.send_json(&Response::ok(req.id, &Events { events, last, boot: b })).await;
                    }
                });
            }
        });
        addr
    }

    fn ev(seq: u64, title: &str) -> EventRow {
        EventRow { seq, kind: "query".into(), title: title.into(), body: "Terminó".into(), ok: true, at: String::new() }
    }

    #[tokio::test]
    async fn news_arrive_once_and_survive_restarts() {
        let dir = std::env::temp_dir().join(format!("dbine-mobile-news-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let desktop = Keys::generate().unwrap();
        let phone = Keys::generate().unwrap();
        let state = Arc::new(StdMutex::new(("boot1".to_string(), vec![ev(1, "vieja")])));
        let addr = fake_dbine(desktop.clone(), phone.public.clone(), state.clone()).await;
        let p = Pairing { phone, desktop_key: desktop.public.clone(), desktop: "mac".into(), addrs: vec![addr], remote: None };
        std::fs::write(dir.join("pairing.json"), serde_json::to_vec(&p).unwrap()).unwrap();

        // First look: learns where the list is; old news isn't news.
        assert!(news_from(&dir).await.unwrap().is_empty());
        // A query ends: it arrives once.
        state.lock().unwrap().1.push(ev(2, "ventas"));
        let got = news_from(&dir).await.unwrap();
        assert_eq!(got.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(), ["ventas"]);
        assert!(news_from(&dir).await.unwrap().is_empty(), "not twice");
        // The position is on disk: a new process (the app restarted, or
        // Android's check) carries on from it.
        state.lock().unwrap().1.push(ev(3, "stock"));
        let pos: Position = serde_json::from_slice(&std::fs::read(dir.join("news.json")).unwrap()).unwrap();
        assert_eq!(pos.after, Some(2));
        assert_eq!(news_from(&dir).await.unwrap()[0].title, "stock");
        // DBine restarted: everything of its new run is new to the phone.
        *state.lock().unwrap() = ("boot2".to_string(), vec![ev(1, "después del reinicio")]);
        assert_eq!(news_from(&dir).await.unwrap()[0].title, "después del reinicio");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
