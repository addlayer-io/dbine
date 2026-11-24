//! The lineage's diagnostic log (`lineage-diagnostic.log`): one plain-text line per stage of a read,
//! with its duration, counts and result, so a slow lineage on somebody else's machine can be measured
//! instead of guessed at (docs/lineage-diagnostic.md).
//!
//! Off unless the app turns it on ([`configure`]), and then the cost of a stage is one atomic load
//! when it is off. A driver host (a separate process) finds the same file through the environment
//! ([`ENV`]). Lines carry no host names, no secrets, no tokens and no row data; object names are
//! written as they are unless the user asked for them hashed (`redact`). Messages go through
//! [`scrub`]. The file is capped: past `max_bytes` it moves to `<name>.1` (one old file is kept).

use std::fmt::Display;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// The file's name in the log folder.
pub const FILE_NAME: &str = "lineage-diagnostic.log";
/// The environment variable a driver host reads: `<redact 0|1>|<path>`.
pub const ENV: &str = "DBINE_LINEAGE_DIAG";
/// The size a file may reach before it rotates.
pub const DEFAULT_MAX_BYTES: u64 = 5 * 1024 * 1024;
/// A message is cut at this many characters.
pub const MESSAGE_CHARS: usize = 240;

/// Where and how the log is written.
#[derive(Debug, Clone)]
pub struct Config {
    pub path: PathBuf,
    /// Hash object and catalog names.
    pub redact: bool,
    pub max_bytes: u64,
    /// This process rotates the file (the app does; a driver host only appends).
    pub rotate: bool,
}

impl Config {
    pub fn new(path: PathBuf, redact: bool) -> Config {
        Config {
            path,
            redact,
            max_bytes: DEFAULT_MAX_BYTES,
            rotate: true,
        }
    }
}

struct Sink {
    cfg: Config,
    file: Option<std::fs::File>,
    written: u64,
    salt: u64,
}

// 0: not decided (the environment is read once), 1: off, 2: on.
static STATE: AtomicU8 = AtomicU8::new(0);
static SINK: Mutex<Option<Sink>> = Mutex::new(None);
static COUNTER: AtomicUsize = AtomicUsize::new(1);

fn lock() -> std::sync::MutexGuard<'static, Option<Sink>> {
    SINK.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// Turns the log on (`Some`) or off (`None`).
pub fn configure(cfg: Option<Config>) {
    let mut sink = lock();
    match cfg {
        Some(cfg) => {
            if let Some(dir) = cfg.path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let written = std::fs::metadata(&cfg.path).map_or(0, |m| m.len());
            // The same hash for a name during one run, not across runs.
            let salt = (now_nanos() as u64) ^ ((std::process::id() as u64) << 32);
            *sink = Some(Sink {
                cfg,
                file: None,
                written,
                salt,
            });
            STATE.store(2, Ordering::Release);
        }
        None => {
            *sink = None;
            STATE.store(1, Ordering::Release);
        }
    }
}

/// What a driver host is started with so it writes to the same file; `None` when the log is off.
pub fn host_env() -> Option<(&'static str, String)> {
    if !enabled() {
        return None;
    }
    let sink = lock();
    let s = sink.as_ref()?;
    Some((
        ENV,
        format!("{}|{}", u8::from(s.cfg.redact), s.cfg.path.display()),
    ))
}

fn init_from_env() {
    let Ok(v) = std::env::var(ENV) else {
        STATE.store(1, Ordering::Release);
        return;
    };
    match v.split_once('|') {
        Some((r, path)) if !path.is_empty() => {
            let mut cfg = Config::new(PathBuf::from(path), r == "1");
            cfg.rotate = false;
            configure(Some(cfg));
        }
        _ => STATE.store(1, Ordering::Release),
    }
}

/// Whether the log is on. One atomic load once decided.
#[inline]
pub fn enabled() -> bool {
    match STATE.load(Ordering::Acquire) {
        2 => true,
        1 => false,
        _ => {
            init_from_env();
            STATE.load(Ordering::Acquire) == 2
        }
    }
}

/// A short id that ties the lines of one operation together.
pub fn new_op() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed) as u64;
    let mut h = fnv(&format!("{}:{}", now_nanos(), n));
    h ^= h >> 24;
    format!("{:06x}", h & 0xff_ffff)
}

fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// A stable short id for something identifying (a connection's id): the same during one run.
pub fn short_id(v: &str) -> String {
    let salt = lock().as_ref().map_or(0, |s| s.salt);
    format!("c{:07x}", fnv(&format!("{salt}:{v}")) & 0xfff_ffff)
}

/// An object or catalog name as it goes in the log: as is, or hashed when the user asked for that.
pub fn name(n: &str) -> String {
    let sink = lock();
    match sink.as_ref() {
        Some(s) if s.cfg.redact => {
            format!("h:{:08x}", fnv(&format!("{}:{n}", s.salt)) & 0xffff_ffff)
        }
        _ => n.to_string(),
    }
}

/// `text` without what must never be in a log: the host of a URL, query strings, tokens, passwords,
/// long opaque strings. Cut at [`MESSAGE_CHARS`].
pub fn scrub(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut after_bearer = false;
    for raw in text.split_whitespace() {
        let w =
            raw.trim_matches(|c: char| matches!(c, '"' | '\'' | ',' | ';' | '(' | ')' | '<' | '>'));
        let lower = w.to_ascii_lowercase();
        let secret_key = [
            "token",
            "password",
            "passwd",
            "secret",
            "key",
            "authorization",
            "sig",
        ];
        let word = if after_bearer {
            after_bearer = false;
            "<redacted>".to_string()
        } else if lower == "bearer" || lower == "basic" {
            after_bearer = true;
            raw.to_string()
        } else if let Some((k, _)) = w.split_once(['=', ':']).filter(|(k, v)| {
            !v.starts_with("//")
                && !v.is_empty()
                && secret_key
                    .iter()
                    .any(|s| k.to_ascii_lowercase().contains(s))
        }) {
            format!("{k}=<redacted>")
        } else if let Some((scheme, rest)) = w.split_once("://") {
            let path = rest.find('/').map_or("", |p| &rest[p..]);
            let path = path.split(['?', '#']).next().unwrap_or("");
            format!("{scheme}://<host>{path}")
        } else if w.starts_with("eyJ")
            || (w.len() >= 32
                && w.chars().all(|c| {
                    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '+' | '/' | '=')
                })
                && w.chars().any(|c| c.is_ascii_digit()))
        {
            "<redacted>".to_string()
        } else {
            raw.to_string()
        };
        out.push(word);
    }
    let joined = out.join(" ");
    if joined.chars().count() > MESSAGE_CHARS {
        let cut: String = joined.chars().take(MESSAGE_CHARS).collect();
        format!("{cut}...")
    } else {
        joined
    }
}

/// The first word of an error's debug form: `Connect`, `Unsupported`, `Query`…
pub fn error_kind<E: std::fmt::Debug>(e: &E) -> String {
    let d = format!("{e:?}");
    d.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("Error")
        .to_string()
}

/// `2026-11-23T10:11:12.345Z`.
pub fn iso_now() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    iso(d.as_secs(), d.subsec_millis())
}

fn iso(secs: u64, millis: u32) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil from days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn quote(v: &str) -> String {
    if !v.is_empty() && !v.chars().any(|c| c.is_whitespace() || c == '"' || c == '=') {
        return v.to_string();
    }
    let body: String = v
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .replace('"', "'");
    format!("\"{body}\"")
}

fn write_line(line: &str) {
    let mut guard = lock();
    let Some(s) = guard.as_mut() else { return };
    let len = line.len() as u64 + 1;
    if s.written + len > s.cfg.max_bytes {
        if s.cfg.rotate {
            s.file = None;
            let old = rotated(&s.cfg.path);
            let _ = std::fs::remove_file(&old);
            let _ = std::fs::rename(&s.cfg.path, &old);
            s.written = 0;
        } else {
            // A host never rotates: the app does, at its next line.
            return;
        }
    }
    if s.file.is_none() {
        s.file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&s.cfg.path)
            .ok();
    }
    if let Some(f) = s.file.as_mut() {
        if writeln!(f, "{line}").is_ok() {
            s.written += len;
        }
    }
}

