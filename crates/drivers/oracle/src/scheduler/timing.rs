//! When a job, a window or a named schedule runs, as the designer edits it
//! and as `DBMS_SCHEDULER` takes it: timestamps with their time zone, and
//! the schedule's mode (once, a calendaring or PL/SQL repeat interval, a
//! named schedule, a window or window group, an event, a file watcher).

use super::calendar::{self, Calendar};
use super::{bad, name_arg, obj_id, shown, Block, Rec, Vals, A};
use dbine_driver::{FormField, FormKind, Result};
use serde_json::{json, Value};

/// A timestamp: local clock `YYYY-MM-DDTHH:MM:SS` and its time zone (a
/// region in upper case or an offset `±HH:MM`; empty: the session's).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Ts {
    pub(crate) local: String,
    pub(crate) tz: String,
}

/// `YYYY-MM-DD[T ]HH:MM[:SS][.fff][offset]` as `YYYY-MM-DDTHH:MM:SS` (an
/// offset the picker kept is dropped: the time zone field says it).
pub(crate) fn local_time(v: &str) -> Option<String> {
    let t = v.trim();
    if t.len() < 16 {
        return None;
    }
    let date = t.get(..10)?;
    let sep = t.as_bytes()[10];
    if sep != b'T' && sep != b' ' {
        return None;
    }
    let rest = &t[11..];
    let end = rest.find(['Z', 'z', '+', '-', '.']).unwrap_or(rest.len());
    let clock = &rest[..end];
    let parts: Vec<&str> = clock.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let num = |p: &str, max: u32| -> Option<u32> {
        (p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse().ok())
            .flatten()
            .filter(|n| *n <= max)
    };
    let h = num(parts[0], 23)?;
    let m = num(parts[1], 59)?;
    let s = match parts.get(2) {
        Some(p) => num(p, 59)?,
        None => 0,
    };
    let d = chrono_date(date)?;
    Some(format!("{d}T{h:02}:{m:02}:{s:02}"))
}

/// A real calendar date `YYYY-MM-DD`.
fn chrono_date(d: &str) -> Option<String> {
    let b = d.as_bytes();
    if d.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i32 = d[..4].parse().ok()?;
    let m: u32 = d[5..7].parse().ok()?;
    let day: u32 = d[8..].parse().ok()?;
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    (y >= 1 && (1..=days).contains(&day)).then(|| d.to_string())
}

/// A time zone Oracle could take: a region (`America/Argentina/Buenos_Aires`,
/// `PST8PDT`, `UTC`) or an offset (`-03:00`). The server says whether the
/// region exists.
pub(crate) fn tz_ok(tz: &str) -> bool {
    let b = tz.as_bytes();
    if matches!(b.first(), Some(b'+' | b'-')) {
        return b.len() == 6
            && b[1..3].iter().all(u8::is_ascii_digit)
            && b[3] == b':'
            && b[4..].iter().all(u8::is_ascii_digit)
            && tz[1..3].parse::<u32>().is_ok_and(|h| h <= 14)
            && tz[4..].parse::<u32>().is_ok_and(|m| m < 60);
    }
    !tz.is_empty()
        && tz.len() <= 64
        && b[0].is_ascii_alphabetic()
        && tz
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '/' | '-' | '+'))
}

/// The time zone as the dictionary writes it (regions in upper case).
pub(crate) fn tz_norm(tz: &str) -> String {
    tz.trim().to_ascii_uppercase()
}

impl Ts {
    /// The `TIMESTAMP WITH TIME ZONE` it is, as PL/SQL.
    pub(crate) fn expr(&self) -> String {
        let clock = self.local.replace('T', " ");
        if self.tz.is_empty() {
            format!(
                "TO_TIMESTAMP_TZ({}, 'YYYY-MM-DD HH24:MI:SS')",
                crate::jobs::lit(&clock)
            )
        } else {
            format!(
                "TO_TIMESTAMP_TZ({}, 'YYYY-MM-DD HH24:MI:SS TZR')",
                crate::jobs::lit(&format!("{clock} {}", self.tz))
            )
        }
    }
}

/// A date-time field with the form's time zone, checked.
pub(crate) fn ts_field(v: Vals, key: &str, label: &str, tz: &str) -> Result<Option<Ts>> {
    match v.opt(key) {
        None => Ok(None),
        Some(t) => match local_time(&t) {
            Some(local) => Ok(Some(Ts {
                local,
                tz: tz.to_string(),
            })),
            None => Err(bad(format!(
                "«{label}»: «{}» no es una fecha y hora válida (AAAA-MM-DD HH:MM:SS).",
                shown(&t)
            ))),
        },
    }
}

