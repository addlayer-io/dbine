//! Unit tests of the scheduled queries' designer: values, forms, the
//! requests and scripts, and the calls against a stand-in service.

use super::*;
use crate::gcp;
use dbine_driver::ConnectionConfig;
use std::sync::{Arc, Mutex};

const NAME: &str =
    "projects/123456/locations/us/transferConfigs/6789abcd-0000-2a6d-91b4-089e0820c3c4";

fn config() -> Json {
    json!({
        "name": NAME,
        "displayName": "ventas diarias",
        "dataSourceId": "scheduled_query",
        "destinationDatasetId": "ventas",
        "datasetRegion": "us",
        "params": {
            "query": "SELECT * FROM ventas.crudo WHERE dia = @run_date",
            "destination_table_name_template": "resumen_{run_date}",
            "write_disposition": "WRITE_TRUNCATE",
            "partitioning_field": "",
            "otro": "se conserva"
        },
        "schedule": "every 24 hours",
        "scheduleOptions": { "startTime": "2026-01-01T03:00:00.123Z" },
        "updateTime": "2026-11-10T12:00:00.5-03:00",
        "nextRunTime": "2026-11-13T03:00:00Z",
        "state": "FAILED",
        "error": { "code": 3, "message": "Syntax error" },
        "ownerInfo": { "email": "ana@example.com" },
        "emailPreferences": { "enableFailureEmail": true },
        "notificationPubsubTopic": "projects/p/topics/avisos",
        "encryptionConfiguration": { "kmsKeyName": "projects/p/locations/us/keyRings/r/cryptoKeys/k" },
        "disabled": false
    })
}

fn cur() -> Current {
    current_of(&config()).unwrap()
}

fn map(v: Json) -> BTreeMap<String, Json> {
    serde_json::from_value(v).unwrap()
}

fn fv(v: Json) -> FormValues {
    FormValues {
        values: map(v),
        lists: Default::default(),
    }
}

/// The values the designer shows for `c`, as it would send them back.
fn values_of(c: &Current) -> BTreeMap<String, Json> {
    form_of(Some(c), &[], None).values
}

fn with(c: &Current, changes: Json) -> BTreeMap<String, Json> {
    let mut v = values_of(c);
    for (k, x) in map(changes) {
        v.insert(k, x);
    }
    v
}

fn new_values() -> Json {
    json!({
        "display_name": "resumen \"diario\"",
        "location": "us",
        "enabled": true,
        "query": "SELECT 1 AS x",
        "dataset": "ventas",
        "table": "resumen_{run_date}",
        "write_disposition": "WRITE_TRUNCATE",
        "partitioning_field": "dia",
        "kms_key": "",
        "mode": "schedule",
        "schedule": "every  24   hours",
        "start_time": "2026-12-01T03:00:00",
        "end_time": null,
        "email_on_failure": true,
        "pubsub_topic": "projects/p/topics/avisos",
        "service_account": "sa@p.iam.gserviceaccount.com"
    })
}

/// An `Api` that sends no token (as against an emulator), at `base`.
fn api_at(base: &str) -> Api {
    let http = reqwest::Client::new();
    let cfg = ConnectionConfig {
        options: [("endpoint_url".to_string(), "http://localhost:1".to_string())].into(),
        ..Default::default()
    };
    Api {
        tokens: gcp::Tokens::from_config(&cfg, http.clone()).unwrap(),
        http,
        base: base.into(),
        project: "mi-proyecto".into(),
        location: Some("US".into()),
        emulator: false,
    }
}

