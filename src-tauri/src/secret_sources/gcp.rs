//! Google Secret Manager: `versions/<v>:access`, with an access token from
//! the same chain as the Google Cloud IAM sign-in (cloud_auth.rs: gcloud,
//! the application default credentials, the metadata server). The endpoint
//! is Google's; only the project, secret and version come from the form.
//!
//! With a gcloud account (an e-mail, kept by the account or the definition)
//! the token is always that account's (`gcloud auth print-access-token
//! --account <e-mail>`), whatever gcloud's active account is; without one,
//! the chain as before. DBine never signs in itself: when that account has
//! no valid sign-in, the error says the command to run ([`no_sign_in`]).

use super::cloud_auth::{self, uri_encode};
use base64::Engine;
use serde_json::Value;
use std::time::Duration;

pub const ENDPOINT: &str = "https://secretmanager.googleapis.com";

/// An access token: the gcloud `account`'s, else the chain's (gcloud's
/// active account, the application default credentials, the metadata
/// server), with the e-mail when known.
pub(super) async fn token(account: &str) -> Result<(String, Option<String>), String> {
    let a = account.trim();
    if a.is_empty() {
        return cloud_auth::gcp_token(None, true)
            .await
            .map(|(t, _, e)| (t, e))
            .map_err(|e| e.to_string());
    }
    if !dbine_core::secret_sources::valid_gcloud_account(a) {
        return Err("la cuenta de gcloud tiene que ser un correo (ana@example.com)".into());
    }
    // `--account` and the e-mail go as two arguments (no shell).
    match cloud_auth::gcp_token(Some(a), true).await {
        Ok((t, _, _)) => Ok((t, Some(a.to_string()))),
        Err(e) => {
            let e = e.to_string();
            Err(if needs_sign_in(&e) { no_sign_in(a) } else { e })
        }
    }
}

/// gcloud has no valid sign-in for the account (a valid e-mail: it goes
/// into the command the user copies).
pub fn no_sign_in(account: &str) -> String {
    format!("gcloud no tiene una sesión válida con esa cuenta: iniciá sesión con «gcloud auth login {account}» y probá de nuevo")
}

/// What gcloud says when an account isn't signed in on this computer or
/// its credentials expired or were revoked.
const SIGN_IN_MARKERS: &[&str] = &[
    "does not have any valid credentials",
    "credentialed accounts",
    "no credentials",
    "not found in the credential store",
    "reauthentication",
    "re-authenticate",
    "reauth",
    "invalid_grant",
    "invalid_rapt",
    "token has been expired or revoked",
    "problem refreshing",
    "gcloud auth login",
    "please run",
];

/// Whether a token error (cloud_auth's chain) is gcloud asking for a
/// sign-in: only its own line, without the hint cloud_auth adds to every
/// CLI failure («(¿falta gcloud auth login?)»).
pub fn needs_sign_in(err: &str) -> bool {
    let line = err
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("- gcloud:"))
        .unwrap_or(if err.contains('\n') { "" } else { err });
    let line = line.trim();
    let line = line
        .strip_suffix("(¿falta gcloud auth login?)")
        .unwrap_or(line)
        .to_lowercase();
    SIGN_IN_MARKERS.iter().any(|m| line.contains(m))
}

pub async fn get_secret(
    project: &str,
    secret: &str,
    version: &str,
    account: &str,
) -> Result<String, String> {
    let (token, _) = token(account).await?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    get_secret_at(
        &client,
        ENDPOINT,
        project.trim(),
        secret.trim(),
        version.trim(),
        &token,
    )
    .await
}

/// What `gcloud config list` says (the wizard's starting point): the
/// active account and the project, and the accounts gcloud has signed in
/// (`gcloud auth list`, [`parse_auth_list`]). Nothing secret.
pub async fn current_config() -> Result<Value, String> {
    let out = cloud_auth::run_cli(
        "gcloud",
        &["config", "list", "--format", "json"],
        &[],
        &[],
        "gcloud auth login",
    )
    .await
    .map_err(|e| e.to_string())?;
    let json: Value = serde_json::from_str(&out)
        .map_err(|_| "«gcloud config list» no devolvió JSON".to_string())?;
    let s = |p: &str| {
        json.pointer(p)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let active = s("/core/account");
    let mut accounts = match cloud_auth::run_cli(
        "gcloud",
        &["auth", "list", "--format", "json"],
        &[],
        &[],
        "gcloud auth login",
    )
    .await
    {
        Ok(out) => parse_auth_list(&out).unwrap_or_default(),
        Err(_) => vec![],
    };
    if dbine_core::secret_sources::valid_gcloud_account(&active)
        && !accounts
            .iter()
            .any(|a| a.account.eq_ignore_ascii_case(&active))
    {
        accounts.insert(
            0,
            GcloudAccount {
                account: active.clone(),
                active: true,
            },
        );
    }
    Ok(
        serde_json::json!({ "account": active, "project": s("/core/project"), "accounts": accounts }),
    )
}

/// An account gcloud has signed in (the wizard's list): nothing secret.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GcloudAccount {
    pub account: String,
    pub active: bool,
}

