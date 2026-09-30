//! etcd v3 through its JSON gRPC gateway (`POST /v3/kv/range`,
//! `/v3/kv/put`…, the same port as gRPC): no tonic / protobuf dependency.
//! Keys and values travel base64-encoded and 64-bit integers as strings
//! (proto3 JSON). With a user, the session authenticates at
//! `/v3/auth/authenticate` and sends the token in `Authorization`,
//! re-authenticating once when it expires.
//!
//! The editor speaks etcdctl commands (see [`command`]). The explorer
//! searches keys by prefix a page at a time (`scan_keys`), under the
//! connection's prefix if it has one; the monitor reads
//! `/v3/maintenance/status`, the member list and the Prometheus `/metrics`.

mod backup;
mod command;
mod ddl;
mod prom;
mod security;
mod permissions;
mod transfer;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use command::{lease_hex, parse_lease, prefix_end, quote_arg, Command};
use dbine_driver::{
    async_trait, json_bytes, json_i64, kinds, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject, DdlParts,
    DesignerSpec, Driver, DriverInfo, Error, Family, Field, FieldKind, KeyEntry, KeyPage, KeyScan, KeySearch, KeySyntax,
    Language, Metric, MetricUnit, MonitorSnapshot,
    MonitorTable, ObjectKindInfo, ObjectRef, QueryOutcome, Result, ResultColumn, Session, TableSchema,
};
use serde_json::{json, Value};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

/// Keys `list_objects` returns at most (the explorer searches with `scan_keys`).
const MAX_KEYS: i64 = 10_000;

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(EtcdDriver { info: info() })]
}

fn info() -> DriverInfo {
    DriverInfo {
        id: "etcd",
        name: "etcd",
        family: Family::KeyValue,
        language: Language::Redis,
        dialect: "etcd",
        default_port: 2379,
        fields: vec![
            Field::host(),
            Field::port().placeholder("2379"),
            Field::username().help("Solo si la autenticación de etcd está habilitada."),
            Field::password(),
            Field::new("prefix", "Prefijo de claves", FieldKind::Text)
                .placeholder("(todas)")
                .help("El explorador lista solo las claves que empiezan con este prefijo.")
                .advanced(),
            Field::encrypt(),
            Field::trust_cert(),
            Field::read_only(),
        ],
        databases_label: "",
        has_schemas: false,
        object_kinds: vec![ObjectKindInfo::new(kinds::KEY, "Claves", true, true, true)],
    }
}

const HELP: &str = "Comandos de etcdctl, uno por línea:\n\
get <clave> [--prefix] [--limit=N] [--rev=N] [--keys-only] [--count-only]\n\
put <clave> <valor> [--lease=<id>] [--ttl=<segundos>]\n\
del <clave> [--prefix]\n\
lease grant <ttl> | lease revoke <id> | lease timetolive <id> [--keys] | lease list\n\
member list | endpoint status | alarm list | compaction <revisión>\n\
user list | user add <usuario> <contraseña> | user passwd <usuario> <contraseña> | user delete <usuario> | user get <usuario>\n\
user grant-role <usuario> <rol> | user revoke-role <usuario> <rol>\n\
role list | role add <rol> | role delete <rol> | role get <rol>\n\
role grant-permission <rol> read|write|readwrite <clave> [--prefix] | role revoke-permission <rol> <clave> [--prefix]\n\
auth status | auth enable | auth disable\n\
snapshot save <archivo en esta computadora>\n\
Los argumentos con espacios van entre comillas; # empieza un comentario.";

pub struct EtcdDriver {
    info: DriverInfo,
}