/// The JSON body a commented request carries in a script.
fn body_in(script: &str) -> Json {
    let lines: Vec<&str> = script
        .lines()
        .skip_while(|l| !l.starts_with("# {"))
        .map(|l| l.strip_prefix("# ").unwrap_or(""))
        .collect();
    serde_json::from_str(&lines.join("\n")).unwrap()
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

#[test]
fn kinds_and_values_are_checked() {
    assert!(kind("job").is_ok());
    assert!(kind("schedule").is_err());
    assert!(location_ok("us") && location_ok("southamerica-east1"));
    assert!(!location_ok("US") && !location_ok("us/../x") && !location_ok("u"));
    assert!(dataset_ok("ventas_2026") && !dataset_ok("ventas.x") && !dataset_ok(""));
    assert!(column_ok("dia") && column_ok("_ts") && !column_ok("1x") && !column_ok("a b"));
    assert!(topic_ok("projects/mi-proyecto/topics/avisos"));
    for bad in [
        "avisos",
        "projects/p/topics/ab",
        "projects/p/topics/goog-x",
        "projects/p/subscriptions/avisos",
        "projects/p/topics/a b c",
    ] {
        assert!(!topic_ok(bad), "{bad}");
    }
    assert!(kms_ok("projects/p/locations/us/keyRings/r/cryptoKeys/k"));
    assert!(!kms_ok("projects/p/locations/us/keyRings/r"));
    assert!(!kms_ok("projects/p/locations/us/keyRings/r/cryptoKeys/k/x"));
    assert!(email_ok("sa@p.iam.gserviceaccount.com"));
    for bad in ["sa", "sa@", "sa@x", "a b@x.com", "a@x.com?x=1", "a@@x.com"] {
        assert!(!email_ok(bad), "{bad}");
    }
}

#[test]
fn schedules_are_the_services_syntax() {
    for ok in [
        "every 24 hours",
        "every 15 minutes",
        "every 5 minutes",
        "every mon,wed 09:00",
        "every day 03:00",
        "1st,3rd monday of month 15:30",
        "first sunday of quarter 00:00",
    ] {
        assert_eq!(schedule_check(ok).unwrap(), ok);
    }
    assert_eq!(
        schedule_check("  every   24\thours ").unwrap(),
        "every 24 hours"
    );
    for bad in ["", "cada día", "0 2 * * *", "every 24 hours; DROP"] {
        assert!(schedule_check(bad).is_err(), "{bad}");
    }
    for often in ["every 1 minutes", "every 4 mins", "every 0 hours"] {
        let e = schedule_check(often).unwrap_err().to_string();
        assert!(e.contains("cada 5 minutos"), "{often}: {e}");
    }
}

#[test]
fn date_times_are_utc() {
    let v = map(
        json!({ "a": "2026-11-13T03:00:00", "b": "2026-11-13T03:00:00-03:00", "c": "mañana", "d": "" }),
    );
    let v = Vals(&v);
    assert_eq!(
        time_value(v, "a", "A").unwrap().as_deref(),
        Some("2026-11-13T03:00:00Z")
    );
    assert_eq!(
        time_value(v, "b", "B").unwrap().as_deref(),
        Some("2026-11-13T06:00:00Z")
    );
    assert!(time_value(v, "c", "C").is_err());
    assert_eq!(time_value(v, "d", "D").unwrap(), None);
}

// ---------------------------------------------------------------------------
// Forms
// ---------------------------------------------------------------------------

#[test]
fn a_new_form_has_every_part() {
    let datasets = vec![
        ("ventas".to_string(), "us".to_string()),
        ("europa".to_string(), "eu".to_string()),
    ];
    let f = form_of(None, &datasets, Some("US"));
    let keys: Vec<&str> = f.sections.iter().map(|s| s.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "general",
            "destination",
            "schedule",
            "notifications",
            "credentials"
        ]
    );
    assert_eq!(f.values["location"], json!("us"));
    assert_eq!(f.values["enabled"], json!(true));
    assert_eq!(f.values["write_disposition"], json!("WRITE_APPEND"));
    assert_eq!(f.values["mode"], json!("schedule"));
    assert_eq!(f.values["schedule"], json!("every 24 hours"));
    assert_eq!(f.values["dataset"], Json::Null);
    assert_eq!(
        f.choices["dataset"],
        [
            ("ventas".to_string(), "ventas (us)".to_string()),
            ("europa".to_string(), "europa (eu)".to_string())
        ]
    );
    let locations: Vec<&str> = f.choices["location"]
        .iter()
        .map(|(v, _)| v.as_str())
        .collect();
    assert_eq!(locations, ["eu", "us"]);
    let field = |k: &str| {
        f.sections
            .iter()
            .flat_map(|s| s.fields.iter())
            .find(|x| x.key == k)
            .unwrap()
    };
    assert!(!field("location").read_only);
    assert_eq!(
        field("schedule").when,
        Some(("mode".into(), vec!["schedule".into()]))
    );
    assert!(matches!(&field("mode").kind, FormKind::Select { options } if options.len() == 2));
    assert!(matches!(&field("query").kind, FormKind::Code { language } if language == "sql"));
    assert!(!f.read_only && f.notes.is_empty());
    // Without datasets nor a location, the common ones.
    let bare = form_of(None, &[], None);
    assert_eq!(bare.values["location"], Json::Null);
    assert_eq!(bare.choices["location"].len(), 2);
}

