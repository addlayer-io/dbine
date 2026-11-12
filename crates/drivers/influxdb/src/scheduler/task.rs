//! InfluxDB 2.x / Cloud tasks: what the UI's "Tasks" page and `influx task`
//! do, through `/api/v2/tasks`.
//!
//! - The designer edits the task's name, its schedule (`every` or `cron`,
//!   and `offset`), how many runs may run at once (`concurrency`) and how
//!   many times a failed one is retried (`retry`), the Flux without its
//!   `option task` line (DBine writes that line from the fields, after the
//!   script's imports, keeping any other option it had, which the form
//!   shows read-only), the status, the description and the labels.
//! - A new task is a `POST /api/v2/tasks` in the session's organization,
//!   then one `POST …/labels` per label (when one fails the new task is
//!   deleted again). A change is a `PATCH` of only what changed: the task's
//!   options alone (the server rewrites the `option task` line itself), or
//!   the whole Flux when the script, `concurrency` or `retry` changed or an
//!   option is cleared (the API takes neither of those two, and can't clear
//!   an offset or a description on its own); then the labels added and
//!   taken away.
//! - The history: a task's runs are kept in the `_tasks` bucket; clearing
//!   it deletes them there with the delete API (`/api/v2/delete`).

use super::{bad, check_line, cron_ok, duration_ok, render, shown, Req, Vals, NEW_ID};
use crate::jobs::{bad_id, s as text_of, task_id, utc};
use crate::v2::{flux_str, FluxSession};
use dbine_driver::{
    Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerInfo,
    SchedulerItem, SchedulerKind, SchedulerList,
};
use serde_json::{json, Map, Value as J};

/// The most runs at once and retries InfluxDB takes (`concurrency`,
/// `retry`; at least 1 each).
const MAX_CONCURRENCY: i64 = 100;
const MAX_RETRY: i64 = 10;

/// An option of `option task` the form has a field for: the name, the
/// schedule, the offset, and `concurrency` and `retry` when they are
/// whole numbers (an expression stays as written, with the other options).
fn own_option(key: &str, value: &str) -> bool {
    match key {
        "name" | "every" | "cron" | "offset" => true,
        "concurrency" | "retry" => value.trim().parse::<i64>().is_ok(),
        _ => false,
    }
}

/// Units of a Flux duration literal.
const FLUX_UNITS: &[&str] = &["ns", "us", "µs", "ms", "s", "m", "h", "d", "w", "mo", "y"];

const NO_OPTIONS: &str = "DBine no reconoce la línea option task del Flux de esta tarea: el nombre y la programación se cambian en el propio Flux.";
const SCRIPT_TASK: &str =
    "Esta tarea ejecuta un script invocable de InfluxDB Cloud: se cambia desde InfluxDB Cloud.";
const MANAGED: &str = "Esta tarea la administra un check o una regla de notificación de InfluxDB: se cambia desde ellos.";
const NO_LABELS: &str =
    "Este token no puede leer las etiquetas de la organización: solo se pueden quitar las que tiene la tarea.";

pub(crate) fn info(s: &FluxSession) -> SchedulerInfo {
    SchedulerInfo {
        kinds: vec![SchedulerKind::new("job", "Tareas", "tarea").abilities(true, true, true)],
        logs: false,
        notes: vec![format!(
            "Las tareas son las de la organización «{}» que este token puede leer.",
            shown(&s.org)
        )],
    }
}

pub(crate) async fn items(s: &FluxSession) -> Result<SchedulerList> {
    let items = s
        .all_tasks()
        .await?
        .iter()
        .filter_map(|t| {
            let id = text_of(t.get("id")).filter(|i| task_id(i).is_some())?;
            Some(SchedulerItem {
                id,
                name: text_of(t.get("name")).unwrap_or_default(),
                description: text_of(t.get("description")),
                enabled: Some(t.get("status").and_then(J::as_str) == Some("active")),
                system: false,
            })
        })
        .collect();
    Ok(SchedulerList {
        items,
        notes: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// The task's Flux and its options
// ---------------------------------------------------------------------------

/// The `option task = {…}` statement of a task's Flux: where it is and its
/// record's fields as written.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TaskOptions {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) fields: Vec<(String, String)>,
}

/// Flux's code (`true`) and what isn't code: strings and comments, byte by
/// byte.
fn code_mask(src: &str) -> Vec<bool> {
    let b = src.as_bytes();
    let mut mask = vec![true; b.len()];
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'"' {
            let start = i;
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            let end = (i + 1).min(b.len());
            mask[start..end].iter_mut().for_each(|m| *m = false);
            i = end;
        } else if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
            let start = i;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            mask[start..i].iter_mut().for_each(|m| *m = false);
        } else {
            i += 1;
        }
    }
    mask
}

