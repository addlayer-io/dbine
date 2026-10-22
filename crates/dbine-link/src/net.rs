//! Reaching DBine from outside the local network, with iroh: QUIC
//! connections dialed by public key, direct when the networks allow it
//! (hole punching, port mapping, IPv6) and through a relay server when they
//! don't.
//!
//! - **Relays:** iroh's public ones by default (free, best effort, run by
//!   n0), or the user's own (`iroh-relay`, for a private network). With the
//!   public ones, iroh's address lookup finds DBine by its id; with one's
//!   own, nothing else is used: the phone dials DBine's relay directly.
//! - **What a relay sees:** encrypted QUIC packets. The channel's Noise
//!   handshake and frames ([`crate::Channel`]) run on top, with the keys of
//!   the QR code, exactly as on the local network.

use crate::{Remote, Result};
use iroh::endpoint::{presets, Connection, RecvStream, SendStream};
pub use iroh::Endpoint;
use iroh::{EndpointAddr, EndpointId, RelayMap, RelayMode, RelayUrl, SecretKey};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

/// The protocol both ends speak over iroh.
pub const ALPN: &[u8] = b"dbine-link/1";
/// Dialing DBine through iroh waits at most this long.
pub const CONNECT_LIMIT: Duration = Duration::from_secs(25);

/// Which relays to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Relays {
    /// iroh's public relays and address lookup.
    Public,
    /// Only these relays (an `iroh-relay` of one's own), no lookup.
    Own(Vec<String>),
}

impl Relays {
    pub fn from_urls(urls: &[String]) -> Relays {
        if urls.is_empty() {
            Relays::Public
        } else {
            Relays::Own(urls.to_vec())
        }
    }

    pub fn urls(&self) -> Vec<String> {
        match self {
            Relays::Public => Vec::new(),
            Relays::Own(u) => u.clone(),
        }
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn relay_urls(urls: &[String]) -> Result<Vec<RelayUrl>> {
    urls.iter()
        .map(|u| {
            u.trim()
                .parse::<RelayUrl>()
                .map_err(|e| format!("«{u}» no es una URL de servidor de paso válida: {e}"))
        })
        .collect()
}

/// Check a list of relay URLs before saving it.
pub fn check_relays(urls: &[String]) -> Result<()> {
    for u in relay_urls(urls)? {
        if u.scheme() != "https" {
            return Err(format!("el servidor de paso {u} tiene que usar https"));
        }
    }
    Ok(())
}

/// An endpoint: DBine's (it accepts [`ALPN`]) or a phone's (it only dials).
pub async fn endpoint(secret: [u8; 32], relays: &Relays, accept: bool) -> Result<Endpoint> {
    let b = match relays {
        Relays::Public => Endpoint::builder(presets::N0),
        Relays::Own(urls) => Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Custom(
            relay_urls(urls)?.into_iter().collect::<RelayMap>(),
        )),
    };
    let b = b.secret_key(SecretKey::from_bytes(&secret));
    let b = if accept {
        b.alpns(vec![ALPN.to_vec()])
    } else {
        b
    };
    b.bind().await.map_err(err)
}

/// An endpoint's id, as the QR code carries it.
pub fn id_of(secret: [u8; 32]) -> String {
    SecretKey::from_bytes(&secret).public().to_string()
}

/// A fresh secret key for an endpoint.
pub fn new_secret() -> [u8; 32] {
    SecretKey::generate().to_bytes()
}

/// Where DBine is: its id, plus its relays when they're one's own (with the
/// public ones, iroh's lookup knows).
fn addr_of(remote: &Remote) -> Result<EndpointAddr> {
    let id: EndpointId = remote
        .node
        .parse()
        .map_err(|e| format!("el código de DBine no es válido: {e}"))?;
    let mut addr = EndpointAddr::new(id);
    for u in relay_urls(&remote.relays)? {
        addr = addr.with_relay_url(u);
    }
    Ok(addr)
}

/// A bidirectional iroh stream as one byte stream; it keeps its connection
/// open while it lives.
pub struct Stream {
    conn: Option<Connection>,
    send: SendStream,
    recv: RecvStream,
}

impl Stream {
    fn new(conn: Connection, send: SendStream, recv: RecvStream) -> Stream {
        Stream {
            conn: Some(conn),
            send,
            recv,
        }
    }
}

/// Closing a QUIC connection drops what the other side hasn't read yet:
/// finish the stream and keep the connection until the other side closes
/// (or a few seconds), so a last message (a pairing's answer) arrives.
impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.send.finish();
        if let (Some(conn), Ok(rt)) = (self.conn.take(), tokio::runtime::Handle::try_current()) {
            rt.spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(5), conn.closed()).await;
            });
        }
    }
}

