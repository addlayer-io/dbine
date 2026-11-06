//! Sessions of the company's connections, through DBine Bastion: a
//! WebSocket per driver crate (`/v1/<package>`), end to end encrypted inside
//! (channel.rs: Noise with this computer's key and Bastion's pinned key, zstd
//! for big frames), carrying the same frames a local driver host reads and
//! writes. Bastion swaps in the profile's server and account, so the config
//! sent from here names only the engine and the profile. Long jobs (exports,
//! migrations: dedicated sessions) get a stream of their own, so they don't
//! hold up the tabs' queries.

use super::{channel, get, PROFILE_OPTION};
use crate::error::{CommandError, CommandResult};
use base64::Engine;
use dashmap::DashMap;
use dbine_driver::{ConnectionConfig, Driver, Error, Result, Session};
use dbine_plugin::proto::{Call, Reply};
use dbine_plugin::remote::{Dialer, Launcher, RemoteDriver};
use futures_util::{SinkExt, StreamExt};
use std::io::{Read, Write};
use std::sync::{Arc, OnceLock};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

/// Frames waiting for the driver's reader: a full buffer holds the stream
/// back (and Bastion, and the database's cursor) instead of growing.
const READ_AHEAD: usize = 256;

/// One driver per (Bastion, package, engine), with its stream.
fn drivers() -> &'static DashMap<String, Arc<RemoteDriver>> {
    static D: OnceLock<DashMap<String, Arc<RemoteDriver>>> = OnceLock::new();
    D.get_or_init(DashMap::new)
}

/// Signed out or another account: the streams go with their drivers.
pub fn forget_drivers() {
    drivers().clear();
}

/// Where and how to reach Bastion.
#[derive(Clone)]
pub(crate) struct Target {
    pub url: String,
    pub bastion_key: Vec<u8>,
    pub device_private: Vec<u8>,
    /// A fixed token (tests); None takes the signed-in one each time.
    pub token: Option<String>,
}

fn target() -> CommandResult<Target> {
    let e = get()?;
    let (url, key) = e.bastion().ok_or_else(|| {
        CommandError::Connect("DBine Enterprise todavía no dijo dónde está DBine Bastion".into())
    })?;
    Ok(Target {
        url,
        bastion_key: base64::engine::general_purpose::STANDARD
            .decode(key)
            .map_err(|_| CommandError::Internal("clave de DBine Bastion inválida".into()))?,
        device_private: super::device_key()?.0,
        token: None,
    })
}

/// Opens a session of a managed connection through Bastion. `dedicated`:
/// a long job's session, on a stream of its own.
pub async fn open_session(
    cfg: &ConnectionConfig,
    database: Option<&str>,
    dedicated: bool,
) -> CommandResult<Box<dyn Session>> {
    let e = get()?;
    let profile = cfg
        .option(PROFILE_OPTION)
        .ok_or_else(|| CommandError::BadRequest("no es una conexión de DBine Enterprise".into()))?;
    let package = e.package_of(profile).ok_or_else(|| {
        CommandError::AuthFailed("ya no tenés acceso a esta conexión de DBine Enterprise".into())
    })?;
    let t = target()?;
    let driver = if dedicated {
        Arc::new(driver_for(&t, &package, &cfg.driver).await?)
    } else {
        let key = format!("{}\u{0}{package}\u{0}{}", t.url, cfg.driver);
        match drivers().get(&key).map(|d| d.clone()) {
            Some(d) => d,
            None => {
                let d = Arc::new(driver_for(&t, &package, &cfg.driver).await?);
                drivers().insert(key, d.clone());
                d
            }
        }
    };
    // Only the engine and the profile travel: Bastion fills the rest.
    let wire = ConnectionConfig {
        driver: cfg.driver.clone(),
        read_only: cfg.read_only,
        options: [(PROFILE_OPTION.to_string(), profile.to_string())].into(),
        ..Default::default()
    };
    Ok(driver.connect(&wire, database).await?)
}

