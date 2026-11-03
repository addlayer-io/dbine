//! Against a real kdb+ (`dbine-test-kdb`). kdb+ needs a license from KX
//! (KDB-X Community Edition, free by registration): the portal token and
//! the license go in the repo's `.env` as `KX_PORTAL_TOKEN` and
//! `KX_LICENSE_B64`, and `tests/docker/Dockerfile` installs it.
//!
//! ```sh
//! set -a; . ./.env; set +a
//! docker build -t dbine-test-kdb-img crates/drivers/kdb/tests/docker \
//!   --secret id=kxtoken,env=KX_PORTAL_TOKEN --secret id=kxlic,env=KX_LICENSE_B64
//! docker run -d --name dbine-test-kdb -p 15000:5000 -e KX_LICENSE_B64 dbine-test-kdb-img
//! DBINE_TEST_KDB_URL=127.0.0.1:15000:dbine:dbinepass \
//!   cargo test -p dbine-driver-kdb --test live -- --ignored
//! docker rm -f dbine-test-kdb
//! ```
//!
//! The `tls` test needs a second one, TLS only, with a certificate for
//! `127.0.0.1` signed by a test CA:
//!
//! ```sh
//! docker run -d --name dbine-test-kdb-tls -p 15001:5000 -e KX_LICENSE_B64 \
//!   -e SSL_CERT_FILE=/tls/server.pem -e SSL_KEY_FILE=/tls/server.key -v "$PWD/tls:/tls:ro" \
//!   dbine-test-kdb-img bash -c '... exec /root/.kx/bin/q -p 5000 -E 2 -u /users.txt'
//! DBINE_TEST_KDB_TLS_URL=127.0.0.1:15001 DBINE_TEST_KDB_CA="$PWD/tls/ca.pem" ...
//! ```

use dbine_driver::{kinds, ConnectionConfig, ObjectRef, QueryOutcome};

/// `host:port[:user:password]` from `DBINE_TEST_KDB_URL`.
fn cfg(read_only: bool) -> ConnectionConfig {
    let url =
        std::env::var("DBINE_TEST_KDB_URL").expect("DBINE_TEST_KDB_URL=host:port[:user:password]");
    let mut parts = url.splitn(4, ':');
    let host = parts.next().unwrap_or("127.0.0.1").to_string();
    let port = parts.next().and_then(|p| p.parse().ok()).unwrap_or(5000);
    ConnectionConfig {
        host,
        port,
        username: parts.next().map(str::to_string),
        password: parts.next().map(str::to_string),
        read_only,
        options: [("tcps".to_string(), "false".to_string())].into(),
        ..Default::default()
    }
}

async fn run(s: &mut Box<dyn dbine_driver::Session>, q: &str) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(q, 1000, &mut out)
        .await
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    out
}

#[tokio::test]
#[ignore]
async fn tables_types_and_edits() {
    let d = dbine_driver_kdb::drivers().remove(0);
    let mut s = d.connect(&cfg(false), None).await.unwrap();
    assert!(s.server_version().await.unwrap().starts_with("kdb+ "));
    run(
        &mut s,
        "dbine_t:([id:1 2 3] sym:`a`b`c; px:1.5 0n 3.0; ts:3#.z.p; d:2024.01.31 0N 2024.02.01; note:(\"x\";\"\";\"z\"))",
    )
    .await;
    let t = ObjectRef {
        kind: kinds::TABLE.into(),
        schema: None,
        name: "dbine_t".into(),
    };
    assert!(s
        .list_objects()
        .await
        .unwrap()
        .iter()
        .any(|o| o.name == "dbine_t"));
    let cols = s.columns(&t).await.unwrap();
    assert_eq!(cols.iter().filter(|c| c.primary_key).count(), 1);
    let browse = s.browse_query(&t, 10);
    let out = run(&mut s, &browse).await;
    assert_eq!(out.results[0].rows.len(), 3);

    // Filters, sort, edits and inserts are q the server accepts.
    let f = d
        .filtered_browse(
            &browse,
            &[dbine_driver::ColumnFilter {
                column: "sym".into(),
                op: dbine_driver::FilterOp::Eq,
                values: vec![serde_json::json!("b")],
                sql: None,
            }],
        )
        .unwrap();
    assert_eq!(run(&mut s, &f).await.results[0].rows.len(), 1);
    let up = d
        .update_script(
            &t,
            &[dbine_driver::RowChange {
                key: vec![("id".into(), serde_json::json!(2))],
                set: vec![
                    ("px".into(), serde_json::json!(9.5)),
                    ("note".into(), serde_json::json!("y")),
                ],
                row: vec![],
            }],
        )
        .unwrap();
    run(&mut s, &up).await;
    let ins = d
        .insert_script(
            &t,
            &[
                "id".into(),
                "sym".into(),
                "px".into(),
                "ts".into(),
                "d".into(),
                "note".into(),
            ],
            &[vec![
                serde_json::json!(4),
                serde_json::json!("d"),
                serde_json::json!(4.5),
                serde_json::json!("2024-01-31 13:45:00"),
                serde_json::json!("2024-01-31"),
                serde_json::json!("w"),
            ]],
        )
        .unwrap();
    run(&mut s, &ins).await;
    let del = d
        .delete_script(&t, &[vec![("id".into(), serde_json::json!(1))]])
        .unwrap();
    run(&mut s, &del).await;
    let out = run(&mut s, "count dbine_t").await;
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(3));
    run(&mut s, "delete dbine_t from `.").await;
}

