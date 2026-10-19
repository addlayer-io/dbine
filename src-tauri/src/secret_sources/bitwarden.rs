//! Bitwarden, natively (no `bw` or `bws` program): two sources.
//!
//! **The password manager** (personal and organization vaults). Bitwarden
//! encrypts every value in the client, so DBine does what its clients do:
//!
//! 1. An access token from the user's personal API key (`client_id`
//!    `user.<uuid>`, `client_secret`): `POST <identity>/connect/token`,
//!    `client_credentials`, scope `api`. The API key skips two-step login and
//!    new-device checks by design; the vault stays encrypted.
//! 2. `GET <api>/sync`: the encrypted vault.
//! 3. The master key from the master password: PBKDF2-SHA256 (salt: the
//!    e-mail) or Argon2id (salt: SHA-256 of the e-mail), with the account's
//!    parameters (the sync's `userDecryption`, else `prelogin`). Stretched
//!    with HKDF-Expand ("enc", "mac") it opens the user key (an EncString of
//!    type 2: AES-256-CBC with an HMAC-SHA256 that is checked first, so a
//!    wrong password says so).
//! 4. An organization's items: its key is RSA-OAEP (SHA-1, or SHA-256) for the
//!    user's private key, itself an EncString under the user key. An item
//!    may have its own key (`key`), an EncString under the user's or the
//!    organization's key.
//! 5. The item, by ID or exact name (names are decrypted to compare; more
//!    than one is an error listing their IDs), and the field.
//!
//! **Secrets Manager**: a machine account's access token
//! (`0.<id>.<secret>:<base64 key>`). Its key opens the encrypted payload of
//! the token response (the organization's key), which opens the secret.
//!
//! The server is part of the definition: `bitwarden.com`, `bitwarden.eu` or a
//! self-hosted URL (dbine_core::secret_sources::bitwarden_urls). Tokens only
//! go to its `/identity` and `/api`, over HTTPS (plain HTTP only for this
//! computer), without following redirects. Nothing here logs a value, and
//! errors carry Bitwarden's messages, item names and IDs, never a token, a
//! key or a value.

use aes::cipher::{generic_array::GenericArray, BlockDecrypt, KeyInit};
use aes::Aes256;
use base64::Engine;
use dbine_core::secret_sources::{bitwarden_urls, is_uuid, wipe, wipe_bytes};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// The master password didn't open the user key. `fill` checks for this
/// exact text to ask an asked-for password again.
pub const WRONG_PASSWORD: &str = "la contraseña maestra de Bitwarden no es la correcta";

const B64: base64::engine::general_purpose::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// The device type sent to Bitwarden: SDK.
const DEVICE_TYPE: &str = "21";

/// The base URLs to talk to: the server's `/identity` and `/api`.
pub struct Endpoints {
    pub identity: String,
    pub api: String,
}

impl Endpoints {
    /// The server's, from the definition. Nothing else is ever used outside
    /// the tests.
    pub fn of(server: &str) -> Option<Self> {
        bitwarden_urls(server).map(|(identity, api)| Self { identity, api })
    }
}

/// No redirects; HTTPS only unless the server is plain HTTP on this
/// computer (bitwarden_urls allows nothing else).
fn client_for(ep: &Endpoints) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .https_only(ep.identity.starts_with("https://"))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| e.to_string())
}

/// `host[:port]` of a URL, for messages.
fn origin(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split('/').next().unwrap_or(rest)
}

// ---- keys and EncStrings -----------------------------------------------------

/// A symmetric key: AES-256 and, except for the legacy master key, the
/// HMAC-SHA256 key. Zeroed on drop.
#[derive(Clone)]
pub struct SymKey {
    enc: [u8; 32],
    mac: Option<[u8; 32]>,
}

impl Drop for SymKey {
    fn drop(&mut self) {
        for b in self.enc.iter_mut().chain(self.mac.iter_mut().flatten()) {
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

impl SymKey {
    /// 64 bytes (AES and MAC keys) or 32 (a bare AES key).
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        let mut enc = [0u8; 32];
        match b.len() {
            64 => {
                let mut mac = [0u8; 32];
                enc.copy_from_slice(&b[..32]);
                mac.copy_from_slice(&b[32..]);
                Some(Self { enc, mac: Some(mac) })
            }
            32 => {
                enc.copy_from_slice(b);
                Some(Self { enc, mac: None })
            }
            _ => None,
        }
    }
}

/// Why an EncString didn't open.
#[derive(Debug, PartialEq)]
pub enum Fail {
    /// The MAC didn't match: another key (or tampered data).
    Mac,
    /// Not a well-formed EncString for that key.
    Format,
    /// A type this code doesn't read (7: the v2 "COSE" format).
    Unsupported(u8),
}

fn unsupported(t: u8) -> String {
    if t == 7 {
        "esta cuenta de Bitwarden usa el formato de cifrado nuevo (v2), que DBine todavía no lee".into()
    } else {
        format!("Bitwarden devolvió un valor cifrado de un tipo que DBine no lee ({t})")
    }
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hmac_parts(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let msg: Vec<u8> = parts.concat();
    super::cloud_auth::hmac(key, &msg)
}

/// HKDF-Expand (RFC 5869) with SHA-256: `prk` is already a key.
pub fn hkdf_expand(prk: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 32);
    let mut t: Vec<u8> = Vec::new();
    let mut i = 1u8;
    while out.len() < len {
        t = hmac_parts(prk, &[&t, info, &[i]]).to_vec();
        out.extend_from_slice(&t);
        i += 1;
    }
    out.truncate(len);
    out
}

fn aes_cbc_decrypt(key: &[u8; 32], iv: &[u8], ct: &[u8]) -> Option<Vec<u8>> {
    if iv.len() != 16 || ct.is_empty() || !ct.len().is_multiple_of(16) {
        return None;
    }
    let cipher = Aes256::new(GenericArray::from_slice(key));
    let mut out = ct.to_vec();
    let mut prev = [0u8; 16];
    prev.copy_from_slice(iv);
    for chunk in out.chunks_mut(16) {
        let mut saved = [0u8; 16];
        saved.copy_from_slice(chunk);
        cipher.decrypt_block(GenericArray::from_mut_slice(chunk));
        for (b, p) in chunk.iter_mut().zip(prev.iter()) {
            *b ^= p;
        }
        prev = saved;
    }
    let pad = *out.last()? as usize;
    if pad == 0 || pad > 16 || !out[out.len() - pad..].iter().all(|b| *b as usize == pad) {
        wipe_bytes(&mut out);
        return None;
    }
    out.truncate(out.len() - pad);
    Some(out)
}

/// An EncString's bytes: type 2 (`2.iv|ct|mac`: AES-256-CBC, HMAC-SHA256
/// over iv and ciphertext, checked first) or, with a bare 32-byte key only
/// (an old account's user key), type 0 (`0.iv|ct`, no MAC).
pub fn decrypt(s: &str, key: &SymKey) -> Result<Vec<u8>, Fail> {
    let s = s.trim();
    let (kind, rest) = match s.split_once('.') {
        Some((t, r)) if !t.is_empty() && t.len() <= 2 && t.chars().all(|c| c.is_ascii_digit()) => (t.parse::<u8>().map_err(|_| Fail::Format)?, r),
        // Very old strings carry no type: two parts are 0, three are 2.
        _ => (if s.split('|').count() == 3 { 2 } else { 0 }, s),
    };
    let parts: Vec<&str> = rest.split('|').collect();
    let (iv, ct) = match (kind, parts.as_slice()) {
        (2, [iv, ct, mac]) => {
            let mac_key = key.mac.as_ref().ok_or(Fail::Format)?;
            let iv = B64.decode(iv).map_err(|_| Fail::Format)?;
            let ct = B64.decode(ct).map_err(|_| Fail::Format)?;
            let mac = B64.decode(mac).map_err(|_| Fail::Format)?;
            if !ct_eq(&hmac_parts(mac_key, &[&iv, &ct]), &mac) {
                return Err(Fail::Mac);
            }
            (iv, ct)
        }
        // Without a MAC only for a key that has none: a MAC'd key never
        // accepts an unauthenticated value (a server can't downgrade it).
        (0, [iv, ct]) if key.mac.is_none() => (B64.decode(iv).map_err(|_| Fail::Format)?, B64.decode(ct).map_err(|_| Fail::Format)?),
        (0 | 2, _) => return Err(Fail::Format),
        (t, _) => return Err(Fail::Unsupported(t)),
    };
    aes_cbc_decrypt(&key.enc, &iv, &ct).ok_or(Fail::Format)
}

/// An EncString as text; `None` when absent or null.
fn decrypt_text(v: Option<&Value>, key: &SymKey) -> Result<Option<String>, Fail> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => {
            let bytes = decrypt(s, key)?;
            String::from_utf8(bytes).map(Some).map_err(|e| {
                wipe_bytes(&mut e.into_bytes());
                Fail::Format
            })
        }
        _ => Ok(None),
    }
}

/// An RSA EncString: type 3 (OAEP SHA-256) or 4 (OAEP SHA-1); 5 and 6 are
/// the same with a MAC the current clients ignore.
pub fn rsa_decrypt(s: &str, key: &rsa::RsaPrivateKey) -> Result<Vec<u8>, Fail> {
    let (t, rest) = s.trim().split_once('.').ok_or(Fail::Format)?;
    let ct = B64.decode(rest.split('|').next().unwrap_or("")).map_err(|_| Fail::Format)?;
    match t {
        "3" | "5" => key.decrypt(rsa::Oaep::<rsa::sha2::Sha256>::new(), &ct).map_err(|_| Fail::Mac),
        "4" | "6" => key.decrypt(rsa::Oaep::<sha1::Sha1>::new(), &ct).map_err(|_| Fail::Mac),
        t => Err(Fail::Unsupported(t.parse().unwrap_or(255))),
    }
}

// ---- key derivation ----------------------------------------------------------

/// The account's KDF: 0 PBKDF2-SHA256, 1 Argon2id (memory in MiB).
#[derive(Debug, Clone, PartialEq)]
pub struct Kdf {
    pub kind: u64,
    pub iterations: u64,
    pub memory: u64,
    pub parallelism: u64,
}

