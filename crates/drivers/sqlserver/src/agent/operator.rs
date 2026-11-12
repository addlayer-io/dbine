//! Operators: SSMS's "New Operator" dialog (General with the pager's
//! on-duty schedule, Notifications, History), `sp_add_operator` /
//! `sp_update_operator` / `sp_delete_operator`, and the alerts that notify
//! the operator (`sp_add_notification` / `sp_update_notification` /
//! `sp_delete_notification`). sysadmin only.

use super::choices::{self, Catalog};
use super::{
    bad, fits, int_id, pick, rows, shown, time_field, time_text, Ctx, Kind, Plan, Script, Vals, A,
    SYSADMIN_ONLY_FORM, SYSNAME,
};
use crate::jobs::{self, agent_time_of, first_with, int, text};
use crate::SqlServerSession;
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result,
    SchedulerItem, SchedulerList,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const NOT_FOUND: &str = "El operador no existe o este usuario no puede verlo.";
const NEVER: &str = "Nunca";
const PAGER_MANAGED: &str = "Azure SQL Managed Instance solo avisa por correo electrónico: el localizador y net send no se usan.";
pub(crate) const UNCATEGORIZED: &str = "[Uncategorized]";

/// `pager_days`: Sunday 1 … Saturday 64.
const DAYS: [(&str, i64); 7] = [
    ("sunday", 1),
    ("monday", 2),
    ("tuesday", 4),
    ("wednesday", 8),
    ("thursday", 16),
    ("friday", 32),
    ("saturday", 64),
];

/// An operator as msdb keeps it.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Operator {
    pub(crate) name: String,
    pub(crate) enabled: bool,
    pub(crate) email: String,
    pub(crate) pager: String,
    pub(crate) netsend: String,
    pub(crate) pager_days: i64,
    /// Weekday, Saturday and Sunday on-duty start and end, `hhmmss`.
    pub(crate) hours: [i64; 6],
    pub(crate) category: String,
    /// Alert name → `notification_method` (1 e-mail, 2 pager, 4 net send).
    pub(crate) alerts: BTreeMap<String, i64>,
}

const HOUR_ARGS: [&str; 6] = [
    "weekday_pager_start_time",
    "weekday_pager_end_time",
    "saturday_pager_start_time",
    "saturday_pager_end_time",
    "sunday_pager_start_time",
    "sunday_pager_end_time",
];

const HOUR_FIELDS: [(&str, &str, i64); 6] = [
    (
        "weekday_start",
        "Inicio de la jornada (lunes a viernes)",
        80000,
    ),
    ("weekday_end", "Fin de la jornada (lunes a viernes)", 180000),
    ("saturday_start", "Inicio (sábado)", 80000),
    ("saturday_end", "Fin (sábado)", 180000),
    ("sunday_start", "Inicio (domingo)", 80000),
    ("sunday_end", "Fin (domingo)", 180000),
];

/// The operator, its alerts, the jobs that notify it; `@P1` its id.
/// (`sp_help_operator` lists them all, for every Agent role; the one is
/// picked by id here, since asking for a missing id is an error.)
const ONE_SQL: &str = "SET NOCOUNT ON;
EXEC msdb.dbo.sp_help_operator;
IF HAS_PERMS_BY_NAME(N'msdb.dbo.sysnotifications', N'OBJECT', N'SELECT') = 1
   AND HAS_PERMS_BY_NAME(N'msdb.dbo.sysalerts', N'OBJECT', N'SELECT') = 1
    SELECT a.name AS dbine_alert, n.notification_method AS dbine_method
      FROM msdb.dbo.sysnotifications n JOIN msdb.dbo.sysalerts a ON a.id = n.alert_id
     WHERE n.operator_id = @P1 ORDER BY a.name;
SELECT name AS dbine_job,
       CASE WHEN notify_email_operator_id = @P1 THEN notify_level_email ELSE 0 END AS dbine_email,
       CASE WHEN notify_page_operator_id = @P1 THEN notify_level_page ELSE 0 END AS dbine_page,
       CASE WHEN notify_netsend_operator_id = @P1 THEN notify_level_netsend ELSE 0 END AS dbine_netsend
  FROM msdb.dbo.sysjobs_view
 WHERE notify_email_operator_id = @P1 OR notify_page_operator_id = @P1 OR notify_netsend_operator_id = @P1
 ORDER BY name;";

