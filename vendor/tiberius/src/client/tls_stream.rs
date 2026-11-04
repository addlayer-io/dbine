use crate::Config;
use futures_util::io::{AsyncRead, AsyncWrite};

/// ALPN protocol name advertised for TDS 8.0 ("strict") encryption.
#[cfg(any(
    feature = "rustls",
    feature = "native-tls",
    feature = "vendored-openssl"
))]
// Used by the native-tls and rustls backends to advertise TDS 8.0 strict; the
// opentls (vendored-openssl) backend cannot set ALPN, so this is unused there.
#[allow(dead_code)]
pub(crate) const TDS_ALPN_PROTOCOL_NAME: &str = "tds/8.0";

// Backend-agnostic CA loader shared by all three TLS backends.
#[cfg(any(
    feature = "rustls",
    feature = "native-tls",
    feature = "vendored-openssl"
))]
mod certs;

#[cfg(feature = "native-tls")]
mod native_tls_stream;

#[cfg(feature = "rustls")]
mod rustls_tls_stream;

#[cfg(feature = "vendored-openssl")]
mod opentls_tls_stream;

#[cfg(feature = "native-tls")]
pub(crate) use native_tls_stream::TlsStream;

#[cfg(all(feature = "rustls", not(feature = "legacy-tls")))]
pub(crate) use rustls_tls_stream::TlsStream;

// PATCH(dbine): legacy TLS (1.0/1.1) through OpenSSL, per connection. With
// the feature the stream is rustls or OpenSSL; without it, rustls alone, as
// upstream.
#[cfg(all(feature = "rustls", feature = "legacy-tls"))]
mod legacy_tls_stream;

#[cfg(all(feature = "rustls", feature = "legacy-tls"))]
pub(crate) use legacy_tls_stream::TlsStream;

#[cfg(feature = "vendored-openssl")]
pub(crate) use opentls_tls_stream::TlsStream;

#[cfg(feature = "rustls")]
pub(crate) async fn create_tls_stream<S: AsyncRead + AsyncWrite + Unpin + Send>(
    config: &Config,
    stream: S,
) -> crate::Result<TlsStream<S>> {
    // PATCH(dbine): legacy TLS only when the connection asked for it.
    #[cfg(feature = "legacy-tls")]
    {
        if config.legacy_tls {
            TlsStream::legacy(config, stream).await
        } else {
            TlsStream::modern(config, stream).await
        }
    }
    #[cfg(not(feature = "legacy-tls"))]
    {
        if config.legacy_tls {
            return Err(crate::Error::Tls(
                "legacy TLS (1.0/1.1) isn't built into this client".into(),
            ));
        }
        TlsStream::new(config, stream).await
    }
}

#[cfg(feature = "native-tls")]
pub(crate) async fn create_tls_stream<S: AsyncRead + AsyncWrite + Unpin + Send>(
    config: &Config,
    stream: S,
) -> crate::Result<TlsStream<S>> {
    native_tls_stream::create_tls_stream(config, stream).await
}

#[cfg(feature = "vendored-openssl")]
pub(crate) async fn create_tls_stream<S: AsyncRead + AsyncWrite + Unpin + Send>(
    config: &Config,
    stream: S,
) -> crate::Result<TlsStream<S>> {
    opentls_tls_stream::create_tls_stream(config, stream).await
}
