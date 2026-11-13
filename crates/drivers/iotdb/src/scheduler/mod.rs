//! IoTDB's and TimechoDB's scheduler managed whole through the scheduler
//! contract ([`dbine_driver::Session::scheduler_info`] and the rest), on the
//! tree model the REST API serves:
//!
//! - **Continuous queries** (kind `job`, ids `cq:<id>` as [`crate::jobs`]
//!   lists them): `CREATE CONTINUOUS QUERY … RESAMPLE EVERY … BOUNDARY …
//!   RANGE … TIMEOUT POLICY … BEGIN SELECT … INTO … END` and `DROP CQ`.
//!   IoTDB has no `ALTER` for them: a change drops the query and creates it
//!   again (nothing else is lost: IoTDB keeps no history of their runs), and
//!   when the server refuses the new statement the old one is created again.
//! - **Pipes** (kind `pipe`, and under `job` too, where the jobs list has
//!   them; ids `pipe:<name>`): `CREATE PIPE … WITH SOURCE / PROCESSOR / SINK
//!   (attributes)`, `ALTER PIPE … MODIFY | REPLACE …` in place (1.3 on for
//!   the processor and the sink, 2.0 on for the source too: the pipe keeps
//!   its progress), `START` / `STOP PIPE` as its enabled state and `DROP
//!   PIPE`. What `ALTER` can't change (a rename, the source before 2.0)
//!   drops the pipe and creates it again, which starts it over.
//! - **Pipe plugins** (kind `pipe_plugin`, ids `plugin:<NAME>`): `CREATE
//!   PIPEPLUGIN … AS 'class' USING URI '…'` and `DROP PIPEPLUGIN`; the
//!   built-in ones are read-only.
//!
//! IoTDB has no "run now", no history of runs and no scheduler log, so
//! `run_job_from`, `clear_job_history` and `scheduler_logs` stay
//! unsupported.
//!
//! Values come from the app as untrusted text: names go backquoted
//! ([`ident`]), attribute keys are checked against the charset IoTDB uses,
//! values go as string literals ([`lit`]), and durations, instants, class
//! and plugin names are checked before they reach a statement. The query of
//! a continuous query is the user's own SQL, checked to be one statement.
//! Credentials (attributes that read as one, [`jobs::secret_key`]) never
//! come back in a form and show as [`SECRET_MASK`] in scripts; only the save
//! writes them.

mod cq;
mod pipe;
mod plugin;

use crate::jobs;
use crate::{IotDbSession, Table};
use dbine_driver::{
    Error, FormValues, ObjectForm, Result, SchedulerInfo, SchedulerItem, SchedulerKind,
    SchedulerList, SECRET_MASK,
};
use serde_json::Value;
use std::collections::BTreeMap;

const NO_CQ: &str = "Este usuario no puede crear, cambiar ni borrar consultas continuas (hace falta el privilegio USE_CQ).";
const NO_PIPE: &str = "Este usuario no puede crear, cambiar ni borrar pipes ni plugins de pipe (hace falta el privilegio USE_PIPE).";
const NOT_FOUND: &str = "no existe o este usuario no lo puede ver";

pub(crate) fn bad(message: String) -> Error {
    Error::Query(message)
}

/// Text from the app, short and on one line, for a message.
pub(crate) fn shown(s: &str) -> String {
    comment_text(s.trim()).chars().take(60).collect()
}

/// Text for a `--` comment: one line.
pub(crate) fn comment_text(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

// ---------------------------------------------------------------------------
// Kinds and ids
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// What the jobs list has: continuous queries (new ones) and pipes.
    Job,
    Pipe,
    Plugin,
}

impl Kind {
    fn parse(kind: &str) -> Result<Kind> {
        Ok(match kind.trim() {
            "job" => Kind::Job,
            "pipe" => Kind::Pipe,
            "pipe_plugin" => Kind::Plugin,
            other => {
                return Err(bad(format!(
                    "«{}» no es un tipo de objeto del planificador de IoTDB",
                    shown(other)
                )))
            }
        })
    }
}

