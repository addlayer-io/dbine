//! "Enviar un mensaje" (docs/scheduled-tasks.md): a short text to a chat
//! the user already has: a Teams or Slack channel (its incoming webhook), a
//! Telegram chat (a bot of the user's) or an ntfy topic.
//!
//! - **Secrets.** The webhook URL and the tokens live in the vault
//!   (`task-secret:<task>:<step>`, with the channel they were typed for),
//!   never in the task's JSON, the history, the logs or the approval's
//!   fingerprint. A saved secret is used only for that same channel.
//!   Errors never carry them: URLs and tokens are redacted.
//! - **Network.** HTTPS only, to the channel's own hosts (Microsoft's for
//!   Teams, `hooks.slack.com`, `api.telegram.org`, a public ntfy server).
//!   The name is resolved here and every address checked (no loopback,
//!   private network, link-local or cloud metadata service), the connection
//!   pinned to them, no proxy, no redirects, 20 s at most.
//! - **Text.** The task's variables, each value escaped for the channel
//!   (Slack's `<…>` links and mentions, Teams' Markdown links); at most
//!   3,500 characters.

use super::{step_secret_name, StepDone};
use crate::error::{CommandError, CommandResult};
use crate::notebook::remote::{ip_blocked, name_blocked};
use dbine_core::tasks::{expand, kinds, ScheduledTask, Step};
use reqwest::Url;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

type Vars = BTreeMap<String, String>;
/// What the vault keeps for a step: `url`, `token` and the `channel` they
/// were typed for.
type Kept = BTreeMap<String, String>;
/// A vault entry by name.
pub(crate) type Get<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Sending waits at most this long.
pub(crate) const SEND_LIMIT: Duration = Duration::from_secs(20);
/// Telegram's limit is 4096 characters; the others take more, but a chat
/// message past this is no longer a notice.
pub(crate) const MAX_CHARS: usize = 3500;
/// The keys of the config that are secret.
pub const SECRET_KEYS: &[&str] = &["url", "token"];
/// The text when the step has none: what each earlier step did.
pub const DEFAULT_TEXT: &str = "{results}";
/// Microsoft's domains for a Teams "Workflows" webhook (and the old
/// connectors' `webhook.office.com`).
const TEAMS_DOMAINS: &[&str] = &["logic.azure.com", "api.powerplatform.com", "webhook.office.com"];
const SLACK_HOST: &str = "hooks.slack.com";
const TELEGRAM_API: &str = "https://api.telegram.org";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Teams,
    Slack,
    Telegram,
    Ntfy,
}

impl Channel {
    pub fn name(self) -> &'static str {
        match self {
            Channel::Teams => "Teams",
            Channel::Slack => "Slack",
            Channel::Telegram => "Telegram",
            Channel::Ntfy => "ntfy",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Channel::Teams => "teams",
            Channel::Slack => "slack",
            Channel::Telegram => "telegram",
            Channel::Ntfy => "ntfy",
        }
    }

    /// The secrets this channel takes.
    fn uses(self, key: &str) -> bool {
        match self {
            Channel::Teams | Channel::Slack => key == "url",
            Channel::Telegram => key == "token",
            Channel::Ntfy => key == "url" || key == "token",
        }
    }
}

/// The step's config. Its "Solo si…" (`when`) is the runner's, as for any
/// step.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct MessageConfig {
    pub channel: Channel,
    /// Teams' or Slack's webhook, or the ntfy topic's URL. Secret: empty in
    /// the task, kept in the vault.
    #[serde(default)]
    pub url: String,
    /// Telegram's bot token, or an ntfy access token. Secret, as `url`.
    #[serde(default)]
    pub token: String,
    /// Telegram's chat id (`-100…`, or `@channel`).
    #[serde(default)]
    pub chat_id: String,
    #[serde(default)]
    pub text: String,
}

fn parse_config(config: &Value) -> CommandResult<MessageConfig> {
    serde_json::from_value(config.clone()).map_err(|e| CommandError::BadRequest(format!("la configuración del paso no es válida: {e}")))
}

