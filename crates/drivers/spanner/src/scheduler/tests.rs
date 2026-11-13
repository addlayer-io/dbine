//! Unit tests of the backup schedules' designer: values, forms, the
//! requests and scripts, and the calls against a stand-in Admin API.

use super::*;
use crate::{gcp, Api};
use dbine_driver::ConnectionConfig;
use std::sync::{Arc, Mutex};

const INSTANCE: &str = "projects/p1/instances/i1";
const NAME: &str = "projects/p1/instances/i1/databases/ventas/backupSchedules/diario";
const KEY: &str = "projects/p1/locations/us/keyRings/r/cryptoKeys/k";

fn sched() -> Json {
    json!({"name": NAME,
        "spec": {"cronSpec": {"text": "0 2 * * *", "timeZone": "UTC", "creationWindow": "14400s"}},
        "retentionDuration": "604800s",
        "encryptionConfig": {"encryptionType": "CUSTOMER_MANAGED_ENCRYPTION", "kmsKeyName": KEY},
        "fullBackupSpec": {}, "updateTime": "2026-11-10T12:00:00.123456Z"})
}

fn cur() -> Current {
    current_of(&sched()).unwrap()
}

fn map(v: Json) -> BTreeMap<String, Json> {
    serde_json::from_value(v).unwrap()
}

fn fv(v: BTreeMap<String, Json>) -> FormValues {
    FormValues {
        values: v,
        lists: Default::default(),
    }
}

fn values_of(c: &Current) -> BTreeMap<String, Json> {
    form_of(Some(c), &[], "ventas").values
}

fn with(c: &Current, changes: Json) -> BTreeMap<String, Json> {
    let mut v = values_of(c);
    v.extend(map(changes));
    v
}

fn new_values() -> BTreeMap<String, Json> {
    map(json!({
        "database": "ventas",
        "schedule_id": "semanal",
        "backup_type": "full",
        "cron": " 0  3 * * 0 ",
        "retention": 2_592_000,
        "encryption": "USE_DATABASE_ENCRYPTION",
        "kms_keys": ""
    }))
}

/// The JSON body of the script's request at `index`.
fn body_at(script: &str, index: usize) -> Json {
    let chunk = script.split("#\n# ").nth(index + 1).unwrap();
    let lines: Vec<&str> = chunk
        .lines()
        .skip(1)
        .map(|l| l.strip_prefix("# ").unwrap_or(""))
        .collect();
    serde_json::from_str(&lines.join("\n")).unwrap()
}

// ---------------------------------------------------------------------------
// Values
// ---------------------------------------------------------------------------

#[test]
fn kinds_ids_and_keys_are_checked() {
    assert!(kind("job").is_ok() && kind("backup").is_err());
    assert!(new_id_ok("ab") && new_id_ok("diario_2026-a"));
    for bad in ["a", "Ab", "a_", "1ab", "a b", "a/b", &"a".repeat(61)] {
        assert!(!new_id_ok(bad), "{bad}");
    }
    assert!(kms_ok(KEY));
    assert!(!kms_ok("projects/p/locations/us/keyRings/r"));
    assert!(!kms_ok("projects/p/locations/us/keyRings/r/cryptoKeys/k?x"));
    assert_eq!(secs_of("604800s"), Some(604_800));
    assert_eq!(secs_of("3.5s"), Some(3));
    assert_eq!(secs_of("7d"), None);
}

#[test]
fn crons_are_checked_as_spanner_takes_them() {
    for (ok, incremental) in [
        ("0 2 * * *", false),
        ("0 2/12 * * *", false),
        ("0 2,14 * * *", false),
        ("0 2 * * 0", false),
        ("0 2 8 * *", false),
        ("30 23 * * 1-5", false),
        ("0 */4 * * *", true),
        ("15 1-21/4 * * *", true),
        ("0 2 * * 7", true),
    ] {
        assert_eq!(cron_check(ok, incremental).unwrap(), ok, "{ok}");
    }
    assert_eq!(cron_check("  0   2 * *\t* ", false).unwrap(), "0 2 * * *");
    for bad in [
        "0 2 * *",
        "0 2 * * * *",
        "0 24 * * *",
        "60 2 * * *",
        "0 2 0 * *",
        "0 2 * 13 *",
        "0 2 * * 8",
        "0 2 * * MON",
        "0 2/0 * * *",
        "0 5-2 * * *",
        "0 2 * * *;",
    ] {
        let e = cron_check(bad, true).unwrap_err().to_string();
        assert!(
            e.contains("cinco campos") || e.contains("no es válido"),
            "{bad}: {e}"
        );
    }
    let e = cron_check("0 */4 * * *", false).unwrap_err().to_string();
    assert!(e.contains("cada 12 horas"), "{e}");
    let e = cron_check("0 2,8 * * *", false).unwrap_err().to_string();
    assert!(e.contains("cada 12 horas"), "{e}");
    for often in ["*/30 2 * * *", "0 * * * *", "0 */2 * * *", "0 22,1 * * *"] {
        let e = cron_check(often, true).unwrap_err().to_string();
        assert!(e.contains("cada 4 horas"), "{often}: {e}");
    }
}

