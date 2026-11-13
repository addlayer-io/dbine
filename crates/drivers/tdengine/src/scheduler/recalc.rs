//! "Recalcular un rango de tiempo" (`recalculate`, a stream's action that
//! asks for values): the new stream engine's `RECALCULATE STREAM
//! [db.]name FROM start [TO end]` (3.3.7 on). TDengine computes the
//! stream's windows in that range of event time (the trigger table's
//! timestamps) again with the data there is now, and their results take
//! the place of the ones the stream wrote to its target table.
//!
//! What the server takes (tried against 3.4.3):
//!
//! - a stream that is running: one stopped, or still starting after a
//!   create or a start, is refused (`0x700A`, "Stream not running");
//! - not a `PERIOD` stream, which has no windows over its trigger table's
//!   time (`0x0100`, "Operation not supported");
//! - `TO` after `FROM` (`0x0115` otherwise); without `TO`, from `FROM` on;
//! - times as literals in the session's time zone (a date, a date and
//!   time, with a fraction or an offset); no expressions (`now - 1d`).
//!
//! The old engine (up to 3.3.6) has nothing like it: the action isn't
//! offered there.

use super::{bad, once, shown, warning, Ctx, Plan, Stmt, Vals};
use crate::ddl::lit;
use crate::jobs::{self, Stream};
use dbine_driver::{
    Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerAction,
};
use serde_json::json;

pub(crate) const RECALCULATE: &str = "recalculate";

const OLD_ENGINE: &str = "Recalcular un stream llegó con el motor de streams nuevo de TDengine (3.3.7): esta versión no lo tiene.";

/// The action, as the kind offers it (the label as a literal: the message
/// catalog finds it by `SchedulerAction::new`).
pub(crate) fn action() -> SchedulerAction {
    SchedulerAction::new("recalculate", "Recalcular un rango de tiempo").with_params()
}

/// A stream triggered by time (`PERIOD`), as its statement says.
fn periodic(st: &Stream) -> bool {
    st.sql
        .as_deref()
        .and_then(jobs::trigger_of)
        .is_some_and(|t| t.trim_start().to_ascii_lowercase().starts_with("period"))
}

/// The stream can be recalculated now, as far as DBine can tell before
/// the server answers.
pub(crate) fn check(c: &Ctx, st: &Stream) -> Result<()> {
    if !c.new_engine {
        return Err(Error::Unsupported(OLD_ENGINE.into()));
    }
    if periodic(st) {
        return Err(bad(format!(
            "El stream «{}» se dispara por tiempo (PERIOD): TDengine no recalcula esos streams.",
            st.id()
        )));
    }
    if !st.enabled() {
        return Err(bad(format!(
            "El stream «{}» está detenido: TDengine recalcula solo streams en marcha. Inicialo primero.",
            st.id()
        )));
    }
    Ok(())
}

/// Whether the item offers the action.
pub(crate) fn offered(c: &Ctx, st: &Stream) -> bool {
    check(c, st).is_ok()
}

// ---------------------------------------------------------------------------
// Times
// ---------------------------------------------------------------------------

/// A time from the form, checked: `YYYY-MM-DD`, then `HH:MM[:SS[.f]]`
/// after a space or a `T`, then an offset (`Z`, `±HH:MM`) or none (the
/// session's time zone).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct At {
    /// As the statement writes it (between quotes).
    pub(crate) text: String,
    /// Seconds since 1970 (the offset applied, when there is one) and
    /// nanoseconds: two times compare when both have an offset or neither
    /// has.
    key: (i64, u32),
    zoned: bool,
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// `n` ASCII digits at the start of `s`, as a number, and the rest.
fn digits(s: &str, n: usize) -> Option<(i64, &str)> {
    let head = s.get(..n)?;
    if !head.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((head.parse().ok()?, &s[n..]))
}

fn parse(t: &str) -> Option<At> {
    let (y, r) = digits(t, 4)?;
    let (m, r) = digits(r.strip_prefix('-')?, 2)?;
    let (d, r) = digits(r.strip_prefix('-')?, 2)?;
    if y < 1 || !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return None;
    }
    let (mut h, mut mi, mut s, mut frac) = (0, 0, 0, String::new());
    let mut r = r;
    if let Some(rest) = r.strip_prefix([' ', 'T', 't']) {
        let (hh, rest) = digits(rest, 2)?;
        let (mm, mut rest) = digits(rest.strip_prefix(':')?, 2)?;
        (h, mi) = (hh, mm);
        if let Some(x) = rest.strip_prefix(':') {
            let (ss, x) = digits(x, 2)?;
            s = ss;
            rest = x;
            if let Some(x) = rest.strip_prefix('.') {
                let n = x.bytes().take_while(u8::is_ascii_digit).count();
                if !(1..=9).contains(&n) {
                    return None;
                }
                frac = x[..n].to_string();
                rest = &x[n..];
            }
        }
        r = rest;
    } else if !r.is_empty() {
        return None;
    }
    if h > 23 || mi > 59 || s > 59 {
        return None;
    }
    let offset = match r.as_bytes().first() {
        None => None,
        Some(b'Z' | b'z') if r.len() == 1 => Some(0),
        Some(&sign @ (b'+' | b'-')) => {
            let (oh, x) = digits(&r[1..], 2)?;
            let x = x.strip_prefix(':').unwrap_or(x);
            let (om, x) = digits(x, 2)?;
            if !x.is_empty() || oh > 14 || om > 59 {
                return None;
            }
            let o = oh * 3600 + om * 60;
            Some(if sign == b'+' { o } else { -o })
        }
        Some(_) => return None,
    };
    let secs = days_from_civil(y, m, d) * 86_400 + h * 3600 + mi * 60 + s - offset.unwrap_or(0);
    let nanos: u32 = format!("{frac:0<9}").parse().unwrap_or(0);
    let fraction = if frac.is_empty() {
        String::new()
    } else {
        format!(".{frac}")
    };
    let text = match offset {
        None => format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}:{s:02}{fraction}"),
        Some(o) => format!(
            "{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}{fraction}{}{:02}:{:02}",
            if o < 0 { '-' } else { '+' },
            o.abs() / 3600,
            o.abs() % 3600 / 60
        ),
    };
    Some(At {
        text,
        key: (secs, nanos),
        zoned: offset.is_some(),
    })
}

