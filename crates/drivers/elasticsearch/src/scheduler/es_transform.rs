//! Elasticsearch transforms: Kibana's transform wizard and its edit flyout
//! as a form. `PUT _transform/<id>` creates one, `_update` changes in place
//! what the API lets change (description, source, destination, frequency,
//! the continuous sync's field and delay, retention, settings, `_meta`),
//! `_start` / `_stop` follow "Iniciado". The id, the pivot or latest, and
//! turning a batch transform into a continuous one (or back) can't change
//! in place: the transform is deleted and created again (`_preview` checks
//! the new one first), which loses its checkpoints and stats; the
//! destination index stays.

use super::{
    bad, json_text, list_text, obj, s, same, same_opt, shown, split_list, strs, Plan, Req, Shape,
    Vals,
};
use crate::ddl::path_segment;
use crate::jobs::{self, text};
use crate::json::J;
use crate::EsSession;
use dbine_driver::{FormField, FormKind, FormSection, FormValues, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(crate) const RECREATE_NOTE: &str = "Elasticsearch no deja cambiar en el lugar el id de un transform, su pivot o su latest, ni convertir uno por lotes en continuo (o al revés): para eso DBine lo borra y lo vuelve a crear, y se pierden sus checkpoints y estadísticas (el índice de destino se conserva).";

/// The settings (`settings.*`): key, the server's default.
const SETTINGS: &[(&str, Setting)] = &[
    ("max_page_search_size", Setting::Int(500, 10, 65_536)),
    ("docs_per_second", Setting::Float),
    ("num_failure_retries", Setting::Int(-2, -1, 100)),
    ("align_checkpoints", Setting::Bool(true)),
    ("dates_as_epoch_millis", Setting::Bool(false)),
    ("deduce_mappings", Setting::Bool(true)),
    ("unattended", Setting::Bool(false)),
];

#[derive(Debug, Clone, Copy)]
enum Setting {
    /// Default (`-2`: the cluster's), min, max.
    Int(i64, i64, i64),
    Float,
    Bool(bool),
}

/// A transform as the cluster has it.
#[derive(Debug, Clone)]
pub(crate) struct Cur {
    pub(crate) id: String,
    pub(crate) def: J,
    /// `stopped`, `started`, `indexing`, `failed`…
    pub(crate) state: String,
    pub(crate) managed: bool,
}

impl Cur {
    fn started(&self) -> bool {
        !self.state.is_empty() && self.state != "stopped"
    }
}

pub(crate) async fn read(s: &EsSession, id: &str) -> Result<Cur> {
    if !jobs::es_transform_id(id) {
        return Err(bad(format!(
            "«{}» no es un id de transform de Elasticsearch",
            shown(id)
        )));
    }
    let (def, stats) = s.es_transform(id).await?;
    Ok(Cur {
        id: id.to_string(),
        managed: def.at(&["_meta", "managed"]).and_then(J::as_bool) == Some(true),
        def,
        state: stats
            .as_ref()
            .and_then(|s| text(s.get("state")))
            .unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

pub(crate) fn sections(new: bool) -> Vec<FormSection> {
    let mut general = vec![
        FormField::new("id", "Id", FormKind::Text)
            .required()
            .placeholder("ventas-por-cliente")
            .help("Minúsculas, números, - y _, empezando y terminando con una letra o un número (hasta 64). Cambiarlo vuelve a crear el transform."),
        FormField::new("description", "Descripción", FormKind::Textarea),
        FormField::new("enabled", "Iniciado", FormKind::Bool)
            .default(json!(true))
            .help("Un transform iniciado procesa los datos; uno por lotes se detiene solo al terminar."),
        FormField::new("meta", "Metadatos (_meta)", FormKind::code("json"))
            .placeholder("{ \"equipo\": \"ventas\" }"),
    ];
    if new {
        general.push(
            FormField::new("defer_validation", "No validar al guardar", FormKind::Bool)
                .help("Crea el transform aunque el índice de origen todavía no exista o el usuario no pueda leerlo (defer_validation)."),
        );
    }
    let mut settings = vec![
        FormField::new(
            "max_page_search_size",
            "Tamaño de página de búsqueda",
            FormKind::number(Some(10.0), Some(65_536.0)),
        )
        .default(json!(500))
        .help("Buckets compuestos por búsqueda (max_page_search_size)."),
        FormField::new(
            "docs_per_second",
            "Documentos por segundo",
            FormKind::number(Some(0.0), None),
        )
        .help("Límite de lectura (docs_per_second); vacío: sin límite."),
        FormField::new(
            "num_failure_retries",
            "Reintentos ante fallos",
            FormKind::number(Some(-1.0), Some(100.0)),
        )
        .help("Vacío: el valor del clúster; -1: reintentar siempre."),
        FormField::new(
            "align_checkpoints",
            "Alinear los checkpoints",
            FormKind::Bool,
        )
        .default(json!(true)),
        FormField::new(
            "dates_as_epoch_millis",
            "Fechas como milisegundos",
            FormKind::Bool,
        ),
        FormField::new("deduce_mappings", "Deducir los mappings", FormKind::Bool)
            .default(json!(true)),
        FormField::new("unattended", "Desatendido", FormKind::Bool)
            .help("No falla por errores que se puedan reintentar: reintenta para siempre."),
    ];
    if !new {
        settings.push(
            FormField::new("defer_validation", "No validar al guardar", FormKind::Bool).help(
                "Guarda los cambios sin comprobar el origen y el destino (defer_validation).",
            ),
        );
    }
    vec![
        FormSection::new("general", "General").fields(general),
        FormSection::new("source", "Origen").fields(vec![
            FormField::new("source_index", "Índices de origen", FormKind::Text)
                .required()
                .placeholder("ventas-*, clientes")
                .help("Índices, alias o patrones, separados por comas (cluster:índice para uno remoto)."),
            FormField::new("source_query", "Consulta", FormKind::code("json"))
                .placeholder("{ \"term\": { \"pais\": \"AR\" } }")
                .help("Query DSL que filtra los documentos de origen; vacío: todos."),
            FormField::new("runtime_mappings", "Campos de runtime", FormKind::code("json"))
                .placeholder("{ \"dia\": { \"type\": \"keyword\", \"script\": \"…\" } }"),
        ]),
        FormSection::new("pivot", "Transformación").fields(vec![
            FormField::new(
                "function",
                "Función",
                FormKind::select(&[
                    ("pivot", "Pivot: agrupar y agregar"),
                    ("latest", "Latest: el documento más reciente"),
                ]),
            )
            .required()
            .default(json!("pivot")),
            FormField::new("group_by", "Agrupar por (group_by)", FormKind::code("json"))
                .required()
                .when("function", &["pivot"])
                .placeholder("{ \"cliente\": { \"terms\": { \"field\": \"cliente\" } } }"),
            FormField::new("aggregations", "Agregaciones", FormKind::code("json"))
                .required()
                .when("function", &["pivot"])
                .placeholder("{ \"total\": { \"sum\": { \"field\": \"importe\" } } }"),
            FormField::new("unique_key", "Clave única", FormKind::Text)
                .required()
                .when("function", &["latest"])
                .placeholder("cliente, producto")
                .help("Campos que identifican la entidad, separados por comas."),
            FormField::new("sort", "Campo de orden", FormKind::Text)
                .required()
                .when("function", &["latest"])
                .placeholder("@timestamp")
                .help("Campo de fecha que decide cuál es el documento más reciente."),
        ]),
        FormSection::new("dest", "Destino").fields(vec![
            FormField::new("dest_index", "Índice de destino", FormKind::Text)
                .required()
                .placeholder("ventas-por-cliente"),
            FormField::new(
                "dest_pipeline",
                "Pipeline de ingesta",
                FormKind::reference("pipeline", false),
            ),
            FormField::new("dest_aliases", "Alias del destino", FormKind::code("json"))
                .placeholder("[{ \"alias\": \"ventas-actual\", \"move_on_creation\": true }]")
                .help("Lista de { alias, move_on_creation } (Elasticsearch 8.8 o posterior)."),
        ]),
        FormSection::new("schedule", "Programación").fields(vec![
            FormField::new("frequency", "Frecuencia", FormKind::Text)
                .placeholder("1m")
                .help("Cada cuánto busca cambios un transform continuo (de 1s a 1h); vacío: 1m."),
            FormField::new("continuous", "Continuo", FormKind::Bool)
                .help("Sigue procesando los documentos nuevos; uno por lotes procesa una vez y se detiene."),
            FormField::new("sync_field", "Campo de tiempo", FormKind::Text)
                .required()
                .when("continuous", &["true"])
                .placeholder("@timestamp")
                .help("El campo con el que el transform reconoce los documentos nuevos."),
            FormField::new("sync_delay", "Demora", FormKind::Text)
                .when("continuous", &["true"])
                .placeholder("60s")
                .help("Lo que espera a los documentos que llegan tarde; vacío: 60s."),
        ]),
        FormSection::new("retention", "Retención").fields(vec![
            FormField::new("retention_field", "Campo de tiempo", FormKind::Text)
                .placeholder("@timestamp")
                .help("Con un campo y una edad, el transform borra del destino los documentos más viejos."),
            FormField::new("retention_max_age", "Edad máxima", FormKind::Text)
                .placeholder("30d"),
        ]),
        FormSection::new("settings", "Ajustes").fields(settings),
    ]
}

/// The transform as the form's values.
pub(crate) fn values_of(cur: &Cur) -> BTreeMap<String, Value> {
    let t = &cur.def;
    let mut v = BTreeMap::new();
    v.insert("id".into(), json!(cur.id));
    v.insert(
        "description".into(),
        json!(text(t.get("description")).unwrap_or_default()),
    );
    v.insert("enabled".into(), json!(cur.started()));
    v.insert("meta".into(), json_text(t.get("_meta")));
    v.insert(
        "source_index".into(),
        json!(list_text(t.at(&["source", "index"]))),
    );
    v.insert("source_query".into(), json_text(t.at(&["source", "query"])));
    v.insert(
        "runtime_mappings".into(),
        json_text(t.at(&["source", "runtime_mappings"])),
    );
    let latest = t.get("latest");
    v.insert(
        "function".into(),
        json!(if latest.is_some() { "latest" } else { "pivot" }),
    );
    v.insert("group_by".into(), json_text(t.at(&["pivot", "group_by"])));
    v.insert(
        "aggregations".into(),
        json_text(
            t.at(&["pivot", "aggregations"])
                .or_else(|| t.at(&["pivot", "aggs"])),
        ),
    );
    v.insert(
        "unique_key".into(),
        json!(list_text(latest.and_then(|l| l.get("unique_key")))),
    );
    v.insert(
        "sort".into(),
        json!(text(latest.and_then(|l| l.get("sort"))).unwrap_or_default()),
    );
    v.insert(
        "dest_index".into(),
        json!(text(t.at(&["dest", "index"])).unwrap_or_default()),
    );
    v.insert(
        "dest_pipeline".into(),
        json!(text(t.at(&["dest", "pipeline"])).unwrap_or_default()),
    );
    v.insert("dest_aliases".into(), json_text(t.at(&["dest", "aliases"])));
    v.insert(
        "frequency".into(),
        json!(text(t.get("frequency")).unwrap_or_default()),
    );
    let sync = t.at(&["sync", "time"]);
    v.insert("continuous".into(), json!(t.get("sync").is_some()));
    v.insert(
        "sync_field".into(),
        json!(text(sync.and_then(|s| s.get("field"))).unwrap_or_default()),
    );
    v.insert(
        "sync_delay".into(),
        json!(text(sync.and_then(|s| s.get("delay"))).unwrap_or_default()),
    );
    let ret = t.at(&["retention_policy", "time"]);
    v.insert(
        "retention_field".into(),
        json!(text(ret.and_then(|r| r.get("field"))).unwrap_or_default()),
    );
    v.insert(
        "retention_max_age".into(),
        json!(text(ret.and_then(|r| r.get("max_age"))).unwrap_or_default()),
    );
    for (k, kind) in SETTINGS {
        let set = t.at(&["settings", k]);
        let value = match (kind, set) {
            (Setting::Bool(_), Some(J::Bool(b))) => json!(b),
            (Setting::Bool(d), _) => json!(d),
            (_, Some(J::Num(n))) => json!(n.as_f64()),
            (Setting::Int(d, _, _), _) if *d > -2 => json!(d),
            _ => json!(""),
        };
        v.insert((*k).into(), value);
    }
    v.insert("defer_validation".into(), json!(false));
    v
}

// ---------------------------------------------------------------------------
// From the form
// ---------------------------------------------------------------------------

/// A transform as the form describes it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tf {
    pub(crate) id: String,
    pub(crate) description: String,
    pub(crate) started: bool,
    pub(crate) source: J,
    pub(crate) latest: bool,
    /// The pivot or the latest.
    pub(crate) function: J,
    pub(crate) dest: J,
    pub(crate) frequency: Option<String>,
    pub(crate) sync: Option<J>,
    pub(crate) retention: Option<J>,
    /// The settings set explicitly.
    pub(crate) settings: Vec<(String, J)>,
    pub(crate) meta: Option<J>,
    pub(crate) defer: bool,
}

/// `explicit`: the settings the cluster has set on the transform (kept
/// even when they hold the default).
pub(crate) fn spec(values: &BTreeMap<String, Value>, explicit: &[String]) -> Result<Tf> {
    let v = Vals(values);
    let id = v
        .opt("id")
        .ok_or_else(|| bad("Falta el id del transform.".into()))?;
    if !jobs::es_transform_id(&id) {
        return Err(bad(format!(
            "«{}» no es un id de transform válido: minúsculas, números, - y _, empezando y terminando con una letra o un número (hasta 64).",
            shown(&id)
        )));
    }
    let indices = split_list(&v.raw("source_index"));
    if indices.is_empty() {
        return Err(bad("Falta el índice de origen del transform.".into()));
    }
    if indices.iter().any(|i| i.chars().any(char::is_control)) {
        return Err(bad(
            "Un índice de origen tiene caracteres de control.".into()
        ));
    }
    let source = obj(vec![
        ("index", Some(strs(&indices))),
        ("query", v.json("source_query", "Consulta", Shape::Object)?),
        (
            "runtime_mappings",
            v.json("runtime_mappings", "Campos de runtime", Shape::Object)?,
        ),
    ]);
    let latest = match v.text("function").as_str() {
        "pivot" | "" => false,
        "latest" => true,
        other => {
            return Err(bad(format!(
                "«{}» no es una función de transform (pivot o latest).",
                shown(other)
            )))
        }
    };
    let function = if latest {
        let keys = split_list(&v.raw("unique_key"));
        if keys.is_empty() {
            return Err(bad("Falta la clave única del latest.".into()));
        }
        let sort = v
            .line("sort", "Campo de orden")?
            .ok_or_else(|| bad("Falta el campo de orden del latest.".into()))?;
        obj(vec![
            ("unique_key", Some(strs(&keys))),
            ("sort", Some(s(sort))),
        ])
    } else {
        let group_by = v
            .json("group_by", "Agrupar por (group_by)", Shape::Object)?
            .ok_or_else(|| bad("Falta «Agrupar por (group_by)» del pivot.".into()))?;
        let aggs = v
            .json("aggregations", "Agregaciones", Shape::Object)?
            .ok_or_else(|| bad("Faltan las agregaciones del pivot.".into()))?;
        obj(vec![
            ("group_by", Some(group_by)),
            ("aggregations", Some(aggs)),
        ])
    };
    let dest_index = v
        .line("dest_index", "Índice de destino")?
        .ok_or_else(|| bad("Falta el índice de destino del transform.".into()))?;
    let dest = obj(vec![
        ("index", Some(s(dest_index))),
        (
            "pipeline",
            v.line("dest_pipeline", "Pipeline de ingesta")?.map(s),
        ),
        (
            "aliases",
            v.json("dest_aliases", "Alias del destino", Shape::Array)?,
        ),
    ]);
    let frequency = v.time("frequency", "Frecuencia")?;
    let sync = if v.bool("continuous") {
        let field = v
            .line("sync_field", "Campo de tiempo")?
            .ok_or_else(|| bad("Un transform continuo necesita su campo de tiempo.".into()))?;
        Some(obj(vec![(
            "time",
            Some(obj(vec![
                ("field", Some(s(field))),
                ("delay", v.time("sync_delay", "Demora")?.map(s)),
            ])),
        )]))
    } else {
        None
    };
    let retention = match (
        v.line("retention_field", "Campo de tiempo de la retención")?,
        v.time("retention_max_age", "Edad máxima")?,
    ) {
        (None, None) => None,
        (Some(f), Some(a)) => Some(obj(vec![(
            "time",
            Some(obj(vec![("field", Some(s(f))), ("max_age", Some(s(a)))])),
        )])),
        _ => {
            return Err(bad(
                "La retención necesita el campo de tiempo y la edad máxima.".into(),
            ))
        }
    };
    let mut settings = Vec::new();
    for (k, kind) in SETTINGS {
        let label = setting_label(k);
        let kept = explicit.iter().any(|e| e == k);
        let value = match kind {
            Setting::Bool(d) => {
                let b = v.bool_or(k, *d);
                (b != *d || kept).then_some(J::Bool(b))
            }
            Setting::Int(d, min, max) => match v.int(k, label, *min, *max)? {
                Some(n) if n != *d || kept => Some(J::Num(n.into())),
                _ => None,
            },
            Setting::Float => v.number(k, label, 0.0)?,
        };
        if let Some(value) = value {
            settings.push((k.to_string(), value));
        }
    }
    Ok(Tf {
        id,
        description: v.text("description"),
        started: v.bool_or("enabled", true),
        source,
        latest,
        function,
        dest,
        frequency,
        sync,
        retention,
        settings,
        meta: v.json("meta", "Metadatos (_meta)", Shape::Object)?,
        defer: v.bool("defer_validation"),
    })
}

fn setting_label(k: &str) -> &'static str {
    match k {
        "max_page_search_size" => "Tamaño de página de búsqueda",
        "docs_per_second" => "Documentos por segundo",
        "num_failure_retries" => "Reintentos ante fallos",
        _ => "Ajuste",
    }
}

/// The settings the cluster has set on a transform.
fn explicit(cur: &Cur) -> Vec<String> {
    cur.def
        .get("settings")
        .and_then(J::as_obj)
        .into_iter()
        .flatten()
        .map(|(k, _)| k.clone())
        .collect()
}

/// `PUT _transform/<id>`'s body.
pub(crate) fn body(t: &Tf) -> J {
    obj(vec![
        (
            "description",
            (!t.description.is_empty()).then(|| s(&t.description)),
        ),
        ("source", Some(t.source.clone())),
        ("dest", Some(t.dest.clone())),
        ("frequency", t.frequency.clone().map(s)),
        ("sync", t.sync.clone()),
        ("retention_policy", t.retention.clone()),
        (
            "settings",
            (!t.settings.is_empty()).then(|| J::Obj(t.settings.clone())),
        ),
        ("_meta", t.meta.clone()),
        (
            if t.latest { "latest" } else { "pivot" },
            Some(t.function.clone()),
        ),
    ])
}

fn defer(t: &Tf) -> &'static str {
    if t.defer {
        "?defer_validation=true"
    } else {
        ""
    }
}

fn create_reqs(t: &Tf) -> Vec<Req> {
    let seg = path_segment(&t.id);
    let mut reqs = vec![Req::new(
        "PUT",
        format!("/_transform/{seg}{}", defer(t)),
        Some(body(t)),
    )];
    if t.started {
        reqs.push(Req::new("POST", format!("/_transform/{seg}/_start"), None));
    }
    reqs
}

/// `_update`'s body with what changed, or `None`.
fn update_body(have: &Tf, want: &Tf) -> Option<J> {
    let mut b: Vec<(String, J)> = Vec::new();
    if want.description != have.description {
        b.push(("description".into(), s(&want.description)));
    }
    if !same(&want.source, &have.source) {
        b.push(("source".into(), want.source.clone()));
    }
    if !same(&want.dest, &have.dest) {
        b.push(("dest".into(), want.dest.clone()));
    }
    if want.frequency != have.frequency {
        b.push((
            "frequency".into(),
            s(want.frequency.clone().unwrap_or_else(|| "1m".into())),
        ));
    }
    if let (Some(w), Some(h)) = (&want.sync, &have.sync) {
        if !same(w, h) {
            b.push(("sync".into(), w.clone()));
        }
    }
    if !same_opt(want.retention.as_ref(), have.retention.as_ref()) {
        b.push((
            "retention_policy".into(),
            want.retention.clone().unwrap_or(J::Null),
        ));
    }
    let mut settings = Vec::new();
    for (k, _) in SETTINGS {
        let w = want.settings.iter().find(|(x, _)| x == k).map(|(_, v)| v);
        let h = have.settings.iter().find(|(x, _)| x == k).map(|(_, v)| v);
        if !same_opt(w, h) {
            settings.push((k.to_string(), w.cloned().unwrap_or(J::Null)));
        }
    }
    if !settings.is_empty() {
        b.push(("settings".into(), J::Obj(settings)));
    }
    if !same_opt(want.meta.as_ref(), have.meta.as_ref()) {
        b.push((
            "_meta".into(),
            want.meta.clone().unwrap_or(J::Obj(Vec::new())),
        ));
    }
    (!b.is_empty()).then_some(J::Obj(b))
}

/// What can't change in place.
fn must_recreate(have: &Tf, want: &Tf) -> bool {
    want.id != have.id
        || want.latest != have.latest
        || !same(&want.function, &have.function)
        || want.sync.is_some() != have.sync.is_some()
}

pub(crate) fn plan(cur: Option<&Cur>, values: &FormValues) -> Result<Plan> {
    let Some(cur) = cur else {
        let want = spec(&values.values, &[])?;
        return Ok(Plan {
            notes: vec![format!("Transform «{}»", want.id)],
            reqs: create_reqs(&want),
            id: Some(super::Type::Transform.job_id(&want.id)),
            name: want.id.clone(),
            ..Plan::default()
        });
    };
    let have_values = values_of(cur);
    let explicit = explicit(cur);
    let have = spec(&have_values, &explicit)?;
    let want = spec(&values.values, &explicit)?;
    plan_change(cur, &have, &want)
}

pub(crate) fn plan_change(cur: &Cur, have: &Tf, want: &Tf) -> Result<Plan> {
    let seg = path_segment(&cur.id);
    let mut plan = Plan {
        notes: vec![format!("Transform «{}»", cur.id)],
        id: Some(super::Type::Transform.job_id(&want.id)),
        name: cur.id.clone(),
        ..Plan::default()
    };
    if must_recreate(have, want) {
        plan.notes.push(RECREATE_NOTE.into());
        if want.id != have.id {
            // The new one first: the old one goes only once the new exists.
            plan.reqs.push(
                Req::new(
                    "PUT",
                    format!("/_transform/{}{}", path_segment(&want.id), defer(want)),
                    Some(body(want)),
                )
                .note(&format!("El transform nuevo, «{}».", want.id)),
            );
            plan.reqs.push(
                Req::new("DELETE", format!("/_transform/{seg}?force=true"), None).note(&format!(
                    "Borra «{}» (su índice de destino se conserva).",
                    cur.id
                )),
            );
            if want.started {
                plan.reqs.push(Req::new(
                    "POST",
                    format!("/_transform/{}/_start", path_segment(&want.id)),
                    None,
                ));
            }
            return Ok(plan);
        }
        if !want.defer {
            let mut preview = body(want);
            if let J::Obj(o) = &mut preview {
                o.retain(|(k, _)| k != "description" && k != "_meta" && k != "retention_policy");
            }
            plan.reqs.push(
                Req::new("POST", "/_transform/_preview".into(), Some(preview))
                    .note("Comprueba la definición nueva antes de borrar la actual."),
            );
        }
        plan.reqs.push(
            Req::new("DELETE", format!("/_transform/{seg}?force=true"), None)
                .note("Borra el transform (su índice de destino se conserva)."),
        );
        plan.reqs.extend(create_reqs(want));
        return Ok(plan);
    }
    if let Some(b) = update_body(have, want) {
        plan.reqs.push(Req::new(
            "POST",
            format!("/_transform/{seg}/_update{}", defer(want)),
            Some(b),
        ));
    }
    if want.started != cur.started() {
        let path = if want.started {
            format!("/_transform/{seg}/_start")
        } else if cur.state == "failed" {
            format!("/_transform/{seg}/_stop?force=true")
        } else {
            format!("/_transform/{seg}/_stop")
        };
        plan.reqs.push(Req::new("POST", path, None));
    }
    Ok(plan)
}

pub(crate) fn delete_plan(cur: &Cur) -> Plan {
    Plan {
        notes: vec![format!(
            "Borra el transform «{}»; su índice de destino se conserva.",
            cur.id
        )],
        reqs: vec![Req::new(
            "DELETE",
            format!("/_transform/{}?force=true", path_segment(&cur.id)),
            None,
        )],
        name: cur.id.clone(),
        ..Plan::default()
    }
}
