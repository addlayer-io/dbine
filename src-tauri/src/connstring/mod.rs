//! "Copiar cadena de conexión…" (docs/connection-strings.md): a saved
//! connection written as the connection string an application uses — a
//! URI, ADO.NET, JDBC, SQLAlchemy, ODBC, libpq, Node.js or Go — for the
//! formats that make sense for its engine.
//!
//! Everything here is pure: it takes the connection's non-secret fields
//! ([`Conn`]) and what to put where a secret goes ([`Fill`]): a placeholder
//! (`${DB_PASSWORD}`), a mask for the preview, or the resolved values when
//! the user copies with "Incluir contraseña". The command
//! (commands/connstring.rs) resolves secrets only for that last case.
//!
//! The SQLAlchemy URL is the one the Python notebook export builds
//! (notebook::python::sqlalchemy_url), rendered as a string.

use crate::notebook::python::{self, PyConn, SaUrl};
use dbine_driver::{ConnectionConfig, DriverInfo};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// What the preview shows where a secret goes until "Mostrar".
pub const MASK: &str = "••••••••";

/// Option keys that hold a secret on some engine: never read from the
/// saved options for a [`Conn`], whatever the driver declares.
const SECRET_KEYS: &[&str] = &[
    "password",
    "token",
    "client_secret",
    "account_key",
    "api_key",
    "auth_token",
    "secret_access_key",
    "session_token",
    "wallet_password",
    "connection_string",
    "access_token",
    "service_account_json",
    "private_key",
    "extra",
    "headers",
];

/// A connection's non-secret fields, as the formats need them.
#[derive(Debug, Clone)]
pub struct Conn {
    pub driver: String,
    pub dialect: String,
    /// Without user info when it's a URL.
    pub host: String,
    /// The saved port, else the driver's default (0 when it has none).
    pub port: u16,
    pub database: String,
    pub user: String,
    pub encrypt: bool,
    pub trust: bool,
    pub read_only: bool,
    /// Only options that aren't secret.
    pub opts: BTreeMap<String, String>,
    /// The sign-in method (`auth` / `auth_mode`), if any.
    pub auth: Option<String>,
    /// The password is a cloud IAM token DBine generates when it connects.
    pub iam: bool,
    /// The Python export's view (for the SQLAlchemy URL).
    pub py: PyConn,
}

impl Conn {
    /// `database` empty = the connection's own.
    pub fn new(name: &str, cfg: &ConnectionConfig, info: Option<&DriverInfo>, database: &str) -> Conn {
        let secret: Vec<&str> = info.map(|i| i.fields.iter().filter(|f| f.secret).map(|f| f.key).collect()).unwrap_or_default();
        let mut opts: BTreeMap<String, String> = cfg
            .options
            .iter()
            .filter(|(k, v)| !v.trim().is_empty() && !k.starts_with("ssh.") && k.as_str() != dbine_core::secret_sources::OPTION)
            .filter(|(k, _)| !SECRET_KEYS.contains(&k.as_str()) && !secret.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.trim().to_string()))
            .collect();
        // A field left at its default isn't saved: the driver's default.
        if let Some(i) = info {
            for f in i.fields.iter().filter(|f| !f.secret && !f.default.is_empty() && !SECRET_KEYS.contains(&f.key)) {
                if matches!(f.key, "odbc_driver" | "auth" | "auth_mode" | "connect_by" | "adb_service" | "serialization") {
                    opts.entry(f.key.to_string()).or_insert_with(|| f.default.to_string());
                }
            }
        }
        let auth = opts.get("auth").or(opts.get("auth_mode")).cloned();
        let iam = matches!(auth.as_deref(), Some("aws_iam" | "gcp_iam" | "azure_entra")) || cfg.driver == "dsql" || (cfg.driver == "neptune" && opts.get("iam").map(String::as_str) == Some("true"));
        let database = if database.is_empty() { cfg.database.clone() } else { database.to_string() };
        Conn {
            driver: cfg.driver.clone(),
            dialect: info.map(|i| i.dialect.to_string()).unwrap_or_default(),
            host: python::strip_url_credentials(&cfg.host),
            port: if cfg.port != 0 { cfg.port } else { info.map(|i| i.default_port).unwrap_or(0) },
            database: database.clone(),
            user: cfg.username_or_empty().trim().to_string(),
            encrypt: cfg.encrypt,
            trust: cfg.trust_server_certificate,
            read_only: cfg.read_only,
            opts,
            auth,
            iam,
            py: PyConn::new("", name, cfg, info, &database),
        }
    }

    fn opt(&self, k: &str) -> Option<&str> {
        self.opts.get(k).map(String::as_str).filter(|v| !v.is_empty())
    }

    fn host_or_localhost(&self) -> &str {
        if self.host.is_empty() {
            "localhost"
        } else {
            &self.host
        }
    }

    /// `host:port` (IPv6 in brackets), or the host alone without a port.
    fn hp(&self) -> String {
        hp(self.host_or_localhost(), self.port)
    }
}

fn hp(host: &str, port: u16) -> String {
    let h = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
    if port == 0 {
        h
    } else {
        format!("{h}:{port}")
    }
}

/// What goes where a secret goes.
pub enum Fill<'a> {
    /// `${DB_PASSWORD}`…, for the user to replace.
    Placeholder,
    /// [`MASK`]: the preview, built without the secret.
    Masked,
    /// The resolved secrets (`password`, and the driver's secret options by key).
    Values(&'a BTreeMap<String, String>),
}

/// A line under the string, translated by the UI (`connString:note.<code>`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Note {
    pub code: &'static str,
    pub args: BTreeMap<String, String>,
}

pub fn note(code: &'static str, args: &[(&str, &str)]) -> Note {
    Note { code, args: args.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }
}

/// One format of a connection.
#[derive(Debug, Clone, Serialize)]
pub struct Built {
    /// `uri`, `adonet`, `jdbc`, `sqlalchemy`, `odbc`, `libpq`, `node`, `go`…
    pub id: &'static str,
    /// The format and its client library ("ADO.NET (Npgsql)"); empty: the
    /// UI names it (`connString:format.<id>`).
    pub label: String,
    pub text: String,
    pub notes: Vec<Note>,
    /// A resolved secret is in `text`.
    pub secret: bool,
}

/// A value: text to escape for the format, or raw (a placeholder, the mask).
#[derive(Debug, Clone)]
enum V {
    T(String),
    R(String),
}

impl V {
    fn t(s: impl Into<String>) -> V {
        V::T(s.into())
    }
    fn esc(&self, f: impl Fn(&str) -> String) -> String {
        match self {
            V::T(s) => f(s),
            V::R(r) => r.clone(),
        }
    }
    fn json(&self) -> Value {
        match self {
            V::T(s) | V::R(s) => Value::String(s.clone()),
        }
    }
}

// ---------------------------------------------------------------------------
// Escaping.