struct Read {
    id: i64,
    op: Operator,
    /// Job name and when it notifies by e-mail, pager and net send.
    jobs: Vec<(String, [&'static str; 3])>,
    last: [String; 3],
}

/// When a job notifies (`notify_level_*`), as the job form's "Cuándo"
/// says it (whole texts, so the app translates them); empty: never.
fn level_text(level: i64) -> &'static str {
    match level {
        0 => "",
        1 => "Cuando el job se complete correctamente",
        2 => "Cuando el job genere un error",
        _ => "Cuando el job se complete",
    }
}

async fn read(s: &mut SqlServerSession, id: &str) -> Result<Read> {
    let n = int_id(Kind::Operator, id)?;
    let ids = n.to_string();
    let sets = jobs::sets(s, ONE_SQL, &[&ids], None).await?;
    let Some(r) = first_with(&sets, "pager_days")
        .and_then(|s| s.rows.iter().find(|r| int(r, "id") == Some(n)))
    else {
        return Err(Error::Query(NOT_FOUND.into()));
    };
    let mut op = Operator {
        name: text(r, "name").unwrap_or_default(),
        enabled: int(r, "enabled") == Some(1),
        email: text(r, "email_address").unwrap_or_default(),
        pager: text(r, "pager_address").unwrap_or_default(),
        netsend: text(r, "netsend_address").unwrap_or_default(),
        pager_days: int(r, "pager_days").unwrap_or(0),
        hours: [0; 6],
        category: text(r, "category_name").unwrap_or_else(|| UNCATEGORIZED.into()),
        alerts: BTreeMap::new(),
    };
    for (i, k) in HOUR_ARGS.iter().enumerate() {
        op.hours[i] = int(r, k).unwrap_or(0);
    }
    let last = [
        ("last_email_date", "last_email_time"),
        ("last_pager_date", "last_pager_time"),
        ("last_netsend_date", "last_netsend_time"),
    ]
    .map(|(d, t)| {
        agent_time_of(r, d, t)
            .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| NEVER.to_string())
    });
    if let Some(set) = first_with(&sets, "dbine_alert") {
        for r in &set.rows {
            if let (Some(a), Some(m)) = (text(r, "dbine_alert"), int(r, "dbine_method")) {
                op.alerts.insert(a, m);
            }
        }
    }
    let jobs = first_with(&sets, "dbine_job")
        .map(|set| {
            set.rows
                .iter()
                .filter_map(|r| {
                    let name = text(r, "dbine_job")?;
                    let when = ["dbine_email", "dbine_page", "dbine_netsend"]
                        .map(|col| level_text(int(r, col).unwrap_or(0)));
                    Some((name, when))
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Read {
        id: n,
        op,
        jobs,
        last,
    })
}

pub(crate) async fn items(s: &mut SqlServerSession, _c: &Ctx) -> Result<SchedulerList> {
    let sets = jobs::sets(
        s,
        "SET NOCOUNT ON;\nEXEC msdb.dbo.sp_help_operator;",
        &[],
        None,
    )
    .await?;
    let items = first_with(&sets, "pager_days")
        .map(|set| {
            set.rows
                .iter()
                .filter_map(|r| {
                    Some(SchedulerItem {
                        id: int(r, "id")?.to_string(),
                        name: text(r, "name")?,
                        description: text(r, "email_address"),
                        enabled: Some(int(r, "enabled") == Some(1)),
                        system: false,
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

fn days_kind() -> FormKind {
    FormKind::multi(&[
        ("monday", "Lunes"),
        ("tuesday", "Martes"),
        ("wednesday", "Miércoles"),
        ("thursday", "Jueves"),
        ("friday", "Viernes"),
        ("saturday", "Sábado"),
        ("sunday", "Domingo"),
    ])
}

pub(crate) fn general_fields() -> Vec<FormField> {
    let mut f = vec![
        FormField::new("name", "Nombre", FormKind::Text).required(),
        FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
        FormField::new("email", "Nombre de correo electrónico", FormKind::Text)
            .placeholder("dba@contoso.com"),
        FormField::new("netsend", "Dirección de envío de red", FormKind::Text),
        FormField::new(
            "pager",
            "Nombre de correo electrónico del localizador",
            FormKind::Text,
        ),
        FormField::new(
            "pager_days",
            "Días de servicio del localizador",
            days_kind(),
        ),
    ];
    let hours = [
        FormField::new(
            "weekday_start",
            "Inicio de la jornada (lunes a viernes)",
            FormKind::Time,
        ),
        FormField::new(
            "weekday_end",
            "Fin de la jornada (lunes a viernes)",
            FormKind::Time,
        ),
        FormField::new("saturday_start", "Inicio (sábado)", FormKind::Time),
        FormField::new("saturday_end", "Fin (sábado)", FormKind::Time),
        FormField::new("sunday_start", "Inicio (domingo)", FormKind::Time),
        FormField::new("sunday_end", "Fin (domingo)", FormKind::Time),
    ];
    for (field, (_, _, default)) in hours.into_iter().zip(HOUR_FIELDS) {
        f.push(field.default(json!(time_text(default))));
    }
    f.push(
        FormField::new(
            "category",
            "Categoría",
            FormKind::reference("category:operator", false),
        )
        .default(json!(UNCATEGORIZED)),
    );
    f
}

pub(crate) fn values_of(op: &Operator) -> BTreeMap<String, Value> {
    let mut v = BTreeMap::new();
    v.insert("name".into(), json!(op.name));
    v.insert("enabled".into(), json!(op.enabled));
    v.insert("email".into(), json!(op.email));
    v.insert("netsend".into(), json!(op.netsend));
    v.insert("pager".into(), json!(op.pager));
    let days: Vec<&str> = DAYS
        .iter()
        .filter(|(_, b)| op.pager_days & b != 0)
        .map(|(k, _)| *k)
        .collect();
    v.insert("pager_days".into(), json!(days));
    for (i, (key, _, _)) in HOUR_FIELDS.iter().enumerate() {
        v.insert((*key).into(), json!(time_text(op.hours[i])));
    }
    v.insert("category".into(), json!(op.category));
    v
}

pub(crate) async fn form(
    s: &mut SqlServerSession,
    c: &Ctx,
    id: Option<&str>,
) -> Result<ObjectForm> {
    let catalog = choices::load(s, c, &["alert", "category"]).await?;
    let mut f = ObjectForm::default();
    let (values, alert_rows, job_rows, last) = match id {
        None => {
            let op = Operator {
                enabled: true,
                hours: [80000, 180000, 80000, 180000, 80000, 180000],
                category: UNCATEGORIZED.into(),
                ..Operator::default()
            };
            (values_of(&op), Vec::new(), Vec::new(), None)
        }
        Some(id) => {
            let r = read(s, id).await?;
            let alerts =
                r.op.alerts
                    .iter()
                    .map(|(a, m)| {
                        BTreeMap::from([
                            ("alert".to_string(), json!(a)),
                            ("email".to_string(), json!(m & 1 != 0)),
                            ("pager".to_string(), json!(m & 2 != 0)),
                            ("netsend".to_string(), json!(m & 4 != 0)),
                        ])
                    })
                    .collect();
            let jobs = r
                .jobs
                .iter()
                .map(|(j, [email, page, netsend])| {
                    BTreeMap::from([
                        ("job".to_string(), json!(j)),
                        ("email".to_string(), json!(email)),
                        ("pager".to_string(), json!(page)),
                        ("netsend".to_string(), json!(netsend)),
                    ])
                })
                .collect();
            (values_of(&r.op), alerts, jobs, Some(r.last))
        }
    };
    let mut history = FormSection::new("history", "Historial");
    f.values = values;
    if let Some(last) = last {
        history = history.fields(vec![
            FormField::new(
                "last_email",
                "Último intento de correo electrónico",
                FormKind::Text,
            )
            .read_only(true),
            FormField::new(
                "last_pager",
                "Último intento de localizador",
                FormKind::Text,
            )
            .read_only(true),
            FormField::new("last_netsend", "Último intento de net send", FormKind::Text)
                .read_only(true),
        ]);
        for (k, v) in ["last_email", "last_pager", "last_netsend"]
            .iter()
            .zip(last)
        {
            f.values.insert((*k).into(), json!(v));
        }
    }
    f.sections = vec![
        FormSection::new("general", "General").fields(general_fields()),
        FormSection::new("notifications", "Notificaciones")
            .list(
                FormList::new(
                    "alerts",
                    "Alertas",
                    "alert",
                    vec![
                        FormField::new("alert", "Alerta", FormKind::reference("alert", false))
                            .required(),
                        FormField::new("email", "Correo electrónico", FormKind::Bool)
                            .default(json!(true)),
                        FormField::new("pager", "Localizador", FormKind::Bool),
                        FormField::new("netsend", "Net send", FormKind::Bool),
                    ],
                )
                .rows(alert_rows)
                .read_only(!c.admin()),
            )
            .list(
                FormList::new(
                    "jobs",
                    "Jobs que lo notifican",
                    "job",
                    vec![
                        FormField::new("job", "Job", FormKind::Text).read_only(true),
                        FormField::new("email", "Correo electrónico", FormKind::Text)
                            .read_only(true),
                        FormField::new("pager", "Localizador", FormKind::Text).read_only(true),
                        FormField::new("netsend", "Net send", FormKind::Text).read_only(true),
                    ],
                )
                .rows(job_rows)
                .read_only(true),
            ),
        history,
    ];
    f.choices = catalog.lists;
    if c.managed() {
        f.notes.push(PAGER_MANAGED.into());
    }
    if !c.admin() {
        f.read_only = true;
        if c.writable() {
            f.notes.insert(0, SYSADMIN_ONLY_FORM.into());
        }
    }
    Ok(f)
}

/// The operator the form describes.
pub(crate) fn from_values(values: &FormValues, catalog: &Catalog) -> Result<Operator> {
    let v = Vals(&values.values);
    let name = v
        .opt("name")
        .ok_or_else(|| bad("El operador necesita un nombre.".into()))?;
    fits("Nombre", &name, SYSNAME)?;
    let (email, pager, netsend) = (v.text("email"), v.text("pager"), v.text("netsend"));
    fits("Nombre de correo electrónico", &email, 100)?;
    fits("Nombre de correo electrónico del localizador", &pager, 100)?;
    fits("Dirección de envío de red", &netsend, 100)?;
    let enabled = if v.0.get("enabled").is_some_and(|x| !x.is_null()) {
        v.bool("enabled")
    } else {
        true
    };
    let mut days = 0;
    for d in v.strings("pager_days") {
        days |= DAYS
            .iter()
            .find(|(k, _)| *k == d)
            .map(|(_, b)| *b)
            .ok_or_else(|| bad(format!("«{}» no es un día de la semana.", shown(&d))))?;
    }
    let mut hours = [0; 6];
    for (i, (key, label, default)) in HOUR_FIELDS.iter().enumerate() {
        hours[i] = time_field(v, key, label, *default)?;
    }
    let category = match v.opt("category") {
        None => UNCATEGORIZED.to_string(),
        Some(cat) => {
            let list = catalog.get("category:operator");
            if list.is_empty() {
                cat
            } else {
                pick(&cat, list, |c| {
                    format!("La categoría de operadores «{c}» no existe.")
                })?
                .to_string()
            }
        }
    };
    let mut alerts = BTreeMap::new();
    for row in rows(values, "alerts") {
        let Some(a) = row.opt("alert") else { continue };
        let list = catalog.get("alert");
        let a = if list.is_empty() {
            a
        } else {
            pick(&a, list, |a| format!("La alerta «{a}» no existe."))?.to_string()
        };
        let m = i64::from(row.bool("email"))
            | i64::from(row.bool("pager")) << 1
            | i64::from(row.bool("netsend")) << 2;
        if m > 0 {
            alerts.insert(a, m);
        }
    }
    Ok(Operator {
        name,
        enabled,
        email,
        pager,
        netsend,
        pager_days: days,
        hours,
        category,
        alerts,
    })
}

fn notifications(
    sc: &mut Script,
    operator: &str,
    want: &BTreeMap<String, i64>,
    have: &BTreeMap<String, i64>,
) {
    for (a, _) in have.iter().filter(|(a, _)| !want.contains_key(*a)) {
        sc.call(
            "msdb.dbo.sp_delete_notification",
            vec![
                ("alert_name", A::T(a.clone())),
                ("operator_name", A::T(operator.into())),
            ],
        );
    }
    for (a, m) in want {
        match have.get(a) {
            Some(h) if h == m => {}
            Some(_) => sc.call(
                "msdb.dbo.sp_update_notification",
                vec![
                    ("alert_name", A::T(a.clone())),
                    ("operator_name", A::T(operator.into())),
                    ("notification_method", A::I(*m)),
                ],
            ),
            None => sc.call(
                "msdb.dbo.sp_add_notification",
                vec![
                    ("alert_name", A::T(a.clone())),
                    ("operator_name", A::T(operator.into())),
                    ("notification_method", A::I(*m)),
                ],
            ),
        }
    }
}

pub(crate) fn create_script(op: &Operator) -> String {
    let mut sc = Script::new(
        &format!("Operador «{}» del Agente SQL Server", op.name),
        &[],
    );
    let mut args = vec![
        ("name", A::T(op.name.clone())),
        ("enabled", A::I(i64::from(op.enabled))),
    ];
    for (k, v) in [
        ("email_address", &op.email),
        ("pager_address", &op.pager),
        ("netsend_address", &op.netsend),
    ] {
        if !v.is_empty() {
            args.push((k, A::T(v.clone())));
        }
    }
    for (k, h) in HOUR_ARGS.iter().zip(op.hours) {
        args.push((k, A::I(h)));
    }
    args.push(("pager_days", A::I(op.pager_days)));
    args.push(("category_name", A::T(op.category.clone())));
    sc.call("msdb.dbo.sp_add_operator", args);
    notifications(&mut sc, &op.name, &op.alerts, &BTreeMap::new());
    sc.finish(None)
}

/// `sp_update_operator` with what changed, the notifications' diff, or
/// `None`.
pub(crate) fn alter_script(cur: &Operator, op: &Operator) -> Option<String> {
    let mut sc = Script::new(
        &format!("Operador «{}» del Agente SQL Server", cur.name),
        &[],
    );
    let mut args = vec![("name", A::T(cur.name.clone()))];
    if op.name != cur.name {
        args.push(("new_name", A::T(op.name.clone())));
    }
    if op.enabled != cur.enabled {
        args.push(("enabled", A::I(i64::from(op.enabled))));
    }
    for (k, d, o) in [
        ("email_address", &op.email, &cur.email),
        ("pager_address", &op.pager, &cur.pager),
        ("netsend_address", &op.netsend, &cur.netsend),
    ] {
        if d != o {
            args.push((k, A::T(d.clone())));
        }
    }
    for (i, k) in HOUR_ARGS.iter().enumerate() {
        if op.hours[i] != cur.hours[i] {
            args.push((k, A::I(op.hours[i])));
        }
    }
    if op.pager_days != cur.pager_days {
        args.push(("pager_days", A::I(op.pager_days)));
    }
    if !op.category.eq_ignore_ascii_case(&cur.category) {
        args.push(("category_name", A::T(op.category.clone())));
    }
    if args.len() > 1 {
        sc.call("msdb.dbo.sp_update_operator", args);
    }
    notifications(&mut sc, &op.name, &op.alerts, &cur.alerts);
    (sc.calls() > 0).then(|| sc.finish(None))
}

pub(crate) fn drop_script(name: &str) -> String {
    let mut sc = Script::new(
        &format!("Borra el operador «{name}» del Agente SQL Server"),
        &[],
    );
    sc.call(
        "msdb.dbo.sp_delete_operator",
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
    let catalog = choices::load(s, c, &["alert", "category"]).await?;
    let op = from_values(values, &catalog)?;
    match id {
        None => Ok(Plan {
            script: create_script(&op),
            empty: false,
            id: None,
            output: None,
            lookup: Some((
                "SELECT CAST(id AS nvarchar(20)) AS id FROM msdb.dbo.sysoperators WHERE name = @P1",
                op.name.clone(),
            )),
        }),
        Some(id) => {
            let r = read(s, id).await?;
            Ok(match alter_script(&r.op, &op) {
                Some(script) => Plan {
                    script,
                    empty: false,
                    id: Some(r.id.to_string()),
                    output: None,
                    lookup: None,
                },
                None => Plan::nothing(&r.op.name, r.id.to_string()),
            })
        }
    }
}

pub(crate) async fn delete_script(s: &mut SqlServerSession, c: &Ctx, id: &str) -> Result<String> {
    if !c.p.sysadmin && c.writable() {
        return Err(Error::Query(SYSADMIN_ONLY_FORM.into()));
    }
    let r = read(s, id).await?;
    Ok(drop_script(&r.op.name))
}
