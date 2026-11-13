//! OrientDB's scheduler managed through the scheduler contract
//! ([`dbine_driver::Session::scheduler_info`] and the rest): the
//! `OSchedule` records of the session's database ("eventos programados"),
//! each running a function (`OFunction`) on a Quartz cron rule with seconds.
//!
//! What an event has: `name`, `rule`, `function` (a link to the function),
//! `arguments` (a map from the function's parameter names to values),
//! `status`, `starttime` and `nextExecId`. The last three are the
//! scheduler's: it writes `status` (`RUNNING` / `STOPPED`) and, at every
//! run, the run's start into `starttime` and its count into `nextExecId`;
//! a `starttime` written by hand doesn't delay anything (the server
//! overwrites it at the first run), so the form shows them read-only.
//!
//! - Create: `INSERT INTO OSchedule SET …`. The server's hook schedules
//!   the event and refuses a name another event has (also one only in its
//!   memory).
//! - Edit: `UPDATE OSchedule SET … WHERE name = …` with only what changed;
//!   the hook reschedules the event when the rule changes and takes the new
//!   function and arguments otherwise. The name is never updated: the
//!   server accepts it but its scheduler keeps the event under the old
//!   name (a name that then can't be used again until a restart). A new
//!   name inserts the event under it and then deletes the old record: the
//!   run count (`nextExecId`) starts again, and the script says so.
//! - Delete: `DELETE FROM OSchedule WHERE name = …` (the hook unschedules
//!   it).
//! - No pause, no "run now", no history and no log: OrientDB has none.
//!
//! Ids are the event names, as [`crate::jobs`] lists them; an id only picks
//! one of the events the server lists, exactly. Every value goes into the
//! script as a string literal ([`string`]) or a number the driver wrote;
//! names, rules and arguments are checked before anything runs, and the
//! function must be one the database has. Comments are `/* … */` (the
//! server's parser doesn't take `--` lines).

use crate::ddl::string;
use crate::jobs::{self, Event};
use crate::{as_text, one_unit_each, OrientSession};
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result,
    SchedulerInfo, SchedulerItem, SchedulerKind, SchedulerList,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Notes and messages
// ---------------------------------------------------------------------------

const NO_DATABASE: &str =
    "Los eventos programados son de cada base: elegí una base de datos para administrarlos.";
const RENAME_NOTE: &str = "OrientDB no permite cambiar el nombre de un evento programado: con otro nombre, DBine lo crea con el nombre nuevo y borra el anterior, y su contador de ejecuciones vuelve a empezar.";
const STATE_NOTE: &str = "El estado, el último inicio y la cantidad de ejecuciones los escribe el planificador de OrientDB en cada ejecución: no se cambian a mano.";
const NO_WRITE: &str =
    "Este usuario no puede crear, cambiar ni borrar eventos programados (la clase OSchedule).";
const UNKNOWN_ACCESS: &str = "No se pudieron leer los roles de este usuario: DBine no sabe si puede cambiar los eventos programados; si no puede, el servidor lo rechazará al guardar.";
const NOT_FOUND: &str = "No existe ese evento programado en esta base.";

/// Functions listed for the form at most.
const MAX_FUNCTIONS: i64 = 5000;
/// Longest name DBine writes (OrientDB has no limit of its own).
const MAX_NAME: usize = 200;

fn bad(message: String) -> Error {
    Error::Query(message)
}

/// Text from the app, short and on one line, for a message.
fn shown(s: &str) -> String {
    crate::comment_text(s.trim()).chars().take(60).collect()
}

/// Text for a `/* … */` comment: nothing in it can end the comment.
fn comment(s: &str) -> String {
    crate::comment_text(s).replace("*/", "* /")
}

fn check_kind(kind: &str) -> Result<()> {
    if kind.trim() == "job" {
        Ok(())
    } else {
        Err(bad(format!(
            "«{}» no es un tipo de objeto del planificador de OrientDB: solo tiene eventos programados (OSchedule).",
            shown(kind)
        )))
    }
}

// ---------------------------------------------------------------------------
// What the login may do: the roles' rules on OSchedule
// ---------------------------------------------------------------------------

const CREATE: i64 = 1;
const UPDATE: i64 = 4;
const DELETE: i64 = 8;

/// What the user's roles allow on `OSchedule`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Access {
    pub create: bool,
    pub update: bool,
    pub delete: bool,
    /// The roles could be read; otherwise everything is allowed and the
    /// server decides.
    pub known: bool,
}

