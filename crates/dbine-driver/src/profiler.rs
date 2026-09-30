//! The profiler: every statement run against a database, by any client,
//! while it's on (SQL Server Profiler style). A session started with
//! [`Session::profiler_start`](crate::Session::profiler_start) is then
//! polled with `profiler_poll`, which returns what arrived since the last
//! poll, and closed with `profiler_stop`.
//!
//! Engines differ in what they can see:
//! - **Complete:** the engine records every statement (an Extended Events
//!   session, MongoDB's profiler, MySQL's statement history, a query log).
//!   Some need a server setting switched on while profiling: the driver does
//!   it at start when allowed and puts it back at stop.
//! - **Sampled:** the engine only shows what is running now (or each
//!   connection's last statement). The driver looks several times within
//!   each poll ([`SAMPLE_EVERY`], for about [`SAMPLE_FOR`]) and a
//!   [`Sampler`] turns the looks into statements; one shorter than the gap
//!   between two looks can go unseen.
//!
//! The profiler's own statements are left out.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How often a sampling driver looks at the server within a poll.
pub const SAMPLE_EVERY: Duration = Duration::from_millis(100);
/// How long one poll of a sampling driver keeps looking before it returns
/// (the UI polls again right away).
pub const SAMPLE_FOR: Duration = Duration::from_millis(900);

/// How the profiler starts.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfilerOptions {
    /// The database to watch (empty: the whole server, where the engine
    /// has no databases).
    pub database: String,
    /// It may switch server settings on to see more (restored at stop).
    /// False on read-only connections: then only what's already there.
    pub change_server: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfilerMode {
    /// Every statement.
    Complete,
    /// What's running at each poll.
    Sampled,
}

/// What the profiler did when it started, for the UI's header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProfilerStarted {
    pub mode: ProfilerMode,
    /// Where the statements come from ("Extended Events", "pg_stat_activity"…).
    pub source: String,
    /// Server settings switched on to profile; put back at stop.
    pub changes: Vec<String>,
    /// Anything the user should know (a limit, what's missing and why).
    pub note: Option<String>,
    /// What [`ProfiledStatement::reads`] / `writes` count on this engine,
    /// in Spanish, plural ("páginas", "bloques", "filas", "bytes",
    /// "documentos"); `None` when the engine doesn't report them.
    #[serde(default)]
    pub reads_unit: Option<String>,
    #[serde(default)]
    pub writes_unit: Option<String>,
}

impl ProfilerStarted {
    pub fn new(mode: ProfilerMode, source: impl Into<String>) -> Self {
        Self { mode, source: source.into(), changes: Vec::new(), note: None, reads_unit: None, writes_unit: None }
    }
    /// What the engine's reads and writes count (see [`ProfilerStarted::reads_unit`]).
    pub fn units(mut self, reads: Option<&str>, writes: Option<&str>) -> Self {
        self.reads_unit = reads.map(str::to_string);
        self.writes_unit = writes.map(str::to_string);
        self
    }
    pub fn change(mut self, what: impl Into<String>) -> Self {
        self.changes.push(what.into());
        self
    }
    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}

/// One statement the profiler saw.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfiledStatement {
    /// When it started (when it ended, where the engine only knows that),
    /// `YYYY-MM-DD HH:MM:SS.mmm` in UTC.
    pub time: String,
    pub duration_ms: Option<f64>,
    /// The statement as the client sent it.
    pub text: String,
    pub database: Option<String>,
    pub user: Option<String>,
    /// Host or address of the client.
    pub client: Option<String>,
    /// The client's application name, where the engine reports one
    /// (SQL Server's program name, PostgreSQL's application_name, MongoDB's
    /// appName…).
    pub application: Option<String>,
    /// Rows returned or affected.
    pub rows: Option<u64>,
    /// The error, when it failed.
    pub error: Option<String>,
    /// Engine-specific figures (operation, memory, plan…), short. CPU,
    /// reads and writes go in their own fields.
    pub detail: Option<String>,
    /// CPU time the statement used, in milliseconds.
    #[serde(default)]
    pub cpu_ms: Option<f64>,
    /// What it read, in the engine's unit (`ProfilerStarted::reads_unit`):
    /// logical reads / pages in SQL Server, buffer gets in Oracle, rows
    /// read in ClickHouse, documents examined in MongoDB…
    #[serde(default)]
    pub reads: Option<u64>,
    /// What it wrote, in `ProfilerStarted::writes_unit`.
    #[serde(default)]
    pub writes: Option<u64>,
}