#[async_trait]
impl Driver for EtcdDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn query_help(&self) -> &'static str {
        HELP
    }

    /// Transactions of puts (see [`transfer`]).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    /// etcd has one keyspace: nothing to create or drop.
    fn capabilities(&self) -> Capabilities {
        Capabilities { monitor: true, ..Default::default() }
    }

    /// Users and roles of the Auth API (enforced only after `auth enable`).
    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec())
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(action)
    }

    /// Keys sort by bytes, so a search is a range: the keys that start
    /// with the text (after the connection's prefix).
    /// A snapshot of the whole keyspace, streamed to a local file.
    fn backup(&self) -> Option<dbine_driver::BackupSpec> {
        Some(backup::spec())
    }

    fn backup_script(&self, action: &dbine_driver::BackupAction) -> Result<String> {
        backup::script(action)
    }

    fn key_search(&self) -> Option<KeySearch> {
        Some(KeySearch { syntax: KeySyntax::Prefix, separator: "/", types: Vec::new(), case_sensitive: true })
    }

    /// No schema to sync: keys are opaque key-value pairs.
    fn sync_script(&self, _changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        Err(Error::Unsupported("etcd no tiene esquema: guarda pares clave-valor con valores opacos, así que no hay cambios de esquema que aplicar; para llevar claves de un clúster a otro usá la copia de datos o la exportación".into()))
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(ddl::designer())
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        ddl::templates()
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        ddl::table_ddl(table, parts)
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
        ddl::insert_script(target, columns, rows)
    }

    fn update_script(&self, _target: &ObjectRef, changes: &[dbine_driver::RowChange]) -> Result<String> {
        ddl::update_script(changes)
    }

    fn delete_script(&self, _target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
        ddl::delete_script(keys)
    }

    /// etcd reads by key or prefix only: nothing to filter by value.
    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        if filters.is_empty() {
            return Ok(browse.to_string());
        }
        Err(Error::Unsupported("etcd lee por clave o prefijo: no filtra por valor en el servidor".into()))
    }

    async fn connect(&self, cfg: &ConnectionConfig, _database: Option<&str>) -> Result<Box<dyn Session>> {
        let scheme = if cfg.encrypt { "https" } else { "http" };
        let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .danger_accept_invalid_certs(cfg.trust_server_certificate)
            .build()
            .map_err(Error::connect)?;
        let conn = Arc::new(Conn {
            http,
            base: format!("{scheme}://{host}:{}", cfg.port_or(2379)),
            user: cfg.username.clone().filter(|u| !u.is_empty()),
            password: cfg.password.clone().unwrap_or_default(),
            token: Mutex::new(None),
        });
        let check = async {
            if conn.user.is_some() {
                conn.authenticate().await?;
            }
            conn.post("/v3/maintenance/status", json!({})).await
        };
        tokio::time::timeout(Duration::from_secs(20), check)
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado".into()))??;
        Ok(Box::new(EtcdSession {
            conn,
            read_only: cfg.read_only,
            prefix: cfg.option("prefix").map(str::to_string),
            cancel: Arc::new(Cancel::default()),
        }))
    }
}

struct Conn {
    http: reqwest::Client,
    base: String,
    user: Option<String>,
    password: String,
    token: Mutex<Option<String>>,
}

fn http_error(e: reqwest::Error) -> Error {
    if e.is_connect() || e.is_timeout() {
        Error::Connect(e.to_string())
    } else {
        Error::Query(e.to_string())
    }
}

/// gRPC status 16 = UNAUTHENTICATED, 7 = PERMISSION_DENIED.
fn is_auth_error(code: i64, msg: &str) -> bool {
    code == 16 || msg.contains("invalid auth token") || msg.contains("user name is empty")
}

impl Conn {
    async fn authenticate(&self) -> Result<()> {
        let Some(user) = &self.user else { return Ok(()) };
        let resp = self
            .http
            .post(format!("{}/v3/auth/authenticate", self.base))
            .json(&json!({"name": user, "password": self.password}))
            .send()
            .await
            .map_err(http_error)?;
        let body: Value = resp.json().await.map_err(Error::query)?;
        match body.get("token").and_then(Value::as_str) {
            Some(t) => {
                *self.token.lock().unwrap_or_else(|e| e.into_inner()) = Some(t.to_string());
                Ok(())
            }
            None => {
                let msg = body.get("message").and_then(Value::as_str).unwrap_or("autenticación rechazada");
                if msg.contains("authentication is not enabled") {
                    // Credentials are optional then: go on without a token.
                    return Ok(());
                }
                Err(Error::AuthFailed(msg.to_string()))
            }
        }
    }

    async fn post_once(&self, path: &str, body: &Value) -> Result<std::result::Result<Value, (i64, String)>> {
        let mut rb = self.http.post(format!("{}{path}", self.base)).json(body);
        if let Some(t) = self.token.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            rb = rb.header("Authorization", t);
        }
        let resp = rb.send().await.map_err(http_error)?;
        let status = resp.status();
        let text = resp.text().await.map_err(http_error)?;
        let v: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({"message": text.trim()}));
        if status.is_success() && v.get("error").is_none() {
            return Ok(Ok(v));
        }
        let code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
        let msg = v
            .get("message")
            .or_else(|| v.get("error"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("HTTP {status}"));
        Ok(Err((code, msg)))
    }

    /// POST a gateway call; on an expired token, authenticate and retry once.
    async fn post(&self, path: &str, body: Value) -> Result<Value> {
        match self.post_once(path, &body).await? {
            Ok(v) => Ok(v),
            Err((code, msg)) if is_auth_error(code, &msg) && self.user.is_some() => {
                self.authenticate().await?;
                match self.post_once(path, &body).await? {
                    Ok(v) => Ok(v),
                    Err((code, msg)) if is_auth_error(code, &msg) => Err(Error::AuthFailed(msg)),
                    Err((_, msg)) => Err(Error::Query(msg)),
                }
            }
            Err((code, msg)) if is_auth_error(code, &msg) => Err(Error::AuthFailed(msg)),
            Err((_, msg)) => Err(Error::Query(msg)),
        }
    }

    async fn get_text(&self, path: &str) -> Result<String> {
        let mut rb = self.http.get(format!("{}{path}", self.base));
        if let Some(t) = self.token.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            rb = rb.header("Authorization", t);
        }
        let resp = rb.send().await.map_err(http_error)?;
        if !resp.status().is_success() {
            return Err(Error::Query(format!("HTTP {}", resp.status())));
        }
        resp.text().await.map_err(http_error)
    }
}

