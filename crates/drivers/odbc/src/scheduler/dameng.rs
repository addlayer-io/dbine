//! Dameng (DM): the job system in the `SYSJOB` schema managed whole with
//! its procedures, the way DM's manager does it: a job (`SP_CREATE_JOB`,
//! `SP_ALTER_JOB`, `SP_DROP_JOB`) with its steps and schedules, changed
//! between `SP_JOB_CONFIG_START` and `SP_JOB_CONFIG_COMMIT`; the operators
//! its notices go to (`SP_CREATE_OPERATOR`…) and the alerts
//! (`SP_CREATE_ALERT`, `SP_ALERT_ADD_OPERATOR`…). DBMS_JOB's and
//! DBMS_SCHEDULER's jobs are in [`super::dameng_dbms`]. Every save is one
//! transaction.
//!
//! Steps are written again whole when any of them changes (dropped and
//! added in their new order: DM keeps them in the order they were added);
//! schedules are dropped, altered or added one by one, by name. DM can't
//! rename a job: it is created with the new name and the old one dropped,
//! and its history stays under the old name. A job is changed by a DBA or
//! its owner; operators and alerts by a DBA. Ids are job `ID`s (DBMS_JOB's
//! too; DBMS_SCHEDULER's are `"OWNER"."JOB"`), operators' and alerts'
//! `ID`s.

use super::dameng_dbms;
use super::*;
use crate::jobs::dameng::scheduler::KIND as KIND_SCHEDULER;
use crate::jobs::dameng::{
    best_effort, me, one, read as read_jobs, rows_of, steps_sql, Me, Tables, KIND as KIND_JOB,
    KIND_DBMS_JOB, NOT_INIT, NO_ACCESS, OWN_ONLY, SCHEMA,
};
use crate::jobs::{int, int_id, lit, not_found};
use dbine_driver::{FormField, FormKind, FormList, FormSection, SchedulerKind};
use serde_json::json;

pub(super) const NO_START_AT: &str =
    "DM corre un job siempre desde su primer paso: no tiene una forma de empezarlo en otro";
const NOT_MINE: &str = "Solo un DBA o el dueño del job lo cambia o lo borra.";
const DBA_ONLY: &str = "Los operadores y las alertas los cambia un DBA.";
const CREATE_PRIV: &str = "Crear jobs pide el rol DBA o el privilegio ADMIN JOB; cada usuario cambia y borra los suyos y un DBA, todos.";
const RENAME: &str = "DM no cambia el nombre de un job: se crea con el nombre nuevo y se borra el anterior; el historial queda con el nombre anterior y el job recibe otro id.";
const RENAME_OWNER: &str = "Solo el dueño del job le cambia el nombre: DM crea el job nuevo a nombre de quien lo guarda, así que cambiaría de dueño.";
const STEPS_AGAIN: &str = "Cambiar los pasos los vuelve a escribir todos, en el orden nuevo.";
const NO_RENAME_OBJECT: &str =
    "DM no cambia el nombre de un operador ni de una alerta: creá otro y borrá este.";
const NO_HISTORY_ACCESS: &str =
    "Borrar el historial de un job pide leer y borrar las tablas de SYSJOB (rol SOI o DBA).";
const ALERT_CODES: &str = "Los códigos de tipo de alerta, tipo de error y tipo de sentencia son los que toma SP_CREATE_ALERT (ver el manual de DM).";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Job,
    DbmsJob,
    SchedulerJob,
    Operator,
    Alert,
}

pub(super) fn kind(kind: &str) -> Result<Kind> {
    match kind.trim() {
        "job" => Ok(Kind::Job),
        "dbms_job" => Ok(Kind::DbmsJob),
        "scheduler_job" => Ok(Kind::SchedulerJob),
        "operator" => Ok(Kind::Operator),
        "alert" => Ok(Kind::Alert),
        other => Err(bad_kind(other)),
    }
}

/// What an existing job id is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Flavor {
    Agent,
    DbmsJob,
    Scheduler,
}

pub(super) fn flavor(src: &mut dyn JobSource, id: &str) -> Result<Flavor> {
    if crate::jobs::dameng::scheduler::is_mine(id) {
        return Ok(Flavor::Scheduler);
    }
    let (s, t, n) = one(src, id)?;
    let dbms = int(s.get(0, "SYS_FLAG")) == Some(2)
        || crate::jobs::dameng::dbms_job_row(src, &s, &t, &n)
            .ok()
            .flatten()
            .is_some();
    Ok(if dbms { Flavor::DbmsJob } else { Flavor::Agent })
}

// ------------------------------------------------------------------ info and lists

pub(super) fn info(src: &mut dyn JobSource) -> Result<SchedulerInfo> {
    let me = me(src);
    let mut kinds = Vec::new();
    let mut notes = Vec::new();
    match read_jobs(src, Some("-1")) {
        Ok((_, t)) => {
            kinds.push(SchedulerKind::new("job", "Jobs", "job").abilities(true, true, true));
            kinds.push(
                SchedulerKind::new("dbms_job", "Jobs de DBMS_JOB", "job de DBMS_JOB")
                    .abilities(true, true, true),
            );
            notes.push(CREATE_PRIV.to_string());
            if t.own {
                notes.push(OWN_ONLY.to_string());
            } else {
                let dba = me.dba;
                kinds.push(
                    SchedulerKind::new("operator", "Operadores", "operador")
                        .abilities(dba, dba, dba),
                );
                kinds.push(
                    SchedulerKind::new("alert", "Alertas", "alerta").abilities(dba, dba, dba),
                );
                if !dba {
                    notes.push(DBA_ONLY.to_string());
                }
            }
        }
        Err(e) => {
            let schema = src.query(SCHEMA, &[]).ok().and_then(|s| int(s.get(0, "N")));
            match schema {
                Some(0) => notes.push(NOT_INIT.to_string()),
                Some(_) => notes.push(NO_ACCESS.to_string()),
                None => return Err(e),
            }
        }
    }
    if crate::jobs::dameng::scheduler::read(src, Some(("", ""))).is_ok() {
        kinds.push(
            SchedulerKind::new(
                "scheduler_job",
                "Jobs de DBMS_SCHEDULER",
                "job de DBMS_SCHEDULER",
            )
            .abilities(true, true, true),
        );
    }
    Ok(SchedulerInfo {
        kinds,
        logs: false,
        notes,
    })
}

pub(super) fn items(
    p: &Preset,
    src: &mut dyn JobSource,
    ctx: &Ctx,
    k: Kind,
) -> Result<SchedulerList> {
    let wanted = match k {
        Kind::Job => KIND_JOB,
        Kind::DbmsJob => KIND_DBMS_JOB,
        Kind::SchedulerJob => KIND_SCHEDULER,
        Kind::Operator => return operator_items(src),
        Kind::Alert => return alert_items(src),
    };
    let l = jobs::list(p, src, ctx)?;
    Ok(SchedulerList {
        items: l
            .jobs
            .into_iter()
            .filter(|j| j.kind == wanted)
            .map(|j| SchedulerItem {
                id: j.id,
                name: j.name,
                description: j.description,
                enabled: Some(j.enabled),
                system: j.system,
                can_edit: None,
                can_delete: None,
                actions: None,
            })
            .collect(),
        notes: l.notes,
    })
}

const OPERATORS_ALL: &str = "SELECT * FROM SYSJOB.SYSOPERATORS ORDER BY NAME";
const ALERTS_ALL: &str = "SELECT * FROM SYSJOB.SYSALERTS ORDER BY NAME";
const OP_EMAIL: &[&str] = &["EMAILADDR", "EMAIL_ADDR", "EMAIL"];
const OP_NETSEND: &[&str] = &["NETSEND_IP", "NETSENDADDR", "NETSEND_ADDR", "NETSEND"];
const AL_ERRTYPE: &[&str] = &["ERRTYPE", "ERR_TYPE"];
const AL_ERRCODE: &[&str] = &["ERRCODE", "ERR_CODE"];
const AL_SQLTYPE: &[&str] = &["SQL_TYPE", "SQLTYPE"];
const AL_DELAY: &[&str] = &["DELAY_TIME", "DELAYTIME", "DELAY"];
const NO_OPERID: &[&str] = &["OPERID", "OPERATORID", "OPERATOR_ID"];
const NO_EMAIL: &[&str] = &["ENABLEEMAIL", "ENABLE_EMAIL"];
const NO_NETSEND: &[&str] = &["ENABLENETSEND", "ENABLE_NETSEND"];

