//! Scheduled tasks (docs/tareas-programadas.md): the model the app edits,
//! the OS scheduler runs (`dbine --run-task <id>`) and the run history
//! records. Kept in this machine's state only: they point at this
//! machine's connections and folders, so they don't sync.
//!
//! A task is a list of steps run in order. Each step has a `kind` and its
//! own `config` (JSON), so new kinds (mail, conditions, loops…) come
//! without touching this model or the tables. Steps see variables
//! (`{date}`, `{task}`…) and what earlier steps produced, under
//! `{steps.<n>.<key>}`.

use chrono::{Datelike, Local, NaiveDateTime, NaiveTime, Timelike};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// The step kinds this version runs.
pub mod kinds {
    pub const RUN_SCRIPT: &str = "run_script";
    pub const EXPORT: &str = "export";
    pub const COMPARE_SCHEMAS: &str = "compare_schemas";
    pub const BACKUP: &str = "backup";
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScheduledTask {
    pub id: String,
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub schedule: Schedule,
    pub steps: Vec<Step>,
    #[serde(default)]
    pub notify: Notify,
    /// What the user approved, when saving, of the steps that change data
    /// or structure: a fingerprint of those steps (connection, database,
    /// script). Editing them changes it, so they don't run until approved
    /// again: an unattended change is always a decision.
    #[serde(default)]
    pub approved_writes: Option<String>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

fn yes() -> bool {
    true
}

/// When it runs, in local time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Schedule {
    /// Every day at `time` ("HH:MM").
    Daily { time: String },
    /// On `days` (1 = Monday … 7 = Sunday) at `time`.
    Weekly { days: Vec<u8>, time: String },
    /// On day `day` (1–28) of each month at `time`.
    Monthly { day: u8, time: String },
    /// Every `minutes` minutes (5 at least).
    Interval { minutes: u32 },
}

impl Default for Schedule {
    fn default() -> Self {
        Schedule::Daily { time: "08:00".into() }
    }
}

/// When the OS notification goes out ("Failure": a failure or an alert).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Notify {
    Never,
    #[default]
    Failure,
    Always,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnError {
    /// The task stops (and is marked failed).
    #[default]
    Stop,
    /// The next steps run anyway (the task ends as "con errores").
    Continue,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    /// One of [`kinds`] (or a later one).
    pub kind: String,
    /// What the user called it ("" : the kind's own label).
    #[serde(default)]
    pub name: String,
    /// The kind's settings.
    #[serde(default)]
    pub config: Value,
    #[serde(default)]
    pub on_error: OnError,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    #[default]
    Running,
    Ok,
    /// Some step failed and others went on (`OnError::Continue`).
    Partial,
    Failed,
}

/// What a step did.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StepRun {
    pub step_id: String,
    pub kind: String,
    pub status: RunStatus,
    /// One line for the history ("1.204 filas exportadas a …").
    pub summary: String,
    #[serde(default)]
    pub messages: Vec<String>,
    /// What later steps can read as `{steps.<n>.<key>}`: `file`, `rows`,
    /// `differences`…
    #[serde(default)]
    pub outputs: BTreeMap<String, String>,
    pub started_at: String,
    pub finished_at: String,
    /// Worth telling even though it worked ("las bases difieren en 3
    /// objetos"): it notifies as a failure does.
    #[serde(default)]
    pub alert: Option<String>,
}

/// One run of a task.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskRun {
    pub id: String,
    pub task_id: String,
    /// "schedule" (the OS ran it) or "manual" ("Ejecutar ahora").
    pub trigger: String,
    pub status: RunStatus,
    pub started_at: String,
    #[serde(default)]
    pub finished_at: String,
    #[serde(default)]
    pub steps: Vec<StepRun>,
    /// Why it couldn't start (no such task, no password…).
    #[serde(default)]
    pub error: Option<String>,
}

fn time_of(t: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(t.trim(), "%H:%M").ok()
}

impl Schedule {
    /// What's wrong with it, in Spanish (`None`: fine).
    pub fn problem(&self) -> Option<String> {
        let bad_time = |t: &str| time_of(t).is_none().then(|| format!("«{t}» no es una hora HH:MM"));
        match self {
            Schedule::Daily { time } => bad_time(time),
            Schedule::Weekly { days, time } => {
                if days.is_empty() || days.iter().any(|d| !(1..=7).contains(d)) {
                    Some("elegí al menos un día de la semana".into())
                } else {
                    bad_time(time)
                }
            }
            Schedule::Monthly { day, time } => {
                if !(1..=28).contains(day) {
                    Some("el día del mes tiene que estar entre 1 y 28".into())
                } else {
                    bad_time(time)
                }
            }
            Schedule::Interval { minutes } => (*minutes < 5).then(|| "el intervalo mínimo es de 5 minutos".into()),
        }
    }

