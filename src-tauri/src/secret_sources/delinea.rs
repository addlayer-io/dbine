//! Delinea Secret Server (on premises and Secret Server Cloud) over its REST
//! API:
//!
//! 1. An OAuth2 access token: `POST <url>/oauth2/token` with the password
//!    grant (a user or an application account, optionally with its domain);
//!    with Delinea Platform, `POST https://<tenant>.delinea.app/identity/api/
//!    oauth2/token/xpmplatform` with a service user's client ID and secret.
//! 2. The secret by ID, or found by name (`GET <url>/api/v1/secrets?
//!    filter.searchText=…`; more than one with that name is an error).
//! 3. `GET <url>/api/v1/secrets/{id}`: the field by slug (`password` by
//!    default) and, when asked, `username`.
//!
//! The token goes only to the URLs the user gave (Secret Server's, and the
//! platform tenant's for the sign-in), over HTTPS (HTTP only to this
//! computer), without following redirects. Errors carry Delinea's codes and
//! messages, never the password, the token or a value.

use super::cloud_auth::uri_encode;
use super::https;
use dbine_core::secret_sources::{delinea_platform_host, wipe};
use serde_json::Value;

/// The password (or the client secret) was refused. `fill` checks for this
/// exact text to ask an asked-for password again.
pub const WRONG_PASSWORD: &str =
    "Delinea rechazó el usuario o la contraseña (o el ID y el secreto del cliente)";

pub struct Request<'a> {
    pub url: &'a str,
    /// Empty: sign in to Secret Server itself.
    pub platform_url: &'a str,
    pub username: &'a str,
    pub domain: &'a str,
    /// An ID (digits) or a name.
    pub secret: &'a str,
    /// The field's slug; empty: `password`.
    pub field: &'a str,
    pub fill_username: bool,
    /// The field (slug or name) the user comes from; empty: `username`.
    pub user_field: &'a str,
}

/// `(value, username)`.
pub async fn read(req: &Request<'_>, password: &str) -> Result<(String, Option<String>), String> {
    let client = https::client(req.url, "Secret Server", "", None)?;
    if !req.platform_url.trim().is_empty() && delinea_platform_host(req.platform_url).is_none() {
        return Err(
            "la URL de Delinea Platform tiene que ser https://<inquilino>.delinea.app".into(),
        );
    }
    read_with(&client, req, password).await
}

pub async fn read_with(
    client: &reqwest::Client,
    req: &Request<'_>,
    password: &str,
) -> Result<(String, Option<String>), String> {
    let mut token = access_token(client, req, password).await?;
    let out = async {
        let id = find(client, req, &token).await?;
        let secret = api(client, req, &token, &format!("/api/v1/secrets/{id}")).await?;
        values(&secret, req)
    }
    .await;
    wipe(&mut token);
    out
}

/// The account wizard's check: the access token, then who it's for
/// (`GET /api/v1/users/current`). The user name.
pub async fn whoami(req: &Request<'_>, password: &str) -> Result<String, String> {
    let client = https::client(req.url, "Secret Server", "", None)?;
    if !req.platform_url.trim().is_empty() && delinea_platform_host(req.platform_url).is_none() {
        return Err(
            "la URL de Delinea Platform tiene que ser https://<inquilino>.delinea.app".into(),
        );
    }
    whoami_with(&client, req, password).await
}