/// Cancellation of the command in flight: the interrupter sets the flag
/// and wakes the waiting request, which is dropped (etcd requests are
/// short; nothing keeps running on the server that matters).
#[derive(Default)]
struct Cancel {
    flag: AtomicBool,
    notify: Notify,
}

impl Cancel {
    async fn run<T>(&self, f: impl Future<Output = Result<T>>) -> Result<T> {
        let woken = self.notify.notified();
        tokio::pin!(woken);
        woken.as_mut().enable();
        if self.flag.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        tokio::select! {
            r = f => r,
            _ = woken => Err(Error::Cancelled),
        }
    }
}

pub struct EtcdSession {
    conn: Arc<Conn>,
    read_only: bool,
    prefix: Option<String>,
    cancel: Arc<Cancel>,
}

fn b64(b: &[u8]) -> String {
    B64.encode(b)
}

fn unb64(v: Option<&Value>) -> Vec<u8> {
    v.and_then(Value::as_str).and_then(|s| B64.decode(s).ok()).unwrap_or_default()
}

/// proto3 JSON sends int64 as strings.
fn int(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        _ => 0,
    }
}

fn uint(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        _ => 0,
    }
}

/// Bytes as text when they are UTF-8, else `0x…`.
fn bytes_cell(b: &[u8]) -> Value {
    match std::str::from_utf8(b) {
        Ok(s) => Value::String(s.to_string()),
        Err(_) => json_bytes(b),
    }
}

fn cols(names: &[&str]) -> Vec<ResultColumn> {
    names.iter().map(|n| ResultColumn { name: n.to_string(), type_name: String::new() }).collect()
}

const KV_COLUMNS: &[&str] = &["key", "value", "create_revision", "mod_revision", "version", "lease"];

fn kv_row(kv: &Value, keys_only: bool) -> Vec<Value> {
    let key = bytes_cell(&unb64(kv.get("key")));
    if keys_only {
        return vec![key];
    }
    let lease = int(kv.get("lease"));
    vec![
        key,
        bytes_cell(&unb64(kv.get("value"))),
        json_i64(int(kv.get("create_revision"))),
        json_i64(int(kv.get("mod_revision"))),
        json_i64(int(kv.get("version"))),
        if lease == 0 { Value::Null } else { Value::String(lease_hex(lease)) },
    ]
}

/// The `key` / `range_end` of a get or del.
fn range(c: &Command) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
    let key = c.args.get(1).cloned().ok_or_else(|| Error::Query(format!("{} necesita una clave", c.verb())))?;
    let end = if c.flag("prefix") {
        Some(prefix_end(&key))
    } else if c.flag("from-key") {
        Some(vec![0])
    } else {
        c.args.get(2).cloned()
    };
    // `get "" --prefix` means every key.
    let key = if key.is_empty() && end.is_some() { vec![0] } else { key };
    Ok((key, end))
}

fn int_flag(c: &Command, name: &str) -> Result<Option<i64>> {
    match c.flag_value(name) {
        None => Ok(None),
        Some(v) => v.parse().map(Some).map_err(|_| Error::Query(format!("--{name} espera un número: {v}"))),
    }
}

impl EtcdSession {
    async fn call(&self, path: &str, body: Value) -> Result<Value> {
        self.cancel.run(self.conn.post(path, body)).await
    }

