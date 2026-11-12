//! The cluster's schedulers managed through the scheduler contract
//! ([`dbine_driver::Session::scheduler_info`] and the rest): what Kibana's
//! Transforms, Watcher and Snapshot Lifecycle Policies pages, or OpenSearch
//! Dashboards' Index Management, Alerting and Snapshot Management pages do.
//!
//! - One kind, "job", whose items are exactly the jobs [`crate::jobs`]
//!   lists (`transform:<id>`, `watch:<id>`, `slm:<id>` on Elasticsearch;
//!   `transform:<id>`, `monitor:<id>`, `sm:<name>` on OpenSearch and Open
//!   Distro), so the tree's job rows open the designer. A new job starts
//!   with its type: the form has every type's fields, each shown only for
//!   its type, in four generic tabs; an existing one gets its type's own
//!   tabs.
//! - Scripts: the requests as the editor's console takes them (`PUT
//!   /_transform/x` plus its JSON body), `#` comments on top. An edit sends
//!   only what changed, in place, where the API has an update
//!   (`_transform/<id>/_update`, a watch's PUT with `if_seq_no`, an SLM
//!   policy's PUT, OpenSearch's PUT with `if_seq_no` / `if_primary_term`);
//!   what the API can't change in place (a transform's pivot or latest, a
//!   continuous transform turned into a batch one, an OpenSearch
//!   transform's indices, query, groups or aggregations, any id) is
//!   deleted and created again, which the script and the form say, with
//!   what is lost.
//! - Values and ids come from the app as untrusted text: ids are checked
//!   (Elasticsearch's own rule for transform ids, one path segment for the
//!   rest) and go into the URL percent-encoded; the JSON fields are parsed
//!   and must have their shape; times, numbers, cron expressions and the
//!   names picked from the cluster's lists (repositories, pipelines,
//!   channels) are checked before anything is sent. Secrets live inside the
//!   JSON (a webhook's password): the cluster returns them redacted
//!   (`::es_redacted::`), an update with `if_seq_no` keeps them, and the
//!   script shows any other `password`-like value masked.
//! - The requests run one after the other (the API has no transactions): a
//!   failure stops the rest and says which request failed after which
//!   others were applied. A recreation first checks the new definition
//!   with the engine's `_preview` where it has one.

mod es_transform;
mod lifecycle;
mod logs;
mod monitor;
mod os_transform;
mod slm;
mod sm;
mod watch;

use crate::ddl::path_segment;
use crate::jobs::{self, one_name, text, Kind as JobKind};
use crate::json::J;
use crate::{es_error_message, http, EsSession};
use dbine_driver::{
    Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerInfo,
    SchedulerItem, SchedulerKind, SchedulerList, SECRET_MASK,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub(crate) use logs::logs;

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

const NOT_A_KIND: &str = "este clúster administra jobs (transforms, watches, monitores y políticas de snapshots) y políticas de ciclo de vida de los índices (ILM en Elasticsearch, ISM en OpenSearch)";
const NO_WATCHER: &str = "Watcher no está disponible en este clúster (hace falta una licencia gold, platinum, enterprise o de prueba): no se pueden crear watches";
const NO_TRANSFORMS: &str =
    "este clúster no tiene transforms (hacen falta X-Pack y un nodo con el rol transform)";
const NO_SLM: &str =
    "este clúster no tiene la administración del ciclo de vida de los snapshots (SLM)";
const NO_OS_TRANSFORMS: &str =
    "este clúster no tiene el plugin Index Management: no hay transforms ni políticas de snapshots";
const NO_ALERTING: &str = "este clúster no tiene el plugin Alerting: no hay monitores";
const OPENDISTRO_SCOPE: &str = "Open Distro no tiene transforms que se puedan listar ni políticas de snapshots: se administran sus monitores de alertas";
const NO_PRIV_TRANSFORM: &str =
    "este usuario no puede crear ni cambiar transforms (hace falta el privilegio manage_transform)";
const NO_PRIV_WATCHER: &str =
    "este usuario no puede crear ni cambiar watches (hace falta el privilegio manage_watcher)";
const NO_PRIV_ILM: &str =
    "este usuario no puede crear ni cambiar políticas ILM (hace falta el privilegio manage_ilm)";
const NO_PRIV_SLM: &str = "este usuario no puede crear ni cambiar políticas de snapshots (hace falta el privilegio manage_slm)";
const MANAGED: &str = "Lo administra Elastic (una integración o Kibana): conviene cambiarlo desde allí, porque una actualización de la integración puede reemplazar los cambios.";
const REDACTED: &str = "::es_redacted::";

fn bad(message: String) -> Error {
    Error::Query(message)
}

/// Text from the app, short and on one line, for a message.
fn shown(s: &str) -> String {
    one_line(s.trim()).chars().take(60).collect()
}

/// Text for a `#` comment line.
fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

// ---------------------------------------------------------------------------
// Types of job
// ---------------------------------------------------------------------------

/// What a job of the "job" kind is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Type {
    /// Elasticsearch transform (`_transform`).
    Transform,
    /// Elasticsearch Watcher watch.
    Watch,
    /// Elasticsearch snapshot lifecycle policy.
    Slm,
    /// OpenSearch Index Management transform.
    OsTransform,
    /// OpenSearch / Open Distro Alerting monitor.
    Monitor,
    /// OpenSearch snapshot management policy.
    Sm,
}

impl Type {
    /// The `type` field's value.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Type::Transform | Type::OsTransform => "transform",
            Type::Watch => "watch",
            Type::Slm => "slm",
            Type::Monitor => "monitor",
            Type::Sm => "sm",
        }
    }

    fn of_job(kind: JobKind, os: bool) -> Type {
        match kind {
            JobKind::Transform if os => Type::OsTransform,
            JobKind::Transform => Type::Transform,
            JobKind::Watch => Type::Watch,
            JobKind::Slm => Type::Slm,
            JobKind::Monitor => Type::Monitor,
            JobKind::Sm => Type::Sm,
        }
    }

    fn job_kind(self) -> JobKind {
        match self {
            Type::Transform | Type::OsTransform => JobKind::Transform,
            Type::Watch => JobKind::Watch,
            Type::Slm => JobKind::Slm,
            Type::Monitor => JobKind::Monitor,
            Type::Sm => JobKind::Sm,
        }
    }

    /// A job id for the object `id` of this type.
    pub(crate) fn job_id(self, id: &str) -> String {
        jobs::job_id(self.job_kind(), id)
    }
}

