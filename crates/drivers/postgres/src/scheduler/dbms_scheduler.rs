//! DBMS_SCHEDULER in the designer, on EDB Postgres Advanced Server and
//! KingbaseES (kdb_schedule): jobs, programs and named schedules, the
//! subset each one documents.
//!
//! - EDB: `CREATE_JOB` (with its own action and timing, or a program and a
//!   schedule), `CREATE_PROGRAM` with `DEFINE_PROGRAM_ARGUMENT`,
//!   `CREATE_SCHEDULE`, `SET_JOB_ARGUMENT_VALUE`, `ENABLE` / `DISABLE`,
//!   `DROP_*` and `DROP_PROGRAM_ARGUMENT`, in one anonymous block. EDB has
//!   no `SET_ATTRIBUTE`: what can't change in place (a job's action or
//!   timing, a program's action, a schedule) is dropped and created again
//!   in the same block (a job loses its run history; a program or a
//!   schedule only while no job uses it). The calls take the bare name,
//!   which EDB resolves in the caller's schema: a user changes only their
//!   own objects, and a job whose name another owner also has isn't
//!   changed.
//! - KingbaseES: `CREATE_JOB` (a program and a schedule only),
//!   `CREATE_PROGRAM`, `CREATE_SCHEDULE`, `ENABLE` / `DISABLE`, `DROP_*`
//!   with `CALL` in `BEGIN` … `COMMIT`; any other change drops and creates
//!   the object again. Its views have no owners: a user changes the jobs
//!   `user_scheduler_jobs` shows them, a superuser any.
//!
//! Repeat intervals are the calendaring syntax both document (`FREQ`,
//! `BYMONTH`, `BYMONTHDAY`, `BYDATE`, `BYDAY`, `BYHOUR`, `BYMINUTE`), built
//! from fields, or a raw expression the server checks.
//!
//! Neither engine was at hand: this follows their documentation, and reads
//! the views through the catalog ([`Catalog`]) so a column a version lacks
//! reads as empty.

use super::{bad, datetime_field, name_of, pairs, plain, shown, valid_datetime, Vals};
use crate::catalog::comment_text;
use crate::jobs::scheduler::{self, bool_lit, Catalog, Dialect, SchedJob, Scope, View};
use crate::jobs::{text, Found, Named, Row};
use crate::session::PgSession;
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result,
    SchedulerItem, SchedulerKind, SchedulerList,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

/// The views read, by lower-case name.
const NAMES: &[&str] = &[
    "dba_scheduler_jobs",
    "all_scheduler_jobs",
    "user_scheduler_jobs",
    "dba_scheduler_programs",
    "all_scheduler_programs",
    "user_scheduler_programs",
    "dba_scheduler_schedules",
    "all_scheduler_schedules",
    "user_scheduler_schedules",
    "dba_scheduler_program_args",
    "all_scheduler_program_args",
    "user_scheduler_program_args",
    "dba_scheduler_job_args",
    "all_scheduler_job_args",
    "user_scheduler_job_args",
];

const NOT_OWN: &str = "EDB busca los objetos de DBMS_SCHEDULER por su nombre, sin su dueño: DBine solo cambia los de tu usuario.";
const SAME_NAME: &str = "Hay más de un job de DBMS_SCHEDULER con este nombre: EDB los busca por su nombre, sin su dueño, y podría cambiar otro.";
const KDB_OTHERS: &str =
    "Este job no es de tu usuario: solo su dueño o un superusuario pueden cambiarlo.";
const JOB_RECREATE: &str = "DBMS_SCHEDULER no cambia la acción, la programación ni las opciones de un job ya creado (no tiene SET_ATTRIBUTE): al guardarlas el job se borra y se crea de nuevo, y su historial de ejecuciones queda con el anterior. Habilitarlo y sus argumentos cambian en el lugar.";
const KDB_JOB_RECREATE: &str = "KingbaseES no cambia un job de DBMS_SCHEDULER ya creado: al guardar cualquier cambio salvo «Habilitado» el job se borra y se crea de nuevo, y su historial de ejecuciones queda con el anterior.";
const RECREATE_UNUSED: &str = "DBMS_SCHEDULER no cambia un programa ni una programación ya creados: al guardar se borran y se crean de nuevo, lo que solo se puede mientras ningún job los usa.";
const NO_ARGS_VIEW: &str = "Este servidor no muestra los argumentos (no tiene la vista *_scheduler_program_args / *_scheduler_job_args): los que tenga no se ven ni se cambian.";
const KDB_PROGRAM_CONN: &str = "La cadena de conexión de un programa de KingbaseES no se lee ni se escribe (puede llevar una contraseña): el programa corre en la base que elijas, como el usuario de kdb_schedule.";

/// What this database has of DBMS_SCHEDULER, and who's asking.
pub(crate) struct Caps {
    pub(crate) d: Dialect,
    pub(crate) user: String,
    pub(crate) superuser: bool,
    pub(crate) database: String,
    pub(crate) cat: Catalog,
    /// KingbaseES: the jobs `user_scheduler_jobs` shows (the user's own).
    pub(crate) own: HashSet<String>,
}

pub(crate) async fn caps(s: &PgSession, f: &Found, d: Dialect) -> Result<Option<Caps>> {
    let cat = Catalog::read(s, d, NAMES).await?;
    if !cat.any("scheduler_jobs") {
        return Ok(None);
    }
    let mut own = HashSet::new();
    if let Some(v) = cat.readable("user_scheduler_jobs") {
        if let Some(col) = v.col("u", "job_name") {
            match s
                .text(&format!(
                    "SELECT DISTINCT {col}::text AS name FROM {} u",
                    v.from()
                ))
                .await
            {
                Ok(rows) => own = rows.iter().filter_map(|r| text(r, "name")).collect(),
                Err(e) => tracing::debug!("DBMS_SCHEDULER: own jobs: {e}"),
            }
        }
    }
    Ok(Some(Caps {
        d,
        user: f.user.clone(),
        superuser: f.superuser,
        database: f.database.clone(),
        cat,
        own,
    }))
}

impl Caps {
    fn edb(&self) -> bool {
        self.d == Dialect::Edb
    }

    fn jobs(&self) -> Option<(&View, Scope)> {
        self.cat
            .widest("scheduler_jobs")
            .filter(|(v, _)| v.has("job_name"))
    }

    fn programs(&self) -> Option<(&View, Scope)> {
        self.cat
            .widest("scheduler_programs")
            .filter(|(v, _)| v.has("program_name"))
    }

    fn schedules(&self) -> Option<(&View, Scope)> {
        self.cat
            .widest("scheduler_schedules")
            .filter(|(v, _)| v.has("schedule_name"))
    }

    fn program_args(&self) -> Option<&View> {
        self.cat
            .widest("scheduler_program_args")
            .map(|(v, _)| v)
            .filter(|v| v.has("program_name") && v.has("argument_position"))
    }

    fn job_args(&self) -> Option<&View> {
        self.cat
            .widest("scheduler_job_args")
            .map(|(v, _)| v)
            .filter(|v| v.has("job_name") && v.has("argument_position"))
    }

    /// The owner of an object read from a view of `scope`: a `USER_*`
    /// view has none, its objects are the user's.
    fn owner_of(&self, owner: Option<String>, scope: Scope) -> Option<String> {
        owner.or_else(|| (scope == Scope::Own).then(|| self.user.clone()))
    }

    /// EDB: only the user's own (the calls take the bare name).
    fn owns(&self, owner: Option<&str>) -> bool {
        !self.edb() || owner == Some(self.user.as_str())
    }

    /// The kinds this scheduler adds to the tree.
    pub(crate) fn kinds(&self) -> Vec<SchedulerKind> {
        vec![
            SchedulerKind::new("program", "Programas", "programa").abilities(true, true, true),
            SchedulerKind::new("schedule", "Programaciones", "programación")
                .abilities(true, true, true),
        ]
    }

    pub(crate) fn notes(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.jobs().is_none() {
            out.push(
                "Tu usuario no puede leer los jobs de DBMS_SCHEDULER (*_scheduler_jobs).".into(),
            );
        }
        if self.edb() && !self.superuser {
            out.push(NOT_OWN.into());
        }
        out
    }

    fn lit(&self, s: &str) -> String {
        self.d.lit(s)
    }

    fn opt_lit(&self, s: Option<&str>) -> String {
        s.map(|x| self.lit(x)).unwrap_or_else(|| "NULL".into())
    }

    fn time(&self, t: Option<&str>) -> String {
        t.map(|x| self.d.time_lit(x))
            .unwrap_or_else(|| "NULL".into())
    }

    /// The calls as one script: an anonymous block on EDB, `CALL`s in a
    /// transaction on KingbaseES.
    fn script(&self, comment: &str, calls: &[String]) -> String {
        if self.edb() {
            let mut out = String::new();
            for line in comment.lines() {
                out.push_str(&format!("-- {}\n", comment_text(line)));
            }
            out.push_str("BEGIN\n");
            for c in calls {
                out.push_str(&format!("  {c};\n"));
            }
            out.push_str("END;\n");
            out
        } else {
            super::transaction(
                comment,
                &calls
                    .iter()
                    .map(|c| format!("CALL {c}"))
                    .collect::<Vec<_>>(),
            )
        }
    }

    fn engine(&self) -> &'static str {
        if self.edb() {
            "EDB Postgres Advanced Server"
        } else {
            "KingbaseES"
        }
    }
}

// ---------------------------------------------------------------------------
// Calendaring expressions
// ---------------------------------------------------------------------------

const FREQS: &[&str] = &["YEARLY", "MONTHLY", "WEEKLY", "DAILY", "HOURLY", "MINUTELY"];
const DAYS: &[&str] = &["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"];

/// A repeat interval built from fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Cal {
    pub(crate) freq: String,
    pub(crate) bymonth: Vec<String>,
    pub(crate) bymonthday: String,
    pub(crate) bydate: String,
    pub(crate) byday: Vec<String>,
    pub(crate) byhour: String,
    pub(crate) byminute: String,
}

/// The fields of an expression, when it only has the ones the form builds.
pub(crate) fn parse_cal(expr: &str) -> Option<Cal> {
    let mut c = Cal::default();
    for part in expr.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (k, v) = part.split_once('=')?;
        let (k, v) = (k.trim().to_ascii_uppercase(), v.trim().to_ascii_uppercase());
        let list =
            |v: &str| -> Vec<String> { v.split(',').map(|x| x.trim().to_string()).collect() };
        match k.as_str() {
            "FREQ" if FREQS.contains(&v.as_str()) => c.freq = v,
            "BYMONTH" => {
                let months = list(&v);
                if !months
                    .iter()
                    .all(|m| m.parse::<u32>().is_ok_and(|n| (1..=12).contains(&n)))
                {
                    return None;
                }
                c.bymonth = months
                    .iter()
                    .map(|m| m.parse::<u32>().unwrap_or(1).to_string())
                    .collect();
            }
            "BYDAY" => {
                let days = list(&v);
                if !days.iter().all(|d| DAYS.contains(&d.as_str())) {
                    return None;
                }
                c.byday = days;
            }
            "BYMONTHDAY" => c.bymonthday = v,
            "BYDATE" => c.bydate = v,
            "BYHOUR" => c.byhour = v,
            "BYMINUTE" => c.byminute = v,
            _ => return None,
        }
    }
    (!c.freq.is_empty()).then_some(c)
}

/// The expression for the fields.
pub(crate) fn cal_text(c: &Cal) -> String {
    let mut parts = vec![format!("FREQ={}", c.freq)];
    if !c.bymonth.is_empty() {
        parts.push(format!("BYMONTH={}", c.bymonth.join(",")));
    }
    for (k, v) in [("BYMONTHDAY", &c.bymonthday), ("BYDATE", &c.bydate)] {
        if !v.is_empty() {
            parts.push(format!("{k}={v}"));
        }
    }
    if !c.byday.is_empty() {
        parts.push(format!("BYDAY={}", c.byday.join(",")));
    }
    for (k, v) in [("BYHOUR", &c.byhour), ("BYMINUTE", &c.byminute)] {
        if !v.is_empty() {
            parts.push(format!("{k}={v}"));
        }
    }
    parts.join(";")
}

