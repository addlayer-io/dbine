//! A small Bolt client (protocol 4.2–5.4): handshake, HELLO / LOGON,
//! RUN + PULL with the result metadata (fields, plans, profiles, counters,
//! notifications) and RESET after a failure. Neo4j, Memgraph and other
//! Bolt servers speak it; one [`Conn`] is one socket, no pooling.

use crate::packstream::{encode, encoded_len, map, Decoder, Value};
use dbine_driver::{ConnectionConfig, Error, Result};
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
const PULL: u8 = 0x3F;
const LOGON: u8 = 0x6A;
const SUCCESS: u8 = 0x70;
const RECORD: u8 = 0x71;
const IGNORED: u8 = 0x7E;
const FAILURE: u8 = 0x7F;

/// Rows asked for per PULL.
const BATCH: i64 = 1000;

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// What a connection closed by [`Conn::close_now`] reads and writes: errors.
struct Closed;

fn closed() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::NotConnected, "conexión Bolt cerrada")
}

impl AsyncRead for Closed {
    fn poll_read(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>, _: &mut tokio::io::ReadBuf<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Err(closed()))
    }
}

impl AsyncWrite for Closed {
    fn poll_write(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>, _: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Err(closed()))
    }
    fn poll_flush(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Err(closed()))
    }
    fn poll_shutdown(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
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
    pub user: String,
    pub password: String,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{} tls={}", self.host, self.port, self.tls)
    }
}