/// A time the form sent for the field `label`.
pub(crate) fn parse_at(v: &str, label: &str) -> Result<At> {
    parse(v.trim()).ok_or_else(|| {
        bad(format!(
            "«{label}»: «{}» no es una fecha y hora (AAAA-MM-DD HH:MM:SS).",
            shown(v)
        ))
    })
}

/// The day and the time now as the form writes them (`YYYY-MM-DD`,
/// `YYYY-MM-DDTHH:MM:SS`), from the server's `NOW()` in the session's time
/// zone (`2026-11-13T17:38:43.883Z`, or with the session's offset).
fn local_now(now: &str) -> Option<(String, String)> {
    let at = now.trim().get(..19)?;
    parse(at)?;
    Some((at[..10].to_string(), at.replace(' ', "T")))
}

// ---------------------------------------------------------------------------
// The form and the plan
// ---------------------------------------------------------------------------

/// The action's form for the stream `st`; `now`, the server's `NOW()`
/// (the range defaults to today so far).
pub(crate) fn form_of(st: &Stream, now: Option<&str>) -> ObjectForm {
    let mut from = FormField::new("from", "Desde", FormKind::DateTime)
        .required()
        .help("La hora de los datos (la marca de tiempo de la tabla que dispara el stream), en la zona horaria de la conexión.");
    let mut to =
        FormField::new("to", "Hasta", FormKind::DateTime).help("Vacío: desde «Desde» en adelante.");
    if let Some((day, at)) = now.and_then(local_now) {
        from = from.default(json!(format!("{day}T00:00:00")));
        to = to.default(json!(at));
    }
    ObjectForm {
        sections: vec![FormSection::new("range", "Rango").field(from).field(to)],
        notes: vec![format!(
            "TDengine calcula de nuevo las ventanas del stream «{}» en ese rango con los datos que hay ahora, y sus resultados reemplazan los que el stream escribió en la tabla de destino.",
            st.id()
        )],
        ..ObjectForm::default()
    }
}

/// The statement for the form's `values`.
pub(crate) fn plan(c: &Ctx, st: &Stream, values: &FormValues) -> Result<Plan> {
    check(c, st)?;
    let v = Vals(&values.values);
    let from = parse_at(
        &v.opt("from").ok_or_else(|| bad("Falta «Desde».".into()))?,
        "Desde",
    )?;
    let to = v.opt("to").map(|t| parse_at(&t, "Hasta")).transpose()?;
    if let Some(to) = &to {
        if to.zoned == from.zoned && to.key <= from.key {
            return Err(bad("«Hasta» tiene que ser posterior a «Desde».".into()));
        }
    }
    let mut p = Plan::new(&st.name, st.id());
    p.comments.push(match &to {
        Some(t) => format!(
            "Recalcula el stream «{}» de TDengine entre {} y {}.",
            st.id(),
            from.text,
            t.text
        ),
        None => format!(
            "Recalcula el stream «{}» de TDengine desde {} en adelante.",
            st.id(),
            from.text
        ),
    });
    p.comments.push(warning(
        "Las ventanas de ese rango se calculan de nuevo con los datos que hay ahora, y sus resultados reemplazan los que el stream escribió en la tabla de destino.",
    ));
    let mut sql = format!(
        "RECALCULATE STREAM {} FROM {}",
        st.target(),
        lit(&from.text)
    );
    if let Some(t) = &to {
        sql.push_str(&format!(" TO {}", lit(&t.text)));
    }
    p.stmts.push(Stmt::plain(sql));
    Ok(p)
}

/// The server's refusals, said in terms of the stream.
fn server_error(e: Error, id: &str) -> Error {
    let code = match &e {
        Error::Statement(se) => se.code.as_deref(),
        _ => None,
    };
    match code {
        Some("0x700A") => bad(format!(
            "El stream «{id}» no está en marcha (detenido, o todavía arrancando después de crearlo o iniciarlo): TDengine recalcula solo streams en marcha. Probá de nuevo en unos segundos."
        )),
        Some("0x0100") => bad(format!(
            "TDengine no recalcula el stream «{id}» (los streams que se disparan por tiempo, con PERIOD, no se recalculan)."
        )),
        _ => e,
    }
}

/// Runs the plan's one statement.
pub(crate) async fn run(s: &crate::TdSession, st: &Stream, p: &Plan) -> Result<()> {
    for x in &p.stmts {
        once(s, x).await.map_err(|e| server_error(e, &st.id()))?;
    }
    Ok(())
}