/// A comma-separated list of whole numbers in `range` (negative month days
/// count from the end), normalized.
fn numbers(v: &str, label: &str, min: i32, max: i32, zero: bool) -> Result<String> {
    let t = v.trim();
    if t.is_empty() {
        return Ok(String::new());
    }
    let mut out = Vec::new();
    for x in t.split(',') {
        let n: i32 = x.trim().parse().map_err(|_| {
            bad(format!(
                "«{label}»: «{}» no es una lista de números separados por comas.",
                shown(t)
            ))
        })?;
        if n < min || n > max || (!zero && n == 0) {
            return Err(bad(format!(
                "«{label}»: {n} está fuera de rango ({min} a {max})."
            )));
        }
        out.push(n.to_string());
    }
    Ok(out.join(","))
}

/// The form's fields as an expression, checked.
pub(crate) fn cal_spec(v: Vals) -> Result<String> {
    let freq = v.text("freq").to_ascii_uppercase();
    if !FREQS.contains(&freq.as_str()) {
        return Err(bad("Elegí la frecuencia.".into()));
    }
    let bymonth = v.strings("bymonth");
    if !bymonth
        .iter()
        .all(|m| m.parse::<u32>().is_ok_and(|n| (1..=12).contains(&n)))
    {
        return Err(bad("«Meses»: uno no es un mes.".into()));
    }
    let byday: Vec<String> = v
        .strings("byday")
        .iter()
        .map(|d| d.to_ascii_uppercase())
        .collect();
    if !byday.iter().all(|d| DAYS.contains(&d.as_str())) {
        return Err(bad("«Días de la semana»: uno no es un día.".into()));
    }
    let bydate = v.text("bydate");
    if !bydate.is_empty()
        && !bydate
            .split(',')
            .all(|d| d.trim().len() == 4 && d.trim().bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(bad(
            "«Fechas»: tienen que ser MMDD separadas por comas (0101,0701).".into(),
        ));
    }
    Ok(cal_text(&Cal {
        freq,
        bymonth,
        bymonthday: numbers(&v.text("bymonthday"), "Días del mes", -31, 31, false)?,
        bydate: bydate
            .split(',')
            .map(str::trim)
            .collect::<Vec<_>>()
            .join(",")
            .trim_matches(',')
            .to_string(),
        byday,
        byhour: numbers(&v.text("byhour"), "Horas", 0, 23, true)?,
        byminute: numbers(&v.text("byminute"), "Minutos", 0, 59, true)?,
    }))
}

/// The builder's fields, shown while `mode` is `fields`; the raw
/// expression while it's `expression`.
fn cal_fields(mode: &str, fields: &str, expression: &str) -> Vec<FormField> {
    vec![
        FormField::new(
            "freq",
            "Frecuencia",
            FormKind::select(&[
                ("YEARLY", "Anual"),
                ("MONTHLY", "Mensual"),
                ("WEEKLY", "Semanal"),
                ("DAILY", "Diaria"),
                ("HOURLY", "Cada hora"),
                ("MINUTELY", "Cada minuto"),
            ]),
        )
        .when(mode, &[fields])
        .default(json!("DAILY")),
        FormField::new(
            "bymonth",
            "Meses",
            FormKind::multi(&[
                ("1", "Enero"),
                ("2", "Febrero"),
                ("3", "Marzo"),
                ("4", "Abril"),
                ("5", "Mayo"),
                ("6", "Junio"),
                ("7", "Julio"),
                ("8", "Agosto"),
                ("9", "Septiembre"),
                ("10", "Octubre"),
                ("11", "Noviembre"),
                ("12", "Diciembre"),
            ]),
        )
        .when(mode, &[fields]),
        FormField::new("bymonthday", "Días del mes", FormKind::Text)
            .when(mode, &[fields])
            .placeholder("1,15,-1")
            .help("Separados por comas; -1 es el último día del mes."),
        FormField::new("bydate", "Fechas", FormKind::Text)
            .when(mode, &[fields])
            .placeholder("0101,0701")
            .help("Días del año como MMDD, separados por comas."),
        FormField::new(
            "byday",
            "Días de la semana",
            FormKind::multi(&[
                ("MON", "Lunes"),
                ("TUE", "Martes"),
                ("WED", "Miércoles"),
                ("THU", "Jueves"),
                ("FRI", "Viernes"),
                ("SAT", "Sábado"),
                ("SUN", "Domingo"),
            ]),
        )
        .when(mode, &[fields]),
        FormField::new("byhour", "Horas", FormKind::Text)
            .when(mode, &[fields])
            .placeholder("3,15")
            .help("De 0 a 23, separadas por comas."),
        FormField::new("byminute", "Minutos", FormKind::Text)
            .when(mode, &[fields])
            .placeholder("0,30")
            .help("De 0 a 59, separados por comas."),
        FormField::new("repeat_interval", "Expresión de calendario", FormKind::Text)
            .when(mode, &[expression])
            .placeholder("FREQ=WEEKLY;BYDAY=MON,FRI;BYHOUR=6")
            .help("La sintaxis de calendario de DBMS_SCHEDULER: FREQ, BYMONTH, BYMONTHDAY, BYDATE, BYDAY, BYHOUR, BYMINUTE."),
    ]
}

/// The builder's values for an expression (`mode` set to `fields` when it
/// reads as one, else `expression`; `once` without one: it runs once, at
/// its start).
fn cal_values(m: &mut BTreeMap<String, Value>, mode: &str, expr: Option<&str>) {
    let cal = expr.and_then(parse_cal);
    match (&cal, expr) {
        (Some(c), _) => {
            m.insert(mode.into(), json!("fields"));
            m.insert("freq".into(), json!(c.freq));
            m.insert("bymonth".into(), json!(c.bymonth));
            m.insert("bymonthday".into(), json!(c.bymonthday));
            m.insert("bydate".into(), json!(c.bydate));
            m.insert("byday".into(), json!(c.byday));
            m.insert("byhour".into(), json!(c.byhour));
            m.insert("byminute".into(), json!(c.byminute));
        }
        (None, Some(e)) => {
            m.insert(mode.into(), json!("expression"));
            m.insert("repeat_interval".into(), json!(e));
        }
        (None, None) => {
            m.insert(mode.into(), json!("once"));
            m.insert("freq".into(), json!("DAILY"));
        }
    }
}

/// The expression the form asks for, by its mode (`once`: none).
fn repeat_of(v: Vals, mode: &str) -> Result<Option<String>> {
    match v.text(mode).as_str() {
        "once" => Ok(None),
        "expression" => {
            let e = v.opt("repeat_interval");
            if let Some(e) = &e {
                plain(e, "Expresión de calendario")?;
            }
            Ok(e)
        }
        _ => cal_spec(v).map(Some),
    }
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// A program.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Prog {
    pub(crate) owner: Option<String>,
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) action: String,
    pub(crate) args: i64,
    pub(crate) enabled: bool,
    pub(crate) comments: Option<String>,
    pub(crate) arguments: Vec<Arg>,
}

/// A program's argument.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Arg {
    pub(crate) position: i64,
    pub(crate) name: Option<String>,
    pub(crate) kind: String,
    pub(crate) default: Option<String>,
    pub(crate) out: bool,
}

/// A named schedule.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Sched {
    pub(crate) owner: Option<String>,
    pub(crate) name: String,
    pub(crate) start: Option<String>,
    pub(crate) interval: Option<String>,
    pub(crate) end: Option<String>,
    pub(crate) comments: Option<String>,
}

fn filter(c: &Caps, v: &View, a: &str, key: &str, id: &Named) -> Option<String> {
    let mut f = format!("{} = {}", v.col(a, key)?, c.lit(&id.name));
    if let (Some(col), Some(owner)) = (v.col(a, "owner"), &id.owner) {
        f.push_str(&format!(" AND {col} = {}", c.lit(owner)));
    }
    Some(f)
}

fn programs_sql(c: &Caps, v: &View, id: Option<&Named>) -> Option<String> {
    let w = match id {
        Some(id) => format!(" WHERE {}", filter(c, v, "p", "program_name", id)?),
        None => String::new(),
    };
    Some(format!(
        "SELECT {} AS owner, {}::text AS name, {} AS kind, {} AS action, {} AS args, {} AS enabled, {} AS comments
         FROM {} p{w} ORDER BY 2",
        v.raw("p", "owner"),
        v.col("p", "program_name")?,
        v.raw("p", "program_type"),
        v.read("p", "program_action"),
        v.raw("p", "number_of_arguments"),
        v.raw("p", "enabled"),
        v.read("p", "comments"),
        v.from()
    ))
}

fn prog_of(r: &dyn Row) -> Option<Prog> {
    Some(Prog {
        owner: text(r, "owner"),
        name: text(r, "name")?,
        kind: text(r, "kind").unwrap_or_else(|| "PLSQL_BLOCK".into()),
        action: r.get("action").unwrap_or_default(),
        args: text(r, "args").and_then(|n| n.parse().ok()).unwrap_or(0),
        enabled: text(r, "enabled").is_some_and(|e| scheduler::yes(&e)),
        comments: text(r, "comments"),
        arguments: Vec::new(),
    })
}

fn args_sql(c: &Caps, v: &View, key: &str, owner: Option<&str>, name: &str) -> Option<String> {
    let id = Named {
        owner: owner.map(Into::into),
        name: name.into(),
    };
    Some(format!(
        "SELECT {} AS position, {} AS name, {} AS kind, {} AS dflt, {} AS out_arg, {} AS value
         FROM {} a WHERE {} ORDER BY {}",
        v.raw("a", "argument_position"),
        v.read("a", "argument_name"),
        v.read("a", "argument_type"),
        v.read("a", "default_value"),
        v.raw("a", "out_argument"),
        v.read("a", "value"),
        v.from(),
        filter(c, v, "a", key, &id)?,
        v.col("a", "argument_position")?
    ))
}

fn schedules_sql(c: &Caps, v: &View, id: Option<&Named>) -> Option<String> {
    let w = match id {
        Some(id) => format!(" WHERE {}", filter(c, v, "s", "schedule_name", id)?),
        None => String::new(),
    };
    Some(format!(
        "SELECT {} AS owner, {}::text AS name, {} AS start_iso, {} AS repeat, {} AS end_iso, {} AS comments
         FROM {} s{w} ORDER BY 2",
        v.raw("s", "owner"),
        v.col("s", "schedule_name")?,
        v.iso("s", "start_date"),
        v.read("s", "repeat_interval"),
        v.iso("s", "end_date"),
        v.read("s", "comments"),
        v.from()
    ))
}

fn sched_of(r: &dyn Row) -> Option<Sched> {
    Some(Sched {
        owner: text(r, "owner"),
        name: text(r, "name")?,
        start: text(r, "start_iso"),
        interval: text(r, "repeat"),
        end: text(r, "end_iso"),
        comments: text(r, "comments"),
    })
}

/// An item id: the object's name, and its owner where the view has owners.
fn parse_item(id: &str) -> Result<Named> {
    Named::parse(id).ok_or_else(|| bad(format!("«{}» no es un objeto de este servidor", shown(id))))
}

