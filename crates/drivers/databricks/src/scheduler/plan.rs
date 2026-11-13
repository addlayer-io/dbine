//! What a save sends: the Jobs API requests for a new job, a change or a
//! deletion, and the script that shows them (commented out, since the SQL
//! editor can't send them).
//!
//! A change is a diff of the job the form says against the job the
//! workspace has, compared as the API means them: an empty object, list or
//! text, a `false`, `UNPAUSED` and `ALL_SUCCESS` say the same as nothing,
//! and the order of a task's dependencies doesn't matter.

use super::spec::{self, blank, IGNORED};
use super::{bad, shown, Current};
use crate::Json;
use dbine_driver::{FormValues, Result};
use serde_json::{json, Map};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const HEADER: [&str; 2] = [
    "Databricks Jobs API: las solicitudes que DBine envía al guardar, en este orden.",
    "El editor de SQL no las envía: van comentadas.",
];
const RESET: [&str; 3] = [
    "Cambian los parámetros o los entornos serverless del job, de los que jobs/update no puede",
    "quitar entradas: se reescriben todos los ajustes con jobs/reset. El job conserva su id,",
    "su historial de ejecuciones y sus permisos.",
];
const NO_OWNER: &str =
    "Permisos: el job necesita un propietario (un usuario o un service principal con «Propietario»).";

// What a script warns about ("⚠" lines: the app lists them and asks to
// continue), each a whole sentence of its own for the message catalog.

/// A delete takes the job's runs with it.
pub(crate) const RUNS_GO: &str = "Se borra también su historial de ejecuciones.";
/// The settings and the permissions are two requests.
pub(crate) const NOT_ATOMIC: &str = "Los ajustes y los permisos van en dos solicitudes: si la segunda falla, los ajustes quedan guardados sin los permisos nuevos.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Post,
    Put,
}

/// One API request.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Request {
    pub(crate) method: Method,
    pub(crate) path: String,
    pub(crate) body: Json,
    /// Lines that say what it does.
    pub(crate) comment: Vec<String>,
}

/// The requests of a save, and the job they're about.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    pub(crate) requests: Vec<Request>,
    /// The job's id when it already exists.
    pub(crate) job_id: Option<u64>,
    /// Its name, for an empty plan's script.
    pub(crate) name: String,
    /// What the user loses or risks with it, one whole Spanish sentence
    /// each: the script's "⚠" lines.
    pub(crate) warnings: Vec<String>,
}

