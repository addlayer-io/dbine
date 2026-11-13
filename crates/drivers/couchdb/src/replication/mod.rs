//! CouchDB's replication scheduler managed whole through the scheduler
//! contract ([`dbine_driver::Session::scheduler_info`] and the rest): what
//! Fauxton's Replication page does, plus the scheduler's own settings.
//!
//! - Kinds: replications (`job`, the ids `jobs()` returns: `_replicator`
//!   documents and transient `_replicate` ones) and the replicator's
//!   settings of each node (`[replicator]` in `/_node/{node}/_config`).
//! - Scripts: the requests as DBine's HTTP console for CouchDB takes them
//!   (`METHOD path` and a JSON body), so "Abrir en una consulta" runs them.
//!   Credentials never come back: a form shows a URL without its password
//!   and the user apart, its password empty (kept unless typed), secret
//!   headers as `********` (kept while they stay so); the script shows
//!   every secret masked, and only the save sends the real ones.
//! - A replication is created with `PUT /{_replicator}/{id}` (or `POST`
//!   without an id), its `_replicator` database first when it's missing;
//!   an edit is the same document saved with its `_rev` (CouchDB restarts
//!   the replication from its checkpoint), every field DBine doesn't show
//!   kept as it was; a delete is `DELETE …?rev=`, which stops it. A
//!   transient replication has no document: it shows read-only and its
//!   delete cancels it (`POST /_replicate` with `cancel`).
//! - "Run from a step" restarts the replication (its one step); CouchDB
//!   keeps its history in memory with no call to clear it, and no log the
//!   HTTP API reads.
//!
//! Every value is checked before the request is written: URLs (http or
//! https, no credentials in them), document and database names by
//! CouchDB's rules, numbers in range, JSON where JSON goes; ids are parsed
//! and the objects they name read from the server first.

mod doc;
mod settings;
#[cfg(test)]
mod tests;

use crate::jobs::{self, denied, no_scheduler};
use crate::{comment_text, CouchSession};
use dbine_driver::{
    Error, FormValues, ObjectForm, Result, SchedulerInfo, SchedulerKind, SchedulerList, SECRET_MASK,
};
use reqwest::Method;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

const TRANSIENT_KIND_NOTE: &str = "Las replicaciones transitorias (POST /_replicate) se ven y se cancelan, pero no se cambian: no tienen documento.";

fn bad(message: String) -> Error {
    Error::Query(message)
}

/// Text from the app, short and on one line, for a message.
fn shown(s: &str) -> String {
    comment_text(s.trim()).chars().take(60).collect()
}

// ---------------------------------------------------------------------------
// Kinds and what the server allows
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Job,
    Settings,
}

