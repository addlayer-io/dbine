//! Unit tests of "Ejecutar para fechas pasadas": the form, the checks, the
//! request and its script, and the calls against a stand-in service.

use super::super::tests::{api_at, body_in, config, cur, map, serve, warned, Seen, NAME};
use super::*;
use crate::scheduler::current_of;
use crate::Api;
use std::sync::Arc;

fn at(t: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(t).unwrap().with_timezone(&Utc)
}

/// The clock of most tests: midday on 2026-11-13, UTC.
fn now() -> DateTime<Utc> {
    at("2026-11-13T12:00:00Z")
}

/// The config, run only on demand.
fn manual() -> Current {
    let mut j = config();
    j.as_object_mut().unwrap().remove("schedule");
    j["scheduleOptions"] = json!({ "disableAutoScheduling": true });
    current_of(&j).unwrap()
}

fn dts(api: &Api) -> Dts<'_> {
    Dts {
        api,
        base: crate::jobs::DTS,
    }
}

fn err<T: std::fmt::Debug>(r: Result<T>) -> String {
    match r {
        Err(Error::Query(m)) => m,
        other => panic!("expected an error, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

#[test]
fn the_form_offers_a_period_only_on_a_schedule() {
    let f = form_for(&cur(), now());
    assert_eq!(f.values["mode"], json!("range"));
    // Yesterday as the period; one run at the time of the next, a day
    // earlier.
    assert_eq!(f.values["start_date"], json!("2026-11-12"));
    assert_eq!(f.values["end_date"], json!("2026-11-12"));
    assert_eq!(f.values["run_time"], json!("2026-11-12T03:00:00Z"));
    assert_eq!(f.values["schedule"], json!("every 24 hours"));
    let fields = &f.sections[0].fields;
    let keys: Vec<&str> = fields.iter().map(|x| x.key.as_str()).collect();
    assert_eq!(
        keys,
        ["mode", "start_date", "end_date", "run_time", "schedule"]
    );
    match &fields[0].kind {
        FormKind::Select { options } => {
            let values: Vec<&str> = options.iter().map(|(v, _)| v.as_str()).collect();
            assert_eq!(values, ["range", "once"]);
        }
        k => panic!("{k:?}"),
    }
    assert_eq!(fields[1].when, Some(("mode".into(), vec!["range".into()])));
    assert_eq!(fields[3].when, Some(("mode".into(), vec!["once".into()])));
    // Each date its mode needs is required: the dialog flags a cleared one
    // (only while it shows) before the script is asked for.
    let required: Vec<&str> = fields
        .iter()
        .filter(|x| x.required)
        .map(|x| x.key.as_str())
        .collect();
    assert_eq!(required, ["mode", "start_date", "end_date", "run_time"]);
    assert!(fields[4].read_only);
    assert_eq!(f.notes, [IN_UTC]);
    assert!(!f.read_only);

    // On demand: one run only, and a note says why.
    let f = form_for(&manual(), now());
    assert_eq!(f.values["mode"], json!("once"));
    match &f.sections[0].fields[0].kind {
        FormKind::Select { options } => assert_eq!(options.len(), 1),
        k => panic!("{k:?}"),
    }
    assert!(f.sections[0].fields.iter().all(|x| x.key != "schedule"));
    assert_eq!(f.notes, [IN_UTC, NO_SCHEDULE]);

    // A next run more than a day away: one run defaults to today's
    // midnight.
    let f = form_for(&cur(), at("2026-11-12T02:00:00Z"));
    assert_eq!(f.values["run_time"], json!("2026-11-12T00:00:00Z"));
    assert_eq!(f.values["end_date"], json!("2026-11-11"));
}

// ---------------------------------------------------------------------------
// The request and its script
// ---------------------------------------------------------------------------

#[test]
fn a_period_is_whole_days_in_utc() {
    let api = api_at("http://localhost:1");
    let d = dts(&api);
    let v = map(json!({ "mode": "range", "start_date": "2026-11-01", "end_date": "2026-11-07" }));
    let p = plan_for(&d, &cur(), Vals(&v), now()).unwrap();
    assert_eq!(p.reqs.len(), 1);
    assert_eq!(p.reqs[0].method, Method::POST);
    assert_eq!(
        p.reqs[0].url.as_str(),
        format!("https://bigquerydatatransfer.googleapis.com/v1/{NAME}:startManualRuns")
    );
    // The last day is included: the range ends at the midnight after it.
    assert_eq!(
        p.reqs[0].body,
        Some(json!({ "requestedTimeRange": {
            "startTime": "2026-11-01T00:00:00Z",
            "endTime": "2026-11-08T00:00:00Z"
        } }))
    );
    let sc = p.script();
    assert!(sc.starts_with("# Ejecuta la consulta programada «ventas diarias» de BigQuery para los días del 2026-11-01 al 2026-11-07 (7 días, en UTC)"), "{sc}");
    assert!(sc.contains(&format!("# {SPACED}\n")), "{sc}");
    assert!(sc.contains(&format!(
        "# POST https://bigquerydatatransfer.googleapis.com/v1/{NAME}:startManualRuns\n"
    )));
    assert_eq!(body_in(&sc), p.reqs[0].body.clone().unwrap());
    // The config replaces its table each run.
    assert_eq!(warned(&sc), [BILLED, REPLACES]);

    // Yesterday alone: one day.
    let v = map(json!({ "mode": "range", "start_date": "2026-11-12", "end_date": "2026-11-12" }));
    let sc = plan_for(&d, &cur(), Vals(&v), now()).unwrap().script();
    assert!(sc.contains("para el día 2026-11-12 (en UTC)"), "{sc}");
    assert!(sc.contains("\"endTime\": \"2026-11-13T00:00:00Z\""), "{sc}");
}

#[test]
fn once_is_a_past_run_time() {
    let api = api_at("http://localhost:1");
    let d = dts(&api);
    for c in [cur(), manual()] {
        let v = map(json!({ "mode": "once", "run_time": "2026-11-10T03:00:00-03:00" }));
        let p = plan_for(&d, &c, Vals(&v), now()).unwrap();
        assert_eq!(
            p.reqs[0].body,
            Some(json!({ "requestedRunTime": "2026-11-10T06:00:00Z" }))
        );
        assert!(p.script().contains("como si fuera 2026-11-10T06:00:00Z"));
    }
    // The designer's own format is UTC; without a mode, an on-demand
    // config runs once.
    let v = map(json!({ "run_time": "2026-11-10T03:00:00" }));
    let p = plan_for(&d, &manual(), Vals(&v), now()).unwrap();
    assert_eq!(
        p.reqs[0].body,
        Some(json!({ "requestedRunTime": "2026-11-10T03:00:00Z" }))
    );
}

#[test]
fn what_the_service_would_refuse_is_refused_first() {
    let api = api_at("http://localhost:1");
    let d = dts(&api);
    let refused = |c: &Current, v: Json| err(plan_for(&d, c, Vals(&map(v)), now()));
    // A day that hasn't ended, backwards, not a date, missing.
    for end in ["2026-11-13", "2026-11-14"] {
        assert_eq!(
            refused(
                &cur(),
                json!({ "mode": "range", "start_date": "2026-11-01", "end_date": end })
            ),
            NOT_ENDED
        );
    }
    assert_eq!(
        refused(
            &cur(),
            json!({ "mode": "range", "start_date": "2026-11-05", "end_date": "2026-11-04" })
        ),
        BACKWARDS
    );
    for bad_day in ["ayer", "2026-13-01", "2026-11-01T00:00:00Z", "'; DROP"] {
        let m = refused(
            &cur(),
            json!({ "mode": "range", "start_date": bad_day, "end_date": "2026-11-04" }),
        );
        assert!(m.contains("no es una fecha"), "{bad_day}: {m}");
    }
    assert!(
        refused(&cur(), json!({ "mode": "range", "end_date": "2026-11-04" })).contains("«Desde»")
    );
    // A period for a config without a schedule.
    assert_eq!(
        refused(
            &manual(),
            json!({ "mode": "range", "start_date": "2026-11-01", "end_date": "2026-11-02" })
        ),
        RANGE_REFUSED
    );
    // A run time ahead, missing, not one.
    assert_eq!(
        refused(
            &cur(),
            json!({ "mode": "once", "run_time": "2026-11-13T12:00:01Z" })
        ),
        FUTURE
    );
    assert_eq!(
        refused(&cur(), json!({ "mode": "once", "run_time": "" })),
        NEED_TIME
    );
    assert!(
        refused(&cur(), json!({ "mode": "once", "run_time": "mañana" }))
            .contains("no es una fecha y hora válida")
    );
    // A mode it doesn't have.
    assert!(refused(&cur(), json!({ "mode": "all" })).contains("no es una forma de ejecutarla"));
}

#[test]
fn the_warnings_follow_the_destination() {
    let api = api_at("http://localhost:1");
    let d = dts(&api);
    let v = map(json!({ "mode": "once", "run_time": "2026-11-10T00:00:00Z" }));
    let warned_for = |change: &dyn Fn(&mut Json)| {
        let mut j = config();
        change(&mut j);
        let c = current_of(&j).unwrap();
        warned(&plan_for(&d, &c, Vals(&v), now()).unwrap().script())
    };
    assert_eq!(warned_for(&|_| {}), [BILLED, REPLACES]);
    assert_eq!(
        warned_for(&|j| j["params"]["write_disposition"] = json!("WRITE_APPEND")),
        [BILLED, APPENDS]
    );
    assert_eq!(
        warned_for(&|j| {
            j["params"]
                .as_object_mut()
                .unwrap()
                .remove("write_disposition");
        }),
        [BILLED, APPENDS]
    );
    assert_eq!(
        warned_for(&|j| {
            j.as_object_mut().unwrap().remove("destinationDatasetId");
        }),
        [BILLED, CHANGES]
    );
}

#[test]
fn only_its_kind_and_action() {
    assert!(check("job", "backfill").is_ok());
    assert!(check("job", " backfill ").is_ok());
    assert!(matches!(check("job", "run"), Err(Error::Unsupported(_))));
    assert!(matches!(check("job", ""), Err(Error::Unsupported(_))));
    assert!(check("schedule", "backfill").is_err());
}

// ---------------------------------------------------------------------------
// Against a stand-in service
// ---------------------------------------------------------------------------

const OTHER: &str = "projects/123456/locations/us/transferConfigs/gcs-copy";

/// The config above, a Cloud Storage transfer, and `startManualRuns`, which
/// makes no run for a range that starts on 2020-01-01.
async fn service(seen: Seen) -> String {
    serve(Arc::new(move |m, p, b| {
        seen.lock()
            .unwrap()
            .push((m.to_string(), p.to_string(), b.to_string()));
        let cfg = format!("/v1/{NAME}");
        let body = match (m, p) {
            ("GET", x) if x == cfg => config(),
            ("GET", x) if x == format!("/v1/{OTHER}") => {
                json!({ "name": OTHER, "dataSourceId": "google_cloud_storage", "displayName": "copia" })
            }
            ("POST", x) if x == format!("{cfg}:startManualRuns") => {
                if b.contains("2020-01-01") {
                    json!({})
                } else {
                    json!({ "runs": [
                        { "name": format!("{NAME}/runs/a"), "state": "PENDING" },
                        { "name": format!("{NAME}/runs/b"), "state": "PENDING" }
                    ] })
                }
            }
            _ => {
                return (
                    404,
                    json!({ "error": { "code": 404, "message": "Not found" } }).to_string(),
                )
            }
        };
        (200, body.to_string())
    }))
    .await
}

fn fv(v: Json) -> FormValues {
    FormValues {
        values: map(v),
        lists: Default::default(),
    }
}

#[tokio::test]
async fn the_action_calls_the_service_as_documented() {
    let seen: Seen = Default::default();
    let base = service(seen.clone()).await;
    let api = api_at(&base);
    let d = Dts {
        api: &api,
        base: &base,
    };
    let calls = || seen.lock().unwrap().clone();
    let yesterday = (Utc::now() - Duration::days(1))
        .date_naive()
        .format(DAY)
        .to_string();
    let week = (Utc::now() - Duration::days(7))
        .date_naive()
        .format(DAY)
        .to_string();

    // The script reads the config and sends nothing.
    let v = fv(json!({ "mode": "range", "start_date": week, "end_date": yesterday }));
    let sc = plan_with(&d, NAME, &v).await.unwrap().script();
    assert!(sc.contains(":startManualRuns"), "{sc}");
    assert!(calls().iter().all(|c| c.0 == "GET"));

    // The run: one POST with the range, the runs it made.
    assert_eq!(run_with(&d, NAME, &v).await.unwrap(), 2);
    let post = calls().into_iter().find(|c| c.0 == "POST").unwrap();
    assert_eq!(post.1, format!("/v1/{NAME}:startManualRuns"));
    let sent: Json = serde_json::from_str(&post.2).unwrap();
    assert_eq!(
        sent["requestedTimeRange"]["startTime"],
        json!(format!("{week}T00:00:00Z"))
    );

    // Days without a scheduled time: the service makes no run, and says so.
    let v = fv(json!({ "mode": "range", "start_date": "2020-01-01", "end_date": "2020-01-01" }));
    assert_eq!(err(run_with(&d, NAME, &v).await), NONE_MADE);

    // Refused before the service: a bad value, another data source's
    // transfer, a bad id.
    let before = calls().len();
    let bad_values =
        fv(json!({ "mode": "range", "start_date": yesterday, "end_date": "2999-01-01" }));
    assert_eq!(err(run_with(&d, NAME, &bad_values).await), NOT_ENDED);
    assert!(err(run_with(&d, OTHER, &v).await).contains("no es una consulta programada"));
    assert!(calls()[before..].iter().all(|c| c.0 == "GET"));
    let before = calls().len();
    for bad in [
        "projects/1/locations/us/transferConfigs/a/../../../x",
        "projects/1/locations/us/transferConfigs/a:startManualRuns",
        "x'; DROP",
    ] {
        assert!(run_with(&d, bad, &v).await.is_err(), "{bad}");
        assert!(plan_with(&d, bad, &v).await.is_err(), "{bad}");
    }
    assert_eq!(calls().len(), before, "a bad id never reaches the service");
}

#[tokio::test]
async fn the_form_reads_the_config() {
    let seen: Seen = Default::default();
    let base = service(seen.clone()).await;
    let api = api_at(&base);
    let d = Dts {
        api: &api,
        base: &base,
    };
    let c = read(&d, NAME).await.unwrap();
    let f = form_for(&c, Utc::now());
    assert_eq!(f.values["mode"], json!("range"));
    assert!(read(&d, OTHER).await.is_err());
    assert!(seen.lock().unwrap().iter().all(|c| c.0 == "GET"));
}
