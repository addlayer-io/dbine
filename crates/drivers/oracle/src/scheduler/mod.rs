//! Oracle's Scheduler managed whole through the scheduler contract
//! ([`dbine_driver::Session::scheduler_info`] and the rest): every kind of
//! object `DBMS_SCHEDULER` has — jobs (and `DBMS_JOB` jobs before 19c),
//! programs, named schedules, chains, file watchers, credentials, job
//! classes, windows, window groups, database and external destinations,
//! destination groups, resources, incompatibilities and the Scheduler's own
//! attributes — each with a designer form, the PL/SQL block a change runs
//! and the Scheduler's logs.
//!
//! - Scripts: every save is one anonymous PL/SQL block of documented
//!   `DBMS_SCHEDULER` (and `DBMS_CREDENTIAL`, `DBMS_JOB`) calls. A new
//!   object is created and then given what its create call doesn't take;
//!   when a later call fails, the block drops what it created, so a create
//!   is all or nothing. An edit is a diff against what the dictionary has
//!   now: only what changed, in place, with `SET_ATTRIBUTE` /
//!   `SET_ATTRIBUTE_NULL`, `DEFINE_*` and the add / remove calls; nothing is
//!   dropped and created again (Oracle has no rename: a name is chosen when
//!   the object is created). `DBMS_SCHEDULER` commits each call, so an edit
//!   that fails halfway keeps the calls before (the script says so); a job
//!   or program that is enabled is disabled first and enabled again at the
//!   end, and a call that fails in between leaves it disabled, with an
//!   error that says so, so it never runs half changed. A calendaring
//!   repeat interval is checked (`EVALUATE_CALENDAR_STRING`) before any
//!   call changes the object.
//! - Values and ids come from the app as untrusted text: ids are parsed
//!   against the shape the lists give and looked up, names picked from
//!   lists are checked against the server's, and every value goes into the
//!   block as a literal (quotes doubled; a line that SQL*Plus would read as
//!   the block's end is written so it can't be). Secrets (a credential's
//!   password) are bound, never written into the block that runs, and
//!   masked in the script shown.
//! - Permissions: the dictionary is read from the `DBA_` views when the
//!   login can, else from `ALL_`. Objects in a schema are changed by their
//!   owner, with ALTER on them, or with the `CREATE ANY …` privilege;
//!   windows, window groups, job classes and the Scheduler's attributes need
//!   MANAGE SCHEDULER. Forms come read-only (with a note) where Oracle
//!   would refuse the change. Oracle's own objects (in SYS or a schema
//!   Oracle maintains; the job classes and internal window groups it
//!   installed) are shown, never changed nor dropped, whoever logs in; its
//!   maintenance windows and their group can be changed, as Oracle
//!   documents, not dropped. Each item says so in the tree, with the
//!   kind's actions it offers now ([`action`]: open or close a window, run
//!   a program or a chain once, stop a job by force, end a detached run).
//! - A job starts at a step of its chain (`RUN_CHAIN`; any other job runs
//!   whole) and its history is cleared with `PURGE_LOG` (MANAGE SCHEDULER).
//!   Fields, lists and tabs show only for the modes and types they apply
//!   to, and what a hidden one still holds is left out.
//! - Versions: 12c and later (11.2 where the views allow); kinds the
//!   version doesn't have aren't offered. Oracle Autonomous Database: the
//!   same, without external jobs, file watchers or remote destinations.

mod action;
mod calendar;
mod chain;
mod choices;
mod class;
mod credential;
mod destination;
mod global;
mod job;
mod logs;
mod program;
mod read;
mod resource;
mod schedule;
mod script;
mod timing;
mod watcher;
mod window;

#[cfg(test)]
mod tests;

pub(crate) use choices::Catalog;
pub(crate) use job::{clear_history, run_from};
pub(crate) use logs::logs;
pub(crate) use read::Rec;
pub(crate) use script::{Block, A};

use crate::jobs::{self, Access, JobId, Off, Views, CALL_LIMIT, QUERY_LIMIT};
use crate::processes::within;
use dbine_driver::{
    Error, FormValues, ObjectForm, Result, SchedulerInfo, SchedulerKind, SchedulerList,
};
use oracledb::Connection;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};

// ---------------------------------------------------------------------------
// Notes and messages
// ---------------------------------------------------------------------------

pub(crate) const NO_CREATE_JOB: &str = "Este usuario no tiene el privilegio CREATE JOB: puede ver los objetos del Scheduler, pero no crear jobs, programas, programaciones ni cadenas.";
const NO_MANAGE: &str = "Las ventanas, los grupos de ventanas, las clases de job y los atributos del Scheduler solo los cambia quien tiene el privilegio MANAGE SCHEDULER.";
pub(crate) const NO_RULES: &str = "Crear cadenas requiere, además de CREATE JOB, los privilegios del motor de reglas: CREATE RULE SET, CREATE RULE y CREATE EVALUATION CONTEXT.";
const CLOUD: &str = "Oracle Autonomous Database no ejecuta jobs externos (ejecutables, scripts externos, de SQL*Plus o de RMAN) ni tiene observadores de archivos ni destinos remotos: esos tipos no se ofrecen.";
const LEGACY: &str = "Antes de 19c, los jobs de DBMS_JOB están aparte del Scheduler: se crean, cambian y borran con DBMS_JOB, y solo los del propio usuario.";
const PARTIAL: &str = "Se ven solo los objetos del Scheduler a los que tiene acceso este usuario: para verlos todos hace falta poder leer las vistas DBA_ (SELECT_CATALOG_ROLE).";
pub(crate) const NO_RENAME: &str =
    "Oracle no permite renombrar los objetos del Scheduler: el nombre se elige al crearlo.";
