//! The channel between DBine (the desktop app) and its phone app
//! (docs/mobile-companion.md).
//!
//! - **Pairing:** DBine shows a QR code with an [`Invite`]: its public key,
//!   a one-time secret that expires, and where to reach it. The phone opens
//!   a Noise `IKpsk2` handshake that only completes with that secret, so
//!   DBine learns the phone's public key and the phone pins DBine's.
//! - **After that:** every connection is a Noise `IK` handshake. DBine
//!   accepts it only from a phone it paired; the phone, only from the key it
//!   pinned. Both ends are authenticated, with forward secrecy.
//! - **Traffic:** JSON requests and responses ([`proto`]) in encrypted
//!   frames ([`Channel`]).
//!
//! Nothing here knows about databases: DBine answers the requests with its
//! own sessions, and the connections never leave the desktop.

pub mod proto;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Noise patterns: pairing proves the QR's secret; later connections, the
/// keys each side pinned.
const PAIR: &str = "Noise_IKpsk2_25519_ChaChaPoly_BLAKE2s";
const CONNECT: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
/// The first byte a phone sends: what kind of handshake follows.
const MODE_PAIR: u8 = 1;
const MODE_CONNECT: u8 = 2;
/// A Noise message is at most 65535 bytes, 16 of them the tag.
const CHUNK: usize = 65_535 - 16;
/// The largest request or response, after reassembly.
pub const MAX_MESSAGE: usize = 16 * 1024 * 1024;
/// A handshake that hasn't finished by then is dropped.
pub const HANDSHAKE_LIMIT: Duration = Duration::from_secs(10);
/// Prefix of the QR's text.
const INVITE_PREFIX: &str = "dbine-link:1:";
/// The protocol version both ends speak.
pub const VERSION: u32 = 1;

pub type Result<T> = std::result::Result<T, String>;

/// A static X25519 key pair (32-byte private and public keys).
#[derive(Clone, Serialize, Deserialize)]
pub struct Keys {
    #[serde(with = "b64")]
    pub private: Vec<u8>,
    #[serde(with = "b64")]
    pub public: Vec<u8>,
}

impl std::fmt::Debug for Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keys").field("public", &fingerprint(&self.public)).finish_non_exhaustive()
    }
}

impl Keys {
    pub fn generate() -> Result<Keys> {
        let kp = snow::Builder::new(CONNECT.parse().map_err(err)?).generate_keypair().map_err(err)?;
        Ok(Keys { private: kp.private, public: kp.public })
    }
}

/// A short, readable form of a public key, to compare on both screens.
pub fn fingerprint(public: &[u8]) -> String {
    let hex: String = public.iter().take(6).map(|b| format!("{b:02X}")).collect();
    hex.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join("-")
}

/// Random bytes from the OS.
pub fn random<const N: usize>() -> Result<[u8; N]> {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).map_err(err)?;
    Ok(out)
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

mod b64 {
    use super::*;
    pub fn serialize<S: serde::Serializer>(v: &[u8], s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&URL_SAFE_NO_PAD.encode(v))
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        URL_SAFE_NO_PAD.decode(s).map_err(serde::de::Error::custom)
    }
}

/// What the QR code carries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invite {
    /// The protocol version.
    pub v: u32,
    /// DBine's public key.
    #[serde(with = "b64")]
    pub key: Vec<u8>,
    /// The one-time pairing secret (32 bytes).
    #[serde(with = "b64")]
    pub secret: Vec<u8>,
    /// Where to reach DBine: `ip:port`, the LAN addresses first.
    pub addrs: Vec<String>,
    /// The computer's name, to show on the phone.
    pub name: String,
}

impl Invite {
    /// The QR's text.
    pub fn encode(&self) -> String {
        format!("{INVITE_PREFIX}{}", URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).unwrap_or_default()))
    }

    pub fn decode(text: &str) -> Result<Invite> {
        let body = text.trim().strip_prefix(INVITE_PREFIX).ok_or("no es un código de DBine")?;
        let raw = URL_SAFE_NO_PAD.decode(body).map_err(|_| "el código está dañado".to_string())?;
        let invite: Invite = serde_json::from_slice(&raw).map_err(|_| "el código está dañado".to_string())?;
        if invite.v != VERSION {
            return Err("el código es de otra versión de DBine: actualizá la app".into());
        }
        if invite.key.len() != 32 || invite.secret.len() != 32 || invite.addrs.is_empty() {
            return Err("el código está incompleto".into());
        }
        Ok(invite)
    }
}

