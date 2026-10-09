//! DbGate: `~/.dbgate/connections.jsonl`, one connection per line. Secret
//! fields are `crypt:` + simple-encryptor output (AES-256-CBC with an
//! HMAC-SHA256, key = SHA-256 of the key text) under the key stored in
//! `~/.dbgate/.key`, itself encrypted with DbGate's built-in key.

use super::{color_of, home, parse_url, port_of, Candidate, Found};
use aes::cipher::{block_padding::Pkcs7, BlockModeDecrypt, KeyIvInit};
use base64::Engine;
use dbine_driver::{Error, Result};
use hmac::{KeyInit, Mac};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// DbGate's built-in key (`packages/api/src/utility/crypting.js`).
const DEFAULT_KEY: &str = "mQAUaXhavRGJDxDTXSCg7Ej0xMmGCrx6OKA07DIMBiDcYYkvkaXjTAzPUEHEHEf9";

pub fn default_location() -> Option<PathBuf> {
    let dir = home()?.join(".dbgate");
    dir.join("connections.jsonl").is_file().then_some(dir)
}

/// simple-encryptor's `decrypt`: hex HMAC (64) + hex IV (32) + base64 data,
/// holding a JSON value.
fn decrypt(key: &str, text: &str) -> Option<Value> {
    let k = Sha256::digest(key.as_bytes());
    let body = match text.get(..64).zip(text.get(64..)) {
        Some((mac_hex, rest)) => {
            let mut mac = <hmac::Hmac<Sha256> as KeyInit>::new_from_slice(&k).ok()?;
            mac.update(rest.as_bytes());
            match hex::decode(mac_hex).ok().map(|m| mac.verify_slice(&m)) {
                Some(Ok(())) => rest,
                // Written without an HMAC.
                _ => text,
            }
        }
        None => text,
    };
    let iv = hex::decode(body.get(..32)?).ok()?;
    let mut data = base64::engine::general_purpose::STANDARD.decode(body.get(32..)?).ok()?;
    let plain = cbc::Decryptor::<aes::Aes256>::new_from_slices(&k, &iv).ok()?.decrypt_padded::<Pkcs7>(&mut data).ok()?;
    serde_json::from_slice(plain).ok()
}

pub fn read(path: &Path) -> Result<Found> {
    let (dir, file) = if path.is_dir() { (path.to_path_buf(), path.join("connections.jsonl")) } else { (path.parent().unwrap_or(path).to_path_buf(), path.to_path_buf()) };
    let text = std::fs::read_to_string(&file).map_err(|e| Error::Query(format!("no se pudo leer {}: {e}", file.display())))?;
    let mut warnings = Vec::new();
    let key = match std::fs::read_to_string(dir.join(".key")) {
        Ok(k) => match decrypt(DEFAULT_KEY, k.trim()).and_then(|v| v.get("encryptionKey").and_then(|k| k.as_str()).map(String::from)) {
            Some(k) => Some(k),
            None => {
                warnings.push("No se pudo leer la clave de DbGate (.key): las contraseñas no se importan.".into());
                None
            }
        },
        // Old installs encrypted with the built-in key.
        Err(_) => Some(DEFAULT_KEY.to_string()),
    };
    let mut candidates = Vec::new();
    let mut unreadable = 0;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        // Opened without saving: DbGate doesn't list them either.
        if v.get("unsaved").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let mut secret = |field: &str| -> Option<String> {
            let raw = v.get(field)?.as_str()?.to_string();
            match raw.strip_prefix("crypt:") {
                None => Some(raw).filter(|s| !s.is_empty()),
                Some(enc) => match key.as_deref().and_then(|k| decrypt(k, enc)) {
                    Some(Value::String(s)) => Some(s).filter(|s| !s.is_empty()),
                    _ => {
                        unreadable += 1;
                        None
                    }
                },
            }
        };
        let password = secret("password");
        let ssh_password = secret("sshPassword");
        let ssh_passphrase = secret("sshKeyfilePassword");
        let mut c = map(&v, password);
        if let Some(p) = ssh_password.filter(|_| c.config.option("ssh.auth") == Some("password")) {
            c.config.options.insert("ssh.password".into(), p);
        }
        if let Some(p) = ssh_passphrase.filter(|_| c.config.option("ssh.auth") == Some("key")) {
            c.config.options.insert("ssh.passphrase".into(), p);
        }
        candidates.push(c);
    }
    if unreadable > 0 {
        warnings.push(format!("{unreadable} contraseña(s) no se pudieron descifrar: habrá que escribirlas al conectar."));
    }
    Ok(Found { path: file, candidates, warnings })
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("").trim()
}
fn b(v: &Value, k: &str) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(false)
}

