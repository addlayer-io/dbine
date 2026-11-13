//! The workspace's jobs managed whole through the scheduler contract
//! ([`dbine_driver::Session::scheduler_info`] and the rest): what the
//! workspace's "Jobs & Pipelines" page does with a job, through the Jobs API
//! 2.1 with the connection's own credentials.
//!
//! - Kinds: the job (Lakeflow Jobs) is the workspace's only scheduler
//!   object; its schedule, triggers, tasks, job clusters, serverless
//!   environments, parameters, notifications, Git source and permissions
//!   are parts of it, each a section of its form.
//! - Scripts: the requests DBine sends, as the API takes them (method, path
//!   and JSON body), commented out: the SQL editor can't send them.
//!   Creating is one `jobs/create` (with the permissions in it). Changing is
//!   one `jobs/update` with only the top-level settings that changed, the
//!   changed tasks and job clusters whole, and `fields_to_remove` for what
//!   goes (a removed or renamed task, a cleared setting); when the job
//!   parameters or the serverless environments change, which `jobs/update`
//!   can't remove entries from, it is one `jobs/reset` with the whole
//!   settings instead (the job keeps its id, its runs and its permissions).
//!   Changed permissions are a second request, `PUT
//!   /api/2.0/permissions/jobs/<id>`, after the settings. The script's "⚠"
//!   lines, which the app asks the user to accept, say that those two
//!   requests aren't atomic, that a bundle's next deploy overwrites the
//!   change, and that a delete takes the job's runs.
//! - Nothing is lost on the way: every setting, task attribute and cluster
//!   attribute the form has no field for stays in an "Otros … (JSON)" field
//!   as the server gave it, and goes back as it is; a value a text line
//!   can't hold is shown as JSON (see [`lines`]). An edit compares the
//!   job the form was read from (a fingerprint in its values) with the
//!   job now and refuses to save over a change made since.
//! - Values and ids come from the app as untrusted text: the job id is a
//!   number, the workspace's ids are checked for their shape, names for the
//!   API's rules, and everything goes into the request as JSON; the paths
//!   are fixed. The API refuses ids that don't exist and changes the login
//!   may not make.
//! - Run from a task: `run-now` with `only`, the task and every task after
//!   it in the graph (a new run; `runs/repair`, which reruns an existing
//!   run's failed tasks, isn't a start at a task). Clear history:
//!   `runs/delete` of each finished run (the API refuses an active one).
//!   "Ejecutar ahora" is the jobs list's `run-now`; the kind's one action,
//!   "Ejecutar con parámetros" ([`run`]), is a `run-now` with other values
//!   for the job's (or its tasks') parameters, after a form that asks for
//!   them. No scheduler log: a job's runs are its history, and the
//!   workspace's audit log of the jobs is the system table
//!   `system.access.audit`.
//! - What the login may do with each job: an admin anything; anyone else
//!   needs CAN_MANAGE (or ownership) to change or delete it, and the
//!   workspace shows a job's permissions only to them, so a refused read
//!   means no. The form is read only then, and the tree's item says it
//!   (one permissions request per job, for the first jobs, a few at a
//!   time).

mod form;
mod lines;
mod plan;
mod run;
mod spec;
#[cfg(test)]
mod tests;

use crate::jobs::{self, next_token, server_job};
use crate::{DatabricksSession, Json};
use dbine_driver::{
    Error, FormKind, FormValues, ObjectForm, Result, SchedulerAction, SchedulerInfo, SchedulerItem,
    SchedulerKind, SchedulerList,
};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

const JOBS_REFUSED: &str = "Databricks no deja usar la API de jobs con estas credenciales";
const NOT_ADMIN: &str = "Se ven solo los jobs que este usuario puede ver; cambiar o borrar uno necesita el permiso CAN_MANAGE sobre él (o ser su propietario).";
const CHANGED: &str =
    "El job cambió en el workspace desde que se abrió el formulario: recargalo antes de guardar.";

