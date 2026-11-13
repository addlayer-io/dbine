//! Snapshot lifecycle policies (SLM): Kibana's policy wizard as a form.
//! `PUT _slm/policy/<id>` creates one and replaces it in place (a new
//! version of the same policy); `_execute` (the job's "run") takes the
//! snapshot now. A new id is a new policy (the old one is deleted after
//! it, with its stats; its snapshots stay in the repository).

use super::{
    bad, json_text, list_text, obj, pick, plain_id, s, same, split_list, strs, Choices, Plan, Req,
    Shape, Vals,
};
use crate::ddl::path_segment;
use crate::jobs::text;
use crate::json::J;
use crate::EsSession;
use dbine_driver::{FormField, FormKind, FormSection, FormValues, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(crate) const NOTE: &str = "Guardar una política de snapshots la reemplaza entera (PUT) con el mismo id. Un id nuevo crea otra política y borra la anterior, con sus estadísticas; los snapshots que tomó siguen en el repositorio.";

/// A policy as the cluster has it (`GET _slm/policy/<id>`'s entry).
#[derive(Debug, Clone)]
pub(crate) struct Cur {
    pub(crate) id: String,
    pub(crate) entry: J,
}

pub(crate) async fn read(s: &EsSession, id: &str) -> Result<Cur> {
    Ok(Cur {
        id: id.to_string(),
        entry: s.es_slm(id).await?,
    })
}

pub(crate) fn sections() -> Vec<FormSection> {
    vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("id", "Id", FormKind::Text)
                .required()
                .placeholder("snapshots-nocturnos")
                .help("Cambiarlo crea una política nueva y borra la anterior."),
            FormField::new("snapshot_name", "Nombre de los snapshots", FormKind::Text)
                .required()
                .placeholder("<nocturno-{now/d}>")
                .help("Admite date math entre < >; Elasticsearch le agrega un sufijo único."),
            FormField::new(
                "repository",
                "Repositorio",
                FormKind::reference("repository", false),
            )
            .required(),
        ]),
        FormSection::new("schedule", "Programación").fields(vec![FormField::new(
            "slm_schedule",
            "Programación (cron)",
            FormKind::Cron,
        )
        .required()
        .placeholder("0 30 1 * * ?")
        .help("Cron de Elasticsearch, con segundos: segundos minutos horas día-del-mes mes día-de-la-semana [año], en UTC.")]),
        FormSection::new("snapshot", "Snapshot").fields(vec![
            FormField::new("indices", "Índices", FormKind::Text)
                .placeholder("ventas-*, -ventas-viejas")
                .help("Índices, data streams o patrones, separados por comas; vacío: todos."),
            FormField::new(
                "include_global_state",
                "Incluir el estado del clúster",
                FormKind::Bool,
            )
            .default(json!(true))
            .help("Plantillas, pipelines, ajustes persistentes y los estados de las funcionalidades."),
            FormField::new(
                "feature_states",
                "Estados de funcionalidades",
                FormKind::reference("feature", true),
            )
            .help("Vacío: los de todas (cuando se incluye el estado del clúster)."),
            FormField::new(
                "ignore_unavailable",
                "Ignorar los índices que falten",
                FormKind::Bool,
            ),
            FormField::new("partial", "Permitir snapshots parciales", FormKind::Bool)
                .help("Toma el snapshot aunque falten shards primarios."),
            FormField::new(
                "snapshot_metadata",
                "Metadatos del snapshot",
                FormKind::code("json"),
            )
            .placeholder("{ \"tomado_por\": \"slm\" }"),
        ]),
        FormSection::new("retention", "Retención").fields(vec![
            FormField::new("expire_after", "Vencen a los", FormKind::Text)
                .placeholder("30d")
                .help("Los snapshots más viejos se borran (respetando el mínimo)."),
            FormField::new(
                "min_count",
                "Mínimo a conservar",
                FormKind::number(Some(1.0), None),
            ),
            FormField::new(
                "max_count",
                "Máximo a conservar",
                FormKind::number(Some(1.0), None),
            ),
        ]),
    ]
}

pub(crate) fn values_of(cur: &Cur) -> BTreeMap<String, Value> {
    let p = cur.entry.get("policy").unwrap_or(&J::Null);
    let c = p.get("config");
    let cfg = |k: &str| c.and_then(|c| c.get(k));
    let flag = |k: &str, d: bool| cfg(k).and_then(J::as_bool).unwrap_or(d);
    let mut v = BTreeMap::new();
    v.insert("id".into(), json!(cur.id));
    v.insert(
        "snapshot_name".into(),
        json!(text(p.get("name")).unwrap_or_default()),
    );
    v.insert(
        "repository".into(),
        json!(text(p.get("repository")).unwrap_or_default()),
    );
    v.insert(
        "slm_schedule".into(),
        json!(text(p.get("schedule")).unwrap_or_default()),
    );
    v.insert("indices".into(), json!(list_text(cfg("indices"))));
    v.insert(
        "include_global_state".into(),
        json!(flag("include_global_state", true)),
    );
    let features: Vec<String> = match cfg("feature_states") {
        Some(J::Arr(a)) => a.iter().map(J::text).collect(),
        _ => Vec::new(),
    };
    v.insert("feature_states".into(), json!(features));
    v.insert(
        "ignore_unavailable".into(),
        json!(flag("ignore_unavailable", false)),
    );
    v.insert("partial".into(), json!(flag("partial", false)));
    v.insert("snapshot_metadata".into(), json_text(cfg("metadata")));
    let r = |k: &str| p.at(&["retention", k]);
    v.insert(
        "expire_after".into(),
        json!(text(r("expire_after")).unwrap_or_default()),
    );
    for k in ["min_count", "max_count"] {
        v.insert(
            k.into(),
            r(k).and_then(J::as_u64).map_or(json!(""), |n| json!(n)),
        );
    }
    v
}