/// Text on one line for a comment.
fn one_line(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// JSON whose every line can go after `-- ` (the line separators Unicode
/// has besides `\n` escaped too).
fn json_lines(v: &Json) -> String {
    serde_json::to_string_pretty(v)
        .unwrap_or_default()
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
        .replace('\u{85}', "\\u0085")
}

impl Plan {
    pub(crate) fn script(&self) -> String {
        if self.requests.is_empty() {
            return format!(
                "-- «{}»: no hay cambios que guardar.\n",
                one_line(&self.name)
            );
        }
        let mut out = String::new();
        for h in HEADER {
            out.push_str(&format!("-- {h}\n"));
        }
        for w in &self.warnings {
            out.push_str(&format!("-- ⚠ {}\n", one_line(w)));
        }
        for r in &self.requests {
            out.push_str("--\n");
            for c in &r.comment {
                out.push_str(&format!("-- {}\n", one_line(c)));
            }
            let m = match r.method {
                Method::Post => "POST",
                Method::Put => "PUT",
            };
            out.push_str(&format!("-- {m} {}\n", r.path));
            for line in json_lines(&r.body).lines() {
                out.push_str(&format!("-- {line}\n"));
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Comparing
// ---------------------------------------------------------------------------

/// A value as the API means it (see the module's doc).
pub(crate) fn norm(v: &Json) -> Json {
    match v {
        Json::Object(m) => {
            let mut out = Map::new();
            for (k, x) in m {
                let default = matches!(
                    (k.as_str(), x.as_str()),
                    ("pause_status", Some("UNPAUSED")) | ("run_if", Some("ALL_SUCCESS"))
                );
                if default {
                    continue;
                }
                let mut n = norm(x);
                if k == "depends_on" {
                    if let Json::Array(a) = &mut n {
                        a.sort_by_key(|d| d.to_string());
                    }
                }
                if !(blank(&n) || n == Json::Bool(false)) {
                    out.insert(k.clone(), n);
                }
            }
            Json::Object(out)
        }
        Json::Array(a) => Json::Array(a.iter().map(norm).collect()),
        x => x.clone(),
    }
}

fn same(a: Option<&Json>, b: Option<&Json>) -> bool {
    let n = |v: Option<&Json>| v.map_or(Json::Null, norm);
    let (a, b) = (n(a), n(b));
    a == b || (blank(&a) && blank(&b))
}

/// Text of a value with sorted keys (serde_json's maps are sorted).
fn canonical(v: &Json) -> String {
    v.to_string()
}

/// The job's fingerprint: the form carries it, a save from an older form
/// is refused.
pub(crate) fn version(cur: &Current) -> String {
    let mut h = Sha256::new();
    h.update(canonical(cur.settings()));
    h.update(b"\n");
    if let Some(acl) = &cur.acl {
        h.update(canonical(&json!(acl)));
    }
    h.finalize()
        .iter()
        .take(12)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The entries of a keyed list (tasks, job clusters) that changed or are
/// new, and the `fields_to_remove` of the ones gone.
fn entries(
    cur: Option<&Json>,
    des: Option<&Json>,
    key: &str,
    list: &str,
) -> (Vec<Json>, Vec<String>) {
    let items =
        |v: Option<&Json>| -> Vec<Json> { v.and_then(Json::as_array).cloned().unwrap_or_default() };
    let key_of = |x: &Json| x.get(key).and_then(Json::as_str).map(str::to_string);
    let (cur, des) = (items(cur), items(des));
    let by_key: BTreeMap<String, &Json> =
        cur.iter().filter_map(|c| Some((key_of(c)?, c))).collect();
    let wanted: BTreeSet<String> = des.iter().filter_map(key_of).collect();
    let changed = des
        .iter()
        .filter(|d| match key_of(d).and_then(|k| by_key.get(&k).copied()) {
            Some(c) => !same(Some(c), Some(d)),
            None => true,
        })
        .cloned()
        .collect();
    let gone = by_key
        .keys()
        .filter(|k| !wanted.contains(*k))
        .map(|k| format!("{list}/{k}"))
        .collect();
    (changed, gone)
}

/// The direct permissions as a set, to compare.
fn acl_set(acl: &[Json]) -> BTreeSet<(String, String, String)> {
    acl.iter()
        .filter_map(|e| {
            let (kind, name) = super::form::principal(e)?;
            Some((
                kind.to_string(),
                name,
                e.get("permission_level")?.as_str()?.to_string(),
            ))
        })
        .collect()
}

/// The ACL as the server lists it, reduced to its direct entries.
fn direct_acl(acl: &[Json]) -> Vec<Json> {
    let (direct, _) = super::form::acl_rows(acl);
    direct
        .iter()
        .filter_map(|r| {
            let key = match r.get("principal_type")?.as_str()? {
                "user" => "user_name",
                "group" => "group_name",
                _ => "service_principal_name",
            };
            Some(json!({ key: r.get("principal")?, "permission_level": r.get("level")? }))
        })
        .collect()
}

fn owners(acl: &[Json]) -> usize {
    acl.iter()
        .filter(|e| e.get("permission_level").and_then(Json::as_str) == Some("IS_OWNER"))
        .count()
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

pub(crate) fn create(values: &FormValues) -> Result<Plan> {
    let s = spec::settings(values, None)?;
    let name = s["name"].as_str().unwrap_or_default().to_string();
    let mut body = s;
    let mut comment = vec![format!("Crea el job «{name}».")];
    if let Some(acl) = spec::acl(values)?.filter(|a| !a.is_empty()) {
        body["access_control_list"] = json!(acl);
        if owners(&acl) == 0 {
            comment.push("Su propietario es quien lo crea.".into());
        }
    }
    Ok(Plan {
        requests: vec![Request {
            method: Method::Post,
            path: "/api/2.1/jobs/create".into(),
            body,
            comment,
        }],
        job_id: None,
        name,
        warnings: Vec::new(),
    })
}

/// The change from the job `cur` to the form's.
pub(crate) fn change(cur: &Current, values: &FormValues) -> Result<Plan> {
    let effective = cur.effective();
    let s = &effective;
    let want = spec::settings(values, Some(s))?;
    let name = cur.name();
    let mut requests = Vec::new();

    let mut keys: BTreeSet<&str> = BTreeSet::new();
    for o in [s, &want] {
        keys.extend(o.as_object().into_iter().flatten().map(|(k, _)| k.as_str()));
    }
    let mut new = Map::new();
    let mut remove: Vec<String> = Vec::new();
    let mut reset = false;
    for k in keys {
        if IGNORED.contains(&k) {
            continue;
        }
        let (c, w) = (s.get(k), want.get(k));
        match k {
            "tasks" | "job_clusters" => {
                let key = if k == "tasks" {
                    "task_key"
                } else {
                    "job_cluster_key"
                };
                let (changed, gone) = entries(c, w, key, k);
                if !changed.is_empty() {
                    new.insert(k.into(), json!(changed));
                }
                remove.extend(gone);
            }
            "parameters" | "environments" => {
                if !same(c, w) {
                    reset = true;
                }
            }
            _ if same(c, w) => {}
            _ => match w {
                Some(w) => {
                    new.insert(k.into(), w.clone());
                }
                None => remove.push(k.into()),
            },
        }
    }
    if reset {
        requests.push(Request {
            method: Method::Post,
            path: "/api/2.1/jobs/reset".into(),
            body: json!({ "job_id": cur.id, "new_settings": want }),
            comment: std::iter::once(format!("Reescribe el job «{name}» ({}).", cur.id))
                .chain(RESET.iter().map(|l| l.to_string()))
                .collect(),
        });
    } else if !new.is_empty() || !remove.is_empty() {
        let mut body = json!({ "job_id": cur.id, "new_settings": new });
        if !remove.is_empty() {
            body["fields_to_remove"] = json!(remove);
        }
        requests.push(Request {
            method: Method::Post,
            path: "/api/2.1/jobs/update".into(),
            body,
            comment: vec![format!(
                "Cambia el job «{name}» ({}): solo lo que cambió.",
                cur.id
            )],
        });
    }

    if let (Some(now), Some(wanted)) = (&cur.acl, spec::acl(values)?) {
        let now = direct_acl(now);
        if acl_set(&now) != acl_set(&wanted) {
            if owners(&wanted) != 1 && owners(&now) > 0 {
                return Err(bad(NO_OWNER.into()));
            }
            requests.push(Request {
                method: Method::Put,
                path: format!("/api/2.0/permissions/jobs/{}", cur.id),
                body: json!({ "access_control_list": wanted }),
                comment: vec![
                    "Reemplaza los permisos directos del job (los heredados no cambian).".into(),
                ],
            });
        }
    }
    let mut warnings = Vec::new();
    if !requests.is_empty() && bundled(cur) {
        warnings.push(super::form::BUNDLE.to_string());
    }
    if requests.len() > 1 {
        warnings.push(NOT_ATOMIC.to_string());
    }
    Ok(Plan {
        requests,
        job_id: Some(cur.id),
        name,
        warnings,
    })
}

/// A Databricks Asset Bundle deploys the job: its next deploy writes the
/// bundle's settings over what DBine saves.
fn bundled(cur: &Current) -> bool {
    cur.settings()
        .pointer("/deployment/kind")
        .and_then(Json::as_str)
        == Some("BUNDLE")
}

pub(crate) fn delete(cur: &Current) -> Plan {
    Plan {
        requests: vec![Request {
            method: Method::Post,
            path: "/api/2.1/jobs/delete".into(),
            body: json!({ "job_id": cur.id }),
            comment: vec![format!("Borra el job «{}» ({}).", cur.name(), cur.id)],
        }],
        job_id: Some(cur.id),
        name: cur.name(),
        warnings: vec![RUNS_GO.to_string()],
    }
}

/// "Start at step": the task and every task after it in the graph (those
/// that depend on it, directly or through others), in the job's order.
pub(crate) fn from_task(s: &Json, step: &str) -> Result<Vec<String>> {
    let tasks: Vec<&Json> = s
        .get("tasks")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .collect();
    let key = |t: &Json| t.get("task_key").and_then(Json::as_str).map(str::to_string);
    let keys: Vec<String> = tasks.iter().filter_map(|t| key(t)).collect();
    let start = keys
        .iter()
        .find(|k| *k == step)
        .or_else(|| keys.iter().find(|k| k.eq_ignore_ascii_case(step.trim())))
        .cloned()
        .ok_or_else(|| {
            bad(format!(
                "El job no tiene una tarea llamada «{}».",
                shown(step)
            ))
        })?;
    let mut picked = BTreeSet::from([start]);
    loop {
        let before = picked.len();
        for t in &tasks {
            let Some(k) = key(t) else { continue };
            let after = t
                .get("depends_on")
                .and_then(Json::as_array)
                .into_iter()
                .flatten()
                .filter_map(|d| d.get("task_key").and_then(Json::as_str))
                .any(|d| picked.contains(d));
            if after {
                picked.insert(k);
            }
        }
        if picked.len() == before {
            break;
        }
    }
    Ok(keys.into_iter().filter(|k| picked.contains(k)).collect())
}