/// An encrypted, established channel.
pub struct Channel<S> {
    stream: S,
    noise: snow::TransportState,
    /// The other side's public key.
    pub peer: Vec<u8>,
}

async fn send_raw<S: AsyncWriteExt + Unpin>(stream: &mut S, bytes: &[u8]) -> Result<()> {
    let len = u16::try_from(bytes.len()).map_err(|_| "mensaje demasiado grande".to_string())?;
    stream.write_all(&len.to_be_bytes()).await.map_err(err)?;
    stream.write_all(bytes).await.map_err(err)?;
    stream.flush().await.map_err(err)
}

async fn recv_raw<S: AsyncReadExt + Unpin>(stream: &mut S) -> Result<Vec<u8>> {
    let mut len = [0u8; 2];
    stream.read_exact(&mut len).await.map_err(err)?;
    let mut buf = vec![0u8; u16::from_be_bytes(len) as usize];
    stream.read_exact(&mut buf).await.map_err(err)?;
    Ok(buf)
}

impl<S: AsyncReadExt + AsyncWriteExt + Unpin> Channel<S> {
    /// Send one message, in as many frames as it takes: a 4-byte length,
    /// then the bytes, each frame encrypted on its own.
    pub async fn send(&mut self, msg: &[u8]) -> Result<()> {
        if msg.len() > MAX_MESSAGE {
            return Err("mensaje demasiado grande".into());
        }
        let mut plain = Vec::with_capacity(msg.len() + 4);
        plain.extend_from_slice(&(msg.len() as u32).to_be_bytes());
        plain.extend_from_slice(msg);
        let mut out = vec![0u8; 65_535];
        for chunk in plain.chunks(CHUNK) {
            let n = self.noise.write_message(chunk, &mut out).map_err(err)?;
            send_raw(&mut self.stream, &out[..n]).await?;
        }
        Ok(())
    }

    /// Receive one whole message.
    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        let mut out = vec![0u8; 65_535];
        let mut first = {
            let frame = recv_raw(&mut self.stream).await?;
            let n = self.noise.read_message(&frame, &mut out).map_err(|_| "mensaje inválido".to_string())?;
            out[..n].to_vec()
        };
        if first.len() < 4 {
            return Err("mensaje inválido".into());
        }
        let want = u32::from_be_bytes([first[0], first[1], first[2], first[3]]) as usize;
        if want > MAX_MESSAGE {
            return Err("mensaje demasiado grande".into());
        }
        let mut msg = first.split_off(4);
        while msg.len() < want {
            let frame = recv_raw(&mut self.stream).await?;
            let n = self.noise.read_message(&frame, &mut out).map_err(|_| "mensaje inválido".to_string())?;
            msg.extend_from_slice(&out[..n]);
        }
        if msg.len() != want {
            return Err("mensaje inválido".into());
        }
        Ok(msg)
    }

    pub async fn send_json<T: Serialize>(&mut self, v: &T) -> Result<()> {
        self.send(&serde_json::to_vec(v).map_err(err)?).await
    }

    pub async fn recv_json<T: for<'de> Deserialize<'de>>(&mut self) -> Result<T> {
        serde_json::from_slice(&self.recv().await?).map_err(err)
    }
}

// -- the phone's side ----------------------------------------------------------

/// The phone pairs: proves it has the QR's secret, and both learn each
/// other's key. `hello` travels in the first message, encrypted: the name
/// DBine shows to confirm.
pub async fn pair<S: AsyncReadExt + AsyncWriteExt + Unpin>(mut stream: S, invite: &Invite, phone: &Keys, hello: &[u8]) -> Result<Channel<S>> {
    let hs = snow::Builder::new(PAIR.parse().map_err(err)?)
        .local_private_key(&phone.private)
        .remote_public_key(&invite.key)
        .psk(2, &invite.secret)
        .build_initiator()
        .map_err(err)?;
    stream.write_all(&[MODE_PAIR]).await.map_err(err)?;
    let (stream, noise) = initiate(stream, hs, hello).await?;
    Ok(Channel { stream, noise, peer: invite.key.clone() })
}

