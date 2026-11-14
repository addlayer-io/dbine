//! The query module against a fake Power BI (answers shaped like the
//! Microsoft Learn samples; Arrow streams written with `arrow-ipc`).

use super::arrow::tests::{error_rowset, every_type, ints, stream};
use super::*;
use crate::catalog::{Model, Workspace};
use crate::http::testing::{ctx_at, serve, settings, Reply, Request};
use crate::meta::{Column, DataType, ModelMeta, Table};
use dbine_driver::{RowSink, RowSinkRef};
use serde_json::json;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

const LEARN: &str = include_str!("../../tests/fixtures/execute_queries.json");
const DAX_ERROR: &str = include_str!("../../tests/fixtures/execute_queries_error.json");

static NEXT: AtomicU64 = AtomicU64::new(1);

/// A model of its own (the host remembers the Arrow fallback per model).
fn model(dedicated: bool) -> Model {
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    Model {
        id: format!("aaaaaaaa-0000-0000-0000-{n:012}"),
        name: "Ventas".into(),
        listed_as: "Ventas".into(),
        workspace: Workspace {
            id: Some("a2f89923-421a-464e-bf4c-25eab39bb09f".into()),
            name: "contoso".into(),
            on_dedicated_capacity: dedicated,
            ..Default::default()
        },
        ..Default::default()
    }
}

type Log = Arc<std::sync::Mutex<Vec<(String, Value)>>>;

/// A fake that answers Arrow with `arrow` and JSON with `json`, logging
/// each query (path, body).
async fn fake(
    arrow: impl Fn(&Value) -> Reply + Send + Sync + 'static,
    json_reply: impl Fn(&Value) -> Reply + Send + Sync + 'static,
) -> (String, Log) {
    let log: Log = Arc::default();
    let l = log.clone();
    let base = serve(Arc::new(move |req: &Request| {
        let body: Value = serde_json::from_str(&req.body).unwrap_or(Value::Null);
        l.lock().unwrap().push((req.path.clone(), body.clone()));
        if req.path.ends_with("/executeDaxQueries") {
            arrow(&body)
        } else if req.path.ends_with("/executeQueries") {
            json_reply(&body)
        } else {
            Reply::json(404, "{}")
        }
    }))
    .await;
    (base, log)
}

fn cx_on(base: &str, opts: &[(&str, &str)], dedicated: bool) -> Ctx {
    let mut cx = ctx_at(base, settings(opts, true));
    cx.model = Some(model(dedicated));
    cx
}

fn xmla_off() -> Reply {
    Reply::json(
        400,
        r#"{"error":{"code":"ExecuteDaxQueriesNotSupported","message":"The XMLA endpoint is disabled for this tenant."}}"#,
    )
}

#[tokio::test]
async fn json_runs_the_learn_sample() {
    let (base, log) = fake(|_| xmla_off(), |_| Reply::json(200, LEARN)).await;
    let mut cx = cx_on(&base, &[("query_api", "json")], false);
    let mut out = QueryOutcome::default();
    execute(&mut cx, "EVALUATE VALUES(MyTable)", 1000, &mut out)
        .await
        .unwrap();
    assert_eq!(out.results.len(), 1);
    let r = &out.results[0];
    let names: Vec<&str> = r.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["MyTable[Year]", "MyTable[Quarter]"]);
    assert_eq!(r.rows[2], vec![json!(2011), json!("Q1")]);
    assert_eq!(api_in_use(&cx), Some(ApiKind::Json));
    let log = log.lock().unwrap();
    assert_eq!(log.len(), 1);
    let model_id = &cx.model.as_ref().unwrap().id;
    assert_eq!(
        log[0].0,
        format!("/v1.0/myorg/groups/a2f89923-421a-464e-bf4c-25eab39bb09f/datasets/{model_id}/executeQueries")
    );
    assert_eq!(log[0].1["queries"][0]["query"], "EVALUATE VALUES(MyTable)");
    assert_eq!(log[0].1["serializerSettings"]["includeNulls"], true);
    assert_eq!(cx.api.window_usage().used, 1);
}

