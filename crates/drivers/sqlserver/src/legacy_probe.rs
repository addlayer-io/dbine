//! Whether a server that failed the TLS 1.2 handshake only speaks TLS
//! 1.0/1.1.
//!
//! rustls offers TLS 1.2+ with AEAD suites only, so an old server (SQL
//! Server 2008 R2 / 2012 without the TLS 1.2 update) has nothing to pick:
//! depending on its TLS stack it answers with a `handshake_failure` alert
//! or just closes the connection, which reads like any other dropped
//! handshake. To tell it apart, a second connection sends PRELOGIN and then
//! a TLS 1.1 ClientHello with the old suites, inside a PRELOGIN packet as
//! TDS does, and reads whether the server answers with a TLS 1.0/1.1
//! ServerHello. It's plaintext only: no key exchange, no login, nothing of
//! the user's is sent, and the connection is dropped right after.
//!
//! The answer only lets the app *ask*: a man in the middle can fake it as
//! easily as it can break TLS 1.2, which is why nothing connects with
//! legacy TLS without the user's stored yes (docs/tls-options.md).

use tiberius::{Config, SqlBrowser};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The whole probe, connection included.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// TDS packet type PRELOGIN (also the TLS handshake's, before login).
const PRELOGIN: u8 = 0x12;

/// The server's PRELOGIN version when it answers a TLS 1.0/1.1 ClientHello
/// with a TLS 1.0/1.1 ServerHello; None otherwise (it negotiates nothing
/// older, it refused, or the probe failed).
pub(crate) async fn old_tls_only(config: &Config) -> Option<u32> {
    tokio::time::timeout(TIMEOUT, probe(config))
        .await
        .ok()
        .flatten()
}

async fn probe(config: &Config) -> Option<u32> {
    let mut tcp = TcpStream::connect_named(config).await.ok()?;
    tcp.set_nodelay(true).ok()?;
    tcp.write_all(&packet(&prelogin())).await.ok()?;
    let answer = read_packet(&mut tcp).await?;
    let (version, encryption) = parse_prelogin(&answer)?;
    // ENCRYPT_NOT_SUP: the server has no TLS at all, not an old one.
    if encryption == 0x02 {
        return None;
    }
    tcp.write_all(&packet(&client_hello())).await.ok()?;
    let reply = read_packet(&mut tcp).await?;
    server_hello_version(&reply)
        .filter(|v| matches!(v, 0x0301 | 0x0302))
        .map(|_| version)
}

/// One TDS PRELOGIN packet (the whole message).
fn packet(payload: &[u8]) -> Vec<u8> {
    let len = (payload.len() + 8) as u16;
    let mut p = vec![PRELOGIN, 0x01, (len >> 8) as u8, len as u8, 0, 0, 1, 0];
    p.extend_from_slice(payload);
    p
}

/// A TDS packet's payload, from its header's length.
async fn read_packet(tcp: &mut TcpStream) -> Option<Vec<u8>> {
    let mut header = [0u8; 8];
    tcp.read_exact(&mut header).await.ok()?;
    let len = u16::from_be_bytes([header[2], header[3]]) as usize;
    let mut payload = vec![0u8; len.checked_sub(8)?];
    tcp.read_exact(&mut payload).await.ok()?;
    Some(payload)
}

/// PRELOGIN asking for encryption: VERSION and ENCRYPTION (ON).
fn prelogin() -> Vec<u8> {
    // Two option tokens (type, offset, length) and the terminator.
    let data_at: u16 = 2 * 5 + 1;
    let mut p = vec![0x00];
    p.extend_from_slice(&data_at.to_be_bytes());
    p.extend_from_slice(&6u16.to_be_bytes());
    p.push(0x01);
    p.extend_from_slice(&(data_at + 6).to_be_bytes());
    p.extend_from_slice(&1u16.to_be_bytes());
    p.push(0xFF);
    // VERSION: this client's (anything), then ENCRYPT_ON.
    p.extend_from_slice(&[0x0F, 0, 0, 0, 0, 0]);
    p.push(0x01);
    p
}