/// The phone connects to a DBine it paired with.
pub async fn connect<S: AsyncReadExt + AsyncWriteExt + Unpin>(mut stream: S, desktop_key: &[u8], phone: &Keys) -> Result<Channel<S>> {
    let hs = snow::Builder::new(CONNECT.parse().map_err(err)?)
        .local_private_key(&phone.private)
        .remote_public_key(desktop_key)
        .build_initiator()
        .map_err(err)?;
    stream.write_all(&[MODE_CONNECT]).await.map_err(err)?;
    let (stream, noise) = initiate(stream, hs, &[]).await?;
    Ok(Channel { stream, noise, peer: desktop_key.to_vec() })
}

async fn initiate<S: AsyncReadExt + AsyncWriteExt + Unpin>(mut stream: S, mut hs: snow::HandshakeState, payload: &[u8]) -> Result<(S, snow::TransportState)> {
    let run = async {
        let mut buf = vec![0u8; 65_535];
        let n = hs.write_message(payload, &mut buf).map_err(err)?;
        send_raw(&mut stream, &buf[..n]).await?;
        let reply = recv_raw(&mut stream).await.map_err(|_| "DBine rechazó la conexión".to_string())?;
        let mut out = vec![0u8; 65_535];
        hs.read_message(&reply, &mut out).map_err(|_| "DBine no se pudo verificar: no es la computadora vinculada".to_string())?;
        Ok::<_, String>(())
    };
    tokio::time::timeout(HANDSHAKE_LIMIT, run).await.map_err(|_| "DBine no respondió".to_string())??;
    Ok((stream, hs.into_transport_mode().map_err(err)?))
}

// -- DBine's side --------------------------------------------------------------

/// How a phone came in.
pub enum Accepted<S> {
    /// A new phone, with the secret of the current invite. DBine asks the
    /// user before keeping `channel.peer`.
    Paired { channel: Channel<S>, hello: Vec<u8> },
    /// A phone DBine already knows.
    Connected(Channel<S>),
}