#[test]
fn an_existing_config_reads_back() {
    let c = cur();
    assert!(!c.v2);
    let f = form_of(Some(&c), &[], None);
    let v = &f.values;
    assert_eq!(v["display_name"], json!("ventas diarias"));
    assert_eq!(v["location"], json!("us"));
    assert_eq!(
        v["query"],
        json!("SELECT * FROM ventas.crudo WHERE dia = @run_date")
    );
    assert_eq!(v["dataset"], json!("ventas"));
    assert_eq!(v["table"], json!("resumen_{run_date}"));
    assert_eq!(v["write_disposition"], json!("WRITE_TRUNCATE"));
    assert_eq!(v["partitioning_field"], json!(""));
    assert_eq!(
        v["kms_key"],
        json!("projects/p/locations/us/keyRings/r/cryptoKeys/k")
    );
    assert_eq!(v["start_time"], json!("2026-01-01T03:00:00Z"));
    assert_eq!(v["end_time"], Json::Null);
    assert_eq!(v["email_on_failure"], json!(true));
    assert_eq!(v["pubsub_topic"], json!("projects/p/topics/avisos"));
    // A user owns it: the service account field stays empty (no change).
    assert_eq!(v["service_account"], json!(""));
    assert_eq!(v["owner"], json!("ana@example.com"));
    assert_eq!(v["state"], json!("FAILED"));
    assert_eq!(v["updated"], json!("2026-11-10T15:00:00Z"));
    assert_eq!(v["error"], json!("Syntax error"));
    assert_eq!(v["config_id"], json!(NAME));
    let status = f.sections.iter().find(|s| s.key == "status").unwrap();
    assert!(status.fields.iter().all(|x| x.read_only));
    let location = f.sections[0]
        .fields
        .iter()
        .find(|x| x.key == "location")
        .unwrap();
    assert!(location.read_only);
    assert!(f.notes.iter().any(|n| n == IN_PLACE));
    assert!(f.choices["dataset"].iter().any(|(d, _)| d == "ventas"));

    // A service account owner shows in its field.
    let mut sa = config();
    sa["ownerInfo"] = json!({ "email": "sa@p.iam.gserviceaccount.com" });
    let f = form_of(current_of(&sa).as_ref(), &[], None);
    assert_eq!(
        f.values["service_account"],
        json!("sa@p.iam.gserviceaccount.com")
    );

    // On demand, through scheduleOptionsV2; and on Pub/Sub messages.
    let mut manual = config();
    manual.as_object_mut().unwrap().remove("schedule");
    manual.as_object_mut().unwrap().remove("scheduleOptions");
    manual["scheduleOptionsV2"] = json!({ "manualSchedule": {} });
    let m = current_of(&manual).unwrap();
    assert!(m.v2);
    assert_eq!(m.spec.mode, Mode::Manual);
    let mut event = manual.clone();
    event["scheduleOptionsV2"] =
        json!({ "eventDrivenSchedule": { "pubsubSubscription": "projects/p/subscriptions/s" } });
    let f = form_of(current_of(&event).as_ref(), &[], None);
    assert_eq!(f.values["mode"], json!("event"));
    assert_eq!(
        f.values["subscription"],
        json!("projects/p/subscriptions/s")
    );
    assert!(f.notes.iter().any(|n| n == EVENT_NOTE));
    let mode = f.sections[2]
        .fields
        .iter()
        .find(|x| x.key == "mode")
        .unwrap();
    assert!(matches!(&mode.kind, FormKind::Select { options } if options.len() == 3));
}

// ---------------------------------------------------------------------------
// Creating
// ---------------------------------------------------------------------------

