//! Refreshes as server jobs (spec §4.6 `jobs`…`job_script`). Owned by part
//! C.
//!
//! - `jobs`: the refreshable models of the session's workspace as
//!   `ServerJob { id: "refresh:<g>:<d>", kind: "Actualización programada",
//!   schedule: "lun, mié, vie · 05:00, 17:30 · <time zone>", … }`, DQ cache
//!   schedules as `dqrefresh:<g>:<d>`; `can_run` off on read-only
//!   connections, `can_stop` only while an enhanced refresh runs,
//!   `can_enable` only for the owner (`configuredBy` = the signed-in UPN),
//!   `has_history` only with write permission; schedules fetched with
//!   concurrency 4; `notes` say what limits the list.
//! - `job_runs`: `GET …/refreshes?$top=min(limit, 500)`; `trigger` from
//!   `refreshType`, `message` from `serviceExceptionJson`, `steps` from
//!   `refreshAttempts`.
//! - `run_job`: `POST …/refreshes {"notifyOption":"NoNotification"}`;
//!   `stop_job`: `DELETE …/refreshes/{requestId}` (enhanced refreshes);
//!   `set_job_enabled`: `PATCH …/refreshSchedule {"value":{"enabled":…}}`
//!   alone; `job_script`: the comment script of that PATCH.

use super::scheduler::{cancellable, plan, refuse_if_not_owner, refuse_if_running};
use super::{
    api_url, comment_script, exception_text, forget, job_id, job_model, runs_from, schedule_path,
    survey, survey_one, utc, Kind, ModelRefresh, Schedule, DAYS,
};
use crate::http::{Method, Priority};
use crate::meta::service::{check_status, is_owner, refuse_on_read_only, Service};
use crate::Ctx;
use dbine_driver::jobs::MAX_JOB_RUNS;
use dbine_driver::{
    Error, FormValues, JobDetail, JobList, JobOutcome, JobRun, JobStep, JobStepRun, PropertyInfo,
    Result, ServerJob,
};
use serde_json::{json, Value};

pub async fn jobs(cx: &mut Ctx) -> Result<JobList> {
    jobs_in(cx).await
}

fn server_job<S: Service>(s: &S, r: &ModelRefresh) -> ServerJob {
    let read_only = s.settings().read_only;
    let m = &r.model;
    let running = r.running();
    let last = r.last_finished();
    let mut job = ServerJob {
        id: job_id(r.kind, m),
        name: m.name.clone(),
        kind: r.kind.label().into(),
        group: Some(m.workspace.name.clone()),
        description: None,
        owner: m.configured_by.clone(),
        enabled: r.schedule.as_ref().is_some_and(|x| x.fires(r.kind)),
        schedule: r.schedule.as_ref().and_then(Schedule::summary),
        running: running.is_some(),
        last_outcome: match (running, last) {
            (_, Some(l)) => Some(l.outcome()),
            (Some(_), None) => Some(JobOutcome::Running),
            (None, None) => None,
        },
        last_started: r
            .recent
            .first()
            .and_then(|x| x.start.as_deref())
            .and_then(utc),
        last_duration_ms: last.and_then(|l| l.duration_ms()),
        next_run: None,
        has_script: true,
        ..Default::default()
    };
    if r.kind == Kind::Import {
        job.can_run = !read_only && m.write_permission && running.is_none();
        job.can_stop = !read_only && running.is_some_and(|x| x.enhanced());
        job.can_enable = !read_only && is_owner(s, m) == Some(true);
        job.has_history = m.write_permission;
    }
    job
}

