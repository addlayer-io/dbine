//! The Backup Service (Couchbase Server Enterprise 7.0 on) managed whole
//! through the scheduler contract ([`dbine_driver::Session::scheduler_info`]
//! and the rest): what the Backup page of Couchbase's console does with its
//! plans and repositories.
//!
//! - Kinds: active repositories (`job`, the ids `jobs()` returns), plans,
//!   archived and imported repositories, and the service's settings (the
//!   task history's rotation).
//! - Scripts: the REST requests the service takes (`/api/v1/…` on port
//!   8097, or `/_p/backup/api/v1/…` on the management port, which is how
//!   DBine sends them), as `METHOD path` lines with their JSON bodies.
//!   Secrets (cloud and KMS keys) are masked in the script and sent only by
//!   the save; the service never gives them back.
//! - Edits are what the service changes in place: a plan is replaced whole
//!   (`PUT`), only when it isn't built in or used by an active repository
//!   (the service refuses both); an active repository changes its plan
//!   (paused around the change, as the service requires), its state
//!   (pause / resume) and its retention. The rest of a repository (bucket,
//!   storage, encryption) is fixed at creation.
//! - Deletes: the service only deletes archived or imported repositories,
//!   and never with DBine their backups (`remove_repository=false`): an
//!   active repository is archived and then deleted from the service, its
//!   backups left where they are, to import again.
//! - Run a task now (`run_job_from`, a task of the repository's plan): a
//!   backup (full or not as the task says), a merge with the task's day
//!   offsets, or a prune. The service keeps no history it can clear (it
//!   rotates it by size): `clear_job_history` says so.
//! - Logs: the cluster's system events of the Backup Service (`/events`,
//!   7.1 on), newest first.
//! - Permissions: `cluster.backup!read` to see, `cluster.backup!all` to
//!   change (`/pools/default/checkPermissions`); forms come read-only with
//!   a note otherwise.
//!
//! Ids and values come from the app as untrusted text: names are checked
//! against the service's own rule before they reach a path (and encoded
//! there), the objects an edit or a delete names are read from the service
//! first, and every value is checked before the request is written.

mod logs;
mod plan;
mod repo;
mod settings;
#[cfg(test)]
mod tests;

pub(crate) use logs::logs;
pub(crate) use repo::{clear_history, run_from};

use crate::jobs::{self, denied, API};
use crate::CbSession;
use dbine_driver::{
    Error, FormValues, ObjectForm, Result, SchedulerAction, SchedulerInfo, SchedulerKind,
    SchedulerList, SECRET_MASK,
};
use reqwest::Method;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Notes and messages
// ---------------------------------------------------------------------------

const READ_ONLY_USER: &str = "Este usuario puede ver el servicio de backup pero no cambiarlo (hace falta el rol Backup Full Admin o Full Admin).";
const READ_ONLY_FORM: &str =
    "Este usuario no puede cambiar el servicio de backup (hace falta el rol Backup Full Admin o Full Admin).";
const DELETE_NOTE: &str = "El servicio de backup solo borra repositorios archivados o importados, y DBine los borra sin sus backups: quedan en su archivo, para importarlos de nuevo.";

/// On forms with keys: they never come back.
pub(crate) const SECRET_KEYS_NOTE: &str = "Las claves secretas se escriben al crear el repositorio: el servicio no las devuelve, y el script las muestra con asteriscos.";

/// A change of more than one request: the service has no transactions.
pub(crate) const NOT_ATOMIC: &str = "Los pedidos se envían uno por uno, sin transacción: si uno falla, los anteriores quedan aplicados y los siguientes no se envían.";

/// The first line of every script.
const HEADER: &str = "// API REST del servicio de backup de Couchbase (puerto 8097, o /_p/backup en el puerto de administración).";

fn bad(message: String) -> Error {
    Error::Query(message)
}

/// Text from the app, short and on one line, for a message.
fn shown(s: &str) -> String {
    comment_text(s.trim()).chars().take(60).collect()
}

/// Text for a `//` comment line: line breaks and other control characters
/// become `?`, so a name can't end the comment.
pub(crate) fn comment_text(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                '?'
            } else {
                c
            }
        })
        .collect()
}

/// A name the service takes for a new plan or repository:
/// `^[0-9A-Za-z][0-9A-Za-z_-]{0,49}$`.
pub(crate) fn new_name(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && b.len() <= 50
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
}