fn ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// The `option task = {…}` statement (outside strings and comments).
pub(crate) fn find_options(flux: &str) -> Option<TaskOptions> {
    let b = flux.as_bytes();
    let code = code_mask(flux);
    let skip_ws = |mut i: usize| {
        while i < b.len() && (b[i] as char).is_ascii_whitespace() {
            i += 1;
        }
        i
    };
    let mut i = 0;
    while let Some(p) = flux[i..].find("option") {
        let at = i + p;
        i = at + 6;
        if !code[at]
            || (at > 0 && ident_byte(b[at - 1]))
            || b.get(at + 6).is_some_and(|c| ident_byte(*c))
        {
            continue;
        }
        let mut j = skip_ws(at + 6);
        if !flux[j..].starts_with("task") || b.get(j + 4).is_some_and(|c| ident_byte(*c)) {
            continue;
        }
        j = skip_ws(j + 4);
        if b.get(j) != Some(&b'=') {
            continue;
        }
        j = skip_ws(j + 1);
        if b.get(j) != Some(&b'{') {
            continue;
        }
        // The record, to its closing brace.
        let open = j;
        let mut depth = 0i32;
        let mut close = None;
        for (k, c) in b.iter().enumerate().skip(open) {
            if !code[k] {
                continue;
            }
            match c {
                b'{' | b'(' | b'[' => depth += 1,
                b'}' | b')' | b']' => {
                    depth -= 1;
                    if depth == 0 {
                        close = Some(k);
                        break;
                    }
                }
                _ => {}
            }
        }
        let close = close?;
        let fields = record_fields(&flux[open + 1..close], &code[open + 1..close])?;
        return Some(TaskOptions {
            start: at,
            end: close + 1,
            fields,
        });
    }
    None
}

/// `k: v, k2: v2` split at its top-level commas.
fn record_fields(src: &str, code: &[bool]) -> Option<Vec<(String, String)>> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut from = 0;
    for (k, c) in src.bytes().enumerate() {
        if !code[k] {
            continue;
        }
        match c {
            b'{' | b'(' | b'[' => depth += 1,
            b'}' | b')' | b']' => depth -= 1,
            b',' if depth == 0 => {
                parts.push(&src[from..k]);
                from = k + 1;
            }
            _ => {}
        }
    }
    parts.push(&src[from..]);
    let mut out = Vec::new();
    for p in parts {
        let p = p.trim();
        if p.is_empty() {
            continue;
        }
        let (k, v) = p.split_once(':')?;
        let k = k.trim();
        if k.is_empty() || !k.bytes().all(ident_byte) || v.trim().is_empty() {
            return None;
        }
        out.push((k.to_string(), v.trim().to_string()));
    }
    Some(out)
}

/// The Flux without its `option task` statement: what the designer edits.
pub(crate) fn body_of(flux: &str, o: &TaskOptions) -> String {
    let before = flux[..o.start].trim_end();
    let after = flux[o.end..].trim_start();
    match (before.is_empty(), after.is_empty()) {
        (true, _) => after.trim_end().to_string(),
        (false, true) => before.to_string(),
        (false, false) => format!("{before}\n\n{}", after.trim_end()),
    }
}

/// Where the script's imports (and the comments among them) end.
fn imports_end(body: &str) -> usize {
    let mut end = 0;
    let mut at = 0;
    for line in body.split_inclusive('\n') {
        let t = line.trim();
        let next = at + line.len();
        if t.starts_with("import ") || t.starts_with("import\t") || t.starts_with("package ") {
            end = next;
        } else if !(t.is_empty() || t.starts_with("//")) {
            break;
        }
        at = next;
    }
    end
}

/// The whole Flux: the body with `option task = {fields}` after its
/// imports.
pub(crate) fn compose(body: &str, fields: &[(String, String)]) -> String {
    let record: Vec<String> = fields.iter().map(|(k, v)| format!("{k}: {v}")).collect();
    let option = format!("option task = {{{}}}", record.join(", "));
    let split = imports_end(body);
    let head = body[..split].trim_end();
    let rest = body[split..].trim();
    match (head.is_empty(), rest.is_empty()) {
        (true, true) => format!("{option}\n"),
        (true, false) => format!("{option}\n\n{rest}\n"),
        (false, true) => format!("{head}\n\n{option}\n"),
        (false, false) => format!("{head}\n\n{option}\n\n{rest}\n"),
    }
}