#[test]
fn a_new_scheduled_query_is_one_post() {
    let api = api_at("http://localhost:1");
    let dts = Dts {
        api: &api,
        base: crate::jobs::DTS,
    };
    let v = map(new_values());
    let w = wanted(Vals(&v), None, None).unwrap();
    assert_eq!(w.schedule.as_deref(), Some("every 24 hours"));
    assert_eq!(w.start.as_deref(), Some("2026-12-01T03:00:00Z"));
    let p = create_plan(&dts, "mi-proyecto", &w).unwrap();
    assert!(p.id.is_none());
    let sc = p.script();
    assert!(sc.lines().all(|l| l.starts_with('#')), "{sc}");
    assert!(sc.starts_with("# Crea la consulta programada «resumen \"diario\"» de BigQuery en us"));
    assert!(sc.contains("\n# POST https://bigquerydatatransfer.googleapis.com/v1/projects/mi-proyecto/locations/us/transferConfigs?serviceAccountName=sa%40p.iam.gserviceaccount.com\n"), "{sc}");
    assert_eq!(
        body_in(&sc),
        json!({
            "displayName": "resumen \"diario\"",
            "dataSourceId": "scheduled_query",
            "destinationDatasetId": "ventas",
            "params": {
                "query": "SELECT 1 AS x",
                "destination_table_name_template": "resumen_{run_date}",
                "write_disposition": "WRITE_TRUNCATE",
                "partitioning_field": "dia"
            },
            "schedule": "every 24 hours",
            "scheduleOptions": { "startTime": "2026-12-01T03:00:00Z" },
            "disabled": false,
            "emailPreferences": { "enableFailureEmail": true },
            "notificationPubsubTopic": "projects/p/topics/avisos"
        })
    );

    // DDL / DML: no destination, only the query; on demand; disabled; the
    // connection's location; the connection's own credentials.
    let v = map(json!({
        "display_name": "limpieza", "query": "DELETE FROM t WHERE true",
        "mode": "manual", "enabled": false, "kms_key": "projects/p/locations/us/keyRings/r/cryptoKeys/k"
    }));
    let w = wanted(Vals(&v), None, Some("US")).unwrap();
    assert_eq!(w.location, "us");
    let sc = create_plan(&dts, "mi-proyecto", &w).unwrap().script();
    assert!(sc.contains("Corre con las credenciales de la cuenta de esta conexión."));
    assert!(sc.contains("/locations/us/transferConfigs\n"), "{sc}");
    assert_eq!(
        body_in(&sc),
        json!({
            "displayName": "limpieza",
            "dataSourceId": "scheduled_query",
            "params": { "query": "DELETE FROM t WHERE true" },
            "scheduleOptions": { "disableAutoScheduling": true },
            "disabled": true,
            "emailPreferences": { "enableFailureEmail": false },
            "encryptionConfiguration": { "kmsKeyName": "projects/p/locations/us/keyRings/r/cryptoKeys/k" }
        })
    );
}

