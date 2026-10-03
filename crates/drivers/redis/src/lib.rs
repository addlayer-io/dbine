//! Redis over RESP with the `redis` crate, and the servers that speak the
//! same protocol (Valkey, Dragonfly). A session is one multiplexed
//! connection to one logical database (`db0`…`db15`); keys are the
//! explorer's objects. Scripts are one command per line (see [`command`]),
//! and each reply becomes a table (see [`shape`]).

mod backup;
mod command;
mod ddl;
mod monitor;
mod permissions;
mod processes;
mod profiler;
mod security;
mod shape;
mod steps;
mod transfer;

use dbine_driver::{
    async_trait, kinds, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject, DdlParts, DesignerSpec,
    Driver, DriverInfo, Error, Family, Field, FieldKind, KeyEntry, KeyPage, KeyScan, KeySearch, KeySyntax, Language,
    ObjectKindInfo, ObjectRef, MonitorSnapshot, QueryOutcome, ResultColumn, Result, ScriptError, Session, TableSchema,
};
use dbine_driver::keys::{glob_escape, has_wildcards};
use redis::aio::MultiplexedConnection;
use steps::Step;
use redis::{AsyncConnectionConfig, ConnectionAddr, ErrorKind, IntoConnectionInfo, RedisConnectionInfo, RedisError, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Keys `list_objects` returns at most (SCAN stops there); the explorer
/// searches with `scan_keys` instead.
const MAX_KEYS: usize = 5000;
const SCAN_COUNT: usize = 1000;
/// Keys one page of a key search looks at, at most: a selective pattern
/// over millions of keys comes back with what it found so far and a cursor.
const SCAN_BUDGET: u64 = 50_000;
/// Types the key search filters by, as the explorer names them.
const KEY_TYPES: &[&str] = &["string", "hash", "list", "set", "zset", "stream", "json"];

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![
        Arc::new(RedisDriver { info: info("redis", "Redis") }),
        Arc::new(RedisDriver { info: info("valkey", "Valkey") }),
        Arc::new(RedisDriver { info: info("dragonfly", "Dragonfly") }),
    ]
}

fn info(id: &'static str, name: &'static str) -> DriverInfo {
    DriverInfo {
        id,
        name,
        family: Family::KeyValue,
        language: Language::Redis,
        dialect: "",
        default_port: 6379,
        fields: vec![
            Field::host(),
            Field::port().placeholder("6379"),
            Field::new("database", "Base de datos", FieldKind::Number)
                .placeholder("0")
                .help("Índice de la base lógica (0–15 en una instalación estándar)."),
            Field::username().help("Solo con ACL (Redis 6 o superior); vacío = default."),
            Field::password(),
            Field::encrypt(),
            Field::trust_cert(),
            Field::read_only(),
        ],
        databases_label: "Bases de datos",
        has_schemas: false,
        object_kinds: vec![ObjectKindInfo::new(kinds::KEY, "Claves", true, true, true)],
    }
}

pub struct RedisDriver {
    info: DriverInfo,
}

/// `db3`, `3` or empty (= 0).
fn db_index(name: &str) -> Result<i64> {
    let n = name.trim();
    let n = n.strip_prefix("db").unwrap_or(n);
    if n.is_empty() {
        return Ok(0);
    }
    n.parse().map_err(|_| Error::Connect(format!("'{name}' no es una base de Redis (db0, db1…)")))
}

