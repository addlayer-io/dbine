//! SAP HANA's scheduler jobs managed through the scheduler contract
//! ([`dbine_driver::Session::scheduler_info`] and the rest), built from
//! SAP's SQL reference (no HANA server in the test containers):
//!
//! - Create: `CREATE SCHEDULER JOB "S"."J" CRON '…' [FROM '…'] [UNTIL '…']
//!   ENABLE | DISABLE PROCEDURE "S"."P" [PARAMETERS p = v, …]`, and
//!   `COMMENT ON SCHEDULER JOB … IS '…'` for the comment.
//! - Edit, in place where HANA allows it: `ALTER SCHEDULER JOB … CRON '…'
//!   [FROM …] [UNTIL …]`, `… ENABLE | DISABLE`, `… PARAMETERS …`, and the
//!   comment. A new name, schema or procedure, a window bound removed or
//!   every parameter removed have no `ALTER`: the job is created again
//!   (the new one first, then the old one dropped, when the name changes;
//!   dropped and created when only the procedure or a bound changes). The
//!   script says so at the top, with what is lost (the runs
//!   `M_SCHEDULER_JOBS` keeps under the old job).
//! - Delete: `DROP SCHEDULER JOB …`.
//! - HANA has no statement to run a job now, to start it at a step or to
//!   clear its runs, and no scheduler log of its own: the runs are the
//!   job's history (`job_runs`).
//!
//! A script of several statements runs in one transaction: DDL commits by
//! itself in HANA unless the session turns `AUTOCOMMIT DDL` off, so the
//! save turns the connection's auto-commit off, runs `SET TRANSACTION
//! AUTOCOMMIT DDL OFF`, the statements and `COMMIT` (`ROLLBACK` when one
//! fails), and puts both settings back. A recreation that fails to create
//! leaves the old job as it was. (Built from SAP's reference, to verify on a
//! real server; a server that refuses the `SET TRANSACTION` gets the
//! statements one by one, each committed by itself, as the script says.)
//!
//! What the login may do comes from `SYS.EFFECTIVE_PRIVILEGES` (its own and
//! its roles') and the schemas it owns: `CREATE ANY` on a schema to create
//! there; being the job's owner, or `ALTER` / `DROP` on the job or its
//! schema, to change or delete it (`DATA ADMIN`: anything). A job the login
//! can't change gets a read-only form with a note. When the privileges
//! can't be read, everything is offered and the server decides.
//!
//! Ids are `"SCHEMA"."JOB"` as
//! [`crate::jobs`] lists them; the schema and the procedure must exist,
//! the parameters must be the procedure's, and each value is written as the
//! parameter's type reads it (numbers checked, everything else a string
//! literal). Names go quoted, texts as `'…'` literals.

use crate::jobs::{self, job_id, lit, parse_id, Columns};
use crate::monitor::{query, Set};
use crate::quote;
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result,
    SchedulerInfo, SchedulerItem, SchedulerKind, SchedulerList,
};
use hdbconnect_async::Connection;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Notes and messages
// ---------------------------------------------------------------------------

const PRIVILEGES: &str = "Para crear un scheduler job hace falta el privilegio CREATE ANY en su esquema y EXECUTE sobre el procedimiento; para cambiarlo o borrarlo, ser su dueño o tener ALTER o DROP sobre él. El job corre con los privilegios de quien lo creó.";
const RECREATE_NOTE: &str = "SAP HANA no cambia con ALTER el nombre, el esquema ni el procedimiento de un scheduler job, ni le quita la ventana o todos los parámetros: para eso DBine lo crea de nuevo y se pierde el historial de sus ejecuciones.";
const IN_TRANSACTION: &str = "DBine guarda estas sentencias en una sola transacción (SET TRANSACTION AUTOCOMMIT DDL OFF): si una falla, se deshacen todas y el job queda como estaba. Si el servidor no deja apagar AUTOCOMMIT DDL, cada sentencia se confirma sola.";
const NO_ALTER: &str = "Este usuario no puede cambiar este scheduler job: no es su dueño ni tiene ALTER sobre él o sobre su esquema.";
const NO_DROP: &str = "Este usuario no puede borrar este scheduler job: no es su dueño ni tiene DROP sobre él o sobre su esquema.";
const NO_DROP_TO_RECREATE: &str = "Para este cambio SAP HANA borra el scheduler job y lo crea de nuevo, y este usuario no tiene DROP sobre él ni sobre su esquema.";
const NO_CREATE_ANY: &str =
    "Este usuario no tiene CREATE ANY en ningún esquema: no puede crear scheduler jobs.";
const DDL_OFF: &str = "SET TRANSACTION AUTOCOMMIT DDL OFF";
const DDL_ON: &str = "SET TRANSACTION AUTOCOMMIT DDL ON";
const NO_COMMENTS: &str = "Esta versión de SAP HANA no guarda comentarios de los scheduler jobs.";
const MAX_NAME: usize = 127;
const MAX_COMMENT: usize = 5000;
const MAX_CHOICES: usize = 5000;

fn bad(message: String) -> Error {
    Error::Query(message)
}

/// Text for a `--` comment line or a message: control characters become
/// `?`, so nothing ends the line.
fn comment_text(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                '?'
            } else {
                c
            }
        })
        .collect()
}

/// Text from the app, short and on one line, for a message.
fn shown(s: &str) -> String {
    comment_text(s.trim()).chars().take(60).collect()
}

fn check_kind(kind: &str) -> Result<()> {
    if kind.trim() == "job" {
        Ok(())
    } else {
        Err(bad(format!(
            "«{}» no es un tipo de objeto del planificador de SAP HANA: solo tiene scheduler jobs.",
            shown(kind)
        )))
    }
}

/// `"S"."N"` for messages and comments.
fn display(schema: &str, name: &str) -> String {
    comment_text(&job_id(schema, name))
}

// ---------------------------------------------------------------------------
// Values: cron, times, parameters
// ---------------------------------------------------------------------------

/// A HANA scheduler cron: seven fields (year, month, date, weekday, hour,
/// minute, second), each `*`, a number, `*/n`, `a:b`, `a:b/n`, `a.y` or a
/// list of them; weekdays also by name (`mon`…`sun`). Returned with single
/// spaces.
pub(crate) fn check_cron(cron: &str) -> std::result::Result<String, String> {
    const DAYS: &[&str] = &["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
    if cron.chars().any(|c| c.is_control() && c != '\t') {
        return Err("El cron no puede tener saltos de línea.".into());
    }
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 7 {
        return Err(format!(
            "El cron de SAP HANA tiene 7 campos (año, mes, día, día de la semana, hora, minuto y segundo): tiene {}.",
            fields.len()
        ));
    }
    for (i, f) in fields.iter().enumerate() {
        let wrong = || cron_field_error(i, f);
        if f.len() > 64
            || !f
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "*/:,.-".contains(c))
        {
            return Err(wrong());
        }
        // Letters only as weekday names.
        let mut word = String::new();
        for c in f.chars().chain(std::iter::once(' ')) {
            if c.is_ascii_alphabetic() {
                word.push(c.to_ascii_lowercase());
            } else if !word.is_empty() {
                if i != 3 || !DAYS.contains(&word.as_str()) {
                    return Err(wrong());
                }
                word.clear();
            }
        }
        if f.split(',').any(str::is_empty) {
            return Err(wrong());
        }
    }
    Ok(fields.join(" "))
}

