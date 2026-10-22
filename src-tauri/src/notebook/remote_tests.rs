//! Tests for files from the web and cloud storage in "DataFrame SQL" cells
//! (`notebook::remote`): local HTTP servers stand for a web server, S3,
//! Google Cloud Storage and Azure Blob Storage (no MinIO container: the S3
//! one is a mock that checks the SigV4 header's shape).

use super::*;
use crate::notebook::access::{grant_targets, Access, Gate};
use crate::notebook::files::plan_with;
use crate::notebook::local::{
    existing_tables, open_sandboxed, open_sandboxed_with, prepare, Fetched, Loaded, LoadedTable,
    Prepared,
};
use axum::body::Body;
use axum::http::{HeaderMap, Request};
use dbine_driver::{QueryOutcome, Session as DuckSession};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

// -- a local server -------------------------------------------------------------------------------

/// What the server answers: status (999: wait 30 s first), headers, body.
type Reply = (u16, Vec<(String, String)>, Vec<u8>);
type Handler = Arc<dyn Fn(&str, &str, &str, &HeaderMap) -> Reply + Send + Sync>;

struct Server {
    base: String,
    port: u16,
    hits: Arc<Mutex<Vec<String>>>,
}

impl Server {
    fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
}

async fn serve(h: Handler) -> Server {
    let hits: Arc<Mutex<Vec<String>>> = Arc::default();
    let hits2 = hits.clone();
    let app = axum::Router::new().fallback(move |req: Request<Body>| {
        let (h, hits) = (h.clone(), hits2.clone());
        async move {
            let m = req.method().as_str().to_string();
            let path = req.uri().path().to_string();
            let q = req.uri().query().unwrap_or("").to_string();
            hits.lock().unwrap().push(format!("{m} {path}"));
            let (mut st, hs, body) = h(&m, &path, &q, req.headers());
            if st == 999 {
                tokio::time::sleep(Duration::from_secs(30)).await;
                st = 200;
            }
            let mut b = axum::http::Response::builder().status(st);
            for (k, v) in hs {
                b = b.header(k, v);
            }
            b.body(Body::from(body)).unwrap()
        }
    });
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Server {
        base: format!("http://{addr}"),
        port: addr.port(),
        hits,
    }
}

fn query(q: &str, k: &str) -> Option<String> {
    Url::parse(&format!("http://x/?{q}"))
        .unwrap()
        .query_pairs()
        .find(|(a, _)| a == k)
        .map(|(_, v)| v.to_string())
}

fn ok(body: &str, etag: &str) -> Reply {
    (
        200,
        vec![
            ("etag".into(), format!("\"{etag}\"")),
            (
                "last-modified".into(),
                "Wed, 14 Oct 2026 10:00:00 GMT".into(),
            ),
        ],
        body.as_bytes().to_vec(),
    )
}

fn not_found() -> Reply {
    (404, vec![], Vec::new())
}

