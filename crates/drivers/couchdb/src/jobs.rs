//! Server jobs ([`dbine_driver::Session::jobs`]): CouchDB's replications,
//! from the replication scheduler (CouchDB 2.1 on, server admins only):
//!
//! - **`_replicator` documents** (`/_scheduler/docs`, every `_replicator`
//!   database, also `prefix/_replicator`): kind `_replicator`, id
//!   `doc:<database>:<doc id>`. CouchDB has no pause: stopping one deletes
//!   its document (the tab says so, and its script brings it back); "run
//!   now" saves the document again without the state the replicator wrote
//!   on it (`_replication_state`…), which restarts a finished, failed or
//!   retrying replication. The script is the document as a `PUT` line of
//!   DBine's HTTP console, with its credentials hidden.
//! - **Transient replications** (`POST /_replicate`, in `/_scheduler/jobs`
//!   with no database): kind `_replicate`, id `rep:<replication id>`. They
//!   are stopped with `_replicate` and `cancel: true`; CouchDB keeps no
//!   definition of them, so their script is rebuilt from what the scheduler
//!   says (source, target, continuous) and says so.
//!
//! The history is the scheduler's events of the job (`started`, `crashed`,
//! `stopped`…, the last `[replicator] max_history`, 20 by default), as
//! runs; a finished one-shot replication, which leaves the scheduler, is
//! its document's final state.
//!
//! Ids that come back are parsed against those shapes (a `_replicator`
//! database name by CouchDB's rule, a document id without control
//! characters or a leading `_`, a replication id of hex digits and `+`
//! options), percent-encoded into paths, and have to name a job the
//! scheduler lists.

use crate::processes::valid_replication_id;
use crate::{seg, CouchSession};
use dbine_driver::jobs::MAX_JOB_RUNS;
use dbine_driver::{
    Error, JobDetail, JobList, JobOutcome, JobRun, JobStep, PropertyInfo, Result, ServerJob,
};
use reqwest::Method;
use serde_json::{json, Map, Value};

const DOC_KIND: &str = "_replicator";
const TRANSIENT_KIND: &str = "_replicate";
/// Rows per scheduler request.
const PAGE: usize = 500;
/// Jobs listed at most.
const MAX_JOBS: usize = 5000;
/// Longest document id taken back.
const MAX_DOC_ID: usize = 1024;
/// Characters kept of a message.
const MAX_MESSAGE: usize = 4000;
/// What replaces a hidden credential.
const HIDDEN: &str = "***";

pub(crate) const DENIED: &str =
    "Solo un administrador del servidor ve las replicaciones (/_scheduler): este usuario no lo es.";
pub(crate) const NO_SCHEDULER: &str = "Esta versión de CouchDB no tiene el planificador de replicaciones (/_scheduler, CouchDB 2.1 en adelante).";
const DOC_STOP_NOTE: &str = "CouchDB no tiene pausa: «Detener» borra el documento de _replicator y la replicación deja de existir. El script de esta pestaña la vuelve a crear.";
const DOC_RUN_NOTE: &str = "«Ejecutar ahora» vuelve a guardar el documento sin el estado que le escribió CouchDB, y la replicación arranca de nuevo.";
pub(crate) const TRANSIENT_NOTE: &str = "Es una replicación transitoria (POST /_replicate): no tiene documento, se pierde si el servidor se reinicia y su script se arma con lo que informa el planificador, sin credenciales ni filtros.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JobId {
    Doc { db: String, doc: String },
    Transient(String),
}

impl JobId {
    pub fn text(&self) -> String {
        match self {
            JobId::Doc { db, doc } => format!("doc:{db}:{doc}"),
            JobId::Transient(r) => format!("rep:{r}"),
        }
    }
}

/// A `_replicator` database: `_replicator`, or `<name>/_replicator` with a
/// name by CouchDB's rule (`[a-z][a-z0-9_$()+/-]*`).
pub(crate) fn replicator_db(db: &str) -> bool {
    if db == "_replicator" {
        return true;
    }
    let Some(prefix) = db.strip_suffix("/_replicator") else {
        return false;
    };
    prefix.len() <= 238
        && prefix.starts_with(|c: char| c.is_ascii_lowercase())
        && prefix
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_$()+-/".contains(c))
}

/// A replication document's id: not empty, no control characters, not a
/// `_design/` or `_local/` one (they aren't replications), and not `.` or
/// `..`: the URL parser takes those (also percent-encoded) as dot segments
/// and the path would name another resource.
pub(crate) fn replication_doc(doc: &str) -> bool {
    !doc.is_empty()
        && doc.len() <= MAX_DOC_ID
        && doc != "."
        && doc != ".."
        && !doc.starts_with('_')
        && !doc.chars().any(char::is_control)
}

