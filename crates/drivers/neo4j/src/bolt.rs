//! A small Bolt client (protocol 4.2–6.0): handshake (with the version
//! manifest of 5.7+ servers), HELLO / LOGON, RUN + PULL with the result
//! metadata (fields, plans, profiles, counters, notifications) and RESET
//! after a failure. Neo4j, Memgraph and other Bolt servers speak it; one
//! [`Conn`] is one socket, no pooling.
//!
//! Bolt 6.0 (Neo4j 2025.10+) is what carries the VECTOR type (an older
//! protocol gets a placeholder map instead of the vector). From 5.6 on the
//! server reports GQL statuses instead of notifications and, from 5.7,
//! `neo4j_code` instead of `code` in failures: [`gql_compat`] puts them
//! back in the 5.4 shape the rest of the driver reads.

use crate::packstream::{encode, encoded_len, map, Decoder, Value};
use dbine_driver::{ConnectionConfig, Error, Field, Result, TlsLevel};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

const HELLO: u8 = 0x01;
const GOODBYE: u8 = 0x02;
const RESET: u8 = 0x0F;
const RUN: u8 = 0x10;
const BEGIN: u8 = 0x11;
const COMMIT: u8 = 0x12;
const ROLLBACK: u8 = 0x13;
const PULL: u8 = 0x3F;
const LOGON: u8 = 0x6A;
const SUCCESS: u8 = 0x70;
const RECORD: u8 = 0x71;
const IGNORED: u8 = 0x7E;
const FAILURE: u8 = 0x7F;

/// Rows asked for per PULL.
const BATCH: i64 = 1000;

/// The versions this client speaks, most wanted first: 6.0 (VECTOR),
/// 5.8–5.6 (GQL statuses and errors, see [`gql_compat`]), 5.4–5.0 and
/// 4.4–4.2. No server ever negotiates 5.5.
const SUPPORTED: &[(u8, u8)] = &[
    (6, 0),
    (5, 8),
    (5, 7),
    (5, 6),
    (5, 4),
    (5, 3),
    (5, 2),
    (5, 1),
    (5, 0),
    (4, 4),
    (4, 3),
    (4, 2),
];

/// Magic, then four proposals: the version manifest (v1, Bolt 5.7+
/// servers answer it with every version they speak), 5.4 down to 5.0 and
/// 4.4 down to 4.2 for servers without it.
const HANDSHAKE: [u8; 20] = [
    0x60, 0x60, 0xB0, 0x17, 0, 0, 1, 0xFF, 0, 4, 4, 5, 0, 2, 4, 4, 0, 0, 0, 0,
];

/// The manifest's answer to a proposal.
const MANIFEST_V1: [u8; 4] = [0, 0, 1, 0xFF];

/// The version to use from a manifest's offer (entries `[_, range, minor,
/// major]`: `minor` and the `range` minors below it).
fn pick(offered: &[[u8; 4]]) -> Option<(u8, u8)> {
    SUPPORTED.iter().copied().find(|&(major, minor)| {
        offered.iter().any(|&[_, range, top, m]| {
            m == major && minor <= top && minor >= top.saturating_sub(range)
        })
    })
}

/// A message's metadata from a 5.6+ server in the 5.4 shape: a SUCCESS'
/// GQL `statuses` that carry a Neo4j code become `notifications` (code,
/// title, description, severity, category, position), and a FAILURE's
/// `neo4j_code` (5.7+) is its `code`.
pub(crate) fn gql_compat(tag: u8, meta: Value) -> Value {
    let Value::Map(mut m) = meta else {
        return meta;
    };
    let has = |m: &[(String, Value)], k: &str| m.iter().any(|(x, _)| x == k);
    if tag == FAILURE && !has(&m, "code") {
        if let Some(code) = m
            .iter()
            .find(|(k, _)| k == "neo4j_code")
            .map(|(_, v)| v.clone())
        {
            m.push(("code".into(), code));
        }
    }
    if tag == SUCCESS && !has(&m, "notifications") {
        let statuses = m
            .iter()
            .find(|(k, _)| k == "statuses")
            .map(|(_, v)| v.as_list().to_vec());
        if let Some(statuses) = statuses {
            let notes: Vec<Value> = statuses
                .iter()
                .filter(|s| s.get("neo4j_code").is_some())
                .map(|s| {
                    let mut n: Vec<(String, Value)> = Vec::new();
                    for (to, from) in [
                        ("code", "neo4j_code"),
                        ("title", "title"),
                        ("description", "description"),
                    ] {
                        if let Some(v) = s.get(from) {
                            n.push((to.into(), v.clone()));
                        }
                    }
                    if let Some(d) = s.get("diagnostic_record") {
                        for (to, from) in [
                            ("severity", "_severity"),
                            ("category", "_classification"),
                            ("position", "_position"),
                        ] {
                            if let Some(v) = d.get(from) {
                                n.push((to.into(), v.clone()));
                            }
                        }
                    }
                    Value::Map(n)
                })
                .collect();
            if !notes.is_empty() {
                m.push(("notifications".into(), Value::List(notes)));
            }
        }
    }
    Value::Map(m)
}

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// What a connection closed by [`Conn::close_now`] reads and writes: errors.
struct Closed;

