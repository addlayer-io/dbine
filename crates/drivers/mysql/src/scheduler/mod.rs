//! The server's scheduler managed whole through the scheduler contract
//! ([`dbine_driver::Session::scheduler_info`] and the rest), per variant:
//!
//! - MySQL and MariaDB (Percona, Aurora, Cloud SQL, Azure) and OceanBase in
//!   MySQL mode: the Event Scheduler's events (see [`events`]), and on
//!   MySQL 8.0.22+ the scheduler's lines of the server's error log.
//! - Doris and VeloDB: `CREATE JOB` jobs, scheduled or streaming, and the
//!   refresh settings of asynchronous materialized views (see [`doris`]).
//! - StarRocks: tasks (`SUBMIT TASK`) and the refresh settings of
//!   asynchronous materialized views (see [`starrocks`]).
//!
//! The "job" kind's ids are the ones `jobs()` hands out, so a job row of
//! the tree opens its designer; a view's refresh task listed among the jobs
//! (`` mv:`…` ``) opens the same form as under the "mv_refresh" kind.
//!
//! - Scripts: every save is the SQL the script shows, run one statement at
//!   a time (`DELIMITER` only frames a compound statement in the shown
//!   script). An edit is a diff against what the server has now: only the
//!   clauses that changed, in place, where the engine has a statement for
//!   it; where it can only drop and create again (a Doris scheduled job, a
//!   StarRocks task's schedule or statement) the script says so at the top,
//!   with what is lost, and a failed create puts the old one back. A
//!   statement the object runs (an event's, a job's, a task's) has to be
//!   one for the server: the session runs with multi-statements on, so a
//!   `;` would end the object's statement and run the rest at once.
//! - With autocommit off, a change is refused while the session has a
//!   transaction open (the DDL would commit it); the transaction the
//!   scheduler's own catalog reads open is rolled back after them.
//! - Values and ids come from the app as untrusted text: ids are parsed
//!   back into the names they quote and looked up on the server, names go
//!   into statements quoted as identifiers, texts as string literals, and
//!   times, intervals, units, zones, SQL modes and property keys are
//!   checked before they reach a script. Names picked from a list (a
//!   database, a definer) have to be the server's.

mod doris;
mod events;
mod starrocks;
#[cfg(test)]
mod tests;

use crate::jobs::clock::Civil;
use crate::session::{at, lit, MySqlSession};
use crate::Variant;
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormValues, MonitorTable, ObjectForm, Result,
    SchedulerInfo, SchedulerList, Session, TxState,
};
use mysql_async::prelude::Queryable;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// The contract's calls, per variant
// ---------------------------------------------------------------------------

fn unsupported(v: Variant) -> Error {
    crate::jobs::unsupported(v)
}

pub(crate) async fn info(s: &mut MySqlSession) -> Result<SchedulerInfo> {
    let had = in_transaction(s).await;
    let out = match s.variant {
        Variant::MySql | Variant::MariaDb | Variant::OceanBase => events::info(s).await,
        Variant::Doris => doris::info(s).await,
        Variant::StarRocks => starrocks::info(s).await,
        v => Err(unsupported(v)),
    };
    end_own_reads(s, had).await;
    out
}

pub(crate) async fn items(s: &mut MySqlSession, kind: &str) -> Result<SchedulerList> {
    let had = in_transaction(s).await;
    let out = match s.variant {
        Variant::MySql | Variant::MariaDb | Variant::OceanBase => events::items(s, kind).await,
        Variant::Doris => doris::items(s, kind).await,
        Variant::StarRocks => starrocks::items(s, kind).await,
        v => Err(unsupported(v)),
    };
    end_own_reads(s, had).await;
    out
}