fn operator_items(src: &mut dyn JobSource) -> Result<SchedulerList> {
    let s = src.query(OPERATORS_ALL, &[])?;
    Ok(SchedulerList {
        items: (0..s.rows.len())
            .filter_map(|r| {
                Some(SchedulerItem {
                    id: int(s.get(r, "ID"))?.to_string(),
                    name: s.get(r, "NAME")?.to_string(),
                    description: crate::jobs::get(&s, r, OP_EMAIL).map(str::to_string),
                    enabled: Some(int(s.get(r, "ENABLE")) != Some(0)),
                    system: false,
                    can_edit: None,
                    can_delete: None,
                    actions: None,
                })
            })
            .collect(),
        notes: Vec::new(),
    })
}

fn alert_items(src: &mut dyn JobSource) -> Result<SchedulerList> {
    let s = src.query(ALERTS_ALL, &[])?;
    Ok(SchedulerList {
        items: (0..s.rows.len())
            .filter_map(|r| {
                Some(SchedulerItem {
                    id: int(s.get(r, "ID"))?.to_string(),
                    name: s.get(r, "NAME")?.to_string(),
                    description: s.get(r, "DESCRIBE").map(str::to_string),
                    enabled: Some(int(s.get(r, "ENABLE")) != Some(0)),
                    system: false,
                    can_edit: None,
                    can_delete: None,
                    actions: None,
                })
            })
            .collect(),
        notes: Vec::new(),
    })
}

/// Operator `(ID, NAME)` pairs (empty without access to SYSJOB's tables).
fn operators(src: &mut dyn JobSource) -> Vec<(String, String)> {
    let s = best_effort(src, OPERATORS_ALL, &[]);
    (0..s.rows.len())
        .filter_map(|r| Some((s.get(r, "ID")?.to_string(), s.get(r, "NAME")?.to_string())))
        .collect()
}

fn operator_choices(ops: &[(String, String)]) -> Vec<(String, String)> {
    ops.iter().map(|(_, n)| (n.clone(), n.clone())).collect()
}

// ------------------------------------------------------------------ agent jobs

/// A step of a job.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct AgentStep {
    pub(super) name: String,
    pub(super) kind: i64,
    pub(super) command: String,
    /// 0: the next step; 1: end, reporting success.
    pub(super) on_success: i64,
    /// 0: end, reporting failure; 1: the next step.
    pub(super) on_failure: i64,
    pub(super) retries: i64,
    pub(super) retry_interval: i64,
    pub(super) output: Option<String>,
    pub(super) append: bool,
    pub(super) description: String,
    pub(super) success_next: Option<String>,
    pub(super) failure_next: Option<String>,
}

/// A schedule of a job. Times: seconds after midnight; dates: ms.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct AgentSched {
    pub(super) name: String,
    pub(super) enabled: bool,
    /// 0 once, 1 daily, 2 weekly, 3 a day of the month, 4–8 the first…
    /// last given day of the month.
    pub(super) kind: i64,
    pub(super) every: i64,
    pub(super) sub: i64,
    pub(super) minutes: i64,
    pub(super) start_time: Option<i64>,
    pub(super) end_time: Option<i64>,
    pub(super) start_date: Option<i64>,
    pub(super) end_date: Option<i64>,
    pub(super) description: String,
}

/// A job of the job system as the form edits it.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct AgentJob {
    pub(super) name: String,
    pub(super) enabled: bool,
    pub(super) description: String,
    pub(super) email: bool,
    pub(super) email_operator: Option<String>,
    pub(super) email_type: i64,
    pub(super) netsend: bool,
    pub(super) netsend_operator: Option<String>,
    pub(super) netsend_type: i64,
    pub(super) steps: Vec<AgentStep>,
    pub(super) schedules: Vec<AgentSched>,
}

/// The weekly schedule's day bits, Sunday to Saturday.
const WEEK: [&str; 7] = ["1", "2", "4", "8", "16", "32", "64"];

fn text_col(s: &Set, r: usize, c: &str) -> String {
    s.get(r, c).unwrap_or_default().to_string()
}

/// The raw text of a column (code keeps its spaces).
fn raw_col(s: &Set, r: usize, c: &str) -> Option<String> {
    let i = s
        .cols
        .iter()
        .position(|x| x.trim().eq_ignore_ascii_case(c))?;
    s.rows
        .get(r)?
        .get(i)?
        .clone()
        .filter(|v| !v.trim().is_empty())
}

fn secs(v: Option<&str>) -> Option<i64> {
    let v = v?;
    match crate::jobs::parse_ts(v).filter(|_| v.trim().len() > 10) {
        Some(ms) => Some(ms.rem_euclid(86_400_000) / 1000),
        None => crate::jobs::parse_time(v).map(|ms| ms / 1000),
    }
}

/// The job `id` of the job system: its row, where it was read from and
/// what the form edits.
fn agent_read(src: &mut dyn JobSource, id: &str) -> Result<(String, Set, Tables, AgentJob, bool)> {
    let (s, t, n) = one(src, id)?;
    let ops = if t.own { Vec::new() } else { operators(src) };
    let op = |c: &str| -> Option<String> {
        let id = s.get(0, c)?;
        ops.iter().find(|(i, _)| i == id).map(|(_, n)| n.clone())
    };
    let st = src.query(&steps_sql(&t), &[&n])?;
    let has_next = st.has("SUCC_NEXT_STEP");
    let steps = rows_of(&st, "JOBID", &n)
        .map(|r| AgentStep {
            name: text_col(&st, r, "NAME"),
            kind: int(st.get(r, "TYPE")).unwrap_or(0),
            command: raw_col(&st, r, "COMMAND").unwrap_or_default(),
            on_success: int(st.get(r, "SUCC_ACTION")).unwrap_or(0),
            on_failure: int(st.get(r, "FAIL_ACTION")).unwrap_or(0),
            retries: int(st.get(r, "RETRY_ATTEMPTS")).unwrap_or(0),
            retry_interval: int(st.get(r, "RETRY_INTERVAL")).unwrap_or(0),
            output: st.get(r, "OUTPUT_FILE_PATH").map(str::to_string),
            append: int(st.get(r, "APPEND_FLAG")) == Some(1),
            description: text_col(&st, r, "DESCRIBE"),
            success_next: st.get(r, "SUCC_NEXT_STEP").map(str::to_string),
            failure_next: st.get(r, "FAIL_NEXT_STEP").map(str::to_string),
        })
        .collect();
    let sch = src.query(
        &format!(
            "SELECT * FROM {} WHERE JOBID = ? ORDER BY NAME",
            t.schedules
        ),
        &[&n],
    )?;
    let schedules = rows_of(&sch, "JOBID", &n)
        .filter(|r| int(sch.get(*r, "TYPE")) != Some(9))
        .map(|r| AgentSched {
            name: text_col(&sch, r, "NAME"),
            enabled: int(sch.get(r, "ENABLE")) != Some(0),
            kind: int(sch.get(r, "TYPE")).unwrap_or(1),
            every: int(sch.get(r, "FREQ_INTERVAL")).unwrap_or(1),
            sub: int(sch.get(r, "FREQ_SUB_INTERVAL")).unwrap_or(0),
            minutes: int(sch.get(r, "FREQ_MINUTE_INTERVAL")).unwrap_or(0),
            start_time: secs(sch.get(r, "STARTTIME")),
            end_time: secs(sch.get(r, "ENDTIME")),
            start_date: sch
                .get(r, "DURING_START_DATE")
                .and_then(crate::jobs::parse_ts),
            end_date: sch
                .get(r, "DURING_END_DATE")
                .and_then(crate::jobs::parse_ts),
            description: text_col(&sch, r, "DESCRIBE"),
        })
        .collect();
    let j = AgentJob {
        name: text_col(&s, 0, "NAME"),
        enabled: int(s.get(0, "ENABLE")) == Some(1),
        description: text_col(&s, 0, "DESCRIBE"),
        email: int(s.get(0, "ENABLE_EMAIL")) == Some(1),
        email_operator: op("EMAIL_OPERID"),
        email_type: int(s.get(0, "EMAIL_TYPE")).unwrap_or(0),
        netsend: int(s.get(0, "ENABLE_NETSEND")) == Some(1),
        netsend_operator: op("NETSEND_OPERID"),
        netsend_type: int(s.get(0, "NETSEND_TYPE")).unwrap_or(0),
        steps,
        schedules,
    };
    Ok((n, s, t, j, has_next))
}

