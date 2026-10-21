//! RSA user keys sign with aws-lc-rs (constant time), not with the `rsa`
//! crate russh would use: `rsa` has a timing side channel in private-key
//! operations (RUSTSEC-2023-0071, "Marvin", no fixed version). russh still
//! reads the key and checks host keys with `rsa`; those are public-key
//! operations, which the advisory doesn't affect.
//!
//! Only rsa-sha2-256 and rsa-sha2-512 (RFC 8332): aws-lc-rs has no SHA-1
//! signing, and OpenSSH itself turned `ssh-rsa` off by default in 8.8.

use russh::keys::{HashAlg, PrivateKey};
use russh::keys::ssh_key::private::KeypairData;
use russh::keys::agent::AgentIdentity;
use russh::SendError;

use aws_lc_rs::rand::SystemRandom;
use aws_lc_rs::rsa::KeyPair;
use aws_lc_rs::signature::{RSA_PKCS1_SHA256, RSA_PKCS1_SHA512};

/// What went wrong signing (the session's or the key's).
#[derive(Debug)]
pub enum SignError {
    Send,
    Sign,
}

impl From<SendError> for SignError {
    fn from(_: SendError) -> Self {
        SignError::Send
    }
}

/// An RSA user key, ready to sign with aws-lc-rs.
pub struct RsaSigner {
    pair: KeyPair,
}

impl RsaSigner {
    /// The key as aws-lc-rs takes it, when it's an RSA key (`None` for any
    /// other kind, which russh signs itself).
    pub fn from_key(key: &PrivateKey) -> Option<Result<Self, String>> {
        let KeypairData::Rsa(rsa) = key.key_data() else {
            return None;
        };
        // Converting and encoding the key are no private-key operations: the
        // signing is what runs in aws-lc-rs.
        use rsa::pkcs1::EncodeRsaPrivateKey;
        let built = rsa::RsaPrivateKey::try_from(rsa)
            .map_err(|e| e.to_string())
            .and_then(|k| k.to_pkcs1_der().map_err(|e| e.to_string()))
            .and_then(|der| KeyPair::from_der(der.as_bytes()).map_err(|e| e.to_string()));
        Some(built.map(|pair| RsaSigner { pair }))
    }
}

/// The SSH signature blob after `data`: `string algorithm, string signature`
/// inside a `string`, as an agent's answer is laid out.
fn append_signature(mut data: Vec<u8>, name: &str, sig: &[u8]) -> Vec<u8> {
    let inner = 4 + name.len() + 4 + sig.len();
    data.extend_from_slice(&(inner as u32).to_be_bytes());
    data.extend_from_slice(&(name.len() as u32).to_be_bytes());
    data.extend_from_slice(name.as_bytes());
    data.extend_from_slice(&(sig.len() as u32).to_be_bytes());
    data.extend_from_slice(sig);
    data
}

impl russh::Signer for RsaSigner {
    type Error = SignError;

    #[allow(clippy::manual_async_fn)]
    fn auth_sign(
        &mut self,
        _key: &AgentIdentity,
        hash_alg: Option<HashAlg>,
        to_sign: Vec<u8>,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, Self::Error>> + Send {
        let signed = (|| {
            let (name, alg) = match hash_alg {
                Some(HashAlg::Sha512) => ("rsa-sha2-512", &RSA_PKCS1_SHA512),
                // rsa-sha2-256 also when the server named nothing: every
                // OpenSSH since 7.2 takes it.
                _ => ("rsa-sha2-256", &RSA_PKCS1_SHA256),
            };
            let mut sig = vec![0; self.pair.public_modulus_len()];
            self.pair.sign(alg, &SystemRandom::new(), &to_sign, &mut sig).map_err(|_| SignError::Sign)?;
            Ok(append_signature(to_sign, name, &sig))
        })();
        async move { signed }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::encoding::AsDer;
    use aws_lc_rs::signature::KeyPair as _;
    use aws_lc_rs::rsa::KeySize;
    use russh::keys::ssh_key::private::RsaKeypair;
    use russh::keys::ssh_key::Algorithm;

    /// A throwaway RSA key, made by aws-lc-rs for the test (nothing on disk).
    fn rsa_key() -> PrivateKey {
        use rsa::pkcs8::DecodePrivateKey;
        let pair = KeyPair::generate(KeySize::Rsa2048).unwrap();
        let der = AsDer::<aws_lc_rs::encoding::Pkcs8V1Der>::as_der(&pair).unwrap();
        let key = rsa::RsaPrivateKey::from_pkcs8_der(der.as_ref()).unwrap();
        PrivateKey::new(KeypairData::Rsa(RsaKeypair::try_from(key).unwrap()), "test").unwrap()
    }

    #[test]
    fn signs_like_an_agent_and_the_public_key_verifies_it() {
        let key = rsa_key();
        let mut signer = RsaSigner::from_key(&key).unwrap().unwrap();
        let id = AgentIdentity::PublicKey { key: key.public_key().clone(), comment: String::new() };
        let data = b"session data".to_vec();
        for (hash, name) in [(Some(HashAlg::Sha256), "rsa-sha2-256"), (Some(HashAlg::Sha512), "rsa-sha2-512"), (None, "rsa-sha2-256")] {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            let out = rt.block_on(russh::Signer::auth_sign(&mut signer, &id, hash, data.clone())).unwrap();
            assert!(out.starts_with(&data));
            let blob = &out[data.len() + 4..];
            let n = u32::from_be_bytes(blob[..4].try_into().unwrap()) as usize;
            assert_eq!(&blob[4..4 + n], name.as_bytes());
            let sig = &blob[4 + n + 4..];
            // Checked with aws-lc-rs against the public half.
            let public = aws_lc_rs::signature::UnparsedPublicKey::new(
                if name == "rsa-sha2-512" { &aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA512 } else { &aws_lc_rs::signature::RSA_PKCS1_2048_8192_SHA256 },
                signer.pair.public_key().as_ref().to_vec(),
            );
            public.verify(&data, sig).unwrap();
        }
    }

    #[test]
    fn other_kinds_are_left_to_russh() {
        let key = PrivateKey::new(
            KeypairData::Ed25519(russh::keys::ssh_key::private::Ed25519Keypair::from_seed(&[7; 32])),
            "test",
        )
        .unwrap();
        assert!(RsaSigner::from_key(&key).is_none());
        let _ = Algorithm::Ed25519;
    }
}