/// A policy as the form describes it: its id and `PUT`'s body.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Slm {
    pub(crate) id: String,
    pub(crate) body: J,
}

/// `choices`: the repositories and features to check the picks against
/// (`None`: don't check, the values were read from the cluster).
pub(crate) fn spec(values: &BTreeMap<String, Value>, choices: Option<&Choices>) -> Result<Slm> {
    let v = Vals(values);
    let id = plain_id(v.opt("id"), "Id")?;
    let name = v
        .line("snapshot_name", "Nombre de los snapshots")?
        .ok_or_else(|| bad("Falta el nombre de los snapshots.".into()))?;
    let mut repository = v
        .line("repository", "Repositorio")?
        .ok_or_else(|| bad("Falta el repositorio de los snapshots.".into()))?;
    let schedule = v
        .cron("slm_schedule", "Programación (cron)", 6..=7)?
        .ok_or_else(|| bad("Falta la programación de la política.".into()))?;
    let mut features = v.strings("feature_states");
    if let Some(ch) = choices {
        repository = pick(&repository, ch.get("repository"), "El repositorio")?;
        for f in &mut features {
            if f != "none" {
                *f = pick(f, ch.get("feature"), "La funcionalidad")?;
            }
        }
    }
    let indices = split_list(&v.raw("indices"));
    let config = obj(vec![
        ("indices", (!indices.is_empty()).then(|| strs(&indices))),
        (
            "include_global_state",
            Some(J::Bool(v.bool_or("include_global_state", true))),
        ),
        (
            "feature_states",
            (!features.is_empty()).then(|| strs(&features)),
        ),
        (
            "ignore_unavailable",
            Some(J::Bool(v.bool("ignore_unavailable"))),
        ),
        ("partial", Some(J::Bool(v.bool("partial")))),
        (
            "metadata",
            v.json("snapshot_metadata", "Metadatos del snapshot", Shape::Object)?,
        ),
    ]);
    let retention = obj(vec![
        (
            "expire_after",
            v.time("expire_after", "Vencen a los")?.map(s),
        ),
        (
            "min_count",
            v.int("min_count", "Mínimo a conservar", 1, i64::from(i32::MAX))?
                .map(|n| J::Num(n.into())),
        ),
        (
            "max_count",
            v.int("max_count", "Máximo a conservar", 1, i64::from(i32::MAX))?
                .map(|n| J::Num(n.into())),
        ),
    ]);
    if let (Some(a), Some(b)) = (
        retention.get("min_count").and_then(J::as_u64),
        retention.get("max_count").and_then(J::as_u64),
    ) {
        if a > b {
            return Err(bad(
                "El mínimo a conservar no puede ser mayor que el máximo.".into(),
            ));
        }
    }
    let has_retention = retention.as_obj().is_some_and(|o| !o.is_empty());
    let body = obj(vec![
        ("name", Some(s(name))),
        ("schedule", Some(s(schedule))),
        ("repository", Some(s(repository))),
        ("config", Some(config)),
        ("retention", has_retention.then_some(retention)),
    ]);
    Ok(Slm { id, body })
}

fn put(p: &Slm) -> Req {
    Req::new(
        "PUT",
        format!("/_slm/policy/{}", path_segment(&p.id)),
        Some(p.body.clone()),
    )
}

pub(crate) fn plan(cur: Option<&Cur>, values: &FormValues, choices: &Choices) -> Result<Plan> {
    let want = spec(&values.values, Some(choices))?;
    let Some(cur) = cur else {
        return Ok(Plan {
            notes: vec![format!("Política de snapshots «{}»", want.id)],
            reqs: vec![put(&want)],
            id: Some(super::Type::Slm.job_id(&want.id)),
            name: want.id.clone(),
            ..Plan::default()
        });
    };
    let have = spec(&values_of(cur), None)?;
    let mut plan = Plan {
        notes: vec![format!("Política de snapshots «{}»", cur.id)],
        id: Some(super::Type::Slm.job_id(&want.id)),
        name: cur.id.clone(),
        ..Plan::default()
    };
    if want.id != cur.id {
        plan.warnings.push(format!(
            "Un id nuevo crea la política «{}» y borra «{}», con sus estadísticas; sus snapshots siguen en el repositorio.",
            want.id, cur.id
        ));
        plan.reqs.push(put(&want));
        plan.reqs.push(Req::new(
            "DELETE",
            format!("/_slm/policy/{}", path_segment(&cur.id)),
            None,
        ));
    } else if !same(&want.body, &have.body) {
        plan.reqs.push(put(&want));
    }
    Ok(plan)
}

pub(crate) fn delete_plan(cur: &Cur) -> Plan {
    Plan {
        notes: vec![format!(
            "Borra la política de snapshots «{}»; los snapshots que tomó siguen en el repositorio.",
            cur.id
        )],
        reqs: vec![Req::new(
            "DELETE",
            format!("/_slm/policy/{}", path_segment(&cur.id)),
            None,
        )],
        name: cur.id.clone(),
        ..Plan::default()
    }
}