#[test]
fn bad_values_are_refused_before_any_request() {
    let refused = |changes: Json, expect: &str| {
        let mut v = new_values();
        v.extend(map(changes));
        let e = wanted(Vals(&v), None, "ventas").unwrap_err().to_string();
        assert!(e.contains(expect), "{e}");
    };
    refused(json!({ "schedule_id": "" }), "necesita un id");
    refused(
        json!({ "schedule_id": "Semanal" }),
        "no es un id de programa",
    );
    refused(json!({ "schedule_id": "../x" }), "no es un id de programa");
    refused(json!({ "database": "Ventas" }), "no es el id de una base");
    refused(json!({ "backup_type": "diff" }), "no es un tipo de backup");
    refused(json!({ "cron": "" }), "necesita un cron");
    refused(json!({ "cron": "@daily" }), "cinco campos");
    refused(json!({ "retention": 3600 }), "entre 6 horas y 366 días");
    refused(
        json!({ "retention": 400 * 86_400 }),
        "entre 6 horas y 366 días",
    );
    refused(json!({ "retention": "7d" }), "cantidad de segundos");
    refused(json!({ "retention": null }), "cuánto tiempo se guarda");
    refused(json!({ "encryption": "NONE" }), "tipo de cifrado");
    refused(
        json!({ "encryption": "CUSTOMER_MANAGED_ENCRYPTION", "kms_keys": " " }),
        "al menos una clave",
    );
    refused(
        json!({ "encryption": "CUSTOMER_MANAGED_ENCRYPTION", "kms_keys": "mi-clave" }),
        "clave de Cloud KMS",
    );
    // The database defaults to the connection's.
    let mut v = new_values();
    v.remove("database");
    assert_eq!(wanted(Vals(&v), None, "ventas").unwrap().db, "ventas");
}

// ---------------------------------------------------------------------------
// Forms
// ---------------------------------------------------------------------------

#[test]
fn a_new_form_has_every_part() {
    let f = form_of(
        None,
        &["ventas".into(), "Bad".into(), "otra".into()],
        "ventas",
    );
    let keys: Vec<&str> = f.sections.iter().map(|s| s.key.as_str()).collect();
    assert_eq!(keys, ["general", "schedule", "encryption"]);
    assert_eq!(f.values["database"], json!("ventas"));
    assert_eq!(f.values["schedule_id"], json!(""));
    assert_eq!(f.values["backup_type"], json!("full"));
    assert_eq!(f.values["cron"], json!("0 2 * * *"));
    assert_eq!(f.values["retention"], json!(604_800));
    assert_eq!(f.values["encryption"], json!("USE_DATABASE_ENCRYPTION"));
    let dbs: Vec<&str> = f.choices["database"]
        .iter()
        .map(|(d, _)| d.as_str())
        .collect();
    assert_eq!(dbs, ["ventas", "otra"]);
    let field = |k: &str| {
        f.sections
            .iter()
            .flat_map(|s| s.fields.iter())
            .find(|x| x.key == k)
            .unwrap()
    };
    assert!(!field("database").read_only && !field("schedule_id").read_only);
    assert_eq!(field("cron").kind, FormKind::Cron);
    assert_eq!(field("retention").kind, FormKind::Duration);
    assert_eq!(
        field("kms_keys").when,
        Some((
            "encryption".into(),
            vec!["CUSTOMER_MANAGED_ENCRYPTION".into()]
        ))
    );
    assert_eq!(f.notes, [NEW_DATABASE]);
}

