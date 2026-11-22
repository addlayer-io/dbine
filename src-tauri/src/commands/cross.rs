//! Cross-connection lineage: from a source a lineage names outside its
//! database (a Power BI table's SQL Server table) to the saved connection
//! that holds it, and the other way, who reads an object from another
//! connection (the Power BI tables that read a SQL table).
//! Matching rules and the links the user made: `dbine_core::lineage_link`.

use crate::commands::explorer::{object_page, ObjectPageArgs};
use crate::commands::lineage_cache;
use crate::error::{CommandError, CommandResult};
use crate::state::{AppState, Lane};
use dbine_core::cache::kinds as cache_kinds;
use dbine_core::lineage_link::{
    match_connections, new_connection_from_source, source_key_of_config, source_key_of_external,
    Match, SourceKey,
};
use dbine_core::secrets::{self, Secrets};
use dbine_core::state::SavedConnection;
use dbine_driver::dependencies::{ExternalRef, LineageExternal, LineageGraph, LineageScope};
use dbine_driver::{ConnectionConfig, DbObject, ObjectQuery, ObjectRef};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::State;

/// How long a Power BI model's sources are kept.
const INDEX_TTL: Duration = Duration::from_secs(15 * 60);
/// A model's lineage reads its whole definition: the long lane's ceiling.
const SCAN_LIMIT: Duration = Duration::from_secs(300);
/// Settings (seconds): the longest one model's sources are waited for, and the longest a whole search runs
/// (what isn't read by then is left out, and a note says how many: the models already read are kept, so
/// searching again goes on from there). The user raises them for very large models.
pub const MODEL_LIMIT_SETTING: &str = "lineage_model_timeout_secs";
pub const SCAN_BUDGET_SETTING: &str = "lineage_search_budget_secs";
const MODEL_LIMIT_DEFAULT: u64 = 180;
const SCAN_BUDGET_DEFAULT: u64 = 900;
/// Nobody waits more than an hour for one read.
const LIMIT_MAX: u64 = 3600;

fn secs_setting(value: Option<&serde_json::Value>, default: u64) -> Duration {
    let n = value
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
        .filter(|n| *n >= 10)
        .unwrap_or(default);
    Duration::from_secs(n.min(LIMIT_MAX))
}

/// (per model, whole search) from the settings. The search is never shorter than one model.
pub(crate) fn scan_limits(state: &AppState) -> (Duration, Duration) {
    let get = |k: &str| state.store.get_setting(k).ok().flatten();
    let model = secs_setting(get(MODEL_LIMIT_SETTING).as_ref(), MODEL_LIMIT_DEFAULT);
    let budget = secs_setting(get(SCAN_BUDGET_SETTING).as_ref(), SCAN_BUDGET_DEFAULT);
    (model, budget.max(model))
}
/// Setting: models read at once per Power BI connection (the Settings page offers 1 to 16).
pub const SCAN_PARALLEL_SETTING: &str = "lineage_scan_parallel";
const SCAN_PARALLEL_DEFAULT: usize = 4;
const SCAN_PARALLEL_MAX: usize = 16;

fn parallel_setting(value: Option<&serde_json::Value>) -> usize {
    value
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f.max(0.0) as u64)))
        .map(|n| (n as usize).clamp(1, SCAN_PARALLEL_MAX))
        .unwrap_or(SCAN_PARALLEL_DEFAULT)
}

/// Models read at once per Power BI connection, from the settings (1 to 16, 4 when unset).
pub(crate) fn scan_parallel(state: &AppState) -> usize {
    parallel_setting(
        state
            .store
            .get_setting(SCAN_PARALLEL_SETTING)
            .ok()
            .flatten()
            .as_ref(),
    )
}

// -- the in-memory index ------------------------------------------------------

/// A map whose entries expire after `ttl`.
struct TtlCache<K, V> {
    ttl: Duration,
    map: HashMap<K, (Instant, V)>,
}

impl<K: Eq + Hash, V: Clone> TtlCache<K, V> {
    fn new(ttl: Duration) -> Self {
        TtlCache {
            ttl,
            map: HashMap::new(),
        }
    }

    fn get(&self, k: &K, now: Instant) -> Option<V> {
        let (at, v) = self.map.get(k)?;
        (now.saturating_duration_since(*at) < self.ttl).then(|| v.clone())
    }

    fn put(&mut self, k: K, v: V, now: Instant) {
        self.map.insert(k, (now, v));
    }

    fn drop_where(&mut self, f: impl Fn(&K) -> bool) {
        self.map.retain(|k, _| !f(k));
    }
}

type Sources = Arc<Vec<LineageExternal>>;

struct Index {
    /// (connection, model) -> what its tables read outside.
    sources: TtlCache<(String, String), Sources>,
    /// connection -> its models.
    models: TtlCache<String, Arc<Vec<String>>>,
    /// source signature -> the managed connections DBine Enterprise resolved it to.
    resolved: TtlCache<String, Vec<String>>,
    /// connection -> what the admin scanner said about its tenant (`None`: it can't be used by this user).
    scans: TtlCache<String, Option<Arc<ScanHint>>>,
}

fn index() -> &'static Mutex<Index> {
    static INDEX: OnceLock<Mutex<Index>> = OnceLock::new();
    INDEX.get_or_init(|| {
        Mutex::new(Index {
            sources: TtlCache::new(INDEX_TTL),
            models: TtlCache::new(INDEX_TTL),
            resolved: TtlCache::new(INDEX_TTL),
            scans: TtlCache::new(INDEX_TTL),
        })
    })
}

fn with_index<T>(f: impl FnOnce(&mut Index) -> T) -> T {
    let mut g = index().lock().unwrap_or_else(|e| e.into_inner());
    f(&mut g)
}

// -- the admin scanner's pre-filter --------------------------------------------

/// What Power BI's admin scanner API says about a tenant: for each model it covers, the data sources it
/// names (server and database, no tables; none at all for a model whose sources it doesn't list). It
/// says nothing about the models it doesn't cover.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub(crate) struct ScanHint {
    pub models: HashMap<String, Vec<ExternalRef>>,
}

impl ScanHint {
    pub(crate) fn of(graph: &LineageGraph) -> ScanHint {
        let mut models: HashMap<String, Vec<ExternalRef>> = graph
            .objects
            .iter()
            .map(|o| (o.name.clone(), Vec::new()))
            .collect();
        for e in &graph.external {
            models
                .entry(e.to.name.clone())
                .or_default()
                .push(e.source.clone());
        }
        ScanHint { models }
    }

    fn sources(&self) -> impl Iterator<Item = &ExternalRef> {
        self.models.values().flatten()
    }
}

/// The engines whose data source the scanner names the way a Power Query source does, so that the same
/// canonical key can be trusted to tell "not this server" from "a spelling I don't know". A source of any
/// other engine (Oracle, files, analysis services, extensions…) is never used to leave a model out.
const DECIDABLE_FAMILIES: [&str; 5] = ["sqlserver", "postgres", "mysql", "snowflake", "databricks"];

/// Whether a model with these data sources may read what `reaches` accepts. It may when it lists none (the
/// scanner can't say), when a source can't be decided on (no server, a file, an engine the key doesn't
/// cover) or when one points to a server and database `reaches` accepts. Only a model whose every source
/// is decided and none of them is the connection's is left out.
pub(crate) fn model_may_read(
    sources: &[ExternalRef],
    reaches: &dyn Fn(&SourceKey) -> bool,
) -> bool {
    sources.is_empty()
        || sources.iter().any(|e| {
            match source_key_of_external(e)
                .filter(|k| DECIDABLE_FAMILIES.contains(&k.family.as_str()))
            {
                Some(k) => reaches(&k),
                None => true,
            }
        })
}

/// The models that have to be read one by one: those the scan doesn't cover, those that may read what
/// `reaches` accepts and those whose sources are already stored (`stored`: reading them costs nothing).
pub(crate) fn models_to_read(
    models: &[String],
    hint: &ScanHint,
    reaches: &dyn Fn(&SourceKey) -> bool,
    stored: &dyn Fn(&str) -> bool,
) -> Vec<String> {
    models
        .iter()
        .filter(|m| match hint.models.get(*m) {
            Some(sources) => model_may_read(sources, reaches) || stored(m),
            None => true,
        })
        .cloned()
        .collect()
}

/// Whether `key` is the server (and database) of `conn`, the object's database taken as `database`.
fn key_is_connection(key: &SourceKey, conn: &SavedConnection, database: &str) -> bool {
    let mut at = conn.clone();
    if !database.is_empty() {
        at.config.database = database.to_string();
    }
    !matches!(
        match_connections(key, std::slice::from_ref(&at)),
        Match::None
    )
}