fn channel_of(config: &Value) -> String {
    config.get("channel").and_then(Value::as_str).unwrap_or("").to_string()
}

// ---------------------------------------------------------------------------
// Secrets

/// What the vault keeps for `step`, if it was typed for its current channel.
fn kept(task_id: &str, step: &Step, get: Get) -> Kept {
    if task_id.is_empty() || step.id.is_empty() {
        return Kept::new();
    }
    let k: Kept = get(&step_secret_name(task_id, &step.id)).and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
    if k.get("channel").map(String::as_str) == Some(channel_of(&step.config).as_str()) {
        k
    } else {
        Kept::new()
    }
}

/// The step's config with its secrets: what was typed, else what the vault
/// keeps for this same channel.
pub(crate) fn resolved(task_id: &str, step: &Step, get: Get) -> CommandResult<MessageConfig> {
    let mut c = parse_config(&step.config)?;
    let k = kept(task_id, step, get);
    if c.url.trim().is_empty() {
        c.url = k.get("url").cloned().unwrap_or_default();
    }
    if c.token.trim().is_empty() {
        c.token = k.get("token").cloned().unwrap_or_default();
    }
    Ok(c)
}

/// A typed secret that can't work, said without repeating it.
fn check_secret(channel: Channel, key: &str, value: &str) -> Result<(), String> {
    match (channel, key) {
        (Channel::Teams | Channel::Slack | Channel::Ntfy, "url") => check_url(channel, value).map(|_| ()),
        (Channel::Telegram, "token") if !telegram_token_ok(value) => {
            Err("el token del bot de Telegram no tiene la forma del que da @BotFather («123456:ABC…»)".into())
        }
        (Channel::Ntfy, "token") if value.len() > 200 || value.chars().any(|c| c.is_whitespace() || c.is_control()) => {
            Err("el token de acceso de ntfy no es válido".into())
        }
        _ => Ok(()),
    }
}

/// Takes the webhook URL and the tokens out of each "Enviar un mensaje"
/// step: the task's JSON keeps them empty, the vault keeps them with the
/// channel they were typed for. Returns, per step, its vault entry and what
/// to keep there (`None`: delete it). An empty field with a secret already
/// kept for the same channel means "unchanged"; another channel's is
/// dropped. A typed value that can't work refuses the save.
pub(crate) fn secrets_apart(task: &mut ScheduledTask, get: Get) -> Result<Vec<(String, Option<String>)>, String> {
    let mut out = Vec::new();
    for step in task.steps.iter_mut().filter(|s| s.kind == kinds::SEND_MESSAGE) {
        let name = step_secret_name(&task.id, &step.id);
        let channel = channel_of(&step.config);
        let parsed: Option<Channel> = serde_json::from_value(Value::String(channel.clone())).ok();
        let mut kept: Kept = get(&name).and_then(|j| serde_json::from_str(&j).ok()).unwrap_or_default();
        if kept.get("channel") != Some(&channel) {
            kept.clear();
        }
        kept.remove("channel");
        let Some(config) = step.config.as_object_mut() else { continue };
        for key in SECRET_KEYS {
            let typed = config.get(*key).and_then(Value::as_str).unwrap_or("").trim().to_string();
            config.insert(key.to_string(), Value::String(String::new()));
            if typed.is_empty() {
                continue;
            }
            if let Some(ch) = parsed.filter(|ch| ch.uses(key)) {
                check_secret(ch, key, &typed)?;
            }
            kept.insert(key.to_string(), typed);
        }
        match parsed {
            Some(ch) => kept.retain(|k, _| ch.uses(k)),
            None => kept.clear(),
        }
        if kept.is_empty() {
            out.push((name, None));
        } else {
            kept.insert("channel".into(), channel);
            out.push((name, Some(serde_json::to_string(&kept).unwrap_or_default())));
        }
    }
    Ok(out)
}

