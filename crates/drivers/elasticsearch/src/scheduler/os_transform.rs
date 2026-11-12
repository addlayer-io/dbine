//! OpenSearch Index Management transforms: OpenSearch Dashboards'
//! transform job wizard as a form. `PUT _plugins/_transform/<id>` creates
//! one and, with `if_seq_no` / `if_primary_term`, changes its description,
//! schedule, page size and "enabled" in place. The plugin refuses to
//! change the source and target indices, the query, the groups, the
//! aggregations and "continuous" ("Not allowed to modify"), and an id is
//! never renamed: those changes delete the transform and create it again
//! (`_preview` checks the new one first), which loses its metadata
//! (progress and stats); the target index stays.

use super::{bad, json_text, obj, plain_id, s, same, Ctx, Plan, Req, Shape, Vals};
use crate::ddl::path_segment;
use crate::jobs::{iso_ms, num, rfc3339_ms, text};
use crate::json::J;
use crate::EsSession;
use dbine_driver::{FormField, FormKind, FormSection, FormValues, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(crate) const RECREATE_NOTE: &str = "El plugin de OpenSearch no deja cambiar en el lugar los índices, la consulta, los grupos, las agregaciones ni «Continuo» de un transform, ni su id: para eso DBine lo borra y lo vuelve a crear, y se pierde su progreso (el índice de destino se conserva).";

/// A transform as the cluster has it.
#[derive(Debug, Clone)]
pub(crate) struct Cur {
    pub(crate) id: String,
    pub(crate) t: J,
    pub(crate) seq: String,
    pub(crate) term: String,
}

impl Cur {
    fn enabled(&self) -> bool {
        self.t.get("enabled").and_then(J::as_bool).unwrap_or(false)
    }
}

pub(crate) async fn read(s: &EsSession, id: &str) -> Result<Cur> {
    let (hit, _) = s.os_transform(id).await?;
    Ok(Cur {
        id: id.to_string(),
        t: hit.get("transform").cloned().unwrap_or(J::Null),
        seq: text(hit.get("_seq_no")).unwrap_or_default(),
        term: text(hit.get("_primary_term")).unwrap_or_default(),
    })
}

/// The interval units, as the form names them.
fn units_kind() -> FormKind {
    FormKind::select(&[("minutes", "Minutos"), ("hours", "Horas"), ("days", "Días")])
}

pub(crate) fn sections() -> Vec<FormSection> {
    vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("id", "Id", FormKind::Text)
                .required()
                .placeholder("ventas-por-cliente")
                .help("Cambiarlo vuelve a crear el transform."),
            FormField::new("description", "Descripción", FormKind::Textarea),
            FormField::new("enabled", "Habilitado", FormKind::Bool)
                .default(json!(true))
                .help("Un transform habilitado se ejecuta según su programación."),
        ]),
        FormSection::new("source", "Origen").fields(vec![
            FormField::new("source_index", "Índice de origen", FormKind::Text)
                .required()
                .placeholder("ventas-*")
                .help("Un índice o un patrón."),
            FormField::new("source_query", "Consulta", FormKind::code("json"))
                .default(json!("{\n  \"match_all\": {}\n}"))
                .help("Query DSL que filtra los documentos de origen (data_selection_query)."),
        ]),
        FormSection::new("pivot", "Transformación").fields(vec![
            FormField::new("groups", "Grupos", FormKind::code("json"))
                .required()
                .placeholder("[{ \"terms\": { \"source_field\": \"cliente\", \"target_field\": \"cliente\" } }]")
                .help("Lista de terms, histogram o date_histogram, cada uno con source_field y target_field."),
            FormField::new("aggregations", "Agregaciones", FormKind::code("json"))
                .placeholder("{ \"total\": { \"sum\": { \"field\": \"importe\" } } }"),
        ]),
        FormSection::new("dest", "Destino").fields(vec![FormField::new(
            "dest_index",
            "Índice de destino",
            FormKind::Text,
        )
        .required()
        .placeholder("ventas-por-cliente")]),
        FormSection::new("schedule", "Programación").fields(vec![
            FormField::new(
                "schedule_kind",
                "Programación",
                FormKind::select(&[
                    ("interval", "Cada cierto tiempo"),
                    ("cron", "Expresión cron"),
                ]),
            )
            .required()
            .default(json!("interval")),
            FormField::new("interval", "Cada", FormKind::number(Some(1.0), None))
                .required()
                .when("schedule_kind", &["interval"])
                .default(json!(1)),
            FormField::new("interval_unit", "Unidad", units_kind())
                .required()
                .when("schedule_kind", &["interval"])
                .default(json!("minutes")),
            FormField::new("start_time", "A partir de", FormKind::DateTime)
                .when("schedule_kind", &["interval"])
                .help("Vacío: desde que se guarda."),
            FormField::new("cron", "Expresión cron", FormKind::Cron)
                .required()
                .when("schedule_kind", &["cron"])
                .placeholder("0 * * * *")
                .help("Cron de cinco campos: minutos horas día-del-mes mes día-de-la-semana."),
            FormField::new("timezone", "Zona horaria", FormKind::Text)
                .when("schedule_kind", &["cron"])
                .default(json!("UTC"))
                .placeholder("America/Argentina/Buenos_Aires"),
            FormField::new("continuous", "Continuo", FormKind::Bool)
                .help("Sigue procesando los documentos nuevos en cada ejecución; uno no continuo procesa todo una vez."),
        ]),
        FormSection::new("options", "Opciones").fields(vec![FormField::new(
            "page_size",
            "Tamaño de página",
            FormKind::number(Some(1.0), Some(10_000.0)),
        )
        .required()
        .default(json!(1000))
        .help("Buckets compuestos que procesa por búsqueda.")]),
    ]
}