fn notice_type() -> FormKind {
    FormKind::select(&[
        ("0", "Cuando termina bien"),
        ("1", "Cuando falla"),
        ("2", "Cuando termina"),
    ])
}

pub(super) fn agent_sections(existing: bool, has_next: bool) -> Vec<FormSection> {
    let monthly: &[&str] = &["4", "5", "6", "7", "8"];
    let repeating: &[&str] = &["1", "2", "3", "4", "5", "6", "7", "8"];
    let mut step_fields = vec![
        FormField::new("name", "Nombre", FormKind::Text).required(),
        FormField::new(
            "type",
            "Tipo",
            FormKind::select(&[
                ("0", "SQL"),
                ("1", "Copia de seguridad (V1.0, tipo 1)"),
                ("2", "Reorganizar la base"),
                ("3", "Actualizar estadísticas"),
                ("4", "Migración DTS"),
                ("5", "Copia de seguridad (V1.0, tipo 5)"),
                ("6", "Copia de seguridad (V2.0)"),
            ]),
        )
        .default(json!("0")),
        FormField::new("command", "Comando", FormKind::code("sql"))
            .required()
            .help("Para SQL, las sentencias; para los demás tipos, el comando como lo arma el manager de DM."),
        FormField::new(
            "on_success",
            "Si termina bien",
            FormKind::select(&[("0", "Seguir con el paso siguiente"), ("1", "Terminar el job con éxito")]),
        )
        .default(json!("0")),
        FormField::new(
            "on_failure",
            "Si falla",
            FormKind::select(&[("0", "Terminar el job con falla"), ("1", "Seguir con el paso siguiente")]),
        )
        .default(json!("0")),
        FormField::new("retries", "Reintentos", FormKind::number(Some(0.0), None)).default(json!(0)),
        FormField::new(
            "retry_interval",
            "Espera entre reintentos (segundos)",
            FormKind::number(Some(0.0), None),
        )
        .default(json!(0)),
        FormField::new("output", "Archivo de salida", FormKind::Text),
        FormField::new("append", "Agregar al archivo", FormKind::Bool).default(json!(false)),
        FormField::new("description", "Descripción", FormKind::Text),
    ];
    if has_next {
        step_fields.push(
            FormField::new(
                "success_next",
                "Si termina bien, ir a",
                FormKind::reference("list:steps", false),
            )
            .help("Vacío: lo que diga «Si termina bien»."),
        );
        step_fields.push(
            FormField::new(
                "failure_next",
                "Si falla, ir a",
                FormKind::reference("list:steps", false),
            )
            .help("Vacío: lo que diga «Si falla»."),
        );
    }
    let sched_fields = vec![
        FormField::new("name", "Nombre", FormKind::Text).required(),
        FormField::new("enabled", "Habilitada", FormKind::Bool).default(json!(true)),
        FormField::new(
            "type",
            "Frecuencia",
            FormKind::select(&[
                ("0", "Una vez"),
                ("1", "Diaria"),
                ("2", "Semanal"),
                ("3", "Mensual, un día del mes"),
                ("4", "Mensual, el primer…"),
                ("5", "Mensual, el segundo…"),
                ("6", "Mensual, el tercer…"),
                ("7", "Mensual, el cuarto…"),
                ("8", "Mensual, el último…"),
            ]),
        )
        .default(json!("1")),
        FormField::new(
            "every",
            "Cada (días, semanas o meses)",
            FormKind::number(Some(1.0), None),
        )
        .default(json!(1))
        .when("type", repeating),
        FormField::new(
            "week_days",
            "Días de la semana",
            FormKind::multi(&[
                ("1", "Domingo"),
                ("2", "Lunes"),
                ("4", "Martes"),
                ("8", "Miércoles"),
                ("16", "Jueves"),
                ("32", "Viernes"),
                ("64", "Sábado"),
            ]),
        )
        .when("type", &["2"]),
        FormField::new(
            "month_day",
            "Día del mes",
            FormKind::number(Some(1.0), Some(31.0)),
        )
        .when("type", &["3"]),
        FormField::new(
            "which_day",
            "Qué día",
            FormKind::select(&[
                ("1", "Domingo"),
                ("2", "Lunes"),
                ("3", "Martes"),
                ("4", "Miércoles"),
                ("5", "Jueves"),
                ("6", "Viernes"),
                ("7", "Sábado"),
                ("8", "Día"),
                ("9", "Día hábil"),
                ("10", "Día de fin de semana"),
            ]),
        )
        .when("type", monthly),
        FormField::new("start_time", "Hora", FormKind::Time)
            .default(json!("00:00:00"))
            .when("type", repeating),
        FormField::new(
            "minutes",
            "Repetir cada (minutos)",
            FormKind::number(Some(0.0), Some(1440.0)),
        )
        .default(json!(0))
        .help("0: una vez, a la hora indicada.")
        .when("type", repeating),
        FormField::new("end_time", "Hasta la hora", FormKind::Time).when("type", repeating),
        FormField::new("start_date", "Desde", FormKind::DateTime)
            .required()
            .help("Para «Una vez», el momento en que corre."),
        FormField::new("end_date", "Hasta", FormKind::DateTime),
        FormField::new("description", "Descripción", FormKind::Text),
    ];
    let mut out = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("name", "Nombre", FormKind::Text)
                .required()
                .help("Cambiarlo crea el job de nuevo con ese nombre."),
            FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
            FormField::new("description", "Descripción", FormKind::Textarea),
        ]),
        FormSection::new("notify", "Avisos").fields(vec![
            FormField::new("email", "Avisar por correo", FormKind::Bool).default(json!(false)),
            FormField::new(
                "email_operator",
                "Operador (correo)",
                FormKind::reference("operator", false),
            )
            .when("email", &["true"]),
            FormField::new("email_type", "Cuándo (correo)", notice_type())
                .default(json!("1"))
                .when("email", &["true"]),
            FormField::new("netsend", "Avisar por net send", FormKind::Bool).default(json!(false)),
            FormField::new(
                "netsend_operator",
                "Operador (net send)",
                FormKind::reference("operator", false),
            )
            .when("netsend", &["true"]),
            FormField::new("netsend_type", "Cuándo (net send)", notice_type())
                .default(json!("1"))
                .when("netsend", &["true"]),
        ]),
        FormSection::new("steps", "Pasos").list(
            FormList::new("steps", "Pasos", "name", step_fields)
                .orderable()
                .min_rows(1),
        ),
        FormSection::new("schedules", "Programaciones").list(FormList::new(
            "schedules",
            "Programaciones",
            "name",
            sched_fields,
        )),
    ];
    if existing {
        out.push(FormSection::new("state", "Estado").fields(vec![
            FormField::new("id", "Id", FormKind::Text).read_only(true),
            FormField::new("owner", "Dueño", FormKind::Text).read_only(true),
            FormField::new("created", "Creado", FormKind::Text).read_only(true),
            FormField::new("modified", "Modificado", FormKind::Text).read_only(true),
            FormField::new("valid", "Completo", FormKind::Text).read_only(true),
            FormField::new("ep_seqno", "Nodo de ejecución", FormKind::Text).read_only(true),
            FormField::new("raft", "Grupo RAFT", FormKind::Text).read_only(true),
        ]));
    }
    out
}