/// A test network: loopback and http allowed (the servers are local).
fn net() -> Net {
    Net {
        test: Some(TestNet {
            allow_local: true,
            http: true,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn never(_: usize) -> std::future::Ready<CommandResult<Fetched>> {
    std::future::ready(Err(CommandError::Internal("no".into())))
}

/// [`prepare`] as a user who approves what it asks about: the cell's SQL
/// rewritten, its tables, and what it asked.
async fn run(
    s: &mut dyn DuckSession,
    tables: &mut HashMap<String, Loaded>,
    sql: &str,
    net: &Net,
) -> CommandResult<(String, Vec<LoadedTable>, Vec<DataFile>)> {
    let present = existing_tables(s).await.unwrap();
    let mut access = Access {
        net: net.clone(),
        ..Default::default()
    };
    let mut asked = Vec::new();
    for _ in 0..2 {
        match prepare(
            s,
            tables,
            &present,
            sql,
            &[],
            1_000,
            None,
            &access,
            &mut QueryOutcome::default(),
            None,
            never,
        )
        .await?
        {
            Prepared::Ready(sql, loaded) => return Ok((sql, loaded, asked)),
            Prepared::Ask(files) => {
                access.approved = files
                    .iter()
                    .flat_map(|f| f.all())
                    .map(|f| f.path.clone())
                    .collect();
                asked = files;
            }
        }
    }
    panic!("asked twice")
}

async fn rows_of(s: &mut dyn DuckSession, sql: &str) -> Vec<Vec<Value>> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 10_000, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    assert!(out.error.is_none(), "{sql}: {:?}", out.error);
    out.results
        .into_iter()
        .find(|r| !r.columns.is_empty())
        .map(|r| r.rows)
        .unwrap_or_default()
}

/// The error of running `sql` on `s`, if any.
async fn error_of(s: &mut dyn DuckSession, sql: &str) -> Option<String> {
    let mut out = QueryOutcome::default();
    match s.execute(sql, 100, &mut out).await {
        Err(e) => Some(e.to_string()),
        Ok(()) => out
            .error
            .or_else(|| out.errors.first().map(|e| e.message.clone())),
    }
}

const CSV: &str = "id,nombre\n1,Ana\n2,Luis\n";

// -- what is refused before anything goes over the network ----------------------------------------

#[test]
fn urls_are_parsed_to_their_canonical_form() {
    let n = Net {
        azure_account: Some("cuenta1".into()),
        ..Default::default()
    };
    let p = |s: &str| parse(s, &n).unwrap().unwrap();
    let u = p("https://Example.COM:443/datos/ventas.csv#x");
    assert_eq!(
        (u.cloud, u.canonical.as_str(), u.host.as_str(), u.glob),
        (
            Cloud::Https,
            "https://example.com/datos/ventas.csv",
            "example.com",
            false
        )
    );
    assert_eq!(
        p("https://example.com:8443/a/../b.csv?region=sur").canonical,
        "https://example.com:8443/b.csv?region=sur"
    );
    let u = p("s3://ventas/2026/*.parquet");
    assert_eq!(
        (
            u.cloud,
            u.canonical.as_str(),
            u.bucket.as_str(),
            u.key.as_str(),
            u.glob
        ),
        (
            Cloud::S3,
            "s3://ventas/2026/*.parquet",
            "ventas",
            "2026/*.parquet",
            true
        )
    );
    assert_eq!(
        p("gcs://mi_bucket/a/b.csv").canonical,
        "gs://mi_bucket/a/b.csv"
    );
    assert_eq!(
        p("az://cuenta1.blob.core.windows.net/datos/x/y.csv").canonical,
        "az://cuenta1.blob.core.windows.net/datos/x/y.csv"
    );
    assert_eq!(
        p("abfss://datos@cuenta1.dfs.core.windows.net/x/y.csv").canonical,
        "az://cuenta1.blob.core.windows.net/datos/x/y.csv"
    );
    assert_eq!(
        p("az://datos/x/*.csv").canonical,
        "az://cuenta1.blob.core.windows.net/datos/x/*.csv"
    );
    assert!(parse("/Users/ana/a.csv", &n).unwrap().is_none());
    assert!(parse("C:\\datos\\a.csv", &n).unwrap().is_none());
}

#[test]
fn urls_with_credentials_other_schemes_and_https_globs_are_refused() {
    let n = Net::default();
    for (url, want) in [
        ("http://example.com/a.csv", "sin cifrar"),
        (
            "https://ana:clave@example.com/a.csv",
            "usuario o una contraseña",
        ),
        ("https://ana@example.com/a.csv", "usuario o una contraseña"),
        (
            "https://b.s3.amazonaws.com/a.csv?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Signature=abc",
            "firma o un token",
        ),
        (
            "https://cuenta.blob.core.windows.net/c/a.csv?sv=2022&sig=abc",
            "firma o un token",
        ),
        (
            "https://storage.googleapis.com/b/a.csv?X-Goog-Signature=abc",
            "firma o un token",
        ),
        ("https://example.com/a.csv?token=abc", "firma o un token"),
        ("https://example.com/a.csv?api_key=abc", "firma o un token"),
        ("s3://ventas/a.csv?X-Amz-Signature=abc", "firma o un token"),
        ("s3://AKID:secret@ventas/a.csv", "usuario o una contraseña"),
        ("https://example.com/datos/*.csv", "no puede ser un patrón"),
        ("file:///etc/passwd", "ningún otro esquema"),
        ("hf://datasets/x/y.parquet", "ningún otro esquema"),
        ("ftp://example.com/a.csv", "ningún otro esquema"),
        ("s3://Ventas_Mal/a.csv", "bucket de S3"),
        ("s3://ventas/", "no nombra un archivo"),
        ("s3://ventas/a/../b.csv", "«..»"),
        ("az://datos/x.csv", "AZURE_STORAGE_ACCOUNT"),
        ("abfss://datos@otra.example.com/x.csv", "abfss://"),
    ] {
        let e = parse(url, &n).unwrap_err();
        assert!(e.contains(want), "{url}: {e}");
    }
}

#[test]
fn local_and_private_hosts_are_refused() {
    let n = Net::default();
    for url in [
        "https://localhost/a.csv",
        "https://LOCALHOST./a.csv",
        "https://api.localhost/a.csv",
        "https://impresora.local/a.csv",
        "https://metadata.google.internal/computeMetadata/v1/",
        "https://127.0.0.1/a.csv",
        "https://127.8.0.1/a.csv",
        "https://10.1.2.3/a.csv",
        "https://172.16.0.1/a.csv",
        "https://172.31.255.255/a.csv",
        "https://192.168.1.10/a.csv",
        "https://169.254.169.254/latest/meta-data/x",
        "https://100.64.0.1/a.csv",
        "https://0.0.0.0/a.csv",
        "https://[::1]/a.csv",
        "https://[fd00::1]/a.csv",
        "https://[fe80::1]/a.csv",
        "https://[::ffff:127.0.0.1]/a.csv",
        "https://[::ffff:169.254.169.254]/a.csv",
        "https://[64:ff9b::a9fe:a9fe]/a.csv",
    ] {
        let e = parse(url, &n).unwrap_err();
        assert!(e.contains("servidores públicos"), "{url}: {e}");
    }
    for ip in [
        "8.8.8.8",
        "1.1.1.1",
        "172.32.0.1",
        "2606:4700::1111",
        "100.128.0.1",
    ] {
        assert!(!ip_blocked(ip.parse().unwrap()), "{ip}");
    }
    for ip in [
        "224.0.0.1",
        "255.255.255.255",
        "240.0.0.1",
        "198.18.0.1",
        "fec0::1",
        "ff02::1",
        "::",
        "fd00:ec2::254",
    ] {
        assert!(ip_blocked(ip.parse().unwrap()), "{ip}");
    }
}

#[test]
fn url_patterns_match_segment_by_segment() {
    assert!(url_pattern_matches(
        "s3://ven/2026/*.csv",
        "s3://ven/2026/ene.csv"
    ));
    assert!(!url_pattern_matches(
        "s3://ven/2026/*.csv",
        "s3://ven/2026/sub/ene.csv"
    ));
    assert!(!url_pattern_matches(
        "s3://ven/2026/*.csv",
        "s3://otro/2026/ene.csv"
    ));
    assert!(!url_pattern_matches(
        "s3://ven/2026/*.csv",
        "gs://v/2026/ene.csv"
    ));
    assert!(url_pattern_matches(
        "s3://ven/**/*.parquet",
        "s3://ven/a/b/c.parquet"
    ));
    assert!(url_pattern_matches(
        "s3://ven/**/*.parquet",
        "s3://ven/c.parquet"
    ));
    assert!(url_pattern_matches(
        "gs://v/año=202?/*.csv",
        "gs://v/año=2026/x.csv"
    ));
}

#[test]
fn reader_calls_over_urls_are_planned_without_the_network() {
    let n = net();
    let r = plan_with("SELECT * FROM read_csv('https://example.com/a.csv', header = true) JOIN read_parquet('s3://ven/2026/*.parquet') USING (id)", None, None, 100, &n).unwrap();
    assert_eq!(r.len(), 2);
    assert!(r.iter().all(|r| r.remote));
    assert_eq!(
        r[0].text,
        "read_csv('https://example.com/a.csv', header = true)"
    );
    assert_eq!(r[0].files[0].host.as_deref(), Some("example.com"));
    assert_eq!(r[1].text, "read_parquet(['s3://ven/2026/*.parquet'])");
    assert_eq!(
        r[1].files[0].pattern.as_deref(),
        Some("s3://ven/2026/*.parquet")
    );
    assert_eq!(r[1].files[0].host.as_deref(), Some("ven (Amazon S3)"));
    for (sql, want) in [
        ("SELECT * FROM sqlite_scan('https://example.com/a.db', 't')", "solo archivos locales"),
        ("SELECT * FROM read_duckdb('s3://ven/a.duckdb', 't')", "solo archivos locales"),
        ("SELECT * FROM read_xlsx('https://example.com/a.xlsx')", "solo archivos locales"),
        ("SELECT * FROM read_csv(['https://example.com/a.csv', '/tmp/b.csv'])", "mezclar"),
        ("SELECT * FROM st_read('s3://ven/zonas.geojson')", "solo https://"),
        ("SELECT * FROM st_read('https://example.com/zonas.shp')", "comprimido"),
        ("SELECT * FROM st_read('https://example.com/capas.vrt')", "apuntan a otros archivos"),
        ("SELECT * FROM st_read('https://example.com/z.geojson', open_options = ['FEATURE_SERVER_PAGING=YES'])", "no se puede usar"),
        ("SELECT * FROM st_read('/vsicurl/https://example.com/z.geojson')", "ruta virtual de GDAL"),
        ("SELECT * FROM read_csv('WFS:http://example.com/wfs')", "ni una URL"),
    ] {
        let e = plan_with(sql, None, None, 100, &n).map(|r| r.len()).unwrap_err();
        assert!(e.contains(want), "{sql}: {e}");
    }
}

#[test]
fn a_url_pattern_is_granted_as_written_and_covers_its_objects_only() {
    let pending = DataFile {
        path: "s3://ven/2026/*.csv".into(),
        pattern: Some("s3://ven/2026/*.csv".into()),
        host: Some("ven (Amazon S3)".into()),
        ..Default::default()
    };
    assert_eq!(
        grant_targets(std::slice::from_ref(&pending)),
        vec![("s3://ven/2026/*.csv".to_string(), true)]
    );
    let one = DataFile {
        path: "https://example.com/a.csv".into(),
        host: Some("example.com".into()),
        ..Default::default()
    };
    assert_eq!(
        grant_targets(std::slice::from_ref(&one)),
        vec![("https://example.com/a.csv".to_string(), false)]
    );
    let grant = |t: &str, pattern: bool| dbine_core::file_grants::FileGrant {
        notebook: "nb".into(),
        target: t.into(),
        pattern,
        granted_at: String::new(),
    };
    let acc = Access {
        grants: vec![
            grant("s3://ven/2026/*.csv", true),
            grant("/Users/ana/*", true),
        ],
        ..Default::default()
    };
    assert!(acc.gate(std::slice::from_ref(&pending)).is_ok());
    let obj = |p: &str| DataFile {
        path: p.into(),
        host: Some("x".into()),
        ..Default::default()
    };
    assert!(acc.gate(&[obj("s3://ven/2026/ene.csv")]).is_ok());
    assert!(matches!(
        acc.gate(&[obj("s3://ven/2025/ene.csv")]),
        Err(Gate::Ask(_))
    ));
    assert!(matches!(
        acc.gate(&[obj("s3://otro/2026/ene.csv")]),
        Err(Gate::Ask(_))
    ));
    // A local pattern never covers a URL, and the blocked list is for local files.
    assert!(matches!(
        acc.gate(&[obj("https://example.com/Users/ana/x")]),
        Err(Gate::Ask(_))
    ));
    assert!(matches!(
        acc.gate(&[obj("https://example.com/.env")]),
        Err(Gate::Ask(_))
    ));
}

// -- https -------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_https_file_is_asked_about_before_any_request_then_read_and_cached_by_etag() {
    let etag = Arc::new(Mutex::new(("v1".to_string(), CSV.to_string())));
    let e2 = etag.clone();
    let a = serve(Arc::new(move |_, path, _, _| match path {
        "/datos/a.csv" => {
            let (t, b) = e2.lock().unwrap().clone();
            ok(&b, &t)
        }
        _ => not_found(),
    }))
    .await;
    let n = net();
    let mut s = open_sandboxed().await.unwrap();
    let mut tables = HashMap::new();
    let url = format!("{}/datos/a.csv", a.base);
    let sql = format!("SELECT nombre FROM read_csv('{url}') ORDER BY id");

    // Asked first, with nothing sent to the server.
    let present = existing_tables(s.as_mut()).await.unwrap();
    let access = Access {
        net: n.clone(),
        ..Default::default()
    };
    match prepare(
        s.as_mut(),
        &mut tables,
        &present,
        &sql,
        &[],
        1_000,
        None,
        &access,
        &mut QueryOutcome::default(),
        None,
        never,
    )
    .await
    .unwrap()
    {
        Prepared::Ask(files) => {
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].path, url);
            assert_eq!(
                files[0].host.as_deref(),
                Some(format!("127.0.0.1:{}", a.port).as_str())
            );
        }
        Prepared::Ready(..) => panic!("read without asking"),
    }
    assert!(a.hits().is_empty(), "{:?}", a.hits());

    let (run1, loaded, asked) = run(s.as_mut(), &mut tables, &sql, &n).await.unwrap();
    assert_eq!(asked.len(), 1);
    assert_eq!(
        rows_of(s.as_mut(), &run1).await,
        vec![vec![json!("Ana")], vec![json!("Luis")]]
    );
    let f = &loaded[0].files[0];
    assert_eq!(
        (f.path.as_str(), f.version.as_deref(), f.mtime_ms > 0),
        (url.as_str(), Some("etag:v1"), true)
    );
    assert_eq!(a.hits(), vec!["HEAD /datos/a.csv", "GET /datos/a.csv"]);

    // Same ETag: only a HEAD, the table is reused.
    let (_, loaded, _) = run(s.as_mut(), &mut tables, &sql, &n).await.unwrap();
    assert!(loaded[0].cached);
    assert_eq!(a.hits().len(), 3);

    // A new ETag: read again.
    *etag.lock().unwrap() = ("v2".into(), "id,nombre\n1,Eva\n".into());
    let (run3, loaded, _) = run(s.as_mut(), &mut tables, &sql, &n).await.unwrap();
    assert!(!loaded[0].cached);
    assert_eq!(rows_of(s.as_mut(), &run3).await, vec![vec![json!("Eva")]]);
    assert_eq!(a.hits().last().unwrap(), "GET /datos/a.csv");
}

