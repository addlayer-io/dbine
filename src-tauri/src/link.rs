//! The phone app's server (docs/mobile-companion.md): Configuración ›
//! Dispositivos turns it on, shows the QR code and lists the paired phones.
//!
//! - **Off unless turned on.** Nothing listens until the user enables it.
//! - **Pairing needs the QR and a yes on this computer.** An invite lasts 2
//!   minutes and pairs one phone; the phone's handshake proves it read the
//!   code, and DBine asks its user (with both screens' fingerprint) before
//!   keeping the phone. Pairing attempts are rate-limited.
//! - **Then, only paired phones.** Each connection is a Noise handshake with
//!   the phone's own key (`dbine-link`); removing a phone cuts its sessions.
//! - **The connections never leave this computer.** The phone asks for
//!   locks, processes or a kill; DBine does it with its own sessions and
//!   answers with what the phone shows. Ending sessions is off per phone
//!   until the user allows it, and the phone confirms each one with the
//!   device's biometrics.
//!
//! State: `local.link` (this machine only, not synced) holds the switch,
//! the port and the phones; DBine's private key is in the vault.

use crate::state::AppState;
use dbine_link::proto::{
    Call, ConnectionItem, EventRow, Events, HealthRow, HealthView, Hello, LockRow, MetricRow, MonitorView, PairResult, ProcessRow, QueriesView, QueryRow, Request, Response, RunRow,
    StepRow, TaskItem,
};
use dbine_link::net::Relays;
use dbine_link::{Accepted, BoxIo, Invite, Keys, Remote};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

const SETTING: &str = "local.link";
const KEYS: &str = "link-keys";
/// DBine's iroh key (its id from outside the local network).
const IROH_KEY: &str = "link-iroh-key";
pub const DEFAULT_PORT: u16 = 27518;
/// An invite (the QR code) is good for this long, and for one phone.
const INVITE_LIFE: Duration = Duration::from_secs(120);
/// The user has this long to accept a phone on this computer.
const ANSWER_LIMIT: Duration = Duration::from_secs(120);
/// Pairing attempts allowed per minute (a wrong code costs one).
const PAIR_ATTEMPTS: usize = 5;
/// A phone that sends nothing for this long is disconnected.
const IDLE_LIMIT: Duration = Duration::from_secs(10 * 60);
/// A database call from the phone waits at most this long (a health check
/// reads a lot).
const CALL_LIMIT: Duration = Duration::from_secs(60);
/// A query from the editor that runs at least this long tells the phone
/// when it ends.
pub const LONG_QUERY: Duration = Duration::from_secs(10);
/// Notices kept for the phone (it asks every few seconds while open).
const EVENTS_KEPT: usize = 100;
/// Queries that ended, kept for the phone's "Actividad".
const QUERIES_KEPT: usize = 30;
/// A query running this long keeps the computer awake until it ends.
const AWAKE_AFTER: Duration = Duration::from_secs(30);
/// How often DBine looks for task runs that ended (the OS scheduler runs
/// them in another process, so they only show in the history).
const TASKS_EVERY: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Device {
    /// Its key's fingerprint ("ABCD-0102-0304").
    pub id: String,
    pub name: String,
    /// Its public key (base64).
    pub key: String,
    pub paired_at: String,
    #[serde(default)]
    pub last_seen: Option<String>,
    /// It may end sessions (each one confirmed on the phone).
    #[serde(default)]
    pub can_kill: bool,
    /// It may run scheduled tasks ("Ejecutar ahora", confirmed on the phone).
    #[serde(default)]
    pub can_run_tasks: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default)]
    pub devices: Vec<Device>,
    /// Reaching DBine from outside the local network.
    #[serde(default)]
    pub remote: RemoteSetting,
}

impl Default for LinkSettings {
    fn default() -> Self {
        LinkSettings { enabled: false, port: DEFAULT_PORT, devices: Vec::new(), remote: RemoteSetting::default() }
    }
}

/// How phones reach DBine from outside the local network (iroh). Off
/// unless the user turns it on: going through a third party's relays is a
/// decision the user makes knowingly (the UI asks), never a default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum RemoteSetting {
    /// iroh's public relays (free, best effort, run by n0: they see both
    /// ends' IP addresses and the traffic's timing and size, never its
    /// content).
    Public,
    /// The user's own relays (`iroh-relay`), nothing else.
    Own { relays: Vec<String> },
    /// Only the local network.
    #[default]
    Off,
}

impl RemoteSetting {
    fn relays(&self) -> Option<Relays> {
        match self {
            RemoteSetting::Public => Some(Relays::Public),
            RemoteSetting::Own { relays } => Some(Relays::Own(relays.clone())),
            RemoteSetting::Off => None,
        }
    }
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

struct OpenInvite {
    secret: [u8; 32],
    until: Instant,
}

struct Running {
    port: u16,
    task: tauri::async_runtime::JoinHandle<()>,
}

/// The iroh side: its endpoint once bound, and the loop taking phones.
struct RunningRemote {
    setting: RemoteSetting,
    endpoint: Arc<Mutex<Option<iroh::Endpoint>>>,
    task: tauri::async_runtime::JoinHandle<()>,
}

pub struct LinkRuntime {
    inner: Arc<Inner>,
}

struct Inner {
    state: AppState,
    app: Mutex<Option<AppHandle>>,
    server: Mutex<Option<Running>>,
    last_error: Mutex<Option<String>>,
    remote: Mutex<Option<RunningRemote>>,
    remote_error: Arc<Mutex<Option<String>>>,
    invite: Mutex<Option<OpenInvite>>,
    /// The pairing waiting for the user's answer.
    pending: Mutex<Option<oneshot::Sender<bool>>>,
    /// Pairing attempts in the last minute.
    attempts: Mutex<VecDeque<Instant>>,
    /// Each phone's open sessions, to cut them when it's removed.
    live: Mutex<HashMap<String, Vec<tauri::async_runtime::JoinHandle<()>>>>,
    /// DBine's keys once read (the vault isn't read on every connection).
    keys: Mutex<Option<Keys>>,
    /// What the phones get notified of, newest last.
    events: Mutex<VecDeque<EventRow>>,
    next_seq: Mutex<u64>,
    /// This run's id: the phones start over when it changes.
    boot: String,
    /// Task runs already turned into notices.
    seen_runs: Mutex<std::collections::HashSet<String>>,
    /// The editor's queries running now, by run id.
    running: Mutex<HashMap<String, Running_>>,
    /// The ones that ended (newest last).
    recent: Mutex<VecDeque<QueryRow>>,
    awake: crate::awake::Awake,
}

/// A query from the editor while it runs.
struct Running_ {
    session_id: String,
    place: String,
    sql: String,
    detail: String,
    started: Instant,
    started_at: String,
}

/// Registered while an editor query runs: [`QueryGuard::finish`] records
/// how it ended; dropped without it (the run failed before ending), it just
/// leaves the list.
pub struct QueryGuard {
    inner: Arc<Inner>,
    id: String,
}

impl QueryGuard {
    pub fn finish(self, out: &dbine_driver::QueryOutcome) {
        let Some(r) = self.inner.running.lock().unwrap().remove(&self.id) else { return };
        let ms = r.started.elapsed().as_millis() as u64;
        if Duration::from_millis(ms) < LONG_QUERY {
            return;
        }
        let failed = out.error.clone().or_else(|| out.errors.first().map(|e| e.message.clone()));
        let took = took(ms);
        let summary = match &failed {
            Some(e) => e.chars().take(300).collect::<String>(),
            None => {
                let rows: u64 = out.results.iter().map(|r| r.total_rows).sum();
                let affected: u64 = out.results.iter().filter_map(|r| r.rows_affected).sum();
                if rows > 0 {
                    format!("{rows} filas")
                } else if affected > 0 {
                    format!("{affected} filas afectadas")
                } else {
                    String::new()
                }
            }
        };
        let body = match (&failed, summary.is_empty()) {
            (Some(e), _) => format!("Falló después de {took}: {}", e.chars().take(200).collect::<String>()),
            (None, true) => format!("Terminó en {took}"),
            (None, false) => format!("Terminó en {took} · {summary}"),
        };
        {
            let mut recent = self.inner.recent.lock().unwrap();
            recent.push_back(QueryRow {
                id: self.id.clone(),
                place: r.place.clone(),
                sql: r.sql,
                started_at: r.started_at,
                elapsed_ms: ms,
                running: false,
                ok: failed.is_none(),
                summary,
                detail: r.detail,
            });
            while recent.len() > QUERIES_KEPT {
                recent.pop_front();
            }
        }
        let s = self.inner.settings();
        if s.enabled && !s.devices.is_empty() {
            self.inner.push("query", r.place, body, failed.is_none());
        }
    }
}

impl Drop for QueryGuard {
    fn drop(&mut self) {
        self.inner.running.lock().unwrap().remove(&self.id);
    }
}

/// The first lines of a query, for a phone's screen.
fn first_lines(sql: &str) -> String {
    let lines: Vec<&str> = sql.lines().map(str::trim).filter(|l| !l.is_empty()).take(3).collect();
    let s = lines.join(" ");
    if s.chars().count() > 160 {
        format!("{}…", s.chars().take(160).collect::<String>())
    } else {
        s
    }
}

fn b64(v: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(v)
}

fn now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M").to_string()
}

