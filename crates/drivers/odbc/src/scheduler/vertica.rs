//! Vertica (11.1.1 on): scheduled execution of stored procedures, managed
//! whole. A schedule (`CREATE SCHEDULE … USING CRON '…' | USING
//! DATETIMES(…)`) fires the one trigger attached to it (`CREATE TRIGGER …
//! ON SCHEDULE … EXECUTE PROCEDURE …(args) AS DEFINER`); triggers are the
//! jobs, schedules their own kind. `ALTER SCHEDULE` renames it, moves it
//! to another schema, changes its owner or its times in place; `ALTER
//! TRIGGER` renames, moves and changes the owner; `ENABLE_TRIGGER` turns a
//! trigger on and off. A trigger's procedure, arguments or schedule are
//! changed by dropping it and creating it again: Vertica keeps no history
//! of the runs, so only the trigger's owner and enabled state need to be
//! set again, which the script does. DDL commits on its own in Vertica: a
//! save's statements run one after the other.
//!
//! All of it is a superuser's: for anyone else the kinds are read-only.
//! Ids are `"schema"."name"`, parsed back strictly and looked up bound;
//! names go into the DDL quoted, the meta-functions get them as literals.

use super::*;
use crate::jobs::vertica::{
    best_effort, datetimes, meta_name, q, safe_args, schedule_row, superuser, table, CONFIG,
    NOT_HERE, OFF, ONE, SCHEDULES, SUPERUSER_ONLY, TRIGGERS,
};
use crate::jobs::{lit, not_found, parse_pair, parse_ts, quote_pair, yes};
use dbine_driver::{FormField, FormKind, FormList, FormSection, SchedulerKind};
use serde_json::json;

const ONLY_SUPERUSER: &str =
    "Solo un superusuario crea, cambia o borra triggers programados y programaciones en Vertica.";
const RECREATE: &str = "Vertica no cambia el procedimiento, los argumentos ni la programación de un trigger: para cambiarlos se borra y se crea de nuevo (no guarda historial que se pierda); el script vuelve a poner su dueño y su estado.";
const ARGS_HELP: &str = "Valores literales separados por comas: números, NULL, TRUE, FALSE o textos entre comillas simples (con las comillas de adentro dobladas).";
const ARGS_UNREADABLE: &str = "El catálogo de Vertica guarda los argumentos de este trigger sin doblar sus comillas: para cambiar el procedimiento o la programación, escribilos de nuevo.";

pub(super) enum Kind {
    Trigger,
    Schedule,
}

pub(super) fn kind(kind: &str) -> Result<Kind> {
    match kind.trim() {
        "job" => Ok(Kind::Trigger),
        "schedule" => Ok(Kind::Schedule),
        other => Err(bad_kind(other)),
    }
}

pub(super) fn info(src: &mut dyn JobSource) -> Result<SchedulerInfo> {
    let su = superuser(src);
    if let Err(e) = src.query(TRIGGERS, &[]) {
        let note = match table(src) {
            Some(None) => NOT_HERE,
            // Refused to someone who isn't a superuser.
            _ if !su => SUPERUSER_ONLY,
            _ => return Err(e),
        };
        return Ok(SchedulerInfo {
            kinds: Vec::new(),
            logs: false,
            notes: vec![note.to_string()],
        });
    }
    let mut notes = Vec::new();
    if !su {
        notes.push(ONLY_SUPERUSER.to_string());
    }
    if best_effort(src, CONFIG).get(0, "V") == Some("0") {
        notes.push(OFF.to_string());
    }
    Ok(SchedulerInfo {
        kinds: vec![
            SchedulerKind::new("job", "Triggers programados", "trigger programado")
                .abilities(su, su, su),
            SchedulerKind::new("schedule", "Programaciones", "programación").abilities(su, su, su),
        ],
        logs: false,
        notes,
    })
}

// -------------------------------------------------------------- choices

pub(super) const PROCEDURE_LIST: &str = "SELECT schema_name, procedure_name, procedure_arguments FROM v_catalog.user_procedures ORDER BY schema_name, procedure_name";
pub(super) const SCHEMA_LIST: &str =
    "SELECT schema_name FROM v_catalog.schemata WHERE NOT is_system_schema ORDER BY schema_name";
