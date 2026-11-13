//! Watcher watches: Kibana's advanced watch editor as a form, the schedule
//! as Watcher's own trigger kinds (interval, cron, hourly, daily, weekly,
//! monthly, yearly) or its JSON, and the input, condition, transform and
//! actions as JSON. `PUT _watcher/watch/<id>` creates one and, with
//! `if_seq_no` / `if_primary_term`, changes it in place (the values the
//! cluster redacts, `::es_redacted::`, are kept by such an update);
//! `_activate` / `_deactivate` follow "Activo". A new id is a new watch
//! (the old one is deleted after it): its history stays under the old id.

use super::{
    bad, has_redacted, json_text, ms_time, obj, plain_id, s, same, shown, split_list, Plan, Req,
    Shape, Vals,
};
use crate::ddl::path_segment;
use crate::jobs::{num, text};
use crate::json::J;
use crate::EsSession;
use dbine_driver::{FormField, FormKind, FormSection, FormValues, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(crate) const NOTE: &str = "Un watch se guarda entero (PUT), y sus acciones vuelven a esperar su primera ejecución para los reconocimientos (ack). Si tiene valores que el clúster oculta (::es_redacted::, como una contraseña) y no se escriben de nuevo, se actualiza conservándolos, pero esa actualización no puede quitar partes del watch. Un id nuevo crea otro watch y borra el anterior, cuyo historial queda con el id viejo.";

const DAYS: [&str; 7] = [
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
];
const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// A watch as the cluster has it.
#[derive(Debug, Clone)]
pub(crate) struct Cur {
    pub(crate) id: String,
    pub(crate) watch: J,
    pub(crate) active: bool,
    /// Some action waits for an acknowledgement.
    pub(crate) ackable: bool,
    pub(crate) seq: String,
    pub(crate) term: String,
}

pub(crate) async fn read(s: &EsSession, id: &str) -> Result<Cur> {
    let hit = s.es_watch(id).await?;
    Ok(Cur {
        id: id.to_string(),
        watch: hit.get("watch").cloned().unwrap_or(J::Null),
        active: hit
            .at(&["status", "state", "active"])
            .and_then(J::as_bool)
            .unwrap_or(true),
        ackable: crate::jobs::watch_ackable(&hit),
        seq: text(hit.get("_seq_no")).unwrap_or_default(),
        term: text(hit.get("_primary_term")).unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

fn days_kind() -> FormKind {
    FormKind::multi(&[
        ("monday", "Lunes"),
        ("tuesday", "Martes"),
        ("wednesday", "Miércoles"),
        ("thursday", "Jueves"),
        ("friday", "Viernes"),
        ("saturday", "Sábado"),
        ("sunday", "Domingo"),
    ])
}

fn months_kind() -> FormKind {
    FormKind::multi(&[
        ("january", "Enero"),
        ("february", "Febrero"),
        ("march", "Marzo"),
        ("april", "Abril"),
        ("may", "Mayo"),
        ("june", "Junio"),
        ("july", "Julio"),
        ("august", "Agosto"),
        ("september", "Septiembre"),
        ("october", "Octubre"),
        ("november", "Noviembre"),
        ("december", "Diciembre"),
    ])
}

pub(crate) fn sections() -> Vec<FormSection> {
    vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("id", "Id", FormKind::Text)
                .required()
                .placeholder("errores-por-hora")
                .help("Cambiarlo crea un watch nuevo y borra el anterior."),
            FormField::new("description", "Nombre", FormKind::Text)
                .help("Se guarda en metadata.name: es el nombre que muestra Kibana."),
            FormField::new("enabled", "Activo", FormKind::Bool)
                .default(json!(true))
                .help("Un watch inactivo no se dispara según su programación."),
            FormField::new("metadata", "Metadatos", FormKind::code("json"))
                .placeholder("{ \"umbral\": 100 }")
                .help("Datos que el watch lee como ctx.metadata (además del nombre)."),
        ]),
        FormSection::new("trigger", "Disparador").fields(vec![
            FormField::new(
                "trigger_kind",
                "Programación",
                FormKind::select(&[
                    ("interval", "Cada cierto tiempo"),
                    ("cron", "Expresión cron"),
                    ("hourly", "Cada hora"),
                    ("daily", "Diaria"),
                    ("weekly", "Semanal"),
                    ("monthly", "Mensual"),
                    ("yearly", "Anual"),
                    ("json", "JSON del disparador"),
                ]),
            )
            .required()
            .default(json!("interval")),
            FormField::new("interval", "Intervalo", FormKind::Text)
                .required()
                .when("trigger_kind", &["interval"])
                .default(json!("10m"))
                .placeholder("10m")
                .help("Un número y su unidad: s, m, h, d o w."),
            FormField::new("cron", "Expresiones cron", FormKind::Textarea)
                .required()
                .when("trigger_kind", &["cron"])
                .placeholder("0 0/15 * * * ?")
                .help("Una por línea, con segundos (sintaxis de Elasticsearch: segundos minutos horas día-del-mes mes día-de-la-semana [año])."),
            FormField::new("minutes", "Minutos", FormKind::Text)
                .required()
                .when("trigger_kind", &["hourly"])
                .placeholder("0, 30")
                .help("Los minutos de cada hora en que se dispara (0 a 59), separados por comas."),
            FormField::new("week_days", "Días de la semana", days_kind())
                .required()
                .when("trigger_kind", &["weekly"]),
            FormField::new("months", "Meses", months_kind())
                .required()
                .when("trigger_kind", &["yearly"]),
            FormField::new("month_days", "Días del mes", FormKind::Text)
                .required()
                .when("trigger_kind", &["monthly", "yearly"])
                .placeholder("1, 15"),
            FormField::new("times", "Horas", FormKind::Text)
                .required()
                .when("trigger_kind", &["daily", "weekly", "monthly", "yearly"])
                .placeholder("09:00, 17:30")
                .help("Horas del día (HH:MM, UTC), separadas por comas."),
            FormField::new("trigger_json", "Programación (JSON)", FormKind::code("json"))
                .required()
                .when("trigger_kind", &["json"])
                .placeholder("{ \"weekly\": [{ \"on\": [\"monday\"], \"at\": [\"09:00\"] }] }")
                .help("El objeto schedule de Watcher, para lo que los campos no expresan (varias combinaciones de días y horas)."),
        ]),
        FormSection::new("input", "Entrada y condición").fields(vec![
            FormField::new("input", "Entrada", FormKind::code("json"))
                .default(json!("{\n  \"none\": {}\n}"))
                .help("search, http, simple, chain o none: lo que el watch lee en cada ejecución."),
            FormField::new("condition", "Condición", FormKind::code("json"))
                .default(json!("{\n  \"always\": {}\n}"))
                .help("always, never, compare, array_compare o script: cuándo se ejecutan las acciones."),
            FormField::new("transform", "Transformación", FormKind::code("json"))
                .help("Cambia el payload antes de las acciones (search, script o chain); vacío: ninguna."),
        ]),
        FormSection::new("actions", "Acciones").fields(vec![
            FormField::new("actions", "Acciones", FormKind::code("json"))
                .default(json!("{}"))
                .placeholder("{ \"registro\": { \"logging\": { \"text\": \"{{ctx.payload.hits.total}} errores\" } } }")
                .help("Un objeto con una entrada por acción (email, webhook, index, logging, slack, pagerduty, jira…); se ejecutan en este orden."),
        ]),
        FormSection::new("options", "Opciones").fields(vec![FormField::new(
            "throttle_period",
            "Espera entre acciones",
            FormKind::Text,
        )
        .placeholder("5m")
        .help("Tiempo mínimo entre dos ejecuciones de una misma acción; vacío: el del clúster (5s).")]),
    ]
}

/// A day or month name as the form's value (`MON` → `monday`).
fn normalize(v: &J, names: &[&str]) -> Option<String> {
    let t = v.text().trim().to_ascii_lowercase();
    if let Ok(n) = t.parse::<usize>() {
        // Watcher numbers months 1-12 and days 1 (Sunday) - 7.
        return if names.len() == 12 {
            names.get(n.checked_sub(1)?).map(|x| x.to_string())
        } else {
            let i = (n + 5) % 7;
            names.get(i).map(|x| x.to_string())
        };
    }
    names
        .iter()
        .find(|n| t.len() >= 3 && n.starts_with(&t[..3]))
        .map(|x| x.to_string())
}

/// A JSON list (or one value) as a list.
fn many(v: Option<&J>) -> Vec<J> {
    match v {
        Some(J::Arr(a)) => a.clone(),
        Some(J::Null) | None => Vec::new(),
        Some(v) => vec![v.clone()],
    }
}

/// `HH:MM` times as written; `None` when any isn't one ("noon", an object).
fn times_of(v: Option<&J>) -> Option<String> {
    let all = many(v);
    let out: Vec<String> = all
        .iter()
        .filter_map(|t| t.as_str().filter(|t| parse_time(t).is_some()))
        .map(str::to_string)
        .collect();
    (!out.is_empty() && out.len() == all.len()).then(|| out.join(", "))
}

/// The schedule as the trigger fields; anything the fields can't say
/// stays JSON.
fn trigger_values(schedule: Option<&J>, v: &mut BTreeMap<String, Value>) {
    let as_json = |v: &mut BTreeMap<String, Value>| {
        v.insert("trigger_kind".into(), json!("json"));
        v.insert("trigger_json".into(), json_text(schedule));
    };
    let Some((kind, body)) = schedule
        .and_then(J::as_obj)
        .filter(|o| o.len() == 1)
        .and_then(|o| o.first())
    else {
        return as_json(v);
    };
    let one = |b: &J| -> Option<J> {
        match b {
            J::Arr(a) if a.len() == 1 => Some(a[0].clone()),
            J::Obj(_) => Some(b.clone()),
            _ => None,
        }
    };
    match kind.as_str() {
        "interval" => {
            let t = body.text();
            let t = if t.bytes().all(|b| b.is_ascii_digit()) {
                format!("{t}s")
            } else {
                t
            };
            v.insert("trigger_kind".into(), json!("interval"));
            v.insert("interval".into(), json!(t));
        }
        "cron" => {
            let lines: Vec<String> = many(Some(body)).iter().map(J::text).collect();
            v.insert("trigger_kind".into(), json!("cron"));
            v.insert("cron".into(), json!(lines.join("\n")));
        }
        "hourly" => {
            let m: Vec<String> = many(body.get("minute")).iter().map(J::text).collect();
            if m.is_empty() {
                return as_json(v);
            }
            v.insert("trigger_kind".into(), json!("hourly"));
            v.insert("minutes".into(), json!(m.join(", ")));
        }
        "daily" | "weekly" | "monthly" | "yearly" => {
            let Some(e) = one(body) else {
                return as_json(v);
            };
            let Some(times) = times_of(e.get("at")) else {
                return as_json(v);
            };
            v.insert("times".into(), json!(times));
            if kind == "weekly" {
                let d: Option<Vec<String>> = many(e.get("on"))
                    .iter()
                    .map(|x| normalize(x, &DAYS))
                    .collect();
                match d {
                    Some(d) if !d.is_empty() => v.insert("week_days".into(), json!(d)),
                    _ => return as_json(v),
                };
            }
            if kind == "monthly" || kind == "yearly" {
                let d: Vec<String> = many(e.get("on")).iter().map(J::text).collect();
                if d.is_empty() || d.iter().any(|x| x.parse::<u8>().is_err()) {
                    return as_json(v);
                }
                v.insert("month_days".into(), json!(d.join(", ")));
            }
            if kind == "yearly" {
                let m: Option<Vec<String>> = many(e.get("in"))
                    .iter()
                    .map(|x| normalize(x, &MONTHS))
                    .collect();
                match m {
                    Some(m) if !m.is_empty() => v.insert("months".into(), json!(m)),
                    _ => return as_json(v),
                };
            }
            v.insert("trigger_kind".into(), json!(kind));
        }
        _ => as_json(v),
    }
}

pub(crate) fn values_of(cur: &Cur) -> BTreeMap<String, Value> {
    let w = &cur.watch;
    let mut v = BTreeMap::new();
    v.insert("id".into(), json!(cur.id));
    v.insert(
        "description".into(),
        json!(text(w.at(&["metadata", "name"])).unwrap_or_default()),
    );
    v.insert("enabled".into(), json!(cur.active));
    let rest = w
        .get("metadata")
        .map(|m| crate::jobs::without(m, &["name"]));
    v.insert(
        "metadata".into(),
        json_text(
            rest.as_ref()
                .filter(|m| !matches!(m, J::Obj(o) if o.is_empty())),
        ),
    );
    trigger_values(w.at(&["trigger", "schedule"]), &mut v);
    v.insert("input".into(), json_text(w.get("input")));
    v.insert("condition".into(), json_text(w.get("condition")));
    v.insert("transform".into(), json_text(w.get("transform")));
    v.insert(
        "actions".into(),
        json_text(Some(w.get("actions").unwrap_or(&J::Obj(Vec::new())))),
    );
    let throttle = text(w.get("throttle_period"))
        .or_else(|| num(w.get("throttle_period_in_millis")).map(|ms| ms_time(ms as i64)));
    v.insert(
        "throttle_period".into(),
        json!(throttle.unwrap_or_default()),
    );
    v
}

// ---------------------------------------------------------------------------
// From the form
// ---------------------------------------------------------------------------

/// A watch as the form describes it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Watch {
    pub(crate) id: String,
    pub(crate) active: bool,
    pub(crate) body: J,
}