/// The form's time zone, checked.
pub(crate) fn tz_field(v: Vals) -> Result<String> {
    let tz = v.text("time_zone");
    if tz.is_empty() {
        return Ok(String::new());
    }
    if !tz_ok(&tz) {
        return Err(bad(format!(
            "«Zona horaria»: «{}» no es una zona horaria (una región como America/Argentina/Buenos_Aires o un desplazamiento como -03:00).",
            shown(&tz)
        )));
    }
    Ok(tz_norm(&tz))
}

/// A column pair of a [`Rec`] (`COL`, `COL_TZR`) as a timestamp.
pub(crate) fn ts_of(r: &Rec, col: &str) -> Option<Ts> {
    let local = r.t(col)?;
    Some(Ts {
        local,
        tz: r
            .t(&format!("{col}_TZR"))
            .map(|t| tz_norm(&t))
            .unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------
// The schedule's mode
// ---------------------------------------------------------------------------

/// When it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum When {
    /// As soon as it's enabled, once (a job without a schedule).
    Immediate,
    Once(Ts),
    Calendar {
        start: Option<Ts>,
        cal: Calendar,
        end: Option<Ts>,
    },
    /// A repeat interval as typed: calendaring syntax the fields can't say,
    /// or a PL/SQL expression.
    Expression {
        start: Option<Ts>,
        expr: String,
        end: Option<Ts>,
    },
    /// A named schedule (its id).
    Named(String),
    /// A window (its id, `SYS.NAME`).
    Window(String),
    WindowGroup(String),
    Event {
        start: Option<Ts>,
        /// `OWNER.QUEUE[, AGENT]`.
        queue: String,
        condition: Option<String>,
        end: Option<Ts>,
    },
    /// A file watcher (its id).
    FileWatcher(String),
}

/// What sets it: kinds of `When` that change from one another by nulling
/// one set of attributes and setting another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Immediate,
    Once,
    Repeat,
    Named,
    Event,
    FileWatcher,
}

impl When {
    pub(crate) fn mode(&self) -> &'static str {
        match self {
            When::Immediate => "immediate",
            When::Once(_) => "once",
            When::Calendar { .. } => "calendar",
            When::Expression { .. } => "expression",
            When::Named(_) => "named",
            When::Window(_) => "window",
            When::WindowGroup(_) => "window_group",
            When::Event { .. } => "event",
            When::FileWatcher(_) => "file_watcher",
        }
    }

    fn family(&self) -> Family {
        match self {
            When::Immediate => Family::Immediate,
            When::Once(_) => Family::Once,
            When::Calendar { .. } | When::Expression { .. } => Family::Repeat,
            When::Named(_) | When::Window(_) | When::WindowGroup(_) => Family::Named,
            When::Event { .. } => Family::Event,
            When::FileWatcher(_) => Family::FileWatcher,
        }
    }

    pub(crate) fn start(&self) -> Option<&Ts> {
        match self {
            When::Once(s) => Some(s),
            When::Calendar { start, .. }
            | When::Expression { start, .. }
            | When::Event { start, .. } => start.as_ref(),
            _ => None,
        }
    }

    pub(crate) fn end(&self) -> Option<&Ts> {
        match self {
            When::Calendar { end, .. } | When::Expression { end, .. } | When::Event { end, .. } => {
                end.as_ref()
            }
            _ => None,
        }
    }

    /// The repeat interval it writes.
    pub(crate) fn repeat(&self) -> Option<String> {
        match self {
            When::Calendar { cal, .. } => Some(cal.expression()),
            When::Expression { expr, .. } => Some(expr.clone()),
            _ => None,
        }
    }

    /// The named schedule, window or group it points at.
    pub(crate) fn schedule_name(&self) -> Option<&str> {
        match self {
            When::Named(n) | When::Window(n) | When::WindowGroup(n) => Some(n),
            _ => None,
        }
    }

    /// Read from a row of `*_SCHEDULER_JOBS`, `*_SCHEDULER_SCHEDULES` or
    /// `*_SCHEDULER_WINDOWS`.
    pub(crate) fn of(r: &Rec) -> When {
        if let Some(fw) = r.t("FILE_WATCHER_NAME") {
            return When::FileWatcher(obj_id(r.t("FILE_WATCHER_OWNER").as_deref(), &fw));
        }
        if let Some(name) = r.t("SCHEDULE_NAME") {
            let id = obj_id(r.t("SCHEDULE_OWNER").as_deref(), &name);
            return match r.t("SCHEDULE_TYPE").as_deref() {
                Some("WINDOW") => When::Window(id),
                Some("WINDOW_GROUP") => When::WindowGroup(id),
                _ => When::Named(id),
            };
        }
        let start = ts_of(r, "START_DATE");
        let end = ts_of(r, "END_DATE");
        if r.t("EVENT_QUEUE_NAME").is_some() || r.t("EVENT_CONDITION").is_some() {
            let mut queue = match (r.t("EVENT_QUEUE_OWNER"), r.t("EVENT_QUEUE_NAME")) {
                (Some(o), Some(q)) => format!("{o}.{q}"),
                (None, Some(q)) => q,
                _ => String::new(),
            };
            if let Some(a) = r.t("EVENT_QUEUE_AGENT") {
                queue = format!("{queue}, {a}");
            }
            return When::Event {
                start,
                queue,
                condition: r.t("EVENT_CONDITION"),
                end,
            };
        }
        if let Some(expr) = r.t("REPEAT_INTERVAL") {
            return match Calendar::parse(&expr) {
                Some(cal) => When::Calendar { start, cal, end },
                None => When::Expression { start, expr, end },
            };
        }
        match start {
            Some(s) => When::Once(s),
            None => When::Immediate,
        }
    }

    /// The designer's values for it.
    pub(crate) fn values(&self, put: &mut impl FnMut(&str, Value)) {
        put("schedule_mode", json!(self.mode()));
        let tz = self
            .start()
            .or(self.end())
            .map(|t| t.tz.clone())
            .unwrap_or_default();
        if !tz.is_empty() {
            put("time_zone", json!(tz));
        }
        if let Some(s) = self.start() {
            put("start_date", json!(s.local));
        }
        if let Some(e) = self.end() {
            put("end_date", json!(e.local));
        }
        match self {
            When::Calendar { cal, .. } => {
                cal.values(put);
                put("repeat_interval", json!(cal.expression()));
            }
            When::Expression { expr, .. } => put("repeat_interval", json!(expr)),
            When::Named(n) => put("schedule", json!(n)),
            When::Window(n) => put("window", json!(n)),
            When::WindowGroup(n) => put("window_group", json!(n)),
            When::Event {
                queue, condition, ..
            } => {
                put("event_queue", json!(queue));
                put(
                    "event_condition",
                    json!(condition.clone().unwrap_or_default()),
                );
            }
            When::FileWatcher(n) => put("file_watcher", json!(n)),
            When::Immediate | When::Once(_) => {}
        }
    }

    /// From the designer, checked: one of `modes`.
    pub(crate) fn from_values(v: Vals, modes: &[&str]) -> Result<When> {
        let mode = v.text("schedule_mode");
        if !modes.contains(&mode.as_str()) {
            return Err(bad(format!(
                "«Tipo de programación»: «{}» no es una opción.",
                shown(&mode)
            )));
        }
        let tz = tz_field(v)?;
        let start = ts_field(v, "start_date", "Inicio", &tz)?;
        let end = ts_field(v, "end_date", "Fin", &tz)?;
        if let (Some(s), Some(e)) = (&start, &end) {
            if e.local <= s.local {
                return Err(bad("«Fin» tiene que ser posterior a «Inicio».".into()));
            }
        }
        let named = |key: &str, label: &str| -> Result<String> {
            let t = v.text(key);
            if t.is_empty() {
                return Err(bad(format!("Falta «{label}».")));
            }
            name_arg(&t, label)?;
            Ok(t)
        };
        Ok(match mode.as_str() {
            "immediate" => When::Immediate,
            "once" => When::Once(start.ok_or_else(|| bad("Falta «Inicio».".into()))?),
            "calendar" => When::Calendar {
                start,
                cal: Calendar::from_values(v)?,
                end,
            },
            "expression" => When::Expression {
                start,
                expr: calendar::expression(v, "repeat_interval", "Expresión")?,
                end,
            },
            "named" => When::Named(named("schedule", "Programación")?),
            "window" => When::Window(named("window", "Ventana")?),
            "window_group" => When::WindowGroup(named("window_group", "Grupo de ventanas")?),
            "event" => {
                let queue = v.text("event_queue");
                if queue.is_empty() {
                    return Err(bad("Falta «Cola del evento».".into()));
                }
                if queue.len() > 400 {
                    return Err(bad("«Cola del evento» es demasiado larga.".into()));
                }
                When::Event {
                    start,
                    queue,
                    condition: v.opt("event_condition"),
                    end,
                }
            }
            "file_watcher" => When::FileWatcher(named("file_watcher", "Observador de archivos")?),
            _ => unreachable!(),
        })
    }
}

