//! Alerts: SSMS's "New Alert" dialog (General: SQL Server event,
//! performance condition or WMI event; Response: the job it runs and the
//! operators it notifies; Options; History), `sp_add_alert` /
//! `sp_update_alert` / `sp_delete_alert` and the notifications, and the
//! alert's actions in SSMS's menu and History page: reset the count,
//! enable, disable. sysadmin only; Azure SQL Managed Instance has no
//! alerts.
//!
//! A performance condition picks its object, then a counter of that
//! object, then an instance of that counter, from the server's counters
//! (`sys.dm_os_performance_counters`, what the Agent watches): each list
//! is a choice source that names the field it depends on.

use super::choices::{self, Catalog, ALL_DATABASES};
use super::{
    bad, fits, guid_literal, int_id, no_action, pick, rows, shown, Ctx, Kind, Plan, Script, Vals,
    A, SYSADMIN_ONLY_FORM, SYSNAME,
};
use crate::jobs::{self, agent_time_of, first_with, guid, int, text};
use crate::SqlServerSession;
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result,
    SchedulerItem, SchedulerList,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const NOT_FOUND: &str = "La alerta no existe o este usuario no puede verla.";
const NEVER: &str = "Nunca";
pub(crate) const UNCATEGORIZED: &str = "[Uncategorized]";

/// What raises the alert.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Raise {
    /// An error number.
    Error(i64),
    Severity(i64),
    /// `object|counter|instance|comparator|value`.
    Performance(String),
    Wmi {
        namespace: String,
        query: String,
    },
}

/// An alert as msdb keeps it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Alert {
    pub(crate) name: String,
    pub(crate) enabled: bool,
    pub(crate) raise: Raise,
    /// Empty: every database.
    pub(crate) database: String,
    pub(crate) keyword: String,
    /// The job it runs (its id, lowercase).
    pub(crate) job: Option<String>,
    /// `include_event_description_in`: 1 e-mail, 2 pager, 4 net send.
    pub(crate) include: i64,
    pub(crate) message: String,
    /// Seconds.
    pub(crate) delay: i64,
    pub(crate) category: String,
    /// Operator → `notification_method`.
    pub(crate) operators: BTreeMap<String, i64>,
}

impl Alert {
    /// msdb's columns for what raises it: message id, severity,
    /// performance condition, WMI namespace and query, database, keyword.
    fn columns(&self) -> (i64, i64, String, String, String, String, String) {
        let (db, kw) = (self.database.clone(), self.keyword.clone());
        match &self.raise {
            Raise::Error(n) => (*n, 0, String::new(), String::new(), String::new(), db, kw),
            Raise::Severity(n) => (0, *n, String::new(), String::new(), String::new(), db, kw),
            Raise::Performance(p) => (
                0,
                0,
                p.clone(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ),
            Raise::Wmi { namespace, query } => (
                0,
                0,
                String::new(),
                namespace.clone(),
                query.clone(),
                String::new(),
                String::new(),
            ),
        }
    }
}

struct Read {
    id: i64,
    alert: Alert,
    history: [String; 4],
}

/// Every alert (`sp_help_alert` refuses an id it doesn't have, so the one
/// is picked here) and the operators the one notifies.
const ONE_SQL: &str = "SET NOCOUNT ON;
EXEC msdb.dbo.sp_help_alert;
SELECT o.name AS dbine_operator, n.notification_method AS dbine_method
  FROM msdb.dbo.sysnotifications n JOIN msdb.dbo.sysoperators o ON o.id = n.operator_id
 WHERE n.alert_id = @P1 ORDER BY o.name;";

fn when(r: &tiberius::Row, d: &str, t: &str) -> String {
    agent_time_of(r, d, t)
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| NEVER.to_string())
}

fn raise_of(r: &tiberius::Row) -> Raise {
    match int(r, "type").unwrap_or(1) {
        4 => Raise::Wmi {
            namespace: text(r, "wmi_namespace").unwrap_or_default(),
            query: text(r, "wmi_query").unwrap_or_default(),
        },
        2 => Raise::Performance(text(r, "performance_condition").unwrap_or_default()),
        _ => match int(r, "message_id").unwrap_or(0) {
            0 => Raise::Severity(int(r, "severity").unwrap_or(0)),
            n => Raise::Error(n),
        },
    }
}