impl Access {
    pub(crate) const ALL: Access = Access {
        create: true,
        update: true,
        delete: true,
        known: true,
    };

    fn any(&self) -> bool {
        self.create || self.update || self.delete
    }
}

/// One role: its rules (resource → CRUD bits) and the role it inherits.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Role {
    pub rules: BTreeMap<String, i64>,
    pub parent: Option<String>,
}

/// Whether `role` allows `op` on `OSchedule`, as OrientDB's `ORole.allow`
/// reads its rules: the class's own rule first, then the rule for every
/// class, then the rule for every resource, then the inherited role.
pub(crate) fn role_allows(roles: &BTreeMap<String, Role>, role: &str, op: i64) -> bool {
    let mut at = Some(role.to_ascii_lowercase());
    let mut seen = 0;
    while let Some(name) = at.take() {
        seen += 1;
        if seen > 16 {
            return false;
        }
        let Some(r) = roles.get(&name) else {
            return false;
        };
        for key in [
            "database.class.oschedule",
            "database.class.*",
            "database.class",
            "*",
        ] {
            if let Some(bits) = r.rules.get(key) {
                return bits & op == op;
            }
        }
        at = r.parent.as_ref().map(|p| p.to_ascii_lowercase());
    }
    false
}

/// What a user with `user_roles` may do (any role allowing is enough).
pub(crate) fn access_of(roles: &BTreeMap<String, Role>, user_roles: &[String]) -> Access {
    let can = |op| user_roles.iter().any(|r| role_allows(roles, r, op));
    Access {
        create: can(CREATE),
        update: can(UPDATE),
        delete: can(DELETE),
        known: true,
    }
}

/// `SELECT name, rules, inheritedRole.name AS parent FROM ORole` rows.
pub(crate) fn roles_of(records: &[Vec<(String, Value)>]) -> BTreeMap<String, Role> {
    let get = |r: &[(String, Value)], k: &str| {
        r.iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .filter(|v| !v.is_null())
    };
    records
        .iter()
        .filter_map(|r| {
            let name = get(r, "name").map(|v| as_text(&v))?;
            let rules = get(r, "rules")
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default()
                .into_iter()
                .filter_map(|(k, v)| Some((k.to_ascii_lowercase(), v.as_i64()?)))
                .collect();
            let parent = get(r, "parent")
                .map(|v| as_text(&v))
                .filter(|p| !p.is_empty());
            Some((name.to_ascii_lowercase(), Role { rules, parent }))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The database's functions
// ---------------------------------------------------------------------------

/// A function an event may run.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Func {
    pub name: String,
    pub language: Option<String>,
    pub params: Vec<String>,
}

const FUNCTIONS: &str = "SELECT name, language, parameters FROM OFunction ORDER BY name";

pub(crate) fn functions_of(records: &[Vec<(String, Value)>]) -> Vec<Func> {
    records
        .iter()
        .filter_map(|r| {
            let get = |k: &str| r.iter().find(|(n, _)| n == k).map(|(_, v)| v);
            let name = get("name").map(as_text).filter(|n| !n.trim().is_empty())?;
            let language = get("language")
                .map(as_text)
                .filter(|l| !l.trim().is_empty());
            let params = get("parameters")
                .and_then(Value::as_array)
                .map(|a| a.iter().map(as_text).collect())
                .unwrap_or_default();
            Some(Func {
                name,
                language,
                params,
            })
        })
        .collect()
}

/// `name (LANGUAGE: a, b)` for the function's select.
fn function_label(f: &Func) -> String {
    let mut l = f.name.clone();
    match (&f.language, f.params.is_empty()) {
        (Some(lang), true) => l.push_str(&format!(" ({lang})")),
        (Some(lang), false) => l.push_str(&format!(" ({lang}: {})", f.params.join(", "))),
        (None, false) => l.push_str(&format!(" ({})", f.params.join(", "))),
        (None, true) => {}
    }
    l
}

// ---------------------------------------------------------------------------
// Values from the designer
// ---------------------------------------------------------------------------

/// A form's values (or a list row's) as the app sent them: untrusted, any
/// JSON type where a field expects another.
#[derive(Clone, Copy)]
pub(crate) struct Vals<'a>(pub &'a BTreeMap<String, Value>);

impl<'a> Vals<'a> {
    fn get(&self, k: &str) -> Option<&'a Value> {
        self.0.get(k).filter(|v| !v.is_null())
    }

    /// The value as typed.
    fn raw(&self, k: &str) -> String {
        match self.get(k) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(_) | Value::Object(_)) | None => String::new(),
            Some(v) => v.to_string(),
        }
    }

    fn text(&self, k: &str) -> String {
        self.raw(k).trim().to_string()
    }
}