#[test]
fn bad_values_are_refused_before_any_request() {
    let base = new_values();
    let refused = |changes: Json, expect: &str| {
        let mut v = map(base.clone());
        for (k, x) in map(changes) {
            v.insert(k, x);
        }
        let e = wanted(Vals(&v), None, None).unwrap_err().to_string();
        assert!(e.contains(expect), "{e}");
    };
    refused(json!({ "display_name": " " }), "necesita un nombre");
    refused(json!({ "display_name": "a\nb" }), "una sola línea");
    refused(json!({ "query": "  " }), "necesita una consulta");
    refused(json!({ "location": "EU/../x" }), "no es una ubicación");
    refused(json!({ "dataset": "ventas.x" }), "no es un id de dataset");
    refused(json!({ "table": "" }), "dataset y la tabla de destino");
    refused(
        json!({ "dataset": null, "table": "t" }),
        "dataset y la tabla de destino",
    );
    refused(
        json!({ "dataset": null, "table": null }),
        "sin destino no se usa",
    );
    refused(
        json!({ "write_disposition": "WRITE_EMPTY" }),
        "preferencia de escritura",
    );
    refused(
        json!({ "partitioning_field": "dia; DROP" }),
        "nombre de columna",
    );
    refused(json!({ "kms_key": "mi-clave" }), "clave de Cloud KMS");
    refused(json!({ "mode": "event" }), "no al llegar mensajes");
    refused(json!({ "mode": "hourly" }), "modo de ejecución");
    refused(json!({ "schedule": "" }), "Escribí la programación");
    refused(json!({ "schedule": "every 2 minutes" }), "cada 5 minutos");
    refused(
        json!({ "end_time": "2026-11-01T00:00:00Z" }),
        "terminar después de empezar",
    );
    refused(json!({ "start_time": "ayer" }), "fecha y hora válida");
    refused(json!({ "pubsub_topic": "avisos" }), "tema de Pub/Sub");
    refused(json!({ "service_account": "sa" }), "cuenta de servicio");
    let mut long = map(base.clone());
    long.insert("query".into(), json!("x".repeat(MAX_QUERY + 1)));
    assert!(wanted(Vals(&long), None, None).is_err());
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

fn masked(c: &Current, changes: Json) -> (Vec<&'static str>, Map<String, Json>, Option<String>) {
    let v = with(c, changes);
    let w = wanted(Vals(&v), Some(c), None).unwrap();
    diff(c, &w)
}

#[test]
fn an_edit_sends_only_what_changed() {
    let c = cur();
    let (mask, body, account) = masked(&c, json!({}));
    assert!(
        mask.is_empty() && body.is_empty() && account.is_none(),
        "{mask:?}"
    );

    let (mask, body, _) = masked(&c, json!({ "display_name": "ventas por día" }));
    assert_eq!(mask, ["display_name"]);
    assert_eq!(
        Json::Object(body),
        json!({ "displayName": "ventas por día" })
    );

    // params goes whole, with the keys DBine doesn't know kept.
    let (mask, body, _) = masked(&c, json!({ "query": "SELECT 2" }));
    assert_eq!(mask, ["params"]);
    assert_eq!(
        body["params"],
        json!({ "query": "SELECT 2", "destination_table_name_template": "resumen_{run_date}",
                "write_disposition": "WRITE_TRUNCATE", "partitioning_field": "", "otro": "se conserva" })
    );

    // Without a destination (a DML query now).
    let (mask, body, _) = masked(&c, json!({ "dataset": null, "table": "" }));
    assert_eq!(mask, ["destination_dataset_id", "params"]);
    assert_eq!(body["destinationDatasetId"], json!(""));
    assert_eq!(
        body["params"],
        json!({ "query": "SELECT * FROM ventas.crudo WHERE dia = @run_date", "partitioning_field": "", "otro": "se conserva" })
    );

    let (mask, body, _) = masked(&c, json!({ "schedule": "every 12 hours" }));
    assert_eq!(mask, ["schedule"]);
    assert_eq!(body["schedule"], json!("every 12 hours"));

    let (mask, body, _) = masked(&c, json!({ "end_time": "2027-01-01T00:00:00" }));
    assert_eq!(mask, ["schedule_options"]);
    assert_eq!(
        body["scheduleOptions"],
        json!({ "startTime": "2026-01-01T03:00:00Z", "endTime": "2027-01-01T00:00:00Z" })
    );

    let (mask, body, _) = masked(&c, json!({ "mode": "manual" }));
    assert_eq!(mask, ["schedule_options"]);
    assert_eq!(
        body["scheduleOptions"],
        json!({ "disableAutoScheduling": true, "startTime": "2026-01-01T03:00:00Z" })
    );

    let (mask, body, _) = masked(&c, json!({ "enabled": false }));
    assert_eq!(mask, ["disabled"]);
    assert_eq!(body["disabled"], json!(true));

    let (mask, body, _) = masked(&c, json!({ "email_on_failure": false }));
    assert_eq!(mask, ["email_preferences"]);
    assert_eq!(
        body["emailPreferences"],
        json!({ "enableFailureEmail": false })
    );

    let (mask, body, _) = masked(&c, json!({ "pubsub_topic": "" }));
    assert_eq!(mask, ["notification_pubsub_topic"]);
    assert_eq!(body["notificationPubsubTopic"], json!(""));

    let (mask, body, _) = masked(&c, json!({ "kms_key": "" }));
    assert_eq!(mask, ["encryption_configuration"]);
    assert_eq!(body["encryptionConfiguration"], json!({}));

    let (mask, body, account) = masked(
        &c,
        json!({ "service_account": "sa@p.iam.gserviceaccount.com" }),
    );
    assert_eq!(mask, ["service_account_name"]);
    assert!(body.is_empty());
    assert_eq!(account.as_deref(), Some("sa@p.iam.gserviceaccount.com"));

    // What can't change (the location) is the config's whatever comes.
    let (mask, _, _) = masked(&c, json!({ "location": "eu" }));
    assert!(mask.is_empty());
}

#[test]
fn configs_on_schedule_options_v2_are_edited_there() {
    let mut j = config();
    j.as_object_mut().unwrap().remove("schedule");
    j.as_object_mut().unwrap().remove("scheduleOptions");
    j["scheduleOptionsV2"] = json!({ "timeBasedSchedule": { "schedule": "every 6 hours", "startTime": "2026-01-01T00:00:00Z" } });
    let c = current_of(&j).unwrap();
    assert!(c.v2);
    assert_eq!(c.spec.schedule.as_deref(), Some("every 6 hours"));
    let (mask, body, _) = masked(&c, json!({ "schedule": "every 8 hours" }));
    assert_eq!(mask, ["schedule_options_v2"]);
    assert_eq!(
        body["scheduleOptionsV2"],
        json!({ "timeBasedSchedule": { "schedule": "every 8 hours", "startTime": "2026-01-01T00:00:00Z" } })
    );
    let (mask, body, _) = masked(&c, json!({ "mode": "manual" }));
    assert_eq!(mask, ["schedule_options_v2"]);
    assert_eq!(body["scheduleOptionsV2"], json!({ "manualSchedule": {} }));

    // A Pub/Sub one keeps its trigger unless it moves to a schedule.
    j["scheduleOptionsV2"] =
        json!({ "eventDrivenSchedule": { "pubsubSubscription": "projects/p/subscriptions/s" } });
    let e = current_of(&j).unwrap();
    let (mask, _, _) = masked(&e, json!({ "display_name": "x" }));
    assert_eq!(mask, ["display_name"]);
    let (mask, body, _) = masked(
        &e,
        json!({ "mode": "schedule", "schedule": "every 24 hours" }),
    );
    assert_eq!(mask, ["schedule_options_v2"]);
    assert_eq!(
        body["scheduleOptionsV2"],
        json!({ "timeBasedSchedule": { "schedule": "every 24 hours" } })
    );
}

#[test]
fn edit_and_delete_scripts() {
    let api = api_at("http://localhost:1");
    let dts = Dts {
        api: &api,
        base: crate::jobs::DTS,
    };
    let c = cur();
    let v = with(
        &c,
        json!({ "display_name": "x", "enabled": false, "service_account": "sa@p.iam.gserviceaccount.com" }),
    );
    let w = wanted(Vals(&v), Some(&c), None).unwrap();
    let p = patch_plan(&dts, &c, &w).unwrap();
    assert_eq!(p.id.as_deref(), Some(NAME));
    let sc = p.script();
    assert!(sc.lines().all(|l| l.starts_with('#')), "{sc}");
    assert!(sc.contains(&format!("\n# PATCH https://bigquerydatatransfer.googleapis.com/v1/{NAME}?updateMask=display_name,disabled,service_account_name&serviceAccountName=sa%40p.iam.gserviceaccount.com\n")), "{sc}");
    assert!(sc.contains(IN_PLACE));
    assert_eq!(
        body_in(&sc),
        json!({ "displayName": "x", "disabled": true })
    );

    let same = wanted(Vals(&values_of(&c)), Some(&c), None).unwrap();
    let p = patch_plan(&dts, &c, &same).unwrap();
    assert!(p.reqs.is_empty());
    assert_eq!(
        p.script(),
        "# «ventas diarias»: no hay cambios que guardar.\n"
    );

    let sc = delete_plan(&dts, &c).unwrap().script();
    assert_eq!(
        sc,
        format!("# Borra la consulta programada «ventas diarias» de BigQuery, con su historial de ejecuciones y sus registros.\n#\n# DELETE https://bigquerydatatransfer.googleapis.com/v1/{NAME}\n")
    );
}

#[test]
fn items_say_where_and_when() {
    let j = ServerJob {
        id: NAME.into(),
        name: "ventas diarias".into(),
        group: Some("us".into()),
        schedule: Some("every 24 hours".into()),
        enabled: false,
        ..Default::default()
    };
    let i = item_of(j.clone());
    assert_eq!(i.id, NAME);
    assert_eq!(i.description.as_deref(), Some("En us: every 24 hours"));
    assert_eq!(i.enabled, Some(false));
    let i = item_of(ServerJob {
        schedule: None,
        ..j
    });
    assert_eq!(i.description.as_deref(), Some("En us: solo a pedido"));
}

// ---------------------------------------------------------------------------
// Against a stand-in service
// ---------------------------------------------------------------------------

/// A tiny HTTP server: one request per connection, answered by `h`
/// (method, path with query, body) → (status, body).
type Handler = Arc<dyn Fn(&str, &str, &str) -> (u16, String) + Send + Sync>;

async fn serve(h: Handler) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = l.accept().await {
            let h = h.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = vec![0u8; 1 << 16];
                let (head_end, len) = loop {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                    if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..p]).to_ascii_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| {
                                l.strip_prefix("content-length:")
                                    .map(|v| v.trim().parse().unwrap())
                            })
                            .unwrap_or(0);
                        break (p + 4, len);
                    }
                };
                while buf.len() < head_end + len {
                    let n = sock.read(&mut tmp).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let mut first = head.lines().next().unwrap_or("").split(' ');
                let (method, path) = (
                    first.next().unwrap_or("").to_string(),
                    first.next().unwrap_or("").to_string(),
                );
                let body = String::from_utf8_lossy(&buf[head_end..head_end + len]).to_string();
                let (status, text) = h(&method, &path, &body);
                let resp = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    text.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.write_all(text.as_bytes()).await;
            });
        }
    });
    format!("http://{addr}")
}

