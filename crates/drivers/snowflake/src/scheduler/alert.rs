//! The `alert` kind: Snowflake alerts, scheduled (every N minutes or a
//! cron expression) or on new data, on a warehouse or serverless.
//! `CREATE ALERT … IF (EXISTS (…)) THEN …` (or `FROM TEMPLATE`), `ALTER
//! ALERT … SET | UNSET | MODIFY CONDITION EXISTS | MODIFY ACTION | RESUME |
//! SUSPEND`, `DROP ALERT`. An alert changes while it runs; it can't switch
//! between scheduled and on new data, nor be renamed.

use super::{
    bad, checked_id, held, ident, json_object, load, one_statement, parse_schema, same_json,
    schedule_fields, schedule_of, schedule_values, shown, shown_ident, Choices, Kind, Plan, Run,
    Schedule, Vals,
};
use crate::jobs::{self, get, job_id, literal, text};
use crate::monitor::{quote, Set};
use dbine_driver::{
    FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerItem, SchedulerList,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const NOT_OWNER: &str = "Solo el rol propietario de la alerta (y los roles que lo heredan) puede cambiarla o borrarla: el rol de la sesión no lo tiene.";
const APP_OWNED: &str =
    "La alerta es de una aplicación nativa de Snowflake: no se puede cambiar ni borrar.";
const NO_SWITCH: &str =
    "Snowflake no permite pasar una alerta programada a una sobre datos nuevos, ni al revés.";
const TEMPLATE_NOTE: &str = "Si la alerta salió de una plantilla, se edita por la condición y la acción que la plantilla generó: Snowflake no muestra la plantilla ni sus parámetros.";
const SUSPEND_UNKNOWN: &str = "Snowflake no muestra el valor actual de SUSPEND_ALERT_AFTER_NUM_FAILURES de una alerta: vacío deja el que tiene.";

/// One row of `SHOW ALERTS`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct AlertRow {
    pub(crate) database: String,
    pub(crate) schema: String,
    pub(crate) name: String,
    pub(crate) owner: Option<String>,
    pub(crate) owner_type: Option<String>,
    pub(crate) comment: Option<String>,
    pub(crate) warehouse: Option<String>,
    pub(crate) schedule: Option<String>,
    pub(crate) started: bool,
    pub(crate) condition: Option<String>,
    pub(crate) action: Option<String>,
    pub(crate) runbook: Option<String>,
    pub(crate) config: Option<String>,
    pub(crate) auto_suspended: Option<String>,
    pub(crate) created_on: Option<String>,
}

impl AlertRow {
    fn id(&self) -> String {
        job_id(&self.database, &self.schema, &self.name)
    }

    fn app_owned(&self) -> bool {
        self.owner_type
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case("APPLICATION"))
    }
}

/// The condition as `SHOW ALERTS` gives it, without an `EXISTS (…)` around
/// it if it comes with one.
fn bare_condition(c: &str) -> String {
    let t = c.trim();
    let up = t.to_ascii_uppercase();
    if let Some(rest) = up.strip_prefix("EXISTS") {
        let rest_t = rest.trim_start();
        if rest_t.starts_with('(') && t.ends_with(')') {
            let start = t.len() - rest_t.len() + 1;
            return t[start..t.len() - 1].trim().to_string();
        }
    }
    t.to_string()
}

pub(crate) fn alert_rows(s: &Set) -> Vec<AlertRow> {
    (0..s.rows.len())
        .filter_map(|i| {
            Some(AlertRow {
                database: text(s, i, "database_name")?,
                schema: text(s, i, "schema_name")?,
                name: text(s, i, "name")?,
                owner: text(s, i, "owner"),
                owner_type: text(s, i, "owner_role_type"),
                comment: text(s, i, "comment"),
                warehouse: text(s, i, "warehouse"),
                schedule: text(s, i, "schedule"),
                started: get(s, i, "state").is_some_and(|v| v.eq_ignore_ascii_case("started")),
                condition: text(s, i, "condition").map(|c| bare_condition(&c)),
                action: text(s, i, "action").map(|a| super::trim_code(&a)),
                runbook: text(s, i, "runbook"),
                config: text(s, i, "config"),
                auto_suspended: text(s, i, "was_auto_suspended"),
                created_on: jobs::shown_time(get(s, i, "created_on")),
            })
        })
        .collect()
}

