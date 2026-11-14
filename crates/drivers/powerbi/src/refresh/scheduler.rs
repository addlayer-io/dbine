//! Refresh schedules in the scheduler designer (spec §4.6 `scheduler_*`).
//! Owned by part C.
//!
//! - Kinds: `refresh_schedule` ("Programaciones de actualización", edit
//!   only; actions `run` "Actualizar ahora…" with a form and `cancel`
//!   "Cancelar la actualización en curso") and `dq_refresh_schedule`
//!   ("Actualización de caché (DirectQuery)", edit only); no logs; notes:
//!   owner-only edits, 8 or 48 slots.
//! - Forms: `enabled`, `days`, `times` (`HH:MM` on :00/:30, at most 8 or
//!   48), `localTimeZoneId` ([`super::timezones`]), `notifyOption`
//!   (service principals: `NoNotification` only); DQ: frequency or
//!   days/times.
//! - Scripts are comment-only: `// ⚠ …` warnings first, then `// PATCH
//!   <url>` and the JSON body as `//` lines; `// POST …/refreshes` and
//!   `// DELETE …/refreshes/<requestId>` for the actions.
//! - `scheduler_save` validates again and sends the PATCH (a disable
//!   alone); delete isn't offered (a schedule is disabled, not deleted).
//!
//! "Actualizar ahora…" on a workspace with dedicated capacity is an
//! enhanced refresh (Learn, "Enhanced refresh with the Power BI REST API":
//! `type`, `commitMode`, `maxParallelism`, `retryCount`, `timeout`,
//! `objects`, `applyRefreshPolicy`, `effectiveDate`, no `notifyOption`); on
//! shared capacity it's the standard one, `notifyOption` only, counted
//! against the eight a day.

