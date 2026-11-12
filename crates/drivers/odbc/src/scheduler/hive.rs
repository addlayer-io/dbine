//! Hive 4 (and Cloudera's Hive): scheduled queries managed whole with
//! `CREATE SCHEDULED QUERY … (CRON | EVERY) [EXECUTED AS] (ENABLED |
//! DISABLED) DEFINED AS …`, `ALTER SCHEDULED QUERY` (one change per
//! statement: schedule, executed as, enabled, the query) and `DROP
//! SCHEDULED QUERY`. Hive has no transactions: an edit's statements run
//! one after the other. A scheduled query can't be renamed: it is dropped
//! and created again, and its executions (kept by id) and next run go with
//! it.
//!
//! Hive keeps the schedule as a Quartz cron expression (`EVERY` is turned
//! into one), so an existing query reads back as cron. The statements act
//! on the session's namespace (`hive.scheduled.queries.namespace`): a
//! query of another one, or with a name that isn't a plain identifier, is
//! shown read-only. The executions are cleaned by the metastore on its own
//! (`hive.metastore.scheduled.queries.execution.max.age`): there's no
//! statement to clear them.

use super::*;
use crate::jobs::hive::{col, hive_lit, ident, namespace, not_there, queries_sql, NO_SCHEDULES};
use crate::jobs::{int, int_id, not_found, yes};
use dbine_driver::{FormField, FormKind, FormSection, SchedulerKind};
use serde_json::json;

pub(super) const NO_CLEAR: &str = "Hive no tiene una sentencia que borre las ejecuciones de una consulta programada: el metastore las borra solo al vencer (hive.metastore.scheduled.queries.execution.max.age)";
const RENAME: &str = "Hive no cambia el nombre de una consulta programada: se borra y se crea de nuevo; sus ejecuciones se pierden y recibe otro id.";
const EXECUTED_AS: &str =
    "Solo un administrador de Hive puede hacer que una consulta corra como otro usuario.";
const UNITS: &[&str] = &["SECOND", "MINUTE", "HOUR", "DAY"];

pub(super) fn kind(kind: &str) -> Result<()> {
    (kind.trim() == "job")
        .then_some(())
        .ok_or_else(|| bad_kind(kind))
}

/// A scheduled query as the form edits it.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Query {
    pub(super) name: String,
    /// "cron" or "every".
    pub(super) mode: String,
    pub(super) cron: Option<String>,
    pub(super) every: Option<i64>,
    pub(super) unit: String,
    /// "none", "at" or "offset".
    pub(super) offset_mode: String,
    pub(super) offset: Option<String>,
    pub(super) executed_as: Option<String>,
    pub(super) enabled: bool,
    pub(super) query: String,
}

pub(super) fn info(src: &mut dyn JobSource) -> Result<SchedulerInfo> {
    if let Err(e) = src.query(&queries_sql(Some(0)), &[]) {
        if not_there(&e) {
            return Ok(SchedulerInfo {
                kinds: Vec::new(),
                logs: false,
                notes: vec![NO_SCHEDULES.to_string()],
            });
        }
        return Err(e);
    }
    let ns = namespace(src);
    Ok(SchedulerInfo {
        kinds: vec![SchedulerKind::new(
            "job",
            "Consultas programadas",
            "consulta programada",
        )
        .abilities(true, true, true)],
        logs: false,
        notes: vec![
            format!("La sesión trabaja en el namespace «{ns}»: las consultas programadas de otros namespaces se ven solo para lectura."),
            EXECUTED_AS.to_string(),
        ],
    })
}

