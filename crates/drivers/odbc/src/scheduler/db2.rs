//! Db2 LUW's Administrative Task Scheduler (ATS), managed whole: a task
//! calls a stored procedure on a UNIX cron schedule between a begin and an
//! end time, up to a number of runs. `SYSPROC.ADMIN_TASK_ADD` creates it,
//! `ADMIN_TASK_UPDATE` changes its schedule, end, maximum of runs and
//! remarks in place, and `ADMIN_TASK_REMOVE` removes it (with an end time,
//! only its status records up to then: the history). The name, the
//! procedure and its input query can't be changed: the task is removed and
//! added again, in one transaction, and its status records and run count
//! start over. `SYSTOOLS.ADMIN_TASK_STATUS` is the scheduler's log.
//!
//! Both procedures refuse a begin or an end time in the past; a begin that
//! passed and is kept goes as NULL (the task may run from now on, the
//! same thing). Db2 shows each user only the tasks they created
//! (`ADMIN_TASK_LIST` is a view on the owner), so those are the ones
//! changed here.

use super::*;
use crate::jobs::db2::{tokens, ATS_TABLES, NOW, NO_ATS, OWN_ONLY};
use crate::jobs::{code, int, int_id, lit, not_found, parse_ts, Tz};
use dbine_driver::{FormField, FormKind, FormSection, SchedulerKind};
use serde_json::json;

const RECREATE: &str = "Db2 no cambia el nombre, el procedimiento ni la consulta de parámetros de una tarea: para cambiarlos se borra y se crea de nuevo, en una transacción. Sus registros de estado (el historial) se pierden, la cuenta de ejecuciones vuelve a cero y la tarea recibe otro id.";
const BEGIN_PAST: &str =
    "«Inicio» ya pasó: Db2 no acepta un inicio en el pasado. Dejalo vacío para que corra desde ya.";
const END_PAST: &str =
    "«Fin» ya pasó: Db2 no acepta un fin en el pasado. Poné uno futuro o dejalo vacío.";
const END_BEFORE: &str = "«Fin» tiene que ser posterior a «Inicio».";

/// A task as `SYSTOOLS.ADMIN_TASK_LIST` has it. Times are the server's
/// local time in ms ([`parse_ts`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Task {
    pub(super) name: String,
    pub(super) begin: Option<i64>,
    pub(super) end: Option<i64>,
    pub(super) max: Option<i64>,
    pub(super) schedule: Option<String>,
    pub(super) schema: String,
    pub(super) procedure: String,
    pub(super) input: Option<String>,
    pub(super) options: Option<String>,
    pub(super) remarks: Option<String>,
}

pub(super) fn kind(kind: &str) -> Result<()> {
    (kind.trim() == "job")
        .then_some(())
        .ok_or_else(|| bad_kind(kind))
}

/// Whether the ATS views exist yet (the first ADMIN_TASK_ADD creates them).
fn has_views(src: &mut dyn JobSource) -> (bool, bool) {
    match src.query(ATS_TABLES, &[]) {
        Ok(s) => {
            let has = |n: &str| (0..s.rows.len()).any(|r| s.at(r, 0) == Some(n));
            (has("ADMIN_TASK_LIST"), has("ADMIN_TASK_STATUS"))
        }
        Err(e) => {
            tracing::debug!("odbc scheduler: SYSCAT.TABLES failed: {e}");
            (true, true)
        }
    }
}

pub(super) fn info(src: &mut dyn JobSource) -> Result<SchedulerInfo> {
    let (list, status) = has_views(src);
    let mut notes = Vec::new();
    if !list {
        notes.push(NO_ATS.to_string());
    }
    if let Some(n) = crate::jobs::db2::ats_off(src) {
        notes.push(n.to_string());
    }
    notes.push(OWN_ONLY.to_string());
    Ok(SchedulerInfo {
        kinds: vec![SchedulerKind::new("job", "Tareas", "tarea").abilities(true, true, true)],
        logs: status,
        notes,
    })
}

fn task_of(s: &Set, r: usize) -> Task {
    let t = |c: &str| s.get(r, c).map(str::to_string);
    Task {
        name: t("NAME").unwrap_or_default(),
        begin: s.get(r, "BEGIN_TIME").and_then(parse_ts),
        end: s.get(r, "END_TIME").and_then(parse_ts),
        max: int(s.get(r, "MAX_INVOCATIONS")),
        schedule: t("SCHEDULE"),
        schema: t("PROCEDURE_SCHEMA").unwrap_or_default(),
        procedure: t("PROCEDURE_NAME").unwrap_or_default(),
        input: t("PROCEDURE_INPUT"),
        options: t("OPTIONS"),
        remarks: t("REMARKS"),
    }
}