// ---------------------------------------------------------------------------
// The task as the server has it
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Task {
    pub(crate) id: String,
    pub(crate) org_id: String,
    pub(crate) org: String,
    pub(crate) owner_id: Option<String>,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) active: bool,
    pub(crate) flux: String,
    pub(crate) every: Option<String>,
    pub(crate) cron: Option<String>,
    pub(crate) offset: Option<String>,
    /// `(id, name)`.
    pub(crate) labels: Vec<(String, String)>,
    pub(crate) kind: Option<String>,
    pub(crate) created: Option<String>,
    pub(crate) updated: Option<String>,
    pub(crate) latest_completed: Option<String>,
    pub(crate) last_status: Option<String>,
    pub(crate) last_error: Option<String>,
}

/// `(id, name)` of each label in a `labels` array.
fn labels_of(v: Option<&J>) -> Vec<(String, String)> {
    v.and_then(J::as_array)
        .into_iter()
        .flatten()
        .filter_map(|l| Some((text_of(l.get("id"))?, text_of(l.get("name"))?)))
        .collect()
}

pub(crate) fn task_of(t: &J) -> Task {
    let raw_flux = t
        .get("flux")
        .and_then(J::as_str)
        .unwrap_or_default()
        .to_string();
    Task {
        id: text_of(t.get("id")).unwrap_or_default(),
        org_id: text_of(t.get("orgID")).unwrap_or_default(),
        org: text_of(t.get("org")).unwrap_or_default(),
        owner_id: text_of(t.get("ownerID")),
        name: text_of(t.get("name")).unwrap_or_default(),
        description: text_of(t.get("description")).unwrap_or_default(),
        active: t.get("status").and_then(J::as_str) == Some("active"),
        flux: raw_flux,
        every: text_of(t.get("every")),
        cron: text_of(t.get("cron")),
        offset: text_of(t.get("offset")),
        labels: labels_of(t.get("labels")),
        kind: text_of(t.get("type")),
        created: text_of(t.get("createdAt")).as_deref().and_then(utc),
        updated: text_of(t.get("updatedAt")).as_deref().and_then(utc),
        latest_completed: text_of(t.get("latestCompleted")).as_deref().and_then(utc),
        last_status: text_of(t.get("lastRunStatus")),
        last_error: text_of(t.get("lastRunError")),
    }
}

impl Task {
    /// Its options' statement, when DBine reads it as the server does.
    fn options(&self) -> Option<TaskOptions> {
        let o = find_options(&self.flux)?;
        // The name the server parsed is the record's: else the statement
        // found isn't the one the server read.
        let named = o
            .fields
            .iter()
            .find(|(k, _)| k == "name")
            .is_some_and(|(_, v)| *v == flux_str(&self.name));
        named.then_some(o)
    }

