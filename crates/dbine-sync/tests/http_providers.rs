//! Google Drive and OneDrive clients against a small in-process imitation
//! of each API (files by name in the app folder, bearer tokens, token
//! refresh after a 401), then a full sync through them.

use dbine_sync::gdrive::GoogleDrive;
use dbine_sync::oauth::{OAuthConfig, TokenSource, Tokens};
use dbine_sync::onedrive::OneDrive;
use dbine_sync::CloudStore;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[derive(Default)]
struct Fake {
    /// name → (id, content, version)
    files: HashMap<String, (String, Vec<u8>, u64)>,
    valid_token: String,
    refreshes: u32,
    log: Vec<String>,
}

struct Req {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

type Handler = fn(&mut Fake, &Req) -> (u16, Vec<u8>);

async fn serve(handler: Handler, fake: Arc<Mutex<Fake>>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = l.accept().await.unwrap();
            let fake = fake.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 8192];
                let head_end = loop {
                    let n = s.read(&mut tmp).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break p + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut lines = head.split("\r\n");
                let mut first = lines.next().unwrap().split(' ');
                let method = first.next().unwrap().to_string();
                let target = first.next().unwrap().to_string();
                let headers: HashMap<String, String> = lines
                    .filter_map(|l| l.split_once(": ").map(|(k, v)| (k.to_ascii_lowercase(), v.to_string())))
                    .collect();
                let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                let mut body = buf[head_end..].to_vec();
                while body.len() < len {
                    let n = s.read(&mut tmp).await.unwrap();
                    body.extend_from_slice(&tmp[..n]);
                }
                let url = url::Url::parse(&format!("http://x{target}")).unwrap();
                let req = Req {
                    method,
                    path: url.path().to_string(),
                    query: url.query_pairs().into_owned().collect(),
                    headers,
                    body,
                };
                let (status, out) = {
                    let mut f = fake.lock().unwrap();
                    f.log.push(format!("{} {}", req.method, req.path));
                    if req.path == "/token" {
                        f.refreshes += 1;
                        f.valid_token = format!("tok{}", f.refreshes);
                        let t = f.valid_token.clone();
                        (200, format!("{{\"access_token\":\"{t}\",\"expires_in\":3600}}").into_bytes())
                    } else if req.headers.get("authorization") != Some(&format!("Bearer {}", f.valid_token)) {
                        (401, b"{\"error\":{\"message\":\"bad token\"}}".to_vec())
                    } else {
                        handler(&mut f, &req)
                    }
                };
                let resp = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", out.len());
                let _ = s.write_all(resp.as_bytes()).await;
                let _ = s.write_all(&out).await;
                let _ = s.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

fn drive_json(id: &str, f: &(String, Vec<u8>, u64)) -> serde_json::Value {
    serde_json::json!({ "id": id, "modifiedTime": "2026-01-01T00:00:00Z", "size": f.1.len().to_string(), "version": f.2.to_string() })
}

fn drive(f: &mut Fake, r: &Req) -> (u16, Vec<u8>) {
    let json = |v: serde_json::Value| (200, v.to_string().into_bytes());
    match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/drive/v3/about") => json(serde_json::json!({ "user": { "emailAddress": "yo@example.com" } })),
        ("GET", "/drive/v3/files") => {
            assert_eq!(r.query["spaces"], "appDataFolder");
            let name = r.query["q"].split('\'').nth(1).unwrap().to_string();
            let files: Vec<_> = f.files.get(&name).map(|x| drive_json(&x.0, x)).into_iter().collect();
            json(serde_json::json!({ "files": files }))
        }
        ("POST", "/upload/drive/v3/files") => {
            assert_eq!(r.query["uploadType"], "multipart");
            let body = String::from_utf8_lossy(&r.body).to_string();
            assert!(body.contains("\"parents\":[\"appDataFolder\"]"), "{body}");
            let boundary = r.headers["content-type"].split("boundary=").nth(1).unwrap().to_string();
            let parts: Vec<&str> = body.split(&format!("--{boundary}")).collect();
            let meta: serde_json::Value = serde_json::from_str(parts[1].split("\r\n\r\n").nth(1).unwrap().trim()).unwrap();
            let content = parts[2].split_once("\r\n\r\n").unwrap().1.trim_end_matches("\r\n").as_bytes().to_vec();
            let name = meta["name"].as_str().unwrap().to_string();
            let id = format!("id-{name}");
            f.files.insert(name.clone(), (id.clone(), content, 1));
            json(drive_json(&id, &f.files[&name]))
        }
        ("PATCH", p) if p.starts_with("/upload/drive/v3/files/") => {
            let id = p.rsplit('/').next().unwrap();
            let (name, entry) = f.files.iter_mut().find(|(_, v)| v.0 == id).unwrap();
            entry.1 = r.body.clone();
            entry.2 += 1;
            let name = name.clone();
            json(drive_json(id, &f.files[&name]))
        }
        ("GET", p) if p.starts_with("/drive/v3/files/") => {
            assert_eq!(r.query["alt"], "media");
            let id = p.rsplit('/').next().unwrap();
            match f.files.values().find(|v| v.0 == id) {
                Some(v) => (200, v.1.clone()),
                None => (404, b"{}".to_vec()),
            }
        }
        ("DELETE", p) if p.starts_with("/drive/v3/files/") => {
            let id = p.rsplit('/').next().unwrap().to_string();
            f.files.retain(|_, v| v.0 != id);
            (204, vec![])
        }
        _ => (400, format!("unexpected {} {}", r.method, r.path).into_bytes()),
    }
}

fn graph(f: &mut Fake, r: &Req) -> (u16, Vec<u8>) {
    let prefix = "/me/drive/special/approot:/";
    let item = |x: &(String, Vec<u8>, u64)| {
        serde_json::json!({ "cTag": format!("c{}", x.2), "eTag": "e", "lastModifiedDateTime": "2026-01-01T00:00:00Z", "size": x.1.len() })
            .to_string()
            .into_bytes()
    };
    if r.path == "/me" {
        return (200, b"{\"displayName\":\"Yo\",\"mail\":null,\"userPrincipalName\":\"yo@outlook.com\"}".to_vec());
    }
    let Some(rest) = r.path.strip_prefix(prefix) else { return (400, vec![]) };
    let (name, content) = match rest.strip_suffix(":/content") {
        Some(n) => (n.to_string(), true),
        None => (rest.to_string(), false),
    };
    match (r.method.as_str(), content) {
        ("GET", false) => f.files.get(&name).map(|x| (200, item(x))).unwrap_or((404, b"{}".to_vec())),
        ("GET", true) => f.files.get(&name).map(|x| (200, x.1.clone())).unwrap_or((404, b"{}".to_vec())),
        ("PUT", true) => {
            let v = f.files.get(&name).map(|x| x.2 + 1).unwrap_or(1);
            f.files.insert(name.clone(), (name.clone(), r.body.clone(), v));
            (200, item(&f.files[&name]))
        }
        ("DELETE", false) => {
            f.files.remove(&name);
            (204, vec![])
        }
        _ => (400, vec![]),
    }
}

fn token_source(base: &str, provider: dbine_sync::ProviderKind) -> (Arc<TokenSource>, Arc<Mutex<Vec<Tokens>>>) {
    let mut cfg = match provider {
        dbine_sync::ProviderKind::GoogleDrive => OAuthConfig::google("cid", Some("sec".into())),
        _ => OAuthConfig::microsoft("cid"),
    };
    cfg.token_url = format!("{base}/token");
    let saved = Arc::new(Mutex::new(Vec::new()));
    let s2 = saved.clone();
    // An expired access token: the first call refreshes.
    let tokens = Tokens { access_token: "stale".into(), refresh_token: Some("r1".into()), expires_at: 0 };
    let src = TokenSource::new(cfg, reqwest::Client::new(), tokens, Arc::new(move |t: &Tokens| s2.lock().unwrap().push(t.clone())));
    (Arc::new(src), saved)
}

async fn exercise(store: &dyn CloudStore, fake: &Arc<Mutex<Fake>>) {
    assert!(store.stat("a.json").await.unwrap().is_none());
    assert!(store.download("a.json").await.unwrap().is_none());
    let m1 = store.upload("a.json", b"uno".to_vec()).await.unwrap();
    assert_eq!(store.download("a.json").await.unwrap().unwrap(), b"uno");
    let m2 = store.upload("a.json", b"dos".to_vec()).await.unwrap();
    assert_ne!(m1.revision, m2.revision);
    assert_eq!(store.stat("a.json").await.unwrap().unwrap().revision, m2.revision);
    assert_eq!(store.download("a.json").await.unwrap().unwrap(), b"dos");
    // The server revokes the token mid-session: refresh once and retry.
    fake.lock().unwrap().valid_token = "rotated-elsewhere".into();
    assert!(store.stat("a.json").await.unwrap().is_some());
    store.delete("a.json").await.unwrap();
    assert!(store.stat("a.json").await.unwrap().is_none());
}

#[tokio::test]
async fn google_drive_app_folder() {
    let fake = Arc::new(Mutex::new(Fake::default()));
    let base = serve(drive, fake.clone()).await;
    let (src, saved) = token_source(&base, dbine_sync::ProviderKind::GoogleDrive);
    let g = GoogleDrive::with_api(src, &base);
    assert_eq!(g.account().await.unwrap(), "yo@example.com");
    // Google doesn't send a new refresh token: the old one is kept.
    assert_eq!(saved.lock().unwrap()[0].refresh_token.as_deref(), Some("r1"));
    exercise(&g, &fake).await;
}

#[tokio::test]
async fn onedrive_app_folder() {
    let fake = Arc::new(Mutex::new(Fake::default()));
    let base = serve(graph, fake.clone()).await;
    let (src, _) = token_source(&base, dbine_sync::ProviderKind::Onedrive);
    let o = OneDrive::with_api(src, &base);
    assert_eq!(o.account().await.unwrap(), "yo@outlook.com");
    exercise(&o, &fake).await;
    let log = fake.lock().unwrap().log.join("\n");
    assert!(log.contains("PUT /me/drive/special/approot:/a.json:/content"), "{log}");
}

#[tokio::test]
async fn a_401_refreshes_the_token_and_retries() {
    let fake = Arc::new(Mutex::new(Fake::default()));
    let base = serve(graph, fake.clone()).await;
    let (src, saved) = token_source(&base, dbine_sync::ProviderKind::Onedrive);
    let o = OneDrive::with_api(src, &base);
    o.upload("x.json", b"1".to_vec()).await.unwrap();
    assert_eq!(fake.lock().unwrap().refreshes, 1);
    // The token stops working before it expires (revoked, clock skew).
    fake.lock().unwrap().valid_token = "tok-new".into();
    fake.lock().unwrap().refreshes = 99;
    // The retry refreshes to "tok100", which the fake then accepts.
    assert_eq!(o.download("x.json").await.unwrap().unwrap(), b"1");
    assert_eq!(saved.lock().unwrap().len(), 2);
}
