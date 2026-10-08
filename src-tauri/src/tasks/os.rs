//! Keeping the OS scheduler in step with the tasks: one entry per enabled
//! task that runs `dbine --run-task <id>`. The entry carries the task id and
//! nothing else (no connection, no secret).
//!
//! - macOS: a LaunchAgent (`~/Library/LaunchAgents`), so it runs in the
//!   user's session with access to the login keychain.
//! - Windows: Task Scheduler, "only when the user is logged on" (the
//!   Credential Manager needs the user's session).
//! - Linux: a systemd user timer; cron where there's no systemd.
//!
//! The builders are plain functions (tested everywhere); only `register`
//! and `unregister` touch the system.

// Each OS uses its own builders; the tests use them all.
#![allow(dead_code)]

use dbine_core::tasks::{Schedule, ScheduledTask};
use std::path::{Path, PathBuf};
#[cfg(not(windows))]
use std::process::Command;

pub const LABEL_PREFIX: &str = "com.addlayer.dbine.task.";

fn hm(time: &str) -> (u32, u32) {
    let (h, m) = time.trim().split_once(':').unwrap_or(("0", "0"));
    (h.parse().unwrap_or(0).min(23), m.parse().unwrap_or(0).min(59))
}

/// The program the scheduler starts. Refused when it would break after the
/// next start: a disk image, a quarantined copy macOS moved
/// (AppTranslocation), or a development build.
pub fn executable() -> Result<PathBuf, String> {
    if cfg!(debug_assertions) && std::env::var_os("DBINE_ALLOW_DEV_SCHEDULE").is_none() {
        return Err("Las tareas programadas no se registran desde una versión de desarrollo: usá la app instalada.".into());
    }
    // An AppImage runs from a mount that changes each start: its own path.
    let exe = std::env::var_os("APPIMAGE").map(PathBuf::from).map_or_else(std::env::current_exe, Ok).map_err(|e| e.to_string())?;
    let s = exe.to_string_lossy();
    if s.starts_with("/Volumes/") || s.contains("/AppTranslocation/") {
        return Err("DBine se está ejecutando desde la imagen de disco o desde una copia temporal de macOS: movelo a Aplicaciones y abrilo desde ahí para programar tareas.".into());
    }
    Ok(exe)
}

// -- macOS -------------------------------------------------------------------

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// launchd's Weekday: 0 or 7 is Sunday, 1 Monday (ours: 1 Monday … 7 Sunday).
pub fn launchd_plist(task_id: &str, exe: &Path, schedule: &Schedule) -> String {
    let cal = |extra: &str, time: &str| {
        let (h, m) = hm(time);
        format!("<dict>{extra}<key>Hour</key><integer>{h}</integer><key>Minute</key><integer>{m}</integer></dict>")
    };
    let when = match schedule {
        Schedule::Daily { time } => format!("<key>StartCalendarInterval</key>{}", cal("", time)),
        Schedule::Weekly { days, time } => format!(
            "<key>StartCalendarInterval</key><array>{}</array>",
            days.iter().map(|d| cal(&format!("<key>Weekday</key><integer>{}</integer>", d % 7), time)).collect::<String>()
        ),
        Schedule::Monthly { day, time } => format!("<key>StartCalendarInterval</key>{}", cal(&format!("<key>Day</key><integer>{day}</integer>"), time)),
        Schedule::Interval { minutes } => format!("<key>StartInterval</key><integer>{}</integer>", minutes * 60),
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
<key>Label</key><string>{LABEL_PREFIX}{id}</string>
<key>ProgramArguments</key><array><string>{exe}</string><string>--run-task</string><string>{id}</string></array>
{when}
<key>ProcessType</key><string>Background</string>
</dict>
</plist>
"#,
        id = xml(task_id),
        exe = xml(&exe.to_string_lossy()),
    )
}

// -- Windows -----------------------------------------------------------------