/// What an id names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target<'a> {
    Cq(&'a str),
    Pipe(&'a str),
    Plugin(&'a str),
}

pub(crate) fn target(kind: Kind, id: &str) -> Result<Target<'_>> {
    let t = match kind {
        Kind::Job => id
            .strip_prefix("cq:")
            .map(Target::Cq)
            .or_else(|| id.strip_prefix("pipe:").map(Target::Pipe)),
        Kind::Pipe => id.strip_prefix("pipe:").map(Target::Pipe),
        Kind::Plugin => id.strip_prefix("plugin:").map(Target::Plugin),
    };
    t.filter(|t| match t {
        Target::Cq(n) | Target::Pipe(n) | Target::Plugin(n) => !n.is_empty(),
    })
    .ok_or_else(|| {
        bad(format!(
            "«{}» no es un id del planificador de IoTDB",
            shown(id)
        ))
    })
}

// ---------------------------------------------------------------------------
// What the server and the user allow
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ctx {
    /// The server's version (`SHOW VERSION`), `(1, 3, 0)` when unknown.
    pub(crate) version: (u32, u32, u32),
    /// May change continuous queries (USE_CQ, or the check was refused:
    /// the server decides).
    pub(crate) cq: bool,
    /// May change pipes and pipe plugins (USE_PIPE, or unknown).
    pub(crate) pipe: bool,
}

impl Ctx {
    /// `ALTER PIPE` changes this clause in place: the processor and the
    /// sink from 1.3, the source from 2.0.
    pub(crate) fn alters(&self, clause: pipe::Clause) -> bool {
        match clause {
            pipe::Clause::Source => self.version >= (2, 0, 0),
            _ => self.version >= (1, 3, 0),
        }
    }

    fn allowed(&self, t: &Target) -> bool {
        match t {
            Target::Cq(_) => self.cq,
            Target::Pipe(_) | Target::Plugin(_) => self.pipe,
        }
    }
}

/// `1.3.2`, `2.0.5`, TimechoDB's `1.3.4.2`: the first three numbers.
pub(crate) fn version_of(text: &str) -> (u32, u32, u32) {
    let mut n = text
        .split(|c: char| !c.is_ascii_digit())
        .filter(|x| !x.is_empty())
        .map(|x| x.parse::<u32>().unwrap_or(0));
    match (n.next(), n.next()) {
        (Some(a), Some(b)) => (a, b, n.next().unwrap_or(0)),
        _ => (1, 3, 0),
    }
}

fn has(grants: &[dbine_driver::Grant], privilege: &str) -> bool {
    grants.iter().any(|g| {
        g.privilege.eq_ignore_ascii_case(privilege) || g.privilege.eq_ignore_ascii_case("ALL")
    })
}

async fn ctx(s: &IotDbSession) -> Result<Ctx> {
    let version = s
        .query("SHOW VERSION", 10)
        .await?
        .rows
        .first()
        .and_then(|r| r.first())
        .and_then(Value::as_str)
        .map(version_of)
        .unwrap_or((1, 3, 0));
    let (cq, pipe) = match crate::security::grants(s, &s.username).await {
        Ok(g) => (has(&g, "USE_CQ"), has(&g, "USE_PIPE")),
        Err(e @ (Error::Connect(_) | Error::ConnectFailed(_))) => return Err(e),
        Err(e) => {
            tracing::debug!("iotdb scheduler: privileges unknown: {e}");
            (true, true)
        }
    };
    Ok(Ctx { version, cq, pipe })
}

fn info_of(c: &Ctx) -> SchedulerInfo {
    let (q, p) = (c.cq, c.pipe);
    let mut notes = Vec::new();
    if !q {
        notes.push(NO_CQ.to_string());
    }
    if !p {
        notes.push(NO_PIPE.to_string());
    }
    SchedulerInfo {
        kinds: vec![
            SchedulerKind::new("job", "Consultas continuas y pipes", "job").abilities(
                q,
                q || p,
                q || p,
            ),
            SchedulerKind::new("pipe", "Pipes", "pipe").abilities(p, p, p),
            SchedulerKind::new("pipe_plugin", "Plugins de pipe", "plugin de pipe")
                .abilities(p, p, p),
        ],
        logs: false,
        notes,
    }
}

// ---------------------------------------------------------------------------
// Reading the server's objects
// ---------------------------------------------------------------------------

pub(crate) async fn read_cq(s: &IotDbSession, id: &str) -> Result<jobs::Cq> {
    let t = s.query("SHOW CQS", jobs::MAX_JOBS).await?;
    jobs::cqs(&t)
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| bad(format!("La consulta continua «{}» {NOT_FOUND}.", shown(id))))
}

