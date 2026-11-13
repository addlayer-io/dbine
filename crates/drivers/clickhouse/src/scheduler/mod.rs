//! The server's scheduler managed whole through the scheduler contract
//! ([`dbine_driver::Session::scheduler_info`] and the rest), on both
//! flavors. Its one kind is the "job" kind, whose ids are the ones
//! [`crate::jobs`] lists, so the tree's job rows open the designer:
//!
//! - ClickHouse: the refreshable materialized views ([`refresh`]): created
//!   with `CREATE MATERIALIZED VIEW … REFRESH …`, changed in place with
//!   `ALTER TABLE … MODIFY REFRESH | MODIFY QUERY | MODIFY SQL SECURITY |
//!   MODIFY COMMENT`, `RENAME TABLE` and `SYSTEM START | STOP VIEW`, and
//!   created again with `CREATE OR REPLACE` only for what ClickHouse can't
//!   change in place (the target, the engine, the columns, `APPEND`). The
//!   history of a view's refreshes is cleared from `system.query_log` (for
//!   a user who holds `ALTER DELETE` on it), and
//!   the scheduler's log is the refreshes there and the view refresher's
//!   lines in `system.text_log`.
//! - Timeplus Proton: its tasks ([`task`]): `CREATE TASK`, `CREATE OR
//!   REPLACE TASK` (in place: the task keeps its UUID and its last run),
//!   `SYSTEM PAUSE | UNPAUSE TASK` and `DROP TASK`. Proton keeps only the
//!   last run of each task, so there is no history to clear nor a log.
//!
//! ClickHouse has no transactions for these statements: a script runs one
//! statement at a time, in an order that puts the ones most likely to be
//! refused first, and a failure says which ones were already applied.
//!
//! Ids and values come from the app as untrusted text: ids are parsed back
//! into their two names ([`crate::jobs::parse_id`]) and looked up on the
//! server, names picked from lists are checked against the server's, names
//! are quoted with [`crate::schema::q`] and texts written as literals with
//! [`crate::schema::string_literal`]; SQL from the form (the query, an
//! engine clause, a column list) has to lex whole, with no `;` in it, and
//! every statement has to read as one.

mod lex;
mod refresh;
mod task;

