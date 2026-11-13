//! Snowflake's scheduler managed whole through the scheduler contract
//! ([`dbine_driver::Session::scheduler_info`] and the rest): what Snowsight's
//! Tasks and Alerts pages do, as designer forms, the DDL a change runs and
//! the scheduler's log.
//!
//! - `job`: a task graph as the jobs list shows it (its root, or a
//!   standalone task, by the same id), with its child tasks and its
//!   finalizer as a list of the form. `CREATE TASK` / `ALTER TASK … SET |
//!   UNSET | MODIFY AS | MODIFY WHEN | REMOVE WHEN | ADD AFTER | REMOVE AFTER
//!   | SET FINALIZE | UNSET FINALIZE | RESUME | SUSPEND` / `DROP TASK`.
//!   Snowflake changes a graph (and a standalone task) only while its root
//!   is suspended: a change to a started one suspends the root first and
//!   resumes it last. A graph that belongs to a task graph object is changed
//!   in place instead (the object stages the change) and committed.
//! - `alert`: alerts (`CREATE ALERT … IF (EXISTS (…)) THEN …`, also from a
//!   template), `ALTER ALERT … SET | UNSET | MODIFY CONDITION | MODIFY
//!   ACTION | RESUME | SUSPEND`, `DROP ALERT`.
//! - `task_graph`: task graph objects (a preview feature): `CREATE TASK
//!   GRAPH`, `ALTER TASK GRAPH … SET | UNSET | SET TASK_DEFAULTS | UNSET
//!   TASK_DEFAULTS | MODIFY WHEN | REMOVE WHEN | COMMIT | RESUME | SUSPEND`,
//!   `DROP TASK GRAPH`. Offered only where the account has them: `info`
//!   asks with `SHOW TASK GRAPHS IN ACCOUNT LIMIT 1`.
//!
//! Kinds' flags and actions: a `job` starts at its root or retries its last
//! run where it failed (`run_from_step`); its history is Snowflake's and
//! isn't cleared. Alerts and task graph objects run now with an "execute"
//! action (`EXECUTE ALERT`, `EXECUTE TASK GRAPH`). An object another role
//! owns (one the session's roles don't inherit), a native application's or
//! a child task whose root the role doesn't see comes with `can_edit` /
//! `can_delete` false, as its read-only form says.
//!
//! Everything here runs on cloud services (SHOW, DDL, EXECUTE TASK): no
//! statement wakes a warehouse. Only the scheduler's log and the start at a
//! task read the history (`TASK_HISTORY`, `ALERT_HISTORY`), and only with
//! the session's warehouse already running, as the jobs code does.
//!
//! Values come from the app as untrusted text: names are read as Snowflake
//! reads identifiers (unquoted: upper case; `"quoted"`: as written) and
//! always written quoted; texts go in as string literals; the SQL a task
//! runs, its `WHEN` and an alert's condition and action have to be one
//! statement each; ids are parsed back with [`crate::jobs::parse_id`].
//! Snowflake commits each DDL statement on its own, so a save runs the
//! script's statements in order and, when one fails, says which one and
//! resumes the root it suspended. A script of several statements says so in
//! a "⚠" line the app asks the user to accept, as it says that a delete
//! takes the whole graph, that the tasks taken out of a graph are dropped
//! and that a task graph object's COMMIT publishes changes staged before.

mod alert;
mod graph;
mod logs;
mod task;

pub(crate) use logs::{clear_history, logs, run_from, run_now};

use crate::jobs::{self, get, literal, parse_id, text};
use crate::monitor::{quote, Runner, Set};
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormValues, ObjectForm, Result, SchedulerAction,
    SchedulerInfo, SchedulerKind, SchedulerList,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::task::Poll;

pub(crate) type Run<'a> = &'a (dyn Runner + Sync);

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

const VISIBLE_ONLY: &str = "Snowflake muestra solo las tareas, alertas y grafos de tareas sobre los que el rol tiene algún privilegio.";
const VISIBLE_ONLY_NO_GRAPHS: &str =
    "Snowflake muestra solo las tareas y alertas sobre las que el rol tiene algún privilegio.";
const PREVIEW: &str =
    "Los grafos de tareas como objeto (TASK GRAPH) están en vista previa en Snowflake.";
const NO_GRAPHS: &str = "Esta cuenta no tiene grafos de tareas como objeto (TASK GRAPH, en vista previa en Snowflake): los grafos de las tareas están en Tareas.";
const GRAPHS_DENIED: &str = "Snowflake no deja al rol listar los grafos de tareas (TASK GRAPH) de la cuenta: la lista puede quedar vacía o incompleta.";

// What a script warns about ("⚠" lines: the app lists them and asks to
// continue), each a whole sentence of its own for the message catalog.

/// A plan of several statements: each one commits on its own.
pub(crate) const NOT_ATOMIC: &str = "Snowflake confirma cada instrucción por separado: si una falla, las anteriores quedan aplicadas.";
/// The same, for a change that suspended the graph's root.
pub(crate) const NOT_ATOMIC_ROOT: &str = "Snowflake confirma cada instrucción por separado: si una falla, las anteriores quedan aplicadas y DBine reanuda la raíz; si no puede, el grafo queda suspendido.";
/// A task graph object's COMMIT over changes staged before this one.
pub(crate) const PENDING_COMMIT: &str =
    "El grafo de tareas tiene cambios pendientes de antes: el COMMIT también los publica.";
/// Dropping a whole graph (no count: one sentence in every language).
pub(crate) const GRAPH_GOES: &str = "Se borra el grafo entero: la raíz y todas sus tareas.";

/// The note of a task graph object's COMMIT when DBine doesn't know whether
/// it has changes staged before this one (not a "⚠" line: it may have none).
pub(crate) const MAY_PUBLISH_EARLIER: &str = "Los cambios quedan pendientes hasta el COMMIT, que también publica los que hubiera pendientes de antes.";