pub(crate) async fn form(s: &mut MySqlSession, kind: &str, id: Option<&str>) -> Result<ObjectForm> {
    let had = in_transaction(s).await;
    let out = match s.variant {
        Variant::MySql | Variant::MariaDb | Variant::OceanBase => events::form(s, kind, id).await,
        Variant::Doris => doris::form(s, kind, id).await,
        Variant::StarRocks => starrocks::form(s, kind, id).await,
        v => Err(unsupported(v)),
    };
    end_own_reads(s, had).await;
    out
}

async fn plan(
    s: &mut MySqlSession,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    match s.variant {
        Variant::MySql | Variant::MariaDb | Variant::OceanBase => {
            events::plan(s, kind, id, values).await
        }
        Variant::Doris => doris::plan(s, kind, id, values).await,
        Variant::StarRocks => starrocks::plan(s, kind, id, values).await,
        v => Err(unsupported(v)),
    }
}

pub(crate) async fn script(
    s: &mut MySqlSession,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<String> {
    let had = in_transaction(s).await;
    let planned = plan(s, kind, id, values).await;
    end_own_reads(s, had).await;
    Ok(planned?.script())
}

pub(crate) async fn save(
    s: &mut MySqlSession,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Option<String>> {
    // The session's own transaction, read before the plan's catalog reads
    // (which open one when autocommit is off).
    let had = open_transaction(s).await?;
    let planned = plan(s, kind, id, values).await;
    end_own_reads(s, had).await;
    let mut plan = planned?;
    if plan.empty {
        return Ok(plan.id);
    }
    if had {
        return Err(Error::State(OPEN_TRANSACTION.into()));
    }
    back_to_database(s, &mut plan).await;
    run(s, &plan).await?;
    Ok(plan.id)
}

/// A script that moves into another database (`USE`) moves the session
/// back afterwards.
async fn back_to_database(s: &mut MySqlSession, plan: &mut Plan) {
    let moves = plan
        .steps
        .iter()
        .chain(&plan.undo)
        .any(|st| st.starts_with("USE "));
    if moves {
        if let Some(db) = s.current_database().await {
            plan.finally
                .push(format!("USE {}", crate::jobs::quoted(&db)));
        }
    }
}

async fn delete_plan(s: &mut MySqlSession, kind: &str, id: &str) -> Result<Plan> {
    match s.variant {
        Variant::MySql | Variant::MariaDb | Variant::OceanBase => {
            events::delete_plan(s, kind, id).await
        }
        Variant::Doris => doris::delete_plan(s, kind, id).await,
        Variant::StarRocks => starrocks::delete_plan(s, kind, id).await,
        v => Err(unsupported(v)),
    }
}

pub(crate) async fn delete_script(s: &mut MySqlSession, kind: &str, id: &str) -> Result<String> {
    let had = in_transaction(s).await;
    let planned = delete_plan(s, kind, id).await;
    end_own_reads(s, had).await;
    Ok(planned?.script())
}

pub(crate) async fn delete(s: &mut MySqlSession, kind: &str, id: &str) -> Result<()> {
    let had = open_transaction(s).await?;
    let planned = delete_plan(s, kind, id).await;
    end_own_reads(s, had).await;
    let mut plan = planned?;
    if had {
        return Err(Error::State(OPEN_TRANSACTION.into()));
    }
    back_to_database(s, &mut plan).await;
    run(s, &plan).await
}

pub(crate) async fn logs(s: &mut MySqlSession) -> Result<Vec<MonitorTable>> {
    let had = in_transaction(s).await;
    let out = match s.variant {
        Variant::MySql | Variant::MariaDb | Variant::OceanBase => events::logs(s).await,
        Variant::Doris | Variant::StarRocks => Err(Error::Unsupported(
            "este motor no tiene un registro de su planificador".into(),
        )),
        v => Err(unsupported(v)),
    };
    end_own_reads(s, had).await;
    out
}

/// The session has a transaction open (MySQL, MariaDB and OceanBase): a
/// statement of the scheduler (DDL) would commit it, so a change is
/// refused while it's open.
async fn open_transaction(s: &mut MySqlSession) -> Result<bool> {
    if !s.variant.has_transactions() {
        return Ok(false);
    }
    Ok(s.transaction_state().await? == Some(TxState::Open))
}

/// As [`open_transaction`]; when it can't be read, as open (nothing of the
/// session's is then ended by [`end_own_reads`]).
async fn in_transaction(s: &mut MySqlSession) -> bool {
    open_transaction(s).await.unwrap_or(true)
}

/// With autocommit off, the scheduler's catalog reads (InnoDB tables, the
/// data dictionary's views) open a transaction. One the session didn't
/// have before them (`had`) holds only those reads: it's rolled back, so
/// that it neither stays open nor stops the next save as the user's.
async fn end_own_reads(s: &mut MySqlSession, had: bool) {
    if had || !open_transaction(s).await.unwrap_or(false) {
        return;
    }
    if let Err(e) = s.conn.query_drop("ROLLBACK").await {
        tracing::debug!(
            "{:?} scheduler: ending its reads' transaction: {e}",
            s.variant
        );
    }
}

const OPEN_TRANSACTION: &str = "Hay una transacción abierta en esta sesión: confirmala o deshacela antes de cambiar el planificador (el servidor la confirmaría con el cambio).";

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

pub(crate) fn bad(message: String) -> Error {
    Error::Query(message)
}

/// Text from the app, short and on one line, for a message.
pub(crate) fn shown(s: &str) -> String {
    one_line(s.trim()).chars().take(60).collect()
}

/// Text for a `-- ` comment line: no line break or other control character
/// can end the comment and turn the rest into a statement.
pub(crate) fn one_line(s: &str) -> String {
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

/// The error for a kind this engine's scheduler doesn't have.
pub(crate) fn no_kind(kind: &str) -> Error {
    bad(format!(
        "«{}» no es un tipo de objeto del planificador de este servidor",
        shown(kind)
    ))
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

    /// The value as typed (statements keep their spaces).
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

    pub(crate) fn bool(&self, k: &str, default: bool) -> bool {
        match self.get(k) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => true,
                "false" | "0" | "no" | "off" => false,
                _ => default,
            },
            _ => default,
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

    /// One of `options` (the select's values); `default` when empty.
    pub(crate) fn choice(
        &self,
        k: &str,
        label: &str,
        options: &[&'static str],
        default: &'static str,
    ) -> Result<&'static str> {
        let t = self.text(k);
        if t.is_empty() {
            return Ok(default);
        }
        options
            .iter()
            .find(|o| o.eq_ignore_ascii_case(&t))
            .copied()
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

/// A time from the form (`YYYY-MM-DDTHH:MM:SS`, also with a space, without
/// seconds or with fractions) as the engines take it in a literal
/// (`YYYY-MM-DD HH:MM:SS`); `None` when empty.
pub(crate) fn datetime(v: Vals, k: &str, label: &str) -> Result<Option<String>> {
    let Some(t) = v.opt(k) else {
        return Ok(None);
    };
    parse_datetime(&t).map(Some).ok_or_else(|| {
        bad(format!(
            "«{label}»: «{}» no es una fecha y hora válida (AAAA-MM-DD HH:MM:SS).",
            shown(&t)
        ))
    })
}

pub(crate) fn parse_datetime(t: &str) -> Option<String> {
    let t = t.trim();
    let ok = t.len() >= 10
        && t.as_bytes()[..10].iter().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                *b == b'-'
            } else {
                b.is_ascii_digit()
            }
        })
        && t.bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b':' | b' ' | b'T' | b'.' | b'Z'));
    if !ok {
        return None;
    }
    let c = Civil::parse(t)?;
    Some(c.sql())
}