/// A scheduler replication id: 32 hex digits and `+option`s.
fn transient_id(r: &str) -> bool {
    let (hex, opts) = r.split_at(r.find('+').unwrap_or(r.len()));
    hex.len() == 32
        && hex.bytes().all(|b| b.is_ascii_hexdigit())
        && valid_replication_id(r)
        && opts
            .split('+')
            .skip(1)
            .all(|o| !o.is_empty() && o.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
}

pub(crate) fn parse_id(id: &str) -> Result<JobId> {
    let bad = || {
        Error::Query(format!(
            "«{}» no es un id de replicación de este servidor",
            id.chars().take(200).collect::<String>()
        ))
    };
    if let Some(rest) = id.strip_prefix("doc:") {
        // A database name has no `:`, so the first one ends it.
        let (db, doc) = rest.split_once(':').ok_or_else(bad)?;
        if !replicator_db(db) || !replication_doc(doc) {
            return Err(bad());
        }
        return Ok(JobId::Doc {
            db: db.into(),
            doc: doc.into(),
        });
    }
    if let Some(r) = id.strip_prefix("rep:") {
        if transient_id(r) {
            return Ok(JobId::Transient(r.into()));
        }
    }
    Err(bad())
}

pub(crate) fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
}

fn cut(t: &str) -> String {
    t.chars().take(MAX_MESSAGE).collect()
}

fn epoch(t: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(t)
        .ok()
        .map(|d| d.timestamp_millis())
}

/// An instant CouchDB wrote as RFC 3339 in UTC, to the second.
pub(crate) fn utc(t: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(t).ok().map(|d| {
        d.with_timezone(&chrono::Utc)
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string()
    })
}

fn between(a: &str, b: &str) -> Option<u64> {
    let (a, b) = (epoch(a)?, epoch(b)?);
    u64::try_from(b - a).ok()
}

/// `http://h:5984/db/` → `db`, for a short name.
fn db_name(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .map(|x| {
            percent_encoding::percent_decode_str(x)
                .decode_utf8_lossy()
                .to_string()
        })
        .unwrap_or_default()
}

/// The state the scheduler reports for a document, as an outcome.
fn doc_outcome(state: &str) -> Option<JobOutcome> {
    match state {
        "running" => Some(JobOutcome::Running),
        "completed" => Some(JobOutcome::Succeeded),
        "failed" => Some(JobOutcome::Failed),
        "crashing" | "error" => Some(JobOutcome::Retrying),
        _ => None,
    }
}

/// A `_scheduler/docs` row (and its job's row of `_scheduler/jobs`, if it
/// has one) as a job.
pub(crate) fn doc_job(d: &Value) -> Option<ServerJob> {
    let db = s(d, "database")?;
    let doc = s(d, "doc_id")?;
    if !replicator_db(&db) || !replication_doc(&doc) {
        return None;
    }
    let state = s(d, "state").unwrap_or_default();
    let (source, target) = (s(d, "source"), s(d, "target"));
    let started = s(d, "start_time");
    let finished = s(d, "last_updated");
    let done = state == "completed";
    Some(ServerJob {
        id: JobId::Doc {
            db: db.clone(),
            doc: doc.clone(),
        }
        .text(),
        name: doc,
        kind: DOC_KIND.into(),
        group: Some(db),
        description: match (&source, &target) {
            (Some(a), Some(b)) => Some(format!("{a} → {b}")),
            _ => None,
        },
        owner: None,
        enabled: true,
        schedule: None,
        running: state == "running",
        last_outcome: doc_outcome(&state),
        last_started: started.as_deref().and_then(utc),
        last_duration_ms: if done {
            started
                .as_deref()
                .zip(finished.as_deref())
                .and_then(|(a, b)| between(a, b))
        } else {
            None
        },
        next_run: None,
        can_run: matches!(
            state.as_str(),
            "completed" | "failed" | "crashing" | "error"
        ),
        can_stop: matches!(
            state.as_str(),
            "running" | "pending" | "crashing" | "error" | "initializing"
        ),
        can_enable: false,
        has_history: true,
        has_script: true,
        system: false,
    })
}

/// The newest event of a scheduler job, by time.
fn last_event(j: &Value) -> Option<&Value> {
    j.get("history")?
        .as_array()?
        .iter()
        .max_by_key(|e| s(e, "timestamp").as_deref().and_then(epoch).unwrap_or(0))
}

