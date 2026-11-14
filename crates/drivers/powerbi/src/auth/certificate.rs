//! A service principal's certificate credential (Microsoft Learn,
//! "Microsoft identity platform application authentication certificate
//! credentials"): instead of the secret, the token request carries a JWT
//! client assertion signed with the certificate's private key.
//!
//! - Header: `alg` `PS256` (RSASSA-PSS with SHA-256 and a 32-byte salt),
//!   `typ` `JWT` and `x5t#S256`, the base64url SHA-256 thumbprint of the
//!   certificate's DER. [`Alg::Rs256`] is the older form (PKCS #1 v1.5 with
//!   the SHA-1 `x5t`).
//! - Claims: `aud` the token endpoint the request goes to, `iss` and `sub`
//!   the client id, a new `jti` each time, `nbf` and `iat` now, `exp` ten
//!   minutes later.
//!
//! The file is a PEM with the certificate and its private key (PKCS #8,
//! plain or encrypted with PBES2, or PKCS #1), or a PFX / PKCS #12 with its
//! password. It's read at each sign-in, so a certificate renewed on disk is
//! picked up, and the key isn't kept between sign-ins. The buffers this
//! module reads the file and the key into are wiped when dropped
//! (`Zeroizing`; `pkcs8`'s decrypted document and AWS-LC's key wipe
//! themselves), but `p12-keystore` frees its decrypted copy of a PFX key
//! without wiping it. The key never shows in `Debug`, an error or a log.
//! RSA keys only: the assertion is signed with RSA (PS256), as Microsoft
//! Entra ID asks.
//!
//! The signature and the thumbprints run in AWS-LC (`aws-lc-rs`, already in
//! the build through rustls); `p12-keystore` reads PFX files and `pkcs8`
//! decrypts PEM keys (the versions the app's secret sources use), PBES2 with
//! AES or 3DES and PBKDF2 with HMAC-SHA-256 or HMAC-SHA-1 (what LibreSSL,
//! macOS's `openssl`, and OpenSSL 1.0 write).

use super::auth;
use aws_lc_rs::digest::{digest, SHA1_FOR_LEGACY_USE_ONLY, SHA256};
use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::signature::{KeyPair, RsaEncoding, RsaKeyPair, RSA_PKCS1_SHA256, RSA_PSS_SHA256};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use dbine_driver::{Error, Result};
use serde_json::{json, Value};
use std::io::Read;
use zeroize::Zeroizing;

/// `client_assertion_type` of a JWT client assertion.
pub const ASSERTION_TYPE: &str = "urn:ietf:params:oauth:client-assertion-type:jwt-bearer";

/// How long an assertion is good for (Microsoft Learn: 5 to 10 minutes).
const LIFETIME: u64 = 600;

/// A PEM or a PFX is a few kilobytes: past this the file isn't one.
const MAX_FILE: u64 = 1 << 20;

/// rsaEncryption and RSASSA-PSS, the RSA keys' PKCS #8 algorithms.
const RSA_OIDS: [&str; 2] = ["1.2.840.113549.1.1.1", "1.2.840.113549.1.1.10"];

const NOT_RSA: &str =
    "la clave del certificado no es RSA: DBine firma la aserción con RSA (PS256), como pide Microsoft Entra ID";

/// A PKCS #8 key encrypted with what `pkcs5` can't decrypt: PBES1
/// (`pbeWithMD5AndDES-CBC`, what LibreSSL's `openssl pkcs8 -topk8` writes
/// with no `-v2`), PKCS #12's PBE (`-v1 PBE-SHA1-3DES`) or single DES.
const UNSUPPORTED_ENCRYPTION: &str =
    "la clave privada está cifrada con un algoritmo que DBine no admite (PBES1, DES u otro antiguo): volvé a cifrarla con openssl pkcs8 -topk8 -v2 aes-256-cbc o usá un PFX";

/// The assertion's signature and thumbprint header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alg {
    /// RSASSA-PSS with SHA-256, with `x5t#S256` (what Microsoft Learn asks).
    Ps256,
    /// RSASSA-PKCS1-v1_5 with SHA-256, with the SHA-1 `x5t`.
    Rs256,
}

