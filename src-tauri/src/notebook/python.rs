//! A notebook exported as a Python (Jupyter) notebook: a new `.ipynb` with
//! kernel `python3` that reproduces it (docs/notebooks.md › Export as a
//! Python notebook).
//!
//! - A setup cell opens one client per connection and database the cells
//!   use: a SQLAlchemy engine where the engine has a dialect, else the
//!   engine's own Python client (pymongo, redis, cassandra-driver…).
//! - Each SQL cell reads its result into a pandas DataFrame named after the
//!   cell; each DataFrame SQL cell runs `duckdb.sql(…).df()` over those
//!   DataFrames (DuckDB's replacement scans of pandas variables).
//! - Engines whose cells can't be mapped reasonably get a commented cell
//!   saying why, with the original text: every engine gets either a working
//!   cell or an explicit commented one.
//!
//! Secrets never go out. A connection reaches this module only as a
//! [`PyConn`], built from the non-secret fields (host, port, database,
//! user, a few named options): passwords, tokens, keys, connection strings,
//! secret-manager references and the SSH tunnel's credentials are never
//! copied into it. The generated code reads each secret from an environment
//! variable named after the connection (`DBINE_VENTAS_PG_PASSWORD`).

use super::{
    local, preview, result_outputs, CellKind, Notebook, NotebookCell, NotebookTarget, PreviewLimits,
};
use crate::connstring::{libpq_sslmode, Tls};
use dbine_driver::{ConnectionConfig, DriverInfo};
use serde_json::{json, Map, Value};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// The connections, without secrets.

/// The options a [`PyConn`] may carry: names of things, never credentials.
const KEPT_OPTIONS: &[&str] = &[
    "account",
    "adb_service",
    "auth_source",
    "aws_profile",
    "aws_region",
    "catalog",
    "cloud_id",
    "cluster",
    "connect_by",
    "connect_descriptor",
    "datacenter",
    "dsn",
    "endpoint_url",
    "http_path",
    "iam",
    "informix_server",
    "instance",
    "location",
    "odbc_driver",
    "org",
    "output_location",
    "profile",
    "project_id",
    "region",
    "replica_set",
    "role",
    "schema",
    "service",
    "wallet_dir",
    "warehouse",
    "workgroup",
];

/// An SSH tunnel DBine opens for a connection: where it goes (no credentials).
#[derive(Debug, Clone, PartialEq)]
pub struct Tunnel {
    pub host: String,
    pub user: String,
    pub port: u16,
}

/// What the export knows of a connection: its non-secret fields only.
#[derive(Debug, Clone, PartialEq)]
pub struct PyConn {
    /// Tells connections apart (connection id and database); never written.
    pub key: String,
    /// The connection's name in DBine.
    pub name: String,
    pub driver: String,
    /// The driver's SQL dialect ("postgres", "mysql"…), "" if unknown.
    pub dialect: String,
    pub default_port: u16,
    /// Without credentials or query string, when it is a URL.
    pub host: String,
    pub port: u16,
    pub database: String,
    pub username: String,
    /// TLS required: the saved bool, or the engine's own TLS options when
    /// the driver has them ([`Tls`]).
    pub encrypt: bool,
    pub tls: Tls,
    /// Only [`KEPT_OPTIONS`], and none that look like a credential.
    pub options: HashMap<String, String>,
    /// The sign-in method (`auth` / `auth_mode`): "aws_iam", "keypair"…
    pub auth: Option<String>,
    pub tunnel: Option<Tunnel>,
    /// The fields DBine reads from a secret manager (their keys only).
    pub secret_fields: Vec<String>,
    /// The driver has extra ODBC attributes set (not copied: they may hold credentials).
    pub odbc_extra: bool,
}

/// Something that looks like a credential in a value we'd otherwise keep.
fn looks_secret(v: &str) -> bool {
    let l = v.to_ascii_lowercase();
    [
        "password",
        "pwd=",
        "passwd",
        "token",
        "secret",
        "apikey",
        "api_key",
        "api-key",
        "accountkey",
        "sharedaccess",
        "sig=",
    ]
    .iter()
    .any(|k| l.contains(k))
}

/// A URL without its user info, query string and fragment; anything else as it is.
pub fn strip_url_credentials(host: &str) -> String {
    let h = host.trim();
    let Some(scheme_end) = h.find("://") else {
        return h.to_string();
    };
    let (scheme, rest) = h.split_at(scheme_end + 3);
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let (authority, path) = rest.split_at(authority_end);
    let authority = authority
        .rsplit_once('@')
        .map(|(_, a)| a)
        .unwrap_or(authority);
    format!("{scheme}{authority}{path}")
}

impl PyConn {
    /// The export's view of a saved connection on `database`. Secrets in
    /// `cfg` (the password, secret options, tunnel credentials, the
    /// secret-manager definitions) are not copied, whether or not they're
    /// present.
    pub fn new(
        key: &str,
        name: &str,
        cfg: &ConnectionConfig,
        info: Option<&DriverInfo>,
        database: &str,
    ) -> PyConn {
        let secret_keys: HashSet<&str> = info
            .map(|i| {
                i.fields
                    .iter()
                    .filter(|f| f.secret)
                    .map(|f| f.key)
                    .collect()
            })
            .unwrap_or_default();
        let options = KEPT_OPTIONS
            .iter()
            .filter(|k| !secret_keys.contains(**k))
            .filter_map(|k| {
                cfg.option(k).map(|v| {
                    (
                        k.to_string(),
                        if v.contains("://") {
                            strip_url_credentials(v)
                        } else {
                            v.trim().to_string()
                        },
                    )
                })
            })
            .filter(|(_, v)| !v.is_empty() && !looks_secret(v))
            .collect();
        let tunnel = (cfg.option("ssh.enabled") == Some("true")).then(|| Tunnel {
            host: cfg.option("ssh.host").unwrap_or("").to_string(),
            user: cfg.option("ssh.user").unwrap_or("").to_string(),
            port: cfg
                .option("ssh.port")
                .and_then(|p| p.parse().ok())
                .unwrap_or(22),
        });
        // Only which fields: the definitions (where each secret is) stay out.
        let secret_fields = match cfg.option(dbine_core::secret_sources::OPTION) {
            None => Vec::new(),
            Some(json) => match serde_json::from_str::<Map<String, Value>>(json) {
                Ok(m) => m
                    .keys()
                    .filter(|k| {
                        k.chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
                    })
                    .cloned()
                    .collect(),
                Err(_) => vec!["password".into()],
            },
        };
        let host = strip_url_credentials(&cfg.host);
        let tls = Tls::of(cfg, info);
        let username = cfg.username_or_empty().trim().to_string();
        let database = if database.is_empty() {
            cfg.database.clone()
        } else {
            database.to_string()
        };
        PyConn {
            key: key.to_string(),
            name: name.to_string(),
            driver: cfg.driver.clone(),
            dialect: info.map(|i| i.dialect.to_string()).unwrap_or_default(),
            default_port: info.map(|i| i.default_port).unwrap_or(0),
            host,
            port: cfg.port,
            database: if looks_secret(&database) {
                String::new()
            } else {
                database
            },
            username: if looks_secret(&username) {
                String::new()
            } else {
                username
            },
            encrypt: tls.encrypt,
            tls,
            options,
            auth: cfg
                .option("auth")
                .or(cfg.option("auth_mode"))
                .filter(|a| {
                    a.len() <= 40
                        && a.chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                })
                .map(str::to_string),
            tunnel,
            secret_fields,
            odbc_extra: cfg.option("extra").is_some()
                && info.is_some_and(|i| i.fields.iter().any(|f| f.key == "extra")),
        }
    }

    fn opt(&self, k: &str) -> Option<&str> {
        self.options
            .get(k)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    fn port(&self) -> u16 {
        if self.port == 0 {
            self.default_port
        } else {
            self.port
        }
    }

    fn is_odbc(&self) -> bool {
        ODBC_IDS.contains(&self.driver.as_str())
    }
}

/// The drivers that connect through ODBC (crates/drivers/odbc).
pub(crate) const ODBC_IDS: &[&str] = &[
    "access",
    "altibase",
    "cache",
    "cloudera",
    "cubrid",
    "dameng",
    "db2",
    "db2i",
    "db2zos",
    "dbase",
    "exasol",
    "gbase8s",
    "heavydb",
    "hive",
    "ignite",
    "ignite3",
    "impala",
    "informix",
    "ingres",
    "iris",
    "kyuubi",
    "machbase",
    "maxdb",
    "mimer",
    "monetdb",
    "netezza",
    "netsuite",
    "nuodb",
    "ocient",
    "odbc",
    "openedge",
    "spark",
    "sqlanywhere",
    "sqream",
    "sybase",
    "teradata",
    "vertica",
    "virtuoso",
    "zen",
];

// ---------------------------------------------------------------------------
// Python text.

/// A Python string literal on one line ("…").
pub fn py_lit(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The text of a [`py_lit`] literal; `None` for any other expression.
pub fn py_unlit(lit: &str) -> Option<String> {
    let inner = lit.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '"' {
            return None;
        }
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'x' => {
                let hex: String = chars.by_ref().take(2).collect();
                out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
            }
            _ => return None,
        }
    }
    Some(out)
}

/// A Python triple-quoted string with `s` exactly (SQL, a script).
pub fn py_block(s: &str) -> String {
    let s = s.replace("\r\n", "\n");
    let s = s.trim_matches('\n');
    let mut out = String::from("\"\"\"");
    let mut quotes = 0;
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => {
                quotes += 1;
                // Every third quote in a row, and a quote right before the end, escaped.
                if quotes % 3 == 0 || i + 1 == chars.len() {
                    out.push_str("\\\"");
                } else {
                    out.push('"');
                }
                continue;
            }
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 && c != '\n' && c != '\t' => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
        quotes = 0;
    }
    out.push_str("\"\"\"");
    out
}

