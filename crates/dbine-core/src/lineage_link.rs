//! Cross-connection lineage: the identity of "the same server and database"
//! seen from a saved connection and from a source another system names (a
//! Power BI table's Power Query source), the matcher between them, and the
//! links the user made by hand (`lineage_links`).
//!
//! A [`SourceKey`] is canonical: lowercase, `.`/`localhost`/`(local)`/
//! `127.0.0.1`/the machine's name are one host, the default port of the
//! engine is filled in, SQL Server's `server\instance` and `server,port`
//! are split, brackets and quotes are gone, a Snowflake account or a
//! Databricks workspace URL is its host. No secret is ever part of it.

use crate::state::{db_err, now, StateStore};
use crate::{Result, SavedConnection};
use dbine_driver::dependencies::ExternalRef;
use dbine_driver::{ConnectionConfig, Error};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::OnceLock;

/// The table, created on open (older state files gain it then). Local to
/// the machine: it is not part of a backup or of the sync.
pub(crate) const TABLE: &str = "CREATE TABLE IF NOT EXISTS lineage_links (
    signature     TEXT PRIMARY KEY,
    connection_id TEXT NOT NULL,
    updated_at    TEXT NOT NULL
)";

/// Which server and database a source is, canonically.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceKey {
    /// Driver ids that talk to the same kind of server share it
    /// (`sqlserver`, `postgres`, `mysql`, `oracle`, `databricks`,
    /// `snowflake`; any other engine is its own, by its driver id).
    pub family: String,
    /// Lowercase; every spelling of "this machine" is `localhost`.
    pub host: String,
    /// The TCP port; `0` = not known (a named SQL Server instance, an
    /// engine with no default). A `0` matches any port.
    pub port: u16,
    /// SQL Server's named instance, lowercase; empty when none.
    pub instance: String,
    /// The database (catalog, Oracle's service), lowercase; empty when the
    /// source names none.
    pub database: String,
}

impl SourceKey {
    /// A stable string of the key, to remember a link by.
    pub fn signature(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            self.family, self.host, self.port, self.instance, self.database
        )
    }

    fn same_server(&self, other: &SourceKey) -> bool {
        self.family == other.family
            && self.host == other.host
            && self.instance == other.instance
            && (self.port == other.port || self.port == 0 || other.port == 0)
    }
}

/// The saved connections a source matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Match {
    None,
    One(String),
    Many(Vec<String>),
}

// -- the families -------------------------------------------------------

/// The family of a driver id.
fn family_of_driver(id: &str) -> String {
    let id = id.trim().to_lowercase();
    let f = match id.as_str() {
        "sqlserver" | "sqlserver_legacy" | "azuresql" | "fabric" | "babelfish" | "synapse" => {
            "sqlserver"
        }
        "postgres" | "postgresql" | "cockroachdb" | "redshift" | "greenplum" | "yugabytedb"
        | "timescaledb" | "kingbase" | "denodo" | "alloydb" | "edb" | "fujitsu" | "opengauss"
        | "cloudberry" | "greengage" | "risingwave" | "materialize" | "cratedb" | "yellowbrick"
        | "h2" | "aurora-postgres" | "cloudsql-postgres" => "postgres",
        "mysql" | "mariadb" | "tidb" | "oceanbase" | "singlestore" | "starrocks" | "doris"
        | "databend" | "manticore" | "greptimedb" | "aurora-mysql" | "velodb"
        | "cloudsql-mysql" => "mysql",
        "oracle" | "oracle_adb" => "oracle",
        "databricks" | "azure_databricks" => "databricks",
        "snowflake" => "snowflake",
        other => return other.to_string(),
    };
    f.to_string()
}

/// The family of the system a Power BI source names, `None` for the ones
/// that are not a server (files, the web, SharePoint, ODBC strings).
fn family_of_system(system: &str) -> Option<String> {
    let s = system.trim().to_lowercase();
    match s.as_str() {
        "sql server" => Some("sqlserver".into()),
        "postgresql" => Some("postgres".into()),
        "mysql" => Some("mysql".into()),
        "oracle" => Some("oracle".into()),
        "databricks" => Some("databricks".into()),
        "snowflake" => Some("snowflake".into()),
        "" | "odbc" | "excel" | "csv" | "file" | "folder" | "sharepoint" | "web" | "xml"
        | "json" | "pdf" | "text" | "html" | "oledb" => None,
        other => Some(other.replace(' ', "")),
    }
}

/// The default port of a family, refined by the driver id when known.
fn default_port(family: &str, driver: Option<&str>) -> u16 {
    let driver = driver.map(str::to_lowercase);
    let d = driver.as_deref().unwrap_or("");
    match family {
        "sqlserver" => 1433,
        "postgres" => match d {
            "cockroachdb" => 26257,
            "redshift" => 5439,
            "yugabytedb" => 5433,
            "kingbase" => 54321,
            "denodo" => 9996,
            "edb" => 5444,
            "fujitsu" => 27500,
            "risingwave" => 4566,
            "materialize" => 6875,
            "h2" => 5435,
            _ => 5432,
        },
        "mysql" => match d {
            "tidb" => 4000,
            "oceanbase" => 2881,
            "starrocks" | "doris" => 9030,
            "databend" => 3307,
            "manticore" => 9306,
            "greptimedb" => 4002,
            _ => 3306,
        },
        "oracle" => 1521,
        "databricks" | "snowflake" => 443,
        _ => 0,
    }
}