/// The master key (32 bytes). Parameters outside what Bitwarden allows are
/// refused (a server can't make this take forever).
pub fn master_key(password: &str, salt: &str, kdf: &Kdf) -> Result<[u8; 32], String> {
    let mut out = [0u8; 32];
    match kdf.kind {
        0 => {
            if !(5_000..=2_000_000).contains(&kdf.iterations) {
                return Err(format!("Bitwarden devolvió una cantidad de iteraciones de PBKDF2 fuera de rango ({})", kdf.iterations));
            }
            pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), salt.as_bytes(), kdf.iterations as u32, &mut out);
        }
        1 => {
            if !(2..=10).contains(&kdf.iterations) || !(15..=1024).contains(&kdf.memory) || !(1..=16).contains(&kdf.parallelism) {
                return Err("Bitwarden devolvió parámetros de Argon2 fuera de rango".into());
            }
            let params = argon2::Params::new((kdf.memory * 1024) as u32, kdf.iterations as u32, kdf.parallelism as u32, Some(32)).map_err(|e| format!("parámetros de Argon2 no válidos: {e}"))?;
            let salt = Sha256::digest(salt.as_bytes());
            argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
                .hash_password_into(password.as_bytes(), &salt, &mut out)
                .map_err(|e| format!("Argon2 falló: {e}"))?;
        }
        k => return Err(format!("Bitwarden usa una derivación de clave que DBine no conoce ({k})")),
    }
    Ok(out)
}

/// HKDF-Expand of the master key into the AES and MAC keys that open the
/// user key.
pub fn stretch(master: &[u8; 32]) -> SymKey {
    let mut both = hkdf_expand(master, b"enc", 32);
    both.extend(hkdf_expand(master, b"mac", 32));
    let k = SymKey::from_bytes(&both).expect("64 bytes");
    wipe_bytes(&mut both);
    k
}

/// The user key, from the master password and the master-key-wrapped user
/// key. Any failure to open it is the wrong password.
pub fn user_key(password: &str, salt: &str, kdf: &Kdf, wrapped: &str) -> Result<SymKey, String> {
    let mut master = master_key(password, salt, kdf)?;
    // Type 0 (very old accounts): the bare master key; else stretched.
    let typed0 = wrapped.trim().starts_with("0.") || (!wrapped.contains('.') && wrapped.split('|').count() == 2);
    let key = if typed0 { SymKey::from_bytes(&master).expect("32 bytes") } else { stretch(&master) };
    master.iter_mut().for_each(|b| *b = 0);
    let mut bytes = match decrypt(wrapped, &key) {
        Ok(b) => b,
        Err(Fail::Unsupported(t)) => return Err(unsupported(t)),
        Err(_) => return Err(WRONG_PASSWORD.into()),
    };
    let k = SymKey::from_bytes(&bytes).filter(|_| bytes.len() == 64);
    wipe_bytes(&mut bytes);
    k.ok_or_else(|| unsupported(7))
}

// ---- JSON --------------------------------------------------------------------

/// A key of an object, ignoring case (the identity server writes
/// PascalCase, the API camelCase).
fn get<'a>(v: &'a Value, key: &str) -> Option<&'a Value> {
    let o = v.as_object()?;
    o.get(key).or_else(|| o.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)).filter(|v| !v.is_null())
}

fn get_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    get(v, key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn get_u64(v: &Value, key: &str) -> Option<u64> {
    get(v, key).and_then(|x| x.as_u64().or_else(|| x.as_str().and_then(|s| s.trim().parse().ok())))
}

/// Bitwarden's message in an error answer.
fn message_of(json: &Value) -> String {
    let m = get(json, "ErrorModel")
        .and_then(|e| get_str(e, "Message"))
        .or_else(|| get_str(json, "error_description"))
        .or_else(|| get_str(json, "message"))
        .or_else(|| get_str(json, "error"))
        .unwrap_or("");
    super::exec::first_lines(m, 300)
}

// ---- the password manager ----------------------------------------------------

/// The API key and the master password; wiped on drop.
pub struct Creds {
    pub client_id: String,
    pub client_secret: String,
    pub password: String,
}

impl Drop for Creds {
    fn drop(&mut self) {
        wipe(&mut self.client_secret);
        wipe(&mut self.password);
    }
}

pub struct Request<'a> {
    pub server: &'a str,
    /// An ID, or a name.
    pub item: &'a str,
    /// Empty: `password`.
    pub attribute: &'a str,
    pub username: bool,
    /// The field the user comes from; empty: the login's username.
    pub user_field: &'a str,
}

/// `(value, username)`.
pub async fn read(req: &Request<'_>, creds: &Creds) -> Result<(String, Option<String>), String> {
    let ep = Endpoints::of(req.server).ok_or("el servidor de Bitwarden no es válido")?;
    let client = client_for(&ep)?;
    read_with(&client, &ep, req, creds).await
}

/// Access tokens by (server, client, secret) digest, while they're valid:
/// Bitwarden limits how often one may log in.
static TOKENS: LazyLock<Mutex<HashMap<String, (Instant, String)>>> = LazyLock::new(Mutex::default);

fn token_cache_key(identity: &str, client_id: &str, secret: &str) -> String {
    let mut h = Sha256::new();
    for part in [identity, client_id, secret] {
        h.update((part.len() as u64).to_le_bytes());
        h.update(part.as_bytes());
    }
    super::cloud_auth::hex(&h.finalize())
}

fn cached_token(key: &str) -> Option<String> {
    let mut c = TOKENS.lock().unwrap_or_else(|e| e.into_inner());
    c.retain(|_, (until, t)| {
        let keep = *until > Instant::now();
        if !keep {
            wipe(t);
        }
        keep
    });
    c.get(key).map(|(_, t)| t.clone())
}

fn remember_token(key: String, token: &str, expires_in: Option<u64>) {
    let secs = expires_in.unwrap_or(3600).clamp(120, 3600);
    TOKENS.lock().unwrap_or_else(|e| e.into_inner()).insert(key, (Instant::now() + Duration::from_secs(secs - 60), token.to_string()));
}

fn forget_token(key: &str) {
    if let Some((_, mut t)) = TOKENS.lock().unwrap_or_else(|e| e.into_inner()).remove(key) {
        wipe(&mut t);
    }
}

/// The same device for the same API key, so Bitwarden doesn't list a new
/// one each time.
fn device_id(client_id: &str) -> String {
    let h = Sha256::digest(format!("dbine-bitwarden-device\u{0}{client_id}").as_bytes());
    let x = super::cloud_auth::hex(&h[..16]);
    format!("{}-{}-{}-{}-{}", &x[..8], &x[8..12], &x[12..16], &x[16..20], &x[20..32])
}

/// `POST <identity>/connect/token`: `(access_token, expires_in)`.
async fn identity_token(client: &reqwest::Client, ep: &Endpoints, form: &[(&str, &str)], what: Who) -> Result<(String, Option<u64>, Value), String> {
    let resp = client
        .post(format!("{}/connect/token", ep.identity))
        .header("Accept", "application/json")
        .header("Device-Type", DEVICE_TYPE)
        .form(form)
        .send()
        .await
        .map_err(|e| format!("no se pudo conectar con Bitwarden ({}): {}", origin(&ep.identity), e.without_url()))?;
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status == 200 {
        if let Some(t) = get_str(&json, "access_token") {
            return Ok((t.to_string(), get_u64(&json, "expires_in"), json));
        }
        return Err("Bitwarden no devolvió un token de acceso".into());
    }
    Err(identity_error(status, &json, what))
}

#[derive(Clone, Copy)]
enum Who {
    ApiKey,
    MachineAccount,
}

fn identity_error(status: u16, json: &Value, who: Who) -> String {
    let code = get_str(json, "error").unwrap_or("");
    let msg = message_of(json);
    match (status, code, who) {
        (429, ..) => "Bitwarden no permite tantos inicios de sesión seguidos; probá en unos minutos".into(),
        (_, "invalid_client", Who::ApiKey) => "Bitwarden rechazó la clave de API (client_id o client_secret): revisala en Configuración › Seguridad › Claves".into(),
        (_, "invalid_client", Who::MachineAccount) => "Bitwarden rechazó el token de acceso de la cuenta de máquina (¿se revocó o es de otro servidor?)".into(),
        (_, "invalid_grant" | "unauthorized_client", _) if !msg.is_empty() => format!("Bitwarden rechazó el inicio de sesión: {msg}"),
        (_, "invalid_scope", _) => "Bitwarden rechazó el pedido (¿la clave es de otro tipo de cuenta?)".into(),
        (s, _, _) if msg.is_empty() => format!("Bitwarden respondió {s} al iniciar sesión"),
        (s, _, _) => format!("Bitwarden respondió {s} al iniciar sesión: {msg}"),
    }
}

/// GET an API path with the token; `Err(None)` when the token is no longer
/// valid (the caller logs in again once).
async fn api_get(client: &reqwest::Client, ep: &Endpoints, token: &str, path: &str) -> Result<(u16, Value), Option<String>> {
    let resp = client
        .get(format!("{}{path}", ep.api))
        .header("Accept", "application/json")
        .header("Device-Type", DEVICE_TYPE)
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| Some(format!("no se pudo conectar con Bitwarden ({}): {}", origin(&ep.api), e.without_url())))?;
    let status = resp.status().as_u16();
    if status == 401 {
        return Err(None);
    }
    let text = resp.text().await.unwrap_or_default();
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

async fn api_key_token(client: &reqwest::Client, ep: &Endpoints, creds: &Creds) -> Result<String, String> {
    let key = token_cache_key(&ep.identity, creds.client_id.trim(), creds.client_secret.trim());
    if let Some(t) = cached_token(&key) {
        return Ok(t);
    }
    if creds.client_secret.trim().is_empty() {
        return Err("falta el client_secret de la clave de API de Bitwarden: escribilo en el formulario".into());
    }
    let device = device_id(creds.client_id.trim());
    let form = [
        ("grant_type", "client_credentials"),
        ("scope", "api"),
        ("client_id", creds.client_id.trim()),
        ("client_secret", creds.client_secret.trim()),
        ("deviceType", DEVICE_TYPE),
        ("deviceIdentifier", device.as_str()),
        ("deviceName", "DBine"),
    ];
    let (token, expires, _) = identity_token(client, ep, &form, Who::ApiKey).await?;
    remember_token(key, &token, expires);
    Ok(token)
}

