//! DataGrip and the other JetBrains IDEs (IntelliJ, PyCharm, Rider…):
//!
//! - global data sources in `<config>/JetBrains/<Product><version>/options/dataSources.xml`;
//! - project ones in `<project>/.idea/dataSources.xml`; the projects come
//!   from each IDE's `recentProjects.xml`;
//! - the user names in the `dataSources.local.xml` next to them;
//! - the passwords in the system keychain, under
//!   `IntelliJ Platform DB — <uuid>` (read only on import).

use super::{app_data, apply_jdbc, home, Candidate, Found, KeychainSecret};
use dbine_driver::{Error, Result};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

fn base() -> Option<PathBuf> {
    Some(app_data()?.join("JetBrains"))
}

pub fn default_location() -> Option<PathBuf> {
    let b = base()?;
    (!config_dirs(&b).is_empty()).then_some(b)
}

/// `<Product><version>` folders with data sources or recent projects,
/// newest first.
fn config_dirs(base: &Path) -> Vec<PathBuf> {
    let mut v: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(base)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("options/dataSources.xml").is_file() || p.join("options/recentProjects.xml").is_file())
        .map(|p| (std::fs::metadata(p.join("options")).and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), p))
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0));
    v.into_iter().map(|(_, p)| p).collect()
}

/// Projects listed in `recentProjects.xml`.
fn recent_projects(config: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(config.join("options/recentProjects.xml")) else { return Vec::new() };
    let Ok(doc) = roxmltree::Document::parse(&text) else { return Vec::new() };
    let home = home().map(|h| h.display().to_string()).unwrap_or_default();
    doc.descendants()
        .filter(|n| n.has_tag_name("entry"))
        .filter_map(|n| n.attribute("key"))
        .map(|k| PathBuf::from(k.replace("$USER_HOME$", &home)))
        .filter(|p| p.join(".idea/dataSources.xml").is_file())
        .collect()
}

/// A `dataSources.xml` to read, and the folder its connections go in.
struct SourceFile {
    file: PathBuf,
    folder: Vec<String>,
}

fn files_under(path: &Path) -> Vec<SourceFile> {
    let one = |file: PathBuf, folder: Vec<String>| SourceFile { file, folder };
    if path.is_file() {
        return vec![one(path.to_path_buf(), Vec::new())];
    }
    if path.join(".idea/dataSources.xml").is_file() {
        return vec![one(path.join(".idea/dataSources.xml"), Vec::new())];
    }
    if path.join("dataSources.xml").is_file() {
        return vec![one(path.join("dataSources.xml"), Vec::new())];
    }
    if path.join("options/dataSources.xml").is_file() || path.join("options/recentProjects.xml").is_file() {
        return from_config(path);
    }
    // The JetBrains folder: every IDE.
    config_dirs(path).iter().flat_map(|c| from_config(c)).collect()
}

fn from_config(config: &Path) -> Vec<SourceFile> {
    let mut out = Vec::new();
    if config.join("options/dataSources.xml").is_file() {
        out.push(SourceFile { file: config.join("options/dataSources.xml"), folder: Vec::new() });
    }
    for p in recent_projects(config) {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("proyecto").to_string();
        out.push(SourceFile { file: p.join(".idea/dataSources.xml"), folder: vec![name] });
    }
    out
}

pub fn read(path: &Path) -> Result<Found> {
    let files = files_under(path);
    if files.is_empty() {
        return Err(Error::Query(format!("no hay conexiones de JetBrains (dataSources.xml) en {}", path.display())));
    }
    let mut candidates = Vec::new();
    let mut warnings = Vec::new();
    // The same data source shows up in several IDE versions: the newest wins.
    let mut seen = HashSet::new();
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f.file) else { continue };
        let doc = match roxmltree::Document::parse(&text) {
            Ok(d) => d,
            Err(e) => {
                warnings.push(format!("{} no se pudo leer: {e}", f.file.display()));
                continue;
            }
        };
        let users = local_users(&f.file.with_file_name("dataSources.local.xml"));
        for ds in doc.descendants().filter(|n| n.has_tag_name("data-source")) {
            let uuid = ds.attribute("uuid").unwrap_or("").to_string();
            if uuid.is_empty() || !seen.insert(uuid.clone()) {
                continue;
            }
            let user = users.get(&uuid).cloned().or_else(|| child_text(ds, "user-name"));
            let mut c = map(ds, user);
            let mut folder = f.folder.clone();
            folder.extend(c.folder.drain(..));
            c.folder = folder;
            candidates.push(c);
        }
    }
    Ok(Found { path: path.to_path_buf(), candidates, warnings })
}