#[test]
fn an_existing_schedule_reads_back() {
    let c = cur();
    let f = form_of(Some(&c), &["ventas".into()], "ventas");
    let v = &f.values;
    assert_eq!(v["database"], json!("ventas"));
    assert_eq!(v["schedule_id"], json!("diario"));
    assert_eq!(v["backup_type"], json!("full"));
    assert_eq!(v["cron"], json!("0 2 * * *"));
    assert_eq!(v["retention"], json!(604_800));
    assert_eq!(v["encryption"], json!("CUSTOMER_MANAGED_ENCRYPTION"));
    assert_eq!(v["kms_keys"], json!(KEY));
    assert_eq!(v["time_zone"], json!("UTC"));
    assert_eq!(v["creation_window"], json!(14_400));
    assert_eq!(v["updated"], json!("2026-11-10T12:00:00Z"));
    let general = &f.sections[0];
    assert!(general.fields[0].read_only && general.fields[1].read_only);
    let status = f.sections.iter().find(|s| s.key == "status").unwrap();
    assert!(status.fields.iter().all(|x| x.read_only));
    assert_eq!(f.notes, [TYPE_CHANGE]);

    let mut inc = sched();
    inc.as_object_mut().unwrap().remove("fullBackupSpec");
    inc.as_object_mut().unwrap().remove("encryptionConfig");
    inc["incrementalBackupSpec"] = json!({});
    let c = current_of(&inc).unwrap();
    assert!(c.spec.incremental);
    assert_eq!(c.spec.encryption, "USE_DATABASE_ENCRYPTION");
    let f = form_of(Some(&c), &[], "ventas");
    assert_eq!(f.values["backup_type"], json!("incremental"));
    assert_eq!(f.notes, [TYPE_CHANGE, CHAIN]);

    assert!(current_of(
        &json!({ "name": "projects/p/instances/i/databases/Bad/backupSchedules/x" })
    )
    .is_none());
}

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

#[test]
fn a_new_schedule_is_one_post() {
    let w = wanted(Vals(&new_values()), None, "ventas").unwrap();
    let p = create_plan(INSTANCE, &w);
    assert_eq!(p.id, "ventas/semanal");
    let sc = p.script("https://spanner.googleapis.com");
    assert!(sc.lines().all(|l| l.starts_with('#')), "{sc}");
    assert!(sc.starts_with("# Crea el programa de backup «semanal» de la base «ventas» de Spanner: backups completos con el cron 0 3 * * 0 (UTC).\n"), "{sc}");
    assert!(sc.contains("\n# POST https://spanner.googleapis.com/v1/projects/p1/instances/i1/databases/ventas/backupSchedules?backupScheduleId=semanal\n"), "{sc}");
    assert_eq!(
        body_at(&sc, 0),
        json!({
            "spec": { "cronSpec": { "text": "0 3 * * 0" } },
            "retentionDuration": "2592000s",
            "encryptionConfig": { "encryptionType": "USE_DATABASE_ENCRYPTION" },
            "fullBackupSpec": {}
        })
    );
    let mut v = new_values();
    v.extend(map(json!({
        "backup_type": "incremental", "cron": "0 */4 * * *", "encryption": "CUSTOMER_MANAGED_ENCRYPTION",
        "kms_keys": format!("{KEY}\n projects/p1/locations/eu/keyRings/r/cryptoKeys/k2 ,")
    })));
    let w = wanted(Vals(&v), None, "ventas").unwrap();
    let sc = create_plan(INSTANCE, &w).script("http://localhost:9020");
    assert!(sc.contains("backups incrementales"));
    assert_eq!(
        body_at(&sc, 0),
        json!({
            "spec": { "cronSpec": { "text": "0 */4 * * *" } },
            "retentionDuration": "2592000s",
            "encryptionConfig": { "encryptionType": "CUSTOMER_MANAGED_ENCRYPTION",
                "kmsKeyNames": [KEY, "projects/p1/locations/eu/keyRings/r/cryptoKeys/k2"] },
            "incrementalBackupSpec": {}
        })
    );
}

fn edit(c: &Current, changes: Json) -> Plan {
    let w = wanted(Vals(&with(c, changes)), Some(c), "ventas").unwrap();
    edit_plan(INSTANCE, &c.spec, &w)
}

