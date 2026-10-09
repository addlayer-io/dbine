//! DBeaver: each project's `.dbeaver/data-sources*.json`, with the user
//! names and passwords in `.dbeaver/credentials-config.json`, encrypted with
//! AES-128-CBC under a key built into DBeaver (the first 16 bytes are the IV).

use super::{apply_jdbc, color_of, home, port_of, Candidate, Found};
use aes::cipher::{block_padding::Pkcs7, BlockModeDecrypt, KeyIvInit};
use dbine_driver::{Error, Result};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

/// DBeaver's built-in key for `credentials-config.json`.
const CREDENTIALS_KEY: [u8; 16] = [0xba, 0xbb, 0x4a, 0x9f, 0x77, 0x4a, 0xb8, 0x53, 0xc9, 0x6c, 0x2d, 0x65, 0x3d, 0xfe, 0x54, 0x4a];

fn workspace() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        home()?.join("Library/DBeaverData")
    } else if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?).join("DBeaverData")
    } else {
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or(home()?.join(".local/share")).join("DBeaverData")
    };
    Some(base.join("workspace6"))
}

pub fn default_location() -> Option<PathBuf> {
    let ws = workspace()?;
    (!project_dirs(&ws).is_empty()).then_some(ws)
}

fn has_sources(dir: &Path) -> bool {
    !source_files(dir).is_empty()
}

fn source_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("data-sources") && n.ends_with(".json")))
        .collect();
    v.sort();
    v
}

/// The `.dbeaver` folders under what the user picked: the folder itself, a
/// project, the workspace or the whole DBeaverData folder.
fn project_dirs(path: &Path) -> Vec<PathBuf> {
    if has_sources(path) {
        return vec![path.to_path_buf()];
    }
    if has_sources(&path.join(".dbeaver")) {
        return vec![path.join(".dbeaver")];
    }
    let ws = if path.join("workspace6").is_dir() { path.join("workspace6") } else { path.to_path_buf() };
    let mut out: Vec<PathBuf> = std::fs::read_dir(&ws)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join(".dbeaver"))
        .filter(|d| has_sources(d))
        .collect();
    out.sort();
    out
}

fn credentials(dir: &Path) -> std::result::Result<Map<String, Value>, String> {
    let Ok(bytes) = std::fs::read(dir.join("credentials-config.json")) else { return Ok(Map::new()) };
    decrypt_credentials(&bytes).ok_or_else(|| "No se pudieron descifrar las credenciales de DBeaver: las contraseñas no se importan.".to_string())
}

fn decrypt_credentials(bytes: &[u8]) -> Option<Map<String, Value>> {
    let (iv, ct) = (bytes.get(..16)?, bytes.get(16..)?);
    let mut buf = ct.to_vec();
    let plain = cbc::Decryptor::<aes::Aes128>::new_from_slices(&CREDENTIALS_KEY, iv).ok()?.decrypt_padded::<Pkcs7>(&mut buf).ok()?;
    match serde_json::from_slice(plain).ok()? {
        Value::Object(m) => Some(m),
        _ => None,
    }
}

pub fn read(path: &Path) -> Result<Found> {
    let dirs: Vec<(PathBuf, Vec<PathBuf>)> = if path.is_file() {
        vec![(path.parent().unwrap_or(path).to_path_buf(), vec![path.to_path_buf()])]
    } else {
        project_dirs(path).into_iter().map(|d| { let f = source_files(&d); (d, f) }).collect()
    };
    if dirs.is_empty() {
        return Err(Error::Query(format!("no hay conexiones de DBeaver (data-sources.json) en {}", path.display())));
    }
    let several = dirs.len() > 1;
    let mut candidates = Vec::new();
    let mut warnings = Vec::new();
    for (dir, files) in &dirs {
        // `<project>/.dbeaver`: the project's name.
        let project = dir.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()).unwrap_or("").to_string();
        let creds = credentials(dir).unwrap_or_else(|w| {
            warnings.push(w);
            Map::new()
        });
        for file in files {
            let text = std::fs::read_to_string(file).map_err(|e| Error::Query(format!("no se pudo leer {}: {e}", file.display())))?;
            let doc: Value = serde_json::from_str(&text).map_err(|e| Error::Query(format!("{} no es JSON válido: {e}", file.display())))?;
            let Some(conns) = doc.get("connections").and_then(Value::as_object) else { continue };
            for (id, v) in conns {
                let mut c = map(id, v, creds.get(id));
                c.key = format!("{project}:{id}");
                // Other projects than the default one become a folder.
                if several && project != "General" && !project.is_empty() {
                    c.folder.insert(0, project.clone());
                }
                candidates.push(c);
            }
        }
    }
    Ok(Found { path: path.to_path_buf(), candidates, warnings })
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("").trim()
}