/// The task `id` (a TASKID) and its row.
fn read(src: &mut dyn JobSource, id: &str) -> Result<(u64, Task, Set)> {
    let n = int_id(id)?;
    let s = src.query(
        &format!("SELECT T.* FROM SYSTOOLS.ADMIN_TASK_LIST T WHERE T.TASKID = {n}"),
        &[],
    )?;
    if s.rows.is_empty() {
        return Err(not_found(id));
    }
    Ok((n, task_of(&s, 0), s))
}

pub(super) fn values_of(t: &Task) -> BTreeMap<String, Value> {
    values(vec![
        ("name", json!(t.name)),
        ("remarks", jtext(t.remarks.as_deref())),
        ("procedure_schema", json!(t.schema)),
        ("procedure_name", json!(t.procedure)),
        ("procedure_input", jtext(t.input.as_deref())),
        ("schedule", jtext(t.schedule.as_deref())),
        (
            "begin",
            t.begin.map_or(Value::Null, |ms| json!(dt_value(ms))),
        ),
        ("end", t.end.map_or(Value::Null, |ms| json!(dt_value(ms)))),
        ("max_invocations", t.max.map_or(Value::Null, |n| json!(n))),
        ("options", jtext(t.options.as_deref())),
    ])
}

pub(super) fn sections(existing: bool) -> Vec<FormSection> {
    let mut out = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("name", "Nombre", FormKind::Text)
                .required()
                .help("Único en la base, hasta 128 caracteres. Cambiarlo vuelve a crear la tarea."),
            FormField::new("remarks", "Comentario", FormKind::Textarea)
                .help("Hasta 254 bytes."),
        ]),
        FormSection::new("procedure", "Procedimiento").fields(vec![
            FormField::new("procedure_schema", "Esquema", FormKind::Text)
                .required()
                .default(json!("SYSPROC")),
            FormField::new("procedure_name", "Procedimiento", FormKind::Text)
                .required()
                .placeholder("ADMIN_CMD"),
            FormField::new(
                "procedure_input",
                "Consulta de parámetros",
                FormKind::code("sql"),
            )
            .help("Un SELECT o VALUES que devuelve una fila con los argumentos de entrada del procedimiento. Vacío: el procedimiento no recibe argumentos.")
            .placeholder("VALUES ('RUNSTATS ON TABLE APP.VENTAS')"),
        ]),
        FormSection::new("schedule", "Programación").fields(vec![
            FormField::new("schedule", "Calendario", FormKind::Cron)
                .help("Formato cron de UNIX: minuto, hora, día del mes, mes y día de la semana.")
                .placeholder("0 3 * * *"),
            FormField::new("begin", "Inicio", FormKind::DateTime)
                .help("La primera vez que puede correr, en la hora del servidor. Vacío: desde ya."),
            FormField::new("end", "Fin", FormKind::DateTime)
                .help("La última vez que puede empezar. Vacío: sin fin."),
            FormField::new(
                "max_invocations",
                "Máximo de ejecuciones",
                FormKind::number(Some(0.0), Some(f64::from(i32::MAX))),
            )
            .help("Vacío: sin límite. Con 0 no corre."),
        ]),
        FormSection::new("advanced", "Avanzado").field(
            FormField::new("options", "Opciones", FormKind::Text)
                .read_only(true)
                .help("Db2 reserva este argumento de ADMIN_TASK_ADD: va siempre vacío."),
        ),
    ];
    if existing {
        out.push(FormSection::new("state", "Estado").fields(vec![
            FormField::new("taskid", "Id de la tarea", FormKind::Text).read_only(true),
            FormField::new("owner", "Dueño", FormKind::Text).read_only(true),
            FormField::new("owner_type", "Tipo de dueño", FormKind::Text).read_only(true),
            FormField::new("updated", "Modificada", FormKind::Text).read_only(true),
            FormField::new("invocations", "Ejecuciones", FormKind::Text).read_only(true),
            FormField::new("last_status", "Último estado", FormKind::Text).read_only(true),
        ]));
    }
    out
}

