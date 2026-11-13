//! MySQL's, MariaDB's and OceanBase's Event Scheduler in the designer: one
//! kind, the event (key "job", the ids `jobs()` gives), with its database,
//! name, definer, status, comment, schedule (`AT` a time, or `EVERY` an
//! interval `STARTS … ENDS …`, read in the event's zone), what happens when
//! it ends, and its statement under its SQL mode.
//!
//! - Create: `CREATE DEFINER = … EVENT …` under the SQL mode and zone the
//!   form names (the event keeps both: the zone with its schedule, the mode
//!   with its statement), the session's put back after it.
//! - Edit: one `ALTER DEFINER = … EVENT …` with only the clauses that
//!   changed. The definer always goes (a plain `ALTER EVENT` makes whoever
//!   runs it the definer); the schedule goes whole (an `EVERY` without
//!   `STARTS` would start now, and drop `ENDS`); a new database or name is
//!   `RENAME TO`. Where `ALTER EVENT` keeps an `ENDS` left out (MariaDB,
//!   MySQL 5.7, OceanBase) a first `ALTER` clears it, and a failed second
//!   one puts the old schedule back.
//! - The statement: one, or one `BEGIN … END` block (a `;` outside it
//!   would end the `CREATE` there and run the rest at once).
//! - Delete: `DROP EVENT`.
//! - No actions: the server runs an event only on its schedule, keeps no
//!   history of its runs, and an event has no steps.
//! - MySQL 8.0.16+: an event whose definer holds SYSTEM_USER can't be
//!   changed nor dropped by a login without it (not even taking it over as
//!   definer): the list marks it so, the form is read-only, and such an
//!   account isn't offered as definer. Any other event of someone else's
//!   is changed by naming the login as definer (or keeping its own with
//!   SET_USER_ID / SET_ANY_DEFINER) and dropped with the EVENT privilege.
//! - OceanBase creates and changes events only in the current database
//!   (the script starts with `USE`), doesn't move one to another database,
//!   and reads every time on the tenant's global zone.
//! - Log: MySQL 8.0.22+ keeps the scheduler's errors in its error log,
//!   readable as `performance_schema.error_log` (`Event Scheduler:
//!   [definer][db.event] …`).

use super::{
    bad, databases, datetime, form_time, name, no_action, no_kind, one_line, pick, shown, Plan,
    Vals,
};
use crate::jobs::clock::Civil;
use crate::jobs::events::{self as ev, Event};
use crate::jobs::{quoted, text};
use crate::session::{at, lit, MySqlSession};
use crate::Variant;
use dbine_driver::{
    Error, FormField, FormKind, FormSection, FormValues, MonitorTable, ObjectForm, Result,
    SchedulerInfo, SchedulerItem, SchedulerKind, SchedulerList,
};
use serde_json::json;

const NO_EVENT_PRIVILEGE: &str = "Este usuario no tiene el privilegio EVENT en ninguna base: puede ver los eventos, pero no crearlos, cambiarlos ni borrarlos.";
const FOREIGN_DEFINER: &str = "El definidor de este evento es otro usuario y este no tiene SET_USER_ID (ni SUPER): para guardarlo, elegí tu usuario como definidor (el evento pasará a correr con tus privilegios).";
const OB_NOTE: &str = "OceanBase lee las horas de los eventos en la zona horaria global del tenant y no mueve un evento a otra base.";
const CHANGED: &str = "El evento cambió en el servidor desde que se abrió el formulario: volvé a abrirlo antes de guardar.";
const NO_LOG: &str = "Este servidor no deja leer el registro del programador de eventos: está en performance_schema.error_log desde MySQL 8.0.22, con el privilegio SELECT sobre esa tabla.";
const ONE_STATEMENT: &str = "La instrucción del evento tiene que ser una sola: con un «;» fuera de un bloque BEGIN … END, el servidor guardaría en el evento solo lo anterior y ejecutaría el resto al guardar. Para varias, encerralas en BEGIN … END (y una columna llamada begin o end, entre comillas invertidas).";
const SYSTEM_DEFINER: &str = "El definidor de este evento tiene el privilegio SYSTEM_USER y este usuario no: el servidor no le deja cambiar ni borrar el evento.";
const SYSTEM_DEFINERS: &str = "Los eventos cuyo definidor tiene el privilegio SYSTEM_USER solo los cambia o los borra un usuario que también lo tenga.";
const SYSTEM_DEFINER_NEW: &str = "Solo un usuario con el privilegio SYSTEM_USER puede poner como definidor a una cuenta que lo tiene.";
const NO_RUN: &str = "el servidor no ejecuta un evento a pedido: solo según su programación";

/// The interval units `EVERY` takes, in the order the form offers them.
pub(super) const UNITS: [&str; 15] = [
    "SECOND",
    "MINUTE",
    "HOUR",
    "DAY",
    "WEEK",
    "MONTH",
    "QUARTER",
    "YEAR",
    "MINUTE_SECOND",
    "HOUR_MINUTE",
    "HOUR_SECOND",
    "DAY_HOUR",
    "DAY_MINUTE",
    "DAY_SECOND",
    "YEAR_MONTH",
];

// ---------------------------------------------------------------------------
// What the server and the login allow
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub(super) struct Ctx {
    /// `@@global.event_scheduler`.
    pub(super) scheduler: String,
    /// `CURRENT_USER()` (`user@host`, as `DEFINER` reads).
    pub(super) user: String,
    pub(super) version: String,
    /// The user's global privileges, upper case.
    pub(super) privileges: Vec<String>,
    /// The user's `SHOW GRANTS` lines (its current role's too).
    pub(super) grants: Vec<String>,
    pub(super) session_zone: String,
    pub(super) session_mode: String,
    /// OceanBase: the tenant's global zone, where event times are read.
    pub(super) ob_zone: Option<String>,
    /// MySQL itself (not MariaDB nor OceanBase).
    pub(super) mysql: bool,
    /// OceanBase.
    pub(super) ob: bool,
    /// `performance_schema.error_log` is readable.
    pub(super) error_log: bool,
    /// MySQL 8.0.16+ and a login without SYSTEM_USER: the accounts that hold
    /// it (`user@host`, from `mysql.global_grants`), `None` when they can't
    /// be read.
    pub(super) system_users: Option<Vec<String>>,
}