/// A unit as the form names it (`Minutes`, `MINUTES` → `minutes`).
pub(crate) fn unit_of(u: Option<&J>) -> String {
    u.map(J::text).unwrap_or_default().to_ascii_lowercase()
}

pub(crate) fn values_of(cur: &Cur) -> BTreeMap<String, Value> {
    let t = &cur.t;
    let mut v = BTreeMap::new();
    v.insert("id".into(), json!(cur.id));
    v.insert(
        "description".into(),
        json!(text(t.get("description")).unwrap_or_default()),
    );
    v.insert("enabled".into(), json!(cur.enabled()));
    v.insert(
        "source_index".into(),
        json!(text(t.get("source_index")).unwrap_or_default()),
    );
    v.insert(
        "source_query".into(),
        json_text(t.get("data_selection_query")),
    );
    v.insert("groups".into(), json_text(t.get("groups")));
    v.insert("aggregations".into(), json_text(t.get("aggregations")));
    v.insert(
        "dest_index".into(),
        json!(text(t.get("target_index")).unwrap_or_default()),
    );
    let sch = t.get("schedule");
    if let Some(c) = sch.and_then(|x| x.get("cron")) {
        v.insert("schedule_kind".into(), json!("cron"));
        v.insert(
            "cron".into(),
            json!(text(c.get("expression")).unwrap_or_default()),
        );
        v.insert(
            "timezone".into(),
            json!(text(c.get("timezone")).unwrap_or_default()),
        );
    } else {
        let i = sch.and_then(|x| x.get("interval"));
        v.insert("schedule_kind".into(), json!("interval"));
        v.insert(
            "interval".into(),
            json!(num(i.and_then(|i| i.get("period"))).unwrap_or(1.0) as i64),
        );
        v.insert(
            "interval_unit".into(),
            json!(unit_of(i.and_then(|i| i.get("unit")))),
        );
        v.insert(
            "start_time".into(),
            json!(num(i.and_then(|i| i.get("start_time")))
                .map(|ms| rfc3339_ms(ms as i64))
                .unwrap_or_default()),
        );
    }
    v.insert(
        "continuous".into(),
        json!(t.get("continuous").and_then(J::as_bool).unwrap_or(false)),
    );
    v.insert(
        "page_size".into(),
        json!(num(t.get("page_size")).unwrap_or(1000.0) as i64),
    );
    v
}

