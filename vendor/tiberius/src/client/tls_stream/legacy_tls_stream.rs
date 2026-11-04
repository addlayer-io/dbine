//! PATCH(dbine): legacy TLS (1.0/1.1) through OpenSSL, chosen per connection
//! with [`Config::legacy_tls`]. Every other connection keeps the rustls
//! stream exactly as before; this backend only exists in builds with the
//! `legacy-tls` feature (DBine's on-demand `sqlserver_legacy` driver host).
//!
//! What changes on this path, and only here:
//! - the protocol floor is TLS 1.0 (OpenSSL still negotiates the highest
//!   version both sides speak);
//! - the cipher list is OpenSSL's whole list at security level 0, so the
//!   SHA-1 / RSA key exchange suites old SChannel servers offer are allowed.
//!
//! Certificate checks are the same as rustls': the OS trust store plus the
//! configured CAs, with the host name verified, unless the connection
//! explicitly trusts the server certificate (`trust_cert`).
use super::{certs, rustls_tls_stream};
use crate::{
    client::{config::Config, config::RootSource, TrustConfig},
    EncryptionLevel, Error,
};
use futures_util::io::{AsyncRead, AsyncWrite};
use openssl::{
    ssl::{Ssl, SslConnector, SslConnectorBuilder, SslMethod, SslVerifyMode, SslVersion},
    x509::X509,
};
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
use tracing::{event, Level};

/// The cipher list on the legacy path: everything OpenSSL has except the
/// unauthenticated and unencrypted suites, at security level 0 (OpenSSL 3
/// refuses TLS 1.0/1.1 and SHA-1 signatures at its default level 1+).
pub(crate) const LEGACY_CIPHERS: &str = "ALL:!aNULL:!eNULL:@SECLEVEL=0";

/// The TLS stream of a build with the legacy backend: rustls, or OpenSSL
/// when the connection asked for legacy TLS.
pub(crate) enum TlsStream<S: AsyncRead + AsyncWrite + Unpin + Send> {
    Modern(rustls_tls_stream::TlsStream<S>),
    Legacy(Compat<tokio_openssl::SslStream<Compat<S>>>),
}