impl Ctx {
    fn has(&self, any: &[&str]) -> bool {
        self.privileges
            .iter()
            .any(|p| p == "ALL" || p == "ALL PRIVILEGES" || any.contains(&p.as_str()))
    }

    /// MySQL 8.0.16+ and the login lacks SYSTEM_USER: the server refuses it
    /// any change to, and the drop of, an object whose definer holds it.
    pub(super) fn guards_system_users(&self) -> bool {
        self.mysql && version_at_least(&self.version, 8, 0, 16) && !self.has(&["SYSTEM_USER"])
    }

    /// The server refuses this login any change to an event of `definer`'s
    /// (and naming `definer`): the definer holds SYSTEM_USER and the login
    /// doesn't. When the accounts that hold it can't be read, root's and
    /// the server's own `mysql.*` accounts, which hold it out of the box.
    /// Never the login's own account: it doesn't hold SYSTEM_USER.
    pub(super) fn protected(&self, definer: &str) -> bool {
        if !self.guards_system_users() || definer.is_empty() {
            return false;
        }
        if same_account(definer, &self.user) {
            return false;
        }
        match &self.system_users {
            Some(accounts) => accounts.iter().any(|a| same_account(a, definer)),
            None => {
                let user = definer.rsplit_once('@').map_or(definer, |(u, _)| u);
                user == "root" || user.starts_with("mysql.")
            }
        }
    }

    /// `DEFINER = <someone else>` is allowed.
    pub(super) fn sets_any_definer(&self) -> bool {
        self.has(&["SUPER", "SET_USER_ID", "SET_ANY_DEFINER", "SET USER"])
    }

    /// The EVENT privilege on some database (or all).
    pub(super) fn may_write(&self) -> bool {
        self.has(&["EVENT"]) || grants_event(&self.grants)
    }

    pub(super) fn flavor(&self) -> Flavor {
        Flavor {
            ob: self.ob,
            replica: self.mysql && version_at_least(&self.version, 8, 2, 0),
            resets_schedule: self.mysql && version_at_least(&self.version, 8, 0, 0),
            any_definer: self.sets_any_definer(),
        }
    }
}

/// Whether `version` (`8.0.36`, `8.4.11-log`, `11.8.2-MariaDB`) is at
/// least `major.minor.patch`.
pub(super) fn version_at_least(version: &str, major: u64, minor: u64, patch: u64) -> bool {
    let nums: Vec<u64> = version
        .split(|c: char| !c.is_ascii_digit())
        .take(3)
        .map(|p| p.parse().unwrap_or(0))
        .collect();
    let v = (
        nums.first().copied().unwrap_or(0),
        nums.get(1).copied().unwrap_or(0),
        nums.get(2).copied().unwrap_or(0),
    );
    v >= (major, minor, patch)
}

/// Two `user@host` accounts are one: the user as written, the host in any
/// case.
pub(super) fn same_account(a: &str, b: &str) -> bool {
    match (a.rsplit_once('@'), b.rsplit_once('@')) {
        (Some((ua, ha)), Some((ub, hb))) => ua == ub && ha.eq_ignore_ascii_case(hb),
        _ => a == b,
    }
}

/// An `EVENT` (or `ALL`) privilege among `SHOW GRANTS` lines, on any level
/// (`GRANT EVENT ON `db`.* TO …`).
pub(super) fn grants_event(lines: &[String]) -> bool {
    lines.iter().any(|g| {
        let g = g.trim();
        let Some(rest) = g
            .get(..6)
            .filter(|p| p.eq_ignore_ascii_case("GRANT "))
            .map(|_| &g[6..])
        else {
            return false;
        };
        let Some(i) = rest.to_ascii_uppercase().find(" ON ") else {
            return false;
        };
        rest[..i].split(',').any(|p| {
            let p = p.trim().to_ascii_uppercase();
            p == "EVENT" || p.starts_with("ALL")
        })
    })
}

async fn grant_lines(s: &mut MySqlSession) -> Vec<String> {
    let mut lines: Vec<String> = s
        .optional_rows("SHOW GRANTS")
        .await
        .iter()
        .filter_map(|r| at(r, 0))
        .collect();
    if s.variant == Variant::MariaDb {
        let role = s
            .optional_rows("SELECT CURRENT_ROLE()")
            .await
            .first()
            .and_then(|r| at(r, 0));
        if role.is_some_and(|r| !r.is_empty()) {
            lines.extend(
                s.optional_rows("SHOW GRANTS FOR CURRENT_ROLE")
                    .await
                    .iter()
                    .filter_map(|r| at(r, 0)),
            );
        }
    }
    lines
}

pub(super) async fn ctx(s: &mut MySqlSession) -> Result<Ctx> {
    let rows = s
        .rows("SELECT @@global.event_scheduler AS ev_scheduler, CURRENT_USER() AS ev_user, VERSION() AS ev_version, @@session.time_zone AS ev_tz, @@session.sql_mode AS ev_mode")
        .await?;
    let r = rows.first();
    let get = |k: &str| r.and_then(|r| text(r, &[k])).unwrap_or_default();
    let mut c = Ctx {
        scheduler: get("ev_scheduler"),
        user: get("ev_user"),
        version: get("ev_version"),
        session_zone: Some(get("ev_tz"))
            .filter(|z| !z.is_empty())
            .unwrap_or_else(|| "SYSTEM".into()),
        // An empty mode is a mode: `text` reads it as missing.
        session_mode: r
            .and_then(|r| crate::session::named(r, &["ev_mode"]))
            .unwrap_or_default(),
        mysql: s.variant == Variant::MySql,
        ob: s.variant == Variant::OceanBase,
        ..Ctx::default()
    };
    c.grants = grant_lines(s).await;
    c.privileges = {
        let mut p: Vec<String> = c
            .grants
            .iter()
            .flat_map(|g| ev::global_privileges(g))
            .collect();
        p.sort();
        p.dedup();
        p
    };
    if s.variant == Variant::OceanBase {
        c.ob_zone = s
            .optional_rows(&format!("SELECT {}", ev::ob_zone("global")))
            .await
            .first()
            .and_then(|r| at(r, 0));
    }
    if s.variant == Variant::MySql && version_at_least(&c.version, 8, 0, 22) {
        c.error_log = s
            .rows("SELECT 1 FROM performance_schema.error_log LIMIT 1")
            .await
            .is_ok();
    }
    if c.guards_system_users() {
        c.system_users = s
            .rows("SELECT USER AS ev_user, HOST AS ev_host FROM mysql.global_grants WHERE PRIV = 'SYSTEM_USER'")
            .await
            .ok()
            .map(|rows| {
                rows.iter()
                    .filter_map(|r| {
                        let user = crate::session::named(r, &["ev_user"])?;
                        let host = crate::session::named(r, &["ev_host"]).unwrap_or_default();
                        Some(format!("{user}@{host}"))
                    })
                    .collect()
            });
    }
    Ok(c)
}

