//! "Ejecutar para fechas pasadas": a scheduled query run again for past
//! dates, as the console's "Schedule backfill" does, with the service's
//! `startManualRuns`:
//!
//! - For a period: `requestedTimeRange`, whole days in UTC (the service
//!   takes a scheduled query's range by the day: from the first day's
//!   midnight to the midnight after the last). It makes a run for each of
//!   the schedule's times in those days, each with its own @run_time and
//!   @run_date; the first starts at once and the rest 15 seconds apart.
//!   Only a query that runs on a schedule has times to repeat: one that
//!   runs on demand or on Pub/Sub messages is offered the next choice
//!   only, and a period for it is refused.
//! - Once, for a date and time: `requestedRunTime`, which becomes the
//!   run's @run_time.
//! - The service runs only the past: a day that hasn't ended, or a time
//!   ahead of now, is refused before the request.
//! - The script's "⚠" lines say what the runs do: each is billed as a
//!   query, and appends to its destination, replaces it, or (without one)
//!   makes its DDL or DML changes again.
//! - It needs `bigquery.transfers.update`, like any change: without it the
//!   form is read-only and the kind has no such action.

use super::{bad, kind, may_change, one, read, send, time_value, Current, Mode, Plan, Req, Vals};
use crate::jobs::{checked, text, Dts};
use crate::{permissions, BigQuerySession, Json};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use dbine_driver::{Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result};
use reqwest::Method;
use serde_json::json;

/// The action's key.
pub(crate) const ACTION: &str = "backfill";

/// A day as the form and the script write it.
const DAY: &str = "%Y-%m-%d";

const READ_ONLY: &str = "Esta cuenta no puede ejecutar consultas programadas para fechas pasadas (hace falta el permiso bigquery.transfers.update en el proyecto).";
const IN_UTC: &str = "Los días y las horas van en UTC, como la programación de la consulta.";
const NO_SCHEDULE: &str = "Esta consulta programada no corre según una programación (corre a pedido o al llegar mensajes de Pub/Sub): no tiene horas que repetir en un período, así que se ejecuta una vez, para una fecha y hora.";
const RANGE_REFUSED: &str = "Esta consulta programada no corre según una programación: no se puede ejecutar para un período. Elegí ejecutarla una vez, para una fecha y hora.";
const NOT_ENDED: &str = "«Hasta» puede ser como mucho ayer: el servicio no ejecuta un día que todavía no terminó (para hoy, ejecutala una vez, para una fecha y hora).";
const BACKWARDS: &str = "«Desde» no puede ser posterior a «Hasta».";
const FUTURE: &str =
    "La fecha y hora de la ejecución no puede ser futura: el servicio solo ejecuta el pasado.";
const NEED_TIME: &str = "Elegí la fecha y hora de la ejecución.";
const SPACED: &str = "El servicio empieza la primera ejecución ahora y cada una de las siguientes 15 segundos después de la anterior.";
const NONE_MADE: &str = "El servicio no creó ninguna ejecución: la programación de la consulta no tiene horas en esos días (o empieza después).";

// What the script warns about ("⚠" lines: the app lists them and asks to
// continue), each a whole sentence of its own for the message catalog.

/// Every run is a query, billed.
pub(super) const BILLED: &str =
    "Cada ejecución corre la consulta y se factura como cualquier consulta de BigQuery.";
/// A destination written with WRITE_TRUNCATE.
pub(super) const REPLACES: &str = "Cada ejecución reemplaza los datos de su tabla de destino: si el nombre de la tabla no lleva la fecha, queda solo el resultado de la última.";
/// A destination written with WRITE_APPEND.
pub(super) const APPENDS: &str = "Cada ejecución agrega sus filas a la tabla de destino: las de los días que ya habían corrido pueden quedar repetidas.";
/// No destination: DDL, DML or a script.
pub(super) const CHANGES: &str =
    "Sin tabla de destino, la consulta es DDL, DML o un script: cada ejecución vuelve a hacer sus cambios.";

/// The kind and the action are the ones this module serves.
fn check(k: &str, a: &str) -> Result<()> {
    kind(k)?;
    if a.trim() == ACTION {
        Ok(())
    } else {
        Err(Error::Unsupported(format!(
            "las consultas programadas de BigQuery no tienen la acción «{}»",
            one(a)
        )))
    }
}