pub(crate) async fn items(r: Run<'_>) -> Result<SchedulerList> {
    let mut notes = Vec::new();
    let s = match r.rows("SHOW ALERTS IN ACCOUNT").await {
        Ok(s) => s,
        Err(e) if jobs::denied(&e) => {
            let Some(db) = r.database() else {
                return Err(e);
            };
            notes.push(format!(
                "se ven solo las alertas de la base {db}: el rol no puede listar las de toda la cuenta"
            ));
            r.rows(&format!("SHOW ALERTS IN DATABASE {}", quote(&db)))
                .await?
        }
        Err(e) => return Err(e),
    };
    let mut items: Vec<SchedulerItem> = alert_rows(&s)
        .iter()
        .map(|a| SchedulerItem {
            id: a.id(),
            name: a.name.clone(),
            description: a.comment.clone(),
            enabled: Some(a.started),
            system: a.app_owned(),
        })
        .collect();
    items.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(SchedulerList { items, notes })
}

async fn read(r: Run<'_>, id: &str) -> Result<AlertRow> {
    let (db, schema, name) = checked_id(Kind::Alert, id)?;
    let s = r
        .rows(&format!(
            "SHOW ALERTS LIKE {} IN SCHEMA {}.{}",
            literal(&name),
            quote(&db),
            quote(&schema)
        ))
        .await?;
    alert_rows(&s)
        .into_iter()
        .find(|a| a.name == name && a.database == db && a.schema == schema)
        .ok_or_else(|| {
            bad(format!(
                "No existe la alerta {db}.{schema}.{name} o el rol no la ve."
            ))
        })
}

