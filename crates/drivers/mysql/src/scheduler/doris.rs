//! Doris' (and VeloDB's) scheduler in the designer (Doris 2.1+):
//!
//! - "job": `CREATE JOB` jobs: an `INSERT … SELECT` once (`ON SCHEDULE AT`),
//!   every so often (`ON SCHEDULE EVERY n MINUTE|HOUR|DAY|WEEK STARTS …
//!   ENDS …`) or continuously (`ON STREAMING`), with a comment and
//!   `PROPERTIES`; `PAUSE JOB` / `RESUME JOB` as enabled. Doris has no
//!   `ALTER` for a scheduled job: a change drops it and creates it again
//!   (its task history, counters and creation time go; the script says it
//!   at the top) and a failed create puts the old one back. Doris doesn't
//!   create a one-time job at a past time, so one whose time has passed
//!   can't be created again: it's read-only. A streaming job is changed in
//!   place (`ALTER JOB … PROPERTIES … INSERT …`, paused while it changes,
//!   resumed if the change fails); one that copies another database's
//!   changes (`FROM … TO DATABASE`) only that way, never created again (it
//!   would read its source from the start, and its source may not
//!   round-trip). Servers whose `jobs()` has no `Properties` column have
//!   neither streaming jobs nor job properties: the form leaves them out.
//!   `jobs()` doesn't say which database a job was created in, where its
//!   statement's bare table names point: the form asks for it when the job
//!   is created again.
//!   Only a streaming job takes `PROPERTIES` (a scheduled job's are
//!   accepted, but neither kept nor applied), so the properties tab, like
//!   the job's progress, shows only for that type. A job has no actions:
//!   Doris has no statement that runs one now (checked up to 4.1.3: its
//!   grammar has only CREATE, ALTER, PAUSE, RESUME and DROP JOB and CANCEL
//!   TASK).
//! - "mv_refresh": how an asynchronous materialized view refreshes: method
//!   (`AUTO`, `COMPLETE`), trigger (`ON MANUAL`, `ON COMMIT`, `ON SCHEDULE
//!   EVERY … STARTS …`), its properties (`SET (…)`) and its refresh job
//!   paused or not, all changed in place (`ALTER MATERIALIZED VIEW`). Views
//!   are created and dropped with the view, not here (their rows can't be
//!   deleted). The job list's `mv:` rows open this form under "job" too.
//!   Actions: refresh now, what changed (`REFRESH MATERIALIZED VIEW …
//!   AUTO`) or all of it (`… COMPLETE`).
//! - Neither kind has steps, and no statement deletes a job's tasks.

use super::{
    action_keys, bad, databases, datetime, form_time, name, no_action, no_kind, one_line, pick,
    properties_list, properties_sql, property_changes, read_properties, refresh_head, shown,
    with_refresh, Plan, Vals, REFRESH, REFRESH_COMPLETE,
};
use crate::jobs::doris::{self as dj, Job, Strategy, Type};
use crate::jobs::{quoted, rows_if_present, text};
use crate::session::{lit, MySqlSession};
use dbine_driver::{
    Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerInfo,
    SchedulerItem, SchedulerKind, SchedulerList,
};
use mysql_async::prelude::Queryable;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const ADMIN_ONLY: &str = "Crear, cambiar y borrar jobs de Doris requiere el privilegio ADMIN.";
const VIEWS_NOTE: &str = "Los refrescos de las vistas materializadas se crean y se borran con la vista: acá se cambia cuándo y cómo se refrescan.";
const NO_JOBS: &str = "Esta versión de Doris no tiene jobs (llegaron en Doris 2.1).";
const RECREATED: &str = "Doris no cambia un job programado: guardar un cambio lo borra y lo vuelve a crear, y se pierden su historial de tareas, sus contadores y su fecha de creación.";
const UNKNOWN_DB: &str = "Doris no informa en qué base se creó el job: si su instrucción nombra tablas sin la base, elegí la base antes de guardar un cambio que lo vuelva a crear.";
const STREAMING_NOTE: &str = "Un job continuo se cambia en su lugar (ALTER JOB) mientras está pausado: el script lo pausa y, si queda habilitado, lo reanuda.";
const CDC_NOTE: &str = "Este job continuo copia los cambios de otra base (FROM … TO DATABASE): DBine cambia sus propiedades y su estado; para cambiar su origen hay que borrarlo y crearlo de nuevo con SQL.";
const VIEW_CREATED_WITH: &str =
    "El refresco de una vista materializada se crea y se borra con la vista.";

/// The units `EVERY` takes (Doris refuses seconds in production mode and
/// months).
pub(super) const UNITS: [&str; 4] = ["MINUTE", "HOUR", "DAY", "WEEK"];

// ---------------------------------------------------------------------------
// Kinds and what the login may do
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Job,
    MvRefresh,
}

fn kind_of(kind: &str) -> Result<Kind> {
    match kind.trim() {
        "job" => Ok(Kind::Job),
        "mv_refresh" => Ok(Kind::MvRefresh),
        other => Err(no_kind(other)),
    }
}

/// Doris has `jobs()` (2.1+).
async fn has_jobs(s: &mut MySqlSession) -> Result<bool> {
    match rows_if_present(s, "SELECT * FROM jobs('type'='mv') LIMIT 0").await {
        Ok(found) => Ok(found.is_some()),
        Err(e) if crate::jobs::denied(&e) => Ok(true),
        Err(e) => Err(e),
    }
}

pub(super) async fn info(s: &mut MySqlSession) -> Result<SchedulerInfo> {
    if !has_jobs(s).await? {
        return Ok(SchedulerInfo {
            kinds: Vec::new(),
            logs: false,
            notes: vec![NO_JOBS.into()],
        });
    }
    let admin = dj::is_admin(s).await;
    let mut notes = Vec::new();
    if !admin {
        notes.push(ADMIN_ONLY.to_string());
    }
    notes.push(VIEWS_NOTE.to_string());
    // A job runs only on its schedule: Doris has no statement that runs one
    // now (PAUSE / RESUME are its enabled switch). The refresh actions are
    // the views' rows', listed among the jobs too. A job runs one statement
    // (no steps), and no statement deletes its tasks.
    Ok(SchedulerInfo {
        kinds: vec![
            with_refresh(SchedulerKind::new("job", "Jobs", "job").abilities(admin, admin, admin))
                .run_from_step(false)
                .clear_history(false),
            with_refresh(
                SchedulerKind::new(
                    "mv_refresh",
                    "Refrescos de vistas materializadas",
                    "refresco de vista materializada",
                )
                .abilities(false, true, false),
            )
            .run_from_step(false)
            .clear_history(false),
        ],
        logs: false,
        notes,
    })
}