use crate::jobs::{self, parse_id, version_at_least, History};
use crate::schema::{comment_text, string_literal};
use crate::{ClickHouseSession, Flavor};
use dbine_driver::{
    Error, FormValues, MonitorTable, ObjectForm, Result, SchedulerInfo, SchedulerItem,
    SchedulerKind, SchedulerList,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Notes and messages
// ---------------------------------------------------------------------------

const READONLY_USER: &str = "Este usuario es de solo lectura (readonly): puede ver los objetos del planificador, pero no crearlos, cambiarlos ni borrarlos.";
const EXPERIMENTAL: &str = "En esta versión de ClickHouse las vistas refrescables son experimentales: DBine las crea y las cambia con allow_experimental_refreshable_materialized_view = 1.";
const NO_HISTORY_TASKS: &str =
    "Timeplus Proton guarda solo la última ejecución de cada tarea: no hay historial que borrar.";
const NO_TASK_LOG: &str =
    "Timeplus Proton no tiene un registro de sus tareas: guarda solo la última ejecución de cada una.";
const NO_LOG: &str = "Este servidor no tiene un registro de refrescos legible: system.query_log etiqueta cada refresco con su vista desde la 25.4, y system.text_log no está habilitado o este usuario no puede leerlo.";

/// The kind every flavor has.
const JOB: &str = "job";

fn bad(message: String) -> Error {
    Error::Query(message)
}

/// Text from the app, short and on one line, for a message.
fn shown(s: &str) -> String {
    comment_text(s.trim()).chars().take(60).collect()
}

fn check_kind(kind: &str) -> Result<()> {
    if kind.trim() == JOB {
        Ok(())
    } else {
        Err(bad(format!(
            "«{}» no es un tipo de objeto del planificador de este servidor",
            shown(kind)
        )))
    }
}

fn bad_id(id: &str) -> Error {
    bad(format!(
        "«{}» no es un id de job de este servidor",
        shown(id)
    ))
}

/// An id the app sent back, as its database and name.
fn ids(id: &str) -> Result<(String, String)> {
    parse_id(id).ok_or_else(|| bad_id(id))
}

// ---------------------------------------------------------------------------
// What the server and the user allow
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub(crate) struct Ctx {
    /// `version()`.
    pub(crate) version: String,
    /// The user's profile is read-only (`readonly` above 0): nothing can be
    /// created, changed or deleted.
    pub(crate) readonly: bool,
    /// The server has a scheduler: refreshable views (ClickHouse 23.12 on)
    /// or tasks (Proton 3.0 on).
    pub(crate) available: bool,
}

impl Ctx {
    /// Before 24.10 refreshable views are experimental: their statements
    /// go with `allow_experimental_refreshable_materialized_view = 1`.
    pub(crate) fn experimental(&self) -> bool {
        !version_at_least(&self.version, 24, 10)
    }

    /// `DEFINER` and `SQL SECURITY` came in 24.2.
    pub(crate) fn sql_security(&self) -> bool {
        version_at_least(&self.version, 24, 2)
    }

    /// The request settings every statement of a change goes with.
    pub(crate) fn settings(&self) -> Vec<(&'static str, &'static str)> {
        if self.experimental() {
            vec![("allow_experimental_refreshable_materialized_view", "1")]
        } else {
            Vec::new()
        }
    }
}

impl ClickHouseSession {
    async fn sched_ctx(&self) -> Result<Ctx> {
        let version = self
            .rows("SELECT version()", &[])
            .await?
            .first()
            .and_then(|r| jobs::opt(r.first()))
            .unwrap_or_default();
        let readonly = self.profile_readonly().await > 0;
        let available = match self.flavor {
            Flavor::ClickHouse => !self.refresh_columns().await?.is_empty(),
            Flavor::Timeplus => match self.rows("SHOW TASKS", &[]).await {
                Ok(_) => true,
                Err(e) if jobs::no_tasks(&e) => false,
                Err(e) => return Err(e),
            },
        };
        Ok(Ctx {
            version,
            readonly,
            available,
        })
    }

    /// The context, refusing a server without a scheduler.
    async fn sched_ready(&self) -> Result<Ctx> {
        let c = self.sched_ctx().await?;
        if !c.available {
            return Err(Error::Unsupported(self.missing().into()));
        }
        Ok(c)
    }

    /// The context, for a change.
    async fn sched_ready_to_change(&self) -> Result<Ctx> {
        let c = self.sched_ready().await?;
        if c.readonly {
            return Err(bad(READONLY_USER.into()));
        }
        Ok(c)
    }

    fn missing(&self) -> &'static str {
        match self.flavor {
            Flavor::ClickHouse => jobs::NO_REFRESHABLE,
            Flavor::Timeplus => jobs::NO_TASKS,
        }
    }

    // -----------------------------------------------------------------------
    // The contract's calls
    // -----------------------------------------------------------------------

    pub(crate) async fn sched_info(&self) -> Result<SchedulerInfo> {
        let c = self.sched_ctx().await?;
        if !c.available {
            return Ok(SchedulerInfo {
                kinds: Vec::new(),
                logs: false,
                notes: vec![self.missing().to_string()],
            });
        }
        let w = !c.readonly;
        // "Run now" and "stop" are the job's: `SYSTEM REFRESH | CANCEL VIEW`
        // (both since refreshable views came, 23.12) and Proton's `SYSTEM
        // EXECUTE TASK`; no other action applies. Neither has steps. Only
        // ClickHouse keeps a history to clear: from 25.4, where
        // system.query_log tags each refresh with its view, for a user who
        // may change things, reads it and holds ALTER DELETE on it (clearing
        // is a lightweight DELETE). `CHECK GRANT` came in 24.5, so it's
        // there wherever the history is; a check the server doesn't answer
        // leaves the option (the DELETE says if it's refused).
        let kind = match self.flavor {
            Flavor::ClickHouse => {
                let clear = w
                    && self.history().await == History::Readable
                    && self.user_grant("ALTER DELETE ON system.query_log").await? != Some(false);
                SchedulerKind::new("job", "Vistas refrescables", "vista refrescable")
                    .clear_history(clear)
            }
            Flavor::Timeplus => SchedulerKind::new("job", "Tareas", "tarea"),
        };
        let mut notes = Vec::new();
        if c.readonly {
            notes.push(READONLY_USER.to_string());
        }
        let logs = match self.flavor {
            Flavor::ClickHouse => {
                if c.experimental() {
                    notes.push(EXPERIMENTAL.to_string());
                }
                self.log_sources().await != (false, false)
            }
            Flavor::Timeplus => false,
        };
        Ok(SchedulerInfo {
            kinds: vec![kind.abilities(w, w, w)],
            logs,
            notes,
        })
    }

    pub(crate) async fn sched_items(&self, kind: &str) -> Result<SchedulerList> {
        check_kind(kind)?;
        let list = self.jobs_list().await?;
        let items = list
            .jobs
            .iter()
            .map(|j| SchedulerItem {
                id: j.id.clone(),
                name: match &j.group {
                    Some(db) => format!("{db}.{}", j.name),
                    None => j.name.clone(),
                },
                description: j.schedule.clone(),
                enabled: Some(j.enabled),
                system: false,
                can_edit: None,
                can_delete: None,
                actions: None,
            })
            .collect();
        let missing = self.missing();
        Ok(SchedulerList {
            items,
            notes: list.notes.into_iter().filter(|n| n == missing).collect(),
        })
    }

    pub(crate) async fn sched_form(&self, kind: &str, id: Option<&str>) -> Result<ObjectForm> {
        check_kind(kind)?;
        let c = self.sched_ready().await?;
        let mut f = match self.flavor {
            Flavor::ClickHouse => refresh::form(self, &c, id).await?,
            Flavor::Timeplus => task::form(self, &c, id).await?,
        };
        if c.readonly {
            f.read_only = true;
            f.notes.insert(0, READONLY_USER.to_string());
        }
        Ok(f)
    }

    async fn sched_plan(&self, c: &Ctx, id: Option<&str>, values: &FormValues) -> Result<Plan> {
        match self.flavor {
            Flavor::ClickHouse => refresh::plan(self, c, id, values).await,
            Flavor::Timeplus => task::plan(self, c, id, values).await,
        }
    }

    pub(crate) async fn sched_script(
        &self,
        kind: &str,
        id: Option<&str>,
        values: &FormValues,
    ) -> Result<String> {
        check_kind(kind)?;
        let c = self.sched_ready().await?;
        Ok(self.sched_plan(&c, id, values).await?.script())
    }

    pub(crate) async fn sched_save(
        &self,
        kind: &str,
        id: Option<&str>,
        values: &FormValues,
    ) -> Result<Option<String>> {
        check_kind(kind)?;
        let c = self.sched_ready_to_change().await?;
        let plan = self.sched_plan(&c, id, values).await?;
        self.run_plan(&plan).await?;
        Ok(Some(plan.id))
    }

    async fn sched_delete_plan(&self, c: &Ctx, id: &str) -> Result<Plan> {
        match self.flavor {
            Flavor::ClickHouse => refresh::delete_plan(self, c, id).await,
            Flavor::Timeplus => task::delete_plan(self, id).await,
        }
    }

    pub(crate) async fn sched_delete_script(&self, kind: &str, id: &str) -> Result<String> {
        check_kind(kind)?;
        let c = self.sched_ready().await?;
        Ok(self.sched_delete_plan(&c, id).await?.script())
    }

    pub(crate) async fn sched_delete(&self, kind: &str, id: &str) -> Result<()> {
        check_kind(kind)?;
        let c = self.sched_ready_to_change().await?;
        let plan = self.sched_delete_plan(&c, id).await?;
        self.run_plan(&plan).await
    }

    /// ClickHouse: the view's refreshes in `system.query_log` (a
    /// lightweight `DELETE`); Proton keeps no history.
    pub(crate) async fn sched_clear_history(&self, id: &str) -> Result<()> {
        if self.flavor == Flavor::Timeplus {
            return Err(Error::Unsupported(NO_HISTORY_TASKS.into()));
        }
        let (db, name) = ids(id)?;
        self.sched_ready_to_change().await?;
        self.refresh(&db, &name).await?;
        if let Some(note) = self.history().await.note() {
            return Err(Error::Unsupported(note.into()));
        }
        self.rows(&clear_history_sql(&db, &name), &[])
            .await
            .map(|_| ())
    }

    /// ClickHouse: the recent refreshes (`system.query_log`, 25.4 on) and
    /// the refresher's own lines (`system.text_log`), newest first.
    pub(crate) async fn sched_logs(&self) -> Result<Vec<MonitorTable>> {
        if self.flavor == Flavor::Timeplus {
            return Err(Error::Unsupported(NO_TASK_LOG.into()));
        }
        self.sched_ready().await?;
        let (refreshes, text) = self.log_sources().await;
        if !refreshes && !text {
            return Err(Error::Unsupported(NO_LOG.into()));
        }
        let mut out = Vec::new();
        if refreshes {
            let rows = self.rows(REFRESHES_SQL, &[]).await?;
            let mut t = MonitorTable::new(
                "refreshes",
                "Refrescos",
                &[
                    "Fecha (UTC)",
                    "Vista",
                    "Resultado",
                    "Duración (ms)",
                    "Filas leídas",
                    "Filas escritas",
                    "Error",
                ],
            );
            t.rows = rows.iter().map(|r| refresh_log_row(r)).collect();
            out.push(t);
        }
        if text {
            let rows = self.rows(TEXT_LOG_SQL, &[]).await?;
            let mut t = MonitorTable::new(
                "refresh_log",
                "Registro de las vistas refrescables",
                &["Fecha (UTC)", "Nivel", "Vista", "Mensaje"],
            );
            t.rows = rows
                .iter()
                .map(|r| {
                    (0..4)
                        .map(|i| json!(jobs::opt(r.get(i)).unwrap_or_default()))
                        .collect()
                })
                .collect();
            out.push(t);
        }
        Ok(out)
    }

    /// Which logs can be read: the tagged refreshes in `system.query_log`,
    /// and `system.text_log`.
    async fn log_sources(&self) -> (bool, bool) {
        let refreshes = self.history().await == History::Readable;
        let text = self
            .rows("SELECT 1 FROM system.text_log LIMIT 0", &[])
            .await
            .is_ok();
        (refreshes, text)
    }

    /// Runs the plan's statements in order. ClickHouse has no transaction
    /// for them: a failure says how many were already applied, and a drop
    /// and create refused at the create puts the old object back.
    async fn run_plan(&self, plan: &Plan) -> Result<()> {
        for (i, stmt) in plan.statements.iter().enumerate() {
            let ran = self
                .rows_in(plan.database.as_deref(), stmt, &[], &plan.settings)
                .await;
            let Err(e) = ran else { continue };
            if let Some(r) = plan
                .restore
                .as_ref()
                .filter(|r| i > r.drop && i <= r.create)
            {
                let mut restored = Ok(());
                for s in &r.statements {
                    restored = self
                        .rows_in(plan.database.as_deref(), s, &[], &plan.settings)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string());
                    if restored.is_err() {
                        break;
                    }
                }
                return Err(Error::Query(r.message(&e.to_string(), restored)));
            }
            return Err(match e {
                Error::Query(m) if i > 0 => Error::Query(format!(
                    "{m}\n(El servidor no deshace los cambios anteriores: ya se aplicaron las primeras {i} instrucciones del script.)"
                )),
                e => e,
            });
        }
        Ok(())
    }
}

