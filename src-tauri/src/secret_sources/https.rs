//! The HTTP client of the providers that talk to a server the user names
//! (CyberArk, Delinea): HTTPS only (plain HTTP only to this computer), no
//! redirects (they'd carry a token elsewhere), an optional CA file and an
//! optional client certificate.

use super::expand_home;
use base64::Engine;
use dbine_core::secret_sources::{service_url, wipe, wipe_bytes};
use std::time::Duration;

/// A client that only talks to `url`'s scheme (HTTPS unless it's this
/// computer over HTTP).
pub fn client(
    url: &str,
    what: &str,
    ca_file: &str,
    identity: Option<reqwest::Identity>,
) -> Result<reqwest::Client, String> {
    let (https, _) = service_url(url).ok_or_else(|| {
        format!("la URL de {what} tiene que ser https:// (http:// solo para esta computadora)")
    })?;
    let mut b = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .https_only(https)
        .redirect(reqwest::redirect::Policy::none());
    if !ca_file.trim().is_empty() {
        let path = expand_home(ca_file.trim());
        let pem = std::fs::read(&path).map_err(|e| {
            format!(
                "no se pudo leer el certificado de la CA {}: {e}",
                path.display()
            )
        })?;
        let certs = reqwest::Certificate::from_pem_bundle(&pem)
            .map_err(|e| format!("el certificado de la CA no es válido: {e}"))?;
        if certs.is_empty() {
            return Err("el archivo de la CA no tiene certificados (PEM)".into());
        }
        for c in certs {
            b = b.add_root_certificate(c);
        }
    }
    if let Some(id) = identity {
        b = b.identity(id);
    }
    b.build().map_err(|e| e.to_string())
}

/// The base URL without the trailing `/`.
pub fn base(url: &str) -> &str {
    url.trim().trim_end_matches('/')
}

fn pem_block(label: &str, der: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or(""));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

#[cfg(test)]
pub fn tests_pem(label: &str, der: &[u8]) -> String {
    pem_block(label, der)
}

fn looks_pem(b: &[u8]) -> bool {
    std::str::from_utf8(b).is_ok_and(|t| t.contains("-----BEGIN "))
}

/// A client certificate: a PKCS#12 file (`.p12`/`.pfx`, opened with the
/// passphrase), or PEM: the certificate and its key in one file, or the
/// key in `key_file` (an encrypted PKCS#8 key is opened with the passphrase).
pub fn identity(
    cert_file: &str,
    key_file: &str,
    passphrase: Option<&str>,
) -> Result<reqwest::Identity, String> {
    let path = expand_home(cert_file.trim());
    let mut cert = std::fs::read(&path).map_err(|e| {
        format!(
            "no se pudo leer el certificado de cliente {}: {e}",
            path.display()
        )
    })?;
    let mut pem = if looks_pem(&cert) {
        let mut text = String::from_utf8(std::mem::take(&mut cert)).unwrap_or_default();
        if !key_file.trim().is_empty() {
            let kp = expand_home(key_file.trim());
            let mut key = std::fs::read_to_string(&kp)
                .map_err(|e| format!("no se pudo leer la clave privada {}: {e}", kp.display()))?;
            text.push('\n');
            text.push_str(&key);
            wipe(&mut key);
        }
        let opened = open_encrypted_key(&text, passphrase);
        wipe(&mut text);
        opened?
    } else {
        let r = pkcs12_to_pem(&cert, passphrase.unwrap_or(""));
        wipe_bytes(&mut cert);
        r?
    };
    let id = reqwest::Identity::from_pem(pem.as_bytes()).map_err(|_| "el certificado de cliente o su clave no son válidos (PEM: el certificado y una clave PKCS#8, RSA o EC)".to_string());
    wipe(&mut pem);
    id
}

/// The PEM with an `ENCRYPTED PRIVATE KEY` replaced by the opened key.
fn open_encrypted_key(text: &str, passphrase: Option<&str>) -> Result<String, String> {
    if text.contains("Proc-Type: 4,ENCRYPTED") {
        return Err("la clave privada usa el cifrado antiguo de OpenSSL: convertila a PKCS#8 (openssl pkcs8 -topk8) o usá un archivo PKCS#12".into());
    }
    const BEGIN: &str = "-----BEGIN ENCRYPTED PRIVATE KEY-----";
    const END: &str = "-----END ENCRYPTED PRIVATE KEY-----";
    let Some(start) = text.find(BEGIN) else {
        return Ok(text.to_string());
    };
    let end = text[start..]
        .find(END)
        .map(|e| start + e)
        .ok_or("la clave privada cifrada está incompleta")?;
    let body: String = text[start + BEGIN.len()..end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let der = base64::engine::general_purpose::STANDARD
        .decode(body.as_bytes())
        .map_err(|_| "la clave privada cifrada no es válida")?;
    let pass = passphrase
        .filter(|p| !p.is_empty())
        .ok_or("la clave privada está cifrada: escribí su frase de contraseña")?;
    let info = pkcs8::EncryptedPrivateKeyInfoRef::try_from(der.as_slice())
        .map_err(|_| "la clave privada cifrada no es válida")?;
    let doc = info
        .decrypt(pass)
        .map_err(|_| "la frase de contraseña de la clave privada no es la correcta")?;
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push_str(&pem_block("PRIVATE KEY", doc.as_bytes()));
    out.push_str(&text[end + END.len()..]);
    Ok(out)
}

fn pkcs12_to_pem(data: &[u8], passphrase: &str) -> Result<String, String> {
    let store = p12_keystore::KeyStore::from_pkcs12(
        data,
        passphrase,
        p12_keystore::Pkcs12ImportPolicy::Relaxed,
    )
    .map_err(|_| {
        "no se pudo abrir el archivo PKCS#12 (¿la frase de contraseña es la correcta?)".to_string()
    })?;
    let (_, chain) = store
        .private_key_chain()
        .ok_or("el archivo PKCS#12 no tiene una clave privada con su certificado")?;
    if chain.certs().is_empty() {
        return Err("el archivo PKCS#12 no tiene el certificado de la clave".into());
    }
    let mut pem = pem_block("PRIVATE KEY", chain.key().as_der());
    for c in chain.certs() {
        pem.push_str(&pem_block("CERTIFICATE", c.as_der()));
    }
    Ok(pem)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pem_blocks_wrap_at_64() {
        let p = pem_block("X", &[7u8; 100]);
        assert!(p.starts_with("-----BEGIN X-----\n") && p.ends_with("-----END X-----\n"));
        assert!(p.lines().all(|l| l.len() <= 64));
    }

    #[test]
    fn plain_http_only_to_this_computer() {
        assert!(client("http://127.0.0.1:9", "X", "", None).is_ok());
        assert!(client("https://ccp.example.com", "X", "", None).is_ok());
        let e = client("http://ccp.example.com", "Conjur", "", None).unwrap_err();
        assert!(e.contains("https://"), "{e}");
    }
}
