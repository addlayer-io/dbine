//! Sending mail (docs/tareas-programadas.md): the SMTP server of
//! Configuración › Correo and the "Enviar un mail" step.
//!
//! The server lives in this machine's state (`local.mail`): the tasks that
//! use it run here, so it doesn't sync. Its password is in the vault, never
//! in the state or the logs; neither is a message's body. Summaries carry
//! only the recipients and the attachments' names.

use super::StepDone;
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::tasks::{expand, Step};
use lettre::message::header::ContentType;
use lettre::message::{Attachment, Mailbox, MultiPart, SinglePart};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

type Vars = BTreeMap<String, String>;

/// The state setting with the server.
pub const SETTING: &str = "local.mail";
/// The vault entry with the server's password.
pub const PASSWORD: &str = "smtp-password";
/// Attachments of one mail, in total.
const MAX_ATTACHMENTS: u64 = 20 * 1024 * 1024;
/// Each SMTP command waits at most this long…
const COMMAND_LIMIT: Duration = Duration::from_secs(30);
/// …and the whole send this long.
const SEND_LIMIT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Security {
    /// Plain connection upgraded with STARTTLS (port 587).
    #[default]
    Starttls,
    /// TLS from the start (port 465).
    Tls,
    /// No encryption: the password travels in clear.
    None,
}

/// The SMTP server (without its password).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MailSettings {
    pub host: String,
    #[serde(default = "submission_port")]
    pub port: u16,
    #[serde(default)]
    pub security: Security,
    /// Empty: the server takes mail without logging in.
    #[serde(default)]
    pub user: String,
    pub from_address: String,
    #[serde(default)]
    pub from_name: String,
}

fn submission_port() -> u16 {
    587
}

impl MailSettings {
    /// What's wrong with it, in Spanish (`None`: it can be saved).
    pub fn problem(&self) -> Option<String> {
        if self.host.trim().is_empty() {
            return Some("falta el servidor de correo".into());
        }
        if self.port == 0 {
            return Some("falta el puerto del servidor de correo".into());
        }
        self.sender().err()
    }

    fn sender(&self) -> Result<Mailbox, String> {
        let address = self.from_address.trim();
        let address = address.parse().map_err(|_| format!("«{address}» no es una dirección de remitente válida"))?;
        let name = self.from_name.trim();
        Ok(Mailbox::new((!name.is_empty()).then(|| name.to_string()), address))
    }

    fn at(&self) -> String {
        format!("{}:{}", self.host.trim(), self.port)
    }
}

/// The saved server, if there's one.
pub fn load(state: &AppState) -> CommandResult<Option<MailSettings>> {
    Ok(state.store.get_setting(SETTING)?.and_then(|v| serde_json::from_value(v).ok()))
}

pub fn password() -> CommandResult<Option<String>> {
    Ok(dbine_core::secrets::get_raw(PASSWORD)?)
}

/// A mail to send.
#[derive(Debug, Default)]
pub struct Outgoing {
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub attachments: Vec<PathBuf>,
}

/// The addresses in `text`, separated by commas, semicolons or lines.
pub fn addresses(text: &str) -> Vec<String> {
    text.split([',', ';', '\n']).map(str::trim).filter(|a| !a.is_empty()).map(str::to_string).collect()
}

fn mailboxes(list: &[String]) -> Result<Vec<Mailbox>, String> {
    list.iter().map(|a| a.parse::<Mailbox>().map_err(|_| format!("«{a}» no es una dirección de correo válida"))).collect()
}

