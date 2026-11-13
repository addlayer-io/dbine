//! Schedules: SQL Server Agent's `freq_*` model as the plain fields of
//! SSMS's "Job Schedule Properties" dialog (type, frequency, daily
//! frequency, duration), for a job's own schedules and for shared ones
//! (the "schedule" kind, SSMS's "Manage Schedules").
//!
//! The type folds SSMS's "schedule type" and "occurs" into one select
//! (one time, daily, weekly, monthly, monthly relative, when the Agent
//! starts, when the CPUs become idle), so every field depends on a single
//! other field, as the form's `when` can say.

use super::choices::{self, Catalog};
use super::{
    bad, date_field, date_text, fits, int_id, pick, shown, time_field, time_text, Ctx, Kind, Plan,
    Script, Vals, A, SYSNAME,
};
use crate::jobs::{self, describe, first_with, int, read_all, text, ScheduleRow};
use crate::SqlServerSession;
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result,
    SchedulerItem, SchedulerList,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const NOT_FOUND: &str = "La programación no existe o este usuario no puede verla.";
const OWNER_ONLY: &str =
    "Solo su propietario o un miembro de sysadmin puede cambiar esta programación.";

/// A schedule as msdb keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Freq {
    pub(crate) enabled: bool,
    pub(crate) freq_type: i64,
    pub(crate) interval: i64,
    pub(crate) subday_type: i64,
    pub(crate) subday_interval: i64,
    pub(crate) relative: i64,
    pub(crate) recurrence: i64,
    pub(crate) start_date: i64,
    pub(crate) end_date: i64,
    pub(crate) start_time: i64,
    pub(crate) end_time: i64,
}

impl Default for Freq {
    fn default() -> Self {
        Freq {
            enabled: true,
            freq_type: 4,
            interval: 1,
            subday_type: 1,
            subday_interval: 0,
            relative: 0,
            recurrence: 0,
            start_date: 0,
            end_date: 99991231,
            start_time: 0,
            end_time: 235959,
        }
    }
}

impl Freq {
    pub(crate) fn of(r: &ScheduleRow) -> Freq {
        Freq {
            enabled: r.enabled,
            freq_type: r.freq_type,
            interval: r.freq_interval,
            subday_type: r.subday_type,
            subday_interval: r.subday_interval,
            relative: r.relative_interval,
            recurrence: r.recurrence_factor,
            start_date: r.start_date,
            end_date: r.end_date,
            start_time: r.start_time,
            end_time: r.end_time,
        }
    }

    /// As a row, for [`describe`].
    pub(crate) fn row(&self, name: &str) -> ScheduleRow {
        ScheduleRow {
            id: None,
            name: name.to_string(),
            enabled: self.enabled,
            freq_type: self.freq_type,
            freq_interval: self.interval,
            subday_type: self.subday_type,
            subday_interval: self.subday_interval,
            relative_interval: self.relative,
            recurrence_factor: self.recurrence,
            start_date: self.start_date,
            end_date: self.end_date,
            start_time: self.start_time,
            end_time: self.end_time,
            uid: None,
            job_count: 1,
        }
    }