// ---------------------------------------------------------------------------
// Kinds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// An active repository: what `jobs()` lists.
    Job,
    Plan,
    Archived,
    Imported,
    Settings,
}

impl Kind {
    fn parse(kind: &str) -> Result<Kind> {
        Ok(match kind.trim() {
            "job" => Kind::Job,
            "plan" => Kind::Plan,
            "archived" => Kind::Archived,
            "imported" => Kind::Imported,
            "settings" => Kind::Settings,
            other => {
                return Err(bad(format!(
                    "«{}» no es un tipo de objeto del servicio de backup de Couchbase",
                    shown(other)
                )))
            }
        })
    }
}

// ---------------------------------------------------------------------------
// What the cluster and the user allow
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Ctx {
    /// Some node runs the Backup Service.
    pub(crate) service: bool,
    pub(crate) enterprise: bool,
    /// `cluster.backup!read`.
    pub(crate) read: bool,
    /// `cluster.backup!all`.
    pub(crate) write: bool,
    /// `cluster.events!read` on 7.1 or later.
    pub(crate) events: bool,
    /// The cluster's version, `(major, minor)`.
    pub(crate) version: (u32, u32),
}

impl Ctx {
    /// Couchbase Server 8.0 on: retention, prune tasks, encrypted
    /// repositories, users in backups, the repository's plan changed.
    pub(crate) fn v8(&self) -> bool {
        self.version >= (8, 0)
    }

    fn missing(&self) -> Option<&'static str> {
        if !self.read {
            Some(jobs::DENIED)
        } else if !self.service {
            Some(if self.enterprise {
                jobs::NO_SERVICE
            } else {
                jobs::COMMUNITY
            })
        } else {
            None
        }
    }
}

/// `8.0.0-3777-enterprise` → `(8, 0)`.
pub(crate) fn version_of(v: &str) -> (u32, u32) {
    let mut parts = v.split(|c: char| !c.is_ascii_digit());
    let major = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let minor = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    (major, minor)
}

impl CbSession {
    /// One request to the Backup Service (`path` after `/api/v1`, already
    /// encoded). Its errors carry the service's own message.
    pub(crate) async fn backup_call(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        let mut rq = self
            .conn
            .http
            .request(method, format!("{}{API}{path}", self.conn.mgmt))
            .basic_auth(&self.conn.user, Some(&self.conn.password));
        if let Some(b) = body {
            rq = rq.json(b);
        }
        let resp = rq.send().await.map_err(crate::http_error)?;
        let status = resp.status();
        let text = resp.text().await.map_err(crate::http_error)?;
        if status.is_success() {
            return Ok(serde_json::from_str(&text).unwrap_or(Value::Null));
        }
        if status.as_u16() == 401 {
            return Err(Error::AuthFailed(
                "Couchbase rechazó el usuario o la contraseña.".into(),
            ));
        }
        Err(Error::Query(service_error(status.as_u16(), &text)))
    }

    /// The permissions the user has, of those asked.
    async fn permissions(&self, perms: &[&str]) -> Result<Map<String, Value>> {
        let text = self
            .conn
            .mgmt_send(
                self.conn
                    .http
                    .post(format!("{}/pools/default/checkPermissions", self.conn.mgmt))
                    .header("Content-Type", "text/plain")
                    .body(perms.join(",")),
            )
            .await?;
        Ok(serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default())
    }
}

/// The service's error (`{"status": 400, "msg": "…", "extras": "…"}`) as a
/// message.
pub(crate) fn service_error(status: u16, text: &str) -> String {
    let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let msg = v
        .get("msg")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|m| !m.is_empty());
    match msg {
        Some(m) => {
            let extras = v
                .get("extras")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|e| !e.is_empty());
            let m: String = m.chars().take(2000).collect();
            match (status, extras) {
                (403, _) => format!("Sin permiso en el servicio de backup: {m}"),
                (_, Some(e)) => format!(
                    "El servicio de backup respondió: {m} ({})",
                    e.chars().take(1000).collect::<String>()
                ),
                _ => format!("El servicio de backup respondió: {m}"),
            }
        }
        None => format!(
            "El servicio de backup respondió HTTP {status}: {}",
            text.trim().chars().take(2000).collect::<String>()
        ),
    }
}