fn map(v: &Value, password: Option<String>) -> Candidate {
    let engine = s(v, "engine");
    let kind = engine.split('@').next().unwrap_or("");
    let name = [s(v, "displayName"), s(v, "server"), s(v, "databaseFile"), s(v, "databaseUrl")]
        .into_iter()
        .find(|x| !x.is_empty())
        .unwrap_or(kind)
        .to_string();
    let mut c = Candidate::new(s(v, "_id").to_string(), name, kind.to_string());
    c.folder = s(v, "parent").split('/').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect();
    c.color = color_of(s(v, "connectionColor"));
    let cfg = &mut c.config;
    cfg.host = s(v, "server").to_string();
    cfg.port = v.get("port").map(|p| p.as_u64().map(|n| n as u16).unwrap_or_else(|| port_of(p.as_str().unwrap_or("")))).unwrap_or(0);
    cfg.database = s(v, "defaultDatabase").to_string();
    cfg.username = Some(s(v, "user").to_string()).filter(|u| !u.is_empty());
    cfg.password = password;
    cfg.read_only = b(v, "isReadOnly");
    cfg.encrypt = b(v, "useSsl");
    cfg.trust_server_certificate = b(v, "trustServerCertificate") || (cfg.encrypt && v.get("sslRejectUnauthorized").and_then(Value::as_bool) == Some(false));
    // SSH tunnel (docs/ssh-tunnels.md); its secrets are added by the caller.
    if b(v, "useSshTunnel") {
        let opts = &mut cfg.options;
        opts.insert("ssh.enabled".into(), "true".into());
        opts.insert("ssh.host".into(), s(v, "sshHost").to_string());
        let port = v.get("sshPort").map(|p| p.as_u64().map(|n| n.to_string()).unwrap_or_else(|| p.as_str().unwrap_or("").to_string())).unwrap_or_default();
        if !port.is_empty() && port != "22" {
            opts.insert("ssh.port".into(), port);
        }
        opts.insert("ssh.user".into(), s(v, "sshLogin").to_string());
        let auth = match s(v, "sshMode") {
            "keyFile" => {
                opts.insert("ssh.key_path".into(), s(v, "sshKeyfile").to_string());
                "key"
            }
            "agent" => "agent",
            _ => "password",
        };
        opts.insert("ssh.auth".into(), auth.into());
    }
    let url = s(v, "databaseUrl");
    let use_url = b(v, "useDatabaseUrl") && !url.is_empty();
    let auth = s(v, "authType");
    let driver = match kind {
        "mssql" => {
            match auth {
                "sspi" | "msnodesqlv8" => c.notes.push("La autenticación de Windows no está en DBine: se importa con usuario y contraseña.".into()),
                "msentra" => {
                    cfg.options.insert("auth".into(), "entra_password".into());
                }
                _ => {}
            }
            "sqlserver"
        }
        "mysql" | "mariadb" => kind,
        "postgres" | "cockroach" | "redshift" => {
            if use_url {
                apply_url(&mut c, url);
            }
            if auth == "awsIam" {
                c.notes.push("La autenticación IAM de AWS no se importa: usá usuario y contraseña.".into());
            }
            match kind {
                "cockroach" => "cockroachdb",
                k => k,
            }
        }
        "sqlite" | "duckdb" => {
            c.config.host = s(v, "databaseFile").to_string();
            kind
        }
        "libsql" => {
            c.config.host = if url.is_empty() { s(v, "databaseFile").to_string() } else { url.to_string() };
            if let Some(t) = Some(s(v, "authToken")).filter(|t| !t.is_empty()) {
                c.config.options.insert("auth_token".into(), t.to_string());
            }
            "libsql"
        }
        "mongo" | "mongo-legacy" => {
            if use_url {
                c.config.options.insert("connection_string".into(), url.to_string());
                if let Some(u) = parse_url(url) {
                    c.config.host = u.host;
                }
            }
            "mongodb"
        }
        "redis" => {
            if use_url {
                apply_url(&mut c, url);
            }
            if b(v, "cluster") {
                c.notes.push("El modo clúster de Redis no se importa: se conecta al primer nodo.".into());
            }
            "redis"
        }
        "oracle" => {
            let service = s(v, "serviceName");
            if use_url {
                c.config.options.insert("connect_descriptor".into(), url.to_string());
            } else if !service.is_empty() {
                c.config.options.insert("service".into(), service.to_string());
                c.config.options.insert("connect_by".into(), if s(v, "serviceNameType") == "sid" { "sid" } else { "service_name" }.into());
            }
            "oracle"
        }
        "clickhouse" => {
            if !url.is_empty() {
                if let Some(u) = parse_url(url) {
                    c.config.encrypt = u.scheme == "https";
                    c.config.host = u.host;
                    c.config.port = u.port;
                }
            }
            "clickhouse"
        }
        "cassandra" => {
            if let Some(dc) = Some(s(v, "localDataCenter")).filter(|x| !x.is_empty()) {
                c.config.options.insert("datacenter".into(), dc.to_string());
            }
            "cassandra"
        }
        "firebird" => {
            c.config.database = s(v, "databaseFile").to_string();
            "firebird"
        }
        _ => {
            c.unsupported = Some(format!("DBine no tiene un driver para «{kind}»"));
            ""
        }
    };
    c.config.driver = driver.to_string();
    c
}