// ---------------------------------------------------------------------------
// What an alert is: the same shape for the server's and the form's
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Body {
    /// `IF (EXISTS (condition)) THEN action`.
    Sql { condition: String, action: String },
    /// `FROM TEMPLATE id … TEMPLATE_PARAMS = '…'` (create only).
    Template { id: String, params: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Spec {
    pub(crate) database: String,
    pub(crate) schema: String,
    pub(crate) name: String,
    pub(crate) warehouse: Option<String>,
    /// `None`: on new data.
    pub(crate) schedule: Option<Schedule>,
    pub(crate) comment: Option<String>,
    pub(crate) config: Option<String>,
    pub(crate) runbook: Option<String>,
    /// `None`: not set here (an edit keeps the alert's).
    pub(crate) suspend_after: Option<i64>,
    pub(crate) body: Body,
    pub(crate) enabled: bool,
}

impl Spec {
    fn id(&self) -> String {
        job_id(&self.database, &self.schema, &self.name)
    }
}

pub(crate) fn current(a: &AlertRow) -> Spec {
    Spec {
        database: a.database.clone(),
        schema: a.schema.clone(),
        name: a.name.clone(),
        warehouse: a.warehouse.clone(),
        schedule: a.schedule.as_deref().and_then(Schedule::parse),
        comment: a.comment.clone(),
        config: a.config.clone(),
        runbook: a.runbook.clone(),
        suspend_after: None,
        body: Body::Sql {
            condition: a.condition.clone().unwrap_or_default(),
            action: a.action.clone().unwrap_or_default(),
        },
        enabled: a.started,
    }
}

/// What an alert's condition may be: SELECT, SHOW or CALL.
fn condition_of(code: &str) -> Result<String> {
    let c = one_statement(code, "Condición")?;
    let first = c
        .trim_start_matches(|ch: char| ch == '(' || ch.is_whitespace())
        .split(|ch: char| !ch.is_ascii_alphabetic())
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if !matches!(first.as_str(), "SELECT" | "WITH" | "SHOW" | "CALL") {
        return Err(bad(
            "La condición de una alerta es una consulta: SELECT, SHOW o CALL.".into(),
        ));
    }
    Ok(c)
}

fn template_id(t: &str) -> Result<String> {
    let t = t.trim();
    let ok = !t.is_empty()
        && t.len() <= 255
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '.');
    if !ok {
        return Err(bad(format!(
            "«{}» no es el id de una plantilla de alertas (por ejemplo TASKS_ERROR_RATE).",
            shown(t)
        )));
    }
    Ok(t.to_ascii_uppercase())
}

pub(crate) fn spec(values: &FormValues, choices: &Choices, cur: Option<&AlertRow>) -> Result<Spec> {
    let v = Vals(&values.values);
    let (database, schema, name) = match cur {
        Some(a) => {
            if !v.text("name").is_empty() && ident(&v.text("name"), "Nombre")? != a.name {
                return Err(bad("Snowflake no permite renombrar una alerta.".into()));
            }
            (a.database.clone(), a.schema.clone(), a.name.clone())
        }
        None => {
            let sch = v
                .opt("schema")
                .ok_or_else(|| bad("Elegí el esquema de la alerta.".into()))?;
            let (db, s) = parse_schema(&sch)
                .ok_or_else(|| bad(format!("«{}» no es un esquema de Snowflake.", shown(&sch))))?;
            (db, s, ident(&v.text("name"), "Nombre")?)
        }
    };
    let warehouse = match v.text("compute").as_str() {
        "" | "warehouse" => {
            let w = v
                .opt("warehouse")
                .ok_or_else(|| bad("Falta el warehouse de la alerta.".into()))?;
            Some(choices.pick("warehouse", &w, "Warehouse")?)
        }
        "serverless" => None,
        other => return Err(bad(format!("«{}» no es un tipo de cómputo.", shown(other)))),
    };
    let schedule = schedule_of(v, true)?;
    if let Some(a) = cur {
        if a.schedule.is_some() != schedule.is_some() {
            return Err(bad(NO_SWITCH.into()));
        }
    }
    let template = cur.is_none() && v.text("source") == "template";
    let body = if template {
        Body::Template {
            id: template_id(&v.text("template_id"))?,
            params: json_object(&v.raw("template_params"), "Parámetros de la plantilla")?,
        }
    } else {
        Body::Sql {
            condition: condition_of(&v.raw("condition"))?,
            action: one_statement(&v.raw("action"), "Acción")?,
        }
    };
    Ok(Spec {
        database,
        schema,
        name,
        warehouse,
        schedule,
        comment: v.opt("comment"),
        config: if template {
            None
        } else {
            json_object(&v.raw("config"), "Configuración (CONFIG)")?
        },
        runbook: match v.opt("runbook") {
            Some(t) if t.chars().count() > 2048 => {
                return Err(bad("El runbook admite hasta 2048 caracteres.".into()))
            }
            o => o,
        },
        suspend_after: v.int(
            "suspend_after",
            "Suspender tras fallos seguidos",
            0,
            i64::from(i32::MAX),
        )?,
        body,
        enabled: v.bool("enabled"),
    })
}

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

pub(crate) fn create_plan(s: &Spec) -> Plan {
    let id = s.id();
    let mut p = Plan::new(
        Some(id.clone()),
        &format!(
            "Alerta {}.{}.{} de Snowflake (nueva)",
            s.database, s.schema, s.name
        ),
    );
    let mut lines = vec![format!("CREATE ALERT {id}")];
    if let Body::Template { id: t, .. } = &s.body {
        lines.push(format!("  FROM TEMPLATE {t}"));
    }
    if let Some(sc) = &s.schedule {
        lines.push(format!("  SCHEDULE = {}", literal(&sc.text(true))));
    }
    if let Some(w) = &s.warehouse {
        lines.push(format!("  WAREHOUSE = {}", quote(w)));
    }
    if let Some(c) = &s.comment {
        lines.push(format!("  COMMENT = {}", literal(c)));
    }
    if let Some(c) = &s.config {
        lines.push(format!("  CONFIG = {}", literal(c)));
    }
    if let Some(rb) = &s.runbook {
        lines.push(format!("  RUNBOOK = {}", literal(rb)));
    }
    if let Some(n) = s.suspend_after {
        lines.push(format!("  SUSPEND_ALERT_AFTER_NUM_FAILURES = {n}"));
    }
    match &s.body {
        Body::Sql { condition, action } => {
            lines.push(format!("  IF (EXISTS (\n{condition}\n  ))"));
            lines.push(format!("  THEN\n{action}"));
        }
        Body::Template { params, .. } => {
            if let Some(tp) = params {
                lines.push(format!("  TEMPLATE_PARAMS = {}", literal(tp)));
            }
        }
    }
    p.push(lines.join("\n"));
    if s.enabled {
        p.note("Snowflake crea las alertas suspendidas: el script la reanuda.");
        p.push(format!("ALTER ALERT {id} RESUME"));
    }
    p
}

pub(crate) fn alter_plan(cur: &Spec, want: &Spec) -> Plan {
    let id = want.id();
    let mut p = Plan::new(
        Some(id.clone()),
        &format!(
            "Alerta {}.{}.{} de Snowflake",
            want.database, want.schema, want.name
        ),
    );
    let mut sets: Vec<String> = Vec::new();
    let mut unsets: Vec<String> = Vec::new();
    match (&cur.warehouse, &want.warehouse) {
        (o, Some(n)) if o.as_deref() != Some(n.as_str()) => {
            sets.push(format!("WAREHOUSE = {}", quote(n)))
        }
        (Some(_), None) => unsets.push("WAREHOUSE".into()),
        _ => {}
    }
    if let Some(n) = &want.schedule {
        if cur.schedule.as_ref() != Some(n) {
            sets.push(format!("SCHEDULE = {}", literal(&n.text(true))));
        }
    }
    for (key, o, n) in [
        ("COMMENT", &cur.comment, &want.comment),
        ("RUNBOOK", &cur.runbook, &want.runbook),
    ] {
        match (o, n) {
            (o, Some(n)) if o.as_deref() != Some(n.as_str()) => {
                sets.push(format!("{key} = {}", literal(n)))
            }
            (Some(_), None) => unsets.push(key.into()),
            _ => {}
        }
    }
    match (&cur.config, &want.config) {
        (o, Some(n)) if !same_json(o.as_deref(), Some(n)) => {
            sets.push(format!("CONFIG = {}", literal(n)))
        }
        (Some(_), None) => unsets.push("CONFIG".into()),
        _ => {}
    }
    if let Some(n) = want.suspend_after {
        sets.push(format!("SUSPEND_ALERT_AFTER_NUM_FAILURES = {n}"));
    }
    if !unsets.is_empty() {
        p.push(format!("ALTER ALERT {id} UNSET {}", unsets.join(", ")));
    }
    if !sets.is_empty() {
        p.push(format!("ALTER ALERT {id} SET\n  {}", sets.join("\n  ")));
    }
    if let (
        Body::Sql {
            condition: oc,
            action: oa,
        },
        Body::Sql {
            condition: nc,
            action: na,
        },
    ) = (&cur.body, &want.body)
    {
        if super::trim_code(oc) != *nc {
            p.push(format!(
                "ALTER ALERT {id} MODIFY CONDITION EXISTS (\n{nc}\n)"
            ));
        }
        if super::trim_code(oa) != *na {
            p.push(format!("ALTER ALERT {id} MODIFY ACTION\n{na}"));
        }
    }
    if want.enabled != cur.enabled {
        p.push(format!(
            "ALTER ALERT {id} {}",
            if want.enabled { "RESUME" } else { "SUSPEND" }
        ));
    }
    p
}

async fn may_change(r: Run<'_>, a: &AlertRow) -> Result<()> {
    if a.app_owned() {
        return Err(bad(APP_OWNED.into()));
    }
    if let Some(owner) = &a.owner {
        if !held(r, std::slice::from_ref(owner))
            .await
            .get(owner)
            .copied()
            .unwrap_or(true)
        {
            return Err(bad(NOT_OWNER.into()));
        }
    }
    Ok(())
}

pub(crate) async fn plan(r: Run<'_>, id: Option<&str>, values: &FormValues) -> Result<Plan> {
    let choices = load(r, &["warehouse"]).await;
    match id {
        None => {
            let s = spec(values, &choices, None)?;
            // The schema exists and the name is free.
            let taken = r
                .rows(&format!(
                    "SHOW ALERTS LIKE {} IN SCHEMA {}.{}",
                    literal(&s.name),
                    quote(&s.database),
                    quote(&s.schema)
                ))
                .await?;
            if alert_rows(&taken).iter().any(|a| a.name == s.name) {
                return Err(bad(format!(
                    "Ya existe una alerta «{}» en {}.{}.",
                    s.name, s.database, s.schema
                )));
            }
            Ok(create_plan(&s))
        }
        Some(id) => {
            let a = read(r, id).await?;
            may_change(r, &a).await?;
            let mut choices = choices;
            if let Some(w) = &a.warehouse {
                choices.keep("warehouse", w, w);
            }
            let want = spec(values, &choices, Some(&a))?;
            Ok(alter_plan(&current(&a), &want))
        }
    }
}

pub(crate) fn drop_plan(a: &AlertRow) -> Plan {
    let mut p = Plan::new(
        None,
        &format!(
            "Borra la alerta {}.{}.{} de Snowflake",
            a.database, a.schema, a.name
        ),
    );
    p.push(format!("DROP ALERT {}", a.id()));
    p
}

pub(crate) async fn delete_plan(r: Run<'_>, id: &str) -> Result<Plan> {
    let a = read(r, id).await?;
    may_change(r, &a).await?;
    Ok(drop_plan(&a))
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

fn opt(v: &Option<String>) -> Value {
    json!(v.clone().unwrap_or_default())
}

pub(crate) async fn form(r: Run<'_>, id: Option<&str>) -> Result<ObjectForm> {
    let sources: &[&str] = if id.is_none() {
        &["warehouse", "schema"]
    } else {
        &["warehouse"]
    };
    let (cur, mut choices) = match id {
        Some(id) => {
            let (a, c) = tokio::join!(read(r, id), load(r, sources));
            (Some(a?), c)
        }
        None => (None, load(r, sources).await),
    };
    let mut f = ObjectForm::default();
    let mut values: BTreeMap<String, Value> = BTreeMap::new();
    let existing = cur.is_some();
    match &cur {
        None => {
            let wh = r.warehouse();
            values.insert(
                "compute".into(),
                json!(if wh.is_some() {
                    "warehouse"
                } else {
                    "serverless"
                }),
            );
            values.insert("warehouse".into(), json!(wh.clone().unwrap_or_default()));
            if let Some(w) = &wh {
                choices.keep("warehouse", w, w);
            }
            values.insert("source".into(), json!("sql"));
            values.insert("enabled".into(), json!(false));
            schedule_values(
                Some(&Schedule::Every(60, super::Unit::Minutes)),
                "new_data",
                &mut values,
            );
        }
        Some(a) => {
            let s = current(a);
            values.insert("name".into(), json!(shown_ident(&a.name)));
            let sid = super::schema_id(&a.database, &a.schema);
            choices.keep("schema", &sid, &format!("{}.{}", a.database, a.schema));
            values.insert("schema".into(), json!(sid));
            values.insert("comment".into(), opt(&a.comment));
            values.insert("runbook".into(), opt(&a.runbook));
            values.insert("enabled".into(), json!(a.started));
            values.insert(
                "compute".into(),
                json!(if a.warehouse.is_some() {
                    "warehouse"
                } else {
                    "serverless"
                }),
            );
            values.insert("warehouse".into(), opt(&a.warehouse));
            if let Some(w) = &a.warehouse {
                choices.keep("warehouse", w, w);
            }
            schedule_values(s.schedule.as_ref(), "new_data", &mut values);
            values.insert("source".into(), json!("sql"));
            values.insert("condition".into(), opt(&a.condition));
            values.insert("action".into(), opt(&a.action));
            values.insert("config".into(), opt(&a.config));
            values.insert("suspend_after".into(), json!(""));
            values.insert("owner".into(), opt(&a.owner));
            values.insert("created".into(), opt(&a.created_on));
            values.insert(
                "auto_suspended".into(),
                json!(match a.auto_suspended.as_deref() {
                    Some(v) if v.eq_ignore_ascii_case("true") => "Sí",
                    Some(v) if v.eq_ignore_ascii_case("false") => "No",
                    _ => "",
                }),
            );
            if a.schedule.is_some() && s.schedule.is_none() {
                f.notes.push(format!(
                    "Snowflake devolvió una programación que DBine no interpreta («{}»): elegí una para guardar cambios.",
                    a.schedule.clone().unwrap_or_default()
                ));
            }
        }
    }
    let new_data = existing && cur.as_ref().is_some_and(|a| a.schedule.is_none());
    let mut general = FormSection::new("general", "General").fields(vec![
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .read_only(existing)
            .help("Sin comillas, Snowflake lo guarda en mayúsculas; entre comillas dobles, tal cual. Snowflake no permite renombrar una alerta."),
        FormField::new("schema", "Esquema", FormKind::reference("schema", false))
            .required()
            .read_only(existing),
        FormField::new("comment", "Comentario", FormKind::Textarea),
        FormField::new("runbook", "Runbook", FormKind::Text)
            .placeholder("https://wiki.example.com/runbooks/ventas")
            .help("Una URL o un texto con qué hacer cuando se dispara (hasta 2048 caracteres)."),
        FormField::new("enabled", "Activa (RESUME)", FormKind::Bool)
            .default(json!(false))
            .help("Snowflake crea las alertas suspendidas; activa la reanuda al guardar."),
    ]);
    if existing {
        general = general.fields(vec![
            FormField::new("owner", "Rol propietario", FormKind::Text).read_only(true),
            FormField::new("created", "Creada", FormKind::Text).read_only(true),
            FormField::new("auto_suspended", "Suspendida por fallos", FormKind::Text)
                .read_only(true),
        ]);
    }
    let schedule_options = if new_data {
        FormKind::select(&[(
            "new_data",
            "Cuando hay datos nuevos (alerta sobre datos nuevos)",
        )])
    } else if existing {
        FormKind::select(&[
            ("interval", "Cada cierta cantidad de minutos"),
            ("cron", "Expresión cron"),
        ])
    } else {
        FormKind::select(&[
            ("interval", "Cada cierta cantidad de minutos"),
            ("cron", "Expresión cron"),
            (
                "new_data",
                "Cuando hay datos nuevos (alerta sobre datos nuevos)",
            ),
        ])
    };
    let mut sched = vec![FormField::new("schedule", "Programación", schedule_options)
        .read_only(new_data)
        .default(json!("interval"))
        .help("Una alerta sobre datos nuevos evalúa la condición cuando cambian las tablas que lee; con una plantilla, sin programación usa la de la plantilla. Snowflake no deja cambiar de una a otra.")];
    sched.extend(schedule_fields(false));
    let schedule = FormSection::new("schedule", "Programación").fields(sched);
    let compute = FormSection::new("compute", "Cómputo").fields(vec![
        FormField::new(
            "compute",
            "Cómputo",
            FormKind::select(&[
                ("warehouse", "Warehouse"),
                ("serverless", "Serverless (cómputo administrado por Snowflake)"),
            ]),
        )
        .default(json!("warehouse")),
        FormField::new("warehouse", "Warehouse", FormKind::reference("warehouse", false))
            .when("compute", &["warehouse"]),
        FormField::new(
            "suspend_after",
            "Suspender tras fallos seguidos",
            FormKind::number(Some(0.0), None),
        )
        .help("SUSPEND_ALERT_AFTER_NUM_FAILURES; 0 no suspende. Snowflake no muestra el valor de una alerta: vacío deja el que tiene (o el heredado)."),
    ]);
    let mut body_fields = Vec::new();
    if !existing {
        body_fields.push(
            FormField::new(
                "source",
                "Definición",
                FormKind::select(&[
                    ("sql", "Condición y acción"),
                    ("template", "Plantilla de alertas de Snowflake"),
                ]),
            )
            .default(json!("sql")),
        );
    }
    body_fields.extend(vec![
        FormField::new("condition", "Condición", FormKind::code("sql"))
            .required()
            .when("source", &["sql"])
            .help("Una consulta (SELECT, SHOW o CALL): si devuelve filas, se ejecuta la acción."),
        FormField::new("action", "Acción", FormKind::code("sql"))
            .required()
            .when("source", &["sql"])
            .help("Una instrucción o un bloque BEGIN … END; para avisar, SYSTEM$SEND_EMAIL o SYSTEM$SEND_SNOWFLAKE_NOTIFICATION."),
        FormField::new("config", "Configuración (CONFIG)", FormKind::code("json"))
            .when("source", &["sql"])
            .help("Un objeto JSON que la condición y la acción leen con SYSTEM$GET_ALERT_CONFIG."),
    ]);
    if !existing {
        body_fields.extend(vec![
            FormField::new("template_id", "Plantilla", FormKind::Text)
                .required()
                .when("source", &["template"])
                .placeholder("TASKS_ERROR_RATE")
                .help("SYSTEM$LIST_ALERT_TEMPLATES las lista y SYSTEM$GET_ALERT_TEMPLATE muestra sus variables."),
            FormField::new(
                "template_params",
                "Parámetros de la plantilla (TEMPLATE_PARAMS)",
                FormKind::code("json"),
            )
            .when("source", &["template"])
            .help("Un objeto JSON con template_variables y notification_config."),
        ]);
    }
    let body = FormSection::new("body", "Condición y acción").fields(body_fields);
    f.sections = vec![general, schedule, body, compute];
    f.values = values;
    f.choices = choices.lists;
    if let Some(a) = &cur {
        f.notes.push(TEMPLATE_NOTE.into());
        f.notes.push(SUSPEND_UNKNOWN.into());
        if new_data {
            f.notes.push(NO_SWITCH.into());
        }
        if a.app_owned() {
            f.read_only = true;
            f.notes.insert(0, APP_OWNED.into());
        } else if let Some(owner) = &a.owner {
            let roles = held(r, std::slice::from_ref(owner)).await;
            if !roles.get(owner).copied().unwrap_or(true) {
                f.read_only = true;
                f.notes.insert(0, NOT_OWNER.into());
            }
        }
    }
    Ok(f)
}
