//! The scheduler designer ([`dbine_driver::Session::scheduler_info`] and
//! the rest) for BigQuery: its scheduled queries, which the BigQuery Data
//! Transfer Service keeps as transfer configs of the `scheduled_query` data
//! source. The tree's jobs are [`crate::jobs`]'s list, with the same ids
//! (the configs' resource names).
//!
//! - One kind, `job`, with everything the console's "Scheduled query"
//!   dialog and the API set: name, location, query, destination (dataset,
//!   table template, write preference, partitioning field, CMEK key),
//!   schedule (the service's own syntax, in UTC, with its start and end; or
//!   on demand), enabled, the owner's failure e-mail, Pub/Sub notifications
//!   and the service account it runs as. What the service fills in (last
//!   state, next run, owner, last error, dataset region) shows read-only.
//! - Scripts are the service's requests, commented (BigQuery's SQL has no
//!   statement for them): `POST …/transferConfigs` creates, `PATCH` with an
//!   `updateMask` of only what changed edits in place (the config keeps its
//!   id and its runs), `DELETE` drops it with its runs and logs. Save sends
//!   the same requests. A "⚠" line, which the app asks the user to accept,
//!   says that a delete takes the runs and logs, and that a query run by
//!   Pub/Sub messages loses that trigger when it moves to a schedule or to
//!   on demand.
//! - "Borrar historial" deletes the config's finished runs (`runs.delete`,
//!   their logs with them). A scheduled query is one query, so there's no
//!   "start at a step"; the service keeps no log of its own beyond each
//!   run's.
//! - "Ejecutar para fechas pasadas" ([`backfill`]) runs it again for past
//!   days or for one past date and time (`startManualRuns`), after a form
//!   that asks which.
//! - Ids are checked segment by segment and escaped, and must name a
//!   scheduled query (another data source's transfer is refused). Every
//!   value is validated before a request is written, the destination
//!   dataset is looked up (it must exist, in the config's location), and
//!   bodies are JSON built here.
//! - The emulator has no Data Transfer Service: no kinds, with a note.

use crate::jobs::{checked, parse_name, text, utc, Dts, Failed, DISABLED, EMULATOR, SOURCE};
use crate::permissions;
use crate::{Api, BigQuerySession, Json};
use dbine_driver::{
    Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerAction,
    SchedulerInfo, SchedulerItem, SchedulerKind, SchedulerList, ServerJob,
};
use reqwest::Method;
use serde_json::{json, Map};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

mod backfill;
pub(crate) use backfill::{action, action_form, action_script};

const TRANSFERS_UPDATE: &str = "bigquery.transfers.update";
/// How long the permission test may take before it's left unknown.
const LIMIT: Duration = Duration::from_secs(20);
/// BigQuery's limit on a query's length.
const MAX_QUERY: usize = 1024 * 1024;
/// Runs read for "Borrar historial", at most.
const MAX_RUNS: usize = 100_000;

const NO_PERMISSION: &str = "Esta cuenta no puede crear, cambiar ni borrar consultas programadas: hace falta el permiso bigquery.transfers.update en el proyecto.";
const READ_ONLY_FORM: &str = "Esta cuenta no puede cambiar consultas programadas (hace falta el permiso bigquery.transfers.update en el proyecto).";
const LIST_DENIED: &str = "Esta cuenta no puede ver las consultas programadas del proyecto (hace falta el permiso bigquery.transfers.get).";
const IN_PLACE: &str = "Los cambios se aplican sobre la consulta programada (PATCH con updateMask de lo que cambió): conserva su id y su historial de ejecuciones.";
const EVENT_NOTE: &str = "Esta consulta programada corre al llegar mensajes de Pub/Sub, una opción que la consola no ofrece: se puede pasar a una programación o a solo a pedido, pero no volver a Pub/Sub desde DBine.";
const NEED_NAME: &str = "La consulta programada necesita un nombre.";
const NEED_QUERY: &str = "La consulta programada necesita una consulta.";
const NEED_LOCATION: &str =
    "Elegí la ubicación de la consulta programada (o un dataset de destino, que la define).";
const BOTH_OR_NEITHER: &str = "Para guardar el resultado hacen falta el dataset y la tabla de destino; para una consulta DDL, DML o un script, dejá los dos vacíos.";
const PARTITION_WITHOUT_TABLE: &str =
    "El campo de partición es de la tabla de destino: sin destino no se usa.";
const NEED_SCHEDULE: &str =
    "Escribí la programación (por ejemplo every 24 hours) o elegí que corra solo a pedido.";
const NO_EVENT: &str = "Las consultas programadas que crea DBine corren según una programación o a pedido, no al llegar mensajes de Pub/Sub.";

// What a script warns about ("⚠" lines: the app lists them and asks to
// continue), each a whole sentence of its own for the message catalog.

/// A delete takes the config's runs and their logs with it.
const RUNS_GO: &str = "Se borran también su historial de ejecuciones y sus registros.";
/// A Pub/Sub-triggered config moved to a schedule or to on demand.
const EVENT_LOST: &str = "La consulta programada deja de correr al llegar mensajes de Pub/Sub, y DBine no puede volver a ponerla así.";

fn bad(m: String) -> Error {
    Error::Query(m)
}

/// Text from the app or the server, short and on one line, for a message
/// or a comment.
fn one(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(120)
        .collect()
}

fn kind(k: &str) -> Result<()> {
    if k.trim() == "job" {
        Ok(())
    } else {
        Err(bad(format!(
            "«{}» no es un tipo de objeto del planificador de BigQuery",
            one(k)
        )))
    }
}

// ---------------------------------------------------------------------------
// Values from the designer
// ---------------------------------------------------------------------------

/// A form's values as the app sent them: untrusted, any JSON type.
#[derive(Clone, Copy)]
struct Vals<'a>(&'a BTreeMap<String, Json>);

impl<'a> Vals<'a> {
    fn get(&self, k: &str) -> Option<&'a Json> {
        self.0.get(k).filter(|v| !v.is_null())
    }

    /// As typed (a query keeps its spaces).
    fn raw(&self, k: &str) -> String {
        match self.get(k) {
            Some(Json::String(s)) => s.clone(),
            Some(Json::Array(_) | Json::Object(_)) | None => String::new(),
            Some(v) => v.to_string(),
        }
    }

    fn opt(&self, k: &str) -> Option<String> {
        Some(self.raw(k).trim().to_string()).filter(|t| !t.is_empty())
    }

    fn bool_or(&self, k: &str, default: bool) -> bool {
        match self.get(k) {
            Some(Json::Bool(b)) => *b,
            Some(Json::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            Some(Json::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => true,
                "false" | "0" | "no" | "off" => false,
                _ => default,
            },
            _ => default,
        }
    }
}

