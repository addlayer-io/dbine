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

fn io_error(e: std::io::Error) -> Error {
    Error::ConnectFailed(Box::new(ConnectFailure::from_io(&e, e.to_string())))
}

impl Conn {
    /// Connect and log in.
    pub async fn open(t: &Target) -> Result<Self> {
        let fut = async {
            let tcp = TcpStream::connect((t.host.as_str(), t.port))
                .await
                .map_err(io_error)?;
            tcp.set_nodelay(true).ok();
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
        tokio::time::timeout(t.timeout, fut)
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado al conectar con kdb+".into()))?
    }

    /// The handshake: kdb+ answers one byte (the capability it agrees to)
    /// or closes the connection when it refuses the user.
    async fn login(&mut self, user: &str, password: &str) -> Result<()> {
        self.io
            .write_all(&ipc::handshake(user, password))
            .await
            .map_err(io_error)?;
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
    /// server sends meanwhile (`neg[.z.w]`) are skipped.
    pub async fn call(&mut self, text: &str) -> Result<K> {
        self.io.write_all(&ipc::query(text)).await.map_err(lost)?;
        self.io.flush().await.map_err(lost)?;
        loop {
            let mut head = [0u8; 8];
            self.io.read_exact(&mut head).await.map_err(lost)?;
            let h = ipc::header(&head).map_err(|e| Error::Query(e.0))?;
            let mut msg = vec![0u8; h.len];
            msg[..8].copy_from_slice(&head);
            self.io.read_exact(&mut msg[8..]).await.map_err(lost)?;
            if h.kind != ipc::RESPONSE {
                continue;
            }
            return ipc::decode(&msg).map_err(|e| Error::Query(e.0));
        }
    }
}

/// The connection broke mid-request.
fn lost(e: std::io::Error) -> Error {
    Error::Connect(format!("se perdió la conexión con kdb+: {e}"))
}
