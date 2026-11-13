//! The Scheduler's own attributes (`SET_SCHEDULER_ATTRIBUTE`): default
//! time zone, log history, slave processes, event expiry, the mail server
//! job notifications use, and whether the Scheduler runs at all. One item;
//! MANAGE SCHEDULER changes it.

use super::choices;
use super::timing::{tz_norm, tz_ok};
use super::{bad, fingerprint, fresh, pick_ref, shown, Block, Ctx, Plan, Vals, A, MANAGE_ONLY};
use crate::jobs;
use dbine_driver::{
    FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerItem, SchedulerList,
};
use oracledb::Connection;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(crate) const ID: &str = "scheduler";

/// The attributes the designer edits, in the dictionary's names.
const EDITABLE: [&str; 9] = [
    "DEFAULT_TIMEZONE",
    "LOG_HISTORY",
    "MAX_JOB_SLAVE_PROCESSES",
    "EVENT_EXPIRY_TIME",
    "EMAIL_SERVER",
    "EMAIL_SERVER_ENCRYPTION",
    "EMAIL_SERVER_CREDENTIAL",
    "EMAIL_SENDER",
    "SCHEDULER_DISABLED",
];

/// Each attribute's value as the designer edits it: `None`, unset.
pub(crate) fn current(ctx: &Ctx) -> BTreeMap<String, Option<String>> {
    EDITABLE
        .iter()
        .filter(|a| ctx.globals.contains_key(**a) || **a == "SCHEDULER_DISABLED")
        .map(|a| {
            let v = ctx
                .globals
                .get(*a)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let v = match *a {
                "SCHEDULER_DISABLED" => Some(
                    if v.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("TRUE")) {
                        "TRUE"
                    } else {
                        "FALSE"
                    }
                    .to_string(),
                ),
                "DEFAULT_TIMEZONE" => v.map(|t| tz_norm(&t)),
                "EMAIL_SERVER_CREDENTIAL" => v.map(|t| match jobs::object_name(&t, "SYS") {
                    Some((o, n)) => super::obj_id(Some(&o), &n),
                    None => t,
                }),
                _ => v,
            };
            (a.to_ascii_lowercase(), v)
        })
        .collect()
}

pub(crate) fn values_of(cur: &BTreeMap<String, Option<String>>) -> BTreeMap<String, Value> {
    cur.iter()
        .map(|(k, v)| {
            let value = match (k.as_str(), v) {
                ("scheduler_disabled", v) => json!(v.as_deref() == Some("TRUE")),
                ("log_history" | "max_job_slave_processes" | "event_expiry_time", Some(n)) => {
                    n.parse::<i64>().map_or(json!(n), |n| json!(n))
                }
                (_, Some(s)) => json!(s),
                (_, None) => json!(""),
            };
            (k.clone(), value)
        })
        .collect()
}

/// The attributes from the designer, checked, as they're set.
pub(crate) fn wanted(
    values: &FormValues,
    cur: &BTreeMap<String, Option<String>>,
    cat: &choices::Catalog,
) -> Result<BTreeMap<String, Option<String>>> {
    let v = Vals(&values.values);
    let mut out = BTreeMap::new();
    for k in cur.keys() {
        let value = match k.as_str() {
            "default_timezone" => match v.opt(k) {
                Some(t) if !tz_ok(&t) => {
                    return Err(bad(format!(
                        "«Zona horaria predeterminada»: «{}» no es una zona horaria.",
                        shown(&t)
                    )))
                }
                t => t.map(|t| tz_norm(&t)),
            },
            "log_history" => v
                .int(k, "Días de historial", 0, 1_000_000)?
                .map(|n| n.to_string()),
            "max_job_slave_processes" => v
                .int(k, "Máximo de procesos de jobs", 1, 1000)?
                .map(|n| n.to_string()),
            "event_expiry_time" => v
                .int(k, "Vida de los eventos", 1, 999_999_999)?
                .map(|n| n.to_string()),
            "email_server_encryption" => Some(v.choice(
                k,
                "Cifrado del correo",
                &["NONE", "SSL_TLS", "STARTTLS"],
                "NONE",
            )?),
            "email_server_credential" => match v.opt(k) {
                Some(c) => Some(pick_ref(
                    cat,
                    "credential",
                    &c,
                    cur.get(k).and_then(|x| x.as_deref()),
                    "Credencial del correo",
                )?),
                None => None,
            },
            "scheduler_disabled" => Some(if v.bool(k) { "TRUE" } else { "FALSE" }.to_string()),
            _ => {
                let label = match k.as_str() {
                    "email_server" => "Servidor de correo",
                    "email_sender" => "Remitente",
                    _ => "Valor",
                };
                let t = v.opt(k);
                super::fits(&t, label, 512)?;
                if t.as_deref()
                    .is_some_and(|t| t.chars().any(char::is_control))
                {
                    return Err(bad(format!("«{label}» no puede tener saltos de línea.")));
                }
                t
            }
        };
        out.insert(k.clone(), value);
    }
    Ok(out)
}