/// The note a search leaves when the admin scanner API can't be used by the user.
pub(crate) const SCANNER_UNAVAILABLE_NOTE: &str =
    "la API de administración de Power BI no está disponible para este usuario: se leen los modelos de a uno";

/// What asking for a tenant's scan came to.
pub(crate) enum Scan {
    Known(Arc<ScanHint>),
    /// The user has no right for it, or the driver can't: the models are read one by one, with one note.
    Unavailable,
    /// Any other failure: the models are read one by one, silently (the scanner is only a head start).
    Failed,
}

/// The scan of a Power BI connection's tenant: memory (15 minutes), then the disk (6 hours), then the
/// server. A refusal is remembered in memory only, so the admin API isn't asked again for every search.
pub(crate) async fn scan_of(state: &AppState, pbi: &SavedConnection, use_stored: bool) -> Scan {
    if use_stored {
        if let Some(h) = with_index(|i| i.scans.get(&pbi.id, Instant::now())) {
            return h.map_or(Scan::Unavailable, Scan::Known);
        }
        let entry = state.cache_get::<ScanHint>(&pbi.id, "", cache_kinds::LINEAGE_SCAN, "tenant");
        if let Some(h) = lineage_cache::fresh_value(entry, chrono::Utc::now(), lineage_cache::TTL) {
            let h = Arc::new(h);
            with_index(|i| i.scans.put(pbi.id.clone(), Some(h.clone()), Instant::now()));
            return Scan::Known(h);
        }
    }
    let Some(driver) = dbine_drivers::find(&pbi.config.driver) else {
        return Scan::Failed;
    };
    let scan = dbine_driver::DependencyScan::new(
        driver.info(),
        driver.script_dialect(),
        driver.capabilities().foreign_keys,
    );
    let scope = LineageScope {
        silent: true,
        tenant_sources: true,
        ..Default::default()
    };
    let read = state
        .meta_read(&pbi.id, "", SCAN_LIMIT, move |s| {
            Box::pin(async move { s.lineage_graph("", &scope, &scan).await })
        })
        .await;
    match read {
        Ok(graph) => {
            let h = Arc::new(ScanHint::of(&graph));
            with_index(|i| i.scans.put(pbi.id.clone(), Some(h.clone()), Instant::now()));
            state.cache_put(&pbi.id, "", cache_kinds::LINEAGE_SCAN, "tenant", &*h);
            Scan::Known(h)
        }
        Err(CommandError::BadRequest(_)) => {
            with_index(|i| i.scans.put(pbi.id.clone(), None, Instant::now()));
            Scan::Unavailable
        }
        Err(e) => {
            tracing::debug!("lineage scan of {}: {e}", pbi.id);
            Scan::Failed
        }
    }
}

/// DBine Enterprise's access ended: what the index kept about the company's
/// connections goes (the managed connections a source resolved to, and the
/// models read through a managed connection).
pub fn forget_company() {
    with_index(|i| {
        i.sources.drop_where(|(c, _)| c.starts_with("ent-"));
        i.models.drop_where(|c| c.starts_with("ent-"));
        i.resolved.drop_where(|_| true);
        i.scans.drop_where(|c| c.starts_with("ent-"));
    });
}

/// One company connection lost (or some of its databases): what the index
/// kept about it goes, and so does what any source resolved to (it can name it).
pub fn forget_connection(id: &str) {
    with_index(|i| {
        i.sources.drop_where(|(c, _)| c == id);
        i.models.drop_where(|c| c == id);
        i.resolved.drop_where(|_| true);
        i.scans.drop_where(|c| c == id);
    });
}

// -- pure helpers -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub connection_id: String,
    pub name: String,
    pub driver: String,
    /// `link` (the user's), `auto` (matched by server), `enterprise` or `internal` (the lineage's own).
    pub via: String,
}

#[derive(Debug, Serialize)]
pub struct SourceMatch {
    /// `one`, `many` or `none`.
    pub status: String,
    pub signature: String,
    pub candidates: Vec<Candidate>,
    /// A new connection's config (no credentials), only when `none`.
    pub prefill: Option<ConnectionConfig>,
    /// Its engine asks for a user and a password: the panel shows the login form.
    pub needs_login: bool,
}

/// `one`, `many` or `none` by how many candidates there are.
fn status_of(candidates: &[Candidate]) -> &'static str {
    match candidates.len() {
        0 => "none",
        1 => "one",
        _ => "many",
    }
}

/// The candidates once each, in the order given (the user's link first,
/// then the automatic ones, then Enterprise's): a connection that came by
/// several ways keeps the first.
fn merge_candidates(lists: Vec<Vec<Candidate>>) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    for c in lists.into_iter().flatten() {
        if !out.iter().any(|o| o.connection_id == c.connection_id) {
            out.push(c);
        }
    }
    out
}

/// The candidates to follow a source, by the order of the rules: the user's
/// link, else the saved matches, then DBine Enterprise's; when nothing
/// matched, the connection the lineage made for it (the open-source path).
/// DBine Enterprise failing (not signed in, unreachable, an older server)
/// is not an error: it adds no candidate.
fn decide_candidates(
    link: Option<Vec<Candidate>>,
    auto: Vec<Candidate>,
    enterprise: CommandResult<Vec<Candidate>>,
    internal: Option<Candidate>,
) -> Vec<Candidate> {
    let mut lists = Vec::new();
    let keep_link = link.as_ref().is_some_and(|l| !l.is_empty());
    if let Some(l) = link {
        lists.push(l);
    }
    // The user's choice stands: no other candidate competes with it.
    if !keep_link {
        lists.push(auto);
        lists.push(enterprise.unwrap_or_default());
    }
    let mut out = merge_candidates(lists);
    if out.is_empty() {
        out.extend(internal);
    }
    out
}

fn candidate(c: &SavedConnection, via: &str) -> Candidate {
    Candidate {
        connection_id: c.id.clone(),
        name: c.name.clone(),
        driver: c.config.driver.clone(),
        via: via.into(),
    }
}

fn candidates_of(ids: &[String], conns: &[SavedConnection], via: &str) -> Vec<Candidate> {
    ids.iter()
        .filter_map(|id| conns.iter().find(|c| &c.id == id))
        .map(|c| candidate(c, via))
        .collect()
}

fn bare(s: &str) -> String {
    s.trim()
        .trim_matches(|c| matches!(c, '[' | ']' | '"' | '`'))
        .to_lowercase()
}

/// Whether `ext` names `object`: the same name, and the same schema when
/// both name one.
fn names_object(ext: &ExternalRef, object: &ObjectRef) -> bool {
    let Some(name) = ext.name.as_deref() else {
        return false;
    };
    if bare(name) != bare(&object.name) {
        return false;
    }
    match (
        ext.schema.as_deref().filter(|s| !s.trim().is_empty()),
        object.schema(),
    ) {
        (Some(a), Some(b)) => bare(a) == bare(b),
        _ => true,
    }
}

/// Whether `ext` is `object` of `database` in a managed (DBine Enterprise)
/// connection `managed_id`: the server resolved the source (`resolved`: by
/// signature) to that connection, the database is not another one, and the
/// name and schema are the object's.
fn external_reads_managed(
    ext: &ExternalRef,
    managed_id: &str,
    resolved: &HashMap<String, Vec<String>>,
    database: &str,
    object: &ObjectRef,
) -> bool {
    let Some(key) = source_key_of_external(ext) else {
        return false;
    };
    if !resolved
        .get(&key.signature())
        .is_some_and(|ids| ids.iter().any(|i| i == managed_id))
    {
        return false;
    }
    if !database.is_empty() && !key.database.is_empty() && bare(database) != key.database {
        return false;
    }
    names_object(ext, object)
}

/// Whether `ext` (a source some lineage names) is `object` of `database` in
/// `conn`: the same server and database as the connection's config, the
/// same name, and the same schema when both name one.
fn external_reads_object(
    ext: &ExternalRef,
    conn: &SavedConnection,
    database: &str,
    object: &ObjectRef,
) -> bool {
    let Some(key) = source_key_of_external(ext) else {
        return false;
    };
    if !names_object(ext, object) {
        return false;
    }
    // The connection with the database the object is in.
    let mut at = conn.clone();
    if !database.is_empty() {
        at.config.database = database.to_string();
    }
    matches!(
        match_connections(&key, std::slice::from_ref(&at)),
        Match::One(_)
    )
}

/// The object a lookup by name found: a table or view before anything else,
/// the schema the source names when it names one.
fn pick_object(objects: &[DbObject], schema: Option<&str>) -> Option<ObjectRef> {
    let schema = schema.map(bare).filter(|s| !s.is_empty());
    let rank = |k: &str| match k {
        "table" => 0,
        "view" => 1,
        "materialized_view" => 2,
        _ => 3,
    };
    objects
        .iter()
        .filter(|o| {
            schema
                .as_deref()
                .is_none_or(|s| o.schema.as_deref().map(bare).as_deref() == Some(s))
        })
        .min_by_key(|o| rank(&o.kind))
        .map(|o| ObjectRef {
            kind: o.kind.clone(),
            schema: o.schema.clone(),
            name: o.name.clone(),
        })
}