// -- the host ------------------------------------------------------------

/// The names this machine answers to, lowercase (its full name and the
/// first label).
fn machine_names() -> &'static HashSet<String> {
    static NAMES: OnceLock<HashSet<String>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut raw: Vec<String> = ["COMPUTERNAME", "HOSTNAME", "HOST"]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .collect();
        if let Ok(s) = std::fs::read_to_string("/etc/hostname") {
            raw.push(s);
        }
        if raw.iter().all(|r| r.trim().is_empty()) {
            if let Ok(o) = std::process::Command::new("hostname").output() {
                raw.push(String::from_utf8_lossy(&o.stdout).into_owned());
            }
        }
        let mut set = HashSet::new();
        for r in raw {
            let r = r.trim().to_lowercase();
            if r.is_empty() {
                continue;
            }
            if let Some((first, _)) = r.split_once('.') {
                set.insert(first.to_string());
            }
            set.insert(r);
        }
        set
    })
}

/// Surrounding quotes or brackets, whitespace.
fn unquote(s: &str) -> &str {
    let mut s = s.trim();
    loop {
        let t = match (s.chars().next(), s.chars().last()) {
            (Some('"'), Some('"')) | (Some('\''), Some('\'')) | (Some('['), Some(']'))
                if s.len() >= 2 =>
            {
                s[1..s.len() - 1].trim()
            }
            _ => return s,
        };
        s = t;
    }
}

fn normalize_host(host: &str, machine: &HashSet<String>) -> String {
    let h = unquote(host).trim_end_matches('.').to_lowercase();
    match h.as_str() {
        "" | "." | "localhost" | "(local)" | "127.0.0.1" | "::1" | "0.0.0.0" | "(localdb)" => {
            "localhost".into()
        }
        _ if machine.contains(&h) => "localhost".into(),
        _ => h,
    }
}

#[derive(Debug, Default)]
struct Server {
    host: String,
    port: Option<u16>,
    instance: String,
    /// What follows a `/` (Oracle's service).
    path: String,
}

/// Takes a server string apart (the case as typed): `[tcp:|https://]host[\instance][,port|:port][/path]`
/// with the host and the instance possibly in brackets or quotes.
fn parse_server(raw: &str) -> Server {
    let mut s = raw.trim();
    s = s.trim_matches(|c| c == '"' || c == '\'').trim();
    if let Some(i) = s.find("://") {
        s = &s[i + 3..];
    }
    let lower = s.to_lowercase();
    for p in ["tcp:", "np:", "lpc:"] {
        if lower.starts_with(p) {
            s = &s[p.len()..];
        }
    }
    let s = s.split(['?', '#']).next().unwrap_or(s);
    let (hp, path) = match s.split_once('/') {
        Some((h, p)) => (h, p.trim_matches('/').trim()),
        None => (s, ""),
    };
    let hp = hp.trim();
    let mut out = Server {
        path: unquote(path).to_string(),
        ..Default::default()
    };
    // The host: bracketed, or up to the first `\`, `,` or `:` (an
    // unbracketed IPv6 address has several colons and no port).
    let (host, mut rest) = if hp.starts_with('[') || hp.starts_with('"') {
        let close = if hp.starts_with('[') { ']' } else { '"' };
        match hp[1..].find(close) {
            Some(i) => (&hp[1..1 + i], &hp[i + 2..]),
            None => (hp, ""),
        }
    } else if hp.matches(':').count() > 1 {
        (hp, "")
    } else {
        match hp.find(['\\', ',', ':']) {
            Some(i) => (&hp[..i], &hp[i..]),
            None => (hp, ""),
        }
    };
    out.host = unquote(host).to_string();
    rest = rest.trim();
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('\\') {
            let r = r.trim_start();
            let (name, after) = if r.starts_with('[') || r.starts_with('"') {
                let close = if r.starts_with('[') { ']' } else { '"' };
                match r[1..].find(close) {
                    Some(i) => (&r[1..1 + i], &r[i + 2..]),
                    None => (r, ""),
                }
            } else {
                match r.find([',', ':']) {
                    Some(i) => (&r[..i], &r[i..]),
                    None => (r, ""),
                }
            };
            out.instance = unquote(name).to_string();
            rest = after.trim();
        } else if let Some(r) = rest.strip_prefix([',', ':']) {
            let digits: String = r
                .trim_start()
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            out.port = digits.parse().ok().filter(|p| *p != 0);
            break;
        } else {
            break;
        }
    }
    out
}

fn lower(s: &str) -> String {
    unquote(s).to_lowercase()
}

// -- building keys ---------------------------------------------------------

/// The key of a Power BI (or another system's) source. `None` when it is
/// not a server we can match: a file, the web, a connection string, no
/// server named.
pub fn source_key_of_external(ext: &ExternalRef) -> Option<SourceKey> {
    key_of_external(ext, machine_names())
}

/// The key of a saved connection's config. `None` for the ones with no
/// server of their own: embedded files, and the connections DBine
/// Enterprise manages (they hold no host).
pub fn source_key_of_config(cfg: &ConnectionConfig) -> Option<SourceKey> {
    key_of_config(cfg, machine_names())
}

fn key_of_external(ext: &ExternalRef, machine: &HashSet<String>) -> Option<SourceKey> {
    let family = family_of_system(ext.system.as_deref()?)?;
    let server = ext.server.as_deref()?.trim();
    // An Oracle descriptor (`(DESCRIPTION=…)`) is not a host.
    if server.is_empty() || (family == "oracle" && server.starts_with('(')) {
        return None;
    }
    let database = ext.database.as_deref().map(lower).unwrap_or_default();
    build(&family, None, server, database, true, machine)
}