#[test]
fn an_edit_sends_only_what_changed() {
    let c = cur();
    let p = edit(&c, json!({}));
    assert!(p.reqs.is_empty() && !p.recreate);
    assert_eq!(p.script("x"), "# «diario»: no hay cambios que guardar.\n");
    // What can't change (database, id) is the schedule's whatever comes.
    assert!(
        edit(&c, json!({ "database": "otra", "schedule_id": "otro" }))
            .reqs
            .is_empty()
    );
    // The same keys in another order are the same.
    let mut two = sched();
    two["encryptionConfig"] = json!({ "encryptionType": "CUSTOMER_MANAGED_ENCRYPTION", "kmsKeyNames": [KEY, "projects/p1/locations/eu/keyRings/r/cryptoKeys/k2"] });
    let c2 = current_of(&two).unwrap();
    assert!(edit(
        &c2,
        json!({ "kms_keys": format!("projects/p1/locations/eu/keyRings/r/cryptoKeys/k2\n{KEY}") })
    )
    .reqs
    .is_empty());

    let base = "https://spanner.googleapis.com";
    let path = |mask: &str| format!("\n# PATCH {base}/v1/{NAME}?updateMask={mask}\n");
    let p = edit(&c, json!({ "cron": "0 3 * * *" }));
    let sc = p.script(base);
    assert!(sc.contains(&path("spec.cron_spec.text")), "{sc}");
    assert_eq!(
        body_at(&sc, 0),
        json!({ "spec": { "cronSpec": { "text": "0 3 * * *" } } })
    );
    assert_eq!(p.id, "ventas/diario");

    let sc = edit(&c, json!({ "retention": 86_400 })).script(base);
    assert!(sc.contains(&path("retention_duration")), "{sc}");
    assert_eq!(body_at(&sc, 0), json!({ "retentionDuration": "86400s" }));

    let sc = edit(&c, json!({ "encryption": "GOOGLE_DEFAULT_ENCRYPTION" })).script(base);
    assert!(sc.contains(&path("encryption_config")), "{sc}");
    assert_eq!(
        body_at(&sc, 0),
        json!({ "encryptionConfig": { "encryptionType": "GOOGLE_DEFAULT_ENCRYPTION" } })
    );

    let sc = edit(
        &c,
        json!({ "cron": "0 4 * * *", "retention": 86_400, "kms_keys": "projects/p1/locations/us/keyRings/r/cryptoKeys/k9" }),
    )
    .script(base);
    assert!(
        sc.contains(&path(
            "spec.cron_spec.text,retention_duration,encryption_config"
        )),
        "{sc}"
    );
    assert_eq!(
        body_at(&sc, 0)["encryptionConfig"],
        json!({ "encryptionType": "CUSTOMER_MANAGED_ENCRYPTION", "kmsKeyNames": ["projects/p1/locations/us/keyRings/r/cryptoKeys/k9"] })
    );

    // An incremental schedule's edit says its chain starts again.
    let mut inc = sched();
    inc.as_object_mut().unwrap().remove("fullBackupSpec");
    inc["incrementalBackupSpec"] = json!({});
    let ci = current_of(&inc).unwrap();
    assert!(edit(&ci, json!({ "retention": 86_400 }))
        .script(base)
        .contains(CHAIN));
}

#[test]
fn a_new_backup_type_deletes_and_creates_again() {
    let c = cur();
    let p = edit(
        &c,
        json!({ "backup_type": "incremental", "cron": "0 */6 * * *" }),
    );
    assert!(p.recreate);
    let sc = p.script("https://spanner.googleapis.com");
    assert!(sc.starts_with("# Cambia el programa de backup «diario» de la base «ventas» de Spanner de backups completos a incrementales.\n"), "{sc}");
    assert!(sc.contains(TYPE_CHANGE));
    let delete = sc
        .find(&format!(
            "# DELETE https://spanner.googleapis.com/v1/{NAME}\n"
        ))
        .unwrap();
    let post = sc.find("# POST https://spanner.googleapis.com/v1/projects/p1/instances/i1/databases/ventas/backupSchedules?backupScheduleId=diario\n").unwrap();
    assert!(delete < post, "{sc}");
    assert_eq!(
        body_at(&sc, 1),
        json!({
            "spec": { "cronSpec": { "text": "0 */6 * * *" } },
            "retentionDuration": "604800s",
            "encryptionConfig": { "encryptionType": "CUSTOMER_MANAGED_ENCRYPTION", "kmsKeyNames": [KEY] },
            "incrementalBackupSpec": {}
        })
    );
    // A full schedule's cron is checked as one.
    let v = with(&c, json!({ "cron": "0 */6 * * *" }));
    assert!(wanted(Vals(&v), Some(&c), "ventas").is_err());
}