/// An argument's value, typed as the form says.
pub(crate) fn argument_value(kind: &str, raw: &str, name: &str) -> Result<Value> {
    let t = raw.trim();
    match kind {
        "" | "text" => Ok(Value::String(raw.to_string())),
        "number" => {
            if let Ok(i) = t.parse::<i64>() {
                return Ok(json!(i));
            }
            t.parse::<f64>()
                .ok()
                .filter(|f| f.is_finite())
                .and_then(serde_json::Number::from_f64)
                .map(Value::Number)
                .ok_or_else(|| {
                    bad(format!(
                        "El argumento «{}» tiene que ser un número (es «{}»).",
                        shown(name),
                        shown(raw)
                    ))
                })
        }
        "boolean" => match t.to_ascii_lowercase().as_str() {
            "true" | "1" | "sí" | "si" | "yes" => Ok(Value::Bool(true)),
            "false" | "0" | "no" => Ok(Value::Bool(false)),
            _ => Err(bad(format!(
                "El argumento «{}» tiene que ser true o false (es «{}»).",
                shown(name),
                shown(raw)
            ))),
        },
        "json" => serde_json::from_str::<Value>(t).map_err(|e| {
            bad(format!(
                "El argumento «{}» no es un JSON válido: {e}.",
                shown(name)
            ))
        }),
        other => Err(bad(format!(
            "«{}» no es un tipo de argumento (texto, número, booleano o JSON).",
            shown(other)
        ))),
    }
}

/// The form's type for a value the server has.
fn argument_kind(v: &Value) -> &'static str {
    match v {
        Value::String(_) => "text",
        Value::Number(_) => "number",
        Value::Bool(_) => "boolean",
        _ => "json",
    }
}

