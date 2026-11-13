//! The scheduler designer ([`dbine_driver::Session::scheduler_info`] and
//! the rest) for Cloud Spanner: the backup schedules of the instance's
//! databases (Database Admin API, `…/databases/{db}/backupSchedules`), the
//! same ones [`crate::jobs`] lists, with the same ids (`<database>/<id>`).
//!
//! - One kind, `job`, with everything a schedule has: its database and id,
//!   full or incremental backups, the cron (UTC), how long each backup is
//!   kept, and the encryption (the database's, Google's, or customer keys
//!   in Cloud KMS). What Spanner fills in (time zone, creation window, last
//!   change) shows read-only.
//! - Scripts are the Admin API's requests, commented (Spanner's SQL has no
//!   statement for a schedule): `POST …/backupSchedules?backupScheduleId=`
//!   creates, `PATCH` with an `updateMask` of only what changed edits in
//!   place, `DELETE` drops it (the backups it made stay until they expire).
//!   Spanner doesn't change a schedule's backup type: that edit deletes
//!   the schedule and creates it again with the same id, said at the top of
//!   the script and in the form. Save sends the same requests.
//! - Checked before a request: ids (Spanner's id rule), that the database
//!   exists, at most four schedules per database, the cron (five fields in
//!   range; full backups at least 12 hours apart, incremental ones 4), the
//!   retention (6 hours to 366 days), the KMS keys, and incremental backups
//!   only on an Enterprise or Enterprise Plus instance.
//! - There's no run, start at a step, history to clear (the history is the
//!   backups themselves) or scheduler log.
//! - The emulator has no backups (it answers UNIMPLEMENTED): no kinds, with
//!   a note.

use crate::jobs::{self, parse_id, s, spanner_id, utc, DENIED, EMULATOR};
use crate::{Json, SpannerSession};
use dbine_driver::{
    Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerInfo,
    SchedulerItem, SchedulerKind, SchedulerList, ServerJob,
};
use reqwest::Method;
use serde_json::{json, Map};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

const CREATE: &str = "spanner.backupSchedules.create";
const UPDATE: &str = "spanner.backupSchedules.update";
const DELETE: &str = "spanner.backupSchedules.delete";
/// Spanner's limit.
const MAX_PER_DATABASE: usize = 4;
const MIN_RETENTION: u64 = 6 * 3600;
const MAX_RETENTION: u64 = 366 * 86_400;
const DEFAULT_RETENTION: u64 = 7 * 86_400;
const LIMIT: Duration = Duration::from_secs(20);

const DATABASE_ENCRYPTION: &str = "USE_DATABASE_ENCRYPTION";
const GOOGLE_ENCRYPTION: &str = "GOOGLE_DEFAULT_ENCRYPTION";
const CUSTOMER_ENCRYPTION: &str = "CUSTOMER_MANAGED_ENCRYPTION";

const NO_CREATE: &str = "Este usuario no puede crear programas de backup (hace falta el permiso spanner.backupSchedules.create).";
const NO_UPDATE: &str = "Este usuario no puede cambiar programas de backup (hace falta el permiso spanner.backupSchedules.update).";
const NO_DELETE: &str = "Este usuario no puede borrar programas de backup (hace falta el permiso spanner.backupSchedules.delete).";
const STANDARD: &str = "La instancia es de la edición Standard: sus programas solo hacen backups completos (los incrementales necesitan Enterprise o Enterprise Plus).";
const TYPE_CHANGE: &str = "Spanner no cambia el tipo de backup de un programa: para cambiarlo, DBine lo borra y lo crea de nuevo con el mismo id. Los backups que ya hizo se conservan; se pierde la fecha de su último cambio y, si era incremental, su cadena (la próxima empieza con un backup completo).";
const CHAIN: &str = "Cambiar un programa incremental hace que Spanner empiece una cadena nueva, con un backup completo.";
const NEW_DATABASE: &str = "Los programas de una base recién creada pueden tardar hasta 24 horas en hacer su primer backup.";
const NO_HISTORY: &str = "El historial de un programa de backup son los backups que hizo: se borran desde los backups de la base, no desde el programa.";

fn bad(m: String) -> Error {
    Error::Query(m)
}

/// Text from the app, short and on one line, for a message or a comment.
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
            "«{}» no es un tipo de objeto del planificador de Spanner",
            one(k)
        )))
    }
}

