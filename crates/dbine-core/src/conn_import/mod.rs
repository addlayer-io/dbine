//! Connections of other database tools, read from their own files so they
//! can be brought into DBine:
//!
//! - DBeaver: `data-sources.json` and its encrypted `credentials-config.json`;
//! - DbGate: `connections.jsonl`, passwords encrypted with the key in `.key`;
//! - DataGrip and the other JetBrains IDEs: `dataSources.xml` (global and per
//!   project), passwords in the system keychain;
//! - Azure Data Studio: `settings.json` (`datasource.connections`);
//! - SSMS: registered servers (`RegSrvr.xml`);
//! - connection URLs / connection strings pasted by the user.
//!
//! Reading never writes anything; the caller decides what to save. Secrets
//! come back inside each candidate's config (password, secret options), or
//! as a keychain entry of the other tool read only on import, and must go to
//! DBine's keychain, never to the UI or the state file.

mod ads;
mod dbeaver;
mod dbgate;
mod jetbrains;
mod ssms;
mod urls;

use dbine_driver::{ConnectionConfig, Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Dbeaver,
    Dbgate,
    Datagrip,
    AzureDataStudio,
    Ssms,
    /// Pasted URLs / connection strings, one per line.
    Url,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Dbeaver => "DBeaver",
            Source::Dbgate => "DbGate",
            Source::Datagrip => "DataGrip / JetBrains",
            Source::AzureDataStudio => "Azure Data Studio",
            Source::Ssms => "SQL Server Management Studio",
            Source::Url => "URL de conexión",
        }
    }
}

/// A password kept in the system keychain by the other tool.
#[derive(Clone, Debug)]
pub struct KeychainSecret {
    pub service: String,
    pub account: String,
}

impl KeychainSecret {
    /// Read it (the system may ask the user to allow it).
    pub fn fetch(&self) -> Option<String> {
        keyring::Entry::new(&self.service, &self.account).ok()?.get_password().ok().filter(|p| !p.is_empty())
    }
}

/// One connection found in the other tool.
#[derive(Clone)]
pub struct Candidate {
    /// Its id in the source (unique within one read).
    pub key: String,
    pub name: String,
    /// Folder path, outermost first.
    pub folder: Vec<String>,
    pub color: Option<String>,
    /// Tags for the connection (DBeaver's connection type: prod, qa, dev).
    pub tags: Vec<String>,
    /// What the source calls the engine (shown when DBine has no equivalent).
    pub source_kind: String,
    /// The DBine config, secrets included. `driver` is empty when
    /// `unsupported` is set.
    pub config: ConnectionConfig,
    /// The password, when it's in the other tool's keychain entry.
    pub keychain: Option<KeychainSecret>,
    /// Settings that don't come along (SSH tunnel, Windows login…).
    pub notes: Vec<String>,
    /// Why it can't be imported.
    pub unsupported: Option<String>,
}

impl Candidate {
    fn new(key: String, name: String, source_kind: String) -> Self {
        Candidate {
            key,
            name,
            folder: Vec::new(),
            color: None,
            tags: Vec::new(),
            source_kind,
            config: ConnectionConfig {
                driver: String::new(),
                host: String::new(),
                port: 0,
                database: String::new(),
                username: None,
                password: None,
                encrypt: false,
                trust_server_certificate: false,
                read_only: false,
                options: Default::default(),
            },
            keychain: None,
            notes: Vec::new(),
            unsupported: None,
        }
    }

    /// Whether it carries a password or another secret.
    pub fn has_secret(&self) -> bool {
        self.config.password.as_deref().is_some_and(|p| !p.is_empty())
            || SECRET_OPTIONS.iter().any(|k| self.config.options.get(*k).is_some_and(|v| !v.is_empty()))
    }

    /// Set the DBine driver for what the source calls the engine, or mark it
    /// unsupported.
    fn set_driver(&mut self, probe: &str) -> bool {
        match driver_for(probe) {
            Ok(d) => {
                self.config.driver = d.to_string();
                true
            }
            Err(why) => {
                self.unsupported = Some(why);
                false
            }
        }
    }
}

/// Option keys this module may fill with a secret.
const SECRET_OPTIONS: &[&str] = &["connection_string", "auth_token", "token", "secret_access_key"];

pub struct Found {
    /// The file read (or the folder, for several projects).
    pub path: PathBuf,
    pub candidates: Vec<Candidate>,
    /// Problems that don't stop the import (passwords that couldn't be read…).
    pub warnings: Vec<String>,
}