/// One line of text without control characters, at most `max` characters.
fn one_line(label: &str, v: &str, max: usize) -> Result<()> {
    if v.chars().any(char::is_control) {
        return Err(bad(format!("«{label}» tiene que ir en una sola línea.")));
    }
    if v.chars().count() > max {
        return Err(bad(format!("«{label}» admite hasta {max} caracteres.")));
    }
    Ok(())
}

/// A location as the service names it (`us`, `eu`, `southamerica-east1`).
fn location_ok(l: &str) -> bool {
    (2..=64).contains(&l.len())
        && l.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A dataset id: letters, digits and `_`, up to 1024.
pub(crate) fn dataset_ok(d: &str) -> bool {
    (1..=1024).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// A column name.
fn column_ok(c: &str) -> bool {
    let b = c.as_bytes();
    (1..=300).contains(&b.len())
        && (b[0].is_ascii_alphabetic() || b[0] == b'_')
        && b.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_')
}

/// `projects/<project>/topics/<topic>`.
fn topic_ok(t: &str) -> bool {
    match t.split('/').collect::<Vec<_>>().as_slice() {
        ["projects", p, "topics", topic] => {
            (1..=100).contains(&p.len())
                && p.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'.' | b':')
                })
                && (3..=255).contains(&topic.len())
                && topic.as_bytes()[0].is_ascii_alphabetic()
                && topic.bytes().all(|b| {
                    b.is_ascii_alphanumeric()
                        || matches!(b, b'-' | b'_' | b'.' | b'~' | b'+' | b'%')
                })
                && !topic.starts_with("goog")
        }
        _ => false,
    }
}

/// `projects/…/locations/…/keyRings/…/cryptoKeys/…`.
pub(crate) fn kms_ok(k: &str) -> bool {
    let seg = |s: &str| {
        (1..=100).contains(&s.len())
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'))
    };
    matches!(
        k.split('/').collect::<Vec<_>>().as_slice(),
        ["projects", p, "locations", l, "keyRings", r, "cryptoKeys", c]
            if seg(p) && seg(l) && seg(r) && seg(c)
    )
}

/// A service account's (or any) e-mail address.
fn email_ok(e: &str) -> bool {
    let Some((user, domain)) = e.split_once('@') else {
        return false;
    };
    e.len() <= 254
        && !user.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && e.chars()
            .all(|c| c.is_ascii_graphic() && !matches!(c, '/' | '?' | '#' | '&' | '\\'))
        && !domain.contains('@')
}

/// The service's schedule syntax (App Engine cron's): `every 24 hours`,
/// `every 15 minutes`, `every mon,wed 09:00`, `1st,3rd monday of month
/// 15:30`, `every day 03:00`… in UTC. The service has the last word; this
/// refuses what can't be one and runs closer than 5 minutes apart.
fn schedule_check(s: &str) -> Result<String> {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    one_line("Programación", &s, 256)?;
    let invalid = || {
        bad(format!(
            "«{}» no es una programación del servicio de transferencia (por ejemplo: every 24 hours, every mon 09:00, 1st monday of month 15:30).",
            one(&s)
        ))
    };
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | ',' | ':'))
    {
        return Err(invalid());
    }
    let words: Vec<String> = s.split(' ').map(str::to_ascii_lowercase).collect();
    if words.first().map(String::as_str) != Some("every") && !words.iter().any(|w| w == "of") {
        return Err(invalid());
    }
    if words.len() >= 3 && words[0] == "every" {
        if let Ok(n) = words[1].parse::<u64>() {
            let minutes = match words[2].as_str() {
                "minute" | "minutes" | "mins" | "min" => Some(n),
                "hour" | "hours" => Some(n * 60),
                _ => None,
            };
            if n == 0 || minutes.is_some_and(|m| m < 5) {
                return Err(bad(format!(
                    "«{}»: las consultas programadas corren como mucho cada 5 minutos.",
                    one(&s)
                )));
            }
        }
    }
    Ok(s)
}

/// A date-time field: RFC 3339, or the designer's own `YYYY-MM-DDTHH:MM:SS`
/// (read as UTC, as the schedule's times are), as RFC 3339 in UTC.
fn time_value(v: Vals, k: &str, label: &str) -> Result<Option<String>> {
    let Some(t) = v.opt(k) else {
        return Ok(None);
    };
    if let Some(u) = utc(&t) {
        return Ok(Some(u));
    }
    for f in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(n) = chrono::NaiveDateTime::parse_from_str(&t, f) {
            return Ok(Some(n.and_utc().format("%Y-%m-%dT%H:%M:%SZ").to_string()));
        }
    }
    Err(bad(format!(
        "«{label}»: «{}» no es una fecha y hora válida.",
        one(&t)
    )))
}

/// A query-string value, percent-encoded.
fn enc(v: &str) -> String {
    v.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// A scheduled query, as the designer edits it
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    /// On the schedule string (`schedule` / `timeBasedSchedule`).
    #[default]
    Schedule,
    /// Only when started (`disableAutoScheduling` / `manualSchedule`).
    Manual,
    /// On Pub/Sub messages (`eventDrivenSchedule`), only as read.
    Event,
}

impl Mode {
    fn key(self) -> &'static str {
        match self {
            Mode::Schedule => "schedule",
            Mode::Manual => "manual",
            Mode::Event => "event",
        }
    }
}

/// What a scheduled query is (as the server has it, or as the designer
/// wants it).
#[derive(Debug, Clone, PartialEq, Default)]
struct Spec {
    name: String,
    /// Lowercase; empty while a new one waits for its dataset's.
    location: String,
    enabled: bool,
    query: String,
    dataset: Option<String>,
    table: Option<String>,
    write: Option<String>,
    partition: Option<String>,
    kms: Option<String>,
    mode: Mode,
    schedule: Option<String>,
    start: Option<String>,
    end: Option<String>,
    email: bool,
    topic: Option<String>,
    /// The service account it runs as (as the server has it: the owner's
    /// e-mail).
    account: Option<String>,
}

/// A config read from the service.
#[derive(Debug, Clone)]
struct Current {
    /// Its resource name, checked.
    name: String,
    json: Json,
    spec: Spec,
    /// It uses `scheduleOptionsV2` (which can't go with `schedule` /
    /// `scheduleOptions`).
    v2: bool,
}

fn s(v: &Json, ptr: &str) -> Option<String> {
    text(v, ptr)
}