// ---------------------------------------------------------------------------
// What the cluster and the user allow
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub(crate) struct Ctx {
    /// OpenSearch or Open Distro (their plugins' APIs).
    pub(crate) os: bool,
    /// `_plugins` or `_opendistro`.
    pub(crate) plugins: &'static str,
    /// Open Distro: monitors without `monitor_type`, destinations instead
    /// of channels.
    pub(crate) opendistro: bool,
    /// The types this cluster has, in the order the type select offers
    /// them.
    pub(crate) types: Vec<Type>,
    /// The types the user may create, change and delete.
    pub(crate) manage: Vec<Type>,
    /// SLM runs (Elasticsearch).
    pub(crate) slm_running: bool,
    /// The cluster has index lifecycle policies (ILM, ISM).
    pub(crate) lifecycle: bool,
    /// The user may change them.
    pub(crate) manage_lifecycle: bool,
    pub(crate) notes: Vec<String>,
}

impl Ctx {
    pub(crate) fn has(&self, t: Type) -> bool {
        self.types.contains(&t)
    }

    pub(crate) fn may(&self, t: Type) -> bool {
        self.manage.contains(&t)
    }
}

/// What `_xpack` says of a feature: available and enabled (unknown: yes).
fn feature(x: Option<&J>, name: &str) -> bool {
    let Some(f) = x.and_then(|x| x.at(&["features", name])) else {
        return x.is_none();
    };
    f.get("available").and_then(J::as_bool) != Some(false)
        && f.get("enabled").and_then(J::as_bool) != Some(false)
}

/// The cluster privileges of `names` the user has (a cluster without
/// security: all).
async fn cluster_privileges(s: &EsSession, names: &[&str]) -> Result<BTreeMap<String, bool>> {
    let body = J::Obj(vec![(
        "cluster".into(),
        J::Arr(names.iter().map(|n| J::Str((*n).into())).collect()),
    )]);
    let (status, v) = s
        .job_probe("POST", "/_security/user/_has_privileges", Some(&body))
        .await?;
    Ok(names
        .iter()
        .map(|n| {
            let has = status >= 400 || v.at(&["cluster", n]).and_then(J::as_bool) != Some(false);
            (n.to_string(), has)
        })
        .collect())
}

async fn ctx(s: &EsSession) -> Result<Ctx> {
    let mut c = Ctx {
        os: s.opensearch,
        plugins: s.plugins(),
        opendistro: s.opendistro,
        slm_running: true,
        ..Ctx::default()
    };
    if !s.opensearch {
        let x = match s
            .job_probe("GET", "/_xpack?categories=features", None)
            .await?
        {
            (200, v) => Some(v),
            _ => None,
        };
        let privs = cluster_privileges(
            s,
            &[
                "manage_transform",
                "manage_watcher",
                "manage_slm",
                "manage_ilm",
            ],
        )
        .await?;
        c.lifecycle = feature(x.as_ref(), "ilm");
        c.manage_lifecycle = c.lifecycle && privs.get("manage_ilm").copied().unwrap_or(true);
        if c.lifecycle && !c.manage_lifecycle {
            c.notes.push(NO_PRIV_ILM.into());
        }
        for (t, feat, privilege, missing, denied) in [
            (
                Type::Transform,
                "transform",
                "manage_transform",
                NO_TRANSFORMS,
                NO_PRIV_TRANSFORM,
            ),
            (
                Type::Watch,
                "watcher",
                "manage_watcher",
                NO_WATCHER,
                NO_PRIV_WATCHER,
            ),
            (Type::Slm, "slm", "manage_slm", NO_SLM, NO_PRIV_SLM),
        ] {
            if !feature(x.as_ref(), feat) {
                c.notes.push(missing.into());
                continue;
            }
            c.types.push(t);
            if privs.get(privilege).copied().unwrap_or(true) {
                c.manage.push(t);
            } else {
                c.notes.push(denied.into());
            }
        }
        if c.has(Type::Slm) {
            c.slm_running = s.slm_running().await;
            if !c.slm_running {
                c.notes.push(jobs::SLM_STOPPED.into());
            }
        }
        return Ok(c);
    }
    let plugins: Vec<String> = match s
        .job_probe("GET", "/_cat/plugins?format=json&h=component", None)
        .await?
    {
        (200, J::Arr(a)) => a.iter().filter_map(|p| text(p.get("component"))).collect(),
        _ => Vec::new(),
    };
    let known = !plugins.is_empty();
    let has = |suffix: &str| !known || plugins.iter().any(|p| p.ends_with(suffix));
    let ism = has("-index-management");
    if s.opendistro {
        c.notes.push(OPENDISTRO_SCOPE.into());
    } else if ism {
        c.types.push(Type::OsTransform);
    } else {
        c.notes.push(NO_OS_TRANSFORMS.into());
    }
    if has("-alerting") {
        c.types.push(Type::Monitor);
    } else {
        c.notes.push(NO_ALERTING.into());
    }
    if ism && s.has_sm() {
        c.types.push(Type::Sm);
    }
    c.lifecycle = ism;
    c.manage_lifecycle = ism;
    // The security plugin has no API that answers what a user may do: the
    // cluster refuses what it doesn't allow when the change is sent.
    c.manage = c.types.clone();
    Ok(c)
}

// ---------------------------------------------------------------------------
// The contract's calls
// ---------------------------------------------------------------------------

/// The scheduler's kinds: its jobs, and the index lifecycle policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Job,
    /// ISM on OpenSearch and Open Distro, ILM on Elasticsearch.
    Lifecycle,
}

fn check_kind(kind: &str) -> Result<Kind> {
    match kind.trim() {
        "job" => Ok(Kind::Job),
        "ism" | "ilm" => Ok(Kind::Lifecycle),
        _ => Err(Error::Unsupported(NOT_A_KIND.into())),
    }
}

/// The kind on this cluster (`ism` asked of Elasticsearch is refused).
fn kind_of(s: &EsSession, kind: &str) -> Result<Kind> {
    let k = check_kind(kind)?;
    if k == Kind::Lifecycle && (kind.trim() == "ism") != s.opensearch {
        return Err(Error::Unsupported(NOT_A_KIND.into()));
    }
    Ok(k)
}

fn info_of(c: &Ctx) -> SchedulerInfo {
    let any = !c.manage.is_empty();
    let mut kinds = vec![SchedulerKind::new("job", "Jobs", "job").abilities(any, any, any)];
    if c.lifecycle {
        let m = c.manage_lifecycle;
        kinds.push(if c.os {
            SchedulerKind::new("ism", "Políticas ISM", "política ISM").abilities(m, m, m)
        } else {
            SchedulerKind::new("ilm", "Políticas ILM", "política ILM").abilities(m, m, m)
        });
    }
    SchedulerInfo {
        kinds,
        logs: !c.types.is_empty(),
        notes: c.notes.clone(),
    }
}

