//! Oracle's calendaring syntax (`FREQ=…;INTERVAL=…;BY…=…`), as the
//! designer's plain fields: an expression the fields can say is read into
//! them, any other (a PL/SQL expression, a named schedule in `FREQ`,
//! `BYDATE`, `INCLUDE` / `EXCLUDE` / `INTERSECT`, `OFFSET`, `BYDAY` with an
//! ordinal) stays as the expression it is.

use super::{bad, shown, Vals};
use dbine_driver::Result;

pub(crate) const FREQS: [&str; 7] = [
    "YEARLY", "MONTHLY", "WEEKLY", "DAILY", "HOURLY", "MINUTELY", "SECONDLY",
];
pub(crate) const MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];
pub(crate) const DAYS: [&str; 7] = ["MON", "TUE", "WED", "THU", "FRI", "SAT", "SUN"];
const DAY_NAMES: [&str; 7] = [
    "MONDAY",
    "TUESDAY",
    "WEDNESDAY",
    "THURSDAY",
    "FRIDAY",
    "SATURDAY",
    "SUNDAY",
];
const MONTH_NAMES: [&str; 12] = [
    "JANUARY",
    "FEBRUARY",
    "MARCH",
    "APRIL",
    "MAY",
    "JUNE",
    "JULY",
    "AUGUST",
    "SEPTEMBER",
    "OCTOBER",
    "NOVEMBER",
    "DECEMBER",
];

/// A repeat interval the designer's fields say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Calendar {
    pub(crate) freq: String,
    pub(crate) interval: i64,
    pub(crate) bymonth: Vec<String>,
    pub(crate) byweekno: Vec<i64>,
    pub(crate) byyearday: Vec<i64>,
    pub(crate) bymonthday: Vec<i64>,
    pub(crate) byday: Vec<String>,
    pub(crate) byhour: Vec<i64>,
    pub(crate) byminute: Vec<i64>,
    pub(crate) bysecond: Vec<i64>,
    pub(crate) bysetpos: Vec<i64>,
}

impl Default for Calendar {
    fn default() -> Self {
        Calendar {
            freq: "DAILY".into(),
            interval: 1,
            bymonth: Vec::new(),
            byweekno: Vec::new(),
            byyearday: Vec::new(),
            bymonthday: Vec::new(),
            byday: Vec::new(),
            byhour: Vec::new(),
            byminute: Vec::new(),
            bysecond: Vec::new(),
            bysetpos: Vec::new(),
        }
    }
}

/// The frequencies Oracle takes BYWEEKNO with (ORA-27417 with the rest).
pub(crate) const WEEKNO_FREQS: [&str; 1] = ["YEARLY"];
/// The frequencies Oracle takes BYSETPOS with (ORA-27424 with the rest).
pub(crate) const SETPOS_FREQS: [&str; 2] = ["YEARLY", "MONTHLY"];

/// The ranges each list takes (0 is never a value of the signed ones).
const BYWEEKNO: (i64, i64) = (-53, 53);
const BYYEARDAY: (i64, i64) = (-366, 366);
const BYMONTHDAY: (i64, i64) = (-31, 31);
const BYSETPOS: (i64, i64) = (-9999, 9999);

fn ints(text: &str, (min, max): (i64, i64), signed: bool) -> Option<Vec<i64>> {
    let mut out = Vec::new();
    for part in text.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        let digits = p.strip_prefix(['-', '+']).unwrap_or(p);
        if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let n: i64 = p.parse().ok()?;
        if n < min || n > max || (signed && n == 0) {
            return None;
        }
        out.push(n);
    }
    Some(out)
}

fn month(v: &str) -> Option<String> {
    let u = v.trim().to_ascii_uppercase();
    if let Ok(n) = u.parse::<usize>() {
        return (1..=12).contains(&n).then(|| MONTHS[n - 1].to_string());
    }
    MONTHS
        .iter()
        .position(|m| *m == u)
        .or_else(|| MONTH_NAMES.iter().position(|m| *m == u))
        .map(|i| MONTHS[i].to_string())
}

fn day(v: &str) -> Option<String> {
    let u = v.trim().to_ascii_uppercase();
    DAYS.iter()
        .position(|d| *d == u)
        .or_else(|| DAY_NAMES.iter().position(|d| *d == u))
        .map(|i| DAYS[i].to_string())
}

impl Calendar {
    /// The fields of `expr`, when they can say it all.
    pub(crate) fn parse(expr: &str) -> Option<Calendar> {
        let mut c = Calendar::default();
        let mut freq = None;
        for clause in expr.split(';') {
            let clause = clause.trim();
            if clause.is_empty() {
                continue;
            }
            let (k, v) = clause.split_once('=')?;
            let v = v.trim();
            match k.trim().to_ascii_uppercase().as_str() {
                "FREQ" => {
                    let f = v.to_ascii_uppercase();
                    if !FREQS.contains(&f.as_str()) || freq.is_some() {
                        return None;
                    }
                    freq = Some(f);
                }
                "INTERVAL" => {
                    let n: i64 = v.parse().ok()?;
                    if !(1..=999).contains(&n) {
                        return None;
                    }
                    c.interval = n;
                }
                "BYMONTH" => {
                    c.bymonth = v.split(',').map(month).collect::<Option<Vec<String>>>()?;
                }
                "BYWEEKNO" => c.byweekno = ints(v, BYWEEKNO, true)?,
                "BYYEARDAY" => c.byyearday = ints(v, BYYEARDAY, true)?,
                "BYMONTHDAY" => c.bymonthday = ints(v, BYMONTHDAY, true)?,
                "BYDAY" => {
                    c.byday = v.split(',').map(day).collect::<Option<Vec<String>>>()?;
                }
                "BYHOUR" => c.byhour = ints(v, (0, 23), false)?,
                "BYMINUTE" => c.byminute = ints(v, (0, 59), false)?,
                "BYSECOND" => c.bysecond = ints(v, (0, 59), false)?,
                "BYSETPOS" => c.bysetpos = ints(v, BYSETPOS, true)?,
                _ => return None,
            }
        }
        c.freq = freq?;
        Some(c)
    }

