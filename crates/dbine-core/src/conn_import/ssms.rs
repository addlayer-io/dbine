//! SQL Server Management Studio: the registered servers
//! (`%APPDATA%\Microsoft\SQL Server Management Studio\<version>\RegSrvr.xml`,
//! or a `.regsrvr` exported from SSMS, same format) and the connection
//! history, the servers the Connect dialog remembers (`UserSettings.xml`
//! beside it in SSMS 18 to 20; SSMS 21 and later keep their files under
//! `%LOCALAPPDATA%\Microsoft\SSMS\<version>_<id>`). Passwords are
//! protected with Windows DPAPI for that user and aren't imported.

use super::{app_data, apply_ado, pct_decode, Candidate, Found};
use dbine_driver::{Error, Result};
use std::path::{Path, PathBuf};

fn base() -> Option<PathBuf> {
    Some(app_data()?.join("Microsoft/SQL Server Management Studio"))
}

const REGISTERED: &str = "RegSrvr.xml";
const HISTORY: &str = "UserSettings.xml";
/// SSMS's server type for the database engine (the others are Analysis,
/// Reporting and Integration Services).
const DATABASE_ENGINE: &str = "8c91a03d-f9b4-46c0-a305-b5dcc79ff907";

/// The newest SSMS version's folder that has registered servers or a
/// connection history (SSMS 18 to 20 in `%APPDATA%`, 21 and later also in
/// `%LOCALAPPDATA%\Microsoft\SSMS`).
pub fn default_location() -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = base().into_iter().collect();
    if let Some(a) = app_data() {
        roots.push(a.join("Microsoft/SSMS"));
    }
    if let Some(l) = std::env::var_os("LOCALAPPDATA").map(PathBuf::from) {
        roots.push(l.join("Microsoft/SSMS"));
        roots.push(l.join("Microsoft/SQL Server Management Studio"));
    }
    roots
        .iter()
        .flat_map(|r| [newest_with(r, REGISTERED), newest_with(r, HISTORY)])
        .flatten()
        .filter_map(|f| f.parent().map(Path::to_path_buf))
        .max_by_key(|d| version_of(d))
}

/// `21.0_a1b2c3` → 21, `18.0` → 18, anything else 0.
fn version_of(dir: &Path) -> u32 {
    dir.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.split(['.', '_']).next())
        .and_then(|n| n.parse::<u32>().ok())
        .unwrap_or(0)
}

#[cfg(test)]
fn newest_in(dir: &Path) -> Option<PathBuf> {
    newest_with(dir, REGISTERED)
}

/// The newest version folder in `dir` that has `file`.
fn newest_with(dir: &Path, file: &str) -> Option<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path().join(file))
        .filter(|p| p.is_file())
        .collect();
    v.sort_by(|a, b| {
        let ver = |p: &PathBuf| {
            p.parent()
                .and_then(|d| d.file_name())
                .and_then(|n| n.to_str())
                .and_then(|n| n.split('.').next())
                .and_then(|n| n.parse::<u32>().ok())
                .unwrap_or(0)
        };
        ver(b).cmp(&ver(a))
    });
    v.into_iter().next()
}

