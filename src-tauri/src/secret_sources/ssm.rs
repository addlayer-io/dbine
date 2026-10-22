//! AWS Systems Manager Parameter Store: `GetParameter` with
//! `WithDecryption`, signed here with SigV4 (aws.rs) and the same credential
//! chain as AWS Secrets Manager and the AWS IAM sign-in (cloud_auth.rs).
//! SecureString, String and StringList (one element, chosen in the form).
//! Only the signature travels, to `ssm.<region>.amazonaws.com`, or to the
//! endpoint in `AWS_ENDPOINT_URL_SSM` / `AWS_ENDPOINT_URL` (the AWS tools'
//! standard variables, for LocalStack and the like).

use super::aws::sign;
use super::cloud_auth::{self, AwsCreds, AwsFiles};
use serde_json::Value;
use std::time::Duration;

const TARGET: &str = "AmazonSSM.GetParameter";
const CONTENT_TYPE: &str = "application/x-amz-json-1.1";

/// `arn:aws:ssm:<region>:…` → the region.
pub fn region_from_arn(name: &str) -> Option<&str> {
    let mut parts = name.split(':');
    (parts.next()? == "arn").then_some(())?;
    parts.next()?;
    (parts.next()? == "ssm").then_some(())?;
    parts
        .next()
        .filter(|r| dbine_core::secret_sources::valid_aws_region(r))
}

/// The regional endpoint's host.
pub fn host(region: &str) -> String {
    if region.starts_with("cn-") {
        format!("ssm.{region}.amazonaws.com.cn")
    } else {
        format!("ssm.{region}.amazonaws.com")
    }
}

/// `(URL, host to sign)` of an endpoint override: `http(s)://host[:port]`.
pub(super) fn endpoint_override(url: &str) -> Option<(String, String)> {
    let u = url.trim().trim_end_matches('/');
    let rest = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))?;
    (!rest.is_empty()
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']')))
    .then(|| (format!("{u}/"), rest.to_string()))
}

/// What the form asks for.
pub struct Request<'a> {
    pub parameter: &'a str,
    pub version: &'a str,
    pub region: &'a str,
    pub profile: &'a str,
    pub list_index: &'a str,
}

/// The parameter's value (one element of a StringList).
pub async fn get_parameter(req: &Request<'_>) -> Result<String, String> {
    let (client, url, host, region, creds) =
        session(req.parameter, req.region, req.profile).await?;
    let (kind, value) = get_parameter_at(
        &client,
        &url,
        &host,
        &region,
        &creds,
        req.parameter.trim(),
        req.version.trim(),
    )
    .await?;
    pick(kind, value, req.list_index.trim(), req.parameter.trim())
}

/// The client, the endpoint (`(url, host)`), the region and the credentials.
async fn session(
    parameter: &str,
    region: &str,
    profile: &str,
) -> Result<(reqwest::Client, String, String, String, AwsCreds), String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let files = AwsFiles::load(&env);
    let profile_field = Some(profile.trim()).filter(|p| !p.is_empty());
    let profile_name = profile_field
        .map(str::to_string)
        .or_else(|| env("AWS_PROFILE"))
        .unwrap_or_else(|| "default".into());
    let region = Some(region.trim())
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .or_else(|| region_from_arn(parameter.trim()).map(str::to_string))
        .or_else(|| env("AWS_REGION"))
        .or_else(|| env("AWS_DEFAULT_REGION"))
        .or_else(|| files.get(&profile_name, "region"))
        .ok_or_else(|| {
            "falta la región de AWS: indicala en el formulario (o usá el ARN del parámetro)"
                .to_string()
        })?;
    if !dbine_core::secret_sources::valid_aws_region(&region) {
        return Err(format!("la región de AWS «{region}» no es válida"));
    }
    let (creds, _) =
        cloud_auth::aws_credentials(profile_field.is_some(), &profile_name, &files, &env)
            .await
            .map_err(|e| e.to_string())?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())?;
    let (url, host) = match env("AWS_ENDPOINT_URL_SSM").or_else(|| env("AWS_ENDPOINT_URL")) {
        Some(e) => {
            endpoint_override(&e).ok_or("AWS_ENDPOINT_URL no es una URL http(s)://host[:puerto]")?
        }
        None => {
            let h = host(&region);
            (format!("https://{h}/"), h)
        }
    };
    Ok((client, url, host, region, creds))
}