/// A time the server printed, as the form shows it (`YYYY-MM-DDTHH:MM:SS`).
pub(crate) fn form_time(t: Option<&str>) -> Value {
    json!(t
        .and_then(Civil::parse)
        .map(|c| c.sql().replacen(' ', "T", 1))
        .unwrap_or_default())
}

/// A name (of an object, a database) from the form: required, at most
/// `max` characters, no control characters, not ending in a space.
pub(crate) fn name(v: Vals, k: &str, label: &str, max: usize) -> Result<String> {
    let n = v.text(k);
    if n.is_empty() {
        return Err(bad(format!("«{label}» es obligatorio.")));
    }
    if n.chars().count() > max {
        return Err(bad(format!("«{label}» admite hasta {max} caracteres.")));
    }
    if n.chars().any(char::is_control) {
        return Err(bad(format!(
            "«{label}» no puede tener saltos de línea ni caracteres de control."
        )));
    }
    Ok(n)
}

/// A name the user picked from a list of the server's: it has to be one of
/// them (any case). Returns the server's spelling.
pub(crate) fn pick<'a>(
    name: &str,
    choices: &'a [(String, String)],
    missing: impl FnOnce(&str) -> String,
) -> Result<&'a str> {
    choices
        .iter()
        .find(|(v, _)| v == name)
        .or_else(|| choices.iter().find(|(v, _)| v.eq_ignore_ascii_case(name)))
        .map(|(v, _)| v.as_str())
        .ok_or_else(|| bad(missing(&shown(name))))
}