/// The lightweight `DELETE` of a view's refreshes from `system.query_log`
/// (the same rows the job's history reads: its own tag, and its retries').
pub(crate) fn clear_history_sql(db: &str, view: &str) -> String {
    let (lc, lca) = jobs::refresh_comments(db, view);
    format!(
        "DELETE FROM system.query_log
WHERE client_name = '{}'
  AND (log_comment = {}
       OR (startsWith(log_comment, {}) AND match(substring(log_comment, {}), '^[0-9]+/[0-9]+[)]$')))",
        jobs::REFRESH_CLIENT,
        string_literal(&lc),
        string_literal(&lca),
        lca.len() + 1
    )
}

/// The refreshes of every view, newest first.
const REFRESHES_SQL: &str =
    "SELECT toString(toTimeZone(event_time, 'UTC')), substring(log_comment, 12), toString(type),
       exception_code, query_duration_ms, read_rows, written_rows, substring(exception, 1, 4000)
FROM system.query_log
WHERE client_name = 'refreshable materialized view' AND type != 'QueryStart'
ORDER BY event_time_microseconds DESC
LIMIT 500";

/// The refresher's lines (its logger is `RefreshTask(db.view)`).
const TEXT_LOG_SQL: &str = "SELECT toString(toTimeZone(event_time, 'UTC')), toString(level),
       substring(logger_name, 13, length(logger_name) - 13), substring(message, 1, 4000)