    fn pairs(&self) -> [(&'static str, i64); 11] {
        [
            ("enabled", i64::from(self.enabled)),
            ("freq_type", self.freq_type),
            ("freq_interval", self.interval),
            ("freq_subday_type", self.subday_type),
            ("freq_subday_interval", self.subday_interval),
            ("freq_relative_interval", self.relative),
            ("freq_recurrence_factor", self.recurrence),
            ("active_start_date", self.start_date),
            ("active_end_date", self.end_date),
            ("active_start_time", self.start_time),
            ("active_end_time", self.end_time),
        ]
    }

    /// Every argument of `sp_add_schedule`.
    pub(crate) fn args(&self) -> Vec<(&'static str, A)> {
        self.pairs()
            .into_iter()
            .map(|(k, v)| (k, A::I(v)))
            .collect()
    }

    /// The `sp_update_schedule` arguments that differ from `old`; when the
    /// type changes, every `freq_*` (msdb checks them together).
    pub(crate) fn changed(&self, old: &Freq) -> Vec<(&'static str, A)> {
        let retype = self.freq_type != old.freq_type;
        self.pairs()
            .into_iter()
            .zip(old.pairs())
            .filter(|((k, v), (_, o))| v != o || (retype && k.starts_with("freq_")))
            .map(|((k, v), _)| (k, A::I(v)))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Fields and values
// ---------------------------------------------------------------------------

const DAYS: [(&str, i64); 7] = [
    ("sunday", 1),
    ("monday", 2),
    ("tuesday", 4),
    ("wednesday", 8),
    ("thursday", 16),
    ("friday", 32),
    ("saturday", 64),
];

const RELATIVE_WEEK: [(&str, i64); 5] = [
    ("first", 1),
    ("second", 2),
    ("third", 4),
    ("fourth", 8),
    ("last", 16),
];

const RELATIVE_DAY: [(&str, i64); 10] = [
    ("sunday", 1),
    ("monday", 2),
    ("tuesday", 3),
    ("wednesday", 4),
    ("thursday", 5),
    ("friday", 6),
    ("saturday", 7),
    ("day", 8),
    ("weekday", 9),
    ("weekend_day", 10),
];

fn type_key(freq_type: i64) -> &'static str {
    match freq_type {
        1 => "once",
        8 => "weekly",
        16 => "monthly",
        32 => "monthly_relative",
        64 => "agent_start",
        128 => "idle",
        _ => "daily",
    }
}

fn type_code(key: &str) -> Option<i64> {
    Some(match key {
        "once" => 1,
        "daily" => 4,
        "weekly" => 8,
        "monthly" => 16,
        "monthly_relative" => 32,
        "agent_start" => 64,
        "idle" => 128,
        _ => return None,
    })
}

fn key_of(table: &[(&'static str, i64)], code: i64, default: &'static str) -> &'static str {
    table
        .iter()
        .find(|(_, c)| *c == code)
        .map_or(default, |(k, _)| *k)
}

fn code_of(table: &[(&'static str, i64)], key: &str) -> Option<i64> {
    table.iter().find(|(k, _)| *k == key).map(|(_, c)| *c)
}

/// The schedule's fields (a job's schedule row, or a shared schedule's
/// General page). `managed`: Azure SQL Managed Instance (no idle-CPU
/// schedules).
pub(crate) fn fields(managed: bool, today: i64) -> Vec<FormField> {
    let recurring = &["daily", "weekly", "monthly", "monthly_relative"];
    let mut kind = FormKind::select(&[
        ("daily", "Periódica: diaria"),
        ("weekly", "Periódica: semanal"),
        ("monthly", "Periódica: mensual"),
        ("monthly_relative", "Periódica: mensual relativa"),
        ("once", "Una vez"),
        ("agent_start", "Al iniciar el Agente SQL Server"),
        ("idle", "Cuando las CPU estén inactivas"),
    ]);
    if managed {
        if let FormKind::Select { options } = &mut kind {
            options.retain(|(v, _)| v != "idle");
        }
    }
    vec![
        FormField::new("name", "Nombre", FormKind::Text).required(),
        FormField::new("enabled", "Habilitada", FormKind::Bool).default(json!(true)),
        FormField::new("type", "Tipo de programación", kind)
            .required()
            .default(json!("daily")),
        FormField::new(
            "every_days",
            "Repetir cada (días)",
            FormKind::number(Some(1.0), Some(100.0)),
        )
        .when("type", &["daily"])
        .default(json!(1)),
        FormField::new(
            "every_weeks",
            "Repetir cada (semanas)",
            FormKind::number(Some(1.0), Some(100.0)),
        )
        .when("type", &["weekly"])
        .default(json!(1)),
        FormField::new(
            "weekdays",
            "Días",
            FormKind::multi(&[
                ("monday", "Lunes"),
                ("tuesday", "Martes"),
                ("wednesday", "Miércoles"),
                ("thursday", "Jueves"),
                ("friday", "Viernes"),
                ("saturday", "Sábado"),
                ("sunday", "Domingo"),
            ]),
        )
        .when("type", &["weekly"])
        .default(json!(["monday"])),
        FormField::new(
            "day_of_month",
            "Día del mes",
            FormKind::number(Some(1.0), Some(31.0)),
        )
        .when("type", &["monthly"])
        .default(json!(1)),
        FormField::new(
            "relative_week",
            "El",
            FormKind::select(&[
                ("first", "Primer"),
                ("second", "Segundo"),
                ("third", "Tercer"),
                ("fourth", "Cuarto"),
                ("last", "Último"),
            ]),
        )
        .when("type", &["monthly_relative"])
        .default(json!("first")),
        FormField::new(
            "relative_day",
            "Día de la semana",
            FormKind::select(&[
                ("monday", "Lunes"),
                ("tuesday", "Martes"),
                ("wednesday", "Miércoles"),
                ("thursday", "Jueves"),
                ("friday", "Viernes"),
                ("saturday", "Sábado"),
                ("sunday", "Domingo"),
                ("day", "Día"),
                ("weekday", "Día laborable"),
                ("weekend_day", "Día de fin de semana"),
            ]),
        )
        .when("type", &["monthly_relative"])
        .default(json!("monday")),
        FormField::new(
            "every_months",
            "De cada (meses)",
            FormKind::number(Some(1.0), Some(100.0)),
        )
        .when("type", &["monthly", "monthly_relative"])
        .default(json!(1)),
        FormField::new(
            "daily_frequency",
            "Frecuencia diaria",
            FormKind::select(&[
                ("once", "Una vez al día"),
                ("hours", "Cada N horas"),
                ("minutes", "Cada N minutos"),
                ("seconds", "Cada N segundos"),
            ]),
        )
        .when("type", recurring)
        .default(json!("once")),
        FormField::new("time", "Hora", FormKind::Time)
            .when(
                "type",
                &["once", "daily", "weekly", "monthly", "monthly_relative"],
            )
            .help(
                "La hora en que se ejecuta o, si se repite durante el día, la hora en que empieza.",
            )
            .default(json!("00:00:00")),
        FormField::new("every", "Cada", FormKind::number(Some(1.0), Some(86_399.0)))
            .when("daily_frequency", &["hours", "minutes", "seconds"])
            .help("Cada cuántas horas, minutos o segundos se repite (en segundos, al menos 10).")
            .default(json!(1)),
        FormField::new("end_time", "Hasta las", FormKind::Time)
            .when("daily_frequency", &["hours", "minutes", "seconds"])
            .default(json!("23:59:59")),
        FormField::new("start_date", "Fecha de inicio", FormKind::Date)
            .when(
                "type",
                &["once", "daily", "weekly", "monthly", "monthly_relative"],
            )
            .default(json!(date_text(today))),
        FormField::new("end_date", "Fecha de finalización", FormKind::Date)
            .when("type", recurring)
            .help("Vacía: sin fecha de finalización."),
        FormField::new("summary", "Resumen", FormKind::Textarea).read_only(true),
    ]
}

/// The fields' values for a schedule.
pub(crate) fn values(name: &str, f: &Freq) -> BTreeMap<String, Value> {
    let t = type_key(f.freq_type);
    let days: Vec<&str> = DAYS
        .iter()
        .filter(|(_, bit)| f.interval & bit != 0)
        .map(|(k, _)| *k)
        .collect();
    let repeating = matches!(f.subday_type, 2 | 4 | 8);
    let mut v = BTreeMap::new();
    let mut put = |k: &str, x: Value| {
        v.insert(k.to_string(), x);
    };
    put("name", json!(name));
    put("enabled", json!(f.enabled));
    put("type", json!(t));
    put(
        "every_days",
        json!(if t == "daily" { f.interval.max(1) } else { 1 }),
    );
    put(
        "every_weeks",
        json!(if t == "weekly" {
            f.recurrence.max(1)
        } else {
            1
        }),
    );
    put(
        "weekdays",
        json!(if t == "weekly" && !days.is_empty() {
            days
        } else {
            vec!["monday"]
        }),
    );
    put(
        "day_of_month",
        json!(if t == "monthly" {
            f.interval.clamp(1, 31)
        } else {
            1
        }),
    );
    put(
        "relative_week",
        json!(key_of(&RELATIVE_WEEK, f.relative, "first")),
    );
    put(
        "relative_day",
        json!(if t == "monthly_relative" {
            key_of(&RELATIVE_DAY, f.interval, "monday")
        } else {
            "monday"
        }),
    );
    put(
        "every_months",
        json!(if matches!(t, "monthly" | "monthly_relative") {
            f.recurrence.max(1)
        } else {
            1
        }),
    );
    put(
        "daily_frequency",
        json!(match f.subday_type {
            2 => "seconds",
            4 => "minutes",
            8 => "hours",
            _ => "once",
        }),
    );
    put("time", json!(time_text(f.start_time)));
    put(
        "every",
        json!(if repeating {
            f.subday_interval.max(1)
        } else {
            1
        }),
    );
    put("end_time", json!(time_text(f.end_time)));
    put(
        "start_date",
        json!(if f.start_date > 0 {
            date_text(f.start_date)
        } else {
            String::new()
        }),
    );
    put(
        "end_date",
        json!(if f.end_date > 0 && f.end_date < 99991231 {
            date_text(f.end_date)
        } else {
            String::new()
        }),
    );
    put("summary", json!(describe(&f.row(name))));
    v
}

/// The schedule the fields describe, checked as msdb would, keeping what
/// doesn't apply to its type from `base` (what msdb has now) so an
/// untouched schedule reads back unchanged.
pub(crate) fn from_values(
    v: Vals,
    base: Option<&Freq>,
    today: i64,
    managed: bool,
) -> Result<(String, Freq)> {
    let name = v
        .opt("name")
        .ok_or_else(|| bad("Cada programación necesita un nombre.".into()))?;
    fits("Nombre", &name, SYSNAME)?;
    let n = shown(&name);
    let type_key = v.text("type");
    let freq_type = type_code(&type_key).ok_or_else(|| {
        bad(format!(
            "La programación «{n}» no tiene un tipo válido («{}»).",
            shown(&type_key)
        ))
    })?;
    if managed && freq_type == 128 {
        return Err(bad(format!(
            "La programación «{n}»: Azure SQL Managed Instance no admite programaciones de CPU inactiva."
        )));
    }
    let same = base.filter(|b| b.freq_type == freq_type);
    let enabled = if v.0.get("enabled").is_some_and(|x| !x.is_null()) {
        v.bool("enabled")
    } else {
        true
    };
    let start_date = date_field(v, "start_date", "Fecha de inicio")?
        .or(same.map(|b| b.start_date).filter(|d| *d > 0))
        .unwrap_or(today);
    let start_time = time_field(v, "time", "Hora", same.map_or(0, |b| b.start_time))?;
    let mut f = Freq {
        enabled,
        freq_type,
        interval: 0,
        subday_type: 0,
        subday_interval: 0,
        relative: 0,
        recurrence: 0,
        start_date,
        end_date: same.map_or(99991231, |b| b.end_date),
        start_time,
        end_time: same.map_or(235959, |b| b.end_time),
    };
    if matches!(freq_type, 1 | 64 | 128) {
        // msdb keeps no frequency for these.
        if freq_type != 1 {
            f.start_time = same.map_or(0, |b| b.start_time);
            f.start_date = same
                .map(|b| b.start_date)
                .filter(|d| *d > 0)
                .unwrap_or(today);
        }
        return Ok((name, f));
    }
    let keep = |get: fn(&Freq) -> i64| same.map_or(0, get);
    match freq_type {
        4 => {
            f.interval = v
                .int("every_days", "Repetir cada (días)", 1, 32767)?
                .unwrap_or(1);
            f.recurrence = keep(|b| b.recurrence);
            f.relative = keep(|b| b.relative);
        }
        8 => {
            let mut bits = 0;
            for d in v.strings("weekdays") {
                bits |= code_of(&DAYS, &d).ok_or_else(|| {
                    bad(format!(
                        "La programación «{n}»: «{}» no es un día de la semana.",
                        shown(&d)
                    ))
                })?;
            }
            if bits == 0 {
                return Err(bad(format!(
                    "La programación «{n}» es semanal: elegí al menos un día."
                )));
            }
            f.interval = bits;
            f.recurrence = v
                .int("every_weeks", "Repetir cada (semanas)", 1, 32767)?
                .unwrap_or(1);
            f.relative = keep(|b| b.relative);
        }
        16 => {
            f.interval = v.int("day_of_month", "Día del mes", 1, 31)?.unwrap_or(1);
            f.recurrence = v
                .int("every_months", "De cada (meses)", 1, 32767)?
                .unwrap_or(1);
            f.relative = keep(|b| b.relative);
        }
        _ => {
            let week = v.text("relative_week");
            f.relative = code_of(&RELATIVE_WEEK, &week).ok_or_else(|| {
                bad(format!(
                    "La programación «{n}»: «{}» no es una semana del mes (primer, segundo, tercer, cuarto, último).",
                    shown(&week)
                ))
            })?;
            let day = v.text("relative_day");
            f.interval = code_of(&RELATIVE_DAY, &day).ok_or_else(|| {
                bad(format!(
                    "La programación «{n}»: «{}» no es un día válido.",
                    shown(&day)
                ))
            })?;
            f.recurrence = v
                .int("every_months", "De cada (meses)", 1, 32767)?
                .unwrap_or(1);
        }
    }
    match v.text("daily_frequency").as_str() {
        "" | "once" => {
            let was_once = same.filter(|b| matches!(b.subday_type, 0 | 1));
            f.subday_type = was_once.map_or(1, |b| b.subday_type);
            f.subday_interval = was_once.map_or(0, |b| b.subday_interval);
            f.end_time = was_once.map_or(235959, |b| b.end_time);
        }
        unit => {
            let (code, min, max) = match unit {
                "hours" => (8, 1, 23),
                "minutes" => (4, 1, 1439),
                "seconds" => (2, 10, 86399),
                other => {
                    return Err(bad(format!(
                        "La programación «{n}»: «{}» no es una frecuencia diaria válida.",
                        shown(other)
                    )))
                }
            };
            f.subday_type = code;
            f.subday_interval = v.int("every", "Cada", min, max)?.unwrap_or(min);
            f.end_time = time_field(v, "end_time", "Hasta las", 235959)?;
            if f.end_time == f.start_time {
                return Err(bad(format!(
                    "La programación «{n}» se repite durante el día: la hora de inicio y la de fin no pueden ser iguales."
                )));
            }
        }
    }
    f.end_date = date_field(v, "end_date", "Fecha de finalización")?.unwrap_or(99991231);
    if f.end_date < f.start_date {
        return Err(bad(format!(
            "La programación «{n}» termina antes de empezar: la fecha de finalización es anterior a la de inicio."
        )));
    }
    Ok((name, f))
}

/// Today on the server's clock, as `yyyymmdd`.
pub(crate) fn today(c: &Ctx) -> i64 {
    let now = chrono::Utc::now().naive_utc() + chrono::Duration::minutes(c.p.offset_min);
    now.format("%Y%m%d").to_string().parse().unwrap_or(20000101)
}

// ---------------------------------------------------------------------------
// The "schedule" kind: shared schedules
// ---------------------------------------------------------------------------

/// One schedule, `@P1` its id, with its owner and the jobs that use it.
const ONE_SQL: &str = "SET NOCOUNT ON;
IF EXISTS (SELECT 1 FROM msdb.dbo.sysschedules_localserver_view WHERE schedule_id = @P1)
BEGIN
    EXEC msdb.dbo.sp_help_schedule @schedule_id = @P1;
    SELECT SUSER_SNAME(owner_sid) AS dbine_owner FROM msdb.dbo.sysschedules_localserver_view WHERE schedule_id = @P1;
    EXEC msdb.dbo.sp_help_jobs_in_schedule @schedule_id = @P1;
END;";

pub(crate) struct Shared {
    pub(crate) id: i64,
    pub(crate) row: ScheduleRow,
    pub(crate) owner: Option<String>,
    pub(crate) jobs: Vec<String>,
}

async fn read(s: &mut SqlServerSession, id: &str) -> Result<Shared> {
    let n = int_id(Kind::Schedule, id)?;
    let ids = n.to_string();
    let sets = jobs::sets(s, ONE_SQL, &[&ids], None).await?;
    let Some(row) = first_with(&sets, "schedule_uid")
        .and_then(|s| s.rows.first())
        .and_then(ScheduleRow::read)
    else {
        return Err(Error::Query(NOT_FOUND.into()));
    };
    let owner = first_with(&sets, "dbine_owner")
        .and_then(|s| s.rows.first())
        .and_then(|r| text(r, "dbine_owner"));
    let jobs = sets
        .iter()
        .filter(|s| s.has("job_id") && s.has("name"))
        .flat_map(|s| s.rows.iter())
        .filter_map(|r| text(r, "name"))
        .collect();
    Ok(Shared {
        id: n,
        row,
        owner,
        jobs,
    })
}

pub(crate) async fn items(s: &mut SqlServerSession, _c: &Ctx) -> Result<SchedulerList> {
    let sets = jobs::sets(
        s,
        "SET NOCOUNT ON;\nEXEC msdb.dbo.sp_help_schedule;",
        &[],
        None,
    )
    .await?;
    let items = read_all(first_with(&sets, "schedule_uid"), |r| {
        Some((int(r, "schedule_id")?, ScheduleRow::read(r)?))
    })
    .into_iter()
    .map(|(id, row)| SchedulerItem {
        id: id.to_string(),
        name: row.name.clone(),
        description: Some(usage(&row)),
        enabled: Some(row.enabled),
        system: false,
        can_edit: None,
        can_delete: None,
        actions: None,
    })
    .collect();
    Ok(SchedulerList {
        items,
        notes: Vec::new(),
    })
}

/// The schedule in words and how many jobs use it.
fn usage(row: &ScheduleRow) -> String {
    let what = describe(&ScheduleRow {
        enabled: true,
        ..row.clone()
    });
    match row.job_count {
        0 => format!("{what} (sin jobs)"),
        1 => format!("{what} (la usa 1 job)"),
        n => format!("{what} (la usan {n} jobs)"),
    }
}

pub(crate) async fn form(
    s: &mut SqlServerSession,
    c: &Ctx,
    id: Option<&str>,
) -> Result<ObjectForm> {
    let today = today(c);
    let catalog = choices::load(s, c, &["login"]).await?;
    let mut general = FormSection::new("general", "General").fields(fields(c.managed(), today));
    let mut f = ObjectForm::default();
    let (values, owner, jobs_rows, read_only) = match id {
        None => (BTreeMap::new(), Some(c.p.login.clone()), Vec::new(), false),
        Some(id) => {
            let sh = read(s, id).await?;
            let freq = Freq::of(&sh.row);
            let rows = sh
                .jobs
                .iter()
                .map(|j| BTreeMap::from([("job".to_string(), json!(j))]))
                .collect();
            let mine = c.p.sysadmin || c.is(sh.owner.as_deref());
            (values(&sh.row.name, &freq), sh.owner, rows, !mine)
        }
    };
    general = general.field(
        FormField::new("owner", "Propietario", FormKind::reference("login", false))
            .read_only(!c.admin())
            .default(json!(c.p.login)),
    );
    f.values = values;
    f.values
        .insert("owner".into(), json!(owner.unwrap_or_default()));
    f.sections = vec![
        general,
        FormSection::new("jobs", "Jobs").list(
            FormList::new(
                "jobs",
                "Jobs que usan esta programación",
                "job",
                vec![FormField::new("job", "Job", FormKind::Text).read_only(true)],
            )
            .rows(jobs_rows)
            .read_only(true),
        ),
    ];
    f.choices = catalog.lists;
    if read_only {
        f.read_only = true;
        f.notes.push(OWNER_ONLY.into());
    }
    Ok(f)
}

pub(crate) async fn plan(
    s: &mut SqlServerSession,
    c: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    let catalog = if c.admin() {
        choices::load(s, c, &["login"]).await?
    } else {
        Catalog::default()
    };
    let v = Vals(&values.values);
    let owner = match v.opt("owner").filter(|_| c.admin()) {
        Some(o) => Some(
            pick(&o, catalog.get("login"), |o| {
                format!("«{o}» no es un inicio de sesión de este servidor.")
            })?
            .to_string(),
        ),
        None => None,
    };
    match id {
        None => {
            let (name, freq) = from_values(v, None, today(c), c.managed())?;
            Ok(Plan {
                script: create_script(&name, &freq, owner.as_deref(), &c.p.login),
                empty: false,
                id: None,
                output: Some("schedule_id"),
                lookup: None,
            })
        }
        Some(id) => {
            let sh = read(s, id).await?;
            if !(c.p.sysadmin || c.is(sh.owner.as_deref())) {
                return Err(Error::Query(OWNER_ONLY.into()));
            }
            let old = Freq::of(&sh.row);
            let (name, freq) = from_values(v, Some(&old), today(c), c.managed())?;
            let owner = owner.filter(|o| {
                !sh.owner
                    .as_deref()
                    .is_some_and(|x| x.eq_ignore_ascii_case(o))
            });
            let script = alter_script(sh.id, &sh.row.name, &name, &old, &freq, owner.as_deref());
            Ok(match script {
                Some(script) => Plan {
                    script,
                    empty: false,
                    id: Some(sh.id.to_string()),
                    output: None,
                    lookup: None,
                },
                None => Plan::nothing(&sh.row.name, sh.id.to_string()),
            })
        }
    }
}

/// `sp_add_schedule`, its id returned after the commit.
pub(crate) fn create_script(name: &str, f: &Freq, owner: Option<&str>, login: &str) -> String {
    let mut sc = Script::new(
        &format!("Programación «{name}» del Agente SQL Server"),
        &["@scheduleId int"],
    );
    let mut args = vec![("schedule_name", A::T(name.into()))];
    args.extend(f.args());
    if let Some(o) = owner.filter(|o| !o.eq_ignore_ascii_case(login)) {
        args.push(("owner_login_name", A::T(o.into())));
    }
    args.push(("schedule_id", A::R("@scheduleId OUTPUT".into())));
    sc.call("msdb.dbo.sp_add_schedule", args);
    sc.finish(Some("SELECT @scheduleId AS schedule_id;"))
}

/// `sp_update_schedule` with what changed, or `None`.
pub(crate) fn alter_script(
    id: i64,
    old_name: &str,
    name: &str,
    old: &Freq,
    f: &Freq,
    owner: Option<&str>,
) -> Option<String> {
    let mut args = vec![("schedule_id", A::I(id))];
    if name != old_name {
        args.push(("new_name", A::T(name.into())));
    }
    args.extend(f.changed(old));
    if let Some(o) = owner {
        args.push(("owner_login_name", A::T(o.into())));
    }
    if args.len() == 1 {
        return None;
    }
    let mut sc = Script::new(
        &format!("Programación «{old_name}» del Agente SQL Server"),
        &[],
    );
    sc.call("msdb.dbo.sp_update_schedule", args);
    Some(sc.finish(None))
}

pub(crate) async fn delete_script(s: &mut SqlServerSession, c: &Ctx, id: &str) -> Result<String> {
    let sh = read(s, id).await?;
    if !(c.p.sysadmin || c.is(sh.owner.as_deref())) && c.writable() {
        return Err(Error::Query(OWNER_ONLY.into()));
    }
    Ok(drop_script(sh.id, &sh.row.name, &sh.jobs))
}

/// `sp_delete_schedule`; when jobs use it, `@force_delete` detaches it
/// from them first (as SSMS does after asking).
pub(crate) fn drop_script(id: i64, name: &str, jobs: &[String]) -> String {
    let mut sc = Script::new(
        &format!("Borra la programación «{name}» del Agente SQL Server"),
        &[],
    );
    let mut args = vec![("schedule_id", A::I(id))];
    if !jobs.is_empty() {
        sc.comment(&format!(
            "La usan {} jobs: se quita de ellos ({}).",
            jobs.len(),
            jobs.join(", ")
        ));
        args.push(("force_delete", A::I(1)));
    }
    sc.call("msdb.dbo.sp_delete_schedule", args);
    sc.finish(None)
}
