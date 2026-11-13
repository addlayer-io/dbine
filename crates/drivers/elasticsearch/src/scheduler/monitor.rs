//! OpenSearch / Open Distro Alerting monitors: the per query, per bucket,
//! per document and cluster metrics monitors of OpenSearch Dashboards'
//! "Create monitor" page, with the schedule as fields and the inputs and
//! the triggers (with their actions, which name a notification channel or
//! an Open Distro destination by id) as JSON. `POST _alerting/monitors`
//! creates one (the cluster gives its id); `PUT` with `if_seq_no` /
//! `if_primary_term` changes it in place, sending back what the form
//! doesn't show (its data sources, owner, Dashboards' visual editor
//! state). Monitors another plugin manages (Security Analytics' detectors)
//! are read only: the REST update refuses them.

use super::{bad, json_text, obj, s, same, Choices, Ctx, Plan, Req, Shape, Vals};
use crate::ddl::path_segment;
use crate::jobs::{self, num, text};
use crate::json::J;
use crate::EsSession;
use dbine_driver::{FormField, FormKind, FormSection, FormValues, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

/// A monitor as the cluster has it.
#[derive(Debug, Clone)]
pub(crate) struct Cur {
    pub(crate) id: String,
    pub(crate) monitor: J,
    pub(crate) seq: String,
    pub(crate) term: String,
}

pub(crate) async fn read(s: &EsSession, id: &str) -> Result<Cur> {
    let (monitor, seq, term) = s.os_monitor(id).await?;
    Ok(Cur {
        id: id.to_string(),
        monitor,
        seq,
        term,
    })
}

const DEFAULT_INPUTS: &str = "[\n  {\n    \"search\": {\n      \"indices\": [\"logs-*\"],\n      \"query\": {\n        \"size\": 0,\n        \"query\": { \"match_all\": {} }\n      }\n    }\n  }\n]";
const BUCKET_INPUTS: &str = "[\n  {\n    \"search\": {\n      \"indices\": [\"logs-*\"],\n      \"query\": {\n        \"size\": 0,\n        \"query\": { \"match_all\": {} },\n        \"aggregations\": {\n          \"por_host\": {\n            \"composite\": {\n              \"sources\": [{ \"host\": { \"terms\": { \"field\": \"host.keyword\" } } }]\n            }\n          }\n        }\n      }\n    }\n  }\n]";
const DOC_INPUTS: &str = "[\n  {\n    \"doc_level_input\": {\n      \"description\": \"\",\n      \"indices\": [\"logs-*\"],\n      \"queries\": [\n        { \"id\": \"errores\", \"name\": \"errores\", \"query\": \"status:500\", \"tags\": [] }\n      ]\n    }\n  }\n]";
const CLUSTER_INPUTS: &str = "[\n  {\n    \"uri\": {\n      \"api_type\": \"CLUSTER_HEALTH\",\n      \"path\": \"_cluster/health\",\n      \"path_params\": \"\",\n      \"url\": \"http://localhost:9200/_cluster/health\"\n    }\n  }\n]";

/// Each type of monitor's inputs and triggers fields: the type, the key
/// suffix of its fields (`inputs_query`…), its default inputs, and the
/// key of its triggers (a cluster metrics monitor's triggers are per query,
/// like a query monitor's).
const SHAPES: [(&str, &str, &str, &str); 4] = [
    ("query_level_monitor", "query", DEFAULT_INPUTS, "query"),
    ("bucket_level_monitor", "bucket", BUCKET_INPUTS, "bucket"),
    ("doc_level_monitor", "doc", DOC_INPUTS, "doc"),
    (
        "cluster_metrics_monitor",
        "cluster",
        CLUSTER_INPUTS,
        "query",
    ),
];

/// The keys of a monitor type's inputs and triggers fields (Open Distro:
/// `inputs` and `triggers`, its monitors have no type).
fn keys_of(c: &Ctx, monitor_type: &str) -> (String, String) {
    if c.opendistro {
        return ("inputs".into(), "triggers".into());
    }
    let (_, i, _, t) = SHAPES
        .iter()
        .find(|(ty, ..)| *ty == monitor_type)
        .unwrap_or(&SHAPES[0]);
    (format!("inputs_{i}"), format!("triggers_{t}"))
}

/// The inputs and triggers fields of each type of monitor, each shown only
/// for its type. The labels and help are literals, for the message
/// extractor.
fn typed_fields() -> (Vec<FormField>, Vec<FormField>) {
    let inputs = vec![
        FormField::new("inputs_query", "Entradas", FormKind::code("json"))
            .required()
            .when("monitor_type", &["query_level_monitor"])
            .default(json!(DEFAULT_INPUTS))
            .help("Una entrada search: los índices y la consulta (query DSL) que corre en cada ejecución; los disparadores leen su respuesta en ctx.results[0]."),
        FormField::new("inputs_bucket", "Entradas", FormKind::code("json"))
            .required()
            .when("monitor_type", &["bucket_level_monitor"])
            .default(json!(BUCKET_INPUTS))
            .help("Una entrada search cuya consulta agrupa con una agregación composite: cada bucket se evalúa por separado."),
        FormField::new("inputs_doc", "Entradas", FormKind::code("json"))
            .required()
            .when("monitor_type", &["doc_level_monitor"])
            .default(json!(DOC_INPUTS))
            .help("Una entrada doc_level_input: los índices y las consultas por documento (id, name, query en sintaxis de query string, tags)."),
        FormField::new("inputs_cluster", "Entradas", FormKind::code("json"))
            .required()
            .when("monitor_type", &["cluster_metrics_monitor"])
            .default(json!(CLUSTER_INPUTS))
            .help("Una entrada uri: la API del clúster que se consulta (api_type, path y path_params, como CLUSTER_HEALTH, CLUSTER_STATS o NODES_STATS)."),
    ];
    let triggers = vec![
        FormField::new("triggers_query", "Disparadores", FormKind::code("json"))
            .when("monitor_type", &["query_level_monitor", "cluster_metrics_monitor"])
            .default(json!("[]"))
            .placeholder("[{ \"query_level_trigger\": { \"name\": \"hay errores\", \"severity\": \"1\", \"condition\": { \"script\": { \"source\": \"ctx.results[0].hits.total.value > 0\", \"lang\": \"painless\" } }, \"actions\": [] } }]")
            .help("Lista de query_level_trigger: name, severity (1 a 5), condition (un script sobre ctx.results) y actions (cada una con destination_id = el id de un canal, message_template y throttle)."),
        FormField::new("triggers_bucket", "Disparadores", FormKind::code("json"))
            .when("monitor_type", &["bucket_level_monitor"])
            .default(json!("[]"))
            .placeholder("[{ \"bucket_level_trigger\": { \"name\": \"muchos\", \"severity\": \"1\", \"condition\": { \"buckets_path\": { \"_count\": \"_count\" }, \"parent_bucket_path\": \"por_host\", \"script\": { \"source\": \"params._count > 50\" } }, \"actions\": [] } }]")
            .help("Lista de bucket_level_trigger: name, severity (1 a 5), condition (buckets_path, parent_bucket_path = la agregación composite, y un script por bucket) y actions."),
        FormField::new("triggers_doc", "Disparadores", FormKind::code("json"))
            .when("monitor_type", &["doc_level_monitor"])
            .default(json!("[]"))
            .placeholder("[{ \"document_level_trigger\": { \"name\": \"errores\", \"severity\": \"1\", \"condition\": { \"script\": { \"source\": \"query[name=errores]\", \"lang\": \"painless\" } }, \"actions\": [] } }]")
            .help("Lista de document_level_trigger: name, severity (1 a 5), condition (un script que nombra las consultas: query[name=…], query[tag=…]) y actions."),
    ];
    (inputs, triggers)
}

pub(crate) fn sections(c: &Ctx, new: bool, choices: &Choices) -> Vec<FormSection> {
    let mut general = vec![FormField::new("name", "Nombre", FormKind::Text)
        .required()
        .placeholder("errores-5xx")];
    if !c.opendistro {
        general.push(
            FormField::new(
                "monitor_type",
                "Tipo de monitor",
                FormKind::select(&[
                    ("query_level_monitor", "Por consulta"),
                    ("bucket_level_monitor", "Por buckets"),
                    ("doc_level_monitor", "Por documentos"),
                    ("cluster_metrics_monitor", "De métricas del clúster"),
                ]),
            )
            .required()
            .read_only(!new)
            .default(json!("query_level_monitor"))
            .help("Decide la forma de las entradas y de los disparadores."),
        );
    }
    general.push(
        FormField::new("enabled", "Habilitado", FormKind::Bool)
            .default(json!(true))
            .help("Un monitor deshabilitado no se ejecuta según su programación."),
    );
    let channels: Vec<String> = choices
        .get("channel")
        .iter()
        .filter(|(id, _)| !id.is_empty())
        .map(|(id, name)| format!("{id}: {name}"))
        .collect();
    let (inputs, mut triggers) = if c.opendistro {
        (
            vec![FormField::new("inputs", "Entradas", FormKind::code("json"))
                .required()
                .default(json!(DEFAULT_INPUTS))
                .help("Lista de entradas: search (índices y consulta) o http.")],
            vec![FormField::new("triggers", "Disparadores", FormKind::code("json"))
                .default(json!("[]"))
                .help("Lista de disparadores: name, severity (1 a 5), condition (un script) y actions (cada una con destination_id, message_template y throttle).")],
        )
    } else {
        typed_fields()
    };
    if !channels.is_empty() {
        triggers.push(
            FormField::new("channels", "Canales disponibles", FormKind::Textarea)
                .read_only(true)
                .help("Los ids que las acciones usan en destination_id."),
        );
    }
    vec![
        FormSection::new("general", "General").fields(general),
        FormSection::new("schedule", "Programación").fields(vec![
            FormField::new(
                "m_schedule_kind",
                "Programación",
                FormKind::select(&[("period", "Cada cierto tiempo"), ("cron", "Expresión cron")]),
            )
            .required()
            .default(json!("period")),
            FormField::new("m_interval", "Cada", FormKind::number(Some(1.0), None))
                .required()
                .when("m_schedule_kind", &["period"])
                .default(json!(1)),
            FormField::new(
                "m_interval_unit",
                "Unidad",
                FormKind::select(&[("minutes", "Minutos"), ("hours", "Horas"), ("days", "Días")]),
            )
            .required()
            .when("m_schedule_kind", &["period"])
            .default(json!("minutes")),
            FormField::new("m_cron", "Expresión cron", FormKind::Cron)
                .required()
                .when("m_schedule_kind", &["cron"])
                .placeholder("0 */6 * * *")
                .help("Cron de cinco campos: minutos horas día-del-mes mes día-de-la-semana."),
            FormField::new("m_timezone", "Zona horaria", FormKind::Text)
                .when("m_schedule_kind", &["cron"])
                .default(json!("UTC")),
        ]),
        FormSection::new("inputs", "Entradas").fields(inputs),
        FormSection::new("triggers", "Disparadores").fields(triggers),
    ]
}

pub(crate) fn values_of(c: &Ctx, cur: &Cur) -> BTreeMap<String, Value> {
    let m = &cur.monitor;
    let mut v = BTreeMap::new();
    v.insert(
        "name".into(),
        json!(text(m.get("name")).unwrap_or_default()),
    );
    let monitor_type = text(m.get("monitor_type")).unwrap_or_else(|| TYPES[0].into());
    let (inputs_key, triggers_key) = keys_of(c, &monitor_type);
    v.insert("monitor_type".into(), json!(monitor_type));
    v.insert(
        "enabled".into(),
        json!(m.get("enabled").and_then(J::as_bool).unwrap_or(false)),
    );
    let sch = m.get("schedule");
    if let Some(c) = sch.and_then(|x| x.get("cron")) {
        v.insert("m_schedule_kind".into(), json!("cron"));
        v.insert(
            "m_cron".into(),
            json!(text(c.get("expression")).unwrap_or_default()),
        );
        v.insert(
            "m_timezone".into(),
            json!(text(c.get("timezone")).unwrap_or_default()),
        );
    } else {
        let p = sch.and_then(|x| x.get("period"));
        v.insert("m_schedule_kind".into(), json!("period"));
        v.insert(
            "m_interval".into(),
            json!(num(p.and_then(|p| p.get("interval"))).unwrap_or(1.0) as i64),
        );
        v.insert(
            "m_interval_unit".into(),
            json!(super::os_transform::unit_of(p.and_then(|p| p.get("unit")))),
        );
    }
    v.insert(
        inputs_key,
        json_text(Some(m.get("inputs").unwrap_or(&J::Arr(Vec::new())))),
    );
    v.insert(
        triggers_key,
        json_text(Some(m.get("triggers").unwrap_or(&J::Arr(Vec::new())))),
    );
    v
}

/// The channels as the read-only field's text.
pub(crate) fn channels_text(choices: &Choices) -> Value {
    json!(choices
        .get("channel")
        .iter()
        .filter(|(id, _)| !id.is_empty())
        .map(|(id, name)| format!("{id}: {name}"))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// What the form edits of a monitor.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Monitor {
    pub(crate) name: String,
    /// `None` on Open Distro.
    pub(crate) monitor_type: Option<String>,
    pub(crate) enabled: bool,
    pub(crate) schedule: J,
    pub(crate) inputs: J,
    pub(crate) triggers: J,
}

const TYPES: [&str; 4] = [
    "query_level_monitor",
    "bucket_level_monitor",
    "doc_level_monitor",
    "cluster_metrics_monitor",
];

pub(crate) fn spec(c: &Ctx, values: &BTreeMap<String, Value>) -> Result<Monitor> {
    let v = Vals(values);
    let name = v
        .line("name", "Nombre")?
        .ok_or_else(|| bad("Falta el nombre del monitor.".into()))?;
    let monitor_type = if c.opendistro {
        None
    } else {
        let t = v.opt("monitor_type").unwrap_or_else(|| TYPES[0].into());
        if !TYPES.contains(&t.as_str()) {
            return Err(bad(format!(
                "«{}» no es un tipo de monitor.",
                super::shown(&t)
            )));
        }
        Some(t)
    };
    let schedule = match v.text("m_schedule_kind").as_str() {
        "period" | "" => {
            let n = v
                .int("m_interval", "Cada", 1, i64::from(i32::MAX))?
                .ok_or_else(|| bad("Falta el intervalo del monitor.".into()))?;
            let unit = match v.text("m_interval_unit").as_str() {
                "minutes" => "MINUTES",
                "hours" => "HOURS",
                "days" => "DAYS",
                _ => return Err(bad("«Unidad» tiene que ser minutos, horas o días.".into())),
            };
            obj(vec![(
                "period",
                Some(obj(vec![
                    ("interval", Some(J::Num(n.into()))),
                    ("unit", Some(s(unit))),
                ])),
            )])
        }
        "cron" => super::os_transform::cron_schedule(v, "m_cron", "m_timezone")?,
        other => {
            return Err(bad(format!(
                "«{}» no es un tipo de programación.",
                super::shown(other)
            )))
        }
    };
    // The fields of the monitor's type; `inputs` / `triggers` when they
    // aren't there at all (a caller that doesn't know the form's keys).
    let (inputs_key, triggers_key) = keys_of(c, monitor_type.as_deref().unwrap_or(TYPES[0]));
    let present = |k: String, generic: &str| {
        if values.contains_key(&k) {
            k
        } else {
            generic.to_string()
        }
    };
    let inputs = v
        .json(&present(inputs_key, "inputs"), "Entradas", Shape::Array)?
        .ok_or_else(|| bad("Faltan las entradas del monitor.".into()))?;
    let triggers = v
        .json(
            &present(triggers_key, "triggers"),
            "Disparadores",
            Shape::Array,
        )?
        .unwrap_or(J::Arr(Vec::new()));
    Ok(Monitor {
        name,
        monitor_type,
        enabled: v.bool_or("enabled", true),
        schedule,
        inputs,
        triggers,
    })
}

/// The request's body: the form's part over `base` (what the cluster has,
/// without what it sets itself; nothing for a new monitor).
fn body(m: &Monitor, base: Option<&J>) -> J {
    let mut out: Vec<(String, J)> = match base {
        Some(b) => jobs::without(
            b,
            &["schema_version", "last_update_time", "enabled_time", "user"],
        )
        .as_obj()
        .cloned()
        .unwrap_or_default(),
        None => vec![("type".into(), s("monitor"))],
    };
    let mut set = |k: &str, v: J| match out.iter_mut().find(|(x, _)| x == k) {
        Some((_, slot)) => *slot = v,
        None => out.push((k.into(), v)),
    };
    if let Some(t) = &m.monitor_type {
        set("monitor_type", s(t));
    }
    set("name", s(&m.name));
    set("enabled", J::Bool(m.enabled));
    set("schedule", m.schedule.clone());
    set("inputs", m.inputs.clone());
    set("triggers", m.triggers.clone());
    J::Obj(out)
}

pub(crate) fn plan(c: &Ctx, cur: Option<&Cur>, values: &FormValues) -> Result<Plan> {
    let p = c.plugins;
    let want = spec(c, &values.values)?;
    let Some(cur) = cur else {
        return Ok(Plan {
            notes: vec![format!("Monitor «{}»", want.name)],
            reqs: vec![Req::new(
                "POST",
                format!("/{p}/_alerting/monitors"),
                Some(body(&want, None)),
            )],
            id_from: Some(0),
            name: want.name.clone(),
            ..Plan::default()
        });
    };
    if let Some(owner) = jobs::os_monitor_owner(&cur.monitor) {
        return Err(bad(format!(
            "este monitor lo administra {owner}: el plugin Alerting no deja cambiarlo desde aquí"
        )));
    }
    let have = spec(c, &values_of(c, cur))?;
    let mut plan = Plan {
        notes: vec![format!("Monitor «{}»", have.name)],
        id: Some(super::Type::Monitor.job_id(&cur.id)),
        name: have.name.clone(),
        ..Plan::default()
    };
    if !same(&body(&want, None), &body(&have, None)) {
        let mut b = body(&want, Some(&cur.monitor));
        // Dashboards rebuilds a visually defined monitor from its editor's
        // state: with other inputs or triggers that state would be stale.
        let redefined = !same(&want.inputs, &have.inputs) || !same(&want.triggers, &have.triggers);
        if redefined && cur.monitor.get("ui_metadata").is_some() {
            b = jobs::without(&b, &["ui_metadata"]);
            plan.notes.push(UI_METADATA.into());
        }
        plan.reqs.push(Req::new(
            "PUT",
            format!(
                "/{p}/_alerting/monitors/{}?if_seq_no={}&if_primary_term={}",
                path_segment(&cur.id),
                cur.seq,
                cur.term
            ),
            Some(b),
        ));
    }
    Ok(plan)
}

const UI_METADATA: &str = "Cambian las entradas o los disparadores: se quita el estado del editor visual de OpenSearch Dashboards (ui_metadata), que desde ahora muestra el monitor como definido por consulta.";

/// Deleting a monitor another plugin manages (a Security Analytics
/// detector's) would pull it from under that plugin: refused, as its edit.
pub(crate) fn delete_plan(p: &str, cur: &Cur) -> Result<Plan> {
    if let Some(owner) = jobs::os_monitor_owner(&cur.monitor) {
        return Err(bad(format!(
            "este monitor lo administra {owner}: se borra desde ese plugin, no desde aquí"
        )));
    }
    let name = text(cur.monitor.get("name")).unwrap_or_else(|| cur.id.clone());
    Ok(Plan {
        notes: vec![format!(
            "Borra el monitor «{name}» ({}); sus alertas pasan a estado DELETED.",
            cur.id
        )],
        reqs: vec![Req::new(
            "DELETE",
            format!("/{p}/_alerting/monitors/{}", path_segment(&cur.id)),
            None,
        )],
        name,
        ..Plan::default()
    })
}
