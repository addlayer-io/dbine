#![no_main]
//! The phone link (`dbine_link`): what reaches DBine from the network or a
//! QR code before anything is trusted.
//!
//! The first byte picks what reads the rest:
//! - DBine's side of the handshake (`accept`), on an in-memory stream: the
//!   mode byte, the length-prefixed Noise message and its checks, with a
//!   pairing code open or not;
//! - an invite (`Invite::decode`), the QR's text;
//! - a phone's request (`proto::Request`), the JSON DBine reads from an
//!   established channel.

use dbine_link::{accept, proto::Request, Invite, Keys};
use libfuzzer_sys::fuzz_target;
use std::io;
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A connection whose peer sent `input`; what DBine writes back is kept.
struct Wire<'a> {
    input: &'a [u8],
    output: Vec<u8>,
}

impl AsyncRead for Wire<'_> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let n = buf.remaining().min(self.input.len());
        buf.put_slice(&self.input[..n]);
        self.input = &self.input[n..];
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Wire<'_> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.output.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("a tokio runtime")
    })
}

fuzz_target!(|data: &[u8]| {
    let Some((&what, rest)) = data.split_first() else {
        return;
    };
    match what % 4 {
        0 | 1 => {
            // Fixed keys keep a crash reproducible; any 32 bytes are an
            // X25519 private key.
            let desktop = Keys {
                private: vec![0x42; 32],
                public: vec![0; 32],
            };
            let secret = [0x24u8; 32];
            let pairing = (what % 4 == 1).then_some(&secret[..]);
            let wire = Wire {
                input: rest,
                output: Vec::new(),
            };
            let _ = runtime().block_on(accept(wire, &desktop, pairing, |_| true));
        }
        2 => {
            let _ = Invite::decode(&String::from_utf8_lossy(rest));
        }
        _ => {
            let _ = serde_json::from_slice::<Request>(rest);
        }
    }
});