FROM system.text_log
WHERE logger_name LIKE 'RefreshTask(%'
ORDER BY event_time_microseconds DESC
LIMIT 500";

/// A row of [`REFRESHES_SQL`] as the log shows it.
fn refresh_log_row(r: &[Value]) -> Vec<Value> {
    let ty = jobs::opt(r.get(2)).unwrap_or_default();
    let code = jobs::num(r.get(3)).unwrap_or(0.0);
    let result = if ty == "QueryFinish" {
        "Correcto"
    } else if code == 394.0 {
        "Cancelado"
    } else {
        "Error"
    };
    let n = |i: usize| jobs::num(r.get(i)).map_or(Value::Null, |v| json!(v.max(0.0) as u64));
    vec![
        json!(jobs::opt(r.first()).unwrap_or_default()),
        json!(jobs::opt(r.get(1)).unwrap_or_default()),
        json!(result),
        n(4),
        n(5),
        n(6),
        json!(jobs::opt(r.get(7)).unwrap_or_default()),
    ]
}

// ---------------------------------------------------------------------------
// What a change runs
// ---------------------------------------------------------------------------

/// A change: comments that say what it does (and what it loses), the
/// statements in order, and the object's id afterwards.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Plan {
    pub(crate) comments: Vec<String>,
    /// Request settings every statement goes with; the script shows them
    /// as a `SET` first (which a query tab keeps for its session).
    pub(crate) settings: Vec<(&'static str, &'static str)>,
    /// Empty: nothing to change.
    pub(crate) statements: Vec<String>,
    /// The database the statements run in, where they hold a query whose
    /// unqualified names it resolves; the script shows it as a `USE` (as a
    /// query tab follows it).
    pub(crate) database: Option<String>,
    pub(crate) id: String,
    /// A drop and create: how to put the old object back when the server
    /// refuses the create.
    pub(crate) restore: Option<Restore>,
}

