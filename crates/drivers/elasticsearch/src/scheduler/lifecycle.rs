//! Index lifecycle policies, each engine's own: OpenSearch / Open Distro's
//! Index State Management (ISM: `_ism/policies`, states with their actions
//! and transitions, run by the job scheduler on every managed index) and,
//! for parity, Elasticsearch's Index Lifecycle Management (ILM:
//! `_ilm/policy`, phases). They aren't jobs: no run, no history; their
//! kinds are "ism" and "ilm".
//!
//! - ISM: `PUT _ism/policies/<id>` creates one; with `if_seq_no` /
//!   `if_primary_term` it changes it in place. The indices it already
//!   manages keep the version they started with (ISM's "change policy"
//!   moves them).
//! - ILM: `PUT _ilm/policy/<name>` creates one or replaces it (a new
//!   version; the indices move to it once they finish their current
//!   phase). Policies Elastic manages are read only.
//!
//! A new id or name is a new policy, then the old one is deleted (the
//! cluster refuses while indices use it). An ISM policy with a template is
//! created first without it, the old one deleted, and the template added
//! last (with the first `PUT`'s `_seq_no` / `_primary_term`): ISM refuses
//! two templates alike, and the policy is never missing.

use super::{bad, json_text, obj, plain_id, s, same, shown, Ctx, Plan, Req, Shape, Vals};
use crate::ddl::path_segment;
use crate::jobs::{self, text};
use crate::json::J;
use crate::EsSession;
use dbine_driver::{
    Error, FormField, FormKind, FormSection, FormValues, Result, SchedulerItem, SchedulerList,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(crate) const ISM_NOTE: &str = "Los índices que la política ya administra siguen con la versión con la que empezaron: ISM aplica los cambios a los índices nuevos (o a los que se cambian de política).";
pub(crate) const ILM_NOTE: &str = "Guardar reemplaza la política entera con una versión nueva: los índices que la usan pasan a ella al terminar la fase en la que están.";
const ILM_MANAGED: &str =
    "Esta política la administra Elastic: no se cambia ni se borra desde aquí.";

/// A policy as the cluster has it.
#[derive(Debug, Clone)]
pub(crate) struct Cur {
    pub(crate) id: String,
    /// ISM: `policy`; ILM: `policy` (`phases`, `_meta`).
    pub(crate) policy: J,
    pub(crate) seq: String,
    pub(crate) term: String,
    /// ILM: indices, data streams and templates using it.
    pub(crate) in_use: Vec<String>,
}

impl Cur {
    pub(crate) fn managed(&self) -> bool {
        self.policy.at(&["_meta", "managed"]).and_then(J::as_bool) == Some(true)
    }
}

fn check_id(id: &str, ilm: bool) -> Result<()> {
    if jobs::one_name(id) {
        Ok(())
    } else {
        Err(bad(format!(
            "«{}» no es el nombre de una política {}",
            shown(id),
            if ilm { "ILM" } else { "ISM" }
        )))
    }
}

pub(crate) async fn read_ism(s: &EsSession, id: &str) -> Result<Cur> {
    check_id(id, false)?;
    let p = s.plugins();
    let (status, v) = s
        .job_probe(
            "GET",
            &format!("/{p}/_ism/policies/{}", path_segment(id)),
            None,
        )
        .await?;
    if status == 404 || text(v.get("_id")).as_deref() != Some(id) {
        return Err(Error::Query(format!("no existe la política ISM {id}")));
    }
    if status >= 400 {
        return Err(Error::Query(crate::es_error_message(status, &v.compact())));
    }
    // The GET of one wraps it twice (`policy.policy`); the list, once.
    let policy = v
        .at(&["policy", "policy"])
        .or_else(|| v.get("policy"))
        .cloned()
        .unwrap_or(J::Null);
    Ok(Cur {
        id: id.to_string(),
        policy,
        seq: text(v.get("_seq_no")).unwrap_or_default(),
        term: text(v.get("_primary_term")).unwrap_or_default(),
        in_use: Vec::new(),
    })
}

pub(crate) async fn read_ilm(s: &EsSession, id: &str) -> Result<Cur> {
    check_id(id, true)?;
    let (status, v) = s
        .job_probe("GET", &format!("/_ilm/policy/{}", path_segment(id)), None)
        .await?;
    if status == 404 {
        return Err(Error::Query(format!("no existe la política ILM {id}")));
    }
    if status >= 400 {
        return Err(Error::Query(crate::es_error_message(status, &v.compact())));
    }
    let e = v
        .get(id)
        .ok_or_else(|| Error::Query(format!("no existe la política ILM {id}")))?;
    Ok(Cur {
        id: id.to_string(),
        policy: e.get("policy").cloned().unwrap_or(J::Null),
        seq: String::new(),
        term: String::new(),
        in_use: in_use(e),
    })
}

fn in_use(e: &J) -> Vec<String> {
    ["indices", "data_streams", "composable_templates"]
        .iter()
        .flat_map(|k| {
            e.at(&["in_use_by", k])
                .and_then(J::as_arr)
                .into_iter()
                .flatten()
                .map(J::text)
                .collect::<Vec<_>>()
        })
        .collect()
}

pub(crate) async fn items(s: &EsSession, c: &Ctx) -> Result<SchedulerList> {
    let mut items = Vec::new();
    let mut notes = Vec::new();
    if c.os {
        let p = c.plugins;
        let mut from = 0;
        loop {
            let (status, v) = s
                .job_probe(
                    "GET",
                    &format!("/{p}/_ism/policies?from={from}&size=1000"),
                    None,
                )
                .await?;
            if status >= 400 {
                if !v.text().contains("index_not_found") {
                    notes.push(
                        "este usuario no puede ver las políticas ISM (faltan permisos de Index Management)"
                            .to_string(),
                    );
                }
                break;
            }
            let page = v.get("policies").and_then(J::as_arr).unwrap_or(&[]);
            for h in page {
                let pol = h.get("policy").unwrap_or(&J::Null);
                if let Some(id) = text(h.get("_id")) {
                    items.push(SchedulerItem {
                        name: id.clone(),
                        id,
                        description: text(pol.get("description")),
                        enabled: None,
                        system: false,
                    });
                }
            }
            from += 1000;
            if page.len() < 1000 {
                break;
            }
        }
    } else {
        let (status, v) = s.job_probe("GET", "/_ilm/policy", None).await?;
        if status >= 400 {
            notes.push(
                "este usuario no puede ver las políticas ILM (hace falta el privilegio read_ilm o manage_ilm)"
                    .to_string(),
            );
        }
        for (id, e) in v.as_obj().into_iter().flatten() {
            let pol = e.get("policy").unwrap_or(&J::Null);
            let phases: Vec<String> = pol
                .get("phases")
                .and_then(J::as_obj)
                .into_iter()
                .flatten()
                .map(|(k, _)| k.clone())
                .collect();
            items.push(SchedulerItem {
                id: id.clone(),
                name: id.clone(),
                description: text(pol.at(&["_meta", "description"]))
                    .or_else(|| (!phases.is_empty()).then(|| phases.join(", "))),
                enabled: None,
                system: pol.at(&["_meta", "managed"]).and_then(J::as_bool) == Some(true),
            });
        }
        items.sort_by(|a, b| a.name.cmp(&b.name));
    }
    Ok(SchedulerList { items, notes })
}

// ---------------------------------------------------------------------------
// ISM
// ---------------------------------------------------------------------------

pub(crate) fn ism_sections() -> Vec<FormSection> {
    vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("id", "Id", FormKind::Text)
                .required()
                .placeholder("borrar-a-los-30-dias")
                .help("Cambiarlo crea una política nueva y borra la anterior."),
            FormField::new("description", "Descripción", FormKind::Textarea),
            FormField::new("default_state", "Estado inicial", FormKind::Text)
                .required()
                .placeholder("caliente")
                .help("El estado en que empiezan los índices; tiene que ser uno de los estados."),
        ]),
        FormSection::new("states", "Estados").fields(vec![FormField::new(
            "states",
            "Estados",
            FormKind::code("json"),
        )
        .required()
        .default(json!("[\n  {\n    \"name\": \"caliente\",\n    \"actions\": [],\n    \"transitions\": [\n      { \"state_name\": \"borrar\", \"conditions\": { \"min_index_age\": \"30d\" } }\n    ]\n  },\n  {\n    \"name\": \"borrar\",\n    \"actions\": [{ \"delete\": {} }],\n    \"transitions\": []\n  }\n]"))
        .help("Lista de estados: name, actions (rollover, force_merge, read_only, replica_count, shrink, snapshot, delete, notification…) y transitions (state_name y conditions).")]),
        FormSection::new("options", "Opciones").fields(vec![
            FormField::new("ism_template", "Plantillas ISM", FormKind::code("json"))
                .placeholder("[{ \"index_patterns\": [\"logs-*\"], \"priority\": 100 }]")
                .help("Los índices nuevos que coinciden con los patrones quedan administrados por la política."),
            FormField::new(
                "error_notification",
                "Aviso de errores",
                FormKind::code("json"),
            )
            .placeholder("{ \"channel\": { \"id\": \"…\" }, \"message_template\": { \"source\": \"…\" } }"),
        ]),
    ]
}

