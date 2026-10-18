//! AWS Secrets Manager: `GetSecretValue`, signed here with SigV4 using the
//! same credential chain as the AWS IAM sign-in (cloud_auth.rs: environment
//! keys, the profile's keys, `credential_process`, the AWS CLI for SSO and
//! roles). Only the signature travels; the endpoint is AWS's for the region.

use super::cloud_auth::{self, amz_date, hex, hmac, sha256, AwsCreds, AwsFiles};
use base64::Engine;
use serde_json::Value;
use std::time::Duration;

const TARGET: &str = "secretsmanager.GetSecretValue";
const CONTENT_TYPE: &str = "application/x-amz-json-1.1";

/// `arn:aws:secretsmanager:<region>:…` → the region.
pub fn region_from_arn(id: &str) -> Option<&str> {
    let mut parts = id.split(':');
    (parts.next()? == "arn").then_some(())?;
    parts.next()?;
    (parts.next()? == "secretsmanager").then_some(())?;
    parts.next().filter(|r| dbine_core::secret_sources::valid_aws_region(r))
}

/// The regional endpoint's host.
pub fn host(region: &str) -> String {
    if region.starts_with("cn-") {
        format!("secretsmanager.{region}.amazonaws.com.cn")
    } else {
        format!("secretsmanager.{region}.amazonaws.com")
    }
}

/// SigV4 headers of a request: `(name, value)` to send, `Authorization`
/// last. `headers` are the ones to sign besides `host` and `x-amz-date`.
#[allow(clippy::too_many_arguments)]
pub fn sign(method: &str, host: &str, path: &str, headers: &[(&str, &str)], body: &[u8], c: &AwsCreds, region: &str, service: &str, unix: u64) -> Vec<(String, String)> {
    let (stamp, date) = amz_date(unix);
    let mut signed: Vec<(String, String)> = vec![("host".into(), host.to_string()), ("x-amz-date".into(), stamp.clone())];
    for (k, v) in headers {
        signed.push((k.to_ascii_lowercase(), v.trim().to_string()));
    }
    if let Some(t) = &c.session_token {
        signed.push(("x-amz-security-token".into(), t.clone()));
    }
    signed.sort();
    let canonical_headers: String = signed.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed_names = signed.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(";");
    let canonical = format!("{method}\n{path}\n\n{canonical_headers}\n{signed_names}\n{}", hex(&sha256(&[body])));
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!("AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}", hex(&sha256(&[canonical.as_bytes()])));
    let mut key = hmac(format!("AWS4{}", c.secret_access_key).as_bytes(), date.as_bytes());
    for part in [region, service, "aws4_request"] {
        key = hmac(&key, part.as_bytes());
    }
    let signature = hex(&hmac(&key, to_sign.as_bytes()));
    let mut out: Vec<(String, String)> = signed.into_iter().filter(|(k, _)| k != "host").collect();
    out.push(("authorization".into(), format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_names}, Signature={signature}", c.access_key_id)));
    out
}

/// The secret's value (`SecretString`, or `SecretBinary` as UTF-8).
pub async fn get_secret(secret_id: &str, region: &str, profile: &str) -> Result<String, String> {
    let (client, region, creds) = session(secret_id, region, profile).await?;
    let host = host(&region);
    get_secret_at(&client, &format!("https://{host}/"), &host, &region, &creds, secret_id.trim()).await
}

/// The client, the region and the credentials (the region: the form's,
/// the ARN's, the environment's or the profile's).
async fn session(secret_id: &str, region: &str, profile: &str) -> Result<(reqwest::Client, String, AwsCreds), String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let files = AwsFiles::load(&env);
    let profile_field = Some(profile.trim()).filter(|p| !p.is_empty());
    let profile_name = profile_field.map(str::to_string).or_else(|| env("AWS_PROFILE")).unwrap_or_else(|| "default".into());
    let region = Some(region.trim())
        .filter(|r| !r.is_empty())
        .map(str::to_string)
        .or_else(|| region_from_arn(secret_id.trim()).map(str::to_string))
        .or_else(|| env("AWS_REGION"))
        .or_else(|| env("AWS_DEFAULT_REGION"))
        .or_else(|| files.get(&profile_name, "region"))
        .ok_or_else(|| "falta la región de AWS: indicala en el formulario (o usá el ARN del secreto)".to_string())?;
    if !dbine_core::secret_sources::valid_aws_region(&region) {
        return Err(format!("la región de AWS «{region}» no es válida"));
    }
    let (creds, _) = cloud_auth::aws_credentials(profile_field.is_some(), &profile_name, &files, &env).await.map_err(|e| e.to_string())?;
    let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(20)).build().map_err(|e| e.to_string())?;
    Ok((client, region, creds))
}

// ---- browse ------------------------------------------------------------------

/// The secrets' names (ListSecrets), or a secret's JSON keys (`item`).
pub async fn browse(region: &str, profile: &str, at: super::browse::At<'_>) -> Result<super::browse::Listing, String> {
    let (client, region, creds) = session(at.item.unwrap_or(""), region, profile).await?;
    let host = host(&region);
    browse_at(&client, &format!("https://{host}/"), &host, &region, &creds, at).await
}

