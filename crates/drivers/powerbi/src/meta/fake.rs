//! A fake Power BI service for part C's tests ([`super::service::Service`]):
//! REST and Fabric routes answer with the Microsoft Learn samples
//! (`tests/fixtures/`), DAX reads answer per `INFO` function, and every
//! request is recorded, so a test checks what was sent and that a refused
//! change sent nothing (S1).

use super::service::Service;
use super::MetaState;
use crate::auth::Account;
use crate::catalog::{Capacity, Model, Workspace};
use crate::config::Settings;
use crate::http::{Method, Priority, RestResponse, WindowUsage};
use crate::query::{ApiKind, QueryOpts, Rowset, Target};
use dbine_driver::{async_trait, ConnectionConfig, Error, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

pub const GROUP: &str = "f089354e-8366-4e18-aea3-4cb4a3a50b48";

/// A result set for a fake DAX answer.
pub fn rowset(columns: &[&str], rows: Vec<Value>) -> Rowset {
    Rowset {
        columns: columns.iter().map(|c| c.to_string()).collect(),
        types: Vec::new(),
        rows: rows
            .into_iter()
            .map(|r| match r {
                Value::Array(a) => a,
                other => vec![other],
            })
            .collect(),
        truncated: false,
    }
}

/// One request that reached the fake service.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: Method,
    pub path: String,
    pub body: Option<Value>,
    pub fabric: bool,
}

#[derive(Debug, Clone)]
pub struct Route {
    pub method: Method,
    pub path: String,
    pub fabric: bool,
    pub status: u16,
    pub body: Value,
    pub location: Option<String>,
    pub retry_after: Option<Duration>,
    /// Answers this many times, then it's gone (`None`: always).
    pub times: Option<usize>,
}

pub struct Fake {
    pub settings: Settings,
    pub model: Option<Model>,
    pub models: Vec<Model>,
    pub account: Option<Account>,
    pub arrow: bool,
    pub arrow_error: Option<Error>,
    pub window: WindowUsage,
    pub meta: MetaState,
    pub capacities: BTreeMap<String, Capacity>,
    /// Function name (or any text of an `EVALUATE`) → its answer; `None`
    /// refuses it (and the request it's in).
    pub dax: Vec<(String, Option<Rowset>)>,
    pub dax_log: Vec<String>,
    pub routes: Mutex<Vec<Route>>,
    pub requests: Mutex<Vec<Request>>,
    pub listings: u32,
}

static NEXT: AtomicU64 = AtomicU64::new(1);