pub(super) fn agent_values(j: &AgentJob) -> BTreeMap<String, Value> {
    values(vec![
        ("name", json!(j.name)),
        ("enabled", json!(j.enabled)),
        ("description", json!(j.description)),
        ("email", json!(j.email)),
        ("email_operator", jtext(j.email_operator.as_deref())),
        ("email_type", json!(j.email_type.to_string())),
        ("netsend", json!(j.netsend)),
        ("netsend_operator", jtext(j.netsend_operator.as_deref())),
        ("netsend_type", json!(j.netsend_type.to_string())),
    ])
}

fn step_rows(j: &AgentJob, has_next: bool) -> Vec<BTreeMap<String, Value>> {
    j.steps
        .iter()
        .map(|s| {
            let mut v = values(vec![
                ("name", json!(s.name)),
                ("type", json!(s.kind.to_string())),
                ("command", json!(s.command)),
                ("on_success", json!(s.on_success.to_string())),
                ("on_failure", json!(s.on_failure.to_string())),
                ("retries", json!(s.retries)),
                ("retry_interval", json!(s.retry_interval)),
                ("output", jtext(s.output.as_deref())),
                ("append", json!(s.append)),
                ("description", json!(s.description)),
            ]);
            if has_next {
                v.insert("success_next".into(), jtext(s.success_next.as_deref()));
                v.insert("failure_next".into(), jtext(s.failure_next.as_deref()));
            }
            v
        })
        .collect()
}

fn sched_rows(j: &AgentJob) -> Vec<BTreeMap<String, Value>> {
    j.schedules
        .iter()
        .map(|s| {
            let week: Vec<String> = WEEK
                .iter()
                .filter(|bit| s.sub & bit.parse::<i64>().unwrap_or(0) != 0)
                .map(|bit| bit.to_string())
                .collect();
            values(vec![
                ("name", json!(s.name)),
                ("enabled", json!(s.enabled)),
                ("type", json!(s.kind.to_string())),
                ("every", json!(s.every)),
                (
                    "week_days",
                    json!(if s.kind == 2 { week } else { Vec::new() }),
                ),
                (
                    "month_day",
                    if s.kind == 3 {
                        json!(s.sub)
                    } else {
                        Value::Null
                    },
                ),
                (
                    "which_day",
                    if (4..=8).contains(&s.kind) {
                        json!(s.sub.to_string())
                    } else {
                        Value::Null
                    },
                ),
                ("minutes", json!(s.minutes)),
                (
                    "start_time",
                    s.start_time.map_or(Value::Null, |t| json!(time_text(t))),
                ),
                (
                    "end_time",
                    s.end_time.map_or(Value::Null, |t| json!(time_text(t))),
                ),
                (
                    "start_date",
                    s.start_date.map_or(Value::Null, |d| json!(dt_value(d))),
                ),
                (
                    "end_date",
                    s.end_date.map_or(Value::Null, |d| json!(dt_value(d))),
                ),
                ("description", json!(s.description)),
            ])
        })
        .collect()
}

fn with_rows(mut sections: Vec<FormSection>, j: &AgentJob, has_next: bool) -> Vec<FormSection> {
    for s in &mut sections {
        for l in &mut s.lists {
            l.rows = match l.key.as_str() {
                "steps" => step_rows(j, has_next),
                "schedules" => sched_rows(j),
                _ => Vec::new(),
            };
        }
    }
    sections
}

/// Whether the steps' table has the next-step columns (best effort).
fn next_steps(src: &mut dyn JobSource) -> bool {
    src.query(
        "SELECT SUCC_NEXT_STEP, FAIL_NEXT_STEP FROM SYSJOB.SYSJOBSTEPS WHERE 1 = 0",
        &[],
    )
    .is_ok()
}

fn agent_form(src: &mut dyn JobSource, id: Option<&str>) -> Result<ObjectForm> {
    let ops = operators(src);
    let choices = BTreeMap::from([("operator".to_string(), operator_choices(&ops))]);
    let Some(id) = id else {
        let has_next = next_steps(src);
        let j = AgentJob {
            enabled: true,
            email_type: 1,
            netsend_type: 1,
            steps: vec![AgentStep {
                name: "paso1".into(),
                ..AgentStep::default()
            }],
            ..AgentJob::default()
        };
        return Ok(ObjectForm {
            sections: with_rows(agent_sections(false, has_next), &j, has_next),
            values: agent_values(&j),
            choices,
            notes: vec![STEPS_AGAIN.to_string()],
            read_only: false,
        });
    };
    let (n, s, _, j, has_next) = agent_read(src, id)?;
    let me = me(src);
    let mine = me.manages(s.get(0, "USERNAME"));
    let mut v = agent_values(&j);
    v.insert("id".into(), json!(n));
    for (k, c) in [
        ("owner", "USERNAME"),
        ("created", "CREATETIME"),
        ("modified", "MODIFYTIME"),
        ("valid", "VALID"),
        ("ep_seqno", "EP_SEQNO"),
        ("raft", "RAFT_NAME"),
    ] {
        v.insert(k.into(), jtext(s.get(0, c)));
    }
    let mut notes = vec![RENAME.to_string(), STEPS_AGAIN.to_string()];
    if !mine {
        notes.insert(0, NOT_MINE.to_string());
    } else if !owns(&me, s.get(0, "USERNAME")) {
        notes.push(RENAME_OWNER.to_string());
    }
    Ok(ObjectForm {
        sections: with_rows(agent_sections(true, has_next), &j, has_next),
        values: v,
        choices,
        notes,
        read_only: !mine,
    })
}

/// An operator's name from the form: one of the server's, when it lists
/// them.
fn operator_pick(
    v: Vals,
    k: &str,
    label: &str,
    ops: &[(String, String)],
) -> Result<Option<String>> {
    let Some(name) = optional(v, k, label, 128)? else {
        return Ok(None);
    };
    if ops.is_empty() {
        return Ok(Some(name));
    }
    ops.iter()
        .find(|(_, n)| n.eq_ignore_ascii_case(&name))
        .map(|(_, n)| Some(n.clone()))
        .ok_or_else(|| bad(format!("No existe el operador «{}».", short(&name))))
}

fn int_choice(v: Vals, k: &str, label: &str, options: &[&str], default: &str) -> Result<i64> {
    Ok(choice(v, k, label, options, default)?.parse().unwrap_or(0))
}