/// A day field (`YYYY-MM-DD`).
fn day(v: Vals, k: &str, label: &str) -> Result<NaiveDate> {
    let t = v
        .opt(k)
        .ok_or_else(|| bad(format!("Elegí el día «{label}».")))?;
    NaiveDate::parse_from_str(&t, DAY).map_err(|_| {
        bad(format!(
            "«{label}»: «{}» no es una fecha (AAAA-MM-DD).",
            one(&t)
        ))
    })
}

/// What the user asked for.
#[derive(Debug, Clone, PartialEq)]
enum Wanted {
    /// The days from the first to the last, both included.
    Days(NaiveDate, NaiveDate),
    /// One run for this time (RFC 3339, UTC).
    Once(String),
}

/// The form's values, checked against the config and the clock.
fn wanted(cur: &Current, v: Vals, now: DateTime<Utc>) -> Result<Wanted> {
    let schedules = cur.spec.mode == Mode::Schedule;
    let mode = v
        .opt("mode")
        .unwrap_or_else(|| if schedules { "range" } else { "once" }.into());
    match mode.as_str() {
        "range" => {
            if !schedules {
                return Err(bad(RANGE_REFUSED.into()));
            }
            let start = day(v, "start_date", "Desde")?;
            let end = day(v, "end_date", "Hasta")?;
            if start > end {
                return Err(bad(BACKWARDS.into()));
            }
            if end >= now.date_naive() {
                return Err(bad(NOT_ENDED.into()));
            }
            Ok(Wanted::Days(start, end))
        }
        "once" => {
            let t =
                time_value(v, "run_time", "Fecha y hora")?.ok_or_else(|| bad(NEED_TIME.into()))?;
            let at = DateTime::parse_from_rfc3339(&t)
                .map_err(|_| bad(NEED_TIME.into()))?
                .with_timezone(&Utc);
            if at > now {
                return Err(bad(FUTURE.into()));
            }
            Ok(Wanted::Once(t))
        }
        other => Err(bad(format!(
            "«{}» no es una forma de ejecutarla (range u once).",
            one(other)
        ))),
    }
}

/// A day's midnight, RFC 3339 in UTC.
fn midnight(d: NaiveDate) -> String {
    format!("{}T00:00:00Z", d.format(DAY))
}

/// The request for `values`: one `startManualRuns`.
fn plan_for(dts: &Dts<'_>, cur: &Current, v: Vals, now: DateTime<Utc>) -> Result<Plan> {
    let parts = checked(&cur.name)?;
    let last = format!("{}:startManualRuns", parts[2]);
    let mut segments = Dts::config_segments(&parts);
    segments[5] = &last;
    let url = dts.url(&segments)?;
    let name = one(&cur.spec.name);
    let (lines, body) = match wanted(cur, v, now)? {
        Wanted::Days(start, end) => {
            let days = (end - start).num_days() + 1;
            let (from, to) = (start.format(DAY), end.format(DAY));
            (
                vec![
                    if days == 1 {
                        format!("Ejecuta la consulta programada «{name}» de BigQuery para el día {from} (en UTC): una ejecución por cada hora de su programación en ese día, con su @run_time y su @run_date.")
                    } else {
                        format!("Ejecuta la consulta programada «{name}» de BigQuery para los días del {from} al {to} ({days} días, en UTC): una ejecución por cada hora de su programación en esos días, con su @run_time y su @run_date.")
                    },
                    SPACED.to_string(),
                ],
                json!({ "requestedTimeRange": {
                    "startTime": midnight(start),
                    "endTime": midnight(end + Duration::days(1)),
                } }),
            )
        }
        Wanted::Once(t) => (
            vec![format!(
                "Ejecuta la consulta programada «{name}» de BigQuery una vez, como si fuera {t}: esa es su @run_time."
            )],
            json!({ "requestedRunTime": t }),
        ),
    };
    let what = match (&cur.spec.dataset, cur.spec.write.as_deref()) {
        (None, _) => CHANGES,
        (Some(_), Some("WRITE_TRUNCATE")) => REPLACES,
        (Some(_), _) => APPENDS,
    };
    Ok(Plan {
        lines,
        reqs: vec![Req {
            method: Method::POST,
            url,
            body: Some(body),
        }],
        id: Some(cur.name.clone()),
        warnings: vec![BILLED.to_string(), what.to_string()],
    })
}

