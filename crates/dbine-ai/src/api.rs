//! The AI services used with the user's own API key (docs/ai-assistant.md,
//! "Your own API key"): OpenAI, Anthropic, Azure OpenAI, Google Gemini, xAI
//! Grok and any OpenAI-compatible server. The provider bills the user's
//! account; DBine has no service of its own in between.
//!
//! - Every one but Anthropic speaks OpenAI's chat completions
//!   (`{base}/chat/completions`, `{base}/models`), with the key as a Bearer
//!   token, or in Azure's `api-key` header. Anthropic: its Messages API
//!   ([`crate::anthropic`]).
//! - The key comes from the app's encrypted vault into [`ApiStore`] and goes
//!   only in the request's header: never in errors (a refused key gets
//!   DBine's own words, and the server's text is searched for it), logs or
//!   `Debug` output.

use crate::detect::{ModelInfo, ProviderInfo};
use crate::{AiError, ChatRequest, OnDelta, ProviderKind, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// How to reach a provider: its base URL, the key and the model typed by hand.
#[derive(Clone, Default, PartialEq)]
pub struct ApiAccess {
    pub base: String,
    pub key: Option<String>,
    pub model: Option<String>,
}

impl std::fmt::Debug for ApiAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiAccess")
            .field("base", &self.base)
            .field("key", &self.key.as_ref().map(|_| "<hidden>"))
            .field("model", &self.model)
            .finish()
    }
}

/// The configured API-key providers, shared with every chat: the app
/// replaces them when it starts the assistant and when the user saves or
/// removes a key.
#[derive(Clone, Default)]
pub struct ApiStore(Arc<RwLock<HashMap<ProviderKind, ApiAccess>>>);

impl std::fmt::Debug for ApiStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kinds: Vec<ProviderKind> = self
            .0
            .read()
            .map(|m| m.keys().copied().collect())
            .unwrap_or_default();
        f.debug_tuple("ApiStore").field(&kinds).finish()
    }
}

impl ApiStore {
    pub fn replace(&self, all: HashMap<ProviderKind, ApiAccess>) {
        *self.0.write().unwrap_or_else(|p| p.into_inner()) = all;
    }

    pub fn get(&self, kind: ProviderKind) -> Option<ApiAccess> {
        self.0
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(&kind)
            .cloned()
    }
}

/// Where each provider is, before the user says otherwise. Azure and a
/// custom server: the user's URL.
pub fn default_base(kind: ProviderKind) -> Option<&'static str> {
    match kind {
        ProviderKind::OpenAi => Some("https://api.openai.com/v1"),
        ProviderKind::Anthropic => Some("https://api.anthropic.com"),
        ProviderKind::Gemini => Some("https://generativelanguage.googleapis.com/v1beta/openai"),
        ProviderKind::Xai => Some("https://api.x.ai/v1"),
        _ => None,
    }
}

/// The base to call: the user's URL, else the provider's. Azure: the
/// resource's origin plus its OpenAI v1 path, whatever was pasted (the
/// resource's endpoint, or an older `/openai/deployments/…` address).
/// `None`: Azure or a custom server without a URL.
pub fn base_url(kind: ProviderKind, configured: Option<&str>) -> Option<String> {
    let Some(given) = configured.map(str::trim).filter(|u| !u.is_empty()) else {
        return default_base(kind).map(str::to_string);
    };
    let url = if given.contains("://") {
        given.to_string()
    } else {
        format!("https://{given}")
    };
    let url = url.trim_end_matches('/').to_string();
    if kind != ProviderKind::AzureOpenAi {
        return Some(url);
    }
    const V1: &str = "/openai/v1";
    if let Some(i) = url.find(V1) {
        return Some(url[..i + V1.len()].to_string());
    }
    let host_start = url.find("://").map_or(0, |i| i + 3);
    let origin = match url[host_start..].find('/') {
        Some(j) => &url[..host_start + j],
        None => &url,
    };
    Some(format!("{origin}{V1}"))
}

/// Who bills the user, for the UI's status line.
pub fn vendor(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenAi => "OpenAI",
        ProviderKind::Anthropic => "Anthropic",
        ProviderKind::AzureOpenAi => "Microsoft Azure",
        ProviderKind::Gemini => "Google",
        ProviderKind::Xai => "xAI",
        _ => "",
    }
}

pub fn not_configured(kind: ProviderKind) -> AiError {
    AiError::NotAvailable(format!(
        "configurá tu clave de API de {} en el asistente",
        kind.label()
    ))
}

pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