#[test]
fn delete_script_and_items() {
    let sc = delete_plan(INSTANCE, &cur().spec).script("https://spanner.googleapis.com");
    assert_eq!(
        sc,
        format!("# Borra el programa de backup «diario» de la base «ventas» de Spanner. Los backups que ya hizo se conservan hasta que vencen.\n#\n# DELETE https://spanner.googleapis.com/v1/{NAME}\n")
    );
    let j = ServerJob {
        id: "ventas/diario".into(),
        name: "diario".into(),
        group: Some("ventas".into()),
        description: Some("Backup incremental".into()),
        schedule: Some("0 */4 * * *".into()),
        ..Default::default()
    };
    let i = item_of(j.clone());
    assert_eq!(i.id, "ventas/diario");
    assert_eq!(
        i.description.as_deref(),
        Some("Backup incremental de «ventas», cron 0 */4 * * * (UTC)")
    );
    assert_eq!(i.enabled, None);
    let i = item_of(ServerJob {
        description: Some("Backup completo".into()),
        ..j
    });
    assert!(i
        .description
        .unwrap()
        .starts_with("Backup completo de «ventas»"));
}

// ---------------------------------------------------------------------------
// Against a stand-in Admin API
// ---------------------------------------------------------------------------

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

fn session(base: &str) -> SpannerSession {
    let http = reqwest::Client::new();
    let cfg = ConnectionConfig {
        options: [("endpoint_url".to_string(), base.to_string())].into(),
        ..Default::default()
    };
    SpannerSession {
        api: Api {
            tokens: gcp::Tokens::from_config(&cfg, http.clone()).unwrap(),
            http,
            base: base.into(),
            last_status: Default::default(),
        },
        instance: INSTANCE.into(),
        database: format!("{INSTANCE}/databases/ventas"),
        session: String::new(),
        seqno: 0,
        read_only: false,
        profiler: None,
        manual: false,
        tx: None,
        tx_failed: false,
    }
}

type Seen = Arc<Mutex<Vec<(String, String, String)>>>;

#[derive(Default)]
struct Knobs {
    edition: &'static str,
    emulator: bool,
    fail_create: bool,
    /// The lists the checks read are refused (a login without them).
    deny_lists: bool,
}

/// The Admin API: databases `ventas` (one schedule, `diario`) and `llena`
/// (four), the instance's edition, IAM granting create and update.
async fn admin(seen: Seen, knobs: Arc<Mutex<Knobs>>) -> String {
    serve(Arc::new(move |m, p, b| {
        seen.lock()
            .unwrap()
            .push((m.to_string(), p.to_string(), b.to_string()));
        let k = knobs.lock().unwrap();
        if k.emulator && p.contains("backupSchedules") {
            return (501, json!({ "code": 12 }).to_string());
        }
        let (path, _query) = p.split_once('?').unwrap_or((p, ""));
        if k.deny_lists
            && m == "GET"
            && (path.ends_with("/databases") || path.ends_with("/backupSchedules"))
        {
            return (
                403,
                json!({ "error": { "code": 403, "message": "Permission denied", "status": "PERMISSION_DENIED" } }).to_string(),
            );
        }
        let db = format!("/v1/{INSTANCE}/databases");
        let full = |id: &str| {
            json!({ "name": format!("{INSTANCE}/databases/llena/backupSchedules/{id}"),
                    "spec": {"cronSpec": {"text": "0 2 * * *"}}, "retentionDuration": "86400s", "fullBackupSpec": {} })
        };
        let body = match (m, path) {
            ("GET", x) if x == format!("/v1/{INSTANCE}") => {
                json!({ "name": INSTANCE, "edition": k.edition })
            }
            ("GET", x) if x == db => json!({ "databases": [
                { "name": format!("{INSTANCE}/databases/ventas") },
                { "name": format!("{INSTANCE}/databases/llena") }
            ] }),
            ("GET", x) if x == format!("{db}/ventas/backupSchedules") => {
                json!({ "backupSchedules": [sched()] })
            }
            ("GET", x) if x == format!("{db}/llena/backupSchedules") => {
                json!({ "backupSchedules": [full("a1"), full("a2"), full("a3"), full("a4")] })
            }
            ("GET", x) if x == format!("/v1/{NAME}") => sched(),
            ("GET", x) if x == format!("/v1/{INSTANCE}/backups") => json!({ "backups": [] }),
            ("POST", x) if x == format!("{db}/ventas/backupSchedules") && k.fail_create => {
                return (
                    400,
                    json!({ "error": { "code": 400, "message": "Invalid cron", "status": "INVALID_ARGUMENT" } }).to_string(),
                )
            }
            ("POST", x) if x == format!("{db}/ventas/backupSchedules") => sched(),
            ("PATCH", x) if x == format!("/v1/{NAME}") => sched(),
            ("DELETE", x) if x == format!("/v1/{NAME}") => json!({}),
            ("POST", x) if x == format!("{db}/ventas:testIamPermissions") => {
                json!({ "permissions": ["spanner.backupSchedules.create", "spanner.backupSchedules.update"] })
            }
            _ => {
                return (
                    404,
                    json!({ "error": { "code": 404, "message": "NOT_FOUND: not found", "status": "NOT_FOUND" } }).to_string(),
                )
            }
        };
        (200, body.to_string())
    }))
    .await
}