fn param(c: &Json, k: &str) -> Option<String> {
    c.pointer(&format!("/params/{k}"))
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn current_of(c: &Json) -> Option<Current> {
    let name = s(c, "/name")?;
    let [_, location, _] = parse_name(&name)?;
    let location = location.to_ascii_lowercase();
    let v2_set = c
        .get("scheduleOptionsV2")
        .and_then(Json::as_object)
        .is_some_and(|o| !o.is_empty());
    let v2 = v2_set && s(c, "/schedule").is_none() && c.get("scheduleOptions").is_none();
    let mode = if c
        .pointer("/scheduleOptionsV2/eventDrivenSchedule")
        .is_some()
    {
        Mode::Event
    } else if c.pointer("/scheduleOptionsV2/manualSchedule").is_some()
        || c.pointer("/scheduleOptions/disableAutoScheduling")
            .and_then(Json::as_bool)
            == Some(true)
    {
        Mode::Manual
    } else {
        Mode::Schedule
    };
    let time = |a: &str, b: &str| s(c, a).or_else(|| s(c, b)).and_then(|t| utc(&t));
    let spec = Spec {
        name: c
            .get("displayName")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_string(),
        location,
        enabled: c.get("disabled").and_then(Json::as_bool) != Some(true),
        query: c
            .pointer("/params/query")
            .and_then(Json::as_str)
            .unwrap_or("")
            .to_string(),
        dataset: s(c, "/destinationDatasetId"),
        table: param(c, "destination_table_name_template"),
        write: param(c, "write_disposition"),
        partition: param(c, "partitioning_field"),
        kms: s(c, "/encryptionConfiguration/kmsKeyName"),
        mode,
        schedule: s(c, "/scheduleOptionsV2/timeBasedSchedule/schedule")
            .or_else(|| s(c, "/schedule")),
        start: time(
            "/scheduleOptionsV2/timeBasedSchedule/startTime",
            "/scheduleOptions/startTime",
        ),
        end: time(
            "/scheduleOptionsV2/timeBasedSchedule/endTime",
            "/scheduleOptions/endTime",
        ),
        email: c
            .pointer("/emailPreferences/enableFailureEmail")
            .and_then(Json::as_bool)
            .unwrap_or(false),
        topic: s(c, "/notificationPubsubTopic"),
        account: s(c, "/ownerInfo/email"),
    };
    Some(Current {
        name,
        json: c.clone(),
        spec,
        v2,
    })
}

/// The designer's values, checked, as what the scheduled query should be.
/// `cur`: the config being edited (what can't change comes from it).
fn wanted(v: Vals, cur: Option<&Current>, default_location: Option<&str>) -> Result<Spec> {
    let old = cur.map(|c| &c.spec);
    let name = v.opt("display_name").ok_or_else(|| bad(NEED_NAME.into()))?;
    one_line("Nombre", &name, 1024)?;
    let location = match old {
        Some(o) => o.location.clone(),
        None => match v
            .opt("location")
            .or_else(|| default_location.map(str::trim).map(str::to_string))
            .map(|l| l.to_ascii_lowercase())
            .filter(|l| !l.is_empty())
        {
            Some(l) if location_ok(&l) => l,
            Some(l) => {
                return Err(bad(format!(
                    "«{}» no es una ubicación de BigQuery (us, eu, southamerica-east1…).",
                    one(&l)
                )))
            }
            None => String::new(),
        },
    };
    let query = v.raw("query");
    if query.trim().is_empty() {
        return Err(bad(NEED_QUERY.into()));
    }
    if query.len() > MAX_QUERY {
        return Err(bad(format!(
            "La consulta pasa del máximo de BigQuery ({MAX_QUERY} bytes)."
        )));
    }
    let dataset = v.opt("dataset");
    if let Some(d) = &dataset {
        if !dataset_ok(d) {
            return Err(bad(format!(
                "«{}» no es un id de dataset (letras, dígitos y _).",
                one(d)
            )));
        }
    }
    let table = v.opt("table");
    if let Some(t) = &table {
        one_line("Tabla de destino", t, 1024)?;
    }
    if dataset.is_some() != table.is_some() {
        return Err(bad(BOTH_OR_NEITHER.into()));
    }
    let (write, partition) = if dataset.is_some() {
        let write = v
            .opt("write_disposition")
            .unwrap_or_else(|| "WRITE_APPEND".into());
        if !matches!(write.as_str(), "WRITE_APPEND" | "WRITE_TRUNCATE")
            && old.and_then(|o| o.write.as_deref()) != Some(write.as_str())
        {
            return Err(bad(format!(
                "«{}» no es una preferencia de escritura (WRITE_APPEND o WRITE_TRUNCATE).",
                one(&write)
            )));
        }
        let partition = v.opt("partitioning_field");
        if let Some(p) = &partition {
            if !column_ok(p) {
                return Err(bad(format!(
                    "«{}» no es un nombre de columna para particionar.",
                    one(p)
                )));
            }
        }
        (Some(write), partition)
    } else {
        if v.opt("partitioning_field").is_some() {
            return Err(bad(PARTITION_WITHOUT_TABLE.into()));
        }
        (None, None)
    };
    let kms = v.opt("kms_key");
    if let Some(k) = &kms {
        if !kms_ok(k) {
            return Err(bad(format!(
                "«{}» no es una clave de Cloud KMS (projects/…/locations/…/keyRings/…/cryptoKeys/…).",
                one(k)
            )));
        }
    }
    let mode = match v.opt("mode").as_deref() {
        None => old.map_or(Mode::Schedule, |o| o.mode),
        Some("schedule") => Mode::Schedule,
        Some("manual") => Mode::Manual,
        Some("event") if old.is_some_and(|o| o.mode == Mode::Event) => Mode::Event,
        Some("event") => return Err(bad(NO_EVENT.into())),
        Some(other) => {
            return Err(bad(format!(
                "«{}» no es un modo de ejecución (schedule, manual).",
                one(other)
            )))
        }
    };
    let (schedule, start, end) = if mode == Mode::Schedule {
        let schedule =
            schedule_check(&v.opt("schedule").ok_or_else(|| bad(NEED_SCHEDULE.into()))?)?;
        let start = time_value(v, "start_time", "Empieza")?;
        let end = time_value(v, "end_time", "Termina")?;
        if let (Some(a), Some(b)) = (&start, &end) {
            if b <= a {
                return Err(bad(
                    "La programación tiene que terminar después de empezar.".into(),
                ));
            }
        }
        (Some(schedule), start, end)
    } else {
        // Hidden in the other modes: what the server has stays.
        (
            old.and_then(|o| o.schedule.clone()),
            old.and_then(|o| o.start.clone()),
            old.and_then(|o| o.end.clone()),
        )
    };
    let topic = v.opt("pubsub_topic");
    if let Some(t) = &topic {
        if !topic_ok(t) {
            return Err(bad(format!(
                "«{}» no es un tema de Pub/Sub (projects/<proyecto>/topics/<tema>).",
                one(t)
            )));
        }
    }
    let account = match v.opt("service_account") {
        Some(a) if email_ok(&a) => Some(a),
        Some(a) => {
            return Err(bad(format!(
                "«{}» no es el e-mail de una cuenta de servicio.",
                one(&a)
            )))
        }
        // Empty: who it runs as now stays.
        None => old.and_then(|o| o.account.clone()),
    };
    Ok(Spec {
        name,
        location,
        enabled: v.bool_or("enabled", old.is_none_or(|o| o.enabled)),
        query,
        dataset,
        table,
        write,
        partition,
        kms,
        mode,
        schedule,
        start,
        end,
        email: v.bool_or("email_on_failure", old.is_some_and(|o| o.email)),
        topic,
        account,
    })
}

/// `params` with the designer's values over what the config has (other
/// keys kept). An empty value the server keeps as `""` stays.
fn params_of(base: Option<&Json>, w: &Spec) -> Map<String, Json> {
    let mut p = base.and_then(Json::as_object).cloned().unwrap_or_default();
    p.insert("query".into(), json!(w.query));
    for (k, v) in [
        ("destination_table_name_template", &w.table),
        ("write_disposition", &w.write),
        ("partitioning_field", &w.partition),
    ] {
        match v {
            Some(v) => {
                p.insert(k.into(), json!(v));
            }
            None => {
                if p.get(k)
                    .and_then(Json::as_str)
                    .is_some_and(|x| !x.trim().is_empty())
                {
                    p.remove(k);
                }
            }
        }
    }
    p
}

fn schedule_options(w: &Spec) -> Map<String, Json> {
    let mut o = Map::new();
    if w.mode == Mode::Manual {
        o.insert("disableAutoScheduling".into(), json!(true));
    }
    if let Some(t) = &w.start {
        o.insert("startTime".into(), json!(t));
    }
    if let Some(t) = &w.end {
        o.insert("endTime".into(), json!(t));
    }
    o
}

fn schedule_options_v2(w: &Spec, cur: &Json) -> Json {
    match w.mode {
        Mode::Schedule => {
            let mut t = Map::new();
            if let Some(s) = &w.schedule {
                t.insert("schedule".into(), json!(s));
            }
            if let Some(x) = &w.start {
                t.insert("startTime".into(), json!(x));
            }
            if let Some(x) = &w.end {
                t.insert("endTime".into(), json!(x));
            }
            json!({ "timeBasedSchedule": t })
        }
        Mode::Manual => json!({ "manualSchedule": {} }),
        Mode::Event => cur.get("scheduleOptionsV2").cloned().unwrap_or(json!({})),
    }
}

/// The body that creates it.
fn create_body(w: &Spec) -> Json {
    let mut b = Map::new();
    b.insert("displayName".into(), json!(w.name));
    b.insert("dataSourceId".into(), json!(SOURCE));
    if let Some(d) = &w.dataset {
        b.insert("destinationDatasetId".into(), json!(d));
    }
    b.insert("params".into(), Json::Object(params_of(None, w)));
    if w.mode == Mode::Schedule {
        if let Some(s) = &w.schedule {
            b.insert("schedule".into(), json!(s));
        }
    }
    let opts = schedule_options(w);
    if !opts.is_empty() {
        b.insert("scheduleOptions".into(), Json::Object(opts));
    }
    b.insert("disabled".into(), json!(!w.enabled));
    b.insert(
        "emailPreferences".into(),
        json!({ "enableFailureEmail": w.email }),
    );
    if let Some(t) = &w.topic {
        b.insert("notificationPubsubTopic".into(), json!(t));
    }
    if let Some(k) = &w.kms {
        b.insert("encryptionConfiguration".into(), json!({ "kmsKeyName": k }));
    }
    Json::Object(b)
}

/// What an edit changes: the `updateMask` paths, the body with only those
/// fields, and the service account when it changes.
fn diff(cur: &Current, w: &Spec) -> (Vec<&'static str>, Map<String, Json>, Option<String>) {
    let o = &cur.spec;
    let mut mask = Vec::new();
    let mut body = Map::new();
    if w.name != o.name {
        mask.push("display_name");
        body.insert("displayName".into(), json!(w.name));
    }
    if w.dataset != o.dataset {
        mask.push("destination_dataset_id");
        body.insert(
            "destinationDatasetId".into(),
            json!(w.dataset.clone().unwrap_or_default()),
        );
    }
    let old_params = cur.json.get("params");
    let params = params_of(old_params, w);
    if Some(&Json::Object(params.clone())) != old_params {
        mask.push("params");
        body.insert("params".into(), Json::Object(params));
    }
    let timing = |x: &Spec| (x.mode, x.schedule.clone(), x.start.clone(), x.end.clone());
    if timing(w) != timing(o) {
        if cur.v2 {
            mask.push("schedule_options_v2");
            body.insert(
                "scheduleOptionsV2".into(),
                schedule_options_v2(w, &cur.json),
            );
        } else {
            if w.mode == Mode::Schedule && w.schedule != o.schedule {
                mask.push("schedule");
                body.insert("schedule".into(), json!(w.schedule));
            }
            let (new, old) = (schedule_options(w), schedule_options(o));
            if new != old {
                mask.push("schedule_options");
                body.insert("scheduleOptions".into(), Json::Object(new));
            }
        }
    }
    if w.enabled != o.enabled {
        mask.push("disabled");
        body.insert("disabled".into(), json!(!w.enabled));
    }
    if w.email != o.email {
        mask.push("email_preferences");
        body.insert(
            "emailPreferences".into(),
            json!({ "enableFailureEmail": w.email }),
        );
    }
    if w.topic != o.topic {
        mask.push("notification_pubsub_topic");
        body.insert(
            "notificationPubsubTopic".into(),
            json!(w.topic.clone().unwrap_or_default()),
        );
    }
    if w.kms != o.kms {
        mask.push("encryption_configuration");
        body.insert(
            "encryptionConfiguration".into(),
            match &w.kms {
                Some(k) => json!({ "kmsKeyName": k }),
                None => json!({}),
            },
        );
    }
    let account = w.account.clone().filter(|a| {
        !o.account
            .as_deref()
            .is_some_and(|b| b.eq_ignore_ascii_case(a))
    });
    if account.is_some() {
        mask.push("service_account_name");
    }
    (mask, body, account)
}

// ---------------------------------------------------------------------------
// Requests and scripts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct Req {
    method: Method,
    url: reqwest::Url,
    body: Option<Json>,
}