pub(crate) const OWNER_ONLY: &str = "Este usuario no puede cambiar este objeto: hace falta ser su dueño, tener ALTER sobre él o el privilegio CREATE ANY JOB.";
pub(crate) const MANAGE_ONLY: &str =
    "Solo quien tiene el privilegio MANAGE SCHEDULER puede cambiar este objeto.";
pub(crate) const ORACLE_OWN: &str = "Es un objeto de Oracle (de SYS o de un esquema que mantiene Oracle): se puede ver, pero no cambiar ni borrar.";
pub(crate) const ORACLE_NO_DROP: &str =
    "Oracle instaló este objeto: se puede cambiar o deshabilitar, pero no borrar.";
pub(crate) const CHANGED: &str = "El objeto cambió en el servidor desde que se abrió el diseñador: volvé a abrirlo antes de guardar, para no deshacer esos cambios.";
const OPEN_TRANSACTION: &str = "Hay una transacción abierta en esta sesión: confirmala o deshacela antes de cambiar el Scheduler (DBMS_SCHEDULER confirma la transacción en cada cambio).";

pub(crate) fn bad(message: String) -> Error {
    Error::Query(message)
}

/// Text from the app, short and on one line, for a message.
pub(crate) fn shown(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(60)
        .collect()
}

// ---------------------------------------------------------------------------
// Kinds
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    Job,
    Program,
    Schedule,
    Chain,
    FileWatcher,
    Credential,
    JobClass,
    Window,
    WindowGroup,
    DbDestination,
    ExternalDestination,
    DestinationGroup,
    Resource,
    Incompatibility,
    Global,
}

impl Kind {
    pub(crate) fn parse(kind: &str) -> Result<Kind> {
        Ok(match kind.trim() {
            "job" => Kind::Job,
            "program" => Kind::Program,
            "schedule" => Kind::Schedule,
            "chain" => Kind::Chain,
            "file_watcher" => Kind::FileWatcher,
            "credential" => Kind::Credential,
            "job_class" => Kind::JobClass,
            "window" => Kind::Window,
            "window_group" => Kind::WindowGroup,
            "db_destination" => Kind::DbDestination,
            "external_destination" => Kind::ExternalDestination,
            "destination_group" => Kind::DestinationGroup,
            "resource" => Kind::Resource,
            "incompatibility" => Kind::Incompatibility,
            "global" => Kind::Global,
            other => {
                return Err(bad(format!(
                    "«{}» no es un tipo de objeto del Scheduler de Oracle",
                    shown(other)
                )))
            }
        })
    }

    /// Owned by SYS and changed with MANAGE SCHEDULER.
    pub(crate) fn sys_owned(self) -> bool {
        matches!(
            self,
            Kind::JobClass
                | Kind::Window
                | Kind::WindowGroup
                | Kind::ExternalDestination
                | Kind::Global
        )
    }
}

// ---------------------------------------------------------------------------
// What the server and the login allow
// ---------------------------------------------------------------------------

/// The privileges the Scheduler's calls look at.
const PRIVS: &str = "SELECT privilege FROM session_privs WHERE privilege IN (
  'CREATE JOB', 'CREATE ANY JOB', 'CREATE EXTERNAL JOB', 'MANAGE SCHEDULER',
  'CREATE CREDENTIAL', 'CREATE ANY CREDENTIAL', 'EXECUTE ANY PROGRAM', 'EXECUTE ANY CLASS',
  'CREATE RULE SET', 'CREATE RULE', 'CREATE EVALUATION CONTEXT',
  'CREATE ANY RULE SET', 'CREATE ANY RULE', 'CREATE ANY EVALUATION CONTEXT')";

/// The views of the kinds that came in later versions.
const VIEWS: &str = "SELECT view_name FROM all_views WHERE owner = 'SYS' AND view_name IN (
  'ALL_SCHEDULER_FILE_WATCHERS', 'ALL_SCHEDULER_DB_DESTS', 'ALL_SCHEDULER_EXTERNAL_DESTS',
  'ALL_SCHEDULER_GROUPS', 'ALL_SCHEDULER_RESOURCES', 'ALL_SCHEDULER_INCOMPATS',
  'ALL_CREDENTIALS', 'ALL_SCHEDULER_CREDENTIALS', 'ALL_SCHEDULER_NOTIFICATIONS')";

const GLOBALS: &str = "SELECT attribute_name, value FROM {v}_scheduler_global_attribute";

/// Set on Autonomous Database (`OLTP`, `DWCS`, `JDCS`…).
const CLOUD_SERVICE: &str = "SELECT SYS_CONTEXT('USERENV', 'CLOUD_SERVICE') FROM dual";

#[derive(Debug, Default)]
pub(crate) struct Ctx {
    pub(crate) v: Option<Views>,
    pub(crate) access: Access,
    pub(crate) privs: HashSet<String>,
    pub(crate) major: usize,
    pub(crate) minor: usize,
    /// Oracle Autonomous Database.
    pub(crate) cloud: bool,
    pub(crate) off: Off,
    /// The Scheduler's attributes (upper-case names).
    pub(crate) globals: HashMap<String, String>,
    /// The `ALL_` views of the later kinds that exist here.
    pub(crate) views: HashSet<String>,
}

