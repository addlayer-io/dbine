//! Files from the web and cloud storage in "DataFrame SQL" cells
//! (docs/notebooks.md › Reading files from the web and cloud storage):
//! `read_parquet('s3://ventas/2026/*.parquet')`,
//! `read_csv('https://example.com/datos.csv')`, `gs://…`, `az://…`.
//!
//! - **No DuckDB session ever reaches the network.** DuckDB's `httpfs`
//!   follows redirects to any host, resolves names itself (so a public
//!   name that resolves to `127.0.0.1` or `169.254.169.254` can't be
//!   checked) and would need the credentials inside DuckDB. So DBine
//!   fetches each object itself ([`download`]) into a private folder in the
//!   temp folder (`0700`), the reader session reads those local copies as it
//!   reads any local file (granted exactly them, then locked down), and the
//!   folder is removed right after. That is the one exception to "nothing
//!   is written to disk", and the copies never outlive the read.
//! - **Which URLs** ([`parse`]): `https://` (no globs), `s3://`, `gs://`
//!   (`gcs://`) and `az://` / `abfss://` (Azure Blob Storage, ADLS Gen2),
//!   whose object keys may be globs. `http://` is refused (unencrypted), as
//!   is any other scheme. A URL with a user or password, or with a signed
//!   query (`X-Amz-Signature`, `sig=`, `token=`…) is refused: credentials
//!   never go in the notebook or the cell's SQL.
//! - **Which hosts** ([`connect`]): the name is resolved here and every
//!   address checked ([`ip_blocked`]): loopback, private, link-local (the
//!   cloud metadata service `169.254.169.254` too), unique-local IPv6,
//!   CGNAT, multicast and reserved ranges are refused, as are `localhost`
//!   and other local names, before connecting. The connection is pinned to
//!   the checked addresses (no second lookup), with no proxy. A redirect is
//!   followed only to the same scheme, host and port (at most
//!   [`MAX_REDIRECTS`]); to anywhere else the read is refused.
//! - **Credentials** come from the machine's sign-ins, never from the
//!   notebook: S3 with the AWS chain (environment, the profile of
//!   Settings › Notebooks or `AWS_PROFILE`, SSO and roles through the AWS
//!   CLI: `cloud_auth`), signed here with SigV4; Google Cloud Storage with
//!   gcloud / the application default credentials; Azure with the Azure
//!   CLI chain. Without a sign-in a public object is read anonymously; one
//!   that needs it says how to sign in.
//! - **Limits:** [`MAX_OBJECT_BYTES`] per object, [`MAX_CALL_BYTES`] per
//!   reader call, the same [`super::files`] file count, timeouts on
//!   connecting and on each read; a cancel of the run stops the download.
//! - **Change notice:** an object's `ETag` (else its `Last-Modified` and
//!   size) is its version: nothing is hashed.

use super::files::{has_glob, wild, DataFile};
use crate::error::{CommandError, CommandResult};
use crate::secret_sources::cloud_auth::{
    self, amz_date, hex, hmac, sha256, uri_encode, AwsCreds, AwsFiles,
};
use reqwest::Url;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Bytes one object may have.
pub const MAX_OBJECT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Bytes one reader call may download in all (a glob's objects).
pub const MAX_CALL_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Redirects followed (same scheme, host and port only).
pub const MAX_REDIRECTS: usize = 5;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Without a byte for this long, the read stops.
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// A listing or a `HEAD`, in all.
const META_TIMEOUT: Duration = Duration::from_secs(60);
/// Azure Blob Storage's REST API version.
const AZURE_VERSION: &str = "2021-08-06";

/// The readers that read URLs; `st_read` only `https://`.
pub const REMOTE_READERS: &[&str] = &[
    "read_csv",
    "read_csv_auto",
    "read_parquet",
    "parquet_scan",
    "read_json",
    "read_json_auto",
    "read_ndjson",
    "read_ndjson_auto",
    "st_read",
];

/// Query parameters that carry a signature or a credential (lowercase).
const SIGNED_PARAMS: &[&str] = &[
    "x-amz-signature",
    "x-amz-credential",
    "x-amz-security-token",
    "x-goog-signature",
    "x-goog-credential",
    "awsaccesskeyid",
    "googleaccessid",
    "signature",
    "sig",
    "token",
    "access_token",
    "id_token",
    "api_key",
    "apikey",
    "key",
    "password",
    "pwd",
    "secret",
    "client_secret",
    "auth",
    "authorization",
];

/// Where an object lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cloud {
    Https,
    S3,
    Gcs,
    Azure,
}

/// A URL as a cell may read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteUrl {
    pub cloud: Cloud,
    /// What is approved, granted and shown: `https://host/path?query`,
    /// `s3://bucket/key`, `gs://bucket/key`,
    /// `az://account.blob.core.windows.net/container/blob`.
    pub canonical: String,
    /// What the dialog shows first: the host, or the bucket with its cloud.
    pub host: String,
    /// `https`: the host (and port); S3 and GCS: the bucket; Azure: the account.
    pub bucket: String,
    /// Azure's container.
    pub container: String,
    /// The object's key (a glob for a pattern); `https`: the path and query.
    pub key: String,
    pub glob: bool,
}

/// How the network is reached: the AWS profile of Settings › Notebooks and
/// the Azure account for the short `az://container/blob` form.
#[derive(Debug, Clone, Default)]
pub struct Net {
    pub aws_profile: Option<String>,
    pub azure_account: Option<String>,
    #[cfg(test)]
    pub test: Option<TestNet>,
}

/// Test servers: loopback and `http://` allowed, the clouds' endpoints
/// and credentials replaced.
#[cfg(test)]
#[derive(Debug, Clone, Default)]
pub struct TestNet {
    pub allow_local: bool,
    /// Skip the name rules (`localhost`…) to test the check after resolving.
    pub skip_names: bool,
    pub http: bool,
    /// `http://127.0.0.1:<port>`: path-style S3, GCS's and Azure's paths below it.
    pub s3: Option<String>,
    pub gcs: Option<String>,
    pub azure: Option<String>,
    pub aws: Option<AwsCreds>,
    /// A bearer token for GCS and Azure; `None`: anonymous.
    pub token: Option<String>,
}