pub(crate) async fn jobs_in<S: Service>(s: &mut S) -> Result<JobList> {
    let survey = survey(s).await?;
    let mut list = JobList {
        notes: survey.notes,
        ..Default::default()
    };
    let mut no_write = 0;
    let mut unreadable = 0;
    for r in &survey.models {
        if r.kind == Kind::Import && !r.model.write_permission {
            no_write += 1;
        }
        if r.schedule.is_none() {
            unreadable += 1;
        }
        list.jobs.push(server_job(s, r));
    }
    if s.settings().read_only {
        list.notes.push(super::scheduler::read_only_note());
    }
    if no_write > 0 {
        list.notes.push(format!(
            "{no_write} modelos sin permiso de escritura: su historial no se ve y no se pueden actualizar desde DBine."
        ));
    }
    if unreadable > 0 {
        list.notes.push(format!(
            "De {unreadable} modelos no se pudo leer la programación (permisos, o modelos que no se actualizan)."
        ));
    }
    Ok(list)
}

pub async fn job_detail(cx: &mut Ctx, id: &str) -> Result<JobDetail> {
    detail_in(cx, id).await
}

fn notify_text(n: &str) -> &str {
    match n {
        "MailOnFailure" => "correo si falla",
        "NoNotification" => "sin aviso",
        other => other,
    }
}

pub(crate) async fn detail_in<S: Service>(s: &mut S, id: &str) -> Result<JobDetail> {
    let (kind, m) = job_model(s, id).await?;
    super::forget(s, &m);
    let r = survey_one(s, &m).await.ok_or_else(|| {
        Error::Query("Ese modelo no tiene una programación de caché de DirectQuery.".into())
    })?;
    if r.kind != kind {
        return Err(Error::Query(
            "Ese trabajo es de otro tipo de programación.".into(),
        ));
    }
    let mut props = vec![PropertyInfo {
        group: String::new(),
        label: "Área de trabajo".into(),
        value: m.workspace.name.clone(),
    }];
    let mut prop = |label: &str, value: String| {
        props.push(PropertyInfo {
            group: String::new(),
            label: label.into(),
            value,
        })
    };
    if let Some(sch) = &r.schedule {
        if let Some(f) = sch.frequency {
            prop("Frecuencia", format!("cada {f} minutos"));
        } else {
            let days: Vec<&str> = DAYS
                .iter()
                .filter(|(en, _, _)| sch.days.iter().any(|d| d.eq_ignore_ascii_case(en)))
                .map(|(_, es, _)| *es)
                .collect();
            if !days.is_empty() {
                prop("Días", days.join(", "));
            }
            if !sch.times.is_empty() {
                prop("Horarios", sch.times.join(", "));
            }
        }
        if let Some(tz) = &sch.time_zone {
            prop("Zona horaria", tz.clone());
        }
        if let Some(n) = &sch.notify {
            prop("Aviso", notify_text(n).into());
        }
    }
    prop(
        "Capacidad",
        if m.workspace.on_dedicated_capacity {
            "Premium o Fabric (dedicada)".into()
        } else {
            "compartida (Pro)".into()
        },
    );
    prop(
        "Horarios por día",
        format!("hasta {}", super::scheduler::max_slots(&m)),
    );
    if let Some(o) = &m.configured_by {
        prop("Propietario", o.clone());
    }
    let mut notes = vec![match &m.configured_by {
        Some(o) => format!("Solo el propietario del modelo ({o}) puede cambiar su programación."),
        None => "Solo el propietario del modelo puede cambiar su programación.".into(),
    }];
    if kind == Kind::Import {
        if !m.workspace.on_dedicated_capacity {
            notes.push("En capacidad compartida el modelo se actualiza como máximo 8 veces por día, contando las programadas.".into());
        }
        notes.push("Solo se pueden detener las actualizaciones mejoradas (pedidas por la API con opciones), no las programadas ni las del portal.".into());
        if !m.write_permission {
            notes.push("Sin permiso de escritura en el modelo no se ve su historial.".into());
        }
    } else {
        notes.push("La caché de DirectQuery se actualiza sola según esta programación; no tiene historial en la API.".into());
    }
    Ok(JobDetail {
        job: server_job(s, &r),
        steps: vec![JobStep {
            name: match kind {
                Kind::Import => "Actualizar el modelo".into(),
                Kind::DirectQuery => "Actualizar la caché".into(),
            },
            kind: Some("Power BI".into()),
            target: Some(m.name.clone()),
            command: None,
        }],
        schedules: Vec::new(),
        properties: props,
        notes,
    })
}