pub(super) const USER_LIST: &str = "SELECT user_name FROM v_catalog.users ORDER BY user_name";

fn pairs_of(s: &Set, col: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = (0..s.rows.len())
        .filter_map(|r| s.get(r, col))
        .map(|v| (v.to_string(), v.to_string()))
        .collect();
    out.dedup();
    out
}

/// What the forms pick from: schemas, users, procedures and schedules
/// (`"schema"."name"`).
fn choices(src: &mut dyn JobSource) -> BTreeMap<String, Vec<(String, String)>> {
    let mut out = BTreeMap::new();
    out.insert(
        "schema".into(),
        pairs_of(&best_effort(src, SCHEMA_LIST), "schema_name"),
    );
    out.insert(
        "user".into(),
        pairs_of(&best_effort(src, USER_LIST), "user_name"),
    );
    let p = best_effort(src, PROCEDURE_LIST);
    let mut procs: Vec<(String, String)> = Vec::new();
    for r in 0..p.rows.len() {
        let (Some(s), Some(n)) = (p.get(r, "schema_name"), p.get(r, "procedure_name")) else {
            continue;
        };
        let value = quote_pair(s, n);
        let label = format!(
            "{s}.{n}({})",
            p.get(r, "procedure_arguments").unwrap_or_default()
        );
        match procs.iter_mut().find(|(v, _)| *v == value) {
            Some((_, l)) => {
                l.push_str(" | ");
                l.push_str(&label);
            }
            None => procs.push((value, label)),
        }
    }
    out.insert("procedure".into(), procs);
    let sch = best_effort(src, SCHEDULES);
    let scheds = (0..sch.rows.len())
        .filter_map(|r| {
            let (s, n) = (sch.get(r, "schema_name")?, sch.get(r, "schedule_name")?);
            let mut label = format!("{s}.{n}");
            if let Some(u) = using_of(&sch, r) {
                label.push_str(&format!(" — {}", u.clause()));
            }
            if let Some(t) = sch.get(r, "attached_trigger") {
                label.push_str(&format!(" (trigger {t})"));
            }
            Some((quote_pair(s, n), label))
        })
        .collect();
    out.insert("schedule".into(), scheds);
    out
}

/// A `"schema"."name"` picked from a list of the server's.
fn pick_pair(
    v: Vals,
    k: &str,
    label: &str,
    list: Option<&Vec<(String, String)>>,
) -> Result<(String, String)> {
    let t = required(v, k, label, 600)?;
    let pair = parse_pair(&t).or_else(|_| {
        // A plain `schema.name` typed by hand.
        t.split_once('.')
            .filter(|(a, b)| !a.is_empty() && !b.is_empty() && !b.contains('.'))
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .ok_or_else(|| bad(format!("«{label}»: «{}» no es esquema.nombre.", short(&t))))
    })?;
    if let Some(l) = list.filter(|l| !l.is_empty()) {
        let value = quote_pair(&pair.0, &pair.1);
        if !l.iter().any(|(v, _)| v.eq_ignore_ascii_case(&value)) {
            return Err(bad(format!(
                "«{label}»: no existe «{}.{}».",
                short(&pair.0),
                short(&pair.1)
            )));
        }
    }
    Ok(pair)
}

/// A schema or user name the server has, when its list could be read.
fn pick_name(
    v: Vals,
    k: &str,
    label: &str,
    list: Option<&Vec<(String, String)>>,
    default: Option<&str>,
) -> Result<Option<String>> {
    let Some(t) = optional(v, k, label, 128)?.or(default.map(str::to_string)) else {
        return Ok(None);
    };
    match list.filter(|l| !l.is_empty()) {
        None => Ok(Some(t)),
        Some(l) => l
            .iter()
            .find(|(v, _)| *v == t)
            .or_else(|| l.iter().find(|(v, _)| v.eq_ignore_ascii_case(&t)))
            .map(|(v, _)| Some(v.clone()))
            .ok_or_else(|| bad(format!("«{label}»: no existe «{}».", short(&t)))),
    }
}