impl Net {
    /// This machine's: the profile set in Settings › Notebooks (checked) and
    /// `AZURE_STORAGE_ACCOUNT`.
    pub fn current(aws_profile: Option<&str>) -> Self {
        let aws_profile = aws_profile
            .map(str::trim)
            .filter(|p| dbine_core::secret_sources::valid_aws_profile(p))
            .map(str::to_string);
        let azure_account = std::env::var("AZURE_STORAGE_ACCOUNT")
            .ok()
            .map(|a| a.trim().to_ascii_lowercase())
            .filter(|a| valid_azure_account(a));
        Net {
            aws_profile,
            azure_account,
            #[cfg(test)]
            test: None,
        }
    }

    fn allow_local(&self) -> bool {
        #[cfg(test)]
        if let Some(t) = &self.test {
            return t.allow_local;
        }
        false
    }

    fn check_names(&self) -> bool {
        #[cfg(test)]
        if let Some(t) = &self.test {
            return !t.allow_local && !t.skip_names;
        }
        true
    }

    fn allow_http(&self) -> bool {
        #[cfg(test)]
        if let Some(t) = &self.test {
            return t.http;
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Parsing

/// The scheme of `p` when it is written as a URL (`https`, `s3`…).
pub fn scheme_of(p: &str) -> Option<String> {
    let (s, _) = p.split_once("://")?;
    let ok = s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    ok.then(|| s.to_ascii_lowercase())
}

const LOGIN_HINT: &str = "aws sso login, gcloud auth application-default login o az login";

fn userinfo(p: &str) -> String {
    format!("«{p}» lleva un usuario o una contraseña en la URL: las credenciales nunca van en el notebook; DBine usa la sesión de la nube de esta computadora ({LOGIN_HINT})")
}

fn signed(p: &str) -> String {
    format!(
        "«{p}» lleva una firma o un token en la URL (X-Amz-Signature, sig=, token=…): las credenciales nunca van en el notebook; escribí la URL sin firma y DBine usa la sesión de la nube de esta computadora ({LOGIN_HINT})"
    )
}

fn valid_s3_bucket(b: &str) -> bool {
    (3..=63).contains(&b.len())
        && b.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-'))
        && b.starts_with(|c: char| c.is_ascii_alphanumeric())
        && b.ends_with(|c: char| c.is_ascii_alphanumeric())
        && !b.contains("..")
}

fn valid_gcs_bucket(b: &str) -> bool {
    (3..=222).contains(&b.len())
        && b.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-' | '_'))
        && b.starts_with(|c: char| c.is_ascii_alphanumeric())
        && b.ends_with(|c: char| c.is_ascii_alphanumeric())
        && b.split('.').all(|l| !l.is_empty() && l.len() <= 63)
}

fn valid_azure_account(a: &str) -> bool {
    (3..=24).contains(&a.len())
        && a.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn valid_azure_container(c: &str) -> bool {
    c == "$root"
        || ((3..=63).contains(&c.len())
            && c.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            && !c.starts_with('-')
            && !c.ends_with('-')
            && !c.contains("--"))
}

/// An object key: not empty, no `.` / `..` segments, no control characters.
fn check_key(p: &str, key: &str) -> Result<(), String> {
    if key.is_empty() || key.ends_with('/') {
        return Err(format!("«{p}» no nombra un archivo: escribí el objeto (…/archivo.parquet) o un patrón (…/*.parquet)"));
    }
    if key.split('/').any(|s| s == "." || s == "..")
        || key.chars().any(|c| c.is_control() || c == '\\')
    {
        return Err(format!(
            "«{p}» tiene segmentos «.» o «..» o caracteres de control"
        ));
    }
    Ok(())
}

/// `p` as a URL a cell may read; `Ok(None)` when it isn't written as a URL.
pub fn parse(p: &str, net: &Net) -> Result<Option<RemoteUrl>, String> {
    let Some(scheme) = scheme_of(p) else {
        return Ok(None);
    };
    let rest = &p[scheme.len() + 3..];
    match scheme.as_str() {
        "https" => parse_https(p, net).map(Some),
        "http" if net.allow_http() => parse_https(p, net).map(Some),
        "http" => Err(format!("«{p}» usa http://, que viaja sin cifrar: estas celdas leen de la web solo https://")),
        "s3" | "gs" | "gcs" | "az" | "azure" | "abfss" => {
            if rest.contains('#') {
                return Err(format!("«{p}» no es una URL de un objeto (tiene «#»)"));
            }
            // A `?` is a glob character in a key; `?…=…` is a query.
            if rest.split_once('?').is_some_and(|(_, q)| q.contains('=')) {
                return Err(signed(p));
            }
            let (auth, key) = rest.split_once('/').unwrap_or((rest, ""));
            match scheme.as_str() {
                "s3" => {
                    if auth.contains('@') {
                        return Err(userinfo(p));
                    }
                    if !valid_s3_bucket(auth) {
                        return Err(format!("«{auth}» no es un nombre de bucket de S3 válido"));
                    }
                    check_key(p, key)?;
                    Ok(Some(RemoteUrl {
                        cloud: Cloud::S3,
                        canonical: format!("s3://{auth}/{key}"),
                        host: format!("{auth} (Amazon S3)"),
                        bucket: auth.into(),
                        container: String::new(),
                        key: key.into(),
                        glob: has_glob(key),
                    }))
                }
                "gs" | "gcs" => {
                    if auth.contains('@') {
                        return Err(userinfo(p));
                    }
                    if !valid_gcs_bucket(auth) {
                        return Err(format!("«{auth}» no es un nombre de bucket de Google Cloud Storage válido"));
                    }
                    check_key(p, key)?;
                    Ok(Some(RemoteUrl {
                        cloud: Cloud::Gcs,
                        canonical: format!("gs://{auth}/{key}"),
                        host: format!("{auth} (Google Cloud Storage)"),
                        bucket: auth.into(),
                        container: String::new(),
                        key: key.into(),
                        glob: has_glob(key),
                    }))
                }
                _ => parse_azure(p, &scheme, auth, key, net).map(Some),
            }
        }
        _ => Err(format!("«{p}»: estas celdas leen de la red solo https://, s3://, gs:// y az:// (o abfss://); ningún otro esquema")),
    }
}

fn parse_https(p: &str, net: &Net) -> Result<RemoteUrl, String> {
    let u = Url::parse(p).map_err(|e| format!("«{p}» no es una URL válida: {e}"))?;
    if !u.username().is_empty() || u.password().is_some() {
        return Err(userinfo(p));
    }
    let Some(host) = u.host_str().filter(|h| !h.is_empty()) else {
        return Err(format!("«{p}» no tiene servidor"));
    };
    if u.path().contains(['*', '[']) {
        return Err(format!("«{p}»: una URL https:// no puede ser un patrón; los patrones (*.parquet) van solo en s3://, gs:// y az://"));
    }
    if u.query_pairs().any(|(k, _)| {
        let k = k.to_ascii_lowercase();
        SIGNED_PARAMS.contains(&k.as_str()) || k.starts_with("x-amz-") || k.starts_with("x-goog-")
    }) {
        return Err(signed(p));
    }
    if net.check_names() && name_blocked(host) {
        return Err(local_host(host));
    }
    let mut c = u.clone();
    c.set_fragment(None);
    let authority = match u.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    let key = format!(
        "{}{}",
        u.path().trim_start_matches('/'),
        u.query().map(|q| format!("?{q}")).unwrap_or_default()
    );
    check_key(p, u.path().trim_start_matches('/'))?;
    Ok(RemoteUrl {
        cloud: Cloud::Https,
        canonical: c.to_string(),
        host: authority.clone(),
        bucket: authority,
        container: String::new(),
        key,
        glob: false,
    })
}

fn parse_azure(
    p: &str,
    scheme: &str,
    auth: &str,
    key: &str,
    net: &Net,
) -> Result<RemoteUrl, String> {
    let bad_form = || {
        format!("«{p}»: escribí az://<cuenta>.blob.core.windows.net/<contenedor>/<archivo>, abfss://<contenedor>@<cuenta>.dfs.core.windows.net/<archivo> o, con AZURE_STORAGE_ACCOUNT definida, az://<contenedor>/<archivo>")
    };
    let (account, container, blob) = if scheme == "abfss" {
        let (container, host) = auth.split_once('@').ok_or_else(bad_form)?;
        if host.contains('@') || container.contains(':') {
            return Err(userinfo(p));
        }
        let account = host
            .strip_suffix(".dfs.core.windows.net")
            .or_else(|| host.strip_suffix(".blob.core.windows.net"))
            .ok_or_else(bad_form)?;
        (account.to_string(), container.to_string(), key.to_string())
    } else {
        if auth.contains('@') {
            return Err(userinfo(p));
        }
        match auth
            .strip_suffix(".blob.core.windows.net")
            .or_else(|| auth.strip_suffix(".dfs.core.windows.net"))
        {
            Some(account) => {
                let (container, blob) = key.split_once('/').ok_or_else(bad_form)?;
                (account.to_string(), container.to_string(), blob.to_string())
            }
            None if auth.contains('.') => return Err(bad_form()),
            None => (
                net.azure_account.clone().ok_or_else(bad_form)?,
                auth.to_string(),
                key.to_string(),
            ),
        }
    };
    if !valid_azure_account(&account) {
        return Err(format!(
            "«{account}» no es un nombre de cuenta de Azure Storage válido"
        ));
    }
    if !valid_azure_container(&container) {
        return Err(format!(
            "«{container}» no es un nombre de contenedor de Azure válido"
        ));
    }
    check_key(p, &blob)?;
    Ok(RemoteUrl {
        cloud: Cloud::Azure,
        canonical: format!("az://{account}.blob.core.windows.net/{container}/{blob}"),
        host: format!("{account}.blob.core.windows.net (Azure)"),
        bucket: account,
        container,
        glob: has_glob(&blob),
        key: blob,
    })
}

/// The scheme and authority of a URL (`s3://bucket`) and the rest.
fn split_authority(u: &str) -> Option<(&str, &str)> {
    let i = u.find("://")? + 3;
    let j = u[i..].find('/').map_or(u.len(), |j| i + j);
    Some((&u[..j], u[j..].trim_start_matches('/')))
}

/// `url` (canonical) matches `pattern` (a canonical URL with a glob in
/// its key): the same scheme and bucket, then segment by segment, `**`
/// standing for any number of them.
pub fn url_pattern_matches(pattern: &str, url: &str) -> bool {
    fn go(pat: &[&str], name: &[&str]) -> bool {
        match pat.first() {
            None => name.is_empty(),
            Some(&"**") => (0..=name.len()).any(|k| go(&pat[1..], &name[k..])),
            Some(p) => {
                let Some(n) = name.first() else { return false };
                let hit = if has_glob(p) {
                    wild(
                        &p.chars().collect::<Vec<_>>(),
                        &n.chars().collect::<Vec<_>>(),
                    )
                } else {
                    p == n
                };
                hit && go(&pat[1..], &name[1..])
            }
        }
    }
    let (Some((pa, pp)), Some((ua, up))) = (split_authority(pattern), split_authority(url)) else {
        return false;
    };
    pa == ua
        && go(
            &pp.split('/').collect::<Vec<_>>(),
            &up.split('/').collect::<Vec<_>>(),
        )
}

// ---------------------------------------------------------------------------
// Hosts

/// `localhost` and the names that stand for this computer, the local
/// network or a cloud's metadata service.
pub fn name_blocked(host: &str) -> bool {
    let h = host
        .trim_end_matches('.')
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    h == "localhost"
        || h.ends_with(".localhost")
        || h.ends_with(".local")
        || h.ends_with(".internal")
        || h.ends_with(".home.arpa")
        || h == "metadata"
        || h.parse::<IpAddr>().is_ok_and(ip_blocked)
}

/// An address that is this computer, the local network, link-local (the
/// cloud metadata service `169.254.169.254`), CGNAT, multicast or reserved.
pub fn ip_blocked(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v4_blocked(v),
        IpAddr::V6(v) => {
            if let Some(m) = v.to_ipv4_mapped() {
                return v4_blocked(m);
            }
            let s = v.segments();
            // NAT64 (64:ff9b::/96) and the deprecated IPv4-compatible ::a.b.c.d.
            if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0]
                || s[..6] == [0, 0, 0, 0, 0, 0] && !v.is_loopback() && !v.is_unspecified()
            {
                return v4_blocked(Ipv4Addr::new(
                    (s[6] >> 8) as u8,
                    s[6] as u8,
                    (s[7] >> 8) as u8,
                    s[7] as u8,
                ));
            }
            v.is_loopback()
                || v.is_unspecified()
                || v.is_multicast()
                || (s[0] & 0xfe00) == 0xfc00 // fc00::/7, unique local (fd00::/8 included)
                || (s[0] & 0xffc0) == 0xfe80 // fe80::/10, link-local
                || (s[0] & 0xffc0) == 0xfec0 // fec0::/10, site-local
                || (s[0] == 0x2001 && s[1] == 0x0db8) // documentation
                || v == Ipv6Addr::new(0xfd00, 0x0ec2, 0, 0, 0, 0, 0, 0x0254) // AWS's metadata over IPv6
        }
    }
}

fn v4_blocked(v: Ipv4Addr) -> bool {
    let o = v.octets();
    v.is_loopback()
        || v.is_private()
        || v.is_link_local()
        || v.is_unspecified()
        || v.is_broadcast()
        || v.is_multicast()
        || v.is_documentation()
        || o[0] == 0
        || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10, CGNAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24
        || (o[0] == 198 && (o[1] & 0xfe) == 18) // 198.18.0.0/15, benchmarking
        || o[0] >= 240 // 240.0.0.0/4, reserved
}

fn local_host(host: &str) -> String {
    format!("«{host}» es esta computadora, la red local o un servicio interno de la nube: estas celdas leen de la red solo servidores públicos")
}

/// A client for `url`'s host only: its name resolved here and every address
/// checked ([`ip_blocked`]), the connection pinned to them (no second
/// lookup), no proxy, no redirects (they are followed by [`send`]).
async fn connect(url: &Url, net: &Net) -> Result<reqwest::Client, String> {
    let allow_http = net.allow_http();
    if url.scheme() != "https" && !(allow_http && url.scheme() == "http") {
        return Err(format!("«{url}»: solo https://"));
    }
    let port = url.port_or_known_default().unwrap_or(443);
    let host = url
        .host_str()
        .ok_or_else(|| format!("«{url}» no tiene servidor"))?;
    let local_ok = net.allow_local();
    let literal = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<IpAddr>()
        .ok();
    let (name, addrs): (Option<String>, Vec<SocketAddr>) = match literal {
        Some(ip) => (None, vec![SocketAddr::new(ip, port)]),
        None => {
            let d = host;
            if net.check_names() && name_blocked(d) {
                return Err(local_host(d));
            }
            let found: Vec<SocketAddr> =
                tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::lookup_host((d, port)))
                    .await
                    .map_err(|_| format!("no se pudo resolver «{d}»: tardó demasiado"))?
                    .map_err(|e| format!("no se pudo resolver «{d}»: {e}"))?
                    .collect();
            (Some(d.to_string()), found)
        }
    };
    if addrs.is_empty() {
        return Err(format!(
            "no se encontró la dirección de «{}»",
            url.host_str().unwrap_or("")
        ));
    }
    if !local_ok {
        if let Some(a) = addrs.iter().find(|a| ip_blocked(a.ip())) {
            return Err(format!(
                "«{}» apunta a {} (esta computadora, la red local o un servicio interno de la nube): estas celdas leen de la red solo servidores públicos",
                url.host_str().unwrap_or(""),
                a.ip()
            ));
        }
    }
    let mut b = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .https_only(!allow_http)
        .user_agent(concat!("DBine/", env!("CARGO_PKG_VERSION")));
    if let Some(d) = &name {
        b = b.resolve_to_addrs(d, &addrs);
    }
    b.build().map_err(|e| e.to_string())
}

fn origin(u: &Url) -> (String, Option<String>, Option<u16>) {
    (
        u.scheme().to_string(),
        u.host_str().map(str::to_ascii_lowercase),
        u.port_or_known_default(),
    )
}

/// Makes the headers for a request to a URL.
type Signer<'a> = dyn Fn(&reqwest::Method, &Url) -> Vec<(String, String)> + Sync + 'a;

/// `method url`, the headers made for each URL it goes to (`sign`),
/// following redirects only within the same scheme, host and port.
async fn send(
    net: &Net,
    method: reqwest::Method,
    url: Url,
    sign: &Signer<'_>,
) -> Result<(reqwest::Response, Url), String> {
    let mut url = url;
    for _ in 0..=MAX_REDIRECTS {
        let client = connect(&url, net).await?;
        let mut req = client.request(method.clone(), url.clone());
        for (k, v) in sign(&method, &url) {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| {
            format!(
                "no se pudo leer «{}»: {}",
                url.host_str().unwrap_or(""),
                e.without_url()
            )
        })?;
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|l| l.to_str().ok())
            .map(str::to_string);
        match (resp.status().is_redirection(), location) {
            (true, Some(loc)) => {
                let next = url
                    .join(&loc)
                    .map_err(|_| format!("«{url}» redirige a una dirección que no es válida"))?;
                if origin(&next) != origin(&url) {
                    return Err(format!(
                        "«{}» redirige a «{}»: DBine sigue redirecciones solo dentro del mismo servidor; escribí la URL final",
                        url.host_str().unwrap_or(""),
                        next.host_str().unwrap_or(next.as_str())
                    ));
                }
                url = next;
            }
            _ => return Ok((resp, url)),
        }
    }
    Err(format!("«{url}» redirige demasiadas veces"))
}