/// An OpenAI-compatible provider's key: a Bearer token, or Azure's `api-key`.
fn authorize(
    kind: ProviderKind,
    rb: reqwest::RequestBuilder,
    key: Option<&str>,
) -> reqwest::RequestBuilder {
    match (kind, key) {
        (_, None) => rb,
        (ProviderKind::AzureOpenAi, Some(k)) => rb.header("api-key", k),
        (_, Some(k)) => rb.bearer_auth(k),
    }
}

/// `text` with the key, if it's in it, hidden.
pub(crate) fn redact(text: &str, key: Option<&str>) -> String {
    match key.filter(|k| k.len() >= 4) {
        Some(k) => text.replace(k, "[clave oculta]"),
        None => text.to_string(),
    }
}

fn redact_error(e: AiError, key: Option<&str>) -> AiError {
    match e {
        AiError::NotAvailable(m) => AiError::NotAvailable(redact(&m, key)),
        AiError::Provider(m) => AiError::Provider(redact(&m, key)),
        other => other,
    }
}

/// The server's refusal, without the key: a refused key gets DBine's own
/// words (some servers quote part of it back), the rest the server's
/// message with any copy of the key hidden.
pub(crate) fn failure(kind: ProviderKind, status: u16, body: &str, key: Option<&str>) -> AiError {
    if status == 401 || status == 403 {
        return AiError::Provider(format!(
            "{}: la clave de API no es válida o no tiene permiso para esto (HTTP {status})",
            kind.label()
        ));
    }
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    let msg = v
        .pointer("/error/message")
        .or_else(|| v.get("error"))
        .or_else(|| v.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| body.chars().take(300).collect());
    AiError::Provider(format!(
        "{}: {} (HTTP {status})",
        kind.label(),
        redact(msg.trim(), key)
    ))
}

pub(crate) fn unreachable(
    kind: ProviderKind,
    base: &str,
    e: &reqwest::Error,
    key: Option<&str>,
) -> AiError {
    AiError::NotAvailable(redact(
        &format!("{} no responde en {base} ({e})", kind.label()),
        key,
    ))
}

/// Model ids that can't chat: embeddings, speech, images, moderation and
/// the old completion-only ones.
fn chat_model(id: &str) -> bool {
    let id = id.to_ascii_lowercase();
    ![
        "embed",
        "tts",
        "whisper",
        "dall-e",
        "moderation",
        "transcribe",
        "image",
        "audio",
        "realtime",
        "babbage",
        "davinci",
        "aqa",
    ]
    .iter()
    .any(|w| id.contains(w))
}

/// An OpenAI-style model list: the chat models, newest first (`created`),
/// without Gemini's `models/` prefix, once each.
pub(crate) fn parse_models(body: &str) -> Vec<ModelInfo> {
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    let mut items: Vec<(i64, String)> = v
        .get("data")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let id = m.get("id")?.as_str()?;
                    let id = id.strip_prefix("models/").unwrap_or(id);
                    chat_model(id).then(|| {
                        (
                            m.get("created").and_then(Value::as_i64).unwrap_or(0),
                            id.to_string(),
                        )
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    items.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|(_, id)| seen.insert(id.clone()))
        .map(|(_, id)| ModelInfo { id, detail: None })
        .collect()
}

/// The provider's chat models.
pub async fn models(kind: ProviderKind, access: &ApiAccess) -> Result<Vec<ModelInfo>> {
    if kind == ProviderKind::Anthropic {
        return crate::anthropic::models(access).await;
    }
    let key = access.key.as_deref();
    let resp = authorize(kind, client().get(format!("{}/models", access.base)), key)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| unreachable(kind, &access.base, &e, key))?;
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        return Err(failure(kind, status, &body, key));
    }
    Ok(parse_models(&body))
}

/// The model to ask: the one chosen in the chat, else the one typed in the
/// settings, else (Anthropic) its default.
fn model_for(kind: ProviderKind, access: &ApiAccess, req: &ChatRequest) -> Result<String> {
    req.model
        .clone()
        .filter(|m| !m.is_empty())
        .or_else(|| access.model.clone().filter(|m| !m.is_empty()))
        .or_else(|| {
            (kind == ProviderKind::Anthropic).then(|| crate::anthropic::DEFAULT_MODEL.to_string())
        })
        .ok_or_else(|| AiError::NotAvailable(format!("elegí un modelo de {}", kind.label())))
}