/// What a change sends: the requests (none when nothing differs), the
/// script's header and the object's id when it exists.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    lines: Vec<String>,
    reqs: Vec<Req>,
    id: Option<String>,
    /// What the user loses with it, one whole Spanish sentence each: the
    /// script's "⚠" lines.
    warnings: Vec<String>,
}

impl Plan {
    /// The requests as a commented script: BigQuery's SQL has no statement
    /// for a scheduled query, so it reads and does nothing in the editor.
    fn script(&self) -> String {
        let mut out = String::new();
        for l in &self.lines {
            out.push_str(&format!("# {l}\n"));
        }
        // Fixed sentences (never cut: the app matches them whole).
        for w in &self.warnings {
            out.push_str(&format!("# ⚠ {w}\n"));
        }
        for r in &self.reqs {
            out.push_str(&format!("#\n# {} {}\n", r.method, r.url));
            if let Some(b) = &r.body {
                let pretty = serde_json::to_string_pretty(b).unwrap_or_default();
                for line in pretty.lines() {
                    out.push_str(&format!("# {line}\n"));
                }
            }
        }
        out
    }
}

/// `url` with a query string written here (values encoded).
fn with_query(mut url: reqwest::Url, q: &[(&str, String)]) -> reqwest::Url {
    if !q.is_empty() {
        let s = q
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        url.set_query(Some(&s));
    }
    url
}