fn find_connection(state: &AppState, id: &str) -> CommandResult<SavedConnection> {
    if let Some(c) = state.store.get_connection(id)? {
        return Ok(c);
    }
    state
        .store
        .list_managed_connections()?
        .into_iter()
        .find(|c| c.id == id)
        .ok_or_else(|| CommandError::NotFound(format!("no existe la conexión '{id}'")))
}

// -- commands -----------------------------------------------------------------

#[derive(Deserialize)]
pub struct ResolveSourceArgs {
    pub connection_id: String,
    pub source: ExternalRef,
}

/// Which saved connection a source of a lineage is: the user's link, else
/// the ones whose server and database match, plus the managed ones DBine
/// Enterprise says this person may use. With none, the config to create one.
#[tauri::command(rename_all = "camelCase")]
pub async fn lineage_resolve_source(
    state: State<'_, AppState>,
    args: ResolveSourceArgs,
) -> CommandResult<SourceMatch> {
    resolve_source_of(&state, args).await
}

async fn resolve_source_of(
    state: &AppState,
    args: ResolveSourceArgs,
) -> CommandResult<SourceMatch> {
    find_connection(state, &args.connection_id)?;
    let Some(key) = source_key_of_external(&args.source) else {
        return Ok(SourceMatch {
            status: "none".into(),
            signature: String::new(),
            candidates: Vec::new(),
            prefill: None,
            needs_login: false,
        });
    };
    let signature = key.signature();
    let mut conns = state.store.list_connections()?;
    let managed = state.store.list_managed_connections()?;
    let linked = state.store.lineage_link_get(&signature)?;
    let link = linked
        .as_ref()
        .map(|id| candidates_of(std::slice::from_ref(id), &conns, "link"));
    let keep_link = link.as_ref().is_some_and(|l| !l.is_empty());
    let (mut auto, mut enterprise) = (Vec::new(), Ok(Vec::new()));
    if !keep_link {
        let ids = match match_connections(&key, &conns) {
            Match::None => Vec::new(),
            Match::One(id) => vec![id],
            Match::Many(ids) => ids,
        };
        auto = candidates_of(&ids, &conns, "auto");
        if let Some(req) = crate::enterprise::SourceRequest::from_external(&args.source) {
            enterprise = crate::enterprise::resolve_source(&req).await.map(|ids| {
                conns.extend(managed);
                candidates_of(&ids, &conns, "enterprise")
            });
        }
    }
    // No saved connection knows it: the one the lineage made for it before (hidden, its login kept
    // or typed this run) is the answer, so following needs nothing from the user.
    let internal = if keep_link {
        None
    } else {
        let internals = state.store.list_lineage_connections()?;
        internal_for(&internals, &signature).map(|c| candidate(c, "internal"))
    };
    let candidates = decide_candidates(link, auto, enterprise, internal);
    let status = status_of(&candidates).to_string();
    let prefill = (status == "none")
        .then(|| new_connection_from_source(&args.source))
        .flatten();
    let needs_login = prefill.as_ref().is_some_and(|p| needs_login(&p.driver));
    Ok(SourceMatch {
        status,
        signature,
        candidates,
        prefill,
        needs_login,
    })
}

/// The connection that opens a source, ready to follow the lineage there.
#[derive(Serialize)]
pub struct OpenedSource {
    pub connection_id: String,
    pub name: String,
    pub driver: String,
    /// The engine needs a user and none is known: ask for it and call again with `username`.
    pub needs_user: bool,
    /// Its password is kept in the keychain ("Recordar en este equipo").
    pub remembered: bool,
}

/// Engines whose connection is a user and a password.
fn needs_login(driver: &str) -> bool {
    matches!(
        driver,
        "sqlserver" | "azuresql" | "postgres" | "mysql" | "oracle"
    )
}

/// The lineage's internal connection already made for a source, if any.
fn internal_for<'a>(
    internals: &'a [SavedConnection],
    signature: &str,
) -> Option<&'a SavedConnection> {
    internals
        .iter()
        .find(|c| source_key_of_config(&c.config).is_some_and(|k| k.signature() == signature))
}

/// Whether the typed password goes to the keychain: only when the user asked and there is one.
fn keeps_password(remember: bool, password: Option<&str>) -> bool {
    remember && password.is_some_and(|p| !p.is_empty())
}

/// The name an internal connection shows (in Settings), from the source's server and database.
fn internal_name(config: &ConnectionConfig) -> String {
    if config.database.is_empty() {
        config.host.clone()
    } else {
        format!("{} · {}", config.database, config.host)
    }
}

#[derive(Deserialize)]
pub struct OpenSourceArgs {
    pub connection_id: String,
    pub source: ExternalRef,
    /// The login the source needs when nothing saved knows it.
    #[serde(default)]
    pub username: Option<String>,
    /// Typed with it. Kept for this run only (never in the state file), or in the keychain with `remember`.
    #[serde(default)]
    pub password: Option<String>,
    /// "Recordar en este equipo": keep the password in the keychain, as a saved connection does.
    #[serde(default)]
    pub remember: bool,
}

/// The connection for a source: the saved one it points to, else the lineage's **internal** one: a
/// hidden connection (not in the explorer, backups, sync, MCP or the phone), read-only, built from
/// the source's engine, server and database, and the same one for the same source every time. It
/// lasts across runs only with a remembered password (in the keychain, like a saved connection's);
/// without, the password is this run's and the connection is gone when DBine restarts.
#[tauri::command(rename_all = "camelCase")]
pub async fn lineage_open_source(
    state: State<'_, AppState>,
    args: OpenSourceArgs,
) -> CommandResult<OpenedSource> {
    let found = resolve_source_of(
        &state,
        ResolveSourceArgs {
            connection_id: args.connection_id.clone(),
            source: args.source.clone(),
        },
    )
    .await?;
    match found.status.as_str() {
        "one" => {
            let c = &found.candidates[0];
            let remembered = state
                .store
                .get_connection(&c.connection_id)?
                .is_some_and(|c| c.save_password);
            return Ok(OpenedSource {
                connection_id: c.connection_id.clone(),
                name: c.name.clone(),
                driver: c.driver.clone(),
                needs_user: false,
                remembered,
            });
        }
        "many" => {
            return Err(CommandError::BadRequest(
                "hay varias conexiones que coinciden: elegí una".into(),
            ))
        }
        _ => {}
    }
    let mut config = found.prefill.ok_or_else(|| {
        CommandError::BadRequest("la fuente no es un servidor al que DBine pueda conectarse".into())
    })?;
    config.read_only = true;
    if let Some(u) = args.username.filter(|u| !u.trim().is_empty()) {
        config.username = Some(u.trim().to_string());
    }
    if needs_login(&config.driver) && config.username.as_deref().unwrap_or("").is_empty() {
        return Ok(OpenedSource {
            connection_id: String::new(),
            name: String::new(),
            driver: config.driver,
            needs_user: true,
            remembered: false,
        });
    }
    let id = uuid::Uuid::new_v4().to_string();
    let password = args.password.filter(|p| !p.is_empty());
    // In the keychain when asked; if it refuses, this run's only (the connection then asks again next time).
    let remembered = keeps_password(args.remember, password.as_deref())
        && match secrets::set(
            &id,
            &Secrets::from([("password".to_string(), password.clone().unwrap_or_default())]),
        ) {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(%e, "lineage source: the keychain refused the password");
                false
            }
        };
    if let Some(p) = password {
        state
            .typed_secrets
            .entry(id.clone())
            .or_default()
            .insert("password".to_string(), p);
    }
    let name = internal_name(&config);
    let saved = state.store.add_lineage_connection(&SavedConnection {
        id,
        name: name.clone(),
        color: None,
        config: config.clone(),
        save_password: remembered,
        folder_id: None,
        tags: Vec::new(),
        mcp_level: Some("disabled".into()),
        updated_at: String::new(),
    })?;
    Ok(OpenedSource {
        connection_id: saved.id,
        name,
        driver: config.driver,
        needs_user: false,
        remembered,
    })
}

#[derive(Deserialize)]
pub struct InternalIdArgs {
    pub id: String,
}

/// The lineage's internal connections: what Settings lists to forget or add to the explorer.
#[tauri::command]
pub async fn lineage_internal_list(
    state: State<'_, AppState>,
) -> CommandResult<Vec<SavedConnection>> {
    Ok(state.store.list_lineage_connections()?)
}