fn content_type(path: &Path) -> ContentType {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let mime = match ext.as_str() {
        "csv" => "text/csv",
        "tsv" => "text/tab-separated-values",
        "txt" | "log" | "sql" | "cql" | "js" | "jsonl" => "text/plain",
        "json" => "application/json",
        "xml" => "application/xml",
        "html" | "htm" => "text/html",
        "md" => "text/markdown",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        _ => "application/octet-stream",
    };
    ContentType::parse(mime).unwrap_or(ContentType::TEXT_PLAIN)
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// The message, ready to send. Fails on a bad address, a missing
/// attachment or attachments over the limit.
pub fn build(settings: &MailSettings, mail: &Outgoing) -> Result<Message, String> {
    if mail.to.is_empty() {
        return Err("el mail no tiene destinatarios".into());
    }
    let mut b = Message::builder().from(settings.sender()?).subject(mail.subject.trim());
    for m in mailboxes(&mail.to)? {
        b = b.to(m);
    }
    for m in mailboxes(&mail.cc)? {
        b = b.cc(m);
    }
    let mut total = 0u64;
    for p in &mail.attachments {
        let meta = std::fs::metadata(p).map_err(|_| format!("no existe el archivo adjunto {}", p.display()))?;
        if !meta.is_file() {
            return Err(format!("el adjunto {} no es un archivo", p.display()));
        }
        total += meta.len();
    }
    if total > MAX_ATTACHMENTS {
        return Err(format!(
            "los adjuntos suman {:.1} MB y el máximo es {} MB: la mayoría de los servidores rechazan mails más grandes",
            total as f64 / 1048576.0,
            MAX_ATTACHMENTS / 1048576
        ));
    }
    let message = if mail.attachments.is_empty() {
        b.header(ContentType::TEXT_PLAIN).body(mail.body.clone())
    } else {
        let mut parts = MultiPart::mixed().singlepart(SinglePart::plain(mail.body.clone()));
        for p in &mail.attachments {
            let bytes = std::fs::read(p).map_err(|e| format!("no se pudo leer el adjunto {}: {e}", p.display()))?;
            parts = parts.singlepart(Attachment::new(file_name(p)).body(bytes, content_type(p)));
        }
        b.multipart(parts)
    };
    message.map_err(|e| format!("no se pudo armar el mail: {e}"))
}

/// Send `message` through the server; the error says what went wrong in
/// words the user can act on.
pub async fn send(settings: &MailSettings, password: Option<String>, message: Message) -> Result<(), String> {
    let host = settings.host.trim();
    let builder = match settings.security {
        Security::Tls => AsyncSmtpTransport::<Tokio1Executor>::relay(host),
        Security::Starttls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host),
        Security::None => Ok(AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(host)),
    }
    .map_err(|e| explain(&e, settings))?;
    let mut builder = builder.port(settings.port).timeout(Some(COMMAND_LIMIT));
    if !settings.user.trim().is_empty() {
        builder = builder.credentials(Credentials::new(settings.user.trim().to_string(), password.unwrap_or_default()));
    }
    let transport = builder.build();
    match tokio::time::timeout(SEND_LIMIT, transport.send(message)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(explain(&e, settings)),
        Err(_) => Err(format!("el servidor de correo {} no terminó de recibir el mail a tiempo", settings.at())),
    }
}

/// The first I/O error behind `e`.
fn io_kind(e: &(dyn std::error::Error + 'static)) -> Option<std::io::ErrorKind> {
    let mut cur = Some(e);
    while let Some(err) = cur {
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            return Some(io.kind());
        }
        cur = err.source();
    }
    None
}