pub(super) fn agent_from(
    values: &FormValues,
    ops: &[(String, String)],
    has_next: bool,
) -> Result<AgentJob> {
    let v = Vals(&values.values);
    let mut j = AgentJob {
        name: required(v, "name", "Nombre", 128)?,
        enabled: v.bool_or("enabled", true),
        description: code_text(v, "description", "Descripción", 4000)?.unwrap_or_default(),
        email: v.bool_or("email", false),
        email_operator: operator_pick(v, "email_operator", "Operador (correo)", ops)?,
        email_type: int_choice(v, "email_type", "Cuándo (correo)", &["0", "1", "2"], "1")?,
        netsend: v.bool_or("netsend", false),
        netsend_operator: operator_pick(v, "netsend_operator", "Operador (net send)", ops)?,
        netsend_type: int_choice(
            v,
            "netsend_type",
            "Cuándo (net send)",
            &["0", "1", "2"],
            "1",
        )?,
        ..AgentJob::default()
    };
    if j.email && j.email_operator.is_none() {
        return Err(bad("Falta «Operador (correo)»."));
    }
    if j.netsend && j.netsend_operator.is_none() {
        return Err(bad("Falta «Operador (net send)»."));
    }
    for (i, row) in rows(values, "steps").into_iter().enumerate() {
        let at = |label: &str| format!("Paso {}: «{label}»", i + 1);
        let step = AgentStep {
            name: required(row, "name", &at("Nombre"), 128)?,
            kind: int_choice(
                row,
                "type",
                &at("Tipo"),
                &["0", "1", "2", "3", "4", "5", "6"],
                "0",
            )?,
            command: code_text(row, "command", &at("Comando"), 1_000_000)?
                .ok_or_else(|| bad(format!("Falta {}.", at("Comando"))))?,
            on_success: int_choice(row, "on_success", &at("Si termina bien"), &["0", "1"], "0")?,
            on_failure: int_choice(row, "on_failure", &at("Si falla"), &["0", "1"], "0")?,
            retries: row
                .int("retries", &at("Reintentos"), 0, 1_000_000)?
                .unwrap_or(0),
            retry_interval: row
                .int(
                    "retry_interval",
                    &at("Espera entre reintentos (segundos)"),
                    0,
                    1_000_000,
                )?
                .unwrap_or(0),
            output: optional(row, "output", &at("Archivo de salida"), 256)?,
            append: row.bool_or("append", false),
            description: optional(row, "description", &at("Descripción"), 4000)?
                .unwrap_or_default(),
            success_next: if has_next {
                optional(row, "success_next", &at("Si termina bien, ir a"), 128)?
            } else {
                None
            },
            failure_next: if has_next {
                optional(row, "failure_next", &at("Si falla, ir a"), 128)?
            } else {
                None
            },
        };
        if j.steps.iter().any(|s| s.name == step.name) {
            return Err(bad(format!(
                "El paso «{}» está repetido.",
                short(&step.name)
            )));
        }
        j.steps.push(step);
    }
    if j.steps.is_empty() {
        return Err(bad("Un job necesita al menos un paso."));
    }
    for s in &j.steps {
        for next in [&s.success_next, &s.failure_next].into_iter().flatten() {
            if !j.steps.iter().any(|x| &x.name == next) {
                return Err(bad(format!("No hay un paso «{}».", short(next))));
            }
        }
    }
    for (i, row) in rows(values, "schedules").into_iter().enumerate() {
        let at = |label: &str| format!("Programación {}: «{label}»", i + 1);
        let kind = int_choice(
            row,
            "type",
            &at("Frecuencia"),
            &["0", "1", "2", "3", "4", "5", "6", "7", "8"],
            "1",
        )?;
        let sub = match kind {
            2 => {
                let mut mask = 0;
                for d in row.strings("week_days") {
                    let bit = WEEK
                        .iter()
                        .find(|b| **b == d)
                        .and_then(|b| b.parse::<i64>().ok())
                        .ok_or_else(|| {
                            bad(format!("«{}» no es un día de la semana.", short(&d)))
                        })?;
                    mask |= bit;
                }
                if mask == 0 {
                    return Err(bad(format!(
                        "{}: elegí al menos un día.",
                        at("Días de la semana")
                    )));
                }
                mask
            }
            3 => row
                .int("month_day", &at("Día del mes"), 1, 31)?
                .ok_or_else(|| bad(format!("Falta {}.", at("Día del mes"))))?,
            4..=8 => int_choice(
                row,
                "which_day",
                &at("Qué día"),
                &["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"],
                "2",
            )?,
            _ => 0,
        };
        let sched = AgentSched {
            name: required(row, "name", &at("Nombre"), 128)?,
            enabled: row.bool_or("enabled", true),
            kind,
            every: if kind == 0 {
                1
            } else {
                row.int("every", &at("Cada (días, semanas o meses)"), 1, 100_000)?
                    .unwrap_or(1)
            },
            sub,
            minutes: if kind == 0 {
                0
            } else {
                row.int("minutes", &at("Repetir cada (minutos)"), 0, 1440)?
                    .unwrap_or(0)
            },
            start_time: if kind == 0 {
                None
            } else {
                Some(time(row, "start_time", &at("Hora"))?.unwrap_or(0))
            },
            end_time: if kind == 0 {
                None
            } else {
                time(row, "end_time", &at("Hasta la hora"))?
            },
            start_date: Some(
                datetime(row, "start_date", &at("Desde"))?
                    .ok_or_else(|| bad(format!("Falta {}.", at("Desde"))))?,
            ),
            end_date: datetime(row, "end_date", &at("Hasta"))?,
            description: optional(row, "description", &at("Descripción"), 4000)?
                .unwrap_or_default(),
        };
        if let (Some(a), Some(b)) = (sched.start_date, sched.end_date) {
            if b < a {
                return Err(bad(format!("{} es anterior a «Desde».", at("Hasta"))));
            }
        }
        if j.schedules.iter().any(|s| s.name == sched.name) {
            return Err(bad(format!(
                "La programación «{}» está repetida.",
                short(&sched.name)
            )));
        }
        j.schedules.push(sched);
    }
    Ok(j)
}

// ------------------------------------------------------------------ scripts

fn arg(v: Option<&str>) -> String {
    v.map(lit).unwrap_or_else(|| "NULL".into())
}

fn flag(b: bool) -> &'static str {
    if b {
        "1"
    } else {
        "0"
    }
}

fn job_args(j: &AgentJob) -> String {
    format!(
        "{}, {}, {}, {}, {}, {}, {}, {}, {}",
        lit(&j.name),
        flag(j.enabled),
        flag(j.email),
        lit(j
            .email_operator
            .as_deref()
            .filter(|_| j.email)
            .unwrap_or("")),
        j.email_type,
        flag(j.netsend),
        lit(j
            .netsend_operator
            .as_deref()
            .filter(|_| j.netsend)
            .unwrap_or("")),
        j.netsend_type,
        lit(&j.description),
    )
}

fn add_step(job: &str, s: &AgentStep) -> String {
    format!(
        "CALL SP_ADD_JOB_STEP_EX({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
        lit(job),
        lit(&s.name),
        s.kind,
        lit(&s.command),
        s.on_success,
        s.on_failure,
        s.retries,
        s.retry_interval,
        arg(s.output.as_deref()),
        flag(s.append),
        lit(&s.description),
    )
}

fn sched_args(job: &str, s: &AgentSched) -> String {
    let t = |v: Option<i64>| {
        v.map(|t| lit(&time_text(t)))
            .unwrap_or_else(|| "NULL".into())
    };
    let d = |v: Option<i64>| {
        v.map(|ms| lit(&crate::jobs::plain(ms)))
            .unwrap_or_else(|| "NULL".into())
    };
    format!(
        "{}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}",
        lit(job),
        lit(&s.name),
        flag(s.enabled),
        s.kind,
        s.every,
        s.sub,
        s.minutes,
        t(s.start_time),
        t(s.end_time),
        d(s.start_date),
        d(s.end_date),
        lit(&s.description),
    )
}

/// The calls that add a job's steps, then their jumps (once every step
/// exists).
fn step_calls(j: &AgentJob) -> Vec<String> {
    let mut out: Vec<String> = j.steps.iter().map(|s| add_step(&j.name, s)).collect();
    for s in &j.steps {
        if s.success_next.is_some() || s.failure_next.is_some() {
            out.push(format!(
                "CALL SP_JOB_STEP_SET_NEXT_STEP({}, {}, {}, {})",
                lit(&j.name),
                lit(&s.name),
                arg(s.success_next.as_deref()),
                arg(s.failure_next.as_deref()),
            ));
        }
    }
    out
}

fn agent_lookup(name: String) -> Lookup {
    Box::new(move |src: &mut dyn JobSource| {
        for t in ["SYSJOB.SYSJOBS", "SYSJOB.USER_JOBS_VIEW"] {
            if let Ok(s) = src.query(&format!("SELECT ID FROM {t} WHERE NAME = ?"), &[&name]) {
                return Ok(int(s.get(0, "ID")).map(|n| n.to_string()));
            }
        }
        Ok(None)
    })
}

fn create_into(p: &mut Plan, j: &AgentJob) {
    p.exec(format!("CALL SP_CREATE_JOB({})", job_args(j)));
    p.exec(format!("CALL SP_JOB_CONFIG_START({})", lit(&j.name)));
    for c in step_calls(j) {
        p.exec(c);
    }
    for s in &j.schedules {
        p.exec(format!(
            "CALL SP_ADD_JOB_SCHEDULE({})",
            sched_args(&j.name, s)
        ));
    }
    p.exec(format!("CALL SP_JOB_CONFIG_COMMIT({})", lit(&j.name)));
    p.lookup = Some(agent_lookup(j.name.clone()));
}

pub(super) fn agent_create_plan(j: &AgentJob) -> Plan {
    let mut p = Plan::new(&format!("Job «{}» de DM", j.name));
    p.atomic = true;
    create_into(&mut p, j);
    p
}