/// Where the tool keeps its connections on this machine, if it's there.
pub fn default_location(source: Source) -> Option<PathBuf> {
    match source {
        Source::Dbgate => dbgate::default_location(),
        Source::Dbeaver => dbeaver::default_location(),
        Source::Datagrip => jetbrains::default_location(),
        Source::AzureDataStudio => ads::default_location(),
        Source::Ssms => ssms::default_location(),
        Source::Url => None,
    }
}

/// Read the connections from `path` (a file or the tool's folder).
pub fn read(source: Source, path: &Path) -> Result<Found> {
    if !path.exists() {
        return Err(Error::Query(format!("no existe {}", path.display())));
    }
    match source {
        Source::Dbgate => dbgate::read(path),
        Source::Dbeaver => dbeaver::read(path),
        Source::Datagrip => jetbrains::read(path),
        Source::AzureDataStudio => ads::read(path),
        Source::Ssms => ssms::read(path),
        Source::Url => Err(Error::Query("las URLs se leen con `read_text`".into())),
    }
}

/// Pasted connection URLs or connection strings, one per line.
pub fn read_text(text: &str) -> Found {
    urls::read(text)
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

/// The per-user application data folder (`~/Library/Application Support`,
/// `%APPDATA%`, `~/.config`).
fn app_data() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        Some(home()?.join("Library/Application Support"))
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        Some(std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or(home()?.join(".config")))
    }
}

/// DBine's driver for what another tool calls the engine: a provider,
/// driver id or JDBC URL, lowercased.
const DRIVERS: &[(&str, &str)] = &[
    ("redshift", "redshift"),
    ("cockroach", "cockroachdb"),
    ("timescale", "timescaledb"),
    ("greenplum", "greenplum"),
    ("yugabyte", "yugabytedb"),
    ("babelfish", "babelfish"),
    ("materialize", "materialize"),
    ("postgres", "postgres"),
    ("pgsql", "postgres"),
    ("mariadb", "mariadb"),
    ("tidb", "tidb"),
    ("singlestore", "singlestore"),
    ("memsql", "singlestore"),
    ("starrocks", "starrocks"),
    ("doris", "doris"),
    ("oceanbase", "oceanbase"),
    ("mysql", "mysql"),
    ("sybase", "sybase"),
    ("jtds", "sqlserver"),
    ("sqlserver", "sqlserver"),
    ("mssql", "sqlserver"),
    ("azure.ms", "azuresql"),
    ("microsoft", "sqlserver"),
    ("oracle", "oracle"),
    ("sqlite", "sqlite"),
    ("duckdb", "duckdb"),
    ("db2", "db2"),
    ("clickhouse", "clickhouse"),
    ("snowflake", "snowflake"),
    ("vertica", "vertica"),
    ("teradata", "teradata"),
    ("exasol", "exasol"),
    ("jaybird", "firebird"),
    ("firebird", "firebird"),
    ("hana", "hana"),
    ("informix", "informix"),
    ("netezza", "netezza"),
    ("trino", "trino"),
    ("presto", "presto"),
    ("scylla", "scylladb"),
    ("cassandra", "cassandra"),
    ("mongo", "mongodb"),
    ("redis", "redis"),
    ("elasticsearch", "elasticsearch"),
    ("opensearch", "opensearch"),
    ("neo4j", "neo4j"),
    ("dremio", "dremio"),
    ("drill", "drill"),
    ("h2", "h2"),
    ("hive", "hive"),
    ("impala", "impala"),
    ("couchbase", "couchbase"),
    ("monetdb", "monetdb"),
    ("mimer", "mimer"),
    ("cubrid", "cubrid"),
];

/// Engines whose login other tools keep in ways that don't carry over.
const MANUAL: &[(&str, &str)] = &[
    ("bigquery", "BigQuery"),
    ("spanner", "Spanner"),
    ("athena", "Athena"),
    ("databricks", "Databricks"),
    ("dynamodb", "DynamoDB"),
    ("cosmos", "Cosmos DB"),
    ("kusto", "Azure Data Explorer"),
    ("loganalytics", "Log Analytics"),
];