/// A certificate and its private key, read from the file.
pub struct Certificate {
    /// The DER of the certificate whose public key is the key's.
    der: Vec<u8>,
    key: RsaKeyPair,
}

// The key never shows, not even through `{:?}`.
impl std::fmt::Debug for Certificate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Certificate")
            .field("thumbprint", &self.thumbprint())
            .finish_non_exhaustive()
    }
}

impl Certificate {
    /// Reads the PEM or PFX file at `path`.
    pub fn load(path: &str, password: Option<&str>) -> Result<Certificate> {
        let cant =
            |e: std::io::Error| auth(format!("no se pudo leer el certificado «{path}»: {e}"));
        let file = std::fs::File::open(path).map_err(cant)?;
        // Sized up front, so reading doesn't reallocate and leave unwiped
        // copies of the key behind.
        let size = file.metadata().map(|m| m.len().min(MAX_FILE)).unwrap_or(0);
        let mut data = Zeroizing::new(Vec::with_capacity(size as usize + 1));
        file.take(MAX_FILE + 1)
            .read_to_end(&mut data)
            .map_err(cant)?;
        if data.len() as u64 > MAX_FILE {
            return Err(auth(format!(
                "«{path}» no es un certificado: pesa más de 1 MB"
            )));
        }
        Self::from_bytes(&data, password)
    }

    /// A PEM (text with `-----BEGIN`) or else a PFX / PKCS #12.
    pub fn from_bytes(data: &[u8], password: Option<&str>) -> Result<Certificate> {
        let password = password.filter(|p| !p.is_empty());
        if data.windows(11).any(|w| w == b"-----BEGIN ") {
            let text = Zeroizing::new(String::from_utf8_lossy(data).into_owned());
            from_pem(&text, password)
        } else {
            from_pkcs12(data, password)
        }
    }

    /// `x5t#S256`: base64url of the SHA-256 of the certificate's DER.
    pub fn x5t_s256(&self) -> String {
        URL_SAFE_NO_PAD.encode(digest(&SHA256, &self.der))
    }

    /// `x5t`: base64url of the SHA-1 of the certificate's DER.
    pub fn x5t(&self) -> String {
        URL_SAFE_NO_PAD.encode(digest(&SHA1_FOR_LEGACY_USE_ONLY, &self.der))
    }

    /// The SHA-1 thumbprint in hex, as the app registration shows it.
    pub fn thumbprint(&self) -> String {
        digest(&SHA1_FOR_LEGACY_USE_ONLY, &self.der)
            .as_ref()
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect()
    }