/// "Olvidar": the internal connection, its sessions and its keychain secret.
#[tauri::command(rename_all = "camelCase")]
pub async fn lineage_internal_forget(
    state: State<'_, AppState>,
    args: InternalIdArgs,
) -> CommandResult<()> {
    if !state.store.is_lineage_connection(&args.id)? {
        return Err(CommandError::NotFound(format!(
            "no existe la conexión interna '{}'",
            args.id
        )));
    }
    state.close_connection_sessions(&args.id);
    state.typed_secrets.remove(&args.id);
    state.keychain_cache.remove(&args.id);
    secrets::delete(&args.id)?;
    state.cache_forget(&args.id);
    state.store.delete_lineage_connection(&args.id)?;
    Ok(())
}

/// "Agregar al explorador": the internal connection becomes a saved one (same id, so the remembered
/// password goes with it), read-only as it was.
#[tauri::command(rename_all = "camelCase")]
pub async fn lineage_internal_promote(
    state: State<'_, AppState>,
    args: InternalIdArgs,
) -> CommandResult<SavedConnection> {
    state
        .store
        .promote_lineage_connection(&args.id)?
        .ok_or_else(|| {
            CommandError::NotFound(format!("no existe la conexión interna '{}'", args.id))
        })
}

#[derive(Deserialize)]
pub struct LinkSourceArgs {
    pub signature: String,
    /// `None`: remove the link.
    pub connection_id: Option<String>,
}

/// Remember (or forget) which connection a source is.
#[tauri::command(rename_all = "camelCase")]
pub async fn lineage_link_source(
    state: State<'_, AppState>,
    args: LinkSourceArgs,
) -> CommandResult<()> {
    match args.connection_id {
        Some(id) => state.store.lineage_link_set(&args.signature, &id)?,
        None => state.store.lineage_link_remove(&args.signature)?,
    }
    Ok(())
}

#[derive(Deserialize)]
pub struct LocateSourceArgs {
    pub connection_id: String,
    pub source: ExternalRef,
}

#[derive(Serialize)]
pub struct Located {
    pub database: String,
    pub object: ObjectRef,
}

/// Where a source's table is in `connection_id`: its database (the
/// source's, else the connection's default) and the object found there.
#[tauri::command(rename_all = "camelCase")]
pub async fn lineage_locate_source(
    state: State<'_, AppState>,
    args: LocateSourceArgs,
) -> CommandResult<Option<Located>> {
    let conn = find_connection(&state, &args.connection_id)?;
    let Some(name) = args
        .source
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
    else {
        return Ok(None);
    };
    let database = args
        .source
        .database
        .as_deref()
        .map(|d| {
            d.trim()
                .trim_matches(|c| matches!(c, '[' | ']' | '"' | '`'))
        })
        .filter(|d| !d.is_empty())
        .map(String::from)
        .unwrap_or_else(|| conn.config.database.clone());
    let page = object_page(
        &state,
        ObjectPageArgs {
            connection_id: args.connection_id.clone(),
            database: database.clone(),
            query: ObjectQuery {
                name: Some(bare_name(name)),
                limit: 200,
                ..Default::default()
            },
            fresh: false,
            tree: false,
        },
    )
    .await?;
    Ok(pick_object(&page.objects, args.source.schema.as_deref())
        .map(|object| Located { database, object }))
}

fn bare_name(s: &str) -> String {
    s.trim()
        .trim_matches(|c| matches!(c, '[' | ']' | '"' | '`'))
        .to_string()
}

#[derive(Deserialize)]
pub struct CrossUsersArgs {
    pub connection_id: String,
    pub database: String,
    pub object: ObjectRef,
    #[serde(default)]
    pub refresh: bool,
}

#[derive(Debug, Serialize)]
pub struct CrossUser {
    pub connection_id: String,
    pub connection_name: String,
    /// The semantic model.
    pub database: String,
    /// The table of it that reads the object.
    pub object: ObjectRef,
    pub detail: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Searched {
    pub connection_id: String,
    pub name: String,
    pub models: usize,
    /// Every model came out of the index kept in memory.
    pub cached: bool,
}

#[derive(Debug, Serialize, Default)]
pub struct CrossUsers {
    pub users: Vec<CrossUser>,
    pub searched: Vec<Searched>,
    pub notes: Vec<String>,
}

/// The users `externals` of one model hold for the object.
fn users_in(
    externals: &[LineageExternal],
    pbi: &SavedConnection,
    model: &str,
    source_conn: &SavedConnection,
    database: &str,
    object: &ObjectRef,
) -> Vec<CrossUser> {
    users_by(externals, pbi, model, |e| {
        external_reads_object(e, source_conn, database, object)
    })
}

/// The users `externals` of one model hold for what `reads` accepts, once per table.
fn users_by(
    externals: &[LineageExternal],
    pbi: &SavedConnection,
    model: &str,
    reads: impl Fn(&ExternalRef) -> bool,
) -> Vec<CrossUser> {
    let mut out: Vec<CrossUser> = Vec::new();
    for e in externals {
        if !reads(&e.source) {
            continue;
        }
        if out
            .iter()
            .any(|u| u.object.kind == e.to.kind && u.object.name == e.to.name)
        {
            continue;
        }
        out.push(CrossUser {
            connection_id: pbi.id.clone(),
            connection_name: pbi.name.clone(),
            database: model.to_string(),
            object: e.to.clone(),
            detail: e.source.detail.clone(),
        });
    }
    out
}

/// A note for a connection or model that couldn't be read.
fn unreadable_note(name: &str, model: Option<&str>, e: &CommandError) -> String {
    match (e, model) {
        (CommandError::PasswordRequired(_), _) => {
            format!("la conexión «{name}» necesita conectarse primero (abrila y volvé a intentar)")
        }
        (_, Some(m)) => format!("no se pudo leer el modelo «{m}» de «{name}»: {e}"),
        (_, None) => format!("no se pudo abrir la conexión «{name}»: {e}"),
    }
}

/// Who reads `object` from another connection: the Power BI tables whose
/// sources are it. Each saved Power BI connection's models are read once
/// and kept for 15 minutes; `refresh` reads them again.
#[tauri::command(rename_all = "camelCase")]
pub async fn get_cross_users(
    state: State<'_, AppState>,
    args: CrossUsersArgs,
) -> CommandResult<CrossUsers> {
    let source_conn = find_connection(&state, &args.connection_id)?;
    let mut res = CrossUsers::default();
    let managed = crate::enterprise::is_managed(&source_conn.config);
    if source_conn.config.driver == "powerbi"
        || (!managed && source_key_of_config(&source_conn.config).is_none())
    {
        // No server of its own to be named by a source (a file) or already a model.
        return Ok(res);
    }
    if managed {
        // It holds no host: DBine Enterprise says which sources are it.
        match crate::enterprise::get() {
            Ok(e) if e.status().signed_in => {}
            _ => {
                res.notes.push(
                    "esta conexión es de DBine Enterprise: iniciá sesión para buscar qué modelos de Power BI leen sus objetos"
                        .into(),
                );
                return Ok(res);
            }
        }
    }
    let pbis: Vec<SavedConnection> = state
        .store
        .list_connections()?
        .into_iter()
        .filter(|c| c.config.driver == "powerbi" && c.id != args.connection_id)
        .collect();
    if args.refresh {
        let ids: Vec<String> = pbis.iter().map(|c| c.id.clone()).collect();
        with_index(|i| {
            i.sources.drop_where(|(c, _)| ids.contains(c));
            i.models.drop_where(|c| ids.contains(c));
            i.resolved.drop_where(|_| true);
        });
    }
    // (connection, model, its sources) of every model read.
    let mut scanned: Vec<(usize, String, Sources)> = Vec::new();
    let (model_limit, scan_budget) = scan_limits(&state);
    let parallel = scan_parallel(&state);
    let deadline = Instant::now() + scan_budget;
    for (pi, pbi) in pbis.iter().enumerate() {
        let models = match models_of(&state, pbi).await {
            Ok(m) => m,
            Err(e) => {
                res.notes.push(unreadable_note(&pbi.name, None, &e));
                continue;
            }
        };
        let mut all_cached = true;
        let mut left_out = 0usize;
        let mut out_of_time = 0usize;
        let st = &*state;
        // The admin scanner API says, without opening a model, which servers each one reads from: the
        // models that can't read this connection's are not opened.
        let to_read = match scan_of(st, pbi, !args.refresh).await {
            Scan::Known(hint) => {
                prefilter(
                    st,
                    pbi,
                    &models,
                    &hint,
                    &source_conn,
                    &args.database,
                    managed,
                    args.refresh,
                )
                .await
            }
            Scan::Unavailable => {
                let note = SCANNER_UNAVAILABLE_NOTE.to_string();
                if !res.notes.contains(&note) {
                    res.notes.push(note);
                }
                models.to_vec()
            }
            Scan::Failed => models.to_vec(),
        };
        let mut work: Vec<(String, Option<CommandResult<ModelSources>>)> =
            futures_util::stream::iter(to_read)
                .map(|model| async move {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return (model, None);
                    }
                    let r = tokio::time::timeout(
                        left.min(model_limit),
                        externals_of(st, pbi, &model, !args.refresh),
                    )
                    .await;
                    (model, r.ok())
                })
                .buffer_unordered(parallel)
                .collect()
                .await;
        work.sort_by(|a, b| a.0.cmp(&b.0));
        for (model, r) in work {
            match r {
                None => out_of_time += 1,
                Some(Ok((externals, cached, silent_skip))) => {
                    all_cached &= cached;
                    left_out += usize::from(silent_skip);
                    scanned.push((pi, model, externals));
                }
                Some(Err(e)) => res.notes.push(unreadable_note(&pbi.name, Some(&model), &e)),
            }
        }
        if out_of_time > 0 {
            res.notes.push(format!(
                "La búsqueda llegó al límite de tiempo: {out_of_time} modelos de «{}» no se leyeron. Los que ya se leyeron quedan guardados: buscá de nuevo para seguir",
                pbi.name
            ));
        }
        if left_out > 0 {
            res.notes.push(format!(
                "{left_out} modelos de «{}» se leyeron sin la API de Fabric, que pide un permiso que todavía no está aprobado: abrí uno desde el explorador para aprobarlo y volvé a buscar",
                pbi.name
            ));
        }
        res.searched.push(Searched {
            connection_id: pbi.id.clone(),
            name: pbi.name.clone(),
            models: models.len(),
            cached: all_cached,
        });
    }
    if managed {
        let all: Vec<&LineageExternal> = scanned.iter().flat_map(|(_, _, e)| e.iter()).collect();
        match resolve_externals(all.iter().map(|e| &e.source), args.refresh).await {
            Ok(resolved) => {
                for (pi, model, externals) in &scanned {
                    res.users
                        .extend(users_by(externals, &pbis[*pi], model, |e| {
                            external_reads_managed(
                                e,
                                &source_conn.id,
                                &resolved,
                                &args.database,
                                &args.object,
                            )
                        }));
                }
            }
            Err(e) => res.notes.push(format!(
                "no se pudo preguntar a DBine Enterprise qué conexiones usan las fuentes de Power BI: {e}"
            )),
        }
    } else {
        for (pi, model, externals) in &scanned {
            res.users.extend(users_in(
                externals,
                &pbis[*pi],
                model,
                &source_conn,
                &args.database,
                &args.object,
            ));
        }
    }
    Ok(res)
}