async fn read_program(s: &PgSession, c: &Caps, id: &Named) -> Result<(Prog, Scope)> {
    let (v, scope) = c.programs().ok_or_else(|| {
        bad(
            "Tu usuario no puede leer los programas de DBMS_SCHEDULER (*_scheduler_programs)."
                .into(),
        )
    })?;
    let sql = programs_sql(c, v, Some(id)).unwrap_or_default();
    let mut p = s
        .text(&sql)
        .await?
        .iter()
        .find_map(|r| prog_of(r))
        .ok_or_else(|| {
            bad(format!(
                "no existe el programa «{}» (o tu usuario no lo ve)",
                shown(&id.name)
            ))
        })?;
    if let Some(av) = c.program_args() {
        if let Some(sql) = args_sql(c, av, "program_name", p.owner.as_deref(), &p.name) {
            p.arguments = s
                .text(&sql)
                .await?
                .iter()
                .filter_map(|r| {
                    Some(Arg {
                        position: text(r, "position")?.parse().ok()?,
                        name: text(r, "name"),
                        kind: text(r, "kind").unwrap_or_default(),
                        default: text(r, "dflt"),
                        out: text(r, "out_arg").is_some_and(|o| scheduler::yes(&o)),
                    })
                })
                .collect();
        }
    }
    p.owner = c.owner_of(p.owner.clone(), scope);
    Ok((p, scope))
}

async fn read_schedule(s: &PgSession, c: &Caps, id: &Named) -> Result<Sched> {
    let (v, scope) = c.schedules().ok_or_else(|| {
        bad("Tu usuario no puede leer las programaciones de DBMS_SCHEDULER (*_scheduler_schedules).".into())
    })?;
    let sql = schedules_sql(c, v, Some(id)).unwrap_or_default();
    let mut x = s
        .text(&sql)
        .await?
        .iter()
        .find_map(|r| sched_of(r))
        .ok_or_else(|| {
            bad(format!(
                "no existe la programación «{}» (o tu usuario no la ve)",
                shown(&id.name)
            ))
        })?;
    x.owner = c.owner_of(x.owner.clone(), scope);
    Ok(x)
}

/// A DBMS_SCHEDULER job, with its argument values.
pub(crate) struct Job {
    pub(crate) job: SchedJob,
    pub(crate) owner: Option<String>,
    pub(crate) args: Vec<(i64, String)>,
}

pub(crate) async fn read_job(s: &PgSession, c: &Caps, id: &Named) -> Result<Job> {
    let (v, scope) = c.jobs().ok_or_else(|| {
        bad("Tu usuario no puede leer los jobs de DBMS_SCHEDULER (*_scheduler_jobs).".into())
    })?;
    let f = scheduler::job_filter(c.d, v, "v", id.owner.as_deref(), &id.name);
    let sql = scheduler::jobs_sql(v, c.cat.widest("scheduler_schedules").map(|(v, _)| v), f)
        .unwrap_or_default();
    let job = s
        .text(&sql)
        .await?
        .iter()
        .find_map(|r| SchedJob::from_row(r))
        .ok_or_else(|| {
            bad(format!(
                "no existe el job «{}» (o tu usuario no lo ve)",
                shown(&id.name)
            ))
        })?;
    let mut args = Vec::new();
    if let Some(av) = c.job_args() {
        if let Some(sql) = args_sql(c, av, "job_name", job.owner.as_deref(), &job.name) {
            args = s
                .text(&sql)
                .await?
                .iter()
                .filter_map(|r| {
                    Some((
                        text(r, "position")?.parse().ok()?,
                        Row::get(r, "value").unwrap_or_default(),
                    ))
                })
                .collect();
        }
    }
    Ok(Job {
        owner: c.owner_of(job.owner.clone(), scope),
        job,
        args,
    })
}

/// How many jobs of other owners have this name (EDB resolves bare names).
async fn others_named(s: &PgSession, c: &Caps, name: &str) -> Result<bool> {
    let Some((v, _)) = c.jobs() else {
        return Ok(false);
    };
    let (Some(owner), Some(col)) = (v.col("v", "owner"), v.col("v", "job_name")) else {
        return Ok(false);
    };
    let n = s
        .text(&format!(
            "SELECT count(DISTINCT {owner})::text AS n FROM {} v WHERE {col} = {}",
            v.from(),
            c.lit(name)
        ))
        .await?
        .first()
        .and_then(|r| text(r, "n"))
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0);
    Ok(n > 1)
}

/// The jobs that use a program or a schedule.
async fn users_of(s: &PgSession, c: &Caps, col: &str, name: &str) -> Result<Vec<String>> {
    let Some((v, _)) = c.jobs() else {
        return Ok(Vec::new());
    };
    let (Some(x), Some(jn)) = (v.col("v", col), v.col("v", "job_name")) else {
        return Ok(Vec::new());
    };
    Ok(s.text(&format!(
        "SELECT DISTINCT {jn}::text AS name FROM {} v WHERE {x} = {} ORDER BY 1",
        v.from(),
        c.lit(name)
    ))
    .await?
    .iter()
    .filter_map(|r| text(r, "name"))
    .collect())
}

// ---------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------

pub(crate) async fn items(s: &PgSession, c: &Caps, kind: &str) -> Result<SchedulerList> {
    let mut out = SchedulerList::default();
    let (v, scope) = match kind {
        "program" => c.programs(),
        _ => c.schedules(),
    }
    .ok_or_else(|| {
        bad("Tu usuario no puede leer los objetos de DBMS_SCHEDULER de este tipo.".into())
    })?;
    let sql = match kind {
        "program" => programs_sql(c, v, None),
        _ => schedules_sql(c, v, None),
    }
    .unwrap_or_default();
    let rows = s.text(&sql).await?;
    for r in &rows {
        let (owner, name, description, enabled) = if kind == "program" {
            let Some(p) = prog_of(r) else { continue };
            (
                p.owner,
                p.name,
                p.comments.or(Some(p.action)),
                Some(p.enabled),
            )
        } else {
            let Some(x) = sched_of(r) else { continue };
            (x.owner, x.name, x.comments.or(x.interval), None)
        };
        let named = Named { owner, name };
        out.items.push(SchedulerItem {
            id: named.to_string(),
            name: match &named.owner {
                Some(o) if scope != Scope::Own => format!("{o}.{}", named.name),
                _ => named.name.clone(),
            },
            description,
            enabled,
            system: false,
            can_edit: None,
            can_delete: None,
            actions: None,
        });
    }
    out.items.retain(|i| Named::parse(&i.id).is_some());
    Ok(out)
}

async fn choices(s: &PgSession, c: &Caps) -> Result<BTreeMap<String, Vec<(String, String)>>> {
    let mut out = BTreeMap::new();
    let names = |rows: Vec<(Option<String>, String)>| {
        let mut v: Vec<String> = rows
            .into_iter()
            .filter(|(o, _)| c.owns(o.as_deref()) || !c.edb())
            .map(|(_, n)| n)
            .collect();
        v.dedup();
        pairs(v)
    };
    if let Some((v, scope)) = c.programs() {
        let rows = s
            .text(&programs_sql(c, v, None).unwrap_or_default())
            .await?;
        out.insert(
            "program".into(),
            names(
                rows.iter()
                    .filter_map(|r| prog_of(r))
                    .map(|p| (c.owner_of(p.owner, scope), p.name))
                    .collect(),
            ),
        );
    }
    if let Some((v, scope)) = c.schedules() {
        let rows = s
            .text(&schedules_sql(c, v, None).unwrap_or_default())
            .await?;
        out.insert(
            "schedule".into(),
            names(
                rows.iter()
                    .filter_map(|r| sched_of(r))
                    .map(|x| (c.owner_of(x.owner, scope), x.name))
                    .collect(),
            ),
        );
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

/// A DBMS_SCHEDULER job as the form asks for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobSpec {
    pub(crate) name: String,
    /// `None`: a program and a schedule.
    pub(crate) own: Option<OwnAction>,
    pub(crate) program: Option<String>,
    pub(crate) schedule: Option<String>,
    pub(crate) class: Option<String>,
    pub(crate) enabled: bool,
    pub(crate) auto_drop: bool,
    pub(crate) comments: Option<String>,
    pub(crate) args: Vec<(i64, String)>,
}

/// A job's own action and timing (EDB).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnAction {
    pub(crate) kind: String,
    pub(crate) action: String,
    pub(crate) args: i64,
    pub(crate) start: Option<String>,
    pub(crate) repeat: Option<String>,
    pub(crate) end: Option<String>,
}

fn job_fields(c: &Caps, edit: bool) -> Vec<FormSection> {
    let mut general = Vec::new();
    if edit {
        general.push(FormField::new("owner", "Propietario", FormKind::Text).read_only(true));
        general.push(FormField::new("state", "Estado", FormKind::Text).read_only(true));
    }
    general.push(FormField::new("name", "Nombre", FormKind::Text).required());
    general.push(FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)));
    general.push(
        FormField::new("auto_drop", "Borrar al terminar", FormKind::Bool)
            .default(json!(true))
            .help("El job se borra solo cuando ya no tiene ejecuciones por delante."),
    );
    if !c.edb() {
        general.push(
            FormField::new("job_class", "Clase", FormKind::Text)
                .default(json!("Routine Maintenance"))
                .help("La clase de job de kdb_schedule."),
        );
    }
    general.push(FormField::new("comments", "Comentario", FormKind::Textarea));
    let mut action = Vec::new();
    if c.edb() {
        action.push(
            FormField::new(
                "timing",
                "Qué ejecuta y cuándo",
                FormKind::select(&[
                    ("fields", "Su propia acción, cada cierto tiempo"),
                    ("expression", "Su propia acción, según una expresión"),
                    ("once", "Su propia acción, una sola vez"),
                    ("program", "Un programa con una programación"),
                ]),
            )
            .default(json!("fields")),
        );
        action.push(
            FormField::new(
                "job_type",
                "Tipo",
                FormKind::select(&[
                    ("PLSQL_BLOCK", "Bloque PL/SQL"),
                    ("STORED_PROCEDURE", "Procedimiento almacenado"),
                ]),
            )
            .when("timing", &["fields", "expression", "once"])
            .default(json!("PLSQL_BLOCK")),
        );
        action.push(
            FormField::new("job_action", "Acción", FormKind::code("plsql"))
                .when("timing", &["fields", "expression", "once"])
                .help("Un bloque PL/SQL, o el nombre de un procedimiento almacenado."),
        );
        action.push(
            FormField::new(
                "number_of_arguments",
                "Cantidad de argumentos",
                FormKind::number(Some(0.0), Some(255.0)),
            )
            .when("timing", &["fields", "expression", "once"])
            .default(json!(0))
            .help("Los de un procedimiento almacenado: sus valores van en «Argumentos»."),
        );
    }
    let program = FormField::new(
        "program_name",
        "Programa",
        FormKind::reference("program", false),
    );
    let schedule = FormField::new(
        "schedule_name",
        "Programación",
        FormKind::reference("schedule", false),
    );
    if c.edb() {
        action.push(program.when("timing", &["program"]));
        action.push(schedule.when("timing", &["program"]));
    } else {
        action.push(program.required());
        action.push(schedule.required());
    }
    let mut sections = vec![
        FormSection::new("general", "General").fields(general),
        FormSection::new("action", "Acción").fields(action),
    ];
    if c.edb() {
        let mut timing = vec![FormField::new("start_date", "Inicio", FormKind::DateTime)
            .when("timing", &["fields", "expression", "once"])
            .help("Vacío: ahora.")];
        timing.extend(cal_fields("timing", "fields", "expression"));
        timing.push(
            FormField::new("end_date", "Fin", FormKind::DateTime)
                .when("timing", &["fields", "expression"])
                .help("Vacío: sin fin."),
        );
        sections.push(FormSection::new("timing", "Programación").fields(timing));
        let list = FormList::new(
            "arguments",
            "Argumentos",
            "position",
            vec![
                FormField::new(
                    "position",
                    "Posición",
                    FormKind::number(Some(1.0), Some(255.0)),
                )
                .required(),
                FormField::new("value", "Valor", FormKind::Text),
            ],
        )
        .read_only(c.job_args().is_none() && edit);
        sections.push(FormSection::new("arguments", "Argumentos").list(list));
    }
    sections
}