// ---------------------------------------------------------------------------
// Credentials and endpoints

/// How requests to one cloud are signed.
#[derive(Clone)]
enum Auth {
    /// No sign-in found (why, for the message when the object needs one).
    Anonymous(Option<String>),
    Aws {
        creds: AwsCreds,
        region: String,
    },
    Bearer(String),
}

/// The AWS region: the environment's, the profile's, else `us-east-1`
/// (a bucket in another region says which, [`s3_head`]).
fn aws_region(net: &Net) -> String {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let files = AwsFiles::load(&env);
    let profile = net
        .aws_profile
        .clone()
        .or_else(|| env("AWS_PROFILE"))
        .unwrap_or_else(|| "default".into());
    env("AWS_REGION")
        .or_else(|| env("AWS_DEFAULT_REGION"))
        .or_else(|| files.get(&profile, "region"))
        .filter(|r| dbine_core::secret_sources::valid_aws_region(r))
        .unwrap_or_else(|| "us-east-1".into())
}

async fn auth_for(cloud: Cloud, net: &Net) -> Auth {
    #[cfg(test)]
    if let Some(t) = &net.test {
        return match cloud {
            Cloud::S3 => t.aws.clone().map_or(
                Auth::Anonymous(Some("sin credenciales de prueba".into())),
                |creds| Auth::Aws {
                    creds,
                    region: "us-east-1".into(),
                },
            ),
            Cloud::Gcs | Cloud::Azure => t.token.clone().map_or(
                Auth::Anonymous(Some("sin token de prueba".into())),
                Auth::Bearer,
            ),
            Cloud::Https => Auth::Anonymous(None),
        };
    }
    match cloud {
        Cloud::Https => Auth::Anonymous(None),
        Cloud::S3 => {
            let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
            let files = AwsFiles::load(&env);
            let named = net.aws_profile.is_some();
            let profile = net
                .aws_profile
                .clone()
                .or_else(|| env("AWS_PROFILE"))
                .unwrap_or_else(|| "default".into());
            match cloud_auth::aws_credentials(named, &profile, &files, &env).await {
                Ok((creds, _)) => Auth::Aws {
                    creds,
                    region: aws_region(net),
                },
                Err(e) => Auth::Anonymous(Some(e.to_string())),
            }
        }
        Cloud::Gcs => match cloud_auth::gcp_token(None, true).await {
            Ok((t, _, _)) => Auth::Bearer(t),
            Err(e) => Auth::Anonymous(Some(e.to_string())),
        },
        Cloud::Azure => match cloud_auth::azure_token_for("https://storage.azure.com/", None).await
        {
            Ok((t, _)) => Auth::Bearer(t),
            Err(e) => Auth::Anonymous(Some(e.to_string())),
        },
    }
}