pub(super) fn agent_alter_plan(n: &str, cur: &AgentJob, j: &AgentJob) -> Plan {
    if j.name != cur.name {
        let mut p = Plan::new(&format!("Job «{}» de DM", cur.name));
        p.atomic = true;
        for line in RENAME.split("; ") {
            p.comment(line);
        }
        create_into(&mut p, j);
        p.exec(format!("CALL SP_DROP_JOB({})", lit(&cur.name)));
        return p;
    }
    let mut body: Vec<String> = Vec::new();
    if job_args(j) != job_args(cur) {
        body.push(format!("CALL SP_ALTER_JOB({})", job_args(j)));
    }
    if j.steps != cur.steps {
        for s in &cur.steps {
            body.push(format!(
                "CALL SP_DROP_JOB_STEP({}, {})",
                lit(&cur.name),
                lit(&s.name)
            ));
        }
        body.extend(step_calls(j));
    }
    for s in &cur.schedules {
        if !j.schedules.iter().any(|x| x.name == s.name) {
            body.push(format!(
                "CALL SP_DROP_JOB_SCHEDULE({}, {})",
                lit(&cur.name),
                lit(&s.name)
            ));
        }
    }
    for s in &j.schedules {
        match cur.schedules.iter().find(|x| x.name == s.name) {
            Some(old) if old == s => {}
            Some(_) => body.push(format!(
                "CALL SP_ALTER_JOB_SCHEDULE({})",
                sched_args(&j.name, s)
            )),
            None => body.push(format!(
                "CALL SP_ADD_JOB_SCHEDULE({})",
                sched_args(&j.name, s)
            )),
        }
    }
    if body.is_empty() {
        return Plan::nothing(&cur.name, n.to_string());
    }
    let mut p = Plan::new(&format!("Job «{}» de DM", cur.name));
    p.atomic = true;
    p.exec(format!("CALL SP_JOB_CONFIG_START({})", lit(&cur.name)));
    for b in body {
        p.exec(b);
    }
    p.exec(format!("CALL SP_JOB_CONFIG_COMMIT({})", lit(&cur.name)));
    p.id = Some(n.to_string());
    p
}

fn agent_plan(src: &mut dyn JobSource, id: Option<&str>, values: &FormValues) -> Result<Plan> {
    let ops = operators(src);
    match id {
        None => {
            let has_next = next_steps(src);
            Ok(agent_create_plan(&agent_from(values, &ops, has_next)?))
        }
        Some(id) => {
            let (n, s, _, cur, has_next) = agent_read(src, id)?;
            let j = agent_from(values, &ops, has_next)?;
            let me = me(src);
            let owner = s.get(0, "USERNAME");
            if !me.manages(owner) {
                return Err(Error::Unsupported(NOT_MINE.into()));
            }
            // SP_CREATE_JOB makes the session's user the owner: a DBA's
            // rename would take the job over.
            if j.name != cur.name && !owns(&me, owner) {
                return Err(bad(RENAME_OWNER));
            }
            Ok(agent_alter_plan(&n, &cur, &j))
        }
    }
}

/// The session's user is `owner` itself (not a DBA acting for it).
fn owns(me: &crate::jobs::dameng::Me, owner: Option<&str>) -> bool {
    matches!((&me.user, owner), (Some(u), Some(o)) if u.eq_ignore_ascii_case(o))
}

pub(super) fn agent_drop_plan(name: &str) -> Plan {
    let mut p = Plan::new(&format!(
        "Borra el job «{name}» de DM, con sus pasos y programaciones"
    ));
    p.atomic = true;
    p.exec(format!("CALL SP_DROP_JOB({})", lit(name)));
    p
}

/// Deletes the history rows of job `name` (the job system has no
/// procedure for it): its steps' and its own.
pub(super) fn history_plan(name: &str) -> Plan {
    let mut p = Plan::new(&format!("Borra el historial del job «{name}» de DM"));
    p.atomic = true;
    p.exec(format!(
        "DELETE FROM SYSJOB.SYSSTEPHISTORIES2 WHERE NAME = {}",
        lit(name)
    ));
    p.exec(format!(
        "DELETE FROM SYSJOB.SYSJOBHISTORIES2 WHERE NAME = {}",
        lit(name)
    ));
    p
}

// ------------------------------------------------------------------ operators

#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Operator {
    pub(super) name: String,
    pub(super) enabled: bool,
    pub(super) email: Option<String>,
    pub(super) netsend: Option<String>,
}

fn operator_read(src: &mut dyn JobSource, id: &str) -> Result<(u64, Operator)> {
    let n = int_id(id)?;
    let s = src.query(
        "SELECT * FROM SYSJOB.SYSOPERATORS WHERE ID = ?",
        &[&n.to_string()],
    )?;
    if s.rows.is_empty() {
        return Err(not_found(id));
    }
    Ok((
        n,
        Operator {
            name: text_col(&s, 0, "NAME"),
            enabled: int(s.get(0, "ENABLE")) != Some(0),
            email: crate::jobs::get(&s, 0, OP_EMAIL).map(str::to_string),
            netsend: crate::jobs::get(&s, 0, OP_NETSEND).map(str::to_string),
        },
    ))
}

pub(super) fn operator_sections(existing: bool) -> Vec<FormSection> {
    vec![FormSection::new("general", "General").fields(vec![
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .read_only(existing),
        FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
        FormField::new("email", "Correo", FormKind::Text),
        FormField::new("netsend", "Dirección IP para net send", FormKind::Text),
    ])]
}

fn operator_values(o: &Operator) -> BTreeMap<String, Value> {
    values(vec![
        ("name", json!(o.name)),
        ("enabled", json!(o.enabled)),
        ("email", jtext(o.email.as_deref())),
        ("netsend", jtext(o.netsend.as_deref())),
    ])
}

pub(super) fn operator_from(values: &FormValues) -> Result<Operator> {
    let v = Vals(&values.values);
    let o = Operator {
        name: required(v, "name", "Nombre", 128)?,
        enabled: v.bool_or("enabled", true),
        email: optional(v, "email", "Correo", 256)?,
        netsend: optional(v, "netsend", "Dirección IP para net send", 128)?,
    };
    if o.email
        .as_deref()
        .is_some_and(|m| !m.contains('@') || m.contains(' '))
    {
        return Err(bad("«Correo» no es una dirección de correo."));
    }
    Ok(o)
}

fn operator_args(o: &Operator) -> String {
    format!(
        "{}, {}, {}, {}",
        lit(&o.name),
        flag(o.enabled),
        arg(o.email.as_deref()),
        arg(o.netsend.as_deref())
    )
}

pub(super) fn operator_plan_of(cur: Option<(u64, &Operator)>, o: &Operator) -> Result<Plan> {
    let Some((n, cur)) = cur else {
        let mut p = Plan::new(&format!("Operador «{}» de DM", o.name));
        p.atomic = true;
        p.exec(format!("CALL SP_CREATE_OPERATOR({})", operator_args(o)));
        p.lookup = Some(by_name("SYSJOB.SYSOPERATORS", o.name.clone()));
        return Ok(p);
    };
    if o.name != cur.name {
        return Err(bad(NO_RENAME_OBJECT));
    }
    if o == cur {
        return Ok(Plan::nothing(&cur.name, n.to_string()));
    }
    let mut p = Plan::new(&format!("Operador «{}» de DM", cur.name));
    p.atomic = true;
    p.exec(format!("CALL SP_ALTER_OPERATOR({})", operator_args(o)));
    p.id = Some(n.to_string());
    Ok(p)
}

fn by_name(table: &'static str, name: String) -> Lookup {
    Box::new(move |src: &mut dyn JobSource| {
        let s = src.query(&format!("SELECT ID FROM {table} WHERE NAME = ?"), &[&name])?;
        Ok(int(s.get(0, "ID")).map(|n| n.to_string()))
    })
}