/// ISM templates without the time the cluster stamps on them.
fn clean_templates(t: Option<&J>) -> Option<J> {
    match t? {
        J::Null => None,
        J::Arr(a) => Some(J::Arr(
            a.iter()
                .map(|x| jobs::without(x, &["last_updated_time"]))
                .collect(),
        )),
        x => Some(jobs::without(x, &["last_updated_time"])),
    }
}

pub(crate) fn ism_values(cur: &Cur) -> BTreeMap<String, Value> {
    let p = &cur.policy;
    let mut v = BTreeMap::new();
    v.insert("id".into(), json!(cur.id));
    v.insert(
        "description".into(),
        json!(text(p.get("description")).unwrap_or_default()),
    );
    v.insert(
        "default_state".into(),
        json!(text(p.get("default_state")).unwrap_or_default()),
    );
    v.insert(
        "states".into(),
        json_text(Some(p.get("states").unwrap_or(&J::Arr(Vec::new())))),
    );
    v.insert(
        "ism_template".into(),
        json_text(clean_templates(p.get("ism_template")).as_ref()),
    );
    v.insert(
        "error_notification".into(),
        json_text(p.get("error_notification")),
    );
    v
}

/// A policy as the form describes it: its id and `PUT`'s body.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Policy {
    pub(crate) id: String,
    pub(crate) body: J,
}