#[async_trait]
impl Driver for RedisDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    /// As redis-cli with a piped script: a failed command doesn't stop the script (the tab's
    /// toggle overrides it).
    fn script_defaults(&self) -> dbine_driver::ScriptDefaults {
        dbine_driver::ScriptDefaults { continue_on_error: true, ..dbine_driver::ScriptDefaults::for_language(self.info().language) }
    }

    /// Redis' databases are a fixed, numbered set (`databases` in the
    /// config): they aren't created or dropped (FLUSHDB empties one, from
    /// the console). Keys have no relations. Clients are listed with
    /// `CLIENT LIST`, closed with `CLIENT KILL` and a blocking command is
    /// cancelled with `CLIENT UNBLOCK`, which Dragonfly lacks (see
    /// [`processes`]).
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            create_database: false,
            drop_database: false,
            foreign_keys: false,
            monitor: true,
            kill_session: true,
            processes: true,
            cancel_query: self.info.id != "dragonfly",
            ..Default::default()
        }
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    /// Pipelined `HSET`s (see [`transfer`]).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    /// ACL users (Redis 6+, Valkey, Dragonfly): no roles.
    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec())
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(action)
    }

    /// An RDB snapshot of the whole server (BGSAVE / SAVE).
    fn backup(&self) -> Option<dbine_driver::BackupSpec> {
        Some(backup::spec())
    }

    fn backup_script(&self, action: &dbine_driver::BackupAction) -> Result<String> {
        backup::script(action)
    }

    fn key_search(&self) -> Option<KeySearch> {
        Some(KeySearch { syntax: KeySyntax::Glob, separator: ":", types: KEY_TYPES.to_vec(), case_sensitive: true })
    }

    /// No schema to sync: keys are data, not tables.
    fn sync_script(&self, _changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        Err(Error::Unsupported("Redis no tiene esquema: las keys son datos (no tablas con columnas), así que no hay cambios de esquema que aplicar; para llevar keys de una base a otra usá la copia de datos o la exportación".into()))
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(ddl::designer(self.info.id))
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        ddl::templates()
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        ddl::table_ddl(table, parts)
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<serde_json::Value>]) -> Result<String> {
        ddl::insert_script(target, columns, rows)
    }

    fn update_script(&self, target: &ObjectRef, changes: &[dbine_driver::RowChange]) -> Result<String> {
        ddl::update_script(target, changes)
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, serde_json::Value)>]) -> Result<String> {
        ddl::delete_script(target, keys)
    }

    /// Redis commands read whole keys (a string, a hash, a list…): nothing
    /// to filter by column on the server.
    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        if filters.is_empty() {
            return Ok(browse.to_string());
        }
        Err(Error::Unsupported("Redis lee claves enteras: no filtra por columna en el servidor".into()))
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let db = db_index(database.filter(|d| !d.is_empty()).unwrap_or(&cfg.database))?;
        let host = if cfg.host.trim().is_empty() { "localhost".to_string() } else { cfg.host.trim().to_string() };
        let port = cfg.port_or(6379);
        let addr = if cfg.encrypt {
            // redis' rustls needs a process-wide crypto provider when more
            // than one is compiled in; a second install is a no-op error.
            let _ = rustls::crypto::ring::default_provider().install_default();
            ConnectionAddr::TcpTls { host, port, insecure: cfg.trust_server_certificate, tls_params: None }
        } else {
            ConnectionAddr::Tcp(host, port)
        };
        let mut redis_info = RedisConnectionInfo::default().set_db(db);
        if let Some(u) = cfg.username.as_deref().filter(|u| !u.is_empty()) {
            redis_info = redis_info.set_username(u);
        }
        if let Some(p) = cfg.password.as_deref().filter(|p| !p.is_empty()) {
            redis_info = redis_info.set_password(p);
        }
        let conn_info = addr.into_connection_info().map_err(connect_err)?.set_redis_settings(redis_info);
        let client = redis::Client::open(conn_info).map_err(connect_err)?;
        let config = AsyncConnectionConfig::new()
            .set_connection_timeout(Some(Duration::from_secs(15)))
            .set_response_timeout(Some(Duration::from_secs(300)));
        let conn = tokio::time::timeout(Duration::from_secs(20), client.get_multiplexed_async_connection_with_config(&config))
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado".into()))?
            .map_err(connect_err)?;
        Ok(Box::new(RedisSession { conn, client, db, read_only: cfg.read_only, types: HashMap::new(), profiler: None, scan_type: true, multi: false, queued: 0, queued_db: None }))
    }
}

fn is_auth(e: &RedisError) -> bool {
    e.kind() == ErrorKind::AuthenticationFailed || matches!(e.code(), Some("WRONGPASS" | "NOAUTH" | "NOPERM"))
}

fn connect_err(e: RedisError) -> Error {
    if is_auth(&e) {
        Error::AuthFailed(e.to_string())
    } else {
        Error::Connect(e.to_string())
    }
}