/// A JSON value as a Python literal.
pub fn py_value(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => (if *b { "True" } else { "False" }).into(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => py_lit(s),
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(py_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter()
                .map(|(k, v)| format!("{}: {}", py_lit(k), py_value(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Every line of `s` as a Python comment.
fn commented(s: &str) -> String {
    s.replace("\r\n", "\n")
        .trim_end()
        .lines()
        .map(|l| {
            if l.is_empty() {
                "#".to_string()
            } else {
                format!("# {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One line of comment text (no line breaks get through).
fn comment_line(s: &str) -> String {
    format!("# {}", s.replace(['\r', '\n'], " "))
}

const PY_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// The names the generated code itself uses at the top level.
const RESERVED_NAMES: &[&str] = &[
    "pd",
    "os",
    "duckdb",
    "text",
    "create_engine",
    "URL",
    "json",
    "json_util",
    "requests",
    "neo4j",
    "warnings",
    "shlex",
    "ssl",
    "redis",
    "boto3",
    "pyodbc",
    "clickhouse_connect",
    "MongoClient",
    "GraphDatabase",
    "Cluster",
    "PlainTextAuthProvider",
    "dict_factory",
    "DCAwareRoundRobinPolicy",
    "InfluxDBClient",
    "InfluxDBClient2",
    "InfluxDBClient3",
    "CouchbaseCluster",
    "CosmosClient",
    "ClusterOptions",
    "PasswordAuthenticator",
    "adbc_driver_flightsql",
    "TypeDeserializer",
    "redis_frame",
    "http_frame",
    "dynamo_frame",
    "flight_frame",
    "display",
];

/// Lowercase ASCII letters, digits and `_` (accents dropped), for a variable.
fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        let c = fold(c);
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    let out = out.trim_end_matches('_').to_string();
    if out.is_empty() {
        "db".into()
    } else if out.starts_with(|c: char| c.is_ascii_digit()) {
        format!("c_{out}")
    } else {
        out
    }
}

fn fold(c: char) -> char {
    match c {
        'á' | 'à' | 'ä' | 'â' | 'ã' | 'å' | 'Á' | 'À' | 'Ä' | 'Â' | 'Ã' | 'Å' => 'a',
        'é' | 'è' | 'ë' | 'ê' | 'É' | 'È' | 'Ë' | 'Ê' => 'e',
        'í' | 'ì' | 'ï' | 'î' | 'Í' | 'Ì' | 'Ï' | 'Î' => 'i',
        'ó' | 'ò' | 'ö' | 'ô' | 'õ' | 'Ó' | 'Ò' | 'Ö' | 'Ô' | 'Õ' => 'o',
        'ú' | 'ù' | 'ü' | 'û' | 'Ú' | 'Ù' | 'Ü' | 'Û' => 'u',
        'ñ' | 'Ñ' => 'n',
        'ç' | 'Ç' => 'c',
        c => c,
    }
}

/// The environment variable for one of a connection's secrets.
pub fn env_var(connection: &str, what: &str) -> String {
    format!("DBINE_{}_{}", slug(connection).to_ascii_uppercase(), what)
}

/// SQLAlchemy's `text()` takes `:name` as a bind parameter: every other
/// one is escaped (`\:name`), as its own pattern finds them.
fn escape_binds(sql: &str, params: &HashSet<String>) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let word = |c: char| c.is_alphanumeric() || c == '_';
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == ':' {
            let prev_ok =
                i == 0 || !(chars[i - 1] == ':' || chars[i - 1] == '\\' || word(chars[i - 1]));
            let mut j = i + 1;
            while j < chars.len() && word(chars[j]) {
                j += 1;
            }
            let is_bind = prev_ok && j > i + 1 && chars.get(j) != Some(&':');
            if is_bind && !params.contains(&chars[i + 1..j].iter().collect::<String>()) {
                out.push('\\');
            }
        }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------------------
// One client per connection.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Http {
    Elastic,
    OpenSearch,
    Solr,
    Couch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// SQLAlchemy engine: `pd.read_sql(text(…), engine)`.
    Sa,
    /// A DB-API connection (pyodbc): `pd.read_sql(sql, conn)`.
    Dbapi,
    DuckDb,
    ClickHouse,
    Mongo,
    Redis,
    Cassandra,
    Neo4j,
    Influx1,
    Influx2,
    Influx3,
    Http(Http),
    Dynamo,
    Couchbase,
    Flight,
    /// The client is set up but its queries need something the cell doesn't say.
    Cosmos,
    /// No client: the cells are commented.
    None,
}

/// What a connection adds to the setup cell and to the instructions.
struct Setup {
    kind: Kind,
    /// The Python lines that open it (assigning `var`).
    code: Vec<String>,
    packages: Vec<&'static str>,
    imports: Vec<&'static str>,
    helpers: Vec<&'static str>,
    /// (variable, what it holds).
    env: Vec<(String, String)>,
    /// What to set up instead (tunnels, cloud sign-in…), for the instructions.
    notes: Vec<String>,
    /// Kind::None: why.
    reason: Option<String>,
    /// Kind::Sa: the engine's URL, also what "Copiar cadena de conexión"
    /// writes as a SQLAlchemy URL (crate::connstring).
    sa: Option<SaUrl>,
}

impl Setup {
    fn new(kind: Kind) -> Self {
        Setup {
            kind,
            code: Vec::new(),
            packages: Vec::new(),
            imports: vec!["import os"],
            helpers: Vec::new(),
            env: Vec::new(),
            notes: Vec::new(),
            reason: None,
            sa: None,
        }
    }

    fn none(reason: impl Into<String>) -> Self {
        Setup {
            reason: Some(reason.into()),
            imports: Vec::new(),
            ..Setup::new(Kind::None)
        }
    }
}

/// Reads a secret from an environment variable (Python expression).
struct Secrets<'a> {
    c: &'a PyConn,
    env: Vec<(String, String)>,
}

impl Secrets<'_> {
    fn get(&mut self, what: &str, desc: &str, required: bool) -> String {
        let name = env_var(&self.c.name, what);
        if !self.env.iter().any(|(n, _)| *n == name) {
            self.env
                .push((name.clone(), format!("«{}»: {desc}", self.c.name)));
        }
        if required {
            format!("os.environ[{}]", py_lit(&name))
        } else {
            format!("os.environ.get({})", py_lit(&name))
        }
    }

    fn password(&mut self) -> String {
        let required = !self.c.username.is_empty();
        let desc = if self.c.username.is_empty() {
            "the password (optional)".to_string()
        } else {
            format!("the password of user `{}`", self.c.username)
        };
        self.get("PASSWORD", &desc, required)
    }

    /// The user: the connection's, else (from a secret manager) a variable.
    fn user(&mut self) -> Option<String> {
        if !self.c.username.is_empty() {
            Some(py_lit(&self.c.username))
        } else if self.c.secret_fields.iter().any(|f| f == "username") {
            Some(self.get("USER", "the user name", true))
        } else {
            None
        }
    }
}

/// `name=value` arguments, without the ones that are None.
fn kwargs(args: &[(&str, Option<String>)]) -> String {
    args.iter()
        .filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}={v}")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A SQLAlchemy `URL.create(…)` and its `connect_args`. The values are
/// Python expressions: literals (`py_lit`, numbers) or reads of an
/// environment variable (`os.environ[…]`, `os.environ.get(…)`).
#[derive(Debug, Clone, PartialEq)]
pub struct SaUrl {
    pub drivername: String,
    pub args: Vec<(String, String)>,
    pub query: Vec<(String, String)>,
    pub connect_args: Vec<(String, String)>,
}

/// The setup lines of a SQLAlchemy engine, and its URL.
struct SaEngine {
    lines: Vec<String>,
    url: SaUrl,
}

/// `create_engine(URL.create(…))`, one argument per line.
fn sa_engine(
    var: &str,
    drivername: &str,
    args: &[(&str, Option<String>)],
    query: &[(&str, String)],
    connect_args: &[(&str, String)],
) -> SaEngine {
    let url = SaUrl {
        drivername: drivername.to_string(),
        args: args
            .iter()
            .filter_map(|(k, v)| v.as_ref().map(|v| (k.to_string(), v.clone())))
            .collect(),
        query: query
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
        connect_args: connect_args
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect(),
    };
    let mut lines = vec![
        format!("{var} = create_engine("),
        "    URL.create(".to_string(),
        format!("        {},", py_lit(drivername)),
    ];
    for (k, v) in args.iter().filter_map(|(k, v)| v.as_ref().map(|v| (k, v))) {
        lines.push(format!("        {k}={v},"));
    }
    if !query.is_empty() {
        let q: Vec<String> = query
            .iter()
            .map(|(k, v)| format!("{}: {}", py_lit(k), py_lit(v)))
            .collect();
        lines.push(format!("        query={{{}}},", q.join(", ")));
    }
    lines.push("    ),".into());
    if !connect_args.is_empty() {
        let a: Vec<String> = connect_args
            .iter()
            .map(|(k, v)| format!("{}: {v}", py_lit(k)))
            .collect();
        lines.push(format!("    connect_args={{{}}},", a.join(", ")));
    }
    lines.push(")".into());
    SaEngine { lines, url }
}

/// The SQLAlchemy URL the Python export uses for a connection, if it uses
/// one (crate::connstring writes it as a URL string).
pub fn sqlalchemy_url(c: &PyConn) -> Option<SaUrl> {
    setup_for(c, "engine").sa
}

fn some_lit(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| py_lit(s))
}

fn port_arg(c: &PyConn) -> Option<String> {
    (c.port() != 0).then(|| c.port().to_string())
}

/// `{value}` when an ODBC attribute value needs quoting.
pub(crate) fn odbc_value(v: &str) -> String {
    if v.contains([';', '{', '}']) || v.starts_with(' ') || v.ends_with(' ') {
        format!("{{{}}}", v.replace('}', "}}"))
    } else {
        v.to_string()
    }
}

/// The base URL of an HTTP engine: its host when it's a URL, else built.
pub(crate) fn http_base(c: &PyConn) -> String {
    if c.host.starts_with("http://") || c.host.starts_with("https://") {
        c.host.trim_end_matches('/').to_string()
    } else {
        let scheme = if c.encrypt { "https" } else { "http" };
        let host = if c.host.is_empty() {
            "localhost"
        } else {
            &c.host
        };
        match c.port() {
            0 => format!("{scheme}://{host}"),
            p => format!("{scheme}://{host}:{p}"),
        }
    }
}

fn sign_in_notes(c: &PyConn, s: &mut Setup) {
    let name = &c.name;
    match c.auth.as_deref() {
        Some("aws_iam") => s.notes.push(format!(
            "«{name}» signs in with AWS IAM in DBine: generate an auth token (`boto3.client(\"rds\").generate_db_auth_token(…)`) and put it in the password variable."
        )),
        Some("gcp_iam") => s.notes.push(format!("«{name}» signs in with Google Cloud IAM in DBine: put an access token (`gcloud auth print-access-token`) in the password variable.")),
        Some("azure_entra") | Some("entra") | Some("entra_interactive") | Some("entra_password") | Some("entra_sp") | Some("entra_token") => s.notes.push(format!(
            "«{name}» signs in with Microsoft Entra ID in DBine: put an access token for the database in the password variable, or sign in the way your client library supports."
        )),
        Some("kerberos") | Some("x509") | Some("aws") | Some("oidc") | Some("ldap") => {
            s.notes.push(format!("«{name}» signs in with {} in DBine: configure that sign-in for the Python client (the code below uses a user and password).", c.auth.as_deref().unwrap_or("")))
        }
        _ => {}
    }
    if c.secret_fields.iter().any(|f| f != "username") {
        s.notes.push(format!("«{name}» reads its secrets from a secret manager in DBine: fill the variables from there (the export doesn't say where they are)."));
    }
    if let Some(t) = &c.tunnel {
        let target = format!(
            "{}:{}",
            if c.host.is_empty() {
                "localhost"
            } else {
                &c.host
            },
            c.port()
        );
        let at = if t.user.is_empty() {
            t.host.clone()
        } else {
            format!("{}@{}", t.user, t.host)
        };
        s.notes.push(format!(
            "«{name}» goes through an SSH tunnel in DBine: open one first (`ssh -N -L {p}:{target} {at} -p {sp}`) and change its host to `localhost` and port to `{p}` below.",
            p = c.port(),
            sp = t.port
        ));
    }
    if c.odbc_extra {
        s.notes.push(format!("«{name}» has extra ODBC attributes in DBine; they aren't copied (they may hold credentials): add the ones it needs."));
    }
}

fn aws_notes(c: &PyConn, s: &mut Setup) {
    match c.auth.as_deref() {
        Some("keys") => {
            for (v, d) in [("AWS_ACCESS_KEY_ID", "the AWS access key"), ("AWS_SECRET_ACCESS_KEY", "the AWS secret key"), ("AWS_SESSION_TOKEN", "the session token, if any")] {
                s.env.push((v.to_string(), format!("«{}»: {d}", c.name)));
            }
        }
        Some("profile") => s.notes.push(format!(
            "«{}» uses the AWS profile `{}`: boto3 reads it from `~/.aws` (or set `AWS_PROFILE`).",
            c.name,
            c.opt("profile").or(c.opt("aws_profile")).unwrap_or("default")
        )),
        _ => s.notes.push(format!("«{}» uses the default AWS credentials (environment, `~/.aws`, instance role): boto3 finds them the same way.", c.name)),
    }
}

fn gcp_notes(c: &PyConn, s: &mut Setup) {
    s.notes.push(format!(
        "«{}» uses Google Cloud credentials: run `gcloud auth application-default login` or set `GOOGLE_APPLICATION_CREDENTIALS` to a service-account file.",
        c.name
    ));
}

/// The client for a connection, assigned to `var`.
fn setup_for(c: &PyConn, var: &str) -> Setup {
    let mut sec = Secrets { c, env: Vec::new() };
    let mut s = setup_inner(c, var, &mut sec);
    if s.kind != Kind::None {
        sign_in_notes(c, &mut s);
    }
    let mut env = sec.env;
    env.append(&mut s.env);
    s.env = env;
    s
}

fn setup_inner(c: &PyConn, var: &str, sec: &mut Secrets) -> Setup {
    let host = some_lit(&c.host);
    let db = some_lit(&c.database);
    let id = c.driver.as_str();
    let sa_imports = vec![
        "import os",
        "from sqlalchemy import URL, create_engine, text",
    ];
    let sa = |packages: Vec<&'static str>, e: SaEngine| Setup {
        packages,
        code: e.lines,
        sa: Some(e.url),
        imports: sa_imports.clone(),
        ..Setup::new(Kind::Sa)
    };

    // Engines with their own way of naming the server first.
    match id {
        "sqlite" => return sa(vec![], sa_engine(var, "sqlite", &[("database", db.or(host))], &[], &[])),
        "libsql" => {
            let h = c.host.trim_start_matches("libsql://").trim_start_matches("https://").trim_end_matches('/').to_string();
            let token = sec.get("AUTH_TOKEN", "the auth token", false);
            return sa(vec!["sqlalchemy-libsql"], sa_engine(var, "sqlite+libsql", &[("host", some_lit(&h))], &[("secure", "true".into())], &[("auth_token", token)]));
        }
        "duckdb" => {
            let mut st = Setup { packages: vec!["duckdb"], imports: vec!["import duckdb"], ..Setup::new(Kind::DuckDb) };
            st.code.push(format!("{var} = duckdb.connect({}, read_only=True)", py_lit(&c.host)));
            return st;
        }
        "duckdb_files" => {
            return Setup::none("DBine shows the files of a folder as its tables: read them in a DataFrame SQL cell instead (`read_csv`, `read_parquet`…)")
        }
        "bigquery" => {
            let mut st = sa(vec!["sqlalchemy-bigquery"], sa_engine(var, "bigquery", &[("host", c.opt("project_id").map(py_lit)), ("database", db)], &[], &[]));
            gcp_notes(c, &mut st);
            return st;
        }
        "spanner" => {
            let path = format!("projects/{}/instances/{}/databases/{}", c.opt("project_id").unwrap_or(""), c.opt("instance").unwrap_or(""), c.database);
            let mut st = sa(vec!["sqlalchemy-spanner"], sa_engine(var, "spanner+spanner", &[("database", Some(py_lit(&path)))], &[], &[]));
            gcp_notes(c, &mut st);
            return st;
        }
        "athena" => {
            let region = c.opt("region").unwrap_or("us-east-1");
            let mut q = Vec::new();
            if let Some(o) = c.opt("output_location") {
                q.push(("s3_staging_dir", o.to_string()));
            }
            if let Some(w) = c.opt("workgroup") {
                q.push(("work_group", w.to_string()));
            }
            let args = [("host", Some(py_lit(&format!("athena.{region}.amazonaws.com")))), ("port", Some("443".into())), ("database", db)];
            let mut st = sa(vec!["pyathena[sqlalchemy]"], sa_engine(var, "awsathena+rest", &args, &q, &[]));
            aws_notes(c, &mut st);
            return st;
        }
        "snowflake" => {
            let database = match c.opt("schema") {
                Some(sc) if !c.database.is_empty() => format!("{}/{sc}", c.database),
                _ => c.database.clone(),
            };
            let mut q = Vec::new();
            for k in ["warehouse", "role"] {
                if let Some(v) = c.opt(k) {
                    q.push((k, v.to_string()));
                }
            }
            if c.auth.as_deref() == Some("keypair") {
                let mut st = sa(vec!["snowflake-sqlalchemy"], sa_engine(var, "snowflake", &[("username", sec.user()), ("host", c.opt("account").map(py_lit)), ("database", some_lit(&database))], &q, &[("private_key_file", sec.get("PRIVATE_KEY_FILE", "the path of the private key file", true))]));
                st.notes.push(format!("«{}» signs in with a key pair in DBine: put the key in a file and its path in the variable (DBine doesn't export the key).", c.name));
                return st;
            }
            let token = sec.get("TOKEN", "the programmatic access token (PAT)", true);
            return sa(vec!["snowflake-sqlalchemy"], sa_engine(var, "snowflake", &[("username", sec.user()), ("password", Some(token)), ("host", c.opt("account").map(py_lit)), ("database", some_lit(&database))], &q, &[]));
        }
        "databricks" | "azure_databricks" => {
            let wh = c.opt("warehouse").unwrap_or("");
            let http_path = if wh.starts_with('/') || wh.is_empty() { wh.to_string() } else { format!("/sql/1.0/warehouses/{wh}") };
            let mut q = vec![("http_path", http_path)];
            if !c.database.is_empty() {
                q.push(("catalog", c.database.clone()));
            }
            if let Some(sc) = c.opt("schema") {
                q.push(("schema", sc.to_string()));
            }
            let token = sec.get("TOKEN", "a personal access token", true);
            let h = c.host.trim_start_matches("https://").trim_end_matches('/').to_string();
            let mut st = sa(vec!["databricks-sqlalchemy"], sa_engine(var, "databricks", &[("username", Some(py_lit("token"))), ("password", Some(token)), ("host", some_lit(&h))], &q, &[]));
            if !matches!(c.auth.as_deref(), None | Some("pat")) {
                st.notes.push(format!("«{}» signs in with a service principal or OAuth in DBine: the code below uses a personal access token instead.", c.name));
            }
            return st;
        }
        "trino" | "starburst" | "presto" => {
            let database = match c.opt("schema") {
                Some(sc) if !c.database.is_empty() => format!("{}/{sc}", c.database),
                _ => c.database.clone(),
            };
            let (driver, pkg) = if id == "presto" { ("presto", "pyhive[presto]") } else { ("trino", "trino[sqlalchemy]") };
            let pw = c.encrypt.then(|| sec.password());
            let q: Vec<(&str, String)> = if c.encrypt && id != "presto" { vec![("http_scheme", "https".into())] } else { vec![] };
            return sa(vec![pkg], sa_engine(var, driver, &[("username", sec.user()), ("password", pw), ("host", host), ("port", port_arg(c)), ("database", some_lit(&database))], &q, &[]));
        }
        "oracle" | "oracle_adb" => {
            let user = sec.user();
            let pw = sec.password();
            if id == "oracle_adb" || c.opt("connect_by").is_some_and(|b| b != "service" && b != "sid") {
                let dsn = c.opt("connect_descriptor").or(c.opt("adb_service")).unwrap_or("");
                let mut ca = vec![("user", user.unwrap_or_else(|| "None".into())), ("password", pw), ("dsn", py_lit(dsn))];
                if let Some(w) = c.opt("wallet_dir") {
                    ca.push(("config_dir", py_lit(w)));
                    ca.push(("wallet_location", py_lit(w)));
                    ca.push(("wallet_password", sec.get("WALLET_PASSWORD", "the wallet's password", true)));
                }
                let mut st = sa(vec!["oracledb"], sa_engine(var, "oracle+oracledb", &[], &[], &ca));
                if dsn.is_empty() {
                    st.notes.push(format!("«{}» connects with a TNS alias in DBine: write the descriptor or alias in `dsn` (and set `TNS_ADMIN`).", c.name));
                }
                return st;
            }
            let (database, q) = if c.opt("connect_by") == Some("sid") {
                (c.opt("service").map(py_lit), vec![])
            } else {
                (None, c.opt("service").map(|sv| vec![("service_name", sv.to_string())]).unwrap_or_default())
            };
            return sa(vec!["oracledb"], sa_engine(var, "oracle+oracledb", &[("username", user), ("password", Some(pw)), ("host", host), ("port", port_arg(c)), ("database", database)], &q, &[]));
        }
        "cosmosdb" => {
            let mut st = Setup { packages: vec!["azure-cosmos"], imports: vec!["import os", "from azure.cosmos import CosmosClient"], ..Setup::new(Kind::Cosmos) };
            let key = sec.get("ACCOUNT_KEY", "the account key", true);
            st.code.push(format!("{var} = CosmosClient({}, credential={key}).get_database_client({})", py_lit(&http_base(c)), py_lit(&c.database)));
            return st;
        }
        "couchbase" => {
            let mut st = Setup {
                packages: vec!["couchbase"],
                imports: vec!["import os", "from couchbase.auth import PasswordAuthenticator", "from couchbase.cluster import Cluster as CouchbaseCluster", "from couchbase.options import ClusterOptions"],
                ..Setup::new(Kind::Couchbase)
            };
            let scheme = if c.encrypt { "couchbases" } else { "couchbase" };
            let user = sec.user().unwrap_or_else(|| py_lit(""));
            let pw = sec.password();
            st.code.push(format!("{var} = CouchbaseCluster({}, ClusterOptions(PasswordAuthenticator({user}, {pw})))", py_lit(&format!("{scheme}://{}", c.host))));
            return st;
        }
        "dynamodb" => {
            let mut st = Setup { packages: vec!["boto3"], imports: vec!["import boto3", "from boto3.dynamodb.types import TypeDeserializer"], helpers: vec![DYNAMO_HELPER], ..Setup::new(Kind::Dynamo) };
            let session = match (c.auth.as_deref(), c.opt("profile")) {
                (Some("profile"), Some(p)) => format!("boto3.Session(profile_name={})", py_lit(p)),
                _ => "boto3.Session()".into(),
            };
            st.code.push(format!(
                "{var} = {session}.client({})",
                kwargs(&[("service_name", Some(py_lit("dynamodb"))), ("region_name", c.opt("region").map(py_lit)), ("endpoint_url", c.opt("endpoint_url").map(py_lit))])
            ));
            aws_notes(c, &mut st);
            return st;
        }
        "flightsql" | "dremio" => {
            let mut st = Setup { packages: vec!["adbc-driver-flightsql", "pyarrow"], imports: vec!["import os", "import adbc_driver_flightsql.dbapi"], helpers: vec![FLIGHT_HELPER], ..Setup::new(Kind::Flight) };
            let port = if id == "dremio" { 32010 } else { c.port() };
            let scheme = if c.encrypt { "grpc+tls" } else { "grpc" };
            let mut kw = Vec::new();
            if let Some(u) = sec.user() {
                kw.push(format!("\"username\": {u}"));
                kw.push(format!("\"password\": {}", sec.password()));
            } else {
                kw.push(format!("\"adbc.flight.sql.authorization_header\": \"Bearer \" + {}", sec.get("TOKEN", "the access token", true)));
            }
            st.code.push(format!("{var} = adbc_driver_flightsql.dbapi.connect({}, db_kwargs={{{}}})", py_lit(&format!("{scheme}://{}:{port}", c.host)), kw.join(", ")));
            if id == "dremio" {
                st.notes.push(format!("«{}»: DBine uses Dremio's REST API; Python uses its Arrow Flight port (32010 by default): change it below if yours is another.", c.name));
            }
            return st;
        }
        "clickhouse" | "timeplus" => {
            let mut st = Setup { packages: vec!["clickhouse-connect"], imports: vec!["import os", "import clickhouse_connect"], ..Setup::new(Kind::ClickHouse) };
            let pw = sec.password();
            st.code.push(format!(
                "{var} = clickhouse_connect.get_client({})",
                kwargs(&[("host", host), ("port", port_arg(c)), ("username", sec.user()), ("password", Some(pw)), ("database", db), ("secure", c.encrypt.then(|| "True".into()))])
            ));
            return st;
        }
        "mongodb" | "documentdb" | "ferretdb" => {
            let mut st = Setup { packages: vec!["pymongo"], imports: vec!["import os", "from pymongo import MongoClient"], ..Setup::new(Kind::Mongo) };
            let uri = c.host.starts_with("mongodb://") || c.host.starts_with("mongodb+srv://");
            let user = sec.user();
            let pw = user.is_some().then(|| sec.password());
            let args = kwargs(&[
                ("host", host),
                ("port", if uri { None } else { port_arg(c) }),
                ("username", user),
                ("password", pw),
                ("authSource", c.opt("auth_source").map(py_lit)),
                ("replicaSet", c.opt("replica_set").map(py_lit)),
                ("tls", c.encrypt.then(|| "True".into())),
            ]);
            st.code.push(format!("{var} = MongoClient({args})[{}]", py_lit(if c.database.is_empty() { "test" } else { &c.database })));
            if id == "documentdb" {
                st.notes.push(format!("«{}» is Amazon DocumentDB: it needs TLS with Amazon's CA bundle (`tlsCAFile=…`) and `retryWrites=False`.", c.name));
            }
            return st;
        }
        "redis" | "valkey" | "dragonfly" => {
            let mut st = Setup { packages: vec!["redis"], imports: vec!["import os", "import shlex", "import redis"], helpers: vec![REDIS_HELPER], ..Setup::new(Kind::Redis) };
            let pw = sec.password();
            st.code.push(format!(
                "{var} = redis.Redis({})",
                kwargs(&[
                    ("host", host),
                    ("port", port_arg(c)),
                    ("db", Some(c.database.parse::<u32>().unwrap_or(0).to_string())),
                    ("username", sec.user()),
                    ("password", Some(pw)),
                    ("ssl", c.encrypt.then(|| "True".into())),
                    ("decode_responses", Some("True".into())),
                ])
            ));
            return st;
        }
        "cassandra" | "scylladb" | "keyspaces" => {
            let mut st = Setup {
                packages: vec!["cassandra-driver"],
                imports: vec!["import os", "from cassandra.auth import PlainTextAuthProvider", "from cassandra.cluster import Cluster", "from cassandra.query import dict_factory"],
                ..Setup::new(Kind::Cassandra)
            };
            let tls = c.encrypt || id == "keyspaces";
            let mut args = vec![format!("[{}]", py_lit(&c.host)), format!("port={}", c.port())];
            if let Some(u) = sec.user() {
                args.push(format!("auth_provider=PlainTextAuthProvider({u}, {})", sec.password()));
            }
            if tls {
                st.imports.push("import ssl");
                args.push("ssl_context=ssl.create_default_context()".into());
            }
            if let Some(dc) = c.opt("datacenter") {
                st.imports.push("from cassandra.policies import DCAwareRoundRobinPolicy");
                args.push(format!("load_balancing_policy=DCAwareRoundRobinPolicy(local_dc={})", py_lit(dc)));
            }
            st.code.push(format!("{var} = Cluster({}).connect({})", args.join(", "), if c.database.is_empty() { String::new() } else { py_lit(&c.database) }));
            st.code.push(format!("{var}.row_factory = dict_factory"));
            if id == "keyspaces" {
                st.notes.push(format!("«{}» is Amazon Keyspaces: it needs TLS with Amazon's certificate (load it into the SSL context) and service-specific credentials.", c.name));
            }
            return st;
        }
        "neo4j" | "memgraph" | "neptune" => {
            let mut st = Setup { packages: vec!["neo4j"], imports: vec!["import os", "import neo4j", "from neo4j import GraphDatabase"], ..Setup::new(Kind::Neo4j) };
            let scheme = match (id, c.encrypt || id == "neptune") {
                ("neo4j", true) => "neo4j+s",
                ("neo4j", false) => "neo4j",
                (_, true) => "bolt+s",
                (_, false) => "bolt",
            };
            let auth = match sec.user() {
                Some(u) if id != "neptune" => format!("({u}, {})", sec.password()),
                _ => "None".into(),
            };
            st.code.push(format!("{var} = GraphDatabase.driver({}, auth={auth})", py_lit(&format!("{scheme}://{}:{}", c.host, c.port()))));
            if id == "neptune" && c.opt("iam") == Some("true") {
                st.notes.push(format!("«{}» signs in to Neptune with IAM in DBine: the driver needs a SigV4 auth token (see AWS's Neptune openCypher Bolt docs).", c.name));
            }
            return st;
        }
        "influxdb1" => {
            let mut st = Setup { packages: vec!["influxdb"], imports: vec!["import os", "from influxdb import InfluxDBClient"], ..Setup::new(Kind::Influx1) };
            let pw = sec.password();
            st.code.push(format!(
                "{var} = InfluxDBClient({})",
                kwargs(&[("host", host), ("port", port_arg(c)), ("username", sec.user()), ("password", Some(pw)), ("database", db), ("ssl", c.encrypt.then(|| "True".into()))])
            ));
            return st;
        }
        "influxdb" => {
            let mut st = Setup { packages: vec!["influxdb-client"], imports: vec!["import os", "from influxdb_client import InfluxDBClient as InfluxDBClient2"], ..Setup::new(Kind::Influx2) };
            let token = sec.get("TOKEN", "the API token", true);
            st.code.push(format!("{var} = InfluxDBClient2({})", kwargs(&[("url", Some(py_lit(&http_base(c)))), ("token", Some(token)), ("org", c.opt("org").map(py_lit))])));
            return st;
        }
        "influxdb3" => {
            let mut st = Setup { packages: vec!["influxdb3-python"], imports: vec!["import os", "from influxdb_client_3 import InfluxDBClient3"], ..Setup::new(Kind::Influx3) };
            let token = sec.get("TOKEN", "the token", true);
            st.code.push(format!("{var} = InfluxDBClient3({})", kwargs(&[("host", Some(py_lit(&http_base(c)))), ("token", Some(token)), ("database", db)])));
            return st;
        }
        "elasticsearch" | "opensearch" | "opendistro" | "solr" | "couchdb" => {
            let flavor = match id {
                "solr" => Http::Solr,
                "couchdb" => Http::Couch,
                "elasticsearch" => Http::Elastic,
                _ => Http::OpenSearch,
            };
            let mut st = Setup { packages: vec!["requests"], imports: vec!["import os", "import requests"], helpers: vec![HTTP_HELPER], ..Setup::new(Kind::Http(flavor)) };
            st.code.push(format!("{var} = requests.Session()"));
            st.code.push(format!("{var}.base_url = {}", py_lit(&http_base(c))));
            if let Some(u) = sec.user() {
                st.code.push(format!("{var}.auth = ({u}, {})", sec.password()));
            } else if matches!(flavor, Http::Elastic | Http::OpenSearch) {
                st.code.push(format!("{var}.headers[\"Authorization\"] = \"ApiKey \" + {}", sec.get("API_KEY", "the API key", true)));
            }
            if c.opt("cloud_id").is_some() {
                st.notes.push(format!("«{}» connects with an Elastic Cloud ID in DBine: write the deployment's Elasticsearch URL in `base_url`.", c.name));
            }
            return st;
        }
        "dsql" => {
            let pw = sec.get("PASSWORD", "an IAM auth token", true);
            let mut st = sa(vec!["psycopg[binary]"], sa_engine(var, "postgresql+psycopg", &[("username", Some(py_lit(if c.username.is_empty() { "admin" } else { &c.username }))), ("password", Some(pw)), ("host", host), ("port", port_arg(c)), ("database", Some(py_lit("postgres")))], &[("sslmode", "require".into())], &[]));
            st.notes.push(format!("«{}» is Aurora DSQL: generate an auth token (`boto3.client(\"dsql\").generate_db_connect_admin_auth_token(…)`) and put it in its variable.", c.name));
            return st;
        }
        "iotdb" | "timechodb" => return Setup::none("DBine reads IoTDB through its REST API; the Python client (apache-iotdb) uses the session port and another API: write that query by hand"),
        "ksqldb" => return Setup::none("ksqlDB has no maintained Python client: post the statement to its REST API (`/query-stream`) with requests"),
        "orientdb" => return Setup::none("OrientDB has no maintained Python client for its current versions: run the query through its REST API with requests"),
        "etcd" => return Setup::none("DBine's etcd commands have no direct Python equivalent: use the etcd3 package's get/get_prefix"),
        "cloudera" => return Setup::none("Cloudera Data Warehouse connects over HTTP with an HTTP path: set up impyla's or pyhive's HTTP transport for it"),
        _ => {}
    }

    // ODBC engines: a SQLAlchemy dialect where there is a maintained one, else pyodbc.
    if c.is_odbc() {
        let with = |drivername: &str, pkgs: Vec<&'static str>, sec: &mut Secrets| -> Setup {
            let user = sec.user();
            let pw = sec.password();
            sa(
                pkgs,
                sa_engine(
                    var,
                    drivername,
                    &[
                        ("username", user),
                        ("password", Some(pw)),
                        ("host", host.clone()),
                        ("port", port_arg(c)),
                        ("database", db.clone()),
                    ],
                    &[],
                    &[],
                ),
            )
        };
        return match id {
            "db2" | "db2i" | "db2zos" => with("db2+ibm_db", vec!["ibm-db-sa"], sec),
            "teradata" => with("teradatasql", vec!["teradatasqlalchemy"], sec),
            "vertica" => with(
                "vertica+vertica_python",
                vec!["sqlalchemy-vertica-python"],
                sec,
            ),
            "exasol" => with("exa+websocket", vec!["sqlalchemy-exasol"], sec),
            "hive" | "spark" | "kyuubi" => with("hive", vec!["pyhive[hive_pure_sasl]"], sec),
            "impala" => with("impala", vec!["impyla"], sec),
            "monetdb" => with("monetdb", vec!["sqlalchemy-monetdb"], sec),
            "iris" | "cache" => with("iris", vec!["sqlalchemy-iris"], sec),
            _ => odbc_setup(c, var, sec),
        };
    }

    match c.dialect.as_str() {
        "postgres" => {
            // The same sslmode as DBine's copied connection strings.
            let q: Vec<(&str, String)> = libpq_sslmode(&c.tls, false)
                .map(|m| vec![("sslmode", m)])
                .unwrap_or_default();
            let user = sec.user();
            let pw = sec.password();
            sa(
                vec!["psycopg[binary]"],
                sa_engine(
                    var,
                    "postgresql+psycopg",
                    &[
                        ("username", user),
                        ("password", Some(pw)),
                        ("host", host),
                        ("port", port_arg(c)),
                        ("database", db),
                    ],
                    &q,
                    &[],
                ),
            )
        }
        "mysql" => {
            let user = sec.user();
            let pw = sec.password();
            let ca: Vec<(&str, String)> = if c.encrypt {
                vec![("ssl", "{}".into())]
            } else {
                vec![]
            };
            sa(
                vec!["pymysql"],
                sa_engine(
                    var,
                    "mysql+pymysql",
                    &[
                        ("username", user),
                        ("password", Some(pw)),
                        ("host", host),
                        ("port", port_arg(c)),
                        ("database", db),
                    ],
                    &[],
                    &ca,
                ),
            )
        }
        "mssql" => {
            let user = sec.user();
            let pw = sec.password();
            sa(
                vec!["pymssql"],
                sa_engine(
                    var,
                    "mssql+pymssql",
                    &[
                        ("username", user),
                        ("password", Some(pw)),
                        ("host", host),
                        ("port", port_arg(c)),
                        ("database", db),
                    ],
                    &[],
                    &[],
                ),
            )
        }
        _ => match id {
            "hana" => {
                let user = sec.user();
                let pw = sec.password();
                let q: Vec<(&str, String)> = if c.encrypt {
                    vec![("encrypt", "true".into())]
                } else {
                    vec![]
                };
                sa(
                    vec!["sqlalchemy-hana", "hdbcli"],
                    sa_engine(
                        var,
                        "hana+hdbcli",
                        &[
                            ("username", user),
                            ("password", Some(pw)),
                            ("host", host),
                            ("port", port_arg(c)),
                            ("database", db),
                        ],
                        &q,
                        &[],
                    ),
                )
            }
            "firebird" => {
                let user = sec.user();
                let pw = sec.password();
                sa(
                    vec!["sqlalchemy-firebird"],
                    sa_engine(
                        var,
                        "firebird+firebird",
                        &[
                            ("username", user),
                            ("password", Some(pw)),
                            ("host", host),
                            ("port", port_arg(c)),
                            ("database", db),
                        ],
                        &[],
                        &[],
                    ),
                )
            }
            "drill" => {
                let user = sec.user();
                let pw = user.is_some().then(|| sec.password());
                let q = vec![(
                    "use_ssl",
                    if c.encrypt { "True" } else { "False" }.to_string(),
                )];
                sa(
                    vec!["sqlalchemy-drill"],
                    sa_engine(
                        var,
                        "drill+sadrill",
                        &[
                            ("username", user),
                            ("password", pw),
                            ("host", host),
                            ("port", port_arg(c)),
                            ("database", db),
                        ],
                        &q,
                        &[],
                    ),
                )
            }
            "phoenix" | "avatica" => sa(
                vec!["phoenixdb"],
                sa_engine(
                    var,
                    "phoenix",
                    &[("host", host), ("port", port_arg(c))],
                    &[],
                    &[],
                ),
            ),
            "tdengine" => {
                let user = sec.user();
                let pw = sec.password();
                sa(
                    vec!["taospy"],
                    sa_engine(
                        var,
                        "taosrest",
                        &[
                            ("username", user),
                            ("password", Some(pw)),
                            ("host", host),
                            ("port", port_arg(c)),
                            ("database", db),
                        ],
                        &[],
                        &[],
                    ),
                )
            }
            _ => Setup::none(format!(
                "DBine has no Python mapping for this engine ({id}) yet"
            )),
        },
    }
}

/// pyodbc with the driver's ODBC driver name (or the DSN).
fn odbc_setup(c: &PyConn, var: &str, sec: &mut Secrets) -> Setup {
    let mut parts: Vec<String> = Vec::new();
    match (c.driver.as_str(), c.opt("dsn"), c.opt("odbc_driver")) {
        ("odbc", Some(dsn), _) => parts.push(format!("DSN={}", odbc_value(dsn))),
        ("odbc", None, _) => return Setup::none("this ODBC connection uses a connection string, which DBine keeps with the secrets (it may hold credentials): write it in pyodbc.connect"),
        (_, _, Some(d)) => parts.push(format!("DRIVER={{{}}}", d.replace('}', "}}"))),
        _ => return Setup::none("the connection names no ODBC driver: write the connection string in pyodbc.connect"),
    }
    let file = matches!(c.driver.as_str(), "access" | "dbase");
    if file {
        parts.push(format!("DBQ={}", odbc_value(&c.database)));
    } else {
        if !c.host.is_empty() {
            parts.push(format!("SERVER={}", odbc_value(&c.host)));
        }
        if c.port() != 0 {
            parts.push(format!("PORT={}", c.port()));
        }
        if !c.database.is_empty() {
            parts.push(format!("DATABASE={}", odbc_value(&c.database)));
        }
    }
    let mut st = Setup {
        packages: vec!["pyodbc"],
        imports: vec!["import os", "import warnings", "import pyodbc"],
        ..Setup::new(Kind::Dbapi)
    };
    let user = sec.user();
    if let Some(u) = &user {
        // A literal user goes in the string; one from a variable is added to it.
        if u.starts_with('"') {
            parts.push(format!("UID={}", odbc_value(&c.username)));
        }
    }
    let mut expr = py_lit(&(parts.join(";") + ";"));
    if let Some(u) = user.filter(|u| !u.starts_with('"')) {
        expr.push_str(&format!(" + \"UID=\" + {u} + \";\""));
    }
    let pw = sec.password();
    expr.push_str(&format!(" + \"PWD=\" + ({pw} or \"\") + \";\""));
    st.code.push(format!("{var} = pyodbc.connect({expr})"));
    st.code.push(
        "warnings.filterwarnings(\"ignore\", message=\".*pandas only supports SQLAlchemy.*\")"
            .into(),
    );
    st
}

const REDIS_HELPER: &str = r##"def redis_frame(client, script):
    """Runs each command line; the last reply as a DataFrame."""
    reply = None
    for line in script.splitlines():
        line = line.strip()
        if line and not line.startswith(("#", "//")):
            reply = client.execute_command(*shlex.split(line))
    if isinstance(reply, dict):
        return pd.DataFrame({"key": list(reply.keys()), "value": list(reply.values())})
    if isinstance(reply, (list, tuple, set)):
        return pd.DataFrame({"value": list(reply)})
    return pd.DataFrame({"value": [reply]})"##;

const HTTP_HELPER: &str = r##"def http_frame(client, method, path, body=None):
    """One HTTP request; the documents or rows of its answer as a DataFrame."""
    r = client.request(method, client.base_url + path, json=body, timeout=120)
    r.raise_for_status()
    data = r.json()
    if isinstance(data, dict):
        if isinstance(data.get("hits"), dict) and "hits" in data["hits"]:
            return pd.json_normalize([h.get("_source", h) for h in data["hits"]["hits"]])
        if "columns" in data and "rows" in data:
            return pd.DataFrame(data["rows"], columns=[c["name"] for c in data["columns"]])
        if "schema" in data and "datarows" in data:
            return pd.DataFrame(data["datarows"], columns=[c["name"] for c in data["schema"]])
        if isinstance(data.get("response"), dict) and "docs" in data["response"]:
            return pd.json_normalize(data["response"]["docs"])
        for key in ("docs", "rows"):
            if isinstance(data.get(key), list):
                return pd.json_normalize(data[key])
    return pd.json_normalize(data if isinstance(data, (dict, list)) else {"value": data})"##;

const DYNAMO_HELPER: &str = r##"def dynamo_frame(client, statement):
    """A PartiQL statement, every page, as a DataFrame."""
    des, items, token = TypeDeserializer(), [], None
    while True:
        page = client.execute_statement(Statement=statement, **({"NextToken": token} if token else {}))
        items += [{k: des.deserialize(v) for k, v in item.items()} for item in page.get("Items", [])]
        token = page.get("NextToken")
        if not token:
            return pd.DataFrame(items)"##;

const FLIGHT_HELPER: &str = r##"def flight_frame(conn, sql):
    with conn.cursor() as cur:
        cur.execute(sql)
        return cur.fetch_df()"##;

// ---------------------------------------------------------------------------
// Cells.

/// A MongoDB shell call DBine accepts that maps to pymongo directly.
#[derive(Debug, PartialEq)]
enum MongoCall {
    Find {
        coll: String,
        args: Vec<Value>,
        chain: Vec<(String, Value)>,
    },
    Aggregate {
        coll: String,
        pipeline: Value,
    },
}

/// The text of a balanced `( … )` starting at `open` (an index of `(`), and the index after it.
fn balanced(s: &[char], open: usize) -> Option<(String, usize)> {
    let mut depth = 0;
    let mut quote: Option<char> = None;
    let mut i = open;
    while i < s.len() {
        let c = s[i];
        if let Some(q) = quote {
            if c == '\\' {
                i += 2;
                continue;
            }
            if c == q {
                quote = None;
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
        } else if c == '(' || c == '[' || c == '{' {
            depth += 1;
        } else if c == ')' || c == ']' || c == '}' {
            depth -= 1;
            if depth == 0 {
                return Some((s[open + 1..i].iter().collect(), i + 1));
            }
        }
        i += 1;
    }
    None
}

/// `db.coll.find(…)…` / `db.getCollection("c").aggregate([…])` with strict
/// JSON arguments; anything else (relaxed JSON, shell constructors, several
/// statements, other methods) isn't mapped.
fn mongo_call(src: &str) -> Option<MongoCall> {
    let text: String = src
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim().trim_end_matches(';').trim();
    let rest = text.strip_prefix("db")?.trim_start();
    let chars: Vec<char> = rest.chars().collect();
    let mut i = 0;
    let coll;
    if rest.starts_with(".getCollection(") {
        let (arg, next) = balanced(&chars, ".getCollection".len())?;
        coll = serde_json::from_str::<String>(arg.trim()).ok()?;
        i = next;
    } else {
        if chars.first() != Some(&'.') {
            return None;
        }
        i += 1;
        let start = i;
        while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '-')
        {
            i += 1;
        }
        coll = chars[start..i].iter().collect();
    }
    if coll.is_empty() {
        return None;
    }
    let mut calls: Vec<(String, String)> = Vec::new();
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i == chars.len() {
            break;
        }
        if chars[i] != '.' {
            return None;
        }
        i += 1;
        let start = i;
        while i < chars.len() && chars[i].is_alphanumeric() {
            i += 1;
        }
        let name: String = chars[start..i].iter().collect();
        if chars.get(i) != Some(&'(') {
            return None;
        }
        let (arg, next) = balanced(&chars, i)?;
        calls.push((name, arg));
        i = next;
    }
    let parse_args = |a: &str| -> Option<Vec<Value>> {
        serde_json::from_str::<Value>(&format!("[{a}]"))
            .ok()?
            .as_array()
            .cloned()
    };
    let (first, rest) = calls.split_first()?;
    match first.0.as_str() {
        "find" => {
            let args = parse_args(&first.1)?;
            if args.len() > 2 || args.iter().any(|a| !a.is_object()) {
                return None;
            }
            let mut chain = Vec::new();
            for (m, a) in rest {
                let mut v = parse_args(a)?;
                match (m.as_str(), v.len()) {
                    ("sort", 1) if v[0].is_object() => chain.push((m.clone(), v.remove(0))),
                    ("limit" | "skip", 1) if v[0].is_u64() => chain.push((m.clone(), v.remove(0))),
                    ("toArray" | "pretty", 0) => {}
                    _ => return None,
                }
            }
            Some(MongoCall::Find { coll, args, chain })
        }
        "aggregate" => {
            let mut args = parse_args(&first.1)?;
            if args.is_empty()
                || args.len() > 2
                || !args[0].is_array()
                || rest.iter().any(|(m, _)| m != "toArray")
            {
                return None;
            }
            Some(MongoCall::Aggregate {
                coll,
                pipeline: args.remove(0),
            })
        }
        _ => None,
    }
}

/// Extended JSON markers pymongo wouldn't read from a plain dict.
fn has_extended_json(v: &Value) -> bool {
    match v {
        Value::Object(o) => {
            o.keys().any(|k| {
                matches!(
                    k.as_str(),
                    "$oid"
                        | "$date"
                        | "$numberLong"
                        | "$numberDecimal"
                        | "$numberInt"
                        | "$numberDouble"
                        | "$regex"
                        | "$regularExpression"
                        | "$binary"
                        | "$timestamp"
                        | "$uuid"
                )
            }) || o.values().any(has_extended_json)
        }
        Value::Array(a) => a.iter().any(has_extended_json),
        _ => false,
    }
}

/// A JSON argument for pymongo: a literal, or `json_util.loads(…)` for extended JSON.
fn mongo_arg(v: &Value, imports: &mut BTreeSet<&'static str>) -> String {
    if has_extended_json(v) {
        imports.insert("from bson import json_util");
        format!("json_util.loads({})", py_lit(&v.to_string()))
    } else {
        py_value(v)
    }
}

/// An HTTP console request DBine accepts (Elasticsearch, OpenSearch, Solr, CouchDB).
fn http_request(
    src: &str,
    flavor: Http,
    database: &str,
) -> Result<(String, String, Option<Value>), String> {
    let lines: Vec<&str> = src
        .lines()
        .filter(|l| !(l.trim_start().starts_with('#') || l.trim_start().starts_with("//")))
        .collect();
    let text = lines.join("\n");
    let text = text.trim();
    if text.is_empty() {
        return Err("the cell is empty".into());
    }
    let first = text.lines().next().unwrap_or("").trim();
    let upper = first.to_ascii_uppercase();
    if matches!(flavor, Http::Elastic | Http::OpenSearch)
        && ["SELECT", "SHOW", "DESCRIBE", "DESC "]
            .iter()
            .any(|k| upper.starts_with(k))
    {
        let path = if flavor == Http::Elastic {
            "/_sql?format=json"
        } else {
            "/_plugins/_sql"
        };
        return Ok((
            "POST".into(),
            path.into(),
            Some(json!({ "query": text.trim_end_matches(';') })),
        ));
    }
    if flavor == Http::Couch && text.starts_with('{') {
        let body: Value = serde_json::from_str(text)
            .map_err(|_| "the Mango query isn't strict JSON (or there are several)".to_string())?;
        return Ok(("POST".into(), format!("/{database}/_find"), Some(body)));
    }
    let mut it = first.splitn(3, char::is_whitespace);
    let method = it.next().unwrap_or("").to_ascii_uppercase();
    let path = it.next().unwrap_or("").to_string();
    if !matches!(
        method.as_str(),
        "GET" | "POST" | "PUT" | "DELETE" | "HEAD" | "PATCH"
    ) || path.is_empty()
    {
        return Err("it isn't a single `METHOD /path` request".into());
    }
    let after_first = it.next().unwrap_or("").to_string();
    let body_text = format!(
        "{after_first}\n{}",
        text.lines().skip(1).collect::<Vec<_>>().join("\n")
    );
    let body_text = body_text.trim();
    let body = if body_text.is_empty() {
        None
    } else {
        Some(serde_json::from_str::<Value>(body_text).map_err(|_| {
            "its body isn't one strict JSON document (or there are several requests)".to_string()
        })?)
    };
    let read = method == "GET"
        || (method == "POST"
            && [
                "_search",
                "_find",
                "_sql",
                "_count",
                "_msearch",
                "/select",
                "_all_docs",
                "_view",
                "/query",
                "_explain",
            ]
            .iter()
            .any(|k| path.contains(k)));
    if !read {
        return Err(format!(
            "`{method} {path}` changes data or settings: it isn't run from the exported notebook"
        ));
    }
    let path = if path.starts_with('/') {
        path
    } else if flavor == Http::Couch {
        format!("/{database}/{path}")
    } else {
        format!("/{path}")
    };
    Ok((method, path, body))
}

/// A SQL cell as Python: the code, or (commented) why it can't be mapped.
fn query_code(
    kind: Kind,
    client: &str,
    var: &str,
    c: &PyConn,
    source: &str,
    params: &[(String, Value)],
    imports: &mut BTreeSet<&'static str>,
) -> Result<String, String> {
    // The notebook's `{{name}}`: SQLAlchemy binds (`:name`); no other client maps them.
    let names: Vec<String> = params.iter().map(|(n, _)| n.clone()).collect();
    let marked = super::params::mentioned_names(source, &names);
    if !marked.is_empty() && kind != Kind::Sa {
        return Err(format!("the notebook's parameters ({}) are only mapped to SQLAlchemy binds: write their values here", marked.join(", ")));
    }
    let bound_source = super::params::to_named_binds(source, &names);
    let source = bound_source.as_str();
    let lit = py_block(source);
    Ok(match kind {
        Kind::Sa => {
            let names: HashSet<String> = params.iter().map(|(n, _)| n.clone()).collect();
            let used: Vec<&String> = params.iter().map(|(n, _)| n).filter(|n| bind_used(source, n)).collect();
            let sql = py_block(&escape_binds(source, &names));
            if used.is_empty() {
                format!("{var} = pd.read_sql(text({sql}), {client})")
            } else {
                let p: Vec<String> = used.iter().map(|n| format!("{}: {n}", py_lit(n))).collect();
                format!("{var} = pd.read_sql(text({sql}), {client}, params={{{}}})", p.join(", "))
            }
        }
        Kind::Dbapi => format!("{var} = pd.read_sql({lit}, {client})"),
        Kind::DuckDb => format!("{var} = {client}.sql({lit}).df()"),
        Kind::ClickHouse => format!("{var} = {client}.query_df({lit})"),
        Kind::Cassandra => format!("{var} = pd.DataFrame(list({client}.execute({})))", py_block(source.trim().trim_end_matches(';'))),
        Kind::Neo4j => {
            let db = if c.database.is_empty() || c.driver != "neo4j" { "None".to_string() } else { py_lit(&c.database) };
            format!("{var} = {client}.execute_query({lit}, database_={db}, result_transformer_=neo4j.Result.to_df)")
        }
        Kind::Influx1 => format!("{var} = pd.DataFrame(list({client}.query({lit}).get_points()))"),
        Kind::Influx2 => format!("{var} = {client}.query_api().query_data_frame({lit})"),
        Kind::Influx3 => format!("{var} = {client}.query({lit}, language=\"sql\", mode=\"pandas\")"),
        Kind::Redis => format!("{var} = redis_frame({client}, {lit})"),
        Kind::Dynamo => format!("{var} = dynamo_frame({client}, {})", py_block(source.trim().trim_end_matches(';'))),
        Kind::Couchbase => format!("{var} = pd.DataFrame(list({client}.query({lit})))"),
        Kind::Flight => format!("{var} = flight_frame({client}, {lit})"),
        Kind::Http(flavor) => {
            let (method, path, body) = http_request(source, flavor, &c.database)?;
            let body = body.map(|b| format!(", {}", py_value(&b))).unwrap_or_default();
            format!("{var} = http_frame({client}, {}, {}{body})", py_lit(&method), py_lit(&path))
        }
        Kind::Mongo => match mongo_call(source).ok_or("only `db.<collection>.find(…)` and `.aggregate([…])` with strict JSON arguments are mapped")? {
            MongoCall::Find { coll, args, chain } => {
                let a: Vec<String> = args.iter().map(|v| mongo_arg(v, imports)).collect();
                let mut call = format!("{client}[{}].find({})", py_lit(&coll), a.join(", "));
                for (m, v) in chain {
                    match m.as_str() {
                        "sort" => {
                            let pairs: Vec<String> = v.as_object().map(|o| o.iter().map(|(k, d)| format!("({}, {})", py_lit(k), py_value(d))).collect()).unwrap_or_default();
                            let _ = write!(call, ".sort([{}])", pairs.join(", "));
                        }
                        _ => {
                            let _ = write!(call, ".{m}({v})");
                        }
                    }
                }
                format!("{var} = pd.DataFrame(list({call}))")
            }
            MongoCall::Aggregate { coll, pipeline } => format!("{var} = pd.DataFrame(list({client}[{}].aggregate({})))", py_lit(&coll), mongo_arg(&pipeline, imports)),
        },
        Kind::Cosmos => return Err(format!("Cosmos DB queries run on one container: `{client}.get_container_client(\"<container>\").query_items(<query>, enable_cross_partition_query=True)`")),
        Kind::None => return Err(String::new()),
    })
}

/// `:name` is used as a bind parameter in `sql`.
fn bind_used(sql: &str, name: &str) -> bool {
    let all: HashSet<String> = HashSet::new();
    // Escaping everything but `name` changes the text only where it isn't `name`;
    // escaping everything escapes `name` too where it's a bind.
    let only = escape_binds(sql, &HashSet::from([name.to_string()]));
    only != escape_binds(sql, &all)
}

/// A DataFrame SQL cell: DuckDB over the DataFrames above (by name).
fn dataframe_code(
    c: &NotebookCell,
    var: &str,
    renamed: &HashMap<String, String>,
    known: &[String],
) -> String {
    let mut lines = Vec::new();
    for r in local::references(&c.source, known) {
        if let Some(v) = renamed.get(&r.to_ascii_lowercase()) {
            lines.push(format!("duckdb.register({}, {v})", py_lit(&r)));
        }
    }
    lines.push(format!("{var} = duckdb.sql({}).df()", py_block(&c.source)));
    lines.join("\n")
}

/// A chart of the stored settings as a pandas plot, when it's a plain one.
fn chart_code(chart: &Value, var: &str) -> Vec<String> {
    let list: Vec<&Value> = match chart.get("charts").and_then(Value::as_array) {
        Some(a) => a.iter().collect(),
        None if chart.get("type").is_some() => vec![chart],
        None => vec![],
    };
    let mut out = Vec::new();
    for ch in list {
        let kind = ch.get("type").and_then(Value::as_str).unwrap_or("");
        let x = ch.get("x").and_then(Value::as_str).unwrap_or("");
        let ys: Vec<&str> = ch
            .get("y")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let split = ch
            .get("split")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let agg = ch.get("agg").and_then(Value::as_str).unwrap_or("none");
        let title = ch.get("title").and_then(Value::as_str).unwrap_or("");
        let pkind = match kind {
            "bar" => "bar",
            "hbar" => "barh",
            "line" => "line",
            "area" => "area",
            "scatter" => "scatter",
            "pie" => "pie",
            _ => continue,
        };
        if x.is_empty() || ys.is_empty() || split.is_some() {
            continue;
        }
        let title_arg = if title.is_empty() {
            String::new()
        } else {
            format!(", title={}", py_lit(title))
        };
        let ylist = format!(
            "[{}]",
            ys.iter().map(|y| py_lit(y)).collect::<Vec<_>>().join(", ")
        );
        let code = if pkind == "scatter" {
            format!(
                "{var}.plot(kind=\"scatter\", x={}, y={}{title_arg})",
                py_lit(x),
                py_lit(ys[0])
            )
        } else {
            let method = match agg {
                "none" => None,
                "sum" => Some("sum"),
                "avg" => Some("mean"),
                "count" => Some("count"),
                "min" => Some("min"),
                "max" => Some("max"),
                _ => continue,
            };
            let sort = match ch.get("sort").and_then(Value::as_str) {
                Some("value") => format!(".sort_values({}, ascending=False)", py_lit(ys[0])),
                Some("label") => ".sort_index()".into(),
                _ => String::new(),
            };
            let frame = match method {
                None => format!("{var}.set_index({})[{ylist}]", py_lit(x)),
                Some(m) => format!("{var}.groupby({})[{ylist}].{m}(){sort}", py_lit(x)),
            };
            if pkind == "pie" {
                format!(
                    "{frame}.plot(kind=\"pie\", y={}, legend=False{title_arg})",
                    py_lit(ys[0])
                )
            } else {
                format!("{frame}.plot(kind={}{title_arg})", py_lit(pkind))
            }
        };
        out.push(code);
    }
    out
}

// ---------------------------------------------------------------------------
// The notebook.

/// How to export.
#[derive(Debug, Clone, Default)]
pub struct PyExport {
    /// Each cell's stored preview as its output ("Incluir vistas previas").
    pub previews: bool,
    pub limits: PreviewLimits,
    /// The notebook's parameters (name, default), papermill-style: a first
    /// cell tagged `parameters`, bound in the SQLAlchemy cells as `:name`.
    pub parameters: Vec<(String, Value)>,
}

/// The notebook's parameters (name, default; null without one), for
/// [`PyExport::parameters`]: the cells' `{{name}}` become `:name` binds. A
/// credential's or a connection's details default is never written, as in
/// the notebook's own file ([`super::params::parameters_json`]).
pub fn parameters(nb: &Notebook) -> Vec<(String, Value)> {
    nb.parameters
        .iter()
        .map(|p| {
            let default = if super::params::sensitive_default(p) {
                None
            } else {
                p.default.clone()
            };
            (p.name.clone(), default.unwrap_or(Value::Null))
        })
        .collect()
}

/// A line of the `parameters` cell: a credential without a value says
/// where to take it from.
fn parameter_line(name: &str, v: &Value) -> String {
    let line = format!("{name} = {}", py_value(v));
    if v.is_null() && super::params::secret_name(name) {
        format!("{line}  # a credential: DBine doesn't write it; set it from an environment variable, os.environ[{}]", py_lit(&name.to_uppercase()))
    } else {
        line
    }
}

fn empty_target(t: Option<&NotebookTarget>) -> bool {
    t.is_none_or(|t| {
        !t.results
            && t.connection.as_deref().unwrap_or("").is_empty()
            && t.environment.as_deref().unwrap_or("").is_empty()
    })
}

/// The cell runs on DataFrames (its own target, else the notebook's).
fn is_dataframe_sql(c: &NotebookCell, nb: &Notebook) -> bool {
    let t = if !empty_target(c.target.as_ref()) {
        c.target.as_ref()
    } else {
        nb.target.as_ref()
    };
    t.is_some_and(|t| t.results)
}

fn stored_target_name(c: &NotebookCell, nb: &Notebook) -> Option<String> {
    let t = if !empty_target(c.target.as_ref()) {
        c.target.as_ref()
    } else {
        nb.target.as_ref()
    }?;
    t.environment
        .clone()
        .filter(|e| !e.is_empty())
        .or_else(|| t.connection.clone())
        .filter(|n| !n.is_empty())
}

struct Cells {
    out: Vec<Value>,
    ids: HashSet<String>,
}

impl Cells {
    fn id(&mut self, want: &str) -> String {
        let ok = !want.is_empty()
            && want.len() <= 64
            && want
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        let mut id = if ok {
            want.to_string()
        } else {
            format!("cell-{}", self.out.len() + 1)
        };
        let mut n = 2;
        while self.ids.contains(&id) {
            id = format!("cell-{}-{n}", self.out.len() + 1);
            n += 1;
        }
        self.ids.insert(id.clone());
        id
    }

    fn markdown(&mut self, want: &str, source: &str, attachments: Option<&Value>) {
        let id = self.id(want);
        let mut cell = json!({ "cell_type": "markdown", "id": id, "metadata": {}, "source": super::lines(source.trim_end()) });
        if let Some(a) = attachments.filter(|a| a.is_object()) {
            cell["attachments"] = a.clone();
        }
        self.out.push(cell);
    }

    fn code(
        &mut self,
        want: &str,
        source: &str,
        metadata: Value,
        outputs: Vec<Value>,
        execution_count: Option<u32>,
    ) {
        let id = self.id(want);
        self.out.push(json!({
            "cell_type": "code",
            "id": id,
            "metadata": metadata,
            "execution_count": execution_count,
            "outputs": outputs,
            "source": super::lines(source.trim_end()),
        }));
    }
}

/// The stored preview of a cell as Jupyter outputs (no DBine metadata).
fn preview_outputs(c: &NotebookCell, nb: &Notebook, opts: &PyExport) -> (Vec<Value>, Option<u32>) {
    let Some(r) = c.result.as_ref().filter(|_| opts.previews) else {
        return (Vec::new(), None);
    };
    let keep = nb.save_results && c.keep_result;
    let p = preview(r, keep, opts.limits, c.execution_count);
    let mut outs = result_outputs(&p, r, keep, c.execution_count);
    for o in &mut outs {
        if let Some(m) = o.get_mut("metadata") {
            *m = json!({});
        }
    }
    (outs, c.execution_count)
}

fn notebook_title(nb: &Notebook) -> String {
    nb.cells
        .iter()
        .filter(|c| c.kind == CellKind::Markdown)
        .flat_map(|c| c.source.lines())
        .find_map(|l| {
            l.trim_start()
                .strip_prefix('#')
                .map(|h| h.trim_start_matches('#').trim().to_string())
        })
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "Notebook".into())
}

/// The notebook as a Python notebook (nbformat 4.5 JSON). `conns`: each SQL
/// cell's connection by cell id (the UI resolves the targets); a cell
/// without one is exported commented.
pub fn to_value(nb: &Notebook, conns: &HashMap<String, PyConn>, opts: &PyExport) -> Value {
    // Variables: each SQL cell's DataFrame, then each connection's client.
    let mut used: HashSet<String> = RESERVED_NAMES.iter().map(|s| s.to_string()).collect();
    let mut cell_var: HashMap<String, String> = HashMap::new();
    let mut renamed: HashMap<String, String> = HashMap::new();
    let mut known: Vec<String> = Vec::new();
    let mut sql_n = 0;
    for p in &opts.parameters {
        used.insert(p.0.clone());
    }
    for c in nb.cells.iter().filter(|c| c.kind == CellKind::Sql) {
        sql_n += 1;
        let name = c
            .name
            .clone()
            .filter(|n| local::valid_name(n))
            .unwrap_or_else(|| format!("c{sql_n}"));
        let mut var = name.clone();
        if PY_KEYWORDS.contains(&var.as_str()) || used.contains(&var) {
            var = format!("{name}_df");
            let mut n = 2;
            while used.contains(&var) {
                var = format!("{name}_df{n}");
                n += 1;
            }
            renamed.insert(name.to_ascii_lowercase(), var.clone());
        }
        used.insert(var.clone());
        known.push(name);
        cell_var.insert(c.id.clone(), var);
    }

    // Connections, in the order the cells use them.
    let mut order: Vec<&PyConn> = Vec::new();
    for c in &nb.cells {
        if c.kind == CellKind::Sql && !is_dataframe_sql(c, nb) {
            if let Some(pc) = conns.get(&c.id) {
                if !order.iter().any(|o| o.key == pc.key) {
                    order.push(pc);
                }
            }
        }
    }
    let mut client_var: HashMap<String, (String, Kind)> = HashMap::new();
    let mut setups: Vec<(&PyConn, String, Setup)> = Vec::new();
    for pc in &order {
        let several_dbs = order.iter().filter(|o| o.name == pc.name).count() > 1;
        let probe = setup_for(pc, "_");
        let prefix = match probe.kind {
            Kind::Sa => "engine",
            Kind::Dbapi => "odbc",
            Kind::DuckDb => "duck",
            Kind::Mongo => "mongo",
            Kind::Redis => "rds",
            Kind::Cassandra => "cql",
            Kind::Neo4j => "graph",
            Kind::ClickHouse => "ch",
            Kind::Influx1 | Kind::Influx2 | Kind::Influx3 => "influx",
            Kind::Http(_) => "http",
            Kind::Dynamo => "dynamo",
            Kind::Couchbase => "cb",
            Kind::Flight => "flight",
            Kind::Cosmos => "cosmos",
            Kind::None => "conn",
        };
        let base = if several_dbs && !pc.database.is_empty() {
            format!("{prefix}_{}_{}", slug(&pc.name), slug(&pc.database))
        } else {
            format!("{prefix}_{}", slug(&pc.name))
        };
        let mut var = base.clone();
        let mut n = 2;
        while used.contains(&var) {
            var = format!("{base}_{n}");
            n += 1;
        }
        used.insert(var.clone());
        let setup = setup_for(pc, &var);
        client_var.insert(pc.key.clone(), (var.clone(), setup.kind));
        setups.push((pc, var, setup));
    }

    // Every cell's code first: what it uses decides the imports and packages.
    let mut imports: BTreeSet<&'static str> = BTreeSet::from(["import pandas as pd"]);
    let mut packages: Vec<&'static str> = vec!["pandas"];
    let mut helpers: Vec<&'static str> = Vec::new();
    let mut duck_ext: BTreeSet<&'static str> = BTreeSet::new();
    let mut charts = false;
    let mut notes: Vec<String> = Vec::new();
    let mut needs_duckdb_15 = false;
    let mut body = Cells {
        out: Vec::new(),
        ids: HashSet::new(),
    };
    let mut setup_cells = Cells {
        out: Vec::new(),
        ids: HashSet::new(),
    };
    for c in &nb.cells {
        match c.kind {
            CellKind::Markdown => {
                let att = c.raw.as_ref().and_then(|r| r.get("attachments"));
                body.markdown(&c.id, &c.source, att);
            }
            CellKind::Foreign => {
                let lang = c.language.as_deref().unwrap_or("");
                if lang.eq_ignore_ascii_case("python") {
                    body.code(&c.id, &c.source, json!({}), Vec::new(), None);
                } else {
                    let label = if lang.is_empty() {
                        "A cell of the original notebook".to_string()
                    } else {
                        format!("A {lang} cell of the original notebook")
                    };
                    body.code(
                        &c.id,
                        &format!(
                            "{}\n{}",
                            comment_line(&format!("{label}: kept as a comment.")),
                            commented(&c.source)
                        ),
                        json!({}),
                        Vec::new(),
                        None,
                    );
                }
            }
            CellKind::Sql => {
                let var = cell_var.get(&c.id).cloned().unwrap_or_else(|| "df".into());
                let (outputs, count) = preview_outputs(c, nb, opts);
                let mut working = false;
                let code = if is_dataframe_sql(c, nb) {
                    imports.insert("import duckdb");
                    if !packages.contains(&"duckdb") {
                        packages.push("duckdb");
                    }
                    let l = c.source.to_ascii_lowercase();
                    if l.contains("st_read") {
                        duck_ext.insert("spatial");
                    }
                    if l.contains("read_xlsx") {
                        duck_ext.insert("excel");
                    }
                    if l.contains("sqlite_scan") {
                        duck_ext.insert("sqlite");
                    }
                    if l.contains("read_duckdb") {
                        needs_duckdb_15 = true;
                    }
                    working = true;
                    format!("{}\n{var}", dataframe_code(c, &var, &renamed, &known))
                } else {
                    match conns
                        .get(&c.id)
                        .and_then(|pc| client_var.get(&pc.key).map(|cv| (pc, cv)))
                    {
                        None => {
                            let name = stored_target_name(c, nb).unwrap_or_else(|| "(none)".into());
                            format!(
                                "{}\n{}\n{}",
                                comment_line(&format!("TODO: this cell runs on «{name}», which isn't set up in DBine on this computer: connect it there and export again.")),
                                comment_line(&format!("{var} = …")),
                                commented(&c.source)
                            )
                        }
                        Some((pc, (client, kind))) => match query_code(
                            *kind,
                            client,
                            &var,
                            pc,
                            &c.source,
                            &opts.parameters,
                            &mut imports,
                        ) {
                            Ok(code) => {
                                working = true;
                                format!("{code}\n{var}")
                            }
                            Err(why) => {
                                let why = if why.is_empty() {
                                    setups
                                        .iter()
                                        .find(|(p, _, _)| p.key == pc.key)
                                        .and_then(|(_, _, s)| s.reason.clone())
                                        .unwrap_or_default()
                                } else {
                                    why
                                };
                                format!(
                                    "{}\n{}\n{}",
                                    comment_line(&format!(
                                        "TODO: not exported as Python ({}): {why}.",
                                        pc.name
                                    )),
                                    comment_line(&format!("{var} = …")),
                                    commented(&c.source)
                                )
                            }
                        },
                    }
                };
                body.code(&c.id, &code, json!({}), outputs, count);
                if working {
                    if let Some(chart) = &c.chart {
                        let plots = chart_code(chart, &var);
                        if !plots.is_empty() {
                            charts = true;
                            body.code("", &plots.join("\n"), json!({}), Vec::new(), None);
                        }
                    }
                }
            }
            // A write cell: a commented `df.to_sql(…)` with a warning, never
            // executable by default (it would write to the database).
            CellKind::Write => {
                let w = c.write.clone().unwrap_or_default();
                let src = w
                    .source
                    .as_deref()
                    .and_then(|n| {
                        nb.cells.iter().find(|x| {
                            x.kind == CellKind::Sql
                                && x.name.as_deref().is_some_and(|m| m.eq_ignore_ascii_case(n))
                        })
                    })
                    .and_then(|x| cell_var.get(&x.id).cloned())
                    .unwrap_or_else(|| "df".into());
                let table = w.table.clone().unwrap_or_default();
                let schema = w
                    .schema
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(|s| format!(", schema={s:?}"))
                    .unwrap_or_default();
                let (if_exists, note) = match w.mode {
                    dbine_driver::WriteMode::Insert => ("append", None),
                    dbine_driver::WriteMode::Replace => ("append", Some("DBine's «replace» deletes the table's rows first (pandas' if_exists=\"replace\" would drop the table): run a DELETE before it.")),
                    dbine_driver::WriteMode::Update | dbine_driver::WriteMode::Upsert => {
                        ("append", Some("DBine updates by key here; pandas' to_sql only appends: write to a staging table and merge it with SQL."))
                    }
                };
                let mut lines = vec![comment_line(&format!(
                    "A DBine write cell: it writes «{}» into «{}». Not run in this export: uncomment it only if you mean to write.",
                    w.source.as_deref().unwrap_or("?"),
                    [w.schema.clone(), w.table.clone()].into_iter().flatten().collect::<Vec<_>>().join(".")
                ))];
                if let Some(n) = note {
                    lines.push(comment_line(n));
                }
                lines.push(commented(&format!(
                    "{src}.to_sql({table:?}, engine{schema}, if_exists={if_exists:?}, index=False)"
                )));
                body.code(&c.id, &lines.join("\n"), json!({}), Vec::new(), None);
                notes.push(format!("A write cell into «{table}» is exported as a comment: it doesn't write unless you uncomment it."));
            }
        }
    }

    let mut env: Vec<(String, String)> = Vec::new();
    let mut setup_code: Vec<String> = Vec::new();
    for (pc, _var, s) in &setups {
        for i in &s.imports {
            imports.insert(i);
        }
        for p in &s.packages {
            if !packages.contains(p) {
                packages.push(p);
            }
        }
        for h in &s.helpers {
            if !helpers.contains(h) {
                helpers.push(h);
            }
        }
        for e in &s.env {
            if !env.iter().any(|(n, _)| *n == e.0) {
                env.push(e.clone());
            }
        }
        notes.extend(s.notes.iter().cloned());
        setup_code.push(comment_line(&format!(
            "«{}»{}",
            pc.name,
            if pc.database.is_empty() {
                String::new()
            } else {
                format!(" › {}", pc.database)
            }
        )));
        match s.kind {
            Kind::None => {
                setup_code.push(comment_line(&format!(
                    "Not exported: {}.",
                    s.reason.as_deref().unwrap_or("")
                )));
                notes.push(format!(
                    "«{}»: its cells are exported as comments ({}).",
                    pc.name,
                    s.reason.as_deref().unwrap_or("")
                ));
            }
            _ => setup_code.extend(s.code.iter().cloned()),
        }
        setup_code.push(String::new());
    }
    if setups.iter().any(|(_, _, s)| s.kind == Kind::Sa) {
        packages.insert(1, "sqlalchemy");
    }
    if charts {
        packages.push("matplotlib");
    }
    for ext in &duck_ext {
        setup_code.push(format!("duckdb.sql(\"INSTALL {ext}; LOAD {ext}\")"));
    }

    // The instructions.
    let title = notebook_title(nb);
    let mut intro = format!(
        "# {title} (Python)\n\nExported from DBine. Each query cell reads its result into a pandas DataFrame named after the cell; \
         each DataFrame SQL cell runs on DuckDB over those DataFrames (by name) and the files it reads.\n\n**Before running it**\n\n1. Install the packages:\n\n   ```\n   pip install {}\n   ```\n",
        packages.iter().map(|p| if p.contains('[') { format!("\"{p}\"") } else { p.to_string() }).collect::<Vec<_>>().join(" ")
    );
    if needs_duckdb_15 {
        intro.push_str("\n   `read_duckdb` needs DuckDB 1.5 or later.\n");
    }
    if env.is_empty() {
        intro.push_str("\n2. No passwords are needed.\n");
    } else {
        intro.push_str("\n2. Set these environment variables (DBine never writes passwords or tokens into the file):\n\n");
        for (n, d) in &env {
            let _ = writeln!(intro, "   - `{n}`: {d}");
        }
    }
    if !duck_ext.is_empty()
        || nb
            .cells
            .iter()
            .any(|c| c.kind == CellKind::Sql && is_dataframe_sql(c, nb))
    {
        intro.push_str("\nRelative paths in `read_csv` and the other readers are read from the folder the notebook runs in (DBine reads them from the notebook's folder): keep this notebook where the original is.\n");
    }
    if !notes.is_empty() {
        intro.push_str("\n**Notes**\n\n");
        for n in &notes {
            let _ = writeln!(intro, "- {n}");
        }
    }
    setup_cells.markdown("dbine-python-setup-notes", &intro, None);
    if !opts.parameters.is_empty() {
        let lines: Vec<String> = opts
            .parameters
            .iter()
            .map(|(n, v)| parameter_line(n, v))
            .collect();
        setup_cells.code(
            "dbine-python-parameters",
            &lines.join("\n"),
            json!({ "tags": ["parameters"] }),
            Vec::new(),
            None,
        );
    }
    let mut first = imports
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    first.push_str("\n\n");
    for h in &helpers {
        first.push_str(h);
        first.push_str("\n\n\n");
    }
    first.push_str(&setup_code.join("\n"));
    setup_cells.code(
        "dbine-python-setup",
        first.trim_end(),
        json!({}),
        Vec::new(),
        None,
    );

    // Body ids mustn't clash with the setup cells'.
    let mut cells = setup_cells.out;
    let taken: HashSet<String> = cells
        .iter()
        .filter_map(|c| c["id"].as_str().map(String::from))
        .collect();
    for (i, mut c) in body.out.into_iter().enumerate() {
        if c["id"].as_str().is_some_and(|id| taken.contains(id)) {
            c["id"] = json!(format!("cell-body-{}", i + 1));
        }
        cells.push(c);
    }
    let mut meta = Map::new();
    meta.insert(
        "kernelspec".into(),
        json!({ "name": "python3", "display_name": "Python 3", "language": "python" }),
    );
    meta.insert("language_info".into(), json!({ "name": "python" }));
    json!({ "cells": cells, "metadata": meta, "nbformat": 4, "nbformat_minor": 5 })
}

/// [`to_value`] as `.ipynb` text (1-space indent like Jupyter).
pub fn python(nb: &Notebook, conns: &HashMap<String, PyConn>, opts: &PyExport) -> String {
    use serde::Serialize;
    let v = to_value(nb, conns, opts);
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(
        &mut buf,
        serde_json::ser::PrettyFormatter::with_indent(b" "),
    );
    let _ = v.serialize(&mut ser);
    let mut s = String::from_utf8(buf).unwrap_or_default();
    s.push('\n');
    s
}

/// Every driver's mapping, for the docs and the parity test: (driver id,
/// "working" or the reason its cells are commented).
#[cfg(test)]
pub fn mapping_of(c: &PyConn) -> (String, Option<String>) {
    let s = setup_for(c, "x");
    if s.kind == Kind::Sa {
        let driver = s
            .code
            .get(2)
            .map(|l| l.trim().trim_end_matches(',').trim_matches('"').to_string())
            .unwrap_or_default();
        return (
            format!("sqlalchemy {driver} ({})", s.packages.join(", ")),
            s.reason,
        );
    }
    let what = match s.kind {
        Kind::Sa => "sqlalchemy",
        Kind::Dbapi => "pyodbc",
        Kind::DuckDb => "duckdb",
        Kind::ClickHouse => "clickhouse-connect",
        Kind::Mongo => "pymongo",
        Kind::Redis => "redis",
        Kind::Cassandra => "cassandra-driver",
        Kind::Neo4j => "neo4j",
        Kind::Influx1 => "influxdb",
        Kind::Influx2 => "influxdb-client",
        Kind::Influx3 => "influxdb3-python",
        Kind::Http(_) => "requests",
        Kind::Dynamo => "boto3",
        Kind::Couchbase => "couchbase",
        Kind::Flight => "adbc-driver-flightsql",
        Kind::Cosmos => "azure-cosmos (commented cells)",
        Kind::None => "none",
    };
    (what.to_string(), s.reason)
}

#[cfg(test)]
#[path = "python_tests.rs"]
mod tests;