/// `opendistro`: Open Distro takes one template, as an object.
pub(crate) fn ism_spec(values: &BTreeMap<String, Value>, opendistro: bool) -> Result<Policy> {
    let v = Vals(values);
    let id = plain_id(v.opt("id"), "Id")?;
    let default_state = v
        .line("default_state", "Estado inicial")?
        .ok_or_else(|| bad("Falta el estado inicial de la política.".into()))?;
    let states = v
        .json("states", "Estados", Shape::Array)?
        .ok_or_else(|| bad("Faltan los estados de la política.".into()))?;
    let names: Vec<String> = states
        .as_arr()
        .into_iter()
        .flatten()
        .filter_map(|x| text(x.get("name")))
        .collect();
    if !names.contains(&default_state) {
        return Err(bad(format!(
            "El estado inicial «{}» no es uno de los estados de la política.",
            shown(&default_state)
        )));
    }
    let template = match v.raw("ism_template").trim().starts_with('{') {
        true => v.json("ism_template", "Plantillas ISM", Shape::Object)?,
        false => v.json("ism_template", "Plantillas ISM", Shape::Array)?,
    };
    let template = match template {
        Some(J::Arr(mut a)) if opendistro => match a.len() {
            0 => None,
            1 => a.pop(),
            _ => {
                return Err(bad(
                    "Open Distro admite una sola plantilla ISM por política.".into(),
                ))
            }
        },
        t => t,
    };
    let body = obj(vec![(
        "policy",
        Some(obj(vec![
            ("description", Some(s(v.text("description")))),
            ("default_state", Some(s(default_state))),
            ("states", Some(states)),
            ("ism_template", template),
            (
                "error_notification",
                v.json("error_notification", "Aviso de errores", Shape::Object)?,
            ),
        ])),
    )]);
    Ok(Policy { id, body })
}

