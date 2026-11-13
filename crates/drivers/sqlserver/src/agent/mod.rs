//! SQL Server Agent managed whole through the scheduler contract
//! ([`dbine_driver::Session::scheduler_info`] and the rest): what SQL Server
//! Management Studio's "SQL Server Agent" node does. Jobs, shared schedules,
//! operators, alerts, proxies and categories, each with the pages of SSMS's
//! dialog as a designer form, the T-SQL that msdb's documented procedures
//! run for a change, and the Agent's error logs.
//!
//! - Scripts: every save is generated T-SQL, `sp_add_*` / `sp_update_*` /
//!   `sp_delete_*` and the attach, grant and notification procedures, in one
//!   transaction with SSMS's checks (`EXEC @ReturnCode = …; IF (@@ERROR <> 0
//!   OR @ReturnCode <> 0) GOTO QuitWithRollback`). An edit is a diff against
//!   what msdb has now: only what changed, in place. A job is never dropped
//!   and created again (its history would go with it); its steps are
//!   inserted, updated, deleted and renumbered by msdb's own procedures.
//!   No `SET XACT_ABORT ON`: msdb's procedures tell the Agent about every
//!   change (`sp_sqlagent_notify`), and with the Agent stopped that raises
//!   error 22022 inside procedures that still succeed; XACT_ABORT would turn
//!   it into a rollback of every save. A batch-ending error is rolled back
//!   by the driver after the batch.
//! - Warnings: what a script does that the user loses or risks, beyond
//!   the change itself, is a `-- ⚠ …` line under its first one (the app
//!   asks to continue): calls run after the commit, a proxy description
//!   msdb won't empty, schedules deleted or taken off jobs, a job's history,
//!   alerts and notifications left without their job or operator, members
//!   moved to the default category, step logs left with another step.
//! - Values and ids come from the app as untrusted text: ids are parsed (a
//!   job's GUID, the integer ids of the rest) and looked up on the server,
//!   names picked from lists are checked against the server's, and every
//!   text goes into the script as an `N'…'` literal (or, when a line of it
//!   could read as `GO`, a variable built on one line).
//! - Permissions: sysadmin changes anything; a SQLAgent role member creates
//!   jobs and schedules and changes the ones it owns; operators, alerts,
//!   proxies and categories are sysadmin's. Forms come read-only (with a
//!   note) where msdb would refuse the change.
//! - Versions: SQL Server 2008 R2 and later (also through `sqlserver_legacy`)
//!   and Azure SQL Managed Instance (no alerts, proxies or idle-CPU
//!   schedules there; its subsystems as the Agent lists them). Express has
//!   msdb but no Agent: everything shows, nothing changes. Azure SQL
//!   Database, Synapse, SQL database in Fabric and Babelfish have no Agent.

mod alert;
mod category;
mod choices;
mod job;
mod logs;
mod operator;
mod proxy;
mod schedule;

use crate::create_db::literal;
use crate::jobs::{self, first_with, int, text, Probe, ScriptWriter};
use crate::{comment_text, err, is_desync, SqlServerSession};
use dbine_driver::{
    Error, FormValues, ObjectForm, Result, SchedulerAction, SchedulerInfo, SchedulerKind,
    SchedulerList,
};
use futures::TryStreamExt;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use tiberius::{MessageItem, Row, Uuid};

pub(crate) use job::{clear_history, run_from};
pub(crate) use logs::logs;

// ---------------------------------------------------------------------------
// Notes and messages
// ---------------------------------------------------------------------------

const EXPRESS_READ_ONLY: &str = "SQL Server Express no tiene el Agente SQL Server: sus objetos se pueden ver, pero no crear ni cambiar.";
const ADMIN_ONLY: &str = "Solo los miembros de sysadmin pueden crear, cambiar o borrar operadores, alertas, proxies y categorías.";
const ALERTS_HIDDEN: &str =
    "Las alertas solo las ven los miembros de SQLAgentOperatorRole o de sysadmin.";
const MANAGED_INSTANCE: &str =
    "Azure SQL Managed Instance no admite alertas, proxies ni programaciones de CPU inactiva del Agente SQL Server.";
const SYSADMIN_ONLY_FORM: &str = "Solo un miembro de sysadmin puede cambiar este objeto.";
/// What the script raises when it rolls back after a refused step, so a
/// failure without a message of its own still reads as one.
const ROLLED_BACK: &str =
    "No se guardó ningún cambio: el servidor rechazó una de las instrucciones del script.";
/// What the script raises when a call it makes after the commit fails.
const AFTER_COMMIT: &str = "Se guardaron los cambios, salvo los que el script hace después de confirmarlos: el servidor rechazó uno.";
/// The warning of a script with calls after the commit: not atomic.
const AFTER_COMMIT_WARNING: &str = "Parte del script se ejecuta después de confirmar el resto, porque msdb no puede hacerlo dentro de una transacción: si esa parte falla, lo anterior queda guardado igual.";