pub async fn job_runs(cx: &mut Ctx, id: &str, limit: u32) -> Result<Vec<JobRun>> {
    runs_in(cx, id, limit).await
}

pub(crate) async fn runs_in<S: Service>(s: &mut S, id: &str, limit: u32) -> Result<Vec<JobRun>> {
    let (kind, m) = job_model(s, id).await?;
    if kind == Kind::DirectQuery {
        return Ok(Vec::new());
    }
    let top = limit.clamp(1, MAX_JOB_RUNS);
    let v = s
        .get(
            &format!("{}/refreshes?$top={top}", m.path()),
            Priority::Editor,
        )
        .await?;
    Ok(runs_from(&v)
        .into_iter()
        .map(|r| JobRun {
            id: r.request_id.clone(),
            started: r.start.as_deref().and_then(utc),
            duration_ms: r.duration_ms(),
            outcome: r.outcome(),
            trigger: r.trigger(),
            message: r.exception.as_deref().map(|e| exception_text(e, true)),
            steps: r
                .attempts
                .iter()
                .map(|a| JobStepRun {
                    name: format!(
                        "Intento {} · {}",
                        a.id.unwrap_or(1),
                        match a.kind.as_deref() {
                            Some("Data") => "datos",
                            Some("Query") => "caché de consultas",
                            Some(other) => other,
                            None => "actualización",
                        }
                    ),
                    outcome: if a.exception.is_some() {
                        JobOutcome::Failed
                    } else if a.end.is_none() {
                        JobOutcome::Running
                    } else {
                        JobOutcome::Succeeded
                    },
                    started: a.start.as_deref().and_then(utc),
                    duration_ms: super::millis_of(a.start.as_deref(), a.end.as_deref()),
                    message: a.exception.as_deref().map(|e| exception_text(e, true)),
                })
                .collect(),
        })
        .collect())
}

pub async fn run_job(cx: &mut Ctx, id: &str) -> Result<()> {
    run_in(cx, id).await
}

pub(crate) async fn run_in<S: Service>(s: &mut S, id: &str) -> Result<()> {
    refuse_on_read_only(s, "actualizar el modelo")?;
    let (kind, m) = job_model(s, id).await?;
    if kind == Kind::DirectQuery {
        return Err(Error::Unsupported(
            "La caché de DirectQuery se actualiza sola según su programación: la API no la actualiza a pedido.".into(),
        ));
    }
    refuse_if_running(s, &m).await?;
    let r = s
        .send(
            Method::Post,
            &format!("{}/refreshes", m.path()),
            Some(&json!({"notifyOption": "NoNotification"})),
            Priority::Editor,
        )
        .await?;
    check_status(r, "Power BI no aceptó la actualización")?;
    forget(s, &m);
    Ok(())
}

pub async fn stop_job(cx: &mut Ctx, id: &str) -> Result<()> {
    stop_in(cx, id).await
}

pub(crate) async fn stop_in<S: Service>(s: &mut S, id: &str) -> Result<()> {
    refuse_on_read_only(s, "cancelar una actualización")?;
    let (kind, m) = job_model(s, id).await?;
    if kind == Kind::DirectQuery {
        return Err(Error::Unsupported(
            "La actualización de la caché de DirectQuery no se cancela.".into(),
        ));
    }
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
    forget(s, &m);
    Ok(())
}

pub async fn set_job_enabled(cx: &mut Ctx, id: &str, enabled: bool) -> Result<()> {
    enable_in(cx, id, enabled).await
}

