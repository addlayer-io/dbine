//! The HTTP side: Streamable HTTP with plain `application/json` answers
//! (no SSE stream, no sessions). One endpoint, `/mcp`, POST only.
//!
//! Before anything is parsed: a request carrying an `Origin` (a browser
//! page) or a `Host` other than this loopback address (DNS rebinding) gets
//! 403, and one without a valid bearer token gets 401.

use super::{tools, Inner, McpClient};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use serde_json::{json, Value};
use std::sync::Arc;

/// Protocol versions this server speaks, newest first.
const PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

#[derive(Clone)]
struct Ctx {
    inner: Arc<Inner>,
    port: u16,
}

pub(super) fn router(inner: Arc<Inner>, port: u16) -> Router {
    Router::new()
        .route("/mcp", any(handle))
        .fallback(|| async { (StatusCode::NOT_FOUND, "not found") })
        .with_state(Ctx { inner, port })
}

/// Why a request is refused before reaching the protocol, if it is.
pub(super) fn check_request(headers: &HeaderMap, port: u16) -> Result<(), (StatusCode, &'static str)> {
    // MCP over HTTP on localhost must refuse browser origins: any page the
    // user visits could otherwise post to it.
    if headers.contains_key(header::ORIGIN) {
        return Err((StatusCode::FORBIDDEN, "browser origins are not allowed"));
    }
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let allowed = [format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")];
    if !allowed.iter().any(|a| a.eq_ignore_ascii_case(host)) {
        return Err((StatusCode::FORBIDDEN, "unexpected Host header"));
    }
    Ok(())
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = v.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim()).filter(|t| !t.is_empty())
}

async fn handle(State(ctx): State<Ctx>, method: Method, headers: HeaderMap, body: Bytes) -> Response {
    if let Err((status, why)) = check_request(&headers, ctx.port) {
        return (status, why).into_response();
    }
    let client = match bearer(&headers).and_then(|t| ctx.inner.authenticate(t)) {
        Some(c) => c,
        None => {
            return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], "missing or invalid bearer token").into_response()
        }
    };
    if method != Method::POST {
        // No server-initiated stream (GET) and no sessions to end (DELETE).
        return (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST")], "").into_response();
    }
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return json_response(rpc_error(Value::Null, -32700, &format!("parse error: {e}"))),
    };
    match msg {
        Value::Array(batch) => {
            let mut out = Vec::new();
            for m in batch {
                if let Some(r) = dispatch(&ctx.inner, &client, m).await {
                    out.push(r);
                }
            }
            if out.is_empty() {
                StatusCode::ACCEPTED.into_response()
            } else {
                json_response(Value::Array(out))
            }
        }
        m => match dispatch(&ctx.inner, &client, m).await {
            Some(r) => json_response(r),
            None => StatusCode::ACCEPTED.into_response(),
        },
    }
}

fn json_response(v: Value) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response()
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// One JSON-RPC message; `None` for notifications and responses (nothing
/// to answer).
async fn dispatch(inner: &Inner, client: &McpClient, msg: Value) -> Option<Value> {
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        // A response to something we never asked, or garbage without an id.
        return msg.get("id").filter(|_| msg.get("result").is_none() && msg.get("error").is_none()).map(|id| {
            rpc_error(id.clone(), -32600, "invalid request")
        });
    };
    let id = msg.get("id").cloned()?;
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    Some(match method {
        "initialize" => {
            let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
            let version = PROTOCOL_VERSIONS.iter().find(|v| **v == asked).copied().unwrap_or(PROTOCOL_VERSIONS[0]);
            rpc_result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "dbine", "title": "DBine", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": tools::INSTRUCTIONS,
                }),
            )
        }
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({ "tools": tools::definitions() })),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(rpc_error(id, -32602, "missing tool name"));
            };
            if !tools::exists(name) {
                return Some(rpc_error(id, -32602, &format!("unknown tool: {name}")));
            }
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            let (text, is_error) = tools::call(inner, client, name, &args).await;
            rpc_result(id, json!({ "content": [{ "type": "text", "text": text }], "isError": is_error }))
        }
        "resources/list" => rpc_result(id, json!({ "resources": [] })),
        "prompts/list" => rpc_result(id, json!({ "prompts": [] })),
        _ => rpc_error(id, -32601, &format!("method not found: {method}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, HeaderValue::from_static(v));
        }
        h
    }

    #[test]
    fn mcp_origin_and_host_are_checked() {
        assert!(check_request(&headers(&[("host", "127.0.0.1:27517")]), 27517).is_ok());
        assert!(check_request(&headers(&[("host", "localhost:27517")]), 27517).is_ok());
        let browser = check_request(&headers(&[("host", "127.0.0.1:27517"), ("origin", "https://evil.example")]), 27517);
        assert_eq!(browser.unwrap_err().0, StatusCode::FORBIDDEN);
        let local_page = check_request(&headers(&[("host", "127.0.0.1:27517"), ("origin", "http://localhost:5173")]), 27517);
        assert_eq!(local_page.unwrap_err().0, StatusCode::FORBIDDEN);
        // DNS rebinding: a page's name resolving to 127.0.0.1.
        assert_eq!(check_request(&headers(&[("host", "evil.example:27517")]), 27517).unwrap_err().0, StatusCode::FORBIDDEN);
    }

    #[test]
    fn mcp_bearer_is_parsed() {
        assert_eq!(bearer(&headers(&[("authorization", "Bearer abc")])), Some("abc"));
        assert_eq!(bearer(&headers(&[("authorization", "bearer  abc ")])), Some("abc"));
        assert_eq!(bearer(&headers(&[("authorization", "Basic abc")])), None);
        assert_eq!(bearer(&headers(&[])), None);
    }
}