/// The DBine driver for `probe` (provider / driver id / URL), or why not.
fn driver_for(probe: &str) -> std::result::Result<&'static str, String> {
    let p = probe.to_ascii_lowercase();
    if let Some((_, label)) = MANUAL.iter().find(|(k, _)| p.contains(k)) {
        return Err(format!("{label} se configura con credenciales de la nube: creala a mano en DBine"));
    }
    DRIVERS
        .iter()
        .find(|(k, _)| p.contains(k))
        .map(|(_, d)| *d)
        .ok_or_else(|| format!("DBine no tiene un driver para «{}»", probe.split_whitespace().next().unwrap_or(probe)))
}

/// A connection URL (`scheme://user:pass@host:port/db?x=y`), split.
#[derive(Debug, Default, PartialEq)]
struct Url {
    scheme: String,
    user: Option<String>,
    password: Option<String>,
    host: String,
    port: u16,
    path: String,
    /// Query parameters, keys lowercased.
    params: Vec<(String, String)>,
}

fn parse_url(url: &str) -> Option<Url> {
    let (scheme, rest) = url.split_once("://")?;
    let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
    let rest = rest.split('#').next().unwrap_or("");
    let (auth, hostpath) = match rest.rsplit_once('@') {
        Some((a, h)) => (Some(a), h),
        None => (None, rest),
    };
    let (hostport, path) = hostpath.split_once('/').unwrap_or((hostpath, ""));
    // Several hosts (`a:1,b:2`): the first.
    let hostport = hostport.split(',').next().unwrap_or("");
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || hostport.starts_with('[') => (h, p.parse().unwrap_or(0)),
        _ => (hostport, 0),
    };
    let (user, password) = match auth {
        Some(a) => match a.split_once(':') {
            Some((u, p)) => (Some(pct_decode(u)), Some(pct_decode(p))),
            None => (Some(pct_decode(a)), None),
        },
        None => (None, None),
    };
    let params = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.to_ascii_lowercase(), pct_decode(v)))
        .collect();
    Some(Url {
        scheme: scheme.to_ascii_lowercase(),
        user: user.filter(|u| !u.is_empty()),
        password: password.filter(|p| !p.is_empty()),
        host: host.trim_matches(['[', ']']).to_string(),
        port,
        path: pct_decode(path.trim_end_matches('/')),
        params,
    })
}

/// `url` without its login (`user:pass@`), query string and fragment: what
/// can be shown or kept as a name or a host without carrying a secret.
pub(crate) fn redact_url(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (Some(s), r),
        None => (None, url),
    };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let rest = rest.rsplit_once('@').map(|(_, r)| r).unwrap_or(rest);
    match scheme {
        Some(s) => format!("{s}://{rest}"),
        None => rest.to_string(),
    }
}

/// A libSQL / Turso URL split into the base the driver connects to and the
/// auth token some tools append (`?authToken=…`). Other query parameters
/// are dropped: the driver ignores them.
pub(crate) fn libsql_url(url: &str) -> (String, Option<String>) {
    let url = url.trim();
    let token = parse_url(url)
        .and_then(|u| u.params.into_iter().find(|(k, _)| k == "authtoken" || k == "auth_token").map(|(_, v)| v))
        .filter(|t| !t.is_empty());
    let base = url.split(['?', '#']).next().unwrap_or("").to_string();
    (base, token)
}

/// `HOST` and `SERVICE_NAME` (or `SID`) of an Oracle TNS descriptor, for a
/// connection's name.
pub(crate) fn descriptor_name(descriptor: &str) -> Option<String> {
    let upper = descriptor.to_ascii_uppercase();
    let value = |key: &str| {
        let at = upper.find(&format!("({key}="))? + key.len() + 2;
        let end = descriptor[at..].find(')')? + at;
        Some(descriptor[at..end].trim().to_string()).filter(|v| !v.is_empty())
    };
    let host = value("HOST")?;
    Some(match value("SERVICE_NAME").or_else(|| value("SID")) {
        Some(service) => format!("{host} / {service}"),
        None => host,
    })
}

