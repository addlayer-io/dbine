//! The two certificate checks the drivers' clients don't have, as rustls
//! verifiers:
//!
//! - [`CaOnlyVerifier`] (`TlsLevel::VerifyCa`: `sslmode=verify-ca`…): the
//!   whole chain against the configured roots, without the host name. Only
//!   the name mismatch is forgiven; an unknown CA, an expired certificate or
//!   a bad signature fail as in the full check.
//! - [`PinVerifier`] (`TlsLevel::Unverified`: `sslmode=require`, `insecure`,
//!   `TrustServerCertificate`…): no chain, but the server's certificate must
//!   be the one pinned on the first connection (trust on first use). With no
//!   pin yet the first one is accepted and its fingerprint kept in a
//!   [`PinWatch`] for the app to save (`Session::tls_pin_seen`); from then
//!   on it's the pin for every handshake on that watch. Another certificate
//!   fails with `ConnectFailureKind::TlsPinChanged`.
//!
//! Both verify the handshake's signatures with the server's key, so the
//! server has to hold the certificate's private key. The crypto is rustls'
//! `ring` provider, named explicitly (reqwest brings aws-lc-rs too, and two
//! providers in the build leave no process default).

use dbine_driver::{ConnectFailure, ConnectFailureKind, ConnectionConfig, Error, TLS_PIN};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

pub use rustls;

/// The crypto provider every config here uses.
pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A certificate's fingerprint: the SHA-256 of its DER, in lowercase hex.
pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A fingerprint as people read it: `AB:CD:…`.
pub fn display_fingerprint(hex: &str) -> String {
    let up = hex.to_ascii_uppercase();
    let b = up.as_bytes();
    if b.len() != 64 || !b.iter().all(u8::is_ascii_hexdigit) {
        return hex.to_string();
    }
    b.chunks(2)
        .map(|c| std::str::from_utf8(c).unwrap_or_default())
        .collect::<Vec<_>>()
        .join(":")
}

/// A pin as typed or saved (hex, with or without `:` or spaces, any case),
/// normalized to [`fingerprint`]'s form; `None` if it isn't a SHA-256.
pub fn normalize_pin(s: &str) -> Option<String> {
    let hex: String = s
        .chars()
        .filter(|c| !matches!(c, ':' | ' ' | '-'))
        .map(|c| c.to_ascii_lowercase())
        .collect();
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hex)
}

/// The connection's pin ([`TLS_PIN`]), if it has one. A value that isn't a
/// SHA-256 stays as it is: it matches no certificate, so the connection
/// fails (and the app offers to trust the new one) instead of trusting any.
pub fn pin_of(cfg: &ConnectionConfig) -> Option<String> {
    let v = cfg.option(TLS_PIN)?.trim();
    if v.is_empty() {
        return None;
    }
    Some(normalize_pin(v).unwrap_or_else(|| v.to_string()))
}

/// The error of a server whose certificate isn't the pinned one.
/// The presented fingerprint also goes as data (`presented_pin`): trusting
/// the new certificate pins that one, not whatever a later handshake shows.
pub fn pin_changed(expected: &str, got: &str) -> Error {
    let presented = normalize_pin(got);
    let (expected, got) = (display_fingerprint(expected), display_fingerprint(got));
    let mut f = ConnectFailure::new(
        ConnectFailureKind::TlsPinChanged,
        format!(
            "El certificado del servidor cambió desde la conexión en la que se lo aceptó: se esperaba la huella SHA-256 {expected} y el servidor presentó {got}. Si el servidor renovó su certificado, confiá en el certificado nuevo; si no, alguien podría estar interceptando la conexión."
        ),
    );
    f.presented_pin = presented;
    Error::connect_failed(f)
}

/// What a [`PinVerifier`] saw, shared with the session that owns the
/// connection.
#[derive(Debug, Clone, Default)]
pub struct PinWatch(Arc<Mutex<Seen>>);

