//! Live tests of sign-in, queries and table reads against a Power BI /
//! Fabric tenant (spec §8.3, the checks no fake server can answer). There
//! is no Power BI image: they need the test tenant of spec §9.2 and run
//! only by hand, with the owner's OK:
//!
//! ```text
//! DBINE_TEST_POWERBI_TENANT=…            # tenant GUID or domain
//! DBINE_TEST_POWERBI_CLIENT_ID=…         # service principal (member of both workspaces)
//! DBINE_TEST_POWERBI_CLIENT_SECRET=…
//! DBINE_TEST_POWERBI_WORKSPACE_PRO=…     # shared capacity
//! DBINE_TEST_POWERBI_WORKSPACE_FABRIC=…  # Fabric trial capacity
//! DBINE_TEST_POWERBI_MODEL=…             # the sample model, published to both
//! DBINE_TEST_POWERBI_TABLE=…             # a table of it (a few thousand rows)
//! DBINE_TEST_POWERBI_VIEWER_TOKEN=…      # a token of a Read + Build only user
//! DBINE_TEST_POWERBI_USER=…              # a user for the browser sign-in tests
//! cargo test -p dbine-driver-powerbi --test live_queries -- --ignored --test-threads=1
//! ```
//!
//! Nothing here writes to the tenant: every test only reads.

use dbine_driver::aggregate::{AggFn, AggregateSpec, Measure};
use dbine_driver::transfer::{BatchSink, RowBatch, TransferColumn};
use dbine_driver::{
    ColumnFilter, ConnectionConfig, Driver, FilterOp, ObjectRef, QueryOutcome, ReadSpec, Session,
    SortKey,
};
use std::sync::{Arc, Mutex};

fn env(k: &str) -> String {
    std::env::var(format!("DBINE_TEST_POWERBI_{k}"))
        .unwrap_or_else(|_| panic!("DBINE_TEST_POWERBI_{k} is not set"))
}

fn driver() -> Arc<dyn Driver> {
    dbine_driver_powerbi::drivers().remove(0)
}

fn config(opts: &[(&str, String)]) -> ConnectionConfig {
    let mut c = ConnectionConfig {
        driver: "powerbi".into(),
        read_only: true,
        ..Default::default()
    };
    for (k, v) in opts {
        c.options.insert(k.to_string(), v.clone());
    }
    c
}

/// A service-principal connection to `workspace`.
fn sp(workspace: &str, extra: &[(&str, &str)]) -> ConnectionConfig {
    let mut opts = vec![
        ("auth_mode", "entra_sp".to_string()),
        ("tenant", env("TENANT")),
        ("client_id", env("CLIENT_ID")),
        ("client_secret", env("CLIENT_SECRET")),
        ("workspace", workspace.to_string()),
    ];
    opts.extend(extra.iter().map(|(k, v)| (*k, v.to_string())));
    config(&opts)
}

async fn open(cfg: &ConnectionConfig) -> Box<dyn Session> {
    driver()
        .connect(cfg, Some(&env("MODEL")))
        .await
        .expect("connect")
}

async fn run(s: &mut dyn Session, dax: &str, max_rows: usize) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    s.execute(dax, max_rows, &mut out).await.expect(dax);
    out
}

fn table() -> ObjectRef {
    ObjectRef {
        kind: "table".into(),
        schema: None,
        name: env("TABLE"),
    }
}

#[derive(Default)]
struct Count {
    columns: Vec<TransferColumn>,
    rows: u64,
}

impl BatchSink for Count {
    fn begin(&mut self, columns: &[TransferColumn]) -> std::io::Result<()> {
        self.columns = columns.to_vec();
        Ok(())
    }
    fn batch(&mut self, batch: RowBatch) -> std::io::Result<()> {
        self.rows += batch.rows.len() as u64;
        Ok(())
    }
}

/// COUNTROWS of the test table.
async fn rows_of_table(s: &mut dyn Session) -> u64 {
    let out = run(
        s,
        &format!(
            "EVALUATE {{ COUNTROWS('{}') }}",
            env("TABLE").replace('\'', "''")
        ),
        1,
    )
    .await;
    out.results[0].rows[0][0].as_u64().expect("a count")
}