/// A transform as the form describes it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tf {
    pub(crate) id: String,
    /// `transform`'s object.
    pub(crate) t: J,
}

/// What the plugin won't change in place.
const FIXED: [&str; 6] = [
    "source_index",
    "target_index",
    "data_selection_query",
    "groups",
    "aggregations",
    "continuous",
];

/// The interval's unit as the transform API writes it.
fn transform_unit(u: &str) -> Option<&'static str> {
    Some(match u {
        "minutes" => "Minutes",
        "hours" => "Hours",
        "days" => "Days",
        _ => return None,
    })
}

/// `now_ms`: the interval's start when the form leaves it empty.
pub(crate) fn spec(values: &BTreeMap<String, Value>, now_ms: i64) -> Result<Tf> {
    let v = Vals(values);
    let id = plain_id(v.opt("id"), "Id")?;
    let source = v
        .line("source_index", "Índice de origen")?
        .ok_or_else(|| bad("Falta el índice de origen del transform.".into()))?;
    let target = v
        .line("dest_index", "Índice de destino")?
        .ok_or_else(|| bad("Falta el índice de destino del transform.".into()))?;
    let groups = v
        .json("groups", "Grupos", Shape::Array)?
        .ok_or_else(|| bad("Faltan los grupos del transform.".into()))?;
    let schedule = match v.text("schedule_kind").as_str() {
        "interval" | "" => {
            let n = v
                .int("interval", "Cada", 1, i64::from(i32::MAX))?
                .ok_or_else(|| bad("Falta el intervalo del transform.".into()))?;
            let unit = transform_unit(&v.text("interval_unit"))
                .ok_or_else(|| bad("«Unidad» tiene que ser minutos, horas o días.".into()))?;
            let start = match v.opt("start_time") {
                None => now_ms,
                Some(t) => iso_ms(&t).ok_or_else(|| {
                    bad(format!(
                        "«A partir de»: «{}» no es una fecha y hora válida.",
                        super::shown(&t)
                    ))
                })?,
            };
            obj(vec![(
                "interval",
                Some(obj(vec![
                    ("start_time", Some(J::Num(start.into()))),
                    ("period", Some(J::Num(n.into()))),
                    ("unit", Some(s(unit))),
                ])),
            )])
        }
        "cron" => cron_schedule(v, "cron", "timezone")?,
        other => {
            return Err(bad(format!(
                "«{}» no es un tipo de programación.",
                super::shown(other)
            )))
        }
    };
    let page = v
        .int("page_size", "Tamaño de página", 1, 10_000)?
        .unwrap_or(1000);
    let t = obj(vec![
        ("enabled", Some(J::Bool(v.bool_or("enabled", true)))),
        ("continuous", Some(J::Bool(v.bool("continuous")))),
        ("schedule", Some(schedule)),
        ("description", Some(s(v.text("description")))),
        ("source_index", Some(s(source))),
        ("target_index", Some(s(target))),
        (
            "data_selection_query",
            Some(
                v.json("source_query", "Consulta", Shape::Object)?
                    .unwrap_or_else(|| obj(vec![("match_all", Some(J::Obj(Vec::new())))])),
            ),
        ),
        ("page_size", Some(J::Num(page.into()))),
        ("groups", Some(groups)),
        (
            "aggregations",
            Some(
                v.json("aggregations", "Agregaciones", Shape::Object)?
                    .unwrap_or(J::Obj(Vec::new())),
            ),
        ),
    ]);
    Ok(Tf { id, t })
}