use super::{
    api_url, comment_script, forget, job_id, job_model, runs_from, schedule_path, survey,
    timezones, Kind, Schedule, DAYS,
};
use crate::catalog::Model;
use crate::http::{Method, Priority};
use crate::meta::service::{
    check_status, is_owner, is_service_principal, refuse_on_read_only, Service,
};
use crate::Ctx;
use dbine_driver::{
    Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerAction,
    SchedulerInfo, SchedulerItem, SchedulerKind, SchedulerList,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub const REFRESH_KIND: &str = "refresh_schedule";
pub const DQ_KIND: &str = "dq_refresh_schedule";

/// The refresh types of an enhanced refresh.
const TYPES: &[&str] = &[
    "Full",
    "ClearValues",
    "Calculate",
    "DataOnly",
    "Automatic",
    "Defragment",
];
const COMMIT_MODES: &[&str] = &["transactional", "partialBatch"];
/// What the DirectQuery cache's frequency takes, in minutes.
const DQ_FREQUENCIES: &[u64] = &[15, 30, 60, 120, 180];
/// Separates a table from a partition in the `objects` choice's values.
const PART: char = '\u{1f}';

fn kind_of(key: &str) -> Result<Kind> {
    match key {
        REFRESH_KIND => Ok(Kind::Import),
        DQ_KIND => Ok(Kind::DirectQuery),
        _ => Err(Error::Unsupported(format!(
            "Power BI no tiene objetos «{key}» en su programador."
        ))),
    }
}

pub async fn scheduler_info(cx: &mut Ctx) -> Result<SchedulerInfo> {
    Ok(info_in(cx))
}

pub(crate) fn info_in<S: Service>(s: &S) -> SchedulerInfo {
    let edit = !s.settings().read_only;
    let mut refresh = SchedulerKind::new(
        REFRESH_KIND,
        "Programaciones de actualización",
        "programación",
    )
    .abilities(false, edit, false);
    if edit {
        refresh = refresh
            .action(SchedulerAction::new("run", "Actualizar ahora…").with_params())
            .action(SchedulerAction::new(
                "cancel",
                "Cancelar la actualización en curso",
            ));
    }
    let dq = SchedulerKind::new(
        DQ_KIND,
        "Actualización de caché (DirectQuery)",
        "programación de caché",
    )
    .abilities(false, edit, false);
    let mut notes = vec![
        "Solo el propietario de un modelo puede cambiar su programación.".to_string(),
        "Una programación tiene hasta 8 horarios por día en capacidad compartida (Pro) y hasta 48 en capacidad Premium o Fabric.".into(),
        "Las programaciones no se crean ni se borran: cada modelo tiene la suya, que se activa o se desactiva.".into(),
    ];
    if !edit {
        notes.push(read_only_note());
    }
    SchedulerInfo {
        kinds: vec![refresh, dq],
        logs: false,
        notes,
    }
}

pub(crate) fn read_only_note() -> String {
    "La conexión es de solo lectura: se ven las actualizaciones y sus programaciones, pero no se ejecutan ni se cambian.".into()
}

pub async fn scheduler_items(cx: &mut Ctx, kind: &str) -> Result<SchedulerList> {
    items_in(cx, kind).await
}

pub(crate) async fn items_in<S: Service>(s: &mut S, kind: &str) -> Result<SchedulerList> {
    let want = kind_of(kind)?;
    let read_only = s.settings().read_only;
    let survey = survey(s).await?;
    let mut list = SchedulerList {
        notes: survey.notes,
        ..Default::default()
    };
    let mut not_owned = 0;
    for r in survey.models.into_iter().filter(|r| r.kind == want) {
        let owner = is_owner(s, &r.model);
        if owner == Some(false) {
            not_owned += 1;
        }
        let actions = if want == Kind::DirectQuery {
            None
        } else if read_only || !r.model.write_permission {
            Some(Vec::new())
        } else {
            match r.running() {
                Some(run) if run.enhanced() => Some(vec!["cancel".to_string()]),
                Some(_) => Some(Vec::new()),
                None => Some(vec!["run".to_string()]),
            }
        };
        let summary = r.schedule.as_ref().and_then(Schedule::summary);
        list.items.push(SchedulerItem {
            id: job_id(want, &r.model),
            name: r.model.name.clone(),
            description: Some(match summary {
                Some(sm) => format!("{} · {sm}", r.model.workspace.name),
                None => r.model.workspace.name.clone(),
            }),
            enabled: match want {
                Kind::Import => Some(r.schedule.as_ref().is_some_and(|x| x.fires(want))),
                Kind::DirectQuery => None,
            },
            system: false,
            can_edit: (read_only || owner == Some(false)).then_some(false),
            can_delete: None,
            actions,
            run_from_step: None,
            clear_history: None,
        });
    }
    if not_owned > 0 {
        list.notes.push(format!(
            "{not_owned} de estos modelos son de otro propietario: solo él puede cambiar su programación."
        ));
    }
    if read_only {
        list.notes.push(read_only_note());
    }
    Ok(list)
}

fn days_kind() -> FormKind {
    FormKind::multi(&[
        ("Sunday", "domingo"),
        ("Monday", "lunes"),
        ("Tuesday", "martes"),
        ("Wednesday", "miércoles"),
        ("Thursday", "jueves"),
        ("Friday", "viernes"),
        ("Saturday", "sábado"),
    ])
}

fn zones_kind() -> FormKind {
    FormKind::Select {
        options: timezones::WINDOWS_TIME_ZONES
            .iter()
            .map(|(id, label)| (id.to_string(), label.to_string()))
            .collect(),
    }
}

/// At most this many times a day (Learn, "Data refresh in Power BI").
pub(crate) fn max_slots(m: &Model) -> usize {
    if m.workspace.on_dedicated_capacity {
        48
    } else {
        8
    }
}

pub async fn scheduler_form(cx: &mut Ctx, kind: &str, id: Option<&str>) -> Result<ObjectForm> {
    form_in(cx, kind, id).await
}

pub(crate) async fn form_in<S: Service>(
    s: &mut S,
    kind: &str,
    id: Option<&str>,
) -> Result<ObjectForm> {
    let want = kind_of(kind)?;
    let Some(id) = id else {
        return Err(Error::Unsupported(
            "Las programaciones no se crean: cada modelo tiene una. Elegí el modelo en la lista."
                .into(),
        ));
    };
    let (k, m) = job_model(s, id).await?;
    if k != want {
        return Err(Error::Query(
            "Ese trabajo es de otro tipo de programación.".into(),
        ));
    }
    let current = Schedule::from_json(&s.get(&schedule_path(k, &m), Priority::Editor).await?);
    let sp = is_service_principal(s);
    let mut values = BTreeMap::new();
    let mut fields = Vec::new();
    let times_help = if m.workspace.on_dedicated_capacity {
        "Uno por línea, en punto o y media (HH:00 o HH:30). Hasta 48 por día en capacidad Premium o Fabric."
    } else {
        "Uno por línea, en punto o y media (HH:00 o HH:30). Hasta 8 por día en capacidad compartida."
    };
    match k {
        Kind::Import => {
            fields.push(FormField::new("enabled", "Activada", FormKind::Bool));
            values.insert("enabled".into(), json!(current.enabled.unwrap_or(false)));
            fields.push(FormField::new("days", "Días", days_kind()).when("enabled", &["true"]));
            fields.push(
                FormField::new("times", "Horarios", FormKind::Textarea)
                    .help(times_help)
                    .placeholder("05:00\n17:30")
                    .when("enabled", &["true"]),
            );
            fields.push(
                FormField::new("localTimeZoneId", "Zona horaria", zones_kind())
                    .when("enabled", &["true"]),
            );
            let notify = if sp {
                FormField::new(
                    "notifyOption",
                    "Aviso",
                    FormKind::select(&[("NoNotification", "Sin aviso")]),
                )
                .help("Una entidad de servicio no recibe correos: solo «Sin aviso».")
            } else {
                FormField::new(
                    "notifyOption",
                    "Aviso",
                    FormKind::select(&[
                        ("MailOnFailure", "Avisar por correo si falla"),
                        ("NoNotification", "Sin aviso"),
                    ]),
                )
            };
            fields.push(notify.when("enabled", &["true"]));
            values.insert(
                "notifyOption".into(),
                json!(if sp {
                    "NoNotification".to_string()
                } else {
                    current
                        .notify
                        .clone()
                        .unwrap_or_else(|| "MailOnFailure".into())
                }),
            );
        }
        Kind::DirectQuery => {
            fields.push(FormField::new(
                "mode",
                "Frecuencia",
                FormKind::select(&[
                    ("frequency", "Cada cierto tiempo"),
                    ("times", "Días y horarios"),
                ]),
            ));
            values.insert(
                "mode".into(),
                json!(if current.frequency.is_some() || current.times.is_empty() {
                    "frequency"
                } else {
                    "times"
                }),
            );
            fields.push(
                FormField::new(
                    "frequency",
                    "Cada",
                    FormKind::select(&[
                        ("15", "15 minutos"),
                        ("30", "30 minutos"),
                        ("60", "1 hora"),
                        ("120", "2 horas"),
                        ("180", "3 horas"),
                    ]),
                )
                .when("mode", &["frequency"]),
            );
            values.insert(
                "frequency".into(),
                json!(current.frequency.unwrap_or(60).to_string()),
            );
            fields.push(FormField::new("days", "Días", days_kind()).when("mode", &["times"]));
            fields.push(
                FormField::new("times", "Horarios", FormKind::Textarea)
                    .help(times_help)
                    .placeholder("05:00\n17:30")
                    .when("mode", &["times"]),
            );
            fields.push(
                FormField::new("localTimeZoneId", "Zona horaria", zones_kind())
                    .when("mode", &["times"]),
            );
        }
    }
    values.insert("days".into(), json!(current.days));
    values.insert("times".into(), json!(current.times.join("\n")));
    values.insert(
        "localTimeZoneId".into(),
        json!(current.time_zone.clone().unwrap_or_else(|| "UTC".into())),
    );
    let read_only = s.settings().read_only || is_owner(s, &m) == Some(false);
    let mut notes = vec![owner_note(&m)];
    if !m.workspace.on_dedicated_capacity && k == Kind::Import {
        notes.push(shared_note().into());
    }
    if s.settings().read_only {
        notes.push(read_only_note());
    }
    Ok(ObjectForm {
        sections: vec![FormSection::new("general", "General").fields(fields)],
        values,
        choices: BTreeMap::new(),
        notes,
        read_only,
    })
}

fn owner_note(m: &Model) -> String {
    match &m.configured_by {
        Some(o) => format!(
            "Solo el propietario del modelo ({o}) puede cambiar su programación: si no sos vos, Power BI rechaza el cambio."
        ),
        None => "Solo el propietario del modelo puede cambiar su programación: si no sos vos, Power BI rechaza el cambio.".into(),
    }
}

fn shared_note() -> &'static str {
    "En capacidad compartida las actualizaciones, programadas o no, son como máximo 8 por día."
}