impl tokio::io::AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.recv).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.send)
            .poll_write(cx, buf)
            .map_err(std::io::Error::other)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.send)
            .poll_flush(cx)
            .map_err(std::io::Error::other)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.send)
            .poll_shutdown(cx)
            .map_err(std::io::Error::other)
    }
}

/// The phone dials DBine.
pub async fn dial(ep: &Endpoint, remote: &Remote) -> Result<Stream> {
    let run = async {
        let conn = ep.connect(addr_of(remote)?, ALPN).await.map_err(err)?;
        let (send, recv) = conn.open_bi().await.map_err(err)?;
        Ok::<_, String>(Stream::new(conn, send, recv))
    };
    tokio::time::timeout(CONNECT_LIMIT, run)
        .await
        .map_err(|_| "DBine no respondió por internet".to_string())?
}

/// DBine takes the next phone that dials it. `None`: the endpoint closed.
pub async fn next(ep: &Endpoint) -> Option<Result<Stream>> {
    let incoming = ep.accept().await?;
    Some(
        async {
            let conn = incoming.await.map_err(err)?;
            // The phone writes first (the mode byte), so the stream shows up.
            let (send, recv) = tokio::time::timeout(crate::HANDSHAKE_LIMIT, conn.accept_bi())
                .await
                .map_err(|_| "sin stream".to_string())?
                .map_err(err)?;
            Ok(Stream::new(conn, send, recv))
        }
        .await,
    )
}

/// DBine's current relay, to tell the phone where to look next time.
pub fn home_relay(ep: &Endpoint) -> Option<String> {
    ep.addr().relay_urls().next().map(|u| u.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Accepted, Keys};

    /// Two endpoints on this machine, no relays (direct addresses): the
    /// channel runs over iroh as over TCP.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_channel_runs_over_iroh() {
        let desk_secret = new_secret();
        let a = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .secret_key(SecretKey::from_bytes(&desk_secret))
            .alpns(vec![ALPN.to_vec()])
            .bind()
            .await
            .unwrap();
        let b = Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await
            .unwrap();
        let desktop = Keys::generate().unwrap();
        let phone = Keys::generate().unwrap();
        let known = phone.public.clone();
        let d2 = desktop.clone();
        let a2 = a.clone();
        let server = tokio::spawn(async move {
            let s = next(&a2).await.unwrap().unwrap();
            let Accepted::Connected(mut ch) =
                crate::accept(s, &d2, None, |k| k == known.as_slice())
                    .await
                    .unwrap()
            else {
                panic!()
            };
            let got = ch.recv().await.unwrap();
            ch.send(&got).await.unwrap();
        });
        // Dial its port on loopback: the machine's firewall may filter a
        // test binary's traffic on the network addresses.
        let port = a
            .bound_sockets()
            .iter()
            .find(|s| s.is_ipv4())
            .unwrap()
            .port();
        let addr = EndpointAddr::from_parts(
            a.id(),
            [iroh::TransportAddr::Ip(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                port,
            )))],
        );
        let conn = b.connect(addr, ALPN).await.unwrap();
        let (send, recv) = conn.open_bi().await.unwrap();
        let s = Stream::new(conn, send, recv);
        let mut ch = crate::connect(s, &desktop.public, &phone).await.unwrap();
        ch.send(b"por internet").await.unwrap();
        assert_eq!(ch.recv().await.unwrap(), b"por internet");
        server.await.unwrap();
        assert_eq!(id_of(desk_secret), a.id().to_string());
    }

    #[test]
    fn relay_urls_are_checked() {
        assert!(check_relays(&["https://relay.example.com".into()]).is_ok());
        assert!(check_relays(&["http://relay.example.com".into()]).is_err());
        assert!(check_relays(&["no es una url".into()]).is_err());
        assert_eq!(Relays::from_urls(&[]), Relays::Public);
    }
}