/// Servers without ACLs (Redis before 6, services that disable the
/// command) refuse `ACL …`.
fn no_acl(e: Error) -> Error {
    match e {
        Error::Query(m) if m.contains("unknown command") || m.contains("ERR unknown") => Error::Unsupported(format!(
            "Este servidor no tiene ACL (Redis 6 o superior) o no permite el comando ACL: no hay usuarios que administrar ({m})."
        )),
        other => other,
    }
}

/// A command the server refused, with its error code (`ERR`, `WRONGTYPE`…).
fn command_err(e: RedisError) -> Error {
    let code = e.code().map(str::to_string);
    match err(e) {
        Error::Query(m) => {
            let mut se = ScriptError::new(m);
            if let Some(c) = code {
                se = se.with_code(c);
            }
            Error::Statement(Box::new(se))
        }
        other => other,
    }
}

pub(crate) fn err(e: RedisError) -> Error {
    if is_auth(&e) {
        Error::AuthFailed(e.to_string())
    } else if e.is_io_error() || e.is_connection_refusal() || e.is_timeout() {
        Error::Connect(e.to_string())
    } else {
        Error::Query(e.to_string())
    }
}

pub struct RedisSession {
    conn: MultiplexedConnection,
    /// For the profiler's own `MONITOR` connection.
    client: redis::Client,
    db: i64,
    read_only: bool,
    /// Key types seen by `list_objects`/`columns`: `browse_query` is sync
    /// and the right read command depends on the type.
    types: HashMap<String, String>,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
    /// The server takes `SCAN … TYPE` (Redis 6+); when it refuses, the key
    /// search filters by type itself.
    scan_type: bool,
    /// A `MULTI` is open: commands are queued until `EXEC` / `DISCARD`.
    multi: bool,
    /// Commands queued in the open `MULTI`.
    queued: usize,
    /// The last `SELECT` queued in the open `MULTI` (its place in the
    /// queue and the database): it only switches at `EXEC`.
    queued_db: Option<(usize, i64)>,
}

impl RedisSession {
    async fn run(&mut self, args: &[&[u8]]) -> Result<Value> {
        self.send(args).await.map_err(err)
    }

    async fn send(&mut self, args: &[&[u8]]) -> std::result::Result<Value, RedisError> {
        let mut c = redis::cmd(&String::from_utf8_lossy(args[0]));
        for a in &args[1..] {
            c.arg(*a);
        }
        c.query_async(&mut self.conn).await
    }

    /// The server's reply as it came: only an error reply as a whole is an
    /// `Err` (`query_async` also fails on an error nested in an array, as
    /// one command of an `EXEC`).
    async fn send_raw(&mut self, args: &[&[u8]]) -> std::result::Result<Value, RedisError> {
        use redis::aio::ConnectionLike;
        let mut c = redis::cmd(&String::from_utf8_lossy(args[0]));
        for a in &args[1..] {
            c.arg(*a);
        }
        match self.conn.req_packed_command(&c).await? {
            Value::ServerError(e) => Err(e.into()),
            v => Ok(v),
        }
    }