pub(super) async fn info(s: &mut MySqlSession) -> Result<SchedulerInfo> {
    let c = ctx(s).await?;
    let w = c.may_write();
    let mut notes = Vec::new();
    if let Some(n) = ev::scheduler_note(&c.scheduler) {
        notes.push(n.to_string());
    }
    if !w {
        notes.push(NO_EVENT_PRIVILEGE.to_string());
    }
    if c.ob_zone.is_some() {
        notes.push(OB_NOTE.to_string());
    }
    Ok(SchedulerInfo {
        // No actions: the server runs an event only on its schedule. An
        // event is one statement with no steps, and the server keeps no
        // history of its runs.
        kinds: vec![SchedulerKind::new("job", "Eventos", "evento")
            .abilities(w, w, w)
            .run_from_step(false)
            .clear_history(false)],
        logs: c.error_log,
        notes,
    })
}

fn check_kind(kind: &str) -> Result<()> {
    if kind.trim() == "job" {
        Ok(())
    } else {
        Err(no_kind(kind))
    }
}

pub(super) async fn items(s: &mut MySqlSession, kind: &str) -> Result<SchedulerList> {
    check_kind(kind)?;
    let c = ctx(s).await?;
    let events = ev::load(s, None).await?;
    let items: Vec<SchedulerItem> = events
        .iter()
        .map(|e| {
            // Another definer's event the login can't change nor drop.
            let locked = c.protected(e.definer.as_deref().unwrap_or(""));
            SchedulerItem {
                id: ev::id(&e.schema, &e.name),
                name: e.name.clone(),
                description: Some(match &e.comment {
                    Some(c) => format!("{} · {c}", e.schema),
                    None => e.schema.clone(),
                }),
                enabled: Some(e.enabled()),
                system: false,
                can_edit: locked.then_some(false),
                can_delete: locked.then_some(false),
                actions: None,
            }
        })
        .collect();
    let mut notes = Vec::new();
    if items.iter().any(|i| i.can_edit == Some(false)) {
        notes.push(SYSTEM_DEFINERS.to_string());
    }
    Ok(SchedulerList { items, notes })
}

// ---------------------------------------------------------------------------
// The event as the form edits it
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Status {
    #[default]
    Enabled,
    Disabled,
    /// Off on replicas (created on the source).
    ReplicaSide,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Schedule {
    /// Once, at this time (`YYYY-MM-DD HH:MM:SS` on the event's zone).
    At(String),
    /// `EVERY value unit [STARTS …] [ENDS …]`; `value` as typed (`1`,
    /// `1:30`), without quotes.
    Every {
        value: String,
        unit: String,
        starts: Option<String>,
        ends: Option<String>,
    },
}

impl Default for Schedule {
    fn default() -> Self {
        Schedule::At(String::new())
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Spec {
    pub(super) schema: String,
    pub(super) name: String,
    /// `user@host` as `DEFINER` reads (a MariaDB role: its name alone).
    pub(super) definer: String,
    pub(super) status: Status,
    pub(super) comment: String,
    pub(super) schedule: Schedule,
    pub(super) preserve: bool,
    pub(super) time_zone: String,
    pub(super) sql_mode: String,
    /// Without a trailing `;`.
    pub(super) body: String,
}

/// What the scripts depend on besides the event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Flavor {
    /// OceanBase: `USE` first, no zone of the event's own.
    pub(super) ob: bool,
    /// `DISABLE ON REPLICA` (MySQL 8.2.0+) rather than `DISABLE ON SLAVE`,
    /// the only form before it (8.0 and 8.1, Aurora MySQL 3).
    pub(super) replica: bool,
    /// `ALTER EVENT … ON SCHEDULE` replaces the whole schedule (MySQL 8):
    /// a missing `ENDS` clears it. MariaDB, MySQL 5.7 and OceanBase keep
    /// the old `STARTS` and `ENDS` when the clause leaves them out.
    pub(super) resets_schedule: bool,
    /// The login may name another account as definer (SET_USER_ID, SUPER…):
    /// a change that fails halfway can put the event's own definer back.
    pub(super) any_definer: bool,
}

/// A time far ahead for the step that clears an event's `ENDS` where
/// `ALTER EVENT` keeps it: a one-time schedule (`AT`) drops `STARTS` and
/// `ENDS`, then the recurring one goes in.
const CLEAR_ENDS_AT: &str = "2037-12-31 23:59:59";

fn status_key(s: Status) -> &'static str {
    match s {
        Status::Enabled => "enable",
        Status::Disabled => "disable",
        Status::ReplicaSide => "replica",
    }
}

fn status_sql(s: Status, f: Flavor) -> &'static str {
    match s {
        Status::Enabled => "ENABLE",
        Status::Disabled => "DISABLE",
        Status::ReplicaSide if f.replica => "DISABLE ON REPLICA",
        Status::ReplicaSide => "DISABLE ON SLAVE",
    }
}

/// A statement without its trailing `;` (and the space around it).
pub(super) fn body_text(b: &str) -> String {
    b.trim().trim_end_matches(';').trim_end().to_string()
}

fn normal_time(t: Option<&str>) -> Option<String> {
    t.and_then(super::parse_datetime)
}

