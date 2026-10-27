//! `insecure` as certificate pinning. redis' own client, asked not to
//! verify, accepts any certificate and takes no verifier of ours, so with
//! `insecure` DBine opens the TLS connections itself (rustls with
//! `dbine_tls::PinVerifier`: the first certificate seen, saved by the app,
//! and only that one afterwards) and hands the stream to redis.

use redis::aio::MultiplexedConnection;
use redis::{AsyncConnectionConfig, RedisConnectionInfo, RedisResult, Value};
use rustls::pki_types::ServerName;
use std::io::{BufReader, Write};
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Duration;

/// Where and how to open a pinned connection.
#[derive(Clone)]
pub(crate) struct Pinned {
    host: String,
    port: u16,
    name: ServerName<'static>,
    pin: Option<String>,
    pub(crate) watch: dbine_tls::PinWatch,
    info: RedisConnectionInfo,
}

impl Pinned {
    pub(crate) fn new(
        host: String,
        port: u16,
        server_name: &str,
        pin: Option<String>,
        info: RedisConnectionInfo,
    ) -> dbine_driver::Result<Self> {
        let name = server_name.trim_matches(['[', ']']).to_string();
        let name = ServerName::try_from(name)
            .or_else(|_| ServerName::try_from(host.trim_matches(['[', ']']).to_string()))
            .map_err(|e| {
                dbine_driver::Error::Connect(format!("nombre de servidor inválido: {e}"))
            })?;
        Ok(Self {
            host,
            port,
            name,
            pin,
            watch: dbine_tls::PinWatch::default(),
            info,
        })
    }

    /// The pin to save: the certificate accepted on first use.
    pub(crate) fn seen(&self) -> Option<String> {
        if self.pin.is_some() {
            return None;
        }
        self.watch.seen()
    }

    fn config(&self) -> RedisResult<Arc<rustls::ClientConfig>> {
        let c = rustls::ClientConfig::builder_with_provider(dbine_tls::provider())
            .with_safe_default_protocol_versions()
            .map_err(std::io::Error::other)?
            .dangerous()
            .with_custom_certificate_verifier(dbine_tls::PinVerifier::watched(
                self.pin.clone(),
                self.watch.clone(),
            ))
            .with_no_client_auth();
        Ok(Arc::new(c))
    }

    /// An async connection (the session's, a bulk load's).
    pub(crate) async fn connect(
        &self,
        config: AsyncConnectionConfig,
    ) -> RedisResult<MultiplexedConnection> {
        let tcp = tokio::net::TcpStream::connect((self.host.as_str(), self.port)).await?;
        tcp.set_nodelay(true).ok();
        let tls = tokio_rustls::TlsConnector::from(self.config()?)
            .connect(self.name.clone(), tcp)
            .await?;
        let (conn, driver) =
            MultiplexedConnection::new_with_config(&self.info, tls, config).await?;
        tokio::spawn(driver);
        Ok(conn)
    }

    /// A blocking connection for the profiler's `MONITOR`, signed in.
    pub(crate) fn blocking(&self, timeout: Duration) -> RedisResult<Blocking> {
        let addr = (self.host.as_str(), self.port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| std::io::Error::other("no address"))?;
        let tcp = std::net::TcpStream::connect_timeout(&addr, timeout)?;
        tcp.set_nodelay(true).ok();
        let conn = rustls::ClientConnection::new(self.config()?, self.name.clone())
            .map_err(std::io::Error::other)?;
        let mut b = Blocking {
            io: BufReader::new(rustls::StreamOwned::new(conn, tcp)),
            parser: redis::Parser::new(),
        };
        if let Some(pw) = self.info.password() {
            let mut auth = redis::cmd("AUTH");
            if let Some(u) = self.info.username() {
                auth.arg(u);
            }
            auth.arg(pw);
            b.send(&auth)?;
            b.recv()?.extract_error()?;
        }
        Ok(b)
    }
}

/// A blocking pinned connection: commands out, replies in.
pub(crate) struct Blocking {
    io: BufReader<rustls::StreamOwned<rustls::ClientConnection, std::net::TcpStream>>,
    parser: redis::Parser,
}

impl Blocking {
    pub(crate) fn send(&mut self, cmd: &redis::Cmd) -> RedisResult<()> {
        let s = self.io.get_mut();
        s.write_all(&cmd.get_packed_command())?;
        s.flush()?;
        Ok(())
    }

    pub(crate) fn recv(&mut self) -> RedisResult<Value> {
        self.parser.parse_value(&mut self.io)
    }
}