/// The engine's driver as Bastion's host describes it.
pub(crate) async fn driver_for(t: &Target, package: &str, engine: &str) -> Result<RemoteDriver> {
    let launcher = Launcher::dialed(package, Arc::new(BastionDialer(t.clone())));
    let host = launcher.get().await?;
    let metas = match host.call(Call::Manifest).await? {
        Reply::Manifest(m) => m,
        _ => return Err(Error::State("respuesta inesperada de DBine Bastion".into())),
    };
    let meta = metas
        .into_iter()
        .find(|m| m.info.id == engine)
        .ok_or_else(|| {
            Error::Unsupported(format!("DBine Bastion no tiene el driver «{engine}»"))
        })?;
    Ok(RemoteDriver::new(meta, launcher))
}

struct BastionDialer(Target);

#[async_trait::async_trait]
impl Dialer for BastionDialer {
    async fn dial(&self, package: &str) -> Result<(Box<dyn Write + Send>, Box<dyn Read + Send>)> {
        let t = &self.0;
        let token = match &t.token {
            Some(x) => x.clone(),
            None => get()
                .map_err(|e| Error::Connect(e.to_string()))?
                .bearer()
                .await
                .map_err(|e| Error::AuthFailed(e.to_string()))?,
        };
        let ws_url = format!("{}/v1/{package}", t.url.trim_end_matches('/'));
        let mut req = ws_url
            .into_client_request()
            .map_err(|e| Error::Connect(e.to_string()))?;
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {token}")
                .parse()
                .map_err(|_| Error::Connect("token inválido".into()))?,
        );
        let (mut ws, _) = tokio_tungstenite::connect_async_tls_with_config(
            req,
            None,
            false,
            Some(tokio_tungstenite::Connector::Rustls(tls())),
        )
        .await
        .map_err(|e| match e {
            tokio_tungstenite::tungstenite::Error::Http(r) if r.status() == 401 => {
                Error::AuthFailed("la sesión de DBine Enterprise terminó: volvé a iniciarla".into())
            }
            other => Error::Connect(format!("no se pudo llegar a DBine Bastion: {other}")),
        })?;
        // The end-to-end channel, before anything of the protocol.
        let refused = || {
            Error::AuthFailed(
                "DBine Bastion no aceptó este equipo o no es el Bastion de la empresa: volvé a iniciar sesión".into(),
            )
        };
        let (first, hs) = channel::initiate(&t.device_private, &t.bastion_key)
            .map_err(|e| Error::Connect(e.to_string()))?;
        ws.send(Message::Binary(first.into()))
            .await
            .map_err(|e| Error::Connect(e.to_string()))?;
        let answer = match tokio::time::timeout(std::time::Duration::from_secs(15), ws.next()).await
        {
            Ok(Some(Ok(Message::Binary(b)))) => b,
            _ => return Err(refused()),
        };
        let mut ch = channel::finish(hs, &answer).map_err(|_| refused())?;

        let (to_ws, mut out) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let (into_reader, from_ws) = std::sync::mpsc::sync_channel::<Vec<u8>>(READ_AHEAD);
        let (mut sink, mut stream) = ws.split();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    msg = stream.next() => match msg {
                        Some(Ok(Message::Binary(b))) => {
                            #[cfg(test)]
                            WIRE.fetch_add(b.len() as u64, std::sync::atomic::Ordering::Relaxed);
                            // Decrypting and decompressing an export is CPU
                            // work: off the threads the rest of DBine runs on.
                            let Ok(frames) = heavy(|| ch.open_all(&b)) else { break };
                            // A full reader holds the stream back: it stops
                            // being read and the sender slows down.
                            let mut gone = false;
                            for body in frames {
                                match into_reader.try_send(body) {
                                    Ok(()) => {}
                                    Err(std::sync::mpsc::TrySendError::Full(body)) => {
                                        let tx = into_reader.clone();
                                        let sent = tokio::task::spawn_blocking(move || tx.send(body)).await;
                                        if !matches!(sent, Ok(Ok(()))) {
                                            gone = true;
                                            break;
                                        }
                                    }
                                    Err(_) => {
                                        gone = true;
                                        break;
                                    }
                                }
                            }
                            if gone {
                                break;
                            }
                        }
                        Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                        // Closed, an error or text: the stream ends, and the
                        // host with it (its calls fail, the next one dials).
                        _ => break,
                    },
                    body = out.recv() => match body {
                        Some(b) => {
                            // Whatever else is ready goes in the same message.
                            let mut size = b.len();
                            let mut batch = vec![b];
                            while size < channel::PACK {
                                match out.try_recv() {
                                    Ok(more) => {
                                        size += more.len();
                                        batch.push(more);
                                    }
                                    Err(_) => break,
                                }
                            }
                            let Ok(sealed) = heavy(|| ch.seal_all(&batch)) else { break };
                            if sink.send(Message::Binary(sealed.into())).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    },
                }
            }
            let _ = sink.close().await;
        });
        Ok((
            Box::new(FrameWriter {
                buf: Vec::new(),
                out: to_ws,
            }),
            Box::new(FrameReader {
                from: from_ws,
                cur: Vec::new(),
                pos: 0,
            }),
        ))
    }
}