#[derive(Debug, Default)]
struct Seen {
    /// The certificate accepted with no pin (first use).
    first: Option<String>,
    /// The last certificate that didn't match the pin: `(pin, presented)`.
    changed: Option<(String, String)>,
}

impl PinWatch {
    /// The fingerprint accepted on first use, for the app to save.
    pub fn seen(&self) -> Option<String> {
        self.0.lock().ok()?.first.clone()
    }

    /// `(pinned, presented)` when a handshake failed on the pin.
    pub fn changed(&self) -> Option<(String, String)> {
        self.0.lock().ok()?.changed.clone()
    }

    /// A connect error, replaced by [`pin_changed`] when the handshake
    /// failed on the pin (the client's own error only says "invalid peer
    /// certificate").
    pub fn explain(&self, e: Error) -> Error {
        match self.changed() {
            Some((pin, got)) => pin_changed(&pin, &got),
            None => e,
        }
    }
}

/// `TlsLevel::Unverified` as certificate pinning: see the crate's docs.
#[derive(Debug)]
pub struct PinVerifier {
    pin: Option<String>,
    watch: PinWatch,
    provider: Arc<CryptoProvider>,
}

impl PinVerifier {
    /// The verifier for `pin` (`None`: first use) and what it will see.
    pub fn new(pin: Option<String>) -> (Arc<Self>, PinWatch) {
        let watch = PinWatch::default();
        (Self::watched(pin, watch.clone()), watch)
    }

    /// A verifier that reports to an existing `watch` (a client that makes
    /// a new config for each reconnect keeps one watch for the session).
    pub fn watched(pin: Option<String>, watch: PinWatch) -> Arc<Self> {
        Arc::new(Self {
            pin,
            watch,
            provider: provider(),
        })
    }
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got = fingerprint(end_entity);
        let Ok(mut seen) = self.watch.0.lock() else {
            return Err(rustls::Error::General("tls pin state poisoned".into()));
        };
        // With no pin, the first certificate accepted becomes the pin for
        // every later handshake that reports to the same watch (the
        // session's reconnects, extra connections, other nodes).
        let pin = match (&self.pin, &seen.first) {
            (Some(pin), _) => pin.clone(),
            (None, Some(first)) => first.clone(),
            (None, None) => {
                seen.first = Some(got);
                return Ok(ServerCertVerified::assertion());
            }
        };
        if pin == got {
            Ok(ServerCertVerified::assertion())
        } else {
            seen.changed = Some((pin, got));
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// `TlsLevel::VerifyCa`: rustls' own check, with the name mismatch
/// forgiven (it's checked last, after the chain, so nothing else is).
#[derive(Debug)]
pub struct CaOnlyVerifier {
    inner: Arc<WebPkiServerVerifier>,
}

impl CaOnlyVerifier {
    pub fn new(roots: RootCertStore) -> Result<Arc<Self>, Error> {
        let inner = WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider())
            .build()
            .map_err(|e| Error::Connect(format!("TLS: {e}")))?;
        Ok(Arc::new(Self { inner }))
    }
}

impl ServerCertVerifier for CaOnlyVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            r => r,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// The web PKI's roots plus the system's (what the drivers trusted when no
/// CA file is given).
pub fn system_roots() -> RootCertStore {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    roots
}

/// The certificates of a PEM bundle (`what` names it in the errors: the
/// option and the path).
pub fn pem_certs(pem: &[u8], what: &str) -> Result<Vec<CertificateDer<'static>>, Error> {
    let certs = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| Error::Connect(format!("{what} no es un PEM válido: {e}")))?;
    if certs.is_empty() {
        return Err(Error::Connect(format!("{what} no tiene certificados PEM.")));
    }
    Ok(certs)
}