#[tokio::test]
#[ignore]
async fn read_only_is_enforced_by_the_server() {
    let d = dbine_driver_kdb::drivers().remove(0);
    let mut s = d.connect(&cfg(true), None).await.unwrap();
    let mut out = QueryOutcome::default();
    s.run_read_only("til 3", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows.len(), 3);
    // A write is refused (before sending it, and by reval on the server).
    let mut out = QueryOutcome::default();
    let r = s.execute("{x::1}[]", 10, &mut out).await;
    assert!(r.is_err());
    // And a plain read still runs.
    run(&mut s, "count til 10").await;
    // What slips past a plain word list is refused before it's sent.
    for q in [
        "0 \"dbine_ro:1\"",
        "(neg .z.w) \"dbine_ro:1\"",
        ".q[`set][`dbine_ro;1]",
        "get \"dbine_ro:1\"",
        "@[(`.);`dbine_ro;:;1]",
    ] {
        let e = s
            .execute(q, 10, &mut QueryOutcome::default())
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("solo lectura"), "{q}: {e}");
    }
    let mut w = d.connect(&cfg(false), None).await.unwrap();
    run(&mut w, "@[`.;`dbine_ro;:;0]").await;
    let mut out = QueryOutcome::default();
    w.execute("dbine_ro", 10, &mut out).await.unwrap();
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(0));
    // A browse runs on a read-only connection.
    let v = ObjectRef {
        kind: "variable".into(),
        schema: None,
        name: "dbine_ro".into(),
    };
    let mut out = QueryOutcome::default();
    s.execute(&s.browse_query(&v, 10), 10, &mut out)
        .await
        .unwrap();
    assert_eq!(out.results[0].rows.len(), 1);
}