// ---- browse ------------------------------------------------------------------

/// The parameters' names and types (`DescribeParameters`: no values), or
/// a parameter's JSON keys (`item`).
pub async fn browse(
    region: &str,
    profile: &str,
    at: super::browse::At<'_>,
) -> Result<super::browse::Listing, String> {
    let (client, url, host, region, creds) =
        session(at.item.unwrap_or(""), region, profile).await?;
    browse_at(&client, &url, &host, &region, &creds, at).await
}

pub async fn browse_at(
    client: &reqwest::Client,
    url: &str,
    host: &str,
    region: &str,
    creds: &AwsCreds,
    at: super::browse::At<'_>,
) -> Result<super::browse::Listing, String> {
    use super::browse::{Entry, Listing, MAX_ENTRIES};
    if let Some(item) = at.item {
        let (kind, value) = get_parameter_at(client, url, host, region, creds, item, "").await?;
        // A StringList's elements aren't keys.
        let keys = if kind == "StringList" {
            let mut v = value;
            dbine_core::secret_sources::wipe(&mut v);
            Vec::new()
        } else {
            super::browse::json_keys(value)
        };
        return Ok(Listing::fields(keys));
    }
    let mut out = Vec::new();
    let mut next: Option<String> = None;
    loop {
        let mut q = serde_json::json!({ "MaxResults": 50 });
        if let Some(n) = &next {
            q["NextToken"] = Value::String(n.clone());
        }
        let body = serde_json::to_vec(&q).map_err(|e| e.to_string())?;
        let headers = sign(
            "POST",
            host,
            "/",
            &[
                ("content-type", CONTENT_TYPE),
                ("x-amz-target", "AmazonSSM.DescribeParameters"),
            ],
            &body,
            creds,
            region,
            "ssm",
            cloud_auth::now(),
        );
        let mut r = client.post(url).body(body);
        for (k, v) in &headers {
            r = r.header(k.as_str(), v.as_str());
        }
        let resp = r.send().await.map_err(|e| {
            format!(
                "no se pudo conectar con AWS Systems Manager: {}",
                e.without_url()
            )
        })?;
        let status = resp.status().as_u16();
        let json: Value =
            serde_json::from_str(&resp.text().await.unwrap_or_default()).unwrap_or(Value::Null);
        if status != 200 {
            return Err(ssm_error(status, &json, ""));
        }
        for p in json
            .get("Parameters")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(name) = p.get("Name").and_then(Value::as_str) {
                out.push(Entry::named(
                    name,
                    p.get("Type").and_then(Value::as_str).unwrap_or(""),
                ));
            }
        }
        next = json
            .get("NextToken")
            .and_then(Value::as_str)
            .map(str::to_string);
        if next.is_none() || out.len() > MAX_ENTRIES {
            break;
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Listing::complete(out))
}

/// `GetParameter` at `url`: `(Type, Value)`.
pub async fn get_parameter_at(
    client: &reqwest::Client,
    url: &str,
    host: &str,
    region: &str,
    creds: &AwsCreds,
    name: &str,
    version: &str,
) -> Result<(String, String), String> {
    let selector = if version.is_empty() {
        name.to_string()
    } else {
        format!("{name}:{version}")
    };
    let body = serde_json::to_vec(&serde_json::json!({ "Name": selector, "WithDecryption": true }))
        .map_err(|e| e.to_string())?;
    let headers = sign(
        "POST",
        host,
        "/",
        &[("content-type", CONTENT_TYPE), ("x-amz-target", TARGET)],
        &body,
        creds,
        region,
        "ssm",
        cloud_auth::now(),
    );
    let mut r = client.post(url).body(body);
    for (k, v) in &headers {
        r = r.header(k.as_str(), v.as_str());
    }
    let resp = r.send().await.map_err(|e| {
        format!(
            "no se pudo conectar con AWS Systems Manager: {}",
            e.without_url()
        )
    })?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status != 200 {
        return Err(ssm_error(status, &json, &selector));
    }
    let p = json
        .get("Parameter")
        .ok_or_else(|| format!("AWS Systems Manager no devolvió el parámetro «{selector}»"))?;
    let kind = p
        .get("Type")
        .and_then(Value::as_str)
        .unwrap_or("String")
        .to_string();
    let value = p
        .get("Value")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("AWS Systems Manager no devolvió un valor para «{selector}»"))?;
    Ok((kind, value.to_string()))
}