/// A chat with the user's key, streamed to `on_delta`.
pub async fn chat(
    kind: ProviderKind,
    access: &ApiAccess,
    req: &ChatRequest,
    on_delta: OnDelta<'_>,
) -> Result<String> {
    if access.base.is_empty() {
        return Err(AiError::NotAvailable(format!(
            "escribí la URL de {} en el asistente",
            kind.label()
        )));
    }
    let model = model_for(kind, access, req)?;
    if kind == ProviderKind::Anthropic {
        return crate::anthropic::chat(access, &model, req, on_delta).await;
    }
    let key = access.key.as_deref();
    let body = json!({ "model": model, "messages": crate::openai::messages(req), "stream": true });
    let resp = authorize(
        kind,
        client().post(format!("{}/chat/completions", access.base)),
        key,
    )
    .json(&body)
    .send()
    .await
    .map_err(|e| unreachable(kind, &access.base, &e, key))?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let text = resp.text().await.unwrap_or_default();
        return Err(failure(kind, status, &text, key));
    }
    crate::openai::relay(resp, kind.label(), on_delta)
        .await
        .map_err(|e| redact_error(e, key))
}

/// "Probar": the key opens the model list; an Azure deployment typed by
/// hand (Azure lists models, not deployments) answers a one-word question
/// when the list can't be read.
pub async fn test(kind: ProviderKind, access: &ApiAccess) -> Result<String> {
    match models(kind, access).await {
        Ok(list) => Ok(format!(
            "La clave funciona: hay {} modelo(s) disponibles",
            list.len()
        )),
        Err(e) => {
            let Some(model) = access
                .model
                .clone()
                .filter(|_| kind == ProviderKind::AzureOpenAi)
            else {
                return Err(e);
            };
            let req = ChatRequest {
                kind,
                model: Some(model.clone()),
                system: String::new(),
                messages: vec![crate::ChatMessage {
                    role: "user".into(),
                    content: "Reply with OK.".into(),
                }],
                lang: "es",
            };
            chat(kind, access, &req, &|_| {}).await?;
            Ok(format!(
                "La clave funciona: la implementación {model} responde"
            ))
        }
    }
}