/// Why field `i` of a cron is wrong, as a whole sentence.
fn cron_field_error(i: usize, field: &str) -> String {
    let f = shown(field);
    match i {
        0 => format!("«{f}» no es un valor válido para el año en el cron."),
        1 => format!("«{f}» no es un valor válido para el mes en el cron."),
        2 => format!("«{f}» no es un valor válido para el día en el cron."),
        3 => format!("«{f}» no es un valor válido para el día de la semana en el cron (números o mon, tue, wed, thu, fri, sat, sun)."),
        4 => format!("«{f}» no es un valor válido para la hora en el cron."),
        5 => format!("«{f}» no es un valor válido para los minutos en el cron."),
        _ => format!("«{f}» no es un valor válido para los segundos en el cron."),
    }
}

fn days_in(y: u32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400) => 29,
        _ => 28,
    }
}

/// `YYYY-MM-DD[T ]HH:MM[:SS[.fff]]` (the server's local time) as HANA's
/// `YYYY-MM-DD HH:MM:SS`; `None` for anything else.
pub(crate) fn timestamp(v: &str) -> Option<String> {
    let t = v.trim();
    let digits = |s: &str, n: usize| -> Option<u32> {
        (s.len() == n && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse().ok())
            .flatten()
    };
    let (date, time) = t.split_at_checked(10)?;
    let time = time.strip_prefix('T').or_else(|| time.strip_prefix(' '))?;
    let d: Vec<&str> = date.split('-').collect();
    if d.len() != 3 {
        return None;
    }
    let (y, m, day) = (digits(d[0], 4)?, digits(d[1], 2)?, digits(d[2], 2)?);
    if !(1..=12).contains(&m) || day == 0 || day > days_in(y, m) || y == 0 {
        return None;
    }
    let time = match time.split_once('.') {
        Some((hms, frac)) if !frac.is_empty() && frac.bytes().all(|b| b.is_ascii_digit()) => hms,
        Some(_) => return None,
        None => time,
    };
    let p: Vec<&str> = time.split(':').collect();
    if !(2..=3).contains(&p.len()) {
        return None;
    }
    let (h, mi) = (digits(p[0], 2)?, digits(p[1], 2)?);
    let s = match p.get(2) {
        Some(s) => digits(s, 2)?,
        None => 0,
    };
    (h < 24 && mi < 60 && s < 60).then(|| format!("{y:04}-{m:02}-{day:02} {h:02}:{mi:02}:{s:02}"))
}

/// A parameter of the job's procedure.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProcParam {
    pub name: String,
    pub data_type: String,
    /// IN, OUT, INOUT.
    pub mode: String,
}

const NUMERIC: &[&str] = &[
    "TINYINT",
    "SMALLINT",
    "INTEGER",
    "INT",
    "BIGINT",
    "DECIMAL",
    "SMALLDECIMAL",
    "REAL",
    "DOUBLE",
    "FLOAT",
];

/// A plain number as SQL writes it (`-12`, `3.5`, `1e3`), nothing else.
fn is_number(v: &str) -> bool {
    let b = v.as_bytes();
    !v.is_empty()
        && v.len() <= 64
        && b.iter()
            .all(|c| c.is_ascii_digit() || matches!(c, b'+' | b'-' | b'.' | b'e' | b'E'))
        && b.iter().any(u8::is_ascii_digit)
        && v.parse::<f64>().is_ok_and(f64::is_finite)
}

/// A parameter's value as the parameter's type reads it (`ty` `None`: the
/// procedure's parameters aren't visible; numbers as they are, the rest
/// as strings).
pub(crate) fn param_literal(value: &str, ty: Option<&str>, name: &str) -> Result<String> {
    let v = value.trim();
    let ty = ty.map(|t| t.trim().to_ascii_uppercase());
    match ty.as_deref() {
        Some(t) if NUMERIC.contains(&t) => {
            if is_number(v) {
                Ok(v.to_string())
            } else {
                Err(bad(format!(
                    "El parámetro {} es {t}: «{}» no es un número.",
                    shown(name),
                    shown(value)
                )))
            }
        }
        Some("BOOLEAN") => match v.to_ascii_uppercase().as_str() {
            "TRUE" | "1" => Ok("TRUE".into()),
            "FALSE" | "0" => Ok("FALSE".into()),
            _ => Err(bad(format!(
                "El parámetro {} es BOOLEAN: tiene que ser TRUE o FALSE.",
                shown(name)
            ))),
        },
        None if is_number(v) => Ok(v.to_string()),
        _ => Ok(lit(value)),
    }
}

/// A parameter's name: bare when HANA would read it the same, quoted
/// otherwise (a lower-case name stays lower case).
pub(crate) fn param_ident(n: &str) -> String {
    let plain = n
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && n.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if plain {
        n.to_string()
    } else {
        quote(n)
    }
}

/// A value as `SCHEDULER_JOB_PARAMETERS` gives it, for the form: a quoted
/// literal loses its quotes.
pub(crate) fn unquote_value(v: &str) -> String {
    let t = v.trim();
    if t.len() >= 2 && t.starts_with('\'') && t.ends_with('\'') {
        t[1..t.len() - 1].replace("''", "'")
    } else {
        t.to_string()
    }
}

// ---------------------------------------------------------------------------
// A job as the catalog has it, and as the designer wants it
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Job {
    pub schema: String,
    pub name: String,
    pub owner: Option<String>,
    pub proc_schema: Option<String>,
    pub proc_name: Option<String>,
    pub cron: String,
    pub enabled: bool,
    pub valid: Option<String>,
    pub comments: Option<String>,
    /// The window as written (`YYYY-MM-DD HH:MM:SS`, local time).
    pub from: Option<String>,
    pub until: Option<String>,
    pub created: Option<String>,
    /// As the catalog gives them (values unquoted).
    pub params: Vec<(String, String)>,
}

impl Job {
    pub(crate) fn of(set: &Set, r: usize, params: &[(String, String)]) -> Option<Job> {
        let s = |c: &str| set.get(r, c).map(str::to_string);
        Some(Job {
            schema: s("SCHEMA_NAME")?,
            name: s("SCHEDULER_JOB_NAME")?,
            owner: s("USER_NAME"),
            proc_schema: s("PROCEDURE_SCHEMA_NAME"),
            proc_name: s("PROCEDURE_NAME"),
            cron: s("CRON").unwrap_or_default(),
            enabled: s("IS_ENABLED").is_some_and(|v| v.eq_ignore_ascii_case("TRUE")),
            valid: s("IS_VALID"),
            comments: s("COMMENTS"),
            from: s("START_TIME_LOCAL"),
            until: s("END_TIME_LOCAL"),
            created: s("CREATE_TIME"),
            params: params
                .iter()
                .map(|(k, v)| (k.clone(), unquote_value(v)))
                .collect(),
        })
    }