#[tokio::test]
#[ignore]
async fn hdb_namespaces_and_big_results() {
    let d = dbine_driver_kdb::drivers().remove(0);
    let mut s = d.connect(&cfg(false), None).await.unwrap();
    // A partitioned table (two dates) and a splayed one, loaded as an HDB
    // (relative paths: `-u` keeps the server inside its directory);
    // a view and a one-letter user namespace; a result over the grid's size.
    run(
        &mut s,
        "{[dt] .Q.dpft[`:dbine_hdb;dt;`sym;`dbine_q set ([] sym:`a`b`a; px:1 2 3f)]} each 2026.11.01 2026.11.02",
    )
    .await;
    run(
        &mut s,
        "`:dbine_hdb/dbine_s/ set .Q.en[`:dbine_hdb] ([] sym:`x`y; n:1 2)",
    )
    .await;
    run(&mut s, "\\l dbine_hdb").await;
    run(&mut s, "dbine_v::1+1").await;
    run(&mut s, ".u.dbine_t:([] a:1 2)").await;
    run(&mut s, "dbine_big:([] a:til 2000000)").await;

    let objs = s.list_objects().await.unwrap();
    let kind = |n: &str| {
        objs.iter()
            .find(|o| o.name == n)
            .map(|o| (o.kind.clone(), o.schema.clone()))
    };
    assert_eq!(kind("dbine_q").unwrap().0, dbine_driver_kdb::PARTITIONED);
    assert_eq!(kind("dbine_s").unwrap().0, dbine_driver_kdb::SPLAYED);
    assert_eq!(kind("dbine_big").unwrap().0, kinds::TABLE);
    assert!(objs.iter().any(|o| o.name.ends_with("dbine_t")), "{objs:?}");
    assert_eq!(kind("dbine_v").unwrap().0, kinds::VIEW);

    let q = ObjectRef {
        kind: dbine_driver_kdb::PARTITIONED.into(),
        schema: None,
        name: "dbine_q".into(),
    };
    let browse = s.browse_query(&q, 4);
    assert_eq!(run(&mut s, &browse).await.results[0].rows.len(), 4);
    let f = d
        .filtered_browse(
            &browse,
            &[dbine_driver::ColumnFilter {
                column: "date".into(),
                op: dbine_driver::FilterOp::Eq,
                values: vec![serde_json::json!("2026-11-02")],
                sql: None,
            }],
        )
        .unwrap();
    assert_eq!(run(&mut s, &f).await.results[0].rows.len(), 3);
    assert!(d
        .update_script(
            &q,
            &[dbine_driver::RowChange {
                key: vec![("sym".into(), serde_json::json!("a"))],
                set: vec![("px".into(), serde_json::json!(9.0))],
                row: vec![],
            }],
        )
        .is_err());

    // The grid gets the first rows and the count; export gets them all.
    let grid = run(&mut s, "dbine_big").await;
    assert_eq!(grid.results[0].rows.len(), 1000);
    assert!(grid.results[0].truncated);
    assert_eq!(grid.results[0].total_rows, 2_000_000);
    let mut all = QueryOutcome::default();
    s.execute("dbine_big", usize::MAX, &mut all).await.unwrap();
    assert_eq!(all.results[0].rows.len(), 2_000_000);
    run(&mut s, "delete dbine_big from `.").await;

    // A function over several lines, with a blank line and a comment, is
    // one statement; a system command runs; the Monitor reads the server.
    let out = run(
        &mut s,
        "dbine_f:{[x]\n  y:x+1;\n\n/ a comment\n  y*2 }\ndbine_f 3\n\\p",
    )
    .await;
    assert_eq!(out.results.len(), 3, "{:?}", out.results);
    assert_eq!(out.results[1].rows[0][0], serde_json::json!(8));
    assert!(!s.monitor().await.unwrap().metrics.is_empty());
}

#[tokio::test]
#[ignore]
async fn wrong_password_types_and_server_requests() {
    let d = dbine_driver_kdb::drivers().remove(0);
    let bad = ConnectionConfig {
        password: Some("nope".into()),
        ..cfg(false)
    };
    assert!(d.connect(&bad, None).await.is_err());

    let mut s = d.connect(&cfg(false), None).await.unwrap();
    // Every q type comes back without breaking the decoder.
    let out = run(
        &mut s,
        "(0b;0x2a;1h;2i;3j;4.5e;6.7;\"c\";`s;2026.11.03D10:20:30.123456789;2026.11m;2026.11.03;\
         2026.11.03T10:20:30.123;0D01:02:03.000000004;10:20u;10:20:30v;10:20:30.123t;\
         first 1?0Ng;0N;0n;0w;-0w;0Nd;`a`b!1 2;([k:1 2] v:`x`y);{x+y};+[;1];sum;+;\
         `sym$`a;1 2 3h;\"str\";(1;`a;\"b\"))",
    )
    .await;
    assert_eq!(out.results.len(), 1, "{:?}", out.results);
    // Primitives by the names kdb+ gives them.
    let rows = &out.results[0].rows;
    assert_eq!(rows[27][0], serde_json::json!("sum"));
    assert_eq!(rows[28][0], serde_json::json!("+"));
    // The server asking us something mid-query gets an error, and the
    // query still answers.
    let out = run(&mut s, "@[.z.w;\"1+1\";{`refused}]").await;
    assert_eq!(out.results[0].rows[0][0], serde_json::json!("refused"));
    // Big answers come compressed over a network (not from localhost).
    let out = run(&mut s, ".z.a").await;
    assert_ne!(
        out.results[0].rows[0][0],
        serde_json::json!(2130706433),
        "connected from localhost: kdb+ wouldn't compress"
    );
    let mut all = QueryOutcome::default();
    s.execute("([] a:til 3000000; b:3000000#`x`y`z)", usize::MAX, &mut all)
        .await
        .unwrap();
    assert_eq!(all.results[0].rows.len(), 3_000_000);
    assert_eq!(
        all.results[0].rows[2_999_999][0],
        serde_json::json!(2_999_999)
    );
}