/// Task Scheduler's XML (schtasks /XML): logged-on user only.
pub fn schtasks_xml(task_id: &str, exe: &Path, schedule: &Schedule, now: chrono::NaiveDateTime) -> String {
    const DAYS: [&str; 7] = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];
    let start = |time: &str| {
        let (h, m) = hm(time);
        format!("<StartBoundary>{}T{h:02}:{m:02}:00</StartBoundary>", now.format("%Y-%m-%d"))
    };
    let trigger = match schedule {
        Schedule::Daily { time } => format!("<CalendarTrigger>{}<ScheduleByDay><DaysInterval>1</DaysInterval></ScheduleByDay></CalendarTrigger>", start(time)),
        Schedule::Weekly { days, time } => format!(
            "<CalendarTrigger>{}<ScheduleByWeek><WeeksInterval>1</WeeksInterval><DaysOfWeek>{}</DaysOfWeek></ScheduleByWeek></CalendarTrigger>",
            start(time),
            days.iter().filter_map(|d| DAYS.get((*d as usize).wrapping_sub(1))).map(|d| format!("<{d} />")).collect::<String>()
        ),
        Schedule::Monthly { day, time } => format!(
            "<CalendarTrigger>{}<ScheduleByMonth><DaysOfMonth><Day>{day}</Day></DaysOfMonth><Months>{}</Months></ScheduleByMonth></CalendarTrigger>",
            start(time),
            ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"]
                .iter()
                .map(|m| format!("<{m} />"))
                .collect::<String>()
        ),
        Schedule::Interval { minutes } => format!(
            "<TimeTrigger>{}<Repetition><Interval>PT{minutes}M</Interval><StopAtDurationEnd>false</StopAtDurationEnd></Repetition></TimeTrigger>",
            start(&now.format("%H:%M").to_string())
        ),
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo><Description>DBine: tarea programada {id}</Description></RegistrationInfo>
  <Triggers>{trigger}</Triggers>
  <Principals><Principal id="Author"><LogonType>InteractiveToken</LogonType><RunLevel>LeastPrivilege</RunLevel></Principal></Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <StartWhenAvailable>true</StartWhenAvailable>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Enabled>true</Enabled>
  </Settings>
  <Actions Context="Author"><Exec><Command>{exe}</Command><Arguments>--run-task {id}</Arguments></Exec></Actions>
</Task>
"#,
        id = xml(task_id),
        exe = xml(&exe.to_string_lossy()),
    )
}

pub fn schtasks_name(task_id: &str) -> String {
    format!("DBine\\task-{task_id}")
}

// -- Linux -------------------------------------------------------------------

/// systemd's `OnCalendar=` (or the interval lines) for the timer.
pub fn systemd_timer(task_id: &str, schedule: &Schedule) -> String {
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    let when = match schedule {
        Schedule::Daily { time } => {
            let (h, m) = hm(time);
            format!("OnCalendar=*-*-* {h:02}:{m:02}:00\nPersistent=true")
        }
        Schedule::Weekly { days, time } => {
            let (h, m) = hm(time);
            let names: Vec<&str> = days.iter().filter_map(|d| DAYS.get((*d as usize).wrapping_sub(1)).copied()).collect();
            format!("OnCalendar={} *-*-* {h:02}:{m:02}:00\nPersistent=true", names.join(","))
        }
        Schedule::Monthly { day, time } => {
            let (h, m) = hm(time);
            format!("OnCalendar=*-*-{day:02} {h:02}:{m:02}:00\nPersistent=true")
        }
        Schedule::Interval { minutes } => format!("OnActiveSec={minutes}min\nOnUnitActiveSec={minutes}min"),
    };
    format!("[Unit]\nDescription=DBine: tarea programada {task_id}\n\n[Timer]\n{when}\n\n[Install]\nWantedBy=timers.target\n")
}

pub fn systemd_service(task_id: &str, exe: &Path) -> String {
    let quoted = format!("\"{}\"", exe.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\""));
    format!("[Unit]\nDescription=DBine: tarea programada {task_id}\n\n[Service]\nType=oneshot\nExecStart={quoted} --run-task {task_id}\n")
}

pub const CRON_TAG: &str = "# dbine-task:";