// ---------------------------------------------------------------------------
// Properties (`PROPERTIES ('k' = 'v')`): a key/value list
// ---------------------------------------------------------------------------

/// The list of an object's properties; `help` (a sentence) says which keys
/// the engine takes.
pub(crate) fn properties_list(rows: &[(String, String)], help: &str) -> FormList {
    FormList::new(
        "properties",
        "Propiedades",
        "key",
        vec![
            FormField::new("key", "Clave", FormKind::Text)
                .required()
                .help(help),
            FormField::new("value", "Valor", FormKind::Text),
        ],
    )
    .rows(
        rows.iter()
            .map(|(k, v)| {
                BTreeMap::from([
                    ("key".to_string(), json!(k)),
                    ("value".to_string(), json!(v)),
                ])
            })
            .collect(),
    )
}

/// The properties the form sends: keys of letters, digits, `_`, `.` and
/// `-`, each once.
pub(crate) fn read_properties(values: &FormValues) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    for r in rows(values, "properties") {
        let k = r.text("key");
        if k.is_empty() {
            continue;
        }
        if k.len() > 128
            || !k
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        {
            return Err(bad(format!(
                "«{}» no es una clave de propiedad válida (letras, números, _, . y -).",
                shown(&k)
            )));
        }
        if out.iter().any(|(o, _)| o.eq_ignore_ascii_case(&k)) {
            return Err(bad(format!("La propiedad «{k}» está repetida.")));
        }
        let v = r.raw("value");
        if v.chars().any(|c| c.is_control() && c != '\t') {
            return Err(bad(format!(
                "El valor de la propiedad «{k}» no puede tener saltos de línea."
            )));
        }
        out.push((k, v));
    }
    Ok(out)
}

/// What changes from `current` to `wanted`: the keys added or with another
/// value (in `wanted`'s order), and the keys taken away.
pub(crate) fn property_changes(
    current: &[(String, String)],
    wanted: &[(String, String)],
) -> (Vec<(String, String)>, Vec<String>) {
    let set = wanted
        .iter()
        .filter(|(k, v)| !current.iter().any(|(ck, cv)| ck == k && cv == v))
        .cloned()
        .collect();
    let removed = current
        .iter()
        .filter(|(k, _)| !wanted.iter().any(|(w, _)| w == k))
        .map(|(k, _)| k.clone())
        .collect();
    (set, removed)
}