/// The Agent isn't running, so msdb couldn't tell it about the change (it
/// is made anyway).
const AGENT_NOT_NOTIFIED: u32 = 22022;

fn bad(message: String) -> Error {
    Error::Query(message)
}

/// A `sysname` parameter's length (names of jobs, steps, schedules…).
pub(crate) const SYSNAME: usize = 128;

/// A text for a procedure parameter of `max` characters: msdb's procedures
/// would cut a longer one without a word (and a name cut short is then not
/// found by it), so it is refused here. Counted as `nvarchar` counts:
/// UTF-16 units.
pub(crate) fn fits(label: &str, value: &str, max: usize) -> Result<()> {
    let n = value.encode_utf16().count();
    if n > max {
        return Err(bad(format!(
            "«{label}» admite como máximo {max} caracteres (tiene {n})."
        )));
    }
    Ok(())
}

/// An item's own ability ([`dbine_driver::SchedulerItem::can_edit`],
/// `can_delete`): `Some(false)` where its kind allows the change but msdb
/// would refuse it on this one (another owner's, a system category, a
/// master server's job), else `None` (as its kind says), so the tree
/// offers only what the server takes.
pub(crate) fn unless(kind_can: bool, this_can: bool) -> Option<bool> {
    (kind_can && !this_can).then_some(false)
}

/// Text from the app, short and on one line, for a message.
fn shown(s: &str) -> String {
    comment_text(s.trim()).chars().take(60).collect()
}

/// Names for a warning: «a», «b».
pub(crate) fn quoted(names: &[String]) -> String {
    names
        .iter()
        .map(|n| format!("«{n}»"))
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// Kinds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Job,
    Schedule,
    Operator,
    Alert,
    Proxy,
    Category,
}

impl Kind {
    fn parse(kind: &str) -> Result<Kind> {
        Ok(match kind.trim() {
            "job" => Kind::Job,
            "schedule" => Kind::Schedule,
            "operator" => Kind::Operator,
            "alert" => Kind::Alert,
            "proxy" => Kind::Proxy,
            "category" => Kind::Category,
            other => {
                return Err(bad(format!(
                    "«{}» no es un tipo de objeto del Agente SQL Server",
                    shown(other)
                )))
            }
        })
    }
}

/// An integer id (schedule, operator, alert, proxy, category), refused
/// before it reaches the server when it is anything else.
fn int_id(kind: Kind, id: &str) -> Result<i64> {
    let t = id.trim();
    let n = (!t.is_empty() && t.len() <= 10 && t.bytes().all(|b| b.is_ascii_digit()))
        .then(|| t.parse::<i64>().ok())
        .flatten()
        .filter(|n| *n <= i64::from(i32::MAX));
    n.ok_or_else(|| {
        let s = shown(t);
        bad(match kind {
            Kind::Job => format!("«{s}» no es un id de job del Agente SQL Server"),
            Kind::Schedule => format!("«{s}» no es un id de programación del Agente SQL Server"),
            Kind::Operator => format!("«{s}» no es un id de operador del Agente SQL Server"),
            Kind::Alert => format!("«{s}» no es un id de alerta del Agente SQL Server"),
            Kind::Proxy => format!("«{s}» no es un id de proxy del Agente SQL Server"),
            Kind::Category => format!("«{s}» no es un id de categoría del Agente SQL Server"),
        })
    })
}

// ---------------------------------------------------------------------------
// What the server and the login allow
// ---------------------------------------------------------------------------

/// The jobs probe and what the rest of the Agent needs: the error log's
/// permissions (VIEW ANY ERROR LOG only asked from SQL Server 2022, the
/// first that knows it), whether alerts are visible, whether jobs start
/// and their history can be purged, the instance's name, whether this is a
/// master server.
const EXTRA_SQL: &str = "SET NOCOUNT ON;
SELECT CAST(COALESCE(IS_SRVROLEMEMBER('securityadmin'), 0) AS int) AS dbine_secadmin,
       CAST(COALESCE(HAS_PERMS_BY_NAME(NULL, NULL, N'VIEW SERVER STATE'), 0) AS int) AS dbine_vss,
       CAST(COALESCE(CASE WHEN CAST(SERVERPROPERTY('ProductMajorVersion') AS int) >= 16
                          THEN HAS_PERMS_BY_NAME(NULL, NULL, N'VIEW ANY ERROR LOG') END, 0) AS int) AS dbine_errorlog,
       CAST(COALESCE(HAS_PERMS_BY_NAME(N'msdb.dbo.sp_help_alert', N'OBJECT', N'EXECUTE'), 0) AS int) AS dbine_alerts,
       CAST(COALESCE(HAS_PERMS_BY_NAME(N'msdb.dbo.sp_start_job', N'OBJECT', N'EXECUTE'), 0) AS int) AS dbine_start,
       CAST(COALESCE(HAS_PERMS_BY_NAME(N'msdb.dbo.sp_purge_jobhistory', N'OBJECT', N'EXECUTE'), 0) AS int) AS dbine_purge,
       CAST(SERVERPROPERTY('InstanceName') AS nvarchar(128)) AS dbine_instance;