fn job_values(j: &Job) -> (BTreeMap<String, Value>, Vec<BTreeMap<String, Value>>) {
    let mut m = BTreeMap::new();
    let x = &j.job;
    m.insert("owner".into(), json!(j.owner.clone().unwrap_or_default()));
    m.insert("state".into(), json!(x.state.clone().unwrap_or_default()));
    m.insert("name".into(), json!(x.name));
    m.insert("enabled".into(), json!(x.is_enabled()));
    m.insert("auto_drop".into(), json!(x.auto_drop.unwrap_or(true)));
    m.insert(
        "job_class".into(),
        json!(x.class.clone().unwrap_or_default()),
    );
    m.insert(
        "comments".into(),
        json!(x.comments.clone().unwrap_or_default()),
    );
    let program = x.program.is_some() && x.schedule_name.is_some();
    if program {
        m.insert("timing".into(), json!("program"));
        m.insert(
            "program_name".into(),
            json!(x.program.clone().unwrap_or_default()),
        );
        m.insert(
            "schedule_name".into(),
            json!(x.schedule_name.clone().unwrap_or_default()),
        );
        m.insert("job_type".into(), json!("PLSQL_BLOCK"));
    } else {
        cal_values(&mut m, "timing", x.interval.as_deref());
        m.insert(
            "job_type".into(),
            json!(match x
                .job_type
                .as_deref()
                .map(str::to_ascii_uppercase)
                .as_deref()
            {
                Some("STORED_PROCEDURE") | Some("PROCEDURE") => "STORED_PROCEDURE",
                _ => "PLSQL_BLOCK",
            }),
        );
        m.insert(
            "job_action".into(),
            json!(x.action.clone().unwrap_or_default()),
        );
        m.insert(
            "number_of_arguments".into(),
            json!(x
                .number_of_arguments
                .as_deref()
                .and_then(|n| n.parse::<i64>().ok())
                .unwrap_or(0)),
        );
        m.insert(
            "start_date".into(),
            json!(x.start_iso.clone().unwrap_or_default()),
        );
        m.insert(
            "end_date".into(),
            json!(x.end_iso.clone().unwrap_or_default()),
        );
    }
    let rows = j
        .args
        .iter()
        .map(|(p, v)| {
            BTreeMap::from([
                ("position".to_string(), json!(p)),
                ("value".to_string(), json!(v)),
            ])
        })
        .collect();
    (m, rows)
}

fn date_field(v: Vals, k: &str, label: &str) -> Result<Option<String>> {
    datetime_field(v, k, label)
}

/// The form's job, checked.
pub(crate) fn job_spec(c: &Caps, values: &FormValues) -> Result<JobSpec> {
    let v = Vals(&values.values);
    let name = name_of(v, "name", "Nombre")?;
    let program_mode = !c.edb() || v.text("timing") == "program";
    let (own, program, schedule) = if program_mode {
        let p = v
            .opt("program_name")
            .ok_or_else(|| bad("Elegí el programa.".into()))?;
        let s = v
            .opt("schedule_name")
            .ok_or_else(|| bad("Elegí la programación.".into()))?;
        plain(&p, "Programa")?;
        plain(&s, "Programación")?;
        (None, Some(p), Some(s))
    } else {
        let kind = match v.text("job_type").as_str() {
            "STORED_PROCEDURE" => "STORED_PROCEDURE",
            _ => "PLSQL_BLOCK",
        };
        let action = v.raw("job_action");
        if action.trim().is_empty() {
            return Err(bad("«Acción» no puede quedar vacía.".into()));
        }
        (
            Some(OwnAction {
                kind: kind.into(),
                action,
                args: v
                    .int("number_of_arguments", "Cantidad de argumentos", 0, 255)?
                    .unwrap_or(0),
                start: date_field(v, "start_date", "Inicio")?,
                repeat: repeat_of(v, "timing")?,
                end: date_field(v, "end_date", "Fin")?,
            }),
            None,
            None,
        )
    };
    let mut args: Vec<(i64, String)> = Vec::new();
    for row in super::rows(values, "arguments") {
        let p = row
            .int("position", "Posición", 1, 255)?
            .ok_or_else(|| bad("Cada argumento necesita su posición.".into()))?;
        if args.iter().any(|(q, _)| *q == p) {
            return Err(bad(format!("La posición {p} está en más de un argumento.")));
        }
        args.push((p, row.raw("value")));
    }
    let class = if c.edb() {
        None
    } else {
        let cl = v.opt("job_class");
        if let Some(cl) = &cl {
            plain(cl, "Clase")?;
        }
        cl
    };
    Ok(JobSpec {
        name,
        own,
        program,
        schedule,
        class,
        enabled: v.bool_or("enabled", true),
        auto_drop: v.bool_or("auto_drop", true),
        comments: v.opt("comments"),
        args,
    })
}

/// The job as the server has it, as a spec (to tell what changed). EDB's
/// jobs take no class from the form: its view's `job_class` isn't compared.
pub(crate) fn spec_of(c: &Caps, j: &Job) -> JobSpec {
    let x = &j.job;
    let program = x.program.is_some() && x.schedule_name.is_some();
    JobSpec {
        name: x.name.clone(),
        own: (!program).then(|| OwnAction {
            kind: match x
                .job_type
                .as_deref()
                .map(str::to_ascii_uppercase)
                .as_deref()
            {
                Some("STORED_PROCEDURE") | Some("PROCEDURE") => "STORED_PROCEDURE".into(),
                _ => "PLSQL_BLOCK".into(),
            },
            action: x.action.clone().unwrap_or_default(),
            args: x
                .number_of_arguments
                .as_deref()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0),
            start: x.start_iso.clone(),
            repeat: x
                .interval
                .clone()
                .map(|i| parse_cal(&i).map(|c| cal_text(&c)).unwrap_or(i)),
            end: x.end_iso.clone(),
        }),
        program: x.program.clone().filter(|_| program),
        schedule: x.schedule_name.clone().filter(|_| program),
        class: if c.edb() { None } else { x.class.clone() },
        enabled: x.is_enabled(),
        auto_drop: x.auto_drop.unwrap_or(true),
        comments: x.comments.clone(),
        args: j.args.clone(),
    }
}

/// `CREATE_JOB` (and its argument values, then `ENABLE`).
pub(crate) fn job_create_calls(c: &Caps, j: &JobSpec) -> Vec<String> {
    let mut calls = Vec::new();
    // Created disabled when its arguments are set first.
    let enabled_now = j.enabled && j.args.is_empty();
    if c.edb() {
        let mut a = vec![format!("job_name => {}", c.lit(&j.name))];
        match &j.own {
            Some(o) => {
                a.push(format!("job_type => {}", c.lit(&o.kind)));
                a.push(format!("job_action => {}", c.lit(&o.action)));
                a.push(format!("number_of_arguments => {}", o.args));
                a.push(format!("start_date => {}", c.time(o.start.as_deref())));
                a.push(format!(
                    "repeat_interval => {}",
                    c.opt_lit(o.repeat.as_deref())
                ));
                a.push(format!("end_date => {}", c.time(o.end.as_deref())));
            }
            None => {
                a.push(format!(
                    "program_name => {}",
                    c.opt_lit(j.program.as_deref())
                ));
                a.push(format!(
                    "schedule_name => {}",
                    c.opt_lit(j.schedule.as_deref())
                ));
            }
        }
        a.push(format!("enabled => {}", bool_lit(enabled_now)));
        a.push(format!("auto_drop => {}", bool_lit(j.auto_drop)));
        a.push(format!("comments => {}", c.opt_lit(j.comments.as_deref())));
        calls.push(format!("DBMS_SCHEDULER.CREATE_JOB({})", a.join(", ")));
        for (p, v) in &j.args {
            calls.push(format!(
                "DBMS_SCHEDULER.SET_JOB_ARGUMENT_VALUE({}, {p}, {})",
                c.lit(&j.name),
                c.lit(v)
            ));
        }
        if j.enabled && !enabled_now {
            calls.push(format!("DBMS_SCHEDULER.ENABLE({})", c.lit(&j.name)));
        }
    } else {
        calls.push(format!(
            "DBMS_SCHEDULER.CREATE_JOB({}, {}, {}, {}, {}, {}, {})",
            c.lit(&j.name),
            c.opt_lit(j.program.as_deref()),
            c.opt_lit(j.schedule.as_deref()),
            c.lit(j.class.as_deref().unwrap_or("Routine Maintenance")),
            bool_lit(j.enabled),
            bool_lit(j.auto_drop),
            c.opt_lit(j.comments.as_deref())
        ));
    }
    calls
}

pub(crate) fn job_create(c: &Caps, j: &JobSpec) -> String {
    c.script(
        &format!("{}: nuevo job «{}» (DBMS_SCHEDULER)", c.engine(), j.name),
        &job_create_calls(c, j),
    )
}

/// The change of a job: enabling and argument values in place (EDB), the
/// rest by dropping and creating it again; `None`: nothing changed.
pub(crate) fn job_alter(c: &Caps, cur: &JobSpec, j: &JobSpec) -> Option<String> {
    let same = JobSpec {
        enabled: cur.enabled,
        args: cur.args.clone(),
        ..j.clone()
    } == *cur;
    let args_changed = j.args != cur.args;
    if same && !args_changed && j.enabled == cur.enabled {
        return None;
    }
    let name = c.lit(&cur.name);
    if same && (c.edb() || !args_changed) {
        let mut calls = Vec::new();
        for (p, v) in &j.args {
            if !cur.args.iter().any(|(q, w)| q == p && w == v) {
                calls.push(format!(
                    "DBMS_SCHEDULER.SET_JOB_ARGUMENT_VALUE({name}, {p}, {})",
                    c.lit(v)
                ));
            }
        }
        if j.enabled != cur.enabled {
            calls.push(format!(
                "DBMS_SCHEDULER.{}({name})",
                if j.enabled { "ENABLE" } else { "DISABLE" }
            ));
        }
        if !calls.is_empty() {
            return Some(c.script(
                &format!("{}: job «{}» (DBMS_SCHEDULER)", c.engine(), cur.name),
                &calls,
            ));
        }
    }
    let mut calls = vec![format!("DBMS_SCHEDULER.DROP_JOB({name})")];
    calls.extend(job_create_calls(c, j));
    Some(c.script(
        &format!(
            "{}: job «{}» (DBMS_SCHEDULER)\n{}",
            c.engine(),
            cur.name,
            if c.edb() {
                JOB_RECREATE
            } else {
                KDB_JOB_RECREATE
            }
        ),
        &calls,
    ))
}

/// Whether the user may change job `name` of `owner`.
async fn may_job(s: &PgSession, c: &Caps, owner: Option<&str>, name: &str) -> Result<()> {
    if c.edb() {
        if !c.owns(owner) {
            return Err(bad(NOT_OWN.into()));
        }
        if others_named(s, c, name).await? {
            return Err(bad(SAME_NAME.into()));
        }
    } else if !c.superuser && !c.own.contains(name) {
        return Err(bad(KDB_OTHERS.into()));
    }
    Ok(())
}

pub(crate) async fn job_form(s: &PgSession, c: &Caps, id: Option<&Named>) -> Result<ObjectForm> {
    let mut f = ObjectForm {
        sections: job_fields(c, id.is_some()),
        notes: c.notes(),
        choices: choices(s, c).await?,
        ..Default::default()
    };
    match id {
        None => {
            f.values.insert("enabled".into(), json!(true));
            f.values.insert("auto_drop".into(), json!(true));
            if c.edb() {
                f.values.insert("timing".into(), json!("fields"));
                f.values.insert("freq".into(), json!("DAILY"));
                f.values.insert("job_type".into(), json!("PLSQL_BLOCK"));
                f.values.insert("number_of_arguments".into(), json!(0));
            } else {
                f.values
                    .insert("job_class".into(), json!("Routine Maintenance"));
            }
        }
        Some(id) => {
            let j = read_job(s, c, id).await?;
            let (values, rows) = job_values(&j);
            f.values = values;
            for sec in &mut f.sections {
                for l in &mut sec.lists {
                    if l.key == "arguments" {
                        l.rows = rows.clone();
                    }
                }
            }
            f.notes.push(
                if c.edb() {
                    JOB_RECREATE
                } else {
                    KDB_JOB_RECREATE
                }
                .into(),
            );
            if c.edb() && c.job_args().is_none() {
                f.notes.push(NO_ARGS_VIEW.into());
            }
            if let Err(e) = may_job(s, c, j.owner.as_deref(), &j.job.name).await {
                f.read_only = true;
                f.notes.insert(0, e.to_string());
            }
        }
    }
    Ok(f)
}

