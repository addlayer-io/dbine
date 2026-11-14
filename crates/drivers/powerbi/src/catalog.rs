//! Workspaces, models and capacities (spec §4.1); `connect`. Owned by part
//! B.
//!
//! - Workspaces: `GET /groups` plus "Mi área de trabajo" (`GET /datasets`);
//!   with the connection's workspace, `GET /groups?$filter=name eq '…'`
//!   (the name an OData literal, `'` doubled, URL-encoded) or the id.
//! - Models: `GET /groups/{g}/datasets`. The full dataset object
//!   (`configuredBy`, `isRefreshable`…) means write permission; readers get
//!   a reduced one.
//! - Listing names: `<model>` with the connection's workspace, else
//!   `<workspace> › <model>`; a repeated name gets ` [<first 8 hex of its
//!   id>]`. The host keeps name → `(group, dataset)` (refreshed by every
//!   listing, cached 60 s; bounded concurrency 4) and `connect` resolves
//!   through it; a GUID is a model id (models shared without workspace
//!   access, on the group-less endpoints).
//! - `connect` never scans every workspace when a copy of the listing up to
//!   [`STALE`] old already names the model: it opens it and refreshes the
//!   listing in the background. A scan it does need stays within the 20 s
//!   limit; for a model id the scan gets part of it and, if it doesn't
//!   finish or doesn't list the model, `GET /datasets/{id}` the rest.
//! - Capacities: `GET /capacities` when the scope allows; the workspace's
//!   capacity is often missing for viewers: the SKU is then unknown (G4).
//! - Every id from the app is checked against its shape (GUID) and the
//!   cached listing before any request (§3.7).

use crate::auth::Audience;
use crate::config::Settings;
use crate::http::Priority;
use crate::{http, meta, query, Ctx};
use dbine_driver::{ConnectFailure, ConnectFailureKind, ConnectionConfig, Error, Result};
use futures::StreamExt;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

/// The personal workspace's name in listings.
pub const MY_WORKSPACE: &str = "Mi área de trabajo";

/// Separator between workspace and model in listing names.
pub const SEPARATOR: &str = " › ";