impl Ctx {
    pub(crate) fn read(c: &Connection) -> Result<Ctx> {
        let v = jobs::views(c)?;
        let first_col = |sql: &str| -> Vec<String> {
            match jobs::texts(c, sql, &[], 1000) {
                Ok(rows) => rows.iter().filter_map(|r| jobs::col(r, 0)).collect(),
                Err(e) => {
                    tracing::debug!("oracle scheduler: {e}");
                    Vec::new()
                }
            }
        };
        let version = c.version().ok();
        let globals = match jobs::texts(c, &v.sql(GLOBALS), &[], 1000) {
            Ok(rows) => rows
                .iter()
                // Unset ones too (empty): the designer offers them.
                .filter_map(|r| {
                    Some((
                        jobs::col(r, 0)?.to_ascii_uppercase(),
                        jobs::col(r, 1).unwrap_or_default(),
                    ))
                })
                .collect(),
            Err(e) => {
                tracing::debug!("oracle scheduler: attributes: {e}");
                HashMap::new()
            }
        };
        Ok(Ctx {
            v: Some(v),
            access: Access::read(c),
            privs: first_col(PRIVS).into_iter().collect(),
            major: version.as_ref().map_or(0, |v| v.0),
            minor: version.as_ref().map_or(0, |v| v.1),
            cloud: !first_col(CLOUD_SERVICE).is_empty(),
            off: Off::read(c, v),
            globals,
            views: first_col(VIEWS).into_iter().collect(),
        })
    }

    pub(crate) fn v(&self) -> Views {
        self.v.unwrap_or(Views::All)
    }

    /// `sql` with `{v}` replaced by the views' prefix.
    pub(crate) fn sql(&self, sql: &str) -> String {
        self.v().sql(sql)
    }

    pub(crate) fn user(&self) -> &str {
        &self.access.user
    }

    pub(crate) fn can(&self, privilege: &str) -> bool {
        self.privs.contains(privilege)
    }

    pub(crate) fn manage(&self) -> bool {
        self.can("MANAGE SCHEDULER")
    }

    /// 12.2 and later take names of up to 128 bytes.
    pub(crate) fn max_name(&self) -> usize {
        if self.major > 12 || (self.major == 12 && self.minor >= 2) || self.major == 0 {
            jobs::MAX_NAME
        } else {
            30
        }
    }

    fn view(&self, name: &str) -> bool {
        self.views.contains(name)
    }

    /// `DBMS_CREDENTIAL` and `*_CREDENTIALS` (12c+).
    pub(crate) fn credential_api(&self) -> bool {
        self.view("ALL_CREDENTIALS")
    }

    pub(crate) fn has(&self, kind: Kind) -> bool {
        match kind {
            Kind::FileWatcher => !self.cloud && self.view("ALL_SCHEDULER_FILE_WATCHERS"),
            Kind::Credential => {
                self.view("ALL_CREDENTIALS") || self.view("ALL_SCHEDULER_CREDENTIALS")
            }
            Kind::DbDestination => !self.cloud && self.view("ALL_SCHEDULER_DB_DESTS"),
            Kind::ExternalDestination => !self.cloud && self.view("ALL_SCHEDULER_EXTERNAL_DESTS"),
            Kind::DestinationGroup => !self.cloud && self.view("ALL_SCHEDULER_GROUPS"),
            Kind::Resource => self.view("ALL_SCHEDULER_RESOURCES"),
            Kind::Incompatibility => self.view("ALL_SCHEDULER_INCOMPATS"),
            _ => true,
        }
    }

    /// Window groups are groups of type WINDOW (`CREATE_GROUP`, 12c+).
    pub(crate) fn groups_api(&self) -> bool {
        self.view("ALL_SCHEDULER_GROUPS")
    }

    pub(crate) fn notifications(&self) -> bool {
        self.view("ALL_SCHEDULER_NOTIFICATIONS")
    }

    /// The Scheduler's mail server is set (job notifications need it).
    pub(crate) fn email(&self) -> bool {
        self.globals
            .get("EMAIL_SERVER")
            .is_some_and(|s| !s.trim().is_empty())
    }

    /// The Scheduler's default time zone, for new objects.
    pub(crate) fn default_tz(&self) -> String {
        self.globals
            .get("DEFAULT_TIMEZONE")
            .map(|t| timing::tz_norm(t))
            .unwrap_or_default()
    }

    /// May create objects of `kind` in its own schema.
    pub(crate) fn create_own(&self, kind: Kind) -> bool {
        match kind {
            k if k.sys_owned() => self.manage(),
            Kind::Chain => {
                self.can("CREATE JOB")
                    && self.can("CREATE RULE SET")
                    && self.can("CREATE RULE")
                    && self.can("CREATE EVALUATION CONTEXT")
            }
            Kind::Credential if self.credential_api() => self.can("CREATE CREDENTIAL"),
            _ => self.can("CREATE JOB"),
        }
    }

    /// May create and change objects of `kind` in other schemas.
    pub(crate) fn create_any(&self, kind: Kind) -> bool {
        match kind {
            k if k.sys_owned() => false,
            Kind::Chain => {
                self.can("CREATE ANY JOB")
                    && self.can("CREATE ANY RULE SET")
                    && self.can("CREATE ANY RULE")
                    && self.can("CREATE ANY EVALUATION CONTEXT")
            }
            Kind::Credential if self.credential_api() => self.can("CREATE ANY CREDENTIAL"),
            _ => self.can("CREATE ANY JOB"),
        }
    }