/// `('k1' = 'v1', 'k2' = 'v2')`.
pub(crate) fn properties_sql(props: &[(String, String)]) -> String {
    let items: Vec<String> = props
        .iter()
        .map(|(k, v)| format!("{} = {}", lit(k), lit(v)))
        .collect();
    format!("({})", items.join(", "))
}

// ---------------------------------------------------------------------------
// Lists of the server's
// ---------------------------------------------------------------------------

/// The databases the user sees, but the server's own catalogs.
pub(crate) async fn databases(s: &mut MySqlSession) -> Vec<(String, String)> {
    let skip = [
        "information_schema",
        "performance_schema",
        "__internal_schema",
        "_statistics_",
        "oceanbase",
        "__recyclebin",
        "__public",
    ];
    let mut out: Vec<(String, String)> = s
        .optional_rows("SHOW DATABASES")
        .await
        .iter()
        .filter_map(|r| at(r, 0))
        .filter(|d| !skip.iter().any(|x| x.eq_ignore_ascii_case(d)))
        .map(|d| (d.clone(), d))
        .collect();
    out.sort();
    out.dedup();
    out
}

// ---------------------------------------------------------------------------
// Scripts and running them
// ---------------------------------------------------------------------------

/// What a change writes and runs.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Plan {
    /// Comment lines at the top of the script.
    pub(crate) head: Vec<String>,
    /// The statements, in order (the first failure stops them).
    pub(crate) steps: Vec<String>,
    /// Statements that run after the steps whatever happened: the
    /// session's SQL mode and zone, its database, put back.
    pub(crate) finally: Vec<String>,
    /// The step that leaves the object half changed (dropped to be created
    /// again, paused, its schedule cleared): a later step that fails runs
    /// `undo`, which puts the object back as it was.
    pub(crate) undo_from: Option<usize>,
    pub(crate) undo: Vec<String>,
    /// Nothing differs from what the server has.
    pub(crate) empty: bool,
    /// The object's id once saved.
    pub(crate) id: Option<String>,
}

impl Plan {
    pub(crate) fn nothing(name: &str, id: String) -> Plan {
        Plan {
            head: vec![format!("«{}»: no hay cambios que guardar.", one_line(name))],
            empty: true,
            id: Some(id),
            ..Plan::default()
        }
    }

    /// The script as the editor runs it.
    pub(crate) fn script(&self) -> String {
        let mut out = String::new();
        for h in &self.head {
            let _ = writeln!(out, "-- {}", one_line(h));
        }
        for st in self.steps.iter().chain(&self.finally) {
            push_statement(&mut out, st);
        }
        out
    }
}

/// A statement and its end: `;`, or between `DELIMITER`s when it holds one
/// (a `BEGIN … END` body, a `;` in a string).
pub(crate) fn push_statement(out: &mut String, st: &str) {
    let st = st.trim_end();
    if st.contains(';') {
        let delim = ["$$", "//", ";;", "$$$$"]
            .into_iter()
            .find(|d| !st.contains(d))
            .unwrap_or("$$$$");
        let _ = write!(out, "DELIMITER {delim}\n{st}{delim}\nDELIMITER ;\n");
    } else {
        out.push_str(st);
        out.push_str(";\n");
    }
}

/// Runs a plan's statements one by one, then its `finally` whatever
/// happened; a failure past `undo_from` puts the object back.
async fn run(s: &mut MySqlSession, plan: &Plan) -> Result<()> {
    let mut outcome = Ok(());
    for (i, st) in plan.steps.iter().enumerate() {
        if let Err(e) = s.conn.query_drop(st.as_str()).await {
            let e = crate::err(e);
            outcome = match plan.undo_from {
                Some(from) if i > from && !matches!(e, Error::Connect(_)) => undo(s, plan, e).await,
                _ => Err(e),
            };
            break;
        }
    }
    for st in &plan.finally {
        if let Err(e) = s.conn.query_drop(st.as_str()).await {
            tracing::debug!("{:?} scheduler: putting the session back: {e}", s.variant);
        }
    }
    outcome
}