pub(crate) async fn info(s: &EsSession) -> Result<SchedulerInfo> {
    Ok(info_of(&ctx(s).await?))
}

pub(crate) async fn items(s: &EsSession, kind: &str) -> Result<SchedulerList> {
    if kind_of(s, kind)? == Kind::Lifecycle {
        let c = ctx(s).await?;
        return lifecycle::items(s, &c).await;
    }
    let list = s.jobs_list().await?;
    Ok(SchedulerList {
        items: list
            .jobs
            .into_iter()
            .map(|j| SchedulerItem {
                id: j.id,
                name: j.name,
                description: j.description,
                enabled: Some(j.enabled),
                system: j.system,
            })
            .collect(),
        notes: list.notes,
    })
}

/// The type and the engine's id of a job id, checked.
fn parse(s: &EsSession, id: &str) -> Result<(Type, String)> {
    let (kind, rest) = s.parse(id)?;
    Ok((Type::of_job(kind, s.opensearch), rest))
}

/// The `type` select: the cluster's types (or, for an existing object,
/// its own, read-only).
fn type_field(c: &Ctx, own: Option<Type>) -> FormField {
    let all = if c.os {
        FormKind::select(&[
            ("transform", "Transform"),
            ("monitor", "Monitor de alertas"),
            ("sm", "Política de snapshots"),
        ])
    } else {
        FormKind::select(&[
            ("transform", "Transform"),
            ("watch", "Watch"),
            ("slm", "Política de snapshots (SLM)"),
        ])
    };
    let kind = match all {
        FormKind::Select { mut options } => {
            options.retain(|(v, _)| match own {
                Some(t) => t.key() == v,
                None => c.types.iter().any(|t| c.may(*t) && t.key() == v),
            });
            FormKind::Select { options }
        }
        other => other,
    };
    FormField::new("type", "Tipo", kind)
        .required()
        .read_only(own.is_some())
}

/// The four tabs of a new job's form, and which of them a type's own tab
/// goes to.
fn generic_index(section: &str) -> usize {
    match section {
        "general" => 0,
        "schedule" | "trigger" => 1,
        "settings" | "retention" | "options" | "deletion" | "notification" => 3,
        _ => 2,
    }
}

fn generic_sections() -> Vec<FormSection> {
    vec![
        FormSection::new("general", "General"),
        FormSection::new("schedule", "Programación"),
        FormSection::new("definition", "Definición"),
        FormSection::new("options", "Opciones"),
    ]
}

/// A new job's form: the type, then each type's fields in the generic
/// tabs, shown only for their type (a field that depends on another of its
/// type's fields follows it).
fn combined(c: &Ctx, parts: Vec<(Type, Vec<FormSection>)>) -> Vec<FormSection> {
    let mut out = generic_sections();
    out[0].fields.push(type_field(c, None));
    for (t, sections) in parts {
        for sec in sections {
            let i = generic_index(&sec.key);
            for f in sec.fields {
                let f = if f.when.is_none() {
                    f.when("type", &[t.key()])
                } else {
                    f
                };
                out[i].fields.push(f);
            }
        }
    }
    out.retain(|s| !s.fields.is_empty());
    out
}

/// A form's field defaults as its values.
fn defaults(sections: &[FormSection]) -> BTreeMap<String, Value> {
    let mut v = BTreeMap::new();
    for f in sections.iter().flat_map(|s| s.fields.iter()) {
        if let Some(d) = &f.default {
            v.entry(f.key.clone()).or_insert_with(|| d.clone());
        }
    }
    v
}

/// An ISM or ILM policy's form.
async fn lifecycle_form(s: &EsSession, c: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let sections = if c.os {
        lifecycle::ism_sections()
    } else {
        lifecycle::ilm_sections()
    };
    let mut notes = Vec::new();
    let mut read_only = !c.manage_lifecycle;
    let mut values = match id {
        None => BTreeMap::new(),
        Some(id) if c.os => {
            notes.push(lifecycle::ISM_NOTE.to_string());
            lifecycle::ism_values(&lifecycle::read_ism(s, id).await?)
        }
        Some(id) => {
            let cur = lifecycle::read_ilm(s, id).await?;
            notes.push(lifecycle::ILM_NOTE.to_string());
            if cur.managed() {
                read_only = true;
                notes.insert(
                    0,
                    "Esta política la administra Elastic: no se cambia ni se borra desde aquí."
                        .into(),
                );
            }
            lifecycle::ilm_values(&cur)
        }
    };
    for (k, v) in defaults(&sections) {
        values.entry(k).or_insert(v);
    }
    if !c.manage_lifecycle {
        notes.insert(
            0,
            "este usuario no puede cambiar las políticas: el formulario es de solo lectura".into(),
        );
    }
    Ok(ObjectForm {
        sections,
        values,
        choices: BTreeMap::new(),
        notes,
        read_only,
    })
}

async fn lifecycle_plan(
    s: &EsSession,
    c: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    if c.os {
        let cur = match id {
            Some(id) => Some(lifecycle::read_ism(s, id).await?),
            None => None,
        };
        lifecycle::ism_plan(c, cur.as_ref(), values)
    } else {
        let cur = match id {
            Some(id) => Some(lifecycle::read_ilm(s, id).await?),
            None => None,
        };
        lifecycle::ilm_plan(cur.as_ref(), values)
    }
}

async fn lifecycle_delete_plan(s: &EsSession, c: &Ctx, id: &str) -> Result<Plan> {
    let cur = if c.os {
        lifecycle::read_ism(s, id).await?
    } else {
        lifecycle::read_ilm(s, id).await?
    };
    lifecycle::delete_plan(c, &cur)
}

/// The lifecycle kind, refused where the cluster doesn't have it.
fn lifecycle_ready(c: &Ctx, change: bool) -> Result<()> {
    if !c.lifecycle {
        return Err(Error::Unsupported(
            "este clúster no tiene políticas de ciclo de vida de los índices".into(),
        ));
    }
    if change && !c.manage_lifecycle {
        return Err(bad(
            "este usuario no puede crear, cambiar ni borrar políticas de ciclo de vida".into(),
        ));
    }
    Ok(())
}