    pub(crate) fn may_create(&self, kind: Kind) -> bool {
        self.create_own(kind) || self.create_any(kind)
    }

    /// May change (and drop) the object `owner.name` of `kind`.
    pub(crate) fn may_change(&self, kind: Kind, owner: &str, name: &str, system: bool) -> bool {
        if kind.sys_owned() {
            return self.manage();
        }
        owner == self.user()
            || (self.create_any(kind) && self.access.any_job_reaches(owner, system))
            || self
                .access
                .granted
                .contains(&(owner.to_string(), name.to_string()))
    }

    /// The object is Oracle's own (SYS or a schema Oracle maintains).
    pub(crate) fn oracle_schema(&self, owner: &str) -> bool {
        self.access.oracle_schema(owner)
    }

    /// Why the login can't change (nor drop) the object `owner.name`, if it
    /// can't. A kind SYS owns needs MANAGE SCHEDULER (what Oracle installed
    /// of those is the kind's own business). An object in a schema is
    /// locked when it's Oracle's own (`system`: in SYS or a schema Oracle
    /// maintains, or flagged by Oracle), whoever logs in, even SYS; else it
    /// takes its owner, ALTER on it or the `CREATE ANY …` privilege.
    pub(crate) fn locked(
        &self,
        kind: Kind,
        owner: &str,
        name: &str,
        system: bool,
    ) -> Option<&'static str> {
        if kind.sys_owned() {
            return (!self.manage()).then_some(MANAGE_ONLY);
        }
        if system {
            return Some(ORACLE_OWN);
        }
        (!self.may_change(kind, owner, name, system)).then_some(OWNER_ONLY)
    }

    /// [`Ctx::locked`] as an error.
    pub(crate) fn check_change(
        &self,
        kind: Kind,
        owner: &str,
        name: &str,
        system: bool,
    ) -> Result<()> {
        match self.locked(kind, owner, name, system) {
            Some(why) => Err(bad(why.into())),
            None => Ok(()),
        }
    }

    /// The login may use the program or chain `owner.name` in a job of its
    /// own: its owner, EXECUTE on it (directly, through PUBLIC or a role)
    /// or EXECUTE ANY PROGRAM.
    pub(crate) fn may_execute(
        &self,
        owner: &str,
        name: &str,
        granted: &HashSet<(String, String)>,
    ) -> bool {
        owner == self.user()
            || self.can("EXECUTE ANY PROGRAM")
            || granted.contains(&(owner.to_string(), name.to_string()))
    }
}

/// An item's `can_edit` / `can_delete`: `Some(false)` where it's locked,
/// else as its kind says.
pub(crate) fn unless(locked: bool) -> Option<bool> {
    locked.then_some(false)
}

/// Programs and chains (of `view`: `{v}_scheduler_programs` or `_chains`)
/// the login was granted EXECUTE on, directly, through PUBLIC or an enabled
/// role.
pub(crate) fn executable(
    c: &Connection,
    ctx: &Ctx,
    view: &str,
    col: &str,
) -> HashSet<(String, String)> {
    let sql = ctx.sql(&format!(
        "SELECT p.table_schema, p.table_name FROM all_tab_privs p
          WHERE p.privilege = 'EXECUTE'
            AND (p.grantee IN (USER, 'PUBLIC') OR p.grantee IN (SELECT role FROM session_roles))
            AND (p.table_schema, p.table_name) IN (SELECT owner, {col} FROM {view})"
    ));
    match jobs::texts(c, &sql, &[], 10_000) {
        Ok(rows) => rows
            .iter()
            .filter_map(|r| Some((jobs::col(r, 0)?, jobs::col(r, 1)?)))
            .collect(),
        Err(e) => {
            tracing::debug!("oracle scheduler: EXECUTE grants: {e}");
            HashSet::new()
        }
    }
}

/// The names of SYS's Scheduler objects of these dictionary types that
/// Oracle installed (`ORACLE_MAINTAINED = 'Y'`, 12.1+): windows, window
/// groups, job classes. `None` where the view doesn't say (11.2): the
/// caller guesses by the names Oracle gives them.
pub(crate) fn installed(c: &Connection, ctx: &Ctx, types: &str) -> Option<HashSet<String>> {
    let sql = ctx.sql(&format!(
        "SELECT object_name FROM {{v}}_objects
          WHERE owner = 'SYS' AND object_type IN ({types}) AND oracle_maintained = 'Y'"
    ));
    match jobs::texts(c, &sql, &[], 10_000) {
        Ok(rows) => Some(rows.iter().filter_map(|r| jobs::col(r, 0)).collect()),
        Err(e) => {
            tracing::debug!("oracle scheduler: Oracle's own objects: {e}");
            None
        }
    }
}

/// Oracle installed `name`: as [`installed`] says, else as `guess` does.
pub(crate) fn installed_by(
    set: &Option<HashSet<String>>,
    name: &str,
    guess: fn(&str) -> bool,
) -> bool {
    match set {
        Some(s) => s.contains(name),
        None => guess(name),
    }
}