/// The server's certificate checked like the OS does (a company CA works).
pub(super) fn tls() -> Arc<rustls::ClientConfig> {
    use rustls_platform_verifier::BuilderVerifierExt;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    Arc::new(
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("TLS versions")
            .with_platform_verifier()
            .expect("the platform's verifier")
            .with_no_client_auth(),
    )
}

/// Takes length-prefixed frames, sends each body as one WebSocket message.
struct FrameWriter {
    buf: Vec<u8>,
    out: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
}

impl Write for FrameWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        while self.buf.len() >= 4 {
            let len = u32::from_le_bytes(self.buf[..4].try_into().expect("4 bytes")) as usize;
            if self.buf.len() < 4 + len {
                break;
            }
            let body: Vec<u8> = self.buf[4..4 + len].to_vec();
            self.buf.drain(..4 + len);
            self.out
                .send(body)
                .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gateway"))?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Gives each WebSocket message back as a length-prefixed frame.
struct FrameReader {
    from: std::sync::mpsc::Receiver<Vec<u8>>,
    cur: Vec<u8>,
    pos: usize,
}

/// Frame bytes received from Bastion, and bytes on the wire (encrypted,
/// compressed), for the benchmark.
#[cfg(test)]
static RECEIVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
#[cfg(test)]
static WIRE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Read for FrameReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.cur.len() {
            let Ok(body) = self.from.recv() else {
                return Ok(0);
            };
            #[cfg(test)]
            RECEIVED.fetch_add(body.len() as u64, std::sync::atomic::Ordering::Relaxed);
            self.cur.clear();
            self.cur
                .extend_from_slice(&(body.len() as u32).to_le_bytes());
            self.cur.extend_from_slice(&body);
            self.pos = 0;
        }
        let n = out.len().min(self.cur.len() - self.pos);
        out[..n].copy_from_slice(&self.cur[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// CPU work (crypto, compression) off the threads that serve other streams,
/// where the runtime allows it.
fn heavy<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(f),
        _ => f(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_plugin::proto::{read_frame, write_frame, ToHost};

    /// Bastion as the tests reach it: DBE_E2E_BASTION (ws://…), its key
    /// DBE_E2E_BASTION_KEY, this test computer's DBE_E2E_DEVICE_PRIVATE
    /// (base64) and a token bound to it, DBE_E2E_TOKEN.
    fn e2e_target() -> Target {
        let b64 = base64::engine::general_purpose::STANDARD;
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
        Target {
            url: env("DBE_E2E_BASTION"),
            bastion_key: b64.decode(env("DBE_E2E_BASTION_KEY")).unwrap(),
            device_private: b64.decode(env("DBE_E2E_DEVICE_PRIVATE")).unwrap(),
            token: Some(env("DBE_E2E_TOKEN")),
        }
    }

    /// Against a running DBine Enterprise (`cargo test -- --ignored`):
    /// DBE_E2E_SERVER, DBE_E2E_TOKEN (a DBine token), DBE_E2E_PROFILE (a
    /// profile it has access to), DBE_E2E_PACKAGE and DBE_E2E_DRIVER, and
    /// DBE_E2E_SQL with DBE_E2E_ROWS, what it returns.
    #[tokio::test]
    #[ignore]
    async fn a_session_through_a_real_gateway() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("set {k}"));
        let driver = driver_for(
            &e2e_target(),
            &env("DBE_E2E_PACKAGE"),
            &env("DBE_E2E_DRIVER"),
        )
        .await
        .unwrap();
        let cfg = ConnectionConfig {
            driver: env("DBE_E2E_DRIVER"),
            options: [(PROFILE_OPTION.to_string(), env("DBE_E2E_PROFILE"))].into(),
            ..Default::default()
        };
        let mut s = driver.connect(&cfg, None).await.unwrap();
        let mut out = dbine_driver::QueryOutcome::default();
        s.execute(&env("DBE_E2E_SQL"), 100, &mut out).await.unwrap();
        let rows: u64 = out.results.iter().map(|r| r.total_rows).sum();
        assert_eq!(rows.to_string(), env("DBE_E2E_ROWS"));
        // A profile it has no access to: refused by the gateway.
        let other = ConnectionConfig {
            driver: env("DBE_E2E_DRIVER"),
            options: [(PROFILE_OPTION.to_string(), "not-a-profile".to_string())].into(),
            ..Default::default()
        };
        assert!(matches!(
            driver.connect(&other, None).await,
            Err(Error::AuthFailed(_))
        ));
    }

    #[test]
    fn frames_cross_as_one_message_each_and_come_back_prefixed() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut w = FrameWriter {
            buf: Vec::new(),
            out: tx,
        };
        let a = ToHost::Close { session: 7 };
        let b = ToHost::Cancel { session: 9 };
        // Written in odd pieces, as a BufWriter may.
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &a).unwrap();
        write_frame(&mut bytes, &b).unwrap();
        for chunk in bytes.chunks(3) {
            w.write_all(chunk).unwrap();
        }
        let m1 = rx.try_recv().unwrap();
        let m2 = rx.try_recv().unwrap();
        assert!(rx.try_recv().is_err());
        assert!(matches!(
            rmp_serde::from_slice(&m1).unwrap(),
            ToHost::Close { session: 7 }
        ));