fn map(id: &str, v: &Value, cred: Option<&Value>) -> Candidate {
    let conf = v.get("configuration").cloned().unwrap_or(Value::Null);
    let provider = s(v, "provider");
    let driver_id = s(v, "driver");
    let url = s(&conf, "url");
    let kind = if driver_id.is_empty() { provider.to_string() } else { format!("{provider} / {driver_id}") };
    let name = Some(s(v, "name")).filter(|n| !n.is_empty()).unwrap_or(id).to_string();
    let mut c = Candidate::new(id.to_string(), name, kind);
    c.folder = s(v, "folder").split('/').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect();
    c.color = color_of(s(&conf, "type"));
    // DBeaver's connection types, as DBine tags.
    if let Some(tag) = match s(&conf, "type") {
        "prod" => Some("prod"),
        "test" => Some("qa"),
        "dev" => Some("dev"),
        _ => None,
    } {
        c.tags.push(tag.to_string());
    }
    c.config.read_only = v.get("read-only").and_then(Value::as_bool).unwrap_or(false);

    // The provider decides; the driver id and URL settle `generic` ones.
    let probe = if provider.is_empty() || provider == "generic" { format!("{driver_id} {url}") } else { format!("{provider} {driver_id} {url}") };
    if !c.set_driver(&probe) {
        return c;
    }
    let driver = c.config.driver.clone();
    let driver = driver.as_str();
    let (host, port, database) = (s(&conf, "host").to_string(), port_of(s(&conf, "port")), s(&conf, "database").to_string());
    if host.is_empty() && !url.is_empty() && !matches!(driver, "sqlite" | "duckdb") {
        apply_jdbc(&mut c, url);
    }
    let cfg = &mut c.config;
    if !host.is_empty() {
        cfg.host = host;
    }
    if port != 0 {
        cfg.port = port;
    }
    if !database.is_empty() {
        cfg.database = database;
    }
    // Login: the encrypted credentials, else the plain fields of old versions.
    let login = cred.and_then(|c| c.get("#connection"));
    let pick = |k: &str| login.map(|l| s(l, k)).filter(|x| !x.is_empty()).or(Some(s(&conf, k)).filter(|x| !x.is_empty())).map(String::from);
    cfg.username = pick("user");
    cfg.password = pick("password");
    // Driver properties, and the ones written in the URL (`;encrypt=true`, `?sslmode=require`).
    let mut props: Vec<(String, String)> = conf
        .get("properties")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.as_str().unwrap_or("").to_ascii_lowercase())).collect())
        .unwrap_or_default();
    for p in url.split([';', '?', '&']).skip(1) {
        if let Some((k, v)) = p.split_once('=') {
            props.push((k.to_ascii_lowercase(), v.to_ascii_lowercase()));
        }
    }
    let prop = |k: &str| props.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.clone()).unwrap_or_default();
    cfg.encrypt = prop("encrypt") == "true" || prop("ssl") == "true" || matches!(prop("sslmode").as_str(), "require" | "verify-ca" | "verify-full");
    cfg.trust_server_certificate = prop("trustservercertificate") == "true";

    let pp = conf.get("provider-properties").cloned().unwrap_or(Value::Null);
    match driver {
        // File engines: the path is the "database".
        "sqlite" | "duckdb" => {
            let file = if cfg.database.is_empty() { url.split_once(&format!("{driver}:")).map(|(_, f)| f.to_string()).unwrap_or_default() } else { cfg.database.clone() };
            cfg.host = file;
            cfg.database.clear();
        }
        "h2" if !url.contains("tcp://") => {
            c.unsupported = Some("H2 embebido no está en DBine (solo H2 en modo servidor)".into());
        }
        "oracle" => {
            let target = std::mem::take(&mut cfg.database);
            if s(&pp, "@dbeaver-connection-type@").eq_ignore_ascii_case("TNS") || s(&pp, "oracle.connection.type").eq_ignore_ascii_case("TNS") {
                c.notes.push("Es una conexión TNS: revisá el descriptor de conexión en DBine.".into());
            }
            if !target.is_empty() {
                let sid = s(&pp, "@dbeaver-sid-service@").eq_ignore_ascii_case("SID");
                cfg.options.insert("service".into(), target);
                cfg.options.insert("connect_by".into(), if sid { "sid" } else { "service_name" }.into());
            }
            if cfg.host.is_empty() && !url.is_empty() {
                cfg.options.insert("connect_descriptor".into(), url.trim_start_matches("jdbc:oracle:thin:@").to_string());
            }
        }
        "snowflake" => {
            let account = cfg.host.trim_end_matches(".snowflakecomputing.com").to_string();
            cfg.options.insert("account".into(), account);
            cfg.host.clear();
            if let Some(w) = Some(s(&pp, "warehouse")).filter(|w| !w.is_empty()) {
                cfg.options.insert("warehouse".into(), w.to_string());
            }
            if cfg.password.is_some() {
                cfg.options.insert("auth_mode".into(), "password".into());
            }
        }
        _ => {}
    }
    let auth = s(&conf, "auth-model").to_ascii_lowercase();
    if ["windows", "kerberos", "ntlm", "sspi"].iter().any(|k| auth.contains(k)) {
        c.notes.push("La autenticación integrada (Windows/Kerberos) no se importa: se usa usuario y contraseña.".into());
    } else if auth.contains("aws") || auth.contains("iam") {
        c.notes.push("La autenticación IAM no se importa: usá usuario y contraseña.".into());
    }
    // SSH tunnel (docs/ssh-tunnels.md): its user and password are in the credentials.
    if conf.pointer("/handlers/ssh_tunnel/enabled").and_then(Value::as_bool).unwrap_or(false) {
        let p = conf.pointer("/handlers/ssh_tunnel/properties").cloned().unwrap_or(Value::Null);
        let login = cred.and_then(|c| c.get("network/ssh_tunnel"));
        let text = |v: &Value, k: &str| -> String {
            match v.get(k) {
                Some(Value::String(s)) => s.trim().to_string(),
                Some(Value::Number(n)) => n.to_string(),
                _ => String::new(),
            }
        };
        let opts = &mut c.config.options;
        opts.insert("ssh.enabled".into(), "true".into());
        opts.insert("ssh.host".into(), text(&p, "host"));
        if let Some(port) = Some(text(&p, "port")).filter(|x| !x.is_empty() && x != "22") {
            opts.insert("ssh.port".into(), port);
        }
        let user = login.map(|l| text(l, "user")).filter(|u| !u.is_empty()).unwrap_or_else(|| text(&p, "user"));
        opts.insert("ssh.user".into(), user);
        let secret = login.map(|l| text(l, "password")).filter(|x| !x.is_empty());
        match text(&p, "authType").to_ascii_uppercase().as_str() {
            "PUBLIC_KEY" => {
                opts.insert("ssh.auth".into(), "key".into());
                opts.insert("ssh.key_path".into(), text(&p, "keyPath"));
                if let Some(s) = secret {
                    opts.insert("ssh.passphrase".into(), s);
                }
            }
            "AGENT" => {
                opts.insert("ssh.auth".into(), "agent".into());
            }
            _ => {
                opts.insert("ssh.auth".into(), "password".into());
                if let Some(s) = secret {
                    opts.insert("ssh.password".into(), s);
                }
            }
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::BlockModeEncrypt;

    fn encrypt_credentials(v: &Value) -> Vec<u8> {
        let iv = [3u8; 16];
        let json = serde_json::to_vec(v).unwrap();
        let mut buf = vec![0u8; json.len() + 16];
        let ct = cbc::Encryptor::<aes::Aes128>::new_from_slices(&CREDENTIALS_KEY, &iv).unwrap().encrypt_padded_b2b::<Pkcs7>(&json, &mut buf).unwrap().to_vec();
        [iv.to_vec(), ct].concat()
    }

    #[test]
    fn reads_a_workspace() {
        let ws = tempfile::tempdir().unwrap();
        let general = ws.path().join("General/.dbeaver");
        let other = ws.path().join("Clientes/.dbeaver");
        std::fs::create_dir_all(&general).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let sources = serde_json::json!({
            "folders": { "Prod": {} },
            "connections": {
                "postgres-jdbc-1": { "provider": "postgresql", "driver": "postgres-jdbc", "name": "App", "folder": "Prod", "read-only": true,
                    "configuration": { "host": "pg.local", "port": "5433", "database": "app", "url": "jdbc:postgresql://pg.local:5433/app", "type": "prod", "auth-model": "native",
                        "handlers": { "ssh_tunnel": { "enabled": true, "properties": { "host": "bastion.local", "port": 2222, "authType": "PUBLIC_KEY", "keyPath": "~/.ssh/id_ed25519" } } } } },
                "mssql-2": { "provider": "sqlserver", "driver": "microsoft", "name": "Ventas",
                    "configuration": { "url": "jdbc:sqlserver://sql.local:14330;databaseName=ventas;encrypt=true", "properties": { "trustServerCertificate": "true" } } },
                "oracle-3": { "provider": "oracle", "driver": "oracle_thin", "name": "ERP",
                    "configuration": { "host": "ora", "port": "1521", "database": "ORCL", "provider-properties": { "@dbeaver-sid-service@": "SID" } } },
                "sqlite-4": { "provider": "generic", "driver": "sqlite_jdbc", "name": "Local", "configuration": { "database": "/tmp/a.db", "url": "jdbc:sqlite:/tmp/a.db" } },
                "bq-5": { "provider": "bigquery", "driver": "bigquery", "name": "BQ", "configuration": {} },
                "x-6": { "provider": "generic", "driver": "some_odd_driver", "name": "Raro", "configuration": { "url": "jdbc:odd://h" } }
            }
        });
        std::fs::write(general.join("data-sources.json"), sources.to_string()).unwrap();
        std::fs::write(
            general.join("credentials-config.json"),
            encrypt_credentials(&serde_json::json!({ "postgres-jdbc-1": { "#connection": { "user": "ana", "password": "p'ss" }, "network/ssh_tunnel": { "user": "ops", "password": "frase" } }, "mssql-2": { "#connection": { "user": "sa" } } })),
        )
        .unwrap();
        std::fs::write(other.join("data-sources.json"), serde_json::json!({ "connections": { "mysql8-7": { "provider": "mysql", "driver": "mysql8", "name": "Tienda", "configuration": { "host": "my", "port": "3306", "user": "root", "password": "plain" } } } }).to_string()).unwrap();

        let found = read(ws.path()).unwrap();
        assert!(found.warnings.is_empty(), "{:?}", found.warnings);
        let by = |n: &str| found.candidates.iter().find(|c| c.name == n).unwrap();
        let pg = by("App");
        assert_eq!((pg.config.driver.as_str(), pg.config.host.as_str(), pg.config.port, pg.config.database.as_str()), ("postgres", "pg.local", 5433, "app"));
        assert_eq!((pg.config.username.as_deref(), pg.config.password.as_deref()), (Some("ana"), Some("p'ss")));
        assert_eq!((pg.folder.clone(), pg.color.as_deref(), pg.config.read_only), (vec!["Prod".to_string()], Some("#f14c4c"), true));
        assert_eq!(pg.tags, vec!["prod".to_string()]);
        assert!(pg.notes.is_empty(), "{:?}", pg.notes);
        let o = |k: &str| pg.config.options.get(k).map(String::as_str);
        assert_eq!((o("ssh.enabled"), o("ssh.host"), o("ssh.port"), o("ssh.user")), (Some("true"), Some("bastion.local"), Some("2222"), Some("ops")));
        assert_eq!((o("ssh.auth"), o("ssh.key_path"), o("ssh.passphrase")), (Some("key"), Some("~/.ssh/id_ed25519"), Some("frase")));
        let ms = by("Ventas");
        assert_eq!((ms.config.driver.as_str(), ms.config.host.as_str(), ms.config.port, ms.config.database.as_str()), ("sqlserver", "sql.local", 14330, "ventas"));
        assert!(ms.config.trust_server_certificate);
        assert_eq!((ms.config.username.as_deref(), ms.config.password.as_deref()), (Some("sa"), None));
        let ora = by("ERP");
        assert_eq!((ora.config.options.get("service").map(String::as_str), ora.config.options.get("connect_by").map(String::as_str)), (Some("ORCL"), Some("sid")));
        assert_eq!(by("Local").config.host, "/tmp/a.db");
        assert!(by("BQ").unsupported.is_some());
        assert!(by("Raro").unsupported.is_some());
        let my = by("Tienda");
        assert_eq!((my.config.driver.as_str(), my.config.password.as_deref(), my.folder.clone()), ("mysql", Some("plain"), vec!["Clientes".to_string()]));

        // A single data-sources.json file.
        let one = read(&general.join("data-sources.json")).unwrap();
        assert_eq!(one.candidates.len(), 6);
        assert!(one.candidates.iter().all(|c| c.folder.first().map(String::as_str) != Some("General")));
    }
}