fn needs_sign_in(cloud: Cloud, what: &str, why: Option<&str>) -> String {
    let how = match cloud {
        Cloud::S3 => "iniciá sesión con «aws sso login» (o elegí el perfil de AWS en Configuración › Notebooks)",
        Cloud::Gcs => "iniciá sesión con «gcloud auth application-default login»",
        Cloud::Azure => "iniciá sesión con «az login»",
        Cloud::Https => "el servidor pide credenciales, y estas celdas no las mandan a sitios web",
    };
    match why {
        Some(w) => format!(
            "«{what}» necesita credenciales y no hay una sesión que sirva: {how} ({})",
            cloud_auth::short(w, 200)
        ),
        None => format!("«{what}»: el acceso fue denegado: {how}, con una cuenta que pueda leerlo"),
    }
}

/// Each segment of a key encoded for a URL path (`/` kept).
fn enc_path(key: &str) -> String {
    key.split('/').map(uri_encode).collect::<Vec<_>>().join("/")
}

fn s3_base(bucket: &str, region: &str, net: &Net) -> String {
    #[cfg(test)]
    if let Some(e) = net.test.as_ref().and_then(|t| t.s3.clone()) {
        return format!("{e}/{bucket}");
    }
    let _ = net;
    let suffix = if region.starts_with("cn-") {
        "amazonaws.com.cn"
    } else {
        "amazonaws.com"
    };
    // A bucket with dots doesn't match the wildcard certificate: path style.
    if bucket.contains('.') {
        format!("https://s3.{region}.{suffix}/{bucket}")
    } else {
        format!("https://{bucket}.s3.{region}.{suffix}")
    }
}