/// The vault (`/sync`), logging in again once if the token expired.
async fn sync(client: &reqwest::Client, ep: &Endpoints, creds: &Creds) -> Result<Value, String> {
    for attempt in 0..2 {
        let token = api_key_token(client, ep, creds).await?;
        match api_get(client, ep, &token, "/sync?excludeDomains=true").await {
            Ok((200, json)) => return Ok(json),
            Ok((status, json)) => {
                let msg = message_of(&json);
                return Err(if msg.is_empty() { format!("Bitwarden respondió {status} al leer la bóveda") } else { format!("Bitwarden respondió {status} al leer la bóveda: {msg}") });
            }
            Err(Some(e)) => return Err(e),
            Err(None) if attempt == 0 => forget_token(&token_cache_key(&ep.identity, creds.client_id.trim(), creds.client_secret.trim())),
            Err(None) => {}
        }
    }
    Err("Bitwarden rechazó el token de acceso".into())
}

/// What opens the user key: the KDF, the salt and the wrapped user key.
/// From the sync's `userDecryption` (current servers); else the profile's
/// key and e-mail with the KDF from `prelogin`.
async fn unlock_data(client: &reqwest::Client, ep: &Endpoints, sync: &Value) -> Result<(Kdf, String, String), String> {
    let profile = get(sync, "profile").ok_or("Bitwarden no devolvió el perfil de la cuenta")?;
    let unlock = get(sync, "userDecryption").and_then(|u| get(u, "masterPasswordUnlock"));
    let email = get_str(profile, "email").map(|e| e.trim().to_lowercase());
    if let Some(u) = unlock {
        let kdf = get(u, "kdf");
        let wrapped = get_str(u, "masterKeyWrappedUserKey").or_else(|| get_str(u, "masterKeyEncryptedUserKey"));
        let salt = get_str(u, "salt").map(str::to_string).or(email.clone());
        if let (Some(k), Some(w), Some(salt)) = (kdf, wrapped, salt) {
            let kdf = Kdf {
                kind: get_u64(k, "kdfType").unwrap_or(0),
                iterations: get_u64(k, "iterations").unwrap_or(0),
                memory: get_u64(k, "memory").unwrap_or(0),
                parallelism: get_u64(k, "parallelism").unwrap_or(0),
            };
            return Ok((kdf, salt, w.to_string()));
        }
    }
    let wrapped = get_str(profile, "key").ok_or("la cuenta de Bitwarden no tiene contraseña maestra (¿inicia sesión con SSO o un conector de claves?): DBine no puede abrir su bóveda")?;
    let email = email.ok_or("Bitwarden no devolvió el correo de la cuenta")?;
    let resp = client
        .post(format!("{}/accounts/prelogin", ep.identity))
        .header("Accept", "application/json")
        .json(&serde_json::json!({ "email": email }))
        .send()
        .await
        .map_err(|e| format!("no se pudo conectar con Bitwarden ({}): {}", origin(&ep.identity), e.without_url()))?;
    let status = resp.status().as_u16();
    let json: Value = serde_json::from_str(&resp.text().await.unwrap_or_default()).unwrap_or(Value::Null);
    if status != 200 {
        return Err(format!("Bitwarden respondió {status} al pedir los parámetros de la cuenta"));
    }
    let kdf = Kdf {
        kind: get_u64(&json, "kdf").unwrap_or(0),
        iterations: get_u64(&json, "kdfIterations").unwrap_or(0),
        memory: get_u64(&json, "kdfMemory").unwrap_or(0),
        parallelism: get_u64(&json, "kdfParallelism").unwrap_or(0),
    };
    Ok((kdf, email, wrapped.to_string()))
}

/// The keys of the vault: the user's, and the organizations' as needed.
struct Keys<'a> {
    user: SymKey,
    profile: &'a Value,
    private: Option<rsa::RsaPrivateKey>,
    orgs: HashMap<String, SymKey>,
}

impl Keys<'_> {
    fn private_key(&mut self) -> Result<&rsa::RsaPrivateKey, String> {
        use rsa::pkcs8::DecodePrivateKey;
        if self.private.is_none() {
            let bad = || "no se pudo descifrar la clave privada de la cuenta de Bitwarden".to_string();
            let wrapped = get_str(self.profile, "privateKey")
                .or_else(|| get(self.profile, "accountKeys").and_then(|a| get(a, "publicKeyEncryptionKeyPair")).and_then(|p| get_str(p, "wrappedPrivateKey")))
                .ok_or_else(bad)?;
            let mut der = decrypt(wrapped, &self.user).map_err(|e| if let Fail::Unsupported(t) = e { unsupported(t) } else { bad() })?;
            let key = rsa::RsaPrivateKey::from_pkcs8_der(&der);
            wipe_bytes(&mut der);
            self.private = Some(key.map_err(|_| bad())?);
        }
        Ok(self.private.as_ref().expect("set above"))
    }

    fn org(&mut self, id: &str) -> Result<SymKey, String> {
        if let Some(k) = self.orgs.get(id) {
            return Ok(k.clone());
        }
        let orgs = get(self.profile, "organizations").and_then(Value::as_array).cloned().unwrap_or_default();
        let wrapped = orgs.iter().find(|o| get_str(o, "id").is_some_and(|i| i.eq_ignore_ascii_case(id))).and_then(|o| get_str(o, "key").map(str::to_string));
        let wrapped = wrapped.ok_or_else(|| format!("la cuenta no tiene la clave de la organización {id} de Bitwarden"))?;
        let bad = || format!("no se pudo descifrar la clave de la organización {id} de Bitwarden");
        let mut raw = rsa_decrypt(&wrapped, self.private_key()?).map_err(|e| if let Fail::Unsupported(t) = e { unsupported(t) } else { bad() })?;
        let key = SymKey::from_bytes(&raw).filter(|_| raw.len() == 64);
        wipe_bytes(&mut raw);
        let key = key.ok_or_else(bad)?;
        self.orgs.insert(id.to_string(), key.clone());
        Ok(key)
    }

    /// The key an item's values are encrypted with.
    fn of_item(&mut self, item: &Value) -> Result<SymKey, String> {
        let base = match get_str(item, "organizationId") {
            Some(org) => self.org(org)?,
            None => self.user.clone(),
        };
        match get_str(item, "key") {
            None => Ok(base),
            Some(k) => {
                let mut raw = decrypt(k, &base).map_err(|e| if let Fail::Unsupported(t) = e { unsupported(t) } else { "no se pudo descifrar la clave del elemento de Bitwarden".into() })?;
                let key = SymKey::from_bytes(&raw).filter(|_| raw.len() == 64);
                wipe_bytes(&mut raw);
                key.ok_or_else(|| "la clave del elemento de Bitwarden no es válida".to_string())
            }
        }
    }
}

/// The vault (`/sync`) and the user key the master password opens.
async fn open_vault(client: &reqwest::Client, ep: &Endpoints, creds: &Creds) -> Result<(Value, SymKey), String> {
    let vault = sync(client, ep, creds).await?;
    let (kdf, salt, wrapped) = unlock_data(client, ep, &vault).await?;
    // The KDF takes its time on purpose: on a blocking thread.
    let mut password = creds.password.clone();
    let user = tokio::task::spawn_blocking(move || {
        let r = user_key(&password, &salt, &kdf, &wrapped);
        wipe(&mut password);
        r
    })
    .await
    .map_err(|e| format!("el descifrado de la bóveda de Bitwarden se interrumpió: {e}"))??;
    Ok((vault, user))
}

/// The account wizard's check: log in with the API key, sync, and open the
/// user key with the master password (a wrong one says so). The account's
/// e-mail.
pub async fn whoami(server: &str, creds: &Creds) -> Result<String, String> {
    let ep = Endpoints::of(server).ok_or("el servidor de Bitwarden no es válido")?;
    let client = client_for(&ep)?;
    whoami_with(&client, &ep, creds).await
}

pub async fn whoami_with(client: &reqwest::Client, ep: &Endpoints, creds: &Creds) -> Result<String, String> {
    if creds.password.is_empty() {
        return Err("falta la contraseña maestra de Bitwarden".into());
    }
    let (mut vault, _user) = open_vault(client, ep, creds).await?;
    let email = get(&vault, "profile").and_then(|p| get_str(p, "email")).unwrap_or("").to_string();
    super::browse::wipe_json(&mut vault);
    Ok(email)
}

/// Drop the access token kept for an API key (its account was removed or
/// reconnected).
pub fn forget_token_of(server: &str, client_id: &str, client_secret: &str) {
    if let Some(ep) = Endpoints::of(server) {
        forget_token(&token_cache_key(&ep.identity, client_id.trim(), client_secret.trim()));
    }
}

/// `(value, username)` of the item, talking to `ep`.
pub async fn read_with(client: &reqwest::Client, ep: &Endpoints, req: &Request<'_>, creds: &Creds) -> Result<(String, Option<String>), String> {
    if creds.password.is_empty() {
        return Err("falta la contraseña maestra de Bitwarden".into());
    }
    let (vault, user) = open_vault(client, ep, creds).await?;
    let profile = get(&vault, "profile").ok_or("Bitwarden no devolvió el perfil de la cuenta")?;
    let mut keys = Keys { user, profile, private: None, orgs: HashMap::new() };
    let items: Vec<&Value> = get(&vault, "ciphers").and_then(Value::as_array).map(|a| a.iter().filter(|c| get(c, "deletedDate").is_none()).collect()).unwrap_or_default();
    let (item, key, name) = find_item(&mut keys, &items, req.item.trim())?;
    let value = field(item, &key, req.attribute, &name)?;
    let user_field = Some(req.user_field.trim()).filter(|f| !f.is_empty()).unwrap_or("username");
    let username = if req.username {
        match field(item, &key, user_field, &name) {
            Ok(u) if !u.is_empty() => Some(u),
            _ => {
                let mut v = value;
                wipe(&mut v);
                return Err(format!("el elemento «{name}» de Bitwarden no tiene usuario"));
            }
        }
    } else {
        None
    };
    Ok((value, username))
}