/// Percent-encoding of everything but the unreserved characters (RFC 3986).
pub fn pct(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A path: [`pct`] keeping `/`.
fn pct_path(s: &str) -> String {
    s.split('/').map(pct).collect::<Vec<_>>().join("/")
}

/// An ADO.NET value: quoted when it has `;`, `=`, quotes, braces or edge spaces.
pub fn ado_val(s: &str) -> String {
    let needs = s.contains([';', '=', '\'', '"', '{', '}']) || s.starts_with(char::is_whitespace) || s.ends_with(char::is_whitespace);
    if !needs {
        s.to_string()
    } else if s.contains('"') && !s.contains('\'') {
        format!("'{s}'")
    } else {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
}

/// An ODBC attribute value: in braces (`}` doubled) when it has a special character.
pub fn odbc_val(s: &str) -> String {
    if s.contains([';', '{', '}', '=']) || s.starts_with(' ') || s.ends_with(' ') {
        format!("{{{}}}", s.replace('}', "}}"))
    } else {
        s.to_string()
    }
}

/// A libpq keyword value: in single quotes (`\` and `'` escaped) when needed.
pub fn libpq_val(s: &str) -> String {
    if s.is_empty() || s.contains(|c: char| c.is_whitespace() || c == '\'' || c == '\\') {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    } else {
        s.to_string()
    }
}

/// A SQL Server JDBC property value: in braces (`}` doubled) when needed.
pub fn mssql_jdbc_val(s: &str) -> String {
    if s.contains([';', '=', '{', '}']) || s.starts_with(' ') || s.ends_with(' ') {
        format!("{{{}}}", s.replace('}', "}}"))
    } else {
        s.to_string()
    }
}

/// A Java properties value (`key=value` line).
pub fn prop_val(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        match c {
            '\\' => out.push_str("\\\\"),
            ':' | '=' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ' ' if i == 0 => out.push_str("\\ "),
            '#' | '!' if i == 0 => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// `key=value;` pairs, without the ones left out.
fn pairs(parts: &[(&str, Option<V>)], val: fn(&str) -> String) -> String {
    parts.iter().filter_map(|(k, v)| v.as_ref().map(|v| format!("{k}={};", v.esc(val)))).collect()
}

/// `?k=v&…` (percent-encoded), empty when there's nothing.
fn query(q: &[(&str, V)]) -> String {
    if q.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = q.iter().map(|(k, v)| format!("{k}={}", v.esc(pct))).collect();
    format!("?{}", parts.join("&"))
}

/// `user:password@` (percent-encoded), empty without either.
fn userinfo(user: &str, pw: Option<&V>) -> String {
    match (user.is_empty(), pw) {
        (true, None) => String::new(),
        (false, None) => format!("{}@", pct(user)),
        (_, Some(p)) => format!("{}:{}@", pct(user), p.esc(pct)),
    }
}

fn some_t(s: &str) -> Option<V> {
    (!s.is_empty()).then(|| V::t(s))
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// A JSON object without the null members.
fn obj(members: Vec<(&str, Value)>) -> Value {
    let mut m = Map::new();
    for (k, v) in members {
        if !v.is_null() {
            m.insert(k.to_string(), v);
        }
    }
    Value::Object(m)
}

fn jv(v: Option<V>) -> Value {
    v.map(|v| v.json()).unwrap_or(Value::Null)
}

fn js(s: &str) -> Value {
    if s.is_empty() {
        Value::Null
    } else {
        Value::String(s.to_string())
    }
}

// ---------------------------------------------------------------------------
// Building.

struct B<'a> {
    c: &'a Conn,
    fill: &'a Fill<'a>,
    out: Vec<Built>,
}

/// The placeholder for a secret.
fn placeholder(key: &str) -> String {
    match key {
        "secret_access_key" => "${AWS_SECRET_ACCESS_KEY}".into(),
        "session_token" => "${AWS_SESSION_TOKEN}".into(),
        k => format!("${{DB_{}}}", k.to_ascii_uppercase()),
    }
}

impl B<'_> {
    /// What goes where secret `key` goes; `None` when there is none to put.
    fn s(&self, key: &str) -> Option<V> {
        if key == "password" && self.c.iam {
            return Some(V::R("${DB_IAM_TOKEN}".into()));
        }
        match self.fill {
            Fill::Placeholder => Some(V::R(placeholder(key))),
            Fill::Masked => Some(V::R(MASK.into())),
            Fill::Values(m) => {
                m.get(key).filter(|v| !v.is_empty()).map(|v| V::t(v.clone()))
            }
        }
    }

    /// The password, when there's a user to go with it.
    fn pw(&self) -> Option<V> {
        if self.c.user.is_empty() {
            None
        } else {
            self.s("password")
        }
    }

    fn push(&mut self, id: &'static str, label: impl Into<String>, text: String, notes: Vec<Note>) {
        self.out.push(Built { id, label: label.into(), text, notes, secret: false });
    }

    /// The SQLAlchemy URL of the Python export, as a string. `extra`
    /// replaces or adds query parameters.
    fn sqlalchemy(&mut self, extra: &[(&str, V)], notes: Vec<Note>) {
        let Some(u) = python::sqlalchemy_url(&self.c.py) else { return };
        if let Some(text) = self.sa_text(&u, extra) {
            self.push("sqlalchemy", "Python (SQLAlchemy)", text, notes);
        }
    }

    /// A Python expression of the export as a value: a literal, a number,
    /// or the read of a secret's variable (its placeholder or value here).
    fn sa_val(&self, expr: &str) -> Option<V> {
        if let Some(s) = python::py_unlit(expr) {
            return Some(V::T(s));
        }
        if !expr.is_empty() && expr.bytes().all(|b| b.is_ascii_digit()) {
            return Some(V::T(expr.to_string()));
        }
        match expr {
            "True" => return Some(V::t("true")),
            "False" => return Some(V::t("false")),
            _ => {}
        }
        let inner = expr.strip_prefix("os.environ[").and_then(|e| e.strip_suffix(']')).or_else(|| expr.strip_prefix("os.environ.get(").and_then(|e| e.strip_suffix(')')))?;
        let name = python::py_unlit(inner)?;
        let what = name.strip_prefix(&python::env_var(&self.c.py.name, ""))?.to_ascii_lowercase();
        match what.as_str() {
            "user" => Some(V::R("${DB_USER}".into())),
            "private_key_file" => Some(V::R("${PRIVATE_KEY_FILE}".into())),
            "token" if self.c.driver == "dsql" => self.s("password"),
            w => self.s(w),
        }
    }

    fn sa_text(&self, u: &SaUrl, extra: &[(&str, V)]) -> Option<String> {
        let arg = |k: &str| u.args.iter().find(|(a, _)| a == k).and_then(|(_, v)| self.sa_val(v));
        let mut connect: Vec<(String, V)> = u.connect_args.iter().filter_map(|(k, v)| Some((k.clone(), if v == "{}" { V::t("{}") } else { self.sa_val(v)? }))).collect();
        let mut user = arg("username");
        let mut pw = arg("password");
        // python-oracledb's user and password given as connect arguments.
        if user.is_none() {
            if let Some(i) = connect.iter().position(|(k, _)| k == "user") {
                user = Some(connect.remove(i).1);
            }
        }
        if pw.is_none() {
            if let Some(i) = connect.iter().position(|(k, _)| k == "password") {
                pw = Some(connect.remove(i).1);
            }
        }
        let mut out = format!("{}://", u.drivername);
        match (&user, &pw) {
            (None, None) => {}
            (Some(u), None) => out.push_str(&format!("{}@", u.esc(pct))),
            (u, Some(p)) => out.push_str(&format!("{}:{}@", u.as_ref().map(|u| u.esc(pct)).unwrap_or_default(), p.esc(pct))),
        }
        if let Some(V::T(h) | V::R(h)) = arg("host") {
            out.push_str(&if h.contains(':') && !h.starts_with('[') { format!("[{h}]") } else { h });
        }
        if let Some(V::T(p) | V::R(p)) = arg("port") {
            out.push_str(&format!(":{p}"));
        }
        if let Some(V::T(d) | V::R(d)) = arg("database") {
            // SQLAlchemy reads the database as it is, up to `?`.
            out.push('/');
            out.push_str(&d.replace('?', "%3F").replace('#', "%23"));
        }
        let mut q: Vec<(String, V)> = u.query.iter().map(|(k, v)| (k.clone(), V::t(v.clone()))).collect();
        for (k, v) in connect {
            match (u.drivername.as_str(), k.as_str()) {
                // PyMySQL turns on TLS with any `ssl_*` parameter.
                (d, "ssl") if d.starts_with("mysql") => q.push(("ssl_check_hostname".into(), V::t(if self.c.trust { "false" } else { "true" }))),
                ("sqlite+libsql", "auth_token") => q.push(("authToken".into(), v)),
                _ => q.push((k, v)),
            }
        }
        for (k, v) in extra {
            q.retain(|(a, _)| a != k);
            q.push((k.to_string(), v.clone()));
        }
        if !q.is_empty() {
            let parts: Vec<String> = q.iter().map(|(k, v)| format!("{k}={}", v.esc(pct))).collect();
            out.push('?');
            out.push_str(&parts.join("&"));
        }
        Some(out)
    }
}

/// Every format of the connection, built with `fill`.
pub fn build_all(c: &Conn, fill: &Fill) -> Vec<Built> {
    let mut b = B { c, fill, out: Vec::new() };
    let id = c.driver.as_str();
    match id {
        "sqlite" => sqlite(&mut b),
        "libsql" => libsql(&mut b),
        "duckdb" | "duckdb_files" => duckdb(&mut b),
        "mongodb" | "documentdb" | "ferretdb" => mongo(&mut b),
        "redis" | "valkey" | "dragonfly" => redis(&mut b),
        "cassandra" | "scylladb" | "keyspaces" => cassandra(&mut b),
        "neo4j" | "memgraph" | "neptune" => neo4j(&mut b),
        "couchbase" => couchbase(&mut b),
        "clickhouse" | "timeplus" => clickhouse(&mut b),
        "cosmosdb" => cosmos(&mut b),
        "dynamodb" => dynamo(&mut b),
        "athena" => athena(&mut b),
        "bigquery" => bigquery(&mut b),
        "spanner" => spanner(&mut b),
        "snowflake" => snowflake(&mut b),
        "databricks" | "azure_databricks" => databricks(&mut b),
        "trino" | "starburst" | "presto" => trino(&mut b),
        "flightsql" | "dremio" => flight(&mut b),
        "drill" => drill(&mut b),
        "phoenix" | "avatica" => phoenix(&mut b),
        "hana" => hana(&mut b),
        "firebird" => firebird(&mut b),
        "tdengine" => tdengine(&mut b),
        "influxdb" | "influxdb1" | "influxdb3" => influx(&mut b),
        "oracle" | "oracle_adb" => oracle(&mut b),
        "elasticsearch" | "opensearch" | "opendistro" | "solr" | "couchdb" | "ksqldb" | "orientdb" | "etcd" | "iotdb" | "timechodb" => http(&mut b),
        _ if python::ODBC_IDS.contains(&id) => odbc_engine(&mut b),
        _ => match c.dialect.as_str() {
            "postgres" => postgres(&mut b),
            "mysql" => mysql(&mut b),
            "mssql" => mssql(&mut b),
            _ => {}
        },
    }
    b.out
}

/// One format, or `None` when the engine doesn't have it. With values,
/// `secret` says whether one of them is in the text (it differs from the
/// same string built without any).
pub fn build(c: &Conn, format: &str, fill: &Fill) -> Option<Built> {
    let mut out = build_all(c, fill).into_iter().find(|b| b.id == format)?;
    if let Fill::Values(_) = fill {
        let none = BTreeMap::new();
        let bare = build_all(c, &Fill::Values(&none)).into_iter().find(|b| b.id == format);
        out.secret = bare.is_none_or(|b| b.text != out.text);
    }
    Some(out)
}

/// The note under every JDBC URL that leaves the credentials out of it.
fn jdbc_props(url: String, user: &str, pw: Option<V>) -> String {
    let mut text = url;
    if !user.is_empty() {
        text.push_str(&format!("\nuser={}", prop_val(user)));
    }
    if let Some(p) = pw {
        text.push_str(&format!("\npassword={}", p.esc(prop_val)));
    }
    text
}

fn jdbc_notes(user: &str, pw: bool) -> Vec<Note> {
    if user.is_empty() && !pw {
        vec![]
    } else {
        vec![note("jdbc_properties", &[])]
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL and the engines that speak its protocol.

/// libpq's sslmode for the connection: DBine requires TLS with `encrypt`,
/// and checks the certificate unless "trust the server certificate".
fn pg_sslmode(c: &Conn) -> Option<&'static str> {
    match (c.encrypt || c.driver == "dsql", c.trust) {
        (false, _) => None,
        (true, true) => Some("require"),
        (true, false) => Some("verify-full"),
    }
}

fn postgres(b: &mut B) {
    let c = b.c;
    let dsql = c.driver == "dsql";
    let user = if dsql && c.user.is_empty() { "admin".to_string() } else { c.user.clone() };
    let db = if dsql { "postgres".to_string() } else { c.database.clone() };
    let pw = if user.is_empty() { None } else { b.s("password") };
    let ssl = pg_sslmode(c);
    let root = c.opt("ssl_root_cert").map(str::to_string);
    let mut notes = Vec::new();
    if c.trust && c.encrypt {
        notes.push(note("tls_unverified", &[]));
    }
    let mut q: Vec<(&str, V)> = Vec::new();
    if let Some(m) = ssl {
        q.push(("sslmode", V::t(m)));
    }
    if let Some(r) = &root {
        q.push(("sslrootcert", V::t(r.clone())));
    }
    let path = if db.is_empty() { String::new() } else { format!("/{}", pct(&db)) };
    let uri = format!("postgresql://{}{}{}{}", userinfo(&user, pw.as_ref()), c.hp(), path, query(&q));
    b.push("uri", "URI (postgresql://)", uri.clone(), notes.clone());

    // libpq keyword/value (psql, psycopg, any libpq client).
    let mut kv = vec![format!("host={}", libpq_val(c.host_or_localhost())), format!("port={}", c.port)];
    if !db.is_empty() {
        kv.push(format!("dbname={}", libpq_val(&db)));
    }
    if !user.is_empty() {
        kv.push(format!("user={}", libpq_val(&user)));
    }
    if let Some(p) = &pw {
        kv.push(format!("password={}", p.esc(libpq_val)));
    }
    if let Some(m) = ssl {
        kv.push(format!("sslmode={m}"));
    }
    if let Some(r) = &root {
        kv.push(format!("sslrootcert={}", libpq_val(r)));
    }
    b.push("libpq", "libpq (psql, psycopg)", kv.join(" "), notes.clone());

    // Npgsql.
    let npg_ssl = ssl.map(|m| if m == "require" { "Require" } else { "VerifyFull" });
    let ado = pairs(
        &[
            ("Host", Some(V::t(c.host_or_localhost()))),
            ("Port", Some(V::t(c.port.to_string()))),
            ("Database", some_t(&db)),
            ("Username", some_t(&user)),
            ("Password", pw.clone()),
            ("SSL Mode", npg_ssl.map(V::t)),
            ("Root Certificate", root.clone().map(V::t)),
        ],
        ado_val,
    );
    b.push("adonet", "ADO.NET (Npgsql)", ado, notes.clone());

    // JDBC: pgjdbc, or Amazon's for Redshift.
    let jdbc_scheme = if c.driver == "redshift" { "redshift" } else { "postgresql" };
    let mut jq: Vec<(&str, V)> = Vec::new();
    if !user.is_empty() {
        jq.push(("user", V::t(user.clone())));
    }
    if let Some(p) = pw.clone() {
        jq.push(("password", p));
    }
    if c.driver == "redshift" && ssl.is_some() {
        jq.push(("ssl", V::t("true")));
    }
    jq.extend(q.iter().cloned());
    b.push("jdbc", "JDBC", format!("jdbc:{jdbc_scheme}://{}/{}{}", c.hp(), pct(&db), query(&jq)), notes.clone());

    let sa_extra: Vec<(&str, V)> = q.clone();
    b.sqlalchemy(&sa_extra, notes.clone());

    // ODBC (psqlODBC, or Amazon's driver for Redshift).
    let driver = if c.driver == "redshift" { "Amazon Redshift (x64)" } else { "PostgreSQL Unicode" };
    let odbc = pairs(
        &[
            ("DRIVER", Some(V::R(format!("{{{driver}}}")))),
            ("SERVER", Some(V::t(c.host_or_localhost()))),
            ("PORT", Some(V::t(c.port.to_string()))),
            ("DATABASE", some_t(&db)),
            ("UID", some_t(&user)),
            ("PWD", pw.clone()),
            ("SSLmode", ssl.map(V::t)),
        ],
        odbc_val,
    );
    let mut on = notes.clone();
    on.push(note("odbc_driver_name", &[("driver", driver)]));
    b.push("odbc", "ODBC", odbc, on);
    b.push("node", "Node.js (pg)", uri.clone(), notes.clone());
    b.push("go", "Go (pgx)", uri.replacen("postgresql://", "postgres://", 1), notes);
}

// ---------------------------------------------------------------------------
// MySQL and compatibles.

fn mysql(b: &mut B) {
    let c = b.c;
    let pw = b.pw();
    let mariadb = c.driver == "mariadb";
    let tls = c.encrypt || c.iam;
    let ca = c.opt("ca_cert").map(str::to_string);
    let notes: Vec<Note> = if tls && c.trust { vec![note("tls_unverified", &[])] } else { vec![] };
    let path = if c.database.is_empty() { String::new() } else { format!("/{}", pct(&c.database)) };

    let mut q: Vec<(&str, V)> = Vec::new();
    if tls {
        q.push(("ssl-mode", V::t(if c.trust { "REQUIRED" } else { "VERIFY_IDENTITY" })));
    }
    if let Some(ca) = &ca {
        q.push(("ssl-ca", V::t(ca.clone())));
    }
    b.push("uri", "URI (mysql://)", format!("mysql://{}{}{}{}", userinfo(&c.user, pw.as_ref()), c.hp(), path, query(&q)), notes.clone());
    let ado = pairs(
        &[
            ("Server", Some(V::t(c.host_or_localhost()))),
            ("Port", Some(V::t(c.port.to_string()))),
            ("Database", some_t(&c.database)),
            ("User ID", some_t(&c.user)),
            ("Password", pw.clone()),
            ("SslMode", tls.then(|| V::t(if c.trust { "Required" } else { "VerifyFull" }))),
            ("SslCa", ca.clone().map(V::t)),
        ],
        ado_val,
    );
    b.push("adonet", "ADO.NET (MySqlConnector)", ado, notes.clone());
    let mut jq: Vec<(&str, V)> = Vec::new();
    if !c.user.is_empty() {
        jq.push(("user", V::t(c.user.clone())));
    }
    if let Some(p) = pw.clone() {
        jq.push(("password", p));
    }
    if tls {
        if mariadb {
            jq.push(("sslMode", V::t(if c.trust { "trust" } else { "verify-full" })));
        } else {
            jq.push(("sslMode", V::t(if c.trust { "REQUIRED" } else { "VERIFY_IDENTITY" })));
        }
    }
    let scheme = if mariadb { "mariadb" } else { "mysql" };
    b.push("jdbc", if mariadb { "JDBC (MariaDB Connector/J)" } else { "JDBC (Connector/J)" }, format!("jdbc:{scheme}://{}/{}{}", c.hp(), pct(&c.database), query(&jq)), notes.clone());
    b.sqlalchemy(&[], notes.clone());
    let odbc = pairs(
        &[
            ("DRIVER", Some(V::R("{MySQL ODBC 8.0 Unicode Driver}".into()))),
            ("SERVER", Some(V::t(c.host_or_localhost()))),
            ("PORT", Some(V::t(c.port.to_string()))),
            ("DATABASE", some_t(&c.database)),
            ("UID", some_t(&c.user)),
            ("PWD", pw.clone()),
            ("SSLMODE", tls.then(|| V::t(if c.trust { "REQUIRED" } else { "VERIFY_IDENTITY" }))),
            ("SSLCA", ca.clone().map(V::t)),
        ],
        odbc_val,
    );
    let mut on = notes.clone();
    on.push(note("odbc_driver_name", &[("driver", "MySQL ODBC 8.0 Unicode Driver")]));
    b.push("odbc", "ODBC", odbc, on);

    // mysql2 reads `ssl` as JSON.
    let nq: Vec<(&str, V)> = if tls { vec![("ssl", V::t(format!("{{\"rejectUnauthorized\":{}}}", !c.trust)))] } else { vec![] };
    b.push("node", "Node.js (mysql2)", format!("mysql://{}{}{}{}", userinfo(&c.user, pw.as_ref()), c.hp(), path, query(&nq)), notes.clone());

    // go-sql-driver/mysql: the password is taken as it is (no escaping).
    let creds = match (c.user.is_empty(), &pw) {
        (true, _) => String::new(),
        (false, None) => format!("{}@", c.user),
        (false, Some(p)) => format!("{}:{}@", c.user, p.esc(|s| s.to_string())),
    };
    let mut gq: Vec<String> = vec!["parseTime=true".into()];
    if tls {
        gq.insert(0, format!("tls={}", if c.trust { "skip-verify" } else { "true" }));
    }
    b.push("go", "Go (go-sql-driver/mysql)", format!("{creds}tcp({})/{}?{}", c.hp(), c.database, gq.join("&")), notes);
}

// ---------------------------------------------------------------------------
// SQL Server, Azure SQL, Fabric, Babelfish.

/// `host\INSTANCE` split, if any.
fn mssql_instance(host: &str) -> (&str, Option<&str>) {
    match host.split_once('\\') {
        Some((h, i)) if !i.is_empty() => (h, Some(i)),
        _ => (host, None),
    }
}

fn mssql(b: &mut B) {
    let c = b.c;
    let auth = c.auth.clone().unwrap_or_else(|| if c.driver == "fabric" { "entra_default".into() } else { "sql".into() });
    let forced = matches!(c.driver.as_str(), "azuresql" | "fabric") || auth == "entra_token";
    let encrypt = c.encrypt || forced;
    let (host, instance) = mssql_instance(c.host_or_localhost());
    let tenant = c.opt("tenant_id").map(str::to_string);
    let client = c.opt("client_id").map(str::to_string);
    let user_auth = matches!(auth.as_str(), "sql" | "entra_password" | "windows_ntlm");
    let user = match auth.as_str() {
        "entra_sp" => client.clone().unwrap_or_default(),
        "entra_msi" => client.clone().unwrap_or_default(),
        "windows_integrated" | "entra_default" | "entra_token" => String::new(),
        _ => c.user.clone(),
    };
    let secret = |b: &B| -> Option<V> {
        match auth.as_str() {
            "entra_sp" => b.s("client_secret"),
            _ if user_auth && !c.user.is_empty() => b.s("password"),
            _ => None,
        }
    };
    let mut notes = Vec::new();
    match auth.as_str() {
        "entra_token" => notes.push(note("access_token", &[])),
        "windows_integrated" => notes.push(note("integrated", &[])),
        "windows_ntlm" => notes.push(note("ntlm", &[])),
        a if a.starts_with("entra_") && a != "entra_password" && a != "entra_sp" => notes.push(note("entra", &[])),
        _ => {}
    }
    if encrypt && c.trust {
        notes.push(note("tls_unverified", &[]));
    }
    let ado_auth = match auth.as_str() {
        "entra_password" => Some("Active Directory Password"),
        "entra_mfa" => Some("Active Directory Interactive"),
        "entra_integrated" => Some("Active Directory Integrated"),
        "entra_sp" => Some("Active Directory Service Principal"),
        "entra_msi" => Some("Active Directory Managed Identity"),
        "entra_default" => Some("Active Directory Default"),
        _ => None,
    };
    let jdbc_auth = ado_auth.map(|a| a.replace(' ', ""));

    // Microsoft.Data.SqlClient.
    let server = match instance {
        Some(i) => format!("{host}\\{i}"),
        None => format!("tcp:{host},{}", c.port),
    };
    let pw = secret(b);
    let ado = pairs(
        &[
            ("Server", Some(V::t(server))),
            ("Database", some_t(&c.database)),
            ("Authentication", ado_auth.map(V::t)),
            ("Integrated Security", (auth == "windows_integrated").then(|| V::t("True"))),
            ("User ID", some_t(&user)),
            ("Password", pw.clone()),
            ("Encrypt", Some(V::t(if encrypt { "True" } else { "False" }))),
            ("TrustServerCertificate", (c.trust || !encrypt).then(|| V::t("True"))),
            ("ApplicationIntent", c.read_only.then(|| V::t("ReadOnly"))),
        ],
        ado_val,
    );
    b.push("adonet", "ADO.NET (Microsoft.Data.SqlClient)", ado, notes.clone());

    // Microsoft JDBC Driver.
    let pw = secret(b);
    let mut url = format!("jdbc:sqlserver://{host}");
    match instance {
        Some(i) => url.push_str(&format!(";instanceName={}", mssql_jdbc_val(i))),
        None => url.push_str(&format!(":{}", c.port)),
    }
    let mut props: Vec<(&str, Option<V>)> = vec![
        ("databaseName", some_t(&c.database)),
        ("encrypt", Some(V::t(if encrypt { "true" } else { "false" }))),
        ("trustServerCertificate", (c.trust || !encrypt).then(|| V::t("true"))),
        ("authentication", jdbc_auth.clone().map(V::t)),
        ("integratedSecurity", matches!(auth.as_str(), "windows_integrated" | "windows_ntlm").then(|| V::t("true"))),
        ("authenticationScheme", (auth == "windows_ntlm").then(|| V::t("NTLM"))),
        ("applicationIntent", c.read_only.then(|| V::t("ReadOnly"))),
        ("user", some_t(&user)),
        ("password", pw.clone()),
    ];
    if auth == "entra_msi" && !user.is_empty() {
        props.retain(|(k, _)| *k != "user");
        props.push(("msiClientId", some_t(&user)));
    }
    for (k, v) in props {
        if let Some(v) = v {
            url.push_str(&format!(";{k}={}", v.esc(mssql_jdbc_val)));
        }
    }
    b.push("jdbc", "JDBC (Microsoft JDBC Driver)", url, notes.clone());

    // ODBC Driver 18.
    let pw = secret(b);
    let odbc_auth = match auth.as_str() {
        "entra_password" => Some("ActiveDirectoryPassword"),
        "entra_mfa" => Some("ActiveDirectoryInteractive"),
        "entra_integrated" => Some("ActiveDirectoryIntegrated"),
        "entra_sp" => Some("ActiveDirectoryServicePrincipal"),
        "entra_msi" => Some("ActiveDirectoryMsi"),
        "entra_default" => Some("ActiveDirectoryDefault"),
        _ => None,
    };
    let odbc_server = match instance {
        Some(i) => format!("{host}\\{i}"),
        None => format!("tcp:{host},{}", c.port),
    };
    let odbc = pairs(
        &[
            ("DRIVER", Some(V::R("{ODBC Driver 18 for SQL Server}".into()))),
            ("SERVER", Some(V::t(odbc_server))),
            ("DATABASE", some_t(&c.database)),
            ("Authentication", odbc_auth.map(V::t)),
            ("Trusted_Connection", (auth == "windows_integrated").then(|| V::t("yes"))),
            ("UID", some_t(&user)),
            ("PWD", pw.clone()),
            ("Encrypt", Some(V::t(if encrypt { "yes" } else { "no" }))),
            ("TrustServerCertificate", (c.trust || !encrypt).then(|| V::t("yes"))),
            ("ApplicationIntent", c.read_only.then(|| V::t("ReadOnly"))),
        ],
        odbc_val,
    );
    let mut on = notes.clone();
    on.push(note("odbc_driver_name", &[("driver", "ODBC Driver 18 for SQL Server")]));
    b.push("odbc", "ODBC", odbc, on);

    // pymssql (the Python export's) has no Entra ID or Windows sign-in.
    if matches!(auth.as_str(), "sql") {
        b.sqlalchemy(&[], notes.clone());
    }

    // mssql / tedious: the config object.
    let pw = secret(b);
    let authentication = match auth.as_str() {
        "windows_ntlm" => {
            let (domain, name) = c.user.split_once('\\').map(|(d, u)| (d.to_string(), u.to_string())).unwrap_or((String::new(), c.user.clone()));
            Some(json!({"type": "ntlm", "options": obj(vec![("userName", js(&name)), ("password", jv(pw.clone())), ("domain", js(&domain))])}))
        }
        "entra_password" => Some(json!({"type": "azure-active-directory-password", "options": obj(vec![("userName", js(&c.user)), ("password", jv(pw.clone())), ("clientId", tenant.as_ref().and(client.as_deref()).map(js).unwrap_or(Value::Null)), ("tenantId", tenant.as_deref().map(js).unwrap_or(Value::Null))])})),
        "entra_sp" => Some(json!({"type": "azure-active-directory-service-principal-secret", "options": obj(vec![("clientId", js(&user)), ("clientSecret", jv(pw.clone())), ("tenantId", tenant.as_deref().map(js).unwrap_or(Value::Null))])})),
        "entra_msi" => Some(json!({"type": "azure-active-directory-msi-app-service", "options": obj(vec![("clientId", js(&user))])})),
        "entra_token" => Some(json!({"type": "azure-active-directory-access-token", "options": {"token": "${DB_ACCESS_TOKEN}"}})),
        "entra_default" | "entra_mfa" | "entra_integrated" => Some(json!({"type": "azure-active-directory-default", "options": obj(vec![("clientId", client.as_deref().map(js).unwrap_or(Value::Null)), ("tenantId", tenant.as_deref().map(js).unwrap_or(Value::Null))])})),
        _ => None,
    };
    let sql_login = auth == "sql";
    let cfg = obj(vec![
        ("server", js(host)),
        ("port", if instance.is_some() { Value::Null } else { json!(c.port) }),
        ("database", js(&c.database)),
        ("user", if sql_login { js(&c.user) } else { Value::Null }),
        ("password", if sql_login { jv(pw.clone()) } else { Value::Null }),
        ("authentication", authentication.unwrap_or(Value::Null)),
        ("options", obj(vec![("encrypt", json!(encrypt)), ("trustServerCertificate", json!(c.trust || !encrypt)), ("instanceName", instance.map(js).unwrap_or(Value::Null)), ("readOnlyIntent", if c.read_only { json!(true) } else { Value::Null })])),
    ]);
    let mut nn = notes.clone();
    if auth == "windows_integrated" {
        nn.push(note("node_integrated", &[]));
    }
    b.push("node", "Node.js (mssql, tedious) · JSON", pretty(&cfg), nn);

    // go-mssqldb.
    let pw = secret(b);
    let mut gq: Vec<(&str, V)> = Vec::new();
    if !c.database.is_empty() {
        gq.push(("database", V::t(c.database.clone())));
    }
    gq.push(("encrypt", V::t(if encrypt { "true" } else { "disable" })));
    if c.trust && encrypt {
        gq.push(("TrustServerCertificate", V::t("true")));
    }
    let fedauth = match auth.as_str() {
        "entra_password" => Some("ActiveDirectoryPassword"),
        "entra_mfa" => Some("ActiveDirectoryInteractive"),
        "entra_sp" => Some("ActiveDirectoryServicePrincipal"),
        "entra_msi" => Some("ActiveDirectoryManagedIdentity"),
        "entra_default" | "entra_integrated" => Some("ActiveDirectoryDefault"),
        _ => None,
    };
    if let Some(f) = fedauth {
        gq.push(("fedauth", V::t(f)));
    }
    if c.read_only {
        gq.push(("ApplicationIntent", V::t("ReadOnly")));
    }
    let at = match instance {
        Some(i) => format!("{}/{}", hp(host, 0), pct(i)),
        None => hp(host, c.port),
    };
    let mut gn = notes.clone();
    if fedauth.is_some() {
        gn.push(note("go_azuread", &[]));
    }
    b.push("go", "Go (go-mssqldb)", format!("sqlserver://{}{at}{}", userinfo(&user, pw.as_ref()), query(&gq)), gn);
}

// ---------------------------------------------------------------------------
// Oracle.

fn oracle(b: &mut B) {
    let c = b.c;
    let adb = c.driver == "oracle_adb";
    let descriptor = c.opt("connect_descriptor").map(|d| d.split_whitespace().collect::<Vec<_>>().join(" "));
    let alias = if adb { c.opt("adb_name").map(|n| format!("{}_{}", n.to_ascii_lowercase(), c.opt("adb_service").unwrap_or("high"))) } else { None };
    let sid = c.opt("connect_by") == Some("sid");
    let service = c.opt("service").unwrap_or("").to_string();
    // Easy Connect, a descriptor or a TNS alias.
    let target = if let Some(d) = descriptor.clone() {
        d
    } else if let Some(a) = alias.clone() {
        a
    } else if sid {
        format!("(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST={})(PORT={}))(CONNECT_DATA=(SID={service})))", c.host_or_localhost(), c.port)
    } else {
        format!("{}/{service}", c.hp())
    };
    let wallet = c.opt("wallet_dir").map(str::to_string);
    let role = c.opt("role").map(|r| r.to_ascii_uppercase());
    let mut notes = Vec::new();
    if wallet.is_some() {
        notes.push(note("wallet", &[("dir", wallet.as_deref().unwrap_or(""))]));
    }
    if alias.is_some() && descriptor.is_none() {
        notes.push(note("tns_alias", &[]));
    }

    b.push("easyconnect", "Easy Connect / TNS", target.clone(), notes.clone());

    let pw = b.pw();
    let ado = pairs(
        &[
            ("User Id", some_t(&c.user)),
            ("Password", pw.clone()),
            ("Data Source", Some(V::t(target.clone()))),
            ("DBA Privilege", role.clone().map(V::t)),
            ("Wallet_Location", wallet.clone().map(V::t)),
            ("TNS_ADMIN", wallet.clone().or(c.opt("tns_admin").map(str::to_string)).map(V::t)),
        ],
        ado_val,
    );
    b.push("adonet", "ADO.NET (Oracle.ManagedDataAccess)", ado, notes.clone());

    let pw = b.pw();
    let url = if descriptor.is_some() || alias.is_some() {
        let mut u = format!("jdbc:oracle:thin:@{target}");
        if let Some(w) = &wallet {
            u.push_str(&format!("?TNS_ADMIN={}", pct_path(w)));
        }
        u
    } else if sid {
        format!("jdbc:oracle:thin:@{}:{service}", c.hp())
    } else {
        format!("jdbc:oracle:thin:@//{}/{service}", c.hp())
    };
    let mut jn = notes.clone();
    jn.extend(jdbc_notes(&c.user, pw.is_some()));
    if let Some(r) = &role {
        jn.push(note("jdbc_role", &[("role", r)]));
    }
    b.push("jdbc", "JDBC (thin)", jdbc_props(url, &c.user, pw), jn);
    // The export's DSN for a descriptor or an alias is the one written here.
    let dsn: Vec<(&str, V)> = if descriptor.is_some() || alias.is_some() || adb { vec![("dsn", V::t(target.clone()))] } else { vec![] };
    b.sqlalchemy(&dsn, notes.clone());

    // python-oracledb: user/password@dsn; a password with special
    // characters goes in double quotes.
    let pw = b.pw();
    let quoted = |s: &str| if s.contains(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '$' && ch != '#') { format!("\"{s}\"") } else { s.to_string() };
    let creds = match (&pw, c.user.is_empty()) {
        (_, true) => String::new(),
        (None, false) => format!("{}@", c.user),
        (Some(p), false) => format!("{}/{}@", c.user, p.esc(quoted)),
    };
    b.push("oracledb", "Python (python-oracledb)", format!("{creds}{target}"), notes.clone());

    let pw = b.pw();
    let node = obj(vec![
        ("user", js(&c.user)),
        ("password", jv(pw)),
        ("connectString", js(&target)),
        ("configDir", wallet.as_deref().map(js).unwrap_or(Value::Null)),
        ("walletLocation", wallet.as_deref().map(js).unwrap_or(Value::Null)),
        ("walletPassword", if wallet.is_some() { jv(b.s("wallet_password")) } else { Value::Null }),
        ("privilege", role.as_deref().map(|r| Value::String(format!("oracledb.{r}"))).unwrap_or(Value::Null)),
    ]);
    b.push("node", "Node.js (node-oracledb) · JSON", pretty(&node), notes.clone());

    // go-ora: only Easy Connect fits its URL.
    if descriptor.is_none() && alias.is_none() {
        let pw = b.pw();
        let (path, mut q) = if sid { (String::new(), vec![("SID", V::t(service.clone()))]) } else { (format!("/{}", pct(&service)), vec![]) };
        if let Some(w) = &wallet {
            q.push(("WALLET", V::t(w.clone())));
        }
        b.push("go", "Go (go-ora)", format!("oracle://{}{}{path}{}", userinfo(&c.user, pw.as_ref()), c.hp(), query(&q)), notes);
    }
}

// ---------------------------------------------------------------------------
// Embedded engines.

fn file_path(c: &Conn) -> String {
    if c.host.is_empty() {
        c.database.clone()
    } else {
        c.host.clone()
    }
}

/// `file:` URI of a path (RFC 8089), as SQLite reads it.
fn file_uri(path: &str, read_only: bool) -> String {
    let p = path.replace('\\', "/");
    let p = if p.len() > 1 && p.as_bytes()[1] == b':' { format!("/{p}") } else { p };
    let mut out = format!("file:{}", pct_path(&p));
    if read_only {
        out.push_str("?mode=ro");
    }
    out
}

fn sqlite(b: &mut B) {
    let c = b.c;
    let path = file_path(c);
    b.push("path", "", path.clone(), vec![]);
    b.push("uri", "URI (file:)", file_uri(&path, c.read_only), vec![]);
    let ado = pairs(&[("Data Source", Some(V::t(path.clone()))), ("Mode", c.read_only.then(|| V::t("ReadOnly")))], ado_val);
    b.push("adonet", "ADO.NET (Microsoft.Data.Sqlite)", ado, vec![]);
    b.push("jdbc", "JDBC (sqlite-jdbc)", format!("jdbc:sqlite:{path}"), vec![]);
    b.sqlalchemy(&[], vec![]);
    let odbc = pairs(&[("DRIVER", Some(V::R("{SQLite3 ODBC Driver}".into()))), ("Database", Some(V::t(path)))], odbc_val);
    b.push("odbc", "ODBC", odbc, vec![note("odbc_driver_name", &[("driver", "SQLite3 ODBC Driver")])]);
}

fn libsql(b: &mut B) {
    let c = b.c;
    let host = c.host.trim_start_matches("libsql://").trim_start_matches("https://").trim_end_matches('/').to_string();
    let tok = b.s("auth_token");
    let q: Vec<(&str, V)> = tok.clone().map(|t| vec![("authToken", t)]).unwrap_or_default();
    b.push("uri", "URI (libsql://)", format!("libsql://{host}{}", query(&q)), vec![]);
    b.sqlalchemy(&[], vec![]);
    let tok = b.s("auth_token");
    b.push("node", "Node.js (@libsql/client) · JSON", pretty(&obj(vec![("url", Value::String(format!("libsql://{host}"))), ("authToken", jv(tok))])), vec![]);
}

fn duckdb(b: &mut B) {
    let c = b.c;
    let path = file_path(c);
    if c.driver == "duckdb_files" {
        b.push("path", "", path, vec![note("files_folder", &[])]);
        return;
    }
    b.push("path", "", path.clone(), vec![]);
    let mut jdbc = format!("jdbc:duckdb:{path}");
    if c.read_only {
        jdbc.push_str("?duckdb.read_only=true");
    }
    b.push("jdbc", "JDBC (duckdb_jdbc)", jdbc, vec![]);
    let mut sa = format!("duckdb:///{path}");
    if c.read_only {
        sa.push_str("?read_only=true");
    }
    b.push("sqlalchemy", "Python (SQLAlchemy, duckdb_engine)", sa, vec![]);
    let odbc = pairs(&[("DRIVER", Some(V::R("{DuckDB Driver}".into()))), ("Database", Some(V::t(path)))], odbc_val);
    b.push("odbc", "ODBC", odbc, vec![note("odbc_driver_name", &[("driver", "DuckDB Driver")])]);
}

// ---------------------------------------------------------------------------
// ODBC engines (crates/drivers/odbc).

/// The JDBC URL of an engine DBine reaches through ODBC, where its vendor
/// has a JDBC driver with a known URL.
fn odbc_jdbc_url(c: &Conn) -> Option<String> {
    let h = c.host_or_localhost();
    let hp = c.hp();
    let d = &c.database;
    let tls = c.encrypt;
    Some(match c.driver.as_str() {
        "db2" | "db2zos" => format!("jdbc:db2://{hp}/{d}{}", if tls { ":sslConnection=true;" } else { "" }),
        "db2i" => format!("jdbc:as400://{h};naming=sql{}", if tls { ";secure=true" } else { "" }),
        "teradata" => format!("jdbc:teradata://{h}/DBS_PORT={}{}{}", c.port, if d.is_empty() { String::new() } else { format!(",DATABASE={d}") }, if tls { ",ENCRYPTDATA=ON" } else { "" }),
        "vertica" => format!("jdbc:vertica://{hp}/{d}{}", if tls { "?TLSmode=require" } else { "" }),
        "exasol" => format!("jdbc:exa:{hp}{}", if d.is_empty() { String::new() } else { format!(";schema={d}") }),
        "hive" | "spark" | "kyuubi" => format!("jdbc:hive2://{hp}/{d}{}", if tls { ";ssl=true" } else { "" }),
        "impala" => format!("jdbc:impala://{hp}/{d}{}", if tls { ";SSL=1" } else { "" }),
        "cloudera" => format!("jdbc:hive2://{hp}/{d};transportMode=http;httpPath={};ssl=true", c.opt("http_path").unwrap_or("cliservice")),
        "informix" => format!("jdbc:informix-sqli://{hp}/{d}:INFORMIXSERVER={}", c.opt("informix_server").unwrap_or("")),
        "gbase8s" => format!("jdbc:gbasedbt-sqli://{hp}/{d}:GBASEDBTSERVER={}", c.opt("informix_server").unwrap_or("")),
        "sybase" => format!("jdbc:sybase:Tds:{hp}/{d}"),
        "sqlanywhere" => format!("jdbc:sqlanywhere:Host={hp}{}", if d.is_empty() { String::new() } else { format!(";DatabaseName={d}") }),
        "netezza" => format!("jdbc:netezza://{hp}/{d}"),
        "monetdb" => format!("jdbc:monetdb://{hp}/{d}"),
        "iris" => format!("jdbc:IRIS://{hp}/{d}"),
        "cache" => format!("jdbc:Cache://{hp}/{d}"),
        "mimer" => format!("jdbc:mimer://{hp}/{d}"),
        "ignite" | "ignite3" => format!("jdbc:ignite:thin://{hp}"),
        "altibase" => format!("jdbc:Altibase://{hp}/{}", if d.is_empty() { "mydb" } else { d }),
        "cubrid" => format!("jdbc:cubrid:{h}:{}:{d}:::", c.port),
        "dameng" => format!("jdbc:dm://{hp}"),
        "heavydb" => format!("jdbc:heavyai:{h}:{}:{d}", c.port),
        "nuodb" => format!("jdbc:com.nuodb://{hp}/{d}"),
        "virtuoso" => format!("jdbc:virtuoso://{hp}"),
        "sqream" => format!("jdbc:Sqream://{hp}/{d}{}", if c.opt("cluster") == Some("true") { ";cluster=true" } else { "" }),
        "openedge" => format!("jdbc:datadirect:openedge://{hp};databaseName={d}"),
        "zen" => format!("jdbc:pervasive://{hp}/{d}"),
        "maxdb" => format!("jdbc:sapdb://{hp}/{d}"),
        "machbase" => format!("jdbc:machbase://{hp}/{}", if d.is_empty() { "mydb" } else { d }),
        "ocient" => format!("jdbc:ocient://{hp}/{d}"),
        "netsuite" => format!(
            "jdbc:ns://{hp};ServerDataSource=NetSuite2.com;Encrypted=1;NegotiateSSLClose=false;CustomProperties=(AccountID={};RoleID={})",
            c.opt("account_id").unwrap_or(""),
            c.opt("role_id").unwrap_or("")
        ),
        "access" => format!("jdbc:ucanaccess://{}", file_path(c)),
        _ => return None,
    })
}

fn odbc_engine(b: &mut B) {
    let c = b.c;
    let id = c.driver.clone();
    let file = matches!(id.as_str(), "access" | "dbase");
    let mut notes = Vec::new();
    if c.opts.contains_key("extra") || c.py.odbc_extra {
        notes.push(note("odbc_extra", &[]));
    }

    // The ODBC string: the generic driver's DSN or saved string, else the driver name.
    if id == "odbc" {
        if let Some(dsn) = c.opt("dsn") {
            let pw = b.pw();
            let s = pairs(&[("DSN", Some(V::t(dsn))), ("UID", some_t(&c.user)), ("PWD", pw)], odbc_val);
            b.push("odbc", "ODBC", s, notes);
        } else {
            // The saved connection string is a secret (it may hold credentials).
            let s = b.s("connection_string").map(|v| v.esc(|s| s.to_string())).unwrap_or_default();
            notes.push(note("saved_string", &[]));
            b.push("odbc", "ODBC", s, notes);
        }
        return;
    }
    let driver = c.opt("odbc_driver").unwrap_or("").to_string();
    let pw = b.pw();
    let mut parts: Vec<(&str, Option<V>)> = vec![("DRIVER", Some(V::R(format!("{{{}}}", if driver.is_empty() { "…" } else { &driver }.replace('}', "}}")))))];
    if file {
        parts.push(("DBQ", Some(V::t(file_path(c)))));
    } else {
        parts.push(("SERVER", some_t(&c.host)));
        if c.port != 0 {
            parts.push(("PORT", Some(V::t(c.port.to_string()))));
        }
        parts.push(("DATABASE", some_t(&c.database)));
    }
    parts.push(("UID", some_t(&c.user)));
    parts.push(("PWD", pw));
    let mut on = notes.clone();
    on.push(note("odbc_driver_name", &[("driver", &driver)]));
    on.push(note("odbc_clients", &[]));
    b.push("odbc", "ODBC", pairs(&parts, odbc_val), on);

    // JDBC, where the vendor has a driver.
    if let Some(url) = odbc_jdbc_url(c) {
        let pw = b.pw();
        let mut jn = notes.clone();
        jn.extend(jdbc_notes(&c.user, pw.is_some()));
        b.push("jdbc", "JDBC", jdbc_props(url, &c.user, pw), jn);
    }

    // ADO.NET, where there's a mainstream provider.
    let pw = b.pw();
    let host = some_t(&c.host);
    let port = (c.port != 0).then(|| V::t(c.port.to_string()));
    let db = some_t(&c.database);
    let user = some_t(&c.user);
    let ado: Option<(&str, String)> = match id.as_str() {
        "db2" | "db2i" | "db2zos" => Some(("ADO.NET (IBM.Data.Db2)", pairs(&[("Server", Some(V::t(c.hp()))), ("Database", db), ("UID", user), ("PWD", pw), ("Security", c.encrypt.then(|| V::t("SSL")))], ado_val))),
        "teradata" => Some(("ADO.NET (Teradata.Client.Provider)", pairs(&[("Data Source", host), ("Port Number", port), ("Database", db), ("User Id", user), ("Password", pw)], ado_val))),
        "vertica" => Some(("ADO.NET (Vertica.Data)", pairs(&[("Host", host), ("Port", port), ("Database", db), ("User", user), ("Password", pw)], ado_val))),
        "sybase" => Some(("ADO.NET (AdoNetCore.AseClient)", pairs(&[("Data Source", host), ("Port", port), ("Database", db), ("Uid", user), ("Pwd", pw)], ado_val))),
        "sqlanywhere" => Some(("ADO.NET (Sap.Data.SQLAnywhere)", pairs(&[("Host", Some(V::t(c.hp()))), ("DatabaseName", db), ("UserID", user), ("Password", pw)], ado_val))),
        "iris" | "cache" => Some(("ADO.NET (InterSystems)", pairs(&[("Server", host), ("Port", port), ("Namespace", db), ("User ID", user), ("Password", pw)], ado_val))),
        "access" => Some(("ADO.NET (OleDb, ACE)", pairs(&[("Provider", Some(V::t("Microsoft.ACE.OLEDB.12.0"))), ("Data Source", Some(V::t(file_path(c)))), ("Jet OLEDB:Database Password", b.s("password"))], ado_val))),
        _ => None,
    };
    if let Some((label, text)) = ado {
        b.push("adonet", label, text, notes.clone());
    }

    // SQLAlchemy, where the Python export has a dialect (else pyodbc takes the ODBC string).
    b.sqlalchemy(&[], notes);
}

// ---------------------------------------------------------------------------
// Document, key-value and graph engines.

fn mongo(b: &mut B) {
    let c = b.c;
    let id = c.driver.as_str();
    // The saved connection string (a secret) is the URI as it is.
    if let Fill::Values(m) = b.fill {
        if let Some(s) = m.get("connection_string").filter(|s| !s.trim().is_empty()) {
            b.push("uri", "URI (mongodb://)", s.trim().to_string(), vec![note("same_uri", &[])]);
            return;
        }
    }
    let kerberos = c.auth.as_deref() == Some("kerberos");
    let pw = if kerberos { None } else { b.pw() };
    let (scheme, rest) = if let Some(r) = c.host.strip_prefix("mongodb+srv://") {
        ("mongodb+srv", r.trim_end_matches('/').to_string())
    } else if let Some(r) = c.host.strip_prefix("mongodb://") {
        ("mongodb", r.trim_end_matches('/').to_string())
    } else {
        ("mongodb", c.hp())
    };
    // A URI host may bring its own path (its options are left out with
    // the user info: they may hold credentials).
    let (hosts, path) = rest.split_once('/').map(|(h, p)| (h.to_string(), p.to_string())).unwrap_or((rest, String::new()));
    let db = if c.database.is_empty() { path } else { pct(&c.database) };
    let mut q: Vec<(&str, V)> = Vec::new();
    if let Some(a) = c.opt("auth_source") {
        q.push(("authSource", V::t(a)));
    }
    if let Some(r) = c.opt("replica_set") {
        q.push(("replicaSet", V::t(r)));
    }
    let tls = c.encrypt || id == "documentdb" && c.opt("ca_file").is_some();
    if tls {
        q.push(("tls", V::t("true")));
        if c.trust {
            q.push(("tlsAllowInvalidCertificates", V::t("true")));
        }
    }
    if let Some(ca) = c.opt("ca_file") {
        q.push(("tlsCAFile", V::t(ca)));
    }
    if id == "documentdb" {
        q.push(("retryWrites", V::t("false")));
    }
    if kerberos {
        q.push(("authMechanism", V::t("GSSAPI")));
        if let Some(s) = c.opt("kerberos_service") {
            q.push(("authMechanismProperties", V::t(format!("SERVICE_NAME:{s}"))));
        }
    }
    let qs = query(&q);
    let mut notes = vec![note("same_uri", &[])];
    if c.host.is_empty() && matches!(b.fill, Fill::Placeholder | Fill::Masked) {
        notes.push(note("saved_string_mongo", &[]));
    }
    b.push("uri", "URI (mongodb://)", format!("{scheme}://{}{hosts}/{db}{qs}", userinfo(&c.user, pw.as_ref())), notes);
}

fn redis(b: &mut B) {
    let c = b.c;
    let pw = b.s("password");
    let scheme = if c.encrypt { "rediss" } else { "redis" };
    let db = c.database.parse::<u32>().unwrap_or(0);
    let notes: Vec<Note> = if c.encrypt && c.trust { vec![note("tls_unverified", &[])] } else { vec![] };
    b.push("uri", "URI (redis://)", format!("{scheme}://{}{}/{db}", userinfo(&c.user, pw.as_ref()), c.hp()), notes.clone());
    let pw = b.s("password");
    let mut parts = vec![c.hp()];
    if !c.user.is_empty() {
        parts.push(format!("user={}", c.user));
    }
    if let Some(p) = pw {
        parts.push(format!("password={}", p.esc(|s| s.to_string())));
    }
    if c.encrypt {
        parts.push("ssl=true".into());
    }
    if db != 0 {
        parts.push(format!("defaultDatabase={db}"));
    }
    parts.push("abortConnect=false".into());
    let mut n = notes;
    n.push(note("stackexchange_commas", &[]));
    b.push("adonet", "ADO.NET (StackExchange.Redis)", parts.join(","), n);
}

fn cassandra(b: &mut B) {
    let c = b.c;
    let keyspaces = c.driver == "keyspaces";
    let tls = c.encrypt || keyspaces;
    let dc = c.opt("datacenter").map(str::to_string);
    let mut notes = Vec::new();
    if keyspaces {
        notes.push(note("keyspaces", &[]));
    }
    let pw = b.pw();
    let cfg = obj(vec![
        ("contactPoints", json!([c.hp()])),
        ("localDataCenter", dc.as_deref().map(js).unwrap_or(Value::Null)),
        ("keyspace", js(&c.database)),
        ("credentials", if c.user.is_empty() { Value::Null } else { obj(vec![("username", js(&c.user)), ("password", jv(pw))]) }),
        ("sslOptions", if tls { json!({"rejectUnauthorized": !c.trust}) } else { Value::Null }),
    ]);
    b.push("node", "Node.js (cassandra-driver) · JSON", pretty(&cfg), notes.clone());
    let pw = b.pw();
    let mut q: Vec<(&str, V)> = Vec::new();
    if let Some(d) = &dc {
        q.push(("localdatacenter", V::t(d.clone())));
    }
    if tls {
        q.push(("enablessl", V::t("true")));
    }
    let url = format!("jdbc:cassandra://{}/{}{}", c.hp(), pct(&c.database), query(&q));
    let mut jn = notes;
    jn.extend(jdbc_notes(&c.user, pw.is_some()));
    b.push("jdbc", "JDBC (cassandra-jdbc-wrapper)", jdbc_props(url, &c.user, pw), jn);
}

fn neo4j(b: &mut B) {
    let c = b.c;
    let id = c.driver.as_str();
    let tls = c.encrypt || id == "neptune";
    let base = if id == "neo4j" { "neo4j" } else { "bolt" };
    let scheme = match (tls, c.trust) {
        (false, _) => base.to_string(),
        (true, false) => format!("{base}+s"),
        (true, true) => format!("{base}+ssc"),
    };
    let uri = format!("{scheme}://{}", c.hp());
    let mut notes = vec![note("credentials_apart", &[])];
    if c.iam {
        notes.push(note("neptune_iam", &[]));
    }
    b.push("uri", "URI (Bolt)", uri.clone(), notes.clone());
    let pw = if id == "neptune" { None } else { b.pw() };
    let cfg = obj(vec![("uri", js(&uri)), ("user", js(&c.user)), ("password", jv(pw)), ("database", js(&c.database))]);
    b.push("node", "Node.js (neo4j-driver) · JSON", pretty(&cfg), notes.clone());
    if id != "neptune" {
        let pw = b.pw();
        let q: Vec<(&str, V)> = if c.database.is_empty() { vec![] } else { vec![("database", V::t(c.database.clone()))] };
        let url = format!("jdbc:neo4j{}://{}{}", if tls { "+s" } else { "" }, c.hp(), query(&q));
        let mut jn = vec![];
        jn.extend(jdbc_notes(&c.user, pw.is_some()));
        b.push("jdbc", "JDBC (Neo4j JDBC)", jdbc_props(url, &c.user, pw), jn);
    }
}

fn couchbase(b: &mut B) {
    let c = b.c;
    let scheme = if c.encrypt { "couchbases" } else { "couchbase" };
    let host = c.host.trim_start_matches("couchbase://").trim_start_matches("couchbases://").to_string();
    let uri = format!("{scheme}://{}", if host.is_empty() { "localhost".into() } else { host });
    b.push("uri", "URI (couchbase://)", uri.clone(), vec![note("credentials_apart", &[])]);
    let pw = b.pw();
    let cfg = obj(vec![("connectionString", js(&uri)), ("username", js(&c.user)), ("password", jv(pw)), ("bucket", js(&c.database))]);
    b.push("node", "Node.js (couchbase) · JSON", pretty(&cfg), vec![]);
}

// ---------------------------------------------------------------------------
// HTTP engines.

/// `scheme://user:password@host:port` of an HTTP engine.
fn http_url(c: &Conn, user: &str, pw: Option<&V>) -> String {
    let base = python::http_base(&c.py);
    match base.split_once("://") {
        Some((s, rest)) => format!("{s}://{}{rest}", userinfo(user, pw)),
        None => base,
    }
}

fn http(b: &mut B) {
    let c = b.c;
    let id = c.driver.clone();
    let base = python::http_base(&c.py);
    let tls = base.starts_with("https://");
    match id.as_str() {
        "elasticsearch" | "opensearch" | "opendistro" => {
            let pw = b.pw();
            b.push("uri", "URL", http_url(c, &c.user, pw.as_ref()), vec![]);
            let pw = b.pw();
            let auth = if !c.user.is_empty() {
                obj(vec![("username", js(&c.user)), ("password", jv(pw))])
            } else {
                obj(vec![("apiKey", jv(b.s("api_key")))])
            };
            let cfg = match c.opt("cloud_id") {
                Some(cid) if id == "elasticsearch" => obj(vec![("cloud", json!({"id": cid})), ("auth", auth)]),
                _ => obj(vec![("node", js(&base)), ("auth", auth), ("tls", if tls && c.trust { json!({"rejectUnauthorized": false}) } else { Value::Null })]),
            };
            let label = if id == "elasticsearch" { "Node.js (@elastic/elasticsearch) · JSON" } else { "Node.js (@opensearch-project/opensearch) · JSON" };
            b.push("node", label, pretty(&cfg), vec![]);
            let pw = b.pw();
            let url = if id == "elasticsearch" { format!("jdbc:es://{base}") } else { format!("jdbc:opensearch://{base}") };
            let jn = jdbc_notes(&c.user, pw.is_some());
            b.push("jdbc", "JDBC", jdbc_props(url, &c.user, pw), jn);
        }
        "orientdb" => {
            let pw = b.pw();
            b.push("uri", "URL (REST)", http_url(c, &c.user, pw.as_ref()), vec![]);
            let pw = b.pw();
            let url = format!("jdbc:orient:remote:{}/{}", c.host_or_localhost(), c.database);
            let mut jn = vec![note("other_port", &[("port", "2424")])];
            jn.extend(jdbc_notes(&c.user, pw.is_some()));
            b.push("jdbc", "JDBC (orientdb-jdbc)", jdbc_props(url, &c.user, pw), jn);
        }
        "iotdb" | "timechodb" => {
            let pw = b.pw();
            b.push("uri", "URL (REST)", http_url(c, &c.user, pw.as_ref()), vec![]);
            let pw = b.pw();
            let url = format!("jdbc:iotdb://{}/", hp(c.host_or_localhost(), 6667));
            let mut jn = vec![note("other_port", &[("port", "6667")])];
            jn.extend(jdbc_notes(&c.user, pw.is_some()));
            b.push("jdbc", "JDBC (iotdb-jdbc)", jdbc_props(url, &c.user, pw), jn);
        }
        "etcd" => {
            let n = if c.user.is_empty() { vec![] } else { vec![note("credentials_apart", &[])] };
            b.push("uri", "URL (endpoint)", base, n);
        }
        "couchdb" => {
            let pw = b.pw();
            let mut u = http_url(c, &c.user, pw.as_ref());
            if !c.database.is_empty() {
                u = format!("{}/{}", u.trim_end_matches('/'), pct(&c.database));
            }
            b.push("uri", "URL", u, vec![]);
        }
        _ => {
            // Solr, ksqlDB: the URL with basic credentials.
            let pw = b.pw();
            b.push("uri", "URL", http_url(c, &c.user, pw.as_ref()), vec![]);
        }
    }
}

fn influx(b: &mut B) {
    let c = b.c;
    let base = python::http_base(&c.py);
    match c.driver.as_str() {
        "influxdb1" => {
            let pw = b.pw();
            let mut u = http_url(c, &c.user, pw.as_ref());
            if !c.database.is_empty() {
                u = format!("{}/?db={}", u.trim_end_matches('/'), pct(&c.database));
            }
            b.push("uri", "URL", u, vec![]);
        }
        "influxdb" => {
            b.push("uri", "URL", base.clone(), vec![note("token_apart", &[])]);
            let tok = b.s("token");
            b.push("node", "Node.js (@influxdata/influxdb-client) · JSON", pretty(&obj(vec![("url", js(&base)), ("token", jv(tok)), ("org", c.opt("org").map(js).unwrap_or(Value::Null)), ("bucket", js(&c.database))])), vec![]);
        }
        _ => {
            b.push("uri", "URL", base.clone(), vec![note("token_apart", &[])]);
            let tok = b.s("token");
            b.push("node", "Node.js (@influxdata/influxdb3-client) · JSON", pretty(&obj(vec![("host", js(&base)), ("token", jv(tok)), ("database", js(&c.database))])), vec![]);
        }
    }
}

fn clickhouse(b: &mut B) {
    let c = b.c;
    let port = if c.py.port != 0 { c.py.port } else if c.encrypt { 8443 } else { 8123 };
    let scheme = if c.encrypt { "https" } else { "http" };
    let at = hp(c.host_or_localhost(), port);
    let notes: Vec<Note> = if c.encrypt && c.trust { vec![note("tls_unverified", &[])] } else { vec![] };
    let pw = b.s("password");
    let user = if c.user.is_empty() { "default".to_string() } else { c.user.clone() };
    let dbq: Vec<(&str, V)> = if c.database.is_empty() { vec![] } else { vec![("database", V::t(c.database.clone()))] };
    b.push("uri", "URL (HTTP)", format!("{scheme}://{}{at}/{}", userinfo(&user, pw.as_ref()), query(&dbq)), notes.clone());

    let pw = b.s("password");
    let ado = pairs(
        &[
            ("Host", Some(V::t(c.host_or_localhost()))),
            ("Port", Some(V::t(port.to_string()))),
            ("Protocol", Some(V::t(scheme))),
            ("Username", Some(V::t(user.clone()))),
            ("Password", pw),
            ("Database", some_t(&c.database)),
        ],
        ado_val,
    );
    b.push("adonet", "ADO.NET (ClickHouse.Client)", ado, notes.clone());

    let pw = b.s("password");
    let url = format!("jdbc:clickhouse:{scheme}://{at}/{}{}", pct(&c.database), if c.encrypt && c.trust { "?ssl=true&sslmode=none" } else { "" });
    let mut jn = notes.clone();
    jn.extend(jdbc_notes(&user, pw.is_some()));
    b.push("jdbc", "JDBC (clickhouse-jdbc)", jdbc_props(url, &user, pw), jn);

    let pw = b.s("password");
    let mut q: Vec<(&str, V)> = Vec::new();
    if c.encrypt {
        q.push(("protocol", V::t("https")));
        if c.trust {
            q.push(("verify", V::t("false")));
        }
    }
    let path = if c.database.is_empty() { String::new() } else { format!("/{}", pct(&c.database)) };
    b.push("sqlalchemy", "Python (SQLAlchemy, clickhouse-connect)", format!("clickhousedb://{}{at}{path}{}", userinfo(&user, pw.as_ref()), query(&q)), notes.clone());

    let pw = b.s("password");
    let cfg = obj(vec![("url", Value::String(format!("{scheme}://{at}"))), ("username", js(&user)), ("password", jv(pw)), ("database", js(&c.database))]);
    b.push("node", "Node.js (@clickhouse/client) · JSON", pretty(&cfg), notes);
}

// ---------------------------------------------------------------------------
// Cloud services.

fn cosmos(b: &mut B) {
    let c = b.c;
    let endpoint = format!("{}/", python::http_base(&c.py).trim_end_matches('/'));
    let key = b.s("account_key");
    let s = pairs(&[("AccountEndpoint", Some(V::t(endpoint))), ("AccountKey", key)], |s| s.to_string());
    let n = if c.database.is_empty() { vec![] } else { vec![note("cosmos_database", &[("database", &c.database)])] };
    b.push("azure", "Azure Cosmos DB", s, n);
}

/// The AWS credentials of a connection, as an SDK config object.
fn aws_credentials(b: &B) -> (Value, Vec<Note>) {
    let c = b.c;
    match c.auth.as_deref() {
        Some("keys") => (
            obj(vec![("accessKeyId", c.opt("access_key_id").map(js).unwrap_or(Value::Null)), ("secretAccessKey", jv(b.s("secret_access_key"))), ("sessionToken", jv(b.s("session_token").filter(|v| matches!(v, V::T(_)))))]),
            vec![],
        ),
        Some("profile") => (Value::Null, vec![note("aws_profile", &[("profile", c.opt("profile").unwrap_or("default"))])]),
        _ => (Value::Null, vec![note("aws_default", &[])]),
    }
}

fn dynamo(b: &mut B) {
    let c = b.c;
    let (creds, notes) = aws_credentials(b);
    let cfg = obj(vec![("region", c.opt("region").map(js).unwrap_or(Value::Null)), ("endpoint", c.opt("endpoint_url").map(js).unwrap_or(Value::Null)), ("credentials", creds)]);
    b.push("node", "AWS SDK · JSON", pretty(&cfg), notes);
}

fn athena(b: &mut B) {
    let c = b.c;
    let region = c.opt("region").unwrap_or("us-east-1").to_string();
    let keys = c.auth.as_deref() == Some("keys");
    let (_, notes) = aws_credentials(b);
    let secret = if keys { b.s("secret_access_key") } else { None };
    let mut parts: Vec<(&str, Option<V>)> = vec![
        ("Region", Some(V::t(region.clone()))),
        ("WorkGroup", c.opt("workgroup").map(V::t)),
        ("OutputLocation", c.opt("output_location").map(V::t)),
        ("Catalog", c.opt("catalog").map(V::t)),
        ("Database", some_t(&c.database)),
    ];
    match c.auth.as_deref() {
        Some("keys") => {
            parts.push(("User", c.opt("access_key_id").map(V::t)));
            parts.push(("Password", secret));
        }
        Some("profile") => {
            parts.push(("CredentialsProvider", Some(V::t("ProfileCredentials"))));
            parts.push(("ProfileName", Some(V::t(c.opt("profile").unwrap_or("default")))));
        }
        _ => parts.push(("CredentialsProvider", Some(V::t("DefaultChain")))),
    }
    b.push("jdbc", "JDBC (Athena JDBC 3)", format!("jdbc:athena://{}", pairs(&parts, mssql_jdbc_val)), notes.clone());

    let secret = if keys { b.s("secret_access_key") } else { None };
    let mut op: Vec<(&str, Option<V>)> = vec![
        ("DRIVER", Some(V::R("{Amazon Athena ODBC (x64)}".into()))),
        ("AwsRegion", Some(V::t(region))),
        ("Workgroup", c.opt("workgroup").map(V::t)),
        ("S3OutputLocation", c.opt("output_location").map(V::t)),
        ("Catalog", c.opt("catalog").map(V::t)),
        ("Schema", some_t(&c.database)),
    ];
    match c.auth.as_deref() {
        Some("keys") => {
            op.push(("AuthenticationType", Some(V::t("IAM Credentials"))));
            op.push(("UID", c.opt("access_key_id").map(V::t)));
            op.push(("PWD", secret));
        }
        Some("profile") => {
            op.push(("AuthenticationType", Some(V::t("IAM Profile"))));
            op.push(("AWSProfile", Some(V::t(c.opt("profile").unwrap_or("default")))));
        }
        _ => op.push(("AuthenticationType", Some(V::t("Default Credentials")))),
    }
    let mut on = notes.clone();
    on.push(note("odbc_driver_name", &[("driver", "Amazon Athena ODBC (x64)")]));
    b.push("odbc", "ODBC", pairs(&op, odbc_val), on);

    b.sqlalchemy(&[], notes);
}

fn bigquery(b: &mut B) {
    let c = b.c;
    let project = c.opt("project_id").unwrap_or("").to_string();
    let sa = c.auth.as_deref() == Some("service_account");
    let notes = vec![note(if sa { "gcp_service_account" } else { "gcp_adc" }, &[])];
    let mut url = format!("jdbc:bigquery://https://www.googleapis.com/bigquery/v2:443;ProjectId={project};");
    if !c.database.is_empty() {
        url.push_str(&format!("DefaultDataset={};", c.database));
    }
    if let Some(l) = c.opt("location") {
        url.push_str(&format!("Location={l};"));
    }
    url.push_str(if sa { "OAuthType=0;OAuthPvtKeyPath=${GOOGLE_APPLICATION_CREDENTIALS};" } else { "OAuthType=3;" });
    b.push("jdbc", "JDBC (Simba BigQuery)", url, notes.clone());
    b.sqlalchemy(&[], notes);
}

fn spanner(b: &mut B) {
    let c = b.c;
    let path = format!("projects/{}/instances/{}/databases/{}", c.opt("project_id").unwrap_or(""), c.opt("instance").unwrap_or(""), c.database);
    let sa = c.auth.as_deref() == Some("service_account");
    let mut notes = vec![note(if sa { "gcp_service_account" } else { "gcp_adc" }, &[])];
    if c.opt("endpoint_url").is_some() {
        notes.push(note("emulator", &[]));
    }
    b.push("jdbc", "JDBC (google-cloud-spanner-jdbc)", format!("jdbc:cloudspanner:/{path}{}", if sa { "?credentials=${GOOGLE_APPLICATION_CREDENTIALS}" } else { "" }), notes.clone());
    b.sqlalchemy(&[], notes.clone());
    b.push("go", "Go (go-sql-spanner)", path, notes);
}

fn snowflake(b: &mut B) {
    let c = b.c;
    let account = c.opt("account").unwrap_or("").to_string();
    let host = format!("{account}.snowflakecomputing.com");
    let keypair = c.auth.as_deref() == Some("keypair");
    let schema = c.opt("schema").map(str::to_string);
    let wh = c.opt("warehouse").map(str::to_string);
    let role = c.opt("role").map(str::to_string);
    let notes: Vec<Note> = if keypair { vec![note("keypair", &[])] } else { vec![note("pat", &[])] };
    let tok = |b: &B| if keypair { None } else { b.s("token") };

    let pw = tok(b);
    let mut q: Vec<(&str, V)> = Vec::new();
    for (k, v) in [("db", some_t(&c.database)), ("schema", schema.clone().map(V::t)), ("warehouse", wh.clone().map(V::t)), ("role", role.clone().map(V::t))] {
        if let Some(v) = v {
            q.push((k, v));
        }
    }
    let mut jq = q.clone();
    if keypair {
        jq.push(("authenticator", V::t("SNOWFLAKE_JWT")));
        jq.push(("private_key_file", V::R("${PRIVATE_KEY_FILE}".into())));
    }
    let mut jn = notes.clone();
    jn.extend(jdbc_notes(&c.user, pw.is_some()));
    b.push("jdbc", "JDBC (snowflake-jdbc)", jdbc_props(format!("jdbc:snowflake://{host}/{}", query(&jq)), &c.user, pw), jn);

    let pw = tok(b);
    let ado = pairs(
        &[
            ("account", Some(V::t(account.clone()))),
            ("user", some_t(&c.user)),
            ("password", pw),
            ("authenticator", keypair.then(|| V::t("snowflake_jwt"))),
            ("private_key_file", keypair.then(|| V::R("${PRIVATE_KEY_FILE}".into()))),
            ("db", some_t(&c.database)),
            ("schema", schema.clone().map(V::t)),
            ("warehouse", wh.clone().map(V::t)),
            ("role", role.clone().map(V::t)),
        ],
        ado_val,
    );
    b.push("adonet", "ADO.NET (Snowflake.Data)", ado, notes.clone());
    b.sqlalchemy(&[], notes.clone());

    let pw = tok(b);
    let mut gq: Vec<(&str, V)> = Vec::new();
    for (k, v) in [("warehouse", wh.clone()), ("role", role.clone())] {
        if let Some(v) = v {
            gq.push((k, V::t(v)));
        }
    }
    if keypair {
        gq.push(("authenticator", V::t("SNOWFLAKE_JWT")));
    }
    let mut path = String::new();
    if !c.database.is_empty() {
        path = format!("/{}", pct(&c.database));
        if let Some(s) = &schema {
            path.push_str(&format!("/{}", pct(s)));
        }
    }
    b.push("go", "Go (gosnowflake)", format!("{}{account}{path}{}", userinfo(&c.user, pw.as_ref()), query(&gq)), notes.clone());

    let pw = tok(b);
    let odbc = pairs(
        &[
            ("Driver", Some(V::R("{SnowflakeDSIIDriver}".into()))),
            ("Server", Some(V::t(host.clone()))),
            ("Database", some_t(&c.database)),
            ("Schema", schema.clone().map(V::t)),
            ("Warehouse", wh.clone().map(V::t)),
            ("Role", role.clone().map(V::t)),
            ("UID", some_t(&c.user)),
            ("PWD", pw),
            ("Authenticator", keypair.then(|| V::t("SNOWFLAKE_JWT"))),
            ("PRIV_KEY_FILE", keypair.then(|| V::R("${PRIVATE_KEY_FILE}".into()))),
        ],
        odbc_val,
    );
    let mut on = notes.clone();
    on.push(note("odbc_driver_name", &[("driver", "SnowflakeDSIIDriver")]));
    b.push("odbc", "ODBC", odbc, on);

    let pw = tok(b);
    let cfg = obj(vec![
        ("account", js(&account)),
        ("username", js(&c.user)),
        ("password", jv(pw)),
        ("authenticator", if keypair { json!("SNOWFLAKE_JWT") } else { Value::Null }),
        ("privateKeyPath", if keypair { json!("${PRIVATE_KEY_FILE}") } else { Value::Null }),
        ("database", js(&c.database)),
        ("schema", schema.as_deref().map(js).unwrap_or(Value::Null)),
        ("warehouse", wh.as_deref().map(js).unwrap_or(Value::Null)),
        ("role", role.as_deref().map(js).unwrap_or(Value::Null)),
    ]);
    b.push("node", "Node.js (snowflake-sdk) · JSON", pretty(&cfg), notes);
}

fn databricks(b: &mut B) {
    let c = b.c;
    let host = c.host.trim_start_matches("https://").trim_end_matches('/').to_string();
    let wh = c.opt("warehouse").unwrap_or("");
    let http_path = if wh.starts_with('/') || wh.is_empty() { wh.to_string() } else { format!("/sql/1.0/warehouses/{wh}") };
    let schema = c.opt("schema").map(str::to_string);
    let notes: Vec<Note> = if matches!(c.auth.as_deref(), None | Some("pat")) { vec![] } else { vec![note("databricks_pat", &[])] };

    let tok = b.s("token");
    let mut url = format!("jdbc:databricks://{host}:443/default;transportMode=http;ssl=1;AuthMech=3;httpPath={http_path}");
    if !c.database.is_empty() {
        url.push_str(&format!(";ConnCatalog={}", mssql_jdbc_val(&c.database)));
    }
    if let Some(s) = &schema {
        url.push_str(&format!(";ConnSchema={}", mssql_jdbc_val(s)));
    }
    url.push_str(";UID=token");
    if let Some(t) = &tok {
        url.push_str(&format!(";PWD={}", t.esc(mssql_jdbc_val)));
    }
    b.push("jdbc", "JDBC (Databricks)", url, notes.clone());

    let tok = b.s("token");
    let odbc = pairs(
        &[
            ("Driver", Some(V::R("{Simba Spark ODBC Driver}".into()))),
            ("Host", Some(V::t(host.clone()))),
            ("Port", Some(V::t("443"))),
            ("HTTPPath", Some(V::t(http_path.clone()))),
            ("SSL", Some(V::t("1"))),
            ("ThriftTransport", Some(V::t("2"))),
            ("AuthMech", Some(V::t("3"))),
            ("UID", Some(V::t("token"))),
            ("PWD", tok),
            ("Catalog", some_t(&c.database)),
            ("Schema", schema.clone().map(V::t)),
        ],
        odbc_val,
    );
    let mut on = notes.clone();
    on.push(note("odbc_driver_name", &[("driver", "Simba Spark ODBC Driver")]));
    b.push("odbc", "ODBC", odbc, on);
    b.sqlalchemy(&[], notes.clone());

    let tok = b.s("token");
    let mut gq: Vec<(&str, V)> = Vec::new();
    if !c.database.is_empty() {
        gq.push(("catalog", V::t(c.database.clone())));
    }
    if let Some(s) = &schema {
        gq.push(("schema", V::t(s.clone())));
    }
    b.push("go", "Go (databricks-sql-go)", format!("token:{}@{host}:443{}{}", tok.map(|t| t.esc(pct)).unwrap_or_default(), http_path, query(&gq)), notes.clone());

    let tok = b.s("token");
    let cfg = obj(vec![("host", js(&host)), ("path", js(&http_path)), ("token", jv(tok))]);
    b.push("node", "Node.js (@databricks/sql) · JSON", pretty(&cfg), notes);
}

fn trino(b: &mut B) {
    let c = b.c;
    let presto = c.driver == "presto";
    let schema = c.opt("schema").map(str::to_string);
    let pw = |b: &B| if c.encrypt { b.pw() } else { None };
    let mut path = String::new();
    if !c.database.is_empty() {
        path = format!("/{}", pct(&c.database));
        if let Some(s) = &schema {
            path.push_str(&format!("/{}", pct(s)));
        }
    }
    let p = pw(b);
    let q: Vec<(&str, V)> = if c.encrypt { vec![("SSL", V::t("true"))] } else { vec![] };
    let scheme = if presto { "presto" } else { "trino" };
    let jn = jdbc_notes(&c.user, p.is_some());
    b.push("jdbc", "JDBC", jdbc_props(format!("jdbc:{scheme}://{}{path}{}", c.hp(), query(&q)), &c.user, p), jn);
    b.sqlalchemy(&[], vec![]);
    if !presto {
        let p = pw(b);
        let mut gq: Vec<(&str, V)> = Vec::new();
        if !c.database.is_empty() {
            gq.push(("catalog", V::t(c.database.clone())));
        }
        if let Some(s) = &schema {
            gq.push(("schema", V::t(s.clone())));
        }
        let s = if c.encrypt { "https" } else { "http" };
        b.push("go", "Go (trino-go-client)", format!("{s}://{}{}{}", userinfo(&c.user, p.as_ref()), c.hp(), query(&gq)), vec![]);
    }
}

fn flight(b: &mut B) {
    let c = b.c;
    let dremio = c.driver == "dremio";
    let port = if dremio { 32010 } else { c.port };
    let scheme = if c.encrypt { "grpc+tls" } else { "grpc" };
    let at = hp(c.host_or_localhost(), port);
    let mut notes = Vec::new();
    if dremio {
        notes.push(note("other_port", &[("port", "32010")]));
    }
    let n = if c.user.is_empty() { vec![note("token_apart", &[])] } else { vec![note("credentials_apart", &[])] };
    let mut un = notes.clone();
    un.extend(n);
    b.push("uri", "URI (Arrow Flight SQL)", format!("{scheme}://{at}"), un);
    let mut q: Vec<(&str, V)> = vec![("useEncryption", V::t(if c.encrypt { "true" } else { "false" }))];
    if c.encrypt && c.trust {
        q.push(("disableCertificateVerification", V::t("true")));
    }
    if c.user.is_empty() {
        if let Some(t) = b.s("token") {
            q.push(("token", t));
        }
        b.push("jdbc", "JDBC (Arrow Flight SQL)", format!("jdbc:arrow-flight-sql://{at}{}", query(&q)), notes);
    } else {
        let pw = b.pw();
        let mut jn = notes;
        jn.extend(jdbc_notes(&c.user, pw.is_some()));
        b.push("jdbc", "JDBC (Arrow Flight SQL)", jdbc_props(format!("jdbc:arrow-flight-sql://{at}{}", query(&q)), &c.user, pw), jn);
    }
}

fn drill(b: &mut B) {
    let c = b.c;
    let pw = b.pw();
    b.push("uri", "URL (REST)", http_url(c, &c.user, pw.as_ref()), vec![]);
    let pw = b.pw();
    let mut jn = vec![note("other_port", &[("port", "31010")])];
    jn.extend(jdbc_notes(&c.user, pw.is_some()));
    let url = format!("jdbc:drill:drillbit={}{}", hp(c.host_or_localhost(), 31010), if c.database.is_empty() { String::new() } else { format!(";schema={}", c.database) });
    b.push("jdbc", "JDBC (drill-jdbc)", jdbc_props(url, &c.user, pw), jn);
    b.sqlalchemy(&[], vec![]);
}

fn phoenix(b: &mut B) {
    let c = b.c;
    let scheme = if c.encrypt { "https" } else { "http" };
    let path = c.opt("path").map(|p| format!("/{}", p.trim_start_matches('/'))).unwrap_or_default();
    let url = format!("{scheme}://{}{path}", c.hp());
    let ser = c.opt("serialization").unwrap_or(if c.driver == "phoenix" { "protobuf" } else { "json" }).to_ascii_uppercase();
    b.push("uri", "URL (Avatica)", url.clone(), vec![]);
    let jdbc = if c.driver == "phoenix" { format!("jdbc:phoenix:thin:url={url};serialization={ser}") } else { format!("jdbc:avatica:remote:url={url};serialization={ser}") };
    let pw = b.pw();
    let jn = jdbc_notes(&c.user, pw.is_some());
    b.push("jdbc", "JDBC (thin client)", jdbc_props(jdbc, &c.user, pw), jn);
    b.sqlalchemy(&[], vec![]);
}

fn hana(b: &mut B) {
    let c = b.c;
    let pw = b.pw();
    let mut q: Vec<(&str, V)> = Vec::new();
    if !c.database.is_empty() {
        q.push(("databaseName", V::t(c.database.clone())));
    }
    if c.encrypt {
        q.push(("encrypt", V::t("true")));
        if c.trust {
            q.push(("validateCertificate", V::t("false")));
        }
    }
    let jn = jdbc_notes(&c.user, pw.is_some());
    b.push("jdbc", "JDBC (ngdbc)", jdbc_props(format!("jdbc:sap://{}/{}", c.hp(), query(&q)), &c.user, pw), jn);
    let pw = b.pw();
    let ado = pairs(&[("Server", Some(V::t(c.hp()))), ("UserName", some_t(&c.user)), ("Password", pw), ("databaseName", some_t(&c.database)), ("encrypt", c.encrypt.then(|| V::t("true")))], ado_val);
    b.push("adonet", "ADO.NET (Sap.Data.Hana)", ado, vec![]);
    b.sqlalchemy(&[], vec![]);
    let pw = b.pw();
    let odbc = pairs(
        &[
            ("DRIVER", Some(V::R("{HDBODBC}".into()))),
            ("SERVERNODE", Some(V::t(c.hp()))),
            ("DATABASENAME", some_t(&c.database)),
            ("UID", some_t(&c.user)),
            ("PWD", pw),
            ("ENCRYPT", c.encrypt.then(|| V::t("TRUE"))),
            ("sslValidateCertificate", (c.encrypt && c.trust).then(|| V::t("FALSE"))),
        ],
        odbc_val,
    );
    b.push("odbc", "ODBC", odbc, vec![note("odbc_driver_name", &[("driver", "HDBODBC")])]);
    let pw = b.pw();
    let cfg = obj(vec![("serverNode", js(&c.hp())), ("uid", js(&c.user)), ("pwd", jv(pw)), ("databaseName", js(&c.database)), ("encrypt", if c.encrypt { json!(true) } else { Value::Null })]);
    b.push("node", "Node.js (@sap/hana-client) · JSON", pretty(&cfg), vec![]);
}

fn firebird(b: &mut B) {
    let c = b.c;
    let role = c.opt("role").map(str::to_string);
    let charset = c.opt("charset").map(str::to_string);
    let pw = b.pw();
    let mut q: Vec<(&str, V)> = Vec::new();
    if let Some(cs) = &charset {
        q.push(("encoding", V::t(cs.clone())));
    }
    if let Some(r) = &role {
        q.push(("roleName", V::t(r.clone())));
    }
    let jn = jdbc_notes(&c.user, pw.is_some());
    b.push("jdbc", "JDBC (Jaybird)", jdbc_props(format!("jdbc:firebirdsql://{}/{}{}", c.hp(), c.database, query(&q)), &c.user, pw), jn);
    let pw = b.pw();
    let ado = pairs(
        &[("DataSource", Some(V::t(c.host_or_localhost()))), ("Port", Some(V::t(c.port.to_string()))), ("Database", some_t(&c.database)), ("User", some_t(&c.user)), ("Password", pw), ("Role", role.clone().map(V::t)), ("Charset", charset.clone().map(V::t))],
        ado_val,
    );
    b.push("adonet", "ADO.NET (FirebirdSql.Data.FirebirdClient)", ado, vec![]);
    b.sqlalchemy(&[], vec![]);
    let pw = b.pw();
    let odbc = pairs(
        &[
            ("DRIVER", Some(V::R("{Firebird/InterBase(r) driver}".into()))),
            ("DBNAME", Some(V::t(format!("{}/{}:{}", c.host_or_localhost(), c.port, c.database)))),
            ("UID", some_t(&c.user)),
            ("PWD", pw),
            ("ROLE", role.map(V::t)),
            ("CHARSET", charset.map(V::t)),
        ],
        odbc_val,
    );
    b.push("odbc", "ODBC", odbc, vec![note("odbc_driver_name", &[("driver", "Firebird/InterBase(r) driver")])]);
}

fn tdengine(b: &mut B) {
    let c = b.c;
    let pw = b.pw();
    let jn = jdbc_notes(&c.user, pw.is_some());
    b.push("jdbc", "JDBC (taos-jdbcdriver, REST)", jdbc_props(format!("jdbc:TAOS-RS://{}/{}", c.hp(), c.database), &c.user, pw), jn);
    b.sqlalchemy(&[], vec![]);
    let pw = b.pw();
    let creds = match (c.user.is_empty(), &pw) {
        (true, _) => String::new(),
        (false, None) => format!("{}@", c.user),
        (false, Some(p)) => format!("{}:{}@", c.user, p.esc(|s| s.to_string())),
    };
    b.push("go", "Go (driver-go, taosRestful)", format!("{creds}http({})/{}", c.hp(), c.database), vec![]);
}

#[cfg(test)]
mod tests;