/// The crontab line (tagged with the id, to find it again).
pub fn cron_line(task_id: &str, exe: &Path, schedule: &Schedule) -> String {
    let when = match schedule {
        Schedule::Daily { time } => {
            let (h, m) = hm(time);
            format!("{m} {h} * * *")
        }
        Schedule::Weekly { days, time } => {
            let (h, m) = hm(time);
            format!("{m} {h} * * {}", days.iter().map(|d| (d % 7).to_string()).collect::<Vec<_>>().join(","))
        }
        Schedule::Monthly { day, time } => {
            let (h, m) = hm(time);
            format!("{m} {h} {day} * *")
        }
        Schedule::Interval { minutes } if *minutes < 60 => format!("*/{minutes} * * * *"),
        Schedule::Interval { minutes } => format!("0 */{} * * *", (minutes / 60).max(1)),
    };
    let exe = format!("'{}'", exe.to_string_lossy().replace('\'', "'\\''"));
    format!("{when} {exe} --run-task {task_id} {CRON_TAG}{task_id}")
}

// -- register / unregister ---------------------------------------------------

/// Make the OS scheduler run `task` as it says (or not at all when it's
/// disabled).
pub fn sync(task: &ScheduledTask) -> Result<(), String> {
    if !task.enabled {
        return unregister(&task.id);
    }
    let exe = executable()?;
    register(task, &exe)
}

#[cfg(not(windows))]
fn run(cmd: &mut Command) -> Result<String, String> {
    let out = cmd.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

#[cfg(target_os = "macos")]
fn agent_path(task_id: &str) -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("sin carpeta de usuario")?;
    Ok(PathBuf::from(home).join("Library/LaunchAgents").join(format!("{LABEL_PREFIX}{task_id}.plist")))
}

#[cfg(target_os = "macos")]
fn gui_domain() -> Result<String, String> {
    Ok(format!("gui/{}", run(Command::new("/usr/bin/id").arg("-u"))?.trim()))
}

#[cfg(target_os = "macos")]
fn register(task: &ScheduledTask, exe: &Path) -> Result<(), String> {
    let path = agent_path(&task.id)?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let domain = gui_domain()?;
    // Replace a previous one: launchd keeps what it loaded until booted out.
    let _ = run(Command::new("/bin/launchctl").args(["bootout", &format!("{domain}/{LABEL_PREFIX}{}", task.id)]));
    std::fs::write(&path, launchd_plist(&task.id, exe, &task.schedule)).map_err(|e| e.to_string())?;
    run(Command::new("/bin/launchctl").arg("bootstrap").arg(&domain).arg(&path))
        .map(|_| ())
        .map_err(|e| format!("macOS no aceptó la tarea programada: {e}"))
}

#[cfg(target_os = "macos")]
pub fn unregister(task_id: &str) -> Result<(), String> {
    let domain = gui_domain()?;
    let _ = run(Command::new("/bin/launchctl").args(["bootout", &format!("{domain}/{LABEL_PREFIX}{task_id}")]));
    match std::fs::remove_file(agent_path(task_id)?) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.to_string()),
        _ => Ok(()),
    }
}

#[cfg(target_os = "macos")]
pub fn is_registered(task_id: &str) -> bool {
    agent_path(task_id).is_ok_and(|p| p.exists())
}

#[cfg(windows)]
fn schtasks(args: &[&str]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let out = std::process::Command::new("schtasks").args(args).creation_flags(CREATE_NO_WINDOW).output().map_err(|e| e.to_string())?;
    if out.status.success() { Ok(()) } else { Err(String::from_utf8_lossy(&out.stderr).trim().to_string()) }
}

#[cfg(windows)]
fn register(task: &ScheduledTask, exe: &Path) -> Result<(), String> {
    let xml = schtasks_xml(&task.id, exe, &task.schedule, chrono::Local::now().naive_local());
    // schtasks reads the XML as UTF-16 (with its BOM).
    let mut bytes = vec![0xFF, 0xFE];
    bytes.extend(xml.encode_utf16().flat_map(|u| u.to_le_bytes()));
    let file = std::env::temp_dir().join(format!("dbine-task-{}.xml", task.id));
    std::fs::write(&file, bytes).map_err(|e| e.to_string())?;
    let r = schtasks(&["/Create", "/TN", &schtasks_name(&task.id), "/XML", &file.to_string_lossy(), "/F"]);
    let _ = std::fs::remove_file(&file);
    r.map_err(|e| format!("Windows no aceptó la tarea programada: {e}"))
}