/// The certificates of a CA file.
pub fn read_ca_file(path: &str, what: &str) -> Result<Vec<CertificateDer<'static>>, Error> {
    let pem = std::fs::read(path)
        .map_err(|e| Error::Connect(format!("No se pudo leer {what} ({path}): {e}")))?;
    pem_certs(&pem, &format!("{what} ({path})"))
}

/// `certs` added to `roots`.
pub fn add_roots(
    roots: &mut RootCertStore,
    certs: Vec<CertificateDer<'static>>,
    what: &str,
) -> Result<(), Error> {
    for c in certs {
        roots
            .add(c)
            .map_err(|e| Error::Connect(format!("certificado inválido en {what}: {e}")))?;
    }
    Ok(())
}

fn builder() -> Result<rustls::ConfigBuilder<ClientConfig, rustls::WantsVerifier>, Error> {
    ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Connect(format!("TLS: {e}")))
}

/// A client config that pins: see [`PinVerifier`].
pub fn pinned_config(pin: Option<String>) -> Result<(ClientConfig, PinWatch), Error> {
    let (v, watch) = PinVerifier::new(pin);
    let c = builder()?
        .dangerous()
        .with_custom_certificate_verifier(v)
        .with_no_client_auth();
    Ok((c, watch))
}

/// A client config that checks the chain against `roots` and not the name.
pub fn ca_only_config(roots: RootCertStore) -> Result<ClientConfig, Error> {
    Ok(builder()?
        .dangerous()
        .with_custom_certificate_verifier(CaOnlyVerifier::new(roots)?)
        .with_no_client_auth())
}