fn key_of_config(cfg: &ConnectionConfig, machine: &HashSet<String>) -> Option<SourceKey> {
    let driver = cfg.driver.trim().to_lowercase();
    let family = family_of_driver(&driver);
    let (server, database) = match family.as_str() {
        "snowflake" => (
            cfg.option("account")
                .unwrap_or(cfg.host.as_str())
                .to_string(),
            lower(&cfg.database),
        ),
        "oracle" => {
            if cfg.option("connect_descriptor").is_some() {
                return None;
            }
            let service = cfg.option("service").map(lower).unwrap_or_default();
            let service = if service.is_empty() {
                lower(&cfg.database)
            } else {
                service
            };
            (cfg.host.clone(), service)
        }
        _ => (cfg.host.clone(), lower(&cfg.database)),
    };
    let server = server.trim();
    if server.is_empty() {
        return None;
    }
    // A connection to a file or an in-memory engine has no network host.
    if matches!(
        driver.as_str(),
        "sqlite" | "duckdb" | "duckdb_files" | "libsql" | "access" | "dbase"
    ) {
        return None;
    }
    let mut k = build(&family, Some(&driver), server, database, false, machine)?;
    if cfg.port != 0 && !matches!(family.as_str(), "databricks" | "snowflake") {
        k.port = cfg.port;
    }
    Some(k)
}

fn build(
    family: &str,
    driver: Option<&str>,
    server: &str,
    database: String,
    external: bool,
    machine: &HashSet<String>,
) -> Option<SourceKey> {
    let default = default_port(family, driver);
    let (host, port, instance, database) = match family {
        "snowflake" => {
            let p = parse_server(server);
            let p = Server {
                host: normalize_host(&p.host, machine),
                instance: p.instance.to_lowercase(),
                path: p.path.to_lowercase(),
                ..p
            };
            let mut host = p.host;
            if host.is_empty() || host == "localhost" {
                return None;
            }
            if !host.contains(".snowflakecomputing.") {
                host = format!("{host}.snowflakecomputing.com");
            }
            (host, default, String::new(), database)
        }
        "databricks" => {
            let p = parse_server(server);
            let p = Server {
                host: normalize_host(&p.host, machine),
                instance: p.instance.to_lowercase(),
                path: p.path.to_lowercase(),
                ..p
            };
            (p.host, default, String::new(), database)
        }
        "oracle" => {
            let p = parse_server(server);
            let p = Server {
                host: normalize_host(&p.host, machine),
                instance: p.instance.to_lowercase(),
                path: p.path.to_lowercase(),
                ..p
            };
            // `host:port/service`: the service is the database.
            let db = if p.path.is_empty() { database } else { p.path };
            (p.host, p.port.unwrap_or(default), String::new(), db)
        }
        "sqlserver" => {
            let p = parse_server(server);
            let p = Server {
                host: normalize_host(&p.host, machine),
                instance: p.instance.to_lowercase(),
                path: p.path.to_lowercase(),
                ..p
            };
            // A named instance is found through the browser service: no port.
            let port = match (p.port, p.instance.is_empty()) {
                (Some(port), _) => port,
                (None, true) => default,
                (None, false) => 0,
            };
            (p.host, port, p.instance, database)
        }
        _ => {
            let p = parse_server(server);
            let p = Server {
                host: normalize_host(&p.host, machine),
                instance: p.instance.to_lowercase(),
                path: p.path.to_lowercase(),
                ..p
            };
            (p.host, p.port.unwrap_or(default), String::new(), database)
        }
    };
    if host.is_empty() {
        return None;
    }
    // A source from outside with no host of its own to a file path.
    if external && (host.contains(['/', '\\']) || host.len() > 253) {
        return None;
    }
    Some(SourceKey {
        family: family.to_string(),
        host,
        port,
        instance,
        database,
    })
}

// -- matching ----------------------------------------------------------------

/// The saved connections that are `key`'s server. A connection naming the
/// same database wins over the ones naming none (a default database may
/// still hold it); two named databases are different. Managed (Enterprise)
/// connections hold no host and are not here: the server resolves them.
pub fn match_connections(key: &SourceKey, conns: &[SavedConnection]) -> Match {
    match_with(key, conns, machine_names())
}

fn match_with(key: &SourceKey, conns: &[SavedConnection], machine: &HashSet<String>) -> Match {
    let mut strong = Vec::new();
    let mut weak = Vec::new();
    for c in conns {
        let Some(k) = key_of_config(&c.config, machine) else {
            continue;
        };
        if !key.same_server(&k) {
            continue;
        }
        if key.database == k.database {
            strong.push(c.id.clone());
        } else if key.database.is_empty() || k.database.is_empty() {
            weak.push(c.id.clone());
        }
    }
    let mut ids = if strong.is_empty() { weak } else { strong };
    match ids.len() {
        0 => Match::None,
        1 => Match::One(ids.remove(0)),
        _ => Match::Many(ids),
    }
}

// -- creating a connection from a source --------------------------------------