/// The `event_spec` attribute's two values.
fn event_args(w: &When) -> (A, A) {
    match w {
        When::Event {
            queue, condition, ..
        } => (A::opt(condition.clone()), A::S(queue.clone())),
        When::FileWatcher(fw) => (A::Null, A::Name(fw.clone())),
        _ => (A::Null, A::Null),
    }
}

fn ts_arg(t: Option<&Ts>) -> A {
    t.map_or(A::Null, |t| A::R(t.expr()))
}

/// A calendaring repeat interval that `new` sets (one that starts
/// `FREQ=`), checked with `EVALUATE_CALENDAR_STRING` where the block calls
/// it: at its top, so a string Oracle refuses (a typo, a BY clause its
/// frequency doesn't take) fails the save before a call changes anything.
/// A PL/SQL expression is left to the call that sets it.
pub(crate) fn check_first(b: &mut Block, old: &When, new: &When) {
    let Some(repeat) = new.repeat() else {
        return;
    };
    if old.repeat().as_deref() == Some(repeat.as_str())
        || !repeat
            .trim_start()
            .get(..5)
            .is_some_and(|f| f.eq_ignore_ascii_case("FREQ="))
    {
        return;
    }
    b.declare("next_run TIMESTAMP WITH TIME ZONE;");
    b.check(
        "DBMS_SCHEDULER.EVALUATE_CALENDAR_STRING",
        vec![
            ("calendar_string", A::S(repeat)),
            ("start_date", A::Null),
            ("return_date_after", A::Null),
            ("next_run_date", A::R("next_run".into())),
        ],
    );
}