type Seen = Arc<Mutex<Vec<(String, String, String)>>>;

const OTHER: &str = "projects/123456/locations/us/transferConfigs/gcs-copy";

/// The service and BigQuery's API in one: the config above, a Cloud
/// Storage transfer, datasets `ventas` (US) and `europa` (EU), runs.
async fn service(seen: Seen) -> String {
    serve(Arc::new(move |m, p, b| {
        seen.lock()
            .unwrap()
            .push((m.to_string(), p.to_string(), b.to_string()));
        let (path, query) = p.split_once('?').unwrap_or((p, ""));
        let cfg = format!("/v1/{NAME}");
        let runs = format!("{cfg}/runs");
        let body = match (m, path) {
            ("GET", x) if x == cfg => config(),
            ("GET", x) if x == format!("/v1/{OTHER}") => {
                json!({ "name": OTHER, "dataSourceId": "google_cloud_storage", "displayName": "copia" })
            }
            ("GET", "/bigquery/v2/projects/mi-proyecto/datasets/ventas") => {
                json!({ "location": "US" })
            }
            ("GET", "/bigquery/v2/projects/mi-proyecto/datasets/europa") => {
                json!({ "location": "EU" })
            }
            ("POST", "/v1/projects/mi-proyecto/locations/us/transferConfigs") => {
                let mut c = config();
                c["name"] = json!("projects/123456/locations/us/transferConfigs/nueva");
                c
            }
            ("PATCH", x) if x == cfg => config(),
            ("DELETE", x) if x == cfg => json!({}),
            ("GET", x) if x == runs && query.contains("pageToken=t2") => json!({ "transferRuns": [
                { "name": format!("{NAME}/runs/r3"), "state": "FAILED" },
                { "name": format!("{NAME}/runs/../../x"), "state": "FAILED" }
            ] }),
            ("GET", x) if x == runs => json!({ "transferRuns": [
                { "name": format!("{NAME}/runs/r1"), "state": "SUCCEEDED" },
                { "name": format!("{NAME}/runs/r2"), "state": "RUNNING" }
            ], "nextPageToken": "t2" }),
            ("DELETE", x) if x.starts_with(&format!("{runs}/")) => json!({}),
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

#[tokio::test]
async fn the_designer_calls_the_service_as_documented() {
    let seen: Seen = Default::default();
    let base = service(seen.clone()).await;
    let api = api_at(&base);
    let dts = Dts {
        api: &api,
        base: &base,
    };
    let calls = || seen.lock().unwrap().clone();

    // Create: the dataset is looked up, its location is the config's.
    let mut v = map(new_values());
    v.remove("location");
    let id = save_with(
        &api,
        &dts,
        None,
        &FormValues {
            values: v.clone(),
            lists: Default::default(),
        },
    )
    .await
    .unwrap();
    assert_eq!(
        id.as_deref(),
        Some("projects/123456/locations/us/transferConfigs/nueva")
    );
    let post = calls().into_iter().find(|c| c.0 == "POST").unwrap();
    assert_eq!(
        post.1,
        "/v1/projects/mi-proyecto/locations/us/transferConfigs?serviceAccountName=sa%40p.iam.gserviceaccount.com"
    );
    let sent: Json = serde_json::from_str(&post.2).unwrap();
    assert_eq!(sent["dataSourceId"], json!("scheduled_query"));
    assert_eq!(sent["destinationDatasetId"], json!("ventas"));

    // A dataset elsewhere, or none: refused before the service.
    let before = calls().len();
    v.insert("location".into(), json!("us"));
    v.insert("dataset".into(), json!("europa"));
    let e = plan(
        &api,
        &dts,
        None,
        &FormValues {
            values: v.clone(),
            lists: Default::default(),
        },
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("misma ubicación"), "{e}");
    v.insert("dataset".into(), json!("no_existe"));
    let e = plan(
        &api,
        &dts,
        None,
        &FormValues {
            values: v,
            lists: Default::default(),
        },
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(e.contains("no existe en el proyecto"), "{e}");
    assert!(calls()[before..].iter().all(|c| c.0 == "GET"));

    // Edit: a PATCH with only the name; nothing when nothing changed.
    let c = cur();
    let id = save_with(
        &api,
        &dts,
        Some(NAME),
        &fv(Json::Object(
            with(&c, json!({ "display_name": "otra" }))
                .into_iter()
                .collect(),
        )),
    )
    .await
    .unwrap();
    assert_eq!(id.as_deref(), Some(NAME));
    let patch = calls().into_iter().find(|c| c.0 == "PATCH").unwrap();
    assert!(patch.1.ends_with("?updateMask=display_name"), "{}", patch.1);
    assert_eq!(
        serde_json::from_str::<Json>(&patch.2).unwrap(),
        json!({ "displayName": "otra" })
    );
    let before = calls().len();
    save_with(
        &api,
        &dts,
        Some(NAME),
        &fv(Json::Object(values_of(&c).into_iter().collect())),
    )
    .await
    .unwrap();
    assert!(calls()[before..].iter().all(|c| c.0 == "GET"));

    // Delete.
    delete_with(&dts, NAME).await.unwrap();
    assert!(calls()
        .iter()
        .any(|c| c.0 == "DELETE" && c.1 == format!("/v1/{NAME}")));

    // Clear the history: the finished runs of every page, not the one
    // going on nor a run name that isn't one.
    assert_eq!(clear_with(&dts, NAME).await.unwrap(), 2);
    let deleted: Vec<String> = calls()
        .into_iter()
        .filter(|c| c.0 == "DELETE" && c.1.contains("/runs/"))
        .map(|c| c.1)
        .collect();
    assert_eq!(
        deleted,
        [format!("/v1/{NAME}/runs/r1"), format!("/v1/{NAME}/runs/r3")]
    );

    // Another data source's transfer isn't a scheduled query.
    let before = calls().len();
    assert!(read(&dts, OTHER)
        .await
        .unwrap_err()
        .to_string()
        .contains("no es una consulta programada"));
    assert!(delete_with(&dts, OTHER).await.is_err());
    assert!(clear_with(&dts, OTHER).await.is_err());
    assert!(save_with(&api, &dts, Some(OTHER), &fv(new_values()))
        .await
        .is_err());
    assert!(calls()[before..].iter().all(|c| c.0 == "GET"));

    // A bad id never reaches the service.
    let before = calls().len();
    for bad in [
        "projects/1/locations/us/transferConfigs/a/../../../x",
        "projects/1/locations/us/transferConfigs/a?updateMask=name",
        "x'; DROP",
    ] {
        assert!(read(&dts, bad).await.is_err());
        assert!(delete_with(&dts, bad).await.is_err());
        assert!(clear_with(&dts, bad).await.is_err());
        assert!(save_with(&api, &dts, Some(bad), &fv(new_values()))
            .await
            .is_err());
    }
    assert_eq!(calls().len(), before, "a bad id never reaches the service");
}

#[tokio::test]
async fn info_says_what_the_service_and_the_account_allow() {
    // The service on; IAM grants the change, or doesn't, or can't say.
    let grants: Arc<Mutex<Option<bool>>> = Arc::new(Mutex::new(Some(true)));
    let g = grants.clone();
    let base = serve(Arc::new(move |m, p, _| {
        let (path, _) = p.split_once('?').unwrap_or((p, ""));
        match (m, path) {
            ("GET", "/v1/projects/mi-proyecto/locations/us/transferConfigs") => (200, "{}".into()),
            ("POST", "/crm/mi-proyecto:testIamPermissions") => match *g.lock().unwrap() {
                Some(true) => (
                    200,
                    json!({ "permissions": [TRANSFERS_UPDATE] }).to_string(),
                ),
                Some(false) => (200, "{}".into()),
                None => (
                    403,
                    json!({ "error": { "code": 403, "message": "API off" } }).to_string(),
                ),
            },
            _ => (404, "{}".into()),
        }
    }))
    .await;
    let api = api_at(&base);
    let dts = Dts {
        api: &api,
        base: &base,
    };
    let crm = format!("{base}/crm");
    let i = info_with(&api, &dts, &crm).await.unwrap();
    let k = &i.kinds[0];
    assert_eq!(
        (k.key.as_str(), k.label.as_str()),
        ("job", "Consultas programadas")
    );
    assert!(k.can_create && k.can_edit && k.can_delete && !i.logs);
    // One query: no step to start at; its finished runs are deleted; "run
    // now" is the jobs list's manual run, so no action.
    assert!(!k.run_from_step && k.clear_history && k.actions.is_empty());
    assert!(i.notes.is_empty());
    *grants.lock().unwrap() = Some(false);
    let i = info_with(&api, &dts, &crm).await.unwrap();
    assert!(!i.kinds[0].can_create && !i.kinds[0].can_edit && !i.kinds[0].can_delete);
    assert!(!i.kinds[0].clear_history);
    assert_eq!(i.notes, [NO_PERMISSION]);
    *grants.lock().unwrap() = None;
    assert!(info_with(&api, &dts, &crm).await.unwrap().kinds[0].can_create);

    // The API off in the project: no kinds; no permission to read: a note.
    let off = serve(Arc::new(|_, _, _| {
        (403, json!({ "error": { "code": 403, "status": "PERMISSION_DENIED",
            "message": "BigQuery Data Transfer API has not been used in project 1 before or it is disabled.",
            "details": [{ "reason": "SERVICE_DISABLED" }] } }).to_string())
    }))
    .await;
    let dts = Dts {
        api: &api,
        base: &off,
    };
    let i = info_with(&api, &dts, &crm).await.unwrap();
    assert!(i.kinds.is_empty());
    assert_eq!(i.notes, [DISABLED]);
    let denied = serve(Arc::new(|_, _, _| {
        (
            403,
            json!({ "error": { "code": 403, "message": "Permission denied" } }).to_string(),
        )
    }))
    .await;
    let dts = Dts {
        api: &api,
        base: &denied,
    };
    let i = info_with(&api, &dts, &crm).await.unwrap();
    assert_eq!(i.notes[0], LIST_DENIED);
}
