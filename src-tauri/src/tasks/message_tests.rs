//! Tests of the "Enviar un mensaje" step: nothing here reaches the real
//! services (the send path runs against a local mock server).

use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const SLACK: &str = "https://hooks.slack.com/services/T000/B000/SECRETslackpart";
const TEAMS: &str = "https://prod-12.westus.logic.azure.com:443/workflows/abc/triggers/manual/paths/invoke?api-version=2016-06-01&sig=SECRETsig";

fn config(channel: Channel, url: &str, token: &str, chat: &str) -> MessageConfig {
    MessageConfig { channel, url: url.into(), token: token.into(), chat_id: chat.into(), text: String::new() }
}

fn message_step(id: &str, config: Value) -> Step {
    Step { id: id.into(), kind: kinds::SEND_MESSAGE.into(), config, ..Default::default() }
}

fn task_with(steps: Vec<Step>) -> ScheduledTask {
    ScheduledTask { id: "t".into(), name: "prod".into(), steps, ..Default::default() }
}

#[test]
fn each_channel_gets_its_request() {
    let s = request(&config(Channel::Slack, SLACK, "", ""), "DBine · prod", "bien").unwrap();
    assert_eq!(s.url.as_str(), SLACK);
    assert_eq!(s.body, json!({ "text": "*DBine · prod*\nbien" }));
    assert_eq!(s.bearer, None);

    let t = request(&config(Channel::Teams, TEAMS, "", ""), "DBine · prod", "bien").unwrap();
    assert_eq!(t.body["type"], "message");
    assert_eq!(t.body["attachments"][0]["contentType"], "application/vnd.microsoft.card.adaptive");
    assert_eq!(t.body["attachments"][0]["content"]["body"][0]["text"], "DBine · prod");
    assert_eq!(t.body["attachments"][0]["content"]["body"][1]["text"], "bien");

    let g = request(&config(Channel::Telegram, "", "123456:ABC-def_9", "-1001"), "DBine · prod", "bien").unwrap();
    assert_eq!(g.url.as_str(), "https://api.telegram.org/bot123456:ABC-def_9/sendMessage");
    assert_eq!(g.body, json!({ "chat_id": "-1001", "text": "DBine · prod\nbien", "disable_web_page_preview": true }));
    assert!(g.body.get("parse_mode").is_none(), "plain text");

    let n = request(&config(Channel::Ntfy, "https://ntfy.sh/dbine-alertas", "tk_1234", ""), "DBine · prod", "bien").unwrap();
    assert_eq!(n.url.as_str(), "https://ntfy.sh/");
    assert_eq!(n.body, json!({ "topic": "dbine-alertas", "title": "DBine · prod", "message": "bien" }));
    assert_eq!(n.bearer.as_deref(), Some("tk_1234"));
    // A server of the user's own, under a path.
    let own = request(&config(Channel::Ntfy, "https://avisos.example.com/ntfy/db_1", "", ""), "t", "x").unwrap();
    assert_eq!(own.url.as_str(), "https://avisos.example.com/ntfy/");
    assert_eq!(own.body["topic"], "db_1");
    assert_eq!(own.bearer, None);
}

#[test]
fn hosts_are_the_channels_own() {
    // Teams: Microsoft's domains only, on 443.
    for ok in [TEAMS, "https://abc.environment.api.powerplatform.com/powerautomate/automations/direct/workflows/x", "https://contoso.webhook.office.com/webhookb2/x"] {
        assert!(check_url(Channel::Teams, ok).is_ok(), "{ok}");
    }
    for bad in [
        "https://evil.example.com/workflows/x",
        "https://logic.azure.com.evil.example.com/x",
        "https://evillogic.azure.com/x",
        "http://prod-1.westus.logic.azure.com/x",
        "https://prod-1.westus.logic.azure.com:8443/x",
        "https://user:pw@prod-1.westus.logic.azure.com/x",
        "",
        "no es una url",
    ] {
        assert!(check_url(Channel::Teams, bad).is_err(), "{bad}");
    }
    // Slack: hooks.slack.com only.
    assert!(check_url(Channel::Slack, SLACK).is_ok());
    for bad in ["http://hooks.slack.com/services/x", "https://hooks.slack.com.evil.example.com/x", "https://slack.com/x", "https://hooks.slack.com:444/x"] {
        assert!(check_url(Channel::Slack, bad).is_err(), "{bad}");
    }
    // ntfy: any public https server, never this machine or the local network.
    assert!(check_url(Channel::Ntfy, "https://ntfy.sh/tema").is_ok());
    for bad in [
        "http://ntfy.sh/tema",
        "https://localhost/tema",
        "https://127.0.0.1/tema",
        "https://10.0.0.5/tema",
        "https://192.168.1.10/tema",
        "https://169.254.169.254/latest",
        "https://[::1]/tema",
        "https://metadata.google.internal/tema",
        "https://ntfy.sh/",
        "https://ntfy.sh/tema?x=1",
        "https://ntfy.sh/no/vale espacio",
    ] {
        assert!(check_url(Channel::Ntfy, bad).is_err(), "{bad}");
    }
    // Telegram's URL is DBine's own: only the token and chat are typed.
    assert!(check_url(Channel::Telegram, "https://api.telegram.org").is_err());
}