/// `host`, `host:port` or a URI (`neo4j://`, `neo4j+s://`, `bolt+ssc://`,
/// `memgraph://`…): the `+s` schemes turn TLS on, `+ssc` also trusts any
/// certificate.
pub fn target(cfg: &ConnectionConfig, default_port: u16) -> Target {
    let mut host = cfg.host.trim().to_string();
    let mut tls = cfg.encrypt;
    let mut trust = cfg.trust_server_certificate;
    if let Some((scheme, rest)) = host.split_once("://") {
        let scheme = scheme.to_ascii_lowercase();
        if scheme.ends_with("+s") {
            tls = true;
        } else if scheme.ends_with("+ssc") {
            tls = true;
            trust = true;
        }
        host = rest.split('/').next().unwrap_or_default().to_string();
    }
    let mut port = cfg.port_or(default_port);
    // host:port (not an IPv6 literal without brackets).
    if let Some((h, p)) = host.rsplit_once(':') {
        if let (Ok(p), false) = (p.parse::<u16>(), h.contains(':') && !h.ends_with(']')) {
            port = p;
            host = h.to_string();
        }
    }
    let host = host.trim_start_matches('[').trim_end_matches(']').to_string();
    Target {
        host: if host.is_empty() { "localhost".into() } else { host },
        port,
        tls,
        trust_cert: trust,
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
    let msg = meta.get("message").and_then(Value::as_str).unwrap_or("error del servidor");
    let text = if code.is_empty() { msg.to_string() } else { format!("{msg} ({code})") };
    if code.contains("Security.Unauthorized") || code.contains("AuthenticationRateLimit") || code.contains("CredentialsExpired")
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
            .map_err(|_| Error::Connect(format!("tiempo de espera agotado al conectar con {}:{}", t.host, t.port)))?
    }

    async fn open_inner(t: &Target, user_agent: &str) -> Result<Conn> {
        let tcp = TcpStream::connect((t.host.as_str(), t.port))
            .await
            .map_err(|e| Error::Connect(format!("no se pudo conectar con {}:{}: {e}", t.host, t.port)))?;
        tcp.set_nodelay(true).ok();
        let io: Box<dyn Io> = if t.tls { Box::new(tls(tcp, &t.host, t.trust_cert).await?) } else { Box::new(tcp) };
        let mut c = Conn { io, version: (0, 0), server: String::new(), connection_id: String::new() };
        c.handshake().await?;
        c.hello(t, user_agent).await?;
        Ok(c)
    }

    async fn handshake(&mut self) -> Result<()> {
        // Magic, then four proposals: 5.4 down to 5.0, 4.4 down to 4.2.
        let hs = [0x60, 0x60, 0xB0, 0x17, 0, 4, 4, 5, 0, 2, 4, 4, 0, 0, 0, 0, 0, 0, 0, 0];
        self.io.write_all(&hs).await.map_err(io_err)?;
        let mut v = [0u8; 4];
        self.io.read_exact(&mut v).await.map_err(|e| {
            Error::Connect(format!("el servidor no respondió al saludo Bolt (¿es el puerto Bolt, con TLS si hace falta?): {e}"))
        })?;
        if v == [0x48, 0x54, 0x54, 0x50] {
            return Err(Error::Connect("ese puerto habla HTTP, no Bolt (el puerto Bolt suele ser 7687)".into()));
        }
        if v == [0, 0, 0, 0] {
            return Err(Error::Connect("el servidor no acepta ninguna versión de Bolt entre 4.2 y 5.4".into()));
        }
        self.version = (v[3], v[2]);
        Ok(())
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
            extra.push(("bolt_agent".into(), map([("product", Value::from(user_agent))])));
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
        self.server = meta.get("server").and_then(Value::as_str).unwrap_or_default().to_string();
        self.connection_id = meta.get("connection_id").and_then(Value::as_str).unwrap_or_default().to_string();
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
            self.io.read_exact(&mut msg[start..]).await.map_err(io_err)?;
        }
        match Decoder::new(&msg).value().map_err(Error::Connect)? {
            Value::Struct(tag, mut fields) => Ok((tag, if fields.is_empty() { Value::Null } else { fields.swap_remove(0) })),
            other => Err(Error::Connect(format!("mensaje Bolt inesperado: {other:?}"))),
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
        let fields: Vec<String> = meta.get("fields").map(Value::as_list).unwrap_or_default().iter().filter_map(|f| f.as_str().map(str::to_string)).collect();
        loop {
            let (tag, v) = self.recv().await?;
            match tag {
                RECORD => on_row(&fields, match v {
                    Value::List(l) => l,
                    other => vec![other],
                }),
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
                other => return Err(Error::Connect(format!("respuesta Bolt inesperada 0x{other:02X}"))),
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

    async fn expect_success(&mut self) -> Result<()> {
        let (tag, meta) = self.recv().await?;
        match tag {
            SUCCESS => Ok(()),
            FAILURE | IGNORED => {
                let e = if tag == FAILURE { failure(&meta) } else { Error::Query("el servidor ignoró el mensaje".into()) };
                self.reset().await?;
                Err(e)
            }
            other => Err(Error::Connect(format!("respuesta Bolt inesperada 0x{other:02X}"))),
        }
    }

    /// Run and collect every row (catalog queries).
    pub async fn query(&mut self, query: &str, params: Value, extra: Value) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
        let mut rows = Vec::new();
        let s = self.run(query, params, extra, &mut |_, r| rows.push(r)).await?;
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
        Error::Query(m) if m.contains("Security") || m.to_lowercase().contains("auth") => Error::AuthFailed(m),
        e => e,
    }
}

async fn tls(tcp: TcpStream, host: &str, trust_any: bool) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    use rustls::pki_types::ServerName;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(Error::connect)?;
    let config = if trust_any {
        builder.dangerous().with_custom_certificate_verifier(Arc::new(AcceptAny(provider))).with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(cert);
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    let name = ServerName::try_from(host.to_string()).map_err(|e| Error::Connect(format!("nombre de servidor inválido: {e}")))?;
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(name, tcp)
        .await
        .map_err(|e| Error::Connect(format!("falló la negociación TLS: {e}")))
}

/// "Confiar en el certificado del servidor": any certificate, signatures still checked.
#[derive(Debug)]
struct AcceptAny(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        let mut c = ConnectionConfig { host: "neo4j+s://abc.databases.neo4j.io".into(), ..Default::default() };
        let t = target(&c, 7687);
        assert_eq!((t.host.as_str(), t.port, t.tls, t.trust_cert), ("abc.databases.neo4j.io", 7687, true, false));
        c.host = "bolt+ssc://db:7688".into();
        let t = target(&c, 7687);
        assert_eq!((t.host.as_str(), t.port, t.tls, t.trust_cert), ("db", 7688, true, true));
        c.host = "localhost".into();
        c.port = 17687;
        let t = target(&c, 7687);
        assert_eq!((t.host.as_str(), t.port, t.tls), ("localhost", 17687, false));
        c.host = "[::1]:9000".into();
        assert_eq!(target(&c, 7687).port, 9000);
        assert_eq!(target(&c, 7687).host, "::1");
    }

    #[test]
    fn failures_map_to_errors() {
        let m = map([("code", Value::from("Neo.ClientError.Security.Unauthorized")), ("message", Value::from("bad"))]);
        assert!(matches!(failure(&m), Error::AuthFailed(_)));
        let m = map([("code", Value::from("Neo.ClientError.Statement.SyntaxError")), ("message", Value::from("x"))]);
        assert!(matches!(failure(&m), Error::Query(t) if t == "x (Neo.ClientError.Statement.SyntaxError)"));
    }
}