IF HAS_PERMS_BY_NAME(N'msdb.dbo.systargetservers', N'OBJECT', N'SELECT') = 1
    SELECT COUNT(*) AS dbine_targets FROM msdb.dbo.systargetservers;";

#[derive(Debug, Clone, Default)]
pub(crate) struct Ctx {
    pub(crate) p: Probe,
    /// May read the error logs on any version (`sp_readerrorlog`).
    securityadmin: bool,
    /// VIEW SERVER STATE: the error logs from SQL Server 2022, performance
    /// counters.
    pub(crate) server_state: bool,
    /// VIEW ANY ERROR LOG: the error logs from SQL Server 2022.
    pub(crate) error_log: bool,
    /// May run `sp_help_alert` (SQLAgentOperatorRole, sysadmin).
    pub(crate) alerts_visible: bool,
    /// May run `sp_start_job` (every SQLAgent role, sysadmin): a job
    /// starts at one of its steps.
    pub(crate) start_jobs: bool,
    /// May run `sp_purge_jobhistory`: msdb grants it to
    /// SQLAgentOperatorRole only (and sysadmin), not even for the jobs a
    /// SQLAgentUserRole member owns, unless someone granted its EXECUTE.
    /// Whose history it then purges: [`job::purges_history`].
    pub(crate) purge_history: bool,
    /// This is a master server (MSX) with target servers enlisted.
    pub(crate) msx: bool,
    /// A named instance's name (`SERVERPROPERTY('InstanceName')`); `None`
    /// for the default instance.
    pub(crate) instance: Option<String>,
}

impl Ctx {
    /// msdb may be changed here (Express has no Agent).
    pub(crate) fn writable(&self) -> bool {
        self.p.edition != 4
    }

    /// sysadmin on a server where things may change.
    pub(crate) fn admin(&self) -> bool {
        self.p.sysadmin && self.writable()
    }

    /// Azure SQL Managed Instance.
    pub(crate) fn managed(&self) -> bool {
        self.p.edition == 8
    }

    /// "Start Job at Step" needs the Agent running (`sp_start_job` is
    /// refused otherwise) and the procedure's EXECUTE; Express has no
    /// Agent.
    pub(crate) fn run_from_step(&self) -> bool {
        self.p.agent_runs() && self.start_jobs
    }

    fn has(&self, kind: Kind) -> bool {
        match kind {
            Kind::Alert => !self.managed() && self.alerts_visible,
            Kind::Proxy => !self.managed(),
            _ => true,
        }
    }

    /// The Agent's error log is readable: securityadmin, or from SQL Server
    /// 2022 VIEW SERVER STATE or VIEW ANY ERROR LOG (what `sp_readerrorlog`
    /// and `sp_enumerrorlogs` check).
    fn logs(&self) -> bool {
        self.p.edition != 4
            && (self.securityadmin || (self.p.major >= 16 && (self.server_state || self.error_log)))
    }

    /// Why a kind this server or login doesn't have is refused: Managed
    /// Instance has no alerts or proxies; elsewhere only alerts can be
    /// missing, for a login without SQLAgentOperatorRole or sysadmin.
    fn missing_kind(&self) -> Error {
        if self.managed() {
            Error::Unsupported(MANAGED_INSTANCE.into())
        } else {
            Error::Query(ALERTS_HIDDEN.into())
        }
    }

    /// The default namespace of a WMI alert: the instance's own
    /// (`MSSQLSERVER` for the default instance), as SSMS proposes it.
    pub(crate) fn wmi_namespace(&self) -> String {
        format!(
            r"\\.\root\Microsoft\SqlServer\ServerEvents\{}",
            self.instance
                .as_deref()
                .map(str::trim)
                .filter(|i| !i.is_empty())
                .unwrap_or("MSSQLSERVER")
        )
    }

    /// The login is this owner (msdb compares logins case-insensitively).
    pub(crate) fn is(&self, owner: Option<&str>) -> bool {
        owner.is_some_and(|o| !self.p.login.is_empty() && o.eq_ignore_ascii_case(&self.p.login))
    }
}

async fn ctx(s: &mut SqlServerSession) -> Result<Ctx> {
    jobs::check_variant(s)?;
    let p = jobs::probe(s).await?;
    if p.missing().is_some() {
        return Ok(Ctx {
            p,
            ..Ctx::default()
        });
    }
    let sets = jobs::sets(s, EXTRA_SQL, &[], None).await?;
    let mut c = Ctx {
        p,
        ..Ctx::default()
    };
    if let Some(r) = first_with(&sets, "dbine_secadmin").and_then(|s| s.rows.first()) {
        c.securityadmin = int(r, "dbine_secadmin") == Some(1);
        c.server_state = int(r, "dbine_vss") == Some(1);
        c.error_log = int(r, "dbine_errorlog") == Some(1);
        c.alerts_visible = int(r, "dbine_alerts") == Some(1);
        c.start_jobs = int(r, "dbine_start") == Some(1);
        c.purge_history = int(r, "dbine_purge") == Some(1);
        c.instance = text(r, "dbine_instance");
    }
    c.msx = first_with(&sets, "dbine_targets")
        .and_then(|s| s.rows.first())
        .and_then(|r| int(r, "dbine_targets"))
        .unwrap_or(0)
        > 0;
    Ok(c)
}