#[tokio::test]
async fn a_define_unit_on_arrow_names_each_result_by_its_evaluate() {
    let (base, log) = fake(
        |_| {
            let mut body = stream(&ints("[x]", &[1]));
            body.extend(stream(&ints("[y]", &[2, 3])));
            Reply::arrow(body)
        },
        |_| Reply::json(500, "{}"),
    )
    .await;
    let mut cx = cx_on(&base, &[], true);
    let unit = "DEFINE VAR x = 1\n// uno\nEVALUATE { x }\n// dos\nEVALUATE { x + 1 }";
    let mut out = QueryOutcome::default();
    execute(&mut cx, unit, 1, &mut out).await.unwrap();
    assert_eq!(out.results.len(), 2);
    let at = |i: usize| out.results[i].offset.unwrap();
    assert!(unit[at(0)..].starts_with("EVALUATE { x }"));
    assert!(unit[at(1)..].starts_with("EVALUATE { x + 1 }"));
    assert_eq!(
        (out.results[0].line, out.results[1].line),
        (Some(3), Some(5))
    );
    // max_rows 1: the second result is cut by the grid, not by the API.
    assert!(out.results[1].truncated && out.results[1].rows.len() == 1);
    assert!(out
        .log
        .iter()
        .all(|m| m.level != dbine_driver::MessageLevel::Warning));
    let log = log.lock().unwrap();
    assert_eq!(log.len(), 1, "one request for the whole unit");
    assert_eq!(log[0].1["query"], unit);
    assert_eq!(log[0].1["resultSetRowCountLimit"], 2);
    assert_eq!(log[0].1["queryTimeout"], 600);
    assert_eq!(api_in_use(&cx), Some(ApiKind::Arrow));
}

#[tokio::test]
async fn without_arrow_the_model_falls_back_to_json_once() {
    let (base, log) = fake(|_| xmla_off(), |_| Reply::json(200, LEARN)).await;
    let mut cx = cx_on(&base, &[], true);
    let unit = "DEFINE MEASURE 'T'[m] = 1\nEVALUATE {[m]}\nEVALUATE {[m] + 1}";
    let mut out = QueryOutcome::default();
    execute(&mut cx, unit, 100, &mut out).await.unwrap();
    assert_eq!(out.results.len(), 2);
    let warnings: Vec<&str> = out
        .log
        .iter()
        .filter(|m| m.level == dbine_driver::MessageLevel::Warning)
        .map(|m| m.text.as_str())
        .collect();
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("Allow XMLA endpoints"), "{warnings:?}");
    {
        let log = log.lock().unwrap();
        // Arrow once, then one JSON request per EVALUATE, each with the DEFINE.
        assert_eq!(log.len(), 3);
        assert!(log[0].0.ends_with("/executeDaxQueries"));
        assert_eq!(
            log[1].1["queries"][0]["query"],
            "DEFINE MEASURE 'T'[m] = 1\nEVALUATE {[m]}\n"
        );
        assert_eq!(
            log[2].1["queries"][0]["query"],
            "DEFINE MEASURE 'T'[m] = 1\nEVALUATE {[m] + 1}"
        );
    }
    // The model stays on JSON, and it isn't said again in this session.
    let mut out = QueryOutcome::default();
    execute(&mut cx, "EVALUATE {1}", 100, &mut out)
        .await
        .unwrap();
    assert!(out
        .log
        .iter()
        .all(|m| m.level != dbine_driver::MessageLevel::Warning));
    assert_eq!(log.lock().unwrap().len(), 4);
    assert!(log.lock().unwrap()[3].0.ends_with("/executeQueries"));
    let target = Target::of(cx.model.as_ref().unwrap());
    assert!(!arrow_available(&mut cx, &target).await.unwrap());
}