/// The kinds, in the tree's order, and the notes.
pub(crate) fn info_of(c: &Ctx) -> SchedulerInfo {
    let w = |k: Kind| c.may_create(k);
    let schema = |k: Kind| (c.may_create(k), true, true);
    let sys = c.manage();
    let mut kinds = Vec::new();
    let mut push = |k: Kind, kind: SchedulerKind, (create, edit, delete): (bool, bool, bool)| {
        if c.has(k) {
            let mut kind = kind.abilities(create, edit, delete);
            kind.actions = action::of(c, k);
            kinds.push(kind);
        }
    };
    push(
        Kind::Job,
        SchedulerKind::new("job", "Jobs", "job")
            // A chain job starts at any of its steps (`RUN_CHAIN`); any
            // other job's one step is itself, and it runs whole.
            .run_from_step(true)
            // `PURGE_LOG` asks for MANAGE SCHEDULER, whoever owns the job.
            .clear_history(c.manage()),
        schema(Kind::Job),
    );
    push(
        Kind::Program,
        SchedulerKind::new("program", "Programas", "programa"),
        schema(Kind::Program),
    );
    push(
        Kind::Schedule,
        SchedulerKind::new("schedule", "Programaciones", "programación"),
        schema(Kind::Schedule),
    );
    push(
        Kind::Chain,
        SchedulerKind::new("chain", "Cadenas", "cadena"),
        schema(Kind::Chain),
    );
    push(
        Kind::FileWatcher,
        SchedulerKind::new(
            "file_watcher",
            "Observadores de archivos",
            "observador de archivos",
        ),
        schema(Kind::FileWatcher),
    );
    push(
        Kind::Credential,
        SchedulerKind::new("credential", "Credenciales", "credencial"),
        schema(Kind::Credential),
    );
    push(
        Kind::JobClass,
        SchedulerKind::new("job_class", "Clases de job", "clase de job"),
        (sys, sys, sys),
    );
    push(
        Kind::Window,
        SchedulerKind::new("window", "Ventanas", "ventana"),
        (sys, sys, sys),
    );
    push(
        Kind::WindowGroup,
        SchedulerKind::new("window_group", "Grupos de ventanas", "grupo de ventanas"),
        (sys, sys, sys),
    );
    push(
        Kind::DbDestination,
        SchedulerKind::new(
            "db_destination",
            "Destinos de base de datos",
            "destino de base de datos",
        ),
        schema(Kind::DbDestination),
    );
    push(
        Kind::ExternalDestination,
        SchedulerKind::new(
            "external_destination",
            "Destinos externos",
            "destino externo",
        ),
        (false, false, sys),
    );
    push(
        Kind::DestinationGroup,
        SchedulerKind::new(
            "destination_group",
            "Grupos de destinos",
            "grupo de destinos",
        ),
        schema(Kind::DestinationGroup),
    );
    push(
        Kind::Resource,
        SchedulerKind::new("resource", "Recursos", "recurso"),
        schema(Kind::Resource),
    );
    push(
        Kind::Incompatibility,
        SchedulerKind::new("incompatibility", "Incompatibilidades", "incompatibilidad"),
        schema(Kind::Incompatibility),
    );
    push(
        Kind::Global,
        SchedulerKind::new(
            "global",
            "Atributos del Scheduler",
            "atributos del Scheduler",
        ),
        (false, sys, false),
    );
    let mut notes = Vec::new();
    if c.v() == Views::All {
        notes.push(PARTIAL.to_string());
    }
    notes.extend(c.off.notes());
    if !w(Kind::Job) {
        notes.push(NO_CREATE_JOB.to_string());
    } else if !w(Kind::Chain) {
        notes.push(NO_RULES.to_string());
    }
    if !sys {
        notes.push(NO_MANAGE.to_string());
    }
    if c.cloud {
        notes.push(CLOUD.to_string());
    }
    if c.major > 0 && c.major < 19 {
        notes.push(LEGACY.to_string());
    }
    SchedulerInfo {
        kinds,
        logs: true,
        notes,
    }
}

// ---------------------------------------------------------------------------
// The contract's calls
// ---------------------------------------------------------------------------

fn ready(c: &Connection, kind: &str) -> Result<(Kind, Ctx)> {
    let kind = Kind::parse(kind)?;
    let ctx = Ctx::read(c)?;
    if !ctx.has(kind) {
        return Err(Error::Unsupported(if ctx.cloud {
            CLOUD.into()
        } else {
            "esta versión de Oracle no tiene ese tipo de objeto del Scheduler".into()
        }));
    }
    Ok((kind, ctx))
}

pub fn info(c: &Connection) -> Result<SchedulerInfo> {
    within(c, QUERY_LIMIT, || Ok(info_of(&Ctx::read(c)?)))
}

pub fn items(c: &Connection, kind: &str) -> Result<SchedulerList> {
    within(c, QUERY_LIMIT, || {
        let (kind, ctx) = ready(c, kind)?;
        match kind {
            Kind::Job => job::items(c, &ctx),
            Kind::Program => program::items(c, &ctx),
            Kind::Schedule => schedule::items(c, &ctx),
            Kind::Chain => chain::items(c, &ctx),
            Kind::FileWatcher => watcher::items(c, &ctx),
            Kind::Credential => credential::items(c, &ctx),
            Kind::JobClass => class::items(c, &ctx),
            Kind::Window => window::items(c, &ctx),
            Kind::WindowGroup => window::group_items(c, &ctx),
            Kind::DbDestination => destination::db_items(c, &ctx),
            Kind::ExternalDestination => destination::external_items(c, &ctx),
            Kind::DestinationGroup => destination::group_items(c, &ctx),
            Kind::Resource => resource::items(c, &ctx),
            Kind::Incompatibility => resource::incompat_items(c, &ctx),
            Kind::Global => global::items(c, &ctx),
        }
    })
}