pub(super) fn form(src: &mut dyn JobSource, id: Option<&str>) -> Result<ObjectForm> {
    let Some(id) = id else {
        let t = Task {
            schema: "SYSPROC".into(),
            ..Task::default()
        };
        return Ok(ObjectForm {
            sections: sections(false),
            values: values_of(&t),
            ..ObjectForm::default()
        });
    };
    let (n, t, s) = read(src, id)?;
    let mut v = values_of(&t);
    v.insert("taskid".into(), json!(n.to_string()));
    for (k, c) in [
        ("owner", "OWNER"),
        ("owner_type", "OWNERTYPE"),
        ("updated", "UPDATE_TIME"),
    ] {
        v.insert(k.into(), jtext(s.get(0, c)));
    }
    let status = src
        .query(
            &format!(
                "SELECT COUNT(*) AS N, MAX(INVOCATION) AS LAST_INVOCATION FROM SYSTOOLS.ADMIN_TASK_STATUS WHERE TASKID = {n}"
            ),
            &[],
        )
        .unwrap_or_else(|e| {
            tracing::debug!("odbc scheduler: ADMIN_TASK_STATUS failed: {e}");
            Set::default()
        });
    v.insert(
        "invocations".into(),
        jtext(s.get(0, "INVOCATIONS").or(status.get(0, "LAST_INVOCATION"))),
    );
    let last = src
        .query(
            &format!(
                "SELECT S.STATUS, S.SQLCODE FROM SYSTOOLS.ADMIN_TASK_STATUS S WHERE S.TASKID = {n} ORDER BY S.BEGIN_TIME DESC, S.INVOCATION DESC FETCH FIRST 1 ROW ONLY"
            ),
            &[],
        )
        .unwrap_or_default();
    let last_status = last
        .get(0, "STATUS")
        .map(|st| match int(last.get(0, "SQLCODE")) {
            Some(c) if c != 0 => format!("{st} (SQLCODE {c})"),
            _ => st.to_string(),
        });
    v.insert("last_status".into(), jtext(last_status.as_deref()));
    Ok(ObjectForm {
        sections: sections(true),
        values: v,
        notes: vec![RECREATE.to_string()],
        ..ObjectForm::default()
    })
}

/// The task the form describes, checked; `options` is the server's.
pub(super) fn from_values(values: &FormValues) -> Result<Task> {
    let v = Vals(&values.values);
    let name = required(v, "name", "Nombre", 128)?;
    let remarks = optional(v, "remarks", "Comentario", 254)?;
    if remarks.as_deref().is_some_and(|r| r.len() > 254) {
        return Err(bad("«Comentario» admite hasta 254 bytes."));
    }
    let schema = required(v, "procedure_schema", "Esquema", 128)?;
    let procedure = required(v, "procedure_name", "Procedimiento", 128)?;
    // A string constant is at most 32 672 bytes in Db2.
    let input = code_text(v, "procedure_input", "Consulta de parámetros", 32_672)?;
    let schedule = cron(v, "schedule", "Calendario", 5..=5)?;
    let begin = datetime(v, "begin", "Inicio")?;
    let end = datetime(v, "end", "Fin")?;
    if let (Some(b), Some(e)) = (begin, end) {
        if e < b {
            return Err(bad(END_BEFORE));
        }
    }
    let max = v.int(
        "max_invocations",
        "Máximo de ejecuciones",
        0,
        i64::from(i32::MAX),
    )?;
    Ok(Task {
        name,
        begin,
        end,
        max,
        schedule,
        schema,
        procedure,
        input,
        options: None,
        remarks,
    })
}

/// `YYYY-MM-DD-HH.MM.SS.000000` as a TIMESTAMP, or NULL.
fn ts_arg(ms: Option<i64>) -> String {
    ms.map(|ms| format!("TIMESTAMP('{}')", crate::jobs::db2::db2_ts(ms)))
        .unwrap_or_else(|| "NULL".into())
}

fn text_arg(v: Option<&str>) -> String {
    v.map(lit).unwrap_or_else(|| "NULL".into())
}

fn int_arg(v: Option<i64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "NULL".into())
}

/// The begin and end arguments: a begin that passed and the user kept
/// (`kept`) goes as NULL with a comment; one the user set in the past and
/// any end in the past are refused.
fn window(
    plan: &mut Plan,
    t: &Task,
    kept_begin: Option<i64>,
    now: Option<i64>,
) -> Result<(String, String)> {
    let past = |ms: Option<i64>| matches!((ms, now), (Some(t), Some(n)) if t <= n);
    let begin = if past(t.begin) {
        if t.begin != kept_begin {
            return Err(bad(BEGIN_PAST));
        }
        plan.comment(&format!(
            "Inicio original: {}. Db2 no acepta un inicio pasado: sin él, la tarea corre desde ya según su calendario.",
            crate::jobs::plain(t.begin.unwrap_or_default())
        ));
        "NULL".to_string()
    } else {
        ts_arg(t.begin)
    };
    if past(t.end) {
        return Err(bad(END_PAST));
    }
    Ok((begin, ts_arg(t.end)))
}

