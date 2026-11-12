//! TDengine's streams managed whole through the scheduler contract
//! ([`dbine_driver::Session::scheduler_info`] and the rest): the only kind
//! of object TDengine runs on its own, under the key `job` with the ids the
//! jobs list gives them ([`crate::jobs`]: the name on the old engine,
//! `db.name` on the new one).
//!
//! - The designer has every clause and option of `CREATE STREAM` for the
//!   engine the server runs ([`stream`]): up to 3.3.6 the trigger mode,
//!   watermark, expired and update handling, history, target supertable
//!   with its columns, tags and subtable names, the query and (3.3.6) the
//!   notifications; from 3.3.7 the trigger (`PERIOD`, `INTERVAL` /
//!   `SLIDING`, `SESSION`, `STATE_WINDOW`, `EVENT_WINDOW`, `COUNT_WINDOW`),
//!   its table and partitioning, every `STREAM_OPTIONS` option, the
//!   notifications and the output.
//! - TDengine has no `ALTER STREAM`: a change drops the stream and creates
//!   it again (its state goes: open windows and progress; the target table
//!   and its rows stay), and when the server refuses the new statement the
//!   old one is created again. Enabled is `PAUSE` / `RESUME STREAM` (old
//!   engine, 3.1 on) or `STOP` / `START STREAM` (new), with `IGNORE
//!   UNTREATED` on request.
//! - Statements run with the stream's database as the current one (the old
//!   engine's source database), so the names without one resolve as they
//!   did; the script says so with a `USE`.
//!
//! TDengine keeps no history of a stream's runs and no scheduler log, and
//! a stream has no steps: `run_job_from`, `clear_job_history` and
//! `scheduler_logs` stay unsupported. Nor does it say what a user may do
//! with streams before trying: the server refuses what it doesn't allow.

mod stream;

use crate::jobs::{self, Stream};
use crate::TdSession;
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result,
    SchedulerInfo, SchedulerItem, SchedulerKind, SchedulerList,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const RECREATE_NOTE: &str = "TDengine no tiene ALTER STREAM: al guardar un cambio, DBine borra el stream y lo vuelve a crear. Se pierde su estado (ventanas abiertas, avance); la tabla de destino y sus filas quedan, y lo que llegue mientras tanto se procesa solo si incluye los datos históricos.";
const UNREADABLE: &str = "DBine no pudo leer la sentencia de este stream: se muestra como está y no se puede cambiar desde el diseñador.";
const NOT_FOUND: &str = "no existe el stream o este usuario no lo puede ver";
const TAG_NAMES: &str = "La tabla de destino ya existe: TAGS lleva solo los nombres de sus tags (TDengine no acepta definirlos de nuevo).";
const SYSTEM_DBS: &[&str] = &["information_schema", "performance_schema"];

pub(crate) fn bad(message: String) -> Error {
    Error::Query(message)
}

/// Text from the app, short and on one line, for a message.
pub(crate) fn shown(s: &str) -> String {
    comment_text(s.trim()).chars().take(60).collect()
}