/// The emulator's UNIMPLEMENTED as what it means.
fn emulator(e: Error) -> Error {
    if jobs::unimplemented(&e) {
        Error::Unsupported(EMULATOR.into())
    } else {
        e
    }
}

/// A list read only to check a value: the emulator stops the change, any
/// other failure (a permission the login lacks) leaves the check to
/// Spanner.
fn unread(e: Error) -> Result<()> {
    if jobs::unimplemented(&e) {
        return Err(Error::Unsupported(EMULATOR.into()));
    }
    tracing::debug!("spanner: a list for a check not read: {e}");
    Ok(())
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

    fn opt(&self, k: &str) -> Option<String> {
        match self.get(k) {
            Some(Json::String(s)) => Some(s.trim().to_string()),
            Some(Json::Number(n)) => Some(n.to_string()),
            _ => None,
        }
        .filter(|t| !t.is_empty())
    }

    /// Whole seconds; `None` when missing.
    fn seconds(&self, k: &str, label: &str) -> Result<Option<u64>> {
        let n = match self.get(k) {
            None => return Ok(None),
            Some(Json::String(s)) if s.trim().is_empty() => return Ok(None),
            Some(Json::Number(n)) => n.as_u64().or_else(|| {
                n.as_f64()
                    .filter(|f| f.fract() == 0.0 && *f >= 0.0)
                    .map(|f| f as u64)
            }),
            Some(Json::String(s)) => s.trim().parse::<u64>().ok(),
            Some(_) => None,
        };
        n.map(Some)
            .ok_or_else(|| bad(format!("«{label}» tiene que ser una cantidad de segundos.")))
    }
}

/// `projects/…/locations/…/keyRings/…/cryptoKeys/…`.
fn kms_ok(k: &str) -> bool {
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

/// A new schedule's id: Spanner's rule, 2 to 60 characters.
fn new_id_ok(id: &str) -> bool {
    id.len() >= 2 && spanner_id(id)
}

fn number(s: &str) -> Option<u32> {
    (!s.is_empty() && s.len() <= 2 && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

/// One cron field as the values it matches (`*`, `n`, `a-b`, lists, and
/// steps `*/n`, `a/n`, `a-b/n`); `None` when it isn't one.
fn cron_field(f: &str, min: u32, max: u32) -> Option<BTreeSet<u32>> {
    let mut out = BTreeSet::new();
    for part in f.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, st)) => (r, Some(number(st).filter(|n| *n > 0)?)),
            None => (part, None),
        };
        let (a, b) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            (number(a)?, number(b)?)
        } else {
            let a = number(range)?;
            (a, if step.is_some() { max } else { a })
        };
        if a < min || b > max || a > b {
            return None;
        }
        out.extend((a..=b).step_by(step.unwrap_or(1) as usize));
    }
    Some(out)
}

/// The cron as Spanner takes it (five fields, UTC), checked: each field in
/// range, and backups no closer than Spanner allows (12 hours for full
/// ones, 4 for incremental ones).
fn cron_check(text: &str, incremental: bool) -> Result<String> {
    let fields: Vec<&str> = text.split_whitespace().collect();
    let shown = one(text);
    if fields.len() != 5 {
        return Err(bad(format!(
            "«{shown}» no es una expresión cron de cinco campos (minuto hora día mes día-de-la-semana)."
        )));
    }
    let names = ["minuto", "hora", "día del mes", "mes", "día de la semana"];
    let ranges = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];
    let mut sets = Vec::new();
    for ((f, name), (min, max)) in fields.iter().zip(names).zip(ranges) {
        match cron_field(f, min, max) {
            Some(set) => sets.push(set),
            None => {
                return Err(bad(format!(
                    "«{shown}»: el campo {name} («{}») no es válido.",
                    one(f)
                )))
            }
        }
    }
    let hours: Vec<u32> = sets[1].iter().copied().collect();
    let gap = if sets[0].len() > 1 {
        0
    } else {
        let mut gap = 24;
        for w in hours.windows(2) {
            gap = gap.min(w[1] - w[0]);
        }
        if let (Some(first), Some(last)) = (hours.first(), hours.last()) {
            if hours.len() > 1 {
                gap = gap.min(24 - last + first);
            }
        }
        gap
    };
    let need = if incremental { 4 } else { 12 };
    if gap < need {
        return Err(bad(if incremental {
            format!("«{shown}»: Spanner hace los backups incrementales como mucho cada 4 horas.")
        } else {
            format!("«{shown}»: Spanner hace los backups completos como mucho cada 12 horas.")
        }));
    }
    Ok(fields.join(" "))
}