fn bad(message: String) -> Error {
    Error::Query(message)
}

/// Text from the app, short and on one line, for a message.
pub(crate) fn shown(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(60)
        .collect()
}

/// A sentence on one comment line (a name in it may hold a line break).
fn shown_line(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Kinds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Job,
    Alert,
    Graph,
}

impl Kind {
    fn parse(kind: &str) -> Result<Kind> {
        Ok(match kind.trim() {
            "job" => Kind::Job,
            "alert" => Kind::Alert,
            "task_graph" => Kind::Graph,
            other => {
                return Err(bad(format!(
                    "«{}» no es un tipo de objeto del planificador de Snowflake",
                    shown(other)
                )))
            }
        })
    }
}

/// An object's id: `"DB"."SCHEMA"."NAME"`, refused when it is anything
/// else.
pub(crate) fn checked_id(kind: Kind, id: &str) -> Result<(String, String, String)> {
    parse_id(id).ok_or_else(|| {
        let s = shown(id);
        bad(match kind {
            Kind::Job => format!("«{s}» no es un id de tarea de Snowflake"),
            Kind::Alert => format!("«{s}» no es un id de alerta de Snowflake"),
            Kind::Graph => format!("«{s}» no es un id de grafo de tareas de Snowflake"),
        })
    })
}

/// Whether the account has task graph objects (a preview not every account
/// has), by what `SHOW TASK GRAPHS` answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Graphs {
    /// Listed (or unknown: a failure that says nothing about the account).
    Listed,
    /// The account has them, but the role can't list them.
    Denied,
    /// The account doesn't know the statement or doesn't have the feature.
    Missing,
}

/// The statement that tells whether the account has task graph objects
/// (cloud services: no warehouse).
const GRAPHS_PROBE: &str = "SHOW TASK GRAPHS IN ACCOUNT LIMIT 1";

fn graphs_of(answer: &Result<Set>) -> Graphs {
    match answer {
        Ok(_) => Graphs::Listed,
        Err(e) if jobs::denied(e) => Graphs::Denied,
        Err(e) if missing_feature(e) => Graphs::Missing,
        // A timeout or a lost connection: the kind stays, its list says
        // what happens.
        Err(_) => Graphs::Listed,
    }
}

/// The server refused the statement itself: a syntax it doesn't know or a
/// feature the account doesn't have (`Unsupported feature`, SQLSTATE 0A000
/// or 42000), not a privilege.
fn missing_feature(e: &Error) -> bool {
    if !e.is_query() {
        return false;
    }
    let m = e.to_string().to_ascii_lowercase();
    m.contains("syntax error")
        || m.contains("unsupported feature")
        || m.contains("not supported")
        || m.contains("sqlstate 0a000")
        || m.contains("sqlstate 42000")
}

/// The key of the one action alerts and task graph objects have: run now
/// (`EXECUTE ALERT`, `EXECUTE TASK GRAPH`); the kinds write it as a literal
/// next to its label, for the message extractor. A graph as a job runs from
/// the jobs list ("Ejecutar ahora"), with no action of its own.
pub(crate) const EXECUTE: &str = "execute";

fn info_of(role_note: Option<String>, graphs: Graphs) -> SchedulerInfo {
    let mut notes = Vec::new();
    let mut kinds = vec![
        // A graph starts at its root, or retries its last run from the
        // task where it failed (`run_from`); Snowflake keeps the history
        // itself, and nothing clears it.
        SchedulerKind::new("job", "Tareas", "tarea")
            .abilities(true, true, true)
            .run_from_step(true)
            .clear_history(false),
        SchedulerKind::new("alert", "Alertas", "alerta")
            .abilities(true, true, true)
            .action(SchedulerAction::new("execute", "Ejecutar ahora")),
    ];
    if graphs == Graphs::Missing {
        if role_note.is_some() {
            notes.push(VISIBLE_ONLY_NO_GRAPHS.to_string());
        }
        notes.push(NO_GRAPHS.to_string());
    } else {
        if role_note.is_some() {
            notes.push(VISIBLE_ONLY.to_string());
        }
        if graphs == Graphs::Denied {
            notes.push(GRAPHS_DENIED.to_string());
        }
        notes.push(PREVIEW.to_string());
        kinds.push(
            SchedulerKind::new("task_graph", "Grafos de tareas", "grafo de tareas")
                .abilities(true, true, true)
                .action(SchedulerAction::new("execute", "Ejecutar ahora")),
        );
    }
    SchedulerInfo {
        kinds,
        logs: true,
        notes,
    }
}

// ---------------------------------------------------------------------------
// The contract's calls
// ---------------------------------------------------------------------------

pub(crate) async fn info(r: Run<'_>) -> Result<SchedulerInfo> {
    let (role, graphs) = tokio::join!(jobs::partial_note(r), r.rows(GRAPHS_PROBE));
    Ok(info_of(role, graphs_of(&graphs)))
}

pub(crate) async fn items(r: Run<'_>, kind: &str) -> Result<SchedulerList> {
    match Kind::parse(kind)? {
        Kind::Job => task::items(r).await,
        Kind::Alert => alert::items(r).await,
        Kind::Graph => graph::items(r).await,
    }
}

pub(crate) async fn form(r: Run<'_>, kind: &str, id: Option<&str>) -> Result<ObjectForm> {
    match Kind::parse(kind)? {
        Kind::Job => task::form(r, id).await,
        Kind::Alert => alert::form(r, id).await,
        Kind::Graph => graph::form(r, id).await,
    }
}

async fn plan(r: Run<'_>, kind: Kind, id: Option<&str>, values: &FormValues) -> Result<Plan> {
    match kind {
        Kind::Job => task::plan(r, id, values).await,
        Kind::Alert => alert::plan(r, id, values).await,
        Kind::Graph => graph::plan(r, id, values).await,
    }
}

pub(crate) async fn script(
    r: Run<'_>,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<String> {
    let kind = Kind::parse(kind)?;
    Ok(plan(r, kind, id, values).await?.script())
}

