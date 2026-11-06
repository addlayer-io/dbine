//! The channel between DBine and DBine Bastion, end to end inside the
//! WebSocket: a load balancer or proxy that terminates TLS can't read it.
//!
//! - Noise `IK` (X25519, ChaCha20-Poly1305, BLAKE2s). DBine starts it with
//!   its computer's static key, registered when it signed in, and Bastion's
//!   static key, which it pinned from the console. Bastion checks the
//!   computer's key is the one the bearer token is bound to: a token taken to
//!   another computer can't finish the handshake.
//! - Handshake payloads are JSON: DBine `{"v":1,"compress":["zstd"]}`,
//!   Bastion `{"v":1,"compress":"zstd"}` (or null).
//! - After it, each WebSocket binary message carries one or more frames of
//!   DBine's driver-host protocol, each prefixed with its length (u32, little
//!   endian): whatever is ready is sent together, so an export's millions of
//!   small row frames travel as a few hundred-KB messages. The plaintext is a
//!   flags byte (bit 0: zstd) and those frames, compressed when that makes
//!   them smaller, encrypted in chunks of at most 65,535 bytes of ciphertext
//!   (Noise's message limit), concatenated.
//!
//! DBine Bastion has the same code (dbine-enterprise,
//! crates/bastion/src/channel.rs): change both together.

pub const PATTERN: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
pub const VERSION: u32 = 1;
const MAX_MSG: usize = 65_535;
const TAG: usize = 16;
const MAX_PLAIN: usize = MAX_MSG - TAG;
/// Messages smaller than this aren't worth compressing.
const COMPRESS_FROM: usize = 1024;
/// How much a sender packs into one message before sealing it.
pub const PACK: usize = 256 * 1024;
const ZSTD: u8 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("noise: {0}")]
    Noise(#[from] snow::Error),
    #[error("{0}")]
    Bad(String),
}

/// A new static key pair: (private, public).
pub fn keypair() -> (Vec<u8>, Vec<u8>) {
    let kp = snow::Builder::new(PATTERN.parse().expect("pattern"))
        .generate_keypair()
        .expect("a key pair");
    (kp.private, kp.public)
}

/// An established channel.
pub struct Channel {
    t: snow::TransportState,
    compress: bool,
    buf: Vec<u8>,
}

impl Channel {
    fn new(t: snow::TransportState, compress: bool) -> Self {
        Channel {
            t,
            compress,
            buf: vec![0u8; MAX_MSG],
        }
    }

    /// Frame bodies, ready to send as one message.
    pub fn seal_all(&mut self, frames: &[Vec<u8>]) -> Result<Vec<u8>, ChannelError> {
        let mut body = Vec::with_capacity(frames.iter().map(|f| f.len() + 4).sum());
        for f in frames {
            body.extend_from_slice(&(f.len() as u32).to_le_bytes());
            body.extend_from_slice(f);
        }
        self.seal_raw(&body)
    }

    /// One frame body, ready to send.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn seal(&mut self, frame: &[u8]) -> Result<Vec<u8>, ChannelError> {
        self.seal_all(std::slice::from_ref(&frame.to_vec()))
    }

    fn seal_raw(&mut self, body: &[u8]) -> Result<Vec<u8>, ChannelError> {
        let mut plain = Vec::with_capacity(body.len() + 1);
        let packed = (self.compress && body.len() >= COMPRESS_FROM)
            .then(|| zstd::bulk::compress(body, 1).ok())
            .flatten()
            .filter(|z| z.len() < body.len());
        match packed {
            Some(z) => {
                plain.push(ZSTD);
                plain.extend_from_slice(&z);
            }
            None => {
                plain.push(0);
                plain.extend_from_slice(body);
            }
        }
        let mut out = Vec::with_capacity(plain.len() + TAG * (plain.len() / MAX_PLAIN + 1));
        for chunk in plain.chunks(MAX_PLAIN) {
            let n = self.t.write_message(chunk, &mut self.buf)?;
            out.extend_from_slice(&self.buf[..n]);
        }
        Ok(out)
    }

    /// A received message, back to its frame bodies.
    pub fn open_all(&mut self, msg: &[u8]) -> Result<Vec<Vec<u8>>, ChannelError> {
        let body = self.open_raw(msg)?;
        let mut out = Vec::new();
        let mut rest = &body[..];
        while !rest.is_empty() {
            let len = rest
                .get(..4)
                .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")) as usize)
                .ok_or_else(|| ChannelError::Bad("cut frame".into()))?;
            let f = rest
                .get(4..4 + len)
                .ok_or_else(|| ChannelError::Bad("cut frame".into()))?;
            out.push(f.to_vec());
            rest = &rest[4 + len..];
        }
        Ok(out)
    }

    /// A received message holding one frame, back to its body.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn open(&mut self, msg: &[u8]) -> Result<Vec<u8>, ChannelError> {
        let mut all = self.open_all(msg)?;
        if all.len() != 1 {
            return Err(ChannelError::Bad("expected one frame".into()));
        }
        Ok(all.remove(0))
    }

    fn open_raw(&mut self, msg: &[u8]) -> Result<Vec<u8>, ChannelError> {
        let mut plain = Vec::with_capacity(msg.len());
        for chunk in msg.chunks(MAX_MSG) {
            let n = self.t.read_message(chunk, &mut self.buf)?;
            plain.extend_from_slice(&self.buf[..n]);
        }
        let (flags, body) = plain
            .split_first()
            .ok_or_else(|| ChannelError::Bad("empty frame".into()))?;
        if flags & ZSTD != 0 {
            zstd::stream::decode_all(body).map_err(|e| ChannelError::Bad(e.to_string()))
        } else {
            Ok(body.to_vec())
        }
    }
}