#[tokio::test]
async fn forcing_arrow_fails_with_the_reason() {
    let (base, log) = fake(|_| Reply::json(404, "{}"), |_| Reply::json(200, LEARN)).await;
    let mut cx = cx_on(&base, &[("query_api", "arrow")], false);
    let mut out = QueryOutcome::default();
    let e = execute(&mut cx, "EVALUATE {1}", 10, &mut out)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("capacidad compartida"), "{e}");
    assert_eq!(log.lock().unwrap().len(), 1);
    // The driver's own reads that need Arrow get Unsupported.
    let mut cx = cx_on(&base, &[], false);
    let target = Target::of(cx.model.as_ref().unwrap());
    let opts = QueryOpts {
        api: Some(ApiKind::Arrow),
        ..QueryOpts::metadata()
    };
    let e = run(&mut cx, &target, "EVALUATE INFO.VIEW.TABLES()", &opts)
        .await
        .unwrap_err();
    assert!(matches!(e, Error::Unsupported(_)), "{e}");
}

#[tokio::test]
async fn cuts_are_marked_in_the_grid_and_refused_in_sink_mode() {
    let cut = r#"{"results":[{"tables":[{"rows":[{"[a]":1},{"[a]":2}],"error":{"code":"x","message":"More than 100000 rows in a query result"}}]}]}"#;
    let (base, _) = fake(|_| xmla_off(), move |_| Reply::json(200, cut)).await;
    let mut cx = cx_on(&base, &[("query_api", "json")], false);
    let mut out = QueryOutcome::default();
    execute(&mut cx, "EVALUATE 'T'", 1000, &mut out)
        .await
        .unwrap();
    assert!(out.results[0].truncated);
    assert!(out.log.iter().any(|m| m.text == json::CUT));
    // Export / chart / notebook loads: an error, never a silent partial file.
    struct Count(usize);
    impl RowSink for Count {
        fn begin(&mut self, _: usize, _: &[ResultColumn]) -> std::io::Result<()> {
            Ok(())
        }
        fn row(&mut self, _: usize, _: &[Value]) -> std::io::Result<()> {
            self.0 += 1;
            Ok(())
        }
    }
    let mut out = QueryOutcome {
        sink: Some(RowSinkRef(Arc::new(std::sync::Mutex::new(Count(0))))),
        ..Default::default()
    };
    let e = execute(&mut cx, "EVALUATE 'T'", usize::MAX, &mut out)
        .await
        .unwrap_err();
    assert!(
        e.to_string().contains("no se escribe un resultado parcial"),
        "{e}"
    );
}

/// A sink that counts the rows streamed to it (export, chart, notebook).
#[derive(Clone, Default)]
struct Counted(Arc<AtomicUsize>);

