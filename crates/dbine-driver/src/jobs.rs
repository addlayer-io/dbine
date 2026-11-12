//! Server jobs: the work a database server runs on its own, on a schedule
//! or on demand (SQL Server Agent, Oracle's `DBMS_SCHEDULER`, Snowflake
//! tasks, Databricks jobs, pg_cron, MySQL events…). They show in
//! "Programadas", under "En los servidores", next to the tasks DBine runs.
//!
//! Engines differ a lot in what a job is. The contract keeps what all of
//! them have (a name, whether it's enabled, its schedule, its runs) and
//! leaves the rest to [`JobDetail::properties`] and
//! [`crate::Session::job_script`]. What a job allows is per job, not per
//! driver: one Postgres server can have pg_cron jobs (no "run now") next to
//! TimescaleDB jobs (which have it).
//!
//! Job ids travel to the app and come back from it in the other calls.
//! Drivers treat an id they get as untrusted text: they bind it as a
//! parameter or quote it as the engine requires, never paste it in a
//! statement, and refuse one that doesn't have their shape.

use serde::{Deserialize, Serialize};

/// How a run ended, or that it's still going.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobOutcome {
    Succeeded,
    Failed,
    /// Stopped by someone, or by a timeout.
    Canceled,
    Running,
    /// Waiting to retry after a failure.
    Retrying,
    /// The engine didn't run it (the previous run still going, a
    /// condition not met).
    Skipped,
    #[default]
    Unknown,
}

/// A server's jobs, for the list ([`crate::Session::jobs`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobList {
    pub jobs: Vec<ServerJob>,
    /// What the user should know about the list, in Spanish (the UI
    /// translates it): why it's empty ("este servidor no tiene pg_cron,
    /// pgAgent ni TimescaleDB"), or partial ("se ven solo los jobs a los que
    /// tiene acceso este usuario"). Empty when there's nothing to say.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// A job as the list shows it. Engines fill what they report; the rest
/// stays `None`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ServerJob {
    /// The engine's id for it, as the other calls take it (a GUID,
    /// `OWNER.NAME`, a task's qualified name, a Databricks job id,
    /// `pg_cron:12`…). Opaque to the app.
    pub id: String,
    pub name: String,
    /// The engine's word for it, shown as a tag: "SQL Server Agent",
    /// "DBMS_SCHEDULER", "Task", "Event", "pg_cron"…
    pub kind: String,
    /// Where it lives when the engine groups them: its schema or owner, its
    /// database, a Snowflake `DB.SCHEMA`, an Agent category.
    pub group: Option<String>,
    pub description: Option<String>,
    pub owner: Option<String>,
    /// Its schedule fires. A disabled job may still run on demand, where
    /// the engine allows it.
    pub enabled: bool,
    /// The schedule as the engine states it ("0 3 * * *",
    /// "FREQ=DAILY;BYHOUR=3", "USING CRON 0 3 * * * UTC", "EVERY 1 HOUR"…).
    /// `None`: it only runs on demand or when another job starts it.
    pub schedule: Option<String>,
    /// A run is going on now.
    #[serde(default)]
    pub running: bool,
    /// How the last finished run ended (or `Running` while one is going and
    /// none finished before).
    pub last_outcome: Option<JobOutcome>,
    /// When the last run started, RFC 3339 in UTC.
    pub last_started: Option<String>,
    /// How long the last finished run took, ms.
    pub last_duration_ms: Option<u64>,
    /// The next scheduled start, RFC 3339 in UTC, where the engine says it.
    pub next_run: Option<String>,
    /// "Ejecutar ahora" works on it ([`crate::Session::run_job`]).
    #[serde(default)]
    pub can_run: bool,
    /// Its current run can be stopped ([`crate::Session::stop_job`]).
    #[serde(default)]
    pub can_stop: bool,
    /// It can be enabled and disabled ([`crate::Session::set_job_enabled`]).
    #[serde(default)]
    pub can_enable: bool,
    /// Its past runs can be read ([`crate::Session::job_runs`]).
    #[serde(default)]
    pub has_history: bool,
    /// Its script can be generated ([`crate::Session::job_script`]).
    #[serde(default)]
    pub has_script: bool,
}

