//! What DBine Enterprise tells DBine while signed in, over one outbound
//! WebSocket (`/api/v1/desktop/events`): each notification goes to the UI as
//! an `enterprise-event`, which words it in the user's language (and as a
//! system notification); the ones that change what DBine may use resync the
//! catalog. A dropped stream reconnects and catches up from the last event.

use super::Enterprise;
use futures_util::StreamExt;
use std::sync::Arc;
use std::time::Duration;
use tauri::Emitter;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// Notifications after which the catalog is read again.
fn changes_catalog(kind: &str) -> bool {
    matches!(
        kind,
        "catalog.changed"
            | "grant.added"
            | "grant.revoked"
            | "request.approved"
            | "request.rejected"
            | "request.expired"
            | "request.cancelled"
            | "request.updated"
            | "session.ended"
    )
}

pub async fn run(e: Arc<Enterprise>) {
    let mut backoff = Duration::from_secs(2);
    loop {
        match listen(&e).await {
            // The server closed it or the token ended: try again shortly
            // (a lost session stops here, `bearer` fails).
            Ok(()) => backoff = Duration::from_secs(2),
            Err(err) => {
                tracing::info!("DBine Enterprise events: {err}");
                {
                    let mut st = e.status.lock().unwrap();
                    if st.online {
                        st.online = false;
                        st.error = Some(err);
                    }
                }
                e.emit_status();
            }
        }
        if e.token.lock().unwrap().is_none() {
            return;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

async fn listen(e: &Arc<Enterprise>) -> Result<(), String> {
    let server = e.server().ok_or("sin servidor")?;
    let token = e.bearer().await.map_err(|x| x.to_string())?;
    let after = e.last_event();
    let url = format!(
        "{}/api/v1/desktop/events?after={after}",
        server
            .replacen("https://", "wss://", 1)
            .replacen("http://", "ws://", 1)
    );
    let mut req = url.into_client_request().map_err(|x| x.to_string())?;
    req.headers_mut().insert(
        "authorization",
        format!("Bearer {token}")
            .parse()
            .map_err(|_| "token inválido".to_string())?,
    );
    let (mut ws, _) = tokio_tungstenite::connect_async_tls_with_config(
        req,
        None,
        false,
        Some(tokio_tungstenite::Connector::Rustls(super::gateway_tls())),
    )
    .await
    .map_err(|x| format!("DBine Enterprise no responde ({x})"))?;
    // Back online: what changed meanwhile comes as events, but resync anyway.
    let _ = e.sync().await;
    let mut resync = tokio::time::interval(Duration::from_secs(600));
    resync.tick().await;
    loop {
        tokio::select! {
            msg = ws.next() => match msg {
                Some(Ok(Message::Text(t))) => {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
                    let kind = v["kind"].as_str().unwrap_or_default().to_string();
                    if let Some(id) = v["id"].as_i64() {
                        e.set_last_event(id);
                    }
                    if changes_catalog(&kind) {
                        let _ = e.sync().await;
                    }
                    let _ = e.app.emit("enterprise-event", v);
                }
                Some(Ok(_)) => {}
                Some(Err(x)) => return Err(x.to_string()),
                None => return Ok(()),
            },
            // Grants that simply expire send nothing: the catalog is read
            // again now and then.
            _ = resync.tick() => {
                let _ = e.sync().await;
            }
        }
    }
}