/// The old object of a drop and create, created again when the server
/// refuses the new one (a database without `CREATE OR REPLACE`).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Restore {
    /// The index of the `DROP`.
    pub(crate) drop: usize,
    /// The index of the `CREATE`: a refusal after the drop, up to it,
    /// calls for the restore.
    pub(crate) create: usize,
    /// The statements that create the old object again (and stop it, when
    /// it was stopped).
    pub(crate) statements: Vec<String>,
    /// The object, for the message.
    pub(crate) what: String,
}

impl Restore {
    /// What the save says when the create was refused with `cause`, and
    /// how creating the old object again went.
    pub(crate) fn message(&self, cause: &str, restored: std::result::Result<(), String>) -> String {
        match restored {
            Ok(()) => format!(
                "El servidor rechazó la nueva definición ({cause}): se volvió a crear «{}» como estaba.",
                self.what
            ),
            Err(m) => format!(
                "El servidor rechazó la nueva definición ({cause}) y tampoco se pudo volver a crear «{}» como estaba ({m}). Su definición era:\n{}",
                self.what,
                self.statements.join(";\n")
            ),
        }
    }
}

impl Plan {
    pub(crate) fn script(&self) -> String {
        let mut out = String::new();
        for c in &self.comments {
            out.push_str("-- ");
            out.push_str(&comment_text(c));
            out.push('\n');
        }
        if self.statements.is_empty() {
            return out;
        }
        for (k, v) in &self.settings {
            out.push_str(&format!("SET {k} = {v};\n"));
        }
        if let Some(db) = &self.database {
            out.push_str(&format!("USE {};\n", qi(db)));
        }
        for s in &self.statements {
            out.push_str(s);
            out.push_str(";\n");
        }
        out
    }
}

/// A name quoted so that ClickHouse and DBine's script splitter read it the
/// same: backslashes escaped, backticks doubled. ([`crate::schema::q`]
/// escapes backticks with a backslash, which the server reads but the
/// splitter doesn't: a script opened in a query tab could split on a name
/// with `` `; `` in it.)
pub(crate) fn qi(name: &str) -> String {
    format!("`{}`", name.replace('\\', "\\\\").replace('`', "``"))
}

/// `db.name`, each quoted with [`qi`].
pub(crate) fn qname(db: &str, name: &str) -> String {
    format!("{}.{}", qi(db), qi(name))
}

/// A statement DBine wrote reads as one statement: nothing from the form
/// ended it early or started another.
pub(crate) fn one_statement(sql: &str) -> Result<()> {
    if crate::statements(sql).len() == 1 {
        Ok(())
    } else {
        Err(bad(
            "La definición tiene un «;» o un comentario sin cerrar: el script no sería una sola instrucción.".into(),
        ))
    }
}

// ---------------------------------------------------------------------------
// Values from the designer
// ---------------------------------------------------------------------------

