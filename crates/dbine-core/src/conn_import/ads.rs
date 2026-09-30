//! Azure Data Studio: `<config>/azuredatastudio/User/settings.json`, with
//! `datasource.connections` and the folders in `datasource.connectionGroups`.
//! The file is JSON with comments. Passwords stay in Azure Data Studio's own
//! credential store and aren't imported.

use super::{app_data, apply_ado, port_of, Candidate, Found};
use dbine_driver::{Error, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub fn default_location() -> Option<PathBuf> {
    let f = app_data()?.join("azuredatastudio/User/settings.json");
    f.is_file().then_some(f)
}

/// JSON with `//` and `/* */` comments and trailing commas, as plain JSON.
fn strip_jsonc(s: &str) -> String {
    let b: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let (mut i, mut in_str) = (0, false);
    while i < b.len() {
        let c = b[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < b.len() {
                out.push(b[i + 1]);
                i += 1;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
            out.push(c);
        } else if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            continue;
        } else if c == '/' && b.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                i += 1;
            }
            i += 2;
            continue;
        } else if c == ',' {
            // A comma before `}` or `]` is dropped.
            let next = b[i + 1..].iter().find(|x| !x.is_whitespace());
            if !matches!(next, Some('}') | Some(']')) {
                out.push(c);
            }
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

pub fn read(path: &Path) -> Result<Found> {
    let file = if path.is_dir() {
        [path.join("settings.json"), path.join("User/settings.json"), path.join("azuredatastudio/User/settings.json")].into_iter().find(|p| p.is_file()).unwrap_or(path.join("settings.json"))
    } else {
        path.to_path_buf()
    };
    let text = std::fs::read_to_string(&file).map_err(|e| Error::Query(format!("no se pudo leer {}: {e}", file.display())))?;
    let doc: Value = serde_json::from_str(&strip_jsonc(&text)).map_err(|e| Error::Query(format!("{} no es JSON válido: {e}", file.display())))?;
    let groups: HashMap<String, (String, Option<String>)> = doc
        .get("datasource.connectionGroups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|g| Some((g.get("id")?.as_str()?.to_string(), (g.get("name")?.as_str()?.to_string(), g.get("parentId").and_then(Value::as_str).map(String::from)))))
        .collect();
    let folder_of = |mut id: Option<String>| {
        let mut path = Vec::new();
        // ROOT is the invisible top; the guard stops cycles.
        while let Some(g) = id.and_then(|i| groups.get(&i)) {
            if g.0 == "ROOT" || path.len() > 20 {
                break;
            }
            path.insert(0, g.0.clone());
            id = g.1.clone();
        }
        path
    };
    let mut candidates = Vec::new();
    let mut with_password = 0;
    for (n, conn) in doc.get("datasource.connections").and_then(Value::as_array).into_iter().flatten().enumerate() {
        let o = conn.get("options").cloned().unwrap_or(Value::Null);
        let s = |k: &str| o.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
        let provider = conn.get("providerName").and_then(Value::as_str).unwrap_or("MSSQL");
        let server = if s("server").is_empty() { s("host") } else { s("server") };
        let database = if s("database").is_empty() { s("dbname") } else { s("database") };
        let name = [s("connectionName"), server.clone()].into_iter().find(|x| !x.is_empty()).unwrap_or_else(|| format!("Conexión {}", n + 1));
        let key = conn.get("id").and_then(Value::as_str).map(String::from).unwrap_or_else(|| n.to_string());
        let mut c = Candidate::new(key, name, provider.to_string());
        let group = conn.get("groupId").or_else(|| o.get("groupId")).and_then(Value::as_str).map(String::from);
        c.folder = folder_of(group);
        if !c.set_driver(provider) {
            candidates.push(c);
            continue;
        }
        if c.config.driver == "sqlserver" {
            // The server as SQL Server writes it (`host,port`, `host\instance`).
            apply_ado(&mut c, &format!("Server={server}"));
        } else {
            c.config.host = server;
        }
        c.config.database = database;
        c.config.username = Some(s("user")).filter(|u| !u.is_empty());
        let port = o.get("port").map(|p| p.as_u64().map(|n| n as u16).unwrap_or_else(|| port_of(p.as_str().unwrap_or("")))).unwrap_or(0);
        if port != 0 {
            c.config.port = port;
        }
        let encrypt = o.get("encrypt").map(|v| v.as_bool().unwrap_or_else(|| super::truthy(v.as_str().unwrap_or("")))).unwrap_or(false);
        c.config.encrypt = encrypt;
        c.config.trust_server_certificate = o.get("trustServerCertificate").and_then(Value::as_bool).unwrap_or(false);
        match s("authenticationType").as_str() {
            "Integrated" => c.notes.push(super::WINDOWS_AUTH.into()),
            "AzureMFA" | "AzureMFAAndUser" => {
                c.config.options.insert("auth".into(), "entra_password".into());
                c.notes.push("Entra ID con MFA no está en DBine: usá usuario y contraseña, una entidad de servicio o un token.".into());
            }
            _ => {}
        }
        if conn.get("savePassword").or_else(|| o.get("savePassword")).and_then(Value::as_bool) == Some(true) {
            with_password += 1;
        }
        candidates.push(c);
    }
    let mut warnings = Vec::new();
    if with_password > 0 {
        warnings.push("Azure Data Studio guarda las contraseñas en su propio almacén: se piden al conectar la primera vez.".into());
    }
    Ok(Found { path: file, candidates, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_settings_with_comments() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("settings.json");
        std::fs::write(
            &f,
            r#"{
              // comment
              "workbench.colorTheme": "Default Dark // not a comment",
              "datasource.connectionGroups": [
                { "name": "ROOT", "id": "root" },
                { "name": "Clientes", "id": "g1", "parentId": "root" },
                { "name": "Acme", "id": "g2", "parentId": "g1" },
              ],
              "datasource.connections": [
                { "options": { "server": "srv.database.windows.net,1433", "database": "ventas", "authenticationType": "SqlLogin", "user": "ana",
                    "connectionName": "Ventas", "encrypt": "Mandatory", "trustServerCertificate": false },
                  "groupId": "g2", "providerName": "MSSQL", "savePassword": true, "id": "c1" },
                { "options": { "host": "pg", "dbname": "app", "user": "postgres", "port": 5433 }, "groupId": "root", "providerName": "PGSQL", "id": "c2" },
                /* block */
                { "options": { "server": "k.kusto.windows.net" }, "providerName": "KUSTO", "id": "c3" },
              ]
            }"#,
        )
        .unwrap();
        let found = read(&f).unwrap();
        let c = &found.candidates;
        assert_eq!(c.len(), 3);
        assert_eq!((c[0].name.as_str(), c[0].config.driver.as_str(), c[0].config.host.as_str(), c[0].config.database.as_str()), ("Ventas", "sqlserver", "srv.database.windows.net,1433", "ventas"));
        assert_eq!((c[0].config.encrypt, c[0].folder.clone()), (true, vec!["Clientes".to_string(), "Acme".to_string()]));
        assert_eq!((c[1].config.driver.as_str(), c[1].config.host.as_str(), c[1].config.port, c[1].config.database.as_str()), ("postgres", "pg", 5433, "app"));
        assert!(c[1].folder.is_empty());
        assert!(c[2].unsupported.is_some());
        assert_eq!(found.warnings.len(), 1);
    }
}