fn create_plan(dts: &Dts<'_>, project: &str, w: &Spec) -> Result<Plan> {
    let url = dts.url(&[
        "projects",
        project,
        "locations",
        &w.location,
        "transferConfigs",
    ])?;
    let q: Vec<(&str, String)> = w
        .account
        .iter()
        .map(|a| ("serviceAccountName", enc(a)))
        .collect();
    let mut lines = vec![format!(
        "Crea la consulta programada «{}» de BigQuery en {} (servicio de transferencia de datos).",
        one(&w.name),
        w.location
    )];
    if w.account.is_none() {
        lines.push("Corre con las credenciales de la cuenta de esta conexión.".into());
    }
    Ok(Plan {
        lines,
        reqs: vec![Req {
            method: Method::POST,
            url: with_query(url, &q),
            body: Some(create_body(w)),
        }],
        id: None,
        warnings: Vec::new(),
    })
}

fn patch_plan(dts: &Dts<'_>, cur: &Current, w: &Spec) -> Result<Plan> {
    let (mask, body, account) = diff(cur, w);
    if mask.is_empty() {
        return Ok(Plan {
            lines: vec![format!(
                "«{}»: no hay cambios que guardar.",
                one(&cur.spec.name)
            )],
            reqs: Vec::new(),
            id: Some(cur.name.clone()),
            warnings: Vec::new(),
        });
    }
    let parts = checked(&cur.name)?;
    let url = dts.url(&Dts::config_segments(&parts))?;
    let mut q = vec![("updateMask", mask.join(","))];
    if let Some(a) = &account {
        q.push(("serviceAccountName", enc(a)));
    }
    let mut lines = vec![format!(
        "Cambia la consulta programada «{}» de BigQuery: {}.",
        one(&cur.spec.name),
        mask.join(", ")
    )];
    lines.push(IN_PLACE.into());
    if account.is_some() {
        lines.push(
            "Pasa a correr con las credenciales de la cuenta de servicio (hace falta poder actuar como ella).".into(),
        );
    }
    let mut warnings = Vec::new();
    if cur.spec.mode == Mode::Event && w.mode != Mode::Event {
        warnings.push(EVENT_LOST.to_string());
    }
    Ok(Plan {
        lines,
        reqs: vec![Req {
            method: Method::PATCH,
            url: with_query(url, &q),
            body: Some(Json::Object(body)),
        }],
        id: Some(cur.name.clone()),
        warnings,
    })
}

fn delete_plan(dts: &Dts<'_>, cur: &Current) -> Result<Plan> {
    let parts = checked(&cur.name)?;
    Ok(Plan {
        lines: vec![format!(
            "Borra la consulta programada «{}» de BigQuery.",
            one(&cur.spec.name)
        )],
        reqs: vec![Req {
            method: Method::DELETE,
            url: dts.url(&Dts::config_segments(&parts))?,
            body: None,
        }],
        id: Some(cur.name.clone()),
        warnings: vec![RUNS_GO.to_string()],
    })
}

/// Sends the plan's requests in order; the last one's answer.
async fn send(dts: &Dts<'_>, plan: &Plan) -> Result<Option<Json>> {
    let mut last = None;
    for r in &plan.reqs {
        let mut req = dts.api.http.request(r.method.clone(), r.url.clone());
        if let Some(b) = &r.body {
            req = req.json(b);
        }
        last = Some(dts.send(req).await?);
    }
    Ok(last)
}

// ---------------------------------------------------------------------------
// Reading the service
// ---------------------------------------------------------------------------

/// The config `id`, which must be a scheduled query.
async fn read(dts: &Dts<'_>, id: &str) -> Result<Current> {
    let parts = checked(id)?;
    let c = dts.config(&parts).await?;
    if c.get("dataSourceId").and_then(Json::as_str) != Some(SOURCE) {
        return Err(bad(format!(
            "«{}» no es una consulta programada: DBine solo administra las consultas programadas del servicio de transferencia.",
            one(id)
        )));
    }
    current_of(&c).ok_or_else(|| {
        bad(format!(
            "el servicio no devolvió la consulta programada {}",
            parts[2]
        ))
    })
}

/// The project's datasets: id and location (lowercase).
async fn datasets(api: &Api) -> Result<Vec<(String, String)>> {
    Ok(api
        .list_all(&["datasets"], "datasets")
        .await?
        .iter()
        .filter_map(|d| {
            Some((
                s(d, "/datasetReference/datasetId")?,
                s(d, "/location").unwrap_or_default().to_ascii_lowercase(),
            ))
        })
        .collect())
}

/// The dataset's location (lowercase); an error when it doesn't exist.
async fn dataset_location(api: &Api, dataset: &str) -> Result<String> {
    match api.get(&["datasets", dataset], &[]).await {
        Ok(d) => Ok(s(&d, "/location").unwrap_or_default().to_ascii_lowercase()),
        Err(e) => Err(bad(format!(
            "El dataset de destino «{}» no existe en el proyecto o esta cuenta no lo puede ver: {e}",
            one(dataset)
        ))),
    }
}