pub(crate) async fn save(
    r: Run<'_>,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Option<String>> {
    let kind = Kind::parse(kind)?;
    let p = plan(r, kind, id, values).await?;
    run_plan(r, &p).await?;
    Ok(p.id)
}

async fn delete_plan(r: Run<'_>, kind: Kind, id: &str) -> Result<Plan> {
    match kind {
        Kind::Job => task::delete_plan(r, id).await,
        Kind::Alert => alert::delete_plan(r, id).await,
        Kind::Graph => graph::delete_plan(r, id).await,
    }
}

pub(crate) async fn delete_script(r: Run<'_>, kind: &str, id: &str) -> Result<String> {
    let kind = Kind::parse(kind)?;
    Ok(delete_plan(r, kind, id).await?.script())
}

pub(crate) async fn delete(r: Run<'_>, kind: &str, id: &str) -> Result<()> {
    let kind = Kind::parse(kind)?;
    let p = delete_plan(r, kind, id).await?;
    run_plan(r, &p).await
}

/// A kind's action on one of its objects: running an alert or a task graph
/// object now. Anything else is refused before anything runs.
async fn action_plan(r: Run<'_>, kind: Kind, id: &str, action: &str) -> Result<Plan> {
    match (kind, action.trim()) {
        (Kind::Alert, EXECUTE) => alert::execute_plan(r, id).await,
        (Kind::Graph, EXECUTE) => graph::execute_plan(r, id).await,
        (_, other) => Err(bad(format!(
            "El planificador de Snowflake no tiene la acción «{}» para este tipo de objeto.",
            shown(other)
        ))),
    }
}

pub(crate) async fn action_script(
    r: Run<'_>,
    kind: &str,
    id: &str,
    action: &str,
) -> Result<String> {
    let kind = Kind::parse(kind)?;
    Ok(action_plan(r, kind, id, action).await?.script())
}

pub(crate) async fn action(r: Run<'_>, kind: &str, id: &str, action: &str) -> Result<()> {
    let kind = Kind::parse(kind)?;
    let p = action_plan(r, kind, id, action).await?;
    run_plan(r, &p).await
}

// ---------------------------------------------------------------------------
// Plans: the statements of a change, shown as a script and run one by one
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Plan {
    /// What the script says first, one comment line each.
    pub(crate) header: Vec<String>,
    /// The statements, in order, without their `;`.
    pub(crate) steps: Vec<String>,
    /// Run when a statement after the first one fails: the resume of the
    /// root the first one suspended.
    pub(crate) restore: Option<String>,
    /// The object's id after the change.
    pub(crate) id: Option<String>,
    /// What the user loses or risks with this change, one whole Spanish
    /// sentence each; the script adds its own about the statements
    /// committing one by one.
    pub(crate) warnings: Vec<String>,
}

impl Plan {
    pub(crate) fn new(id: Option<String>, header: &str) -> Plan {
        Plan {
            header: vec![header.to_string()],
            id,
            ..Plan::default()
        }
    }

    pub(crate) fn note(&mut self, line: &str) {
        self.header.push(line.to_string());
    }

    /// A consequence the user must accept (a "⚠" line of the script).
    pub(crate) fn warn(&mut self, sentence: &str) {
        self.warnings.push(sentence.to_string());
    }

    pub(crate) fn push(&mut self, stmt: String) {
        self.steps.push(stmt);
    }

    /// What the script warns about: the plan's own warnings and, with more
    /// than one statement, that each one commits on its own.
    pub(crate) fn warnings(&self) -> Vec<&str> {
        if self.steps.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<&str> = self.warnings.iter().map(String::as_str).collect();
        if self.steps.len() > 1 {
            out.push(if self.restore.is_some() {
                NOT_ATOMIC_ROOT
            } else {
                NOT_ATOMIC
            });
        }
        out
    }

    /// The script as the designer shows it.
    pub(crate) fn script(&self) -> String {
        let mut out = String::new();
        for h in &self.header {
            for line in h.lines() {
                out.push_str("-- ");
                out.push_str(line.trim_end());
                out.push('\n');
            }
        }
        if self.steps.is_empty() {
            out.push_str("-- No hay cambios que guardar.\n");
            return out;
        }
        for w in self.warnings() {
            out.push_str(&format!("-- ⚠ {}\n", shown_line(w)));
        }
        for s in &self.steps {
            out.push('\n');
            out.push_str(s.trim_end());
            // A statement that ends in a line comment gets its `;` on a line
            // of its own.
            let last = s.trim_end().lines().last().unwrap_or_default();
            if last.contains("--") || last.contains("//") {
                out.push('\n');
            }
            out.push_str(";\n");
        }
        out
    }
}

