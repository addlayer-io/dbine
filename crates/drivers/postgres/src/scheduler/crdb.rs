//! CockroachDB's schedules in the designer: backup schedules, changefeed
//! schedules, the changefeeds themselves, row-level TTL and the cluster's
//! own schedules.
//!
//! - Backups (`CREATE SCHEDULE … FOR BACKUP`): one schedule, or a full +
//!   incremental pair with the same label that the statements treat as one
//!   (`ALTER BACKUP SCHEDULE` changes both, `DROP SCHEDULE` drops both).
//!   Edited in place with `ALTER BACKUP SCHEDULE` (label, location,
//!   options, recurrences, schedule options) and `PAUSE` / `RESUME`. What
//!   ALTER can't change (what is backed up, the incremental location) drops
//!   the schedule and creates it again in one transaction: new ids, no run
//!   history; the backups already in the location stay
//!   (`ignore_existing_backups`).
//! - Changefeed schedules (`CREATE SCHEDULE … FOR CHANGEFEED`): there is no
//!   ALTER for them, so a change drops and creates the schedule again (new
//!   id, no run history); pausing and resuming are `PAUSE` / `RESUME
//!   SCHEDULE`.
//! - Changefeeds (`CREATE CHANGEFEED`: a job that runs until cancelled):
//!   `ALTER CHANGEFEED` (targets, sink, options) only works while it's
//!   paused, so DBine pauses it, waits until it is, changes it and resumes
//!   it. Deleting one is `CANCEL JOB`.
//! - Row-level TTL, its own kind (one per table): the table's `ttl_*`
//!   storage parameters (`ALTER TABLE … SET (…)` / `RESET (…)`); its
//!   schedule's id in the jobs list opens the same form. CockroachDB commits
//!   each schema change on its own, so those statements aren't one
//!   transaction.
//! - The cluster's own schedules (SQL stats compaction, schema telemetry):
//!   their recurrence is a cluster setting (`SET CLUSTER SETTING`, never in
//!   a transaction); they pause and resume like the rest and can't be
//!   deleted.
//!
//! Secrets: the server redacts a location's or a sink's credentials and the
//! encryption passphrase when it shows a schedule; the script DBine shows
//! masks the ones typed in the form, and errors are masked the same way.
//! Names typed in the form are parsed and written back quoted; option
//! values are always string literals; a changefeed's query must be one
//! `SELECT`.

use super::{bad, combine, datetime_field, name_of, plain, redact_passwords, shown, Source, Vals};
use crate::catalog::{comment_text, rows};
use crate::jobs::cockroach::{self as cj, ScheduleKind};
use crate::jobs::{ident, text, time, Row};
use crate::session::PgSession;
use dbine_driver::{
    Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerItem,
    SchedulerKind, SchedulerList, Session, TxState, SECRET_MASK,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio_postgres::SimpleQueryMessage;

const SCHEDULE: &str = "crdb_schedule:";
const FEED: &str = "crdb_job:";
/// Longest DBine waits for a changefeed to pause before changing it.
const PAUSE_WAIT: Duration = Duration::from_secs(60);

const NO_VIEWJOB: &str = "Tu usuario no puede ver los schedules: CockroachDB pide el privilegio VIEWJOB (o el rol admin). Se ven y se cambian solo tus changefeeds.";
const NOT_ADMIN: &str = "Tu usuario no es admin: los backups programados piden el privilegio BACKUP, los changefeeds el privilegio CHANGEFEED en sus tablas, la TTL de una tabla poder modificarla y la programación de los schedules del clúster MODIFYCLUSTERSETTING; el servidor rechaza lo que falte.";
const OTHERS: &str = "Este schedule es de otro usuario: solo su dueño o un admin pueden cambiarlo.";
const FEED_OTHERS: &str =
    "Este changefeed es de otro usuario: solo su dueño o un admin pueden cambiarlo.";
const BACKUP_RECREATE: &str = "ALTER BACKUP SCHEDULE no cambia qué se respalda ni la ubicación de los incrementales: al guardar esos cambios el schedule se borra y se crea de nuevo (otros ids, sin su historial de ejecuciones); los backups que ya están en la ubicación quedan.";
const FEED_RECREATE: &str = "CockroachDB no cambia un changefeed programado: al guardar cualquier cambio salvo «Activo» el schedule se borra y se crea de nuevo, con otro id y sin su historial de ejecuciones.";
const FEED_PAUSE: &str = "ALTER CHANGEFEED solo cambia un changefeed en pausa: si está corriendo, DBine lo pone en pausa, espera a que lo esté, lo cambia y lo reanuda.";
const PAIR: &str = "Este backup es un par de schedules (completo e incremental) con el mismo nombre: se cambian, pausan y borran juntos.";
const SECRETS: &str = "CockroachDB oculta las credenciales de la ubicación (redacted) y la contraseña de cifrado: para crear el schedule de nuevo hay que escribirlas.";
const TTL_COMMITS: &str = "CockroachDB confirma cada cambio de esquema por separado: si una sentencia falla, las anteriores quedan hechas.";
const SYSTEM: &str = "Este schedule es del clúster: no se borra, y su programación es una configuración del clúster.";
const OTHER: &str =
    "DBine no conoce este tipo de schedule: se ve tal como está y solo se puede borrar.";
const NO_SOURCE: &str = "Tu usuario no puede crear schedules ni changefeeds en esta base: CockroachDB pide el privilegio BACKUP para los backups programados y CHANGEFEED en sus tablas para los changefeeds.";
const QUERY_FIXED: &str = "La consulta de un changefeed no se cambia (ALTER CHANGEFEED no la admite): para otra, creá otro changefeed.";

fn q(s: &str) -> String {
    // CockroachDB's strings are always standard: only quotes are doubled.
    format!("'{}'", s.replace('\'', "''"))
}

// ---------------------------------------------------------------------------
// Who's asking
// ---------------------------------------------------------------------------

/// The user, the version and what the user may do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Caps {
    pub(crate) user: String,
    pub(crate) admin: bool,
    pub(crate) version: Option<(u32, u32)>,
    pub(crate) database: String,
    /// `SHOW SCHEDULES` answers (VIEWJOB or admin).
    pub(crate) schedules: bool,
    /// The system privileges granted to the user (BACKUP,
    /// MODIFYCLUSTERSETTING…).
    pub(crate) privileges: Vec<String>,
    /// The privileges the user holds on this database's objects, its roles'
    /// included (`SHOW GRANTS FOR`); `None`: unknown.
    pub(crate) granted: Option<Vec<String>>,
}

impl Caps {
    pub(crate) fn has(&self, privilege: &str) -> bool {
        self.admin
            || self
                .privileges
                .iter()
                .any(|p| p.eq_ignore_ascii_case(privilege))
    }

    /// The user may hold `privilege` somewhere: system-wide, or on an
    /// object of this database (`ALL` counts). Unknown grants count as yes:
    /// the server decides.
    pub(crate) fn may(&self, privilege: &str) -> bool {
        self.has(privilege)
            || self.granted.as_ref().is_none_or(|g| {
                g.iter()
                    .any(|p| p.eq_ignore_ascii_case(privilege) || p.eq_ignore_ascii_case("ALL"))
            })
    }

    pub(crate) fn may_change(&self, owner: Option<&str>) -> bool {
        self.admin || owner == Some(self.user.as_str())
    }

    fn at_least(&self, v: (u32, u32)) -> bool {
        self.version.is_some_and(|x| x >= v)
    }

    /// Keeps a drop and a create in one transaction: from 24.3 CockroachDB
    /// commits before each DDL statement unless told not to.
    fn hold_ddl(&self) -> Option<String> {
        self.at_least((24, 3))
            .then(|| "SET LOCAL autocommit_before_ddl = false".to_string())
    }

    pub(crate) fn notes(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.schedules {
            out.push(NO_VIEWJOB.to_string());
        }
        if !self.admin {
            out.push(NOT_ADMIN.to_string());
        }
        out
    }

    /// The tree's kinds, with what the user's privileges allow: a backup
    /// schedule needs BACKUP, a changefeed (scheduled or not) CHANGEFEED;
    /// without VIEWJOB only the user's own changefeeds show. A table's TTL
    /// needs rights on that table: the server decides (see NOT_ADMIN).
    pub(crate) fn kinds(&self) -> Vec<SchedulerKind> {
        let change = self.schedules || self.may("CHANGEFEED");
        vec![
            SchedulerKind::new("job", "Schedules", "schedule").abilities(
                !sources(self).is_empty(),
                change,
                change,
            ),
            SchedulerKind::new("ttl", "TTL por filas", "TTL de una tabla")
                .abilities(true, true, true),
        ]
    }
}

pub(crate) async fn caps(s: &PgSession) -> Result<Caps> {
    let who = cj::who(s).await?;
    let database = s
        .text("SELECT current_database() AS db")
        .await?
        .first()
        .and_then(|r| text(r, "db"))
        .unwrap_or_default();
    let privileges = match s
        .client
        .simple_query(
            "SELECT privilege_type FROM [SHOW SYSTEM GRANTS] WHERE grantee = current_user",
        )
        .await
    {
        Ok(m) => rows(m)
            .iter()
            .filter_map(|r| text(r, "privilege_type"))
            .collect(),
        Err(e) => {
            tracing::debug!("cockroach: system grants: {e}");
            Vec::new()
        }
    };
    let schedules = who.admin
        || cj::guarded(s, "SELECT 1 FROM [SHOW SCHEDULES] LIMIT 1")
            .await?
            .is_some();
    let granted = if who.admin {
        None
    } else {
        match s
            .client
            .simple_query(&format!(
                "SELECT privilege_type FROM [SHOW GRANTS FOR {}]",
                ident(&who.user)
            ))
            .await
        {
            Ok(m) => Some(
                rows(m)
                    .iter()
                    .filter_map(|r| text(r, "privilege_type"))
                    .collect(),
            ),
            Err(e) => {
                tracing::debug!("cockroach: grants: {e}");
                None
            }
        }
    };
    Ok(Caps {
        user: who.user,
        admin: who.admin,
        version: who.version,
        database,
        schedules,
        privileges,
        granted,
    })
}

// ---------------------------------------------------------------------------
// What a job id leads to
// ---------------------------------------------------------------------------

/// A "job" of the tree, or a TTL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Obj {
    /// A new one, in this scheduler.
    New(Source),
    /// A schedule (`crdb_schedule:<n>`).
    Schedule(i64),
    /// A changefeed (`crdb_job:<n>`).
    Feed(i64),
    /// The row-level TTL of the table with this descriptor id; `None`: a
    /// new one.
    Ttl(Option<i64>),
}

/// A TTL's id: the table's descriptor id.
pub(crate) fn ttl_obj(id: Option<&str>) -> Result<Obj> {
    match id {
        None => Ok(Obj::Ttl(None)),
        Some(t) => {
            let n: i64 = t
                .parse()
                .ok()
                .filter(|n: &i64| *n > 0 && n.to_string() == t)
                .ok_or_else(|| bad(format!("«{}» no es una tabla de este servidor", shown(t))))?;
            Ok(Obj::Ttl(Some(n)))
        }
    }
}

/// The schedulers a new job can go to, by the user's privileges.
pub(crate) fn sources(c: &Caps) -> Vec<Source> {
    let mut out = Vec::new();
    if c.may("BACKUP") {
        out.push(Source::CrdbBackup);
    }
    if c.may("CHANGEFEED") {
        out.extend([Source::CrdbChangefeed, Source::CrdbFeed]);
    }
    out
}

// ---------------------------------------------------------------------------
// Schedules as the server shows them
// ---------------------------------------------------------------------------

/// A schedule (`SHOW SCHEDULES`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Sched {
    pub(crate) id: i64,
    pub(crate) label: String,
    pub(crate) active: bool,
    pub(crate) state: Option<String>,
    pub(crate) recurrence: Option<String>,
    pub(crate) owner: Option<String>,
    pub(crate) next_run: Option<String>,
    pub(crate) previous: Option<String>,
    pub(crate) failure: Option<String>,
    pub(crate) command: Value,
    pub(crate) kind: ScheduleKind,
}

impl Sched {
    pub(crate) fn of(r: &dyn Row) -> Option<Sched> {
        let command = text(r, "command")
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or(Value::Null);
        Some(Sched {
            id: text(r, "sid")?.parse().ok()?,
            label: r.get("label").unwrap_or_default(),
            active: text(r, "schedule_status").as_deref() == Some("ACTIVE"),
            state: text(r, "state").filter(|s| s != "NULL"),
            recurrence: text(r, "recurrence"),
            owner: text(r, "owner"),
            next_run: time(r, "next_run_epoch"),
            previous: text(r, "on_previous_running"),
            failure: text(r, "on_execution_failure"),
            kind: ScheduleKind::of(r),
            command,
        })
    }

    /// The statement it runs.
    pub(crate) fn statement(&self) -> Option<String> {
        ["backup_statement", "changefeed_statement"]
            .iter()
            .find_map(|k| self.command.get(*k).and_then(Value::as_str))
            .map(str::to_string)
    }

    fn number(&self, key: &str) -> Option<i64> {
        match self.command.get(key)? {
            Value::Number(n) => n.as_i64(),
            Value::String(s) => s.parse().ok(),
            _ => None,
        }
    }

    /// The other schedule of a backup pair.
    pub(crate) fn dependent(&self) -> Option<i64> {
        self.number("dependent_schedule_id").filter(|n| *n > 0)
    }

    /// The incremental schedule of a backup pair.
    pub(crate) fn incremental(&self) -> bool {
        match self.command.get("backup_type") {
            Some(Value::Number(n)) => n.as_i64() == Some(1),
            Some(Value::String(t)) => t.eq_ignore_ascii_case("INCREMENTAL"),
            _ => false,
        }
    }

    /// A row-level TTL schedule's table.
    pub(crate) fn table_id(&self) -> Option<i64> {
        self.number("tableId")
    }

    /// Paused until the pair's first full backup ends.
    fn waiting(&self) -> bool {
        self.state.as_deref().is_some_and(|s| {
            s.to_ascii_lowercase()
                .contains("waiting for initial backup")
        })
    }
}

fn sched_sql(filter: &str) -> String {
    format!(
        "SELECT *, id::STRING AS sid, extract(epoch FROM next_run)::STRING AS next_run_epoch
         FROM [SHOW SCHEDULES] WHERE {filter} ORDER BY id"
    )
}

async fn schedules(s: &PgSession, filter: &str) -> Result<Vec<Sched>> {
    match cj::guarded(s, &sched_sql(filter)).await? {
        Some(rows) => Ok(rows.iter().filter_map(|r| Sched::of(r)).collect()),
        None => Err(bad(
            "Tu usuario no puede ver los schedules: CockroachDB pide el privilegio VIEWJOB (o el rol admin)."
                .into(),
        )),
    }
}

async fn read(s: &PgSession, n: i64) -> Result<Sched> {
    schedules(s, &format!("id = {n}"))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| bad(format!("no existe el schedule {n} (o tu usuario no lo ve)")))
}

/// A backup schedule and the other half of its pair: (full, incremental).
async fn pair(s: &PgSession, sched: Sched) -> Result<(Sched, Option<Sched>)> {
    let other = match sched.dependent() {
        Some(n) => schedules(s, &format!("id = {n}")).await?.into_iter().next(),
        None => None,
    };
    Ok(match other {
        Some(o) if sched.incremental() => (o, Some(sched)),
        Some(o) => (sched, Some(o)),
        None => (sched, None),
    })
}

/// No other schedule has this label (the new one is found by it).
async fn label_free(s: &PgSession, label: &str, except: &[i64]) -> Result<()> {
    let not = if except.is_empty() {
        String::new()
    } else {
        format!(
            " AND id NOT IN ({})",
            except
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let taken = match cj::guarded(
        s,
        &format!(
            "SELECT 1 FROM [SHOW SCHEDULES] WHERE label = {}{not} LIMIT 1",
            q(label)
        ),
    )
    .await?
    {
        Some(rows) => !rows.is_empty(),
        None => false,
    };
    if taken {
        return Err(bad(format!(
            "Ya hay un schedule llamado «{}»: elegí otro nombre.",
            shown(label)
        )));
    }
    Ok(())
}

/// The id of the schedule with this label (a pair's full one).
fn by_label(label: &str) -> String {
    format!(
        "SELECT id::STRING AS id FROM [SHOW SCHEDULES] WHERE label = {}
         ORDER BY (command ? 'backup_type'), id LIMIT 1",
        q(label)
    )
}

// ---------------------------------------------------------------------------
// Reading CockroachDB's statements
// ---------------------------------------------------------------------------

/// A token of a statement as CockroachDB writes it.
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Ident(String),
    Str(String),
    Sym(char),
}

/// The tokens and where each starts; `None` when a quote isn't closed.
fn lex(s: &str) -> Option<Vec<(Tok, usize)>> {
    let chars: Vec<(usize, char)> = s.char_indices().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let at = |i: usize| chars.get(i).map(|(_, c)| *c);
    while let Some(c) = at(i) {
        let pos = chars[i].0;
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && at(i + 1) == Some('-') {
            while at(i).is_some_and(|c| c != '\n') {
                i += 1;
            }
        } else if c == '/' && at(i + 1) == Some('*') {
            i += 2;
            while at(i).is_some() && !(at(i) == Some('*') && at(i + 1) == Some('/')) {
                i += 1;
            }
            i += 2;
        } else if (c == 'e' || c == 'E') && at(i + 1) == Some('\'') {
            // An escape string.
            i += 2;
            let mut v = String::new();
            loop {
                match at(i)? {
                    '\\' => {
                        let n = at(i + 1)?;
                        v.push(match n {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            other => other,
                        });
                        i += 2;
                    }
                    '\'' if at(i + 1) == Some('\'') => {
                        v.push('\'');
                        i += 2;
                    }
                    '\'' => {
                        i += 1;
                        break;
                    }
                    ch => {
                        v.push(ch);
                        i += 1;
                    }
                }
            }
            out.push((Tok::Str(v), pos));
        } else if c == '\'' || c == '"' {
            i += 1;
            let mut v = String::new();
            loop {
                let ch = at(i)?;
                if ch == c && at(i + 1) == Some(c) {
                    v.push(c);
                    i += 2;
                } else if ch == c {
                    i += 1;
                    break;
                } else {
                    v.push(ch);
                    i += 1;
                }
            }
            out.push((
                if c == '\'' {
                    Tok::Str(v)
                } else {
                    Tok::Ident(v)
                },
                pos,
            ));
        } else if c.is_alphanumeric() || c == '_' || c == '$' {
            let mut v = String::new();
            while let Some(ch) = at(i).filter(|ch| ch.is_alphanumeric() || *ch == '_' || *ch == '$')
            {
                v.push(ch);
                i += 1;
            }
            out.push((Tok::Word(v), pos));
        } else {
            out.push((Tok::Sym(c), pos));
            i += 1;
        }
    }
    Some(out)
}

struct P<'a> {
    toks: &'a [(Tok, usize)],
    i: usize,
}

impl<'a> P<'a> {
    fn peek(&self) -> Option<&'a Tok> {
        self.toks.get(self.i).map(|(t, _)| t)
    }

    fn pos(&self) -> Option<usize> {
        self.toks.get(self.i).map(|(_, p)| *p)
    }

    fn bump(&mut self) -> Option<&'a Tok> {
        let t = self.peek();
        self.i += 1;
        t
    }

    fn is_word(&self, w: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(x)) if x.eq_ignore_ascii_case(w))
    }

    fn word(&mut self, w: &str) -> bool {
        let yes = self.is_word(w);
        if yes {
            self.i += 1;
        }
        yes
    }

    fn sym(&mut self, c: char) -> bool {
        let yes = self.peek() == Some(&Tok::Sym(c));
        if yes {
            self.i += 1;
        }
        yes
    }

    fn string(&mut self) -> Option<String> {
        match self.peek()? {
            Tok::Str(s) => {
                self.i += 1;
                Some(s.clone())
            }
            _ => None,
        }
    }

    fn done(&self) -> bool {
        self.i >= self.toks.len()
    }
}