/// The server's VERSION (u32, high byte the major) and ENCRYPTION from its
/// PRELOGIN answer.
fn parse_prelogin(p: &[u8]) -> Option<(u32, u8)> {
    let (mut version, mut encryption) = (None, None);
    let mut i = 0;
    while *p.get(i)? != 0xFF {
        let token = p[i];
        let at = u16::from_be_bytes([*p.get(i + 1)?, *p.get(i + 2)?]) as usize;
        let len = u16::from_be_bytes([*p.get(i + 3)?, *p.get(i + 4)?]) as usize;
        let data = p.get(at..at + len)?;
        match token {
            0x00 if len >= 4 => {
                version = Some(u32::from_be_bytes([data[0], data[1], data[2], data[3]]))
            }
            0x01 if len >= 1 => encryption = Some(data[0]),
            _ => {}
        }
        i += 5;
    }
    Some((version?, encryption?))
}

/// A TLS 1.1 ClientHello (so a server picks 1.0 or 1.1) with the suites
/// old SChannel and OpenSSL servers have, and no extensions.
fn client_hello() -> Vec<u8> {
    const SUITES: [u16; 7] = [
        0xC014, // ECDHE-RSA-AES256-SHA
        0xC013, // ECDHE-RSA-AES128-SHA
        0x0035, // AES256-SHA
        0x002F, // AES128-SHA
        0x000A, // DES-CBC3-SHA
        0x0005, // RC4-SHA
        0x00FF, // renegotiation info SCSV
    ];
    let mut body = vec![0x03, 0x02];
    let mut random = [0u8; 32];
    let _ = getrandom::getrandom(&mut random);
    body.extend_from_slice(&random);
    body.push(0); // no session id
    body.extend_from_slice(&((SUITES.len() * 2) as u16).to_be_bytes());
    for s in SUITES {
        body.extend_from_slice(&s.to_be_bytes());
    }
    body.extend_from_slice(&[1, 0]); // compression: null
    let mut hs = vec![0x01, 0, (body.len() >> 8) as u8, body.len() as u8];
    hs.extend_from_slice(&body);
    let mut record = vec![0x16, 0x03, 0x01];
    record.extend_from_slice(&(hs.len() as u16).to_be_bytes());
    record.extend_from_slice(&hs);
    record
}

/// The version a ServerHello record chose (0x0301 is TLS 1.0); None for an
/// alert or anything else.
fn server_hello_version(p: &[u8]) -> Option<u16> {
    // Record: type, version, length; then the handshake: type, length (3).
    if *p.first()? != 0x16 || *p.get(5)? != 0x02 {
        return None;
    }
    Some(u16::from_be_bytes([*p.get(9)?, *p.get(10)?]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prelogin_round_trips() {
        // The server's answer has the same shape as the request.
        let mut answer = prelogin();
        answer[11..15].copy_from_slice(&((11u32 << 24) | 2100).to_be_bytes());
        answer[17] = 0x00;
        assert_eq!(parse_prelogin(&answer), Some(((11 << 24) | 2100, 0x00)));
        assert_eq!(parse_prelogin(&[0x00, 0, 11]), None);
        assert_eq!(parse_prelogin(&[0x00, 0, 200, 0, 6, 0xFF]), None);
        let p = packet(&prelogin());
        assert_eq!((p[0], p[1]), (PRELOGIN, 0x01));
        assert_eq!(u16::from_be_bytes([p[2], p[3]]) as usize, p.len());
    }

    #[test]
    fn the_client_hello_offers_tls_11_and_the_old_suites() {
        let h = client_hello();
        assert_eq!(&h[..3], &[0x16, 0x03, 0x01]);
        assert_eq!(u16::from_be_bytes([h[3], h[4]]) as usize, h.len() - 5);
        assert_eq!((h[5], h[9], h[10]), (0x01, 0x03, 0x02));
        // AES128-SHA is on the list.
        assert!(h.windows(2).any(|w| w == [0x00, 0x2F]));
    }

    #[test]
    fn only_a_server_hello_names_a_version() {
        let hello = [0x16, 0x03, 0x01, 0, 50, 0x02, 0, 0, 46, 0x03, 0x01];
        assert_eq!(server_hello_version(&hello), Some(0x0301));
        // An alert, or a truncated record.
        assert_eq!(server_hello_version(&[0x15, 0x03, 0x01, 0, 2, 2, 40]), None);
        assert_eq!(server_hello_version(&hello[..8]), None);
    }
}