/// Where `task`'s message steps send, without the secret part: the channel
/// and the host (Telegram: the bot's number and the chat). Part of the
/// approval's fingerprint of a task that writes, so a new destination asks
/// for the approval again.
pub(crate) fn destinations(task: &ScheduledTask, get: Get) -> Vec<String> {
    let mut out: Vec<String> = task.steps.iter().filter(|s| s.kind == kinds::SEND_MESSAGE).map(|s| destination(&task.id, s, get)).collect();
    out.sort();
    out
}

fn destination(task_id: &str, step: &Step, get: Get) -> String {
    let Ok(c) = resolved(task_id, step, get) else { return format!("message:{}", channel_of(&step.config)) };
    match c.channel {
        Channel::Telegram => {
            let bot = c.token.trim().split_once(':').map(|(id, _)| id).unwrap_or("");
            format!("telegram:{bot}:{}", c.chat_id.trim())
        }
        ch => {
            let host = Url::parse(c.url.trim())
                .ok()
                .and_then(|u| u.host_str().map(|h| match u.port() {
                    Some(p) => format!("{h}:{p}"),
                    None => h.to_string(),
                }))
                .unwrap_or_default();
            format!("{}:{}", ch.key(), host.to_ascii_lowercase())
        }
    }
}

// ---------------------------------------------------------------------------
// Checks

fn under(host: &str, domain: &str) -> bool {
    host == domain || host.strip_suffix(domain).is_some_and(|h| h.ends_with('.'))
}

/// The webhook or topic URL, if it is one this channel sends to: https, no
/// user or password, the channel's own host (ntfy: a public server).
pub(crate) fn check_url(channel: Channel, url: &str) -> Result<Url, String> {
    let what = match channel {
        Channel::Teams => "la URL del webhook de Teams",
        Channel::Slack => "la URL del webhook de Slack",
        Channel::Ntfy => "la URL del tema de ntfy",
        Channel::Telegram => return Err("Telegram no usa URL: va con el token del bot y el chat".into()),
    };
    let url = url.trim();
    if url.is_empty() {
        return Err(format!("falta {what}"));
    }
    let u = Url::parse(url).map_err(|_| format!("{what} no es una URL válida"))?;
    if u.scheme() != "https" {
        return Err(format!("{what} tiene que empezar con https://"));
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err(format!("{what} no puede llevar usuario ni contraseña"));
    }
    let host = u.host_str().unwrap_or("").trim_end_matches('.').to_ascii_lowercase();
    match channel {
        Channel::Teams if !(TEAMS_DOMAINS.iter().any(|d| under(&host, d)) && u.port().is_none()) => {
            Err("la URL del webhook de Teams tiene que ser de Microsoft (logic.azure.com, api.powerplatform.com o webhook.office.com)".into())
        }
        Channel::Slack if !(host == SLACK_HOST && u.port().is_none()) => Err("la URL del webhook de Slack tiene que ser de hooks.slack.com".into()),
        Channel::Ntfy if host.is_empty() || name_blocked(&host) => {
            Err("el servidor de ntfy tiene que ser público: no puede ser esta computadora, la red local ni un servicio interno".into())
        }
        Channel::Ntfy => ntfy_parts(&u).map(|_| u),
        _ => Ok(u),
    }
}

/// An ntfy topic URL as the server's base and the topic.
fn ntfy_parts(u: &Url) -> Result<(Url, String), String> {
    let bad = || "la URL de ntfy tiene que ser la del tema, por ejemplo https://ntfy.sh/mi-tema".to_string();
    let topic = u.path_segments().and_then(|mut s| s.next_back()).unwrap_or("").to_string();
    if topic.is_empty() || topic.len() > 64 || !topic.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') || u.query().is_some() {
        return Err(bad());
    }
    let mut base = u.clone();
    base.set_fragment(None);
    let path = u.path();
    base.set_path(&path[..path.len() - topic.len()]);
    Ok((base, topic))
}