fn closed() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotConnected, "conexión Bolt cerrada")
}

impl AsyncRead for Closed {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Err(closed()))
    }
}

impl AsyncWrite for Closed {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        _: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Err(closed()))
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Err(closed()))
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

pub struct Conn {
    io: Box<dyn Io>,
    /// (major, minor).
    pub version: (u8, u8),
    /// `server` from HELLO's reply ("Neo4j/5.26.0", "Neo4j/v5.11.0" for Memgraph…).
    pub server: String,
    /// `connection_id` from HELLO's reply ("bolt-12").
    pub connection_id: String,
}

/// Where and how to connect, resolved from the form.
#[derive(Clone)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub trust_cert: bool,
    /// The name the certificate is checked against (the real server's
    /// through an SSH tunnel).
    pub server_name: String,
    /// `trustedCertificates`: extra CA certificates (PEM).
    pub ca_file: Option<String>,
    /// `+ssc`: the pinned certificate (`tls_pin`, `None` on first use) and
    /// what the handshakes saw, shared by every reconnect.
    pub pin: Option<String>,
    pub watch: dbine_tls::PinWatch,
    pub user: String,
    pub password: String,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{} tls={}", self.host, self.port, self.tls)
    }
}

/// The scheme option: Neo4j's own URI schemes (`bolt`, `bolt+s`,
/// `bolt+ssc`; `neo4j…` behave the same, DBine doesn't route).
pub const SCHEME: &str = "scheme";
/// The CA file option (the official drivers' `trustedCertificates`).
pub const TRUSTED_CERTIFICATES: &str = "trustedCertificates";

/// The engine's TLS options: the URI scheme and the CA file.
pub fn tls_fields() -> Vec<Field> {
    vec![
        Field::tls_mode(
            SCHEME,
            "scheme",
            &[
                ("bolt", "bolt", TlsLevel::Off),
                ("bolt+s", "bolt+s", TlsLevel::VerifyFull),
                ("bolt+ssc", "bolt+ssc", TlsLevel::Unverified),
            ],
            "bolt+s",
            ["bolt", "bolt", "bolt+s", "bolt+ssc"],
        )
        .help("bolt+s cifra y verifica el certificado; bolt+ssc cifra sin verificarlo; bolt no cifra. Un esquema escrito en el servidor (neo4j+s://…) tiene prioridad."),
        Field::tls_ca_file(TRUSTED_CERTIFICATES, "trustedCertificates")
            .help("Certificados de CA (PEM) en los que confiar además de los del sistema."),
    ]
}

/// (TLS, skip the checks) of a scheme; `neo4j…` like `bolt…`. An unknown
/// value verifies.
fn scheme_tls(scheme: &str) -> (bool, bool) {
    let s = scheme.trim().to_ascii_lowercase();
    match s.strip_prefix("neo4j").or_else(|| s.strip_prefix("bolt")) {
        Some("") => (false, false),
        Some("+ssc") => (true, true),
        _ => (true, false),
    }
}