pub async fn whoami_with(
    client: &reqwest::Client,
    req: &Request<'_>,
    password: &str,
) -> Result<String, String> {
    let mut token = access_token(client, req, password).await?;
    let r = api(client, req, &token, "/api/v1/users/current").await;
    wipe(&mut token);
    let json = r?;
    let s = |k: &str| {
        json.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    Ok(s("userName")
        .or_else(|| s("displayName"))
        .or_else(|| s("emailAddress"))
        .unwrap_or_else(|| req.username.trim().to_string()))
}

async fn access_token(
    client: &reqwest::Client,
    req: &Request<'_>,
    password: &str,
) -> Result<String, String> {
    if password.is_empty() {
        return Err("falta la contraseña de Delinea: escribila en el formulario".into());
    }
    let platform = req.platform_url.trim().trim_end_matches('/');
    let (url, form): (String, Vec<(&str, &str)>) = if platform.is_empty() {
        let mut f = vec![
            ("grant_type", "password"),
            ("username", req.username.trim()),
            ("password", password),
        ];
        if !req.domain.trim().is_empty() {
            f.push(("domain", req.domain.trim()));
        }
        (format!("{}/oauth2/token", https::base(req.url)), f)
    } else {
        let f = vec![
            ("grant_type", "client_credentials"),
            ("client_id", req.username.trim()),
            ("client_secret", password),
            ("scope", "xpmheadless"),
        ];
        (
            format!("{platform}/identity/api/oauth2/token/xpmplatform"),
            f,
        )
    };
    let host = url.split('/').take(3).collect::<Vec<_>>().join("/");
    let resp = client.post(&url).form(&form).send().await.map_err(|e| {
        format!(
            "no se pudo conectar con Delinea en {host}: {}",
            e.without_url()
        )
    })?;
    let status = resp.status().as_u16();
    let mut text = resp.text().await.unwrap_or_default();
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    wipe(&mut text);
    if let Some(t) = json
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    {
        return Ok(t.to_string());
    }
    Err(token_error(status, &json))
}

pub fn token_error(status: u16, json: &Value) -> String {
    let code = json.get("error").and_then(Value::as_str).unwrap_or("");
    let desc = json
        .get("error_description")
        .and_then(Value::as_str)
        .unwrap_or("");
    match (status, code) {
        (_, "invalid_grant" | "invalid_client" | "unauthorized_client") | (401, _) => {
            WRONG_PASSWORD.into()
        }
        (400, c) if c.to_ascii_lowercase().contains("login failed") => WRONG_PASSWORD.into(),
        (200, "") => "Delinea no devolvió un token de acceso".into(),
        (_, "") => format!("Delinea respondió {status} al pedir el token de acceso"),
        _ => format!(
            "Delinea rechazó el pedido del token de acceso: {}",
            super::exec::first_lines(&format!("{code} {desc}"), 200).trim()
        ),
    }
}

async fn api(
    client: &reqwest::Client,
    req: &Request<'_>,
    token: &str,
    path_and_query: &str,
) -> Result<Value, String> {
    let base = https::base(req.url);
    let resp = client
        .get(format!("{base}{path_and_query}"))
        .bearer_auth(token)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| {
            format!(
                "no se pudo conectar con Secret Server en {base}: {}",
                e.without_url()
            )
        })?;
    let status = resp.status().as_u16();
    let mut text = resp.text().await.unwrap_or_default();
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    wipe(&mut text);
    if (200..300).contains(&status) {
        return Ok(json);
    }
    Err(api_error(status, &json))
}

pub fn api_error(status: u16, json: &Value) -> String {
    let code = json.get("errorCode").and_then(Value::as_str).unwrap_or("");
    let msg = super::exec::first_lines(
        json.get("message").and_then(Value::as_str).unwrap_or(""),
        300,
    );
    match (status, code) {
        (
            _,
            "API_CommentRequired"
            | "API_DoubleLockPasswordRequired"
            | "API_CheckoutRequired"
            | "API_ApprovalRequired",
        ) => {
            format!("Secret Server pide un paso más para ver este secreto ({code}): DBine no puede leerlo así")
        }
        (401, _) => "Secret Server rechazó el token de acceso".into(),
        (403, _) | (_, "API_AccessDenied") => format!(
            "Secret Server: permiso denegado{}",
            if msg.is_empty() {
                String::new()
            } else {
                format!(": {msg}")
            }
        ),
        (_, c) if !c.is_empty() => format!("Secret Server respondió {c}: {msg}"),
        _ if msg.is_empty() => format!("Secret Server respondió {status}"),
        _ => format!("Secret Server respondió {status}: {msg}"),
    }
}