fn gcs_base(net: &Net) -> String {
    #[cfg(test)]
    if let Some(e) = net.test.as_ref().and_then(|t| t.gcs.clone()) {
        return e;
    }
    let _ = net;
    "https://storage.googleapis.com".into()
}

fn azure_base(account: &str, net: &Net) -> String {
    #[cfg(test)]
    if let Some(e) = net.test.as_ref().and_then(|t| t.azure.clone()) {
        return format!("{e}/{account}");
    }
    let _ = net;
    format!("https://{account}.blob.core.windows.net")
}

/// SigV4 headers of an S3 request (unsigned payload).
fn s3_sign(
    method: &reqwest::Method,
    url: &Url,
    creds: &AwsCreds,
    region: &str,
    unix: u64,
) -> Vec<(String, String)> {
    let (stamp, date) = amz_date(unix);
    let host = match url.port() {
        Some(p) => format!("{}:{p}", url.host_str().unwrap_or("")),
        None => url.host_str().unwrap_or("").to_string(),
    };
    let mut query: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (uri_encode(&k), uri_encode(&v)))
        .collect();
    query.sort();
    let query = query
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let mut signed: Vec<(String, String)> = vec![
        ("host".into(), host),
        ("x-amz-content-sha256".into(), "UNSIGNED-PAYLOAD".into()),
        ("x-amz-date".into(), stamp.clone()),
    ];
    if let Some(t) = &creds.session_token {
        signed.push(("x-amz-security-token".into(), t.clone()));
    }
    signed.sort();
    let canonical_headers: String = signed.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let names = signed
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical = format!(
        "{}\n{}\n{query}\n{canonical_headers}\n{names}\nUNSIGNED-PAYLOAD",
        method.as_str(),
        url.path()
    );
    let scope = format!("{date}/{region}/s3/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
        hex(&sha256(&[canonical.as_bytes()]))
    );
    let mut key = hmac(
        format!("AWS4{}", creds.secret_access_key).as_bytes(),
        date.as_bytes(),
    );
    for part in [region, "s3", "aws4_request"] {
        key = hmac(&key, part.as_bytes());
    }
    let signature = hex(&hmac(&key, to_sign.as_bytes()));
    let mut out: Vec<(String, String)> = signed.into_iter().filter(|(k, _)| k != "host").collect();
    out.push((
        "authorization".into(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={names}, Signature={signature}",
            creds.access_key_id
        ),
    ));
    out
}

/// The headers of a request with `auth` (Azure's version header too).
fn headers_for(
    cloud: Cloud,
    auth: &Auth,
    method: &reqwest::Method,
    url: &Url,
) -> Vec<(String, String)> {
    let mut h = match auth {
        Auth::Aws { creds, region } => s3_sign(method, url, creds, region, cloud_auth::now()),
        Auth::Bearer(t) => vec![("authorization".into(), format!("Bearer {t}"))],
        Auth::Anonymous(_) => Vec::new(),
    };
    if cloud == Cloud::Azure {
        h.push(("x-ms-version".into(), AZURE_VERSION.into()));
    }
    h
}

// ---------------------------------------------------------------------------
// Objects

/// One object, as found by a listing or a `HEAD`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    /// Canonical (`s3://bucket/key`).
    pub url: String,
    pub key: String,
    pub size: u64,
    pub mtime_ms: i64,
    /// The `ETag`, else `Last-Modified` and size; `None`: unknown (read
    /// every time).
    pub version: Option<String>,
}

fn http_date_ms(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc2822(s.trim())
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(s.trim()))
        .map(|d| d.timestamp_millis())
        .unwrap_or(0)
}

fn version_of(etag: Option<&str>, last_modified: Option<&str>, size: u64) -> Option<String> {
    match (
        etag.map(str::trim).filter(|e| !e.is_empty()),
        last_modified.map(str::trim).filter(|l| !l.is_empty()),
    ) {
        (Some(e), _) => Some(format!("etag:{}", e.trim_matches('"'))),
        (None, Some(l)) => Some(format!("lm:{l}:{size}")),
        _ => None,
    }
}

/// A cloud's state for one run: its sign-in (found once).
pub struct Session<'a> {
    net: &'a Net,
    auths: Vec<(Cloud, Auth)>,
}