/// How long a listing or the capacities stay fresh.
const TTL: Duration = Duration::from_secs(60);
/// The connect's limit beyond the sign-in.
const CONNECT_LIMIT: Duration = Duration::from_secs(20);
/// How old a listing may be for a connect to open a model it names (the
/// listing is then refreshed in the background).
const STALE: Duration = Duration::from_secs(6 * 60 * 60);

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Workspace {
    /// `None`: My workspace.
    pub id: Option<String>,
    pub name: String,
    pub on_dedicated_capacity: bool,
    pub capacity_id: Option<String>,
    pub read_only: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Capacity {
    pub id: String,
    /// `F64`, `P1`, `PP3` (Premium Per User, unverified)…
    pub sku: Option<String>,
    pub state: Option<String>,
    pub region: Option<String>,
}

/// A semantic model (the REST API's "dataset").
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Model {
    /// The dataset id (GUID).
    pub id: String,
    /// As the service names it.
    pub name: String,
    /// As [`list_databases`] names it (the session's database).
    pub listed_as: String,
    pub workspace: Workspace,
    /// The full dataset object came back: the user can write the model
    /// (refresh history, the definition API, data sources).
    pub write_permission: bool,
    pub configured_by: Option<String>,
    pub is_refreshable: Option<bool>,
    pub effective_identity_required: Option<bool>,
    pub target_storage_mode: Option<String>,
    pub web_url: Option<String>,
    pub created: Option<String>,
    /// The dataset object as the service returned it (properties read the
    /// rest from here).
    pub raw: serde_json::Value,
}

impl Model {
    /// `/groups/{g}/datasets/{d}`, or `/datasets/{d}` (My workspace, a model
    /// shared by id).
    pub fn path(&self) -> String {
        match &self.workspace.id {
            Some(g) => format!("/groups/{g}/datasets/{}", self.id),
            None => format!("/datasets/{}", self.id),
        }
    }
}

/// Whether `s` is a GUID (`8-4-4-4-12` hex digits).
pub fn valid_guid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Percent-encoding of everything but the unreserved characters.
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `GET /groups?$filter=name eq '<name>'`: an OData literal (`'` doubled),
/// URL-encoded.
fn group_filter_path(name: &str) -> String {
    format!(
        "/groups?$filter={}",
        pct(&format!("name eq '{}'", name.replace('\'', "''")))
    )
}

fn is_my_workspace(w: &str) -> bool {
    w.eq_ignore_ascii_case(MY_WORKSPACE) || w.eq_ignore_ascii_case("me")
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(Value::as_str).map(str::to_string)
}

fn workspace_of(v: &Value) -> Option<Workspace> {
    let id = s(v, "id").filter(|i| valid_guid(i))?;
    Some(Workspace {
        id: Some(id),
        name: s(v, "name").unwrap_or_default(),
        on_dedicated_capacity: v
            .get("isOnDedicatedCapacity")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        capacity_id: s(v, "capacityId").filter(|i| valid_guid(i)),
        read_only: v
            .get("isReadOnly")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn my_workspace() -> Workspace {
    Workspace {
        id: None,
        name: MY_WORKSPACE.to_string(),
        ..Default::default()
    }
}

/// A dataset object of the listing (ids that aren't GUIDs are dropped).
fn model_of(v: &Value, ws: &Workspace) -> Option<Model> {
    let id = s(v, "id").filter(|i| valid_guid(i))?;
    let b = |k: &str| v.get(k).and_then(Value::as_bool);
    Some(Model {
        name: s(v, "name").unwrap_or_else(|| id.clone()),
        id,
        listed_as: String::new(),
        workspace: ws.clone(),
        write_permission: v.get("configuredBy").is_some() || v.get("isRefreshable").is_some(),
        configured_by: s(v, "configuredBy"),
        is_refreshable: b("isRefreshable"),
        effective_identity_required: b("isEffectiveIdentityRequired"),
        target_storage_mode: s(v, "targetStorageMode"),
        web_url: s(v, "webUrl"),
        created: s(v, "createdDate"),
        raw: v.clone(),
    })
}

fn values(v: &Value) -> Vec<Value> {
    v.get("value")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// Listing names: `<model>` (one workspace) or `<workspace> › <model>`; a
/// name used twice gets ` [<first 8 hex of the id>]` on every copy.
fn name_models(models: &mut [Model], one_workspace: bool) {
    for m in models.iter_mut() {
        m.listed_as = if one_workspace {
            m.name.clone()
        } else {
            format!("{}{SEPARATOR}{}", m.workspace.name, m.name)
        };
    }
    let mut count: HashMap<String, usize> = HashMap::new();
    for m in models.iter() {
        *count.entry(m.listed_as.to_lowercase()).or_default() += 1;
    }
    for m in models.iter_mut() {
        if count[&m.listed_as.to_lowercase()] > 1 {
            m.listed_as = format!("{} [{}]", m.listed_as, &m.id[..8]);
        }
    }
}

// ------------------------------------------------------------ host caches

struct Listing {
    at: Instant,
    models: Arc<Vec<Model>>,
}

/// The capacities an account sees (`None`: it may see none), and when.
type Capacities = (Instant, Option<Arc<Vec<Capacity>>>);

static LISTINGS: LazyLock<Mutex<HashMap<String, Listing>>> = LazyLock::new(Mutex::default);
static CAPACITIES: LazyLock<Mutex<HashMap<String, Capacities>>> = LazyLock::new(Mutex::default);
/// Listings being refreshed in the background, by key.
static REFRESHING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Mutex::default);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whose listing: the account, the cloud and the connection's workspace.
fn listing_key(cx: &Ctx) -> String {
    let a = cx.api.account();
    format!(
        "{:?}|{}|{}|{}",
        cx.settings.cloud,
        a.as_ref().map_or("", |a| a.tenant_id.as_str()),
        a.as_ref().map_or("", |a| a.object_id.as_str()),
        cx.settings
            .workspace
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
    )
}

fn not_found_workspace(w: &str) -> Error {
    Error::Query(format!(
        "No se encontró el área de trabajo «{w}», o tu cuenta no tiene acceso a ella."
    ))
}

/// The connection's workspaces (none and `true`: My workspace only).
async fn workspaces(cx: &Ctx) -> Result<(Vec<Workspace>, bool)> {
    let api = &cx.api;
    let all = || async {
        let mut out = Vec::new();
        let mut skip = 0usize;
        loop {
            let page = api
                .rest_get(
                    &format!("/groups?$top=5000&$skip={skip}"),
                    Priority::Background,
                )
                .await?;
            let items = values(&page);
            let n = items.len();
            out.extend(items.iter().filter_map(workspace_of));
            if n < 5000 {
                break;
            }
            skip += n;
        }
        Ok::<_, Error>(out)
    };
    match cx.settings.workspace.as_deref() {
        None => Ok((all().await?, true)),
        Some(w) if is_my_workspace(w) => Ok((Vec::new(), true)),
        Some(w) if valid_guid(w) => {
            let found: Vec<Workspace> = all()
                .await?
                .into_iter()
                .filter(|x| x.id.as_deref().is_some_and(|i| i.eq_ignore_ascii_case(w)))
                .collect();
            if found.is_empty() {
                return Err(not_found_workspace(w));
            }
            Ok((found, false))
        }
        Some(w) => {
            let found: Vec<Workspace> = values(
                &api.rest_get(&group_filter_path(w), Priority::Background)
                    .await?,
            )
            .iter()
            .filter_map(workspace_of)
            .filter(|x| x.name.eq_ignore_ascii_case(w))
            .collect();
            if found.is_empty() {
                return Err(not_found_workspace(w));
            }
            Ok((found, false))
        }
    }
}

/// Every model the connection reaches, named for the listing.
async fn fetch(cx: &Ctx) -> Result<Vec<Model>> {
    let (spaces, mine) = workspaces(cx).await?;
    let explicit = cx.settings.workspace.is_some();
    let api = &cx.api;
    let lists: Vec<(Workspace, Result<Value>)> = futures::stream::iter(spaces)
        .map(|ws| async move {
            let path = format!("/groups/{}/datasets", ws.id.as_deref().unwrap_or(""));
            let r = api.rest_get(&path, Priority::Background).await;
            (ws, r)
        })
        .buffered(4)
        .collect()
        .await;
    let mut models = Vec::new();
    let mut first_error = None;
    let mut any_ok = false;
    for (ws, r) in lists {
        match r {
            Ok(v) => {
                any_ok = true;
                models.extend(values(&v).iter().filter_map(|d| model_of(d, &ws)));
            }
            Err(e) if explicit => return Err(e),
            Err(e) => {
                // The status and code went to the log already (http.rs).
                tracing::debug!("Power BI: a workspace's models couldn't be listed");
                first_error.get_or_insert(e);
            }
        }
    }
    if mine {
        match api.rest_get("/datasets", Priority::Background).await {
            Ok(v) => {
                any_ok = true;
                let ws = my_workspace();
                models.extend(values(&v).iter().filter_map(|d| model_of(d, &ws)));
            }
            // A service principal has no personal workspace.
            Err(e) if explicit => return Err(e),
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    if !any_ok {
        if let Some(e) = first_error {
            return Err(e);
        }
    }
    name_models(&mut models, explicit);
    Ok(models)
}

/// The listing, from the host's cache while it's fresh (`refresh`: always
/// asked again).
async fn listing(cx: &Ctx, refresh: bool) -> Result<Arc<Vec<Model>>> {
    let key = listing_key(cx);
    if !refresh {
        if let Some(l) = lock(&LISTINGS).get(&key) {
            if l.at.elapsed() < TTL {
                return Ok(l.models.clone());
            }
        }
    }
    let models = Arc::new(fetch(cx).await?);
    lock(&LISTINGS).insert(
        key,
        Listing {
            at: Instant::now(),
            models: models.clone(),
        },
    );
    Ok(models)
}

/// The host's copy of the listing up to `max_age` old, without asking: the
/// models and whether they're still fresh ([`TTL`]).
fn cached_listing(cx: &Ctx, max_age: Duration) -> Option<(Arc<Vec<Model>>, bool)> {
    lock(&LISTINGS)
        .get(&listing_key(cx))
        .filter(|l| l.at.elapsed() < max_age)
        .map(|l| (l.models.clone(), l.at.elapsed() < TTL))
}

/// Refreshes the listing in the background (one at a time per listing),
/// on a client of its own: a stale copy just served a connect.
fn refresh_later(cx: &Ctx) {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let key = listing_key(cx);
    if !lock(&REFRESHING).insert(key.clone()) {
        return;
    }
    let api = match cx.api.sibling(&cx.settings) {
        Ok(api) => api,
        Err(_) => {
            lock(&REFRESHING).remove(&key);
            return;
        }
    };
    let bg = Ctx {
        settings: cx.settings.clone(),
        api,
        model: None,
        query: query::QueryState::default(),
        meta: meta::MetaState::default(),
    };
    handle.spawn(async move {
        // The token first: the listing is keyed by the account.
        let done = match bg.api.tokens.token(Audience::PowerBi).await {
            Ok(_) => listing(&bg, true).await.map(|_| ()),
            Err(e) => Err(e),
        };
        if done.is_err() {
            // The status and code went to the log already (http.rs).
            tracing::debug!("Power BI: the model listing couldn't be refreshed in the background");
        }
        lock(&REFRESHING).remove(&key);
    });
}

/// Open a session: settings, the client (no network), then the model
/// `database` (else the connection's) resolved through the listing; none
/// named: a session without a model (it lists models only). 20 s limit
/// beyond the sign-in.
pub async fn connect(cfg: &ConnectionConfig, database: Option<&str>) -> Result<Ctx> {
    let settings = Settings::from_config(cfg)?;
    let api = http::Api::new(&settings)?;
    open(settings, api, database).await
}

pub(crate) async fn open(
    settings: Settings,
    api: http::Api,
    database: Option<&str>,
) -> Result<Ctx> {
    open_within(settings, api, database, CONNECT_LIMIT).await
}

async fn open_within(
    settings: Settings,
    api: http::Api,
    database: Option<&str>,
    limit: Duration,
) -> Result<Ctx> {
    // The sign-in first (a browser sign-in may take minutes).
    api.tokens.token(Audience::PowerBi).await?;
    let wanted = database
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_string)
        .or_else(|| settings.database.clone());
    let mut cx = Ctx {
        settings,
        api,
        model: None,
        query: query::QueryState::default(),
        meta: meta::MetaState::default(),
    };
    if let Some(name) = wanted {
        let found = resolve_within(&mut cx, &name, Some(limit)).await;
        cx.model = Some(found.map_err(|e| match e {
            Error::Query(m) => Error::Connect(m),
            other => other,
        })?);
    }
    Ok(cx)
}

/// The connect took longer than its limit while looking for the model.
fn slow(scanning: bool) -> Error {
    let secs = CONNECT_LIMIT.as_secs();
    Error::connect_failed(ConnectFailure::new(
        ConnectFailureKind::Timeout,
        if scanning {
            format!("Power BI no respondió en {secs} s al buscar el modelo entre las áreas de trabajo: indicá el área de trabajo en la conexión, o el ID del modelo.")
        } else {
            format!("Power BI no respondió en {secs} s al buscar el modelo")
        },
    ))
}

fn license(sku: &str) -> String {
    let u = sku.trim().to_ascii_uppercase();
    if u.starts_with("PP") {
        "Premium por usuario".into()
    } else if u.starts_with("FT") {
        "Fabric (prueba)".into()
    } else if u.starts_with('F') {
        format!("Fabric {u}")
    } else if u.starts_with('P') {
        format!("Premium {u}")
    } else if u.starts_with('A') || u.starts_with("EM") {
        format!("Embedded {u}")
    } else {
        format!("capacidad {u}")
    }
}

/// "Power BI · <workspace> · <license or SKU> · <query API>" (G4: an
/// unknown SKU says so).
pub async fn server_version(cx: &mut Ctx) -> Result<String> {
    let Some(m) = cx.model.clone() else {
        let ws = cx
            .settings
            .workspace
            .clone()
            .unwrap_or_else(|| "todas las áreas de trabajo".into());
        return Ok(format!("Power BI · {ws}"));
    };
    let lic = if !m.workspace.on_dedicated_capacity {
        "Pro (capacidad compartida)".to_string()
    } else {
        let cap = match &m.workspace.capacity_id {
            Some(id) => capacity(cx, id).await.ok().flatten(),
            None => None,
        };
        match cap.and_then(|c| c.sku) {
            Some(sku) => {
                cx.api.set_capacity_sku(Some(&sku));
                license(&sku)
            }
            None => "capacidad (sin permiso para ver el SKU)".into(),
        }
    };
    let api = match query::api_expected(cx) {
        Some(query::ApiKind::Json) => "executeQueries",
        Some(query::ApiKind::Arrow) => "executeDaxQueries",
        None => "executeDaxQueries o executeQueries (automática)",
    };
    Ok(format!("Power BI · {} · {lic} · {api}", m.workspace.name))
}

/// Every model the connection reaches, by its listing name.
pub async fn list_databases(cx: &mut Ctx) -> Result<Vec<String>> {
    let models = listing(cx, true).await?;
    Ok(models.iter().map(|m| m.listed_as.clone()).collect())
}

/// The session's model, or the error that says to pick one.
pub fn model(cx: &Ctx) -> Result<&Model> {
    cx.model.as_ref().ok_or_else(|| {
        Error::Query("Elegí un modelo semántico: esta sesión no tiene uno abierto.".into())
    })
}

/// Every model listed (as [`list_databases`] names them), with their
/// workspaces.
pub async fn models(cx: &mut Ctx) -> Result<Vec<Model>> {
    Ok(listing(cx, false).await?.as_ref().clone())
}

/// The models of the session's workspace (refresh jobs and schedules): the
/// open model's workspace, else the connection's (or every model).
pub async fn workspace_models(cx: &mut Ctx) -> Result<Vec<Model>> {
    let ws = cx.model.as_ref().map(|m| m.workspace.id.clone());
    let all = models(cx).await?;
    Ok(match ws {
        Some(id) => all.into_iter().filter(|m| m.workspace.id == id).collect(),
        None => all,
    })
}

/// What a listing says about a name or id.
enum Found {
    One(Box<Model>),
    /// Several models take a bare name: their listing names.
    Many(Vec<String>),
    None,
}

fn find(models: &[Model], listed: &str) -> Found {
    if valid_guid(listed) {
        return match models.iter().find(|m| m.id.eq_ignore_ascii_case(listed)) {
            Some(m) => Found::One(Box::new(m.clone())),
            None => Found::None,
        };
    }
    if let Some(m) = models.iter().find(|m| m.listed_as == listed) {
        return Found::One(Box::new(m.clone()));
    }
    if let Some(m) = models
        .iter()
        .find(|m| m.listed_as.eq_ignore_ascii_case(listed))
    {
        return Found::One(Box::new(m.clone()));
    }
    // A bare model name, when only one model has it.
    let same: Vec<&Model> = models
        .iter()
        .filter(|m| m.name.eq_ignore_ascii_case(listed))
        .collect();
    match same.as_slice() {
        [one] => Found::One(Box::new((*one).clone())),
        [] => Found::None,
        many => Found::Many(many.iter().map(|m| m.listed_as.clone()).collect()),
    }
}

fn many(listed: &str, names: &[String]) -> Error {
    Error::Query(format!(
        "Hay varios modelos «{listed}»: elegí uno de la lista ({}).",
        names.join(", ")
    ))
}

/// A model by its id alone (`GET /datasets/{id}`): one shared with the
/// account without access to its workspace, on the group-less endpoints.
async fn by_id(cx: &Ctx, id: &str) -> Result<Model> {
    let v = cx
        .api
        .rest_get(&format!("/datasets/{id}"), Priority::Background)
        .await?;
    let mut m = model_of(&v, &my_workspace())
        .filter(|m| m.id.eq_ignore_ascii_case(id))
        .ok_or_else(|| Error::Query(format!("No se encontró el modelo «{id}».")))?;
    m.workspace.name = String::new();
    m.listed_as = m.name.clone();
    Ok(m)
}

/// A model by its listing name (or a GUID), checked against the listing
/// (fresh, else listed again).
pub async fn resolve(cx: &mut Ctx, listed: &str) -> Result<Model> {
    resolve_within(cx, listed, None).await
}

/// [`resolve`] for a connect (`limit`): a copy of the listing up to
/// [`STALE`] old that names the model is enough (the listing is refreshed
/// in the background), and a scan stays within the limit. For an id the
/// scan gets three fifths of it, and the model itself the rest.
async fn resolve_within(cx: &mut Ctx, listed: &str, limit: Option<Duration>) -> Result<Model> {
    let start = Instant::now();
    let listed = listed.trim();
    let guid = valid_guid(listed);
    let max_age = if limit.is_some() { STALE } else { TTL };
    if let Some((models, fresh)) = cached_listing(cx, max_age) {
        match find(&models, listed) {
            Found::One(m) => {
                if !fresh {
                    refresh_later(cx);
                }
                return Ok(*m);
            }
            // An old copy may be out of date: ask again.
            Found::Many(names) if fresh => return Err(many(listed, &names)),
            _ => {}
        }
    }
    let scan = listing(cx, true);
    let scanned = match limit {
        None => Some(scan.await),
        Some(l) => {
            let share = if guid { l * 3 / 5 } else { l };
            tokio::time::timeout(share, scan).await.ok()
        }
    };
    match scanned {
        Some(Ok(models)) => match find(&models, listed) {
            Found::One(m) => return Ok(*m),
            Found::Many(names) => return Err(many(listed, &names)),
            Found::None => {}
        },
        // An id: the model itself may still answer.
        Some(Err(_)) | None if guid => {}
        Some(Err(e)) => return Err(e),
        None => return Err(slow(true)),
    }
    if guid {
        return match limit {
            None => by_id(cx, listed).await,
            Some(l) => {
                let rest = l.saturating_sub(start.elapsed()).max(l * 2 / 5);
                tokio::time::timeout(rest, by_id(cx, listed))
                    .await
                    .map_err(|_| slow(false))?
            }
        };
    }
    Err(Error::Query(format!(
        "No se encontró el modelo «{listed}», o tu cuenta no tiene acceso a él."
    )))
}

/// A capacity's SKU, state and region; `None` when the user can't see it.
pub async fn capacity(cx: &mut Ctx, id: &str) -> Result<Option<Capacity>> {
    if !valid_guid(id) {
        return Ok(None);
    }
    let key = listing_key(cx);
    let cached = lock(&CAPACITIES)
        .get(&key)
        .filter(|(at, _)| at.elapsed() < TTL)
        .map(|(_, c)| c.clone());
    let list = match cached {
        Some(c) => c,
        None => {
            let list = match cx.api.rest_get("/capacities", Priority::Background).await {
                Ok(v) => Some(Arc::new(
                    values(&v)
                        .iter()
                        .filter_map(|c| {
                            Some(Capacity {
                                id: s(c, "id").filter(|i| valid_guid(i))?,
                                sku: s(c, "sku"),
                                state: s(c, "state"),
                                region: s(c, "region"),
                            })
                        })
                        .collect(),
                )),
                // No `Capacity.Read.All`, or no right on any capacity.
                Err(Error::Query(_) | Error::AuthFailed(_)) => None,
                Err(e) => return Err(e),
            };
            lock(&CAPACITIES).insert(key, (Instant::now(), list.clone()));
            list
        }
    };
    Ok(list.and_then(|l| l.iter().find(|c| c.id.eq_ignore_ascii_case(id)).cloned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::testing::{serve, settings, Reply, Request};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const GROUPS: &str = include_str!("../tests/fixtures/groups.json");
    const DATASETS: &str = include_str!("../tests/fixtures/datasets_in_group.json");
    const DATASETS_READER: &str = include_str!("../tests/fixtures/datasets_reader.json");
    const CAPACITIES_JSON: &str = include_str!("../tests/fixtures/capacities.json");

    #[test]
    fn guids_and_paths() {
        assert!(valid_guid("cfafbeb1-8037-4d0c-896e-a46fb27ff229"));
        for bad in [
            "",
            "cfafbeb1-8037-4d0c-896e-a46fb27ff22",
            "cfafbeb1-8037-4d0c-896e-a46fb27ff22g",
            "cfafbeb1/8037-4d0c-896e-a46fb27ff229",
            "../../../groups/x/datasets/y",
        ] {
            assert!(!valid_guid(bad), "{bad}");
        }
        let mut m = Model {
            id: "d".into(),
            ..Default::default()
        };
        assert_eq!(m.path(), "/datasets/d");
        m.workspace.id = Some("g".into());
        assert_eq!(m.path(), "/groups/g/datasets/d");
    }

    #[test]
    fn workspace_filters_are_odata_literals() {
        assert_eq!(
            group_filter_path("O'Brien & Co"),
            "/groups?$filter=name%20eq%20%27O%27%27Brien%20%26%20Co%27"
        );
    }

    #[test]
    fn repeated_names_get_the_id() {
        let ws = |n: &str| Workspace {
            id: Some("f089354e-8366-4e18-aea3-4cb4a3a50b48".into()),
            name: n.into(),
            ..Default::default()
        };
        let m = |id: &str, name: &str, w: &str| Model {
            id: id.into(),
            name: name.into(),
            workspace: ws(w),
            ..Default::default()
        };
        let mut all = vec![
            m("aaaaaaaa-0000-0000-0000-000000000000", "Ventas", "A"),
            m("bbbbbbbb-0000-0000-0000-000000000000", "Ventas", "A"),
            m("cccccccc-0000-0000-0000-000000000000", "Ventas", "B"),
        ];
        name_models(&mut all, false);
        let names: Vec<&str> = all.iter().map(|m| m.listed_as.as_str()).collect();
        assert_eq!(
            names,
            [
                "A › Ventas [aaaaaaaa]",
                "A › Ventas [bbbbbbbb]",
                "B › Ventas"
            ]
        );
        name_models(&mut all, true);
        assert_eq!(all[2].listed_as, "Ventas [cccccccc]");
    }

    fn fake() -> crate::http::testing::Handler {
        Arc::new(|req: &Request| {
            let p = req.path.as_str();
            if p.starts_with("/v1.0/myorg/groups?$top=5000") {
                Reply::json(200, GROUPS)
            } else if p.starts_with("/v1.0/myorg/groups?$filter=") {
                if p.contains("contoso") {
                    let v: Value = serde_json::from_str(GROUPS).unwrap();
                    let only = serde_json::json!({"value": [v["value"][2].clone()]});
                    Reply::json(200, &only.to_string())
                } else {
                    Reply::json(200, r#"{"value":[]}"#)
                }
            } else if p == "/v1.0/myorg/groups/a2f89923-421a-464e-bf4c-25eab39bb09f/datasets" {
                Reply::json(200, DATASETS)
            } else if p.ends_with("/datasets") && p.contains("/groups/") {
                Reply::json(200, DATASETS_READER)
            } else if p == "/v1.0/myorg/datasets" {
                Reply::json(
                    200,
                    r#"{"value":[{"id":"dddddddd-1111-2222-3333-444444444444","name":"Personal"}]}"#,
                )
            } else if p == "/v1.0/myorg/datasets/eeeeeeee-1111-2222-3333-444444444444" {
                Reply::json(
                    200,
                    r#"{"id":"eeeeeeee-1111-2222-3333-444444444444","name":"Compartido"}"#,
                )
            } else if p == "/v1.0/myorg/capacities" {
                Reply::json(200, CAPACITIES_JSON)
            } else {
                Reply::json(404, r#"{"error":{"code":"ItemNotFound"}}"#)
            }
        })
    }

    #[tokio::test]
    async fn every_workspace_and_my_workspace() {
        let base = serve(fake()).await;
        let s = settings(&[], true);
        let api = http::Api::with_endpoints(&s, crate::auth::Endpoints::test(&base)).unwrap();
        let mut cx = open(s, api, None).await.unwrap();
        assert!(cx.model.is_none());
        let names = list_databases(&mut cx).await.unwrap();
        assert!(
            names.contains(&"contoso › SalesMarketing".to_string()),
            "{names:?}"
        );
        assert!(
            names.contains(&format!("{MY_WORKSPACE} › Personal")),
            "{names:?}"
        );
        // The reader's (reduced) objects of two workspaces.
        assert!(
            names.contains(&"sample group › Ventas".to_string()),
            "{names:?}"
        );
        let all = models(&mut cx).await.unwrap();
        let full = all.iter().find(|m| m.name == "SalesMarketing").unwrap();
        assert!(full.write_permission);
        assert_eq!(full.configured_by.as_deref(), Some("john@contoso.com"));
        assert!(full.workspace.on_dedicated_capacity);
        let reduced = all.iter().find(|m| m.name == "Ventas").unwrap();
        assert!(!reduced.write_permission && reduced.configured_by.is_none());
        // Resolved by listing name, bare name, or id.
        let m = resolve(&mut cx, "contoso › SalesMarketing").await.unwrap();
        assert_eq!(m.path(), "/groups/a2f89923-421a-464e-bf4c-25eab39bb09f/datasets/cfafbeb1-8037-4d0c-896e-a46fb27ff229");
        assert_eq!(resolve(&mut cx, "salesmarketing").await.unwrap().id, m.id);
        let e = resolve(&mut cx, "Ventas").await.unwrap_err();
        assert!(e.to_string().contains("varios modelos"), "{e}");
        let shared = resolve(&mut cx, "eeeeeeee-1111-2222-3333-444444444444")
            .await
            .unwrap();
        assert_eq!(
            shared.path(),
            "/datasets/eeeeeeee-1111-2222-3333-444444444444"
        );
        assert!(resolve(&mut cx, "Nada").await.is_err());
    }

    #[tokio::test]
    async fn one_workspace_by_name_and_the_capacity() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let inner = fake();
        let base = serve(Arc::new(move |req: &Request| {
            c.fetch_add(1, Ordering::SeqCst);
            inner(req)
        }))
        .await;
        let s = settings(&[("workspace", "contoso")], true);
        let api = http::Api::with_endpoints(&s, crate::auth::Endpoints::test(&base)).unwrap();
        let mut cx = open(s, api, Some("SalesMarketing")).await.unwrap();
        let m = cx.model.clone().unwrap();
        assert_eq!(m.listed_as, "SalesMarketing");
        assert_eq!(list_databases(&mut cx).await.unwrap(), ["SalesMarketing"]);
        let v = server_version(&mut cx).await.unwrap();
        assert_eq!(
            v,
            "Power BI · contoso · Embedded A1 · executeDaxQueries o executeQueries (automática)"
        );
        assert_eq!(cx.api.query_rate(true), 120);
        let before = calls.load(Ordering::SeqCst);
        assert_eq!(workspace_models(&mut cx).await.unwrap().len(), 1);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            before,
            "the listing is cached"
        );
        // An unknown workspace or model.
        let s = settings(&[("workspace", "nope")], true);
        let api = http::Api::with_endpoints(&s, crate::auth::Endpoints::test(&base)).unwrap();
        let mut cx = open(s, api, None).await.unwrap();
        assert!(list_databases(&mut cx)
            .await
            .unwrap_err()
            .to_string()
            .contains("«nope»"));
        let s = settings(&[("workspace", "contoso")], true);
        let api = http::Api::with_endpoints(&s, crate::auth::Endpoints::test(&base)).unwrap();
        let Err(e) = open(s, api, Some("Otro")).await else {
            panic!("a model that isn't there opened");
        };
        assert!(matches!(e, Error::Connect(_)), "{e}");
    }

    /// The fake of [`fake`] whose workspace listing takes `slow` while the
    /// flag is on, counting `GET /groups` calls.
    fn slow_fake(
        slow: Duration,
        on: Arc<std::sync::atomic::AtomicBool>,
        groups: Arc<AtomicUsize>,
    ) -> crate::http::testing::Handler {
        let inner = fake();
        Arc::new(move |req: &Request| {
            let reply = inner(req);
            if req.path.starts_with("/v1.0/myorg/groups?") {
                groups.fetch_add(1, Ordering::SeqCst);
                if on.load(Ordering::SeqCst) {
                    return reply.after(slow);
                }
            }
            reply
        })
    }

    #[tokio::test]
    async fn a_stale_listing_opens_the_model_and_is_refreshed_behind() {
        let on = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let groups = Arc::new(AtomicUsize::new(0));
        let base = serve(slow_fake(
            Duration::from_millis(400),
            on.clone(),
            groups.clone(),
        ))
        .await;
        let s = settings(&[], true);
        let api = http::Api::with_endpoints(&s, crate::auth::Endpoints::test(&base)).unwrap();
        let mut cx = open(s.clone(), api, None).await.unwrap();
        list_databases(&mut cx).await.unwrap();
        assert_eq!(groups.load(Ordering::SeqCst), 1);
        // Older than the 60 s the listing stays fresh.
        let key = listing_key(&cx);
        if let Some(l) = lock(&LISTINGS).get_mut(&key) {
            l.at = Instant::now()
                .checked_sub(TTL * 2)
                .unwrap_or_else(Instant::now);
        }
        // Every workspace now answers slower than the connect's limit: the
        // old copy opens the model all the same, without a scan in the way.
        on.store(true, Ordering::SeqCst);
        let api = http::Api::with_endpoints(&s, crate::auth::Endpoints::test(&base)).unwrap();
        let cx = open_within(
            s.clone(),
            api,
            Some("contoso › SalesMarketing"),
            Duration::from_millis(150),
        )
        .await
        .unwrap();
        assert_eq!(
            cx.model.as_ref().unwrap().id,
            "cfafbeb1-8037-4d0c-896e-a46fb27ff229"
        );
        // …and the listing is asked again behind it.
        for _ in 0..100 {
            let fresh = lock(&LISTINGS)
                .get(&key)
                .is_some_and(|l| l.at.elapsed() < TTL);
            if fresh && !lock(&REFRESHING).contains(&key) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(groups.load(Ordering::SeqCst), 2);
        assert!(lock(&LISTINGS)
            .get(&key)
            .is_some_and(|l| l.at.elapsed() < TTL));
    }

    #[tokio::test]
    async fn a_slow_scan_still_opens_a_model_by_its_id() {
        let on = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let groups = Arc::new(AtomicUsize::new(0));
        let base = serve(slow_fake(Duration::from_millis(800), on, groups)).await;
        // An id: the scan gets part of the limit, the model itself the rest.
        let s = settings(&[], true);
        let api = http::Api::with_endpoints(&s, crate::auth::Endpoints::test(&base)).unwrap();
        let cx = open_within(
            s,
            api,
            Some("eeeeeeee-1111-2222-3333-444444444444"),
            Duration::from_millis(300),
        )
        .await
        .unwrap();
        assert_eq!(
            cx.model.as_ref().unwrap().path(),
            "/datasets/eeeeeeee-1111-2222-3333-444444444444"
        );
        // A name with no listing to go by: the timeout says what to do.
        let s = settings(&[], true);
        let api = http::Api::with_endpoints(&s, crate::auth::Endpoints::test(&base)).unwrap();
        let Err(e) = open_within(s, api, Some("SalesMarketing"), Duration::from_millis(300)).await
        else {
            panic!("a scan past the limit opened");
        };
        assert!(matches!(e, Error::ConnectFailed(_)), "{e}");
        assert!(e.to_string().contains("indicá el área de trabajo"), "{e}");
    }

    #[test]
    fn licenses() {
        assert_eq!(license("PP3"), "Premium por usuario");
        assert_eq!(license("F64"), "Fabric F64");
        assert_eq!(license("FT1"), "Fabric (prueba)");
        assert_eq!(license("P1"), "Premium P1");
        assert_eq!(license("EM2"), "Embedded EM2");
    }
}