fn local<'a>(n: roxmltree::Node<'a, 'a>, name: &str) -> Option<roxmltree::Node<'a, 'a>> {
    n.children().find(|c| c.tag_name().name() == name)
}
fn text(n: roxmltree::Node, name: &str) -> String {
    local(n, name)
        .and_then(|c| c.text())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// The group path out of `/RegisteredServersStore/ServerGroup/DatabaseEngineServerGroup/ServerGroup/Prod/…`.
fn folder_of(uri: &str) -> Vec<String> {
    let parts: Vec<&str> = uri.split('/').collect();
    let Some(start) = parts.iter().position(|p| *p == "DatabaseEngineServerGroup") else {
        return Vec::new();
    };
    parts[start + 1..]
        .chunks(2)
        .filter(|c| c.len() == 2 && c[0] == "ServerGroup")
        .map(|c| pct_decode(c[1]))
        .collect()
}

pub fn read(path: &Path) -> Result<Found> {
    // A folder: its registered servers and its connection history (or the
    // newest version's of each, for the folder of all versions).
    let files: Vec<PathBuf> = if path.is_dir() {
        [REGISTERED, HISTORY]
            .into_iter()
            .filter_map(|name| {
                Some(path.join(name))
                    .filter(|f| f.is_file())
                    .or_else(|| newest_with(path, name))
            })
            .collect()
    } else {
        vec![path.to_path_buf()]
    };
    let Some(first) = files.first().cloned() else {
        return Err(Error::Query(format!(
            "no se pudo leer {}: no tiene {REGISTERED} ni {HISTORY}",
            path.display()
        )));
    };
    let mut candidates = Vec::new();
    let mut with_password = 0;
    for file in &files {
        let text_all = std::fs::read_to_string(file)
            .map_err(|e| Error::Query(format!("no se pudo leer {}: {e}", file.display())))?;
        // UserSettings.xml starts with a byte-order mark.
        let doc = roxmltree::Document::parse(text_all.trim_start_matches('\u{feff}'))
            .map_err(|e| Error::Query(format!("{} no es XML válido: {e}", file.display())))?;
        registered(&doc, &mut candidates, &mut with_password);
        history(&doc, &mut candidates, &mut with_password);
    }
    let mut warnings = Vec::new();
    if with_password > 0 {
        warnings.push("SSMS protege las contraseñas con la cuenta de Windows: se piden al conectar la primera vez.".into());
    }
    Ok(Found {
        path: first,
        candidates,
        warnings,
    })
}

/// The registered servers (`RegSrvr.xml`, a `.regsrvr` export).
fn registered(
    doc: &roxmltree::Document,
    candidates: &mut Vec<Candidate>,
    with_password: &mut usize,
) {
    for (n, rs) in doc
        .descendants()
        .filter(|n| n.tag_name().name() == "RegisteredServer")
        .enumerate()
    {
        let server = text(rs, "ServerName");
        let name = Some(text(rs, "Name"))
            .filter(|x| !x.is_empty())
            .unwrap_or_else(|| server.clone());
        let kind = Some(text(rs, "ServerType"))
            .filter(|x| !x.is_empty())
            .unwrap_or_else(|| "DatabaseEngine".into());
        let mut c = Candidate::new(format!("{n}:{name}"), name, kind.clone());
        let uri = rs
            .descendants()
            .find(|d| d.tag_name().name() == "Uri")
            .and_then(|d| d.text())
            .unwrap_or("");
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
            *with_password += 1;
        }
        if text(rs, "UseCustomConnectionColor") == "true" {
            if let Ok(argb) = text(rs, "CustomConnectionColorArgb").parse::<i64>() {
                c.color = Some(format!("#{:06x}", (argb as u32) & 0xff_ff_ff));
            }
        }
        candidates.push(c);
    }
}