    /// One editor command into `out`.
    async fn command(&mut self, cmd: &[Vec<u8>], max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let name = String::from_utf8_lossy(&cmd[0]).to_ascii_uppercase();
        if command::is_streaming(&name) {
            return Err(Error::Unsupported(format!("El editor no admite {name}: deja la conexión ocupada.")));
        }
        let args: Vec<&[u8]> = cmd.iter().map(Vec::as_slice).collect();
        let reply = self.send_raw(&args).await.map_err(command_err);
        // A refused command inside MULTI makes EXEC fail (EXECABORT): the
        // transaction is over either way.
        let was_multi = self.multi;
        let mut switch_to = None;
        match (name.as_str(), &reply) {
            ("MULTI", Ok(_)) => {
                self.multi = true;
                self.queued = 0;
                self.queued_db = None;
            }
            ("EXEC" | "DISCARD", _) => {
                self.multi = false;
                // EXEC switches to the last queued SELECT that didn't fail.
                if let (Some((at, db)), "EXEC", Ok(Value::Array(items))) = (self.queued_db.take(), name.as_str(), &reply) {
                    if !matches!(items.get(at), Some(Value::ServerError(_))) {
                        switch_to = Some(db);
                    }
                }
            }
            _ => {}
        }
        let reply = reply?;
        let select = (name == "SELECT").then(|| cmd.get(1).and_then(|d| std::str::from_utf8(d).ok()?.parse::<i64>().ok())).flatten();
        if was_multi && self.multi && name != "MULTI" {
            if let Some(db) = select {
                self.queued_db = Some((self.queued, db));
            }
            self.queued += 1;
        } else if let Some(db) = select {
            switch_to = Some(db);
        }
        if let Some(db) = switch_to {
            self.db = db;
            self.types.clear();
            // The tab's database selector follows it.
            out.database = Some(format!("db{db}"));
        }
        if name == "EXEC" {
            // As redis-cli: a command that failed inside the transaction is
            // an error line of the reply; the others were applied.
            if let Value::Array(items) = &reply {
                let failed = items.iter().filter(|v| matches!(v, Value::ServerError(_))).count();
                if failed > 0 {
                    out.warning(format!(
                        "EXEC: {failed} de {} comandos de la transacción fallaron; los demás se aplicaron.",
                        items.len()
                    ));
                }
            }
        }
        let table = shape::shape(cmd, reply);
        out.begin_result(table.columns.iter().map(|c| ResultColumn { name: c.clone(), type_name: String::new() }).collect());
        if let Some(m) = table.message {
            out.info(m);
        }
        for row in table.rows {
            out.push_row(row, max_rows);
        }
        Ok(())
    }

    async fn key_type(&mut self, key: &str) -> Result<String> {
        let t = shape::text_of(&self.run(&[b"TYPE", key.as_bytes()]).await?);
        if t != "none" {
            self.types.insert(key.to_string(), t.clone());
        }
        Ok(t)
    }

    /// A number from a command, or `None` when the server refuses it
    /// (commands renamed or disabled on managed services).
    async fn int(&mut self, args: &[&[u8]]) -> Option<i64> {
        match self.run(args).await.ok()? {
            Value::Int(i) => Some(i),
            _ => None,
        }
    }
}

fn col(name: &str, data_type: &str, primary_key: bool) -> ColumnInfo {
    ColumnInfo {
        name: name.into(),
        data_type: data_type.into(),
        nullable: false,
        primary_key,
        auto_increment: false,
        default_value: None,
    }
}

/// A type as `TYPE` / `SCAN … TYPE` name it, from the explorer's name.
fn server_type(t: &str) -> &str {
    match t {
        "json" => "ReJSON-RL",
        "timeseries" => "TSDB-TYPE",
        other => other,
    }
}

/// Redis' name for a type (`TYPE`), as the explorer shows it.
fn type_label(t: &str) -> &str {
    match t {
        "ReJSON-RL" => "json",
        "TSDB-TYPE" => "timeseries",
        other => other,
    }
}