/// The models of `pbi` that have to be opened to know whether they read the object: the scan's pre-filter.
/// For a managed (DBine Enterprise) connection the sources the scan names are resolved by the server first;
/// when that fails every model stays.
#[allow(clippy::too_many_arguments)]
async fn prefilter(
    state: &AppState,
    pbi: &SavedConnection,
    models: &[String],
    hint: &ScanHint,
    source_conn: &SavedConnection,
    database: &str,
    managed: bool,
    refresh: bool,
) -> Vec<String> {
    let stored = |m: &str| {
        !refresh
            && (with_index(|i| {
                i.sources
                    .get(&(pbi.id.clone(), m.to_string()), Instant::now())
            })
            .is_some()
                || sources_on_disk(state, &pbi.id, m).is_some())
    };
    if managed {
        let Ok(resolved) = resolve_externals(hint.sources(), refresh).await else {
            return models.to_vec();
        };
        let reaches = |k: &SourceKey| {
            resolved
                .get(&k.signature())
                .is_some_and(|ids| ids.contains(&source_conn.id))
                && (database.is_empty() || k.database.is_empty() || bare(database) == k.database)
        };
        models_to_read(models, hint, &reaches, &stored)
    } else {
        let reaches = |k: &SourceKey| key_is_connection(k, source_conn, database);
        models_to_read(models, hint, &reaches, &stored)
    }
}

/// What DBine Enterprise resolves each source of `sources` to (managed
/// connection ids, by the source's signature). The sources are unique by
/// signature, asked in one batch and kept for the index's time; `refresh`
/// asks them all again.
async fn resolve_externals(
    sources: impl Iterator<Item = &ExternalRef>,
    refresh: bool,
) -> CommandResult<HashMap<String, Vec<String>>> {
    let (known, missing) = with_index(|i| {
        if refresh {
            i.resolved.drop_where(|_| true);
        }
        split_cached(sources, |sig| {
            i.resolved.get(&sig.to_string(), Instant::now())
        })
    });
    let mut out = known;
    if !missing.is_empty() {
        let reqs: Vec<crate::enterprise::SourceRequest> =
            missing.iter().map(|(_, r)| r.clone()).collect();
        let answers = crate::enterprise::resolve_sources(&reqs).await?;
        with_index(|i| {
            for ((sig, _), ids) in missing.iter().zip(&answers) {
                i.resolved.put(sig.clone(), ids.clone(), Instant::now());
            }
        });
        for ((sig, _), ids) in missing.into_iter().zip(answers) {
            out.insert(sig, ids);
        }
    }
    Ok(out)
}

/// The sources' answers already known, and the requests for the rest, one per signature.
type Asked = Vec<(String, crate::enterprise::SourceRequest)>;

fn split_cached<'a>(
    sources: impl Iterator<Item = &'a ExternalRef>,
    cached: impl Fn(&str) -> Option<Vec<String>>,
) -> (HashMap<String, Vec<String>>, Asked) {
    let mut known = HashMap::new();
    let mut missing: Asked = Vec::new();
    for e in sources {
        let Some(key) = source_key_of_external(e) else {
            continue;
        };
        let Some(req) = crate::enterprise::SourceRequest::from_external(e) else {
            continue;
        };
        let sig = key.signature();
        if known.contains_key(&sig) || missing.iter().any(|(s, _)| *s == sig) {
            continue;
        }
        match cached(&sig) {
            Some(ids) => {
                known.insert(sig, ids);
            }
            None => missing.push((sig, req)),
        }
    }
    (known, missing)
}

pub(crate) async fn models_of(
    state: &AppState,
    pbi: &SavedConnection,
) -> CommandResult<Arc<Vec<String>>> {
    if let Some(m) = with_index(|i| i.models.get(&pbi.id, Instant::now())) {
        return Ok(m);
    }
    let list = state
        .meta_read(&pbi.id, "", SCAN_LIMIT, |s| {
            Box::pin(async move { s.list_databases().await })
        })
        .await?;
    let list = Arc::new(list);
    with_index(|i| i.models.put(pbi.id.clone(), list.clone(), Instant::now()));
    Ok(list)
}

/// A model's external sources, whether they came from the index, and whether the Fabric API was left out.
type ModelSources = (Sources, bool, bool);

/// The start of the note a Power BI read leaves when the Fabric definition API was left out of a
/// background read (`LineageScope::silent`): its permission isn't granted yet.
const FABRIC_SILENT_PREFIX: &str = "Este modelo se lee mejor con la API de Fabric";

/// A model's external sources, whether they came from the index, and whether the read left the
/// Fabric API out for want of its permission (then nothing is kept: approving it must show more).
///
/// Three layers, nearest first: the in-memory index (15 minutes), the disk cache (6 hours) and the
/// server. Every server read writes both; `use_stored` false (a refresh) skips the two reads.
pub(crate) async fn externals_of(
    state: &AppState,
    pbi: &SavedConnection,
    model: &str,
    use_stored: bool,
) -> CommandResult<ModelSources> {
    let key = (pbi.id.clone(), model.to_string());
    if use_stored {
        if let Some(s) = with_index(|i| i.sources.get(&key, Instant::now())) {
            return Ok((s, true, false));
        }
        if let Some(s) = sources_on_disk(state, &pbi.id, model) {
            with_index(|i| i.sources.put(key, s.clone(), Instant::now()));
            return Ok((s, true, false));
        }
    }
    let driver = dbine_drivers::find(&pbi.config.driver).ok_or_else(|| {
        CommandError::BadRequest(format!(
            "esta versión no incluye el driver '{}'",
            pbi.config.driver
        ))
    })?;
    let scan = dbine_driver::DependencyScan::new(
        driver.info(),
        driver.script_dialect(),
        driver.capabilities().foreign_keys,
    );
    let db = model.to_string();
    let (model_limit, _) = scan_limits(state);
    let graph = state
        .meta_read_on(Lane::Long, &pbi.id, model, model_limit, move |s| {
            Box::pin(async move {
                // A search across models the user didn't open never asks for a sign-in or a consent.
                let scope = LineageScope {
                    silent: true,
                    sources_only: true,
                    ..Default::default()
                };
                s.lineage_graph(&db, &scope, &scan).await
            })
        })
        .await?;
    let skipped = graph
        .notes
        .iter()
        .any(|n| n.starts_with(FABRIC_SILENT_PREFIX));
    let s = Arc::new(graph.external);
    if !skipped {
        with_index(|i| i.sources.put(key, s.clone(), Instant::now()));
        state.cache_put(&pbi.id, "", cache_kinds::LINEAGE_SOURCES, model, &*s);
    }
    Ok((s, false, skipped))
}