/// One thing a job runs: an Agent step, a Databricks task, a chain step, or
/// the one statement of a pg_cron job, a task or an event.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobStep {
    pub name: String,
    /// What runs it: "T-SQL", "PowerShell", "Notebook", "SQL", "PL/SQL"…
    pub kind: Option<String>,
    /// The database, warehouse or cluster it runs on, when it isn't the
    /// job's own.
    pub target: Option<String>,
    /// What it runs: the statement, the command, the notebook's path…
    pub command: Option<String>,
}

/// Everything about one job ([`crate::Session::job_detail`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobDetail {
    pub job: ServerJob,
    /// What it runs, in order.
    #[serde(default)]
    pub steps: Vec<JobStep>,
    /// Its schedules as the engine states them, when there's more than the
    /// one in `job.schedule` (an Agent job can have several).
    #[serde(default)]
    pub schedules: Vec<String>,
    /// The rest the engine says about it, in its terms ("Categoría",
    /// "Warehouse", "Máximo de ejecuciones a la vez"…), grouped in sections
    /// by `group` (empty: General). Labels are in Spanish; the UI translates
    /// them.
    #[serde(default)]
    pub properties: Vec<crate::PropertyInfo>,
}

/// How one step of a run went.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobStepRun {
    pub name: String,
    pub outcome: JobOutcome,
    /// RFC 3339 in UTC.
    pub started: Option<String>,
    pub duration_ms: Option<u64>,
    /// The error or output the engine recorded for the step.
    pub message: Option<String>,
}

/// One run of a job ([`crate::Session::job_runs`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct JobRun {
    /// The engine's id for the run, when it has one.
    pub id: Option<String>,
    /// RFC 3339 in UTC.
    pub started: Option<String>,
    pub duration_ms: Option<u64>,
    pub outcome: JobOutcome,
    /// What started it, in Spanish, where the engine says it
    /// ("programada", "manual", "otro job"…).
    pub trigger: Option<String>,
    /// The error, output or message the engine recorded for the run.
    pub message: Option<String>,
    /// Its steps, where the engine records them per step.
    #[serde(default)]
    pub steps: Vec<JobStepRun>,
}

/// Most runs [`crate::Session::job_runs`] returns, whatever is asked.
pub const MAX_JOB_RUNS: u32 = 500;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_job_list_survives_json() {
        let list = JobList {
            jobs: vec![ServerJob {
                id: "pg_cron:12".into(),
                name: "vacuum nightly".into(),
                kind: "pg_cron".into(),
                enabled: true,
                schedule: Some("0 3 * * *".into()),
                last_outcome: Some(JobOutcome::Failed),
                last_started: Some("2026-11-12T03:00:00Z".into()),
                last_duration_ms: Some(1234),
                can_enable: true,
                has_history: true,
                ..Default::default()
            }],
            notes: vec!["pg_cron está en la base 'postgres'".into()],
        };
        let json = serde_json::to_value(&list).unwrap();
        assert_eq!(json["jobs"][0]["last_outcome"], "failed");
        let back: JobList = serde_json::from_value(json).unwrap();
        assert_eq!(back, list);
    }

    #[test]
    fn missing_flags_and_notes_read_as_none() {
        let job: ServerJob = serde_json::from_value(serde_json::json!({
            "id": "1", "name": "j", "kind": "Event", "group": null,
            "description": null, "owner": null, "enabled": false,
            "schedule": null, "last_outcome": null, "last_started": null,
            "last_duration_ms": null, "next_run": null
        }))
        .unwrap();
        assert!(!job.can_run && !job.can_stop && !job.can_enable);
        assert!(!job.has_history && !job.has_script && !job.running);
        let list: JobList = serde_json::from_value(serde_json::json!({"jobs": []})).unwrap();
        assert!(list.notes.is_empty());
        assert!(serde_json::to_value(&list).unwrap().get("notes").is_none());
    }
}