pub fn form(c: &Connection, kind: &str, id: Option<&str>) -> Result<ObjectForm> {
    within(c, QUERY_LIMIT, || {
        let (kind, ctx) = ready(c, kind)?;
        if id.is_none() && kind == Kind::Global {
            return Err(Error::Unsupported(
                "los atributos del Scheduler no se crean: se cambian".into(),
            ));
        }
        if id.is_none() && kind == Kind::ExternalDestination {
            return Err(Error::Unsupported(destination::AGENT_ONLY.into()));
        }
        let mut f = match kind {
            Kind::Job => job::form(c, &ctx, id)?,
            Kind::Program => program::form(c, &ctx, id)?,
            Kind::Schedule => schedule::form(c, &ctx, id)?,
            Kind::Chain => chain::form(c, &ctx, id)?,
            Kind::FileWatcher => watcher::form(c, &ctx, id)?,
            Kind::Credential => credential::form(c, &ctx, id)?,
            Kind::JobClass => class::form(c, &ctx, id)?,
            Kind::Window => window::form(c, &ctx, id)?,
            Kind::WindowGroup => window::group_form(c, &ctx, id)?,
            Kind::DbDestination => destination::db_form(c, &ctx, id)?,
            Kind::ExternalDestination => destination::external_form(c, &ctx, id)?,
            Kind::DestinationGroup => destination::group_form(c, &ctx, id)?,
            Kind::Resource => resource::form(c, &ctx, id)?,
            Kind::Incompatibility => resource::incompat_form(c, &ctx, id)?,
            Kind::Global => global::form(c, &ctx)?,
        };
        if id.is_some() && kind != Kind::Global && kind != Kind::ExternalDestination {
            f.notes.push(NO_RENAME.into());
        }
        Ok(f)
    })
}

/// What a save runs: the block (`None`: nothing differs) and the object's
/// id afterwards.
pub(crate) struct Plan {
    pub(crate) block: Option<Block>,
    pub(crate) id: Option<String>,
    /// For the "nothing to save" script.
    pub(crate) name: String,
    /// A `DBMS_JOB.SUBMIT`: the new job's number comes back from the block.
    pub(crate) legacy_submit: bool,
}

impl Plan {
    pub(crate) fn new(block: Block, id: String, name: &str) -> Plan {
        Plan {
            block: Some(block),
            id: Some(id),
            name: name.to_string(),
            legacy_submit: false,
        }
    }

    /// `block`, or nothing when it calls nothing.
    pub(crate) fn changes(block: Block, id: String, name: &str) -> Plan {
        Plan {
            block: (block.calls() > 0).then_some(block),
            id: Some(id),
            name: name.to_string(),
            legacy_submit: false,
        }
    }

    pub(crate) fn script(&self) -> String {
        match &self.block {
            Some(b) => b.script(),
            None => format!(
                "-- «{}»: no hay cambios que guardar.\n",
                script::comment_text(&self.name)
            ),
        }
    }
}

fn plan(
    c: &Connection,
    ctx: &Ctx,
    kind: Kind,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    match kind {
        Kind::Job => job::plan(c, ctx, id, values),
        Kind::Program => program::plan(c, ctx, id, values),
        Kind::Schedule => schedule::plan(c, ctx, id, values),
        Kind::Chain => chain::plan(c, ctx, id, values),
        Kind::FileWatcher => watcher::plan(c, ctx, id, values),
        Kind::Credential => credential::plan(c, ctx, id, values),
        Kind::JobClass => class::plan(c, ctx, id, values),
        Kind::Window => window::plan(c, ctx, id, values),
        Kind::WindowGroup => window::group_plan(c, ctx, id, values),
        Kind::DbDestination => destination::db_plan(c, ctx, id, values),
        Kind::ExternalDestination => Err(Error::Unsupported(destination::AGENT_ONLY.into())),
        Kind::DestinationGroup => destination::group_plan(c, ctx, id, values),
        Kind::Resource => resource::plan(c, ctx, id, values),
        Kind::Incompatibility => resource::incompat_plan(c, ctx, id, values),
        Kind::Global => global::plan(c, ctx, values),
    }
}

pub fn script(c: &Connection, kind: &str, id: Option<&str>, values: &FormValues) -> Result<String> {
    within(c, QUERY_LIMIT, || {
        let (kind, ctx) = ready(c, kind)?;
        Ok(plan(c, &ctx, kind, id, values)?.script())
    })
}