/// The jobs and alerts that send notices to operator `n`.
fn operator_users(src: &mut dyn JobSource, n: u64) -> Vec<String> {
    let id = n.to_string();
    let jobs = best_effort(
        src,
        "SELECT NAME FROM SYSJOB.SYSJOBS WHERE EMAIL_OPERID = ? OR NETSEND_OPERID = ?",
        &[&id, &id],
    );
    let mut out: Vec<String> = (0..jobs.rows.len())
        .filter_map(|r| jobs.get(r, "NAME").map(|n| format!("job «{n}»")))
        .collect();
    let alerts = best_effort(src, "SELECT * FROM SYSJOB.SYSALERTNOTIFICATIONS", &[]);
    let names = best_effort(src, "SELECT ID, NAME FROM SYSJOB.SYSALERTS", &[]);
    for r in 0..alerts.rows.len() {
        if crate::jobs::get(&alerts, r, NO_OPERID) == Some(id.as_str()) {
            if let Some(a) = alerts
                .get(r, "ALERTID")
                .and_then(|a| rows_of(&names, "ID", a).next())
                .and_then(|i| names.get(i, "NAME"))
            {
                out.push(format!("alerta «{a}»"));
            }
        }
    }
    out
}

// ------------------------------------------------------------------ alerts

#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Alert {
    pub(super) name: String,
    pub(super) enabled: bool,
    pub(super) kind: i64,
    pub(super) err_type: i64,
    pub(super) err_code: i64,
    pub(super) sql_type: i64,
    pub(super) delay: i64,
    pub(super) description: String,
    /// Operator name → (by mail, by net send).
    pub(super) notify: BTreeMap<String, (bool, bool)>,
}

fn alert_read(src: &mut dyn JobSource, id: &str) -> Result<(u64, Alert)> {
    let n = int_id(id)?;
    let s = src.query(
        "SELECT * FROM SYSJOB.SYSALERTS WHERE ID = ?",
        &[&n.to_string()],
    )?;
    if s.rows.is_empty() {
        return Err(not_found(id));
    }
    let ops = operators(src);
    let no = best_effort(
        src,
        "SELECT * FROM SYSJOB.SYSALERTNOTIFICATIONS WHERE ALERTID = ?",
        &[&n.to_string()],
    );
    let notify = (0..no.rows.len())
        .filter_map(|r| {
            let op = crate::jobs::get(&no, r, NO_OPERID)?;
            let name = ops.iter().find(|(i, _)| i == op)?.1.clone();
            Some((
                name,
                (
                    int(crate::jobs::get(&no, r, NO_EMAIL)) == Some(1),
                    int(crate::jobs::get(&no, r, NO_NETSEND)) == Some(1),
                ),
            ))
        })
        .collect();
    let g = |cols: &[&str]| int(crate::jobs::get(&s, 0, cols)).unwrap_or(0);
    Ok((
        n,
        Alert {
            name: text_col(&s, 0, "NAME"),
            enabled: int(s.get(0, "ENABLE")) != Some(0),
            kind: int(s.get(0, "TYPE")).unwrap_or(0),
            err_type: g(AL_ERRTYPE),
            err_code: g(AL_ERRCODE),
            sql_type: g(AL_SQLTYPE),
            delay: g(AL_DELAY),
            description: text_col(&s, 0, "DESCRIBE"),
            notify,
        },
    ))
}

pub(super) fn alert_sections(existing: bool) -> Vec<FormSection> {
    vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("name", "Nombre", FormKind::Text)
                .required()
                .read_only(existing),
            FormField::new("enabled", "Habilitada", FormKind::Bool).default(json!(true)),
            FormField::new("description", "Descripción", FormKind::Text),
        ]),
        FormSection::new("condition", "Condición").fields(vec![
            FormField::new(
                "type",
                "Tipo de alerta (TYPE)",
                FormKind::number(Some(0.0), None),
            )
            .default(json!(0))
            .help(ALERT_CODES),
            FormField::new(
                "err_type",
                "Tipo de error (ERRTYPE)",
                FormKind::number(None, None),
            )
            .default(json!(0)),
            FormField::new("err_code", "Código de error", FormKind::number(None, None))
                .default(json!(0))
                .help("Los códigos de error de DM son negativos."),
            FormField::new(
                "sql_type",
                "Tipo de sentencia (SQL_TYPE)",
                FormKind::number(None, None),
            )
            .default(json!(0)),
            FormField::new(
                "delay",
                "Espera entre avisos (segundos)",
                FormKind::number(Some(0.0), None),
            )
            .default(json!(0)),
        ]),
        FormSection::new("notify", "Avisos").list(FormList::new(
            "notify",
            "Operadores que reciben el aviso",
            "operator",
            vec![
                FormField::new(
                    "operator",
                    "Operador",
                    FormKind::reference("operator", false),
                )
                .required(),
                FormField::new("email", "Por correo", FormKind::Bool).default(json!(true)),
                FormField::new("netsend", "Por net send", FormKind::Bool).default(json!(false)),
            ],
        )),
    ]
}

fn alert_values(a: &Alert) -> BTreeMap<String, Value> {
    values(vec![
        ("name", json!(a.name)),
        ("enabled", json!(a.enabled)),
        ("description", json!(a.description)),
        ("type", json!(a.kind)),
        ("err_type", json!(a.err_type)),
        ("err_code", json!(a.err_code)),
        ("sql_type", json!(a.sql_type)),
        ("delay", json!(a.delay)),
    ])
}

fn alert_rows(a: &Alert) -> Vec<BTreeMap<String, Value>> {
    a.notify
        .iter()
        .map(|(op, (mail, net))| {
            values(vec![
                ("operator", json!(op)),
                ("email", json!(mail)),
                ("netsend", json!(net)),
            ])
        })
        .collect()
}

pub(super) fn alert_from(values: &FormValues, ops: &[(String, String)]) -> Result<Alert> {
    let v = Vals(&values.values);
    let big = i64::from(i32::MAX);
    let mut a = Alert {
        name: required(v, "name", "Nombre", 128)?,
        enabled: v.bool_or("enabled", true),
        kind: v.int("type", "Tipo de alerta (TYPE)", 0, big)?.unwrap_or(0),
        err_type: v
            .int("err_type", "Tipo de error (ERRTYPE)", -big, big)?
            .unwrap_or(0),
        err_code: v
            .int("err_code", "Código de error", -big, big)?
            .unwrap_or(0),
        sql_type: v
            .int("sql_type", "Tipo de sentencia (SQL_TYPE)", -big, big)?
            .unwrap_or(0),
        delay: v
            .int("delay", "Espera entre avisos (segundos)", 0, big)?
            .unwrap_or(0),
        description: optional(v, "description", "Descripción", 500)?.unwrap_or_default(),
        notify: BTreeMap::new(),
    };
    for (i, row) in rows(values, "notify").into_iter().enumerate() {
        let op = operator_pick(
            row,
            "operator",
            &format!("Aviso {}: «Operador»", i + 1),
            ops,
        )?
        .ok_or_else(|| bad(format!("Falta Aviso {}: «Operador».", i + 1)))?;
        let pair = (row.bool_or("email", true), row.bool_or("netsend", false));
        if a.notify.insert(op.clone(), pair).is_some() {
            return Err(bad(format!("El operador «{}» está repetido.", short(&op))));
        }
    }
    Ok(a)
}

fn alert_args(a: &Alert) -> String {
    format!(
        "{}, {}, {}, {}, {}, {}, {}, {}",
        lit(&a.name),
        flag(a.enabled),
        a.kind,
        a.err_type,
        a.err_code,
        a.sql_type,
        a.delay,
        lit(&a.description),
    )
}

fn add_notice(name: &str, op: &str, (mail, net): (bool, bool)) -> String {
    format!(
        "CALL SP_ALERT_ADD_OPERATOR({}, {}, {}, {})",
        lit(name),
        lit(op),
        flag(mail),
        flag(net)
    )
}

