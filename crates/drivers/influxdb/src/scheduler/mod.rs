//! Each API's scheduler managed whole through the scheduler contract
//! ([`dbine_driver::Session::scheduler_info`] and the rest), on top of the
//! server jobs of [`crate::jobs`]:
//!
//! - 2.x / Cloud: tasks ([`task`]): name, `every` or `cron`, `offset`,
//!   `concurrency` and `retry` (the other `option task` fields kept and
//!   shown read-only), the Flux, status, description and labels, through
//!   `/api/v2/tasks` (`POST`, `PATCH` of what changed, `DELETE`); the run
//!   history is cleared with the delete API on the `_tasks` bucket.
//! - 1.x: continuous queries ([`cq`]): `CREATE CONTINUOUS QUERY … RESAMPLE
//!   EVERY … FOR … BEGIN … END` and `DROP CONTINUOUS QUERY`. There is no
//!   `ALTER`: a change drops the query and creates it again, and puts the
//!   old one back when the new one is refused.
//! - 3.x: the processing engine's triggers ([`trigger`]): plugin, trigger
//!   specification (`every:`, `cron:`, `table:`, `all_tables`,
//!   `request:`), arguments, error behavior, asynchronous runs, enabled,
//!   and the plugin's code, through `/api/v3/configure/
//!   processing_engine_trigger` and `/api/v3/plugins/files`. Only enabling
//!   and the plugin's code change in place; anything else deletes the
//!   trigger and creates it again. The processing engine's log is the
//!   scheduler's log.
//!
//! Each API has one kind, `job`, whose ids are those of
//! [`dbine_driver::Session::jobs`]. Ids and values come from the app as
//! untrusted text: ids are parsed and looked up on the server, names and
//! durations are checked, and every value travels as a JSON value, a URL
//! parameter or a quoted InfluxQL identifier / string, never pasted.
//! Scripts are the requests the save sends, as comments in the editor's
//! language (2.x and 3.x), or the InfluxQL statements (1.x).

pub(crate) mod cq;
pub(crate) mod task;
pub(crate) mod trigger;

#[cfg(test)]
mod tests;

use dbine_driver::{Error, FormValues, Result};
use serde_json::Value;
use std::collections::BTreeMap;

/// The one kind of each API (tasks, continuous queries, triggers).
pub(crate) const JOB: &str = "job";

/// Where a request's path takes the id of the object the plan creates.
pub(crate) const NEW_ID: &str = "{id}";

pub(crate) fn bad(message: String) -> Error {
    Error::Query(message)
}

/// Text from the app or the server, on one line and short, for a message
/// or a script's comment.
pub(crate) fn shown(s: &str) -> String {
    one_line(s.trim()).chars().take(80).collect()
}

/// Control characters (line breaks included) as spaces: what a comment of a
/// script may hold.
pub(crate) fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Refuses any kind but [`JOB`].
pub(crate) fn check_kind(kind: &str) -> Result<()> {
    if kind.trim() == JOB {
        Ok(())
    } else {
        Err(bad(format!(
            "«{}» no es un tipo de objeto del planificador de InfluxDB",
            shown(kind)
        )))
    }
}

// ---------------------------------------------------------------------------
// Values from the designer
// ---------------------------------------------------------------------------

/// A form's values (or a list row's) as the app sent them: untrusted, any
/// JSON type where a field expects another.
#[derive(Clone, Copy)]
pub(crate) struct Vals<'a>(pub(crate) &'a BTreeMap<String, Value>);

impl<'a> Vals<'a> {
    fn get(&self, k: &str) -> Option<&'a Value> {
        self.0.get(k).filter(|v| !v.is_null())
    }

    /// The value as typed (code keeps its spaces).
    pub(crate) fn raw(&self, k: &str) -> String {
        match self.get(k) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(_) | Value::Object(_)) | None => String::new(),
            Some(v) => v.to_string(),
        }
    }

    /// The value trimmed.
    pub(crate) fn text(&self, k: &str) -> String {
        self.raw(k).trim().to_string()
    }

    /// The value trimmed, `None` when empty.
    pub(crate) fn opt(&self, k: &str) -> Option<String> {
        Some(self.text(k)).filter(|t| !t.is_empty())
    }

    /// A switch; `None` when the app sent nothing for it.
    pub(crate) fn flag(&self, k: &str) -> Option<bool> {
        match self.get(k)? {
            Value::Bool(b) => Some(*b),
            Value::Number(n) => Some(n.as_f64().is_some_and(|f| f != 0.0)),
            Value::String(s) => Some(matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "true" | "1" | "yes" | "on"
            )),
            _ => None,
        }
    }

    /// A list of values (a multi-select's), trimmed, without empties or
    /// repeats.
    pub(crate) fn strings(&self, k: &str) -> Vec<String> {
        let all: Vec<String> = match self.get(k) {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.trim().to_string()),
                    Value::Null => None,
                    v => Some(v.to_string()),
                })
                .filter(|s| !s.is_empty())
                .collect(),
            Some(Value::String(s)) if !s.trim().is_empty() => vec![s.trim().to_string()],
            _ => Vec::new(),
        };
        let mut out: Vec<String> = Vec::new();
        for s in all {
            if !out.contains(&s) {
                out.push(s);
            }
        }
        out
    }
}