#[test]
fn bad_settings_fail_clearly() {
    assert!(request(&config(Channel::Teams, "", "", ""), "t", "x").unwrap_err().contains("falta"));
    assert!(request(&config(Channel::Telegram, "", "", "1"), "t", "x").unwrap_err().contains("falta el token"));
    assert!(request(&config(Channel::Telegram, "", "1:a", ""), "t", "x").unwrap_err().contains("falta el chat"));
    assert!(request(&config(Channel::Telegram, "", "1:a/../x", "1"), "t", "x").is_err(), "the token can't change the path");
    assert!(request(&config(Channel::Telegram, "", "1:a?x=1", "1"), "t", "x").is_err());
    assert!(request(&config(Channel::Telegram, "", "1:a", "1; DROP"), "t", "x").is_err());
    assert!(request(&config(Channel::Telegram, "", "1:a", "@canal_ok"), "t", "x").is_ok());
}

#[test]
fn long_texts_are_cut() {
    let (_, text) = compose(Channel::Slack, "t", "{x}", &BTreeMap::from([("x".to_string(), "a".repeat(5000))]));
    assert_eq!(text.chars().count(), MAX_CHARS + 1);
    assert!(text.ends_with('…'));
    // An escape is never left half-written at the cut.
    let (_, text) = compose(Channel::Slack, "t", "{x}", &BTreeMap::from([("x".to_string(), format!("{}<<<<", "a".repeat(MAX_CHARS - 2)))]));
    assert!(text.ends_with("a&lt;…") || text.ends_with("a…"), "{}", &text[text.len() - 12..]);
    // Short texts stay as they are.
    assert_eq!(cut("  hola  "), "hola");
}

#[test]
fn values_cant_add_links_or_mentions() {
    let vars = BTreeMap::from([
        ("results".to_string(), "<!channel> <@U123> <https://evil.example.com|clic> & @here [clic](https://evil.example.com)".to_string()),
        ("task".to_string(), "prod".to_string()),
    ]);
    let (title, slack) = compose(Channel::Slack, "prod <!here>", "Hola <!subteam^S1> {results}", &vars);
    assert!(!slack.contains("<!channel>") && !slack.contains("<@U123>") && !slack.contains("<https://"), "{slack}");
    assert!(slack.contains("&lt;!channel&gt;") && slack.contains("&amp;"), "{slack}");
    assert!(!slack.contains("@here"), "{slack}");
    // The user's own text is left as written.
    assert!(slack.starts_with("Hola <!subteam^S1> "), "{slack}");
    assert!(!title.contains("<!here>"), "{title}");

    let (_, teams) = compose(Channel::Teams, "prod", "{results}", &vars);
    assert!(!teams.contains("](https://"), "{teams}");
    assert!(!teams.contains("@here"), "{teams}");

    // Plain text channels get the values as they are.
    let (_, tg) = compose(Channel::Telegram, "prod", "{results}", &vars);
    assert_eq!(tg, vars["results"]);
    let (_, nt) = compose(Channel::Ntfy, "prod", "{results}", &vars);
    assert_eq!(nt, vars["results"]);

    // No text: what each earlier step did.
    let (_, default) = compose(Channel::Telegram, "prod", "  ", &BTreeMap::from([("results".to_string(), "✓ 1. export: 3 filas".to_string())]));
    assert_eq!(default, "✓ 1. export: 3 filas");
}

#[test]
fn errors_redact_urls_and_tokens() {
    let secrets = vec!["123:SECRETtoken".to_string(), "SECRETtoken".to_string()];
    let msg = redact("falló https://api.telegram.org/bot123:SECRETtoken/sendMessage (SECRETtoken) y https://hooks.slack.com/services/T/B/X?y=1", &secrets);
    assert!(!msg.contains("SECRET") && !msg.contains("/services/"), "{msg}");
    assert!(msg.contains("https://hooks.slack.com/…"), "{msg}");
}