/// `HH:MM` (or `H:MM`).
fn parse_time(t: &str) -> Option<(u8, u8)> {
    let (h, m) = t.trim().split_once(':')?;
    let ok = |x: &str| !x.is_empty() && x.len() <= 2 && x.bytes().all(|b| b.is_ascii_digit());
    if !ok(h) || !ok(m) || m.len() != 2 {
        return None;
    }
    let (h, m) = (h.parse::<u8>().ok()?, m.parse::<u8>().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

fn numbers(v: Vals, k: &str, label: &str, min: u8, max: u8) -> Result<Vec<J>> {
    let items = split_list(&v.raw(k));
    if items.is_empty() {
        return Err(bad(format!("Falta «{label}».")));
    }
    items
        .iter()
        .map(|x| match x.parse::<u8>() {
            Ok(n) if (min..=max).contains(&n) => Ok(J::Num(n.into())),
            _ => Err(bad(format!(
                "«{label}»: «{}» no es un número entre {min} y {max}.",
                shown(x)
            ))),
        })
        .collect()
}

fn times(v: Vals) -> Result<J> {
    let items = split_list(&v.raw("times"));
    if items.is_empty() {
        return Err(bad("Faltan las horas del disparador.".into()));
    }
    let mut out = Vec::new();
    for t in items {
        let (h, m) = parse_time(&t).ok_or_else(|| {
            bad(format!(
                "«Horas»: «{}» no es una hora válida (HH:MM).",
                shown(&t)
            ))
        })?;
        out.push(s(format!("{h:02}:{m:02}")));
    }
    Ok(J::Arr(out))
}

fn picked(v: Vals, k: &str, label: &str, names: &[&str]) -> Result<J> {
    let items = v.strings(k);
    if items.is_empty() {
        return Err(bad(format!("Falta «{label}».")));
    }
    let mut out = Vec::new();
    for x in items {
        if !names.contains(&x.as_str()) {
            return Err(bad(format!(
                "«{label}»: «{}» no es un valor válido.",
                shown(&x)
            )));
        }
        out.push(s(x));
    }
    Ok(J::Arr(out))
}

/// Watcher's interval: a number and s, m, h, d or w.
fn is_interval(t: &str) -> bool {
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    let (n, unit) = t.split_at(digits);
    !n.is_empty() && n.len() <= 9 && matches!(unit, "s" | "m" | "h" | "d" | "w")
}

pub(crate) fn schedule(v: Vals) -> Result<J> {
    let kind = v.text("trigger_kind");
    let entry = |k: &str, body: J| J::Obj(vec![(k.to_string(), body)]);
    Ok(match kind.as_str() {
        "interval" | "" => {
            let t = v
                .opt("interval")
                .ok_or_else(|| bad("Falta el intervalo del disparador.".into()))?;
            if !is_interval(&t) {
                return Err(bad(format!(
                    "«Intervalo»: «{}» no es válido (un número y s, m, h, d o w).",
                    shown(&t)
                )));
            }
            entry("interval", s(t))
        }
        "cron" => {
            let lines: Vec<String> = v
                .raw("cron")
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(|l| super::check_cron(l, "Expresiones cron", 6..=7))
                .collect::<Result<_>>()?;
            match lines.len() {
                0 => return Err(bad("Falta la expresión cron del disparador.".into())),
                1 => entry("cron", s(lines[0].clone())),
                _ => entry("cron", J::Arr(lines.into_iter().map(s).collect())),
            }
        }
        "hourly" => entry(
            "hourly",
            obj(vec![(
                "minute",
                Some(J::Arr(numbers(v, "minutes", "Minutos", 0, 59)?)),
            )]),
        ),
        "daily" => entry("daily", obj(vec![("at", Some(times(v)?))])),
        "weekly" => entry(
            "weekly",
            obj(vec![
                (
                    "on",
                    Some(picked(v, "week_days", "Días de la semana", &DAYS)?),
                ),
                ("at", Some(times(v)?)),
            ]),
        ),
        "monthly" => entry(
            "monthly",
            obj(vec![
                (
                    "on",
                    Some(J::Arr(numbers(v, "month_days", "Días del mes", 1, 31)?)),
                ),
                ("at", Some(times(v)?)),
            ]),
        ),
        "yearly" => entry(
            "yearly",
            obj(vec![
                ("in", Some(picked(v, "months", "Meses", &MONTHS)?)),
                (
                    "on",
                    Some(J::Arr(numbers(v, "month_days", "Días del mes", 1, 31)?)),
                ),
                ("at", Some(times(v)?)),
            ]),
        ),
        "json" => {
            let j = v
                .json("trigger_json", "Programación (JSON)", Shape::Object)?
                .ok_or_else(|| bad("Falta el JSON de la programación.".into()))?;
            if j.as_obj().map(Vec::len) != Some(1) {
                return Err(bad(
                    "«Programación (JSON)» tiene que ser un objeto con un solo tipo de programación ({ \"weekly\": … })."
                        .into(),
                ));
            }
            j
        }
        other => {
            return Err(bad(format!(
                "«{}» no es un tipo de programación de Watcher.",
                shown(other)
            )))
        }
    })
}

pub(crate) fn spec(values: &BTreeMap<String, Value>) -> Result<Watch> {
    let v = Vals(values);
    let id = plain_id(v.opt("id"), "Id")?;
    let mut metadata = match v.json("metadata", "Metadatos", Shape::Object)? {
        Some(J::Obj(o)) => o,
        _ => Vec::new(),
    };
    metadata.retain(|(k, _)| k != "name");
    if let Some(name) = v.line("description", "Nombre")? {
        metadata.insert(0, ("name".into(), s(name)));
    }
    let throttle = v.time("throttle_period", "Espera entre acciones")?;
    let body = obj(vec![
        ("trigger", Some(obj(vec![("schedule", Some(schedule(v)?))]))),
        ("input", v.json("input", "Entrada", Shape::Object)?),
        (
            "condition",
            v.json("condition", "Condición", Shape::Object)?,
        ),
        (
            "transform",
            v.json("transform", "Transformación", Shape::Object)?,
        ),
        (
            "actions",
            Some(
                v.json("actions", "Acciones", Shape::Object)?
                    .unwrap_or(J::Obj(Vec::new())),
            ),
        ),
        ("throttle_period", throttle.map(s)),
        (
            "metadata",
            (!metadata.is_empty()).then_some(J::Obj(metadata)),
        ),
    ]);
    Ok(Watch {
        id,
        active: v.bool_or("enabled", true),
        body,
    })
}

fn put(id: &str, active: bool, extra: &str, body: &J) -> Req {
    Req::new(
        "PUT",
        format!(
            "/_watcher/watch/{}?{extra}active={active}",
            path_segment(id)
        ),
        Some(body.clone()),
    )
}

const REDACTED_NEW: &str = "El watch tiene valores que el clúster ocultó (::es_redacted::): un watch nuevo (o con otro id) necesita los valores reales escritos en su lugar.";
const REDACTED_REMOVAL: &str = "El watch tiene valores que el clúster ocultó (::es_redacted::) y el cambio quita partes de su definición: Elasticsearch conserva esos valores solo con una actualización que no puede quitar nada. Escriba los valores reales en lugar de ::es_redacted:: para reemplazar el watch entero.";
const KEEPS_SECRETS: &str = "Se actualiza con if_seq_no para conservar los valores que el clúster ocultó (::es_redacted::).";

/// The definition merged into `base` as an update with `if_seq_no` does
/// (objects merged key by key, the rest replaced).
pub(crate) fn merge(base: &J, patch: &J) -> J {
    match (base, patch) {
        (J::Obj(b), J::Obj(p)) => {
            let mut out = b.clone();
            for (k, v) in p {
                match out.iter_mut().find(|(x, _)| x == k) {
                    Some((_, slot)) => *slot = merge(slot, v),
                    None => out.push((k.clone(), v.clone())),
                }
            }
            J::Obj(out)
        }
        _ => patch.clone(),
    }
}

/// A time value in milliseconds (`1m` → 60000).
fn time_ms(t: &str) -> Option<i64> {
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    let (n, unit) = t.split_at(digits);
    let n: i64 = n.parse().ok()?;
    let f = match unit {
        "ms" => 1,
        "s" => 1000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    n.checked_mul(f)
}

/// The definition with the throttle written as the stored watch writes it
/// (`throttle_period_in_millis`), so a merge doesn't keep both.
fn as_stored(body: &J, stored: &J) -> J {
    let mut b = body.clone();
    if stored.get("throttle_period_in_millis").is_some() {
        if let J::Obj(o) = &mut b {
            for (k, v) in o.iter_mut() {
                if k == "throttle_period" {
                    if let Some(ms) = v.as_str().and_then(time_ms) {
                        *k = "throttle_period_in_millis".into();
                        *v = J::Num(ms.into());
                    }
                }
            }
        }
    }
    b
}

pub(crate) fn plan(cur: Option<&Cur>, values: &FormValues) -> Result<Plan> {
    let want = spec(&values.values)?;
    let Some(cur) = cur else {
        if has_redacted(&want.body) {
            return Err(bad(REDACTED_NEW.into()));
        }
        return Ok(Plan {
            notes: vec![format!("Watch «{}»", want.id)],
            reqs: vec![put(&want.id, want.active, "", &want.body)],
            id: Some(super::Type::Watch.job_id(&want.id)),
            name: want.id.clone(),
            ..Plan::default()
        });
    };
    let have = spec(&values_of(cur))?;
    let seg = path_segment(&cur.id);
    let mut plan = Plan {
        notes: vec![format!("Watch «{}»", cur.id)],
        id: Some(super::Type::Watch.job_id(&want.id)),
        name: cur.id.clone(),
        ..Plan::default()
    };
    if want.id != cur.id {
        if has_redacted(&want.body) {
            return Err(bad(REDACTED_NEW.into()));
        }
        plan.notes.push(format!(
            "Un id nuevo crea el watch «{}» y borra «{}»: el historial queda con el id viejo.",
            want.id, cur.id
        ));
        plan.reqs.push(put(&want.id, want.active, "", &want.body));
        plan.reqs
            .push(Req::new("DELETE", format!("/_watcher/watch/{seg}"), None));
        return Ok(plan);
    }
    if !same(&want.body, &have.body) {
        if has_redacted(&want.body) {
            // Only an update with if_seq_no keeps the redacted values, and
            // it merges the new definition into the stored one.
            let body = as_stored(&want.body, &cur.watch);
            if !same(&merge(&cur.watch, &body), &body) {
                return Err(bad(REDACTED_REMOVAL.into()));
            }
            plan.notes.push(KEEPS_SECRETS.into());
            let guard = format!("if_seq_no={}&if_primary_term={}&", cur.seq, cur.term);
            plan.reqs.push(put(&cur.id, want.active, &guard, &body));
        } else {
            plan.reqs.push(put(&cur.id, want.active, "", &want.body));
        }
    } else if want.active != cur.active {
        let verb = if want.active {
            "_activate"
        } else {
            "_deactivate"
        };
        plan.reqs.push(Req::new(
            "PUT",
            format!("/_watcher/watch/{seg}/{verb}"),
            None,
        ));
    }
    Ok(plan)
}

pub(crate) fn delete_plan(cur: &Cur) -> Plan {
    Plan {
        notes: vec![format!(
            "Borra el watch «{}»; su historial (.watcher-history-*) se conserva.",
            cur.id
        )],
        reqs: vec![Req::new(
            "DELETE",
            format!("/_watcher/watch/{}", path_segment(&cur.id)),
            None,
        )],
        name: cur.id.clone(),
        ..Plan::default()
    }
}

/// "Reconocer": `_ack` on every action of the watch, which Watcher then
/// throttles until the watch's condition stops holding. Only an action
/// that ran since its condition last held (`ackable`) has something to
/// acknowledge.
pub(crate) fn ack_plan(cur: &Cur) -> Result<Plan> {
    if !cur.ackable {
        return Err(bad(format!(
            "Ninguna acción del watch «{}» espera un reconocimiento: se reconoce una acción que se ejecutó porque se cumplió la condición.",
            cur.id
        )));
    }
    Ok(Plan {
        notes: vec![format!(
            "Reconoce las acciones del watch «{}»: Watcher no las vuelve a ejecutar hasta que su condición deje de cumplirse.",
            cur.id
        )],
        reqs: vec![Req::new(
            "PUT",
            format!("/_watcher/watch/{}/_ack", path_segment(&cur.id)),
            None,
        )],
        name: cur.id.clone(),
        ..Plan::default()
    })
}

/// `_execute`'s body that runs the actions from `step` on: the earlier
/// ones skipped, the rest forced past their throttling; the run is
/// recorded in the history.
pub(crate) fn run_from_body(cur: &Cur, step: &str) -> Result<J> {
    let names: Vec<&str> = cur
        .watch
        .get("actions")
        .and_then(J::as_obj)
        .into_iter()
        .flatten()
        .map(|(k, _)| k.as_str())
        .collect();
    let at = names
        .iter()
        .position(|n| *n == step)
        .ok_or_else(|| bad(format!("«{}» no es una acción de este watch.", shown(step))))?;
    let modes = names
        .iter()
        .enumerate()
        .map(|(i, n)| {
            (
                n.to_string(),
                s(if i < at { "skip" } else { "force_execute" }),
            )
        })
        .collect();
    Ok(obj(vec![
        ("record_execution", Some(J::Bool(true))),
        ("action_modes", Some(J::Obj(modes))),
    ]))
}