/// A list's rows from the designer.
pub(crate) fn rows<'a>(values: &'a FormValues, list: &str) -> Vec<Vals<'a>> {
    values
        .lists
        .get(list)
        .map(|rows| rows.iter().map(Vals).collect())
        .unwrap_or_default()
}

/// A name for the server: on one line.
pub(crate) fn check_line(value: &str, label: &str) -> Result<()> {
    if value.chars().any(char::is_control) {
        return Err(bad(format!(
            "«{label}» no puede tener saltos de línea ni caracteres de control."
        )));
    }
    Ok(())
}

/// A duration made of `<integer><unit>` parts (`1h30m`), each unit one of
/// `units`; with `spaced`, parts may be apart (`1h 30m`).
pub(crate) fn duration_ok(s: &str, units: &[&str], spaced: bool) -> bool {
    let mut chars = s.trim().chars().peekable();
    let mut parts = 0;
    loop {
        if spaced {
            while chars.peek().is_some_and(|c| *c == ' ') {
                chars.next();
            }
        }
        if chars.peek().is_none() {
            return parts > 0;
        }
        let mut digits = 0;
        while chars.peek().is_some_and(char::is_ascii_digit) {
            chars.next();
            digits += 1;
        }
        if digits == 0 {
            return false;
        }
        if spaced {
            while chars.peek().is_some_and(|c| *c == ' ') {
                chars.next();
            }
        }
        let mut unit = String::new();
        while chars
            .peek()
            .is_some_and(|c| c.is_ascii_alphabetic() || *c == 'µ')
        {
            unit.push(chars.next().unwrap_or_default());
        }
        if !units.contains(&unit.as_str()) {
            return false;
        }
        parts += 1;
    }
}

/// A cron expression of `min..=max` fields (or an `@…` macro where
/// `macros`), checked for its shape only: the server reads it.
pub(crate) fn cron_ok(s: &str, min: usize, max: usize, macros: bool) -> bool {
    let s = s.trim();
    if macros && s.starts_with('@') {
        return s.len() > 1 && s[1..].chars().all(|c| c.is_ascii_alphanumeric());
    }
    let fields: Vec<&str> = s.split_whitespace().collect();
    (min..=max).contains(&fields.len())
        && fields.iter().all(|f| {
            f.chars()
                .all(|c| c.is_ascii_alphanumeric() || "*/,-?#".contains(c))
        })
}

// ---------------------------------------------------------------------------
// HTTP requests as scripts (2.x and 3.x)
// ---------------------------------------------------------------------------

/// One request a save sends.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Req {
    pub(crate) method: &'static str,
    /// From the API's root (`/api/v2/tasks`); [`NEW_ID`] stands for the id
    /// of what an earlier request created.
    pub(crate) path: String,
    pub(crate) query: Vec<(String, String)>,
    pub(crate) body: Option<Value>,
}

impl Req {
    pub(crate) fn new(method: &'static str, path: impl Into<String>, body: Option<Value>) -> Req {
        Req {
            method,
            path: path.into(),
            query: Vec::new(),
            body,
        }
    }

    pub(crate) fn query(mut self, k: &str, v: &str) -> Req {
        self.query.push((k.into(), v.into()));
        self
    }

    /// The URL from `base`, with the created object's id in place of
    /// [`NEW_ID`].
    pub(crate) fn url(&self, base: &str, new_id: Option<&str>) -> Result<reqwest::Url> {
        let path = match new_id {
            Some(id) => self.path.replace(NEW_ID, id),
            None => self.path.clone(),
        };
        let mut url = reqwest::Url::parse(&format!("{base}{path}"))
            .map_err(|e| Error::Query(e.to_string()))?;
        if !self.query.is_empty() {
            url.query_pairs_mut().extend_pairs(&self.query);
        }
        Ok(url)
    }

    pub(crate) fn method(&self) -> reqwest::Method {
        match self.method {
            "POST" => reqwest::Method::POST,
            "PATCH" => reqwest::Method::PATCH,
            "PUT" => reqwest::Method::PUT,
            "DELETE" => reqwest::Method::DELETE,
            _ => reqwest::Method::GET,
        }
    }

    /// `METHOD /path?query` and the JSON body, as the API takes them (the
    /// path as written, [`NEW_ID`] included).
    fn text(&self) -> String {
        let query = self
            .url("http://h", None)
            .ok()
            .and_then(|u| u.query().map(str::to_string));
        let mut out = match query {
            Some(q) => format!("{} {}?{q}", self.method, self.path),
            None => format!("{} {}", self.method, self.path),
        };
        if let Some(b) = &self.body {
            out.push('\n');
            out.push_str(&serde_json::to_string_pretty(b).unwrap_or_default());
        }
        out
    }
}

/// The requests as comments (`comment`: `//` for Flux, `--` for SQL),
/// after the header's lines.
pub(crate) fn render(comment: &str, header: &[String], reqs: &[Req]) -> String {
    let mut out = String::new();
    for h in header {
        out.push_str(&format!("{comment} {}\n", one_line(h)));
    }
    for r in reqs {
        out.push_str(&format!("{comment}\n"));
        for line in r.text().lines() {
            // JSON keeps line breaks inside strings escaped: one request
            // line never ends the comment.
            out.push_str(&format!("{comment} {line}\n"));
        }
    }
    out
}