/// The event as the server has it, as a spec (`ob_zone`: OceanBase's).
pub(super) fn spec_of(e: &Event, ob_zone: Option<&str>) -> Spec {
    let status = if e.enabled() {
        Status::Enabled
    } else if e.replica_side() {
        Status::ReplicaSide
    } else {
        Status::Disabled
    };
    Spec {
        schema: e.schema.clone(),
        name: e.name.clone(),
        definer: e.definer.clone().unwrap_or_default(),
        status,
        comment: e.comment.clone().unwrap_or_default(),
        schedule: if e.recurring {
            Schedule::Every {
                value: e
                    .every
                    .as_deref()
                    .unwrap_or("")
                    .trim()
                    .trim_matches('\'')
                    .to_string(),
                unit: e.unit.as_deref().unwrap_or("").to_ascii_uppercase(),
                starts: normal_time(e.starts.as_deref()),
                ends: normal_time(e.ends.as_deref()),
            }
        } else {
            Schedule::At(normal_time(e.at.as_deref()).unwrap_or_default())
        },
        preserve: e.preserve,
        time_zone: ob_zone.map_or_else(|| e.tz.clone(), str::to_string),
        sql_mode: e.sql_mode.clone().unwrap_or_default(),
        body: body_text(e.body.as_deref().unwrap_or("")),
    }
}

/// A string literal under `sql_mode`: with `NO_BACKSLASH_ESCAPES` a
/// backslash is itself, otherwise it's escaped.
pub(super) fn lit_in(sql_mode: &str, s: &str) -> String {
    let nbe = sql_mode
        .split(',')
        .any(|m| m.trim().eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES"));
    if nbe {
        format!("'{}'", s.replace('\'', "''"))
    } else {
        lit(s)
    }
}

/// The interval value as `EVERY` takes it: a number bare, the parts of a
/// composite unit as a string.
fn interval_value(v: &str) -> String {
    if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) {
        v.to_string()
    } else {
        lit(v)
    }
}

pub(super) fn schedule_sql(s: &Schedule) -> String {
    match s {
        Schedule::At(t) => format!("AT {}", lit(t)),
        Schedule::Every {
            value,
            unit,
            starts,
            ends,
        } => {
            let mut out = format!("EVERY {} {unit}", interval_value(value));
            if let Some(t) = starts {
                out.push_str(&format!(" STARTS {}", lit(t)));
            }
            if let Some(t) = ends {
                out.push_str(&format!(" ENDS {}", lit(t)));
            }
            out
        }
    }
}

fn completion_sql(preserve: bool) -> &'static str {
    if preserve {
        "ON COMPLETION PRESERVE"
    } else {
        "ON COMPLETION NOT PRESERVE"
    }
}

/// The statement between the session's SQL mode and zone set to the
/// event's and put back; after `USE` on OceanBase.
fn wrapped(spec: &Spec, f: Flavor, use_db: &str, stmt: String, head: String) -> Plan {
    let mut steps = Vec::new();
    if f.ob {
        steps.push(format!("USE {}", quoted(use_db)));
        steps.push("SET @dbine_sql_mode = @@session.sql_mode".into());
        steps.push(format!("SET SESSION sql_mode = {}", lit(&spec.sql_mode)));
    } else {
        steps.push(
            "SET @dbine_sql_mode = @@session.sql_mode, @dbine_time_zone = @@session.time_zone"
                .into(),
        );
        steps.push(format!(
            "SET SESSION sql_mode = {}, time_zone = {}",
            lit(&spec.sql_mode),
            lit(&spec.time_zone)
        ));
    }
    steps.push(stmt);
    let finally = vec![if f.ob {
        "SET SESSION sql_mode = @dbine_sql_mode".to_string()
    } else {
        "SET SESSION sql_mode = @dbine_sql_mode, time_zone = @dbine_time_zone".to_string()
    }];
    Plan {
        head: vec![head],
        steps,
        finally,
        id: Some(ev::id(&spec.schema, &spec.name)),
        ..Plan::default()
    }
}

pub(super) fn create_plan(spec: &Spec, f: Flavor) -> Plan {
    let mut stmt = format!(
        "CREATE DEFINER = {} EVENT {}\nON SCHEDULE {}\n{}\n{}",
        ev::definer_clause(&spec.definer),
        ev::id(&spec.schema, &spec.name),
        schedule_sql(&spec.schedule),
        completion_sql(spec.preserve),
        status_sql(spec.status, f)
    );
    if !spec.comment.is_empty() {
        stmt.push_str(&format!(
            "\nCOMMENT {}",
            lit_in(&spec.sql_mode, &spec.comment)
        ));
    }
    stmt.push_str(&format!("\nDO {}", spec.body));
    wrapped(
        spec,
        f,
        &spec.schema,
        stmt,
        format!("Crea el evento «{}.{}»", spec.schema, spec.name),
    )
}