/// Bastion's side: answers DBine's first message. `expected_peer` is the
/// computer key the token is bound to (base64); returns the channel and the
/// answer to send.
// Bastion's side: DBine only runs it in the tests.
#[cfg_attr(not(test), allow(dead_code))]
pub fn accept(
    private: &[u8],
    first: &[u8],
    expected_peer: &str,
) -> Result<(Channel, Vec<u8>), ChannelError> {
    use base64::Engine;
    let mut hs = snow::Builder::new(PATTERN.parse().expect("pattern"))
        .local_private_key(private)
        .build_responder()?;
    let mut payload = vec![0u8; MAX_MSG];
    let n = hs.read_message(first, &mut payload)?;
    let peer = hs
        .get_remote_static()
        .map(|k| base64::engine::general_purpose::STANDARD.encode(k))
        .ok_or_else(|| ChannelError::Bad("missing computer key".into()))?;
    if peer != expected_peer {
        return Err(ChannelError::Bad(
            "this token belongs to another computer".into(),
        ));
    }
    let hello: serde_json::Value = serde_json::from_slice(&payload[..n]).unwrap_or_default();
    let compress = hello["compress"]
        .as_array()
        .is_some_and(|a| a.iter().any(|c| c == "zstd"));
    let answer = serde_json::json!({
        "v": VERSION,
        "compress": if compress { Some("zstd") } else { None },
    });
    let mut out = vec![0u8; MAX_MSG];
    let n = hs.write_message(answer.to_string().as_bytes(), &mut out)?;
    out.truncate(n);
    Ok((Channel::new(hs.into_transport_mode()?, compress), out))
}

/// DBine's side, for tests: the first message, and a function that takes
/// Bastion's answer and gives the channel.
pub fn initiate(
    private: &[u8],
    bastion_public: &[u8],
) -> Result<(Vec<u8>, snow::HandshakeState), ChannelError> {
    let mut hs = snow::Builder::new(PATTERN.parse().expect("pattern"))
        .local_private_key(private)
        .remote_public_key(bastion_public)
        .build_initiator()?;
    let mut out = vec![0u8; MAX_MSG];
    let hello = serde_json::json!({ "v": VERSION, "compress": ["zstd"] });
    let n = hs.write_message(hello.to_string().as_bytes(), &mut out)?;
    out.truncate(n);
    Ok((out, hs))
}

pub fn finish(mut hs: snow::HandshakeState, answer: &[u8]) -> Result<Channel, ChannelError> {
    let mut payload = vec![0u8; MAX_MSG];
    let n = hs.read_message(answer, &mut payload)?;
    let v: serde_json::Value = serde_json::from_slice(&payload[..n]).unwrap_or_default();
    let compress = v["compress"] == "zstd";
    Ok(Channel::new(hs.into_transport_mode()?, compress))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn frames_cross_compressed_and_chunked_and_only_the_bound_computer_gets_in() {
        let (bp, bpub) = keypair();
        let (dp, dpub) = keypair();
        let dkey = base64::engine::general_purpose::STANDARD.encode(&dpub);

        let (first, hs) = initiate(&dp, &bpub).unwrap();
        let (mut bastion, answer) = accept(&bp, &first, &dkey).unwrap();
        let mut dbine = finish(hs, &answer).unwrap();

        // Small, big and compressible, big and random-ish: all round-trip.
        let small = b"hello".to_vec();
        let big: Vec<u8> = (0..300_000u32)
            .flat_map(|i| (i % 7).to_le_bytes())
            .collect();
        let noisy: Vec<u8> = (0..200_000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        for body in [&small, &big, &noisy] {
            let m = dbine.seal(body).unwrap();
            assert_eq!(&bastion.open(&m).unwrap(), body);
            let m = bastion.seal(body).unwrap();
            assert_eq!(&dbine.open(&m).unwrap(), body);
        }
        // The repetitive one went compressed: far smaller on the wire.
        assert!(dbine.seal(&big).unwrap().len() < big.len() / 10);
        // Many small frames in one message: compressed together.
        let rows: Vec<Vec<u8>> = (0..5000u32)
            .map(|i| format!("row {i} cliente {} pedido estándar", i % 50).into_bytes())
            .collect();
        let m = bastion.seal_all(&rows).unwrap();
        assert!(m.len() < rows.iter().map(|r| r.len()).sum::<usize>() / 3);
        assert_eq!(dbine.open_all(&m).unwrap(), rows);
        // Tampering fails.
        let mut m = dbine.seal(&small).unwrap();
        m[3] ^= 1;
        assert!(bastion.open(&m).is_err());

        // Another computer with the same token: refused.
        let (op, _) = keypair();
        let (first, _) = initiate(&op, &bpub).unwrap();
        assert!(accept(&bp, &first, &dkey).is_err());
        // A Bastion impostor without the pinned key can't answer.
        let (fake, _) = keypair();
        let (first, _) = initiate(&dp, &bpub).unwrap();
        assert!(accept(&fake, &first, &dkey).is_err());
    }
}