    async fn run(&mut self, c: &Command, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        match c.name().as_str() {
            "get" => {
                let (key, end) = range(c)?;
                let mut body = json!({"key": b64(&key)});
                if let Some(e) = end {
                    body["range_end"] = json!(b64(&e));
                }
                if let Some(n) = int_flag(c, "limit")? {
                    body["limit"] = json!(n.to_string());
                }
                if let Some(n) = int_flag(c, "rev")? {
                    body["revision"] = json!(n.to_string());
                }
                let keys_only = c.flag("keys-only");
                let count_only = c.flag("count-only");
                body["keys_only"] = json!(keys_only);
                body["count_only"] = json!(count_only);
                if let Some(o) = c.flag_value("order") {
                    body["sort_order"] = json!(o.to_ascii_uppercase());
                    body["sort_target"] = json!(c.flag_value("sort-by").unwrap_or("KEY").to_ascii_uppercase());
                } else if let Some(t) = c.flag_value("sort-by") {
                    body["sort_order"] = json!("ASCEND");
                    body["sort_target"] = json!(t.to_ascii_uppercase());
                }
                let r = self.call("/v3/kv/range", body).await?;
                if count_only {
                    out.begin_result(cols(&["count"]));
                    out.push_row(vec![json_i64(int(r.get("count")))], max_rows);
                } else {
                    out.begin_result(cols(if keys_only { &["key"] } else { KV_COLUMNS }));
                    for kv in r.get("kvs").and_then(Value::as_array).into_iter().flatten() {
                        out.push_row(kv_row(kv, keys_only), max_rows);
                    }
                    if r.get("more").and_then(Value::as_bool) == Some(true) {
                        out.messages.push(format!("Hay más claves en el rango (total: {}).", int(r.get("count"))));
                    }
                }
                out.messages.push(format!("revisión {}", int(r.pointer("/header/revision"))));
            }
            "put" => {
                let key = c.args.get(1).ok_or_else(|| Error::Query("put necesita una clave y un valor".into()))?;
                let value = c.args.get(2).ok_or_else(|| Error::Query("put necesita un valor".into()))?;
                let mut body = json!({"key": b64(key), "value": b64(value), "prev_kv": c.flag("prev-kv")});
                if let Some(l) = c.flag_value("lease") {
                    body["lease"] = json!(parse_lease(l).map_err(Error::Query)?.to_string());
                }
                if let Some(ttl) = int_flag(c, "ttl")? {
                    let g = self.call("/v3/lease/grant", json!({"TTL": ttl.to_string()})).await?;
                    let id = int(g.get("ID"));
                    out.messages.push(format!("lease {} otorgado (TTL {ttl} s)", lease_hex(id)));
                    body["lease"] = json!(id.to_string());
                }
                let r = self.call("/v3/kv/put", body).await?;
                if let Some(prev) = r.get("prev_kv") {
                    out.begin_result(cols(KV_COLUMNS));
                    out.push_row(kv_row(prev, false), max_rows);
                } else {
                    out.push_affected(1);
                }
                out.messages.push(format!("revisión {}", int(r.pointer("/header/revision"))));
            }
            "del" | "delete" => {
                let (key, end) = range(c)?;
                let mut body = json!({"key": b64(&key), "prev_kv": c.flag("prev-kv")});
                if let Some(e) = end {
                    body["range_end"] = json!(b64(&e));
                }
                let r = self.call("/v3/kv/deleterange", body).await?;
                if let Some(prev) = r.get("prev_kvs").and_then(Value::as_array) {
                    out.begin_result(cols(KV_COLUMNS));
                    for kv in prev {
                        out.push_row(kv_row(kv, false), max_rows);
                    }
                } else {
                    out.push_affected(uint(r.get("deleted")));
                }
            }
            "lease grant" => {
                let ttl: i64 = c.word(2).parse().map_err(|_| Error::Query("lease grant necesita el TTL en segundos".into()))?;
                let r = self.call("/v3/lease/grant", json!({"TTL": ttl.to_string()})).await?;
                out.begin_result(cols(&["lease", "ttl"]));
                out.push_row(vec![Value::String(lease_hex(int(r.get("ID")))), json_i64(int(r.get("TTL")))], max_rows);
            }
            "lease revoke" => {
                let id = parse_lease(&c.word(2)).map_err(Error::Query)?;
                self.call("/v3/lease/revoke", json!({"ID": id.to_string()})).await?;
                out.push_affected(1);
            }
            "lease timetolive" => {
                let id = parse_lease(&c.word(2)).map_err(Error::Query)?;
                let r = self.call("/v3/lease/timetolive", json!({"ID": id.to_string(), "keys": c.flag("keys")})).await?;
                let keys: Vec<String> = r
                    .get("keys")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|k| String::from_utf8_lossy(&unb64(Some(k))).into_owned())
                    .collect();
                out.begin_result(cols(&["lease", "ttl", "granted_ttl", "keys"]));
                out.push_row(
                    vec![
                        Value::String(lease_hex(id)),
                        json_i64(int(r.get("TTL"))),
                        json_i64(int(r.get("grantedTTL"))),
                        Value::String(keys.join(", ")),
                    ],
                    max_rows,
                );
            }
            "lease list" => {
                let r = self.call("/v3/lease/leases", json!({})).await?;
                out.begin_result(cols(&["lease"]));
                for l in r.get("leases").and_then(Value::as_array).into_iter().flatten() {
                    out.push_row(vec![Value::String(lease_hex(int(l.get("ID"))))], max_rows);
                }
            }
            "member list" => {
                let r = self.call("/v3/cluster/member/list", json!({})).await?;
                out.begin_result(cols(&["id", "name", "peer_urls", "client_urls", "is_learner"]));
                for m in r.get("members").and_then(Value::as_array).into_iter().flatten() {
                    out.push_row(member_row(m), max_rows);
                }
            }
            "endpoint status" => {
                let r = self.call("/v3/maintenance/status", json!({})).await?;
                out.begin_result(cols(&["member", "version", "db_size", "db_size_in_use", "leader", "raft_term", "raft_index", "raft_applied_index", "is_learner"]));
                out.push_row(
                    vec![
                        Value::String(format!("{:x}", uint(r.pointer("/header/member_id")))),
                        r.get("version").cloned().unwrap_or(Value::Null),
                        json_i64(int(r.get("dbSize"))),
                        json_i64(int(r.get("dbSizeInUse"))),
                        Value::String(format!("{:x}", uint(r.get("leader")))),
                        json_i64(int(r.get("raftTerm"))),
                        json_i64(int(r.get("raftIndex"))),
                        json_i64(int(r.get("raftAppliedIndex"))),
                        Value::Bool(r.get("isLearner").and_then(Value::as_bool).unwrap_or(false)),
                    ],
                    max_rows,
                );
            }
            "alarm list" => {
                let r = self.call("/v3/maintenance/alarm", json!({"action": "GET"})).await?;
                out.begin_result(cols(&["member", "alarm"]));
                for a in r.get("alarms").and_then(Value::as_array).into_iter().flatten() {
                    out.push_row(vec![Value::String(format!("{:x}", uint(a.get("memberID")))), a.get("alarm").cloned().unwrap_or(Value::Null)], max_rows);
                }
            }
            "compaction" => {
                let rev: i64 = c.word(1).parse().map_err(|_| Error::Query("compaction necesita una revisión".into()))?;
                self.call("/v3/kv/compaction", json!({"revision": rev.to_string(), "physical": c.flag("physical")})).await?;
                out.push_affected(0);
                out.messages.push(format!("historial compactado hasta la revisión {rev}"));
            }
            "user list" | "role list" => {
                let what = if c.verb() == "user" { "user" } else { "role" };
                let r = self.call(&format!("/v3/auth/{what}/list"), json!({})).await?;
                out.begin_result(cols(&[what]));
                for u in r.get(if what == "user" { "users" } else { "roles" }).and_then(Value::as_array).into_iter().flatten() {
                    out.push_row(vec![u.clone()], max_rows);
                }
            }
            name if security::handles(name) => security::run(self, c, max_rows, out).await?,
            "snapshot save" => {
                let path = c.word(2);
                if path.trim().is_empty() {
                    return Err(Error::Query("snapshot save necesita el archivo donde guardarlo".into()));
                }
                let bytes = self.cancel.run(backup::save(&self.conn, &path)).await?;
                out.push_affected(0);
                out.messages.push(format!("snapshot guardado en {path} ({bytes} bytes)"));
            }
            "watch" | "lease keep-alive" | "elect" | "lock" => {
                return Err(Error::Unsupported(format!("El editor no admite {}: deja la conexión esperando eventos.", c.name())));
            }
            other => {
                return Err(Error::Query(format!(
                    "Comando desconocido: {other}. Se admiten get, put, del, lease, member list, endpoint status, alarm list, compaction, user, role, auth y snapshot save."
                )))
            }
        }
        Ok(())
    }

    async fn metrics(&self) -> Result<prom::Samples> {
        let text = self.cancel.run(self.conn.get_text("/metrics")).await?;
        Ok(prom::Samples::parse(&text))
    }
}