/// A client config with rustls' full check against `roots`.
pub fn verified_config(roots: RootCertStore) -> Result<ClientConfig, Error> {
    Ok(builder()?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pem(name: &str) -> Vec<CertificateDer<'static>> {
        let path = format!("{}/../../tests/tls/{name}", env!("CARGO_MANIFEST_DIR"));
        read_ca_file(&path, name).unwrap()
    }

    fn ca_roots() -> RootCertStore {
        let mut r = RootCertStore::empty();
        add_roots(&mut r, pem("ca.pem"), "ca.pem").unwrap();
        r
    }

    fn name(n: &str) -> ServerName<'static> {
        ServerName::try_from(n.to_string()).unwrap()
    }

    fn check(v: &dyn ServerCertVerifier, cert: &str, host: &str) -> Result<(), rustls::Error> {
        let Some(c) = pem(cert).into_iter().next() else {
            panic!("the test certificate has no PEM block");
        };
        v.verify_server_cert(&c, &[], &name(host), &[], UnixTime::now())
            .map(|_| ())
    }

    #[test]
    fn verify_ca_checks_the_chain_but_not_the_name() {
        let v = CaOnlyVerifier::new(ca_roots()).unwrap();
        assert!(check(v.as_ref(), "server.pem", "localhost").is_ok());
        // The full check refuses this name; verify-ca doesn't.
        let full = WebPkiServerVerifier::builder_with_provider(Arc::new(ca_roots()), provider())
            .build()
            .unwrap();
        assert!(check(full.as_ref(), "server.pem", "wrong.example").is_err());
        assert!(check(v.as_ref(), "server.pem", "wrong.example").is_ok());
        // A certificate the CA didn't sign fails, whatever the name.
        let e = check(v.as_ref(), "selfsigned.pem", "localhost").unwrap_err();
        assert!(matches!(e, rustls::Error::InvalidCertificate(_)), "{e:?}");
        // Unknown CA: the system's roots don't have the test CA.
        let sys = CaOnlyVerifier::new(system_roots()).unwrap();
        assert!(check(sys.as_ref(), "server.pem", "wrong.example").is_err());
    }

    #[test]
    fn pinning_accepts_the_first_and_then_only_that_certificate() {
        let server = fingerprint(&pem("server.pem")[0]);
        let other = fingerprint(&pem("selfsigned.pem")[0]);
        assert_ne!(server, other);
        // First use: anything, and it's reported.
        let (v, watch) = PinVerifier::new(None);
        assert!(check(v.as_ref(), "server.pem", "anything.example").is_ok());
        assert_eq!(watch.seen().as_deref(), Some(server.as_str()));
        assert!(watch.changed().is_none());
        // The same certificate passes, nothing new to save.
        let (v, watch) = PinVerifier::new(Some(server.clone()));
        assert!(check(v.as_ref(), "server.pem", "localhost").is_ok());
        assert!(watch.seen().is_none() && watch.changed().is_none());
        // Another one fails, and the error names both.
        assert!(check(v.as_ref(), "selfsigned.pem", "localhost").is_err());
        assert_eq!(watch.changed(), Some((server.clone(), other.clone())));
        let e = watch.explain(Error::Connect("invalid peer certificate".into()));
        let Error::ConnectFailed(f) = &e else {
            panic!("{e}");
        };
        assert_eq!(f.kind, ConnectFailureKind::TlsPinChanged);
        assert!(f.kind.is_tls());
        assert!(f.message.contains(&display_fingerprint(&server)));
        assert!(f.message.contains(&display_fingerprint(&other)));
        assert_eq!(f.presented_pin.as_deref(), Some(other.as_str()));
    }

    #[test]
    fn first_use_locks_onto_the_first_certificate() {
        let server = fingerprint(&pem("server.pem")[0]);
        let other = fingerprint(&pem("selfsigned.pem")[0]);
        let (v, watch) = PinVerifier::new(None);
        assert!(check(v.as_ref(), "server.pem", "localhost").is_ok());
        // The same verifier (a reconnect of the session) takes only that one.
        assert!(check(v.as_ref(), "server.pem", "localhost").is_ok());
        assert!(check(v.as_ref(), "selfsigned.pem", "localhost").is_err());
        assert_eq!(watch.seen().as_deref(), Some(server.as_str()));
        assert_eq!(watch.changed(), Some((server.clone(), other.clone())));
        // So does another verifier on the same watch (a new config per
        // reconnect, another node of the cluster).
        let again = PinVerifier::watched(None, watch.clone());
        assert!(check(again.as_ref(), "selfsigned.pem", "localhost").is_err());
        assert!(check(again.as_ref(), "server.pem", "localhost").is_ok());
        assert_eq!(watch.seen().as_deref(), Some(server.as_str()));
        let e = watch.explain(Error::Connect("invalid peer certificate".into()));
        let Error::ConnectFailed(f) = &e else {
            panic!("{e}");
        };
        assert_eq!(f.kind, ConnectFailureKind::TlsPinChanged);
        assert_eq!(f.presented_pin.as_deref(), Some(other.as_str()));
    }

    #[test]
    fn pins_are_read_in_any_spelling_and_garbage_matches_nothing() {
        let hex = "ab".repeat(32);
        assert_eq!(normalize_pin(&display_fingerprint(&hex)), Some(hex.clone()));
        assert_eq!(normalize_pin(&hex.to_uppercase()), Some(hex.clone()));
        assert_eq!(normalize_pin("abc"), None);
        let cfg = |v: &str| ConnectionConfig {
            options: [(TLS_PIN.to_string(), v.to_string())].into_iter().collect(),
            ..Default::default()
        };
        assert_eq!(pin_of(&cfg(&hex.to_uppercase())), Some(hex));
        assert_eq!(pin_of(&cfg("")), None);
        assert_eq!(pin_of(&ConnectionConfig::default()), None);
        // Not a SHA-256: kept, so no certificate matches it.
        let (v, _) = PinVerifier::new(pin_of(&cfg("not-a-hash")));
        assert!(check(v.as_ref(), "server.pem", "localhost").is_err());
    }

    #[test]
    fn configs_build() {
        assert!(pinned_config(None).is_ok());
        assert!(ca_only_config(ca_roots()).is_ok());
        assert!(verified_config(ca_roots()).is_ok());
    }
}