fn call(proc_name: &str, args: &[String]) -> String {
    format!("CALL SYSPROC.{proc_name}(\n    {}\n)", args.join(",\n    "))
}

/// `ADMIN_TASK_ADD` for `t` into `plan`.
pub(super) fn add(
    plan: &mut Plan,
    t: &Task,
    kept_begin: Option<i64>,
    now: Option<i64>,
) -> Result<()> {
    let (begin, end) = window(plan, t, kept_begin, now)?;
    plan.exec(call(
        "ADMIN_TASK_ADD",
        &[
            lit(&t.name),
            begin,
            end,
            int_arg(t.max),
            text_arg(t.schedule.as_deref()),
            lit(&t.schema),
            lit(&t.procedure),
            text_arg(t.input.as_deref()),
            text_arg(t.options.as_deref()),
            text_arg(t.remarks.as_deref()),
        ],
    ));
    Ok(())
}

/// The new task's TASKID, by its name (unique in the database).
fn lookup(name: String) -> Lookup {
    Box::new(move |src: &mut dyn JobSource| {
        let s = src.query(
            "SELECT TASKID FROM SYSTOOLS.ADMIN_TASK_LIST WHERE NAME = ?",
            &[&name],
        )?;
        Ok(int(s.get(0, "TASKID")).map(|n| n.to_string()))
    })
}

pub(super) fn create_plan(t: &Task, now: Option<i64>) -> Result<Plan> {
    let mut p = Plan::new(&format!(
        "Tarea «{}» del planificador de tareas de Db2",
        t.name
    ));
    add(&mut p, t, None, now)?;
    p.atomic = true;
    p.lookup = Some(lookup(t.name.clone()));
    Ok(p)
}

/// What changes `cur` into `t`: ADMIN_TASK_UPDATE in place, or remove and
/// add when the name, procedure or input change.
pub(super) fn alter_plan(id: u64, cur: &Task, t: &Task, now: Option<i64>) -> Result<Plan> {
    let mut t = t.clone();
    t.options.clone_from(&cur.options);
    let recreate = t.name != cur.name
        || t.schema != cur.schema
        || t.procedure != cur.procedure
        || !same(&t.input, &cur.input);
    let changed = t.begin != cur.begin
        || t.end != cur.end
        || t.max != cur.max
        || !same(&t.schedule, &cur.schedule)
        || !same(&t.remarks, &cur.remarks);
    if !recreate && !changed {
        return Ok(Plan::nothing(&cur.name, id.to_string()));
    }
    let mut p = Plan::new(&format!(
        "Tarea «{}» del planificador de tareas de Db2",
        cur.name
    ));
    p.atomic = true;
    if recreate {
        for line in RECREATE.split(". ").filter(|l| !l.is_empty()) {
            p.comment(line.trim_end_matches('.'));
        }
        p.exec(call("ADMIN_TASK_REMOVE", &[lit(&cur.name), "NULL".into()]));
        add(&mut p, &t, cur.begin, now)?;
        p.lookup = Some(lookup(t.name.clone()));
        return Ok(p);
    }
    let (begin, end) = window(&mut p, &t, cur.begin, now)?;
    p.exec(call(
        "ADMIN_TASK_UPDATE",
        &[
            lit(&t.name),
            begin,
            end,
            int_arg(t.max),
            text_arg(t.schedule.as_deref()),
            text_arg(t.options.as_deref()),
            text_arg(t.remarks.as_deref()),
        ],
    ));
    p.id = Some(id.to_string());
    Ok(p)
}