/// The pipe as the server answered (credentials as it shows them).
pub(crate) async fn read_pipe(s: &IotDbSession, name: &str) -> Result<jobs::Pipe> {
    let t = s.query("SHOW PIPES", jobs::MAX_JOBS).await?;
    jobs::raw_pipes(&t)
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| bad(format!("El pipe «{}» {NOT_FOUND}.", shown(name))))
}

pub(crate) async fn plugin_table(s: &IotDbSession) -> Result<Table> {
    s.query("SHOW PIPEPLUGINS", jobs::MAX_JOBS).await
}

pub(crate) async fn read_plugin(s: &IotDbSession, name: &str) -> Result<plugin::Plugin> {
    plugin::plugins(&plugin_table(s).await?)
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| bad(format!("El plugin de pipe «{}» {NOT_FOUND}.", shown(name))))
}

/// `Some(false)` where the item can't be changed although its kind can;
/// `None`: as the kind says.
fn unless(may: bool) -> Option<bool> {
    (!may).then_some(false)
}

/// A continuous query; `may`: the user may change continuous queries (in
/// the "job" kind, which pipes share, the kind's abilities are either's).
fn cq_item(c: &jobs::Cq, may: bool) -> SchedulerItem {
    SchedulerItem {
        id: format!("cq:{}", c.id),
        name: c.id.clone(),
        description: c.query.as_deref().and_then(jobs::every),
        enabled: Some(c.state.eq_ignore_ascii_case("ACTIVE")),
        system: false,
        can_edit: unless(may),
        can_delete: unless(may),
        actions: None,
    }
}

/// A pipe; IoTDB's own (`__…`) are shown only. `START` / `STOP PIPE` is
/// its enabled switch, so the kind has no start or stop action apart.
fn pipe_item(p: &jobs::Pipe, may: bool) -> SchedulerItem {
    let job = jobs::pipe_job(p);
    let may = may && !job.system;
    SchedulerItem {
        id: job.id,
        name: job.name,
        description: job.description,
        enabled: Some(job.enabled),
        system: job.system,
        can_edit: unless(may),
        can_delete: unless(may),
        actions: None,
    }
}

const CQS_DENIED: &str =
    "Este usuario no puede ver las consultas continuas (hace falta el privilegio USE_CQ).";
const PIPES_DENIED: &str =
    "Este usuario no puede ver los pipes (hace falta el privilegio USE_PIPE).";

async fn cq_items(s: &IotDbSession, may: bool, list: &mut SchedulerList) -> Result<()> {
    match s.query("SHOW CQS", jobs::MAX_JOBS).await {
        Ok(t) => list
            .items
            .extend(jobs::cqs(&t).iter().map(|c| cq_item(c, may))),
        Err(e) if jobs::denied(&e) => list.notes.push(CQS_DENIED.into()),
        Err(e) => return Err(e),
    }
    Ok(())
}

