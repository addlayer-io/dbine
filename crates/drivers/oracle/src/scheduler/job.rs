//! Jobs: `DBMS_SCHEDULER.CREATE_JOB` and every attribute `SET_ATTRIBUTE`
//! takes that the dictionary shows (so the designer can show what the job
//! has), its arguments, its schedule in any mode, its e-mail
//! notifications; `DROP_JOB`. Before 19c, `DBMS_JOB` jobs too
//! (`SUBMIT`, `WHAT`, `NEXT_DATE`, `INTERVAL`, `INSTANCE`, `BROKEN`,
//! `REMOVE`). Start at a chain's step (`RUN_CHAIN`) and clear the history
//! (`PURGE_LOG`).

use super::choices::{self, Catalog};
use super::read::{self, Rec};
use super::script::name_lit;
use super::timing::{self, local_time, When};
use super::{
    bad, fingerprint, fresh, new_name, new_owner, obj_id, only, opt_ref, parse_obj, pick_ref, rows,
    secs, shown, Block, Ctx, Kind, Plan, Vals, A, OWNER_ONLY,
};
use crate::jobs::{self, JobId};
use crate::processes::within;
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result,
    SchedulerItem, SchedulerList,
};
use oracledb::Connection;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

const TYPES: [&str; 8] = [
    "PLSQL_BLOCK",
    "STORED_PROCEDURE",
    "EXECUTABLE",
    "EXTERNAL_SCRIPT",
    "SQL_SCRIPT",
    "BACKUP_SCRIPT",
    "CHAIN",
    "PROGRAM",
];
/// Run outside the database: need CREATE EXTERNAL JOB, not in Autonomous.
pub(crate) const EXTERNAL: [&str; 4] = [
    "EXECUTABLE",
    "EXTERNAL_SCRIPT",
    "SQL_SCRIPT",
    "BACKUP_SCRIPT",
];
/// The inline types that take arguments.
pub(crate) const WITH_ARGS: [&str; 3] = ["STORED_PROCEDURE", "EXECUTABLE", "EXTERNAL_SCRIPT"];
/// The job types that take arguments: those, and a program's.
const ARG_TYPES: [&str; 4] = [
    "STORED_PROCEDURE",
    "EXECUTABLE",
    "EXTERNAL_SCRIPT",
    "PROGRAM",
];
/// The job types whose script can fail (`fail_on_script_error`), a
/// program's maybe.
const SCRIPT_TYPES: [&str; 4] = ["EXTERNAL_SCRIPT", "SQL_SCRIPT", "BACKUP_SCRIPT", "PROGRAM"];
const STYLES: [&str; 4] = [
    "REGULAR",
    "LIGHTWEIGHT",
    "IN_MEMORY_RUNTIME",
    "IN_MEMORY_FULL",
];
const LOGGING: [&str; 4] = ["OFF", "RUNS", "FAILED_RUNS", "FULL"];
/// The job events a job can raise or notify of (each one bit).
pub(crate) const EVENTS: [&str; 10] = [
    "JOB_STARTED",
    "JOB_SUCCEEDED",
    "JOB_FAILED",
    "JOB_BROKEN",
    "JOB_COMPLETED",
    "JOB_STOPPED",
    "JOB_SCH_LIM_REACHED",
    "JOB_DISABLED",
    "JOB_CHAIN_STALLED",
    "JOB_OVER_MAX_DUR",
];
const DEFAULT_CLASS: &str = "SYS.DEFAULT_JOB_CLASS";
const MODES: [&str; 9] = [
    "immediate",
    "once",
    "calendar",
    "expression",
    "named",
    "window",
    "window_group",
    "event",
    "file_watcher",
];
const SOURCES: [&str; 10] = [
    "schema",
    "job_class",
    "program",
    "chain",
    "schedule",
    "window",
    "window_group",
    "file_watcher",
    "credential",
    "destination",
];

const NO_EMAIL: &str = "Las notificaciones por correo necesitan el servidor de correo del Scheduler (el atributo email_server): hasta que se configure no se pueden agregar.";
const LEGACY_OWN: &str = "DBMS_JOB solo cambia los jobs del propio usuario.";
const EXTERNAL_NOTE: &str = "Los jobs ejecutables, de script externo, de SQL*Plus y de RMAN necesitan el privilegio CREATE EXTERNAL JOB y, salvo en el servidor local con el usuario del agente, una credencial.";
const ENABLED_EDIT: &str = "Si el job está habilitado, el script lo deshabilita antes de cambiar qué ejecuta o cuándo, y lo vuelve a habilitar al final. Si una llamada falla en el medio, queda deshabilitado, para que no se ejecute a medio cambiar: revisalo y habilitalo.";

/// The field of each inline type's action.
fn action_key(kind: &str) -> &'static str {
    match kind {
        "STORED_PROCEDURE" => "procedure",
        "EXECUTABLE" => "executable",
        "EXTERNAL_SCRIPT" => "external_script",
        "SQL_SCRIPT" => "sql_script",
        "BACKUP_SCRIPT" => "backup_script",
        _ => "plsql",
    }
}

// ---------------------------------------------------------------------------
// The job as the form says it
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Action {
    /// `job_type` and `job_action`.
    Inline { kind: String, action: String },
    /// A chain (its id).
    Chain(String),
    /// A program (its id).
    Program(String),
}

impl Action {
    pub(crate) fn job_type(&self) -> &str {
        match self {
            Action::Inline { kind, .. } => kind,
            Action::Chain(_) => "CHAIN",
            Action::Program(_) => "PROGRAM",
        }
    }

    /// Its arguments are the job's own (`number_of_arguments`).
    fn own_args(&self) -> bool {
        matches!(self, Action::Inline { kind, .. } if WITH_ARGS.contains(&kind.as_str()))
    }

    fn takes_args(&self) -> bool {
        self.own_args() || matches!(self, Action::Program(_))
    }
}