/// A Duration (`604800s`, `3.5s`) in whole seconds.
fn secs_of(d: &str) -> Option<u64> {
    d.trim()
        .strip_suffix('s')?
        .parse::<f64>()
        .ok()
        .filter(|f| f.is_finite() && *f >= 0.0)
        .map(|f| f as u64)
}

// ---------------------------------------------------------------------------
// A backup schedule, as the designer edits it
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default)]
struct Spec {
    db: String,
    id: String,
    incremental: bool,
    cron: String,
    retention: u64,
    /// The API's encryption type.
    encryption: String,
    /// Cloud KMS keys (customer-managed encryption only).
    keys: Vec<String>,
}

impl Spec {
    fn encryption_config(&self) -> Json {
        if self.encryption == CUSTOMER_ENCRYPTION {
            json!({ "encryptionType": CUSTOMER_ENCRYPTION, "kmsKeyNames": self.keys })
        } else {
            json!({ "encryptionType": self.encryption })
        }
    }

    fn sorted_keys(&self) -> Vec<String> {
        let mut k = self.keys.clone();
        k.sort();
        k
    }
}

/// A schedule read from Spanner.
#[derive(Debug, Clone)]
struct Current {
    json: Json,
    spec: Spec,
}

fn current_of(sc: &Json) -> Option<Current> {
    let name = s(sc, "/name")?;
    let parts: Vec<&str> = name.split('/').collect();
    let (db, id) = match parts.as_slice() {
        [.., "databases", db, "backupSchedules", id] if spanner_id(db) && spanner_id(id) => {
            (db.to_string(), id.to_string())
        }
        _ => return None,
    };
    let keys = sc
        .pointer("/encryptionConfig/kmsKeyNames")
        .and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Json::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter(|k| !k.is_empty())
        .or_else(|| s(sc, "/encryptionConfig/kmsKeyName").map(|k| vec![k]))
        .unwrap_or_default();
    Some(Current {
        json: sc.clone(),
        spec: Spec {
            db,
            id,
            incremental: sc.get("incrementalBackupSpec").is_some(),
            cron: s(sc, "/spec/cronSpec/text").unwrap_or_default(),
            retention: s(sc, "/retentionDuration")
                .as_deref()
                .and_then(secs_of)
                .unwrap_or(0),
            encryption: s(sc, "/encryptionConfig/encryptionType")
                .filter(|e| e != "ENCRYPTION_TYPE_UNSPECIFIED")
                .unwrap_or_else(|| DATABASE_ENCRYPTION.into()),
            keys,
        },
    })
}