#[tokio::test]
#[ignore]
async fn cancel_and_dropped_connections() {
    let d = dbine_driver_kdb::drivers().remove(0);
    let mut s = d.connect(&cfg(false), None).await.unwrap();
    // Cancel: the query stops for us at once, the next one reconnects
    // (once the server finishes the old one).
    let stop = s.interrupter().unwrap();
    let t0 = std::time::Instant::now();
    let handle = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        stop();
    });
    let mut out = QueryOutcome::default();
    let r = s
        .execute("{t:.z.p+0D00:00:03; while[.z.p<t]; 1}[]", 100, &mut out)
        .await;
    handle.await.unwrap();
    assert!(r.is_err(), "{out:?}");
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        t0.elapsed()
    );
    let out = run(&mut s, "1+1").await;
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(2));

    // A connection closed by the server (here, from another session) is
    // reopened by the next query.
    let h = run(&mut s, ".z.w").await.results[0].rows[0][0].clone();
    let mut other = d.connect(&cfg(false), None).await.unwrap();
    other.kill_session(&h.to_string()).await.unwrap();
    let out = run(&mut s, "2+2").await;
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(4));
}

/// `DBINE_TEST_KDB_TLS_URL` (a kdb+ started with `-E 2`) and
/// `DBINE_TEST_KDB_CA` (the CA that signed its certificate).
fn tls_cfg(opts: &[(&str, &str)]) -> ConnectionConfig {
    let url = std::env::var("DBINE_TEST_KDB_TLS_URL").expect("DBINE_TEST_KDB_TLS_URL");
    let mut c = cfg(false);
    let mut parts = url.splitn(2, ':');
    c.host = parts.next().unwrap().to_string();
    c.port = parts.next().unwrap().parse().unwrap();
    c.options = opts
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    c
}

#[tokio::test]
#[ignore]
async fn tls() {
    let d = dbine_driver_kdb::drivers().remove(0);
    let ca = std::env::var("DBINE_TEST_KDB_CA").expect("DBINE_TEST_KDB_CA");
    // Verified by the CA that signed it.
    let mut s = d
        .connect(
            &tls_cfg(&[("tcps", "true"), ("SSL_CA_CERT_FILE", &ca)]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        run(&mut s, "1+1").await.results[0].rows[0][0],
        serde_json::json!(2)
    );
    // Not trusted by the system: refused.
    assert!(d
        .connect(&tls_cfg(&[("tcps", "true")]), None)
        .await
        .is_err());
    // Without verification, pinned to the first certificate seen; a pin
    // that doesn't match is refused.
    let mut s = d
        .connect(
            &tls_cfg(&[("tcps", "true"), ("SSL_VERIFY_SERVER", "false")]),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        run(&mut s, "2+2").await.results[0].rows[0][0],
        serde_json::json!(4)
    );
    let wrong = "00".repeat(32);
    assert!(d
        .connect(
            &tls_cfg(&[
                ("tcps", "true"),
                ("SSL_VERIFY_SERVER", "false"),
                ("tls_pin", &wrong),
            ]),
            None,
        )
        .await
        .is_err());
    // Plain TCP to a TLS-only server fails instead of hanging.
    let plain = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        d.connect(&tls_cfg(&[("tcps", "false")]), None),
    )
    .await
    .expect("plain connect to a TLS-only server hung");
    assert!(plain.is_err());
}