/// A form's values as the app sent them: untrusted, any JSON type where a
/// field expects another.
#[derive(Clone, Copy)]
pub(crate) struct Vals<'a>(pub(crate) &'a BTreeMap<String, Value>);

impl<'a> Vals<'a> {
    fn get(&self, k: &str) -> Option<&'a Value> {
        self.0.get(k).filter(|v| !v.is_null())
    }

    /// The value as typed (code keeps its spaces).
    pub(crate) fn raw(&self, k: &str) -> String {
        match self.get(k) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(_) | Value::Object(_)) | None => String::new(),
            Some(v) => v.to_string(),
        }
    }

    /// The value trimmed.
    pub(crate) fn text(&self, k: &str) -> String {
        self.raw(k).trim().to_string()
    }

    /// The value trimmed, `None` when empty.
    pub(crate) fn opt(&self, k: &str) -> Option<String> {
        Some(self.text(k)).filter(|t| !t.is_empty())
    }

    /// A switch; `default` when the form didn't send it.
    pub(crate) fn bool_or(&self, k: &str, default: bool) -> bool {
        match self.get(k) {
            None => default,
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            Some(Value::String(s)) => matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "true" | "1" | "yes" | "on"
            ),
            Some(_) => false,
        }
    }

    /// A whole number in `min..=max`; `None` when missing or empty.
    pub(crate) fn int(&self, k: &str, label: &str, min: i64, max: i64) -> Result<Option<i64>> {
        let n = match self.get(k) {
            None => return Ok(None),
            Some(Value::String(s)) if s.trim().is_empty() => return Ok(None),
            Some(Value::Number(n)) => n
                .as_i64()
                .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
            Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
            Some(_) => None,
        };
        match n {
            Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
            Some(n) => Err(bad(format!(
                "«{label}» tiene que estar entre {min} y {max} (es {n})."
            ))),
            None => Err(bad(format!("«{label}» tiene que ser un número entero."))),
        }
    }

    /// A list of values (a multi-select's), trimmed, without empties.
    pub(crate) fn strings(&self, k: &str) -> Vec<String> {
        match self.get(k) {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.trim().to_string()),
                    Value::Null => None,
                    v => Some(v.to_string()),
                })
                .filter(|s| !s.is_empty())
                .collect(),
            Some(Value::String(s)) if !s.trim().is_empty() => vec![s.trim().to_string()],
            _ => Vec::new(),
        }
    }
}

/// An object's name from the form: present, on one line.
pub(crate) fn name_field(v: Vals, k: &str, label: &str) -> Result<String> {
    let name = v
        .opt(k)
        .ok_or_else(|| bad(format!("«{label}» es obligatorio.")))?;
    if name.chars().any(char::is_control) || name.len() > 512 {
        return Err(bad(format!(
            "«{label}»: «{}» no es un nombre válido.",
            shown(&name)
        )));
    }
    Ok(name)
}

/// A database the user picked: one of the server's.
pub(crate) fn database_field(v: Vals, databases: &[String]) -> Result<String> {
    let db = v
        .opt("database")
        .ok_or_else(|| bad("«Base de datos» es obligatorio.".into()))?;
    if databases.contains(&db) {
        Ok(db)
    } else {
        Err(bad(format!(
            "La base de datos «{}» no existe en este servidor.",
            shown(&db)
        )))
    }
}

/// The SQL query of a form: present, without the `;` at its end, and closed.
pub(crate) fn query_field(v: Vals) -> Result<String> {
    let query = lex::trim_query(&v.raw("query"));
    if query.is_empty() {
        return Err(bad("«Consulta» es obligatorio.".into()));
    }
    if !lex::closed(&query) {
        return Err(bad(
            "«Consulta»: tiene un texto, un nombre o un comentario sin cerrar, paréntesis que no cierran o un «;» en el medio."
                .into(),
        ));
    }
    Ok(query)
}

/// `(value, label)` pairs where both are the same.
pub(crate) fn same_pairs(names: &[String]) -> Vec<(String, String)> {
    names.iter().map(|n| (n.clone(), n.clone())).collect()
}

/// `(id, "db.name")` pairs.
pub(crate) fn id_pairs(names: &[(String, String)]) -> Vec<(String, String)> {
    names
        .iter()
        .map(|(db, n)| (jobs::job_id(db, n), format!("{db}.{n}")))
        .collect()
}

#[cfg(test)]
mod tests;