/// `host`, `host:port` or `scheme://host:port/…`: (scheme, host, port).
fn split_host(text: &str) -> (Option<String>, String, Option<u16>) {
    let mut host = text.trim().to_string();
    let mut scheme = None;
    if let Some((s, rest)) = host.split_once("://") {
        scheme = Some(s.to_ascii_lowercase());
        host = rest.split('/').next().unwrap_or_default().to_string();
    }
    let mut port = None;
    // host:port (not an IPv6 literal without brackets).
    if let Some((h, p)) = host.rsplit_once(':') {
        if let (Ok(p), false) = (p.parse::<u16>(), h.contains(':') && !h.ends_with(']')) {
            port = Some(p);
            host = h.to_string();
        }
    }
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    (scheme, host, port)
}

/// `host`, `host:port` or a URI (`neo4j://`, `neo4j+s://`, `bolt+ssc://`,
/// `memgraph://`…). The `scheme` option sets TLS; a `+s` / `+ssc` scheme
/// typed in the host wins over it (`+ssc` also skips the checks).
pub fn target(cfg: &ConnectionConfig, default_port: u16) -> Target {
    let fields = tls_fields();
    let (mut tls, mut trust) = scheme_tls(cfg.tls_value(&fields[0]).unwrap_or("bolt+s"));
    // Before the option existed the trust checkbox applied to a `+s` scheme
    // typed in the host too (ft with neo4j+s:// skipped the checks).
    if cfg.option(SCHEME).is_none() {
        trust = cfg.trust_server_certificate;
    }
    let (scheme, host, port) = split_host(&cfg.host);
    if let Some(scheme) = scheme {
        if scheme.ends_with("+s") {
            tls = true;
        } else if scheme.ends_with("+ssc") {
            tls = true;
            trust = true;
        }
    }
    let port = port.unwrap_or_else(|| cfg.port_or(default_port));
    let host = if host.is_empty() {
        "localhost".to_string()
    } else {
        host
    };
    let (_, name, _) = split_host(cfg.tls_server_name());
    Target {
        server_name: if name.is_empty() { host.clone() } else { name },
        host,
        port,
        tls,
        trust_cert: trust,
        ca_file: cfg
            .tls_value(&fields[1])
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string),
        pin: dbine_tls::pin_of(cfg),
        watch: dbine_tls::PinWatch::default(),
        user: cfg.username_or_empty().to_string(),
        password: cfg.password_or_empty().to_string(),
    }
}

/// A server reply to one RUN + PULL.
#[derive(Debug)]
pub struct Summary {
    pub fields: Vec<String>,
    /// PULL's final SUCCESS: `type`, `stats`, `plan`, `profile`, `notifications`, `db`…
    pub meta: Value,
}

/// A failure the server reported (`Neo.ClientError…`), or the socket's.
fn failure(meta: &Value) -> Error {
    let code = meta.get("code").and_then(Value::as_str).unwrap_or_default();
    let msg = meta
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("error del servidor");
    let text = if code.is_empty() {
        msg.to_string()
    } else {
        format!("{msg} ({code})")
    };
    if code.contains("Security.Unauthorized")
        || code.contains("AuthenticationRateLimit")
        || code.contains("CredentialsExpired")
    {
        Error::AuthFailed(text)
    } else {
        Error::Query(text)
    }
}

fn io_err(e: std::io::Error) -> Error {
    Error::Connect(format!("se cortó la conexión Bolt: {e}"))
}

impl Conn {
    pub async fn open(t: &Target, user_agent: &str) -> Result<Conn> {
        tokio::time::timeout(CONNECT_TIMEOUT, Self::open_inner(t, user_agent))
            .await
            .map_err(|_| {
                Error::Connect(format!(
                    "tiempo de espera agotado al conectar con {}:{}",
                    t.host, t.port
                ))
            })?
    }

    async fn open_inner(t: &Target, user_agent: &str) -> Result<Conn> {
        let tcp = TcpStream::connect((t.host.as_str(), t.port))
            .await
            .map_err(|e| {
                Error::Connect(format!(
                    "no se pudo conectar con {}:{}: {e}",
                    t.host, t.port
                ))
            })?;
        tcp.set_nodelay(true).ok();
        let io: Box<dyn Io> = if t.tls {
            Box::new(tls(tcp, t).await?)
        } else {
            Box::new(tcp)
        };
        let mut c = Conn {
            io,
            version: (0, 0),
            server: String::new(),
            connection_id: String::new(),
        };
        c.handshake().await?;
        c.hello(t, user_agent).await?;
        Ok(c)
    }