    fn procedure(&self) -> Option<String> {
        Some(job_id(
            self.proc_schema.as_deref()?,
            self.proc_name.as_deref()?,
        ))
    }

    fn system(&self) -> bool {
        jobs::hana_own(&self.schema) || self.owner.as_deref().is_some_and(jobs::hana_own)
    }
}

/// What the designer asks for, checked (the procedure's existence and its
/// parameters are checked against the server in [`Wanted::typed`]).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Wanted {
    pub schema: String,
    pub name: String,
    pub proc_schema: String,
    pub proc_name: String,
    pub cron: String,
    pub from: Option<String>,
    pub until: Option<String>,
    pub enabled: bool,
    pub comments: Option<String>,
    /// Name and value as typed.
    pub params: Vec<(String, String)>,
}

fn text(v: &BTreeMap<String, Value>, k: &str) -> String {
    match v.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(Value::Array(_) | Value::Object(_)) => String::new(),
        Some(x) => x.to_string(),
    }
}

fn flag(v: &BTreeMap<String, Value>, k: &str, default: bool) -> bool {
    match v.get(k) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => matches!(
            s.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        ),
        _ => default,
    }
}

/// Which name [`check_name`] checks.
#[derive(Clone, Copy)]
enum NameOf {
    Schema,
    Job,
    Parameter,
}

fn check_name(v: &str, of: NameOf) -> Result<String> {
    let t = v.trim();
    if t.is_empty() {
        return Err(bad(match of {
            NameOf::Schema => "Elegí el esquema del job.".into(),
            NameOf::Job => "El job necesita un nombre.".into(),
            NameOf::Parameter => "Cada parámetro necesita su nombre.".into(),
        }));
    }
    if t.chars().count() > MAX_NAME || t.chars().any(char::is_control) {
        let n = shown(t);
        return Err(bad(match of {
            NameOf::Schema => format!("«{n}» no es un nombre de esquema válido (hasta {MAX_NAME} caracteres, sin saltos de línea)."),
            NameOf::Job => format!("«{n}» no es un nombre de job válido (hasta {MAX_NAME} caracteres, sin saltos de línea)."),
            NameOf::Parameter => format!("«{n}» no es un nombre de parámetro válido (hasta {MAX_NAME} caracteres, sin saltos de línea)."),
        }));
    }
    Ok(t.to_string())
}

pub(crate) fn wanted(values: &FormValues, comments: bool) -> Result<Wanted> {
    let v = &values.values;
    let schema = check_name(&text(v, "schema"), NameOf::Schema)?;
    let name = check_name(&text(v, "name"), NameOf::Job)?;
    let procedure = text(v, "procedure");
    let (proc_schema, proc_name) = parse_id(procedure.trim()).ok_or_else(|| {
        if procedure.trim().is_empty() {
            bad("Elegí el procedimiento que ejecuta el job.".into())
        } else {
            bad(format!(
                "«{}» no es un procedimiento de la lista.",
                shown(&procedure)
            ))
        }
    })?;
    let cron = text(v, "cron");
    if cron.trim().is_empty() {
        return Err(bad("El job necesita un cron.".into()));
    }
    let cron = check_cron(&cron).map_err(bad)?;
    let time = |k: &str| -> Option<std::result::Result<String, ()>> {
        let t = text(v, k);
        (!t.trim().is_empty()).then(|| timestamp(&t).ok_or(()))
    };
    let from = time("from").transpose().map_err(|_| {
        bad("«Desde» tiene que ser una fecha y hora (AAAA-MM-DD HH:MM:SS), en la hora del servidor.".into())
    })?;
    let until = time("until").transpose().map_err(|_| {
        bad("«Hasta» tiene que ser una fecha y hora (AAAA-MM-DD HH:MM:SS), en la hora del servidor.".into())
    })?;
    if let (Some(a), Some(b)) = (&from, &until) {
        if b <= a {
            return Err(bad("«Hasta» tiene que ser posterior a «Desde».".into()));
        }
    }
    let comments = if comments {
        let c = text(v, "comments");
        if c.chars().count() > MAX_COMMENT {
            return Err(bad(format!(
                "El comentario puede tener hasta {MAX_COMMENT} caracteres."
            )));
        }
        Some(c.trim().to_string()).filter(|c| !c.is_empty())
    } else {
        None
    };
    let mut params: Vec<(String, String)> = Vec::new();
    for row in values.lists.get("parameters").into_iter().flatten() {
        let pname = check_name(&text(row, "name"), NameOf::Parameter)?;
        if params.iter().any(|(n, _)| *n == pname) {
            return Err(bad(format!(
                "El parámetro {} está dos veces.",
                shown(&pname)
            )));
        }
        let value = text(row, "value");
        if value.chars().count() > MAX_COMMENT {
            return Err(bad(format!(
                "El valor del parámetro {} es demasiado largo.",
                shown(&pname)
            )));
        }
        params.push((pname, value));
    }
    Ok(Wanted {
        schema,
        name,
        proc_schema,
        proc_name,
        cron,
        from,
        until,
        enabled: flag(v, "enabled", true),
        comments,
        params,
    })
}

