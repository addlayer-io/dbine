//! OpenSearch snapshot management policies (OpenSearch 2.1 on): OpenSearch
//! Dashboards' "Create policy" page as a form. `POST _plugins/_sm/policies/
//! <name>` creates one; `PUT` with `if_seq_no` / `if_primary_term`
//! replaces it in place; `_start` / `_stop` follow "Habilitada". A new name
//! is a new policy (the old one is deleted after it, with its state; its
//! snapshots stay in the repository). OpenSearch has no API that takes a
//! policy's snapshot now.

use super::{bad, json_text, obj, pick, plain_id, s, same, Choices, Ctx, Plan, Req, Shape, Vals};
use crate::ddl::path_segment;
use crate::jobs::{num, text};
use crate::json::J;
use crate::EsSession;
use dbine_driver::{FormField, FormKind, FormSection, FormValues, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(crate) const NOTE: &str = "Guardar una política de snapshots la reemplaza entera con el mismo nombre. Un nombre nuevo crea otra política y borra la anterior, con su estado; los snapshots que tomó siguen en el repositorio.";

/// A policy as the cluster has it.
#[derive(Debug, Clone)]
pub(crate) struct Cur {
    pub(crate) name: String,
    pub(crate) policy: J,
    pub(crate) seq: String,
    pub(crate) term: String,
}

impl Cur {
    fn enabled(&self) -> bool {
        self.policy
            .get("enabled")
            .and_then(J::as_bool)
            .unwrap_or(false)
    }
}

pub(crate) async fn read(s: &EsSession, name: &str) -> Result<Cur> {
    let (policy, seq, term, _) = s.os_sm(name).await?;
    Ok(Cur {
        name: name.to_string(),
        policy,
        seq,
        term,
    })
}

pub(crate) fn sections() -> Vec<FormSection> {
    vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("id", "Nombre", FormKind::Text)
                .required()
                .placeholder("snapshots-diarios")
                .help("Cambiarlo crea una política nueva y borra la anterior."),
            FormField::new("description", "Descripción", FormKind::Textarea),
            FormField::new("enabled", "Habilitada", FormKind::Bool).default(json!(true)),
        ]),
        FormSection::new("schedule", "Programación").fields(vec![
            FormField::new("creation_cron", "Creación (cron)", FormKind::Cron)
                .required()
                .placeholder("0 8 * * *")
                .help("Cuándo se toma cada snapshot: cron de cinco campos."),
            FormField::new("creation_timezone", "Zona horaria", FormKind::Text)
                .default(json!("UTC"))
                .placeholder("America/Argentina/Buenos_Aires"),
            FormField::new("creation_time_limit", "Tiempo máximo", FormKind::Text)
                .placeholder("1h")
                .help("Si el snapshot tarda más, la ejecución se da por fallida."),
        ]),
        FormSection::new("snapshot", "Snapshot").fields(vec![
            FormField::new(
                "repository",
                "Repositorio",
                FormKind::reference("repository", false),
            )
            .required(),
            FormField::new("indices", "Índices", FormKind::Text)
                .placeholder("ventas-*,logs-*")
                .help("Índices o patrones separados por comas; vacío: todos."),
            FormField::new("date_format", "Formato de fecha del nombre", FormKind::Text)
                .placeholder("yyyy-MM-dd'T'HH:mm:ss")
                .help("Los nombres son <política>-<fecha>-<sufijo>."),
            FormField::new(
                "date_format_timezone",
                "Zona horaria del nombre",
                FormKind::Text,
            )
            .placeholder("UTC"),
            FormField::new(
                "include_global_state",
                "Incluir el estado del clúster",
                FormKind::Bool,
            )
            .default(json!(true)),
            FormField::new("partial", "Permitir snapshots parciales", FormKind::Bool),
            FormField::new(
                "ignore_unavailable",
                "Ignorar los índices que falten",
                FormKind::Bool,
            ),
            FormField::new(
                "snapshot_metadata",
                "Metadatos del snapshot",
                FormKind::code("json"),
            ),
        ]),
        FormSection::new("deletion", "Borrado").fields(vec![
            FormField::new("deletion", "Borrar snapshots viejos", FormKind::Bool),
            FormField::new("max_age", "Edad máxima", FormKind::Text)
                .when("deletion", &["true"])
                .placeholder("30d"),
            FormField::new(
                "max_count",
                "Cantidad máxima",
                FormKind::number(Some(1.0), None),
            )
            .when("deletion", &["true"]),
            FormField::new(
                "min_count",
                "Cantidad mínima",
                FormKind::number(Some(1.0), None),
            )
            .when("deletion", &["true"]),
            FormField::new("deletion_cron", "Borrado (cron)", FormKind::Cron)
                .when("deletion", &["true"])
                .placeholder("0 1 * * *")
                .help("Vacío: según la programación de la creación."),
            FormField::new(
                "deletion_timezone",
                "Zona horaria del borrado",
                FormKind::Text,
            )
            .when("deletion", &["true"])
            .placeholder("UTC"),
            FormField::new(
                "deletion_time_limit",
                "Tiempo máximo del borrado",
                FormKind::Text,
            )
            .when("deletion", &["true"])
            .placeholder("1h"),
        ]),
        FormSection::new("notification", "Notificaciones").fields(vec![
            FormField::new("channel", "Canal", FormKind::reference("channel", false)),
            FormField::new(
                "notify_on",
                "Avisar ante",
                FormKind::multi(&[
                    ("creation", "Snapshot creado"),
                    ("deletion", "Snapshots borrados"),
                    ("failure", "Fallo"),
                    ("time_limit_exceeded", "Tiempo máximo superado"),
                ]),
            )
            .default(json!(["failure"])),
        ]),
    ]
}