/// Query `id`: the row, whether this session may change it (its
/// namespace, a plain name) and why not.
fn read(src: &mut dyn JobSource, id: &str) -> Result<(u64, Query, Set, Option<String>)> {
    let n = int_id(id)?;
    let s = src.query(&queries_sql(Some(n)), &[])?;
    if s.rows.is_empty() {
        return Err(not_found(id));
    }
    let name = col(&s, 0, "name").unwrap_or_default().to_string();
    let ns = namespace(src);
    let why = if ident(&name).is_none() {
        Some(
            "DBine solo cambia consultas programadas con nombres simples (letras, números y _)."
                .to_string(),
        )
    } else {
        col(&s, 0, "namespace").filter(|q| *q != ns).map(|q| {
            format!("Esta consulta programada es del namespace «{q}» y la sesión trabaja en «{ns}»: se ve solo para lectura.")
        })
    };
    let q = Query {
        name,
        mode: "cron".into(),
        cron: col(&s, 0, "schedule").map(str::to_string),
        every: None,
        unit: "HOUR".into(),
        offset_mode: "none".into(),
        offset: None,
        executed_as: col(&s, 0, "owner").map(str::to_string),
        enabled: yes(col(&s, 0, "enabled")),
        query: col(&s, 0, "query").unwrap_or_default().to_string(),
    };
    Ok((n, q, s, why))
}

pub(super) fn values_of(q: &Query) -> BTreeMap<String, Value> {
    values(vec![
        ("name", json!(q.name)),
        ("enabled", json!(q.enabled)),
        ("executed_as", jtext(q.executed_as.as_deref())),
        ("mode", json!(q.mode)),
        ("cron", jtext(q.cron.as_deref())),
        ("every", q.every.map_or(Value::Null, |n| json!(n))),
        ("unit", json!(q.unit)),
        ("offset_mode", json!(q.offset_mode)),
        ("offset", jtext(q.offset.as_deref())),
        ("query", json!(q.query)),
    ])
}

pub(super) fn sections(existing: bool) -> Vec<FormSection> {
    let mut out = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("name", "Nombre", FormKind::Text)
                .required()
                .help("Letras, números y _. Cambiarlo vuelve a crear la consulta programada."),
            FormField::new("enabled", "Habilitada", FormKind::Bool).default(json!(true)),
            FormField::new("executed_as", "Ejecuta como", FormKind::Text)
                .help("El usuario con el que corre. Vacío: quien la crea. Otro usuario lo pone solo un administrador."),
        ]),
        FormSection::new("schedule", "Programación").fields(vec![
            FormField::new(
                "mode",
                "Programación",
                FormKind::select(&[
                    ("cron", "Expresión cron (Quartz)"),
                    ("every", "Cada cierto tiempo"),
                ]),
            )
            .default(json!("cron")),
            FormField::new("cron", "Expresión cron", FormKind::Cron)
                .when("mode", &["cron"])
                .help("Formato de Quartz: segundo, minuto, hora, día del mes, mes, día de la semana y año (opcional).")
                .placeholder("0 0 3 * * ? *"),
            FormField::new("every", "Cada", FormKind::number(Some(1.0), None))
                .when("mode", &["every"])
                .default(json!(1)),
            FormField::new(
                "unit",
                "Unidad",
                FormKind::select(&[
                    ("SECOND", "Segundos"),
                    ("MINUTE", "Minutos"),
                    ("HOUR", "Horas"),
                    ("DAY", "Días"),
                ]),
            )
            .when("mode", &["every"])
            .default(json!("HOUR")),
            FormField::new(
                "offset_mode",
                "Desfase",
                FormKind::select(&[
                    ("none", "Ninguno"),
                    ("at", "A una hora (AT)"),
                    ("offset", "Corrido (OFFSET BY)"),
                ]),
            )
            .when("mode", &["every"])
            .default(json!("none")),
            FormField::new("offset", "Hora o desfase", FormKind::Text)
                .when("offset_mode", &["at", "offset"])
                .placeholder("03:30:00"),
        ]),
        FormSection::new("query", "Consulta").field(
            FormField::new("query", "Consulta", FormKind::code("sql"))
                .required()
                .help("Una sola sentencia HiveQL, sin el punto y coma final."),
        ),
    ];
    if existing {
        out.push(FormSection::new("state", "Estado").fields(vec![
            FormField::new("id", "Id", FormKind::Text).read_only(true),
            FormField::new("namespace", "Namespace", FormKind::Text).read_only(true),
            FormField::new("next_execution", "Próxima ejecución", FormKind::Text).read_only(true),
        ]));
    }
    out
}