/// The `SET_ATTRIBUTE` / `SET_ATTRIBUTE_NULL` calls that take `target`
/// from `old` to `new`. Between modes, what the old one set is nulled
/// first (Oracle refuses a named schedule while a repeat interval is set,
/// ORA-27488).
pub(crate) fn changes(b: &mut Block, target: &A, old: &When, new: &When) {
    if old == new {
        return;
    }
    let set = |b: &mut Block, attr: &str, a: A| b.set_attr(target, attr, a);
    if old.family() != new.family() {
        match old {
            When::Named(_) | When::Window(_) | When::WindowGroup(_) => {
                set(b, "schedule_name", A::Null)
            }
            When::FileWatcher(_) => set(b, "event_spec", A::Null),
            When::Event { .. } => set(b, "event_spec", A::Null),
            When::Calendar { .. } | When::Expression { .. } => set(b, "repeat_interval", A::Null),
            When::Once(_) | When::Immediate => {}
        }
        if old.start().is_some() && new.start().is_none() {
            set(b, "start_date", A::Null);
        }
        if old.end().is_some() && new.end().is_none() {
            set(b, "end_date", A::Null);
        }
        if new.start().is_some() && new.start() != old.start() {
            set(b, "start_date", ts_arg(new.start()));
        }
        match new {
            When::Calendar { .. } | When::Expression { .. } => {
                set(b, "repeat_interval", A::opt(new.repeat()))
            }
            When::Named(n) | When::Window(n) | When::WindowGroup(n) => {
                set(b, "schedule_name", A::Name(n.clone()))
            }
            When::Event { .. } | When::FileWatcher(_) => {
                let (v1, v2) = event_args(new);
                b.set_attr2(target, "event_spec", v1, v2);
            }
            When::Once(_) | When::Immediate => {}
        }
        if new.end().is_some() && new.end() != old.end() {
            set(b, "end_date", ts_arg(new.end()));
        }
        return;
    }
    if old.start() != new.start() {
        set(b, "start_date", ts_arg(new.start()));
    }
    match (old, new) {
        (When::Calendar { .. } | When::Expression { .. }, _) if old.repeat() != new.repeat() => {
            set(b, "repeat_interval", A::opt(new.repeat()))
        }
        (_, When::Named(n) | When::Window(n) | When::WindowGroup(n)) => {
            set(b, "schedule_name", A::Name(n.clone()))
        }
        (
            When::Event {
                queue: q1,
                condition: c1,
                ..
            },
            When::Event {
                queue: q2,
                condition: c2,
                ..
            },
        ) if q1 != q2 || c1 != c2 => {
            let (v1, v2) = event_args(new);
            b.set_attr2(target, "event_spec", v1, v2);
        }
        (When::FileWatcher(_), When::FileWatcher(_)) => {
            let (v1, v2) = event_args(new);
            b.set_attr2(target, "event_spec", v1, v2);
        }
        _ => {}
    }
    if old.end() != new.end() {
        set(b, "end_date", ts_arg(new.end()));
    }
}