/// The value: as it is, or one element (from 1) of a StringList.
pub fn pick(kind: String, mut value: String, index: &str, name: &str) -> Result<String, String> {
    if kind != "StringList" {
        if !index.is_empty() {
            dbine_core::secret_sources::wipe(&mut value);
            return Err(format!("el parámetro «{name}» es {kind}, no una StringList: dejá vacío el elemento de la lista"));
        }
        return Ok(value);
    }
    let n = value.split(',').count();
    let out = match index.parse::<usize>() {
        Ok(i) if (1..=n).contains(&i) => Ok(value.split(',').nth(i - 1).unwrap_or("").to_string()),
        Ok(i) => Err(format!("el parámetro «{name}» tiene {n} elementos: el {i} no existe")),
        Err(_) => Err(format!("el parámetro «{name}» es una StringList de {n} elementos: indicá cuál usar (de 1 a {n})")),
    };
    dbine_core::secret_sources::wipe(&mut value);
    out
}

fn ssm_error(status: u16, json: &Value, name: &str) -> String {
    let kind = json.get("__type").and_then(Value::as_str).unwrap_or("");
    let kind = kind.rsplit('#').next().unwrap_or(kind);
    let msg = json
        .get("message")
        .or_else(|| json.get("Message"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let msg = super::exec::first_lines(msg, 300);
    match kind {
        "ParameterNotFound" => {
            format!("AWS Systems Manager: no existe el parámetro «{name}» en esa región")
        }
        "ParameterVersionNotFound" => format!(
            "AWS Systems Manager: el parámetro no tiene la versión o etiqueta pedida («{name}»)"
        ),
        "AccessDeniedException" if name.is_empty() => {
            format!("AWS Systems Manager: permiso denegado (falta ssm:DescribeParameters): {msg}")
        }
        "AccessDeniedException" => format!(
            "AWS Systems Manager: permiso denegado (falta ssm:GetParameter o kms:Decrypt): {msg}"
        ),
        "UnrecognizedClientException"
        | "InvalidSignatureException"
        | "ExpiredTokenException"
        | "IncompleteSignature"
        | "InvalidClientTokenId" => {
            format!("AWS rechazó las credenciales (¿vencieron? probá «aws sso login»): {msg}")
        }
        "InvalidKeyId" | "KMSAccessDeniedException" => {
            format!("AWS no pudo descifrar el parámetro con su clave KMS: {msg}")
        }
        "ValidationException" => format!("AWS Systems Manager: {msg}"),
        _ if msg.is_empty() => format!("AWS Systems Manager respondió {status}"),
        _ => format!("AWS Systems Manager respondió {status}: {msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_hosts_and_endpoints() {
        assert_eq!(
            region_from_arn("arn:aws:ssm:eu-west-1:123456789012:parameter/prod/db"),
            Some("eu-west-1")
        );
        assert_eq!(region_from_arn("/prod/db"), None);
        assert_eq!(
            region_from_arn("arn:aws:secretsmanager:eu-west-1:1:secret:x"),
            None
        );
        assert_eq!(host("us-east-1"), "ssm.us-east-1.amazonaws.com");
        assert_eq!(host("cn-north-1"), "ssm.cn-north-1.amazonaws.com.cn");
        assert_eq!(
            endpoint_override("http://127.0.0.1:24566/"),
            Some(("http://127.0.0.1:24566/".into(), "127.0.0.1:24566".into()))
        );
        assert_eq!(endpoint_override("ftp://x"), None);
        assert_eq!(endpoint_override("https://a@b"), None);
        assert_eq!(endpoint_override("https://h/path?x"), None);
    }

    #[test]
    fn string_lists() {
        let p = |kind: &str, v: &str, i: &str| pick(kind.into(), v.into(), i, "/p");
        assert_eq!(p("SecureString", "pw", "").unwrap(), "pw");
        assert_eq!(
            p("String", "a,b", "").unwrap(),
            "a,b",
            "only a StringList is split"
        );
        assert!(p("String", "a,b", "1")
            .unwrap_err()
            .contains("no una StringList"));
        assert_eq!(p("StringList", "a,b,c", "2").unwrap(), "b");
        let e = p("StringList", "a,secret-b,c", "").unwrap_err();
        assert!(e.contains("3 elementos") && !e.contains("secret-b"), "{e}");
        assert!(p("StringList", "a,b", "3")
            .unwrap_err()
            .contains("el 3 no existe"));
    }

    #[test]
    fn errors() {
        let e = ssm_error(
            400,
            &serde_json::json!({"__type": "ParameterNotFound", "message": ""}),
            "/prod/db",
        );
        assert!(e.contains("no existe el parámetro «/prod/db»"), "{e}");
        let e = ssm_error(
            400,
            &serde_json::json!({"__type": "com.amazonaws#AccessDeniedException", "Message": "User is not authorized"}),
            "x",
        );
        assert!(e.contains("permiso denegado") && e.contains("User is not authorized"));
        assert_eq!(
            ssm_error(500, &Value::Null, "x"),
            "AWS Systems Manager respondió 500"
        );
    }

    /// The request is SigV4-signed for the `ssm` service, with the target.
    #[test]
    fn signed_for_ssm() {
        let c = AwsCreds {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "secret".into(),
            session_token: None,
            expires_on: None,
        };
        let h = sign(
            "POST",
            "ssm.eu-west-1.amazonaws.com",
            "/",
            &[("content-type", CONTENT_TYPE), ("x-amz-target", TARGET)],
            b"{}",
            &c,
            "eu-west-1",
            "ssm",
            1_440_938_160,
        );
        let auth = &h.last().unwrap().1;
        assert!(
            auth.contains("/20150830/eu-west-1/ssm/aws4_request")
                && auth.contains("SignedHeaders=content-type;host;x-amz-date;x-amz-target"),
            "{auth}"
        );
        assert!(h.iter().any(|(k, v)| k == "x-amz-target" && v == TARGET));
    }

    /// Against LocalStack as `dbine-test-localstack` (port 24566) with
    /// `/dbine/pg` (SecureString `{"username":"postgres","password":"pw"}`),
    /// `/dbine/plain` (String, versions 1 and 2, label `current` on 2) and
    /// `/dbine/list` (StringList `a,pw,c`); see docs/secret-sources.md. Then,
    /// through `fill` as a connection does (`AWS_ENDPOINT_URL_SSM`) and with
    /// `DBINE_TEST_PG_SECRETS`, a real session on `dbine-test-postgres`:
    /// `DBINE_TEST_LOCALSTACK=http://127.0.0.1:24566 cargo test -p dbine secret_sources::ssm -- --ignored --test-threads=1`.
    #[tokio::test]
    #[ignore]
    async fn live_localstack() {
        use dbine_core::secret_sources::{pick_json, SecretSource, SecretSources};
        let Ok(url) = std::env::var("DBINE_TEST_LOCALSTACK") else {
            return;
        };
        let _ = rustls::crypto::ring::default_provider().install_default();
        let creds = AwsCreds {
            access_key_id: "test".into(),
            secret_access_key: "test".into(),
            session_token: None,
            expires_on: None,
        };
        let client = reqwest::Client::new();
        let (u, host) = endpoint_override(&url).unwrap();
        let get = |name: &'static str, version: &'static str| {
            let (client, u, host, creds) = (&client, &u, &host, &creds);
            async move { get_parameter_at(client, u, host, "us-east-1", creds, name, version).await }
        };
        let (kind, raw) = get("/dbine/pg", "").await.unwrap();
        assert_eq!(kind, "SecureString");
        assert_eq!(pick_json(&raw, "password").unwrap(), "pw");
        assert_eq!(get("/dbine/plain", "1").await.unwrap().1, "plain-v1");
        assert_eq!(
            get("/dbine/plain", "current").await.unwrap().1,
            "plain-v2",
            "a label"
        );
        assert_eq!(get("/dbine/plain", "").await.unwrap().1, "plain-v2");
        let (kind, list) = get("/dbine/list", "").await.unwrap();
        assert_eq!(pick(kind, list, "2", "/dbine/list").unwrap(), "pw");
        assert!(get("/dbine/none", "")
            .await
            .unwrap_err()
            .contains("no existe el parámetro"));
        assert!(get("/dbine/plain", "9").await.is_err(), "no such version");
        // Browsing: names and types, then a parameter's JSON keys.
        let l = browse_at(&client, &u, &host, "us-east-1", &creds, Default::default())
            .await
            .unwrap();
        assert!(
            l.entries.iter().any(|e| e.id == "/dbine/pg"
                && e.name == "pg"
                && e.path == "dbine"
                && e.detail == "SecureString"),
            "{:?}",
            l.entries
        );
        let at = super::super::browse::At {
            item: Some("/dbine/pg"),
            ..Default::default()
        };
        assert_eq!(
            browse_at(&client, &u, &host, "us-east-1", &creds, at)
                .await
                .unwrap()
                .fields
                .unwrap(),
            ["password", "username"]
        );
        let at = super::super::browse::At {
            item: Some("/dbine/list"),
            ..Default::default()
        };
        assert!(
            browse_at(&client, &u, &host, "us-east-1", &creds, at)
                .await
                .unwrap()
                .fields
                .unwrap()
                .is_empty(),
            "a StringList has no keys"
        );
        // As a connection does: fill, with the endpoint and keys from the environment.
        // This ignored test runs alone (`--test-threads=1`).
        std::env::set_var("AWS_ENDPOINT_URL_SSM", &url);
        std::env::set_var("AWS_ACCESS_KEY_ID", "test");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "test");
        let dir = super::super::tests::temp_dir("ssm-live");
        let store = dbine_core::StateStore::open(&dir.join("s.sqlite")).unwrap();
        let src = SecretSource::Ssm {
            parameter: "/dbine/pg".into(),
            version: String::new(),
            region: "us-east-1".into(),
            profile: String::new(),
            list_index: String::new(),
            json_key: "password".into(),
            fill_username: true,
            secret_account: String::new(),
        };
        let mut cfg = dbine_driver::ConnectionConfig {
            driver: "postgres".into(),
            host: "127.0.0.1".into(),
            port: 25010,
            database: "postgres".into(),
            username: Some("someone-else".into()),
            ..Default::default()
        };
        let sources: SecretSources = [("password".to_string(), src)].into();
        super::super::fill(
            &store,
            &mut cfg,
            &sources,
            &Default::default(),
            super::super::Origin::Form {
                id: None,
                duplicate_of: None,
                name: "",
            },
        )
        .await
        .unwrap();
        assert_eq!(
            (cfg.password.as_deref(), cfg.username.as_deref()),
            (Some("pw"), Some("postgres"))
        );
        let listed = SecretSource::Ssm {
            parameter: "/dbine/list".into(),
            version: String::new(),
            region: "us-east-1".into(),
            profile: String::new(),
            list_index: "2".into(),
            json_key: String::new(),
            fill_username: false,
            secret_account: String::new(),
        };
        assert_eq!(
            super::super::fetch(&listed, &Default::default())
                .await
                .unwrap()
                .value,
            "pw"
        );
        if std::env::var("DBINE_TEST_PG_SECRETS").is_ok() {
            let mut s = dbine_drivers::open_session(&cfg, None)
                .await
                .unwrap_or_else(|e| panic!("connect: {e}"));
            eprintln!("ssm: connected, {}", s.server_version().await.unwrap());
        }
        std::env::remove_var("AWS_ENDPOINT_URL_SSM");
    }
}
