//! Server jobs ([`dbine_driver::Session::jobs`]): Spanner's backup
//! schedules (Database Admin API, `…/databases/{db}/backupSchedules`), of
//! every database of the connection's instance. Spanner makes a backup of
//! the database on each one's cron (always in UTC), full or incremental,
//! and keeps it for its retention.
//!
//! The API has no call to take a schedule's backup now, to stop it or to
//! pause a schedule: there's only the list, the detail, the history and
//! the script. The history is the backups the schedule made (each backup
//! names its schedules in `backupSchedules`), newest first: `CREATING`
//! while it's being made, `READY` after. A backup that failed leaves no
//! backup, so failures don't show. The script is the `gcloud` command that
//! creates the schedule, with the API's JSON after it as a comment.
//!
//! The emulator has no backups (it answers UNIMPLEMENTED): its list is
//! empty with a note. Ids are `<database>/<schedule>`, both checked
//! against Spanner's id rule before they go into a path.

use crate::monitor::rfc3339;
use crate::profiler::epoch_ms;
use crate::SpannerSession;
use dbine_driver::jobs::MAX_JOB_RUNS;
use dbine_driver::{
    Error, JobDetail, JobList, JobOutcome, JobRun, JobStep, PropertyInfo, Result, ServerJob,
};
use serde_json::{Map, Value as Json};

const KIND: &str = "Backup schedule";
/// Databases whose schedules are read.
const MAX_DATABASES: usize = 500;
/// Backups read for a history, at most.
const MAX_BACKUPS: usize = 2000;

pub(crate) const EMULATOR: &str = "El emulador de Spanner no tiene backups ni programas de backup.";
pub(crate) const DENIED: &str = "Este usuario no puede ver los programas de backup (hace falta el permiso spanner.backupSchedules.list).";
const NOTE: &str = "Spanner no permite hacer ahora el backup de un programa, detenerlo ni pausarlo. El historial son los backups que hizo el programa y que siguen guardados: un backup que falló no deja rastro.";

/// A database or schedule id: a lowercase letter, then lowercase letters,
/// digits, `_` and `-`, ending in a letter or digit; 60 characters at
/// most.
pub(crate) fn spanner_id(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=60).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'_' | b'-'))
        && b[b.len() - 1].is_ascii_alphanumeric()
}

pub(crate) fn parse_id(id: &str) -> Result<(String, String)> {
    id.split_once('/')
        .filter(|(db, sc)| spanner_id(db) && spanner_id(sc))
        .map(|(db, sc)| (db.to_string(), sc.to_string()))
        .ok_or_else(|| {
            Error::Query(format!(
                "«{}» no es un id de programa de backup de Spanner (se espera «base/programa»)",
                id.chars().take(200).collect::<String>()
            ))
        })
}