/// A server on 127.0.0.1 that answers one request with `answer` and
/// returns what it got.
async fn serve_once(answer: &'static str) -> (SocketAddr, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut got = Vec::new();
        let mut buf = vec![0u8; 8192];
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            got.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&got).to_string();
            if let Some(end) = text.find("\r\n\r\n") {
                let len = text.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))).unwrap_or(0);
                if got.len() >= end + 4 + len {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        sock.write_all(answer.as_bytes()).await.unwrap();
        let _ = sock.shutdown().await;
        String::from_utf8_lossy(&got).to_string()
    });
    (addr, server)
}

/// `req` sent to the mock server instead (same path and query).
fn to_mock(mut req: Request, addr: SocketAddr) -> Request {
    let mut url = Url::parse(&format!("http://{addr}")).unwrap();
    url.set_path(req.url.path());
    url.set_query(req.url.query());
    req.url = url;
    req
}

#[tokio::test]
async fn the_send_path_posts_the_json() {
    let (addr, server) = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await;
    let req = request(&config(Channel::Ntfy, "https://ntfy.sh/dbine-alertas", "tk_secret", ""), "DBine · prod", "✓ 1. export").unwrap();
    send_on(&to_mock(req, addr), true).await.unwrap();
    let got = server.await.unwrap();
    assert!(got.starts_with("POST / HTTP/1.1"), "{got}");
    assert!(got.to_ascii_lowercase().contains("authorization: bearer tk_secret"), "{got}");
    assert!(got.to_ascii_lowercase().contains("content-type: application/json"), "{got}");
    let body: Value = serde_json::from_str(got.split("\r\n\r\n").nth(1).unwrap()).unwrap();
    assert_eq!(body, json!({ "topic": "dbine-alertas", "title": "DBine · prod", "message": "✓ 1. export" }));
}

#[tokio::test]
async fn the_service_answer_is_reported_without_the_secret() {
    let (addr, server) = serve_once("HTTP/1.1 404 Not Found\r\nContent-Length: 44\r\nConnection: close\r\n\r\nchat not found for bot123456:SECRET-tok_123").await;
    let req = request(&config(Channel::Telegram, "", "123456:SECRET-tok_123", "1"), "t", "x").unwrap();
    let err = send_on(&to_mock(req, addr), true).await.unwrap_err();
    assert!(err.contains("404") && err.contains("chat not found"), "{err}");
    assert!(!err.contains("SECRET"), "{err}");
    let got = server.await.unwrap();
    assert!(got.starts_with("POST /bot123456:SECRET-tok_123/sendMessage"), "{got}");
}

#[tokio::test]
async fn redirects_are_not_followed() {
    // Where the redirect points: it must never be asked.
    let elsewhere = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = elsewhere.local_addr().unwrap();
    let answer: &'static str = Box::leak(format!("HTTP/1.1 302 Found\r\nLocation: http://{target}/steal\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_boxed_str());
    let (addr, server) = serve_once(answer).await;
    let req = request(&config(Channel::Slack, SLACK, "", ""), "t", "x").unwrap();
    let err = send_on(&to_mock(req, addr), true).await.unwrap_err();
    assert!(err.contains("redirección") && err.contains("302"), "{err}");
    assert!(!err.contains("SECRET"), "{err}");
    server.await.unwrap();
    assert!(tokio::time::timeout(Duration::from_millis(300), elsewhere.accept()).await.is_err(), "the redirect was followed");
}

#[tokio::test]
async fn errors_never_show_the_secret() {
    // Nothing listens on port 9.
    let req = request(&config(Channel::Telegram, "", "999:SECRET-TOKEN", "1"), "t", "x").unwrap();
    let mut req = req;
    req.url = Url::parse("http://127.0.0.1:9/bot999:SECRET-TOKEN/sendMessage").unwrap();
    let err = send_on(&req, true).await.unwrap_err();
    assert!(!err.contains("SECRET-TOKEN"), "{err}");
}

#[tokio::test]
async fn local_and_private_addresses_are_refused() {
    let (addr, _server) = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n").await;
    let req = request(&config(Channel::Slack, SLACK, "", ""), "t", "x").unwrap();
    // Plain http, or https to this machine, without the tests' allowance.
    let err = send(&to_mock(req, addr)).await.unwrap_err();
    assert!(err.contains("HTTPS"), "{err}");
    let mut req = request(&config(Channel::Slack, SLACK, "", ""), "t", "x").unwrap();
    req.url = Url::parse(&format!("https://127.0.0.1:{}/x", addr.port())).unwrap();
    let err = send(&req).await.unwrap_err();
    assert!(err.contains("servidores públicos"), "{err}");
    req.url = Url::parse("https://169.254.169.254/latest/meta-data").unwrap();
    assert!(send(&req).await.unwrap_err().contains("servidores públicos"));
    req.url = Url::parse("https://localhost/x").unwrap();
    assert!(send(&req).await.unwrap_err().contains("servidores públicos"));
}