async fn read(s: &mut SqlServerSession, id: &str) -> Result<Read> {
    let n = int_id(Kind::Alert, id)?;
    let ids = n.to_string();
    let sets = jobs::sets(s, ONE_SQL, &[&ids], None).await?;
    let Some(r) = first_with(&sets, "event_source")
        .and_then(|s| s.rows.iter().find(|r| int(r, "id") == Some(n)))
    else {
        return Err(Error::Query(NOT_FOUND.into()));
    };
    let mut operators = BTreeMap::new();
    if let Some(set) = first_with(&sets, "dbine_operator") {
        for o in &set.rows {
            if let (Some(name), Some(m)) = (text(o, "dbine_operator"), int(o, "dbine_method")) {
                operators.insert(name, m);
            }
        }
    }
    let job = guid(r, "job_id")
        .filter(|g| !g.is_nil())
        .map(|g| g.to_string());
    let alert = Alert {
        name: text(r, "name").unwrap_or_default(),
        enabled: int(r, "enabled") == Some(1),
        raise: raise_of(r),
        database: text(r, "database_name").unwrap_or_default(),
        keyword: text(r, "event_description_keyword").unwrap_or_default(),
        job,
        include: int(r, "include_event_description").unwrap_or(0),
        message: text(r, "notification_message").unwrap_or_default(),
        delay: int(r, "delay_between_responses").unwrap_or(0),
        category: text(r, "category_name").unwrap_or_else(|| UNCATEGORIZED.into()),
        operators,
    };
    let history = [
        when(r, "last_occurrence_date", "last_occurrence_time"),
        when(r, "last_response_date", "last_response_time"),
        int(r, "occurrence_count").unwrap_or(0).to_string(),
        when(r, "count_reset_date", "count_reset_time"),
    ];
    Ok(Read {
        id: n,
        alert,
        history,
    })
}

/// What raises it, in words, for the list.
fn summary(r: &Raise) -> String {
    match r {
        Raise::Error(n) => format!("Error {n}"),
        Raise::Severity(n) => format!("Gravedad {n}"),
        Raise::Performance(p) => p.replace('|', " "),
        Raise::Wmi { query, .. } => format!("WMI: {query}"),
    }
}

/// The alert's actions now: the count reset, and enable or disable,
/// whichever it isn't (SSMS greys out the other).
pub(crate) fn actions_now(enabled: bool) -> Vec<String> {
    vec![
        "reset_count".into(),
        if enabled { "disable" } else { "enable" }.into(),
    ]
}