pub(crate) fn short(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

pub(crate) fn s(v: &Json, path: &str) -> Option<String> {
    v.pointer(path)
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
}

pub(crate) fn utc(t: Option<String>) -> Option<String> {
    let ms = epoch_ms(t.as_deref()?)?;
    u64::try_from(ms.div_euclid(1000)).ok().map(rfc3339)
}

/// `604800s` → `7d`, `14400s` → `4h`: the largest whole unit (gcloud's
/// duration form).
pub(crate) fn duration_text(d: &str) -> String {
    let Some(secs) = d
        .trim()
        .strip_suffix('s')
        .and_then(|n| n.parse::<f64>().ok())
        .filter(|n| n.fract() == 0.0 && *n >= 0.0)
        .map(|n| n as u64)
    else {
        return d.to_string();
    };
    for (unit, size) in [("d", 86_400), ("h", 3600), ("m", 60)] {
        if secs > 0 && secs % size == 0 {
            return format!("{}{unit}", secs / size);
        }
    }
    format!("{secs}s")
}

fn full(sc: &Json) -> bool {
    sc.get("incrementalBackupSpec").is_none()
}

/// A backup schedule as a job, with its backups newest first (see
/// [`backups_of`]): the last run is the newest `READY` one, and it's
/// running while one is `CREATING`.
pub(crate) fn job_of(sc: &Json, mine: &[&Json]) -> Option<ServerJob> {
    let name = s(sc, "/name")?;
    let parts: Vec<&str> = name.split('/').collect();
    // projects/P/instances/I/databases/D/backupSchedules/S
    let (db, id) = match parts.as_slice() {
        [.., "databases", db, "backupSchedules", id] => (*db, *id),
        _ => return None,
    };
    if !spanner_id(db) || !spanner_id(id) {
        return None;
    }
    let state = |b: &Json| s(b, "/state");
    let creating = mine
        .iter()
        .find(|b| state(b).as_deref() == Some("CREATING"));
    let running = creating.is_some();
    let last = mine.iter().find(|b| state(b).as_deref() == Some("READY"));
    Some(ServerJob {
        id: format!("{db}/{id}"),
        name: id.to_string(),
        kind: KIND.into(),
        group: Some(db.to_string()),
        description: Some(
            if full(sc) {
                "Backup completo"
            } else {
                "Backup incremental"
            }
            .into(),
        ),
        owner: None,
        enabled: true,
        schedule: s(sc, "/spec/cronSpec/text"),
        running,
        // How the last finished backup ended; Running only while the
        // first one is being made.
        last_outcome: last
            .map(|_| JobOutcome::Succeeded)
            .or(running.then_some(JobOutcome::Running)),
        last_started: utc(last.or(creating).and_then(|b| s(b, "/createTime"))),
        last_duration_ms: None,
        next_run: None,
        can_run: false,
        can_stop: false,
        can_enable: false,
        has_history: true,
        has_script: true,
        system: false,
    })
}

fn prop(label: &str, value: impl Into<String>) -> PropertyInfo {
    PropertyInfo {
        group: String::new(),
        label: label.into(),
        value: value.into(),
    }
}

pub(crate) fn detail_of(sc: &Json, mine: &[&Json]) -> Option<JobDetail> {
    let job = job_of(sc, mine)?;
    let mut properties = Vec::new();
    if let Some(r) = s(sc, "/retentionDuration") {
        properties.push(prop("Se guarda", duration_text(&r)));
    }
    if let Some(w) = s(sc, "/spec/cronSpec/creationWindow") {
        properties.push(prop("Ventana para empezar", duration_text(&w)));
    }
    if let Some(z) = s(sc, "/spec/cronSpec/timeZone") {
        properties.push(prop("Zona horaria", z));
    }
    if let Some(e) = s(sc, "/encryptionConfig/encryptionType") {
        properties.push(prop("Cifrado", e));
    }
    let keys: Vec<String> = sc
        .pointer("/encryptionConfig/kmsKeyNames")
        .and_then(Json::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Json::as_str)
                .map(str::to_string)
                .collect()
        })
        .or_else(|| s(sc, "/encryptionConfig/kmsKeyName").map(|k| vec![k]))
        .unwrap_or_default();
    if !keys.is_empty() {
        properties.push(prop("Claves KMS", keys.join(", ")));
    }
    if let Some(t) = utc(s(sc, "/updateTime")) {
        properties.push(prop("Modificado", t));
    }
    Some(JobDetail {
        steps: vec![JobStep {
            name: "Backup".into(),
            kind: job.description.clone(),
            target: job.group.clone(),
            command: None,
        }],
        schedules: Vec::new(),
        properties,
        notes: vec![NOTE.into()],
        job,
    })
}

/// A backup as a run.
pub(crate) fn run_of(b: &Json) -> JobRun {
    let state = s(b, "/state");
    let mut message = Vec::new();
    if let Some(n) = b.get("sizeBytes").and_then(|v| {
        v.as_str()
            .and_then(|x| x.parse::<u64>().ok())
            .or(v.as_u64())
    }) {
        message.push(format!("{n} bytes"));
    }
    if let Some(e) = utc(s(b, "/expireTime")) {
        message.push(format!("vence {e}"));
    }
    JobRun {
        id: s(b, "/name").map(|n| short(&n).to_string()),
        started: utc(s(b, "/createTime")),
        duration_ms: None,
        outcome: match state.as_deref() {
            Some("READY") => JobOutcome::Succeeded,
            Some("CREATING") => JobOutcome::Running,
            _ => JobOutcome::Unknown,
        },
        trigger: Some("programada".into()),
        message: (!message.is_empty()).then(|| message.join(", ")),
        steps: Vec::new(),
    }
}

/// The schedule's backups, newest first.
pub(crate) fn backups_of<'a>(backups: &'a [Json], schedule: &str) -> Vec<&'a Json> {
    let mut out: Vec<&Json> = backups
        .iter()
        .filter(|b| {
            b.get("backupSchedules")
                .and_then(Json::as_array)
                .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(schedule)))
        })
        .collect();
    out.sort_by_key(|b| std::cmp::Reverse(s(b, "/createTime").as_deref().and_then(epoch_ms)));
    out
}