#[async_trait]
impl Session for RedisSession {
    async fn server_version(&mut self) -> Result<String> {
        let text = shape::text_of(&self.run(&[b"INFO", b"server"]).await?);
        let field = |k: &str| {
            text.lines().find_map(|l| l.trim().strip_prefix(k).and_then(|r| r.strip_prefix(':')).map(str::to_string))
        };
        Ok(if let Some(v) = field("valkey_version") {
            format!("Valkey {v}")
        } else if let Some(v) = field("dragonfly_version") {
            format!("Dragonfly {}", v.trim_start_matches("df-v"))
        } else {
            let mode = field("redis_mode").filter(|m| m != "standalone").map(|m| format!(" ({m})")).unwrap_or_default();
            format!("Redis {}{mode}", field("redis_version").unwrap_or_default())
        })
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let n = match self.run(&[b"CONFIG", b"GET", b"databases"]).await {
            Ok(v) => {
                let t = shape::shape(&[b"CONFIG".to_vec(), b"GET".to_vec()], v);
                t.rows.first().and_then(|r| r.get(1)).and_then(|v| v.as_str()).and_then(|s| s.parse().ok()).unwrap_or(16)
            }
            Err(_) => 16,
        };
        let n = n.clamp(1, 1024).max(self.db as usize + 1);
        Ok((0..n).map(|i| format!("db{i}")).collect())
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let mut keys: Vec<Vec<u8>> = Vec::new();
        let mut cursor = String::from("0");
        loop {
            let reply = self.run(&[b"SCAN", cursor.as_bytes(), b"COUNT", SCAN_COUNT.to_string().as_bytes()]).await?;
            let Value::Array(mut parts) = reply else { break };
            if parts.len() != 2 {
                break;
            }
            let batch = parts.pop().expect("two parts");
            cursor = shape::text_of(&parts[0]);
            if let Value::Array(items) = batch {
                keys.extend(items.into_iter().filter_map(|k| match k {
                    Value::BulkString(b) => Some(b),
                    _ => None,
                }));
            }
            if cursor == "0" || keys.len() >= MAX_KEYS {
                break;
            }
        }
        keys.sort();
        keys.dedup();
        keys.truncate(MAX_KEYS);

        // Every key's type in one round trip, for `browse_query`.
        let mut pipe = redis::pipe();
        for k in &keys {
            pipe.cmd("TYPE").arg(k.as_slice());
        }
        let types: Vec<Value> = if keys.is_empty() { Vec::new() } else { pipe.query_async(&mut self.conn).await.map_err(err)? };
        self.types.clear();
        let mut out = Vec::with_capacity(keys.len());
        for (k, t) in keys.into_iter().zip(types) {
            let name = String::from_utf8_lossy(&k).into_owned();
            self.types.insert(name.clone(), shape::text_of(&t));
            out.push(DbObject { kind: kinds::KEY.into(), schema: None, name, parent: None });
        }
        Ok(out)
    }