/// The procedure as the catalog spells it (Db2 keeps unquoted names in
/// capitals), or an error when there is none. When the catalog can't be
/// read, the names go as typed and Db2 decides.
fn resolve_procedure(src: &mut dyn JobSource, t: &mut Task) -> Result<()> {
    let s = match src.query(
        "SELECT ROUTINESCHEMA, ROUTINENAME FROM SYSCAT.ROUTINES WHERE ROUTINETYPE = 'P' AND UPPER(ROUTINESCHEMA) = UPPER(?) AND UPPER(ROUTINENAME) = UPPER(?)",
        &[&t.schema, &t.procedure],
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("odbc scheduler: SYSCAT.ROUTINES failed: {e}");
            return Ok(());
        }
    };
    let found: Vec<(String, String)> = (0..s.rows.len())
        .filter_map(|r| {
            Some((
                s.get(r, "ROUTINESCHEMA")?.to_string(),
                s.get(r, "ROUTINENAME")?.to_string(),
            ))
        })
        .collect();
    let exact = found
        .iter()
        .find(|(a, b)| *a == t.schema && *b == t.procedure);
    match exact.or(found.first()) {
        Some((a, b)) => {
            t.schema.clone_from(a);
            t.procedure.clone_from(b);
            Ok(())
        }
        None => Err(bad(format!(
            "No existe el procedimiento «{}.{}».",
            short(&t.schema),
            short(&t.procedure)
        ))),
    }
}

pub(super) fn plan(src: &mut dyn JobSource, id: Option<&str>, values: &FormValues) -> Result<Plan> {
    let mut t = from_values(values)?;
    let now = Tz::query(src, NOW).now();
    match id {
        None => {
            resolve_procedure(src, &mut t)?;
            create_plan(&t, now)
        }
        Some(id) => {
            let (n, cur, _) = read(src, id)?;
            if t.schema != cur.schema || t.procedure != cur.procedure {
                resolve_procedure(src, &mut t)?;
            }
            alter_plan(n, &cur, &t, now)
        }
    }
}

pub(super) fn drop_plan(name: &str) -> Plan {
    let mut p = Plan::new(&format!(
        "Borra la tarea «{name}» del planificador de tareas de Db2 y sus registros de estado"
    ));
    p.exec(call("ADMIN_TASK_REMOVE", &[lit(name), "NULL".into()]));
    p
}

pub(super) fn delete_plan(src: &mut dyn JobSource, id: &str) -> Result<Plan> {
    let (_, t, _) = read(src, id)?;
    Ok(drop_plan(&t.name))
}

/// ADMIN_TASK_REMOVE with an end time removes only the task's status
/// records that ended before it; one that is running stays.
pub(super) fn clear_history_plan(name: &str) -> Plan {
    let mut p = Plan::new(&format!(
        "Borra el historial (registros de estado terminados) de la tarea «{name}»; la tarea queda"
    ));
    p.exec(call(
        "ADMIN_TASK_REMOVE",
        &[lit(name), "CURRENT TIMESTAMP".into()],
    ));
    p
}

pub(super) fn clear_plan(src: &mut dyn JobSource, id: &str) -> Result<Plan> {
    let (_, t, _) = read(src, id)?;
    Ok(clear_history_plan(&t.name))
}

pub(super) const LOG_SQL: &str = "SELECT S.* FROM SYSTOOLS.ADMIN_TASK_STATUS S ORDER BY S.BEGIN_TIME DESC, S.INVOCATION DESC FETCH FIRST 500 ROWS ONLY";

/// A timestamp as the log shows it: to the second.
fn shown_ts(v: Option<&str>) -> Value {
    jtext(v.map(|t| t.get(..19).unwrap_or(t)))
}

pub(super) fn logs(src: &mut dyn JobSource) -> Result<Vec<MonitorTable>> {
    let s = match src.query(LOG_SQL, &[]) {
        Ok(s) => s,
        Err(e) if code(&e) == Some(-204) => Set::default(),
        Err(e) => return Err(e),
    };
    let mut t = MonitorTable::new(
        "admin_task_status",
        "Ejecuciones de las tareas",
        &[
            "Tarea",
            "Ejecución",
            "Estado",
            "Inicio",
            "Fin",
            "SQLCODE",
            "SQLSTATE",
            "Mensaje",
            "Código de retorno",
        ],
    );
    for r in 0..s.rows.len() {
        t.rows.push(vec![
            jtext(s.get(r, "NAME").or(s.get(r, "TASKID"))),
            jtext(s.get(r, "INVOCATION")),
            jtext(s.get(r, "STATUS")),
            shown_ts(s.get(r, "BEGIN_TIME")),
            shown_ts(s.get(r, "END_TIME")),
            jtext(s.get(r, "SQLCODE")),
            jtext(s.get(r, "SQLSTATE")),
            jtext(
                s.get(r, "SQLERRMC")
                    .map(tokens)
                    .as_deref()
                    .filter(|m| !m.is_empty()),
            ),
            jtext(s.get(r, "RC")),
        ]);
    }
    Ok(vec![t])
}