    async fn handshake(&mut self) -> Result<()> {
        self.io.write_all(&HANDSHAKE).await.map_err(io_err)?;
        let mut v = [0u8; 4];
        self.io.read_exact(&mut v).await.map_err(|e| {
            Error::Connect(format!("el servidor no respondió al saludo Bolt (¿es el puerto Bolt? Si el servidor solo acepta TLS, elegí bolt+s en scheme): {e}"))
        })?;
        if v == [0x48, 0x54, 0x54, 0x50] {
            return Err(Error::Connect(
                "ese puerto habla HTTP, no Bolt (el puerto Bolt suele ser 7687)".into(),
            ));
        }
        if v == [0, 0, 0, 0] {
            return Err(Error::Connect(
                "el servidor no acepta ninguna versión de Bolt entre 4.2 y 6.0".into(),
            ));
        }
        if v == MANIFEST_V1 {
            return self.manifest().await;
        }
        self.version = (v[3], v[2]);
        Ok(())
    }

    /// The manifest (v1): the versions the server speaks and its
    /// capabilities; the client answers with the version it picked and no
    /// capabilities (the server sends nothing back).
    async fn manifest(&mut self) -> Result<()> {
        let n = self.varint().await?;
        let mut offered = Vec::new();
        for _ in 0..n.min(256) {
            let mut e = [0u8; 4];
            self.io.read_exact(&mut e).await.map_err(io_err)?;
            offered.push(e);
        }
        let _capabilities = self.varint().await?;
        let Some((major, minor)) = pick(&offered) else {
            let _ = self.io.write_all(&[0, 0, 0, 0, 0]).await;
            return Err(Error::Connect(
                "el servidor no acepta ninguna versión de Bolt entre 4.2 y 6.0".into(),
            ));
        };
        self.io
            .write_all(&[0, 0, minor, major, 0])
            .await
            .map_err(io_err)?;
        self.version = (major, minor);
        Ok(())
    }

    /// A handshake VarInt: 7 bits a byte, the lowest first.
    async fn varint(&mut self) -> Result<u64> {
        let mut out = 0u64;
        for shift in (0..64).step_by(7) {
            let mut b = [0u8; 1];
            self.io.read_exact(&mut b).await.map_err(io_err)?;
            out |= u64::from(b[0] & 0x7F) << shift;
            if b[0] & 0x80 == 0 {
                return Ok(out);
            }
        }
        Err(Error::Connect(
            "saludo Bolt inválido (número demasiado largo)".into(),
        ))
    }

    async fn hello(&mut self, t: &Target, user_agent: &str) -> Result<()> {
        let auth = if t.user.is_empty() {
            vec![("scheme".to_string(), Value::from("none"))]
        } else {
            vec![
                ("scheme".to_string(), Value::from("basic")),
                ("principal".to_string(), Value::from(t.user.as_str())),
                ("credentials".to_string(), Value::from(t.password.as_str())),
            ]
        };
        let mut extra = vec![("user_agent".to_string(), Value::from(user_agent))];
        if self.version >= (5, 3) {
            extra.push((
                "bolt_agent".into(),
                map([("product", Value::from(user_agent))]),
            ));
        }
        let logon = self.version >= (5, 1);
        if !logon {
            extra.extend(auth.clone());
        }
        self.send(HELLO, &[Value::Map(extra)]).await?;
        let (tag, meta) = self.recv().await?;
        if tag != SUCCESS {
            return Err(auth_error(&meta));
        }
        self.server = meta
            .get("server")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        self.connection_id = meta
            .get("connection_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if logon {
            self.send(LOGON, &[Value::Map(auth)]).await?;
            let (tag, meta) = self.recv().await?;
            if tag != SUCCESS {
                return Err(auth_error(&meta));
            }
        }
        Ok(())
    }