pub async fn browse_at(client: &reqwest::Client, url: &str, host: &str, region: &str, creds: &AwsCreds, at: super::browse::At<'_>) -> Result<super::browse::Listing, String> {
    use super::browse::{Entry, Listing, MAX_ENTRIES};
    if let Some(item) = at.item {
        let raw = get_secret_at(client, url, host, region, creds, item).await?;
        return Ok(Listing::fields(super::browse::json_keys(raw)));
    }
    let mut out = Vec::new();
    let mut next: Option<String> = None;
    loop {
        let mut q = serde_json::json!({ "MaxResults": 100, "SortOrder": "asc" });
        if let Some(n) = &next {
            q["NextToken"] = Value::String(n.clone());
        }
        let json = call(client, url, host, region, creds, "secretsmanager.ListSecrets", &q).await.map_err(|(status, json)| aws_error(status, &json, ""))?;
        for s in json.get("SecretList").and_then(Value::as_array).into_iter().flatten() {
            if let Some(name) = s.get("Name").and_then(Value::as_str) {
                out.push(Entry::named(name, ""));
            }
        }
        next = json.get("NextToken").and_then(Value::as_str).map(str::to_string);
        if next.is_none() || out.len() > MAX_ENTRIES {
            break;
        }
    }
    Ok(Listing::complete(out))
}

/// One signed JSON call; the answer, or `(status, body)`.
async fn call(client: &reqwest::Client, url: &str, host: &str, region: &str, creds: &AwsCreds, target: &str, body: &Value) -> Result<Value, (u16, Value)> {
    let body = serde_json::to_vec(body).unwrap_or_default();
    let headers = sign("POST", host, "/", &[("content-type", CONTENT_TYPE), ("x-amz-target", target)], &body, creds, region, "secretsmanager", cloud_auth::now());
    let mut req = client.post(url).body(body);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.map_err(|e| (0, Value::String(format!("no se pudo conectar con AWS Secrets Manager: {}", e.without_url()))))?;
    let status = resp.status().as_u16();
    let json: Value = serde_json::from_str(&resp.text().await.unwrap_or_default()).unwrap_or(Value::Null);
    if status == 200 { Ok(json) } else { Err((status, json)) }
}

/// `GetSecretValue` at `url` (AWS's endpoint; an emulator in tests).
pub async fn get_secret_at(client: &reqwest::Client, url: &str, host: &str, region: &str, creds: &AwsCreds, secret_id: &str) -> Result<String, String> {
    let body = serde_json::to_vec(&serde_json::json!({ "SecretId": secret_id })).map_err(|e| e.to_string())?;
    let headers = sign("POST", host, "/", &[("content-type", CONTENT_TYPE), ("x-amz-target", TARGET)], &body, creds, region, "secretsmanager", cloud_auth::now());
    let mut req = client.post(url).body(body);
    for (k, v) in &headers {
        req = req.header(k.as_str(), v.as_str());
    }
    let resp = req.send().await.map_err(|e| format!("no se pudo conectar con AWS Secrets Manager: {}", e.without_url()))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status != 200 {
        return Err(aws_error(status, &json, secret_id));
    }
    if let Some(s) = json.get("SecretString").and_then(Value::as_str) {
        return Ok(s.to_string());
    }
    if let Some(b) = json.get("SecretBinary").and_then(Value::as_str) {
        let bytes = base64::engine::general_purpose::STANDARD.decode(b).map_err(|_| "el secreto binario de AWS no es base64 válido".to_string())?;
        return String::from_utf8(bytes).map_err(|_| "el secreto binario de AWS no es texto".to_string());
    }
    Err(format!("AWS Secrets Manager no devolvió un valor para «{secret_id}»"))
}