/// The computer's name, to show on the phone.
pub fn computer_name() -> String {
    if let Ok(n) = std::env::var("COMPUTERNAME") {
        return n;
    }
    #[cfg(target_os = "macos")]
    if let Ok(o) = std::process::Command::new("/usr/sbin/scutil").args(["--get", "ComputerName"]).output() {
        let n = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !n.is_empty() {
            return n;
        }
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "DBine".into())
}

/// This computer's address on the local network: the one the OS would use
/// to go out (no packet is sent).
fn lan_address() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    s.local_addr().ok().map(|a| a.ip()).filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
}

impl LinkRuntime {
    pub fn new(state: AppState) -> LinkRuntime {
        LinkRuntime {
            inner: Arc::new(Inner {
                state,
                app: Mutex::new(None),
                server: Mutex::new(None),
                last_error: Mutex::new(None),
                remote: Mutex::new(None),
                remote_error: Arc::new(Mutex::new(None)),
                invite: Mutex::new(None),
                pending: Mutex::new(None),
                attempts: Mutex::new(VecDeque::new()),
                live: Mutex::new(HashMap::new()),
                keys: Mutex::new(None),
                events: Mutex::new(VecDeque::new()),
                next_seq: Mutex::new(0),
                boot: dbine_link::random::<8>().map(|b| b.iter().map(|x| format!("{x:02x}")).collect()).unwrap_or_default(),
                seen_runs: Mutex::new(std::collections::HashSet::new()),
                running: Mutex::new(HashMap::new()),
                recent: Mutex::new(VecDeque::new()),
                awake: crate::awake::Awake::new(),
            }),
        }
    }

    /// Start listening if the user turned it on.
    pub fn attach(&self, app: AppHandle) {
        *self.inner.app.lock().unwrap() = Some(app);
        self.inner.apply();
        // The runs already in the history aren't news.
        self.inner.watch_tasks(true);
        let me = self.inner.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(TASKS_EVERY).await;
                me.watch_tasks(false);
            }
        });
        // The computer stays awake while a query has been running a while.
        let me = self.inner.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                let long = me.running.lock().unwrap().values().any(|r| r.started.elapsed() >= AWAKE_AFTER);
                me.awake.set(long);
            }
        });
    }

    /// An editor query starts: it shows on the phone while it runs.
    pub fn query_started(&self, session_id: &str, connection_id: &str, database: &str, sql: &str) -> QueryGuard {
        let id: String = dbine_link::random::<8>().map(|b| b.iter().map(|x| format!("{x:02x}")).collect()).unwrap_or_default();
        let conn = self.inner.state.store.get_connection(connection_id).ok().flatten().map(|c| c.name).unwrap_or_else(|| connection_id.to_string());
        let place = if database.is_empty() { conn } else { format!("{conn} › {database}") };
        self.inner.running.lock().unwrap().insert(
            id.clone(),
            Running_ {
                session_id: session_id.to_string(),
                place,
                sql: first_lines(sql),
                detail: sql.trim().chars().take(2000).collect(),
                started: Instant::now(),
                started_at: now(),
            },
        );
        QueryGuard { inner: self.inner.clone(), id }
    }

    pub fn settings(&self) -> LinkSettings {
        self.inner.settings()
    }

    pub fn status(&self) -> (bool, Option<String>) {
        let running = self.inner.server.lock().unwrap().is_some();
        (running, self.inner.last_error.lock().unwrap().clone())
    }

    pub fn desktop_fingerprint(&self) -> Result<String, String> {
        Ok(dbine_link::fingerprint(&self.inner.keys()?.public))
    }

    pub fn save(&self, f: impl FnOnce(&mut LinkSettings)) -> Result<LinkSettings, String> {
        let mut s = self.inner.settings();
        f(&mut s);
        let v = serde_json::to_value(&s).map_err(|e| e.to_string())?;
        self.inner.state.store.set_setting(SETTING, Some(&v)).map_err(|e| e.to_string())?;
        // Removed phones lose their open sessions now.
        let ids: Vec<String> = s.devices.iter().map(|d| d.id.clone()).collect();
        self.inner.live.lock().unwrap().retain(|id, tasks| {
            let keep = ids.contains(id);
            if !keep {
                tasks.iter().for_each(|t| t.abort());
            }
            keep
        });
        self.inner.apply();
        Ok(s)
    }

    /// A new invite: the QR's text. Replaces any open one.
    pub fn invite(&self) -> Result<String, String> {
        let s = self.inner.settings();
        if !s.enabled {
            return Err("activá «Permitir celulares» primero".into());
        }
        if self.inner.server.lock().unwrap().is_none() {
            return Err(self.inner.last_error.lock().unwrap().clone().unwrap_or_else(|| "el servidor para celulares no está funcionando".into()));
        }
        let keys = self.inner.keys()?;
        let secret: [u8; 32] = dbine_link::random()?;
        let addrs: Vec<String> = lan_address().into_iter().map(|ip| std::net::SocketAddr::new(ip, s.port).to_string()).collect();
        let remote = self.inner.remote_info();
        if addrs.is_empty() && remote.is_none() {
            return Err("esta computadora no está conectada a una red".into());
        }
        *self.inner.invite.lock().unwrap() = Some(OpenInvite { secret, until: Instant::now() + INVITE_LIFE });
        Ok(Invite { v: dbine_link::VERSION, key: keys.public, secret: secret.to_vec(), addrs, name: computer_name(), remote }.encode())
    }

    pub fn cancel_invite(&self) {
        *self.inner.invite.lock().unwrap() = None;
        if let Some(tx) = self.inner.pending.lock().unwrap().take() {
            let _ = tx.send(false);
        }
    }

    /// The user's answer to "¿Vincular este celular?".
    pub fn answer(&self, accept: bool) {
        if let Some(tx) = self.inner.pending.lock().unwrap().take() {
            let _ = tx.send(accept);
        }
    }
}

