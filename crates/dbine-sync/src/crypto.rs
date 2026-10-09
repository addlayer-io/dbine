//! The backup file: end-to-end encrypted with a key derived from the user's
//! passphrase, which never leaves the machine (docs/sync.md).
//!
//! - Key: Argon2id(passphrase, random 16-byte salt) → 32 bytes.
//! - Cipher: XChaCha20-Poly1305 with a random 24-byte nonce per write.
//! - The file is a small JSON envelope: the plaintext header (format,
//!   dates, device name, KDF parameters) is bound to the ciphertext as
//!   associated data, so it can't be altered without the file failing to
//!   open; everything the user keeps (connections, hosts, users, queries,
//!   passwords) is inside `data`.

use crate::{Result, SyncError};
use argon2::{Algorithm, Argon2, Params, Version};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;
use serde::{Deserialize, Serialize};

pub const FORMAT: &str = "dbine-backup";
pub const VERSION: u32 = 1;

/// Argon2id parameters (stored in the file, so they can be raised later
/// without breaking older backups).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kdf {
    pub alg: String,
    /// Base64.
    pub salt: String,
    /// Memory in KiB.
    pub m_cost: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

impl Kdf {
    /// Fresh parameters with a random salt (OWASP's Argon2id baseline, with
    /// more memory: a backup is opened rarely, so it can afford ~0.3 s).
    pub fn generate() -> Self {
        let mut salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);
        Self { alg: "argon2id".into(), salt: B64.encode(salt), m_cost: 64 * 1024, t_cost: 3, p_cost: 1 }
    }

    #[cfg(test)]
    pub fn fast() -> Self {
        Self { m_cost: 1024, t_cost: 1, ..Self::generate() }
    }
}

/// The policy `Kdf::fast()` passes (tests only).
#[cfg(test)]
pub const FAST_POLICY: KdfPolicy = KdfPolicy { min_m_cost: 1024, min_t_cost: 1, ..POLICY };

/// A derived key; it zeroes itself when dropped.
pub struct Key([u8; 32]);

impl Drop for Key {
    fn drop(&mut self) {
        self.0.iter_mut().for_each(|b| *b = 0);
    }
}

/// Bounds for KDF parameters read from a backup file. The header is written
/// by whoever can write the backup location, so it's untrusted until the
/// file decrypts: a floor stops a forged header from making this machine
/// derive (and later re-encrypt everything with) a weak key, and a ceiling
/// stops it from exhausting memory or CPU.
#[derive(Debug, Clone, Copy)]
pub struct KdfPolicy {
    pub min_m_cost: u32,
    pub max_m_cost: u32,
    pub min_t_cost: u32,
    pub max_t_cost: u32,
    pub min_p_cost: u32,
    pub max_p_cost: u32,
    pub min_salt: usize,
    pub max_salt: usize,
}

/// The floor is exactly what `Kdf::generate()` writes (and has written since
/// the first version), so every legitimate backup opens.
pub const POLICY: KdfPolicy = KdfPolicy {
    min_m_cost: 64 * 1024,
    max_m_cost: 1024 * 1024,
    min_t_cost: 3,
    max_t_cost: 20,
    min_p_cost: 1,
    max_p_cost: 16,
    min_salt: 16,
    max_salt: 64,
};

impl Kdf {
    /// Reject an unknown algorithm or parameters outside `policy`.
    pub fn check(&self, policy: &KdfPolicy) -> Result<()> {
        if self.alg != "argon2id" {
            return Err(SyncError::Format(format!("algoritmo de clave desconocido: {}", self.alg)));
        }
        let salt = B64.decode(&self.salt).map_err(|_| SyncError::Format("sal inválida".into()))?;
        let ok = (policy.min_salt..=policy.max_salt).contains(&salt.len())
            && (policy.min_m_cost..=policy.max_m_cost).contains(&self.m_cost)
            && (policy.min_t_cost..=policy.max_t_cost).contains(&self.t_cost)
            && (policy.min_p_cost..=policy.max_p_cost).contains(&self.p_cost);
        if !ok {
            return Err(SyncError::Format("los parámetros de clave del backup están fuera de lo permitido: el archivo no es confiable".into()));
        }
        Ok(())
    }
}

/// Derive the key for `passphrase` with `kdf`, which must pass `POLICY`.
pub fn derive(passphrase: &str, kdf: &Kdf) -> Result<Key> {
    derive_with(passphrase, kdf, &POLICY)
}

/// `derive` with an explicit policy (tests use a cheaper one).
pub fn derive_with(passphrase: &str, kdf: &Kdf, policy: &KdfPolicy) -> Result<Key> {
    kdf.check(policy)?;
    let salt = B64.decode(&kdf.salt).map_err(|_| SyncError::Format("sal inválida".into()))?;
    let params = Params::new(kdf.m_cost, kdf.t_cost, kdf.p_cost, Some(32))
        .map_err(|e| SyncError::Format(format!("parámetros de clave inválidos: {e}")))?;
    let mut out = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), &salt, &mut out)
        .map_err(|e| SyncError::Format(format!("no se pudo derivar la clave: {e}")))?;
    Ok(Key(out))
}

/// What can be read from a backup without the passphrase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub format: String,
    pub version: u32,
    /// When it was written (RFC 3339).
    pub updated_at: String,
    /// The machine that wrote it.
    pub device: String,
    pub app_version: String,
    pub kdf: Kdf,
    pub cipher: String,
}

#[derive(Serialize, Deserialize)]
struct Envelope {
    #[serde(flatten)]
    header: Header,
    nonce: String,
    data: String,
}

fn aad(h: &Header) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(h)?)
}