pub(crate) async fn job_plan(
    s: &PgSession,
    c: &Caps,
    id: Option<&Named>,
    values: &FormValues,
    prefix: &'static str,
) -> Result<super::Plan> {
    let j = job_spec(c, values)?;
    let lookup = |name: &str| {
        Named {
            owner: c
                .edb()
                .then(|| c.user.clone())
                .filter(|_| c.jobs().is_some_and(|(v, _)| v.has("owner"))),
            name: name.into(),
        }
        .to_string()
    };
    match id {
        None => {
            if c.edb() && others_named(s, c, &j.name).await? {
                return Err(bad(SAME_NAME.into()));
            }
            Ok(super::Plan {
                script: job_create(c, &j),
                empty: false,
                id: Some(format!("{prefix}{}", lookup(&j.name))),
                new_id: None,
            })
        }
        Some(id) => {
            let cur = read_job(s, c, id).await?;
            may_job(s, c, cur.owner.as_deref(), &cur.job.name).await?;
            if c.edb() && !j.args.is_empty() && c.job_args().is_none() {
                return Err(bad(NO_ARGS_VIEW.into()));
            }
            let cur = spec_of(c, &cur);
            match job_alter(c, &cur, &j) {
                None => Ok(super::Plan::nothing(&cur.name, format!("{prefix}{id}"))),
                Some(script) => Ok(super::Plan {
                    script,
                    empty: false,
                    id: Some(if j.name == cur.name {
                        format!("{prefix}{id}")
                    } else {
                        format!("{prefix}{}", lookup(&j.name))
                    }),
                    new_id: None,
                }),
            }
        }
    }
}

pub(crate) async fn job_delete_script(s: &PgSession, c: &Caps, id: &Named) -> Result<String> {
    let cur = read_job(s, c, id).await?;
    may_job(s, c, cur.owner.as_deref(), &cur.job.name).await?;
    Ok(c.script(
        &format!(
            "{}: borrar el job «{}» (DBMS_SCHEDULER)",
            c.engine(),
            cur.job.name
        ),
        &[format!("DBMS_SCHEDULER.DROP_JOB({})", c.lit(&cur.job.name))],
    ))
}

// ---------------------------------------------------------------------------
// Programs
// ---------------------------------------------------------------------------

fn program_fields(c: &Caps, edit: bool) -> Vec<FormSection> {
    let mut general = Vec::new();
    if edit {
        general.push(FormField::new("owner", "Propietario", FormKind::Text).read_only(true));
        general.push(FormField::new("used_by", "Lo usan", FormKind::Text).read_only(true));
    }
    general.push(FormField::new("name", "Nombre", FormKind::Text).required());
    general.push(
        FormField::new(
            "program_type",
            "Tipo",
            if c.edb() {
                FormKind::select(&[
                    ("PLSQL_BLOCK", "Bloque PL/SQL"),
                    ("STORED_PROCEDURE", "Procedimiento almacenado"),
                ])
            } else {
                FormKind::select(&[
                    ("PLSQL_BLOCK", "Bloque PL/SQL"),
                    ("EXECUTABLE", "Comando del sistema"),
                ])
            },
        )
        .default(json!("PLSQL_BLOCK")),
    );
    general.push(
        FormField::new("program_action", "Acción", FormKind::code("plsql"))
            .required()
            .help("Un bloque PL/SQL, el nombre de un procedimiento almacenado o un comando, según el tipo."),
    );
    if c.edb() {
        general.push(
            FormField::new(
                "number_of_arguments",
                "Cantidad de argumentos",
                FormKind::number(Some(0.0), Some(255.0)),
            )
            .default(json!(0)),
        );
    } else {
        general.push(
            FormField::new(
                "database",
                "Base de datos",
                FormKind::reference("database", false),
            )
            .default(json!(c.database))
            .help("La base en la que corre el programa."),
        );
    }
    general.push(FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)));
    general.push(FormField::new("comments", "Comentario", FormKind::Textarea));
    let mut sections = vec![FormSection::new("general", "General").fields(general)];
    if c.edb() {
        sections.push(
            FormSection::new("arguments", "Argumentos").list(
                FormList::new(
                    "arguments",
                    "Argumentos",
                    "position",
                    vec![
                        FormField::new(
                            "position",
                            "Posición",
                            FormKind::number(Some(1.0), Some(255.0)),
                        )
                        .required(),
                        FormField::new("name", "Nombre", FormKind::Text),
                        FormField::new("type", "Tipo", FormKind::Text)
                            .required()
                            .placeholder("VARCHAR2"),
                        FormField::new("default", "Valor por defecto", FormKind::Text),
                        FormField::new("out", "De salida", FormKind::Bool),
                    ],
                )
                .read_only(edit && c.program_args().is_none()),
            ),
        );
    }
    sections
}

/// A program as the form asks for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProgSpec {
    pub(crate) name: String,
    pub(crate) kind: String,
    pub(crate) action: String,
    pub(crate) args: i64,
    pub(crate) database: Option<String>,
    pub(crate) enabled: bool,
    pub(crate) comments: Option<String>,
    pub(crate) arguments: Vec<Arg>,
}

pub(crate) fn prog_spec(c: &Caps, values: &FormValues) -> Result<ProgSpec> {
    let v = Vals(&values.values);
    let kinds: &[&str] = if c.edb() {
        &["PLSQL_BLOCK", "STORED_PROCEDURE"]
    } else {
        &["PLSQL_BLOCK", "EXECUTABLE"]
    };
    let kind = v
        .opt("program_type")
        .unwrap_or_else(|| "PLSQL_BLOCK".into());
    if !kinds.contains(&kind.as_str()) {
        return Err(bad(format!(
            "«Tipo»: «{}» no es uno de sus tipos.",
            shown(&kind)
        )));
    }
    let action = v.raw("program_action");
    if action.trim().is_empty() {
        return Err(bad("«Acción» no puede quedar vacía.".into()));
    }
    let mut arguments = Vec::new();
    for row in super::rows(values, "arguments") {
        let position = row
            .int("position", "Posición", 1, 255)?
            .ok_or_else(|| bad("Cada argumento necesita su posición.".into()))?;
        if arguments.iter().any(|a: &Arg| a.position == position) {
            return Err(bad(format!(
                "La posición {position} está en más de un argumento."
            )));
        }
        let kind = row.text("type");
        if kind.is_empty() {
            return Err(bad(format!("El argumento {position} necesita su tipo.")));
        }
        plain(&kind, "Tipo")?;
        let name = row.opt("name");
        if let Some(n) = &name {
            plain(n, "Nombre")?;
        }
        arguments.push(Arg {
            position,
            name,
            kind,
            default: row.opt("default"),
            out: row.bool("out"),
        });
    }
    arguments.sort_by_key(|a| a.position);
    let args = v
        .int("number_of_arguments", "Cantidad de argumentos", 0, 255)?
        .unwrap_or(0);
    if arguments.iter().any(|a| a.position > args) {
        return Err(bad(
            "Un argumento tiene una posición mayor que la cantidad de argumentos del programa."
                .into(),
        ));
    }
    let database = if c.edb() {
        None
    } else {
        Some(v.opt("database").unwrap_or_else(|| c.database.clone()))
    };
    if let Some(d) = &database {
        plain(d, "Base de datos")?;
    }
    Ok(ProgSpec {
        name: name_of(v, "name", "Nombre")?,
        kind,
        action,
        args,
        database,
        enabled: v.bool_or("enabled", true),
        comments: v.opt("comments"),
        arguments,
    })
}

fn define_arg(c: &Caps, program: &str, a: &Arg) -> String {
    let mut parts = vec![
        format!("program_name => {}", c.lit(program)),
        format!("argument_position => {}", a.position),
        format!("argument_name => {}", c.opt_lit(a.name.as_deref())),
        format!("argument_type => {}", c.lit(&a.kind)),
    ];
    if a.default.is_some() || a.out {
        parts.push(format!(
            "default_value => {}",
            c.opt_lit(a.default.as_deref())
        ));
        parts.push(format!("out_argument => {}", bool_lit(a.out)));
    }
    format!(
        "DBMS_SCHEDULER.DEFINE_PROGRAM_ARGUMENT({})",
        parts.join(", ")
    )
}

pub(crate) fn prog_create_calls(c: &Caps, p: &ProgSpec) -> Vec<String> {
    let mut calls = Vec::new();
    if c.edb() {
        // Arguments are defined while it's disabled.
        let enabled_now = p.enabled && p.arguments.is_empty();
        calls.push(format!(
            "DBMS_SCHEDULER.CREATE_PROGRAM(program_name => {}, program_type => {}, program_action => {}, number_of_arguments => {}, enabled => {}, comments => {})",
            c.lit(&p.name),
            c.lit(&p.kind),
            c.lit(&p.action),
            p.args,
            bool_lit(enabled_now),
            c.opt_lit(p.comments.as_deref())
        ));
        for a in &p.arguments {
            calls.push(define_arg(c, &p.name, a));
        }
        if p.enabled && !enabled_now {
            calls.push(format!("DBMS_SCHEDULER.ENABLE({})", c.lit(&p.name)));
        }
    } else {
        calls.push(format!(
            "DBMS_SCHEDULER.CREATE_PROGRAM({}, {}, {}, NULL, {}, 0, {}, {})",
            c.lit(&p.name),
            c.lit(&p.kind),
            c.lit(&p.action),
            c.opt_lit(p.database.as_deref()),
            bool_lit(p.enabled),
            c.opt_lit(p.comments.as_deref())
        ));
    }
    calls
}

fn prog_spec_of(p: &Prog, database: &str) -> ProgSpec {
    ProgSpec {
        name: p.name.clone(),
        kind: match p.kind.to_ascii_uppercase().as_str() {
            "STORED_PROCEDURE" | "PROCEDURE" => "STORED_PROCEDURE".into(),
            "EXECUTABLE" | "BATCH" => "EXECUTABLE".into(),
            _ => "PLSQL_BLOCK".into(),
        },
        action: p.action.clone(),
        args: p.args,
        database: (!database.is_empty()).then(|| database.to_string()),
        enabled: p.enabled,
        comments: p.comments.clone(),
        arguments: p.arguments.clone(),
    }
}