fn tls_error(e: impl std::fmt::Display) -> Error {
    Error::Tls(e.to_string())
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> TlsStream<S> {
    pub(super) async fn modern(config: &Config, stream: S) -> crate::Result<Self> {
        Ok(TlsStream::Modern(
            rustls_tls_stream::TlsStream::new(config, stream).await?,
        ))
    }

    pub(super) async fn legacy(config: &Config, stream: S) -> crate::Result<Self> {
        event!(
            Level::WARN,
            "Performing a legacy TLS handshake (TLS 1.0+, security level 0)"
        );
        let ssl = legacy_ssl(config)?;
        let mut tls = tokio_openssl::SslStream::new(ssl, stream.compat()).map_err(tls_error)?;
        Pin::new(&mut tls)
            .connect()
            .await
            .map_err(|e| Error::Tls(format!("legacy TLS handshake failed: {e}")))?;
        if let Some(v) = tls.ssl().version2() {
            event!(Level::DEBUG, "legacy TLS negotiated {:?}", v);
        }
        Ok(TlsStream::Legacy(tls.compat()))
    }

    pub(crate) fn get_mut(&mut self) -> &mut S {
        match self {
            TlsStream::Modern(s) => s.get_mut(),
            TlsStream::Legacy(s) => s.get_mut().get_mut().get_mut(),
        }
    }
}

/// The OpenSSL session of a legacy connection, set up for its host.
fn legacy_ssl(config: &Config) -> crate::Result<Ssl> {
    // Neither applies to servers that only speak TLS 1.0/1.1: TDS 8.0 needs
    // SQL Server 2022, and the client certificate is a TDS 8.0 feature.
    if matches!(config.encryption, EncryptionLevel::Strict) {
        return Err(Error::Tls(
            "legacy TLS can't be used with strict (TDS 8.0) encryption".into(),
        ));
    }
    if config.get_client_certificate().is_some() {
        return Err(Error::Tls(
            "legacy TLS doesn't present client certificates".into(),
        ));
    }
    let bypass = config.trust.bypass;
    let builder = legacy_builder(&config.trust)?;
    let mut ssl = builder.build().configure().map_err(tls_error)?;
    if bypass {
        ssl.set_verify_hostname(false);
    }
    ssl.into_ssl(&config.get_hostname_in_certificate())
        .map_err(tls_error)
}

/// The OpenSSL context of a legacy connection: TLS 1.0 floor, the old
/// cipher suites, and the same trust as the rustls path.
fn legacy_builder(trust: &TrustConfig) -> crate::Result<SslConnectorBuilder> {
    let mut builder = SslConnector::builder(SslMethod::tls_client()).map_err(tls_error)?;
    builder
        .set_min_proto_version(Some(SslVersion::TLS1))
        .map_err(tls_error)?;
    builder.set_cipher_list(LEGACY_CIPHERS).map_err(tls_error)?;
    if trust.bypass {
        event!(
            Level::WARN,
            "Trusting the server certificate without validation."
        );
        builder.set_verify(SslVerifyMode::NONE);
    } else {
        for cert in trust_anchors(trust)? {
            // A certificate twice (the OS store and an extra CA) is fine.
            let _ = builder.cert_store_mut().add_cert(cert);
        }
        builder.set_verify(SslVerifyMode::PEER);
    }
    Ok(builder)
}

/// The certificates a validating connection trusts: the OS store's plus
/// the configured CAs, failing closed as the rustls path does when there
/// are none.
fn trust_anchors(trust: &TrustConfig) -> crate::Result<Vec<X509>> {
    let mut out = Vec::new();
    let (base_added, base_had_errors) = match &trust.source {
        RootSource::Native => {
            let native = rustls_native_certs::load_native_certs();
            let errors = !native.errors.is_empty();
            let before = out.len();
            for cert in native.certs {
                match X509::from_der(cert.as_ref()) {
                    Ok(c) => out.push(c),
                    Err(err) => event!(
                        Level::DEBUG,
                        "skipping invalid platform certificate: {:?}",
                        err
                    ),
                }
            }
            (out.len() - before, errors)
        }
        #[cfg(feature = "rustls-webpki-roots")]
        RootSource::WebpkiRoots => {
            return Err(Error::Tls(
                "legacy TLS can't use the bundled webpki roots: use the OS store or a CA file"
                    .into(),
            ))
        }
    };
    for extra in &trust.extra_cas {
        for cert in certs::trust_anchors(extra)? {
            out.push(
                X509::from_der(cert.as_ref()).map_err(|e| certs::invalid_cert_error(extra, e))?,
            );
        }
    }
    rustls_tls_stream::ensure_base_or_extra(
        base_added,
        base_had_errors,
        !trust.extra_cas.is_empty(),
    )?;
    Ok(out)
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> AsyncRead for TlsStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            TlsStream::Modern(s) => Pin::new(s).poll_read(cx, buf),
            TlsStream::Legacy(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin + Send> AsyncWrite for TlsStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            TlsStream::Modern(s) => Pin::new(s).poll_write(cx, buf),
            TlsStream::Legacy(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            TlsStream::Modern(s) => Pin::new(s).poll_flush(cx),
            TlsStream::Legacy(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            TlsStream::Modern(s) => Pin::new(s).poll_close(cx),
            TlsStream::Legacy(s) => Pin::new(s).poll_close(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        let mut c = Config::new();
        c.host("legacy.example.com");
        c.legacy_tls(true);
        c
    }

    #[test]
    fn legacy_path_allows_tls10_at_security_level_0() {
        let ssl = legacy_ssl(&config()).unwrap();
        assert_eq!(ssl.security_level(), 0);
        let mut b = legacy_builder(&TrustConfig::default()).unwrap();
        assert_eq!(b.min_proto_version(), Some(SslVersion::TLS1));
        // The suites old SChannel servers offer are built in and allowed.
        b.set_cipher_list("AES128-SHA:AES256-SHA:@SECLEVEL=0")
            .unwrap();
    }

    #[test]
    fn legacy_path_verifies_by_default() {
        let ssl = legacy_ssl(&config()).unwrap();
        assert_eq!(ssl.ssl_context().verify_mode(), SslVerifyMode::PEER);
    }

    #[test]
    fn legacy_path_skips_verification_only_when_trusted() {
        let mut c = config();
        c.trust_cert();
        let ssl = legacy_ssl(&c).unwrap();
        assert_eq!(ssl.ssl_context().verify_mode(), SslVerifyMode::NONE);
    }

    #[test]
    fn legacy_path_refuses_strict() {
        let mut c = config();
        c.encryption(EncryptionLevel::Strict);
        assert!(legacy_ssl(&c).is_err());
    }
}