    async fn scan_keys(&mut self, scan: &KeyScan) -> Result<KeyPage> {
        let text = scan.pattern.trim();
        let literal = !text.is_empty() && !has_wildcards(text);
        // Plain text finds the keys that contain it; wildcards go as typed.
        let pattern = match text {
            "" => "*".to_string(),
            t if literal => format!("*{}*", glob_escape(t)),
            t => t.to_string(),
        };
        let want = scan.count.clamp(1, 5000) as usize;
        let wanted_type = scan.key_type.as_deref().filter(|t| !t.is_empty()).map(server_type);
        let first = scan.cursor.is_none();
        let mut cursor = scan.cursor.clone().unwrap_or_else(|| "0".into());
        let mut names: Vec<Vec<u8>> = Vec::new();
        // The exact key first, when there is one.
        if first && literal && self.int(&[b"EXISTS", text.as_bytes()]).await == Some(1) {
            names.push(text.as_bytes().to_vec());
        }
        let count = SCAN_COUNT.to_string();
        let mut scanned = 0u64;
        loop {
            let mut args: Vec<&[u8]> = vec![b"SCAN", cursor.as_bytes(), b"MATCH", pattern.as_bytes(), b"COUNT", count.as_bytes()];
            let by_server = wanted_type.filter(|_| self.scan_type);
            if let Some(t) = by_server {
                args.extend([b"TYPE".as_slice(), t.as_bytes()]);
            }
            let reply = match self.run(&args).await {
                // Before Redis 6 SCAN has no TYPE: filter here instead.
                Err(Error::Query(_)) if by_server.is_some() => {
                    self.scan_type = false;
                    continue;
                }
                r => r?,
            };
            let Value::Array(mut parts) = reply else { break };
            if parts.len() != 2 {
                break;
            }
            let batch = parts.pop().expect("two parts");
            cursor = shape::text_of(&parts[0]);
            scanned += SCAN_COUNT as u64;
            if let Value::Array(items) = batch {
                names.extend(items.into_iter().filter_map(|k| match k {
                    Value::BulkString(b) => Some(b),
                    _ => None,
                }));
            }
            if cursor == "0" || names.len() >= want || scanned >= SCAN_BUDGET {
                break;
            }
        }
        // SCAN may repeat a key; keep the first time it came.
        let mut seen = std::collections::HashSet::new();
        names.retain(|k| seen.insert(k.clone()));

        // Every key's type and time to live in one round trip.
        let mut pipe = redis::pipe();
        for k in &names {
            pipe.cmd("TYPE").arg(k.as_slice()).cmd("PTTL").arg(k.as_slice());
        }
        let meta: Vec<Value> = if names.is_empty() { Vec::new() } else { pipe.query_async(&mut self.conn).await.map_err(err)? };
        let mut keys = Vec::with_capacity(names.len());
        for (k, m) in names.into_iter().zip(meta.chunks(2)) {
            let t = shape::text_of(&m[0]);
            if t == "none" || wanted_type.is_some_and(|w| w != t) {
                continue;
            }
            let name = String::from_utf8_lossy(&k).into_owned();
            self.types.insert(name.clone(), t.clone());
            let ttl_ms = match m.get(1) {
                Some(Value::Int(ms)) if *ms >= 0 => Some(*ms),
                _ => None,
            };
            keys.push(KeyEntry { name, key_type: Some(type_label(&t).to_string()), ttl_ms });
        }
        let total = if first { self.int(&[b"DBSIZE"]).await.map(|n| n.max(0) as u64) } else { None };
        Ok(KeyPage { keys, cursor: (cursor != "0").then_some(cursor), total, scanned })
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let t = self.key_type(&obj.name).await?;
        Ok(match t.as_str() {
            "string" => vec![col("value", "string", false)],
            "hash" => vec![col("field", "string", true), col("value", "string", false)],
            "list" => vec![col("value", "string", false)],
            "set" => vec![col("value", "string", true)],
            "zset" => vec![col("member", "string", true), col("score", "double", false)],
            "stream" => {
                let v = self.run(&[b"XRANGE", obj.name.as_bytes(), b"-", b"+", b"COUNT", b"100"]).await?;
                let table = shape::shape(&[b"XRANGE".to_vec()], v);
                let mut cols = vec![col("id", "stream id", true)];
                cols.extend(table.columns.iter().skip(1).map(|c| ColumnInfo { nullable: true, ..col(c, "string", false) }));
                cols
            }
            "ReJSON-RL" => vec![col("value", "json", false)],
            "none" => Vec::new(),
            other => vec![col("value", type_label(other), false)],
        })
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let key = obj.name.as_bytes();
        let t = self.key_type(&obj.name).await?;
        if t == "none" {
            return Ok(None);
        }
        let mut lines = vec![format!("TYPE      {}", type_label(&t))];
        let ttl = self.int(&[b"TTL", key]).await.unwrap_or(-1);
        lines.push(match ttl {
            -1 => "TTL       -1 (sin vencimiento)".to_string(),
            s => format!("TTL       {s} s"),
        });
        if let Some(bytes) = self.int(&[b"MEMORY", b"USAGE", key]).await {
            lines.push(format!("MEMORY    {bytes} bytes"));
        }
        if let Ok(v) = self.run(&[b"OBJECT", b"ENCODING", key]).await {
            lines.push(format!("ENCODING  {}", shape::text_of(&v)));
        }
        let len_cmd: Option<&[u8]> = match t.as_str() {
            "string" => Some(b"STRLEN"),
            "hash" => Some(b"HLEN"),
            "list" => Some(b"LLEN"),
            "set" => Some(b"SCARD"),
            "zset" => Some(b"ZCARD"),
            "stream" => Some(b"XLEN"),
            _ => None,
        };
        if let Some(c) = len_cmd {
            if let Some(n) = self.int(&[c, key]).await {
                let unit = if t == "string" { "bytes" } else { "elementos" };
                lines.push(format!("LENGTH    {n} {unit}"));
            }
        }
        Ok(Some(lines.join("\n")))
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        let k = command::quote_arg(&obj.name);
        let last = limit.max(1) - 1;
        match self.types.get(&obj.name).map(String::as_str) {
            Some("string") => format!("GET {k}"),
            Some("hash") => format!("HGETALL {k}"),
            Some("list") => format!("LRANGE {k} 0 {last}"),
            Some("set") => format!("SSCAN {k} 0 COUNT {limit}"),
            Some("zset") => format!("ZRANGE {k} 0 {last} WITHSCORES"),
            Some("stream") => format!("XRANGE {k} - + COUNT {limit}"),
            Some("ReJSON-RL") => format!("JSON.GET {k} $"),
            Some("TSDB-TYPE") => format!("TS.RANGE {k} - + COUNT {limit}"),
            _ => format!("TYPE {k}"),
        }
    }