#[tokio::test]
#[ignore]
async fn the_api_choice_on_pro_and_on_fabric() {
    // G3: does executeDaxQueries answer on Pro? Either way the query runs
    // and server_version says which API it went to.
    for w in [env("WORKSPACE_PRO"), env("WORKSPACE_FABRIC")] {
        let mut s = open(&sp(&w, &[])).await;
        let out = run(s.as_mut(), "EVALUATE { 1 }", 10).await;
        assert_eq!(out.results.len(), 1);
        let v = s.server_version().await.unwrap();
        assert!(
            v.contains("executeQueries") || v.contains("executeDaxQueries"),
            "{v}"
        );
        eprintln!("{w}: {v}; messages: {:?}", out.messages);
    }
}

#[tokio::test]
#[ignore]
async fn a_define_unit_names_each_evaluate_on_arrow() {
    let mut s = open(&sp(&env("WORKSPACE_FABRIC"), &[("query_api", "arrow")])).await;
    let unit = "DEFINE VAR x = 1\n// uno\nEVALUATE { x }\n// dos\nEVALUATE { x + 1 }";
    let out = run(s.as_mut(), unit, 10).await;
    assert_eq!(out.results.len(), 2);
    assert!(unit[out.results[1].offset.unwrap()..].starts_with("EVALUATE { x + 1 }"));
}

#[tokio::test]
#[ignore]
async fn the_json_cap_is_marked_and_the_arrow_limit_holds() {
    // executeQueries: past 100,000 rows the answer carries an error next to
    // the rows (F3); the grid gets the warning.
    let mut s = open(&sp(&env("WORKSPACE_PRO"), &[("query_api", "json")])).await;
    let out = run(s.as_mut(), "EVALUATE GENERATESERIES(1, 150000)", 200_000).await;
    assert!(out.results[0].truncated, "the cap wasn't marked");
    // executeDaxQueries: resultSetRowCountLimit = max_rows + 1.
    let mut s = open(&sp(&env("WORKSPACE_FABRIC"), &[("query_api", "arrow")])).await;
    let out = run(s.as_mut(), "EVALUATE GENERATESERIES(1, 5000)", 1000).await;
    assert_eq!(out.results[0].rows.len(), 1000);
    assert!(out.results[0].truncated);
}

/// Rows streamed to an export, chart or notebook sink.
struct Streamed(Arc<Mutex<u64>>);

impl dbine_driver::RowSink for Streamed {
    fn begin(&mut self, _: usize, _: &[dbine_driver::ResultColumn]) -> std::io::Result<()> {
        Ok(())
    }
    fn row(&mut self, _: usize, _: &[serde_json::Value]) -> std::io::Result<()> {
        *self.0.lock().unwrap() += 1;
        Ok(())
    }
}

fn streaming() -> (QueryOutcome, Arc<Mutex<u64>>) {
    let n = Arc::new(Mutex::new(0));
    let out = QueryOutcome {
        sink: Some(dbine_driver::RowSinkRef(Arc::new(Mutex::new(Streamed(
            n.clone(),
        ))))),
        ..Default::default()
    };
    (out, n)
}

#[tokio::test]
#[ignore]
async fn the_json_value_and_size_caps_are_marked_without_an_error_object() {
    let mut s = open(&sp(&env("WORKSPACE_PRO"), &[("query_api", "json")])).await;
    // 13 columns: the value cap stops the answer at 76,923 rows; whether
    // an `error` object comes with it is what this checks.
    let cols: Vec<String> = (1..13).map(|i| format!("\"c{i}\", [Value]")).collect();
    let dax = format!(
        "EVALUATE ADDCOLUMNS(GENERATESERIES(1, 100000), {})",
        cols.join(", ")
    );
    let out = run(s.as_mut(), &dax, 200_000).await;
    let r = &out.results[0];
    eprintln!(
        "13 columns: {} rows, truncated {}",
        r.rows.len(),
        r.truncated
    );
    assert!(r.rows.len() <= 76_923 && r.truncated, "{}", r.rows.len());
    // About 20 MB of text: the 15 MB cut (bytes or MiB, with or without an
    // `error`, are live checks).
    let out = run(
        s.as_mut(),
        "EVALUATE ADDCOLUMNS(GENERATESERIES(1, 20000), \"t\", REPT(\"x\", 1000))",
        200_000,
    )
    .await;
    let r = &out.results[0];
    eprintln!("15 MB: {} rows, truncated {}", r.rows.len(), r.truncated);
    assert!(r.rows.len() < 20_000 && r.truncated, "{}", r.rows.len());
}