    /// The signed client assertion for `client_id` at the token endpoint
    /// `audience`, made at `now` (Unix seconds).
    pub fn assertion(&self, client_id: &str, audience: &str, alg: Alg, now: u64) -> Result<String> {
        let (header, padding): (Value, &'static dyn RsaEncoding) = match alg {
            Alg::Ps256 => (
                json!({"alg": "PS256", "typ": "JWT", "x5t#S256": self.x5t_s256()}),
                &RSA_PSS_SHA256,
            ),
            Alg::Rs256 => (
                json!({"alg": "RS256", "typ": "JWT", "x5t": self.x5t()}),
                &RSA_PKCS1_SHA256,
            ),
        };
        let claims = json!({
            "aud": audience,
            "iss": client_id,
            "sub": client_id,
            "jti": uuid::Uuid::new_v4().to_string(),
            "nbf": now,
            "iat": now,
            "exp": now + LIFETIME,
        });
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut signature = vec![0; self.key.public_modulus_len()];
        self.key
            .sign(
                padding,
                &SystemRandom::new(),
                signed.as_bytes(),
                &mut signature,
            )
            .map_err(|_| auth("no se pudo firmar la aserción con la clave del certificado"))?;
        Ok(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }
}

/// An RSA key from its PKCS #8 DER, or why it isn't one.
fn rsa_pkcs8(der: &[u8]) -> Result<RsaKeyPair> {
    RsaKeyPair::from_pkcs8(der).map_err(|e| match pkcs8::PrivateKeyInfoRef::try_from(der) {
        Ok(info) if !RSA_OIDS.contains(&info.algorithm.oid.to_string().as_str()) => auth(NOT_RSA),
        _ => auth(format!(
            "la clave privada del certificado no es válida ({e})"
        )),
    })
}

/// The certificate whose public key is the key's (a file may carry the
/// chain): its DER holds the key's `RSAPublicKey` as is.
fn matching(key: RsaKeyPair, certs: Vec<Vec<u8>>) -> Result<Certificate> {
    if certs.is_empty() {
        return Err(auth(
            "el archivo no tiene el certificado, solo la clave privada",
        ));
    }
    let public = key.public_key().as_ref().to_vec();
    let der = certs
        .into_iter()
        .find(|c| c.windows(public.len()).any(|w| w == public.as_slice()))
        .ok_or_else(|| auth("ningún certificado del archivo corresponde a su clave privada"))?;
    Ok(Certificate { der, key })
}

struct Block {
    label: String,
    /// RFC 1421 headers (`Proc-Type: 4,ENCRYPTED`): OpenSSL's traditional
    /// encryption.
    traditional: bool,
    der: Zeroizing<Vec<u8>>,
}

fn pem_blocks(text: &str) -> Result<Vec<Block>> {
    const BEGIN: &str = "-----BEGIN ";
    let incomplete = || auth("el archivo PEM está incompleto o dañado");
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(BEGIN) {
        let after = &rest[start + BEGIN.len()..];
        let close = after.find("-----").ok_or_else(incomplete)?;
        let label = after[..close].trim().to_string();
        let body = &after[close + 5..];
        let end = format!("-----END {label}-----");
        let stop = body.find(&end).ok_or_else(incomplete)?;
        let lines = body[..stop].lines().map(str::trim);
        let traditional = lines.clone().any(|l| l.contains(':'));
        let der = Zeroizing::new(if traditional {
            Vec::new()
        } else {
            // Sized up front: growing would leave unwiped copies of a key.
            let mut b64 = Zeroizing::new(String::with_capacity(stop));
            lines.for_each(|l| b64.push_str(l));
            STANDARD.decode(b64.as_bytes()).map_err(|_| incomplete())?
        });
        out.push(Block {
            label,
            traditional,
            der,
        });
        rest = &body[stop + end.len()..];
    }
    Ok(out)
}

fn from_pem(text: &str, password: Option<&str>) -> Result<Certificate> {
    let blocks = pem_blocks(text)?;
    let certs: Vec<Vec<u8>> = blocks
        .iter()
        .filter(|b| b.label == "CERTIFICATE")
        .map(|b| b.der.to_vec())
        .collect();
    let block = blocks
        .iter()
        .find(|b| b.label.ends_with("PRIVATE KEY"))
        .ok_or_else(|| {
            auth("el archivo PEM no tiene la clave privada: hace falta el certificado y su clave en el mismo archivo, o un PFX")
        })?;
    if block.traditional {
        return Err(auth(
            "la clave privada está cifrada con el formato tradicional de OpenSSL: convertila a PKCS#8 (openssl pkcs8 -topk8 -v2 aes-256-cbc) o usá un PFX",
        ));
    }
    let key = match block.label.as_str() {
        "PRIVATE KEY" => rsa_pkcs8(&block.der)?,
        "RSA PRIVATE KEY" => RsaKeyPair::from_der(&block.der).map_err(|e| {
            auth(format!(
                "la clave privada del certificado no es válida ({e})"
            ))
        })?,
        "ENCRYPTED PRIVATE KEY" => {
            let password = password.ok_or_else(|| {
                auth("la clave privada está cifrada: escribí la contraseña del certificado")
            })?;
            let info =
                pkcs8::EncryptedPrivateKeyInfoRef::try_from(block.der.as_slice()).map_err(|e| {
                    use pkcs8::der::{ErrorKind, Tag};
                    match e {
                        // An algorithm `pkcs5` doesn't know, such as PKCS
                        // #12's PBE: it refuses the OID's value.
                        pkcs8::Error::Asn1(e)
                            if matches!(
                                e.kind(),
                                ErrorKind::OidUnknown { .. }
                                    | ErrorKind::Value {
                                        tag: Tag::ObjectIdentifier
                                    }
                            ) =>
                        {
                            auth(UNSUPPORTED_ENCRYPTION)
                        }
                        _ => auth("la clave privada cifrada no es válida"),
                    }
                })?;
            let plain = info.decrypt(password).map_err(|e| {
                use pkcs8::pkcs5::Error as Pkcs5;
                match e {
                    pkcs8::Error::EncryptedPrivateKey(
                        Pkcs5::UnsupportedAlgorithm { .. } | Pkcs5::NoPbes1CryptSupport,
                    ) => auth(UNSUPPORTED_ENCRYPTION),
                    // The padding or the decrypted DER don't check out.
                    _ => auth(
                        "no se pudo descifrar la clave privada: la contraseña no es la correcta",
                    ),
                }
            })?;
            rsa_pkcs8(plain.as_bytes())?
        }
        "EC PRIVATE KEY" => return Err(auth(NOT_RSA)),
        other => {
            return Err(auth(format!(
                "formato de clave privada desconocido ({other})"
            )))
        }
    };
    matching(key, certs)
}

fn from_pkcs12(data: &[u8], password: Option<&str>) -> Result<Certificate> {
    use p12_keystore::{KeyStore, KeyStoreEntry, Pkcs12ImportPolicy};
    let store = KeyStore::from_pkcs12(data, password.unwrap_or(""), Pkcs12ImportPolicy::Relaxed)
        .map_err(|_| {
            auth(if password.is_some() {
                "no se pudo abrir el certificado: la contraseña no es la correcta, o el archivo no es un PEM ni un PFX"
            } else {
                "no se pudo abrir el certificado: si es un PFX con contraseña, escribila; si no, el archivo no es un PEM ni un PFX"
            })
        })?;
    let (_, chain) = store
        .private_key_chain()
        .ok_or_else(|| auth("el PFX no tiene la clave privada del certificado"))?;
    let key = rsa_pkcs8(chain.key().as_der())?;
    let mut certs: Vec<Vec<u8>> = chain.certs().iter().map(|c| c.as_der().to_vec()).collect();
    certs.extend(store.entries().filter_map(|(_, e)| match e {
        KeyStoreEntry::Certificate(c) => Some(c.as_der().to_vec()),
        _ => None,
    }));
    matching(key, certs)
}

/// Entra's refusals of a certificate said plainly: "this certificate isn't
/// one of the app's" (AADSTS700027) names the `thumbprint` to compare, and
/// an assertion out of its time window (AADSTS700024) points to the clock.
/// The thumbprint goes first: the default chain cuts each step's error to
/// its first 200 characters.
pub(crate) fn explain_refusal(e: Error, thumbprint: &str) -> Error {
    match e {
        Error::AuthFailed(m) if m.contains("AADSTS700027") => auth(format!(
            "Microsoft Entra ID no acepta el certificado con huella {thumbprint} para la aplicación (AADSTS700027): subí el certificado (sin la clave) en Certificados y secretos del registro de la aplicación y revisá que no haya vencido."
        )),
        Error::AuthFailed(m) if m.contains("AADSTS700024") => auth(
            "Microsoft Entra ID rechazó la aserción por la hora (AADSTS700024): revisá que el reloj de esta computadora esté en hora.",
        ),
        e => e,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! The fixtures in `tests/fixtures/certificate/` are a self-signed,
    //! test-only certificate and key (never trusted anywhere), made with
    //! OpenSSL 3 or later (the `/usr/bin/openssl` lines with macOS's
    //! LibreSSL 3.3.6):
    //!
    //! ```text
    //! openssl req -x509 -newkey rsa:2048 -nodes -keyout key.pem -out cert.pem -days 36500 -sha256 \
    //!   -subj "/CN=DBine test service principal (test only, never trusted)"
    //! cat cert.pem key.pem > sp.pem
    //! openssl rsa -in key.pem -traditional -out key1.pem && cat cert.pem key1.pem > sp_pkcs1.pem
    //! openssl pkcs8 -topk8 -in key.pem -v2 aes-256-cbc -v2prf hmacWithSHA256 -passout pass:dbine-test -out enc.pem
    //! cat cert.pem enc.pem > sp_encrypted.pem
    //! # The PRF LibreSSL (macOS's `openssl`) and OpenSSL 1.0 use with `-v2`:
    //! openssl pkcs8 -topk8 -in key.pem -v2 aes-256-cbc -v2prf hmacWithSHA1 -passout pass:dbine-test -out enc1.pem
    //! cat cert.pem enc1.pem > sp_encrypted_sha1.pem
    //! /usr/bin/openssl pkcs8 -topk8 -in key.pem -v2 aes-256-cbc -passout pass:dbine-test -out encl.pem
    //! cat cert.pem encl.pem > sp_encrypted_libressl.pem
    //! openssl pkcs8 -topk8 -in key.pem -v2 des3 -passout pass:dbine-test -out enc3.pem
    //! cat cert.pem enc3.pem > sp_encrypted_3des.pem
    //! # Not supported: PBES1 (macOS's LibreSSL 3.3.6 with no -v2) and PKCS #12's PBE.
    //! /usr/bin/openssl pkcs8 -topk8 -in key.pem -passout pass:dbine-test -out pbes1.pem
    //! cat cert.pem pbes1.pem > sp_encrypted_pbes1.pem
    //! openssl pkcs8 -topk8 -in key.pem -v1 PBE-SHA1-3DES -passout pass:dbine-test -out p12pbe.pem
    //! cat cert.pem p12pbe.pem > sp_encrypted_pkcs12_pbe.pem
    //! openssl pkcs12 -export -inkey key.pem -in cert.pem -passout pass:dbine-test -out sp.pfx
    //! openssl pkcs12 -export -legacy -inkey key.pem -in cert.pem -passout pass:dbine-test -out sp_legacy.pfx
    //! openssl rsa -in key.pem -RSAPublicKey_out -outform DER -out sp_public.der
    //! ```
    //!
    //! The expected thumbprints are OpenSSL's (`openssl dgst -sha256 -binary`
    //! of the DER, base64url; `openssl x509 -fingerprint -sha1`).
    use super::*;
    use aws_lc_rs::signature::{
        UnparsedPublicKey, RSA_PKCS1_2048_8192_SHA256, RSA_PSS_2048_8192_SHA256,
    };

    pub(crate) const DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/certificate");
    pub(crate) const PASSWORD: &str = "dbine-test";
    const X5T_S256: &str = "VaSm7tyz8X-YzuC4-Omv_-rdaLJ_CQjI9Ck0rpI9uC0";
    const X5T: &str = "toFOpLcYr0TIbU_CT8WdRdmKiLg";
    const THUMBPRINT: &str = "B6814EA4B718AF44C86D4FC24FC59D45D98A88B8";

    pub(crate) fn fixture(name: &str) -> String {
        format!("{DIR}/{name}")
    }

    fn pem(name: &str) -> String {
        std::fs::read_to_string(fixture(name)).unwrap()
    }

    /// The PEM blocks of a fixture with this label, as text.
    fn blocks_of(text: &str, label: &str) -> String {
        let begin = format!("-----BEGIN {label}-----");
        let end = format!("-----END {label}-----");
        let mut out = String::new();
        let mut rest = text;
        while let Some(s) = rest.find(&begin) {
            let e = rest[s..].find(&end).unwrap() + s + end.len();
            out.push_str(&rest[s..e]);
            out.push('\n');
            rest = &rest[e..];
        }
        out
    }

    fn b64_json(part: &str) -> Value {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
    }

    #[test]
    fn every_file_format_reads_the_same_certificate() {
        let loads = [
            ("sp.pem", None),
            ("sp_pkcs1.pem", None),
            ("sp_encrypted.pem", Some(PASSWORD)),
            ("sp_encrypted_sha1.pem", Some(PASSWORD)),
            ("sp_encrypted_libressl.pem", Some(PASSWORD)),
            ("sp_encrypted_3des.pem", Some(PASSWORD)),
            ("sp.pfx", Some(PASSWORD)),
            ("sp_legacy.pfx", Some(PASSWORD)),
        ];
        for (name, password) in loads {
            let c = Certificate::load(&fixture(name), password)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(c.x5t_s256(), X5T_S256, "{name}");
            assert_eq!(c.x5t(), X5T, "{name}");
            assert_eq!(c.thumbprint(), THUMBPRINT, "{name}");
            let public = std::fs::read(fixture("sp_public.der")).unwrap();
            assert_eq!(c.key.public_key().as_ref(), public.as_slice(), "{name}");
        }
        // A password on a key that has none is ignored.
        assert!(Certificate::load(&fixture("sp.pem"), Some("x")).is_ok());
    }

    #[test]
    fn the_assertion_follows_microsoft_learn_and_verifies() {
        let c = Certificate::load(&fixture("sp.pem"), None).unwrap();
        let aud = "https://login.microsoftonline.com/contoso.onmicrosoft.com/oauth2/v2.0/token";
        let client = "11111111-2222-3333-4444-555555555555";
        let public = std::fs::read(fixture("sp_public.der")).unwrap();
        let mut jtis = Vec::new();
        for (alg, name, verify) in [
            (Alg::Ps256, "PS256", &RSA_PSS_2048_8192_SHA256),
            (Alg::Rs256, "RS256", &RSA_PKCS1_2048_8192_SHA256),
        ] {
            let jwt = c.assertion(client, aud, alg, 1_800_000_000).unwrap();
            let parts: Vec<&str> = jwt.split('.').collect();
            assert_eq!(parts.len(), 3);
            let header = b64_json(parts[0]);
            assert_eq!(header["alg"], name);
            assert_eq!(header["typ"], "JWT");
            match alg {
                Alg::Ps256 => {
                    assert_eq!(header["x5t#S256"], X5T_S256);
                    assert!(header.get("x5t").is_none());
                }
                Alg::Rs256 => assert_eq!(header["x5t"], X5T),
            }
            let claims = b64_json(parts[1]);
            assert_eq!(claims["aud"], aud);
            assert_eq!(claims["iss"], client);
            assert_eq!(claims["sub"], client);
            assert_eq!(claims["nbf"], 1_800_000_000u64);
            assert_eq!(claims["iat"], 1_800_000_000u64);
            assert_eq!(claims["exp"], 1_800_000_600u64);
            let jti = claims["jti"].as_str().unwrap();
            assert!(uuid::Uuid::parse_str(jti).is_ok(), "{jti}");
            jtis.push(jti.to_string());
            // The signature over `header.claims`, with the certificate's
            // public key as OpenSSL wrote it.
            let signature = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
            assert_eq!(signature.len(), 256);
            let signed = format!("{}.{}", parts[0], parts[1]);
            UnparsedPublicKey::new(verify, &public)
                .verify(signed.as_bytes(), &signature)
                .unwrap_or_else(|_| panic!("{name} signature"));
            let mut tampered = signed.clone().into_bytes();
            tampered[3] ^= 1;
            assert!(UnparsedPublicKey::new(verify, &public)
                .verify(&tampered, &signature)
                .is_err());
        }
        assert_ne!(jtis[0], jtis[1], "a new jti each time");
    }

    #[test]
    fn clear_errors_for_files_that_cant_sign() {
        let err = |data: &[u8], password: Option<&str>| {
            Certificate::from_bytes(data, password)
                .unwrap_err()
                .to_string()
        };
        let sp = pem("sp.pem");
        let cert = blocks_of(&sp, "CERTIFICATE");
        let key = blocks_of(&sp, "PRIVATE KEY");
        let encrypted = pem("sp_encrypted.pem");
        let pfx = std::fs::read(fixture("sp.pfx")).unwrap();

        assert!(err(cert.as_bytes(), None).contains("no tiene la clave privada"));
        assert!(err(key.as_bytes(), None).contains("no tiene el certificado"));
        assert!(err(encrypted.as_bytes(), None).contains("escribí la contraseña"));
        assert!(err(encrypted.as_bytes(), Some("wrong")).contains("no es la correcta"));
        let sha1 = pem("sp_encrypted_sha1.pem");
        assert!(err(sha1.as_bytes(), Some("wrong")).contains("no es la correcta"));
        // An encryption DBine can't decrypt says so, not "wrong password",
        // with the right password.
        for name in ["sp_encrypted_pbes1.pem", "sp_encrypted_pkcs12_pbe.pem"] {
            let e = err(pem(name).as_bytes(), Some(PASSWORD));
            assert!(
                e.contains("no admite")
                    && e.contains("-v2 aes-256-cbc")
                    && !e.contains("contraseña"),
                "{name}: {e}"
            );
        }
        assert!(err(&pfx, Some("wrong")).contains("no es la correcta"));
        assert!(err(&pfx, None).contains("escribila"));
        assert!(err(b"not a certificate", None).contains("ni un PFX"));
        assert!(err(b"-----BEGIN CERTIFICATE-----\nAAAA", None).contains("incompleto"));

        // Another RSA key than the certificate's.
        let other = RsaKeyPair::generate(aws_lc_rs::rsa::KeySize::Rsa2048).unwrap();
        let other =
            aws_lc_rs::encoding::AsDer::<aws_lc_rs::encoding::Pkcs8V1Der>::as_der(&other).unwrap();
        let other_pem = format!(
            "{cert}-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            STANDARD.encode(other.as_ref())
        );
        assert!(err(other_pem.as_bytes(), None).contains("ningún certificado"));

        // An EC key, in PKCS #8 and in SEC 1.
        let rng = SystemRandom::new();
        let ec = aws_lc_rs::signature::EcdsaKeyPair::generate_pkcs8(
            &aws_lc_rs::signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            &rng,
        )
        .unwrap();
        let ec_pem = format!(
            "{cert}-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n",
            STANDARD.encode(ec.as_ref())
        );
        assert!(err(ec_pem.as_bytes(), None).contains("no es RSA"));
        let sec1 =
            format!("{cert}-----BEGIN EC PRIVATE KEY-----\nAAAA\n-----END EC PRIVATE KEY-----\n");
        assert!(err(sec1.as_bytes(), None).contains("no es RSA"));

        // OpenSSL's traditional encryption.
        let traditional = format!(
            "{cert}-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-256-CBC,00112233445566778899AABBCCDDEEFF\n\nAAAA\n-----END RSA PRIVATE KEY-----\n"
        );
        assert!(err(traditional.as_bytes(), Some(PASSWORD)).contains("pkcs8 -topk8"));

        // A missing file names it; a huge one isn't read whole.
        let missing = Certificate::load("/nonexistent/dbine/sp.pem", None).unwrap_err();
        assert!(
            missing.to_string().contains("/nonexistent/dbine/sp.pem"),
            "{missing}"
        );
        let huge = std::env::temp_dir().join(format!("dbine-powerbi-{}.pfx", uuid::Uuid::new_v4()));
        std::fs::write(&huge, vec![0u8; MAX_FILE as usize + 1]).unwrap();
        let e = Certificate::load(huge.to_str().unwrap(), None);
        std::fs::remove_file(&huge).unwrap();
        assert!(e.unwrap_err().to_string().contains("más de 1 MB"));
    }

    #[test]
    fn the_key_never_shows() {
        let c = Certificate::load(&fixture("sp_encrypted.pem"), Some(PASSWORD)).unwrap();
        let shown = format!("{c:?}");
        assert!(shown.contains(THUMBPRINT), "{shown}");
        let key = blocks_of(&pem("sp.pem"), "PRIVATE KEY");
        let body: String = key.lines().filter(|l| !l.starts_with("-----")).collect();
        assert!(!shown.contains(&body[..40]), "{shown}");
        // Errors never quote the file's content either.
        let e = Certificate::from_bytes(pem("sp_encrypted.pem").as_bytes(), Some("wrong"))
            .unwrap_err()
            .to_string();
        assert!(
            !e.contains(PASSWORD) && !e.contains("wrong") && !e.contains("MII"),
            "{e}"
        );
    }

    #[test]
    fn entra_errors_name_the_thumbprint() {
        let t = Certificate::load(&fixture("sp.pem"), None)
            .unwrap()
            .thumbprint();
        let e = explain_refusal(
            auth("Microsoft Entra ID rechazó el inicio de sesión: AADSTS700027: The certificate with identifier used to sign the client assertion is not registered on application."),
            &t,
        );
        // Within what the default chain keeps of each step's error.
        assert!(
            super::super::short(&e.to_string(), 200).contains(THUMBPRINT),
            "{e}"
        );
        let e = explain_refusal(
            auth("AADSTS700024: Client assertion is not within its valid time range."),
            &t,
        );
        assert!(e.to_string().contains("reloj"), "{e}");
        let e = explain_refusal(auth("AADSTS7000215"), &t);
        assert_eq!(e.to_string(), auth("AADSTS7000215").to_string());
    }
}