    /// A whole-number option (`concurrency`, `retry`) of its readable
    /// options.
    fn int_option(&self, key: &str) -> Option<i64> {
        self.options()?
            .fields
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.trim().parse().ok())
    }

    /// The options of its `option task` line the form has no field for,
    /// as written.
    fn other_options(&self) -> Vec<(String, String)> {
        self.options()
            .map(|o| {
                o.fields
                    .into_iter()
                    .filter(|(k, v)| !own_option(k, v))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The Flux the designer edits: without its options when DBine reads
    /// them.
    fn body(&self) -> String {
        match self.options() {
            Some(o) => body_of(&self.flux, &o),
            None => self.flux.trim().to_string(),
        }
    }

    /// Made by a check or a notification rule, or by a Cloud script.
    fn managed(&self) -> Option<&'static str> {
        if self.flux.trim().is_empty() {
            return Some(SCRIPT_TASK);
        }
        match self.kind.as_deref() {
            None | Some("" | "basic" | "system") => None,
            Some(_) => Some(MANAGED),
        }
    }
}

async fn read(s: &FluxSession, id: &str) -> Result<Task> {
    let id = task_id(id).ok_or_else(|| bad_id(id))?;
    let t = task_of(&s.task(id).await?);
    if t.id != id {
        return Err(Error::Query("la tarea no existe".into()));
    }
    Ok(t)
}

/// The organization's labels, `(id, name)`; `None` when the token can't
/// list them.
async fn org_labels(s: &FluxSession, org_id: &str) -> Option<Vec<(String, String)>> {
    let v = s.get("/api/v2/labels", &[("orgID", org_id)]).await.ok()?;
    Some(labels_of(v.get("labels")))
}

/// The session's organization's id.
async fn org_id(s: &FluxSession) -> Result<String> {
    let v = s.get("/api/v2/orgs", &[("org", s.org.as_str())]).await?;
    v.get("orgs")
        .and_then(J::as_array)
        .and_then(|o| o.first())
        .and_then(|o| text_of(o.get("id")))
        .ok_or_else(|| Error::Query(format!("no existe la organización «{}»", shown(&s.org))))
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

const BODY_PLACEHOLDER: &str = "from(bucket: \"mi_bucket\")\n  |> range(start: -task.every)\n  |> filter(fn: (r) => r._measurement == \"cpu\")\n  |> aggregateWindow(every: 1h, fn: mean)\n  |> to(bucket: \"mi_bucket_1h\")";

/// The form for `task` (`None`: a new one), the organization's labels
/// (`None`: not readable) and the owner's name.
pub(crate) fn form_of(
    task: Option<&Task>,
    labels: Option<&[(String, String)]>,
    owner: Option<String>,
    org: &str,
) -> ObjectForm {
    let mut f = ObjectForm::default();
    let t = task.cloned().unwrap_or_default();
    let existing = task.is_some();
    let options = !existing || t.options().is_some();
    let v = &mut f.values;
    v.insert("name".into(), json!(t.name));
    v.insert("description".into(), json!(t.description));
    v.insert("enabled".into(), json!(!existing || t.active));
    v.insert(
        "mode".into(),
        json!(if t.cron.is_some() { "cron" } else { "every" }),
    );
    v.insert(
        "every".into(),
        json!(if existing {
            t.every.clone().unwrap_or_default()
        } else {
            "1h".into()
        }),
    );
    v.insert("cron".into(), json!(t.cron.clone().unwrap_or_default()));
    v.insert("offset".into(), json!(t.offset.clone().unwrap_or_default()));
    v.insert("concurrency".into(), json!(t.int_option("concurrency")));
    v.insert("retry".into(), json!(t.int_option("retry")));
    let others = t.other_options();
    v.insert(
        "other_options".into(),
        json!(others
            .iter()
            .map(|(k, x)| format!("{k}: {x}"))
            .collect::<Vec<_>>()
            .join("\n")),
    );
    v.insert(
        "flux".into(),
        json!(if existing { t.body() } else { String::new() }),
    );
    v.insert(
        "labels".into(),
        json!(t
            .labels
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>()),
    );
    v.insert(
        "org".into(),
        json!(if existing {
            t.org.clone()
        } else {
            org.to_string()
        }),
    );

    // Labels to pick from: the organization's, and the task's own.
    let mut choices: Vec<(String, String)> = labels.map(<[_]>::to_vec).unwrap_or_default();
    for l in &t.labels {
        if !choices.iter().any(|(id, _)| *id == l.0) {
            choices.push(l.clone());
        }
    }
    choices.sort_by_key(|(_, name)| name.to_lowercase());
    f.choices.insert("label".into(), choices);

    let mut general = FormSection::new("general", "General").fields(vec![
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .read_only(!options),
        FormField::new("description", "Descripción", FormKind::Textarea),
        FormField::new("enabled", "Activa", FormKind::Bool)
            .help("Una tarea inactiva no se ejecuta según su programación ni a mano.")
            .default(json!(true)),
        FormField::new("labels", "Etiquetas", FormKind::reference("label", true)),
        FormField::new("org", "Organización", FormKind::Text).read_only(true),
    ]);
    if existing {
        let v = &mut f.values;
        v.insert("owner".into(), json!(owner.unwrap_or_default()));
        v.insert(
            "created".into(),
            json!(t.created.clone().unwrap_or_default()),
        );
        v.insert(
            "updated".into(),
            json!(t.updated.clone().unwrap_or_default()),
        );
        general = general.fields(vec![
            FormField::new("owner", "Propietario", FormKind::Text).read_only(true),
            FormField::new("created", "Creada", FormKind::DateTime).read_only(true),
            FormField::new("updated", "Modificada", FormKind::DateTime).read_only(true),
        ]);
    }
    let mut schedule = FormSection::new("schedule", "Programación").fields(vec![
        FormField::new(
            "mode",
            "Se ejecuta",
            FormKind::select(&[
                ("every", "Cada un intervalo (every)"),
                ("cron", "Según un cron (cron)"),
            ]),
        )
        .required()
        .read_only(!options)
        .default(json!("every")),
        FormField::new("every", "Cada (every)", FormKind::Text)
            .when("mode", &["every"])
            .required()
            .read_only(!options)
            .placeholder("1h")
            .help("Una duración de Flux: 30s, 15m, 1h, 1d, 1h30m."),
        FormField::new("cron", "Cron", FormKind::Cron)
            .when("mode", &["cron"])
            .required()
            .read_only(!options)
            .placeholder("0 3 * * *")
            .help("Expresión cron en UTC (minuto, hora, día, mes, día de la semana)."),
        FormField::new("offset", "Desfase (offset)", FormKind::Text)
            .read_only(!options)
            .placeholder("5m")
            .help("Retrasa cada ejecución esta duración, sin cambiar el período que procesa. Vacío: sin desfase."),
        FormField::new(
            "concurrency",
            "Ejecuciones a la vez (concurrency)",
            FormKind::number(Some(1.0), Some(MAX_CONCURRENCY as f64)),
        )
        .read_only(!options)
        .help("Cuántas ejecuciones de la tarea pueden correr al mismo tiempo, de 1 a 100; las demás esperan. Vacío: el valor por defecto de InfluxDB."),
        FormField::new(
            "retry",
            "Reintentos (retry)",
            FormKind::number(Some(1.0), Some(MAX_RETRY as f64)),
        )
        .read_only(!options)
        .help("Cuántas veces se reintenta una ejecución que falla, de 1 a 10. Vacío: el valor por defecto de InfluxDB."),
    ]);
    if !others.is_empty() {
        schedule = schedule.field(
            FormField::new("other_options", "Otras opciones de la tarea", FormKind::Textarea)
                .read_only(true)
                .help("Las demás opciones de su línea option task, como están escritas: DBine las conserva sin cambiarlas; se cambian en el Flux de la tarea desde InfluxDB."),
        );
    }
    let flux = FormSection::new("flux", "Flux").field(
        FormField::new("flux", "Flux", FormKind::code("flux"))
            .required()
            .placeholder(BODY_PLACEHOLDER)
            .help(if options {
                "Sin la línea option task: DBine la escribe con el nombre y la programación, después de los import."
            } else {
                "El Flux completo de la tarea, con su línea option task."
            }),
    );
    let mut sections = vec![general, schedule, flux];
    if existing {
        let v = &mut f.values;
        v.insert(
            "latest_completed".into(),
            json!(t.latest_completed.clone().unwrap_or_default()),
        );
        v.insert(
            "last_status".into(),
            json!(t.last_status.clone().unwrap_or_default()),
        );
        v.insert(
            "last_error".into(),
            json!(t.last_error.clone().unwrap_or_default()),
        );
        sections.push(FormSection::new("state", "Estado").fields(vec![
            FormField::new(
                "latest_completed",
                "Último horario completado",
                FormKind::DateTime,
            )
            .read_only(true),
            FormField::new("last_status", "Última ejecución", FormKind::Text).read_only(true),
            FormField::new("last_error", "Último error", FormKind::Textarea).read_only(true),
        ]));
    }
    f.sections = sections;
    if !options {
        f.notes.push(NO_OPTIONS.into());
    }
    if labels.is_none() {
        f.notes.push(NO_LABELS.into());
    }
    if let Some(note) = task.and_then(Task::managed) {
        f.read_only = true;
        f.notes.insert(0, note.into());
    }
    f
}

pub(crate) async fn form(s: &FluxSession, id: Option<&str>) -> Result<ObjectForm> {
    match id {
        None => {
            let labels = match org_id(s).await {
                Ok(o) => org_labels(s, &o).await,
                Err(_) => None,
            };
            Ok(form_of(None, labels.as_deref(), None, &s.org))
        }
        Some(id) => {
            let t = read(s, id).await?;
            let labels = org_labels(s, &t.org_id).await;
            let owner = match &t.owner_id {
                Some(o) => s.user_names().await.get(o).cloned().or(Some(o.clone())),
                None => None,
            };
            Ok(form_of(Some(&t), labels.as_deref(), owner, &s.org))
        }
    }
}

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Schedule {
    Every(String),
    Cron(String),
}

/// What the designer asks for, checked.
#[derive(Debug, Clone, PartialEq)]
struct Wanted {
    name: String,
    description: String,
    active: bool,
    schedule: Schedule,
    offset: Option<String>,
    concurrency: Option<i64>,
    retry: Option<i64>,
    body: String,
    labels: Vec<String>,
}

/// A whole number from `min` to `max`, or empty.
fn int_field(x: Vals, key: &str, label: &str, min: i64, max: i64) -> Result<Option<i64>> {
    let Some(t) = x.opt(key) else {
        return Ok(None);
    };
    match t.parse::<i64>() {
        Ok(n) if (min..=max).contains(&n) => Ok(Some(n)),
        _ => Err(bad(format!(
            "«{label}» tiene que ser un número entero de {min} a {max}."
        ))),
    }
}

fn wanted(v: &FormValues, known_labels: &[(String, String)], options: bool) -> Result<Wanted> {
    let x = Vals(&v.values);
    let name = x
        .opt("name")
        .ok_or_else(|| bad("La tarea necesita un nombre.".into()))?;
    check_line(&name, "Nombre")?;
    let schedule = match x.text("mode").as_str() {
        "" | "every" => {
            let every = x
                .opt("every")
                .ok_or_else(|| bad("Falta cada cuánto se ejecuta la tarea (every).".into()))?;
            if !duration_ok(&every, FLUX_UNITS, false) {
                return Err(bad(format!(
                    "«{}» no es una duración de Flux (por ejemplo 30s, 15m, 1h, 1h30m).",
                    shown(&every)
                )));
            }
            Schedule::Every(every)
        }
        "cron" => {
            let cron = x
                .opt("cron")
                .ok_or_else(|| bad("Falta la expresión cron de la tarea.".into()))?;
            check_line(&cron, "Cron")?;
            if !cron_ok(&cron, 5, 7, true) {
                return Err(bad(format!(
                    "«{}» no es una expresión cron (cinco campos: minuto, hora, día, mes y día de la semana).",
                    shown(&cron)
                )));
            }
            Schedule::Cron(cron)
        }
        other => {
            return Err(bad(format!(
                "«{}» no es una forma de programar una tarea (every o cron).",
                shown(other)
            )))
        }
    };
    let offset = x.opt("offset");
    if let Some(o) = &offset {
        if !duration_ok(o, FLUX_UNITS, false) {
            return Err(bad(format!(
                "«{}» no es una duración de Flux para el desfase (por ejemplo 5m).",
                shown(o)
            )));
        }
    }
    let concurrency = int_field(
        x,
        "concurrency",
        "Ejecuciones a la vez (concurrency)",
        1,
        MAX_CONCURRENCY,
    )?;
    let retry = int_field(x, "retry", "Reintentos (retry)", 1, MAX_RETRY)?;
    let body = x.raw("flux").trim().to_string();
    if body.is_empty() {
        return Err(bad("La tarea necesita su Flux.".into()));
    }
    if options && find_options(&body).is_some() {
        return Err(bad(
            "El Flux no lleva su propia línea option task: el nombre y la programación van en los campos del formulario.".into(),
        ));
    }
    let labels = x.strings("labels");
    for l in &labels {
        if !known_labels.iter().any(|(id, _)| id == l) {
            return Err(bad(format!(
                "«{}» no es una etiqueta de la organización.",
                shown(l)
            )));
        }
    }
    Ok(Wanted {
        name,
        description: x.text("description"),
        active: x.flag("enabled").unwrap_or(true),
        schedule,
        offset,
        concurrency,
        retry,
        body,
        labels,
    })
}

/// The record's fields: the name, the schedule, the offset, `concurrency`
/// and `retry`, then any other option the task had, as written.
fn option_fields(w: &Wanted, extra: &[(String, String)]) -> Vec<(String, String)> {
    let mut f = vec![("name".to_string(), flux_str(&w.name))];
    match &w.schedule {
        Schedule::Every(e) => f.push(("every".into(), e.clone())),
        Schedule::Cron(c) => f.push(("cron".into(), flux_str(c))),
    }
    if let Some(o) = &w.offset {
        f.push(("offset".into(), o.clone()));
    }
    if let Some(n) = w.concurrency {
        f.push(("concurrency".into(), n.to_string()));
    }
    if let Some(n) = w.retry {
        f.push(("retry".into(), n.to_string()));
    }
    f.extend(
        extra
            .iter()
            .filter(|(k, v)| {
                !own_option(k, v)
                    && !(k == "concurrency" && w.concurrency.is_some())
                    && !(k == "retry" && w.retry.is_some())
            })
            .cloned(),
    );
    f
}

fn label_name(known: &[(String, String)], id: &str) -> String {
    known
        .iter()
        .find(|(i, _)| i == id)
        .map(|(_, n)| n.clone())
        .unwrap_or_else(|| id.to_string())
}

/// What a save sends, and the script that shows it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    pub(crate) header: Vec<String>,
    pub(crate) reqs: Vec<Req>,
    /// The task's id when it exists already.
    pub(crate) id: Option<String>,
}

impl Plan {
    pub(crate) fn script(&self) -> String {
        render("//", &self.header, &self.reqs)
    }
}

/// A new task in the organization `org`.
pub(crate) fn create_plan(
    org: &str,
    known_labels: &[(String, String)],
    v: &FormValues,
) -> Result<Plan> {
    let w = wanted(v, known_labels, true)?;
    let mut body = Map::new();
    body.insert("org".into(), json!(org));
    body.insert(
        "status".into(),
        json!(if w.active { "active" } else { "inactive" }),
    );
    if !w.description.is_empty() {
        body.insert("description".into(), json!(w.description));
    }
    body.insert(
        "flux".into(),
        json!(compose(&w.body, &option_fields(&w, &[]))),
    );
    let mut reqs = vec![Req::new("POST", "/api/v2/tasks", Some(J::Object(body)))];
    let mut header = vec![format!(
        "Crea la tarea «{}» en la organización «{}» de InfluxDB.",
        w.name, org
    )];
    for l in &w.labels {
        reqs.push(Req::new(
            "POST",
            format!("/api/v2/tasks/{NEW_ID}/labels"),
            Some(json!({ "labelID": l })),
        ));
    }
    if !w.labels.is_empty() {
        header.push(format!(
            "{{id}} es el id de la tarea nueva; si una etiqueta no se puede poner, la tarea se borra. Etiquetas: {}.",
            w.labels
                .iter()
                .map(|l| label_name(known_labels, l))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(Plan {
        header,
        reqs,
        id: None,
    })
}

/// The change of `t` to what the designer asks for: only what differs.
pub(crate) fn edit_plan(
    t: &Task,
    known_labels: &[(String, String)],
    v: &FormValues,
) -> Result<Plan> {
    if let Some(note) = t.managed() {
        return Err(Error::Query(note.into()));
    }
    let opts = t.options();
    let mut known = known_labels.to_vec();
    known.extend(t.labels.iter().cloned());
    let mut w = wanted(v, &known, opts.is_some())?;
    // Without options DBine can read, they stay as the Flux says.
    if opts.is_none() {
        w.name = t.name.clone();
        w.schedule = match (&t.cron, &t.every) {
            (Some(c), _) => Schedule::Cron(c.clone()),
            (None, e) => Schedule::Every(e.clone().unwrap_or_default()),
        };
        w.offset = t.offset.clone();
        w.concurrency = None;
        w.retry = None;
    }
    let old_schedule = match (&t.cron, &t.every) {
        (Some(c), _) => Schedule::Cron(c.clone()),
        (None, e) => Schedule::Every(e.clone().unwrap_or_default()),
    };
    let name_changed = w.name != t.name;
    let schedule_changed = w.schedule != old_schedule;
    let offset_changed = w.offset != t.offset;
    let body_changed = w.body != t.body();
    let description_changed = w.description != t.description;
    let status_changed = w.active != t.active;
    // The API has no field for these two: they change in the Flux.
    let runs_changed =
        w.concurrency != t.int_option("concurrency") || w.retry != t.int_option("retry");

    let mut patch = Map::new();
    // The API takes an option alone, but can't clear the offset or the
    // description without the Flux.
    let whole = body_changed
        || runs_changed
        || (offset_changed && w.offset.is_none())
        || (description_changed && w.description.is_empty());
    if whole {
        let flux = match &opts {
            Some(o) => compose(&w.body, &option_fields(&w, &o.fields)),
            None => w.body.clone(),
        };
        patch.insert("flux".into(), json!(flux));
    } else {
        if name_changed {
            patch.insert("name".into(), json!(w.name));
        }
        if schedule_changed {
            match &w.schedule {
                Schedule::Every(e) => patch.insert("every".into(), json!(e)),
                Schedule::Cron(c) => patch.insert("cron".into(), json!(c)),
            };
        }
        if offset_changed {
            if let Some(o) = &w.offset {
                patch.insert("offset".into(), json!(o));
            }
        }
    }
    if description_changed {
        patch.insert("description".into(), json!(w.description));
    }
    if status_changed {
        patch.insert(
            "status".into(),
            json!(if w.active { "active" } else { "inactive" }),
        );
    }
    let mut reqs = Vec::new();
    if !patch.is_empty() {
        reqs.push(Req::new(
            "PATCH",
            format!("/api/v2/tasks/{}", t.id),
            Some(J::Object(patch)),
        ));
    }
    let had: Vec<&String> = t.labels.iter().map(|(id, _)| id).collect();
    for l in &w.labels {
        if !had.contains(&l) {
            reqs.push(Req::new(
                "POST",
                format!("/api/v2/tasks/{}/labels", t.id),
                Some(json!({ "labelID": l })),
            ));
        }
    }
    for l in had {
        if !w.labels.contains(l) {
            reqs.push(Req::new(
                "DELETE",
                format!("/api/v2/tasks/{}/labels/{l}", t.id),
                None,
            ));
        }
    }
    let header = if reqs.is_empty() {
        vec![format!("«{}»: no hay cambios que guardar.", t.name)]
    } else {
        vec![format!("Cambia la tarea «{}» ({}).", t.name, t.id)]
    };
    Ok(Plan {
        header,
        reqs,
        id: Some(t.id.clone()),
    })
}

async fn plan(s: &FluxSession, id: Option<&str>, v: &FormValues) -> Result<Plan> {
    match id {
        None => {
            let labels = match org_id(s).await {
                Ok(o) => org_labels(s, &o).await.unwrap_or_default(),
                Err(_) => Vec::new(),
            };
            create_plan(&s.org, &labels, v)
        }
        Some(id) => {
            let t = read(s, id).await?;
            let labels = org_labels(s, &t.org_id).await.unwrap_or_default();
            edit_plan(&t, &labels, v)
        }
    }
}

pub(crate) async fn script(s: &FluxSession, id: Option<&str>, v: &FormValues) -> Result<String> {
    Ok(plan(s, id, v).await?.script())
}

/// One request of a plan; the answer's JSON.
async fn call(s: &FluxSession, r: &Req, new_id: Option<&str>) -> Result<J> {
    let mut req = s.http.request(r.method(), r.url(&s.base, new_id)?);
    if let Some(b) = &r.body {
        req = req.json(b);
    }
    s.send(req).await
}

pub(crate) async fn save(
    s: &FluxSession,
    id: Option<&str>,
    v: &FormValues,
) -> Result<Option<String>> {
    let p = plan(s, id, v).await?;
    let mut new_id = p.id.clone();
    for (i, r) in p.reqs.iter().enumerate() {
        match call(s, r, new_id.as_deref()).await {
            Ok(answer) => {
                if new_id.is_none() {
                    new_id = text_of(answer.get("id")).filter(|i| task_id(i).is_some());
                    if new_id.is_none() {
                        return Err(Error::Query(
                            "InfluxDB no devolvió el id de la tarea creada".into(),
                        ));
                    }
                }
            }
            Err(e) if p.id.is_none() && i > 0 => {
                // The task was created but a label failed: it goes again.
                if let Some(created) = &new_id {
                    let undo = Req::new("DELETE", format!("/api/v2/tasks/{created}"), None);
                    let _ = call(s, &undo, None).await;
                }
                return Err(Error::Query(format!(
                    "No se creó la tarea: no se le pudo poner una etiqueta ({e})"
                )));
            }
            Err(e) if i > 0 => {
                return Err(Error::Query(format!(
                    "Se guardó la tarea, salvo sus etiquetas: {e}"
                )))
            }
            Err(e) => return Err(e),
        }
    }
    Ok(new_id)
}

pub(crate) fn drop_plan(t: &Task) -> Plan {
    Plan {
        header: vec![
            format!("Borra la tarea «{}» ({}).", t.name, t.id),
            "Sus ejecuciones quedan en el bucket _tasks hasta que vence su retención.".into(),
        ],
        reqs: vec![Req::new("DELETE", format!("/api/v2/tasks/{}", t.id), None)],
        id: Some(t.id.clone()),
    }
}

pub(crate) async fn delete_script(s: &FluxSession, id: &str) -> Result<String> {
    Ok(drop_plan(&read(s, id).await?).script())
}

pub(crate) async fn delete(s: &FluxSession, id: &str) -> Result<()> {
    let t = read(s, id).await?;
    for r in &drop_plan(&t).reqs {
        call(s, r, None).await?;
    }
    Ok(())
}

/// The delete API's request that clears the task's runs from `_tasks`.
pub(crate) fn clear_request(t: &Task) -> Req {
    Req::new(
        "POST",
        "/api/v2/delete",
        Some(json!({
            "start": "1970-01-01T00:00:00Z",
            "stop": "2262-04-11T00:00:00Z",
            "predicate": format!("_measurement=\"runs\" AND taskID=\"{}\"", t.id),
        })),
    )
    .query("orgID", &t.org_id)
    .query("bucket", "_tasks")
}

/// Deletes the task's runs from the `_tasks` bucket (the run going on, if
/// any, is listed again until it ends).
pub(crate) async fn clear_history(s: &FluxSession, id: &str) -> Result<()> {
    let t = read(s, id).await?;
    call(s, &clear_request(&t), None).await.map(|_| ())
}

/// Why a task can't start at a step.
pub(crate) fn no_steps() -> Error {
    Error::Unsupported(
        "Una tarea de InfluxDB es un solo script de Flux, sin pasos: se ejecuta entera con «Ejecutar ahora».".into(),
    )
}
