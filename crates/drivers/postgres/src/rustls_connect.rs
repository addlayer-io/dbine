//! tokio-postgres over rustls: the connector a connection's TLS plan builds
//! (`dbine_tls`'s pinned, verify-ca or full check), with the
//! `tls-server-end-point` channel binding SCRAM-SHA-256-PLUS uses.
//!
//! Shared with the Aurora DSQL driver (`#[path]`); keep it free of this
//! crate's other modules.

use sha2::Digest;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, BufReader, ReadBuf};
use tokio_postgres::tls::{self, ChannelBinding, MakeTlsConnect, TlsConnect};
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::ServerName;

/// Makes a TLS connector per connection (and per cancel request), all on
/// the same rustls configuration.
#[derive(Clone)]
pub struct MakeRustlsConnect {
    config: Arc<rustls::ClientConfig>,
}

impl MakeRustlsConnect {
    pub fn new(config: rustls::ClientConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
}

impl<S> MakeTlsConnect<S> for MakeRustlsConnect
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = RustlsStream<S>;
    type TlsConnect = RustlsConnect;
    type Error = io::Error;

    fn make_tls_connect(&mut self, domain: &str) -> io::Result<RustlsConnect> {
        let name = ServerName::try_from(domain.trim_matches(['[', ']']).to_string())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        Ok(RustlsConnect {
            connector: tokio_rustls::TlsConnector::from(self.config.clone()),
            name,
        })
    }
}

pub struct RustlsConnect {
    connector: tokio_rustls::TlsConnector,
    name: ServerName<'static>,
}

impl<S> TlsConnect<S> for RustlsConnect
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = RustlsStream<S>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<RustlsStream<S>>> + Send>>;

    fn connect(self, stream: S) -> Self::Future {
        let stream = BufReader::with_capacity(8192, stream);
        Box::pin(async move {
            let s = self.connector.connect(self.name, stream).await?;
            Ok(RustlsStream(Box::new(s)))
        })
    }
}

pub struct RustlsStream<S>(Box<tokio_rustls::client::TlsStream<BufReader<S>>>);

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for RustlsStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for RustlsStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut *self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut *self.0).poll_shutdown(cx)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> tls::TlsStream for RustlsStream<S> {
    fn channel_binding(&self) -> ChannelBinding {
        let cert = self
            .0
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|c| c.first());
        match cert.and_then(|c| server_end_point(c.as_ref())) {
            Some(hash) => ChannelBinding::tls_server_end_point(hash),
            None => ChannelBinding::none(),
        }
    }
}

/// RFC 5929's `tls-server-end-point`: the certificate hashed with its
/// signature's hash, SHA-256 for MD5 and SHA-1. `None` when that hash isn't
/// known (RSA-PSS, Ed25519…): SCRAM then goes without channel binding.
fn server_end_point(der: &[u8]) -> Option<Vec<u8>> {
    const SHA384: [&[u8]; 2] = [
        // sha384WithRSAEncryption, ecdsa-with-SHA384
        &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0c],
        &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x03],
    ];
    const SHA512: [&[u8]; 2] = [
        &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0d],
        &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x04],
    ];
    const SHA256_OR_WEAKER: [&[u8]; 6] = [
        // sha256WithRSAEncryption, md5WithRSA, sha1WithRSA, ecdsa-with-SHA256,
        // ecdsa-with-SHA1, sha224WithRSA
        &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b],
        &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x04],
        &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x05],
        &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02],
        &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x01],
        &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0e],
    ];
    let oid = signature_oid(der)?;
    if SHA384.contains(&oid) {
        Some(sha2::Sha384::digest(der).to_vec())
    } else if SHA512.contains(&oid) {
        Some(sha2::Sha512::digest(der).to_vec())
    } else if SHA256_OR_WEAKER.contains(&oid) {
        Some(sha2::Sha256::digest(der).to_vec())
    } else {
        None
    }
}

/// The OID of a certificate's `signatureAlgorithm` (the field after
/// `tbsCertificate`).
fn signature_oid(der: &[u8]) -> Option<&[u8]> {
    // A DER element at `at`: (tag, content start, end).
    fn element(d: &[u8], at: usize) -> Option<(u8, usize, usize)> {
        let tag = *d.get(at)?;
        let first = *d.get(at + 1)?;
        let (len, start) = if first < 0x80 {
            (first as usize, at + 2)
        } else {
            let n = (first & 0x7f) as usize;
            if n == 0 || n > 4 {
                return None;
            }
            let mut len = 0usize;
            for i in 0..n {
                len = (len << 8) | *d.get(at + 2 + i)? as usize;
            }
            (len, at + 2 + n)
        };
        let end = start.checked_add(len)?;
        (end <= d.len()).then_some((tag, start, end))
    }
    let (0x30, cert, _) = element(der, 0)? else {
        return None;
    };
    let (0x30, _, tbs_end) = element(der, cert)? else {
        return None;
    };
    let (0x30, alg, _) = element(der, tbs_end)? else {
        return None;
    };
    let (0x06, oid, oid_end) = element(der, alg)? else {
        return None;
    };
    der.get(oid..oid_end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_channel_binding_hashes_with_the_signature_hash() {
        let path = format!(
            "{}/../../../tests/tls/server.pem",
            env!("CARGO_MANIFEST_DIR")
        );
        let pem = std::fs::read(path).unwrap();
        let cert = dbine_tls::pem_certs(&pem, "server.pem").unwrap().remove(0);
        // The test certificates are sha256WithRSAEncryption.
        assert_eq!(
            signature_oid(&cert),
            Some(&[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x0b][..])
        );
        assert_eq!(
            server_end_point(&cert),
            Some(sha2::Sha256::digest(cert.as_ref()).to_vec())
        );
        assert_eq!(server_end_point(b"\x30\x03\x02\x01\x00"), None);
    }
}