/// A create call's schedule arguments for `w` (the overload they pick).
pub(crate) fn create_args(w: &When) -> Vec<(&'static str, A)> {
    match w {
        When::Immediate => Vec::new(),
        When::Once(s) => vec![("start_date", A::R(s.expr()))],
        When::Calendar { start, end, .. } | When::Expression { start, end, .. } => {
            let mut a = Vec::new();
            if let Some(s) = start {
                a.push(("start_date", A::R(s.expr())));
            }
            a.push(("repeat_interval", A::opt(w.repeat())));
            if let Some(e) = end {
                a.push(("end_date", A::R(e.expr())));
            }
            a
        }
        When::Named(n) | When::Window(n) | When::WindowGroup(n) => {
            vec![("schedule_name", A::Name(n.clone()))]
        }
        When::Event {
            start,
            queue,
            condition,
            end,
        } => {
            let mut a = Vec::new();
            if let Some(s) = start {
                a.push(("start_date", A::R(s.expr())));
            }
            a.push(("event_condition", A::opt(condition.clone())));
            a.push(("queue_spec", A::S(queue.clone())));
            if let Some(e) = end {
                a.push(("end_date", A::R(e.expr())));
            }
            a
        }
        When::FileWatcher(fw) => vec![
            ("event_condition", A::Null),
            ("queue_spec", A::Name(fw.clone())),
        ],
    }
}

// ---------------------------------------------------------------------------
// The fields
// ---------------------------------------------------------------------------

/// The modes a kind has, of all of them.
pub(crate) fn mode_select(has: &[&str]) -> FormKind {
    match FormKind::select(&[
        ("immediate", "Una vez, en cuanto se habilita"),
        ("once", "Una vez, en una fecha"),
        ("calendar", "Repetir (calendario)"),
        ("expression", "Repetir (expresión)"),
        ("named", "Programación con nombre"),
        ("window", "Ventana"),
        ("window_group", "Grupo de ventanas"),
        ("event", "Evento de una cola"),
        ("file_watcher", "Observador de archivos"),
    ]) {
        FormKind::Select { options } => FormKind::Select {
            options: options
                .into_iter()
                .filter(|(v, _)| has.contains(&v.as_str()))
                .collect(),
        },
        other => other,
    }
}