/// A model's sources kept on disk, if younger than the lineage cache's TTL.
fn sources_on_disk(state: &AppState, connection_id: &str, model: &str) -> Option<Sources> {
    let entry = state.cache_get::<Vec<LineageExternal>>(
        connection_id,
        "",
        cache_kinds::LINEAGE_SOURCES,
        model,
    );
    lineage_cache::fresh_value(entry, chrono::Utc::now(), lineage_cache::TTL).map(Arc::new)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_parallelism_setting_is_clamped_between_one_and_sixteen() {
        use serde_json::json;
        assert_eq!(parallel_setting(None), 4);
        assert_eq!(parallel_setting(Some(&json!(8))), 8);
        assert_eq!(parallel_setting(Some(&json!(0))), 1);
        assert_eq!(parallel_setting(Some(&json!(-3))), 1);
        assert_eq!(parallel_setting(Some(&json!(500))), 16);
        assert_eq!(parallel_setting(Some(&json!(6.0))), 6);
        assert_eq!(parallel_setting(Some(&json!("x"))), 4);
    }

    #[test]
    fn scan_limits_follow_the_settings_with_floor_and_ceiling() {
        use serde_json::json;
        assert_eq!(secs_setting(None, 180), Duration::from_secs(180));
        assert_eq!(
            secs_setting(Some(&json!(300)), 180),
            Duration::from_secs(300)
        );
        assert_eq!(secs_setting(Some(&json!(2)), 180), Duration::from_secs(180));
        assert_eq!(
            secs_setting(Some(&json!(999999)), 180),
            Duration::from_secs(3600)
        );
        assert_eq!(
            secs_setting(Some(&json!("x")), 180),
            Duration::from_secs(180)
        );
    }

    fn state_with_cache() -> AppState {
        let state = AppState::new(dbine_core::state::StateStore::open_in_memory().unwrap());
        let _ = state
            .cache
            .set(dbine_core::ExplorerCache::open_in_memory().unwrap());
        state
    }

    fn source(name: &str) -> LineageExternal {
        LineageExternal {
            source: ext("srv", "db", Some("dbo"), name),
            to: obj(Some("dbo"), "t"),
        }
    }

    // The connection's driver doesn't exist: a read that reaches for the server fails at once, so
    // a success proves the answer came from a layer in front of it.
    fn offline_pbi(id: &str) -> SavedConnection {
        let mut c = conn(id, "no-such-driver", "", "");
        c.config.driver = "no-such-driver".into();
        c
    }

    #[tokio::test]
    async fn sources_come_from_memory_then_disk_then_the_server() {
        let state = state_with_cache();
        let pbi = offline_pbi("pbi-layers");
        // Nothing anywhere: the server is asked (and fails here).
        assert!(externals_of(&state, &pbi, "m", true).await.is_err());
        // On disk only: served, flagged as cached, and now in memory too.
        state.cache_put(
            &pbi.id,
            "",
            cache_kinds::LINEAGE_SOURCES,
            "m",
            &vec![source("disk")],
        );
        let (got, cached, skipped) = externals_of(&state, &pbi, "m", true).await.unwrap();
        assert!(cached && !skipped);
        assert_eq!(got.len(), 1);
        // Memory is first: the disk entry can vanish and the answer stays.
        state.cache_forget(&pbi.id);
        assert!(externals_of(&state, &pbi, "m", true).await.is_ok());
        // A refresh ignores both layers.
        assert!(externals_of(&state, &pbi, "m", false).await.is_err());
        with_index(|i| i.sources.drop_where(|(c, _)| c == "pbi-layers"));
    }

    #[tokio::test]
    async fn the_scan_comes_from_memory_then_disk_then_the_server_and_a_refusal_is_remembered() {
        let state = state_with_cache();
        let pbi = offline_pbi("pbi-scan-layers");
        // Nothing anywhere: the server is asked (and fails here), silently.
        assert!(matches!(scan_of(&state, &pbi, true).await, Scan::Failed));
        // On disk: served, and in memory now.
        let h = hint(&[("m", vec![sql_source("srv1", "dwh")])]);
        state.cache_put(&pbi.id, "", cache_kinds::LINEAGE_SCAN, "tenant", &h);
        let Scan::Known(got) = scan_of(&state, &pbi, true).await else {
            panic!("the disk entry is not used");
        };
        assert_eq!(*got, h);
        state.cache_forget(&pbi.id);
        assert!(matches!(scan_of(&state, &pbi, true).await, Scan::Known(_)));
        // A refresh ignores both layers.
        assert!(matches!(scan_of(&state, &pbi, false).await, Scan::Failed));
        // A refusal is remembered: the admin API isn't asked again until the memory expires.
        with_index(|i| i.scans.put(pbi.id.clone(), None, Instant::now()));
        assert!(matches!(
            scan_of(&state, &pbi, true).await,
            Scan::Unavailable
        ));
        with_index(|i| i.scans.drop_where(|c| c == "pbi-scan-layers"));
    }

    #[test]
    fn the_disk_layer_of_the_sources_is_per_connection_and_model() {
        let state = state_with_cache();
        assert!(sources_on_disk(&state, "p1", "m").is_none());
        state.cache_put(
            "p1",
            "",
            cache_kinds::LINEAGE_SOURCES,
            "m",
            &vec![source("a")],
        );
        assert_eq!(sources_on_disk(&state, "p1", "m").unwrap().len(), 1);
        assert!(sources_on_disk(&state, "p1", "other").is_none());
        assert!(sources_on_disk(&state, "p2", "m").is_none());
    }

    use super::*;

    fn conn(id: &str, driver: &str, host: &str, db: &str) -> SavedConnection {
        SavedConnection {
            id: id.into(),
            name: format!("name-{id}"),
            color: None,
            config: ConnectionConfig {
                driver: driver.into(),
                host: host.into(),
                database: db.into(),
                ..Default::default()
            },
            save_password: false,
            folder_id: None,
            tags: vec![],
            mcp_level: None,
            updated_at: String::new(),
        }
    }

    fn ext(server: &str, db: &str, schema: Option<&str>, name: &str) -> ExternalRef {
        ExternalRef {
            system: Some("SQL Server".into()),
            server: Some(server.into()),
            database: Some(db.into()),
            schema: schema.map(Into::into),
            name: Some(name.into()),
            ..Default::default()
        }
    }

    fn obj(schema: Option<&str>, name: &str) -> ObjectRef {
        ObjectRef {
            kind: "table".into(),
            schema: schema.map(Into::into),
            name: name.into(),
        }
    }

    fn sql_source(server: &str, db: &str) -> ExternalRef {
        ExternalRef {
            name: None,
            ..ext(server, db, None, "")
        }
    }

    fn hint(models: &[(&str, Vec<ExternalRef>)]) -> ScanHint {
        ScanHint {
            models: models
                .iter()
                .map(|(m, s)| (m.to_string(), s.clone()))
                .collect(),
        }
    }

    #[test]
    fn a_model_is_left_out_only_when_every_source_is_decided_and_none_is_the_connections() {
        let c = conn("c1", "sqlserver", "srv1", "dwh");
        let reaches = |k: &SourceKey| key_is_connection(k, &c, "");
        // Its server and database.
        assert!(model_may_read(&[sql_source("srv1", "dwh")], &reaches));
        // Another database, another server.
        assert!(!model_may_read(&[sql_source("srv1", "other")], &reaches));
        assert!(!model_may_read(&[sql_source("srv2", "dwh")], &reaches));
        // One of several is enough.
        assert!(model_may_read(
            &[sql_source("srv2", "x"), sql_source("SRV1", "DWH")],
            &reaches
        ));
        // A source that names no database may hold it.
        assert!(model_may_read(&[sql_source("srv1", "")], &reaches));
        // The scanner lists none: it can't say.
        assert!(model_may_read(&[], &reaches));
        // A file, an engine the key doesn't cover, or no server at all are never a reason to leave out.
        let file = ExternalRef {
            system: Some("Excel".into()),
            detail: Some("C:\\x.xlsx".into()),
            ..Default::default()
        };
        let other = ExternalRef {
            system: Some("SapHana".into()),
            server: Some("hana1".into()),
            ..Default::default()
        };
        let noserver = ExternalRef {
            system: Some("SQL Server".into()),
            ..Default::default()
        };
        for undecided in [file, other, noserver] {
            assert!(model_may_read(
                &[sql_source("srv2", "x"), undecided],
                &reaches
            ));
        }
    }

    #[test]
    fn the_object_s_database_decides_when_the_connection_names_none() {
        let c = conn("c1", "sqlserver", "srv1", "");
        let k = source_key_of_external(&sql_source("srv1", "sales")).unwrap();
        assert!(key_is_connection(&k, &c, ""));
        assert!(key_is_connection(&k, &c, "sales"));
        assert!(!key_is_connection(&k, &c, "other"));
    }

    #[test]
    fn only_the_models_that_may_read_the_connection_or_are_not_covered_are_opened() {
        let c = conn("c1", "sqlserver", "srv1", "dwh");
        let reaches = |k: &SourceKey| key_is_connection(k, &c, "");
        let h = hint(&[
            ("yes", vec![sql_source("srv1", "dwh")]),
            ("no", vec![sql_source("srv9", "dwh")]),
            ("stored", vec![sql_source("srv9", "dwh")]),
            ("empty", vec![]),
        ]);
        let models: Vec<String> = ["yes", "no", "stored", "empty", "unknown"]
            .map(String::from)
            .to_vec();
        let got = models_to_read(&models, &h, &reaches, &|m| m == "stored");
        assert_eq!(got, ["yes", "stored", "empty", "unknown"]);
        // Nothing is stored: the model that reads elsewhere is the only one left out.
        let got = models_to_read(&models, &h, &reaches, &|_| false);
        assert_eq!(got, ["yes", "empty", "unknown"]);
    }

    #[test]
    fn the_scan_hint_keeps_the_covered_models_even_without_sources() {
        use dbine_driver::dependencies::LineageGraph;
        let node = |n: &str| obj(None, n);
        let g = LineageGraph {
            objects: vec![node("a"), node("b")],
            external: vec![LineageExternal {
                source: sql_source("srv1", "dwh"),
                to: node("a"),
            }],
            ..Default::default()
        };
        let h = ScanHint::of(&g);
        assert_eq!(h.models["a"], [sql_source("srv1", "dwh")]);
        assert!(h.models["b"].is_empty());
        assert!(!h.models.contains_key("c"));
        // It survives the disk cache.
        let again: ScanHint = serde_json::from_str(&serde_json::to_string(&h).unwrap()).unwrap();
        assert_eq!(again, h);
    }

    fn cand(id: &str, via: &str) -> Candidate {
        Candidate {
            connection_id: id.into(),
            name: id.into(),
            driver: "sqlserver".into(),
            via: via.into(),
        }
    }

    #[test]
    fn the_status_follows_the_number_of_candidates() {
        assert_eq!(status_of(&[]), "none");
        assert_eq!(status_of(&[cand("a", "auto")]), "one");
        assert_eq!(status_of(&[cand("a", "auto"), cand("b", "auto")]), "many");
    }

    #[test]
    fn merging_keeps_each_connection_once_by_its_first_way() {
        let m = merge_candidates(vec![
            vec![cand("a", "link")],
            vec![cand("a", "auto"), cand("b", "auto")],
            vec![cand("b", "enterprise"), cand("ent-p", "enterprise")],
        ]);
        let got: Vec<_> = m
            .iter()
            .map(|c| (c.connection_id.as_str(), c.via.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![("a", "link"), ("b", "auto"), ("ent-p", "enterprise")]
        );
        assert!(merge_candidates(vec![]).is_empty());
    }

    #[test]
    fn ids_of_deleted_connections_make_no_candidate() {
        let conns = vec![conn("a", "sqlserver", "h", "d")];
        let c = candidates_of(&["a".into(), "gone".into()], &conns, "auto");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].driver, "sqlserver");
    }

    #[test]
    fn a_source_is_the_object_of_the_connection_that_has_its_server() {
        let c = conn("a", "sqlserver", "dw01", "sales");
        let o = obj(Some("dbo"), "Orders");
        // Spellings of the name, the schema and the server do not matter.
        assert!(external_reads_object(
            &ext("DW01", "Sales", Some("[dbo]"), "orders"),
            &c,
            "sales",
            &o
        ));
        assert!(external_reads_object(
            &ext("dw01,1433", "sales", None, "[Orders]"),
            &c,
            "sales",
            &o
        ));
    }

    #[test]
    fn another_server_database_schema_or_name_is_not_the_object() {
        let c = conn("a", "sqlserver", "dw01", "sales");
        let o = obj(Some("dbo"), "Orders");
        let no = |e: ExternalRef, db: &str| !external_reads_object(&e, &c, db, &o);
        assert!(no(ext("other", "sales", Some("dbo"), "Orders"), "sales"));
        assert!(no(ext("dw01", "hr", Some("dbo"), "Orders"), "sales"));
        assert!(no(ext("dw01", "sales", Some("etl"), "Orders"), "sales"));
        assert!(no(ext("dw01", "sales", Some("dbo"), "Customers"), "sales"));
        // The object lives in another database of that server.
        assert!(no(ext("dw01", "sales", Some("dbo"), "Orders"), "hr"));
        // A file is not a server.
        let mut f = ext("dw01", "sales", None, "Orders");
        f.system = Some("Excel".into());
        assert!(no(f, "sales"));
        // Nothing named.
        let mut n = ext("dw01", "sales", None, "x");
        n.name = None;
        assert!(no(n, "sales"));
    }

    #[test]
    fn a_connection_without_default_database_still_matches_the_named_one() {
        let c = conn("a", "sqlserver", "dw01", "");
        assert!(external_reads_object(
            &ext("dw01", "sales", Some("dbo"), "Orders"),
            &c,
            "sales",
            &obj(Some("dbo"), "Orders")
        ));
    }

    #[test]
    fn the_object_found_prefers_tables_then_views_and_the_schema_asked() {
        let o = |k: &str, s: &str, n: &str| DbObject {
            kind: k.into(),
            schema: Some(s.into()),
            name: n.into(),
            parent: None,
        };
        let found = vec![
            o("view", "dbo", "T"),
            o("table", "etl", "T"),
            o("synonym", "dbo", "T"),
        ];
        assert_eq!(pick_object(&found, None).unwrap().kind, "table");
        let p = pick_object(&found, Some("[DBO]")).unwrap();
        assert_eq!(
            (p.kind.as_str(), p.schema.as_deref()),
            ("view", Some("dbo"))
        );
        assert!(pick_object(&found, Some("nope")).is_none());
        assert!(pick_object(&[], None).is_none());
    }

    #[test]
    fn the_index_entries_expire_after_the_ttl() {
        let mut c: TtlCache<String, u32> = TtlCache::new(Duration::from_secs(900));
        let t0 = Instant::now();
        c.put("k".into(), 7, t0);
        assert_eq!(c.get(&"k".into(), t0 + Duration::from_secs(899)), Some(7));
        assert_eq!(c.get(&"k".into(), t0 + Duration::from_secs(900)), None);
        assert_eq!(c.get(&"other".into(), t0), None);
        c.drop_where(|k| k == "k");
        assert_eq!(c.get(&"k".into(), t0), None);
    }

    #[test]
    fn forgetting_the_company_empties_what_it_resolved_and_keeps_the_rest() {
        let now = Instant::now();
        with_index(|i| {
            i.resolved
                .put("purge-sig".into(), vec!["ent-p1".into()], now);
            i.models
                .put("ent-purge".into(), Arc::new(vec!["m".into()]), now);
            i.models
                .put("c1-purge".into(), Arc::new(vec!["m".into()]), now);
            i.sources
                .put(("ent-purge".into(), "m".into()), Arc::new(Vec::new()), now);
            i.sources
                .put(("c1-purge".into(), "m".into()), Arc::new(Vec::new()), now);
        });
        forget_company();
        with_index(|i| {
            assert!(i.resolved.get(&"purge-sig".to_string(), now).is_none());
            assert!(i.models.get(&"ent-purge".to_string(), now).is_none());
            assert!(i.models.get(&"c1-purge".to_string(), now).is_some());
            assert!(i
                .sources
                .get(&("ent-purge".to_string(), "m".to_string()), now)
                .is_none());
            assert!(i
                .sources
                .get(&("c1-purge".to_string(), "m".to_string()), now)
                .is_some());
        });
    }

    #[test]
    fn the_tables_that_read_the_object_are_its_users_once_each() {
        let sql = conn("s", "sqlserver", "dw01", "sales");
        let pbi = conn("p", "powerbi", "", "");
        let table = |n: &str| obj(None, n);
        let ex = |name: &str, to: &str| LineageExternal {
            source: ExternalRef {
                detail: Some("partition".into()),
                ..ext("dw01", "sales", Some("dbo"), name)
            },
            to: table(to),
        };
        let all = vec![
            ex("Orders", "FactOrders"),
            ex("Orders", "FactOrders"),
            ex("Customers", "DimC"),
        ];
        let u = users_in(
            &all,
            &pbi,
            "Model1",
            &sql,
            "sales",
            &obj(Some("dbo"), "Orders"),
        );
        assert_eq!(u.len(), 1);
        assert_eq!(
            (
                u[0].connection_id.as_str(),
                u[0].database.as_str(),
                u[0].object.name.as_str()
            ),
            ("p", "Model1", "FactOrders")
        );
        assert_eq!(u[0].detail.as_deref(), Some("partition"));
    }

    fn managed_fixture() -> HashMap<String, Vec<String>> {
        let sig = |s: &str, d: &str| {
            source_key_of_external(&ext(s, d, None, "x"))
                .unwrap()
                .signature()
        };
        HashMap::from([
            (sig("dw01", "sales"), vec!["ent-p1".to_string()]),
            (sig("dw02", "sales"), vec!["ent-p2".to_string()]),
        ])
    }

    #[test]
    fn a_source_the_server_resolved_to_the_managed_connection_reads_its_object() {
        let r = managed_fixture();
        let o = obj(Some("dbo"), "Orders");
        let read = |e: &ExternalRef, id: &str, db: &str| external_reads_managed(e, id, &r, db, &o);
        let e = ext("dw01", "sales", Some("dbo"), "Orders");
        assert!(read(&e, "ent-p1", ""));
        assert!(read(&e, "ent-p1", "SALES"));
        // another profile, another database of the connection, another schema or name
        assert!(!read(&e, "ent-p2", ""));
        assert!(!read(&e, "ent-p1", "hr"));
        assert!(!read(
            &ext("dw01", "sales", Some("hr"), "Orders"),
            "ent-p1",
            ""
        ));
        assert!(!read(
            &ext("dw01", "sales", Some("dbo"), "Customers"),
            "ent-p1",
            ""
        ));
        // a source the server did not resolve
        assert!(!read(
            &ext("dw09", "sales", Some("dbo"), "Orders"),
            "ent-p1",
            ""
        ));
        // a schema on one side only still matches by name
        assert!(read(&ext("dw01", "sales", None, "orders"), "ent-p1", ""));
    }

    #[test]
    fn the_managed_users_are_the_tables_reading_it_once_each() {
        let pbi = conn("p", "powerbi", "", "");
        let r = managed_fixture();
        let o = obj(Some("dbo"), "Orders");
        let ex = |server: &str, to: &str| LineageExternal {
            source: ext(server, "sales", Some("dbo"), "Orders"),
            to: obj(None, to),
        };
        let all = vec![
            ex("dw01", "FactOrders"),
            ex("dw01", "FactOrders"),
            ex("dw02", "Other"),
        ];
        let u = users_by(&all, &pbi, "M", |e| {
            external_reads_managed(e, "ent-p1", &r, "", &o)
        });
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].object.name, "FactOrders");
    }

    #[test]
    fn the_sources_asked_to_the_server_are_unique_and_skip_the_cached_ones() {
        let a = ext("dw01", "sales", None, "A");
        let b = ext("dw01", "sales", None, "B");
        let c = ext("dw02", "sales", None, "C");
        let files = ExternalRef {
            system: Some("SharePoint".into()),
            server: Some("x".into()),
            ..Default::default()
        };
        let list = [a.clone(), b, c.clone(), files];
        let (known, missing) = split_cached(list.iter(), |_| None);
        assert!(known.is_empty());
        assert_eq!(missing.len(), 2);
        assert_eq!(missing[0].1.server, "dw01");
        let sig_a = source_key_of_external(&a).unwrap().signature();
        let (known, missing) = split_cached(list.iter(), |s| {
            (s == sig_a).then(|| vec!["ent-p1".to_string()])
        });
        assert_eq!(known[&sig_a], vec!["ent-p1".to_string()]);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].1.server, "dw02");
    }

    #[test]
    fn the_candidates_follow_link_then_saved_then_enterprise_then_internal() {
        let ok = |l: &[(&str, &str)]| Ok(l.iter().map(|(i, v)| cand(i, v)).collect::<Vec<_>>());
        // a link stands alone
        let d = decide_candidates(
            Some(vec![cand("l", "link")]),
            vec![cand("a", "auto")],
            ok(&[("e", "enterprise")]),
            Some(cand("i", "internal")),
        );
        assert_eq!(
            d.iter()
                .map(|c| c.connection_id.as_str())
                .collect::<Vec<_>>(),
            ["l"]
        );
        // a link to a deleted connection lets the rest compete
        let d = decide_candidates(
            Some(vec![]),
            vec![cand("a", "auto")],
            ok(&[("a", "enterprise"), ("e", "enterprise")]),
            Some(cand("i", "internal")),
        );
        assert_eq!(
            d.iter()
                .map(|c| c.connection_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "e"]
        );
        assert_eq!(d[0].via, "auto");
        // only Enterprise knows it
        let d = decide_candidates(
            None,
            vec![],
            ok(&[("e", "enterprise")]),
            Some(cand("i", "internal")),
        );
        assert_eq!(
            d.iter()
                .map(|c| c.connection_id.as_str())
                .collect::<Vec<_>>(),
            ["e"]
        );
        // nobody knows it: the open-source path's connection
        let d = decide_candidates(None, vec![], ok(&[]), Some(cand("i", "internal")));
        assert_eq!(
            d.iter().map(|c| c.via.as_str()).collect::<Vec<_>>(),
            ["internal"]
        );
        assert!(decide_candidates(None, vec![], ok(&[]), None).is_empty());
    }

    #[test]
    fn enterprise_failing_is_ignored_and_the_open_source_path_goes_on() {
        for e in [
            CommandError::AuthFailed("iniciá sesión en DBine Enterprise".into()),
            CommandError::Connect("DBine Enterprise no responde".into()),
            CommandError::NotFound("old server".into()),
        ] {
            let d = decide_candidates(None, vec![cand("a", "auto")], Err(e), None);
            assert_eq!(d.len(), 1);
        }
        let d = decide_candidates(
            None,
            vec![],
            Err(CommandError::Connect("x".into())),
            Some(cand("i", "internal")),
        );
        assert_eq!(d[0].via, "internal");
        assert!(
            decide_candidates(None, vec![], Err(CommandError::Connect("x".into())), None)
                .is_empty()
        );
    }

    #[test]
    fn a_connection_that_wants_a_password_is_told_to_connect() {
        let n = unreadable_note("PBI", None, &CommandError::PasswordRequired(None));
        assert!(n.contains("conectarse primero") && n.contains("PBI"));
        let n = unreadable_note("PBI", Some("M"), &CommandError::Internal("x".into()));
        assert!(n.contains("«M»") && n.contains(": x"));
    }

    #[test]
    fn engines_with_a_user_and_a_password_ask_for_the_user() {
        for d in ["sqlserver", "azuresql", "postgres", "mysql", "oracle"] {
            assert!(needs_login(d), "{d}");
        }
        // A token or a service account: no user to type.
        for d in ["databricks", "azure_databricks", "snowflake", "fabric"] {
            assert!(!needs_login(d), "{d}");
        }
    }

    #[test]
    fn the_login_form_is_only_for_engines_that_ask_for_one() {
        for d in ["sqlserver", "azuresql", "postgres", "mysql", "oracle"] {
            assert!(needs_login(d), "{d}");
        }
        for d in ["databricks", "snowflake", "duckdb", "powerbi"] {
            assert!(!needs_login(d), "{d}");
        }
    }

    #[test]
    fn a_password_goes_to_the_keychain_only_when_asked_and_typed() {
        assert!(keeps_password(true, Some("x")));
        assert!(!keeps_password(false, Some("x")));
        assert!(!keeps_password(true, Some("")));
        assert!(!keeps_password(true, None));
    }

    #[test]
    fn the_same_source_finds_the_same_internal_connection() {
        let e = ext("srv-dw01", "indicadores", Some("dbo"), "medidas");
        let key = source_key_of_external(&e).unwrap();
        let mut mine = conn("mine", "sqlserver", "srv-dw01", "indicadores");
        mine.config.read_only = true;
        let other = conn("other", "sqlserver", "srv-dw02", "indicadores");
        let list = vec![other, mine];
        assert_eq!(
            internal_for(&list, &key.signature()).map(|c| c.id.as_str()),
            Some("mine")
        );
        let none = source_key_of_external(&ext("srv-x", "d", None, "t")).unwrap();
        assert!(internal_for(&list, &none.signature()).is_none());
        assert_eq!(internal_name(&list[1].config), "indicadores · srv-dw01");
    }
}