pub(super) fn alert_plan_of(cur: Option<(u64, &Alert)>, a: &Alert) -> Result<Plan> {
    let Some((n, cur)) = cur else {
        let mut p = Plan::new(&format!("Alerta «{}» de DM", a.name));
        p.atomic = true;
        p.exec(format!("CALL SP_CREATE_ALERT({})", alert_args(a)));
        for (op, how) in &a.notify {
            p.exec(add_notice(&a.name, op, *how));
        }
        p.lookup = Some(by_name("SYSJOB.SYSALERTS", a.name.clone()));
        return Ok(p);
    };
    if a.name != cur.name {
        return Err(bad(NO_RENAME_OBJECT));
    }
    let mut body = Vec::new();
    if alert_args(a) != alert_args(cur) {
        body.push(format!("CALL SP_ALTER_ALERT({})", alert_args(a)));
    }
    for (op, how) in &cur.notify {
        if a.notify.get(op) != Some(how) {
            body.push(format!(
                "CALL SP_ALERT_DROP_OPERATOR({}, {})",
                lit(&cur.name),
                lit(op)
            ));
        }
    }
    for (op, how) in &a.notify {
        if cur.notify.get(op) != Some(how) {
            body.push(add_notice(&cur.name, op, *how));
        }
    }
    if body.is_empty() {
        return Ok(Plan::nothing(&cur.name, n.to_string()));
    }
    let mut p = Plan::new(&format!("Alerta «{}» de DM", cur.name));
    p.atomic = true;
    for b in body {
        p.exec(b);
    }
    p.id = Some(n.to_string());
    Ok(p)
}

// ------------------------------------------------------------------ dispatch

fn dba(me: &Me) -> Result<()> {
    if me.dba {
        Ok(())
    } else {
        Err(Error::Unsupported(DBA_ONLY.into()))
    }
}

pub(super) fn form(src: &mut dyn JobSource, k: Kind, id: Option<&str>) -> Result<ObjectForm> {
    match (k, id) {
        (Kind::Job, None) => agent_form(src, None),
        (Kind::DbmsJob, None) => dameng_dbms::dbms_job_form(src, None),
        (Kind::SchedulerJob, None) => dameng_dbms::scheduler_form(src, None),
        (Kind::Job | Kind::DbmsJob | Kind::SchedulerJob, Some(id)) => match flavor(src, id)? {
            Flavor::Agent => agent_form(src, Some(id)),
            Flavor::DbmsJob => dameng_dbms::dbms_job_form(src, Some(id)),
            Flavor::Scheduler => dameng_dbms::scheduler_form(src, Some(id)),
        },
        (Kind::Operator, id) => {
            let me = me(src);
            let (o, n) = match id {
                Some(id) => {
                    let (n, o) = operator_read(src, id)?;
                    (o, Some(n))
                }
                None => (
                    Operator {
                        enabled: true,
                        ..Operator::default()
                    },
                    None,
                ),
            };
            let mut v = operator_values(&o);
            let mut sections = operator_sections(n.is_some());
            if let Some(n) = n {
                sections.push(FormSection::new("state", "Estado").fields(vec![
                    FormField::new("id", "Id", FormKind::Text).read_only(true),
                    FormField::new("used_by", "Lo usan", FormKind::Text).read_only(true),
                ]));
                v.insert("id".into(), json!(n.to_string()));
                v.insert("used_by".into(), json!(operator_users(src, n).join(", ")));
            }
            Ok(ObjectForm {
                sections,
                values: v,
                choices: BTreeMap::new(),
                notes: if me.dba {
                    Vec::new()
                } else {
                    vec![DBA_ONLY.to_string()]
                },
                read_only: !me.dba,
            })
        }
        (Kind::Alert, id) => {
            let me = me(src);
            let ops = operators(src);
            let (a, n) = match id {
                Some(id) => {
                    let (n, a) = alert_read(src, id)?;
                    (a, Some(n))
                }
                None => (
                    Alert {
                        enabled: true,
                        ..Alert::default()
                    },
                    None,
                ),
            };
            let mut sections = alert_sections(n.is_some());
            for s in &mut sections {
                for l in &mut s.lists {
                    l.rows = alert_rows(&a);
                }
            }
            let mut v = alert_values(&a);
            if let Some(n) = n {
                sections.push(
                    FormSection::new("state", "Estado")
                        .field(FormField::new("id", "Id", FormKind::Text).read_only(true)),
                );
                v.insert("id".into(), json!(n.to_string()));
            }
            let mut notes = vec![ALERT_CODES.to_string()];
            if !me.dba {
                notes.insert(0, DBA_ONLY.to_string());
            }
            Ok(ObjectForm {
                sections,
                values: v,
                choices: BTreeMap::from([("operator".to_string(), operator_choices(&ops))]),
                notes,
                read_only: !me.dba,
            })
        }
    }
}

pub(super) fn plan(
    src: &mut dyn JobSource,
    k: Kind,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    match (k, id) {
        (Kind::Job, None) => agent_plan(src, None, values),
        (Kind::DbmsJob, None) => dameng_dbms::dbms_job_plan(src, None, values),
        (Kind::SchedulerJob, None) => dameng_dbms::scheduler_plan(src, None, values),
        (Kind::Job | Kind::DbmsJob | Kind::SchedulerJob, Some(id)) => match flavor(src, id)? {
            Flavor::Agent => agent_plan(src, Some(id), values),
            Flavor::DbmsJob => dameng_dbms::dbms_job_plan(src, Some(id), values),
            Flavor::Scheduler => dameng_dbms::scheduler_plan(src, Some(id), values),
        },
        (Kind::Operator, id) => {
            dba(&me(src))?;
            let o = operator_from(values)?;
            match id {
                None => operator_plan_of(None, &o),
                Some(id) => {
                    let (n, cur) = operator_read(src, id)?;
                    operator_plan_of(Some((n, &cur)), &o)
                }
            }
        }
        (Kind::Alert, id) => {
            dba(&me(src))?;
            let ops = operators(src);
            let a = alert_from(values, &ops)?;
            match id {
                None => alert_plan_of(None, &a),
                Some(id) => {
                    let (n, cur) = alert_read(src, id)?;
                    alert_plan_of(Some((n, &cur)), &a)
                }
            }
        }
    }
}

pub(super) fn delete_plan(src: &mut dyn JobSource, k: Kind, id: &str) -> Result<Plan> {
    match k {
        Kind::Job | Kind::DbmsJob | Kind::SchedulerJob => match flavor(src, id)? {
            Flavor::Agent => {
                let (_, s, _, j, _) = agent_read(src, id)?;
                if !me(src).manages(s.get(0, "USERNAME")) {
                    return Err(Error::Unsupported(NOT_MINE.into()));
                }
                Ok(agent_drop_plan(&j.name))
            }
            Flavor::DbmsJob => dameng_dbms::dbms_job_delete_plan(src, id),
            Flavor::Scheduler => dameng_dbms::scheduler_delete_plan(src, id),
        },
        Kind::Operator => {
            dba(&me(src))?;
            let (n, o) = operator_read(src, id)?;
            let used = operator_users(src, n);
            if !used.is_empty() {
                return Err(bad(format!(
                    "Lo usan {}: sacalo de ahí antes de borrarlo.",
                    used.join(", ")
                )));
            }
            let mut p = Plan::new(&format!("Borra el operador «{}» de DM", o.name));
            p.atomic = true;
            p.exec(format!("CALL SP_DROP_OPERATOR({})", lit(&o.name)));
            Ok(p)
        }
        Kind::Alert => {
            dba(&me(src))?;
            let (_, a) = alert_read(src, id)?;
            let mut p = Plan::new(&format!("Borra la alerta «{}» de DM", a.name));
            p.atomic = true;
            p.exec(format!("CALL SP_DROP_ALERT({})", lit(&a.name)));
            Ok(p)
        }
    }
}

pub(super) fn clear_plan(src: &mut dyn JobSource, id: &str) -> Result<Plan> {
    match flavor(src, id)? {
        Flavor::Scheduler => dameng_dbms::scheduler_clear_plan(src, id),
        Flavor::Agent | Flavor::DbmsJob => {
            let (s, t, _) = one(src, id)?;
            if t.own {
                return Err(Error::Unsupported(NO_HISTORY_ACCESS.into()));
            }
            if !me(src).manages(s.get(0, "USERNAME")) {
                return Err(Error::Unsupported(NOT_MINE.into()));
            }
            Ok(history_plan(s.get(0, "NAME").unwrap_or_default()))
        }
    }
}