/// What follows the `@` of an Oracle JDBC/EZConnect URL, without the
/// `user/password@` some carry before it.
fn oracle_target(url: &str) -> String {
    let rest = url.trim().strip_prefix("jdbc:oracle:thin:").unwrap_or(url.trim());
    rest.split_once('@').map(|(_, t)| t).unwrap_or(rest).to_string()
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        // Decode the two hex digits from the bytes: slicing the &str at byte
        // offsets would panic when a multi-byte character follows the `%`.
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(hi), Some(lo)) = (hex_val(b[i + 1]), hex_val(b[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

fn truthy(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "yes" | "1" | "require" | "required" | "verify-ca" | "verify-full" | "mandatory" | "strict" | "sspi")
}

/// Fill the host, port, database, login and TLS of `c` (its driver already
/// set) from a JDBC URL (`jdbc:postgresql://…`, `jdbc:sqlserver://h;k=v`,
/// `jdbc:oracle:thin:@…`, `jdbc:sqlite:/path`). Returns false when it
/// couldn't be read.
fn apply_jdbc(c: &mut Candidate, url: &str) -> bool {
    let rest = url.trim().strip_prefix("jdbc:").unwrap_or(url.trim());
    let cfg = &mut c.config;
    match cfg.driver.as_str() {
        "sqlite" | "duckdb" => {
            let file = rest.split_once(':').map(|(_, f)| f).unwrap_or("");
            cfg.host = file.trim_start_matches("//").to_string();
            return !cfg.host.is_empty();
        }
        "oracle" => {
            let Some((login, target)) = rest.split_once('@') else { return false };
            // `user/password@…`: the login goes where secrets are kept, never
            // into the target (or a name made from it).
            let login = login.strip_prefix("oracle:thin:").unwrap_or(login);
            if let Some((user, password)) = login.split_once('/') {
                if cfg.username.is_none() && !user.is_empty() {
                    cfg.username = Some(user.to_string());
                }
                if cfg.password.is_none() && !password.is_empty() {
                    cfg.password = Some(password.to_string());
                }
            }
            let target = target.trim();
            if target.starts_with('(') {
                cfg.options.insert("connect_descriptor".into(), target.to_string());
                return true;
            }
            let t = target.trim_start_matches("//");
            // host:port/service, host:port:SID, host/service.
            let (hostport, service, sid) = match t.split_once('/') {
                Some((hp, s)) => (hp, s, false),
                None => match t.rsplitn(3, ':').collect::<Vec<_>>().as_slice() {
                    [sid, port, host] => {
                        cfg.host = host.to_string();
                        cfg.port = port.parse().unwrap_or(0);
                        cfg.options.insert("service".into(), sid.to_string());
                        cfg.options.insert("connect_by".into(), "sid".into());
                        return true;
                    }
                    _ => (t, "", false),
                },
            };
            let (h, p) = hostport.split_once(':').unwrap_or((hostport, ""));
            cfg.host = h.to_string();
            cfg.port = p.parse().unwrap_or(0);
            if !service.is_empty() {
                cfg.options.insert("service".into(), service.to_string());
                cfg.options.insert("connect_by".into(), if sid { "sid" } else { "service_name" }.into());
            }
            return !cfg.host.is_empty();
        }
        _ => {}
    }
    // `sqlserver://h[:p];k=v;…` and the usual `scheme://…?k=v`.
    let (main, props) = rest.split_once(';').unwrap_or((rest, ""));
    let Some(u) = parse_url(main) else { return false };
    cfg.host = u.host.clone();
    if u.port != 0 {
        cfg.port = u.port;
    }
    cfg.database = u.path.clone();
    if u.user.is_some() {
        cfg.username = u.user.clone();
    }
    if u.password.is_some() {
        cfg.password = u.password.clone();
    }
    let mut params = u.params;
    params.extend(props.split(';').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())));
    for (k, v) in &params {
        match k.as_str() {
            "databasename" | "database" => cfg.database = v.clone(),
            "user" | "username" if cfg.username.is_none() => cfg.username = Some(v.clone()),
            "password" if cfg.password.is_none() => cfg.password = Some(v.clone()),
            "encrypt" | "ssl" | "sslmode" | "usessl" => cfg.encrypt = truthy(v),
            "trustservercertificate" => cfg.trust_server_certificate = truthy(v),
            "integratedsecurity" if truthy(v) => c.notes.push(WINDOWS_AUTH.into()),
            _ => {}
        }
    }
    true
}

const WINDOWS_AUTH: &str = "La autenticación de Windows no está en DBine: se importa con usuario y contraseña.";