/// "40 s", "3 min 20 s", "1 h 05 min".
fn took(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min {:02} s", s / 60, s % 60),
        s => format!("{} h {:02} min", s / 3600, (s % 3600) / 60),
    }
}

fn status_text(s: dbine_core::tasks::RunStatus) -> &'static str {
    use dbine_core::tasks::RunStatus::*;
    match s {
        Running => "running",
        Ok => "ok",
        Partial => "partial",
        Failed => "failed",
    }
}

fn run_row(r: dbine_core::tasks::TaskRun) -> RunRow {
    RunRow {
        id: r.id,
        status: status_text(r.status).into(),
        trigger: r.trigger,
        started_at: r.started_at,
        finished_at: r.finished_at,
        error: r.error,
        steps: r.steps.into_iter().map(|s| StepRow { kind: s.kind, status: status_text(s.status).into(), summary: s.summary, alert: s.alert }).collect(),
    }
}

/// A statement as the phone gets it: trimmed (docs/mobile-security.md).
fn trimmed_sql(sql: Option<String>) -> Option<String> {
    sql.map(|s| if s.chars().count() > 500 { format!("{}…", s.chars().take(500).collect::<String>()) } else { s })
}

/// The session key of a phone's calls on a connection › database.
fn key(connection_id: &str, database: &str) -> String {
    format!("link:{connection_id}:{database}")
}

/// The QR code of `text`, as SVG.
pub fn qr_svg(text: &str) -> Result<String, String> {
    let code = qrcode::QrCode::with_error_correction_level(text, qrcode::EcLevel::M).map_err(|e| e.to_string())?;
    Ok(code.render::<qrcode::render::svg::Color>().min_dimensions(240, 240).quiet_zone(true).build())
}

impl Inner {
    fn push(&self, kind: &str, title: String, body: String, ok: bool) {
        let seq = {
            let mut n = self.next_seq.lock().unwrap();
            *n += 1;
            *n
        };
        let mut ev = self.events.lock().unwrap();
        ev.push_back(EventRow { seq, kind: kind.into(), title, body, ok, at: now() });
        while ev.len() > EVENTS_KEPT {
            ev.pop_front();
        }
    }

    /// Turn task runs that ended into notices, as the task's own "Avisar"
    /// says: on failure or alert (the default), always, or never.
    fn watch_tasks(&self, first: bool) {
        use dbine_core::tasks::{Notify, RunStatus};
        let Ok(runs) = self.state.store.list_task_runs(None, 50) else { return };
        let mut seen = self.seen_runs.lock().unwrap();
        for r in runs.into_iter().rev() {
            if r.status == RunStatus::Running || !seen.insert(r.id.clone()) || first {
                continue;
            }
            let Ok(Some(task)) = self.state.store.get_task(&r.task_id) else { continue };
            let alerts: Vec<&str> = r.steps.iter().filter_map(|s| s.alert.as_deref()).collect();
            let ok = r.status == RunStatus::Ok && alerts.is_empty();
            let tell = match task.notify {
                Notify::Never => false,
                Notify::Always => true,
                Notify::Failure => !ok,
            };
            if !tell {
                continue;
            }
            let body = match r.status {
                RunStatus::Ok if alerts.is_empty() => "Terminó bien.".to_string(),
                RunStatus::Ok => alerts.join("\n"),
                RunStatus::Partial => format!("Terminó con errores. {}", alerts.join("\n")).trim().to_string(),
                _ => r
                    .error
                    .clone()
                    .or_else(|| r.steps.iter().find(|s| s.status == RunStatus::Failed).map(|s| format!("Falló: {}", s.summary)))
                    .unwrap_or_else(|| "Falló.".into()),
            };
            drop(seen);
            self.push("task", task.name.clone(), body, ok);
            seen = self.seen_runs.lock().unwrap();
        }
    }

    fn settings(&self) -> LinkSettings {
        self.state.store.get_setting(SETTING).ok().flatten().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
    }