/// The secret's ID: as given (digits), else the one secret with that name.
async fn find(client: &reqwest::Client, req: &Request<'_>, token: &str) -> Result<u64, String> {
    let wanted = req.secret.trim();
    if let Ok(id) = wanted.parse::<u64>() {
        return Ok(id);
    }
    let q = format!(
        "/api/v1/secrets?filter.searchText={}&filter.isExactMatch=true&take=100",
        uri_encode(wanted)
    );
    let json = api(client, req, token, &q).await?;
    pick_by_name(
        json.get("records")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]),
        wanted,
    )
}

fn id_of(v: &Value) -> Option<u64> {
    v.get("id").and_then(|i| {
        i.as_u64()
            .or_else(|| i.as_str().and_then(|s| s.parse().ok()))
    })
}

fn name_of(v: &Value) -> &str {
    v.get("name").and_then(Value::as_str).unwrap_or("")
}

/// The one record named `wanted` (exactly, else ignoring case).
pub fn pick_by_name(list: &[Value], wanted: &str) -> Result<u64, String> {
    let exact: Vec<&Value> = list.iter().filter(|s| name_of(s) == wanted).collect();
    let found = if exact.is_empty() {
        list.iter()
            .filter(|s| name_of(s).eq_ignore_ascii_case(wanted))
            .collect()
    } else {
        exact
    };
    match found.len() {
        1 => {
            id_of(found[0]).ok_or_else(|| "Secret Server no devolvió el ID del secreto".to_string())
        }
        0 => Err(format!(
            "Secret Server: no hay un secreto «{wanted}» (o el usuario no tiene acceso)"
        )),
        n => {
            let ids: Vec<String> = found
                .iter()
                .take(6)
                .filter_map(|s| id_of(s))
                .map(|i| i.to_string())
                .collect();
            Err(format!("Secret Server tiene {n} secretos «{wanted}» (IDs: {}): indicá el ID del que corresponde", ids.join(", ")))
        }
    }
}