/// Runs the plan's statements one by one (each its own request): Snowflake
/// commits DDL statement by statement, so a failure says which one stopped
/// the change, and resumes the root the plan suspended.
pub(crate) async fn run_plan(r: Run<'_>, p: &Plan) -> Result<()> {
    let n = p.steps.len();
    for (i, stmt) in p.steps.iter().enumerate() {
        let Err(e) = r.rows(stmt).await else {
            continue;
        };
        if i == 0 {
            return Err(e);
        }
        // Each message whole, so the UI translates it.
        let at = i + 1;
        let msg = match &p.restore {
            None => format!(
                "Snowflake rechazó la instrucción {at} de {n}: {e}. Las anteriores quedaron aplicadas: Snowflake confirma cada instrucción de DDL por separado."
            ),
            Some(restore) => match r.rows(restore).await {
                Ok(_) => format!(
                    "Snowflake rechazó la instrucción {at} de {n}: {e}. Las anteriores quedaron aplicadas (Snowflake confirma cada instrucción de DDL por separado) y la tarea raíz se reanudó."
                ),
                Err(e2) => format!(
                    "Snowflake rechazó la instrucción {at} de {n}: {e}. Las anteriores quedaron aplicadas (Snowflake confirma cada instrucción de DDL por separado) y la tarea raíz quedó suspendida: no se pudo reanudar ({e2})."
                ),
            },
        };
        return Err(Error::Query(msg));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Values from the designer
// ---------------------------------------------------------------------------

/// A form's values (or a list row's) as the app sent them: untrusted, any
/// JSON type where a field expects another.
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

    pub(crate) fn bool(&self, k: &str) -> bool {
        match self.get(k) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            Some(Value::String(s)) => {
                matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "true" | "1" | "yes" | "on"
                )
            }
            _ => false,
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

    /// A list of values (a multi-select's or a multiple Ref's), trimmed,
    /// without empties.
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

    /// One of `options` (its value), `default` when empty.
    pub(crate) fn pick(
        &self,
        k: &str,
        label: &str,
        options: &[&'static str],
        default: Option<&'static str>,
    ) -> Result<Option<&'static str>> {
        let t = self.text(k);
        if t.is_empty() {
            return Ok(default);
        }
        options
            .iter()
            .find(|o| o.eq_ignore_ascii_case(&t))
            .copied()
            .map(Some)
            .ok_or_else(|| {
                bad(format!(
                    "«{label}»: «{}» no es una opción válida.",
                    shown(&t)
                ))
            })
    }
}

/// A list's rows from the designer.
pub(crate) fn rows<'a>(values: &'a FormValues, list: &str) -> Vec<Vals<'a>> {
    values
        .lists
        .get(list)
        .map(|rows| rows.iter().map(Vals).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Identifiers, literals and code
// ---------------------------------------------------------------------------

/// Snowflake reads it unquoted, and stores it as is: upper-case letters,
/// digits, `_` and `$`, not starting with a digit or `$`.
fn plain_upper(name: &str) -> bool {
    let mut c = name.chars();
    c.next().is_some_and(|f| f.is_ascii_uppercase() || f == '_')
        && c.all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_' || ch == '$')
}

/// A name as the form shows it: as is when Snowflake would read it back
/// unquoted, in double quotes otherwise (`"myTask"`).
pub(crate) fn shown_ident(name: &str) -> String {
    if plain_upper(name) {
        name.to_string()
    } else {
        quote(name)
    }
}

/// A name typed in the form, as Snowflake stores it: unquoted, in upper
/// case (letters, digits, `_` and `$`); in double quotes, as written
/// (`""` for a quote inside).
pub(crate) fn ident(text: &str, label: &str) -> Result<String> {
    let t = text.trim();
    if t.is_empty() {
        return Err(bad(format!("«{label}» no puede quedar vacío.")));
    }
    let name = if let Some(inner) = t.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = inner.chars().peekable();
        let mut closed = false;
        while let Some(c) = chars.next() {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    out.push('"');
                }
                '"' => {
                    closed = true;
                    break;
                }
                c => out.push(c),
            }
        }
        if !closed || chars.next().is_some() || out.is_empty() {
            return Err(bad(format!(
                "«{label}»: «{}» no es un nombre entre comillas válido.",
                shown(t)
            )));
        }
        out
    } else {
        let mut c = t.chars();
        let ok = c
            .next()
            .is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
            && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '$');
        if !ok {
            return Err(bad(format!(
                "«{label}»: «{}» no es un nombre válido; si lleva espacios, puntos o símbolos, escribilo entre comillas dobles.",
                shown(t)
            )));
        }
        t.to_ascii_uppercase()
    };
    if name.chars().any(char::is_control) || name.chars().count() > 255 {
        return Err(bad(format!(
            "«{label}»: «{}» no es un nombre válido.",
            shown(t)
        )));
    }
    Ok(name)
}

/// `"DB"."SCHEMA"` back into its two names.
pub(crate) fn parse_schema(v: &str) -> Option<(String, String)> {
    // Two quoted names and nothing else: an id's first two parts.
    parse_id(&format!("{}.\"x\"", v.trim())).map(|(db, s, _)| (db, s))
}

/// `"DB"."SCHEMA"`.
pub(crate) fn schema_id(db: &str, schema: &str) -> String {
    format!("{}.{}", quote(db), quote(schema))
}

/// A qualified name from `task_relations` (`DB.S.T`, parts quoted where
/// they need it) as an id.
pub(crate) fn relation_id(name: &str) -> Option<String> {
    match jobs::name_parts(name).as_slice() {
        [db, s, n] if !db.is_empty() && !s.is_empty() && !n.is_empty() => {
            Some(jobs::job_id(db, s, n))
        }
        _ => None,
    }
}

/// Code from the form as one statement: trimmed, without the final `;`.
/// Snowflake Scripting blocks (`BEGIN … END`, `DECLARE … BEGIN … END`)
/// count as one.
pub(crate) fn one_statement(code: &str, label: &str) -> Result<String> {
    let t = trim_code(code);
    if t.is_empty() {
        return Err(bad(format!("«{label}» no puede quedar vacío.")));
    }
    let n = crate::script::units(&t).len();
    if n != 1 {
        return Err(bad(format!(
            "«{label}» tiene que ser una sola instrucción (o un bloque BEGIN … END): tiene {n}."
        )));
    }
    Ok(t)
}

/// Code as it is compared and written: trimmed, without final `;`.
pub(crate) fn trim_code(code: &str) -> String {
    code.trim()
        .trim_end_matches(|c: char| c == ';' || c.is_whitespace())
        .to_string()
}

/// A boolean expression from the form (a `WHEN`): one expression, no `;`.
pub(crate) fn expression(code: &str, label: &str) -> Result<Option<String>> {
    let t = trim_code(code);
    if t.is_empty() {
        return Ok(None);
    }
    one_statement(&t, label).map(Some)
}