/// An API-key provider for the UI: whether it's configured on this machine
/// and, when it is, its models.
pub async fn detect(kind: ProviderKind, store: &ApiStore) -> ProviderInfo {
    let mut p = ProviderInfo {
        kind,
        label: kind.label(),
        installed: false,
        available: false,
        local: false,
        status: String::new(),
        models: vec![],
        action: Some("configure_key"),
        path: None,
        key_saved: false,
        base_url: None,
        model: None,
    };
    let Some(access) = store.get(kind) else {
        p.status = "sin configurar: pegá tu clave de API".into();
        return p;
    };
    p.key_saved = access.key.is_some();
    p.base_url = Some(access.base.clone()).filter(|b| !b.is_empty());
    p.model = access.model.clone();
    if access.base.is_empty() {
        p.status = "falta la URL del servidor".into();
        return p;
    }
    if access.key.is_none() && kind != ProviderKind::OpenAiCompatible {
        p.status = "falta la clave de API en este equipo".into();
        return p;
    }
    p.installed = true;
    let typed = access.model.clone().filter(|m| !m.is_empty());
    let listed = tokio::time::timeout(Duration::from_secs(12), models(kind, &access))
        .await
        .unwrap_or_else(|_| {
            Err(AiError::NotAvailable(format!(
                "{} no respondió a tiempo",
                kind.label()
            )))
        });
    match listed {
        Ok(mut list) => {
            if let Some(m) = &typed {
                if !list.iter().any(|x| &x.id == m) {
                    list.insert(
                        0,
                        ModelInfo {
                            id: m.clone(),
                            detail: Some("Modelo escrito a mano".into()),
                        },
                    );
                }
            }
            if list.is_empty() {
                p.status = "la clave funciona, pero no hay modelos: escribí uno".into();
            } else {
                p.available = true;
                p.action = None;
                p.status = match vendor(kind) {
                    "" => "configurado · las preguntas van a ese servidor".into(),
                    v => format!("clave guardada · lo cobra tu cuenta de {v}"),
                };
            }
            p.models = list;
        }
        Err(e) => match typed {
            Some(m) => {
                p.available = true;
                p.action = None;
                p.models = vec![ModelInfo {
                    id: m.clone(),
                    detail: Some("Modelo escrito a mano".into()),
                }];
                p.status = format!("no se pudo leer la lista de modelos ({e}); usa {m}");
            }
            None => p.status = e.to_string(),
        },
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_server::serve;
    use crate::{ChatMessage, Delta};

    pub(crate) fn req(model: Option<&str>) -> ChatRequest {
        ChatRequest {
            kind: ProviderKind::OpenAi,
            model: model.map(str::to_string),
            system: "You write SQL.".into(),
            messages: vec![ChatMessage {
                role: "user".into(),
                content: "count the customers".into(),
            }],
            lang: "en",
        }
    }

    const KEY: &str = "sk-test-0123456789abcdef";

    fn sse_answer() -> String {
        [
            r#"data: {"choices":[{"delta":{"content":"SELECT "}}]}"#,
            r#"data: {"choices":[{"delta":{"content":"count(*)"}}]}"#,
            "data: [DONE]",
            "",
        ]
        .join("\n\n")
    }

    #[test]
    fn bases_default_or_the_users_and_azure_gets_its_v1_path() {
        assert_eq!(
            base_url(ProviderKind::OpenAi, None).as_deref(),
            Some("https://api.openai.com/v1")
        );
        assert_eq!(
            base_url(ProviderKind::Xai, Some("  ")).as_deref(),
            Some("https://api.x.ai/v1")
        );
        assert_eq!(base_url(ProviderKind::AzureOpenAi, None), None);
        assert_eq!(base_url(ProviderKind::OpenAiCompatible, None), None);
        for given in [
            "https://acme.openai.azure.com",
            "https://acme.openai.azure.com/",
            "acme.openai.azure.com",
            "https://acme.openai.azure.com/openai/v1/",
            "https://acme.openai.azure.com/openai/deployments/gpt/chat/completions?api-version=2024-10-21",
        ] {
            assert_eq!(
                base_url(ProviderKind::AzureOpenAi, Some(given)).as_deref(),
                Some("https://acme.openai.azure.com/openai/v1"),
                "{given}"
            );
        }
        assert_eq!(
            base_url(
                ProviderKind::OpenAiCompatible,
                Some("http://10.0.0.5:8000/v1/")
            )
            .as_deref(),
            Some("http://10.0.0.5:8000/v1")
        );
    }

    #[test]
    fn the_key_never_shows_in_debug_output() {
        let a = ApiAccess {
            base: "https://api.openai.com/v1".into(),
            key: Some(KEY.into()),
            model: None,
        };
        let s = ApiStore::default();
        s.replace(HashMap::from([(ProviderKind::OpenAi, a.clone())]));
        let shown = format!("{a:?} {s:?}");
        assert!(
            !shown.contains(KEY) && shown.contains("<hidden>"),
            "{shown}"
        );
    }

    #[test]
    fn model_lists_keep_chat_models_newest_first() {
        let body = r#"{"data":[
            {"id":"gpt-4.1","created":10},{"id":"text-embedding-3-large","created":30},
            {"id":"models/gemini-2.5-pro","created":20},{"id":"whisper-1","created":5},
            {"id":"gpt-4.1","created":10},{"id":"o4-mini"}]}"#;
        let ids: Vec<String> = parse_models(body).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["gemini-2.5-pro", "gpt-4.1", "o4-mini"]);
    }

    /// Each OpenAI-compatible provider: its path, its auth header and the
    /// streamed answer.
    #[tokio::test]
    async fn openai_compatible_providers_send_their_auth_and_stream() {
        for kind in [
            ProviderKind::OpenAi,
            ProviderKind::Gemini,
            ProviderKind::Xai,
            ProviderKind::AzureOpenAi,
            ProviderKind::OpenAiCompatible,
        ] {
            let (base, seen) = serve(vec![(200, "text/event-stream", sse_answer())]).await;
            let access = ApiAccess {
                base: format!("{base}/v1"),
                key: Some(KEY.into()),
                model: None,
            };
            let got = std::sync::Mutex::new(String::new());
            let on = |d: Delta| {
                if let Delta::Text(t) = d {
                    got.lock().unwrap().push_str(&t);
                }
            };
            let out = chat(kind, &access, &req(Some("m1")), &on).await.unwrap();
            assert_eq!(out, "SELECT count(*)", "{kind:?}");
            assert_eq!(*got.lock().unwrap(), "SELECT count(*)");
            let r = &seen.lock().unwrap()[0];
            assert_eq!(
                (r.method.as_str(), r.path.as_str()),
                ("POST", "/v1/chat/completions")
            );
            if kind == ProviderKind::AzureOpenAi {
                assert_eq!(r.header("api-key"), Some(KEY));
                assert_eq!(r.header("authorization"), None);
            } else {
                assert_eq!(
                    r.header("authorization"),
                    Some(format!("Bearer {KEY}").as_str())
                );
                assert_eq!(r.header("api-key"), None);
            }
            let body: Value = serde_json::from_str(&r.body).unwrap();
            assert_eq!(body["model"], "m1");
            assert_eq!(body["stream"], true);
            assert_eq!(body["messages"][0]["role"], "system");
        }
    }

    #[tokio::test]
    async fn a_custom_server_without_a_key_sends_no_auth() {
        let (base, seen) = serve(vec![(200, "text/event-stream", sse_answer())]).await;
        let access = ApiAccess {
            base,
            key: None,
            model: Some("local-model".into()),
        };
        chat(ProviderKind::OpenAiCompatible, &access, &req(None), &|_| {})
            .await
            .unwrap();
        let r = &seen.lock().unwrap()[0];
        assert_eq!(r.header("authorization"), None);
        let body: Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(
            body["model"], "local-model",
            "the typed model when none is chosen"
        );
    }

    /// A refused key: DBine's words, never the key the server quoted back;
    /// other errors: the server's message with the key hidden.
    #[tokio::test]
    async fn errors_never_carry_the_key() {
        let quoted = format!(r#"{{"error":{{"message":"Incorrect API key provided: {KEY}."}}}}"#);
        let (base, _) = serve(vec![
            (401, "application/json", quoted.clone()),
            (429, "application/json", quoted.clone()),
            (401, "application/json", quoted),
        ])
        .await;
        let access = ApiAccess {
            base,
            key: Some(KEY.into()),
            model: Some("m".into()),
        };
        let e = chat(ProviderKind::OpenAi, &access, &req(None), &|_| {})
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("clave de API no es válida") && e.contains("401"),
            "{e}"
        );
        assert!(!e.contains(KEY), "{e}");
        let e = chat(ProviderKind::Xai, &access, &req(None), &|_| {})
            .await
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("429") && e.contains("[clave oculta]") && !e.contains(KEY),
            "{e}"
        );
        let e = models(ProviderKind::Gemini, &access)
            .await
            .unwrap_err()
            .to_string();
        assert!(!e.contains(KEY), "{e}");
    }

    #[tokio::test]
    async fn detect_lists_models_or_says_what_is_missing() {
        let store = ApiStore::default();
        let p = detect(ProviderKind::OpenAi, &store).await;
        assert!(!p.available && !p.key_saved);
        assert_eq!(p.action, Some("configure_key"));
        let (base, seen) = serve(vec![(
            200,
            "application/json",
            r#"{"data":[{"id":"gpt-4.1","created":2},{"id":"gpt-4.1-mini","created":1}]}"#.into(),
        )])
        .await;
        store.replace(HashMap::from([(
            ProviderKind::Xai,
            ApiAccess {
                base,
                key: Some(KEY.into()),
                model: Some("grok-code".into()),
            },
        )]));
        let p = detect(ProviderKind::Xai, &store).await;
        assert!(p.available && p.key_saved, "{p:?}");
        let ids: Vec<&str> = p.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            ["grok-code", "gpt-4.1", "gpt-4.1-mini"],
            "the typed one first"
        );
        assert!(p.status.contains("xAI"), "{}", p.status);
        assert_eq!(seen.lock().unwrap()[0].path, "/models");
        // Configured without a key: says so, nothing is called.
        store.replace(HashMap::from([(
            ProviderKind::OpenAi,
            ApiAccess {
                base: "https://api.openai.com/v1".into(),
                key: None,
                model: None,
            },
        )]));
        let p = detect(ProviderKind::OpenAi, &store).await;
        assert!(
            !p.available && p.status.contains("falta la clave"),
            "{}",
            p.status
        );
    }

    #[tokio::test]
    async fn an_azure_deployment_is_tested_by_asking_it() {
        let (base, seen) = serve(vec![
            (
                404,
                "application/json",
                r#"{"error":{"message":"not found"}}"#.into(),
            ),
            (200, "text/event-stream", sse_answer()),
        ])
        .await;
        let access = ApiAccess {
            base,
            key: Some(KEY.into()),
            model: Some("gpt-dep".into()),
        };
        let ok = test(ProviderKind::AzureOpenAi, &access).await.unwrap();
        assert!(ok.contains("gpt-dep"), "{ok}");
        let s = seen.lock().unwrap();
        assert_eq!(s[1].path, "/chat/completions");
        assert_eq!(s[1].header("api-key"), Some(KEY));
    }

    #[tokio::test]
    async fn without_a_model_the_chat_asks_for_one() {
        let access = ApiAccess {
            base: "http://127.0.0.1:9".into(),
            key: Some(KEY.into()),
            model: None,
        };
        let e = chat(ProviderKind::OpenAi, &access, &req(None), &|_| {})
            .await
            .unwrap_err();
        assert!(e.to_string().contains("elegí un modelo"), "{e}");
    }
}
