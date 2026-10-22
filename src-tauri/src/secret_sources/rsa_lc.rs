//! RSA decryption with a private key, done by AWS-LC (constant time) instead
//! of the `rsa` crate, which leaks timing in private-key operations
//! (RUSTSEC-2023-0071, "Marvin", no fixed version). `rsa` still builds and
//! encodes the keys these managers hand out (jsbn's hex parts, PKCS#8): that
//! is no decryption.
//!
//! PKCS#1 v1.5 goes through aws-lc-sys directly: Zoho's older vaults use
//! 1024-bit keys, below what aws-lc-rs's safe wrapper takes (2048 to 8192).
//! OAEP goes through aws-lc-rs.

use aws_lc_rs::rsa::{
    OaepPrivateDecryptingKey, PrivateDecryptingKey, OAEP_SHA1_MGF1SHA1, OAEP_SHA256_MGF1SHA256,
};
use rsa::pkcs1::EncodeRsaPrivateKey;
use rsa::pkcs8::EncodePrivateKey;

#[derive(Clone, Copy, Debug)]
pub enum Padding {
    Pkcs1v15,
    OaepSha1,
    OaepSha256,
}

/// `ciphertext` decrypted with `key`; `None` if the padding doesn't check
/// out (or the key can't be used), as the callers treat any refusal alike.
pub fn decrypt(key: &rsa::RsaPrivateKey, padding: Padding, ciphertext: &[u8]) -> Option<Vec<u8>> {
    match padding {
        Padding::Pkcs1v15 => pkcs1v15(key, ciphertext),
        Padding::OaepSha1 | Padding::OaepSha256 => {
            let der = key.to_pkcs8_der().ok()?;
            let key = OaepPrivateDecryptingKey::new(
                PrivateDecryptingKey::from_pkcs8(der.as_bytes()).ok()?,
            )
            .ok()?;
            let alg = if matches!(padding, Padding::OaepSha1) {
                &OAEP_SHA1_MGF1SHA1
            } else {
                &OAEP_SHA256_MGF1SHA256
            };
            let mut out = vec![0u8; key.min_output_size()];
            let plain = key
                .decrypt(alg, ciphertext, &mut out, None)
                .ok()
                .map(|p| p.to_vec());
            out.iter_mut().for_each(|b| *b = 0);
            plain
        }
    }
}

fn pkcs1v15(key: &rsa::RsaPrivateKey, ciphertext: &[u8]) -> Option<Vec<u8>> {
    // The key's DER is zeroized when dropped (`SecretDocument`).
    let der = key.to_pkcs1_der().ok()?;
    let bytes = der.as_bytes();
    // SAFETY: AWS-LC's documented API. The key is parsed from a buffer that
    // outlives the call and freed below; `out` has `RSA_size` bytes, the most
    // RSA_decrypt writes, and `n` how many it did.
    unsafe {
        let rsa = aws_lc_sys::RSA_private_key_from_bytes(bytes.as_ptr(), bytes.len());
        if rsa.is_null() {
            return None;
        }
        let mut out = vec![0u8; aws_lc_sys::RSA_size(rsa) as usize];
        let mut n = 0usize;
        let ok = aws_lc_sys::RSA_decrypt(
            rsa,
            &mut n,
            out.as_mut_ptr(),
            out.len(),
            ciphertext.as_ptr(),
            ciphertext.len(),
            aws_lc_sys::RSA_PKCS1_PADDING as _,
        );
        aws_lc_sys::RSA_free(rsa);
        if ok == 1 && n <= out.len() {
            let plain = out[..n].to_vec();
            out.iter_mut().for_each(|b| *b = 0);
            Some(plain)
        } else {
            out.iter_mut().for_each(|b| *b = 0);
            None
        }
    }
}

// Exercised end to end by the Zoho (RSA_1024 and RSA_4096) and Bitwarden
// (RSA-OAEP) test vectors in zoho.rs and bitwarden.rs.
