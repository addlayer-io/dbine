//! StarRocks' scheduler in the designer:
//!
//! - "job": tasks (`SUBMIT TASK db.name [SCHEDULE [START(…)] EVERY(INTERVAL
//!   n SECOND|MINUTE|HOUR|DAY)] [PROPERTIES (…)] AS <statement>`), run once
//!   when created or periodically. Their properties change in place
//!   (`ALTER TASK … SET`), and `ALTER TASK … SUSPEND | RESUME` stops and
//!   restarts the schedule (the catalog doesn't say which tasks are
//!   suspended, so the form asks what to do rather than showing a state).
//!   StarRocks can't change a task's schedule, statement, database or name:
//!   a change drops it and submits it again (its run history goes, and a
//!   task without a schedule runs again); the script says so at the top and
//!   a failed submit puts a scheduled task back (one without a schedule
//!   already ran: submitted again it would run again, so it stays
//!   dropped). The statement has to be one: a `;` would run the rest at
//!   once. `DROP TASK` deletes. Actions: a scheduled task's schedule
//!   suspended and resumed (`ALTER TASK … SUSPEND | RESUME`); no "run
//!   now", which StarRocks lacks (checked up to 4.1.4). A pipe's tasks
//!   (`pipe-…-task-…`) are the server's: shown read-only.
//! - "mv_refresh": how an asynchronous materialized view refreshes
//!   (`REFRESH MANUAL`, `REFRESH ASYNC` when its base tables change, or
//!   `REFRESH ASYNC [START(…)] EVERY(INTERVAL …)`), its refresh-related
//!   properties (`SET (…)`) and whether it is active, all in place with
//!   `ALTER MATERIALIZED VIEW`. Views are created and dropped with the
//!   view, not here (their rows can't be deleted); the job list's `mv:`
//!   rows open this form under "job". Actions, while the view is active:
//!   refresh now what changed (`REFRESH MATERIALIZED VIEW … WITH ASYNC
//!   MODE`) or all of it (`… FORCE WITH ASYNC MODE`).
//! - Neither kind has steps, and StarRocks expires a task's runs on its
//!   own: no statement deletes them.

