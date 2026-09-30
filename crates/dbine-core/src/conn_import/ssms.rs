//! SQL Server Management Studio: the registered servers
//! (`%APPDATA%\Microsoft\SQL Server Management Studio\<version>\RegSrvr.xml`,
//! or a `.regsrvr` exported from SSMS, same format). Each server has a
//! connection string; its password is protected with Windows DPAPI for that
//! user and isn't imported.

use super::{app_data, apply_ado, pct_decode, Candidate, Found};
use dbine_driver::{Error, Result};
use std::path::{Path, PathBuf};

fn base() -> Option<PathBuf> {
    Some(app_data()?.join("Microsoft/SQL Server Management Studio"))
}

/// The newest SSMS version's `RegSrvr.xml`.
pub fn default_location() -> Option<PathBuf> {
    newest_in(&base()?)
}

fn newest_in(dir: &Path) -> Option<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path().join("RegSrvr.xml")).filter(|p| p.is_file()).collect();
    v.sort_by(|a, b| {
        let ver = |p: &PathBuf| p.parent().and_then(|d| d.file_name()).and_then(|n| n.to_str()).and_then(|n| n.split('.').next()).and_then(|n| n.parse::<u32>().ok()).unwrap_or(0);
        ver(b).cmp(&ver(a))
    });
    v.into_iter().next()
}

fn local<'a>(n: roxmltree::Node<'a, 'a>, name: &str) -> Option<roxmltree::Node<'a, 'a>> {
    n.children().find(|c| c.tag_name().name() == name)
}
fn text(n: roxmltree::Node, name: &str) -> String {
    local(n, name).and_then(|c| c.text()).unwrap_or("").trim().to_string()
}

/// The group path out of `/RegisteredServersStore/ServerGroup/DatabaseEngineServerGroup/ServerGroup/Prod/…`.
fn folder_of(uri: &str) -> Vec<String> {
    let parts: Vec<&str> = uri.split('/').collect();
    let Some(start) = parts.iter().position(|p| *p == "DatabaseEngineServerGroup") else { return Vec::new() };
    parts[start + 1..].chunks(2).filter(|c| c.len() == 2 && c[0] == "ServerGroup").map(|c| pct_decode(c[1])).collect()
}

pub fn read(path: &Path) -> Result<Found> {
    let file = if path.is_dir() {
        if path.join("RegSrvr.xml").is_file() { path.join("RegSrvr.xml") } else { newest_in(path).unwrap_or(path.join("RegSrvr.xml")) }
    } else {
        path.to_path_buf()
    };
    let text_all = std::fs::read_to_string(&file).map_err(|e| Error::Query(format!("no se pudo leer {}: {e}", file.display())))?;
    let doc = roxmltree::Document::parse(&text_all).map_err(|e| Error::Query(format!("{} no es XML válido: {e}", file.display())))?;
    let mut candidates = Vec::new();
    let mut with_password = 0;
    for (n, rs) in doc.descendants().filter(|n| n.tag_name().name() == "RegisteredServer").enumerate() {
        let server = text(rs, "ServerName");
        let name = Some(text(rs, "Name")).filter(|x| !x.is_empty()).unwrap_or_else(|| server.clone());
        let kind = Some(text(rs, "ServerType")).filter(|x| !x.is_empty()).unwrap_or_else(|| "DatabaseEngine".into());
        let mut c = Candidate::new(format!("{n}:{name}"), name, kind.clone());
        let uri = rs.descendants().find(|d| d.tag_name().name() == "Uri").and_then(|d| d.text()).unwrap_or("");
        c.folder = folder_of(uri);
        if kind != "DatabaseEngine" {
            c.unsupported = Some(format!("{kind} no es un motor de base de datos"));
            candidates.push(c);
            continue;
        }
        c.config.driver = "sqlserver".into();
        apply_ado(&mut c, &text(rs, "ConnectionStringWithEncryptedPassword"));
        if c.config.host.is_empty() {
            c.config.host = server;
        }
        // What's there is DPAPI-encrypted for the Windows user: not a password.
        if c.config.password.take().is_some() {
            with_password += 1;
        }
        if text(rs, "UseCustomConnectionColor") == "true" {
            if let Ok(argb) = text(rs, "CustomConnectionColorArgb").parse::<i64>() {
                c.color = Some(format!("#{:06x}", (argb as u32) & 0xff_ff_ff));
            }
        }
        candidates.push(c);
    }
    let mut warnings = Vec::new();
    if with_password > 0 {
        warnings.push("SSMS protege las contraseñas con la cuenta de Windows: se piden al conectar la primera vez.".into());
    }
    Ok(Found { path: file, candidates, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_registered_servers() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("RegSrvr.xml");
        let server = |name: &str, group: &str, cs: &str, kind: &str| {
            format!(
                r#"<document><data><RegisteredServers:RegisteredServer xmlns:RegisteredServers="http://schemas.microsoft.com/sqlserver/RegisteredServers/2007/08" xmlns:sfc="http://schemas.microsoft.com/sqlserver/sfc/serialization/2007/08" xmlns:sml="http://schemas.serviceml.org/sml/2007/02">
                  <RegisteredServers:Parent><sfc:Reference sml:ref="true"><sml:Uri>/RegisteredServersStore/ServerGroup/DatabaseEngineServerGroup{group}</sml:Uri></sfc:Reference></RegisteredServers:Parent>
                  <RegisteredServers:Name type="string">{name}</RegisteredServers:Name>
                  <RegisteredServers:ServerName type="string">{name}.local</RegisteredServers:ServerName>
                  <RegisteredServers:UseCustomConnectionColor type="boolean">true</RegisteredServers:UseCustomConnectionColor>
                  <RegisteredServers:CustomConnectionColorArgb type="int">-65536</RegisteredServers:CustomConnectionColorArgb>
                  <RegisteredServers:ServerType type="ServerType">{kind}</RegisteredServers:ServerType>
                  <RegisteredServers:ConnectionStringWithEncryptedPassword type="string">{cs}</RegisteredServers:ConnectionStringWithEncryptedPassword>
                </RegisteredServers:RegisteredServer></data></document>"#
            )
        };
        let xml = format!(
            r#"<?xml version="1.0"?><model xmlns="http://schemas.serviceml.org/sml/2007/02"><instances>{}{}{}</instances></model>"#,
            server("ventas", "/ServerGroup/Prod/ServerGroup/Latam", "data source=ventas.local,14330;initial catalog=ventas;user id=sa;password=AQAAANCMnd8BFdERjHoAwE;encrypt=True;trustservercertificate=True", "DatabaseEngine"),
            server("local", "", "data source=.\\SQLEXPRESS;integrated security=True", "DatabaseEngine"),
            server("olap", "", "data source=olap", "AnalysisServices"),
        );
        std::fs::write(&f, xml).unwrap();
        let found = read(dir.path()).unwrap();
        let c = &found.candidates;
        assert_eq!(c.len(), 3);
        assert_eq!((c[0].config.host.as_str(), c[0].config.database.as_str(), c[0].config.username.as_deref(), c[0].config.password.as_deref()), ("ventas.local,14330", "ventas", Some("sa"), None));
        assert_eq!((c[0].folder.clone(), c[0].color.as_deref(), c[0].config.encrypt), (vec!["Prod".to_string(), "Latam".to_string()], Some("#ff0000"), true));
        assert_eq!((c[1].config.host.as_str(), c[1].notes.len()), (".\\SQLEXPRESS", 1));
        assert!(c[2].unsupported.is_some());
        assert_eq!(found.warnings.len(), 1);
    }
}