        let (stx, srx) = std::sync::mpsc::channel();
        stx.send(m1).unwrap();
        stx.send(m2).unwrap();
        drop(stx);
        let mut r = FrameReader {
            from: srx,
            cur: Vec::new(),
            pos: 0,
        };
        let x: ToHost = read_frame(&mut r).unwrap().unwrap();
        let y: ToHost = read_frame(&mut r).unwrap().unwrap();
        assert!(matches!(x, ToHost::Close { session: 7 }));
        assert!(matches!(y, ToHost::Cancel { session: 9 }));
        // The socket closed: the host's reader sees the end.
        assert!(read_frame::<ToHost>(&mut r).unwrap().is_none());
    }

    /// Counts what an export would write.
    struct Count(u64);
    impl dbine_driver::RowSink for Count {
        fn begin(&mut self, _: usize, _: &[dbine_driver::ResultColumn]) -> std::io::Result<()> {
            Ok(())
        }
        fn row(&mut self, _: usize, _: &[serde_json::Value]) -> std::io::Result<()> {
            self.0 += 1;
            Ok(())
        }
    }

    /// Reads all of `sql` as DBine's export does (rows into a sink).
    async fn export(s: &mut Box<dyn Session>, sql: &str) -> (u64, f64) {
        let sink = Arc::new(std::sync::Mutex::new(Count(0)));
        let mut out = dbine_driver::QueryOutcome {
            sink: Some(dbine_driver::RowSinkRef(sink.clone())),
            ..Default::default()
        };
        let t = std::time::Instant::now();
        s.execute(sql, 100, &mut out).await.unwrap();
        assert!(out.error.is_none(), "{:?}", out.error);
        let rows = sink.lock().unwrap().0;
        (rows, t.elapsed().as_secs_f64())
    }

    fn env(k: &str) -> String {
        std::env::var(k).unwrap_or_else(|_| panic!("set {k}"))
    }

    async fn through_gateway() -> Box<dyn Session> {
        let d = driver_for(&e2e_target(), "postgres", "postgres")
            .await
            .unwrap();
        let cfg = ConnectionConfig {
            driver: "postgres".into(),
            read_only: true,
            options: [(PROFILE_OPTION.to_string(), env("BENCH_PROFILE"))].into(),
            ..Default::default()
        };
        d.connect(&cfg, None).await.unwrap()
    }

    /// The baseline: one export of the whole table directly and through the
    /// gateway, then BENCH_USERS users exporting BENCH_ROWS rows each at once
    /// while another user runs small queries. `cargo test -- --ignored
    /// --nocapture bench_`; needs BENCH_PG_URL-style pieces for the direct
    /// run (BENCH_PG_HOST, BENCH_PG_PORT, BENCH_PG_USER, BENCH_PG_PASSWORD).
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    #[ignore]
    async fn bench_exports_direct_and_through_the_gateway() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let all = "SELECT * FROM t";
        // Directly.
        let direct = ConnectionConfig {
            driver: "postgres".into(),
            host: env("BENCH_PG_HOST"),
            port: env("BENCH_PG_PORT").parse().unwrap(),
            username: Some(env("BENCH_PG_USER")),
            password: Some(env("BENCH_PG_PASSWORD")),
            database: "bench".into(),
            options: [("sslmode".to_string(), "disable".to_string())].into(),
            ..Default::default()
        };
        let mut s = dbine_drivers::open_session(&direct, None).await.unwrap();
        let (rows, secs) = export(&mut s, all).await;
        println!(
            "BENCH direct: {rows} rows in {secs:.1} s ({:.0} rows/s)",
            rows as f64 / secs
        );
        drop(s);
        // Through the gateway.
        let mut s = through_gateway().await;
        let before = RECEIVED.load(std::sync::atomic::Ordering::Relaxed);
        let wire0 = WIRE.load(std::sync::atomic::Ordering::Relaxed);
        let (rows, secs) = export(&mut s, all).await;
        let bytes = RECEIVED.load(std::sync::atomic::Ordering::Relaxed) - before;
        let wire = WIRE.load(std::sync::atomic::Ordering::Relaxed) - wire0;
        println!(
            "BENCH bastion: {rows} rows in {secs:.1} s ({:.0} rows/s), {:.0} MB of frames, {:.0} MB on the wire ({:.0} B/row)",
            rows as f64 / secs,
            bytes as f64 / 1e6,
            wire as f64 / 1e6,
            wire as f64 / rows as f64
        );
        drop(s);
        // Many users at once, and one with small queries.
        let users: usize = std::env::var("BENCH_USERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);
        let each: u64 = std::env::var("BENCH_ROWS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1_000_000);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe = {
            let stop = stop.clone();
            tokio::spawn(async move {
                let mut s = through_gateway().await;
                let mut lat = Vec::new();
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let t = std::time::Instant::now();
                    let mut out = dbine_driver::QueryOutcome::default();
                    s.execute("SELECT count(*) FROM t WHERE id = 42", 10, &mut out)
                        .await
                        .unwrap();
                    lat.push(t.elapsed().as_secs_f64() * 1000.0);
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
                lat
            })
        };
        let t = std::time::Instant::now();
        let runs: Vec<_> = (0..users)
            .map(|i| {
                tokio::spawn(async move {
                    let mut s = through_gateway().await;
                    let sql = format!(
                        "SELECT * FROM t WHERE id > {} LIMIT {each}",
                        i as u64 * 7919
                    );
                    export(&mut s, &sql).await
                })
            })
            .collect();
        let mut total = 0;
        for r in runs {
            total += r.await.unwrap().0;
        }
        let secs = t.elapsed().as_secs_f64();
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let lat = probe.await.unwrap();
        let pct = |p: f64| lat[((lat.len() as f64 - 1.0) * p) as usize];
        println!(
            "BENCH {users} users x {each} rows: {total} rows in {secs:.1} s ({:.0} rows/s overall); small query ms p50 {:.0} p95 {:.0} max {:.0} ({} probes)",
            total as f64 / secs,
            pct(0.5),
            pct(0.95),
            lat.last().unwrap(),
            lat.len()
        );
    }
}