/// After a step past `undo_from` failed with `e`: the object back as it
/// was.
async fn undo(s: &mut MySqlSession, plan: &Plan, e: Error) -> Result<()> {
    for st in &plan.undo {
        if let Err(u) = s.conn.query_drop(st.as_str()).await {
            let mut old = String::new();
            for st in &plan.undo {
                push_statement(&mut old, st);
            }
            return Err(Error::Query(format!(
                "No se guardó el cambio ({e}) ni se pudo dejar el objeto como estaba ({}). Para dejarlo como estaba:\n{old}",
                crate::err(u)
            )));
        }
    }
    Err(Error::Query(format!(
        "No se guardó el cambio: {e}. DBine dejó el objeto como estaba."
    )))
}

// ---------------------------------------------------------------------------
// A statement from the form: one, for the server too
// ---------------------------------------------------------------------------

/// The readings of a text that split it differently: with backslash
/// escapes or without (`NO_BACKSLASH_ESCAPES`), `"…"` a string or a name
/// (`ANSI_QUOTES`). The object's statement may run under any of them.
const READINGS: [(bool, bool); 4] = [(true, false), (true, true), (false, false), (false, true)];

/// Whether the server reads `body` (what goes after `DO` or `AS`) as the
/// rest of the object's statement. A save runs with multi-statements on:
/// a `;` outside a string, a quoted name or a comment (an executable
/// `/*! … */` is code) would end the object's statement there and run what
/// follows at once, as a statement of its own. `block`: the body may be
/// one `BEGIN … END` (an event's), with its `;` inside, and nothing after
/// its `END` but its label.
pub(crate) fn one_statement(body: &str, block: bool) -> bool {
    READINGS.iter().all(|&(backslash, ansi)| {
        let lexed = crate::rename_db::lexed(body, backslash, ansi);
        let toks = tokens(&lexed);
        if block && opens_block(&toks) {
            one_block(&toks)
        } else {
            !toks.iter().any(|t| t.text == ";")
        }
    })
}

/// A token of a lexed text (strings `'s'`, quoted names `` `i` ``,
/// comments gone).
struct Tok<'a> {
    text: &'a str,
    word: bool,
}

impl Tok<'_> {
    fn is(&self, w: &str) -> bool {
        self.word && self.text.eq_ignore_ascii_case(w)
    }

    /// A name: a word or a quoted one.
    fn name(&self) -> bool {
        self.word || self.text == "`i`"
    }
}

fn word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

fn tokens(t: &str) -> Vec<Tok<'_>> {
    let b = t.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let word = word_byte(c);
        if word {
            while i < b.len() && word_byte(b[i]) {
                i += 1;
            }
        } else if t[i..].starts_with("'s'") || t[i..].starts_with("`i`") {
            i += 3;
        } else {
            i += 1;
        }
        out.push(Tok {
            text: &t[start..i],
            word,
        });
    }
    out
}

/// `BEGIN` first, or after a label (`name:`).
fn opens_block(toks: &[Tok]) -> bool {
    match toks {
        [b, ..] if b.is("begin") => true,
        [l, c, b, ..] => l.name() && c.text == ":" && b.is("begin"),
        _ => false,
    }
}

/// What a `BEGIN … END` body holds open, as the server's grammar opens it:
/// `BEGIN`, `IF`, a `CASE` statement, `LOOP`, `REPEAT`, `WHILE` and
/// MariaDB's `FOR` only where a statement starts (else `BEGIN` is a name,
/// `IF` and `REPEAT` functions), a `CASE` expression anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    Block,
    If,
    CaseStmt,
    CaseExpr,
    Loop,
    Repeat,
    While,
    For,
}