/// Host, port, user, password and database from a connection URL.
fn apply_url(c: &mut Candidate, url: &str) {
    let Some(u) = parse_url(url) else { return };
    c.config.host = u.host;
    if u.port != 0 {
        c.config.port = u.port;
    }
    if u.user.is_some() {
        c.config.username = u.user;
    }
    if u.password.is_some() {
        c.config.password = u.password;
    }
    if !u.path.is_empty() {
        c.config.database = u.path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockModeEncrypt;

    /// simple-encryptor's `encrypt`, to build fixtures.
    fn encrypt(key: &str, value: &Value) -> String {
        let k = Sha256::digest(key.as_bytes());
        let iv = [7u8; 16];
        let json = serde_json::to_vec(value).unwrap();
        let mut buf = vec![0u8; json.len() + 16];
        let ct = cbc::Encryptor::<aes::Aes256>::new_from_slices(&k, &iv).unwrap().encrypt_padded_b2b::<Pkcs7>(&json, &mut buf).unwrap().to_vec();
        let body = format!("{}{}", hex::encode(iv), base64::engine::general_purpose::STANDARD.encode(ct));
        let mut mac = <hmac::Hmac<Sha256> as KeyInit>::new_from_slice(&k).unwrap();
        mac.update(body.as_bytes());
        format!("{}{body}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn reads_connections_and_passwords() {
        let dir = tempfile::tempdir().unwrap();
        let key = "0123abcd";
        std::fs::write(dir.path().join(".key"), encrypt(DEFAULT_KEY, &serde_json::json!({ "encryptionKey": key }))).unwrap();
        let pass = format!("crypt:{}", encrypt(key, &Value::String("s3cr3t'".into())));
        let lines = [
            serde_json::json!({ "_id": "a", "engine": "mssql@dbgate-plugin-mssql", "server": "sql.local", "port": "14330", "user": "sa", "password": pass, "displayName": "Ventas", "parent": "Clientes/Acme", "connectionColor": "red", "trustServerCertificate": true, "defaultDatabase": "ventas" }),
            serde_json::json!({ "_id": "b", "engine": "postgres@dbgate-plugin-postgres", "useDatabaseUrl": true, "databaseUrl": "postgres://ana:pw@pg:5433/app", "isReadOnly": true,
                "useSshTunnel": true, "sshHost": "bastion", "sshPort": "2200", "sshLogin": "ops", "sshMode": "userPassword", "sshPassword": format!("crypt:{}", encrypt(key, &Value::String("sshpw".into()))) }),
            serde_json::json!({ "_id": "c", "engine": "sqlite@dbgate-plugin-sqlite", "databaseFile": "/tmp/x.db" }),
            serde_json::json!({ "_id": "d", "engine": "oracle@dbgate-plugin-oracle", "server": "ora", "serviceName": "XE", "serviceNameType": "sid", "password": "crypt:broken" }),
            serde_json::json!({ "_id": "e", "engine": "cloudflare-d1@dbgate-plugin-sqlite" }),
            serde_json::json!({ "_id": "f", "engine": "mysql@dbgate-plugin-mysql", "unsaved": true }),
        ];
        std::fs::write(dir.path().join("connections.jsonl"), lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n")).unwrap();
        let found = read(dir.path()).unwrap();
        let c = &found.candidates;
        assert_eq!(c.len(), 5);
        assert_eq!((c[0].name.as_str(), c[0].config.driver.as_str(), c[0].config.port), ("Ventas", "sqlserver", 14330));
        assert_eq!(c[0].config.password.as_deref(), Some("s3cr3t'"));
        assert_eq!(c[0].folder, ["Clientes", "Acme"]);
        assert_eq!(c[0].color.as_deref(), Some("#f14c4c"));
        assert!(c[0].config.trust_server_certificate);
        assert_eq!((c[1].config.host.as_str(), c[1].config.port, c[1].config.database.as_str()), ("pg", 5433, "app"));
        assert_eq!((c[1].config.username.as_deref(), c[1].config.password.as_deref(), c[1].config.read_only), (Some("ana"), Some("pw"), true));
        let o = |k: &str| c[1].config.options.get(k).map(String::as_str);
        assert_eq!((o("ssh.enabled"), o("ssh.host"), o("ssh.port"), o("ssh.user"), o("ssh.auth"), o("ssh.password")), (Some("true"), Some("bastion"), Some("2200"), Some("ops"), Some("password"), Some("sshpw")));
        assert!(c[1].notes.is_empty(), "{:?}", c[1].notes);
        assert_eq!((c[2].config.driver.as_str(), c[2].config.host.as_str()), ("sqlite", "/tmp/x.db"));
        assert_eq!(c[3].config.options.get("connect_by").map(String::as_str), Some("sid"));
        assert_eq!(c[3].config.password, None);
        assert!(c[4].unsupported.is_some());
        assert_eq!(found.warnings.len(), 1, "{:?}", found.warnings);
    }
}