/// The file a full log moves to.
pub fn rotated(path: &Path) -> PathBuf {
    let mut n = path.as_os_str().to_os_string();
    n.push(".1");
    PathBuf::from(n)
}

/// One event: `ts op=… stage=… k=v…`. Does nothing when the log is off.
pub fn event(op: &str, stage: &str, fields: &[(&str, String)]) {
    if !enabled() {
        return;
    }
    let mut line = format!("{} op={} stage={}", iso_now(), quote(op), quote(stage));
    for (k, v) in fields {
        line.push(' ');
        line.push_str(k);
        line.push('=');
        line.push_str(&quote(v));
    }
    write_line(&line);
}

/// `diag!(op, "stage", count = n, limit = 5)`: one event, built only when the log is on.
#[macro_export]
macro_rules! diag {
    ($op:expr, $stage:expr $(, $k:ident = $v:expr)* $(,)?) => {
        if $crate::diag::enabled() {
            $crate::diag::event($op, $stage, &[$((stringify!($k), ($v).to_string())),*]);
        }
    };
}

struct Inner {
    op: String,
    stage: String,
    start: Instant,
    fields: Vec<(String, String)>,
    result: Option<(String, String)>,
}

/// A stage being timed: it logs its line when dropped, with the duration and the result. A stage
/// that never said how it ended (its future was dropped by a timeout or a cancel) is `cancelled`.
pub struct DiagTimer {
    inner: Option<Inner>,
}

impl DiagTimer {
    pub fn start(op: &str, stage: &str) -> DiagTimer {
        if !enabled() {
            return DiagTimer { inner: None };
        }
        DiagTimer {
            inner: Some(Inner {
                op: op.to_string(),
                stage: stage.to_string(),
                start: Instant::now(),
                fields: Vec::new(),
                result: None,
            }),
        }
    }

    /// Adds `k=v` to the line.
    pub fn field(&mut self, k: &str, v: impl Display) -> &mut Self {
        if let Some(i) = self.inner.as_mut() {
            i.fields.push((k.to_string(), v.to_string()));
        }
        self
    }

    /// Adds `k=<name>`, hashed when names are redacted.
    pub fn object(&mut self, k: &str, v: &str) -> &mut Self {
        if self.inner.is_some() {
            let n = name(v);
            self.field(k, n);
        }
        self
    }

    pub fn ok(&mut self) {
        if let Some(i) = self.inner.as_mut() {
            i.result = Some(("ok".into(), String::new()));
        }
    }

    /// `res=<kind>` with a scrubbed, trimmed message (`timeout`, `error`…).
    pub fn fail(&mut self, kind: &str, message: &str) {
        if let Some(i) = self.inner.as_mut() {
            i.result = Some((kind.to_string(), scrub(message)));
        }
    }

    /// `ok`, or `error` with the error's kind and message.
    pub fn finish<T, E: std::fmt::Debug + Display>(&mut self, r: &Result<T, E>) {
        match r {
            Ok(_) => self.ok(),
            Err(e) => {
                let kind = error_kind(e);
                self.field("error_kind", kind);
                self.fail("error", &e.to_string());
            }
        }
    }

    /// Milliseconds since the stage started (0 when the log is off).
    pub fn elapsed_ms(&self) -> u128 {
        self.inner
            .as_ref()
            .map_or(0, |i| i.start.elapsed().as_millis())
    }
}

impl Drop for DiagTimer {
    fn drop(&mut self) {
        let Some(i) = self.inner.take() else { return };
        let mut fields: Vec<(&str, String)> =
            vec![("ms", i.start.elapsed().as_millis().to_string())];
        for (k, v) in &i.fields {
            fields.push((k.as_str(), v.clone()));
        }
        let (res, msg) = i.result.unwrap_or(("cancelled".into(), String::new()));
        fields.push(("res", res));
        if !msg.is_empty() {
            fields.push(("msg", msg));
        }
        event(&i.op, &i.stage, &fields);
    }
}