/// `gcloud spanner backup-schedules create`, and the API's JSON as a
/// comment.
pub(crate) fn script_of(sc: &Json) -> Result<String> {
    let name =
        s(sc, "/name").ok_or_else(|| Error::Query("programa de backup sin nombre".into()))?;
    let parts: Vec<&str> = name.split('/').collect();
    let [_, project, _, instance, _, db, _, id] = parts.as_slice() else {
        return Err(Error::Query(format!(
            "nombre de programa de backup inesperado: {name}"
        )));
    };
    let q = |v: &str| format!("'{}'", v.replace('\'', "'\\''"));
    let mut cmd = format!(
        "gcloud spanner backup-schedules create {id} \\\n  --project={project} --instance={instance} --database={db} \\\n  --backup-type={}",
        if full(sc) {
            "full-backup"
        } else {
            "incremental-backup"
        }
    );
    if let Some(c) = s(sc, "/spec/cronSpec/text") {
        cmd.push_str(&format!(" \\\n  --cron={}", q(&c)));
    }
    if let Some(r) = s(sc, "/retentionDuration") {
        cmd.push_str(&format!(
            " \\\n  --retention-duration={}",
            duration_text(&r)
        ));
    }
    match s(sc, "/encryptionConfig/encryptionType").as_deref() {
        Some("CUSTOMER_MANAGED_ENCRYPTION") => {
            cmd.push_str(" \\\n  --encryption-type=customer-managed-encryption");
            let keys: Vec<String> = sc
                .pointer("/encryptionConfig/kmsKeyNames")
                .and_then(Json::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Json::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .filter(|k: &Vec<String>| !k.is_empty())
                .or_else(|| s(sc, "/encryptionConfig/kmsKeyName").map(|k| vec![k]))
                .unwrap_or_default();
            if keys.len() == 1 {
                cmd.push_str(&format!(" \\\n  --kms-key={}", q(&keys[0])));
            } else if !keys.is_empty() {
                cmd.push_str(&format!(" \\\n  --kms-keys={}", q(&keys.join(","))));
            }
        }
        Some("GOOGLE_DEFAULT_ENCRYPTION") => {
            cmd.push_str(" \\\n  --encryption-type=google-default-encryption")
        }
        _ => {}
    }
    // The API's body: what the schedule is, without what Spanner fills in.
    let mut body = Map::new();
    for k in [
        "spec",
        "retentionDuration",
        "encryptionConfig",
        "fullBackupSpec",
        "incrementalBackupSpec",
    ] {
        if let Some(v) = sc.get(k) {
            body.insert(k.into(), v.clone());
        }
    }
    if let Some(spec) = body
        .get_mut("spec")
        .and_then(|v| v.pointer_mut("/cronSpec"))
        .and_then(Json::as_object_mut)
    {
        spec.remove("timeZone");
    }
    let json = serde_json::to_string_pretty(&Json::Object(body)).unwrap_or_default();
    let mut out = format!("{cmd}\n\n# Con la API: POST https://spanner.googleapis.com/v1/projects/{project}/instances/{instance}/databases/{db}/backupSchedules?backup_schedule_id={id}\n");
    for line in json.lines() {
        out.push_str(&format!("# {line}\n"));
    }
    Ok(out)
}

/// A query-string value, percent-encoded (Google's page tokens have `+`,
/// `/` and `=`; the backups' filter has quotes and spaces).
pub(crate) fn enc_query(v: &str) -> String {
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

/// The principal can't list backup schedules: the call's gRPC status, or
/// Google's message when the status didn't come.
pub(crate) fn denied(status: Option<&str>, e: &Error) -> bool {
    status == Some("PERMISSION_DENIED")
        || matches!(e, Error::Query(m) if m.contains("PERMISSION_DENIED") || m.contains("missing IAM permission"))
}

/// The note when the principal can't list the schedules of `refused` of the
/// `read` databases.
pub(crate) fn denied_note(refused: usize, read: usize) -> Option<String> {
    match refused {
        0 => None,
        n if n >= read => Some(DENIED.into()),
        n => Some(format!(
            "Este usuario no puede ver los programas de backup de {n} bases (hace falta el permiso spanner.backupSchedules.list): se muestran los de las demás."
        )),
    }
}

/// One note for the databases whose schedules couldn't be read, with the
/// first error.
pub(crate) fn failed_note(dbs: &[String], first: &str) -> Option<String> {
    const NAMED: usize = 10;
    let names = dbs
        .iter()
        .take(NAMED)
        .map(|d| format!("«{d}»"))
        .collect::<Vec<_>>()
        .join(", ");
    match dbs.len() {
        0 => None,
        1 => Some(format!(
            "No se pudieron leer los programas de backup de la base {names}: {first}"
        )),
        n if n <= NAMED => Some(format!(
            "No se pudieron leer los programas de backup de las bases {names}: {first}"
        )),
        n => Some(format!(
            "No se pudieron leer los programas de backup de las bases {names} y {} más: {first}",
            n - NAMED
        )),
    }
}

/// The emulator answers `{"code": 12}` (UNIMPLEMENTED) to backup calls.
pub(crate) fn unimplemented(e: &Error) -> bool {
    matches!(e, Error::Query(m) | Error::Unsupported(m)
        if m.contains("\"code\":12") || m.contains("UNIMPLEMENTED"))
}

fn not_found(id: &str) -> Error {
    Error::Query(format!(
        "no existe el programa de backup «{id}» o este usuario no lo puede ver"
    ))
}

impl SpannerSession {
    pub(crate) async fn pages(&self, path: &str, key: &str, max: usize) -> Result<Vec<Json>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let sep = if path.contains('?') { '&' } else { '?' };
            let mut url = format!("{path}{sep}pageSize=500");
            if let Some(t) = token.take() {
                url.push_str(&format!("&pageToken={}", enc_query(&t)));
            }
            let r = self.api.get(&url).await?;
            out.extend(
                r.get(key)
                    .and_then(Json::as_array)
                    .cloned()
                    .unwrap_or_default(),
            );
            match r.get("nextPageToken").and_then(Json::as_str) {
                Some(t) if !t.is_empty() && out.len() < max => token = Some(t.to_string()),
                _ => return Ok(out),
            }
        }
    }

    pub(crate) async fn schedules_of(&self, db: &str) -> Result<Vec<Json>> {
        self.pages(
            &format!("{}/databases/{db}/backupSchedules", self.instance),
            "backupSchedules",
            MAX_BACKUPS,
        )
        .await
    }

    /// The backups of a database (its schedules' history).
    async fn backups_of_db(&self, db: &str) -> Result<Vec<Json>> {
        let filter = format!("database:\"{}/databases/{db}\"", self.instance);
        self.pages(
            &format!("{}/backups?filter={}", self.instance, enc_query(&filter)),
            "backups",
            MAX_BACKUPS,
        )
        .await
    }

    pub(crate) async fn schedule(&self, db: &str, id: &str) -> Result<Json> {
        self.api
            .get(&format!(
                "{}/databases/{db}/backupSchedules/{id}",
                self.instance
            ))
            .await
            .map_err(|e| match e {
                Error::Query(m) if m.contains("NOT_FOUND") || m.contains("not found") => {
                    not_found(&format!("{db}/{id}"))
                }
                e => e,
            })
    }

    pub(crate) async fn server_jobs(&mut self) -> Result<JobList> {
        let dbs = self.list_database_ids().await?;
        let mut jobs = Vec::new();
        let mut notes = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        let mut first_error = String::new();
        let mut refused = 0usize;
        let read: Vec<&String> = dbs
            .iter()
            .filter(|d| spanner_id(d))
            .take(MAX_DATABASES)
            .collect();
        for db in read.iter().copied() {
            self.api
                .last_status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            let schedules = match self.schedules_of(db).await {
                Ok(s) => s,
                Err(e) if unimplemented(&e) => {
                    return Ok(JobList {
                        jobs: Vec::new(),
                        notes: vec![EMULATOR.into()],
                    })
                }
                Err(e) => {
                    let status = self
                        .api
                        .last_status
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take();
                    // IAM can be granted per database: count them, one note.
                    if denied(status.as_deref(), &e) {
                        refused += 1;
                        continue;
                    }
                    if failed.is_empty() {
                        first_error = e.to_string();
                    }
                    failed.push(db.clone());
                    continue;
                }
            };
            if schedules.is_empty() {
                continue;
            }
            let backups = self.backups_of_db(db).await.unwrap_or_else(|e| {
                tracing::debug!("spanner backups of {db}: {e}");
                Vec::new()
            });
            for sc in &schedules {
                let name = s(sc, "/name").unwrap_or_default();
                jobs.extend(job_of(sc, &backups_of(&backups, &name)));
            }
        }
        notes.extend(denied_note(refused, read.len()));
        notes.extend(failed_note(&failed, &first_error));
        if dbs.len() > MAX_DATABASES {
            notes.push(format!(
                "Se leen los programas de backup de las primeras {MAX_DATABASES} bases de la instancia."
            ));
        }
        Ok(JobList { jobs, notes })
    }

    pub(crate) async fn server_job_detail(&mut self, id: &str) -> Result<JobDetail> {
        let (db, sid) = parse_id(id)?;
        let sc = self.schedule(&db, &sid).await?;
        let backups = self.backups_of_db(&db).await.unwrap_or_default();
        let name = s(&sc, "/name").unwrap_or_default();
        detail_of(&sc, &backups_of(&backups, &name)).ok_or_else(|| not_found(id))
    }

    pub(crate) async fn server_job_runs(&mut self, id: &str, limit: u32) -> Result<Vec<JobRun>> {
        let (db, sid) = parse_id(id)?;
        let sc = self.schedule(&db, &sid).await?;
        let name = s(&sc, "/name").unwrap_or_default();
        let backups = self.backups_of_db(&db).await?;
        Ok(backups_of(&backups, &name)
            .into_iter()
            .take(limit.min(MAX_JOB_RUNS) as usize)
            .map(run_of)
            .collect())
    }

    pub(crate) async fn server_job_script(&mut self, id: &str) -> Result<String> {
        let (db, sid) = parse_id(id)?;
        script_of(&self.schedule(&db, &sid).await?)
    }

    /// The instance's database ids.
    pub(crate) async fn list_database_ids(&self) -> Result<Vec<String>> {
        Ok(self
            .pages(&format!("{}/databases", self.instance), "databases", 10_000)
            .await?
            .iter()
            .filter_map(|d| s(d, "/name"))
            .map(|n| short(&n).to_string())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NAME: &str = "projects/p1/instances/i1/databases/ventas/backupSchedules/diario";

    fn sched() -> Json {
        json!({"name": NAME,
            "spec": {"cronSpec": {"text": "0 2 * * *", "timeZone": "UTC", "creationWindow": "14400s"}},
            "retentionDuration": "604800s",
            "encryptionConfig": {"encryptionType": "CUSTOMER_MANAGED_ENCRYPTION",
                "kmsKeyName": "projects/p1/locations/us/keyRings/r/cryptoKeys/k"},
            "fullBackupSpec": {}, "updateTime": "2026-11-10T12:00:00.123456Z"})
    }

    fn backups() -> Vec<Json> {
        vec![
            json!({"name": "projects/p1/instances/i1/backups/b1", "state": "READY", "createTime": "2026-11-11T02:00:05.5Z",
                "sizeBytes": "1024", "expireTime": "2026-11-18T02:00:05Z", "backupSchedules": [NAME]}),
            json!({"name": "projects/p1/instances/i1/backups/b2", "state": "CREATING", "createTime": "2026-11-12T02:00:03Z",
                "backupSchedules": [NAME]}),
            json!({"name": "projects/p1/instances/i1/backups/manual", "state": "READY", "createTime": "2026-11-12T09:00:00Z"}),
        ]
    }

    #[test]
    fn schedules_become_jobs() {
        let b = backups();
        let mine = backups_of(&b, NAME);
        assert_eq!(mine.len(), 2, "a manual backup isn't the schedule's");
        let j = job_of(&sched(), &mine).unwrap();
        assert_eq!(j.id, "ventas/diario");
        assert_eq!(j.kind, "Backup schedule");
        assert_eq!(j.group.as_deref(), Some("ventas"));
        assert_eq!(j.description.as_deref(), Some("Backup completo"));
        assert_eq!(j.schedule.as_deref(), Some("0 2 * * *"));
        // A backup being made: running, and the last finished one says how
        // the last run ended.
        assert!(j.running);
        assert_eq!(j.last_outcome, Some(JobOutcome::Succeeded));
        assert_eq!(j.last_started.as_deref(), Some("2026-11-11T02:00:05Z"));
        let first = job_of(&sched(), &mine[..1]).unwrap();
        assert!(first.running);
        assert_eq!(first.last_outcome, Some(JobOutcome::Running));
        assert_eq!(first.last_started.as_deref(), Some("2026-11-12T02:00:03Z"));
        let done = job_of(&sched(), &mine[1..]).unwrap();
        assert!(!done.running);
        assert_eq!(done.last_outcome, Some(JobOutcome::Succeeded));
        assert!(j.enabled && j.has_history && j.has_script);
        assert!(!j.can_run && !j.can_stop && !j.can_enable && !j.system);
        let runs: Vec<JobRun> = mine.into_iter().map(run_of).collect();
        assert_eq!(runs[1].id.as_deref(), Some("b1"));
        assert_eq!(runs[1].outcome, JobOutcome::Succeeded);
        assert_eq!(
            runs[1].message.as_deref(),
            Some("1024 bytes, vence 2026-11-18T02:00:05Z")
        );
        let mut inc = sched();
        inc.as_object_mut().unwrap().remove("fullBackupSpec");
        inc["incrementalBackupSpec"] = json!({});
        assert_eq!(
            job_of(&inc, &[]).unwrap().description.as_deref(),
            Some("Backup incremental")
        );
        assert!(job_of(
            &json!({"name": "projects/p/instances/i/databases/Bad/backupSchedules/x"}),
            &[]
        )
        .is_none());
    }

    #[test]
    fn detail_and_script() {
        let d = detail_of(&sched(), &[]).unwrap();
        let p = |l: &str| {
            d.properties
                .iter()
                .find(|p| p.label == l)
                .map(|p| p.value.clone())
        };
        assert_eq!(p("Se guarda").as_deref(), Some("7d"));
        assert_eq!(p("Ventana para empezar").as_deref(), Some("4h"));
        assert_eq!(p("Modificado").as_deref(), Some("2026-11-10T12:00:00Z"));
        let sc = script_of(&sched()).unwrap();
        assert!(sc.starts_with("gcloud spanner backup-schedules create diario \\\n  --project=p1 --instance=i1 --database=ventas"));
        assert!(sc.contains("--backup-type=full-backup"));
        assert!(sc.contains("--cron='0 2 * * *'"));
        assert!(sc.contains("--retention-duration=7d"));
        assert!(sc.contains("--encryption-type=customer-managed-encryption"));
        assert!(sc.contains("--kms-key='projects/p1/locations/us/keyRings/r/cryptoKeys/k'"));
        assert!(sc.contains("# Con la API: POST https://spanner.googleapis.com/v1/projects/p1/instances/i1/databases/ventas/backupSchedules?backup_schedule_id=diario"));
        assert!(sc.contains("#   \"retentionDuration\": \"604800s\""));
        assert!(!sc.contains("timeZone") && !sc.contains("updateTime"));
    }

    #[test]
    fn ids_and_durations() {
        assert_eq!(
            parse_id("ventas/diario").unwrap(),
            ("ventas".into(), "diario".into())
        );
        for bad in [
            "",
            "ventas",
            "ventas/",
            "Ventas/diario",
            "ventas/diario/x",
            "ventas/../x",
            "ventas/diario?x=1",
            "ventas/a b",
        ] {
            assert!(parse_id(bad).is_err(), "{bad:?}");
        }
        assert_eq!(duration_text("86400s"), "1d");
        assert_eq!(duration_text("90s"), "90s");
        assert_eq!(duration_text("120s"), "2m");
        assert_eq!(duration_text("1.5s"), "1.5s");
        assert!(unimplemented(&Error::Query("{\"code\":12}".into())));
    }

    #[test]
    fn page_tokens_and_failures() {
        assert_eq!(enc_query("Cg+a/b=="), "Cg%2Ba%2Fb%3D%3D");
        assert_eq!(
            enc_query("database:\"projects/p/instances/i/databases/d\""),
            "database%3A%22projects%2Fp%2Finstances%2Fi%2Fdatabases%2Fd%22"
        );
        let e = Error::Query(
            "Caller is missing IAM permission spanner.backupSchedules.list on resource x.".into(),
        );
        assert!(denied(None, &e));
        assert!(denied(Some("PERMISSION_DENIED"), &Error::Query("x".into())));
        assert!(!denied(Some("UNAVAILABLE"), &Error::Query("x".into())));
        assert_eq!(denied_note(0, 3), None);
        assert_eq!(denied_note(3, 3).as_deref(), Some(DENIED));
        assert!(denied_note(2, 3).unwrap().contains("de 2 bases"));
        assert_eq!(failed_note(&[], "x"), None);
        assert_eq!(
            failed_note(&["a".into()], "boom").as_deref(),
            Some("No se pudieron leer los programas de backup de la base «a»: boom")
        );
        let many: Vec<String> = (0..12).map(|i| format!("d{i}")).collect();
        let n = failed_note(&many, "boom").unwrap();
        assert!(
            n.contains("«d9»") && !n.contains("«d10»") && n.contains("y 2 más: boom"),
            "{n}"
        );
    }
}