/// The `ALTER EVENT` from `cur` to `want`, only what changed; `None` when
/// nothing did.
pub(super) fn alter_plan(cur: &Spec, want: &Spec, f: Flavor) -> Option<Plan> {
    let mut clauses = Vec::new();
    let mut before = None;
    let zone_changed = !f.ob && !cur.time_zone.eq_ignore_ascii_case(&want.time_zone);
    if cur.schedule != want.schedule || zone_changed {
        let mut schedule = schedule_sql(&want.schedule);
        if let Schedule::Every { starts, ends, .. } = &want.schedule {
            // Left out, STARTS would stay where ALTER keeps it.
            if starts.is_none() {
                schedule.push_str(" STARTS CURRENT_TIMESTAMP");
            }
            let had_end = matches!(&cur.schedule, Schedule::Every { ends: Some(_), .. });
            if ends.is_none() && had_end && !f.resets_schedule {
                before = Some(format!(
                    "ALTER DEFINER = {} EVENT {}\nON SCHEDULE AT {}",
                    ev::definer_clause(&want.definer),
                    ev::id(&cur.schema, &cur.name),
                    lit(CLEAR_ENDS_AT)
                ));
            }
        }
        clauses.push(format!("ON SCHEDULE {schedule}"));
    }
    if cur.preserve != want.preserve {
        clauses.push(completion_sql(want.preserve).to_string());
    }
    if cur.schema != want.schema || cur.name != want.name {
        clauses.push(format!("RENAME TO {}", ev::id(&want.schema, &want.name)));
    }
    if cur.status != want.status {
        clauses.push(status_sql(want.status, f).to_string());
    }
    if cur.comment != want.comment {
        clauses.push(format!("COMMENT {}", lit_in(&want.sql_mode, &want.comment)));
    }
    if body_text(&cur.body) != want.body || cur.sql_mode != want.sql_mode {
        clauses.push(format!("DO {}", want.body));
    }
    if clauses.is_empty() && cur.definer == want.definer {
        return None;
    }
    if clauses.is_empty() {
        // ALTER EVENT takes at least one clause: the status as it is.
        clauses.push(status_sql(want.status, f).to_string());
    }
    let stmt = format!(
        "ALTER DEFINER = {} EVENT {}\n{}",
        ev::definer_clause(&want.definer),
        ev::id(&cur.schema, &cur.name),
        clauses.join("\n")
    );
    // The session takes the event's zone only when the schedule goes:
    // the zone the event keeps is the one in effect then.
    let mut plan = wrapped(
        want,
        f,
        &cur.schema,
        stmt,
        format!("Cambia el evento «{}.{}»", cur.schema, cur.name),
    );
    if let Some(b) = before {
        // Just before the ALTER: the schedule loses its end first. If the
        // ALTER then fails (a bad statement, a name taken), the event gets
        // its schedule back, under the zone and SQL mode it had (the event
        // keeps those of the session that sets its schedule).
        let at = plan.steps.len() - 1;
        plan.steps.insert(at, b);
        plan.undo_from = Some(at);
        plan.undo = vec![
            if f.ob {
                format!("SET SESSION sql_mode = {}", lit(&cur.sql_mode))
            } else {
                format!(
                    "SET SESSION sql_mode = {}, time_zone = {}",
                    lit(&cur.sql_mode),
                    lit(&cur.time_zone)
                )
            },
            format!(
                "ALTER DEFINER = {} EVENT {}\nON SCHEDULE {}",
                ev::definer_clause(if f.any_definer {
                    &cur.definer
                } else {
                    &want.definer
                }),
                ev::id(&cur.schema, &cur.name),
                schedule_sql(&cur.schedule)
            ),
        ];
        plan.head.push(
            "Este servidor no quita el fin de una programación con un solo ALTER EVENT: el script la pasa antes a una sola vez lejana."
                .into(),
        );
        plan.head
            .push("Si el cambio falla, DBine le devuelve al evento su programación.".into());
    }
    plan.id = Some(ev::id(&want.schema, &want.name));
    Some(plan)
}

pub(super) fn drop_plan(schema: &str, name: &str, f: Flavor) -> Plan {
    let mut steps = Vec::new();
    if f.ob {
        steps.push(format!("USE {}", quoted(schema)));
    }
    steps.push(format!("DROP EVENT {}", ev::id(schema, name)));
    Plan {
        head: vec![format!("Borra el evento «{schema}.{name}»")],
        steps,
        id: Some(ev::id(schema, name)),
        ..Plan::default()
    }
}

// ---------------------------------------------------------------------------
// The form's values, checked
// ---------------------------------------------------------------------------

/// A zone `SET time_zone` takes: `SYSTEM`, an offset or a zone's name.
pub(super) fn valid_zone(z: &str) -> bool {
    !z.is_empty()
        && z.len() <= 64
        && z.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'+' | b'-' | b':'))
}

/// A SQL mode: names of modes, comma-separated (or none).
pub(super) fn valid_mode(m: &str) -> bool {
    m.len() <= 1024
        && m.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b','))
}

/// A definer as `DEFINER` reads it: `user@host` (or a MariaDB role).
fn valid_definer(d: &str) -> bool {
    let ok_user =
        |u: &str| !u.is_empty() && u.chars().count() <= 80 && !u.chars().any(char::is_control);
    match d.rsplit_once('@') {
        Some((u, h)) => {
            ok_user(u)
                && h.len() <= 255
                && h.bytes().all(|b| {
                    b.is_ascii_alphanumeric()
                        || matches!(b, b'.' | b'-' | b'_' | b'%' | b':' | b'/')
                })
        }
        None => ok_user(d),
    }
}

/// The interval value for `unit`: digits, and for a composite unit at most
/// as many parts as it has (`1:30` HOUR_MINUTE), not zero.
pub(super) fn valid_interval(value: &str, unit: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b':' | b' ' | b'-'))
        && ev::step(Some(value), Some(unit)).is_some()
}