    async fn send(&mut self, tag: u8, fields: &[Value]) -> Result<()> {
        // As `encode(&Value::Struct(..))`, without copying the fields (a
        // bulk load's rows), into a buffer of the exact size (growing it by
        // doubling would hold up to three times the message at once).
        let mut body = Vec::with_capacity(2 + fields.iter().map(encoded_len).sum::<usize>());
        body.extend_from_slice(&[0xB0 | fields.len() as u8, tag]);
        for f in fields {
            encode(f, &mut body);
        }
        // Framed a chunk at a time (one write each), not as a second copy.
        let chunks = body.len().div_ceil(0xFFFF);
        let mut frame = Vec::with_capacity(body.len().min(0xFFFF) + 4);
        for (i, chunk) in body.chunks(0xFFFF).enumerate() {
            frame.clear();
            frame.extend_from_slice(&(chunk.len() as u16).to_be_bytes());
            frame.extend_from_slice(chunk);
            if i + 1 == chunks {
                frame.extend_from_slice(&[0, 0]);
            }
            self.io.write_all(&frame).await.map_err(io_err)?;
        }
        self.io.flush().await.map_err(io_err)
    }

    /// One message: (tag, first field). Empty chunks (keep-alive NOOPs) are skipped.
    async fn recv(&mut self) -> Result<(u8, Value)> {
        let mut msg = Vec::new();
        loop {
            let mut h = [0u8; 2];
            self.io.read_exact(&mut h).await.map_err(io_err)?;
            let n = u16::from_be_bytes(h) as usize;
            if n == 0 {
                if msg.is_empty() {
                    continue;
                }
                break;
            }
            let start = msg.len();
            msg.resize(start + n, 0);
            self.io
                .read_exact(&mut msg[start..])
                .await
                .map_err(io_err)?;
        }
        match Decoder::new(&msg).value().map_err(Error::Connect)? {
            Value::Struct(tag, mut fields) => {
                let first = if fields.is_empty() {
                    Value::Null
                } else {
                    fields.swap_remove(0)
                };
                Ok((
                    tag,
                    if self.version >= (5, 6) {
                        gql_compat(tag, first)
                    } else {
                        first
                    },
                ))
            }
            other => Err(Error::Connect(format!(
                "mensaje Bolt inesperado: {other:?}"
            ))),
        }
    }

    /// Run one statement, handing each record to `on_row`. `extra` goes in
    /// RUN (`db`, `mode`, `tx_metadata`…).
    pub async fn run(
        &mut self,
        query: &str,
        params: Value,
        extra: Value,
        on_row: &mut (dyn FnMut(&[String], Vec<Value>) + Send),
    ) -> Result<Summary> {
        self.send(RUN, &[Value::from(query), params, extra]).await?;
        self.send(PULL, &[map([("n", Value::Int(BATCH))])]).await?;
        let (tag, meta) = self.recv().await?;
        if tag != SUCCESS {
            // PULL is IGNORED after a failed RUN.
            let _ = self.recv().await;
            let e = failure(&meta);
            self.reset().await?;
            return Err(e);
        }
        let fields: Vec<String> = meta
            .get("fields")
            .map(Value::as_list)
            .unwrap_or_default()
            .iter()
            .filter_map(|f| f.as_str().map(str::to_string))
            .collect();
        loop {
            let (tag, v) = self.recv().await?;
            match tag {
                RECORD => on_row(
                    &fields,
                    match v {
                        Value::List(l) => l,
                        other => vec![other],
                    },
                ),
                SUCCESS => {
                    if v.get("has_more") == Some(&Value::Bool(true)) {
                        self.send(PULL, &[map([("n", Value::Int(BATCH))])]).await?;
                        continue;
                    }
                    return Ok(Summary { fields, meta: v });
                }
                FAILURE => {
                    let e = failure(&v);
                    self.reset().await?;
                    return Err(e);
                }
                IGNORED => {
                    self.reset().await?;
                    return Err(Error::Query("el servidor ignoró la sentencia".into()));
                }
                other => {
                    return Err(Error::Connect(format!(
                        "respuesta Bolt inesperada 0x{other:02X}"
                    )))
                }
            }
        }
    }

    /// Open an explicit transaction; `extra` as RUN's (`db`, `mode`,
    /// `tx_metadata`…). Statements then go with an empty `extra`, and
    /// nothing is kept until [`Conn::commit`]: a connection closed or reset
    /// before it rolls the transaction back.
    pub async fn begin(&mut self, extra: Value) -> Result<()> {
        self.send(BEGIN, &[extra]).await?;
        self.expect_success().await
    }