/// What a sampling engine shows for one connection at one look: the
/// statement it runs now, or the last one it ran.
#[derive(Debug, Clone, Default)]
pub struct Sample {
    /// The connection on the server (pid, thread, session id).
    pub session: String,
    /// When the statement started, in [`ProfiledStatement::time`]'s format;
    /// with `session`, it tells one statement from the next.
    pub started: String,
    pub text: String,
    /// Still running (else it finished).
    pub running: bool,
    /// Known once it finished (or how long it has been running).
    pub duration_ms: Option<f64>,
    pub database: Option<String>,
    pub user: Option<String>,
    pub client: Option<String>,
    pub application: Option<String>,
    pub rows: Option<u64>,
    pub error: Option<String>,
    pub detail: Option<String>,
    pub cpu_ms: Option<f64>,
    pub reads: Option<u64>,
    pub writes: Option<u64>,
}

impl Sample {
    /// A later look at the same statement keeps the figures an earlier
    /// look had when this one has none: a finished statement often only
    /// shows its session's totals, which aren't its own.
    fn or_figures_of(mut self, earlier: &Sample) -> Sample {
        self.cpu_ms = self.cpu_ms.or(earlier.cpu_ms);
        self.reads = self.reads.or(earlier.reads);
        self.writes = self.writes.or(earlier.writes);
        self
    }

    fn statement(self) -> ProfiledStatement {
        ProfiledStatement {
            time: self.started,
            duration_ms: self.duration_ms,
            text: self.text,
            database: self.database,
            user: self.user,
            client: self.client,
            application: self.application,
            rows: self.rows,
            error: self.error,
            detail: self.detail,
            cpu_ms: self.cpu_ms,
            reads: self.reads,
            writes: self.writes,
        }
    }
}

/// Turns repeated looks at what each connection runs into the statements
/// that ran: each is reported once, when it's seen finished, or when its
/// connection moves on to another statement or goes away (then with the
/// last duration seen).
#[derive(Debug, Default)]
pub struct Sampler {
    /// Statements that finished before this (same format as `started`) were
    /// already there when profiling began: not reported.
    since: String,
    seen: std::collections::HashMap<String, Tracked>,
}

#[derive(Debug)]
struct Tracked {
    started: String,
    /// Seen running and not reported yet.
    pending: Option<Sample>,
}

impl Sampler {
    /// `since`: the server's time when profiling began.
    pub fn new(since: impl Into<String>) -> Self {
        Self { since: since.into(), seen: Default::default() }
    }