/// The designer's values, checked, as what the schedule should be. `cur`:
/// the schedule being edited (its database and id can't change).
fn wanted(v: Vals, cur: Option<&Current>, default_db: &str) -> Result<Spec> {
    let old = cur.map(|c| &c.spec);
    let (db, id) = match old {
        Some(o) => (o.db.clone(), o.id.clone()),
        None => {
            let db = v.opt("database").unwrap_or_else(|| default_db.to_string());
            if !spanner_id(&db) {
                return Err(bad(format!(
                    "«{}» no es el id de una base de Spanner.",
                    one(&db)
                )));
            }
            let id = v
                .opt("schedule_id")
                .ok_or_else(|| bad("El programa de backup necesita un id.".into()))?;
            if !new_id_ok(&id) {
                return Err(bad(format!(
                    "«{}» no es un id de programa de backup: letras minúsculas, dígitos, _ y -, que empiece con una letra y termine con letra o dígito, de 2 a 60 caracteres.",
                    one(&id)
                )));
            }
            (db, id)
        }
    };
    let incremental = match v.opt("backup_type").as_deref() {
        None => old.is_some_and(|o| o.incremental),
        Some("full") => false,
        Some("incremental") => true,
        Some(other) => {
            return Err(bad(format!(
                "«{}» no es un tipo de backup (full o incremental).",
                one(other)
            )))
        }
    };
    let cron = v
        .opt("cron")
        .ok_or_else(|| bad("El programa de backup necesita un cron.".into()))?;
    let cron = cron_check(&cron, incremental)?;
    let retention = v
        .seconds("retention", "Se guarda")?
        .ok_or_else(|| bad("Falta cuánto tiempo se guarda cada backup.".into()))?;
    if !(MIN_RETENTION..=MAX_RETENTION).contains(&retention) {
        return Err(bad(format!(
            "Spanner guarda un backup entre 6 horas y 366 días ({MIN_RETENTION} a {MAX_RETENTION} segundos); se pidieron {retention}."
        )));
    }
    let encryption = v
        .opt("encryption")
        .unwrap_or_else(|| DATABASE_ENCRYPTION.into());
    if ![DATABASE_ENCRYPTION, GOOGLE_ENCRYPTION, CUSTOMER_ENCRYPTION].contains(&encryption.as_str())
    {
        return Err(bad(format!(
            "«{}» no es un tipo de cifrado de Spanner.",
            one(&encryption)
        )));
    }
    let keys: Vec<String> = if encryption == CUSTOMER_ENCRYPTION {
        let keys: Vec<String> = v
            .opt("kms_keys")
            .unwrap_or_default()
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|k| !k.is_empty())
            .map(str::to_string)
            .collect();
        if keys.is_empty() {
            return Err(bad(
                "Con claves del cliente hace falta al menos una clave de Cloud KMS.".into(),
            ));
        }
        if let Some(k) = keys.iter().find(|k| !kms_ok(k)) {
            return Err(bad(format!(
                "«{}» no es una clave de Cloud KMS (projects/…/locations/…/keyRings/…/cryptoKeys/…).",
                one(k)
            )));
        }
        keys
    } else {
        Vec::new()
    };
    Ok(Spec {
        db,
        id,
        incremental,
        cron,
        retention,
        encryption,
        keys,
    })
}

/// The body that creates it.
fn create_body(w: &Spec) -> Json {
    let mut b = Map::new();
    b.insert("spec".into(), json!({ "cronSpec": { "text": w.cron } }));
    b.insert(
        "retentionDuration".into(),
        json!(format!("{}s", w.retention)),
    );
    b.insert("encryptionConfig".into(), w.encryption_config());
    if w.incremental {
        b.insert("incrementalBackupSpec".into(), json!({}));
    } else {
        b.insert("fullBackupSpec".into(), json!({}));
    }
    Json::Object(b)
}

/// What an edit in place changes: the `updateMask` paths and the body with
/// only those fields.
fn diff(o: &Spec, w: &Spec) -> (Vec<&'static str>, Map<String, Json>) {
    let mut mask = Vec::new();
    let mut body = Map::new();
    if w.cron != o.cron {
        mask.push("spec.cron_spec.text");
        body.insert("spec".into(), json!({ "cronSpec": { "text": w.cron } }));
    }
    if w.retention != o.retention {
        mask.push("retention_duration");
        body.insert(
            "retentionDuration".into(),
            json!(format!("{}s", w.retention)),
        );
    }
    if w.encryption != o.encryption
        || (w.encryption == CUSTOMER_ENCRYPTION && w.sorted_keys() != o.sorted_keys())
    {
        mask.push("encryption_config");
        body.insert("encryptionConfig".into(), w.encryption_config());
    }
    (mask, body)
}

// ---------------------------------------------------------------------------
// Requests and scripts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Req {
    method: Method,
    /// After `<base>/v1/`, with its query.
    path: String,
    body: Option<Json>,
}

/// What a change sends: the requests (none when nothing differs), the
/// script's header, the schedule's id afterwards, and whether the first
/// request deletes what the second creates again.
#[derive(Debug, Clone)]
struct Plan {
    lines: Vec<String>,
    reqs: Vec<Req>,
    id: String,
    recreate: bool,
}

