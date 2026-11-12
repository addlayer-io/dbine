use super::plan::{self, plan_from, plan_json, plan_of, PlanDef};
use super::repo::{self, Stored};
use super::*;
use dbine_driver::FormValues;
use serde_json::json;

fn ctx8() -> Ctx {
    Ctx {
        service: true,
        enterprise: true,
        read: true,
        write: true,
        events: true,
        version: (8, 0),
    }
}

fn ctx7() -> Ctx {
    Ctx {
        version: (7, 2),
        ..ctx8()
    }
}

fn vals(v: serde_json::Value) -> BTreeMap<String, Value> {
    serde_json::from_value(v).unwrap()
}

fn form_values(v: serde_json::Value, tasks: Vec<serde_json::Value>) -> FormValues {
    FormValues {
        values: vals(v),
        lists: BTreeMap::from([("tasks".to_string(), tasks.into_iter().map(vals).collect())]),
    }
}

fn err(r: Result<impl std::fmt::Debug>) -> String {
    match r {
        Err(e) => e.to_string(),
        Ok(v) => panic!("expected an error, got {v:?}"),
    }
}

fn hourly() -> serde_json::Value {
    json!({"name": "hourly", "task_type": "BACKUP", "full_backup": true,
           "frequency": 1, "period": "HOURS", "time": "00:30:00"})
}

// ---------------------------------------------------------------------------
// Common
// ---------------------------------------------------------------------------

#[test]
fn versions_names_and_times() {
    assert_eq!(version_of("8.0.0-3777-enterprise"), (8, 0));
    assert_eq!(version_of("7.6.2-1234-enterprise"), (7, 6));
    assert_eq!(version_of(""), (0, 0));
    assert!(new_name("nightly-1_b") && new_name("A"));
    for bad in ["", "_x", "-x", "a.b", "a b", "a/b", &"x".repeat(51)] {
        assert!(!new_name(bad), "{bad:?}");
    }
    assert_eq!(parse_time("22:00:00").as_deref(), Some("22:00"));
    assert_eq!(parse_time("07:05").as_deref(), Some("07:05"));
    for bad in ["24:00", "10:60", "10:00:30", "7:00", "aa:bb", ""] {
        assert_eq!(parse_time(bad), None, "{bad:?}");
    }
    assert_eq!(form_time("22:00"), "22:00:00");
    assert!(Kind::parse("job").is_ok() && Kind::parse("settings").is_ok());
    assert!(err(Kind::parse("schedule")).contains("no es un tipo"));
}