#[tokio::test]
async fn the_designer_calls_the_admin_api_as_documented() {
    let seen: Seen = Default::default();
    let knobs: Arc<Mutex<Knobs>> = Arc::new(Mutex::new(Knobs {
        edition: "ENTERPRISE",
        ..Default::default()
    }));
    let base = admin(seen.clone(), knobs.clone()).await;
    let mut s = session(&base);
    let calls = || seen.lock().unwrap().clone();

    // What the login may do, from IAM; the edition's limits.
    let info = info(&s).await.unwrap();
    let k = &info.kinds[0];
    assert_eq!(k.key, "job");
    assert!(k.can_create && k.can_edit && !k.can_delete);
    // No steps, no run call, a history that is the backups themselves.
    assert!(!k.run_from_step && !k.clear_history && k.actions.is_empty());
    assert_eq!(info.notes, [NO_DELETE]);
    knobs.lock().unwrap().edition = "STANDARD";
    assert_eq!(super::info(&s).await.unwrap().notes, [NO_DELETE, STANDARD]);

    let list = items(&mut s, "job").await.unwrap();
    assert_eq!(list.items.len(), 5);
    assert!(list.items.iter().any(|i| i.id == "ventas/diario"));

    let f = form(&s, "job", Some("ventas/diario")).await.unwrap();
    assert!(!f.read_only);
    assert!(f.notes.contains(&STANDARD.to_string()));
    let f = form(&s, "job", None).await.unwrap();
    assert_eq!(f.choices["database"].len(), 2);

    // Create: checked against what the instance has.
    let refused = |e: Error, expect: &str| {
        let e = e.to_string();
        assert!(e.contains(expect), "{e}");
    };
    let mut v = new_values();
    v.insert("backup_type".into(), json!("incremental"));
    refused(s.plan(None, &fv(v)).await.unwrap_err(), "edición Standard");
    knobs.lock().unwrap().edition = "ENTERPRISE";
    let mut v = new_values();
    v.insert("database".into(), json!("no_existe"));
    refused(
        s.plan(None, &fv(v)).await.unwrap_err(),
        "no existe en la instancia",
    );
    let mut v = new_values();
    v.insert("schedule_id".into(), json!("diario"));
    refused(s.plan(None, &fv(v)).await.unwrap_err(), "Ya existe");
    let mut v = new_values();
    v.insert("database".into(), json!("llena"));
    refused(
        s.plan(None, &fv(v)).await.unwrap_err(),
        "ya tiene 4 programas",
    );
    assert!(calls()
        .iter()
        .all(|c| c.0 == "GET" || c.1.contains("testIamPermissions")));

    let id = save(&s, "job", None, &fv(new_values())).await.unwrap();
    assert_eq!(id.as_deref(), Some("ventas/semanal"));
    let post = calls()
        .into_iter()
        .find(|c| c.0 == "POST" && c.1.contains("backupScheduleId"))
        .unwrap();
    assert_eq!(
        post.1,
        format!("/v1/{INSTANCE}/databases/ventas/backupSchedules?backupScheduleId=semanal")
    );
    assert_eq!(
        serde_json::from_str::<Json>(&post.2).unwrap()["retentionDuration"],
        json!("2592000s")
    );

    // Edit in place; nothing when nothing changed.
    let c = cur();
    let before = calls().len();
    save(&s, "job", Some("ventas/diario"), &fv(values_of(&c)))
        .await
        .unwrap();
    assert!(calls()[before..].iter().all(|c| c.0 == "GET"));
    save(
        &s,
        "job",
        Some("ventas/diario"),
        &fv(with(&c, json!({ "retention": 86_400 }))),
    )
    .await
    .unwrap();
    let patch = calls().into_iter().find(|c| c.0 == "PATCH").unwrap();
    assert_eq!(patch.1, format!("/v1/{NAME}?updateMask=retention_duration"));
    assert_eq!(
        serde_json::from_str::<Json>(&patch.2).unwrap(),
        json!({ "retentionDuration": "86400s" })
    );

    // A new type: delete, then create; a refused create says what was lost.
    let before = calls().len();
    let typed = with(
        &c,
        json!({ "backup_type": "incremental", "cron": "0 */4 * * *" }),
    );
    save(&s, "job", Some("ventas/diario"), &fv(typed.clone()))
        .await
        .unwrap();
    let sent: Vec<String> = calls()[before..]
        .iter()
        .filter(|c| c.0 != "GET")
        .map(|c| c.0.clone())
        .collect();
    assert_eq!(sent, ["DELETE", "POST"]);
    knobs.lock().unwrap().fail_create = true;
    refused(
        save(&s, "job", Some("ventas/diario"), &fv(typed))
            .await
            .unwrap_err(),
        "Se borró el programa de backup «ventas/diario» para cambiar su tipo",
    );
    knobs.lock().unwrap().fail_create = false;

    // Without the lists, the checks are Spanner's: the plan and the form
    // still come.
    knobs.lock().unwrap().deny_lists = true;
    let p = s.plan(None, &fv(new_values())).await.unwrap();
    assert_eq!(p.id, "ventas/semanal");
    let f = form(&s, "job", None).await.unwrap();
    assert_eq!(
        f.choices["database"],
        [("ventas".to_string(), "ventas".to_string())]
    );
    knobs.lock().unwrap().deny_lists = false;

    // Delete.
    let sc = delete_script(&s, "job", "ventas/diario").await.unwrap();
    assert!(sc.contains(&format!("# DELETE {base}/v1/{NAME}")));
    let before = calls().len();
    delete(&s, "job", "ventas/diario").await.unwrap();
    assert!(calls()[before..]
        .iter()
        .any(|c| c.0 == "DELETE" && c.1 == format!("/v1/{NAME}")));

    // A schedule that isn't there, and the history.
    refused(
        form(&s, "job", Some("ventas/otro")).await.unwrap_err(),
        "no existe el programa de backup",
    );
    assert!(matches!(
        clear_history("ventas/diario").await,
        Err(Error::Unsupported(m)) if m == NO_HISTORY
    ));

    // A bad id or kind never reaches the API.
    let before = calls().len();
    for bad in [
        "ventas",
        "ventas/../x",
        "Ventas/diario",
        "ventas/a b",
        "ventas/diario?x=1",
        "",
    ] {
        assert!(form(&s, "job", Some(bad)).await.is_err());
        assert!(script(&s, "job", Some(bad), &fv(new_values()))
            .await
            .is_err());
        assert!(save(&s, "job", Some(bad), &fv(new_values())).await.is_err());
        assert!(delete_script(&s, "job", bad).await.is_err());
        assert!(delete(&s, "job", bad).await.is_err());
        assert!(clear_history(bad).await.is_err());
    }
    assert!(form(&s, "backup", None).await.is_err());
    assert_eq!(calls().len(), before, "a bad id never reaches the API");

    // The emulator: no kinds, and every call refused as unsupported.
    knobs.lock().unwrap().emulator = true;
    let info = super::info(&s).await.unwrap();
    assert!(info.kinds.is_empty());
    assert_eq!(info.notes, [EMULATOR]);
    assert!(matches!(
        form(&s, "job", None).await,
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        form(&s, "job", Some("ventas/diario")).await,
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        save(&s, "job", None, &fv(new_values())).await,
        Err(Error::Unsupported(_))
    ));
    assert!(matches!(
        delete(&s, "job", "ventas/diario").await,
        Err(Error::Unsupported(_))
    ));
}