fn explain(e: &lettre::transport::smtp::Error, s: &MailSettings) -> String {
    use std::io::ErrorKind;
    let at = s.at();
    let code = e.status().map(|c| c.to_string()).unwrap_or_default();
    let io = io_kind(e);
    if e.is_timeout() || io == Some(ErrorKind::TimedOut) {
        return format!("el servidor de correo {at} no respondió a tiempo: revisá el servidor, el puerto y que un firewall no bloquee la conexión");
    }
    if io == Some(ErrorKind::ConnectionRefused) {
        return format!("el servidor de correo {at} rechazó la conexión: revisá el puerto (587 con STARTTLS, 465 con SSL/TLS, 25 sin cifrado)");
    }
    // rustls reports a failed handshake (a plain port, a bad certificate)
    // as invalid data under a connection error.
    let handshake = s.security != Security::None && io == Some(ErrorKind::InvalidData);
    if e.is_tls() || handshake || e.to_string().contains("STARTTLS") {
        return format!(
            "falló la conexión segura con {at} ({e}): revisá que la seguridad elegida corresponda al puerto (STARTTLS con 587, SSL/TLS con 465)"
        );
    }
    if matches!(code.as_str(), "530" | "534" | "535" | "538" | "454") {
        return format!("el servidor de correo rechazó el usuario o la contraseña ({e})");
    }
    if e.is_permanent() || e.is_transient() {
        return format!("el servidor de correo rechazó el mail: {e}");
    }
    if io.is_some() {
        return format!("no se pudo conectar con el servidor de correo {at}: {e}");
    }
    format!("error del servidor de correo {at}: {e}")
}

/// Build and send `mail` with the saved server. The answer is the summary:
/// recipients and attachments' names, never the body.
pub async fn deliver(settings: &MailSettings, password: Option<String>, mail: &Outgoing) -> Result<String, String> {
    let message = build(settings, mail)?;
    send(settings, password, message).await?;
    let mut summary = format!("Mail enviado a {}", mail.to.join(", "));
    if !mail.cc.is_empty() {
        summary.push_str(&format!(" (cc: {})", mail.cc.join(", ")));
    }
    if !mail.attachments.is_empty() {
        let names: Vec<String> = mail.attachments.iter().map(|p| file_name(p)).collect();
        summary.push_str(&format!("; adjuntos: {}", names.join(", ")));
    }
    summary.push('.');
    Ok(summary)
}

// -- the step ----------------------------------------------------------------

/// Addresses as a text ("a@x, b@y") or a list.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Recipients {
    Text(String),
    List(Vec<String>),
}

impl Default for Recipients {
    fn default() -> Self {
        Recipients::Text(String::new())
    }
}

impl Recipients {
    fn expanded(&self, vars: &Vars) -> Vec<String> {
        match self {
            Recipients::Text(t) => addresses(&expand(t, vars)),
            Recipients::List(l) => l.iter().flat_map(|a| addresses(&expand(a, vars))).collect(),
        }
    }
}

/// The step's config. Its "Solo si…" (`when`) is the runner's, as for any
/// step.
#[derive(Debug, Deserialize)]
struct MailConfig {
    #[serde(default)]
    to: Recipients,
    #[serde(default)]
    cc: Recipients,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    body: String,
    /// Paths; empty lines are ignored.
    #[serde(default)]
    attachments: Vec<String>,
}

fn outgoing(c: &MailConfig, vars: &Vars) -> Outgoing {
    Outgoing {
        to: c.to.expanded(vars),
        cc: c.cc.expanded(vars),
        subject: expand(&c.subject, vars),
        body: expand(&c.body, vars),
        attachments: c.attachments.iter().map(|a| expand(a.trim(), vars)).filter(|a| !a.is_empty()).map(PathBuf::from).collect(),
    }
}