/// At most this many accounts go to the wizard.
const MAX_ACCOUNTS: usize = 200;

/// `gcloud auth list --format json`: `[{"account": "…", "status":
/// "ACTIVE" | ""}]`; well-formed e-mails only, once each, the active first.
pub fn parse_auth_list(stdout: &str) -> Option<Vec<GcloudAccount>> {
    let json: Value = serde_json::from_str(stdout).ok()?;
    let mut out: Vec<GcloudAccount> = Vec::new();
    for a in json.as_array()? {
        let account = a
            .get("account")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if !dbine_core::secret_sources::valid_gcloud_account(account)
            || out.iter().any(|o| o.account.eq_ignore_ascii_case(account))
        {
            continue;
        }
        let active = a
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|s| s.eq_ignore_ascii_case("active"));
        out.push(GcloudAccount {
            account: account.to_string(),
            active,
        });
        if out.len() >= MAX_ACCOUNTS {
            break;
        }
    }
    out.sort_by_key(|a| !a.active);
    Some(out)
}

/// The account wizard's check: a token comes from the same place the
/// connections get it (the gcloud `account`'s; without one, the chain:
/// gcloud, the application default credentials, the metadata server);
/// who it's for, as far as that says.
pub async fn whoami(account: &str) -> Result<String, String> {
    let (mut token, email) = token(account).await?;
    dbine_core::secret_sources::wipe(&mut token);
    if let Some(e) = email {
        return Ok(e);
    }
    let account = cloud_auth::run_cli(
        "gcloud",
        &["config", "get-value", "account", "--quiet"],
        &[],
        &[],
        "gcloud auth login",
    )
    .await
    .map(|a| a.trim().to_string())
    .unwrap_or_default();
    Ok(if account.contains('@') {
        account
    } else {
        String::new()
    })
}

pub fn access_url(base: &str, project: &str, secret: &str, version: &str) -> String {
    let version = if version.is_empty() {
        "latest"
    } else {
        version
    };
    format!(
        "{}/v1/projects/{}/secrets/{}/versions/{}:access",
        base.trim_end_matches('/'),
        uri_encode(project),
        uri_encode(secret),
        uri_encode(version)
    )
}

pub async fn get_secret_at(
    client: &reqwest::Client,
    base: &str,
    project: &str,
    secret: &str,
    version: &str,
    token: &str,
) -> Result<String, String> {
    let resp = client
        .get(access_url(base, project, secret, version))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| {
            format!(
                "no se pudo conectar con Google Secret Manager: {}",
                e.without_url()
            )
        })?;
    let status = resp.status().as_u16();
    let json: Value =
        serde_json::from_str(&resp.text().await.unwrap_or_default()).unwrap_or(Value::Null);
    if status == 200 {
        let data = json
            .pointer("/payload/data")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("Google Secret Manager no devolvió un valor para «{secret}»"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| "el valor de Google Secret Manager no es base64 válido".to_string())?;
        return String::from_utf8(bytes)
            .map_err(|_| "el valor de Google Secret Manager no es texto".to_string());
    }
    let msg = json
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("");
    let msg = super::exec::first_lines(msg, 300);
    Err(match status {
        401 => format!("Google rechazó el token (¿«gcloud auth login»?): {msg}"),
        403 => format!("Google Secret Manager: permiso denegado (hace falta el rol «Secret Manager Secret Accessor»): {msg}"),
        404 => format!("Google Secret Manager: no existe el secreto «{secret}» (o esa versión) en el proyecto «{project}»"),
        _ if msg.is_empty() => format!("Google Secret Manager respondió {status}"),
        _ => format!("Google Secret Manager respondió {status}: {msg}"),
    })
}

// ---- browse ------------------------------------------------------------------

/// The project's secret names (paged), or a secret's JSON keys (`item`,
/// its latest version).
pub async fn browse(
    project: &str,
    account: &str,
    at: super::browse::At<'_>,
) -> Result<super::browse::Listing, String> {
    let (token, _) = token(account).await?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    browse_at(&client, ENDPOINT, project.trim(), &token, at).await
}