/// The item by ID, else by its name (exactly, else ignoring case): the item,
/// its key and its name.
fn find_item<'v>(keys: &mut Keys<'_>, items: &[&'v Value], wanted: &str) -> Result<(&'v Value, SymKey, String), String> {
    if is_uuid(wanted) {
        let item = items.iter().find(|c| get_str(c, "id").is_some_and(|i| i.eq_ignore_ascii_case(wanted))).ok_or_else(|| format!("Bitwarden: no hay un elemento con el ID {wanted} (o la cuenta no tiene acceso)"))?;
        let key = keys.of_item(item)?;
        let name = decrypt_text(get(item, "name"), &key).ok().flatten().unwrap_or_else(|| wanted.to_string());
        return Ok((item, key, name));
    }
    let mut named: Vec<(&Value, SymKey, String)> = Vec::new();
    let mut unreadable = 0;
    for item in items {
        let Ok(key) = keys.of_item(item) else {
            unreadable += 1;
            continue;
        };
        match decrypt_text(get(item, "name"), &key) {
            Ok(Some(n)) => named.push((item, key, n)),
            _ => unreadable += 1,
        }
    }
    let exact: Vec<usize> = (0..named.len()).filter(|i| named[*i].2 == wanted).collect();
    let found = if exact.is_empty() { (0..named.len()).filter(|i| named[*i].2.eq_ignore_ascii_case(wanted)).collect() } else { exact };
    match found.len() {
        1 => Ok(named.swap_remove(found[0])),
        0 => {
            let low = wanted.to_lowercase();
            let mut similar: Vec<&str> = named.iter().map(|(_, _, n)| n.as_str()).filter(|n| n.to_lowercase().contains(&low) || low.contains(&n.to_lowercase())).take(8).collect();
            similar.sort_unstable();
            similar.dedup();
            // Whole messages, so the UI can translate each one.
            let similar = similar.join(", ");
            Err(match (similar.is_empty(), unreadable) {
                (true, 0) => format!("Bitwarden: no hay un elemento «{wanted}»"),
                (false, 0) => format!("Bitwarden: no hay un elemento «{wanted}» (parecidos: {similar})"),
                (true, n) => format!("Bitwarden: no hay un elemento «{wanted}» ({n} elementos no se pudieron descifrar)"),
                (false, n) => format!("Bitwarden: no hay un elemento «{wanted}» (parecidos: {similar}; {n} elementos no se pudieron descifrar)"),
            })
        }
        n => {
            let ids: Vec<&str> = found.iter().take(6).map(|i| get_str(named[*i].0, "id").unwrap_or("?")).collect();
            Err(format!("Bitwarden tiene {n} elementos «{wanted}» (IDs: {}): indicá el ID del que corresponde", ids.join(", ")))
        }
    }
}

/// One field of the item: `password` (empty), `username`, `notes`, `totp`,
/// else a custom field by name (exactly, else ignoring case; a linked field
/// gives the login's username or password).
fn field(item: &Value, key: &SymKey, attribute: &str, name: &str) -> Result<String, String> {
    let open = |v: Option<&Value>| decrypt_text(v, key).map_err(|e| if let Fail::Unsupported(t) = e { unsupported(t) } else { format!("no se pudo descifrar un campo del elemento «{name}» de Bitwarden") });
    let login = get(item, "login");
    let standard = |w: &str| -> Option<Option<&Value>> {
        match w.to_ascii_lowercase().as_str() {
            "password" => Some(login.and_then(|l| get(l, "password"))),
            "username" => Some(login.and_then(|l| get(l, "username"))),
            "notes" => Some(get(item, "notes")),
            "totp" => Some(login.and_then(|l| get(l, "totp"))),
            _ => None,
        }
    };
    let want = Some(attribute.trim()).filter(|a| !a.is_empty()).unwrap_or("password");
    if let Some(Some(v)) = standard(want) {
        if let Some(s) = open(Some(v))? {
            return Ok(s);
        }
    }
    // Custom fields: names decrypted to compare.
    let fields: Vec<&Value> = get(item, "fields").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    let mut names: Vec<(String, &Value)> = Vec::new();
    for f in &fields {
        if let Some(n) = open(get(f, "name"))? {
            names.push((n, f));
        }
    }
    let hit = names.iter().find(|(n, _)| n == want).or_else(|| names.iter().find(|(n, _)| n.eq_ignore_ascii_case(want)));
    if let Some((_, f)) = hit {
        if get_u64(f, "type") == Some(3) {
            let linked = match get_u64(f, "linkedId") {
                Some(100) => standard("username"),
                Some(101) => standard("password"),
                _ => None,
            };
            if let Some(Some(s)) = linked.map(&open).transpose()? {
                return Ok(s);
            }
        } else if let Some(s) = open(get(f, "value"))? {
            return Ok(s);
        }
        return Err(format!("el campo «{want}» del elemento «{name}» de Bitwarden está vacío"));
    }
    let mut have: Vec<String> = ["password", "username", "notes", "totp"].iter().filter(|w| matches!(standard(w), Some(Some(_)))).map(|w| w.to_string()).collect();
    have.extend(names.into_iter().map(|(n, _)| n).take(30));
    Err(format!("el elemento «{name}» de Bitwarden no tiene el campo «{want}» (tiene: {})", have.join(", ")))
}

// ---- browse ------------------------------------------------------------------

/// The vault's items (names decrypted here) by folder or organization, or
/// an item's field names (`item`, an ID or a name): the standard ones it
/// has and its custom fields' names. No value is decrypted.
pub async fn browse(server: &str, creds: &Creds, at: super::browse::At<'_>) -> Result<super::browse::Listing, String> {
    let ep = Endpoints::of(server).ok_or("el servidor de Bitwarden no es válido")?;
    let client = client_for(&ep)?;
    browse_with(&client, &ep, creds, at).await
}

pub async fn browse_with(client: &reqwest::Client, ep: &Endpoints, creds: &Creds, at: super::browse::At<'_>) -> Result<super::browse::Listing, String> {
    use super::browse::{unique_names, Entry, Listing};
    if creds.password.is_empty() {
        return Err("falta la contraseña maestra de Bitwarden".into());
    }
    let (mut vault, user) = open_vault(client, ep, creds).await?;
    let out = (|| {
        let profile = get(&vault, "profile").ok_or("Bitwarden no devolvió el perfil de la cuenta")?;
        let mut keys = Keys { user: user.clone(), profile, private: None, orgs: HashMap::new() };
        let items: Vec<&Value> = get(&vault, "ciphers").and_then(Value::as_array).map(|a| a.iter().filter(|c| get(c, "deletedDate").is_none()).collect()).unwrap_or_default();
        if let Some(wanted) = at.item {
            let (item, key, name) = find_item(&mut keys, &items, wanted)?;
            return Ok(Listing::fields(field_names(item, &key, &name)?));
        }
        // Folders (the user's key) and organizations (their names are plain).
        let folders: HashMap<String, String> = get(&vault, "folders")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|f| Some((get_str(f, "id")?.to_lowercase(), decrypt_text(get(f, "name"), &keys.user).ok().flatten()?)))
            .collect();
        let orgs: HashMap<String, String> = get(profile, "organizations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|o| Some((get_str(o, "id")?.to_lowercase(), get_str(o, "name").unwrap_or("").to_string())))
            .collect();
        let mut named: Vec<(String, String, String)> = Vec::new();
        for item in &items {
            let Ok(key) = keys.of_item(item) else { continue };
            let Ok(Some(name)) = decrypt_text(get(item, "name"), &key) else { continue };
            let id = get_str(item, "id").unwrap_or("").to_string();
            let place = match get_str(item, "organizationId") {
                Some(o) => orgs.get(&o.to_lowercase()).cloned().unwrap_or_default(),
                None => get_str(item, "folderId").and_then(|f| folders.get(&f.to_lowercase()).cloned()).unwrap_or_default(),
            };
            named.push((name, id, place));
        }
        let unique = unique_names(named.iter().map(|(n, _, _)| n.as_str()));
        let mut out: Vec<Entry> = named
            .into_iter()
            .map(|(name, id, place)| {
                let by_name = !name.trim().is_empty() && !is_uuid(&name) && unique.contains(&name);
                // Bitwarden nests folders by `/` in their names.
                Entry::item(if by_name { name.clone() } else { id.clone() }, name, place, id)
            })
            .collect();
        out.sort_by_key(|e| (e.path.to_lowercase(), e.name.to_lowercase()));
        Ok(Listing::complete(out))
    })();
    super::browse::wipe_json(&mut vault);
    out
}

/// The names of an item's fields, as [`field`] takes them.
fn field_names(item: &Value, key: &SymKey, name: &str) -> Result<Vec<String>, String> {
    let login = get(item, "login");
    let has = |v: Option<&Value>| matches!(v, Some(Value::String(s)) if !s.is_empty());
    let mut out: Vec<String> = Vec::new();
    for (n, v) in [("password", login.and_then(|l| get(l, "password"))), ("username", login.and_then(|l| get(l, "username"))), ("notes", get(item, "notes")), ("totp", login.and_then(|l| get(l, "totp")))] {
        if has(v) {
            out.push(n.to_string());
        }
    }
    for f in get(item, "fields").and_then(Value::as_array).into_iter().flatten() {
        match decrypt_text(get(f, "name"), key) {
            Ok(Some(n)) if !out.contains(&n) => out.push(n),
            Ok(_) => {}
            Err(Fail::Unsupported(t)) => return Err(unsupported(t)),
            Err(_) => return Err(format!("no se pudo descifrar un campo del elemento «{name}» de Bitwarden")),
        }
    }
    Ok(out)
}

// ---- Secrets Manager ---------------------------------------------------------

/// A machine account's access token, parsed; wiped on drop.
pub struct AccessToken {
    pub id: String,
    secret: String,
    seed: [u8; 16],
}

impl Drop for AccessToken {
    fn drop(&mut self) {
        wipe(&mut self.secret);
        self.seed.iter_mut().for_each(|b| unsafe { std::ptr::write_volatile(b, 0) });
    }
}

const BAD_TOKEN: &str = "el token de acceso de Bitwarden Secrets Manager no tiene el formato esperado (0.<id>.<secreto>:<clave>)";

