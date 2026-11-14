//! What part C's modules need from a session, as one trait: the REST and
//! Fabric calls, the driver's own DAX reads, the listing and the session's
//! state. [`Ctx`] implements it by handing each call to part B's client
//! ([`crate::http::Api`], [`crate::query`], [`crate::catalog`]); the tests
//! implement it with a fake service ([`super::fake`]) that answers with the
//! Microsoft Learn samples and records every request, so a test can check
//! what was sent, and that nothing was (S1).

use super::MetaState;
use crate::auth::Account;
use crate::catalog::{self, Capacity, Model};
use crate::config::{AuthMode, Settings};
use crate::http::{Method, Priority, RestResponse, WindowUsage};
use crate::query::{self, ApiKind, QueryOpts, Rowset, Target};
use crate::Ctx;
use dbine_driver::{async_trait, Error, Result};
use serde_json::Value;

#[async_trait]
pub trait Service: Send + Sync {
    fn settings(&self) -> &Settings;
    /// The session's model, when it has one.
    fn session_model(&self) -> Option<&Model>;
    fn account(&self) -> Option<Account>;
    fn window_usage(&self) -> WindowUsage;
    fn api_in_use(&self) -> Option<ApiKind>;
    fn meta_state(&mut self) -> &mut MetaState;

    /// `GET {api base}{path}`.
    async fn get(&self, path: &str, priority: Priority) -> Result<Value>;
    /// Any method on `{api base}{path}`.
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        priority: Priority,
    ) -> Result<RestResponse>;
    /// Any method on the Fabric API (`{FABRIC_API}{path}`).
    async fn fabric_send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        priority: Priority,
    ) -> Result<RestResponse>;
    /// `GET` a Fabric operation's `Location` (part B checks the host).
    async fn fabric_follow(&self, location: &str, priority: Priority) -> Result<RestResponse>;
    /// A DAX read of the driver's own (never impersonated unless asked).
    async fn dax(&mut self, target: &Target, dax: &str, opts: &QueryOpts) -> Result<Vec<Rowset>>;
    async fn arrow_available(&mut self, target: &Target) -> Result<bool>;
    /// The models of the session's workspace (jobs, schedules).
    async fn workspace_models(&mut self) -> Result<Vec<Model>>;
    /// A model by its listing name (or id), checked against the listing.
    async fn resolve(&mut self, listed: &str) -> Result<Model>;
    async fn capacity(&mut self, id: &str) -> Result<Option<Capacity>>;
    /// Read the listing again (after a rename or a delete).
    async fn refresh_listing(&mut self) -> Result<()>;
}

#[async_trait]
impl Service for Ctx {
    fn settings(&self) -> &Settings {
        &self.settings
    }
    fn session_model(&self) -> Option<&Model> {
        self.model.as_ref()
    }
    fn account(&self) -> Option<Account> {
        self.api.account()
    }
    fn window_usage(&self) -> WindowUsage {
        self.api.window_usage()
    }
    fn api_in_use(&self) -> Option<ApiKind> {
        query::api_in_use(self)
    }
    fn meta_state(&mut self) -> &mut MetaState {
        &mut self.meta
    }
    async fn get(&self, path: &str, priority: Priority) -> Result<Value> {
        self.api.rest_get(path, priority).await
    }
    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        priority: Priority,
    ) -> Result<RestResponse> {
        self.api.rest_send(method, path, body, priority).await
    }
    async fn fabric_send(
        &self,
        method: Method,
        path: &str,
        body: Option<&Value>,
        priority: Priority,
    ) -> Result<RestResponse> {
        self.api.fabric_send(method, path, body, priority).await
    }
    async fn fabric_follow(&self, location: &str, priority: Priority) -> Result<RestResponse> {
        self.api.fabric_follow(location, priority).await
    }
    async fn dax(&mut self, target: &Target, dax: &str, opts: &QueryOpts) -> Result<Vec<Rowset>> {
        query::run(self, target, dax, opts).await
    }
    async fn arrow_available(&mut self, target: &Target) -> Result<bool> {
        query::arrow_available(self, target).await
    }
    async fn workspace_models(&mut self) -> Result<Vec<Model>> {
        catalog::workspace_models(self).await
    }
    async fn resolve(&mut self, listed: &str) -> Result<Model> {
        catalog::resolve(self, listed).await
    }
    async fn capacity(&mut self, id: &str) -> Result<Option<Capacity>> {
        catalog::capacity(self, id).await
    }
    async fn refresh_listing(&mut self) -> Result<()> {
        catalog::list_databases(self).await.map(|_| ())
    }
}

/// The session's model, or the error that says to pick one (the same text
/// as [`catalog::model`]).
pub fn session_model<S: Service + ?Sized>(s: &S) -> Result<Model> {
    s.session_model().cloned().ok_or_else(|| {
        Error::Query("Elegí un modelo semántico: esta sesión no tiene uno abierto.".into())
    })
}

/// The model `database` names (as `list_databases` does): the session's own
/// when it's that one, else looked up in the listing. Properties, health
/// and deletes run on the connection's listing session, which has none.
pub async fn named_model<S: Service + ?Sized>(s: &mut S, database: &str) -> Result<Model> {
    if let Some(m) = s.session_model() {
        if database.is_empty() || m.listed_as == database || m.id.eq_ignore_ascii_case(database) {
            return Ok(m.clone());
        }
    }
    if database.is_empty() {
        return session_model(s);
    }
    s.resolve(database).await
}

/// The signed-in principal is a service principal (or a managed identity):
/// refreshes and schedules then take `NoNotification` only.
pub fn is_service_principal<S: Service + ?Sized>(s: &S) -> bool {
    match s.settings().auth {
        AuthMode::EntraSp | AuthMode::EntraMsi => true,
        AuthMode::EntraDefault => s.account().is_some_and(|a| a.upn.is_none()),
        AuthMode::EntraMfa | AuthMode::Token => false,
    }
}

/// The signed-in user owns the model (its `configuredBy`). `None` when
/// either side is unknown.
pub fn is_owner<S: Service + ?Sized>(s: &S, model: &Model) -> Option<bool> {
    let owner = model.configured_by.as_deref()?;
    let me = s.account()?.upn?;
    Some(owner.eq_ignore_ascii_case(&me))
}

/// Refuses a change on a read-only connection, before any request (S1):
/// the token may carry write scopes whatever the connection asked for.
pub fn refuse_on_read_only<S: Service + ?Sized>(s: &S, what: &str) -> Result<()> {
    if s.settings().read_only {
        return Err(Error::Query(format!(
            "La conexión es de solo lectura: {what} no se hace desde ella. Desmarcá «Solo lectura» en la conexión para hacerlo."
        )));
    }
    Ok(())
}

/// A REST answer that came back with an error status (part B maps most of
/// them to `Err`; this covers the ones it passes through).
pub fn check_status(resp: RestResponse, what: &str) -> Result<RestResponse> {
    if resp.status < 400 {
        return Ok(resp);
    }
    let code = resp
        .body
        .pointer("/error/code")
        .or_else(|| resp.body.get("errorCode"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let detail = resp
        .body
        .pointer("/error/message")
        .or_else(|| resp.body.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut msg = format!("{what}: Power BI respondió {}", resp.status);
    if !code.is_empty() {
        msg.push_str(&format!(" ({code})"));
    }
    if !detail.is_empty() {
        msg.push_str(&format!(
            ": {}",
            detail.chars().take(500).collect::<String>()
        ));
    }
    Err(Error::Query(msg))
}