impl Kind {
    fn parse(kind: &str) -> Result<Kind> {
        match kind.trim() {
            "job" => Ok(Kind::Job),
            "settings" => Ok(Kind::Settings),
            other => Err(bad(format!(
                "«{}» no es un tipo de objeto del planificador de replicaciones de CouchDB",
                shown(other)
            ))),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Ctx {
    /// The server's version, `(major, minor)`.
    pub(crate) version: (u32, u32),
    /// Why the scheduler can't be managed (not an admin, before 2.1).
    pub(crate) missing: Option<&'static str>,
}

impl Ctx {
    /// CouchDB 3.2 on: credentials as `auth.basic`, not in the URL.
    pub(crate) fn basic_auth(&self) -> bool {
        self.version >= (3, 2)
    }

    /// CouchDB 3.3 on: `winning_revs_only`, `use_bulk_get`.
    pub(crate) fn v33(&self) -> bool {
        self.version >= (3, 3)
    }
}

/// `3.5.2` → `(3, 5)`.
pub(crate) fn version_of(v: &str) -> (u32, u32) {
    let mut parts = v.split(|c: char| !c.is_ascii_digit());
    let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (major, minor)
}

async fn ctx(s: &CouchSession) -> Result<Ctx> {
    let mut c = Ctx {
        version: version_of(&s.version),
        missing: None,
    };
    match s.call(Method::GET, "/_scheduler/jobs?limit=1", None).await {
        Ok(_) => {}
        Err(e) if denied(&e) => c.missing = Some(jobs::DENIED),
        Err(e) if no_scheduler(&e) => c.missing = Some(jobs::NO_SCHEDULER),
        Err(e) => return Err(e),
    }
    Ok(c)
}

/// The context, refusing a server whose scheduler DBine can't manage.
async fn ready(s: &CouchSession) -> Result<Ctx> {
    let c = ctx(s).await?;
    match c.missing {
        Some(note) if note == jobs::NO_SCHEDULER => Err(Error::Unsupported(note.into())),
        Some(note) => Err(Error::Query(note.into())),
        None => Ok(c),
    }
}

pub(crate) fn info_of(c: &Ctx) -> SchedulerInfo {
    if let Some(note) = c.missing {
        return SchedulerInfo {
            kinds: Vec::new(),
            logs: false,
            notes: vec![note.to_string()],
        };
    }
    SchedulerInfo {
        kinds: vec![
            SchedulerKind::new("job", "Replicaciones", "replicación")
                .abilities(true, true, true)
                // One step, the replication itself: starting it again is
                // the job's "run now" (a new revision of its document);
                // `run_job_from` refuses.
                .run_from_step(false)
                // Its history lives in the scheduler's memory, with no call
                // to clear it.
                .clear_history(false),
            SchedulerKind::new("settings", "Configuración", "configuración")
                .abilities(false, true, false),
        ],
        logs: false,
        notes: vec![TRANSIENT_KIND_NOTE.to_string()],
    }
}

// ---------------------------------------------------------------------------
// The contract's calls
// ---------------------------------------------------------------------------

pub(crate) async fn info(s: &CouchSession) -> Result<SchedulerInfo> {
    Ok(info_of(&ctx(s).await?))
}

pub(crate) async fn items(s: &mut CouchSession, kind: &str) -> Result<SchedulerList> {
    let kind = Kind::parse(kind)?;
    ready(s).await?;
    match kind {
        Kind::Job => doc::items(s).await,
        Kind::Settings => settings::items(s).await,
    }
}

pub(crate) async fn form(s: &CouchSession, kind: &str, id: Option<&str>) -> Result<ObjectForm> {
    let kind = Kind::parse(kind)?;
    let c = ready(s).await?;
    match kind {
        Kind::Job => doc::form(s, &c, id).await,
        Kind::Settings => settings::form(s, id).await,
    }
}

async fn change(
    s: &CouchSession,
    c: &Ctx,
    kind: Kind,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Change> {
    match kind {
        Kind::Job => doc::change(s, c, id, values).await,
        Kind::Settings => settings::change(s, id, values).await,
    }
}

pub(crate) async fn script(
    s: &CouchSession,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<String> {
    let kind = Kind::parse(kind)?;
    let c = ready(s).await?;
    Ok(change(s, &c, kind, id, values).await?.script())
}

pub(crate) async fn save(
    s: &CouchSession,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Option<String>> {
    let kind = Kind::parse(kind)?;
    let c = ready(s).await?;
    let ch = change(s, &c, kind, id, values).await?;
    run(s, &ch).await
}

async fn delete_change(s: &CouchSession, kind: Kind, id: &str) -> Result<Change> {
    match kind {
        Kind::Job => doc::delete(s, id).await,
        Kind::Settings => Err(bad(
            "La configuración del replicador no se borra: se cambia.".into(),
        )),
    }
}

pub(crate) async fn delete_script(s: &CouchSession, kind: &str, id: &str) -> Result<String> {
    let kind = Kind::parse(kind)?;
    ready(s).await?;
    Ok(delete_change(s, kind, id).await?.script())
}

pub(crate) async fn delete(s: &CouchSession, kind: &str, id: &str) -> Result<()> {
    let kind = Kind::parse(kind)?;
    ready(s).await?;
    let ch = delete_change(s, kind, id).await?;
    run(s, &ch).await.map(|_| ())
}

pub(crate) async fn run_from(s: &CouchSession, id: &str) -> Result<()> {
    ready(s).await?;
    doc::run_from(id)
}

pub(crate) async fn clear_history(s: &CouchSession, id: &str) -> Result<()> {
    ready(s).await?;
    jobs::parse_id(id)?;
    Err(Error::Unsupported(
        "CouchDB guarda el historial de una replicación en la memoria del planificador (los últimos eventos, según [replicator] max_history) y no tiene una llamada para borrarlo.".into(),
    ))
}

// ---------------------------------------------------------------------------
// Changes: the requests a save (or a delete) sends, and their script
// ---------------------------------------------------------------------------

/// One request of a change.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Call {
    pub(crate) method: Method,
    /// The path, its segments encoded.
    pub(crate) path: String,
    /// What is sent.
    pub(crate) body: Option<Value>,
    /// The whole body is a secret (a setting's password).
    pub(crate) secret: bool,
}

impl Call {
    pub(crate) fn new(method: Method, path: String, body: Option<Value>) -> Call {
        Call {
            method,
            path,
            body,
            secret: false,
        }
    }

    /// The body as the script shows it.
    fn shown_body(&self) -> Option<Value> {
        let b = self.body.as_ref()?;
        Some(if self.secret {
            Value::String(SECRET_MASK.into())
        } else {
            masked(b)
        })
    }
}

/// What a save (or a delete) sends.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Change {
    /// Comment lines on top of the script (without the `// `).
    pub(crate) comments: Vec<String>,
    pub(crate) calls: Vec<Call>,
    /// The object's id after the change.
    pub(crate) id: Option<String>,
    /// A document `POST`ed without an id: its database; the id comes in
    /// the last reply.
    pub(crate) id_from_reply: Option<String>,
}

impl Change {
    pub(crate) fn nothing(name: &str, id: String) -> Change {
        Change {
            comments: vec![format!("«{name}»: no hay cambios que guardar.")],
            id: Some(id),
            ..Change::default()
        }
    }

    pub(crate) fn script(&self) -> String {
        let mut out = String::new();
        for c in &self.comments {
            out.push_str(&format!("// {}\n", comment_text(c)));
        }
        for call in &self.calls {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("{} {}\n", call.method, call.path));
            if let Some(b) = call.shown_body() {
                out.push_str(&serde_json::to_string_pretty(&b).unwrap_or_default());
                out.push('\n');
            }
        }
        out
    }
}

/// Sends a change's requests in order; the id of what it saved.
async fn run(s: &CouchSession, ch: &Change) -> Result<Option<String>> {
    let mut last = Value::Null;
    for (i, call) in ch.calls.iter().enumerate() {
        match s
            .call(call.method.clone(), &call.path, call.body.as_ref())
            .await
        {
            Ok(v) => last = v,
            Err(e) if i == 0 => return Err(e),
            Err(e) => {
                return Err(Error::Query(format!(
                    "Se aplicaron los primeros {i} pedidos del script y falló el siguiente: {e}"
                )))
            }
        }
    }
    if let Some(db) = &ch.id_from_reply {
        if let Some(doc) = last.get("id").and_then(Value::as_str) {
            return Ok(Some(
                jobs::JobId::Doc {
                    db: db.clone(),
                    doc: doc.to_string(),
                }
                .text(),
            ));
        }
    }
    Ok(ch.id.clone())
}

/// A URL with its password masked (`http://u:********@h/db`).
pub(crate) fn mask_url(u: &str) -> String {
    let Some((scheme, rest)) = u.split_once("://") else {
        return u.to_string();
    };
    let host_end = rest.find('/').unwrap_or(rest.len());
    let Some(at) = rest[..host_end].rfind('@') else {
        return u.to_string();
    };
    match rest[..at].split_once(':') {
        Some((user, _)) => format!("{scheme}://{user}:{SECRET_MASK}{}", &rest[at..]),
        None => u.to_string(),
    }
}

/// A document with every secret masked: values under secret keys
/// (passwords, `Authorization`, cookies, tokens…) and passwords in URLs.
pub(crate) fn masked(v: &Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, v)| {
                    if jobs::secret_key(k) && !v.is_null() {
                        (k.clone(), Value::String(SECRET_MASK.into()))
                    } else {
                        (k.clone(), masked(v))
                    }
                })
                .collect::<Map<String, Value>>(),
        ),
        Value::Array(a) => Value::Array(a.iter().map(masked).collect()),
        Value::String(t) => Value::String(mask_url(t)),
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Values from the designer
// ---------------------------------------------------------------------------

/// A form's values as the app sent them: untrusted, any JSON type where a
/// field expects another.
#[derive(Clone, Copy)]
pub(crate) struct Vals<'a>(pub(crate) &'a BTreeMap<String, Value>);