pub(super) fn form(src: &mut dyn JobSource, id: Option<&str>) -> Result<ObjectForm> {
    let Some(id) = id else {
        let q = Query {
            mode: "cron".into(),
            unit: "HOUR".into(),
            offset_mode: "none".into(),
            enabled: true,
            ..Query::default()
        };
        return Ok(ObjectForm {
            sections: sections(false),
            values: values_of(&q),
            notes: vec![EXECUTED_AS.to_string()],
            ..ObjectForm::default()
        });
    };
    let (n, q, s, why) = read(src, id)?;
    let mut v = values_of(&q);
    v.insert("id".into(), json!(n.to_string()));
    v.insert("namespace".into(), jtext(col(&s, 0, "namespace")));
    v.insert("next_execution".into(), jtext(col(&s, 0, "next_execution")));
    let mut notes = vec![RENAME.to_string()];
    let read_only = why.is_some();
    if let Some(w) = why {
        notes.insert(0, w);
    }
    Ok(ObjectForm {
        sections: sections(true),
        values: v,
        notes,
        read_only,
        ..ObjectForm::default()
    })
}

/// The query the form describes, checked.
pub(super) fn from_values(values: &FormValues) -> Result<Query> {
    let v = Vals(&values.values);
    let name = required(v, "name", "Nombre", 128)?;
    if ident(&name).is_none() {
        return Err(bad(format!(
            "«Nombre»: «{}» tiene que ser de letras, números y _.",
            short(&name)
        )));
    }
    let mode = choice(v, "mode", "Programación", &["cron", "every"], "cron")?;
    let mut q = Query {
        name,
        mode: mode.clone(),
        unit: "HOUR".into(),
        offset_mode: "none".into(),
        enabled: v.bool_or("enabled", true),
        ..Query::default()
    };
    if mode == "cron" {
        q.cron = Some(
            cron(v, "cron", "Expresión cron", 6..=7)?
                .ok_or_else(|| bad("Falta «Expresión cron»."))?,
        );
    } else {
        q.every = Some(
            v.int("every", "Cada", 1, i64::from(i32::MAX))?
                .ok_or_else(|| bad("Falta «Cada»."))?,
        );
        q.unit = choice(v, "unit", "Unidad", UNITS, "HOUR")?;
        q.offset_mode = choice(
            v,
            "offset_mode",
            "Desfase",
            &["none", "at", "offset"],
            "none",
        )?;
        if q.offset_mode != "none" {
            let o = required(v, "offset", "Hora o desfase", 30)?;
            let ok = o
                .bytes()
                .all(|b| b.is_ascii_digit() || matches!(b, b':' | b'-' | b' ' | b'.' | b'T'));
            if !ok {
                return Err(bad(format!(
                    "«Hora o desfase»: «{}» no es una hora (HH:MM:SS) ni una fecha y hora.",
                    short(&o)
                )));
            }
            q.offset = Some(o);
        }
    }
    q.executed_as = optional(v, "executed_as", "Ejecuta como", 128)?;
    let text =
        code_text(v, "query", "Consulta", 1_000_000)?.ok_or_else(|| bad("Falta «Consulta»."))?;
    q.query = text
        .trim_end_matches(|c: char| c == ';' || c.is_whitespace())
        .to_string();
    if q.query.is_empty() {
        return Err(bad("Falta «Consulta»."));
    }
    Ok(q)
}

/// `CRON '…'` or `EVERY n UNITS [AT | OFFSET BY '…']`.
pub(super) fn schedule_clause(q: &Query) -> String {
    if q.mode == "every" {
        let n = q.every.unwrap_or(1);
        let unit = if n == 1 {
            q.unit.clone()
        } else {
            format!("{}S", q.unit)
        };
        let mut out = format!("EVERY {n} {unit}");
        match (q.offset_mode.as_str(), &q.offset) {
            ("at", Some(o)) => out.push_str(&format!(" AT {}", hive_lit(o))),
            ("offset", Some(o)) => out.push_str(&format!(" OFFSET BY {}", hive_lit(o))),
            _ => {}
        }
        out
    } else {
        format!("CRON {}", hive_lit(q.cron.as_deref().unwrap_or_default()))
    }
}

fn enabled_clause(q: &Query) -> &'static str {
    if q.enabled {
        "ENABLED"
    } else {
        "DISABLED"
    }
}