    /// The expression, in a fixed order.
    pub(crate) fn expression(&self) -> String {
        fn list<T: ToString>(v: &[T]) -> String {
            v.iter().map(T::to_string).collect::<Vec<_>>().join(",")
        }
        let mut parts = vec![format!("FREQ={}", self.freq)];
        if self.interval != 1 {
            parts.push(format!("INTERVAL={}", self.interval));
        }
        for (k, v) in [
            ("BYMONTH", list(&self.bymonth)),
            ("BYWEEKNO", list(&self.byweekno)),
            ("BYYEARDAY", list(&self.byyearday)),
            ("BYMONTHDAY", list(&self.bymonthday)),
            ("BYDAY", list(&self.byday)),
            ("BYHOUR", list(&self.byhour)),
            ("BYMINUTE", list(&self.byminute)),
            ("BYSECOND", list(&self.bysecond)),
            ("BYSETPOS", list(&self.bysetpos)),
        ] {
            if !v.is_empty() {
                parts.push(format!("{k}={v}"));
            }
        }
        parts.join(";")
    }

    /// The fields from the designer, checked.
    pub(crate) fn from_values(v: Vals) -> Result<Calendar> {
        let freq = v.text("freq").to_ascii_uppercase();
        if !FREQS.contains(&freq.as_str()) {
            return Err(bad(format!(
                "«Frecuencia»: «{}» no es una frecuencia de Oracle.",
                shown(&freq)
            )));
        }
        let list = |k: &str, label: &str, range: (i64, i64), signed: bool| -> Result<Vec<i64>> {
            let t = v.text(k);
            ints(&t, range, signed).ok_or_else(|| {
                bad(format!(
                    "«{label}» tiene que ser una lista de números entre {} y {}{} separados por comas (es «{}»).",
                    range.0,
                    range.1,
                    if signed { ", sin el 0," } else { "" },
                    shown(&t)
                ))
            })
        };
        let mut bymonth = Vec::new();
        for m in v.strings("bymonth") {
            bymonth.push(
                month(&m).ok_or_else(|| bad(format!("«Meses»: «{}» no es un mes.", shown(&m))))?,
            );
        }
        let mut byday = Vec::new();
        for d in v.strings("byday") {
            byday.push(day(&d).ok_or_else(|| {
                bad(format!(
                    "«Días de la semana»: «{}» no es un día.",
                    shown(&d)
                ))
            })?);
        }
        // The form hides the lists the frequency doesn't take: what they
        // still hold doesn't apply.
        let byweekno = if WEEKNO_FREQS.contains(&freq.as_str()) {
            list("byweekno", "Semanas del año", BYWEEKNO, true)?
        } else {
            Vec::new()
        };
        let bysetpos = if SETPOS_FREQS.contains(&freq.as_str()) {
            list("bysetpos", "Posiciones", BYSETPOS, true)?
        } else {
            Vec::new()
        };
        Ok(Calendar {
            interval: v.int("interval", "Cada", 1, 999)?.unwrap_or(1),
            bymonth,
            byweekno,
            byyearday: list("byyearday", "Días del año", BYYEARDAY, true)?,
            bymonthday: list("bymonthday", "Días del mes", BYMONTHDAY, true)?,
            byday,
            byhour: list("byhour", "Horas", (0, 23), false)?,
            byminute: list("byminute", "Minutos", (0, 59), false)?,
            bysecond: list("bysecond", "Segundos", (0, 59), false)?,
            bysetpos,
            freq,
        })
    }

    /// The designer's values for the fields.
    pub(crate) fn values(&self, put: &mut impl FnMut(&str, serde_json::Value)) {
        use serde_json::json;
        fn text(v: &[i64]) -> String {
            v.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
        }
        put("freq", json!(self.freq));
        put("interval", json!(self.interval));
        put("bymonth", json!(self.bymonth));
        put("byweekno", json!(text(&self.byweekno)));
        put("byyearday", json!(text(&self.byyearday)));
        put("bymonthday", json!(text(&self.bymonthday)));
        put("byday", json!(self.byday));
        put("byhour", json!(text(&self.byhour)));
        put("byminute", json!(text(&self.byminute)));
        put("bysecond", json!(text(&self.bysecond)));
        put("bysetpos", json!(text(&self.bysetpos)));
    }
}

/// A repeat interval as typed: a calendaring expression or a PL/SQL one.
/// Checked only for what can't reach the server: empty, too long.
pub(crate) fn expression(v: Vals, key: &str, label: &str) -> Result<String> {
    let t = v.text(key);
    if t.is_empty() {
        return Err(bad(format!("Falta «{label}».")));
    }
    if t.len() > 4000 {
        return Err(bad(format!(
            "«{label}» tiene más de 4000 bytes, lo que Oracle acepta."
        )));
    }
    Ok(t)
}