/// Jobs the tree lists at most.
const MAX_JOBS: usize = 5000;
/// Entries a choice list reads at most.
const MAX_CHOICES: usize = 2000;
/// Finished runs `clear_job_history` deletes at most in one call.
const MAX_CLEARED: usize = 100_000;
/// How long the form waits for SCIM `Me`.
const ME_LIMIT: Duration = Duration::from_secs(20);
/// Jobs whose permissions the tree reads for a login that isn't an admin,
/// at most (one request each).
const MAX_ACL_ITEMS: usize = 200;
/// Permission requests at a time.
const ACL_PARALLEL: usize = 8;
/// How long the tree waits for them.
const ACL_LIMIT: Duration = Duration::from_secs(15);

/// A permissions answer's `access_control_list`.
fn acl_of(r: &Json) -> Vec<Json> {
    r.get("access_control_list")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default()
}

fn bad(message: String) -> Error {
    Error::Query(message)
}

/// Text from the app, short and on one line, for a message.
fn shown(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(60)
        .collect()
}

fn check_kind(kind: &str) -> Result<()> {
    match kind.trim() {
        "job" => Ok(()),
        other => Err(bad(format!(
            "«{}» no es un tipo de objeto del planificador de Databricks",
            shown(other)
        ))),
    }
}

/// An id of the workspace's objects (warehouse, cluster, pipeline, policy,
/// pool, destination, query, alert, dashboard): letters, digits, `-`, `_`
/// and `.`, at most 128.
pub(crate) fn checked_id(id: &str, label: &str) -> Result<String> {
    let t = id.trim();
    let ok = !t.is_empty()
        && t.len() <= 128
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if ok {
        Ok(t.to_string())
    } else {
        Err(bad(format!(
            "«{label}»: «{}» no es un id válido.",
            shown(t)
        )))
    }
}

/// A key of a task, a job cluster or an environment: letters, digits, `_`
/// and `-`, at most 100.
pub(crate) fn checked_key(key: &str, what: &str) -> Result<String> {
    let t = key.trim();
    let ok = !t.is_empty()
        && t.len() <= 100
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
    if ok {
        Ok(t.to_string())
    } else {
        Err(bad(format!(
            "{what}: «{}» no es una clave válida (letras, números, «_» y «-», hasta 100).",
            shown(t)
        )))
    }
}

// ---------------------------------------------------------------------------
// Values from the designer
// ---------------------------------------------------------------------------

/// A form's values (or a list row's) as the app sent them: untrusted, any
/// JSON type where a field expects another.
#[derive(Clone, Copy)]
pub(crate) struct Vals<'a>(pub(crate) &'a BTreeMap<String, Json>);