/// A program's change: enabling and (EDB) its arguments in place, the rest
/// by dropping and creating it again while no job uses it.
pub(crate) fn prog_alter(
    c: &Caps,
    cur: &ProgSpec,
    p: &ProgSpec,
    used_by: &[String],
) -> Result<Option<String>> {
    let same = ProgSpec {
        enabled: cur.enabled,
        arguments: cur.arguments.clone(),
        ..p.clone()
    } == *cur;
    let args_changed = p.arguments != cur.arguments;
    if same && !args_changed && p.enabled == cur.enabled {
        return Ok(None);
    }
    let name = c.lit(&cur.name);
    if same && (c.edb() || !args_changed) {
        let mut calls = Vec::new();
        if args_changed {
            if cur.enabled {
                calls.push(format!("DBMS_SCHEDULER.DISABLE({name})"));
            }
            for a in &cur.arguments {
                if !p.arguments.contains(a) {
                    calls.push(format!(
                        "DBMS_SCHEDULER.DROP_PROGRAM_ARGUMENT(program_name => {name}, argument_position => {})",
                        a.position
                    ));
                }
            }
            for a in &p.arguments {
                if !cur.arguments.contains(a) {
                    calls.push(define_arg(c, &cur.name, a));
                }
            }
            if p.enabled {
                calls.push(format!("DBMS_SCHEDULER.ENABLE({name})"));
            }
        } else {
            calls.push(format!(
                "DBMS_SCHEDULER.{}({name})",
                if p.enabled { "ENABLE" } else { "DISABLE" }
            ));
        }
        return Ok(Some(c.script(
            &format!("{}: programa «{}» (DBMS_SCHEDULER)", c.engine(), cur.name),
            &calls,
        )));
    }
    if !used_by.is_empty() {
        return Err(bad(format!(
            "El programa lo usan {} job(s) ({}): DBMS_SCHEDULER no lo cambia, y para crearlo de nuevo no tiene que usarlo ninguno.",
            used_by.len(),
            shown(&used_by.join(", "))
        )));
    }
    let mut calls = vec![format!("DBMS_SCHEDULER.DROP_PROGRAM({name})")];
    calls.extend(prog_create_calls(c, p));
    Ok(Some(c.script(
        &format!(
            "{}: programa «{}» (DBMS_SCHEDULER)\n{RECREATE_UNUSED}",
            c.engine(),
            cur.name
        ),
        &calls,
    )))
}

fn may_object(c: &Caps, owner: Option<&str>) -> Result<()> {
    if !c.owns(owner) {
        return Err(bad(NOT_OWN.into()));
    }
    Ok(())
}

pub(crate) async fn program_form(s: &PgSession, c: &Caps, id: Option<&str>) -> Result<ObjectForm> {
    let mut f = ObjectForm {
        sections: program_fields(c, id.is_some()),
        notes: c.notes(),
        ..Default::default()
    };
    if !c.edb() {
        f.choices
            .insert("database".into(), super::databases(s).await?);
        f.notes.push(KDB_PROGRAM_CONN.into());
    }
    match id {
        None => {
            f.values.insert("program_type".into(), json!("PLSQL_BLOCK"));
            f.values.insert("enabled".into(), json!(true));
            if c.edb() {
                f.values.insert("number_of_arguments".into(), json!(0));
            } else {
                f.values.insert("database".into(), json!(c.database));
            }
        }
        Some(id) => {
            let named = parse_item(id)?;
            let (p, _) = read_program(s, c, &named).await?;
            let used = users_of(s, c, "program_name", &p.name).await?;
            let sp = prog_spec_of(&p, "");
            f.values
                .insert("owner".into(), json!(p.owner.clone().unwrap_or_default()));
            f.values.insert(
                "used_by".into(),
                json!(if used.is_empty() {
                    "ningún job".to_string()
                } else {
                    used.join(", ")
                }),
            );
            f.values.insert("name".into(), json!(sp.name));
            f.values.insert("program_type".into(), json!(sp.kind));
            f.values.insert("program_action".into(), json!(sp.action));
            f.values
                .insert("number_of_arguments".into(), json!(sp.args));
            f.values.insert("enabled".into(), json!(sp.enabled));
            f.values
                .insert("comments".into(), json!(sp.comments.unwrap_or_default()));
            if !c.edb() {
                f.values.insert("database".into(), json!(c.database));
            }
            let rows: Vec<BTreeMap<String, Value>> = p
                .arguments
                .iter()
                .map(|a| {
                    BTreeMap::from([
                        ("position".to_string(), json!(a.position)),
                        (
                            "name".to_string(),
                            json!(a.name.clone().unwrap_or_default()),
                        ),
                        ("type".to_string(), json!(a.kind)),
                        (
                            "default".to_string(),
                            json!(a.default.clone().unwrap_or_default()),
                        ),
                        ("out".to_string(), json!(a.out)),
                    ])
                })
                .collect();
            for sec in &mut f.sections {
                for l in &mut sec.lists {
                    l.rows = rows.clone();
                }
            }
            f.notes.push(RECREATE_UNUSED.into());
            if c.edb() && c.program_args().is_none() {
                f.notes.push(NO_ARGS_VIEW.into());
            }
            if let Err(e) = may_object(c, p.owner.as_deref()) {
                f.read_only = true;
                f.notes.insert(0, e.to_string());
            }
        }
    }
    Ok(f)
}