pub async fn browse_at(
    client: &reqwest::Client,
    base: &str,
    project: &str,
    token: &str,
    at: super::browse::At<'_>,
) -> Result<super::browse::Listing, String> {
    use super::browse::{Entry, Listing, MAX_ENTRIES};
    if let Some(item) = at.item {
        let raw = get_secret_at(client, base, project, item, "", token).await?;
        return Ok(Listing::fields(super::browse::json_keys(raw)));
    }
    let mut out = Vec::new();
    let mut page = String::new();
    loop {
        let mut url = format!(
            "{}/v1/projects/{}/secrets?pageSize=250",
            base.trim_end_matches('/'),
            uri_encode(project)
        );
        if !page.is_empty() {
            url.push_str(&format!("&pageToken={}", uri_encode(&page)));
        }
        let resp = client
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| {
                format!(
                    "no se pudo conectar con Google Secret Manager: {}",
                    e.without_url()
                )
            })?;
        let status = resp.status().as_u16();
        let json: Value =
            serde_json::from_str(&resp.text().await.unwrap_or_default()).unwrap_or(Value::Null);
        if status != 200 {
            let msg = super::exec::first_lines(
                json.pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or(""),
                300,
            );
            return Err(match status {
                401 => format!("Google rechazó el token (¿«gcloud auth login»?): {msg}"),
                403 => format!("Google Secret Manager: permiso denegado para listar los secretos (hace falta el rol «Secret Manager Viewer»): {msg}"),
                404 => format!("Google Secret Manager: no existe el proyecto «{project}»"),
                _ if msg.is_empty() => format!("Google Secret Manager respondió {status}"),
                _ => format!("Google Secret Manager respondió {status}: {msg}"),
            });
        }
        for s in json
            .get("secrets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(name) = s
                .get("name")
                .and_then(Value::as_str)
                .and_then(|n| n.rsplit('/').next())
            {
                out.push(Entry::item(name, name, "", ""));
            }
        }
        page = json
            .get("nextPageToken")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if page.is_empty() || out.len() > MAX_ENTRIES {
            break;
        }
    }
    out.sort_by_key(|e| e.name.to_lowercase());
    Ok(Listing::complete(out))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn gcloud_accounts_and_sign_in() {
        let list = r#"[{"account":"bob@example.com","status":""},{"account":"ana@example.com","status":"ACTIVE"},{"account":"--debug","status":""},{"account":"ANA@example.com","status":""},{"account":"dbine@p.iam.gserviceaccount.com"}]"#;
        let got = parse_auth_list(list).unwrap();
        let names: Vec<(&str, bool)> = got.iter().map(|a| (a.account.as_str(), a.active)).collect();
        assert_eq!(
            names,
            [
                ("ana@example.com", true),
                ("bob@example.com", false),
                ("dbine@p.iam.gserviceaccount.com", false)
            ]
        );
        assert_eq!(parse_auth_list("[]").unwrap(), []);
        assert!(parse_auth_list("not json").is_none());
        // gcloud's own line asks for a sign-in; cloud_auth's hint alone doesn't.
        let chain = |line: &str| {
            format!("no se pudo obtener un token de Google Cloud. Se probó:\n- gcloud: {line} (¿falta gcloud auth login?)")
        };
        assert!(needs_sign_in(&chain("(gcloud.auth.print-access-token) Your current active account [bob@example.com] does not have any valid credentials")));
        assert!(needs_sign_in(&chain("(gcloud.auth.print-access-token) There was a problem refreshing your current auth tokens: Reauthentication failed. Please run: $ gcloud auth login")));
        assert!(needs_sign_in(&chain(
            "(gcloud.auth.print-access-token) invalid_grant: Token has been expired or revoked."
        )));
        assert!(!needs_sign_in(&chain("terminó con exit status: 3")));
        assert!(!needs_sign_in("«gcloud» no está instalada"));
        assert_eq!(no_sign_in("bob@example.com"), "gcloud no tiene una sesión válida con esa cuenta: iniciá sesión con «gcloud auth login bob@example.com» y probá de nuevo");
    }

    #[test]
    fn urls() {
        assert_eq!(access_url(ENDPOINT, "my-proj", "db-pw", ""), "https://secretmanager.googleapis.com/v1/projects/my-proj/secrets/db-pw/versions/latest:access");
        assert_eq!(access_url(ENDPOINT, "my-proj", "db-pw", "3"), "https://secretmanager.googleapis.com/v1/projects/my-proj/secrets/db-pw/versions/3:access");
    }

    /// A one-route HTTP server: answers `body` with `status` to every
    /// request whose first line contains `path` and carries the bearer
    /// token, 404 otherwise.
    pub async fn mock(
        path: &'static str,
        token: &'static str,
        status: u16,
        body: String,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = listener.accept().await else {
                    return;
                };
                let body = body.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = s.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let first = req.lines().next().unwrap_or("");
                    let authorized = req
                        .lines()
                        .any(|l| l.eq_ignore_ascii_case(&format!("authorization: Bearer {token}")));
                    let (status, body) = if first.contains(path) && authorized {
                        (status, body)
                    } else {
                        (
                            404,
                            r#"{"error":{"code":404,"message":"not found"}}"#.to_string(),
                        )
                    };
                    let resp = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                    let _ = s.write_all(resp.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn responses() {
        let ok = serde_json::json!({ "name": "projects/1/secrets/db-pw/versions/3", "payload": { "data": base64::engine::general_purpose::STANDARD.encode("{\"password\":\"pw\"}") } }).to_string();
        let base = mock(
            "/v1/projects/my-proj/secrets/db-pw/versions/latest:access",
            "tok",
            200,
            ok,
        )
        .await;
        let client = reqwest::Client::new();
        let raw = get_secret_at(&client, &base, "my-proj", "db-pw", "", "tok")
            .await
            .unwrap();
        assert_eq!(
            dbine_core::secret_sources::pick_json(&raw, "password").unwrap(),
            "pw"
        );
        let e = get_secret_at(&client, &base, "my-proj", "other", "", "tok")
            .await
            .unwrap_err();
        assert!(e.contains("no existe el secreto «other»"), "{e}");
        let denied = mock("/", "tok", 403, r#"{"error":{"code":403,"message":"Permission 'secretmanager.versions.access' denied","status":"PERMISSION_DENIED"}}"#.into()).await;
        let e = get_secret_at(&client, &denied, "p", "s", "", "tok")
            .await
            .unwrap_err();
        assert!(
            e.contains("permiso denegado") && e.contains("secretmanager.versions.access"),
            "{e}"
        );
    }

    /// The whole path with a fake `gcloud` on PATH (its token) and the mock
    /// API: `DBINE_TEST_FAKE_GCLOUD=1 cargo test -p dbine secret_sources::gcp -- --ignored --test-threads=1`.
    #[tokio::test]
    async fn browse_names_then_keys() {
        let list = serde_json::json!({ "secrets": [{ "name": "projects/my-proj/secrets/db-pw" }, { "name": "projects/my-proj/secrets/api" }] }).to_string();
        let base = mock(
            "/v1/projects/my-proj/secrets?pageSize=250",
            "tok",
            200,
            list,
        )
        .await;
        let client = reqwest::Client::new();
        let l = browse_at(&client, &base, "my-proj", "tok", Default::default())
            .await
            .unwrap();
        assert_eq!(
            l.entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(),
            ["api", "db-pw"]
        );
        let ok = serde_json::json!({ "payload": { "data": base64::engine::general_purpose::STANDARD.encode("{\"password\":\"pw\",\"user\":\"u\"}") } }).to_string();
        let base = mock(
            "/v1/projects/my-proj/secrets/db-pw/versions/latest:access",
            "tok",
            200,
            ok,
        )
        .await;
        let at = super::super::browse::At {
            item: Some("db-pw"),
            ..Default::default()
        };
        let l = browse_at(&client, &base, "my-proj", "tok", at)
            .await
            .unwrap();
        assert_eq!(l.fields.as_deref().unwrap(), ["password", "user"]);
        assert!(!serde_json::to_string(&l).unwrap().contains("pw\""));
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore]
    async fn live_fake_gcloud() {
        if std::env::var("DBINE_TEST_FAKE_GCLOUD").is_err() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = super::super::tests::temp_dir("fake-gcloud");
        let gcloud = dir.join("gcloud");
        std::fs::write(&gcloud, "#!/bin/sh\n[ \"$1 $2\" = 'auth print-access-token' ] && { echo ya29.fake-token; exit 0; }\nexit 7\n").unwrap();
        std::fs::set_permissions(&gcloud, std::fs::Permissions::from_mode(0o755)).unwrap();
        let old = std::env::var_os("PATH").unwrap_or_default();
        let mut paths = vec![dir.clone()];
        paths.extend(std::env::split_paths(&old));
        std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
        let (token, _, _) = cloud_auth::gcp_token(None, true).await.unwrap();
        std::env::set_var("PATH", old);
        assert_eq!(token, "ya29.fake-token");
        let ok = serde_json::json!({ "payload": { "data": base64::engine::general_purpose::STANDARD.encode("pw") } }).to_string();
        let base = mock(
            "/v1/projects/p1/secrets/s1/versions/2:access",
            "ya29.fake-token",
            200,
            ok,
        )
        .await;
        assert_eq!(
            get_secret_at(&reqwest::Client::new(), &base, "p1", "s1", "2", &token)
                .await
                .unwrap(),
            "pw"
        );
    }
}