fn child_text(n: roxmltree::Node, tag: &str) -> Option<String> {
    n.children().find(|c| c.has_tag_name(tag)).and_then(|c| c.text()).map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

/// uuid → user name, from `dataSources.local.xml`.
fn local_users(file: &Path) -> HashMap<String, String> {
    let Ok(text) = std::fs::read_to_string(file) else { return HashMap::new() };
    let Ok(doc) = roxmltree::Document::parse(&text) else { return HashMap::new() };
    doc.descendants()
        .filter(|n| n.has_tag_name("data-source"))
        .filter_map(|n| Some((n.attribute("uuid")?.to_string(), child_text(n, "user-name")?)))
        .collect()
}

fn map(ds: roxmltree::Node, user: Option<String>) -> Candidate {
    let uuid = ds.attribute("uuid").unwrap_or("");
    let driver_ref = child_text(ds, "driver-ref").unwrap_or_default();
    let url = child_text(ds, "jdbc-url").unwrap_or_default();
    let name = ds.attribute("name").unwrap_or(uuid).to_string();
    let mut c = Candidate::new(uuid.to_string(), name, driver_ref.clone());
    c.folder = ds.attribute("group").unwrap_or("").split('/').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect();
    c.config.read_only = ds.attribute("read-only") == Some("true");
    if !c.set_driver(&format!("{driver_ref} {url}")) {
        return c;
    }
    if c.config.driver == "h2" && !url.contains("tcp://") {
        c.unsupported = Some("H2 embebido no está en DBine (solo H2 en modo servidor)".into());
        return c;
    }
    if !apply_jdbc(&mut c, &url) {
        c.notes.push("No se pudo leer la URL de conexión: revisá el servidor en DBine.".into());
    }
    if c.config.username.is_none() {
        c.config.username = user;
    }
    if let Some(account) = c.config.username.clone() {
        c.keychain = Some(KeychainSecret { service: format!("IntelliJ Platform DB \u{2014} {uuid}"), account });
    }
    for p in ds.descendants().filter(|n| n.has_tag_name("property")) {
        let (k, v) = (p.attribute("name").unwrap_or("").to_ascii_lowercase(), p.attribute("value").unwrap_or(""));
        match k.as_str() {
            "sslmode" | "ssl" | "encrypt" | "usessl" => c.config.encrypt = super::truthy(v),
            "trustservercertificate" => c.config.trust_server_certificate = super::truthy(v),
            _ => {}
        }
    }
    if ds.descendants().any(|n| n.has_tag_name("ssh-properties") && child_text(n, "enabled").as_deref() == Some("true")) {
        c.notes.push("El túnel SSH no se importa: configuralo aparte o conectate por la red.".into());
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_global_and_project_sources() {
        let root = tempfile::tempdir().unwrap();
        let cfg = root.path().join("DataGrip2025.1");
        std::fs::create_dir_all(cfg.join("options")).unwrap();
        let project = root.path().join("proj/tienda");
        std::fs::create_dir_all(project.join(".idea")).unwrap();
        std::fs::write(
            cfg.join("options/dataSources.xml"),
            r#"<application><component name="dataSourceStorage">
              <data-source source="LOCAL" name="PG prod" uuid="u1" group="Prod" read-only="true">
                <driver-ref>postgresql</driver-ref><jdbc-url>jdbc:postgresql://pg.local:5433/app</jdbc-url><user-name>ana</user-name>
              </data-source>
              <data-source source="LOCAL" name="BQ" uuid="u2"><driver-ref>bigquery</driver-ref><jdbc-url>jdbc:bigquery://x</jdbc-url></data-source>
            </component></application>"#,
        )
        .unwrap();
        std::fs::write(
            cfg.join("options/recentProjects.xml"),
            format!(r#"<application><component name="RecentProjectsManager"><option name="additionalInfo"><map><entry key="{}"/></map></option></component></application>"#, project.display()),
        )
        .unwrap();
        std::fs::write(
            project.join(".idea/dataSources.xml"),
            r#"<project version="4"><component name="DataSourceManagerImpl">
              <data-source source="LOCAL" name="Ventas" uuid="u3"><driver-ref>sqlserver.ms</driver-ref>
                <jdbc-url>jdbc:sqlserver://sql.local:1433;database=ventas;trustServerCertificate=true</jdbc-url></data-source>
              <data-source source="LOCAL" name="PG prod (copia)" uuid="u1"><driver-ref>postgresql</driver-ref><jdbc-url>jdbc:postgresql://x/y</jdbc-url></data-source>
            </component></project>"#,
        )
        .unwrap();
        std::fs::write(project.join(".idea/dataSources.local.xml"), r#"<project><component name="dataSourceStorageLocal"><data-source name="Ventas" uuid="u3"><user-name>sa</user-name></data-source></component></project>"#).unwrap();

        let found = read(root.path()).unwrap();
        let c = &found.candidates;
        assert_eq!(c.len(), 3, "duplicate uuid skipped");
        let pg = &c[0];
        assert_eq!((pg.config.driver.as_str(), pg.config.host.as_str(), pg.config.port, pg.config.database.as_str()), ("postgres", "pg.local", 5433, "app"));
        assert_eq!((pg.config.username.as_deref(), pg.config.read_only, pg.folder.clone()), (Some("ana"), true, vec!["Prod".to_string()]));
        assert_eq!(pg.keychain.as_ref().unwrap().service, "IntelliJ Platform DB \u{2014} u1");
        assert!(c[1].unsupported.is_some());
        let ms = &c[2];
        assert_eq!((ms.config.driver.as_str(), ms.config.database.as_str(), ms.config.username.as_deref(), ms.config.trust_server_certificate), ("sqlserver", "ventas", Some("sa"), true));
        assert_eq!(ms.folder, vec!["tienda".to_string()]);
    }
}