/// The field (by slug, else by name) and, when asked, the user.
pub fn values(secret: &Value, req: &Request<'_>) -> Result<(String, Option<String>), String> {
    let items: &[Value] = secret
        .get("items")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let name = Some(name_of(secret))
        .filter(|n| !n.is_empty())
        .unwrap_or(req.secret.trim())
        .to_string();
    let find = |w: &str| {
        let by = |k: &str| {
            items.iter().find(|i| {
                i.get(k)
                    .and_then(Value::as_str)
                    .is_some_and(|v| v.eq_ignore_ascii_case(w))
            })
        };
        by("slug").or_else(|| by("fieldName"))
    };
    let text = |i: &Value| {
        i.get("itemValue")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let want = Some(req.field.trim())
        .filter(|f| !f.is_empty())
        .unwrap_or("password");
    let Some(item) = find(want) else {
        let mut slugs: Vec<&str> = items
            .iter()
            .filter_map(|i| i.get("slug").and_then(Value::as_str))
            .take(30)
            .collect();
        slugs.sort_unstable();
        return Err(format!(
            "el secreto «{name}» de Secret Server no tiene el campo «{want}» (tiene: {})",
            slugs.join(", ")
        ));
    };
    if item.get("isFile").and_then(Value::as_bool) == Some(true) {
        return Err(format!(
            "el campo «{want}» del secreto «{name}» es un archivo"
        ));
    }
    let user_field = Some(req.user_field.trim())
        .filter(|f| !f.is_empty())
        .unwrap_or("username");
    let username = if req.fill_username {
        match find(user_field).map(text).filter(|u| !u.is_empty()) {
            Some(u) => Some(u),
            None => {
                return Err(format!(
                    "el secreto «{name}» de Secret Server no tiene usuario ({user_field})"
                ))
            }
        }
    } else {
        None
    };
    Ok((text(item), username))
}

// ---- browse ------------------------------------------------------------------

/// The secrets matching `query` (Secret Server's search; empty: the first
/// ones), or a secret's field slugs (`item`, an ID or a name), from its
/// template: no value is read.
pub async fn browse(
    req: &Request<'_>,
    password: &str,
    at: super::browse::At<'_>,
) -> Result<super::browse::Listing, String> {
    let client = https::client(req.url, "Secret Server", "", None)?;
    if !req.platform_url.trim().is_empty() && delinea_platform_host(req.platform_url).is_none() {
        return Err(
            "la URL de Delinea Platform tiene que ser https://<inquilino>.delinea.app".into(),
        );
    }
    browse_with(&client, req, password, at).await
}

pub async fn browse_with(
    client: &reqwest::Client,
    req: &Request<'_>,
    password: &str,
    at: super::browse::At<'_>,
) -> Result<super::browse::Listing, String> {
    use super::browse::{unique_names, Entry, Listing, MAX_ENTRIES};
    let mut token = access_token(client, req, password).await?;
    let out = async {
        if let Some(item) = at.item {
            let r = Request {
                secret: item,
                ..*req
            };
            let id = find(client, &r, &token).await?;
            let summary = api(
                client,
                req,
                &token,
                &format!("/api/v1/secrets/{id}/summary"),
            )
            .await?;
            let template = summary
                .get("secretTemplateId")
                .and_then(|i| {
                    i.as_u64()
                        .or_else(|| i.as_str().and_then(|s| s.parse().ok()))
                })
                .ok_or("Secret Server no devolvió la plantilla del secreto")?;
            let t = api(
                client,
                req,
                &token,
                &format!("/api/v1/secret-templates/{template}"),
            )
            .await?;
            let slugs: Vec<String> = t
                .get("fields")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|f| f.get("isFile").and_then(Value::as_bool) != Some(true))
                .filter_map(|f| {
                    ["fieldSlugName", "slug", "name"]
                        .iter()
                        .find_map(|k| f.get(*k).and_then(Value::as_str).filter(|v| !v.is_empty()))
                        .map(str::to_string)
                })
                .collect();
            return Ok(Listing::fields(slugs));
        }
        let q = format!(
            "/api/v1/secrets?filter.searchText={}&take={}",
            uri_encode(at.query),
            MAX_ENTRIES
        );
        let json = api(client, req, &token, &q).await?;
        let records: Vec<Value> = json
            .get("records")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let unique = unique_names(records.iter().map(name_of));
        let mut out: Vec<Entry> = records
            .iter()
            .filter_map(|r| {
                let id = id_of(r)?.to_string();
                let name = name_of(r).to_string();
                let folder = r
                    .get("folderPath")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .replace('\\', "/")
                    .trim_matches('/')
                    .to_string();
                let template = r
                    .get("secretTemplateName")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let by_name = !name.trim().is_empty()
                    && name.parse::<u64>().is_err()
                    && unique.contains(&name);
                let detail = if template.is_empty() {
                    id.clone()
                } else {
                    format!("{id} · {template}")
                };
                Some(Entry::item(
                    if by_name { name.clone() } else { id.clone() },
                    if name.is_empty() { id } else { name },
                    folder,
                    detail,
                ))
            })
            .collect();
        out.sort_by_key(|e| (e.path.to_lowercase(), e.name.to_lowercase()));
        let mut l = Listing::complete(out);
        l.search = true;
        l.truncated = l.truncated || json.get("hasNext").and_then(Value::as_bool) == Some(true);
        Ok(l)
    }
    .await;
    wipe(&mut token);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path as AxPath, Query, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::{Form, Json, Router};
    use serde_json::json;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    fn secret_json(id: u64, name: &str) -> Value {
        json!({ "id": id, "name": name, "folderId": 3, "items": [
            { "fieldName": "Server", "slug": "server", "itemValue": "db.example.com", "isFile": false, "isPassword": false },
            { "fieldName": "Username", "slug": "username", "itemValue": "app_user", "isFile": false, "isPassword": false },
            { "fieldName": "Password", "slug": "password", "itemValue": "pg-secret-1", "isFile": false, "isPassword": true },
            { "fieldName": "Key", "slug": "private-key", "itemValue": "", "isFile": true, "isPassword": false }
        ] })
    }

    #[derive(Default)]
    struct Seen {
        grants: Mutex<Vec<HashMap<String, String>>>,
    }

    async fn mock() -> (String, Arc<Seen>) {
        let seen = Arc::new(Seen::default());
        let ok = |h: &HeaderMap| {
            h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer at-1")
        };
        let app = Router::new()
            .route(
                "/oauth2/token",
                post(|State(s): State<Arc<Seen>>, Form(f): Form<HashMap<String, String>>| async move {
                    s.grants.lock().unwrap().push(f.clone());
                    let good = f.get("grant_type").map(String::as_str) == Some("password")
                        && f.get("username").map(String::as_str) == Some("svc_dbine")
                        && f.get("password").map(String::as_str) == Some("dummy-pass");
                    if good {
                        (StatusCode::OK, Json(json!({ "access_token": "at-1", "token_type": "bearer", "expires_in": 1199 })))
                    } else {
                        (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid_grant" })))
                    }
                }),
            )
            .route(
                "/identity/api/oauth2/token/xpmplatform",
                post(|Form(f): Form<HashMap<String, String>>| async move {
                    let good = f.get("grant_type").map(String::as_str) == Some("client_credentials")
                        && f.get("client_id").map(String::as_str) == Some("client-1")
                        && f.get("client_secret").map(String::as_str) == Some("dummy-pass")
                        && f.get("scope").map(String::as_str) == Some("xpmheadless");
                    if good { (StatusCode::OK, Json(json!({ "access_token": "at-1", "expires_in": 3600 }))) } else { (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid_client" }))) }
                }),
            )
            .route(
                "/api/v1/secrets",
                get(move |h: HeaderMap, Query(q): Query<HashMap<String, String>>| async move {
                    if !ok(&h) {
                        return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "Authentication failed." })));
                    }
                    let text = q.get("filter.searchText").cloned().unwrap_or_default();
                    let all = [(41, "pg-prod"), (42, "pg-prod-old"), (51, "twin"), (52, "twin")];
                    let records: Vec<Value> = all.iter().filter(|(_, n)| n.contains(text.as_str())).map(|(i, n)| json!({ "id": i, "name": n })).collect();
                    (StatusCode::OK, Json(json!({ "records": records, "total": records.len() })))
                }),
            )
            .route(
                "/api/v1/users/current",
                get(move |h: HeaderMap| async move {
                    if !ok(&h) {
                        return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "Authentication failed." })));
                    }
                    (StatusCode::OK, Json(json!({ "id": 7, "userName": "svc_dbine", "displayName": "Service DBine", "emailAddress": "svc@example.com" })))
                }),
            )
            .route(
                "/api/v1/secrets/{id}/summary",
                get(move |h: HeaderMap, AxPath(id): AxPath<u64>| async move {
                    if !ok(&h) || id != 41 {
                        return (StatusCode::NOT_FOUND, Json(json!({ "message": "Secret not found" })));
                    }
                    (StatusCode::OK, Json(json!({ "id": 41, "name": "pg-prod", "secretTemplateId": 6001 })))
                }),
            )
            .route(
                "/api/v1/secret-templates/{id}",
                get(move |h: HeaderMap, AxPath(id): AxPath<u64>| async move {
                    if !ok(&h) || id != 6001 {
                        return (StatusCode::NOT_FOUND, Json(json!({ "message": "Template not found" })));
                    }
                    (StatusCode::OK, Json(json!({ "id": 6001, "name": "SQL Server Account", "fields": [
                        { "fieldSlugName": "server", "displayName": "Server", "isFile": false },
                        { "fieldSlugName": "username", "displayName": "Username", "isFile": false },
                        { "fieldSlugName": "password", "displayName": "Password", "isFile": false, "isPassword": true },
                        { "fieldSlugName": "private-key", "displayName": "Key", "isFile": true }
                    ] })))
                }),
            )
            .route(
                "/api/v1/secrets/{id}",
                get(move |h: HeaderMap, AxPath(id): AxPath<u64>| async move {
                    if !ok(&h) {
                        return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "Authentication failed." })));
                    }
                    match id {
                        41 => (StatusCode::OK, Json(secret_json(41, "pg-prod"))),
                        43 => (StatusCode::BAD_REQUEST, Json(json!({ "errorCode": "API_CommentRequired", "message": "A comment is required." }))),
                        44 => (StatusCode::FORBIDDEN, Json(json!({ "errorCode": "API_AccessDenied", "message": "Access Denied" }))),
                        _ => (StatusCode::NOT_FOUND, Json(json!({ "message": "Secret not found" }))),
                    }
                }),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), seen)
    }

    fn r<'a>(url: &'a str, secret: &'a str, field: &'a str, fill: bool) -> Request<'a> {
        Request {
            url,
            platform_url: "",
            username: "svc_dbine",
            domain: "",
            secret,
            field,
            fill_username: fill,
            user_field: "",
        }
    }

    /// Against a mock of Secret Server's API (no Delinea tenant was
    /// available): the password grant (with a domain), the platform's
    /// client credentials, by ID and by name, ambiguity, fields, errors.
    #[tokio::test]
    async fn against_a_mock_of_the_api() {
        let (url, seen) = mock().await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let pw = "dummy-pass";
        assert_eq!(
            read_with(&client, &r(&url, "41", "", true), pw)
                .await
                .unwrap(),
            ("pg-secret-1".into(), Some("app_user".into()))
        );
        assert_eq!(
            read_with(&client, &r(&url, "pg-prod", "", false), pw)
                .await
                .unwrap(),
            ("pg-secret-1".into(), None),
            "by name, exact"
        );
        assert_eq!(
            read_with(&client, &r(&url, "PG-PROD-old", "server", false), pw)
                .await
                .unwrap_err(),
            "Secret Server: no hay un secreto «PG-PROD-old» (o el usuario no tiene acceso)",
            "the mock's search is case sensitive"
        );
        assert_eq!(
            read_with(&client, &r(&url, "41", "Server", false), pw)
                .await
                .unwrap()
                .0,
            "db.example.com",
            "a field by name"
        );
        let mut other_user = r(&url, "41", "", true);
        other_user.user_field = "server";
        assert_eq!(
            read_with(&client, &other_user, pw)
                .await
                .unwrap()
                .1
                .as_deref(),
            Some("db.example.com"),
            "the user from another slug"
        );
        let e = read_with(&client, &r(&url, "twin", "", false), pw)
            .await
            .unwrap_err();
        assert!(
            e.contains("2 secretos «twin»") && e.contains("51") && e.contains("52"),
            "{e}"
        );
        let e = read_with(&client, &r(&url, "41", "pin", false), pw)
            .await
            .unwrap_err();
        assert!(
            e.contains("no tiene el campo «pin»")
                && e.contains("password")
                && !e.contains("pg-secret"),
            "{e}"
        );
        assert!(read_with(&client, &r(&url, "41", "private-key", false), pw)
            .await
            .unwrap_err()
            .contains("es un archivo"));
        assert!(read_with(&client, &r(&url, "43", "", false), pw)
            .await
            .unwrap_err()
            .contains("API_CommentRequired"));
        assert!(read_with(&client, &r(&url, "44", "", false), pw)
            .await
            .unwrap_err()
            .contains("permiso denegado"));
        assert_eq!(
            read_with(&client, &r(&url, "41", "", false), "nope")
                .await
                .unwrap_err(),
            WRONG_PASSWORD
        );
        // The domain goes with the grant.
        let mut with_domain = r(&url, "41", "", false);
        with_domain.domain = "CORP";
        let _ = read_with(&client, &with_domain, pw).await;
        assert_eq!(
            seen.grants
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .get("domain")
                .map(String::as_str),
            Some("CORP")
        );
        // Delinea Platform: the client credentials at the tenant.
        let mut platform = r(&url, "41", "", true);
        platform.platform_url = &url;
        platform.username = "client-1";
        assert_eq!(
            read_with(&client, &platform, pw)
                .await
                .unwrap()
                .1
                .as_deref(),
            Some("app_user")
        );
        assert_eq!(
            read_with(&client, &platform, "nope").await.unwrap_err(),
            WRONG_PASSWORD
        );
    }

    /// Browsing: a search (repeated names by ID), then a secret's field
    /// slugs from its template, never a value.
    #[tokio::test]
    async fn browse_against_the_mock() {
        let (url, _) = mock().await;
        let client = reqwest::Client::new();
        let l = browse_with(
            &client,
            &r(&url, "", "", false),
            "dummy-pass",
            Default::default(),
        )
        .await
        .unwrap();
        assert!(l.search && l.complete);
        assert_eq!(
            l.entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["pg-prod", "pg-prod-old", "51", "52"]
        );
        let at = crate::secret_sources::browse::At {
            query: "pg",
            ..Default::default()
        };
        assert_eq!(
            browse_with(&client, &r(&url, "", "", false), "dummy-pass", at)
                .await
                .unwrap()
                .entries
                .len(),
            2
        );
        let at = crate::secret_sources::browse::At {
            item: Some("pg-prod"),
            ..Default::default()
        };
        let l = browse_with(&client, &r(&url, "", "", false), "dummy-pass", at)
            .await
            .unwrap();
        assert_eq!(
            l.fields.as_deref().unwrap(),
            ["server", "username", "password"]
        );
        assert!(!serde_json::to_string(&l).unwrap().contains("pg-secret"));
        assert_eq!(
            browse_with(&client, &r(&url, "", "", false), "nope", Default::default())
                .await
                .unwrap_err(),
            WRONG_PASSWORD
        );
    }

    #[test]
    fn errors() {
        assert_eq!(
            token_error(400, &json!({"error": "invalid_grant"})),
            WRONG_PASSWORD
        );
        assert_eq!(
            token_error(500, &Value::Null),
            "Delinea respondió 500 al pedir el token de acceso"
        );
        assert!(
            api_error(400, &json!({"errorCode": "API_DoubleLockPasswordRequired"}))
                .contains("un paso más")
        );
        assert_eq!(
            api_error(500, &json!({"message": "boom"})),
            "Secret Server respondió 500: boom"
        );
    }

    /// The real client refuses plain HTTP to another computer and a
    /// platform URL that isn't a delinea.app tenant.
    #[tokio::test]
    async fn only_https_and_the_tenant() {
        let e = read(&r("http://ss.example.com", "41", "", false), "x")
            .await
            .unwrap_err();
        assert!(e.contains("https://"), "{e}");
        let mut p = r("https://ss.example.com", "41", "", false);
        p.platform_url = "https://evil.example.com";
        assert!(read(&p, "x").await.unwrap_err().contains("delinea.app"));
    }

    /// The account wizard's check: the token, then the current user.
    #[tokio::test]
    async fn whoami_against_the_mock() {
        let (url, _) = mock().await;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        assert_eq!(
            whoami_with(&client, &r(&url, "", "", false), "dummy-pass")
                .await
                .unwrap(),
            "svc_dbine"
        );
        assert_eq!(
            whoami_with(&client, &r(&url, "", "", false), "nope")
                .await
                .unwrap_err(),
            WRONG_PASSWORD
        );
        let mut platform = r(&url, "", "", false);
        platform.platform_url = &url;
        platform.username = "client-1";
        assert_eq!(
            whoami_with(&client, &platform, "dummy-pass").await.unwrap(),
            "svc_dbine"
        );
        // The real client: HTTPS only, and only a delinea.app tenant.
        assert!(whoami(&r("http://ss.example.com", "", "", false), "x")
            .await
            .unwrap_err()
            .contains("https://"));
        let mut p = r("https://ss.example.com", "", "", false);
        p.platform_url = "https://evil.example.com";
        assert!(whoami(&p, "x").await.unwrap_err().contains("delinea.app"));
    }
}