impl<'a> Vals<'a> {
    fn get(&self, k: &str) -> Option<&'a Value> {
        self.0.get(k).filter(|v| !v.is_null())
    }

    /// The value as typed.
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

    /// A secret: `None` when the user typed none (keep what's there).
    pub(crate) fn secret(&self, k: &str) -> Option<String> {
        Some(self.raw(k)).filter(|t| !t.is_empty())
    }

    pub(crate) fn bool(&self, k: &str) -> bool {
        match self.get(k) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            Some(Value::String(s)) => matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "true" | "1" | "yes" | "on"
            ),
            _ => false,
        }
    }

    /// A whole number in `min..=max`; `None` when missing or empty.
    pub(crate) fn int(&self, k: &str, label: &str, min: i64, max: i64) -> Result<Option<i64>> {
        let n = match self.get(k) {
            None => return Ok(None),
            Some(Value::String(s)) if s.trim().is_empty() => return Ok(None),
            Some(Value::Number(n)) => n
                .as_i64()
                .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
            Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
            Some(_) => None,
        };
        match n {
            Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
            Some(n) => Err(bad(format!(
                "«{label}» tiene que estar entre {min} y {max} (es {n})."
            ))),
            None => Err(bad(format!("«{label}» tiene que ser un número entero."))),
        }
    }
}