pub(crate) fn values_of(cur: &Cur) -> BTreeMap<String, Value> {
    let p = &cur.policy;
    let t = |path: &[&str]| json!(text(p.at(path)).unwrap_or_default());
    let mut v = BTreeMap::new();
    v.insert("id".into(), json!(cur.name));
    v.insert("description".into(), t(&["description"]));
    v.insert("enabled".into(), json!(cur.enabled()));
    v.insert(
        "creation_cron".into(),
        t(&["creation", "schedule", "cron", "expression"]),
    );
    v.insert(
        "creation_timezone".into(),
        t(&["creation", "schedule", "cron", "timezone"]),
    );
    v.insert("creation_time_limit".into(), t(&["creation", "time_limit"]));
    v.insert("repository".into(), t(&["snapshot_config", "repository"]));
    v.insert("indices".into(), t(&["snapshot_config", "indices"]));
    v.insert("date_format".into(), t(&["snapshot_config", "date_format"]));
    v.insert(
        "date_format_timezone".into(),
        t(&["snapshot_config", "timezone"]),
    );
    let flag = |k: &str, d: bool| {
        json!(p
            .at(&["snapshot_config", k])
            .and_then(|x| match x {
                J::Bool(b) => Some(*b),
                J::Str(s) => Some(s == "true"),
                _ => None,
            })
            .unwrap_or(d))
    };
    v.insert(
        "include_global_state".into(),
        flag("include_global_state", true),
    );
    v.insert("partial".into(), flag("partial", false));
    v.insert(
        "ignore_unavailable".into(),
        flag("ignore_unavailable", false),
    );
    v.insert(
        "snapshot_metadata".into(),
        json_text(p.at(&["snapshot_config", "metadata"])),
    );
    let d = p.get("deletion");
    v.insert("deletion".into(), json!(d.is_some()));
    v.insert("max_age".into(), t(&["deletion", "condition", "max_age"]));
    for k in ["max_count", "min_count"] {
        v.insert(
            k.into(),
            num(p.at(&["deletion", "condition", k])).map_or(json!(""), |n| json!(n as i64)),
        );
    }
    v.insert(
        "deletion_cron".into(),
        t(&["deletion", "schedule", "cron", "expression"]),
    );
    v.insert(
        "deletion_timezone".into(),
        t(&["deletion", "schedule", "cron", "timezone"]),
    );
    v.insert("deletion_time_limit".into(), t(&["deletion", "time_limit"]));
    v.insert("channel".into(), t(&["notification", "channel", "id"]));
    let on: Vec<&str> = ["creation", "deletion", "failure", "time_limit_exceeded"]
        .into_iter()
        .filter(|k| {
            p.at(&["notification", "conditions", k])
                .and_then(J::as_bool)
                == Some(true)
        })
        .collect();
    v.insert("notify_on".into(), json!(on));
    v
}

/// A policy as the form describes it: its name and the request's body.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Sm {
    pub(crate) name: String,
    pub(crate) enabled: bool,
    pub(crate) body: J,
}

fn cron(v: Vals, cron: &str, tz: &str, required: bool) -> Result<Option<J>> {
    if !required && v.opt(cron).is_none() {
        return Ok(None);
    }
    let c = super::os_transform::cron_schedule(v, cron, tz)?;
    Ok(Some(c))
}