/// The schedule's fields for the modes a kind has.
pub(crate) fn fields(has: &[&str], default_tz: &str) -> Vec<FormField> {
    fn on<'a>(m: &[&'a str], has: &[&str]) -> Vec<&'a str> {
        m.iter().copied().filter(|x| has.contains(x)).collect()
    }
    let dated = on(&["once", "calendar", "expression", "event"], has);
    let ending = on(&["calendar", "expression", "event"], has);
    let mut out = vec![
        FormField::new("schedule_mode", "Tipo de programación", mode_select(has)).required(),
        FormField::new("time_zone", "Zona horaria", FormKind::Text)
            .placeholder("America/Argentina/Buenos_Aires")
            .help("La zona horaria de las fechas: una región o un desplazamiento como -03:00. Vacía: la de la sesión.")
            .default(json!(default_tz))
            .when("schedule_mode", &dated),
        FormField::new("start_date", "Inicio", FormKind::DateTime).when("schedule_mode", &dated),
        FormField::new("end_date", "Fin", FormKind::DateTime).when("schedule_mode", &ending),
    ];
    if has.contains(&"calendar") {
        let cal = ["calendar"];
        out.extend([
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
                    ("SECONDLY", "Cada segundo"),
                ]),
            )
            .default(json!("DAILY"))
            .when("schedule_mode", &cal),
            FormField::new("interval", "Cada", FormKind::number(Some(1.0), Some(999.0)))
                .help("Cada cuántas unidades de la frecuencia (INTERVAL).")
                .default(json!(1))
                .when("schedule_mode", &cal),
            FormField::new(
                "bymonth",
                "Meses",
                FormKind::multi(&[
                    ("JAN", "Enero"),
                    ("FEB", "Febrero"),
                    ("MAR", "Marzo"),
                    ("APR", "Abril"),
                    ("MAY", "Mayo"),
                    ("JUN", "Junio"),
                    ("JUL", "Julio"),
                    ("AUG", "Agosto"),
                    ("SEP", "Septiembre"),
                    ("OCT", "Octubre"),
                    ("NOV", "Noviembre"),
                    ("DEC", "Diciembre"),
                ]),
            )
            .when("schedule_mode", &cal),
            FormField::new("bymonthday", "Días del mes", FormKind::Text)
                .placeholder("1,15,-1")
                .help("Números del 1 al 31 separados por comas; los negativos cuentan desde el final del mes.")
                .when("schedule_mode", &cal),
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
            .when("schedule_mode", &cal),
            FormField::new("byhour", "Horas", FormKind::Text)
                .placeholder("3,15")
                .help("De 0 a 23, separadas por comas.")
                .when("schedule_mode", &cal),
            FormField::new("byminute", "Minutos", FormKind::Text)
                .placeholder("0,30")
                .help("De 0 a 59, separados por comas.")
                .when("schedule_mode", &cal),
            FormField::new("bysecond", "Segundos", FormKind::Text)
                .placeholder("0")
                .help("De 0 a 59, separados por comas.")
                .when("schedule_mode", &cal),
            // Oracle takes BYWEEKNO only with FREQ=YEARLY (ORA-27417).
            FormField::new("byweekno", "Semanas del año", FormKind::Text)
                .placeholder("1,-1")
                .help("Semanas ISO de -53 a 53, sin el 0, separadas por comas.")
                .when("schedule_mode", &cal)
                .and_when("freq", &calendar::WEEKNO_FREQS),
            FormField::new("byyearday", "Días del año", FormKind::Text)
                .placeholder("1,-1")
                .help("De -366 a 366, sin el 0, separados por comas.")
                .when("schedule_mode", &cal),
            // And BYSETPOS only with YEARLY or MONTHLY (ORA-27424).
            FormField::new("bysetpos", "Posiciones", FormKind::Text)
                .placeholder("-1")
                .help("BYSETPOS: cuáles de las fechas de cada período se toman (1 la primera, -1 la última).")
                .when("schedule_mode", &cal)
                .and_when("freq", &calendar::SETPOS_FREQS),
        ]);
    }
    if has.contains(&"expression") {
        out.push(
            FormField::new("repeat_interval", "Expresión", FormKind::Textarea)
                .placeholder("FREQ=MONTHLY;BYDAY=-1FRI;BYHOUR=22")
                .help("Una expresión de calendario de Oracle (también con BYDATE, INCLUDE, EXCLUDE u OFFSET) o una expresión PL/SQL que dé la próxima fecha.")
                .when("schedule_mode", &["expression"]),
        );
    }
    if has.contains(&"named") {
        out.push(
            FormField::new(
                "schedule",
                "Programación",
                FormKind::reference("schedule", false),
            )
            .when("schedule_mode", &["named"]),
        );
    }
    if has.contains(&"window") {
        out.push(
            FormField::new("window", "Ventana", FormKind::reference("window", false))
                .when("schedule_mode", &["window"]),
        );
    }
    if has.contains(&"window_group") {
        out.push(
            FormField::new(
                "window_group",
                "Grupo de ventanas",
                FormKind::reference("window_group", false),
            )
            .when("schedule_mode", &["window_group"]),
        );
    }
    if has.contains(&"event") {
        out.extend([
            FormField::new("event_queue", "Cola del evento", FormKind::Text)
                .placeholder("APP.EVENTOS_Q, AGENTE")
                .help("La cola (ESQUEMA.COLA) y, para una cola segura, el agente suscriptor después de una coma.")
                .when("schedule_mode", &["event"]),
            FormField::new("event_condition", "Condición del evento", FormKind::Textarea)
                .placeholder("tab.user_data.event_type = 'CARGA_TERMINADA'")
                .when("schedule_mode", &["event"]),
        ]);
    }
    if has.contains(&"file_watcher") {
        out.push(
            FormField::new(
                "file_watcher",
                "Observador de archivos",
                FormKind::reference("file_watcher", false),
            )
            .when("schedule_mode", &["file_watcher"]),
        );
    }
    out
}