impl Wanted {
    /// The parameters as SQL (`NAME = value`), checked against the
    /// procedure's (`proc` empty: not visible, written as typed).
    pub(crate) fn typed(&self, proc: &[ProcParam]) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for (n, v) in &self.params {
            let p = if proc.is_empty() {
                None
            } else {
                let p = proc
                    .iter()
                    .find(|p| p.name == *n)
                    .or_else(|| proc.iter().find(|p| p.name.eq_ignore_ascii_case(n)))
                    .ok_or_else(|| {
                        bad(format!(
                            "El procedimiento {} no tiene el parámetro {} (tiene: {}).",
                            display(&self.proc_schema, &self.proc_name),
                            shown(n),
                            proc.iter()
                                .map(|p| p.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ))
                    })?;
                if p.mode.eq_ignore_ascii_case("OUT") {
                    return Err(bad(format!(
                        "El parámetro {} es de salida (OUT): el job no le pasa un valor.",
                        shown(&p.name)
                    )));
                }
                Some(p)
            };
            let name = p.map_or(n.as_str(), |p| p.name.as_str());
            out.push((
                name.to_string(),
                param_literal(v, p.map(|p| p.data_type.as_str()), name)?,
            ));
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

/// What a change runs (in order, in one transaction where the server
/// allows it: [`run`]), the script shown, and the job's id.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    pub script: String,
    pub statements: Vec<String>,
    pub id: String,
    /// The existing job (schema, name) the plan drops to create it again.
    pub drops: Option<(String, String)>,
}

impl Plan {
    fn new(comments: &[String], statements: Vec<String>, id: String) -> Result<Plan> {
        let mut script = String::new();
        for c in comments {
            script.push_str("-- ");
            script.push_str(&comment_text(c));
            script.push('\n');
        }
        for s in &statements {
            script.push_str(s);
            script.push_str(";\n");
        }
        // Each statement is one piece of the script as the editor splits it.
        let pieces = crate::script::split(&script);
        if pieces.len() != statements.len()
            || pieces
                .iter()
                .zip(&statements)
                .any(|(p, s)| !p.ends_with(s.as_str()))
        {
            return Err(Error::Unsupported(
                "DBine no puede escribir este scheduler job como un script que se lea igual: revisá los nombres y los valores.".into(),
            ));
        }
        Ok(Plan {
            script,
            statements,
            id,
            drops: None,
        })
    }

    fn nothing(job: &Job) -> Plan {
        Plan {
            script: format!(
                "-- {}: no hay cambios que guardar.\n",
                display(&job.schema, &job.name)
            ),
            statements: Vec::new(),
            id: job_id(&job.schema, &job.name),
            drops: None,
        }
    }
}

fn params_clause(params: &[(String, String)]) -> String {
    params
        .iter()
        .map(|(n, v)| format!("{} = {v}", param_ident(n)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn window(from: &Option<String>, until: &Option<String>) -> String {
    let mut s = String::new();
    if let Some(f) = from {
        s.push_str(&format!(" FROM {}", lit(f)));
    }
    if let Some(u) = until {
        s.push_str(&format!(" UNTIL {}", lit(u)));
    }
    s
}

pub(crate) fn create_statement(w: &Wanted, params: &[(String, String)]) -> String {
    let mut s = format!(
        "CREATE SCHEDULER JOB {}\n  CRON {}",
        job_id(&w.schema, &w.name),
        lit(&w.cron)
    );
    let win = window(&w.from, &w.until);
    if !win.is_empty() {
        s.push_str("\n ");
        s.push_str(&win);
    }
    s.push_str(if w.enabled {
        "\n  ENABLE"
    } else {
        "\n  DISABLE"
    });
    s.push_str(&format!(
        "\n  PROCEDURE {}",
        job_id(&w.proc_schema, &w.proc_name)
    ));
    if !params.is_empty() {
        s.push_str(&format!("\n  PARAMETERS {}", params_clause(params)));
    }
    s
}

fn comment_statement(schema: &str, name: &str, comment: &Option<String>) -> String {
    format!(
        "COMMENT ON SCHEDULER JOB {} IS {}",
        job_id(schema, name),
        comment.as_deref().map_or_else(|| "NULL".to_string(), lit)
    )
}

pub(crate) fn drop_statement(schema: &str, name: &str) -> String {
    format!("DROP SCHEDULER JOB {}", job_id(schema, name))
}

/// The statements that create the job and its comment.
fn create_all(w: &Wanted, params: &[(String, String)]) -> Vec<String> {
    let mut out = vec![create_statement(w, params)];
    if w.comments.is_some() {
        out.push(comment_statement(&w.schema, &w.name, &w.comments));
    }
    out
}

/// Two parameter lists hold the same values (by name, values unquoted).
fn same_params(cur: &[(String, String)], wanted: &[(String, String)]) -> bool {
    let norm = |l: &[(String, String)]| -> BTreeMap<String, String> {
        l.iter()
            .map(|(k, v)| (k.to_ascii_uppercase(), unquote_value(v)))
            .collect()
    };
    norm(cur) == norm(wanted)
}

/// The script for `w`: a new job (`current` `None`) or the change of
/// `current`. `params` are `w`'s parameters as SQL ([`Wanted::typed`]).
pub(crate) fn plan(
    current: Option<&Job>,
    w: &Wanted,
    params: &[(String, String)],
    comments: bool,
) -> Result<Plan> {
    let id = job_id(&w.schema, &w.name);
    let header = format!("Scheduler job {} de SAP HANA", display(&w.schema, &w.name));
    let Some(cur) = current else {
        let stmts = create_all(w, params);
        let mut notes = vec![header];
        if stmts.len() > 1 {
            notes.push(IN_TRANSACTION.into());
        }
        return Plan::new(&notes, stmts, id);
    };
    let renamed = cur.schema != w.schema || cur.name != w.name;
    let same_proc = cur.proc_schema.as_deref() == Some(w.proc_schema.as_str())
        && cur.proc_name.as_deref() == Some(w.proc_name.as_str());
    let bound_removed =
        (cur.from.is_some() && w.from.is_none()) || (cur.until.is_some() && w.until.is_none());
    let params_removed = !cur.params.is_empty() && w.params.is_empty();
    if renamed || !same_proc || bound_removed || params_removed {
        let old = display(&cur.schema, &cur.name);
        let mut notes = vec![header];
        let stmts = if renamed {
            notes.push(format!(
                "SAP HANA no cambia el nombre ni el esquema de un scheduler job: se crea {} y se borra {old}; el historial de ejecuciones de {old} se pierde.",
                display(&w.schema, &w.name)
            ));
            let mut s = create_all(w, params);
            s.push(drop_statement(&cur.schema, &cur.name));
            s
        } else {
            notes.push(if !same_proc {
                format!("SAP HANA no cambia con ALTER el procedimiento de un scheduler job: se borra {old} y se vuelve a crear; se pierde el historial de sus ejecuciones.")
            } else if bound_removed {
                format!("SAP HANA no quita con ALTER la ventana (FROM / UNTIL) de un scheduler job: se borra {old} y se vuelve a crear; se pierde el historial de sus ejecuciones.")
            } else {
                format!("SAP HANA no quita con ALTER todos los parámetros de un scheduler job: se borra {old} y se vuelve a crear; se pierde el historial de sus ejecuciones.")
            });
            let mut s = vec![drop_statement(&cur.schema, &cur.name)];
            s.extend(create_all(w, params));
            s
        };
        notes.push(IN_TRANSACTION.into());
        let mut plan = Plan::new(&notes, stmts, id)?;
        plan.drops = Some((cur.schema.clone(), cur.name.clone()));
        return Ok(plan);
    }
    let job = job_id(&cur.schema, &cur.name);
    let mut stmts = Vec::new();
    let cur_cron = cur.cron.split_whitespace().collect::<Vec<_>>().join(" ");
    if cur_cron != w.cron || cur.from != w.from || cur.until != w.until {
        stmts.push(format!(
            "ALTER SCHEDULER JOB {job} CRON {}{}",
            lit(&w.cron),
            window(&w.from, &w.until)
        ));
    }
    if cur.enabled != w.enabled {
        stmts.push(format!(
            "ALTER SCHEDULER JOB {job} {}",
            if w.enabled { "ENABLE" } else { "DISABLE" }
        ));
    }
    if !same_params(&cur.params, &w.params) {
        stmts.push(format!(
            "ALTER SCHEDULER JOB {job} PARAMETERS {}",
            params_clause(params)
        ));
    }
    if comments
        && cur
            .comments
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            != w.comments.as_deref()
    {
        stmts.push(comment_statement(&cur.schema, &cur.name, &w.comments));
    }
    if stmts.is_empty() {
        return Ok(Plan::nothing(cur));
    }
    let mut notes = vec![format!("{header}: cambia en el lugar")];
    if stmts.len() > 1 {
        notes.push(IN_TRANSACTION.into());
    }
    Plan::new(&notes, stmts, id)
}

pub(crate) fn delete_plan(job: &Job) -> Result<Plan> {
    Plan::new(
        &[format!(
            "Borra el scheduler job {} de SAP HANA (con el historial de sus ejecuciones)",
            display(&job.schema, &job.name)
        )],
        vec![drop_statement(&job.schema, &job.name)],
        job_id(&job.schema, &job.name),
    )
}

// ---------------------------------------------------------------------------
// What the login may do
// ---------------------------------------------------------------------------

/// The login's privileges on scheduler jobs: its `SYS.EFFECTIVE_PRIVILEGES`
/// rows (own and through roles) and the schemas it owns.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Rights {
    pub user: String,
    /// `DATA ADMIN`: any DDL.
    pub admin: bool,
    pub owns: BTreeSet<String>,
    /// (privilege, schema, object): `object` `None` for a schema privilege.
    pub grants: Vec<(String, String, Option<String>)>,
}

impl Rights {
    /// From `SELECT SCHEMA_NAME FROM SYS.SCHEMAS WHERE SCHEMA_OWNER = …`
    /// and `SELECT PRIVILEGE, OBJECT_TYPE, SCHEMA_NAME, OBJECT_NAME FROM
    /// SYS.EFFECTIVE_PRIVILEGES WHERE USER_NAME = …`.
    pub(crate) fn of(user: &str, owned: &Set, privileges: &Set) -> Rights {
        let mut r = Rights {
            user: user.to_string(),
            owns: (0..owned.rows.len())
                .filter_map(|i| owned.get(i, "SCHEMA_NAME").map(str::to_string))
                .collect(),
            ..Rights::default()
        };
        for i in 0..privileges.rows.len() {
            let Some(p) = privileges.get(i, "PRIVILEGE").map(str::to_ascii_uppercase) else {
                continue;
            };
            if p == "DATA ADMIN" {
                r.admin = true;
                continue;
            }
            let schema = privileges.get(i, "SCHEMA_NAME");
            let object = privileges.get(i, "OBJECT_NAME");
            let on_schema = privileges
                .get(i, "OBJECT_TYPE")
                .is_some_and(|t| t.eq_ignore_ascii_case("SCHEMA"));
            match (on_schema, schema, object) {
                // A schema privilege names the schema in one column or the other.
                (true, Some(s), _) | (true, None, Some(s)) => {
                    r.grants.push((p, s.to_string(), None))
                }
                (false, Some(s), Some(o)) => r.grants.push((p, s.to_string(), Some(o.to_string()))),
                _ => {}
            }
        }
        r
    }

    fn grant(&self, privilege: &str, schema: &str, object: Option<&str>) -> bool {
        self.grants.iter().any(|(p, s, o)| {
            p == privilege && s == schema && (o.is_none() || o.as_deref() == object)
        })
    }

    /// May create a job in `schema`.
    pub(crate) fn create_in(&self, schema: &str) -> bool {
        self.admin || self.owns.contains(schema) || self.grant("CREATE ANY", schema, None)
    }

    /// May create a job somewhere.
    pub(crate) fn creates(&self) -> bool {
        self.admin
            || !self.owns.is_empty()
            || self
                .grants
                .iter()
                .any(|(p, _, o)| p == "CREATE ANY" && o.is_none())
    }

    fn changes(&self, privilege: &str, job: &Job) -> bool {
        self.admin
            || job.owner.as_deref() == Some(self.user.as_str())
            || self.owns.contains(&job.schema)
            || self.grant(privilege, &job.schema, Some(&job.name))
    }

    pub(crate) fn alters(&self, job: &Job) -> bool {
        self.changes("ALTER", job)
    }

    pub(crate) fn drops(&self, job: &Job) -> bool {
        self.changes("DROP", job)
    }

    /// May change or delete some job (its own, or others' through a grant).
    pub(crate) fn changes_any(&self) -> bool {
        self.creates()
            || self
                .grants
                .iter()
                .any(|(p, _, _)| p == "ALTER" || p == "DROP")
    }
}

/// `plan` refused where the login's privileges say the server would refuse
/// it (`rights` `None`: unknown, the server decides).
pub(crate) fn check_rights(
    rights: Option<&Rights>,
    current: Option<&Job>,
    w: &Wanted,
    plan: &Plan,
) -> Result<()> {
    let Some(r) = rights else {
        return Ok(());
    };
    let create_here = || {
        if r.create_in(&w.schema) {
            Ok(())
        } else {
            Err(bad(format!(
                "Este usuario no tiene CREATE ANY en el esquema {}: no puede crear scheduler jobs ahí.",
                quote(&w.schema)
            )))
        }
    };
    match current {
        None => create_here(),
        Some(_) if plan.statements.is_empty() => Ok(()),
        Some(cur) => {
            if !r.alters(cur) {
                return Err(bad(NO_ALTER.into()));
            }
            if plan.drops.is_some() {
                if !r.drops(cur) {
                    return Err(bad(NO_DROP_TO_RECREATE.into()));
                }
                create_here()?;
            }
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

/// The server's lists the form picks from.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Catalog {
    pub schemas: Vec<String>,
    /// (schema, name)
    pub procedures: Vec<(String, String)>,
    /// The current schema, for a new job.
    pub current: String,
}

/// `rights` `None`: the login's privileges couldn't be read.
pub(crate) fn form_of(
    job: Option<&Job>,
    cat: &Catalog,
    comments: bool,
    rights: Option<&Rights>,
) -> ObjectForm {
    let mut f = ObjectForm::default();
    // Where the login may create a job (the job's own schema stays).
    let mut schemas: Vec<(String, String)> = cat
        .schemas
        .iter()
        .filter(|s| rights.is_none_or(|r| r.create_in(s)))
        .map(|s| (s.clone(), s.clone()))
        .collect();
    let mut procs: Vec<(String, String)> = cat
        .procedures
        .iter()
        .map(|(s, n)| (job_id(s, n), format!("{s}.{n}")))
        .collect();
    if let Some(j) = job {
        if !schemas.iter().any(|(v, _)| *v == j.schema) {
            schemas.push((j.schema.clone(), j.schema.clone()));
        }
        if let (Some(p), Some(ps), Some(pn)) = (j.procedure(), &j.proc_schema, &j.proc_name) {
            if !procs.iter().any(|(v, _)| *v == p) {
                procs.push((p, format!("{ps}.{pn}")));
            }
        }
    }
    f.choices.insert("schema".into(), schemas);
    f.choices.insert("procedure".into(), procs);

    let mut general = FormSection::new("general", "General").fields(vec![
        FormField::new("schema", "Esquema", FormKind::reference("schema", false))
            .required()
            .help("El esquema del job. Cambiarlo crea el job de nuevo."),
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .help("Cambiarlo crea el job de nuevo con ese nombre."),
        FormField::new(
            "procedure",
            "Procedimiento",
            FormKind::reference("procedure", false),
        )
        .required()
        .help("El procedimiento que ejecuta el job. Cambiarlo crea el job de nuevo."),
        FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
    ]);
    if comments {
        general = general.field(FormField::new("comments", "Comentario", FormKind::Textarea));
    }
    if job.is_some() {
        general = general.fields(vec![
            FormField::new("owner", "Usuario que lo ejecuta", FormKind::Text).read_only(true),
            FormField::new("valid", "Válido", FormKind::Text).read_only(true),
            FormField::new("created", "Creado", FormKind::DateTime).read_only(true),
        ]);
    }
    let schedule = FormSection::new("schedule", "Programación").fields(vec![
        FormField::new("cron", "Cron", FormKind::Cron)
            .required()
            .placeholder("* * * * 3 0 0")
            .help("Siete campos: año, mes, día, día de la semana, hora, minuto y segundo. Cada uno admite * (todos), un número, */n (cada n), a:b (de a a b), a:b/n y listas a,b,c; el día de la semana también mon…sun. Ej.: * * * * 3 0 0 todos los días a las 3:00."),
        FormField::new("from", "Desde", FormKind::DateTime)
            .help("Desde cuándo corre, en la hora del servidor. Vacío: desde que se crea."),
        FormField::new("until", "Hasta", FormKind::DateTime)
            .help("Hasta cuándo corre, en la hora del servidor. Vacío: sin fin."),
    ]);
    let rows: Vec<BTreeMap<String, Value>> = job
        .map(|j| {
            j.params
                .iter()
                .map(|(k, v)| {
                    BTreeMap::from([
                        ("name".to_string(), json!(k)),
                        ("value".to_string(), json!(v)),
                    ])
                })
                .collect()
        })
        .unwrap_or_default();
    let parameters = FormSection::new("parameters", "Parámetros").list(
        FormList::new(
            "parameters",
            "Parámetros",
            "name",
            vec![
                FormField::new("name", "Parámetro", FormKind::Text)
                    .required()
                    .help("Un parámetro de entrada del procedimiento."),
                FormField::new("value", "Valor", FormKind::Text).help(
                    "Se escribe según el tipo del parámetro: los números tal cual, el resto como texto entre comillas.",
                ),
            ],
        )
        .rows(rows),
    );
    f.sections = vec![general, schedule, parameters];

    match job {
        None => {
            let allowed = &f.choices["schema"];
            let schema = if allowed.iter().any(|(v, _)| *v == cat.current) {
                cat.current.clone()
            } else {
                allowed.first().map(|(v, _)| v.clone()).unwrap_or_default()
            };
            f.values.insert("schema".into(), json!(schema));
            f.values.insert("name".into(), json!(""));
            f.values.insert("procedure".into(), json!(""));
            f.values.insert("enabled".into(), json!(true));
            f.values.insert("cron".into(), json!(""));
            if comments {
                f.values.insert("comments".into(), json!(""));
            }
        }
        Some(j) => {
            f.values.insert("schema".into(), json!(j.schema));
            f.values.insert("name".into(), json!(j.name));
            f.values
                .insert("procedure".into(), json!(j.procedure().unwrap_or_default()));
            f.values.insert("enabled".into(), json!(j.enabled));
            f.values.insert("cron".into(), json!(j.cron));
            let t = |v: &Option<String>| {
                v.as_ref()
                    .map(|s| json!(s.replacen(' ', "T", 1)))
                    .unwrap_or(Value::Null)
            };
            f.values.insert("from".into(), t(&j.from));
            f.values.insert("until".into(), t(&j.until));
            if comments {
                f.values.insert(
                    "comments".into(),
                    json!(j.comments.clone().unwrap_or_default()),
                );
            }
            f.values
                .insert("owner".into(), json!(j.owner.clone().unwrap_or_default()));
            f.values
                .insert("valid".into(), json!(j.valid.clone().unwrap_or_default()));
            f.values.insert(
                "created".into(),
                j.created.clone().map(Value::String).unwrap_or(Value::Null),
            );
            f.notes.push(RECREATE_NOTE.into());
            if j.system() {
                f.read_only = true;
                f.notes.insert(
                    0,
                    "Es un scheduler job de SAP HANA (esquema o usuario del sistema): no se cambia desde DBine.".into(),
                );
            } else if rights.is_some_and(|r| !r.alters(j)) {
                f.read_only = true;
                f.notes.insert(0, NO_ALTER.into());
            }
        }
    }
    if job.is_none() && rights.is_some_and(|r| !r.creates()) {
        f.read_only = true;
        f.notes.insert(0, NO_CREATE_ANY.into());
    }
    if !comments {
        f.notes.push(NO_COMMENTS.into());
    }
    f.notes.push(PRIVILEGES.into());
    f
}

// ---------------------------------------------------------------------------
// The contract's calls
// ---------------------------------------------------------------------------

async fn ready(conn: &Connection) -> Result<Columns> {
    let c = jobs::columns_of(conn).await?;
    if c.jobs.is_empty() {
        return Err(Error::Unsupported(jobs::NO_SCHEDULER.into()));
    }
    Ok(c)
}

/// The job `id` names, with its parameters.
async fn read(conn: &Connection, c: &Columns, id: &str) -> Result<Job> {
    let (schema, name) = parse_id(id).ok_or_else(|| {
        bad(format!(
            "«{}» no es un id de scheduler job de SAP HANA",
            id.chars().take(200).collect::<String>()
        ))
    })?;
    let set = query(conn, &jobs::jobs_sql(c, Some((&schema, &name)))).await?;
    if set.rows.is_empty() {
        return Err(bad(format!(
            "No existe el scheduler job {}.",
            display(&schema, &name)
        )));
    }
    let params = match jobs::params_sql(c, &schema, &name) {
        Some(sql) => match query(conn, &sql).await {
            Ok(p) => (0..p.rows.len())
                .filter_map(|r| {
                    Some((
                        p.get(r, "PARAMETER_NAME")?.to_string(),
                        p.get(r, "PARAMETER_VALUE").unwrap_or_default().to_string(),
                    ))
                })
                .collect(),
            Err(e) => {
                tracing::debug!("hana scheduler: SCHEDULER_JOB_PARAMETERS: {e}");
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    Job::of(&set, 0, &params).ok_or_else(|| bad("No existe ese scheduler job.".into()))
}

async fn catalog(conn: &Connection, current: &str) -> Result<Catalog> {
    let schemas = query(
        conn,
        &format!("SELECT TOP {MAX_CHOICES} SCHEMA_NAME FROM SYS.SCHEMAS ORDER BY SCHEMA_NAME"),
    )
    .await?;
    let procs = query(
        conn,
        &format!(
            "SELECT TOP {MAX_CHOICES} SCHEMA_NAME, PROCEDURE_NAME FROM SYS.PROCEDURES
              ORDER BY CASE WHEN SCHEMA_NAME = 'SYS' OR SCHEMA_NAME LIKE '\\_SYS%' ESCAPE '\\' THEN 1 ELSE 0 END, SCHEMA_NAME, PROCEDURE_NAME"
        ),
    )
    .await?;
    let current = if current.is_empty() {
        query(conn, "SELECT CURRENT_SCHEMA AS S FROM DUMMY")
            .await
            .ok()
            .and_then(|s| s.get(0, "S").map(str::to_string))
            .unwrap_or_default()
    } else {
        current.to_string()
    };
    Ok(Catalog {
        schemas: (0..schemas.rows.len())
            .filter_map(|r| schemas.get(r, "SCHEMA_NAME").map(str::to_string))
            .collect(),
        procedures: (0..procs.rows.len())
            .filter_map(|r| {
                Some((
                    procs.get(r, "SCHEMA_NAME")?.to_string(),
                    procs.get(r, "PROCEDURE_NAME")?.to_string(),
                ))
            })
            .collect(),
        current,
    })
}

/// The schema exists, the procedure exists, and its parameters.
async fn check_server(conn: &Connection, w: &Wanted) -> Result<Vec<ProcParam>> {
    let n = query(
        conn,
        &format!(
            "SELECT COUNT(*) AS N FROM SYS.SCHEMAS WHERE SCHEMA_NAME = {}",
            lit(&w.schema)
        ),
    )
    .await?;
    if n.get(0, "N") == Some("0") {
        return Err(bad(format!("No existe el esquema {}.", quote(&w.schema))));
    }
    let n = query(
        conn,
        &format!(
            "SELECT COUNT(*) AS N FROM SYS.PROCEDURES WHERE SCHEMA_NAME = {} AND PROCEDURE_NAME = {}",
            lit(&w.proc_schema),
            lit(&w.proc_name)
        ),
    )
    .await?;
    if n.get(0, "N") == Some("0") {
        return Err(bad(format!(
            "No existe el procedimiento {} (o este usuario no lo ve).",
            display(&w.proc_schema, &w.proc_name)
        )));
    }
    let p = query(
        conn,
        &format!(
            "SELECT PARAMETER_NAME, DATA_TYPE_NAME, PARAMETER_TYPE FROM SYS.PROCEDURE_PARAMETERS
              WHERE SCHEMA_NAME = {} AND PROCEDURE_NAME = {} ORDER BY POSITION",
            lit(&w.proc_schema),
            lit(&w.proc_name)
        ),
    )
    .await?;
    Ok((0..p.rows.len())
        .filter_map(|r| {
            Some(ProcParam {
                name: p.get(r, "PARAMETER_NAME")?.to_string(),
                data_type: p.get(r, "DATA_TYPE_NAME").unwrap_or_default().to_string(),
                mode: p.get(r, "PARAMETER_TYPE").unwrap_or("IN").to_string(),
            })
        })
        .collect())
}

/// The login's privileges, or `None` when they can't be read.
async fn rights(conn: &Connection) -> Option<Rights> {
    let read = async {
        let user = query(conn, "SELECT CURRENT_USER AS U FROM DUMMY").await?;
        let user = user.get(0, "U").unwrap_or_default().to_string();
        let owned = query(
            conn,
            &format!(
                "SELECT SCHEMA_NAME FROM SYS.SCHEMAS WHERE SCHEMA_OWNER = {}",
                lit(&user)
            ),
        )
        .await?;
        let privileges = query(
            conn,
            &format!(
                "SELECT PRIVILEGE, OBJECT_TYPE, SCHEMA_NAME, OBJECT_NAME FROM SYS.EFFECTIVE_PRIVILEGES
                  WHERE USER_NAME = {} AND PRIVILEGE IN ('CREATE ANY', 'ALTER', 'DROP', 'DATA ADMIN')",
                lit(&user)
            ),
        )
        .await?;
        Ok::<_, Error>(Rights::of(&user, &owned, &privileges))
    };
    match read.await {
        Ok(r) if !r.user.is_empty() => Some(r),
        Ok(_) => None,
        Err(e) => {
            tracing::debug!("hana scheduler: the login's privileges: {e}");
            None
        }
    }
}

pub(crate) async fn info(conn: &Connection) -> Result<SchedulerInfo> {
    let c = jobs::columns_of(conn).await?;
    if c.jobs.is_empty() {
        return Ok(SchedulerInfo {
            kinds: Vec::new(),
            logs: false,
            notes: vec![jobs::NO_SCHEDULER.into()],
        });
    }
    let mut notes = Vec::new();
    if !jobs::sees_all(conn).await {
        notes.push(jobs::PARTIAL.to_string());
    }
    // Unknown privileges: everything offered, the server decides. Per job,
    // the form is read-only where the login can't change it.
    let (create, change) = match rights(conn).await {
        Some(r) => (r.creates(), r.changes_any()),
        None => (true, true),
    };
    if !create {
        notes.push(NO_CREATE_ANY.into());
    }
    notes.push(PRIVILEGES.into());
    Ok(SchedulerInfo {
        kinds: vec![
            SchedulerKind::new("job", "Jobs del planificador", "job del planificador")
                .abilities(create, change, change),
        ],
        logs: false,
        notes,
    })
}

pub(crate) async fn items(conn: &Connection, kind: &str) -> Result<SchedulerList> {
    check_kind(kind)?;
    let list = jobs::list(conn).await?;
    Ok(SchedulerList {
        items: list
            .jobs
            .into_iter()
            .map(|j| SchedulerItem {
                description: j.description.clone().or(j.schedule.clone()),
                id: j.id,
                name: match &j.group {
                    Some(schema) => format!("{schema}.{}", j.name),
                    None => j.name,
                },
                enabled: Some(j.enabled),
                system: j.system,
                can_edit: None,
                can_delete: None,
                actions: None,
            })
            .collect(),
        notes: list.notes,
    })
}

pub(crate) async fn form(
    conn: &Connection,
    current: &str,
    kind: &str,
    id: Option<&str>,
) -> Result<ObjectForm> {
    check_kind(kind)?;
    let c = ready(conn).await?;
    let job = match id {
        Some(id) => Some(read(conn, &c, id).await?),
        None => None,
    };
    let cat = catalog(conn, current).await?;
    let rights = rights(conn).await;
    Ok(form_of(
        job.as_ref(),
        &cat,
        c.jobs.contains("COMMENTS"),
        rights.as_ref(),
    ))
}

async fn plan_for(conn: &Connection, id: Option<&str>, values: &FormValues) -> Result<Plan> {
    let c = ready(conn).await?;
    let comments = c.jobs.contains("COMMENTS");
    let w = wanted(values, comments)?;
    let current = match id {
        Some(id) => {
            let j = read(conn, &c, id).await?;
            if j.system() {
                return Err(bad(
                    "Es un scheduler job de SAP HANA (esquema o usuario del sistema): no se cambia desde DBine.".into(),
                ));
            }
            Some(j)
        }
        None => None,
    };
    let proc = check_server(conn, &w).await?;
    let params = w.typed(&proc)?;
    let p = plan(current.as_ref(), &w, &params, comments)?;
    check_rights(rights(conn).await.as_ref(), current.as_ref(), &w, &p)?;
    Ok(p)
}

pub(crate) async fn script(
    conn: &Connection,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<String> {
    check_kind(kind)?;
    Ok(plan_for(conn, id, values).await?.script)
}

/// What [`run`] needs from the connection (a fake records the calls in
/// the unit tests).
pub(crate) trait Conn {
    async fn sql(&self, stmt: &str) -> Result<()>;
    async fn auto_commit(&self) -> bool;
    async fn set_auto(&self, on: bool);
    /// `COMMIT` / `ROLLBACK`.
    async fn end(&self, commit: bool) -> Result<()>;
}

impl Conn for Connection {
    async fn sql(&self, stmt: &str) -> Result<()> {
        self.exec(stmt).await.map_err(crate::err)
    }
    async fn auto_commit(&self) -> bool {
        self.is_auto_commit().await
    }
    async fn set_auto(&self, on: bool) {
        self.set_auto_commit(on).await
    }
    async fn end(&self, commit: bool) -> Result<()> {
        if commit {
            self.commit().await
        } else {
            self.rollback().await
        }
        .map_err(crate::err)
    }
}

/// Runs the plan: one statement as it is; several in one transaction
/// (auto-commit off, `AUTOCOMMIT DDL OFF`, `COMMIT`, or `ROLLBACK` when
/// one fails), both settings put back afterwards. A server that refuses
/// `SET TRANSACTION AUTOCOMMIT DDL` gets them one by one, each committed by
/// itself.
pub(crate) async fn run<C: Conn>(conn: &C, plan: &Plan) -> Result<()> {
    if plan.statements.len() < 2 {
        for stmt in &plan.statements {
            conn.sql(stmt).await?;
        }
        return Ok(());
    }
    let auto = conn.auto_commit().await;
    conn.set_auto(false).await;
    if let Err(e) = conn.sql(DDL_OFF).await {
        tracing::debug!("hana scheduler: {DDL_OFF}: {e}");
        conn.set_auto(auto).await;
        return one_by_one(conn, plan).await;
    }
    let r = in_transaction(conn, plan).await;
    if let Err(e) = conn.sql(DDL_ON).await {
        tracing::debug!("hana scheduler: {DDL_ON}: {e}");
    }
    conn.set_auto(auto).await;
    r
}

async fn in_transaction<C: Conn>(conn: &C, plan: &Plan) -> Result<()> {
    let n = plan.statements.len();
    for (i, stmt) in plan.statements.iter().enumerate() {
        if let Err(e) = conn.sql(stmt).await {
            return Err(match conn.end(false).await {
                Ok(()) => bad(format!(
                    "SAP HANA rechazó la sentencia {} de {n} del script; se deshizo todo, así que no cambió nada: {e}",
                    i + 1
                )),
                Err(r) => bad(format!(
                    "SAP HANA rechazó la sentencia {} de {n} del script y no se pudieron deshacer las anteriores ({r}): {e}",
                    i + 1
                )),
            });
        }
    }
    if let Err(e) = conn.end(true).await {
        if let Err(r) = conn.end(false).await {
            tracing::debug!("hana scheduler: rollback after a failed commit: {r}");
        }
        return Err(bad(format!(
            "SAP HANA no confirmó el script; se deshizo: {e}"
        )));
    }
    Ok(())
}

async fn one_by_one<C: Conn>(conn: &C, plan: &Plan) -> Result<()> {
    for (i, stmt) in plan.statements.iter().enumerate() {
        if let Err(e) = conn.sql(stmt).await {
            if i == 0 {
                return Err(e);
            }
            return Err(bad(format!(
                "Se ejecutaron {i} de {} sentencias del script (cada una se confirmó sola); la siguiente falló: {e}",
                plan.statements.len()
            )));
        }
    }
    Ok(())
}

/// Whether the job is gone from the catalog (`false` when it can't tell).
async fn gone(conn: &Connection, schema: &str, name: &str) -> bool {
    query(
        conn,
        &format!(
            "SELECT COUNT(*) AS N FROM SYS.SCHEDULER_JOBS WHERE SCHEMA_NAME = {} AND SCHEDULER_JOB_NAME = {}",
            lit(schema),
            lit(name)
        ),
    )
    .await
    .ok()
    .is_some_and(|n| n.get(0, "N") == Some("0"))
}

pub(crate) async fn save(
    conn: &Connection,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Option<String>> {
    check_kind(kind)?;
    let plan = plan_for(conn, id, values).await?;
    if let Err(e) = run(conn, &plan).await {
        // A recreation that failed must have left the old job: say so if
        // the server kept its DROP anyway.
        if let Some((schema, name)) = &plan.drops {
            if gone(conn, schema, name).await {
                return Err(bad(format!(
                    "El scheduler job {} ya no existe: SAP HANA confirmó su borrado aunque el resto del script falló. {e}",
                    display(schema, name)
                )));
            }
        }
        return Err(e);
    }
    Ok(Some(plan.id))
}

pub(crate) async fn delete_script(conn: &Connection, kind: &str, id: &str) -> Result<String> {
    check_kind(kind)?;
    let c = ready(conn).await?;
    let j = read(conn, &c, id).await?;
    Ok(delete_plan(&j)?.script)
}

pub(crate) async fn delete(conn: &Connection, kind: &str, id: &str) -> Result<()> {
    check_kind(kind)?;
    let c = ready(conn).await?;
    let j = read(conn, &c, id).await?;
    if j.system() {
        return Err(bad(
            "Es un scheduler job de SAP HANA (esquema o usuario del sistema): no se borra desde DBine.".into(),
        ));
    }
    if rights(conn).await.is_some_and(|r| !r.drops(&j)) {
        return Err(bad(NO_DROP.into()));
    }
    run(conn, &delete_plan(&j)?).await
}