use super::{
    action_keys, bad, databases, datetime, form_time, name, no_action, no_kind, one_line, pick,
    properties_list, properties_sql, property_changes, read_properties, refresh_head, with_refresh,
    Plan, Vals, PAUSE, REFRESH, REFRESH_COMPLETE, RESUME,
};
use crate::jobs::starrocks::{self as sj, Task, View};
use crate::jobs::{quoted, rows_if_present};
use crate::session::{lit, MySqlSession};
use dbine_driver::{
    Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerAction,
    SchedulerInfo, SchedulerItem, SchedulerKind, SchedulerList,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const NO_TASKS: &str = "Esta versión de StarRocks no tiene tareas.";
const VIEWS_NOTE: &str = "Los refrescos de las vistas materializadas se crean y se borran con la vista: acá se cambia cuándo y cómo se refrescan.";
const SUSPENDED_NOTE: &str = "StarRocks no informa si una tarea periódica está suspendida: el formulario pregunta qué hacer con su programación al guardar.";
const RECREATED: &str = "StarRocks no cambia la programación, la instrucción, la base ni el nombre de una tarea: guardar uno de ellos la borra y la vuelve a crear, y se pierden su historial de ejecuciones y su suspensión.";
const RUNS_AGAIN: &str =
    "Una tarea sin programación corre en cuanto se crea: volver a crearla la ejecuta otra vez.";
const VIEW_CREATED_WITH: &str =
    "El refresco de una vista materializada se crea y se borra con la vista.";
const PIPE_TASK: &str = "Esta tarea es de un pipe (CREATE PIPE): StarRocks la crea y la borra con las cargas del pipe, así que DBine no la cambia ni la borra. Para cambiarla, cambiá el pipe.";

/// The units `EVERY(INTERVAL …)` takes, for tasks and views alike.
pub(super) const UNITS: [&str; 4] = ["SECOND", "MINUTE", "HOUR", "DAY"];

/// The view properties the refresh form shows (and `session.*`): those
/// that say how and when the view refreshes and how fresh it must be.
const MV_KEYS: [&str; 12] = [
    "partition_refresh_number",
    "partition_refresh_strategy",
    "auto_refresh_partitions_limit",
    "excluded_trigger_tables",
    "excluded_refresh_tables",
    "partition_ttl_number",
    "partition_ttl",
    "resource_group",
    "refresh_mode",
    "mv_rewrite_staleness_second",
    "query_rewrite_consistency",
    "warehouse",
];

pub(super) fn refresh_key(k: &str) -> bool {
    let k = k.to_ascii_lowercase();
    k.starts_with("session.") || MV_KEYS.contains(&k.as_str())
}

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

pub(super) async fn info(s: &mut MySqlSession) -> Result<SchedulerInfo> {
    if rows_if_present(s, "SELECT * FROM information_schema.tasks LIMIT 0")
        .await?
        .is_none()
    {
        return Ok(SchedulerInfo {
            kinds: Vec::new(),
            logs: false,
            notes: vec![NO_TASKS.into()],
        });
    }
    let mut notes = Vec::new();
    if !sj::sees_all_databases(s).await {
        notes.push(sj::PARTIAL_NOTE.to_string());
    }
    notes.push(SUSPENDED_NOTE.to_string());
    notes.push(VIEWS_NOTE.to_string());
    // No "run now": StarRocks has no statement that runs a task again. Its
    // schedule is suspended and resumed apart from any switch (the catalog
    // doesn't say which), and the refresh actions are the views' rows',
    // listed among the tasks too. A task runs one statement (no steps), and
    // StarRocks expires its runs on its own: no statement deletes them.
    Ok(SchedulerInfo {
        kinds: vec![
            with_refresh(
                SchedulerKind::new("job", "Tareas", "tarea")
                    .abilities(true, true, true)
                    .action(SchedulerAction::new("pause", "Suspender"))
                    .action(SchedulerAction::new("resume", "Reanudar")),
            )
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

pub(super) async fn items(s: &mut MySqlSession, kind: &str) -> Result<SchedulerList> {
    let kind = kind_of(kind)?;
    let c = match sj::catalog(s, None).await? {
        Ok(c) => c,
        Err(note) => {
            return Ok(SchedulerList {
                items: Vec::new(),
                notes: vec![note.into()],
            })
        }
    };
    let mut items: Vec<SchedulerItem> = c
        .tasks
        .iter()
        .filter_map(|t| {
            let view = c.views.get(&t.name);
            if kind == Kind::MvRefresh && view.is_none() {
                return None;
            }
            Some(item(t, view))
        })
        .collect();
    items.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(SchedulerList {
        items,
        notes: Vec::new(),
    })
}

/// A row of the list, with what can be done with it: a view's refresh task
/// is edited and refreshed (while the view is active), not deleted (it
/// goes with the view); a scheduled task is suspended and resumed; a pipe's
/// task is the server's, read only.
pub(super) fn item(t: &Task, view: Option<&View>) -> SchedulerItem {
    match view {
        Some(v) => SchedulerItem {
            id: sj::task_id(&t.name, true),
            name: v.name.clone(),
            description: Some(match &t.schedule {
                Some(sch) => format!("{} · {sch}", v.schema),
                None => v.schema.clone(),
            }),
            enabled: Some(v.active),
            system: false,
            can_edit: None,
            can_delete: Some(false),
            actions: action_keys(if v.active {
                &[REFRESH, REFRESH_COMPLETE]
            } else {
                &[]
            }),
        },
        None => {
            let pipe = sj::pipe_task(&t.name);
            let scheduled = t.start_every().1.is_some();
            SchedulerItem {
                id: sj::task_id(&t.name, false),
                name: t.name.clone(),
                description: match (&t.database, &t.schedule) {
                    (Some(db), Some(sch)) => Some(format!("{db} · {sch}")),
                    (db, sch) => db.clone().or_else(|| sch.clone()),
                },
                enabled: None,
                system: pipe,
                can_edit: pipe.then_some(false),
                can_delete: pipe.then_some(false),
                actions: action_keys(if scheduled && !pipe {
                    &[PAUSE, RESUME]
                } else {
                    &[]
                }),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Every {
    pub(super) start: Option<String>,
    pub(super) n: u64,
    pub(super) unit: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct TaskSpec {
    pub(super) name: String,
    pub(super) database: String,
    /// `None`: run once, when submitted.
    pub(super) every: Option<Every>,
    /// Without `warehouse`, which isn't one of them.
    pub(super) properties: Vec<(String, String)>,
    /// Without a trailing `;`.
    pub(super) definition: String,
}

/// What to do with the schedule: StarRocks doesn't say which tasks are
/// suspended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StateChange {
    Keep,
    Suspend,
    Resume,
}

fn body_text(s: &str) -> String {
    s.trim().trim_end_matches(';').trim_end().to_string()
}

fn every_sql(e: &Every) -> String {
    let mut s = String::from("SCHEDULE ");
    if let Some(st) = &e.start {
        s.push_str(&format!("START({}) ", lit(st)));
    }
    s.push_str(&format!("EVERY(INTERVAL {} {})", e.n, e.unit));
    s
}

/// `USE`, then `SUBMIT TASK`.
pub(super) fn submit_steps(t: &TaskSpec) -> Vec<String> {
    let mut st = format!("SUBMIT TASK {}.{}", quoted(&t.database), quoted(&t.name));
    if let Some(e) = &t.every {
        st.push('\n');
        st.push_str(&every_sql(e));
    }
    if !t.properties.is_empty() {
        st.push_str(&format!("\nPROPERTIES {}", properties_sql(&t.properties)));
    }
    // No `;` after it: StarRocks keeps everything after AS as the task's
    // definition.
    st.push_str(&format!("\nAS {}", t.definition));
    vec![format!("USE {}", quoted(&t.database)), st]
}

fn state_sql(name: &str, change: StateChange) -> Option<String> {
    match change {
        StateChange::Keep => None,
        StateChange::Suspend => Some(format!("ALTER TASK {} SUSPEND", quoted(name))),
        StateChange::Resume => Some(format!("ALTER TASK {} RESUME", quoted(name))),
    }
}

pub(super) fn create_plan(t: &TaskSpec, change: StateChange) -> Plan {
    let mut steps = submit_steps(t);
    if t.every.is_some() {
        steps.extend(state_sql(&t.name, change));
    }
    let mut head = vec![format!("Crea la tarea «{}» de StarRocks", t.name)];
    if t.every.is_none() {
        head.push("Sin programación, la tarea corre en cuanto se crea.".into());
    }
    Plan {
        head,
        steps,
        id: Some(sj::task_id(&t.name, false)),
        ..Plan::default()
    }
}

/// From `cur` to `want`: properties and the schedule's state in place,
/// anything else by submitting the task again.
pub(super) fn alter_plan(cur: &TaskSpec, want: &TaskSpec, change: StateChange) -> Option<Plan> {
    let id = Some(sj::task_id(&want.name, false));
    let (set, removed) = property_changes(&cur.properties, &want.properties);
    let same_rest = cur.name == want.name
        && cur.database == want.database
        && cur.every == want.every
        && cur.definition == want.definition;
    let change = if want.every.is_some() {
        change
    } else {
        StateChange::Keep
    };
    if same_rest && removed.is_empty() {
        let mut steps = Vec::new();
        if !set.is_empty() {
            steps.push(format!(
                "ALTER TASK {} SET {}",
                quoted(&cur.name),
                properties_sql(&set)
            ));
        }
        steps.extend(state_sql(&cur.name, change));
        if steps.is_empty() {
            return None;
        }
        return Some(Plan {
            head: vec![format!("Cambia la tarea «{}» de StarRocks", cur.name)],
            steps,
            id,
            ..Plan::default()
        });
    }
    let mut steps = vec![format!("DROP TASK {}", quoted(&cur.name))];
    steps.extend(submit_steps(want));
    if want.every.is_some() {
        steps.extend(state_sql(&want.name, change));
    }
    let mut head = vec![
        format!("Cambia la tarea «{}» de StarRocks", cur.name),
        RECREATED.into(),
    ];
    if want.every.is_none() {
        head.push(RUNS_AGAIN.into());
    }
    // A task without a schedule already ran: submitted again to undo a
    // failed change, it would run its statement once more.
    let restorable = cur.every.is_some();
    head.push(if restorable {
        "Si la creación falla, DBine vuelve a crear la tarea como estaba.".into()
    } else {
        "Si la creación falla, la tarea anterior queda borrada: ya corrió, y DBine no la vuelve a crear porque la ejecutaría otra vez.".into()
    });
    Some(Plan {
        head,
        steps,
        undo_from: restorable.then_some(0),
        undo: if restorable {
            submit_steps(cur)
        } else {
            Vec::new()
        },
        id,
        ..Plan::default()
    })
}

/// A task as the server has it.
pub(super) fn spec_of(t: &Task) -> TaskSpec {
    let every = match t.start_every() {
        (start, Some((n, unit))) => Some(Every { start, n, unit }),
        _ => None,
    };
    TaskSpec {
        name: t.name.clone(),
        database: t.database.clone().unwrap_or_default(),
        every,
        properties: t
            .properties
            .as_deref()
            .map(sj::parse_properties)
            .unwrap_or_default()
            .into_iter()
            .filter(|(k, _)| !k.eq_ignore_ascii_case("warehouse"))
            .collect(),
        definition: body_text(t.definition.as_deref().unwrap_or("")),
    }
}

pub(super) fn task_spec_from(
    values: &FormValues,
    dbs: &[(String, String)],
) -> Result<(TaskSpec, StateChange)> {
    let v = Vals(&values.values);
    let task = name(v, "name", "Nombre", 64)?;
    let mut database = name(v, "database", "Base de datos", 256)?;
    if !dbs.is_empty() {
        database = pick(&database, dbs, |s| {
            format!("No existe la base «{s}» o este usuario no la ve.")
        })?
        .to_string();
    }
    let every = match v.choice("type", "Tipo", &["once", "every"], "every")? {
        "once" => None,
        _ => Some(Every {
            start: datetime(v, "starts", "Desde")?,
            n: v.int("every", "Cada", 1, i64::from(i32::MAX))?
                .ok_or_else(|| bad("Una tarea periódica necesita «Cada».".into()))?
                as u64,
            unit: v.choice("unit", "Unidad", &UNITS, "HOUR")?.to_string(),
        }),
    };
    let properties = read_properties(values)?;
    if properties
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("warehouse"))
    {
        return Err(bad(
            "StarRocks no toma el warehouse como propiedad de una tarea: la tarea usa el de la sesión que la crea.".into(),
        ));
    }
    let definition = body_text(&v.raw("definition"));
    if definition.is_empty() {
        return Err(bad("La tarea necesita una instrucción.".into()));
    }
    if !super::one_statement(&definition, false) {
        return Err(bad(
            "La instrucción de la tarea tiene que ser una sola, sin «;» en el medio: el servidor guardaría en la tarea solo lo anterior y ejecutaría el resto al guardar.".into(),
        ));
    }
    let change = match v.choice(
        "state",
        "Suspender o reanudar",
        &["keep", "suspend", "resume"],
        "keep",
    )? {
        "suspend" => StateChange::Suspend,
        "resume" => StateChange::Resume,
        _ => StateChange::Keep,
    };
    Ok((
        TaskSpec {
            name: task,
            database,
            every,
            properties,
            definition,
        },
        change,
    ))
}

fn unit_select() -> FormKind {
    FormKind::select(&[
        ("SECOND", "Segundos"),
        ("MINUTE", "Minutos"),
        ("HOUR", "Horas"),
        ("DAY", "Días"),
    ])
}

const TASK_PROPS_HELP: &str =
    "Variables de sesión con las que corre la tarea, como query_timeout o session.query_timeout.";

pub(super) fn task_sections(existing: bool, props: &[(String, String)]) -> Vec<FormSection> {
    let mut general = FormSection::new("general", "General").fields(vec![
        FormField::new("name", "Nombre", FormKind::Text).required(),
        FormField::new(
            "database",
            "Base de datos",
            FormKind::reference("database", false),
        )
        .required()
        .help("La base de la tarea: ahí se buscan las tablas sin la base de su instrucción."),
    ]);
    if existing {
        general = general.fields(vec![
            FormField::new("creator", "Creador", FormKind::Text).read_only(true),
            FormField::new("created", "Creada", FormKind::DateTime).read_only(true),
            FormField::new("catalog", "Catálogo", FormKind::Text).read_only(true),
            FormField::new("expire", "Vence", FormKind::DateTime).read_only(true),
            FormField::new("warehouse", "Warehouse", FormKind::Text).read_only(true),
        ]);
    }
    vec![
        general,
        FormSection::new("schedule", "Programación").fields(vec![
            FormField::new(
                "type",
                "Tipo",
                FormKind::select(&[
                    ("once", "Una sola vez, al crearla"),
                    ("every", "Periódica (SCHEDULE … EVERY)"),
                ]),
            )
            .default(json!("every")),
            FormField::new("starts", "Desde", FormKind::DateTime)
                .help("Vacío: desde que se crea.")
                .when("type", &["every"]),
            FormField::new("every", "Cada", FormKind::number(Some(1.0), None))
                .required()
                .when("type", &["every"])
                .default(json!(1)),
            FormField::new("unit", "Unidad", unit_select())
                .when("type", &["every"])
                .default(json!("HOUR")),
            FormField::new(
                "state",
                "Suspender o reanudar",
                FormKind::select(&[
                    ("keep", "No cambiar"),
                    ("suspend", "Suspender la programación (ALTER TASK … SUSPEND)"),
                    ("resume", "Reanudarla (ALTER TASK … RESUME)"),
                ]),
            )
            .help("StarRocks no informa si una tarea está suspendida: elegí qué hacer con su programación al guardar.")
            .when("type", &["every"])
            .default(json!("keep")),
        ]),
        FormSection::new("statement", "Instrucción").field(
            FormField::new("definition", "Instrucción", FormKind::code("sql"))
                .required()
                .help("Lo que ejecuta la tarea: un INSERT … SELECT … o un CREATE TABLE … AS SELECT …."),
        ),
        FormSection::new("properties", "Propiedades").list(properties_list(props, TASK_PROPS_HELP)),
    ]
}

fn task_values(spec: &TaskSpec, t: Option<&Task>) -> BTreeMap<String, Value> {
    let mut v = BTreeMap::new();
    let mut put = |k: &str, x: Value| {
        v.insert(k.to_string(), x);
    };
    put("name", json!(spec.name));
    put("database", json!(spec.database));
    put("state", json!("keep"));
    match &spec.every {
        None => put("type", json!("once")),
        Some(e) => {
            put("type", json!("every"));
            put("starts", form_time(e.start.as_deref()));
            put("every", json!(e.n));
            put("unit", json!(e.unit));
        }
    }
    put("definition", json!(spec.definition));
    if let Some(t) = t {
        put("creator", json!(t.creator.clone().unwrap_or_default()));
        put("created", form_time(t.created.as_deref()));
        put("catalog", json!(t.catalog.clone().unwrap_or_default()));
        put("expire", form_time(t.expire.as_deref()));
        let warehouse = t
            .properties
            .as_deref()
            .map(sj::parse_properties)
            .unwrap_or_default()
            .into_iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("warehouse"))
            .map(|(_, v)| v)
            .unwrap_or_default();
        put("warehouse", json!(warehouse));
    }
    v
}

// ---------------------------------------------------------------------------
// Materialized views' refresh
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Trigger {
    Manual,
    /// When the base tables change.
    Async,
    Every(Every),
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct MvSpec {
    pub(super) trigger: Trigger,
    pub(super) active: bool,
    pub(super) properties: Vec<(String, String)>,
}

/// The trigger of a view's refresh task, from its `SCHEDULE`.
pub(super) fn trigger_of(t: &Task) -> Trigger {
    match (t.schedule.as_deref().map(str::trim), t.start_every()) {
        (_, (start, Some((n, unit)))) => Trigger::Every(Every { start, n, unit }),
        (Some(s), _) if s.eq_ignore_ascii_case("EVENT_TRIGGERED") => Trigger::Async,
        _ => Trigger::Manual,
    }
}

fn trigger_sql(t: &Trigger) -> String {
    match t {
        Trigger::Manual => "REFRESH MANUAL".into(),
        Trigger::Async => "REFRESH ASYNC".into(),
        Trigger::Every(e) => {
            let mut s = String::from("REFRESH ASYNC ");
            if let Some(st) = &e.start {
                s.push_str(&format!("START({}) ", lit(st)));
            }
            s.push_str(&format!("EVERY(INTERVAL {} {})", e.n, e.unit));
            s
        }
    }
}

/// The `PROPERTIES (…)` of a `CREATE MATERIALIZED VIEW`, the refresh's
/// ones.
pub(super) fn view_properties(create: &str) -> Vec<(String, String)> {
    let Some(i) = create.find("PROPERTIES") else {
        return Vec::new();
    };
    let rest = &create[i + "PROPERTIES".len()..];
    let Some(open) = rest.find('(') else {
        return Vec::new();
    };
    // The closing parenthesis, outside quotes.
    let body = &rest[open..];
    let mut quote: Option<char> = None;
    let mut end = None;
    let mut chars = body.char_indices().peekable();
    while let Some((j, c)) = chars.next() {
        match quote {
            Some(q) if c == '\\' => {
                let _ = q;
                chars.next();
            }
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c == ')' => {
                end = Some(j);
                break;
            }
            None => {}
        }
    }
    let Some(end) = end else {
        return Vec::new();
    };
    sj::parse_properties(&body[..=end])
        .into_iter()
        .filter(|(k, _)| refresh_key(k))
        .collect()
}

pub(super) fn mv_alter_plan(
    view: &str,
    label: &str,
    cur: &MvSpec,
    want: &MvSpec,
) -> Result<Option<Plan>> {
    let (set, removed) = property_changes(&cur.properties, &want.properties);
    if !removed.is_empty() {
        return Err(bad(format!(
            "StarRocks no quita una propiedad de una vista materializada ({}): dejala con el valor que corresponda.",
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
    if cur.trigger != want.trigger {
        steps.push(format!(
            "ALTER MATERIALIZED VIEW {view} {}",
            trigger_sql(&want.trigger)
        ));
    }
    if cur.active != want.active {
        steps.push(format!(
            "ALTER MATERIALIZED VIEW {view} {}",
            if want.active { "ACTIVE" } else { "INACTIVE" }
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
    let trigger = match v.choice(
        "trigger",
        "Disparador",
        &["MANUAL", "ASYNC", "SCHEDULE"],
        "MANUAL",
    )? {
        "ASYNC" => Trigger::Async,
        "SCHEDULE" => Trigger::Every(Every {
            start: datetime(v, "starts", "Desde")?,
            n: v.int("every", "Cada", 1, i64::from(i32::MAX))?
                .ok_or_else(|| bad("Un refresco programado necesita «Cada».".into()))?
                as u64,
            unit: v.choice("unit", "Unidad", &UNITS, "HOUR")?.to_string(),
        }),
        _ => Trigger::Manual,
    };
    Ok(MvSpec {
        trigger,
        active: v.bool("active", true),
        properties: read_properties(values)?
            .into_iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v))
            .collect(),
    })
}

struct MvState {
    task: Task,
    view: View,
    spec: MvSpec,
}

async fn read_mv(s: &mut MySqlSession, id: &str) -> Result<MvState> {
    let (name, mv) = sj::parse_id(id)?;
    if !mv {
        return Err(crate::jobs::bad_id(id, "refresco de vista materializada"));
    }
    let (task, view, _) = sj::one(s, id).await?;
    let view = view.ok_or_else(|| sj::not_found(&name))?;
    let rows = s
        .rows(&format!(
            "SHOW CREATE MATERIALIZED VIEW {}.{}",
            quoted(&view.schema),
            quoted(&view.name)
        ))
        .await?;
    let create = rows
        .first()
        .and_then(|r| crate::session::named(r, &["Create Materialized View"]))
        .unwrap_or_default();
    let spec = MvSpec {
        trigger: trigger_of(&task),
        active: view.active,
        properties: view_properties(&create),
    };
    Ok(MvState { task, view, spec })
}

fn mv_form(st: &MvState) -> ObjectForm {
    let mut f = ObjectForm::default();
    let mut put = |k: &str, v: Value| {
        f.values.insert(k.to_string(), v);
    };
    put(
        "view",
        json!(format!("{}.{}", st.view.schema, st.view.name)),
    );
    put("task", json!(st.task.name));
    put("active", json!(st.spec.active));
    put(
        "inactive_reason",
        json!(st.view.inactive_reason.clone().unwrap_or_default()),
    );
    put(
        "refresh_type",
        json!(st.view.refresh_type.clone().unwrap_or_default()),
    );
    match &st.spec.trigger {
        Trigger::Manual => put("trigger", json!("MANUAL")),
        Trigger::Async => put("trigger", json!("ASYNC")),
        Trigger::Every(e) => {
            put("trigger", json!("SCHEDULE"));
            put("starts", form_time(e.start.as_deref()));
            put("every", json!(e.n));
            put("unit", json!(e.unit));
        }
    }
    f.sections = vec![
        FormSection::new("refresh", "Refresco").fields(vec![
            FormField::new("view", "Vista materializada", FormKind::Text).read_only(true),
            FormField::new(
                "trigger",
                "Disparador",
                FormKind::select(&[
                    ("MANUAL", "Manual (REFRESH MANUAL)"),
                    ("ASYNC", "Al cambiar las tablas base (REFRESH ASYNC)"),
                    ("SCHEDULE", "Programado (REFRESH ASYNC … EVERY)"),
                ]),
            )
            .default(json!("MANUAL")),
            FormField::new("starts", "Desde", FormKind::DateTime)
                .help("Vacío: desde que se guarda.")
                .when("trigger", &["SCHEDULE"]),
            FormField::new("every", "Cada", FormKind::number(Some(1.0), None))
                .required()
                .when("trigger", &["SCHEDULE"])
                .default(json!(1)),
            FormField::new("unit", "Unidad", unit_select())
                .when("trigger", &["SCHEDULE"])
                .default(json!("HOUR")),
            FormField::new("active", "Activa", FormKind::Bool)
                .help("Una vista inactiva no se refresca ni se usa para reescribir consultas.")
                .default(json!(true)),
            FormField::new("task", "Tarea", FormKind::Text).read_only(true),
            FormField::new("refresh_type", "Tipo de refresco", FormKind::Text).read_only(true),
            FormField::new("inactive_reason", "Motivo de inactividad", FormKind::Text)
                .read_only(true),
        ]),
        FormSection::new("properties", "Propiedades").list(properties_list(
            &st.spec.properties,
            "Las del refresco: partition_refresh_number, auto_refresh_partitions_limit, excluded_trigger_tables, resource_group, mv_rewrite_staleness_second, session.…",
        )),
    ];
    f
}

// ---------------------------------------------------------------------------
// The contract's calls
// ---------------------------------------------------------------------------

async fn read_task(s: &mut MySqlSession, id: &str) -> Result<Task> {
    let (name, mv) = sj::parse_id(id)?;
    if mv {
        return Err(crate::jobs::bad_id(id, "tarea de StarRocks"));
    }
    let (task, _, _) = sj::one(s, id).await?;
    if task.name != name {
        return Err(sj::not_found(&name));
    }
    Ok(task)
}

fn is_mv_id(id: &str) -> bool {
    matches!(sj::parse_id(id), Ok((_, true)))
}

pub(super) async fn form(s: &mut MySqlSession, kind: &str, id: Option<&str>) -> Result<ObjectForm> {
    let kind = kind_of(kind)?;
    match (kind, id) {
        (Kind::MvRefresh, None) => Err(Error::Unsupported(VIEW_CREATED_WITH.into())),
        (_, Some(id)) if kind == Kind::MvRefresh || is_mv_id(id) => {
            Ok(mv_form(&read_mv(s, id).await?))
        }
        (_, id) => {
            let dbs = databases(s).await;
            let mut f = ObjectForm::default();
            match id {
                Some(id) => {
                    let t = read_task(s, id).await?;
                    let spec = spec_of(&t);
                    f.sections = task_sections(true, &spec.properties);
                    f.values = task_values(&spec, Some(&t));
                    if sj::pipe_task(&t.name) {
                        f.read_only = true;
                        f.notes.push(PIPE_TASK.into());
                    } else {
                        f.notes.push(RECREATED.into());
                        if spec.every.is_none() {
                            f.notes.push(RUNS_AGAIN.into());
                        } else {
                            f.notes.push(SUSPENDED_NOTE.into());
                        }
                    }
                }
                None => {
                    let database = s
                        .current_database()
                        .await
                        .filter(|d| dbs.iter().any(|(v, _)| v == d))
                        .or_else(|| dbs.first().map(|(v, _)| v.clone()))
                        .unwrap_or_default();
                    let spec = TaskSpec {
                        name: String::new(),
                        database,
                        every: Some(Every {
                            start: None,
                            n: 1,
                            unit: "HOUR".into(),
                        }),
                        properties: Vec::new(),
                        definition: String::new(),
                    };
                    f.sections = task_sections(false, &[]);
                    f.values = task_values(&spec, None);
                }
            }
            f.choices.insert("database".into(), dbs);
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
            let label = format!("{}.{}", st.view.schema, st.view.name);
            let view = format!("{}.{}", quoted(&st.view.schema), quoted(&st.view.name));
            let mut plan = mv_alter_plan(&view, &label, &st.spec, &want)?
                .unwrap_or_else(|| Plan::nothing(&label, id.to_string()));
            plan.id = Some(id.to_string());
            Ok(plan)
        }
        (_, None) => {
            let dbs = databases(s).await;
            let (spec, change) = task_spec_from(values, &dbs)?;
            Ok(create_plan(&spec, change))
        }
        (_, Some(id)) => {
            let t = read_task(s, id).await?;
            if sj::pipe_task(&t.name) {
                return Err(bad(PIPE_TASK.into()));
            }
            let dbs = databases(s).await;
            let cur = spec_of(&t);
            let (want, change) = task_spec_from(values, &dbs)?;
            Ok(alter_plan(&cur, &want, change)
                .unwrap_or_else(|| Plan::nothing(&cur.name, id.to_string())))
        }
    }
}

pub(super) async fn delete_plan(s: &mut MySqlSession, kind: &str, id: &str) -> Result<Plan> {
    let kind = kind_of(kind)?;
    if kind == Kind::MvRefresh || is_mv_id(id) {
        sj::parse_id(id)?;
        return Err(Error::Unsupported(VIEW_CREATED_WITH.into()));
    }
    let t = read_task(s, id).await?;
    if sj::pipe_task(&t.name) {
        return Err(bad(PIPE_TASK.into()));
    }
    Ok(Plan {
        head: vec![format!(
            "Borra la tarea «{}» de StarRocks",
            one_line(&t.name)
        )],
        steps: vec![format!("DROP TASK {}", quoted(&t.name))],
        id: Some(id.to_string()),
        ..Plan::default()
    })
}

const NO_RUN: &str = "StarRocks no ejecuta una tarea a pedido: no tiene una instrucción para eso (ALTER TASK solo la suspende, la reanuda o cambia sus propiedades).";

/// What an action does: a view refreshed now (`REFRESH MATERIALIZED VIEW
/// … [FORCE] WITH ASYNC MODE`: StarRocks queues it and answers at once), a
/// scheduled task suspended or resumed.
pub(super) async fn action_plan(
    s: &mut MySqlSession,
    kind: &str,
    id: &str,
    action: &str,
) -> Result<Plan> {
    let kind = kind_of(kind)?;
    let action = action.trim();
    if kind == Kind::MvRefresh || is_mv_id(id) {
        let st = read_mv(s, id).await?;
        let complete = match action {
            REFRESH => false,
            REFRESH_COMPLETE => true,
            other => return Err(no_action(other)),
        };
        let label = format!("{}.{}", st.view.schema, st.view.name);
        if !st.view.active {
            return Err(bad(format!(
                "La vista materializada «{label}» está inactiva: StarRocks no la refresca. Activala antes, en su refresco."
            )));
        }
        return Ok(Plan {
            head: refresh_head(&label, complete),
            steps: vec![format!(
                "REFRESH MATERIALIZED VIEW {}.{}{} WITH ASYNC MODE",
                quoted(&st.view.schema),
                quoted(&st.view.name),
                if complete { " FORCE" } else { "" }
            )],
            id: Some(id.to_string()),
            ..Plan::default()
        });
    }
    let t = read_task(s, id).await?;
    let suspend = match action {
        PAUSE => true,
        RESUME => false,
        "run" => return Err(Error::Unsupported(NO_RUN.into())),
        other => return Err(no_action(other)),
    };
    if sj::pipe_task(&t.name) {
        return Err(bad(PIPE_TASK.into()));
    }
    if t.start_every().1.is_none() {
        return Err(bad(
            "Una tarea sin programación no se suspende ni se reanuda: corre una sola vez, al crearse."
                .into(),
        ));
    }
    let change = if suspend {
        StateChange::Suspend
    } else {
        StateChange::Resume
    };
    Ok(Plan {
        head: vec![
            if suspend {
                format!(
                    "Suspende la programación de la tarea «{}» de StarRocks",
                    t.name
                )
            } else {
                format!(
                    "Reanuda la programación de la tarea «{}» de StarRocks",
                    t.name
                )
            },
            "StarRocks no informa si ya lo estaba: repetirlo no cambia nada.".into(),
        ],
        steps: state_sql(&t.name, change).into_iter().collect(),
        id: Some(id.to_string()),
        ..Plan::default()
    })
}
