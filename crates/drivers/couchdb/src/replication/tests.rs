use super::doc::{self, build_doc, doc_values, endpoint_from, endpoint_view, SOURCE, TARGET};
use super::settings;
use super::*;
use serde_json::json;

fn c35() -> Ctx {
    Ctx {
        version: (3, 5),
        missing: None,
    }
}

fn c31() -> Ctx {
    Ctx {
        version: (3, 1),
        missing: None,
    }
}

fn vals(v: serde_json::Value) -> BTreeMap<String, Value> {
    serde_json::from_value(v).unwrap()
}

fn fv(v: serde_json::Value) -> FormValues {
    FormValues {
        values: vals(v),
        lists: BTreeMap::new(),
    }
}

fn obj(v: serde_json::Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

fn err<T: std::fmt::Debug>(r: Result<T>) -> String {
    match r {
        Err(e) => e.to_string(),
        Ok(v) => panic!("expected an error, got {v:?}"),
    }
}

fn base() -> serde_json::Value {
    json!({"source_url": "http://127.0.0.1:5984/a", "target_url": "http://127.0.0.1:5984/b"})
}

fn with(mut v: serde_json::Value, extra: serde_json::Value) -> serde_json::Value {
    for (k, x) in extra.as_object().unwrap() {
        v[k] = x.clone();
    }
    v
}

#[test]
fn kinds_versions_and_notes() {
    assert_eq!(version_of("3.5.2"), (3, 5));
    assert_eq!(version_of("2.1.0"), (2, 1));
    assert!(Kind::parse("job").is_ok() && Kind::parse("settings").is_ok());
    assert!(err(Kind::parse("plan")).contains("no es un tipo"));
    let i = info_of(&c35());
    let keys: Vec<&str> = i.kinds.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(keys, ["job", "settings"]);
    assert!(i.kinds[0].can_create && i.kinds[0].can_edit && i.kinds[0].can_delete);
    assert!(!i.kinds[1].can_create && i.kinds[1].can_edit && !i.kinds[1].can_delete);
    assert!(!i.logs);
    let denied = info_of(&Ctx {
        missing: Some(jobs::DENIED),
        ..c35()
    });
    assert!(denied.kinds.is_empty() && denied.notes[0].contains("administrador"));
}

#[test]
fn secrets_are_masked() {
    assert_eq!(
        mask_url("http://u:p%40ss@h:5984/db"),
        format!("http://u:{SECRET_MASK}@h:5984/db")
    );
    assert_eq!(mask_url("http://h/db"), "http://h/db");
    assert_eq!(mask_url("http://u@h/db"), "http://u@h/db");
    let m = masked(&json!({
        "source": {"url": "http://h/a", "auth": {"basic": {"username": "u", "password": "s3cr3t"}},
                   "headers": {"Authorization": "Basic eDp5", "X-Other": "ok"}},
        "target": "http://u:pw@h/b"
    }));
    let t = m.to_string();
    assert!(
        !t.contains("s3cr3t") && !t.contains("eDp5") && !t.contains(":pw@"),
        "{t}"
    );
    assert!(t.contains("X-Other") && t.contains("\"ok\""));
}

#[test]
fn endpoints_read_without_their_secrets() {
    assert_eq!(
        doc::split_userinfo("https://a%40b:p%3Aw@h:6984/db"),
        (
            "https://h:6984/db".into(),
            Some("a@b".into()),
            Some("p:w".into())
        )
    );
    let v = endpoint_view(&json!("http://u:pw@h/db"));
    assert_eq!(v.url, "http://h/db");
    assert_eq!(v.user.as_deref(), Some("u"));
    assert!(v.has_password);
    let v = endpoint_view(
        &json!({"url": "http://h/db", "auth": {"basic": {"username": "x", "password": "y"}},
        "headers": {"Cookie": "AuthSession=1", "X-A": "b"}}),
    );
    assert_eq!(v.user.as_deref(), Some("x"));
    assert_eq!(v.headers["Cookie"], SECRET_MASK);
    assert_eq!(v.headers["X-A"], "b");
    assert_eq!(
        doc::password_of(&json!("http://u:pw@h/db")).as_deref(),
        Some("pw")
    );
    assert_eq!(
        doc::password_of(
            &json!({"url": "http://h", "auth": {"basic": {"username": "u", "password": "z"}}})
        )
        .as_deref(),
        Some("z")
    );
}

#[test]
fn endpoints_from_the_form() {
    let v = vals(
        json!({"source_url": "http://h:5984/db", "source_user": "u", "source_password": "p@ss:w/rd"}),
    );
    // 3.2 on: auth.basic.
    let e = endpoint_from(&c35(), SOURCE, Vals(&v), None).unwrap();
    assert_eq!(
        e,
        json!({"url": "http://h:5984/db", "auth": {"basic": {"username": "u", "password": "p@ss:w/rd"}}})
    );
    // Before: in the URL, encoded.
    let e = endpoint_from(&c31(), SOURCE, Vals(&v), None).unwrap();
    assert_eq!(e, json!("http://u:p%40ss%3Aw%2Frd@h:5984/db"));
    // No credentials: the URL alone; headers make it an object.
    let v = vals(json!({"target_url": "https://h/db", "target_headers": "{\"X-A\": \"1\"}"}));
    assert_eq!(
        endpoint_from(&c35(), TARGET, Vals(&v), None).unwrap(),
        json!({"url": "https://h/db", "headers": {"X-A": "1"}})
    );
    for (v, want) in [
        (
            json!({"source_url": "http://u:p@h/db"}),
            "no puede llevar usuario",
        ),
        (json!({"source_url": "ftp://h/db"}), "http o https"),
        (json!({"source_url": "ventas"}), "no es una URL"),
        (json!({}), "Falta la URL"),
        (
            json!({"source_url": "http://h/db", "source_user": "u"}),
            "Falta la contraseña",
        ),
        (
            json!({"source_url": "http://h/db", "source_password": "p"}),
            "Falta el usuario",
        ),
        (
            json!({"source_url": "http://h/db", "source_headers": "[1]"}),
            "objeto JSON",
        ),
        (
            json!({"source_url": "http://h/db", "source_headers": "{\"A\": 1}"}),
            "tiene que ser un texto",
        ),
        (
            json!({"source_url": "http://h/db", "source_headers": "{\"A\": \"x\\ny\"}"}),
            "saltos de línea",
        ),
        (
            json!({"source_url": "http://h/db", "source_headers": format!("{{\"Authorization\": \"{SECRET_MASK}\"}}")}),
            "asteriscos",
        ),
    ] {
        let e = err(endpoint_from(&c35(), SOURCE, Vals(&vals(v)), None));
        assert!(e.contains(want), "{e} / {want}");
    }
}

#[test]
fn endpoints_keep_what_the_form_cannot_show() {
    // Untouched: the old value as it was, whatever its form.
    let old = json!("http://u:pw@h/db");
    let v = vals(json!({"source_url": "http://h/db", "source_user": "u"}));
    assert_eq!(
        endpoint_from(&c35(), SOURCE, Vals(&v), Some(&old)).unwrap(),
        old
    );
    // Another user, the password kept.
    let v = vals(json!({"source_url": "http://h/db", "source_user": "w"}));
    assert_eq!(
        endpoint_from(&c35(), SOURCE, Vals(&v), Some(&old)).unwrap(),
        json!({"url": "http://h/db", "auth": {"basic": {"username": "w", "password": "pw"}}})
    );
    // A secret header left masked keeps its value; a plugin's key stays.
    let old =
        json!({"url": "http://h/db", "headers": {"Authorization": "Bearer t0k"}, "x_plugin": 1});
    let view = endpoint_view(&old);
    let v = vals(json!({"source_url": "http://h/other",
        "source_headers": serde_json::to_string(&Value::Object(view.headers)).unwrap()}));
    assert_eq!(
        endpoint_from(&c35(), SOURCE, Vals(&v), Some(&old)).unwrap(),
        json!({"url": "http://h/other", "headers": {"Authorization": "Bearer t0k"}, "x_plugin": 1})
    );
    // Credentials removed: no user, no password typed.
    let old = json!({"url": "http://h/db", "auth": {"basic": {"username": "u", "password": "p"}}});
    let v = vals(json!({"source_url": "http://h/db2"}));
    assert_eq!(
        endpoint_from(&c35(), SOURCE, Vals(&v), Some(&old)).unwrap(),
        json!("http://h/db2")
    );
}

#[test]
fn a_whole_document() {
    let v = fv(with(
        base(),
        json!({"source_user": "admin", "source_password": "secret", "continuous": true,
            "create_target": true, "create_target_q": 2, "create_target_partitioned": true,
            "filter_type": "selector", "selector": "{\"tipo\": \"pedido\"}", "since_seq": "now",
            "use_checkpoints": true, "checkpoint_interval": 5000, "worker_processes": 2,
            "socket_options": "[{keepalive, true}]", "target_proxy": "http://proxy:3128",
            "winning_revs_only": true, "use_bulk_get": true}),
    ));
    let d = build_doc(&c35(), &v, None).unwrap();
    assert_eq!(
        Value::Object(d.clone()),
        json!({
            "source": {"url": "http://127.0.0.1:5984/a", "auth": {"basic": {"username": "admin", "password": "secret"}}},
            "target": "http://127.0.0.1:5984/b",
            "continuous": true,
            "create_target": true,
            "create_target_params": {"q": 2, "partitioned": true},
            "selector": {"tipo": "pedido"},
            "since_seq": "now",
            "checkpoint_interval": 5000,
            "worker_processes": 2,
            "socket_options": "[{keepalive, true}]",
            "target_proxy": "http://proxy:3128",
            "winning_revs_only": true
        })
    );
    // Before 3.3: no winning_revs_only / use_bulk_get.
    let d = build_doc(&c31(), &v, None).unwrap();
    assert!(!d.contains_key("winning_revs_only"));
    // The script shows the password masked.
    let ch = doc::create_change(
        "_replicator",
        Some("r1"),
        build_doc(&c35(), &v, None).unwrap(),
        false,
    );
    let sc = ch.script();
    assert!(
        sc.contains("PUT /_replicator\n") && sc.contains("PUT /_replicator/r1\n{"),
        "{sc}"
    );
    assert!(
        sc.contains(SECRET_MASK) && !sc.contains("\"secret\""),
        "{sc}"
    );
    assert_eq!(ch.id.as_deref(), Some("doc:_replicator:r1"));
    // Without an id: POSTed, the id from the reply.
    let ch = doc::create_change("app/_replicator", None, Map::new(), true);
    assert_eq!(ch.calls.len(), 1);
    assert_eq!(ch.calls[0].path, "/app%2F_replicator");
    assert_eq!(ch.id_from_reply.as_deref(), Some("app/_replicator"));
}

#[test]
fn filters() {
    let f = |extra: serde_json::Value| build_doc(&c35(), &fv(with(base(), extra)), None);
    let d = f(json!({"filter_type": "doc_ids", "doc_ids": "a\r\nb c\n\n"})).unwrap();
    assert_eq!(d["doc_ids"], json!(["a", "b c"]));
    let d = f(json!({"filter_type": "design"})).unwrap();
    assert_eq!(d["filter"], "_design");
    let d = f(json!({"filter_type": "filter", "filter_name": "app/by_type", "query_params": "{\"t\": \"x\"}"})).unwrap();
    assert_eq!(
        (d["filter"].clone(), d["query_params"].clone()),
        (json!("app/by_type"), json!({"t": "x"}))
    );
    let d = f(json!({"filter_type": "view", "view_name": "app/active"})).unwrap();
    assert_eq!(
        (d["filter"].clone(), d["query_params"].clone()),
        (json!("_view"), json!({"view": "app/active"}))
    );
    for (extra, want) in [
        (
            json!({"filter_type": "doc_ids", "doc_ids": " "}),
            "uno por línea",
        ),
        (
            json!({"filter_type": "selector", "selector": "{"}),
            "objeto JSON",
        ),
        (json!({"filter_type": "selector"}), "Falta el selector"),
        (
            json!({"filter_type": "filter", "filter_name": "nofilter"}),
            "no es una función",
        ),
        (
            json!({"filter_type": "view", "view_name": "a/b/c"}),
            "no es una vista",
        ),
        (json!({"filter_type": "magic"}), "no es un filtro"),
        (
            json!({"proxy": "http://p", "source_proxy": "http://q"}),
            "proxy para los dos lados",
        ),
        (json!({"worker_processes": 0}), "entre 1 y 128"),
        (
            json!({"create_target": true, "create_target_q": 0}),
            "entre 1 y 256",
        ),
    ] {
        let e = err(f(extra));
        assert!(e.contains(want), "{e} / {want}");
    }
}

/// Documents as CouchDB keeps them read back into the form and saved
/// unchanged come out the same.
#[test]
fn documents_survive_the_form() {
    let docs = [
        json!({"_id": "r", "_rev": "1-a", "source": "http://u:pw@h/a", "target": "http://h/b",
               "continuous": true, "owner": "admin", "user_ctx": {"name": "x", "roles": []},
               "_replication_state": "completed", "_replication_id": "abc"}),
        json!({"_id": "r", "_rev": "2-b",
               "source": {"url": "http://h/a", "auth": {"basic": {"username": "u", "password": "p"}},
                          "headers": {"Authorization": "Basic eDp5"}},
               "target": {"url": "http://h/b"}, "filter": "_view", "query_params": {"view": "app/v", "k": 1},
               "since_seq": 42, "use_checkpoints": false, "use_bulk_get": true,
               "create_target": true, "create_target_params": {"n": 1}}),
        json!({"_id": "r", "_rev": "3-c", "source": "http://h/a", "target": "http://h/b",
               "doc_ids": ["x", "y"], "worker_batch_size": 100, "proxy": "http://p:1"}),
        json!({"_id": "r", "_rev": "4-d", "source": "http://h/a", "target": "http://h/b",
               "source_proxy": "http://pu:ppw@proxy:3128"}),
    ];
    for d in docs {
        let old = obj(d.clone());
        let values = doc_values("_replicator", "r", &old);
        let v = FormValues {
            values: values.clone().into_iter().collect(),
            lists: BTreeMap::new(),
        };
        assert!(values["source_password"].is_null());
        assert!(!values.values().any(|x| x.to_string().contains("pw@")));
        assert!(!values.values().any(|x| x.to_string().contains("ppw")));
        let new = build_doc(&c35(), &v, Some(&old)).unwrap();
        let ch = doc::update_change("_replicator", "r", "1-a", &old, new);
        assert!(ch.calls.is_empty(), "{d}: {}", ch.script());
    }
}

#[test]
fn edits_and_deletes() {
    let old = obj(
        json!({"_id": "r", "_rev": "1-a", "source": "http://h/a", "target": "http://h/b", "owner": "admin"}),
    );
    let mut values = doc_values("_replicator", "r", &old);
    values.insert("continuous".into(), json!(true));
    values.insert("worker_processes".into(), json!(3));
    let v = FormValues {
        values: values.into_iter().collect(),
        lists: BTreeMap::new(),
    };
    let new = build_doc(&c35(), &v, Some(&old)).unwrap();
    assert_eq!(new["owner"], "admin", "fields DBine doesn't show stay");
    let ch = doc::update_change("_replicator", "r", "1-a", &old, new);
    let sc = ch.script();
    assert!(
        sc.contains("Cambia la replicación «r»: continuous, worker_processes."),
        "{sc}"
    );
    assert!(
        sc.contains("PUT /_replicator/r\n{") && sc.contains("\"_rev\": \"1-a\""),
        "{sc}"
    );
    let sc = doc::delete_doc_change("app/_replicator", "a b", "3-x").script();
    assert!(
        sc.contains("DELETE /app%2F_replicator/a%20b?rev=3-x"),
        "{sc}"
    );
    let sc = doc::cancel_change("abc+continuous").script();
    assert!(
        sc.contains("POST /_replicate\n{") && sc.contains("\"cancel\": true"),
        "{sc}"
    );
    // Comments stay comments.
    let ch = Change::nothing("a\nDELETE /x", "id".into());
    assert!(ch.script().starts_with("// «a?DELETE /x»"));
}

#[test]
fn replicator_settings() {
    let cfg = obj(
        json!({"max_jobs": "600", "use_checkpoints": "false", "socket_options": "[]",
        "password": "k3y", "max_document_id_length": "infinity", "auth_plugins": "a,b",
        "interval": "soon"}),
    );
    let f = settings::build_form("n@h", &cfg);
    assert_eq!(f.values["max_jobs"], 600);
    assert_eq!(f.values["use_checkpoints"], "false");
    assert_eq!(f.values["use_bulk_get"], "");
    assert!(
        f.values["password"].is_null(),
        "the password never comes back"
    );
    assert!(f.values["interval"].is_null());
    assert!(f.notes.iter().any(|n| n.contains("«interval» vale «soon»")));
    let others = &f.sections.iter().find(|s| s.key == "others").unwrap().lists[0].rows;
    assert_eq!(others.len(), 2);

    // As it came: nothing.
    let same = FormValues {
        values: f.values.clone(),
        lists: BTreeMap::new(),
    };
    assert!(settings::config_change("n@h", &cfg, &same)
        .unwrap()
        .calls
        .is_empty());
    // Set, reset, the password typed.
    let mut v = same.clone();
    v.values.insert("max_jobs".into(), Value::Null);
    v.values.insert("max_churn".into(), json!(30));
    v.values.insert("use_checkpoints".into(), json!("true"));
    v.values.insert("password".into(), json!("n3w"));
    let ch = settings::config_change("n@h", &cfg, &v).unwrap();
    let sc = ch.script();
    assert!(
        sc.contains("DELETE /_node/n%40h/_config/replicator/max_jobs"),
        "{sc}"
    );
    assert!(
        sc.contains("PUT /_node/n%40h/_config/replicator/max_churn\n\"30\""),
        "{sc}"
    );
    assert!(sc.contains("PUT /_node/n%40h/_config/replicator/use_checkpoints\n\"true\""));
    assert!(sc.contains(&format!("replicator/password\n\"{SECRET_MASK}\"")) && !sc.contains("n3w"));
    assert_eq!(ch.calls.last().unwrap().body, Some(json!("n3w")));
    let mut v = same.clone();
    v.values
        .insert("verify_ssl_certificates".into(), json!("maybe"));
    assert!(err(settings::config_change("n@h", &cfg, &v)).contains("«true», «false» o vacío"));
}