/// A transient replication (a `_scheduler/jobs` row without a database).
pub(crate) fn transient_job(j: &Value) -> Option<ServerJob> {
    if j.get("database").is_some_and(|d| !d.is_null()) {
        return None;
    }
    let rep = s(j, "id").filter(|r| transient_id(r))?;
    let (source, target) = (s(j, "source"), s(j, "target"));
    let running = j.get("pid").is_some_and(|p| !p.is_null());
    let last = last_event(j).and_then(|e| s(e, "type"));
    Some(ServerJob {
        id: JobId::Transient(rep.clone()).text(),
        name: match (&source, &target) {
            (Some(a), Some(b)) => format!("{} → {}", db_name(a), db_name(b)),
            _ => rep.clone(),
        },
        kind: TRANSIENT_KIND.into(),
        group: None,
        description: match (&source, &target) {
            (Some(a), Some(b)) => Some(format!("{a} → {b}")),
            _ => None,
        },
        owner: s(j, "user"),
        enabled: true,
        schedule: None,
        running,
        last_outcome: if running {
            Some(JobOutcome::Running)
        } else {
            match last.as_deref() {
                Some("crashed") => Some(JobOutcome::Retrying),
                Some("stopped") => Some(JobOutcome::Canceled),
                _ => None,
            }
        },
        last_started: s(j, "start_time").as_deref().and_then(utc),
        last_duration_ms: None,
        next_run: None,
        can_run: false,
        can_stop: true,
        can_enable: false,
        has_history: true,
        has_script: true,
        system: false,
    })
}

/// The job list from the scheduler's `docs` and `jobs` rows, with a note
/// when either listing (or both together) reached [`MAX_JOBS`].
pub(crate) fn list_of(docs: &[Value], jobs: &[Value]) -> JobList {
    let mut out: Vec<ServerJob> = docs.iter().filter_map(doc_job).collect();
    out.extend(jobs.iter().filter_map(transient_job));
    let cut = docs.len() >= MAX_JOBS || jobs.len() >= MAX_JOBS || out.len() > MAX_JOBS;
    out.truncate(MAX_JOBS);
    JobList {
        jobs: out,
        notes: if cut {
            vec![truncated_note()]
        } else {
            Vec::new()
        },
    }
}

/// A scheduler job's events as runs, newest first: each `started` opens a
/// run that the next `crashed` (failed), `stopped` (stopped by the
/// scheduler to make room, or cancelled) or `completed` closes.
pub(crate) fn runs_of(history: &[Value], running: bool) -> Vec<JobRun> {
    let mut events: Vec<&Value> = history.iter().collect();
    events.sort_by_key(|e| s(e, "timestamp").as_deref().and_then(epoch).unwrap_or(0));
    let mut out: Vec<JobRun> = Vec::new();
    let mut open: Option<JobRun> = None;
    let close = |r: Option<JobRun>, at: Option<&str>, outcome, message| {
        let mut r = r.unwrap_or_else(|| JobRun {
            started: at.and_then(utc),
            ..Default::default()
        });
        if let (Some(a), Some(b)) = (r.started.as_deref(), at) {
            r.duration_ms = between(a, b);
        }
        r.outcome = outcome;
        r.message = message;
        r
    };
    for e in events {
        let at = s(e, "timestamp");
        match s(e, "type").as_deref() {
            Some("started") => {
                if let Some(mut prev) = open.take() {
                    prev.outcome = JobOutcome::Unknown;
                    out.push(prev);
                }
                open = Some(JobRun {
                    started: at.as_deref().and_then(utc),
                    outcome: JobOutcome::Running,
                    ..Default::default()
                });
            }
            Some("crashed") => out.push(close(
                open.take(),
                at.as_deref(),
                JobOutcome::Failed,
                s(e, "reason").map(|m| cut(&m)),
            )),
            Some("stopped") => out.push(close(
                open.take(),
                at.as_deref(),
                JobOutcome::Canceled,
                s(e, "reason").map(|m| cut(&m)),
            )),
            Some("completed") => out.push(close(
                open.take(),
                at.as_deref(),
                JobOutcome::Succeeded,
                None,
            )),
            _ => {}
        }
    }
    if let Some(mut r) = open {
        r.outcome = if running {
            JobOutcome::Running
        } else {
            JobOutcome::Unknown
        };
        out.push(r);
    }
    out.reverse();
    out
}

/// The one run a document that left the scheduler (finished or failed for
/// good) still tells of.
fn final_run(d: &Value) -> Option<JobRun> {
    let state = s(d, "state")?;
    let outcome = match state.as_str() {
        "completed" => JobOutcome::Succeeded,
        "failed" => JobOutcome::Failed,
        _ => return None,
    };
    let started = s(d, "start_time");
    let finished = s(d, "last_updated");
    Some(JobRun {
        id: None,
        started: started.as_deref().and_then(utc),
        duration_ms: started
            .as_deref()
            .zip(finished.as_deref())
            .and_then(|(a, b)| between(a, b)),
        outcome,
        trigger: None,
        message: d
            .pointer("/info/error")
            .and_then(Value::as_str)
            .map(cut)
            .or_else(|| {
                let docs = d.pointer("/info/docs_written").and_then(Value::as_u64)?;
                Some(format!("{docs} documentos escritos"))
            }),
        steps: Vec::new(),
    })
}

/// Keys whose value is a secret, wherever they are in a document.
pub(crate) fn secret_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    [
        "password",
        "authorization",
        "cookie",
        "api_key",
        "apikey",
        "token",
        "secret",
    ]
    .iter()
    .any(|w| k.contains(w))
}