pub(crate) async fn form(s: &EsSession, kind: &str, id: Option<&str>) -> Result<ObjectForm> {
    let k = kind_of(s, kind)?;
    let c = ctx(s).await?;
    if k == Kind::Lifecycle {
        lifecycle_ready(&c, false)?;
        return lifecycle_form(s, &c, id).await;
    }
    let Some(id) = id else {
        let choices = load_choices(s, &c).await?;
        let mut parts = Vec::new();
        for t in c.types.iter().filter(|t| c.may(**t)) {
            parts.push((*t, sections_of(&c, *t, true, &choices)));
        }
        if parts.is_empty() {
            let mut f = ObjectForm {
                sections: vec![FormSection::new("general", "General").field(type_field(&c, None))],
                read_only: true,
                ..ObjectForm::default()
            };
            f.notes = c.notes.clone();
            return Ok(f);
        }
        let first = parts[0].0;
        let sections = combined(&c, parts);
        let mut values = BTreeMap::new();
        // The first type's defaults win where two types share a key.
        for sec in sections.iter() {
            for fld in &sec.fields {
                let first_type = fld
                    .when
                    .as_ref()
                    .is_some_and(|(k, v)| k == "type" && v.iter().any(|x| x == first.key()));
                if let (true, Some(d)) = (first_type, &fld.default) {
                    values.insert(fld.key.clone(), d.clone());
                }
            }
        }
        for (k, v) in defaults(&sections) {
            values.entry(k).or_insert(v);
        }
        values.insert("type".into(), json!(first.key()));
        if c.may(Type::Monitor) {
            values.insert("channels".into(), monitor::channels_text(&choices));
        }
        let mut notes = c.notes.clone();
        notes.extend(new_notes(&c));
        return Ok(ObjectForm {
            sections,
            values,
            choices: choices.lists,
            notes,
            read_only: false,
        });
    };
    let (t, rest) = parse(s, id)?;
    let choices = load_choices(s, &c).await?;
    let mut sections = sections_of(&c, t, false, &choices);
    sections[0].fields.insert(0, type_field(&c, Some(t)));
    let mut locked = false;
    let (mut values, mut notes) = match t {
        Type::Transform => {
            let cur = es_transform::read(s, &rest).await?;
            let mut notes = vec![es_transform::RECREATE_NOTE.to_string()];
            if cur.managed {
                notes.push(MANAGED.into());
            }
            (es_transform::values_of(&cur), notes)
        }
        Type::Watch => {
            let cur = watch::read(s, &rest).await?;
            (watch::values_of(&cur), vec![watch::NOTE.to_string()])
        }
        Type::Slm => {
            let cur = slm::read(s, &rest).await?;
            (slm::values_of(&cur), vec![slm::NOTE.to_string()])
        }
        Type::OsTransform => {
            let cur = os_transform::read(s, &rest).await?;
            (
                os_transform::values_of(&cur),
                vec![os_transform::RECREATE_NOTE.to_string()],
            )
        }
        Type::Monitor => {
            let cur = monitor::read(s, &rest).await?;
            let mut notes = Vec::new();
            if let Some(owner) = jobs::os_monitor_owner(&cur.monitor) {
                locked = true;
                notes.push(format!(
                    "este monitor lo administra {owner}: el plugin Alerting no deja cambiarlo desde aquí"
                ));
            }
            (monitor::values_of(&cur), notes)
        }
        Type::Sm => {
            let cur = sm::read(s, &rest).await?;
            (sm::values_of(&cur), vec![sm::NOTE.to_string()])
        }
    };
    values.insert("type".into(), json!(t.key()));
    for (k, v) in defaults(&sections) {
        values.entry(k).or_insert(v);
    }
    if t == Type::Slm && !c.slm_running {
        notes.insert(0, jobs::SLM_STOPPED.into());
    }
    let read_only = !c.may(t) || !c.has(t) || locked;
    if !c.has(t) {
        notes.insert(
            0,
            "este clúster ya no tiene habilitado este tipo de job: el formulario es de solo lectura"
                .into(),
        );
    } else if read_only && !locked {
        notes.insert(
            0,
            "este usuario no puede cambiar este job: el formulario es de solo lectura".into(),
        );
    }
    if t == Type::Monitor {
        values.insert("channels".into(), monitor::channels_text(&choices));
    }
    Ok(ObjectForm {
        sections,
        values,
        choices: choices.lists,
        notes,
        read_only,
    })
}

/// What a new job's form says about each type.
fn new_notes(c: &Ctx) -> Vec<String> {
    let mut n = Vec::new();
    if c.may(Type::Watch) {
        n.push(watch::NOTE.to_string());
    }
    if c.may(Type::OsTransform) {
        n.push(os_transform::RECREATE_NOTE.to_string());
    }
    n
}

fn sections_of(c: &Ctx, t: Type, new: bool, choices: &Choices) -> Vec<FormSection> {
    match t {
        Type::Transform => es_transform::sections(new),
        Type::Watch => watch::sections(),
        Type::Slm => slm::sections(),
        Type::OsTransform => os_transform::sections(),
        Type::Monitor => monitor::sections(c, new, choices),
        Type::Sm => sm::sections(),
    }
}

/// The type a form's values ask for (a new job's select), checked against
/// what the cluster has and the user may manage.
fn wanted_type(c: &Ctx, values: &FormValues) -> Result<Type> {
    let v = Vals(&values.values);
    let key = v.text("type");
    let t = c
        .types
        .iter()
        .copied()
        .find(|t| t.key() == key)
        .ok_or_else(|| {
            if key.is_empty() {
                bad("Elija el tipo de job.".into())
            } else {
                bad(format!(
                    "«{}» no es un tipo de job de este clúster.",
                    shown(&key)
                ))
            }
        })?;
    Ok(t)
}