impl Plan {
    /// The requests as a commented script (Spanner's SQL has no statement
    /// for a backup schedule).
    fn script(&self, base: &str) -> String {
        let mut out = String::new();
        for l in &self.lines {
            out.push_str(&format!("# {l}\n"));
        }
        for r in &self.reqs {
            out.push_str(&format!("#\n# {} {base}/v1/{}\n", r.method, r.path));
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

fn schedules_path(instance: &str, db: &str) -> String {
    format!("{instance}/databases/{db}/backupSchedules")
}

fn create_req(instance: &str, w: &Spec) -> Req {
    Req {
        method: Method::POST,
        path: format!(
            "{}?backupScheduleId={}",
            schedules_path(instance, &w.db),
            w.id
        ),
        body: Some(create_body(w)),
    }
}

fn delete_req(instance: &str, o: &Spec) -> Req {
    Req {
        method: Method::DELETE,
        path: format!("{}/{}", schedules_path(instance, &o.db), o.id),
        body: None,
    }
}

fn create_plan(instance: &str, w: &Spec) -> Plan {
    Plan {
        lines: vec![format!(
            "Crea el programa de backup «{}» de la base «{}» de Spanner: backups {} con el cron {} (UTC).",
            w.id,
            w.db,
            if w.incremental { "incrementales" } else { "completos" },
            w.cron
        )],
        reqs: vec![create_req(instance, w)],
        id: format!("{}/{}", w.db, w.id),
        recreate: false,
    }
}

fn edit_plan(instance: &str, o: &Spec, w: &Spec) -> Plan {
    let id = format!("{}/{}", o.db, o.id);
    if w.incremental != o.incremental {
        return Plan {
            lines: vec![
                format!(
                    "Cambia el programa de backup «{}» de la base «{}» de Spanner de backups {} a {}.",
                    o.id,
                    o.db,
                    if o.incremental { "incrementales" } else { "completos" },
                    if w.incremental { "incrementales" } else { "completos" },
                ),
                TYPE_CHANGE.into(),
            ],
            reqs: vec![delete_req(instance, o), create_req(instance, w)],
            id,
            recreate: true,
        };
    }
    let (mask, body) = diff(o, w);
    if mask.is_empty() {
        return Plan {
            lines: vec![format!("«{}»: no hay cambios que guardar.", o.id)],
            reqs: Vec::new(),
            id,
            recreate: false,
        };
    }
    let mut lines = vec![format!(
        "Cambia el programa de backup «{}» de la base «{}» de Spanner: {}.",
        o.id,
        o.db,
        mask.join(", ")
    )];
    if o.incremental {
        lines.push(CHAIN.into());
    }
    Plan {
        lines,
        reqs: vec![Req {
            method: Method::PATCH,
            path: format!(
                "{}/{}?updateMask={}",
                schedules_path(instance, &o.db),
                o.id,
                mask.join(",")
            ),
            body: Some(Json::Object(body)),
        }],
        id,
        recreate: false,
    }
}

fn delete_plan(instance: &str, o: &Spec) -> Plan {
    Plan {
        lines: vec![format!(
            "Borra el programa de backup «{}» de la base «{}» de Spanner. Los backups que ya hizo se conservan hasta que vencen.",
            o.id, o.db
        )],
        reqs: vec![delete_req(instance, o)],
        id: format!("{}/{}", o.db, o.id),
        recreate: false,
    }
}

// ---------------------------------------------------------------------------
// Reading Spanner
// ---------------------------------------------------------------------------

impl SpannerSession {
    /// The connection's database id.
    fn database_id(&self) -> &str {
        jobs::short(&self.database)
    }

    /// One schedule, as the designer edits it.
    async fn read_schedule(&self, id: &str) -> Result<Current> {
        let (db, sid) = parse_id(id)?;
        let sc = self.schedule(&db, &sid).await.map_err(emulator)?;
        current_of(&sc).ok_or_else(|| {
            bad(format!(
                "no existe el programa de backup «{}» o este usuario no lo puede ver",
                one(id)
            ))
        })
    }

    /// The instance's edition (`STANDARD`, `ENTERPRISE`, `ENTERPRISE_PLUS`),
    /// when Spanner says it.
    async fn edition(&self) -> Option<String> {
        match self.api.get(&self.instance).await {
            Ok(i) => s(&i, "/edition").filter(|e| e != "EDITION_UNSPECIFIED"),
            Err(e) => {
                tracing::debug!("spanner: instance not read for its edition: {e}");
                None
            }
        }
    }

    /// The permissions granted on a database (`None`: the test couldn't
    /// run; Spanner decides then).
    async fn granted(&self, db: &str, perms: &[&str]) -> Option<BTreeSet<String>> {
        let resource = format!("{}/databases/{db}", self.instance);
        tokio::time::timeout(LIMIT, self.test_iam(&resource, perms))
            .await
            .ok()
            .flatten()
    }

    /// The plan for a change, checked against Spanner.
    async fn plan(&self, id: Option<&str>, values: &FormValues) -> Result<Plan> {
        let cur = match id {
            Some(id) => Some(self.read_schedule(id).await?),
            None => None,
        };
        let w = wanted(Vals(&values.values), cur.as_ref(), self.database_id())?;
        let becomes_incremental =
            w.incremental && !cur.as_ref().is_some_and(|c| c.spec.incremental);
        if becomes_incremental && self.edition().await.as_deref() == Some("STANDARD") {
            return Err(bad(STANDARD.into()));
        }
        match &cur {
            Some(c) => Ok(edit_plan(&self.instance, &c.spec, &w)),
            None => {
                // A list the login can't read leaves that check to Spanner;
                // only the emulator's answer stops here.
                match self.list_database_ids().await {
                    Ok(dbs) if !dbs.contains(&w.db) => {
                        return Err(bad(format!(
                            "La base «{}» no existe en la instancia.",
                            w.db
                        )))
                    }
                    Ok(_) => {}
                    Err(e) => unread(e)?,
                }
                match self.schedules_of(&w.db).await {
                    Ok(theirs) => {
                        let ids: Vec<String> = theirs
                            .iter()
                            .filter_map(|sc| s(sc, "/name"))
                            .map(|n| jobs::short(&n).to_string())
                            .collect();
                        if ids.contains(&w.id) {
                            return Err(bad(format!(
                                "Ya existe el programa de backup «{}» en la base «{}».",
                                w.id, w.db
                            )));
                        }
                        if ids.len() >= MAX_PER_DATABASE {
                            return Err(bad(format!(
                                "La base «{}» ya tiene {MAX_PER_DATABASE} programas de backup, el máximo de Spanner.",
                                w.db
                            )));
                        }
                    }
                    Err(e) => unread(e)?,
                }
                Ok(create_plan(&self.instance, &w))
            }
        }
    }

    /// Sends the plan's requests in order.
    async fn send_plan(&self, plan: &Plan) -> Result<()> {
        for (i, r) in plan.reqs.iter().enumerate() {
            let url = format!("{}/v1/{}", self.api.base, r.path);
            let mut req = self.api.http.request(r.method.clone(), url);
            if let Some(b) = &r.body {
                req = req.json(b);
            }
            if let Err(e) = self.api.send(req).await.map_err(emulator) {
                return Err(if plan.recreate && i > 0 {
                    bad(format!(
                        "Se borró el programa de backup «{}» para cambiar su tipo, pero Spanner no aceptó crearlo de nuevo: {e}",
                        plan.id
                    ))
                } else {
                    e
                });
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The designer's form
// ---------------------------------------------------------------------------

fn form_of(cur: Option<&Current>, databases: &[String], default_db: &str) -> ObjectForm {
    let existing = cur.is_some();
    let spec = cur.map(|c| c.spec.clone()).unwrap_or_else(|| Spec {
        db: default_db.to_string(),
        cron: "0 2 * * *".into(),
        retention: DEFAULT_RETENTION,
        encryption: DATABASE_ENCRYPTION.into(),
        ..Spec::default()
    });
    let mut f = ObjectForm::default();
    for (k, v) in [
        ("database", json!(spec.db)),
        ("schedule_id", json!(spec.id)),
        (
            "backup_type",
            json!(if spec.incremental {
                "incremental"
            } else {
                "full"
            }),
        ),
        ("cron", json!(spec.cron)),
        ("retention", json!(spec.retention)),
        ("encryption", json!(spec.encryption)),
        ("kms_keys", json!(spec.keys.join("\n"))),
    ] {
        f.values.insert(k.into(), v);
    }
    let mut dbs: Vec<String> = databases
        .iter()
        .filter(|d| spanner_id(d))
        .cloned()
        .collect();
    if !dbs.contains(&spec.db) {
        dbs.push(spec.db.clone());
    }
    f.choices.insert(
        "database".into(),
        dbs.into_iter().map(|d| (d.clone(), d)).collect(),
    );
    let mut sections = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("database", "Base de datos", FormKind::reference("database", false))
                .required()
                .read_only(existing),
            FormField::new("schedule_id", "Id del programa", FormKind::Text)
                .required()
                .read_only(existing)
                .placeholder("diario")
                .help("Letras minúsculas, dígitos, _ y -; empieza con una letra y termina con letra o dígito; de 2 a 60 caracteres. No se puede cambiar después."),
            FormField::new(
                "backup_type",
                "Tipo de backup",
                FormKind::select(&[("full", "Completo"), ("incremental", "Incremental")]),
            )
            .default(json!("full"))
            .help("Los incrementales guardan solo lo que cambió desde el anterior de su cadena (hasta 14 backups por cadena, el primero completo); necesitan la edición Enterprise o Enterprise Plus."),
        ]),
        FormSection::new("schedule", "Programación").fields(vec![
            FormField::new("cron", "Cron", FormKind::Cron)
                .required()
                .placeholder("0 2 * * *")
                .default(json!("0 2 * * *"))
                .help("Cinco campos en UTC: minuto, hora, día del mes, mes y día de la semana. Los backups completos van al menos cada 12 horas y los incrementales cada 4. Ejemplos: 0 2 * * * (todos los días a las 2), 0 2/12 * * *, 0 2 * * 0 (los domingos), 0 2 8 * * (el 8 de cada mes)."),
            FormField::new("retention", "Se guarda", FormKind::Duration)
                .required()
                .default(json!(DEFAULT_RETENTION))
                .help("Cuánto se guarda cada backup desde que se hace: entre 6 horas y 366 días."),
        ]),
        FormSection::new("encryption", "Cifrado").fields(vec![
            FormField::new(
                "encryption",
                "Cifrado",
                FormKind::select(&[
                    ("USE_DATABASE_ENCRYPTION", "El de la base de datos"),
                    ("GOOGLE_DEFAULT_ENCRYPTION", "Clave administrada por Google"),
                    ("CUSTOMER_MANAGED_ENCRYPTION", "Claves administradas por el cliente (CMEK)"),
                ]),
            )
            .default(json!(DATABASE_ENCRYPTION)),
            FormField::new("kms_keys", "Claves de Cloud KMS", FormKind::Textarea)
                .when("encryption", &["CUSTOMER_MANAGED_ENCRYPTION"])
                .placeholder("projects/p/locations/us-central1/keyRings/r/cryptoKeys/k")
                .help("Una por línea. En una instancia regional, una clave de su región; en una multirregional, las que cubran todas sus regiones."),
        ]),
    ];
    if let Some(c) = cur {
        let creation = s(&c.json, "/spec/cronSpec/creationWindow")
            .as_deref()
            .and_then(secs_of);
        for (k, v) in [
            (
                "time_zone",
                s(&c.json, "/spec/cronSpec/timeZone").map(|z| json!(z)),
            ),
            ("creation_window", creation.map(|w| json!(w))),
            ("updated", utc(s(&c.json, "/updateTime")).map(|t| json!(t))),
        ] {
            f.values.insert(k.into(), v.unwrap_or(Json::Null));
        }
        sections.push(FormSection::new("status", "Estado").fields(vec![
            FormField::new("time_zone", "Zona horaria", FormKind::Text).read_only(true),
            FormField::new("creation_window", "Ventana para empezar", FormKind::Duration)
                .read_only(true)
                .help("Spanner empieza cada backup dentro de este plazo desde la hora del cron; el backup tiene los datos de esa hora."),
            FormField::new("updated", "Modificado", FormKind::DateTime).read_only(true),
        ]));
        f.notes.push(TYPE_CHANGE.into());
        if c.spec.incremental {
            f.notes.push(CHAIN.into());
        }
    } else {
        f.notes.push(NEW_DATABASE.into());
    }
    f.sections = sections;
    f
}

// ---------------------------------------------------------------------------
// The contract's calls
// ---------------------------------------------------------------------------

fn item_of(j: ServerJob) -> SchedulerItem {
    let db = j.group.clone().unwrap_or_default();
    let cron = j.schedule.clone().unwrap_or_default();
    let incremental = j.description.as_deref() == Some("Backup incremental");
    SchedulerItem {
        description: Some(if incremental {
            format!("Backup incremental de «{db}», cron {cron} (UTC)")
        } else {
            format!("Backup completo de «{db}», cron {cron} (UTC)")
        }),
        enabled: None,
        system: false,
        id: j.id,
        name: j.name,
        can_edit: None,
        can_delete: None,
        actions: None,
    }
}

pub(crate) async fn info(s: &SpannerSession) -> Result<SchedulerInfo> {
    let mut notes = Vec::new();
    s.api
        .last_status
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    let probe = s
        .api
        .get(&format!("{}/backupSchedules?pageSize=1", s.database))
        .await;
    if let Err(e) = probe {
        if jobs::unimplemented(&e) {
            return Ok(SchedulerInfo {
                kinds: Vec::new(),
                logs: false,
                notes: vec![EMULATOR.into()],
            });
        }
        let status = s
            .api
            .last_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        notes.push(if jobs::denied(status.as_deref(), &e) {
            DENIED.to_string()
        } else {
            format!(
                "No se pudieron leer los programas de backup de la base «{}»: {e}",
                s.database_id()
            )
        });
    }
    let granted = s.granted(s.database_id(), &[CREATE, UPDATE, DELETE]).await;
    let has = |p: &str| granted.as_ref().is_none_or(|g| g.contains(p));
    let (c, u, d) = (has(CREATE), has(UPDATE), has(DELETE));
    for (ok, note) in [(c, NO_CREATE), (u, NO_UPDATE), (d, NO_DELETE)] {
        if !ok {
            notes.push(note.into());
        }
    }
    if s.edition().await.as_deref() == Some("STANDARD") {
        notes.push(STANDARD.into());
    }
    // A backup schedule has no steps, no run call and no history but its
    // backups (which clearing would delete): no start at a step, no
    // clearing, no action.
    Ok(SchedulerInfo {
        kinds: vec![
            SchedulerKind::new("job", "Programas de backup", "programa de backup")
                .abilities(c, u, d)
                .run_from_step(false)
                .clear_history(false),
        ],
        logs: false,
        notes,
    })
}

pub(crate) async fn items(s: &mut SpannerSession, k: &str) -> Result<SchedulerList> {
    kind(k)?;
    let list = s.server_jobs().await?;
    Ok(SchedulerList {
        items: list.jobs.into_iter().map(item_of).collect(),
        notes: list.notes,
    })
}

pub(crate) async fn form(s: &SpannerSession, k: &str, id: Option<&str>) -> Result<ObjectForm> {
    kind(k)?;
    let cur = match id {
        Some(id) => Some(s.read_schedule(id).await?),
        None => {
            // The emulator answers here what it answers everywhere.
            if let Err(e) = s.schedules_of(s.database_id()).await {
                unread(e)?;
            }
            None
        }
    };
    let dbs = match s.list_database_ids().await {
        Ok(dbs) => dbs,
        Err(e) => {
            unread(e)?;
            Vec::new()
        }
    };
    let mut f = form_of(cur.as_ref(), &dbs, s.database_id());
    let (db, need, note) = match &cur {
        Some(c) => (c.spec.db.as_str(), UPDATE, NO_UPDATE),
        None => (s.database_id(), CREATE, NO_CREATE),
    };
    if s.granted(db, &[need])
        .await
        .is_some_and(|g| !g.contains(need))
    {
        f.read_only = true;
        f.notes.insert(0, note.into());
    }
    if s.edition().await.as_deref() == Some("STANDARD") {
        f.notes.push(STANDARD.into());
    }
    Ok(f)
}

pub(crate) async fn script(
    s: &SpannerSession,
    k: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<String> {
    kind(k)?;
    Ok(s.plan(id, values).await?.script(&s.api.base))
}

pub(crate) async fn save(
    s: &SpannerSession,
    k: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Option<String>> {
    kind(k)?;
    let plan = s.plan(id, values).await?;
    s.send_plan(&plan).await?;
    Ok(Some(plan.id))
}

pub(crate) async fn delete_script(s: &SpannerSession, k: &str, id: &str) -> Result<String> {
    kind(k)?;
    let cur = s.read_schedule(id).await?;
    Ok(delete_plan(&s.instance, &cur.spec).script(&s.api.base))
}

pub(crate) async fn delete(s: &SpannerSession, k: &str, id: &str) -> Result<()> {
    kind(k)?;
    let cur = s.read_schedule(id).await?;
    s.send_plan(&delete_plan(&s.instance, &cur.spec)).await
}

/// The history is the backups themselves: not cleared from here.
pub(crate) async fn clear_history(id: &str) -> Result<()> {
    parse_id(id)?;
    Err(Error::Unsupported(NO_HISTORY.into()))
}

#[cfg(test)]
mod tests;