/// The spec the form asks for. `cur`: the event being edited.
pub(super) fn spec_from(
    values: &FormValues,
    c: &Ctx,
    dbs: &[(String, String)],
    definers: &[(String, String)],
    cur: Option<&Spec>,
) -> Result<Spec> {
    let v = Vals(&values.values);
    let mut schema = name(v, "schema", "Base de datos", ev::MAX_NAME)?;
    if !dbs.is_empty() {
        schema = pick(&schema, dbs, |s| {
            format!("No existe la base «{s}» o este usuario no la ve.")
        })?
        .to_string();
    }
    if let Some(cur) = cur {
        if c.ob_zone.is_some() && schema != cur.schema {
            return Err(bad(
                "OceanBase no mueve un evento a otra base: crealo en la nueva y borrá este.".into(),
            ));
        }
    }
    let event = name(v, "name", "Nombre", ev::MAX_NAME)?;
    let definer = v.opt("definer").unwrap_or_else(|| c.user.clone());
    if !valid_definer(&definer) {
        return Err(bad(format!(
            "«{}» no es un definidor válido (usuario@host).",
            shown(&definer)
        )));
    }
    if c.protected(&definer) {
        // Left out of the choices: the server would refuse it.
        return Err(bad(if cur.is_some_and(|cur| cur.definer == definer) {
            SYSTEM_DEFINER.into()
        } else {
            SYSTEM_DEFINER_NEW.into()
        }));
    }
    let definer = if definers.is_empty() {
        definer
    } else {
        pick(&definer, definers, |d| {
            format!("«{d}» no es una cuenta de este servidor.")
        })?
        .to_string()
    };
    let unchanged = cur.is_some_and(|c| c.definer == definer);
    if !c.sets_any_definer() && definer != c.user {
        return Err(bad(if unchanged {
            FOREIGN_DEFINER.into()
        } else {
            "Solo un usuario con SET_USER_ID (o SUPER) puede poner a otro como definidor de un evento."
                .into()
        }));
    }
    let status = match v.choice(
        "status",
        "Estado",
        &["enable", "disable", "replica"],
        "enable",
    )? {
        "disable" => Status::Disabled,
        "replica" => Status::ReplicaSide,
        _ => Status::Enabled,
    };
    let schedule = match v.choice("type", "Tipo", &["once", "every"], "every")? {
        "once" => Schedule::At(
            datetime(v, "at", "Fecha y hora")?
                .ok_or_else(|| bad("Un evento de una sola vez necesita «Fecha y hora».".into()))?,
        ),
        _ => {
            let unit = v.choice("unit", "Unidad", &UNITS, "DAY")?;
            let value = v.text("every").trim_matches('\'').trim().to_string();
            if !valid_interval(&value, unit) {
                return Err(bad(format!(
                    "«Cada»: «{}» no es un intervalo para {unit} (un número mayor que cero; en las unidades compuestas, sus partes, como 1:30 en HOUR_MINUTE).",
                    shown(&value)
                )));
            }
            let starts = datetime(v, "starts", "Desde")?;
            let ends = datetime(v, "ends", "Hasta")?;
            if let (Some(a), Some(b)) = (&starts, &ends) {
                if b <= a {
                    return Err(bad("«Hasta» tiene que ser posterior a «Desde».".into()));
                }
            }
            Schedule::Every {
                value,
                unit: unit.to_string(),
                starts,
                ends,
            }
        }
    };
    let preserve =
        v.choice("completion", "Al terminar", &["drop", "preserve"], "drop")? == "preserve";
    let time_zone = match &c.ob_zone {
        Some(z) => z.clone(),
        None => {
            let z = v.opt("time_zone").unwrap_or_else(|| c.session_zone.clone());
            if !valid_zone(&z) {
                return Err(bad(format!(
                    "«Zona horaria»: «{}» no es una zona (SYSTEM, +03:00 o un nombre como Europe/Madrid).",
                    shown(&z)
                )));
            }
            z
        }
    };
    let sql_mode = match v.get_raw_mode() {
        Some(m) => m,
        None => cur.map_or_else(|| c.session_mode.clone(), |c| c.sql_mode.clone()),
    };
    if !valid_mode(&sql_mode) {
        return Err(bad(format!(
            "«Modo SQL»: «{}» no es un modo SQL (nombres separados por comas).",
            shown(&sql_mode)
        )));
    }
    let body = body_text(&v.raw("body"));
    if body.is_empty() {
        return Err(bad("El evento necesita una instrucción.".into()));
    }
    if !super::one_statement(&body, true) {
        return Err(bad(ONE_STATEMENT.into()));
    }
    Ok(Spec {
        schema,
        name: event,
        definer,
        status,
        comment: v.raw("comment").trim().to_string(),
        schedule,
        preserve,
        time_zone,
        sql_mode: sql_mode.to_ascii_uppercase(),
        body,
    })
}

impl Vals<'_> {
    /// The SQL mode as typed, spaces dropped; `None` when the form doesn't
    /// carry it.
    fn get_raw_mode(&self) -> Option<String> {
        self.0
            .get("sql_mode")
            .filter(|v| !v.is_null())
            .map(|_| self.text("sql_mode").replace(' ', ""))
    }
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

/// The accounts a definer can be: the server's users (MariaDB's roles
/// too), or, when they can't be read, the current user; and `also`.
async fn definers(s: &mut MySqlSession, c: &Ctx, also: Option<&str>) -> Vec<(String, String)> {
    let sql = if s.variant == Variant::MariaDb {
        "SELECT User, Host, is_role FROM mysql.user ORDER BY User, Host"
    } else {
        "SELECT User, Host FROM mysql.user ORDER BY User, Host"
    };
    let mut out: Vec<(String, String)> = Vec::new();
    if c.sets_any_definer() {
        for r in s.optional_rows(sql).await.iter() {
            let (Some(u), host) = (at(r, 0), at(r, 1).unwrap_or_default()) else {
                continue;
            };
            if u.is_empty() || u.starts_with("mysql.") || u.starts_with("mariadb.") {
                continue;
            }
            let role = at(r, 2).is_some_and(|x| x.eq_ignore_ascii_case("Y"));
            let d = if role || (host.is_empty() && s.variant == Variant::MariaDb) {
                u
            } else {
                format!("{u}@{host}")
            };
            // One the server wouldn't take from this login.
            if c.protected(&d) {
                continue;
            }
            out.push((d.clone(), d));
        }
    }
    for d in [Some(c.user.as_str()), also].into_iter().flatten() {
        if !d.is_empty() && !out.iter().any(|(v, _)| v == d) {
            out.push((d.to_string(), d.to_string()));
        }
    }
    out
}

fn status_select() -> FormKind {
    FormKind::select(&[
        ("enable", "Habilitado"),
        ("disable", "Deshabilitado"),
        ("replica", "Deshabilitado en las réplicas"),
    ])
}

fn unit_select() -> FormKind {
    FormKind::select(&[
        ("SECOND", "Segundos"),
        ("MINUTE", "Minutos"),
        ("HOUR", "Horas"),
        ("DAY", "Días"),
        ("WEEK", "Semanas"),
        ("MONTH", "Meses"),
        ("QUARTER", "Trimestres"),
        ("YEAR", "Años"),
        ("MINUTE_SECOND", "Minutos y segundos (MINUTE_SECOND)"),
        ("HOUR_MINUTE", "Horas y minutos (HOUR_MINUTE)"),
        ("HOUR_SECOND", "Horas, minutos y segundos (HOUR_SECOND)"),
        ("DAY_HOUR", "Días y horas (DAY_HOUR)"),
        ("DAY_MINUTE", "Días, horas y minutos (DAY_MINUTE)"),
        ("DAY_SECOND", "Días, horas, minutos y segundos (DAY_SECOND)"),
        ("YEAR_MONTH", "Años y meses (YEAR_MONTH)"),
    ])
}