/// One part of a name.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Ident(String),
    Star,
}

/// A qualified name (`db.schema.table`, `db.*`), its parts as the server
/// has them (an unquoted part folded to lower case).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Name(Vec<Part>);

fn simple(p: &str) -> bool {
    let mut cs = p.chars();
    cs.next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && cs.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

impl Name {
    /// As the form shows it: quoted only where it has to be.
    fn display(&self) -> String {
        self.0
            .iter()
            .map(|p| match p {
                Part::Star => "*".to_string(),
                Part::Ident(x) if simple(x) => x.clone(),
                Part::Ident(x) => ident(x),
            })
            .collect::<Vec<_>>()
            .join(".")
    }

    /// As a statement takes it: every part quoted.
    fn sql(&self) -> String {
        self.0
            .iter()
            .map(|p| match p {
                Part::Star => "*".to_string(),
                Part::Ident(x) => ident(x),
            })
            .collect::<Vec<_>>()
            .join(".")
    }
}

/// A name at the cursor; `star`: `*` may end it.
fn name_at(p: &mut P, star: bool) -> Option<Name> {
    let mut parts = Vec::new();
    loop {
        match p.peek()? {
            Tok::Word(w) => parts.push(Part::Ident(w.to_lowercase())),
            Tok::Ident(x) => parts.push(Part::Ident(x.clone())),
            Tok::Sym('*') if star && !parts.is_empty() => parts.push(Part::Star),
            _ => return None,
        }
        p.bump();
        if parts.last() == Some(&Part::Star) || !p.sym('.') {
            break;
        }
    }
    (!parts.is_empty()).then_some(Name(parts))
}

/// A changefeed target: a table and maybe one of its column families.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) table: Name,
    pub(crate) family: Option<String>,
}

impl Target {
    fn display(&self) -> String {
        match &self.family {
            Some(f) if simple(f) => format!("{} FAMILY {f}", self.table.display()),
            Some(f) => format!("{} FAMILY {}", self.table.display(), ident(f)),
            None => self.table.display(),
        }
    }

    fn sql(&self) -> String {
        match &self.family {
            Some(f) => format!("TABLE {} FAMILY {}", self.table.sql(), ident(f)),
            None => format!("TABLE {}", self.table.sql()),
        }
    }
}

/// An option's value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OptVal {
    Flag,
    One(String),
    List(Vec<String>),
}

/// `name [= value]`, the name in lower case with single spaces.
pub(crate) type Opt = (String, OptVal);

fn options_at(p: &mut P) -> Option<Vec<Opt>> {
    let parens = p.sym('(');
    let mut out = Vec::new();
    loop {
        let mut words = Vec::new();
        while let Some(Tok::Word(w)) = p.peek() {
            if !parens && w.eq_ignore_ascii_case("RECURRING") && !words.is_empty() {
                break;
            }
            if !parens && words.is_empty() && w.eq_ignore_ascii_case("RECURRING") {
                break;
            }
            words.push(w.to_lowercase());
            p.bump();
        }
        if words.is_empty() {
            break;
        }
        let value = if p.sym('=') {
            if p.sym('(') {
                let mut list = Vec::new();
                loop {
                    list.push(p.string()?);
                    if !p.sym(',') {
                        break;
                    }
                }
                p.sym(')').then_some(())?;
                OptVal::List(list)
            } else {
                match p.bump()? {
                    Tok::Str(s) | Tok::Word(s) | Tok::Ident(s) => OptVal::One(s.clone()),
                    Tok::Sym('-') => match p.bump()? {
                        Tok::Word(s) => OptVal::One(format!("-{s}")),
                        _ => return None,
                    },
                    _ => return None,
                }
            }
        } else {
            OptVal::Flag
        };
        out.push((words.join(" "), value));
        if !p.sym(',') {
            break;
        }
    }
    if parens && !p.sym(')') {
        return None;
    }
    Some(out)
}

/// One location, or a list of them in parentheses.
fn locations_at(p: &mut P) -> Option<Vec<String>> {
    if p.sym('(') {
        let mut out = Vec::new();
        loop {
            out.push(p.string()?);
            if !p.sym(',') {
                break;
            }
        }
        p.sym(')').then_some(out)
    } else {
        Some(vec![p.string()?])
    }
}

/// What a backup statement backs up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum What {
    Cluster,
    Databases(Vec<Name>),
    Tables(Vec<Name>),
}

/// A backup statement as a schedule keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BackupStmt {
    pub(crate) what: What,
    pub(crate) locations: Vec<String>,
    pub(crate) options: Vec<Opt>,
}

pub(crate) fn parse_backup(stmt: &str) -> Option<BackupStmt> {
    let toks = lex(stmt)?;
    let mut p = P { toks: &toks, i: 0 };
    p.word("BACKUP").then_some(())?;
    let what = if p.word("DATABASE") {
        What::Databases(names_until_into(&mut p, false)?)
    } else if p.word("TABLE") {
        What::Tables(names_until_into(&mut p, true)?)
    } else {
        What::Cluster
    };
    p.word("INTO").then_some(())?;
    if p.word("LATEST") {
        p.word("IN").then_some(())?;
    }
    let locations = locations_at(&mut p)?;
    // `AS OF SYSTEM TIME …` isn't in schedules; skip to the options.
    while !p.done() && !p.is_word("WITH") {
        p.bump();
    }
    let options = if p.word("WITH") {
        p.word("OPTIONS");
        options_at(&mut p)?
    } else {
        Vec::new()
    };
    Some(BackupStmt {
        what,
        locations,
        options,
    })
}

fn names_until_into(p: &mut P, star: bool) -> Option<Vec<Name>> {
    let mut out = Vec::new();
    loop {
        out.push(name_at(p, star)?);
        if !p.sym(',') {
            break;
        }
    }
    Some(out)
}

/// A changefeed's statement (`CREATE CHANGEFEED …`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FeedStmt {
    pub(crate) targets: Vec<Target>,
    pub(crate) sink: Option<String>,
    pub(crate) options: Vec<Opt>,
    pub(crate) query: Option<String>,
}

pub(crate) fn parse_feed(stmt: &str) -> Option<FeedStmt> {
    let toks = lex(stmt)?;
    let mut p = P { toks: &toks, i: 0 };
    (p.word("CREATE") && p.word("CHANGEFEED")).then_some(())?;
    let mut targets = Vec::new();
    if p.word("FOR") {
        loop {
            p.word("TABLE");
            let table = name_at(&mut p, false)?;
            let family = if p.word("FAMILY") {
                match p.bump()? {
                    Tok::Word(w) => Some(w.to_lowercase()),
                    Tok::Ident(x) => Some(x.clone()),
                    _ => return None,
                }
            } else {
                None
            };
            targets.push(Target { table, family });
            if !p.sym(',') {
                break;
            }
        }
    }
    let sink = if p.word("INTO") {
        Some(p.string()?)
    } else {
        None
    };
    let options = if p.word("WITH") {
        p.word("OPTIONS");
        options_at(&mut p)?
    } else {
        Vec::new()
    };
    let query = if p.is_word("AS") {
        let at = p.pos()?;
        Some(
            stmt[at + 2..]
                .trim()
                .trim_end_matches(';')
                .trim()
                .to_string(),
        )
    } else {
        None
    };
    Some(FeedStmt {
        targets,
        sink,
        options,
        query,
    })
}

// ---------------------------------------------------------------------------
// Values typed in the form
// ---------------------------------------------------------------------------

/// The lines of a textarea, trimmed, without empties.
fn lines(v: Vals, k: &str) -> Vec<String> {
    v.raw(k)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// A name typed in the form.
fn parse_name(t: &str, star: bool, label: &str) -> Result<Name> {
    let refuse = || {
        bad(format!(
            "«{label}»: «{}» no es un nombre válido (partes separadas por puntos; entre comillas dobles las que tengan otros caracteres).",
            shown(t)
        ))
    };
    plain(t, label)?;
    let toks = lex(t).ok_or_else(refuse)?;
    let mut p = P { toks: &toks, i: 0 };
    let name = name_at(&mut p, star).ok_or_else(refuse)?;
    if !p.done() {
        return Err(refuse());
    }
    Ok(name)
}

/// A changefeed target typed in the form: `table [FAMILY family]`.
fn parse_target(t: &str) -> Result<Target> {
    let label = "Tablas";
    let refuse = || {
        bad(format!(
            "«{label}»: «{}» no es una tabla (db.esquema.tabla, y FAMILY familia si es una sola).",
            shown(t)
        ))
    };
    plain(t, label)?;
    let toks = lex(t).ok_or_else(refuse)?;
    let mut p = P { toks: &toks, i: 0 };
    p.word("TABLE");
    let table = name_at(&mut p, false).ok_or_else(refuse)?;
    let family = if p.word("FAMILY") {
        match p.bump() {
            Some(Tok::Word(w)) => Some(w.to_lowercase()),
            Some(Tok::Ident(x)) => Some(x.clone()),
            _ => return Err(refuse()),
        }
    } else {
        None
    };
    if !p.done() {
        return Err(refuse());
    }
    Ok(Target { table, family })
}

/// A location or sink: one line with a scheme.
fn check_uri(u: &str, label: &str) -> Result<String> {
    plain(u, label)?;
    let t = u.trim();
    let scheme_ok = t.split_once("://").is_some_and(|(s, _)| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    });
    if !scheme_ok {
        return Err(bad(format!(
            "«{label}»: «{}» no es una URI (esquema://…).",
            shown(&mask_uri(t))
        )));
    }
    Ok(t.to_string())
}

/// The value as the server showed it, credentials hidden.
fn redacted(u: &str) -> bool {
    u.contains("redacted") || u.contains("*****")
}

/// A cron expression (five fields) or a `@daily`-style macro.
pub(crate) fn check_cron(t: &str, label: &str) -> Result<String> {
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let macro_ok =
        t.starts_with('@') && t.len() > 1 && t[1..].bytes().all(|b| b.is_ascii_alphabetic());
    let fields: Vec<&str> = t.split(' ').collect();
    let fields_ok = fields.len() == 5
        && fields.iter().all(|f| {
            !f.is_empty()
                && f.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'*' | b',' | b'/' | b'-'))
        });
    if macro_ok || fields_ok {
        Ok(t)
    } else {
        Err(bad(format!(
            "«{label}»: «{}» no es una expresión cron de cinco campos (minuto hora día mes día-de-la-semana) ni una como @daily.",
            shown(&t)
        )))
    }
}

/// `name = value` lines (a value may be quoted) as options.
fn parse_options(v: Vals, k: &str) -> Result<Vec<Opt>> {
    let mut out: Vec<Opt> = Vec::new();
    for line in lines(v, k) {
        plain(&line, "Opciones")?;
        let (name, value) = match line.split_once('=') {
            Some((n, v)) => (n.trim().to_lowercase(), Some(v.trim().to_string())),
            None => (line.trim().to_lowercase(), None),
        };
        let name_ok = !name.is_empty()
            && name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !name_ok {
            return Err(bad(format!(
                "«Opciones»: «{}» no es el nombre de una opción.",
                shown(&name)
            )));
        }
        let value = match value {
            None => OptVal::Flag,
            Some(v) => {
                let unq = if v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'') {
                    v[1..v.len() - 1].replace("''", "'")
                } else {
                    v
                };
                OptVal::One(unq)
            }
        };
        if out.iter().any(|(n, _)| *n == name) {
            return Err(bad(format!(
                "«Opciones»: «{}» está más de una vez.",
                shown(&name)
            )));
        }
        out.push((name, value));
    }
    Ok(out)
}