/// A URL with its password replaced (`http://u:***@h/db`).
fn hide_url_password(u: &str) -> (String, bool) {
    let Some((scheme, rest)) = u.split_once("://") else {
        return (u.to_string(), false);
    };
    let host_end = rest.find('/').unwrap_or(rest.len());
    let Some(at) = rest[..host_end].rfind('@') else {
        return (u.to_string(), false);
    };
    let userinfo = &rest[..at];
    match userinfo.split_once(':') {
        Some((user, _)) => (format!("{scheme}://{user}:{HIDDEN}{}", &rest[at..]), true),
        None => (u.to_string(), false),
    }
}

/// The document without the secrets it carries; true when there were any.
pub(crate) fn redacted(v: &Value) -> (Value, bool) {
    match v {
        Value::Object(m) => {
            let mut hid = false;
            let out: Map<String, Value> = m
                .iter()
                .map(|(k, v)| {
                    if secret_key(k) && !v.is_null() {
                        hid = true;
                        (k.clone(), Value::String(HIDDEN.into()))
                    } else {
                        let (v, h) = redacted(v);
                        hid |= h;
                        (k.clone(), v)
                    }
                })
                .collect();
            (Value::Object(out), hid)
        }
        Value::Array(a) => {
            let mut hid = false;
            let out = a
                .iter()
                .map(|x| {
                    let (x, h) = redacted(x);
                    hid |= h;
                    x
                })
                .collect();
            (Value::Array(out), hid)
        }
        Value::String(t) => {
            let (t, h) = hide_url_password(t);
            (Value::String(t), h)
        }
        other => (other.clone(), false),
    }
}

/// What the replicator writes on a document, which isn't part of its
/// definition.
pub(crate) fn replicator_state(k: &str) -> bool {
    k.starts_with("_replication_")
}

/// The `PUT` line of DBine's HTTP console that recreates a `_replicator`
/// document, its credentials hidden.
pub(crate) fn doc_script(db: &str, doc_id: &str, doc: &Value) -> String {
    let body: Map<String, Value> = doc
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(k, _)| *k != "_id" && *k != "_rev" && !replicator_state(k))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    let (body, hid) = redacted(&Value::Object(body));
    let mut out = String::new();
    if hid {
        out.push_str("// Las credenciales se ocultaron (***): completalas antes de ejecutarlo.\n");
    }
    out.push_str(&format!(
        "PUT /{}/{}\n{}\n",
        seg(db),
        seg(doc_id),
        serde_json::to_string_pretty(&body).unwrap_or_default()
    ));
    out
}

/// The `POST /_replicate` that starts a transient replication again, from
/// what the scheduler says of it (its options are in its id).
pub(crate) fn transient_script(rep: &str, j: &Value) -> String {
    let mut body = Map::new();
    body.insert(
        "source".into(),
        Value::String(s(j, "source").unwrap_or_default()),
    );
    body.insert(
        "target".into(),
        Value::String(s(j, "target").unwrap_or_default()),
    );
    for opt in rep.split('+').skip(1) {
        if matches!(opt, "continuous" | "create_target") {
            body.insert(opt.into(), Value::Bool(true));
        }
    }
    let (body, _) = redacted(&Value::Object(body));
    format!(
        "// Replicación transitoria: CouchDB no guarda su definición. Este script se arma con el origen y el destino que informa el planificador (sin credenciales ni filtros).\nPOST /_replicate\n{}\n",
        serde_json::to_string_pretty(&body).unwrap_or_default()
    )
}

fn prop(group: &str, label: &str, value: impl Into<String>) -> PropertyInfo {
    PropertyInfo {
        group: group.into(),
        label: label.into(),
        value: value.into(),
    }
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "sí"
    } else {
        "no"
    }
}

/// The figures a scheduler row carries in `info` (a document's or a job's).
fn stats_props(info: &Value, out: &mut Vec<PropertyInfo>) {
    for (k, label) in [
        ("docs_read", "Documentos leídos"),
        ("docs_written", "Documentos escritos"),
        ("doc_write_failures", "Escrituras fallidas"),
        ("changes_pending", "Cambios pendientes"),
        ("revisions_checked", "Revisiones revisadas"),
    ] {
        if let Some(n) = info.get(k).and_then(Value::as_u64) {
            out.push(prop("Estadísticas", label, n.to_string()));
        }
    }
}

pub(crate) fn denied(e: &Error) -> bool {
    match e {
        Error::AuthFailed(_) => true,
        Error::Statement(se) => matches!(se.code.as_deref(), Some("unauthorized" | "forbidden")),
        _ => false,
    }
}

pub(crate) fn missing(e: &Error) -> bool {
    match e {
        Error::Statement(se) => matches!(se.code.as_deref(), Some("not_found" | "missing")),
        _ => false,
    }
}