/// A JSON object from the form (a `CONFIG`), as typed.
pub(crate) fn json_object(text: &str, label: &str) -> Result<Option<String>> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    match serde_json::from_str::<Value>(t) {
        Ok(Value::Object(_)) => Ok(Some(t.to_string())),
        Ok(_) => Err(bad(format!(
            "«{label}» tiene que ser un objeto JSON ({{…}})."
        ))),
        Err(e) => Err(bad(format!("«{label}» no es JSON válido: {e}"))),
    }
}

/// Two JSON texts that say the same (the server may reformat it).
pub(crate) fn same_json(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => match (
            serde_json::from_str::<Value>(a),
            serde_json::from_str::<Value>(b),
        ) {
            (Ok(x), Ok(y)) => x == y,
            _ => a.trim() == b.trim(),
        },
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Schedules
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unit {
    Seconds,
    Minutes,
    Hours,
}

impl Unit {
    pub(crate) fn key(self) -> &'static str {
        match self {
            Unit::Seconds => "SECONDS",
            Unit::Minutes => "MINUTES",
            Unit::Hours => "HOURS",
        }
    }

    fn parse(t: &str) -> Option<Unit> {
        match t.trim().to_ascii_uppercase().as_str() {
            "S" | "SECOND" | "SECONDS" => Some(Unit::Seconds),
            "M" | "MINUTE" | "MINUTES" => Some(Unit::Minutes),
            "H" | "HOUR" | "HOURS" => Some(Unit::Hours),
            _ => None,
        }
    }

    /// The interval's range in this unit: 10 seconds to 8 days.
    fn range(self) -> (i64, i64) {
        match self {
            Unit::Seconds => (10, 691_200),
            Unit::Minutes => (1, 11_520),
            Unit::Hours => (1, 192),
        }
    }
}

/// A task's or an alert's `SCHEDULE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Schedule {
    Every(i64, Unit),
    /// The five fields (one space between them) and the time zone.
    Cron(String, String),
}

impl Schedule {
    /// The literal's text: `5 MINUTES`, `USING CRON 0 3 * * * UTC`; alerts
    /// say `MINUTE`.
    pub(crate) fn text(&self, alert: bool) -> String {
        match self {
            Schedule::Every(n, Unit::Minutes) if alert => format!("{n} MINUTE"),
            Schedule::Every(n, u) => format!("{n} {}", u.key()),
            Schedule::Cron(expr, tz) => format!("USING CRON {expr} {tz}"),
        }
    }

    /// `SHOW`'s `schedule` back into a schedule.
    pub(crate) fn parse(s: &str) -> Option<Schedule> {
        let t = s.trim();
        let words: Vec<&str> = t.split_whitespace().collect();
        if words.len() >= 2
            && words[0].eq_ignore_ascii_case("USING")
            && words[1].eq_ignore_ascii_case("CRON")
        {
            let rest = &words[2..];
            if rest.len() != 6 {
                return None;
            }
            return Some(Schedule::Cron(rest[..5].join(" "), rest[5].to_string()));
        }
        let digits: String = t.chars().take_while(char::is_ascii_digit).collect();
        let n: i64 = digits.parse().ok()?;
        let unit = Unit::parse(&t[digits.len()..])?;
        Some(Schedule::Every(n, unit))
    }
}

/// A cron expression's five fields as Snowflake reads them (`*`, numbers,
/// names, `L`, `/`, `,`, `-`, `#`, `?`), one space between them.
pub(crate) fn cron_fields(expr: &str, label: &str) -> Result<String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    let ok = fields.len() == 5
        && fields.iter().all(|f| {
            f.chars()
                .all(|c| c.is_ascii_alphanumeric() || "*/,-#?".contains(c))
        });
    if !ok {
        return Err(bad(format!(
            "«{label}»: «{}» no es una expresión cron de cinco campos (minuto, hora, día del mes, mes, día de la semana).",
            shown(expr)
        )));
    }
    Ok(fields.join(" "))
}

/// A time zone name (`UTC`, `America/Argentina/Buenos_Aires`).
pub(crate) fn time_zone(tz: &str) -> Result<String> {
    let t = tz.trim();
    let ok = !t.is_empty()
        && t.len() <= 64
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_+-/".contains(c));
    if !ok {
        return Err(bad(format!(
            "«{}» no es una zona horaria válida (por ejemplo UTC o America/Argentina/Buenos_Aires).",
            shown(t)
        )));
    }
    Ok(t.to_string())
}

/// The schedule the form says (`schedule` mode, `every` + `unit`, `cron` +
/// `timezone`). Alerts: minutes only.
pub(crate) fn schedule_of(v: Vals, alert: bool) -> Result<Option<Schedule>> {
    match v.text("schedule").as_str() {
        "" | "none" | "new_data" => Ok(None),
        "interval" => {
            let unit = if alert {
                Unit::Minutes
            } else {
                match v.opt("unit") {
                    None => Unit::Minutes,
                    Some(u) => Unit::parse(&u).ok_or_else(|| {
                        bad(format!("«{}» no es una unidad de intervalo.", shown(&u)))
                    })?,
                }
            };
            let (min, max) = unit.range();
            let n = v
                .int("every", "Cada", min, max)?
                .ok_or_else(|| bad("Falta cada cuánto se ejecuta («Cada»).".into()))?;
            Ok(Some(Schedule::Every(n, unit)))
        }
        "cron" => {
            let expr = cron_fields(&v.text("cron"), "Expresión cron")?;
            let tz = time_zone(&v.text("timezone"))?;
            Ok(Some(Schedule::Cron(expr, tz)))
        }
        other => Err(bad(format!(
            "«{}» no es un tipo de programación.",
            shown(other)
        ))),
    }
}