/// `{ cron: { expression, timezone } }` from two fields (five-field cron).
pub(crate) fn cron_schedule(v: Vals, cron: &str, tz: &str) -> Result<J> {
    let expr = v
        .cron(cron, "Expresión cron", 5..=5)?
        .ok_or_else(|| bad("Falta la expresión cron.".into()))?;
    let zone = v.line(tz, "Zona horaria")?.unwrap_or_else(|| "UTC".into());
    if !zone
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "/_+-:".contains(c))
    {
        return Err(bad(format!(
            "«Zona horaria»: «{}» no es una zona válida.",
            super::shown(&zone)
        )));
    }
    Ok(obj(vec![(
        "cron",
        Some(obj(vec![
            ("expression", Some(s(expr))),
            ("timezone", Some(s(zone))),
        ])),
    )]))
}

fn body(t: &Tf) -> J {
    obj(vec![("transform", Some(t.t.clone()))])
}

pub(crate) fn plan(c: &Ctx, cur: Option<&Cur>, values: &FormValues) -> Result<Plan> {
    let p = c.plugins;
    let now = now_ms();
    let want = spec(&values.values, now)?;
    let Some(cur) = cur else {
        return Ok(Plan {
            notes: vec![format!("Transform «{}»", want.id)],
            reqs: vec![Req::new(
                "PUT",
                format!("/{p}/_transform/{}", path_segment(&want.id)),
                Some(body(&want)),
            )],
            id: Some(super::Type::OsTransform.job_id(&want.id)),
            name: want.id.clone(),
            ..Plan::default()
        });
    };
    let have = spec(&values_of(cur), now)?;
    Ok(plan_change(p, cur, &have, &want))
}

pub(crate) fn plan_change(p: &str, cur: &Cur, have: &Tf, want: &Tf) -> Plan {
    let seg = path_segment(&cur.id);
    let mut plan = Plan {
        notes: vec![format!("Transform «{}»", cur.id)],
        id: Some(super::Type::OsTransform.job_id(&want.id)),
        name: cur.id.clone(),
        ..Plan::default()
    };
    let fixed_changed = FIXED
        .iter()
        .any(|k| !super::same_opt(want.t.get(k), have.t.get(k)));
    if want.id != cur.id || fixed_changed {
        plan.notes.push(RECREATE_NOTE.into());
        let new_seg = path_segment(&want.id);
        if want.id == cur.id {
            plan.reqs.push(
                Req::new(
                    "POST",
                    format!("/{p}/_transform/_preview"),
                    Some(body(want)),
                )
                .note("Comprueba la definición nueva antes de borrar la actual."),
            );
        }
        let put = Req::new(
            "PUT",
            format!("/{p}/_transform/{new_seg}"),
            Some(body(want)),
        );
        if want.id != cur.id {
            plan.reqs.push(
                put.clone()
                    .note(&format!("El transform nuevo, «{}».", want.id)),
            );
        }
        if cur.enabled() {
            plan.reqs.push(Req::new(
                "POST",
                format!("/{p}/_transform/{seg}/_stop"),
                None,
            ));
        }
        plan.reqs.push(
            Req::new("DELETE", format!("/{p}/_transform/{seg}"), None)
                .note("Borra el transform (su índice de destino se conserva)."),
        );
        if want.id == cur.id {
            plan.reqs.push(put);
        }
        return plan;
    }
    if !same(&want.t, &have.t) {
        plan.reqs.push(Req::new(
            "PUT",
            format!(
                "/{p}/_transform/{seg}?if_seq_no={}&if_primary_term={}",
                cur.seq, cur.term
            ),
            Some(body(want)),
        ));
    }
    plan
}

pub(crate) fn delete_plan(p: &str, cur: &Cur) -> Plan {
    let seg = path_segment(&cur.id);
    let mut reqs = Vec::new();
    if cur.enabled() {
        reqs.push(Req::new(
            "POST",
            format!("/{p}/_transform/{seg}/_stop"),
            None,
        ));
    }
    reqs.push(Req::new("DELETE", format!("/{p}/_transform/{seg}"), None));
    Plan {
        notes: vec![format!(
            "Borra el transform «{}»; su índice de destino se conserva.",
            cur.id
        )],
        reqs,
        name: cur.id.clone(),
        ..Plan::default()
    }
}

/// Now, in milliseconds since 1970.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