    /// Close the socket now, without waiting (callable from `Drop`): the
    /// server rolls back an open transaction and frees its locks. The
    /// connection only gives errors afterwards.
    pub fn close_now(&mut self) {
        self.io = Box::new(Closed);
    }

    /// Commit the open transaction.
    pub async fn commit(&mut self) -> Result<()> {
        self.send(COMMIT, &[]).await?;
        self.expect_success().await
    }

    /// Roll back the open transaction.
    pub async fn rollback(&mut self) -> Result<()> {
        self.send(ROLLBACK, &[]).await?;
        self.expect_success().await
    }

    async fn expect_success(&mut self) -> Result<()> {
        let (tag, meta) = self.recv().await?;
        match tag {
            SUCCESS => Ok(()),
            FAILURE | IGNORED => {
                let e = if tag == FAILURE {
                    failure(&meta)
                } else {
                    Error::Query("el servidor ignoró el mensaje".into())
                };
                self.reset().await?;
                Err(e)
            }
            other => Err(Error::Connect(format!(
                "respuesta Bolt inesperada 0x{other:02X}"
            ))),
        }
    }

    /// Run and collect every row (catalog queries).
    pub async fn query(
        &mut self,
        query: &str,
        params: Value,
        extra: Value,
    ) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
        let mut rows = Vec::new();
        let s = self
            .run(query, params, extra, &mut |_, r| rows.push(r))
            .await?;
        Ok((s.fields, rows))
    }

    async fn reset(&mut self) -> Result<()> {
        self.send(RESET, &[]).await?;
        loop {
            let (tag, meta) = self.recv().await?;
            match tag {
                SUCCESS => return Ok(()),
                FAILURE => return Err(failure(&meta)),
                _ => continue,
            }
        }
    }

    pub async fn close(mut self) {
        let _ = self.send(GOODBYE, &[]).await;
    }
}

fn auth_error(meta: &Value) -> Error {
    match failure(meta) {
        Error::Query(m) if m.contains("Security") || m.to_lowercase().contains("auth") => {
            Error::AuthFailed(m)
        }
        e => e,
    }
}

async fn tls(tcp: TcpStream, t: &Target) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    use rustls::pki_types::ServerName;
    let config = client_config(t)?;
    let name = ServerName::try_from(t.server_name.clone())
        .map_err(|e| Error::Connect(format!("nombre de servidor inválido: {e}")))?;
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .map_err(|e| t.watch.explain(tls_error(e)))
}