/// The webhook and the tokens go to the vault (for their channel) and the
/// task keeps them empty; an empty field keeps the saved one.
#[test]
fn secrets_go_to_the_vault_for_their_channel() {
    use std::cell::RefCell;
    use std::collections::HashMap;
    let vault: RefCell<HashMap<String, String>> = RefCell::default();
    let get = |n: &str| vault.borrow().get(n).cloned();
    let save = |t: &mut ScheduledTask| -> Result<(), String> {
        for (name, kept) in secrets_apart(t, &get)? {
            match kept {
                Some(json) => vault.borrow_mut().insert(name, json),
                None => vault.borrow_mut().remove(&name),
            };
        }
        Ok(())
    };
    let name = step_secret_name("t", "m");
    let mut t = task_with(vec![message_step("m", json!({"channel": "slack", "url": SLACK, "token": "", "text": "{results}", "when": "always"}))]);
    save(&mut t).unwrap();
    let stored = serde_json::to_string(&t).unwrap();
    assert!(!stored.contains("SECRET") && !stored.contains("hooks.slack.com"), "{stored}");
    assert_eq!(t.steps[0].config["url"], "");
    assert!(vault.borrow()[&name].contains("SECRETslackpart"));
    assert!(vault.borrow()[&name].contains(r#""channel":"slack""#));
    // The run gets it back.
    assert_eq!(resolved("t", &t.steps[0], &get).unwrap().url, SLACK);

    // Saved again untouched: it stands.
    save(&mut t).unwrap();
    assert_eq!(resolved("t", &t.steps[0], &get).unwrap().url, SLACK);

    // Another channel never gets it: ntfy with the field left empty has no
    // URL (and the Slack one is dropped from the vault).
    t.steps[0].config["channel"] = json!("ntfy");
    assert_eq!(resolved("t", &t.steps[0], &get).unwrap().url, "", "a Slack webhook must not reach ntfy");
    save(&mut t).unwrap();
    assert!(!vault.borrow().contains_key(&name));

    // Telegram: the token is kept, the chat stays in the task.
    t.steps[0].config = json!({"channel": "telegram", "url": "", "token": "123:SECRETtg", "chat_id": "-1001", "text": ""});
    save(&mut t).unwrap();
    assert_eq!(t.steps[0].config["token"], "");
    assert_eq!(t.steps[0].config["chat_id"], "-1001");
    assert_eq!(resolved("t", &t.steps[0], &get).unwrap().token, "123:SECRETtg");

    // A typed value that can't work refuses the save, without echoing it.
    let mut bad = task_with(vec![message_step("m", json!({"channel": "teams", "url": "https://evil.example.com/SECRETx"}))]);
    let err = save(&mut bad).unwrap_err();
    assert!(err.contains("Microsoft") && !err.contains("SECRETx"), "{err}");
    let mut bad = task_with(vec![message_step("m", json!({"channel": "telegram", "token": "SECRET no"}))]);
    assert!(!save(&mut bad).unwrap_err().contains("SECRET"));

    // Other steps are left alone.
    let mut other = task_with(vec![Step { id: "s".into(), kind: kinds::RUN_SCRIPT.into(), config: json!({"url": "x"}), ..Default::default() }]);
    assert!(secrets_apart(&mut other, &get).unwrap().is_empty());
    assert_eq!(other.steps[0].config["url"], "x");
}

/// The approval sees where a message goes (channel and host), never the
/// secret part.
#[test]
fn destinations_name_the_host_not_the_secret() {
    let vault = BTreeMap::from([(step_secret_name("t", "m"), json!({"channel": "teams", "url": TEAMS}).to_string())]);
    let get = |n: &str| vault.get(n).cloned();
    let t = task_with(vec![
        message_step("m", json!({"channel": "teams", "url": ""})),
        message_step("g", json!({"channel": "telegram", "token": "42:SECRETx", "chat_id": "-100"})),
        message_step("n", json!({"channel": "ntfy", "url": "https://ntfy.example.com/SECRETtopic", "token": "SECRETtk"})),
    ]);
    let d = destinations(&t, &get);
    assert_eq!(d, vec!["ntfy:ntfy.example.com", "teams:prod-12.westus.logic.azure.com", "telegram:42:-100"]);
    assert!(!d.concat().contains("SECRET"));
    // Another host is another destination.
    let mut moved = t.clone();
    moved.steps[0].config["url"] = json!("https://prod-99.eastus.logic.azure.com/workflows/x?sig=other");
    assert_ne!(destinations(&moved, &get), d);
    // A new secret path on the same host is not.
    let mut same = t.clone();
    same.steps[0].config["url"] = json!("https://prod-12.westus.logic.azure.com/workflows/other?sig=new");
    assert_eq!(destinations(&same, &get), d);
}