async fn pipe_items(s: &IotDbSession, may: bool, list: &mut SchedulerList) -> Result<()> {
    match s.query("SHOW PIPES", jobs::MAX_JOBS).await {
        Ok(t) => list
            .items
            .extend(jobs::pipes(&t).iter().map(|p| pipe_item(p, may))),
        Err(e) if jobs::denied(&e) => list.notes.push(PIPES_DENIED.into()),
        Err(e) => return Err(e),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The contract's calls
// ---------------------------------------------------------------------------

pub(crate) async fn info(s: &mut IotDbSession) -> Result<SchedulerInfo> {
    Ok(info_of(&ctx(s).await?))
}

pub(crate) async fn items(s: &mut IotDbSession, kind: &str) -> Result<SchedulerList> {
    let kind = Kind::parse(kind)?;
    let mut list = SchedulerList::default();
    match kind {
        Kind::Job => {
            // The kind may be changed when either may: each item says
            // whether it can.
            let c = ctx(s).await?;
            cq_items(s, c.cq, &mut list).await?;
            pipe_items(s, c.pipe, &mut list).await?;
        }
        // The kind's abilities are already USE_PIPE's.
        Kind::Pipe => pipe_items(s, true, &mut list).await?,
        Kind::Plugin => match plugin_table(s).await {
            Ok(t) => list.items = plugin::plugins(&t).iter().map(plugin::item).collect(),
            Err(e) if jobs::denied(&e) => list.notes.push(PIPES_DENIED.into()),
            Err(e) => return Err(e),
        },
    }
    Ok(list)
}

pub(crate) async fn form(s: &mut IotDbSession, kind: &str, id: Option<&str>) -> Result<ObjectForm> {
    let kind = Kind::parse(kind)?;
    let c = ctx(s).await?;
    match id {
        None => match kind {
            Kind::Job => Ok(cq::form(&c, None)),
            Kind::Pipe => pipe::form(s, &c, None).await,
            Kind::Plugin => Ok(plugin::form(&c, None)),
        },
        Some(id) => match target(kind, id)? {
            Target::Cq(n) => {
                let cq = read_cq(s, n).await?;
                Ok(cq::form(&c, Some(&cq)))
            }
            Target::Pipe(n) => {
                let p = read_pipe(s, n).await?;
                pipe::form(s, &c, Some(&p)).await
            }
            Target::Plugin(n) => {
                let p = read_plugin(s, n).await?;
                Ok(plugin::form(&c, Some(&p)))
            }
        },
    }
}

async fn plan(
    s: &IotDbSession,
    c: &Ctx,
    kind: Kind,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    match id {
        None => match kind {
            Kind::Job => cq::plan(None, values),
            Kind::Pipe => pipe::plan(c, None, values),
            Kind::Plugin => plugin::plan(None, values),
        },
        Some(id) => match target(kind, id)? {
            Target::Cq(n) => cq::plan(Some(&read_cq(s, n).await?), values),
            Target::Pipe(n) => pipe::plan(c, Some(&read_pipe(s, n).await?), values),
            Target::Plugin(n) => plugin::plan(Some(&read_plugin(s, n).await?), values),
        },
    }
}

pub(crate) async fn script(
    s: &mut IotDbSession,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<String> {
    let kind = Kind::parse(kind)?;
    let c = ctx(s).await?;
    Ok(plan(s, &c, kind, id, values).await?.script())
}

/// Refuses a change the user may not make, before anything is read.
fn check_allowed(c: &Ctx, kind: Kind, id: Option<&str>) -> Result<()> {
    let t = match id {
        Some(id) => target(kind, id)?,
        None => match kind {
            Kind::Job => Target::Cq(""),
            Kind::Pipe => Target::Pipe(""),
            Kind::Plugin => Target::Plugin(""),
        },
    };
    if c.allowed(&t) {
        return Ok(());
    }
    Err(bad(match t {
        Target::Cq(_) => NO_CQ.into(),
        _ => NO_PIPE.into(),
    }))
}

pub(crate) async fn save(
    s: &mut IotDbSession,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Option<String>> {
    let kind = Kind::parse(kind)?;
    let c = ctx(s).await?;
    check_allowed(&c, kind, id)?;
    let plan = plan(s, &c, kind, id, values).await?;
    run(s, &plan).await?;
    Ok(Some(plan.id))
}

async fn delete_plan(s: &IotDbSession, kind: Kind, id: &str) -> Result<Plan> {
    match target(kind, id)? {
        Target::Cq(n) => Ok(cq::drop_plan(&read_cq(s, n).await?)),
        Target::Pipe(n) => pipe::drop_plan(&read_pipe(s, n).await?),
        Target::Plugin(n) => plugin::drop_plan(&read_plugin(s, n).await?),
    }
}

pub(crate) async fn delete_script(s: &mut IotDbSession, kind: &str, id: &str) -> Result<String> {
    let kind = Kind::parse(kind)?;
    Ok(delete_plan(s, kind, id).await?.script())
}

pub(crate) async fn delete(s: &mut IotDbSession, kind: &str, id: &str) -> Result<()> {
    let kind = Kind::parse(kind)?;
    let c = ctx(s).await?;
    check_allowed(&c, kind, Some(id))?;
    let plan = delete_plan(s, kind, id).await?;
    run(s, &plan).await
}

// ---------------------------------------------------------------------------
// Plans: what a change runs
// ---------------------------------------------------------------------------

/// One statement: as the script shows it (credentials masked) and as the
/// save sends it (`Err`: a credential the save doesn't have, and why).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Stmt {
    pub(crate) shown: String,
    pub(crate) real: std::result::Result<String, String>,
    /// The credentials `real` carries: taken out of the server's errors.
    pub(crate) secrets: Vec<String>,
}

impl Stmt {
    pub(crate) fn plain(sql: String) -> Stmt {
        Stmt {
            shown: sql.clone(),
            real: Ok(sql),
            secrets: Vec::new(),
        }
    }
}

/// A step that drops the object and the one that creates it again: when
/// one from `drop + 1` to `create` fails, `restore` puts the old object
/// back.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Recreate {
    pub(crate) drop: usize,
    pub(crate) create: usize,
    pub(crate) restore: Vec<Stmt>,
    /// What the old object is called, for the message.
    pub(crate) what: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    /// The object, for "no changes".
    pub(crate) name: String,
    /// Lines of the script's header comment.
    pub(crate) comments: Vec<String>,
    pub(crate) stmts: Vec<Stmt>,
    /// The object's id after the save.
    pub(crate) id: String,
    pub(crate) recreate: Option<Recreate>,
}

impl Plan {
    pub(crate) fn new(name: &str, id: String) -> Plan {
        Plan {
            name: name.to_string(),
            comments: Vec::new(),
            stmts: Vec::new(),
            id,
            recreate: None,
        }
    }

    pub(crate) fn comment(&mut self, text: impl Into<String>) {
        self.comments.push(text.into());
    }

    pub(crate) fn push(&mut self, st: Stmt) -> usize {
        self.stmts.push(st);
        self.stmts.len() - 1
    }

    pub(crate) fn script(&self) -> String {
        if self.stmts.is_empty() {
            return format!(
                "-- «{}»: no hay cambios que guardar.\n",
                comment_text(&self.name)
            );
        }
        let mut out = String::new();
        for c in &self.comments {
            out.push_str("-- ");
            out.push_str(&comment_text(c));
            out.push('\n');
        }
        for st in &self.stmts {
            out.push_str(&st.shown);
            out.push_str(";\n");
        }
        out
    }
}

/// The first line of a statement, for a message.
fn head(sql: &str) -> String {
    shown(sql.lines().next().unwrap_or(sql))
}

/// Runs the plan's statements in order, one request each (IoTDB has no
/// transactions). A drop and create that fails at the create puts the old
/// object back.
/// An error of the server without the credentials the statement sent (an
/// error may quote the statement).
fn scrub(e: Error, secrets: &[String]) -> Error {
    let text = e.to_string();
    let mut out = text.clone();
    for s in secrets.iter().filter(|s| !s.is_empty()) {
        out = out
            .replace(s.as_str(), SECRET_MASK)
            .replace(&s.replace('\'', "''"), SECRET_MASK);
    }
    if out == text {
        e
    } else {
        Error::Query(out)
    }
}

async fn run(s: &IotDbSession, plan: &Plan) -> Result<()> {
    let real: Vec<&str> = plan
        .stmts
        .iter()
        .map(|st| st.real.as_deref().map_err(|m| bad(m.clone())))
        .collect::<Result<_>>()?;
    for (i, sql) in real.iter().enumerate() {
        let Err(e) = s.non_query(sql).await else {
            continue;
        };
        let e = scrub(e, &plan.stmts[i].secrets);
        let cause = e.to_string();
        if let Some(r) = plan
            .recreate
            .as_ref()
            .filter(|r| i > r.drop && i <= r.create)
        {
            let mut restored = Ok(());
            for st in &r.restore {
                restored = match &st.real {
                    Ok(sql) => s
                        .non_query(sql)
                        .await
                        .map_err(|e| scrub(e, &st.secrets).to_string()),
                    Err(m) => Err(m.clone()),
                };
                if restored.is_err() {
                    break;
                }
            }
            return Err(bad(match restored {
                Ok(()) => format!(
                    "El servidor rechazó la nueva definición ({cause}): se volvió a crear «{}» como estaba.",
                    r.what
                ),
                Err(m) => format!(
                    "El servidor rechazó la nueva definición ({cause}) y tampoco se pudo volver a crear «{}» como estaba ({m}). Su definición era:\n{}",
                    r.what,
                    r.restore.first().map(|st| st.shown.as_str()).unwrap_or("")
                ),
            }));
        }
        if i == 0 {
            return Err(e);
        }
        return Err(bad(format!(
            "El servidor rechazó «{}»: {cause}. Las instrucciones anteriores del script ya se habían aplicado.",
            head(sql)
        )));
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

    /// The value as typed (queries keep their spaces).
    pub(crate) fn raw(&self, k: &str) -> String {
        match self.get(k) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(_) | Value::Object(_)) | None => String::new(),
            Some(v) => v.to_string(),
        }
    }

    /// The value trimmed, `None` when empty.
    pub(crate) fn opt(&self, k: &str) -> Option<String> {
        Some(self.raw(k).trim().to_string()).filter(|t| !t.is_empty())
    }

    /// A secret: `None` when not typed (keep the server's).
    pub(crate) fn secret(&self, k: &str) -> Option<String> {
        Some(self.raw(k)).filter(|t| !t.is_empty())
    }

    pub(crate) fn bool_or(&self, k: &str, default: bool) -> bool {
        match self.get(k) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => true,
                "false" | "0" | "no" | "off" => false,
                _ => default,
            },
            _ => default,
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

/// The name of a new or renamed object: required, on one line.
pub(crate) fn name_field(v: Vals, what: &str) -> Result<String> {
    let n = v
        .opt("name")
        .ok_or_else(|| bad(format!("{what} necesita un nombre.")))?;
    if n.chars().any(char::is_control) || n.chars().count() > 256 {
        return Err(bad(format!(
            "«{}» no es un nombre válido: una línea de hasta 256 caracteres.",
            shown(&n)
        )));
    }
    Ok(n)
}

/// A name for a statement: always backquoted, a backquote doubled.
pub(crate) fn ident(name: &str) -> String {
    jobs::pipe_name(name)
}

/// `v` as an IoTDB string literal: single quotes, a quote doubled. IoTDB's
/// lexer takes a backslash as escaping the next character and keeps both,
/// so a value where an odd run of backslashes meets a quote or the end
/// can't be written: refused (`\\` and `C:\dir\file` go through).
pub(crate) fn lit(v: &str, label: &str) -> Result<String> {
    let mut run = 0usize;
    for ch in v.chars() {
        match ch {
            '\\' => {
                run += 1;
                continue;
            }
            '\'' if run % 2 == 1 => return Err(backslash(label)),
            '\0' => return Err(bad(format!("«{}» tiene un carácter nulo.", shown(label)))),
            _ => {}
        }
        run = 0;
    }
    if run % 2 == 1 {
        return Err(backslash(label));
    }
    Ok(format!("'{}'", v.replace('\'', "''")))
}

fn backslash(label: &str) -> Error {
    bad(format!(
        "«{}»: IoTDB no puede guardar una barra invertida justo antes de una comilla o al final del valor.",
        shown(label)
    ))
}

/// An IoTDB duration (`30s`, `1h30m`, `500ms`, `1mo`), in lowercase.
pub(crate) fn duration(v: &str, label: &str) -> Result<String> {
    let t = v.trim().to_ascii_lowercase();
    let b = t.as_bytes();
    let mut i = 0;
    let mut parts = 0;
    while i < b.len() {
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            break;
        }
        let rest = &t[i..];
        let unit = ["mo", "ms", "us", "ns", "y", "w", "d", "h", "m", "s"]
            .into_iter()
            .find(|u| rest.starts_with(u));
        match unit {
            Some(u) => {
                i += u.len();
                parts += 1;
            }
            None => {
                i = start;
                break;
            }
        }
    }
    if parts == 0 || i != b.len() || t.len() > 64 {
        return Err(bad(format!(
            "«{label}»: «{}» no es una duración de IoTDB (por ejemplo 30s, 5m, 1h30m, 1d).",
            shown(v)
        )));
    }
    Ok(t)
}

/// A point in time as IoTDB reads it: an integer (in the server's
/// precision) or a date and time (`2024-01-01T00:00:00`, `2024/01/01
/// 08:00:00.000+08:00`).
pub(crate) fn instant(v: &str, label: &str) -> Result<String> {
    let t = v.trim();
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let ok = if let Some(n) = t.strip_prefix('-') {
        digits(n)
    } else if digits(t) {
        true
    } else {
        datetime(t)
    };
    if !ok || t.len() > 64 {
        return Err(bad(format!(
            "«{label}»: «{}» no es una fecha y hora (AAAA-MM-DDTHH:MM:SS) ni un instante.",
            shown(v)
        )));
    }
    Ok(t.replacen(' ', "T", 1))
}

fn datetime(t: &str) -> bool {
    let b = t.as_bytes();
    let n = |from: usize, len: usize| {
        b.get(from..from + len)
            .is_some_and(|s| s.iter().all(u8::is_ascii_digit))
    };
    // The date: YYYY-MM-DD, with - / or . between.
    if !(n(0, 4) && n(5, 2) && n(8, 2)) {
        return false;
    }
    let sep = b[4];
    if !matches!(sep, b'-' | b'/' | b'.') || b[7] != sep {
        return false;
    }
    if b.len() == 10 {
        return true;
    }
    // The time: THH:MM:SS[.fraction][+HH:MM].
    if !matches!(b[10], b'T' | b' ') || !(n(11, 2) && b.get(13) == Some(&b':') && n(14, 2)) {
        return false;
    }
    let mut i = 16;
    if b.get(i) == Some(&b':') {
        if !n(i + 1, 2) {
            return false;
        }
        i += 3;
        if b.get(i) == Some(&b'.') {
            let start = i + 1;
            i = start;
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
            if i == start || i - start > 9 {
                return false;
            }
        }
    }
    match b.get(i) {
        None => true,
        Some(b'+' | b'-') => {
            n(i + 1, 2) && b.get(i + 3) == Some(&b':') && n(i + 4, 2) && b.len() == i + 6
        }
        _ => false,
    }
}

/// The mask a script shows for a credential.
pub(crate) fn mask() -> &'static str {
    SECRET_MASK
}

#[cfg(test)]
mod tests;