pub(crate) fn block(
    old: &BTreeMap<String, Option<String>>,
    new: &BTreeMap<String, Option<String>>,
) -> Block {
    let mut b = Block::new("Cambia los atributos del Scheduler.");
    for (k, v) in new {
        if old.get(k) == Some(v) {
            continue;
        }
        let value = match (k.as_str(), v) {
            ("email_server_credential", Some(id)) => match super::parse_obj(id, "credencial") {
                Ok((o, n)) => A::S(format!("{}.{}", crate::quote(&o), crate::quote(&n))),
                Err(_) => A::S(id.clone()),
            },
            (_, v) => A::opt(v.clone()),
        };
        b.call(
            "DBMS_SCHEDULER.SET_SCHEDULER_ATTRIBUTE",
            vec![("attribute", A::S(k.clone())), ("value", value)],
        );
    }
    b
}

pub(crate) fn items(_c: &Connection, _ctx: &Ctx) -> Result<SchedulerList> {
    Ok(SchedulerList {
        items: vec![SchedulerItem {
            id: ID.into(),
            name: "Atributos del Scheduler".into(),
            description: None,
            enabled: None,
            system: true,
            can_edit: None,
            can_delete: None,
            actions: None,
        }],
        notes: Vec::new(),
    })
}

pub(crate) fn plan(c: &Connection, ctx: &Ctx, values: &FormValues) -> Result<Plan> {
    if !ctx.manage() {
        return Err(bad(MANAGE_ONLY.into()));
    }
    let cat = choices::load(c, ctx, &["credential"]);
    let cur = current(ctx);
    fresh(values, &fingerprint(&values_of(&cur), &BTreeMap::new()))?;
    let new = wanted(values, &cur, &cat)?;
    Ok(Plan::changes(
        block(&cur, &new),
        ID.into(),
        "Atributos del Scheduler",
    ))
}

pub(crate) fn form(c: &Connection, ctx: &Ctx) -> Result<ObjectForm> {
    let cat = choices::load(c, ctx, &["credential"]);
    let cur = current(ctx);
    let mut values = values_of(&cur);
    let fp = fingerprint(&values, &BTreeMap::new());
    values.insert("dbine_state".into(), json!(fp));
    for (k, a) in [
        ("current_open_window", "CURRENT_OPEN_WINDOW"),
        ("file_watcher_count", "FILE_WATCHER_COUNT"),
        ("last_observed_event", "LAST_OBSERVED_EVENT"),
    ] {
        values.insert(
            k.into(),
            json!(ctx.globals.get(a).cloned().unwrap_or_default()),
        );
    }
    let has = |k: &str| cur.contains_key(k);
    let mut general = FormSection::new("general", "General");
    let mut mail = FormSection::new("mail", "Correo");
    let candidates: Vec<(bool, FormField)> = vec![
        (
            true,
            FormField::new("default_timezone", "Zona horaria predeterminada", FormKind::Text)
                .placeholder("America/Argentina/Buenos_Aires")
                .help("La de los jobs y ventanas que no dicen la suya."),
        ),
        (
            true,
            FormField::new(
                "log_history",
                "Días de historial",
                FormKind::number(Some(0.0), Some(1_000_000.0)),
            )
            .help("Cuántos días guarda el Scheduler sus registros (las clases de job pueden decir otro)."),
        ),
        (
            true,
            FormField::new(
                "max_job_slave_processes",
                "Máximo de procesos de jobs",
                FormKind::number(Some(1.0), Some(1000.0)),
            )
            .help("Vacío: sin límite propio (manda job_queue_processes)."),
        ),
        (
            true,
            FormField::new("event_expiry_time", "Vida de los eventos", FormKind::Duration)
                .help("Cuánto quedan en la cola los eventos que publica el Scheduler. Vacío: 24 horas."),
        ),
        (
            true,
            FormField::new("scheduler_disabled", "Scheduler deshabilitado", FormKind::Bool)
                .help("Detiene todos los jobs del Scheduler. Oracle no lo permite dentro de una base enchufable (PDB)."),
        ),
        (
            false,
            FormField::new("email_server", "Servidor de correo", FormKind::Text)
                .placeholder("smtp.empresa.com:25")
                .help("Host y puerto SMTP para las notificaciones de los jobs."),
        ),
        (
            false,
            FormField::new(
                "email_server_encryption",
                "Cifrado del correo",
                FormKind::select(&[
                    ("NONE", "Ninguno"),
                    ("SSL_TLS", "SSL/TLS"),
                    ("STARTTLS", "STARTTLS"),
                ]),
            ),
        ),
        (
            false,
            FormField::new(
                "email_server_credential",
                "Credencial del correo",
                FormKind::reference("credential", false),
            )
            .help("El usuario y la contraseña del servidor SMTP, si los pide."),
        ),
        (
            false,
            FormField::new("email_sender", "Remitente", FormKind::Text)
                .placeholder("oracle@empresa.com"),
        ),
    ];
    for (in_general, field) in candidates {
        if !has(&field.key) {
            continue;
        }
        if in_general {
            general = general.field(field);
        } else {
            mail = mail.field(field);
        }
    }
    general = general.fields(vec![
        FormField::new(
            "current_open_window",
            "Ventana abierta ahora",
            FormKind::Text,
        )
        .read_only(true),
        FormField::new(
            "file_watcher_count",
            "Observadores de archivos",
            FormKind::Text,
        )
        .read_only(true),
        FormField::new(
            "last_observed_event",
            "Último evento observado",
            FormKind::Text,
        )
        .read_only(true),
    ]);
    let mut f = ObjectForm {
        sections: vec![general, mail],
        values,
        choices: cat.lists,
        ..ObjectForm::default()
    };
    if !ctx.manage() {
        f.read_only = true;
        f.notes.push(MANAGE_ONLY.into());
    }
    Ok(f)
}