/// DBine answers a phone. `pairing_secret` is the open invite's secret (if
/// any); `known` says whether a public key belongs to a paired phone.
pub async fn accept<S: AsyncReadExt + AsyncWriteExt + Unpin>(
    mut stream: S,
    desktop: &Keys,
    pairing_secret: Option<&[u8]>,
    known: impl Fn(&[u8]) -> bool,
) -> Result<Accepted<S>> {
    let run = async {
        let mut mode = [0u8; 1];
        stream.read_exact(&mut mode).await.map_err(err)?;
        let mut hs = match (mode[0], pairing_secret) {
            (MODE_PAIR, Some(secret)) => snow::Builder::new(PAIR.parse().map_err(err)?).local_private_key(&desktop.private).psk(2, secret).build_responder().map_err(err)?,
            (MODE_PAIR, None) => return Err("no hay un código de vinculación abierto".to_string()),
            (MODE_CONNECT, _) => snow::Builder::new(CONNECT.parse().map_err(err)?).local_private_key(&desktop.private).build_responder().map_err(err)?,
            _ => return Err("protocolo desconocido".to_string()),
        };
        let first = recv_raw(&mut stream).await?;
        let mut hello = vec![0u8; 65_535];
        // With psk2, a wrong secret only shows when the phone reads our
        // reply; the payload's MAC already fails here for IK when keys don't
        // match.
        let n = hs.read_message(&first, &mut hello).map_err(|_| "handshake inválido".to_string())?;
        hello.truncate(n);
        let peer = hs.get_remote_static().ok_or("handshake sin clave")?.to_vec();
        if mode[0] == MODE_CONNECT && !known(&peer) {
            return Err("celular no vinculado".to_string());
        }
        let mut buf = vec![0u8; 65_535];
        let n = hs.write_message(&[], &mut buf).map_err(err)?;
        send_raw(&mut stream, &buf[..n]).await?;
        Ok::<_, String>((mode[0], hs, hello, peer))
    };
    let (mode, hs, hello, peer) = tokio::time::timeout(HANDSHAKE_LIMIT, run).await.map_err(|_| "handshake demasiado lento".to_string())??;
    let channel = Channel { stream, noise: hs.into_transport_mode().map_err(err)?, peer };
    // A pairing with the wrong secret fails on the first transport message
    // (the phone can't decrypt our reply and closes): the caller only keeps
    // the phone after the confirmation exchange in `proto`.
    Ok(if mode == MODE_PAIR { Accepted::Paired { channel, hello } } else { Accepted::Connected(channel) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    fn invite(desktop: &Keys, secret: [u8; 32]) -> Invite {
        Invite { v: VERSION, key: desktop.public.clone(), secret: secret.to_vec(), addrs: vec!["192.168.1.10:27518".into()], name: "mac".into() }
    }

    #[test]
    fn invite_round_trip() {
        let d = Keys::generate().unwrap();
        let i = invite(&d, [7; 32]);
        let text = i.encode();
        assert!(text.starts_with("dbine-link:1:"));
        assert_eq!(Invite::decode(&text).unwrap(), i);
        assert!(Invite::decode("https://example.com").is_err());
        assert!(Invite::decode("dbine-link:1:!!!").is_err());
    }

    #[test]
    fn fingerprints_are_short() {
        assert_eq!(fingerprint(&[0xAB, 0xCD, 0x01, 0x02, 0x03, 0x04, 0x99]), "ABCD-0102-0304");
    }

    #[tokio::test]
    async fn pair_then_connect_and_talk() {
        let desktop = Keys::generate().unwrap();
        let phone = Keys::generate().unwrap();
        let secret: [u8; 32] = random().unwrap();
        let inv = invite(&desktop, secret);

        // Pairing.
        let (a, b) = duplex(1 << 20);
        let d2 = desktop.clone();
        let server = tokio::spawn(async move { accept(b, &d2, Some(&secret), |_| false).await });
        let mut ch = pair(a, &inv, &phone, b"Pixel").await.unwrap();
        let Accepted::Paired { channel: mut sch, hello } = server.await.unwrap().unwrap() else { panic!("not a pairing") };
        assert_eq!(hello, b"Pixel");
        assert_eq!(sch.peer, phone.public);
        ch.send(b"hola").await.unwrap();
        assert_eq!(sch.recv().await.unwrap(), b"hola");
        // A message bigger than one frame.
        let big = vec![42u8; 200_000];
        sch.send(&big).await.unwrap();
        assert_eq!(ch.recv().await.unwrap(), big);

        // Later: a known phone connects.
        let (a, b) = duplex(1 << 20);
        let d2 = desktop.clone();
        let known = phone.public.clone();
        let server = tokio::spawn(async move { accept(b, &d2, None, move |k| k == known.as_slice()).await });
        let mut ch = connect(a, &desktop.public, &phone).await.unwrap();
        let Accepted::Connected(mut sch) = server.await.unwrap().unwrap() else { panic!("not a connection") };
        ch.send_json(&serde_json::json!({"x": 1})).await.unwrap();
        assert_eq!(sch.recv_json::<serde_json::Value>().await.unwrap()["x"], 1);
    }

    #[tokio::test]
    async fn strangers_and_wrong_secrets_are_refused() {
        let desktop = Keys::generate().unwrap();
        let phone = Keys::generate().unwrap();

        // An unknown phone.
        let (a, b) = duplex(1 << 16);
        let d2 = desktop.clone();
        let server = tokio::spawn(async move { accept(b, &d2, None, |_| false).await });
        let client = tokio::spawn({
            let desktop_pub = desktop.public.clone();
            let phone = phone.clone();
            async move { connect(a, &desktop_pub, &phone).await.map(|_| ()) }
        });
        assert!(server.await.unwrap().is_err());
        assert!(client.await.unwrap().is_err());

        // The wrong pairing secret: the phone can't verify DBine's reply.
        let (a, b) = duplex(1 << 16);
        let d2 = desktop.clone();
        let server = tokio::spawn(async move { accept(b, &d2, Some(&[1u8; 32]), |_| false).await });
        let bad = invite(&desktop, [2; 32]);
        assert!(pair(a, &bad, &phone, b"x").await.is_err());
        // DBine may finish its half, but the channel is useless: the phone
        // closed and never confirmed.
        if let Ok(Ok(Accepted::Paired { mut channel, .. })) = server.await {
            assert!(channel.recv().await.is_err());
        }

        // A phone that pins another desktop's key.
        let other = Keys::generate().unwrap();
        let (a, b) = duplex(1 << 16);
        let d2 = desktop.clone();
        let known = phone.public.clone();
        let server = tokio::spawn(async move { accept(b, &d2, None, move |k| k == known.as_slice()).await });
        assert!(connect(a, &other.public, &phone).await.is_err());
        let _ = server.await;
    }
}