/// `0.<id>.<client secret>:<base64 of a 16-byte key>`.
pub fn parse_access_token(t: &str) -> Result<AccessToken, String> {
    let (left, key) = t.trim().split_once(':').ok_or(BAD_TOKEN)?;
    let mut parts = left.splitn(3, '.');
    let (Some("0"), Some(id), Some(secret)) = (parts.next(), parts.next(), parts.next()) else {
        return Err(BAD_TOKEN.into());
    };
    if !is_uuid(id) || secret.is_empty() {
        return Err(BAD_TOKEN.into());
    }
    let mut raw = B64.decode(key).map_err(|_| BAD_TOKEN.to_string())?;
    if raw.len() != 16 {
        wipe_bytes(&mut raw);
        return Err(BAD_TOKEN.into());
    }
    let mut seed = [0u8; 16];
    seed.copy_from_slice(&raw);
    wipe_bytes(&mut raw);
    Ok(AccessToken { id: id.to_string(), secret: secret.to_string(), seed })
}

impl AccessToken {
    /// The key of the token's encrypted payload (the SDK's
    /// `derive_shareable_key(seed, "accesstoken", "sm-access-token")`).
    pub fn key(&self) -> SymKey {
        let prk = super::cloud_auth::hmac(b"bitwarden-accesstoken", &self.seed);
        let mut okm = hkdf_expand(&prk, b"sm-access-token", 64);
        let k = SymKey::from_bytes(&okm).expect("64 bytes");
        wipe_bytes(&mut okm);
        k
    }
}

/// Access tokens and their encrypted payloads, by (server, token) digest:
/// `(until, access token, payload)`.
type SmTokens = HashMap<String, (Instant, String, String)>;
static SM_TOKENS: LazyLock<Mutex<SmTokens>> = LazyLock::new(Mutex::default);

/// The secret's value. `token`: typed in the form (the keychain), else
/// `BWS_ACCESS_TOKEN`.
pub async fn read_secret(server: &str, secret_id: &str, token: Option<&str>) -> Result<String, String> {
    let ep = Endpoints::of(server).ok_or("el servidor de Bitwarden no es válido")?;
    let mut t = token
        .map(str::to_string)
        .filter(|t| !t.trim().is_empty())
        .or_else(|| std::env::var("BWS_ACCESS_TOKEN").ok().filter(|t| !t.trim().is_empty()))
        .ok_or("falta el token de acceso de la cuenta de máquina: escribilo en el formulario (o definí BWS_ACCESS_TOKEN)")?;
    let parsed = parse_access_token(&t);
    wipe(&mut t);
    let client = client_for(&ep)?;
    read_secret_with(&client, &ep, secret_id.trim(), &parsed?).await
}

pub async fn read_secret_with(client: &reqwest::Client, ep: &Endpoints, secret_id: &str, token: &AccessToken) -> Result<String, String> {
    match sm_get(client, ep, token, &format!("/secrets/{secret_id}")).await? {
        (200, json, payload, _) => open_secret(&json, &payload, token),
        (404, ..) => Err(format!("Bitwarden Secrets Manager: no hay un secreto con el ID {secret_id} (o la cuenta de máquina no tiene acceso a su proyecto)")),
        (status, json, ..) => {
            let msg = message_of(&json);
            Err(if msg.is_empty() { format!("Bitwarden Secrets Manager respondió {status}") } else { format!("Bitwarden Secrets Manager respondió {status}: {msg}") })
        }
    }
}

/// A GET to Secrets Manager's API with the machine account's access token
/// (cached, or a new one once if it expired): `(status, body, the
/// token's encrypted payload, the access token)`.
async fn sm_get(client: &reqwest::Client, ep: &Endpoints, token: &AccessToken, path: &str) -> Result<(u16, Value, String, String), String> {
    let cache = token_cache_key(&ep.identity, &token.id, &token.secret);
    for attempt in 0..2 {
        let (access, payload) = sm_session(client, ep, token).await?;
        match api_get(client, ep, &access, path).await {
            Ok((status, json)) => return Ok((status, json, payload, access)),
            Err(Some(e)) => return Err(e),
            Err(None) if attempt == 0 => {
                if let Some((_, mut a, _)) = SM_TOKENS.lock().unwrap_or_else(|e| e.into_inner()).remove(&cache) {
                    wipe(&mut a);
                }
            }
            Err(None) => {}
        }
    }
    Err("Bitwarden rechazó el token de acceso de la cuenta de máquina".into())
}

/// The machine account's access token and encrypted payload: cached while
/// valid, else asked for.
async fn sm_session(client: &reqwest::Client, ep: &Endpoints, token: &AccessToken) -> Result<(String, String), String> {
    let cache = token_cache_key(&ep.identity, &token.id, &token.secret);
    let cached = {
        let mut c = SM_TOKENS.lock().unwrap_or_else(|e| e.into_inner());
        c.retain(|_, (until, at, _)| {
            let keep = *until > Instant::now();
            if !keep {
                wipe(at);
            }
            keep
        });
        c.get(&cache).map(|(_, a, p)| (a.clone(), p.clone()))
    };
    if let Some(v) = cached {
        return Ok(v);
    }
    let form = [("grant_type", "client_credentials"), ("scope", "api.secrets"), ("client_id", token.id.as_str()), ("client_secret", token.secret.as_str())];
    let (access, expires, json) = identity_token(client, ep, &form, Who::MachineAccount).await?;
    let payload = get_str(&json, "encrypted_payload").ok_or("Bitwarden no devolvió la clave de la organización con el token")?.to_string();
    let secs = expires.unwrap_or(3600).clamp(120, 3600);
    SM_TOKENS.lock().unwrap_or_else(|e| e.into_inner()).insert(cache, (Instant::now() + Duration::from_secs(secs - 60), access.clone(), payload.clone()));
    Ok((access, payload))
}

/// The organization key in a token's payload.
fn org_key_of(payload: &str, token: &AccessToken) -> Result<SymKey, String> {
    let bad = || "no se pudo descifrar la clave que acompaña al token de Bitwarden Secrets Manager".to_string();
    let mut raw = decrypt(payload, &token.key()).map_err(|e| if let Fail::Unsupported(t) = e { unsupported(t) } else { bad() })?;
    let parsed: Option<Value> = serde_json::from_slice(&raw).ok();
    wipe_bytes(&mut raw);
    let mut key_b64 = parsed.as_ref().and_then(|p| get_str(p, "encryptionKey")).map(str::to_string).ok_or_else(bad)?;
    let key = B64.decode(key_b64.trim());
    wipe(&mut key_b64);
    let mut key = key.map_err(|_| bad())?;
    let org = SymKey::from_bytes(&key).filter(|_| key.len() == 64);
    wipe_bytes(&mut key);
    org.ok_or_else(bad)
}

/// The account wizard's check for a machine account: its access token,
/// the organization it's for and how many secrets it can read (listed,
/// never opened). `(organization, secrets)`.
pub async fn sm_whoami(server: &str, token: &str) -> Result<(String, usize), String> {
    let ep = Endpoints::of(server).ok_or("el servidor de Bitwarden no es válido")?;
    let parsed = parse_access_token(token)?;
    let client = client_for(&ep)?;
    sm_whoami_with(&client, &ep, &parsed).await
}

pub async fn sm_whoami_with(client: &reqwest::Client, ep: &Endpoints, token: &AccessToken) -> Result<(String, usize), String> {
    let (access, _) = sm_session(client, ep, token).await?;
    let org = token_organization(&access).ok_or("Bitwarden no dijo de qué organización es la cuenta de máquina")?;
    let (status, mut json, _, _) = sm_get(client, ep, token, &format!("/organizations/{org}/secrets")).await?;
    if status != 200 {
        let msg = message_of(&json);
        return Err(if msg.is_empty() { format!("Bitwarden Secrets Manager respondió {status}") } else { format!("Bitwarden Secrets Manager respondió {status}: {msg}") });
    }
    let n = get(&json, "secrets").or_else(|| get(&json, "data")).and_then(Value::as_array).map_or(0, Vec::len);
    super::browse::wipe_json(&mut json);
    Ok((org, n))
}

/// Drop the session kept for a machine account's token.
pub fn sm_forget(server: &str, token: &str) {
    let (Some(ep), Ok(t)) = (Endpoints::of(server), parse_access_token(token)) else { return };
    if let Some((_, mut a, _)) = SM_TOKENS.lock().unwrap_or_else(|e| e.into_inner()).remove(&token_cache_key(&ep.identity, &t.id, &t.secret)) {
        wipe(&mut a);
    }
}

/// The organization a machine account's access token (a JWT) is for.
pub fn token_organization(access: &str) -> Option<String> {
    let claims = access.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(claims.trim_end_matches('=')).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    get_str(&v, "organization").filter(|o| is_uuid(o)).map(str::to_string)
}

/// The secrets the machine account can read (their keys decrypted here,
/// never their values), or a secret's JSON keys (`item`, its ID).
pub async fn browse_secrets(server: &str, token: Option<&str>, at: super::browse::At<'_>) -> Result<super::browse::Listing, String> {
    let ep = Endpoints::of(server).ok_or("el servidor de Bitwarden no es válido")?;
    let mut t = token
        .map(str::to_string)
        .filter(|t| !t.trim().is_empty())
        .or_else(|| std::env::var("BWS_ACCESS_TOKEN").ok().filter(|t| !t.trim().is_empty()))
        .ok_or("falta el token de acceso de la cuenta de máquina: escribilo en el formulario (o definí BWS_ACCESS_TOKEN)")?;
    let parsed = parse_access_token(&t);
    wipe(&mut t);
    let client = client_for(&ep)?;
    browse_secrets_with(&client, &ep, &parsed?, at).await
}