impl<'a> Session<'a> {
    pub fn new(net: &'a Net) -> Self {
        Session {
            net,
            auths: Vec::new(),
        }
    }

    async fn auth(&mut self, cloud: Cloud) -> Auth {
        if let Some((_, a)) = self.auths.iter().find(|(c, _)| *c == cloud) {
            return a.clone();
        }
        let a = auth_for(cloud, self.net).await;
        self.auths.push((cloud, a.clone()));
        a
    }

    fn set_auth(&mut self, cloud: Cloud, a: Auth) {
        self.auths.retain(|(c, _)| *c != cloud);
        self.auths.push((cloud, a));
    }

    /// Where `u` (one object, `key`) is fetched from.
    fn object_url(&self, u: &RemoteUrl, key: &str, auth: &Auth) -> Result<Url, String> {
        let s = match u.cloud {
            Cloud::Https => return Url::parse(&u.canonical).map_err(|e| e.to_string()),
            Cloud::S3 => {
                let region = match auth {
                    Auth::Aws { region, .. } => region.clone(),
                    _ => aws_region(self.net),
                };
                format!(
                    "{}/{}",
                    s3_base(&u.bucket, &region, self.net),
                    enc_path(key)
                )
            }
            Cloud::Gcs => format!("{}/{}/{}", gcs_base(self.net), u.bucket, enc_path(key)),
            Cloud::Azure => format!(
                "{}/{}/{}",
                azure_base(&u.bucket, self.net),
                u.container,
                enc_path(key)
            ),
        };
        Url::parse(&s).map_err(|e| format!("«{}»: {e}", u.canonical))
    }

    /// `method` on one object, signed; an S3 bucket in another region is
    /// asked again there (the region its answer names).
    async fn request(
        &mut self,
        u: &RemoteUrl,
        key: &str,
        method: reqwest::Method,
    ) -> Result<reqwest::Response, String> {
        let mut auth = self.auth(u.cloud).await;
        for _ in 0..2 {
            let url = self.object_url(u, key, &auth)?;
            let a = auth.clone();
            let cloud = u.cloud;
            let (resp, _) = send(self.net, method.clone(), url, &|m, url| {
                headers_for(cloud, &a, m, url)
            })
            .await?;
            if u.cloud == Cloud::S3 {
                let region = resp
                    .headers()
                    .get("x-amz-bucket-region")
                    .and_then(|r| r.to_str().ok())
                    .map(str::to_string);
                if let (Auth::Aws { creds, region: was }, Some(r)) = (&auth, region) {
                    if r != *was
                        && dbine_core::secret_sources::valid_aws_region(&r)
                        && matches!(resp.status().as_u16(), 301 | 400)
                    {
                        auth = Auth::Aws {
                            creds: creds.clone(),
                            region: r,
                        };
                        self.set_auth(u.cloud, auth.clone());
                        continue;
                    }
                }
            }
            return Ok(resp);
        }
        Err(format!(
            "«{}»: el bucket no responde en su región",
            u.canonical
        ))
    }

    /// The status of a response as an error for `what`, or `Ok` for a 2xx.
    async fn check(
        &mut self,
        u: &RemoteUrl,
        what: &str,
        resp: &reqwest::Response,
    ) -> Result<(), String> {
        let status = resp.status().as_u16();
        match status {
            200..=299 => Ok(()),
            401 | 403 => {
                let why = match self.auth(u.cloud).await {
                    Auth::Anonymous(why) => why.or_else(|| Some("sin sesión".into())),
                    _ => None,
                };
                Err(needs_sign_in(
                    u.cloud,
                    what,
                    why.as_deref().filter(|_| u.cloud != Cloud::Https),
                ))
            }
            404 => Err(format!("no existe «{what}»")),
            _ => Err(format!("«{what}»: el servidor respondió {status}")),
        }
    }

    /// One object's size, time and version (`HEAD`). A server that refuses
    /// `HEAD` (405, 501) leaves it unknown: it is read every time.
    pub async fn head(&mut self, u: &RemoteUrl) -> Result<Object, String> {
        let resp =
            tokio::time::timeout(META_TIMEOUT, self.request(u, &u.key, reqwest::Method::HEAD))
                .await
                .map_err(|_| format!("«{}» tardó demasiado en responder", u.canonical))??;
        if matches!(resp.status().as_u16(), 405 | 501) {
            return Ok(Object {
                url: u.canonical.clone(),
                key: u.key.clone(),
                size: 0,
                mtime_ms: 0,
                version: None,
            });
        }
        self.check(u, &u.canonical, &resp).await?;
        let h = resp.headers();
        let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
        let size = get("content-length")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let lm = get("last-modified");
        let version = version_of(get("etag").as_deref(), lm.as_deref(), size);
        Ok(Object {
            url: u.canonical.clone(),
            key: u.key.clone(),
            size,
            mtime_ms: lm.as_deref().map_or(0, http_date_ms),
            version,
        })
    }