/// Serializes the tests that use the process-wide sink.
#[cfg(test)]
pub static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dbine-diag-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn disabled_writes_no_file() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        configure(None);
        let dir = temp("off");
        diag!("op1", "stage", n = 3);
        let mut t = DiagTimer::start("op1", "stage");
        t.field("a", 1).ok();
        drop(t);
        assert!(!dir.exists());
        assert!(!enabled());
        assert!(host_env().is_none());
    }

    #[test]
    fn a_line_has_the_op_stage_duration_and_result() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp("on");
        let path = dir.join(FILE_NAME);
        configure(Some(Config::new(path.clone(), false)));
        {
            let mut t = DiagTimer::start("abc123", "graph_scan.list");
            t.field("objects", 42).object("catalog", "dev_gold").ok();
        }
        {
            let _t = DiagTimer::start("abc123", "lineage_sql");
        }
        diag!("abc123", "note", rows = 7);
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("op=abc123 stage=graph_scan.list ms="));
        assert!(lines[0].contains("objects=42 catalog=dev_gold res=ok"));
        assert!(lines[1].contains("res=cancelled"), "{}", lines[1]);
        assert!(lines[2].ends_with("rows=7"));
        assert!(lines[0].starts_with("20") && lines[0].contains('T') && lines[0].contains("Z "));
        configure(None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn names_are_hashed_when_asked_and_stable_within_a_run() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp("redact");
        configure(Some(Config::new(dir.join(FILE_NAME), true)));
        let a = name("directorio.dnis");
        assert!(a.starts_with("h:") && !a.contains("dnis"));
        assert_eq!(a, name("directorio.dnis"));
        assert_ne!(a, name("otra"));
        let mut t = DiagTimer::start("o", "s");
        t.object("table", "directorio.dnis").ok();
        drop(t);
        let text = std::fs::read_to_string(dir.join(FILE_NAME)).unwrap();
        assert!(!text.contains("dnis"));
        configure(None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_file_rotates_and_keeps_one_old() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp("rotate");
        let path = dir.join(FILE_NAME);
        let mut cfg = Config::new(path.clone(), false);
        cfg.max_bytes = 600;
        configure(Some(cfg));
        for i in 0..40 {
            diag!("op", "fill", i = i);
        }
        let old = rotated(&path);
        assert!(old.exists());
        assert!(std::fs::metadata(&path).unwrap().len() <= 600);
        assert!(std::fs::metadata(&old).unwrap().len() <= 600);
        assert!(!rotated(&old).exists());
        configure(None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn messages_carry_no_hosts_tokens_or_passwords() {
        // Built at run time: a literal like a real token trips the secret scanners.
        let pat = format!("{}{}", "da", "pi0123456789abcdef0123456789abcdef");
        let m = scrub(&format!(
            "error sending request for url (https://adb-123.azuredatabricks.net/api/2.0/x?token=abc) \
             Authorization: Bearer {pat} password=hunter2 \
             eyJhbGciOiJIUzI1NiJ9.payload.sig code 403"
        ));
        for bad in ["azuredatabricks", "abc)", "dapi0123", "hunter2", "eyJhbG"] {
            assert!(!m.contains(bad), "{bad} in {m}");
        }
        assert!(m.contains("https://<host>/api/2.0/x"));
        assert!(m.contains("403"));
        let long = scrub(&"palabra ".repeat(200));
        assert!(long.chars().count() <= MESSAGE_CHARS + 3);
    }

    #[test]
    fn error_kinds_and_timestamps() {
        #[derive(Debug)]
        #[allow(dead_code)]
        enum E {
            Query(String),
        }
        assert_eq!(error_kind(&E::Query("x".into())), "Query");
        assert_eq!(iso(0, 5), "1970-01-01T00:00:00.005Z");
        assert_eq!(iso(1_700_000_000, 0), "2023-11-14T22:13:20.000Z");
    }

    #[test]
    fn a_host_reads_the_same_file_from_the_environment() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = temp("env");
        configure(Some(Config::new(dir.join(FILE_NAME), true)));
        let (k, v) = host_env().unwrap();
        assert_eq!(k, ENV);
        assert!(v.starts_with("1|") && v.ends_with(FILE_NAME));
        configure(None);
    }

    #[test]
    fn new_ops_differ_and_ids_are_short() {
        let a = new_op();
        let b = new_op();
        assert_ne!(a, b);
        assert_eq!(a.len(), 6);
        assert!(short_id("conn-1").len() <= 9);
    }
}