    /// DBine's key pair, made the first time.
    fn keys(&self) -> Result<Keys, String> {
        let mut cached = self.keys.lock().unwrap();
        if let Some(k) = cached.as_ref() {
            return Ok(k.clone());
        }
        let k = match dbine_core::secrets::get_raw(KEYS).map_err(|e| e.to_string())?.and_then(|j| serde_json::from_str::<Keys>(&j).ok()) {
            Some(k) => k,
            None => {
                let k = Keys::generate()?;
                dbine_core::secrets::set_raw(KEYS, &serde_json::to_string(&k).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
                k
            }
        };
        *cached = Some(k.clone());
        Ok(k)
    }

    /// Listen or stop as the settings say.
    fn apply(self: &Arc<Self>) {
        let s = self.settings();
        self.apply_local(&s);
        self.apply_remote(&s);
    }

    /// Outside the local network: an iroh endpoint while turned on, with
    /// the chosen relays.
    fn apply_remote(self: &Arc<Self>, s: &LinkSettings) {
        let want = s.enabled.then(|| s.remote.clone()).filter(|r| *r != RemoteSetting::Off);
        let mut remote = self.remote.lock().unwrap();
        if remote.as_ref().map(|r| &r.setting) == want.as_ref() {
            return;
        }
        if let Some(r) = remote.take() {
            r.task.abort();
            if let Some(ep) = r.endpoint.lock().unwrap().take() {
                tauri::async_runtime::spawn(async move { ep.close().await });
            }
        }
        *self.remote_error.lock().unwrap() = None;
        let Some(setting) = want else { return };
        let Some(relays) = setting.relays() else { return };
        let secret = match self.iroh_secret() {
            Ok(k) => k,
            Err(e) => {
                *self.remote_error.lock().unwrap() = Some(e);
                return;
            }
        };
        let endpoint = Arc::new(Mutex::new(None));
        let (me, slot, errors) = (self.clone(), endpoint.clone(), self.remote_error.clone());
        let task = tauri::async_runtime::spawn(async move {
            let ep = match dbine_link::net::endpoint(secret, &relays, true).await {
                Ok(ep) => ep,
                Err(e) => {
                    tracing::warn!("phone link (internet) not started: {e}");
                    *errors.lock().unwrap() = Some(e);
                    return;
                }
            };
            tracing::info!(id = %ep.id(), "phone link listening through iroh");
            *slot.lock().unwrap() = Some(ep.clone());
            while let Some(next) = dbine_link::net::next(&ep).await {
                let me = me.clone();
                tauri::async_runtime::spawn(async move {
                    match next {
                        Ok(stream) => {
                            if let Err(e) = me.serve(Box::new(stream)).await {
                                tracing::info!("phone link connection (internet) closed: {e}");
                            }
                        }
                        Err(e) => tracing::info!("phone link connection (internet) refused: {e}"),
                    }
                });
            }
        });
        *remote = Some(RunningRemote { setting, endpoint, task });
    }

    /// DBine's iroh key, made the first time.
    fn iroh_secret(&self) -> Result<[u8; 32], String> {
        use base64::Engine;
        let b64e = base64::engine::general_purpose::STANDARD;
        if let Some(k) = dbine_core::secrets::get_raw(IROH_KEY).map_err(|e| e.to_string())?.and_then(|t| b64e.decode(t).ok()).and_then(|v| <[u8; 32]>::try_from(v).ok()) {
            return Ok(k);
        }
        let k = dbine_link::net::new_secret();
        dbine_core::secrets::set_raw(IROH_KEY, &b64e.encode(k)).map_err(|e| e.to_string())?;
        Ok(k)
    }

    /// How a phone reaches DBine from anywhere, while it can.
    fn remote_info(&self) -> Option<Remote> {
        let r = self.remote.lock().unwrap();
        let r = r.as_ref()?;
        let ep = r.endpoint.lock().unwrap().clone()?;
        Some(Remote { node: ep.id().to_string(), relays: r.setting.relays().map(|x| x.urls()).unwrap_or_default() })
    }

    /// The relay DBine is reachable through now, for the settings.
    fn remote_status(&self) -> (Option<String>, Option<String>) {
        let ep = self.remote.lock().unwrap().as_ref().and_then(|r| r.endpoint.lock().unwrap().clone());
        (ep.as_ref().and_then(dbine_link::net::home_relay), self.remote_error.lock().unwrap().clone())
    }

    /// The local network: the TCP listener while turned on.
    fn apply_local(self: &Arc<Self>, s: &LinkSettings) {
        let s = s.clone();
        let mut server = self.server.lock().unwrap();
        let same = server.as_ref().is_some_and(|r| r.port == s.port);
        if s.enabled && same {
            return;
        }
        if let Some(r) = server.take() {
            r.task.abort();
        }
        *self.last_error.lock().unwrap() = None;
        if !s.enabled {
            for (_, tasks) in self.live.lock().unwrap().drain() {
                tasks.iter().for_each(|t| t.abort());
            }
            return;
        }
        let listener = match std::net::TcpListener::bind(("0.0.0.0", s.port)).and_then(|l| l.set_nonblocking(true).map(|_| l)) {
            Ok(l) => l,
            Err(e) => {
                let msg = if e.kind() == std::io::ErrorKind::AddrInUse {
                    format!("el puerto {} está en uso por otro programa: elegí otro", s.port)
                } else {
                    format!("no se pudo abrir el puerto {}: {e}", s.port)
                };
                tracing::warn!("phone link server not started: {msg}");
                *self.last_error.lock().unwrap() = Some(msg);
                return;
            }
        };
        let me = self.clone();
        let task = tauri::async_runtime::spawn(async move {
            let listener = match TcpListener::from_std(listener) {
                Ok(l) => l,
                Err(e) => {
                    *me.last_error.lock().unwrap() = Some(e.to_string());
                    return;
                }
            };
            tracing::info!(port = s.port, "phone link server listening");
            loop {
                let Ok((stream, from)) = listener.accept().await else { continue };
                let _ = stream.set_nodelay(true);
                let me = me.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = me.serve(Box::new(stream)).await {
                        tracing::info!(%from, "phone link connection closed: {e}");
                    }
                });
            }
        });
        *server = Some(Running { port: s.port, task });
    }

    /// A pairing attempt counts against the limit; `false`: too many.
    fn allow_pairing(&self) -> bool {
        let mut a = self.attempts.lock().unwrap();
        let minute_ago = Instant::now() - Duration::from_secs(60);
        while a.front().is_some_and(|t| *t < minute_ago) {
            a.pop_front();
        }
        if a.len() >= PAIR_ATTEMPTS {
            return false;
        }
        a.push_back(Instant::now());
        true
    }

    async fn serve(self: Arc<Self>, mut stream: BoxIo) -> Result<(), String> {
        let keys = self.keys()?;
        let secret = {
            let mut inv = self.invite.lock().unwrap();
            if inv.as_ref().is_some_and(|i| i.until < Instant::now()) {
                *inv = None;
            }
            inv.as_ref().map(|i| i.secret)
        };
        let devices = self.settings().devices;
        let known: Vec<Vec<u8>> = devices
            .iter()
            .filter_map(|d| {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.decode(&d.key).ok()
            })
            .collect();
        // Rate-limit only what could be a pairing, before any crypto runs.
        let pairing = dbine_link::read_mode(&mut stream).await?;
        if pairing && (secret.is_none() || !self.allow_pairing()) {
            return Err("pairing refused (no open invite or too many attempts)".into());
        }
        match dbine_link::accept_mode(stream, pairing, &keys, secret.as_ref().map(|s| s.as_slice()), |k| known.iter().any(|x| x == k)).await? {
            Accepted::Paired { channel, hello } => self.pair(channel, hello).await,
            Accepted::Connected(channel) => {
                let id = dbine_link::fingerprint(&channel.peer);
                let me = self.clone();
                let task = tauri::async_runtime::spawn({
                    let id = id.clone();
                    async move {
                        if let Err(e) = me.session(channel, &id).await {
                            tracing::info!(device = %id, "phone session ended: {e}");
                        }
                    }
                });
                self.live.lock().unwrap().entry(id).or_default().push(task);
                Ok(())
            }
        }
    }

    async fn pair(self: Arc<Self>, mut channel: dbine_link::Channel<BoxIo>, hello: Vec<u8>) -> Result<(), String> {
        // The phone proves it read the code: with a wrong secret it can't
        // decrypt our reply and never sends this.
        let ready = tokio::time::timeout(Duration::from_secs(10), channel.recv()).await.map_err(|_| "pairing: no confirmation".to_string())??;
        if ready != b"ready" {
            return Err("pairing: unexpected message".into());
        }
        // One phone per code.
        *self.invite.lock().unwrap() = None;
        let name: String = String::from_utf8_lossy(&hello).chars().filter(|c| !c.is_control()).take(60).collect::<String>().trim().to_string();
        let name = if name.is_empty() { "Celular".to_string() } else { name };
        let id = dbine_link::fingerprint(&channel.peer);
        let (tx, rx) = oneshot::channel();
        if let Some(old) = self.pending.lock().unwrap().replace(tx) {
            let _ = old.send(false);
        }
        if let Some(app) = self.app.lock().unwrap().clone() {
            #[derive(Serialize, Clone)]
            struct Ask {
                name: String,
                fingerprint: String,
            }
            let _ = app.emit("link-pair-request", Ask { name: name.clone(), fingerprint: id.clone() });
        }
        let accepted = matches!(tokio::time::timeout(ANSWER_LIMIT, rx).await, Ok(Ok(true)));
        let desktop = computer_name();
        if accepted {
            let device = Device { id: id.clone(), name: name.clone(), key: b64(&channel.peer), paired_at: now(), last_seen: None, can_kill: false, can_run_tasks: false };
            let mut s = self.settings();
            s.devices.retain(|d| d.id != id);
            s.devices.push(device);
            let v = serde_json::to_value(&s).map_err(|e| e.to_string())?;
            self.state.store.set_setting(SETTING, Some(&v)).map_err(|e| e.to_string())?;
            tracing::info!(device = %id, "phone paired");
        }
        channel
            .send_json(&PairResult { accepted, desktop, reason: (!accepted).then(|| "rechazado o sin respuesta en la computadora".to_string()) })
            .await?;
        if let Some(app) = self.app.lock().unwrap().clone() {
            let _ = app.emit("link-changed", ());
        }
        Ok(())
    }

    async fn session(self: Arc<Self>, mut channel: dbine_link::Channel<BoxIo>, id: &str) -> Result<(), String> {
        let touch = |me: &Arc<Self>| {
            let mut s = me.settings();
            if let Some(d) = s.devices.iter_mut().find(|d| d.id == id) {
                d.last_seen = Some(now());
                if let Ok(v) = serde_json::to_value(&s) {
                    let _ = me.state.store.set_setting(SETTING, Some(&v));
                }
            }
        };
        touch(&self);
        loop {
            let req: Request = tokio::time::timeout(IDLE_LIMIT, channel.recv_json()).await.map_err(|_| "idle".to_string())??;
            // Removed meanwhile: stop answering.
            let Some(device) = self.settings().devices.into_iter().find(|d| d.id == id) else {
                return Err("device removed".into());
            };
            let id_n = req.id;
            let resp = match tokio::time::timeout(CALL_LIMIT, self.call(&device, req.call)).await {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => Response::error(id_n, e),
                Err(_) => Response::error(id_n, format!("el servidor no respondió en {} s", CALL_LIMIT.as_secs())),
            };
            let resp = Response { id: id_n, ..resp };
            channel.send_json(&resp).await?;
        }
    }

    async fn call(&self, device: &Device, call: Call) -> Result<Response, String> {
        let state = &self.state;
        Ok(match call {
            Call::Hello => Response::ok(
                0,
                &Hello {
                    desktop: computer_name(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    can_kill: device.can_kill,
                    can_run_tasks: device.can_run_tasks,
                    remote: self.remote_info(),
                },
            ),
            Call::Events { after, boot } => {
                let ev = self.events.lock().unwrap();
                let last = ev.back().map(|e| e.seq).unwrap_or(0);
                // A phone that knew another run of DBine gets all of this
                // run's: it hasn't seen any of them. One that never looked
                // only learns where the list is.
                let after = match (after, boot) {
                    (Some(a), Some(b)) if b == self.boot => Some(a),
                    (Some(_), _) => Some(0),
                    (None, _) => None,
                };
                let events = after.map(|a| ev.iter().filter(|e| e.seq > a).cloned().collect()).unwrap_or_default();
                Response::ok(0, &Events { events, last, boot: self.boot.clone() })
            }
            Call::Tasks => {
                let now_t = chrono::Local::now().naive_local();
                let list: Vec<TaskItem> = state
                    .store
                    .list_tasks()
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .map(|t| {
                        let fp = crate::tasks::write_fingerprint(state, &t);
                        TaskItem {
                            next_run: t.enabled.then(|| t.schedule.next_after(now_t)).flatten().map(|x| x.format("%Y-%m-%d %H:%M").to_string()),
                            last: state.store.list_task_runs(Some(&t.id), 1).ok().and_then(|r| r.into_iter().next()).map(run_row),
                            needs_approval: fp.is_some() && fp != t.approved_writes,
                            steps: t.steps.iter().map(|s| if s.name.is_empty() { s.kind.clone() } else { s.name.clone() }).collect(),
                            id: t.id,
                            name: t.name,
                            enabled: t.enabled,
                        }
                    })
                    .collect();
                Response::ok(0, &list)
            }
            Call::TaskRuns { task_id } => {
                let runs: Vec<RunRow> = state.store.list_task_runs(Some(&task_id), 10).map_err(|e| e.to_string())?.into_iter().map(run_row).collect();
                Response::ok(0, &runs)
            }
            Call::RunTask { task_id } => {
                if !device.can_run_tasks {
                    return Err("este celular no tiene permiso para ejecutar tareas: se habilita en DBine, Configuración › Dispositivos".into());
                }
                let task = crate::tasks::load(state, &task_id).map_err(|e| e.to_string())?;
                let app = self.app.lock().unwrap().clone();
                let st = state.clone();
                tracing::info!(device = %device.id, task = %task_id, "task run from a phone");
                tauri::async_runtime::spawn(async move {
                    let run = crate::tasks::run_task(&st, app.as_ref(), &task, "phone").await;
                    if let Some(app) = app {
                        let _ = app.emit("task-run-finished", &run);
                    }
                });
                Response::ok(0, &true)
            }
            Call::Queries => {
                let running: Vec<QueryRow> = {
                    let r = self.running.lock().unwrap();
                    let mut v: Vec<QueryRow> = r
                        .iter()
                        .map(|(id, q)| QueryRow {
                            id: id.clone(),
                            place: q.place.clone(),
                            sql: q.sql.clone(),
                            started_at: q.started_at.clone(),
                            elapsed_ms: q.started.elapsed().as_millis() as u64,
                            running: true,
                            ok: true,
                            summary: String::new(),
                            detail: q.detail.clone(),
                        })
                        .collect();
                    v.sort_by_key(|q| std::cmp::Reverse(q.elapsed_ms));
                    v
                };
                let recent: Vec<QueryRow> = self.recent.lock().unwrap().iter().rev().cloned().collect();
                Response::ok(0, &QueriesView { running, recent })
            }
            Call::CancelQuery { query_id } => {
                if !device.can_kill {
                    return Err("este celular no tiene permiso para cancelar consultas: se habilita en DBine, Configuración › Dispositivos".into());
                }
                let session_id = self.running.lock().unwrap().get(&query_id).map(|q| q.session_id.clone()).ok_or("la consulta ya terminó")?;
                crate::commands::query::cancel_session(state, &session_id);
                tracing::info!(device = %device.id, "query cancelled from a phone");
                Response::ok(0, &true)
            }
            Call::CancelTaskRun { run_id } => {
                if !device.can_run_tasks {
                    return Err("este celular no tiene permiso para ejecutar ni cancelar tareas: se habilita en DBine, Configuración › Dispositivos".into());
                }
                let running = state
                    .store
                    .list_task_runs(None, 200)
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .any(|r| r.id == run_id && r.status == dbine_core::tasks::RunStatus::Running);
                if !running {
                    return Err("la tarea ya terminó".into());
                }
                state.store.set_setting(&crate::tasks::cancel_key(&run_id), Some(&serde_json::json!(true))).map_err(|e| e.to_string())?;
                tracing::info!(device = %device.id, run = %run_id, "task run cancelled from a phone");
                Response::ok(0, &true)
            }
            Call::Health { connection_id, database } => {
                let args = crate::commands::db_health::HealthArgs { connection_id, database, run_id: format!("phone-{}", device.id) };
                let r = crate::commands::db_health::report(state, args).await.map_err(|e| e.to_string())?;
                let sev = |s: dbine_driver::health::Severity| serde_json::to_value(s).ok().and_then(|v| v.as_str().map(str::to_lowercase)).unwrap_or_else(|| "info".into());
                let view = HealthView {
                    checks: r
                        .checks
                        .into_iter()
                        .map(|c| HealthRow { category: c.category, title: c.title, severity: sev(c.severity), detail: (!c.detail.is_empty()).then_some(c.detail), objects: c.objects.into_iter().take(20).collect() })
                        .collect(),
                    skipped: r.skipped,
                };
                Response::ok(0, &view)
            }
            Call::Connections => {
                let list: Vec<ConnectionItem> = state
                    .store
                    .list_connections()
                    .map_err(|e| e.to_string())?
                    .into_iter()
                    .filter_map(|c| {
                        let d = dbine_drivers::find(&c.config.driver)?;
                        let caps = d.capabilities();
                        Some(ConnectionItem {
                            id: c.id,
                            name: c.name,
                            engine: d.info().name.to_string(),
                            color: c.color,
                            locks: caps.blocking,
                            processes: caps.processes,
                            kill: caps.kill_session && !c.config.read_only,
                            monitor: caps.monitor,
                        })
                    })
                    .collect();
                Response::ok(0, &list)
            }
            Call::Databases { connection_id } => {
                let entry = state.session(&key(&connection_id, ""), &connection_id, "").await.map_err(|e| e.to_string())?;
                let dbs = entry.session.lock().await.list_databases().await.map_err(|e| e.to_string())?;
                Response::ok(0, &dbs)
            }
            Call::Locks { connection_id, database } => {
                let entry = state.session(&key(&connection_id, &database), &connection_id, &database).await.map_err(|e| e.to_string())?;
                let rows = entry.session.lock().await.blocking().await;
                let rows = self.forget_if_dropped(&key(&connection_id, &database), rows)?;
                let out: Vec<LockRow> = rows
                    .into_iter()
                    .map(|b| LockRow {
                        id: b.id,
                        blocked_by: b.blocked_by,
                        user: b.user,
                        client: b.client,
                        database: b.database,
                        wait: b.wait,
                        waited_ms: b.waited_ms,
                        object: b.object,
                        sql: trimmed_sql(b.sql),
                    })
                    .collect();
                Response::ok(0, &out)
            }
            Call::Processes { connection_id, database } => {
                let entry = state.session(&key(&connection_id, &database), &connection_id, &database).await.map_err(|e| e.to_string())?;
                let rows = entry.session.lock().await.processes().await;
                let rows = self.forget_if_dropped(&key(&connection_id, &database), rows)?;
                let out: Vec<ProcessRow> = rows
                    .into_iter()
                    .filter(|p| p.active && !p.own && !p.system)
                    .map(|p| ProcessRow {
                        id: p.id,
                        status: p.status,
                        user: p.user,
                        host: p.host,
                        database: p.database,
                        elapsed_ms: p.elapsed_ms,
                        blocked_by: p.blocked_by,
                        sql: trimmed_sql(p.sql),
                    })
                    .collect();
                Response::ok(0, &out)
            }
            Call::Monitor { connection_id } => {
                let k = key(&connection_id, "");
                let entry = state.session(&k, &connection_id, "").await.map_err(|e| e.to_string())?;
                let snap = entry.session.lock().await.monitor().await;
                let snap = self.forget_if_dropped(&k, snap)?;
                let unit = |u: &dbine_driver::MetricUnit| {
                    serde_json::to_value(u).ok().and_then(|v| v.as_str().map(str::to_lowercase)).unwrap_or_else(|| "count".into())
                };
                let view = MonitorView {
                    metrics: snap
                        .metrics
                        .iter()
                        .map(|m| MetricRow { key: m.key.clone(), label: m.label.clone(), group: m.group.clone(), unit: unit(&m.unit), value: m.value, max: m.max, counter: m.counter })
                        .collect(),
                    info: snap.info,
                    notes: snap.notes,
                };
                Response::ok(0, &view)
            }
            Call::Kill { connection_id, database, session_id } => {
                if !device.can_kill {
                    return Err("este celular no tiene permiso para terminar sesiones: se habilita en DBine, Configuración › Dispositivos".into());
                }
                let entry = state.session(&key(&connection_id, &database), &connection_id, &database).await.map_err(|e| e.to_string())?;
                entry.session.lock().await.kill_session(&session_id).await.map_err(|e| e.to_string())?;
                tracing::info!(device = %device.id, connection = %connection_id, session = %session_id, "session ended from a phone");
                Response::ok(0, &true)
            }
        })
    }

    /// A dropped connection would fail every call: reconnect next time.
    fn forget_if_dropped<T>(&self, key: &str, r: dbine_driver::Result<T>) -> Result<T, String> {
        if matches!(r, Err(dbine_driver::Error::Connect(_) | dbine_driver::Error::ConnectFailed(_) | dbine_driver::Error::Io(_))) {
            self.state.sessions.remove(key);
        }
        r.map_err(|e| e.to_string())
    }
}