/// A server without `/_scheduler` (before CouchDB 2.1) takes `_scheduler`
/// as a database name: `illegal_database_name` (or `not_found`).
pub(crate) fn no_scheduler(e: &Error) -> bool {
    match e {
        Error::Statement(se) => matches!(
            se.code.as_deref(),
            Some("not_found" | "missing" | "illegal_database_name" | "bad_request")
        ),
        _ => false,
    }
}

/// The note of a list cut at [`MAX_JOBS`].
fn truncated_note() -> String {
    format!("Se muestran las primeras {MAX_JOBS} replicaciones.")
}

pub(crate) fn not_found(id: &str) -> Error {
    Error::Query(format!(
        "no existe la replicación «{}» o ya no está en el planificador",
        id.chars().take(200).collect::<String>()
    ))
}

impl CouchSession {
    /// Every row of a paged scheduler listing (`docs` or `jobs`).
    pub(crate) async fn scheduler(&self, what: &str) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        loop {
            let page = self
                .call(
                    Method::GET,
                    &format!("/_scheduler/{what}?limit={PAGE}&skip={}", out.len()),
                    None,
                )
                .await?;
            let rows = page
                .get(what)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let n = rows.len();
            out.extend(rows);
            if n < PAGE || out.len() >= MAX_JOBS {
                return Ok(out);
            }
        }
    }

    /// The `_replicator` documents the scheduler knows (none when there's
    /// no `_replicator` database).
    async fn scheduler_docs(&self) -> Result<Vec<Value>> {
        match self.scheduler("docs").await {
            Err(e) if missing(&e) => Ok(Vec::new()),
            r => r,
        }
    }

    pub(crate) async fn scheduler_doc(&self, db: &str, doc: &str) -> Result<Value> {
        self.call(
            Method::GET,
            &format!("/_scheduler/docs/{}/{}", seg(db), seg(doc)),
            None,
        )
        .await
    }

    /// The scheduler's job row of a document, if it has one now.
    async fn job_of_doc(&self, db: &str, doc: &str) -> Result<Option<Value>> {
        Ok(self.scheduler("jobs").await?.into_iter().find(|j| {
            s(j, "database").as_deref() == Some(db) && s(j, "doc_id").as_deref() == Some(doc)
        }))
    }

    pub(crate) async fn transient(&self, rep: &str) -> Result<Value> {
        self.scheduler("jobs")
            .await?
            .into_iter()
            .find(|j| {
                j.get("database").is_none_or(Value::is_null) && s(j, "id").as_deref() == Some(rep)
            })
            .ok_or_else(|| not_found(&format!("rep:{rep}")))
    }

    pub(crate) async fn replication_doc(&self, db: &str, doc: &str) -> Result<Value> {
        self.call(Method::GET, &format!("/{}/{}", seg(db), seg(doc)), None)
            .await
    }

    pub(crate) async fn server_jobs(&mut self) -> Result<JobList> {
        let empty = |note: &str| JobList {
            jobs: Vec::new(),
            notes: vec![note.into()],
        };
        // `/_scheduler/jobs` always answers on 2.1+, so it tells whether
        // the server has the scheduler at all; `/_scheduler/docs` may be
        // missing only for lack of a `_replicator` database.
        let jobs = match self.scheduler("jobs").await {
            Ok(j) => j,
            Err(e) if denied(&e) => return Ok(empty(DENIED)),
            Err(e) if no_scheduler(&e) => return Ok(empty(NO_SCHEDULER)),
            Err(e) => return Err(e),
        };
        let docs = match self.scheduler_docs().await {
            Ok(d) => d,
            Err(e) if denied(&e) => return Ok(empty(DENIED)),
            Err(e) => return Err(e),
        };
        Ok(list_of(&docs, &jobs))
    }

    pub(crate) async fn server_job_detail(&mut self, id: &str) -> Result<JobDetail> {
        match parse_id(id)? {
            JobId::Doc { db, doc } => {
                let d = self.scheduler_doc(&db, &doc).await.map_err(|e| {
                    if missing(&e) {
                        not_found(id)
                    } else {
                        e
                    }
                })?;
                let job = doc_job(&d).ok_or_else(|| not_found(id))?;
                let mut properties = Vec::new();
                if let Some(st) = s(&d, "state") {
                    properties.push(prop("", "Estado", st));
                }
                if let Some(e) = d.pointer("/info/error").and_then(Value::as_str) {
                    properties.push(prop("", "Último error", cut(e)));
                }
                if let Some(n) = d.get("error_count").and_then(Value::as_u64) {
                    properties.push(prop("", "Errores seguidos", n.to_string()));
                }
                if let Some(r) = s(&d, "id") {
                    properties.push(prop("", "Id de replicación", r));
                }
                if let Some(n) = s(&d, "node") {
                    properties.push(prop("", "Nodo", n));
                }
                if let Some(t) = s(&d, "last_updated").as_deref().and_then(utc) {
                    properties.push(prop("", "Actualizada", t));
                }
                // The document says what the scheduler doesn't.
                match self.replication_doc(&db, &doc).await {
                    Ok(body) => {
                        if let Some(o) = s(&body, "owner") {
                            properties.push(prop("", "Dueño", o));
                        }
                        let flag = |k: &str| body.get(k).and_then(Value::as_bool).unwrap_or(false);
                        properties.push(prop("", "Continua", yes_no(flag("continuous"))));
                        if flag("create_target") {
                            properties.push(prop("", "Crea el destino", yes_no(true)));
                        }
                        if let Some(f) = s(&body, "filter") {
                            properties.push(prop("", "Filtro", f));
                        }
                        if let Some(ids) = body.get("doc_ids").and_then(Value::as_array) {
                            properties.push(prop("", "Documentos elegidos", ids.len().to_string()));
                        }
                        if let Some(sel) = body.get("selector") {
                            properties.push(prop("", "Selector", sel.to_string()));
                        }
                    }
                    Err(e) => tracing::debug!("couchdb: replication doc {db}/{doc}: {e}"),
                }
                if let Some(info) = d.get("info") {
                    stats_props(info, &mut properties);
                }
                let mut notes = vec![DOC_STOP_NOTE.to_string()];
                if job.can_run {
                    notes.push(DOC_RUN_NOTE.into());
                }
                Ok(JobDetail {
                    steps: vec![JobStep {
                        name: "Replicación".into(),
                        kind: Some("CouchDB".into()),
                        target: s(&d, "target"),
                        command: job.description.clone(),
                    }],
                    schedules: Vec::new(),
                    properties,
                    notes,
                    job,
                })
            }
            JobId::Transient(rep) => {
                let j = self.transient(&rep).await?;
                let job = transient_job(&j).ok_or_else(|| not_found(id))?;
                let mut properties = vec![prop("", "Id de replicación", rep.clone())];
                properties.push(prop("", "Continua", yes_no(rep.contains("+continuous"))));
                if let Some(n) = s(&j, "node") {
                    properties.push(prop("", "Nodo", n));
                }
                if let Some(info) = j.get("info") {
                    stats_props(info, &mut properties);
                }
                Ok(JobDetail {
                    steps: vec![JobStep {
                        name: "Replicación".into(),
                        kind: Some("CouchDB".into()),
                        target: s(&j, "target"),
                        command: job.description.clone(),
                    }],
                    schedules: Vec::new(),
                    properties,
                    notes: vec![TRANSIENT_NOTE.into()],
                    job,
                })
            }
        }
    }

    pub(crate) async fn server_job_runs(&mut self, id: &str, limit: u32) -> Result<Vec<JobRun>> {
        let limit = limit.min(MAX_JOB_RUNS) as usize;
        let mut runs = match parse_id(id)? {
            JobId::Doc { db, doc } => {
                let d = self.scheduler_doc(&db, &doc).await.map_err(|e| {
                    if missing(&e) {
                        not_found(id)
                    } else {
                        e
                    }
                })?;
                let running = s(&d, "state").as_deref() == Some("running");
                match self.job_of_doc(&db, &doc).await? {
                    Some(j) => runs_of(
                        j.get("history")
                            .and_then(Value::as_array)
                            .map(Vec::as_slice)
                            .unwrap_or(&[]),
                        running,
                    ),
                    None => final_run(&d).into_iter().collect(),
                }
            }
            JobId::Transient(rep) => {
                let j = self.transient(&rep).await?;
                let running = j.get("pid").is_some_and(|p| !p.is_null());
                runs_of(
                    j.get("history")
                        .and_then(Value::as_array)
                        .map(Vec::as_slice)
                        .unwrap_or(&[]),
                    running,
                )
            }
        };
        runs.truncate(limit);
        Ok(runs)
    }

    pub(crate) async fn run_server_job(&mut self, id: &str) -> Result<()> {
        self.refuse_if_read_only(
            "Conexión de solo lectura: no se pueden ejecutar jobs del servidor.",
        )?;
        let JobId::Doc { db, doc } = parse_id(id)? else {
            return Err(Error::Unsupported(
                "una replicación transitoria no se puede volver a ejecutar: hay que iniciarla de nuevo con POST /_replicate".into(),
            ));
        };
        let d = self.scheduler_doc(&db, &doc).await?;
        let job = doc_job(&d).ok_or_else(|| not_found(id))?;
        if !job.can_run {
            return Err(Error::Query(format!(
                "la replicación «{doc}» ya está en marcha o esperando su turno"
            )));
        }
        // The same document without the replicator's state: a new
        // revision makes the replicator take it again.
        let body = self.replication_doc(&db, &doc).await?;
        let clean: Map<String, Value> = body
            .as_object()
            .map(|m| {
                m.iter()
                    .filter(|(k, _)| !replicator_state(k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default();
        self.call(
            Method::PUT,
            &format!("/{}/{}", seg(&db), seg(&doc)),
            Some(&Value::Object(clean)),
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn stop_server_job(&mut self, id: &str) -> Result<()> {
        self.refuse_if_read_only(
            "Conexión de solo lectura: no se pueden detener jobs del servidor.",
        )?;
        match parse_id(id)? {
            JobId::Doc { db, doc } => {
                let body = self.replication_doc(&db, &doc).await.map_err(|e| {
                    if missing(&e) {
                        not_found(id)
                    } else {
                        e
                    }
                })?;
                let rev = s(&body, "_rev").ok_or_else(|| not_found(id))?;
                self.call(
                    Method::DELETE,
                    &format!("/{}/{}?rev={}", seg(&db), seg(&doc), seg(&rev)),
                    None,
                )
                .await
                .map(|_| ())
            }
            JobId::Transient(rep) => {
                self.transient(&rep).await?;
                self.call(
                    Method::POST,
                    "/_replicate",
                    Some(&json!({ "replication_id": rep, "cancel": true })),
                )
                .await
                .map(|_| ())
            }
        }
    }

    pub(crate) async fn server_job_script(&mut self, id: &str) -> Result<String> {
        match parse_id(id)? {
            JobId::Doc { db, doc } => {
                let body = self.replication_doc(&db, &doc).await.map_err(|e| {
                    if missing(&e) {
                        not_found(id)
                    } else {
                        e
                    }
                })?;
                Ok(doc_script(&db, &doc, &body))
            }
            JobId::Transient(rep) => Ok(transient_script(&rep, &self.transient(&rep).await?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REP: &str = "479f1ae13ed5c42df80a2de598fcd1d9+continuous";

    #[test]
    fn ids_have_their_shape() {
        assert_eq!(
            parse_id("doc:_replicator:nightly").unwrap(),
            JobId::Doc {
                db: "_replicator".into(),
                doc: "nightly".into()
            }
        );
        assert_eq!(
            parse_id("doc:app/_replicator:a:b c").unwrap(),
            JobId::Doc {
                db: "app/_replicator".into(),
                doc: "a:b c".into()
            }
        );
        assert_eq!(
            parse_id(&format!("rep:{REP}")).unwrap(),
            JobId::Transient(REP.into())
        );
        for bad in [
            "",
            "nightly",
            "doc:users:x",
            "doc:_replicator:",
            "doc:_replicator:_design/x",
            "doc:Bad/_replicator:x",
            "doc:_replicator:a\nb",
            "doc:_replicator:..",
            "doc:_replicator:.",
            "doc:app/_replicator:..",
            "rep:xyz",
            "rep:479f1ae13ed5c42df80a2de598fcd1d9+continuous;x",
            "rep:479f1ae13ed5c42df80a2de598fcd1d9/../_users",
            "rep:479f1ae13ed5c42df80a2de598fcd1d9+",
        ] {
            assert!(parse_id(bad).is_err(), "{bad:?}");
        }
    }

    fn docs() -> Vec<Value> {
        vec![
            json!({"database": "_replicator", "doc_id": "live", "id": REP, "node": "n1",
                "source": "http://h:5984/src/", "target": "http://h:5984/dst/", "state": "running",
                "info": {"docs_read": 3, "docs_written": 3}, "error_count": 0,
                "start_time": "2026-11-12T16:09:56Z", "last_updated": "2026-11-12T16:09:56Z"}),
            json!({"database": "_replicator", "doc_id": "once", "state": "completed",
                "source": "http://h:5984/src/", "target": "http://h:5984/dst/",
                "info": {"docs_written": 1}, "start_time": "2026-11-12T16:39:02Z", "last_updated": "2026-11-12T16:39:03Z"}),
            json!({"database": "_replicator", "doc_id": "bad", "state": "crashing",
                "info": {"error": "db_not_found: could not open http://h:5984/nope/"}, "error_count": 1,
                "start_time": "2026-11-12T16:09:56Z"}),
            json!({"database": "_users", "doc_id": "x", "state": "running"}),
        ]
    }

    #[test]
    fn documents_become_jobs() {
        let jobs: Vec<ServerJob> = docs().iter().filter_map(doc_job).collect();
        assert_eq!(jobs.len(), 3, "a document of another database is left out");
        let (live, once, bad) = (&jobs[0], &jobs[1], &jobs[2]);
        assert_eq!(live.id, "doc:_replicator:live");
        assert_eq!(live.kind, "_replicator");
        assert_eq!(live.group.as_deref(), Some("_replicator"));
        assert_eq!(
            live.description.as_deref(),
            Some("http://h:5984/src/ → http://h:5984/dst/")
        );
        assert!(live.running && live.can_stop && !live.can_run && !live.can_enable);
        assert_eq!(live.last_outcome, Some(JobOutcome::Running));
        assert_eq!(live.last_started.as_deref(), Some("2026-11-12T16:09:56Z"));
        assert_eq!(once.last_outcome, Some(JobOutcome::Succeeded));
        assert_eq!(once.last_duration_ms, Some(1000));
        assert!(once.can_run && !once.can_stop);
        assert_eq!(bad.last_outcome, Some(JobOutcome::Retrying));
        assert!(bad.can_run && bad.can_stop);
        assert!(jobs
            .iter()
            .all(|j| j.has_history && j.has_script && !j.system));
        let r = final_run(&docs()[1]).unwrap();
        assert_eq!(r.outcome, JobOutcome::Succeeded);
        assert_eq!(r.message.as_deref(), Some("1 documentos escritos"));
    }

    #[test]
    fn a_cut_list_says_so() {
        let full = list_of(&docs(), &[]);
        assert_eq!(full.jobs.len(), 3);
        assert!(full.notes.is_empty());
        let many: Vec<Value> = (0..MAX_JOBS)
            .map(|i| json!({"database": "_replicator", "doc_id": format!("d{i}"), "state": "running"}))
            .collect();
        let cut = list_of(&many, &[json!({"database": null, "id": REP})]);
        assert_eq!(cut.jobs.len(), MAX_JOBS);
        assert_eq!(cut.notes, vec![truncated_note()]);
    }

    #[test]
    fn a_server_without_the_scheduler_is_told_apart() {
        let err = |code: &str| {
            Error::Statement(Box::new(
                dbine_driver::ScriptError::new("x".to_string()).with_code(code),
            ))
        };
        assert!(no_scheduler(&err("illegal_database_name")));
        assert!(no_scheduler(&err("not_found")));
        assert!(!no_scheduler(&err("unauthorized")));
        assert!(denied(&err("unauthorized")));
    }

    #[test]
    fn transient_jobs_and_their_runs() {
        let j = json!({"database": null, "id": REP, "pid": "<0.1899.0>", "user": "admin", "doc_id": null,
        "source": "http://h:5984/src/", "target": "http://h:5984/dst/", "node": "n1",
        "start_time": "2026-11-12T16:10:07Z",
        "history": [
            {"timestamp": "2026-11-12T16:10:30Z", "type": "started"},
            {"timestamp": "2026-11-12T16:10:20Z", "type": "crashed", "reason": "timeout"},
            {"timestamp": "2026-11-12T16:10:07Z", "type": "started"},
            {"timestamp": "2026-11-12T16:10:07Z", "type": "added"}
        ]});
        let job = transient_job(&j).unwrap();
        assert_eq!(job.id, format!("rep:{REP}"));
        assert_eq!(job.name, "src → dst");
        assert_eq!(job.owner.as_deref(), Some("admin"));
        assert!(job.running && job.can_stop && !job.can_run);
        let runs = runs_of(j["history"].as_array().unwrap(), true);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].outcome, JobOutcome::Running);
        assert_eq!(runs[0].started.as_deref(), Some("2026-11-12T16:10:30Z"));
        assert_eq!(runs[1].outcome, JobOutcome::Failed);
        assert_eq!(runs[1].duration_ms, Some(13_000));
        assert_eq!(runs[1].message.as_deref(), Some("timeout"));
        // A doc job's row isn't a transient one.
        assert!(transient_job(&json!({"database": "_replicator", "id": REP})).is_none());
        let s = transient_script(REP, &j);
        assert!(s.contains("POST /_replicate") && s.contains("\"continuous\": true"));
    }

    #[test]
    fn scripts_hide_credentials() {
        let doc = json!({"_id": "once", "_rev": "2-ff", "owner": "admin",
            "source": {"url": "http://127.0.0.1:5984/src", "auth": {"basic": {"username": "admin", "password": "secret"}},
                       "headers": {"Authorization": "Basic YWRtaW46c2VjcmV0"}},
            "target": "http://admin:secret@127.0.0.1:5984/dst",
            "continuous": true,
            "_replication_state": "completed", "_replication_stats": {"docs_read": 1}});
        let s = doc_script("_replicator", "once", &doc);
        assert!(!s.contains("secret") && !s.contains("YWRtaW46"), "{s}");
        assert!(s.starts_with("// Las credenciales se ocultaron"));
        assert!(s.contains("PUT /_replicator/once\n{"));
        assert!(s.contains("http://admin:***@127.0.0.1:5984/dst"));
        assert!(!s.contains("_rev") && !s.contains("_replication_"));
        assert!(s.contains("\"continuous\": true"));
        // A database with a prefix and an id with spaces are encoded.
        let s = doc_script(
            "app/_replicator",
            "a b",
            &json!({"source": "s", "target": "t"}),
        );
        assert!(s.starts_with("PUT /app%2F_replicator/a%20b\n"), "{s}");
        assert!(!s.contains("ocultaron"));
        assert_eq!(hide_url_password("http://h:5984/db").0, "http://h:5984/db");
        assert_eq!(hide_url_password("https://u@h/db").0, "https://u@h/db");
    }
}