#[tokio::test]
async fn the_notebook_session_never_reaches_another_host() {
    let a = serve(Arc::new(|_, path, _, _| {
        if path == "/a.csv" {
            ok(CSV, "1")
        } else {
            not_found()
        }
    }))
    .await;
    let b = serve(Arc::new(|_, _, _, _| ok(CSV, "x"))).await;
    let n = net();
    let mut s = open_sandboxed().await.unwrap();
    let mut tables = HashMap::new();
    // A second URL hidden in a named argument: the reader session has no
    // network, the value is only a column name.
    let sql = format!(
        "SELECT * FROM read_csv('{}/a.csv', names = ['{}/b.csv', 'n'])",
        a.base, b.base
    );
    let (run1, _, asked) = run(s.as_mut(), &mut tables, &sql, &n).await.unwrap();
    assert_eq!(asked.len(), 1);
    let rows = rows_of(s.as_mut(), &run1).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    // URLs in the cell's own SQL (not a reader's path) are DuckDB's, and
    // the notebook's session has no network.
    for sql in [
        format!("SELECT * FROM '{}/b.csv'", b.base),
        format!("SELECT * FROM read_text('{}/b.csv')", b.base),
        format!("SELECT * FROM read_blob('{}/b.csv')", b.base),
    ] {
        assert!(error_of(s.as_mut(), &sql).await.is_some(), "{sql} ran");
    }
    assert!(b.hits().is_empty(), "{:?}", b.hits());
}