pub mod commands {
    use super::*;
    use crate::error::{CommandError, CommandResult};
    use tauri::State;

    #[derive(Serialize)]
    pub struct LinkStatus {
        pub enabled: bool,
        pub port: u16,
        pub running: bool,
        pub error: Option<String>,
        pub fingerprint: String,
        pub devices: Vec<Device>,
        pub remote: RemoteSetting,
        /// The relay DBine is reachable through now (outside the network).
        pub remote_relay: Option<String>,
        pub remote_error: Option<String>,
    }

    fn status(rt: &LinkRuntime) -> CommandResult<LinkStatus> {
        let s = rt.settings();
        let (running, error) = rt.status();
        let (remote_relay, remote_error) = rt.inner.remote_status();
        Ok(LinkStatus {
            enabled: s.enabled,
            port: s.port,
            running,
            error,
            fingerprint: rt.desktop_fingerprint().map_err(CommandError::Internal)?,
            devices: s.devices,
            remote: s.remote,
            remote_relay,
            remote_error,
        })
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn link_status(rt: State<'_, LinkRuntime>) -> CommandResult<LinkStatus> {
        status(&rt)
    }

    #[derive(Deserialize)]
    pub struct ConfigureArgs {
        pub enabled: bool,
        pub port: u16,
        #[serde(default)]
        pub remote: Option<RemoteSetting>,
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn link_configure(rt: State<'_, LinkRuntime>, args: ConfigureArgs) -> CommandResult<LinkStatus> {
        if args.port < 1024 {
            return Err(CommandError::BadRequest("elegí un puerto de 1024 en adelante".into()));
        }
        if let Some(RemoteSetting::Own { relays }) = &args.remote {
            if relays.is_empty() {
                return Err(CommandError::BadRequest("indicá al menos un servidor de paso".into()));
            }
            dbine_link::net::check_relays(relays).map_err(CommandError::BadRequest)?;
        }
        rt.save(|s| {
            s.enabled = args.enabled;
            s.port = args.port;
            if let Some(r) = args.remote {
                s.remote = r;
            }
        })
        .map_err(CommandError::Internal)?;
        status(&rt)
    }

    #[derive(Serialize)]
    pub struct InviteOut {
        pub svg: String,
        pub fingerprint: String,
        pub seconds: u64,
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn link_invite(rt: State<'_, LinkRuntime>) -> CommandResult<InviteOut> {
        let text = rt.invite().map_err(CommandError::BadRequest)?;
        Ok(InviteOut { svg: qr_svg(&text).map_err(CommandError::Internal)?, fingerprint: rt.desktop_fingerprint().map_err(CommandError::Internal)?, seconds: INVITE_LIFE.as_secs() })
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn link_cancel_invite(rt: State<'_, LinkRuntime>) -> CommandResult<()> {
        rt.cancel_invite();
        Ok(())
    }

    #[derive(Deserialize)]
    pub struct AnswerArgs {
        pub accept: bool,
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn link_answer(rt: State<'_, LinkRuntime>, args: AnswerArgs) -> CommandResult<()> {
        rt.answer(args.accept);
        Ok(())
    }

    #[derive(Deserialize)]
    pub struct DeviceArgs {
        pub id: String,
        #[serde(default)]
        pub can_kill: Option<bool>,
        #[serde(default)]
        pub can_run_tasks: Option<bool>,
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn link_remove_device(rt: State<'_, LinkRuntime>, args: DeviceArgs) -> CommandResult<LinkStatus> {
        rt.save(|s| s.devices.retain(|d| d.id != args.id)).map_err(CommandError::Internal)?;
        status(&rt)
    }

    #[tauri::command(rename_all = "camelCase")]
    pub async fn link_set_device(rt: State<'_, LinkRuntime>, args: DeviceArgs) -> CommandResult<LinkStatus> {
        rt.save(|s| {
            if let Some(d) = s.devices.iter_mut().find(|d| d.id == args.id) {
                if let Some(k) = args.can_kill {
                    d.can_kill = k;
                }
                if let Some(r) = args.can_run_tasks {
                    d.can_run_tasks = r;
                }
            }
        })
        .map_err(CommandError::Internal)?;
        status(&rt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpStream;
    use dbine_core::state::SavedConnection;
    use dbine_core::StateStore;
    use dbine_driver::ConnectionConfig;
    use dbine_link::proto::{Call, ConnectionItem, Hello, PairResult, Request, Response};

    async fn ask<S: dbine_link::Io>(ch: &mut dbine_link::Channel<S>, id: u64, call: Call) -> Response {
        ch.send_json(&Request { id, call }).await.unwrap();
        let r: Response = ch.recv_json().await.unwrap();
        assert_eq!(r.id, id);
        r
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pair_confirm_ask_and_revoke() {
        let dir = std::env::temp_dir().join(format!("dbine-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.sqlite");
        rusqlite::Connection::open(&file).unwrap().execute_batch("CREATE TABLE t (x INTEGER);").unwrap();
        let state = AppState::new(StateStore::open_in_memory().unwrap());
        state
            .store
            .save_connection(&SavedConnection {
                id: "a".into(),
                name: "base a".into(),
                color: None,
                config: ConnectionConfig { driver: "sqlite".into(), host: file.to_string_lossy().into_owned(), ..Default::default() },
                save_password: false,
                folder_id: None,
                tags: vec![],
                mcp_level: None,
                updated_at: String::new(),
            })
            .unwrap();
        let rt = LinkRuntime::new(state);
        *rt.inner.keys.lock().unwrap() = Some(Keys::generate().unwrap());
        // Off: no invite, nothing listening.
        assert!(rt.invite().is_err());
        // A free port can be taken by another process before DBine binds
        // it: try another one then.
        let mut port = 0;
        for _ in 0..5 {
            port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
            rt.save(|s| {
                s.enabled = true;
                s.port = port;
                // The internet path has its own check (dbine-link's `remote`
                // example): this test stays on this machine.
                s.remote = RemoteSetting::Off;
            })
            .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            if rt.status().0 {
                break;
            }
        }
        assert!(rt.status().0, "{:?}", rt.status());
        let invite = Invite::decode(&rt.invite().unwrap()).unwrap();
        let addr = format!("127.0.0.1:{port}");
        let phone = Keys::generate().unwrap();

        // A stranger without the code is refused.
        let stranger = Keys::generate().unwrap();
        let s = TcpStream::connect(&addr).await.unwrap();
        assert!(dbine_link::connect(s, &invite.key, &stranger).await.is_err());

        // Pairing: the user says yes on the computer.
        let s = TcpStream::connect(&addr).await.unwrap();
        let mut ch = dbine_link::pair(s, &invite, &phone, b"Pixel de prueba").await.unwrap();
        ch.send(b"ready").await.unwrap();
        for _ in 0..50 {
            if rt.inner.pending.lock().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        rt.answer(true);
        let result: PairResult = ch.recv_json().await.unwrap();
        assert!(result.accepted);
        let devices = rt.settings().devices;
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "Pixel de prueba");
        assert!(!devices[0].can_kill, "ending sessions is off until allowed");

        // The code was for one phone.
        let s = TcpStream::connect(&addr).await.unwrap();
        let other = Keys::generate().unwrap();
        let second = dbine_link::pair(s, &invite, &other, b"otro").await;
        assert!(second.is_err(), "a used code doesn't pair again");

        // Connected: what it may see and do.
        let s = TcpStream::connect(&addr).await.unwrap();
        let mut ch = dbine_link::connect(s, &invite.key, &phone).await.unwrap();
        let hello: Hello = serde_json::from_value(ask(&mut ch, 1, Call::Hello).await.ok.unwrap()).unwrap();
        assert!(!hello.can_kill);
        let conns: Vec<ConnectionItem> = serde_json::from_value(ask(&mut ch, 2, Call::Connections).await.ok.unwrap()).unwrap();
        assert_eq!(conns.len(), 1);
        assert_eq!(conns[0].name, "base a");
        assert!(conns[0].monitor);
        let mon: dbine_link::proto::MonitorView = serde_json::from_value(ask(&mut ch, 20, Call::Monitor { connection_id: "a".into() }).await.ok.unwrap()).unwrap();
        assert!(!mon.metrics.is_empty(), "{mon:?}");
        assert!(mon.metrics.iter().all(|m| ["percent", "bytes", "count", "millis", "seconds"].contains(&m.unit.as_str())));
        let kill = ask(&mut ch, 3, Call::Kill { connection_id: "a".into(), database: "".into(), session_id: "1".into() }).await;
        assert!(kill.error.unwrap().contains("no tiene permiso"));
        // Notices: a long query and a task that failed.
        let first: Events = serde_json::from_value(ask(&mut ch, 21, Call::Events { after: None, boot: None }).await.ok.unwrap()).unwrap();
        assert!(first.events.is_empty());
        // An editor query: shown while it runs, then recorded with how it ended.
        let guard = rt.query_started("tab-1", "a", "", "SELECT *\nFROM ventas\nWHERE año = 2026");
        let q: QueriesView = serde_json::from_value(ask(&mut ch, 30, Call::Queries).await.ok.unwrap()).unwrap();
        assert_eq!(q.running.len(), 1);
        assert_eq!(q.running[0].sql, "SELECT * FROM ventas WHERE año = 2026");
        assert_eq!(q.running[0].place, "base a");
        // Cancelling needs the permission.
        let id = q.running[0].id.clone();
        assert!(ask(&mut ch, 31, Call::CancelQuery { query_id: id.clone() }).await.error.unwrap().contains("no tiene permiso"));
        // It ran long (as if 200 s) and failed.
        rt.inner.running.lock().unwrap().get_mut(&id).unwrap().started = Instant::now() - Duration::from_secs(200);
        let out = dbine_driver::QueryOutcome { error: Some("relation \"x\" does not exist".into()), ..Default::default() };
        guard.finish(&out);
        let q: QueriesView = serde_json::from_value(ask(&mut ch, 32, Call::Queries).await.ok.unwrap()).unwrap();
        assert!(q.running.is_empty());
        assert_eq!(q.recent.len(), 1);
        assert!(!q.recent[0].ok && q.recent[0].summary.contains("does not exist"));
        // A short one leaves no trace, and a run that failed early leaves the list.
        rt.query_started("tab-2", "a", "", "SELECT 1").finish(&dbine_driver::QueryOutcome::default());
        drop(rt.query_started("tab-3", "a", "", "SELECT 2"));
        let q: QueriesView = serde_json::from_value(ask(&mut ch, 33, Call::Queries).await.ok.unwrap()).unwrap();
        assert!(q.running.is_empty() && q.recent.len() == 1);
        rt.inner
            .state
            .store
            .save_task(&dbine_core::tasks::ScheduledTask { id: "t1".into(), name: "backup nocturno".into(), steps: vec![dbine_core::tasks::Step::default()], ..Default::default() })
            .unwrap();
        rt.inner
            .state
            .store
            .save_task_run(&dbine_core::tasks::TaskRun {
                id: "r1".into(),
                task_id: "t1".into(),
                trigger: "schedule".into(),
                status: dbine_core::tasks::RunStatus::Failed,
                error: Some("sin espacio".into()),
                ..Default::default()
            })
            .unwrap();
        rt.inner.watch_tasks(false);
        let news: Events = serde_json::from_value(ask(&mut ch, 22, Call::Events { after: Some(first.last), boot: Some(first.boot.clone()) }).await.ok.unwrap()).unwrap();
        assert_eq!(news.events.len(), 2, "{news:?}");
        assert_eq!(news.events[0].kind, "query");
        assert!(news.events[0].body.starts_with("Falló después de 3 min 20 s"), "{}", news.events[0].body);
        assert_eq!(news.events[1].title, "backup nocturno");
        assert_eq!(news.events[1].body, "sin espacio");
        // Seen once: not again.
        rt.inner.watch_tasks(false);
        let again: Events = serde_json::from_value(ask(&mut ch, 23, Call::Events { after: Some(news.last), boot: Some(news.boot.clone()) }).await.ok.unwrap()).unwrap();
        assert!(again.events.is_empty());
        // Tasks: listed; running one needs the permission.
        let tasks: Vec<TaskItem> = serde_json::from_value(ask(&mut ch, 24, Call::Tasks).await.ok.unwrap()).unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].last.as_ref().unwrap().status, "failed");
        assert!(ask(&mut ch, 25, Call::RunTask { task_id: "t1".into() }).await.error.unwrap().contains("no tiene permiso"));
        assert!(ask(&mut ch, 27, Call::CancelTaskRun { run_id: "r1".into() }).await.error.unwrap().contains("no tiene permiso"));
        assert_eq!(tasks[0].steps.len(), 1);
        // The health check of a database.
        let health: HealthView = serde_json::from_value(ask(&mut ch, 26, Call::Health { connection_id: "a".into(), database: "".into() }).await.ok.unwrap()).unwrap();
        assert!(!health.checks.is_empty() || !health.skipped.is_empty(), "{health:?}");

        // SQLite has no locks: an error, not a hang.
        assert!(ask(&mut ch, 4, Call::Locks { connection_id: "a".into(), database: "".into() }).await.error.is_some());

        // Removing the phone cuts it off.
        let id = devices[0].id.clone();
        rt.save(|s| s.devices.retain(|d| d.id != id)).unwrap();
        ch.send_json(&Request { id: 5, call: Call::Hello }).await.ok();
        assert!(ch.recv_json::<Response>().await.is_err());
        let s = TcpStream::connect(&addr).await.unwrap();
        assert!(dbine_link::connect(s, &invite.key, &phone).await.is_err());

        // Off again: nothing listens.
        rt.save(|s| s.enabled = false).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(TcpStream::connect(&addr).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Leaving the local network through a third party is never a default:
    /// a fresh install, and settings saved before the option existed, stay
    /// on the local network (docs/mobile-companion.md, "Security model").
    #[test]
    fn internet_is_off_unless_chosen() {
        assert_eq!(LinkSettings::default().remote, RemoteSetting::Off);
        let old: LinkSettings = serde_json::from_value(serde_json::json!({"enabled": true, "port": 27518, "devices": []})).unwrap();
        assert_eq!(old.remote, RemoteSetting::Off);
        assert!(RemoteSetting::Off.relays().is_none());
    }

    #[test]
    fn pairing_attempts_are_limited() {
        let rt = LinkRuntime::new(AppState::new(StateStore::open_in_memory().unwrap()));
        for _ in 0..PAIR_ATTEMPTS {
            assert!(rt.inner.allow_pairing());
        }
        assert!(!rt.inner.allow_pairing());
    }

    #[test]
    fn qr_renders() {
        let svg = qr_svg("dbine-link:1:abc").unwrap();
        assert!(svg.starts_with("<?xml") || svg.starts_with("<svg"));
    }
}