/// Whether the account may change transfer configs (`None`: the test
/// couldn't run; the service decides then).
async fn may_change(api: &Api, crm: &str) -> Option<bool> {
    let url = format!("{crm}/{}:testIamPermissions", api.project);
    let test = api.send(
        api.http
            .post(url)
            .json(&permissions::request(&[TRANSFERS_UPDATE])),
    );
    match tokio::time::timeout(LIMIT, test).await {
        Ok(Ok(r)) => Some(permissions::granted(&r).contains(TRANSFERS_UPDATE)),
        Ok(Err(e)) => {
            tracing::debug!("bigquery: transfers permission not tested: {e}");
            None
        }
        Err(_) => None,
    }
}

/// One list call: whether the service is on and readable.
async fn probe(dts: &Dts<'_>, project: &str, location: &str) -> std::result::Result<(), Failed> {
    let url = dts.url(&[
        "projects",
        project,
        "locations",
        location,
        "transferConfigs",
    ])?;
    dts.send(
        dts.api
            .http
            .get(url)
            .query(&[("dataSourceIds", SOURCE), ("pageSize", "1")]),
    )
    .await
    .map(|_| ())
}

/// The plan for a change, checked against the service.
async fn plan(api: &Api, dts: &Dts<'_>, id: Option<&str>, values: &FormValues) -> Result<Plan> {
    let cur = match id {
        Some(id) => Some(read(dts, id).await?),
        None => None,
    };
    let mut w = wanted(Vals(&values.values), cur.as_ref(), api.location.as_deref())?;
    let changed_dataset = w.dataset.clone().filter(|d| {
        cur.as_ref()
            .is_none_or(|c| c.spec.dataset.as_ref() != Some(d))
    });
    if let Some(d) = changed_dataset {
        let at = dataset_location(api, &d).await?;
        if w.location.is_empty() {
            w.location = at;
        } else if !at.is_empty() && at != w.location {
            return Err(bad(format!(
                "El dataset «{}» está en {at} y la consulta programada en {}: tienen que estar en la misma ubicación.",
                one(&d),
                w.location
            )));
        }
    }
    if w.location.is_empty() || !location_ok(&w.location) {
        return Err(bad(NEED_LOCATION.into()));
    }
    match &cur {
        None => create_plan(dts, &api.project, &w),
        Some(c) => patch_plan(dts, c, &w),
    }
}

// ---------------------------------------------------------------------------
// The designer's form
// ---------------------------------------------------------------------------