async fn plan(s: &EsSession, c: &Ctx, id: Option<&str>, values: &FormValues) -> Result<Plan> {
    let choices = load_choices(s, c).await?;
    let (t, cur) = match id {
        None => (wanted_type(c, values)?, None),
        Some(id) => {
            let (t, rest) = parse(s, id)?;
            (t, Some(rest))
        }
    };
    if !c.has(t) {
        return Err(Error::Unsupported(
            "este clúster no tiene este tipo de job".into(),
        ));
    }
    match t {
        Type::Transform => {
            let cur = match &cur {
                Some(id) => Some(es_transform::read(s, id).await?),
                None => None,
            };
            es_transform::plan(cur.as_ref(), values)
        }
        Type::Watch => {
            let cur = match &cur {
                Some(id) => Some(watch::read(s, id).await?),
                None => None,
            };
            watch::plan(cur.as_ref(), values)
        }
        Type::Slm => {
            let cur = match &cur {
                Some(id) => Some(slm::read(s, id).await?),
                None => None,
            };
            slm::plan(cur.as_ref(), values, &choices)
        }
        Type::OsTransform => {
            let cur = match &cur {
                Some(id) => Some(os_transform::read(s, id).await?),
                None => None,
            };
            os_transform::plan(c, cur.as_ref(), values)
        }
        Type::Monitor => {
            let cur = match &cur {
                Some(id) => Some(monitor::read(s, id).await?),
                None => None,
            };
            monitor::plan(c, cur.as_ref(), values)
        }
        Type::Sm => {
            let cur = match &cur {
                Some(id) => Some(sm::read(s, id).await?),
                None => None,
            };
            sm::plan(c, cur.as_ref(), values, &choices)
        }
    }
}