/// A target's rustls configuration: with `+ssc` the pinned certificate (any
/// on first use, see `dbine_tls::PinVerifier`), else the webpki and system
/// roots plus `trustedCertificates`.
fn client_config(t: &Target) -> Result<rustls::ClientConfig> {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(Error::connect)?;
    if t.trust_cert {
        return Ok(builder
            .dangerous()
            .with_custom_certificate_verifier(dbine_tls::PinVerifier::watched(
                t.pin.clone(),
                t.watch.clone(),
            ))
            .with_no_client_auth());
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if let Some(path) = &t.ca_file {
        let certs = CertificateDer::pem_file_iter(path)
            .and_then(|i| i.collect::<std::result::Result<Vec<_>, _>>())
            .map_err(|e| {
                Error::Connect(format!("no se pudo leer trustedCertificates ({path}): {e}"))
            })?;
        if certs.is_empty() {
            return Err(Error::Connect(format!(
                "trustedCertificates ({path}) no tiene certificados PEM"
            )));
        }
        for cert in certs {
            roots.add(cert).map_err(|e| {
                Error::Connect(format!("certificado inválido en trustedCertificates: {e}"))
            })?;
        }
    }
    Ok(builder.with_root_certificates(roots).with_no_client_auth())
}

/// A failed handshake, naming the option to change.
fn tls_error(e: std::io::Error) -> Error {
    let untrusted = e
        .get_ref()
        .and_then(|r| r.downcast_ref::<rustls::Error>())
        .is_some_and(|r| matches!(r, rustls::Error::InvalidCertificate(_)));
    if untrusted {
        Error::Connect(format!(
            "el certificado del servidor no es de confianza ({e}): indicá su CA en trustedCertificates, o elegí bolt+ssc en scheme para no verificarlo"
        ))
    } else {
        Error::Connect(format!(
            "falló la negociación TLS: {e} (si el servidor no usa TLS, elegí bolt en scheme)"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        let mut c = ConnectionConfig {
            host: "neo4j+s://abc.databases.neo4j.io".into(),
            ..Default::default()
        };
        let t = target(&c, 7687);
        assert_eq!(
            (t.host.as_str(), t.port, t.tls, t.trust_cert),
            ("abc.databases.neo4j.io", 7687, true, false)
        );
        c.host = "bolt+ssc://db:7688".into();
        let t = target(&c, 7687);
        assert_eq!(
            (t.host.as_str(), t.port, t.tls, t.trust_cert),
            ("db", 7688, true, true)
        );
        c.host = "localhost".into();
        c.port = 17687;
        let t = target(&c, 7687);
        assert_eq!(
            (t.host.as_str(), t.port, t.tls),
            ("localhost", 17687, false)
        );
        c.host = "[::1]:9000".into();
        assert_eq!(target(&c, 7687).port, 9000);
        assert_eq!(target(&c, 7687).host, "::1");
    }

    fn legacy(encrypt: bool, trust: bool, host: &str) -> (bool, bool) {
        let t = target(
            &ConnectionConfig {
                host: host.into(),
                encrypt,
                trust_server_certificate: trust,
                ..Default::default()
            },
            7687,
        );
        (t.tls, t.trust_cert)
    }

    #[test]
    fn saved_connections_keep_their_tls() {
        // [ff, ft, tf, tt] → bolt, bolt, bolt+s, bolt+ssc, as before.
        assert_eq!(legacy(false, false, "db"), (false, false));
        assert_eq!(legacy(false, true, "db"), (false, true));
        assert_eq!(legacy(true, false, "db"), (true, false));
        assert_eq!(legacy(true, true, "db"), (true, true));
        // A +s scheme in the host turned TLS on and kept the trust box.
        assert_eq!(legacy(false, true, "neo4j+s://db"), (true, true));
        assert_eq!(legacy(false, false, "neo4j+s://db"), (true, false));
        // A plain scheme in the host never turned TLS off.
        assert_eq!(legacy(true, false, "bolt://db"), (true, false));
    }

    #[test]
    fn the_scheme_option_decides_and_new_connections_verify() {
        let f = &tls_fields()[0];
        assert_eq!(f.default, "bolt+s");
        assert!(f.tls_level_of(f.default).unwrap().verified());
        let t = |scheme: &str, host: &str| {
            let mut c = ConnectionConfig {
                host: host.into(),
                // The bools are dual-written; the option wins.
                encrypt: true,
                trust_server_certificate: true,
                ..Default::default()
            };
            c.options.insert(SCHEME.into(), scheme.into());
            let t = target(&c, 7687);
            (t.tls, t.trust_cert)
        };
        assert_eq!(t("bolt+s", "db"), (true, false));
        assert_eq!(t("bolt+ssc", "db"), (true, true));
        assert_eq!(t("bolt", "db"), (false, false));
        assert_eq!(t("neo4j+s", "db"), (true, false));
        assert_eq!(t("neo4j", "db"), (false, false));
        // The host's scheme wins.
        assert_eq!(t("bolt", "bolt+s://db"), (true, false));
        assert_eq!(t("bolt+s", "neo4j+ssc://db"), (true, true));
    }

    #[test]
    fn the_certificate_names_the_real_server_through_a_tunnel() {
        let mut c = ConnectionConfig {
            host: "127.0.0.1".into(),
            port: 40001,
            ..Default::default()
        };
        c.options.insert(
            dbine_driver::TUNNEL_ORIGIN_HOST.into(),
            "graph.example.com".into(),
        );
        c.options
            .insert(TRUSTED_CERTIFICATES.into(), " /tmp/ca.pem ".into());
        let t = target(&c, 7687);
        assert_eq!((t.host.as_str(), t.port), ("127.0.0.1", 40001));
        assert_eq!(t.server_name, "graph.example.com");
        assert_eq!(t.ca_file.as_deref(), Some("/tmp/ca.pem"));
        c.options.clear();
        c.host = "neo4j+s://abc.databases.neo4j.io:7687".into();
        assert_eq!(target(&c, 7687).server_name, "abc.databases.neo4j.io");
    }

    #[test]
    fn versions_from_the_manifest() {
        // The Bolt docs' example: 5.8–5.6 and 4.4–4.0.
        assert_eq!(pick(&[[0, 2, 8, 5], [0, 4, 4, 4]]), Some((5, 8)));
        // Neo4j 2025.10+: 6.0 first.
        assert_eq!(
            pick(&[[0, 0, 0, 6], [0, 8, 8, 5], [0, 4, 4, 4]]),
            Some((6, 0))
        );
        // A future 6.1 server that still speaks 6.0.
        assert_eq!(pick(&[[0, 1, 1, 6]]), Some((6, 0)));
        // Only 5.5 (never used) and 3.0: nothing.
        assert_eq!(pick(&[[0, 0, 5, 5], [0, 0, 0, 3]]), None);
        assert_eq!(pick(&[[0, 0, 2, 4]]), Some((4, 2)));
        // The manifest goes first, the classic proposals after it.
        assert_eq!(&HANDSHAKE[4..8], &MANIFEST_V1);
        assert_eq!(&HANDSHAKE[8..12], &[0, 4, 4, 5]);
    }

    #[test]
    fn gql_statuses_read_as_notifications() {
        let status = |code: Option<&str>, sev: &str| {
            let mut s = vec![
                ("gql_status", Value::from("01N00")),
                (
                    "status_description",
                    Value::from("warn: feature deprecated"),
                ),
            ];
            if let Some(c) = code {
                s.push(("neo4j_code", Value::from(c)));
                s.push(("title", Value::from("Deprecated")));
                s.push(("description", Value::from("Use the other one")));
            }
            s.push((
                "diagnostic_record",
                map([
                    ("_severity", Value::from(sev)),
                    ("_classification", Value::from("DEPRECATION")),
                    (
                        "_position",
                        map([
                            ("offset", Value::Int(7)),
                            ("line", Value::Int(1)),
                            ("column", Value::Int(8)),
                        ]),
                    ),
                ]),
            ));
            Value::Map(s.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
        };
        let meta = map([
            ("t_last", Value::Int(1)),
            (
                "statuses",
                Value::List(vec![
                    status(None, "INFORMATION"),
                    status(
                        Some("Neo.ClientNotification.Statement.FeatureDeprecationWarning"),
                        "WARNING",
                    ),
                ]),
            ),
        ]);
        let got = gql_compat(SUCCESS, meta);
        let notes = got.get("notifications").unwrap().as_list();
        assert_eq!(notes.len(), 1);
        let n = &notes[0];
        assert_eq!(
            n.get("code").and_then(Value::as_str),
            Some("Neo.ClientNotification.Statement.FeatureDeprecationWarning")
        );
        assert_eq!(n.get("severity").and_then(Value::as_str), Some("WARNING"));
        assert_eq!(n.get("title").and_then(Value::as_str), Some("Deprecated"));
        assert_eq!(
            n.get("position").and_then(|p| p.get("line")),
            Some(&Value::Int(1))
        );
        // Failures: neo4j_code is the code.
        let f = gql_compat(
            FAILURE,
            map([
                ("gql_status", Value::from("42001")),
                ("message", Value::from("x")),
                (
                    "neo4j_code",
                    Value::from("Neo.ClientError.Statement.SyntaxError"),
                ),
            ]),
        );
        assert!(
            matches!(failure(&f), Error::Query(t) if t == "x (Neo.ClientError.Statement.SyntaxError)")
        );
        // A 5.4 reply is left as it is.
        let old = map([("code", Value::from("c")), ("message", Value::from("m"))]);
        assert_eq!(gql_compat(FAILURE, old.clone()), old);
    }

    #[test]
    fn failures_map_to_errors() {
        let m = map([
            ("code", Value::from("Neo.ClientError.Security.Unauthorized")),
            ("message", Value::from("bad")),
        ]);
        assert!(matches!(failure(&m), Error::AuthFailed(_)));
        let m = map([
            ("code", Value::from("Neo.ClientError.Statement.SyntaxError")),
            ("message", Value::from("x")),
        ]);
        assert!(
            matches!(failure(&m), Error::Query(t) if t == "x (Neo.ClientError.Statement.SyntaxError)")
        );
    }
}