fn form_of(
    cur: Option<&Current>,
    datasets: &[(String, String)],
    default_location: Option<&str>,
) -> ObjectForm {
    let existing = cur.is_some();
    let spec = cur.map(|c| c.spec.clone()).unwrap_or_else(|| Spec {
        location: default_location
            .map(|l| l.trim().to_ascii_lowercase())
            .unwrap_or_default(),
        enabled: true,
        write: Some("WRITE_APPEND".into()),
        schedule: Some("every 24 hours".into()),
        ..Spec::default()
    });
    let mut f = ObjectForm::default();
    let opt = |v: &Option<String>| v.as_ref().map_or(Json::Null, |x| json!(x));
    let vals = [
        ("display_name", json!(spec.name)),
        (
            "location",
            if spec.location.is_empty() {
                Json::Null
            } else {
                json!(spec.location)
            },
        ),
        ("enabled", json!(spec.enabled)),
        ("query", json!(spec.query)),
        ("dataset", opt(&spec.dataset)),
        ("table", json!(spec.table.clone().unwrap_or_default())),
        (
            "write_disposition",
            json!(spec.write.clone().unwrap_or_else(|| "WRITE_APPEND".into())),
        ),
        (
            "partitioning_field",
            json!(spec.partition.clone().unwrap_or_default()),
        ),
        ("kms_key", json!(spec.kms.clone().unwrap_or_default())),
        ("mode", json!(spec.mode.key())),
        ("schedule", json!(spec.schedule.clone().unwrap_or_default())),
        ("start_time", opt(&spec.start)),
        ("end_time", opt(&spec.end)),
        ("email_on_failure", json!(spec.email)),
        (
            "pubsub_topic",
            json!(spec.topic.clone().unwrap_or_default()),
        ),
        (
            "service_account",
            // A service account shows (and stays unless changed); a user
            // doesn't go in a field that takes service accounts.
            json!(spec
                .account
                .clone()
                .filter(|a| a.ends_with(".gserviceaccount.com"))
                .unwrap_or_default()),
        ),
    ];
    for (k, v) in vals {
        f.values.insert(k.into(), v);
    }

    let mut locations: BTreeSet<String> = datasets
        .iter()
        .map(|(_, l)| l.clone())
        .filter(|l| !l.is_empty())
        .collect();
    if !spec.location.is_empty() {
        locations.insert(spec.location.clone());
    }
    if locations.is_empty() {
        locations.extend(["us".to_string(), "eu".to_string()]);
    }
    f.choices.insert(
        "location".into(),
        locations.into_iter().map(|l| (l.clone(), l)).collect(),
    );
    let mut ds: Vec<(String, String)> = datasets
        .iter()
        .map(|(d, l)| {
            (
                d.clone(),
                if l.is_empty() {
                    d.clone()
                } else {
                    format!("{d} ({l})")
                },
            )
        })
        .collect();
    if let Some(d) = &spec.dataset {
        if !ds.iter().any(|(x, _)| x == d) {
            ds.push((d.clone(), d.clone()));
        }
    }
    f.choices.insert("dataset".into(), ds);

    let mode = if spec.mode == Mode::Event {
        FormKind::select(&[
            ("schedule", "Según una programación"),
            ("manual", "Solo a pedido"),
            ("event", "Al llegar mensajes de Pub/Sub"),
        ])
    } else {
        FormKind::select(&[
            ("schedule", "Según una programación"),
            ("manual", "Solo a pedido"),
        ])
    };
    let mut sections = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("display_name", "Nombre", FormKind::Text).required(),
            FormField::new("location", "Ubicación", FormKind::reference("location", false))
                .read_only(existing)
                .help("La región donde corre la consulta; la del dataset de destino si tiene uno. No se puede cambiar después."),
            FormField::new("enabled", "Habilitada", FormKind::Bool)
                .default(json!(true))
                .help("Deshabilitada no corre según su programación, pero se puede ejecutar a pedido."),
            FormField::new("query", "Consulta", FormKind::code("sql"))
                .required()
                .help("GoogleSQL. Cada ejecución recibe @run_time (TIMESTAMP, en UTC) y @run_date (DATE). Una consulta DDL, DML o un script no lleva destino."),
        ]),
        FormSection::new("destination", "Destino").fields(vec![
            FormField::new("dataset", "Dataset de destino", FormKind::reference("dataset", false))
                .help("Vacío para consultas DDL, DML o scripts, que no escriben un resultado."),
            FormField::new("table", "Tabla de destino", FormKind::Text)
                .placeholder("ventas_{run_date}")
                .help("Admite plantillas: {run_date}, {run_time|\"%Y%m%d\"}; tabla${run_date} escribe en la partición de ese día."),
            FormField::new(
                "write_disposition",
                "Si la tabla ya existe",
                FormKind::select(&[
                    ("WRITE_APPEND", "Agregar las filas"),
                    ("WRITE_TRUNCATE", "Reemplazar sus datos"),
                ]),
            )
            .default(json!("WRITE_APPEND")),
            FormField::new("partitioning_field", "Campo de partición", FormKind::Text)
                .help("Columna TIMESTAMP o DATE por la que se particiona la tabla de destino. Vacío: sin partición, o por fecha de ingesta con tabla$aaaammdd."),
            FormField::new("kms_key", "Clave de cifrado (CMEK)", FormKind::Text)
                .placeholder("projects/p/locations/us/keyRings/r/cryptoKeys/k")
                .help("Vacía: la predeterminada del proyecto, o la de Google."),
        ]),
        FormSection::new("schedule", "Programación").fields(vec![
            FormField::new("mode", "Corre", mode).default(json!("schedule")),
            FormField::new("schedule", "Programación", FormKind::Text)
                .placeholder("every 24 hours")
                .when("mode", &["schedule"])
                .default(json!("every 24 hours"))
                .help("En UTC, con la sintaxis del servicio: every 24 hours, every 15 minutes, every mon,wed 09:00, every day 03:00, 1st,3rd monday of month 15:30. Como mucho cada 5 minutos."),
            FormField::new("start_time", "Empieza", FormKind::DateTime)
                .when("mode", &["schedule"])
                .help("En UTC. Vacío: ahora."),
            FormField::new("end_time", "Termina", FormKind::DateTime)
                .when("mode", &["schedule"])
                .help("En UTC. Vacío: nunca."),
        ]),
        FormSection::new("notifications", "Notificaciones").fields(vec![
            FormField::new("email_on_failure", "Avisar por correo al dueño si falla", FormKind::Bool)
                .default(json!(false)),
            FormField::new("pubsub_topic", "Tema de Pub/Sub", FormKind::Text)
                .placeholder("projects/mi-proyecto/topics/consultas")
                .help("Recibe un mensaje cuando termina cada ejecución."),
        ]),
        FormSection::new("credentials", "Credenciales").fields(vec![
            FormField::new("service_account", "Cuenta de servicio", FormKind::Text)
                .placeholder("cuenta@proyecto.iam.gserviceaccount.com")
                .help("Corre con sus credenciales (hace falta poder actuar como ella). Vacía al crearla: corre como la cuenta de esta conexión; vacía al editarla: no cambia."),
        ]),
    ];
    if let Some(c) = cur {
        if spec.mode == Mode::Event {
            f.values.insert(
                "subscription".into(),
                json!(s(
                    &c.json,
                    "/scheduleOptionsV2/eventDrivenSchedule/pubsubSubscription"
                )
                .unwrap_or_default()),
            );
            sections[2].fields.push(
                FormField::new("subscription", "Suscripción de Pub/Sub", FormKind::Text)
                    .when("mode", &["event"])
                    .read_only(true),
            );
            f.notes.push(EVENT_NOTE.into());
        }
        f.values.insert(
            "owner".into(),
            json!(spec.account.clone().unwrap_or_default()),
        );
        sections[4].fields.insert(
            0,
            FormField::new("owner", "Corre con las credenciales de", FormKind::Text)
                .read_only(true),
        );
        for (k, ptr) in [
            ("state", "/state"),
            ("next_run", "/nextRunTime"),
            ("updated", "/updateTime"),
            ("error", "/error/message"),
            ("region", "/datasetRegion"),
            ("config_id", "/name"),
        ] {
            let v = s(&c.json, ptr);
            let v = match k {
                "next_run" | "updated" => v.and_then(|t| utc(&t)),
                _ => v,
            };
            f.values
                .insert(k.into(), v.map_or(Json::Null, |x| json!(x)));
        }
        sections.push(FormSection::new("status", "Estado").fields(vec![
            FormField::new(
                "state",
                "Última ejecución",
                FormKind::select(&[
                    ("SUCCEEDED", "Correcta"),
                    ("FAILED", "Falló"),
                    ("CANCELLED", "Cancelada"),
                    ("RUNNING", "En curso"),
                    ("PENDING", "Pendiente"),
                ]),
            )
            .read_only(true),
            FormField::new("next_run", "Próxima ejecución", FormKind::DateTime).read_only(true),
            FormField::new("updated", "Modificada", FormKind::DateTime).read_only(true),
            FormField::new("error", "Último error", FormKind::Textarea).read_only(true),
            FormField::new("region", "Región del dataset", FormKind::Text).read_only(true),
            FormField::new("config_id", "Id", FormKind::Text).read_only(true),
        ]));
        f.notes.push(IN_PLACE.into());
    }
    f.sections = sections;
    f
}

// ---------------------------------------------------------------------------
// The contract's calls
// ---------------------------------------------------------------------------

fn item_of(j: ServerJob) -> SchedulerItem {
    let at = j.group.clone().unwrap_or_default();
    SchedulerItem {
        description: Some(match &j.schedule {
            Some(s) => format!("En {at}: {s}"),
            None => format!("En {at}: solo a pedido"),
        }),
        enabled: Some(j.enabled),
        system: false,
        id: j.id,
        name: j.name,
        can_edit: None,
        can_delete: None,
        actions: None,
        run_from_step: None,
        clear_history: None,
    }
}

pub(crate) async fn info(s: &BigQuerySession) -> Result<SchedulerInfo> {
    if s.api.emulator {
        return Ok(SchedulerInfo {
            kinds: Vec::new(),
            logs: false,
            notes: vec![EMULATOR.into()],
        });
    }
    info_with(&s.api, &s.dts()?, permissions::CRM).await
}