/// A schedule's form values.
pub(crate) fn schedule_values(
    s: Option<&Schedule>,
    none: &str,
    values: &mut BTreeMap<String, Value>,
) {
    let (mode, every, unit, cron, tz) = match s {
        None => (none, json!(""), "MINUTES", String::new(), "UTC".to_string()),
        Some(Schedule::Every(n, u)) => ("interval", json!(n), u.key(), String::new(), "UTC".into()),
        Some(Schedule::Cron(e, tz)) => ("cron", json!(""), "MINUTES", e.clone(), tz.clone()),
    };
    values.insert("schedule".into(), json!(mode));
    values.insert("every".into(), every);
    values.insert("unit".into(), json!(unit));
    values.insert("cron".into(), json!(cron));
    values.insert("timezone".into(), json!(tz));
}

/// The schedule's fields, shown by mode.
pub(crate) fn schedule_fields(task: bool) -> Vec<FormField> {
    let mut f = vec![FormField::new(
        "every",
        "Cada",
        FormKind::number(Some(1.0), Some(691_200.0)),
    )
    .when("schedule", &["interval"])
    .help(if task {
        "De 10 segundos a 8 días."
    } else {
        "En minutos: de 1 a 11520 (8 días)."
    })];
    if task {
        f.push(
            FormField::new(
                "unit",
                "Unidad",
                FormKind::select(&[
                    ("SECONDS", "Segundos"),
                    ("MINUTES", "Minutos"),
                    ("HOURS", "Horas"),
                ]),
            )
            .when("schedule", &["interval"])
            .default(json!("MINUTES")),
        );
    }
    f.push(
        FormField::new("cron", "Expresión cron", FormKind::Cron)
            .when("schedule", &["cron"])
            .placeholder("0 3 * * *")
            .help("Minuto, hora, día del mes, mes y día de la semana; admite L (último) y /n."),
    );
    f.push(
        FormField::new("timezone", "Zona horaria", FormKind::Text)
            .when("schedule", &["cron"])
            .placeholder("America/Argentina/Buenos_Aires")
            .default(json!("UTC")),
    );
    f
}

// ---------------------------------------------------------------------------
// Sizes, levels, overlap
// ---------------------------------------------------------------------------

pub(crate) const SIZES: [&str; 6] = ["XSMALL", "SMALL", "MEDIUM", "LARGE", "XLARGE", "XXLARGE"];
pub(crate) const LOG_LEVELS: [&str; 7] =
    ["TRACE", "DEBUG", "INFO", "WARN", "ERROR", "FATAL", "OFF"];
pub(crate) const TRACE_LEVELS: [&str; 4] = ["ALWAYS", "ON_EVENT", "PROPAGATE", "OFF"];
pub(crate) const OVERLAPS: [&str; 3] = ["NO_OVERLAP", "ALLOW_CHILD_OVERLAP", "ALLOW_ALL_OVERLAP"];

pub(crate) fn size_select() -> FormKind {
    FormKind::select(&[
        ("XSMALL", "X-Small"),
        ("SMALL", "Small"),
        ("MEDIUM", "Medium"),
        ("LARGE", "Large"),
        ("XLARGE", "X-Large"),
        ("XXLARGE", "2X-Large"),
    ])
}

pub(crate) fn log_select() -> FormKind {
    FormKind::select(&[
        ("TRACE", "TRACE"),
        ("DEBUG", "DEBUG"),
        ("INFO", "INFO"),
        ("WARN", "WARN"),
        ("ERROR", "ERROR"),
        ("FATAL", "FATAL"),
        ("OFF", "OFF"),
    ])
}

pub(crate) fn trace_select() -> FormKind {
    FormKind::select(&[
        ("ALWAYS", "ALWAYS"),
        ("ON_EVENT", "ON_EVENT"),
        ("PROPAGATE", "PROPAGATE"),
        ("OFF", "OFF"),
    ])
}

pub(crate) fn overlap_select() -> FormKind {
    FormKind::select(&[
        (
            "NO_OVERLAP",
            "Sin superposición: una ejecución del grafo por vez",
        ),
        (
            "ALLOW_CHILD_OVERLAP",
            "Las hijas pueden superponerse con la ejecución siguiente",
        ),
        ("ALLOW_ALL_OVERLAP", "Todo el grafo puede superponerse"),
    ])
}

/// A warehouse size as Snowflake shows it (`X-Small`, `X2LARGE`…) as the
/// form's option.
pub(crate) fn size_key(v: &str) -> String {
    let k: String = v
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase();
    match k.as_str() {
        "X2LARGE" | "2XLARGE" => "XXLARGE".into(),
        "XXSMALL" => "XSMALL".into(),
        _ => k,
    }
}

/// A task's `TARGET_COMPLETION_INTERVAL` (`10 MINUTES`): 10 seconds to a
/// day.
pub(crate) fn completion_interval(t: &str, label: &str) -> Result<Option<String>> {
    let t = t.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let wrong = || {
        bad(format!(
            "«{label}»: «{}» no es un intervalo como «10 MINUTES» (de 10 segundos a 24 horas).",
            shown(t)
        ))
    };
    match Schedule::parse(t) {
        Some(Schedule::Every(n, u)) => {
            let secs = match u {
                Unit::Seconds => n,
                Unit::Minutes => n * 60,
                Unit::Hours => n * 3600,
            };
            if !(10..=86_400).contains(&secs) || n < 1 {
                return Err(wrong());
            }
            Ok(Some(format!("{n} {}", u.key())))
        }
        _ => Err(wrong()),
    }
}

// ---------------------------------------------------------------------------
// What the forms pick from
// ---------------------------------------------------------------------------

/// The server's objects the forms' `Ref` fields list, by source, and the
/// sources that couldn't be read (anything well formed is taken then).
#[derive(Debug, Clone, Default)]
pub(crate) struct Choices {
    pub(crate) lists: BTreeMap<String, Vec<(String, String)>>,
    pub(crate) failed: BTreeSet<String>,
    /// Session parameters' types (`STRING`, `NUMBER`, `BOOLEAN`), by name.
    pub(crate) param_types: HashMap<String, String>,
}