pub async fn browse_secrets_with(client: &reqwest::Client, ep: &Endpoints, token: &AccessToken, at: super::browse::At<'_>) -> Result<super::browse::Listing, String> {
    use super::browse::{Entry, Listing};
    if let Some(item) = at.item {
        let raw = read_secret_with(client, ep, item.trim(), token).await?;
        return Ok(Listing::fields(super::browse::json_keys(raw)));
    }
    // The organization is in the access token.
    let (access, _) = sm_session(client, ep, token).await?;
    let org = token_organization(&access).ok_or("Bitwarden no dijo de qué organización es la cuenta de máquina")?;
    let (status, mut json, payload, _) = sm_get(client, ep, token, &format!("/organizations/{org}/secrets")).await?;
    if status != 200 {
        let msg = message_of(&json);
        return Err(if msg.is_empty() { format!("Bitwarden Secrets Manager respondió {status}") } else { format!("Bitwarden Secrets Manager respondió {status}: {msg}") });
    }
    let key = org_key_of(&payload, token)?;
    let mut out: Vec<Entry> = get(&json, "secrets")
        .or_else(|| get(&json, "data"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| {
            let id = get_str(s, "id")?.to_string();
            let name = decrypt_text(get(s, "key"), &key).ok().flatten().unwrap_or_else(|| id.clone());
            Some(Entry::item(id.clone(), name, "", id))
        })
        .collect();
    super::browse::wipe_json(&mut json);
    out.sort_by_key(|e| e.name.to_lowercase());
    Ok(Listing::complete(out))
}

/// The organization key (the token's payload), then the secret's value.
pub fn open_secret(secret: &Value, payload: &str, token: &AccessToken) -> Result<String, String> {
    let org = org_key_of(payload, token)?;
    match decrypt_text(get(secret, "value"), &org) {
        Ok(Some(v)) => Ok(v),
        Ok(None) => Err("el secreto de Bitwarden Secrets Manager no tiene valor".into()),
        Err(Fail::Unsupported(t)) => Err(unsupported(t)),
        Err(_) => Err("no se pudo descifrar el secreto de Bitwarden Secrets Manager (¿es de otra organización que la cuenta de máquina?)".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// testdata/bitwarden-vectors.mjs: Secrets Manager values the official
    /// SDK decrypted back, KDF vectors from Node's crypto.
    fn vectors() -> Value {
        serde_json::from_str(include_str!("testdata/bitwarden-vectors.json")).unwrap()
    }

    /// testdata/bitwarden-setup.mjs: a vault filled by the official CLI on
    /// vaultwarden, as DBine's API key login and sync got it.
    fn recorded() -> Value {
        serde_json::from_str(include_str!("testdata/bitwarden-recorded.json")).unwrap()
    }

    fn s<'a>(v: &'a Value, k: &str) -> &'a str {
        v[k].as_str().unwrap()
    }

    /// EncString type 2 with the given IV (to build test data).
    fn encrypt(plain: &[u8], key: &SymKey, iv: [u8; 16]) -> String {
        use aes::cipher::BlockEncrypt;
        let cipher = Aes256::new(GenericArray::from_slice(&key.enc));
        let pad = 16 - plain.len() % 16;
        let mut data = plain.to_vec();
        data.extend(std::iter::repeat_n(pad as u8, pad));
        let mut prev = iv;
        for chunk in data.chunks_mut(16) {
            for (b, p) in chunk.iter_mut().zip(prev.iter()) {
                *b ^= p;
            }
            cipher.encrypt_block(GenericArray::from_mut_slice(chunk));
            prev.copy_from_slice(chunk);
        }
        let mac = hmac_parts(key.mac.as_ref().unwrap(), &[&iv, &data]);
        format!("2.{}|{}|{}", B64.encode(iv), B64.encode(&data), B64.encode(mac))
    }

    #[test]
    fn key_derivation_vectors() {
        let v = &vectors()["vault"];
        let p = &v["pbkdf2"];
        let kdf = Kdf { kind: 0, iterations: p["iterations"].as_u64().unwrap(), memory: 0, parallelism: 0 };
        let m = master_key(s(p, "password"), s(p, "salt"), &kdf).unwrap();
        assert_eq!(super::super::cloud_auth::hex(&m), s(p, "master"));
        let st = stretch(&m);
        assert_eq!(B64.encode([st.enc.as_slice(), st.mac.unwrap().as_slice()].concat()), s(p, "stretched"));
        let a = &v["argon2id"];
        let kdf = Kdf { kind: 1, iterations: a["iterations"].as_u64().unwrap(), memory: a["memory"].as_u64().unwrap(), parallelism: a["parallelism"].as_u64().unwrap() };
        assert_eq!(super::super::cloud_auth::hex(&master_key(s(a, "password"), s(a, "salt"), &kdf).unwrap()), s(a, "master"));
        // The user key: right password, wrong password.
        let kdf = Kdf { kind: 0, iterations: 5000, memory: 0, parallelism: 0 };
        let uk = user_key(s(p, "password"), s(p, "salt"), &kdf, s(v, "protected_user_key")).unwrap();
        assert_eq!(B64.encode([uk.enc.as_slice(), uk.mac.unwrap().as_slice()].concat()), s(v, "user_key"));
        assert_eq!(user_key("nope", s(p, "salt"), &kdf, s(v, "protected_user_key")).err().unwrap(), WRONG_PASSWORD);
        // Out of range: refused before deriving anything.
        for bad in [Kdf { kind: 0, iterations: 10, memory: 0, parallelism: 0 }, Kdf { kind: 0, iterations: 50_000_000, memory: 0, parallelism: 0 }, Kdf { kind: 1, iterations: 3, memory: 100_000, parallelism: 4 }, Kdf { kind: 9, iterations: 3, memory: 64, parallelism: 4 }] {
            assert!(master_key("x", "y", &bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn enc_strings_are_authenticated() {
        let key = SymKey::from_bytes(&[7u8; 64]).unwrap();
        let e = encrypt(b"hello", &key, [1; 16]);
        assert_eq!(decrypt(&e, &key).unwrap(), b"hello");
        // A flipped bit of the ciphertext: the MAC catches it.
        let mut parts: Vec<String> = e[2..].split('|').map(str::to_string).collect();
        let mut ct = B64.decode(&parts[1]).unwrap();
        ct[0] ^= 1;
        parts[1] = B64.encode(&ct);
        assert_eq!(decrypt(&format!("2.{}", parts.join("|")), &key), Err(Fail::Mac));
        // Another key.
        assert_eq!(decrypt(&e, &SymKey::from_bytes(&[8u8; 64]).unwrap()), Err(Fail::Mac));
        // A MAC'd key never takes an unauthenticated (type 0) value.
        let iv_ct = e[2..].rsplit_once('|').unwrap().0.to_string();
        assert_eq!(decrypt(&format!("0.{iv_ct}"), &key), Err(Fail::Format));
        assert_eq!(decrypt(&iv_ct, &key), Err(Fail::Format), "untyped two parts are type 0");
        assert_eq!(decrypt("7.abc", &key), Err(Fail::Unsupported(7)));
        assert!(unsupported(7).contains("v2"));
        assert_eq!(decrypt("2.!!|!!|!!", &key), Err(Fail::Format));
    }

    #[test]
    fn an_item_with_its_own_key() {
        let v = &vectors()["vault"];
        let user = SymKey::from_bytes(&B64.decode(s(v, "user_key")).unwrap()).unwrap();
        let profile = json!({ "organizations": [] });
        let mut keys = Keys { user, profile: &profile, private: None, orgs: HashMap::new() };
        let item = &v["item_with_cipher_key"];
        let items = vec![item];
        let (found, key, name) = find_item(&mut keys, &items, "with-cipher-key").unwrap();
        assert_eq!(name, "with-cipher-key");
        assert_eq!(field(found, &key, "", &name).unwrap(), "ck-secret");
        assert_eq!(field(found, &key, "username", &name).unwrap(), "ck_user");
        assert_eq!(field(found, &key, "linked PASSWORD", &name).unwrap(), "ck-secret", "a linked field, ignoring case");
        let e = field(found, &key, "pin", &name).unwrap_err();
        assert!(e.contains("no tiene el campo «pin»") && e.contains("password") && e.contains("Linked password") && !e.contains("ck-secret"), "{e}");
    }

    #[test]
    fn secrets_manager_vectors() {
        let v = vectors();
        let t = parse_access_token(s(&v, "access_token")).unwrap();
        let k = t.key();
        assert_eq!(B64.encode([k.enc.as_slice(), k.mac.unwrap().as_slice()].concat()), s(&v, "token_key"));
        let payload = s(&v["token_response"], "encrypted_payload");
        for (enc, plain) in v["secrets"].as_array().unwrap().iter().zip(v["plain"].as_array().unwrap()) {
            assert_eq!(open_secret(enc, payload, &t).unwrap(), s(plain, "value"));
        }
        // Another token's key doesn't open the payload.
        let other = parse_access_token("0.7b1d1c26-3f5e-4c8a-9a0b-2f3e4d5c6b7a.x:AAAAAAAAAAAAAAAAAAAAAA==").unwrap();
        let e = open_secret(&v["secrets"][0], payload, &other).unwrap_err();
        assert!(e.contains("no se pudo descifrar la clave"), "{e}");
        for bad in ["", "0.x.y:AAAAAAAAAAAAAAAAAAAAAA==", "1.7b1d1c26-3f5e-4c8a-9a0b-2f3e4d5c6b7a.s:AAAAAAAAAAAAAAAAAAAAAA==", "0.7b1d1c26-3f5e-4c8a-9a0b-2f3e4d5c6b7a.s:AAAA", "0.7b1d1c26-3f5e-4c8a-9a0b-2f3e4d5c6b7a.s"] {
            let e = parse_access_token(bad).err().unwrap();
            assert_eq!(e, BAD_TOKEN, "{bad}");
        }
        assert!(!BAD_TOKEN.contains("dummyClientSecret"));
    }

    #[test]
    fn servers_and_devices() {
        let ep = Endpoints::of("bitwarden.eu").unwrap();
        assert_eq!((ep.identity.as_str(), ep.api.as_str()), ("https://identity.bitwarden.eu", "https://api.bitwarden.eu"));
        let ep = Endpoints::of("https://vw.example.com/bw/").unwrap();
        assert_eq!((ep.identity.as_str(), ep.api.as_str()), ("https://vw.example.com/bw/identity", "https://vw.example.com/bw/api"));
        assert!(Endpoints::of("http://vw.example.com").is_none());
        assert_eq!(origin("https://vw.example.com:8443/bw/identity"), "vw.example.com:8443");
        let d = device_id("user.6a2f40a5-d692-4c8e-953c-5271ccb77291");
        assert!(is_uuid(&d) && d == device_id("user.6a2f40a5-d692-4c8e-953c-5271ccb77291") && d != device_id("user.other"));
    }

    // ---- against a mock of the API -------------------------------------------

    use axum::extract::{Path as AxPath, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get as aget, post};
    use axum::{Form, Json, Router};
    use std::sync::Arc;

    #[derive(Default)]
    struct Mock {
        logins: Mutex<u32>,
        /// Answer the next API call with 401.
        expire_once: Mutex<bool>,
        /// The sync without `userDecryption` (older servers): `prelogin`.
        old_server: bool,
        prelogins: Mutex<u32>,
    }

    fn bearer_ok(h: &HeaderMap) -> bool {
        h.get("authorization").and_then(|v| v.to_str().ok()).is_some_and(|v| v == "Bearer at-vault" || v.starts_with("Bearer eyJ"))
    }

    async fn mock_server(old_server: bool) -> (String, Arc<Mock>) {
        let state = Arc::new(Mock { old_server, ..Default::default() });
        let rec = recorded();
        let vec = vectors();
        let app = Router::new()
            .route(
                "/identity/connect/token",
                post(move |State(m): State<Arc<Mock>>, Form(f): Form<HashMap<String, String>>| {
                    let rec = recorded();
                    let vec = vectors();
                    async move {
                        *m.logins.lock().unwrap() += 1;
                        let g = |k: &str| f.get(k).map(String::as_str).unwrap_or("");
                        if g("grant_type") != "client_credentials" {
                            return (StatusCode::BAD_REQUEST, Json(json!({ "error": "unsupported_grant_type" })));
                        }
                        match g("scope") {
                            "api" if g("client_id") == s(&rec, "client_id") && g("client_secret") == "dummy-api-secret" && !g("deviceIdentifier").is_empty() => {
                                let mut t = rec["token_response"].clone();
                                t["access_token"] = json!("at-vault");
                                (StatusCode::OK, Json(t))
                            }
                            "api.secrets" if g("client_id") == "7b1d1c26-3f5e-4c8a-9a0b-2f3e4d5c6b7a" && g("client_secret") == "dummyClientSecretAbCdEf0123456789xy" => (StatusCode::OK, Json(vec["token_response"].clone())),
                            _ => (StatusCode::BAD_REQUEST, Json(json!({ "error": "invalid_client", "error_description": "invalid_client" }))),
                        }
                    }
                }),
            )
            .route(
                "/identity/accounts/prelogin",
                post(|State(m): State<Arc<Mock>>, Json(b): Json<Value>| async move {
                    *m.prelogins.lock().unwrap() += 1;
                    assert_eq!(b["email"], "pbkdf2@dbine.test");
                    Json(json!({ "kdf": 0, "kdfIterations": 100000, "kdfMemory": null, "kdfParallelism": null }))
                }),
            )
            .route(
                "/api/sync",
                aget(move |State(m): State<Arc<Mock>>, h: HeaderMap| {
                    let mut sync = rec["sync"].clone();
                    async move {
                        if std::mem::take(&mut *m.expire_once.lock().unwrap()) || !bearer_ok(&h) {
                            return (StatusCode::UNAUTHORIZED, Json(Value::Null));
                        }
                        if m.old_server {
                            sync.as_object_mut().unwrap().remove("userDecryption");
                        }
                        (StatusCode::OK, Json(sync))
                    }
                }),
            )
            .route(
                "/api/organizations/{org}/secrets",
                aget(move |h: HeaderMap, AxPath(org): AxPath<String>| {
                    let v = vectors();
                    async move {
                        if !bearer_ok(&h) || org != s(&v, "organization_id") {
                            return (StatusCode::NOT_FOUND, Json(json!({ "message": "Resource not found." })));
                        }
                        let list: Vec<Value> = v["secrets"].as_array().unwrap().iter().map(|x| json!({ "id": x["id"], "organizationId": x["organizationId"], "key": x["key"] })).collect();
                        (StatusCode::OK, Json(json!({ "secrets": list, "projects": [] })))
                    }
                }),
            )
            .route(
                "/api/secrets/{id}",
                aget(move |State(m): State<Arc<Mock>>, h: HeaderMap, AxPath(id): AxPath<String>| {
                    let vec = vec.clone();
                    async move {
                        if std::mem::take(&mut *m.expire_once.lock().unwrap()) || !bearer_ok(&h) {
                            return (StatusCode::UNAUTHORIZED, Json(Value::Null));
                        }
                        match vec["secrets"].as_array().unwrap().iter().find(|x| x["id"] == id.as_str()) {
                            Some(x) => (StatusCode::OK, Json(x.clone())),
                            None => (StatusCode::NOT_FOUND, Json(json!({ "message": "Resource not found." }))),
                        }
                    }
                }),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}"), state)
    }

    fn endpoints(base: &str) -> Endpoints {
        Endpoints::of(base).unwrap()
    }

    fn creds(password: &str, secret: &str) -> Creds {
        Creds { client_id: s(&recorded(), "client_id").into(), client_secret: secret.into(), password: password.into() }
    }

    /// The vault the official CLI filled (recorded from vaultwarden), served
    /// by a mock: by name and ID, fields, notes, an organization's item,
    /// ambiguity, wrong password, token expiry and reuse, an older server.
    #[tokio::test]
    async fn a_recorded_vault() {
        let (base, mock) = mock_server(false).await;
        let ep = endpoints(&base);
        let client = client_for(&ep).unwrap();
        let rec = recorded();
        let pw = s(&rec, "password").to_string();
        // Each test its own secret: the token cache is keyed by it.
        let c = creds(&pw, "dummy-api-secret");
        let r = |item: &'static str, attribute: &'static str, username: bool| Request { server: "", item, attribute, username, user_field: "" };
        assert_eq!(read_with(&client, &ep, &r("dbine-test-postgres", "", true), &c).await.unwrap(), ("pw".into(), Some("postgres".into())));
        assert_eq!(read_with(&client, &ep, &r("DBINE-test-postgres", "port", false), &c).await.unwrap().0, "5432", "a custom field; the name ignoring case");
        assert_eq!(read_with(&client, &ep, &r("dbine-test-postgres", "API token", false), &c).await.unwrap().0, "hidden-token-1", "a hidden field");
        let notes = read_with(&client, &ep, &r("dbine-test-postgres", "notes", false), &c).await.unwrap().0;
        assert_eq!(dbine_core::secret_sources::pick_json(&notes, "password").unwrap(), "pw");
        let custom_user = Request { server: "", item: "dbine-test-postgres", attribute: "", username: true, user_field: "port" };
        assert_eq!(read_with(&client, &ep, &custom_user, &c).await.unwrap(), ("pw".into(), Some("5432".into())), "the user from a custom field");
        let main = rec["items"]["main"].as_str().unwrap();
        let by_id = Request { server: "", item: main, attribute: "username", username: false, user_field: "" };
        assert_eq!(read_with(&client, &ep, &by_id, &c).await.unwrap().0, "postgres", "by ID");
        assert_eq!(read_with(&client, &ep, &r("org-db", "", true), &c).await.unwrap(), ("org-secret-9".into(), Some("org_user".into())), "an organization's item");
        assert_eq!(read_with(&client, &ep, &r("pg notes", "notes", false), &c).await.unwrap().0, r#"{"password":"pw-from-note"}"#, "a secure note");
        assert_eq!(*mock.logins.lock().unwrap(), 1, "one login for all of them");
        let e = read_with(&client, &ep, &r("twin", "", false), &c).await.unwrap_err();
        assert!(e.contains("2 elementos «twin»") && e.contains(rec["items"]["twin1"].as_str().unwrap()) && !e.contains("twin-1"), "{e}");
        let e = read_with(&client, &ep, &r("dbine-test", "", false), &c).await.unwrap_err();
        assert!(e.contains("no hay un elemento «dbine-test»") && e.contains("dbine-test-postgres"), "{e}");
        let e = read_with(&client, &ep, &r("pg notes", "", true), &c).await.unwrap_err();
        assert!(e.contains("no tiene el campo «password»") && e.contains("notes"), "{e}");
        let e = read_with(&client, &ep, &r("00000000-0000-4000-8000-000000000000", "", false), &c).await.unwrap_err();
        assert!(e.contains("no hay un elemento con el ID"), "{e}");
        let e = read_with(&client, &ep, &r("dbine-test-postgres", "", false), &creds("wrong", "dummy-api-secret")).await.unwrap_err();
        assert_eq!(e, WRONG_PASSWORD);
        // The token expired: a new login, once.
        *mock.expire_once.lock().unwrap() = true;
        assert_eq!(read_with(&client, &ep, &r("dbine-test-postgres", "", false), &c).await.unwrap().0, "pw");
        assert_eq!(*mock.logins.lock().unwrap(), 2);
        // A wrong API key: Bitwarden's answer, never the values.
        let e = read_with(&client, &ep, &r("dbine-test-postgres", "", false), &creds(&pw, "nope-secret")).await.unwrap_err();
        assert!(e.contains("clave de API") && !e.contains("nope-secret"), "{e}");
        forget_token(&token_cache_key(&ep.identity, s(&rec, "client_id"), "dummy-api-secret"));
    }

    /// Browsing the recorded vault: names decrypted (a repeated one by its
    /// ID), an organization's item under its name, an item's field names;
    /// never a value.
    #[tokio::test]
    async fn browse_the_recorded_vault() {
        use crate::secret_sources::browse::At;
        let (base, _) = mock_server(false).await;
        let ep = endpoints(&base);
        let client = client_for(&ep).unwrap();
        let rec = recorded();
        let c = creds(s(&rec, "password"), "dummy-api-secret");
        let l = browse_with(&client, &ep, &c, At::default()).await.unwrap();
        let find = |n: &str| l.entries.iter().filter(|e| e.name == n).collect::<Vec<_>>();
        assert_eq!(find("dbine-test-postgres")[0].id, "dbine-test-postgres");
        assert_eq!(find("dbine-test-postgres")[0].detail, rec["items"]["main"].as_str().unwrap());
        let twins: Vec<&str> = l.entries.iter().filter(|e| e.name == "twin").map(|e| e.id.as_str()).collect();
        assert_eq!(twins.len(), 2);
        assert!(twins.iter().all(|t| is_uuid(t)), "a repeated name goes by ID: {twins:?}");
        assert!(!find("org-db")[0].path.is_empty(), "under its organization");
        let json = serde_json::to_string(&l).unwrap();
        assert!(!json.contains("\"pw\"") && !json.contains("org-secret-9"), "{json}");
        let l = browse_with(&client, &ep, &c, At { item: Some("dbine-test-postgres"), ..Default::default() }).await.unwrap();
        let f = l.fields.unwrap();
        assert!(["password", "username", "notes", "Port", "API token"].iter().all(|n| f.iter().any(|x| x == n)), "{f:?}");
        assert_eq!(browse_with(&client, &ep, &creds("wrong", "dummy-api-secret"), At::default()).await.unwrap_err(), WRONG_PASSWORD);
        forget_token(&token_cache_key(&ep.identity, s(&rec, "client_id"), "dummy-api-secret"));
    }

    #[tokio::test]
    async fn an_older_server_asks_prelogin() {
        let (base, mock) = mock_server(true).await;
        let ep = endpoints(&base);
        let client = client_for(&ep).unwrap();
        // Its own server (port): its own cached token.
        let c = creds(s(&recorded(), "password"), "dummy-api-secret");
        let req = Request { server: "", item: "dbine-test-postgres", attribute: "", username: false, user_field: "" };
        assert_eq!(read_with(&client, &ep, &req, &c).await.unwrap().0, "pw");
        assert_eq!(*mock.prelogins.lock().unwrap(), 1);
        forget_token(&token_cache_key(&ep.identity, &c.client_id, &c.client_secret));
    }

    #[tokio::test]
    async fn secrets_manager_against_a_mock() {
        let (base, mock) = mock_server(false).await;
        let ep = endpoints(&base);
        let client = client_for(&ep).unwrap();
        let v = vectors();
        let t = parse_access_token(s(&v, "access_token")).unwrap();
        let id0 = s(&v["plain"][0], "id");
        let raw = read_secret_with(&client, &ep, id0, &t).await.unwrap();
        assert_eq!(dbine_core::secret_sources::pick_json(&raw, "password").unwrap(), "pw");
        assert_eq!(read_secret_with(&client, &ep, s(&v["plain"][1], "id"), &t).await.unwrap(), s(&v["plain"][1], "value"));
        assert_eq!(*mock.logins.lock().unwrap(), 1, "the token is reused");
        *mock.expire_once.lock().unwrap() = true;
        assert!(read_secret_with(&client, &ep, id0, &t).await.is_ok());
        assert_eq!(*mock.logins.lock().unwrap(), 2, "expired: a new one, once");
        let e = read_secret_with(&client, &ep, "00000000-0000-4000-8000-000000000000", &t).await.unwrap_err();
        assert!(e.contains("no hay un secreto con el ID"), "{e}");
        // Browsing: the secrets' keys (names), then one's JSON keys.
        use crate::secret_sources::browse::At;
        let l = browse_secrets_with(&client, &ep, &t, At::default()).await.unwrap();
        assert!(l.entries.iter().any(|e| e.id == id0), "{:?}", l.entries);
        assert!(l.entries.iter().all(|e| !e.name.is_empty() && e.name != e.id), "keys decrypted: {:?}", l.entries);
        let l = browse_secrets_with(&client, &ep, &t, At { item: Some(id0), ..Default::default() }).await.unwrap();
        assert!(l.fields.unwrap().contains(&"password".to_string()));
        let wrong = parse_access_token("0.7b1d1c26-3f5e-4c8a-9a0b-2f3e4d5c6b7a.revoked-secret:AAECAwQFBgcICQoLDA0ODw==").unwrap();
        let e = read_secret_with(&client, &ep, id0, &wrong).await.unwrap_err();
        assert!(e.contains("cuenta de máquina") && !e.contains("revoked-secret"), "{e}");
    }

    /// A self-hosted HTTPS server is HTTPS only: a plain HTTP answer at that
    /// address is refused.
    #[tokio::test]
    async fn production_client_is_https_only() {
        let (base, _) = mock_server(false).await;
        let ep = Endpoints::of(&base.replace("http://", "https://")).unwrap();
        let client = client_for(&ep).unwrap();
        let ep_http = Endpoints { identity: format!("{base}/identity"), api: format!("{base}/api") };
        let e = read_with(&client, &ep_http, &Request { server: "", item: "x", attribute: "", username: false, user_field: "" }, &creds("x", "dummy-api-secret-https")).await.unwrap_err();
        assert!(e.starts_with("no se pudo conectar con Bitwarden"), "{e}");
    }

    /// Against `dbine-test-vaultwarden`, filled by bitwarden-setup.mjs (the
    /// official CLI created the items), through `fill` as a connection does,
    /// then (with `DBINE_TEST_PG_SECRETS`) a real session on
    /// `dbine-test-postgres` with the password and user from Bitwarden:
    /// `DBINE_TEST_BITWARDEN=live.json cargo test -p dbine secret_sources::bitwarden -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_vaultwarden() {
        use dbine_core::secret_sources::{self as defs, MasterSource, SecretSource, SecretSources};
        let Ok(path) = std::env::var("DBINE_TEST_BITWARDEN") else { return };
        let _ = rustls::crypto::ring::default_provider().install_default();
        let live: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let dir = super::super::tests::temp_dir("bw-live");
        let store = dbine_core::StateStore::open(&dir.join("s.sqlite")).unwrap();
        for u in live["users"].as_array().unwrap() {
            let bw = |item: &str, attribute: &str, json_key: &str, master: MasterSource, fill_username: bool| SecretSource::Bitwarden {
                server: s(&live, "server").into(),
                item: item.into(),
                attribute: attribute.into(),
                json_key: json_key.into(),
                client_id: s(u, "client_id").into(),
                master,
                master_command: String::new(),
                fill_username,
                user_field: String::new(),
                secret_account: String::new(),
            };
            let src = bw("dbine-test-postgres", "", "", MasterSource::Keychain, true);
            let tokens: dbine_core::secrets::Secrets = [(defs::token_key("password", ""), s(u, "password").to_string()), (defs::token_key("password", "client_secret"), s(u, "client_secret").to_string())].into();
            let mut cfg = dbine_driver::ConnectionConfig { driver: "postgres".into(), host: "127.0.0.1".into(), port: 25010, database: "postgres".into(), username: Some("someone-else".into()), ..Default::default() };
            let sources: SecretSources = [("password".to_string(), src)].into();
            super::super::fill(&store, &mut cfg, &sources, &tokens, super::super::Origin::Form { id: None, duplicate_of: None, name: "" }).await.unwrap_or_else(|e| panic!("{}: {e}", s(u, "email")));
            assert_eq!((cfg.password.as_deref(), cfg.username.as_deref()), (Some("pw"), Some("postgres")), "{}", s(u, "email"));
            // The notes as JSON, and the organization's item.
            let notes = bw("dbine-test-postgres", "notes", "password", MasterSource::Keychain, false);
            let r = super::super::fetch(&notes, &super::super::field_tokens(&tokens, "password")).await.unwrap();
            assert_eq!(r.value, "pw");
            if !u["organization_id"].is_null() {
                let org = bw("org-db", "", "", MasterSource::Keychain, false);
                assert_eq!(super::super::fetch(&org, &super::super::field_tokens(&tokens, "password")).await.unwrap().value, "org-secret-9");
            }
            // A wrong master password, asked: the UI is asked to ask again.
            let asked = bw("dbine-test-postgres", "", "", MasterSource::Ask, true);
            let wrong: dbine_core::secrets::Secrets = [(defs::token_key("password", ""), "nope".to_string()), (defs::token_key("password", "client_secret"), s(u, "client_secret").to_string())].into();
            let e = super::super::fill(&store, &mut cfg, &[("password".to_string(), asked)].into(), &wrong, super::super::Origin::Form { id: None, duplicate_of: None, name: "" }).await.unwrap_err();
            assert!(matches!(&e, crate::error::CommandError::SecretSourceUnlock(u) if u.wrong && u.kind == "bitwarden"), "{e:?}");
            if std::env::var("DBINE_TEST_PG_SECRETS").is_ok() {
                let mut sess = dbine_drivers::open_session(&cfg, None).await.unwrap_or_else(|e| panic!("connect: {e}"));
                eprintln!("bitwarden ({}): connected, {}", s(u, "email"), sess.server_version().await.unwrap());
            }
        }
    }

    /// The account wizards' checks against the recorded vault: the API key
    /// and master password open it (the account's e-mail), a wrong password
    /// says so; a machine account's token lists its organization's secrets.
    #[tokio::test]
    async fn the_account_wizard_checks() {
        let (base, _) = mock_server(false).await;
        let ep = endpoints(&base);
        let client = client_for(&ep).unwrap();
        let rec = recorded();
        assert_eq!(whoami_with(&client, &ep, &creds(s(&rec, "password"), "dummy-api-secret")).await.unwrap(), "pbkdf2@dbine.test");
        assert_eq!(whoami_with(&client, &ep, &creds("wrong", "dummy-api-secret")).await.unwrap_err(), WRONG_PASSWORD);
        let e = whoami_with(&client, &ep, &creds("x", "nope-secret")).await.unwrap_err();
        assert!(e.contains("clave de API") && !e.contains("nope-secret"), "{e}");
        assert!(whoami_with(&client, &ep, &creds("", "dummy-api-secret")).await.is_err(), "no password");
        forget_token_of(&base, s(&rec, "client_id"), "dummy-api-secret");
        let v = vectors();
        let t = parse_access_token(s(&v, "access_token")).unwrap();
        assert_eq!(sm_whoami_with(&client, &ep, &t).await.unwrap(), (s(&v, "organization_id").to_string(), 2));
        let wrong = parse_access_token("0.7b1d1c26-3f5e-4c8a-9a0b-2f3e4d5c6b7a.revoked-secret:AAECAwQFBgcICQoLDA0ODw==").unwrap();
        let e = sm_whoami_with(&client, &ep, &wrong).await.unwrap_err();
        assert!(e.contains("cuenta de máquina") && !e.contains("revoked-secret"), "{e}");
        sm_forget(&base, s(&v, "access_token"));
    }
}