pub(crate) async fn enable_in<S: Service>(s: &mut S, id: &str, enabled: bool) -> Result<()> {
    refuse_on_read_only(s, "cambiar la programación")?;
    let (kind, m) = job_model(s, id).await?;
    if kind == Kind::DirectQuery {
        return Err(Error::Unsupported(
            "La programación de la caché de DirectQuery no se activa ni se desactiva: se cambia su frecuencia en el programador.".into(),
        ));
    }
    refuse_if_not_owner(s, &m)?;
    // Power BI takes `enabled` alone in the request that changes it.
    let r = s
        .send(
            Method::Patch,
            &schedule_path(kind, &m),
            Some(&json!({"value": {"enabled": enabled}})),
            Priority::Editor,
        )
        .await?;
    check_status(r, "No se pudo cambiar la programación")?;
    forget(s, &m);
    Ok(())
}

pub async fn job_script(cx: &mut Ctx, id: &str) -> Result<String> {
    script_in(cx, id).await
}

/// The PATCH that sets the schedule as it is now.
pub(crate) async fn script_in<S: Service>(s: &mut S, id: &str) -> Result<String> {
    let (kind, m) = job_model(s, id).await?;
    let current = Schedule::from_json(&s.get(&schedule_path(kind, &m), Priority::Editor).await?);
    let mut values = FormValues::default();
    let body = current.body(kind);
    if let Some(Value::Object(o)) = body.get("value") {
        for (k, v) in o {
            values.values.insert(k.clone(), v.clone());
        }
    }
    if let Some(f) = current.frequency {
        values.values.insert("mode".into(), json!("frequency"));
        values
            .values
            .insert("frequency".into(), json!(f.to_string()));
    } else if kind == Kind::DirectQuery {
        values.values.insert("mode".into(), json!("times"));
    }
    // A schedule that doesn't validate (no times yet) is shown as it is.
    let (body, warnings) = plan(s, kind, &m, &values).unwrap_or((body, Vec::new()));
    Ok(comment_script(
        &warnings,
        "PATCH",
        &api_url(s, &schedule_path(kind, &m)),
        Some(&body),
    ))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{v, HISTORY, HISTORY_FAILED, HISTORY_RUNNING, SCHEDULE};
    use super::*;
    use crate::meta::fake::Fake;

    fn routes(f: &Fake, history: &str) {
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
            v(history),
        );
        f.route(
            Method::Get,
            &format!("{}/refreshes?$top=1", m.path()),
            200,
            v(history),
        );
        f.route(
            Method::Get,
            &format!("{}/refreshes?$top=50", m.path()),
            200,
            v(history),
        );
        f.route(
            Method::Post,
            &format!("{}/refreshes", m.path()),
            202,
            Value::Null,
        );
    }

    fn id(f: &Fake) -> String {
        job_id(Kind::Import, f.model.as_ref().unwrap())
    }

    #[tokio::test]
    async fn the_list_says_what_each_job_allows() {
        let mut f = Fake::writer();
        routes(&f, HISTORY_FAILED);
        let l = jobs_in(&mut f).await.unwrap();
        let j = &l.jobs[0];
        assert_eq!(j.kind, "Actualización programada");
        assert_eq!(j.group.as_deref(), Some("Finanzas"));
        assert_eq!(
            j.schedule.as_deref(),
            Some("dom, vie, sáb · 05:00, 11:30, 17:30, 23:00 · UTC")
        );
        assert!(j.enabled && j.can_run && j.can_enable && j.has_history && j.has_script);
        assert!(!j.can_stop && !j.running);
        assert_eq!(j.last_outcome, Some(JobOutcome::Failed));
        assert_eq!(j.last_started.as_deref(), Some("2017-06-13T09:25:43.153Z"));
        assert_eq!(j.last_duration_ms, Some(360_000));
        assert!(l.notes.is_empty(), "{:?}", l.notes);
        // A viewer on a read-only connection: no history, nothing to do.
        let mut f = Fake::new();
        let m = f.model.clone().unwrap();
        f.route(
            Method::Get,
            &format!("{}/refreshSchedule", m.path()),
            200,
            v(SCHEDULE),
        );
        let l = jobs_in(&mut f).await.unwrap();
        let j = &l.jobs[0];
        assert!(!j.can_run && !j.can_enable && !j.has_history && !j.can_stop);
        assert_eq!(l.notes.len(), 2, "{:?}", l.notes);
    }

    #[tokio::test]
    async fn a_running_refresh_shows_and_blocks_another() {
        let mut f = Fake::writer();
        routes(&f, HISTORY_RUNNING);
        let l = jobs_in(&mut f).await.unwrap();
        let j = &l.jobs[0];
        assert!(
            j.running && !j.can_run && !j.can_stop,
            "a portal/API refresh can't be stopped"
        );
        assert_eq!(j.last_outcome, Some(JobOutcome::Running));
        let jid = self::id(&f);
        assert!(run_in(&mut f, &jid).await.is_err());
        let jid = self::id(&f);
        assert!(stop_in(&mut f, &jid).await.is_err());
        assert!(f.writes().is_empty());
    }

    #[tokio::test]
    async fn runs_read_learns_history() {
        let mut f = Fake::writer();
        routes(&f, HISTORY_FAILED);
        let jid = self::id(&f);
        let runs = runs_in(&mut f, &jid, 50).await.unwrap();
        assert_eq!(runs[0].outcome, JobOutcome::Failed);
        assert_eq!(runs[0].trigger.as_deref(), Some("API"));
        assert_eq!(
            runs[0].message.as_deref(),
            Some("ModelRefreshFailed_CredentialsNotSpecified")
        );
        assert_eq!(runs[0].steps[0].name, "Intento 1 · datos");
        assert_eq!(runs[0].steps[0].outcome, JobOutcome::Failed);
        assert_eq!(
            runs[0].id.as_deref(),
            Some("11bf290a-346b-48b7-8973-c5df149337ff")
        );
    }

    #[tokio::test]
    async fn run_enable_and_script() {
        let mut f = Fake::writer();
        routes(&f, HISTORY);
        let id = id(&f);
        run_in(&mut f, &id).await.unwrap();
        enable_in(&mut f, &id, false).await.unwrap();
        let w = f.writes();
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].body, Some(json!({"notifyOption": "NoNotification"})));
        assert_eq!(w[1].method, Method::Patch);
        assert_eq!(w[1].body, Some(json!({"value": {"enabled": false}})));
        let s = script_in(&mut f, &id).await.unwrap();
        assert!(
            s.starts_with("// ⚠ Solo el propietario del modelo (john@contoso.com)"),
            "{s}"
        );
        assert!(s.contains("\"localTimeZoneId\": \"UTC\""));
        let d = detail_in(&mut f, &id).await.unwrap();
        assert!(d
            .properties
            .iter()
            .any(|p| p.label == "Zona horaria" && p.value == "UTC"));
        assert!(d
            .properties
            .iter()
            .any(|p| p.label == "Horarios por día" && p.value == "hasta 8"));
        assert!(d.notes.iter().any(|n| n.contains("8 veces por día")));
    }

    /// S1, the jobs side: nothing reaches the service.
    #[tokio::test]
    async fn read_only_connections_send_nothing() {
        let mut f = Fake::writer();
        f.set_read_only(true);
        routes(&f, HISTORY);
        let id = id(&f);
        assert!(run_in(&mut f, &id).await.is_err());
        assert!(stop_in(&mut f, &id).await.is_err());
        assert!(enable_in(&mut f, &id, false).await.is_err());
        assert!(f.requests().is_empty());
        // Not the owner: refused before the PATCH.
        let mut f = Fake::writer();
        f.account.as_mut().unwrap().upn = Some("otra@contoso.com".into());
        routes(&f, HISTORY);
        let jid = self::id(&f);
        assert!(enable_in(&mut f, &jid, true).await.is_err());
        assert!(f.requests().is_empty());
        // Ids from elsewhere never reach a path.
        let mut f = Fake::writer();
        for bad in ["refresh:me:../../x", "refresh:x:y", "pg_cron:1"] {
            assert!(run_in(&mut f, bad).await.is_err());
        }
        assert!(f.requests().is_empty());
    }
}