/// A connection's config prefilled from a source, to create it from "Crear
/// conexión desde la fuente…": the engine, host, port and database; no
/// credentials. `None` when the source is not a server.
pub fn new_connection_from_source(ext: &ExternalRef) -> Option<ConnectionConfig> {
    let machine = machine_names();
    let key = key_of_external(ext, machine)?;
    let server = ext.server.as_deref()?.trim();
    let p = parse_server(server);
    let database = ext
        .database
        .as_deref()
        .map(|d| unquote(d).to_string())
        .unwrap_or_default();
    let mut cfg = ConnectionConfig {
        // A connection made to follow a lineage only reads: never a write nobody asked for.
        read_only: true,
        ..ConnectionConfig::default()
    };
    match key.family.as_str() {
        "sqlserver" => {
            cfg.driver = if key.host.ends_with(".database.windows.net") {
                "azuresql"
            } else if key.host.ends_with(".fabric.microsoft.com") {
                "fabric"
            } else {
                "sqlserver"
            }
            .into();
            // The host as typed, without brackets, quotes or the port;
            // `server\instance` stays together as the driver reads it.
            cfg.host = if p.instance.is_empty() {
                p.host.clone()
            } else {
                format!("{}\\{}", p.host, p.instance)
            };
            if let Some(port) = p.port {
                cfg.port = port;
            }
            cfg.database = database;
        }
        "postgres" | "mysql" => {
            cfg.driver = key.family.clone();
            cfg.host = p.host.clone();
            if let Some(port) = p.port {
                cfg.port = port;
            }
            cfg.database = database;
        }
        "oracle" => {
            cfg.driver = "oracle".into();
            cfg.host = p.host.clone();
            if let Some(port) = p.port {
                cfg.port = port;
            }
            let service = if p.path.is_empty() {
                database
            } else {
                p.path.clone()
            };
            cfg.options.insert("service".into(), service.clone());
            cfg.options
                .insert("connect_by".into(), "service_name".into());
            cfg.database = service;
        }
        "databricks" => {
            cfg.driver = if key.host.contains("azuredatabricks.net") {
                "azure_databricks"
            } else {
                "databricks"
            }
            .into();
            cfg.host = key.host.clone();
            cfg.database = database;
            // The SQL warehouse's HTTP path, when the source names it.
            if let Some(w) = ext
                .detail
                .as_deref()
                .and_then(|d| d.split_whitespace().find(|t| t.starts_with("/sql/")))
            {
                cfg.options.insert("warehouse".into(), w.to_string());
            }
        }
        "snowflake" => {
            cfg.driver = "snowflake".into();
            cfg.host = key.host.clone();
            cfg.options.insert("account".into(), key.host.clone());
            cfg.database = database;
        }
        // Another engine: DBine has a driver of that id or it doesn't.
        other => {
            cfg.driver = other.to_string();
            cfg.host = p.host.clone();
            if let Some(port) = p.port {
                cfg.port = port;
            }
            cfg.database = database;
        }
    }
    Some(cfg)
}

// -- the links the user made --------------------------------------------------

impl StateStore {
    /// The connection the user linked `signature` (a [`SourceKey::signature`])
    /// to, if it still exists; a link to a deleted connection is dropped.
    pub fn lineage_link_get(&self, signature: &str) -> Result<Option<String>> {
        let c = self.lock()?;
        let id: Option<String> = c
            .query_row(
                "SELECT connection_id FROM lineage_links WHERE signature = ?1",
                [signature],
                |r| r.get(0),
            )
            .optional()
            .map_err(db_err)?;
        let Some(id) = id else { return Ok(None) };
        let exists: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM connections WHERE id = ?1",
                [&id],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        if exists > 0 {
            return Ok(Some(id));
        }
        c.execute(
            "DELETE FROM lineage_links WHERE signature = ?1",
            [signature],
        )
        .map_err(db_err)?;
        Ok(None)
    }