fn options_text(opts: &[Opt]) -> String {
    opts.iter()
        .map(|(n, v)| match v {
            OptVal::Flag => n.clone(),
            OptVal::One(x) => format!("{n} = {x}"),
            OptVal::List(xs) => format!("{n} = {}", xs.join(", ")),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `name = 'value'`, a flag alone; a secret-looking value masked unless
/// `reveal`.
fn options_sql(opts: &[Opt], reveal: bool) -> String {
    opts.iter()
        .map(|(n, v)| match v {
            OptVal::Flag => n.clone(),
            OptVal::One(x) => format!("{n} = {}", q(&shown_value(n, x, reveal))),
            OptVal::List(xs) => format!(
                "{n} = ({})",
                xs.iter()
                    .map(|x| q(&shown_value(n, x, reveal)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// An option value as a shown script carries it.
fn shown_value(name: &str, v: &str, reveal: bool) -> String {
    if reveal {
        return v.to_string();
    }
    if secret_key(name) {
        SECRET_MASK.to_string()
    } else if v.contains("://") {
        mask_uri(v)
    } else {
        v.to_string()
    }
}

/// A query parameter or option whose value is a credential.
fn secret_key(k: &str) -> bool {
    let k = k.to_ascii_uppercase();
    k.contains("SECRET")
        || k.contains("PASSWORD")
        || k.contains("PASSPHRASE")
        || k.contains("TOKEN")
        || k.contains("CREDENTIALS")
        || k.contains("AUTH_HEADER")
        || k.ends_with("_KEY")
        || k == "KEY"
        || k.ends_with("ACCOUNT_KEY")
}

/// A URI with its credentials masked: the password of `user:password@` and
/// the values of the query parameters that hold secrets.
pub(crate) fn mask_uri(uri: &str) -> String {
    let (base, query) = match uri.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (uri, None),
    };
    let mut out = match base.split_once("://") {
        Some((scheme, rest)) => {
            let (auth, path) = match rest.find('/') {
                Some(i) => (&rest[..i], &rest[i..]),
                None => (rest, ""),
            };
            match auth.rsplit_once('@') {
                Some((userinfo, host)) => match userinfo.split_once(':') {
                    Some((user, _)) => format!("{scheme}://{user}:{SECRET_MASK}@{host}{path}"),
                    None => base.to_string(),
                },
                None => base.to_string(),
            }
        }
        None => base.to_string(),
    };
    if let Some(query) = query {
        out.push('?');
        out.push_str(
            &query
                .split('&')
                .map(|kv| match kv.split_once('=') {
                    Some((k, _)) if secret_key(k) => format!("{k}={SECRET_MASK}"),
                    _ => kv.to_string(),
                })
                .collect::<Vec<_>>()
                .join("&"),
        );
    }
    out
}

/// A message with the credentials of the URIs in it masked (the server
/// echoes a location it couldn't reach).
pub(crate) fn redact(text: &str) -> String {
    let text = redact_passwords(text);
    let mut out = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(i) = rest.find("://") {
        let start = rest[..i]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')))
            .map_or(0, |j| j + 1);
        let end = rest[i..]
            .find(|c: char| c.is_whitespace() || matches!(c, '\'' | '"' | ')' | ','))
            .map_or(rest.len(), |j| i + j);
        out.push_str(&rest[..start]);
        out.push_str(&mask_uri(&rest[start..end]));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn uri_sql(u: &str, reveal: bool) -> String {
    q(&if reveal { u.to_string() } else { mask_uri(u) })
}

fn locations_sql(locs: &[String], reveal: bool) -> String {
    if locs.len() == 1 {
        uri_sql(&locs[0], reveal)
    } else {
        format!(
            "({})",
            locs.iter()
                .map(|l| uri_sql(l, reveal))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// A schedule option as the form keeps it.
fn previous_of(server: Option<&str>) -> &'static str {
    match server.map(str::to_ascii_uppercase).as_deref() {
        Some("SKIP") => "skip",
        Some("NO_WAIT" | "START") => "start",
        _ => "wait",
    }
}

fn failure_of(server: Option<&str>) -> &'static str {
    match server.map(str::to_ascii_uppercase).as_deref() {
        Some("RETRY_SOON" | "RETRY") => "retry",
        Some("PAUSE_SCHED" | "PAUSE") => "pause",
        _ => "reschedule",
    }
}

fn one_of(v: Vals, k: &str, label: &str, options: &[&str], default: &str) -> Result<String> {
    match v.opt(k) {
        None => Ok(default.to_string()),
        Some(x) if options.contains(&x.as_str()) => Ok(x),
        Some(x) => Err(bad(format!(
            "«{label}»: «{}» no es una de sus opciones.",
            shown(&x)
        ))),
    }
}

fn previous_field() -> FormField {
    FormField::new(
        "on_previous_running",
        "Si la ejecución anterior sigue corriendo",
        FormKind::select(&[
            ("wait", "Esperar a que termine"),
            ("skip", "Saltear esta ejecución"),
            ("start", "Empezar otra igual"),
        ]),
    )
    .default(json!("wait"))
}

fn failure_field() -> FormField {
    FormField::new(
        "on_execution_failure",
        "Si una ejecución falla",
        FormKind::select(&[
            ("reschedule", "Reintentar en la próxima programación"),
            ("retry", "Reintentar enseguida"),
            ("pause", "Pausar el schedule"),
        ]),
    )
    .default(json!("reschedule"))
}

/// A schedule's own read-only fields, for an existing one.
fn sched_info(fields: &mut Vec<FormField>) {
    fields.push(FormField::new("ids", "Id", FormKind::Text).read_only(true));
    fields.push(FormField::new("owner", "Propietario", FormKind::Text).read_only(true));
    fields
        .push(FormField::new("next_run", "Próxima ejecución", FormKind::DateTime).read_only(true));
    fields.push(FormField::new("state", "Estado", FormKind::Text).read_only(true));
}

fn put_sched_info(values: &mut BTreeMap<String, Value>, ids: &str, s: &Sched) {
    values.insert("ids".into(), json!(ids));
    values.insert("owner".into(), json!(s.owner.clone().unwrap_or_default()));
    values.insert(
        "next_run".into(),
        json!(s.next_run.clone().unwrap_or_default()),
    );
    values.insert("state".into(), json!(s.state.clone().unwrap_or_default()));
}

// ---------------------------------------------------------------------------
// Backup schedules
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Encryption {
    None,
    Passphrase,
    Kms(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Full {
    /// CockroachDB picks it from the recurrence (new schedules only).
    Auto,
    Always,
    Cron(String),
}

/// A backup schedule (or pair) as one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Backup {
    pub(crate) label: String,
    pub(crate) what: What,
    pub(crate) locations: Vec<String>,
    pub(crate) revision_history: bool,
    pub(crate) encryption: Encryption,
    /// The passphrase typed in the form; `None`: keep the schedule's.
    pub(crate) passphrase: Option<String>,
    pub(crate) locality: Option<String>,
    pub(crate) incremental_location: Vec<String>,
    pub(crate) all_tenants: bool,
    pub(crate) metrics: bool,
    pub(crate) recurring: String,
    pub(crate) full: Full,
    pub(crate) first_run: Option<String>,
    pub(crate) previous: String,
    pub(crate) failure: String,
    pub(crate) ignore_existing: bool,
    pub(crate) enabled: bool,
}

fn truthy(v: &OptVal) -> bool {
    match v {
        OptVal::Flag => true,
        OptVal::One(x) => matches!(x.to_ascii_lowercase().as_str(), "true" | "on" | "1"),
        OptVal::List(_) => false,
    }
}

fn values_of(v: &OptVal) -> Vec<String> {
    match v {
        OptVal::Flag => Vec::new(),
        OptVal::One(x) => vec![x.clone()],
        OptVal::List(xs) => xs.clone(),
    }
}

/// The backup a schedule (or pair) runs; `None` when its statement doesn't
/// read as one.
pub(crate) fn backup_of(full: &Sched, inc: Option<&Sched>) -> Option<Backup> {
    let main = parse_backup(&inc.unwrap_or(full).statement()?)?;
    // Options from both: some show only on one of the pair.
    let mut options = parse_backup(&full.statement()?)
        .map(|f| f.options)
        .unwrap_or_default();
    for (n, v) in main.options.clone() {
        options.retain(|(m, _)| *m != n);
        options.push((n, v));
    }
    let get = |name: &str| options.iter().find(|(n, _)| n == name).map(|(_, v)| v);
    let encryption = if get("encryption_passphrase").is_some() {
        Encryption::Passphrase
    } else if let Some(k) = get("kms") {
        Encryption::Kms(values_of(k))
    } else {
        Encryption::None
    };
    let recurring = inc.unwrap_or(full).recurrence.clone().unwrap_or_default();
    Some(Backup {
        label: full.label.clone(),
        what: main.what,
        locations: main.locations,
        revision_history: get("revision_history").is_some_and(truthy),
        encryption,
        passphrase: None,
        locality: get("execution locality").and_then(|v| values_of(v).into_iter().next()),
        incremental_location: get("incremental_location")
            .map(values_of)
            .unwrap_or_default(),
        all_tenants: get("include_all_virtual_clusters")
            .or(get("include_all_secondary_tenants"))
            .is_some_and(truthy),
        metrics: get("updates_cluster_monitoring_metrics").is_some_and(truthy),
        recurring,
        full: match inc {
            Some(_) => Full::Cron(full.recurrence.clone().unwrap_or_default()),
            None => Full::Always,
        },
        first_run: None,
        previous: previous_of(full.previous.as_deref()).into(),
        failure: failure_of(full.failure.as_deref()).into(),
        ignore_existing: false,
        enabled: full.active,
    })
}

fn backup_fields(edit: bool) -> Vec<FormSection> {
    let mut general = Vec::new();
    if edit {
        sched_info(&mut general);
    }
    general.push(
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .help("El nombre del schedule (su label): DBine no deja que se repita."),
    );
    general.push(
        FormField::new("enabled", "Activo", FormKind::Bool)
            .default(json!(true))
            .help("En pausa, el schedule no hace backups hasta reanudarlo."),
    );
    let backup = FormSection::new("backup", "Backup").fields(vec![
        FormField::new(
            "backup_target",
            "Qué respalda",
            FormKind::select(&[
                ("cluster", "Todo el clúster"),
                ("database", "Bases de datos"),
                ("table", "Tablas"),
            ]),
        )
        .required()
        .default(json!("database")),
        FormField::new("databases", "Bases de datos", FormKind::reference("database", true))
            .when("backup_target", &["database"]),
        FormField::new("tables", "Tablas", FormKind::Textarea)
            .when("backup_target", &["table"])
            .placeholder("db.public.pedidos\ndb.ventas.*")
            .help("Una por línea: base.esquema.tabla, o base.* / base.esquema.* para todas las de una base o un esquema."),
        FormField::new("locations", "Ubicación", FormKind::Textarea)
            .required()
            .placeholder("s3://bucket/backups?AUTH=implicit")
            .help("La URI de la colección de backups (s3://, gs://, azure-blob://, nodelocal://…). Varias, una por línea, para un backup por localidad (con COCKROACH_LOCALITY). CockroachDB oculta sus credenciales al mostrarla."),
        FormField::new("revision_history", "Historial de revisiones", FormKind::Bool)
            .help("Guarda cada versión de los datos entre backups, para restaurar a cualquier momento."),
        FormField::new(
            "encryption",
            "Cifrado",
            FormKind::select(&[
                ("none", "Sin cifrar"),
                ("passphrase", "Con contraseña"),
                ("kms", "Con KMS"),
            ]),
        )
        .default(json!("none")),
        FormField::new("encryption_passphrase", "Contraseña de cifrado", FormKind::Secret)
            .when("encryption", &["passphrase"])
            .help("No se muestra: dejala vacía para mantener la que tiene."),
        FormField::new("kms", "URI del KMS", FormKind::Textarea)
            .when("encryption", &["kms"])
            .placeholder("aws-kms:///clave?AUTH=implicit&REGION=us-east-1")
            .help("Una por línea; varias para backups por localidad."),
        FormField::new("execution_locality", "Localidad de ejecución", FormKind::Text)
            .placeholder("region=us-east-1")
            .help("Solo los nodos con esta localidad ejecutan el backup."),
        FormField::new("incremental_location", "Ubicación de los incrementales", FormKind::Textarea)
            .help("Otra colección para los backups incrementales; vacía, la misma ubicación."),
        FormField::new(
            "include_all_virtual_clusters",
            "Incluir todos los clústeres virtuales",
            FormKind::Bool,
        )
        .when("backup_target", &["cluster"]),
        FormField::new(
            "updates_cluster_monitoring_metrics",
            "Actualizar las métricas de monitoreo",
            FormKind::Bool,
        )
        .help("Las métricas schedules.BACKUP.* del clúster siguen a este schedule."),
    ]);
    let full = if edit {
        FormKind::select(&[
            ("always", "Siempre completo"),
            ("cron", "Completo según su propia programación"),
        ])
    } else {
        FormKind::select(&[
            ("auto", "La que elija CockroachDB"),
            ("always", "Siempre completo"),
            ("cron", "Completo según su propia programación"),
        ])
    };
    let mut schedule = vec![
        FormField::new("recurring", "Frecuencia", FormKind::Cron)
            .required()
            .placeholder("@daily")
            .help("Cada cuánto se hace un backup (incremental, salvo que sean siempre completos). Una expresión cron de cinco campos o @hourly, @daily…; CockroachDB la lee en UTC."),
        FormField::new("full_backup", "Backup completo", full)
            .required()
            .default(json!(if edit { "always" } else { "auto" }))
            .help("Sin elegir, CockroachDB hace uno completo por día si la frecuencia es de una hora o menos, uno por semana si es de un día o menos, y si no, siempre completos."),
        FormField::new("full_cron", "Programación de los completos", FormKind::Cron)
            .when("full_backup", &["cron"])
            .placeholder("@weekly"),
    ];
    if !edit {
        schedule.push(
            FormField::new("first_run", "Primera ejecución", FormKind::DateTime)
                .help("Vacía: la que dé la frecuencia."),
        );
    }
    schedule.push(previous_field());
    schedule.push(failure_field());
    if !edit {
        schedule.push(
            FormField::new(
                "ignore_existing_backups",
                "Aceptar backups que ya estén en la ubicación",
                FormKind::Bool,
            )
            .help("CockroachDB no crea el schedule si la ubicación ya tiene backups, salvo con esta opción."),
        );
    }
    vec![
        FormSection::new("general", "General").fields(general),
        backup,
        FormSection::new("schedule", "Programación").fields(schedule),
    ]
}

fn backup_values(b: &Backup) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    let (target, dbs, tables) = match &b.what {
        What::Cluster => ("cluster", Vec::new(), String::new()),
        What::Databases(n) => (
            "database",
            n.iter().map(Name::display).collect(),
            String::new(),
        ),
        What::Tables(n) => (
            "table",
            Vec::new(),
            n.iter().map(Name::display).collect::<Vec<_>>().join("\n"),
        ),
    };
    m.insert("name".into(), json!(b.label));
    m.insert("enabled".into(), json!(b.enabled));
    m.insert("backup_target".into(), json!(target));
    m.insert("databases".into(), json!(dbs));
    m.insert("tables".into(), json!(tables));
    m.insert("locations".into(), json!(b.locations.join("\n")));
    m.insert("revision_history".into(), json!(b.revision_history));
    let (enc, kms) = match &b.encryption {
        Encryption::None => ("none", String::new()),
        Encryption::Passphrase => ("passphrase", String::new()),
        Encryption::Kms(k) => ("kms", k.join("\n")),
    };
    m.insert("encryption".into(), json!(enc));
    m.insert("encryption_passphrase".into(), Value::Null);
    m.insert("kms".into(), json!(kms));
    m.insert(
        "execution_locality".into(),
        json!(b.locality.clone().unwrap_or_default()),
    );
    m.insert(
        "incremental_location".into(),
        json!(b.incremental_location.join("\n")),
    );
    m.insert("include_all_virtual_clusters".into(), json!(b.all_tenants));
    m.insert(
        "updates_cluster_monitoring_metrics".into(),
        json!(b.metrics),
    );
    m.insert("recurring".into(), json!(b.recurring));
    let (full, cron) = match &b.full {
        Full::Auto => ("auto", String::new()),
        Full::Always => ("always", String::new()),
        Full::Cron(c) => ("cron", c.clone()),
    };
    m.insert("full_backup".into(), json!(full));
    m.insert("full_cron".into(), json!(cron));
    m.insert("on_previous_running".into(), json!(b.previous));
    m.insert("on_execution_failure".into(), json!(b.failure));
    m
}

/// The form's backup, checked. `cur`: the schedule being edited.
pub(crate) fn backup_spec(values: &FormValues, cur: Option<&Backup>) -> Result<Backup> {
    let v = Vals(&values.values);
    let label = name_of(v, "name", "Nombre")?;
    let what = match one_of(
        v,
        "backup_target",
        "Qué respalda",
        &["cluster", "database", "table"],
        "database",
    )?
    .as_str()
    {
        "cluster" => What::Cluster,
        "database" => {
            let names = v.strings("databases");
            if names.is_empty() {
                return Err(bad(
                    "Elegí al menos una base de datos para respaldar.".into()
                ));
            }
            What::Databases(
                names
                    .iter()
                    .map(|n| parse_name(n, false, "Bases de datos"))
                    .collect::<Result<_>>()?,
            )
        }
        _ => {
            let names = lines(v, "tables");
            if names.is_empty() {
                return Err(bad("Escribí al menos una tabla para respaldar.".into()));
            }
            What::Tables(
                names
                    .iter()
                    .map(|n| parse_name(n, true, "Tablas"))
                    .collect::<Result<_>>()?,
            )
        }
    };
    let locations: Vec<String> = lines(v, "locations")
        .iter()
        .map(|l| check_uri(l, "Ubicación"))
        .collect::<Result<_>>()?;
    if locations.is_empty() {
        return Err(bad("«Ubicación» no puede quedar vacía.".into()));
    }
    let encryption = match one_of(
        v,
        "encryption",
        "Cifrado",
        &["none", "passphrase", "kms"],
        "none",
    )?
    .as_str()
    {
        "passphrase" => Encryption::Passphrase,
        "kms" => {
            let k: Vec<String> = lines(v, "kms")
                .iter()
                .map(|l| check_uri(l, "URI del KMS"))
                .collect::<Result<_>>()?;
            if k.is_empty() {
                return Err(bad("«URI del KMS» no puede quedar vacía.".into()));
            }
            Encryption::Kms(k)
        }
        _ => Encryption::None,
    };
    let passphrase = v.secret("encryption_passphrase");
    if let Some(p) = &passphrase {
        if p.chars().any(char::is_control) {
            return Err(bad(
                "«Contraseña de cifrado» no puede tener saltos de línea ni caracteres de control."
                    .into(),
            ));
        }
    }
    if encryption == Encryption::Passphrase
        && passphrase.is_none()
        && cur.is_none_or(|c| c.encryption != Encryption::Passphrase)
    {
        return Err(bad("Escribí la contraseña de cifrado.".into()));
    }
    let locality = v.opt("execution_locality");
    if let Some(l) = &locality {
        plain(l, "Localidad de ejecución")?;
    }
    let incremental_location: Vec<String> = lines(v, "incremental_location")
        .iter()
        .map(|l| check_uri(l, "Ubicación de los incrementales"))
        .collect::<Result<_>>()?;
    let recurring = check_cron(&v.text("recurring"), "Frecuencia")?;
    let full = match one_of(
        v,
        "full_backup",
        "Backup completo",
        &["auto", "always", "cron"],
        if cur.is_some() { "always" } else { "auto" },
    )?
    .as_str()
    {
        "auto" if cur.is_some() => {
            return Err(bad(
                "«Backup completo»: elegí «Siempre completo» o una programación propia.".into(),
            ))
        }
        "auto" => Full::Auto,
        "always" => Full::Always,
        _ => Full::Cron(check_cron(
            &v.text("full_cron"),
            "Programación de los completos",
        )?),
    };
    let first_run = if cur.is_some() {
        None
    } else {
        datetime_field(v, "first_run", "Primera ejecución")?
    };
    Ok(Backup {
        label,
        what,
        locations,
        revision_history: v.bool("revision_history"),
        encryption,
        passphrase,
        locality,
        incremental_location,
        all_tenants: v.bool("include_all_virtual_clusters"),
        metrics: v.bool("updates_cluster_monitoring_metrics"),
        recurring,
        full,
        first_run,
        previous: one_of(
            v,
            "on_previous_running",
            "Si la ejecución anterior sigue corriendo",
            &["wait", "skip", "start"],
            "wait",
        )?,
        failure: one_of(
            v,
            "on_execution_failure",
            "Si una ejecución falla",
            &["reschedule", "retry", "pause"],
            "reschedule",
        )?,
        ignore_existing: cur.is_none() && v.bool("ignore_existing_backups"),
        enabled: v.bool_or("enabled", true),
    })
}

fn what_sql(w: &What) -> String {
    let list = |n: &[Name]| n.iter().map(Name::sql).collect::<Vec<_>>().join(", ");
    match w {
        What::Cluster => String::new(),
        What::Databases(n) => format!("DATABASE {} ", list(n)),
        What::Tables(n) => format!("TABLE {} ", list(n)),
    }
}

/// `CREATE SCHEDULE … FOR BACKUP …`. `ignore`: the location may have
/// backups already.
pub(crate) fn backup_create_sql(b: &Backup, ignore: bool, reveal: bool) -> String {
    let mut opts = Vec::new();
    if b.revision_history {
        opts.push("revision_history = true".to_string());
    }
    match &b.encryption {
        Encryption::Passphrase => opts.push(format!(
            "encryption_passphrase = {}",
            q(match (&b.passphrase, reveal) {
                (Some(p), true) => p,
                _ => SECRET_MASK,
            })
        )),
        Encryption::Kms(k) => opts.push(format!("kms = {}", locations_sql(k, reveal))),
        Encryption::None => {}
    }
    if let Some(l) = &b.locality {
        opts.push(format!("execution locality = {}", q(l)));
    }
    if !b.incremental_location.is_empty() {
        opts.push(format!(
            "incremental_location = {}",
            locations_sql(&b.incremental_location, reveal)
        ));
    }
    if b.all_tenants && b.what == What::Cluster {
        opts.push("include_all_virtual_clusters = true".to_string());
    }
    if b.metrics {
        opts.push("updates_cluster_monitoring_metrics = true".to_string());
    }
    let with = if opts.is_empty() {
        String::new()
    } else {
        format!(" WITH {}", opts.join(", "))
    };
    let full = match &b.full {
        Full::Auto => String::new(),
        Full::Always => " FULL BACKUP ALWAYS".to_string(),
        Full::Cron(c) => format!(" FULL BACKUP {}", q(c)),
    };
    let mut sched = Vec::new();
    if let Some(t) = &b.first_run {
        sched.push(format!("first_run = {}", q(t)));
    }
    sched.push(format!("on_previous_running = {}", q(&b.previous)));
    sched.push(format!("on_execution_failure = {}", q(&b.failure)));
    if ignore || b.ignore_existing {
        sched.push("ignore_existing_backups".to_string());
    }
    format!(
        "CREATE SCHEDULE {} FOR BACKUP {}INTO {}{with} RECURRING {}{full} WITH SCHEDULE OPTIONS {}",
        q(&b.label),
        what_sql(&b.what),
        locations_sql(&b.locations, reveal),
        q(&b.recurring),
        sched.join(", ")
    )
}

/// A script in one transaction (`hold`: DDL kept in it), comment first.
fn transaction(comment: &str, hold: Option<String>, statements: Vec<String>) -> String {
    let mut all = Vec::new();
    all.extend(hold);
    all.extend(statements);
    super::transaction(comment, &all)
}

fn pause_by_label(label: &str) -> String {
    format!(
        "PAUSE SCHEDULES SELECT id FROM [SHOW SCHEDULES] WHERE label = {}",
        q(label)
    )
}

pub(crate) fn backup_create(b: &Backup, reveal: bool) -> String {
    let mut st = vec![backup_create_sql(b, false, reveal)];
    if !b.enabled {
        st.push(pause_by_label(&b.label));
    }
    transaction(
        &format!("CockroachDB: nuevo backup programado «{}»", b.label),
        None,
        st,
    )
}

fn ids_list(ids: &[i64]) -> String {
    ids.iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The change of a backup schedule (or pair) `ids` (full first) in place,
/// or dropped and created again when ALTER can't make it; `None`: nothing
/// changed.
pub(crate) fn backup_alter(
    c: &Caps,
    ids: &[i64],
    waiting: bool,
    cur: &Backup,
    b: &Backup,
    reveal: bool,
) -> Result<Option<String>> {
    let id = ids[0];
    if b.what != cur.what || b.incremental_location != cur.incremental_location {
        if b.locations.iter().any(|l| redacted(l))
            || (b.encryption == Encryption::Passphrase && b.passphrase.is_none())
            || matches!(&b.encryption, Encryption::Kms(k) if k.iter().any(|l| redacted(l)))
            || b.incremental_location.iter().any(|l| redacted(l))
        {
            return Err(bad(SECRETS.into()));
        }
        let mut st = vec![format!(
            "DROP SCHEDULES SELECT id FROM [SHOW SCHEDULES] WHERE id IN ({})",
            ids_list(ids)
        )];
        st.push(backup_create_sql(b, true, reveal));
        if !b.enabled {
            st.push(pause_by_label(&b.label));
        }
        return Ok(Some(transaction(
            &format!(
                "CockroachDB: backup programado «{}»\n{BACKUP_RECREATE}",
                cur.label
            ),
            c.hold_ddl(),
            st,
        )));
    }
    let mut cmds = Vec::new();
    if b.label != cur.label {
        cmds.push(format!("SET LABEL {}", q(&b.label)));
    }
    if b.locations != cur.locations {
        if b.locations.iter().any(|l| redacted(l)) {
            return Err(bad(
                "«Ubicación»: escribila completa, con sus credenciales (CockroachDB las oculta al mostrarla).".into(),
            ));
        }
        cmds.push(format!("SET INTO {}", locations_sql(&b.locations, reveal)));
    }
    if b.revision_history != cur.revision_history {
        cmds.push(format!(
            "SET WITH revision_history = {}",
            b.revision_history
        ));
    }
    match (&cur.encryption, &b.encryption) {
        (Encryption::None, Encryption::None) => {}
        (_, Encryption::None) => {
            return Err(bad(
                "CockroachDB no le quita el cifrado a un backup programado: creá otro schedule sin cifrar.".into(),
            ))
        }
        (_, Encryption::Passphrase) => {
            if let Some(p) = &b.passphrase {
                cmds.push(format!(
                    "SET WITH encryption_passphrase = {}",
                    q(if reveal { p } else { SECRET_MASK })
                ));
            }
        }
        (old, Encryption::Kms(k)) => {
            if old != &b.encryption {
                if k.iter().any(|l| redacted(l)) {
                    return Err(bad(
                        "«URI del KMS»: escribila completa, con sus credenciales.".into(),
                    ));
                }
                cmds.push(format!("SET WITH kms = {}", locations_sql(k, reveal)));
            }
        }
    }
    if b.locality != cur.locality {
        match &b.locality {
            Some(l) => cmds.push(format!("SET WITH execution locality = {}", q(l))),
            None => {
                return Err(bad(
                    "CockroachDB no le quita la localidad de ejecución a un backup programado: escribí otra o creá otro schedule.".into(),
                ))
            }
        }
    }
    if b.metrics != cur.metrics {
        cmds.push(format!(
            "SET WITH updates_cluster_monitoring_metrics = {}",
            b.metrics
        ));
    }
    if b.all_tenants != cur.all_tenants && b.what == What::Cluster {
        cmds.push(format!(
            "SET WITH include_all_virtual_clusters = {}",
            b.all_tenants
        ));
    }
    if b.recurring != cur.recurring {
        cmds.push(format!("SET RECURRING {}", q(&b.recurring)));
    }
    if b.full != cur.full {
        cmds.push(match &b.full {
            Full::Cron(c) => format!("SET FULL BACKUP {}", q(c)),
            _ => "SET FULL BACKUP ALWAYS".to_string(),
        });
    }
    if b.previous != cur.previous {
        cmds.push(format!(
            "SET SCHEDULE OPTION on_previous_running = {}",
            q(&b.previous)
        ));
    }
    if b.failure != cur.failure {
        cmds.push(format!(
            "SET SCHEDULE OPTION on_execution_failure = {}",
            q(&b.failure)
        ));
    }
    let mut st = Vec::new();
    if !cmds.is_empty() {
        st.push(format!("ALTER BACKUP SCHEDULE {id} {}", cmds.join(", ")));
    }
    if b.enabled != cur.enabled {
        st.push(if b.enabled {
            // A pair's incremental waits for its first full backup.
            if waiting {
                format!("RESUME SCHEDULE {id}")
            } else {
                format!(
                    "RESUME SCHEDULES VALUES ({})",
                    ids.iter()
                        .map(i64::to_string)
                        .collect::<Vec<_>>()
                        .join("), (")
                )
            }
        } else {
            format!(
                "PAUSE SCHEDULES VALUES ({})",
                ids.iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join("), (")
            )
        });
    }
    if st.is_empty() {
        return Ok(None);
    }
    Ok(Some(transaction(
        &format!("CockroachDB: backup programado «{}»", cur.label),
        None,
        st,
    )))
}

// ---------------------------------------------------------------------------
// Changefeeds and changefeed schedules
// ---------------------------------------------------------------------------

/// A changefeed (scheduled or not) as the form asks for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Feed {
    pub(crate) label: String,
    pub(crate) targets: Vec<Target>,
    pub(crate) query: Option<String>,
    pub(crate) sink: String,
    pub(crate) options: Vec<Opt>,
    pub(crate) recurring: String,
    pub(crate) first_run: Option<String>,
    pub(crate) previous: String,
    pub(crate) failure: String,
    pub(crate) enabled: bool,
}

/// A changefeed schedule as the server has it.
pub(crate) fn feed_of_sched(s: &Sched) -> Option<Feed> {
    let st = parse_feed(&s.statement()?)?;
    Some(Feed {
        label: s.label.clone(),
        targets: st.targets,
        query: st.query,
        sink: st.sink.unwrap_or_default(),
        options: st.options,
        recurring: s.recurrence.clone().unwrap_or_default(),
        first_run: None,
        previous: previous_of(s.previous.as_deref()).into(),
        failure: failure_of(s.failure.as_deref()).into(),
        enabled: s.active,
    })
}

/// A query that is one `SELECT`: no `;` outside quotes and comments.
fn check_query(qy: &str) -> Result<String> {
    let t = qy.trim().trim_end_matches(';').trim();
    let starts = t
        .split_whitespace()
        .next()
        .is_some_and(|w| w.eq_ignore_ascii_case("SELECT"));
    let toks = lex(t);
    let one = toks
        .as_ref()
        .is_some_and(|ts| !ts.iter().any(|(tk, _)| *tk == Tok::Sym(';')));
    if !starts || !one {
        return Err(bad(
            "«Consulta»: tiene que ser una sola sentencia SELECT.".into()
        ));
    }
    Ok(t.to_string())
}

fn feed_fields(schedule: bool, edit: bool, query_mode: bool) -> Vec<FormSection> {
    let mut general = Vec::new();
    if edit && schedule {
        sched_info(&mut general);
    }
    if edit && !schedule {
        general.extend([
            FormField::new("ids", "Id", FormKind::Text).read_only(true),
            FormField::new("status", "Estado", FormKind::Text).read_only(true),
            FormField::new("owner", "Propietario", FormKind::Text).read_only(true),
            FormField::new("high_water", "Marca de agua", FormKind::Text)
                .read_only(true)
                .help("Hasta dónde emitió los cambios."),
            FormField::new("error", "Error", FormKind::Textarea).read_only(true),
        ]);
    }
    if schedule {
        general.push(
            FormField::new("name", "Nombre", FormKind::Text)
                .required()
                .help("El nombre del schedule (su label): DBine no deja que se repita."),
        );
    }
    general.push(
        FormField::new("enabled", "Activo", FormKind::Bool)
            .default(json!(true))
            .help(if schedule {
                "En pausa, el schedule no crea changefeeds hasta reanudarlo."
            } else {
                "En pausa, el changefeed no emite cambios hasta reanudarlo (y CockroachDB guarda los que haya)."
            }),
    );
    let feed = FormSection::new("changefeed", "Changefeed").fields(vec![
        FormField::new(
            "feed_mode",
            "Qué emite",
            FormKind::select(&[("tables", "Tablas"), ("query", "Una consulta (CDC)")]),
        )
        .default(json!("tables"))
        .read_only(edit),
        FormField::new("targets", "Tablas", FormKind::Textarea)
            .when("feed_mode", &["tables"])
            .placeholder("db.public.pedidos\ndb.public.clientes FAMILY datos")
            .help("Una por línea: base.esquema.tabla, y FAMILY familia para emitir una sola familia de columnas."),
        FormField::new("query", "Consulta", FormKind::code("sql"))
            .when("feed_mode", &["query"])
            .read_only(edit && query_mode)
            .help("Un SELECT sobre una tabla: qué columnas y filas emite."),
        FormField::new("sink", "Destino", FormKind::Text)
            .required()
            .placeholder("kafka://broker:9092?topic_prefix=dbine_")
            .help("La URI del sink (kafka://, s3://, gs://, webhook-https://, pubsub://…). CockroachDB oculta sus credenciales al mostrarla."),
        FormField::new("options", "Opciones", FormKind::Textarea)
            .placeholder("format = json\nresolved = 10s\ndiff")
            .help("Una por línea: nombre = valor, o solo el nombre de una opción sin valor (format, envelope, resolved, updated, diff, compression, key_in_value…)."),
    ]);
    let mut sections = vec![FormSection::new("general", "General").fields(general), feed];
    if schedule {
        let mut s = vec![FormField::new("recurring", "Frecuencia", FormKind::Cron)
            .required()
            .placeholder("@daily")
            .help("Cada cuánto exporta los datos (una expresión cron de cinco campos o @hourly, @daily…; en UTC). CockroachDB le agrega initial_scan = 'only': cada ejecución exporta la tabla una vez y termina.")];
        if !edit {
            s.push(
                FormField::new("first_run", "Primera ejecución", FormKind::DateTime)
                    .help("Vacía: la que dé la frecuencia."),
            );
        }
        s.push(previous_field());
        s.push(failure_field());
        sections.push(FormSection::new("schedule", "Programación").fields(s));
    }
    sections
}

fn feed_values(f: &Feed, schedule: bool) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    if schedule {
        m.insert("name".into(), json!(f.label));
        m.insert("recurring".into(), json!(f.recurring));
        m.insert("on_previous_running".into(), json!(f.previous));
        m.insert("on_execution_failure".into(), json!(f.failure));
    }
    m.insert("enabled".into(), json!(f.enabled));
    m.insert(
        "feed_mode".into(),
        json!(if f.query.is_some() { "query" } else { "tables" }),
    );
    m.insert(
        "targets".into(),
        json!(f
            .targets
            .iter()
            .map(Target::display)
            .collect::<Vec<_>>()
            .join("\n")),
    );
    m.insert("query".into(), json!(f.query.clone().unwrap_or_default()));
    m.insert("sink".into(), json!(f.sink));
    m.insert("options".into(), json!(options_text(&f.options)));
    m
}

/// The form's changefeed, checked.
pub(crate) fn feed_spec(values: &FormValues, schedule: bool, edit: bool) -> Result<Feed> {
    let v = Vals(&values.values);
    let label = if schedule {
        name_of(v, "name", "Nombre")?
    } else {
        String::new()
    };
    let (targets, query) =
        match one_of(v, "feed_mode", "Qué emite", &["tables", "query"], "tables")?.as_str() {
            "query" => (Vec::new(), Some(check_query(&v.raw("query"))?)),
            _ => {
                let t: Vec<Target> = lines(v, "targets")
                    .iter()
                    .map(|l| parse_target(l))
                    .collect::<Result<_>>()?;
                if t.is_empty() {
                    return Err(bad("Escribí al menos una tabla.".into()));
                }
                (t, None)
            }
        };
    let sink = v.opt("sink").ok_or_else(|| {
        bad("«Destino» no puede quedar vacío: un changefeed sin destino devuelve los cambios a la sesión que lo crea.".into())
    })?;
    let sink = check_uri(&sink, "Destino")?;
    let (recurring, first_run, previous, failure) = if schedule {
        (
            check_cron(&v.text("recurring"), "Frecuencia")?,
            if edit {
                None
            } else {
                datetime_field(v, "first_run", "Primera ejecución")?
            },
            one_of(
                v,
                "on_previous_running",
                "Si la ejecución anterior sigue corriendo",
                &["wait", "skip", "start"],
                "wait",
            )?,
            one_of(
                v,
                "on_execution_failure",
                "Si una ejecución falla",
                &["reschedule", "retry", "pause"],
                "reschedule",
            )?,
        )
    } else {
        (String::new(), None, String::new(), String::new())
    };
    Ok(Feed {
        label,
        targets,
        query,
        sink,
        options: parse_options(v, "options")?,
        recurring,
        first_run,
        previous,
        failure,
        enabled: v.bool_or("enabled", true),
    })
}

fn targets_sql(t: &[Target]) -> String {
    t.iter().map(Target::sql).collect::<Vec<_>>().join(", ")
}

/// `CREATE CHANGEFEED …` (`schedule`: `CREATE SCHEDULE … FOR CHANGEFEED …`).
pub(crate) fn feed_create_sql(f: &Feed, schedule: bool, reveal: bool) -> String {
    let with = if f.options.is_empty() {
        String::new()
    } else {
        format!(" WITH {}", options_sql(&f.options, reveal))
    };
    let body = match &f.query {
        Some(qy) => format!("INTO {}{with} AS {qy}", uri_sql(&f.sink, reveal)),
        None if schedule => format!(
            "{} INTO {}{with}",
            targets_sql(&f.targets),
            uri_sql(&f.sink, reveal)
        ),
        None => format!(
            "FOR {} INTO {}{with}",
            targets_sql(&f.targets),
            uri_sql(&f.sink, reveal)
        ),
    };
    if !schedule {
        return format!("CREATE CHANGEFEED {body}");
    }
    let mut sched = Vec::new();
    if let Some(t) = &f.first_run {
        sched.push(format!("first_run = {}", q(t)));
    }
    sched.push(format!("on_previous_running = {}", q(&f.previous)));
    sched.push(format!("on_execution_failure = {}", q(&f.failure)));
    format!(
        "CREATE SCHEDULE {} FOR CHANGEFEED {body} RECURRING {} WITH SCHEDULE OPTIONS {}",
        q(&f.label),
        q(&f.recurring),
        sched.join(", ")
    )
}

pub(crate) fn feed_sched_create(f: &Feed, reveal: bool) -> String {
    let mut st = vec![feed_create_sql(f, true, reveal)];
    if !f.enabled {
        st.push(pause_by_label(&f.label));
    }
    transaction(
        &format!("CockroachDB: nuevo changefeed programado «{}»", f.label),
        None,
        st,
    )
}

/// A changefeed schedule's change: pause or resume in place, anything else
/// drops and creates it again.
pub(crate) fn feed_sched_alter(
    c: &Caps,
    id: i64,
    cur: &Feed,
    f: &Feed,
    reveal: bool,
) -> Result<Option<String>> {
    let same = Feed {
        enabled: cur.enabled,
        ..f.clone()
    };
    if same == *cur {
        if f.enabled == cur.enabled {
            return Ok(None);
        }
        return Ok(Some(format!(
            "-- CockroachDB: changefeed programado «{}»\n{} SCHEDULE {id};\n",
            comment_text(&cur.label),
            if f.enabled { "RESUME" } else { "PAUSE" }
        )));
    }
    if redacted(&f.sink) {
        return Err(bad(
            "«Destino»: escribilo completo, con sus credenciales (CockroachDB las oculta al mostrarlo); hace falta para crear el schedule de nuevo.".into(),
        ));
    }
    let mut st = vec![
        format!("DROP SCHEDULE {id}"),
        feed_create_sql(f, true, reveal),
    ];
    if !f.enabled {
        st.push(pause_by_label(&f.label));
    }
    Ok(Some(transaction(
        &format!(
            "CockroachDB: changefeed programado «{}»\n{FEED_RECREATE}",
            cur.label
        ),
        c.hold_ddl(),
        st,
    )))
}

/// A changefeed as `SHOW CHANGEFEED JOB` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Running {
    pub(crate) id: i64,
    pub(crate) status: String,
    pub(crate) owner: Option<String>,
    pub(crate) high_water: Option<String>,
    pub(crate) error: Option<String>,
    pub(crate) feed: Feed,
}

impl Running {
    fn paused(&self) -> bool {
        self.status == "paused"
    }

    fn going(&self) -> bool {
        matches!(self.status.as_str(), "running" | "pending")
    }
}

async fn running(s: &PgSession, n: i64) -> Result<Running> {
    let r = s
        .text(&cj::changefeeds_sql(Some(n)))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| {
            bad(format!(
                "no existe el changefeed {n} (o tu usuario no lo ve)"
            ))
        })?;
    let status = text(&r, "status").unwrap_or_default();
    let stmt = text(&r, "description").unwrap_or_default();
    let st = parse_feed(&stmt).ok_or_else(|| {
        bad("DBine no puede leer la sentencia de este changefeed: se ve tal como está.".into())
    })?;
    // The sink as SHOW CHANGEFEED JOBS shows it: credentials redacted.
    let sink = text(&r, "sink_uri").or(st.sink).unwrap_or_default();
    Ok(Running {
        id: n,
        owner: text(&r, "user_name"),
        high_water: text(&r, "readable_high_water_timestamptz")
            .or_else(|| text(&r, "high_water_timestamp")),
        error: text(&r, "error"),
        feed: Feed {
            label: String::new(),
            targets: st.targets,
            query: st.query,
            sink,
            options: st.options,
            recurring: String::new(),
            first_run: None,
            previous: String::new(),
            failure: String::new(),
            enabled: matches!(status.as_str(), "running" | "pending"),
        },
        status,
    })
}

/// What a script does, step by step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Stage {
    /// SQL run as one simple query (a transaction, or one statement).
    Sql(String),
    /// `PAUSE JOB n` of a changefeed that was running, to change it: if a
    /// later stage fails, the save resumes it.
    PauseFor(i64),
    /// Wait until changefeed `n` is paused.
    WaitPaused(i64),
    /// Pause the changefeed the previous stage created.
    PauseCreated,
}

/// The stages of a changefeed's change, or `None` when nothing changed.
pub(crate) fn feed_alter(cur: &Running, f: &Feed, reveal: bool) -> Result<Option<Vec<Stage>>> {
    let n = cur.id;
    if f.query != cur.feed.query {
        return Err(bad(QUERY_FIXED.into()));
    }
    let mut parts = Vec::new();
    let added: Vec<&Target> = f
        .targets
        .iter()
        .filter(|t| !cur.feed.targets.contains(t))
        .collect();
    let dropped: Vec<&Target> = cur
        .feed
        .targets
        .iter()
        .filter(|t| !f.targets.contains(t))
        .collect();
    if !added.is_empty() {
        parts.push(format!(
            "ADD {}",
            added.iter().map(|t| t.sql()).collect::<Vec<_>>().join(", ")
        ));
    }
    if !dropped.is_empty() {
        parts.push(format!(
            "DROP {}",
            dropped
                .iter()
                .map(|t| t.sql())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let mut set: Vec<Opt> = f
        .options
        .iter()
        .filter(|o| !cur.feed.options.contains(o))
        .cloned()
        .collect();
    if f.sink != cur.feed.sink {
        if redacted(&f.sink) {
            return Err(bad(
                "«Destino»: escribilo completo, con sus credenciales (CockroachDB las oculta al mostrarlo).".into(),
            ));
        }
        set.insert(0, ("sink".into(), OptVal::One(f.sink.clone())));
    }
    if !set.is_empty() {
        parts.push(format!("SET {}", options_sql(&set, reveal)));
    }
    let unset: Vec<String> = cur
        .feed
        .options
        .iter()
        .filter(|(n, _)| !f.options.iter().any(|(m, _)| m == n))
        .map(|(n, _)| n.clone())
        .collect();
    if !unset.is_empty() {
        parts.push(format!("UNSET {}", unset.join(", ")));
    }
    let mut out = Vec::new();
    if parts.is_empty() {
        if f.enabled == cur.feed.enabled {
            return Ok(None);
        }
        out.push(Stage::Sql(format!(
            "{} JOB {n}",
            if f.enabled { "RESUME" } else { "PAUSE" }
        )));
        return Ok(Some(out));
    }
    if !cur.paused() {
        out.push(Stage::PauseFor(n));
        out.push(Stage::WaitPaused(n));
    }
    out.push(Stage::Sql(format!(
        "ALTER CHANGEFEED {n} {}",
        parts.join(" ")
    )));
    if f.enabled {
        out.push(Stage::Sql(format!("RESUME JOB {n}")));
    }
    Ok(Some(out))
}

/// The stages as a script to show.
pub(crate) fn render(comment: &str, stages: &[Stage]) -> String {
    let mut out = String::new();
    for line in comment.lines() {
        out.push_str(&format!("-- {}\n", comment_text(line)));
    }
    for st in stages {
        match st {
            Stage::Sql(sql) => {
                out.push_str(sql.trim_end());
                if !sql.trim_end().ends_with(';') {
                    out.push(';');
                }
                out.push('\n');
            }
            Stage::PauseFor(n) => out.push_str(&format!("PAUSE JOB {n};\n")),
            Stage::WaitPaused(n) => out.push_str(&format!(
                "-- DBine espera a que el changefeed {n} quede en pausa (ALTER CHANGEFEED lo pide).\n"
            )),
            Stage::PauseCreated => out.push_str(
                "-- Después, DBine pone en pausa el changefeed nuevo: PAUSE JOB <su id>;\n",
            ),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Row-level TTL
// ---------------------------------------------------------------------------

/// A table of the cluster.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Table {
    pub(crate) id: i64,
    pub(crate) db: String,
    pub(crate) schema: String,
    pub(crate) name: String,
}

impl Table {
    fn sql(&self) -> String {
        format!(
            "{}.{}.{}",
            ident(&self.db),
            ident(&self.schema),
            ident(&self.name)
        )
    }

    fn display(&self) -> String {
        Name(vec![
            Part::Ident(self.db.clone()),
            Part::Ident(self.schema.clone()),
            Part::Ident(self.name.clone()),
        ])
        .display()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Param {
    Interval,
    Text,
    Int,
    Bool,
}

/// The `ttl_*` storage parameters DBine edits, and since which version.
const TTL_PARAMS: &[(&str, Param, (u32, u32))] = &[
    ("ttl_expire_after", Param::Interval, (22, 2)),
    ("ttl_expiration_expression", Param::Text, (22, 2)),
    ("ttl_job_cron", Param::Text, (22, 2)),
    ("ttl_pause", Param::Bool, (22, 2)),
    ("ttl_select_batch_size", Param::Int, (22, 2)),
    ("ttl_delete_batch_size", Param::Int, (22, 2)),
    ("ttl_select_rate_limit", Param::Int, (24, 1)),
    ("ttl_delete_rate_limit", Param::Int, (22, 2)),
    ("ttl_row_stats_poll_interval", Param::Interval, (22, 2)),
    ("ttl_label_metrics", Param::Bool, (22, 2)),
    ("ttl_disable_changefeed_replication", Param::Bool, (24, 1)),
];

/// A table's TTL: parameter → value as text (no quotes, no casts).
pub(crate) type TtlParams = BTreeMap<String, String>;

/// `key=value` of `reloptions` (`ttl_expire_after='3 days':::INTERVAL`).
pub(crate) fn reloption(o: &str) -> Option<(String, String)> {
    let (k, v) = o.split_once('=')?;
    let v = v.trim();
    let value = if let Some(body) = v.strip_prefix('\'') {
        let mut out = String::new();
        let mut chars = body.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\'' {
                if chars.peek() == Some(&'\'') {
                    out.push('\'');
                    chars.next();
                } else {
                    break;
                }
            } else {
                out.push(c);
            }
        }
        out
    } else {
        v.split(":::").next().unwrap_or(v).to_string()
    };
    Some((k.trim().to_string(), value))
}

fn ttl_params(opts: &[String]) -> TtlParams {
    opts.iter()
        .filter_map(|o| reloption(o))
        .filter(|(k, _)| k.starts_with("ttl"))
        .collect()
}

/// The databases to look for TTL tables in.
async fn databases(s: &PgSession) -> Result<Vec<String>> {
    Ok(
        s.text("SELECT database_name FROM [SHOW DATABASES] ORDER BY 1")
            .await?
            .iter()
            .filter_map(|r| text(r, "database_name"))
            .filter(|d| d != "system")
            .collect(),
    )
}

/// Tables with a TTL (or the one with `id`), in every database.
async fn ttl_tables(s: &PgSession, id: Option<i64>) -> Result<Vec<(Table, TtlParams)>> {
    let dbs = databases(s).await?;
    if dbs.is_empty() {
        return Ok(Vec::new());
    }
    let filter = match id {
        Some(n) => format!("c.oid = {n}"),
        None => {
            "c.relkind = 'r' AND EXISTS (SELECT 1 FROM unnest(c.reloptions) o WHERE o LIKE 'ttl%')"
                .into()
        }
    };
    let sql = dbs
        .iter()
        .map(|db| {
            format!(
                "SELECT {} AS db, n.nspname AS schema, c.relname AS name, c.oid::STRING AS id,
                        to_json(c.reloptions)::STRING AS opts
                 FROM {d}.pg_catalog.pg_class c JOIN {d}.pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                 WHERE {filter}",
                q(db),
                d = ident(db)
            )
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let rows = s
        .text(&format!(
            "SELECT * FROM ({sql}) t ORDER BY db, schema, name"
        ))
        .await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let opts: Vec<String> = text(r, "opts")
                .and_then(|o| serde_json::from_str::<Option<Vec<String>>>(&o).ok())
                .flatten()
                .unwrap_or_default();
            Some((
                Table {
                    id: text(r, "id")?.parse().ok()?,
                    db: text(r, "db")?,
                    schema: text(r, "schema")?,
                    name: text(r, "name")?,
                },
                ttl_params(&opts),
            ))
        })
        .collect())
}

/// The TTL schedule of each table, when the user may see schedules.
async fn ttl_schedules(s: &PgSession, c: &Caps) -> Vec<Sched> {
    if !c.schedules {
        return Vec::new();
    }
    match schedules(s, "label LIKE 'row-level-ttl%'").await {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("cockroach: TTL schedules: {e}");
            Vec::new()
        }
    }
}

pub(crate) async fn ttl_items(s: &PgSession, c: &Caps) -> Result<SchedulerList> {
    let tables = ttl_tables(s, None).await?;
    let scheds = ttl_schedules(s, c).await;
    Ok(SchedulerList {
        items: tables
            .into_iter()
            .map(|(t, p)| {
                let sched = scheds.iter().find(|x| x.table_id() == Some(t.id));
                SchedulerItem {
                    id: t.id.to_string(),
                    name: t.display(),
                    description: Some(match (p.get("ttl_expire_after"), p.get("ttl_job_cron")) {
                        (Some(a), Some(c)) => format!("vencen a los {a} · {c}"),
                        (Some(a), None) => format!("vencen a los {a}"),
                        (None, Some(c)) => c.clone(),
                        (None, None) => "TTL".into(),
                    }),
                    enabled: Some(
                        p.get("ttl_pause").is_none_or(|v| v != "true")
                            && sched.is_none_or(|x| x.active),
                    ),
                    system: false,
                }
            })
            .collect(),
        notes: Vec::new(),
    })
}

fn ttl_fields(c: &Caps, edit: bool, sched: bool) -> Vec<FormSection> {
    let mut general = vec![if edit {
        FormField::new("table", "Tabla", FormKind::Text).read_only(true)
    } else {
        FormField::new("table", "Tabla", FormKind::reference("table", false))
            .required()
            .help("Una tabla de esta base sin TTL.")
    }];
    if edit && sched {
        general.push(FormField::new("schedule_id", "Schedule", FormKind::Text).read_only(true));
        general.push(
            FormField::new("enabled", "Schedule activo", FormKind::Bool)
                .help("En pausa, CockroachDB no borra las filas vencidas hasta reanudarlo."),
        );
    }
    let expire = FormSection::new("expire", "Vencimiento").fields(vec![
        FormField::new(
            "expire_mode",
            "Cuándo vence una fila",
            FormKind::select(&[
                ("after", "Un tiempo después de escribirla"),
                ("expression", "Según una expresión"),
            ]),
        )
        .default(json!("after")),
        FormField::new("ttl_expire_after", "Vence a los", FormKind::Text)
            .when("expire_mode", &["after"])
            .placeholder("30 days")
            .help("Un intervalo: CockroachDB agrega la columna oculta crdb_internal_expiration con la fecha de vencimiento de cada fila (y la borra si se quita)."),
        FormField::new("ttl_expiration_expression", "Expresión de vencimiento", FormKind::code("sql"))
            .when("expire_mode", &["expression"])
            .placeholder("created_at + INTERVAL '30 days'")
            .help("Una expresión TIMESTAMPTZ sobre las columnas de la fila."),
    ]);
    let schedule = FormSection::new("schedule", "Programación").fields(vec![
        FormField::new("ttl_job_cron", "Programación", FormKind::Cron)
            .placeholder("@hourly")
            .default(json!("@hourly"))
            .help("Cuándo corre el borrado (cron de cinco campos o @hourly, @daily…; en UTC)."),
        FormField::new("ttl_pause", "Borrado en pausa", FormKind::Bool)
            .help("ttl_pause: el job corre pero no borra filas."),
    ]);
    let mut batches = vec![
        FormField::new(
            "ttl_select_batch_size",
            "Filas leídas por lote",
            FormKind::number(Some(1.0), None),
        )
        .help("Vacío: el valor de CockroachDB (500)."),
        FormField::new(
            "ttl_delete_batch_size",
            "Filas borradas por lote",
            FormKind::number(Some(1.0), None),
        )
        .help("Vacío: el valor de CockroachDB (100)."),
    ];
    if c.version.is_none() || c.at_least((24, 1)) {
        batches.push(
            FormField::new(
                "ttl_select_rate_limit",
                "Filas leídas por segundo",
                FormKind::number(Some(0.0), None),
            )
            .help("Por nodo; 0 o vacío: sin límite."),
        );
    }
    batches.push(
        FormField::new(
            "ttl_delete_rate_limit",
            "Filas borradas por segundo",
            FormKind::number(Some(0.0), None),
        )
        .help("Por nodo; vacío: el valor de CockroachDB."),
    );
    batches.push(
        FormField::new("ttl_row_stats_poll_interval", "Intervalo de las estadísticas de filas", FormKind::Text)
            .placeholder("1 minute")
            .help("Cada cuánto cuenta las filas vencidas y las totales para las métricas; vacío: no las cuenta."),
    );
    batches.push(FormField::new(
        "ttl_label_metrics",
        "Métricas con el nombre de la tabla",
        FormKind::Bool,
    ));
    if c.version.is_none() || c.at_least((24, 1)) {
        batches.push(
            FormField::new(
                "ttl_disable_changefeed_replication",
                "Sin emitir los borrados a los changefeeds",
                FormKind::Bool,
            )
            .help("Los changefeeds de la tabla no reciben las filas que borra la TTL."),
        );
    }
    vec![
        FormSection::new("general", "General").fields(general),
        expire,
        schedule,
        FormSection::new("batches", "Lotes").fields(batches),
    ]
}

fn ttl_values(p: &TtlParams) -> BTreeMap<String, Value> {
    let mut m = BTreeMap::new();
    for (k, kind, _) in TTL_PARAMS {
        let v = p.get(*k);
        m.insert(
            k.to_string(),
            match kind {
                Param::Bool => json!(v.is_some_and(|x| x == "true")),
                Param::Int => v
                    .and_then(|x| x.parse::<i64>().ok())
                    .map_or(Value::Null, |n| json!(n)),
                _ => json!(v.cloned().unwrap_or_default()),
            },
        );
    }
    m.insert(
        "expire_mode".into(),
        json!(
            if p.contains_key("ttl_expiration_expression") && !p.contains_key("ttl_expire_after") {
                "expression"
            } else {
                "after"
            }
        ),
    );
    m
}

/// The form's TTL, checked: each parameter's value, `None` to leave it unset.
pub(crate) fn ttl_spec(values: &FormValues) -> Result<BTreeMap<String, Option<String>>> {
    let v = Vals(&values.values);
    let mode = one_of(
        v,
        "expire_mode",
        "Cuándo vence una fila",
        &["after", "expression"],
        "after",
    )?;
    let mut out = BTreeMap::new();
    for (k, kind, _) in TTL_PARAMS {
        let label = match *k {
            "ttl_expire_after" => "Vence a los",
            "ttl_expiration_expression" => "Expresión de vencimiento",
            "ttl_job_cron" => "Programación",
            "ttl_row_stats_poll_interval" => "Intervalo de las estadísticas de filas",
            "ttl_select_batch_size" => "Filas leídas por lote",
            "ttl_delete_batch_size" => "Filas borradas por lote",
            "ttl_select_rate_limit" => "Filas leídas por segundo",
            "ttl_delete_rate_limit" => "Filas borradas por segundo",
            _ => k,
        };
        let value = match kind {
            Param::Bool => v.bool(k).then(|| "true".to_string()),
            Param::Int => {
                // A batch has at least one row; a rate limit may be 0.
                let min = if k.ends_with("_batch_size") { 1 } else { 0 };
                v.int(k, label, min, i64::from(i32::MAX))?
                    .map(|n| n.to_string())
            }
            Param::Interval | Param::Text => {
                let t = v.opt(k);
                if let Some(t) = &t {
                    if *k != "ttl_expiration_expression" {
                        plain(t, label)?;
                    }
                }
                t
            }
        };
        out.insert(k.to_string(), value);
    }
    match mode.as_str() {
        "after" => {
            out.insert("ttl_expiration_expression".into(), None);
            if out.get("ttl_expire_after").is_none_or(Option::is_none) {
                return Err(bad("«Vence a los» no puede quedar vacío.".into()));
            }
        }
        _ => {
            out.insert("ttl_expire_after".into(), None);
            match out.get("ttl_expiration_expression").cloned().flatten() {
                None => {
                    return Err(bad(
                        "«Expresión de vencimiento» no puede quedar vacía.".into()
                    ))
                }
                Some(e) => {
                    if lex(&e).is_none_or(|t| t.iter().any(|(t, _)| *t == Tok::Sym(';'))) {
                        return Err(bad(
                            "«Expresión de vencimiento» tiene que ser una sola expresión.".into(),
                        ));
                    }
                }
            }
        }
    }
    if let Some(Some(cron)) = out.get("ttl_job_cron").cloned() {
        out.insert(
            "ttl_job_cron".into(),
            Some(check_cron(&cron, "Programación")?),
        );
    }
    Ok(out)
}

fn param_sql(k: &str, v: &str) -> String {
    let kind = TTL_PARAMS
        .iter()
        .find(|(n, _, _)| *n == k)
        .map_or(Param::Text, |(_, kind, _)| *kind);
    match kind {
        Param::Int => format!("{k} = {}", v.parse::<i64>().unwrap_or(0)),
        Param::Bool => format!("{k} = {}", v == "true"),
        _ => format!("{k} = {}", q(v)),
    }
}

/// The statements that take a table's TTL from `cur` to `new` (`cur`
/// empty: a new TTL). A value set first, the old way of expiring reset
/// after (the table always has one).
pub(crate) fn ttl_statements(
    t: &Table,
    cur: &TtlParams,
    new: &BTreeMap<String, Option<String>>,
) -> Vec<String> {
    let mut set = Vec::new();
    let mut reset = Vec::new();
    for (k, v) in new {
        match (cur.get(k), v) {
            (Some(a), Some(b)) if a == b => {}
            // Unset flags stay unset.
            (None, None) => {}
            (Some(_), None)
                if TTL_PARAMS
                    .iter()
                    .any(|(n, p, _)| n == k && *p == Param::Bool) =>
            {
                set.push(param_sql(k, "false"))
            }
            (_, Some(b)) => set.push(param_sql(k, b)),
            (Some(_), None) => reset.push(k.clone()),
        }
    }
    // A parameter set to false that the table never had is left out.
    set.retain(|s| !s.ends_with("= false") || cur.keys().any(|k| s.starts_with(&format!("{k} ="))));
    let mut out = Vec::new();
    if !set.is_empty() {
        out.push(format!("ALTER TABLE {} SET ({})", t.sql(), set.join(", ")));
    }
    if !reset.is_empty() {
        out.push(format!(
            "ALTER TABLE {} RESET ({})",
            t.sql(),
            reset.join(", ")
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// The cluster's own schedules
// ---------------------------------------------------------------------------

/// The cluster setting that holds a cluster schedule's recurrence.
pub(crate) fn setting_of(label: &str) -> Option<&'static str> {
    match label {
        "sql-stats-compaction" => Some("sql.stats.cleanup.recurrence"),
        "sql-schema-telemetry" => Some("sql.schema.telemetry.recurrence"),
        _ => None,
    }
}

/// The cluster setting's value, when the user may read it (the schedule
/// takes it on a later pass).
async fn setting_value(s: &PgSession, setting: Option<&str>) -> Option<String> {
    let setting = setting?;
    match s
        .client
        .simple_query(&format!("SHOW CLUSTER SETTING {setting}"))
        .await
    {
        Ok(m) => rows(m).first().and_then(|r| r.get(0).map(str::to_string)),
        Err(e) => {
            tracing::debug!("cockroach: {setting}: {e}");
            None
        }
    }
}

/// The statements for a cluster schedule; none when nothing changed.
/// `current`: the recurrence it has now.
pub(crate) fn system_statements(
    sched: &Sched,
    setting: Option<&str>,
    current: Option<&str>,
    recurring: Option<&str>,
    enabled: bool,
) -> Vec<String> {
    let mut out = Vec::new();
    if let (Some(setting), Some(r)) = (setting, recurring) {
        if Some(r) != current {
            out.push(format!("SET CLUSTER SETTING {setting} = {}", q(r)));
        }
    }
    if enabled != sched.active {
        out.push(format!(
            "{} SCHEDULE {}",
            if enabled { "RESUME" } else { "PAUSE" },
            sched.id
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Forms
// ---------------------------------------------------------------------------

async fn database_choices(s: &PgSession) -> Result<Vec<(String, String)>> {
    Ok(super::pairs(databases(s).await?))
}

/// The current database's tables without a TTL: (descriptor id, name).
async fn table_choices(s: &PgSession, c: &Caps) -> Result<Vec<(String, String)>> {
    let rows = s
        .text(
            "SELECT c.oid::STRING AS id, n.nspname AS schema, c.relname AS name
             FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relkind = 'r'
               AND n.nspname NOT IN ('pg_catalog', 'information_schema', 'crdb_internal', 'pg_extension')
               AND NOT EXISTS (SELECT 1 FROM unnest(c.reloptions) o WHERE o LIKE 'ttl%')
             ORDER BY 2, 3 LIMIT 5000",
        )
        .await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let t = Table {
                id: text(r, "id")?.parse().ok()?,
                db: c.database.clone(),
                schema: text(r, "schema")?,
                name: text(r, "name")?,
            };
            Some((t.id.to_string(), t.display()))
        })
        .collect())
}

pub(crate) async fn form(s: &PgSession, c: &Caps, obj: Obj) -> Result<ObjectForm> {
    match obj {
        Obj::New(Source::CrdbBackup) => {
            let mut f = ObjectForm {
                sections: backup_fields(false),
                notes: c.notes(),
                ..Default::default()
            };
            f.values.insert("enabled".into(), json!(true));
            f.values.insert("backup_target".into(), json!("database"));
            f.values
                .insert("databases".into(), json!(vec![c.database.clone()]));
            f.values.insert("full_backup".into(), json!("auto"));
            f.values.insert("encryption".into(), json!("none"));
            f.values.insert("on_previous_running".into(), json!("wait"));
            f.values
                .insert("on_execution_failure".into(), json!("reschedule"));
            f.choices
                .insert("database".into(), database_choices(s).await?);
            Ok(f)
        }
        Obj::New(source) => {
            let schedule = source == Source::CrdbChangefeed;
            let mut f = ObjectForm {
                sections: feed_fields(schedule, false, false),
                notes: c.notes(),
                ..Default::default()
            };
            f.values.insert("enabled".into(), json!(true));
            f.values.insert("feed_mode".into(), json!("tables"));
            if schedule {
                f.values.insert("on_previous_running".into(), json!("wait"));
                f.values
                    .insert("on_execution_failure".into(), json!("reschedule"));
            }
            Ok(f)
        }
        Obj::Schedule(n) => schedule_form(s, c, n).await,
        Obj::Feed(n) => {
            let r = running(s, n).await?;
            let mut f = ObjectForm {
                sections: feed_fields(false, true, r.feed.query.is_some()),
                values: feed_values(&r.feed, false),
                notes: vec![FEED_PAUSE.into()],
                ..Default::default()
            };
            f.values.insert("ids".into(), json!(n.to_string()));
            f.values.insert("status".into(), json!(r.status));
            f.values
                .insert("owner".into(), json!(r.owner.clone().unwrap_or_default()));
            f.values.insert(
                "high_water".into(),
                json!(r.high_water.clone().unwrap_or_default()),
            );
            f.values
                .insert("error".into(), json!(r.error.clone().unwrap_or_default()));
            if r.feed.query.is_some() {
                f.notes.push(QUERY_FIXED.into());
            }
            if !c.may_change(r.owner.as_deref()) {
                f.read_only = true;
                f.notes.insert(0, FEED_OTHERS.into());
            } else if !r.going() && !r.paused() {
                f.read_only = true;
                f.notes.insert(
                    0,
                    format!(
                        "El changefeed está «{}»: ya no se puede cambiar.",
                        comment_text(&r.status)
                    ),
                );
            }
            Ok(f)
        }
        Obj::Ttl(id) => ttl_form(s, c, id, None).await,
    }
}

async fn schedule_form(s: &PgSession, c: &Caps, n: i64) -> Result<ObjectForm> {
    let sched = read(s, n).await?;
    let mine = c.may_change(sched.owner.as_deref());
    let mut f = match sched.kind {
        ScheduleKind::Backup => {
            let (full, inc) = pair(s, sched.clone()).await?;
            let Some(b) = backup_of(&full, inc.as_ref()) else {
                return Ok(other_form(&sched));
            };
            let ids = match &inc {
                Some(i) => format!("{} (completo), {} (incremental)", full.id, i.id),
                None => full.id.to_string(),
            };
            let mut f = ObjectForm {
                sections: backup_fields(true),
                values: backup_values(&b),
                notes: vec![BACKUP_RECREATE.into()],
                ..Default::default()
            };
            put_sched_info(&mut f.values, &ids, &full);
            if inc.is_some() {
                f.notes.insert(0, PAIR.into());
            }
            if b.locations.iter().any(|l| redacted(l)) || b.encryption == Encryption::Passphrase {
                f.notes.push(SECRETS.into());
            }
            f.choices
                .insert("database".into(), database_choices(s).await?);
            f
        }
        ScheduleKind::Changefeed => {
            let Some(feed) = feed_of_sched(&sched) else {
                return Ok(other_form(&sched));
            };
            let mut f = ObjectForm {
                sections: feed_fields(true, true, false),
                values: feed_values(&feed, true),
                notes: vec![FEED_RECREATE.into()],
                ..Default::default()
            };
            put_sched_info(&mut f.values, &n.to_string(), &sched);
            f
        }
        ScheduleKind::Ttl => {
            let table = sched.table_id().ok_or_else(|| {
                bad("DBine no encuentra la tabla de este schedule de TTL.".into())
            })?;
            return ttl_form(s, c, Some(table), Some(&sched)).await;
        }
        ScheduleKind::System => {
            let setting = setting_of(&sched.label);
            let may_set = setting.is_some() && c.has("MODIFYCLUSTERSETTING");
            let mut general = Vec::new();
            sched_info(&mut general);
            general.push(FormField::new("name", "Nombre", FormKind::Text).read_only(true));
            general.push(
                FormField::new("setting", "Configuración del clúster", FormKind::Text)
                    .read_only(true),
            );
            general.push(
                FormField::new("recurring", "Programación", FormKind::Cron)
                    .read_only(!may_set)
                    .help("Se guarda con SET CLUSTER SETTING (en UTC); CockroachDB la pasa al schedule enseguida."),
            );
            general.push(FormField::new("enabled", "Activo", FormKind::Bool).read_only(!c.admin));
            let mut f = ObjectForm {
                sections: vec![FormSection::new("general", "General").fields(general)],
                notes: vec![SYSTEM.into()],
                ..Default::default()
            };
            put_sched_info(&mut f.values, &n.to_string(), &sched);
            f.values.insert("name".into(), json!(sched.label));
            f.values
                .insert("setting".into(), json!(setting.unwrap_or_default()));
            let current = setting_value(s, setting).await.or(sched.recurrence.clone());
            f.values
                .insert("recurring".into(), json!(current.unwrap_or_default()));
            f.values.insert("enabled".into(), json!(sched.active));
            if setting.is_none() {
                f.notes.push("DBine no sabe qué configuración del clúster tiene la programación de este schedule: solo se pausa y se reanuda.".into());
            } else if !may_set {
                f.notes.push("Cambiar su programación pide el privilegio MODIFYCLUSTERSETTING (o el rol admin).".into());
            }
            f.read_only = !c.admin && !may_set;
            return Ok(f);
        }
        ScheduleKind::Other => other_form(&sched),
    };
    if !mine {
        f.read_only = true;
        f.notes.insert(0, OTHERS.into());
    }
    Ok(f)
}

/// A schedule DBine can't edit: shown as it is.
fn other_form(sched: &Sched) -> ObjectForm {
    let mut general = Vec::new();
    sched_info(&mut general);
    general.push(FormField::new("name", "Nombre", FormKind::Text).read_only(true));
    general.push(FormField::new("recurring", "Programación", FormKind::Cron).read_only(true));
    general.push(FormField::new("statement", "Sentencia", FormKind::code("sql")).read_only(true));
    let mut f = ObjectForm {
        sections: vec![FormSection::new("general", "General").fields(general)],
        notes: vec![OTHER.into()],
        read_only: true,
        ..Default::default()
    };
    put_sched_info(&mut f.values, &sched.id.to_string(), sched);
    f.values.insert("name".into(), json!(sched.label));
    f.values.insert(
        "recurring".into(),
        json!(sched.recurrence.clone().unwrap_or_default()),
    );
    f.values.insert(
        "statement".into(),
        json!(sched
            .statement()
            .unwrap_or_else(|| sched.command.to_string())),
    );
    f
}

async fn ttl_table(s: &PgSession, id: i64) -> Result<(Table, TtlParams)> {
    ttl_tables(s, Some(id))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| bad(format!("no existe la tabla {id} (o tu usuario no la ve)")))
}

async fn ttl_form(
    s: &PgSession,
    c: &Caps,
    id: Option<i64>,
    sched: Option<&Sched>,
) -> Result<ObjectForm> {
    let Some(id) = id else {
        let mut f = ObjectForm {
            sections: ttl_fields(c, false, false),
            notes: c.notes(),
            ..Default::default()
        };
        f.values.insert("expire_mode".into(), json!("after"));
        f.values.insert("ttl_job_cron".into(), json!("@hourly"));
        f.choices.insert("table".into(), table_choices(s, c).await?);
        return Ok(f);
    };
    let (table, params) = ttl_table(s, id).await?;
    if params.is_empty() {
        return Err(bad(format!(
            "La tabla «{}» no tiene TTL.",
            shown(&table.display())
        )));
    }
    let scheds;
    let sched = match sched {
        Some(x) => Some(x),
        None => {
            scheds = ttl_schedules(s, c).await;
            scheds.iter().find(|x| x.table_id() == Some(id))
        }
    };
    let mut f = ObjectForm {
        sections: ttl_fields(c, true, sched.is_some()),
        values: ttl_values(&params),
        notes: vec![TTL_COMMITS.into()],
        ..Default::default()
    };
    f.values.insert("table".into(), json!(table.display()));
    if let Some(x) = sched {
        f.values
            .insert("schedule_id".into(), json!(x.id.to_string()));
        f.values.insert("enabled".into(), json!(x.active));
    }
    Ok(f)
}

/// A new job's form: every source's, with the scheduler to pick first.
pub(crate) async fn new_job_form(s: &PgSession, c: &Caps) -> Result<ObjectForm> {
    let mut forms = Vec::new();
    for source in sources(c) {
        forms.push((source, form(s, c, Obj::New(source)).await?));
    }
    if forms.is_empty() {
        return Err(Error::Unsupported(NO_SOURCE.into()));
    }
    Ok(combine(forms))
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

/// How a new object's id is learned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NewId {
    /// A query run after the script, answering `id`.
    Query(String, &'static str),
    /// The `job_id` the script's first stage answered.
    Job,
}

/// What a change runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CPlan {
    pub(crate) script: String,
    pub(crate) stages: Vec<Stage>,
    pub(crate) id: Option<String>,
    pub(crate) new_id: Option<NewId>,
}

impl CPlan {
    fn nothing(name: &str, id: String) -> CPlan {
        CPlan {
            script: format!("-- «{}»: no hay cambios que guardar.\n", comment_text(name)),
            stages: Vec::new(),
            id: Some(id),
            new_id: None,
        }
    }

    fn statements(
        comment: &str,
        st: Vec<String>,
        id: Option<String>,
        new_id: Option<NewId>,
    ) -> CPlan {
        let stages: Vec<Stage> = st.into_iter().map(Stage::Sql).collect();
        CPlan {
            script: render(comment, &stages),
            stages,
            id,
            new_id,
        }
    }
}

pub(crate) async fn plan(
    s: &PgSession,
    c: &Caps,
    obj: Obj,
    values: &FormValues,
    reveal: bool,
) -> Result<CPlan> {
    match obj {
        Obj::New(Source::CrdbBackup) => {
            let b = backup_spec(values, None)?;
            label_free(s, &b.label, &[]).await?;
            let script = backup_create(&b, reveal);
            Ok(CPlan {
                stages: vec![Stage::Sql(script.clone())],
                script,
                id: None,
                new_id: Some(NewId::Query(by_label(&b.label), SCHEDULE)),
            })
        }
        Obj::New(Source::CrdbChangefeed) => {
            let f = feed_spec(values, true, false)?;
            label_free(s, &f.label, &[]).await?;
            let script = feed_sched_create(&f, reveal);
            Ok(CPlan {
                stages: vec![Stage::Sql(script.clone())],
                script,
                id: None,
                new_id: Some(NewId::Query(by_label(&f.label), SCHEDULE)),
            })
        }
        Obj::New(_) => {
            let f = feed_spec(values, false, false)?;
            let mut stages = vec![Stage::Sql(feed_create_sql(&f, false, reveal))];
            if !f.enabled {
                stages.push(Stage::PauseCreated);
            }
            Ok(CPlan {
                script: render("CockroachDB: nuevo changefeed", &stages),
                stages,
                id: None,
                new_id: Some(NewId::Job),
            })
        }
        Obj::Feed(n) => {
            let cur = running(s, n).await?;
            if !c.may_change(cur.owner.as_deref()) {
                return Err(bad(FEED_OTHERS.into()));
            }
            let f = feed_spec(values, false, true)?;
            let id = format!("{FEED}{n}");
            match feed_alter(&cur, &f, reveal)? {
                None => Ok(CPlan::nothing(&format!("changefeed {n}"), id)),
                Some(stages) => Ok(CPlan {
                    script: render(&format!("CockroachDB: changefeed {n}"), &stages),
                    stages,
                    id: Some(id),
                    new_id: None,
                }),
            }
        }
        Obj::Schedule(n) => {
            let sched = read(s, n).await?;
            if sched.kind == ScheduleKind::Ttl {
                let table = sched.table_id().ok_or_else(|| {
                    bad("DBine no encuentra la tabla de este schedule de TTL.".into())
                })?;
                let mut p = ttl_plan(s, c, Some(table), Some(&sched), values).await?;
                p.id = Some(format!("{SCHEDULE}{n}"));
                return Ok(p);
            }
            if sched.kind == ScheduleKind::System {
                let v = Vals(&values.values);
                let setting = setting_of(&sched.label);
                let current = setting_value(s, setting).await.or(sched.recurrence.clone());
                let recurring = match v.opt("recurring") {
                    Some(r) if Some(r.as_str()) != current.as_deref() => {
                        if setting.is_none() {
                            return Err(bad("DBine no sabe qué configuración del clúster tiene la programación de este schedule.".into()));
                        }
                        Some(check_cron(&r, "Programación")?)
                    }
                    _ => None,
                };
                let st = system_statements(
                    &sched,
                    setting,
                    current.as_deref(),
                    recurring.as_deref(),
                    v.bool_or("enabled", sched.active),
                );
                let id = format!("{SCHEDULE}{n}");
                if st.is_empty() {
                    return Ok(CPlan::nothing(&sched.label, id));
                }
                return Ok(CPlan::statements(
                    &format!("CockroachDB: schedule del clúster «{}»\nSET CLUSTER SETTING no va en una transacción: cada sentencia se ejecuta sola.", sched.label),
                    st,
                    Some(id),
                    None,
                ));
            }
            if !c.may_change(sched.owner.as_deref()) {
                return Err(bad(OTHERS.into()));
            }
            match sched.kind {
                ScheduleKind::Backup => {
                    let (full, inc) = pair(s, sched).await?;
                    let cur = backup_of(&full, inc.as_ref()).ok_or_else(|| bad(OTHER.into()))?;
                    let b = backup_spec(values, Some(&cur))?;
                    let mut ids = vec![full.id];
                    ids.extend(inc.as_ref().map(|i| i.id));
                    if b.label != cur.label {
                        label_free(s, &b.label, &ids).await?;
                    }
                    let waiting = inc.as_ref().is_some_and(Sched::waiting);
                    let id = format!("{SCHEDULE}{}", full.id);
                    let recreate =
                        b.what != cur.what || b.incremental_location != cur.incremental_location;
                    match backup_alter(c, &ids, waiting, &cur, &b, reveal)? {
                        None => Ok(CPlan::nothing(&cur.label, id)),
                        Some(sql) => Ok(CPlan {
                            stages: vec![Stage::Sql(sql.clone())],
                            script: sql,
                            id: Some(id),
                            new_id: recreate.then(|| NewId::Query(by_label(&b.label), SCHEDULE)),
                        }),
                    }
                }
                ScheduleKind::Changefeed => {
                    let cur = feed_of_sched(&sched).ok_or_else(|| bad(OTHER.into()))?;
                    let f = feed_spec(values, true, true)?;
                    if f.label != cur.label {
                        label_free(s, &f.label, &[n]).await?;
                    }
                    let id = format!("{SCHEDULE}{n}");
                    let recreate = Feed {
                        enabled: cur.enabled,
                        ..f.clone()
                    } != cur;
                    match feed_sched_alter(c, n, &cur, &f, reveal)? {
                        None => Ok(CPlan::nothing(&cur.label, id)),
                        Some(sql) => Ok(CPlan {
                            stages: vec![Stage::Sql(sql.clone())],
                            script: sql,
                            id: Some(id),
                            new_id: recreate.then(|| NewId::Query(by_label(&f.label), SCHEDULE)),
                        }),
                    }
                }
                _ => Err(Error::Unsupported(
                    "DBine no cambia este tipo de schedule de CockroachDB".into(),
                )),
            }
        }
        Obj::Ttl(id) => ttl_plan(s, c, id, None, values).await,
    }
}

async fn ttl_plan(
    s: &PgSession,
    c: &Caps,
    id: Option<i64>,
    sched: Option<&Sched>,
    values: &FormValues,
) -> Result<CPlan> {
    let new = ttl_spec(values)?;
    let v = Vals(&values.values);
    let (table, cur) = match id {
        Some(n) => ttl_table(s, n).await?,
        None => {
            let picked = v
                .opt("table")
                .ok_or_else(|| bad("Elegí la tabla.".into()))?;
            let choices = table_choices(s, c).await?;
            let found = super::pick(&picked, &choices, |n| {
                format!("La tabla «{n}» no está en esta base, o ya tiene TTL.")
            })?
            .to_string();
            ttl_table(s, found.parse().unwrap_or(0)).await?
        }
    };
    let mut st = ttl_statements(&table, &cur, &new);
    if let Some(n) = id {
        let scheds;
        let sched = match sched {
            Some(x) => Some(x),
            None => {
                scheds = ttl_schedules(s, c).await;
                scheds.iter().find(|x| x.table_id() == Some(n))
            }
        };
        if let Some(x) = sched {
            let enabled = v.bool_or("enabled", x.active);
            if enabled != x.active {
                st.push(format!(
                    "{} SCHEDULE {}",
                    if enabled { "RESUME" } else { "PAUSE" },
                    x.id
                ));
            }
        }
    }
    let item = table.id.to_string();
    if st.is_empty() {
        return Ok(CPlan::nothing(&table.display(), item));
    }
    Ok(CPlan::statements(
        &format!("CockroachDB: TTL de «{}»\n{TTL_COMMITS}", table.display()),
        st,
        Some(item),
        None,
    ))
}

// ---------------------------------------------------------------------------
// Running a plan
// ---------------------------------------------------------------------------

/// Runs the plan's stages in order; answers the new or kept id.
pub(crate) async fn save(s: &mut PgSession, plan: CPlan) -> Result<Option<String>> {
    if plan.stages.is_empty() {
        return Ok(plan.id);
    }
    if matches!(
        Session::transaction_state(s).await?,
        Some(TxState::Open | TxState::Failed)
    ) {
        return Err(Error::State(super::OPEN_TRANSACTION.into()));
    }
    let mut job: Option<String> = None;
    // A running changefeed paused to change it.
    let mut paused: Option<i64> = None;
    for stage in &plan.stages {
        let r = run_stage(s, stage, job.as_deref()).await;
        s.discard_notices();
        match r {
            Ok(Some(j)) if job.is_none() => job = Some(j),
            Ok(_) => {}
            Err(e) => {
                return Err(match paused {
                    Some(n) => resume_after(s, n, e).await,
                    None => e,
                })
            }
        }
        if let Stage::PauseFor(n) = stage {
            paused = Some(*n);
        }
    }
    match plan.new_id {
        Some(NewId::Job) => Ok(job.map(|j| format!("{FEED}{j}")).or(plan.id)),
        Some(NewId::Query(sql, prefix)) => {
            let found = cj::guarded(s, &sql)
                .await?
                .and_then(|rows| rows.first().and_then(|r| text(r, "id")))
                .map(|n| format!("{prefix}{n}"));
            Ok(found.or(plan.id))
        }
        None => Ok(plan.id),
    }
}

/// A changefeed's status (`running`, `paused`…).
async fn feed_status(s: &PgSession, n: i64) -> Result<String> {
    Ok(
        s.text(&format!("SELECT status FROM [SHOW CHANGEFEED JOB {n}]"))
            .await?
            .first()
            .and_then(|r| text(r, "status"))
            .unwrap_or_default(),
    )
}

/// A save that failed after pausing changefeed `n` (running before) to
/// change it: the changefeed is resumed, as a best effort, and the error
/// says how it was left.
async fn resume_after(s: &mut PgSession, n: i64, e: Error) -> Error {
    if e.is_connect() {
        return e;
    }
    match feed_status(s, n).await.as_deref() {
        Ok("paused" | "pause-requested") => {}
        _ => return e,
    }
    let resumed = s.client.simple_query(&format!("RESUME JOB {n}")).await;
    s.discard_notices();
    let note = match resumed {
        Ok(_) => format!("DBine reanudó el changefeed {n}: sigue como estaba."),
        Err(r) => {
            tracing::debug!("cockroach: resume {n} after a failed change: {r}");
            format!("El changefeed {n} quedó en pausa: reanudalo con «Activo».")
        }
    };
    match e {
        Error::Query(m) => Error::Query(format!("{m}\n{note}")),
        other => {
            tracing::debug!("cockroach: changefeed {n}: {note}");
            other
        }
    }
}

/// One statement (or transaction) of a plan: the `job_id` it answered.
async fn run_sql(s: &mut PgSession, sql: &str) -> Result<Option<String>> {
    match s.client.simple_query(sql).await {
        Ok(msgs) => Ok(msgs.iter().find_map(|m| match m {
            SimpleQueryMessage::Row(r) => r
                .columns()
                .iter()
                .position(|c| c.name() == "job_id")
                .and_then(|i| r.get(i))
                .map(str::to_string),
            _ => None,
        })),
        Err(e) => {
            if let Err(r) = s.client.batch_execute("ROLLBACK").await {
                tracing::debug!("cockroach: scheduler script: rollback: {r}");
            }
            Err(match crate::err(e) {
                Error::Query(m) => Error::Query(redact(&m)),
                other => other,
            })
        }
    }
}

async fn run_stage(s: &mut PgSession, stage: &Stage, job: Option<&str>) -> Result<Option<String>> {
    match stage {
        Stage::Sql(sql) => run_sql(s, sql).await,
        Stage::PauseFor(n) => run_sql(s, &format!("PAUSE JOB {n}")).await,
        Stage::WaitPaused(n) => {
            let start = std::time::Instant::now();
            loop {
                let status = feed_status(s, *n).await?;
                match status.as_str() {
                    "paused" => return Ok(None),
                    "pause-requested" | "running" | "pending" => {}
                    other => {
                        return Err(bad(format!(
                            "El changefeed {n} quedó «{}» en vez de en pausa: no se cambió.",
                            comment_text(other)
                        )))
                    }
                }
                if start.elapsed() > PAUSE_WAIT {
                    return Err(bad(format!(
                        "El changefeed {n} no quedó en pausa en {} segundos: no se cambió.",
                        PAUSE_WAIT.as_secs()
                    )));
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
        Stage::PauseCreated => {
            let Some(j) = job.filter(|j| j.parse::<i64>().is_ok()) else {
                return Err(bad(
                    "CockroachDB no devolvió el id del changefeed nuevo: quedó activo.".into(),
                ));
            };
            s.text(&format!("PAUSE JOB {j}"))
                .await
                .map_err(|e| match e {
                    Error::Query(m) => Error::Query(redact(&m)),
                    other => other,
                })?;
            Ok(None)
        }
    }
}

// ---------------------------------------------------------------------------
// Deleting
// ---------------------------------------------------------------------------

pub(crate) async fn delete_script(s: &PgSession, c: &Caps, obj: Obj) -> Result<String> {
    match obj {
        Obj::New(_) => Err(bad("No hay nada que borrar.".into())),
        Obj::Feed(n) => {
            let r = running(s, n).await?;
            if !c.may_change(r.owner.as_deref()) {
                return Err(bad(FEED_OTHERS.into()));
            }
            Ok(format!(
                "-- CockroachDB: cancelar el changefeed {n} (deja de emitir cambios; no se puede reanudar)\nCANCEL JOB {n};\n"
            ))
        }
        Obj::Ttl(Some(n)) => ttl_delete(s, n).await,
        Obj::Ttl(None) => Err(bad("No hay nada que borrar.".into())),
        Obj::Schedule(n) => {
            let sched = read(s, n).await?;
            match sched.kind {
                ScheduleKind::Ttl => {
                    let table = sched.table_id().ok_or_else(|| {
                        bad("DBine no encuentra la tabla de este schedule de TTL.".into())
                    })?;
                    ttl_delete(s, table).await
                }
                ScheduleKind::System => Err(Error::Unsupported(
                    "los schedules del clúster no se borran".into(),
                )),
                _ if !c.may_change(sched.owner.as_deref()) => Err(bad(OTHERS.into())),
                ScheduleKind::Backup => {
                    let (full, inc) = pair(s, sched).await?;
                    let mut ids = vec![full.id];
                    ids.extend(inc.as_ref().map(|i| i.id));
                    Ok(format!(
                        "-- CockroachDB: borrar el backup programado «{}»{} (los backups hechos quedan en su ubicación)\nDROP SCHEDULES SELECT id FROM [SHOW SCHEDULES] WHERE id IN ({});\n",
                        comment_text(&full.label),
                        if inc.is_some() { ", sus dos schedules" } else { "" },
                        ids_list(&ids)
                    ))
                }
                _ => Ok(format!(
                    "-- CockroachDB: borrar el schedule «{}»\nDROP SCHEDULE {n};\n",
                    comment_text(&sched.label)
                )),
            }
        }
    }
}

async fn ttl_delete(s: &PgSession, n: i64) -> Result<String> {
    let (table, _) = ttl_table(s, n).await?;
    Ok(format!(
        "-- CockroachDB: quitar la TTL de «{}» (y su schedule; con «Vence a los», también la columna crdb_internal_expiration)\nALTER TABLE {} RESET (ttl);\n",
        comment_text(&table.display()),
        table.sql()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::fixture;

    fn root() -> Caps {
        Caps {
            user: "root".into(),
            admin: true,
            version: Some((26, 3)),
            database: "defaultdb".into(),
            schedules: true,
            privileges: Vec::new(),
            granted: None,
        }
    }

    fn fv(v: Value) -> FormValues {
        FormValues {
            values: serde_json::from_value(v).unwrap(),
            lists: BTreeMap::new(),
        }
    }

    fn backup() -> Backup {
        Backup {
            label: "nightly".into(),
            what: What::Databases(vec![Name(vec![Part::Ident("app".into())])]),
            locations: vec!["s3://b/p?AUTH=implicit".into()],
            revision_history: false,
            encryption: Encryption::None,
            passphrase: None,
            locality: None,
            incremental_location: Vec::new(),
            all_tenants: false,
            metrics: false,
            recurring: "@daily".into(),
            full: Full::Cron("@weekly".into()),
            first_run: None,
            previous: "wait".into(),
            failure: "reschedule".into(),
            ignore_existing: false,
            enabled: true,
        }
    }

    #[test]
    fn backup_statements_read_whole() {
        let b = parse_backup("BACKUP DATABASE app, \"Other DB\" INTO LATEST IN 'nodelocal://1/x' WITH OPTIONS (revision_history = true, encryption_passphrase = '*****', detached, execution locality = 'region=us', kms = ('aws:///a', 'aws:///b'))").unwrap();
        assert_eq!(
            b.what,
            What::Databases(vec![
                Name(vec![Part::Ident("app".into())]),
                Name(vec![Part::Ident("Other DB".into())]),
            ])
        );
        assert_eq!(b.locations, ["nodelocal://1/x"]);
        let get = |n: &str| {
            b.options
                .iter()
                .find(|(k, _)| k == n)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("revision_history"), Some(OptVal::One("true".into())));
        assert_eq!(get("detached"), Some(OptVal::Flag));
        assert_eq!(
            get("execution locality"),
            Some(OptVal::One("region=us".into()))
        );
        assert_eq!(
            get("kms"),
            Some(OptVal::List(vec!["aws:///a".into(), "aws:///b".into()]))
        );
        let t = parse_backup("BACKUP TABLE db.public.t, db.* INTO ('s3://a?COCKROACH_LOCALITY=default', 's3://b?COCKROACH_LOCALITY=region%3Dus') WITH OPTIONS (detached)").unwrap();
        match &t.what {
            What::Tables(n) => {
                assert_eq!(n[0].display(), "db.public.t");
                assert_eq!(n[1].display(), "db.*");
                assert_eq!(n[1].sql(), "\"db\".*");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(t.locations.len(), 2);
        let c = parse_backup("BACKUP INTO 'gs://b/c' WITH OPTIONS (detached)").unwrap();
        assert_eq!(c.what, What::Cluster);
        assert!(parse_backup("SELECT 1").is_none());
        assert!(parse_backup("BACKUP INTO 'unterminated").is_none());
    }

    #[test]
    fn changefeed_statements_read_whole() {
        let f = parse_feed("CREATE CHANGEFEED FOR TABLE d.public.t, TABLE d.public.u FAMILY f1 INTO 'kafka://k:9092' WITH OPTIONS (format = 'json', diff, resolved = '10s')").unwrap();
        assert_eq!(f.targets.len(), 2);
        assert_eq!(f.targets[1].display(), "d.public.u FAMILY f1");
        assert_eq!(
            f.targets[1].sql(),
            "TABLE \"d\".\"public\".\"u\" FAMILY \"f1\""
        );
        assert_eq!(f.sink.as_deref(), Some("kafka://k:9092"));
        assert_eq!(
            options_text(&f.options),
            "format = json\ndiff\nresolved = 10s"
        );
        assert!(f.query.is_none());
        let q = parse_feed("CREATE CHANGEFEED INTO 'nodelocal://1/q' WITH OPTIONS (format = 'csv', initial_scan = 'only') AS SELECT id FROM d.public.t WHERE id > 1").unwrap();
        assert_eq!(
            q.query.as_deref(),
            Some("SELECT id FROM d.public.t WHERE id > 1")
        );
        assert!(q.targets.is_empty());
    }

    #[test]
    fn names_typed_are_parsed_and_quoted() {
        let n = parse_name("App.Public.\"Mixed Case\"", false, "Tablas").unwrap();
        assert_eq!(n.sql(), "\"app\".\"public\".\"Mixed Case\"");
        assert_eq!(n.display(), "app.public.\"Mixed Case\"");
        assert_eq!(
            parse_name("db.*", true, "Tablas").unwrap().sql(),
            "\"db\".*"
        );
        for bad in [
            "t; DROP TABLE x",
            "a..b",
            "\"open",
            "a.b'c",
            "a\nb",
            "*",
            "db.*.t",
            "",
        ] {
            assert!(parse_name(bad, true, "Tablas").is_err(), "{bad:?}");
        }
        assert!(parse_name("db.*", false, "Bases de datos").is_err());
        let t = parse_target("TABLE d.s.t FAMILY \"F\"").unwrap();
        assert_eq!(t.sql(), "TABLE \"d\".\"s\".\"t\" FAMILY \"F\"");
        assert!(parse_target("d.s.t FAMILY").is_err());
        assert!(parse_target("d.s.t, other").is_err());
    }

    #[test]
    fn credentials_are_masked() {
        assert_eq!(
            mask_uri("s3://b/p?AWS_ACCESS_KEY_ID=AKID&AWS_SECRET_ACCESS_KEY=s3cr3t&AWS_SESSION_TOKEN=tok"),
            "s3://b/p?AWS_ACCESS_KEY_ID=AKID&AWS_SECRET_ACCESS_KEY=********&AWS_SESSION_TOKEN=********"
        );
        assert_eq!(
            mask_uri("kafka://user:pa55@k:9092/x?sasl_password=pw&topic_prefix=a"),
            "kafka://user:********@k:9092/x?sasl_password=********&topic_prefix=a"
        );
        assert_eq!(
            mask_uri("azure-blob://c/p?AZURE_ACCOUNT_NAME=a&AZURE_ACCOUNT_KEY=k"),
            "azure-blob://c/p?AZURE_ACCOUNT_NAME=a&AZURE_ACCOUNT_KEY=********"
        );
        assert_eq!(mask_uri("nodelocal://1/x"), "nodelocal://1/x");
        let e = redact("unexpected error in s3://bucket/path?AWS_SECRET_ACCESS_KEY=supersecret: failed, also password=hunter2");
        assert!(!e.contains("supersecret") && !e.contains("hunter2"), "{e}");
        assert!(
            e.contains("s3://bucket/path?AWS_SECRET_ACCESS_KEY=********"),
            "{e}"
        );
        assert!(redacted("s3://b?AWS_SECRET_ACCESS_KEY=redacted") && redacted("'*****'"));
        // A webhook's Authorization header is a credential too.
        let opts = vec![
            (
                "webhook_auth_header".to_string(),
                OptVal::One("Bearer t0k3n".into()),
            ),
            ("format".to_string(), OptVal::One("json".into())),
        ];
        let shown = options_sql(&opts, false);
        assert_eq!(
            shown,
            format!("webhook_auth_header = '{SECRET_MASK}', format = 'json'")
        );
        assert!(options_sql(&opts, true).contains("'Bearer t0k3n'"));
        assert!(!redacted("s3://b?AUTH=implicit"));
    }

    #[test]
    fn crons_and_options_are_checked() {
        for ok in ["@daily", "0 3 * * *", "*/10 * * * 1-5", "  @hourly "] {
            assert!(check_cron(ok, "Frecuencia").is_ok(), "{ok}");
        }
        for bad in [
            "",
            "@",
            "0 3 * *",
            "0 3 * * * *",
            "0 3 * * '; DROP",
            "@daily x",
        ] {
            assert!(check_cron(bad, "Frecuencia").is_err(), "{bad}");
        }
        let v = fv(json!({"options": "Format = json\n diff \nresolved = '10s'\nkey = 'it''s'"}));
        let o = parse_options(Vals(&v.values), "options").unwrap();
        assert_eq!(
            o,
            vec![
                ("format".into(), OptVal::One("json".into())),
                ("diff".into(), OptVal::Flag),
                ("resolved".into(), OptVal::One("10s".into())),
                ("key".into(), OptVal::One("it's".into())),
            ]
        );
        assert_eq!(
            options_sql(&o, true),
            "format = 'json', diff, resolved = '10s', key = 'it''s'"
        );
        for bad in ["a b = 1", "x) = 1", "dup\ndup", "1x = 2"] {
            let v = fv(json!({ "options": bad }));
            assert!(parse_options(Vals(&v.values), "options").is_err(), "{bad}");
        }
        let secret = vec![(
            "confluent_schema_registry".to_string(),
            OptVal::One("https://u:p@reg?api_secret=x".into()),
        )];
        assert!(!options_sql(&secret, false).contains(":p@"));
        let pw = vec![("sasl_password".to_string(), OptVal::One("x".into()))];
        assert_eq!(options_sql(&pw, false), "sasl_password = '********'");
        assert!(check_query("SELECT a FROM t WHERE b = ';'").is_ok());
        for bad in ["SELECT 1; DROP TABLE t", "DELETE FROM t", "", "SELECT 'x"] {
            assert!(check_query(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_new_backup_schedule() {
        let mut b = backup();
        b.revision_history = true;
        b.encryption = Encryption::Passphrase;
        b.passphrase = Some("p'w".into());
        b.locality = Some("region=us-east1".into());
        b.metrics = true;
        b.first_run = Some("2031-01-01T00:00:00Z".into());
        b.previous = "skip".into();
        let shown = backup_create_sql(&b, false, false);
        assert_eq!(
            shown,
            "CREATE SCHEDULE 'nightly' FOR BACKUP DATABASE \"app\" INTO 's3://b/p?AUTH=implicit' WITH revision_history = true, encryption_passphrase = '********', execution locality = 'region=us-east1', updates_cluster_monitoring_metrics = true RECURRING '@daily' FULL BACKUP '@weekly' WITH SCHEDULE OPTIONS first_run = '2031-01-01T00:00:00Z', on_previous_running = 'skip', on_execution_failure = 'reschedule'"
        );
        assert!(backup_create_sql(&b, false, true).contains("encryption_passphrase = 'p''w'"));
        let mut c = backup();
        c.what = What::Cluster;
        c.full = Full::Always;
        c.all_tenants = true;
        c.encryption =
            Encryption::Kms(vec!["aws-kms:///k?AUTH=implicit".into(), "gs:///k2".into()]);
        c.incremental_location = vec!["s3://inc".into()];
        c.locations = vec!["s3://a?AWS_SECRET_ACCESS_KEY=x".into(), "s3://b".into()];
        c.ignore_existing = true;
        let sql = backup_create_sql(&c, false, false);
        assert!(
            sql.contains("FOR BACKUP INTO ('s3://a?AWS_SECRET_ACCESS_KEY=********', 's3://b')"),
            "{sql}"
        );
        assert!(
            sql.contains("kms = ('aws-kms:///k?AUTH=implicit', 'gs:///k2')"),
            "{sql}"
        );
        assert!(sql.contains("include_all_virtual_clusters = true"), "{sql}");
        assert!(
            sql.contains("FULL BACKUP ALWAYS") && sql.ends_with("ignore_existing_backups"),
            "{sql}"
        );
        let mut a = backup();
        a.full = Full::Auto;
        a.enabled = false;
        let script = backup_create(&a, true);
        assert!(!script.contains("FULL BACKUP"), "{script}");
        assert!(
            script.contains(
                "PAUSE SCHEDULES SELECT id FROM [SHOW SCHEDULES] WHERE label = 'nightly';"
            ),
            "{script}"
        );
        assert!(script.starts_with("-- CockroachDB: nuevo backup programado «nightly»\nBEGIN;\n"));
    }

    #[test]
    fn a_backup_schedule_changes_in_place() {
        let c = root();
        let cur = backup();
        assert_eq!(
            backup_alter(&c, &[1, 2], false, &cur, &cur, false).unwrap(),
            None
        );
        let mut b = cur.clone();
        b.label = "n2".into();
        b.locations = vec!["s3://other".into()];
        b.revision_history = true;
        b.recurring = "@hourly".into();
        b.full = Full::Always;
        b.previous = "start".into();
        b.failure = "pause".into();
        b.metrics = true;
        b.locality = Some("region=eu".into());
        let sql = backup_alter(&c, &[1, 2], false, &cur, &b, false)
            .unwrap()
            .unwrap();
        assert!(sql.contains("ALTER BACKUP SCHEDULE 1 SET LABEL 'n2', SET INTO 's3://other', SET WITH revision_history = true, SET WITH execution locality = 'region=eu', SET WITH updates_cluster_monitoring_metrics = true, SET RECURRING '@hourly', SET FULL BACKUP ALWAYS, SET SCHEDULE OPTION on_previous_running = 'start', SET SCHEDULE OPTION on_execution_failure = 'pause';"), "{sql}");
        assert!(!sql.contains("autocommit_before_ddl"));
        // Paused: both; resumed: the incremental only when it isn't
        // waiting for its first full backup.
        let off = Backup {
            enabled: false,
            ..cur.clone()
        };
        let p = backup_alter(&c, &[1, 2], false, &cur, &off, false)
            .unwrap()
            .unwrap();
        assert!(p.contains("PAUSE SCHEDULES VALUES (1), (2);"), "{p}");
        let on = backup_alter(&c, &[1, 2], true, &off, &cur, false)
            .unwrap()
            .unwrap();
        assert!(
            on.contains("RESUME SCHEDULE 1;") && !on.contains("(2)"),
            "{on}"
        );
        let both = backup_alter(&c, &[1, 2], false, &off, &cur, false)
            .unwrap()
            .unwrap();
        assert!(both.contains("RESUME SCHEDULES VALUES (1), (2);"), "{both}");
        // A new passphrase, masked in the script shown.
        let enc = Backup {
            encryption: Encryption::Passphrase,
            passphrase: Some("x".into()),
            ..cur.clone()
        };
        let e = backup_alter(&c, &[1], false, &cur, &enc, false)
            .unwrap()
            .unwrap();
        assert!(
            e.contains("SET WITH encryption_passphrase = '********'"),
            "{e}"
        );
        // Encryption isn't removed, a locality isn't cleared.
        assert!(backup_alter(
            &c,
            &[1],
            false,
            &Backup {
                encryption: Encryption::Passphrase,
                ..cur.clone()
            },
            &cur,
            false
        )
        .is_err());
        assert!(backup_alter(
            &c,
            &[1],
            false,
            &Backup {
                locality: Some("x".into()),
                ..cur.clone()
            },
            &cur,
            false
        )
        .is_err());
        // A redacted location can't be written back.
        let red = Backup {
            locations: vec!["s3://b?AWS_SECRET_ACCESS_KEY=redacted&x=1".into()],
            ..cur.clone()
        };
        assert!(backup_alter(&c, &[1], false, &cur, &red, false).is_err());
    }

    #[test]
    fn what_a_backup_backs_up_recreates_it() {
        let c = root();
        let cur = backup();
        let mut b = cur.clone();
        b.what = What::Tables(vec![parse_name("app.public.t", true, "Tablas").unwrap()]);
        let sql = backup_alter(&c, &[1, 2], false, &cur, &b, false)
            .unwrap()
            .unwrap();
        assert!(
            sql.contains("ALTER BACKUP SCHEDULE no cambia qué se respalda"),
            "{sql}"
        );
        assert!(sql.contains("BEGIN;\nSET LOCAL autocommit_before_ddl = false;\nDROP SCHEDULES SELECT id FROM [SHOW SCHEDULES] WHERE id IN (1, 2);\nCREATE SCHEDULE 'nightly' FOR BACKUP TABLE \"app\".\"public\".\"t\" INTO"), "{sql}");
        assert!(sql.contains("ignore_existing_backups;\nCOMMIT;"), "{sql}");
        // Before 24.3 there's no autocommit_before_ddl.
        let old = Caps {
            version: Some((23, 2)),
            ..root()
        };
        assert!(!backup_alter(&old, &[1], false, &cur, &b, false)
            .unwrap()
            .unwrap()
            .contains("autocommit_before_ddl"));
        // Secrets the server hides have to be typed again.
        let mut red = b.clone();
        red.locations = vec!["s3://b?AWS_SECRET_ACCESS_KEY=redacted".into()];
        assert!(backup_alter(&c, &[1], false, &cur, &red, false)
            .unwrap_err()
            .to_string()
            .contains("escribirlas"));
        let mut pw = b.clone();
        pw.encryption = Encryption::Passphrase;
        let cur_pw = Backup {
            encryption: Encryption::Passphrase,
            ..cur.clone()
        };
        assert!(backup_alter(&c, &[1], false, &cur_pw, &pw, false).is_err());
    }

    #[test]
    fn a_backup_pair_reads_as_one() {
        let row = |id: &str, rec: &str, cmd: &str, status: &str, prev: &str| {
            fixture(&[
                ("sid", id),
                ("label", "nightly"),
                ("schedule_status", status),
                ("recurrence", rec),
                ("owner", "root"),
                ("on_previous_running", prev),
                ("on_execution_failure", "PAUSE_SCHED"),
                ("command", cmd),
            ])
        };
        let inc = Sched::of(&row("2", "@daily", r#"{"backup_statement": "BACKUP DATABASE app INTO LATEST IN 'nodelocal://1/x' WITH OPTIONS (revision_history = true, detached)", "backup_type": 1, "dependent_schedule_id": 1}"#, "PAUSED", "WAIT")).unwrap();
        let full = Sched::of(&row("1", "@weekly", r#"{"backup_statement": "BACKUP DATABASE app INTO 'nodelocal://1/x' WITH OPTIONS (detached)", "dependent_schedule_id": 2, "unpause_on_success": 2}"#, "ACTIVE", "SKIP")).unwrap();
        assert!(inc.incremental() && !full.incremental());
        assert_eq!((inc.dependent(), full.dependent()), (Some(1), Some(2)));
        assert_eq!(full.kind, ScheduleKind::Backup);
        let b = backup_of(&full, Some(&inc)).unwrap();
        assert_eq!(b.recurring, "@daily");
        assert_eq!(b.full, Full::Cron("@weekly".into()));
        assert!(b.revision_history && b.enabled);
        assert_eq!((b.previous.as_str(), b.failure.as_str()), ("skip", "pause"));
        let v = backup_values(&b);
        assert_eq!(v["backup_target"], json!("database"));
        assert_eq!(v["databases"], json!(["app"]));
        assert_eq!(v["encryption_passphrase"], Value::Null);
        // The form's values give the same backup back.
        let again = backup_spec(
            &FormValues {
                values: v,
                lists: BTreeMap::new(),
            },
            Some(&b),
        )
        .unwrap();
        assert_eq!(again, b);
        let single = backup_of(&full, None).unwrap();
        assert_eq!(single.full, Full::Always);
        let ttl = Sched::of(&fixture(&[
            ("sid", "5"),
            ("label", "row-level-ttl: t [104]"),
            ("command", "{\"tableId\": 104}"),
        ]))
        .unwrap();
        assert_eq!((ttl.kind, ttl.table_id()), (ScheduleKind::Ttl, Some(104)));
    }

    #[test]
    fn backup_forms_are_checked() {
        let ok = json!({
            "name": "n", "backup_target": "database", "databases": ["app"],
            "locations": "s3://b", "recurring": "@daily", "full_backup": "auto"
        });
        assert!(backup_spec(&fv(ok.clone()), None).is_ok());
        let with = |k: &str, x: Value| {
            let mut v = ok.clone();
            v[k] = x;
            backup_spec(&fv(v), None)
        };
        assert!(with("name", json!("")).is_err());
        assert!(with("databases", json!([])).is_err());
        assert!(with("databases", json!(["a; DROP DATABASE b"])).is_err());
        assert!(with("locations", json!("not a uri")).is_err());
        assert!(with("locations", json!("")).is_err());
        assert!(with("recurring", json!("every day")).is_err());
        assert!(with("backup_target", json!("everything")).is_err());
        assert!(with("encryption", json!("passphrase"))
            .unwrap_err()
            .to_string()
            .contains("contraseña"));
        assert!(with("first_run", json!("tomorrow")).is_err());
        assert!(with("on_previous_running", json!("'; DROP")).is_err());
        // "auto" is for new schedules only.
        assert!(backup_spec(&fv(ok.clone()), Some(&backup())).is_err());
        let mut t = ok.clone();
        t["backup_target"] = json!("table");
        t["tables"] = json!("app.public.t\n\napp.*");
        match backup_spec(&fv(t), None).unwrap().what {
            What::Tables(n) => assert_eq!(n.len(), 2),
            w => panic!("{w:?}"),
        }
    }

    fn feed() -> Feed {
        Feed {
            label: "export".into(),
            targets: vec![parse_target("d.public.t").unwrap()],
            query: None,
            sink: "s3://b/cf?AWS_SECRET_ACCESS_KEY=k".into(),
            options: vec![("format".into(), OptVal::One("csv".into()))],
            recurring: "@daily".into(),
            first_run: None,
            previous: "wait".into(),
            failure: "reschedule".into(),
            enabled: true,
        }
    }

    #[test]
    fn changefeed_statements() {
        let f = feed();
        assert_eq!(
            feed_create_sql(&f, true, false),
            "CREATE SCHEDULE 'export' FOR CHANGEFEED TABLE \"d\".\"public\".\"t\" INTO 's3://b/cf?AWS_SECRET_ACCESS_KEY=********' WITH format = 'csv' RECURRING '@daily' WITH SCHEDULE OPTIONS on_previous_running = 'wait', on_execution_failure = 'reschedule'"
        );
        assert!(feed_create_sql(&f, true, true).contains("AWS_SECRET_ACCESS_KEY=k'"));
        assert_eq!(
            feed_create_sql(&f, false, false),
            "CREATE CHANGEFEED FOR TABLE \"d\".\"public\".\"t\" INTO 's3://b/cf?AWS_SECRET_ACCESS_KEY=********' WITH format = 'csv'"
        );
        let q = Feed {
            query: Some("SELECT id FROM d.public.t".into()),
            targets: Vec::new(),
            ..feed()
        };
        assert!(feed_create_sql(&q, true, true).contains("FOR CHANGEFEED INTO 's3://b/cf?AWS_SECRET_ACCESS_KEY=k' WITH format = 'csv' AS SELECT id FROM d.public.t RECURRING '@daily'"));
        assert!(feed_create_sql(&q, false, true)
            .ends_with("WITH format = 'csv' AS SELECT id FROM d.public.t"));
        let off = Feed {
            enabled: false,
            ..feed()
        };
        assert!(feed_sched_create(&off, false)
            .contains("PAUSE SCHEDULES SELECT id FROM [SHOW SCHEDULES] WHERE label = 'export';"));
    }

    #[test]
    fn a_changefeed_schedule_pauses_in_place_and_recreates_otherwise() {
        let c = root();
        let cur = feed();
        assert_eq!(feed_sched_alter(&c, 7, &cur, &cur, false).unwrap(), None);
        let off = Feed {
            enabled: false,
            ..cur.clone()
        };
        assert_eq!(
            feed_sched_alter(&c, 7, &cur, &off, false).unwrap().unwrap(),
            "-- CockroachDB: changefeed programado «export»\nPAUSE SCHEDULE 7;\n"
        );
        let other = Feed {
            options: vec![("format".into(), OptVal::One("parquet".into()))],
            ..cur.clone()
        };
        let sql = feed_sched_alter(&c, 7, &cur, &other, false)
            .unwrap()
            .unwrap();
        assert!(sql.contains("SET LOCAL autocommit_before_ddl = false;\nDROP SCHEDULE 7;\nCREATE SCHEDULE 'export'"), "{sql}");
        assert!(sql.contains("sin su historial"), "{sql}");
        let red = Feed {
            sink: "s3://b?AWS_SECRET_ACCESS_KEY=redacted".into(),
            ..other
        };
        assert!(feed_sched_alter(&c, 7, &cur, &red, false).is_err());
    }

    fn running(status: &str) -> Running {
        Running {
            id: 9,
            status: status.into(),
            owner: Some("root".into()),
            high_water: None,
            error: None,
            feed: Feed {
                label: String::new(),
                options: vec![
                    ("format".into(), OptVal::One("json".into())),
                    ("diff".into(), OptVal::Flag),
                ],
                enabled: status == "running",
                ..feed()
            },
        }
    }

    #[test]
    fn a_changefeed_changes_paused() {
        let cur = running("running");
        assert_eq!(feed_alter(&cur, &cur.feed, false).unwrap(), None);
        let mut f = cur.feed.clone();
        f.targets.push(parse_target("d.public.u FAMILY f").unwrap());
        f.options = vec![
            ("format".into(), OptVal::One("json".into())),
            ("resolved".into(), OptVal::One("10s".into())),
        ];
        let st = feed_alter(&cur, &f, false).unwrap().unwrap();
        assert_eq!(
            st,
            vec![
                Stage::PauseFor(9),
                Stage::WaitPaused(9),
                Stage::Sql("ALTER CHANGEFEED 9 ADD TABLE \"d\".\"public\".\"u\" FAMILY \"f\" SET resolved = '10s' UNSET diff".into()),
                Stage::Sql("RESUME JOB 9".into()),
            ]
        );
        let shown = render("x", &st);
        assert!(
            shown.contains("PAUSE JOB 9;\n-- DBine espera a que el changefeed 9 quede en pausa"),
            "{shown}"
        );
        // Without resuming it: paused, changed, left paused.
        let off = Feed {
            enabled: false,
            ..f.clone()
        };
        let st = feed_alter(&cur, &off, false).unwrap().unwrap();
        assert_eq!(st[0], Stage::PauseFor(9));
        assert!(!st.contains(&Stage::Sql("RESUME JOB 9".into())));
        // Paused, it stays paused; a new sink is set with the rest.
        let p = running("paused");
        let mut g = p.feed.clone();
        g.targets.clear();
        g.targets.push(parse_target("d.public.v").unwrap());
        g.sink = "kafka://k:9092?sasl_password=pw".into();
        let st = feed_alter(&p, &g, false).unwrap().unwrap();
        assert_eq!(
            st,
            vec![Stage::Sql("ALTER CHANGEFEED 9 ADD TABLE \"d\".\"public\".\"v\" DROP TABLE \"d\".\"public\".\"t\" SET sink = 'kafka://k:9092?sasl_password=********'".into())]
        );
        // Only pausing or resuming.
        let off = Feed {
            enabled: false,
            ..cur.feed.clone()
        };
        assert_eq!(
            feed_alter(&cur, &off, false).unwrap().unwrap(),
            vec![Stage::Sql("PAUSE JOB 9".into())]
        );
        let on = Feed {
            enabled: true,
            ..p.feed.clone()
        };
        assert_eq!(
            feed_alter(&p, &on, false).unwrap().unwrap(),
            vec![Stage::Sql("RESUME JOB 9".into())]
        );
        // Its query can't change.
        let q = Feed {
            query: Some("SELECT 1 FROM t".into()),
            ..cur.feed.clone()
        };
        assert!(feed_alter(&cur, &q, false).is_err());
        assert!(render("x", &[Stage::PauseCreated]).contains("PAUSE JOB <su id>"));
    }

    #[test]
    fn ttl_parameters() {
        assert_eq!(
            reloption("ttl_expire_after='3 days':::INTERVAL"),
            Some(("ttl_expire_after".into(), "3 days".into()))
        );
        assert_eq!(
            reloption("ttl_expiration_expression='created + INTERVAL ''30 days'''"),
            Some((
                "ttl_expiration_expression".into(),
                "created + INTERVAL '30 days'".into()
            ))
        );
        assert_eq!(
            reloption("ttl_select_batch_size=1000"),
            Some(("ttl_select_batch_size".into(), "1000".into()))
        );
        let p = ttl_params(&[
            "ttl='on'".into(),
            "ttl_job_cron='@daily'".into(),
            "schema_locked=true".into(),
            "ttl_pause=true".into(),
        ]);
        assert_eq!(p.get("ttl_job_cron").map(String::as_str), Some("@daily"));
        assert!(!p.contains_key("schema_locked"));
        let v = ttl_values(&p);
        assert_eq!(v["ttl_pause"], json!(true));
        assert_eq!(v["ttl_select_batch_size"], Value::Null);
        assert_eq!(v["expire_mode"], json!("after"));
        // The form's values, checked.
        let spec = ttl_spec(&fv(json!({
            "expire_mode": "after", "ttl_expire_after": "30 days", "ttl_job_cron": " @daily ",
            "ttl_select_batch_size": "500", "ttl_label_metrics": true
        })))
        .unwrap();
        assert_eq!(spec["ttl_job_cron"].as_deref(), Some("@daily"));
        assert_eq!(spec["ttl_select_batch_size"].as_deref(), Some("500"));
        assert_eq!(spec["ttl_expiration_expression"], None);
        assert!(ttl_spec(&fv(json!({"expire_mode": "after"}))).is_err());
        assert!(ttl_spec(&fv(
            json!({"expire_mode": "expression", "ttl_expiration_expression": "now(); DROP TABLE t"})
        ))
        .is_err());
        assert!(ttl_spec(&fv(
            json!({"expire_mode": "after", "ttl_expire_after": "1 day", "ttl_job_cron": "x"})
        ))
        .is_err());
        assert!(ttl_spec(&fv(json!({"expire_mode": "after", "ttl_expire_after": "1 day", "ttl_delete_batch_size": -1}))).is_err());
    }

    #[test]
    fn ttl_numbers_are_checked_by_their_label() {
        let e = ttl_spec(&fv(json!({
            "expire_mode": "after", "ttl_expire_after": "1 day", "ttl_select_batch_size": 0
        })))
        .unwrap_err();
        assert!(e.to_string().contains("«Filas leídas por lote»"), "{e}");
        let e = ttl_spec(&fv(json!({
            "expire_mode": "after", "ttl_expire_after": "1 day", "ttl_delete_rate_limit": -1
        })))
        .unwrap_err();
        assert!(
            e.to_string().contains("«Filas borradas por segundo»"),
            "{e}"
        );
        assert!(ttl_spec(&fv(json!({
            "expire_mode": "after", "ttl_expire_after": "1 day", "ttl_select_rate_limit": 0
        })))
        .is_ok());
    }

    #[test]
    fn ttl_statements_set_then_reset() {
        let t = Table {
            id: 104,
            db: "d".into(),
            schema: "public".into(),
            name: "t".into(),
        };
        let none = TtlParams::new();
        let new = ttl_spec(&fv(
            json!({"expire_mode": "after", "ttl_expire_after": "3 days", "ttl_job_cron": "@daily"}),
        ))
        .unwrap();
        assert_eq!(
            ttl_statements(&t, &none, &new),
            vec!["ALTER TABLE \"d\".\"public\".\"t\" SET (ttl_expire_after = '3 days', ttl_job_cron = '@daily')"]
        );
        let cur: TtlParams = [
            ("ttl".to_string(), "on".to_string()),
            ("ttl_expire_after".into(), "3 days".into()),
            ("ttl_job_cron".into(), "@daily".into()),
            ("ttl_select_batch_size".into(), "1000".into()),
            ("ttl_pause".into(), "true".into()),
        ]
        .into_iter()
        .collect();
        let new = ttl_spec(&fv(json!({
            "expire_mode": "expression", "ttl_expiration_expression": "c + INTERVAL '1 day'",
            "ttl_job_cron": "@daily", "ttl_delete_batch_size": 50, "ttl_pause": false
        })))
        .unwrap();
        assert_eq!(
            ttl_statements(&t, &cur, &new),
            vec![
                "ALTER TABLE \"d\".\"public\".\"t\" SET (ttl_delete_batch_size = 50, ttl_expiration_expression = 'c + INTERVAL ''1 day''', ttl_pause = false)".to_string(),
                "ALTER TABLE \"d\".\"public\".\"t\" RESET (ttl_expire_after, ttl_select_batch_size)".into(),
            ]
        );
        let same = ttl_spec(&fv(ttl_values(&cur)
            .into_iter()
            .collect::<serde_json::Map<_, _>>()
            .into()))
        .unwrap();
        assert!(ttl_statements(&t, &cur, &same).is_empty());
    }

    #[test]
    fn the_clusters_schedules() {
        assert_eq!(
            setting_of("sql-stats-compaction"),
            Some("sql.stats.cleanup.recurrence")
        );
        assert_eq!(
            setting_of("sql-schema-telemetry"),
            Some("sql.schema.telemetry.recurrence")
        );
        assert_eq!(setting_of("other"), None);
        let s = Sched::of(&fixture(&[
            ("sid", "3"),
            ("label", "sql-stats-compaction"),
            ("owner", "node"),
            ("schedule_status", "ACTIVE"),
            ("recurrence", "@hourly"),
            ("command", "{}"),
        ]))
        .unwrap();
        assert_eq!(s.kind, ScheduleKind::System);
        let set = Some("sql.stats.cleanup.recurrence");
        assert!(system_statements(&s, set, Some("@hourly"), Some("@hourly"), true).is_empty());
        assert_eq!(
            system_statements(&s, set, Some("@hourly"), Some("0 * * * *"), false),
            vec![
                "SET CLUSTER SETTING sql.stats.cleanup.recurrence = '0 * * * *'".to_string(),
                "PAUSE SCHEDULE 3".into(),
            ]
        );
        // Without a known setting only pausing and resuming.
        assert_eq!(
            system_statements(&s, None, None, Some("x"), false),
            vec!["PAUSE SCHEDULE 3".to_string()]
        );
    }

    #[test]
    fn a_new_job_offers_every_kind() {
        let forms: Vec<(Source, ObjectForm)> = sources(&root())
            .into_iter()
            .map(|src| {
                let sections = match src {
                    Source::CrdbBackup => backup_fields(false),
                    Source::CrdbChangefeed => feed_fields(true, false, false),
                    _ => feed_fields(false, false, false),
                };
                (
                    src,
                    ObjectForm {
                        sections,
                        ..Default::default()
                    },
                )
            })
            .collect();
        let f = combine(forms);
        let all: Vec<&FormField> = f.sections.iter().flat_map(|s| s.fields.iter()).collect();
        let get = |k: &str| {
            *all.iter()
                .find(|x| x.key == k)
                .unwrap_or_else(|| panic!("{k}"))
        };
        assert_eq!(get("source").key, "source");
        assert_eq!(
            get("locations").when,
            Some(("source".into(), vec!["crdb_backup".into()]))
        );
        // Each schedule's frequency says what it does: each its own.
        assert_eq!(
            get("recurring").when,
            Some(("source".into(), vec!["crdb_backup".into()]))
        );
        assert_eq!(
            get("crdb_changefeed.recurring").when,
            Some(("source".into(), vec!["crdb_changefeed".into()]))
        );
        assert!(get("crdb_changefeed.recurring")
            .help
            .as_deref()
            .is_some_and(|h| h.contains("exporta")));
        assert_eq!(
            get("sink").when,
            Some((
                "source".into(),
                vec!["crdb_changefeed".into(), "crdb_job".into()]
            ))
        );
        // "Activo" says what pausing each one does: each its own too.
        assert_eq!(
            get("enabled").when,
            Some(("source".into(), vec!["crdb_backup".into()]))
        );
        for src in ["crdb_changefeed", "crdb_job"] {
            assert_eq!(
                get(&format!("{src}.enabled")).when,
                Some(("source".into(), vec![src.to_string()]))
            );
        }
        // Secrets are secret fields; the TTL form has every parameter.
        assert!(matches!(
            get("encryption_passphrase").kind,
            FormKind::Secret
        ));
        let ttl: Vec<String> = ttl_fields(&root(), true, true)
            .iter()
            .flat_map(|s| s.fields.iter().map(|f| f.key.clone()))
            .collect();
        for (k, _, _) in TTL_PARAMS {
            assert!(ttl.iter().any(|x| x == k), "{k}");
        }
        let old = Caps {
            version: Some((23, 2)),
            ..root()
        };
        let ttl_old: Vec<String> = ttl_fields(&old, true, true)
            .iter()
            .flat_map(|s| s.fields.iter().map(|f| f.key.clone()))
            .collect();
        assert!(!ttl_old
            .iter()
            .any(|k| k == "ttl_disable_changefeed_replication"));
    }

    #[test]
    fn ids_and_rights() {
        assert_eq!(ttl_obj(None).unwrap(), Obj::Ttl(None));
        assert_eq!(ttl_obj(Some("104")).unwrap(), Obj::Ttl(Some(104)));
        for bad in ["0", "-1", "01", "1 OR 1=1", "x", ""] {
            assert!(ttl_obj(Some(bad)).is_err(), "{bad}");
        }
        let bob = Caps {
            user: "bob".into(),
            admin: false,
            ..root()
        };
        assert!(
            bob.may_change(Some("bob")) && !bob.may_change(Some("root")) && !bob.may_change(None)
        );
        assert!(root().may_change(Some("anyone")));
        assert!(bob.notes().iter().any(|n| n.contains("no es admin")));
        assert!(Caps {
            schedules: false,
            ..bob.clone()
        }
        .notes()
        .iter()
        .any(|n| n.contains("VIEWJOB")));
        let granted = Caps {
            privileges: vec!["MODIFYCLUSTERSETTING".into()],
            ..bob
        };
        assert!(granted.has("modifyclustersetting") && !granted.has("BACKUP"));
        // What the tree offers follows the privileges.
        let abilities = |c: &Caps| {
            let k = &c.kinds()[0];
            (k.can_create, k.can_edit, k.can_delete)
        };
        let nothing = Caps {
            schedules: false,
            granted: Some(vec!["USAGE".into()]),
            ..granted.clone()
        };
        assert!(sources(&nothing).is_empty());
        assert_eq!(abilities(&nothing), (false, false, false));
        let viewer = Caps {
            schedules: true,
            ..nothing.clone()
        };
        assert_eq!(abilities(&viewer), (false, true, true));
        let feeds = Caps {
            granted: Some(vec!["CHANGEFEED".into()]),
            ..nothing.clone()
        };
        assert_eq!(sources(&feeds), [Source::CrdbChangefeed, Source::CrdbFeed]);
        assert_eq!(abilities(&feeds), (true, true, true));
        let owner = Caps {
            granted: Some(vec!["ALL".into()]),
            ..nothing.clone()
        };
        assert_eq!(sources(&owner).len(), 3);
        let backups = Caps {
            privileges: vec!["BACKUP".into()],
            ..nothing.clone()
        };
        assert_eq!(sources(&backups), [Source::CrdbBackup]);
        // Grants that couldn't be read: the server decides.
        let unknown = Caps {
            granted: None,
            ..nothing
        };
        assert_eq!(sources(&unknown).len(), 3);
        assert_eq!(sources(&root()).len(), 3);
        assert_eq!(previous_of(Some("NO_WAIT")), "start");
        assert_eq!(failure_of(Some("RETRY_SOON")), "retry");
        assert_eq!(failure_of(Some("RESCHEDULE")), "reschedule");
    }
}