pub(crate) async fn program_plan(
    s: &PgSession,
    c: &Caps,
    id: Option<&str>,
    values: &FormValues,
) -> Result<super::Plan> {
    let p = prog_spec(c, values)?;
    let item = |name: &str| {
        Named {
            owner: c
                .edb()
                .then(|| c.user.clone())
                .filter(|_| c.programs().is_some_and(|(v, _)| v.has("owner"))),
            name: name.into(),
        }
        .to_string()
    };
    match id {
        None => Ok(super::Plan {
            script: c.script(
                &format!(
                    "{}: nuevo programa «{}» (DBMS_SCHEDULER)",
                    c.engine(),
                    p.name
                ),
                &prog_create_calls(c, &p),
            ),
            empty: false,
            id: Some(item(&p.name)),
            new_id: None,
        }),
        Some(id) => {
            let named = parse_item(id)?;
            let (cur, _) = read_program(s, c, &named).await?;
            may_object(c, cur.owner.as_deref())?;
            if c.edb() && p.arguments != cur.arguments && c.program_args().is_none() {
                return Err(bad(NO_ARGS_VIEW.into()));
            }
            let used = users_of(s, c, "program_name", &cur.name).await?;
            let mut cur_spec = prog_spec_of(&cur, p.database.as_deref().unwrap_or_default());
            if c.edb() {
                cur_spec.database = None;
            }
            match prog_alter(c, &cur_spec, &p, &used)? {
                None => Ok(super::Plan::nothing(&cur.name, id.to_string())),
                Some(script) => Ok(super::Plan {
                    script,
                    empty: false,
                    id: Some(if p.name == cur.name {
                        id.to_string()
                    } else {
                        item(&p.name)
                    }),
                    new_id: None,
                }),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Schedules
// ---------------------------------------------------------------------------

fn schedule_fields(edit: bool) -> Vec<FormSection> {
    let mut general = Vec::new();
    if edit {
        general.push(FormField::new("owner", "Propietario", FormKind::Text).read_only(true));
        general.push(FormField::new("used_by", "La usan", FormKind::Text).read_only(true));
    }
    general.push(FormField::new("name", "Nombre", FormKind::Text).required());
    general.push(FormField::new("comments", "Comentario", FormKind::Textarea));
    let mut timing = vec![
        FormField::new("start_date", "Inicio", FormKind::DateTime).help("Vacío: ahora."),
        FormField::new(
            "rep_mode",
            "Repetición",
            FormKind::select(&[
                ("fields", "Por campos"),
                ("expression", "Una expresión"),
                ("once", "Una sola vez"),
            ]),
        )
        .default(json!("fields")),
    ];
    timing.extend(cal_fields("rep_mode", "fields", "expression"));
    timing.push(FormField::new("end_date", "Fin", FormKind::DateTime).help("Vacío: sin fin."));
    vec![
        FormSection::new("general", "General").fields(general),
        FormSection::new("timing", "Programación").fields(timing),
    ]
}

/// A schedule as the form asks for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SchedSpec {
    pub(crate) name: String,
    pub(crate) start: Option<String>,
    pub(crate) repeat: Option<String>,
    pub(crate) end: Option<String>,
    pub(crate) comments: Option<String>,
}

pub(crate) fn sched_spec(values: &FormValues) -> Result<SchedSpec> {
    let v = Vals(&values.values);
    let start = date_field(v, "start_date", "Inicio")?;
    let end = date_field(v, "end_date", "Fin")?;
    if let (Some(a), Some(b)) = (&start, &end) {
        if valid_datetime(a) && valid_datetime(b) && b < a {
            return Err(bad("«Fin» es anterior a «Inicio».".into()));
        }
    }
    Ok(SchedSpec {
        name: name_of(v, "name", "Nombre")?,
        start,
        repeat: repeat_of(v, "rep_mode")?,
        end,
        comments: v.opt("comments"),
    })
}

pub(crate) fn sched_create_call(c: &Caps, x: &SchedSpec) -> String {
    if c.edb() {
        format!(
            "DBMS_SCHEDULER.CREATE_SCHEDULE(schedule_name => {}, start_date => {}, repeat_interval => {}, end_date => {}, comments => {})",
            c.lit(&x.name),
            c.time(x.start.as_deref()),
            c.opt_lit(x.repeat.as_deref()),
            c.time(x.end.as_deref()),
            c.opt_lit(x.comments.as_deref())
        )
    } else {
        format!(
            "DBMS_SCHEDULER.CREATE_SCHEDULE({}, {}, {}, {}, {})",
            c.lit(&x.name),
            c.time(x.start.as_deref()),
            c.opt_lit(x.repeat.as_deref()),
            c.time(x.end.as_deref()),
            c.opt_lit(x.comments.as_deref())
        )
    }
}

fn sched_spec_of(x: &Sched) -> SchedSpec {
    SchedSpec {
        name: x.name.clone(),
        start: x.start.clone(),
        repeat: x
            .interval
            .clone()
            .map(|i| parse_cal(&i).map(|c| cal_text(&c)).unwrap_or(i)),
        end: x.end.clone(),
        comments: x.comments.clone(),
    }
}

pub(crate) fn sched_alter(
    c: &Caps,
    cur: &SchedSpec,
    x: &SchedSpec,
    used_by: &[String],
) -> Result<Option<String>> {
    if cur == x {
        return Ok(None);
    }
    if !used_by.is_empty() {
        return Err(bad(format!(
            "La programación la usan {} job(s) ({}): DBMS_SCHEDULER no la cambia, y para crearla de nuevo no tiene que usarla ninguno.",
            used_by.len(),
            shown(&used_by.join(", "))
        )));
    }
    Ok(Some(c.script(
        &format!(
            "{}: programación «{}» (DBMS_SCHEDULER)\n{RECREATE_UNUSED}",
            c.engine(),
            cur.name
        ),
        &[
            format!("DBMS_SCHEDULER.DROP_SCHEDULE({})", c.lit(&cur.name)),
            sched_create_call(c, x),
        ],
    )))
}

pub(crate) async fn schedule_form(s: &PgSession, c: &Caps, id: Option<&str>) -> Result<ObjectForm> {
    let mut f = ObjectForm {
        sections: schedule_fields(id.is_some()),
        notes: c.notes(),
        ..Default::default()
    };
    match id {
        None => {
            f.values.insert("rep_mode".into(), json!("fields"));
            f.values.insert("freq".into(), json!("DAILY"));
        }
        Some(id) => {
            let named = parse_item(id)?;
            let x = read_schedule(s, c, &named).await?;
            let used = users_of(s, c, "schedule_name", &x.name).await?;
            f.values
                .insert("owner".into(), json!(x.owner.clone().unwrap_or_default()));
            f.values.insert(
                "used_by".into(),
                json!(if used.is_empty() {
                    "ningún job".to_string()
                } else {
                    used.join(", ")
                }),
            );
            f.values.insert("name".into(), json!(x.name));
            f.values.insert(
                "comments".into(),
                json!(x.comments.clone().unwrap_or_default()),
            );
            f.values.insert(
                "start_date".into(),
                json!(x.start.clone().unwrap_or_default()),
            );
            f.values
                .insert("end_date".into(), json!(x.end.clone().unwrap_or_default()));
            cal_values(&mut f.values, "rep_mode", x.interval.as_deref());
            f.notes.push(RECREATE_UNUSED.into());
            if let Err(e) = may_object(c, x.owner.as_deref()) {
                f.read_only = true;
                f.notes.insert(0, e.to_string());
            }
        }
    }
    Ok(f)
}

pub(crate) async fn schedule_plan(
    s: &PgSession,
    c: &Caps,
    id: Option<&str>,
    values: &FormValues,
) -> Result<super::Plan> {
    let x = sched_spec(values)?;
    let item = |name: &str| {
        Named {
            owner: c
                .edb()
                .then(|| c.user.clone())
                .filter(|_| c.schedules().is_some_and(|(v, _)| v.has("owner"))),
            name: name.into(),
        }
        .to_string()
    };
    match id {
        None => Ok(super::Plan {
            script: c.script(
                &format!(
                    "{}: nueva programación «{}» (DBMS_SCHEDULER)",
                    c.engine(),
                    x.name
                ),
                &[sched_create_call(c, &x)],
            ),
            empty: false,
            id: Some(item(&x.name)),
            new_id: None,
        }),
        Some(id) => {
            let named = parse_item(id)?;
            let cur = read_schedule(s, c, &named).await?;
            may_object(c, cur.owner.as_deref())?;
            let used = users_of(s, c, "schedule_name", &cur.name).await?;
            match sched_alter(c, &sched_spec_of(&cur), &x, &used)? {
                None => Ok(super::Plan::nothing(&cur.name, id.to_string())),
                Some(script) => Ok(super::Plan {
                    script,
                    empty: false,
                    id: Some(item(&x.name)),
                    new_id: None,
                }),
            }
        }
    }
}

/// `DROP_PROGRAM` / `DROP_SCHEDULE`, refused while a job uses it.
pub(crate) async fn object_delete_script(
    s: &PgSession,
    c: &Caps,
    kind: &str,
    id: &str,
) -> Result<String> {
    let named = parse_item(id)?;
    let (name, owner, col, what, call) = if kind == "program" {
        let (p, _) = read_program(s, c, &named).await?;
        (
            p.name,
            p.owner,
            "program_name",
            "el programa",
            "DROP_PROGRAM",
        )
    } else {
        let x = read_schedule(s, c, &named).await?;
        (
            x.name,
            x.owner,
            "schedule_name",
            "la programación",
            "DROP_SCHEDULE",
        )
    };
    may_object(c, owner.as_deref())?;
    let used = users_of(s, c, col, &name).await?;
    if !used.is_empty() {
        return Err(bad(format!(
            "No se puede borrar {what} «{}»: la usan {} job(s) ({}).",
            shown(&name),
            used.len(),
            shown(&used.join(", "))
        )));
    }
    Ok(c.script(
        &format!("{}: borrar {what} «{name}» (DBMS_SCHEDULER)", c.engine()),
        &[format!("DBMS_SCHEDULER.{call}({})", c.lit(&name))],
    ))
}

/// Errors that say a call isn't available.
pub(crate) fn no_steps() -> Error {
    Error::Unsupported(
        "un job de DBMS_SCHEDULER de este servidor es una sola acción: no tiene pasos".into(),
    )
}

/// Whether the user may clear an EDB job's history: theirs, with a name no
/// other owner's job has (its runs are found by name).
pub(crate) async fn edb_may_clear(s: &PgSession, c: &Caps, j: &Job) -> Result<()> {
    if !c.edb() {
        return Err(Error::Unsupported(
            "este servidor no tiene el historial de edb_job_scheduler".into(),
        ));
    }
    may_job(s, c, j.owner.as_deref(), &j.job.name).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::scheduler::{Kind, View};

    fn caps(d: Dialect) -> Caps {
        Caps {
            d,
            user: "app".into(),
            superuser: false,
            database: "db".into(),
            cat: Catalog::with(vec![
                View::fake(
                    "sys",
                    "dba_scheduler_jobs",
                    &[("owner", Kind::Other), ("job_name", Kind::Other)],
                ),
                View::fake(
                    "sys",
                    "dba_scheduler_job_args",
                    &[
                        ("owner", Kind::Other),
                        ("job_name", Kind::Other),
                        ("argument_position", Kind::Other),
                        ("value", Kind::Other),
                    ],
                ),
                View::fake(
                    "sys",
                    "dba_scheduler_program_args",
                    &[
                        ("owner", Kind::Other),
                        ("program_name", Kind::Other),
                        ("argument_position", Kind::Other),
                    ],
                ),
            ]),
            own: HashSet::from(["mine".to_string()]),
        }
    }

    fn fv(v: Value, lists: Value) -> FormValues {
        FormValues {
            values: serde_json::from_value(v).unwrap(),
            lists: serde_json::from_value(lists).unwrap(),
        }
    }

    #[test]
    fn calendars_round_trip() {
        let c =
            parse_cal("freq=weekly; byday=MON,fri; byhour=6;BYMINUTE=0,30;BYMONTH=01,12").unwrap();
        assert_eq!(c.freq, "WEEKLY");
        assert_eq!(c.byday, ["MON", "FRI"]);
        assert_eq!(c.bymonth, ["1", "12"]);
        assert_eq!(
            cal_text(&c),
            "FREQ=WEEKLY;BYMONTH=1,12;BYDAY=MON,FRI;BYHOUR=6;BYMINUTE=0,30"
        );
        assert!(parse_cal("FREQ=DAILY;INTERVAL=2").is_none());
        assert!(parse_cal("FREQ=SECONDLY").is_none());
        assert!(parse_cal("BYHOUR=3").is_none());
        assert!(parse_cal("FREQ=DAILY;BYDAY=XYZ").is_none());
        let m: BTreeMap<String, Value> = serde_json::from_value(json!({
            "freq": "monthly", "bymonthday": "1, 15,-1", "byhour": "3", "bymonth": ["2"], "bydate": "0101, 0701"
        }))
        .unwrap();
        assert_eq!(
            cal_spec(Vals(&m)).unwrap(),
            "FREQ=MONTHLY;BYMONTH=2;BYMONTHDAY=1,15,-1;BYDATE=0101,0701;BYHOUR=3"
        );
        for (k, bad) in [
            ("bymonthday", json!("0")),
            ("bymonthday", json!("32")),
            ("byhour", json!("24")),
            ("byminute", json!("1;DROP")),
            ("bymonth", json!(["13"])),
            ("byday", json!(["XX"])),
            ("bydate", json!("1301x")),
            ("freq", json!("SECONDLY")),
        ] {
            let mut m: BTreeMap<String, Value> =
                serde_json::from_value(json!({"freq": "DAILY"})).unwrap();
            m.insert(k.into(), bad.clone());
            assert!(cal_spec(Vals(&m)).is_err(), "{k} {bad}");
        }
        let mut v = BTreeMap::new();
        cal_values(&mut v, "rep_mode", Some("FREQ=HOURLY;BYMINUTE=5"));
        assert_eq!(
            (v["rep_mode"].clone(), v["freq"].clone()),
            (json!("fields"), json!("HOURLY"))
        );
        let mut v = BTreeMap::new();
        cal_values(&mut v, "rep_mode", Some("FREQ=DAILY;INTERVAL=2"));
        assert_eq!(v["repeat_interval"], json!("FREQ=DAILY;INTERVAL=2"));
    }

    #[test]
    fn an_edb_job_with_its_own_action() {
        let c = caps(Dialect::Edb);
        let j = job_spec(
            &c,
            &fv(
                json!({
                    "name": "nightly", "timing": "fields", "job_type": "STORED_PROCEDURE",
                    "job_action": "purge", "number_of_arguments": 1, "freq": "DAILY", "byhour": "3",
                    "start_date": "2031-01-01T00:00:00", "enabled": true, "auto_drop": false, "comments": "it's"
                }),
                json!({"arguments": [{"position": 1, "value": "30"}]}),
            ),
        )
        .unwrap();
        let calls = job_create_calls(&c, &j);
        assert_eq!(
            calls[0],
            "DBMS_SCHEDULER.CREATE_JOB(job_name => E'nightly', job_type => E'STORED_PROCEDURE', job_action => E'purge', number_of_arguments => 1, start_date => CAST(E'2031-01-01T00:00:00' AS TIMESTAMP WITH TIME ZONE), repeat_interval => E'FREQ=DAILY;BYHOUR=3', end_date => NULL, enabled => FALSE, auto_drop => FALSE, comments => E'it''s')"
        );
        assert_eq!(
            calls[1],
            "DBMS_SCHEDULER.SET_JOB_ARGUMENT_VALUE(E'nightly', 1, E'30')"
        );
        assert_eq!(calls[2], "DBMS_SCHEDULER.ENABLE(E'nightly')");
        let script = job_create(&c, &j);
        assert!(script.starts_with("-- EDB Postgres Advanced Server: nuevo job «nightly» (DBMS_SCHEDULER)\nBEGIN\n  DBMS_SCHEDULER.CREATE_JOB("), "{script}");
        assert!(script.ends_with(";\nEND;\n"), "{script}");
        // With a program and a schedule.
        let p = job_spec(&c, &fv(json!({"name": "n2", "timing": "program", "program_name": "prog", "schedule_name": "sched"}), json!({}))).unwrap();
        assert_eq!(
            job_create_calls(&c, &p)[0],
            "DBMS_SCHEDULER.CREATE_JOB(job_name => E'n2', program_name => E'prog', schedule_name => E'sched', enabled => TRUE, auto_drop => TRUE, comments => NULL)"
        );
        assert!(job_spec(
            &c,
            &fv(json!({"name": "n", "timing": "program"}), json!({}))
        )
        .is_err());
        assert!(job_spec(
            &c,
            &fv(
                json!({"name": "n", "timing": "fields", "freq": "DAILY"}),
                json!({})
            )
        )
        .is_err());
        assert!(job_spec(
            &c,
            &fv(
                json!({"name": "n", "timing": "fields", "freq": "DAILY", "job_action": "x"}),
                json!({"arguments": [{"position": 1}, {"position": 1}]})
            )
        )
        .is_err());
    }

    #[test]
    fn edb_jobs_change_in_place_or_again() {
        let c = caps(Dialect::Edb);
        let base = json!({
            "name": "nightly", "timing": "expression", "job_type": "PLSQL_BLOCK",
            "job_action": "BEGIN NULL; END;", "repeat_interval": "FREQ=DAILY", "enabled": true
        });
        let cur = job_spec(
            &c,
            &fv(
                base.clone(),
                json!({"arguments": [{"position": 1, "value": "a"}]}),
            ),
        )
        .unwrap();
        assert_eq!(job_alter(&c, &cur, &cur), None);
        let mut off = cur.clone();
        off.enabled = false;
        let sql = job_alter(&c, &cur, &off).unwrap();
        assert!(
            sql.contains("BEGIN\n  DBMS_SCHEDULER.DISABLE(E'nightly');\nEND;"),
            "{sql}"
        );
        let mut args = cur.clone();
        args.args = vec![(1, "b".into()), (2, "c".into())];
        let sql = job_alter(&c, &cur, &args).unwrap();
        assert!(sql.contains("SET_JOB_ARGUMENT_VALUE(E'nightly', 1, E'b');\n  DBMS_SCHEDULER.SET_JOB_ARGUMENT_VALUE(E'nightly', 2, E'c');") && !sql.contains("DROP_JOB"), "{sql}");
        let mut other = cur.clone();
        other.own.as_mut().unwrap().repeat = Some("FREQ=HOURLY".into());
        let sql = job_alter(&c, &cur, &other).unwrap();
        assert!(
            sql.contains("DBMS_SCHEDULER.DROP_JOB(E'nightly');\n  DBMS_SCHEDULER.CREATE_JOB("),
            "{sql}"
        );
        assert!(sql.contains("no tiene SET_ATTRIBUTE"), "{sql}");
    }

    /// An EDB job that runs once (its own action, no repeat interval), as
    /// the view shows it: with its class.
    fn once_job() -> Job {
        Job {
            job: crate::jobs::scheduler::SchedJob {
                owner: Some("app".into()),
                name: "once".into(),
                job_type: Some("PLSQL_BLOCK".into()),
                action: Some("BEGIN NULL; END;".into()),
                start_iso: Some("2031-01-01T00:00:00Z".into()),
                class: Some("DEFAULT_JOB_CLASS".into()),
                enabled: Some(true),
                auto_drop: Some(false),
                number_of_arguments: Some("0".into()),
                ..Default::default()
            },
            owner: Some("app".into()),
            args: Vec::new(),
        }
    }

    #[test]
    fn a_job_that_runs_once_reads_back_as_no_change() {
        let c = caps(Dialect::Edb);
        let j = once_job();
        let (values, rows) = job_values(&j);
        assert_eq!(values["timing"], json!("once"));
        let form = FormValues {
            values: values.clone(),
            lists: BTreeMap::from([("arguments".to_string(), rows)]),
        };
        let spec = job_spec(&c, &form).unwrap();
        assert_eq!(spec.own.as_ref().unwrap().repeat, None);
        let cur = spec_of(&c, &j);
        assert_eq!(job_alter(&c, &cur, &spec), None);
        // Only "Habilitado": in place, even with the view's class.
        let mut off = form.clone();
        off.values.insert("enabled".into(), json!(false));
        let sql = job_alter(&c, &cur, &job_spec(&c, &off).unwrap()).unwrap();
        assert!(sql.contains("DBMS_SCHEDULER.DISABLE(E'once');"), "{sql}");
        assert!(!sql.contains("DROP_JOB") && !sql.contains("FREQ="), "{sql}");
        // A new job that runs once: no repeat interval.
        let new = job_spec(
            &c,
            &fv(
                json!({"name": "n", "timing": "once", "job_action": "BEGIN NULL; END;", "start_date": "2031-01-01T00:00:00"}),
                json!({}),
            ),
        )
        .unwrap();
        assert!(job_create_calls(&c, &new)[0].contains("repeat_interval => NULL"));
        // KingbaseES keeps comparing its class (the form has it).
        let k = caps(Dialect::Kingbase);
        assert_eq!(spec_of(&k, &j).class.as_deref(), Some("DEFAULT_JOB_CLASS"));
    }

    #[test]
    fn a_schedule_without_a_repeat_interval_reads_back_as_no_change() {
        let c = caps(Dialect::Edb);
        let x = Sched {
            name: "at_once".into(),
            start: Some("2031-01-01T00:00:00Z".into()),
            ..Default::default()
        };
        let mut m = BTreeMap::new();
        m.insert("name".to_string(), json!(x.name));
        m.insert("start_date".to_string(), json!("2031-01-01T00:00:00Z"));
        cal_values(&mut m, "rep_mode", x.interval.as_deref());
        assert_eq!(m["rep_mode"], json!("once"));
        let spec = sched_spec(&FormValues {
            values: m,
            lists: BTreeMap::new(),
        })
        .unwrap();
        assert_eq!(
            sched_alter(&c, &sched_spec_of(&x), &spec, &[]).unwrap(),
            None
        );
    }

    #[test]
    fn kingbase_jobs() {
        let c = caps(Dialect::Kingbase);
        let j = job_spec(&c, &fv(json!({"name": "kj", "program_name": "p", "schedule_name": "s", "job_class": "Data Export", "enabled": false}), json!({}))).unwrap();
        assert_eq!(
            job_create(&c, &j),
            "-- KingbaseES: nuevo job «kj» (DBMS_SCHEDULER)\nBEGIN;\nCALL DBMS_SCHEDULER.CREATE_JOB(E'kj', E'p', E's', E'Data Export', FALSE, TRUE, NULL);\nCOMMIT;\n"
        );
        let mut on = j.clone();
        on.enabled = true;
        assert!(job_alter(&c, &j, &on)
            .unwrap()
            .contains("CALL DBMS_SCHEDULER.ENABLE(E'kj');"));
        let mut other = j.clone();
        other.schedule = Some("s2".into());
        let sql = job_alter(&c, &j, &other).unwrap();
        assert!(sql.contains("CALL DBMS_SCHEDULER.DROP_JOB(E'kj');\nCALL DBMS_SCHEDULER.CREATE_JOB(E'kj', E'p', E's2'"), "{sql}");
        // Kingbase's jobs take a program and a schedule.
        assert!(job_spec(
            &c,
            &fv(json!({"name": "kj", "program_name": "p"}), json!({}))
        )
        .is_err());
    }

    #[test]
    fn programs() {
        let c = caps(Dialect::Edb);
        let p = prog_spec(
            &c,
            &fv(
                json!({"name": "purge", "program_type": "STORED_PROCEDURE", "program_action": "purge_logs", "number_of_arguments": 2, "enabled": true}),
                json!({"arguments": [
                    {"position": 2, "name": "keep", "type": "NUMBER", "default": "30"},
                    {"position": 1, "type": "VARCHAR2"}
                ]}),
            ),
        )
        .unwrap();
        assert_eq!(p.arguments[0].position, 1);
        let calls = prog_create_calls(&c, &p);
        assert_eq!(calls[0], "DBMS_SCHEDULER.CREATE_PROGRAM(program_name => E'purge', program_type => E'STORED_PROCEDURE', program_action => E'purge_logs', number_of_arguments => 2, enabled => FALSE, comments => NULL)");
        assert_eq!(calls[1], "DBMS_SCHEDULER.DEFINE_PROGRAM_ARGUMENT(program_name => E'purge', argument_position => 1, argument_name => NULL, argument_type => E'VARCHAR2')");
        assert_eq!(calls[2], "DBMS_SCHEDULER.DEFINE_PROGRAM_ARGUMENT(program_name => E'purge', argument_position => 2, argument_name => E'keep', argument_type => E'NUMBER', default_value => E'30', out_argument => FALSE)");
        assert_eq!(calls[3], "DBMS_SCHEDULER.ENABLE(E'purge')");
        // An argument past the count, a bad type, no action.
        assert!(prog_spec(
            &c,
            &fv(
                json!({"name": "p", "program_action": "x", "number_of_arguments": 0}),
                json!({"arguments": [{"position": 1, "type": "NUMBER"}]})
            )
        )
        .is_err());
        assert!(prog_spec(
            &c,
            &fv(
                json!({"name": "p", "program_action": "x", "program_type": "EXECUTABLE"}),
                json!({})
            )
        )
        .is_err());
        assert!(prog_spec(&c, &fv(json!({"name": "p"}), json!({}))).is_err());
        // In place: enabling, and its arguments while disabled.
        assert_eq!(prog_alter(&c, &p, &p, &[]).unwrap(), None);
        let mut off = p.clone();
        off.enabled = false;
        assert!(prog_alter(&c, &p, &off, &["j".into()])
            .unwrap()
            .unwrap()
            .contains("DBMS_SCHEDULER.DISABLE(E'purge');"));
        let mut args = p.clone();
        args.arguments[1].default = Some("60".into());
        let sql = prog_alter(&c, &p, &args, &["j".into()]).unwrap().unwrap();
        assert!(sql.contains("DISABLE(E'purge');\n  DBMS_SCHEDULER.DROP_PROGRAM_ARGUMENT(program_name => E'purge', argument_position => 2);\n  DBMS_SCHEDULER.DEFINE_PROGRAM_ARGUMENT(program_name => E'purge', argument_position => 2, argument_name => E'keep', argument_type => E'NUMBER', default_value => E'60', out_argument => FALSE);\n  DBMS_SCHEDULER.ENABLE(E'purge');"), "{sql}");
        // Anything else: again, only while no job uses it.
        let mut action = p.clone();
        action.action = "purge_more".into();
        assert!(prog_alter(&c, &p, &action, &["j1".into()])
            .unwrap_err()
            .to_string()
            .contains("j1"));
        assert!(prog_alter(&c, &p, &action, &[])
            .unwrap()
            .unwrap()
            .contains("DBMS_SCHEDULER.DROP_PROGRAM(E'purge');\n  DBMS_SCHEDULER.CREATE_PROGRAM("));
        // KingbaseES: positional, with the database it runs in.
        let k = caps(Dialect::Kingbase);
        let kp = prog_spec(&k, &fv(json!({"name": "kp", "program_type": "EXECUTABLE", "program_action": "/bin/true", "database": "db2"}), json!({}))).unwrap();
        assert_eq!(prog_create_calls(&k, &kp), vec!["DBMS_SCHEDULER.CREATE_PROGRAM(E'kp', E'EXECUTABLE', E'/bin/true', NULL, E'db2', 0, TRUE, NULL)".to_string()]);
    }

    #[test]
    fn schedules() {
        let c = caps(Dialect::Edb);
        let x = sched_spec(&fv(json!({"name": "weekdays", "rep_mode": "fields", "freq": "WEEKLY", "byday": ["MON", "FRI"], "byhour": "6", "start_date": "2031-01-01T00:00:00", "comments": "c"}), json!({}))).unwrap();
        assert_eq!(
            sched_create_call(&c, &x),
            "DBMS_SCHEDULER.CREATE_SCHEDULE(schedule_name => E'weekdays', start_date => CAST(E'2031-01-01T00:00:00' AS TIMESTAMP WITH TIME ZONE), repeat_interval => E'FREQ=WEEKLY;BYDAY=MON,FRI;BYHOUR=6', end_date => NULL, comments => E'c')"
        );
        let k = caps(Dialect::Kingbase);
        assert_eq!(
            sched_create_call(&k, &x),
            "DBMS_SCHEDULER.CREATE_SCHEDULE(E'weekdays', CAST(E'2031-01-01T00:00:00' AS TIMESTAMP WITH TIME ZONE), E'FREQ=WEEKLY;BYDAY=MON,FRI;BYHOUR=6', NULL, E'c')"
        );
        assert_eq!(sched_alter(&c, &x, &x, &[]).unwrap(), None);
        let mut y = x.clone();
        y.repeat = Some("FREQ=DAILY".into());
        assert!(sched_alter(&c, &x, &y, &["j".into()]).is_err());
        let sql = sched_alter(&c, &x, &y, &[]).unwrap().unwrap();
        assert!(
            sql.contains(
                "DBMS_SCHEDULER.DROP_SCHEDULE(E'weekdays');\n  DBMS_SCHEDULER.CREATE_SCHEDULE("
            ),
            "{sql}"
        );
        assert!(sched_spec(&fv(
            json!({"name": "s", "rep_mode": "expression", "repeat_interval": "FREQ=DAILY\n;x"}),
            json!({})
        ))
        .is_err());
        assert!(sched_spec(&fv(json!({"name": "s", "rep_mode": "fields", "freq": "DAILY", "start_date": "2031-02-01T00:00:00", "end_date": "2031-01-01T00:00:00"}), json!({}))).is_err());
    }

    #[test]
    fn owners_and_ids() {
        let c = caps(Dialect::Edb);
        assert!(c.owns(Some("app")) && !c.owns(Some("bob")) && !c.owns(None));
        let k = caps(Dialect::Kingbase);
        assert!(k.owns(None));
        assert!(parse_item("\"app\".\"p\"").is_ok() && parse_item("\"p\"").is_ok());
        for bad in ["p", "\"p\" OR 1=1", "\"a\".\"b\".\"c\"", ""] {
            assert!(parse_item(bad).is_err(), "{bad}");
        }
        assert_eq!(
            c.script("t", &["X(1)".into()]),
            "-- t\nBEGIN\n  X(1);\nEND;\n"
        );
        assert_eq!(
            k.script("t", &["X(1)".into()]),
            "-- t\nBEGIN;\nCALL X(1);\nCOMMIT;\n"
        );
        let kinds: Vec<String> = c.kinds().into_iter().map(|k| k.key).collect();
        assert_eq!(kinds, ["program", "schedule"]);
        // The job form: EDB has its own action and arguments, KingbaseES a
        // program and a schedule.
        let keys = |c: &Caps| -> Vec<String> {
            job_fields(c, false)
                .iter()
                .flat_map(|s| s.fields.iter().map(|f| f.key.clone()))
                .collect()
        };
        assert!(
            keys(&c).contains(&"job_action".to_string())
                && !keys(&c).contains(&"job_class".to_string())
        );
        assert!(
            !keys(&k).contains(&"job_action".to_string())
                && keys(&k).contains(&"job_class".to_string())
        );
    }
}