pub(crate) async fn items(s: &mut SqlServerSession, c: &Ctx) -> Result<SchedulerList> {
    let sets = jobs::sets(
        s,
        "SET NOCOUNT ON;\nEXEC msdb.dbo.sp_help_alert;",
        &[],
        None,
    )
    .await?;
    let items = first_with(&sets, "event_source")
        .map(|set| {
            set.rows
                .iter()
                .filter_map(|r| {
                    let enabled = int(r, "enabled") == Some(1);
                    Some(SchedulerItem {
                        id: int(r, "id")?.to_string(),
                        name: text(r, "name")?,
                        description: Some(summary(&raise_of(r))),
                        enabled: Some(enabled),
                        system: false,
                        can_edit: None,
                        can_delete: None,
                        actions: c.admin().then(|| actions_now(enabled)),
                        run_from_step: None,
                        clear_history: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(SchedulerList {
        items,
        notes: Vec::new(),
    })
}

fn severities() -> FormKind {
    FormKind::select(&[
        ("1", "001 - Información diversa del sistema"),
        ("2", "002 - Reservado"),
        ("3", "003 - Reservado"),
        ("4", "004 - Reservado"),
        ("5", "005 - Reservado"),
        ("6", "006 - Reservado"),
        ("7", "007 - Notificación: información de estado"),
        ("8", "008 - Notificación: interacción del usuario necesaria"),
        ("9", "009 - Definido por el usuario"),
        ("10", "010 - Información"),
        (
            "11",
            "011 - No se encontró el objeto de base de datos especificado",
        ),
        ("12", "012 - No usado"),
        ("13", "013 - Error de sintaxis en la transacción de usuario"),
        ("14", "014 - Permisos insuficientes para la operación"),
        ("15", "015 - Error de sintaxis en las instrucciones SQL"),
        ("16", "016 - Error de usuario varios"),
        ("17", "017 - Recursos insuficientes en el servidor"),
        ("18", "018 - Error interno no grave"),
        ("19", "019 - Error grave en un recurso"),
        ("20", "020 - Error grave en el proceso actual"),
        (
            "21",
            "021 - Error grave en los procesos de la base de datos",
        ),
        ("22", "022 - Error grave: integridad de la tabla sospechosa"),
        (
            "23",
            "023 - Error grave: integridad de la base de datos sospechosa",
        ),
        ("24", "024 - Error grave: error de hardware"),
        ("25", "025 - Error grave del sistema"),
    ])
}

/// Where the counters of an object are among the form's choices, and the
/// instances of one of its counters: the same sources the fields name with
/// their `{field}` placeholders, filled.
pub(crate) const PERF_OBJECTS: &str = "perf_object";
pub(crate) const PERF_COUNTERS: &str = "perf_counter:{perf_object}";
pub(crate) const PERF_INSTANCES: &str = "perf_instance:{perf_object}|{perf_counter}";

pub(crate) fn perf_counters(object: &str) -> String {
    format!("perf_counter:{object}")
}

pub(crate) fn perf_instances(object: &str, counter: &str) -> String {
    format!("perf_instance:{object}|{counter}")
}

/// `perf_list`: the server's counters are among the choices (VIEW SERVER
/// STATE), so object, counter and instance are picked, each from what the
/// one before allows; without them, they are typed. `wmi_namespace`: the
/// instance's own (see [`Ctx::wmi_namespace`]).
pub(crate) fn general_fields(perf_list: bool, wmi_namespace: &str) -> Vec<FormField> {
    let event = &["error", "severity"];
    let perf = |f: FormField, source: &str, example: &str| {
        let f = if perf_list {
            FormField {
                kind: FormKind::reference(source, false),
                ..f
            }
        } else if example.is_empty() {
            f
        } else {
            f.placeholder(example)
        };
        f.when("type", &["performance"])
    };
    vec![
        FormField::new("name", "Nombre", FormKind::Text).required(),
        FormField::new("enabled", "Habilitada", FormKind::Bool).default(json!(true)),
        FormField::new(
            "type",
            "Tipo",
            FormKind::select(&[
                ("error", "Alerta de eventos de SQL Server: número de error"),
                ("severity", "Alerta de eventos de SQL Server: gravedad"),
                (
                    "performance",
                    "Alerta de condición de rendimiento de SQL Server",
                ),
                ("wmi", "Alerta de eventos WMI"),
            ]),
        )
        .required()
        .default(json!("severity")),
        FormField::new(
            "message_id",
            "Número de error",
            FormKind::number(Some(1.0), Some(f64::from(i32::MAX))),
        )
        .when("type", &["error"]),
        FormField::new("severity", "Gravedad", severities())
            .when("type", &["severity"])
            .default(json!("17")),
        FormField::new(
            "database",
            "Nombre de la base de datos",
            FormKind::reference("alert_database", false),
        )
        .when("type", event)
        .default(json!("")),
        FormField::new(
            "message_text",
            "Generar la alerta cuando el mensaje contenga",
            FormKind::Text,
        )
        .when("type", event),
        perf(
            FormField::new("perf_object", "Objeto", FormKind::Text),
            PERF_OBJECTS,
            "SQLServer:General Statistics",
        ),
        perf(
            FormField::new("perf_counter", "Contador", FormKind::Text),
            PERF_COUNTERS,
            "User Connections",
        ),
        perf(
            FormField::new("perf_instance", "Instancia", FormKind::Text),
            PERF_INSTANCES,
            "",
        ),
        FormField::new(
            "perf_comparison",
            "Generar la alerta si el contador",
            FormKind::select(&[
                ("<", "es menor que"),
                ("=", "es igual a"),
                (">", "es mayor que"),
            ]),
        )
        .when("type", &["performance"])
        .default(json!(">")),
        // msdb takes only a value that starts with a digit.
        FormField::new("perf_value", "Valor", FormKind::number(Some(0.0), None))
            .when("type", &["performance"])
            .default(json!(0)),
        FormField::new("wmi_namespace", "Espacio de nombres", FormKind::Text)
            .when("type", &["wmi"])
            .default(json!(wmi_namespace)),
        FormField::new("wmi_query", "Consulta", FormKind::Textarea).when("type", &["wmi"]),
        FormField::new(
            "category",
            "Categoría",
            FormKind::reference("category:alert", false),
        )
        .default(json!(UNCATEGORIZED)),
    ]
}

pub(crate) fn values_of(a: &Alert, wmi_namespace: &str) -> BTreeMap<String, Value> {
    let mut v = BTreeMap::new();
    let mut put = |k: &str, x: Value| {
        v.insert(k.to_string(), x);
    };
    put("name", json!(a.name));
    put("enabled", json!(a.enabled));
    put("database", json!(a.database));
    put("message_text", json!(a.keyword));
    put("severity", json!("17"));
    put("perf_comparison", json!(">"));
    put("perf_value", json!(0));
    put("wmi_namespace", json!(wmi_namespace));
    match &a.raise {
        Raise::Error(n) => {
            put("type", json!("error"));
            put("message_id", json!(n));
        }
        Raise::Severity(n) => {
            put("type", json!("severity"));
            put("severity", json!(n.to_string()));
        }
        Raise::Performance(p) => {
            put("type", json!("performance"));
            let parts: Vec<&str> = p.split('|').collect();
            put("perf_object", json!(parts.first().copied().unwrap_or("")));
            put("perf_counter", json!(parts.get(1).copied().unwrap_or("")));
            put("perf_instance", json!(parts.get(2).copied().unwrap_or("")));
            put(
                "perf_comparison",
                json!(parts.get(3).copied().unwrap_or(">")),
            );
            let value = parts.get(4).copied().unwrap_or("0");
            put(
                "perf_value",
                value
                    .parse::<f64>()
                    .map(|f| json!(f))
                    .unwrap_or_else(|_| json!(value)),
            );
        }
        Raise::Wmi { namespace, query } => {
            put("type", json!("wmi"));
            put("wmi_namespace", json!(namespace));
            put("wmi_query", json!(query));
        }
    }
    put("category", json!(a.category));
    put("run_job", json!(a.job.is_some()));
    put("job", json!(a.job.clone().unwrap_or_default()));
    put("include_email", json!(a.include & 1 != 0));
    put("include_pager", json!(a.include & 2 != 0));
    put("include_netsend", json!(a.include & 4 != 0));
    put("notification_message", json!(a.message));
    put("delay_minutes", json!(a.delay / 60));
    put("delay_seconds", json!(a.delay % 60));
    v
}

pub(crate) async fn form(
    s: &mut SqlServerSession,
    c: &Ctx,
    id: Option<&str>,
) -> Result<ObjectForm> {
    let mut catalog = choices::load(
        s,
        c,
        &["database", "category", "operator", "job", "perf_object"],
    )
    .await?;
    let mut dbs = vec![(String::new(), ALL_DATABASES.to_string())];
    dbs.extend(catalog.get("database").iter().cloned());
    catalog.lists.insert("alert_database".into(), dbs);
    let namespace = c.wmi_namespace();
    let mut f = ObjectForm::default();
    let mut history = FormSection::new("history", "Historial");
    let operators: Vec<BTreeMap<String, Value>>;
    match id {
        None => {
            let a = Alert {
                name: String::new(),
                enabled: true,
                raise: Raise::Severity(17),
                database: String::new(),
                keyword: String::new(),
                job: None,
                include: 0,
                message: String::new(),
                delay: 0,
                category: UNCATEGORIZED.into(),
                operators: BTreeMap::new(),
            };
            f.values = values_of(&a, &namespace);
            operators = Vec::new();
        }
        Some(id) => {
            let r = read(s, id).await?;
            f.values = values_of(&r.alert, &namespace);
            operators = r
                .alert
                .operators
                .iter()
                .map(|(o, m)| {
                    BTreeMap::from([
                        ("operator".to_string(), json!(o)),
                        ("email".to_string(), json!(m & 1 != 0)),
                        ("pager".to_string(), json!(m & 2 != 0)),
                        ("netsend".to_string(), json!(m & 4 != 0)),
                    ])
                })
                .collect();
            history = history.fields(vec![
                FormField::new("last_occurrence", "Última alerta", FormKind::Text).read_only(true),
                FormField::new("last_response", "Última respuesta", FormKind::Text).read_only(true),
                FormField::new(
                    "occurrences",
                    "Número de veces que se produjo",
                    FormKind::Text,
                )
                .read_only(true),
                // Reset with the alert's "Restablecer el recuento" action.
                FormField::new("count_reset", "Recuento desde", FormKind::Text).read_only(true),
            ]);
            for (k, v) in [
                "last_occurrence",
                "last_response",
                "occurrences",
                "count_reset",
            ]
            .iter()
            .zip(r.history)
            {
                f.values.insert((*k).into(), json!(v));
            }
        }
    }
    let perf_list = !catalog.get("perf_object").is_empty();
    f.sections = vec![
        FormSection::new("general", "General").fields(general_fields(perf_list, &namespace)),
        FormSection::new("response", "Respuesta")
            .fields(vec![
                FormField::new("run_job", "Ejecutar job", FormKind::Bool),
                FormField::new("job", "Job", FormKind::reference("job", false))
                    .when("run_job", &["true"]),
            ])
            .list(
                FormList::new(
                    "operators",
                    "Notificar a operadores",
                    "operator",
                    vec![
                        FormField::new(
                            "operator",
                            "Operador",
                            FormKind::reference("operator", false),
                        )
                        .required(),
                        FormField::new("email", "Correo electrónico", FormKind::Bool)
                            .default(json!(true)),
                        FormField::new("pager", "Localizador", FormKind::Bool),
                        FormField::new("netsend", "Net send", FormKind::Bool),
                    ],
                )
                .rows(operators),
            ),
        FormSection::new("options", "Opciones").fields(vec![
            FormField::new(
                "include_email",
                "Incluir el texto de error de la alerta en el correo electrónico",
                FormKind::Bool,
            ),
            FormField::new(
                "include_pager",
                "Incluir el texto de error de la alerta en el localizador",
                FormKind::Bool,
            ),
            FormField::new(
                "include_netsend",
                "Incluir el texto de error de la alerta en net send",
                FormKind::Bool,
            ),
            FormField::new(
                "notification_message",
                "Mensaje de notificación adicional",
                FormKind::Textarea,
            ),
            FormField::new(
                "delay_minutes",
                "Retraso entre respuestas (minutos)",
                FormKind::number(Some(0.0), Some(35_791_394.0)),
            )
            .default(json!(0)),
            FormField::new(
                "delay_seconds",
                "Retraso entre respuestas (segundos)",
                FormKind::number(Some(0.0), Some(59.0)),
            )
            .default(json!(0)),
        ]),
        history,
    ];
    f.choices = catalog.lists;
    if !c.admin() {
        f.read_only = true;
        if c.writable() {
            f.notes.push(SYSADMIN_ONLY_FORM.into());
        }
    }
    Ok(f)
}

/// The alert the form describes, checked.
pub(crate) fn from_values(values: &FormValues, catalog: &Catalog) -> Result<Alert> {
    let v = Vals(&values.values);
    let name = v
        .opt("name")
        .ok_or_else(|| bad("La alerta necesita un nombre.".into()))?;
    fits("Nombre", &name, SYSNAME)?;
    let enabled = if v.0.get("enabled").is_some_and(|x| !x.is_null()) {
        v.bool("enabled")
    } else {
        true
    };
    let kind = v.text("type");
    let raise = match kind.as_str() {
        "error" => Raise::Error(
            v.int("message_id", "Número de error", 1, i64::from(i32::MAX))?
                .ok_or_else(
                    || bad("La alerta necesita el número de error que la genera.".into()),
                )?,
        ),
        "severity" => Raise::Severity(
            v.int("severity", "Gravedad", 1, 25)?
                .ok_or_else(|| bad("La alerta necesita la gravedad que la genera.".into()))?,
        ),
        "performance" => {
            let object = v.opt("perf_object").ok_or_else(|| {
                bad("La alerta de rendimiento necesita el objeto del contador.".into())
            })?;
            let counter = v
                .opt("perf_counter")
                .ok_or_else(|| bad("La alerta de rendimiento necesita el contador.".into()))?;
            // An instance is a database's name, among others: a leading
            // space is part of it (msdb compares right-trimmed names).
            let instance = v.raw("perf_instance").trim_end().to_string();
            let cmp = v.text("perf_comparison");
            if !matches!(cmp.as_str(), "<" | "=" | ">") {
                return Err(bad(format!(
                    "«{}» no es una comparación válida (<, = o >).",
                    shown(&cmp)
                )));
            }
            let value = v.number("perf_value", "Valor")?.ok_or_else(|| {
                bad("La alerta de rendimiento necesita el valor con el que comparar.".into())
            })?;
            if value.starts_with('-') {
                return Err(bad(
                    "El valor de una alerta de rendimiento no puede ser negativo.".into(),
                ));
            }
            for part in [&object, &counter, &instance] {
                if part.contains('|') {
                    return Err(bad(format!(
                        "La condición de rendimiento no puede llevar «|» («{}»).",
                        shown(part)
                    )));
                }
            }
            let condition = format!("{object}|{counter}|{instance}|{cmp}|{value}");
            fits("Condición de rendimiento", &condition, 512)?;
            Raise::Performance(condition)
        }
        "wmi" => {
            let namespace = v
                .opt("wmi_namespace")
                .ok_or_else(|| bad("La alerta WMI necesita el espacio de nombres.".into()))?;
            let query = v
                .opt("wmi_query")
                .ok_or_else(|| bad("La alerta WMI necesita la consulta.".into()))?;
            // `@wmi_namespace` is a sysname.
            fits("Espacio de nombres", &namespace, SYSNAME)?;
            fits("Consulta", &query, 512)?;
            Raise::Wmi { namespace, query }
        }
        other => return Err(bad(format!("«{}» no es un tipo de alerta.", shown(other)))),
    };
    let event = matches!(raise, Raise::Error(_) | Raise::Severity(_));
    let database = match v.opt("database").filter(|_| event) {
        None => String::new(),
        Some(db) => {
            let list = catalog.get("database");
            if list.is_empty() {
                db
            } else {
                pick(&db, list, |d| format!("La base de datos «{d}» no existe."))?.to_string()
            }
        }
    };
    let keyword = if event {
        v.text("message_text")
    } else {
        String::new()
    };
    if keyword.chars().count() > 100 {
        return Err(bad(
            "El texto que debe contener el mensaje admite hasta 100 caracteres.".into(),
        ));
    }
    let job = match v.opt("job").filter(|_| v.bool("run_job")) {
        None if v.bool("run_job") => {
            return Err(bad("Elegí el job que ejecuta la alerta.".into()));
        }
        None => None,
        Some(j) => {
            let id = jobs::job_id(&j)?;
            let list = catalog.get("job");
            Some(
                pick(&id, list, |_| {
                    "El job elegido no existe o este usuario no puede verlo.".into()
                })?
                .to_ascii_lowercase(),
            )
        }
    };
    let message = v.text("notification_message");
    fits("Mensaje de notificación adicional", &message, 512)?;
    let include = i64::from(v.bool("include_email"))
        | i64::from(v.bool("include_pager")) << 1
        | i64::from(v.bool("include_netsend")) << 2;
    let delay = v
        .int(
            "delay_minutes",
            "Retraso entre respuestas (minutos)",
            0,
            35_791_394,
        )?
        .unwrap_or(0)
        * 60
        + v.int(
            "delay_seconds",
            "Retraso entre respuestas (segundos)",
            0,
            59,
        )?
        .unwrap_or(0);
    let category = match v.opt("category") {
        None => UNCATEGORIZED.to_string(),
        Some(cat) => {
            let list = catalog.get("category:alert");
            if list.is_empty() {
                cat
            } else {
                pick(&cat, list, |c| {
                    format!("La categoría de alertas «{c}» no existe.")
                })?
                .to_string()
            }
        }
    };
    let mut operators = BTreeMap::new();
    for row in rows(values, "operators") {
        let Some(o) = row.opt("operator") else {
            continue;
        };
        let list = catalog.get("operator");
        let o = if list.is_empty() {
            o
        } else {
            pick(&o, list, |o| format!("El operador «{o}» no existe."))?.to_string()
        };
        let m = i64::from(row.bool("email"))
            | i64::from(row.bool("pager")) << 1
            | i64::from(row.bool("netsend")) << 2;
        if m > 0 {
            operators.insert(o, m);
        }
    }
    Ok(Alert {
        name,
        enabled,
        raise,
        database,
        keyword,
        job,
        include,
        message,
        delay,
        category,
        operators,
    })
}

/// A performance object's name past its instance's prefix ("SQLServer:" on
/// a default instance, "MSSQL$NAME:" on a named one), as msdb compares it:
/// what follows the first ':', or the whole name without one.
fn unprefixed(object: &str) -> &str {
    object.split_once(':').map_or(object, |(_, rest)| rest)
}

/// The server's object a condition names: the same name (any case) or, as
/// `sp_verify_performance_condition` matches it, the same name past the
/// instance's prefix (any case), so one scripted on another instance
/// ("MSSQL$OTHER:General Statistics") or without a prefix ("General
/// Statistics") names this server's. Returns the server's spelling.
fn perf_object<'a>(object: &str, objects: &'a [(String, String)]) -> Result<&'a str> {
    let bare = unprefixed(object);
    objects
        .iter()
        .find(|(v, _)| v == object)
        .or_else(|| objects.iter().find(|(v, _)| v.eq_ignore_ascii_case(object)))
        .or_else(|| objects.iter().find(|(v, _)| unprefixed(v) == bare))
        .or_else(|| {
            objects
                .iter()
                .find(|(v, _)| unprefixed(v).eq_ignore_ascii_case(bare))
        })
        .map(|(v, _)| v.as_str())
        .ok_or_else(|| {
            bad(format!(
                "El objeto de rendimiento «{}» no existe en este servidor.",
                shown(object)
            ))
        })
}

/// A performance condition checked as `sp_verify_performance_condition`
/// checks it (an object, a counter of it and an instance of that counter,
/// none instead only where the counter has a row without one), against the
/// server's counters, and written in the server's spelling (its object's
/// prefix included). Without the counters (no VIEW SERVER STATE) msdb
/// checks it alone.
pub(crate) fn check_performance(condition: &str, catalog: &Catalog) -> Result<String> {
    let objects = catalog.get(PERF_OBJECTS);
    let parts: Vec<&str> = condition.split('|').collect();
    let [object, counter, instance, cmp, value] = parts[..] else {
        return Ok(condition.to_string());
    };
    if objects.is_empty() {
        return Ok(condition.to_string());
    }
    let object = perf_object(object, objects)?;
    let counter = pick(counter, catalog.get(&perf_counters(object)), |c| {
        format!("El objeto «{}» no tiene el contador «{c}».", shown(object))
    })?;
    let instances = catalog.get(&perf_instances(object, counter));
    let instance = pick(instance, instances, |i| {
        if i.is_empty() {
            format!("Elegí una instancia del contador «{}».", shown(counter))
        } else {
            format!(
                "El contador «{}» no tiene la instancia «{i}».",
                shown(counter)
            )
        }
    })?;
    Ok(format!("{object}|{counter}|{instance}|{cmp}|{value}"))
}

/// The alert kind's action keys (see [`super::info_of`]).
pub(crate) const ACTIONS: [&str; 3] = ["reset_count", "enable", "disable"];

/// The script of one of the alert's actions, or `None` for a key it
/// doesn't have. "reset_count" is SSMS's "Reset Count": the count to zero,
/// counted from now on the server's clock (`sp_update_alert` would keep
/// the old reset date otherwise; a procedure's argument can't be an
/// expression, so the date goes through variables).
pub(crate) fn action_sql(name: &str, action: &str) -> Option<String> {
    let args = vec![("name", A::T(name.into()))];
    match action {
        "reset_count" => {
            let mut sc = Script::new(
                &format!("Restablece el recuento de la alerta «{name}» del Agente SQL Server"),
                &["@resetAt datetime", "@resetDate int", "@resetTime int"],
            );
            sc.line("SET @resetAt = GETDATE();");
            sc.line("SET @resetDate = CONVERT(int, CONVERT(char(8), @resetAt, 112));");
            sc.line(
                "SET @resetTime = DATEPART(HOUR, @resetAt) * 10000 + DATEPART(MINUTE, @resetAt) * 100 + DATEPART(SECOND, @resetAt);",
            );
            let mut args = args;
            args.push(("occurrence_count", A::I(0)));
            args.push(("count_reset_date", A::R("@resetDate".into())));
            args.push(("count_reset_time", A::R("@resetTime".into())));
            sc.call("msdb.dbo.sp_update_alert", args);
            Some(sc.finish(None))
        }
        "enable" | "disable" => {
            let on = action == "enable";
            let comment = if on {
                format!("Habilita la alerta «{name}» del Agente SQL Server")
            } else {
                format!("Deshabilita la alerta «{name}» del Agente SQL Server")
            };
            let mut sc = Script::new(&comment, &[]);
            let mut args = args;
            args.push(("enabled", A::I(i64::from(on))));
            sc.call("msdb.dbo.sp_update_alert", args);
            Some(sc.finish(None))
        }
        _ => None,
    }
}

pub(crate) async fn action_script(
    s: &mut SqlServerSession,
    c: &Ctx,
    id: &str,
    action: &str,
) -> Result<String> {
    if !ACTIONS.contains(&action) {
        return Err(no_action(action));
    }
    if !c.p.sysadmin && c.writable() {
        return Err(Error::Query(SYSADMIN_ONLY_FORM.into()));
    }
    let r = read(s, id).await?;
    action_sql(&r.alert.name, action).ok_or_else(|| no_action(action))
}

fn notifications(
    sc: &mut Script,
    alert: &str,
    want: &BTreeMap<String, i64>,
    have: &BTreeMap<String, i64>,
) {
    for o in have.keys().filter(|o| !want.contains_key(*o)) {
        sc.call(
            "msdb.dbo.sp_delete_notification",
            vec![
                ("alert_name", A::T(alert.into())),
                ("operator_name", A::T(o.clone())),
            ],
        );
    }
    for (o, m) in want {
        let proc_name = match have.get(o) {
            Some(h) if h == m => continue,
            Some(_) => "msdb.dbo.sp_update_notification",
            None => "msdb.dbo.sp_add_notification",
        };
        sc.call(
            proc_name,
            vec![
                ("alert_name", A::T(alert.into())),
                ("operator_name", A::T(o.clone())),
                ("notification_method", A::I(*m)),
            ],
        );
    }
}

pub(crate) fn create_script(a: &Alert) -> String {
    let mut sc = Script::new(&format!("Alerta «{}» del Agente SQL Server", a.name), &[]);
    let (message_id, severity, perf, ns, query, db, kw) = a.columns();
    let mut args = vec![
        ("name", A::T(a.name.clone())),
        ("message_id", A::I(message_id)),
        ("severity", A::I(severity)),
        ("enabled", A::I(i64::from(a.enabled))),
        ("delay_between_responses", A::I(a.delay)),
        ("include_event_description_in", A::I(a.include)),
    ];
    for (k, v) in [
        ("notification_message", &a.message),
        ("database_name", &db),
        ("event_description_keyword", &kw),
        ("performance_condition", &perf),
        ("wmi_namespace", &ns),
        ("wmi_query", &query),
    ] {
        if !v.is_empty() {
            args.push((k, A::T(v.clone())));
        }
    }
    if let Some(j) = &a.job {
        args.push(("job_id", A::R(guid_literal(j))));
    }
    args.push(("category_name", A::T(a.category.clone())));
    sc.call("msdb.dbo.sp_add_alert", args);
    notifications(&mut sc, &a.name, &a.operators, &BTreeMap::new());
    sc.finish(None)
}

/// `sp_update_alert` with what changed (what raises it as a whole when its
/// type changes), the notifications' diff, or `None`.
pub(crate) fn alter_script(cur: &Alert, a: &Alert) -> Option<String> {
    let mut sc = Script::new(&format!("Alerta «{}» del Agente SQL Server", cur.name), &[]);
    let mut args = vec![("name", A::T(cur.name.clone()))];
    if a.name != cur.name {
        args.push(("new_name", A::T(a.name.clone())));
    }
    if a.enabled != cur.enabled {
        args.push(("enabled", A::I(i64::from(a.enabled))));
    }
    let retype = std::mem::discriminant(&a.raise) != std::mem::discriminant(&cur.raise);
    let (m, sev, perf, ns, q, db, kw) = a.columns();
    let (om, osev, operf, ons, oq, odb, okw) = cur.columns();
    if m != om || retype {
        args.push(("message_id", A::I(m)));
    }
    if sev != osev || retype {
        args.push(("severity", A::I(sev)));
    }
    for (k, d, o) in [
        ("performance_condition", &perf, &operf),
        ("wmi_namespace", &ns, &ons),
        ("wmi_query", &q, &oq),
        ("database_name", &db, &odb),
        ("event_description_keyword", &kw, &okw),
    ] {
        if d != o {
            args.push((k, A::T(d.clone())));
        }
    }
    if a.job != cur.job {
        args.push((
            "job_id",
            A::R(match &a.job {
                Some(j) => guid_literal(j),
                None => "N'00000000-0000-0000-0000-000000000000'".into(),
            }),
        ));
    }
    if a.include != cur.include {
        args.push(("include_event_description_in", A::I(a.include)));
    }
    if a.message != cur.message {
        args.push(("notification_message", A::T(a.message.clone())));
    }
    if a.delay != cur.delay {
        args.push(("delay_between_responses", A::I(a.delay)));
    }
    if !a.category.eq_ignore_ascii_case(&cur.category) {
        args.push(("category_name", A::T(a.category.clone())));
    }
    if args.len() > 1 {
        sc.call("msdb.dbo.sp_update_alert", args);
    }
    notifications(&mut sc, &a.name, &a.operators, &cur.operators);
    (sc.calls() > 0).then(|| sc.finish(None))
}

pub(crate) fn drop_script(name: &str) -> String {
    let mut sc = Script::new(
        &format!("Borra la alerta «{name}» del Agente SQL Server"),
        &[],
    );
    sc.call(
        "msdb.dbo.sp_delete_alert",
        vec![("name", A::T(name.into()))],
    );
    sc.finish(None)
}

pub(crate) async fn plan(
    s: &mut SqlServerSession,
    c: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    if !c.p.sysadmin {
        return Err(Error::Query(SYSADMIN_ONLY_FORM.into()));
    }
    let mut sources = vec!["database", "category", "operator", "job"];
    if Vals(&values.values).text("type") == "performance" {
        sources.push(PERF_OBJECTS);
    }
    let catalog = choices::load(s, c, &sources).await?;
    let mut a = from_values(values, &catalog)?;
    let cur = match id {
        Some(id) => Some(read(s, id).await?),
        None => None,
    };
    // A new or changed condition is checked here first, for a clear
    // message and the server's spelling. One left as it was isn't: msdb
    // checks it again on every sp_update_alert anyway (the stored one when
    // none is passed, so this kind's actions too), and writing it in the
    // server's spelling would turn a save with no changes into a change.
    if let Raise::Performance(p) = &a.raise {
        let same = cur
            .as_ref()
            .is_some_and(|r| r.alert.raise == Raise::Performance(p.clone()));
        if !same {
            a.raise = Raise::Performance(check_performance(p, &catalog)?);
        }
    }
    match cur {
        None => Ok(Plan {
            script: create_script(&a),
            empty: false,
            id: None,
            output: None,
            lookup: Some((
                "SELECT CAST(id AS nvarchar(20)) AS id FROM msdb.dbo.sysalerts WHERE name = @P1",
                a.name.clone(),
            )),
        }),
        Some(r) => Ok(match alter_script(&r.alert, &a) {
            Some(script) => Plan {
                script,
                empty: false,
                id: Some(r.id.to_string()),
                output: None,
                lookup: None,
            },
            None => Plan::nothing(&r.alert.name, r.id.to_string()),
        }),
    }
}

pub(crate) async fn delete_script(s: &mut SqlServerSession, c: &Ctx, id: &str) -> Result<String> {
    if !c.p.sysadmin && c.writable() {
        return Err(Error::Query(SYSADMIN_ONLY_FORM.into()));
    }
    let r = read(s, id).await?;
    Ok(drop_script(&r.alert.name))
}