/// The body is one `BEGIN … END` that ends with the text (its label after
/// it at most). Read the way the server nests it; anything it can't place
/// (an `END` that closes something else, `end` as a column's name, text
/// after the block) is not one, which only refuses: a block that ends
/// earlier for the server than here would let it run what follows.
fn one_block(toks: &[Tok]) -> bool {
    let mut stack: Vec<Open> = Vec::new();
    let mut at_start = true;
    // A WHILE or FOR header, until its DO.
    let mut header = false;
    // The statement is a DECLARE, of a HANDLER.
    let mut declare = false;
    let mut handler = false;
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        let start = std::mem::take(&mut at_start);
        if start {
            declare = false;
            handler = false;
            // `label:` keeps the statement's start.
            if t.name() && !t.is("end") && toks.get(i + 1).is_some_and(|n| n.text == ":") {
                at_start = true;
                i += 2;
                continue;
            }
        }
        if t.text == ";" {
            at_start = true;
        } else if t.word {
            match t.text.to_ascii_uppercase().as_str() {
                "BEGIN" if start => {
                    stack.push(Open::Block);
                    at_start = true;
                }
                "IF" if start => stack.push(Open::If),
                "CASE" => stack.push(if start {
                    Open::CaseStmt
                } else {
                    Open::CaseExpr
                }),
                "LOOP" if start => {
                    stack.push(Open::Loop);
                    at_start = true;
                }
                "REPEAT" if start => {
                    stack.push(Open::Repeat);
                    at_start = true;
                }
                "WHILE" if start => {
                    stack.push(Open::While);
                    header = true;
                }
                "FOR" if start => {
                    stack.push(Open::For);
                    header = true;
                }
                "DO" if header => {
                    header = false;
                    at_start = true;
                }
                "THEN" | "ELSE" if matches!(stack.last(), Some(Open::If | Open::CaseStmt)) => {
                    at_start = true;
                }
                "DECLARE" if start => declare = true,
                "HANDLER" if declare => handler = true,
                "FOR" if handler => {
                    // The conditions, then the handler's statement.
                    let Some(next) = conditions(toks, i + 1) else {
                        return false;
                    };
                    handler = false;
                    at_start = true;
                    i = next;
                    continue;
                }
                "END" => {
                    let closes = match toks.get(i + 1).filter(|n| n.word) {
                        Some(n) if n.is("if") => Some(Open::If),
                        Some(n) if n.is("case") => Some(Open::CaseStmt),
                        Some(n) if n.is("loop") => Some(Open::Loop),
                        Some(n) if n.is("repeat") => Some(Open::Repeat),
                        Some(n) if n.is("while") => Some(Open::While),
                        Some(n) if n.is("for") => Some(Open::For),
                        _ => None,
                    };
                    match (closes, stack.pop()) {
                        (Some(k), Some(o)) if k == o => i += 1,
                        (None, Some(Open::Block | Open::CaseExpr)) => {}
                        _ => return false,
                    }
                    if stack.is_empty() {
                        // The body's end: its label at most after it.
                        let rest = &toks[i + 1..];
                        return rest.is_empty() || (rest.len() == 1 && rest[0].name());
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    false
}

/// After `HANDLER FOR`: its conditions (`SQLSTATE [VALUE] '…'`, a number,
/// a name, `SQLWARNING`, `SQLEXCEPTION`, `NOT FOUND`), comma-separated.
/// Returns where the handler's statement starts.
fn conditions(toks: &[Tok], mut i: usize) -> Option<usize> {
    loop {
        let t = toks.get(i)?;
        if t.is("sqlstate") {
            i += 1;
            if toks.get(i)?.is("value") {
                i += 1;
            }
            if toks.get(i)?.text != "'s'" {
                return None;
            }
            i += 1;
        } else if t.is("not") {
            if !toks.get(i + 1)?.is("found") {
                return None;
            }
            i += 2;
        } else if t.name() {
            i += 1;
        } else {
            return None;
        }
        match toks.get(i) {
            Some(c) if c.text == "," => i += 1,
            _ => return Some(i),
        }
    }
}