pub(crate) fn spec(values: &BTreeMap<String, Value>, choices: Option<&Choices>) -> Result<Sm> {
    let v = Vals(values);
    let name = plain_id(v.opt("id"), "Nombre")?;
    let mut repository = v
        .line("repository", "Repositorio")?
        .ok_or_else(|| bad("Falta el repositorio de los snapshots.".into()))?;
    let mut channel = v.line("channel", "Canal")?;
    if let Some(ch) = choices {
        repository = pick(&repository, ch.get("repository"), "El repositorio")?;
        if let Some(c) = &channel {
            channel = Some(pick(c, ch.get("channel"), "El canal")?);
        }
    }
    let creation = obj(vec![
        (
            "schedule",
            cron(v, "creation_cron", "creation_timezone", true)?,
        ),
        (
            "time_limit",
            v.time("creation_time_limit", "Tiempo máximo")?.map(s),
        ),
    ]);
    let deletion = if v.bool("deletion") {
        let condition = obj(vec![
            ("max_age", v.time("max_age", "Edad máxima")?.map(s)),
            (
                "max_count",
                v.int("max_count", "Cantidad máxima", 1, i64::from(i32::MAX))?
                    .map(|n| J::Num(n.into())),
            ),
            (
                "min_count",
                v.int("min_count", "Cantidad mínima", 1, i64::from(i32::MAX))?
                    .map(|n| J::Num(n.into())),
            ),
        ]);
        if condition.as_obj().is_some_and(Vec::is_empty) {
            return Err(bad(
                "El borrado necesita una edad máxima o una cantidad máxima.".into(),
            ));
        }
        Some(obj(vec![
            (
                "schedule",
                cron(v, "deletion_cron", "deletion_timezone", false)?,
            ),
            ("condition", Some(condition)),
            (
                "time_limit",
                v.time("deletion_time_limit", "Tiempo máximo del borrado")?
                    .map(s),
            ),
        ]))
    } else {
        None
    };
    let flag = |k: &str, d: bool| Some(J::Bool(v.bool_or(k, d)));
    let snapshot_config = obj(vec![
        ("repository", Some(s(repository))),
        ("indices", v.line("indices", "Índices")?.map(s)),
        (
            "date_format",
            v.line("date_format", "Formato de fecha del nombre")?.map(s),
        ),
        (
            "timezone",
            v.line("date_format_timezone", "Zona horaria del nombre")?
                .map(s),
        ),
        ("include_global_state", flag("include_global_state", true)),
        ("partial", flag("partial", false)),
        ("ignore_unavailable", flag("ignore_unavailable", false)),
        (
            "metadata",
            v.json("snapshot_metadata", "Metadatos del snapshot", Shape::Object)?,
        ),
    ]);
    let notification = channel.map(|id| {
        let on = v.strings("notify_on");
        let conditions = ["creation", "deletion", "failure", "time_limit_exceeded"]
            .iter()
            .map(|k| (k.to_string(), J::Bool(on.iter().any(|x| x == k))))
            .collect();
        obj(vec![
            ("channel", Some(obj(vec![("id", Some(s(id)))]))),
            ("conditions", Some(J::Obj(conditions))),
        ])
    });
    let enabled = v.bool_or("enabled", true);
    let body = obj(vec![
        ("description", Some(s(v.text("description")))),
        ("creation", Some(creation)),
        ("deletion", deletion),
        ("snapshot_config", Some(snapshot_config)),
        ("notification", notification),
    ]);
    Ok(Sm {
        name,
        enabled,
        body,
    })
}

/// A create request's body: the policy and whether it starts enabled.
fn create_body(p: &Sm) -> J {
    let mut b = p.body.clone();
    if let J::Obj(o) = &mut b {
        o.push(("enabled".into(), J::Bool(p.enabled)));
    }
    b
}

pub(crate) fn plan(
    c: &Ctx,
    cur: Option<&Cur>,
    values: &FormValues,
    choices: &Choices,
) -> Result<Plan> {
    let pfx = c.plugins;
    let want = spec(&values.values, Some(choices))?;
    let create = Req::new(
        "POST",
        format!("/{pfx}/_sm/policies/{}", path_segment(&want.name)),
        Some(create_body(&want)),
    );
    let Some(cur) = cur else {
        return Ok(Plan {
            notes: vec![format!("Política de snapshots «{}»", want.name)],
            reqs: vec![create],
            id: Some(super::Type::Sm.job_id(&want.name)),
            name: want.name.clone(),
            ..Plan::default()
        });
    };
    let have = spec(&values_of(cur), None)?;
    let seg = path_segment(&cur.name);
    let mut plan = Plan {
        notes: vec![format!("Política de snapshots «{}»", cur.name)],
        id: Some(super::Type::Sm.job_id(&want.name)),
        name: cur.name.clone(),
        ..Plan::default()
    };
    if want.name != cur.name {
        plan.notes.push(format!(
            "Un nombre nuevo crea la política «{}» y borra «{}», con su estado; sus snapshots siguen en el repositorio.",
            want.name, cur.name
        ));
        plan.reqs.push(create);
        plan.reqs.push(Req::new(
            "DELETE",
            format!("/{pfx}/_sm/policies/{seg}"),
            None,
        ));
        return Ok(plan);
    }
    if !same(&want.body, &have.body) {
        plan.reqs.push(Req::new(
            "PUT",
            format!(
                "/{pfx}/_sm/policies/{seg}?if_seq_no={}&if_primary_term={}",
                cur.seq, cur.term
            ),
            Some(want.body.clone()),
        ));
    }
    if want.enabled != cur.enabled() {
        let verb = if want.enabled { "_start" } else { "_stop" };
        plan.reqs.push(Req::new(
            "POST",
            format!("/{pfx}/_sm/policies/{seg}/{verb}"),
            None,
        ));
    }
    Ok(plan)
}

pub(crate) fn delete_plan(pfx: &str, cur: &Cur) -> Plan {
    Plan {
        notes: vec![format!(
            "Borra la política de snapshots «{}»; los snapshots que tomó siguen en el repositorio.",
            cur.name
        )],
        reqs: vec![Req::new(
            "DELETE",
            format!("/{pfx}/_sm/policies/{}", path_segment(&cur.name)),
            None,
        )],
        name: cur.name.clone(),
        ..Plan::default()
    }
}