fn comment_text(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

// ---------------------------------------------------------------------------
// What the server has
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ctx {
    /// The new stream engine (3.3.7 on).
    pub(crate) new_engine: bool,
    /// Streams can be suspended (old engine from 3.1; new engine always).
    pub(crate) suspend: bool,
    /// Streams can notify (old engine from 3.3.6; new engine always).
    pub(crate) notify: bool,
}

fn numbers(version: &str) -> Vec<u32> {
    version
        .split(|c: char| !c.is_ascii_digit())
        .filter(|x| !x.is_empty())
        .map(|x| x.parse().unwrap_or(0))
        .collect()
}

pub(crate) fn ctx_of(version: &str, new_engine: bool) -> Ctx {
    let n = numbers(version);
    let at_least = |want: &[u32]| n.is_empty() || n.as_slice() >= want;
    Ctx {
        new_engine,
        suspend: jobs::can_suspend(version, new_engine),
        notify: new_engine || at_least(&[3, 3, 6]),
    }
}

async fn read(s: &TdSession) -> Result<(Ctx, Vec<Stream>)> {
    let version = s
        .strings("SELECT SERVER_VERSION()")
        .await?
        .first()
        .and_then(|r| r.first())
        .cloned()
        .unwrap_or_default();
    let a = s
        .query("SELECT * FROM information_schema.ins_streams")
        .await?;
    let new_engine = a.columns.iter().any(|(n, _)| n == "db_name");
    let rows: Vec<Map<String, Value>> = a
        .data
        .into_iter()
        .map(|r| {
            a.columns
                .iter()
                .map(|(n, _)| n.clone())
                .zip(r)
                .collect::<Map<_, _>>()
        })
        .collect();
    Ok((ctx_of(&version, new_engine), jobs::streams(&rows)))
}

fn find(all: Vec<Stream>, id: &str) -> Result<Stream> {
    all.into_iter()
        .find(|s| s.id() == id)
        .ok_or_else(|| bad(format!("«{}»: {NOT_FOUND}.", shown(id))))
}

async fn databases(s: &TdSession) -> Vec<String> {
    match s
        .strings("SELECT name FROM information_schema.ins_databases")
        .await
    {
        Ok(rows) => rows
            .into_iter()
            .filter_map(|r| r.into_iter().next())
            .filter(|d| !SYSTEM_DBS.contains(&d.as_str()))
            .collect(),
        Err(e) => {
            tracing::debug!("tdengine scheduler: databases unknown: {e}");
            Vec::new()
        }
    }
}

fn check_kind(kind: &str) -> Result<()> {
    if kind.trim() == "job" {
        return Ok(());
    }
    Err(bad(format!(
        "«{}» no es un tipo de objeto del planificador de TDengine",
        shown(kind)
    )))
}

fn info_of(c: &Ctx) -> SchedulerInfo {
    SchedulerInfo {
        kinds: vec![SchedulerKind::new("job", "Streams", "stream").abilities(true, true, true)],
        logs: false,
        notes: if c.suspend {
            Vec::new()
        } else {
            vec![jobs::NO_PAUSE.to_string()]
        },
    }
}

// ---------------------------------------------------------------------------
// The contract's calls
// ---------------------------------------------------------------------------

pub(crate) async fn info(s: &mut TdSession) -> Result<SchedulerInfo> {
    Ok(info_of(&read(s).await?.0))
}

pub(crate) async fn items(s: &mut TdSession, kind: &str) -> Result<SchedulerList> {
    check_kind(kind)?;
    let (c, all) = read(s).await?;
    Ok(SchedulerList {
        items: all
            .iter()
            .take(jobs::MAX_JOBS)
            .map(|st| {
                let j = jobs::job_of(st, c.suspend);
                SchedulerItem {
                    id: j.id,
                    name: j.name,
                    description: j.schedule,
                    enabled: Some(j.enabled),
                    system: false,
                }
            })
            .collect(),
        notes: Vec::new(),
    })
}

pub(crate) async fn form(s: &mut TdSession, kind: &str, id: Option<&str>) -> Result<ObjectForm> {
    check_kind(kind)?;
    let (c, all) = read(s).await?;
    let current = id.map(|id| find(all, id)).transpose()?;
    let dbs = databases(s).await;
    Ok(form_of(&c, current.as_ref(), &dbs, s.db.as_deref()))
}

pub(crate) async fn script(
    s: &mut TdSession,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<String> {
    check_kind(kind)?;
    let (c, all) = read(s).await?;
    let current = id.map(|id| find(all, id)).transpose()?;
    let t = targets(s, &c, current.as_ref(), values).await;
    Ok(plan(&c, current.as_ref(), values, t)?.script())
}

pub(crate) async fn save(
    s: &mut TdSession,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Option<String>> {
    check_kind(kind)?;
    s.cancel.flag.store(false, Ordering::SeqCst);
    let (c, all) = read(s).await?;
    let current = id.map(|id| find(all, id)).transpose()?;
    let t = targets(s, &c, current.as_ref(), values).await;
    let p = plan(&c, current.as_ref(), values, t)?;
    run(s, &p).await?;
    Ok(Some(p.id))
}

pub(crate) async fn delete_script(s: &mut TdSession, kind: &str, id: &str) -> Result<String> {
    check_kind(kind)?;
    let (_, all) = read(s).await?;
    Ok(drop_plan(&find(all, id)?).script())
}

pub(crate) async fn delete(s: &mut TdSession, kind: &str, id: &str) -> Result<()> {
    check_kind(kind)?;
    s.cancel.flag.store(false, Ordering::SeqCst);
    let (_, all) = read(s).await?;
    run(s, &drop_plan(&find(all, id)?)).await
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

    pub(crate) fn bool(&self, k: &str) -> bool {
        self.bool_or(k, false)
    }

    /// A multi-select's values, trimmed, without empties.
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

/// A list's rows from the designer.
pub(crate) fn rows<'a>(values: &'a FormValues, list: &str) -> Vec<Vals<'a>> {
    values
        .lists
        .get(list)
        .map(|rows| rows.iter().map(Vals).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

/// One statement and the database it runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Stmt {
    pub(crate) sql: String,
    pub(crate) db: Option<String>,
    /// A suspend or resume: the stream (by id) is watched until it says
    /// it is enabled (`true`) or suspended, and asked again while its tasks
    /// are still starting (TDengine refuses or ignores it then).
    pub(crate) wait: Option<(String, bool)>,
}

impl Stmt {
    fn plain(sql: String) -> Stmt {
        Stmt {
            sql,
            db: None,
            wait: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Recreate {
    pub(crate) drop: usize,
    pub(crate) create: usize,
    pub(crate) restore: Vec<Stmt>,
    pub(crate) what: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Plan {
    pub(crate) name: String,
    pub(crate) comments: Vec<String>,
    pub(crate) stmts: Vec<Stmt>,
    pub(crate) id: String,
    pub(crate) recreate: Option<Recreate>,
}

impl Plan {
    fn new(name: &str, id: String) -> Plan {
        Plan {
            name: name.into(),
            comments: Vec::new(),
            stmts: Vec::new(),
            id,
            recreate: None,
        }
    }

    fn push(&mut self, st: Stmt) -> usize {
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
        let mut current: Option<&str> = None;
        for st in &self.stmts {
            if let Some(d) = st.db.as_deref().filter(|d| Some(*d) != current) {
                out.push_str(&format!("USE {};\n", crate::ddl::q(d)));
                current = Some(d);
            }
            out.push_str(&st.sql);
            out.push_str(";\n");
        }
        out
    }
}

/// `PAUSE` / `RESUME` (old engine) or `STOP` / `START` (new) of the stream
/// `id` (`target` in the statement).
fn suspend_stmt(c: &Ctx, id: &str, target: &str, enabled: bool, ignore_untreated: bool) -> Stmt {
    let verb = match (c.new_engine, enabled) {
        (true, true) => "START",
        (true, false) => "STOP",
        (false, true) => "RESUME",
        (false, false) => "PAUSE",
    };
    let skip = if enabled && ignore_untreated {
        "IGNORE UNTREATED "
    } else {
        ""
    };
    Stmt {
        sql: format!("{verb} STREAM {skip}{target}"),
        db: None,
        wait: Some((id.to_string(), enabled)),
    }
}

fn build(c: &Ctx, values: &FormValues, target_exists: bool) -> Result<stream::Built> {
    if c.new_engine {
        stream::build_new(values)
    } else {
        stream::build_old(values, c.notify, target_exists)
    }
}

/// Which target tables exist: the one the stream writes to now and the one
/// the designer's values name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Targets {
    pub(crate) old: bool,
    pub(crate) new: bool,
}

async fn table_exists(s: &TdSession, (db, t): (String, String)) -> bool {
    use crate::ddl::lit;
    let sql = format!(
        "SELECT stable_name FROM information_schema.ins_stables WHERE db_name = {} AND stable_name = {}",
        lit(&db),
        lit(&t)
    );
    if s.strings(&sql).await.is_ok_and(|r| !r.is_empty()) {
        return true;
    }
    let sql = format!(
        "SELECT table_name FROM information_schema.ins_tables WHERE db_name = {} AND table_name = {}",
        lit(&db),
        lit(&t)
    );
    s.strings(&sql).await.is_ok_and(|r| !r.is_empty())
}

async fn targets(s: &TdSession, c: &Ctx, current: Option<&Stream>, values: &FormValues) -> Targets {
    let mut t = Targets::default();
    if let Some(x) = stream::target_table(values) {
        t.new = table_exists(s, x).await;
    }
    if let Some(x) = current
        .and_then(|st| current_values(c, st))
        .and_then(|v| stream::target_table(&v))
    {
        t.old = table_exists(s, x).await;
    }
    t
}

fn id_of(c: &Ctx, b: &stream::Built) -> String {
    if c.new_engine {
        format!("{}.{}", b.db, b.name)
    } else {
        b.name.clone()
    }
}

fn target_of(c: &Ctx, b: &stream::Built) -> String {
    if c.new_engine {
        format!("{}.{}", crate::ddl::q(&b.db), crate::ddl::q(&b.name))
    } else {
        crate::ddl::q(&b.name)
    }
}

/// The database a stream's statement ran in: the new engine's own, the
/// old engine's source database.
fn db_of(st: &Stream) -> Option<String> {
    st.db.clone().or_else(|| {
        st.extra
            .iter()
            .find(|(k, _)| k == "source_db")
            .map(|(_, v)| v.clone())
    })
}

/// The form's values for an existing stream, from its statement.
pub(crate) fn current_values(c: &Ctx, st: &Stream) -> Option<FormValues> {
    let sql = st.sql.as_deref()?;
    let mut v = if c.new_engine {
        stream::parse_new(sql)?
    } else {
        stream::parse_old(sql)?
    };
    if let Some(d) = db_of(st) {
        v.values.entry("database".into()).or_insert(json!(d));
    }
    v.values.insert("enabled".into(), json!(st.enabled()));
    Some(v)
}

pub(crate) fn plan(
    c: &Ctx,
    current: Option<&Stream>,
    values: &FormValues,
    targets: Targets,
) -> Result<Plan> {
    let v = Vals(&values.values);
    let enabled = v.bool_or("enabled", true);
    let skip = v.bool("resume_ignore_untreated");
    let new = build(c, values, targets.new)?;
    let as_written = build(c, values, false)?;
    let id = id_of(c, &new);
    if !enabled && !c.suspend {
        return Err(Error::Unsupported(jobs::NO_PAUSE.into()));
    }
    let Some(st) = current else {
        let mut p = Plan::new(&new.name, id);
        p.comments
            .push(format!("Stream «{}» de TDengine", new.name));
        if new != as_written {
            p.comments.push(TAG_NAMES.into());
        }
        p.push(Stmt {
            sql: new.sql.clone(),
            db: Some(new.db.clone()),
            wait: None,
        });
        if !enabled {
            p.comments
                .push("Arranca al crearse: se suspende apenas sus tareas están listas.".into());
            p.push(suspend_stmt(
                c,
                &p.id.clone(),
                &target_of(c, &new),
                false,
                false,
            ));
        }
        return Ok(p);
    };
    let old_values = current_values(c, st).ok_or_else(|| bad(UNREADABLE.into()))?;
    let old = build(c, &old_values, targets.old).ok();
    let mut p = Plan::new(&st.name, id);
    p.comments.push(format!("Stream «{}» de TDengine", st.id()));
    if old.as_ref() != Some(&new) {
        p.comments
            .push("TDengine no tiene ALTER STREAM: se borra el stream y se vuelve a crear.".into());
        p.comments.push("Se pierde su estado (ventanas abiertas, avance); la tabla de destino y sus filas quedan.".into());
        if new != as_written {
            p.comments.push(TAG_NAMES.into());
        }
        let drop = p.push(Stmt::plain(format!("DROP STREAM {}", st.target())));
        let create = p.push(Stmt {
            sql: new.sql.clone(),
            db: Some(new.db.clone()),
            wait: None,
        });
        // The old statement, written again: its target exists now.
        let restore_sql = build(c, &old_values, true)
            .map(|b| b.sql)
            .unwrap_or_else(|_| {
                st.sql
                    .clone()
                    .unwrap_or_default()
                    .trim()
                    .trim_end_matches(';')
                    .to_string()
            });
        let mut restore = vec![Stmt {
            sql: restore_sql,
            db: db_of(st),
            wait: None,
        }];
        if !st.enabled() && c.suspend {
            restore.push(suspend_stmt(c, &st.id(), &st.target(), false, false));
        }
        p.recreate = Some(Recreate {
            drop,
            create,
            restore,
            what: st.id(),
        });
        if !enabled {
            p.push(suspend_stmt(
                c,
                &p.id.clone(),
                &target_of(c, &new),
                false,
                false,
            ));
        }
        return Ok(p);
    }
    if enabled != st.enabled() {
        if !c.suspend {
            return Err(Error::Unsupported(jobs::NO_PAUSE.into()));
        }
        p.push(suspend_stmt(c, &st.id(), &st.target(), enabled, skip));
    }
    Ok(p)
}

pub(crate) fn drop_plan(st: &Stream) -> Plan {
    let mut p = Plan::new(&st.name, st.id());
    p.comments
        .push(format!("Borra el stream «{}» de TDengine", st.id()));
    p.push(Stmt::plain(format!("DROP STREAM {}", st.target())));
    p
}

// ---------------------------------------------------------------------------
// Running a plan
// ---------------------------------------------------------------------------

async fn once(s: &TdSession, st: &Stmt) -> Result<()> {
    s.cancel
        .run(s.conn.sql(st.db.as_deref(), &st.sql))
        .await
        .map(|_| ())
}

/// The stream `id` says it is enabled (`want`) or suspended.
async fn reached(s: &TdSession, id: &str, want: bool) -> Result<bool> {
    let (_, all) = read(s).await?;
    Ok(all
        .iter()
        .find(|x| x.id() == id)
        .is_some_and(|x| x.enabled() == want))
}

/// How long a stream gets to suspend or resume (one just made needs its
/// tasks ready first).
const SUSPEND_WAIT: Duration = Duration::from_secs(90);

async fn exec(s: &TdSession, st: &Stmt) -> Result<()> {
    let Some((id, want)) = &st.wait else {
        return once(s, st).await;
    };
    let start = Instant::now();
    let mut asked: Option<Instant> = None;
    loop {
        if asked.is_none_or(|a| a.elapsed() >= Duration::from_secs(5)) {
            match once(s, st).await {
                Ok(()) => {}
                Err(Error::Statement(e)) if e.code.as_deref() == Some("0x4104") => {}
                Err(e) => return Err(e),
            }
            asked = Some(Instant::now());
        }
        if reached(s, id, *want).await? {
            return Ok(());
        }
        if start.elapsed() > SUSPEND_WAIT {
            return Err(bad(if *want {
                format!(
                    "El stream «{}» no volvió a ponerse en marcha: probá de nuevo en unos segundos.",
                    shown(id)
                )
            } else {
                format!(
                    "El stream «{}» sigue poniéndose en marcha y no se pudo suspender: probá de nuevo en unos segundos.",
                    shown(id)
                )
            }));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn head(sql: &str) -> String {
    shown(sql.lines().next().unwrap_or(sql))
}

/// Runs the plan's statements in order (TDengine has no transactions for
/// them). A drop and create that fails at the create puts the old stream
/// back.
async fn run(s: &TdSession, plan: &Plan) -> Result<()> {
    for (i, st) in plan.stmts.iter().enumerate() {
        let Err(e) = exec(s, st).await else {
            continue;
        };
        let e = jobs::starting_up(e, &plan.name);
        let cause = e.to_string();
        if let Some(r) = plan
            .recreate
            .as_ref()
            .filter(|r| i > r.drop && i <= r.create)
        {
            let mut restored = Ok(());
            for x in &r.restore {
                restored = exec(s, x).await.map_err(|e| e.to_string());
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
                    r.restore.first().map(|x| x.sql.as_str()).unwrap_or("")
                ),
            }));
        }
        if i == 0 {
            return Err(e);
        }
        return Err(bad(format!(
            "El servidor rechazó «{}»: {cause}. Las instrucciones anteriores del script ya se habían aplicado.",
            head(&st.sql)
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

fn general_fields(c: &Ctx, existing: bool) -> Vec<FormField> {
    let mut f = vec![
        FormField::new("database", "Base de datos", FormKind::reference("database", false))
            .required()
            .help(if c.new_engine {
                "La base de datos del stream; los nombres de tabla sin base de datos se buscan en ella."
            } else {
                "Donde se buscan los nombres de tabla sin base de datos de la consulta y de INTO (TDengine la guarda como base de origen)."
            }),
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .help(if c.new_engine {
                "El nombre del stream dentro de su base de datos."
            } else {
                "El nombre del stream en el servidor."
            }),
        FormField::new("enabled", "En marcha", FormKind::Bool)
            .default(json!(true))
            .read_only(!c.suspend)
            .help(if c.new_engine {
                "Desmarcado, el stream se detiene (STOP STREAM); marcado, sigue (START STREAM)."
            } else {
                "Desmarcado, el stream se pausa (PAUSE STREAM); marcado, sigue (RESUME STREAM)."
            }),
    ];
    if existing {
        f.extend([
            FormField::new(
                "resume_ignore_untreated",
                "Al reanudarlo, saltear lo que llegó mientras estuvo suspendido (IGNORE UNTREATED)",
                FormKind::Bool,
            )
            .default(json!(false))
            .when("enabled", &["true"])
            .read_only(!c.suspend),
            FormField::new("status", "Estado", FormKind::Text).read_only(true),
            FormField::new("created", "Creado", FormKind::Text).read_only(true),
            FormField::new("message", "Mensaje", FormKind::Textarea).read_only(true),
        ]);
    }
    f
}

fn yes_no_default() -> FormKind {
    FormKind::select(&[("", "Por defecto"), ("1", "Sí"), ("0", "No")])
}

fn events_kind() -> FormKind {
    FormKind::multi(&[
        ("window_open", "Al abrirse la ventana (WINDOW_OPEN)"),
        ("window_close", "Al cerrarse la ventana (WINDOW_CLOSE)"),
    ])
}

fn urls_list() -> FormList {
    FormList::new(
        "notify_urls",
        "Direcciones",
        "url",
        vec![FormField::new("url", "URL", FormKind::Text)
            .required()
            .placeholder("ws://servidor:8080/notify")],
    )
}

fn output_fields(c: &Ctx) -> Vec<FormField> {
    vec![
        FormField::new("into", "Tabla de destino (INTO)", FormKind::Text)
            .placeholder("base.resultados")
            .help(if c.new_engine {
                "Donde escribe el resultado; vacío en un stream que solo notifica (CALC_NOTIFY_ONLY)."
            } else {
                "La supertabla donde escribe el resultado (se crea si no existe)."
            }),
        FormField::new(
            "subtable",
            if c.new_engine {
                "Nombre de cada subtabla (OUTPUT_SUBTABLE)"
            } else {
                "Nombre de cada subtabla (SUBTABLE)"
            },
            FormKind::Text,
        )
        .placeholder("CONCAT('t_', tbname)")
        .help("Una expresión que nombra la subtabla de destino de cada grupo."),
        FormField::new("columns", "Columnas", FormKind::Text)
            .placeholder("ts, valor")
            .help("Los nombres de las columnas del resultado, en orden (opcional)."),
        FormField::new("tags", "Tags", FormKind::Text)
            .placeholder("loc VARCHAR(16)")
            .help("Los tags de la tabla de destino (opcional)."),
    ]
}

fn query_field(c: &Ctx) -> FormField {
    let f = FormField::new("query", "Consulta", FormKind::code("sql"))
        .placeholder("SELECT _wstart, count(*) FROM base.medidores INTERVAL(1m)")
        .help("La consulta cuyo resultado escribe el stream (AS …).");
    if c.new_engine {
        f
    } else {
        f.required()
    }
}

fn old_sections(c: &Ctx, existing: bool) -> Vec<FormSection> {
    let mut s = vec![
        FormSection::new("general", "General").fields(general_fields(c, existing)),
        FormSection::new("trigger", "Disparo").fields(vec![
            FormField::new(
                "trigger",
                "Cuándo calcula (TRIGGER)",
                FormKind::select(&[
                    ("", "Por defecto (WINDOW_CLOSE)"),
                    ("at_once", "Apenas llegan los datos (AT_ONCE)"),
                    ("window_close", "Al cerrarse la ventana (WINDOW_CLOSE)"),
                    (
                        "max_delay",
                        "Al cerrarse la ventana o pasada una demora (MAX_DELAY)",
                    ),
                    (
                        "force_window_close",
                        "Al cerrarse la ventana según el reloj (FORCE_WINDOW_CLOSE)",
                    ),
                    (
                        "continuous_window_close",
                        "Al cerrarse cada ventana, recalculando (CONTINUOUS_WINDOW_CLOSE)",
                    ),
                ]),
            )
            .default(json!("")),
            FormField::new("max_delay", "Demora máxima", FormKind::Text)
                .placeholder("5s")
                .when("trigger", &["max_delay"]),
            FormField::new(
                "recalculate",
                "Recalcular cada (RECALCULATE)",
                FormKind::Text,
            )
            .placeholder("1h")
            .when("trigger", &["continuous_window_close"]),
            FormField::new("watermark", "Watermark", FormKind::Text)
                .placeholder("10s")
                .help("Cuánto espera datos atrasados antes de cerrar una ventana."),
            FormField::new(
                "ignore_expired",
                "Ignorar datos vencidos (IGNORE EXPIRED)",
                yes_no_default(),
            )
            .default(json!("")),
            FormField::new(
                "delete_mark",
                "Conservar el estado (DELETE_MARK)",
                FormKind::Text,
            )
            .placeholder("1d")
            .help("Cuánto guarda el estado de las ventanas cerradas."),
            FormField::new(
                "fill_history",
                "Procesar los datos históricos (FILL_HISTORY)",
                yes_no_default(),
            )
            .default(json!("")),
            FormField::new(
                "fill_history_async",
                "Procesarlos en segundo plano (ASYNC)",
                FormKind::Bool,
            )
            .when("fill_history", &["1"]),
            FormField::new(
                "ignore_update",
                "Ignorar actualizaciones (IGNORE UPDATE)",
                yes_no_default(),
            )
            .default(json!("")),
        ]),
        FormSection::new("output", "Resultado").fields(
            output_fields(c)
                .into_iter()
                .map(|f| if f.key == "into" { f.required() } else { f })
                .collect(),
        ),
        FormSection::new("query", "Consulta").field(query_field(c)),
    ];
    if c.notify {
        s.push(
            FormSection::new("notify", "Notificación")
                .list(urls_list())
                .fields(vec![
                    FormField::new("notify_events", "Notificar (ON)", events_kind()),
                    FormField::new(
                        "notify_history",
                        "Notificar también lo histórico (NOTIFY_HISTORY)",
                        yes_no_default(),
                    )
                    .default(json!("")),
                    FormField::new(
                        "on_failure",
                        "Si falla la notificación (ON_FAILURE)",
                        FormKind::select(&[
                            ("", "Por defecto (DROP)"),
                            ("drop", "Descartarla (DROP)"),
                            ("pause", "Pausar el stream (PAUSE)"),
                        ]),
                    )
                    .default(json!("")),
                ]),
        );
    }
    s
}

fn new_sections(c: &Ctx, existing: bool) -> Vec<FormSection> {
    let w = |f: FormField, t: &[&str]| f.when("trigger", t);
    vec![
        FormSection::new("general", "General").fields(general_fields(c, existing)),
        FormSection::new("trigger", "Disparo").fields(vec![
            FormField::new(
                "trigger",
                "Disparador",
                FormKind::select(&[
                    ("period", "Periódico (PERIOD)"),
                    ("sliding", "Ventana de tiempo (INTERVAL / SLIDING)"),
                    ("session", "Sesión (SESSION)"),
                    ("state", "Estado (STATE_WINDOW)"),
                    ("event", "Evento (EVENT_WINDOW)"),
                    ("count", "Cantidad de filas (COUNT_WINDOW)"),
                ]),
            )
            .required()
            .default(json!("sliding")),
            w(FormField::new("period", "Cada (PERIOD)", FormKind::Text).placeholder("1m"), &["period"]),
            w(FormField::new("period_offset", "Desfase", FormKind::Text).placeholder("10s"), &["period"]),
            w(
                FormField::new("interval", "Ventana (INTERVAL)", FormKind::Text)
                    .placeholder("1m")
                    .help("Vacío: solo SLIDING, que dispara cada paso sin ventana."),
                &["sliding"],
            ),
            w(FormField::new("interval_offset", "Desfase de la ventana", FormKind::Text).placeholder("0s"), &["sliding"]),
            w(FormField::new("sliding", "Paso (SLIDING)", FormKind::Text).placeholder("1m"), &["sliding"]),
            w(FormField::new("sliding_offset", "Desfase del paso", FormKind::Text).placeholder("0s"), &["sliding"]),
            w(FormField::new("session_col", "Columna de tiempo", FormKind::Text).placeholder("ts"), &["session"]),
            w(
                FormField::new("session_gap", "Pausa máxima entre filas", FormKind::Text).placeholder("10s"),
                &["session"],
            ),
            w(
                FormField::new("state_expr", "Estado (STATE_WINDOW)", FormKind::Text)
                    .placeholder("estado")
                    .help("La columna o expresión cuyo valor define cada ventana."),
                &["state"],
            ),
            w(FormField::new("start_with", "Empieza cuando (START WITH)", FormKind::Text).placeholder("v > 10"), &["event"]),
            w(FormField::new("end_with", "Termina cuando (END WITH)", FormKind::Text).placeholder("v < 5"), &["event"]),
            w(
                FormField::new("true_for", "Duración mínima (TRUE_FOR)", FormKind::Text).placeholder("5s"),
                &["state", "event"],
            ),
            w(FormField::new("count", "Filas por ventana", FormKind::number(Some(1.0), None)), &["count"]),
            w(FormField::new("count_sliding", "Paso (filas)", FormKind::number(Some(1.0), None)), &["count"]),
            w(
                FormField::new("count_cols", "Columnas que cuentan", FormKind::Text).placeholder("v"),
                &["count"],
            ),
            FormField::new("from_table", "Tabla que lo dispara (FROM)", FormKind::Text)
                .placeholder("base.medidores")
                .help("La tabla o supertabla cuyos datos disparan el stream; vacío en un PERIOD sin tabla."),
            FormField::new("partition_by", "Agrupar por (PARTITION BY)", FormKind::Text).placeholder("tbname"),
        ]),
        FormSection::new("options", "Opciones").fields(vec![
            FormField::new("watermark", "Watermark", FormKind::Text)
                .placeholder("10s")
                .help("Cuánto espera datos atrasados antes de cerrar una ventana."),
            FormField::new("expired_time", "Vencimiento (EXPIRED_TIME)", FormKind::Text)
                .placeholder("1d")
                .help("Los datos más viejos que esto no se recalculan."),
            FormField::new("max_delay", "Demora máxima (MAX_DELAY)", FormKind::Text).placeholder("5s"),
            FormField::new(
                "fill",
                "Datos históricos",
                FormKind::select(&[
                    ("", "No procesarlos"),
                    ("fill_history", "Procesarlos (FILL_HISTORY)"),
                    ("fill_history_first", "Procesarlos antes que los nuevos (FILL_HISTORY_FIRST)"),
                ]),
            )
            .default(json!("")),
            FormField::new("fill_start", "Procesar desde", FormKind::Text)
                .placeholder("2025-01-01 00:00:00")
                .when("fill", &["fill_history", "fill_history_first"]),
            FormField::new("ignore_disorder", "Ignorar datos fuera de orden (IGNORE_DISORDER)", FormKind::Bool),
            FormField::new("delete_recalc", "Recalcular al borrar datos (DELETE_RECALC)", FormKind::Bool),
            FormField::new(
                "delete_output_table",
                "Borrar la tabla de salida al borrar la de origen (DELETE_OUTPUT_TABLE)",
                FormKind::Bool,
            ),
            FormField::new("calc_notify_only", "Solo notificar (CALC_NOTIFY_ONLY)", FormKind::Bool),
            FormField::new("low_latency_calc", "Calcular con baja latencia (LOW_LATENCY_CALC)", FormKind::Bool),
            FormField::new("pre_filter", "Filtro previo (PRE_FILTER)", FormKind::Text).placeholder("v > 0"),
            FormField::new("force_output", "Escribir aunque no haya datos (FORCE_OUTPUT)", FormKind::Bool),
            FormField::new("event_type", "Eventos que calculan (EVENT_TYPE)", events_kind()),
            FormField::new(
                "ignore_nodata_trigger",
                "No disparar sin datos (IGNORE_NODATA_TRIGGER)",
                FormKind::Bool,
            ),
            FormField::new("timezone", "Zona horaria (CONFIG)", FormKind::Text)
                .placeholder("America/Argentina/Buenos_Aires")
                .help("La que guardó el servidor al crearlo; vacía: la de la conexión."),
            FormField::new(
                "first_day_of_week",
                "Primer día de la semana (CONFIG)",
                FormKind::select(&[
                    ("", "Por defecto"),
                    ("0", "Domingo"),
                    ("1", "Lunes"),
                    ("2", "Martes"),
                    ("3", "Miércoles"),
                    ("4", "Jueves"),
                    ("5", "Viernes"),
                    ("6", "Sábado"),
                ]),
            )
            .default(json!("")),
            FormField::new("extra_options", "Otras opciones", FormKind::Text)
                .help("Opciones de STREAM_OPTIONS que DBine no conoce, separadas por «|»."),
        ]),
        FormSection::new("notify", "Notificación").list(urls_list()).fields(vec![
            FormField::new("notify_events", "Notificar (ON)", events_kind()),
            FormField::new("notify_where", "Notificar solo si (WHERE)", FormKind::Text).placeholder("c > 0"),
            FormField::new("notify_history", "Notificar también lo histórico (NOTIFY_HISTORY)", FormKind::Bool),
            FormField::new("on_failure_pause", "Pausar el stream si falla la notificación (ON_FAILURE_PAUSE)", FormKind::Bool)
                .help("Solo en las versiones de TDengine que lo admiten."),
        ]),
        FormSection::new("output", "Resultado").fields(output_fields(c)),
        FormSection::new("query", "Consulta").field(query_field(c)),
    ]
}

pub(crate) fn form_of(
    c: &Ctx,
    current: Option<&Stream>,
    dbs: &[String],
    session_db: Option<&str>,
) -> ObjectForm {
    let mut f = ObjectForm {
        sections: if c.new_engine {
            new_sections(c, current.is_some())
        } else {
            old_sections(c, current.is_some())
        },
        ..ObjectForm::default()
    };
    let mut choices: Vec<(String, String)> = dbs.iter().map(|d| (d.clone(), d.clone())).collect();
    if !c.suspend {
        f.notes.push(jobs::NO_PAUSE.into());
    }
    let Some(st) = current else {
        f.values.insert("enabled".into(), json!(true));
        if let Some(d) = session_db.filter(|d| !SYSTEM_DBS.contains(d)) {
            f.values.insert("database".into(), json!(d));
            if !choices.iter().any(|(x, _)| x == d) {
                choices.push((d.to_string(), d.to_string()));
            }
        }
        f.choices.insert("database".into(), choices);
        return f;
    };
    f.values.insert("status".into(), json!(st.status));
    f.values.insert("created".into(), json!(st.created));
    f.values.insert("message".into(), json!(st.message));
    match current_values(c, st) {
        Some(v) => {
            for (k, x) in v.values {
                f.values.insert(k, x);
            }
            for s in f.sections.iter_mut() {
                for l in s.lists.iter_mut() {
                    if let Some(rows) = v.lists.get(&l.key) {
                        l.rows = rows.clone();
                    }
                }
            }
            f.notes.push(RECREATE_NOTE.into());
        }
        None => {
            f.read_only = true;
            f.notes.push(UNREADABLE.into());
            f.values.insert("name".into(), json!(st.name));
            if let Some(d) = db_of(st) {
                f.values.insert("database".into(), json!(d));
            }
            f.values.insert("query".into(), json!(st.sql));
        }
    }
    if let Some(d) = f.values.get("database").and_then(Value::as_str) {
        if !choices.iter().any(|(x, _)| x == d) {
            choices.push((d.to_string(), d.to_string()));
        }
    }
    f.choices.insert("database".into(), choices);
    f
}

#[cfg(test)]
mod tests;
