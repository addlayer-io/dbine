//! One IPC connection to a kdb+ process, over TCP or TLS (q's `tcps`).
//!
//! kdb+ answers one sync request at a time, in order. A cancel drops the
//! connection: kdb+ has no request to stop a query from a client, so the
//! server finishes it on its own; the session opens a new connection on
//! its next request.

use crate::ipc::{self, K};
use dbine_driver::{ConnectFailure, Error, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// The largest message DBine reads: what the capability it asks for allows
/// kdb+ to send (2 GB). A header that claims more is refused before
/// anything is allocated, and a message is read as its bytes arrive, so a
/// header that lies costs no more memory than what really came.
pub const MAX_MESSAGE: usize = i32::MAX as usize;

/// TLS settings for a `tcps` connection.
#[derive(Clone)]
pub struct Tls {
    pub config: Arc<rustls::ClientConfig>,
    pub server_name: rustls::pki_types::ServerName<'static>,
}

/// Where and as whom to connect (kept to reconnect after a cancel).
#[derive(Clone)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub tls: Option<Tls>,
    pub timeout: Duration,
}

pub struct Conn {
    io: Box<dyn Io>,
}

/// A request that failed. `unsent`: the server can't have run it (the
/// write failed, or the connection closed before a byte of the answer
/// came), so sending it again on a new connection can't run it twice.
pub struct Failed {
    pub error: Error,
    pub unsent: bool,
}

fn io_error(e: std::io::Error) -> Error {
    Error::ConnectFailed(Box::new(ConnectFailure::from_io(&e, e.to_string())))
}

/// TCP keepalive, so a connection a firewall or a NAT dropped while idle
/// is noticed instead of hanging the next request.
fn keepalive(tcp: &TcpStream) {
    let ka = socket2::TcpKeepalive::new().with_time(Duration::from_secs(60));
    #[cfg(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows",
        target_os = "freebsd",
        target_os = "android",
        target_os = "ios"
    ))]
    let ka = ka.with_interval(Duration::from_secs(15));
    socket2::SockRef::from(tcp).set_tcp_keepalive(&ka).ok();
}

impl Conn {
    /// Connect and log in. `after_cancel`: the previous connection was
    /// dropped by a cancel, so a timeout most likely means kdb+ (which
    /// serves one request at a time) is still running that query.
    pub async fn open(t: &Target, after_cancel: bool) -> Result<Self> {
        let fut = async {
            let tcp = TcpStream::connect((t.host.as_str(), t.port))
                .await
                .map_err(io_error)?;
            tcp.set_nodelay(true).ok();
            keepalive(&tcp);
            let io: Box<dyn Io> = match &t.tls {
                None => Box::new(tcp),
                Some(tls) => Box::new(
                    tokio_rustls::TlsConnector::from(tls.config.clone())
                        .connect(tls.server_name.clone(), tcp)
                        .await
                        .map_err(|e| {
                            Error::ConnectFailed(Box::new(ConnectFailure::from_message(format!(
                                "TLS: {e}"
                            ))))
                        })?,
                ),
            };
            let mut c = Conn { io };
            c.login(&t.user, &t.password).await?;
            Ok(c)
        };
        tokio::time::timeout(t.timeout, fut).await.map_err(|_| {
            Error::Connect(if after_cancel {
                "kdb+ no respondió al volver a conectar: el servidor sigue ocupado con la consulta cancelada (kdb+ la termina igual) y atiende un pedido por vez".into()
            } else {
                "tiempo de espera agotado al conectar con kdb+".into()
            })
        })?
    }

    /// The handshake: kdb+ answers one byte (the capability it agrees to)
    /// or closes the connection when it refuses the user.
    async fn login(&mut self, user: &str, password: &str) -> Result<()> {
        self.io
            .write_all(&ipc::handshake(user, password))
            .await
            .map_err(io_error)?;
        self.io.flush().await.map_err(io_error)?;
        let mut b = [0u8; 1];
        match self.io.read(&mut b).await {
            Ok(1) => Ok(()),
            Ok(_) => Err(Error::AuthFailed(
                "kdb+ cerró la conexión al iniciar sesión: el usuario o la contraseña no son válidos, o el servidor no acepta este cliente".into(),
            )),
            Err(e) => Err(io_error(e)),
        }
    }

    /// Send `text` as a sync query and read the answer. Async messages the
    /// server sends meanwhile (`neg[.z.w]`) are skipped; a sync request
    /// from the server (`.z.w`) gets an error back, since DBine serves
    /// none, and the answer is read after it.
    pub async fn call(&mut self, text: &str) -> std::result::Result<K, Failed> {
        self.request(&ipc::query(text)).await
    }

    /// Send `msg` (a whole sync message, see [`ipc::message`]) and read
    /// the answer, as [`Conn::call`] does.
    pub async fn request(&mut self, msg: &[u8]) -> std::result::Result<K, Failed> {
        let unsent = |e: std::io::Error| Failed {
            error: lost(e),
            unsent: true,
        };
        self.io.write_all(msg).await.map_err(unsent)?;
        self.io.flush().await.map_err(unsent)?;
        let mut first = true;
        loop {
            let msg = self.message(first).await?;
            first = false;
            let fatal = |error: Error| Failed {
                error,
                unsent: false,
            };
            let h = ipc::header(msg[..8].try_into().expect("8 bytes"))
                .map_err(|e| fatal(Error::Query(e.0)))?;
            match h.kind {
                ipc::RESPONSE => return ipc::decode(&msg).map_err(|e| fatal(Error::Query(e.0))),
                ipc::SYNC => {
                    let no = ipc::message(
                        ipc::RESPONSE,
                        &K::Error("dbine client: sync requests unsupported".into()),
                    );
                    self.io.write_all(&no).await.map_err(|e| fatal(lost(e)))?;
                    self.io.flush().await.map_err(|e| fatal(lost(e)))?;
                }
                _ => {}
            }
        }
    }

    /// One whole message, header included. `first`: the first after the
    /// request, where a connection closed before any byte means the
    /// request wasn't run.
    async fn message(&mut self, first: bool) -> std::result::Result<Vec<u8>, Failed> {
        let fatal = |error: Error| Failed {
            error,
            unsent: false,
        };
        let mut head = [0u8; 8];
        let n = self.io.read(&mut head).await.map_err(|e| fatal(lost(e)))?;
        if n == 0 {
            return Err(Failed {
                error: lost(std::io::ErrorKind::UnexpectedEof.into()),
                unsent: first,
            });
        }
        self.io
            .read_exact(&mut head[n..])
            .await
            .map_err(|e| fatal(lost(e)))?;
        let h = ipc::header(&head).map_err(|e| fatal(Error::Query(e.0)))?;
        if h.len > MAX_MESSAGE {
            return Err(fatal(Error::Query(format!(
                "kdb+ anunció un mensaje de {} bytes, más de lo que permite su protocolo",
                h.len
            ))));
        }
        let body = (h.len - 8) as u64;
        let mut msg = Vec::with_capacity(h.len.min(1 << 20));
        msg.extend_from_slice(&head);
        (&mut self.io)
            .take(body)
            .read_to_end(&mut msg)
            .await
            .map_err(|e| fatal(lost(e)))?;
        if msg.len() != h.len {
            return Err(fatal(lost(std::io::ErrorKind::UnexpectedEof.into())));
        }
        Ok(msg)
    }
}

/// The connection broke mid-request.
fn lost(e: std::io::Error) -> Error {
    Error::Connect(format!("se perdió la conexión con kdb+: {e}"))
}