pub(super) async fn step(state: &AppState, step: &Step, vars: &Vars) -> CommandResult<StepDone> {
    let c: MailConfig = serde_json::from_value(step.config.clone()).map_err(|e| CommandError::BadRequest(format!("la configuración del paso no es válida: {e}")))?;
    let Some(settings) = load(state)? else {
        return Err(CommandError::BadRequest("falta configurar el servidor de correo en Configuración › Correo".into()));
    };
    let mail = outgoing(&c, vars);
    let password = if settings.user.trim().is_empty() { None } else { password()? };
    let summary = deliver(&settings, password, &mail).await.map_err(CommandError::BadRequest)?;
    let mut done = StepDone { summary, ..Default::default() };
    done.outputs.insert("recipients".into(), (mail.to.len() + mail.cc.len()).to_string());
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn server() -> MailSettings {
        MailSettings {
            host: "localhost".into(),
            port: 1025,
            security: Security::None,
            user: String::new(),
            from_address: "dbine@example.com".into(),
            from_name: "DBine Pruebas".into(),
        }
    }

    fn text(m: &Message) -> String {
        String::from_utf8_lossy(&m.formatted()).into_owned()
    }

    #[test]
    fn settings_parse_and_validate() {
        let s: MailSettings = serde_json::from_value(json!({"host": "smtp.example.com", "from_address": "a@example.com"})).unwrap();
        assert_eq!((s.port, s.security), (587, Security::Starttls));
        assert!(s.problem().is_none());
        let s: MailSettings = serde_json::from_value(json!({"host": "h", "port": 465, "security": "tls", "from_address": "nope"})).unwrap();
        assert_eq!(s.security, Security::Tls);
        assert!(s.problem().unwrap().contains("nope"));
        assert!(MailSettings { host: " ".into(), ..server() }.problem().is_some());
        // Nothing secret in what's stored.
        assert!(!serde_json::to_string(&server()).unwrap().contains("password"));
    }

    #[test]
    fn step_config_expands_variables() {
        let mut vars = Vars::new();
        vars.insert("task".into(), "Ventas".into());
        vars.insert("date".into(), "2026-10-08".into());
        vars.insert("steps.1.file".into(), "/tmp/ventas.csv".into());
        vars.insert("steps.1.rows".into(), "42".into());
        let c: MailConfig = serde_json::from_value(json!({
            "to": "ana@example.com; beto@example.com\n", "cc": ["{task}@example.com"],
            "subject": "{task} del {date}", "body": "{steps.1.rows} filas", "attachments": ["{steps.1.file}", " "], "when": "alert"
        }))
        .unwrap();
        let m = outgoing(&c, &vars);
        assert_eq!(m.to, ["ana@example.com", "beto@example.com"]);
        assert_eq!(m.cc, ["Ventas@example.com"]);
        assert_eq!((m.subject.as_str(), m.body.as_str()), ("Ventas del 2026-10-08", "42 filas"));
        assert_eq!(m.attachments, [PathBuf::from("/tmp/ventas.csv")]);
        // An empty config parses (the form saves it before it's filled).
        let c: MailConfig = serde_json::from_value(json!({})).unwrap();
        assert!(outgoing(&c, &vars).to.is_empty());
    }

    #[test]
    fn mime_with_utf8_and_attachments() {
        let dir = std::env::temp_dir().join(format!("dbine-mail-mime-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let csv = dir.join("reporte año.csv");
        std::fs::write(&csv, "a,b\n1,2\n").unwrap();
        let mail = Outgoing {
            to: vec!["Ana <ana@example.com>".into()],
            cc: vec!["beto@example.com".into()],
            subject: "Reporte del año: ñandú ✓".into(),
            body: "Hola, adjunto el reporte.\nSaludos".into(),
            attachments: vec![csv.clone()],
        };
        let raw = text(&build(&server(), &mail).unwrap());
        assert!(raw.contains("From: \"DBine Pruebas\" <dbine@example.com>"), "{raw}");
        assert!(raw.contains("To: Ana <ana@example.com>") && raw.contains("Cc: beto@example.com"), "{raw}");
        // The subject is encoded, not raw UTF-8.
        let subject = raw.lines().find(|l| l.starts_with("Subject:")).unwrap();
        assert!(subject.contains("=?utf-8?"), "{subject}");
        assert!(raw.contains("multipart/mixed") && raw.contains("Content-Type: text/csv"), "{raw}");
        assert!(raw.contains("Content-Disposition: attachment"), "{raw}");

        // Without attachments: one plain part.
        let plain = text(&build(&server(), &Outgoing { attachments: vec![], ..mail }).unwrap());
        assert!(!plain.contains("multipart") && plain.contains("Content-Type: text/plain; charset=utf-8"), "{plain}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bad_mails_fail_clearly() {
        let to = vec!["a@example.com".to_string()];
        let missing = Outgoing { to: to.clone(), attachments: vec![PathBuf::from("/no/existe/reporte.csv")], ..Default::default() };
        assert!(build(&server(), &missing).unwrap_err().contains("/no/existe/reporte.csv"));
        assert!(build(&server(), &Outgoing::default()).unwrap_err().contains("destinatarios"));
        let bad = Outgoing { to: vec!["no es un mail".into()], ..Default::default() };
        assert!(build(&server(), &bad).unwrap_err().contains("no es un mail"));

        let dir = std::env::temp_dir().join(format!("dbine-mail-big-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big = dir.join("big.bin");
        std::fs::File::create(&big).unwrap().set_len(MAX_ATTACHMENTS + 1).unwrap();
        let err = build(&server(), &Outgoing { to, attachments: vec![big], ..Default::default() }).unwrap_err();
        assert!(err.contains("20 MB"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn refused_connection_is_explained() {
        // Nothing listens on port 1 of this machine.
        let s = MailSettings { port: 1, ..server() };
        let m = build(&s, &Outgoing { to: vec!["a@example.com".into()], ..Default::default() }).unwrap();
        let err = send(&s, None, m).await.unwrap_err();
        assert!(err.contains("rechazó la conexión"), "{err}");
    }

    /// Against Mailpit: `docker run -d --name dbine-test-mailpit -p 1025:1025
    /// -p 8025:8025 axllent/mailpit`, then `DBINE_TEST_MAILPIT=1 cargo test
    /// -p dbine mail::tests::live`.
    #[tokio::test]
    async fn live_mailpit() {
        if std::env::var("DBINE_TEST_MAILPIT").is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("dbine-mail-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let csv = dir.join("ventas.csv");
        std::fs::write(&csv, "producto,total\nmate,3\n").unwrap();
        let subject = format!("Reporte del año {}", uuid::Uuid::new_v4());
        let mail = Outgoing {
            to: vec!["ana@example.com".into()],
            cc: vec!["beto@example.com".into()],
            subject: subject.clone(),
            body: "Hola: va el reporte.".into(),
            attachments: vec![csv],
        };
        let summary = deliver(&server(), None, &mail).await.unwrap();
        assert_eq!(summary, "Mail enviado a ana@example.com (cc: beto@example.com); adjuntos: ventas.csv.");

        // Mailpit's API: find it, then read it and its attachment.
        let http = reqwest::Client::new();
        let found: serde_json::Value = http
            .get("http://localhost:8025/api/v1/search")
            .query(&[("query", format!("subject:\"{subject}\""))])
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let id = found["messages"][0]["ID"].as_str().expect("the message reached Mailpit").to_string();
        let msg: serde_json::Value = http.get(format!("http://localhost:8025/api/v1/message/{id}")).send().await.unwrap().json().await.unwrap();
        assert_eq!(msg["Subject"], subject);
        assert_eq!(msg["From"]["Address"], "dbine@example.com");
        assert_eq!(msg["Cc"][0]["Address"], "beto@example.com");
        assert!(msg["Text"].as_str().unwrap().contains("va el reporte"));
        let att = &msg["Attachments"][0];
        assert_eq!(att["FileName"], "ventas.csv");
        let part = att["PartID"].as_str().unwrap();
        let body = http.get(format!("http://localhost:8025/api/v1/message/{id}/part/{part}")).send().await.unwrap().text().await.unwrap();
        assert_eq!(body, "producto,total\nmate,3\n");

        // Mailpit speaks plain SMTP: asking for TLS says so.
        let to = Outgoing { to: vec!["a@example.com".into()], ..Default::default() };
        for security in [Security::Tls, Security::Starttls] {
            let s = MailSettings { security, ..server() };
            let err = deliver(&s, None, &to).await.unwrap_err();
            assert!(err.contains("conexión segura"), "{security:?}: {err}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