pub(crate) fn ism_plan(c: &Ctx, cur: Option<&Cur>, values: &FormValues) -> Result<Plan> {
    let p = c.plugins;
    let want = ism_spec(&values.values, c.opendistro)?;
    let put = |id: &str, guard: &str| {
        Req::new(
            "PUT",
            format!("/{p}/_ism/policies/{}{guard}", path_segment(id)),
            Some(want.body.clone()),
        )
    };
    let Some(cur) = cur else {
        return Ok(Plan {
            notes: vec![format!("Política ISM «{}»", want.id)],
            reqs: vec![put(&want.id, "")],
            id: Some(want.id.clone()),
            name: want.id.clone(),
            ..Plan::default()
        });
    };
    let have = ism_spec(&ism_values(cur), c.opendistro)?;
    let mut plan = Plan {
        notes: vec![format!("Política ISM «{}»", cur.id)],
        id: Some(want.id.clone()),
        name: cur.id.clone(),
        ..Plan::default()
    };
    if want.id != cur.id {
        plan.notes.push(format!(
            "Un id nuevo crea la política «{}» y borra «{}»; los índices que esta administra no pasan solos a la nueva.",
            want.id, cur.id
        ));
        let delete = Req::new(
            "DELETE",
            format!("/{p}/_ism/policies/{}", path_segment(&cur.id)),
            None,
        );
        plan.reqs.push(put(&want.id, ""));
        if want.body.at(&["policy", "ism_template"]).is_some() {
            // ISM refuses two templates with the same patterns and
            // priority, so the template can't go on the new policy while
            // the old one lives. The new policy is created first without
            // it (the cluster checks its states before anything is
            // deleted), the old one deleted, and then the template added
            // to the new one: there is never a moment without the policy.
            plan.notes.push(
                "Con plantilla ISM: se crea la política nueva sin la plantilla, se borra la vieja y después se le agrega la plantilla a la nueva (ISM no deja dos plantillas con los mismos patrones y la misma prioridad).".into(),
            );
            plan.reqs[0].body = Some(obj(vec![(
                "policy",
                want.body
                    .get("policy")
                    .map(|p| jobs::without(p, &["ism_template"])),
            )]));
            plan.reqs.push(delete);
            plan.reqs.push(
                put(&want.id, "")
                    .guard_from(0)
                    .note("Con if_seq_no / if_primary_term de la respuesta del primer PUT (DBine los agrega al guardar).")
                    .if_fails(format!(
                        "La política ISM «{}» quedó creada sin su plantilla y «{}» ya se borró.",
                        want.id, cur.id
                    )),
            );
        } else {
            plan.reqs.push(delete);
        }
    } else if !same(&want.body, &have.body) {
        plan.notes.push(ISM_NOTE.into());
        plan.reqs.push(put(
            &cur.id,
            &format!("?if_seq_no={}&if_primary_term={}", cur.seq, cur.term),
        ));
    }
    Ok(plan)
}

// ---------------------------------------------------------------------------
// ILM
// ---------------------------------------------------------------------------

pub(crate) fn ilm_sections() -> Vec<FormSection> {
    vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("id", "Nombre", FormKind::Text)
                .required()
                .placeholder("logs-30-dias")
                .help("Cambiarlo crea una política nueva y borra la anterior."),
            FormField::new("meta", "Metadatos (_meta)", FormKind::code("json"))
                .placeholder("{ \"description\": \"…\" }"),
            FormField::new("in_use", "En uso por", FormKind::Textarea)
                .read_only(true)
                .help("Índices, data streams y plantillas que usan la política."),
        ]),
        FormSection::new("phases", "Fases").fields(vec![FormField::new(
            "phases",
            "Fases",
            FormKind::code("json"),
        )
        .required()
        .default(json!("{\n  \"hot\": {\n    \"actions\": {\n      \"rollover\": { \"max_primary_shard_size\": \"50gb\", \"max_age\": \"30d\" }\n    }\n  },\n  \"delete\": {\n    \"min_age\": \"90d\",\n    \"actions\": { \"delete\": {} }\n  }\n}"))
        .help("hot, warm, cold, frozen y delete, cada una con min_age y sus acciones.")]),
    ]
}