#[tokio::test]
async fn redirects_are_followed_only_within_the_same_server() {
    let b = serve(Arc::new(|_, _, _, _| ok(CSV, "b"))).await;
    let other = format!("{}/a.csv", b.base);
    let a = serve(Arc::new(move |_, path, _, _| match path {
        "/a.csv" => ok(CSV, "a"),
        "/mismo" => (302, vec![("location".into(), "/a.csv".into())], Vec::new()),
        "/otro" => (302, vec![("location".into(), other.clone())], Vec::new()),
        "/bucle" => (302, vec![("location".into(), "/bucle".into())], Vec::new()),
        _ => not_found(),
    }))
    .await;
    let n = net();
    let mut s = open_sandboxed().await.unwrap();
    let (run1, _, _) = run(
        s.as_mut(),
        &mut HashMap::new(),
        &format!("SELECT count(*) FROM read_csv('{}/mismo')", a.base),
        &n,
    )
    .await
    .unwrap();
    assert_eq!(rows_of(s.as_mut(), &run1).await, vec![vec![json!(2)]]);
    let e = run(
        s.as_mut(),
        &mut HashMap::new(),
        &format!("SELECT * FROM read_csv('{}/otro')", a.base),
        &n,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        e.contains("redirige a") && e.contains("mismo servidor"),
        "{e}"
    );
    let e = run(
        s.as_mut(),
        &mut HashMap::new(),
        &format!("SELECT * FROM read_csv('{}/bucle')", a.base),
        &n,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("demasiadas veces"), "{e}");
    assert!(b.hits().is_empty(), "{:?}", b.hits());
}