#[cfg(windows)]
pub fn unregister(task_id: &str) -> Result<(), String> {
    if !is_registered(task_id) {
        return Ok(());
    }
    schtasks(&["/Delete", "/TN", &schtasks_name(task_id), "/F"])
}

#[cfg(windows)]
pub fn is_registered(task_id: &str) -> bool {
    schtasks(&["/Query", "/TN", &schtasks_name(task_id)]).is_ok()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn units_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .ok_or("sin carpeta de usuario")?;
    Ok(base.join("systemd/user"))
}

#[cfg(all(unix, not(target_os = "macos")))]
fn has_systemd() -> bool {
    run(Command::new("systemctl").args(["--user", "is-system-running"])).is_ok()
        || std::path::Path::new("/run/systemd/system").exists() && run(Command::new("systemctl").args(["--user", "--version"])).is_ok()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn unit(task_id: &str) -> String {
    format!("dbine-task-{task_id}")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn crontab_without(task_id: &str) -> Result<String, String> {
    let current = run(Command::new("crontab").arg("-l")).unwrap_or_default();
    let tag = format!("{CRON_TAG}{task_id}");
    Ok(current.lines().filter(|l| !l.trim_end().ends_with(&tag)).map(|l| format!("{l}\n")).collect())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn write_crontab(text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut child = Command::new("crontab").arg("-").stdin(std::process::Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    child.stdin.take().ok_or("crontab")?.write_all(text.as_bytes()).map_err(|e| e.to_string())?;
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() { Ok(()) } else { Err(format!("crontab terminó con {status}")) }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn register(task: &ScheduledTask, exe: &Path) -> Result<(), String> {
    if has_systemd() {
        let dir = units_dir()?;
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let name = unit(&task.id);
        std::fs::write(dir.join(format!("{name}.service")), systemd_service(&task.id, exe)).map_err(|e| e.to_string())?;
        std::fs::write(dir.join(format!("{name}.timer")), systemd_timer(&task.id, &task.schedule)).map_err(|e| e.to_string())?;
        run(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
        run(Command::new("systemctl").args(["--user", "enable", "--now", &format!("{name}.timer")]))
            .map(|_| ())
            .map_err(|e| format!("systemd no aceptó la tarea programada: {e}"))?;
        // Restart so a changed schedule takes effect now.
        let _ = run(Command::new("systemctl").args(["--user", "restart", &format!("{name}.timer")]));
        return Ok(());
    }
    let mut text = crontab_without(&task.id)?;
    text.push_str(&cron_line(&task.id, exe, &task.schedule));
    text.push('\n');
    write_crontab(&text)
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn unregister(task_id: &str) -> Result<(), String> {
    let name = unit(task_id);
    if let Ok(dir) = units_dir() {
        let timer = dir.join(format!("{name}.timer"));
        if timer.exists() {
            let _ = run(Command::new("systemctl").args(["--user", "disable", "--now", &format!("{name}.timer")]));
            let _ = std::fs::remove_file(&timer);
            let _ = std::fs::remove_file(dir.join(format!("{name}.service")));
            let _ = run(Command::new("systemctl").args(["--user", "daemon-reload"]));
        }
    }
    let current = run(Command::new("crontab").arg("-l")).unwrap_or_default();
    if current.contains(&format!("{CRON_TAG}{task_id}")) {
        write_crontab(&crontab_without(task_id)?)?;
    }
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
pub fn is_registered(task_id: &str) -> bool {
    units_dir().is_ok_and(|d| d.join(format!("{}.timer", unit(task_id))).exists())
        || run(Command::new("crontab").arg("-l")).is_ok_and(|c| c.contains(&format!("{CRON_TAG}{task_id}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe() -> PathBuf {
        PathBuf::from("/Applications/DBine.app/Contents/MacOS/dbine")
    }

    #[test]
    fn launchd() {
        let p = launchd_plist("t1", &exe(), &Schedule::Weekly { days: vec![1, 7], time: "08:05".into() });
        assert!(p.contains("<string>com.addlayer.dbine.task.t1</string>"));
        assert!(p.contains("<key>Weekday</key><integer>1</integer><key>Hour</key><integer>8</integer><key>Minute</key><integer>5</integer>"));
        assert!(p.contains("<key>Weekday</key><integer>0</integer>"));
        assert!(p.contains("<string>--run-task</string><string>t1</string>"));
        let p = launchd_plist("t1", &exe(), &Schedule::Interval { minutes: 15 });
        assert!(p.contains("<key>StartInterval</key><integer>900</integer>"));
    }

    #[test]
    fn windows() {
        let now = chrono::NaiveDate::from_ymd_opt(2026, 10, 8).unwrap().and_hms_opt(9, 30, 0).unwrap();
        let x = schtasks_xml("t1", Path::new("C:\\Program Files\\DBine\\dbine.exe"), &Schedule::Weekly { days: vec![2, 6], time: "07:00".into() }, now);
        assert!(x.contains("<StartBoundary>2026-10-08T07:00:00</StartBoundary>"));
        assert!(x.contains("<DaysOfWeek><Tuesday /><Saturday /></DaysOfWeek>"));
        assert!(x.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(x.contains("<Arguments>--run-task t1</Arguments>"));
        let x = schtasks_xml("t1", &exe(), &Schedule::Interval { minutes: 30 }, now);
        assert!(x.contains("<Interval>PT30M</Interval>"));
    }

    #[test]
    fn linux() {
        assert!(systemd_timer("t1", &Schedule::Weekly { days: vec![1, 3], time: "22:00".into() }).contains("OnCalendar=Mon,Wed *-*-* 22:00:00"));
        assert!(systemd_timer("t1", &Schedule::Monthly { day: 5, time: "01:30".into() }).contains("OnCalendar=*-*-05 01:30:00"));
        assert!(systemd_service("t1", Path::new("/opt/DBine/dbine")).contains("ExecStart=\"/opt/DBine/dbine\" --run-task t1"));
        assert_eq!(cron_line("t1", Path::new("/opt/dbine"), &Schedule::Daily { time: "06:45".into() }), "45 6 * * * '/opt/dbine' --run-task t1 # dbine-task:t1");
        assert!(cron_line("t1", Path::new("/opt/dbine"), &Schedule::Weekly { days: vec![7], time: "06:00".into() }).starts_with("0 6 * * 0 "));
        assert!(cron_line("t1", Path::new("/opt/dbine"), &Schedule::Interval { minutes: 10 }).starts_with("*/10 * * * * "));
    }
}

/// Registers and removes a real entry (macOS: launchd). Run by hand:
/// `DBINE_ALLOW_DEV_SCHEDULE=1 cargo test -p dbine --lib os_live -- --ignored`.
#[cfg(all(test, target_os = "macos"))]
mod os_live {
    use super::*;

    #[test]
    #[ignore]
    fn register_and_unregister() {
        let task = ScheduledTask {
            id: format!("live-test-{}", std::process::id()),
            name: "live".into(),
            enabled: true,
            schedule: Schedule::Monthly { day: 28, time: "03:00".into() },
            ..Default::default()
        };
        sync(&task).unwrap();
        assert!(is_registered(&task.id));
        let listed = std::process::Command::new("/bin/launchctl").arg("list").output().unwrap();
        assert!(String::from_utf8_lossy(&listed.stdout).contains(&format!("{LABEL_PREFIX}{}", task.id)));
        let plist = agent_path(&task.id).unwrap();
        assert!(std::process::Command::new("/usr/bin/plutil").arg("-lint").arg(&plist).status().unwrap().success());
        // Registering again replaces it.
        sync(&task).unwrap();
        unregister(&task.id).unwrap();
        assert!(!is_registered(&task.id));
        let listed = std::process::Command::new("/bin/launchctl").arg("list").output().unwrap();
        assert!(!String::from_utf8_lossy(&listed.stdout).contains(&task.id));
    }
}