/// An argument by position: its name (the program's), its value as text,
/// and whether it holds a value that isn't text (kept as is).
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Arg {
    pub(crate) name: Option<String>,
    pub(crate) value: Option<String>,
    pub(crate) anydata: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Notification {
    pub(crate) recipients: Vec<String>,
    pub(crate) events: Vec<String>,
    pub(crate) sender: Option<String>,
    pub(crate) subject: Option<String>,
    pub(crate) body: Option<String>,
    pub(crate) filter: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct JobSpec {
    pub(crate) owner: String,
    pub(crate) name: String,
    pub(crate) style: String,
    pub(crate) action: Action,
    pub(crate) args: Vec<Arg>,
    pub(crate) when: When,
    /// The job class's id (`SYS.NAME`).
    pub(crate) class: String,
    pub(crate) enabled: bool,
    pub(crate) auto_drop: bool,
    pub(crate) comments: Option<String>,
    pub(crate) priority: i64,
    pub(crate) logging: String,
    pub(crate) restartable: bool,
    pub(crate) restart_on_recovery: bool,
    pub(crate) restart_on_failure: bool,
    pub(crate) max_runs: Option<i64>,
    pub(crate) max_failures: Option<i64>,
    pub(crate) max_run_duration: Option<i64>,
    pub(crate) schedule_limit: Option<i64>,
    pub(crate) instance_id: Option<i64>,
    pub(crate) instance_stickiness: bool,
    pub(crate) stop_on_window_close: bool,
    pub(crate) raise_events: Vec<String>,
    pub(crate) store_output: bool,
    pub(crate) allow_restricted: bool,
    pub(crate) fail_on_script_error: bool,
    pub(crate) credential: Option<String>,
    pub(crate) connect_credential: Option<String>,
    pub(crate) destination: Option<String>,
    pub(crate) notifications: Vec<Notification>,
}

impl JobSpec {
    /// A new job as Oracle makes it (what `CREATE_JOB` leaves unset).
    pub(crate) fn defaults(owner: &str, name: &str) -> JobSpec {
        JobSpec {
            owner: owner.into(),
            name: name.into(),
            style: "REGULAR".into(),
            action: Action::Inline {
                kind: "PLSQL_BLOCK".into(),
                action: String::new(),
            },
            args: Vec::new(),
            when: When::Immediate,
            class: DEFAULT_CLASS.into(),
            enabled: true,
            auto_drop: false,
            comments: None,
            priority: 3,
            logging: "OFF".into(),
            restartable: false,
            restart_on_recovery: false,
            restart_on_failure: false,
            max_runs: None,
            max_failures: None,
            max_run_duration: None,
            schedule_limit: None,
            instance_id: None,
            instance_stickiness: true,
            stop_on_window_close: false,
            raise_events: Vec::new(),
            store_output: true,
            allow_restricted: false,
            fail_on_script_error: false,
            credential: None,
            connect_credential: None,
            destination: None,
            notifications: Vec::new(),
        }
    }

    fn id(&self) -> String {
        obj_id(Some(&self.owner), &self.name)
    }

    /// The attributes `CREATE_JOB` doesn't take, as `SET_ATTRIBUTE` sets
    /// them: only those the dictionary has here.
    pub(crate) fn extra_attrs(&self, cols: &HashSet<String>) -> Vec<(&'static str, A)> {
        let all = [
            ("JOB_PRIORITY", "job_priority", A::I(self.priority)),
            (
                "LOGGING_LEVEL",
                "logging_level",
                A::R(format!("DBMS_SCHEDULER.LOGGING_{}", self.logging)),
            ),
            ("RESTARTABLE", "restartable", A::B(self.restartable)),
            (
                "RESTART_ON_RECOVERY",
                "restart_on_recovery",
                A::B(self.restart_on_recovery),
            ),
            (
                "RESTART_ON_FAILURE",
                "restart_on_failure",
                A::B(self.restart_on_failure),
            ),
            ("MAX_RUNS", "max_runs", A::int(self.max_runs)),
            ("MAX_FAILURES", "max_failures", A::int(self.max_failures)),
            (
                "MAX_RUN_DURATION",
                "max_run_duration",
                A::secs(self.max_run_duration),
            ),
            (
                "SCHEDULE_LIMIT",
                "schedule_limit",
                A::secs(self.schedule_limit),
            ),
            ("INSTANCE_ID", "instance_id", A::int(self.instance_id)),
            (
                "INSTANCE_STICKINESS",
                "instance_stickiness",
                A::B(self.instance_stickiness),
            ),
            (
                "STOP_ON_WINDOW_CLOSE",
                "stop_on_window_close",
                A::B(self.stop_on_window_close),
            ),
            (
                "RAISE_EVENTS",
                "raise_events",
                events_arg(&self.raise_events),
            ),
            ("STORE_OUTPUT", "store_output", A::B(self.store_output)),
            (
                "ALLOW_RUNS_IN_RESTRICTED_MODE",
                "allow_runs_in_restricted_mode",
                A::B(self.allow_restricted),
            ),
            (
                "FAIL_ON_SCRIPT_ERROR",
                "fail_on_script_error",
                A::B(self.fail_on_script_error),
            ),
            (
                "CONNECT_CREDENTIAL_NAME",
                "connect_credential_name",
                self.connect_credential.clone().map_or(A::Null, A::Name),
            ),
        ];
        all.into_iter()
            .filter(|(col, _, _)| cols.contains(*col))
            .map(|(_, attr, a)| (attr, a))
            .collect()
    }

    /// What `CREATE_JOB` takes and `SET_ATTRIBUTE` changes after.
    fn create_attrs(&self) -> Vec<(&'static str, A)> {
        vec![
            ("auto_drop", A::B(self.auto_drop)),
            ("comments", A::opt(self.comments.clone())),
            ("job_class", class_arg(&self.class)),
            (
                "credential_name",
                self.credential.clone().map_or(A::Null, A::Name),
            ),
            (
                "destination_name",
                self.destination.clone().map_or(A::Null, A::Name),
            ),
        ]
    }

    /// What it runs, where and as whom, changed: the job is disabled
    /// around such a change.
    fn structure(
        &self,
    ) -> (
        &Action,
        &Vec<Arg>,
        &When,
        &String,
        &Option<String>,
        &Option<String>,
    ) {
        (
            &self.action,
            &self.args,
            &self.when,
            &self.class,
            &self.credential,
            &self.destination,
        )
    }
}

/// `'"NAME"'` of a job class (they are SYS's).
fn class_arg(id: &str) -> A {
    super::bare(id)
}

/// `DBMS_SCHEDULER.JOB_STARTED + …` (distinct bits: their sum is their OR).
fn events_arg(events: &[String]) -> A {
    if events.is_empty() {
        return A::Null;
    }
    A::R(
        events
            .iter()
            .map(|e| format!("DBMS_SCHEDULER.{e}"))
            .collect::<Vec<_>>()
            .join(" + "),
    )
}

/// Events as the dictionary lists them, in [`EVENTS`]' order, the
/// combined ones taken apart.
pub(crate) fn events_of(list: &str) -> Vec<String> {
    let mut set: Vec<&str> = Vec::new();
    for e in list.split(',').map(|e| e.trim().to_ascii_uppercase()) {
        match e.as_str() {
            "JOB_ALL_EVENTS" => set.extend(EVENTS),
            "JOB_RUN_COMPLETED" => set.extend(["JOB_SUCCEEDED", "JOB_FAILED", "JOB_STOPPED"]),
            other => {
                if let Some(k) = EVENTS.iter().find(|k| **k == other) {
                    set.push(k);
                }
            }
        }
    }
    EVENTS
        .iter()
        .filter(|e| set.contains(e))
        .map(|e| e.to_string())
        .collect()
}

fn events_field(v: Vals, key: &str, label: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for e in v.strings(key) {
        let u = e.to_ascii_uppercase();
        if !EVENTS.contains(&u.as_str()) {
            return Err(bad(format!(
                "«{label}»: «{}» no es un evento de job.",
                shown(&e)
            )));
        }
        out.push(u);
    }
    Ok(EVENTS
        .iter()
        .filter(|e| out.iter().any(|o| o == *e))
        .map(|e| e.to_string())
        .collect())
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

pub(crate) struct JobState {
    pub(crate) spec: JobSpec,
    pub(crate) rec: Rec,
    pub(crate) system: bool,
}

fn yes_or(r: &Rec, col: &str, default: bool) -> bool {
    match r.t(col).as_deref() {
        Some("TRUE") => true,
        Some("FALSE") => false,
        _ => default,
    }
}

/// The spec a dictionary row (plus its arguments and notifications) says.
pub(crate) fn spec_of_rec(r: &Rec, args: Vec<Arg>, notifications: Vec<Notification>) -> JobSpec {
    let owner = r.t("OWNER").unwrap_or_default();
    let name = r.raw("JOB_NAME").unwrap_or_default();
    let action = match (r.t("PROGRAM_NAME"), r.t("JOB_TYPE")) {
        (Some(p), _) => Action::Program(obj_id(r.t("PROGRAM_OWNER").as_deref(), &p)),
        (None, Some(t)) if t == "CHAIN" => {
            let text = r.t("JOB_ACTION").unwrap_or_default();
            match jobs::object_name(&text, &owner) {
                Some((o, n)) => Action::Chain(obj_id(Some(&o), &n)),
                None => Action::Chain(text),
            }
        }
        (None, t) => Action::Inline {
            kind: t.unwrap_or_else(|| "PLSQL_BLOCK".into()),
            action: r
                .raw("JOB_ACTION")
                .unwrap_or_default()
                .trim_end()
                .to_string(),
        },
    };
    let d = JobSpec::defaults(&owner, &name);
    JobSpec {
        style: r.t("JOB_STYLE").unwrap_or_else(|| "REGULAR".into()),
        action,
        args,
        when: When::of(r),
        class: r
            .t("JOB_CLASS")
            .map(|c| obj_id(Some("SYS"), &c))
            .unwrap_or_else(|| DEFAULT_CLASS.into()),
        enabled: r.yes("ENABLED"),
        auto_drop: r.yes("AUTO_DROP"),
        comments: r.t("COMMENTS"),
        priority: r.int("JOB_PRIORITY").unwrap_or(3),
        logging: r
            .t("LOGGING_LEVEL")
            .map(|l| l.replace(' ', "_"))
            .filter(|l| LOGGING.contains(&l.as_str()))
            .unwrap_or_else(|| "OFF".into()),
        restartable: yes_or(r, "RESTARTABLE", d.restartable),
        restart_on_recovery: yes_or(r, "RESTART_ON_RECOVERY", d.restart_on_recovery),
        restart_on_failure: yes_or(r, "RESTART_ON_FAILURE", d.restart_on_failure),
        max_runs: r.int("MAX_RUNS"),
        max_failures: r.int("MAX_FAILURES"),
        max_run_duration: r.int("MAX_RUN_DURATION"),
        schedule_limit: r.int("SCHEDULE_LIMIT"),
        instance_id: r.int("INSTANCE_ID"),
        instance_stickiness: yes_or(r, "INSTANCE_STICKINESS", d.instance_stickiness),
        stop_on_window_close: yes_or(r, "STOP_ON_WINDOW_CLOSE", d.stop_on_window_close),
        raise_events: r
            .t("RAISE_EVENTS")
            .map(|e| events_of(&e))
            .unwrap_or_default(),
        store_output: yes_or(r, "STORE_OUTPUT", d.store_output),
        allow_restricted: yes_or(r, "ALLOW_RUNS_IN_RESTRICTED_MODE", d.allow_restricted),
        fail_on_script_error: yes_or(r, "FAIL_ON_SCRIPT_ERROR", d.fail_on_script_error),
        credential: r
            .t("CREDENTIAL_NAME")
            .map(|n| obj_id(r.t("CREDENTIAL_OWNER").as_deref(), &n)),
        connect_credential: r
            .t("CONNECT_CREDENTIAL_NAME")
            .map(|n| obj_id(r.t("CONNECT_CREDENTIAL_OWNER").as_deref(), &n)),
        destination: r
            .t("DESTINATION")
            .map(|n| obj_id(r.t("DESTINATION_OWNER").as_deref(), &n)),
        notifications,
        ..d
    }
}

/// The job's arguments by position: the program's definitions (names,
/// metadata) and the job's values.
pub(crate) fn args_of(job_args: &[Rec], program_args: &[Rec], own: Option<i64>) -> Vec<Arg> {
    let max = job_args
        .iter()
        .chain(program_args)
        .filter_map(|r| r.int("ARGUMENT_POSITION"))
        .max()
        .unwrap_or(0)
        .max(own.unwrap_or(0));
    (1..=max)
        .map(|p| {
            let at = |list: &[Rec]| {
                list.iter()
                    .find(|r| r.int("ARGUMENT_POSITION") == Some(p))
                    .cloned()
            };
            let def = at(program_args);
            let val = at(job_args);
            let name = def
                .as_ref()
                .and_then(|d| {
                    let n = d.t("ARGUMENT_NAME");
                    match d.t("METADATA_ATTRIBUTE") {
                        Some(m) => Some(
                            format!("{} ({m})", n.unwrap_or_default())
                                .trim()
                                .to_string(),
                        ),
                        None => n,
                    }
                })
                .or_else(|| val.as_ref().and_then(|v| v.t("ARGUMENT_NAME")));
            Arg {
                name,
                value: val.as_ref().and_then(|v| v.raw("VALUE")),
                anydata: val
                    .as_ref()
                    .is_some_and(|v| v.raw("VALUE").is_none() && v.yes("ANYDATA_VALUE")),
            }
        })
        .collect()
}

/// Notifications as the dictionary has them (one row per recipient and
/// event), grouped back into what was added.
pub(crate) fn notifications_of(rows: &[Rec]) -> Vec<Notification> {
    type Key = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let mut by_recipient: BTreeMap<(Key, String), Vec<String>> = BTreeMap::new();
    for r in rows {
        let Some(recipient) = r.t("RECIPIENT") else {
            continue;
        };
        let key = (
            r.t("SENDER"),
            r.t("SUBJECT"),
            r.t("BODY"),
            r.t("FILTER_CONDITION"),
        );
        let events = by_recipient.entry((key, recipient)).or_default();
        if let Some(e) = r.t("EVENT") {
            events.push(e);
        }
    }
    let mut merged: BTreeMap<(Key, Vec<String>), Vec<String>> = BTreeMap::new();
    for ((key, recipient), events) in by_recipient {
        let events = events_of(&events.join(","));
        merged.entry((key, events)).or_default().push(recipient);
    }
    let mut out: Vec<Notification> = merged
        .into_iter()
        .map(|((key, events), recipients)| Notification {
            recipients,
            events,
            sender: key.0,
            subject: key.1,
            body: key.2,
            filter: key.3,
        })
        .collect();
    out.sort();
    out
}

fn view_cols(c: &Connection, ctx: &Ctx) -> Result<HashSet<String>> {
    read::columns(c, &ctx.sql("{v}_scheduler_jobs")).map_err(crate::err)
}

pub(crate) fn read_state(c: &Connection, ctx: &Ctx, owner: &str, name: &str) -> Result<JobState> {
    let rec = read::row(
        c,
        &ctx.sql("{v}_scheduler_jobs"),
        "WHERE owner = :1 AND job_name = :2 AND job_subname IS NULL",
        &[&owner, &name],
    )
    .map_err(crate::err)?
    .ok_or_else(|| not_found(owner, name))?;
    let job_args = read::rows(
        c,
        &ctx.sql("{v}_scheduler_job_args"),
        "WHERE owner = :1 AND job_name = :2 ORDER BY argument_position",
        &[&owner, &name],
        1000,
    )
    .unwrap_or_else(|e| {
        tracing::debug!("oracle scheduler: job arguments: {e}");
        Vec::new()
    });
    let program_args = match (rec.t("PROGRAM_OWNER"), rec.t("PROGRAM_NAME")) {
        (Some(po), Some(pn)) => read::rows(
            c,
            &ctx.sql("{v}_scheduler_program_args"),
            "WHERE owner = :1 AND program_name = :2 ORDER BY argument_position",
            &[&po, &pn],
            1000,
        )
        .unwrap_or_default(),
        _ => Vec::new(),
    };
    let own = rec
        .t("PROGRAM_NAME")
        .is_none()
        .then(|| rec.int("NUMBER_OF_ARGUMENTS"))
        .flatten();
    let notifications = if ctx.notifications() {
        read::rows(
            c,
            &ctx.sql("{v}_scheduler_notifications"),
            "WHERE owner = :1 AND job_name = :2 AND job_subname IS NULL",
            &[&owner, &name],
            1000,
        )
        .map(|r| notifications_of(&r))
        .unwrap_or_default()
    } else {
        Vec::new()
    };
    let spec = spec_of_rec(&rec, args_of(&job_args, &program_args, own), notifications);
    let system = rec.yes("SYSTEM") || ctx.oracle_schema(owner);
    Ok(JobState { spec, rec, system })
}

fn not_found(owner: &str, name: &str) -> Error {
    bad(format!(
        "no existe el job «{}» o este usuario no lo puede ver",
        jobs::job_id(owner, name)
    ))
}

// ---------------------------------------------------------------------------
// The form's values
// ---------------------------------------------------------------------------

type Rows = Vec<BTreeMap<String, Value>>;

/// The designer's values for a spec (what the fingerprint covers).
pub(crate) fn values_of(s: &JobSpec) -> (BTreeMap<String, Value>, BTreeMap<String, Rows>) {
    let mut values = BTreeMap::new();
    let mut put = |k: &str, v: Value| {
        values.insert(k.to_string(), v);
    };
    put("owner", json!(s.owner));
    put("name", json!(s.name));
    put("job_style", json!(s.style));
    put("job_type", json!(s.action.job_type()));
    match &s.action {
        Action::Inline { kind, action } => put(action_key(kind), json!(action)),
        Action::Chain(id) => put("chain", json!(id)),
        Action::Program(id) => put("program", json!(id)),
    }
    s.when.values(&mut put);
    put("job_class", json!(s.class));
    put("enabled", json!(s.enabled));
    put("auto_drop", json!(s.auto_drop));
    put("comments", json!(s.comments.clone().unwrap_or_default()));
    put("job_priority", json!(s.priority));
    put("logging_level", json!(s.logging));
    put("restartable", json!(s.restartable));
    put("restart_on_recovery", json!(s.restart_on_recovery));
    put("restart_on_failure", json!(s.restart_on_failure));
    put("max_runs", json!(s.max_runs));
    put("max_failures", json!(s.max_failures));
    put("max_run_duration", json!(s.max_run_duration));
    put("schedule_limit", json!(s.schedule_limit));
    put("instance_id", json!(s.instance_id));
    put("instance_stickiness", json!(s.instance_stickiness));
    put("stop_on_window_close", json!(s.stop_on_window_close));
    put("raise_events", json!(s.raise_events));
    put("store_output", json!(s.store_output));
    put("allow_runs_in_restricted_mode", json!(s.allow_restricted));
    put("fail_on_script_error", json!(s.fail_on_script_error));
    put(
        "credential",
        json!(s.credential.clone().unwrap_or_default()),
    );
    put(
        "connect_credential",
        json!(s.connect_credential.clone().unwrap_or_default()),
    );
    put(
        "destination",
        json!(s.destination.clone().unwrap_or_default()),
    );
    let mut lists = BTreeMap::new();
    lists.insert(
        "arguments".to_string(),
        s.args
            .iter()
            .map(|a| {
                let mut row = BTreeMap::from([
                    (
                        "name".to_string(),
                        json!(a.name.clone().unwrap_or_default()),
                    ),
                    (
                        "value".to_string(),
                        json!(a.value.clone().unwrap_or_default()),
                    ),
                ]);
                if a.anydata {
                    row.insert("anydata".into(), json!(true));
                }
                row
            })
            .collect(),
    );
    lists.insert(
        "notifications".to_string(),
        s.notifications
            .iter()
            .map(|n| {
                BTreeMap::from([
                    ("recipients".to_string(), json!(n.recipients.join(", "))),
                    ("events".to_string(), json!(n.events)),
                    (
                        "sender".to_string(),
                        json!(n.sender.clone().unwrap_or_default()),
                    ),
                    (
                        "subject".to_string(),
                        json!(n.subject.clone().unwrap_or_default()),
                    ),
                    (
                        "body".to_string(),
                        json!(n.body.clone().unwrap_or_default()),
                    ),
                    (
                        "filter_condition".to_string(),
                        json!(n.filter.clone().unwrap_or_default()),
                    ),
                ])
            })
            .collect(),
    );
    (values, lists)
}

/// The spec from the designer, checked. `cur`: the job as it is now.
pub(crate) fn spec_of(
    values: &FormValues,
    ctx: &Ctx,
    cols: &HashSet<String>,
    cat: &Catalog,
    cur: Option<&JobSpec>,
) -> Result<JobSpec> {
    let v = Vals(&values.values);
    let (owner, name) = match cur {
        Some(s) => (s.owner.clone(), s.name.clone()),
        None => (
            new_owner(v, ctx, Kind::Job, cat)?,
            new_name(&v.raw("name"), ctx, "Nombre")?,
        ),
    };
    let mut s = JobSpec::defaults(&owner, &name);
    let had = |f: fn(&JobSpec) -> Option<&str>| cur.and_then(f);
    s.style = match cur {
        Some(c) => c.style.clone(),
        None => v.choice("job_style", "Estilo", &STYLES, "REGULAR")?,
    };
    let kind = v.choice("job_type", "Tipo", &TYPES, "PLSQL_BLOCK")?;
    if ctx.cloud && EXTERNAL.contains(&kind.as_str()) {
        return Err(bad(format!(
            "Oracle Autonomous Database no ejecuta jobs de tipo {kind}."
        )));
    }
    let cur_action = cur.map(|c| &c.action);
    s.action = match kind.as_str() {
        "CHAIN" => {
            let now = match cur_action {
                Some(Action::Chain(id)) => Some(id.as_str()),
                _ => None,
            };
            Action::Chain(pick_ref(cat, "chain", &v.text("chain"), now, "Cadena")?)
        }
        "PROGRAM" => {
            let now = match cur_action {
                Some(Action::Program(id)) => Some(id.as_str()),
                _ => None,
            };
            Action::Program(pick_ref(
                cat,
                "program",
                &v.text("program"),
                now,
                "Programa",
            )?)
        }
        k => {
            let key = action_key(k);
            let action = if matches!(k, "STORED_PROCEDURE" | "EXECUTABLE") {
                v.text(key)
            } else {
                v.code(key).unwrap_or_default()
            };
            if action.trim().is_empty() {
                return Err(bad("Falta la acción del job: qué ejecuta.".into()));
            }
            if action.len() > 4000 {
                return Err(bad(
                    "La acción del job tiene más de 4000 bytes, lo que Oracle acepta: pasala a un procedimiento o un programa."
                        .into(),
                ));
            }
            Action::Inline {
                kind: k.to_string(),
                action,
            }
        }
    };
    if s.style != "REGULAR" && !matches!(s.action, Action::Program(_)) {
        return Err(bad(
            "Los jobs livianos y en memoria ejecutan un programa: elegí uno en «Tipo».".into(),
        ));
    }
    // Arguments.
    let cur_args = cur.map(|c| c.args.as_slice()).unwrap_or(&[]);
    // The form hides the list for a type that takes none: rows it still
    // holds don't apply (nor are they checked).
    let arg_rows = if s.action.takes_args() {
        rows(values, "arguments")
    } else {
        Vec::new()
    };
    for (i, r) in arg_rows.into_iter().enumerate() {
        let raw = r.raw("value");
        let anydata = r.bool("anydata") && cur_args.get(i).is_some_and(|a| a.anydata);
        s.args.push(Arg {
            name: r.opt("name"),
            value: (!raw.is_empty()).then_some(raw),
            anydata,
        });
    }
    if s.args.len() > 255 {
        return Err(bad("Un job recibe a lo sumo 255 argumentos.".into()));
    }
    for (i, a) in s.args.iter().enumerate() {
        super::fits(&a.value, &format!("Argumento {}", i + 1), 4000)?;
    }
    // Schedule.
    let modes: Vec<&str> = MODES
        .iter()
        .copied()
        .filter(|m| *m != "file_watcher" || ctx.has(Kind::FileWatcher))
        .collect();
    s.when = resolve_when(When::from_values(v, &modes)?, cat, cur.map(|c| &c.when))?;
    // General and options.
    s.class = pick_ref(
        cat,
        "job_class",
        &v.opt("job_class").unwrap_or_else(|| DEFAULT_CLASS.into()),
        // The default class is everyone's, listed or not.
        had(|c| Some(c.class.as_str())).or(Some(DEFAULT_CLASS)),
        "Clase",
    )?;
    s.enabled = v.bool_or("enabled", true);
    s.auto_drop = v.bool("auto_drop");
    s.comments = v.opt("comments");
    super::fits(&s.comments, "Descripción", 4000)?;
    let d = JobSpec::defaults("", "");
    let has = |col: &str| cols.contains(col);
    if has("JOB_PRIORITY") {
        s.priority = v.int("job_priority", "Prioridad", 1, 5)?.unwrap_or(3);
    }
    if has("LOGGING_LEVEL") {
        s.logging = v.choice("logging_level", "Nivel de registro", &LOGGING, "OFF")?;
    }
    let flag = |k: &str, col: &str, default: bool| {
        if has(col) {
            v.bool_or(k, default)
        } else {
            default
        }
    };
    s.restartable = flag("restartable", "RESTARTABLE", d.restartable);
    s.restart_on_recovery = flag(
        "restart_on_recovery",
        "RESTART_ON_RECOVERY",
        d.restart_on_recovery,
    );
    s.restart_on_failure = flag(
        "restart_on_failure",
        "RESTART_ON_FAILURE",
        d.restart_on_failure,
    );
    s.instance_stickiness = flag(
        "instance_stickiness",
        "INSTANCE_STICKINESS",
        d.instance_stickiness,
    );
    s.stop_on_window_close = flag(
        "stop_on_window_close",
        "STOP_ON_WINDOW_CLOSE",
        d.stop_on_window_close,
    );
    s.store_output = flag("store_output", "STORE_OUTPUT", d.store_output);
    s.allow_restricted = flag(
        "allow_runs_in_restricted_mode",
        "ALLOW_RUNS_IN_RESTRICTED_MODE",
        d.allow_restricted,
    );
    s.fail_on_script_error = flag(
        "fail_on_script_error",
        "FAIL_ON_SCRIPT_ERROR",
        d.fail_on_script_error,
    );
    if has("MAX_RUNS") {
        s.max_runs = v.int("max_runs", "Máximo de ejecuciones", 1, 2_147_483_647)?;
    }
    if has("MAX_FAILURES") {
        s.max_failures = v.int("max_failures", "Máximo de fallas", 1, 2_147_483_647)?;
    }
    if has("MAX_RUN_DURATION") {
        s.max_run_duration = secs(v, "max_run_duration", "Duración máxima")?.filter(|n| *n > 0);
    }
    if has("SCHEDULE_LIMIT") {
        s.schedule_limit = secs(v, "schedule_limit", "Demora máxima")?.filter(|n| *n > 0);
    }
    if has("INSTANCE_ID") {
        s.instance_id = v.int("instance_id", "Instancia", 1, 9999)?;
    }
    if has("RAISE_EVENTS") {
        s.raise_events = events_field(v, "raise_events", "Eventos que publica")?;
    }
    s.credential = opt_ref(
        v,
        "credential",
        cat,
        "credential",
        had(|c| c.credential.as_deref()),
        "Credencial",
    )?;
    if has("CONNECT_CREDENTIAL_NAME") {
        s.connect_credential = opt_ref(
            v,
            "connect_credential",
            cat,
            "credential",
            had(|c| c.connect_credential.as_deref()),
            "Credencial de conexión",
        )?;
    }
    s.destination = opt_ref(
        v,
        "destination",
        cat,
        "destination",
        had(|c| c.destination.as_deref()),
        "Destino",
    )?;
    // Notifications.
    if ctx.notifications() {
        for r in rows(values, "notifications") {
            s.notifications.push(notification(r)?);
        }
        s.notifications.sort();
    }
    Ok(s)
}

/// A schedule's named objects checked against the server's.
fn resolve_when(w: When, cat: &Catalog, cur: Option<&When>) -> Result<When> {
    let now = cur.and_then(When::schedule_name).or(match cur {
        Some(When::FileWatcher(f)) => Some(f.as_str()),
        _ => None,
    });
    Ok(match w {
        When::Named(n) => When::Named(pick_ref(cat, "schedule", &n, now, "Programación")?),
        When::Window(n) => When::Window(pick_ref(cat, "window", &n, now, "Ventana")?),
        When::WindowGroup(n) => {
            When::WindowGroup(pick_ref(cat, "window_group", &n, now, "Grupo de ventanas")?)
        }
        When::FileWatcher(n) => When::FileWatcher(pick_ref(
            cat,
            "file_watcher",
            &n,
            now,
            "Observador de archivos",
        )?),
        other => other,
    })
}

fn notification(r: Vals) -> Result<Notification> {
    let recipients: Vec<String> = r
        .text("recipients")
        .split([',', ';'])
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if recipients.is_empty() {
        return Err(bad("Notificaciones: falta «Destinatarios».".into()));
    }
    for m in &recipients {
        if !m.contains('@')
            || m.chars()
                .any(|c| c.is_whitespace() || c == '\'' || c == '"')
        {
            return Err(bad(format!(
                "Notificaciones: «{}» no es una dirección de correo.",
                shown(m)
            )));
        }
    }
    let events = events_field(r, "events", "Eventos")?;
    if events.is_empty() {
        return Err(bad("Notificaciones: falta «Eventos».".into()));
    }
    let n = Notification {
        recipients,
        events,
        sender: r.opt("sender"),
        subject: r.opt("subject"),
        body: r.opt("body"),
        filter: r.opt("filter_condition"),
    };
    super::fits(&n.body, "Mensaje", 4000)?;
    super::fits(&n.subject, "Asunto", 4000)?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

fn add_notification(b: &mut Block, target: &A, n: &Notification) {
    let mut args = vec![
        ("job_name", target.clone()),
        ("recipients", A::S(n.recipients.join(", "))),
    ];
    if let Some(s) = &n.sender {
        args.push(("sender", A::S(s.clone())));
    }
    if let Some(s) = &n.subject {
        args.push(("subject", A::S(s.clone())));
    }
    if let Some(s) = &n.body {
        args.push(("body", A::S(s.clone())));
    }
    args.push(("events", A::S(n.events.join(", "))));
    if let Some(s) = &n.filter {
        args.push(("filter_condition", A::S(s.clone())));
    }
    b.call("DBMS_SCHEDULER.ADD_JOB_EMAIL_NOTIFICATION", args);
}

pub(crate) fn create_block(s: &JobSpec, cols: &HashSet<String>) -> Block {
    let id = s.id();
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Crea el job {id}."));
    let mut args = vec![("job_name", target.clone())];
    match &s.action {
        Action::Inline { kind, action } => {
            args.push(("job_type", A::S(kind.clone())));
            args.push(("job_action", A::S(action.clone())));
            if s.action.own_args() && !s.args.is_empty() {
                args.push(("number_of_arguments", A::I(s.args.len() as i64)));
            }
        }
        Action::Chain(chain) => {
            args.push(("job_type", A::S("CHAIN".into())));
            args.push(("job_action", A::Name(chain.clone())));
        }
        Action::Program(p) => args.push(("program_name", A::Name(p.clone()))),
    }
    args.extend(timing::create_args(&s.when));
    if s.class != DEFAULT_CLASS {
        args.push(("job_class", class_arg(&s.class)));
    }
    args.push(("enabled", A::B(false)));
    args.push(("auto_drop", A::B(s.auto_drop)));
    if let Some(cm) = &s.comments {
        args.push(("comments", A::S(cm.clone())));
    }
    if matches!(s.action, Action::Program(_)) && s.style != "REGULAR" {
        args.push(("job_style", A::S(s.style.clone())));
    }
    if let Some(cr) = &s.credential {
        args.push(("credential_name", A::Name(cr.clone())));
    }
    if let Some(d) = &s.destination {
        args.push(("destination_name", A::Name(d.clone())));
    }
    b.call("DBMS_SCHEDULER.CREATE_JOB", args);
    b.created(&format!(
        "DBMS_SCHEDULER.DROP_JOB(job_name => {}, force => TRUE);",
        name_lit(&id)
    ));
    let base = JobSpec::defaults(&s.owner, &s.name);
    let defaults = base.extra_attrs(cols);
    for (attr, a) in s.extra_attrs(cols) {
        let same = defaults.iter().any(|(k, d)| *k == attr && *d == a);
        if !same {
            b.set_attr(&target, attr, a);
        }
    }
    for (i, a) in s.args.iter().enumerate() {
        if let Some(value) = &a.value {
            b.call(
                "DBMS_SCHEDULER.SET_JOB_ARGUMENT_VALUE",
                vec![
                    ("job_name", target.clone()),
                    ("argument_position", A::I(i as i64 + 1)),
                    ("argument_value", A::S(value.clone())),
                ],
            );
        }
    }
    for n in &s.notifications {
        add_notification(&mut b, &target, n);
    }
    if s.enabled {
        b.pcall("DBMS_SCHEDULER.ENABLE", vec![target]);
    }
    b
}

/// The calls that take the job from `old` to `new`, in place.
pub(crate) fn alter_block(old: &JobSpec, new: &JobSpec, cols: &HashSet<String>) -> Block {
    let id = old.id();
    let target = A::Name(id.clone());
    let mut b = Block::new(&format!("Cambia el job {id}."));
    // A calendar string Oracle refuses fails here, before anything changes.
    timing::check_first(&mut b, &old.when, &new.when);
    let structural = old.structure() != new.structure();
    let mut disabled = false;
    if old.enabled && structural {
        let t = target.clone();
        b.disabled_first(&id, |b| {
            b.call(
                "DBMS_SCHEDULER.DISABLE",
                vec![("name", t), ("force", A::B(true))],
            )
        });
        disabled = true;
    }
    // Values that won't apply: reset while their positions still exist
    // (all of them when what it runs changes: Oracle checks the positions
    // against the new action's).
    let same_action = old.action == new.action;
    if old.action.takes_args() {
        let keep = if same_action { new.args.len() } else { 0 };
        for (i, a) in old.args.iter().enumerate().skip(keep) {
            if a.value.is_some() {
                reset_arg(&mut b, &target, i);
            }
        }
    }
    // What it runs.
    if !same_action {
        match (&old.action, &new.action) {
            (Action::Program(_), Action::Program(p)) => {
                b.set_attr(&target, "program_name", A::Name(p.clone()))
            }
            (Action::Program(_), _) => {
                b.set_attr(&target, "program_name", A::Null);
                set_inline(&mut b, &target, &new.action, None);
            }
            (_, Action::Program(p)) => {
                b.set_attr(&target, "job_action", A::Null);
                b.set_attr(&target, "job_type", A::Null);
                b.set_attr(&target, "number_of_arguments", A::Null);
                b.set_attr(&target, "program_name", A::Name(p.clone()));
            }
            (o, n) => set_inline(&mut b, &target, n, Some(o)),
        }
    }
    // A job's own action always has a number of arguments (0 when its
    // type takes none); a program's job has none of its own.
    if !matches!(new.action, Action::Program(_)) {
        let n = |a: &Action, args: &[Arg]| match a {
            Action::Program(_) => None,
            a if a.own_args() => Some(args.len() as i64),
            _ => Some(0),
        };
        let new_n = n(&new.action, &new.args);
        if n(&old.action, &old.args) != new_n {
            b.set_attr(&target, "number_of_arguments", A::int(new_n));
        }
    }
    // Argument values.
    for (i, a) in new.args.iter().enumerate() {
        let before = old.args.get(i).filter(|_| same_action);
        match &a.value {
            Some(value) if before.and_then(|o| o.value.as_ref()) != Some(value) => b.call(
                "DBMS_SCHEDULER.SET_JOB_ARGUMENT_VALUE",
                vec![
                    ("job_name", target.clone()),
                    ("argument_position", A::I(i as i64 + 1)),
                    ("argument_value", A::S(value.clone())),
                ],
            ),
            None if before.is_some_and(|o| o.value.is_some()) => reset_arg(&mut b, &target, i),
            _ => {}
        }
    }
    // When.
    timing::changes(&mut b, &target, &old.when, &new.when);
    // The rest of its attributes.
    let olds: Vec<(&str, A)> = old
        .create_attrs()
        .into_iter()
        .chain(old.extra_attrs(cols))
        .collect();
    for (attr, a) in new.create_attrs().into_iter().chain(new.extra_attrs(cols)) {
        if !olds.iter().any(|(k, o)| *k == attr && *o == a) {
            b.set_attr(&target, attr, a);
        }
    }
    // Notifications: removed and added again when they differ.
    if old.notifications != new.notifications {
        if !old.notifications.is_empty() {
            b.call(
                "DBMS_SCHEDULER.REMOVE_JOB_EMAIL_NOTIFICATION",
                vec![("job_name", target.clone())],
            );
        }
        for n in &new.notifications {
            add_notification(&mut b, &target, n);
        }
    }
    // Enabled.
    if new.enabled && (disabled || !old.enabled) {
        let t = target.clone();
        b.enabled_again(|b| b.pcall("DBMS_SCHEDULER.ENABLE", vec![t]));
    } else if !new.enabled && old.enabled && !disabled {
        b.call(
            "DBMS_SCHEDULER.DISABLE",
            vec![("name", target), ("force", A::B(true))],
        );
    }
    b
}

fn reset_arg(b: &mut Block, target: &A, i: usize) {
    b.call(
        "DBMS_SCHEDULER.RESET_JOB_ARGUMENT_VALUE",
        vec![
            ("job_name", target.clone()),
            ("argument_position", A::I(i as i64 + 1)),
        ],
    );
}

/// `job_type` and `job_action` of an inline or chain job, only what
/// differs from `old`.
fn set_inline(b: &mut Block, target: &A, new: &Action, old: Option<&Action>) {
    let (kind, action) = match new {
        Action::Inline { kind, action } => (kind.clone(), A::S(action.clone())),
        Action::Chain(c) => ("CHAIN".to_string(), A::Name(c.clone())),
        Action::Program(_) => return,
    };
    let (old_kind, old_action) = match old {
        Some(Action::Inline { kind, action }) => (Some(kind.clone()), Some(A::S(action.clone()))),
        Some(Action::Chain(c)) => (Some("CHAIN".to_string()), Some(A::Name(c.clone()))),
        _ => (None, None),
    };
    if old_kind.as_deref() != Some(kind.as_str()) {
        b.set_attr(target, "job_type", A::S(kind));
    }
    if old_action.as_ref() != Some(&action) {
        b.set_attr(target, "job_action", action);
    }
}

// ---------------------------------------------------------------------------
// The contract's calls
// ---------------------------------------------------------------------------

pub(crate) fn items(c: &Connection, ctx: &Ctx) -> Result<SchedulerList> {
    use super::action::{END_DETACHED, FORCE_STOP};
    let list = jobs::list(c)?;
    let offered: Vec<String> = super::action::of(ctx, Kind::Job)
        .into_iter()
        .map(|a| a.key)
        .collect();
    let detached = if offered.iter().any(|k| k == END_DETACHED) {
        super::action::detached_runs(c, ctx)
    } else {
        HashSet::new()
    };
    Ok(SchedulerList {
        items: list
            .jobs
            .into_iter()
            .map(|j| {
                // Edited and dropped as its form allows: not Oracle's own,
                // and where the login manages it (DBMS_JOB: its own).
                let locked = j.system || !j.can_enable;
                // Stopped by force while it runs: MANAGE SCHEDULER is
                // all Oracle asks, whoever owns the job (Oracle's own too,
                // but on Autonomous Database).
                let mut actions = Vec::new();
                if j.running && super::action::force_stoppable(ctx, &j.id, j.system) {
                    actions.push(FORCE_STOP.to_string());
                }
                // Ended while it runs, where the login manages it as the
                // jobs list's "Detener" does.
                if j.can_stop && detached.contains(&j.id) {
                    actions.push(END_DETACHED.to_string());
                }
                SchedulerItem {
                    id: j.id,
                    name: j.name,
                    description: j.description,
                    enabled: Some(j.enabled),
                    system: j.system,
                    can_edit: super::unless(locked),
                    can_delete: super::unless(locked),
                    actions: (!offered.is_empty()).then_some(actions),
                }
            })
            .collect(),
        notes: list.notes,
    })
}

fn legacy_id(id: &str) -> Option<i64> {
    match jobs::parse_id(id) {
        Ok(JobId::Legacy(n)) => Some(n),
        _ => None,
    }
}

pub(crate) fn plan(
    c: &Connection,
    ctx: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    if let Some(n) = id.and_then(legacy_id) {
        return legacy::plan(c, ctx, Some(n), values);
    }
    if id.is_none() && Vals(&values.values).text("job_api") == "dbms_job" {
        if !(ctx.major > 0 && ctx.major < 19) {
            return Err(bad(
                "Desde 19c, DBMS_JOB crea jobs del Scheduler: elegí DBMS_SCHEDULER.".into(),
            ));
        }
        return legacy::plan(c, ctx, None, values);
    }
    let cols = view_cols(c, ctx)?;
    let cat = choices::load(c, ctx, &SOURCES);
    match id {
        None => {
            let s = spec_of(values, ctx, &cols, &cat, None)?;
            Ok(Plan::new(create_block(&s, &cols), s.id(), &s.name))
        }
        Some(id) => {
            let (owner, name) = parse_obj(id, "job")?;
            let st = read_state(c, ctx, &owner, &name)?;
            ctx.check_change(Kind::Job, &owner, &name, st.system)?;
            let (v, l) = values_of(&st.spec);
            fresh(values, &fingerprint(&v, &l))?;
            let s = spec_of(values, ctx, &cols, &cat, Some(&st.spec))?;
            Ok(Plan::changes(
                alter_block(&st.spec, &s, &cols),
                st.spec.id(),
                &name,
            ))
        }
    }
}

pub(crate) fn delete_block(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    if let Some(n) = legacy_id(id) {
        return legacy::delete_block(c, ctx, n);
    }
    let (owner, name) = parse_obj(id, "job")?;
    let st = read_state(c, ctx, &owner, &name)?;
    ctx.check_change(Kind::Job, &owner, &name, st.system)?;
    let id = st.spec.id();
    let mut b = Block::new(&format!("Borra el job {id}."));
    b.note("Si se está ejecutando, hay que detenerlo antes (o pasar force => TRUE).");
    b.call(
        "DBMS_SCHEDULER.DROP_JOB",
        vec![("job_name", A::Name(id)), ("force", A::B(false))],
    );
    Ok(b)
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

fn type_select(cloud: bool) -> FormKind {
    only(
        FormKind::select(&[
            ("PLSQL_BLOCK", "Bloque PL/SQL"),
            ("STORED_PROCEDURE", "Procedimiento almacenado"),
            ("EXECUTABLE", "Ejecutable"),
            ("EXTERNAL_SCRIPT", "Script externo"),
            ("SQL_SCRIPT", "Script de SQL*Plus"),
            ("BACKUP_SCRIPT", "Script de RMAN"),
            ("CHAIN", "Cadena"),
            ("PROGRAM", "Programa"),
        ]),
        |t| !(cloud && EXTERNAL.contains(&t)),
    )
}

/// The action's fields of an inline type, for jobs and programs.
pub(crate) fn action_fields(type_key: &str) -> Vec<FormField> {
    vec![
        FormField::new("plsql", "Bloque PL/SQL", FormKind::code("plsql"))
            .placeholder("BEGIN\n  mi_paquete.procesar;\nEND;")
            .when(type_key, &["PLSQL_BLOCK"]),
        FormField::new("procedure", "Procedimiento", FormKind::Text)
            .placeholder("ESQUEMA.PAQUETE.PROCEDIMIENTO")
            .help("El nombre del procedimiento; sus parámetros son los argumentos.")
            .when(type_key, &["STORED_PROCEDURE"]),
        FormField::new("executable", "Ejecutable", FormKind::Text)
            .placeholder("/u01/scripts/cargar.sh")
            .help("La ruta completa del ejecutable en el servidor; sus parámetros son los argumentos.")
            .when(type_key, &["EXECUTABLE"]),
        FormField::new("external_script", "Script externo", FormKind::code("shell"))
            .help("El texto del script o la ruta de un archivo del servidor.")
            .when(type_key, &["EXTERNAL_SCRIPT"]),
        FormField::new("sql_script", "Script de SQL*Plus", FormKind::code("sql"))
            .when(type_key, &["SQL_SCRIPT"]),
        FormField::new("backup_script", "Script de RMAN", FormKind::code("rman"))
            .when(type_key, &["BACKUP_SCRIPT"]),
    ]
}

pub(crate) fn form(c: &Connection, ctx: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    if let Some(n) = id.and_then(legacy_id) {
        return legacy::form(c, ctx, n);
    }
    let cols = view_cols(c, ctx)?;
    let cat = choices::load(c, ctx, &SOURCES);
    let state = match id {
        Some(id) => {
            let (o, n) = parse_obj(id, "job")?;
            Some(read_state(c, ctx, &o, &n)?)
        }
        None => None,
    };
    let spec = state
        .as_ref()
        .map(|s| s.spec.clone())
        .unwrap_or_else(|| JobSpec::defaults(ctx.user(), ""));
    let (mut values, lists) = values_of(&spec);
    let existing = state.is_some();
    let has = |col: &str| cols.contains(col);
    let mut f = ObjectForm::default();
    let legacy_new = !existing && ctx.major > 0 && ctx.major < 19;

    // General.
    let mut general = FormSection::new("general", "General");
    if legacy_new {
        general = general.field(
            FormField::new(
                "job_api",
                "Crear con",
                FormKind::select(&[
                    ("dbms_scheduler", "DBMS_SCHEDULER"),
                    ("dbms_job", "DBMS_JOB (antes de 19c)"),
                ]),
            )
            .help("Con DBMS_JOB solo cuentan «Qué ejecuta», «Próxima ejecución», «Intervalo», «Instancia» y «Habilitado».")
            .default(json!("dbms_scheduler")),
        );
        values.insert("job_api".into(), json!("dbms_scheduler"));
        general = general.fields(legacy::create_fields());
    }
    // Before 19c a new job may be a DBMS_JOB one: these fields (and the
    // tabs after General) are DBMS_SCHEDULER's.
    let sched_only = |f: FormField| {
        if legacy_new {
            f.when("job_api", &["dbms_scheduler"])
        } else {
            f
        }
    };
    let sched_tab = |s: FormSection| {
        if legacy_new {
            s.when("job_api", &["dbms_scheduler"])
        } else {
            s
        }
    };
    general = general.fields(
        vec![
        FormField::new("owner", "Esquema", FormKind::reference("schema", false))
            .read_only(existing || !ctx.create_any(Kind::Job))
            .default(json!(ctx.user())),
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .read_only(existing)
            .help("Sin comillas se guarda en mayúsculas, como lo hace Oracle."),
        FormField::new(
            "job_style",
            "Estilo",
            only(
                FormKind::select(&[
                    ("REGULAR", "Normal"),
                    ("LIGHTWEIGHT", "Liviano"),
                    ("IN_MEMORY_RUNTIME", "En memoria (en ejecución)"),
                    ("IN_MEMORY_FULL", "En memoria (completo)"),
                ]),
                |s| !s.starts_with("IN_MEMORY") || ctx.max_name() > 30,
            ),
        )
        .help("Los jobs livianos y en memoria ejecutan un programa y guardan menos en el diccionario.")
        .read_only(existing)
        .default(json!("REGULAR")),
        FormField::new("job_class", "Clase", FormKind::reference("job_class", false))
            .default(json!(DEFAULT_CLASS)),
        FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
        FormField::new("auto_drop", "Borrarlo al terminar", FormKind::Bool)
            .help("Oracle borra el job cuando ya no le quedan ejecuciones (se venció su fecha de fin o su máximo de ejecuciones).")
            .default(json!(false)),
        FormField::new("comments", "Descripción", FormKind::Textarea),
    ]
        .into_iter()
        // "Habilitado" is both's.
        .map(|f| if f.key == "enabled" { f } else { sched_only(f) })
        .collect(),
    );
    if let Some(st) = &state {
        let r = &st.rec;
        general = general.fields(vec![
            FormField::new("state", "Estado", FormKind::Text).read_only(true),
            FormField::new("job_creator", "Creado por", FormKind::Text).read_only(true),
            FormField::new("run_count", "Ejecuciones", FormKind::Text).read_only(true),
            FormField::new("failure_count", "Fallas", FormKind::Text).read_only(true),
            FormField::new("retry_count", "Reintentos", FormKind::Text).read_only(true),
            FormField::new("last_start_date", "Última ejecución", FormKind::Text).read_only(true),
            FormField::new("next_run_date", "Próxima ejecución", FormKind::Text).read_only(true),
        ]);
        let ts = |col: &str| {
            timing::ts_of(r, col)
                .map(|t| format!("{} {}", t.local.replace('T', " "), t.tz))
                .unwrap_or_default()
        };
        for (k, col) in [
            ("state", "STATE"),
            ("job_creator", "JOB_CREATOR"),
            ("run_count", "RUN_COUNT"),
            ("failure_count", "FAILURE_COUNT"),
            ("retry_count", "RETRY_COUNT"),
        ] {
            values.insert(k.into(), json!(r.t(col).unwrap_or_default()));
        }
        values.insert("last_start_date".into(), json!(ts("LAST_START_DATE")));
        values.insert("next_run_date".into(), json!(ts("NEXT_RUN_DATE")));
        values.insert(
            "dbine_state".into(),
            json!(fingerprint(&values_of(&st.spec).0, &lists)),
        );
    }

    // What it runs.
    let mut action = FormSection::new("action", "Acción").field(
        FormField::new("job_type", "Tipo", type_select(ctx.cloud))
            .required()
            .default(json!("PLSQL_BLOCK")),
    );
    action = action.fields(action_fields("job_type"));
    action = action.fields(vec![
        FormField::new("chain", "Cadena", FormKind::reference("chain", false))
            .when("job_type", &["CHAIN"]),
        FormField::new("program", "Programa", FormKind::reference("program", false))
            .when("job_type", &["PROGRAM"]),
    ]);
    action = action.list(
        FormList::new(
            "arguments",
            "Argumentos",
            "value",
            vec![
                FormField::new("name", "Nombre", FormKind::Text)
                    .read_only(true)
                    .help("El del programa, si lo define."),
                FormField::new("value", "Valor", FormKind::Text)
                    .help("Vacío: el valor predeterminado del programa (o ninguno)."),
            ],
        )
        // Not orderable: a row's place is its argument's position, the
        // one the program or procedure gives it.
        .rows(lists.get("arguments").cloned().unwrap_or_default())
        .when("job_type", &ARG_TYPES),
    );

    // When.
    let modes: Vec<&str> = MODES
        .iter()
        .copied()
        .filter(|m| *m != "file_watcher" || ctx.has(Kind::FileWatcher))
        .collect();
    let mut schedule = FormSection::new("schedule", "Programación")
        .fields(timing::fields(&modes, &ctx.default_tz()));
    if has("STOP_ON_WINDOW_CLOSE") {
        schedule = schedule.field(
            FormField::new(
                "stop_on_window_close",
                "Detener al cerrar la ventana",
                FormKind::Bool,
            )
            .when("schedule_mode", &["window", "window_group"]),
        );
    }
    if has("SCHEDULE_LIMIT") {
        schedule = schedule.field(
            FormField::new("schedule_limit", "Demora máxima", FormKind::Duration)
                .help("Si el job no pudo empezar dentro de este tiempo desde su hora programada, esa ejecución se saltea."),
        );
    }
    if !existing {
        values.insert("schedule_mode".into(), json!("immediate"));
        values.insert("time_zone".into(), json!(ctx.default_tz()));
    }

    // Options.
    let mut options = FormSection::new("options", "Opciones");
    if has("JOB_PRIORITY") {
        options = options.field(
            FormField::new(
                "job_priority",
                "Prioridad",
                FormKind::number(Some(1.0), Some(5.0)),
            )
            .help("Dentro de su clase: 1 es la más alta, 5 la más baja.")
            .default(json!(3)),
        );
    }
    if has("LOGGING_LEVEL") {
        options = options.field(
            FormField::new(
                "logging_level",
                "Nivel de registro",
                FormKind::select(&[
                    ("OFF", "El de su clase"),
                    ("RUNS", "Ejecuciones"),
                    ("FAILED_RUNS", "Ejecuciones con error"),
                    ("FULL", "Completo (también los cambios del job)"),
                ]),
            )
            .default(json!("OFF")),
        );
    }
    let bools: [(&str, &str, &str, Option<&str>); 7] = [
        (
            "restartable",
            "Reiniciable",
            "RESTARTABLE",
            Some("Si falla, Oracle lo reintenta (hasta 6 veces)."),
        ),
        (
            "restart_on_recovery",
            "Reiniciar al recuperarse la base",
            "RESTART_ON_RECOVERY",
            None,
        ),
        (
            "restart_on_failure",
            "Reiniciar si falla",
            "RESTART_ON_FAILURE",
            None,
        ),
        (
            "instance_stickiness",
            "Preferir la misma instancia",
            "INSTANCE_STICKINESS",
            None,
        ),
        ("store_output", "Guardar la salida", "STORE_OUTPUT", None),
        (
            "allow_runs_in_restricted_mode",
            "Ejecutar en modo restringido",
            "ALLOW_RUNS_IN_RESTRICTED_MODE",
            None,
        ),
        (
            "fail_on_script_error",
            "Fallar ante un error del script",
            "FAIL_ON_SCRIPT_ERROR",
            None,
        ),
    ];
    for (key, label, col, help) in bools {
        if !has(col) {
            continue;
        }
        let mut field = FormField::new(key, label, FormKind::Bool);
        if let Some(h) = help {
            field = field.help(h);
        }
        if key == "fail_on_script_error" {
            field = field.when("job_type", &SCRIPT_TYPES);
        }
        options = options.field(field);
    }
    if has("MAX_RUNS") {
        options = options.field(FormField::new(
            "max_runs",
            "Máximo de ejecuciones",
            FormKind::number(Some(1.0), None),
        ));
    }
    if has("MAX_FAILURES") {
        options = options.field(
            FormField::new(
                "max_failures",
                "Máximo de fallas",
                FormKind::number(Some(1.0), None),
            )
            .help(
                "Al llegar a este número de fallas seguidas, Oracle lo marca como roto (BROKEN).",
            ),
        );
    }
    if has("MAX_RUN_DURATION") {
        options = options.field(
            FormField::new("max_run_duration", "Duración máxima", FormKind::Duration)
                .help("Pasado este tiempo, el job publica el evento JOB_OVER_MAX_DUR (sigue ejecutándose)."),
        );
    }
    if has("INSTANCE_ID") {
        options = options.field(
            FormField::new(
                "instance_id",
                "Instancia",
                FormKind::number(Some(1.0), None),
            )
            .help("En RAC, la instancia donde se ejecuta. Vacío: cualquiera."),
        );
    }
    if has("RAISE_EVENTS") {
        options = options.field(
            FormField::new("raise_events", "Eventos que publica", events_multi())
                .help("Los publica en la cola de eventos del Scheduler (SYS.SCHEDULER$_EVENT_QUEUE). Con una duración máxima, Oracle publica además «Pasó su duración máxima»."),
        );
    }

    // Where and as whom.
    let mut dest =
        FormSection::new("destination", "Destino y credenciales").fields(vec![FormField::new(
            "credential",
            "Credencial",
            FormKind::reference("credential", false),
        )
        .help("El usuario del sistema operativo (o de la base remota) con el que se ejecuta.")]);
    if has("CONNECT_CREDENTIAL_NAME") {
        dest = dest.field(
            FormField::new(
                "connect_credential",
                "Credencial de conexión",
                FormKind::reference("credential", false),
            )
            .help("Para los scripts de SQL*Plus y de RMAN: el usuario de la base con el que se conectan.")
            .when("job_type", &["SQL_SCRIPT", "BACKUP_SCRIPT"]),
        );
    }
    if ctx.has(Kind::DbDestination) || ctx.has(Kind::ExternalDestination) {
        dest = dest.field(
            FormField::new("destination", "Destino", FormKind::reference("destination", false))
                .help("Un destino remoto o un grupo de destinos (el job se ejecuta en cada uno). Vacío: esta base."),
        );
    }

    let mut sections = vec![
        general,
        sched_tab(action),
        sched_tab(schedule),
        sched_tab(options),
        sched_tab(dest),
    ];
    if ctx.notifications() {
        sections.push(sched_tab(
            FormSection::new("notifications", "Notificaciones").list(
                FormList::new(
                    "notifications",
                    "Notificaciones por correo",
                    "recipients",
                    vec![
                        FormField::new("recipients", "Destinatarios", FormKind::Text)
                            .required()
                            .placeholder("dba@empresa.com, guardia@empresa.com"),
                        FormField::new("events", "Eventos", events_multi())
                            .required()
                            .default(json!(["JOB_FAILED", "JOB_BROKEN", "JOB_SCH_LIM_REACHED", "JOB_CHAIN_STALLED", "JOB_OVER_MAX_DUR"])),
                        FormField::new("sender", "Remitente", FormKind::Text)
                            .help("Vacío: el remitente del Scheduler (email_sender)."),
                        FormField::new("subject", "Asunto", FormKind::Text)
                            .placeholder("%job_owner%.%job_name%: %event_type%")
                            .help("Puede llevar variables como %job_name%, %event_type%, %error_message%. Vacío: el de Oracle."),
                        FormField::new("body", "Mensaje", FormKind::Textarea)
                            .help("Vacío: el mensaje de Oracle, con los datos del evento."),
                        FormField::new("filter_condition", "Filtro", FormKind::Text)
                            .placeholder(":event.error_code = 600")
                            .help("Una condición sobre el evento (:event); solo se avisa cuando se cumple."),
                    ],
                )
                .rows(lists.get("notifications").cloned().unwrap_or_default())
                .read_only(!ctx.email()),
            ),
        ));
        if !ctx.email() {
            f.notes.push(NO_EMAIL.into());
        }
    }
    f.sections = sections;
    f.values = values;
    f.choices = cat.lists;
    if !ctx.cloud {
        f.notes.push(EXTERNAL_NOTE.into());
    }
    if let Some(st) = &state {
        if st.spec.enabled {
            f.notes.push(ENABLED_EDIT.into());
        }
        if let Some(why) = ctx.locked(Kind::Job, &st.spec.owner, &st.spec.name, st.system) {
            f.read_only = true;
            f.notes.insert(0, why.into());
        }
    } else if !ctx.may_create(Kind::Job) {
        f.read_only = true;
        f.notes.insert(0, super::NO_CREATE_JOB.into());
    }
    Ok(f)
}

fn events_multi() -> FormKind {
    FormKind::multi(&[
        ("JOB_STARTED", "Empezó"),
        ("JOB_SUCCEEDED", "Terminó bien"),
        ("JOB_FAILED", "Falló"),
        ("JOB_BROKEN", "Quedó roto"),
        ("JOB_COMPLETED", "Completó sus ejecuciones"),
        ("JOB_STOPPED", "Se detuvo"),
        ("JOB_SCH_LIM_REACHED", "Pasó su demora máxima"),
        ("JOB_DISABLED", "Se deshabilitó"),
        ("JOB_CHAIN_STALLED", "Su cadena se trabó"),
        ("JOB_OVER_MAX_DUR", "Pasó su duración máxima"),
    ])
}

// ---------------------------------------------------------------------------
// Start at a step, clear the history
// ---------------------------------------------------------------------------

/// "Iniciar en el paso…": a chain job's chain run from that step
/// (`RUN_CHAIN`, a run-once job of its own); any other job has one step,
/// itself, and runs now.
pub(crate) fn run_from(c: &Connection, id: &str, step: &str) -> Result<()> {
    if legacy_id(id).is_some() {
        return jobs::run(c, id);
    }
    within(c, jobs::CALL_LIMIT, || {
        let ctx = Ctx::read(c)?;
        let (owner, name) = parse_obj(id, "job")?;
        let st = read_state(c, &ctx, &owner, &name)?;
        // As the jobs list runs it: Oracle's own jobs too, where the login
        // manages them.
        if !ctx.may_change(Kind::Job, &owner, &name, st.system) {
            return Err(bad(OWNER_ONLY.into()));
        }
        let Action::Chain(chain) = &st.spec.action else {
            return jobs::run(c, id);
        };
        let (co, cn) = parse_obj(chain, "cadena")?;
        let steps = read::rows(
            c,
            &ctx.sql("{v}_scheduler_chain_steps"),
            "WHERE owner = :1 AND chain_name = :2",
            &[&co, &cn],
            1000,
        )
        .map_err(crate::err)?;
        let found = steps
            .iter()
            .filter_map(|r| r.raw("STEP_NAME"))
            .find(|s| s == step)
            .or_else(|| {
                steps
                    .iter()
                    .filter_map(|r| r.raw("STEP_NAME"))
                    .find(|s| s.eq_ignore_ascii_case(step.trim()))
            })
            .ok_or_else(|| {
                bad(format!(
                    "La cadena del job no tiene un paso llamado «{}».",
                    shown(step)
                ))
            })?;
        let start = if jobs::plain(&found) {
            found
        } else {
            crate::quote(&found)
        };
        super::no_open_transaction(c)?;
        c.execute(
            "BEGIN DBMS_SCHEDULER.RUN_CHAIN(chain_name => :1, start_steps => :2, job_name => NULL); END;",
            &[&format!("{}.{}", crate::quote(&co), crate::quote(&cn)), &start],
        )
        .map(|_| ())
        .map_err(|e| match crate::db_code(&e) {
            Some(27366) => Error::Query(
                "El Scheduler no llegó a empezar la cadena: no hubo un proceso de jobs libre a tiempo (job_queue_processes)."
                    .into(),
            ),
            _ => Error::Query(super::script::message(&e)),
        })
    })
}

const NO_PURGE: &str =
    "Este usuario no puede borrar el historial del job: hace falta el privilegio MANAGE SCHEDULER.";

/// `PURGE_LOG` of the job: its log and run details. Oracle asks for
/// MANAGE SCHEDULER, whoever owns the job.
pub(crate) fn clear_history(c: &Connection, id: &str) -> Result<()> {
    if legacy_id(id).is_some() {
        return Err(Error::Unsupported(
            "los jobs de DBMS_JOB no guardan el historial de sus ejecuciones".into(),
        ));
    }
    within(c, jobs::CALL_LIMIT, || {
        let ctx = Ctx::read(c)?;
        let (owner, name) = parse_obj(id, "job")?;
        // That it exists, and that the login sees it.
        read_state(c, &ctx, &owner, &name)?;
        if !ctx.manage() {
            return Err(bad(NO_PURGE.into()));
        }
        super::no_open_transaction(c)?;
        c.execute(
            "BEGIN DBMS_SCHEDULER.PURGE_LOG(log_history => 0, which_log => 'JOB_LOG', job_name => :1); END;",
            &[&format!("{}.{}", crate::quote(&owner), crate::quote(&name))],
        )
        .map(|_| ())
        .map_err(|e| match crate::db_code(&e) {
            Some(27486 | 1031) => Error::Query(NO_PURGE.into()),
            _ => Error::Query(super::script::message(&e)),
        })
    })
}

// ---------------------------------------------------------------------------
// DBMS_JOB before 19c
// ---------------------------------------------------------------------------

pub(crate) mod legacy {
    use super::*;

    #[derive(Debug, Clone, PartialEq)]
    pub(crate) struct LegacySpec {
        pub(crate) what: String,
        /// Server clock, `YYYY-MM-DDTHH:MM:SS`.
        pub(crate) next: Option<String>,
        pub(crate) interval: Option<String>,
        pub(crate) enabled: bool,
        pub(crate) instance: i64,
    }

    /// `DBMS_JOB` keeps a broken job's next date at 4000-01-01.
    fn far(next: &Option<String>) -> bool {
        next.as_deref().is_some_and(|n| n.starts_with("4000"))
    }

    pub(crate) fn spec_of_row(r: &jobs::LegacyRow) -> LegacySpec {
        LegacySpec {
            what: r.what.clone().unwrap_or_default().trim_end().to_string(),
            next: r.next_local.as_deref().and_then(local_time),
            interval: r
                .interval
                .clone()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty() && s != "null"),
            enabled: !r.broken,
            instance: jobs::integer(&r.instance).unwrap_or(0),
        }
    }

    pub(crate) fn values_of(s: &LegacySpec) -> BTreeMap<String, Value> {
        BTreeMap::from([
            ("what".to_string(), json!(s.what)),
            (
                "next_date".to_string(),
                json!(s.next.clone().unwrap_or_default()),
            ),
            (
                "interval".to_string(),
                json!(s.interval.clone().unwrap_or_default()),
            ),
            ("enabled".to_string(), json!(s.enabled)),
            ("instance".to_string(), json!(s.instance)),
        ])
    }

    pub(crate) fn spec_of(v: Vals) -> Result<LegacySpec> {
        let what = v.code("what").unwrap_or_default();
        if what.trim().is_empty() {
            return Err(bad("Falta «Qué ejecuta».".into()));
        }
        if !what.trim_end().ends_with(';') {
            return Err(bad(
                "«Qué ejecuta» tiene que terminar en «;» (DBMS_JOB lo ejecuta entre BEGIN y END)."
                    .into(),
            ));
        }
        super::super::fits(&Some(what.clone()), "Qué ejecuta", 4000)?;
        let next = match v.opt("next_date") {
            None => None,
            Some(t) => Some(local_time(&t).ok_or_else(|| {
                bad(format!(
                    "«Próxima ejecución»: «{}» no es una fecha y hora válida.",
                    shown(&t)
                ))
            })?),
        };
        let interval = v.opt("interval");
        super::super::fits(&interval, "Intervalo", 200)?;
        Ok(LegacySpec {
            what,
            next,
            interval,
            enabled: v.bool_or("enabled", true),
            instance: v.int("instance", "Instancia", 0, 9999)?.unwrap_or(0),
        })
    }

    fn date(next: &Option<String>) -> A {
        match next {
            Some(n) if !far(next) => A::R(format!(
                "TO_DATE({}, 'YYYY-MM-DD HH24:MI:SS')",
                jobs::lit(&n.replace('T', " "))
            )),
            _ => A::R("SYSDATE".into()),
        }
    }

    /// The interval as DBMS_JOB takes it: none is the text `'null'` (its
    /// default; `DBA_JOBS.INTERVAL` can't be NULL, so SQL NULL fails before
    /// 19c), a job that runs once.
    fn interval(i: &Option<String>) -> A {
        A::S(i.clone().unwrap_or_else(|| "null".into()))
    }

    pub(crate) fn create_block(s: &LegacySpec, force: bool) -> Block {
        let mut b = Block::new("Crea un job de DBMS_JOB en el esquema de quien ejecuta el script.");
        b.declare("j BINARY_INTEGER;");
        b.rollback_on_error();
        b.call(
            "DBMS_JOB.SUBMIT",
            vec![
                ("job", A::R("j".into())),
                ("what", A::S(s.what.clone())),
                ("next_date", date(&s.next)),
                ("interval", interval(&s.interval)),
                ("no_parse", A::B(false)),
                ("instance", A::I(s.instance)),
                ("force", A::B(force)),
            ],
        );
        if !s.enabled {
            b.call(
                "DBMS_JOB.BROKEN",
                vec![("job", A::R("j".into())), ("broken", A::B(true))],
            );
        }
        b.line("COMMIT;");
        b.returns_job();
        b
    }

    /// `seen`: the next date the form was read with (a run moves it on;
    /// only a date the user changed is written).
    pub(crate) fn alter_block(
        n: i64,
        old: &LegacySpec,
        new: &LegacySpec,
        seen: Option<&str>,
        force: bool,
    ) -> Block {
        let mut b = Block::new(&format!("Cambia el job {n} de DBMS_JOB."));
        b.rollback_on_error();
        let job = || ("job", A::I(n));
        if old.what != new.what {
            b.call(
                "DBMS_JOB.WHAT",
                vec![job(), ("what", A::S(new.what.clone()))],
            );
        }
        let next_changed = new.next.as_deref() != seen && new.next != old.next;
        if old.interval != new.interval {
            b.call(
                "DBMS_JOB.INTERVAL",
                vec![job(), ("interval", interval(&new.interval))],
            );
        }
        if old.instance != new.instance {
            b.call(
                "DBMS_JOB.INSTANCE",
                vec![
                    job(),
                    ("instance", A::I(new.instance)),
                    ("force", A::B(force)),
                ],
            );
        }
        if old.enabled != new.enabled {
            let mut args = vec![job(), ("broken", A::B(!new.enabled))];
            if new.enabled {
                // Fixed again: due at the date given, else at the one it
                // had if still ahead, else now (as DBMS_JOB.BROKEN does).
                let next = if next_changed {
                    date(&new.next)
                } else {
                    match &old.next {
                        Some(n) if !far(&old.next) => A::R(format!(
                            "GREATEST(TO_DATE({}, 'YYYY-MM-DD HH24:MI:SS'), SYSDATE)",
                            jobs::lit(&n.replace('T', " "))
                        )),
                        _ => A::R("SYSDATE".into()),
                    }
                };
                args.push(("next_date", next));
            }
            b.call("DBMS_JOB.BROKEN", args);
        } else if next_changed {
            b.call(
                "DBMS_JOB.NEXT_DATE",
                vec![job(), ("next_date", date(&new.next))],
            );
        }
        if b.calls() > 0 {
            b.line("COMMIT;");
        }
        b
    }

    pub(crate) fn create_fields() -> Vec<FormField> {
        let only_legacy = ["dbms_job"];
        vec![
            FormField::new("what", "Qué ejecuta", FormKind::code("plsql"))
                .placeholder("mi_paquete.procesar;")
                .help("Una o más llamadas PL/SQL, cada una terminada en «;».")
                .when("job_api", &only_legacy),
            FormField::new("next_date", "Próxima ejecución", FormKind::DateTime)
                .help("En la hora del servidor. Vacía: ahora.")
                .when("job_api", &only_legacy),
            FormField::new("interval", "Intervalo", FormKind::Text)
                .placeholder("SYSDATE + 1")
                .help("Una expresión de fecha que da la ejecución siguiente. Vacío: se ejecuta una sola vez.")
                .when("job_api", &only_legacy),
            FormField::new("instance", "Instancia", FormKind::number(Some(0.0), None))
                .help("En RAC, la instancia que lo ejecuta; 0: cualquiera.")
                .when("job_api", &only_legacy),
            FormField::new("force", "Forzar la instancia", FormKind::Bool)
                .help("Acepta una instancia que no está levantada.")
                .when("job_api", &only_legacy),
        ]
    }

    fn may_change(ctx: &Ctx, r: &jobs::LegacyRow) -> bool {
        r.log_user.as_deref() == Some(ctx.user())
    }

    pub(crate) fn form(c: &Connection, ctx: &Ctx, n: i64) -> Result<ObjectForm> {
        let row = jobs::legacy_row(c, ctx.v(), n)?;
        let spec = spec_of_row(&row);
        let mut values = values_of(&spec);
        let fp = fingerprint(&values, &BTreeMap::new());
        values.insert("dbine_state".into(), json!(fp));
        values.insert(
            "next_date_seen".into(),
            json!(spec.next.clone().unwrap_or_default()),
        );
        values.insert("force".into(), json!(false));
        for (k, v) in [
            ("job", Some(n.to_string())),
            ("log_user", row.log_user.clone()),
            ("schema_user", row.schema_user.clone()),
            ("last_date", row.last.clone()),
            ("failures", row.failures.clone()),
        ] {
            values.insert(k.into(), json!(v.unwrap_or_default()));
        }
        let general = FormSection::new("general", "General").fields(vec![
            FormField::new("job", "Número", FormKind::Text).read_only(true),
            FormField::new("what", "Qué ejecuta", FormKind::code("plsql")).required(),
            FormField::new("next_date", "Próxima ejecución", FormKind::DateTime)
                .help("En la hora del servidor."),
            FormField::new("interval", "Intervalo", FormKind::Text)
                .placeholder("SYSDATE + 1")
                .help("Una expresión de fecha que da la ejecución siguiente. Vacío: se ejecuta una sola vez."),
            FormField::new("enabled", "Habilitado", FormKind::Bool)
                .help("Deshabilitado es «roto» (BROKEN) para DBMS_JOB: no se ejecuta."),
            FormField::new("instance", "Instancia", FormKind::number(Some(0.0), None))
                .help("En RAC, la instancia que lo ejecuta; 0: cualquiera."),
            FormField::new("force", "Forzar la instancia", FormKind::Bool),
            FormField::new("log_user", "Usuario", FormKind::Text).read_only(true),
            FormField::new("schema_user", "Esquema", FormKind::Text).read_only(true),
            FormField::new("last_date", "Última ejecución correcta", FormKind::Text).read_only(true),
            FormField::new("failures", "Fallas", FormKind::Text).read_only(true),
        ]);
        let mut f = ObjectForm {
            sections: vec![general],
            values,
            ..ObjectForm::default()
        };
        if !may_change(ctx, &row) {
            f.read_only = true;
            f.notes.push(LEGACY_OWN.into());
        }
        Ok(f)
    }

    pub(crate) fn plan(
        c: &Connection,
        ctx: &Ctx,
        n: Option<i64>,
        values: &FormValues,
    ) -> Result<Plan> {
        let v = Vals(&values.values);
        let new = spec_of(v)?;
        let force = v.bool("force");
        match n {
            None => {
                if !ctx.can("CREATE JOB") {
                    return Err(bad(super::super::NO_CREATE_JOB.into()));
                }
                Ok(Plan {
                    block: Some(create_block(&new, force)),
                    id: None,
                    name: "DBMS_JOB".into(),
                    legacy_submit: true,
                })
            }
            Some(n) => {
                let row = jobs::legacy_row(c, ctx.v(), n)?;
                if !may_change(ctx, &row) {
                    return Err(bad(LEGACY_OWN.into()));
                }
                let old = spec_of_row(&row);
                // The next date moves on with every run: the form is
                // checked against what it was read with, at the date it saw.
                let mut read_with = values_of(&old);
                read_with.insert("next_date".into(), json!(v.text("next_date_seen")));
                fresh(values, &fingerprint(&read_with, &BTreeMap::new()))?;
                let seen = v.opt("next_date_seen").and_then(|t| local_time(&t));
                let block = alter_block(n, &old, &new, seen.as_deref(), force);
                Ok(Plan::changes(
                    block,
                    format!("{}{n}", jobs::LEGACY_ID),
                    &format!("Job {n}"),
                ))
            }
        }
    }

    pub(crate) fn delete_block(c: &Connection, ctx: &Ctx, n: i64) -> Result<Block> {
        let row = jobs::legacy_row(c, ctx.v(), n)?;
        if !may_change(ctx, &row) {
            return Err(bad(LEGACY_OWN.into()));
        }
        let mut b = Block::new(&format!("Borra el job {n} de DBMS_JOB."));
        b.rollback_on_error();
        b.call("DBMS_JOB.REMOVE", vec![("job", A::I(n))]);
        b.line("COMMIT;");
        Ok(b)
    }
}