/// A model id of its own per fake, so the host-wide caches never mix
/// tests.
fn fresh_id() -> String {
    format!(
        "cfafbeb1-8037-4d0c-896e-{:012x}",
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

pub fn config(read_only: bool, opts: &[(&str, &str)]) -> ConnectionConfig {
    ConnectionConfig {
        driver: "powerbi".into(),
        read_only,
        options: opts
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    }
}

impl Fake {
    /// A read-only connection of a Read + Build user on a Pro workspace
    /// model ("Finanzas › Ventas").
    pub fn new() -> Fake {
        let model = Model {
            id: fresh_id(),
            name: "Ventas".into(),
            listed_as: "Ventas".into(),
            workspace: Workspace {
                id: Some(GROUP.into()),
                name: "Finanzas".into(),
                ..Default::default()
            },
            configured_by: Some("john@contoso.com".into()),
            ..Default::default()
        };
        Fake {
            settings: Settings::from_config(&config(true, &[])).unwrap(),
            model: Some(model.clone()),
            models: vec![model],
            account: None,
            arrow: false,
            arrow_error: None,
            window: WindowUsage {
                used: 3,
                limit: 120,
            },
            meta: MetaState::default(),
            capacities: BTreeMap::new(),
            dax: Vec::new(),
            dax_log: Vec::new(),
            routes: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
            listings: 0,
        }
    }

    /// The model's owner on a connection that isn't read-only, whose token
    /// carries the write scopes.
    pub fn writer() -> Fake {
        let mut f = Fake::new();
        f.settings = Settings::from_config(&config(false, &[])).unwrap();
        f.account = Some(owner());
        for m in f.models.iter_mut().chain(f.model.as_mut()) {
            m.write_permission = true;
            m.is_refreshable = Some(true);
        }
        f
    }

    pub fn set_read_only(&mut self, read_only: bool) {
        self.settings.read_only = read_only;
    }

    pub fn dax_answer(&mut self, needle: &str, sets: Vec<Rowset>) {
        for s in sets {
            self.dax.push((needle.to_string(), Some(s)));
        }
    }

    pub fn dax_refuse(&mut self, needle: &str) {
        self.dax.push((needle.to_string(), None));
    }

    pub fn dax_calls(&self) -> usize {
        self.dax_log.len()
    }

    pub fn route(&self, method: Method, path: &str, status: u16, body: Value) {
        self.push(Route {
            method,
            path: path.into(),
            fabric: false,
            status,
            body,
            location: None,
            retry_after: None,
            times: None,
        });
    }

    pub fn push(&self, r: Route) {
        self.routes.lock().unwrap().push(r);
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    /// Requests that change something (anything but a `GET` and the reads
    /// that are `POST`s).
    pub fn writes(&self) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|r| r.method != Method::Get && !r.path.contains("getDefinition"))
            .collect()
    }

    /// Fabric's `getDefinition` as a long-running operation: 202, one poll
    /// still running, then the result with `model.bim`.
    pub fn definition(&mut self, model_bim: &str) {
        use base64::Engine;
        let m = self.model.clone().unwrap();
        let g = m.workspace.id.clone().unwrap();
        let op =
            "https://api.fabric.microsoft.com/v1/operations/431e8d7b-4a95-4c02-8ccd-6faef5ba1bd7";
        self.push(Route {
            method: Method::Post,
            path: format!(
                "/workspaces/{g}/semanticModels/{}/getDefinition?format=TMSL",
                m.id
            ),
            fabric: true,
            status: 202,
            body: Value::Null,
            location: Some(op.into()),
            retry_after: Some(Duration::from_millis(1)),
            times: None,
        });
        self.push(Route {
            method: Method::Get,
            path: op.into(),
            fabric: true,
            status: 200,
            body: serde_json::json!({"status": "Running", "percentComplete": 25}),
            location: None,
            retry_after: Some(Duration::from_millis(1)),
            times: Some(1),
        });
        self.push(Route {
            method: Method::Get,
            path: op.into(),
            fabric: true,
            status: 200,
            body: serde_json::json!({"status": "Succeeded", "percentComplete": 100, "error": null}),
            location: Some(format!("{op}/result")),
            retry_after: None,
            times: None,
        });
        let payload = base64::engine::general_purpose::STANDARD.encode(model_bim);
        self.push(Route {
            method: Method::Get,
            path: format!("{op}/result"),
            fabric: true,
            status: 200,
            body: serde_json::json!({"definition": {"parts": [
                {"path": "model.bim", "payload": payload, "payloadType": "InlineBase64"},
                {"path": "definition.pbism", "payload": "e30=", "payloadType": "InlineBase64"},
                {"path": ".platform", "payload": "ZG90UGxhdGZvcm1CYXNlNjRTdHJpbmc=", "payloadType": "InlineBase64"}
            ]}}),
            location: None,
            retry_after: None,
            times: None,
        });
    }

    fn answer(
        &self,
        method: Method,
        path: &str,
        fabric: bool,
        body: Option<&Value>,
    ) -> Result<RestResponse> {
        self.requests.lock().unwrap().push(Request {
            method,
            path: path.to_string(),
            body: body.cloned(),
            fabric,
        });
        let mut routes = self.routes.lock().unwrap();
        let Some(i) = routes
            .iter()
            .position(|r| r.method == method && r.path == path && r.fabric == fabric)
        else {
            return Err(Error::Query(
                "No se encontró el modelo o el área de trabajo, o tu cuenta no tiene acceso".into(),
            ));
        };
        let r = routes[i].clone();
        if let Some(n) = routes[i].times.as_mut() {
            *n -= 1;
            if *n == 0 {
                routes.remove(i);
            }
        }
        if r.status >= 400 {
            let code = r
                .body
                .pointer("/error/code")
                .and_then(Value::as_str)
                .unwrap_or("");
            return Err(Error::Query(format!(
                "Power BI respondió {} {code}",
                r.status
            )));
        }
        Ok(RestResponse {
            status: r.status,
            location: r.location,
            request_id: None,
            retry_after: r.retry_after,
            body: r.body,
        })
    }
}

/// The owner of the fake model, signed in.
pub fn owner() -> Account {
    Account {
        tenant_id: "72f988bf-86f1-41af-91ab-2d7cd011db47".into(),
        object_id: format!(
            "{:08x}-0000-0000-0000-000000000001",
            NEXT.fetch_add(1, Ordering::Relaxed)
        ),
        upn: Some("John@Contoso.com".into()),
        scopes: vec![
            "Dataset.Read.All".into(),
            "Dataset.ReadWrite.All".into(),
            "Workspace.Read.All".into(),
        ],
    }
}

#[async_trait]
impl Service for Fake {
    fn settings(&self) -> &Settings {
        &self.settings
    }
    fn session_model(&self) -> Option<&Model> {
        self.model.as_ref()
    }
    fn account(&self) -> Option<Account> {
        self.account.clone()
    }
    fn window_usage(&self) -> WindowUsage {
        self.window
    }
    fn api_in_use(&self) -> Option<ApiKind> {
        Some(if self.arrow {
            ApiKind::Arrow
        } else {
            ApiKind::Json
        })
    }
    fn meta_state(&mut self) -> &mut MetaState {
        &mut self.meta
    }
    async fn get(&self, path: &str, _priority: Priority) -> Result<Value> {
        self.answer(Method::Get, path, false, None).map(|r| r.body)
    }
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        _priority: Priority,
    ) -> Result<RestResponse> {
        self.answer(method, path, false, body)
    }
    async fn fabric_send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        _priority: Priority,
    ) -> Result<RestResponse> {
        self.answer(method, path, true, body)
    }
    async fn fabric_follow(&self, location: &str, _priority: Priority) -> Result<RestResponse> {
        // Any Location is taken and recorded: which hosts, schemes and
        // ports the real client refuses (S3) is tested against
        // `http::fabric_location` itself (`fabric_locations_only_on_fabric`),
        // not against a copy of the rule here.
        self.answer(Method::Get, location, true, None)
    }
    async fn dax(&mut self, _target: &Target, dax: &str, opts: &QueryOpts) -> Result<Vec<Rowset>> {
        self.dax_log.push(dax.to_string());
        if opts.api == Some(ApiKind::Arrow) && !self.arrow {
            return Err(Error::Query("executeDaxQueries no está disponible".into()));
        }
        let mut out = Vec::new();
        let mut used = vec![false; self.dax.len()];
        for piece in dax.split("EVALUATE").skip(1) {
            let found = self
                .dax
                .iter()
                .enumerate()
                .find(|(i, (needle, _))| !used[*i] && piece.contains(needle.as_str()));
            match found {
                Some((i, (_, Some(set)))) => {
                    used[i] = true;
                    out.push(set.clone());
                }
                _ => return Err(Error::Query(
                    "La consulta falló: la función requiere permisos de administrador del modelo"
                        .into(),
                )),
            }
        }
        Ok(out)
    }
    async fn arrow_available(&mut self, _target: &Target) -> Result<bool> {
        match self.arrow_error.take() {
            Some(e) => Err(e),
            None => Ok(self.arrow),
        }
    }
    async fn workspace_models(&mut self) -> Result<Vec<Model>> {
        Ok(self.models.clone())
    }
    async fn resolve(&mut self, listed: &str) -> Result<Model> {
        self.models
            .iter()
            .find(|m| m.listed_as == listed || m.id == listed)
            .cloned()
            .ok_or_else(|| Error::Query(format!("No hay un modelo «{listed}» en la lista.")))
    }
    async fn capacity(&mut self, id: &str) -> Result<Option<Capacity>> {
        Ok(self.capacities.get(id).cloned())
    }
    async fn refresh_listing(&mut self) -> Result<()> {
        self.listings += 1;
        Ok(())
    }
}