impl RowSink for Counted {
    fn begin(&mut self, _: usize, _: &[ResultColumn]) -> std::io::Result<()> {
        Ok(())
    }
    fn row(&mut self, _: usize, _: &[Value]) -> std::io::Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn sink_outcome(sink: &Counted) -> QueryOutcome {
    QueryOutcome {
        sink: Some(RowSinkRef(Arc::new(std::sync::Mutex::new(sink.clone())))),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_cut_past_the_callers_cap_streams_in_sink_mode() {
    let (base, log) = fake(
        |b| {
            let n = b["resultSetRowCountLimit"].as_i64().unwrap();
            // The service returns exactly the limit: there may be more.
            let values: Vec<i64> = (0..n.min(5)).collect();
            Reply::arrow(stream(&ints("[a]", &values)))
        },
        |_| Reply::json(500, "{}"),
    )
    .await;
    let mut cx = cx_on(&base, &[], true);
    // A chart's cap of 3 (it asks for 3 + 1): the limit is 4 and 4 come
    // back. More than it asked for: its own cap, not the service's.
    let rows = Counted::default();
    let mut out = sink_outcome(&rows);
    execute(&mut cx, "EVALUATE 'T'", 4, &mut out).await.unwrap();
    assert_eq!(log.lock().unwrap()[0].1["resultSetRowCountLimit"], 5);
    assert_eq!(rows.0.load(Ordering::SeqCst), 5);
    let rows = Counted::default();
    let mut out = sink_outcome(&rows);
    execute(&mut cx, "EVALUATE 'T'", 3, &mut out).await.unwrap();
    assert_eq!(log.lock().unwrap()[1].1["resultSetRowCountLimit"], 4);
    assert_eq!(rows.0.load(Ordering::SeqCst), 4);
    // The same through the transfer sink (DataFrame SQL loads).
    let mut out = QueryOutcome {
        sink: Some(RowSinkRef(Arc::new(std::sync::Mutex::new(
            dbine_driver::transfer::JsonBatches::new(Arc::new(std::sync::Mutex::new(Drop0))),
        )))),
        ..Default::default()
    };
    execute(&mut cx, "EVALUATE 'T'", 3, &mut out).await.unwrap();
    // Under the limit: whole.
    let mut out = QueryOutcome::default();
    execute(&mut cx, "EVALUATE 'T'", 10, &mut out)
        .await
        .unwrap();
    assert_eq!(out.results[0].rows.len(), 5);
    assert!(!out.results[0].truncated);
}

#[tokio::test]
async fn a_json_cut_in_sink_mode_errors_only_at_or_under_the_cap() {
    let cut = r#"{"results":[{"tables":[{"rows":[{"[a]":1},{"[a]":2}],"error":{"code":"x","message":"More than 100000 rows in a query result"}}]}]}"#;
    let (base, _) = fake(|_| xmla_off(), move |_| Reply::json(200, cut)).await;
    let mut cx = cx_on(&base, &[("query_api", "json")], false);
    // The caller asked for 1 + 1 and got 2: its cap, whatever the service
    // says.
    let rows = Counted::default();
    let mut out = sink_outcome(&rows);
    execute(&mut cx, "EVALUATE 'T'", 1, &mut out).await.unwrap();
    assert_eq!(rows.0.load(Ordering::SeqCst), 2);
    // The caller would take 5: the service's cut is a partial result.
    let mut out = sink_outcome(&Counted::default());
    let e = execute(&mut cx, "EVALUATE 'T'", 5, &mut out)
        .await
        .unwrap_err();
    assert!(
        e.to_string().contains("no se escribe un resultado parcial"),
        "{e}"
    );
    // Exactly at the cap: still the service's.
    let mut out = sink_outcome(&Counted::default());
    assert!(execute(&mut cx, "EVALUATE 'T'", 2, &mut out).await.is_err());
}

#[test]
fn refusals_are_classified() {
    let f = |status: u16, body: &str| Failure {
        status,
        body: serde_json::from_str(body).unwrap_or(Value::Null),
        request_id: None,
    };
    let lasting = |r: Option<Refusal>| matches!(r, Some(Refusal::Lasting(_)));
    let for_now = |r: Option<Refusal>| matches!(r, Some(Refusal::ForNow(_)));
    // The path isn't there.
    assert!(lasting(refusal(&f(404, "{}"), false)));
    assert!(lasting(refusal(&f(405, ""), false)));
    // The XMLA tenant setting, and shared capacity.
    assert!(lasting(refusal(
        &f(
            400,
            r#"{"error":{"code":"ExecuteDaxQueriesNotSupported","message":"The XMLA endpoint is disabled for this tenant."}}"#
        ),
        false
    )));
    assert!(lasting(refusal(
        &f(
            403,
            r#"{"error":{"code":"x","message":"This API is not supported on shared capacity."}}"#
        ),
        true
    )));
    // "Not available" with no cause DBine knows: for a while.
    assert!(for_now(refusal(
        &f(
            403,
            r#"{"error":{"code":"FeatureNotAvailable","message":"The feature is not available."}}"#
        ),
        false
    )));
    // A paused or inactive capacity: this query only.
    for body in [
        r#"{"error":{"code":"CapacityNotActive","message":"The capacity is not active."}}"#,
        r#"{"error":{"code":"x","message":"The Premium capacity is paused."}}"#,
        r#"{"error":{"code":"PowerBICapacitySuspended"}}"#,
    ] {
        assert_eq!(
            refusal(&f(400, body), false),
            Some(Refusal::Inactive),
            "{body}"
        );
    }
    // Anything else is the query's or the account's error, whatever words
    // a DAX error uses.
    assert_eq!(
        refusal(
            &f(
                400,
                r#"{"error":{"code":"x","message":"Query (2, 5) Function 'X' is not supported in this context; the column 'Paused' is inactive."}}"#
            ),
            false
        ),
        None
    );
    assert_eq!(
        refusal(
            &f(
                400,
                r#"{"error":{"code":"DatasetExecuteQueriesError","message":"Query (1, 1) The syntax for 'x' is incorrect."}}"#
            ),
            false
        ),
        None
    );
    assert_eq!(
        refusal(
            &f(403, r#"{"error":{"code":"PowerBINotAuthorizedException"}}"#),
            false
        ),
        None
    );
    assert_eq!(refusal(&f(500, "{}"), false), None);
}

#[tokio::test]
async fn a_paused_capacity_fails_the_query_and_isnt_remembered() {
    let paused = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let p = paused.clone();
    let (base, log) = fake(
        move |_| {
            if p.load(Ordering::SeqCst) {
                Reply::json(
                    400,
                    r#"{"error":{"code":"CapacityNotActive","message":"The capacity is paused."}}"#,
                )
            } else {
                Reply::arrow(stream(&ints("[a]", &[1])))
            }
        },
        |_| Reply::json(200, LEARN),
    )
    .await;
    let mut cx = cx_on(&base, &[], true);
    let mut out = QueryOutcome::default();
    let e = execute(&mut cx, "EVALUATE {1}", 10, &mut out)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("en pausa"), "{e}");
    assert_eq!(log.lock().unwrap().len(), 1, "no fallback to JSON");
    // Resumed: Arrow again, and nothing was remembered against it.
    paused.store(false, Ordering::SeqCst);
    let mut out = QueryOutcome::default();
    execute(&mut cx, "EVALUATE {1}", 10, &mut out)
        .await
        .unwrap();
    assert!(log.lock().unwrap()[1].0.ends_with("/executeDaxQueries"));
    assert_eq!(api_in_use(&cx), Some(ApiKind::Arrow));
}

#[tokio::test]
async fn a_refusal_is_remembered_per_account_and_never_by_forced_reads() {
    let (base, log) = fake(|_| xmla_off(), |_| Reply::json(200, LEARN)).await;
    // A forced metadata read refused (INFO functions on Arrow) leaves the
    // model's queries alone.
    let mut a = cx_on(&base, &[], true);
    let target = Target::of(a.model.as_ref().unwrap());
    let opts = QueryOpts {
        api: Some(ApiKind::Arrow),
        ..QueryOpts::metadata()
    };
    assert!(matches!(
        run(&mut a, &target, "EVALUATE INFO.VIEW.TABLES()", &opts).await,
        Err(Error::Unsupported(_))
    ));
    assert_eq!(api_expected(&a), None);
    let mut out = QueryOutcome::default();
    execute(&mut a, "EVALUATE {1}", 10, &mut out).await.unwrap();
    {
        let log = log.lock().unwrap();
        assert!(log[1].0.ends_with("/executeDaxQueries"), "Arrow was asked");
        assert!(log[2].0.ends_with("/executeQueries"));
    }
    // The user's own refusal is remembered for that account (another
    // session of it goes straight to JSON)…
    assert!(matches!(memo(&a, &target), Some(Memo::Unavailable(..))));
    let mut same = ctx_at(&base, a.settings.clone());
    same.model = a.model.clone();
    same.api
        .tokens
        .token(crate::auth::Audience::PowerBi)
        .await
        .unwrap();
    let mut out = QueryOutcome::default();
    execute(&mut same, "EVALUATE {1}", 10, &mut out)
        .await
        .unwrap();
    assert!(log.lock().unwrap()[3].0.ends_with("/executeQueries"));
    // …not for another account on the same model.
    let mut b = cx_on(&base, &[], true);
    b.model = a.model.clone();
    b.api
        .tokens
        .token(crate::auth::Audience::PowerBi)
        .await
        .unwrap();
    assert_eq!(memo(&b, &target), None);
    let mut out = QueryOutcome::default();
    execute(&mut b, "EVALUATE {1}", 10, &mut out).await.unwrap();
    assert!(log.lock().unwrap()[4].0.ends_with("/executeDaxQueries"));
}

#[tokio::test]
async fn a_refusal_for_now_expires() {
    let (base, _) = fake(
        |_| Reply::arrow(stream(&ints("[a]", &[1]))),
        |_| Reply::json(500, "{}"),
    )
    .await;
    let mut cx = cx_on(&base, &[], true);
    // Any token call binds the account the memory is keyed by.
    let mut out = QueryOutcome::default();
    execute(&mut cx, "EVALUATE {1}", 10, &mut out)
        .await
        .unwrap();
    let target = Target::of(cx.model.as_ref().unwrap());
    let past = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    remember(&cx, &target, Memo::Unavailable("x".into(), Some(past)));
    assert_eq!(memo(&cx, &target), None, "expired");
    remember(&cx, &target, unavailable("x".into(), Some(RETRY_ARROW)));
    assert!(matches!(memo(&cx, &target), Some(Memo::Unavailable(..))));
    assert_eq!(choose(&cx, &target, None), ApiKind::Json);
}

struct Drop0;
impl dbine_driver::transfer::BatchSink for Drop0 {
    fn begin(&mut self, _: &[dbine_driver::transfer::TransferColumn]) -> std::io::Result<()> {
        Ok(())
    }
    fn batch(&mut self, _: dbine_driver::transfer::RowBatch) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn dax_errors_point_at_the_unit() {
    // JSON: "Query (3, 12)" of the second EVALUATE's request → its place
    // in the unit.
    let n = Arc::new(AtomicUsize::new(0));
    let c = n.clone();
    let (base, _) = fake(
        |_| xmla_off(),
        move |_| {
            if c.fetch_add(1, Ordering::SeqCst) == 0 {
                Reply::json(200, LEARN)
            } else {
                Reply::json(400, DAX_ERROR)
            }
        },
    )
    .await;
    let mut cx = cx_on(&base, &[("query_api", "json")], false);
    let unit = "DEFINE VAR x = 1\nEVALUATE {x}\nEVALUATE\n  ROW(\"x\", SUMX(Sales[Amount]))";
    let mut out = QueryOutcome::default();
    let Err(Error::Statement(e)) = execute(&mut cx, unit, 10, &mut out).await else {
        panic!("expected a statement error");
    };
    // The results before the error stay.
    assert_eq!(out.results.len(), 1);
    assert_eq!(e.code.as_deref(), Some("3238002695"));
    // The second request is "DEFINE VAR x = 1\nEVALUATE\n  ROW(…)": its
    // line 3, column 12 is "SUMX", on line 4 of the unit.
    assert_eq!(e.line, Some(4));
    assert!(
        unit[e.offset.unwrap()..].starts_with("SUMX"),
        "{:?}",
        &unit[e.offset.unwrap()..]
    );
    // Arrow: the error rowset, line 2 column 5 of the unit.
    let (base, _) = fake(
        |_| {
            let mut b = stream(&ints("[ok]", &[1]));
            b.extend(stream(&error_rowset("The syntax for 'x' is incorrect.")));
            Reply::arrow(b)
        },
        |_| Reply::json(500, "{}"),
    )
    .await;
    let mut cx = cx_on(&base, &[], true);
    let unit = "DEFINE VAR a = 1\nEVAL {x}\nEVALUATE {2}";
    let mut out = QueryOutcome::default();
    let Err(Error::Statement(e)) = execute(&mut cx, unit, 10, &mut out).await else {
        panic!("expected a statement error");
    };
    assert_eq!(e.message, "The syntax for 'x' is incorrect.");
    assert_eq!((e.line, e.offset), (Some(2), Some(21)));
    assert_eq!(out.results.len(), 1);
}

#[tokio::test]
async fn only_queries_run_and_read_only_runs_take_one_evaluate() {
    let (base, log) = fake(
        |_| Reply::arrow(stream(&ints("[a]", &[1]))),
        |_| Reply::json(200, LEARN),
    )
    .await;
    let mut cx = cx_on(&base, &[], true);
    let mut out = QueryOutcome::default();
    for bad in ["SELECT 1", "GRANT Read TO \"x\"", "// nothing", "{1}"] {
        let e = execute(&mut cx, bad, 10, &mut out).await.unwrap_err();
        assert!(matches!(e, Error::Query(_)), "{bad}: {e}");
    }
    for bad in [
        "EVALUATE {1}\nEVALUATE {2}",
        "DEFINE VAR x = 1 EVALUATE {x} EVALUATE {x}",
        "DEFINE VAR x = 1",
        "SELECT 1",
    ] {
        let e = run_read_only(&mut cx, bad, 10, &mut out).await.unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{bad}: {e}");
    }
    assert!(log.lock().unwrap().is_empty(), "nothing was sent");
    let mut out = QueryOutcome::default();
    run_read_only(&mut cx, "// the chart\nEVALUATE 'T'", 50, &mut out)
        .await
        .unwrap();
    assert_eq!(out.results[0].rows, vec![vec![json!(1)]]);
    let log = log.lock().unwrap();
    assert_eq!(log[0].1["resultSetRowCountLimit"], 51);
    assert_eq!(log[0].1["queryTimeout"], 600);
}

#[tokio::test]
async fn the_drivers_browse_shows_the_models_column_names() {
    let (base, _) = fake(
        |_| Reply::arrow(stream(&every_type())),
        |_| Reply::json(200, r#"{"results":[{"tables":[{"rows":[]}]}]}"#),
    )
    .await;
    let mut cx = cx_on(&base, &[], true);
    let mut out = QueryOutcome::default();
    execute(&mut cx, "EVALUATE TOPN(200, 'Sales')", 200, &mut out)
        .await
        .unwrap();
    let names: Vec<&str> = out.results[0]
        .columns
        .iter()
        .map(|c| c.name.as_str())
        .collect();
    // Sales' own columns lose the prefix; another table's keep theirs.
    assert_eq!(names[..3], ["Qty", "Ratio", "Amount"]);
    assert_eq!(names[3], "Date[Date]");
    assert_eq!(out.results[0].columns[2].type_name, "Decimal");
    // An empty JSON browse: the columns from the metadata.
    let mut cx = cx_on(&base, &[("query_api", "json")], false);
    cx.meta.cached = Some(Arc::new(ModelMeta {
        tables: vec![Table {
            name: "Sales".into(),
            columns: vec![
                Column {
                    name: "Qty".into(),
                    data_type: DataType::Int64,
                    ..Default::default()
                },
                Column {
                    name: "Day".into(),
                    data_type: DataType::DateTime,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }],
        ..Default::default()
    }));
    let mut out = QueryOutcome::default();
    execute(&mut cx, "EVALUATE TOPN(200, 'Sales')", 200, &mut out)
        .await
        .unwrap();
    let cols: Vec<(&str, &str)> = out.results[0]
        .columns
        .iter()
        .map(|c| (c.name.as_str(), c.type_name.as_str()))
        .collect();
    assert_eq!(cols, [("Qty", "Int64"), ("Day", "DateTime")]);
    assert!(out.results[0].rows.is_empty());
}

#[tokio::test]
async fn testing_rls_goes_in_user_queries_only() {
    let (base, log) = fake(
        |_| Reply::arrow(stream(&ints("[a]", &[1]))),
        |_| Reply::json(200, LEARN),
    )
    .await;
    let opts = [
        ("rls_user", "ana@contoso.com"),
        ("rls_roles", "Norte, Sur"),
        ("custom_data", "c1"),
    ];
    let mut cx = cx_on(&base, &opts, true);
    let mut out = QueryOutcome::default();
    execute(&mut cx, "EVALUATE {1}", 10, &mut out)
        .await
        .unwrap();
    assert!(out
        .messages
        .iter()
        .any(|m| m.starts_with("Consultando como ana@contoso.com")));
    let target = Target::of(cx.model.as_ref().unwrap());
    run(
        &mut cx,
        &target,
        "EVALUATE INFO.VIEW.TABLES()",
        &QueryOpts::metadata(),
    )
    .await
    .unwrap();
    {
        let log = log.lock().unwrap();
        assert_eq!(log[0].1["effectiveUsername"], "ana@contoso.com");
        assert_eq!(log[0].1["roles"], json!(["Norte", "Sur"]));
        assert_eq!(log[0].1["customData"], "c1");
        // Metadata reads are never impersonated (G13).
        assert!(log[1].1.get("effectiveUsername").is_none() && log[1].1.get("roles").is_none());
    }
    // On JSON: the user only, and a note that roles don't apply.
    let mut cx = cx_on(&base, &[opts[0], opts[1], ("query_api", "json")], false);
    let mut out = QueryOutcome::default();
    execute(&mut cx, "EVALUATE {1}", 10, &mut out)
        .await
        .unwrap();
    let last = log.lock().unwrap().last().unwrap().1.clone();
    assert_eq!(last["impersonatedUserName"], "ana@contoso.com");
    assert!(out
        .log
        .iter()
        .any(|m| m.text.contains("solo se aplican con executeDaxQueries")));
}

#[tokio::test]
async fn labels_and_runs_for_the_driver() {
    let labelled = r#"{"results":[{"tables":[{"rows":[{"[a]":1}]}]}],"informationProtectionLabel":{"id":"x","name":"Confidencial"}}"#;
    let (base, log) = fake(|_| xmla_off(), move |_| Reply::json(200, labelled)).await;
    let mut cx = cx_on(&base, &[], false);
    let mut out = QueryOutcome::default();
    execute(&mut cx, "EVALUATE {1}", 10, &mut out)
        .await
        .unwrap();
    assert!(out.messages.iter().any(|m| m.contains("«Confidencial»")));
    // On shared capacity the fallback is expected: an info, not a warning.
    assert!(out
        .log
        .iter()
        .all(|m| m.level != dbine_driver::MessageLevel::Warning));
    let target = Target::of(cx.model.as_ref().unwrap());
    let sets = run(
        &mut cx,
        &target,
        "DEFINE VAR x = 1 EVALUATE {x} EVALUATE {x}",
        &QueryOpts::metadata(),
    )
    .await
    .unwrap();
    assert_eq!(sets.len(), 2);
    assert_eq!(sets[0].columns, ["[a]"]);
    // A background read: the window counts it too.
    assert!(log.lock().unwrap().len() >= 3);
}

#[tokio::test]
async fn arrow_availability_is_probed_once() {
    let n = Arc::new(AtomicUsize::new(0));
    let c = n.clone();
    let (base, _) = fake(
        move |_| {
            c.fetch_add(1, Ordering::SeqCst);
            Reply::arrow(stream(&ints("[Value]", &[1])))
        },
        |_| Reply::json(500, "{}"),
    )
    .await;
    let mut cx = cx_on(&base, &[], false);
    let target = Target::of(cx.model.as_ref().unwrap());
    assert!(arrow_available(&mut cx, &target).await.unwrap());
    assert!(arrow_available(&mut cx, &target).await.unwrap());
    assert_eq!(n.load(Ordering::SeqCst), 1);
    assert_eq!(api_expected(&cx), Some(ApiKind::Arrow));
    let cx = cx_on(&base, &[("query_api", "json")], false);
    assert_eq!(api_expected(&cx), Some(ApiKind::Json));
}

#[test]
fn offsets_of_lines_and_columns() {
    let t = "ab\ncdé\nf";
    assert_eq!(offset_at(t, 1, 1), Some(0));
    assert_eq!(offset_at(t, 2, 3), Some(5));
    assert_eq!(offset_at(t, 2, 9), Some(7));
    assert_eq!(offset_at(t, 3, 1), Some(8));
    assert_eq!(offset_at(t, 4, 1), None);
    assert_eq!(arrow_limit(usize::MAX), i32::MAX as u64);
    assert_eq!(arrow_limit(200), 201);
}