/// A trigger or schedule name: one line, 128 characters at most.
fn object_name(v: Vals, k: &str, label: &str) -> Result<String> {
    required(v, k, label, 128)
}

// ------------------------------------------------------------- schedules

/// When a schedule fires.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Using {
    Cron(String),
    /// Local times, ms.
    Times(Vec<i64>),
}

impl Using {
    pub(super) fn clause(&self) -> String {
        match self {
            Using::Cron(c) => format!("USING CRON {}", lit(c)),
            Using::Times(t) => format!(
                "USING DATETIMES({})",
                t.iter()
                    .map(|ms| lit(&crate::jobs::plain(*ms)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

fn using_of(sch: &Set, r: usize) -> Option<Using> {
    let text = sch.get(r, "date_time_string")?;
    let kind = sch
        .get(r, "date_time_type")
        .unwrap_or_default()
        .to_ascii_uppercase();
    Some(if kind.contains("LIST") || kind.contains("DATE") {
        Using::Times(datetimes(text).into_iter().filter_map(parse_ts).collect())
    } else {
        Using::Cron(text.split_whitespace().collect::<Vec<_>>().join(" "))
    })
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Schedule {
    pub(super) schema: String,
    pub(super) name: String,
    pub(super) using: Using,
    pub(super) owner: Option<String>,
}

pub(super) fn schedule_items(src: &mut dyn JobSource) -> Result<SchedulerList> {
    let s = src.query(SCHEDULES, &[])?;
    let mut items: Vec<SchedulerItem> = (0..s.rows.len())
        .filter_map(|r| {
            let (schema, name) = (s.get(r, "schema_name")?, s.get(r, "schedule_name")?);
            Some(SchedulerItem {
                id: quote_pair(schema, name),
                name: format!("{schema}.{name}"),
                description: using_of(&s, r).map(|u| u.clause()),
                enabled: None,
                system: false,
            })
        })
        .collect();
    items.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(SchedulerList {
        items,
        notes: Vec::new(),
    })
}

/// Schedule `id`, the trigger attached to it.
fn read_schedule(src: &mut dyn JobSource, id: &str) -> Result<(Schedule, Option<String>)> {
    let (schema, name) = parse_pair(id)?;
    let s = src.query(
        "SELECT * FROM v_catalog.user_schedules WHERE schema_name = ? AND schedule_name = ?",
        &[&schema, &name],
    )?;
    if s.rows.is_empty() {
        return Err(not_found(id));
    }
    let using = using_of(&s, 0).unwrap_or(Using::Cron(String::new()));
    Ok((
        Schedule {
            schema,
            name,
            using,
            owner: s.get(0, "owner").map(str::to_string),
        },
        s.get(0, "attached_trigger").map(str::to_string),
    ))
}

pub(super) fn schedule_values(s: &Schedule) -> BTreeMap<String, Value> {
    let (kind, cron) = match &s.using {
        Using::Cron(c) => ("cron", Some(c.as_str())),
        Using::Times(_) => ("datetimes", None),
    };
    values(vec![
        ("name", json!(s.name)),
        ("schema", json!(s.schema)),
        ("owner", jtext(s.owner.as_deref())),
        ("kind", json!(kind)),
        ("cron", jtext(cron)),
    ])
}

fn times_rows(s: &Schedule) -> Vec<BTreeMap<String, Value>> {
    match &s.using {
        Using::Times(t) => t
            .iter()
            .map(|ms| BTreeMap::from([("at".to_string(), json!(dt_value(*ms)))]))
            .collect(),
        Using::Cron(_) => Vec::new(),
    }
}

pub(super) fn schedule_sections(
    existing: bool,
    times: Vec<BTreeMap<String, Value>>,
) -> Vec<FormSection> {
    let mut general = FormSection::new("general", "General").fields(vec![
        FormField::new("name", "Nombre", FormKind::Text).required(),
        FormField::new("schema", "Esquema", FormKind::reference("schema", false))
            .required()
            .default(json!("public")),
        FormField::new("owner", "Dueño", FormKind::reference("user", false))
            .help("Vacío al crearla: quien la crea."),
        FormField::new(
            "kind",
            "Se dispara",
            FormKind::select(&[
                ("cron", "Según una expresión cron"),
                ("datetimes", "En fechas y horas fijas"),
            ]),
        )
        .default(json!("cron")),
        FormField::new("cron", "Expresión cron", FormKind::Cron)
            .when("kind", &["cron"])
            .help("Minuto, hora, día del mes, mes y día de la semana.")
            .placeholder("0 3 * * *"),
    ]);
    if existing {
        general = general
            .field(FormField::new("trigger", "Trigger que la usa", FormKind::Text).read_only(true));
    }
    vec![
        general,
        FormSection::new("times", "Fechas y horas").list(
            FormList::new(
                "times",
                "Fechas y horas fijas",
                "at",
                vec![FormField::new("at", "Momento", FormKind::DateTime).required()],
            )
            .rows(times)
            .orderable(),
        ),
    ]
}

fn schedule_form(src: &mut dyn JobSource, id: Option<&str>, su: bool) -> Result<ObjectForm> {
    let choices = choices(src);
    let (s, attached) = match id {
        None => (
            Schedule {
                schema: "public".into(),
                name: String::new(),
                using: Using::Cron(String::new()),
                owner: None,
            },
            None,
        ),
        Some(id) => read_schedule(src, id)?,
    };
    let mut v = schedule_values(&s);
    if id.is_some() {
        v.insert("trigger".into(), jtext(attached.as_deref()));
    }
    let mut notes = Vec::new();
    if !su {
        notes.push(ONLY_SUPERUSER.to_string());
    }
    Ok(ObjectForm {
        sections: schedule_sections(id.is_some(), times_rows(&s)),
        values: v,
        choices,
        notes,
        read_only: !su,
    })
}

pub(super) fn schedule_from_values(
    values: &FormValues,
    choices: &BTreeMap<String, Vec<(String, String)>>,
) -> Result<Schedule> {
    let v = Vals(&values.values);
    let name = object_name(v, "name", "Nombre")?;
    let schema = pick_name(
        v,
        "schema",
        "Esquema",
        choices.get("schema"),
        Some("public"),
    )?
    .unwrap_or_else(|| "public".into());
    let owner = pick_name(v, "owner", "Dueño", choices.get("user"), None)?;
    let kind = choice(v, "kind", "Se dispara", &["cron", "datetimes"], "cron")?;
    let using = if kind == "cron" {
        Using::Cron(
            cron(v, "cron", "Expresión cron", 5..=5)?
                .ok_or_else(|| bad("Falta «Expresión cron»."))?,
        )
    } else {
        let mut times = Vec::new();
        for row in rows(values, "times") {
            if let Some(t) = datetime(row, "at", "Momento")? {
                if !times.contains(&t) {
                    times.push(t);
                }
            }
        }
        if times.is_empty() {
            return Err(bad("Faltan las «Fechas y horas fijas»."));
        }
        Using::Times(times)
    };
    Ok(Schedule {
        schema,
        name,
        using,
        owner,
    })
}

pub(super) fn create_schedule_plan(s: &Schedule) -> Plan {
    let mut p = Plan::new(&format!(
        "Programación «{}.{}» de Vertica",
        s.schema, s.name
    ));
    let id = quote_pair(&s.schema, &s.name);
    p.exec(format!("CREATE SCHEDULE {id} {}", s.using.clause()));
    if let Some(o) = &s.owner {
        p.exec(format!("ALTER SCHEDULE {id} OWNER TO {}", q(o)));
    }
    p.id = Some(id);
    p
}

pub(super) fn alter_schedule_plan(cur: &Schedule, s: &Schedule) -> Plan {
    let mut id = quote_pair(&cur.schema, &cur.name);
    let mut steps = Vec::new();
    if s.using != cur.using {
        steps.push(format!("ALTER SCHEDULE {id} {}", s.using.clause()));
    }
    if let Some(o) = s.owner.as_ref().filter(|o| Some(*o) != cur.owner.as_ref()) {
        steps.push(format!("ALTER SCHEDULE {id} OWNER TO {}", q(o)));
    }
    if s.name != cur.name {
        steps.push(format!("ALTER SCHEDULE {id} RENAME TO {}", q(&s.name)));
        id = quote_pair(&cur.schema, &s.name);
    }
    if s.schema != cur.schema {
        steps.push(format!("ALTER SCHEDULE {id} SET SCHEMA {}", q(&s.schema)));
        id = quote_pair(&s.schema, &s.name);
    }
    if steps.is_empty() {
        return Plan::nothing(&format!("{}.{}", cur.schema, cur.name), id);
    }
    let mut p = Plan::new(&format!(
        "Programación «{}.{}» de Vertica",
        cur.schema, cur.name
    ));
    for st in steps {
        p.exec(st);
    }
    p.id = Some(id);
    p
}

// -------------------------------------------------------------- triggers

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Trigger {
    pub(super) schema: String,
    pub(super) name: String,
    pub(super) schedule: (String, String),
    pub(super) procedure: (String, String),
    /// The arguments as the form has them (as the catalog keeps them, for
    /// one read from the server).
    pub(super) args: String,
    pub(super) enabled: bool,
    pub(super) owner: Option<String>,
}

/// The arguments as a call takes them: literals separated by commas,
/// written back in one shape (`7, 'it''s', NULL`).
pub(super) fn parse_args(text: &str) -> Result<String> {
    let invalid = || {
        bad(format!(
            "«Argumentos»: «{}» no es una lista de valores literales.",
            short(text)
        ))
    };
    let mut out: Vec<String> = Vec::new();
    let mut rest = text.trim();
    if rest.is_empty() {
        return Ok(String::new());
    }
    loop {
        let (tok, after) = if let Some(r) = rest.strip_prefix('\'') {
            let mut s = String::new();
            let mut it = r.char_indices().peekable();
            let mut end = None;
            while let Some((i, c)) = it.next() {
                if c == '\'' {
                    if matches!(it.peek(), Some((_, '\''))) {
                        it.next();
                        s.push('\'');
                        continue;
                    }
                    end = Some(i + 1);
                    break;
                }
                if c.is_control() {
                    return Err(invalid());
                }
                s.push(c);
            }
            let end = end.ok_or_else(invalid)?;
            (lit(&s), &r[end..])
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            let t = rest[..end].trim();
            let word = ["NULL", "TRUE", "FALSE"]
                .iter()
                .find(|w| t.eq_ignore_ascii_case(w));
            let number = t.parse::<f64>().is_ok_and(f64::is_finite)
                && t.bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'+' | b'e' | b'E'));
            match (word, number) {
                (Some(w), _) => (w.to_string(), &rest[end..]),
                (None, true) => (t.to_string(), &rest[end..]),
                _ => return Err(invalid()),
            }
        };
        out.push(tok);
        let after = after.trim_start();
        if after.is_empty() {
            break;
        }
        rest = after.strip_prefix(',').ok_or_else(invalid)?.trim_start();
        if rest.is_empty() {
            return Err(invalid());
        }
    }
    Ok(out.join(", "))
}

/// The procedure the trigger calls: its own schema column where the
/// catalog has one, else the only schema with a procedure of that name
/// (the trigger's, when there are several).
fn procedure_of(src: &mut dyn JobSource, s: &Set, schema: &str) -> (String, String) {
    let name = s.get(0, "procedure_name").unwrap_or_default().to_string();
    if let Some(ps) = s.get(0, "procedure_schema") {
        return (ps.to_string(), name);
    }
    let p = src
        .query(crate::jobs::vertica::PROCEDURES, &[&name])
        .unwrap_or_default();
    let mut schemas: Vec<&str> = (0..p.rows.len())
        .filter_map(|r| p.get(r, "schema_name"))
        .collect();
    schemas.sort_unstable();
    schemas.dedup();
    let chosen = match schemas.as_slice() {
        [one] => one.to_string(),
        many if many.contains(&schema) => schema.to_string(),
        [first, ..] => first.to_string(),
        [] => schema.to_string(),
    };
    (chosen, name)
}

/// Trigger `id`, and whether its arguments read back as they were written.
fn read_trigger(src: &mut dyn JobSource, id: &str) -> Result<(Trigger, bool, Set)> {
    let (schema, name) = parse_pair(id)?;
    let s = src.query(ONE, &[&schema, &name])?;
    if s.rows.is_empty() {
        return Err(not_found(id));
    }
    let sch = best_effort(src, SCHEDULES);
    let sr = schedule_row(&sch, &schema, &name);
    let schedule = sr
        .and_then(|r| {
            Some((
                sch.get(r, "schema_name")?.to_string(),
                sch.get(r, "schedule_name")?.to_string(),
            ))
        })
        .unwrap_or_default();
    let raw = s.get(0, "procedure_args").unwrap_or_default();
    let safe = safe_args(Some(raw));
    let procedure = procedure_of(src, &s, &schema);
    Ok((
        Trigger {
            schema,
            name,
            schedule,
            procedure,
            args: safe.clone().unwrap_or_else(|| raw.to_string()),
            enabled: s.get(0, "enabled").is_none_or(|v| yes(Some(v))),
            owner: s.get(0, "owner").map(str::to_string),
        },
        safe.is_some(),
        s,
    ))
}

pub(super) fn trigger_values(t: &Trigger) -> BTreeMap<String, Value> {
    let pair = |p: &(String, String)| {
        if p.1.is_empty() {
            Value::Null
        } else {
            json!(quote_pair(&p.0, &p.1))
        }
    };
    values(vec![
        ("name", json!(t.name)),
        ("schema", json!(t.schema)),
        ("enabled", json!(t.enabled)),
        ("owner", jtext(t.owner.as_deref())),
        ("procedure", pair(&t.procedure)),
        ("args", json!(t.args)),
        ("schedule", pair(&t.schedule)),
    ])
}

pub(super) fn trigger_sections(existing: bool) -> Vec<FormSection> {
    let mut general = FormSection::new("general", "General").fields(vec![
        FormField::new("name", "Nombre", FormKind::Text).required(),
        FormField::new("schema", "Esquema", FormKind::reference("schema", false))
            .required()
            .default(json!("public")),
        FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
        FormField::new("owner", "Dueño", FormKind::reference("user", false))
            .help("Vacío al crearlo: quien lo crea. El procedimiento corre con los permisos de su dueño (AS DEFINER)."),
    ]);
    if existing {
        general = general.field(
            FormField::new("next_run", "Próxima ejecución (UTC)", FormKind::Text).read_only(true),
        );
    }
    vec![
        general,
        FormSection::new("action", "Procedimiento").fields(vec![
            FormField::new(
                "procedure",
                "Procedimiento",
                FormKind::reference("procedure", false),
            )
            .required(),
            FormField::new("args", "Argumentos", FormKind::Text)
                .help(ARGS_HELP)
                .placeholder("7, 'diario'"),
        ]),
        FormSection::new("schedule", "Programación").field(
            FormField::new(
                "schedule",
                "Programación",
                FormKind::reference("schedule", false),
            )
            .required()
            .help("Una de «Programaciones» que no dispare otro trigger: cada programación dispara uno solo."),
        ),
    ]
}

fn trigger_form(src: &mut dyn JobSource, id: Option<&str>, su: bool) -> Result<ObjectForm> {
    let choices = choices(src);
    let mut notes = Vec::new();
    if !su {
        notes.push(ONLY_SUPERUSER.to_string());
    }
    let Some(id) = id else {
        let t = Trigger {
            schema: "public".into(),
            name: String::new(),
            schedule: Default::default(),
            procedure: Default::default(),
            args: String::new(),
            enabled: true,
            owner: None,
        };
        return Ok(ObjectForm {
            sections: trigger_sections(false),
            values: trigger_values(&t),
            choices,
            notes,
            read_only: !su,
        });
    };
    let (t, args_ok, _) = read_trigger(src, id)?;
    let mut v = trigger_values(&t);
    let job = crate::jobs::vertica::next_run(
        &crate::jobs::vertica::time_table(src),
        Some(t.schedule.1.as_str()).filter(|s| !s.is_empty()),
        &t.name,
    );
    v.insert("next_run".into(), jtext(job.as_deref()));
    notes.insert(0, RECREATE.to_string());
    if !args_ok {
        notes.push(ARGS_UNREADABLE.to_string());
    }
    Ok(ObjectForm {
        sections: trigger_sections(true),
        values: v,
        choices,
        notes,
        read_only: !su,
    })
}

pub(super) fn trigger_from_values(
    values: &FormValues,
    choices: &BTreeMap<String, Vec<(String, String)>>,
) -> Result<Trigger> {
    let v = Vals(&values.values);
    Ok(Trigger {
        name: object_name(v, "name", "Nombre")?,
        schema: pick_name(
            v,
            "schema",
            "Esquema",
            choices.get("schema"),
            Some("public"),
        )?
        .unwrap_or_else(|| "public".into()),
        enabled: v.bool_or("enabled", true),
        owner: pick_name(v, "owner", "Dueño", choices.get("user"), None)?,
        procedure: pick_pair(v, "procedure", "Procedimiento", choices.get("procedure"))?,
        args: optional(v, "args", "Argumentos", 30_000)?.unwrap_or_default(),
        schedule: pick_pair(v, "schedule", "Programación", choices.get("schedule"))?,
    })
}

fn enable(name_schema: &str, name: &str, on: bool) -> String {
    format!(
        "SELECT ENABLE_TRIGGER({}, {on})",
        lit(&meta_name(name_schema, name))
    )
}

fn create_trigger(p: &mut Plan, t: &Trigger, args: &str) {
    let id = quote_pair(&t.schema, &t.name);
    p.exec(format!(
        "CREATE TRIGGER {id} ON SCHEDULE {} EXECUTE PROCEDURE {}({args}) AS DEFINER",
        quote_pair(&t.schedule.0, &t.schedule.1),
        quote_pair(&t.procedure.0, &t.procedure.1)
    ));
    if let Some(o) = &t.owner {
        p.exec(format!("ALTER TRIGGER {id} OWNER TO {}", q(o)));
    }
    if !t.enabled {
        p.query(enable(&t.schema, &t.name, false));
    }
}

pub(super) fn create_trigger_plan(t: &Trigger) -> Result<Plan> {
    let args = parse_args(&t.args)?;
    let mut p = Plan::new(&format!(
        "Trigger programado «{}.{}» de Vertica",
        t.schema, t.name
    ));
    create_trigger(&mut p, t, &args);
    p.id = Some(quote_pair(&t.schema, &t.name));
    Ok(p)
}

pub(super) fn alter_trigger_plan(cur: &Trigger, t: &Trigger) -> Result<Plan> {
    let args_changed = t.args.trim() != cur.args.trim();
    let recreate = t.procedure != cur.procedure || t.schedule != cur.schedule || args_changed;
    let new_id = quote_pair(&t.schema, &t.name);
    let mut p = Plan::new(&format!(
        "Trigger programado «{}.{}» de Vertica",
        cur.schema, cur.name
    ));
    p.id = Some(new_id.clone());
    if recreate {
        let args = parse_args(&t.args)?;
        for line in RECREATE.split("; ") {
            p.comment(line);
        }
        p.exec(format!(
            "DROP TRIGGER {}",
            quote_pair(&cur.schema, &cur.name)
        ));
        let mut t = t.clone();
        // Created by this login: its owner goes back to what it was.
        if t.owner.is_none() {
            t.owner.clone_from(&cur.owner);
        }
        create_trigger(&mut p, &t, &args);
        return Ok(p);
    }
    let mut id = quote_pair(&cur.schema, &cur.name);
    let mut changed = false;
    if let Some(o) = t.owner.as_ref().filter(|o| Some(*o) != cur.owner.as_ref()) {
        p.exec(format!("ALTER TRIGGER {id} OWNER TO {}", q(o)));
        changed = true;
    }
    if t.name != cur.name {
        p.exec(format!("ALTER TRIGGER {id} RENAME TO {}", q(&t.name)));
        id = quote_pair(&cur.schema, &t.name);
        changed = true;
    }
    if t.schema != cur.schema {
        p.exec(format!("ALTER TRIGGER {id} SET SCHEMA {}", q(&t.schema)));
        changed = true;
    }
    if t.enabled != cur.enabled {
        p.query(enable(&t.schema, &t.name, t.enabled));
        changed = true;
    }
    if !changed {
        return Ok(Plan::nothing(
            &format!("{}.{}", cur.schema, cur.name),
            quote_pair(&cur.schema, &cur.name),
        ));
    }
    Ok(p)
}

/// The schedule picked for a trigger has no other trigger.
fn schedule_free(src: &mut dyn JobSource, t: &Trigger, cur: Option<&Trigger>) -> Result<()> {
    if cur.is_some_and(|c| c.schedule == t.schedule) {
        return Ok(());
    }
    let sch = best_effort(src, SCHEDULES);
    let attached = (0..sch.rows.len())
        .find(|r| {
            sch.get(*r, "schema_name") == Some(t.schedule.0.as_str())
                && sch.get(*r, "schedule_name") == Some(t.schedule.1.as_str())
        })
        .and_then(|r| sch.get(r, "attached_trigger"));
    match attached {
        Some(other) => Err(bad(format!(
            "La programación «{}.{}» ya dispara el trigger «{}»: cada programación dispara uno solo.",
            short(&t.schedule.0),
            short(&t.schedule.1),
            short(other)
        ))),
        None => Ok(()),
    }
}

// ------------------------------------------------------------- dispatch

fn ready(src: &mut dyn JobSource) -> Result<()> {
    if superuser(src) {
        Ok(())
    } else {
        Err(Error::Unsupported(ONLY_SUPERUSER.into()))
    }
}

pub(super) fn form(src: &mut dyn JobSource, kind: Kind, id: Option<&str>) -> Result<ObjectForm> {
    let su = superuser(src);
    match kind {
        Kind::Trigger => trigger_form(src, id, su),
        Kind::Schedule => schedule_form(src, id, su),
    }
}

pub(super) fn plan(
    src: &mut dyn JobSource,
    kind: Kind,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    ready(src)?;
    let choices = choices(src);
    match kind {
        Kind::Trigger => {
            let t = trigger_from_values(values, &choices)?;
            match id {
                None => {
                    schedule_free(src, &t, None)?;
                    create_trigger_plan(&t)
                }
                Some(id) => {
                    let (cur, _, _) = read_trigger(src, id)?;
                    schedule_free(src, &t, Some(&cur))?;
                    alter_trigger_plan(&cur, &t)
                }
            }
        }
        Kind::Schedule => {
            let s = schedule_from_values(values, &choices)?;
            match id {
                None => Ok(create_schedule_plan(&s)),
                Some(id) => {
                    let (cur, _) = read_schedule(src, id)?;
                    Ok(alter_schedule_plan(&cur, &s))
                }
            }
        }
    }
}

pub(super) fn drop_trigger_plan(schema: &str, name: &str) -> Plan {
    let mut p = Plan::new(&format!(
        "Borra el trigger programado «{schema}.{name}» de Vertica (su programación queda)"
    ));
    p.exec(format!("DROP TRIGGER {}", quote_pair(schema, name)));
    p
}

pub(super) fn drop_schedule_plan(schema: &str, name: &str) -> Plan {
    let mut p = Plan::new(&format!(
        "Borra la programación «{schema}.{name}» de Vertica"
    ));
    p.exec(format!("DROP SCHEDULE {}", quote_pair(schema, name)));
    p
}

pub(super) fn delete_plan(src: &mut dyn JobSource, kind: Kind, id: &str) -> Result<Plan> {
    ready(src)?;
    match kind {
        Kind::Trigger => {
            let (t, _, _) = read_trigger(src, id)?;
            Ok(drop_trigger_plan(&t.schema, &t.name))
        }
        Kind::Schedule => {
            let (s, attached) = read_schedule(src, id)?;
            if let Some(t) = attached {
                return Err(bad(format!(
                    "La programación «{}.{}» dispara el trigger «{}»: borrá el trigger primero.",
                    short(&s.schema),
                    short(&s.name),
                    short(&t)
                )));
            }
            Ok(drop_schedule_plan(&s.schema, &s.name))
        }
    }
}