pub(super) fn sections(ob: bool, existing: bool) -> Vec<FormSection> {
    let mut general = FormSection::new("general", "General").fields(vec![
        FormField::new(
            "schema",
            "Base de datos",
            FormKind::reference("database", false),
        )
        .required()
        .read_only(ob && existing),
        FormField::new("name", "Nombre", FormKind::Text).required(),
        FormField::new(
            "definer",
            "Definidor",
            FormKind::reference("definer", false),
        )
        .required()
        .help("La cuenta con cuyos privilegios corre el evento."),
        FormField::new("status", "Estado", status_select()).default(json!("enable")),
        FormField::new("comment", "Comentario", FormKind::Text),
    ]);
    if existing {
        general = general.fields(vec![
            FormField::new("created", "Creado", FormKind::DateTime).read_only(true),
            FormField::new("altered", "Modificado", FormKind::DateTime).read_only(true),
            FormField::new("last_executed", "Última ejecución", FormKind::DateTime).read_only(true),
            FormField::new("originator", "Servidor de origen", FormKind::Text).read_only(true),
        ]);
    }
    let schedule = FormSection::new("schedule", "Programación").fields(vec![
        FormField::new(
            "type",
            "Tipo",
            FormKind::select(&[("once", "Una sola vez (AT)"), ("every", "Recurrente (EVERY)")]),
        )
        .default(json!("every")),
        FormField::new("at", "Fecha y hora", FormKind::DateTime)
            .required()
            .when("type", &["once"]),
        FormField::new("every", "Cada", FormKind::Text)
            .required()
            .placeholder("1")
            .help("Un número; en las unidades compuestas, sus partes: 1:30 en HOUR_MINUTE, 2 12 en DAY_HOUR, 1-6 en YEAR_MONTH.")
            .when("type", &["every"])
            .default(json!("1")),
        FormField::new("unit", "Unidad", unit_select())
            .when("type", &["every"])
            .default(json!("DAY")),
        FormField::new("starts", "Desde", FormKind::DateTime)
            .help("Vacío: desde que se guarda.")
            .when("type", &["every"]),
        FormField::new("ends", "Hasta", FormKind::DateTime)
            .help("Vacío: sin fin.")
            .when("type", &["every"]),
        FormField::new("time_zone", "Zona horaria", FormKind::Text)
            .read_only(ob)
            .placeholder("SYSTEM")
            .help("La zona en la que se leen las horas de la programación y en la que corre el evento: SYSTEM, +03:00 o un nombre como Europe/Madrid."),
        FormField::new(
            "completion",
            "Al terminar",
            FormKind::select(&[
                ("drop", "Borrar el evento (ON COMPLETION NOT PRESERVE)"),
                ("preserve", "Conservarlo (ON COMPLETION PRESERVE)"),
            ]),
        )
        .default(json!("drop")),
    ]);
    let mut body = FormSection::new("body", "Instrucción").fields(vec![
        FormField::new("body", "Instrucción", FormKind::code("sql"))
            .required()
            .help("Lo que ejecuta el evento: una instrucción, o un bloque BEGIN … END con varias."),
        FormField::new("sql_mode", "Modo SQL", FormKind::Text)
            .help("El modo SQL con el que corre la instrucción: el evento lo guarda al crearse o al cambiar la instrucción."),
    ]);
    if existing {
        body = body.fields(vec![
            FormField::new("charset", "Juego de caracteres del cliente", FormKind::Text)
                .read_only(true),
            FormField::new("collation", "Intercalación de la conexión", FormKind::Text)
                .read_only(true),
            FormField::new("db_collation", "Intercalación de la base", FormKind::Text)
                .read_only(true),
        ]);
    }
    vec![general, schedule, body]
}

/// The form's values for a spec (`e`: the event's row, for what only the
/// server sets).
pub(super) fn values_of(
    spec: &Spec,
    e: Option<&Event>,
) -> std::collections::BTreeMap<String, serde_json::Value> {
    let mut v = std::collections::BTreeMap::new();
    let mut put = |k: &str, x: serde_json::Value| {
        v.insert(k.to_string(), x);
    };
    put("schema", json!(spec.schema));
    put("name", json!(spec.name));
    put("definer", json!(spec.definer));
    put("status", json!(status_key(spec.status)));
    put("comment", json!(spec.comment));
    match &spec.schedule {
        Schedule::At(t) => {
            put("type", json!("once"));
            put("at", form_time(Some(t)));
            put("every", json!("1"));
            put("unit", json!("DAY"));
        }
        Schedule::Every {
            value,
            unit,
            starts,
            ends,
        } => {
            put("type", json!("every"));
            put("every", json!(value));
            put("unit", json!(unit));
            put("starts", form_time(starts.as_deref()));
            put("ends", form_time(ends.as_deref()));
        }
    }
    put("time_zone", json!(spec.time_zone));
    put(
        "completion",
        json!(if spec.preserve { "preserve" } else { "drop" }),
    );
    put("body", json!(spec.body));
    put("sql_mode", json!(spec.sql_mode));
    if let Some(e) = e {
        put("created", form_time(e.created.as_deref()));
        put("altered", form_time(e.altered.as_deref()));
        put("last_executed", form_time(e.last.as_deref()));
        put(
            "originator",
            json!(e.originator.clone().unwrap_or_default()),
        );
        put("charset", json!(e.charset.clone().unwrap_or_default()));
        put("collation", json!(e.collation.clone().unwrap_or_default()));
        put(
            "db_collation",
            json!(e.db_collation.clone().unwrap_or_default()),
        );
    }
    v
}

async fn read(s: &mut MySqlSession, id: &str) -> Result<Event> {
    let (schema, name) = ev::event_id(id)?;
    ev::load(s, Some((&schema, &name)))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| ev::not_found(&schema, &name))
}

pub(super) async fn form(s: &mut MySqlSession, kind: &str, id: Option<&str>) -> Result<ObjectForm> {
    check_kind(kind)?;
    let c = ctx(s).await?;
    let ob = c.ob_zone.is_some();
    let mut f = ObjectForm::default();
    let current = match id {
        Some(id) => Some(read(s, id).await?),
        None => None,
    };
    let dbs = databases(s).await;
    match &current {
        Some(e) => {
            let spec = spec_of(e, c.ob_zone.as_deref());
            f.values = values_of(&spec, Some(e));
            f.choices
                .insert("definer".into(), definers(s, &c, Some(&spec.definer)).await);
            if c.protected(&spec.definer) {
                f.read_only = true;
                f.notes.push(SYSTEM_DEFINER.into());
            } else if !c.sets_any_definer() && spec.definer != c.user {
                f.notes.push(FOREIGN_DEFINER.into());
            }
        }
        None => {
            let current_db = s.current_database().await;
            let schema = current_db
                .filter(|d| dbs.iter().any(|(v, _)| v == d))
                .or_else(|| dbs.first().map(|(v, _)| v.clone()))
                .unwrap_or_default();
            let spec = Spec {
                schema,
                definer: c.user.clone(),
                schedule: Schedule::Every {
                    value: "1".into(),
                    unit: "DAY".into(),
                    starts: None,
                    ends: None,
                },
                time_zone: c.ob_zone.clone().unwrap_or_else(|| c.session_zone.clone()),
                sql_mode: c.session_mode.clone(),
                ..Spec::default()
            };
            f.values = values_of(&spec, None);
            f.choices
                .insert("definer".into(), definers(s, &c, None).await);
        }
    }
    f.choices.insert("database".into(), dbs);
    f.sections = sections(ob, current.is_some());
    if !c.may_write() {
        f.read_only = true;
        f.notes.insert(0, NO_EVENT_PRIVILEGE.into());
    }
    if ob {
        f.notes.push(OB_NOTE.into());
    }
    if let Some(n) = ev::scheduler_note(&c.scheduler) {
        f.notes.push(n.into());
    }
    Ok(f)
}