pub(crate) fn ilm_values(cur: &Cur) -> BTreeMap<String, Value> {
    let p = &cur.policy;
    let mut v = BTreeMap::new();
    v.insert("id".into(), json!(cur.id));
    v.insert("meta".into(), json_text(p.get("_meta")));
    v.insert("in_use".into(), json!(cur.in_use.join("\n")));
    v.insert(
        "phases".into(),
        json_text(Some(p.get("phases").unwrap_or(&J::Obj(Vec::new())))),
    );
    v
}

pub(crate) fn ilm_spec(values: &BTreeMap<String, Value>) -> Result<Policy> {
    let v = Vals(values);
    let id = plain_id(v.opt("id"), "Nombre")?;
    let phases = v
        .json("phases", "Fases", Shape::Object)?
        .ok_or_else(|| bad("Faltan las fases de la política.".into()))?;
    let body = obj(vec![(
        "policy",
        Some(obj(vec![
            ("phases", Some(phases)),
            ("_meta", v.json("meta", "Metadatos (_meta)", Shape::Object)?),
        ])),
    )]);
    Ok(Policy { id, body })
}

pub(crate) fn ilm_plan(cur: Option<&Cur>, values: &FormValues) -> Result<Plan> {
    let want = ilm_spec(&values.values)?;
    let put = |id: &str| {
        Req::new(
            "PUT",
            format!("/_ilm/policy/{}", path_segment(id)),
            Some(want.body.clone()),
        )
    };
    let Some(cur) = cur else {
        return Ok(Plan {
            notes: vec![format!("Política ILM «{}»", want.id)],
            reqs: vec![put(&want.id)],
            id: Some(want.id.clone()),
            name: want.id.clone(),
            ..Plan::default()
        });
    };
    if cur.managed() {
        return Err(bad(ILM_MANAGED.into()));
    }
    let have = ilm_spec(&ilm_values(cur))?;
    let mut plan = Plan {
        notes: vec![format!("Política ILM «{}»", cur.id)],
        id: Some(want.id.clone()),
        name: cur.id.clone(),
        ..Plan::default()
    };
    if want.id != cur.id {
        plan.notes.push(format!(
            "Un nombre nuevo crea la política «{}» y borra «{}»: el clúster no la borra mientras la usen índices o plantillas.",
            want.id, cur.id
        ));
        plan.reqs.push(put(&want.id));
        plan.reqs.push(Req::new(
            "DELETE",
            format!("/_ilm/policy/{}", path_segment(&cur.id)),
            None,
        ));
    } else if !same(&want.body, &have.body) {
        plan.notes.push(ILM_NOTE.into());
        plan.reqs.push(put(&cur.id));
    }
    Ok(plan)
}

pub(crate) fn delete_plan(c: &Ctx, cur: &Cur) -> Result<Plan> {
    if !c.os && cur.managed() {
        return Err(bad(ILM_MANAGED.into()));
    }
    let (what, path) = if c.os {
        (
            "ISM",
            format!("/{}/_ism/policies/{}", c.plugins, path_segment(&cur.id)),
        )
    } else {
        ("ILM", format!("/_ilm/policy/{}", path_segment(&cur.id)))
    };
    let note = if c.os {
        format!("Borra la política {what} «{}».", cur.id)
    } else {
        format!(
            "Borra la política {what} «{}»; el clúster no la borra mientras la usen índices o plantillas.",
            cur.id
        )
    };
    Ok(Plan {
        notes: vec![note],
        reqs: vec![Req::new("DELETE", path, None)],
        name: cur.id.clone(),
        ..Plan::default()
    })
}