#[test]
fn service_errors_read_as_messages() {
    assert_eq!(
        service_error(
            400,
            r#"{"status":400,"msg":"Cannot update a default plan"}"#
        ),
        "El servicio de backup respondió: Cannot update a default plan"
    );
    assert_eq!(
        service_error(500, r#"{"status":500,"msg":"boom","extras":"disk"}"#),
        "El servicio de backup respondió: boom (disk)"
    );
    assert!(service_error(403, r#"{"msg":"forbidden"}"#).starts_with("Sin permiso"));
    assert!(service_error(404, "404 page not found").contains("HTTP 404"));
}

#[test]
fn the_kinds_follow_the_user() {
    let i = info_of(&ctx8());
    let keys: Vec<&str> = i.kinds.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, ["job", "plan", "archived", "imported", "settings"]);
    assert!(i.kinds[0].can_create && i.kinds[0].can_edit && i.kinds[0].can_delete);
    assert!(i.kinds[2].can_create && !i.kinds[2].can_edit && i.kinds[2].can_delete);
    assert!(!i.kinds[4].can_create && i.kinds[4].can_edit && !i.kinds[4].can_delete);
    assert!(i.logs);
    assert!(
        !info_of(&Ctx {
            events: false,
            ..ctx8()
        })
        .logs
    );
    assert!(
        !info_of(&Ctx {
            version: (7, 0),
            ..ctx8()
        })
        .logs
    );

    let ro = info_of(&Ctx {
        write: false,
        ..ctx8()
    });
    assert!(ro
        .kinds
        .iter()
        .all(|k| !k.can_create && !k.can_edit && !k.can_delete));
    assert!(ro.notes.iter().any(|n| n.contains("no cambiarlo")));

    let none = info_of(&Ctx {
        service: false,
        ..ctx8()
    });
    assert!(none.kinds.is_empty() && none.notes[0].contains("no tiene el servicio"));
    let ce = info_of(&Ctx {
        service: false,
        enterprise: false,
        ..ctx8()
    });
    assert!(ce.notes[0].contains("Community"));
    let denied = info_of(&Ctx {
        read: false,
        ..ctx8()
    });
    assert!(denied.kinds.is_empty() && denied.notes[0].contains("no puede ver"));
}

#[test]
fn scripts_mask_secrets_and_keep_comments_on_one_line() {
    let ch = Change {
        comments: vec!["a\nPOST /x".into()],
        calls: vec![Call {
            method: Method::POST,
            path: "/cluster/self/repository/active/r".into(),
            body: Some(json!({"plan": "p", "cloud_credentials_key": "s3cr3t"})),
            secrets: vec!["cloud_credentials_key"],
        }],
        undo: None,
        id: None,
    };
    let sc = ch.script();
    assert!(sc.starts_with(HEADER));
    assert!(sc.contains("// a?POST /x\n"));
    assert!(sc.contains("POST /api/v1/cluster/self/repository/active/r\n{"));
    assert!(sc.contains(SECRET_MASK) && !sc.contains("s3cr3t"));
    // The real value is what gets sent.
    assert_eq!(
        ch.calls[0].body.as_ref().unwrap()["cloud_credentials_key"],
        "s3cr3t"
    );
    assert!(Change::nothing("p", "p".into())
        .script()
        .contains("«p»: no hay cambios que guardar."));
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

fn builtin() -> serde_json::Value {
    json!({"name": "_hourly_backups", "description": "Do a backup an hour", "services": null,
        "default": true, "tasks": [
        {"name": "backup_hourly", "task_type": "BACKUP", "full_backup": false,
         "schedule": {"job_type": "BACKUP", "frequency": 1, "period": "HOURS", "time": "00:00"}},
        {"name": "merge_week", "task_type": "MERGE", "full_backup": false,
         "merge_options": {"offset_start": 0, "offset_end": 6},
         "schedule": {"job_type": "MERGE", "frequency": 1, "period": "SUNDAY", "time": "23:40"}}
    ]})
}

#[test]
fn plans_read_and_write_as_the_service_keeps_them() {
    let p = plan_of(&builtin()).unwrap();
    assert!(p.default && p.services.is_none() && p.tasks.len() == 2);
    assert_eq!(p.tasks[1].merge, Some((0, 6)));
    assert_eq!(p.tasks[0].time.as_deref(), Some("00:00"));
    let j = plan_json(&p);
    assert_eq!(j["services"], Value::Null);
    assert_eq!(j["tasks"][1]["schedule"]["job_type"], "MERGE");
    assert_eq!(j["tasks"][1]["merge_options"]["offset_end"], 6);
    assert!(j.get("default").is_none(), "the default flag isn't sent");
    // What the service answers reads back the same.
    assert_eq!(
        plan_of(&json!({"name": p.name, "description": p.description,
        "default": true, "services": null, "tasks": j["tasks"]}))
        .unwrap(),
        p
    );
}

#[test]
fn a_plan_from_the_form() {
    let c = ctx8();
    let v = form_values(
        json!({"name": "p", "description": "cada hora", "services": ["data", "gsi"]}),
        vec![
            hourly(),
            json!({"name": "merge", "task_type": "MERGE", "frequency": 1, "period": "SUNDAY",
                   "time": "23:50:00", "offset_start": 0, "offset_end": 6, "full_backup": true}),
            json!({"name": "prune", "task_type": "PRUNE", "frequency": 1, "period": "DAYS", "time": "02:00:00"}),
            json!({"name": "often", "task_type": "BACKUP", "frequency": 30, "period": "MINUTES", "time": "10:00:00"}),
        ],
    );
    let p = plan_from(&c, "p", &v).unwrap();
    assert_eq!(
        p.services,
        Some(vec!["data".to_string(), "gsi".to_string()])
    );
    assert_eq!(p.tasks[0].time.as_deref(), Some("00:30"));
    assert!(p.tasks[0].full_backup);
    assert!(!p.tasks[1].full_backup, "a merge is never a full backup");
    assert_eq!(p.tasks[1].merge, Some((0, 6)));
    assert_eq!(p.tasks[3].time, None, "minutes have no time");
    let sc = plan::create_change(&p).script();
    assert!(sc.contains("POST /api/v1/plan/p\n{"), "{sc}");
    assert!(sc.contains("\"period\": \"MINUTES\""));

    // Every service is the service's `null`.
    let all = form_values(json!({"services": plan::SERVICES}), vec![hourly()]);
    assert_eq!(plan_from(&c, "p", &all).unwrap().services, None);
    let missing = form_values(json!({}), vec![hourly()]);
    assert_eq!(plan_from(&c, "p", &missing).unwrap().services, None);
}

#[test]
fn bad_plans_are_refused() {
    let c = ctx8();
    let check = |values: serde_json::Value, tasks: Vec<serde_json::Value>, want: &str| {
        let e = err(plan_from(&c, "p", &form_values(values, tasks)));
        assert!(e.contains(want), "{e} / {want}");
    };
    check(json!({}), vec![], "al menos una tarea");
    check(
        json!({}),
        vec![
            json!({"name": "m", "task_type": "MERGE", "frequency": 1, "period": "DAYS", "time": "01:00:00"}),
        ],
        "tarea de backup",
    );
    check(
        json!({}),
        vec![hourly(), hourly()],
        "dos tareas con el nombre",
    );
    check(
        json!({}),
        vec![
            json!({"name": "a b", "task_type": "BACKUP", "frequency": 1, "period": "DAYS", "time": "01:00:00"}),
        ],
        "el nombre de una tarea",
    );
    check(
        json!({}),
        vec![
            json!({"name": "a", "task_type": "BACKUP", "frequency": 1, "period": "DAYS", "time": "25:00:00"}),
        ],
        "no es una hora válida",
    );
    check(
        json!({}),
        vec![json!({"name": "a", "task_type": "BACKUP", "frequency": 1, "period": "DAYS"})],
        "necesita la hora",
    );
    check(
        json!({}),
        vec![
            json!({"name": "a", "task_type": "BACKUP", "frequency": 201, "period": "DAYS", "time": "01:00:00"}),
        ],
        "entre 1 y 200",
    );
    check(
        json!({}),
        vec![
            json!({"name": "a", "task_type": "BACKUP", "frequency": 1, "period": "YEARS", "time": "01:00:00"}),
        ],
        "necesita un período",
    );
    check(
        json!({}),
        vec![
            json!({"name": "a", "task_type": "RESTORE", "frequency": 1, "period": "DAYS", "time": "01:00:00"}),
        ],
        "no es un tipo de tarea",
    );
    check(
        json!({}),
        vec![
            hourly(),
            json!({"name": "m", "task_type": "MERGE", "frequency": 1, "period": "DAYS", "time": "01:00:00", "offset_start": 1}),
        ],
        "juntos",
    );
    check(
        json!({}),
        vec![
            hourly(),
            json!({"name": "m", "task_type": "MERGE", "frequency": 1, "period": "DAYS", "time": "01:00:00", "offset_start": 7, "offset_end": 1}),
        ],
        "no puede ser más días",
    );
    check(
        json!({"services": []}),
        vec![hourly()],
        "al menos un servicio",
    );
    check(
        json!({"services": ["data", "bogus"]}),
        vec![hourly()],
        "no es un servicio",
    );
    check(
        json!({"description": "x".repeat(141)}),
        vec![hourly()],
        "140",
    );
    let many: Vec<_> = (0..25)
        .map(|i| json!({"name": format!("b{i}"), "task_type": "BACKUP", "frequency": 1, "period": "DAYS", "time": "01:00:00"}))
        .collect();
    check(json!({}), many, "hasta 24 tareas");
    // Prune before 8.0.
    let e = err(plan_from(
        &ctx7(),
        "p",
        &form_values(
            json!({}),
            vec![
                hourly(),
                json!({"name": "x", "task_type": "PRUNE", "frequency": 1, "period": "DAYS", "time": "01:00:00"}),
            ],
        ),
    ));
    assert!(e.contains("8.0"), "{e}");
}

#[test]
fn plan_edits_say_what_changes() {
    let c = ctx8();
    let old = plan_from(
        &c,
        "p",
        &form_values(json!({"description": "a"}), vec![hourly()]),
    )
    .unwrap();
    let same = plan_from(
        &c,
        "p",
        &form_values(
            json!({"description": "a", "services": plan::SERVICES}),
            vec![hourly()],
        ),
    )
    .unwrap();
    assert!(plan::update_change(&old, &same).calls.is_empty());
    let mut t = hourly();
    t["frequency"] = json!(2);
    let new = plan_from(&c, "p", &form_values(json!({"description": "b"}), vec![t])).unwrap();
    let ch = plan::update_change(&old, &new);
    let sc = ch.script();
    assert!(sc.contains("descripción, tareas"), "{sc}");
    assert!(sc.contains("PUT /api/v1/plan/p\n{"));
    assert!(sc.contains("\"frequency\": 2"));
    let del = plan::delete_change(&old).script();
    assert!(del.contains("DELETE /api/v1/plan/p"), "{del}");
}

#[test]
fn plan_forms() {
    let c = ctx8();
    let p = plan_of(&builtin()).unwrap();
    let f = plan::build_form(&c, Some(&p), &[]);
    assert!(f.read_only && f.notes.iter().any(|n| n.contains("viene con Couchbase")));
    let rows = &f.sections[1].lists[0].rows;
    assert_eq!(rows[0]["time"], "00:00:00");
    assert_eq!(rows[1]["offset_end"], 6);
    assert_eq!(f.values["services"], json!(plan::SERVICES));
    let mine = PlanDef {
        default: false,
        ..p.clone()
    };
    let used = plan::build_form(&c, Some(&mine), &["r1".into()]);
    assert!(used.read_only && used.notes.iter().any(|n| n.contains("«r1»")));
    let free = plan::build_form(&c, Some(&mine), &[]);
    assert!(!free.read_only);
    assert!(free.sections[0].fields[0].read_only, "the name stays");
    let new = plan::build_form(&ctx7(), None, &[]);
    assert!(!new.read_only && new.sections[1].lists[0].rows.is_empty());
    // No prune option before 8.0.
    let types = &new.sections[1].lists[0].fields[1].kind;
    assert!(!serde_json::to_string(types).unwrap().contains("PRUNE"));
    assert!(plan::parse_id("_hourly_backups").is_ok());
    for bad in ["", "a/b", "a b", "..", "a?x"] {
        assert!(plan::parse_id(bad).is_err(), "{bad:?}");
    }
}

// ---------------------------------------------------------------------------
// Repositories
// ---------------------------------------------------------------------------

#[test]
fn archives_split_and_build() {
    assert_eq!(
        repo::split_archive("s3://bkt/a/b"),
        ("s3".into(), "".into(), "bkt".into(), "a/b".into())
    );
    assert_eq!(
        repo::split_archive("/data/backups"),
        (
            "filesystem".into(),
            "/data/backups".into(),
            "".into(),
            "".into()
        )
    );
    let c = ctx8();
    let mut body = Map::new();
    let mut secrets = Vec::new();
    repo::storage_body(
        &c,
        Vals(&vals(json!({"storage": "s3", "cloud_bucket": "s3://bkt/", "path_prefix": "/x/y/",
            "staging_dir": "/stage", "auth_type": "key", "credentials_id": "AKIA", "credentials_key": "k",
            "region": "us-east-1", "force_path_style": true}))),
        &mut body,
        &mut secrets,
    )
    .unwrap();
    assert_eq!(body["archive"], "s3://bkt/x/y");
    assert_eq!(body["cloud_staging_dir"], "/stage");
    assert_eq!(body["cloud_auth_type"], 0);
    assert_eq!(body["cloud_force_path_style"], true);
    assert_eq!(secrets, ["cloud_credentials_key"]);

    let mut body = Map::new();
    let mut secrets = Vec::new();
    repo::storage_body(
        &ctx7(),
        Vals(&vals(
            json!({"storage": "az", "cloud_bucket": "cont", "staging_dir": "/s",
            "auth_type": "instance", "storage_account": "acct", "force_path_style": true}),
        )),
        &mut body,
        &mut secrets,
    )
    .unwrap();
    assert_eq!(body["archive"], "az://cont/");
    assert_eq!(body["cloud_credentials_id"], "acct");
    assert!(body.get("cloud_auth_type").is_none(), "7.x doesn't take it");
    assert!(body.get("cloud_force_path_style").is_none(), "S3 only");
    assert!(secrets.is_empty());

    for (v, want) in [
        (json!({"storage": "filesystem"}), "directorio del archivo"),
        (
            json!({"storage": "filesystem", "archive": "/a\nb"}),
            "saltos de línea",
        ),
        (
            json!({"storage": "gs", "cloud_bucket": "a/b", "staging_dir": "/s"}),
            "sin barras",
        ),
        (
            json!({"storage": "gs", "cloud_bucket": "a"}),
            "directorio de preparación",
        ),
        (
            json!({"storage": "gs", "cloud_bucket": "a", "staging_dir": "/s", "auth_type": "key"}),
            "ID de la clave",
        ),
        (
            json!({"storage": "gs", "cloud_bucket": "a", "staging_dir": "/s", "credentials_id": "i"}),
            "clave secreta",
        ),
        (json!({"storage": "ftp"}), "no es un almacenamiento"),
    ] {
        let mut body = Map::new();
        let e = err(repo::storage_body(&c, Vals(&vals(v)), &mut body, &mut Vec::new()).map(|_| ()));
        assert!(e.contains(want), "{e} / {want}");
    }
}

#[test]
fn encryption_is_8_0_and_hides_its_key() {
    let v = vals(json!({"encrypted": true, "key_url": "awskms://alias/k",
        "km_auth_type": "credentials", "km_credentials_id": "id", "km_credentials_key": "kk", "km_region": "r"}));
    let mut body = Map::new();
    let mut secrets = Vec::new();
    repo::encryption_body(&ctx8(), Vals(&v), &mut body, &mut secrets).unwrap();
    assert_eq!(body["encryption_key_url"], "awskms://alias/k");
    assert_eq!(body["km_auth_type"], "credentials");
    assert_eq!(secrets, ["km_credentials_key"]);
    let e =
        err(repo::encryption_body(&ctx7(), Vals(&v), &mut Map::new(), &mut Vec::new()).map(|_| ()));
    assert!(e.contains("8.0"));
    let e = err(repo::encryption_body(
        &ctx8(),
        Vals(&vals(json!({"encrypted": true}))),
        &mut Map::new(),
        &mut Vec::new(),
    )
    .map(|_| ()));
    assert!(e.contains("URL de su clave"));
    // Not encrypted: nothing.
    let mut body = Map::new();
    repo::encryption_body(
        &ctx7(),
        Vals(&vals(json!({"encrypted": false}))),
        &mut body,
        &mut Vec::new(),
    )
    .unwrap();
    assert!(body.is_empty());
}

#[test]
fn creating_an_active_repository() {
    let c = ctx8();
    let plans = vec!["p".to_string()];
    let buckets = vec!["sales".to_string()];
    let v = vals(
        json!({"name": "r", "plan": "p", "bucket": "sales", "storage": "filesystem",
        "archive": "/data/b", "retention": 30, "enable_users": true}),
    );
    let (body, secrets) = repo::create_body(&c, Vals(&v), &plans, &buckets).unwrap();
    assert_eq!(body["plan"], "p");
    assert_eq!(body["bucket_name"], "sales");
    assert_eq!(body["default_retention"], 30);
    assert_eq!(body["enable_users"], true);
    let sc = repo::create_change("r", body, secrets, true).script();
    assert!(
        sc.contains("POST /api/v1/cluster/self/repository/active/r\n{"),
        "{sc}"
    );
    assert!(sc.contains("POST /api/v1/cluster/self/repository/active/r/pause"));
    // Before 8.0, no retention or users.
    let (body, _) = repo::create_body(&ctx7(), Vals(&v), &plans, &buckets).unwrap();
    assert!(body.get("default_retention").is_none() && body.get("enable_users").is_none());
    for (v, want) in [
        (
            json!({"plan": "nope", "archive": "/a"}),
            "No existe el plan",
        ),
        (json!({"archive": "/a"}), "necesita un plan"),
        (
            json!({"plan": "p", "bucket": "x", "archive": "/a"}),
            "No existe el bucket",
        ),
        (
            json!({"plan": "p", "archive": "/a", "retention": 40000}),
            "entre 0 y 36500",
        ),
    ] {
        let e = err(repo::create_body(&c, Vals(&vals(v)), &plans, &buckets));
        assert!(e.contains(want), "{e} / {want}");
    }
}

#[test]
fn editing_an_active_repository() {
    // The plan of an active repository: paused around it, resumed after;
    // a failed change resumes it.
    let ch = repo::edit_change("r", ("p1", "p2"), ("active", "active"), None);
    let paths: Vec<String> = ch.calls.iter().map(|c| c.path.clone()).collect();
    assert_eq!(
        paths,
        [
            "/cluster/self/repository/active/r/pause",
            "/cluster/self/repository/active/r/plan/p2",
            "/cluster/self/repository/active/r/resume",
        ]
    );
    assert_eq!(
        ch.undo.as_ref().unwrap().1.path,
        "/cluster/self/repository/active/r/resume"
    );
    assert_eq!(ch.id.as_deref(), Some("repo:r"));
    // A paused one that stays paused: only the plan.
    let ch = repo::edit_change("r", ("p1", "p2"), ("paused", "paused"), None);
    assert_eq!(ch.calls.len(), 1);
    assert!(ch.undo.is_none());
    // Paused → active with the plan: plan, then resume.
    let ch = repo::edit_change("r", ("p1", "p2"), ("paused", "active"), None);
    assert_eq!(ch.calls.len(), 2);
    // State alone.
    let ch = repo::edit_change("r", ("p", "p"), ("active", "paused"), None);
    assert_eq!(ch.calls[0].path, "/cluster/self/repository/active/r/pause");
    // Retention.
    let ch = repo::edit_change(
        "r",
        ("p", "p"),
        ("active", "active"),
        Some((Some(30), 45, true)),
    );
    assert_eq!(
        ch.calls[0].body,
        Some(json!({"period": 45, "retroactive": true}))
    );
    assert!(ch.script().contains("45 días"));
    // Nothing.
    let ch = repo::edit_change(
        "r",
        ("p", "p"),
        ("active", "active"),
        Some((Some(30), 30, false)),
    );
    assert!(ch.calls.is_empty() && ch.script().contains("no hay cambios"));
}

#[test]
fn deleting_and_archiving() {
    assert_eq!(repo::archive_name("r", &[]), "r");
    assert_eq!(repo::archive_name("r", &["r".into()]), "r-2");
    assert_eq!(repo::archive_name("r", &["r".into(), "r-2".into()]), "r-3");
    assert_eq!(repo::archive_name("a.b", &[]), "a_b");
    assert_eq!(repo::archive_name(".x", &[]), "r_x");
    let long = "x".repeat(50);
    let n = repo::archive_name(&long, std::slice::from_ref(&long));
    assert!(n.len() <= 50 && n.ends_with("-2"));

    let ch = repo::active_delete_change("r", "/data/b", "r-2");
    let sc = ch.script();
    assert!(
        sc.contains(
            "POST /api/v1/cluster/self/repository/active/r/archive\n{\n  \"id\": \"r-2\"\n}"
        ),
        "{sc}"
    );
    assert!(
        sc.contains("DELETE /api/v1/cluster/self/repository/archived/r-2?remove_repository=false")
    );
    assert!(sc.contains("«/data/b»"));
    let sc = repo::archive_change("r", "r_old").script();
    assert!(sc.contains("active/r/archive") && sc.contains("\"r_old\""));
    let sc = repo::stored_delete_change(Stored::Imported, "i", "s3://b/x").script();
    assert!(
        sc.contains("DELETE /api/v1/cluster/self/repository/imported/i?remove_repository=false")
    );
}

#[test]
fn importing() {
    let c = ctx8();
    let v = vals(
        json!({"name": "imp", "repo": "a3a8a3e1-c58f-4be7", "storage": "filesystem", "archive": "/a"}),
    );
    let (body, secrets) = repo::import_body(&c, Vals(&v)).unwrap();
    assert_eq!(body["id"], "imp");
    assert_eq!(body["repo"], "a3a8a3e1-c58f-4be7");
    let sc = repo::import_change("imp", body, secrets).script();
    assert!(sc.contains("POST /api/v1/cluster/self/repository/import\n{"));
    for (v, want) in [
        (json!({"repo": "x", "archive": "/a"}), "necesita un nombre"),
        (
            json!({"name": "_x", "repo": "x", "archive": "/a"}),
            "el nombre de un repositorio",
        ),
        (
            json!({"name": "i", "archive": "/a"}),
            "Falta el repositorio",
        ),
        (
            json!({"name": "i", "repo": "../x", "archive": "/a"}),
            "no es el nombre de un repositorio",
        ),
    ] {
        let e = err(repo::import_body(&c, Vals(&vals(v))));
        assert!(e.contains(want), "{e} / {want}");
    }
}

#[test]
fn running_a_task_now() {
    let t = |kind: &str, full: bool, merge: Option<(i64, i64)>| plan::Task {
        name: "t".into(),
        kind: kind.into(),
        full_backup: full,
        frequency: 1,
        period: "DAYS".into(),
        time: Some("01:00".into()),
        merge,
    };
    let c = repo::run_call("r", &t("BACKUP", true, None), false).unwrap();
    assert_eq!(c.path, "/cluster/self/repository/active/r/backup");
    assert_eq!(c.body, Some(json!({"full_backup": true})));
    let c = repo::run_call("r", &t("MERGE", false, Some((0, 6))), false).unwrap();
    assert_eq!(c.body, Some(json!({"offset_start": 0, "offset_end": 6})));
    let c = repo::run_call("r", &t("MERGE", false, None), false).unwrap();
    assert_eq!(c.body, Some(json!({})));
    assert!(err(repo::run_call("r", &t("MERGE", false, None), true)).contains("en la nube"));
    let c = repo::run_call("r", &t("PRUNE", false, None), false).unwrap();
    assert_eq!(c.path, "/cluster/self/repository/active/r/prune");
}

// ---------------------------------------------------------------------------
// Settings and logs
// ---------------------------------------------------------------------------

#[test]
fn service_settings() {
    let cfg = json!({"history_rotation_size": 50});
    let f = settings::build_form(&cfg);
    assert_eq!(f.sections[0].fields.len(), 1);
    assert_eq!(f.values["history_rotation_size"], 50);
    let with_period =
        settings::build_form(&json!({"history_rotation_size": 50, "history_rotation_period": 30}));
    assert_eq!(with_period.sections[0].fields.len(), 2);

    let v = |x: serde_json::Value| FormValues {
        values: vals(x),
        lists: BTreeMap::new(),
    };
    let ch = settings::config_change(&cfg, &v(json!({"history_rotation_size": 50}))).unwrap();
    assert!(ch.calls.is_empty());
    let ch = settings::config_change(
        &cfg,
        &v(json!({"history_rotation_size": 80, "history_rotation_period": 9})),
    )
    .unwrap();
    assert_eq!(
        ch.calls[0].body,
        Some(json!({"history_rotation_size": 80})),
        "only what the server has"
    );
    assert!(ch.script().contains("POST /api/v1/config"));
    let e = err(settings::config_change(
        &cfg,
        &v(json!({"history_rotation_size": 3})),
    ));
    assert!(e.contains("entre 5 y 200"));
}

#[test]
fn the_service_events_newest_first() {
    let t = logs::table(&json!({"events": [
        {"timestamp": "2026-11-12T21:49:10.101Z", "component": "backup", "severity": "info",
         "description": "A merge has been skipped", "node": "n1",
         "extra_attributes": {"repo": "r", "run_type": "one_off"}},
        {"timestamp": "2026-11-12T21:49:30.797Z", "component": "backup", "severity": "warn",
         "description": "A repository has been deleted", "node": "n1"},
        {"timestamp": "2026-11-12T21:50:00.000Z", "component": "ns_server", "severity": "info",
         "description": "other"}
    ]}));
    assert_eq!(t.columns, ["Fecha", "Nivel", "Evento", "Detalle", "Nodo"]);
    assert_eq!(t.rows.len(), 2);
    assert_eq!(t.rows[0][2], "A repository has been deleted");
    assert_eq!(t.rows[0][1], "Advertencia");
    assert_eq!(t.rows[1][3], "repo=r, run_type=one_off");
}