#[tokio::test]
async fn a_name_that_resolves_to_this_computer_is_refused_before_connecting() {
    let a = serve(Arc::new(|_, _, _, _| ok(CSV, "a"))).await;
    // The name rules skipped: the check after resolving catches it.
    let n = Net {
        test: Some(TestNet {
            skip_names: true,
            http: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut s = open_sandboxed().await.unwrap();
    let e = run(
        s.as_mut(),
        &mut HashMap::new(),
        &format!(
            "SELECT * FROM read_csv('http://localhost:{}/a.csv')",
            a.port
        ),
        &n,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        e.contains("apunta a") && e.contains("servidores públicos"),
        "{e}"
    );
    // And with them, before resolving.
    let n = Net {
        test: Some(TestNet {
            http: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    let e = run(
        s.as_mut(),
        &mut HashMap::new(),
        &format!(
            "SELECT * FROM read_csv('http://localhost:{}/a.csv')",
            a.port
        ),
        &n,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("servidores públicos"), "{e}");
    let e = run(
        s.as_mut(),
        &mut HashMap::new(),
        &format!(
            "SELECT * FROM read_csv('http://127.0.0.1:{}/a.csv')",
            a.port
        ),
        &n,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("servidores públicos"), "{e}");
    assert!(a.hits().is_empty(), "{:?}", a.hits());
}

#[tokio::test]
async fn a_download_is_cancelled_with_the_run() {
    let a = serve(Arc::new(|m, _, _, _| {
        if m == "HEAD" {
            ok("", "a")
        } else {
            (999, vec![], CSV.as_bytes().to_vec())
        }
    }))
    .await;
    let n = net();
    let mut s = open_sandboxed().await.unwrap();
    let sql = format!("SELECT * FROM read_csv('{}/lento.csv')", a.base);
    let url = format!("{}/lento.csv", a.base);
    let access = Access {
        net: n,
        approved: vec![url],
        ..Default::default()
    };
    let cancel = Arc::new(tokio::sync::Notify::new());
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        c2.notify_waiters();
    });
    let t0 = std::time::Instant::now();
    let present = existing_tables(s.as_mut()).await.unwrap();
    let r = prepare(
        s.as_mut(),
        &mut HashMap::new(),
        &present,
        &sql,
        &[],
        1_000,
        None,
        &access,
        &mut QueryOutcome::default(),
        Some(&cancel),
        never,
    )
    .await;
    assert!(matches!(r, Err(CommandError::Cancelled)), "{:?}", r.err());
    assert!(t0.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn st_read_reads_a_geojson_over_https() {
    let geo = r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"Norte"},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#;
    let a = serve(Arc::new(move |_, path, _, _| {
        if path == "/z.geojson" {
            ok(geo, "g")
        } else {
            not_found()
        }
    }))
    .await;
    let mut s = match open_sandboxed_with(true).await {
        Ok(s) => s,
        Err(e) => return eprintln!("skipped (spatial): {e}"),
    };
    match run(
        s.as_mut(),
        &mut HashMap::new(),
        &format!(
            "SELECT name, ST_X(geom) FROM st_read('{}/z.geojson')",
            a.base
        ),
        &net(),
    )
    .await
    {
        Ok((run1, _, _)) => assert_eq!(
            rows_of(s.as_mut(), &run1).await,
            vec![vec![json!("Norte"), json!(1.0)]]
        ),
        Err(e) if e.to_string().contains("extensión spatial") => eprintln!("skipped: {e}"),
        Err(e) => panic!("{e}"),
    }
}

// -- S3, Google Cloud Storage, Azure ----------------------------------------------------------------

fn s3_list(bucket_keys: &[(&str, u64)], prefix: &str) -> Vec<u8> {
    let mut x =
        String::from("<?xml version=\"1.0\"?><ListBucketResult><IsTruncated>false</IsTruncated>");
    for (k, size) in bucket_keys.iter().filter(|(k, _)| k.starts_with(prefix)) {
        x.push_str(&format!("<Contents><Key>{k}</Key><LastModified>2026-10-14T10:00:00.000Z</LastModified><ETag>&quot;e-{k}&quot;</ETag><Size>{size}</Size></Contents>"));
    }
    x.push_str("</ListBucketResult>");
    x.into_bytes()
}

fn s3_signed(h: &HeaderMap) -> bool {
    let a = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    a.starts_with("AWS4-HMAC-SHA256 Credential=AKIDTEST/")
        && a.contains("/s3/aws4_request")
        && a.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date")
        && h.contains_key("x-amz-date")
}

#[tokio::test]
async fn s3_objects_and_prefix_globs_are_read_with_the_aws_chain_signature() {
    let keys: &[(&str, u64)] = &[
        ("2026/ene.csv", 20),
        ("2026/feb.csv", 20),
        ("2026/notas.txt", 3),
        ("2026/sub/mar.csv", 20),
        ("2025/dic.csv", 20),
    ];
    let s3 = serve(Arc::new(move |_, path, q, h| {
        if !s3_signed(h) {
            return (
                403,
                vec![],
                b"<Error><Code>AccessDenied</Code></Error>".to_vec(),
            );
        }
        match path {
            "/ventas/" if query(q, "list-type").as_deref() == Some("2") => (
                200,
                vec![],
                s3_list(keys, &query(q, "prefix").unwrap_or_default()),
            ),
            "/ventas/2026/ene.csv" => ok("id,nombre\n1,Ana\n", "e1"),
            "/ventas/2026/feb.csv" => ok("id,nombre\n2,Luis\n", "e2"),
            _ => not_found(),
        }
    }))
    .await;
    let creds = AwsCreds {
        access_key_id: "AKIDTEST".into(),
        secret_access_key: "secreto".into(),
        session_token: None,
        expires_on: None,
    };
    let n = Net {
        test: Some(TestNet {
            allow_local: true,
            http: true,
            s3: Some(s3.base.clone()),
            aws: Some(creds),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut s = open_sandboxed().await.unwrap();
    let (run1, loaded, asked) = run(
        s.as_mut(),
        &mut HashMap::new(),
        "SELECT nombre FROM read_csv('s3://ventas/2026/*.csv') ORDER BY id",
        &n,
    )
    .await
    .unwrap();
    assert_eq!(
        asked.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
        ["s3://ventas/2026/*.csv"]
    );
    assert_eq!(
        rows_of(s.as_mut(), &run1).await,
        vec![vec![json!("Ana")], vec![json!("Luis")]]
    );
    let files: Vec<(&str, Option<&str>)> = loaded[0]
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.pattern.as_deref()))
        .collect();
    assert_eq!(
        files,
        [
            ("s3://ventas/2026/ene.csv", Some("s3://ventas/2026/*.csv")),
            ("s3://ventas/2026/feb.csv", Some("s3://ventas/2026/*.csv"))
        ]
    );
    assert_eq!(
        loaded[0].files[0].version.as_deref(),
        Some("etag:e-2026/ene.csv")
    );
    assert!(
        !s3.hits()
            .iter()
            .any(|h| h.contains("sub/mar") || h.contains("notas") || h.contains("2025")),
        "{:?}",
        s3.hits()
    );

    // Without a sign-in: anonymous, and the denial says how to sign in.
    let anon = Net {
        test: Some(TestNet {
            allow_local: true,
            http: true,
            s3: Some(s3.base.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let e = run(
        s.as_mut(),
        &mut HashMap::new(),
        "SELECT * FROM read_csv('s3://ventas/2026/ene.csv')",
        &anon,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("aws sso login"), "{e}");
    // Nothing matches.
    let e = run(
        s.as_mut(),
        &mut HashMap::new(),
        "SELECT * FROM read_csv('s3://ventas/2024/*.csv')",
        &n,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("ningún archivo coincide"), "{e}");
}

#[tokio::test]
async fn an_object_over_the_size_cap_is_refused_before_downloading() {
    let s3 = serve(Arc::new(|_, path, q, _| match path {
        "/ven/" => (
            200,
            vec![],
            s3_list(
                &[("big/a.csv", MAX_OBJECT_BYTES + 1)],
                &query(q, "prefix").unwrap_or_default(),
            ),
        ),
        _ => ok(CSV, "x"),
    }))
    .await;
    let n = Net {
        test: Some(TestNet {
            allow_local: true,
            http: true,
            s3: Some(s3.base.clone()),
            aws: Some(AwsCreds {
                access_key_id: "AKIDTEST".into(),
                secret_access_key: "s".into(),
                session_token: None,
                expires_on: None,
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut s = open_sandboxed().await.unwrap();
    let e = run(
        s.as_mut(),
        &mut HashMap::new(),
        "SELECT * FROM read_csv('s3://ven/big/*.csv')",
        &n,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("el máximo por archivo"), "{e}");
    assert!(
        !s3.hits()
            .iter()
            .any(|h| h.starts_with("GET /ven/big/a.csv")),
        "{:?}",
        s3.hits()
    );
}

#[tokio::test]
async fn gcs_and_azure_objects_are_read_with_a_bearer_token() {
    let bearer =
        |h: &HeaderMap| h.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer tok");
    let gcs = serve(Arc::new(move |_, path, q, h| {
        if !bearer(h) {
            return (401, vec![], Vec::new());
        }
        match path {
            "/storage/v1/b/bkt/o" => {
                let prefix = query(q, "prefix").unwrap_or_default();
                let items: Vec<Value> = ["d/a.csv", "d/b.csv", "e/c.csv"]
                    .iter()
                    .filter(|k| k.starts_with(&prefix))
                    .map(|k| json!({ "name": k, "size": "16", "etag": format!("g-{k}"), "updated": "2026-10-14T10:00:00Z" }))
                    .collect();
                (200, vec![], json!({ "items": items }).to_string().into_bytes())
            }
            "/bkt/d/a.csv" => ok("id,nombre\n1,Ana\n", "1"),
            "/bkt/d/b.csv" => ok("id,nombre\n2,Luis\n", "2"),
            _ => not_found(),
        }
    }))
    .await;
    let az = serve(Arc::new(move |_, path, q, h| {
        if !bearer(h) || h.get("x-ms-version").is_none() {
            return (403, vec![], Vec::new());
        }
        match path {
            "/cuenta1/datos" if query(q, "comp").as_deref() == Some("list") => (
                200,
                vec![],
                b"<EnumerationResults><Blobs><Blob><Name>x/a.csv</Name><Properties><Last-Modified>Wed, 14 Oct 2026 10:00:00 GMT</Last-Modified><Etag>0x1</Etag><Content-Length>16</Content-Length></Properties></Blob></Blobs><NextMarker/></EnumerationResults>".to_vec(),
            ),
            "/cuenta1/datos/x/a.csv" => ok("id,nombre\n3,Eva\n", "0x1"),
            _ => not_found(),
        }
    }))
    .await;
    let n = Net {
        azure_account: Some("cuenta1".into()),
        test: Some(TestNet {
            allow_local: true,
            http: true,
            gcs: Some(gcs.base.clone()),
            azure: Some(az.base.clone()),
            token: Some("tok".into()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut s = open_sandboxed().await.unwrap();
    let (r1, _, _) = run(
        s.as_mut(),
        &mut HashMap::new(),
        "SELECT nombre FROM read_csv('gs://bkt/d/*.csv') ORDER BY id",
        &n,
    )
    .await
    .unwrap();
    assert_eq!(
        rows_of(s.as_mut(), &r1).await,
        vec![vec![json!("Ana")], vec![json!("Luis")]]
    );
    let (r2, _, _) = run(
        s.as_mut(),
        &mut HashMap::new(),
        "SELECT nombre FROM read_csv('gs://bkt/d/a.csv')",
        &n,
    )
    .await
    .unwrap();
    assert_eq!(rows_of(s.as_mut(), &r2).await, vec![vec![json!("Ana")]]);
    let (r3, l3, _) = run(
        s.as_mut(),
        &mut HashMap::new(),
        "SELECT nombre FROM read_csv('abfss://datos@cuenta1.dfs.core.windows.net/x/*.csv')",
        &n,
    )
    .await
    .unwrap();
    assert_eq!(rows_of(s.as_mut(), &r3).await, vec![vec![json!("Eva")]]);
    assert_eq!(
        l3[0].files[0].path,
        "az://cuenta1.blob.core.windows.net/datos/x/a.csv"
    );
    let (r4, _, _) = run(
        s.as_mut(),
        &mut HashMap::new(),
        "SELECT nombre FROM read_csv('az://datos/x/a.csv')",
        &n,
    )
    .await
    .unwrap();
    assert_eq!(rows_of(s.as_mut(), &r4).await, vec![vec![json!("Eva")]]);
    // Without a token: the message says how to sign in.
    let anon = Net {
        test: Some(TestNet {
            allow_local: true,
            http: true,
            gcs: Some(gcs.base.clone()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let e = run(
        s.as_mut(),
        &mut HashMap::new(),
        "SELECT * FROM read_csv('gs://bkt/d/a.csv')",
        &anon,
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("gcloud auth application-default login"), "{e}");
}

#[tokio::test]
async fn the_change_notice_compares_an_objects_version() {
    let store = dbine_core::StateStore::open_in_memory().unwrap();
    let acc = Access {
        notebook: Some("nb".into()),
        store: Some(&store),
        ..Default::default()
    };
    let obj = |v: Option<&str>, mtime_ms: i64| DataFile {
        path: "s3://ven/a.csv".into(),
        host: Some("v".into()),
        size: 10,
        mtime_ms,
        version: v.map(Into::into),
        ..Default::default()
    };
    let seen = crate::notebook::access::changes(&acc, &[obj(Some("etag:1"), 1_000)]).await;
    assert!(seen[0].dirty && seen[0].previous_mtime_ms.is_none());
    crate::notebook::access::remember(&acc, &seen);
    let seen = crate::notebook::access::changes(&acc, &[obj(Some("etag:1"), 1_000)]).await;
    assert!(!seen[0].dirty && seen[0].previous_mtime_ms.is_none());
    let seen = crate::notebook::access::changes(&acc, &[obj(Some("etag:2"), 2_000)]).await;
    assert_eq!(seen[0].previous_mtime_ms, Some(1_000));
    // No version: no record, no notice (it is read every time).
    assert!(crate::notebook::access::changes(&acc, &[obj(None, 3_000)])
        .await
        .is_empty());
}

#[test]
fn local_names_keep_partitions_and_never_leave_their_folder() {
    assert_eq!(
        local_name("año=2026/mes=01/a.csv.gz"),
        PathBuf::from("año=2026/mes=01/a.csv.gz")
    );
    assert_eq!(local_name("a/../b/./c.csv"), PathBuf::from("a/_/b/_/c.csv"));
    assert_eq!(
        local_name("datos/x.csv?region=sur"),
        PathBuf::from("datos/x.csv")
    );
    assert!(local_name("C:\\x/y:z.csv")
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_))));
}