    /// Links `signature` to a saved connection (replacing an older link).
    pub fn lineage_link_set(&self, signature: &str, connection_id: &str) -> Result<()> {
        let c = self.lock()?;
        let exists: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM connections WHERE id = ?1",
                [connection_id],
                |r| r.get(0),
            )
            .map_err(db_err)?;
        if exists == 0 {
            return Err(Error::State("la conexión ya no existe".into()));
        }
        c.execute(
            "INSERT INTO lineage_links (signature, connection_id, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(signature) DO UPDATE SET connection_id = ?2, updated_at = ?3",
            params![signature, connection_id, now()],
        )
        .map_err(db_err)?;
        Ok(())
    }

    /// Forgets a link (none: nothing happens).
    pub fn lineage_link_remove(&self, signature: &str) -> Result<()> {
        self.lock()?
            .execute(
                "DELETE FROM lineage_links WHERE signature = ?1",
                [signature],
            )
            .map_err(db_err)?;
        Ok(())
    }

    /// What `ext` resolves to among `conns`: the user's link when it still
    /// holds, else the automatic match (see [`match_connections`]).
    pub fn lineage_resolve_source(
        &self,
        ext: &ExternalRef,
        conns: &[SavedConnection],
    ) -> Result<Match> {
        let Some(key) = source_key_of_external(ext) else {
            return Ok(Match::None);
        };
        if let Some(id) = self.lineage_link_get(&key.signature())? {
            return Ok(Match::One(id));
        }
        Ok(match_connections(&key, conns))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> HashSet<String> {
        HashSet::new()
    }

    fn machine() -> HashSet<String> {
        ["devbox".to_string()].into_iter().collect()
    }

    fn ext(system: &str, server: &str, database: Option<&str>) -> ExternalRef {
        ExternalRef {
            system: Some(system.into()),
            server: Some(server.into()),
            database: database.map(Into::into),
            ..Default::default()
        }
    }

    fn cfg(driver: &str, host: &str, port: u16, database: &str) -> ConnectionConfig {
        ConnectionConfig {
            driver: driver.into(),
            host: host.into(),
            port,
            database: database.into(),
            ..Default::default()
        }
    }

    fn saved(id: &str, config: ConnectionConfig) -> SavedConnection {
        SavedConnection {
            id: id.into(),
            name: id.into(),
            color: None,
            config,
            save_password: false,
            folder_id: None,
            tags: vec![],
            mcp_level: None,
            updated_at: String::new(),
        }
    }

    fn ek(system: &str, server: &str, db: &str) -> SourceKey {
        key_of_external(&ext(system, server, Some(db)), &machine()).unwrap()
    }

    fn ck(driver: &str, host: &str, port: u16, db: &str) -> SourceKey {
        key_of_config(&cfg(driver, host, port, db), &machine()).unwrap()
    }

    #[test]
    fn every_spelling_of_this_machine_is_localhost() {
        for h in [
            ".",
            "localhost",
            "LOCALHOST",
            "(local)",
            "127.0.0.1",
            "[localhost]",
            "DevBox",
            "devbox.",
            "::1",
        ] {
            assert_eq!(ck("sqlserver", h, 0, "d").host, "localhost", "{h}");
        }
        assert_eq!(ck("sqlserver", "[::1]", 0, "d").host, "localhost");
        // Another machine stays itself.
        assert_eq!(ck("sqlserver", "Other", 0, "d").host, "other");
    }

    #[test]
    fn sql_server_instances_and_ports() {
        let a = ek("SQL Server", r"DevBox\SQLEXPRESS", "DW");
        assert_eq!(
            (a.host.as_str(), a.instance.as_str(), a.port),
            ("localhost", "sqlexpress", 0)
        );
        assert_eq!(a, ck("sqlserver", r"[.]\[SQLExpress]", 0, "dw"));
        assert_eq!(a, ck("sqlserver", r"(local)\SQLEXPRESS", 0, "Dw"));
        // `server,port` and a separate port are the same.
        let b = ek("SQL Server", "srv,1444", "dw");
        assert_eq!(b.port, 1444);
        assert_eq!(b, ck("sqlserver", "srv", 1444, "dw"));
        assert_eq!(b, ck("sqlserver", "SRV,1444", 0, "DW"));
        // The default port fills in.
        let c = ek("SQL Server", "srv", "dw");
        assert_eq!(c.port, 1433);
        assert_eq!(c, ck("sqlserver", "srv", 1433, "dw"));
        assert_eq!(c, ck("sqlserver", "tcp:srv,1433", 0, "dw"));
        // Instance and port together.
        let d = ek("SQL Server", r"srv\inst,1500", "dw");
        assert_eq!((d.instance.as_str(), d.port), ("inst", 1500));
        // Another instance is another server.
        assert!(!ek("SQL Server", r"srv\a", "dw").same_server(&ek("SQL Server", r"srv\b", "dw")));
        assert!(!ek("SQL Server", "srv", "dw").same_server(&ek("SQL Server", r"srv\b", "dw")));
        // A port that differs is another server.
        assert!(!ek("SQL Server", "srv,1444", "dw").same_server(&ek("SQL Server", "srv", "dw")));
    }

    #[test]
    fn the_sql_server_family_shares_azure_and_fabric() {
        let az = ek("SQL Server", "tcp:Acme.Database.Windows.net,1433", "sales");
        assert_eq!(az.host, "acme.database.windows.net");
        assert_eq!(az, ck("azuresql", "acme.database.windows.net", 0, "Sales"));
        assert_eq!(
            az,
            ck("sqlserver", "acme.database.windows.net", 1433, "sales")
        );
        let fab = ek(
            "SQL Server",
            "abc123.datawarehouse.fabric.microsoft.com",
            "wh",
        );
        assert_eq!(
            fab,
            ck(
                "fabric",
                "abc123.datawarehouse.fabric.microsoft.com",
                0,
                "wh"
            )
        );
        assert_eq!(fab.family, "sqlserver");
        assert_eq!(ck("sqlserver_legacy", "s", 0, "d").family, "sqlserver");
        assert_eq!(ck("babelfish", "s", 0, "d").family, "sqlserver");
    }

    #[test]
    fn databricks_workspace_urls_and_hosts() {
        let host = "adb-1234567890123456.7.azuredatabricks.net";
        let e = ek("Databricks", host, "main");
        assert_eq!((e.host.as_str(), e.port), (host, 443));
        assert_eq!(e, ck("azure_databricks", host, 0, "Main"));
        assert_eq!(
            e,
            ck("azure_databricks", &format!("https://{host}/"), 0, "main")
        );
        assert_eq!(
            e,
            ck(
                "databricks",
                &format!("HTTPS://{}/sql/1.0", host.to_uppercase()),
                0,
                "main"
            )
        );
        let aws = ek("Databricks", "dbc-a1b2c3d4-e5f6.cloud.databricks.com", "");
        assert_eq!(aws.family, "databricks");
        assert_eq!(aws.database, "");
    }

    #[test]
    fn snowflake_accounts_and_urls() {
        let e = ek(
            "Snowflake",
            "xy12345.us-east-1.snowflakecomputing.com",
            "ANALYTICS",
        );
        assert_eq!(e.host, "xy12345.us-east-1.snowflakecomputing.com");
        let mut c = cfg("snowflake", "", 0, "analytics");
        c.options
            .insert("account".into(), "xy12345.us-east-1".into());
        assert_eq!(key_of_config(&c, &machine()).unwrap(), e);
        c.options.insert(
            "account".into(),
            "https://XY12345.us-east-1.snowflakecomputing.com/".into(),
        );
        assert_eq!(key_of_config(&c, &machine()).unwrap(), e);
        // The org-account form.
        let mut o = cfg("snowflake", "", 0, "");
        o.options.insert("account".into(), "myorg-acct".into());
        assert_eq!(
            key_of_config(&o, &machine()).unwrap().host,
            "myorg-acct.snowflakecomputing.com"
        );
        assert!(key_of_config(&cfg("snowflake", "", 0, "x"), &machine()).is_none());
    }

    #[test]
    fn oracle_host_port_service() {
        let e = ek("Oracle", "DB.Corp.com:1521/FREEPDB1", "");
        assert_eq!(
            (e.host.as_str(), e.port, e.database.as_str()),
            ("db.corp.com", 1521, "freepdb1")
        );
        let mut c = cfg("oracle", "db.corp.com", 0, "");
        c.options.insert("service".into(), "FREEPDB1".into());
        assert_eq!(key_of_config(&c, &machine()).unwrap(), e);
        // No port: the default; a different one differs.
        assert_eq!(ek("Oracle", "db.corp.com/FREEPDB1", "").port, 1521);
        assert!(!ek("Oracle", "db.corp.com:1600/FREEPDB1", "").same_server(&e));
        // A descriptor is not a host.
        assert!(key_of_external(
            &ext("Oracle", "(DESCRIPTION=(ADDRESS=...))", None),
            &machine()
        )
        .is_none());
        c.options
            .insert("connect_descriptor".into(), "(DESCRIPTION=)".into());
        assert!(key_of_config(&c, &machine()).is_none());
        // Autonomous DB has no host in its config.
        assert!(key_of_config(&cfg("oracle_adb", "", 0, ""), &machine()).is_none());
    }

    #[test]
    fn postgres_and_mysql_families() {
        let p = ek("PostgreSQL", "PG.Corp.com", "App");
        assert_eq!((p.port, p.family.as_str()), (5432, "postgres"));
        assert_eq!(p, ck("postgres", "pg.corp.com", 5432, "app"));
        assert_eq!(p, ck("postgres", "pg.corp.com", 0, "app"));
        assert_eq!(ek("PostgreSQL", "pg:5433", "app").port, 5433);
        // Other engines that speak PostgreSQL join the family, with their
        // own default port.
        assert_eq!(ck("cockroachdb", "h", 0, "d").family, "postgres");
        assert_eq!(ck("cockroachdb", "h", 0, "d").port, 26257);
        assert_eq!(ck("redshift", "h", 0, "d").port, 5439);
        let m = ek("MySQL", "my.corp.com", "shop");
        assert_eq!((m.port, m.family.as_str()), (3306, "mysql"));
        assert_eq!(m, ck("mariadb", "MY.corp.com", 3306, "SHOP"));
        assert_eq!(ck("tidb", "h", 0, "d").port, 4000);
        // Not the same family.
        assert!(!p.same_server(&ek("MySQL", "pg.corp.com", "app")));
        // IPv6.
        assert_eq!(
            ek("PostgreSQL", "[2001:db8::1]:5433", "d").host,
            "2001:db8::1"
        );
        assert_eq!(ek("PostgreSQL", "[2001:db8::1]:5433", "d").port, 5433);
        assert_eq!(ek("PostgreSQL", "2001:db8::1", "d").port, 5432);
    }

    #[test]
    fn what_is_not_a_server_has_no_key() {
        for (s, srv) in [
            ("Excel", "C:\\data\\a.xlsx"),
            ("CSV", "https://x.com/a.csv"),
            ("SharePoint", "https://x.sharepoint.com"),
            ("Web", "https://x.com"),
            ("ODBC", "dsn=Ventas;uid=ana"),
            ("Folder", "/data"),
        ] {
            assert!(
                key_of_external(&ext(s, srv, None), &machine()).is_none(),
                "{s}"
            );
        }
        assert!(key_of_external(&ExternalRef::default(), &machine()).is_none());
        assert!(key_of_external(&ext("SQL Server", "  ", None), &machine()).is_none());
        // A file engine's config, and a managed (hostless) one.
        assert!(key_of_config(&cfg("sqlite", "/tmp/a.db", 0, ""), &machine()).is_none());
        assert!(key_of_config(&cfg("sqlserver", "", 0, "d"), &machine()).is_none());
        assert!(key_of_config(&cfg("postgres", "  ", 0, ""), &machine()).is_none());
    }

    #[test]
    fn quotes_brackets_and_case_are_stripped() {
        let a = ek("SQL Server", "\"SRV\"", "[DW]");
        assert_eq!((a.host.as_str(), a.database.as_str()), ("srv", "dw"));
        assert_eq!(a, ck("sqlserver", "[Srv]", 0, "'dw'"));
    }

    #[test]
    fn signature_is_stable_and_tells_keys_apart() {
        let a = ek("SQL Server", "srv", "dw");
        assert_eq!(a.signature(), "sqlserver|srv|1433||dw");
        assert_ne!(a.signature(), ek("SQL Server", "srv", "dw2").signature());
        assert_eq!(
            a.signature(),
            ck("sqlserver", "SRV", 1433, "DW").signature()
        );
    }

    #[test]
    fn unknown_engines_match_by_their_own_id() {
        let e = ek("Teradata", "td.corp.com", "sales");
        assert_eq!(e.family, "teradata");
        assert_eq!(e, ck("teradata", "td.corp.com", 0, "sales"));
        assert_eq!(e.port, 0);
        // An unknown default port matches whatever the other side says.
        assert!(e.same_server(&ck("teradata", "td.corp.com", 1025, "sales")));
    }

    #[test]
    fn matching_none_one_and_many() {
        let conns = vec![
            saved("pg", cfg("postgres", "pg.corp.com", 0, "app")),
            saved("sql-a", cfg("sqlserver", "SRV", 0, "dw")),
            saved("sql-b", cfg("azuresql", "srv", 1433, "dw")),
            saved("sql-other", cfg("sqlserver", "srv", 0, "other")),
            saved("lite", cfg("sqlite", "/tmp/x.db", 0, "")),
            saved("managed", cfg("sqlserver", "", 0, "dw")),
        ];
        let m = |k: &SourceKey| match_with(k, &conns, &machine());
        assert_eq!(
            m(&ek("PostgreSQL", "pg.corp.com", "app")),
            Match::One("pg".into())
        );
        assert_eq!(m(&ek("PostgreSQL", "pg.corp.com", "nope")), Match::None);
        assert_eq!(m(&ek("MySQL", "pg.corp.com", "app")), Match::None);
        assert_eq!(
            m(&ek("SQL Server", "srv", "dw")),
            Match::Many(vec!["sql-a".into(), "sql-b".into()])
        );
        assert_eq!(
            m(&ek("SQL Server", "srv", "other")),
            Match::One("sql-other".into())
        );
        assert_eq!(m(&ek("SQL Server", "srv,9999", "dw")), Match::None);
        assert_eq!(m(&ek("SQL Server", "elsewhere", "dw")), Match::None);
    }

    #[test]
    fn a_connection_with_no_database_is_a_weaker_match() {
        let conns = vec![
            saved("any", cfg("postgres", "pg", 0, "")),
            saved("named", cfg("postgres", "pg", 0, "app")),
        ];
        // The one naming the database wins.
        assert_eq!(
            match_with(&ek("PostgreSQL", "pg", "app"), &conns, &machine()),
            Match::One("named".into())
        );
        // None names it: the one with no database may hold it.
        assert_eq!(
            match_with(&ek("PostgreSQL", "pg", "reports"), &conns, &machine()),
            Match::One("any".into())
        );
        // A source with no database matches the connections with none first.
        assert_eq!(
            match_with(&ek("PostgreSQL", "pg", ""), &conns, &machine()),
            Match::One("any".into())
        );
    }

    #[test]
    fn local_aliases_match_across_sides() {
        let conns = vec![saved("l", cfg("sqlserver", "localhost", 0, "dw"))];
        for s in ["(local)", ".", "127.0.0.1", "DEVBOX", "devbox"] {
            assert_eq!(
                match_with(&ek("SQL Server", s, "dw"), &conns, &machine()),
                Match::One("l".into()),
                "{s}"
            );
        }
    }

    #[test]
    fn this_machines_real_name_is_known() {
        // Whatever the environment calls this machine is `localhost`.
        for name in machine_names() {
            assert_eq!(normalize_host(name, machine_names()), "localhost");
        }
        let _ = none();
    }

    #[test]
    fn new_connection_from_a_sql_server_source() {
        let c =
            new_connection_from_source(&ext("SQL Server", r"Srv.Corp.com\Inst,1500", Some("DW")))
                .unwrap();
        assert_eq!(c.driver, "sqlserver");
        assert_eq!(c.host, r"Srv.Corp.com\Inst");
        assert_eq!((c.port, c.database.as_str()), (1500, "DW"));
        assert!(c.username.is_none() && c.password.is_none());
        let a = new_connection_from_source(&ext(
            "SQL Server",
            "Acme.database.windows.net",
            Some("sales"),
        ))
        .unwrap();
        assert_eq!(a.driver, "azuresql");
        assert_eq!(a.host, "Acme.database.windows.net");
        assert_eq!(a.port, 0);
        let f = new_connection_from_source(&ext(
            "SQL Server",
            "x.datawarehouse.fabric.microsoft.com",
            Some("wh"),
        ))
        .unwrap();
        assert_eq!(f.driver, "fabric");
    }

    #[test]
    fn new_connection_from_the_other_engines() {
        let p = new_connection_from_source(&ext("PostgreSQL", "PG.corp.com:5433", Some("App")))
            .unwrap();
        assert_eq!(
            (
                p.driver.as_str(),
                p.host.as_str(),
                p.port,
                p.database.as_str()
            ),
            ("postgres", "PG.corp.com", 5433, "App")
        );
        let m = new_connection_from_source(&ext("MySQL", "my.corp.com", Some("shop"))).unwrap();
        assert_eq!((m.driver.as_str(), m.port), ("mysql", 0));
        let o =
            new_connection_from_source(&ext("Oracle", "db.corp.com:1521/FREEPDB1", None)).unwrap();
        assert_eq!(o.driver, "oracle");
        assert_eq!((o.host.as_str(), o.port), ("db.corp.com", 1521));
        assert_eq!(
            o.options.get("service").map(String::as_str),
            Some("FREEPDB1")
        );
        let mut d = ext("Databricks", "adb-1.7.azuredatabricks.net", Some("main"));
        d.detail = Some("/sql/1.0/warehouses/abc · ventas.a".into());
        let d = new_connection_from_source(&d).unwrap();
        assert_eq!(d.driver, "azure_databricks");
        assert_eq!(d.host, "adb-1.7.azuredatabricks.net");
        assert_eq!(
            d.options.get("warehouse").map(String::as_str),
            Some("/sql/1.0/warehouses/abc")
        );
        let s = new_connection_from_source(&ext(
            "Snowflake",
            "xy12345.us-east-1.snowflakecomputing.com",
            Some("ANALYTICS"),
        ))
        .unwrap();
        assert_eq!(s.driver, "snowflake");
        assert_eq!(
            s.options.get("account").map(String::as_str),
            Some("xy12345.us-east-1.snowflakecomputing.com")
        );
        assert!(new_connection_from_source(&ext("Excel", "a.xlsx", None)).is_none());
        // What it prefills is what matches back.
        let back = key_of_config(&p, &machine()).unwrap();
        assert_eq!(
            back,
            key_of_external(
                &ext("PostgreSQL", "PG.corp.com:5433", Some("App")),
                &machine()
            )
            .unwrap()
        );
    }

    #[test]
    fn links_round_trip_and_forget_deleted_connections() {
        let s = StateStore::open_in_memory().unwrap();
        let sig = ek("SQL Server", "srv", "dw").signature();
        assert_eq!(s.lineage_link_get(&sig).unwrap(), None);
        // Only a saved connection can be linked.
        assert!(s.lineage_link_set(&sig, "ghost").is_err());
        s.save_connection(&saved("a", cfg("sqlserver", "srv", 0, "dw")))
            .unwrap();
        s.save_connection(&saved("b", cfg("azuresql", "srv", 0, "dw")))
            .unwrap();
        s.lineage_link_set(&sig, "a").unwrap();
        assert_eq!(s.lineage_link_get(&sig).unwrap(), Some("a".into()));
        // Relinking replaces.
        s.lineage_link_set(&sig, "b").unwrap();
        assert_eq!(s.lineage_link_get(&sig).unwrap(), Some("b".into()));
        // Another source is not affected.
        assert_eq!(s.lineage_link_get("other").unwrap(), None);
        // A deleted connection's link is ignored and dropped.
        s.delete_connection("b").unwrap();
        assert_eq!(s.lineage_link_get(&sig).unwrap(), None);
        let n: i64 = s
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM lineage_links", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
        // Remove, twice.
        s.lineage_link_set(&sig, "a").unwrap();
        s.lineage_link_remove(&sig).unwrap();
        s.lineage_link_remove(&sig).unwrap();
        assert_eq!(s.lineage_link_get(&sig).unwrap(), None);
    }

    #[test]
    fn a_link_survives_reopening_the_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let s = StateStore::open(&path).unwrap();
            s.save_connection(&saved("a", cfg("postgres", "pg", 0, "app")))
                .unwrap();
            s.lineage_link_set("sig", "a").unwrap();
        }
        let s = StateStore::open(&path).unwrap();
        assert_eq!(s.lineage_link_get("sig").unwrap(), Some("a".into()));
    }

    #[test]
    fn the_user_link_beats_the_automatic_match() {
        let s = StateStore::open_in_memory().unwrap();
        let a = saved("a", cfg("sqlserver", "SRV", 0, "dw"));
        let b = saved("b", cfg("azuresql", "srv", 1433, "dw"));
        s.save_connection(&a).unwrap();
        s.save_connection(&b).unwrap();
        let conns = vec![a, b];
        let e = ext("SQL Server", "srv", Some("dw"));
        assert_eq!(
            s.lineage_resolve_source(&e, &conns).unwrap(),
            Match::Many(vec!["a".into(), "b".into()])
        );
        let key = source_key_of_external(&e).unwrap();
        s.lineage_link_set(&key.signature(), "b").unwrap();
        assert_eq!(
            s.lineage_resolve_source(&e, &conns).unwrap(),
            Match::One("b".into())
        );
        // A source with no key resolves to nothing.
        assert_eq!(
            s.lineage_resolve_source(&ext("Excel", "a.xlsx", None), &conns)
                .unwrap(),
            Match::None
        );
    }

    #[test]
    fn a_state_file_from_before_the_table_gains_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        StateStore::open(&path).unwrap();
        {
            let c = rusqlite::Connection::open(&path).unwrap();
            c.execute_batch("DROP TABLE lineage_links").unwrap();
        }
        let s = StateStore::open(&path).unwrap();
        assert_eq!(s.lineage_link_get("x").unwrap(), None);
    }

    /// A connection made from a source only reads, whatever the engine: nothing asked for a write.
    #[test]
    fn a_connection_from_a_source_is_read_only() {
        for (system, server, db) in [
            ("SQL Server", "srv.corp.com", Some("DW")),
            ("PostgreSQL", "pg.corp.com:5433", Some("App")),
            ("MySQL", "my.corp.com", Some("shop")),
            ("Oracle", "ora.corp.com:1521/ORCL", None),
            ("Databricks", "adb-1.azuredatabricks.net", None),
        ] {
            let c = new_connection_from_source(&ext(system, server, db)).unwrap();
            assert!(c.read_only, "{system}");
            assert!(c.username.is_none() && c.password.is_none(), "{system}");
        }
    }
}