#[tokio::test]
#[ignore]
async fn foreign_keys_are_linked_columns() {
    let d = dbine_driver_kdb::drivers().remove(0);
    assert!(d.capabilities().foreign_keys);
    let mut s = d.connect(&cfg(false), None).await.unwrap();
    // A keyed parent, a child linked to it (`dbine_fk_p$`), and a link
    // over a two-column key (left out: nothing pairs with its index).
    run(
        &mut s,
        "dbine_fk_p:([id:`a`b`c] name:(\"x\";\"y\";\"z\")); dbine_fk_c:([] p:`dbine_fk_p$`a`b`a; v:1 2 3); dbine_fk_k:([a:1 2; b:`x`y] w:3 4); dbine_fk_m:([] r:`dbine_fk_k$((1;`x);(2;`y)))",
    )
    .await;
    let schema = s.database_schema().await.unwrap();
    let t = |n: &str| {
        schema
            .iter()
            .find(|t| t.name == n)
            .unwrap_or_else(|| panic!("{n} not in the schema"))
    };
    let child = t("dbine_fk_c");
    assert_eq!(child.foreign_keys.len(), 1, "{child:?}");
    let fk = &child.foreign_keys[0];
    assert_eq!(
        (
            fk.columns.clone(),
            fk.ref_table.as_str(),
            fk.ref_columns.clone()
        ),
        (vec!["p".to_string()], "dbine_fk_p", vec!["id".to_string()])
    );
    assert_eq!(
        child
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["p", "v"]
    );
    let parent = t("dbine_fk_p");
    assert_eq!(parent.primary_key.as_ref().unwrap().columns, ["id"]);
    assert!(parent.foreign_keys.is_empty());
    assert!(t("dbine_fk_m").foreign_keys.is_empty());
    run(
        &mut s,
        "delete dbine_fk_m, dbine_fk_k, dbine_fk_c, dbine_fk_p from `.",
    )
    .await;
}

/// Batches handed to a bulk load.
struct Batches(Vec<dbine_driver::transfer::RowBatch>);

#[dbine_driver::async_trait]
impl dbine_driver::transfer::BatchSource for Batches {
    async fn next(&mut self) -> Option<dbine_driver::transfer::RowBatch> {
        (!self.0.is_empty()).then(|| self.0.remove(0))
    }
}

fn load_spec(kind: &str, name: &str, columns: &[&str]) -> dbine_driver::transfer::LoadSpec {
    dbine_driver::transfer::LoadSpec {
        table: ObjectRef {
            kind: kind.into(),
            schema: None,
            name: name.into(),
        },
        columns: columns.iter().map(|c| c.to_string()).collect(),
        table_lock: false,
        keep_identity: false,
        commit_rows: dbine_driver::transfer::LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: dbine_driver::transfer::LoadSpec::DEFAULT_COMMIT_BYTES,
    }
}