pub(crate) async fn script(
    s: &EsSession,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<String> {
    let k = kind_of(s, kind)?;
    let c = ctx(s).await?;
    if k == Kind::Lifecycle {
        lifecycle_ready(&c, false)?;
        return Ok(lifecycle_plan(s, &c, id, values).await?.render());
    }
    Ok(plan(s, &c, id, values).await?.render())
}

pub(crate) async fn save(
    s: &EsSession,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Option<String>> {
    let k = kind_of(s, kind)?;
    let c = ctx(s).await?;
    if k == Kind::Lifecycle {
        lifecycle_ready(&c, true)?;
        let plan = lifecycle_plan(s, &c, id, values).await?;
        return run(s, &plan).await;
    }
    let t = match id {
        Some(id) => parse(s, id)?.0,
        None => wanted_type(&c, values)?,
    };
    if !c.may(t) {
        return Err(bad(
            "este usuario no puede crear ni cambiar este tipo de job".into(),
        ));
    }
    let plan = plan(s, &c, id, values).await?;
    run(s, &plan).await
}

pub(crate) async fn delete_script(s: &EsSession, kind: &str, id: &str) -> Result<String> {
    if kind_of(s, kind)? == Kind::Lifecycle {
        let c = ctx(s).await?;
        lifecycle_ready(&c, false)?;
        return Ok(lifecycle_delete_plan(s, &c, id).await?.render());
    }
    Ok(delete_plan(s, id).await?.render())
}

async fn delete_plan(s: &EsSession, id: &str) -> Result<Plan> {
    let (t, rest) = parse(s, id)?;
    Ok(match t {
        Type::Transform => es_transform::delete_plan(&es_transform::read(s, &rest).await?),
        Type::Watch => watch::delete_plan(&watch::read(s, &rest).await?),
        Type::Slm => slm::delete_plan(&slm::read(s, &rest).await?),
        Type::OsTransform => {
            os_transform::delete_plan(s.plugins(), &os_transform::read(s, &rest).await?)
        }
        Type::Monitor => monitor::delete_plan(s.plugins(), &monitor::read(s, &rest).await?)?,
        Type::Sm => sm::delete_plan(s.plugins(), &sm::read(s, &rest).await?),
    })
}

pub(crate) async fn delete(s: &EsSession, kind: &str, id: &str) -> Result<()> {
    let k = kind_of(s, kind)?;
    let c = ctx(s).await?;
    if k == Kind::Lifecycle {
        lifecycle_ready(&c, true)?;
        let plan = lifecycle_delete_plan(s, &c, id).await?;
        return run(s, &plan).await.map(|_| ());
    }
    let (t, _) = parse(s, id)?;
    if !c.may(t) {
        return Err(bad("este usuario no puede borrar este tipo de job".into()));
    }
    let plan = delete_plan(s, id).await?;
    run(s, &plan).await.map(|_| ())
}

/// "Start at step": a watch runs its actions from that one on (the earlier
/// ones skipped, the rest forced past their throttling); a job of one
/// step, or a monitor at its inputs, runs whole.
pub(crate) async fn run_from(s: &EsSession, id: &str, step: &str) -> Result<()> {
    let (t, rest) = parse(s, id)?;
    let step = step.trim();
    match t {
        Type::Watch => {
            let cur = watch::read(s, &rest).await?;
            let body = watch::run_from_body(&cur, step)?;
            s.execute_detached(
                format!("/_watcher/watch/{}/_execute", path_segment(&rest)),
                body,
            )
            .await
        }
        Type::Monitor => {
            if step != "Entradas" {
                return Err(bad(
                    "el plugin Alerting ejecuta el monitor entero: solo se puede empezar por sus entradas".into(),
                ));
            }
            s.job_start(id).await
        }
        Type::Transform | Type::OsTransform | Type::Slm | Type::Sm => {
            let detail = s.job_info(id).await?;
            if !detail.steps.iter().any(|x| x.name == step) {
                return Err(bad(format!("«{}» no es un paso de este job.", shown(step))));
            }
            s.job_start(id).await
        }
    }
}

/// A watch's records in `.watcher-history-*`, an SLM policy's in
/// `.slm-history-*`.
pub(crate) async fn clear_history(s: &EsSession, id: &str) -> Result<()> {
    let (t, rest) = parse(s, id)?;
    let (pattern, field) = match t {
        Type::Watch => {
            watch::read(s, &rest).await?;
            (".watcher-history-*", "watch_id")
        }
        Type::Slm => {
            slm::read(s, &rest).await?;
            (".slm-history-*", "policy")
        }
        _ => {
            return Err(Error::Unsupported(
                "el clúster no guarda el historial de ejecuciones de este job".into(),
            ))
        }
    };
    let body = J::Obj(vec![(
        "query".into(),
        J::Obj(vec![(
            "term".into(),
            J::Obj(vec![(field.into(), J::Str(rest.clone()))]),
        )]),
    )]);
    let path = format!(
        "/{pattern}/_delete_by_query?ignore_unavailable=true&allow_no_indices=true&refresh=true&conflicts=proceed"
    );
    let (_, v) = s.job_call("POST", &path, Some(&body)).await?;
    let failures = v.get("failures").and_then(J::as_arr).unwrap_or(&[]);
    if let Some(f) = failures.first() {
        return Err(bad(format!(
            "no se pudo borrar todo el historial: {}",
            jobs::cut(&f.compact())
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The cluster's lists the forms pick from
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub(crate) struct Choices {
    pub(crate) lists: BTreeMap<String, Vec<(String, String)>>,
}

impl Choices {
    pub(crate) fn get(&self, k: &str) -> &[(String, String)] {
        self.lists.get(k).map(Vec::as_slice).unwrap_or(&[])
    }
}

async fn load_choices(s: &EsSession, c: &Ctx) -> Result<Choices> {
    let mut ch = Choices::default();
    let names = |v: &J| -> Vec<(String, String)> {
        let mut n: Vec<String> = v
            .as_obj()
            .into_iter()
            .flatten()
            .map(|(k, _)| k.clone())
            .collect();
        n.sort();
        n.into_iter().map(|k| (k.clone(), k)).collect()
    };
    if c.has(Type::Slm) || c.has(Type::Sm) {
        if let (200, v) = s.job_probe("GET", "/_snapshot", None).await? {
            ch.lists.insert("repository".into(), names(&v));
        }
    }
    if c.has(Type::Transform) {
        if let (200, v) = s.job_probe("GET", "/_ingest/pipeline", None).await? {
            let mut p = vec![(String::new(), "Ninguno".to_string())];
            p.extend(names(&v));
            ch.lists.insert("pipeline".into(), p);
        }
    }
    if c.has(Type::Slm) {
        if let (200, v) = s.job_probe("GET", "/_features", None).await? {
            let f = v
                .get("features")
                .and_then(J::as_arr)
                .into_iter()
                .flatten()
                .filter_map(|f| {
                    let n = text(f.get("name"))?;
                    Some((n.clone(), n))
                })
                .collect();
            ch.lists.insert("feature".into(), f);
        }
    }
    if c.has(Type::Monitor) || c.has(Type::Sm) {
        ch.lists.insert("channel".into(), channels(s, c).await?);
    }
    Ok(ch)
}

/// The Notifications plugin's channels (OpenSearch 2.0 on), else the
/// Alerting plugin's destinations, as (id, name).
async fn channels(s: &EsSession, c: &Ctx) -> Result<Vec<(String, String)>> {
    let p = c.plugins;
    let mut out = vec![(String::new(), "Ninguno".to_string())];
    if !c.opendistro {
        if let (200, v) = s
            .job_probe(
                "GET",
                &format!("/{p}/_notifications/configs?max_items=1000"),
                None,
            )
            .await?
        {
            for x in v
                .get("config_list")
                .and_then(J::as_arr)
                .into_iter()
                .flatten()
            {
                if let Some(id) = text(x.get("config_id")) {
                    let name = text(x.at(&["config", "name"])).unwrap_or_else(|| id.clone());
                    out.push((id, name));
                }
            }
            return Ok(out);
        }
    }
    if let (200, v) = s
        .job_probe(
            "GET",
            &format!("/{p}/_alerting/destinations?size=1000"),
            None,
        )
        .await?
    {
        for x in v
            .get("destinations")
            .and_then(J::as_arr)
            .into_iter()
            .flatten()
        {
            if let Some(id) = text(x.get("id")) {
                let name = text(x.get("name")).unwrap_or_else(|| id.clone());
                out.push((id, name));
            }
        }
    }
    Ok(out)
}

/// A name picked from one of the cluster's lists: it has to be one of them.
/// An empty list (the user may not read it) lets the name through: the
/// cluster checks it.
pub(crate) fn pick(name: &str, list: &[(String, String)], what: &str) -> Result<String> {
    if list.is_empty() || list.iter().any(|(v, _)| v == name) {
        return Ok(name.to_string());
    }
    Err(bad(format!(
        "{what} «{}» no existe en este clúster.",
        shown(name)
    )))
}

// ---------------------------------------------------------------------------
// Values from the designer
// ---------------------------------------------------------------------------

/// A form's values as the app sent them: untrusted, any JSON type where a
/// field expects another.
#[derive(Clone, Copy)]
pub(crate) struct Vals<'a>(pub(crate) &'a BTreeMap<String, Value>);

/// What a JSON field has to hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    Object,
    Array,
}

impl<'a> Vals<'a> {
    fn get(&self, k: &str) -> Option<&'a Value> {
        self.0.get(k).filter(|v| !v.is_null())
    }

    /// The value as typed.
    pub(crate) fn raw(&self, k: &str) -> String {
        match self.get(k) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(_) | Value::Object(_)) | None => String::new(),
            Some(v) => v.to_string(),
        }
    }

    /// The value trimmed.
    pub(crate) fn text(&self, k: &str) -> String {
        self.raw(k).trim().to_string()
    }

    /// The value trimmed, `None` when empty.
    pub(crate) fn opt(&self, k: &str) -> Option<String> {
        Some(self.text(k)).filter(|t| !t.is_empty())
    }

    /// One line of text (a name, an index): no control characters.
    pub(crate) fn line(&self, k: &str, label: &str) -> Result<Option<String>> {
        match self.opt(k) {
            Some(t) if t.chars().any(char::is_control) => Err(bad(format!(
                "«{label}» no puede tener saltos de línea ni caracteres de control."
            ))),
            t => Ok(t),
        }
    }

    pub(crate) fn bool(&self, k: &str) -> bool {
        match self.get(k) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            Some(Value::String(s)) => {
                matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "true" | "1" | "yes" | "on"
                )
            }
            _ => false,
        }
    }

    /// A bool that defaults to `default` when the form didn't send it.
    pub(crate) fn bool_or(&self, k: &str, default: bool) -> bool {
        if self.get(k).is_some() {
            self.bool(k)
        } else {
            default
        }
    }

    /// A whole number in `min..=max`; `None` when missing or empty.
    pub(crate) fn int(&self, k: &str, label: &str, min: i64, max: i64) -> Result<Option<i64>> {
        let n = match self.get(k) {
            None => return Ok(None),
            Some(Value::String(s)) if s.trim().is_empty() => return Ok(None),
            Some(Value::Number(n)) => n
                .as_i64()
                .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
            Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
            Some(_) => None,
        };
        match n {
            Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
            Some(n) => Err(bad(format!(
                "«{label}» tiene que estar entre {min} y {max} (es {n})."
            ))),
            None => Err(bad(format!("«{label}» tiene que ser un número entero."))),
        }
    }

    /// A number (decimals allowed), at least `min`.
    pub(crate) fn number(&self, k: &str, label: &str, min: f64) -> Result<Option<J>> {
        let v = match self.get(k) {
            None => return Ok(None),
            Some(Value::String(s)) if s.trim().is_empty() => return Ok(None),
            Some(Value::Number(n)) => n.as_f64(),
            Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
            Some(_) => None,
        };
        match v.filter(|f| f.is_finite()) {
            Some(f) if f >= min => Ok(Some(num_j(f))),
            Some(_) => Err(bad(format!("«{label}» tiene que ser al menos {min}."))),
            None => Err(bad(format!("«{label}» tiene que ser un número."))),
        }
    }

    /// A multi-select's values (or a comma-separated text), trimmed,
    /// without empties.
    pub(crate) fn strings(&self, k: &str) -> Vec<String> {
        match self.get(k) {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.trim().to_string()),
                    Value::Null => None,
                    v => Some(v.to_string()),
                })
                .filter(|s| !s.is_empty())
                .collect(),
            Some(Value::String(s)) => split_list(s),
            Some(v) => vec![v.to_string()],
            None => Vec::new(),
        }
    }

    /// A JSON field: `None` when empty; else it has to parse and have
    /// `shape`.
    pub(crate) fn json(&self, k: &str, label: &str, shape: Shape) -> Result<Option<J>> {
        let raw = match self.get(k) {
            None => return Ok(None),
            Some(Value::String(s)) => s.clone(),
            // A JSON value sent as itself.
            Some(v) => v.to_string(),
        };
        if raw.trim().is_empty() {
            return Ok(None);
        }
        let j = J::parse(raw.trim()).map_err(|e| {
            bad(format!(
                "«{label}» no es un JSON válido: {}",
                shown(&e.to_string())
            ))
        })?;
        match (shape, &j) {
            (Shape::Object, J::Obj(_)) | (Shape::Array, J::Arr(_)) => Ok(Some(j)),
            (Shape::Object, _) => Err(bad(format!(
                "«{label}» tiene que ser un objeto JSON ({{ … }})."
            ))),
            (Shape::Array, _) => Err(bad(format!(
                "«{label}» tiene que ser una lista JSON ([ … ])."
            ))),
        }
    }

    /// An Elasticsearch time value (`30s`, `5m`, `1h`, `7d`…).
    pub(crate) fn time(&self, k: &str, label: &str) -> Result<Option<String>> {
        match self.opt(k) {
            None => Ok(None),
            Some(t) if is_time(&t) => Ok(Some(t)),
            Some(t) => Err(bad(format!(
                "«{label}»: «{}» no es un tiempo válido (por ejemplo 30s, 5m, 1h o 7d).",
                shown(&t)
            ))),
        }
    }

    /// A cron expression with `fields` fields (or, with a range, any count
    /// in it).
    pub(crate) fn cron(
        &self,
        k: &str,
        label: &str,
        fields: std::ops::RangeInclusive<usize>,
    ) -> Result<Option<String>> {
        let Some(c) = self.opt(k) else {
            return Ok(None);
        };
        check_cron(&c, label, fields).map(Some)
    }
}