impl<'a> Vals<'a> {
    fn get(&self, k: &str) -> Option<&'a Json> {
        self.0.get(k).filter(|v| !v.is_null())
    }

    /// The value as typed (code keeps its spaces).
    pub(crate) fn raw(&self, k: &str) -> String {
        match self.get(k) {
            Some(Json::String(s)) => s.clone(),
            Some(Json::Array(_) | Json::Object(_)) | None => String::new(),
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

    pub(crate) fn bool(&self, k: &str) -> bool {
        match self.get(k) {
            Some(Json::Bool(b)) => *b,
            Some(Json::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            Some(Json::String(s)) => {
                matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "true" | "1" | "yes" | "on"
                )
            }
            _ => false,
        }
    }

    /// A whole number in `min..=max`; `None` when missing or empty.
    pub(crate) fn int(&self, k: &str, label: &str, min: i64, max: i64) -> Result<Option<i64>> {
        let n = match self.get(k) {
            None => return Ok(None),
            Some(Json::String(s)) if s.trim().is_empty() => return Ok(None),
            Some(Json::Number(n)) => n
                .as_i64()
                .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
            Some(Json::String(s)) => s.trim().parse::<i64>().ok(),
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

    /// A list of values (a multiple reference's), trimmed, without empties;
    /// a text (the field without a list to pick from) is split by lines and
    /// commas.
    pub(crate) fn strings(&self, k: &str) -> Vec<String> {
        let mut out: Vec<String> = match self.get(k) {
            Some(Json::Array(a)) => a
                .iter()
                .filter_map(|v| match v {
                    Json::String(s) => Some(s.trim().to_string()),
                    Json::Null => None,
                    v => Some(v.to_string()),
                })
                .collect(),
            Some(Json::String(s)) => s.split(['\n', ',']).map(|x| x.trim().to_string()).collect(),
            _ => Vec::new(),
        };
        out.retain(|s| !s.is_empty());
        let mut seen = std::collections::HashSet::new();
        out.retain(|s| seen.insert(s.clone()));
        out
    }
}

/// A list's rows from the designer.
pub(crate) fn rows<'a>(values: &'a FormValues, list: &str) -> Vec<Vals<'a>> {
    values
        .lists
        .get(list)
        .map(|rows| rows.iter().map(Vals).collect())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The workspace's objects the form picks from
// ---------------------------------------------------------------------------

/// The `Ref` lists of the form, and the ones the workspace didn't give.
#[derive(Debug, Clone, Default)]
pub(crate) struct Sources {
    pub(crate) lists: BTreeMap<String, Vec<(String, String)>>,
    pub(crate) missing: Vec<&'static str>,
}

impl Sources {
    pub(crate) fn has(&self, source: &str) -> bool {
        self.lists.get(source).is_some_and(|l| !l.is_empty())
    }

    /// A pick from the list, or a text field for the id when there is no
    /// list (not readable, or empty).
    pub(crate) fn one(&self, source: &str) -> FormKind {
        if self.has(source) {
            FormKind::reference(source, false)
        } else {
            FormKind::Text
        }
    }

    /// Several picks from the list, or ids one per line.
    pub(crate) fn many(&self, source: &str) -> FormKind {
        if self.has(source) {
            FormKind::reference(source, true)
        } else {
            FormKind::Textarea
        }
    }

    /// The notes for the lists that couldn't be read.
    fn notes(&self) -> Vec<String> {
        self.missing
            .iter()
            .map(|s| {
                match *s {
                    "warehouse" => "No se pudo leer la lista de SQL warehouses del workspace: su id se escribe a mano.",
                    "cluster" => "No se pudo leer la lista de clústeres del workspace: su id se escribe a mano.",
                    "pipeline" => "No se pudo leer la lista de pipelines del workspace: su id se escribe a mano.",
                    "job" => "No se pudo leer la lista de jobs del workspace: el id del job que se ejecuta se escribe a mano.",
                    "destination" => "No se pudo leer la lista de destinos de notificación del workspace: sus ids se escriben a mano, uno por línea.",
                    "spark_version" => "No se pudo leer la lista de versiones de Databricks Runtime: se escribe a mano.",
                    "node_type" => "No se pudo leer la lista de tipos de nodo: se escriben a mano.",
                    "policy" => "No se pudo leer la lista de políticas de clúster: su id se escribe a mano.",
                    _ => "No se pudo leer la lista de pools de instancias: su id se escribe a mano.",
                }
                .to_string()
            })
            .collect()
    }
}

fn by_label(mut v: Vec<(String, String)>) -> Vec<(String, String)> {
    v.sort_by_key(|(_, l)| l.to_lowercase());
    v.dedup_by(|a, b| a.0 == b.0);
    v
}

fn pairs(
    items: &[Json],
    id: &str,
    label: impl Fn(&Json) -> Option<String>,
) -> Vec<(String, String)> {
    by_label(
        items
            .iter()
            .filter_map(|x| {
                let v = match x.get(id)? {
                    Json::String(s) => s.clone(),
                    Json::Number(n) => n.to_string(),
                    _ => return None,
                };
                let l = label(x)
                    .filter(|l| !l.is_empty())
                    .unwrap_or_else(|| v.clone());
                Some((v, l))
            })
            .collect(),
    )
}

fn str_at(v: &Json, ptr: &str) -> Option<String> {
    v.pointer(ptr).and_then(Json::as_str).map(str::to_string)
}

/// A job's settings and permissions as the workspace has them now.
#[derive(Debug, Clone)]
pub(crate) struct Current {
    pub(crate) id: u64,
    /// `jobs/get` whole.
    pub(crate) job: Json,
    /// Its `access_control_list`, when the login may read it.
    pub(crate) acl: Option<Vec<Json>>,
}

impl Current {
    pub(crate) fn settings(&self) -> &Json {
        self.job.get("settings").unwrap_or(&Json::Null)
    }

    pub(crate) fn name(&self) -> String {
        str_at(self.settings(), "/name").unwrap_or_else(|| self.id.to_string())
    }

    /// The settings with `run_as` as the job really runs: the API may leave
    /// it out of the settings and say only `run_as_user_name`, which is the
    /// owner's unless someone else was chosen (a service principal's is its
    /// application id). Without it a `jobs/reset` would put the owner back.
    pub(crate) fn effective(&self) -> Json {
        let mut s = self.settings().clone();
        if s.get("run_as").is_some() || !s.is_object() {
            return s;
        }
        let Some(who) = str_at(&self.job, "/run_as_user_name") else {
            return s;
        };
        let owner = self
            .acl
            .iter()
            .flatten()
            .find(|e| {
                e.get("all_permissions")
                    .and_then(Json::as_array)
                    .into_iter()
                    .flatten()
                    .any(|p| p.get("permission_level").and_then(Json::as_str) == Some("IS_OWNER"))
            })
            .and_then(|e| {
                e.get("user_name")
                    .or_else(|| e.get("service_principal_name"))
                    .and_then(Json::as_str)
                    .map(str::to_string)
            })
            .or_else(|| str_at(&self.job, "/creator_user_name"));
        if owner.is_some_and(|o| o.eq_ignore_ascii_case(&who)) {
            return s;
        }
        let key = if who.contains('@') {
            "user_name"
        } else {
            "service_principal_name"
        };
        s["run_as"] = json!({ key: who });
        s
    }
}

impl DatabricksSession {
    /// Pages of a list endpoint (`page_token` / `next_page_token`): the
    /// items under `key`, at most `max`.
    async fn pages(
        &self,
        path: &str,
        query: &[(&str, String)],
        key: &str,
        max: usize,
    ) -> Result<(Vec<Json>, bool)> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut q = query.to_vec();
            if let Some(t) = token.take() {
                q.push(("page_token", t));
            }
            let page = self.api.get_query(path, &q).await?;
            out.extend(
                page.get(key)
                    .and_then(Json::as_array)
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
            match next_token(&page) {
                Some(_) if out.len() >= max => return Ok((out, true)),
                Some(t) => token = Some(t),
                None => return Ok((out, false)),
            }
        }
    }

    async fn source(&self, which: &'static str) -> Result<Vec<(String, String)>> {
        Ok(match which {
            "warehouse" => {
                let r = self.api.get("/api/2.0/sql/warehouses").await?;
                pairs(
                    r.get("warehouses")
                        .and_then(Json::as_array)
                        .map_or(&[][..], Vec::as_slice),
                    "id",
                    |w| str_at(w, "/name"),
                )
            }
            "cluster" => {
                let (all, _) = self
                    .pages(
                        "/api/2.1/clusters/list",
                        &[("page_size", "100".into())],
                        "clusters",
                        MAX_CHOICES,
                    )
                    .await?;
                let mine: Vec<Json> = all
                    .into_iter()
                    .filter(|c| {
                        matches!(
                            c.get("cluster_source").and_then(Json::as_str),
                            None | Some("UI" | "API")
                        )
                    })
                    .collect();
                pairs(&mine, "cluster_id", |c| str_at(c, "/cluster_name"))
            }
            "pipeline" => {
                let (all, _) = self
                    .pages(
                        "/api/2.0/pipelines",
                        &[("max_results", "100".into())],
                        "statuses",
                        MAX_CHOICES,
                    )
                    .await?;
                pairs(&all, "pipeline_id", |p| str_at(p, "/name"))
            }
            "job" => {
                let (all, _) = self
                    .pages(
                        "/api/2.1/jobs/list",
                        &[("limit", "100".into())],
                        "jobs",
                        MAX_CHOICES,
                    )
                    .await?;
                pairs(&all, "job_id", |j| {
                    let id = j.get("job_id").map(|v| v.to_string()).unwrap_or_default();
                    str_at(j, "/settings/name").map(|n| format!("{n} ({id})"))
                })
            }
            "destination" => {
                let (all, _) = self
                    .pages(
                        "/api/2.0/notification-destinations",
                        &[("page_size", "100".into())],
                        "results",
                        MAX_CHOICES,
                    )
                    .await?;
                pairs(&all, "id", |d| {
                    let name = str_at(d, "/display_name")?;
                    Some(match str_at(d, "/destination_type") {
                        Some(t) => format!("{name} ({t})"),
                        None => name,
                    })
                })
            }
            "spark_version" => {
                let r = self.api.get("/api/2.1/clusters/spark-versions").await?;
                let mut v = pairs(
                    r.get("versions")
                        .and_then(Json::as_array)
                        .map_or(&[][..], Vec::as_slice),
                    "key",
                    |x| str_at(x, "/name"),
                );
                // Newest first, as the workspace offers them.
                v.reverse();
                v
            }
            "node_type" => {
                let r = self.api.get("/api/2.1/clusters/list-node-types").await?;
                pairs(
                    r.get("node_types")
                        .and_then(Json::as_array)
                        .map_or(&[][..], Vec::as_slice),
                    "node_type_id",
                    |n| {
                        let id = str_at(n, "/node_type_id")?;
                        let cores = n.get("num_cores").and_then(Json::as_f64);
                        let mem = n.get("memory_mb").and_then(Json::as_f64);
                        Some(match (cores, mem) {
                            (Some(c), Some(m)) => {
                                format!("{id} · {c} núcleos · {:.0} GB", m / 1024.0)
                            }
                            _ => id,
                        })
                    },
                )
            }
            "policy" => {
                let r = self.api.get("/api/2.0/policies/clusters/list").await?;
                pairs(
                    r.get("policies")
                        .and_then(Json::as_array)
                        .map_or(&[][..], Vec::as_slice),
                    "policy_id",
                    |p| str_at(p, "/name"),
                )
            }
            _ => {
                let r = self.api.get("/api/2.0/instance-pools/list").await?;
                pairs(
                    r.get("instance_pools")
                        .and_then(Json::as_array)
                        .map_or(&[][..], Vec::as_slice),
                    "instance_pool_id",
                    |p| str_at(p, "/instance_pool_name"),
                )
            }
        })
    }

    /// Every list the form picks from, read at once; one the workspace
    /// refuses becomes a text field (and a note).
    pub(crate) async fn sources(&self) -> Sources {
        const ALL: [&str; 9] = [
            "warehouse",
            "cluster",
            "pipeline",
            "job",
            "destination",
            "spark_version",
            "node_type",
            "policy",
            "instance_pool",
        ];
        let (a, b, c, d, e, f, g, h, i) = tokio::join!(
            self.source(ALL[0]),
            self.source(ALL[1]),
            self.source(ALL[2]),
            self.source(ALL[3]),
            self.source(ALL[4]),
            self.source(ALL[5]),
            self.source(ALL[6]),
            self.source(ALL[7]),
            self.source(ALL[8]),
        );
        let mut s = Sources::default();
        for (name, r) in ALL.iter().zip([a, b, c, d, e, f, g, h, i]) {
            match r {
                Ok(list) => {
                    s.lists.insert(name.to_string(), list);
                }
                Err(e) => {
                    tracing::debug!("databricks scheduler: the {name} list wasn't read: {e}");
                    s.missing.push(name);
                }
            }
        }
        s
    }

    /// SCIM `Me`: the login's name and groups.
    async fn me(&self) -> Option<Json> {
        match tokio::time::timeout(ME_LIMIT, self.api.get("/api/2.0/preview/scim/v2/Me")).await {
            Ok(Ok(me)) => Some(me),
            Ok(Err(e)) => {
                tracing::debug!("databricks scheduler (SCIM Me): {e}");
                None
            }
            Err(_) => None,
        }
    }

    async fn acl(&self, id: u64) -> Result<Vec<Json>> {
        let r = self
            .api
            .get(&format!("/api/2.0/permissions/jobs/{id}"))
            .await?;
        Ok(acl_of(&r))
    }

    /// The job as the workspace has it; its permissions too when `acl`.
    async fn current(&self, id: &str, acl: bool) -> Result<Current> {
        let n = jobs::checked(id)?;
        let job = self.job_json(n).await?;
        if job.get("settings").is_none() {
            return Err(bad(format!("Databricks no devolvió el job {n}")));
        }
        let acl = if acl { Some(self.acl(n).await?) } else { None };
        Ok(Current { id: n, job, acl })
    }

    // -----------------------------------------------------------------------
    // The contract's calls
    // -----------------------------------------------------------------------

    pub(crate) async fn sched_info(&self) -> Result<SchedulerInfo> {
        let probe = self
            .api
            .get_query("/api/2.1/jobs/list", &[("limit", "1".into())])
            .await;
        let (ok, mut notes) = match probe {
            Ok(_) => (true, Vec::new()),
            Err(Error::AuthFailed(m)) => (false, vec![format!("{JOBS_REFUSED}: {m}")]),
            Err(e) => return Err(e),
        };
        if ok && !self.workspace_admin().await {
            notes.push(NOT_ADMIN.to_string());
        }
        // A job starts at a task (`run-now` with `only`: the task and every
        // one after it) and its finished runs can be deleted (an active one
        // can't); "Ejecutar ahora" is the jobs list's `run-now`, and its
        // action a `run-now` with other parameters.
        let mut job = SchedulerKind::new("job", "Jobs", "job")
            .abilities(ok, ok, ok)
            .run_from_step(ok)
            .clear_history(ok);
        if ok {
            job = job
                .action(SchedulerAction::new(run::ACTION, "Ejecutar con parámetros").with_params());
        }
        Ok(SchedulerInfo {
            kinds: vec![job],
            logs: false,
            notes,
        })
    }

    /// The permissions of these jobs, a few requests at a time and within
    /// [`ACL_LIMIT`]: each one's list, or whether the workspace refused to
    /// show it (`Err(true)`: the login has no CAN_MANAGE) or failed
    /// otherwise (`Err(false)`). Jobs not read in time are left out.
    async fn acls(&self, ids: &[u64]) -> HashMap<u64, std::result::Result<Vec<Json>, bool>> {
        let mut out = HashMap::new();
        let deadline = tokio::time::Instant::now() + ACL_LIMIT;
        for chunk in ids.chunks(ACL_PARALLEL) {
            let mut set = tokio::task::JoinSet::new();
            for &id in chunk {
                let api = self.api.clone();
                set.spawn(async move {
                    let r = api.get(&format!("/api/2.0/permissions/jobs/{id}")).await;
                    (id, r)
                });
            }
            loop {
                match tokio::time::timeout_at(deadline, set.join_next()).await {
                    Ok(Some(Ok((id, r)))) => {
                        let read = match r {
                            Ok(resp) => Ok(acl_of(&resp)),
                            Err(e) => {
                                tracing::debug!(
                                    "databricks scheduler: permissions of job {id}: {e}"
                                );
                                Err(matches!(e, Error::AuthFailed(_)))
                            }
                        };
                        out.insert(id, read);
                    }
                    Ok(Some(Err(e))) => tracing::debug!("databricks scheduler: {e}"),
                    Ok(None) => break,
                    // Out of time: the rest stay unknown (dropping the set
                    // stops their requests).
                    Err(_) => return out,
                }
            }
        }
        out
    }

    pub(crate) async fn sched_items(&self, kind: &str) -> Result<SchedulerList> {
        check_kind(kind)?;
        let query = [("limit", "100".to_string())];
        let (listed, me) = tokio::join!(
            self.pages("/api/2.1/jobs/list", &query, "jobs", MAX_JOBS),
            self.me()
        );
        let (all, cut) = match listed {
            Ok(p) => p,
            Err(Error::AuthFailed(m)) => {
                return Ok(SchedulerList {
                    items: Vec::new(),
                    notes: vec![format!("{JOBS_REFUSED}: {m}")],
                })
            }
            Err(e) => return Err(e),
        };
        let mut items: Vec<SchedulerItem> = all
            .iter()
            .filter_map(server_job)
            .map(|j| SchedulerItem {
                id: j.id,
                name: j.name,
                description: j.description,
                enabled: j.can_enable.then_some(j.enabled),
                system: false,
                can_edit: None,
                can_delete: None,
                actions: None,
                run_from_step: None,
                clear_history: None,
            })
            .collect();
        items.sort_by_key(|i| i.name.to_lowercase());
        // What the login may do with each job, as its form says: an admin
        // anything; anyone else, what its permissions give (the first
        // MAX_ACL_ITEMS jobs, one request each; the rest as the kind says,
        // and the API decides).
        if !me.as_ref().is_some_and(crate::permissions::is_admin) {
            let ids: Vec<u64> = items
                .iter()
                .take(MAX_ACL_ITEMS)
                .filter_map(|i| i.id.parse().ok())
                .collect();
            let acls = self.acls(&ids).await;
            for i in &mut items {
                let manages = match i.id.parse::<u64>().ok().and_then(|n| acls.get(&n)) {
                    Some(Ok(acl)) => form::may_manage(Some(acl), false, me.as_ref()),
                    Some(Err(denied)) => form::may_manage(None, *denied, me.as_ref()),
                    None => None,
                };
                if manages == Some(false) {
                    i.can_edit = Some(false);
                    i.can_delete = Some(false);
                }
            }
        }
        let mut notes = Vec::new();
        if cut {
            notes.push(format!(
                "Se muestran los primeros {MAX_JOBS} jobs del workspace."
            ));
        }
        Ok(SchedulerList { items, notes })
    }

    pub(crate) async fn sched_form(&self, kind: &str, id: Option<&str>) -> Result<ObjectForm> {
        check_kind(kind)?;
        let mut denied = false;
        let current = match id {
            Some(id) => {
                let n = jobs::checked(id)?;
                let job = self.job_json(n).await?;
                if job.get("settings").is_none() {
                    return Err(bad(format!("Databricks no devolvió el job {n}")));
                }
                // Reading the permissions needs CAN_MANAGE: a refusal leaves
                // them out of the form, not the job, and says the login
                // can't change it.
                let acl = match self.acl(n).await {
                    Ok(a) => Some(a),
                    Err(e) => {
                        tracing::debug!("databricks scheduler: permissions of job {n}: {e}");
                        denied = matches!(e, Error::AuthFailed(_));
                        None
                    }
                };
                Some(Current { id: n, job, acl })
            }
            None => None,
        };
        let (sources, me) = tokio::join!(self.sources(), self.me());
        let mut f = form::form_with(current.as_ref(), &sources, me.as_ref(), denied);
        f.notes.extend(sources.notes());
        Ok(f)
    }

    /// The requests for `values`: what the script shows and the save sends.
    async fn plan(&self, id: Option<&str>, values: &FormValues) -> Result<plan::Plan> {
        match id {
            None => plan::create(values),
            Some(id) => {
                let read_acl = Vals(&values.values).bool("permissions_read");
                let cur = self.current(id, read_acl).await?;
                if let Some(seen) = Vals(&values.values).opt("version") {
                    if seen != plan::version(&cur) {
                        return Err(bad(CHANGED.into()));
                    }
                }
                plan::change(&cur, values)
            }
        }
    }

    pub(crate) async fn sched_script(
        &self,
        kind: &str,
        id: Option<&str>,
        values: &FormValues,
    ) -> Result<String> {
        check_kind(kind)?;
        Ok(self.plan(id, values).await?.script())
    }

    pub(crate) async fn sched_save(
        &self,
        kind: &str,
        id: Option<&str>,
        values: &FormValues,
    ) -> Result<Option<String>> {
        check_kind(kind)?;
        let p = self.plan(id, values).await?;
        let mut job_id = p.job_id;
        for (i, r) in p.requests.iter().enumerate() {
            let sent = match r.method {
                plan::Method::Post => self.api.post(&r.path, &r.body).await,
                plan::Method::Put => self.api.put(&r.path, &r.body).await,
            };
            match sent {
                Ok(resp) => {
                    if job_id.is_none() {
                        job_id = resp.get("job_id").and_then(Json::as_u64);
                    }
                }
                // The settings went in; the permissions didn't.
                Err(e) if i > 0 => {
                    return Err(bad(format!(
                        "Se guardaron los ajustes del job, pero no sus permisos: {e}"
                    )))
                }
                Err(e) => return Err(e),
            }
        }
        Ok(job_id.map(|n| n.to_string()))
    }

    pub(crate) async fn sched_delete_script(&self, kind: &str, id: &str) -> Result<String> {
        check_kind(kind)?;
        let cur = self.current(id, false).await?;
        Ok(plan::delete(&cur).script())
    }

    pub(crate) async fn sched_delete(&self, kind: &str, id: &str) -> Result<()> {
        check_kind(kind)?;
        let n = jobs::checked(id)?;
        self.api
            .post("/api/2.1/jobs/delete", &json!({ "job_id": n }))
            .await
            .map(|_| ())
    }

    /// "Iniciar en el paso…": `run-now` with `only`, the task and every
    /// task that depends on it (directly or through others).
    pub(crate) async fn sched_run_from(&self, id: &str, step: &str) -> Result<()> {
        let cur = self.current(id, false).await?;
        let only = plan::from_task(cur.settings(), step)?;
        self.api
            .post(
                "/api/2.1/jobs/run-now",
                &json!({ "job_id": cur.id, "only": only }),
            )
            .await
            .map(|_| ())
    }

    /// Deletes every finished run of the job (`runs/delete`; a run going on
    /// can't be deleted and stays).
    pub(crate) async fn sched_clear_history(&self, id: &str) -> Result<()> {
        let n = jobs::checked(id)?;
        let mut deleted = std::collections::HashSet::new();
        // Passes over the job's finished runs until one finds none left
        // (deleting moves the pages; the list may still show a deleted run
        // for a moment).
        loop {
            let mut found = 0;
            let mut token = None;
            loop {
                let (runs, next) = self
                    .runs_page(
                        &[("job_id", n.to_string()), ("completed_only", "true".into())],
                        token.take(),
                    )
                    .await?;
                let ids: Vec<u64> = runs
                    .iter()
                    .filter_map(|r| r.get("run_id").and_then(Json::as_u64))
                    .filter(|r| !deleted.contains(r))
                    .collect();
                for r in ids {
                    if let Err(e) = self
                        .api
                        .post("/api/2.1/jobs/runs/delete", &json!({ "run_id": r }))
                        .await
                    {
                        return Err(bad(format!(
                            "Se borraron {} ejecuciones del job; la {r} no: {e}",
                            deleted.len()
                        )));
                    }
                    deleted.insert(r);
                    found += 1;
                }
                if deleted.len() >= MAX_CLEARED {
                    return Err(bad(format!(
                        "Se borraron {} ejecuciones del job; quedan más: volvé a borrar el historial.",
                        deleted.len()
                    )));
                }
                match next {
                    Some(t) => token = Some(t),
                    None => break,
                }
            }
            if found == 0 {
                return Ok(());
            }
        }
    }
}