impl Choices {
    /// Adds a value the object has now, so the form shows it even when the
    /// list doesn't have it.
    pub(crate) fn keep(&mut self, source: &str, value: &str, label: &str) {
        let l = self.lists.entry(source.to_string()).or_default();
        if !value.is_empty() && !l.iter().any(|(v, _)| v == value) {
            l.push((value.to_string(), label.to_string()));
        }
    }

    /// A value picked from a list: one of them (the server's spelling, any
    /// case), or anything when the list couldn't be read.
    pub(crate) fn pick(&self, source: &str, value: &str, label: &str) -> Result<String> {
        let list = self.lists.get(source).map(Vec::as_slice).unwrap_or(&[]);
        if let Some((v, _)) = list
            .iter()
            .find(|(v, _)| v == value)
            .or_else(|| list.iter().find(|(v, _)| v.eq_ignore_ascii_case(value)))
        {
            return Ok(v.clone());
        }
        if self.failed.contains(source) {
            return Ok(value.to_string());
        }
        Err(bad(format!(
            "«{label}»: «{}» no está entre los que el rol ve en Snowflake.",
            shown(value)
        )))
    }
}

/// `(source, statement)`: what each source lists.
fn source_sql(source: &str) -> Option<&'static str> {
    Some(match source {
        "warehouse" => "SHOW WAREHOUSES",
        "notification_integration" => "SHOW NOTIFICATION INTEGRATIONS",
        "schema" => "SHOW SCHEMAS IN ACCOUNT LIMIT 10000",
        "session_parameter" => "SHOW PARAMETERS IN SESSION",
        "task" => "SHOW TASKS IN ACCOUNT LIMIT 10000",
        _ => return None,
    })
}

fn source_rows(source: &str, s: &Set, c: &mut Choices) {
    let mut out = Vec::new();
    for i in 0..s.rows.len() {
        match source {
            "schema" => {
                let (Some(db), Some(n)) = (text(s, i, "database_name"), text(s, i, "name")) else {
                    continue;
                };
                if n.eq_ignore_ascii_case("INFORMATION_SCHEMA") {
                    continue;
                }
                out.push((schema_id(&db, &n), format!("{db}.{n}")));
            }
            "task" => {
                let (Some(db), Some(sc), Some(n)) = (
                    text(s, i, "database_name"),
                    text(s, i, "schema_name"),
                    text(s, i, "name"),
                ) else {
                    continue;
                };
                out.push((jobs::job_id(&db, &sc, &n), format!("{db}.{sc}.{n}")));
            }
            "session_parameter" => {
                let Some(k) = text(s, i, "key") else { continue };
                let k = k.to_ascii_uppercase();
                if task::OWN_PARAMS.contains(&k.as_str()) {
                    continue;
                }
                if let Some(t) = text(s, i, "type") {
                    c.param_types.insert(k.clone(), t.to_ascii_uppercase());
                }
                out.push((k.clone(), k));
            }
            _ => {
                let Some(n) = text(s, i, "name") else {
                    continue;
                };
                out.push((n.clone(), n));
            }
        }
    }
    out.sort_by(|a, b| a.1.cmp(&b.1));
    c.lists.insert(source.to_string(), out);
}