/// An ADO.NET / ODBC connection string (`Server=h,1433;Database=d;User Id=u;…`)
/// for SQL Server, applied to `c`.
fn apply_ado(c: &mut Candidate, s: &str) {
    let cfg = &mut c.config;
    for part in split_ado(s) {
        let Some((k, v)) = part.split_once('=') else { continue };
        let v = v.trim();
        // `len() >= 2`: a lone `"` both starts and ends with a quote.
        let v = if v.len() >= 2 && (v.starts_with('{') && v.ends_with('}')) || (v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')) {
            v[1..v.len() - 1].to_string()
        } else {
            v.to_string()
        };
        match k.trim().to_ascii_lowercase().replace(' ', "").as_str() {
            "server" | "datasource" | "address" | "addr" | "networkaddress" => {
                let h = v.strip_prefix("tcp:").unwrap_or(&v);
                cfg.host = h.to_string();
            }
            "initialcatalog" | "database" => cfg.database = v,
            "userid" | "uid" | "user" | "username" => cfg.username = Some(v).filter(|x| !x.is_empty()),
            "password" | "pwd" => cfg.password = Some(v).filter(|x| !x.is_empty()),
            "encrypt" => cfg.encrypt = truthy(&v),
            "trustservercertificate" => cfg.trust_server_certificate = truthy(&v),
            "integratedsecurity" | "trusted_connection" if truthy(&v) => c.notes.push(WINDOWS_AUTH.into()),
            "authentication" => {
                let a = v.to_ascii_lowercase();
                let auth = if a.contains("service principal") {
                    Some("entra_sp")
                } else if a.contains("password") {
                    Some("entra_password")
                } else if a.contains("interactive") || a.contains("integrated") || a.contains("mfa") || a.contains("default") {
                    c.notes.push("Ese método de Microsoft Entra ID no está en DBine: elegí otro al conectar.".into());
                    Some("entra_password")
                } else {
                    None
                };
                if let Some(a) = auth {
                    cfg.options.insert("auth".into(), a.into());
                }
            }
            _ => {}
        }
    }
}

/// `k=v;k=v`, keeping `;` inside `{…}` or quotes.
fn split_ado(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut close: Option<char> = None;
    for ch in s.chars() {
        match close {
            Some(c) if ch == c => {
                close = None;
                cur.push(ch);
            }
            Some(_) => cur.push(ch),
            None if ch == ';' => out.push(std::mem::take(&mut cur)),
            None => {
                // Only right after `=` does a brace or quote open a value.
                if cur.trim_end().ends_with('=') {
                    close = match ch {
                        '{' => Some('}'),
                        '"' | '\'' => Some(ch),
                        _ => None,
                    };
                }
                cur.push(ch);
            }
        }
    }
    out.push(cur);
    out
}

/// A color name of the other tool, as one of DBine's connection colors.
fn color_of(name: &str) -> Option<String> {
    let c = match name.trim().to_ascii_lowercase().as_str() {
        "red" | "prod" | "production" => "#f14c4c",
        "orange" | "brown" => "#ce9178",
        "yellow" | "gold" | "test" => "#cca700",
        "green" | "lime" | "olive" => "#89d185",
        "blue" | "navy" | "sky" => "#3794ff",
        "cyan" | "teal" | "aqua" => "#4ec9b0",
        "purple" | "magenta" | "violet" | "pink" => "#c586c0",
        _ => return None,
    };
    Some(c.to_string())
}