fn value_str(v: &FormValues, key: &str) -> Option<String> {
    match v.values.get(key)? {
        Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn value_bool(v: &FormValues, key: &str) -> Option<bool> {
    match v.values.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => Some(s.eq_ignore_ascii_case("true")),
        _ => None,
    }
}

/// A list value: an array, or text split on lines and commas.
fn value_list(v: &FormValues, key: &str) -> Vec<String> {
    match v.values.get(key) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|x| match x {
                Value::String(s) => Some(s.trim().to_string()),
                Value::Number(n) => Some(n.to_string()),
                _ => None,
            })
            .filter(|s| !s.is_empty())
            .collect(),
        Some(Value::String(s)) => s
            .split(['\n', ',', ';'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    }
}

fn days(v: &FormValues) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for d in value_list(v, "days") {
        let Some((en, _, _)) = DAYS
            .iter()
            .find(|(en, es, _)| en.eq_ignore_ascii_case(&d) || es.eq_ignore_ascii_case(&d))
        else {
            return Err(Error::Query(format!("«{d}» no es un día de la semana.")));
        };
        if !out.iter().any(|x| x == en) {
            out.push(en.to_string());
        }
    }
    if out.is_empty() {
        return Err(Error::Query("Elegí al menos un día.".into()));
    }
    // In the week's order, as Power BI shows them.
    out.sort_by_key(|d| DAYS.iter().position(|(en, _, _)| en == d));
    Ok(out)
}

/// `HH:MM` on the hour or the half hour, at most `max`, sorted, once each.
pub(crate) fn times(v: &FormValues, max: usize) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for t in value_list(v, "times") {
        let bad = || Error::Query(format!("«{t}» no es un horario HH:MM en punto o y media."));
        let (h, m) = t.split_once(':').ok_or_else(bad)?;
        let h: u32 = h.trim().parse().map_err(|_| bad())?;
        let m = m.trim();
        if h > 23 || !(m == "00" || m == "30") {
            return Err(bad());
        }
        let norm = format!("{h:02}:{m}");
        if !out.contains(&norm) {
            out.push(norm);
        }
    }
    if out.is_empty() {
        return Err(Error::Query("Indicá al menos un horario.".into()));
    }
    if out.len() > max {
        return Err(Error::Query(format!(
            "Son {} horarios: este modelo admite hasta {max} por día.",
            out.len()
        )));
    }
    out.sort();
    Ok(out)
}

fn time_zone(v: &FormValues) -> Result<String> {
    let tz = value_str(v, "localTimeZoneId").unwrap_or_else(|| "UTC".into());
    if !timezones::known(&tz) {
        return Err(Error::Query(format!(
            "«{tz}» no es una zona horaria de Windows."
        )));
    }
    Ok(tz)
}

/// The PATCH body for a schedule change and what its script warns about.
pub(crate) fn plan<S: Service>(
    s: &S,
    kind: Kind,
    m: &Model,
    v: &FormValues,
) -> Result<(Value, Vec<String>)> {
    let mut warnings = vec![owner_note(m)];
    let body = match kind {
        Kind::Import => {
            let enabled = value_bool(v, "enabled").unwrap_or(true);
            if !enabled {
                warnings.push("Desactivarla envía solo «enabled: false» (Power BI no acepta nada más junto con eso): los días y horarios quedan guardados como estaban.".into());
                json!({"value": {"enabled": false}})
            } else {
                let notify = value_str(v, "notifyOption").unwrap_or_else(|| "MailOnFailure".into());
                if !matches!(notify.as_str(), "MailOnFailure" | "NoNotification") {
                    return Err(Error::Query(format!(
                        "«{notify}» no es una opción de aviso."
                    )));
                }
                if is_service_principal(s) && notify != "NoNotification" {
                    return Err(Error::Query(
                        "Una entidad de servicio solo puede programar con «Sin aviso».".into(),
                    ));
                }
                if !m.workspace.on_dedicated_capacity {
                    warnings.push(shared_note().into());
                }
                json!({"value": {
                    "enabled": true,
                    "days": days(v)?,
                    "times": times(v, max_slots(m))?,
                    "localTimeZoneId": time_zone(v)?,
                    "notifyOption": notify,
                }})
            }
        }
        Kind::DirectQuery => match value_str(v, "mode").as_deref().unwrap_or("frequency") {
            "frequency" => {
                let f: u64 = value_str(v, "frequency")
                    .and_then(|f| f.parse().ok())
                    .filter(|f| DQ_FREQUENCIES.contains(f))
                    .ok_or_else(|| {
                        Error::Query(
                            "La frecuencia va de 15 minutos a 3 horas (15, 30, 60, 120 o 180)."
                                .into(),
                        )
                    })?;
                json!({"value": {"frequency": f}})
            }
            "times" => json!({"value": {
                "days": days(v)?,
                "times": times(v, max_slots(m))?,
                "localTimeZoneId": time_zone(v)?,
            }}),
            other => return Err(Error::Query(format!("«{other}» no es una frecuencia."))),
        },
    };
    Ok((body, warnings))
}