fn telegram_token_ok(token: &str) -> bool {
    let Some((id, key)) = token.split_once(':') else { return false };
    !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) && !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn chat_ok(chat: &str) -> bool {
    let digits = chat.strip_prefix('-').unwrap_or(chat);
    (!digits.is_empty() && digits.len() <= 20 && digits.chars().all(|c| c.is_ascii_digit()))
        || chat.strip_prefix('@').is_some_and(|n| n.len() >= 4 && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

// ---------------------------------------------------------------------------
// The text

/// A variable's value as data for `channel`: it can't add links or
/// mentions. Slack: its three escapes (`<!here>`, `<@U…>` and `<url|text>`
/// stay text). Teams: a Markdown link (`[text](url)`) is broken. Both: an
/// `@` can't start a mention (`@channel`). Telegram and ntfy get plain text.
pub(crate) fn escape(channel: Channel, value: &str) -> String {
    match channel {
        Channel::Slack => value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('@', "@\u{2060}"),
        Channel::Teams => value.replace("](", "]\u{200B}(").replace('<', "<\u{200B}").replace('@', "@\u{2060}"),
        Channel::Telegram | Channel::Ntfy => value.to_string(),
    }
}

/// At most [`MAX_CHARS`], without leaving half of an escape at the end.
pub(crate) fn cut(text: &str) -> String {
    let t = text.trim();
    if t.chars().count() <= MAX_CHARS {
        return t.to_string();
    }
    let mut s: String = t.chars().take(MAX_CHARS).collect();
    if let Some(amp) = s.rfind('&') {
        if !s[amp..].contains(';') && s.len() - amp <= 5 {
            s.truncate(amp);
        }
    }
    s.push('…');
    s
}

/// The title and text the step sends: `template` (or [`DEFAULT_TEXT`])
/// with the variables' values escaped for the channel, cut.
pub(crate) fn compose(channel: Channel, task_name: &str, template: &str, vars: &Vars) -> (String, String) {
    let escaped: Vars = vars.iter().map(|(k, v)| (k.clone(), escape(channel, v))).collect();
    let template = if template.trim().is_empty() { DEFAULT_TEXT } else { template };
    let title = format!("DBine · {}", escape(channel, task_name.trim()));
    (title, cut(&expand(template, &escaped)))
}

// ---------------------------------------------------------------------------
// The request

/// What to post: always JSON, to a checked URL.
#[derive(Debug, PartialEq)]
pub(crate) struct Request {
    pub url: Url,
    pub body: Value,
    /// ntfy's access token.
    pub bearer: Option<String>,
    /// What an error must never show.
    secrets: Vec<String>,
}

/// The request each channel expects.
pub(crate) fn request(c: &MessageConfig, title: &str, text: &str) -> Result<Request, String> {
    let token = c.token.trim();
    let mut secrets = vec![c.url.trim().to_string(), token.to_string()];
    let (url, body, bearer) = match c.channel {
        Channel::Slack => (check_url(Channel::Slack, &c.url)?, json!({ "text": format!("*{title}*\n{text}") }), None),
        // A Teams "Workflows" webhook (the old Office 365 connectors are
        // gone): it takes an Adaptive Card.
        Channel::Teams => {
            let card = json!({
                "type": "message",
                "attachments": [{
                    "contentType": "application/vnd.microsoft.card.adaptive",
                    "content": {
                        "$schema": "http://adaptivecards.io/schemas/adaptive-card.json",
                        "type": "AdaptiveCard",
                        "version": "1.4",
                        "body": [
                            { "type": "TextBlock", "text": title, "weight": "Bolder", "wrap": true },
                            { "type": "TextBlock", "text": text, "wrap": true }
                        ]
                    }
                }]
            });
            (check_url(Channel::Teams, &c.url)?, card, None)
        }
        // Plain text (no parse mode): nothing in it is markup.
        Channel::Telegram => {
            if token.is_empty() {
                return Err("falta el token del bot de Telegram (el que da @BotFather, «123456:ABC…»)".into());
            }
            if !telegram_token_ok(token) {
                return Err("el token del bot de Telegram no tiene la forma del que da @BotFather («123456:ABC…»)".into());
            }
            let chat = c.chat_id.trim();
            if chat.is_empty() {
                return Err("falta el chat de Telegram al que mandar el mensaje".into());
            }
            if !chat_ok(chat) {
                return Err("el chat de Telegram tiene que ser un número (-100…) o un @canal".into());
            }
            if let Some((_, key)) = token.split_once(':') {
                secrets.push(key.to_string());
            }
            let url = Url::parse(&format!("{TELEGRAM_API}/bot{token}/sendMessage")).map_err(|_| "el token del bot de Telegram no es válido".to_string())?;
            (url, json!({ "chat_id": chat, "text": format!("{title}\n{text}"), "disable_web_page_preview": true }), None)
        }
        // Published as JSON to the server's base (UTF-8 title, no Markdown).
        Channel::Ntfy => {
            let (base, topic) = ntfy_parts(&check_url(Channel::Ntfy, &c.url)?)?;
            secrets.push(topic.clone());
            (base, json!({ "topic": topic, "title": title, "message": text }), (!token.is_empty()).then(|| token.to_string()))
        }
    };
    secrets.retain(|s| s.len() >= 4);
    Ok(Request { url, body, bearer, secrets })
}

/// `text` without the secrets and with every URL cut to its host.
pub(crate) fn redact(text: &str, secrets: &[String]) -> String {
    let mut s = text.to_string();
    for secret in secrets.iter().filter(|x| x.len() >= 4) {
        s = s.replace(secret.as_str(), "•••");
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(i) = ["https://", "http://"].iter().filter_map(|p| rest.find(p)).min() {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '»' | ')' | '>' | ',')).unwrap_or(tail.len());
        match Url::parse(&tail[..end]).ok().and_then(|u| u.host_str().map(|h| format!("{}://{h}/…", u.scheme()))) {
            Some(short) => out.push_str(&short),
            None => out.push_str("•••"),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// A client for `url`'s host only: its name resolved here and every address
/// checked ([`ip_blocked`]), the connection pinned to them (no second
/// lookup), no proxy, no redirects. `local_ok`: tests' mock servers.
async fn client_for(url: &Url, local_ok: bool) -> Result<reqwest::Client, String> {
    if url.scheme() != "https" && !local_ok {
        return Err("solo se envía por HTTPS".into());
    }
    let host = url.host_str().ok_or_else(|| "la URL no tiene servidor".to_string())?.to_string();
    let port = url.port_or_known_default().unwrap_or(443);
    let literal = host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().ok();
    let addrs: Vec<SocketAddr> = match literal {
        Some(ip) => vec![SocketAddr::new(ip, port)],
        None => {
            if !local_ok && name_blocked(&host) {
                return Err(format!("«{host}» es esta computadora, la red local o un servicio interno: los mensajes van solo a servidores públicos"));
            }
            tokio::net::lookup_host((host.as_str(), port)).await.map_err(|e| format!("no se pudo resolver «{host}»: {e}"))?.collect()
        }
    };
    if addrs.is_empty() {
        return Err(format!("no se encontró la dirección de «{host}»"));
    }
    if !local_ok {
        if let Some(a) = addrs.iter().find(|a| ip_blocked(a.ip())) {
            return Err(format!(
                "«{host}» apunta a {} (esta computadora, la red local o un servicio interno de la nube): los mensajes van solo a servidores públicos",
                a.ip()
            ));
        }
    }
    let mut b = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(SEND_LIMIT)
        .https_only(!local_ok)
        .user_agent(concat!("DBine/", env!("CARGO_PKG_VERSION")));
    if literal.is_none() {
        b = b.resolve_to_addrs(&host, &addrs);
    }
    b.build().map_err(|e| e.without_url().to_string())
}

/// What went wrong, with the causes (reqwest's top line alone says little).
fn describe(e: reqwest::Error) -> String {
    let e = e.without_url();
    if e.is_timeout() {
        return format!("el servicio no respondió en {} s", SEND_LIMIT.as_secs());
    }
    let mut why = e.to_string();
    let mut src = std::error::Error::source(&e);
    while let Some(s) = src {
        why.push_str(": ");
        why.push_str(&s.to_string());
        src = s.source();
    }
    why
}

/// The start of a response's body (the service's own words: Telegram's
/// "chat not found", Slack's "invalid_token"…).
async fn said(mut resp: reqwest::Response) -> String {
    let mut bytes = Vec::new();
    while let Ok(Some(chunk)) = resp.chunk().await {
        bytes.extend_from_slice(&chunk);
        if bytes.len() >= 1024 {
            break;
        }
    }
    String::from_utf8_lossy(&bytes).chars().take(200).collect::<String>().trim().to_string()
}

/// Send it. The error never carries the URL or the token.
pub(crate) async fn send(req: &Request) -> Result<(), String> {
    send_on(req, false).await
}

async fn send_on(req: &Request, local_ok: bool) -> Result<(), String> {
    let attempt = async {
        let client = client_for(&req.url, local_ok).await?;
        let mut b = client.post(req.url.clone()).json(&req.body);
        if let Some(t) = &req.bearer {
            b = b.bearer_auth(t);
        }
        let resp = b.send().await.map_err(describe)?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        if status.is_redirection() {
            return Err(format!("el servicio respondió con una redirección ({status}): DBine no sigue redirecciones, revisá la URL"));
        }
        let said = said(resp).await;
        Err(match status.as_u16() {
            401 | 403 => format!("el servicio rechazó el envío ({status}): revisá el token o la URL. {said}"),
            404 => format!("el servicio no encontró el destino ({status}): revisá la URL o el chat. {said}"),
            _ => format!("el servicio respondió {status}. {said}"),
        }
        .trim()
        .to_string())
    };
    let out = tokio::time::timeout(SEND_LIMIT, attempt).await.unwrap_or_else(|_| Err(format!("el servicio no respondió en {} s", SEND_LIMIT.as_secs())));
    out.map_err(|e| redact(&e, &req.secrets))
}

// ---------------------------------------------------------------------------
// The step and its test

/// The vault entry of a step, read once (an error is the keychain's).
fn vault_entry(task_id: &str, step: &Step) -> CommandResult<Option<String>> {
    if task_id.is_empty() || step.id.is_empty() {
        return Ok(None);
    }
    Ok(dbine_core::secrets::get_raw(&step_secret_name(task_id, &step.id))?)
}

async fn deliver(task_id: &str, task_name: &str, step: &Step, template: Option<&str>, vars: &Vars) -> CommandResult<Channel> {
    let raw = vault_entry(task_id, step)?;
    let c = resolved(task_id, step, &|_| raw.clone())?;
    let (title, text) = compose(c.channel, task_name, template.unwrap_or(&c.text), vars);
    if text.trim().is_empty() {
        return Err(CommandError::BadRequest("el mensaje quedó vacío".into()));
    }
    let req = request(&c, &title, &text).map_err(CommandError::BadRequest)?;
    send(&req).await.map_err(|e| CommandError::BadRequest(format!("no se pudo enviar el mensaje a {}: {e}", c.channel.name())))?;
    Ok(c.channel)
}

pub(super) async fn step(task: &ScheduledTask, step: &Step, vars: &Vars) -> CommandResult<StepDone> {
    let channel = deliver(&task.id, &task.name, step, None, vars).await?;
    Ok(StepDone { summary: format!("Mensaje enviado a {}", channel.name()), ..Default::default() })
}

/// "Probar": sends `text` (with the task's date variables) through the step
/// as it is in the editor: the secrets typed there, or the saved ones for
/// the same channel.
pub async fn test(task_id: &str, task_name: &str, step: &Step, text: &str) -> CommandResult<String> {
    let probe = ScheduledTask { name: task_name.to_string(), ..Default::default() };
    let vars = dbine_core::tasks::base_vars(&probe, chrono::Local::now().naive_local());
    let channel = deliver(task_id, task_name, step, Some(text), &vars).await?;
    Ok(format!("Mensaje de prueba enviado a {}", channel.name()))
}

#[cfg(test)]
#[path = "message_tests.rs"]
mod tests;