fn port_of(s: &str) -> u16 {
    s.trim().parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let u = parse_url("postgresql://ana%40x:p%3Ass@db.local:6543/ventas?sslmode=require").unwrap();
        assert_eq!((u.user.as_deref(), u.password.as_deref(), u.host.as_str(), u.port, u.path.as_str()), (Some("ana@x"), Some("p:ss"), "db.local", 6543, "ventas"));
        assert_eq!(u.params, vec![("sslmode".to_string(), "require".to_string())]);
        let u = parse_url("mongodb+srv://c.example.net/").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.user), ("c.example.net", 0, None));
        let u = parse_url("redis://:secreto@127.0.0.1:6380/2").unwrap();
        assert_eq!((u.password.as_deref(), u.port, u.path.as_str()), (Some("secreto"), 6380, "2"));
        assert!(parse_url("host:1521/XE").is_none());
    }

    #[test]
    fn urls_lose_their_secrets() {
        assert_eq!(redact_url("postgres://ana:pw@pg:5433/app?sslmode=require"), "postgres://pg:5433/app");
        assert_eq!(redact_url("libsql://app-org.turso.io?authToken=eyJ.x.y"), "libsql://app-org.turso.io");
        assert_eq!(redact_url("https://u:p@h/x#frag"), "https://h/x");
        assert_eq!(redact_url("h:1521/XE"), "h:1521/XE");
        assert_eq!(libsql_url("libsql://app-org.turso.io?authToken=eyJ%2Ex&tls=1"), ("libsql://app-org.turso.io".to_string(), Some("eyJ.x".to_string())));
        assert_eq!(libsql_url("http://localhost:8080?auth_token=t"), ("http://localhost:8080".to_string(), Some("t".to_string())));
        assert_eq!(libsql_url("libsql://db.turso.io"), ("libsql://db.turso.io".to_string(), None));
        assert_eq!(oracle_target("jdbc:oracle:thin:scott/tiger@//ora:1521/XE"), "//ora:1521/XE");
        assert_eq!(oracle_target("jdbc:oracle:thin:@(DESCRIPTION=(ADDRESS=(HOST=h)))"), "(DESCRIPTION=(ADDRESS=(HOST=h)))");
        assert_eq!(oracle_target("ora:1521/XE"), "ora:1521/XE");
    }

    #[test]
    fn multibyte_input_does_not_panic() {
        // A `%` followed by a multi-byte character used to slice the &str
        // inside that character.
        assert_eq!(pct_decode("%a€"), "%a€");
        assert_eq!(pct_decode("%€"), "%€");
        assert_eq!(pct_decode("x%€y%4"), "x%€y%4");
        assert_eq!(pct_decode("caf%C3%A9 %41ñ"), "café Añ");
        assert!(parse_url("postgres://%€:%a€@h/%€db").is_some());
        let mut c = cand("sqlserver");
        apply_ado(&mut c, "Server=h;Password=\";User ID=€");
        apply_ado(&mut c, "Password=';Initial Catalog={");
        read_text("postgres://ü%€:p%a€@h:5432/d%€");
    }

    fn cand(driver: &str) -> Candidate {
        let mut c = Candidate::new("k".into(), "n".into(), "x".into());
        c.config.driver = driver.into();
        c
    }

    #[test]
    fn jdbc_urls() {
        let mut c = cand("sqlserver");
        assert!(apply_jdbc(&mut c, "jdbc:sqlserver://sql.local:14330;databaseName=ventas;encrypt=true;trustServerCertificate=true"));
        assert_eq!((c.config.host.as_str(), c.config.port, c.config.database.as_str(), c.config.encrypt, c.config.trust_server_certificate), ("sql.local", 14330, "ventas", true, true));
        let mut c = cand("oracle");
        assert!(apply_jdbc(&mut c, "jdbc:oracle:thin:@ora.local:1521:ORCL"));
        assert_eq!((c.config.host.as_str(), c.config.port, c.config.options["service"].as_str(), c.config.options["connect_by"].as_str()), ("ora.local", 1521, "ORCL", "sid"));
        let mut c = cand("oracle");
        assert!(apply_jdbc(&mut c, "jdbc:oracle:thin:@//ora.local:1522/FREEPDB1"));
        assert_eq!((c.config.port, c.config.options["service"].as_str(), c.config.options["connect_by"].as_str()), (1522, "FREEPDB1", "service_name"));
        let mut c = cand("sqlite");
        assert!(apply_jdbc(&mut c, "jdbc:sqlite:/Users/ana/x.db"));
        assert_eq!(c.config.host, "/Users/ana/x.db");
        let mut c = cand("postgres");
        assert!(apply_jdbc(&mut c, "jdbc:postgresql://pg:5433/app?sslmode=require&user=ana"));
        assert_eq!((c.config.host.as_str(), c.config.port, c.config.database.as_str(), c.config.encrypt, c.config.username.as_deref()), ("pg", 5433, "app", true, Some("ana")));
    }

    #[test]
    fn ado_strings() {
        let mut c = cand("sqlserver");
        apply_ado(&mut c, "Server=tcp:srv.database.windows.net,1433;Initial Catalog=ventas;User ID=ana;Password={p;x};Encrypt=True;TrustServerCertificate=False;Authentication=Active Directory Password");
        assert_eq!((c.config.host.as_str(), c.config.database.as_str(), c.config.username.as_deref(), c.config.encrypt), ("srv.database.windows.net,1433", "ventas", Some("ana"), true));
        assert_eq!(c.config.options["auth"], "entra_password");
        assert_eq!(c.config.password.as_deref(), Some("p;x"));
    }
}