async fn ctx(s: &CbSession) -> Result<Ctx> {
    let pools = s.conn.mgmt_get("/pools").await?;
    let version = version_of(
        pools
            .get("implementationVersion")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let mut c = Ctx {
        version,
        enterprise: pools
            .get("isEnterprise")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        ..Ctx::default()
    };
    match s.backup_service().await {
        Ok((has, enterprise)) => {
            c.service = has;
            c.enterprise = enterprise;
        }
        Err(e) if denied(&e) => return Ok(c),
        Err(e) => return Err(e),
    }
    let mut asked = vec!["cluster.backup!read", "cluster.backup!all"];
    if version >= (7, 1) {
        asked.push("cluster.events!read");
    }
    let perms = match s.permissions(&asked).await {
        Ok(p) => p,
        // A version that doesn't know the events' permission refuses the
        // whole question: ask again without it.
        Err(e) if !denied(&e) && asked.len() > 2 => {
            asked.pop();
            s.permissions(&asked).await?
        }
        Err(e) => return Err(e),
    };
    let has = |p: &str| perms.get(p).and_then(Value::as_bool) == Some(true);
    c.read = has("cluster.backup!read");
    c.write = has("cluster.backup!all");
    c.events = has("cluster.events!read");
    Ok(c)
}

/// The context, refusing a cluster whose Backup Service DBine can't see.
async fn ready(s: &CbSession) -> Result<Ctx> {
    let c = ctx(s).await?;
    match c.missing() {
        Some(note) if c.read => Err(Error::Unsupported(note.into())),
        Some(note) => Err(Error::Query(note.into())),
        None => Ok(c),
    }
}

/// The context, for a change.
async fn ready_to_change(s: &CbSession) -> Result<Ctx> {
    let c = ready(s).await?;
    if !c.write {
        return Err(Error::Query(READ_ONLY_FORM.into()));
    }
    Ok(c)
}

/// The kinds, in the order of the console's Backup page, and the notes.
pub(crate) fn info_of(c: &Ctx) -> SchedulerInfo {
    if let Some(note) = c.missing() {
        return SchedulerInfo {
            kinds: Vec::new(),
            logs: false,
            notes: vec![note.to_string()],
        };
    }
    let w = c.write;
    let mut job = SchedulerKind::new("job", "Repositorios", "repositorio")
        .abilities(w, w, w)
        // A task of the repository's plan, now (`run_job_from`).
        .run_from_step(w)
        // The service rotates the task history by size: nothing to clear.
        .clear_history(false);
    if w {
        // "Ejecutar ahora" is an incremental backup: a full one, and a
        // merge of every backup, are the console's other two.
        job = job
            .action(SchedulerAction::new("backup_now", "Backup completo ahora"))
            .action(SchedulerAction::new(
                "merge_now",
                "Unir todos los backups ahora",
            ));
    }
    let kinds = vec![
        job,
        SchedulerKind::new("plan", "Planes", "plan").abilities(w, w, w),
        SchedulerKind::new(
            "archived",
            "Repositorios archivados",
            "repositorio archivado",
        )
        .abilities(w, false, w),
        SchedulerKind::new(
            "imported",
            "Repositorios importados",
            "repositorio importado",
        )
        .abilities(w, false, w),
        SchedulerKind::new("settings", "Configuración", "configuración").abilities(false, w, false),
    ];
    let mut notes = Vec::new();
    if !w {
        notes.push(READ_ONLY_USER.to_string());
    }
    notes.push(DELETE_NOTE.to_string());
    SchedulerInfo {
        kinds,
        logs: c.events && c.version >= (7, 1),
        notes,
    }
}

// ---------------------------------------------------------------------------
// The contract's calls
// ---------------------------------------------------------------------------

pub(crate) async fn info(s: &CbSession) -> Result<SchedulerInfo> {
    Ok(info_of(&ctx(s).await?))
}

pub(crate) async fn items(s: &CbSession, kind: &str) -> Result<SchedulerList> {
    let kind = Kind::parse(kind)?;
    let c = ready(s).await?;
    match kind {
        Kind::Job => repo::active_items(s, c.write).await,
        Kind::Plan => plan::items(s).await,
        Kind::Archived => repo::stored_items(s, repo::Stored::Archived).await,
        Kind::Imported => repo::stored_items(s, repo::Stored::Imported).await,
        Kind::Settings => settings::items(&c),
    }
}

pub(crate) async fn form(s: &CbSession, kind: &str, id: Option<&str>) -> Result<ObjectForm> {
    let kind = Kind::parse(kind)?;
    let c = ready(s).await?;
    let mut f = match kind {
        Kind::Job => repo::active_form(s, &c, id).await?,
        Kind::Plan => plan::form(s, &c, id).await?,
        Kind::Archived => repo::archived_form(s, &c, id).await?,
        Kind::Imported => repo::imported_form(s, &c, id).await?,
        Kind::Settings => settings::form(s, &c, id).await?,
    };
    if !c.write && !f.read_only {
        f.read_only = true;
        f.notes.insert(0, READ_ONLY_FORM.to_string());
    }
    Ok(f)
}

pub(crate) async fn script(
    s: &CbSession,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<String> {
    let kind = Kind::parse(kind)?;
    let c = ready(s).await?;
    Ok(change(s, &c, kind, id, values).await?.script())
}

pub(crate) async fn save(
    s: &CbSession,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Option<String>> {
    let kind = Kind::parse(kind)?;
    let c = ready_to_change(s).await?;
    let ch = change(s, &c, kind, id, values).await?;
    run(s, &ch).await?;
    Ok(ch.id)
}

pub(crate) async fn delete_script(s: &CbSession, kind: &str, id: &str) -> Result<String> {
    let kind = Kind::parse(kind)?;
    ready(s).await?;
    Ok(delete_change(s, kind, id).await?.script())
}

pub(crate) async fn delete(s: &CbSession, kind: &str, id: &str) -> Result<()> {
    let kind = Kind::parse(kind)?;
    ready_to_change(s).await?;
    let ch = delete_change(s, kind, id).await?;
    run(s, &ch).await
}

pub(crate) async fn action_script(
    s: &CbSession,
    kind: &str,
    id: &str,
    action: &str,
) -> Result<String> {
    let kind = Kind::parse(kind)?;
    ready(s).await?;
    Ok(action_change(s, kind, id, action).await?.script())
}

pub(crate) async fn action(s: &CbSession, kind: &str, id: &str, action: &str) -> Result<()> {
    let kind = Kind::parse(kind)?;
    ready_to_change(s).await?;
    let ch = action_change(s, kind, id, action).await?;
    run(s, &ch).await
}

/// Only active repositories have actions (a full backup, a merge now).
async fn action_change(s: &CbSession, kind: Kind, id: &str, action: &str) -> Result<Change> {
    match kind {
        Kind::Job => repo::action_change(s, id, action).await,
        _ => Err(Error::Unsupported(
            "solo los repositorios activos tienen acciones (un backup completo o unir sus backups ahora)"
                .into(),
        )),
    }
}

async fn change(
    s: &CbSession,
    c: &Ctx,
    kind: Kind,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Change> {
    match kind {
        Kind::Job => repo::active_change(s, c, id, values).await,
        Kind::Plan => plan::change(s, c, id, values).await,
        Kind::Archived => repo::archived_change(s, id, values).await,
        Kind::Imported => repo::imported_change(s, c, id, values).await,
        Kind::Settings => settings::change(s, id, values).await,
    }
}

async fn delete_change(s: &CbSession, kind: Kind, id: &str) -> Result<Change> {
    match kind {
        Kind::Job => repo::active_delete(s, id).await,
        Kind::Plan => plan::delete(s, id).await,
        Kind::Archived => repo::stored_delete(s, repo::Stored::Archived, id).await,
        Kind::Imported => repo::stored_delete(s, repo::Stored::Imported, id).await,
        Kind::Settings => Err(bad(
            "La configuración del servicio de backup no se borra.".into()
        )),
    }
}

// ---------------------------------------------------------------------------
// Changes: the requests a save sends, and their script
// ---------------------------------------------------------------------------

/// One request of a change.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Call {
    pub(crate) method: Method,
    /// After `/api/v1`, its segments encoded.
    pub(crate) path: String,
    /// What is sent.
    pub(crate) body: Option<Value>,
    /// Top-level keys of `body` that hold secrets: masked in the script.
    pub(crate) secrets: Vec<&'static str>,
}

impl Call {
    pub(crate) fn new(method: Method, path: String, body: Option<Value>) -> Call {
        Call {
            method,
            path,
            body,
            secrets: Vec::new(),
        }
    }

    /// The body as the script shows it.
    fn shown_body(&self) -> Option<Value> {
        let mut b = self.body.clone()?;
        if let Some(m) = b.as_object_mut() {
            for k in &self.secrets {
                if let Some(v) = m.get_mut(*k) {
                    if !v.is_null() {
                        *v = Value::String(SECRET_MASK.into());
                    }
                }
            }
        }
        Some(b)
    }
}

/// What a save (or a delete) sends.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Change {
    /// Comment lines on top of the script (without the `// `).
    pub(crate) comments: Vec<String>,
    /// What the user loses or risks with it (backups merged into one, a
    /// repository left paused…), one Spanish sentence each, as `// ⚠`
    /// lines after the comments: the app lists them and asks to continue.
    /// [`NOT_ATOMIC`] is added by [`Change::warnings`].
    pub(crate) warnings: Vec<String>,
    pub(crate) calls: Vec<Call>,
    /// If a call after the one at this index fails, this call undoes what
    /// the earlier ones did (a resume after a pause).
    pub(crate) undo: Option<(usize, Call)>,
    /// The object's id after the change.
    pub(crate) id: Option<String>,
}

impl Change {
    pub(crate) fn nothing(name: &str, id: String) -> Change {
        Change {
            comments: vec![format!("«{name}»: no hay cambios que guardar.")],
            id: Some(id),
            ..Change::default()
        }
    }

    /// The change's warnings, and [`NOT_ATOMIC`] when it sends more than
    /// one request.
    pub(crate) fn warnings(&self) -> Vec<String> {
        let mut w = self.warnings.clone();
        if self.calls.len() > 1 {
            w.push(NOT_ATOMIC.into());
        }
        w
    }

    pub(crate) fn script(&self) -> String {
        let mut out = format!("{HEADER}\n");
        for c in &self.comments {
            out.push_str(&format!("// {}\n", comment_text(c)));
        }
        for w in self.warnings() {
            out.push_str(&format!("// ⚠ {}\n", comment_text(&w)));
        }
        for call in &self.calls {
            out.push('\n');
            out.push_str(&format!("{} /api/v1{}\n", call.method, call.path));
            if let Some(b) = call.shown_body() {
                out.push_str(&serde_json::to_string_pretty(&b).unwrap_or_default());
                out.push('\n');
            }
        }
        out
    }
}

/// Sends a change's requests in order. A failure after the first one says
/// how many went through (the service has no transactions).
async fn run(s: &CbSession, ch: &Change) -> Result<()> {
    for (i, call) in ch.calls.iter().enumerate() {
        if let Err(e) = s
            .backup_call(call.method.clone(), &call.path, call.body.as_ref())
            .await
        {
            if let Some((after, undo)) = &ch.undo {
                if i > *after {
                    if let Err(u) = s
                        .backup_call(undo.method.clone(), &undo.path, undo.body.as_ref())
                        .await
                    {
                        tracing::warn!("couchbase backup: undo after a failed change: {u}");
                    }
                }
            }
            if i == 0 {
                return Err(e);
            }
            return Err(Error::Query(format!(
                "Se aplicaron los primeros {i} pedidos del script y falló el siguiente: {e}"
            )));
        }
    }
    Ok(())
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

    /// A secret: `None` when the user typed none (keep or leave out).
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

    /// A multi-select's values, trimmed, without empties; `None` when the
    /// field didn't come.
    pub(crate) fn strings(&self, k: &str) -> Option<Vec<String>> {
        match self.get(k)? {
            Value::Array(a) => Some(
                a.iter()
                    .filter_map(|v| match v {
                        Value::String(s) => Some(s.trim().to_string()),
                        Value::Null => None,
                        v => Some(v.to_string()),
                    })
                    .filter(|s| !s.is_empty())
                    .collect(),
            ),
            Value::String(s) if !s.trim().is_empty() => Some(vec![s.trim().to_string()]),
            _ => Some(Vec::new()),
        }
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

/// `HH:MM` or `HH:MM:SS` (seconds at zero: the service plans to the
/// minute) as the service's `HH:MM`.
pub(crate) fn parse_time(v: &str) -> Option<String> {
    let parts: Vec<&str> = v.trim().split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let num = |p: &str| -> Option<u32> {
        (p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse().ok())
            .flatten()
    };
    let h = num(parts[0])?;
    let m = num(parts[1])?;
    let sec = match parts.get(2) {
        Some(p) => num(p)?,
        None => 0,
    };
    (h < 24 && m < 60 && sec == 0).then(|| format!("{h:02}:{m:02}"))
}

/// The service's `HH:MM` as the form's `HH:MM:SS`.
pub(crate) fn form_time(t: &str) -> String {
    match parse_time(t) {
        Some(hm) => format!("{hm}:00"),
        None => t.to_string(),
    }
}
