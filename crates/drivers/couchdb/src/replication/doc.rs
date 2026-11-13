//! Replications: `_replicator` documents, created, changed and deleted
//! through their document, and transient `_replicate` ones, shown and
//! cancelled.
//!
//! A document's fields as CouchDB 3 reads them: `source` / `target` (a URL,
//! or `{url, auth: {basic: {username, password}}, headers}`; 3.2 on writes
//! `auth.basic`, earlier versions the credentials in the URL), `continuous`,
//! `create_target` and `create_target_params` (`q`, `n`, `partitioned`),
//! the filter (`doc_ids`, `selector`, `_design`, a filter function or a
//! view with `query_params`), `since_seq`, the checkpoint and worker
//! settings, `socket_options`, the proxies, `winning_revs_only` and
//! `use_bulk_get` (3.3 on). The fields DBine doesn't show (`owner`,
//! `user_ctx`, a plugin's) stay as they are.

use super::{bad, shown, Call, Change, Ctx, Vals};
use crate::jobs::{self, replication_doc, replicator_db, s as field, JobId};
use crate::{seg, CouchSession};
use dbine_driver::{
    Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerItem,
    SchedulerList, SECRET_MASK,
};
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use reqwest::Method;
use serde_json::{json, Map, Value};

const DEFAULT_DB: &str = "_replicator";
/// "Start at step" on a replication, refused: its job kind doesn't offer
/// it.
const ONE_STEP: &str = "una replicación se ejecuta entera: no empieza en uno de sus pasos; para correrla de nuevo está «Ejecutar ahora»";

const EDIT_NOTE: &str =
    "Guardar un cambio reinicia la replicación: CouchDB la retoma desde su último checkpoint.";
const SECRET_NOTE: &str = "Las contraseñas, los tokens y los encabezados secretos no se muestran: una contraseña o un token vacíos y los encabezados con asteriscos quedan como están.";
const TRANSIENT_READ_ONLY: &str =
    "Una replicación transitoria no se puede cambiar: no tiene documento. Borrarla la cancela.";

/// Characters kept in a URL's user or password.
const USERINFO: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

#[derive(Debug, Clone, Copy)]
pub(crate) struct Side {
    /// `source` or `target`.
    pub(crate) key: &'static str,
    /// In Spanish, for messages.
    pub(crate) label: &'static str,
}

pub(crate) const SOURCE: Side = Side {
    key: "source",
    label: "Origen",
};
pub(crate) const TARGET: Side = Side {
    key: "target",
    label: "Destino",
};

impl Side {
    fn f(self, what: &str) -> String {
        format!("{}_{what}", self.key)
    }
}

// ---------------------------------------------------------------------------
// Endpoints: a URL, its credentials and headers
// ---------------------------------------------------------------------------

/// A URL without its credentials, and the user and password it had.
pub(crate) fn split_userinfo(u: &str) -> (String, Option<String>, Option<String>) {
    let Some((scheme, rest)) = u.split_once("://") else {
        return (u.to_string(), None, None);
    };
    let host_end = rest.find('/').unwrap_or(rest.len());
    let Some(at) = rest[..host_end].rfind('@') else {
        return (u.to_string(), None, None);
    };
    let dec = |x: &str| percent_decode_str(x).decode_utf8_lossy().to_string();
    let (user, pass) = match rest[..at].split_once(':') {
        Some((u, p)) => (dec(u), Some(dec(p))),
        None => (dec(&rest[..at]), None),
    };
    (
        format!("{scheme}://{}", &rest[at + 1..]),
        Some(user).filter(|u| !u.is_empty()),
        pass,
    )
}

/// What a form shows of an endpoint: its URL without credentials, how it
/// authenticates, its user, whether it has a password, its headers
/// (secret ones masked; a bearer token's `Authorization` left out, it has
/// its own field) and its proxy.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct EndpointView {
    pub(crate) url: String,
    /// `none`, `basic` (a user and a password, in `auth.basic` or the URL)
    /// or `bearer` (an `Authorization: Bearer` header).
    pub(crate) auth: &'static str,
    pub(crate) user: Option<String>,
    pub(crate) has_password: bool,
    pub(crate) headers: Map<String, Value>,
}

/// The token of an `Authorization: Bearer …` header value.
fn bearer_token(v: &str) -> Option<&str> {
    let v = v.trim();
    let head = v.get(..7)?;
    head.eq_ignore_ascii_case("bearer ")
        .then(|| v[7..].trim())
        .filter(|t| !t.is_empty())
}

/// The bearer token an endpoint sends, to keep it when the form brings
/// none.
fn bearer_of(v: &Value) -> Option<String> {
    v.get("headers")?
        .as_object()?
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("authorization"))
        .and_then(|(_, v)| v.as_str())
        .and_then(bearer_token)
        .map(str::to_string)
}