/// The action's form for the config, with yesterday as the period and, for
/// one run, the time of its next run a day earlier (or today's midnight).
fn form_for(cur: &Current, now: DateTime<Utc>) -> ObjectForm {
    let schedules = cur.spec.mode == Mode::Schedule;
    let yesterday = (now - Duration::days(1)).date_naive();
    let run_time = text(&cur.json, "/nextRunTime")
        .and_then(|t| DateTime::parse_from_rfc3339(&t).ok())
        .map(|t| t.with_timezone(&Utc) - Duration::days(1))
        .filter(|t| *t <= now)
        .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| midnight(now.date_naive()));
    let mode = if schedules {
        FormKind::select(&[
            (
                "range",
                "Para un período: cada ejecución programada de esos días",
            ),
            ("once", "Una vez, para una fecha y hora"),
        ])
    } else {
        FormKind::select(&[("once", "Una vez, para una fecha y hora")])
    };
    let mut fields = vec![
        FormField::new("mode", "Ejecutar", mode)
            .required()
            .default(json!(if schedules { "range" } else { "once" })),
        FormField::new("start_date", "Desde", FormKind::Date)
            .required()
            .when("mode", &["range"])
            .help("El primer día, incluido, en UTC."),
        FormField::new("end_date", "Hasta", FormKind::Date)
            .required()
            .when("mode", &["range"])
            .help("El último día, incluido, en UTC. Como mucho ayer: el servicio no ejecuta un día que no terminó."),
        FormField::new("run_time", "Fecha y hora", FormKind::DateTime)
            .required()
            .when("mode", &["once"])
            .help("En UTC. La consulta la recibe como @run_time, y su día como @run_date. No puede ser futura."),
    ];
    let mut f = ObjectForm::default();
    f.values.insert(
        "mode".into(),
        json!(if schedules { "range" } else { "once" }),
    );
    f.values.insert(
        "start_date".into(),
        json!(yesterday.format(DAY).to_string()),
    );
    f.values
        .insert("end_date".into(), json!(yesterday.format(DAY).to_string()));
    f.values.insert("run_time".into(), json!(run_time));
    if schedules {
        f.values.insert(
            "schedule".into(),
            json!(cur.spec.schedule.clone().unwrap_or_default()),
        );
        fields.push(
            FormField::new("schedule", "Programación", FormKind::Text)
                .read_only(true)
                .when("mode", &["range"])
                .help("En UTC: cada día del período corre a estas horas."),
        );
    }
    f.sections = vec![FormSection::new("backfill", "Ejecución").fields(fields)];
    f.notes.push(IN_UTC.into());
    if !schedules {
        f.notes.push(NO_SCHEDULE.into());
    }
    f
}

pub(crate) async fn action_form(
    s: &BigQuerySession,
    k: &str,
    id: &str,
    a: &str,
) -> Result<ObjectForm> {
    check(k, a)?;
    let dts = s.dts()?;
    let cur = read(&dts, id).await?;
    let mut f = form_for(&cur, Utc::now());
    if may_change(&s.api, permissions::CRM).await == Some(false) {
        f.read_only = true;
        f.notes.insert(0, READ_ONLY.into());
    }
    Ok(f)
}

pub(crate) async fn action_script(
    s: &BigQuerySession,
    k: &str,
    id: &str,
    a: &str,
    values: &FormValues,
) -> Result<String> {
    check(k, a)?;
    let dts = s.dts()?;
    Ok(plan_with(&dts, id, values).await?.script())
}

pub(crate) async fn action(
    s: &BigQuerySession,
    k: &str,
    id: &str,
    a: &str,
    values: &FormValues,
) -> Result<()> {
    check(k, a)?;
    let dts = s.dts()?;
    run_with(&dts, id, values).await.map(|_| ())
}

/// The config `id` (a scheduled query) read now, and its request.
async fn plan_with(dts: &Dts<'_>, id: &str, values: &FormValues) -> Result<Plan> {
    let cur = read(dts, id).await?;
    plan_for(dts, &cur, Vals(&values.values), Utc::now())
}

/// Sends the request; the runs the service made.
async fn run_with(dts: &Dts<'_>, id: &str, values: &FormValues) -> Result<usize> {
    let p = plan_with(dts, id, values).await?;
    let answer = send(dts, &p).await?.unwrap_or(Json::Null);
    // The answer lists the runs made (an empty list is left out).
    let made = answer
        .get("runs")
        .and_then(Json::as_array)
        .map_or(0, Vec::len);
    if made == 0 {
        return Err(bad(NONE_MADE.into()));
    }
    Ok(made)
}

#[cfg(test)]
mod tests;