/// Reads the sources the form needs, together.
pub(crate) async fn load(r: Run<'_>, sources: &[&str]) -> Choices {
    let wanted: Vec<(&str, &'static str)> = sources
        .iter()
        .filter_map(|s| Some((*s, source_sql(s)?)))
        .collect();
    let answers = together(wanted.iter().map(|(_, sql)| r.rows(sql)).collect()).await;
    let mut c = Choices::default();
    for ((source, _), a) in wanted.iter().zip(answers) {
        match a {
            Ok(s) => source_rows(source, &s, &mut c),
            Err(e) => {
                tracing::debug!("snowflake scheduler: {source} not listed: {e}");
                c.failed.insert(source.to_string());
                c.lists.entry(source.to_string()).or_default();
            }
        }
    }
    c
}

/// Whether the session holds each role (`IS_ROLE_IN_SESSION`: the current
/// role and the ones it inherits, secondary roles too). A role whose check
/// fails counts as held: the server refuses what it doesn't own anyway.
pub(crate) async fn held(r: Run<'_>, roles: &[String]) -> HashMap<String, bool> {
    let roles: Vec<&String> = roles
        .iter()
        .filter(|r| !r.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut out: HashMap<String, bool> = roles.iter().map(|r| ((*r).clone(), true)).collect();
    if roles.is_empty() {
        return out;
    }
    let cols: Vec<String> = roles
        .iter()
        .enumerate()
        .map(|(i, role)| format!("IS_ROLE_IN_SESSION({}) AS r{i}", literal(role)))
        .collect();
    match r.rows(&format!("SELECT {}", cols.join(", "))).await {
        Ok(s) => {
            for (i, role) in roles.iter().enumerate() {
                if let Some(v) = get(&s, 0, &format!("r{i}")) {
                    out.insert((*role).clone(), v.eq_ignore_ascii_case("true") || v == "1");
                }
            }
        }
        Err(e) => tracing::debug!("snowflake scheduler: IS_ROLE_IN_SESSION: {e}"),
    }
    out
}

/// The session's warehouse, running: what reading the history needs.
pub(crate) async fn running_warehouse(r: Run<'_>, what: &str) -> Result<()> {
    match jobs::warehouse(r).await? {
        jobs::Warehouse::Running => Ok(()),
        jobs::Warehouse::Suspended(wh) => Err(bad(format!(
            "el warehouse {wh} está suspendido: {what} no lo reanuda para no consumir créditos"
        ))),
        jobs::Warehouse::Missing => Err(bad(format!(
            "Snowflake necesita un warehouse para {what}: la conexión no tiene uno"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Running statements together
// ---------------------------------------------------------------------------

/// Requests in flight at once, at most.
const PARALLEL: usize = 8;

/// The futures' outputs in their order, running `PARALLEL` at a time (the
/// SQL API takes each request on its own).
pub(crate) async fn together<F: Future>(futs: Vec<F>) -> Vec<F::Output> {
    let mut out = Vec::with_capacity(futs.len());
    let mut it = futs.into_iter();
    loop {
        let chunk: Vec<F> = it.by_ref().take(PARALLEL).collect();
        if chunk.is_empty() {
            break;
        }
        out.extend(join(chunk).await);
    }
    out
}

async fn join<F: Future>(futs: Vec<F>) -> Vec<F::Output> {
    let mut futs: Vec<Pin<Box<F>>> = futs.into_iter().map(Box::pin).collect();
    let mut done: Vec<Option<F::Output>> = futs.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (f, d) in futs.iter_mut().zip(done.iter_mut()) {
            if d.is_none() {
                match f.as_mut().poll(cx) {
                    Poll::Ready(v) => *d = Some(v),
                    Poll::Pending => pending = true,
                }
            }
        }
        if pending {
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })
    .await;
    done.into_iter().flatten().collect()
}

// ---------------------------------------------------------------------------
// Session parameters
// ---------------------------------------------------------------------------

/// A session parameter's value as written in `SET`: by its type (numbers
/// and booleans bare, the rest as a literal).
pub(crate) fn param_value(name: &str, value: &str, kind: Option<&str>) -> Result<String> {
    let v = value.trim();
    match kind.unwrap_or("STRING") {
        "NUMBER" | "INTEGER" | "FLOAT" => {
            if v.parse::<f64>().is_ok_and(f64::is_finite) {
                Ok(v.to_string())
            } else {
                Err(bad(format!(
                    "El parámetro {name} lleva un número: «{}» no lo es.",
                    shown(v)
                )))
            }
        }
        "BOOLEAN" => match v.to_ascii_lowercase().as_str() {
            "true" => Ok("TRUE".into()),
            "false" => Ok("FALSE".into()),
            _ => Err(bad(format!(
                "El parámetro {name} lleva TRUE o FALSE: «{}» no lo es.",
                shown(v)
            ))),
        },
        _ => Ok(literal(v)),
    }
}

/// A literal's text (`'it''s'` → `it's`); anything else as is.
pub(crate) fn unliteral(v: &str) -> String {
    match v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
        Some(x) if v.len() >= 2 => x.replace("''", "'").replace("\\\\", "\\"),
        _ => v.to_string(),
    }
}

/// A parameter's name typed in the form: an identifier, upper case.
pub(crate) fn param_name(t: &str) -> Result<String> {
    let t = t.trim();
    let mut c = t.chars();
    let ok = c
        .next()
        .is_some_and(|f| f.is_ascii_alphabetic() || f == '_')
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
        && t.len() <= 255;
    if !ok {
        return Err(bad(format!(
            "«{}» no es un nombre de parámetro de Snowflake.",
            shown(t)
        )));
    }
    Ok(t.to_ascii_uppercase())
}

/// The session parameters' list (`session_params`, rows of `name` and
/// `value`).
pub(crate) fn params_list(rows: Vec<BTreeMap<String, Value>>, read_only: bool) -> FormList {
    FormList::new(
        "session_params",
        "Parámetros de sesión",
        "name",
        vec![
            FormField::new(
                "name",
                "Parámetro",
                FormKind::reference("session_parameter", false),
            )
            .required(),
            FormField::new("value", "Valor", FormKind::Text).required(),
        ],
    )
    .rows(rows)
    .read_only(read_only)
}

/// The parameters the form's list says, each `(NAME, value as written)`,
/// checked against the session's parameters.
pub(crate) fn params_of(
    values: &FormValues,
    choices: &Choices,
    known: &HashMap<String, String>,
) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    for row in rows(values, "session_params") {
        let raw = row.text("name");
        if raw.is_empty() && row.text("value").is_empty() {
            continue;
        }
        let name = param_name(&raw)?;
        param_checked(&name, choices, known)?;
        if out.iter().any(|(n, _)| *n == name) {
            return Err(bad(format!("El parámetro {name} está dos veces.")));
        }
        let kind = choices
            .param_types
            .get(&name)
            .or_else(|| known.get(&name))
            .map(String::as_str);
        let v = param_value(&name, &row.raw("value"), kind)?;
        out.push((name, v));
    }
    Ok(out)
}

/// A parameter the list may set: a session parameter, not one with a field
/// of its own.
pub(crate) fn param_checked(
    name: &str,
    choices: &Choices,
    known: &HashMap<String, String>,
) -> Result<()> {
    if task::OWN_PARAMS.contains(&name) {
        return Err(bad(format!(
            "El parámetro {name} tiene su propio campo en el formulario."
        )));
    }
    let listed = choices
        .lists
        .get("session_parameter")
        .is_some_and(|l| l.iter().any(|(v, _)| v == name));
    if listed || known.contains_key(name) || choices.failed.contains("session_parameter") {
        Ok(())
    } else {
        Err(bad(format!(
            "{name} no es un parámetro de sesión de Snowflake."
        )))
    }
}

/// `NAME = value` lines (a child task's parameters) into pairs.
pub(crate) fn params_text(
    text: &str,
    choices: &Choices,
    known: &HashMap<String, String>,
) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (n, v) = line.split_once('=').ok_or_else(|| {
            bad(format!(
                "«{}»: cada parámetro va en su línea como NOMBRE = valor.",
                shown(line)
            ))
        })?;
        let name = param_name(n)?;
        param_checked(&name, choices, known)?;
        if out.iter().any(|(x, _)| *x == name) {
            return Err(bad(format!("El parámetro {name} está dos veces.")));
        }
        // A quoted value is a literal already: its text without the quotes.
        let unq = unliteral(v.trim());
        let kind = choices
            .param_types
            .get(&name)
            .or_else(|| known.get(&name))
            .map(String::as_str);
        out.push((name.clone(), param_value(&name, &unq, kind)?));
    }
    Ok(out)
}

#[cfg(test)]
mod tests;