pub(crate) fn endpoint_view(v: &Value) -> EndpointView {
    let (raw, obj) = match v {
        Value::String(u) => (u.clone(), None),
        Value::Object(m) => (
            m.get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            Some(m),
        ),
        _ => (String::new(), None),
    };
    let (url, url_user, url_pass) = split_userinfo(&raw);
    let basic = obj.and_then(|m| m.get("auth")).and_then(|a| a.get("basic"));
    let user = basic.and_then(|b| field(b, "username")).or(url_user);
    let has_password = basic
        .and_then(|b| b.get("password"))
        .and_then(Value::as_str)
        .is_some_and(|p| !p.is_empty())
        || url_pass.is_some_and(|p| !p.is_empty());
    let auth = if user.is_some() || has_password {
        "basic"
    } else if bearer_of(v).is_some() {
        "bearer"
    } else {
        "none"
    };
    let headers = obj
        .and_then(|m| m.get("headers"))
        .and_then(Value::as_object)
        .map(|h| {
            h.iter()
                .filter(|(k, _)| auth != "bearer" || !k.eq_ignore_ascii_case("authorization"))
                .map(|(k, v)| {
                    if jobs::secret_key(k) {
                        (k.clone(), Value::String(SECRET_MASK.into()))
                    } else {
                        (k.clone(), v.clone())
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    EndpointView {
        url,
        auth,
        user,
        has_password,
        headers,
    }
}

/// The endpoint's password, to keep it when the form brings none.
pub(crate) fn password_of(v: &Value) -> Option<String> {
    let from_auth = v
        .pointer("/auth/basic/password")
        .and_then(Value::as_str)
        .map(str::to_string);
    let url = match v {
        Value::String(u) => Some(u.as_str()),
        Value::Object(m) => m.get("url").and_then(Value::as_str),
        _ => None,
    };
    from_auth
        .or_else(|| url.and_then(|u| split_userinfo(u).2))
        .filter(|p| !p.is_empty())
}

fn check_url(side: Side, url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url).map_err(|_| {
        bad(format!(
            "«{}» no es una URL de base de datos para «{}» (http://servidor:5984/base).",
            shown(url),
            side.label
        ))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(bad(format!(
            "La URL de «{}» tiene que ser http o https.",
            side.label
        )));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(bad(format!(
            "La URL de «{}» no puede llevar usuario ni contraseña: van en sus campos.",
            side.label
        )));
    }
    if url.chars().any(char::is_control) {
        return Err(bad(format!(
            "La URL de «{}» no puede tener saltos de línea.",
            side.label
        )));
    }
    Ok(())
}

/// The headers the form brings (a JSON object of texts); the ones left
/// masked take the value the document had.
fn headers_from(side: Side, text: &str, old: Option<&Value>) -> Result<Map<String, Value>> {
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    let v: Value = serde_json::from_str(text).map_err(|e| {
        bad(format!(
            "Los encabezados de «{}» no son un objeto JSON: {e}",
            side.label
        ))
    })?;
    let Value::Object(m) = v else {
        return Err(bad(format!(
            "Los encabezados de «{}» van como un objeto JSON ({{\"Nombre\": \"valor\"}}).",
            side.label
        )));
    };
    let old_headers = old
        .and_then(|o| o.get("headers"))
        .and_then(Value::as_object);
    let mut out = Map::new();
    for (k, v) in m {
        if k.is_empty() || k.chars().any(|c| c.is_control() || c == ':' || c == ' ') {
            return Err(bad(format!(
                "«{}» no es el nombre de un encabezado.",
                shown(&k)
            )));
        }
        let Value::String(val) = v else {
            return Err(bad(format!(
                "El encabezado «{}» de «{}» tiene que ser un texto.",
                shown(&k),
                side.label
            )));
        };
        if val.chars().any(|c| c == '\r' || c == '\n') {
            return Err(bad(format!(
                "El encabezado «{}» no puede tener saltos de línea.",
                shown(&k)
            )));
        }
        let val = if val == SECRET_MASK {
            old_headers
                .and_then(|h| {
                    h.iter()
                        .find(|(name, _)| name.eq_ignore_ascii_case(&k))
                        .and_then(|(_, v)| v.as_str())
                })
                .map(str::to_string)
                .ok_or_else(|| {
                    bad(format!(
                        "El encabezado «{}» de «{}» quedó con asteriscos: escribí su valor.",
                        shown(&k),
                        side.label
                    ))
                })?
        } else {
            val
        };
        out.insert(k, Value::String(val));
    }
    Ok(out)
}

fn headers_text(h: &Map<String, Value>) -> String {
    if h.is_empty() {
        String::new()
    } else {
        serde_json::to_string_pretty(&Value::Object(h.clone())).unwrap_or_default()
    }
}

/// The endpoint the form describes; the old one as it was when nothing of
/// it changed.
/// How the form says the endpoint authenticates. A caller that doesn't
/// send it gets what its other values say: a user is a user and password.
fn auth_of(side: Side, v: Vals) -> Result<&'static str> {
    match v.text(&side.f("auth")).as_str() {
        "" if v.opt(&side.f("user")).is_some() || v.secret(&side.f("password")).is_some() => {
            Ok("basic")
        }
        "" | "none" => Ok("none"),
        "basic" => Ok("basic"),
        "bearer" => Ok("bearer"),
        other => Err(bad(format!(
            "«{}» no es una forma de autenticación de «{}».",
            shown(other),
            side.label
        ))),
    }
}

pub(crate) fn endpoint_from(c: &Ctx, side: Side, v: Vals, old: Option<&Value>) -> Result<Value> {
    let url = v
        .opt(&side.f("url"))
        .ok_or_else(|| bad(format!("Falta la URL de «{}».", side.label)))?;
    check_url(side, &url)?;
    let auth = auth_of(side, v)?;
    // The other ways' fields may still hold what they had: not this one's.
    let user = if auth == "basic" {
        v.opt(&side.f("user"))
    } else {
        None
    };
    if user
        .as_deref()
        .is_some_and(|u| u.chars().any(char::is_control))
    {
        return Err(bad(format!(
            "El usuario de «{}» no puede tener caracteres de control.",
            side.label
        )));
    }
    let typed = if auth == "basic" {
        v.secret(&side.f("password"))
    } else {
        None
    };
    let token_typed = if auth == "bearer" {
        v.secret(&side.f("token"))
    } else {
        None
    };
    let headers_raw = v.raw(&side.f("headers"));
    if let Some(old) = old {
        let view = endpoint_view(old);
        // The headers as the form showed them, to tell whether they moved.
        let same_headers = if headers_raw.trim().is_empty() {
            view.headers.is_empty()
        } else {
            serde_json::from_str::<Value>(&headers_raw).ok()
                == Some(Value::Object(view.headers.clone()))
        };
        if typed.is_none()
            && token_typed.is_none()
            && view.auth == auth
            && view.url == url
            && view.user == user
            && same_headers
        {
            return Ok(old.clone());
        }
    }
    let mut headers = headers_from(side, &headers_raw, old)?;
    if auth == "bearer" {
        if headers
            .keys()
            .any(|k| k.eq_ignore_ascii_case("authorization"))
        {
            return Err(bad(format!(
                "Con un token, el encabezado Authorization de «{}» sale del campo Token: sacalo de los encabezados.",
                side.label
            )));
        }
        let token = match token_typed {
            // Typed with its "Bearer " or without it.
            Some(t) => {
                let t = t.trim();
                bearer_token(t).unwrap_or(t).to_string()
            }
            None => old.and_then(bearer_of).unwrap_or_default(),
        };
        if token.is_empty() {
            return Err(bad(format!("Falta el token de «{}».", side.label)));
        }
        if token.chars().any(char::is_control) {
            return Err(bad(format!(
                "El token de «{}» no puede tener saltos de línea.",
                side.label
            )));
        }
        headers.insert(
            "Authorization".into(),
            Value::String(format!("Bearer {token}")),
        );
    }
    let password = typed.clone().or_else(|| old.and_then(password_of));
    let creds = match (&user, password) {
        (Some(u), Some(p)) => Some((u.clone(), p)),
        (Some(_), None) => return Err(bad(format!("Falta la contraseña de «{}».", side.label))),
        (None, _) if auth == "basic" => {
            return Err(bad(format!("Falta el usuario de «{}».", side.label)))
        }
        (None, _) => None,
    };
    // What the old object had besides these (a plugin's settings).
    let mut obj: Map<String, Value> = old
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter(|(k, _)| !matches!(k.as_str(), "url" | "auth" | "headers"))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default();
    let url = match (&creds, c.basic_auth()) {
        (Some((u, p)), false) => {
            let (scheme, rest) = url.split_once("://").unwrap_or(("http", &url));
            format!(
                "{scheme}://{}:{}@{rest}",
                utf8_percent_encode(u, USERINFO),
                utf8_percent_encode(p, USERINFO)
            )
        }
        _ => url,
    };
    if let (Some((u, p)), true) = (&creds, c.basic_auth()) {
        obj.insert(
            "auth".into(),
            json!({"basic": {"username": u, "password": p}}),
        );
    }
    if !headers.is_empty() {
        obj.insert("headers".into(), Value::Object(headers));
    }
    if obj.is_empty() {
        return Ok(Value::String(url));
    }
    obj.insert("url".into(), Value::String(url));
    Ok(Value::Object(obj))
}

// ---------------------------------------------------------------------------
// The document
// ---------------------------------------------------------------------------

/// The numbers a replication takes: key, label, min, max.
const NUMBERS: &[(&str, &str, i64, i64)] = &[
    (
        "checkpoint_interval",
        "Intervalo de checkpoint (ms)",
        1_000,
        86_400_000,
    ),
    ("worker_processes", "Procesos", 1, 128),
    ("worker_batch_size", "Documentos por lote", 1, 1_000_000),
    ("http_connections", "Conexiones HTTP", 1, 10_000),
    (
        "connection_timeout",
        "Tiempo de espera de conexión (ms)",
        1_000,
        86_400_000,
    ),
    ("retries_per_request", "Reintentos por pedido", 0, 1_000),
];

/// The texts a replication takes: key, label.
const TEXTS: &[(&str, &str)] = &[
    ("socket_options", "Opciones de socket"),
    ("proxy", "Proxy (origen y destino)"),
    ("source_proxy", "Proxy del origen"),
    ("target_proxy", "Proxy del destino"),
];

/// What the replicator writes on a document, and its ids: not part of
/// the definition.
fn definition(doc: &Map<String, Value>) -> Map<String, Value> {
    doc.iter()
        .filter(|(k, _)| *k != "_id" && *k != "_rev" && !jobs::replicator_state(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// A flag written only when it isn't CouchDB's default (or the document
/// already had it).
fn flag(
    doc: &mut Map<String, Value>,
    old: Option<&Map<String, Value>>,
    key: &str,
    v: bool,
    default: bool,
) {
    if v == default && !old.is_some_and(|o| o.contains_key(key)) {
        doc.remove(key);
    } else {
        doc.insert(key.into(), json!(v));
    }
}

fn json_object(text: &str, label: &str) -> Result<Option<Map<String, Value>>> {
    if text.trim().is_empty() {
        return Ok(None);
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(m)) => Ok(Some(m)),
        _ => Err(bad(format!("«{label}» tiene que ser un objeto JSON."))),
    }
}

/// `ddoc/name`: a design document's function.
fn ddoc_name(v: &str) -> bool {
    match v.split_once('/') {
        Some((d, n)) => {
            !d.is_empty()
                && !n.is_empty()
                && !n.contains('/')
                && !v.chars().any(char::is_control)
                && v.len() <= 512
        }
        None => false,
    }
}

/// A flag of the form, or its default when the form didn't bring it.
fn bool_or(v: Vals, key: &str, default: bool) -> bool {
    match v.0.get(key) {
        None | Some(Value::Null) => default,
        Some(_) => v.bool(key),
    }
}

/// The filter the form describes, written on the document.
fn put_filter(doc: &mut Map<String, Value>, v: Vals) -> Result<()> {
    for k in ["doc_ids", "selector", "filter", "query_params"] {
        doc.remove(k);
    }
    match v.text("filter_type").as_str() {
        "" | "none" => {}
        "doc_ids" => {
            let ids: Vec<Value> = v
                .raw("doc_ids")
                .lines()
                .map(|l| l.trim_end_matches('\r'))
                .filter(|l| !l.trim().is_empty())
                .map(|l| Value::String(l.to_string()))
                .collect();
            if ids.is_empty() {
                return Err(bad(
                    "Escribí los ids de los documentos que se replican, uno por línea.".into(),
                ));
            }
            doc.insert("doc_ids".into(), Value::Array(ids));
        }
        "selector" => {
            let sel = json_object(&v.raw("selector"), "Selector")?
                .ok_or_else(|| bad("Falta el selector Mango.".into()))?;
            doc.insert("selector".into(), Value::Object(sel));
        }
        "design" => {
            doc.insert("filter".into(), json!("_design"));
        }
        "filter" => {
            let name = v
                .opt("filter_name")
                .ok_or_else(|| bad("Falta la función de filtro (diseño/nombre).".into()))?;
            if !ddoc_name(&name) {
                return Err(bad(format!(
                    "«{}» no es una función de filtro (diseño/nombre).",
                    shown(&name)
                )));
            }
            doc.insert("filter".into(), json!(name));
            if let Some(q) = json_object(&v.raw("query_params"), "Parámetros del filtro")? {
                doc.insert("query_params".into(), Value::Object(q));
            }
        }
        "view" => {
            let name = v
                .opt("view_name")
                .ok_or_else(|| bad("Falta la vista (diseño/nombre).".into()))?;
            if !ddoc_name(&name) {
                return Err(bad(format!(
                    "«{}» no es una vista (diseño/nombre).",
                    shown(&name)
                )));
            }
            let mut q =
                json_object(&v.raw("query_params"), "Parámetros del filtro")?.unwrap_or_default();
            q.insert("view".into(), json!(name));
            doc.insert("filter".into(), json!("_view"));
            doc.insert("query_params".into(), Value::Object(q));
        }
        other => {
            return Err(bad(format!(
                "«{}» no es un filtro de replicación.",
                shown(other)
            )))
        }
    }
    Ok(())
}

/// The document the form describes: the old one (when there is) with what
/// the form shows changed, the rest kept.
pub(crate) fn build_doc(
    c: &Ctx,
    values: &FormValues,
    old: Option<&Map<String, Value>>,
) -> Result<Map<String, Value>> {
    let v = Vals(&values.values);
    let mut doc = old.map(definition).unwrap_or_default();
    let old_side = |side: Side| old.and_then(|o| o.get(side.key));
    doc.insert(
        "source".into(),
        endpoint_from(c, SOURCE, v, old_side(SOURCE))?,
    );
    doc.insert(
        "target".into(),
        endpoint_from(c, TARGET, v, old_side(TARGET))?,
    );
    flag(&mut doc, old, "continuous", v.bool("continuous"), false);
    let create = v.bool("create_target");
    flag(&mut doc, old, "create_target", create, false);
    let mut params = Map::new();
    if create {
        if let Some(q) = v.int("create_target_q", "Fragmentos (q)", 1, 256)? {
            params.insert("q".into(), json!(q));
        }
        if let Some(n) = v.int("create_target_n", "Réplicas (n)", 1, 32)? {
            params.insert("n".into(), json!(n));
        }
        if v.bool("create_target_partitioned") {
            params.insert("partitioned".into(), json!(true));
        }
    }
    if params.is_empty() {
        doc.remove("create_target_params");
    } else {
        doc.insert("create_target_params".into(), Value::Object(params));
    }
    put_filter(&mut doc, v)?;
    match v.opt("since_seq") {
        Some(seq) => {
            let kept = old
                .and_then(|o| o.get("since_seq"))
                .filter(|x| match x {
                    Value::String(s) => *s == seq,
                    x => x.to_string() == seq,
                })
                .cloned();
            doc.insert("since_seq".into(), kept.unwrap_or(Value::String(seq)));
        }
        None => {
            doc.remove("since_seq");
        }
    }
    flag(
        &mut doc,
        old,
        "use_checkpoints",
        bool_or(v, "use_checkpoints", true),
        true,
    );
    for (key, label, min, max) in NUMBERS {
        match v.int(key, label, *min, *max)? {
            Some(n) => {
                doc.insert((*key).into(), json!(n));
            }
            None => {
                doc.remove(*key);
            }
        }
    }
    for (key, label) in TEXTS {
        let was = old.and_then(|o| o.get(*key)).and_then(Value::as_str);
        match v.opt(key) {
            Some(t) if t.chars().any(char::is_control) => {
                return Err(bad(format!("«{label}» no puede tener saltos de línea.")))
            }
            // A proxy URL shown with its password masked, untouched.
            Some(t) if was.is_some_and(|w| w != t && super::mask_url(w) == t) => {}
            Some(t) if t.contains(SECRET_MASK) => {
                return Err(bad(format!(
                    "«{label}» quedó con asteriscos: escribí la contraseña de la URL."
                )))
            }
            Some(t) => {
                doc.insert((*key).into(), json!(t));
            }
            None => {
                doc.remove(*key);
            }
        }
    }
    if doc.contains_key("proxy")
        && (doc.contains_key("source_proxy") || doc.contains_key("target_proxy"))
    {
        return Err(bad(
            "CouchDB no acepta un proxy para los dos lados junto con el del origen o el del destino."
                .into(),
        ));
    }
    if c.v33() {
        flag(
            &mut doc,
            old,
            "winning_revs_only",
            v.bool("winning_revs_only"),
            false,
        );
        flag(
            &mut doc,
            old,
            "use_bulk_get",
            bool_or(v, "use_bulk_get", true),
            true,
        );
    }
    Ok(doc)
}

/// The top-level fields that differ, for the script's comment.
pub(crate) fn changed(old: &Map<String, Value>, new: &Map<String, Value>) -> Vec<String> {
    let mut keys: Vec<&String> = old.keys().chain(new.keys()).collect();
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter(|k| old.get(*k) != new.get(*k))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// Reading and the list
// ---------------------------------------------------------------------------

pub(crate) async fn items(s: &mut CouchSession) -> Result<SchedulerList> {
    let list = s.server_jobs().await?;
    Ok(SchedulerList {
        items: list
            .jobs
            .into_iter()
            .map(|j| SchedulerItem {
                // A transient replication has no document to change; its
                // delete cancels it.
                can_edit: matches!(jobs::parse_id(&j.id), Ok(JobId::Transient(_))).then_some(false),
                id: j.id,
                name: j.name,
                description: j.description,
                enabled: None,
                system: false,
                can_delete: None,
                actions: None,
            })
            .collect(),
        notes: list.notes,
    })
}

/// The `_replicator` databases, and whether `_replicator` itself is there.
async fn replicator_dbs(s: &CouchSession) -> Result<Vec<String>> {
    let all = s.call(Method::GET, "/_all_dbs", None).await?;
    Ok(all
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|d| replicator_db(d))
        .map(str::to_string)
        .collect())
}

async fn read_doc(s: &CouchSession, db: &str, doc: &str, id: &str) -> Result<Map<String, Value>> {
    let v = s.replication_doc(db, doc).await.map_err(|e| {
        if jobs::missing(&e) {
            jobs::not_found(id)
        } else {
            e
        }
    })?;
    v.as_object().cloned().ok_or_else(|| jobs::not_found(id))
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

fn side_fields(side: Side, existing: bool) -> Vec<FormField> {
    // The keys are literals so the message extractor finds the labels.
    let [url, auth, user, password, token, headers] = if side.key == "source" {
        [
            FormField::new("source_url", "URL", FormKind::Text).help(
                "La base de datos que se copia, como la ve el servidor (localhost es el propio servidor).",
            ),
            FormField::new(
                "source_auth",
                "Autenticación",
                FormKind::select(&[
                    ("none", "Sin credenciales"),
                    ("basic", "Usuario y contraseña"),
                    ("bearer", "Token (Authorization: Bearer)"),
                ]),
            ),
            FormField::new("source_user", "Usuario", FormKind::Text),
            FormField::new("source_password", "Contraseña", FormKind::Secret),
            FormField::new("source_token", "Token", FormKind::Secret),
            FormField::new("source_headers", "Encabezados (JSON)", FormKind::code("json")),
        ]
    } else {
        [
            FormField::new("target_url", "URL", FormKind::Text)
                .help("La base de datos donde se copia, como la ve el servidor."),
            FormField::new(
                "target_auth",
                "Autenticación",
                FormKind::select(&[
                    ("none", "Sin credenciales"),
                    ("basic", "Usuario y contraseña"),
                    ("bearer", "Token (Authorization: Bearer)"),
                ]),
            ),
            FormField::new("target_user", "Usuario", FormKind::Text),
            FormField::new("target_password", "Contraseña", FormKind::Secret),
            FormField::new("target_token", "Token", FormKind::Secret),
            FormField::new(
                "target_headers",
                "Encabezados (JSON)",
                FormKind::code("json"),
            ),
        ]
    };
    debug_assert!(url.key == side.f("url") && auth.key == side.f("auth"));
    let by = side.f("auth");
    vec![
        url.required().placeholder("https://servidor:6984/ventas"),
        auth.required()
            .default(json!("basic"))
            .help("Cómo se identifica el replicador ante esta base de datos: con un usuario y su contraseña (auth.basic), o con un token, como un JWT, en el encabezado Authorization."),
        user.required().when(&by, &["basic"]),
        password.when(&by, &["basic"]).help(if existing {
            "Vacía: queda la que tiene."
        } else {
            "La contraseña del usuario."
        }),
        token.when(&by, &["bearer"]).help(if existing {
            "Vacío: queda el que tiene."
        } else {
            "Va en el encabezado Authorization: Bearer; se puede escribir con o sin «Bearer»."
        }),
        headers
            .placeholder("{\"X-Origen\": \"dbine\"}")
            .help("Encabezados HTTP de cada pedido; los secretos se ven con asteriscos y así quedan como están."),
    ]
}

fn general_fields(existing: bool) -> Vec<FormField> {
    vec![
        FormField::new("doc_id", "Id del documento", FormKind::Text)
            .read_only(existing)
            .placeholder("ventas-a-respaldo")
            .help("El _id del documento en _replicator; vacío: lo elige CouchDB. No se puede cambiar después."),
        FormField::new("database", "Base de replicación", FormKind::reference("replicator_db", false))
            .required()
            .read_only(existing)
            .default(json!(DEFAULT_DB))
            .help("_replicator, u otra base terminada en /_replicator."),
        FormField::new("continuous", "Continua", FormKind::Bool)
            .default(json!(false))
            .help("Sigue copiando los cambios nuevos; si no, termina al alcanzar al origen."),
        FormField::new("create_target", "Crear el destino si no existe", FormKind::Bool)
            .default(json!(false)),
        FormField::new("create_target_q", "Fragmentos (q)", FormKind::number(Some(1.0), Some(256.0)))
            .when("create_target", &["true"])
            .help("Vacío: el valor del servidor."),
        FormField::new("create_target_n", "Réplicas (n)", FormKind::number(Some(1.0), Some(32.0)))
            .when("create_target", &["true"])
            .help("Vacío: el valor del servidor."),
        FormField::new("create_target_partitioned", "Base particionada", FormKind::Bool)
            .when("create_target", &["true"])
            .default(json!(false)),
    ]
}

fn filter_fields() -> Vec<FormField> {
    vec![
        FormField::new(
            "filter_type",
            "Qué se replica",
            FormKind::select(&[
                ("none", "Todos los documentos"),
                ("doc_ids", "Los documentos elegidos por id"),
                ("selector", "Los que cumplen un selector Mango"),
                ("design", "Solo los documentos de diseño"),
                ("filter", "Los que pasa una función de filtro"),
                ("view", "Los que emite una vista"),
            ]),
        )
        .required()
        .default(json!("none")),
        FormField::new("doc_ids", "Ids de documentos", FormKind::Textarea)
            .when("filter_type", &["doc_ids"])
            .help("Uno por línea."),
        FormField::new("selector", "Selector", FormKind::code("json"))
            .when("filter_type", &["selector"])
            .placeholder("{\"tipo\": \"pedido\"}"),
        FormField::new("filter_name", "Función de filtro", FormKind::Text)
            .when("filter_type", &["filter"])
            .placeholder("app/por_tipo")
            .help("diseño/nombre: el filtro del documento de diseño del origen."),
        FormField::new("view_name", "Vista", FormKind::Text)
            .when("filter_type", &["view"])
            .placeholder("app/activos")
            .help("diseño/nombre: una vista del origen; se replican los documentos que emite."),
        FormField::new(
            "query_params",
            "Parámetros del filtro",
            FormKind::code("json"),
        )
        .when("filter_type", &["filter", "view"])
        .placeholder("{\"tipo\": \"pedido\"}"),
        FormField::new("since_seq", "Desde la secuencia", FormKind::Text).help(
            "Empieza a copiar desde esta secuencia de cambios del origen, no desde el principio.",
        ),
    ]
}

fn advanced_fields(c: &Ctx) -> Vec<FormField> {
    let mut out = vec![
        FormField::new("use_checkpoints", "Usar checkpoints", FormKind::Bool)
            .default(json!(true))
            .help("Guarda hasta dónde llegó, para retomar desde ahí."),
        FormField::new(
            "checkpoint_interval",
            "Intervalo de checkpoint (ms)",
            FormKind::number(Some(1000.0), None),
        )
        .help("Vacío: el del servidor (30000)."),
        FormField::new(
            "worker_processes",
            "Procesos",
            FormKind::number(Some(1.0), Some(128.0)),
        )
        .help("Vacío: el del servidor (4)."),
        FormField::new(
            "worker_batch_size",
            "Documentos por lote",
            FormKind::number(Some(1.0), None),
        )
        .help("Vacío: el del servidor (500)."),
        FormField::new(
            "http_connections",
            "Conexiones HTTP",
            FormKind::number(Some(1.0), None),
        )
        .help("Vacío: el del servidor (20)."),
        FormField::new(
            "connection_timeout",
            "Tiempo de espera de conexión (ms)",
            FormKind::number(Some(1000.0), None),
        )
        .help("Vacío: el del servidor (30000)."),
        FormField::new(
            "retries_per_request",
            "Reintentos por pedido",
            FormKind::number(Some(0.0), None),
        )
        .help("Vacío: el del servidor (5)."),
        FormField::new("socket_options", "Opciones de socket", FormKind::Text)
            .placeholder("[{keepalive, true}, {nodelay, false}]")
            .help("En la sintaxis de Erlang, como en la configuración del replicador."),
        FormField::new("proxy", "Proxy (origen y destino)", FormKind::Text)
            .placeholder("http://proxy:3128"),
        FormField::new("source_proxy", "Proxy del origen", FormKind::Text),
        FormField::new("target_proxy", "Proxy del destino", FormKind::Text),
    ];
    if c.v33() {
        out.push(
            FormField::new(
                "winning_revs_only",
                "Solo las revisiones ganadoras",
                FormKind::Bool,
            )
            .default(json!(false))
            .help("Copia solo la revisión ganadora de cada documento, sin sus conflictos."),
        );
        out.push(
            FormField::new("use_bulk_get", "Usar _bulk_get", FormKind::Bool).default(json!(true)),
        );
    }
    out
}

fn status_section() -> FormSection {
    FormSection::new("status", "Estado").fields(vec![
        FormField::new("owner", "Dueño", FormKind::Text).read_only(true),
        FormField::new("state", "Estado", FormKind::Text).read_only(true),
        FormField::new("error_count", "Errores seguidos", FormKind::Text).read_only(true),
        FormField::new("last_error", "Último error", FormKind::Textarea).read_only(true),
        FormField::new("replication_id", "Id de replicación", FormKind::Text).read_only(true),
        FormField::new("node", "Nodo", FormKind::Text).read_only(true),
        FormField::new("last_updated", "Actualizada", FormKind::DateTime).read_only(true),
    ])
}

fn sections(c: &Ctx, existing: bool) -> Vec<FormSection> {
    let mut out = vec![
        FormSection::new("general", "General").fields(general_fields(existing)),
        FormSection::new("source", "Origen").fields(side_fields(SOURCE, existing)),
        FormSection::new("target", "Destino").fields(side_fields(TARGET, existing)),
        FormSection::new("filter", "Filtro").fields(filter_fields()),
        FormSection::new("advanced", "Avanzado").fields(advanced_fields(c)),
    ];
    if existing {
        out.push(status_section());
    }
    out
}

/// The form's values of a document.
pub(crate) fn doc_values(db: &str, doc_id: &str, doc: &Map<String, Value>) -> Map<String, Value> {
    let mut v = Map::new();
    v.insert("doc_id".into(), json!(doc_id));
    v.insert("database".into(), json!(db));
    for side in [SOURCE, TARGET] {
        let e = doc.get(side.key).map(endpoint_view).unwrap_or_default();
        v.insert(side.f("url"), json!(e.url));
        v.insert(
            side.f("auth"),
            json!(if e.auth.is_empty() { "none" } else { e.auth }),
        );
        v.insert(side.f("user"), e.user.map_or(Value::Null, Value::from));
        v.insert(side.f("password"), Value::Null);
        v.insert(side.f("token"), Value::Null);
        v.insert(side.f("headers"), json!(headers_text(&e.headers)));
    }
    let b = |k: &str, d: bool| doc.get(k).and_then(Value::as_bool).unwrap_or(d);
    v.insert("continuous".into(), json!(b("continuous", false)));
    v.insert("create_target".into(), json!(b("create_target", false)));
    let p = doc.get("create_target_params");
    let n = |k: &str| p.and_then(|p| p.get(k)).cloned().unwrap_or(Value::Null);
    v.insert("create_target_q".into(), n("q"));
    v.insert("create_target_n".into(), n("n"));
    v.insert(
        "create_target_partitioned".into(),
        json!(p
            .and_then(|p| p.get("partitioned"))
            .and_then(Value::as_bool)
            .unwrap_or(false)),
    );
    // The filter.
    let filter = doc.get("filter").and_then(Value::as_str);
    let mut params = doc
        .get("query_params")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let kind = if doc.get("doc_ids").is_some_and(Value::is_array) {
        "doc_ids"
    } else if doc.get("selector").is_some_and(Value::is_object) {
        "selector"
    } else {
        match filter {
            Some("_design") => "design",
            Some("_view") => "view",
            Some(_) => "filter",
            None => "none",
        }
    };
    v.insert("filter_type".into(), json!(kind));
    v.insert(
        "doc_ids".into(),
        json!(doc
            .get("doc_ids")
            .and_then(Value::as_array)
            .map(|a| a
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join("\n"))
            .unwrap_or_default()),
    );
    v.insert(
        "selector".into(),
        json!(doc
            .get("selector")
            .map(|x| serde_json::to_string_pretty(x).unwrap_or_default())
            .unwrap_or_default()),
    );
    v.insert(
        "filter_name".into(),
        json!(if kind == "filter" {
            filter.unwrap_or_default()
        } else {
            ""
        }),
    );
    let view = if kind == "view" {
        params
            .remove("view")
            .and_then(|x| x.as_str().map(str::to_string))
            .unwrap_or_default()
    } else {
        String::new()
    };
    v.insert("view_name".into(), json!(view));
    v.insert(
        "query_params".into(),
        json!(if params.is_empty() {
            String::new()
        } else {
            serde_json::to_string_pretty(&Value::Object(params)).unwrap_or_default()
        }),
    );
    v.insert(
        "since_seq".into(),
        json!(match doc.get("since_seq") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(x) => x.to_string(),
        }),
    );
    v.insert("use_checkpoints".into(), json!(b("use_checkpoints", true)));
    for (key, ..) in NUMBERS {
        v.insert((*key).into(), doc.get(*key).cloned().unwrap_or(Value::Null));
    }
    for (key, _) in TEXTS {
        v.insert(
            (*key).into(),
            json!(doc
                .get(*key)
                .and_then(Value::as_str)
                .map(super::mask_url)
                .unwrap_or_default()),
        );
    }
    v.insert(
        "winning_revs_only".into(),
        json!(b("winning_revs_only", false)),
    );
    v.insert("use_bulk_get".into(), json!(b("use_bulk_get", true)));
    v.insert(
        "owner".into(),
        json!(doc.get("owner").and_then(Value::as_str).unwrap_or_default()),
    );
    v
}

/// The scheduler's view of a document: its state.
fn status_values(sched: Option<&Value>, v: &mut Map<String, Value>) {
    let st = |k: &str| sched.and_then(|d| field(d, k)).unwrap_or_default();
    v.insert("state".into(), json!(st("state")));
    v.insert(
        "error_count".into(),
        json!(sched
            .and_then(|d| d.get("error_count"))
            .and_then(Value::as_u64)
            .map(|n| n.to_string())
            .unwrap_or_default()),
    );
    v.insert(
        "last_error".into(),
        json!(sched
            .and_then(|d| d.pointer("/info/error"))
            .and_then(Value::as_str)
            .map(super::mask_url)
            .unwrap_or_default()),
    );
    v.insert("replication_id".into(), json!(st("id")));
    v.insert("node".into(), json!(st("node")));
    v.insert(
        "last_updated".into(),
        json!(jobs::utc(&st("last_updated")).unwrap_or_default()),
    );
}

pub(crate) async fn form(s: &CouchSession, c: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let mut f = ObjectForm::default();
    let Some(id) = id else {
        let dbs = replicator_dbs(s).await?;
        let mut choices: Vec<(String, String)> =
            dbs.iter().map(|d| (d.clone(), d.clone())).collect();
        if !dbs.iter().any(|d| d == DEFAULT_DB) {
            choices.insert(
                0,
                (DEFAULT_DB.into(), "_replicator (se crea al guardar)".into()),
            );
        }
        f.choices.insert("replicator_db".into(), choices);
        f.sections = sections(c, false);
        for fl in f.sections.iter().flat_map(|s| s.fields.iter()) {
            f.values
                .insert(fl.key.clone(), fl.default.clone().unwrap_or(Value::Null));
        }
        f.notes.push(SECRET_NOTE.into());
        return Ok(f);
    };
    match jobs::parse_id(id)? {
        JobId::Doc { db, doc } => {
            let d = read_doc(s, &db, &doc, id).await?;
            let sched = s.scheduler_doc(&db, &doc).await.ok();
            let mut values = doc_values(&db, &doc, &d);
            status_values(sched.as_ref(), &mut values);
            f.values = values.into_iter().collect();
            f.choices
                .insert("replicator_db".into(), vec![(db.clone(), db.clone())]);
            f.sections = sections(c, true);
            f.notes.push(EDIT_NOTE.into());
            f.notes.push(SECRET_NOTE.into());
        }
        JobId::Transient(rep) => {
            let j = s.transient(&rep).await?;
            let mut values = Map::new();
            for side in [SOURCE, TARGET] {
                let url = field(&j, side.key).unwrap_or_default();
                values.insert(side.f("url"), json!(split_userinfo(&url).0));
                values.insert(side.f("auth"), json!("none"));
                values.insert(side.f("user"), Value::Null);
                values.insert(side.f("password"), Value::Null);
                values.insert(side.f("token"), Value::Null);
                values.insert(side.f("headers"), json!(""));
            }
            values.insert("doc_id".into(), json!(""));
            values.insert("database".into(), json!(""));
            values.insert("continuous".into(), json!(rep.contains("+continuous")));
            values.insert(
                "create_target".into(),
                json!(rep.contains("+create_target")),
            );
            values.insert("filter_type".into(), json!("none"));
            values.insert("owner".into(), json!(field(&j, "user").unwrap_or_default()));
            values.insert(
                "state".into(),
                json!(if j.get("pid").is_some_and(|p| !p.is_null()) {
                    "running"
                } else {
                    ""
                }),
            );
            values.insert("replication_id".into(), json!(rep));
            values.insert("node".into(), json!(field(&j, "node").unwrap_or_default()));
            f.values = values.into_iter().collect();
            f.sections = sections(c, true);
            f.read_only = true;
            f.notes.push(TRANSIENT_READ_ONLY.into());
            f.notes.push(jobs::TRANSIENT_NOTE.into());
        }
    }
    Ok(f)
}

// ---------------------------------------------------------------------------
// Changes
// ---------------------------------------------------------------------------

pub(crate) fn doc_path(db: &str, doc: &str) -> String {
    format!("/{}/{}", seg(db), seg(doc))
}

/// Creating a replication: its database first when it's missing, then the
/// document (`POST`ed when it has no id).
pub(crate) fn create_change(
    db: &str,
    doc_id: Option<&str>,
    body: Map<String, Value>,
    db_exists: bool,
) -> Change {
    let mut ch = Change::default();
    if !db_exists {
        ch.comments
            .push(format!("La base «{db}» no existe: se crea primero."));
        ch.calls
            .push(Call::new(Method::PUT, format!("/{}", seg(db)), None));
    }
    match doc_id {
        Some(id) => {
            ch.comments
                .insert(0, format!("Crea la replicación «{id}» en «{db}»."));
            ch.calls.push(Call::new(
                Method::PUT,
                doc_path(db, id),
                Some(Value::Object(body)),
            ));
            ch.id = Some(
                JobId::Doc {
                    db: db.into(),
                    doc: id.into(),
                }
                .text(),
            );
        }
        None => {
            ch.comments.insert(
                0,
                format!("Crea una replicación en «{db}»; CouchDB elige el id del documento."),
            );
            ch.calls.push(Call::new(
                Method::POST,
                format!("/{}", seg(db)),
                Some(Value::Object(body)),
            ));
            ch.id_from_reply = Some(db.into());
        }
    }
    ch
}

pub(crate) fn update_change(
    db: &str,
    doc_id: &str,
    rev: &str,
    old: &Map<String, Value>,
    new: Map<String, Value>,
) -> Change {
    let id = JobId::Doc {
        db: db.into(),
        doc: doc_id.into(),
    }
    .text();
    let what = changed(&definition(old), &new);
    if what.is_empty() {
        return Change::nothing(doc_id, id);
    }
    let mut body = Map::new();
    body.insert("_rev".into(), json!(rev));
    body.extend(new);
    Change {
        comments: vec![
            format!("Cambia la replicación «{doc_id}»: {}.", what.join(", ")),
            EDIT_NOTE.into(),
        ],
        calls: vec![Call::new(
            Method::PUT,
            doc_path(db, doc_id),
            Some(Value::Object(body)),
        )],
        id: Some(id),
        id_from_reply: None,
    }
}

pub(crate) async fn change(
    s: &CouchSession,
    c: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Change> {
    let v = Vals(&values.values);
    match id {
        None => {
            let db = v.opt("database").unwrap_or_else(|| DEFAULT_DB.into());
            if !replicator_db(&db) {
                return Err(bad(format!(
                    "«{}» no es una base de replicación (_replicator, u otra terminada en /_replicator).",
                    shown(&db)
                )));
            }
            let doc_id = v.opt("doc_id");
            if let Some(d) = &doc_id {
                if !replication_doc(d) {
                    return Err(bad(format!(
                        "«{}» no puede ser el id de una replicación (no empieza con _ ni tiene caracteres de control).",
                        shown(d)
                    )));
                }
            }
            let exists = replicator_dbs(s).await?.contains(&db);
            if let (Some(d), true) = (&doc_id, exists) {
                match s.replication_doc(&db, d).await {
                    Ok(_) => return Err(bad(format!("Ya hay una replicación «{d}» en «{db}»."))),
                    Err(e) if jobs::missing(&e) => {}
                    Err(e) => return Err(e),
                }
            }
            let body = build_doc(c, values, None)?;
            Ok(create_change(&db, doc_id.as_deref(), body, exists))
        }
        Some(id) => match jobs::parse_id(id)? {
            JobId::Transient(_) => Err(bad(TRANSIENT_READ_ONLY.into())),
            JobId::Doc { db, doc } => {
                let old = read_doc(s, &db, &doc, id).await?;
                let rev = old
                    .get("_rev")
                    .and_then(Value::as_str)
                    .ok_or_else(|| jobs::not_found(id))?
                    .to_string();
                if v.opt("doc_id").is_some_and(|d| d != doc) {
                    return Err(bad(
                        "El id de un documento no se cambia: creá otra replicación y borrá esta."
                            .into(),
                    ));
                }
                let new = build_doc(c, values, Some(&old))?;
                Ok(update_change(&db, &doc, &rev, &old, new))
            }
        },
    }
}

pub(crate) fn delete_doc_change(db: &str, doc: &str, rev: &str) -> Change {
    Change {
        comments: vec![format!(
            "Borra la replicación «{doc}» (su documento en «{db}»): CouchDB la detiene."
        )],
        calls: vec![Call::new(
            Method::DELETE,
            format!("{}?rev={}", doc_path(db, doc), seg(rev)),
            None,
        )],
        id: None,
        id_from_reply: None,
    }
}

pub(crate) fn cancel_change(rep: &str) -> Change {
    Change {
        comments: vec!["Cancela la replicación transitoria: no tiene documento que borrar.".into()],
        calls: vec![Call::new(
            Method::POST,
            "/_replicate".into(),
            Some(json!({"replication_id": rep, "cancel": true})),
        )],
        id: None,
        id_from_reply: None,
    }
}

pub(crate) async fn delete(s: &CouchSession, id: &str) -> Result<Change> {
    match jobs::parse_id(id)? {
        JobId::Doc { db, doc } => {
            let d = read_doc(s, &db, &doc, id).await?;
            let rev = d
                .get("_rev")
                .and_then(Value::as_str)
                .ok_or_else(|| jobs::not_found(id))?;
            Ok(delete_doc_change(&db, &doc, rev))
        }
        JobId::Transient(rep) => {
            s.transient(&rep).await?;
            Ok(cancel_change(&rep))
        }
    }
}

/// "Start at step": a replication has one step, the replication itself,
/// and runs again whole with the job's "run now" (a new revision of its
/// document), so its job kind doesn't offer starting at a step
/// (`run_from_step` false) and this refuses it.
pub(crate) fn run_from(id: &str) -> Result<()> {
    jobs::parse_id(id)?;
    Err(Error::Unsupported(ONE_STEP.into()))
}