/// The context, refusing a server without an Agent to manage.
async fn ready(s: &mut SqlServerSession) -> Result<Ctx> {
    let c = ctx(s).await?;
    match c.p.missing() {
        Some(note) if c.p.no_agent() => Err(Error::Unsupported(note.into())),
        Some(note) => Err(Error::Query(note.into())),
        None => Ok(c),
    }
}

/// The context, for a change: refused where msdb may not change.
async fn ready_to_change(s: &mut SqlServerSession, kind: Kind) -> Result<Ctx> {
    let c = ready(s).await?;
    if !c.writable() {
        return Err(Error::Query(EXPRESS_READ_ONLY.into()));
    }
    if !c.has(kind) {
        return Err(c.missing_kind());
    }
    Ok(c)
}

/// The kinds, in the order of SSMS's tree, and the notes.
fn info_of(c: &Ctx) -> SchedulerInfo {
    if let Some(note) = c.p.missing() {
        return SchedulerInfo {
            kinds: Vec::new(),
            logs: false,
            notes: vec![note.to_string()],
        };
    }
    let w = c.writable();
    let a = c.admin();
    let mut kinds = vec![
        // "Start Job at Step" needs the Agent running (`sp_start_job` is
        // refused otherwise); the history is msdb's to purge with or without
        // it. Express has neither. Each needs its procedure's EXECUTE.
        SchedulerKind::new("job", "Jobs", "job")
            .abilities(w, w, w)
            .run_from_step(c.run_from_step())
            .clear_history(w && c.purge_history),
        SchedulerKind::new("schedule", "Programaciones", "programación").abilities(w, w, w),
        SchedulerKind::new("operator", "Operadores", "operador").abilities(a, a, a),
    ];
    if c.has(Kind::Alert) {
        let mut k = SchedulerKind::new("alert", "Alertas", "alerta").abilities(a, a, a);
        if a {
            k = k
                .action(SchedulerAction::new(
                    "reset_count",
                    "Restablecer el recuento",
                ))
                .action(SchedulerAction::new("enable", "Habilitar"))
                .action(SchedulerAction::new("disable", "Deshabilitar"));
        }
        kinds.push(k);
    }
    if c.has(Kind::Proxy) {
        kinds.push(SchedulerKind::new("proxy", "Proxies", "proxy").abilities(a, a, a));
    }
    kinds.push(SchedulerKind::new("category", "Categorías", "categoría").abilities(a, a, a));
    let mut notes = c.p.notes();
    if !w {
        notes.push(EXPRESS_READ_ONLY.to_string());
    } else if !c.p.sysadmin {
        notes.push(ADMIN_ONLY.to_string());
    }
    if c.managed() {
        notes.push(MANAGED_INSTANCE.to_string());
    } else if !c.alerts_visible {
        notes.push(ALERTS_HIDDEN.to_string());
    }
    SchedulerInfo {
        kinds,
        logs: c.logs(),
        notes,
    }
}

// ---------------------------------------------------------------------------
// The contract's calls
// ---------------------------------------------------------------------------

pub(crate) async fn info(s: &mut SqlServerSession) -> Result<SchedulerInfo> {
    let c = ctx(s).await?;
    Ok(info_of(&c))
}

pub(crate) async fn items(s: &mut SqlServerSession, kind: &str) -> Result<SchedulerList> {
    let kind = Kind::parse(kind)?;
    let c = ready(s).await?;
    if !c.has(kind) {
        return Ok(SchedulerList {
            items: Vec::new(),
            notes: vec![c.missing_kind().to_string()],
        });
    }
    match kind {
        Kind::Job => job::items(s, &c).await,
        Kind::Schedule => schedule::items(s, &c).await,
        Kind::Operator => operator::items(s, &c).await,
        Kind::Alert => alert::items(s, &c).await,
        Kind::Proxy => proxy::items(s, &c).await,
        Kind::Category => category::items(s, &c).await,
    }
}

pub(crate) async fn form(
    s: &mut SqlServerSession,
    kind: &str,
    id: Option<&str>,
) -> Result<ObjectForm> {
    let kind = Kind::parse(kind)?;
    let c = ready(s).await?;
    if !c.has(kind) {
        return Err(c.missing_kind());
    }
    let mut f = match kind {
        Kind::Job => job::form(s, &c, id).await?,
        Kind::Schedule => schedule::form(s, &c, id).await?,
        Kind::Operator => operator::form(s, &c, id).await?,
        Kind::Alert => alert::form(s, &c, id).await?,
        Kind::Proxy => proxy::form(s, &c, id).await?,
        Kind::Category => category::form(s, &c, id).await?,
    };
    if !c.writable() {
        f.read_only = true;
        f.notes.insert(0, EXPRESS_READ_ONLY.to_string());
    }
    Ok(f)
}

