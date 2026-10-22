//! Keeps the computer from going to sleep while a long query runs: sleep
//! drops the connection and the query dies with nobody watching. Only idle
//! sleep is held off: the display still turns off, and closing the lid or
//! choosing Sleep still sleeps.
//!
//! - macOS: `caffeinate -i -w <DBine's pid>` (it also ends if DBine does).
//! - Linux: `systemd-inhibit --what=idle:sleep … sleep infinity`; without
//!   systemd, nothing.
//! - Windows: `SetThreadExecutionState(ES_SYSTEM_REQUIRED)` on a thread of
//!   its own that holds it until released.

use std::sync::Mutex;

pub struct Awake {
    held: Mutex<Option<Hold>>,
}

#[cfg(not(windows))]
type Hold = std::process::Child;
#[cfg(windows)]
type Hold = std::sync::mpsc::Sender<()>;

impl Awake {
    pub const fn new() -> Awake {
        Awake {
            held: Mutex::new(None),
        }
    }

    /// Hold the computer awake (or release it).
    pub fn set(&self, on: bool) {
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        match (on, held.is_some()) {
            (true, false) => {
                *held = start();
                if held.is_some() {
                    tracing::info!("keeping the computer awake: a long query is running");
                }
            }
            (false, true) => {
                if let Some(h) = held.take() {
                    stop(h);
                    tracing::info!("the computer may sleep again");
                }
            }
            _ => {}
        }
    }
}

impl Drop for Awake {
    fn drop(&mut self) {
        self.set(false);
    }
}

#[cfg(target_os = "macos")]
fn start() -> Option<Hold> {
    std::process::Command::new("/usr/bin/caffeinate")
        .args(["-i", "-w", &std::process::id().to_string()])
        .spawn()
        .ok()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn start() -> Option<Hold> {
    std::process::Command::new("systemd-inhibit")
        .args([
            "--what=idle:sleep",
            "--who=DBine",
            "--why=Una consulta está en curso",
            "--mode=block",
            "sleep",
            "infinity",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()
}

#[cfg(not(windows))]
fn stop(mut child: Hold) {
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(windows)]
fn start() -> Option<Hold> {
    use windows::Win32::System::Power::{
        SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED,
    };
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::Builder::new()
        .name("dbine-awake".into())
        .spawn(move || {
            // The state belongs to this thread: it holds while it waits.
            unsafe { SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED) };
            let _ = rx.recv();
            unsafe { SetThreadExecutionState(ES_CONTINUOUS) };
        })
        .ok()?;
    Some(tx)
}

#[cfg(windows)]
fn stop(h: Hold) {
    let _ = h.send(());
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn holds_and_releases() {
        let a = Awake::new();
        a.set(true);
        let pid = {
            let held = a.held.lock().unwrap();
            held.as_ref().map(|c| c.id()).expect("caffeinate started")
        };
        let alive = |pid: u32| {
            std::process::Command::new("/bin/kill")
                .args(["-0", &pid.to_string()])
                .status()
                .is_ok_and(|s| s.success())
        };
        assert!(alive(pid));
        // Holding again doesn't start a second one.
        a.set(true);
        assert_eq!(a.held.lock().unwrap().as_ref().map(|c| c.id()), Some(pid));
        a.set(false);
        assert!(!alive(pid), "released");
    }
}