async fn jobs_of(s: &mut MySqlSession, t: Type, only: Option<&str>) -> Result<Vec<Job>> {
    match dj::jobs_of(s, t, only).await {
        Ok(j) => Ok(j.unwrap_or_default()),
        Err(e) if only.is_none() && crate::jobs::denied(&e) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// A row of the list, with what can be done with it: a view's job (Doris'
/// own, made and dropped with the view) has its refresh edited and run, not
/// deleted; a job has no actions, and a one-time job that already ran has
/// nothing left to change.
pub(super) fn item(j: &Job) -> SchedulerItem {
    let mv = j.ty() == Type::Mv;
    let (can_edit, can_delete, actions) = if mv {
        let refresh: &[&str] = if switchable(j) {
            &[REFRESH, REFRESH_COMPLETE]
        } else {
            &[]
        };
        (Some(true), Some(false), action_keys(refresh))
    } else {
        let ran = j.execute_type == "ONE_TIME" && j.status == "FINISHED";
        (ran.then_some(false), None, action_keys(&[]))
    };
    SchedulerItem {
        id: dj::job_id(j.ty(), &j.name),
        name: if mv {
            j.mv_name.clone().unwrap_or_else(|| j.name.clone())
        } else {
            j.name.clone()
        },
        description: if mv {
            Some(match (&j.mv_db, &j.strategy) {
                (Some(db), Some(st)) => format!("{db} · {st}"),
                (Some(db), None) => db.clone(),
                (None, st) => st.clone().unwrap_or_default(),
            })
        } else {
            j.comment.clone().or_else(|| j.strategy.clone())
        },
        enabled: Some(j.active()),
        system: false,
        can_edit,
        can_delete,
        actions,
    }
}

pub(super) async fn items(s: &mut MySqlSession, kind: &str) -> Result<SchedulerList> {
    let kind = kind_of(kind)?;
    let mut jobs = Vec::new();
    if kind == Kind::Job {
        jobs.extend(jobs_of(s, Type::Insert, None).await?);
    }
    jobs.extend(jobs_of(s, Type::Mv, None).await?);
    let mut items: Vec<SchedulerItem> = jobs.iter().map(item).collect();
    items.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(SchedulerList {
        items,
        notes: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// Jobs: the spec, its scripts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Mode {
    At(String),
    Every {
        n: u64,
        unit: String,
        starts: Option<String>,
        ends: Option<String>,
    },
    Streaming,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct JobSpec {
    pub(super) name: String,
    /// Where the job's statement runs (`USE` before `CREATE JOB`).
    pub(super) database: Option<String>,
    pub(super) enabled: bool,
    pub(super) comment: String,
    pub(super) mode: Mode,
    pub(super) properties: Vec<(String, String)>,
    /// Without a trailing `;`.
    pub(super) sql: String,
}

/// A streaming job that reads another database (`FROM MYSQL (…) TO
/// DATABASE …`) rather than running an `INSERT`.
pub(super) fn is_cdc(spec: &JobSpec) -> bool {
    spec.mode == Mode::Streaming && !starts_with_word(&spec.sql, "INSERT")
}

fn starts_with_word(sql: &str, word: &str) -> bool {
    let t = sql.trim_start();
    t.get(..word.len())
        .is_some_and(|w| w.eq_ignore_ascii_case(word))
        && t[word.len()..]
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == '(')
}

fn mode_sql(m: &Mode) -> String {
    match m {
        Mode::At(at) => format!("ON SCHEDULE AT {}", lit(at)),
        Mode::Every {
            n,
            unit,
            starts,
            ends,
        } => Strategy::Every {
            n: *n,
            unit: unit.clone(),
            starts: starts.clone(),
            ends: ends.clone(),
        }
        .sql()
        .replacen("EVERY", "ON SCHEDULE EVERY", 1),
        Mode::Streaming => "ON STREAMING".into(),
    }
}

/// `CREATE JOB`, then `PAUSE JOB` for a job created disabled.
pub(super) fn create_steps(j: &JobSpec) -> Vec<String> {
    let mut steps = Vec::new();
    if let Some(db) = &j.database {
        steps.push(format!("USE {}", quoted(db)));
    }
    let mut st = format!("CREATE JOB {}", quoted(&j.name));
    if !j.properties.is_empty() {
        st.push_str(&format!("\nPROPERTIES {}", properties_sql(&j.properties)));
    }
    st.push('\n');
    st.push_str(&mode_sql(&j.mode));
    if !j.comment.is_empty() {
        st.push_str(&format!("\nCOMMENT {}", comment_lit(&j.comment)));
    }
    if is_cdc(j) {
        st.push('\n');
    } else {
        st.push_str("\nDO ");
    }
    st.push_str(&j.sql);
    steps.push(st);
    if !j.enabled {
        steps.push(pause_job(&j.name, false));
    }
    steps
}

/// A job's `COMMENT`: Doris keeps a doubled quote there as two, so quotes
/// and backslashes go escaped with a backslash.
pub(super) fn comment_lit(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn pause_job(name: &str, resume: bool) -> String {
    format!(
        "{} JOB WHERE jobname = {}",
        if resume { "RESUME" } else { "PAUSE" },
        lit(name)
    )
}

fn drop_job(name: &str) -> String {
    format!("DROP JOB WHERE jobname = {}", lit(name))
}

pub(super) fn create_plan(j: &JobSpec) -> Plan {
    Plan {
        head: vec![format!("Crea el job «{}» de Doris", j.name)],
        steps: create_steps(j),
        id: Some(dj::job_id(Type::Insert, &j.name)),
        ..Plan::default()
    }
}

/// Nothing but whether it's enabled differs.
fn same_but_enabled(cur: &JobSpec, want: &JobSpec) -> bool {
    JobSpec {
        enabled: cur.enabled,
        database: cur.database.clone(),
        ..want.clone()
    } == *cur
}

/// A streaming job's change that `ALTER JOB` makes: its properties set
/// (not taken away) and its `INSERT`.
fn in_place(cur: &JobSpec, want: &JobSpec) -> bool {
    let (_, removed) = property_changes(&cur.properties, &want.properties);
    cur.mode == Mode::Streaming
        && want.mode == Mode::Streaming
        && cur.name == want.name
        && cur.comment == want.comment
        && removed.is_empty()
        && (!is_cdc(cur) || cur.sql == want.sql)
}

/// The change drops the job and creates it again.
pub(super) fn recreates(cur: &JobSpec, want: &JobSpec) -> bool {
    !same_but_enabled(cur, want) && !in_place(cur, want)
}

/// Why a job that copies another database's changes can't take this
/// change: only dropping and creating it again would make it.
pub(super) fn cdc_recreate(cur: &JobSpec, want: &JobSpec) -> Option<&'static str> {
    if !is_cdc(cur) || !recreates(cur, want) {
        return None;
    }
    let (_, removed) = property_changes(&cur.properties, &want.properties);
    Some(if cur.name != want.name {
        "Doris no renombra un job: habría que borrarlo y crearlo de nuevo, y DBine no vuelve a crear un job que copia los cambios de otra base (FROM … TO DATABASE), porque volvería a leerla desde el principio y su origen puede no crearse igual. Hacelo con SQL."
    } else if cur.comment != want.comment {
        "Doris no cambia el comentario de un job: habría que borrarlo y crearlo de nuevo, y DBine no vuelve a crear un job que copia los cambios de otra base (FROM … TO DATABASE), porque volvería a leerla desde el principio y su origen puede no crearse igual. Hacelo con SQL."
    } else if !removed.is_empty() {
        "Doris no le quita una propiedad a un job: habría que borrarlo y crearlo de nuevo, y DBine no vuelve a crear un job que copia los cambios de otra base (FROM … TO DATABASE), porque volvería a leerla desde el principio y su origen puede no crearse igual. Dejala con el valor que corresponda o hacelo con SQL."
    } else {
        "Un job que copia los cambios de otra base (FROM … TO DATABASE) no cambia de tipo: habría que borrarlo y crearlo de nuevo, y DBine no lo hace, porque volvería a leerla desde el principio y su origen puede no crearse igual. Hacelo con SQL."
    })
}

/// A one-time job whose time has passed (or that already ran): Doris
/// doesn't create one at a past time, so it can't be created again.
pub(super) fn once_past(spec: &JobSpec, status: &str, now: Option<&str>) -> bool {
    match &spec.mode {
        Mode::At(t) => status == "FINISHED" || now.is_some_and(|n| t.as_str() <= n),
        _ => false,
    }
}

/// A one-time job's time is still ahead on the server's clock.
pub(super) fn not_past(spec: &JobSpec, now: Option<&str>) -> Result<()> {
    match (&spec.mode, now) {
        (Mode::At(t), Some(n)) if t.as_str() <= n => Err(bad(format!(
            "«Fecha y hora» ya pasó (en el servidor son las {n}): Doris no crea un job de una sola vez con una hora pasada."
        ))),
        _ => Ok(()),
    }
}

const PAST_ONCE: &str = "Este job de una sola vez ya pasó su hora: Doris no crea un job con una hora pasada, así que DBine no lo cambia (habría que borrarlo y no se podría volver a crear). Para repetirlo, creá otro job.";

/// From `cur` (as the server has it, `cur.database` as the form says) to
/// `want`: in place where Doris can, else dropped and created again.
pub(super) fn alter_plan(cur: &JobSpec, want: &JobSpec, switchable: bool) -> Option<Plan> {
    let id = Some(dj::job_id(Type::Insert, &want.name));
    let enabled_changed = switchable && cur.enabled != want.enabled;
    if same_but_enabled(cur, want) {
        if !enabled_changed {
            return None;
        }
        return Some(Plan {
            head: vec![format!(
                "{} el job «{}» de Doris",
                if want.enabled { "Reanuda" } else { "Pausa" },
                cur.name
            )],
            steps: vec![pause_job(&cur.name, want.enabled)],
            id,
            ..Plan::default()
        });
    }
    if in_place(cur, want) {
        let (set, _) = property_changes(&cur.properties, &want.properties);
        let mut st = format!("ALTER JOB {}", quoted(&cur.name));
        if !set.is_empty() {
            st.push_str(&format!("\nPROPERTIES {}", properties_sql(&set)));
        }
        if cur.sql != want.sql {
            st.push('\n');
            st.push_str(&want.sql);
        }
        let mut steps = Vec::new();
        if let Some(db) = &want.database {
            steps.push(format!("USE {}", quoted(db)));
        }
        let mut head = vec![
            format!("Cambia el job continuo «{}» de Doris", cur.name),
            STREAMING_NOTE.into(),
        ];
        // A running job paused for the change runs again if it fails.
        let mut undo_from = None;
        let mut undo = Vec::new();
        if cur.enabled {
            undo_from = Some(steps.len());
            undo.push(pause_job(&cur.name, true));
            steps.push(pause_job(&cur.name, false));
            head.push("Si el cambio falla, DBine reanuda el job.".into());
        }
        steps.push(st);
        if want.enabled {
            steps.push(pause_job(&cur.name, true));
        }
        return Some(Plan {
            head,
            steps,
            undo_from,
            undo,
            id,
            ..Plan::default()
        });
    }
    // Dropped and created again; the old one back if the create fails.
    let mut steps = Vec::new();
    let mut undo = Vec::new();
    if let Some(db) = &want.database {
        steps.push(format!("USE {}", quoted(db)));
    }
    let drop_at = steps.len();
    steps.push(drop_job(&cur.name));
    steps.extend(
        create_steps(want)
            .into_iter()
            .filter(|s| !s.starts_with("USE ")),
    );
    let old = JobSpec {
        database: want.database.clone(),
        ..cur.clone()
    };
    undo.extend(create_steps(&old));
    let mut head = vec![
        format!("Cambia el job «{}» de Doris", cur.name),
        RECREATED.into(),
    ];
    if cur.mode == Mode::Streaming {
        head.push("Un job continuo creado de nuevo vuelve a leer desde el principio: pierde hasta dónde había leído.".into());
    }
    head.push("Si la creación falla, DBine vuelve a crear el job como estaba.".into());
    Some(Plan {
        head,
        steps,
        undo_from: Some(drop_at),
        undo,
        id,
        ..Plan::default()
    })
}

/// A job's spec as the server has it.
pub(super) fn spec_of(j: &Job) -> JobSpec {
    let mode = if j.execute_type == "STREAMING" {
        Mode::Streaming
    } else {
        match j.strategy() {
            Some(Strategy::At(at)) => Mode::At(super::parse_datetime(&at).unwrap_or(at)),
            Some(Strategy::Every {
                n,
                unit,
                starts,
                ends,
            }) => Mode::Every {
                n,
                unit,
                starts: starts.map(|t| super::parse_datetime(&t).unwrap_or(t)),
                ends: ends.map(|t| super::parse_datetime(&t).unwrap_or(t)),
            },
            None => Mode::At(String::new()),
        }
    };
    JobSpec {
        name: j.name.clone(),
        database: None,
        enabled: j.active(),
        comment: j.comment.clone().unwrap_or_default(),
        mode,
        properties: j
            .properties
            .as_deref()
            .map(parse_properties)
            .unwrap_or_default(),
        sql: body_text(j.sql.as_deref().unwrap_or("")),
    }
}

fn body_text(s: &str) -> String {
    s.trim().trim_end_matches(';').trim_end().to_string()
}

/// A job's or a view's properties as Doris prints them: a JSON object,
/// `{k=v, k2=v2}` (a Java map) or `('k'='v', …)`.
pub(super) fn parse_properties(s: &str) -> Vec<(String, String)> {
    let t = s.trim();
    if t.is_empty() || t == "{}" || t == "\\N" {
        return Vec::new();
    }
    if let Ok(Value::Object(o)) = serde_json::from_str::<Value>(t) {
        return o
            .into_iter()
            .map(|(k, v)| {
                let v = match v {
                    Value::String(s) => s,
                    v => v.to_string(),
                };
                (k, v)
            })
            .collect();
    }
    if t.starts_with('(') {
        return crate::jobs::starrocks::parse_properties(t);
    }
    let inner = t
        .strip_prefix('{')
        .and_then(|x| x.strip_suffix('}'))
        .unwrap_or(t);
    // Entries split at ", " when a `key=` follows.
    let mut out: Vec<(String, String)> = Vec::new();
    let mut rest = inner;
    while !rest.is_empty() {
        let mut cut = rest.len();
        let mut from = 0;
        while let Some(i) = rest[from..].find(", ") {
            let at = from + i;
            let next = &rest[at + 2..];
            let key_len = next.find('=').filter(|&e| {
                e > 0
                    && next[..e]
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
            });
            if key_len.is_some() {
                cut = at;
                break;
            }
            from = at + 2;
        }
        let entry = &rest[..cut];
        if let Some((k, v)) = entry.split_once('=') {
            out.push((k.trim().to_string(), v.to_string()));
        }
        rest = if cut < rest.len() {
            &rest[cut + 2..]
        } else {
            ""
        };
    }
    out
}

/// The job the form asks for. `streaming`: the server has streaming jobs
/// and job properties.
pub(super) fn job_spec_from(
    values: &FormValues,
    dbs: &[(String, String)],
    cur: Option<&JobSpec>,
    streaming: bool,
) -> Result<JobSpec> {
    let v = Vals(&values.values);
    let job = name(v, "name", "Nombre", 128)?;
    let database = match v.opt("database") {
        None => None,
        Some(d) if dbs.is_empty() => Some(d),
        Some(d) => Some(
            pick(&d, dbs, |s| {
                format!("No existe la base «{s}» o este usuario no la ve.")
            })?
            .to_string(),
        ),
    };
    let mode = match v.choice("type", "Tipo", &["at", "every", "streaming"], "every")? {
        "at" => Mode::At(
            datetime(v, "at", "Fecha y hora")?
                .ok_or_else(|| bad("Un job de una sola vez necesita «Fecha y hora».".into()))?,
        ),
        "streaming" => Mode::Streaming,
        _ => {
            let n = v
                .int("every", "Cada", 1, i64::from(i32::MAX))?
                .ok_or_else(|| bad("Un job recurrente necesita «Cada».".into()))?;
            let unit = v.choice("unit", "Unidad", &UNITS, "HOUR")?;
            let starts = datetime(v, "starts", "Desde")?;
            let ends = datetime(v, "ends", "Hasta")?;
            if let (Some(a), Some(b)) = (&starts, &ends) {
                if b <= a {
                    return Err(bad("«Hasta» tiene que ser posterior a «Desde».".into()));
                }
            }
            Mode::Every {
                n: n as u64,
                unit: unit.to_string(),
                starts,
                ends,
            }
        }
    };
    let sql = body_text(&v.raw("sql"));
    if sql.is_empty() {
        return Err(bad("El job necesita una instrucción.".into()));
    }
    let spec = JobSpec {
        name: job,
        database,
        enabled: v.bool("enabled", true),
        comment: v.raw("comment").trim().to_string(),
        mode,
        properties: read_properties(values)?,
        sql,
    };
    let cdc_unchanged = cur.is_some_and(|c| is_cdc(c) && c.sql == spec.sql);
    if !starts_with_word(&spec.sql, "INSERT") && !cdc_unchanged {
        return Err(bad(
            "La instrucción de un job de Doris tiene que ser un INSERT (INSERT INTO … SELECT …)."
                .into(),
        ));
    }
    if !super::one_statement(&spec.sql, false) {
        return Err(bad(ONE_STATEMENT.into()));
    }
    if !streaming {
        if spec.mode == Mode::Streaming {
            return Err(bad(NO_STREAMING_JOBS.into()));
        }
        if !spec.properties.is_empty() {
            return Err(bad(NO_JOB_PROPERTIES.into()));
        }
    }
    // A scheduled job's PROPERTIES are accepted but neither kept nor
    // applied: refused rather than shown in a script that loses them (and
    // read back as none, which would drop and create the job for nothing).
    if spec.mode != Mode::Streaming && !spec.properties.is_empty() {
        return Err(bad(SCHEDULED_PROPERTIES.into()));
    }
    Ok(spec)
}

const SCHEDULED_PROPERTIES: &str = "Doris solo aplica propiedades a un job continuo: en un job programado las acepta pero no las guarda ni las usa.";
const ONE_STATEMENT: &str = "La instrucción del job tiene que ser una sola, sin «;» en el medio: el servidor guardaría en el job solo lo anterior y ejecutaría el resto al guardar.";
const NO_STREAMING_JOBS: &str = "Esta versión de Doris no tiene jobs continuos (ON STREAMING).";
const NO_JOB_PROPERTIES: &str = "Esta versión de Doris no toma propiedades en un job.";
const OLD_JOBS_NOTE: &str = "Esta versión de Doris no tiene jobs continuos (ON STREAMING) ni propiedades de jobs: el formulario no los ofrece.";

/// The server has streaming jobs and job properties (later Doris
/// versions): `jobs()` then has a `Properties` column.
async fn has_streaming(s: &mut MySqlSession) -> bool {
    let Ok(r) = s
        .conn
        .query_iter("SELECT * FROM jobs('type'='insert') LIMIT 0")
        .await
    else {
        return false;
    };
    let has = r.columns().is_some_and(|c| {
        c.iter()
            .any(|c| c.name_str().eq_ignore_ascii_case("Properties"))
    });
    if let Err(e) = r.drop_result().await {
        tracing::debug!("Doris scheduler: reading jobs()' columns: {e}");
    }
    has
}

/// The server's clock (`NOW()`, on the session's zone, where a job's times
/// are read), as `YYYY-MM-DD HH:MM:SS`.
async fn server_now(s: &mut MySqlSession) -> Option<String> {
    s.optional_rows("SELECT NOW()")
        .await
        .first()
        .and_then(|r| crate::session::at(r, 0))
        .and_then(|t| super::parse_datetime(&t))
}

/// The database a job created again needs: its statement's bare table
/// names point there.
fn needs_database(spec: &JobSpec) -> bool {
    spec.database.is_none() && !is_cdc(spec) && dj::names_bare_tables(&spec.sql)
}

const NEEDS_DATABASE: &str = "La instrucción nombra tablas sin la base y Doris no informa en qué base se creó el job: elegí «Base de datos» antes de guardar (el job se vuelve a crear en ella).";

// ---------------------------------------------------------------------------
// Materialized views' refresh
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Trigger {
    Manual,
    Commit,
    Schedule {
        n: u64,
        unit: String,
        starts: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct MvSpec {
    pub(super) method: String,
    pub(super) trigger: Trigger,
    pub(super) enabled: bool,
    pub(super) properties: Vec<(String, String)>,
}

/// What `mv_infos()` says of the view's refresh, `BUILD DEFERRED REFRESH
/// AUTO ON SCHEDULE EVERY 1 HOUR STARTS "2030-01-01 00:00:00"`: how it was
/// built, its method and its trigger.
pub(super) fn refresh_info(s: &str) -> Option<(String, String, Trigger)> {
    let toks: Vec<&str> = s.split_whitespace().collect();
    let up = |i: usize| toks.get(i).map(|t| t.to_ascii_uppercase());
    let build = toks
        .iter()
        .position(|t| t.eq_ignore_ascii_case("BUILD"))
        .and_then(|i| up(i + 1))
        .unwrap_or_default();
    let r = toks
        .iter()
        .position(|t| t.eq_ignore_ascii_case("REFRESH"))?;
    let method = up(r + 1)?;
    if up(r + 2)?.as_str() != "ON" {
        return None;
    }
    let trigger = match up(r + 3)?.as_str() {
        "MANUAL" => Trigger::Manual,
        "COMMIT" => Trigger::Commit,
        "SCHEDULE" => {
            if up(r + 4)?.as_str() != "EVERY" {
                return None;
            }
            let n = toks.get(r + 5)?.parse().ok()?;
            let unit = up(r + 6)?;
            let starts = (up(r + 7).as_deref() == Some("STARTS")).then(|| {
                toks[r + 8..]
                    .join(" ")
                    .trim_matches(|c| c == '"' || c == '\'')
                    .to_string()
            });
            Trigger::Schedule {
                n,
                unit,
                starts: starts.map(|t| super::parse_datetime(&t).unwrap_or(t)),
            }
        }
        _ => return None,
    };
    Some((build, method, trigger))
}

fn trigger_sql(t: &Trigger) -> String {
    match t {
        Trigger::Manual => "ON MANUAL".into(),
        Trigger::Commit => "ON COMMIT".into(),
        Trigger::Schedule { n, unit, starts } => {
            let mut s = format!("ON SCHEDULE EVERY {n} {unit}");
            if let Some(st) = starts {
                s.push_str(&format!(" STARTS {}", lit(st)));
            }
            s
        }
    }
}

/// The view's refresh from `cur` to `want`, in place.
pub(super) fn mv_alter_plan(
    view: &str,
    label: &str,
    cur: &MvSpec,
    want: &MvSpec,
) -> Result<Option<Plan>> {
    let (set, removed) = property_changes(&cur.properties, &want.properties);
    if !removed.is_empty() {
        return Err(bad(format!(
            "Doris no quita una propiedad de una vista materializada ({}): dejala con el valor que corresponda.",
            removed.join(", ")
        )));
    }
    let mut steps = Vec::new();
    if !set.is_empty() {
        steps.push(format!(
            "ALTER MATERIALIZED VIEW {view} SET {}",
            properties_sql(&set)
        ));
    }
    if cur.method != want.method || cur.trigger != want.trigger {
        steps.push(format!(
            "ALTER MATERIALIZED VIEW {view} REFRESH {} {}",
            want.method,
            trigger_sql(&want.trigger)
        ));
    }
    if cur.enabled != want.enabled {
        steps.push(format!(
            "{} MATERIALIZED VIEW JOB ON {view}",
            if want.enabled { "RESUME" } else { "PAUSE" }
        ));
    }
    if steps.is_empty() {
        return Ok(None);
    }
    Ok(Some(Plan {
        head: vec![format!(
            "Cambia el refresco de la vista materializada «{label}»"
        )],
        steps,
        ..Plan::default()
    }))
}

pub(super) fn mv_spec_from(values: &FormValues) -> Result<MvSpec> {
    let v = Vals(&values.values);
    let method = v.choice("method", "Método", &["AUTO", "COMPLETE"], "AUTO")?;
    let trigger = match v.choice(
        "trigger",
        "Disparador",
        &["MANUAL", "COMMIT", "SCHEDULE"],
        "MANUAL",
    )? {
        "COMMIT" => Trigger::Commit,
        "SCHEDULE" => {
            let n = v
                .int("every", "Cada", 1, i64::from(i32::MAX))?
                .ok_or_else(|| bad("Un refresco programado necesita «Cada».".into()))?;
            Trigger::Schedule {
                n: n as u64,
                unit: v.choice("unit", "Unidad", &UNITS, "HOUR")?.to_string(),
                starts: datetime(v, "starts", "Desde")?,
            }
        }
        _ => Trigger::Manual,
    };
    Ok(MvSpec {
        method: method.to_string(),
        trigger,
        enabled: v.bool("enabled", true),
        properties: read_properties(values)?,
    })
}

/// A view's refresh as the server has it.
struct MvState {
    job: Job,
    db: String,
    mv: String,
    spec: MvSpec,
    build: String,
    state: Option<String>,
    refresh_state: Option<String>,
}

async fn read_mv(s: &mut MySqlSession, id: &str) -> Result<MvState> {
    let (t, job_name) = dj::parse_id(id)?;
    if t != Type::Mv {
        return Err(crate::jobs::bad_id(id, "refresco de vista materializada"));
    }
    let job = jobs_of(s, Type::Mv, Some(&job_name))
        .await?
        .into_iter()
        .find(|j| j.name == job_name)
        .ok_or_else(|| dj::not_found(&job_name))?;
    let (Some(db), Some(mv)) = (job.mv_db.clone(), job.mv_name.clone()) else {
        return Err(dj::not_found(&job_name));
    };
    let rows = s
        .rows(&format!(
            "SELECT * FROM mv_infos('database'={}) WHERE Name = {}",
            lit(&db),
            lit(&mv)
        ))
        .await?;
    let r = rows.first().ok_or_else(|| dj::not_found(&job_name))?;
    let info = text(r, &["RefreshInfo"]).unwrap_or_default();
    let (build, method, trigger) = refresh_info(&info).ok_or_else(|| {
        Error::Query(format!(
            "No se entiende cómo se refresca la vista «{db}.{mv}»: {}",
            shown(&info)
        ))
    })?;
    let spec = MvSpec {
        method,
        trigger,
        enabled: job.active(),
        properties: text(r, &["MvProperties"])
            .as_deref()
            .map(parse_properties)
            .unwrap_or_default(),
    };
    Ok(MvState {
        state: text(r, &["State"]),
        refresh_state: text(r, &["RefreshState"]),
        job,
        db,
        mv,
        spec,
        build,
    })
}

fn view_sql(st: &MvState) -> String {
    format!("{}.{}", quoted(&st.db), quoted(&st.mv))
}

fn mv_unit_select() -> FormKind {
    FormKind::select(&[
        ("MINUTE", "Minutos"),
        ("HOUR", "Horas"),
        ("DAY", "Días"),
        ("WEEK", "Semanas"),
    ])
}

fn mv_form(st: &MvState) -> ObjectForm {
    let mut f = ObjectForm::default();
    let mut put = |k: &str, v: Value| {
        f.values.insert(k.to_string(), v);
    };
    put("view", json!(format!("{}.{}", st.db, st.mv)));
    put("job", json!(st.job.name));
    put("build", json!(st.build));
    put("state", json!(st.state.clone().unwrap_or_default()));
    put(
        "refresh_state",
        json!(st.refresh_state.clone().unwrap_or_default()),
    );
    put("method", json!(st.spec.method));
    put("enabled", json!(st.spec.enabled));
    match &st.spec.trigger {
        Trigger::Manual => put("trigger", json!("MANUAL")),
        Trigger::Commit => put("trigger", json!("COMMIT")),
        Trigger::Schedule { n, unit, starts } => {
            put("trigger", json!("SCHEDULE"));
            put("every", json!(n));
            put("unit", json!(unit));
            put("starts", form_time(starts.as_deref()));
        }
    }
    f.sections = vec![
        FormSection::new("refresh", "Refresco").fields(vec![
            FormField::new("view", "Vista materializada", FormKind::Text).read_only(true),
            FormField::new(
                "method",
                "Método",
                FormKind::select(&[
                    ("AUTO", "Automático: solo lo que cambió (AUTO)"),
                    ("COMPLETE", "Completo (COMPLETE)"),
                ]),
            )
            .default(json!("AUTO")),
            FormField::new(
                "trigger",
                "Disparador",
                FormKind::select(&[
                    ("MANUAL", "Manual (ON MANUAL)"),
                    ("COMMIT", "Al cambiar las tablas base (ON COMMIT)"),
                    ("SCHEDULE", "Programado (ON SCHEDULE)"),
                ]),
            )
            .default(json!("MANUAL")),
            FormField::new("every", "Cada", FormKind::number(Some(1.0), None))
                .required()
                .when("trigger", &["SCHEDULE"])
                .default(json!(1)),
            FormField::new("unit", "Unidad", mv_unit_select())
                .when("trigger", &["SCHEDULE"])
                .default(json!("HOUR")),
            FormField::new("starts", "Desde", FormKind::DateTime)
                .help("Vacío: desde que se guarda.")
                .when("trigger", &["SCHEDULE"]),
            FormField::new("enabled", "Habilitado", FormKind::Bool)
                .help("Pausa o reanuda el job que refresca la vista.")
                .default(json!(true)),
            FormField::new("job", "Job", FormKind::Text).read_only(true),
            FormField::new("build", "Construcción", FormKind::Text).read_only(true),
            FormField::new("state", "Estado de la vista", FormKind::Text).read_only(true),
            FormField::new("refresh_state", "Estado del último refresco", FormKind::Text)
                .read_only(true),
        ]),
        FormSection::new("properties", "Propiedades").list(properties_list(
            &st.spec.properties,
            "Las propiedades de la vista: grace_period, refresh_partition_num, excluded_trigger_tables, workload_group, partition_sync_limit…",
        )),
    ];
    f
}

// ---------------------------------------------------------------------------
// The contract's calls
// ---------------------------------------------------------------------------

fn job_unit_select() -> FormKind {
    FormKind::select(&[
        ("MINUTE", "Minutos"),
        ("HOUR", "Horas"),
        ("DAY", "Días"),
        ("WEEK", "Semanas"),
    ])
}

const JOB_PROPS_HELP: &str = "Las de un job continuo: max_interval, s3.max_batch_files, s3.max_batch_bytes y variables de sesión como session.query_timeout.";

/// The job form's sections; without `streaming` (an older Doris), no
/// streaming type and no properties. `props`: the job's properties.
///
/// Only a streaming job takes properties: Doris accepts a `PROPERTIES`
/// clause on a scheduled job but neither keeps nor applies it (its
/// `session.*` variables don't reach the job's tasks), so the tab and its
/// list show only for that type, as does a streaming job's progress.
pub(super) fn job_sections(
    existing: bool,
    streaming: bool,
    props: &[(String, String)],
) -> Vec<FormSection> {
    let mut general = FormSection::new("general", "General").fields(vec![
        FormField::new("name", "Nombre", FormKind::Text).required(),
        FormField::new(
            "database",
            "Base de datos",
            FormKind::reference("database", false),
        )
        .help("La base en la que corre la instrucción: ahí se buscan sus tablas sin la base."),
        FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
        FormField::new("comment", "Comentario", FormKind::Text),
    ]);
    if existing {
        general = general.fields(vec![
            FormField::new("definer", "Definidor", FormKind::Text).read_only(true),
            FormField::new("status", "Estado", FormKind::Text).read_only(true),
            FormField::new("created", "Creado", FormKind::DateTime).read_only(true),
            FormField::new("succeeded", "Tareas correctas", FormKind::Text).read_only(true),
            FormField::new("failed", "Tareas con error", FormKind::Text).read_only(true),
            FormField::new("canceled", "Tareas canceladas", FormKind::Text).read_only(true),
            FormField::new("error", "Último error", FormKind::Textarea).read_only(true),
        ]);
    }
    let types = if streaming {
        FormKind::select(&[
            ("at", "Una sola vez (AT)"),
            ("every", "Recurrente (EVERY)"),
            ("streaming", "Continuo (ON STREAMING)"),
        ])
    } else {
        FormKind::select(&[("at", "Una sola vez (AT)"), ("every", "Recurrente (EVERY)")])
    };
    let mut sections = vec![
        general,
        FormSection::new("schedule", "Programación").fields(vec![
            FormField::new("type", "Tipo", types).default(json!("every")),
            FormField::new("at", "Fecha y hora", FormKind::DateTime)
                .required()
                .when("type", &["at"]),
            FormField::new("every", "Cada", FormKind::number(Some(1.0), None))
                .required()
                .when("type", &["every"])
                .default(json!(1)),
            FormField::new("unit", "Unidad", job_unit_select())
                .when("type", &["every"])
                .default(json!("HOUR")),
            FormField::new("starts", "Desde", FormKind::DateTime)
                .help("Vacío: un intervalo después de crearlo.")
                .when("type", &["every"]),
            FormField::new("ends", "Hasta", FormKind::DateTime)
                .help("Vacío: sin fin.")
                .when("type", &["every"]),
        ]),
        FormSection::new("statement", "Instrucción").field(
            FormField::new("sql", "Instrucción", FormKind::code("sql"))
                .required()
                .help("Un INSERT INTO … SELECT …; en un job continuo, desde una función de tabla de archivos como S3(…)."),
        ),
    ];
    if streaming {
        sections.push(
            FormSection::new("properties", "Propiedades")
                .list(properties_list(props, JOB_PROPS_HELP).when("type", &["streaming"]))
                .when("type", &["streaming"]),
        );
        if existing {
            sections.push(
                FormSection::new("progress", "Progreso")
                    .fields(vec![
                        FormField::new("current_offset", "Leído hasta", FormKind::Textarea)
                            .read_only(true),
                        FormField::new("end_offset", "Fin del origen", FormKind::Textarea)
                            .read_only(true),
                        FormField::new("lag", "Retraso", FormKind::Text).read_only(true),
                        FormField::new("last_success", "Última tarea correcta", FormKind::DateTime)
                            .read_only(true),
                        FormField::new(
                            "load_statistic",
                            "Estadísticas de carga",
                            FormKind::Textarea,
                        )
                        .read_only(true),
                        FormField::new("runtime_msg", "Estado de la ejecución", FormKind::Textarea)
                            .read_only(true),
                    ])
                    .when("type", &["streaming"]),
            );
        }
    }
    sections
}

fn job_values(spec: &JobSpec, j: Option<&Job>) -> BTreeMap<String, Value> {
    let mut v = BTreeMap::new();
    let mut put = |k: &str, x: Value| {
        v.insert(k.to_string(), x);
    };
    put("name", json!(spec.name));
    put("database", json!(spec.database.clone().unwrap_or_default()));
    put("enabled", json!(spec.enabled));
    put("comment", json!(spec.comment));
    match &spec.mode {
        Mode::At(t) => {
            put("type", json!("at"));
            put("at", form_time(Some(t)));
        }
        Mode::Every {
            n,
            unit,
            starts,
            ends,
        } => {
            put("type", json!("every"));
            put("every", json!(n));
            put("unit", json!(unit));
            put("starts", form_time(starts.as_deref()));
            put("ends", form_time(ends.as_deref()));
        }
        Mode::Streaming => put("type", json!("streaming")),
    }
    put("sql", json!(spec.sql));
    if let Some(j) = j {
        put("definer", json!(j.definer.clone().unwrap_or_default()));
        put("status", json!(j.status));
        put("created", form_time(j.created.as_deref()));
        for (k, n) in [
            ("succeeded", j.succeeded),
            ("failed", j.failed),
            ("canceled", j.canceled),
        ] {
            put(k, json!(n.map(|n| n.to_string()).unwrap_or_default()));
        }
        put("error", json!(j.error.clone().unwrap_or_default()));
        if spec.mode == Mode::Streaming {
            for (k, x) in [
                ("current_offset", &j.current_offset),
                ("end_offset", &j.end_offset),
                ("lag", &j.lag),
                ("load_statistic", &j.load_statistic),
                ("runtime_msg", &j.runtime_msg),
            ] {
                put(k, json!(x.clone().unwrap_or_default()));
            }
            put("last_success", form_time(j.last_success.as_deref()));
        }
    }
    v
}

async fn read_job(s: &mut MySqlSession, id: &str) -> Result<Job> {
    let (t, job_name) = dj::parse_id(id)?;
    if t != Type::Insert {
        return Err(crate::jobs::bad_id(id, "job de Doris"));
    }
    jobs_of(s, Type::Insert, Some(&job_name))
        .await?
        .into_iter()
        .find(|j| j.name == job_name)
        .ok_or_else(|| dj::not_found(&job_name))
}

/// The job may be paused or resumed (it hasn't finished).
fn switchable(j: &Job) -> bool {
    live(&j.status)
}

/// A job's status while it may be paused or resumed (and its view
/// refreshed): it hasn't finished, stopped or failed.
fn live(status: &str) -> bool {
    matches!(status, "RUNNING" | "PAUSED")
}

/// An `mv:` id, also under "job".
fn is_mv_id(id: &str) -> bool {
    matches!(dj::parse_id(id), Ok((Type::Mv, _)))
}

pub(super) async fn form(s: &mut MySqlSession, kind: &str, id: Option<&str>) -> Result<ObjectForm> {
    let kind = kind_of(kind)?;
    match (kind, id) {
        (Kind::MvRefresh, None) => Err(Error::Unsupported(VIEW_CREATED_WITH.into())),
        (_, Some(id)) if kind == Kind::MvRefresh || is_mv_id(id) => {
            let st = read_mv(s, id).await?;
            Ok(mv_form(&st))
        }
        (_, id) => {
            let admin = dj::is_admin(s).await;
            let dbs = databases(s).await;
            let streaming = has_streaming(s).await;
            let mut f = ObjectForm::default();
            let existing = match id {
                Some(id) => Some(read_job(s, id).await?),
                None => None,
            };
            if !streaming {
                f.notes.push(OLD_JOBS_NOTE.into());
            }
            match &existing {
                Some(j) => {
                    let spec = spec_of(j);
                    f.sections = job_sections(true, streaming, &spec.properties);
                    f.values = job_values(&spec, Some(j));
                    let fixed = |f: &mut ObjectForm, keys: &[&str]| {
                        for sec in f.sections.iter_mut() {
                            for fld in sec
                                .fields
                                .iter_mut()
                                .filter(|x| keys.contains(&x.key.as_str()))
                            {
                                fld.read_only = true;
                            }
                        }
                    };
                    if !switchable(j) {
                        fixed(&mut f, &["enabled"]);
                    }
                    if spec.mode == Mode::Streaming {
                        f.notes.push(STREAMING_NOTE.into());
                        if is_cdc(&spec) {
                            // Only its properties and state change in place.
                            f.notes.push(CDC_NOTE.into());
                            fixed(&mut f, &["sql", "name", "comment", "type"]);
                        }
                    } else if once_past(&spec, &j.status, server_now(s).await.as_deref()) {
                        // Only paused or resumed, while it can be.
                        f.notes.push(PAST_ONCE.into());
                        for sec in f.sections.iter_mut() {
                            for fld in sec.fields.iter_mut().filter(|x| x.key != "enabled") {
                                fld.read_only = true;
                            }
                            for l in sec.lists.iter_mut() {
                                l.read_only = true;
                            }
                        }
                        if !switchable(j) {
                            f.read_only = true;
                        }
                    } else {
                        f.notes.push(RECREATED.into());
                        f.notes.push(UNKNOWN_DB.into());
                    }
                }
                None => {
                    let current = s.current_database().await;
                    let spec = JobSpec {
                        name: String::new(),
                        database: current.filter(|d| dbs.iter().any(|(v, _)| v == d)),
                        enabled: true,
                        comment: String::new(),
                        mode: Mode::Every {
                            n: 1,
                            unit: "HOUR".into(),
                            starts: None,
                            ends: None,
                        },
                        properties: Vec::new(),
                        sql: String::new(),
                    };
                    f.sections = job_sections(false, streaming, &[]);
                    f.values = job_values(&spec, None);
                }
            }
            f.choices.insert("database".into(), dbs);
            if !admin {
                f.read_only = true;
                f.notes.insert(0, ADMIN_ONLY.into());
            }
            Ok(f)
        }
    }
}

pub(super) async fn plan(
    s: &mut MySqlSession,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    let kind = kind_of(kind)?;
    match (kind, id) {
        (Kind::MvRefresh, None) => Err(Error::Unsupported(VIEW_CREATED_WITH.into())),
        (_, Some(id)) if kind == Kind::MvRefresh || is_mv_id(id) => {
            let st = read_mv(s, id).await?;
            let want = mv_spec_from(values)?;
            let label = format!("{}.{}", st.db, st.mv);
            let mut plan = mv_alter_plan(&view_sql(&st), &label, &st.spec, &want)?
                .unwrap_or_else(|| Plan::nothing(&label, id.to_string()));
            plan.id = Some(id.to_string());
            Ok(plan)
        }
        (_, None) => {
            let dbs = databases(s).await;
            let streaming = has_streaming(s).await;
            let spec = job_spec_from(values, &dbs, None, streaming)?;
            if is_cdc(&spec) {
                return Err(bad(
                    "La instrucción de un job de Doris tiene que ser un INSERT (INSERT INTO … SELECT …).".into(),
                ));
            }
            if matches!(spec.mode, Mode::At(_)) {
                not_past(&spec, server_now(s).await.as_deref())?;
            }
            Ok(create_plan(&spec))
        }
        (_, Some(id)) => {
            let j = read_job(s, id).await?;
            let dbs = databases(s).await;
            let streaming = has_streaming(s).await;
            let mut cur = spec_of(&j);
            let want = job_spec_from(values, &dbs, Some(&cur), streaming)?;
            cur.database = want.database.clone();
            if recreates(&cur, &want) {
                // Dropped and created again: only where both the new job
                // and, if that fails, the old one can be created.
                if let Some(m) = cdc_recreate(&cur, &want) {
                    return Err(bad(m.into()));
                }
                let now = server_now(s).await;
                if once_past(&cur, &j.status, now.as_deref()) {
                    return Err(bad(PAST_ONCE.into()));
                }
                not_past(&want, now.as_deref())?;
                if needs_database(&want) || needs_database(&cur) {
                    return Err(bad(NEEDS_DATABASE.into()));
                }
            }
            let plan = alter_plan(&cur, &want, switchable(&j));
            Ok(plan.unwrap_or_else(|| Plan::nothing(&j.name, id.to_string())))
        }
    }
}

pub(super) async fn delete_plan(s: &mut MySqlSession, kind: &str, id: &str) -> Result<Plan> {
    let kind = kind_of(kind)?;
    if kind == Kind::MvRefresh || is_mv_id(id) {
        dj::parse_id(id)?;
        return Err(Error::Unsupported(VIEW_CREATED_WITH.into()));
    }
    let j = read_job(s, id).await?;
    Ok(Plan {
        head: vec![format!("Borra el job «{}» de Doris", one_line(&j.name))],
        steps: vec![drop_job(&j.name)],
        id: Some(id.to_string()),
        ..Plan::default()
    })
}

const NO_RUN: &str = "Doris no ejecuta un job a pedido: solo según su programación";

/// The view `db`.`mv`'s refresh now for `action`, while its refresh job is
/// `status`: only one running or paused (not finished, stopped or
/// failed) refreshes it.
pub(super) fn refresh_plan(
    db: &str,
    mv: &str,
    status: &str,
    action: &str,
    id: &str,
) -> Result<Plan> {
    let complete = match action.trim() {
        REFRESH => false,
        REFRESH_COMPLETE => true,
        other => return Err(no_action(other)),
    };
    if !live(status) {
        return Err(bad(format!(
            "El job que refresca la vista «{}.{}» está {}: Doris no la refresca.",
            shown(db),
            shown(mv),
            shown(status)
        )));
    }
    Ok(Plan {
        head: refresh_head(&format!("{db}.{mv}"), complete),
        steps: vec![format!(
            "REFRESH MATERIALIZED VIEW {}.{} {}",
            quoted(db),
            quoted(mv),
            if complete { "COMPLETE" } else { "AUTO" }
        )],
        id: Some(id.to_string()),
        ..Plan::default()
    })
}

/// A view's refresh now (`REFRESH MATERIALIZED VIEW … AUTO | COMPLETE`:
/// Doris queues it and answers at once). A job has no actions: Doris has no
/// statement that runs one now.
pub(super) async fn action_plan(
    s: &mut MySqlSession,
    kind: &str,
    id: &str,
    action: &str,
) -> Result<Plan> {
    let kind = kind_of(kind)?;
    if kind == Kind::MvRefresh || is_mv_id(id) {
        let st = read_mv(s, id).await?;
        return refresh_plan(&st.db, &st.mv, &st.job.status, action, id);
    }
    // A job: read back first, so a foreign id is refused as such.
    read_job(s, id).await?;
    match action.trim() {
        "run" => Err(Error::Unsupported(NO_RUN.into())),
        other => Err(no_action(other)),
    }
}