/// What a change writes: the script, whether it changes anything, and how
/// the saved object's id is known afterwards.
pub(crate) struct Plan {
    pub(crate) script: String,
    /// Nothing differs from what the server has.
    pub(crate) empty: bool,
    /// The object's id when it already exists.
    pub(crate) id: Option<String>,
    /// A new object: the script's result column with its id.
    pub(crate) output: Option<&'static str>,
    /// A new object found by name afterwards: a query with `@P1` and its
    /// value, answering one row with an `id` column.
    pub(crate) lookup: Option<(&'static str, String)>,
}

impl Plan {
    fn nothing(name: &str, id: String) -> Plan {
        Plan {
            script: format!("-- «{}»: no hay cambios que guardar.\n", comment_text(name)),
            empty: true,
            id: Some(id),
            output: None,
            lookup: None,
        }
    }

    /// Nothing to save although the form changed something msdb won't
    /// take: `why` is a warning, so the app still shows it.
    fn nothing_but(name: &str, id: String, why: &str) -> Plan {
        let mut p = Plan::nothing(name, id);
        p.script.push_str(&warning_line(why));
        p
    }
}

/// A consequence the user must accept, one whole Spanish sentence: a
/// `-- ⚠ …` line, which the app lists above the script before asking to
/// continue.
fn warning_line(text: &str) -> String {
    format!("-- ⚠ {}\n", comment_text(text))
}

async fn plan(
    s: &mut SqlServerSession,
    c: &Ctx,
    kind: Kind,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    match kind {
        Kind::Job => job::plan(s, c, id, values).await,
        Kind::Schedule => schedule::plan(s, c, id, values).await,
        Kind::Operator => operator::plan(s, c, id, values).await,
        Kind::Alert => alert::plan(s, c, id, values).await,
        Kind::Proxy => proxy::plan(s, c, id, values).await,
        Kind::Category => category::plan(s, c, id, values).await,
    }
}

pub(crate) async fn script(
    s: &mut SqlServerSession,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<String> {
    let kind = Kind::parse(kind)?;
    let c = ready(s).await?;
    if !c.has(kind) {
        return Err(c.missing_kind());
    }
    Ok(plan(s, &c, kind, id, values).await?.script)
}

pub(crate) async fn save(
    s: &mut SqlServerSession,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Option<String>> {
    let kind = Kind::parse(kind)?;
    let c = ready_to_change(s, kind).await?;
    let plan = plan(s, &c, kind, id, values).await?;
    if plan.empty {
        return Ok(plan.id);
    }
    let ran = run_script(s, &plan.script).await?;
    if let Some(column) = plan.output {
        return Ok(ran.values.get(column).cloned().or(plan.id));
    }
    if let Some((sql, name)) = &plan.lookup {
        let sets = jobs::sets(s, sql, &[name.as_str()], None).await?;
        let found = sets
            .iter()
            .flat_map(|set| set.rows.iter())
            .find_map(|r| cell(r, "id"));
        return Ok(found.or(plan.id));
    }
    Ok(plan.id)
}

pub(crate) async fn delete_script(
    s: &mut SqlServerSession,
    kind: &str,
    id: &str,
) -> Result<String> {
    let kind = Kind::parse(kind)?;
    let c = ready(s).await?;
    if !c.has(kind) {
        return Err(c.missing_kind());
    }
    delete_plan(s, &c, kind, id).await
}

async fn delete_plan(s: &mut SqlServerSession, c: &Ctx, kind: Kind, id: &str) -> Result<String> {
    match kind {
        Kind::Job => job::delete_script(s, c, id).await,
        Kind::Schedule => schedule::delete_script(s, c, id).await,
        Kind::Operator => operator::delete_script(s, c, id).await,
        Kind::Alert => alert::delete_script(s, c, id).await,
        Kind::Proxy => proxy::delete_script(s, c, id).await,
        Kind::Category => category::delete_script(s, c, id).await,
    }
}

pub(crate) async fn delete(s: &mut SqlServerSession, kind: &str, id: &str) -> Result<()> {
    let kind = Kind::parse(kind)?;
    let c = ready_to_change(s, kind).await?;
    let script = delete_plan(s, &c, kind, id).await?;
    run_script(s, &script).await.map(|_| ())
}

/// An action key the kind doesn't offer (only alerts have actions here:
/// a job's start, stop and enable are the jobs list's).
fn no_action(action: &str) -> Error {
    bad(format!(
        "«{}» no es una acción del Agente SQL Server para este objeto.",
        shown(action)
    ))
}

pub(crate) async fn action_script(
    s: &mut SqlServerSession,
    kind: &str,
    id: &str,
    action: &str,
) -> Result<String> {
    let kind = Kind::parse(kind)?;
    let c = ready(s).await?;
    if !c.has(kind) {
        return Err(c.missing_kind());
    }
    action_plan(s, &c, kind, id, action).await
}

async fn action_plan(
    s: &mut SqlServerSession,
    c: &Ctx,
    kind: Kind,
    id: &str,
    action: &str,
) -> Result<String> {
    match kind {
        Kind::Alert => alert::action_script(s, c, id, action).await,
        _ => Err(no_action(action)),
    }
}

pub(crate) async fn action(
    s: &mut SqlServerSession,
    kind: &str,
    id: &str,
    action: &str,
) -> Result<()> {
    let kind = Kind::parse(kind)?;
    let c = ready_to_change(s, kind).await?;
    let script = action_plan(s, &c, kind, id, action).await?;
    run_script(s, &script).await.map(|_| ())
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

    /// The value as typed (commands keep their spaces).
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

    pub(crate) fn bool(&self, k: &str) -> bool {
        match self.get(k) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            Some(Value::String(s)) => {
                matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "true" | "1" | "yes" | "on"
                )
            }
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

    /// A number (decimals allowed), as text the server reads.
    pub(crate) fn number(&self, k: &str, label: &str) -> Result<Option<String>> {
        let v = match self.get(k) {
            None => return Ok(None),
            Some(Value::String(s)) if s.trim().is_empty() => return Ok(None),
            Some(Value::Number(n)) => n.as_f64(),
            Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
            Some(_) => None,
        };
        match v.filter(|f| f.is_finite()) {
            Some(f) => Ok(Some(number_text(f))),
            None => Err(bad(format!("«{label}» tiene que ser un número."))),
        }
    }

    /// A list of values (a multi-select's), trimmed, without empties.
    pub(crate) fn strings(&self, k: &str) -> Vec<String> {
        match self.get(k) {
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
        }
    }
}

/// A number as the server writes it back (`100`, `0.5`).
pub(crate) fn number_text(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        format!("{f}")
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

/// `HH:MM[:SS]` as `hhmmss`.
pub(crate) fn parse_time(v: &str) -> Option<i64> {
    let parts: Vec<&str> = v.trim().split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let num = |p: &str| -> Option<i64> {
        (!p.is_empty() && p.len() <= 2 && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse().ok())
            .flatten()
    };
    let h = num(parts[0])?;
    let m = num(parts[1])?;
    let s = match parts.get(2) {
        Some(p) => num(p)?,
        None => 0,
    };
    (h < 24 && m < 60 && s < 60).then_some(h * 10000 + m * 100 + s)
}

/// `YYYY-MM-DD` (or the date of an RFC 3339 time) as `yyyymmdd`.
pub(crate) fn parse_date(v: &str) -> Option<i64> {
    let t = v.trim();
    let d = t.get(..10)?;
    if t.len() > 10 && !t[10..].starts_with('T') && !t[10..].starts_with(' ') {
        return None;
    }
    let date = chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()?;
    let n = d.replace('-', "").parse::<i64>().ok()?;
    (date.format("%Y-%m-%d").to_string() == d).then_some(n)
}

pub(crate) fn time_text(t: i64) -> String {
    format!("{:02}:{:02}:{:02}", t / 10000, t / 100 % 100, t % 100)
}

pub(crate) fn date_text(d: i64) -> String {
    jobs::day(d)
}

/// A field's time, checked.
pub(crate) fn time_field(v: Vals, k: &str, label: &str, default: i64) -> Result<i64> {
    match v.opt(k) {
        None => Ok(default),
        Some(t) => parse_time(&t).ok_or_else(|| {
            bad(format!(
                "«{label}»: «{}» no es una hora válida (HH:MM:SS).",
                shown(&t)
            ))
        }),
    }
}

/// A field's date, checked (msdb takes 1990-01-01 to 9999-12-31).
pub(crate) fn date_field(v: Vals, k: &str, label: &str) -> Result<Option<i64>> {
    match v.opt(k) {
        None => Ok(None),
        Some(t) => match parse_date(&t) {
            Some(d) if (19900101..=99991231).contains(&d) => Ok(Some(d)),
            _ => Err(bad(format!(
                "«{label}»: «{}» no es una fecha válida (AAAA-MM-DD, desde 1990-01-01).",
                shown(&t)
            ))),
        },
    }
}

/// A name the user picked from a list of the server's: it has to be one of
/// them (any case). Returns the server's spelling.
pub(crate) fn pick<'a>(
    name: &str,
    choices: &'a [(String, String)],
    missing: impl FnOnce(&str) -> String,
) -> Result<&'a str> {
    choices
        .iter()
        .find(|(v, _)| v == name)
        .or_else(|| choices.iter().find(|(v, _)| v.eq_ignore_ascii_case(name)))
        .map(|(v, _)| v.as_str())
        .ok_or_else(|| bad(missing(&shown(name))))
}

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

/// A procedure's argument.
#[derive(Debug, Clone)]
pub(crate) enum A {
    /// Text: an `N'…'` literal (or a variable when a line reads as `GO`).
    T(String),
    /// A whole number.
    I(i64),
    /// Already T-SQL (a variable, `@jobId OUTPUT`, a checked GUID literal).
    R(String),
}

/// A script in SSMS's shape: one transaction, each call checked, a rollback
/// on the first failure.
pub(crate) struct Script {
    w: ScriptWriter,
    calls: usize,
    /// Calls msdb can't make inside a transaction, run after the commit.
    after: String,
    /// What the user must accept before it runs (`-- ⚠ …` lines, under
    /// the script's first line): the app lists them and asks to continue.
    warnings: Vec<String>,
}

impl Script {
    /// `comment`: what the script does (one line). `declares`: the
    /// variables beyond `@ReturnCode` (`@jobId uniqueidentifier`…).
    pub(crate) fn new(comment: &str, declares: &[&str]) -> Script {
        let mut w = ScriptWriter {
            out: String::new(),
            vars: 0,
            nested: false,
        };
        let mut vars = vec!["@ReturnCode int"];
        vars.extend_from_slice(declares);
        let _ = writeln!(
            w.out,
            "-- {}\nSET NOCOUNT ON;\nBEGIN TRANSACTION;\nDECLARE {};\nSET @ReturnCode = 0;",
            comment_text(comment),
            vars.join(", ")
        );
        Script {
            w,
            calls: 0,
            after: String::new(),
            warnings: Vec::new(),
        }
    }

    /// A consequence the user must accept (something lost or at risk),
    /// one whole Spanish sentence: a `-- ⚠ …` line under the script's
    /// first one.
    pub(crate) fn warn(&mut self, text: &str) {
        self.warnings.push(warning_line(text));
    }

    /// `EXEC @ReturnCode = proc @a = x, …;` and the check.
    pub(crate) fn call(&mut self, proc_name: &str, args: Vec<(&str, A)>) {
        let rendered: Vec<(&str, String)> = args
            .into_iter()
            .map(|(k, a)| {
                let v = match a {
                    A::T(t) => self.w.text(&t),
                    A::I(n) => n.to_string(),
                    A::R(r) => r,
                };
                (k, v)
            })
            .collect();
        self.w.exec(proc_name, &rendered);
        self.calls += 1;
    }

    /// A call made after the commit: msdb's procedures that impersonate a
    /// principal inside a TRY/CATCH (`sp_revoke_login_from_proxy`,
    /// `sp_grant_login_to_proxy` for a Windows login) doom an open
    /// transaction when the principal can't be impersonated (a role, a
    /// group). Its failure can't undo the rest.
    pub(crate) fn call_after_commit(&mut self, proc_name: &str, args: Vec<(&str, A)>) {
        let start = self.w.out.len();
        self.call(proc_name, args);
        let text = self.w.out.split_off(start);
        self.after
            .push_str(&text.replace("GOTO QuitWithRollback;", "GOTO QuitAfterCommit;"));
    }

    /// A line as is (a comment, `SET @x = …`).
    pub(crate) fn line(&mut self, line: &str) {
        self.w.out.push_str(line);
        self.w.out.push('\n');
    }

    pub(crate) fn calls(&self) -> usize {
        self.calls
    }

    /// The commit, `tail` after it (the new object's id), and the
    /// rollback.
    pub(crate) fn finish(mut self, tail: Option<&str>) -> String {
        if !self.after.is_empty() {
            self.warn(AFTER_COMMIT_WARNING);
        }
        let head = self.w.out.find('\n').map_or(self.w.out.len(), |i| i + 1);
        self.w.out.insert_str(head, &self.warnings.concat());
        self.w.out.push_str("COMMIT TRANSACTION;\n");
        if let Some(t) = tail {
            self.w.out.push_str(t);
            self.w.out.push('\n');
        }
        let after = !self.after.is_empty();
        if after {
            self.w.out.push_str(
                "-- msdb can't make these inside a transaction (they impersonate the principal):\n",
            );
            self.w.out.push_str(&self.after);
        }
        let _ = write!(
            self.w.out,
            "GOTO EndSave;\nQuitWithRollback:\n    IF (@@TRANCOUNT > 0) ROLLBACK TRANSACTION;\n    RAISERROR ({}, 16, 1);\n",
            literal(ROLLED_BACK)
        );
        if after {
            let _ = write!(
                self.w.out,
                "    GOTO EndSave;\nQuitAfterCommit:\n    RAISERROR ({}, 16, 1);\n",
                literal(AFTER_COMMIT)
            );
        }
        self.w.out.push_str("EndSave:\n");
        self.w.out
    }
}

/// A checked GUID as a literal.
pub(crate) fn guid_literal(id: &str) -> String {
    format!("N'{id}'")
}

// ---------------------------------------------------------------------------
// Running a script
// ---------------------------------------------------------------------------

/// What a script answered: the first value of each column it returned.
#[derive(Debug, Default)]
pub(crate) struct Ran {
    pub(crate) values: HashMap<String, String>,
}

/// A cell as text: text, a number or a GUID.
fn cell_at(r: &Row, i: usize) -> Option<String> {
    if let Ok(Some(v)) = r.try_get::<&str, _>(i) {
        return Some(v.to_string());
    }
    if let Ok(Some(v)) = r.try_get::<Uuid, _>(i) {
        return Some(v.to_string());
    }
    if let Ok(Some(v)) = r.try_get::<i32, _>(i) {
        return Some(v.to_string());
    }
    if let Ok(Some(v)) = r.try_get::<i64, _>(i) {
        return Some(v.to_string());
    }
    if let Ok(Some(v)) = r.try_get::<i16, _>(i) {
        return Some(v.to_string());
    }
    if let Ok(Some(v)) = r.try_get::<u8, _>(i) {
        return Some(v.to_string());
    }
    None
}

fn cell(r: &Row, column: &str) -> Option<String> {
    let i = r.columns().iter().position(|c| c.name() == column)?;
    cell_at(r, i)
}

/// Runs a script the way a query tab would (one batch), outside any
/// transaction of the session, and tells a committed change from a
/// refused one by the script's own rollback and by what stays open: every
/// error the server sent is read (the first one isn't always the cause).
async fn run_script(s: &mut SqlServerSession, script: &str) -> Result<Ran> {
    let open = trancount(s).await?;
    if open > 0 {
        return Err(Error::State(jobs::OPEN_TRANSACTION.into()));
    }
    let manual = !s.autocommit;
    if manual {
        s.batch("SET IMPLICIT_TRANSACTIONS OFF").await?;
    }
    let read = read_script(s, script).await;
    let left_open = match &read {
        Err(Error::Connect(_)) => 0,
        _ => trancount(s).await.unwrap_or(0),
    };
    if left_open > 0 {
        if let Err(e) = s.batch("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await {
            tracing::debug!("sqlserver agent: could not roll back: {e}");
        }
    }
    if manual {
        if let Err(e) = s.batch("SET IMPLICIT_TRANSACTIONS ON").await {
            tracing::debug!("sqlserver agent: could not restore implicit transactions: {e}");
        }
    }
    let (errors, ran) = read?;
    outcome(errors, ran, left_open > 0)
}

async fn trancount(s: &mut SqlServerSession) -> Result<i64> {
    Ok(s.rows("SELECT CAST(@@TRANCOUNT AS int) AS n", &[])
        .await?
        .first()
        .and_then(|r| int(r, "n"))
        .unwrap_or(0))
}

async fn read_script(s: &mut SqlServerSession, script: &str) -> Result<(Vec<(u32, String)>, Ran)> {
    let mut errors = Vec::new();
    let mut ran = Ran::default();
    let broken = {
        let sent = s.client.simple_query_messages(script).await;
        match sent {
            Err(e) => Some(e),
            Ok(mut stream) => {
                let mut columns: Vec<String> = Vec::new();
                let mut broken = None;
                loop {
                    match stream.try_next().await {
                        Ok(None) => break,
                        Err(e) => {
                            broken = Some(e);
                            break;
                        }
                        Ok(Some(MessageItem::Metadata(m))) => {
                            columns = m.columns().iter().map(|c| c.name().to_string()).collect();
                        }
                        Ok(Some(MessageItem::Row(r))) => {
                            for (i, c) in columns.iter().enumerate() {
                                if let Some(v) = cell_at(&r, i) {
                                    ran.values.entry(c.clone()).or_insert(v);
                                }
                            }
                        }
                        Ok(Some(MessageItem::Error(m))) => errors.push((m.number, m.message)),
                        Ok(Some(_)) => {}
                    }
                }
                broken
            }
        }
    };
    match broken {
        Some(e) if is_desync(&e) => Err(s.lost_connection().await),
        Some(e) => Err(err(e)),
        None => Ok((errors, ran)),
    }
}

/// Whether the script's change stands, from the errors it raised and
/// whether a transaction was left open (a batch-ending error).
fn outcome(errors: Vec<(u32, String)>, ran: Ran, left_open: bool) -> Result<Ran> {
    let rolled_back = left_open || errors.iter().any(|(_, m)| m == ROLLED_BACK);
    let cause = errors
        .iter()
        .find(|(n, m)| *n != AGENT_NOT_NOTIFIED && m != ROLLED_BACK && m != AFTER_COMMIT)
        .map(|(_, m)| m.clone());
    if rolled_back {
        return Err(Error::Query(cause.unwrap_or_else(|| ROLLED_BACK.into())));
    }
    match cause {
        Some(m) => Err(Error::Query(format!(
            "Los cambios se guardaron, pero el servidor respondió con un error: {m}"
        ))),
        None => Ok(ran),
    }
}

#[cfg(test)]
mod tests;