pub fn save(
    c: &Connection,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Option<String>> {
    within(c, CALL_LIMIT, || {
        let (kind, ctx) = ready(c, kind)?;
        let plan = plan(c, &ctx, kind, id, values)?;
        let Some(block) = &plan.block else {
            return Ok(plan.id);
        };
        if plan.legacy_submit {
            let n = script::run_returning(c, block)?;
            return Ok(Some(format!("{}{n}", jobs::LEGACY_ID)));
        }
        script::run(c, block)?;
        Ok(plan.id)
    })
}

fn delete_block(c: &Connection, ctx: &Ctx, kind: Kind, id: &str) -> Result<Block> {
    match kind {
        Kind::Job => job::delete_block(c, ctx, id),
        Kind::Program => program::delete_block(c, ctx, id),
        Kind::Schedule => schedule::delete_block(c, ctx, id),
        Kind::Chain => chain::delete_block(c, ctx, id),
        Kind::FileWatcher => watcher::delete_block(c, ctx, id),
        Kind::Credential => credential::delete_block(c, ctx, id),
        Kind::JobClass => class::delete_block(c, ctx, id),
        Kind::Window => window::delete_block(c, ctx, id),
        Kind::WindowGroup => window::group_delete_block(c, ctx, id),
        Kind::DbDestination => destination::db_delete_block(c, ctx, id),
        Kind::ExternalDestination => destination::external_delete_block(c, ctx, id),
        Kind::DestinationGroup => destination::group_delete_block(c, ctx, id),
        Kind::Resource => resource::delete_block(c, ctx, id),
        Kind::Incompatibility => resource::incompat_delete_block(c, ctx, id),
        Kind::Global => Err(Error::Unsupported(
            "los atributos del Scheduler no se borran".into(),
        )),
    }
}

pub fn delete_script(c: &Connection, kind: &str, id: &str) -> Result<String> {
    within(c, QUERY_LIMIT, || {
        let (kind, ctx) = ready(c, kind)?;
        Ok(delete_block(c, &ctx, kind, id)?.script())
    })
}

pub fn delete(c: &Connection, kind: &str, id: &str) -> Result<()> {
    within(c, CALL_LIMIT, || {
        let (kind, ctx) = ready(c, kind)?;
        let block = delete_block(c, &ctx, kind, id)?;
        script::run(c, &block)
    })
}

/// The script of one of the kind's actions ([`action`]); nothing runs.
pub fn action_script(c: &Connection, kind: &str, id: &str, action: &str) -> Result<String> {
    within(c, QUERY_LIMIT, || {
        let (kind, ctx) = ready(c, kind)?;
        Ok(action::block(c, &ctx, kind, id, action)?.script())
    })
}

/// Runs one of the kind's actions, checked again as for its script.
pub fn run_action(c: &Connection, kind: &str, id: &str, action: &str) -> Result<()> {
    within(c, CALL_LIMIT, || {
        let (kind, ctx) = ready(c, kind)?;
        let block = action::block(c, &ctx, kind, id, action)?;
        script::run(c, &block)
    })
}

/// No transaction of the session's own is open: DBMS_SCHEDULER would
/// commit it.
pub(crate) fn no_open_transaction(c: &Connection) -> Result<()> {
    let id: Option<String> = c
        .query_row(
            "SELECT DBMS_TRANSACTION.LOCAL_TRANSACTION_ID FROM dual",
            &[],
        )
        .and_then(|r| r.get(0))
        .map_err(crate::err)?;
    match id {
        Some(_) => Err(Error::State(OPEN_TRANSACTION.into())),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Ids and names
// ---------------------------------------------------------------------------

/// An object's id: `OWNER.NAME`, or `"Owner"."Name"` (as the jobs list
/// gives them). SYS-owned objects without an owner column get SYS.
pub(crate) fn obj_id(owner: Option<&str>, name: &str) -> String {
    jobs::job_id(owner.filter(|o| !o.is_empty()).unwrap_or("SYS"), name)
}

/// An id that came back from the app: (owner, name), refused when it isn't
/// the shape [`obj_id`] gives or a DBMS_SCHEDULER call couldn't name it.
pub(crate) fn parse_obj(id: &str, what: &str) -> Result<(String, String)> {
    let refuse = || {
        bad(format!(
            "«{}» no es un id de {what} de Oracle (se espera «ESQUEMA.NOMBRE»)",
            shown(id)
        ))
    };
    match jobs::parse_id(id) {
        Ok(JobId::Scheduler { owner, name }) if jobs::callable(&owner, &name) => Ok((owner, name)),
        _ => Err(refuse()),
    }
}

/// A name a `Ref` field picked, as an id: checked the same way.
pub(crate) fn name_arg(id: &str, label: &str) -> Result<(String, String)> {
    match jobs::parse_id(id.trim()) {
        Ok(JobId::Scheduler { owner, name }) if jobs::callable(&owner, &name) => Ok((owner, name)),
        _ => Err(bad(format!(
            "«{label}»: «{}» no es un nombre de objeto de Oracle (ESQUEMA.NOMBRE).",
            shown(id)
        ))),
    }
}

/// A new object's name as Oracle stores it: a plain identifier in upper
/// case (as Oracle folds it unquoted), anything else exactly as written.
pub(crate) fn new_name(raw: &str, ctx: &Ctx, label: &str) -> Result<String> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(bad(format!("Falta «{label}».")));
    }
    let simple = t.starts_with(|c: char| c.is_ascii_alphabetic())
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '#'));
    let name = if simple {
        t.to_ascii_uppercase()
    } else {
        t.to_string()
    };
    if name.chars().any(|c| c == '"' || c.is_control()) {
        return Err(bad(format!(
            "«{label}» no puede tener comillas dobles ni caracteres de control."
        )));
    }
    if name.contains(',') {
        return Err(bad(format!(
            "«{label}» no puede tener comas: DBMS_SCHEDULER toma listas de nombres separados por comas."
        )));
    }
    let max = ctx.max_name();
    if name.len() > max {
        return Err(bad(format!(
            "«{label}» tiene más de {max} bytes, lo que Oracle acepta en esta versión."
        )));
    }
    Ok(name)
}