    /// The next run after `now` (for the list; the OS scheduler is the one
    /// that runs it).
    pub fn next_after(&self, now: NaiveDateTime) -> Option<NaiveDateTime> {
        match self {
            Schedule::Interval { minutes } => Some(now + chrono::Duration::minutes(*minutes as i64)),
            Schedule::Daily { time } | Schedule::Weekly { time, .. } | Schedule::Monthly { time, .. } => {
                let t = time_of(time)?;
                (0..400).map(|d| (now.date() + chrono::Duration::days(d)).and_time(t)).find(|at| {
                    *at > now
                        && match self {
                            Schedule::Weekly { days, .. } => days.contains(&(at.weekday().number_from_monday() as u8)),
                            Schedule::Monthly { day, .. } => at.day() == *day as u32,
                            _ => true,
                        }
                })
            }
        }
    }
}

impl ScheduledTask {
    /// What's wrong with it, in Spanish (`None`: it can be saved).
    pub fn problem(&self) -> Option<String> {
        if self.name.trim().is_empty() {
            return Some("la tarea necesita un nombre".into());
        }
        if self.steps.is_empty() {
            return Some("la tarea necesita al menos un paso".into());
        }
        self.schedule.problem()
    }
}

/// `{name}` replaced by `vars[name]` (unknown names stay as they are).
pub fn expand(text: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        match rest[open + 1..].find('}') {
            Some(close) => {
                let name = &rest[open + 1..open + 1 + close];
                match vars.get(name) {
                    Some(v) => out.push_str(v),
                    None => out.push_str(&rest[open..open + close + 2]),
                }
                rest = &rest[open + close + 2..];
            }
            None => {
                out.push_str(&rest[open..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// The variables every step sees: `{task}`, `{date}` (YYYY-MM-DD), `{time}`
/// (HHMMSS), `{datetime}` (YYYY-MM-DD_HHMMSS), `{year}`, `{month}`, `{day}`.
pub fn base_vars(task: &ScheduledTask, at: NaiveDateTime) -> BTreeMap<String, String> {
    let mut v = BTreeMap::new();
    v.insert("task".into(), task.name.clone());
    v.insert("date".into(), at.format("%Y-%m-%d").to_string());
    v.insert("time".into(), format!("{:02}{:02}{:02}", at.hour(), at.minute(), at.second()));
    v.insert("datetime".into(), at.format("%Y-%m-%d_%H%M%S").to_string());
    v.insert("year".into(), at.format("%Y").to_string());
    v.insert("month".into(), at.format("%m").to_string());
    v.insert("day".into(), at.format("%d").to_string());
    v
}

/// Now, local, for timestamps in the history.
pub fn now_text() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn at(y: i32, m: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, mi, 0).unwrap()
    }

    #[test]
    fn next_runs() {
        // 2026-10-08 is a Thursday.
        let now = at(2026, 10, 8, 9, 0);
        assert_eq!(Schedule::Daily { time: "08:30".into() }.next_after(now), Some(at(2026, 10, 9, 8, 30)));
        assert_eq!(Schedule::Daily { time: "10:00".into() }.next_after(now), Some(at(2026, 10, 8, 10, 0)));
        assert_eq!(Schedule::Weekly { days: vec![1], time: "07:00".into() }.next_after(now), Some(at(2026, 10, 12, 7, 0)));
        assert_eq!(Schedule::Monthly { day: 1, time: "00:00".into() }.next_after(now), Some(at(2026, 11, 1, 0, 0)));
        assert_eq!(Schedule::Interval { minutes: 30 }.next_after(now), Some(at(2026, 10, 8, 9, 30)));
    }

    #[test]
    fn validation() {
        assert!(Schedule::Daily { time: "25:00".into() }.problem().is_some());
        assert!(Schedule::Weekly { days: vec![], time: "08:00".into() }.problem().is_some());
        assert!(Schedule::Monthly { day: 31, time: "08:00".into() }.problem().is_some());
        assert!(Schedule::Interval { minutes: 1 }.problem().is_some());
        let t = ScheduledTask { name: "x".into(), steps: vec![Step::default()], ..Default::default() };
        assert!(t.problem().is_none());
    }

    #[test]
    fn variables() {
        let t = ScheduledTask { name: "ventas".into(), ..Default::default() };
        let mut v = base_vars(&t, at(2026, 1, 2, 3, 4));
        v.insert("steps.1.rows".into(), "42".into());
        assert_eq!(expand("{task}-{date}_{time}.csv", &v), "ventas-2026-01-02_030400.csv");
        assert_eq!(expand("{steps.1.rows} filas, {unknown} {", &v), "42 filas, {unknown} {");
    }

    #[test]
    fn model_round_trips() {
        let t = ScheduledTask {
            id: "a".into(),
            name: "Reporte".into(),
            enabled: true,
            schedule: Schedule::Weekly { days: vec![1, 5], time: "08:00".into() },
            steps: vec![Step { id: "s1".into(), kind: kinds::EXPORT.into(), config: serde_json::json!({"sql": "SELECT 1"}), ..Default::default() }],
            notify: Notify::Always,
            ..Default::default()
        };
        let j = serde_json::to_string(&t).unwrap();
        assert!(j.contains(r#""type":"weekly""#));
        assert_eq!(serde_json::from_str::<ScheduledTask>(&j).unwrap(), t);
    }
}