    /// The objects under `u`'s key up to its first glob character, those
    /// that match its pattern ([`url_pattern_matches`]), at most `max`.
    pub async fn list(&mut self, u: &RemoteUrl, max: usize) -> Result<Vec<Object>, String> {
        let prefix: String = u
            .key
            .chars()
            .take_while(|c| !matches!(c, '*' | '?' | '['))
            .collect();
        let mut out: Vec<Object> = Vec::new();
        let mut next: Option<String> = None;
        let started = std::time::Instant::now();
        loop {
            if started.elapsed() > META_TIMEOUT * 5 {
                return Err(format!("listar «{}» tardó demasiado", u.canonical));
            }
            let (page, more) = self.list_page(u, &prefix, next.as_deref()).await?;
            for o in page {
                if url_pattern_matches(&u.canonical, &o.url) && !o.key.ends_with('/') {
                    out.push(o);
                    if out.len() > max {
                        return Err(format!(
                            "«{}» abarca demasiados archivos (hasta {max})",
                            u.canonical
                        ));
                    }
                }
            }
            match more {
                Some(m) => next = Some(m),
                None => break,
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    async fn list_page(
        &mut self,
        u: &RemoteUrl,
        prefix: &str,
        next: Option<&str>,
    ) -> Result<(Vec<Object>, Option<String>), String> {
        let mut auth = self.auth(u.cloud).await;
        for _ in 0..2 {
            let mut url = match u.cloud {
                Cloud::S3 => {
                    let region = match &auth {
                        Auth::Aws { region, .. } => region.clone(),
                        _ => aws_region(self.net),
                    };
                    Url::parse(&format!("{}/", s3_base(&u.bucket, &region, self.net)))
                        .map_err(|e| e.to_string())?
                }
                Cloud::Gcs => Url::parse(&format!(
                    "{}/storage/v1/b/{}/o",
                    gcs_base(self.net),
                    u.bucket
                ))
                .map_err(|e| e.to_string())?,
                Cloud::Azure => Url::parse(&format!(
                    "{}/{}",
                    azure_base(&u.bucket, self.net),
                    u.container
                ))
                .map_err(|e| e.to_string())?,
                Cloud::Https => return Err("una URL https:// no se lista".into()),
            };
            {
                let mut q = url.query_pairs_mut();
                match u.cloud {
                    Cloud::S3 => {
                        q.append_pair("list-type", "2")
                            .append_pair("prefix", prefix);
                        if let Some(n) = next {
                            q.append_pair("continuation-token", n);
                        }
                    }
                    Cloud::Gcs => {
                        q.append_pair("prefix", prefix)
                            .append_pair("fields", "items(name,size,etag,updated),nextPageToken");
                        if let Some(n) = next {
                            q.append_pair("pageToken", n);
                        }
                    }
                    _ => {
                        q.append_pair("restype", "container")
                            .append_pair("comp", "list")
                            .append_pair("prefix", prefix);
                        if let Some(n) = next {
                            q.append_pair("marker", n);
                        }
                    }
                }
            }
            let a = auth.clone();
            let cloud = u.cloud;
            let sign = |m: &reqwest::Method, url: &Url| headers_for(cloud, &a, m, url);
            let fut = send(self.net, reqwest::Method::GET, url, &sign);
            let (resp, _) = tokio::time::timeout(META_TIMEOUT, fut)
                .await
                .map_err(|_| format!("listar «{}» tardó demasiado", u.canonical))??;
            if u.cloud == Cloud::S3 {
                let region = resp
                    .headers()
                    .get("x-amz-bucket-region")
                    .and_then(|r| r.to_str().ok())
                    .map(str::to_string);
                if let (Auth::Aws { creds, region: was }, Some(r)) = (&auth, region) {
                    if r != *was
                        && dbine_core::secret_sources::valid_aws_region(&r)
                        && matches!(resp.status().as_u16(), 301 | 400)
                    {
                        auth = Auth::Aws {
                            creds: creds.clone(),
                            region: r,
                        };
                        self.set_auth(u.cloud, auth.clone());
                        continue;
                    }
                }
            }
            self.check(u, &u.canonical, &resp).await?;
            let body = read_small(resp, 32 * 1024 * 1024).await?;
            return Ok(parse_listing(u, &body));
        }
        Err(format!(
            "«{}»: el bucket no responde en su región",
            u.canonical
        ))
    }
}

/// A response body of at most `max` bytes, as text.
async fn read_small(mut resp: reqwest::Response, max: usize) -> Result<String, String> {
    let mut buf = Vec::new();
    while let Some(c) = resp
        .chunk()
        .await
        .map_err(|e| e.without_url().to_string())?
    {
        buf.extend_from_slice(&c);
        if buf.len() > max {
            return Err("la respuesta del listado es demasiado grande".into());
        }
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// The text of every `<tag>…</tag>` in `xml` (`open`: `"<tag>"`).
fn tags<'x>(xml: &'x str, open: &str) -> Vec<&'x str> {
    let close = open.replacen('<', "</", 1);
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(i) = xml[at..].find(open) {
        let from = at + i + open.len();
        let Some(j) = xml[from..].find(&close) else {
            break;
        };
        out.push(&xml[from..from + j]);
        at = from + j + close.len();
    }
    out
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

/// One page of a listing: its objects and the token of the next page.
pub fn parse_listing(u: &RemoteUrl, body: &str) -> (Vec<Object>, Option<String>) {
    let canonical = |key: &str| match u.cloud {
        Cloud::S3 => format!("s3://{}/{key}", u.bucket),
        Cloud::Gcs => format!("gs://{}/{key}", u.bucket),
        _ => format!(
            "az://{}.blob.core.windows.net/{}/{key}",
            u.bucket, u.container
        ),
    };
    let first = |x: &str, t: &str| tags(x, t).first().map(|s| unescape(s));
    match u.cloud {
        Cloud::Gcs => {
            let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
            let items = v
                .get("items")
                .and_then(|i| i.as_array())
                .cloned()
                .unwrap_or_default();
            let objs = items
                .iter()
                .filter_map(|it| {
                    let key = it.get("name")?.as_str()?.to_string();
                    let size = it
                        .get("size")
                        .and_then(|s| {
                            s.as_str()
                                .and_then(|s| s.parse().ok())
                                .or_else(|| s.as_u64())
                        })
                        .unwrap_or(0);
                    let updated = it.get("updated").and_then(|s| s.as_str());
                    Some(Object {
                        url: canonical(&key),
                        version: version_of(it.get("etag").and_then(|e| e.as_str()), updated, size),
                        mtime_ms: updated.map_or(0, http_date_ms),
                        size,
                        key,
                    })
                })
                .collect();
            (
                objs,
                v.get("nextPageToken")
                    .and_then(|t| t.as_str())
                    .filter(|t| !t.is_empty())
                    .map(str::to_string),
            )
        }
        Cloud::S3 => {
            let objs = tags(body, "<Contents>")
                .into_iter()
                .filter_map(|c| {
                    let key = first(c, "<Key>")?;
                    let size = first(c, "<Size>").and_then(|s| s.parse().ok()).unwrap_or(0);
                    let lm = first(c, "<LastModified>");
                    Some(Object {
                        url: canonical(&key),
                        version: version_of(first(c, "<ETag>").as_deref(), lm.as_deref(), size),
                        mtime_ms: lm.as_deref().map_or(0, http_date_ms),
                        size,
                        key,
                    })
                })
                .collect();
            let more = first(body, "<IsTruncated>").is_some_and(|t| t == "true");
            (
                objs,
                if more {
                    first(body, "<NextContinuationToken>").filter(|t| !t.is_empty())
                } else {
                    None
                },
            )
        }
        _ => {
            let objs = tags(body, "<Blob>")
                .into_iter()
                .filter_map(|b| {
                    let key = first(b, "<Name>")?;
                    let size = first(b, "<Content-Length>")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    let lm = first(b, "<Last-Modified>");
                    Some(Object {
                        url: canonical(&key),
                        version: version_of(first(b, "<Etag>").as_deref(), lm.as_deref(), size),
                        mtime_ms: lm.as_deref().map_or(0, http_date_ms),
                        size,
                        key,
                    })
                })
                .collect();
            (objs, first(body, "<NextMarker>").filter(|t| !t.is_empty()))
        }
    }
}

// ---------------------------------------------------------------------------
// Planning and reading

/// The entry the approval asks about for `u`, before anything goes over
/// the network: the URL (or the pattern), its host, no size or time yet.
pub fn pending_file(u: &RemoteUrl) -> DataFile {
    DataFile {
        path: u.canonical.clone(),
        display: u.canonical.clone(),
        pattern: u.glob.then(|| u.canonical.clone()),
        host: Some(u.host.clone()),
        ..Default::default()
    }
}

/// The objects an approved entry stands for (a `HEAD` for one URL, a
/// listing for a pattern), as the files the read reads.
pub async fn expand(
    entry: &DataFile,
    net: &Net,
    session: &mut Session<'_>,
    max: usize,
) -> CommandResult<Vec<DataFile>> {
    let u = parse(&entry.path, net)
        .map_err(CommandError::BadRequest)?
        .ok_or_else(|| CommandError::BadRequest(format!("«{}» no es una URL", entry.path)))?;
    let objects = if u.glob {
        let found = session
            .list(&u, max)
            .await
            .map_err(CommandError::BadRequest)?;
        if found.is_empty() {
            return Err(CommandError::BadRequest(format!(
                "ningún archivo coincide con «{}»",
                u.canonical
            )));
        }
        found
    } else {
        vec![session.head(&u).await.map_err(CommandError::BadRequest)?]
    };
    let mut out = Vec::new();
    for o in objects {
        if o.size > MAX_OBJECT_BYTES {
            return Err(CommandError::BadRequest(format!(
                "«{}» pesa más de {} GB, el máximo por archivo",
                o.url,
                MAX_OBJECT_BYTES >> 30
            )));
        }
        out.push(DataFile {
            path: o.url.clone(),
            display: o.url,
            size: o.size,
            mtime_ms: o.mtime_ms,
            pattern: entry.pattern.clone(),
            host: Some(u.host.clone()),
            version: o.version,
            ..Default::default()
        });
    }
    Ok(out)
}

/// A folder of DBine's own in the temp folder (`0700`), removed when
/// dropped: where downloaded objects live while they are read.
pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Result<Self, String> {
        let dir =
            std::env::temp_dir().join(format!("dbine-{tag}-{}", uuid::Uuid::new_v4().simple()));
        let mut b = std::fs::DirBuilder::new();
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
        b.create(&dir)
            .map_err(|e| format!("no se pudo preparar la carpeta temporal: {e}"))?;
        // Canonical (macOS' temp folder is under a symlink): DuckDB's
        // `allowed_paths` compares the real path.
        let dir = std::fs::canonicalize(&dir)
            .map(super::files::plain_path)
            .unwrap_or(dir);
        Ok(TempDir(dir))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A key's segments as a relative path: each one kept when it's a plain
/// name (so `año=2026/` stays a Hive partition and `.csv.gz` its
/// compression), anything else replaced by `_`.
fn local_name(key: &str) -> PathBuf {
    let mut p = PathBuf::new();
    for seg in key.split('?').next().unwrap_or(key).split('/') {
        let s: String = seg
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '=' | '+' | ' ' | ',' | '@')
                {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let s = if s.is_empty() || s.chars().all(|c| c == '.') {
            "_".to_string()
        } else {
            s
        };
        p.push(s);
    }
    p
}

/// Download `files` (approved objects, [`expand`]) into `dir`: each one
/// below `dir/<n>/` with its key's path. At most [`MAX_OBJECT_BYTES`] each
/// and [`MAX_CALL_BYTES`] in all, whatever the server announced.
pub async fn download(files: &[DataFile], net: &Net, dir: &Path) -> CommandResult<Vec<String>> {
    use tokio::io::AsyncWriteExt;
    let mut session = Session::new(net);
    let mut total: u64 = 0;
    let mut out = Vec::new();
    for (i, f) in files.iter().enumerate() {
        let u = parse(&f.path, net)
            .map_err(CommandError::BadRequest)?
            .ok_or_else(|| CommandError::BadRequest(format!("«{}» no es una URL", f.path)))?;
        if u.glob {
            return Err(CommandError::BadRequest(format!(
                "«{}» es un patrón, no un archivo",
                f.path
            )));
        }
        let resp = session
            .request(&u, &u.key, reqwest::Method::GET)
            .await
            .map_err(CommandError::BadRequest)?;
        session
            .check(&u, &u.canonical, &resp)
            .await
            .map_err(CommandError::BadRequest)?;
        if resp.content_length().is_some_and(|n| n > MAX_OBJECT_BYTES) {
            return Err(CommandError::BadRequest(format!(
                "«{}» pesa más de {} GB, el máximo por archivo",
                u.canonical,
                MAX_OBJECT_BYTES >> 30
            )));
        }
        let path = dir.join(i.to_string()).join(local_name(&u.key));
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)
                .map_err(|e| CommandError::Internal(format!("carpeta temporal: {e}")))?;
        }
        let mut file = tokio::fs::File::create(&path)
            .await
            .map_err(|e| CommandError::Internal(format!("archivo temporal: {e}")))?;
        let mut resp = resp;
        let mut size: u64 = 0;
        while let Some(chunk) = resp.chunk().await.map_err(|e| {
            CommandError::BadRequest(format!(
                "se cortó la descarga de «{}»: {}",
                u.canonical,
                e.without_url()
            ))
        })? {
            size += chunk.len() as u64;
            total += chunk.len() as u64;
            if size > MAX_OBJECT_BYTES {
                return Err(CommandError::BadRequest(format!(
                    "«{}» pesa más de {} GB, el máximo por archivo",
                    u.canonical,
                    MAX_OBJECT_BYTES >> 30
                )));
            }
            if total > MAX_CALL_BYTES {
                return Err(CommandError::BadRequest(format!(
                    "los archivos de «{}» pesan más de {} GB en total",
                    u.canonical,
                    MAX_CALL_BYTES >> 30
                )));
            }
            file.write_all(&chunk)
                .await
                .map_err(|e| CommandError::Internal(format!("archivo temporal: {e}")))?;
        }
        file.flush()
            .await
            .map_err(|e| CommandError::Internal(format!("archivo temporal: {e}")))?;
        out.push(path.to_string_lossy().to_string());
    }
    Ok(out)
}

#[cfg(test)]
#[path = "remote_tests.rs"]
mod tests;