/// What the service and the account allow: `crm` is Resource Manager's
/// projects endpoint.
async fn info_with(api: &Api, dts: &Dts<'_>, crm: &str) -> Result<SchedulerInfo> {
    let location = api
        .location
        .as_deref()
        .map(|l| l.trim().to_ascii_lowercase())
        .filter(|l| location_ok(l))
        .unwrap_or_else(|| "us".into());
    let mut notes = Vec::new();
    match probe(dts, &api.project, &location).await {
        Ok(()) => {}
        Err(f) if f.disabled => {
            return Ok(SchedulerInfo {
                kinds: Vec::new(),
                logs: false,
                notes: vec![DISABLED.into()],
            })
        }
        Err(f) if f.status == 403 => notes.push(LIST_DENIED.into()),
        Err(f) => tracing::debug!("bigquery: scheduled queries not probed: {}", f.error),
    }
    let may = may_change(api, crm).await.unwrap_or(true);
    if !may {
        notes.push(NO_PERMISSION.into());
    }
    // A scheduled query is one query: no step to start at, and "Ejecutar
    // ahora" is the jobs list's manual run (`startManualRuns`). Its own
    // action runs it for past dates (the same call with a time range or a
    // past run time), which `bigquery.transfers.update` allows, as it does
    // deleting its finished runs (`runs.delete`; one going on stays).
    let mut job = SchedulerKind::new("job", "Consultas programadas", "consulta programada")
        .abilities(may, may, may)
        .run_from_step(false)
        .clear_history(may);
    if may {
        job = job.action(
            SchedulerAction::new(backfill::ACTION, "Ejecutar para fechas pasadas").with_params(),
        );
    }
    Ok(SchedulerInfo {
        kinds: vec![job],
        logs: false,
        notes,
    })
}

pub(crate) async fn items(s: &BigQuerySession, k: &str) -> Result<SchedulerList> {
    kind(k)?;
    let list = s.jobs_list().await?;
    Ok(SchedulerList {
        items: list.jobs.into_iter().map(item_of).collect(),
        notes: list.notes,
    })
}

pub(crate) async fn form(s: &BigQuerySession, k: &str, id: Option<&str>) -> Result<ObjectForm> {
    kind(k)?;
    let dts = s.dts()?;
    let cur = match id {
        Some(id) => Some(read(&dts, id).await?),
        None => None,
    };
    let (sets, note) = match datasets(&s.api).await {
        Ok(d) => (d, None),
        Err(e) => (
            Vec::new(),
            Some(format!(
                "No se pudieron leer los datasets del proyecto: {e}"
            )),
        ),
    };
    let mut f = form_of(cur.as_ref(), &sets, s.api.location.as_deref());
    f.notes.extend(note);
    if may_change(&s.api, permissions::CRM).await == Some(false) {
        f.read_only = true;
        f.notes.insert(0, READ_ONLY_FORM.into());
    }
    Ok(f)
}

pub(crate) async fn script(
    s: &BigQuerySession,
    k: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<String> {
    kind(k)?;
    let dts = s.dts()?;
    Ok(plan(&s.api, &dts, id, values).await?.script())
}

pub(crate) async fn save(
    s: &BigQuerySession,
    k: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Option<String>> {
    kind(k)?;
    let dts = s.dts()?;
    save_with(&s.api, &dts, id, values).await
}

async fn save_with(
    api: &Api,
    dts: &Dts<'_>,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Option<String>> {
    let p = plan(api, dts, id, values).await?;
    let answer = send(dts, &p).await?;
    if p.id.is_some() {
        return Ok(p.id);
    }
    Ok(answer
        .as_ref()
        .and_then(|a| s(a, "/name"))
        .filter(|n| parse_name(n).is_some()))
}

pub(crate) async fn delete_script(s: &BigQuerySession, k: &str, id: &str) -> Result<String> {
    kind(k)?;
    let dts = s.dts()?;
    let cur = read(&dts, id).await?;
    Ok(delete_plan(&dts, &cur)?.script())
}

pub(crate) async fn delete(s: &BigQuerySession, k: &str, id: &str) -> Result<()> {
    kind(k)?;
    let dts = s.dts()?;
    delete_with(&dts, id).await
}

async fn delete_with(dts: &Dts<'_>, id: &str) -> Result<()> {
    let cur = read(dts, id).await?;
    send(dts, &delete_plan(dts, &cur)?).await.map(|_| ())
}

pub(crate) async fn clear_history(s: &BigQuerySession, id: &str) -> Result<()> {
    let dts = s.dts()?;
    clear_with(&dts, id).await.map(|_| ())
}

/// A run's id, from its name (`…/transferConfigs/<config>/runs/<id>`), when
/// it is one of `config`'s.
fn run_id<'a>(name: &'a str, config: &str) -> Option<&'a str> {
    let ok = |s: &str| {
        (1..=128).contains(&s.len())
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    match name.split('/').collect::<Vec<_>>().as_slice() {
        ["projects", p, "locations", l, "transferConfigs", c, "runs", id]
            if ok(p) && ok(l) && *c == config && ok(id) =>
        {
            Some(id)
        }
        _ => None,
    }
}

/// Deletes the config's finished runs (a run going on stays); how many.
async fn clear_with(dts: &Dts<'_>, id: &str) -> Result<usize> {
    read(dts, id).await?;
    let parts = checked(id)?;
    let mut segments = Dts::config_segments(&parts).to_vec();
    segments.push("runs");
    let mut runs: Vec<Json> = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let mut q = vec![("pageSize", "1000".to_string())];
        if let Some(t) = token.take() {
            q.push(("pageToken", t));
        }
        let url = dts.url(&segments)?;
        let page = dts.send(dts.api.http.get(url).query(&q)).await?;
        runs.extend(
            page.get("transferRuns")
                .and_then(Json::as_array)
                .into_iter()
                .flatten()
                .cloned(),
        );
        match page.get("nextPageToken").and_then(Json::as_str) {
            Some(t) if !t.is_empty() && runs.len() < MAX_RUNS => token = Some(t.to_string()),
            _ => break,
        }
    }
    let done: Vec<String> = runs
        .iter()
        .filter(|r| !matches!(s(r, "/state").as_deref(), Some("RUNNING" | "PENDING")))
        .filter_map(|r| s(r, "/name"))
        .filter_map(|n| run_id(&n, parts[2]).map(str::to_string))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    for (i, run) in done.iter().enumerate() {
        let mut path = segments.clone();
        path.push(run);
        let url = dts.url(&path)?;
        if let Err(f) = dts.send(dts.api.http.delete(url)).await {
            return Err(bad(format!(
                "Se borraron {i} de {} ejecuciones de la consulta programada; el servicio rechazó la siguiente: {}",
                done.len(),
                f.error
            )));
        }
    }
    Ok(done.len())
}

#[cfg(test)]
mod tests;