/// A comma- or line-separated list, trimmed, without empties.
pub(crate) fn split_list(s: &str) -> Vec<String> {
    s.split([',', '\n'])
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

pub(crate) fn check_cron(
    c: &str,
    label: &str,
    fields: std::ops::RangeInclusive<usize>,
) -> Result<String> {
    let c = c.trim();
    let n = c.split_whitespace().count();
    if c.chars().any(char::is_control) || !fields.contains(&n) {
        let (a, b) = (fields.start(), fields.end());
        let want = if a == b {
            format!("{a} campos")
        } else {
            format!("de {a} a {b} campos")
        };
        return Err(bad(format!(
            "«{label}»: «{}» no es una expresión cron válida ({want}).",
            shown(c)
        )));
    }
    Ok(c.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// `30s`, `5m`, `1h`, `7d`, `500ms`, `100micros`, `10nanos`.
pub(crate) fn is_time(t: &str) -> bool {
    let digits = t.bytes().take_while(u8::is_ascii_digit).count();
    let (n, unit) = t.split_at(digits);
    !n.is_empty()
        && n.len() <= 12
        && matches!(unit, "nanos" | "micros" | "ms" | "s" | "m" | "h" | "d")
}

/// Milliseconds as the shortest time value (`600000` → `10m`).
pub(crate) fn ms_time(ms: i64) -> String {
    for (n, unit) in [
        (86_400_000, "d"),
        (3_600_000, "h"),
        (60_000, "m"),
        (1000, "s"),
    ] {
        if ms > 0 && ms % n == 0 {
            return format!("{}{unit}", ms / n);
        }
    }
    format!("{ms}ms")
}

/// A number as JSON (whole numbers without a fraction).
pub(crate) fn num_j(f: f64) -> J {
    if f.fract() == 0.0 && f.abs() < 1e15 {
        J::Num((f as i64).into())
    } else {
        serde_json::Number::from_f64(f).map_or(J::Null, J::Num)
    }
}

/// A JSON value as the form's text (pretty), empty when absent.
pub(crate) fn json_text(v: Option<&J>) -> Value {
    match v {
        None | Some(J::Null) => json!(""),
        Some(v) => json!(v.pretty()),
    }
}

/// The same JSON, whatever the order of its objects' keys and however its
/// numbers are written.
pub(crate) fn same(a: &J, b: &J) -> bool {
    match (a, b) {
        (J::Obj(x), J::Obj(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.iter().any(|(k2, v2)| k == k2 && same(v, v2)))
        }
        (J::Arr(x), J::Arr(y)) => x.len() == y.len() && x.iter().zip(y).all(|(a, b)| same(a, b)),
        (J::Num(x), J::Num(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

pub(crate) fn same_opt(a: Option<&J>, b: Option<&J>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => same(a, b),
        _ => false,
    }
}

/// An object from its present entries.
pub(crate) fn obj(entries: Vec<(&str, Option<J>)>) -> J {
    J::Obj(
        entries
            .into_iter()
            .filter_map(|(k, v)| Some((k.to_string(), v?)))
            .collect(),
    )
}

pub(crate) fn s(v: impl Into<String>) -> J {
    J::Str(v.into())
}

pub(crate) fn strs(v: &[String]) -> J {
    J::Arr(v.iter().map(|x| J::Str(x.clone())).collect())
}

/// A JSON list of scalars as the form's comma-separated text.
pub(crate) fn list_text(v: Option<&J>) -> String {
    match v {
        Some(J::Arr(a)) => a.iter().map(J::text).collect::<Vec<_>>().join(", "),
        Some(J::Null) | None => String::new(),
        Some(v) => v.text(),
    }
}

/// An id the user typed: one path segment, at most 255 bytes.
pub(crate) fn plain_id(id: Option<String>, label: &str) -> Result<String> {
    let id = id.ok_or_else(|| bad(format!("Falta «{label}».")))?;
    if one_name(&id) && id.len() <= 255 && !id.starts_with('_') {
        return Ok(id);
    }
    Err(bad(format!(
        "«{label}»: «{}» no es válido (no puede empezar con _, ni tener *, comas o caracteres de control, ni ser solo puntos).",
        shown(&id)
    )))
}

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

/// One request of a change.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Req {
    pub(crate) method: &'static str,
    /// Starts with `/`, the ids percent-encoded.
    pub(crate) path: String,
    pub(crate) body: Option<J>,
    /// A `#` line above it.
    pub(crate) note: Option<String>,
    /// Sent with `if_seq_no` / `if_primary_term` taken from the answer to
    /// the request at this index (the document that request wrote), which
    /// only exist once it ran.
    pub(crate) guard_from: Option<usize>,
    /// What is left on the cluster when this request fails after the
    /// earlier ones were applied.
    pub(crate) if_fails: Option<String>,
}

impl Req {
    pub(crate) fn new(method: &'static str, path: String, body: Option<J>) -> Req {
        Req {
            method,
            path,
            body,
            note: None,
            guard_from: None,
            if_fails: None,
        }
    }

    pub(crate) fn note(mut self, n: &str) -> Req {
        self.note = Some(n.to_string());
        self
    }

    pub(crate) fn guard_from(mut self, i: usize) -> Req {
        self.guard_from = Some(i);
        self
    }

    pub(crate) fn if_fails(mut self, n: String) -> Req {
        self.if_fails = Some(n);
        self
    }
}

/// `path` with the `if_seq_no` / `if_primary_term` of `answer` (a write's
/// reply), or `None` when the reply doesn't carry them as numbers.
pub(crate) fn guarded(path: &str, answer: &J) -> Option<String> {
    let n = |k: &str| {
        text(answer.get(k)).filter(|v| v.len() <= 20 && v.bytes().all(|b| b.is_ascii_digit()))
    };
    Some(format!(
        "{path}?if_seq_no={}&if_primary_term={}",
        n("_seq_no")?,
        n("_primary_term")?
    ))
}

/// What a save or a delete sends.
#[derive(Debug, Clone, Default)]
pub(crate) struct Plan {
    /// What the script does and what it costs, as `#` lines on top.
    pub(crate) notes: Vec<String>,
    pub(crate) reqs: Vec<Req>,
    /// The saved job's id, when it's known before running.
    pub(crate) id: Option<String>,
    /// A new monitor: the request whose answer's `_id` is its id.
    pub(crate) id_from: Option<usize>,
    /// The object's name for "no changes".
    pub(crate) name: String,
}

impl Plan {
    pub(crate) fn render(&self) -> String {
        let mut out = String::new();
        for n in &self.notes {
            out.push_str("# ");
            out.push_str(&one_line(n));
            out.push('\n');
        }
        if self.reqs.is_empty() {
            out.push_str(&format!(
                "# «{}»: no hay cambios que guardar.\n",
                one_line(&self.name)
            ));
            return out;
        }
        if !self.notes.is_empty() {
            out.push('\n');
        }
        for (i, r) in self.reqs.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            if let Some(n) = &r.note {
                out.push_str("# ");
                out.push_str(&one_line(n));
                out.push('\n');
            }
            out.push_str(r.method);
            out.push(' ');
            out.push_str(&r.path);
            out.push('\n');
            if let Some(b) = &r.body {
                out.push_str(&mask(b).pretty());
                out.push('\n');
            }
        }
        out
    }
}

/// Keys whose values are secrets.
fn secret_key(k: &str) -> bool {
    matches!(
        k.to_ascii_lowercase().as_str(),
        "password" | "passphrase" | "secret" | "secret_key" | "api_key" | "authorization"
    )
}

/// The body as the script shows it: secret values masked (the cluster's
/// own redaction marker kept).
pub(crate) fn mask(v: &J) -> J {
    match v {
        J::Obj(o) => J::Obj(
            o.iter()
                .map(|(k, v)| {
                    let v = match v {
                        J::Str(s) if secret_key(k) && s != REDACTED && !s.is_empty() => {
                            J::Str(SECRET_MASK.into())
                        }
                        v => mask(v),
                    };
                    (k.clone(), v)
                })
                .collect(),
        ),
        J::Arr(a) => J::Arr(a.iter().map(mask).collect()),
        v => v.clone(),
    }
}

/// Whether the cluster's redaction marker is somewhere in the body.
pub(crate) fn has_redacted(v: &J) -> bool {
    match v {
        J::Str(s) => s == REDACTED,
        J::Obj(o) => o.iter().any(|(_, v)| has_redacted(v)),
        J::Arr(a) => a.iter().any(has_redacted),
        _ => false,
    }
}

/// Sends the plan's requests in order; the first failure stops the rest.
/// Returns the saved job's id.
async fn run(s: &EsSession, plan: &Plan) -> Result<Option<String>> {
    let mut id = plan.id.clone();
    let mut answers: Vec<J> = Vec::with_capacity(plan.reqs.len());
    for (i, r) in plan.reqs.iter().enumerate() {
        let failed = |why: String| {
            let path = r.path.split('?').next().unwrap_or("");
            bad(match (i, &r.if_fails) {
                (0, _) => why,
                (_, Some(left)) => format!(
                    "{left} El clúster rechazó «{} {path}» después de aplicar los {i} pedidos anteriores: {why}",
                    r.method
                ),
                (_, None) => format!(
                    "Se aplicaron los {i} pedidos anteriores, pero el clúster rechazó «{} {path}»: {why}",
                    r.method
                ),
            })
        };
        let path = match r.guard_from {
            Some(k) => answers
                .get(k)
                .and_then(|a| guarded(&r.path, a))
                .ok_or_else(|| {
                    failed("su respuesta anterior no trajo _seq_no ni _primary_term".into())
                })?,
            None => r.path.clone(),
        };
        let mut rb = s.request(r.method, &path);
        if let Some(b) = &r.body {
            rb = rb
                .header("Content-Type", "application/json")
                .body(b.compact());
        }
        let (status, body) = http::send(rb).await?;
        if status >= 400 {
            return Err(failed(es_error_message(status, &body)));
        }
        let answer = J::parse(&body).unwrap_or(J::Null);
        if plan.id_from == Some(i) {
            if let Some(new) = text(answer.get("_id")) {
                id = Some(Type::Monitor.job_id(&new));
            }
        }
        answers.push(answer);
    }
    Ok(id)
}

#[cfg(test)]
mod tests;