/// Encrypt `plain` into a backup file.
pub fn seal(key: &Key, header: Header, plain: &[u8]) -> Result<Vec<u8>> {
    let mut nonce = [0u8; 24];
    rand::thread_rng().fill_bytes(&mut nonce);
    let cipher = XChaCha20Poly1305::new((&key.0).into());
    let data = cipher
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad: &aad(&header)? })
        .map_err(|_| SyncError::Format("no se pudo cifrar".into()))?;
    Ok(serde_json::to_vec_pretty(&Envelope { header, nonce: B64.encode(nonce), data: B64.encode(data) })?)
}

/// The header of a backup file (no passphrase needed).
pub fn peek(file: &[u8]) -> Result<Header> {
    let env: Envelope = serde_json::from_slice(file).map_err(|_| SyncError::Format("el archivo no es un backup de DBine".into()))?;
    if env.header.format != FORMAT {
        return Err(SyncError::Format("el archivo no es un backup de DBine".into()));
    }
    if env.header.version > VERSION {
        return Err(SyncError::Format("el backup lo escribió una versión más nueva de DBine: actualizá la app".into()));
    }
    Ok(env.header)
}

/// Decrypt a backup file with a key derived from its own KDF parameters.
pub fn open(key: &Key, file: &[u8]) -> Result<Vec<u8>> {
    peek(file)?;
    let env: Envelope = serde_json::from_slice(file)?;
    let nonce = B64.decode(&env.nonce).map_err(|_| SyncError::Format("nonce inválido".into()))?;
    if nonce.len() != 24 {
        return Err(SyncError::Format("nonce inválido".into()));
    }
    let data = B64.decode(&env.data).map_err(|_| SyncError::Format("contenido inválido".into()))?;
    XChaCha20Poly1305::new((&key.0).into())
        .decrypt(XNonce::from_slice(&nonce), Payload { msg: &data, aad: &aad(&env.header)? })
        // A wrong key and a tampered file look the same to an AEAD.
        .map_err(|_| SyncError::WrongPassphrase)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(kdf: Kdf) -> Header {
        Header {
            format: FORMAT.into(),
            version: VERSION,
            updated_at: "2026-01-01T00:00:00Z".into(),
            device: "mac".into(),
            app_version: "0.1.0".into(),
            kdf,
            cipher: "xchacha20poly1305".into(),
        }
    }

    #[test]
    fn roundtrip_and_nothing_readable_in_the_file() {
        let kdf = Kdf::fast();
        let key = derive_with("una frase larga", &kdf, &FAST_POLICY).unwrap();
        let file = seal(&key, header(kdf.clone()), b"host=prod.example password=hunter2").unwrap();
        let text = String::from_utf8(file.clone()).unwrap();
        assert!(!text.contains("hunter2") && !text.contains("prod.example"));
        assert_eq!(open(&derive_with("una frase larga", &kdf, &FAST_POLICY).unwrap(), &file).unwrap(), b"host=prod.example password=hunter2");
        assert_eq!(peek(&file).unwrap().device, "mac");
    }

    #[test]
    fn wrong_passphrase_fails() {
        let kdf = Kdf::fast();
        let file = seal(&derive_with("correcta", &kdf, &FAST_POLICY).unwrap(), header(kdf.clone()), b"x").unwrap();
        assert!(matches!(open(&derive_with("incorrecta", &kdf, &FAST_POLICY).unwrap(), &file), Err(SyncError::WrongPassphrase)));
    }

    #[test]
    fn tampered_header_fails() {
        let kdf = Kdf::fast();
        let key = derive_with("frase", &kdf, &FAST_POLICY).unwrap();
        let file = seal(&key, header(kdf), b"x").unwrap();
        let tampered = String::from_utf8(file).unwrap().replace("\"mac\"", "\"otra\"");
        assert!(open(&key, tampered.as_bytes()).is_err());
    }

    #[test]
    fn not_a_backup() {
        assert!(matches!(peek(b"{\"hola\":1}"), Err(SyncError::Format(_))));
    }

    fn weak(m_cost: u32, t_cost: u32, p_cost: u32, salt_len: usize) -> Kdf {
        Kdf { alg: "argon2id".into(), salt: B64.encode(vec![7u8; salt_len]), m_cost, t_cost, p_cost }
    }

    #[test]
    fn generated_parameters_pass_the_policy() {
        // What every DBine version has written: 64 MiB, t=3, p=1, 16-byte salt.
        Kdf::generate().check(&POLICY).unwrap();
        weak(64 * 1024, 3, 1, 16).check(&POLICY).unwrap();
    }

    #[test]
    fn parameters_below_the_floor_are_rejected() {
        for k in [weak(8, 1, 1, 16), weak(64 * 1024 - 1, 3, 1, 16), weak(64 * 1024, 2, 1, 16), weak(64 * 1024, 3, 0, 16), weak(64 * 1024, 3, 1, 8)] {
            assert!(matches!(k.check(&POLICY), Err(SyncError::Format(_))), "{k:?}");
            // `derive` refuses before hashing.
            assert!(matches!(derive("frase", &k), Err(SyncError::Format(_))), "{k:?}");
        }
    }

    #[test]
    fn parameters_above_the_ceiling_are_rejected() {
        // Checked before Argon2 allocates anything (4 GiB here).
        for k in [weak(4 * 1024 * 1024, 3, 1, 16), weak(64 * 1024, 1000, 1, 16), weak(64 * 1024, 3, 64, 16), weak(64 * 1024, 3, 1, 4096)] {
            assert!(matches!(derive("frase", &k), Err(SyncError::Format(_))), "{k:?}");
        }
    }

    #[test]
    fn unknown_algorithm_is_rejected() {
        let k = Kdf { alg: "pbkdf2".into(), ..Kdf::generate() };
        assert!(matches!(derive("frase", &k), Err(SyncError::Format(_))));
    }
}