/// The form's text for a value the server has.
fn argument_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A JSON value as an OrientDB SQL literal: strings quoted and escaped by
/// [`string`], maps with quoted keys.
pub(crate) fn literal(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => string(s),
        Value::Array(a) => format!("[{}]", a.iter().map(literal).collect::<Vec<_>>().join(", ")),
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter()
                .map(|(k, v)| format!("{}: {}", string(k), literal(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The checks of a Quartz cron rule, as OrientDB's `OCronExpression`
/// parses it (it answers a bad rule with a bare NullPointerException).
pub(crate) fn check_rule(rule: &str) -> std::result::Result<(), String> {
    const MONTHS: &[&str] = &[
        "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
    ];
    const DAYS: &[&str] = &["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
    let fields: Vec<&str> = rule.split_whitespace().collect();
    if !(6..=7).contains(&fields.len()) {
        return Err(format!(
            "La regla cron necesita 6 o 7 campos (segundos, minutos, horas, día del mes, mes, día de la semana y, si se quiere, año): tiene {}.",
            fields.len()
        ));
    }
    if rule
        .chars()
        .any(|c| c.is_control() && c != ' ' && c != '\t')
    {
        return Err("La regla cron no puede tener saltos de línea.".into());
    }
    // (min, max, names)
    let specs: [(u32, u32, &[&str]); 7] = [
        (0, 59, &[]),
        (0, 59, &[]),
        (0, 23, &[]),
        (1, 31, &[]),
        (1, 12, MONTHS),
        (1, 7, DAYS),
        (1970, 2099, &[]),
    ];
    let value = |v: &str, min: u32, max: u32, names: &[&str]| -> bool {
        if let Ok(n) = v.parse::<u32>() {
            return (min..=max).contains(&n) && v.len() <= 4;
        }
        names.iter().any(|n| n.eq_ignore_ascii_case(v))
    };
    for (i, field) in fields.iter().enumerate() {
        let (min, max, names) = specs[i];
        let wrong = || rule_field_error(i, field);
        let is_dom = i == 3;
        let is_dow = i == 5;
        if *field == "?" {
            if is_dom || is_dow {
                continue;
            }
            return Err(wrong());
        }
        for item in field.split(',') {
            if item.is_empty() {
                return Err(wrong());
            }
            // Day of month: L, L-n, LW, nW.
            if is_dom {
                let u = item.to_ascii_uppercase();
                if u == "L" || u == "LW" {
                    continue;
                }
                if let Some(off) = u.strip_prefix("L-") {
                    if off.parse::<u32>().is_ok_and(|n| n <= 30) {
                        continue;
                    }
                    return Err(wrong());
                }
                if let Some(d) = u.strip_suffix('W') {
                    if value(d, 1, 31, &[]) {
                        continue;
                    }
                    return Err(wrong());
                }
            }
            // Day of week: L, nL, n#k.
            if is_dow {
                let u = item.to_ascii_uppercase();
                if u == "L" {
                    continue;
                }
                if let Some(d) = u.strip_suffix('L') {
                    if value(d, 1, 7, DAYS) {
                        continue;
                    }
                    return Err(wrong());
                }
                if let Some((d, k)) = u.split_once('#') {
                    if value(d, 1, 7, DAYS) && k.parse::<u32>().is_ok_and(|k| (1..=5).contains(&k))
                    {
                        continue;
                    }
                    return Err(wrong());
                }
            }
            let (base, step) = match item.split_once('/') {
                Some((b, s)) => (b, Some(s)),
                None => (item, None),
            };
            if let Some(s) = step {
                if !s.parse::<u32>().is_ok_and(|s| s >= 1 && s <= max.max(1)) {
                    return Err(wrong());
                }
            }
            let ok = match base {
                "*" => true,
                _ => match base.split_once('-') {
                    Some((a, b)) => value(a, min, max, names) && value(b, min, max, names),
                    None => value(base, min, max, names),
                },
            };
            if !ok {
                return Err(wrong());
            }
        }
    }
    let dom = fields[3];
    let dow = fields[5];
    if (dom == "?") == (dow == "?") {
        return Err("En la regla cron, el día del mes o el día de la semana (uno de los dos) tiene que ser «?»: OrientDB no admite los dos ni ninguno.".into());
    }
    Ok(())
}

/// Why field `i` of a cron rule is wrong, as a whole sentence.
fn rule_field_error(i: usize, field: &str) -> String {
    let f = shown(field);
    match i {
        0 => format!("«{f}» no es un valor válido para los segundos en la regla cron (0 a 59)."),
        1 => format!("«{f}» no es un valor válido para los minutos en la regla cron (0 a 59)."),
        2 => format!("«{f}» no es un valor válido para las horas en la regla cron (0 a 23)."),
        3 => format!("«{f}» no es un valor válido para el día del mes en la regla cron (1 a 31, ?, L o W)."),
        4 => format!("«{f}» no es un valor válido para el mes en la regla cron (1 a 12 o JAN a DEC)."),
        5 => format!("«{f}» no es un valor válido para el día de la semana en la regla cron (1 a 7 o SUN a SAT, ?, L o #)."),
        _ => format!("«{f}» no es un valor válido para el año en la regla cron (1970 a 2099)."),
    }
}

/// What the designer asks for, checked.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Wanted {
    pub name: String,
    pub function: String,
    pub rule: String,
    pub arguments: Map<String, Value>,
}

/// Checks the designer's values against the database's functions.
pub(crate) fn wanted(values: &FormValues, functions: &[Func]) -> Result<Wanted> {
    let v = Vals(&values.values);
    let name = v.text("name");
    if name.is_empty() {
        return Err(bad("El evento programado necesita un nombre.".into()));
    }
    if name.chars().count() > MAX_NAME {
        return Err(bad(format!(
            "El nombre del evento programado puede tener hasta {MAX_NAME} caracteres."
        )));
    }
    if name.chars().any(|c| c.is_control()) {
        return Err(bad(
            "El nombre del evento programado no puede tener saltos de línea ni caracteres de control."
                .into(),
        ));
    }
    let function = v.text("function");
    if function.is_empty() {
        return Err(bad(
            "Elegí la función que ejecuta el evento programado.".into()
        ));
    }
    let Some(func) = functions.iter().find(|f| f.name == function) else {
        return Err(bad(format!(
            "No existe la función «{}» en esta base.",
            shown(&function)
        )));
    };
    let rule = v.text("rule");
    if rule.is_empty() {
        return Err(bad("El evento programado necesita una regla cron.".into()));
    }
    check_rule(&rule).map_err(bad)?;
    let rule = rule.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut arguments = Map::new();
    for row in values.lists.get("arguments").into_iter().flatten() {
        let r = Vals(row);
        let pname = r.text("name");
        if pname.is_empty() {
            return Err(bad(
                "Cada argumento necesita el nombre de un parámetro.".into()
            ));
        }
        if pname.chars().any(|c| c.is_control()) || pname.chars().count() > MAX_NAME {
            return Err(bad(format!(
                "«{}» no es un nombre de parámetro válido.",
                shown(&pname)
            )));
        }
        if !func.params.is_empty() && !func.params.contains(&pname) {
            return Err(bad(format!(
                "La función «{}» no tiene el parámetro «{}» (tiene: {}).",
                shown(&func.name),
                shown(&pname),
                func.params.join(", ")
            )));
        }
        if arguments.contains_key(&pname) {
            return Err(bad(format!(
                "El parámetro «{}» está dos veces en los argumentos.",
                shown(&pname)
            )));
        }
        let value = argument_value(&r.text("type"), &r.raw("value"), &pname)?;
        arguments.insert(pname, value);
    }
    Ok(Wanted {
        name,
        function,
        rule,
        arguments,
    })
}

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

/// What a change writes: its statements (run one by one, in order), the
/// script shown (a comment, then the statements), and the event's id.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    pub script: String,
    pub statements: Vec<String>,
    pub id: String,
}

impl Plan {
    fn new(header: &str, statements: Vec<String>, id: String) -> Result<Plan> {
        one_unit_each(&statements)?;
        let mut script = format!("/* {} */\n", comment(header));
        for s in &statements {
            script.push_str(s);
            script.push_str(";\n");
        }
        Ok(Plan {
            script,
            statements,
            id,
        })
    }

    fn nothing(name: &str) -> Plan {
        Plan {
            script: format!("/* «{}»: no hay cambios que guardar. */\n", comment(name)),
            statements: Vec::new(),
            id: name.to_string(),
        }
    }
}

fn function_ref(name: &str) -> String {
    format!("(SELECT FROM OFunction WHERE name = {})", string(name))
}

pub(crate) fn insert_statement(w: &Wanted) -> String {
    let mut s = format!(
        "INSERT INTO OSchedule SET name = {}, rule = {}, function = {}",
        string(&w.name),
        string(&w.rule),
        function_ref(&w.function)
    );
    if !w.arguments.is_empty() {
        s.push_str(&format!(
            ", arguments = {}",
            literal(&Value::Object(w.arguments.clone()))
        ));
    }
    s
}

pub(crate) fn delete_statement(name: &str) -> String {
    format!("DELETE FROM OSchedule WHERE name = {}", string(name))
}

/// The event's arguments as the server keeps them (none: an empty map).
fn current_arguments(e: &Event) -> Map<String, Value> {
    e.arguments
        .as_ref()
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// Two argument maps hold the same values (`3` and `3.0` are one number).
fn same_arguments(a: &Map<String, Value>, b: &Map<String, Value>) -> bool {
    a.len() == b.len()
        && a.iter().all(|(k, v)| {
            b.get(k).is_some_and(|w| match (v, w) {
                (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
                (x, y) => x == y,
            })
        })
}

/// The script for `w`: a new event (`current` `None`), or the change of
/// `current` in place; `events` are the database's, to refuse a name
/// already taken.
pub(crate) fn plan(
    current: Option<&Event>,
    w: &Wanted,
    events: &[Event],
    database: &str,
) -> Result<Plan> {
    let taken = |name: &str| events.iter().any(|e| e.name == name);
    let Some(cur) = current else {
        if taken(&w.name) {
            return Err(bad(format!(
                "Ya hay un evento programado «{}» en esta base.",
                shown(&w.name)
            )));
        }
        return Plan::new(
            &format!("Evento programado «{}» de la base «{database}»", w.name),
            vec![insert_statement(w)],
            w.name.clone(),
        );
    };
    if w.name != cur.name {
        if taken(&w.name) {
            return Err(bad(format!(
                "Ya hay un evento programado «{}» en esta base.",
                shown(&w.name)
            )));
        }
        return Plan::new(
            &format!(
                "Evento programado «{}» de la base «{database}», con el nombre nuevo «{}». OrientDB no permite cambiar el nombre de un evento: se crea con el nombre nuevo y se borra el anterior; su contador de ejecuciones (nextExecId) vuelve a empezar.",
                cur.name, w.name
            ),
            vec![insert_statement(w), delete_statement(&cur.name)],
            w.name.clone(),
        );
    }
    let mut sets = Vec::new();
    let cur_rule = cur
        .rule
        .as_deref()
        .map(|r| r.split_whitespace().collect::<Vec<_>>().join(" "));
    if cur_rule.as_deref() != Some(w.rule.as_str()) {
        sets.push(format!("rule = {}", string(&w.rule)));
    }
    if cur.function.as_deref() != Some(w.function.as_str()) {
        sets.push(format!("function = {}", function_ref(&w.function)));
    }
    if !same_arguments(&current_arguments(cur), &w.arguments) {
        sets.push(format!(
            "arguments = {}",
            literal(&Value::Object(w.arguments.clone()))
        ));
    }
    if sets.is_empty() {
        return Ok(Plan::nothing(&cur.name));
    }
    Plan::new(
        &format!(
            "Evento programado «{}» de la base «{database}»: cambia en el lugar",
            cur.name
        ),
        vec![format!(
            "UPDATE OSchedule SET {} WHERE name = {}",
            sets.join(", "),
            string(&cur.name)
        )],
        cur.name.clone(),
    )
}

pub(crate) fn delete_plan(e: &Event, database: &str) -> Result<Plan> {
    Plan::new(
        &format!(
            "Borra el evento programado «{}» de la base «{database}»",
            e.name
        ),
        vec![delete_statement(&e.name)],
        e.name.clone(),
    )
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

pub(crate) fn form_of(
    event: Option<&Event>,
    functions: &[Func],
    access: Access,
    database: &str,
) -> ObjectForm {
    let mut f = ObjectForm::default();
    let existing = event.is_some();
    let mut choices: Vec<(String, String)> = functions
        .iter()
        .map(|fu| (fu.name.clone(), function_label(fu)))
        .collect();
    if let Some(fname) = event.and_then(|e| e.function.clone()) {
        if !choices.iter().any(|(v, _)| *v == fname) {
            choices.push((fname.clone(), fname));
        }
    }
    f.choices.insert("function".into(), choices);

    let mut general = FormSection::new("general", "General").fields(vec![
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .help("Único en la base. Cambiarlo crea el evento con el nombre nuevo y borra el anterior."),
        FormField::new("function", "Función", FormKind::reference("function", false))
            .required()
            .help("La función de la base (OFunction) que ejecuta el evento."),
        FormField::new("rule", "Regla (cron)", FormKind::Cron)
            .required()
            .placeholder("0 0 3 * * ?")
            .help("Expresión cron de Quartz, con segundos: segundos, minutos, horas, día del mes, mes, día de la semana y, si se quiere, año. El día del mes o el de la semana tiene que ser «?». Ej.: 0 0/5 * * * ? cada cinco minutos."),
        FormField::new("database", "Base de datos", FormKind::Text)
            .read_only(true)
            .help("Cada base tiene sus eventos programados."),
    ]);
    if existing {
        general = general.fields(vec![
            FormField::new("status", "Estado", FormKind::Text).read_only(true),
            FormField::new("last_start", "Último inicio", FormKind::DateTime).read_only(true),
            FormField::new("runs", "Ejecuciones", FormKind::number(Some(0.0), None))
                .read_only(true),
        ]);
    }
    let arguments = FormList::new(
        "arguments",
        "Argumentos",
        "name",
        vec![
            FormField::new("name", "Parámetro", FormKind::Text)
                .required()
                .help("Un parámetro de la función: el evento le pasa este valor."),
            FormField::new(
                "type",
                "Tipo",
                FormKind::select(&[
                    ("text", "Texto"),
                    ("number", "Número"),
                    ("boolean", "Booleano"),
                    ("json", "JSON"),
                ]),
            )
            .default(json!("text")),
            FormField::new("value", "Valor", FormKind::Text),
        ],
    );
    let rows: Vec<BTreeMap<String, Value>> = event
        .map(current_arguments)
        .unwrap_or_default()
        .iter()
        .map(|(k, v)| {
            BTreeMap::from([
                ("name".to_string(), json!(k)),
                ("type".to_string(), json!(argument_kind(v))),
                ("value".to_string(), json!(argument_text(v))),
            ])
        })
        .collect();
    f.sections = vec![
        general,
        FormSection::new("arguments", "Argumentos").list(arguments.rows(rows)),
    ];

    f.values.insert("database".into(), json!(database));
    match event {
        None => {
            f.values.insert("name".into(), json!(""));
            f.values.insert("function".into(), json!(""));
            f.values.insert("rule".into(), json!(""));
        }
        Some(e) => {
            f.values.insert("name".into(), json!(e.name));
            f.values.insert(
                "function".into(),
                json!(e.function.clone().unwrap_or_default()),
            );
            f.values
                .insert("rule".into(), json!(e.rule.clone().unwrap_or_default()));
            f.values
                .insert("status".into(), json!(e.status.clone().unwrap_or_default()));
            let ran = e.runs.unwrap_or(0) > 0;
            f.values.insert(
                "last_start".into(),
                e.start_ms
                    .filter(|_| ran)
                    .map(|ms| json!(jobs::utc_ms(ms)))
                    .unwrap_or(Value::Null),
            );
            f.values.insert("runs".into(), json!(e.runs.unwrap_or(0)));
        }
    }

    f.notes.push(jobs::EVENT_NOTE.into());
    if existing {
        f.notes.push(RENAME_NOTE.into());
        f.notes.push(STATE_NOTE.into());
    }
    let allowed = if existing {
        access.update
    } else {
        access.create
    };
    if !allowed {
        f.read_only = true;
        f.notes.insert(0, NO_WRITE.into());
    } else if !access.known {
        f.notes.push(UNKNOWN_ACCESS.into());
    }
    f
}

// ---------------------------------------------------------------------------
// The contract's calls
// ---------------------------------------------------------------------------

impl OrientSession {
    /// What the login's roles allow on `OSchedule`; [`Access::ALL`] with
    /// `known` false when they can't be read (a server user such as root
    /// isn't in `OUser` and may do everything).
    async fn scheduler_access(&self) -> Access {
        let Some(user) = self.auth.as_ref().map(|(u, _)| u.clone()) else {
            return Access::ALL;
        };
        let users = match self
            .command(
                &format!(
                    "SELECT roles.name AS roles FROM OUser WHERE name = {}",
                    string(&user)
                ),
                1,
            )
            .await
        {
            Ok(r) => r.records,
            Err(_) => {
                return Access {
                    known: false,
                    ..Access::ALL
                }
            }
        };
        let Some(user_roles) = users.first().map(|r| {
            r.iter()
                .find(|(k, _)| k == "roles")
                .and_then(|(_, v)| v.as_array().cloned())
                .unwrap_or_default()
                .iter()
                .map(as_text)
                .collect::<Vec<_>>()
        }) else {
            // A server user (root…): not a database user.
            return Access::ALL;
        };
        match self
            .command(
                "SELECT name, rules, inheritedRole.name AS parent FROM ORole",
                1000,
            )
            .await
        {
            Ok(r) => access_of(&roles_of(&r.records), &user_roles),
            Err(_) => Access {
                known: false,
                ..Access::ALL
            },
        }
    }

    async fn scheduler_functions(&self) -> Result<Vec<Func>> {
        Ok(functions_of(
            &self.command(FUNCTIONS, MAX_FUNCTIONS).await?.records,
        ))
    }

    /// The event `id` names, exactly (ids are names).
    async fn scheduler_event(&self, id: &str) -> Result<(Event, Vec<Event>)> {
        let all = self.events().await?;
        let e = all
            .iter()
            .find(|e| e.name == id)
            .cloned()
            .ok_or_else(|| bad(NOT_FOUND.into()))?;
        Ok((e, all))
    }

    pub(crate) async fn sched_info(&mut self) -> Result<SchedulerInfo> {
        if self.db.is_empty() {
            return Ok(SchedulerInfo {
                kinds: Vec::new(),
                logs: false,
                notes: vec![NO_DATABASE.into()],
            });
        }
        let mut notes = vec![format!(
            "Se administran los eventos programados de la base «{}»; cada base tiene los suyos.",
            self.db
        )];
        let access = match self.events().await {
            Ok(_) => self.scheduler_access().await,
            Err(e) if jobs::denied(&e) => {
                notes.push(jobs::DENIED.into());
                Access {
                    create: false,
                    update: false,
                    delete: false,
                    known: true,
                }
            }
            Err(e) => return Err(e),
        };
        if access.known && !access.any() {
            if !notes.iter().any(|n| n == jobs::DENIED) {
                notes.push(NO_WRITE.into());
            }
        } else if !access.known {
            notes.push(UNKNOWN_ACCESS.into());
        }
        notes.push(jobs::EVENT_NOTE.into());
        Ok(SchedulerInfo {
            kinds: vec![
                SchedulerKind::new("job", "Eventos programados", "evento programado").abilities(
                    access.create,
                    access.update,
                    access.delete,
                ),
            ],
            logs: false,
            notes,
        })
    }

    pub(crate) async fn sched_items(&mut self, kind: &str) -> Result<SchedulerList> {
        check_kind(kind)?;
        if self.db.is_empty() {
            return Ok(SchedulerList {
                items: Vec::new(),
                notes: vec![NO_DATABASE.into()],
            });
        }
        let all = match self.events().await {
            Ok(all) => all,
            Err(e) if jobs::denied(&e) => {
                return Ok(SchedulerList {
                    items: Vec::new(),
                    notes: vec![jobs::DENIED.into()],
                })
            }
            Err(e) => return Err(e),
        };
        let mut notes = Vec::new();
        if all.len() as i64 > jobs::MAX_JOBS {
            notes.push(format!(
                "Se muestran los primeros {} eventos programados.",
                jobs::MAX_JOBS
            ));
        }
        Ok(SchedulerList {
            items: all
                .iter()
                .take(jobs::MAX_JOBS as usize)
                .map(|e| SchedulerItem {
                    id: e.name.clone(),
                    name: e.name.clone(),
                    description: match (&e.function, &e.rule) {
                        (Some(f), Some(r)) => Some(format!("{f} · {r}")),
                        (Some(f), None) => Some(f.clone()),
                        (None, r) => r.clone(),
                    },
                    enabled: None,
                    system: false,
                    can_edit: None,
                    can_delete: None,
                    actions: None,
                    run_from_step: None,
                    clear_history: None,
                })
                .collect(),
            notes,
        })
    }

    pub(crate) async fn sched_form(&mut self, kind: &str, id: Option<&str>) -> Result<ObjectForm> {
        check_kind(kind)?;
        self.db_seg()?;
        let event = match id {
            Some(id) => Some(self.scheduler_event(id).await?.0),
            None => None,
        };
        let functions = self.scheduler_functions().await?;
        let access = self.scheduler_access().await;
        Ok(form_of(event.as_ref(), &functions, access, &self.db))
    }

    async fn sched_plan(&self, id: Option<&str>, values: &FormValues) -> Result<Plan> {
        self.db_seg()?;
        let functions = self.scheduler_functions().await?;
        let w = wanted(values, &functions)?;
        match id {
            None => plan(None, &w, &self.events().await?, &self.db),
            Some(id) => {
                let (cur, all) = self.scheduler_event(id).await?;
                plan(Some(&cur), &w, &all, &self.db)
            }
        }
    }

    pub(crate) async fn sched_script(
        &mut self,
        kind: &str,
        id: Option<&str>,
        values: &FormValues,
    ) -> Result<String> {
        check_kind(kind)?;
        Ok(self.sched_plan(id, values).await?.script)
    }

    /// Runs the statements in order. A rename inserts the new event first,
    /// so a refused insert changes nothing.
    async fn run_plan(&self, plan: &Plan) -> Result<()> {
        for (i, stmt) in plan.statements.iter().enumerate() {
            if let Err(e) = self.command(stmt, -1).await {
                if i == 0 {
                    return Err(e);
                }
                return Err(bad(format!(
                    "Se guardó una parte del cambio, pero el servidor rechazó la sentencia {} del script: {e}",
                    i + 1
                )));
            }
        }
        Ok(())
    }

    pub(crate) async fn sched_save(
        &mut self,
        kind: &str,
        id: Option<&str>,
        values: &FormValues,
    ) -> Result<Option<String>> {
        check_kind(kind)?;
        let plan = self.sched_plan(id, values).await?;
        self.run_plan(&plan).await?;
        Ok(Some(plan.id))
    }

    pub(crate) async fn sched_delete_script(&mut self, kind: &str, id: &str) -> Result<String> {
        check_kind(kind)?;
        self.db_seg()?;
        let (e, _) = self.scheduler_event(id).await?;
        Ok(delete_plan(&e, &self.db)?.script)
    }

    pub(crate) async fn sched_delete(&mut self, kind: &str, id: &str) -> Result<()> {
        check_kind(kind)?;
        self.db_seg()?;
        let (e, _) = self.scheduler_event(id).await?;
        self.run_plan(&delete_plan(&e, &self.db)?).await
    }
}