fn member_row(m: &Value) -> Vec<Value> {
    let urls = |k: &str| m.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default();
    vec![
        Value::String(format!("{:x}", uint(m.get("ID")))),
        m.get("name").cloned().unwrap_or(Value::Null),
        Value::String(urls("peerURLs")),
        Value::String(urls("clientURLs")),
        Value::Bool(m.get("isLearner").and_then(Value::as_bool).unwrap_or(false)),
    ]
}

#[async_trait]
impl Session for EtcdSession {
    async fn server_version(&mut self) -> Result<String> {
        let r = self.call("/v3/maintenance/status", json!({})).await?;
        Ok(format!("etcd {}", r.get("version").and_then(Value::as_str).unwrap_or("")))
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        Ok(vec!["default".into()])
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let (key, end) = match self.prefix.as_deref() {
            Some(p) => (p.as_bytes().to_vec(), prefix_end(p.as_bytes())),
            None => (vec![0], vec![0]),
        };
        let r = self
            .call("/v3/kv/range", json!({"key": b64(&key), "range_end": b64(&end), "keys_only": true, "limit": MAX_KEYS.to_string()}))
            .await?;
        Ok(r.get("kvs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|kv| DbObject {
                kind: kinds::KEY.into(),
                schema: None,
                name: String::from_utf8_lossy(&unb64(kv.get("key"))).into_owned(),
                parent: None,
            })
            .collect())
    }

    async fn scan_keys(&mut self, scan: &KeyScan) -> Result<KeyPage> {
        let wanted = format!("{}{}", self.prefix.as_deref().unwrap_or(""), scan.pattern.trim());
        let (from, end) = if wanted.is_empty() {
            (vec![0], vec![0])
        } else {
            (wanted.as_bytes().to_vec(), prefix_end(wanted.as_bytes()))
        };
        // The cursor is the key to start from (base64: keys are bytes).
        let from = match &scan.cursor {
            Some(c) => B64.decode(c).map_err(|_| Error::Query("cursor de búsqueda inválido".into()))?,
            None => from,
        };
        let limit = scan.count.clamp(1, 5000);
        let r = self
            .call("/v3/kv/range", json!({"key": b64(&from), "range_end": b64(&end), "keys_only": true, "limit": limit.to_string()}))
            .await?;
        let kvs: Vec<&Value> = r.get("kvs").and_then(Value::as_array).into_iter().flatten().collect();
        // Time to live comes from the lease: one lookup per lease in the page.
        let mut ttls: std::collections::HashMap<i64, Option<i64>> = std::collections::HashMap::new();
        for kv in &kvs {
            let lease = int(kv.get("lease"));
            if lease != 0 && !ttls.contains_key(&lease) {
                let t = self.call("/v3/lease/timetolive", json!({"ID": lease.to_string()})).await.ok();
                ttls.insert(lease, t.map(|t| int(t.get("TTL"))).filter(|s| *s >= 0).map(|s| s * 1000));
            }
        }
        let next = r.get("more").and_then(Value::as_bool).unwrap_or(false).then(|| {
            let mut after = unb64(kvs.last().and_then(|kv| kv.get("key")));
            after.push(0);
            b64(&after)
        });
        let keys = kvs
            .iter()
            .map(|kv| KeyEntry {
                name: String::from_utf8_lossy(&unb64(kv.get("key"))).into_owned(),
                key_type: None,
                ttl_ms: ttls.get(&int(kv.get("lease"))).copied().flatten(),
            })
            .collect::<Vec<_>>();
        Ok(KeyPage {
            scanned: keys.len() as u64,
            keys,
            cursor: next,
            // `count` is every key from the start key on: the whole range
            // only in the first page.
            total: scan.cursor.is_none().then(|| uint(r.get("count"))),
        })
    }

    async fn columns(&mut self, _obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        Ok(KV_COLUMNS
            .iter()
            .enumerate()
            .map(|(i, n)| ColumnInfo {
                name: n.to_string(),
                data_type: match i {
                    0 | 1 => "bytes",
                    5 => "lease",
                    _ => "int64",
                }
                .into(),
                nullable: i == 5,
                primary_key: i == 0,
                auto_increment: false,
                default_value: None,
            })
            .collect())
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let r = self.call("/v3/kv/range", json!({"key": b64(obj.name.as_bytes())})).await?;
        let Some(kv) = r.get("kvs").and_then(Value::as_array).and_then(|a| a.first()) else { return Ok(None) };
        let value = unb64(kv.get("value"));
        let mut lines = vec![
            format!("KEY              {}", obj.name),
            format!("CREATE_REVISION  {}", int(kv.get("create_revision"))),
            format!("MOD_REVISION     {}", int(kv.get("mod_revision"))),
            format!("VERSION          {}", int(kv.get("version"))),
            format!("SIZE             {} bytes", value.len()),
        ];
        let lease = int(kv.get("lease"));
        if lease == 0 {
            lines.push("LEASE            (sin lease)".into());
        } else {
            let ttl = self.call("/v3/lease/timetolive", json!({"ID": lease.to_string()})).await.ok();
            lines.push(format!("LEASE            {}", lease_hex(lease)));
            if let Some(t) = ttl {
                lines.push(format!("TTL              {} s (de {} s)", int(t.get("TTL")), int(t.get("grantedTTL"))));
            }
        }
        Ok(Some(lines.join("\n")))
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        // A key that ends in `/` is a directory-like prefix.
        if obj.name.ends_with('/') {
            format!("get {} --prefix --limit={limit}", quote_arg(&obj.name))
        } else {
            format!("get {}", quote_arg(&obj.name))
        }
    }

    /// Keys have no columns of their own: no ER diagram or schema script.
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        Ok(Vec::new())
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

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        let script = command::parse_script(text).map_err(Error::Query)?;
        if self.read_only {
            if let Some(w) = script.iter().find(|c| !command::is_read(c)) {
                return Err(Error::Query(format!(
                    "Conexión de solo lectura: se bloqueó el comando {}. Solo se permiten lecturas (get, lease list, member list…).",
                    w.name()
                )));
            }
        }
        for c in &script {
            self.run(c, max_rows, out).await?;
        }
        Ok(())
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        security::principals(self).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        security::grants(self, principal).await
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let cancel = self.cancel.clone();
        Some(Arc::new(move || {
            cancel.flag.store(true, Ordering::SeqCst);
            cancel.notify.notify_waiters();
        }))
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        let mut snap = MonitorSnapshot::default();
        let status = self.call("/v3/maintenance/status", json!({})).await?;
        let members = self.call("/v3/cluster/member/list", json!({})).await.ok();
        let leases = self.call("/v3/lease/leases", json!({})).await.ok();
        let alarms = self.call("/v3/maintenance/alarm", json!({"action": "GET"})).await.ok();
        let m = match self.metrics().await {
            Ok(m) => Some(m),
            Err(e) => {
                snap.notes.push(format!("No se pudo leer /metrics ({e}): faltan CPU, memoria, red y actividad."));
                None
            }
        };
        let g = |name: &str| m.as_ref().and_then(|m| m.sum(name));
        let me = uint(status.pointer("/header/member_id"));
        let leader = uint(status.get("leader"));
        let quota = g("etcd_server_quota_backend_bytes");
        let db_size = g("etcd_mvcc_db_total_size_in_bytes").or(Some(int(status.get("dbSize")) as f64));
        let in_use = g("etcd_mvcc_db_total_size_in_use_in_bytes").or(Some(int(status.get("dbSizeInUse")) as f64));
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
        let avg_ms = |base: &str| match (g(&format!("{base}_sum")), g(&format!("{base}_count"))) {
            (Some(s), Some(c)) if c > 0.0 => Some(s / c * 1000.0),
            _ => None,
        };
        let streams = m.as_ref().and_then(|m| {
            let started = m.sum_where("grpc_server_started_total", "grpc_type=\"bidi_stream\"")?;
            let handled = m.sum_where("grpc_server_handled_total", "grpc_type=\"bidi_stream\"").unwrap_or(0.0);
            Some((started - handled).max(0.0))
        });
        let writes = match (g("etcd_mvcc_put_total"), g("etcd_mvcc_delete_total")) {
            (None, None) => None,
            (p, d) => Some(p.unwrap_or(0.0) + d.unwrap_or(0.0)),
        };
        let fds = g("process_open_fds");
        use MetricUnit::*;
        snap.metrics = vec![
            Metric::new("cpu_time", "CPU del proceso", "CPU", Percent, g("process_cpu_seconds_total").map(|s| s * 100.0)).counter(),
            Metric::new("mem_used", "Memoria residente", "Memoria", Bytes, g("process_resident_memory_bytes")),
            Metric::new("active_sessions", "Streams gRPC activos", "Conexiones", Count, streams),
            Metric::new("watchers", "Watchers", "Conexiones", Count, g("etcd_debugging_mvcc_watcher_total")),
            Metric::new("open_fds", "Descriptores abiertos", "Conexiones", Count, fds).max(g("process_max_fds")),
            Metric::new("queries", "Pedidos gRPC", "Actividad", Count, g("grpc_server_handled_total")).counter(),
            Metric::new("rows_read", "Lecturas (range)", "Actividad", Count, g("etcd_mvcc_range_total")).counter(),
            Metric::new("rows_written", "Escrituras (put + delete)", "Actividad", Count, writes).counter(),
            Metric::new("transactions", "Transacciones", "Actividad", Count, g("etcd_mvcc_txn_total")).counter(),
            Metric::new("net_in", "Red entrante (clientes)", "Red", Bytes, g("etcd_network_client_grpc_received_bytes_total")).counter(),
            Metric::new("net_out", "Red saliente (clientes)", "Red", Bytes, g("etcd_network_client_grpc_sent_bytes_total")).counter(),
            Metric::new("wal_fsync", "fsync del WAL (promedio)", "Disco", Millis, avg_ms("etcd_disk_wal_fsync_duration_seconds")),
            Metric::new("backend_commit", "Commit del backend (promedio)", "Disco", Millis, avg_ms("etcd_disk_backend_commit_duration_seconds")),
            Metric::new("storage_used", "Tamaño de la base", "Almacenamiento", Bytes, db_size).max(quota),
            Metric::new("storage_in_use", "Espacio en uso (sin fragmentación)", "Almacenamiento", Bytes, in_use).max(db_size),
            Metric::new("keys", "Claves", "Datos", Count, g("etcd_debugging_mvcc_keys_total")),
            Metric::new(
                "leases",
                "Leases",
                "Datos",
                Count,
                leases.as_ref().map(|l| l.get("leases").and_then(Value::as_array).map_or(0, Vec::len) as f64),
            ),
            Metric::new("leader_changes", "Cambios de líder", "Replicación", Count, g("etcd_server_leader_changes_seen_total")).counter(),
            Metric::new("proposals_pending", "Propuestas pendientes", "Replicación", Count, g("etcd_server_proposals_pending")),
            Metric::new("proposals_failed", "Propuestas fallidas", "Replicación", Count, g("etcd_server_proposals_failed_total")).counter(),
            Metric::new("proposals_committed", "Propuestas confirmadas", "Replicación", Count, g("etcd_server_proposals_committed_total")).counter(),
            Metric::new(
                "apply_lag",
                "Entradas sin aplicar (raft)",
                "Replicación",
                Count,
                Some((int(status.get("raftIndex")) - int(status.get("raftAppliedIndex"))).max(0) as f64),
            ),
            Metric::new("uptime", "Tiempo activo", "Servidor", Seconds, g("process_start_time_seconds").map(|s| (now - s).max(0.0))),
        ];

        let mut nodes = MonitorTable::new("nodes", "Miembros del cluster", &["id", "nombre", "rol", "peer URLs", "client URLs"]);
        for mm in members.as_ref().and_then(|r| r.get("members")).and_then(Value::as_array).into_iter().flatten() {
            let row = member_row(mm);
            let id = uint(mm.get("ID"));
            let role = if row[4] == Value::Bool(true) {
                "learner"
            } else if id == leader {
                "líder"
            } else {
                "seguidor"
            };
            nodes.rows.push(vec![row[0].clone(), row[1].clone(), Value::String(role.into()), row[2].clone(), row[3].clone()]);
        }
        if nodes.rows.is_empty() {
            snap.notes.push("No se pudo leer la lista de miembros (hace falta el rol root si la autenticación está habilitada).".into());
        }
        snap.tables.push(nodes);
        let mut al = MonitorTable::new("alarms", "Alarmas", &["miembro", "alarma"]);
        for a in alarms.as_ref().and_then(|r| r.get("alarms")).and_then(Value::as_array).into_iter().flatten() {
            al.rows.push(vec![Value::String(format!("{:x}", uint(a.get("memberID")))), a.get("alarm").cloned().unwrap_or(Value::Null)]);
        }
        snap.tables.push(al);

        snap.info = vec![
            ("Versión".into(), status.get("version").and_then(Value::as_str).unwrap_or("").to_string()),
            ("Cluster".into(), format!("{:x}", uint(status.pointer("/header/cluster_id")))),
            ("Miembro".into(), format!("{me:x}")),
            (
                "Rol".into(),
                if status.get("isLearner").and_then(Value::as_bool) == Some(true) {
                    "learner".into()
                } else if me == leader {
                    "líder".into()
                } else {
                    "seguidor".into()
                },
            ),
            ("Líder".into(), format!("{leader:x}")),
            ("Término raft".into(), int(status.get("raftTerm")).to_string()),
            ("Revisión".into(), int(status.pointer("/header/revision")).to_string()),
        ];
        if let Some(q) = quota {
            snap.info.push(("Cuota del backend".into(), format!("{:.0} MiB", q / 1_048_576.0)));
        }
        if let Some(errors) = status.get("errors").and_then(Value::as_array).filter(|e| !e.is_empty()) {
            snap.info.push(("Errores".into(), errors.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; ")));
        }
        snap.notes.push("etcd no expone el uso de CPU del host ni la cantidad de conexiones de clientes: se muestran el CPU del proceso y los streams gRPC activos (watch, keep-alive).".into());
        snap.notes.push("Las métricas son del miembro al que se conecta DBine; para ver otro miembro, conectate a su dirección.".into());
        Ok(snap)
    }

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
            assert_eq!(d.filtered_browse("get k", &[]).unwrap(), "get k");
            assert!(matches!(d.filtered_browse("get k", &[f.clone()]), Err(Error::Unsupported(_))));
        }
    }

    #[test]
    fn one_driver_with_monitor() {
        let d = drivers();
        assert_eq!(d[0].info().id, "etcd");
        assert!(d[0].capabilities().monitor && !d[0].capabilities().create_database);
        assert!(!d[0].query_help().is_empty());
    }

    #[test]
    fn rows_decode_base64_and_int64_strings() {
        let kv = json!({"key": b64(b"/a"), "value": b64(&[0xff, 0x00]), "create_revision": "2", "mod_revision": "5", "version": "3", "lease": "26"});
        let r = kv_row(&kv, false);
        assert_eq!(r[0], json!("/a"));
        assert_eq!(r[1], json!("0xFF00"));
        assert_eq!((r[2].clone(), r[3].clone(), r[4].clone()), (json!(2), json!(5), json!(3)));
        assert_eq!(r[5], json!("1a"));
        assert_eq!(kv_row(&kv, true), vec![json!("/a")]);
    }

    #[test]
    fn ranges_from_flags() {
        let c = &command::parse_script("get /a --prefix").unwrap()[0];
        assert_eq!(range(c).unwrap(), (b"/a".to_vec(), Some(b"/b".to_vec())));
        let c = &command::parse_script("get \"\" --prefix").unwrap()[0];
        assert_eq!(range(c).unwrap(), (vec![0], Some(vec![0])));
        let c = &command::parse_script("del a c").unwrap()[0];
        assert_eq!(range(c).unwrap(), (b"a".to_vec(), Some(b"c".to_vec())));
        assert!(range(&command::parse_script("get").unwrap()[0]).is_err());
    }

    #[test]
    fn browse_prefix_or_key() {
        let s = EtcdSession {
            conn: Arc::new(Conn { http: reqwest::Client::new(), base: String::new(), user: None, password: String::new(), token: Mutex::new(None) }),
            read_only: false,
            prefix: None,
            cancel: Arc::new(Cancel::default()),
        };
        let o = |n: &str| ObjectRef { kind: kinds::KEY.into(), schema: None, name: n.into() };
        assert_eq!(s.browse_query(&o("/cfg/a b"), 10), "get \"/cfg/a b\"");
        assert_eq!(s.browse_query(&o("/cfg/"), 10), "get /cfg/ --prefix --limit=10");
    }
}