/// The form was read from an older version of the event.
pub(super) fn stale(values: &FormValues, e: &Event) -> Result<()> {
    let seen = Vals(&values.values).opt("altered");
    let now = e.altered.as_deref().and_then(super::parse_datetime);
    match (seen.as_deref().and_then(super::parse_datetime), now) {
        (Some(a), Some(b)) if a != b => Err(Error::Query(CHANGED.into())),
        _ => Ok(()),
    }
}

pub(super) async fn plan(
    s: &mut MySqlSession,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    check_kind(kind)?;
    let c = ctx(s).await?;
    let dbs = databases(s).await;
    match id {
        None => {
            let defs = definers(s, &c, None).await;
            let spec = spec_from(values, &c, &dbs, &defs, None)?;
            Ok(create_plan(&spec, c.flavor()))
        }
        Some(id) => {
            let e = read(s, id).await?;
            stale(values, &e)?;
            let cur = spec_of(&e, c.ob_zone.as_deref());
            if c.protected(&cur.definer) {
                return Err(bad(SYSTEM_DEFINER.into()));
            }
            let defs = definers(s, &c, Some(&cur.definer)).await;
            let want = spec_from(values, &c, &dbs, &defs, Some(&cur))?;
            Ok(alter_plan(&cur, &want, c.flavor()).unwrap_or_else(|| {
                Plan::nothing(
                    &format!("{}.{}", cur.schema, cur.name),
                    ev::id(&cur.schema, &cur.name),
                )
            }))
        }
    }
}

pub(super) async fn delete_plan(s: &mut MySqlSession, kind: &str, id: &str) -> Result<Plan> {
    check_kind(kind)?;
    let e = read(s, id).await?;
    if s.variant == Variant::MySql && ctx(s).await?.protected(e.definer.as_deref().unwrap_or("")) {
        return Err(bad(SYSTEM_DEFINER.into()));
    }
    let f = Flavor {
        ob: s.variant == Variant::OceanBase,
        ..Flavor::default()
    };
    Ok(drop_plan(&e.schema, &e.name, f))
}

/// The kind has no actions: the server runs an event only on its schedule
/// (no statement runs one now). The id is still read back and looked up;
/// "run" is answered why, any other key as not an action.
pub(super) async fn action_plan(
    s: &mut MySqlSession,
    kind: &str,
    id: &str,
    action: &str,
) -> Result<Plan> {
    check_kind(kind)?;
    read(s, id).await?;
    match action.trim() {
        "run" => Err(Error::Unsupported(NO_RUN.into())),
        other => Err(no_action(other)),
    }
}

// ---------------------------------------------------------------------------
// The scheduler's log (MySQL 8.0.22+)
// ---------------------------------------------------------------------------

/// `LOGGED` is a TIMESTAMP: read as seconds since the epoch, shown in UTC.
const LOG_SQL: &str = "SELECT UNIX_TIMESTAMP(LOGGED) AS ev_epoch, PRIO AS ev_prio, ERROR_CODE AS ev_code, DATA AS ev_data
  FROM performance_schema.error_log
 WHERE DATA LIKE 'Event Scheduler:%'
 ORDER BY LOGGED DESC
 LIMIT 2000";

fn level(prio: &str) -> &'static str {
    match prio.trim().to_ascii_uppercase().as_str() {
        "ERROR" => "Error",
        "WARNING" => "Advertencia",
        _ => "Información",
    }
}

/// `Event Scheduler: [root@localhost][db.ev] message` as the event
/// (`db.ev`) and the message.
pub(super) fn log_line(data: &str) -> (String, String) {
    let rest = data
        .strip_prefix("Event Scheduler:")
        .unwrap_or(data)
        .trim_start();
    let mut parts = Vec::new();
    let mut r = rest;
    while let Some(after) = r.strip_prefix('[') {
        let Some(end) = after.find(']') else { break };
        parts.push(&after[..end]);
        r = &after[end + 1..];
    }
    match parts.as_slice() {
        [_, event, ..] => (event.to_string(), r.trim().to_string()),
        _ => (String::new(), rest.trim().to_string()),
    }
}

pub(super) async fn logs(s: &mut MySqlSession) -> Result<Vec<MonitorTable>> {
    let c = ctx(s).await?;
    if !c.error_log {
        return Err(Error::Unsupported(NO_LOG.into()));
    }
    let rows = s.rows(LOG_SQL).await?;
    let mut t = MonitorTable::new(
        "event_log",
        "Registro del programador de eventos",
        &["Fecha (UTC)", "Tipo", "Código", "Evento", "Mensaje"],
    );
    t.rows = rows
        .iter()
        .map(|r| {
            let data = crate::session::named(r, &["ev_data"]).unwrap_or_default();
            let (event, message) = log_line(&data);
            vec![
                json!(text(r, &["ev_epoch"])
                    .and_then(|t| t.parse::<f64>().ok())
                    .filter(|t| t.is_finite())
                    .map(|t| Civil::from_seconds(t.floor() as i64).sql())
                    .unwrap_or_default()),
                json!(level(&text(r, &["ev_prio"]).unwrap_or_default())),
                json!(text(r, &["ev_code"]).unwrap_or_default()),
                json!(event),
                json!(one_line(&message)),
            ]
        })
        .collect();
    Ok(vec![t])
}