fn aws_error(status: u16, json: &Value, secret_id: &str) -> String {
    if let (0, Value::String(m)) = (status, json) {
        return m.clone();
    }
    let kind = json.get("__type").and_then(Value::as_str).unwrap_or("");
    let kind = kind.rsplit('#').next().unwrap_or(kind);
    let msg = json.get("message").or_else(|| json.get("Message")).and_then(Value::as_str).unwrap_or("");
    let msg = super::exec::first_lines(msg, 300);
    match kind {
        "ResourceNotFoundException" => format!("AWS Secrets Manager: no existe el secreto «{secret_id}» en esa región"),
        "AccessDeniedException" if secret_id.is_empty() => format!("AWS Secrets Manager: permiso denegado (falta secretsmanager:ListSecrets): {msg}"),
        "AccessDeniedException" => format!("AWS Secrets Manager: permiso denegado (falta secretsmanager:GetSecretValue o kms:Decrypt): {msg}"),
        "UnrecognizedClientException" | "InvalidSignatureException" | "ExpiredTokenException" | "IncompleteSignature" | "InvalidClientTokenId" => {
            format!("AWS rechazó las credenciales (¿vencieron? probá «aws sso login»): {msg}")
        }
        "DecryptionFailure" => format!("AWS no pudo descifrar el secreto con su clave KMS: {msg}"),
        "InvalidRequestException" => format!("AWS Secrets Manager: {msg}"),
        _ if msg.is_empty() => format!("AWS Secrets Manager respondió {status}"),
        _ => format!("AWS Secrets Manager respondió {status}: {msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> AwsCreds {
        AwsCreds {
            access_key_id: "AKIDEXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
            expires_on: None,
        }
    }

    /// AWS's SigV4 test suite, `get-vanilla` and `post-vanilla`
    /// (2015-08-30T12:36:00Z, us-east-1, service "service").
    #[test]
    fn sigv4_test_suite() {
        let unix = 1_440_938_160;
        let h = sign("GET", "example.amazonaws.com", "/", &[], b"", &example(), "us-east-1", "service", unix);
        assert_eq!(
            h.last().unwrap().1,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
        let h = sign("POST", "example.amazonaws.com", "/", &[], b"", &example(), "us-east-1", "service", unix);
        assert!(h.last().unwrap().1.ends_with("Signature=5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b"), "{:?}", h.last());
        assert!(h.iter().any(|(k, v)| k == "x-amz-date" && v == "20150830T123600Z"));
    }

    #[test]
    fn session_token_is_signed() {
        let mut c = example();
        c.session_token = Some("TOKEN".into());
        let h = sign("POST", "h", "/", &[("X-Amz-Target", TARGET), ("Content-Type", CONTENT_TYPE)], b"{}", &c, "eu-west-1", "secretsmanager", 1_440_938_160);
        let auth = &h.last().unwrap().1;
        assert!(auth.contains("SignedHeaders=content-type;host;x-amz-date;x-amz-security-token;x-amz-target"), "{auth}");
        assert!(h.iter().any(|(k, v)| k == "x-amz-security-token" && v == "TOKEN"));
        assert!(!h.iter().any(|(k, _)| k == "host"), "reqwest sets Host itself");
    }

    #[test]
    fn regions_and_hosts() {
        assert_eq!(region_from_arn("arn:aws:secretsmanager:eu-west-1:123456789012:secret:prod/db-AbCdEf"), Some("eu-west-1"));
        assert_eq!(region_from_arn("prod/db"), None);
        assert_eq!(region_from_arn("arn:aws:s3:::bucket"), None);
        assert_eq!(host("us-east-1"), "secretsmanager.us-east-1.amazonaws.com");
        assert_eq!(host("cn-north-1"), "secretsmanager.cn-north-1.amazonaws.com.cn");
    }

    #[test]
    fn errors() {
        let e = aws_error(400, &serde_json::json!({"__type": "ResourceNotFoundException", "message": "Secrets Manager can't find the specified secret."}), "prod/db");
        assert!(e.contains("no existe el secreto «prod/db»"));
        let e = aws_error(400, &serde_json::json!({"__type": "com.amazonaws#AccessDeniedException", "Message": "User is not authorized"}), "x");
        assert!(e.contains("permiso denegado") && e.contains("User is not authorized"));
    }

    /// Against LocalStack as `dbine-test-localstack` (port 24566), with a
    /// secret `dbine/pg` = `{"username":"postgres","password":"pw"}`:
    /// `DBINE_TEST_LOCALSTACK=http://127.0.0.1:24566 cargo test -p dbine secret_sources::aws -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_localstack() {
        let Ok(url) = std::env::var("DBINE_TEST_LOCALSTACK") else { return };
        let creds = AwsCreds { access_key_id: "test".into(), secret_access_key: "test".into(), session_token: None, expires_on: None };
        let client = reqwest::Client::new();
        let host = url.trim_start_matches("http://").trim_end_matches('/').to_string();
        let raw = get_secret_at(&client, &url, &host, "us-east-1", &creds, "dbine/pg").await.unwrap();
        assert_eq!(dbine_core::secret_sources::pick_json(&raw, "password").unwrap(), "pw");
        assert_eq!(dbine_core::secret_sources::pick_json(&raw, "username").unwrap(), "postgres");
        let e = get_secret_at(&client, &url, &host, "us-east-1", &creds, "dbine/none").await.unwrap_err();
        assert!(e.contains("no existe el secreto"), "{e}");
        // Browsing: the names, then the JSON keys of one, never its values.
        let l = browse_at(&client, &url, &host, "us-east-1", &creds, Default::default()).await.unwrap();
        assert!(l.entries.iter().any(|e| e.id == "dbine/pg" && e.name == "pg" && e.path == "dbine"), "{:?}", l.entries);
        let at = crate::secret_sources::browse::At { item: Some("dbine/pg"), ..Default::default() };
        let l = browse_at(&client, &url, &host, "us-east-1", &creds, at).await.unwrap();
        assert_eq!(l.fields.as_deref().unwrap(), ["password", "username"]);
    }
}