    /// One look at the server; returns the statements that finished since
    /// the previous one, oldest first.
    pub fn feed(&mut self, samples: Vec<Sample>) -> Vec<ProfiledStatement> {
        let mut out = Vec::new();
        let mut present = std::collections::HashSet::new();
        for s in samples {
            present.insert(s.session.clone());
            if let Some(t) = self.seen.get_mut(&s.session) {
                if t.started == s.started {
                    if let Some(p) = t.pending.take() {
                        let s = s.or_figures_of(&p);
                        if s.running {
                            t.pending = Some(s);
                        } else {
                            out.push(s.statement());
                        }
                    }
                    continue;
                }
                // The connection moved on: what it ran before is over.
                if let Some(p) = t.pending.take() {
                    out.push(p.statement());
                }
            }
            let before = !s.running && s.started < self.since;
            let started = s.started.clone();
            let session = s.session.clone();
            let pending = if s.running {
                Some(s)
            } else {
                if !before && !s.text.trim().is_empty() {
                    out.push(s.statement());
                }
                None
            };
            self.seen.insert(session, Tracked { started, pending });
        }
        let gone: Vec<String> = self.seen.keys().filter(|k| !present.contains(*k)).cloned().collect();
        for k in gone {
            if let Some(p) = self.seen.remove(&k).and_then(|t| t.pending) {
                out.push(p.statement());
            }
        }
        out.retain(|s| !s.text.trim().is_empty());
        out.sort_by(|a, b| a.time.cmp(&b.time));
        out
    }

    /// Statements still running when profiling stops (with the time seen so
    /// far).
    pub fn finish(&mut self) -> Vec<ProfiledStatement> {
        let mut out: Vec<ProfiledStatement> =
            self.seen.drain().filter_map(|(_, t)| t.pending).map(Sample::statement).collect();
        out.sort_by(|a, b| a.time.cmp(&b.time));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_finished_look_keeps_the_running_figures() {
        let mut sp = Sampler::new("2026-01-01 00:00:00.000");
        let running = Sample {
            session: "1".into(), started: "2026-01-01 00:00:01.000".into(), text: "select 1".into(), running: true,
            cpu_ms: Some(40.0), reads: Some(900), writes: Some(3), ..Default::default()
        };
        assert!(sp.feed(vec![running]).is_empty());
        let done = Sample {
            session: "1".into(), started: "2026-01-01 00:00:01.000".into(), text: "select 1".into(), running: false,
            duration_ms: Some(80.0), reads: Some(950), ..Default::default()
        };
        let out = sp.feed(vec![done]);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].cpu_ms, out[0].reads, out[0].writes), (Some(40.0), Some(950), Some(3)));
    }

    fn s(session: &str, started: &str, text: &str, running: bool, ms: f64) -> Sample {
        Sample {
            session: session.into(),
            started: started.into(),
            text: text.into(),
            running,
            duration_ms: Some(ms),
            ..Default::default()
        }
    }

    #[test]
    fn sampler_reports_each_statement_once() {
        let mut p = Sampler::new("2024-01-01 10:00:00.000");
        // Already finished before profiling began: not reported.
        assert!(p.feed(vec![s("1", "2024-01-01 09:59:00.000", "old", false, 5.0)]).is_empty());
        // A statement seen running, then finished.
        assert!(p.feed(vec![s("1", "2024-01-01 10:00:01.000", "slow", true, 100.0)]).is_empty());
        let out = p.feed(vec![s("1", "2024-01-01 10:00:01.000", "slow", false, 900.0)]);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].text.as_str(), out[0].duration_ms), ("slow", Some(900.0)));
        // Seen again, finished: not reported twice.
        assert!(p.feed(vec![s("1", "2024-01-01 10:00:01.000", "slow", false, 900.0)]).is_empty());
        // Finished between two looks: reported at once.
        let out = p.feed(vec![s("1", "2024-01-01 10:00:02.000", "fast", false, 3.0)]);
        assert_eq!(out[0].text, "fast");
        // Running, then the connection runs something else: the first is over.
        p.feed(vec![s("1", "2024-01-01 10:00:03.000", "a", true, 10.0)]);
        let out = p.feed(vec![s("1", "2024-01-01 10:00:04.000", "b", true, 1.0)]);
        assert_eq!(out[0].text, "a");
        // The connection goes away while running: reported.
        let out = p.feed(vec![]);
        assert_eq!(out[0].text, "b");
        // Running at stop.
        p.feed(vec![s("2", "2024-01-01 10:00:05.000", "c", true, 1.0)]);
        assert_eq!(p.finish()[0].text, "c");
    }
}