#[tokio::test]
#[ignore]
async fn bulk_load_sends_typed_batches() {
    use dbine_driver::transfer::{Cell, RowBatch};
    let d = dbine_driver_kdb::drivers().remove(0);
    assert!(d.supports_bulk_load());
    let mut s = d.connect(&cfg(false), None).await.unwrap();
    run(
        &mut s,
        "dbine_bl:([] id:`long$(); sym:`symbol$(); px:`float$(); qty:`int$(); d:`date$(); ts:`timestamp$(); note:(); ok:`boolean$(); g:`guid$(); t:`time$())",
    )
    .await;
    let cols = ["id", "sym", "px", "qty", "d", "ts", "note", "ok", "g", "t"];
    const ROWS: i64 = 100_000;
    let row = |i: i64| -> Vec<Cell> {
        vec![
            Cell::Int(i),
            Cell::Text(["AAPL", "MSFT", "IBM"][i as usize % 3].into()),
            if i % 10 == 0 {
                Cell::Null
            } else {
                Cell::Float(i as f64 / 4.0)
            },
            // q's infinity as text: that batch's column goes as strings,
            // cast on the server.
            if i == 7 {
                Cell::Text("0W".into())
            } else {
                Cell::Int(i % 1000)
            },
            Cell::Date(format!("2024-01-{:02}", i % 28 + 1)),
            Cell::DateTime(format!("2024-02-03 04:05:{:02}.123456789", i % 60)),
            Cell::Text(format!("note {i}")),
            Cell::Bool(i % 2 == 0),
            Cell::Uuid(format!("00000000-0000-0000-0000-{i:012x}")),
            Cell::Time("13:45:01.250".into()),
        ]
    };
    let batches = (0..ROWS)
        .collect::<Vec<_>>()
        .chunks(1000)
        .map(|c| RowBatch {
            rows: c.iter().map(|&i| row(i)).collect(),
            bytes: 0,
        })
        .collect();
    let seen = std::sync::atomic::AtomicU64::new(0);
    let progress = |n: u64| seen.store(n, std::sync::atomic::Ordering::SeqCst);
    let n = s
        .bulk_load(
            &load_spec(kinds::TABLE, "dbine_bl", &cols),
            &[],
            &mut Batches(batches),
            &progress,
        )
        .await
        .unwrap();
    assert_eq!(n, ROWS as u64);
    assert_eq!(seen.load(std::sync::atomic::Ordering::SeqCst), ROWS as u64);
    let out = run(&mut s, "count dbine_bl").await;
    assert_eq!(out.results[0].rows[0][0], serde_json::json!(ROWS));
    // The table's types are what they were.
    let out = run(&mut s, "exec t from meta dbine_bl").await;
    let types: String = out.results[0]
        .rows
        .iter()
        .map(|r| r[0].as_str().unwrap().to_string())
        .collect();
    assert_eq!(types, "jsfidpCbgt");
    let out = run(&mut s, "select from dbine_bl where id in 0 7 12345").await;
    let r = &out.results[0].rows;
    assert_eq!(r.len(), 3);
    let at = |row: usize, col: &str| r[row][cols.iter().position(|c| *c == col).unwrap()].clone();
    assert_eq!(at(0, "px"), serde_json::Value::Null);
    assert_eq!(at(1, "qty"), serde_json::json!("0W"));
    assert_eq!(at(1, "px"), serde_json::json!(1.75));
    assert_eq!(at(2, "sym"), serde_json::json!("AAPL"));
    assert_eq!(at(2, "px"), serde_json::json!(12345.0 / 4.0));
    assert_eq!(at(2, "qty"), serde_json::json!(345));
    assert_eq!(at(2, "d"), serde_json::json!("2024-01-26"));
    assert_eq!(
        at(2, "ts"),
        serde_json::json!("2024-02-03 04:05:45.123456789")
    );
    assert_eq!(at(2, "note"), serde_json::json!("note 12345"));
    assert_eq!(at(2, "ok"), serde_json::json!(false));
    assert_eq!(
        at(2, "g"),
        serde_json::json!("00000000-0000-0000-0000-000000003039")
    );
    assert_eq!(at(2, "t"), serde_json::json!("13:45:01.250"));

    // A keyed table takes the rows by upsert: a key already there is
    // updated. Only some columns: the others get nulls.
    run(&mut s, "dbine_bk:([id:`long$()] v:`float$(); w:`symbol$())").await;
    for v in [1.0, 2.0] {
        let rows = (0..10)
            .map(|i| vec![Cell::Int(i), Cell::Float(v)])
            .collect();
        s.bulk_load(
            &load_spec(kinds::TABLE, "dbine_bk", &["id", "v"]),
            &[],
            &mut Batches(vec![RowBatch { rows, bytes: 0 }]),
            &|_| {},
        )
        .await
        .unwrap();
    }
    let out = run(&mut s, "(count dbine_bk; exec sum v from dbine_bk)").await;
    assert_eq!(
        out.results[0].rows,
        [vec![serde_json::json!(10)], vec![serde_json::json!(20.0)]]
    );

    // Partitioned and splayed tables are refused before anything is sent.
    for kind in [dbine_driver_kdb::PARTITIONED, dbine_driver_kdb::SPLAYED] {
        let e = s
            .bulk_load(
                &load_spec(kind, "dbine_bl", &["id"]),
                &[],
                &mut Batches(vec![]),
                &|_| {},
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(e.contains("particionada o splayed"), "{e}");
    }
    // A read-only connection doesn't load.
    let mut ro = d.connect(&cfg(true), None).await.unwrap();
    assert!(ro
        .bulk_load(
            &load_spec(kinds::TABLE, "dbine_bk", &["id"]),
            &[],
            &mut Batches(vec![]),
            &|_| {},
        )
        .await
        .is_err());
    run(&mut s, "delete dbine_bl, dbine_bk from `.").await;
}

#[tokio::test]
#[ignore]
async fn charts_aggregate_on_the_server() {
    use dbine_driver::aggregate::{from_grouped, AggFn, AggregateSpec, ChartAggregate, Measure};
    let d = dbine_driver_kdb::drivers().remove(0);
    let mut s = d.connect(&cfg(false), None).await.unwrap();
    run(
        &mut s,
        "dbine_ag:([] sym:`b`a`b``a`c; side:`buy`sell`sell`buy`buy`sell; px:1 0N 3 4 5 0N; note:(\"y\";\"x\";\"y\";\"z\";\"x\";\"x\"))",
    )
    .await;
    // The read-only session runs it, as charts do (inside reval).
    let mut ro = d.connect(&cfg(true), None).await.unwrap();
    let measure = |c: Option<&str>, func| Measure {
        column: c.map(str::to_string),
        func,
    };
    async fn agg(
        d: &dyn dbine_driver::Driver,
        ro: &mut Box<dyn dbine_driver::Session>,
        spec: AggregateSpec,
        query: &str,
    ) -> ChartAggregate {
        let statement = d.aggregate_query(query, &spec).unwrap();
        let mut out = QueryOutcome::default();
        ro.run_read_only(&statement, spec.max_groups as usize + 1, &mut out)
            .await
            .unwrap_or_else(|e| panic!("{statement}: {e}"));
        from_grouped(&spec, &out.results[0].rows).unwrap()
    }
    let by_sym = AggregateSpec {
        x: Some("sym".into()),
        split: None,
        measures: vec![
            measure(None, AggFn::Count),
            measure(Some("px"), AggFn::Sum),
            measure(Some("px"), AggFn::Count),
            measure(Some("px"), AggFn::Avg),
            measure(Some("px"), AggFn::Max),
        ],
        max_groups: 100,
    };
    let a = agg(
        d.as_ref(),
        &mut ro,
        by_sym.clone(),
        "select from dbine_ag / every row",
    )
    .await;
    assert_eq!(
        a.categories,
        [None, Some("a".into()), Some("b".into()), Some("c".into())]
    );
    let values: Vec<Vec<Option<f64>>> = a.series.iter().map(|s| s.values.clone()).collect();
    assert_eq!(values[0], [Some(1.0), Some(2.0), Some(2.0), Some(1.0)]);
    // SUM / AVG / MAX over no numbers (group `c`) have no value.
    assert_eq!(values[1], [Some(4.0), Some(5.0), Some(4.0), None]);
    assert_eq!(values[2], [Some(1.0), Some(1.0), Some(2.0), Some(0.0)]);
    assert_eq!(values[3], [Some(4.0), Some(5.0), Some(2.0), None]);
    assert_eq!(values[4], [Some(4.0), Some(5.0), Some(3.0), None]);
    assert_eq!(a.rows_scanned, Some(6));

    // Groups of strings come sorted, a split makes series, and a cut past
    // max_groups says so.
    let a = agg(
        d.as_ref(),
        &mut ro,
        AggregateSpec {
            x: Some("note".into()),
            split: Some("side".into()),
            measures: vec![measure(None, AggFn::Count)],
            max_groups: 100,
        },
        "select from dbine_ag",
    )
    .await;
    assert_eq!(
        a.categories,
        [Some("x".into()), Some("y".into()), Some("z".into())]
    );
    assert_eq!(a.series.len(), 2);
    assert_eq!(a.series[0].split.as_deref(), Some("buy"));
    assert_eq!(a.series[0].values, [Some(1.0), Some(1.0), Some(1.0)]);
    assert_eq!(a.series[1].values, [Some(2.0), Some(1.0), None]);
    let a = agg(
        d.as_ref(),
        &mut ro,
        AggregateSpec {
            max_groups: 2,
            ..by_sym.clone()
        },
        "select from dbine_ag",
    )
    .await;
    assert!(a.groups_truncated);
    assert_eq!(a.categories.len(), 2);

    // A total (no X) over a keyed result.
    let a = agg(
        d.as_ref(),
        &mut ro,
        AggregateSpec {
            x: None,
            split: None,
            measures: vec![measure(Some("px"), AggFn::Min)],
            max_groups: 1,
        },
        "select sum px by sym from dbine_ag",
    )
    .await;
    assert_eq!(a.series[0].values, [Some(0.0)]);
    assert_eq!(a.rows_scanned, Some(4));
    run(&mut s, "delete dbine_ag from `.").await;
}