/// The connection history (`UserSettings.xml`): each database engine the
/// Connect dialog remembers, once per login used with it. A server that is
/// also registered with the same login comes once, as registered.
fn history(doc: &roxmltree::Document, candidates: &mut Vec<Candidate>, with_password: &mut usize) {
    for item in doc
        .descendants()
        .filter(|n| n.tag_name().name() == "ServerConnectionItem")
    {
        // `ServerTypes/Element/{Key/guid, Value/ServerTypeItem/…}`.
        let kind = item
            .ancestors()
            .find(|a| a.tag_name().name() == "ServerTypeItem")
            .and_then(|t| t.parent())
            .and_then(|v| v.parent())
            .and_then(|e| local(e, "Key"))
            .and_then(|k| local(k, "guid"))
            .and_then(|g| g.text())
            .map(|g| g.trim().to_ascii_lowercase());
        if kind.as_deref().is_some_and(|k| k != DATABASE_ENGINE) {
            continue;
        }
        let instance = text(item, "Instance");
        if instance.is_empty() {
            continue;
        }
        let mut settings: Vec<roxmltree::Node> = item
            .descendants()
            .filter(|n| n.tag_name().name() == "ServerConnectionSettings")
            .collect();
        if settings.is_empty() {
            settings.push(item);
        }
        let mut users = Vec::new();
        for s in settings {
            let user = text(s, "UserName");
            if users.contains(&user) {
                continue;
            }
            users.push(user.clone());
            let auth = Some(text(s, "AuthenticationMethod"))
                .filter(|a| !a.is_empty())
                .unwrap_or_else(|| text(item, "AuthenticationMethod"));
            // `Advanced`: `EC` encrypts, `TSC` trusts the certificate,
            // `IniDb` the database the dialog opened.
            let advanced = |key: &str| {
                local(s, "Advanced").and_then(|a| {
                    a.children()
                        .filter(|e| e.tag_name().name() == "Element")
                        .find(|e| {
                            local(*e, "Key")
                                .and_then(|k| local(k, "string"))
                                .and_then(|k| k.text())
                                == Some(key)
                        })
                        .and_then(|e| local(e, "Value"))
                        .and_then(|v| local(v, "string"))
                        .and_then(|v| v.text())
                        .map(str::trim)
                        .map(str::to_string)
                })
            };
            let database = Some(text(s, "Database"))
                .filter(|d| !d.is_empty())
                .or_else(|| advanced("IniDb").filter(|d| !d.is_empty()))
                .unwrap_or_default();
            let quoted = |v: &str| format!("{{{v}}}");
            let mut ado = format!(
                "Server={};Database={}",
                quoted(&instance),
                quoted(&database)
            );
            match auth.as_str() {
                "0" => ado.push_str(";Integrated Security=True"),
                "1" | "" => {
                    if !user.is_empty() {
                        ado.push_str(&format!(";User Id={}", quoted(&user)));
                    }
                }
                // Microsoft Entra ID, in one of its forms.
                _ => {
                    ado.push_str(";Authentication=Active Directory Interactive");
                    if !user.is_empty() {
                        ado.push_str(&format!(";User Id={}", quoted(&user)));
                    }
                }
            }
            if let Some(v) = advanced("EC") {
                ado.push_str(&format!(";Encrypt={v}"));
            }
            if let Some(v) = advanced("TSC") {
                ado.push_str(&format!(";TrustServerCertificate={v}"));
            }
            let same = |c: &Candidate| {
                c.config.host.eq_ignore_ascii_case(&instance)
                    && c.config.username.as_deref().unwrap_or("") == user
            };
            if candidates.iter().any(same) {
                continue;
            }
            let name = if user.is_empty() || auth == "0" {
                instance.clone()
            } else {
                format!("{instance} ({user})")
            };
            let mut c = Candidate::new(
                format!("history:{}:{name}", candidates.len()),
                name,
                "DatabaseEngine".into(),
            );
            c.config.driver = "sqlserver".into();
            apply_ado(&mut c, &ado);
            if !text(s, "Password").is_empty() {
                *with_password += 1;
            }
            candidates.push(c);
        }
    }
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
        assert_eq!(
            (
                c[0].config.host.as_str(),
                c[0].config.database.as_str(),
                c[0].config.username.as_deref(),
                c[0].config.password.as_deref()
            ),
            ("ventas.local,14330", "ventas", Some("sa"), None)
        );
        assert_eq!(
            (
                c[0].folder.clone(),
                c[0].color.as_deref(),
                c[0].config.encrypt
            ),
            (
                vec!["Prod".to_string(), "Latam".to_string()],
                Some("#ff0000"),
                true
            )
        );
        assert_eq!(
            (c[1].config.host.as_str(), c[1].notes.len()),
            (".\\SQLEXPRESS", 1)
        );
        assert!(c[2].unsupported.is_some());
        assert_eq!(found.warnings.len(), 1);
    }

    #[test]
    fn picks_the_newest_ssms_version() {
        let dir = tempfile::tempdir().unwrap();
        for (version, with_file) in [("18.0", true), ("20.0", true), ("21.0", false), ("x", true)] {
            let d = dir.path().join(version);
            std::fs::create_dir_all(&d).unwrap();
            if with_file {
                std::fs::write(d.join("RegSrvr.xml"), "<x/>").unwrap();
            }
        }
        // 21.0 has no RegSrvr.xml and "x" isn't a version: 20.0 wins.
        assert_eq!(
            newest_in(dir.path()),
            Some(dir.path().join("20.0").join("RegSrvr.xml"))
        );
        // The folder of all versions reads the newest one.
        assert_eq!(
            read(dir.path()).unwrap().path,
            dir.path().join("20.0").join("RegSrvr.xml")
        );
        assert_eq!(newest_in(&dir.path().join("missing")), None);
    }

    /// A connection history as SSMS 18–20 write it (the structure of a real
    /// `UserSettings.xml`, with made-up servers).
    fn user_settings(servers: &[(&str, &str, &str, &str, &str, &str)], other_type: bool) -> String {
        let item = |(instance, auth, user, db, ec, tsc): &(&str, &str, &str, &str, &str, &str)| {
            let adv = |k: &str, v: &str| {
                format!("<Element><Key><string>{k}</string></Key><Value><string>{v}</string></Value></Element>")
            };
            format!(
                "<Element><Time><long>-1</long></Time><Item><ServerConnectionItem>\
                 <Instance>{instance}</Instance><AuthenticationMethod>{auth}</AuthenticationMethod>\
                 <Connections><Element><Time><long>-1</long></Time><Item><ServerConnectionSettings>\
                 <Password>AQAAANCMnd8BFdERjHoAwE</Password><Instance>{instance}</Instance>\
                 <UserName>{user}</UserName><ServerType>{DATABASE_ENGINE}</ServerType>\
                 <AuthenticationMethod>{auth}</AuthenticationMethod><Database />\
                 <Advanced>{}{}{}{}</Advanced><OtherParams /></ServerConnectionSettings></Item></Element>\
                 </Connections></ServerConnectionItem></Item></Element>",
                adv("IniDb", db),
                adv("CT", "30"),
                adv("EC", ec),
                adv("TSC", tsc)
            )
        };
        let engine: String = servers.iter().map(item).collect();
        let other = if other_type {
            format!(
                "<Element><Key><guid>8c91a03d-f9b4-46c0-a305-b5dcc79ff908</guid></Key><Value><ServerTypeItem><Servers>{}</Servers></ServerTypeItem></Value></Element>",
                item(&("olap.local", "0", "", "", "False", "False"))
            )
        } else {
            String::new()
        };
        format!(
            "\u{feff}<?xml version=\"1.0\"?><SqlStudio><SSMS><SSMSVersion>15.0.18424.0</SSMSVersion>\
             <ConnectionOptions><ServerTypes><Element><Key><guid>{DATABASE_ENGINE}</guid></Key>\
             <Value><ServerTypeItem><Servers>{engine}</Servers></ServerTypeItem></Value></Element>{other}\
             </ServerTypes></ConnectionOptions></SSMS></SqlStudio>"
        )
    }

    #[test]
    fn reads_the_connection_history() {
        let dir = tempfile::tempdir().unwrap();
        let v = dir.path().join("20.0");
        std::fs::create_dir_all(&v).unwrap();
        std::fs::write(
            v.join(HISTORY),
            user_settings(
                &[
                    (
                        "srv.database.windows.net",
                        "1",
                        "app",
                        "ventas",
                        "True",
                        "False",
                    ),
                    (
                        "srv.database.windows.net",
                        "1",
                        "admin",
                        "",
                        "True",
                        "False",
                    ),
                    (".\\SQLEXPRESS", "0", "", "", "False", "True"),
                    (
                        "entra.database.windows.net",
                        "4",
                        "ana@corp.com",
                        "",
                        "True",
                        "False",
                    ),
                ],
                true,
            ),
        )
        .unwrap();
        // The folder of all versions reads the newest one's history.
        let found = read(dir.path()).unwrap();
        assert_eq!(found.path, v.join(HISTORY));
        let c = &found.candidates;
        assert_eq!(c.len(), 4, "olap is Analysis Services: skipped");
        assert_eq!(c[0].name, "srv.database.windows.net (app)");
        assert_eq!(
            (
                c[0].config.driver.as_str(),
                c[0].config.host.as_str(),
                c[0].config.database.as_str(),
                c[0].config.username.as_deref(),
                c[0].config.password.as_deref(),
            ),
            (
                "sqlserver",
                "srv.database.windows.net",
                "ventas",
                Some("app"),
                None
            )
        );
        assert_eq!(
            c[0].config.options.get("Encrypt").map(String::as_str),
            Some("Mandatory")
        );
        // Same server, another login: its own connection.
        assert_eq!(c[1].config.username.as_deref(), Some("admin"));
        // Windows sign-in: noted; the certificate trusted as in SSMS.
        assert_eq!((c[2].name.as_str(), c[2].notes.len()), (".\\SQLEXPRESS", 1));
        assert_eq!(
            c[2].config.options.get("Encrypt").map(String::as_str),
            Some("Optional")
        );
        assert_eq!(
            c[2].config
                .options
                .get("TrustServerCertificate")
                .map(String::as_str),
            Some("true")
        );
        // Entra ID: its user and a note to pick the method.
        assert_eq!(c[3].config.username.as_deref(), Some("ana@corp.com"));
        assert!(!c[3].notes.is_empty());
        assert_eq!(found.warnings.len(), 1, "passwords stay behind");
    }

    #[test]
    fn registered_and_history_together_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(REGISTERED),
            r#"<model><RegisteredServer><Name>Ventas</Name><ServerName>db1.corp</ServerName>
                 <ConnectionStringWithEncryptedPassword>data source=db1.corp;user id=sa</ConnectionStringWithEncryptedPassword>
               </RegisteredServer></model>"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join(HISTORY),
            user_settings(
                &[
                    ("db1.corp", "1", "sa", "", "True", "False"),
                    ("db2.corp", "1", "sa", "", "True", "False"),
                ],
                false,
            ),
        )
        .unwrap();
        let found = read(dir.path()).unwrap();
        let names: Vec<&str> = found.candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["Ventas", "db2.corp (sa)"]);
    }

    #[test]
    fn finds_ssms_21_folders_by_version() {
        let dir = tempfile::tempdir().unwrap();
        for d in ["20.0", "21.0_a1b2c3d4"] {
            std::fs::create_dir_all(dir.path().join(d)).unwrap();
            std::fs::write(dir.path().join(d).join(HISTORY), "<x/>").unwrap();
        }
        assert_eq!(
            newest_with(dir.path(), HISTORY),
            Some(dir.path().join("21.0_a1b2c3d4").join(HISTORY))
        );
        assert_eq!(version_of(&dir.path().join("21.0_a1b2c3d4")), 21);
    }

    #[test]
    fn folder_of_only_follows_server_groups() {
        assert_eq!(
            folder_of("/RegisteredServersStore/ServerGroup/DatabaseEngineServerGroup/ServerGroup/A%20B/ServerGroup/C"),
            vec!["A B".to_string(), "C".to_string()]
        );
        assert!(folder_of("/RegisteredServersStore/ServerGroup/Other/ServerGroup/A").is_empty());
        assert!(folder_of("").is_empty());
        // A trailing half pair is ignored.
        assert_eq!(
            folder_of("/x/DatabaseEngineServerGroup/ServerGroup/A/ServerGroup"),
            vec!["A".to_string()]
        );
    }

    #[test]
    fn minimal_server_uses_its_server_name() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("export.regsrvr");
        std::fs::write(
            &f,
            r#"<model><RegisteredServer>
                 <ServerName> db1.corp </ServerName>
                 <UseCustomConnectionColor>false</UseCustomConnectionColor>
                 <CustomConnectionColorArgb>-65536</CustomConnectionColorArgb>
               </RegisteredServer></model>"#,
        )
        .unwrap();
        let found = read(&f).unwrap();
        assert_eq!(found.path, f);
        let c = &found.candidates[0];
        // No Name or ServerType: the server name, a database engine.
        assert_eq!(c.name, "db1.corp");
        assert_eq!(c.config.driver, "sqlserver");
        assert_eq!(c.config.host, "db1.corp");
        assert!(c.unsupported.is_none());
        // The color only applies when it's turned on.
        assert_eq!(c.color, None);
        assert!(c.folder.is_empty());
        assert!(found.warnings.is_empty());
    }

    #[test]
    fn unreadable_or_invalid_files_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read(&dir.path().join("missing.regsrvr")).is_err());
        // An empty folder looks for its RegSrvr.xml, which isn't there.
        assert!(read(dir.path()).is_err());
        let bad = dir.path().join("bad.regsrvr");
        std::fs::write(&bad, "<model><unclosed></model>").unwrap();
        let Err(err) = read(&bad) else {
            panic!("invalid XML was read");
        };
        let err = err.to_string();
        assert!(err.contains("XML"), "{err}");
    }
}