pub(super) fn create_statement(q: &Query) -> String {
    let mut out = format!(
        "CREATE SCHEDULED QUERY `{}`\n{}",
        q.name,
        schedule_clause(q)
    );
    if let Some(u) = &q.executed_as {
        out.push_str(&format!("\nEXECUTED AS {}", hive_lit(u)));
    }
    out.push_str(&format!("\n{}\nDEFINED AS {}", enabled_clause(q), q.query));
    out
}

/// The new query's id, by name in the session's namespace.
fn lookup(name: String) -> Lookup {
    Box::new(move |src: &mut dyn JobSource| {
        let ns = namespace(src);
        let s = src.query(
            &format!(
                "SELECT q.scheduled_query_id AS id, q.cluster_namespace AS namespace\n  FROM information_schema.scheduled_queries q WHERE q.schedule_name = '{name}'"
            ),
            &[],
        )?;
        Ok((0..s.rows.len())
            .filter(|r| col(&s, *r, "namespace").is_none_or(|n| n == ns))
            .filter_map(|r| int(col(&s, r, "id")))
            .max()
            .map(|n| n.to_string()))
    })
}

pub(super) fn create_plan(q: &Query) -> Plan {
    let mut p = Plan::new(&format!("Consulta programada «{}» de Hive", q.name));
    p.exec(create_statement(q));
    p.lookup = Some(lookup(q.name.clone()));
    p
}

pub(super) fn alter_plan(id: u64, cur: &Query, q: &Query) -> Plan {
    if q.name != cur.name {
        let mut p = Plan::new(&format!("Consulta programada «{}» de Hive", cur.name));
        for line in RENAME.split("; ") {
            p.comment(line);
        }
        p.exec(format!("DROP SCHEDULED QUERY `{}`", cur.name));
        p.exec(create_statement(q));
        p.lookup = Some(lookup(q.name.clone()));
        return p;
    }
    let alter = |what: String| format!("ALTER SCHEDULED QUERY `{}` {what}", cur.name);
    let mut steps = Vec::new();
    let schedule_changed = q.mode != "cron"
        || !same(
            &q.cron
                .as_deref()
                .map(|c| c.split_whitespace().collect::<Vec<_>>().join(" ")),
            &cur.cron
                .as_deref()
                .map(|c| c.split_whitespace().collect::<Vec<_>>().join(" ")),
        );
    if schedule_changed {
        steps.push(alter(schedule_clause(q)));
    }
    if let Some(u) = &q.executed_as {
        if Some(u) != cur.executed_as.as_ref() {
            steps.push(alter(format!("EXECUTED AS {}", hive_lit(u))));
        }
    }
    if q.query.trim() != cur.query.trim() {
        steps.push(alter(format!("DEFINED AS {}", q.query)));
    }
    if q.enabled != cur.enabled {
        steps.push(alter(enabled_clause(q).to_string()));
    }
    if steps.is_empty() {
        return Plan::nothing(&cur.name, id.to_string());
    }
    let mut p = Plan::new(&format!("Consulta programada «{}» de Hive", cur.name));
    for s in steps {
        p.exec(s);
    }
    p.id = Some(id.to_string());
    p
}

pub(super) fn plan(src: &mut dyn JobSource, id: Option<&str>, values: &FormValues) -> Result<Plan> {
    let q = from_values(values)?;
    match id {
        None => Ok(create_plan(&q)),
        Some(id) => {
            let (n, cur, _, why) = read(src, id)?;
            if let Some(w) = why {
                return Err(Error::Unsupported(w));
            }
            Ok(alter_plan(n, &cur, &q))
        }
    }
}

pub(super) fn drop_plan(name: &str) -> Plan {
    let mut p = Plan::new(&format!(
        "Borra la consulta programada «{name}» de Hive y sus ejecuciones"
    ));
    p.exec(format!("DROP SCHEDULED QUERY `{name}`"));
    p
}

pub(super) fn delete_plan(src: &mut dyn JobSource, id: &str) -> Result<Plan> {
    let (_, q, _, why) = read(src, id)?;
    if let Some(w) = why {
        return Err(Error::Unsupported(w));
    }
    Ok(drop_plan(&q.name))
}