/// The owner of a new object: one of the schemas the login may create in.
pub(crate) fn new_owner(v: Vals, ctx: &Ctx, kind: Kind, catalog: &Catalog) -> Result<String> {
    let owner = v.opt("owner").unwrap_or_else(|| ctx.user().to_string());
    if owner == ctx.user() {
        if !ctx.create_own(kind) {
            return Err(bad(format!(
                "Este usuario no tiene el privilegio para crear este objeto en su esquema ({}).",
                shown(&owner)
            )));
        }
        return Ok(owner);
    }
    if !ctx.create_any(kind) {
        return Err(bad(format!(
            "Este usuario no puede crear objetos del Scheduler en el esquema «{}»: hace falta el privilegio CREATE ANY JOB (o CREATE ANY CREDENTIAL para una credencial).",
            shown(&owner)
        )));
    }
    catalog
        .pick("schema", &owner)
        .map(str::to_string)
        .ok_or_else(|| bad(format!("No existe el esquema «{}».", shown(&owner))))
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

    pub(crate) fn has(&self, k: &str) -> bool {
        self.get(k).is_some()
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

    /// Code as typed (only trailing blank lines dropped), `None` when blank.
    pub(crate) fn code(&self, k: &str) -> Option<String> {
        let raw = self.raw(k);
        let t = raw.trim_end();
        (!t.trim().is_empty()).then(|| t.to_string())
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

    /// The value, or `default` when it didn't come.
    pub(crate) fn bool_or(&self, k: &str, default: bool) -> bool {
        if self.has(k) {
            self.bool(k)
        } else {
            default
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

    /// One of `options` (by value), `default` when empty.
    pub(crate) fn choice(
        &self,
        k: &str,
        label: &str,
        options: &[&str],
        default: &str,
    ) -> Result<String> {
        let t = self.text(k);
        if t.is_empty() {
            return Ok(default.to_string());
        }
        options
            .iter()
            .find(|o| o.eq_ignore_ascii_case(&t))
            .map(|o| o.to_string())
            .ok_or_else(|| bad(format!("«{label}»: «{}» no es una opción.", shown(&t))))
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

/// Text that goes into a VARCHAR2 the Scheduler keeps: at most `max` bytes.
pub(crate) fn fits(t: &Option<String>, label: &str, max: usize) -> Result<()> {
    match t {
        Some(s) if s.len() > max => Err(bad(format!(
            "«{label}» tiene más de {max} bytes, lo que Oracle acepta."
        ))),
        _ => Ok(()),
    }
}

/// Seconds as a duration field gives them.
pub(crate) fn secs(v: Vals, k: &str, label: &str) -> Result<Option<i64>> {
    v.int(k, label, 0, 999_999_999)
}

// ---------------------------------------------------------------------------
// What the form was read from
// ---------------------------------------------------------------------------

/// A fingerprint of an object's editable values as read: the form carries
/// it (`dbine_state`, no field shows it) and a save from a form older than
/// the object is refused.
pub(crate) fn fingerprint(
    values: &BTreeMap<String, Value>,
    lists: &BTreeMap<String, Vec<BTreeMap<String, Value>>>,
) -> String {
    let text = serde_json::json!({ "values": values, "lists": lists }).to_string();
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in text.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{h:016x}")
}

/// The form was read from what the server has now.
pub(crate) fn fresh(values: &FormValues, now: &str) -> Result<()> {
    match values.values.get("dbine_state").and_then(Value::as_str) {
        Some(seen) if seen != now => Err(bad(CHANGED.into())),
        _ => Ok(()),
    }
}

/// A select's options, only the ones `keep` keeps.
pub(crate) fn only(
    kind: dbine_driver::FormKind,
    keep: impl Fn(&str) -> bool,
) -> dbine_driver::FormKind {
    use dbine_driver::FormKind;
    match kind {
        FormKind::Select { options } => FormKind::Select {
            options: options.into_iter().filter(|(v, _)| keep(v)).collect(),
        },
        FormKind::MultiSelect { options } => FormKind::MultiSelect {
            options: options.into_iter().filter(|(v, _)| keep(v)).collect(),
        },
        other => other,
    }
}

/// A `Ref` field's value: one the catalog lists (in its spelling), or the
/// one the object has now.
pub(crate) fn pick_ref(
    cat: &Catalog,
    source: &str,
    value: &str,
    current: Option<&str>,
    label: &str,
) -> Result<String> {
    if current == Some(value) {
        return Ok(value.to_string());
    }
    cat.pick(source, value).map(str::to_string).ok_or_else(|| {
        bad(format!(
            "«{label}»: no existe «{}» o este usuario no lo puede ver.",
            shown(value)
        ))
    })
}

/// An optional `Ref` field.
pub(crate) fn opt_ref(
    v: Vals,
    key: &str,
    cat: &Catalog,
    source: &str,
    current: Option<&str>,
    label: &str,
) -> Result<Option<String>> {
    match v.opt(key) {
        None => Ok(None),
        Some(t) => pick_ref(cat, source, &t, current, label).map(Some),
    }
}

/// `'"NAME"'` of an id, without its owner: the calls that take a SYS
/// object by name alone (a job class, an agent).
pub(crate) fn bare(id: &str) -> A {
    match jobs::parse_id(id) {
        Ok(JobId::Scheduler { name, .. }) => A::R(script::lit(&crate::quote(&name))),
        _ => A::S(id.to_string()),
    }
}

/// How the tree names an object: its name, with the schema when it isn't
/// the login's own.
pub(crate) fn item_name(ctx: &Ctx, owner: &str, name: &str) -> String {
    if owner == ctx.user() {
        name.to_string()
    } else {
        format!("{owner}.{name}")
    }
}