#[tokio::test]
#[ignore]
async fn sink_runs_error_only_on_the_services_own_cut() {
    let mut s = open(&sp(&env("WORKSPACE_PRO"), &[("query_api", "json")])).await;
    // An export (no cap of its own) of a result past the JSON cap: an
    // error, never a partial file.
    let (mut out, _) = streaming();
    let e = s
        .execute("EVALUATE GENERATESERIES(1, 150000)", usize::MAX, &mut out)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("resultado parcial"), "{e}");
    // A notebook load capped at 1,000 rows (it asks for 1,001): its own
    // cap, streamed whole up to the service's answer.
    let (mut out, n) = streaming();
    s.execute("EVALUATE GENERATESERIES(1, 150000)", 1001, &mut out)
        .await
        .unwrap();
    assert!(*n.lock().unwrap() > 1001);
    // Arrow: a chart capped at 3 rows asks for 4; the limit of 5 is
    // reached, but past what the chart asked for: its own cap, no error.
    let mut s = open(&sp(&env("WORKSPACE_FABRIC"), &[("query_api", "arrow")])).await;
    let (mut out, n) = streaming();
    s.execute("EVALUATE GENERATESERIES(1, 5000)", 4, &mut out)
        .await
        .unwrap();
    assert_eq!(*n.lock().unwrap(), 5);
}

#[tokio::test]
#[ignore]
async fn dax_errors_come_with_their_place() {
    for (w, api) in [
        (env("WORKSPACE_PRO"), "json"),
        (env("WORKSPACE_FABRIC"), "arrow"),
    ] {
        let mut s = open(&sp(&w, &[("query_api", api)])).await;
        let mut out = QueryOutcome::default();
        let e = s
            .execute("EVALUATE\n  ROW(\"x\", SUMX(1))", 10, &mut out)
            .await
            .unwrap_err();
        // Save the real bodies as fixtures when they differ from ours.
        let se = e.to_script_error();
        eprintln!("{api}: {se:?}");
        assert!(se.line.is_some(), "{api}: no place in {se:?}");
    }
}

#[tokio::test]
#[ignore]
async fn browse_filter_sort_and_chart_queries_run() {
    let d = driver();
    for w in [env("WORKSPACE_PRO"), env("WORKSPACE_FABRIC")] {
        let mut s = open(&sp(&w, &[])).await;
        let browse = s.browse_query(&table(), 50);
        let out = run(s.as_mut(), &browse, 50).await;
        let first = out.results[0].columns[0].name.clone();
        assert!(!first.contains('['), "the browse keeps the prefix: {first}");
        let filtered = d
            .filtered_browse(
                &browse,
                &[ColumnFilter {
                    column: first.clone(),
                    op: FilterOp::NotNull,
                    values: vec![],
                    sql: None,
                }],
            )
            .unwrap();
        let sorted = d
            .sorted_browse(
                &filtered,
                &[SortKey {
                    column: first.clone(),
                    descending: true,
                }],
            )
            .unwrap();
        run(s.as_mut(), &sorted, 50).await;
        // GROUPBY over the browse (and over extension columns: a live check).
        let spec = AggregateSpec {
            x: Some(first.clone()),
            split: None,
            measures: vec![Measure {
                column: None,
                func: AggFn::Count,
            }],
            max_groups: 100,
        };
        let grouped = d.aggregate_query(&browse, &spec).unwrap();
        run(s.as_mut(), &grouped, 101).await;
        let ext = "EVALUATE ADDCOLUMNS(GENERATESERIES(1, 10), \"Par\", MOD([Value], 2))";
        let spec = AggregateSpec {
            x: Some("[Par]".into()),
            ..spec
        };
        let mut out = QueryOutcome::default();
        let r = s
            .execute(&d.aggregate_query(ext, &spec).unwrap(), 101, &mut out)
            .await;
        eprintln!("GROUPBY over an extension column: {r:?}");
    }
}

#[tokio::test]
#[ignore]
async fn tables_are_read_whole() {
    for w in [env("WORKSPACE_PRO"), env("WORKSPACE_FABRIC")] {
        let mut s = open(&sp(&w, &[])).await;
        let expected = rows_of_table(s.as_mut()).await;
        let sink = Arc::new(Mutex::new(Count::default()));
        let n = s
            .read_batches(
                &ReadSpec {
                    table: table(),
                    columns: None,
                    filter: None,
                },
                sink.clone(),
            )
            .await
            .unwrap();
        assert_eq!(n, expected, "{w}");
        assert_eq!(sink.lock().unwrap().rows, expected, "{w}");
        assert!(sink
            .lock()
            .unwrap()
            .columns
            .iter()
            .all(|c| !c.name.contains('[')));
    }
}