pub async fn scheduler_script(
    cx: &mut Ctx,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<String> {
    script_in(cx, kind, id, values).await
}

pub(crate) async fn script_in<S: Service>(
    s: &mut S,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<String> {
    let want = kind_of(kind)?;
    let id = id.ok_or_else(|| Error::Unsupported("Las programaciones no se crean.".into()))?;
    let (k, m) = job_model(s, id).await?;
    if k != want {
        return Err(Error::Query(
            "Ese trabajo es de otro tipo de programación.".into(),
        ));
    }
    let (body, warnings) = plan(s, k, &m, values)?;
    Ok(comment_script(
        &warnings,
        "PATCH",
        &api_url(s, &schedule_path(k, &m)),
        Some(&body),
    ))
}

pub async fn scheduler_save(
    cx: &mut Ctx,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Option<String>> {
    save_in(cx, kind, id, values).await
}

pub(crate) async fn save_in<S: Service>(
    s: &mut S,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Option<String>> {
    refuse_on_read_only(s, "cambiar la programación")?;
    let want = kind_of(kind)?;
    let id = id.ok_or_else(|| Error::Unsupported("Las programaciones no se crean.".into()))?;
    let (k, m) = job_model(s, id).await?;
    if k != want {
        return Err(Error::Query(
            "Ese trabajo es de otro tipo de programación.".into(),
        ));
    }
    refuse_if_not_owner(s, &m)?;
    let (body, _) = plan(s, k, &m, values)?;
    let r = s
        .send(
            Method::Patch,
            &schedule_path(k, &m),
            Some(&body),
            Priority::Editor,
        )
        .await?;
    check_status(r, "No se pudo cambiar la programación")?;
    forget(s, &m);
    Ok(Some(job_id(k, &m)))
}

/// Refuses a schedule change by someone who isn't the model's owner, when
/// both are known (the service would refuse it anyway).
pub(crate) fn refuse_if_not_owner<S: Service>(s: &S, m: &Model) -> Result<()> {
    if is_owner(s, m) == Some(false) {
        return Err(Error::Query(format!(
            "Solo el propietario del modelo ({}) puede cambiar su programación.",
            m.configured_by.as_deref().unwrap_or("otro usuario")
        )));
    }
    Ok(())
}

pub async fn scheduler_action_form(
    cx: &mut Ctx,
    kind: &str,
    id: &str,
    action: &str,
) -> Result<ObjectForm> {
    action_form_in(cx, kind, id, action).await
}

fn action_model_kind(kind: &str) -> Result<()> {
    if kind_of(kind)? != Kind::Import {
        return Err(Error::Unsupported(
            "La caché de DirectQuery no tiene acciones: se actualiza sola según su programación."
                .into(),
        ));
    }
    Ok(())
}

pub(crate) async fn action_form_in<S: Service>(
    s: &mut S,
    kind: &str,
    id: &str,
    action: &str,
) -> Result<ObjectForm> {
    action_model_kind(kind)?;
    let (_, m) = job_model(s, id).await?;
    let mut form = ObjectForm::default();
    match action {
        "run" if m.workspace.on_dedicated_capacity => {
            let mut fields = vec![
                FormField::new(
                    "type",
                    "Tipo",
                    FormKind::select(&[
                        ("Full", "Completa"),
                        ("ClearValues", "Borrar valores"),
                        ("Calculate", "Recalcular"),
                        ("DataOnly", "Solo datos"),
                        ("Automatic", "Automática"),
                        ("Defragment", "Desfragmentar"),
                    ]),
                ),
                FormField::new(
                    "commitMode",
                    "Confirmación",
                    FormKind::select(&[
                        ("transactional", "Transaccional (todo o nada)"),
                        ("partialBatch", "Por lotes"),
                    ]),
                ),
                FormField::new(
                    "maxParallelism",
                    "Paralelismo máximo",
                    FormKind::number(Some(1.0), Some(100.0)),
                )
                .help("Cuántas tablas o particiones se procesan a la vez."),
                FormField::new(
                    "retryCount",
                    "Reintentos",
                    FormKind::number(Some(0.0), Some(10.0)),
                ),
                FormField::new("timeout", "Tiempo máximo", FormKind::Text)
                    .placeholder("05:00:00")
                    .help("hh:mm:ss, hasta 24 horas."),
            ];
            form.values.insert("type".into(), json!("Full"));
            form.values
                .insert("commitMode".into(), json!("transactional"));
            form.values.insert("maxParallelism".into(), json!(10));
            form.values.insert("retryCount".into(), json!(0));
            form.values.insert("timeout".into(), json!("05:00:00"));
            // Tables and partitions from the model's metadata, when readable.
            if let Ok(meta) = crate::meta::model_for(s, &m).await {
                let mut options = Vec::new();
                for t in meta.plain_tables() {
                    options.push((t.name.clone(), t.name.clone()));
                    if t.partitions.len() > 1 {
                        for p in &t.partitions {
                            options.push((
                                format!("{}{PART}{}", t.name, p.name),
                                format!("{} › {}", t.name, p.name),
                            ));
                        }
                    }
                }
                if !options.is_empty() {
                    fields.push(
                        FormField::new(
                            "objects",
                            "Tablas y particiones",
                            FormKind::MultiSelect { options },
                        )
                        .help("Vacío: todo el modelo."),
                    );
                }
            }
            // Learn (enhanced refresh): with commitMode partialBatch,
            // applyRefreshPolicy must be false, so both are offered only
            // for a transactional refresh.
            fields.push(
                FormField::new("applyRefreshPolicy", "Aplicar la actualización incremental", FormKind::Bool)
                    .help("Las tablas con actualización incremental refrescan solo sus particiones recientes.")
                    .when("commitMode", &["transactional"]),
            );
            form.values.insert("applyRefreshPolicy".into(), json!(true));
            fields.push(
                FormField::new("effectiveDate", "Fecha efectiva", FormKind::Date)
                    .help("La fecha que la actualización incremental toma como hoy (vacío: hoy).")
                    .when("commitMode", &["transactional"])
                    .and_when("applyRefreshPolicy", &["true"]),
            );
            form.sections = vec![FormSection::new("general", "General").fields(fields)];
            form.notes = vec![
                "Es una actualización mejorada: se puede cancelar mientras corre.".into(),
                "Power BI actualiza un modelo por vez: si ya hay una actualización en curso, rechaza esta.".into(),
            ];
        }
        "run" => {
            form.notes = vec![
                "En capacidad compartida se pide una actualización estándar: cuenta para el límite de 8 por día y no se puede cancelar.".into(),
            ];
        }
        "cancel" => {
            form.notes = vec![
                "Cancela la actualización mejorada en curso. Las programadas y las del portal no se cancelan por la API.".into(),
            ];
        }
        other => {
            return Err(Error::Unsupported(format!(
                "«{other}» no es una acción de las programaciones de Power BI."
            )))
        }
    }
    if s.settings().read_only {
        form.read_only = true;
        form.notes.push(read_only_note());
    }
    Ok(form)
}

/// The body of "Actualizar ahora…" and what its script warns about.
pub(crate) fn run_plan(m: &Model, v: &FormValues) -> Result<(Value, Vec<String>)> {
    let mut warnings = vec![
        "Power BI actualiza un modelo por vez: si ya hay una actualización en curso, rechaza esta."
            .to_string(),
    ];
    if !m.workspace.on_dedicated_capacity {
        warnings.push(
            "Cuenta para el límite de 8 actualizaciones por día de la capacidad compartida.".into(),
        );
        return Ok((json!({"notifyOption": "NoNotification"}), warnings));
    }
    let t = value_str(v, "type").unwrap_or_else(|| "Full".into());
    if !TYPES.contains(&t.as_str()) {
        return Err(Error::Query(format!(
            "«{t}» no es un tipo de actualización."
        )));
    }
    let commit = value_str(v, "commitMode").unwrap_or_else(|| "transactional".into());
    if !COMMIT_MODES.contains(&commit.as_str()) {
        return Err(Error::Query(format!(
            "«{commit}» no es un modo de confirmación."
        )));
    }
    let number = |key: &str,
                  default: u64,
                  range: std::ops::RangeInclusive<u64>,
                  what: &str|
     -> Result<u64> {
        let n = match value_str(v, key) {
            None => default,
            Some(x) => x
                .parse::<f64>()
                .ok()
                .filter(|f| f.fract() == 0.0 && *f >= 0.0)
                .map(|f| f as u64)
                .ok_or_else(|| Error::Query(format!("{what}: «{x}» no es un número entero.")))?,
        };
        if !range.contains(&n) {
            return Err(Error::Query(format!(
                "{what} va de {} a {}.",
                range.start(),
                range.end()
            )));
        }
        Ok(n)
    };
    let parallel = number("maxParallelism", 10, 1..=100, "El paralelismo máximo")?;
    let retries = number("retryCount", 0, 0..=10, "Los reintentos")?;
    let timeout = value_str(v, "timeout").unwrap_or_else(|| "05:00:00".into());
    let secs = parse_hms(&timeout)
        .ok_or_else(|| Error::Query(format!("«{timeout}» no es un tiempo hh:mm:ss.")))?;
    if secs == 0 || secs > 24 * 3600 {
        return Err(Error::Query(
            "El tiempo máximo va de 1 segundo a 24 horas.".into(),
        ));
    }
    let mut body = serde_json::Map::new();
    body.insert("type".into(), json!(t));
    body.insert("commitMode".into(), json!(commit));
    body.insert("maxParallelism".into(), json!(parallel));
    body.insert("retryCount".into(), json!(retries));
    body.insert("timeout".into(), json!(timeout));
    let objects: Vec<Value> = value_list(v, "objects")
        .into_iter()
        .map(|o| match o.split_once(PART) {
            Some((t, p)) => json!({"table": t, "partition": p}),
            None => json!({"table": o}),
        })
        .collect();
    if !objects.is_empty() {
        body.insert("objects".into(), Value::Array(objects));
    }
    // Learn (enhanced refresh): "If commitMode is partialBatch,
    // applyRefreshPolicy of true isn't supported, and applyRefreshPolicy
    // must be set to false." A batched refresh therefore always sends false,
    // and the script says so.
    let policy = if commit == "partialBatch" {
        warnings.push(
            "Con la confirmación por lotes Power BI no admite la actualización incremental: se envía applyRefreshPolicy en false y las tablas con esa política se actualizan enteras."
                .into(),
        );
        false
    } else {
        value_bool(v, "applyRefreshPolicy").unwrap_or(true)
    };
    body.insert("applyRefreshPolicy".into(), json!(policy));
    if let Some(d) = value_str(v, "effectiveDate") {
        if chrono::NaiveDate::parse_from_str(&d, "%Y-%m-%d").is_err() {
            return Err(Error::Query(format!("«{d}» no es una fecha AAAA-MM-DD.")));
        }
        // The date incremental refresh takes as today: meaningless without
        // the policy.
        if policy {
            body.insert("effectiveDate".into(), json!(d));
        }
    }
    if t == "ClearValues" {
        warnings.push(
            "«Borrar valores» deja las tablas sin datos hasta la próxima actualización completa."
                .into(),
        );
    }
    Ok((Value::Object(body), warnings))
}

fn parse_hms(s: &str) -> Option<u64> {
    let p: Vec<&str> = s.trim().split(':').collect();
    let [h, m, sec] = p.as_slice() else {
        return None;
    };
    let (h, m, sec): (u64, u64, u64) = (h.parse().ok()?, m.parse().ok()?, sec.parse().ok()?);
    (m < 60 && sec < 60).then_some(h * 3600 + m * 60 + sec)
}

/// The refresh going on now, if it's one that can be cancelled: its
/// request id (a GUID, checked).
pub(crate) async fn cancellable<S: Service>(s: &S, m: &Model) -> Result<String> {
    let v = s
        .get(&format!("{}/refreshes?$top=1", m.path()), Priority::Editor)
        .await?;
    let runs = runs_from(&v);
    let Some(run) = runs.first().filter(|r| r.running()) else {
        return Err(Error::Query("No hay una actualización en curso.".into()));
    };
    if !run.enhanced() {
        return Err(Error::Query(
            "La actualización en curso no se puede cancelar: solo se cancelan las actualizaciones mejoradas (pedidas por la API con opciones), no las programadas ni las del portal.".into(),
        ));
    }
    run.request_id
        .as_deref()
        .filter(|id| crate::catalog::valid_guid(id))
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| {
            Error::Query("La actualización en curso no tiene un identificador válido.".into())
        })
}

/// Refuses a refresh while another one runs (the service would answer 400).
pub(crate) async fn refuse_if_running<S: Service>(s: &S, m: &Model) -> Result<()> {
    if !m.write_permission {
        return Ok(());
    }
    if let Ok(v) = s
        .get(&format!("{}/refreshes?$top=1", m.path()), Priority::Editor)
        .await
    {
        if runs_from(&v).first().is_some_and(|r| r.running()) {
            return Err(Error::Query(
                "Ya hay una actualización en curso: Power BI actualiza un modelo por vez.".into(),
            ));
        }
    }
    Ok(())
}

pub async fn scheduler_action_script(
    cx: &mut Ctx,
    kind: &str,
    id: &str,
    action: &str,
    values: &FormValues,
) -> Result<String> {
    action_script_in(cx, kind, id, action, values).await
}

pub(crate) async fn action_script_in<S: Service>(
    s: &mut S,
    kind: &str,
    id: &str,
    action: &str,
    values: &FormValues,
) -> Result<String> {
    action_model_kind(kind)?;
    let (_, m) = job_model(s, id).await?;
    match action {
        "run" => {
            let (body, warnings) = run_plan(&m, values)?;
            Ok(comment_script(
                &warnings,
                "POST",
                &api_url(s, &format!("{}/refreshes", m.path())),
                Some(&body),
            ))
        }
        "cancel" => {
            let request = cancellable(s, &m).await?;
            Ok(comment_script(
                &["La actualización se detiene y el modelo queda como estaba antes de empezarla (o con lo confirmado, si es por lotes).".into()],
                "DELETE",
                &api_url(s, &format!("{}/refreshes/{request}", m.path())),
                None,
            ))
        }
        other => Err(Error::Unsupported(format!(
            "«{other}» no es una acción de las programaciones de Power BI."
        ))),
    }
}

pub async fn scheduler_action(
    cx: &mut Ctx,
    kind: &str,
    id: &str,
    action: &str,
    values: &FormValues,
) -> Result<()> {
    action_in(cx, kind, id, action, values).await
}

pub(crate) async fn action_in<S: Service>(
    s: &mut S,
    kind: &str,
    id: &str,
    action: &str,
    values: &FormValues,
) -> Result<()> {
    refuse_on_read_only(s, "actualizar el modelo")?;
    action_model_kind(kind)?;
    let (_, m) = job_model(s, id).await?;
    match action {
        "run" => {
            let (body, _) = run_plan(&m, values)?;
            refuse_if_running(s, &m).await?;
            let r = s
                .send(
                    Method::Post,
                    &format!("{}/refreshes", m.path()),
                    Some(&body),
                    Priority::Editor,
                )
                .await?;
            check_status(r, "Power BI no aceptó la actualización")?;
        }
        "cancel" => {
            let request = cancellable(s, &m).await?;
            let r = s
                .send(
                    Method::Delete,
                    &format!("{}/refreshes/{request}", m.path()),
                    None,
                    Priority::Editor,
                )
                .await?;
            check_status(r, "No se pudo cancelar la actualización")?;
        }
        other => {
            return Err(Error::Unsupported(format!(
                "«{other}» no es una acción de las programaciones de Power BI."
            )))
        }
    }
    forget(s, &m);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{v, DQ_SCHEDULE, HISTORY, HISTORY_RUNNING, SCHEDULE};
    use super::*;
    use crate::config::Settings;
    use crate::meta::fake::{config, Fake};

    fn id(f: &Fake) -> String {
        job_id(Kind::Import, f.model.as_ref().unwrap())
    }

    fn vals(pairs: &[(&str, Value)]) -> FormValues {
        FormValues {
            values: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            lists: BTreeMap::new(),
        }
    }

    fn with_schedule(f: &Fake) {
        let m = f.model.clone().unwrap();
        f.route(
            Method::Get,
            &format!("{}/refreshSchedule", m.path()),
            200,
            v(SCHEDULE),
        );
        f.route(
            Method::Patch,
            &format!("{}/refreshSchedule", m.path()),
            200,
            Value::Null,
        );
        f.route(
            Method::Get,
            &format!("{}/refreshes?$top=2", m.path()),
            200,
            v(HISTORY),
        );
        f.route(
            Method::Get,
            &format!("{}/refreshes?$top=1", m.path()),
            200,
            v(HISTORY),
        );
        f.route(
            Method::Post,
            &format!("{}/refreshes", m.path()),
            202,
            Value::Null,
        );
    }

    #[test]
    fn the_kinds_and_their_actions() {
        let f = Fake::writer();
        let i = info_in(&f);
        assert_eq!(i.kinds[0].key, REFRESH_KIND);
        assert!(i.kinds[0].can_edit && !i.kinds[0].can_create && !i.kinds[0].can_delete);
        assert_eq!(i.kinds[0].actions.len(), 2);
        assert!(i.kinds[0].actions[0].params);
        assert!(!i.logs);
        let f = Fake::new();
        let i = info_in(&f);
        assert!(!i.kinds[0].can_edit && i.kinds[0].actions.is_empty());
        assert!(i.notes.iter().any(|n| n.contains("solo lectura")));
    }

    #[tokio::test]
    async fn the_form_reads_the_schedule() {
        let mut f = Fake::writer();
        with_schedule(&f);
        let jid = self::id(&f);
        let form = form_in(&mut f, REFRESH_KIND, Some(&jid)).await.unwrap();
        assert!(!form.read_only);
        assert_eq!(form.values["enabled"], json!(true));
        assert_eq!(form.values["times"], json!("05:00\n11:30\n17:30\n23:00"));
        assert_eq!(form.values["days"], json!(["Sunday", "Friday", "Saturday"]));
        assert_eq!(form.values["localTimeZoneId"], json!("UTC"));
        assert!(form.notes.iter().any(|n| n.contains("8 por día")));
        // Someone else's model: shown, not editable.
        let mut f = Fake::writer();
        f.model.as_mut().unwrap().configured_by = Some("otra@contoso.com".into());
        f.models[0].configured_by = Some("otra@contoso.com".into());
        with_schedule(&f);
        let jid = self::id(&f);
        let form = form_in(&mut f, REFRESH_KIND, Some(&jid)).await.unwrap();
        assert!(form.read_only);
        // A service principal: no mail.
        let mut f = Fake::writer();
        f.settings = Settings::from_config(&config(
            false,
            &[
                ("auth_mode", "entra_sp"),
                ("tenant", "contoso.onmicrosoft.com"),
                ("client_id", "11111111-2222-3333-4444-555555555555"),
                ("client_secret", "s"),
            ],
        ))
        .unwrap();
        with_schedule(&f);
        let jid = self::id(&f);
        let form = form_in(&mut f, REFRESH_KIND, Some(&jid)).await.unwrap();
        assert_eq!(form.values["notifyOption"], json!("NoNotification"));
        assert!(form_in(&mut f, REFRESH_KIND, None).await.is_err());
    }

    #[tokio::test]
    async fn scripts_warn_first_and_a_disable_goes_alone() {
        let mut f = Fake::writer();
        let jid = self::id(&f);
        let s = script_in(
            &mut f,
            REFRESH_KIND,
            Some(&jid),
            &vals(&[("enabled", json!(false)), ("times", json!("05:00"))]),
        )
        .await
        .unwrap();
        let warnings: Vec<&str> = s.lines().filter_map(|l| l.strip_prefix("// ⚠ ")).collect();
        assert_eq!(warnings.len(), 2, "{s}");
        assert!(warnings[0].contains("propietario"));
        assert!(s.contains("// PATCH https://api.powerbi.com/v1.0/myorg/groups/"));
        let body: String = s
            .lines()
            .filter(|l| !l.starts_with("// ⚠") && !l.starts_with("// PATCH"))
            .map(|l| l.trim_start_matches("//"))
            .collect();
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body, json!({"value": {"enabled": false}}));
        // Enabled: everything, validated.
        let jid = self::id(&f);
        let s = script_in(
            &mut f,
            REFRESH_KIND,
            Some(&jid),
            &vals(&[
                ("enabled", json!(true)),
                ("days", json!(["Monday", "viernes"])),
                ("times", json!("17:30\n5:00")),
                ("localTimeZoneId", json!("Argentina Standard Time")),
                ("notifyOption", json!("MailOnFailure")),
            ]),
        )
        .await
        .unwrap();
        assert!(s.contains("\"05:00\",") && s.contains("\"Friday\""), "{s}");
        assert!(s.lines().all(|l| l.starts_with("//")));
    }

    #[test]
    fn times_are_checked() {
        let mut m = Model::default();
        for (t, ok) in [
            ("05:00", true),
            ("5:30", true),
            ("05:15", false),
            ("24:00", false),
            ("x", false),
            ("", false),
        ] {
            assert_eq!(times(&vals(&[("times", json!(t))]), 8).is_ok(), ok, "{t}");
        }
        let nine: Vec<String> = (0..9).map(|h| format!("{h:02}:00")).collect();
        let v9 = vals(&[("times", json!(nine.join("\n")))]);
        assert!(times(&v9, max_slots(&m)).is_err());
        m.workspace.on_dedicated_capacity = true;
        assert_eq!(times(&v9, max_slots(&m)).unwrap().len(), 9);
        let all: Vec<String> = (0..24)
            .flat_map(|h| [format!("{h:02}:00"), format!("{h:02}:30")])
            .collect();
        assert_eq!(
            times(&vals(&[("times", json!(all))]), 48).unwrap().len(),
            48
        );
    }

    #[tokio::test]
    async fn service_principals_schedule_without_mail() {
        let mut f = Fake::writer();
        f.settings.auth = crate::config::AuthMode::EntraSp;
        let jid = self::id(&f);
        let r = script_in(
            &mut f,
            REFRESH_KIND,
            Some(&jid),
            &vals(&[
                ("enabled", json!(true)),
                ("days", json!(["Monday"])),
                ("times", json!("05:00")),
                ("notifyOption", json!("MailOnFailure")),
            ]),
        )
        .await;
        assert!(r.unwrap_err().to_string().contains("Sin aviso"));
    }

    #[tokio::test]
    async fn saving_patches_and_refusals_send_nothing() {
        let mut f = Fake::writer();
        with_schedule(&f);
        let id = id(&f);
        let v = vals(&[("enabled", json!(false))]);
        assert_eq!(
            save_in(&mut f, REFRESH_KIND, Some(&id), &v).await.unwrap(),
            Some(id.clone())
        );
        let w = f.writes();
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].method, Method::Patch);
        assert_eq!(w[0].body, Some(json!({"value": {"enabled": false}})));
        // Not the owner: refused before any request.
        let mut f = Fake::writer();
        f.model.as_mut().unwrap().configured_by = Some("otra@contoso.com".into());
        f.models[0].configured_by = Some("otra@contoso.com".into());
        let jid = self::id(&f);
        assert!(save_in(&mut f, REFRESH_KIND, Some(&jid), &v).await.is_err());
        assert!(f.requests().is_empty());
        // Ids that aren't in the listing.
        let mut f = Fake::writer();
        let stranger =
            "refresh:f089354e-8366-4e18-aea3-4cb4a3a50b48:00000000-0000-0000-0000-000000000000";
        assert!(save_in(&mut f, REFRESH_KIND, Some(stranger), &v)
            .await
            .is_err());
        assert!(f.requests().is_empty());
    }

    fn id_of(f: &Fake) -> String {
        id(f)
    }

    /// S1: a read-only connection whose token carries
    /// `Dataset.ReadWrite.All` sends nothing for any change.
    #[tokio::test]
    async fn read_only_connections_send_nothing() {
        let mut f = Fake::writer();
        f.set_read_only(true);
        assert!(f
            .account
            .as_ref()
            .unwrap()
            .scopes
            .iter()
            .any(|s| s == "Dataset.ReadWrite.All"));
        with_schedule(&f);
        let id = id(&f);
        let v = vals(&[("enabled", json!(false))]);
        assert!(save_in(&mut f, REFRESH_KIND, Some(&id), &v).await.is_err());
        assert!(action_in(&mut f, REFRESH_KIND, &id, "run", &v)
            .await
            .is_err());
        assert!(action_in(&mut f, REFRESH_KIND, &id, "cancel", &v)
            .await
            .is_err());
        assert!(f.requests().is_empty(), "{:?}", f.requests());
    }

    #[tokio::test]
    async fn run_now_on_shared_and_dedicated_capacity() {
        let mut f = Fake::writer();
        with_schedule(&f);
        let id = id(&f);
        // Shared: the standard refresh, notifyOption only.
        let s = action_script_in(&mut f, REFRESH_KIND, &id, "run", &FormValues::default())
            .await
            .unwrap();
        assert!(
            s.contains("// POST https://api.powerbi.com/v1.0/myorg/groups/")
                && s.contains("8 actualizaciones por día")
        );
        action_in(&mut f, REFRESH_KIND, &id, "run", &FormValues::default())
            .await
            .unwrap();
        let w = f.writes();
        assert_eq!(w[0].body, Some(json!({"notifyOption": "NoNotification"})));
        let form = action_form_in(&mut f, REFRESH_KIND, &id, "run")
            .await
            .unwrap();
        assert!(form.sections.is_empty());
        // Dedicated: the enhanced refresh, checked.
        let mut f = Fake::writer();
        f.model.as_mut().unwrap().workspace.on_dedicated_capacity = true;
        f.models[0].workspace.on_dedicated_capacity = true;
        let m = f.model.clone().unwrap();
        let (body, _) = run_plan(
            &m,
            &vals(&[
                ("type", json!("DataOnly")),
                ("maxParallelism", json!(4)),
                (
                    "objects",
                    json!(["Sales", format!("Customer{PART}Customer-2024")]),
                ),
                ("effectiveDate", json!("2026-01-31")),
            ]),
        )
        .unwrap();
        assert_eq!(body["type"], "DataOnly");
        assert_eq!(
            body["objects"],
            json!([{"table": "Sales"}, {"table": "Customer", "partition": "Customer-2024"}])
        );
        assert!(body.get("notifyOption").is_none());
        for bad in [
            vals(&[("type", json!("Everything"))]),
            vals(&[("maxParallelism", json!(0))]),
            vals(&[("retryCount", json!(11))]),
            vals(&[("timeout", json!("25:00:00"))]),
            vals(&[("timeout", json!("1h"))]),
            vals(&[("effectiveDate", json!("31/01/2026"))]),
        ] {
            assert!(run_plan(&m, &bad).is_err(), "{bad:?}");
        }
        let form = action_form_in(&mut f, REFRESH_KIND, &job_id(Kind::Import, &m), "run")
            .await
            .unwrap();
        assert!(form.sections[0]
            .fields
            .iter()
            .any(|x| x.key == "commitMode"));
        // The incremental-refresh fields show only for a transactional one.
        let policy = form.sections[0]
            .fields
            .iter()
            .find(|x| x.key == "applyRefreshPolicy")
            .unwrap();
        assert_eq!(
            policy.when,
            Some(("commitMode".into(), vec!["transactional".into()]))
        );
        assert_eq!(body["applyRefreshPolicy"], true);
        assert_eq!(body["effectiveDate"], "2026-01-31");
    }

    #[tokio::test]
    async fn a_batched_refresh_never_sends_the_refresh_policy() {
        let mut f = Fake::writer();
        f.model.as_mut().unwrap().workspace.on_dedicated_capacity = true;
        f.models[0].workspace.on_dedicated_capacity = true;
        let m = f.model.clone().unwrap();
        // Learn: with partialBatch, applyRefreshPolicy must be false, even
        // when the (hidden) box still says true.
        let values = vals(&[
            ("commitMode", json!("partialBatch")),
            ("applyRefreshPolicy", json!(true)),
            ("effectiveDate", json!("2026-01-31")),
        ]);
        let (body, warnings) = run_plan(&m, &values).unwrap();
        assert_eq!(body["commitMode"], "partialBatch");
        assert_eq!(body["applyRefreshPolicy"], false);
        assert!(body.get("effectiveDate").is_none(), "{body}");
        assert!(warnings.iter().any(|w| w.contains("applyRefreshPolicy")));
        // The preview says it, and what is sent matches it.
        let id = job_id(Kind::Import, &m);
        let script = action_script_in(&mut f, REFRESH_KIND, &id, "run", &values)
            .await
            .unwrap();
        assert!(
            script.contains("// ⚠") && script.contains("\"applyRefreshPolicy\": false"),
            "{script}"
        );
        with_schedule(&f);
        action_in(&mut f, REFRESH_KIND, &id, "run", &values)
            .await
            .unwrap();
        let w = f.writes();
        let sent = w.last().unwrap().body.as_ref().unwrap();
        assert_eq!(sent["applyRefreshPolicy"], false);
    }

    #[tokio::test]
    async fn one_refresh_at_a_time_and_only_enhanced_ones_cancel() {
        let mut f = Fake::writer();
        let m = f.model.clone().unwrap();
        f.route(
            Method::Get,
            &format!("{}/refreshes?$top=1", m.path()),
            200,
            v(HISTORY_RUNNING),
        );
        let id = id(&f);
        let e = action_in(&mut f, REFRESH_KIND, &id, "run", &FormValues::default())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("en curso"));
        let e = action_in(&mut f, REFRESH_KIND, &id, "cancel", &FormValues::default())
            .await
            .unwrap_err();
        assert!(e.to_string().contains("mejoradas"));
        assert!(f.writes().is_empty());
        // An enhanced refresh going on: DELETE with its request id.
        let mut f = Fake::writer();
        let m = f.model.clone().unwrap();
        let running = json!({"value": [{"refreshType": "ViaEnhancedApi", "startTime": "2021-12-14T03:46:04.833", "status": "Unknown", "requestId": "630110f5-8f93-49c2-afee-bfc87072460c"}]});
        f.route(
            Method::Get,
            &format!("{}/refreshes?$top=1", m.path()),
            200,
            running,
        );
        f.route(
            Method::Delete,
            &format!(
                "{}/refreshes/630110f5-8f93-49c2-afee-bfc87072460c",
                m.path()
            ),
            200,
            Value::Null,
        );
        let id = id_of(&f);
        let s = action_script_in(&mut f, REFRESH_KIND, &id, "cancel", &FormValues::default())
            .await
            .unwrap();
        assert!(
            s.contains("// DELETE https://api.powerbi.com/v1.0/myorg/groups/")
                && s.ends_with("/refreshes/630110f5-8f93-49c2-afee-bfc87072460c\n")
        );
        action_in(&mut f, REFRESH_KIND, &id, "cancel", &FormValues::default())
            .await
            .unwrap();
        assert_eq!(f.writes()[0].method, Method::Delete);
    }

    #[tokio::test]
    async fn items_and_the_direct_query_cache() {
        let mut f = Fake::writer();
        let mut dq = f.models[0].clone();
        dq.id = "9b2c1a77-0000-4000-8000-000000000001".into();
        dq.name = "Inventario".into();
        dq.is_refreshable = Some(false);
        f.models.push(dq.clone());
        with_schedule(&f);
        f.route(
            Method::Get,
            &format!("{}/directQueryRefreshSchedule", dq.path()),
            200,
            v(DQ_SCHEDULE),
        );
        let l = items_in(&mut f, REFRESH_KIND).await.unwrap();
        assert_eq!(l.items.len(), 1);
        assert_eq!(l.items[0].enabled, Some(true));
        assert_eq!(
            l.items[0].actions.as_deref(),
            Some(&["run".to_string()][..])
        );
        assert!(l.items[0].description.as_deref().unwrap().contains("05:00"));
        let l = items_in(&mut f, DQ_KIND).await.unwrap();
        assert_eq!(l.items.len(), 1);
        assert_eq!(l.items[0].name, "Inventario");
        let dq_id = l.items[0].id.clone();
        assert!(dq_id.starts_with("dqrefresh:"));
        let s = script_in(
            &mut f,
            DQ_KIND,
            Some(&dq_id),
            &vals(&[("mode", json!("frequency")), ("frequency", json!("30"))]),
        )
        .await
        .unwrap();
        assert!(s.contains("directQueryRefreshSchedule") && s.contains("\"frequency\": 30"));
        assert!(script_in(
            &mut f,
            DQ_KIND,
            Some(&dq_id),
            &vals(&[("frequency", json!("45"))])
        )
        .await
        .is_err());
        assert!(action_form_in(&mut f, DQ_KIND, &dq_id, "run")
            .await
            .is_err());
    }
}