    /// Keys aren't tables: there's no schema to draw or script, so the ER
    /// diagram and the script generator don't apply to Redis.
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        Ok(Vec::new())
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        self.snapshot().await
    }

    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        RedisSession::processes(self).await
    }

    async fn cancel_query(&mut self, id: &str) -> Result<()> {
        self.cancel(id).await
    }

    async fn kill_session(&mut self, id: &str) -> Result<()> {
        self.kill(id).await
    }

    async fn backups(&mut self, _database: Option<&str>) -> Result<Vec<dbine_driver::BackupEntry>> {
        backup::history(self).await
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        security::principals(self).await.map_err(no_acl)
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        security::grants(self, principal).await.map_err(no_acl)
    }

    async fn profiler_start(&mut self, opts: &dbine_driver::ProfilerOptions) -> Result<dbine_driver::ProfilerStarted> {
        if let Some(old) = self.profiler.take() {
            profiler::stop(old).await?;
        }
        let (state, started) = profiler::start(self, opts).await?;
        self.profiler = Some(state);
        Ok(started)
    }

    async fn profiler_poll(&mut self) -> Result<Vec<dbine_driver::ProfiledStatement>> {
        profiler::poll(self.profiler.as_mut().ok_or_else(|| Error::State("el profiler no está iniciado".into()))?)
    }

    async fn profiler_stop(&mut self) -> Result<()> {
        match self.profiler.take() {
            Some(state) => profiler::stop(state).await,
            None => Ok(()),
        }
    }

    async fn read_batches(&mut self, spec: &dbine_driver::transfer::ReadSpec, sink: dbine_driver::transfer::BatchSinkRef) -> Result<u64> {
        transfer::read(self, spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::transfer::LoadSpec,
        _columns: &[dbine_driver::transfer::TransferColumn],
        source: &mut dyn dbine_driver::transfer::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        transfer::load(self, spec, source, progress).await
    }

    /// One command per line, as redis-cli reads a script. A line that
    /// doesn't parse runs nothing; a refused command stops the script
    /// unless the editor run continues on errors, as redis-cli does (see
    /// `Step::end`).
    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let script = command::parse_placed(text).map_err(|(m, line, at)| Error::from(ScriptError::new(m).at_line(line).at_offset(at)))?;
        if self.read_only {
            if let Some(w) = script.iter().find(|c| !command::is_read(&c.args)) {
                return Err(Error::Query(format!(
                    "Conexión de solo lectura: se bloqueó el comando {}. Solo se permiten lecturas (GET, HGETALL, SCAN, INFO…).",
                    String::from_utf8_lossy(&w.args[0]).to_uppercase()
                )));
            }
        }
        let own = out.current_statement.is_none();
        for (i, c) in script.iter().enumerate() {
            let step = Step::start(out, own, i, c.start, c.line);
            let r = self.command(&c.args, max_rows, out).await;
            step.end(out, r)?;
        }
        if self.multi {
            out.warning("MULTI sigue abierto: los comandos siguientes se encolan hasta EXEC o DISCARD.");
        }
        Ok(())
    }

    /// `ACL DRYRUN` per action (see `permissions`); the database doesn't
    /// matter, ACL rules are server-wide.
    async fn permissions(&mut self, _database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_server_side_filters() {
        let f = dbine_driver::ColumnFilter { column: "value".into(), op: dbine_driver::FilterOp::Eq, values: vec![serde_json::json!("a")], sql: None };
        for d in drivers() {
            assert_eq!(d.filtered_browse("GET k", &[]).unwrap(), "GET k");
            assert!(matches!(d.filtered_browse("GET k", &[f.clone()]), Err(Error::Unsupported(_))));
        }
    }

    #[test]
    fn database_names() {
        assert_eq!(db_index("db3").unwrap(), 3);
        assert_eq!(db_index("5").unwrap(), 5);
        assert_eq!(db_index("").unwrap(), 0);
        assert!(db_index("users").is_err());
    }

    #[test]
    fn three_drivers_with_stable_ids() {
        let ids: Vec<_> = drivers().iter().map(|d| d.info().id).collect();
        assert_eq!(ids, ["redis", "valkey", "dragonfly"]);
    }
}