#[tokio::test]
#[ignore]
async fn json_pages_read_only_the_wanted_columns() {
    // The JSON pages project the wanted columns under names of their own
    // (`SELECTCOLUMNS(…, "__dbine_0", …)`): this checks how executeQueries
    // names them, on both workspaces.
    for w in [env("WORKSPACE_PRO"), env("WORKSPACE_FABRIC")] {
        let mut s = open(&sp(&w, &[("query_api", "json")])).await;
        let expected = rows_of_table(s.as_mut()).await;
        let browse = s.browse_query(&table(), 1);
        let first = run(s.as_mut(), &browse, 1).await.results[0].columns[0]
            .name
            .clone();
        let sink = Arc::new(Mutex::new(Count::default()));
        let n = s
            .read_batches(
                &ReadSpec {
                    table: table(),
                    columns: Some(vec![first.clone()]),
                    filter: None,
                },
                sink.clone(),
            )
            .await
            .unwrap();
        assert_eq!(n, expected, "{w}");
        let got = sink.lock().unwrap();
        assert_eq!(got.rows, expected, "{w}");
        let names: Vec<&str> = got.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, [first.as_str()], "{w}");
    }
}

#[tokio::test]
#[ignore]
async fn read_only_runs_and_the_viewer_token() {
    let mut c = config(&[
        ("auth_mode", "token".to_string()),
        ("access_token", env("VIEWER_TOKEN")),
        ("workspace", env("WORKSPACE_PRO")),
    ]);
    c.read_only = true;
    let mut s = open(&c).await;
    let mut out = QueryOutcome::default();
    s.run_read_only("// mcp\nEVALUATE { 1 }", 10, &mut out)
        .await
        .unwrap();
    assert_eq!(out.results.len(), 1);
    // Two EVALUATEs: refused before running.
    let mut out = QueryOutcome::default();
    assert!(s
        .run_read_only("EVALUATE {1}\nEVALUATE {2}", 10, &mut out)
        .await
        .is_err());
    // Impersonation as a Build-only viewer (expected: refused, F5).
    let mut c2 = c.clone();
    c2.options
        .insert("rls_user".into(), "someone@contoso.com".into());
    let mut s = open(&c2).await;
    let mut out = QueryOutcome::default();
    let r = s.execute("EVALUATE { 1 }", 10, &mut out).await;
    eprintln!("impersonation as a viewer: {r:?}");
}

#[tokio::test]
#[ignore]
async fn the_browser_sign_ins() {
    // Opens the browser: the DBine app (consent), then the Azure CLI client
    // with `.default` (the fallback the consent error names).
    for app in ["dbine", "azure_cli"] {
        let mut c = config(&[
            ("auth_mode", "entra_mfa".to_string()),
            ("tenant", env("TENANT")),
            ("client_app", app.to_string()),
            ("workspace", env("WORKSPACE_PRO")),
        ]);
        c.username = Some(env("USER"));
        let mut s = driver().connect(&c, None).await.expect(app);
        assert!(!s.list_databases().await.unwrap().is_empty(), "{app}");
    }
}

#[tokio::test]
#[ignore]
async fn the_default_chain() {
    // `az login` (or the AZURE_* variables) on the machine running it.
    let c = config(&[
        ("auth_mode", "entra_default".to_string()),
        ("tenant", env("TENANT")),
        ("workspace", env("WORKSPACE_PRO")),
    ]);
    let mut s = driver().connect(&c, None).await.expect("connect");
    assert!(!s.list_databases().await.unwrap().is_empty());
}

#[tokio::test]
#[ignore]
async fn the_query_window_waits_instead_of_failing() {
    // 130 calls in a row: the 121st waits for the window (it says so), and
    // a 429 from the service, if any, is retried with its Retry-After.
    let mut s = open(&sp(&env("WORKSPACE_PRO"), &[("query_api", "json")])).await;
    let mut waited = false;
    for _ in 0..130 {
        let out = run(s.as_mut(), "EVALUATE { 1 }", 1).await;
        waited |= out
            .messages
            .iter()
            .any(|m| m.starts_with("Esperando el límite de consultas"));
    }
    assert!(waited);
}
